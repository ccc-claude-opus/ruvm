//! AArch64 floating-point and Advanced SIMD: register file access, SIMD&FP loads and
//! stores (single register, pairs, literal, structure loads/stores), and the data
//! processing groups (see `fp.rs` / `neon.rs`).

use super::{Aarch64, Ex};
use crate::bus::Bus;

#[inline]
pub fn mask(bytes: u32) -> u64 {
    if bytes >= 8 { !0 } else { (1u64 << (8 * bytes)) - 1 }
}

/// Read lane `i` of width `1 << esz` bytes.
#[inline]
pub fn lane(v: u128, esz: u32, i: usize) -> u64 {
    ((v >> (i << (3 + esz))) as u64) & mask(1 << esz)
}

/// Write lane `i` of width `1 << esz` bytes.
#[inline]
pub fn set_lane(v: &mut u128, esz: u32, i: usize, x: u64) {
    let sh = i << (3 + esz);
    let m = (mask(1 << esz) as u128) << sh;
    *v = (*v & !m) | (((x & mask(1 << esz)) as u128) << sh);
}

impl Aarch64 {
    /// Load `1 << esz` bytes (up to 16) from `addr` into the low bits of a vector value.
    pub(super) fn vload(&mut self, bus: &mut Bus, addr: u64, bytes: u32) -> Result<u128, Ex> {
        if bytes == 16 {
            let lo = self.rd(bus, addr, 8)?;
            let hi = self.rd(bus, addr.wrapping_add(8), 8)?;
            Ok(lo as u128 | (hi as u128) << 64)
        } else {
            Ok(self.rd(bus, addr, bytes as usize)? as u128)
        }
    }

    pub(super) fn vstore(&mut self, bus: &mut Bus, addr: u64, bytes: u32, v: u128) -> Result<(), Ex> {
        if bytes == 16 {
            self.wr(bus, addr, 8, v as u64)?;
            self.wr(bus, addr.wrapping_add(8), 8, (v >> 64) as u64)
        } else {
            self.wr(bus, addr, bytes as usize, v as u64 & mask(bytes))
        }
    }

    /// Single SIMD&FP register load/store at a computed address.
    pub(super) fn fp_ldst(&mut self, bus: &mut Bus, i: u32, addr: u64) -> Result<(), Ex> {
        let (size, opc, rt) = (i >> 30, (i >> 22) & 3, (i & 31) as usize);
        let bytes = match (size, opc >> 1) {
            (0, 1) => 16,
            (_, 1) => return Err(self.undef(i)),
            (s, _) => 1u32 << s,
        };
        if opc & 1 == 1 {
            self.vr[rt] = self.vload(bus, addr, bytes)?;
        } else {
            let v = self.vr[rt];
            self.vstore(bus, addr, bytes, v)?;
        }
        Ok(())
    }

    /// LDP / STP / LDNP / STNP (SIMD&FP).
    pub(super) fn fp_pair(&mut self, bus: &mut Bus, i: u32) -> Result<(), Ex> {
        let opc = i >> 30;
        if opc == 3 {
            return Err(self.undef(i));
        }
        let bytes = 4u32 << opc;
        let scale = 2 + opc;
        let load = (i >> 22) & 1 == 1;
        let off = super::sext(((i >> 15) & 0x7f) as u64, 7) << scale;
        let (rt, rt2, rn) = ((i & 31) as usize, ((i >> 10) & 31) as usize, ((i >> 5) & 31) as usize);
        let base = self.rs(rn);
        let (addr, wb) = match (i >> 23) & 3 {
            1 => (base, Some(base.wrapping_add(off))),
            0 | 2 => (base.wrapping_add(off), None),
            _ => (base.wrapping_add(off), Some(base.wrapping_add(off))),
        };
        if load {
            let a = self.vload(bus, addr, bytes)?;
            let b = self.vload(bus, addr.wrapping_add(bytes as u64), bytes)?;
            self.vr[rt] = a;
            self.vr[rt2] = b;
        } else {
            let (a, b) = (self.vr[rt], self.vr[rt2]);
            self.vstore(bus, addr, bytes, a)?;
            self.vstore(bus, addr.wrapping_add(bytes as u64), bytes, b)?;
        }
        if let Some(w) = wb {
            self.ws(rn, w, true);
        }
        Ok(())
    }

    /// LDR (literal), SIMD&FP.
    pub(super) fn fp_literal(&mut self, bus: &mut Bus, i: u32) -> Result<(), Ex> {
        let opc = i >> 30;
        if opc == 3 {
            return Err(self.undef(i));
        }
        let addr = self.pc.wrapping_add(super::sext(((i >> 5) & 0x7ffff) as u64, 19) << 2);
        self.vr[(i & 31) as usize] = self.vload(bus, addr, 4 << opc)?;
        Ok(())
    }

    /// Advanced SIMD load/store multiple structures (LD1-LD4 / ST1-ST4), with optional
    /// post-index, and single structures (lane forms and LDnR).
    pub(super) fn simd_structure(&mut self, bus: &mut Bus, i: u32) -> Result<(), Ex> {
        let q = (i >> 30) & 1 == 1;
        let load = (i >> 22) & 1 == 1;
        let post = (i >> 23) & 1 == 1;
        let rn = ((i >> 5) & 31) as usize;
        let rm = ((i >> 16) & 31) as usize;
        let rt = (i & 31) as usize;
        let base = self.rs(rn);
        let single = (i >> 24) & 1 == 1;
        let total;
        if !single {
            let size = (i >> 10) & 3;
            let (nregs, selem, interleave) = match (i >> 12) & 15 {
                0b0000 => (4, 4, true),
                0b0010 => (4, 1, false),
                0b0100 => (3, 3, true),
                0b0110 => (3, 1, false),
                0b0111 => (1, 1, false),
                0b1000 => (2, 2, true),
                0b1010 => (2, 1, false),
                _ => return Err(self.undef(i)),
            };
            if size == 3 && !q && selem > 1 {
                return Err(self.undef(i)); // 1D arrangement is reserved for LD2-LD4
            }
            let ebytes = 1u32 << size;
            let rbytes = if q { 16u32 } else { 8 };
            let elems = (rbytes / ebytes) as usize;
            total = (nregs * rbytes) as u64;
            let mut addr = base;
            if interleave {
                for e in 0..elems {
                    for s in 0..selem as usize {
                        let r = (rt + s) & 31;
                        if load {
                            let x = self.rd(bus, addr, ebytes as usize)?;
                            if e == 0 {
                                self.vr[r] = 0;
                            }
                            set_lane(&mut self.vr[r], size, e, x);
                        } else {
                            let x = lane(self.vr[r], size, e);
                            self.wr(bus, addr, ebytes as usize, x)?;
                        }
                        addr = addr.wrapping_add(ebytes as u64);
                    }
                }
            } else {
                for k in 0..nregs as usize {
                    let r = (rt + k) & 31;
                    if load {
                        let x = self.vload(bus, addr, rbytes)?;
                        self.vr[r] = x;
                    } else {
                        let x = self.vr[r];
                        self.vstore(bus, addr, rbytes, x)?;
                    }
                    addr = addr.wrapping_add(rbytes as u64);
                }
            }
        } else {
            let r = (i >> 21) & 1;
            let opcode = (i >> 13) & 7;
            let s = (i >> 12) & 1;
            let size = (i >> 10) & 3;
            let selem = ((((opcode & 1) << 1) | r) + 1) as usize;
            let qi = q as u32;
            let (esz, index, replicate) = match opcode >> 1 {
                0 => (0, (qi << 3 | s << 2 | size) as usize, false),
                1 => {
                    if size & 1 == 1 {
                        return Err(self.undef(i));
                    }
                    (1, (qi << 2 | s << 1 | size >> 1) as usize, false)
                }
                2 => match size {
                    0 => (2, (qi << 1 | s) as usize, false),
                    1 => {
                        if s == 1 {
                            return Err(self.undef(i));
                        }
                        (3, qi as usize, false)
                    }
                    _ => return Err(self.undef(i)),
                },
                _ => {
                    if !load || s == 1 {
                        return Err(self.undef(i));
                    }
                    (size, 0, true)
                }
            };
            let ebytes = 1u32 << esz;
            total = (selem as u64) * ebytes as u64;
            let mut addr = base;
            for k in 0..selem {
                let reg = (rt + k) & 31;
                if replicate {
                    let x = self.rd(bus, addr, ebytes as usize)?;
                    let n = (if q { 16 } else { 8 } / ebytes) as usize;
                    let mut v = 0u128;
                    for e in 0..n {
                        set_lane(&mut v, esz, e, x);
                    }
                    self.vr[reg] = v;
                } else if load {
                    let x = self.rd(bus, addr, ebytes as usize)?;
                    set_lane(&mut self.vr[reg], esz, index, x);
                } else {
                    let x = lane(self.vr[reg], esz, index);
                    self.wr(bus, addr, ebytes as usize, x)?;
                }
                addr = addr.wrapping_add(ebytes as u64);
            }
        }
        if post {
            let inc = if rm == 31 { total } else { self.rz(rm) };
            self.ws(rn, base.wrapping_add(inc), true);
        }
        Ok(())
    }
}

impl Aarch64 {
    /// Scalar floating-point and Advanced SIMD data-processing instructions.
    pub(super) fn fpsimd(&mut self, _bus: &mut Bus, i: u32) -> Result<(), Ex> {
        let top5 = (i >> 24) & 0x1f;
        let b = |n: u32| (i >> n) & 1;
        if b(30) == 0 && b(29) == 0 && (top5 == 0b11110 || top5 == 0b11111) {
            return self.fp_scalar(i);
        }
        let bits = |hi: u32, lo: u32| (i >> lo) & ((1 << (hi - lo + 1)) - 1);
        if b(31) == 0 && b(30) == 1 && top5 == 0b11110 || b(31) == 0 && b(30) == 1 && top5 == 0b11111 {
            // ---- Advanced SIMD scalar
            return match top5 {
                0b11110 if b(21) == 1 && b(10) == 1 => self.simd_three_same(i, true),
                0b11110 if bits(21, 17) == 0b10000 && bits(11, 10) == 0b10 => self.simd_two_reg(i, true),
                0b11110 if bits(21, 17) == 0b11000 && bits(11, 10) == 0b10 => self.simd_scalar_pairwise(i),
                0b11110 if b(21) == 0 && bits(23, 21) == 0 && b(10) == 1 && bits(14, 11) == 0 => self.simd_scalar_dup(i),
                0b11111 if b(10) == 1 => self.simd_shift_imm(i, true),
                0b11111 => self.simd_by_element(i, true),
                _ => Err(self.undef(i)),
            };
        }
        if b(31) == 0 {
            // ---- Advanced SIMD vector
            return match top5 {
                0b01110 => {
                    if b(21) == 1 {
                        match bits(11, 10) {
                            _ if b(10) == 1 => self.simd_three_same(i, false),
                            0b00 => self.simd_three_diff(i),
                            _ if bits(21, 17) == 0b10000 => self.simd_two_reg(i, false),
                            _ if bits(21, 17) == 0b11000 => self.simd_across(i),
                            _ => Err(self.undef(i)),
                        }
                    } else if b(29) == 0 && b(21) == 0 && b(15) == 0 && bits(11, 10) == 0b10 {
                        self.simd_permute(i)
                    } else if b(29) == 0 && bits(23, 22) == 0 && b(15) == 0 && bits(11, 10) == 0b00 {
                        self.simd_tbl(i)
                    } else if b(29) == 1 && b(15) == 0 && b(10) == 0 && bits(23, 22) == 0 {
                        self.simd_ext(i)
                    } else if bits(23, 21) == 0 && b(15) == 0 && b(10) == 1 {
                        self.simd_copy(i)
                    } else {
                        Err(self.undef(i))
                    }
                }
                0b01111 => {
                    if b(10) == 0 {
                        self.simd_by_element(i, false)
                    } else if bits(22, 19) == 0 {
                        self.simd_modimm(i)
                    } else {
                        self.simd_shift_imm(i, false)
                    }
                }
                _ => Err(self.undef(i)),
            };
        }
        Err(self.undef(i))
    }
}
