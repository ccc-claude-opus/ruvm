//! Minimal device models.

use crate::bus::Device;
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, Sender};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UartKind {
    /// NS16550A, as found on the RISC-V `virt` board.
    Ns16550,
    /// ARM PrimeCell PL011, as found on the ARM `virt` board.
    Pl011,
}

pub struct Uart {
    kind: UartKind,
    rx: Option<Receiver<u8>>,
    pending: Option<u8>,
    // 16550 registers
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    fcr: u8,
    dll: u8,
    dlm: u8,
    /// "THR empty" interrupt condition (cleared by reading IIR, re-armed by a THR write).
    thre: bool,
}

impl Uart {
    /// `rx`: bytes the guest will receive (host stdin, scripted input, ...).
    pub fn new(kind: UartKind, rx: Option<Receiver<u8>>) -> Self {
        Uart { kind, rx, pending: None, ier: 0, lcr: 0, mcr: 0, scr: 0, fcr: 0, dll: 0, dlm: 0, thre: false }
    }

    fn rx_ready(&mut self) -> bool {
        if self.pending.is_none() {
            if let Some(rx) = &self.rx {
                self.pending = rx.try_recv().ok();
            }
        }
        self.pending.is_some()
    }

    fn tx(&self, b: u8) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&[b]);
        let _ = out.flush();
    }

    fn dlab(&self) -> bool {
        self.lcr & 0x80 != 0
    }
}

/// Spawn a thread that forwards host stdin to `tx`.
pub fn forward_stdin(tx: Sender<u8>) {
    std::thread::spawn(move || {
        let mut b = [0u8; 1];
        while let Ok(1) = std::io::stdin().read(&mut b) {
            if tx.send(b[0]).is_err() {
                break;
            }
        }
    });
}

impl Device for Uart {
    fn read(&mut self, off: u64, _size: usize) -> u64 {
        match self.kind {
            UartKind::Ns16550 => (match off {
                0 if self.dlab() => self.dll,
                0 => {
                    self.rx_ready();
                    self.pending.take().unwrap_or(0)
                }
                1 if self.dlab() => self.dlm,
                1 => self.ier,
                2 => {
                    // IIR: FIFOs enabled (0xc0) | interrupt id
                    let fifo = if self.fcr & 1 != 0 { 0xc0 } else { 0 };
                    fifo | if self.ier & 1 != 0 && self.rx_ready() {
                        0x04
                    } else if self.ier & 2 != 0 && self.thre {
                        self.thre = false;
                        0x02
                    } else {
                        0x01
                    }
                }
                3 => self.lcr,
                4 => self.mcr,
                5 => 0x60 | self.rx_ready() as u8, // THRE | TEMT | DR
                6 => 0xb0,                         // MSR: DCD | DSR | CTS
                7 => self.scr,
                _ => 0,
            }) as u64,
            // PL011: DR / FR
            UartKind::Pl011 => match off {
                0x00 => {
                    self.rx_ready();
                    self.pending.take().unwrap_or(0) as u64
                }
                0x18 => {
                    if self.rx_ready() { 0x80 } else { 0x90 } // TXFE (| RXFE)
                }
                _ => 0,
            },
        }
    }

    fn write(&mut self, off: u64, _size: usize, val: u64) {
        let v = val as u8;
        match self.kind {
            UartKind::Ns16550 => match off {
                0 if self.dlab() => self.dll = v,
                0 => {
                    self.tx(v);
                    self.thre = true;
                }
                1 if self.dlab() => self.dlm = v,
                1 => {
                    if v & 2 != 0 && self.ier & 2 == 0 {
                        self.thre = true; // enabling ETBEI with an empty THR raises it at once
                    }
                    self.ier = v & 0x0f;
                }
                2 => self.fcr = v & 0xc9,
                3 => self.lcr = v,
                4 => self.mcr = v,
                7 => self.scr = v,
                _ => {}
            },
            UartKind::Pl011 => {
                if off == 0 {
                    self.tx(v);
                }
            }
        }
    }

    fn irq(&mut self) -> bool {
        match self.kind {
            UartKind::Ns16550 => (self.ier & 1 != 0 && self.rx_ready()) || (self.ier & 2 != 0 && self.thre),
            UartKind::Pl011 => false,
        }
    }
}

/// SiFive "test finisher" — QEMU's `virt` board uses it for power-off.
/// 0x5555 = pass, (code << 16) | 0x3333 = fail(code), 0x7777 = reset.
#[derive(Default)]
pub struct TestFinisher {
    code: Option<i32>,
}

impl Device for TestFinisher {
    fn read(&mut self, _off: u64, _size: usize) -> u64 {
        0
    }
    fn write(&mut self, off: u64, _size: usize, val: u64) {
        if off != 0 {
            return;
        }
        match val & 0xffff {
            0x5555 | 0x7777 => self.code = Some(0),
            0x3333 => self.code = Some((val >> 16) as i32),
            _ => {}
        }
    }
    fn halt_code(&self) -> Option<i32> {
        self.code
    }
}

/// Read `size` bytes at byte offset `off` (relative to the start of a 64-bit register).
fn reg_read(reg: u64, off: u64, size: usize) -> u64 {
    let v = reg >> ((off & 7) * 8);
    if size == 8 { v } else { v & ((1u64 << (size * 8)) - 1) }
}

fn reg_write(reg: &mut u64, off: u64, size: usize, val: u64) {
    let sh = (off & 7) * 8;
    let mask = if size == 8 { !0 } else { ((1u64 << (size * 8)) - 1) << sh };
    *reg = (*reg & !mask) | ((val << sh) & mask);
}

/// SiFive CLINT (one hart): `msip` @0, `mtimecmp` @0x4000, `mtime` @0xbff8.
pub struct Clint {
    pub msip: bool,
    pub mtimecmp: u64,
    pub mtime: u64,
}

impl Default for Clint {
    fn default() -> Self {
        Clint { msip: false, mtimecmp: u64::MAX, mtime: 0 }
    }
}

impl Clint {
    /// Frequency the guest is told `mtime` runs at (device-tree `timebase-frequency`).
    pub const FREQ: u64 = 10_000_000;

    pub fn read(&mut self, off: u64, size: usize) -> u64 {
        match off {
            0..=3 => self.msip as u64,
            0x4000..=0x4007 => reg_read(self.mtimecmp, off, size),
            0xbff8..=0xbfff => reg_read(self.mtime, off, size),
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, size: usize, val: u64) {
        match off {
            0 => self.msip = val & 1 != 0,
            0x4000..=0x4007 => reg_write(&mut self.mtimecmp, off, size, val),
            0xbff8..=0xbfff => reg_write(&mut self.mtime, off, size, val),
            _ => {}
        }
    }

    #[inline]
    pub fn mtip(&self) -> bool {
        self.mtime >= self.mtimecmp
    }
}

/// SiFive PLIC with 127 sources and two contexts (hart 0 M-mode = 0, S-mode = 1).
///   priority @ 4*src, pending @ 0x1000, enables @ 0x2000 + 0x80*ctx,
///   threshold @ 0x20_0000 + 0x1000*ctx, claim/complete @ threshold + 4.
pub struct Plic {
    priority: [u32; 128],
    pending: u128,
    claimed: u128,
    enable: [u128; 2],
    threshold: [u32; 2],
    /// Interrupt output of each context (MEIP / SEIP).
    pub out: [bool; 2],
}

impl Default for Plic {
    fn default() -> Self {
        Plic { priority: [0; 128], pending: 0, claimed: 0, enable: [0; 2], threshold: [0; 2], out: [false; 2] }
    }
}

impl Plic {
    /// Sample a level-triggered source. As in the PLIC gateway (and QEMU), a high
    /// level latches the pending bit; only a claim clears it.
    pub fn set_level(&mut self, src: u32, level: bool) {
        if level {
            self.pending |= 1u128 << src;
            self.update();
        }
    }

    /// Highest-priority pending, enabled, unclaimed source above the threshold.
    fn best(&self, ctx: usize) -> u32 {
        let cand = self.pending & self.enable[ctx] & !self.claimed;
        let (mut best, mut prio) = (0, self.threshold[ctx]);
        for s in 1..128 {
            if cand >> s & 1 != 0 && self.priority[s] > prio {
                best = s as u32;
                prio = self.priority[s];
            }
        }
        best
    }

    fn update(&mut self) {
        for ctx in 0..2 {
            self.out[ctx] = self.best(ctx) != 0;
        }
    }

    fn word(v: u128, w: u64) -> u64 {
        (v >> (32 * w)) as u32 as u64
    }

    pub fn read(&mut self, off: u64, _size: usize) -> u64 {
        match off {
            0..=0x1ff => self.priority[(off / 4) as usize] as u64,
            0x1000..=0x100f => Self::word(self.pending, (off - 0x1000) / 4),
            0x2000..=0x20ff => {
                let ctx = ((off - 0x2000) / 0x80) as usize;
                let w = (off & 0x7f) / 4;
                if ctx < 2 && w < 4 { Self::word(self.enable[ctx], w) } else { 0 }
            }
            0x20_0000..=0x20_1fff => {
                let ctx = ((off - 0x20_0000) / 0x1000) as usize;
                match off & 0xfff {
                    0 => self.threshold[ctx] as u64,
                    4 => {
                        let s = self.best(ctx);
                        if s != 0 {
                            self.claimed |= 1 << s;
                            self.pending &= !(1 << s);
                            self.update();
                        }
                        s as u64
                    }
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, _size: usize, val: u64) {
        let v = val as u32;
        match off {
            0..=0x1ff => self.priority[(off / 4) as usize] = v & 7,
            0x2000..=0x20ff => {
                let ctx = ((off - 0x2000) / 0x80) as usize;
                let w = (off & 0x7f) / 4;
                if ctx < 2 && w < 4 {
                    let mask = 0xffff_ffffu128 << (32 * w);
                    self.enable[ctx] = (self.enable[ctx] & !mask) | ((v as u128) << (32 * w) & mask);
                    self.enable[ctx] &= !1; // source 0 does not exist
                }
            }
            0x20_0000..=0x20_1fff => {
                let ctx = ((off - 0x20_0000) / 0x1000) as usize;
                match off & 0xfff {
                    0 => self.threshold[ctx] = v & 7,
                    4 if v < 128 => self.claimed &= !(1u128 << v), // complete
                    _ => {}
                }
            }
            _ => {}
        }
        self.update();
    }
}
