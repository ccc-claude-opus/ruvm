//! RV64IMAC + Zicsr/Zifencei with M/S/U privilege modes, Sv39 paging and interrupts.
//!
//! Supported: RV64I, M, A (LR/SC + AMOs, single hart), C (via `rvc::expand`), Zicsr,
//! FENCE/FENCE.I/SFENCE.VMA, ECALL/EBREAK/MRET/SRET/WFI, trap delegation
//! (medeleg/mideleg), vectored/direct trap vectors, MPRV/SUM/MXR/TVM/TW/TSR,
//! Sv39 with hardware A/D update and a small TLB, CLINT timer/software and PLIC
//! external interrupts.
//! Not supported: F/D, PMP (CSRs accepted but not enforced), Sv48/57, H extension.

use super::{rvc, sext, Cpu};
use crate::bus::Bus;
use crate::host::{self, Host};
use crate::machine::Stop;

// privilege levels
const PRV_U: u64 = 0;
const PRV_S: u64 = 1;
const PRV_M: u64 = 3;

// CSR addresses
const SSTATUS: usize = 0x100;
const SIE: usize = 0x104;
const STVEC: usize = 0x105;
const SCOUNTEREN: usize = 0x106;
const SENVCFG: usize = 0x10a;
const SSCRATCH: usize = 0x140;
const SEPC: usize = 0x141;
const SCAUSE: usize = 0x142;
const STVAL: usize = 0x143;
const SIP: usize = 0x144;
const SATP: usize = 0x180;
const MSTATUS: usize = 0x300;
const MISA: usize = 0x301;
const MEDELEG: usize = 0x302;
const MIDELEG: usize = 0x303;
const MIE: usize = 0x304;
const MTVEC: usize = 0x305;
const MCOUNTEREN: usize = 0x306;
const MENVCFG: usize = 0x30a;
const MCOUNTINHIBIT: usize = 0x320;
const MSCRATCH: usize = 0x340;
const MEPC: usize = 0x341;
const MCAUSE: usize = 0x342;
const MTVAL: usize = 0x343;
const MIP: usize = 0x344;

// mstatus bits
const MS_SIE: u64 = 1 << 1;
const MS_MIE: u64 = 1 << 3;
const MS_SPIE: u64 = 1 << 5;
const MS_MPIE: u64 = 1 << 7;
const MS_SPP: u64 = 1 << 8;
const MS_MPP: u64 = 3 << 11;
const MS_MPRV: u64 = 1 << 17;
const MS_SUM: u64 = 1 << 18;
const MS_MXR: u64 = 1 << 19;
const MS_TVM: u64 = 1 << 20;
const MS_TW: u64 = 1 << 21;
const MS_TSR: u64 = 1 << 22;
const MS_UXL_SXL: u64 = 0xa << 32; // both 64-bit
const MSTATUS_W: u64 =
    MS_SIE | MS_MIE | MS_SPIE | MS_MPIE | MS_SPP | MS_MPP | MS_MPRV | MS_SUM | MS_MXR | MS_TVM | MS_TW | MS_TSR;
const SSTATUS_MASK: u64 = MS_SIE | MS_SPIE | MS_SPP | MS_SUM | MS_MXR | (3 << 32);

// interrupt bits (mip / mie)
const SSI: u64 = 1 << 1;
const MSI: u64 = 1 << 3;
const STI: u64 = 1 << 5;
const MTI: u64 = 1 << 7;
const SEI: u64 = 1 << 9;
const MEI: u64 = 1 << 11;
const S_INTS: u64 = SSI | STI | SEI;
const ALL_INTS: u64 = S_INTS | MSI | MTI | MEI;
/// Priority order for simultaneously pending interrupts.
const INT_ORDER: [u64; 6] = [11, 3, 7, 9, 1, 5];

// exception causes
const INSN_ACCESS: u64 = 1;
const ILLEGAL: u64 = 2;
const BREAKPOINT: u64 = 3;
const LOAD_MISALIGNED: u64 = 4;
const LOAD_ACCESS: u64 = 5;
const STORE_MISALIGNED: u64 = 6;
const STORE_ACCESS: u64 = 7;
const ECALL_U: u64 = 8;
const INSN_PAGE: u64 = 12;
const LOAD_PAGE: u64 = 13;
const STORE_PAGE: u64 = 15;
const INTERRUPT: u64 = 1 << 63;

// page-table entry bits
const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_U: u64 = 1 << 4;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;

const TLB_SIZE: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Fetch,
    Load,
    Store,
}

impl Access {
    fn page_fault(self) -> u64 {
        match self {
            Access::Fetch => INSN_PAGE,
            Access::Load => LOAD_PAGE,
            Access::Store => STORE_PAGE,
        }
    }
    fn access_fault(self) -> u64 {
        match self {
            Access::Fetch => INSN_ACCESS,
            Access::Load => LOAD_ACCESS,
            Access::Store => STORE_ACCESS,
        }
    }
}

enum Ex {
    Trap(u64, u64), // cause, tval
    Stop(Stop),
}

#[derive(Clone, Copy, Default)]
struct TlbEntry {
    vpn: u64, // virtual page number + 1 (0 = invalid)
    ppn: u64, // physical page number of this 4 KiB page
    pte: u64, // leaf PTE flags (low 8 bits)
}

pub struct Riscv64 {
    x: [u64; 32],
    pc: u64,
    prv: u64,
    csr: Vec<u64>,
    resv: Option<u64>,
    insns: u64,
    /// Snapshot of the time base and hardware interrupt lines, taken every step.
    time: u64,
    hw_ip: u64,
    tlb: Box<[TlbEntry; TLB_SIZE]>,
}

#[inline]
fn sx32(v: u32) -> u64 {
    v as i32 as i64 as u64
}

impl Riscv64 {
    pub fn new() -> Self {
        let mut csr = vec![0u64; 4096];
        // RV64 (MXL=2), extensions A C I M S U
        csr[MISA] = (2u64 << 62) | 1 | (1 << 2) | (1 << 8) | (1 << 12) | (1 << 18) | (1 << 20);
        csr[MSTATUS] = MS_UXL_SXL;
        Riscv64 {
            x: [0; 32],
            pc: 0,
            prv: PRV_M,
            csr,
            resv: None,
            insns: 0,
            time: 0,
            hw_ip: 0,
            tlb: Box::new([TlbEntry::default(); TLB_SIZE]),
        }
    }

    /// Current privilege level (0 = U, 1 = S, 3 = M).
    pub fn privilege(&self) -> u64 {
        self.prv
    }

    #[inline]
    fn set(&mut self, rd: usize, v: u64) {
        if rd != 0 {
            self.x[rd] = v;
        }
    }

    fn mip(&self) -> u64 {
        self.csr[MIP] | self.hw_ip
    }

    // ------------------------------------------------------------------ CSRs

    /// `None` = no such CSR (illegal instruction).
    fn csr_read(&self, a: usize) -> Option<u64> {
        Some(match a {
            SSTATUS => self.csr[MSTATUS] & SSTATUS_MASK,
            SIE => self.csr[MIE] & self.csr[MIDELEG],
            SIP => self.mip() & self.csr[MIDELEG],
            MIP => self.mip(),
            STVEC | SCOUNTEREN | SENVCFG | SSCRATCH | SEPC | SCAUSE | STVAL | SATP => self.csr[a],
            MSTATUS | MISA | MEDELEG | MIDELEG | MIE | MTVEC | MCOUNTEREN | MENVCFG | MCOUNTINHIBIT
            | MSCRATCH | MEPC | MCAUSE | MTVAL => self.csr[a],
            0x3a0..=0x3ef => self.csr[a], // pmpcfg / pmpaddr: stored, not enforced
            0xb00 | 0xb02 | 0xc00 | 0xc02 => self.insns, // mcycle minstret cycle instret
            0xc01 => self.time,
            0xb03..=0xb1f | 0xc03..=0xc1f | 0x323..=0x33f => 0, // hpm counters / events
            0xf11..=0xf15 => 0, // mvendorid marchid mimpid mhartid mconfigptr
            _ => return None,
        })
    }

    fn csr_write(&mut self, a: usize, v: u64) {
        match a {
            SSTATUS => {
                let m = SSTATUS_MASK & MSTATUS_W;
                self.csr[MSTATUS] = (self.csr[MSTATUS] & !m) | (v & m);
            }
            SIE => {
                let m = self.csr[MIDELEG];
                self.csr[MIE] = (self.csr[MIE] & !m) | (v & m);
            }
            SIP => {
                let m = self.csr[MIDELEG] & SSI;
                self.csr[MIP] = (self.csr[MIP] & !m) | (v & m);
            }
            MSTATUS => {
                let mut v = v & MSTATUS_W;
                if v & MS_MPP == 2 << 11 {
                    v &= !MS_MPP; // reserved encoding -> U
                }
                self.csr[MSTATUS] = v | MS_UXL_SXL;
            }
            MIE => self.csr[MIE] = v & ALL_INTS,
            MIP => self.csr[MIP] = (self.csr[MIP] & !S_INTS) | (v & S_INTS),
            MIDELEG => self.csr[MIDELEG] = v & S_INTS,
            MEDELEG => self.csr[MEDELEG] = v & 0xb3ff, // not ecall-from-M (11) nor 10/14
            MTVEC | STVEC => self.csr[a] = v & !2,
            MEPC | SEPC => self.csr[a] = v & !1,
            SATP => {
                let mode = v >> 60;
                if mode == 0 || mode == 8 {
                    self.csr[SATP] = v; // ASID bits kept but unused (TLB is flushed instead)
                    self.flush_tlb();
                }
            }
            MCOUNTEREN | SCOUNTEREN => self.csr[a] = v & 0xffff_ffff,
            MISA | MCOUNTINHIBIT | 0xb00..=0xb1f | 0x323..=0x33f => {} // WARL, read-only zero or ignored
            _ => self.csr[a] = v,
        }
    }

    fn csr_op(&mut self, i: u32, rd: usize, rs1: usize, f3: u32) -> Result<(), Ex> {
        let a = (i >> 20) as usize;
        let illegal = Ex::Trap(ILLEGAL, i as u64);
        let src = if f3 >= 5 { rs1 as u64 } else { self.x[rs1] };
        let writes = f3 & 3 == 1 || rs1 != 0;
        if (a as u64 >> 8) & 3 > self.prv || (writes && a >> 10 == 3) {
            return Err(illegal);
        }
        if a == SATP && self.prv == PRV_S && self.csr[MSTATUS] & MS_TVM != 0 {
            return Err(illegal);
        }
        if (0xc00..=0xc1f).contains(&a) {
            let bit = 1 << (a - 0xc00);
            if (self.prv < PRV_M && self.csr[MCOUNTEREN] & bit == 0)
                || (self.prv == PRV_U && self.csr[SCOUNTEREN] & bit == 0)
            {
                return Err(illegal);
            }
        }
        let old = self.csr_read(a).ok_or(illegal)?;
        match f3 & 3 {
            1 => self.csr_write(a, src),
            2 if writes => self.csr_write(a, old | src),
            3 if writes => self.csr_write(a, old & !src),
            _ => {}
        }
        self.set(rd, old);
        Ok(())
    }

    // ------------------------------------------------------------------ traps

    /// Enter a trap. `cause` has bit 63 set for interrupts.
    fn take_trap(&mut self, cause: u64, tval: u64) -> Result<(), Stop> {
        let irq = cause & INTERRUPT != 0;
        let code = cause & !INTERRUPT;
        let deleg = if irq { self.csr[MIDELEG] } else { self.csr[MEDELEG] };
        let ms = self.csr[MSTATUS];
        let vector = |tvec: u64| {
            if irq && tvec & 1 == 1 { (tvec & !3).wrapping_add(4 * code) } else { tvec & !3 }
        };
        if self.prv <= PRV_S && (deleg >> code) & 1 == 1 {
            self.csr[SEPC] = self.pc;
            self.csr[SCAUSE] = cause;
            self.csr[STVAL] = tval;
            let spie = if ms & MS_SIE != 0 { MS_SPIE } else { 0 };
            let spp = if self.prv == PRV_S { MS_SPP } else { 0 };
            self.csr[MSTATUS] = (ms & !(MS_SIE | MS_SPIE | MS_SPP)) | spie | spp;
            self.prv = PRV_S;
            self.pc = vector(self.csr[STVEC]);
        } else {
            let tvec = self.csr[MTVEC];
            if tvec == 0 {
                return Err(Stop::Fatal(format!(
                    "unhandled trap: cause={}{} tval={:#x} at pc={:#x} (mtvec not set)",
                    if irq { "irq " } else { "" },
                    code,
                    tval,
                    self.pc
                )));
            }
            self.csr[MEPC] = self.pc;
            self.csr[MCAUSE] = cause;
            self.csr[MTVAL] = tval;
            let mpie = if ms & MS_MIE != 0 { MS_MPIE } else { 0 };
            self.csr[MSTATUS] = (ms & !(MS_MIE | MS_MPIE | MS_MPP)) | mpie | (self.prv << 11);
            self.prv = PRV_M;
            self.pc = vector(tvec);
        }
        Ok(())
    }

    /// Highest-priority interrupt that is pending, enabled and not masked.
    fn pending_interrupt(&self) -> Option<u64> {
        let pend = self.mip() & self.csr[MIE];
        if pend == 0 {
            return None;
        }
        let ms = self.csr[MSTATUS];
        let deleg = self.csr[MIDELEG];
        let m_on = self.prv < PRV_M || ms & MS_MIE != 0;
        let s_on = self.prv < PRV_S || (self.prv == PRV_S && ms & MS_SIE != 0);
        let m = if m_on { pend & !deleg } else { 0 };
        let s = if s_on { pend & deleg } else { 0 };
        let pick = |set: u64| INT_ORDER.iter().copied().find(|&c| set >> c & 1 == 1);
        pick(m).or_else(|| pick(s))
    }

    // ------------------------------------------------------------------ MMU

    fn flush_tlb(&mut self) {
        self.tlb.iter_mut().for_each(|e| e.vpn = 0);
    }

    /// Check a leaf PTE's permissions for `acc` at privilege `prv`.
    fn pte_allows(&self, pte: u64, prv: u64, acc: Access) -> bool {
        let ms = self.csr[MSTATUS];
        if pte & PTE_U != 0 {
            if prv == PRV_S && (acc == Access::Fetch || ms & MS_SUM == 0) {
                return false;
            }
        } else if prv == PRV_U {
            return false;
        }
        match acc {
            Access::Fetch => pte & PTE_X != 0,
            Access::Load => pte & PTE_R != 0 || (ms & MS_MXR != 0 && pte & PTE_X != 0),
            Access::Store => pte & PTE_W != 0,
        }
    }

    /// Virtual -> physical translation (Sv39).
    fn translate(&mut self, bus: &mut Bus, va: u64, acc: Access) -> Result<u64, Ex> {
        let ms = self.csr[MSTATUS];
        let prv = if acc != Access::Fetch && ms & MS_MPRV != 0 { (ms >> 11) & 3 } else { self.prv };
        let satp = self.csr[SATP];
        if prv == PRV_M || satp >> 60 != 8 {
            return Ok(va);
        }
        let pf = Ex::Trap(acc.page_fault(), va);
        if sext(va, 39) != va {
            return Err(pf);
        }
        let vpn = va >> 12;
        let slot = (vpn as usize) % TLB_SIZE;
        let e = self.tlb[slot];
        if e.vpn == vpn + 1 && (acc != Access::Store || e.pte & PTE_D != 0) {
            if !self.pte_allows(e.pte, prv, acc) {
                return Err(pf);
            }
            return Ok(e.ppn << 12 | (va & 0xfff));
        }

        // page-table walk
        let mut table = (satp & ((1 << 44) - 1)) << 12;
        let mut level = 2;
        let (pte, pte_addr) = loop {
            let addr = table + ((vpn >> (9 * level)) & 0x1ff) * 8;
            let pte = bus.read(addr, 8).map_err(|_| Ex::Trap(acc.access_fault(), va))?;
            if pte & PTE_V == 0 || (pte & PTE_R == 0 && pte & PTE_W != 0) || pte >> 54 != 0 {
                return Err(pf);
            }
            if pte & (PTE_R | PTE_X) != 0 {
                break (pte, addr);
            }
            if level == 0 {
                return Err(pf);
            }
            level -= 1;
            table = ((pte >> 10) & ((1 << 44) - 1)) << 12;
        };
        if !self.pte_allows(pte, prv, acc) {
            return Err(pf);
        }
        let mut ppn = (pte >> 10) & ((1 << 44) - 1);
        let low = (1u64 << (9 * level)) - 1;
        if ppn & low != 0 {
            return Err(pf); // misaligned superpage
        }
        ppn |= vpn & low;
        // hardware A/D update
        let want = PTE_A | if acc == Access::Store { PTE_D } else { 0 };
        let mut pte = pte;
        if pte & want != want {
            pte |= want;
            bus.write(pte_addr, 8, pte).map_err(|_| Ex::Trap(acc.access_fault(), va))?;
        }
        self.tlb[slot] = TlbEntry { vpn: vpn + 1, ppn, pte: pte & 0xff };
        Ok(ppn << 12 | (va & 0xfff))
    }

    fn load(&mut self, bus: &mut Bus, va: u64, size: usize) -> Result<u64, Ex> {
        if (va & 0xfff) as usize + size > 0x1000 {
            // crosses a page: do it byte by byte
            let mut v = 0;
            for k in 0..size {
                v |= self.load(bus, va.wrapping_add(k as u64), 1)? << (8 * k);
            }
            return Ok(v);
        }
        let pa = self.translate(bus, va, Access::Load)?;
        bus.read(pa, size).map_err(|_| Ex::Trap(LOAD_ACCESS, va))
    }

    fn store(&mut self, bus: &mut Bus, va: u64, size: usize, v: u64) -> Result<(), Ex> {
        if (va & 0xfff) as usize + size > 0x1000 {
            // translate every byte first so a fault leaves memory untouched
            let mut pas = [0u64; 8];
            for k in 0..size {
                pas[k] = self.translate(bus, va.wrapping_add(k as u64), Access::Store)?;
            }
            for k in 0..size {
                bus.write(pas[k], 1, v >> (8 * k)).map_err(|_| Ex::Trap(STORE_ACCESS, va))?;
            }
            return Ok(());
        }
        let pa = self.translate(bus, va, Access::Store)?;
        bus.write(pa, size, v).map_err(|_| Ex::Trap(STORE_ACCESS, va))
    }

    /// Fetch the instruction at pc; returns (32-bit instruction, length in bytes).
    fn fetch(&mut self, bus: &mut Bus) -> Result<(u32, u64), Ex> {
        let pc = self.pc;
        let pa = self.translate(bus, pc, Access::Fetch)?;
        let lo = bus.read(pa, 2).map_err(|_| Ex::Trap(INSN_ACCESS, pc))? as u32;
        if lo & 3 != 3 {
            let i = rvc::expand(lo as u16).ok_or(Ex::Trap(ILLEGAL, lo as u64))?;
            return Ok((i, 2));
        }
        let hi_va = pc.wrapping_add(2);
        let hi_pa = if pc & 0xfff == 0xffe { self.translate(bus, hi_va, Access::Fetch)? } else { pa + 2 };
        let hi = bus.read(hi_pa, 2).map_err(|_| Ex::Trap(INSN_ACCESS, hi_va))? as u32;
        Ok((lo | hi << 16, 4))
    }

    // ------------------------------------------------------------------ execute

    fn exec(&mut self, bus: &mut Bus) -> Result<(), Ex> {
        let pc = self.pc;
        let (i, len) = self.fetch(bus)?;
        let illegal = Ex::Trap(ILLEGAL, i as u64);
        let mut next = pc.wrapping_add(len);

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
                let v = self.load(bus, addr, sz)?;
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
                self.store(bus, addr, sz, b)?;
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
            0x0f => {} // FENCE / FENCE.I: nothing to do on a single hart without caches
            0x2f => self.amo(bus, i, rd, f3, a, b)?,
            0x73 if f3 == 0 => {
                match (f7, rs2, rd) {
                    (0x00, 0, 0) if rs1 == 0 => {
                        // ECALL
                        if self.prv == PRV_M && self.csr[MTVEC] == 0 {
                            let args = [self.x[10], self.x[11], self.x[12]];
                            match host::syscall(bus, self.x[17], args) {
                                Host::Exit(c) => return Err(Ex::Stop(Stop::Halt(c))),
                                Host::Ret(v) => self.x[10] = v,
                            }
                        } else {
                            return Err(Ex::Trap(ECALL_U + self.prv, 0));
                        }
                    }
                    (0x00, 1, 0) if rs1 == 0 => return Err(Ex::Trap(BREAKPOINT, pc)),
                    (0x18, 2, 0) if rs1 == 0 => {
                        // MRET
                        if self.prv < PRV_M {
                            return Err(illegal);
                        }
                        let ms = self.csr[MSTATUS];
                        let mpp = (ms >> 11) & 3;
                        let mie = if ms & MS_MPIE != 0 { MS_MIE } else { 0 };
                        let mut ns = (ms & !(MS_MIE | MS_MPP)) | mie | MS_MPIE;
                        if mpp != PRV_M {
                            ns &= !MS_MPRV;
                        }
                        self.csr[MSTATUS] = ns;
                        self.prv = mpp;
                        next = self.csr[MEPC];
                    }
                    (0x08, 2, 0) if rs1 == 0 => {
                        // SRET
                        let ms = self.csr[MSTATUS];
                        if self.prv < PRV_S || (self.prv == PRV_S && ms & MS_TSR != 0) {
                            return Err(illegal);
                        }
                        let spp = (ms >> 8) & 1;
                        let sie = if ms & MS_SPIE != 0 { MS_SIE } else { 0 };
                        self.csr[MSTATUS] = (ms & !(MS_SIE | MS_SPP | MS_MPRV)) | sie | MS_SPIE;
                        self.prv = spp;
                        next = self.csr[SEPC];
                    }
                    (0x08, 5, 0) if rs1 == 0 => {
                        // WFI
                        if self.prv == PRV_U || (self.prv == PRV_S && self.csr[MSTATUS] & MS_TW != 0) {
                            return Err(illegal);
                        }
                        if self.mip() & self.csr[MIE] == 0 {
                            bus.idle();
                        }
                    }
                    (0x09, _, 0) => {
                        // SFENCE.VMA
                        if self.prv == PRV_U || (self.prv == PRV_S && self.csr[MSTATUS] & MS_TVM != 0) {
                            return Err(illegal);
                        }
                        self.flush_tlb();
                    }
                    _ => return Err(illegal),
                }
            }
            0x73 if f3 != 4 => self.csr_op(i, rd, rs1, f3)?,
            _ => return Err(illegal),
        }

        self.pc = next;
        Ok(())
    }

    /// LR / SC / AMO*.{W,D}
    fn amo(&mut self, bus: &mut Bus, i: u32, rd: usize, f3: u32, addr: u64, b: u64) -> Result<(), Ex> {
        let w = match f3 {
            2 => true,
            3 => false,
            _ => return Err(Ex::Trap(ILLEGAL, i as u64)),
        };
        let sz = if w { 4 } else { 8 };
        let ext = |v: u64| if w { sext(v, 32) } else { v };
        let f5 = i >> 27;
        if addr & (sz as u64 - 1) != 0 {
            return Err(Ex::Trap(if f5 == 0x02 { LOAD_MISALIGNED } else { STORE_MISALIGNED }, addr));
        }
        match f5 {
            0x02 => {
                let pa = self.translate(bus, addr, Access::Load)?;
                let v = bus.read(pa, sz).map_err(|_| Ex::Trap(LOAD_ACCESS, addr))?;
                self.resv = Some(pa);
                self.set(rd, ext(v));
            }
            0x03 => {
                let pa = self.translate(bus, addr, Access::Store)?;
                if self.resv == Some(pa) {
                    bus.write(pa, sz, b).map_err(|_| Ex::Trap(STORE_ACCESS, addr))?;
                    self.set(rd, 0);
                } else {
                    self.set(rd, 1);
                }
                self.resv = None;
            }
            0x00 | 0x01 | 0x04 | 0x08 | 0x0c | 0x10 | 0x14 | 0x18 | 0x1c => {
                let pa = self.translate(bus, addr, Access::Store)?;
                let fault = |_| Ex::Trap(STORE_ACCESS, addr);
                let old = ext(bus.read(pa, sz).map_err(fault)?);
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
                bus.write(pa, sz, new).map_err(fault)?;
                self.set(rd, old);
            }
            _ => return Err(Ex::Trap(ILLEGAL, i as u64)),
        }
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
        self.time = bus.clint.mtime;
        self.hw_ip = (bus.clint.mtip() as u64) << 7
            | (bus.clint.msip as u64) << 3
            | (bus.plic.out[0] as u64) << 11
            | (bus.plic.out[1] as u64) << 9;
        if let Some(code) = self.pending_interrupt() {
            self.take_trap(INTERRUPT | code, 0)?;
        }
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
        let mut s = format!("pc  = {:#018x}   priv = {}\n", self.pc, ["U", "S", "?", "M"][self.prv as usize]);
        for r in 0..32 {
            s += &format!("{:>4} = {:#018x}", ABI[r], self.x[r]);
            s += if r % 2 == 1 { "\n" } else { "   " };
        }
        s += &format!(
            "mstatus={:#x} mepc={:#x} mcause={:#x} mtval={:#x} mtvec={:#x}\n",
            self.csr[MSTATUS], self.csr[MEPC], self.csr[MCAUSE], self.csr[MTVAL], self.csr[MTVEC]
        );
        s += &format!(
            "sepc={:#x} scause={:#x} stval={:#x} stvec={:#x} satp={:#x}\n",
            self.csr[SEPC], self.csr[SCAUSE], self.csr[STVAL], self.csr[STVEC], self.csr[SATP]
        );
        s
    }
}
