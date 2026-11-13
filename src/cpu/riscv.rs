//! RV64IMAFDC + Zicsr/Zifencei with M/S/U privilege modes, Sv39 paging and interrupts.
//!
//! Supported: RV64I, M, A (LR/SC + AMOs, single hart), F/D (bit-exact, see
//! `softfloat`), C (via `rvc::expand`), Zicsr,
//! FENCE/FENCE.I/SFENCE.VMA, ECALL/EBREAK/MRET/SRET/WFI, trap delegation
//! (medeleg/mideleg), vectored/direct trap vectors, MPRV/SUM/MXR/TVM/TW/TSR,
//! Sv39 with hardware A/D update and a small TLB, CLINT timer/software and PLIC
//! external interrupts, Sstc (`stimecmp`).
//! Not supported: PMP (CSRs accepted but not enforced), Sv48/57, H extension.

use super::softfloat::{self as sf, Fmt, F32, F64};
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
const STIMECMP: usize = 0x14d;
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
const MS_FS: u64 = 3 << 13;
const MS_SD: u64 = 1 << 63;
const MS_MPRV: u64 = 1 << 17;
const MS_SUM: u64 = 1 << 18;
const MS_MXR: u64 = 1 << 19;
const MS_TVM: u64 = 1 << 20;
const MS_TW: u64 = 1 << 21;
const MS_TSR: u64 = 1 << 22;
const MS_UXL_SXL: u64 = 0xa << 32; // both 64-bit
const MSTATUS_W: u64 = MS_SIE
    | MS_MIE
    | MS_SPIE
    | MS_MPIE
    | MS_SPP
    | MS_MPP
    | MS_FS
    | MS_MPRV
    | MS_SUM
    | MS_MXR
    | MS_TVM
    | MS_TW
    | MS_TSR;
const SSTATUS_MASK: u64 = MS_SIE | MS_SPIE | MS_SPP | MS_FS | MS_SUM | MS_MXR | (3 << 32) | MS_SD;

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
    /// FP registers; single-precision values are NaN-boxed (upper 32 bits all ones).
    f: [u64; 32],
    fflags: u8,
    frm: u8,
    pc: u64,
    prv: u64,
    csr: Vec<u64>,
    resv: Option<u64>,
    insns: u64,
    /// Snapshot of the time base and hardware interrupt lines, taken every step.
    time: u64,
    hw_ip: u64,
    tlb: Box<[TlbEntry; TLB_SIZE]>,
    /// Fetch fast path: virtual page number + 1 of the current code page and the
    /// RAM offset of its first byte (0 = invalid). Dropped on any change of
    /// privilege, translation or mstatus.
    fetch_vpn: u64,
    fetch_off: usize,
    /// Built-in SBI firmware: `ecall` from S-mode is served by the emulator
    /// (set by `boot_supervisor`; with `-bios <file>` the real firmware does it).
    sbi: bool,
    /// Hart id (mhartid) and the number of harts on the board.
    id: usize,
    /// All 65536 compressed encodings expanded to 32-bit instructions (0 = illegal).
    rvc_table: Box<[u32]>,
    /// Interrupt-relevant state changed: end the current batch so `pre` runs again.
    recheck: bool,
    /// PMP: active regions in priority order, whether S/U accesses are unrestricted, and
    /// whether any locked region constrains M-mode.
    pmp: Vec<PmpEntry>,
    pmp_all: bool,
    pmp_locked_any: bool,
}

#[derive(Clone, Copy)]
struct PmpEntry {
    lo: u64,
    hi: u64,
    /// bit 0 = R, bit 1 = W, bit 2 = X
    perm: u8,
    locked: bool,
}

/// Carry-less multiply: (low 64 bits, high 64 bits) of the 128-bit product.
fn clmul128(a: u64, b: u64) -> (u64, u64) {
    let (mut lo, mut hi) = (0u64, 0u64);
    for k in 0..64 {
        if b >> k & 1 == 1 {
            lo ^= a << k;
            if k > 0 {
                hi ^= a >> (64 - k);
            }
        }
    }
    (lo, hi)
}

#[inline]
fn sx32(v: u32) -> u64 {
    v as i32 as i64 as u64
}

impl Riscv64 {
    /// Sample time and interrupt lines and take a pending interrupt, between instructions.
    #[inline(always)]
    fn sample_lines(&mut self, bus: &mut Bus) {
        let id = self.id;
        self.time = bus.clint.mtime;
        if std::mem::take(&mut bus.ctl[id].ssip) {
            self.csr[MIP] |= SSI; // SBI IPI from another hart
        }
        self.hw_ip = (bus.clint.mtip(id) as u64) << 7
            | (bus.clint.msip[id] as u64) << 3
            | (bus.plic.out[2 * id] as u64) << 11
            | (bus.plic.out[2 * id + 1] as u64) << 9;
        if self.csr[MENVCFG] >> 63 == 1 && self.time >= self.csr[STIMECMP] {
            self.hw_ip |= STI; // Sstc
        }
    }

    /// Requests from other harts: TLB flush, start (SBI HSM).
    fn service_ctl(&mut self, bus: &mut Bus) {
        let id = self.id;
        if std::mem::take(&mut bus.ctl[id].flush_tlb) {
            self.flush_tlb();
        }
        if let Some((pc, arg)) = bus.ctl[id].start.take() {
            // a (re)started hart begins in S-mode with a clean state
            *self = Riscv64::new(id);
            self.boot_supervisor(pc, arg);
            bus.ctl[id].parked = false;
            bus.ctl[id].waiting = false;
        }
    }

    #[inline(always)]
    fn pre(&mut self, bus: &mut Bus) -> Result<(), Stop> {
        self.sample_lines(bus);
        if let Some(code) = self.pending_interrupt() {
            self.take_trap(INTERRUPT | code, 0)?;
        }
        Ok(())
    }

    /// An instruction raised a trap (or stopped the machine).
    #[cold]
    #[inline(never)]
    fn handle_ex(&mut self, bus: &mut Bus, e: Ex) -> Result<(), Stop> {
        match e {
            Ex::Stop(s) => Err(s),
            Ex::Trap(cause, _) if self.sbi && cause == ECALL_U + PRV_S => {
                self.sbi_call(bus)?;
                self.pc += 4;
                Ok(())
            }
            Ex::Trap(cause, tval) => self.take_trap(cause, tval),
        }
    }

    pub fn new(id: usize) -> Self {
        let mut csr = vec![0u64; 4096];
        // RV64 (MXL=2), extensions A C D F I M S U
        csr[MISA] = (2u64 << 62) | 1 | (1 << 1) | (1 << 2) | (1 << 3) | (1 << 5) | (1 << 8) | (1 << 12) | (1 << 18) | (1 << 20);
        csr[MSTATUS] = MS_UXL_SXL;
        csr[STIMECMP] = u64::MAX;
        Riscv64 {
            x: [0; 32],
            f: [0; 32],
            fflags: 0,
            frm: 0,
            pc: 0,
            prv: PRV_M,
            csr,
            resv: None,
            insns: 0,
            time: 0,
            hw_ip: 0,
            tlb: Box::new([TlbEntry::default(); TLB_SIZE]),
            fetch_vpn: 0,
            fetch_off: 0,
            sbi: false,
            id,
            recheck: false,
            pmp: Vec::new(),
            pmp_all: false,
            pmp_locked_any: false,
            rvc_table: (0..=u16::MAX).map(|c| rvc::expand(c).unwrap_or(0)).collect(),
        }
    }

    /// Current privilege level (0 = U, 1 = S, 3 = M).
    pub fn privilege(&self) -> u64 {
        self.prv
    }

    pub fn set_reg(&mut self, r: usize, v: u64) {
        self.set(r, v);
    }

    /// Start like a kernel entered from SBI firmware: S-mode at `entry` with
    /// a0 = hart id, a1 = device-tree address; the emulator itself answers SBI calls.
    pub fn boot_supervisor(&mut self, entry: u64, dtb: u64) {
        self.sbi = true;
        self.prv = PRV_S;
        self.pc = entry;
        self.x[10] = self.id as u64;
        self.x[11] = dtb;
        // what OpenSBI would leave behind: delegate everything S-mode can handle
        self.csr[MEDELEG] = 0xb1ff | 1 << 12 | 1 << 13 | 1 << 15;
        self.csr[MIDELEG] = S_INTS;
        self.csr[MCOUNTEREN] = 0xffff_ffff;
        self.csr[MENVCFG] = 1 << 63 | 1 << 61 | 3 << 4 | 1 << 6 | 1 << 7; // STCE, ADUE, CBIE/CBCFE/CBZE
        // like OpenSBI: let S/U-mode reach all of memory
        self.csr[0x3b0] = (1 << 54) - 1;
        self.csr[0x3a0] = 0x1f; // NAPOT, RWX
        self.pmp_rebuild();
    }

    /// SBI v2.0 subset: BASE, TIME, IPI, RFENCE, HSM, SRST, DBCN, PMU and the legacy calls.
    fn sbi_call(&mut self, bus: &mut Bus) -> Result<(), Stop> {
        const NOT_SUPPORTED: u64 = -2i64 as u64;
        const INVALID_PARAM: u64 = -3i64 as u64;
        const ALREADY_AVAILABLE: u64 = -6i64 as u64;
        const EXTS: [u64; 8] =
            [0x10, 0x5449_4d45, 0x73_5049, 0x5246_4e43, 0x48_534d, 0x5352_5354, 0x4442_434e, 0x50_4d55];
        let (eid, fid) = (self.x[17], self.x[16]);
        let a = [self.x[10], self.x[11], self.x[12]];
        // console output, with "\n" -> "\r\n" like OpenSBI
        let put = |bytes: &[u8]| {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            for &b in bytes {
                let _ = if b == b'\n' { out.write_all(b"\r\n") } else { out.write_all(&[b]) };
            }
            let _ = out.flush();
        };
        let (err, val) = match eid {
            0x00 | 0x5449_4d45 => {
                self.csr[STIMECMP] = a[0]; // set_timer (Sstc compare register)
                (0, 0)
            }
            0x01 => {
                put(&[a[0] as u8]);
                (0, 0)
            }
            0x02 => (-1i64 as u64, 0), // console_getchar: nothing
            0x08 => return Err(Stop::Halt(0)),
            0x10 => match fid {
                0 => (0, 2 << 24),          // spec version 2.0
                1 => (0, 0x7275_766d),      // implementation id ("ruvm")
                2 => (0, 0x0004_0000),      // implementation version
                3 => (0, (EXTS.contains(&a[0]) || a[0] <= 8) as u64),
                _ => (0, 0),                // mvendorid / marchid / mimpid
            },
            0x73_5049 | 0x5246_4e43 => {
                // IPI send_ipi / RFENCE remote_*: (hart_mask, hart_mask_base) select the target harts
                let (mask, base) = (a[0], a[1]);
                let n = bus.ncpus;
                for h in 0..n {
                    let hit = base == u64::MAX || (h as u64 >= base && h as u64 - base < 64 && mask >> (h as u64 - base) & 1 == 1);
                    if !hit {
                        continue;
                    }
                    if eid == 0x73_5049 {
                        if h == self.id { self.csr[MIP] |= SSI } else { bus.ctl[h].ssip = true }
                    } else if h == self.id {
                        self.flush_tlb();
                    } else {
                        bus.ctl[h].flush_tlb = true;
                    }
                }
                (0, 0)
            }
            0x48_534d => match fid {
                0 => {
                    // hart_start(hartid, start_addr, opaque)
                    let h = a[0] as usize;
                    if a[0] >= bus.ncpus as u64 {
                        (INVALID_PARAM, 0)
                    } else if !bus.ctl[h].parked || bus.ctl[h].start.is_some() {
                        (ALREADY_AVAILABLE, 0)
                    } else {
                        bus.ctl[h].start = Some((a[1], a[2]));
                        (0, 0)
                    }
                }
                1 => {
                    // hart_stop: this hart stops (it never returns from the call)
                    bus.ctl[self.id].parked = true;
                    self.recheck = true;
                    (0, 0)
                }
                2 => {
                    // hart_get_status: STARTED 0, STOPPED 1, START_PENDING 2
                    if a[0] >= bus.ncpus as u64 {
                        (INVALID_PARAM, 0)
                    } else {
                        let c = &bus.ctl[a[0] as usize];
                        (0, if c.start.is_some() { 2 } else if c.parked { 1 } else { 0 })
                    }
                }
                3 if a[0] as u32 == 0 => {
                    // hart_suspend, default retentive: sleep until an interrupt, then resume here
                    bus.ctl[self.id].waiting = true;
                    self.recheck = true;
                    (0, 0)
                }
                _ => (NOT_SUPPORTED, 0),
            },
            0x5352_5354 if fid == 0 => {
                // system_reset: type 0 = shutdown, 1/2 = cold/warm reboot
                return Err(if a[0] == 0 { Stop::Halt((a[1] != 0) as i32) } else { Stop::Reset });
            }
            // PMU: just the fixed cycle (0) and instret (2) counters, no events. Linux's
            // driver needs this to grant user-mode `rdtime` (it then sets scounteren).
            0x50_4d55 => match fid {
                0 => (0, 3),                                                   // num_counters
                1 if a[0] == 0 || a[0] == 2 => (0, 0xc00 + a[0] | 63 << 12),   // counter_get_info
                1 => (INVALID_PARAM, 0),
                3 | 4 => (0, 0),                                               // start / stop
                _ => (NOT_SUPPORTED, 0),                                       // cfg_match, fw_read, ...
            },
            0x4442_434e => match fid {
                0 => {
                    let base = a[1] | a[2] << 32;
                    let bytes: Vec<u8> = (0..a[0]).map(|k| bus.read(base + k, 1).unwrap_or(0) as u8).collect();
                    put(&bytes);
                    (0, a[0])
                }
                1 => (0, 0),
                2 => {
                    put(&[a[0] as u8]);
                    (0, 0)
                }
                _ => (NOT_SUPPORTED, 0),
            },
            _ => (NOT_SUPPORTED, 0),
        };
        if eid < 0x10 {
            self.x[10] = err | val; // legacy calls return a single value
        } else {
            self.x[10] = err;
            self.x[11] = val;
        }
        Ok(())
    }

    #[inline]
    fn set(&mut self, rd: usize, v: u64) {
        if rd != 0 {
            self.x[rd] = v;
        }
    }

    /// mstatus with the read-only SD summary bit.
    fn mstatus(&self) -> u64 {
        let ms = self.csr[MSTATUS];
        if ms & MS_FS == MS_FS { ms | MS_SD } else { ms }
    }

    fn fp_dirty(&mut self) {
        self.csr[MSTATUS] |= MS_FS;
    }

    fn mip(&self) -> u64 {
        self.csr[MIP] | self.hw_ip
    }

    // ------------------------------------------------------------------ CSRs

    /// `None` = no such CSR (illegal instruction).
    fn csr_read(&self, a: usize) -> Option<u64> {
        Some(match a {
            0x001..=0x003 if self.csr[MSTATUS] & MS_FS == 0 => return None,
            0x001 => self.fflags as u64,
            0x002 => self.frm as u64,
            0x003 => (self.frm as u64) << 5 | self.fflags as u64,
            SSTATUS => self.mstatus() & SSTATUS_MASK,
            MSTATUS => self.mstatus(),
            SIE => self.csr[MIE] & self.csr[MIDELEG],
            SIP => self.mip() & self.csr[MIDELEG],
            MIP => self.mip(),
            STVEC | SCOUNTEREN | SENVCFG | SSCRATCH | SEPC | SCAUSE | STVAL | SATP | STIMECMP => self.csr[a],
            MISA | MEDELEG | MIDELEG | MIE | MTVEC | MCOUNTEREN | MENVCFG | MCOUNTINHIBIT
            | MSCRATCH | MEPC | MCAUSE | MTVAL => self.csr[a],
            0x3a0 | 0x3a2 | 0x3b0..=0x3bf => self.csr[a], // pmpcfg0/2, pmpaddr0-15 (16 regions)
            0x3a1 | 0x3a3 => return None,               // odd pmpcfg registers do not exist on RV64
            0x3a4..=0x3af | 0x3c0..=0x3ef => 0,         // regions 16.. are not implemented: read as zero
            0xb00 | 0xb02 | 0xc00 | 0xc02 => self.insns, // mcycle minstret cycle instret
            0xc01 => self.time,
            0xb03..=0xb1f | 0xc03..=0xc1f | 0x323..=0x33f => 0, // hpm counters / events
            0xf14 => self.id as u64, // mhartid
            0xf11..=0xf15 => 0,      // mvendorid marchid mimpid mconfigptr
            _ => return None,
        })
    }

    fn csr_write(&mut self, a: usize, v: u64) {
        self.fetch_vpn = 0;
        match a {
            0x001 => {
                self.fflags = v as u8 & 0x1f;
                self.fp_dirty();
            }
            0x002 => {
                self.frm = v as u8 & 7;
                self.fp_dirty();
            }
            0x003 => {
                self.fflags = v as u8 & 0x1f;
                self.frm = (v >> 5) as u8 & 7;
                self.fp_dirty();
            }
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
                if mode == 0 || mode == 8 || mode == 9 {
                    self.csr[SATP] = v; // ASID bits kept but unused (TLB is flushed instead)
                    self.flush_tlb();
                }
            }
            0x3a0..=0x3ef => self.pmp_csr_write(a, v),
            MCOUNTEREN | SCOUNTEREN => self.csr[a] = v & 0xffff_ffff,
            MISA | MCOUNTINHIBIT | 0xb00..=0xb1f | 0x323..=0x33f => {} // WARL, read-only zero or ignored
            _ => self.csr[a] = v,
        }
    }

    fn csr_op(&mut self, i: u32, rd: usize, rs1: usize, f3: u32) -> Result<(), Ex> {
        self.recheck = true;
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
        // Sstc: S-mode may use stimecmp only when menvcfg.STCE and mcounteren.TM are set
        if a == STIMECMP && self.prv == PRV_S && (self.csr[MENVCFG] >> 63 == 0 || self.csr[MCOUNTEREN] & 2 == 0) {
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
        self.fetch_vpn = 0;
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
        self.fetch_vpn = 0;
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
    /// Virtual -> physical address translation followed by the PMP check.
    #[inline]
    fn translate(&mut self, bus: &mut Bus, va: u64, acc: Access) -> Result<u64, Ex> {
        let pa = self.translate_va(bus, va, acc)?;
        let ms = self.csr[MSTATUS];
        let prv = if acc != Access::Fetch && ms & MS_MPRV != 0 { (ms >> 11) & 3 } else { self.prv };
        if (prv < PRV_M && !self.pmp_all) || (prv == PRV_M && self.pmp_locked_any) {
            if !self.pmp_allows(pa, acc, prv) {
                return Err(Ex::Trap(acc.access_fault(), va));
            }
        }
        Ok(pa)
    }

    /// PMP lookup: the first (lowest-numbered) matching region decides; with no match M-mode
    /// is allowed and S/U-mode is not.
    fn pmp_allows(&self, pa: u64, acc: Access, prv: u64) -> bool {
        let need = match acc {
            Access::Load => 1,
            Access::Store => 2,
            Access::Fetch => 4,
        };
        for e in &self.pmp {
            if pa >= e.lo && pa < e.hi {
                return (prv == PRV_M && !e.locked) || e.perm & need != 0;
            }
        }
        prv == PRV_M
    }

    /// Recompute the active PMP regions from pmpcfg0/2 and pmpaddr0..15.
    fn pmp_rebuild(&mut self) {
        self.pmp.clear();
        self.pmp_locked_any = false;
        let mut prev_addr = 0u64;
        for k in 0..16usize {
            let cfg = (self.csr[0x3a0 + 2 * (k / 8)] >> (8 * (k % 8))) as u8;
            let addr = self.csr[0x3b0 + k];
            let (lo, hi) = match (cfg >> 3) & 3 {
                0 => (0, 0), // OFF
                1 => (prev_addr << 2, addr << 2), // TOR
                2 => (addr << 2, (addr << 2) + 4), // NA4
                _ => {
                    let mask = addr ^ addr.wrapping_add(1);
                    ((addr & !mask) << 2, ((addr & !mask) << 2).saturating_add((mask.wrapping_add(1)) << 2))
                }
            };
            if hi > lo {
                let locked = cfg & 0x80 != 0;
                self.pmp_locked_any |= locked;
                self.pmp.push(PmpEntry { lo, hi, perm: cfg & 7, locked });
            }
            prev_addr = addr;
        }
        // one region covering everything with RWX (what OpenSBI and xv6 configure)
        self.pmp_all = matches!(self.pmp.first(), Some(e) if e.lo == 0 && e.hi >= 1 << 56 && e.perm == 7);
    }

    fn pmp_csr_write(&mut self, a: usize, v: u64) {
        if (0x3a0..=0x3af).contains(&a) {
            if a & 1 == 0 && a < 0x3a4 {
                let old = self.csr[a];
                let mut new = 0u64;
                for k in 0..8 {
                    let byte = if old >> (8 * k) & 0x80 != 0 { old >> (8 * k) } else { v >> (8 * k) };
                    new |= (byte & 0x9f) << (8 * k);
                }
                self.csr[a] = new;
            }
        } else if (0x3b0..=0x3bf).contains(&a) {
            let k = a - 0x3b0;
            let cfg_of = |s: &Self, n: usize| (s.csr[0x3a0 + 2 * (n / 8)] >> (8 * (n % 8))) as u8;
            let locked = cfg_of(self, k) & 0x80 != 0;
            let tor_next_locked = k + 1 < 16 && cfg_of(self, k + 1) & 0x80 != 0 && (cfg_of(self, k + 1) >> 3) & 3 == 1;
            if !locked && !tor_next_locked {
                self.csr[a] = v & ((1 << 54) - 1);
            }
        }
        self.pmp_rebuild();
    }

    fn translate_va(&mut self, bus: &mut Bus, va: u64, acc: Access) -> Result<u64, Ex> {
        let ms = self.csr[MSTATUS];
        let prv = if acc != Access::Fetch && ms & MS_MPRV != 0 { (ms >> 11) & 3 } else { self.prv };
        let satp = self.csr[SATP];
        let mode = satp >> 60;
        if prv == PRV_M || (mode != 8 && mode != 9) {
            return Ok(va);
        }
        let pf = Ex::Trap(acc.page_fault(), va);
        if sext(va, if mode == 8 { 39 } else { 48 }) != va {
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
        let mut level = if mode == 8 { 2 } else { 3 };
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
        let po = (pc & 0xfff) as usize;
        if self.fetch_vpn == (pc >> 12) + 1 && po <= 0xffc {
            let off = self.fetch_off + po;
            let w = u32::from_le_bytes(bus.ram()[off..off + 4].try_into().unwrap());
            if w & 3 != 3 {
                let i = self.rvc_table[(w & 0xffff) as usize];
                if i == 0 {
                    return Err(Ex::Trap(ILLEGAL, (w & 0xffff) as u64));
                }
                return Ok((i, 2));
            }
            return Ok((w, 4));
        }
        let pa = self.translate(bus, pc, Access::Fetch)?;
        if let Some(off) = bus.ram_off(pa & !0xfff, 0x1000) {
            self.fetch_vpn = (pc >> 12) + 1;
            self.fetch_off = off;
        }
        let lo = bus.read(pa, 2).map_err(|_| Ex::Trap(INSN_ACCESS, pc))? as u32;
        if lo & 3 != 3 {
            let i = self.rvc_table[lo as u16 as usize];
            if i == 0 {
                return Err(Ex::Trap(ILLEGAL, lo as u64));
            }
            return Ok((i, 2));
        }
        let hi_va = pc.wrapping_add(2);
        let hi_pa = if pc & 0xfff == 0xffe { self.translate(bus, hi_va, Access::Fetch)? } else { pa + 2 };
        let hi = bus.read(hi_pa, 2).map_err(|_| Ex::Trap(INSN_ACCESS, hi_va))? as u32;
        Ok((lo | hi << 16, 4))
    }

    // ------------------------------------------------------------------ execute

    #[inline(never)]
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
                    1 if (i >> 20) == 0x600 => a.leading_zeros() as u64,                 // clz
                    1 if (i >> 20) == 0x601 => a.trailing_zeros() as u64,                // ctz
                    1 if (i >> 20) == 0x602 => a.count_ones() as u64,                    // cpop
                    1 if (i >> 20) == 0x604 => a as i8 as i64 as u64,                    // sext.b
                    1 if (i >> 20) == 0x605 => a as i16 as i64 as u64,                   // sext.h
                    1 if (i >> 26) == 0b010010 => a & !(1u64 << sh),                     // bclri
                    1 if (i >> 26) == 0b011010 => a ^ (1u64 << sh),                      // binvi
                    1 if (i >> 26) == 0b001010 => a | (1u64 << sh),                      // bseti
                    2 => ((a as i64) < (imm_i as i64)) as u64,
                    3 => (a < imm_i) as u64,
                    4 => a ^ imm_i,
                    5 if (i >> 26) == 0 => a >> sh,
                    5 if (i >> 26) == 0x10 => ((a as i64) >> sh) as u64,
                    5 if (i >> 26) == 0b011000 => a.rotate_right(sh),                    // rori
                    5 if (i >> 26) == 0b010010 => a >> sh & 1,                           // bexti
                    5 if (i >> 20) == 0x6b8 => a.swap_bytes(),                           // rev8
                    5 if (i >> 20) == 0x287 => (0..8).fold(0u64, |r, k| {                // orc.b
                        if a >> (8 * k) & 0xff != 0 { r | 0xffu64 << (8 * k) } else { r }
                    }),
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
                    (1, _) if (i >> 26) == 0b000010 => (a & 0xffff_ffff) << ((i >> 20) & 0x3f), // slli.uw
                    (1, 0x30) if (i >> 20) & 0x1f == 0 => (a as u32).leading_zeros() as u64,   // clzw
                    (1, 0x30) if (i >> 20) & 0x1f == 1 => (a as u32).trailing_zeros() as u64,  // ctzw
                    (1, 0x30) if (i >> 20) & 0x1f == 2 => (a as u32).count_ones() as u64,      // cpopw
                    (5, 0) => sx32((a as u32) >> sh),
                    (5, 0x20) => ((a as i32) >> sh) as i64 as u64,
                    (5, 0x30) => sx32((a as u32).rotate_right(sh)),                            // roriw
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
                    // Zba
                    (0x10, 2) => (a << 1).wrapping_add(b),
                    (0x10, 4) => (a << 2).wrapping_add(b),
                    (0x10, 6) => (a << 3).wrapping_add(b),
                    // Zbb
                    (0x20, 7) => a & !b,
                    (0x20, 6) => a | !b,
                    (0x20, 4) => !(a ^ b),
                    (5, 4) => (a as i64).min(b as i64) as u64,
                    (5, 5) => a.min(b),
                    (5, 6) => (a as i64).max(b as i64) as u64,
                    (5, 7) => a.max(b),
                    (0x30, 1) => a.rotate_left((b & 63) as u32),
                    (0x30, 5) => a.rotate_right((b & 63) as u32),
                    // Zbs
                    (0x24, 1) => a & !(1u64 << (b & 63)),
                    (0x24, 5) => a >> (b & 63) & 1,
                    (0x34, 1) => a ^ (1u64 << (b & 63)),
                    (0x14, 1) => a | (1u64 << (b & 63)),
                    // Zbc
                    (5, 1) => clmul128(a, b).0,
                    (5, 3) => clmul128(a, b).1,
                    (5, 2) => {
                        // clmulr: bits 126..63 of the product
                        let (lo, hi) = clmul128(a, b);
                        hi << 1 | lo >> 63
                    }
                    // Zicond
                    (7, 5) => if b == 0 { 0 } else { a },
                    (7, 7) => if b != 0 { 0 } else { a },
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
                    (4, 0) => (a & 0xffff_ffff).wrapping_add(b),                       // add.uw
                    (4, 4) if rs2 == 0 => a & 0xffff,                                  // zext.h
                    (0x10, 2) => ((a & 0xffff_ffff) << 1).wrapping_add(b),             // sh1add.uw
                    (0x10, 4) => ((a & 0xffff_ffff) << 2).wrapping_add(b),             // sh2add.uw
                    (0x10, 6) => ((a & 0xffff_ffff) << 3).wrapping_add(b),             // sh3add.uw
                    (0x30, 1) => sx32(x.rotate_left(y & 31)),                          // rolw
                    (0x30, 5) => sx32(x.rotate_right(y & 31)),                         // rorw
                    _ => return Err(illegal),
                };
                self.set(rd, v);
            }
            0x0f if f3 == 2 => {
                // Zicbom / Zicboz: cbo.inval / clean / flush / zero (cache blocks are 64 bytes)
                let imm = i >> 20;
                let (menv, senv) = (self.csr[MENVCFG], self.csr[0x10a]);
                let allowed = |field_mask: u64, nonzero: bool| {
                    let f = |c: u64| if nonzero { c & field_mask != 0 } else { c & field_mask != 0 };
                    self.prv == PRV_M || (f(menv) && (self.prv == PRV_S || f(senv)))
                };
                if rd != 0 {
                    return Err(illegal);
                }
                let addr = a & !63;
                // blocked by menvcfg/senvcfg: QEMU reports mtval = 0 for this trap
                let blocked = Ex::Trap(ILLEGAL, 0);
                match imm {
                    0 if allowed(3 << 4, true) => {
                        self.translate(bus, addr, Access::Store)?; // permission check only
                    }
                    1 | 2 if allowed(1 << 6, true) => {
                        self.translate(bus, addr, Access::Load)?;
                    }
                    4 if allowed(1 << 7, true) => {
                        for k in 0..8 {
                            self.store(bus, addr + 8 * k, 8, 0)?;
                        }
                    }
                    0 | 1 | 2 | 4 => return Err(blocked),
                    _ => return Err(illegal),
                }
            }
            0x0f => {} // FENCE / FENCE.I: nothing to do on a single hart without caches
            0x07 | 0x27 | 0x43 | 0x47 | 0x4b | 0x4f | 0x53 => self.fp(bus, i, op, rd, rs1, rs2, f3)?,
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
                        self.recheck = true;
                        self.prv = mpp;
                        self.fetch_vpn = 0;
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
                        self.recheck = true;
                        self.prv = spp;
                        self.fetch_vpn = 0;
                        next = self.csr[SEPC];
                    }
                    (0x08, 5, 0) if rs1 == 0 => {
                        // WFI
                        if self.prv == PRV_U || (self.prv == PRV_S && self.csr[MSTATUS] & MS_TW != 0) {
                            return Err(illegal);
                        }
                        if self.mip() & self.csr[MIE] == 0 {
                            // sleep until an interrupt is pending (the scheduler skips this hart)
                            bus.ctl[self.id].waiting = true;
                            self.recheck = true;
                        }
                    }
                    (0x09, _, 0) | (0x0b, _, 0) => {
                        // SFENCE.VMA / SINVAL.VMA (Svinval)
                        if self.prv == PRV_U || (self.prv == PRV_S && self.csr[MSTATUS] & MS_TVM != 0) {
                            return Err(illegal);
                        }
                        self.flush_tlb();
                    }
                    (0x0c, 0 | 1, 0) if rd == 0 && rs1 == 0 => {
                        // SFENCE.W.INVAL / SFENCE.INVAL.IR (Svinval): ordering only
                        if self.prv == PRV_U || (self.prv == PRV_S && self.csr[MSTATUS] & MS_TVM != 0) {
                            return Err(illegal);
                        }
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

    // ------------------------------------------------------------------ F / D

    /// Read FP register `r` as format `fmt` (an improperly NaN-boxed single reads as NaN).
    fn fget(&self, fmt: Fmt, r: usize) -> u64 {
        let v = self.f[r];
        if fmt.sign_bit() == F64.sign_bit() {
            v
        } else if v >> 32 == 0xffff_ffff {
            v & 0xffff_ffff
        } else {
            F32.canonical_nan()
        }
    }

    fn fset(&mut self, fmt: Fmt, r: usize, v: u64) {
        self.f[r] = if fmt.sign_bit() == F64.sign_bit() { v } else { v | 0xffff_ffff_0000_0000 };
        self.fp_dirty();
    }

    #[allow(clippy::too_many_arguments)]
    fn fp(&mut self, bus: &mut Bus, i: u32, op: u32, rd: usize, rs1: usize, rs2: usize, f3: u32) -> Result<(), Ex> {
        let illegal = Ex::Trap(ILLEGAL, i as u64);
        if self.csr[MSTATUS] & MS_FS == 0 {
            return Err(illegal);
        }
        // loads / stores
        if op == 0x07 || op == 0x27 {
            let s = i as i32 as i64;
            let imm = if op == 0x07 { (s >> 20) as u64 } else { (((s >> 25) << 5) | ((i >> 7) & 0x1f) as i64) as u64 };
            let addr = self.x[rs1].wrapping_add(imm);
            match (op, f3) {
                (0x07, 2) => {
                    let v = self.load(bus, addr, 4)?;
                    self.fset(F32, rd, v);
                }
                (0x07, 3) => {
                    let v = self.load(bus, addr, 8)?;
                    self.fset(F64, rd, v);
                }
                (0x27, 2) => self.store(bus, addr, 4, self.f[rs2])?,
                (0x27, 3) => self.store(bus, addr, 8, self.f[rs2])?,
                _ => return Err(illegal),
            }
            return Ok(());
        }
        let fmt = match (i >> 25) & 3 {
            0 => F32,
            1 => F64,
            _ => return Err(illegal),
        };
        let rm = if f3 == 7 { self.frm as u32 } else { f3 };
        let rm_ok = rm <= 4;
        let rm = rm as u8;
        let mut fl = 0u8;
        let (a, b) = (self.fget(fmt, rs1), self.fget(fmt, rs2));
        if op != 0x53 {
            // FMADD / FMSUB / FNMSUB / FNMADD
            if !rm_ok {
                return Err(illegal);
            }
            let c = self.fget(fmt, (i >> 27) as usize);
            let neg_prod = op == 0x4b || op == 0x4f;
            let neg_c = op == 0x47 || op == 0x4f;
            let a = if neg_prod { a ^ fmt.sign_bit() } else { a };
            let c = if neg_c { c ^ fmt.sign_bit() } else { c };
            let r = sf::fma(fmt, a, b, c, rm, &mut fl);
            self.fset(fmt, rd, r);
        } else {
            let needs_rm = matches!(i >> 27, 0x00 | 0x01 | 0x02 | 0x03 | 0x0b | 0x08 | 0x18 | 0x1a);
            if needs_rm && !rm_ok {
                return Err(illegal);
            }
            match (i >> 27, f3, rs2) {
                (0x00, _, _) => self.fset(fmt, rd, sf::add(fmt, a, b, rm, &mut fl)),
                (0x01, _, _) => self.fset(fmt, rd, sf::sub(fmt, a, b, rm, &mut fl)),
                (0x02, _, _) => self.fset(fmt, rd, sf::mul(fmt, a, b, rm, &mut fl)),
                (0x03, _, _) => self.fset(fmt, rd, sf::div(fmt, a, b, rm, &mut fl)),
                (0x0b, _, 0) => self.fset(fmt, rd, sf::sqrt(fmt, a, rm, &mut fl)),
                (0x04, 0..=2, _) => {
                    let sb = fmt.sign_bit();
                    let sign = match f3 {
                        0 => b & sb,
                        1 => !b & sb,
                        _ => (a ^ b) & sb,
                    };
                    self.fset(fmt, rd, (a & !sb) | sign);
                }
                (0x05, 0 | 1, _) => self.fset(fmt, rd, sf::min_max(fmt, a, b, f3 == 1, &mut fl)),
                (0x08, _, 1) if fmt.sign_bit() == F32.sign_bit() => {
                    let v = sf::convert(F64, F32, self.fget(F64, rs1), rm, &mut fl); // fcvt.s.d
                    self.fset(F32, rd, v);
                }
                (0x08, _, 0) if fmt.sign_bit() == F64.sign_bit() => {
                    let v = sf::convert(F32, F64, self.fget(F32, rs1), rm, &mut fl); // fcvt.d.s
                    self.fset(F64, rd, v);
                }
                (0x14, 0..=2, _) => {
                    let r = match f3 {
                        2 => sf::eq(fmt, a, b, &mut fl),
                        1 => sf::lt(fmt, a, b, &mut fl),
                        _ => sf::le(fmt, a, b, &mut fl),
                    };
                    self.set(rd, r as u64);
                }
                (0x18, _, 0..=3) => {
                    let bits = if rs2 < 2 { 32 } else { 64 };
                    let v = sf::to_int(fmt, a, rm, rs2 & 1 == 0, bits, &mut fl);
                    self.set(rd, v);
                }
                (0x1a, _, 0..=3) => {
                    let bits = if rs2 < 2 { 32 } else { 64 };
                    let v = sf::from_int(fmt, self.x[rs1], rs2 & 1 == 0, bits, rm, &mut fl);
                    self.fset(fmt, rd, v);
                }
                (0x1c, 0, 0) => {
                    // fmv.x.w / fmv.x.d: raw bits
                    let v = self.f[rs1];
                    self.set(rd, if fmt.sign_bit() == F32.sign_bit() { v as i32 as i64 as u64 } else { v });
                }
                (0x1c, 1, 0) => self.set(rd, sf::classify(fmt, a)),
                (0x1e, 0, 0) => {
                    let v = self.x[rs1];
                    self.fset(fmt, rd, if fmt.sign_bit() == F32.sign_bit() { v & 0xffff_ffff } else { v });
                }
                _ => return Err(illegal),
            }
        }
        if fl != 0 {
            self.fflags |= fl;
            self.fp_dirty();
        }
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
        Self::new(0)
    }
}

impl Cpu for Riscv64 {
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
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
        self.pre(bus)?;
        match self.exec(bus) {
            Ok(()) => Ok(()),
            Err(e) => self.handle_ex(bus, e),
        }
    }

    /// Batched execution: board time, interrupt lines and the halt flag are examined once
    /// per batch of up to `BATCH` instructions (interrupts are taken at batch boundaries).
    fn wake_deadline(&self, bus: &Bus) -> u64 {
        let mut d = bus.clint.mtimecmp[self.id];
        if self.csr[MENVCFG] >> 63 == 1 {
            d = d.min(self.csr[STIMECMP]);
        }
        d
    }

    fn run(&mut self, bus: &mut Bus, n: u64) -> (u64, Option<Stop>) {
        const BATCH: u64 = 32;
        bus.cur_cpu = self.id;
        self.service_ctl(bus);
        if bus.ctl[self.id].parked {
            return (0, None);
        }
        if bus.ctl[self.id].waiting {
            self.sample_lines(bus);
            if self.mip() & self.csr[MIE] == 0 {
                return (0, None); // still asleep
            }
            bus.ctl[self.id].waiting = false;
        }
        let mut done = 0;
        while done < n {
            let k = (n - done).min(BATCH);
            bus.clint.mtime += bus.ticks(k);
            self.insns += k;
            if let Err(s) = self.pre(bus) {
                return (done + 1, Some(s));
            }
            let mut j = 0;
            while j < k {
                j += 1;
                if let Err(e) = self.exec(bus) {
                    if let Err(s) = self.handle_ex(bus, e) {
                        return (done + j, Some(s));
                    }
                }
                if self.recheck | bus.irq_dirty {
                    break;
                }
            }
            self.recheck = false;
            bus.irq_dirty = false;
            // give back the unused part of the batch (board time and instret stay exact)
            bus.clint.mtime -= bus.ticks(k) - bus.ticks(j);
            self.insns -= k - j;
            done += j;
            if let Some(code) = bus.halt {
                return (done, Some(Stop::Halt(code)));
            }
            if bus.reset || bus.ctl[self.id].waiting || bus.ctl[self.id].parked {
                break;
            }
        }
        // never hand the board to another hart in the middle of an LR/SC sequence
        let mut extra = 0;
        while self.resv.is_some() && extra < 64 {
            extra += 1;
            self.insns += 1;
            if let Err(e) = self.exec(bus) {
                if let Err(s) = self.handle_ex(bus, e) {
                    return (done + extra, Some(s));
                }
            }
        }
        (done + extra, None)
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
