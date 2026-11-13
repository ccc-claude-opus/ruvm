//! Advanced SIMD (NEON) data-processing instructions, vector and scalar forms.
//!
//! Every group is a loop over lanes of `1 << esz` bytes. Floating-point lanes go through
//! the ARM wrappers in `fp.rs`, so NaN / rounding / FPSR behaviour is shared with the
//! scalar FP instructions.

use super::fp;
use super::simd::{lane, mask, set_lane};
use super::{Aarch64, Ex};
use crate::cpu::softfloat::{self as sf, Fmt, F32, F64};

#[inline]
fn bits_of(esz: u32) -> u32 {
    8 << esz
}

#[inline]
fn sx(x: u64, bits: u32) -> i128 {
    (((x << (64 - bits)) as i64) >> (64 - bits)) as i128
}

#[inline]
fn ux(x: u64, bits: u32) -> i128 {
    (x & if bits >= 64 { !0 } else { (1u64 << bits) - 1 }) as i128
}

fn wrap(v: i128, bits: u32) -> u64 {
    (v as u64) & if bits >= 64 { !0 } else { (1u64 << bits) - 1 }
}

fn sat_s(v: i128, bits: u32, qc: &mut bool) -> u64 {
    let (max, min) = ((1i128 << (bits - 1)) - 1, -(1i128 << (bits - 1)));
    if v > max {
        *qc = true;
        wrap(max, bits)
    } else if v < min {
        *qc = true;
        wrap(min, bits)
    } else {
        wrap(v, bits)
    }
}

fn sat_u(v: i128, bits: u32, qc: &mut bool) -> u64 {
    let max = (1i128 << bits) - 1;
    if v > max {
        *qc = true;
        max as u64
    } else if v < 0 {
        *qc = true;
        0
    } else {
        v as u64
    }
}

/// Saturate to the signed or unsigned range depending on `signed`.
fn sat(v: i128, bits: u32, signed: bool, qc: &mut bool) -> u64 {
    if signed { sat_s(v, bits, qc) } else { sat_u(v, bits, qc) }
}

/// (x << sh) or (x >> -sh) with optional rounding / saturation: the SSHL family.
fn shift_reg(x: u64, y: u64, bits: u32, signed: bool, round: bool, saturate: bool, qc: &mut bool) -> u64 {
    let sh = y as u8 as i8 as i32;
    let v = if signed { sx(x, bits) } else { ux(x, bits) };
    if sh >= 0 {
        let exact = if sh >= 100 || (v != 0 && sh as u32 >= 120 - bits) {
            if v == 0 { 0 } else if v > 0 { i128::MAX } else { i128::MIN }
        } else {
            v << sh
        };
        if saturate { sat(exact, bits, signed, qc) } else { wrap(if sh as u32 >= bits { 0 } else { v << sh }, bits) }
    } else {
        let r = (-sh) as u32;
        let res = if r >= 127 {
            // a rounding shift by more than the width gives 0 either way
            if signed && v < 0 && !round { -1 } else { 0 }
        } else {
            let rnd = if round { 1i128 << (r - 1) } else { 0 };
            (v + rnd) >> r
        };
        wrap(res, bits)
    }
}

impl Aarch64 {
    fn write_vec(&mut self, rd: usize, q: bool, v: u128) {
        self.vr[rd] = if q { v } else { v & u64::MAX as u128 };
    }

    fn qc(&mut self, qc: bool) {
        if qc {
            self.fpsr |= 1 << 27;
        }
    }

    // ------------------------------------------------------------------ modified immediate

    pub(super) fn simd_modimm(&mut self, i: u32) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let op = i >> 29 & 1;
        let imm8 = ((i >> 16 & 7) << 5 | (i >> 5 & 31)) as u64;
        let cmode = i >> 12 & 15;
        let rd = (i & 31) as usize;
        let rep32 = |x: u64| x | x << 32;
        let rep16 = |x: u64| rep32(x | x << 16);
        let imm64 = match cmode >> 1 {
            0 => rep32(imm8),
            1 => rep32(imm8 << 8),
            2 => rep32(imm8 << 16),
            3 => rep32(imm8 << 24),
            4 => rep16(imm8),
            5 => rep16(imm8 << 8),
            6 => rep32(if cmode & 1 == 0 { imm8 << 8 | 0xff } else { imm8 << 16 | 0xffff }),
            _ => match (cmode & 1, op) {
                (0, 0) => (0..8).fold(0u64, |a, k| a | imm8 << (8 * k)),
                (0, _) => (0..8).fold(0u64, |a, k| a | if imm8 >> k & 1 == 1 { 0xffu64 << (8 * k) } else { 0 }),
                (_, 0) => rep32(fp::expand_imm(F32, imm8 as u32)),
                _ => {
                    if !q {
                        return Err(self.undef(i));
                    }
                    fp::expand_imm(F64, imm8 as u32)
                }
            },
        };
        let wide = |x: u64| x as u128 | (x as u128) << 64;
        let orr_bic = cmode < 12 && cmode & 1 == 1;
        let v = if orr_bic {
            let m = if q { wide(imm64) } else { imm64 as u128 };
            if op == 0 { self.vr[rd] | m } else { self.vr[rd] & !m }
        } else {
            let x = if op == 1 && cmode != 14 && cmode != 15 { !imm64 } else { imm64 };
            if q { wide(x) } else { x as u128 }
        };
        self.write_vec(rd, q, v);
        Ok(())
    }

    // ------------------------------------------------------------------ copy

    pub(super) fn simd_copy(&mut self, i: u32) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let op = i >> 29 & 1;
        let imm5 = i >> 16 & 31;
        let imm4 = i >> 11 & 15;
        let (rn, rd) = (((i >> 5) & 31) as usize, (i & 31) as usize);
        if imm5 & 15 == 0 && !(imm5 == 8 && false) {
            // imm5 = x0000: only valid for the 64-bit element
            if imm5 != 16 {
                return Err(self.undef(i));
            }
        }
        let esz = imm5.trailing_zeros().min(4);
        if esz > 3 {
            return Err(self.undef(i));
        }
        let idx = (imm5 >> (esz + 1)) as usize;
        let n = ((if q { 16 } else { 8 }) >> esz) as usize;
        if op == 1 {
            // INS (element)
            let src = (imm4 >> esz) as usize;
            let x = lane(self.vr[rn], esz, src);
            set_lane(&mut self.vr[rd], esz, idx, x);
            return Ok(());
        }
        match imm4 {
            0 => {
                let x = lane(self.vr[rn], esz, idx);
                let mut v = 0;
                (0..n).for_each(|k| set_lane(&mut v, esz, k, x));
                self.write_vec(rd, q, v);
            }
            1 => {
                let x = self.rz(rn);
                let mut v = 0;
                (0..n).for_each(|k| set_lane(&mut v, esz, k, x));
                self.write_vec(rd, q, v);
            }
            3 => {
                let x = self.rz(rn);
                set_lane(&mut self.vr[rd], esz, idx, x);
            }
            5 => {
                let x = sx(lane(self.vr[rn], esz, idx), bits_of(esz)) as u64;
                self.wz(rd, x, q);
            }
            7 => {
                if esz == 3 && !q || esz < 3 && q && false {
                    return Err(self.undef(i));
                }
                let x = lane(self.vr[rn], esz, idx);
                self.wz(rd, x, q);
            }
            _ => return Err(self.undef(i)),
        }
        Ok(())
    }

    // ------------------------------------------------------------------ three same

    /// One lane of an integer three-same operation. `d` is the old destination lane.
    fn int3(&self, opc: u32, u: bool, esz: u32, x: u64, y: u64, d: u64, qc: &mut bool) -> Option<u64> {
        let bits = bits_of(esz);
        let (sxv, syv) = (sx(x, bits), sx(y, bits));
        let (uxv, uyv) = (ux(x, bits), ux(y, bits));
        let (a, b) = if u { (uxv, uyv) } else { (sxv, syv) };
        let ones = wrap(-1, bits);
        Some(match opc {
            0x00 => wrap((a + b) >> 1, bits),                            // SHADD / UHADD
            0x01 => sat(a + b, bits, !u, qc),                            // SQADD / UQADD
            0x02 => wrap((a + b + 1) >> 1, bits),                        // SRHADD / URHADD
            0x04 => wrap((a - b) >> 1, bits),                            // SHSUB / UHSUB
            0x05 => sat(a - b, bits, !u, qc),                            // SQSUB / UQSUB
            0x06 => if a > b { ones } else { 0 },                        // CMGT / CMHI
            0x07 => if a >= b { ones } else { 0 },                       // CMGE / CMHS
            0x08 => shift_reg(x, y, bits, !u, false, false, qc),         // SSHL / USHL
            0x09 => shift_reg(x, y, bits, !u, false, true, qc),          // SQSHL / UQSHL
            0x0a => shift_reg(x, y, bits, !u, true, false, qc),          // SRSHL / URSHL
            0x0b => shift_reg(x, y, bits, !u, true, true, qc),           // SQRSHL / UQRSHL
            0x0c => wrap(a.max(b), bits),                                // SMAX / UMAX
            0x0d => wrap(a.min(b), bits),                                // SMIN / UMIN
            0x0e => wrap((a - b).abs(), bits),                           // SABD / UABD
            0x0f => wrap(d as i128 + (a - b).abs(), bits),               // SABA / UABA
            0x10 => if u { wrap(uxv - uyv, bits) } else { wrap(uxv + uyv, bits) }, // SUB / ADD
            0x11 => if u { if x == y { ones } else { 0 } } else if x & y != 0 { ones } else { 0 }, // CMEQ / CMTST
            0x12 => if u { wrap(d as i128 - uxv * uyv, bits) } else { wrap(d as i128 + uxv * uyv, bits) }, // MLS / MLA
            0x13 => {
                if u {
                    // PMUL (bytes)
                    let mut r = 0u64;
                    for k in 0..8 {
                        if y >> k & 1 == 1 {
                            r ^= x << k;
                        }
                    }
                    r & 0xff
                } else {
                    wrap(uxv * uyv, bits)
                }
            }
            0x16 => {
                // SQDMULH / SQRDMULH
                let p = 2 * sxv * syv + if u { 1i128 << (bits - 1) } else { 0 };
                sat_s(p >> bits, bits, qc)
            }
            _ => return None,
        })
    }

    pub(super) fn simd_three_same(&mut self, i: u32, scalar: bool) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let u = i >> 29 & 1 == 1;
        let size = i >> 22 & 3;
        let opc = i >> 11 & 31;
        let (rm, rn, rd) = (((i >> 16) & 31) as usize, ((i >> 5) & 31) as usize, (i & 31) as usize);
        let (a, b, d) = (self.vr[rn], self.vr[rm], self.vr[rd]);
        if opc >= 0x18 {
            return self.simd_three_same_fp(i, scalar);
        }
        if opc == 0x03 {
            // bitwise logic, whole register
            if scalar {
                return Err(self.undef(i));
            }
            let r = match (u, size) {
                (false, 0) => a & b,
                (false, 1) => a & !b,
                (false, 2) => a | b,
                (false, _) => a | !b,
                (true, 0) => a ^ b,
                (true, 1) => (d & a) | (!d & b), // BSL: Vd selects between Vn and Vm
                (true, 2) => (d & !b) | (a & b), // BIT: insert Vn bits where Vm is 1
                (true, _) => (d & b) | (a & !b), // BIF: insert Vn bits where Vm is 0
            };
            self.write_vec(rd, q, r);
            return Ok(());
        }
        let esz = size;
        let n = if scalar { 1 } else { ((if q { 16 } else { 8 }) >> esz) as usize };
        let mut qc = false;
        let mut res = 0u128;
        let pairwise = matches!(opc, 0x14 | 0x15 | 0x17);
        let half = if q { 16 } else { 8 } >> esz;
        for k in 0..n {
            let (x, y) = if pairwise {
                // concatenate Vn:Vm, take adjacent pairs
                let (src, j) = if k < half / 2 { (a, 2 * k) } else { (b, 2 * (k - half / 2)) };
                (lane(src, esz, j), lane(src, esz, j + 1))
            } else {
                (lane(a, esz, k), lane(b, esz, k))
            };
            let r = if pairwise {
                let bits = bits_of(esz);
                let (p, qv) = if u { (ux(x, bits), ux(y, bits)) } else { (sx(x, bits), sx(y, bits)) };
                match opc {
                    0x14 => wrap(p.max(qv), bits),
                    0x15 => wrap(p.min(qv), bits),
                    _ => wrap(p + qv, bits), // ADDP
                }
            } else {
                match self.int3(opc, u, esz, x, y, lane(d, esz, k), &mut qc) {
                    Some(r) => r,
                    None => return Err(self.undef(i)),
                }
            };
            set_lane(&mut res, esz, k, r);
        }
        self.qc(qc);
        self.write_vec(rd, q || scalar, res);
        if scalar {
            self.vr[rd] &= mask(1 << esz) as u128;
        }
        Ok(())
    }

    fn simd_three_same_fp(&mut self, i: u32, scalar: bool) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let u = i >> 29 & 1 == 1;
        let b23 = i >> 23 & 1 == 1;
        let sz = i >> 22 & 1;
        let opc = i >> 11 & 31;
        let (rm, rn, rd) = (((i >> 16) & 31) as usize, ((i >> 5) & 31) as usize, (i & 31) as usize);
        let (f, esz) = if sz == 1 { (F64, 3) } else { (F32, 2) };
        if sz == 1 && !q && !scalar {
            return Err(self.undef(i));
        }
        let (a, b, d) = (self.vr[rn], self.vr[rm], self.vr[rd]);
        let n = if scalar { 1 } else { ((if q { 16 } else { 8 }) >> esz) as usize };
        let (rmode, dn) = (self.fp_rm(), self.fp_dn());
        let mut fl = sf::TINY_BEFORE;
        let ones = mask(1 << esz);
        let pairwise = !scalar && matches!((opc, u), (0x18, true) | (0x1a, true) | (0x1e, true)) && !(opc == 0x1a && b23);
        let half = n / 2;
        let mut res = 0u128;
        for k in 0..n {
            let (x, y) = if pairwise {
                let (src, j) = if k < half { (a, 2 * k) } else { (b, 2 * (k - half)) };
                (lane(src, esz, j), lane(src, esz, j + 1))
            } else {
                (lane(a, esz, k), lane(b, esz, k))
            };
            let acc = lane(d, esz, k);
            let r = match (opc, u) {
                (0x18, _) => fp::fmaxnm(f, x, y, !b23, dn, &mut fl),
                (0x19, false) => {
                    // FMLA / FMLS: acc + x*y
                    let x = if b23 { sf::neg(f, x) } else { x };
                    fp::fma(f, x, y, acc, rmode, dn, &mut fl)
                }
                (0x1a, false) => {
                    if b23 { fp::sub(f, x, y, rmode, dn, &mut fl) } else { fp::add(f, x, y, rmode, dn, &mut fl) }
                }
                (0x1a, true) if b23 => {
                    let t = fp::sub(f, x, y, rmode, dn, &mut fl);
                    sf::abs(f, t)
                }
                (0x1a, true) => fp::add(f, x, y, rmode, dn, &mut fl), // FADDP
                (0x1b, false) => fp::mulx(f, x, y, rmode, dn, &mut fl),
                (0x1b, true) => fp::mul(f, x, y, rmode, dn, &mut fl),
                (0x1c, false) => if sf::eq(f, x, y, &mut fl) { ones } else { 0 },
                (0x1c, true) => {
                    let t = if b23 { sf::lt(f, y, x, &mut fl) } else { sf::le(f, y, x, &mut fl) };
                    if t { ones } else { 0 }
                }
                (0x1d, true) => {
                    let (ax, ay) = (sf::abs(f, x), sf::abs(f, y));
                    let t = if b23 { sf::lt(f, ay, ax, &mut fl) } else { sf::le(f, ay, ax, &mut fl) };
                    if t { ones } else { 0 }
                }
                (0x1e, _) => fp::fmax(f, x, y, !b23, dn, &mut fl),
                (0x1f, false) => {
                    // FRECPS / FRSQRTS
                    self.fp_step(f, x, y, !b23, rmode, dn, &mut fl)
                }
                (0x1f, true) if !b23 => fp::div(f, x, y, rmode, dn, &mut fl),
                _ => return Err(self.undef(i)),
            };
            set_lane(&mut res, esz, k, r);
        }
        self.fp_raise(fl);
        self.write_vec(rd, q || scalar, res);
        Ok(())
    }

    /// FRECPS (`recip`) / FRSQRTS step: 2 - a*b, or (3 - a*b) / 2.
    fn fp_step(&self, f: Fmt, a: u64, b: u64, recip: bool, rm: u8, dn: bool, fl: &mut u8) -> u64 {
        let two = sf::from_int(f, 2, false, 64, rm, &mut 0);
        let three = sf::from_int(f, 3, false, 64, rm, &mut 0);
        if (sf::is_inf(f, a) && sf::is_zero(f, b)) || (sf::is_zero(f, a) && sf::is_inf(f, b)) {
            if sf::is_nan_bits(f, a) || sf::is_nan_bits(f, b) {
                return fp::mul(f, a, b, rm, dn, fl);
            }
            return if recip { two } else { sf::from_int_scaled(f, 3, false, 64, -1, rm, &mut 0) };
        }
        let na = sf::neg(f, a);
        fp::fma_scaled(f, na, b, if recip { two } else { three }, if recip { 0 } else { -1 }, rm, dn, fl)
    }

    // ------------------------------------------------------------------ three different

    pub(super) fn simd_three_diff(&mut self, i: u32) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let u = i >> 29 & 1 == 1;
        let size = i >> 22 & 3;
        let opc = i >> 12 & 15;
        let (rm, rn, rd) = (((i >> 16) & 31) as usize, ((i >> 5) & 31) as usize, (i & 31) as usize);
        if size == 3 && opc != 0xe {
            return Err(self.undef(i));
        }
        let esz = size;
        let (a, b, d) = (self.vr[rn], self.vr[rm], self.vr[rd]);
        let bits = bits_of(esz);
        let wbits = bits * 2;
        let wesz = esz + 1;
        let n = (8 >> esz) as usize; // lanes in the narrow half
        let off = if q { n } else { 0 }; // lane offset for the "2" variants
        let mut qc = false;
        let mut res = 0u128;
        let sv = |x: u64, bits: u32| if u { ux(x, bits) } else { sx(x, bits) };
        match opc {
            0..=3 | 5 | 7..=0xd => {
                for k in 0..n {
                    let wide_n = matches!(opc, 1 | 3);
                    let x = if wide_n { sv(lane(a, wesz, k), wbits) } else { sv(lane(a, esz, off + k), bits) };
                    let y = sv(lane(b, esz, off + k), bits);
                    let acc = lane(d, wesz, k);
                    let r = match opc {
                        0 | 1 => x + y,
                        2 | 3 => x - y,
                        5 | 7 => {
                            let ad = (x - y).abs();
                            if opc == 5 { acc as i128 + ad } else { ad }
                        }
                        8 => acc as i128 + x * y,
                        0xa => acc as i128 - x * y,
                        0xc => x * y,
                        9 | 0xb | 0xd => {
                            // SQDMLAL / SQDMLSL / SQDMULL (signed only)
                            let p = 2 * sx(lane(a, esz, off + k), bits) * sx(lane(b, esz, off + k), bits);
                            let p = sat_s(p, wbits, &mut qc) as i128;
                            let p = sx(p as u64, wbits);
                            match opc {
                                9 => sat_s(sx(acc, wbits) + p, wbits, &mut qc) as i128,
                                0xb => sat_s(sx(acc, wbits) - p, wbits, &mut qc) as i128,
                                _ => p,
                            }
                        }
                        _ => unreachable!(),
                    };
                    set_lane(&mut res, wesz, k, wrap(r, wbits));
                }
                self.qc(qc);
                self.vr[rd] = res;
            }
            4 | 6 => {
                // ADDHN / RADDHN / SUBHN / RSUBHN: narrow the high half
                let round = if u { 1i128 << (bits - 1) } else { 0 };
                let m = (8 >> esz) as usize;
                for k in 0..m {
                    let x = ux(lane(a, wesz, k), wbits);
                    let y = ux(lane(b, wesz, k), wbits);
                    let r = if opc == 4 { x + y + round } else { x - y + round };
                    set_lane(&mut res, esz, k, wrap(r >> bits, bits));
                }
                self.vr[rd] = if q { (d & u64::MAX as u128) | res << 64 } else { res };
            }
            0xe => {
                // PMULL (polynomial): size 0 = 8b -> 8h, size 3 = 64 -> 128
                if size == 0 {
                    for k in 0..8 {
                        let (x, y) = (lane(a, 0, off + k), lane(b, 0, off + k));
                        let mut r = 0u64;
                        for t in 0..8 {
                            if y >> t & 1 == 1 {
                                r ^= x << t;
                            }
                        }
                        set_lane(&mut res, 1, k, r & 0xffff);
                    }
                } else {
                    let (x, y) = (lane(a, 3, q as usize), lane(b, 3, q as usize));
                    let mut r = 0u128;
                    for t in 0..64 {
                        if y >> t & 1 == 1 {
                            r ^= (x as u128) << t;
                        }
                    }
                    res = r;
                }
                self.vr[rd] = res;
            }
            _ => return Err(self.undef(i)),
        }
        Ok(())
    }

    // ------------------------------------------------------------------ two-register misc

    pub(super) fn simd_two_reg(&mut self, i: u32, scalar: bool) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let u = i >> 29 & 1 == 1;
        let size = i >> 22 & 3;
        let opc = i >> 12 & 31;
        let (rn, rd) = (((i >> 5) & 31) as usize, (i & 31) as usize);
        let a = self.vr[rn];
        let esz = size;
        let n = if scalar { 1 } else { ((if q { 16 } else { 8 }) >> esz) as usize };
        let bits = bits_of(esz);
        let mut qc = false;
        let mut res = 0u128;
        let d = self.vr[rd];

        // floating-point forms (opcode >= 0x0c, size selects float width and variant)
        if opc >= 0x0c && !matches!(opc, 0x12 | 0x13 | 0x14) || (matches!(opc, 0x16 | 0x17)) {
            return self.simd_two_reg_fp(i, scalar);
        }
        match (opc, u) {
            (0x00, false) | (0x00, true) | (0x01, false) => {
                // REV64 / REV32 / REV16: reverse elements within containers
                let cbits = match (opc, u) {
                    (0, false) => 64,
                    (0, true) => 32,
                    _ => 16,
                };
                if esz >= (cbits as u32 / 8).trailing_zeros() {
                    return Err(self.undef(i));
                }
                let per = cbits / bits as usize;
                for k in 0..n {
                    let base = k / per * per;
                    let src = base + (per - 1 - (k - base));
                    set_lane(&mut res, esz, k, lane(a, esz, src));
                }
            }
            (0x02, _) | (0x06, _) => {
                // SADDLP / UADDLP / SADALP / UADALP: pairwise add long
                if esz == 3 {
                    return Err(self.undef(i));
                }
                let wesz = esz + 1;
                let m = n / 2;
                for k in 0..m {
                    let (x, y) = (lane(a, esz, 2 * k), lane(a, esz, 2 * k + 1));
                    let s = if u { ux(x, bits) + ux(y, bits) } else { sx(x, bits) + sx(y, bits) };
                    let acc = if opc == 6 { lane(d, wesz, k) as i128 } else { 0 };
                    set_lane(&mut res, wesz, k, wrap(s + acc, bits * 2));
                }
            }
            (0x03, _) => {
                // SUQADD / USQADD
                for k in 0..n {
                    let (x, acc) = (lane(a, esz, k), lane(d, esz, k));
                    let r = if u { sat_u(ux(acc, bits) + sx(x, bits), bits, &mut qc) } else { sat_s(sx(acc, bits) + ux(x, bits), bits, &mut qc) };
                    set_lane(&mut res, esz, k, r);
                }
            }
            (0x04, false) => {
                // CLS
                for k in 0..n {
                    let x = lane(a, esz, k);
                    let s = sx(x, bits) as i64;
                    let r = ((s ^ (s >> 1)) as u64).leading_zeros() as i64 - (64 - bits as i64) - 1;
                    set_lane(&mut res, esz, k, r.max(0).min(bits as i64 - 1) as u64);
                }
            }
            (0x04, true) => {
                for k in 0..n {
                    let x = lane(a, esz, k);
                    set_lane(&mut res, esz, k, (x.leading_zeros() as i64 - (64 - bits as i64)).max(0) as u64);
                }
            }
            (0x05, false) => {
                for k in 0..n {
                    set_lane(&mut res, esz, k, (lane(a, esz, k) as u8).count_ones() as u64);
                }
            }
            (0x05, true) => {
                if esz == 0 {
                    res = !a;
                } else if esz == 1 {
                    // RBIT: reverse the bits of each byte (size = 01 selects it; lanes are bytes)
                    for k in 0..if q { 16 } else { 8 } {
                        set_lane(&mut res, 0, k, (lane(a, 0, k) as u8).reverse_bits() as u64);
                    }
                } else {
                    return Err(self.undef(i));
                }
            }
            (0x07, _) => {
                for k in 0..n {
                    let x = sx(lane(a, esz, k), bits);
                    let r = if u { -x } else { x.abs() };
                    set_lane(&mut res, esz, k, sat_s(r, bits, &mut qc));
                }
            }
            (0x08..=0x0b, _) => {
                for k in 0..n {
                    let x = lane(a, esz, k);
                    let v = sx(x, bits);
                    let ones = mask(1 << esz);
                    let r = match (opc, u) {
                        (0x08, false) => if v > 0 { ones } else { 0 },    // CMGT #0
                        (0x08, true) => if v >= 0 { ones } else { 0 },    // CMGE #0
                        (0x09, false) => if v == 0 { ones } else { 0 },   // CMEQ #0
                        (0x09, true) => if v <= 0 { ones } else { 0 },    // CMLE #0
                        (0x0a, false) => if v < 0 { ones } else { 0 },    // CMLT #0
                        (0x0b, false) => wrap(v.abs(), bits),             // ABS
                        (0x0b, true) => wrap(-v, bits),                   // NEG
                        _ => return Err(self.undef(i)),
                    };
                    set_lane(&mut res, esz, k, r);
                }
            }
            (0x12, false) | (0x12, true) | (0x14, _) => {
                // XTN / SQXTUN / SQXTN / UQXTN: narrow; Q selects the upper half of the destination
                if esz == 3 {
                    return Err(self.undef(i));
                }
                let wbits = bits * 2;
                let m = if scalar { 1 } else { 8 >> esz };
                for k in 0..m {
                    let x = lane(a, esz + 1, k);
                    let r = match (opc, u) {
                        (0x12, false) => x & mask(1 << esz),
                        (0x12, true) => sat_u(sx(x, wbits), bits, &mut qc),
                        (0x14, false) => sat_s(sx(x, wbits), bits, &mut qc),
                        _ => sat_u(ux(x, wbits), bits, &mut qc),
                    };
                    set_lane(&mut res, esz, k, r);
                }
                self.qc(qc);
                self.vr[rd] = if q && !scalar { (d & u64::MAX as u128) | res << 64 } else { res };
                return Ok(());
            }
            (0x13, true) => {
                // SHLL: shift left by the element size, widening
                if esz == 3 {
                    return Err(self.undef(i));
                }
                let m = (8 >> esz) as usize;
                let off = if q { m } else { 0 };
                for k in 0..m {
                    set_lane(&mut res, esz + 1, k, lane(a, esz, off + k) << bits);
                }
                self.vr[rd] = res;
                return Ok(());
            }
            _ => return Err(self.undef(i)),
        }
        self.qc(qc);
        self.write_vec(rd, q || scalar, res);
        Ok(())
    }

    fn simd_two_reg_fp(&mut self, i: u32, scalar: bool) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let u = i >> 29 & 1 == 1;
        let size = i >> 22 & 3;
        let opc = i >> 12 & 31;
        let (rn, rd) = (((i >> 5) & 31) as usize, (i & 31) as usize);
        let a = self.vr[rn];
        let sz = size & 1;
        let hi = size >> 1 == 1;
        let (rmode, dn) = (self.fp_rm(), self.fp_dn());
        let mut fl = sf::TINY_BEFORE;

        // conversions between precisions
        match (opc, u) {
            (0x16, false) => {
                // FCVTN / FCVTN2: double -> single (sz=1) or single -> half (sz=0)
                let (from, to, fe, te) = if sz == 1 { (F64, F32, 3, 2) } else { (sf::F16, F32, 2, 1) };
                let (from, to, fe, te) = if sz == 1 { (from, to, fe, te) } else { (F32, sf::F16, 2, 1) };
                let n = (8 >> te) as usize;
                let mut res = 0u128;
                for k in 0..n {
                    let r = fp::convert(from, to, lane(a, fe, k), rmode, dn, &mut fl);
                    set_lane(&mut res, te, k, r);
                }
                self.fp_raise(fl);
                let d = self.vr[rd];
                self.vr[rd] = if q { (d & u64::MAX as u128) | res << 64 } else { res };
                return Ok(());
            }
            (0x17, false) => {
                // FCVTL / FCVTL2: single -> double (sz=1) or half -> single
                let (from, to, fe, te) = if sz == 1 { (F32, F64, 2, 3) } else { (sf::F16, F32, 1, 2) };
                let n = (16 >> te) as usize; // result lanes fill the whole 128-bit register
                let off = if q { n } else { 0 };
                let mut res = 0u128;
                for k in 0..n {
                    let r = fp::convert(from, to, lane(a, fe, off + k), rmode, dn, &mut fl);
                    set_lane(&mut res, te, k, r);
                }
                self.fp_raise(fl);
                self.vr[rd] = res;
                return Ok(());
            }
            (0x16, true) => {
                // FCVTXN: double -> single, round to odd
                if sz == 0 {
                    return Err(self.undef(i));
                }
                let mut res = 0u128;
                for k in 0..if scalar { 1 } else { 2 } {
                    let x = lane(a, 3, k);
                    let mut t = sf::TINY_BEFORE;
                    let mut r = fp::convert(F64, F32, x, sf::RTZ, dn, &mut t);
                    if t & 1 != 0 && !sf::is_nan_bits(F32, r) && !sf::is_inf(F32, r) {
                        r |= 1; // round to odd: set the lsb when inexact
                    }
                    fl |= t;
                    set_lane(&mut res, 2, k, r);
                }
                self.fp_raise(fl);
                let d = self.vr[rd];
                self.vr[rd] = if q && !scalar { (d & u64::MAX as u128) | res << 64 } else { res };
                return Ok(());
            }
            _ => {}
        }
        if sz == 1 && !q && !scalar {
            return Err(self.undef(i));
        }
        let (f, esz) = if sz == 1 { (F64, 3) } else { (F32, 2) };
        let n = if scalar { 1 } else { ((if q { 16 } else { 8 }) >> esz) as usize };
        let ones = mask(1 << esz);
        let mut res = 0u128;
        let bitsz = 32 << sz;
        for k in 0..n {
            let x = lane(a, esz, k);
            let r = match (opc, u, hi) {
                (0x0c, false, true) => if sf::is_nan_bits(f, x) { fl |= sf::NV; 0 } else if sf::compare(f, x, 0) == std::cmp::Ordering::Greater { ones } else { 0 }, // FCMGT #0
                (0x0c, true, true) => if sf::le(f, 0, x, &mut fl) { ones } else { 0 },                                  // FCMGE #0
                (0x0d, false, true) => if sf::eq(f, x, 0, &mut fl) { ones } else { 0 },                                 // FCMEQ #0
                (0x0d, true, true) => if sf::le(f, x, 0, &mut fl) { ones } else { 0 },                                  // FCMLE #0
                (0x0e, false, true) => if sf::lt(f, x, 0, &mut fl) { ones } else { 0 },                                 // FCMLT #0
                (0x0f, false, true) => sf::abs(f, x),                                                                    // FABS
                (0x0f, true, true) => sf::neg(f, x),                                                                     // FNEG
                (0x1f, true, true) => fp::sqrt(f, x, rmode, dn, &mut fl),                                                // FSQRT
                (0x18..=0x19, _, _) => {
                    let (rm2, exact) = match (opc, u, hi) {
                        (0x18, false, false) => (sf::RNE, false), // FRINTN
                        (0x18, false, true) => (sf::RUP, false),  // FRINTP
                        (0x19, false, false) => (sf::RDN, false), // FRINTM
                        (0x19, false, true) => (sf::RTZ, false),  // FRINTZ
                        (0x18, true, false) => (sf::RMM, false),  // FRINTA
                        (0x19, true, false) => (rmode, true),     // FRINTX
                        (0x19, true, true) => (rmode, false),     // FRINTI
                        _ => return Err(self.undef(i)),
                    };
                    fp::rint(f, x, rm2, exact, dn, &mut fl)
                }
                (0x1a..=0x1c, _, _) if !(opc == 0x1c && hi) => {
                    let rm2 = match (opc, hi) {
                        (0x1a, false) => sf::RNE,
                        (0x1a, true) => sf::RUP,
                        (0x1b, false) => sf::RDN,
                        (0x1b, true) => sf::RTZ,
                        _ => sf::RMM,
                    };
                    fp::to_int(f, x, rm2, !u, bitsz, 0, &mut fl)
                }
                (0x1d, false, true) => fp::frecpe(f, x, rmode, dn, &mut fl),
                (0x1d, true, true) => fp::frsqrte(f, x, dn, &mut fl),
                (0x1f, false, true) if scalar => fp::frecpx(f, x, dn, &mut fl),
                (0x1c, false, true) => {
                    // URECPE: unsigned reciprocal estimate (32-bit lanes)
                    let v = x as u32;
                    if v >> 31 == 0 { 0xffff_ffff } else { ((fp::recip_estimate(v >> 23) & 0x1ff) << 23) as u64 }
                }
                (0x1c, true, true) => {
                    // URSQRTE
                    let v = x as u32;
                    if v >> 30 == 0 { 0xffff_ffff } else { ((fp::rsqrt_estimate(v >> 23) & 0x1ff) << 23) as u64 }
                }
                (0x1d, _, false) => {
                    // SCVTF / UCVTF (integer lanes)
                    sf::from_int(f, x, !u, bitsz, rmode, &mut fl)
                }
                _ => return Err(self.undef(i)),
            };
            set_lane(&mut res, esz, k, r);
        }
        self.fp_raise(fl);
        self.write_vec(rd, q || scalar, res);
        Ok(())
    }

    // ------------------------------------------------------------------ across lanes / pairwise

    pub(super) fn simd_across(&mut self, i: u32) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let u = i >> 29 & 1 == 1;
        let size = i >> 22 & 3;
        let opc = i >> 12 & 31;
        let (rn, rd) = (((i >> 5) & 31) as usize, (i & 31) as usize);
        let a = self.vr[rn];
        let esz = size;
        let n = ((if q { 16 } else { 8 }) >> esz) as usize;
        let bits = bits_of(esz);
        let v = |k: usize| if u { ux(lane(a, esz, k), bits) } else { sx(lane(a, esz, k), bits) };
        let r: u64 = match opc {
            0x03 => wrap((0..n).map(v).sum::<i128>(), bits * 2),            // SADDLV / UADDLV
            0x0a => wrap((0..n).map(v).max().unwrap(), bits),              // SMAXV / UMAXV
            0x1a => wrap((0..n).map(v).min().unwrap(), bits),              // SMINV / UMINV
            0x1b if !u => wrap((0..n).map(|k| ux(lane(a, esz, k), bits)).sum::<i128>(), bits), // ADDV
            0x0c | 0x0f if u || opc == 0x0f => {
                // FMAXNMV / FMINNMV / FMAXV / FMINV (single precision, 4 lanes)
                if size & 1 == 1 || !q {
                    return Err(self.undef(i)); // only the 4S form exists (sz = 0)
                }
                let (dn, mut fl) = (self.fp_dn(), sf::TINY_BEFORE);
                let min = size >> 1 == 1;
                let xs: Vec<u64> = (0..4).map(|k| lane(a, 2, k)).collect();
                let combine = |x: u64, y: u64, fl: &mut u8| {
                    if opc == 0x0c { fp::fmaxnm(F32, x, y, !min, dn, fl) } else { fp::fmax(F32, x, y, !min, dn, fl) }
                };
                let a1 = combine(xs[0], xs[1], &mut fl);
                let a2 = combine(xs[2], xs[3], &mut fl);
                let r = combine(a1, a2, &mut fl);
                self.fp_raise(fl);
                self.vr[rd] = r as u128;
                return Ok(());
            }
            _ => return Err(self.undef(i)),
        };
        self.vr[rd] = r as u128;
        Ok(())
    }

    /// Scalar pairwise: ADDP Dd, Vn.2D / FADDP / FMAXP / ... (two elements -> one).
    pub(super) fn simd_scalar_pairwise(&mut self, i: u32) -> Result<(), Ex> {
        let u = i >> 29 & 1 == 1;
        let size = i >> 22 & 3;
        let opc = i >> 12 & 31;
        let (rn, rd) = (((i >> 5) & 31) as usize, (i & 31) as usize);
        let a = self.vr[rn];
        if opc == 0x1b && !u && size == 3 {
            self.vr[rd] = (lane(a, 3, 0).wrapping_add(lane(a, 3, 1))) as u128;
            return Ok(());
        }
        if !u {
            return Err(self.undef(i));
        }
        let (f, esz) = if size & 1 == 1 { (F64, 3) } else { (F32, 2) };
        let (x, y) = (lane(a, esz, 0), lane(a, esz, 1));
        let (rmode, dn) = (self.fp_rm(), self.fp_dn());
        let mut fl = sf::TINY_BEFORE;
        let min = size >> 1 == 1;
        let r = match opc {
            0x0c => fp::fmaxnm(f, x, y, !min, dn, &mut fl),
            0x0d => fp::add(f, x, y, rmode, dn, &mut fl),
            0x0f => fp::fmax(f, x, y, !min, dn, &mut fl),
            _ => return Err(self.undef(i)),
        };
        self.fp_raise(fl);
        self.vr[rd] = r as u128;
        Ok(())
    }

    // ------------------------------------------------------------------ shifts by immediate

    pub(super) fn simd_shift_imm(&mut self, i: u32, scalar: bool) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let u = i >> 29 & 1 == 1;
        let immh = i >> 19 & 15;
        let immb = i >> 16 & 7;
        let opc = i >> 11 & 31;
        let (rn, rd) = (((i >> 5) & 31) as usize, (i & 31) as usize);
        if immh == 0 {
            return Err(self.undef(i));
        }
        let esz = 3 - immh.leading_zeros() + 28; // index of the highest set bit of immh
        let esz = esz.min(3);
        let bits = bits_of(esz);
        let imm = (immh << 3 | immb) as i32;
        let rshift = (2 * bits as i32 - imm) as u32; // 1..=bits
        let lshift = (imm - bits as i32) as u32; // 0..bits-1
        let a = self.vr[rn];
        let d = self.vr[rd];
        let n = if scalar { 1 } else { ((if q { 16 } else { 8 }) >> esz) as usize };
        let mut qc = false;
        let mut res = 0u128;
        let sv = |x: u64| if u { ux(x, bits) } else { sx(x, bits) };

        match opc {
            0x00 | 0x02 | 0x04 | 0x06 => {
                // SSHR/USHR, SSRA/USRA, SRSHR/URSHR, SRSRA/URSRA
                for k in 0..n {
                    let x = sv(lane(a, esz, k));
                    let rnd = if opc >= 4 { 1i128 << (rshift - 1) } else { 0 };
                    let mut r = (x + rnd) >> rshift;
                    if opc & 2 != 0 {
                        r += lane(d, esz, k) as i128;
                    }
                    set_lane(&mut res, esz, k, wrap(r, bits));
                }
            }
            0x08 if u => {
                // SRI
                for k in 0..n {
                    let x = lane(a, esz, k);
                    let keep = if rshift >= bits { 0 } else { mask(1 << esz) >> rshift };
                    let sh = if rshift >= bits { 0 } else { x >> rshift };
                    set_lane(&mut res, esz, k, (lane(d, esz, k) & !keep & mask(1 << esz)) | sh);
                }
            }
            0x0a => {
                // SHL / SLI
                for k in 0..n {
                    let x = lane(a, esz, k);
                    if u {
                        let keep = (1u64 << lshift) - 1;
                        set_lane(&mut res, esz, k, (lane(d, esz, k) & keep) | (x << lshift));
                    } else {
                        set_lane(&mut res, esz, k, x << lshift);
                    }
                }
            }
            0x0c | 0x0e => {
                // SQSHLU (u) / SQSHL / UQSHL (immediate)
                for k in 0..n {
                    let x = lane(a, esz, k);
                    let r = if opc == 0x0c {
                        sat_u(sx(x, bits) << lshift, bits, &mut qc)
                    } else if u {
                        sat_u(ux(x, bits) << lshift, bits, &mut qc)
                    } else {
                        sat_s(sx(x, bits) << lshift, bits, &mut qc)
                    };
                    set_lane(&mut res, esz, k, r);
                }
            }
            0x10..=0x13 => {
                // SHRN / RSHRN / SQSHRN / UQSHRN / SQSHRUN / SQRSHRN...: narrowing right shifts
                if esz == 3 {
                    return Err(self.undef(i));
                }
                let nb = bits; // result element size = immh element size
                let (wesz, wbits) = (esz + 1, bits * 2);
                // for the narrow forms immh encodes the *narrow* element size
                let _ = (wesz, wbits);
                let esz_n = esz;
                let src_esz = esz_n + 1;
                let src_bits = nb * 2;
                let rsh = (2 * nb as i32 - imm) as u32;
                let m = if scalar { 1 } else { 8 >> esz_n };
                for k in 0..m {
                    let x = lane(a, src_esz, k);
                    let v = if u && opc & 0x10 != 0 && opc < 0x12 { sx(x, src_bits) } else if u { ux(x, src_bits) } else { sx(x, src_bits) };
                    let rnd = if opc & 1 == 1 { 1i128 << (rsh - 1) } else { 0 };
                    let sh = (v + rnd) >> rsh;
                    let r = match (opc, u) {
                        (0x10, false) | (0x11, false) => wrap(sh, nb),               // SHRN / RSHRN
                        (0x10, true) | (0x11, true) => sat_u(sh, nb, &mut qc),       // SQSHRUN / SQRSHRUN
                        (_, false) => sat_s(sh, nb, &mut qc),                        // SQSHRN / SQRSHRN
                        (_, true) => sat_u(sh, nb, &mut qc),                         // UQSHRN / UQRSHRN
                    };
                    set_lane(&mut res, esz_n, k, r);
                }
                self.qc(qc);
                self.vr[rd] = if q && !scalar { (d & u64::MAX as u128) | res << 64 } else { res };
                return Ok(());
            }
            0x14 => {
                // SSHLL / USHLL (widening left shift)
                if esz == 3 {
                    return Err(self.undef(i));
                }
                let m = (8 >> esz) as usize;
                let off = if q { m } else { 0 };
                for k in 0..m {
                    let x = sv(lane(a, esz, off + k));
                    set_lane(&mut res, esz + 1, k, wrap(x << lshift, bits * 2));
                }
                self.vr[rd] = res;
                return Ok(());
            }
            0x1c | 0x1f => {
                // fixed-point conversions SCVTF/UCVTF (0x1c), FCVTZS/FCVTZU (0x1f)
                let (f, fesz) = match esz {
                    2 => (F32, 2),
                    3 => (F64, 3),
                    _ => return Err(self.undef(i)),
                };
                let (rmode, mut fl) = (self.fp_rm(), sf::TINY_BEFORE);
                let fbits = rshift as i32;
                for k in 0..n {
                    let x = lane(a, fesz, k);
                    let r = if opc == 0x1c {
                        sf::from_int_scaled(f, x, !u, bits, -fbits, rmode, &mut fl)
                    } else {
                        fp::to_int(f, x, sf::RTZ, !u, bits, fbits, &mut fl)
                    };
                    set_lane(&mut res, fesz, k, r);
                }
                self.fp_raise(fl);
            }
            _ => return Err(self.undef(i)),
        }
        self.qc(qc);
        self.write_vec(rd, q || scalar, res);
        Ok(())
    }

    // ------------------------------------------------------------------ permute / EXT / TBL

    pub(super) fn simd_permute(&mut self, i: u32) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let size = i >> 22 & 3;
        let opc = i >> 12 & 7;
        let (rm, rn, rd) = (((i >> 16) & 31) as usize, ((i >> 5) & 31) as usize, (i & 31) as usize);
        let (a, b) = (self.vr[rn], self.vr[rm]);
        let esz = size;
        let n = ((if q { 16 } else { 8 }) >> esz) as usize;
        if size == 3 && !q {
            return Err(self.undef(i));
        }
        let mut res = 0u128;
        let part = (opc >> 2) as usize; // 0 = "1" variants, 1 = "2" variants
        match opc & 3 {
            1 => {
                // UZP1 / UZP2
                for k in 0..n / 2 {
                    set_lane(&mut res, esz, k, lane(a, esz, 2 * k + part));
                    set_lane(&mut res, esz, n / 2 + k, lane(b, esz, 2 * k + part));
                }
            }
            2 => {
                // TRN1 / TRN2
                for k in 0..n / 2 {
                    set_lane(&mut res, esz, 2 * k, lane(a, esz, 2 * k + part));
                    set_lane(&mut res, esz, 2 * k + 1, lane(b, esz, 2 * k + part));
                }
            }
            3 => {
                // ZIP1 / ZIP2
                let base = part * n / 2;
                for k in 0..n / 2 {
                    set_lane(&mut res, esz, 2 * k, lane(a, esz, base + k));
                    set_lane(&mut res, esz, 2 * k + 1, lane(b, esz, base + k));
                }
            }
            _ => return Err(self.undef(i)),
        }
        self.write_vec(rd, q, res);
        Ok(())
    }

    pub(super) fn simd_ext(&mut self, i: u32) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let imm4 = (i >> 11 & 15) as usize;
        let (rm, rn, rd) = (((i >> 16) & 31) as usize, ((i >> 5) & 31) as usize, (i & 31) as usize);
        let nbytes = if q { 16 } else { 8 };
        if imm4 >= nbytes {
            return Err(self.undef(i));
        }
        let (a, b) = (self.vr[rn], self.vr[rm]);
        let mut res = 0u128;
        for k in 0..nbytes {
            let src = imm4 + k;
            let byte = if src < nbytes { lane(a, 0, src) } else { lane(b, 0, src - nbytes) };
            set_lane(&mut res, 0, k, byte);
        }
        self.write_vec(rd, q, res);
        Ok(())
    }

    pub(super) fn simd_tbl(&mut self, i: u32) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let len = (i >> 13 & 3) as usize + 1;
        let tbx = i >> 12 & 1 == 1;
        let (rm, rn, rd) = (((i >> 16) & 31) as usize, ((i >> 5) & 31) as usize, (i & 31) as usize);
        let idx = self.vr[rm];
        let nbytes = if q { 16 } else { 8 };
        let mut res = 0u128;
        let old = self.vr[rd];
        for k in 0..nbytes {
            let ix = lane(idx, 0, k) as usize;
            let byte = if ix < 16 * len {
                lane(self.vr[(rn + ix / 16) & 31], 0, ix % 16)
            } else if tbx {
                lane(old, 0, k)
            } else {
                0
            };
            set_lane(&mut res, 0, k, byte);
        }
        self.write_vec(rd, q, res);
        Ok(())
    }

    // ------------------------------------------------------------------ vector x indexed element

    pub(super) fn simd_by_element(&mut self, i: u32, scalar: bool) -> Result<(), Ex> {
        let q = i >> 30 & 1 == 1;
        let u = i >> 29 & 1 == 1;
        let size = i >> 22 & 3;
        let l = i >> 21 & 1;
        let m = i >> 20 & 1;
        let rm4 = i >> 16 & 15;
        let opc = i >> 12 & 15;
        let h = i >> 11 & 1;
        let (rn, rd) = (((i >> 5) & 31) as usize, (i & 31) as usize);
        let fp_op = matches!(opc, 1 | 5 | 9);
        let (esz, rm, idx) = if fp_op {
            // size bit 0 selects double
            if size & 1 == 1 {
                (3, (m << 4 | rm4) as usize, h as usize)
            } else {
                (2, (m << 4 | rm4) as usize, (h << 1 | l) as usize)
            }
        } else {
            match size {
                1 => (1, rm4 as usize, (h << 2 | l << 1 | m) as usize),
                2 => (2, (m << 4 | rm4) as usize, (h << 1 | l) as usize),
                _ => return Err(self.undef(i)),
            }
        };
        let (a, d) = (self.vr[rn], self.vr[rd]);
        let y = lane(self.vr[rm], esz, idx);
        let n = if scalar { 1 } else { ((if q { 16 } else { 8 }) >> esz) as usize };
        let bits = bits_of(esz);
        let mut res = 0u128;
        let mut qc = false;
        if fp_op {
            let f = if esz == 3 { F64 } else { F32 };
            let (rmode, dn) = (self.fp_rm(), self.fp_dn());
            let mut fl = sf::TINY_BEFORE;
            if esz == 3 && !q && !scalar {
                return Err(self.undef(i));
            }
            for k in 0..n {
                let x = lane(a, esz, k);
                let r = match (opc, u) {
                    (1, false) => fp::fma(f, x, y, lane(d, esz, k), rmode, dn, &mut fl), // FMLA
                    (5, false) => {
                        let x = sf::neg(f, x);
                        fp::fma(f, x, y, lane(d, esz, k), rmode, dn, &mut fl)
                    }
                    (9, false) => fp::mul(f, x, y, rmode, dn, &mut fl),
                    (9, true) => fp::mulx(f, x, y, rmode, dn, &mut fl),
                    _ => return Err(self.undef(i)),
                };
                set_lane(&mut res, esz, k, r);
            }
            self.fp_raise(fl);
            self.write_vec(rd, q || scalar, res);
            return Ok(());
        }
        match opc {
            0x0 | 0x4 | 0x8 | 0xc | 0xd => {
                for k in 0..n {
                    let x = lane(a, esz, k);
                    let acc = lane(d, esz, k);
                    let r = match opc {
                        0x0 => wrap(acc as i128 + ux(x, bits) * ux(y, bits), bits), // MLA
                        0x4 => wrap(acc as i128 - ux(x, bits) * ux(y, bits), bits), // MLS
                        0x8 => wrap(ux(x, bits) * ux(y, bits), bits),               // MUL
                        _ => {
                            let p = 2 * sx(x, bits) * sx(y, bits) + if opc == 0xd { 1i128 << (bits - 1) } else { 0 };
                            sat_s(p >> bits, bits, &mut qc)
                        }
                    };
                    set_lane(&mut res, esz, k, r);
                }
                self.qc(qc);
                self.write_vec(rd, q || scalar, res);
            }
            0x2 | 0x6 | 0xa | 0x3 | 0x7 | 0xb => {
                // widening: SMLAL SMLSL SMULL (+U) and SQDMLAL SQDMLSL SQDMULL
                let nn = if scalar { 1 } else { (8 >> esz) as usize };
                let off = if q && !scalar { nn } else { 0 };
                let wb = bits * 2;
                for k in 0..nn {
                    let x = lane(a, esz, off + k);
                    let acc = lane(d, esz + 1, k);
                    let (xv, yv) = if u { (ux(x, bits), ux(y, bits)) } else { (sx(x, bits), sx(y, bits)) };
                    let r = match opc {
                        0x2 => acc as i128 + xv * yv,
                        0x6 => acc as i128 - xv * yv,
                        0xa => xv * yv,
                        _ => {
                            let p = sat_s(2 * xv * yv, wb, &mut qc) as i128;
                            let p = sx(p as u64, wb);
                            match opc {
                                0x3 => sat_s(sx(acc, wb) + p, wb, &mut qc) as i128,
                                0x7 => sat_s(sx(acc, wb) - p, wb, &mut qc) as i128,
                                _ => p,
                            }
                        }
                    };
                    set_lane(&mut res, esz + 1, k, wrap(r, wb));
                }
                self.qc(qc);
                self.vr[rd] = res;
            }
            _ => return Err(self.undef(i)),
        }
        Ok(())
    }

    // ------------------------------------------------------------------ scalar copy / dispatch

    pub(super) fn simd_scalar_dup(&mut self, i: u32) -> Result<(), Ex> {
        let imm5 = i >> 16 & 31;
        let (rn, rd) = (((i >> 5) & 31) as usize, (i & 31) as usize);
        if imm5 & 15 == 0 && imm5 != 16 {
            return Err(self.undef(i));
        }
        let esz = imm5.trailing_zeros().min(3);
        let idx = (imm5 >> (esz + 1)) as usize;
        self.vr[rd] = lane(self.vr[rn], esz, idx) as u128;
        Ok(())
    }
}
