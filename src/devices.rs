//! Minimal device models.

use crate::bus::Device;
use std::io::{Read, Write};
use std::sync::mpsc::{channel, Receiver};

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
}

impl Uart {
    /// `interactive`: spawn a thread that feeds host stdin to the guest.
    pub fn new(kind: UartKind, interactive: bool) -> Self {
        let rx = if interactive {
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                let mut b = [0u8; 1];
                while let Ok(1) = std::io::stdin().read(&mut b) {
                    if tx.send(b[0]).is_err() {
                        break;
                    }
                }
            });
            Some(rx)
        } else {
            None
        };
        Uart { kind, rx, pending: None }
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
}

impl Device for Uart {
    fn read(&mut self, off: u64, _size: usize) -> u64 {
        match (self.kind, off) {
            // 16550: RBR / LSR
            (UartKind::Ns16550, 0) => self.pending.take().unwrap_or(0) as u64,
            (UartKind::Ns16550, 5) => 0x60 | self.rx_ready() as u64, // THRE|TEMT|DR
            // PL011: DR / FR
            (UartKind::Pl011, 0x00) => self.pending.take().unwrap_or(0) as u64,
            (UartKind::Pl011, 0x18) => {
                if self.rx_ready() { 0x80 } else { 0x90 } // TXFE (| RXFE)
            }
            _ => 0,
        }
    }

    fn write(&mut self, off: u64, _size: usize, val: u64) {
        if off == 0 {
            self.tx(val as u8);
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
