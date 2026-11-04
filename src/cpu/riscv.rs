//! RV64IMA + Zicsr, machine mode only.
//!
//! Supported: RV64I, M (mul/div), A (LR/SC + AMOs, single hart), Zicsr, FENCE,
//! ECALL/EBREAK/MRET/WFI, machine-mode traps through mtvec.
//! Not supported: C, F/D, S/U modes, MMU, interrupts/timers.

use super::{sext, Cpu};
use crate::bus::Bus;
use crate::host::{self, Host};
use crate::machine::Stop;

const MSTATUS: usize = 0x300;
const MISA: usize = 0x301;
const MTVEC: usize = 0x305;
const MEPC: usize = 0x341;
const MCAUSE: usize = 0x342;
const MTVAL: usize = 0x343;

// exception causes
const INSN_MISALIGNED: u64 = 0;
const INSN_ACCESS: u64 = 1;
const ILLEGAL: u64 = 2;
const BREAKPOINT: u64 = 3;
const LOAD_ACCESS: u64 = 5;
const STORE_ACCESS: u64 = 7;
const ECALL_M: u64 = 11;

enum Ex {
    Trap(u64, u64), // cause, tval
    Stop(Stop),
}

pub struct Riscv64 {
    x: [u64; 32],
    pc: u64,
    csr: Vec<u64>,
    resv: Option<u64>,
    insns: u64,
}

#[inline]
fn sx32(v: u32) -> u64 {
    v as i32 as i64 as u64
}

impl Riscv64 {
    pub fn new() -> Self {
        let mut csr = vec![0u64; 4096];
        // RV64 (MXL=2), extensions A, I, M
        csr[MISA] = (2u64 << 62) | 1 | (1 << 8) | (1 << 12);
        Riscv64 { x: [0; 32], pc: 0, csr, resv: None, insns: 0 }
    }

    #[inline]
    fn set(&mut self, rd: usize, v: u64) {
        if rd != 0 {
            self.x[rd] = v;
        }
    }

    fn csr_read(&self, a: usize) -> u64 {
        match a {
            0xb00 | 0xb02 | 0xc00 | 0xc01 | 0xc02 => self.insns,
            0xf14 => 0, // mhartid
            _ => self.csr[a],
        }
    }

    fn csr_write(&mut self, a: usize, v: u64) {
        match a {
            0xb00 | 0xb02 | 0xc00 | 0xc01 | 0xc02 | 0xf14 | MISA => {}
            _ => self.csr[a] = v,
        }
    }

    fn take_trap(&mut self, cause: u64, tval: u64) -> Result<(), Stop> {
        let tvec = self.csr[MTVEC];
        if tvec == 0 {
            return Err(Stop::Fatal(format!(
                "unhandled trap: cause={} tval={:#x} at pc={:#x} (mtvec not set)",
                cause, tval, self.pc
            )));
        }
        self.csr[MEPC] = self.pc;
        self.csr[MCAUSE] = cause;
        self.csr[MTVAL] = tval;
        let ms = self.csr[MSTATUS];
        let mie = (ms >> 3) & 1;
        self.csr[MSTATUS] = (ms & !((1 << 3) | (1 << 7) | (3 << 11))) | (mie << 7) | (3 << 11);
        self.pc = tvec & !3;
        Ok(())
    }

    fn exec(&mut self, bus: &mut Bus) -> Result<(), Ex> {
        let pc = self.pc;
        let i = bus.read(pc, 4).map_err(|_| Ex::Trap(INSN_ACCESS, pc))? as u32;
        let illegal = Ex::Trap(ILLEGAL, i as u64);
        let mut next = pc.wrapping_add(4);

        let op = i & 0x7f;
        let rd = ((i >> 7) & 31) as usize;
        let f3 = (i >> 12) & 7;
        let rs1 = ((i >> 15) & 31) as usize;
        let rs2 = ((i >> 20) & 31) as usize;
        let f7 = i >> 25;
        let a = self.x[rs1];
        let b = self.x[rs2];

        let s = i as i32 as i64;
        let imm_i = (s >> 20) as u64;
        let imm_s = (((s >> 25) << 5) | ((i >> 7) & 0x1f) as i64) as u64;
        let imm_b = (((s >> 31) << 12)
            | (((i >> 7) & 1) as i64) << 11
            | (((i >> 25) & 0x3f) as i64) << 5
            | (((i >> 8) & 0xf) as i64) << 1) as u64;
        let imm_u = (i & 0xffff_f000) as i32 as i64 as u64;
        let imm_j = (((s >> 31) << 20)
            | (((i >> 12) & 0xff) as i64) << 12
            | (((i >> 20) & 1) as i64) << 11
            | (((i >> 21) & 0x3ff) as i64) << 1) as u64;

        match op {
            0x37 => self.set(rd, imm_u),                       // LUI
            0x17 => self.set(rd, pc.wrapping_add(imm_u)),      // AUIPC
            0x6f => {
                self.set(rd, next);                            // JAL
                next = pc.wrapping_add(imm_j);
            }
            0x67 => {
                if f3 != 0 {
                    return Err(illegal);
                }
                let t = a.wrapping_add(imm_i) & !1;            // JALR
                self.set(rd, next);
                next = t;
            }
            0x63 => {
                let taken = match f3 {
                    0 => a == b,
                    1 => a != b,
                    4 => (a as i64) < (b as i64),
                    5 => (a as i64) >= (b as i64),
                    6 => a < b,
                    7 => a >= b,
                    _ => return Err(illegal),
                };
                if taken {
                    next = pc.wrapping_add(imm_b);
                }
            }
            0x03 => {
                let addr = a.wrapping_add(imm_i);
                let (sz, signed) = match f3 {
                    0 => (1, true),
                    1 => (2, true),
                    2 => (4, true),
                    3 => (8, false),
                    4 => (1, false),
                    5 => (2, false),
                    6 => (4, false),
                    _ => return Err(illegal),
                };
                let v = bus.read(addr, sz).map_err(|_| Ex::Trap(LOAD_ACCESS, addr))?;
                self.set(rd, if signed { sext(v, sz as u32 * 8) } else { v });
            }
            0x23 => {
                let addr = a.wrapping_add(imm_s);
                let sz = match f3 {
                    0 => 1,
                    1 => 2,
                    2 => 4,
                    3 => 8,
                    _ => return Err(illegal),
                };
                bus.write(addr, sz, b).map_err(|_| Ex::Trap(STORE_ACCESS, addr))?;
            }
            0x13 => {
                let sh = (i >> 20) & 0x3f;
                let v = match f3 {
                    0 => a.wrapping_add(imm_i),
                    1 if (i >> 26) == 0 => a << sh,
                    2 => ((a as i64) < (imm_i as i64)) as u64,
                    3 => (a < imm_i) as u64,
                    4 => a ^ imm_i,
                    5 if (i >> 26) == 0 => a >> sh,
                    5 if (i >> 26) == 0x10 => ((a as i64) >> sh) as u64,
                    6 => a | imm_i,
                    7 => a & imm_i,
                    _ => return Err(illegal),
                };
                self.set(rd, v);
            }
            0x1b => {
                let sh = (i >> 20) & 0x1f;
                let v = match (f3, f7) {
                    (0, _) => sx32((a as u32).wrapping_add(imm_i as u32)),
                    (1, 0) => sx32((a as u32) << sh),
                    (5, 0) => sx32((a as u32) >> sh),
                    (5, 0x20) => ((a as i32) >> sh) as i64 as u64,
                    _ => return Err(illegal),
                };
                self.set(rd, v);
            }
            0x33 => {
                let v = match (f7, f3) {
                    (1, 0) => a.wrapping_mul(b),
                    (1, 1) => (((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64,
                    (1, 2) => (((a as i64 as i128) * (b as u128 as i128)) >> 64) as u64,
                    (1, 3) => (((a as u128) * (b as u128)) >> 64) as u64,
                    (1, 4) => if b == 0 { u64::MAX } else { (a as i64).wrapping_div(b as i64) as u64 },
                    (1, 5) => if b == 0 { u64::MAX } else { a / b },
                    (1, 6) => if b == 0 { a } else { (a as i64).wrapping_rem(b as i64) as u64 },
                    (1, 7) => if b == 0 { a } else { a % b },
                    (0, 0) => a.wrapping_add(b),
                    (0x20, 0) => a.wrapping_sub(b),
                    (0, 1) => a << (b & 63),
                    (0, 2) => ((a as i64) < (b as i64)) as u64,
                    (0, 3) => (a < b) as u64,
                    (0, 4) => a ^ b,
                    (0, 5) => a >> (b & 63),
                    (0x20, 5) => ((a as i64) >> (b & 63)) as u64,
                    (0, 6) => a | b,
                    (0, 7) => a & b,
                    _ => return Err(illegal),
                };
                self.set(rd, v);
            }
            0x3b => {
                let (x, y) = (a as u32, b as u32);
                let v = match (f7, f3) {
                    (1, 0) => sx32(x.wrapping_mul(y)),
                    (1, 4) => if y == 0 { u64::MAX } else { sx32((x as i32).wrapping_div(y as i32) as u32) },
                    (1, 5) => if y == 0 { u64::MAX } else { sx32(x / y) },
                    (1, 6) => if y == 0 { sx32(x) } else { sx32((x as i32).wrapping_rem(y as i32) as u32) },
                    (1, 7) => if y == 0 { sx32(x) } else { sx32(x % y) },
                    (0, 0) => sx32(x.wrapping_add(y)),
                    (0x20, 0) => sx32(x.wrapping_sub(y)),
                    (0, 1) => sx32(x << (y & 31)),
                    (0, 5) => sx32(x >> (y & 31)),
                    (0x20, 5) => ((x as i32) >> (y & 31)) as i64 as u64,
                    _ => return Err(illegal),
                };
                self.set(rd, v);
            }
            0x0f => {} // FENCE / FENCE.I: nothing to do on a single hart
            0x2f => {
                let w = match f3 {
                    2 => true,
                    3 => false,
                    _ => return Err(illegal),
                };
                let sz = if w { 4 } else { 8 };
                let addr = a;
                let ext = |v: u64| if w { sext(v, 32) } else { v };
                let f5 = i >> 27;
                let fault = |_| Ex::Trap(STORE_ACCESS, addr);
                match f5 {
                    0x02 => {
                        let v = bus.read(addr, sz).map_err(|_| Ex::Trap(LOAD_ACCESS, addr))?;
                        self.resv = Some(addr);
                        self.set(rd, ext(v));
                    }
                    0x03 => {
                        if self.resv == Some(addr) {
                            bus.write(addr, sz, b).map_err(fault)?;
                            self.set(rd, 0);
                        } else {
                            self.set(rd, 1);
                        }
                        self.resv = None;
                    }
                    0x00 | 0x01 | 0x04 | 0x08 | 0x0c | 0x10 | 0x14 | 0x18 | 0x1c => {
                        let old = ext(bus.read(addr, sz).map_err(fault)?);
                        let bv = ext(b);
                        let new = match f5 {
                            0x00 => old.wrapping_add(bv),
                            0x01 => bv,
                            0x04 => old ^ bv,
                            0x08 => old | bv,
                            0x0c => old & bv,
                            0x10 => (old as i64).min(bv as i64) as u64,
                            0x14 => (old as i64).max(bv as i64) as u64,
                            0x18 => old.min(bv),
                            _ => old.max(bv),
                        };
                        bus.write(addr, sz, new).map_err(fault)?;
                        self.set(rd, old);
                    }
                    _ => return Err(illegal),
                }
            }
            0x73 => {
                let csr = (i >> 20) as usize;
                if f3 == 0 {
                    match (i >> 20, rs1, rd) {
                        (0x000, 0, 0) => {
                            // ECALL
                            if self.csr[MTVEC] == 0 {
                                let args = [self.x[10], self.x[11], self.x[12]];
                                match host::syscall(bus, self.x[17], args) {
                                    Host::Exit(c) => return Err(Ex::Stop(Stop::Halt(c))),
                                    Host::Ret(v) => self.x[10] = v,
                                }
                            } else {
                                return Err(Ex::Trap(ECALL_M, 0));
                            }
                        }
                        (0x001, 0, 0) => return Err(Ex::Trap(BREAKPOINT, pc)),
                        (0x302, 0, 0) => {
                            // MRET
                            let ms = self.csr[MSTATUS];
                            let mpie = (ms >> 7) & 1;
                            self.csr[MSTATUS] = (ms & !((1 << 3) | (1 << 7))) | (mpie << 3) | (1 << 7);
                            next = self.csr[MEPC];
                        }
                        (0x105, 0, 0) => {} // WFI: no interrupts modelled, treat as NOP
                        _ => return Err(illegal),
                    }
                } else {
                    let src = if f3 >= 5 { rs1 as u64 } else { a };
                    let old = self.csr_read(csr);
                    match f3 & 3 {
                        1 => self.csr_write(csr, src),
                        2 if rs1 != 0 => self.csr_write(csr, old | src),
                        3 if rs1 != 0 => self.csr_write(csr, old & !src),
                        2 | 3 => {}
                        _ => return Err(illegal),
                    }
                    self.set(rd, old);
                }
            }
            _ => return Err(illegal),
        }

        if next & 3 != 0 {
            return Err(Ex::Trap(INSN_MISALIGNED, next));
        }
        self.pc = next;
        Ok(())
    }
}

impl Default for Riscv64 {
    fn default() -> Self {
        Self::new()
    }
}

impl Cpu for Riscv64 {
    fn name(&self) -> &'static str {
        "riscv64"
    }
    fn pc(&self) -> u64 {
        self.pc
    }
    fn set_pc(&mut self, pc: u64) {
        self.pc = pc;
    }

    fn step(&mut self, bus: &mut Bus) -> Result<(), Stop> {
        self.insns += 1;
        match self.exec(bus) {
            Ok(()) => Ok(()),
            Err(Ex::Stop(s)) => Err(s),
            Err(Ex::Trap(cause, tval)) => self.take_trap(cause, tval),
        }
    }

    fn dump(&self) -> String {
        const ABI: [&str; 32] = [
            "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
            "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
            "t5", "t6",
        ];
        let mut s = format!("pc  = {:#018x}\n", self.pc);
        for r in 0..32 {
            s += &format!("{:>4} = {:#018x}", ABI[r], self.x[r]);
            s += if r % 2 == 1 { "\n" } else { "   " };
        }
        s += &format!("mstatus={:#x} mepc={:#x} mcause={:#x} mtvec={:#x}\n",
            self.csr[MSTATUS], self.csr[MEPC], self.csr[MCAUSE], self.csr[MTVEC]);
        s
    }
}
