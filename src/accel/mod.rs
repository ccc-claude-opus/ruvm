//! Hardware virtualization for AArch64 guests on AArch64 hosts: the guest's instructions run
//! directly on the host CPU (Hypervisor.framework on macOS, KVM on Linux) and only device
//! accesses come back to ruvm. The board is the same as with the interpreter (RAM, GICv2,
//! PL011, PL031, virtio-mmio, PSCI), so the same kernels and disks boot unchanged.
//!
//! Each guest CPU runs on its own host thread. The bus is shared behind a mutex: MMIO exits,
//! PSCI calls and the I/O thread (console input, network, interrupt lines) lock it briefly.
//! Guest time is real time (the host's counter), not one tick per instruction.

// without a backend for this host only `Accel` is used
#![cfg_attr(not(all(target_arch = "aarch64", any(target_os = "macos", target_os = "linux"))), allow(dead_code))]

use crate::bus::Bus;
use crate::machine::{Arch, Machine, Stop};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod hvf;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
mod kvm;

/// How guest instructions are executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Accel {
    /// ruvm's interpreter (QEMU calls its own "tcg").
    #[default]
    Tcg,
    /// Apple Hypervisor.framework.
    Hvf,
    /// Linux KVM.
    Kvm,
}

impl Accel {
    pub fn parse(s: &str) -> Option<Accel> {
        match s {
            "tcg" => Some(Accel::Tcg),
            "hvf" => Some(Accel::Hvf),
            "kvm" => Some(Accel::Kvm),
            _ => None,
        }
    }

    /// The hardware accelerator this host has, if any.
    pub fn native() -> Option<Accel> {
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            Some(Accel::Hvf)
        } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
            Some(Accel::Kvm)
        } else {
            None
        }
    }
}

/// Interrupt ID of the EL1 virtual timer (PPI 11), the timer Linux uses at EL1.
pub(crate) const VTIMER_IRQ: u32 = 27;

/// No vCPU handle yet.
const NO_VCPU: u64 = u64::MAX;

/// State shared by the vCPU threads and the I/O thread.
pub(crate) struct Shared {
    pub bus: Mutex<Bus>,
    /// Signalled whenever a sleeping CPU may have something to do (interrupt, CPU_ON, stop).
    pub cv: Condvar,
    stop: Mutex<Option<Stop>>,
    stopping: AtomicBool,
    /// Backend handle of each vCPU, to force it out of the guest (`kick`).
    pub vcpus: Vec<AtomicU64>,
    /// The interrupt line level each vCPU was last given.
    pub given: Vec<AtomicBool>,
    kick: fn(u64),
    /// Backends that can set a vCPU's interrupt line while it runs (KVM) do it here; the others
    /// are kicked out of the guest and set it themselves before entering it again (HVF).
    set_line: Option<fn(usize, bool)>,
    /// Print unexpected exits (`-d`).
    pub trace: bool,
}

impl Shared {
    fn new(bus: Bus, kick: fn(u64), set_line: Option<fn(usize, bool)>, trace: bool) -> Shared {
        let n = bus.ncpus;
        Shared {
            bus: Mutex::new(bus),
            cv: Condvar::new(),
            stop: Mutex::new(None),
            stopping: AtomicBool::new(false),
            vcpus: (0..n).map(|_| AtomicU64::new(NO_VCPU)).collect(),
            given: (0..n).map(|_| AtomicBool::new(false)).collect(),
            kick,
            set_line,
            trace,
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, Bus> {
        self.bus.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    /// End the run (the first reason wins) and get every CPU out of the guest.
    pub fn set_stop(&self, s: Stop) {
        self.stop.lock().unwrap().get_or_insert(s);
        self.stopping.store(true, Ordering::Release);
        for v in &self.vcpus {
            let h = v.load(Ordering::Acquire);
            if h != NO_VCPU {
                (self.kick)(h);
            }
        }
        self.cv.notify_all();
    }

    /// After the bus changed: pick up a power-off or reset request, and pass the interrupt lines
    /// on: set them directly, or interrupt the CPUs (other than `me`) whose line is now up but who
    /// have not been told yet.
    pub fn wake(&self, bus: &mut Bus, me: Option<usize>) {
        if let Some(code) = bus.halt.take() {
            self.set_stop(Stop::Halt(code));
        }
        if std::mem::take(&mut bus.reset) {
            self.set_stop(Stop::Reset);
        }
        if let Some(set) = self.set_line {
            for c in 0..bus.ncpus {
                let line = bus.gic.irq_line[c];
                if self.given[c].swap(line, Ordering::AcqRel) != line {
                    set(c, line);
                }
            }
            return;
        }
        let mut any = false;
        for c in 0..bus.ncpus {
            if Some(c) != me && bus.gic.irq_line[c] && !self.given[c].load(Ordering::Acquire) {
                let h = self.vcpus[c].load(Ordering::Acquire);
                if h != NO_VCPU {
                    (self.kick)(h);
                }
                any = true;
            }
        }
        if any {
            self.cv.notify_all();
        }
    }

    /// A guest load or store to a non-RAM address (`write` = the value stored).
    pub fn mmio(&self, id: usize, addr: u64, size: usize, write: Option<u64>) -> u64 {
        let mut bus = self.lock();
        bus.cur_cpu = id;
        let r = match write {
            Some(v) => bus.write(addr, size, v).map(|_| 0),
            None => bus.read(addr, size),
        };
        self.wake(&mut bus, Some(id));
        r.unwrap_or_else(|e| {
            if self.trace {
                eprintln!("[accel] cpu{id}: {e} ({} bytes, {})", size, if write.is_some() { "write" } else { "read" });
            }
            0
        })
    }

    // used by HVF; KVM handles PSCI and WFI itself
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    /// PSCI call from CPU `id`: the new x0, and whether the CPU powered itself off
    /// (SYSTEM_OFF / SYSTEM_RESET stop the machine).
    pub fn psci(&self, id: usize, x: [u64; 4]) -> (u64, bool) {
        let mut bus = self.lock();
        let r = match crate::psci::call(&mut bus, id, x) {
            crate::psci::Psci::Ret(v) => (v, false),
            crate::psci::Psci::Off => (0, true),
            crate::psci::Psci::Stop(s) => {
                self.set_stop(s);
                (0, false)
            }
        };
        self.cv.notify_all(); // CPU_ON
        r
    }

    // used by HVF; KVM handles PSCI and WFI itself
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    /// Block a powered-off CPU until PSCI CPU_ON starts it: its (entry, context id),
    /// or `None` when the machine stops.
    pub fn wait_start(&self, id: usize) -> Option<(u64, u64)> {
        let mut bus = self.lock();
        loop {
            if self.stopping() {
                return None;
            }
            if let Some(s) = bus.ctl[id].start.take() {
                bus.ctl[id].parked = false;
                return Some(s);
            }
            bus = self.cv.wait(bus).unwrap_or_else(|e| e.into_inner());
        }
    }

    // used by HVF; KVM handles PSCI and WFI itself
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    /// WFI: sleep until this CPU's interrupt line is up, `deadline` passes, or the machine stops.
    pub fn idle(&self, id: usize, deadline: Option<Instant>) {
        self.given[id].store(false, Ordering::Release);
        let mut bus = self.lock();
        loop {
            if self.stopping() || bus.gic.irq_line[id] {
                return;
            }
            let wait = match deadline {
                Some(d) => match d.checked_duration_since(Instant::now()) {
                    Some(w) if !w.is_zero() => w,
                    _ => return,
                },
                None => Duration::from_millis(100),
            };
            bus = self.cv.wait_timeout(bus, wait).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// The I/O thread: devices with asynchronous work (console input, network receive) are polled
/// every millisecond and the board clock follows real time (10 MHz, as with the interpreter).
fn io_loop(sh: &Shared) {
    let t0 = Instant::now();
    while !sh.stopping() {
        std::thread::sleep(Duration::from_millis(1));
        let mut bus = sh.lock();
        bus.clint.mtime = (t0.elapsed().as_nanos() / 100) as u64;
        bus.poll_irqs();
        sh.wake(&mut bus, None);
    }
}

/// Run `m` with its hardware accelerator until the guest powers off, resets or fails.
pub fn run(m: &mut Machine) -> Stop {
    if m.arch != Arch::Aarch64 {
        return Stop::Fatal(format!("-accel {:?} needs an AArch64 guest", m.accel).to_lowercase());
    }
    if Accel::native() != Some(m.accel) {
        return Stop::Fatal(format!("-accel {} is not available on this host", format!("{:?}", m.accel).to_lowercase()));
    }
    let entry = {
        let c = m.cpus[0].as_any().downcast_mut::<crate::cpu::aarch64::Aarch64>().expect("AArch64 CPU");
        (crate::cpu::Cpu::pc(c), c.reg(0))
    };
    let bus = std::mem::replace(&mut m.bus, Bus::new(0, 0));
    let (stop, bus) = run_backend(m.accel, bus, entry, m.trace);
    m.bus = bus;
    stop
}

fn run_backend(accel: Accel, bus: Bus, entry: (u64, u64), trace: bool) -> (Stop, Bus) {
    match accel {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        Accel::Hvf => hvf::run(bus, entry, trace),
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        Accel::Kvm => kvm::run(bus, entry, trace),
        _ => {
            let _ = (entry, trace);
            (Stop::Fatal("no hardware accelerator".into()), bus)
        }
    }
}

/// Start the vCPU threads (`vcpu(shared, id, entry)`; CPU 0 gets the boot entry point and the
/// others wait for CPU_ON) and the I/O thread; return when the machine stops.
/// A backend's vCPU thread body: `(shared, cpu id, boot entry point and x0 for CPU 0)`.
pub(crate) type VcpuFn = fn(&Shared, usize, Option<(u64, u64)>) -> Result<(), String>;

pub(crate) fn run_threads(sh: Shared, entry: (u64, u64), vcpu: VcpuFn) -> (Stop, Bus) {
    let n = sh.lock().ncpus;
    std::thread::scope(|s| {
        for id in 0..n {
            let sh = &sh;
            std::thread::Builder::new()
                .name(format!("vcpu{id}"))
                .spawn_scoped(s, move || {
                    if let Err(e) = vcpu(sh, id, (id == 0).then_some(entry)) {
                        sh.set_stop(Stop::Fatal(format!("cpu{id}: {e}")));
                    }
                })
                .expect("vCPU thread");
        }
        s.spawn(|| io_loop(&sh));
    });
    let stop = sh.stop.into_inner().unwrap().unwrap_or(Stop::Fatal("all CPUs stopped".into()));
    (stop, sh.bus.into_inner().unwrap_or_else(|e| e.into_inner()))
}
