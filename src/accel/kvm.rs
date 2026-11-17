//! Linux KVM backend (AArch64 hosts).
//!
//! The VM has no in-kernel interrupt controller: ruvm's GICv2 model drives each vCPU's IRQ
//! line with KVM_IRQ_LINE, and KVM reports the virtual timer's output in `kvm_run`
//! (KVM_CAP_ARM_USER_IRQ), which is wired to PPI 27. PSCI and WFI are handled by KVM itself:
//! secondary vCPUs start powered off and wait inside KVM_RUN for CPU_ON, and SYSTEM_OFF /
//! SYSTEM_RESET come back as system events.

use super::{run_threads, Shared, VTIMER_IRQ};
use crate::bus::Bus;
use crate::machine::Stop;
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

extern "C" {
    fn open(path: *const u8, flags: i32, ...) -> i32;
    fn close(fd: i32) -> i32;
    fn ioctl(fd: i32, req: u64, ...) -> i32;
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    fn pthread_self() -> u64;
    fn pthread_kill(thread: u64, sig: i32) -> i32;
}

const O_RDWR: i32 = 2;
const O_CLOEXEC: i32 = 0o2000000;
const PROT_RW: i32 = 3;
const MAP_SHARED: i32 = 1;
const SIGUSR1: i32 = 10;
const EINTR: i32 = 4;

// ioctl numbers (KVMIO = 0xae)
const KVM_GET_API_VERSION: u64 = 0xae00;
const KVM_CREATE_VM: u64 = 0xae01;
const KVM_CHECK_EXTENSION: u64 = 0xae03;
const KVM_GET_VCPU_MMAP_SIZE: u64 = 0xae04;
const KVM_CREATE_VCPU: u64 = 0xae41;
const KVM_SET_USER_MEMORY_REGION: u64 = 0x4020_ae46;
const KVM_IRQ_LINE: u64 = 0x4008_ae61;
const KVM_RUN: u64 = 0xae80;
const KVM_SET_ONE_REG: u64 = 0x4010_aeac;
const KVM_ARM_VCPU_INIT: u64 = 0x4020_aeae;
const KVM_ARM_PREFERRED_TARGET: u64 = 0x8020_aeaf;

const KVM_CAP_ARM_PSCI_0_2: u64 = 102;
const KVM_CAP_ARM_USER_IRQ: u64 = 144;

// kvm_vcpu_init.features[0]
const VCPU_POWER_OFF: u32 = 1 << 0;
const VCPU_PSCI_0_2: u32 = 1 << 2;

// core registers: KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE | (offset in kvm_regs / 4)
const REG_CORE: u64 = 0x6030_0000_0010_0000;
const REG_X0: u64 = REG_CORE;
const REG_PC: u64 = REG_CORE | (32 * 8 / 4);

// exit reasons
const EXIT_MMIO: u32 = 6;
const EXIT_INTR: u32 = 10;
const EXIT_SYSTEM_EVENT: u32 = 24;
const SYSTEM_EVENT_SHUTDOWN: u32 = 1;
const SYSTEM_EVENT_RESET: u32 = 2;

// struct kvm_run (fields used here)
const RUN_IMMEDIATE_EXIT: usize = 1;
const RUN_EXIT_REASON: usize = 8;
const RUN_EXIT: usize = 32; // the exit-specific union
const RUN_DEVICE_IRQ_LEVEL: usize = 304; // s.regs.device_irq_level
const DEV_EL1_VTIMER: u64 = 1;

/// The VM file descriptor (for KVM_IRQ_LINE from any thread).
static VM_FD: AtomicI32 = AtomicI32::new(-1);
/// Thread and `kvm_run` page of each vCPU, for kicking it out of KVM_RUN.
static VCPUS: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());

fn os_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

fn kvm_ioctl(fd: i32, req: u64, arg: u64, what: &str) -> Result<i32, String> {
    // SAFETY: each caller passes the argument its request expects.
    let r = unsafe { ioctl(fd, req, arg) };
    if r < 0 { Err(os_error(what)) } else { Ok(r) }
}

pub fn run(mut bus: Bus, entry: (u64, u64), trace: bool) -> (Stop, Bus) {
    match create_vm(&mut bus) {
        Ok(vm) => {
            VM_FD.store(vm, Ordering::Release);
            // SAFETY: an empty handler: the signal only interrupts KVM_RUN
            unsafe { signal(SIGUSR1, on_kick) };
            let ncpus = bus.ncpus;
            *VCPUS.lock().unwrap() = vec![(0, 0); ncpus];
            let out = run_threads(Shared::new(bus, kick, Some(set_line), trace), entry, vcpu_thread);
            VM_FD.store(-1, Ordering::Release);
            unsafe { close(vm) };
            out
        }
        Err(e) => (Stop::Fatal(e), bus),
    }
}

fn create_vm(bus: &mut Bus) -> Result<i32, String> {
    let kvm = unsafe { open(c"/dev/kvm".as_ptr().cast(), O_RDWR | O_CLOEXEC) };
    if kvm < 0 {
        return Err(os_error("cannot open /dev/kvm"));
    }
    let r = (|| {
        if kvm_ioctl(kvm, KVM_GET_API_VERSION, 0, "KVM_GET_API_VERSION")? != 12 {
            return Err("unsupported KVM API version".into());
        }
        for (cap, name) in [(KVM_CAP_ARM_USER_IRQ, "KVM_CAP_ARM_USER_IRQ"), (KVM_CAP_ARM_PSCI_0_2, "KVM_CAP_ARM_PSCI_0_2")] {
            if kvm_ioctl(kvm, KVM_CHECK_EXTENSION, cap, "KVM_CHECK_EXTENSION")? <= 0 {
                return Err(format!("KVM lacks {name}"));
            }
        }
        let vm = kvm_ioctl(kvm, KVM_CREATE_VM, 0, "KVM_CREATE_VM")?;
        // struct kvm_userspace_memory_region { slot, flags, guest_phys_addr, memory_size, userspace_addr }
        let region: [u64; 4] = [0, bus.ram_base(), bus.ram_size(), bus.ram_ptr() as u64];
        if let Err(e) = kvm_ioctl(vm, KVM_SET_USER_MEMORY_REGION, region.as_ptr() as u64, "KVM_SET_USER_MEMORY_REGION") {
            unsafe { close(vm) };
            return Err(e);
        }
        Ok(vm)
    })();
    unsafe { close(kvm) };
    r
}

extern "C" fn on_kick(_: i32) {}

fn kick(h: u64) {
    if let Some(&(thread, run)) = VCPUS.lock().unwrap().get(h as usize) {
        if run != 0 {
            // SAFETY: the kvm_run page stays mapped while the vCPU is registered
            unsafe { std::ptr::write_volatile((run + RUN_IMMEDIATE_EXIT) as *mut u8, 1) };
            unsafe { pthread_kill(thread, SIGUSR1) };
        }
    }
}

fn set_line(cpu: usize, level: bool) {
    // struct kvm_irq_level: irq = KVM_ARM_IRQ_TYPE_CPU (0) << 24 | vcpu << 16 | KVM_ARM_IRQ_CPU_IRQ (0)
    let irq: [u32; 2] = [(cpu as u32) << 16, level as u32];
    let vm = VM_FD.load(Ordering::Acquire);
    if vm >= 0 {
        unsafe { ioctl(vm, KVM_IRQ_LINE, irq.as_ptr()) };
    }
}

/// A vCPU file descriptor and its mapped `kvm_run`.
struct Vcpu {
    fd: i32,
    run: *mut u8,
    size: usize,
}

impl Drop for Vcpu {
    fn drop(&mut self) {
        unsafe {
            munmap(self.run.cast(), self.size);
            close(self.fd);
        }
    }
}

impl Vcpu {
    fn set_reg(&self, id: u64, v: u64) -> Result<(), String> {
        let reg: [u64; 2] = [id, &v as *const u64 as u64];
        kvm_ioctl(self.fd, KVM_SET_ONE_REG, reg.as_ptr() as u64, "KVM_SET_ONE_REG").map(|_| ())
    }
    fn at<T: Copy>(&self, off: usize) -> T {
        // SAFETY: offsets inside the kvm_run page
        unsafe { std::ptr::read_volatile(self.run.add(off).cast()) }
    }
    fn put<T>(&self, off: usize, v: T) {
        unsafe { std::ptr::write_volatile(self.run.add(off).cast(), v) }
    }
}

fn vcpu_thread(sh: &Shared, id: usize, boot: Option<(u64, u64)>) -> Result<(), String> {
    let vm = VM_FD.load(Ordering::Acquire);
    let fd = kvm_ioctl(vm, KVM_CREATE_VCPU, id as u64, "KVM_CREATE_VCPU")?;
    let size = {
        let kvm = unsafe { open(c"/dev/kvm".as_ptr().cast(), O_RDWR | O_CLOEXEC) };
        let r = kvm_ioctl(kvm, KVM_GET_VCPU_MMAP_SIZE, 0, "KVM_GET_VCPU_MMAP_SIZE");
        unsafe { close(kvm) };
        r? as usize
    };
    let p = unsafe { mmap(std::ptr::null_mut(), size, PROT_RW, MAP_SHARED, fd, 0) };
    if p as isize == -1 {
        unsafe { close(fd) };
        return Err(os_error("mmap kvm_run"));
    }
    let cpu = Vcpu { fd, run: p.cast(), size };
    // struct kvm_vcpu_init { target, features[7] }
    let mut init = [0u32; 8];
    kvm_ioctl(vm, KVM_ARM_PREFERRED_TARGET, init.as_mut_ptr() as u64, "KVM_ARM_PREFERRED_TARGET")?;
    init[1] = VCPU_PSCI_0_2 | if boot.is_some() { 0 } else { VCPU_POWER_OFF };
    kvm_ioctl(cpu.fd, KVM_ARM_VCPU_INIT, init.as_ptr() as u64, "KVM_ARM_VCPU_INIT")?;
    if let Some((pc, x0)) = boot {
        cpu.set_reg(REG_PC, pc)?;
        cpu.set_reg(REG_X0, x0)?;
    }
    VCPUS.lock().unwrap()[id] = (unsafe { pthread_self() }, cpu.run as usize);
    sh.vcpus[id].store(id as u64, Ordering::Release);
    let r = vcpu_loop(sh, id, &cpu);
    sh.vcpus[id].store(super::NO_VCPU, Ordering::Release);
    VCPUS.lock().unwrap()[id] = (0, 0);
    r
}

fn vcpu_loop(sh: &Shared, id: usize, cpu: &Vcpu) -> Result<(), String> {
    let mut vtimer = false;
    loop {
        if sh.stopping() {
            return Ok(());
        }
        let r = unsafe { ioctl(cpu.fd, KVM_RUN, 0u64) };
        cpu.put(RUN_IMMEDIATE_EXIT, 0u8);
        let reason: u32 = if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(EINTR) {
                return Err(format!("KVM_RUN: {e}"));
            }
            EXIT_INTR
        } else {
            cpu.at(RUN_EXIT_REASON)
        };
        // the virtual timer's output, reported on every exit
        let level = cpu.at::<u64>(RUN_DEVICE_IRQ_LEVEL) & DEV_EL1_VTIMER != 0;
        if level != vtimer {
            vtimer = level;
            let mut bus = sh.lock();
            bus.gic.set_ppi(id, VTIMER_IRQ, level);
            sh.wake(&mut bus, Some(id));
        }
        match reason {
            EXIT_INTR => {}
            EXIT_MMIO => {
                // struct { phys_addr, data[8], len, is_write }
                let addr: u64 = cpu.at(RUN_EXIT);
                let len = cpu.at::<u32>(RUN_EXIT + 16) as usize;
                if cpu.at::<u8>(RUN_EXIT + 20) != 0 {
                    let v = cpu.at::<u64>(RUN_EXIT + 8) & if len >= 8 { u64::MAX } else { (1u64 << (8 * len)) - 1 };
                    sh.mmio(id, addr, len, Some(v));
                } else {
                    let v = sh.mmio(id, addr, len, None);
                    cpu.put(RUN_EXIT + 8, v);
                }
            }
            EXIT_SYSTEM_EVENT => match cpu.at::<u32>(RUN_EXIT) {
                SYSTEM_EVENT_SHUTDOWN => sh.set_stop(Stop::Halt(0)),
                SYSTEM_EVENT_RESET => sh.set_stop(Stop::Reset),
                t => sh.set_stop(Stop::Fatal(format!("guest crashed (system event {t})"))),
            },
            r => return Err(format!("unexpected KVM exit reason {r}")),
        }
    }
}
