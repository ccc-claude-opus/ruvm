//! Physical address space: one RAM region, the interrupt controllers (CLINT/PLIC)
//! and a list of MMIO devices.

use crate::devices::{Clint, Plic};
use crate::gic::Gic;
use std::fmt;

#[derive(Debug, Clone, Copy)]
pub struct BusError(pub u64);

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bus error at {:#x}", self.0)
    }
}

/// A memory-mapped device. Offsets are relative to the device base.
pub trait Device {
    fn read(&mut self, off: u64, size: usize) -> u64;
    fn write(&mut self, off: u64, size: usize, val: u64);
    /// If the guest asked the machine to power off, return the exit code.
    fn halt_code(&self) -> Option<i32> {
        None
    }
    /// Level of the device's interrupt line.
    fn irq(&mut self) -> bool {
        false
    }
    /// Called after every register write: bus-mastering devices (virtio) access RAM here.
    fn dma(&mut self, _mem: &mut Dma) {}
    /// Called periodically (every few thousand instructions): devices with asynchronous
    /// work (network receive) complete it here.
    fn poll(&mut self, _mem: &mut Dma) {}
    /// The guest asked the machine to reset (reboot).
    fn reset_request(&self) -> bool {
        false
    }
}

/// A device's view of guest RAM for DMA. Accesses outside RAM return `None`.
pub struct Dma<'a> {
    ram: &'a mut [u8],
    base: u64,
}

impl Dma<'_> {
    fn range(&self, addr: u64, len: usize) -> Option<std::ops::Range<usize>> {
        let off = addr.checked_sub(self.base)?;
        let end = off.checked_add(len as u64)?;
        (end <= self.ram.len() as u64).then(|| off as usize..end as usize)
    }
    pub fn read(&self, addr: u64, len: usize) -> Option<&[u8]> {
        Some(&self.ram[self.range(addr, len)?])
    }
    pub fn write(&mut self, addr: u64, data: &[u8]) -> Option<()> {
        let r = self.range(addr, data.len())?;
        self.ram[r].copy_from_slice(data);
        Some(())
    }
    fn rn<const N: usize>(&self, addr: u64) -> Option<[u8; N]> {
        self.read(addr, N)?.try_into().ok()
    }
    pub fn r16(&self, addr: u64) -> Option<u16> {
        self.rn(addr).map(u16::from_le_bytes)
    }
    pub fn r32(&self, addr: u64) -> Option<u32> {
        self.rn(addr).map(u32::from_le_bytes)
    }
    pub fn r64(&self, addr: u64) -> Option<u64> {
        self.rn(addr).map(u64::from_le_bytes)
    }
    pub fn w16(&mut self, addr: u64, v: u16) -> Option<()> {
        self.write(addr, &v.to_le_bytes())
    }
    pub fn w32(&mut self, addr: u64, v: u32) -> Option<()> {
        self.write(addr, &v.to_le_bytes())
    }
}

struct Mapping {
    base: u64,
    size: u64,
    irq: u32, // PLIC source number, 0 = none
    dev: Box<dyn Device>,
}

/// Cross-CPU requests and state for one CPU (hart / core), kept on the bus so that one CPU
/// can start, interrupt or flush another without owning it.
#[derive(Clone, Default)]
pub struct CpuCtl {
    /// The CPU is powered off / stopped (waiting for `start`).
    pub parked: bool,
    /// Start request: (entry pc, argument).
    pub start: Option<(u64, u64)>,
    /// Another CPU asked for a TLB flush (remote `sfence.vma`, broadcast TLBI).
    pub flush_tlb: bool,
    /// Pending supervisor software interrupt (SBI IPI).
    pub ssip: bool,
    /// The CPU executed WFI and sleeps until an interrupt is pending.
    pub waiting: bool,
}

pub struct Bus {
    ram_base: u64,
    ram: Vec<u8>,
    devs: Vec<Mapping>,
    /// Machine timer + software interrupts. `clint.mtime` is the board's time base
    /// (one tick per executed instruction) and is also used for the ARM generic timer.
    pub clint: Clint,
    clint_base: Option<u64>,
    /// Number of CPUs, the CPU whose instruction is executing (selects banked GIC state),
    /// and per-CPU cross-CPU control state.
    pub ncpus: usize,
    pub cur_cpu: usize,
    pub ctl: Vec<CpuCtl>,
    pub plic: Plic,
    plic_base: Option<u64>,
    /// ARM GICv2 (distributor at `gic_base`, CPU interface at `gic_base + 0x10000`).
    pub gic: Gic,
    gic_base: Option<u64>,
    /// Set when a device asked to power off.
    pub halt: Option<i32>,
    /// Set when the guest asked for a reset (reboot).
    pub reset: bool,
    /// Set by every device access: interrupt lines may have changed, so the CPU should
    /// re-evaluate pending interrupts before its next instruction.
    pub irq_dirty: bool,
    /// Host stdin is connected: idle loops (WFI) should really sleep.
    pub interactive: bool,
    /// External events can arrive at any time (network): idle in real time, never skip ahead.
    pub realtime_idle: bool,
    /// User-mode emulation state (`-user`): Linux system calls are served by the host.
    pub user: Option<Box<crate::linux::LinuxUser>>,
}

const CLINT_SIZE: u64 = 0x1_0000;
const PLIC_SIZE: u64 = 0x400_0000;

impl Bus {
    pub fn new(ram_base: u64, ram_size: usize) -> Self {
        Self::with_cpus(ram_base, ram_size, 1)
    }

    pub fn with_cpus(ram_base: u64, ram_size: usize, ncpus: usize) -> Self {
        Bus {
            ram_base,
            ram: vec![0; ram_size],
            devs: Vec::new(),
            clint: Clint::new(ncpus),
            clint_base: None,
            ncpus,
            cur_cpu: 0,
            ctl: vec![CpuCtl::default(); ncpus],
            plic: Plic::new(ncpus),
            plic_base: None,
            gic: Gic::new(ncpus),
            gic_base: None,
            halt: None,
            reset: false,
            irq_dirty: false,
            interactive: false,
            realtime_idle: false,
            user: None,
        }
    }

    /// Board-clock ticks that `n` instructions of one CPU are worth: with several CPUs
    /// taking turns, each CPU's instructions count for `1/ncpus` tick so that simulated
    /// time per instruction does not depend on the CPU count.
    #[inline(always)]
    pub fn ticks(&self, n: u64) -> u64 {
        if self.ncpus == 1 { n } else { n.div_ceil(self.ncpus as u64) }
    }

    pub fn ram_base(&self) -> u64 {
        self.ram_base
    }

    pub fn ram_size(&self) -> u64 {
        self.ram.len() as u64
    }

    pub fn map(&mut self, base: u64, size: u64, dev: Box<dyn Device>) {
        self.map_irq(base, size, 0, dev);
    }

    /// Map a device whose interrupt line is wired to PLIC source `irq`.
    pub fn map_irq(&mut self, base: u64, size: u64, irq: u32, dev: Box<dyn Device>) {
        self.devs.push(Mapping { base, size, irq, dev });
    }

    pub fn map_clint(&mut self, base: u64) {
        self.clint_base = Some(base);
    }

    pub fn map_plic(&mut self, base: u64) {
        self.plic_base = Some(base);
    }

    pub fn map_gic(&mut self, base: u64) {
        self.gic_base = Some(base);
    }

    /// Copy a blob into RAM (used by the image loader).
    pub fn load(&mut self, addr: u64, data: &[u8]) -> Result<(), String> {
        let end = addr.checked_add(data.len() as u64);
        match (addr.checked_sub(self.ram_base), end) {
            (Some(off), Some(end)) if end <= self.ram_base + self.ram_size() => {
                let off = off as usize;
                self.ram[off..off + data.len()].copy_from_slice(data);
                Ok(())
            }
            _ => Err(format!(
                "segment {:#x}..{:#x} does not fit in RAM ({:#x}..{:#x})",
                addr,
                addr.wrapping_add(data.len() as u64),
                self.ram_base,
                self.ram_base + self.ram_size()
            )),
        }
    }

    /// Guest RAM as a byte slice (fast paths that already know an address is RAM).
    #[inline]
    pub fn ram(&self) -> &[u8] {
        &self.ram
    }

    /// Offset into `ram()` of `size` bytes at physical `addr`, if they are all RAM.
    #[inline]
    pub fn ram_off(&self, addr: u64, size: usize) -> Option<usize> {
        let off = addr.wrapping_sub(self.ram_base);
        if off < self.ram.len() as u64 && off + size as u64 <= self.ram.len() as u64 {
            Some(off as usize)
        } else {
            None
        }
    }

    /// Guest RAM bytes at `addr` (None if not entirely inside RAM).
    pub fn guest_bytes(&self, addr: u64, len: usize) -> Option<&[u8]> {
        let off = self.ram_off(addr, len)?;
        Some(&self.ram[off..off + len])
    }

    pub fn guest_bytes_mut(&mut self, addr: u64, len: usize) -> Option<&mut [u8]> {
        let off = self.ram_off(addr, len)?;
        Some(&mut self.ram[off..off + len])
    }

    #[inline]
    pub fn read(&mut self, addr: u64, size: usize) -> Result<u64, BusError> {
        debug_assert!(matches!(size, 1 | 2 | 4 | 8));
        if let Some(off) = self.ram_off(addr, size) {
            let mut b = [0u8; 8];
            b[..size].copy_from_slice(&self.ram[off..off + size]);
            return Ok(u64::from_le_bytes(b));
        }
        self.mmio_read(addr, size)
    }

    #[inline]
    pub fn write(&mut self, addr: u64, size: usize, val: u64) -> Result<(), BusError> {
        debug_assert!(matches!(size, 1 | 2 | 4 | 8));
        if let Some(off) = self.ram_off(addr, size) {
            self.ram[off..off + size].copy_from_slice(&val.to_le_bytes()[..size]);
            return Ok(());
        }
        self.mmio_write(addr, size, val)
    }

    fn mmio_read(&mut self, addr: u64, size: usize) -> Result<u64, BusError> {
        self.irq_dirty = true;
        if let Some(b) = self.clint_base.filter(|&b| addr >= b && addr < b + CLINT_SIZE) {
            return Ok(self.clint.read(addr - b, size));
        }
        if let Some(b) = self.plic_base.filter(|&b| addr >= b && addr < b + PLIC_SIZE) {
            let v = self.plic.read(addr - b, size);
            self.poll_irqs(); // a claim may have changed what is pending
            return Ok(v);
        }
        if let Some(b) = self.gic_base.filter(|&b| addr >= b && addr < b + 0x2_0000) {
            let v = self.gic.read(self.cur_cpu, addr - b, size);
            return Ok(v);
        }
        let k = self.devs.iter().position(|m| addr >= m.base && addr < m.base + m.size).ok_or(BusError(addr))?;
        let m = &mut self.devs[k];
        let v = m.dev.read(addr - m.base, size);
        if m.irq != 0 {
            self.poll_irqs();
        }
        Ok(v)
    }

    fn mmio_write(&mut self, addr: u64, size: usize, val: u64) -> Result<(), BusError> {
        self.irq_dirty = true;
        if let Some(b) = self.clint_base.filter(|&b| addr >= b && addr < b + CLINT_SIZE) {
            self.clint.write(addr - b, size, val);
            return Ok(());
        }
        if let Some(b) = self.plic_base.filter(|&b| addr >= b && addr < b + PLIC_SIZE) {
            self.plic.write(addr - b, size, val);
            self.poll_irqs();
            return Ok(());
        }
        if let Some(b) = self.gic_base.filter(|&b| addr >= b && addr < b + 0x2_0000) {
            self.gic.write(self.cur_cpu, addr - b, size, val);
            return Ok(());
        }
        let k = self.devs.iter().position(|m| addr >= m.base && addr < m.base + m.size).ok_or(BusError(addr))?;
        let m = &mut self.devs[k];
        m.dev.write(addr - m.base, size, val);
        m.dev.dma(&mut Dma { ram: &mut self.ram, base: self.ram_base });
        if let Some(c) = m.dev.halt_code() {
            self.halt = Some(c);
        }
        if m.dev.reset_request() {
            self.reset = true;
        }
        if m.irq != 0 {
            self.poll_irqs();
        }
        Ok(())
    }

    /// Sample every device interrupt line into the PLIC.
    pub fn poll_irqs(&mut self) {
        let (plic, gic) = (self.plic_base.is_some(), self.gic_base.is_some());
        if !plic && !gic {
            return;
        }
        for m in &mut self.devs {
            m.dev.poll(&mut Dma { ram: &mut self.ram, base: self.ram_base });
            if m.irq != 0 {
                let level = m.dev.irq();
                if plic {
                    self.plic.set_level(m.irq, level);
                } else {
                    self.gic.set_level(m.irq, level);
                }
            }
        }
    }

    /// Called when the CPU has nothing to do (WFI with no interrupt pending):
    /// jump the clock to the next timer deadline (CLINT `mtimecmp` or the CPU's own
    /// `deadline`, e.g. Sstc `stimecmp`) instead of spinning. With a real console
    /// attached, time advances in small real-time steps so typing is noticed.
    pub fn idle(&mut self, deadline: u64) {
        let c = &mut self.clint;
        let next = c.next_deadline().min(deadline);
        if self.interactive || self.realtime_idle {
            std::thread::sleep(std::time::Duration::from_micros(500));
            c.mtime += 5_000;
            if next > c.mtime && next - c.mtime < 5_000 {
                c.mtime = next;
            }
        } else if next != u64::MAX && next > c.mtime {
            c.mtime = next;
        } else {
            c.mtime += 1_000;
        }
        self.poll_irqs();
    }

    /// Legacy accessor (kept for API compatibility).
    pub fn halted(&self) -> Option<i32> {
        self.halt
    }
}
