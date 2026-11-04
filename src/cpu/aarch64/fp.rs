//! AArch64 scalar floating-point (half/single/double) on top of `softfloat`, with the
//! ARM rules for NaN propagation (quieting, FPCR.DN), rounding-mode selection from
//! FPCR.RMode, cumulative FPSR exception flags and tininess-before-rounding.

use super::simd::mask;
use super::{Aarch64, Ex};
use crate::cpu::softfloat::{self as sf, Fmt, F16, F32, F64};

/// Map softfloat flag bits (NV=16 DZ=8 OF=4 UF=2 NX=1) to FPSR cumulative bits
/// (IOC=0 DZC=1 OFC=2 UFC=3 IXC=4).
pub fn fpsr_bits(fl: u8) -> u32 {
    ((fl & 16) >> 4 | (fl & 8) >> 2 | (fl & 4) | (fl & 2) << 2 | (fl & 1) << 4) as u32
}

pub fn fmt_of(ty: u32) -> Option<Fmt> {
    match ty {
        0 => Some(F32),
        1 => Some(F64),
        3 => Some(F16),
        _ => None,
    }
}

pub fn fmt_bytes(f: Fmt) -> u32 {
    if f.sign_bit() == F64.sign_bit() {
        8
    } else if f.sign_bit() == F32.sign_bit() {
        4
    } else {
        2
    }
}

/// ARM two-operand NaN processing: a signalling NaN wins (quieted), then a quiet NaN.
fn nan2(f: Fmt, a: u64, b: u64, dn: bool, fl: &mut u8) -> Option<u64> {
    let (sa, sb) = (sf::is_snan(f, a), sf::is_snan(f, b));
    if sa || sb {
        *fl |= sf::NV;
    }
    let n = if sa {
        a
    } else if sb {
        b
    } else if sf::is_qnan(f, a) {
        a
    } else if sf::is_qnan(f, b) {
        b
    } else {
        return None;
    };
    Some(if dn { sf::default_nan(f) } else { sf::quiet(f, n) })
}

fn nan1(f: Fmt, a: u64, dn: bool, fl: &mut u8) -> Option<u64> {
    if !sf::is_nan_bits(f, a) {
        return None;
    }
    if sf::is_snan(f, a) {
        *fl |= sf::NV;
    }
    Some(if dn { sf::default_nan(f) } else { sf::quiet(f, a) })
}

pub fn add(f: Fmt, a: u64, b: u64, rm: u8, dn: bool, fl: &mut u8) -> u64 {
    nan2(f, a, b, dn, fl).unwrap_or_else(|| sf::add(f, a, b, rm, fl))
}
pub fn sub(f: Fmt, a: u64, b: u64, rm: u8, dn: bool, fl: &mut u8) -> u64 {
    nan2(f, a, b, dn, fl).unwrap_or_else(|| sf::sub(f, a, b, rm, fl))
}
pub fn mul(f: Fmt, a: u64, b: u64, rm: u8, dn: bool, fl: &mut u8) -> u64 {
    nan2(f, a, b, dn, fl).unwrap_or_else(|| sf::mul(f, a, b, rm, fl))
}
pub fn div(f: Fmt, a: u64, b: u64, rm: u8, dn: bool, fl: &mut u8) -> u64 {
    nan2(f, a, b, dn, fl).unwrap_or_else(|| sf::div(f, a, b, rm, fl))
}
pub fn sqrt(f: Fmt, a: u64, rm: u8, dn: bool, fl: &mut u8) -> u64 {
    nan1(f, a, dn, fl).unwrap_or_else(|| sf::sqrt(f, a, rm, fl))
}

/// FMULX: like multiply, but infinity * zero gives +/-2.0.
pub fn mulx(f: Fmt, a: u64, b: u64, rm: u8, dn: bool, fl: &mut u8) -> u64 {
    if let Some(n) = nan2(f, a, b, dn, fl) {
        return n;
    }
    if (sf::is_inf(f, a) && sf::is_zero(f, b)) || (sf::is_zero(f, a) && sf::is_inf(f, b)) {
        let sign = (a ^ b) & f.sign_bit();
        let two = sf::from_int(f, 2, false, 64, rm, &mut 0);
        return two | sign;
    }
    sf::mul(f, a, b, rm, fl)
}

/// a * b + c (single rounding) with ARM's three-operand NaN rules: signalling NaNs
/// first, then quiet NaNs, each searched in the order addend, op1, op2.
pub fn fma(f: Fmt, a: u64, b: u64, c: u64, rm: u8, dn: bool, fl: &mut u8) -> u64 {
    fma_scaled(f, a, b, c, 0, rm, dn, fl)
}

/// `fma` with the result scaled by 2^scale before the single rounding.
pub fn fma_scaled(f: Fmt, a: u64, b: u64, c: u64, scale: i32, rm: u8, dn: bool, fl: &mut u8) -> u64 {
    let ops = [c, a, b];
    let snan = ops.iter().copied().find(|&x| sf::is_snan(f, x));
    let qnan = ops.iter().copied().find(|&x| sf::is_qnan(f, x));
    let inf_zero = (sf::is_inf(f, a) && sf::is_zero(f, b)) || (sf::is_zero(f, a) && sf::is_inf(f, b));
    if snan.is_some() {
        *fl |= sf::NV;
    }
    let propagated = snan.or(qnan).map(|n| if dn { sf::default_nan(f) } else { sf::quiet(f, n) });
    if sf::is_qnan(f, c) && inf_zero {
        *fl |= sf::NV;
        return sf::default_nan(f);
    }
    match propagated {
        Some(n) => n,
        None => sf::fma_scaled(f, a, b, c, scale, rm, fl),
    }
}

fn max_core(f: Fmt, a: u64, b: u64, max: bool) -> u64 {
    // -0 < +0 for both FMAX and FMIN
    let ord = sf::compare(f, a, b);
    let (za, zb) = (sf::is_zero(f, a), sf::is_zero(f, b));
    if za && zb {
        let (sa, sb) = (a & f.sign_bit() != 0, b & f.sign_bit() != 0);
        return if max { if sa { b } else { a } } else if sa { a } else if sb { b } else { a };
    }
    if (ord == std::cmp::Ordering::Less) == max { b } else { a }
}

/// FMAX/FMIN: any NaN propagates.
pub fn fmax(f: Fmt, a: u64, b: u64, max: bool, dn: bool, fl: &mut u8) -> u64 {
    nan2(f, a, b, dn, fl).unwrap_or_else(|| max_core(f, a, b, max))
}

/// FMAXNM/FMINNM: a quiet NaN loses against a number.
pub fn fmaxnm(f: Fmt, a: u64, b: u64, max: bool, dn: bool, fl: &mut u8) -> u64 {
    let (qa, qb) = (sf::is_qnan(f, a), sf::is_qnan(f, b));
    if sf::is_snan(f, a) || sf::is_snan(f, b) || (qa && qb) {
        return nan2(f, a, b, dn, fl).unwrap();
    }
    if qa {
        return b;
    }
    if qb {
        return a;
    }
    max_core(f, a, b, max)
}

/// FP -> integer with ARM saturation (NaN -> 0). Result is zero-extended from `bits`.
pub fn to_int(f: Fmt, x: u64, rm: u8, signed: bool, bits: u32, scale: i32, fl: &mut u8) -> u64 {
    if sf::is_nan_bits(f, x) {
        *fl |= sf::NV;
        return 0;
    }
    let r = sf::to_int_scaled(f, x, rm, signed, bits, scale, fl);
    if bits == 32 { r as u32 as u64 } else { r }
}

/// FP precision conversion with ARM NaN handling.
pub fn convert(from: Fmt, to: Fmt, x: u64, rm: u8, dn: bool, fl: &mut u8) -> u64 {
    if sf::is_nan_bits(from, x) {
        if sf::is_snan(from, x) {
            *fl |= sf::NV;
        }
        return if dn { sf::default_nan(to) } else { sf::convert_nan(from, to, x) };
    }
    sf::convert(from, to, x, rm, fl)
}

/// Round to an integral value (FRINT*).
pub fn rint(f: Fmt, x: u64, rm: u8, exact: bool, dn: bool, fl: &mut u8) -> u64 {
    nan1(f, x, dn, fl).unwrap_or_else(|| sf::round_to_int(f, x, rm, exact, fl))
}

/// VFPExpandImm
pub fn expand_imm(f: Fmt, imm8: u32) -> u64 {
    let (sign, b6, low) = ((imm8 >> 7) as u64, (imm8 >> 6) as u64 & 1, imm8 as u64 & 0x3f);
    match fmt_bytes(f) {
        8 => sign << 63 | (b6 ^ 1) << 62 | (if b6 == 1 { 0xff } else { 0 }) << 54 | low << 48,
        4 => sign << 31 | (b6 ^ 1) << 30 | (if b6 == 1 { 0x1f } else { 0 }) << 25 | low << 19,
        _ => sign << 15 | (b6 ^ 1) << 14 | (if b6 == 1 { 0x3 } else { 0 }) << 12 | low << 6,
    }
}

impl Aarch64 {
    /// Rounding mode selected by FPCR.RMode, in softfloat's encoding.
    pub(super) fn fp_rm(&self) -> u8 {
        match (self.fpcr >> 22) & 3 {
            0 => sf::RNE,
            1 => sf::RUP,
            2 => sf::RDN,
            _ => sf::RTZ,
        }
    }

    pub(super) fn fp_dn(&self) -> bool {
        self.fpcr >> 25 & 1 == 1
    }

    pub(super) fn fp_raise(&mut self, fl: u8) {
        self.fpsr |= fpsr_bits(fl & 0x1f);
    }

    pub(super) fn fget(&self, f: Fmt, r: usize) -> u64 {
        self.vr[r] as u64 & mask(fmt_bytes(f))
    }

    /// Write a scalar result: the rest of the vector register is zeroed.
    pub(super) fn fput(&mut self, r: usize, v: u64) {
        self.vr[r] = v as u128;
    }

    fn set_nzcv(&mut self, n: u32) {
        self.set_flags((n >> 3 & 1 == 1, n >> 2 & 1 == 1, n >> 1 & 1 == 1, n & 1 == 1));
    }

    /// FCMP/FCMPE result as NZCV bits.
    fn fp_compare(&mut self, f: Fmt, a: u64, b: u64, signal: bool) -> u32 {
        let mut fl = 0u8;
        let (na, nb) = (sf::is_nan_bits(f, a), sf::is_nan_bits(f, b));
        if na || nb {
            if signal || sf::is_snan(f, a) || sf::is_snan(f, b) {
                fl |= sf::NV;
            }
            self.fp_raise(fl);
            return 0b0011;
        }
        match sf::compare(f, a, b) {
            std::cmp::Ordering::Less => 0b1000,
            std::cmp::Ordering::Equal => 0b0110,
            std::cmp::Ordering::Greater => 0b0010,
        }
    }

    /// Scalar floating-point instructions (bits 28:24 = 1111x, bit 30 = 0).
    pub(super) fn fp_scalar(&mut self, i: u32) -> Result<(), Ex> {
        let (rd, rn, rm) = ((i & 31) as usize, ((i >> 5) & 31) as usize, ((i >> 16) & 31) as usize);
        let ty = (i >> 22) & 3;
        let (rmode, dn) = (self.fp_rm(), self.fp_dn());
        let mut fl = sf::TINY_BEFORE;

        if (i >> 24) & 0x1f == 0b11111 {
            // ---- FMADD / FMSUB / FNMADD / FNMSUB
            let f = fmt_of(ty).ok_or_else(|| self.undef(i))?;
            let (o1, o0, ra) = ((i >> 21) & 1, (i >> 15) & 1, ((i >> 10) & 31) as usize);
            let (n, m, a) = (self.fget(f, rn), self.fget(f, rm), self.fget(f, ra));
            // FMADD: a + n*m   FMSUB: a - n*m   FNMADD: -a - n*m   FNMSUB: -a + n*m
            let n = if o0 != o1 { sf::neg(f, n) } else { n };
            let a = if o1 == 1 { sf::neg(f, a) } else { a };
            let r = fma(f, n, m, a, rmode, dn, &mut fl);
            self.fp_raise(fl);
            self.fput(rd, r);
            return Ok(());
        }
        let b21 = (i >> 21) & 1 == 1;
        if !b21 {
            // ---- fixed-point <-> FP conversions
            return self.fp_int_conv(i, true);
        }
        match (i >> 10) & 0x3f {
            0 => return self.fp_int_conv(i, false),
            _ if (i >> 10) & 7 == 0b100 => {
                // ---- FMOV (scalar, immediate)
                let f = fmt_of(ty).ok_or_else(|| self.undef(i))?;
                self.fput(rd, expand_imm(f, (i >> 13) & 0xff));
                return Ok(());
            }
            _ => {}
        }
        let f = fmt_of(ty).ok_or_else(|| self.undef(i))?;
        match (i >> 10) & 3 {
            0 if (i >> 10) & 0xf == 0b1000 => {
                // ---- FCMP / FCMPE
                let a = self.fget(f, rn);
                let (signal, zero) = ((i >> 4) & 1 == 1, (i >> 3) & 1 == 1);
                let b = if zero { 0 } else { self.fget(f, rm) };
                let n = self.fp_compare(f, a, b, signal);
                self.set_nzcv(n);
            }
            0 if (i >> 10) & 0x1f == 0b10000 => {
                // ---- 1-source: FMOV FABS FNEG FSQRT FCVT FRINT*
                let a = self.fget(f, rn);
                let opcode = (i >> 15) & 0x3f;
                let r = match opcode {
                    0 => a,
                    1 => sf::abs(f, a),
                    2 => sf::neg(f, a),
                    3 => sqrt(f, a, rmode, dn, &mut fl),
                    4 | 5 | 7 => {
                        // FCVT between precisions
                        let to = match opcode {
                            4 => F32,
                            5 => F64,
                            _ => F16,
                        };
                        if to.sign_bit() == f.sign_bit() {
                            return Err(self.undef(i));
                        }
                        convert(f, to, a, rmode, dn, &mut fl)
                    }
                    8..=15 => {
                        let (rm2, exact) = match opcode & 7 {
                            0 => (sf::RNE, false),
                            1 => (sf::RUP, false),
                            2 => (sf::RDN, false),
                            3 => (sf::RTZ, false),
                            4 => (sf::RMM, false),
                            6 => (rmode, true),
                            7 => (rmode, false),
                            _ => return Err(self.undef(i)),
                        };
                        rint(f, a, rm2, exact, dn, &mut fl)
                    }
                    _ => return Err(self.undef(i)),
                };
                self.fp_raise(fl);
                self.fput(rd, r);
            }
            1 => {
                // ---- FCCMP / FCCMPE
                let cond = (i >> 12) & 15;
                let nzcv = i & 15;
                if self.cond(cond) {
                    let (a, b) = (self.fget(f, rn), self.fget(f, rm));
                    let n = self.fp_compare(f, a, b, (i >> 4) & 1 == 1);
                    self.set_nzcv(n);
                } else {
                    self.set_nzcv(nzcv);
                }
            }
            2 => {
                // ---- 2-source
                let (a, b) = (self.fget(f, rn), self.fget(f, rm));
                let r = match (i >> 12) & 15 {
                    0 => mul(f, a, b, rmode, dn, &mut fl),
                    1 => div(f, a, b, rmode, dn, &mut fl),
                    2 => add(f, a, b, rmode, dn, &mut fl),
                    3 => sub(f, a, b, rmode, dn, &mut fl),
                    4 => fmax(f, a, b, true, dn, &mut fl),
                    5 => fmax(f, a, b, false, dn, &mut fl),
                    6 => fmaxnm(f, a, b, true, dn, &mut fl),
                    7 => fmaxnm(f, a, b, false, dn, &mut fl),
                    8 => {
                        // FNMUL
                        sf::neg(f, mul(f, a, b, rmode, dn, &mut fl))
                    }
                    _ => return Err(self.undef(i)),
                };
                self.fp_raise(fl);
                self.fput(rd, r);
            }
            _ => {
                // ---- FCSEL
                let cond = (i >> 12) & 15;
                let r = if self.cond(cond) { self.fget(f, rn) } else { self.fget(f, rm) };
                self.fput(rd, r);
            }
        }
        Ok(())
    }

    /// SCVTF / UCVTF / FCVT{N,P,M,Z,A}{S,U} / FMOV between FP and general registers,
    /// in integer (`fixed = false`) or fixed-point (`fixed = true`) form.
    fn fp_int_conv(&mut self, i: u32, fixed: bool) -> Result<(), Ex> {
        let (rd, rn) = ((i & 31) as usize, ((i >> 5) & 31) as usize);
        let sf64 = (i >> 31) & 1 == 1;
        let ty = (i >> 22) & 3;
        let rmode = (i >> 19) & 3;
        let opcode = (i >> 16) & 7;
        let bits = if sf64 { 64 } else { 32 };
        let (cur, dn) = (self.fp_rm(), self.fp_dn());
        let _ = dn;
        let mut fl = sf::TINY_BEFORE;
        // FMOV between registers
        if !fixed && (opcode == 6 || opcode == 7) {
            match (sf64, ty, rmode, opcode) {
                (_, _, 0, 6) => {
                    // FMOV Wd/Xd, Hn/Sn/Dn
                    let f = fmt_of(ty).ok_or_else(|| self.undef(i))?;
                    if (fmt_bytes(f) == 8) != sf64 && f.sign_bit() != F16.sign_bit() {
                        return Err(self.undef(i));
                    }
                    let v = self.fget(f, rn);
                    self.wz(rd, v, sf64);
                }
                (_, _, 0, 7) => {
                    let f = fmt_of(ty).ok_or_else(|| self.undef(i))?;
                    if (fmt_bytes(f) == 8) != sf64 && f.sign_bit() != F16.sign_bit() {
                        return Err(self.undef(i));
                    }
                    let v = self.rz(rn) & mask(fmt_bytes(f));
                    self.fput(rd, v);
                }
                (true, 2, 1, 6) => {
                    let v = (self.vr[rn] >> 64) as u64; // FMOV Xd, Vn.D[1]
                    self.wz(rd, v, true);
                }
                (true, 2, 1, 7) => {
                    let lo = self.vr[rd] as u64; // FMOV Vd.D[1], Xn
                    self.vr[rd] = lo as u128 | (self.rz(rn) as u128) << 64;
                }
                _ => return Err(self.undef(i)),
            }
            return Ok(());
        }
        let f = fmt_of(ty).ok_or_else(|| self.undef(i))?;
        let scale = if fixed { 64 - ((i >> 10) & 63) as i32 } else { 0 };
        if fixed && !sf64 && scale > 32 {
            return Err(self.undef(i)); // fbits > 32 is invalid for the W form
        }
        match (rmode, opcode) {
            (_, 2) | (_, 3) if rmode == 0 => {
                // SCVTF / UCVTF
                let src = self.rz(rn);
                let r = sf::from_int_scaled(f, src, opcode == 2, bits, -scale, cur, &mut fl);
                self.fp_raise(fl);
                self.fput(rd, r);
            }
            (m, 0) | (m, 1) | (m @ 0, 4) | (m @ 0, 5) if !fixed || (m == 3 && opcode < 2) => {
                let rmx = match (m, opcode) {
                    (0, 0) | (0, 1) => sf::RNE,
                    (1, _) => sf::RUP,
                    (2, _) => sf::RDN,
                    (3, _) => sf::RTZ,
                    _ => sf::RMM, // FCVTAS / FCVTAU
                };
                let a = self.fget(f, rn);
                let r = to_int(f, a, rmx, opcode & 1 == 0, bits, scale, &mut fl);
                self.fp_raise(fl);
                self.wz(rd, r, sf64);
            }
            _ => return Err(self.undef(i)),
        }
        Ok(())
    }
}
