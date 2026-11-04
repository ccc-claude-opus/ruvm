//! Physical address space: one RAM region, the interrupt controllers (CLINT/PLIC)
//! and a list of MMIO devices.

use crate::devices::{Clint, Plic};
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
}

struct Mapping {
    base: u64,
    size: u64,
    irq: u32, // PLIC source number, 0 = none
    dev: Box<dyn Device>,
}

pub struct Bus {
    ram_base: u64,
    ram: Vec<u8>,
    devs: Vec<Mapping>,
    /// Machine timer + software interrupts. `clint.mtime` is the board's time base
    /// (one tick per executed instruction) and is also used for the ARM generic timer.
    pub clint: Clint,
    clint_base: Option<u64>,
    pub plic: Plic,
    plic_base: Option<u64>,
    /// Set when a device asked to power off.
    pub halt: Option<i32>,
    /// Host stdin is connected: idle loops (WFI) should really sleep.
    pub interactive: bool,
}

const CLINT_SIZE: u64 = 0x1_0000;
const PLIC_SIZE: u64 = 0x400_0000;

impl Bus {
    pub fn new(ram_base: u64, ram_size: usize) -> Self {
        Bus {
            ram_base,
            ram: vec![0; ram_size],
            devs: Vec::new(),
            clint: Clint::default(),
            clint_base: None,
            plic: Plic::default(),
            plic_base: None,
            halt: None,
            interactive: false,
        }
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

    #[inline]
    fn ram_off(&self, addr: u64, size: usize) -> Option<usize> {
        let off = addr.wrapping_sub(self.ram_base);
        if off < self.ram.len() as u64 && off + size as u64 <= self.ram.len() as u64 {
            Some(off as usize)
        } else {
            None
        }
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
        if let Some(b) = self.clint_base.filter(|&b| addr >= b && addr < b + CLINT_SIZE) {
            return Ok(self.clint.read(addr - b, size));
        }
        if let Some(b) = self.plic_base.filter(|&b| addr >= b && addr < b + PLIC_SIZE) {
            let v = self.plic.read(addr - b, size);
            self.poll_irqs(); // a claim may have changed what is pending
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
        if let Some(b) = self.clint_base.filter(|&b| addr >= b && addr < b + CLINT_SIZE) {
            self.clint.write(addr - b, size, val);
            return Ok(());
        }
        if let Some(b) = self.plic_base.filter(|&b| addr >= b && addr < b + PLIC_SIZE) {
            self.plic.write(addr - b, size, val);
            self.poll_irqs();
            return Ok(());
        }
        let k = self.devs.iter().position(|m| addr >= m.base && addr < m.base + m.size).ok_or(BusError(addr))?;
        let m = &mut self.devs[k];
        m.dev.write(addr - m.base, size, val);
        if let Some(c) = m.dev.halt_code() {
            self.halt = Some(c);
        }
        if m.irq != 0 {
            self.poll_irqs();
        }
        Ok(())
    }

    /// Sample every device interrupt line into the PLIC.
    pub fn poll_irqs(&mut self) {
        if self.plic_base.is_none() {
            return;
        }
        for m in &mut self.devs {
            if m.irq != 0 {
                let level = m.dev.irq();
                self.plic.set_level(m.irq, level);
            }
        }
    }

    /// Called when the CPU has nothing to do (WFI with no interrupt pending):
    /// jump the clock to the next timer deadline instead of spinning.
    pub fn idle(&mut self) {
        let c = &mut self.clint;
        if self.interactive {
            std::thread::sleep(std::time::Duration::from_micros(500));
            c.mtime += 5_000;
            if c.mtimecmp > c.mtime && c.mtimecmp - c.mtime < 5_000 {
                c.mtime = c.mtimecmp;
            }
        } else if c.mtimecmp != u64::MAX && c.mtimecmp > c.mtime {
            c.mtime = c.mtimecmp;
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
