//! A machine = one CPU + a bus with RAM and devices, laid out like QEMU's `virt`.

use crate::bus::Bus;
use crate::cpu::{aarch64::Aarch64, riscv::Riscv64, Cpu};
use crate::devices::{
    forward_stdin, Clint, GoldfishRtc, Pl011, Pl031, SharedRx, SharedScript, TestFinisher, Uart, UartKind,
};
use crate::fdt::Fdt;
use crate::virtio::blk::{Disk, VirtioBlk};
use crate::virtio::net::{NetSpec, VirtioNet};
use crate::virtio::rng::VirtioRng;
use crate::virtio::VirtioMmio;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Guest powered off / exited with this code.
    Halt(i32),
    /// Unrecoverable emulation error (bad instruction, bus fault, ...).
    Fatal(String),
    /// Instruction limit reached.
    Limit,
    /// The guest asked for a reboot (the caller decides whether to call `Machine::reset`).
    Reset,
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
    console_rx: SharedRx,
    script: SharedScript,
    ram_mb: usize,
    /// virtio devices in slot order; recreated on reset (disks keep their contents).
    devices: Vec<DevSpec>,
    /// How the guest was loaded, replayed on reset.
    boot: Option<BootRecord>,
    /// Fixed start time of the RTC (seconds since the epoch); `None` = host clock.
    rtc_epoch: Option<u64>,
}

/// A virtio device attached to the board.
#[derive(Clone)]
pub enum DevSpec {
    Blk(Disk),
    Rng(Option<u64>),
    Net(NetSpec),
}

#[derive(Clone)]
enum BootRecord {
    Image(Vec<u8>),
    Raw(u64, Vec<u8>),
    Linux(LinuxBoot),
}

/// MMIO window and interrupt of virtio slot `n` (QEMU `virt` layout).
fn virtio_slot(arch: Arch, n: usize) -> (u64, u64, u32) {
    match arch {
        Arch::Riscv64 => (0x1000_1000 + 0x1000 * n as u64, 0x1000, 1 + n as u32),
        Arch::Aarch64 => (0x0a00_0000 + 0x200 * n as u64, 0x200, 48 + n as u32),
    }
}

pub const MAX_VIRTIO: usize = 8;

/// What to boot with `Machine::boot_linux` (QEMU's -kernel/-initrd/-append/-bios).
#[derive(Default, Clone)]
pub struct LinuxBoot {
    /// Kernel `Image` (the arm64 / RISC-V boot-image format).
    pub kernel: Vec<u8>,
    pub initrd: Option<Vec<u8>>,
    pub cmdline: String,
    /// Firmware to run first in M-mode (OpenSBI `fw_dynamic`); `None` = built-in SBI.
    pub bios: Option<Vec<u8>>,
}

/// Is this a Linux `Image` for `arch` (magic at offset 0x38)?
pub fn is_linux_image(arch: Arch, data: &[u8]) -> bool {
    let magic: &[u8] = match arch {
        Arch::Riscv64 => b"RSC\x05",
        Arch::Aarch64 => b"ARM\x64",
    };
    data.len() > 0x40 && &data[0x38..0x3c] == magic
}

impl Machine {
    /// Memory map (same addresses as QEMU's `virt` boards)
    ///   riscv64: 0x0010_0000 test finisher, 0x0200_0000 CLINT, 0x0c00_0000 PLIC,
    ///            0x1000_0000 NS16550 UART (PLIC irq 10), RAM @ 0x8000_0000
    ///   aarch64: 0x0900_0000 PL011 UART,                              RAM @ 0x4000_0000
    pub fn new(arch: Arch, ram_mb: usize, interactive: bool) -> Machine {
        let (console_tx, console_rx) = channel();
        if interactive {
            forward_stdin(console_tx.clone());
        }
        let bus = Bus::new(arch.ram_base(), ram_mb << 20);
        let mut m = Machine {
            arch,
            cpu: Box::new(Riscv64::new()),
            bus,
            trace: false,
            insns: 0,
            console_tx,
            console_rx: Arc::new(Mutex::new(console_rx)),
            script: SharedScript::default(),
            ram_mb,
            devices: Vec::new(),
            boot: None,
            rtc_epoch: None,
        };
        m.bus.interactive = interactive;
        m.build_board();
        m
    }

    /// (Re)create the CPU and the bus with all devices: power-on state.
    fn build_board(&mut self) {
        let arch = self.arch;
        let interactive = self.bus.interactive;
        let mut bus = Bus::new(arch.ram_base(), self.ram_mb << 20);
        bus.interactive = interactive;
        let (rx, script) = (Some(self.console_rx.clone()), self.script.clone());
        let cpu: Box<dyn Cpu> = match arch {
            Arch::Riscv64 => {
                bus.map(0x10_0000, 0x1000, Box::new(TestFinisher::default()));
                bus.map_irq(0x10_1000, 0x1000, 11, Box::new(GoldfishRtc::new(self.rtc_epoch)));
                bus.map_clint(0x200_0000);
                bus.map_plic(0xc00_0000);
                bus.map_irq(0x1000_0000, 0x100, 10, Box::new(Uart::new(UartKind::Ns16550, rx, script)));
                Box::new(Riscv64::new())
            }
            Arch::Aarch64 => {
                bus.map_gic(0x0800_0000);
                bus.map_irq(0x0900_0000, 0x1000, 33, Box::new(Pl011::new(rx, script)));
                bus.map_irq(0x0901_0000, 0x1000, 34, Box::new(Pl031::new(self.rtc_epoch)));
                Box::new(Aarch64::new())
            }
        };
        bus.realtime_idle = self.devices.iter().any(|d| matches!(d, DevSpec::Net(_)));
        for (n, spec) in self.devices.iter().enumerate() {
            let (base, size, irq) = virtio_slot(arch, n);
            let backend: Box<dyn crate::virtio::Backend> = match spec {
                DevSpec::Blk(d) => Box::new(VirtioBlk::new(d.clone())),
                DevSpec::Rng(seed) => Box::new(VirtioRng::new(*seed)),
                DevSpec::Net(spec) => Box::new(VirtioNet::new(spec.clone())),
            };
            bus.map_irq(base, size, irq, Box::new(VirtioMmio::new(backend)));
        }
        self.bus = bus;
        self.cpu = cpu;
        self.cpu.set_pc(arch.ram_base());
    }

    /// Start the RTCs at this Unix time instead of following the host clock.
    pub fn set_rtc_epoch(&mut self, epoch: Option<u64>) {
        self.rtc_epoch = epoch;
        self.build_board();
    }

    /// Reboot: power-cycle the board (RAM cleared, devices reset, disks keep their
    /// contents) and boot the same images again.
    pub fn reset(&mut self) -> Result<(), String> {
        self.build_board();
        match self.boot.clone() {
            Some(BootRecord::Image(d)) => self.load_image(&d),
            Some(BootRecord::Raw(a, d)) => self.load_raw(a, &d),
            Some(BootRecord::Linux(b)) => self.boot_linux(&b).map(|_| ()),
            None => Ok(()),
        }
    }

    /// Queue bytes for the guest console (as if typed on the serial line).
    pub fn send_input(&self, bytes: &[u8]) {
        for &b in bytes {
            let _ = self.console_tx.send(b);
        }
    }

    /// Type `text` on the console once the guest has printed `wait_for`
    /// (steps run in order; an empty pattern types immediately after the previous step).
    pub fn send_input_after(&self, wait_for: &[u8], text: &[u8]) {
        self.script.lock().unwrap().push(wait_for, text);
    }

    /// Attach a raw disk image as a virtio-mmio block device in the next free slot.
    /// With `snapshot`, guest writes stay in memory instead of going to the file.
    pub fn attach_disk(&mut self, path: &str, snapshot: bool) -> Result<(), String> {
        let disk = if snapshot { Disk::read_file(path)? } else { Disk::open(path)? };
        self.attach(DevSpec::Blk(disk))
    }

    /// Attach an in-memory disk image (writes are not persisted).
    pub fn attach_disk_image(&mut self, image: Vec<u8>) -> Result<(), String> {
        self.attach(DevSpec::Blk(Disk::from_image(image)))
    }

    /// Attach a virtio device in the next free slot.
    pub fn attach(&mut self, spec: DevSpec) -> Result<(), String> {
        if self.devices.len() >= MAX_VIRTIO {
            return Err("too many virtio devices".into());
        }
        if matches!(spec, DevSpec::Net(_)) {
            self.bus.realtime_idle = true;
        }
        self.devices.push(spec);
        let n = self.devices.len() - 1;
        let (base, size, irq) = virtio_slot(self.arch, n);
        let backend: Box<dyn crate::virtio::Backend> = match &self.devices[n] {
            DevSpec::Blk(d) => Box::new(VirtioBlk::new(d.clone())),
            DevSpec::Rng(seed) => Box::new(VirtioRng::new(*seed)),
            DevSpec::Net(spec) => Box::new(VirtioNet::new(spec.clone())),
        };
        self.bus.map_irq(base, size, irq, Box::new(VirtioMmio::new(backend)));
        Ok(())
    }

    /// Device tree describing this board (what QEMU's `virt` would pass).
    pub fn device_tree(&self, cmdline: &str, initrd: Option<(u64, u64)>) -> Vec<u8> {
        let ram = (self.ram_mb as u64) << 20;
        let mut t = Fdt::new();
        t.begin("");
        t.u32("#address-cells", 2);
        t.u32("#size-cells", 2);
        match self.arch {
            Arch::Riscv64 => {
                t.str("compatible", "riscv-virtio");
                t.str("model", "riscv-virtio,qemu");
                t.begin("chosen");
                t.str("bootargs", cmdline);
                t.str("stdout-path", "/soc/serial@10000000");
                if let Some((s, e)) = initrd {
                    t.u64("linux,initrd-start", s);
                    t.u64("linux,initrd-end", e);
                }
                t.end();
                t.begin("memory@80000000");
                t.str("device_type", "memory");
                t.u64s("reg", &[0x8000_0000, ram]);
                t.end();
                t.begin("cpus");
                t.u32("#address-cells", 1);
                t.u32("#size-cells", 0);
                t.u32("timebase-frequency", Clint::FREQ as u32);
                t.begin("cpu@0");
                t.str("device_type", "cpu");
                t.u32("reg", 0);
                t.str("status", "okay");
                t.str("compatible", "riscv");
                t.str("riscv,isa", "rv64imafdc_zicntr_zicsr_zifencei_sstc");
                t.str("riscv,isa-base", "rv64i");
                t.strs("riscv,isa-extensions", &["i", "m", "a", "f", "d", "c", "zicntr", "zicsr", "zifencei", "sstc"]);
                t.str("mmu-type", "riscv,sv39");
                t.begin("interrupt-controller");
                t.u32("#interrupt-cells", 1);
                t.empty("interrupt-controller");
                t.str("compatible", "riscv,cpu-intc");
                t.u32("phandle", 1);
                t.end();
                t.end();
                t.end();
                t.begin("soc");
                t.u32("#address-cells", 2);
                t.u32("#size-cells", 2);
                t.str("compatible", "simple-bus");
                t.empty("ranges");
                t.begin("clint@2000000");
                t.strs("compatible", &["sifive,clint0", "riscv,clint0"]);
                t.u64s("reg", &[0x200_0000, 0x1_0000]);
                t.u32s("interrupts-extended", &[1, 3, 1, 7]);
                t.end();
                t.begin("plic@c000000");
                t.strs("compatible", &["sifive,plic-1.0.0", "riscv,plic0"]);
                t.u64s("reg", &[0xc00_0000, 0x60_0000]);
                t.u32("#interrupt-cells", 1);
                t.u32("#address-cells", 0);
                t.empty("interrupt-controller");
                t.u32("riscv,ndev", 127);
                t.u32s("interrupts-extended", &[1, 11, 1, 9]);
                t.u32("phandle", 2);
                t.end();
                t.begin("serial@10000000");
                t.str("compatible", "ns16550a");
                t.u64s("reg", &[0x1000_0000, 0x100]);
                t.u32("clock-frequency", 3_686_400);
                t.u32("interrupt-parent", 2);
                t.u32("interrupts", 10);
                t.end();
                for n in 0..self.devices.len() {
                    let (base, size, irq) = virtio_slot(self.arch, n);
                    t.begin(&format!("virtio_mmio@{base:x}"));
                    t.str("compatible", "virtio,mmio");
                    t.u64s("reg", &[base, size]);
                    t.u32("interrupt-parent", 2);
                    t.u32("interrupts", irq);
                    t.end();
                }
                t.begin("rtc@101000");
                t.str("compatible", "google,goldfish-rtc");
                t.u64s("reg", &[0x10_1000, 0x1000]);
                t.u32("interrupt-parent", 2);
                t.u32("interrupts", 11);
                t.end();
                t.begin("test@100000");
                t.strs("compatible", &["sifive,test1", "sifive,test0", "syscon"]);
                t.u64s("reg", &[0x10_0000, 0x1000]);
                t.u32("phandle", 3);
                t.end();
                t.end();
                t.begin("poweroff");
                t.str("compatible", "syscon-poweroff");
                t.u32("regmap", 3);
                t.u32("offset", 0);
                t.u32("value", 0x5555);
                t.end();
                t.begin("reboot");
                t.str("compatible", "syscon-reboot");
                t.u32("regmap", 3);
                t.u32("offset", 0);
                t.u32("value", 0x7777);
                t.end();
            }
            Arch::Aarch64 => {
                t.str("compatible", "linux,dummy-virt");
                t.u32("interrupt-parent", 1);
                t.begin("chosen");
                t.str("bootargs", cmdline);
                t.str("stdout-path", "/pl011@9000000");
                if let Some((s, e)) = initrd {
                    t.u64("linux,initrd-start", s);
                    t.u64("linux,initrd-end", e);
                }
                t.end();
                t.begin("memory@40000000");
                t.str("device_type", "memory");
                t.u64s("reg", &[0x4000_0000, ram]);
                t.end();
                t.begin("psci");
                t.strs("compatible", &["arm,psci-1.0", "arm,psci-0.2", "arm,psci"]);
                t.str("method", "hvc");
                t.end();
                t.begin("cpus");
                t.u32("#address-cells", 1);
                t.u32("#size-cells", 0);
                t.begin("cpu@0");
                t.str("device_type", "cpu");
                t.str("compatible", "arm,cortex-a57");
                t.u32("reg", 0);
                t.str("enable-method", "psci");
                t.end();
                t.end();
                t.begin("interrupt-controller@8000000");
                t.str("compatible", "arm,cortex-a15-gic");
                t.u32("#interrupt-cells", 3);
                t.empty("interrupt-controller");
                t.u64s("reg", &[0x0800_0000, 0x1_0000, 0x0801_0000, 0x1_0000]);
                t.u32("phandle", 1);
                t.end();
                t.begin("timer");
                t.str("compatible", "arm,armv8-timer");
                t.empty("always-on");
                // secure phys (13), non-secure phys (14), virtual (11), hypervisor (10); level low
                t.u32s("interrupts", &[1, 13, 0xf08, 1, 14, 0xf08, 1, 11, 0xf08, 1, 10, 0xf08]);
                t.end();
                t.begin("apb-pclk");
                t.str("compatible", "fixed-clock");
                t.u32("#clock-cells", 0);
                t.u32("clock-frequency", 24_000_000);
                t.str("clock-output-names", "clk24mhz");
                t.u32("phandle", 2);
                t.end();
                t.begin("pl011@9000000");
                t.strs("compatible", &["arm,pl011", "arm,primecell"]);
                t.u64s("reg", &[0x0900_0000, 0x1000]);
                t.u32s("interrupts", &[0, 1, 4]);
                t.u32s("clocks", &[2, 2]);
                t.strs("clock-names", &["uartclk", "apb_pclk"]);
                t.end();
                for n in 0..self.devices.len() {
                    let (base, size, irq) = virtio_slot(self.arch, n);
                    t.begin(&format!("virtio_mmio@{base:x}"));
                    t.str("compatible", "virtio,mmio");
                    t.u64s("reg", &[base, size]);
                    t.u32s("interrupts", &[0, irq - 32, 1]);
                    t.empty("dma-coherent");
                    t.end();
                }
                t.begin("pl031@9010000");
                t.strs("compatible", &["arm,pl031", "arm,primecell"]);
                t.u64s("reg", &[0x0901_0000, 0x1000]);
                t.u32s("interrupts", &[0, 2, 4]);
                t.u32("clocks", 2);
                t.str("clock-names", "apb_pclk");
                t.end();
            }
        }
        t.end();
        t.finish()
    }

    /// Boot a Linux kernel `Image` the way QEMU's `-kernel/-initrd/-append` does:
    /// kernel at RAM + text_offset, device tree near the top of RAM, initrd below it.
    /// Returns the device tree that was passed.
    pub fn boot_linux(&mut self, b: &LinuxBoot) -> Result<Vec<u8>, String> {
        let base = self.arch.ram_base();
        let ram_end = base + ((self.ram_mb as u64) << 20);
        let text_offset = u64::from_le_bytes(b.kernel[8..16].try_into().unwrap());
        let kernel_addr = base + if text_offset == 0 { 0x20_0000 } else { text_offset };
        self.bus.load(kernel_addr, &b.kernel)?;

        let (dtb_addr, initrd) = match self.arch {
            Arch::Riscv64 => {
                let dtb_addr = (ram_end - 0x10_0000) & !0xffff; // 1 MiB reserved at the top
                let initrd = match &b.initrd {
                    Some(data) => {
                        let start = (dtb_addr - data.len() as u64) & !0xfff;
                        if start < kernel_addr + b.kernel.len() as u64 + 0x100_0000 {
                            return Err("initrd does not fit in RAM (try a larger -m)".into());
                        }
                        self.bus.load(start, data)?;
                        Some((start, start + data.len() as u64))
                    }
                    None => None,
                };
                (dtb_addr, initrd)
            }
            Arch::Aarch64 => {
                // like QEMU: device tree at the start of RAM, initrd after the kernel's
                // full footprint (image_size covers .bss)
                let image_size = u64::from_le_bytes(b.kernel[16..24].try_into().unwrap());
                let end = kernel_addr + image_size.max(b.kernel.len() as u64);
                let initrd = match &b.initrd {
                    Some(data) => {
                        let start = (end + 0x20_0000 + 0x1f_ffff) & !0x1f_ffff;
                        if start + data.len() as u64 + 0x10_0000 > ram_end {
                            return Err("initrd does not fit in RAM (try a larger -m)".into());
                        }
                        self.bus.load(start, data)?;
                        Some((start, start + data.len() as u64))
                    }
                    None => None,
                };
                (base, initrd)
            }
        };
        let dtb = self.device_tree(&b.cmdline, initrd);
        self.bus.load(dtb_addr, &dtb)?;

        match self.arch {
            Arch::Riscv64 => {
                let cpu = self.cpu.as_any().downcast_mut::<Riscv64>().unwrap();
                match &b.bios {
                    None => cpu.boot_supervisor(kernel_addr, dtb_addr),
                    Some(fw) => {
                        // OpenSBI fw_dynamic: a0 = hartid, a1 = dtb, a2 = &fw_dynamic_info
                        self.bus.load(base, fw)?;
                        let info = dtb_addr - 0x1000;
                        let words: [u64; 6] = [0x4942_534f, 2, kernel_addr, 1, 0, 0];
                        let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
                        self.bus.load(info, &bytes)?;
                        cpu.set_pc(base);
                        cpu.set_reg(10, 0);
                        cpu.set_reg(11, dtb_addr);
                        cpu.set_reg(12, info);
                    }
                }
            }
            Arch::Aarch64 => {
                let cpu = self.cpu.as_any().downcast_mut::<Aarch64>().unwrap();
                cpu.set_pc(kernel_addr);
                cpu.set_reg(0, dtb_addr);
            }
        }
        self.boot = Some(BootRecord::Linux(b.clone()));
        Ok(dtb)
    }

    /// Load a raw binary at `addr` and start executing there.
    pub fn load_raw(&mut self, addr: u64, data: &[u8]) -> Result<(), String> {
        self.bus.load(addr, data)?;
        self.cpu.set_pc(addr);
        self.boot = Some(BootRecord::Raw(addr, data.to_vec()));
        Ok(())
    }

    /// Load an image: ELF64 (auto-detected) or a raw binary placed at the start of RAM.
    pub fn load_image(&mut self, data: &[u8]) -> Result<(), String> {
        if data.starts_with(b"\x7fELF") {
            let entry = self.load_elf(data)?;
            self.cpu.set_pc(entry);
            self.boot = Some(BootRecord::Image(data.to_vec()));
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
            if self.bus.reset {
                return Stop::Reset;
            }
            self.bus.poll_irqs(); // pick up console input and asynchronous device work
        }
    }
}
