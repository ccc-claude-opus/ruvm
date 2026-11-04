//! A machine = one CPU + a bus with RAM and devices, laid out like QEMU's `virt`.

use crate::bus::Bus;
use crate::cpu::{aarch64::Aarch64, riscv::Riscv64, Cpu};
use crate::devices::{TestFinisher, Uart, UartKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Guest powered off / exited with this code.
    Halt(i32),
    /// Unrecoverable emulation error (bad instruction, bus fault, ...).
    Fatal(String),
    /// Instruction limit reached.
    Limit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Riscv64,
    Aarch64,
}

impl Arch {
    pub fn parse(s: &str) -> Option<Arch> {
        match s.to_ascii_lowercase().as_str() {
            "riscv" | "riscv64" | "rv64" => Some(Arch::Riscv64),
            "arm" | "arm64" | "aarch64" => Some(Arch::Aarch64),
            _ => None,
        }
    }
    pub fn ram_base(self) -> u64 {
        match self {
            Arch::Riscv64 => 0x8000_0000,
            Arch::Aarch64 => 0x4000_0000,
        }
    }
    fn elf_machine(self) -> u16 {
        match self {
            Arch::Riscv64 => 243,
            Arch::Aarch64 => 183,
        }
    }
}

pub struct Machine {
    pub arch: Arch,
    pub cpu: Box<dyn Cpu>,
    pub bus: Bus,
    pub trace: bool,
    pub insns: u64,
}

impl Machine {
    /// Memory map
    ///   riscv64: 0x0010_0000 test finisher, 0x1000_0000 NS16550 UART, RAM @ 0x8000_0000
    ///   aarch64: 0x0900_0000 PL011 UART,                              RAM @ 0x4000_0000
    pub fn new(arch: Arch, ram_mb: usize, interactive: bool) -> Machine {
        let mut bus = Bus::new(arch.ram_base(), ram_mb << 20);
        let cpu: Box<dyn Cpu> = match arch {
            Arch::Riscv64 => {
                bus.map(0x10_0000, 0x1000, Box::new(TestFinisher::default()));
                bus.map(0x1000_0000, 0x100, Box::new(Uart::new(UartKind::Ns16550, interactive)));
                Box::new(Riscv64::new())
            }
            Arch::Aarch64 => {
                bus.map(0x0900_0000, 0x1000, Box::new(Uart::new(UartKind::Pl011, interactive)));
                Box::new(Aarch64::new())
            }
        };
        let mut m = Machine { arch, cpu, bus, trace: false, insns: 0 };
        m.cpu.set_pc(arch.ram_base());
        m
    }

    /// Load a raw binary at `addr` and start executing there.
    pub fn load_raw(&mut self, addr: u64, data: &[u8]) -> Result<(), String> {
        self.bus.load(addr, data)?;
        self.cpu.set_pc(addr);
        Ok(())
    }

    /// Load an image: ELF64 (auto-detected) or a raw binary placed at the start of RAM.
    pub fn load_image(&mut self, data: &[u8]) -> Result<(), String> {
        if data.starts_with(b"\x7fELF") {
            let entry = self.load_elf(data)?;
            self.cpu.set_pc(entry);
            Ok(())
        } else {
            self.load_raw(self.arch.ram_base(), data)
        }
    }

    fn load_elf(&mut self, d: &[u8]) -> Result<u64, String> {
        let u16_at = |o: usize| u16::from_le_bytes(d[o..o + 2].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(d[o..o + 8].try_into().unwrap());
        if d.len() < 64 || d[4] != 2 || d[5] != 1 {
            return Err("only little-endian ELF64 is supported".into());
        }
        if u16_at(18) != self.arch.elf_machine() {
            return Err(format!("ELF e_machine {} does not match {:?}", u16_at(18), self.arch));
        }
        let entry = u64_at(24);
        let (phoff, phentsize, phnum) = (u64_at(32) as usize, u16_at(54) as usize, u16_at(56) as usize);
        for k in 0..phnum {
            let p = phoff + k * phentsize;
            if p + 56 > d.len() {
                return Err("truncated ELF program headers".into());
            }
            if u32_at(p) != 1 {
                continue; // not PT_LOAD
            }
            let (off, paddr, filesz) = (u64_at(p + 8) as usize, u64_at(p + 24), u64_at(p + 32) as usize);
            if off + filesz > d.len() {
                return Err("truncated ELF segment".into());
            }
            self.bus.load(paddr, &d[off..off + filesz])?; // .bss is already zero
        }
        Ok(entry)
    }

    /// Run until the guest halts, faults, or `max_insns` instructions have executed.
    pub fn run(&mut self, max_insns: Option<u64>) -> Stop {
        loop {
            if max_insns.map_or(false, |m| self.insns >= m) {
                return Stop::Limit;
            }
            if self.trace {
                let pc = self.cpu.pc();
                let raw = self.bus.read(pc, 4).unwrap_or(0);
                eprintln!("[{:>10}] {:#018x}: {:08x}", self.insns, pc, raw);
            }
            self.insns += 1;
            if let Err(s) = self.cpu.step(&mut self.bus) {
                return s;
            }
            if let Some(code) = self.bus.halted() {
                return Stop::Halt(code);
            }
        }
    }
}
