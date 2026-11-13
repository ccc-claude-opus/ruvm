//! AArch64 (ARMv8-A, A64): integer, scalar FP and Advanced SIMD, EL0/EL1, stage-1 MMU,
//! generic timer and GIC interrupts; single core.
//!
//! Supported: data-processing (immediate & register), bitfield/extract, move-wide,
//! ADR/ADRP, all integer loads/stores (imm, register offset, pre/post-index, pairs,
//! literal), branches, CSEL family, CCMP/CCMN, MUL/DIV/MADD, CLZ/REV/RBIT,
//! NOP/barriers, exclusives (LDXR/STXR/LDXP/STXP), load-acquire/store-release,
//! LSE atomics (LDADD.., SWP, CAS), HVC/SMC (PSCI), scalar FP (`fp`), Advanced SIMD (`neon`).
//! Exception levels EL0/EL1: VBAR_EL1 vectors, ESR/ELR/SPSR/FAR_EL1, ERET, SP_EL0/SP_EL1,
//! DAIF, synchronous exceptions (SVC, BRK, undefined, data/instruction abort).
//! While VBAR_EL1 is 0, SVC is a host syscall and faults stop the VM (bare programs).
//! Stage-1 MMU (4 KiB granule, see `mmu`), TLB, cache/TLB maintenance instructions.
//! Not supported: EL2/EL3, 16K/64K granules, hardware A/D updates.

use super::{sext, Cpu};
use crate::bus::Bus;
use crate::devices::Clint;
use crate::host::{self, Host};
use crate::machine::Stop;
use std::collections::HashMap;

mod crypto;
mod fp;
mod mmu;
mod neon;
mod simd;
use mmu::TlbEntry;

/// System-register key: the o0:op1:CRn:CRm:op2 field of MRS/MSR (bits 19:5).
const fn sr(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    (op0 - 2) << 14 | op1 << 11 | crn << 7 | crm << 3 | op2
}
const SYS_NZCV: u32 = sr(3, 3, 4, 2, 0);
const SYS_DAIF: u32 = sr(3, 3, 4, 2, 1);
const SYS_CURRENT_EL: u32 = sr(3, 0, 4, 2, 2);
const SYS_SPSEL: u32 = sr(3, 0, 4, 2, 0);
const SYS_SPSR_EL1: u32 = sr(3, 0, 4, 0, 0);
const SYS_ELR_EL1: u32 = sr(3, 0, 4, 0, 1);
const SYS_SP_EL0: u32 = sr(3, 0, 4, 1, 0);
const SYS_ESR_EL1: u32 = sr(3, 0, 5, 2, 0);
const SYS_FAR_EL1: u32 = sr(3, 0, 6, 0, 0);
const SYS_VBAR_EL1: u32 = sr(3, 0, 12, 0, 0);
const SYS_MIDR_EL1: u32 = sr(3, 0, 0, 0, 0);
const SYS_MPIDR_EL1: u32 = sr(3, 0, 0, 0, 5);
const SYS_CNTFRQ_EL0: u32 = sr(3, 3, 14, 0, 0);
const SYS_CNTPCT_EL0: u32 = sr(3, 3, 14, 0, 1);
const SYS_CNTVCT_EL0: u32 = sr(3, 3, 14, 0, 2);
const SYS_CNTP_TVAL: u32 = sr(3, 3, 14, 2, 0);
const SYS_CNTP_CTL: u32 = sr(3, 3, 14, 2, 1);
const SYS_CNTP_CVAL: u32 = sr(3, 3, 14, 2, 2);
const SYS_CNTV_TVAL: u32 = sr(3, 3, 14, 3, 0);
const SYS_CNTV_CTL: u32 = sr(3, 3, 14, 3, 1);
const SYS_CNTV_CVAL: u32 = sr(3, 3, 14, 3, 2);
const SYS_SCTLR_EL1: u32 = sr(3, 0, 1, 0, 0);
const SYS_TTBR0_EL1: u32 = sr(3, 0, 2, 0, 0);
const SYS_TTBR1_EL1: u32 = sr(3, 0, 2, 0, 1);
const SYS_TCR_EL1: u32 = sr(3, 0, 2, 0, 2);
const SYS_REVIDR_EL1: u32 = sr(3, 0, 0, 0, 6);
const SYS_CTR_EL0: u32 = sr(3, 3, 0, 0, 1);
const SYS_DCZID_EL0: u32 = sr(3, 3, 0, 0, 7);
const SYS_CLIDR_EL1: u32 = sr(3, 1, 0, 0, 1);
const fn sr_cntkctl() -> u32 {
    sr(3, 0, 14, 1, 0)
}
const SYS_FPCR: u32 = sr(3, 3, 4, 4, 0);
const SYS_FPSR: u32 = sr(3, 3, 4, 4, 1);

/// ID_AA64* / ID_* register values (read-only). A Cortex-A57 r1p3-like CPU without
/// EL2/EL3, PMU, SVE, pointer authentication or hardware A/D updates (but with AES/SHA/CRC32/LSE).
fn id_reg(key: u32) -> Option<u64> {
    const fn r(crm: u32, op2: u32) -> u32 {
        sr(3, 0, 0, crm, op2)
    }
    Some(match key {
        k if k == r(4, 0) => 0x0000_0000_0000_0011, // ID_AA64PFR0: EL0/EL1 = AArch64, FP + AdvSIMD present
        k if k == r(5, 0) => 0x0000_0000_0000_0006, // ID_AA64DFR0: ARMv8.0 debug, no PMU
        k if k == r(6, 0) => 0x0000_0000_0021_1120, // ID_AA64ISAR0: AES+PMULL, SHA1, SHA2, CRC32, LSE atomics
        k if k == r(7, 0) => 0x0000_0000_0f00_0022, // ID_AA64MMFR0: 40-bit PA, 16-bit ASID, 4K granule only
        // everything else in the ID block (PFR1, DFR1, AFR, ISAR0-2, MMFR1-2, ZFR0, AArch32 regs) reads as 0
        k if k >> 14 == 1 && (k >> 11) & 7 == 0 && (k >> 7) & 15 == 0 && (k >> 3) & 15 >= 1 => 0,
        _ => return None,
    })
}

// exception classes (ESR_ELx.EC)
const EC_UNKNOWN: u32 = 0x00;
const EC_SVC64: u32 = 0x15;
const EC_IABORT_LOWER: u32 = 0x20;
const EC_DABORT_LOWER: u32 = 0x24;
const EC_BRK: u32 = 0x3c;

/// Why an instruction did not complete. Boxed so that `Result<T, Ex>` stays two words wide
/// (the common, successful path returns in registers); allocation only happens on faults.
pub struct Ex(Box<ExKind>);

enum ExKind {
    Stop(Stop),
    /// Synchronous exception: class, syndrome, faulting address, return to next insn?
    Sync { ec: u32, iss: u32, far: Option<u64>, next: bool },
}

impl Ex {
    #[cold]
    fn sync(ec: u32, iss: u32, far: Option<u64>, next: bool) -> Ex {
        Ex(Box::new(ExKind::Sync { ec, iss, far, next }))
    }
    #[cold]
    fn stop(s: Stop) -> Ex {
        Ex(Box::new(ExKind::Stop(s)))
    }
}

impl From<Stop> for Ex {
    fn from(s: Stop) -> Ex {
        Ex::stop(s)
    }
}

pub struct Aarch64 {
    x: [u64; 31],
    sp: u64,
    pc: u64,
    n: bool,
    z: bool,
    c: bool,
    v: bool,
    /// Current exception level (0 or 1) and PSTATE.SP / PSTATE.DAIF.
    el: u32,
    spsel: bool,
    daif: u32,
    /// The banked stack pointer that is not currently selected (SP_EL0 or SP_EL1).
    sp_other: u64,
    /// Exclusive monitor: (address, size) of the last LDXR.
    excl: Option<(u64, usize)>,
    sysregs: HashMap<u32, u64>,
    // MMU
    sctlr: u64,
    tcr: u64,
    ttbr0: u64,
    ttbr1: u64,
    asid: u16,
    tlb: Vec<TlbEntry>,
    tlb_gen: u32,
    /// Instruction-fetch fast path: VPN + 1 of the current code page, RAM offset of the page.
    fetch_vpn: u64,
    fetch_off: usize,
    fetch_el: u32,
    /// Next data access is an unprivileged one (LDTR/STTR).
    unpriv: bool,
    /// Generic timers: [0] = physical (CNTP), [1] = virtual (CNTV): (ctl, cval).
    timer: [(u64, u64); 2],
    /// `mtime` value at which a timer comparator next changes state.
    timer_deadline: u64,
    cntkctl: u64,
    fpcr: u32,
    fpsr: u32,
    /// SIMD & FP register file (scalar B/H/S/D/Q registers are the low bits).
    vr: [u128; 32],
    /// Debug aid (env RUVM_LOG_UNDEF): log every undefined-instruction exception.
    log_undef: bool,
    last_insn: u32,
    /// CPU number (MPIDR Aff0).
    id: usize,
    /// Interrupt-relevant state changed: end the current batch so `pre` runs again.
    recheck: bool,
    bitmask_cache: Vec<(u32, Option<(u64, u64)>)>,
}

#[inline(always)]
fn ones(n: u32) -> u64 {
    if n >= 64 { !0 } else { (1u64 << n) - 1 }
}

#[inline(always)]
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

#[inline(always)]
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

#[inline(always)]
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
#[inline(always)]
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
    /// Requests from other CPUs: TLB flush, power-on (PSCI CPU_ON).
    fn service_ctl(&mut self, bus: &mut Bus) {
        let id = self.id;
        if std::mem::take(&mut bus.ctl[id].flush_tlb) {
            self.flush_tlb();
        }
        if let Some((pc, ctx)) = bus.ctl[id].start.take() {
            // power-on reset state: EL1h, MMU off, interrupts masked, x0 = context id
            *self = Aarch64::new(id);
            self.pc = pc;
            self.x[0] = ctx;
            bus.ctl[id].parked = false;
            bus.ctl[id].waiting = false;
        }
    }

    /// Timers and interrupts, checked between instructions. Returns true if an IRQ was taken.
    #[inline(always)]
    fn pre(&mut self, bus: &mut Bus) -> bool {
        if bus.clint.mtime >= self.timer_deadline {
            self.update_timers(bus);
        }
        if bus.gic.irq_line[self.id] && self.daif & 2 == 0 && self.sysreg(SYS_VBAR_EL1) != 0 {
            self.irq_exception();
            return true;
        }
        false
    }

    /// Handle an exception that stopped an instruction: enter the EL1 handler, or stop the VM.
    #[cold]
    #[inline(never)]
    fn handle_ex(&mut self, bus: &mut Bus, e: Ex) -> Result<(), Stop> {
        match *e.0 {
            ExKind::Stop(s) => Err(s),
            ExKind::Sync { ec, iss, far, next } => {
                if self.sysreg(SYS_VBAR_EL1) == 0 {
                    let i = bus.read(self.pc, 4).unwrap_or(0);
                    return Err(Stop::Fatal(match ec {
                        EC_UNKNOWN => format!("undefined instruction {:#010x} at pc={:#x}", i, self.pc),
                        EC_BRK => format!("BRK #{} at pc={:#x}", iss, self.pc),
                        _ => format!(
                            "unhandled exception EC={:#x} ISS={:#x} FAR={:#x} at pc={:#x} (data abort / bus error, VBAR_EL1 not set)",
                            ec, iss, far.unwrap_or(0), self.pc
                        ),
                    }));
                }
                if ec == EC_UNKNOWN && self.log_undef {
                    eprintln!("[undef] EL{} pc={:#x} insn={:08x}", self.el, self.pc, self.last_insn);
                }
                let ret = if next { self.pc.wrapping_add(4) } else { self.pc };
                self.exception(ec, iss, far, ret);
                Ok(())
            }
        }
    }

    pub fn new(id: usize) -> Self {
        Aarch64 {
            x: [0; 31],
            sp: 0,
            pc: 0,
            n: false,
            z: false,
            c: false,
            v: false,
            el: 1,
            spsel: true,
            daif: 0xf,
            sp_other: 0,
            excl: None,
            sysregs: HashMap::new(),
            sctlr: 0x30d0_0800, // RES1 bits
            tcr: 0,
            ttbr0: 0,
            ttbr1: 0,
            asid: 0,
            tlb: vec![TlbEntry::default(); mmu::TLB_SIZE],
            tlb_gen: 1,
            fetch_vpn: 0,
            fetch_off: 0,
            fetch_el: 0,
            unpriv: false,
            timer: [(0, 0); 2],
            timer_deadline: u64::MAX,
            cntkctl: 0,
            fpcr: 0,
            fpsr: 0,
            vr: [0; 32],
            log_undef: std::env::var_os("RUVM_LOG_UNDEF").is_some(),
            last_insn: 0,
            id,
            recheck: false,
            bitmask_cache: vec![(0, None); 256],
        }
    }

    pub fn set_reg(&mut self, r: usize, v: u64) {
        if r < 31 {
            self.x[r] = v;
        }
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

    #[inline(always)]
    fn set_flags(&mut self, f: (bool, bool, bool, bool)) {
        self.n = f.0;
        self.z = f.1;
        self.c = f.2;
        self.v = f.3;
    }

    #[inline(always)]
    fn nzcv(&self) -> u64 {
        ((self.n as u64) << 31) | ((self.z as u64) << 30) | ((self.c as u64) << 29) | ((self.v as u64) << 28)
    }

    #[inline(always)]
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

    /// DFSC 0x10 = synchronous external abort; bit 6 (WnR) marks writes.
    fn abort(&self, addr: u64, write: bool) -> Ex {
        Ex::sync(EC_DABORT_LOWER, 0x10 | (write as u32) << 6, Some(addr), false)
    }
    /// Recompute timer interrupt lines (PPI 30 = physical, PPI 27 = virtual) and the
    /// next time anything about them changes.
    fn update_timers(&mut self, bus: &mut Bus) {
        let now = bus.clint.mtime;
        let mut deadline = u64::MAX;
        for (k, ppi) in [30u32, 27].into_iter().enumerate() {
            let (ctl, cval) = self.timer[k];
            let enabled = ctl & 1 == 1;
            let met = now >= cval;
            let line = enabled && ctl & 2 == 0 && met;
            bus.gic.set_ppi(self.id, ppi, line);
            if enabled && !met {
                deadline = deadline.min(cval);
            }
        }
        self.timer_deadline = deadline;
    }

    fn timer_ctl(&self, k: usize, now: u64) -> u64 {
        let (ctl, cval) = self.timer[k];
        (ctl & 3) | ((ctl & 1 == 1 && now >= cval) as u64) << 2
    }

    fn sysreg_read(&mut self, bus: &mut Bus, key: u32) -> u64 {
        let now = bus.clint.mtime;
        if let Some(v) = id_reg(key) {
            return v;
        }
        match key {
            SYS_NZCV => self.nzcv(),
            SYS_DAIF => (self.daif as u64) << 6,
            SYS_CURRENT_EL => (self.el as u64) << 2,
            SYS_SPSEL => self.spsel as u64,
            SYS_SP_EL0 => self.sp_el0(),
            SYS_MIDR_EL1 => 0x411f_d073, // Cortex-A57 r1p3
            SYS_MPIDR_EL1 => 0x8000_0000 | self.id as u64,
            SYS_REVIDR_EL1 => 0,
            SYS_CTR_EL0 => 0x8444_c004,
            SYS_DCZID_EL0 => 4, // DC ZVA allowed, 64-byte blocks
            SYS_CLIDR_EL1 => 0,
            SYS_CNTFRQ_EL0 => Clint::FREQ,
            SYS_CNTPCT_EL0 | SYS_CNTVCT_EL0 => now,
            SYS_CNTP_CTL => self.timer_ctl(0, now),
            SYS_CNTV_CTL => self.timer_ctl(1, now),
            SYS_CNTP_CVAL => self.timer[0].1,
            SYS_CNTV_CVAL => self.timer[1].1,
            SYS_CNTP_TVAL => self.timer[0].1.wrapping_sub(now) as u32 as u64,
            SYS_CNTV_TVAL => self.timer[1].1.wrapping_sub(now) as u32 as u64,
            sr if sr == sr_cntkctl() => self.cntkctl,
            SYS_SCTLR_EL1 => self.sctlr,
            SYS_TTBR0_EL1 => self.ttbr0,
            SYS_TTBR1_EL1 => self.ttbr1,
            SYS_TCR_EL1 => self.tcr,
            SYS_FPCR => self.fpcr as u64,
            SYS_FPSR => self.fpsr as u64,
            _ => self.sysreg(key),
        }
    }

    fn sysreg_write(&mut self, bus: &mut Bus, key: u32, v: u64) {
        self.recheck = true;
        let now = bus.clint.mtime;
        match key {
            SYS_NZCV => self.set_flags((v >> 31 & 1 == 1, v >> 30 & 1 == 1, v >> 29 & 1 == 1, v >> 28 & 1 == 1)),
            SYS_DAIF => self.daif = (v >> 6) as u32 & 0xf,
            SYS_SPSEL => self.set_mode(self.el, v & 1 == 1),
            SYS_SP_EL0 => self.set_sp_el0(v),
            SYS_CURRENT_EL | SYS_MIDR_EL1 | SYS_MPIDR_EL1 | SYS_CNTPCT_EL0 | SYS_CNTVCT_EL0 => {}
            SYS_CNTP_CTL | SYS_CNTV_CTL => {
                let k = (key == SYS_CNTV_CTL) as usize;
                self.timer[k].0 = v & 3;
                self.update_timers(bus);
            }
            SYS_CNTP_CVAL | SYS_CNTV_CVAL => {
                self.timer[(key == SYS_CNTV_CVAL) as usize].1 = v;
                self.update_timers(bus);
            }
            SYS_CNTP_TVAL | SYS_CNTV_TVAL => {
                self.timer[(key == SYS_CNTV_TVAL) as usize].1 = now.wrapping_add(v as u32 as i32 as i64 as u64);
                self.update_timers(bus);
            }
            sr if sr == sr_cntkctl() => self.cntkctl = v,
            SYS_SCTLR_EL1 => {
                self.sctlr = v;
                self.flush_tlb();
            }
            SYS_TCR_EL1 => {
                self.tcr = v;
                self.update_asid();
                self.flush_tlb();
            }
            SYS_TTBR0_EL1 => {
                self.ttbr0 = v;
                self.update_asid();
            }
            SYS_TTBR1_EL1 => {
                self.ttbr1 = v;
                self.update_asid();
            }
            SYS_FPCR => self.fpcr = v as u32 & 0x07ff_9f00,
            SYS_FPSR => self.fpsr = v as u32 & 0xf800_009f,
            _ => {
                self.sysregs.insert(key, v);
            }
        }
    }

    /// SYS instruction (DC / IC / TLBI / AT): cache maintenance is a no-op (the emulated
    /// memory is always coherent); TLB maintenance flushes the TLB.
    fn sys_insn(&mut self, bus: &mut Bus, i: u32) -> Result<(), Ex> {
        let (op1, crn, crm, op2, rt) = ((i >> 16) & 7, (i >> 12) & 15, (i >> 8) & 15, (i >> 5) & 7, (i & 31) as usize);
        match (op1, crn, crm, op2) {
            (3, 7, 4, 1) => {
                let a = self.rz(rt);
                self.dc_zva(bus, a)?;
            }
            (_, 8, _, _) | (_, 7, 5, _) | (_, 7, 1, _) => {
                if crn == 8 {
                    // TLBI: flush this CPU and (inner-shareable broadcast) every other CPU
                    self.flush_tlb();
                    for o in 0..bus.ncpus {
                        if o != self.id {
                            bus.ctl[o].flush_tlb = true;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// `decode_bitmasks` through a small direct-mapped cache (logical immediates and
    /// bitfield moves decode the same few patterns over and over).
    #[inline(always)]
    fn cached_bitmasks(&mut self, n: u32, imms: u32, immr: u32, immediate: bool, datasize: u32) -> Option<(u64, u64)> {
        let key = 1 + (n | imms << 1 | immr << 7 | (immediate as u32) << 13 | (datasize >> 5) << 14);
        let slot = (key as usize ^ (key as usize >> 6)) & 255;
        let (k, r) = self.bitmask_cache[slot];
        if k == key {
            return r;
        }
        let r = decode_bitmasks(n, imms, immr, immediate, datasize);
        self.bitmask_cache[slot] = (key, r);
        r
    }

    fn undef(&self, _i: u32) -> Ex {
        Ex::sync(EC_UNKNOWN, 0, None, false)
    }

    /// SPSR value describing the current PSTATE.
    fn pstate(&self) -> u64 {
        self.nzcv() | (self.daif as u64) << 6 | (self.el as u64) << 2 | self.spsel as u64
    }

    /// Which SP is in use for (el, spsel): true = SP_EL1.
    fn uses_sp1(el: u32, spsel: bool) -> bool {
        el == 1 && spsel
    }

    /// Change EL / SPSel, swapping the banked stack pointers as needed.
    fn set_mode(&mut self, el: u32, spsel: bool) {
        if Self::uses_sp1(self.el, self.spsel) != Self::uses_sp1(el, spsel) {
            std::mem::swap(&mut self.sp, &mut self.sp_other);
        }
        self.el = el;
        self.spsel = spsel;
    }

    fn sp_el0(&self) -> u64 {
        if Self::uses_sp1(self.el, self.spsel) { self.sp_other } else { self.sp }
    }

    fn set_sp_el0(&mut self, v: u64) {
        if Self::uses_sp1(self.el, self.spsel) { self.sp_other = v } else { self.sp = v }
    }

    fn sysreg(&self, key: u32) -> u64 {
        *self.sysregs.get(&key).unwrap_or(&0)
    }

    /// Take an IRQ exception to EL1 (the return address is the interrupted instruction).
    fn irq_exception(&mut self) {
        let offset = match (self.el, self.spsel) {
            (1, false) => 0x080,
            (1, true) => 0x280,
            _ => 0x480,
        };
        self.sysregs.insert(SYS_SPSR_EL1, self.pstate());
        self.sysregs.insert(SYS_ELR_EL1, self.pc);
        self.excl = None;
        self.daif = 0xf;
        self.set_mode(1, true);
        self.pc = self.sysreg(SYS_VBAR_EL1).wrapping_add(offset);
    }

    /// Take a synchronous exception to EL1.
    fn exception(&mut self, ec: u32, iss: u32, far: Option<u64>, ret: u64) {
        // aborts taken from EL1 use the "same EL" class (lower-EL class + 1)
        let ec = if (ec == EC_IABORT_LOWER || ec == EC_DABORT_LOWER) && self.el == 1 { ec + 1 } else { ec };
        let offset = match (self.el, self.spsel) {
            (1, false) => 0x000,
            (1, true) => 0x200,
            _ => 0x400,
        };
        self.sysregs.insert(SYS_SPSR_EL1, self.pstate());
        self.sysregs.insert(SYS_ELR_EL1, ret);
        self.sysregs.insert(SYS_ESR_EL1, (ec as u64) << 26 | 1 << 25 | iss as u64);
        if let Some(a) = far {
            self.sysregs.insert(SYS_FAR_EL1, a);
        }
        self.excl = None;
        self.daif = 0xf;
        self.set_mode(1, true);
        self.pc = self.sysreg(SYS_VBAR_EL1).wrapping_add(offset);
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn addsub(&mut self, rd: usize, a: u64, b: u64, sub: bool, s: bool, sf: bool, rd_sp: bool) {
        let (res, fl) = if sub { awc(a, !b, true, sf) } else { awc(a, b, false, sf) };
        if s {
            self.set_flags(fl);
        }
        if rd_sp { self.ws(rd, res, sf) } else { self.wz(rd, res, sf) }
    }

    /// Single-register load/store; `opc`/`size` come from the instruction word.
    #[inline(always)]
    fn ldst(&mut self, bus: &mut Bus, i: u32, addr: u64) -> Result<(), Ex> {
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

    /// LDXR/STXR (+acquire/release forms), LDXP/STXP, LDAR/STLR, CAS.
    fn exclusive(&mut self, bus: &mut Bus, i: u32) -> Result<(), Ex> {
        let n = 1usize << (i >> 30);
        let (o2, l, o1) = ((i >> 23) & 1, (i >> 22) & 1, (i >> 21) & 1);
        let rs = ((i >> 16) & 31) as usize;
        let rt2 = ((i >> 10) & 31) as usize;
        let rt = (i & 31) as usize;
        let addr = self.rs(((i >> 5) & 31) as usize);
        let mask = if n == 8 { !0 } else { (1u64 << (8 * n)) - 1 };
        match (o2, o1, l) {
            (0, 0, 1) => {
                // LDXR / LDAXR
                let v = self.rd(bus, addr, n)?;
                self.excl = Some((addr, n));
                self.wz(rt, v, true);
            }
            (0, 0, 0) => {
                // STXR / STLXR: Ws = 0 on success
                let ok = self.excl == Some((addr, n));
                if ok {
                    self.wr(bus, addr, n, self.rz(rt))?;
                }
                self.excl = None;
                self.wz(rs, !ok as u64, false);
            }
            (0, 1, _) if n >= 4 => {
                // LDXP / STXP (and acquire/release forms)
                if l == 1 {
                    let v1 = self.rd(bus, addr, n)?;
                    let v2 = self.rd(bus, addr.wrapping_add(n as u64), n)?;
                    self.excl = Some((addr, 2 * n));
                    self.wz(rt, v1, true);
                    self.wz(rt2, v2, true);
                } else {
                    let ok = self.excl == Some((addr, 2 * n));
                    if ok {
                        self.wr(bus, addr, n, self.rz(rt))?;
                        self.wr(bus, addr.wrapping_add(n as u64), n, self.rz(rt2))?;
                    }
                    self.excl = None;
                    self.wz(rs, !ok as u64, false);
                }
            }
            (0, 1, _) if n < 4 && rt2 == 31 => {
                // CASP / CASPA / CASPL / CASPAL: compare-and-swap pair of 32- or 64-bit values
                let eb = 4usize << ((i >> 30) & 1);
                if rs & 1 == 1 || rt & 1 == 1 {
                    return Err(self.undef(i));
                }
                let wide = eb == 8;
                let m = if wide { !0u64 } else { 0xffff_ffff };
                let (o1, o2) = (self.rd(bus, addr, eb)?, self.rd(bus, addr.wrapping_add(eb as u64), eb)?);
                let (c1, c2) = (self.rz(rs) & m, self.rz((rs + 1) & 31) & m);
                if o1 == c1 && o2 == c2 {
                    let (n1, n2) = (self.rz(rt), self.rz((rt + 1) & 31));
                    self.wr(bus, addr, eb, n1)?;
                    self.wr(bus, addr.wrapping_add(eb as u64), eb, n2)?;
                }
                self.wz(rs, o1, wide);
                self.wz((rs + 1) & 31, o2, wide);
            }
            (1, 0, 1) => {
                // LDAR / LDLAR
                let v = self.rd(bus, addr, n)?;
                self.wz(rt, v, true);
            }
            (1, 0, 0) => self.wr(bus, addr, n, self.rz(rt))?, // STLR / STLLR
            (1, 1, _) if rt2 == 31 => {
                // CAS / CASA / CASL / CASAL
                let old = self.rd(bus, addr, n)?;
                if old == self.rz(rs) & mask {
                    self.wr(bus, addr, n, self.rz(rt))?;
                }
                self.wz(rs, old, true);
            }
            _ => return Err(self.undef(i)),
        }
        Ok(())
    }

    /// LSE: LDADD LDCLR LDEOR LDSET LDSMAX LDSMIN LDUMAX LDUMIN SWP (all ordering
    /// variants; ST* aliases have Rt = XZR), plus LDAPR.
    fn atomic(&mut self, bus: &mut Bus, i: u32) -> Result<(), Ex> {
        let n = 1usize << (i >> 30);
        let rs = ((i >> 16) & 31) as usize;
        let rt = (i & 31) as usize;
        let (o3, opc) = ((i >> 15) & 1, (i >> 12) & 7);
        let addr = self.rs(((i >> 5) & 31) as usize);
        if o3 == 1 && opc == 4 {
            let v = self.rd(bus, addr, n)?; // LDAPR
            self.wz(rt, v, true);
            return Ok(());
        }
        if o3 == 1 && opc != 0 {
            return Err(self.undef(i));
        }
        let bits = 8 * n as u32;
        let mask = if n == 8 { !0 } else { (1u64 << bits) - 1 };
        let old = self.rd(bus, addr, n)?;
        let src = self.rz(rs) & mask;
        let sx = |v: u64| sext(v, bits) as i64;
        let new = if o3 == 1 {
            src // SWP
        } else {
            match opc {
                0 => old.wrapping_add(src),
                1 => old & !src,
                2 => old ^ src,
                3 => old | src,
                4 => if sx(old) >= sx(src) { old } else { src },
                5 => if sx(old) <= sx(src) { old } else { src },
                6 => old.max(src),
                _ => old.min(src),
            }
        };
        self.wr(bus, addr, n, new & mask)?;
        self.wz(rt, old, true);
        Ok(())
    }

    #[inline(never)]
    fn exec(&mut self, bus: &mut Bus) -> Result<(), Ex> {
        let pc = self.pc;
        let i = self.fetch(bus)?;
        self.last_insn = i;
        let mut next = pc.wrapping_add(4);

        let sf = i >> 31 == 1;
        let rd = (i & 31) as usize;
        let rn = ((i >> 5) & 31) as usize;
        let rm = ((i >> 16) & 31) as usize;

        let class = (i >> 25) & 0xf;
        if class >> 1 == 0b100 {
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
            let (wmask, _) = self.cached_bitmasks(n, (i >> 10) & 0x3f, (i >> 16) & 0x3f, true, if sf { 64 } else { 32 })
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
            let (wmask, tmask) = self.cached_bitmasks(n, imms, immr, false, ds).ok_or_else(|| self.undef(i))?;
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
        } else {
            return Err(self.undef(i));
        }
        } else if class >> 1 == 0b101 {
        if i >> 26 == 0b000101 || i >> 26 == 0b100101 {
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
        } else if i & 0xffff_f0ff == 0xd503_305f {
            self.excl = None; // CLREX
        } else if i == 0xd503_207f {
            // ---- WFI: sleep until the next interrupt or timer deadline
            if !bus.gic.irq_line[self.id] {
                // the scheduler skips this CPU until an interrupt (or its timer) wakes it
                bus.ctl[self.id].waiting = true;
                self.recheck = true;
            }
        } else if i & 0xfffff01f == 0xd503201f || i & 0xfffff01f == 0xd503301f {
            // ---- NOP / hints (WFE, YIELD...) / barriers; WFE and YIELD hand the board to the next CPU
            if i == 0xd503_205f || i == 0xd503_203f {
                self.recheck = true;
            }
        } else if i & 0xfff8f01f == 0xd500401f {
            // ---- MSR (immediate): DAIFSet / DAIFClr / SPSel; others (PAN, UAO...) ignored
            self.recheck = true;
            let (op1, op2, crm) = ((i >> 16) & 7, (i >> 5) & 7, (i >> 8) & 15);
            match (op1, op2) {
                (3, 6) | (3, 7) | (0, 5) if self.el == 0 => return Err(self.undef(i)),
                (3, 6) => self.daif |= crm,
                (3, 7) => self.daif &= !crm,
                (0, 5) => self.set_mode(self.el, crm & 1 == 1),
                _ => {}
            }
        } else if i & 0xfff8_0000 == 0xd508_0000 {
            self.sys_insn(bus, i)?;
        } else if i & 0xffd0_0000 == 0xd510_0000 {
            // ---- MRS / MSR (register)
            let key = (i >> 5) & 0x7fff;
            if self.el == 0 && (key >> 11) & 7 != 3 {
                return Err(self.undef(i)); // only op1 = 3 (EL0-accessible) registers
            }
            if (i >> 21) & 1 == 1 {
                let v = self.sysreg_read(bus, key);
                self.wz(rd, v, true);
            } else {
                let v = self.rz(rd);
                self.sysreg_write(bus, key, v);
            }
        } else if i == 0xd69f_03e0 {
            // ---- ERET
            if self.el == 0 {
                return Err(self.undef(i));
            }
            let spsr = self.sysreg(SYS_SPSR_EL1);
            self.set_flags((spsr >> 31 & 1 == 1, spsr >> 30 & 1 == 1, spsr >> 29 & 1 == 1, spsr >> 28 & 1 == 1));
            self.daif = (spsr >> 6) as u32 & 0xf;
            self.set_mode(((spsr >> 2) & 3).min(1) as u32, spsr & 1 == 1);
            self.excl = None;
            self.recheck = true;
            next = self.sysreg(SYS_ELR_EL1);
        } else if i & 0xffe0_001f == 0xd400_0001 {
            // ---- SVC: exception to EL1, or a Linux-style host syscall (x8) while VBAR_EL1 = 0
            if self.sysreg(SYS_VBAR_EL1) != 0 {
                return Err(Ex::sync(EC_SVC64, (i >> 5) & 0xffff, None, true));
            }
            match host::syscall(bus, self.x[8], [self.x[0], self.x[1], self.x[2]]) {
                Host::Exit(c) => return Err(Stop::Halt(c).into()),
                Host::Ret(v) => self.x[0] = v,
            }
        } else if i & 0xffe0_001f == 0xd400_0002 || i & 0xffe0_001f == 0xd400_0003 {
            // ---- HVC / SMC: PSCI
            if self.el == 0 {
                return Err(self.undef(i));
            }
            const NOT_SUPPORTED: u64 = -1i64 as u64;
            const INVALID_PARAMS: u64 = -2i64 as u64;
            const ALREADY_ON: u64 = -4i64 as u64;
            const ON_PENDING: u64 = -5i64 as u64;
            let (a1, a2, a3) = (self.x[1], self.x[2], self.x[3]);
            self.x[0] = match self.x[0] as u32 {
                0x8400_0008 => return Err(Stop::Halt(0).into()), // SYSTEM_OFF
                0x8400_0009 => return Err(Stop::Reset.into()),   // SYSTEM_RESET
                0x8400_0000 => 0x0001_0001,                      // PSCI_VERSION 1.1
                0x8400_0002 => {
                    // CPU_OFF: this CPU powers down and never returns
                    bus.ctl[self.id].parked = true;
                    self.recheck = true;
                    0
                }
                0x8400_0003 | 0xc400_0003 => {
                    // CPU_ON(target_cpu, entry_point, context_id)
                    let t = (a1 & 0xff) as usize;
                    if a1 & !0xff != 0 && a1 & !0xff != 0x8000_0000 || t >= bus.ncpus {
                        INVALID_PARAMS
                    } else if bus.ctl[t].start.is_some() {
                        ON_PENDING
                    } else if !bus.ctl[t].parked {
                        ALREADY_ON
                    } else {
                        bus.ctl[t].start = Some((a2, a3));
                        0
                    }
                }
                0x8400_0004 | 0xc400_0004 => {
                    // AFFINITY_INFO: 0 = ON, 1 = OFF, 2 = ON_PENDING
                    let t = (a1 & 0xff) as usize;
                    if t >= bus.ncpus {
                        INVALID_PARAMS
                    } else if bus.ctl[t].start.is_some() {
                        2
                    } else {
                        bus.ctl[t].parked as u64
                    }
                }
                0x8400_0006 => 2, // MIGRATE_INFO_TYPE: trusted OS not present / not migratable
                0x8400_000a => match a1 as u32 {
                    // PSCI_FEATURES
                    0x8400_0000 | 0x8400_0002 | 0x8400_0003 | 0xc400_0003 | 0x8400_0004 | 0xc400_0004 | 0x8400_0006
                    | 0x8400_0008 | 0x8400_0009 | 0x8400_000a => 0,
                    _ => NOT_SUPPORTED,
                },
                _ => NOT_SUPPORTED,
            };
        } else if i & 0xffe0_001f == 0xd440_0000 {
            // ---- HLT: stop the VM, exit code = x0
            return Err(Stop::Halt(self.x[0] as i32).into());
        } else if i & 0xffe0_001f == 0xd420_0000 {
            // ---- BRK
            return Err(Ex::sync(EC_BRK, (i >> 5) & 0xffff, None, false));
        } else {
            return Err(self.undef(i));
        }
        } else if class & 0b0101 == 0b0100 {
        if i & 0x3f20_0c00 == 0x3820_0000 {
            // ---- LSE atomics: LDADD/LDCLR/LDEOR/LDSET/LD{S,U}{MAX,MIN}, SWP, LDAPR
            self.atomic(bus, i)?;
        } else if i & 0x3a00_0000 == 0x3800_0000 {
            // ---- LDR/STR family (single register, integer or SIMD&FP)
            let fp = (i >> 26) & 1 == 1;
            // SIMD&FP 128-bit accesses are scaled by 16
            let size = if fp && (i >> 23) & 1 == 1 && i >> 30 == 0 { 4 } else { i >> 30 };
            let base = self.rs(rn);
            if (i >> 24) & 1 == 1 {
                let addr = base.wrapping_add((((i >> 10) & 0xfff) as u64) << size);
                if fp { self.fp_ldst(bus, i, addr)? } else { self.ldst(bus, i, addr)? };
            } else if (i >> 21) & 1 == 0 {
                let imm9 = sext(((i >> 12) & 0x1ff) as u64, 9);
                let (addr, wb) = match (i >> 10) & 3 {
                    0 => (base.wrapping_add(imm9), None),
                    1 => (base, Some(base.wrapping_add(imm9))),
                    3 => (base.wrapping_add(imm9), Some(base.wrapping_add(imm9))),
                    _ => (base.wrapping_add(imm9), None), // LDTR / STTR: EL0 permissions
                };
                self.unpriv = (i >> 10) & 3 == 2;
                let r = if fp { self.fp_ldst(bus, i, addr) } else { self.ldst(bus, i, addr) };
                self.unpriv = false;
                r?;
                if let Some(w) = wb {
                    self.ws(rn, w, true);
                }
            } else if (i >> 10) & 3 == 2 {
                let mut off = extend(self.rz(rm), (i >> 13) & 7);
                if (i >> 12) & 1 == 1 {
                    off <<= size;
                }
                let addr = base.wrapping_add(off);
                if fp { self.fp_ldst(bus, i, addr)? } else { self.ldst(bus, i, addr)? };
            } else {
                return Err(self.undef(i));
            }
        } else if i & 0x3f00_0000 == 0x0800_0000 {
            // ---- load/store exclusive, acquire/release, CAS
            self.exclusive(bus, i)?;
        } else if i & 0x3e00_0000 == 0x2c00_0000 {
            self.fp_pair(bus, i)?; // LDP / STP (SIMD&FP)
        } else if i & 0xbf00_0000 == 0x0c00_0000 || i & 0xbf00_0000 == 0x0d00_0000 {
            self.simd_structure(bus, i)?; // LD1-LD4 / ST1-ST4
        } else if i & 0x3f00_0000 == 0x1c00_0000 {
            self.fp_literal(bus, i)?; // LDR (literal), SIMD&FP
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
                0 | 2 => (base.wrapping_add(off), None), // 0 = STNP / LDNP (non-temporal hint ignored)
                _ => (base.wrapping_add(off), Some(base.wrapping_add(off))),
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
        } else {
            return Err(self.undef(i));
        }
        } else if class & 7 == 0b101 {
        if (i >> 24) & 0x1f == 0b01011 {
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
                    16..=23 => self.crc32_insn(i, sf)?, // CRC32 / CRC32C
                    _ => return Err(self.undef(i)),
                };
                self.wz(rd, res, sf);
            } else {
                // ---- RBIT / REV / CLZ
                let res = match (opcode, sf) {
                    (0, true) => a.reverse_bits(),
                    (0, false) => (a as u32).reverse_bits() as u64,
                    // REV16: swap the bytes of each halfword
                    (1, true) => ((a & 0x00ff_00ff_00ff_00ff) << 8) | ((a >> 8) & 0x00ff_00ff_00ff_00ff),
                    (1, false) => ((a as u32 & 0x00ff_00ff) << 8 | (a as u32 >> 8) & 0x00ff_00ff) as u64,
                    (2, false) => (a as u32).swap_bytes() as u64,
                    // REV32 (64-bit form): swap bytes within each word
                    (2, true) => ((a as u32).swap_bytes() as u64) | (((a >> 32) as u32).swap_bytes() as u64) << 32,
                    (3, true) => a.swap_bytes(),
                    (4, true) => a.leading_zeros() as u64,
                    (4, false) => (a as u32).leading_zeros() as u64,
                    // CLS: leading bits equal to the sign bit (not counting the sign bit itself)
                    (5, true) => ((a ^ (a >> 1)).leading_zeros() as u64).saturating_sub(1).min(63),
                    (5, false) => (((a as u32) ^ ((a as u32) >> 1)).leading_zeros() as u64).saturating_sub(1).min(31),
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
        } else {
            return Err(self.undef(i));
        }
        } else if class & 7 == 7 {
            self.fpsimd(bus, i)?; // scalar FP and Advanced SIMD data processing
        } else {
            return Err(self.undef(i));
        }

        self.pc = next;
        Ok(())
    }
}

impl Default for Aarch64 {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Cpu for Aarch64 {
    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }
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
        if self.pre(bus) {
            return Ok(());
        }
        match self.exec(bus) {
            Ok(()) => Ok(()),
            Err(e) => self.handle_ex(bus, e),
        }
    }

    /// Batched execution: the per-instruction bookkeeping (board clock, timers, interrupt
    /// line, halt flag) is done once per batch of up to `BATCH` instructions.
    fn wake_deadline(&self, _bus: &Bus) -> u64 {
        self.timer_deadline
    }

    fn run(&mut self, bus: &mut Bus, n: u64) -> (u64, Option<Stop>) {
        const BATCH: u64 = 32;
        bus.cur_cpu = self.id;
        self.service_ctl(bus);
        if bus.ctl[self.id].parked {
            return (0, None);
        }
        if bus.ctl[self.id].waiting {
            if bus.clint.mtime >= self.timer_deadline {
                self.update_timers(bus);
            }
            if !bus.gic.irq_line[self.id] {
                return (0, None); // still asleep
            }
            bus.ctl[self.id].waiting = false;
        }
        let mut done = 0;
        while done < n {
            let k = (n - done).min(BATCH);
            bus.clint.mtime += bus.ticks(k);
            if self.pre(bus) {
                done += 1;
                continue;
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
            bus.clint.mtime -= bus.ticks(k) - bus.ticks(j); // give back the unused part of the batch
            done += j;
            if let Some(code) = bus.halt {
                return (done, Some(Stop::Halt(code)));
            }
            if bus.reset || bus.ctl[self.id].waiting || bus.ctl[self.id].parked {
                break;
            }
        }
        // never hand the board to another CPU in the middle of an exclusive sequence
        let mut extra = 0;
        while self.excl.is_some() && extra < 64 {
            extra += 1;
            if let Err(e) = self.exec(bus) {
                if let Err(s) = self.handle_ex(bus, e) {
                    return (done + extra, Some(s));
                }
            }
        }
        (done + extra, None)
    }
    fn dump(&self) -> String {
        let mut s = format!("pc  = {:#018x}\n", self.pc);
        for r in 0..31 {
            s += &format!(" x{:<2} = {:#018x}", r, self.x[r]);
            s += if r % 2 == 1 { "\n" } else { "  " };
        }
        s += &format!("\n  sp = {:#018x}\n", self.sp);
        s += &format!(
            "nzcv = {}{}{}{}  EL{}  SPSel={}  DAIF={:#x}\n",
            self.n as u8, self.z as u8, self.c as u8, self.v as u8, self.el, self.spsel as u8, self.daif
        );
        s += &format!(
            "ELR_EL1={:#x} SPSR_EL1={:#x} ESR_EL1={:#x} FAR_EL1={:#x} VBAR_EL1={:#x}\n",
            self.sysreg(SYS_ELR_EL1), self.sysreg(SYS_SPSR_EL1), self.sysreg(SYS_ESR_EL1),
            self.sysreg(SYS_FAR_EL1), self.sysreg(SYS_VBAR_EL1)
        );
        s
    }
}
