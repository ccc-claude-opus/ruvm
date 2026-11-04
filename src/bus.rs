//! Physical address space: one RAM region plus a list of MMIO devices.

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
}

struct Mapping {
    base: u64,
    size: u64,
    dev: Box<dyn Device>,
}

pub struct Bus {
    ram_base: u64,
    ram: Vec<u8>,
    devs: Vec<Mapping>,
}

impl Bus {
    pub fn new(ram_base: u64, ram_size: usize) -> Self {
        Bus { ram_base, ram: vec![0; ram_size], devs: Vec::new() }
    }

    pub fn ram_base(&self) -> u64 {
        self.ram_base
    }

    pub fn ram_size(&self) -> u64 {
        self.ram.len() as u64
    }

    pub fn map(&mut self, base: u64, size: u64, dev: Box<dyn Device>) {
        self.devs.push(Mapping { base, size, dev });
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

    pub fn read(&mut self, addr: u64, size: usize) -> Result<u64, BusError> {
        debug_assert!(matches!(size, 1 | 2 | 4 | 8));
        if let Some(off) = addr.checked_sub(self.ram_base) {
            if off.checked_add(size as u64).map_or(false, |e| e <= self.ram_size()) {
                let off = off as usize;
                let mut b = [0u8; 8];
                b[..size].copy_from_slice(&self.ram[off..off + size]);
                return Ok(u64::from_le_bytes(b));
            }
        }
        for m in &mut self.devs {
            if addr >= m.base && addr < m.base + m.size {
                return Ok(m.dev.read(addr - m.base, size));
            }
        }
        Err(BusError(addr))
    }

    pub fn write(&mut self, addr: u64, size: usize, val: u64) -> Result<(), BusError> {
        debug_assert!(matches!(size, 1 | 2 | 4 | 8));
        if let Some(off) = addr.checked_sub(self.ram_base) {
            if off.checked_add(size as u64).map_or(false, |e| e <= self.ram_size()) {
                let off = off as usize;
                self.ram[off..off + size].copy_from_slice(&val.to_le_bytes()[..size]);
                return Ok(());
            }
        }
        for m in &mut self.devs {
            if addr >= m.base && addr < m.base + m.size {
                m.dev.write(addr - m.base, size, val);
                return Ok(());
            }
        }
        Err(BusError(addr))
    }

    pub fn halted(&self) -> Option<i32> {
        self.devs.iter().find_map(|m| m.dev.halt_code())
    }
}
