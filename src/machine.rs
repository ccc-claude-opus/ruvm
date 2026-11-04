//! A machine = one CPU + a bus with RAM and devices, laid out like QEMU's `virt`.

use crate::bus::Bus;
use crate::cpu::{aarch64::Aarch64, riscv::Riscv64, Cpu};
use crate::devices::{forward_stdin, TestFinisher, Uart, UartKind};
use std::sync::mpsc::{channel, Sender};
use crate::virtio::VirtioBlk;

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
    console_tx: Sender<u8>,
}

impl Machine {
    /// Memory map (same addresses as QEMU's `virt` boards)
    ///   riscv64: 0x0010_0000 test finisher, 0x0200_0000 CLINT, 0x0c00_0000 PLIC,
    ///            0x1000_0000 NS16550 UART (PLIC irq 10), RAM @ 0x8000_0000
    ///   aarch64: 0x0900_0000 PL011 UART,                              RAM @ 0x4000_0000
    pub fn new(arch: Arch, ram_mb: usize, interactive: bool) -> Machine {
        let mut bus = Bus::new(arch.ram_base(), ram_mb << 20);
        bus.interactive = interactive;
        let (console_tx, console_rx) = channel();
        if interactive {
            forward_stdin(console_tx.clone());
        }
        let rx = Some(console_rx);
        let cpu: Box<dyn Cpu> = match arch {
            Arch::Riscv64 => {
                bus.map(0x10_0000, 0x1000, Box::new(TestFinisher::default()));
                bus.map_clint(0x200_0000);
                bus.map_plic(0xc00_0000);
                bus.map_irq(0x1000_0000, 0x100, 10, Box::new(Uart::new(UartKind::Ns16550, rx)));
                Box::new(Riscv64::new())
            }
            Arch::Aarch64 => {
                bus.map(0x0900_0000, 0x1000, Box::new(Uart::new(UartKind::Pl011, rx)));
                Box::new(Aarch64::new())
            }
        };
        let mut m = Machine { arch, cpu, bus, trace: false, insns: 0, console_tx };
        m.cpu.set_pc(arch.ram_base());
        m
    }

    /// Queue bytes for the guest console (as if typed on the serial line).
    pub fn send_input(&self, bytes: &[u8]) {
        for &b in bytes {
            let _ = self.console_tx.send(b);
        }
    }

    /// Attach a raw disk image as a virtio-mmio block device
    /// (riscv64: 0x1000_1000, PLIC irq 1 — the first virtio slot of QEMU `virt`).
    /// With `snapshot`, guest writes stay in memory instead of going to the file.
    pub fn attach_disk(&mut self, path: &str, snapshot: bool) -> Result<(), String> {
        let dev = if snapshot {
            VirtioBlk::with_image(std::fs::read(path).map_err(|e| format!("{path}: {e}"))?, None)
        } else {
            VirtioBlk::open(path)?
        };
        self.attach_blk(dev)
    }

    /// Attach an in-memory disk image (writes are not persisted).
    pub fn attach_disk_image(&mut self, image: Vec<u8>) -> Result<(), String> {
        self.attach_blk(VirtioBlk::with_image(image, None))
    }

    fn attach_blk(&mut self, dev: VirtioBlk) -> Result<(), String> {
        match self.arch {
            Arch::Riscv64 => self.bus.map_irq(0x1000_1000, 0x1000, 1, Box::new(dev)),
            Arch::Aarch64 => return Err("virtio disks are not wired up on aarch64 yet".into()),
        }
        Ok(())
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
        const BATCH: u64 = 4096;
        loop {
            let left = max_insns.map_or(BATCH, |m| m.saturating_sub(self.insns).min(BATCH));
            if left == 0 {
                return Stop::Limit;
            }
            if self.trace {
                // trace mode: one instruction at a time (raw word read at the physical pc)
                let pc = self.cpu.pc();
                let raw = self.bus.read(pc, 4).unwrap_or(0);
                eprintln!("[{:>10}] {:#018x}: {:08x}", self.insns, pc, raw);
            }
            let (ran, stop) = self.cpu.run(&mut self.bus, if self.trace { 1 } else { left });
            self.insns += ran;
            if let Some(s) = stop {
                return s;
            }
            self.bus.poll_irqs(); // pick up console input
        }
    }
}
