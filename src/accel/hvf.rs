//! Apple Hypervisor.framework backend (macOS on Apple silicon).
//!
//! The process must be signed with the `com.apple.security.hypervisor` entitlement
//! (`tools/hvf-sign.sh`). Exits handled here: data aborts on MMIO (decoded from the ESR
//! syndrome), HVC/SMC (PSCI), WFI (sleep until an interrupt or the virtual timer), trapped
//! system registers (read as zero, writes ignored) and the virtual timer, which is wired to
//! PPI 27 of ruvm's GIC.

use super::{run_threads, Shared, VTIMER_IRQ};
use crate::bus::Bus;
use crate::machine::Stop;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[repr(C)]
struct Exit {
    reason: u32,
    syndrome: u64,
    virtual_address: u64,
    physical_address: u64,
}

#[link(name = "Hypervisor", kind = "framework")]
extern "C" {
    fn hv_vm_create(config: *mut c_void) -> i32;
    fn hv_vm_destroy() -> i32;
    fn hv_vm_map(addr: *mut c_void, ipa: u64, size: usize, flags: u64) -> i32;
    fn hv_vcpu_create(vcpu: *mut u64, exit: *mut *const Exit, config: *mut c_void) -> i32;
    fn hv_vcpu_destroy(vcpu: u64) -> i32;
    fn hv_vcpu_get_reg(vcpu: u64, reg: u32, value: *mut u64) -> i32;
    fn hv_vcpu_set_reg(vcpu: u64, reg: u32, value: u64) -> i32;
    fn hv_vcpu_get_sys_reg(vcpu: u64, reg: u16, value: *mut u64) -> i32;
    fn hv_vcpu_set_sys_reg(vcpu: u64, reg: u16, value: u64) -> i32;
    fn hv_vcpu_set_pending_interrupt(vcpu: u64, kind: u32, pending: bool) -> i32;
    fn hv_vcpu_run(vcpu: u64) -> i32;
    fn hv_vcpus_exit(vcpus: *const u64, count: u32) -> i32;
    fn hv_vcpu_set_vtimer_mask(vcpu: u64, masked: bool) -> i32;
    fn hv_vcpu_set_vtimer_offset(vcpu: u64, offset: u64) -> i32;
}

const HV_DENIED: i32 = 0xfae9_4007_u32 as i32;
const MEMORY_RWX: u64 = 0b111;
const INTERRUPT_IRQ: u32 = 0;

const EXIT_CANCELED: u32 = 0;
const EXIT_EXCEPTION: u32 = 1;
const EXIT_VTIMER: u32 = 2;

// hv_reg_t: X0..X30 = 0..30, then
const REG_PC: u32 = 31;
const REG_CPSR: u32 = 34;
// hv_sys_reg_t: the op0:op1:CRn:CRm:op2 encoding
const MPIDR_EL1: u16 = 0xc005;
const SCTLR_EL1: u16 = 0xc080;
const CNTV_CTL_EL0: u16 = 0xdf19;
const CNTV_CVAL_EL0: u16 = 0xdf1a;

/// EL1h with D, A, I and F masked: the state a kernel is entered in.
const CPSR_EL1H: u64 = 0x3c5;

/// Host counter value at which the guest's virtual counter reads 0.
static VTIMER_OFFSET: AtomicU64 = AtomicU64::new(0);

extern "C" {
    fn mach_absolute_time() -> u64;
}

/// The host counter the virtual timer offset is relative to.
fn counter() -> u64 {
    // SAFETY: no arguments, no side effects.
    unsafe { mach_absolute_time() }
}

fn counter_freq() -> u64 {
    let v: u64;
    // SAFETY: CNTFRQ_EL0 is readable at EL0.
    unsafe { std::arch::asm!("mrs {}, cntfrq_el0", out(reg) v) };
    v
}

fn check(r: i32, what: &str) -> Result<(), String> {
    if r == 0 {
        Ok(())
    } else {
        Err(format!("{what} failed ({:#x})", r as u32))
    }
}

pub fn run(mut bus: Bus, entry: (u64, u64), trace: bool) -> (Stop, Bus) {
    // SAFETY: plain FFI calls; RAM stays allocated (owned by `bus`) until after hv_vm_destroy.
    let r = unsafe { hv_vm_create(null_mut()) };
    if r == HV_DENIED {
        return (Stop::Fatal("Hypervisor.framework denied access: sign ruvm with the com.apple.security.hypervisor entitlement (tools/hvf-sign.sh)".into()), bus);
    }
    if let Err(e) = check(r, "hv_vm_create") {
        return (Stop::Fatal(e), bus);
    }
    let (base, size) = (bus.ram_base(), bus.ram_size() as usize);
    if let Err(e) = check(unsafe { hv_vm_map(bus.ram_ptr().cast(), base, size, MEMORY_RWX) }, "hv_vm_map") {
        unsafe { hv_vm_destroy() };
        return (Stop::Fatal(e), bus);
    }
    VTIMER_OFFSET.store(counter(), Ordering::Relaxed);
    let out = run_threads(Shared::new(bus, kick, None, trace), entry, vcpu_thread);
    unsafe { hv_vm_destroy() };
    out
}

fn kick(v: u64) {
    // SAFETY: `v` is a live vCPU (handles are withdrawn before the vCPU is destroyed).
    unsafe { hv_vcpus_exit(&v, 1) };
}

/// A vCPU owned by the current thread.
struct Vcpu {
    v: u64,
    exit: *const Exit,
}

impl Vcpu {
    fn reg(&self, r: u32) -> u64 {
        let mut x = 0;
        unsafe { hv_vcpu_get_reg(self.v, r, &mut x) };
        x
    }
    fn set_reg(&self, r: u32, x: u64) {
        unsafe { hv_vcpu_set_reg(self.v, r, x) };
    }
    /// General register `n`, where 31 is the zero register.
    fn x(&self, n: u32) -> u64 {
        if n == 31 { 0 } else { self.reg(n) }
    }
    fn set_x(&self, n: u32, x: u64) {
        if n != 31 {
            self.set_reg(n, x);
        }
    }
    fn sys(&self, r: u16) -> u64 {
        let mut x = 0;
        unsafe { hv_vcpu_get_sys_reg(self.v, r, &mut x) };
        x
    }
    fn set_sys(&self, r: u16, x: u64) {
        unsafe { hv_vcpu_set_sys_reg(self.v, r, x) };
    }
    fn advance(&self) {
        self.set_reg(REG_PC, self.reg(REG_PC) + 4);
    }
}

fn vcpu_thread(sh: &Shared, id: usize, boot: Option<(u64, u64)>) -> Result<(), String> {
    let mut v = 0;
    let mut exit = null();
    // SAFETY: the vCPU belongs to this thread and is destroyed below, on this thread.
    check(unsafe { hv_vcpu_create(&mut v, &mut exit, null_mut()) }, "hv_vcpu_create")?;
    let cpu = Vcpu { v, exit };
    sh.vcpus[id].store(v, Ordering::Release);
    let r = vcpu_loop(sh, id, &cpu, boot);
    sh.vcpus[id].store(super::NO_VCPU, Ordering::Release);
    unsafe { hv_vcpu_destroy(v) };
    r
}

fn vcpu_loop(sh: &Shared, id: usize, cpu: &Vcpu, boot: Option<(u64, u64)>) -> Result<(), String> {
    cpu.set_sys(MPIDR_EL1, 0x8000_0000 | id as u64);
    check(unsafe { hv_vcpu_set_vtimer_offset(cpu.v, VTIMER_OFFSET.load(Ordering::Relaxed)) }, "hv_vcpu_set_vtimer_offset")?;
    let sctlr = cpu.sys(SCTLR_EL1);
    let mut start = boot;
    loop {
        let Some((pc, ctx)) = start.take().or_else(|| sh.wait_start(id)) else { return Ok(()) };
        // power-on state (also after CPU_OFF): MMU and caches off, interrupts masked
        cpu.set_sys(SCTLR_EL1, sctlr);
        cpu.set_reg(REG_CPSR, CPSR_EL1H);
        cpu.set_reg(REG_PC, pc);
        for n in 0..31 {
            cpu.set_reg(n, 0);
        }
        cpu.set_reg(0, ctx);
        if !run_on(sh, id, cpu)? {
            return Ok(());
        }
    }
}

/// Run the guest on this CPU until it powers itself off (`true`) or the machine stops (`false`).
fn run_on(sh: &Shared, id: usize, cpu: &Vcpu) -> Result<bool, String> {
    let mut vtimer_masked = false;
    loop {
        if sh.stopping() {
            return Ok(false);
        }
        let irq = {
            let mut bus = sh.lock();
            if vtimer_masked {
                // the timer interrupt stays pending until the guest re-arms or masks the timer
                let level = cpu.sys(CNTV_CTL_EL0) & 7 == 5; // ENABLE and ISTATUS, not IMASK
                bus.gic.set_ppi(id, VTIMER_IRQ, level);
                if !level {
                    unsafe { hv_vcpu_set_vtimer_mask(cpu.v, false) };
                    vtimer_masked = false;
                }
            }
            let line = bus.gic.irq_line[id];
            sh.given[id].store(line, Ordering::Release);
            line
        };
        unsafe { hv_vcpu_set_pending_interrupt(cpu.v, INTERRUPT_IRQ, irq) };
        check(unsafe { hv_vcpu_run(cpu.v) }, "hv_vcpu_run")?;
        // SAFETY: the exit record of this vCPU, valid while it lives.
        let e = unsafe { &*cpu.exit };
        match e.reason {
            EXIT_CANCELED => {}
            EXIT_VTIMER => {
                vtimer_masked = true;
                sh.lock().gic.set_ppi(id, VTIMER_IRQ, true);
            }
            EXIT_EXCEPTION => {
                if exception(sh, id, cpu, e)? {
                    return Ok(true);
                }
            }
            r => return Err(format!("unexpected exit reason {r}")),
        }
    }
}

/// Handle a synchronous exception from the guest; `true` if the CPU powered itself off.
fn exception(sh: &Shared, id: usize, cpu: &Vcpu, e: &Exit) -> Result<bool, String> {
    let esr = e.syndrome;
    match esr >> 26 & 0x3f {
        0x01 => {
            // WFI (WFE just yields)
            cpu.advance();
            if esr & 1 == 0 {
                wfi(sh, id, cpu);
            }
        }
        ec @ (0x16 | 0x17) => {
            // HVC / SMC: PSCI (the return address of HVC is already past the instruction)
            let (x0, off) = sh.psci(id, [cpu.x(0), cpu.x(1), cpu.x(2), cpu.x(3)]);
            cpu.set_reg(0, x0);
            if ec == 0x17 {
                cpu.advance();
            }
            return Ok(off);
        }
        0x18 => {
            // trapped system register (debug, PMU, ...): reads as zero, writes are ignored
            let (rt, read) = ((esr >> 5 & 31) as u32, esr & 1 == 1);
            if sh.trace {
                eprintln!("[hvf] cpu{id}: system register {} (op0 {} op1 {} CRn {} CRm {} op2 {})", if read { "read" } else { "write" }, esr >> 20 & 3, esr >> 14 & 7, esr >> 10 & 15, esr >> 1 & 15, esr >> 17 & 7);
            }
            if read {
                cpu.set_x(rt, 0);
            }
            cpu.advance();
        }
        0x24 => {
            // data abort outside RAM: a device register access
            if esr >> 24 & 1 == 0 {
                return Err(format!("data abort at {:#x} without instruction syndrome (pc {:#x})", e.physical_address, cpu.reg(REG_PC)));
            }
            let size = 1usize << (esr >> 22 & 3);
            let rt = (esr >> 16 & 31) as u32;
            if esr >> 6 & 1 == 1 {
                let mask = if size == 8 { u64::MAX } else { (1u64 << (8 * size)) - 1 };
                sh.mmio(id, e.physical_address, size, Some(cpu.x(rt) & mask));
            } else {
                let mut v = sh.mmio(id, e.physical_address, size, None);
                if esr >> 21 & 1 == 1 {
                    v = crate::cpu::sext(v, 8 * size as u32);
                    if esr >> 15 & 1 == 0 {
                        v &= 0xffff_ffff;
                    }
                }
                cpu.set_x(rt, v);
            }
            cpu.advance();
        }
        ec => {
            return Err(format!("unexpected exception class {ec:#x} (ESR {esr:#x}) at pc {:#x}, address {:#x}", cpu.reg(REG_PC), e.virtual_address));
        }
    }
    Ok(false)
}

/// WFI: sleep until an interrupt is pending or the virtual timer's deadline.
fn wfi(sh: &Shared, id: usize, cpu: &Vcpu) {
    let ctl = cpu.sys(CNTV_CTL_EL0);
    let deadline = if ctl & 3 == 1 {
        let now = counter().wrapping_sub(VTIMER_OFFSET.load(Ordering::Relaxed));
        let cval = cpu.sys(CNTV_CVAL_EL0);
        if cval <= now {
            return;
        }
        let ns = ((cval - now) as u128 * 1_000_000_000 / counter_freq() as u128).min(u64::MAX as u128) as u64;
        Some(Instant::now() + Duration::from_nanos(ns))
    } else {
        None
    };
    sh.idle(id, deadline);
}
