//! AArch64 (ARMv8-A, A64) integer subset, single core, flat physical memory.
//!
//! Supported: data-processing (immediate & register), bitfield/extract, move-wide,
//! ADR/ADRP, all integer loads/stores (imm, register offset, pre/post-index, pairs,
//! literal), branches, CSEL family, CCMP/CCMN, MUL/DIV/MADD, CLZ/REV/RBIT,
//! NOP/barriers, minimal MRS/MSR, SVC (host syscalls), HVC/SMC (PSCI power-off).
//! Not supported: SIMD/FP, exclusives/atomics, MMU, exception levels, interrupts.

use super::{sext, Cpu};
use crate::bus::Bus;
use crate::host::{self, Host};
use crate::machine::Stop;
use std::collections::HashMap;

const SYS_NZCV: u32 = 23056; // S3_3_C4_C2_0
const SYS_CURRENT_EL: u32 = 16914; // S3_0_C4_C2_2

pub struct Aarch64 {
    x: [u64; 31],
    sp: u64,
    pc: u64,
    n: bool,
    z: bool,
    c: bool,
    v: bool,
    sysregs: HashMap<u32, u64>,
}

fn ones(n: u32) -> u64 {
    if n >= 64 { !0 } else { (1u64 << n) - 1 }
}

fn ror(v: u64, r: u32, size: u32) -> u64 {
    if size == 32 {
        (v as u32).rotate_right(r) as u64
    } else {
        v.rotate_right(r)
    }
}

/// ARM ARM `DecodeBitMasks`: returns (wmask, tmask).
fn decode_bitmasks(n: u32, imms: u32, immr: u32, immediate: bool, datasize: u32) -> Option<(u64, u64)> {
    let x = (n << 6) | (!imms & 0x3f);
    if x == 0 {
        return None;
    }
    let len = 31 - x.leading_zeros();
    if len < 1 || (1u32 << len) > datasize {
        return None;
    }
    let esize = 1u32 << len;
    let levels = esize - 1;
    if immediate && (imms & levels) == levels {
        return None;
    }
    let s = imms & levels;
    let r = immr & levels;
    let d = s.wrapping_sub(r) & levels;
    let welem = ones(s + 1);
    let telem = ones(d + 1);
    let wrot = if r == 0 { welem } else { ((welem >> r) | (welem << (esize - r))) & ones(esize) };
    let rep = |e: u64| {
        let mut o = 0u64;
        let mut p = 0;
        while p < datasize {
            o |= e << p;
            p += esize;
        }
        o
    };
    Some((rep(wrot), rep(telem)))
}

fn shift_val(v: u64, kind: u32, amt: u32, sf: bool) -> u64 {
    if sf {
        let a = amt & 63;
        match kind {
            0 => v << a,
            1 => v >> a,
            2 => ((v as i64) >> a) as u64,
            _ => v.rotate_right(a),
        }
    } else {
        let a = amt & 31;
        let w = v as u32;
        (match kind {
            0 => w << a,
            1 => w >> a,
            2 => ((w as i32) >> a) as u32,
            _ => w.rotate_right(a),
        }) as u64
    }
}

fn extend(v: u64, opt: u32) -> u64 {
    match opt {
        0 => v & 0xff,
        1 => v & 0xffff,
        2 => v & 0xffff_ffff,
        4 => sext(v & 0xff, 8),
        5 => sext(v & 0xffff, 16),
        6 => sext(v & 0xffff_ffff, 32),
        _ => v,
    }
}

/// AddWithCarry; returns (result, (N, Z, C, V)).
fn awc(a: u64, b: u64, cin: bool, sf: bool) -> (u64, (bool, bool, bool, bool)) {
    if sf {
        let (r1, c1) = a.overflowing_add(b);
        let (r, c2) = r1.overflowing_add(cin as u64);
        let v = ((a ^ r) & (b ^ r)) >> 63 == 1;
        (r, (r >> 63 == 1, r == 0, c1 || c2, v))
    } else {
        let (a, b) = (a as u32, b as u32);
        let (r1, c1) = a.overflowing_add(b);
        let (r, c2) = r1.overflowing_add(cin as u32);
        let v = ((a ^ r) & (b ^ r)) >> 31 == 1;
        (r as u64, (r >> 31 == 1, r == 0, c1 || c2, v))
    }
}

impl Aarch64 {
    pub fn new() -> Self {
        Aarch64 { x: [0; 31], sp: 0, pc: 0, n: false, z: false, c: false, v: false, sysregs: HashMap::new() }
    }

    #[inline]
    fn rz(&self, r: usize) -> u64 {
        if r == 31 { 0 } else { self.x[r] }
    }
    #[inline]
    fn rs(&self, r: usize) -> u64 {
        if r == 31 { self.sp } else { self.x[r] }
    }
    #[inline]
    fn wz(&mut self, r: usize, v: u64, sf: bool) {
        if r != 31 {
            self.x[r] = if sf { v } else { v as u32 as u64 };
        }
    }
    #[inline]
    fn ws(&mut self, r: usize, v: u64, sf: bool) {
        let v = if sf { v } else { v as u32 as u64 };
        if r == 31 { self.sp = v } else { self.x[r] = v }
    }

    fn set_flags(&mut self, f: (bool, bool, bool, bool)) {
        self.n = f.0;
        self.z = f.1;
        self.c = f.2;
        self.v = f.3;
    }

    fn nzcv(&self) -> u64 {
        ((self.n as u64) << 31) | ((self.z as u64) << 30) | ((self.c as u64) << 29) | ((self.v as u64) << 28)
    }

    fn cond(&self, c: u32) -> bool {
        let r = match c >> 1 {
            0 => self.z,
            1 => self.c,
            2 => self.n,
            3 => self.v,
            4 => self.c && !self.z,
            5 => self.n == self.v,
            6 => self.n == self.v && !self.z,
            _ => true,
        };
        if c & 1 == 1 && c != 15 { !r } else { r }
    }

    fn rd(&self, bus: &mut Bus, addr: u64, n: usize) -> Result<u64, Stop> {
        bus.read(addr, n)
            .map_err(|e| Stop::Fatal(format!("data abort: {} (pc={:#x})", e, self.pc)))
    }
    fn wr(&self, bus: &mut Bus, addr: u64, n: usize, v: u64) -> Result<(), Stop> {
        bus.write(addr, n, v)
            .map_err(|e| Stop::Fatal(format!("data abort: {} (pc={:#x})", e, self.pc)))
    }

    fn undef(&self, i: u32) -> Stop {
        Stop::Fatal(format!("undefined instruction {:#010x} at pc={:#x}", i, self.pc))
    }

    #[allow(clippy::too_many_arguments)]
    fn addsub(&mut self, rd: usize, a: u64, b: u64, sub: bool, s: bool, sf: bool, rd_sp: bool) {
        let (res, fl) = if sub { awc(a, !b, true, sf) } else { awc(a, b, false, sf) };
        if s {
            self.set_flags(fl);
        }
        if rd_sp { self.ws(rd, res, sf) } else { self.wz(rd, res, sf) }
    }

    /// Single-register load/store; `opc`/`size` come from the instruction word.
    fn ldst(&mut self, bus: &mut Bus, i: u32, addr: u64) -> Result<(), Stop> {
        let size = i >> 30;
        let opc = (i >> 22) & 3;
        let rt = (i & 31) as usize;
        let n = 1usize << size;
        match opc {
            0 => {
                let v = self.rz(rt);
                self.wr(bus, addr, n, v)
            }
            1 => {
                let v = self.rd(bus, addr, n)?;
                self.wz(rt, v, true);
                Ok(())
            }
            2 if size == 3 => Ok(()), // PRFM
            2 => {
                let v = sext(self.rd(bus, addr, n)?, n as u32 * 8);
                self.wz(rt, v, true);
                Ok(())
            }
            _ if size < 2 => {
                let v = sext(self.rd(bus, addr, n)?, n as u32 * 8);
                self.wz(rt, v, false);
                Ok(())
            }
            _ => Err(self.undef(i)),
        }
    }

    fn exec(&mut self, bus: &mut Bus) -> Result<(), Stop> {
        let pc = self.pc;
        let i = self.rd(bus, pc, 4)? as u32;
        let mut next = pc.wrapping_add(4);

        let sf = i >> 31 == 1;
        let rd = (i & 31) as usize;
        let rn = ((i >> 5) & 31) as usize;
        let rm = ((i >> 16) & 31) as usize;

        if (i >> 23) & 0x3f == 0b100101 {
            // ---- MOVN / MOVZ / MOVK
            let opc = (i >> 29) & 3;
            let hw = (i >> 21) & 3;
            if !sf && hw > 1 {
                return Err(self.undef(i));
            }
            let sh = hw * 16;
            let imm = ((i >> 5) & 0xffff) as u64;
            let res = match opc {
                0 => !(imm << sh),
                2 => imm << sh,
                3 => (self.rz(rd) & !(0xffffu64 << sh)) | (imm << sh),
                _ => return Err(self.undef(i)),
            };
            self.wz(rd, res, sf);
        } else if (i >> 24) & 0x1f == 0b10001 {
            // ---- ADD/SUB (immediate)
            let sub = (i >> 30) & 1 == 1;
            let s = (i >> 29) & 1 == 1;
            let mut imm = ((i >> 10) & 0xfff) as u64;
            if (i >> 22) & 1 == 1 {
                imm <<= 12;
            }
            let a = self.rs(rn);
            self.addsub(rd, a, imm, sub, s, sf, !s);
        } else if (i >> 24) & 0x1f == 0b10000 {
            // ---- ADR / ADRP
            let immlo = ((i >> 29) & 3) as u64;
            let immhi = ((i >> 5) & 0x7ffff) as u64;
            let imm = sext((immhi << 2) | immlo, 21);
            let res = if sf { (pc & !0xfff).wrapping_add(imm << 12) } else { pc.wrapping_add(imm) };
            self.wz(rd, res, true);
        } else if (i >> 23) & 0x3f == 0b100100 {
            // ---- AND/ORR/EOR/ANDS (immediate)
            let opc = (i >> 29) & 3;
            let n = (i >> 22) & 1;
            if !sf && n == 1 {
                return Err(self.undef(i));
            }
            let (wmask, _) = decode_bitmasks(n, (i >> 10) & 0x3f, (i >> 16) & 0x3f, true, if sf { 64 } else { 32 })
                .ok_or_else(|| self.undef(i))?;
            let a = self.rz(rn);
            let res = match opc {
                0 | 3 => a & wmask,
                1 => a | wmask,
                _ => a ^ wmask,
            };
            let res = if sf { res } else { res as u32 as u64 };
            if opc == 3 {
                self.set_flags((res >> (if sf { 63 } else { 31 }) & 1 == 1, res == 0, false, false));
                self.wz(rd, res, sf);
            } else {
                self.ws(rd, res, sf);
            }
        } else if (i >> 23) & 0x3f == 0b100110 {
            // ---- SBFM / BFM / UBFM
            let opc = (i >> 29) & 3;
            let (n, immr, imms) = ((i >> 22) & 1, (i >> 16) & 0x3f, (i >> 10) & 0x3f);
            let ds = if sf { 64 } else { 32 };
            if (sf as u32) != n {
                return Err(self.undef(i));
            }
            let (wmask, tmask) = decode_bitmasks(n, imms, immr, false, ds).ok_or_else(|| self.undef(i))?;
            let src = if sf { self.rz(rn) } else { self.rz(rn) as u32 as u64 };
            let rot = ror(src, immr, ds);
            let res = match opc {
                0 => {
                    let top = if (src >> imms) & 1 == 1 { !0u64 } else { 0 };
                    (top & !tmask) | (rot & wmask & tmask)
                }
                1 => {
                    let dst = self.rz(rd);
                    let bot = (dst & !wmask) | (rot & wmask);
                    (dst & !tmask) | (bot & tmask)
                }
                2 => rot & wmask & tmask,
                _ => return Err(self.undef(i)),
            };
            self.wz(rd, res, sf);
        } else if (i >> 23) & 0x3f == 0b100111 {
            // ---- EXTR (ROR immediate when Rn == Rm)
            let lsb = (i >> 10) & 0x3f;
            let res = if sf {
                (((self.rz(rn) as u128) << 64 | self.rz(rm) as u128) >> lsb) as u64
            } else {
                (((self.rz(rn) as u32 as u64) << 32 | self.rz(rm) as u32 as u64) >> (lsb & 31)) as u32 as u64
            };
            self.wz(rd, res, sf);
        } else if (i >> 24) & 0x1f == 0b01011 {
            // ---- ADD/SUB (shifted or extended register)
            let sub = (i >> 30) & 1 == 1;
            let s = (i >> 29) & 1 == 1;
            if (i >> 21) & 1 == 0 {
                let sh = (i >> 22) & 3;
                if sh == 3 {
                    return Err(self.undef(i));
                }
                let b = shift_val(self.rz(rm), sh, (i >> 10) & 0x3f, sf);
                let a = self.rz(rn);
                self.addsub(rd, a, b, sub, s, sf, false);
            } else {
                let b = extend(self.rz(rm), (i >> 13) & 7) << ((i >> 10) & 7);
                let a = self.rs(rn);
                self.addsub(rd, a, b, sub, s, sf, !s);
            }
        } else if (i >> 24) & 0x1f == 0b01010 {
            // ---- AND/ORR/EOR/ANDS/BIC/ORN/EON/BICS (shifted register)
            let opc = (i >> 29) & 3;
            let mut b = shift_val(self.rz(rm), (i >> 22) & 3, (i >> 10) & 0x3f, sf);
            if (i >> 21) & 1 == 1 {
                b = !b;
            }
            let a = self.rz(rn);
            let res = match opc {
                0 | 3 => a & b,
                1 => a | b,
                _ => a ^ b,
            };
            let res = if sf { res } else { res as u32 as u64 };
            if opc == 3 {
                self.set_flags((res >> (if sf { 63 } else { 31 }) & 1 == 1, res == 0, false, false));
            }
            self.wz(rd, res, sf);
        } else if (i >> 21) & 0xff == 0b11010000 {
            // ---- ADC / ADCS / SBC / SBCS
            let sub = (i >> 30) & 1 == 1;
            let s = (i >> 29) & 1 == 1;
            let mut b = self.rz(rm);
            if sub {
                b = !b;
            }
            let (res, fl) = awc(self.rz(rn), b, self.c, sf);
            if s {
                self.set_flags(fl);
            }
            self.wz(rd, res, sf);
        } else if (i >> 21) & 0xff == 0b11010010 && (i >> 29) & 1 == 1 {
            // ---- CCMP / CCMN
            let sub = (i >> 30) & 1 == 1;
            if self.cond((i >> 12) & 0xf) {
                let b = if (i >> 11) & 1 == 1 { rm as u64 } else { self.rz(rm) };
                let a = self.rz(rn);
                let (_, fl) = if sub { awc(a, !b, true, sf) } else { awc(a, b, false, sf) };
                self.set_flags(fl);
            } else {
                let f = i & 0xf;
                self.set_flags((f & 8 != 0, f & 4 != 0, f & 2 != 0, f & 1 != 0));
            }
        } else if (i >> 21) & 0xff == 0b11010100 && (i >> 29) & 1 == 0 {
            // ---- CSEL / CSINC / CSINV / CSNEG
            let op = (i >> 30) & 1;
            let o2 = (i >> 10) & 1;
            let res = if self.cond((i >> 12) & 0xf) {
                self.rz(rn)
            } else {
                let m = self.rz(rm);
                match (op, o2) {
                    (0, 0) => m,
                    (0, _) => m.wrapping_add(1),
                    (_, 0) => !m,
                    _ => m.wrapping_neg(),
                }
            };
            self.wz(rd, res, sf);
        } else if (i >> 21) & 0xff == 0b11010110 && (i >> 29) & 1 == 0 {
            let opcode = (i >> 10) & 0x3f;
            let a = self.rz(rn);
            if (i >> 30) & 1 == 0 {
                // ---- UDIV / SDIV / LSLV / LSRV / ASRV / RORV
                let b = self.rz(rm);
                let res = match opcode {
                    2 if sf => if b == 0 { 0 } else { a / b },
                    2 => { let (x, y) = (a as u32, b as u32); if y == 0 { 0 } else { (x / y) as u64 } }
                    3 if sf => if b == 0 { 0 } else { (a as i64).wrapping_div(b as i64) as u64 },
                    3 => { let (x, y) = (a as i32, b as i32); if y == 0 { 0 } else { x.wrapping_div(y) as u32 as u64 } }
                    8..=11 => shift_val(a, opcode - 8, b as u32, sf),
                    _ => return Err(self.undef(i)),
                };
                self.wz(rd, res, sf);
            } else {
                // ---- RBIT / REV / CLZ
                let res = match (opcode, sf) {
                    (0, true) => a.reverse_bits(),
                    (0, false) => (a as u32).reverse_bits() as u64,
                    (2, false) => (a as u32).swap_bytes() as u64,
                    (3, true) => a.swap_bytes(),
                    (4, true) => a.leading_zeros() as u64,
                    (4, false) => (a as u32).leading_zeros() as u64,
                    _ => return Err(self.undef(i)),
                };
                self.wz(rd, res, sf);
            }
        } else if (i >> 24) & 0x1f == 0b11011 {
            // ---- MADD / MSUB / SMADDL / UMADDL / SMULH / UMULH ...
            let o0 = (i >> 15) & 1;
            let (a, b, c) = (self.rz(rn), self.rz(rm), self.rz(((i >> 10) & 31) as usize));
            let acc = |p: u64| if o0 == 0 { c.wrapping_add(p) } else { c.wrapping_sub(p) };
            let res = match (i >> 21) & 7 {
                0 => acc(a.wrapping_mul(b)),
                1 if sf => acc((a as i32 as i64).wrapping_mul(b as i32 as i64) as u64),
                2 if sf && o0 == 0 => (((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64,
                5 if sf => acc((a as u32 as u64).wrapping_mul(b as u32 as u64)),
                6 if sf && o0 == 0 => (((a as u128) * (b as u128)) >> 64) as u64,
                _ => return Err(self.undef(i)),
            };
            self.wz(rd, res, sf);
        } else if i >> 26 == 0b000101 || i >> 26 == 0b100101 {
            // ---- B / BL
            if i >> 31 == 1 {
                self.x[30] = next;
            }
            next = pc.wrapping_add(sext((i & 0x03ff_ffff) as u64, 26) << 2);
        } else if i >> 24 == 0x54 && i & 0x10 == 0 {
            // ---- B.cond
            if self.cond(i & 0xf) {
                next = pc.wrapping_add(sext(((i >> 5) & 0x7ffff) as u64, 19) << 2);
            }
        } else if (i >> 25) & 0x3f == 0b011010 {
            // ---- CBZ / CBNZ
            let v = self.rz(rd);
            let v = if sf { v } else { v as u32 as u64 };
            if (v == 0) != ((i >> 24) & 1 == 1) {
                next = pc.wrapping_add(sext(((i >> 5) & 0x7ffff) as u64, 19) << 2);
            }
        } else if (i >> 25) & 0x3f == 0b011011 {
            // ---- TBZ / TBNZ
            let bit = ((i >> 31) << 5) | ((i >> 19) & 0x1f);
            let set = (self.rz(rd) >> bit) & 1 == 1;
            if set == ((i >> 24) & 1 == 1) {
                next = pc.wrapping_add(sext(((i >> 5) & 0x3fff) as u64, 14) << 2);
            }
        } else if matches!(i & 0xffff_fc1f, 0xd61f_0000 | 0xd63f_0000 | 0xd65f_0000) {
            // ---- BR / BLR / RET
            let target = self.rz(rn);
            if i & 0xffff_fc1f == 0xd63f_0000 {
                self.x[30] = next;
            }
            next = target;
        } else if i & 0x3e00_0000 == 0x3800_0000 {
            // ---- LDR/STR family (single register, integer)
            let size = i >> 30;
            let base = self.rs(rn);
            if (i >> 24) & 1 == 1 {
                let addr = base.wrapping_add((((i >> 10) & 0xfff) as u64) << size);
                self.ldst(bus, i, addr)?;
            } else if (i >> 21) & 1 == 0 {
                let imm9 = sext(((i >> 12) & 0x1ff) as u64, 9);
                let (addr, wb) = match (i >> 10) & 3 {
                    0 => (base.wrapping_add(imm9), None),
                    1 => (base, Some(base.wrapping_add(imm9))),
                    3 => (base.wrapping_add(imm9), Some(base.wrapping_add(imm9))),
                    _ => return Err(self.undef(i)),
                };
                self.ldst(bus, i, addr)?;
                if let Some(w) = wb {
                    self.ws(rn, w, true);
                }
            } else if (i >> 10) & 3 == 2 {
                let mut off = extend(self.rz(rm), (i >> 13) & 7);
                if (i >> 12) & 1 == 1 {
                    off <<= size;
                }
                self.ldst(bus, i, base.wrapping_add(off))?;
            } else {
                return Err(self.undef(i));
            }
        } else if i & 0x3e00_0000 == 0x2800_0000 {
            // ---- LDP / STP / LDPSW
            let opc = i >> 30;
            if opc == 3 {
                return Err(self.undef(i));
            }
            let load = (i >> 22) & 1 == 1;
            let scale = if opc == 2 { 3 } else { 2 };
            let off = sext(((i >> 15) & 0x7f) as u64, 7) << scale;
            let rt2 = ((i >> 10) & 31) as usize;
            let base = self.rs(rn);
            let (addr, wb) = match (i >> 23) & 3 {
                1 => (base, Some(base.wrapping_add(off))),
                2 => (base.wrapping_add(off), None),
                3 => (base.wrapping_add(off), Some(base.wrapping_add(off))),
                _ => return Err(self.undef(i)),
            };
            let n = 1usize << scale;
            if load {
                let (mut v1, mut v2) = (self.rd(bus, addr, n)?, self.rd(bus, addr.wrapping_add(n as u64), n)?);
                if opc == 1 {
                    v1 = sext(v1, 32);
                    v2 = sext(v2, 32);
                }
                self.wz(rd, v1, true);
                self.wz(rt2, v2, true);
            } else {
                let (v1, v2) = (self.rz(rd), self.rz(rt2));
                self.wr(bus, addr, n, v1)?;
                self.wr(bus, addr.wrapping_add(n as u64), n, v2)?;
            }
            if let Some(w) = wb {
                self.ws(rn, w, true);
            }
        } else if i & 0x3f00_0000 == 0x1800_0000 {
            // ---- LDR (literal)
            let opc = i >> 30;
            let addr = pc.wrapping_add(sext(((i >> 5) & 0x7ffff) as u64, 19) << 2);
            let v = match opc {
                0 => self.rd(bus, addr, 4)?,
                1 => self.rd(bus, addr, 8)?,
                2 => sext(self.rd(bus, addr, 4)?, 32),
                _ => 0, // PRFM
            };
            if opc != 3 {
                self.wz(rd, v, true);
            }
        } else if i & 0xfffff01f == 0xd503201f || i & 0xfffff01f == 0xd503301f || i & 0xfff8f01f == 0xd500401f {
            // ---- NOP / hints / barriers / MSR (immediate): ignored
        } else if i & 0xfff0_0000 == 0xd530_0000 {
            // ---- MRS
            let key = (i >> 5) & 0x7fff;
            let v = match key {
                SYS_NZCV => self.nzcv(),
                SYS_CURRENT_EL => 1 << 2, // EL1
                _ => *self.sysregs.get(&key).unwrap_or(&0),
            };
            self.wz(rd, v, true);
        } else if i & 0xfff0_0000 == 0xd510_0000 {
            // ---- MSR (register)
            let key = (i >> 5) & 0x7fff;
            let v = self.rz(rd);
            if key == SYS_NZCV {
                self.set_flags((v >> 31 & 1 == 1, v >> 30 & 1 == 1, v >> 29 & 1 == 1, v >> 28 & 1 == 1));
            } else {
                self.sysregs.insert(key, v);
            }
        } else if i & 0xffe0_001f == 0xd400_0001 {
            // ---- SVC: Linux-style host syscall, number in x8
            match host::syscall(bus, self.x[8], [self.x[0], self.x[1], self.x[2]]) {
                Host::Exit(c) => return Err(Stop::Halt(c)),
                Host::Ret(v) => self.x[0] = v,
            }
        } else if i & 0xffe0_001f == 0xd400_0002 || i & 0xffe0_001f == 0xd400_0003 {
            // ---- HVC / SMC: PSCI
            self.x[0] = match self.x[0] as u32 {
                0x8400_0008 | 0x8400_0009 => return Err(Stop::Halt(0)), // SYSTEM_OFF / SYSTEM_RESET
                0x8400_0000 => 0x0001_0000,                              // PSCI_VERSION 1.0
                _ => (-1i64) as u64,                                     // NOT_SUPPORTED
            };
        } else if i & 0xffe0_001f == 0xd440_0000 {
            // ---- HLT: stop the VM, exit code = x0
            return Err(Stop::Halt(self.x[0] as i32));
        } else if i & 0xffe0_001f == 0xd420_0000 {
            return Err(Stop::Fatal(format!("BRK #{} at pc={:#x}", (i >> 5) & 0xffff, pc)));
        } else {
            return Err(self.undef(i));
        }

        self.pc = next;
        Ok(())
    }
}

impl Default for Aarch64 {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu for Aarch64 {
    fn name(&self) -> &'static str {
        "aarch64"
    }
    fn pc(&self) -> u64 {
        self.pc
    }
    fn set_pc(&mut self, pc: u64) {
        self.pc = pc;
    }
    fn step(&mut self, bus: &mut Bus) -> Result<(), Stop> {
        self.exec(bus)
    }
    fn dump(&self) -> String {
        let mut s = format!("pc  = {:#018x}\n", self.pc);
        for r in 0..31 {
            s += &format!(" x{:<2} = {:#018x}", r, self.x[r]);
            s += if r % 2 == 1 { "\n" } else { "  " };
        }
        s += &format!("\n  sp = {:#018x}\n", self.sp);
        s += &format!(
            "nzcv = {}{}{}{}\n",
            self.n as u8, self.z as u8, self.c as u8, self.v as u8
        );
        s
    }
}
