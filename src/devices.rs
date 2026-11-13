//! Minimal device models.

use crate::bus::Device;
use std::io::{Read, Write};
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender};

/// Console input shared by the UART and the machine (so it survives a reset).
pub type SharedRx = Arc<Mutex<Receiver<u8>>>;
use std::sync::{Arc, Mutex};

/// Expect-style console input: each step types its text once the guest's console
/// output ends with the step's pattern (like `expect`/`send`).
#[derive(Default)]
pub struct ConsoleScript {
    steps: VecDeque<(Vec<u8>, Vec<u8>)>,
    seen: Vec<u8>,
    typed: VecDeque<u8>,
}

impl ConsoleScript {
    pub fn push(&mut self, wait_for: &[u8], text: &[u8]) {
        self.steps.push_back((wait_for.to_vec(), text.to_vec()));
        self.advance();
    }

    fn advance(&mut self) {
        while let Some((pat, _)) = self.steps.front() {
            if !self.seen.ends_with(pat) {
                break;
            }
            let (_, text) = self.steps.pop_front().unwrap();
            self.typed.extend(text);
            self.seen.clear();
        }
    }

    fn output(&mut self, b: u8) {
        if self.steps.is_empty() {
            return;
        }
        self.seen.push(b);
        if self.seen.len() > 4096 {
            self.seen.drain(..2048);
        }
        self.advance();
    }
}

pub type SharedScript = Arc<Mutex<ConsoleScript>>;

pub struct Uart {
    rx: Option<SharedRx>,
    script: SharedScript,
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
    /// `rx`: bytes the guest will receive (host stdin, scripted input, ...);
    /// `script`: expect-style input that waits for console output.
    pub fn new(rx: Option<SharedRx>, script: SharedScript) -> Self {
        Uart { rx, script, pending: None, ier: 0, lcr: 0, mcr: 0, scr: 0, fcr: 0, dll: 0, dlm: 0, thre: false }
    }

    fn rx_ready(&mut self) -> bool {
        if self.pending.is_none() {
            self.pending = self.script.lock().unwrap().typed.pop_front();
        }
        if self.pending.is_none() {
            if let Some(rx) = &self.rx {
                self.pending = rx.lock().unwrap().try_recv().ok();
            }
        }
        self.pending.is_some()
    }

    fn tx(&self, b: u8) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&[b]);
        let _ = out.flush();
        self.script.lock().unwrap().output(b);
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
        (match off {
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
        }) as u64
    }

    fn write(&mut self, off: u64, _size: usize, val: u64) {
        let v = val as u8;
        match off {
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
        }
    }

    fn irq(&mut self) -> bool {
        (self.ier & 1 != 0 && self.rx_ready()) || (self.ier & 2 != 0 && self.thre)
    }
}

/// ARM PrimeCell PL011 UART with FIFOs modelled as unbounded queues and QEMU's
/// interrupt behaviour (TX interrupt raised after each transmit, RX when data waits).
pub struct Pl011 {
    rx: Option<SharedRx>,
    script: SharedScript,
    pending: Option<u8>,
    ibrd: u32,
    fbrd: u32,
    lcrh: u32,
    cr: u32,
    ifls: u32,
    imsc: u32,
    /// Raw interrupt status bits other than RX/RT (which follow the receive queue).
    ris: u32,
}

const PL011_RX: u32 = 1 << 4;
const PL011_TX: u32 = 1 << 5;
const PL011_RT: u32 = 1 << 6;

impl Pl011 {
    pub fn new(rx: Option<SharedRx>, script: SharedScript) -> Self {
        Pl011 { rx, script, pending: None, ibrd: 0, fbrd: 0, lcrh: 0, cr: 0x300, ifls: 0x12, imsc: 0, ris: PL011_TX }
    }

    fn rx_ready(&mut self) -> bool {
        if self.pending.is_none() {
            self.pending = self.script.lock().unwrap().typed.pop_front();
        }
        if self.pending.is_none() {
            if let Some(rx) = &self.rx {
                self.pending = rx.lock().unwrap().try_recv().ok();
            }
        }
        self.pending.is_some()
    }

    fn raw(&mut self) -> u32 {
        let rx = if self.rx_ready() { PL011_RX | PL011_RT } else { 0 };
        self.ris | rx
    }
}

impl Device for Pl011 {
    fn read(&mut self, off: u64, _size: usize) -> u64 {
        (match off {
            0x00 => {
                self.rx_ready();
                self.pending.take().unwrap_or(0) as u32
            }
            0x04 => 0,
            0x18 => {
                // FR: TXFE always, RXFE when nothing waits
                0x90 & !(if self.rx_ready() { 0x10 } else { 0 })
            }
            0x24 => self.ibrd,
            0x28 => self.fbrd,
            0x2c => self.lcrh,
            0x30 => self.cr,
            0x34 => self.ifls,
            0x38 => self.imsc,
            0x3c => self.raw(),
            0x40 => self.raw() & self.imsc,
            0xfe0 => 0x11,
            0xfe4 => 0x10,
            0xfe8 => 0x14,
            0xfec => 0x00,
            0xff0 => 0x0d,
            0xff4 => 0xf0,
            0xff8 => 0x05,
            0xffc => 0xb1,
            _ => 0,
        }) as u64
    }

    fn write(&mut self, off: u64, _size: usize, val: u64) {
        let v = val as u32;
        match off {
            0x00 => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&[v as u8]);
                let _ = out.flush();
                self.script.lock().unwrap().output(v as u8);
                self.ris |= PL011_TX;
            }
            0x24 => self.ibrd = v & 0xffff,
            0x28 => self.fbrd = v & 0x3f,
            0x2c => self.lcrh = v & 0xff,
            0x30 => self.cr = v & 0xffff,
            0x34 => self.ifls = v & 0x3f,
            0x38 => self.imsc = v & 0x7ff,
            0x44 => self.ris &= !v,
            _ => {}
        }
    }

    fn irq(&mut self) -> bool {
        self.raw() & self.imsc != 0
    }
}

/// SiFive "test finisher" — QEMU's `virt` board uses it for power-off.
/// 0x5555 = pass, (code << 16) | 0x3333 = fail(code), 0x7777 = reset.
#[derive(Default)]
pub struct TestFinisher {
    code: Option<i32>,
    reset: bool,
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
            0x5555 => self.code = Some(0),
            0x7777 => self.reset = true,
            0x3333 => self.code = Some((val >> 16) as i32),
            _ => {}
        }
    }
    fn halt_code(&self) -> Option<i32> {
        self.code
    }
    fn reset_request(&self) -> bool {
        self.reset
    }
}

/// Wall-clock source for the RTCs: host time, or a fixed start time for reproducible runs
/// (then the clock advances with real time from that point).
pub fn rtc_now_ns(fixed_epoch: Option<u64>, started: std::time::Instant) -> u64 {
    match fixed_epoch {
        Some(s) => s * 1_000_000_000 + started.elapsed().as_nanos() as u64,
        None => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64),
    }
}

/// Goldfish RTC (RISC-V `virt`): 64-bit nanoseconds since the epoch; alarms are accepted
/// but never fire.
pub struct GoldfishRtc {
    fixed: Option<u64>,
    started: std::time::Instant,
    high: u32,
}

impl GoldfishRtc {
    pub fn new(fixed_epoch: Option<u64>) -> Self {
        GoldfishRtc { fixed: fixed_epoch, started: std::time::Instant::now(), high: 0 }
    }
}

impl Device for GoldfishRtc {
    fn read(&mut self, off: u64, _size: usize) -> u64 {
        match off {
            0x00 => {
                // reading TIME_LOW latches TIME_HIGH
                let t = rtc_now_ns(self.fixed, self.started);
                self.high = (t >> 32) as u32;
                t & 0xffff_ffff
            }
            0x04 => self.high as u64,
            _ => 0,
        }
    }
    fn write(&mut self, _off: u64, _size: usize, _val: u64) {}
}

/// ARM PrimeCell PL031 RTC: seconds counter with a load register (no alarm interrupt).
pub struct Pl031 {
    fixed: Option<u64>,
    started: std::time::Instant,
    offset: i64,
    mr: u32,
    imsc: u32,
}

impl Pl031 {
    pub fn new(fixed_epoch: Option<u64>) -> Self {
        Pl031 { fixed: fixed_epoch, started: std::time::Instant::now(), offset: 0, mr: 0, imsc: 0 }
    }
    fn secs(&self) -> i64 {
        (rtc_now_ns(self.fixed, self.started) / 1_000_000_000) as i64
    }
}

impl Device for Pl031 {
    fn read(&mut self, off: u64, _size: usize) -> u64 {
        (match off {
            0x00 => (self.secs() + self.offset) as u32,
            0x04 => self.mr,
            0x08 => (self.secs() + self.offset) as u32,
            0x0c => 1,
            0x10 => self.imsc,
            0xfe0 => 0x31,
            0xfe4 => 0x10,
            0xfe8 => 0x04,
            0xff0 => 0x0d,
            0xff4 => 0xf0,
            0xff8 => 0x05,
            0xffc => 0xb1,
            _ => 0,
        }) as u64
    }
    fn write(&mut self, off: u64, _size: usize, val: u64) {
        match off {
            0x04 => self.mr = val as u32,
            0x08 => self.offset = val as u32 as i64 - self.secs(),
            0x10 => self.imsc = val as u32 & 1,
            _ => {}
        }
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

/// Maximum number of CPUs (harts / cores) on the board.
pub const MAX_CPUS: usize = 8;

/// SiFive CLINT: `msip[hart]` @ 4*hart, `mtimecmp[hart]` @ 0x4000 + 8*hart, `mtime` @ 0xbff8.
pub struct Clint {
    pub msip: Vec<bool>,
    pub mtimecmp: Vec<u64>,
    pub mtime: u64,
}

impl Default for Clint {
    fn default() -> Self {
        Clint::new(1)
    }
}

impl Clint {
    /// Frequency the guest is told `mtime` runs at (device-tree `timebase-frequency`).
    pub const FREQ: u64 = 10_000_000;

    pub fn new(harts: usize) -> Clint {
        Clint { msip: vec![false; harts], mtimecmp: vec![u64::MAX; harts], mtime: 0 }
    }

    pub fn read(&mut self, off: u64, size: usize) -> u64 {
        match off {
            0..=0x3fff => self.msip.get((off / 4) as usize).map_or(0, |&b| b as u64),
            0x4000..=0xbff7 => {
                let h = ((off - 0x4000) / 8) as usize;
                self.mtimecmp.get(h).map_or(0, |&c| reg_read(c, off, size))
            }
            0xbff8..=0xbfff => reg_read(self.mtime, off, size),
            _ => 0,
        }
    }

    pub fn write(&mut self, off: u64, size: usize, val: u64) {
        match off {
            0..=0x3fff => {
                if let Some(b) = self.msip.get_mut((off / 4) as usize) {
                    *b = val & 1 != 0;
                }
            }
            0x4000..=0xbff7 => {
                let h = ((off - 0x4000) / 8) as usize;
                if let Some(c) = self.mtimecmp.get_mut(h) {
                    reg_write(c, off, size, val);
                }
            }
            0xbff8..=0xbfff => reg_write(&mut self.mtime, off, size, val),
            _ => {}
        }
    }

    #[inline]
    pub fn mtip(&self, hart: usize) -> bool {
        self.mtime >= self.mtimecmp[hart]
    }

    /// Earliest timer deadline of any hart (for idle fast-forwarding).
    pub fn next_deadline(&self) -> u64 {
        self.mtimecmp.iter().copied().min().unwrap_or(u64::MAX)
    }
}

/// SiFive PLIC with 127 sources and two contexts per hart (M-mode = 2*hart, S-mode = 2*hart + 1).
///   priority @ 4*src, pending @ 0x1000, enables @ 0x2000 + 0x80*ctx,
///   threshold @ 0x20_0000 + 0x1000*ctx, claim/complete @ threshold + 4.
pub struct Plic {
    priority: [u32; 128],
    pending: u128,
    claimed: u128,
    enable: Vec<u128>,
    threshold: Vec<u32>,
    /// Interrupt output of each context (MEIP / SEIP).
    pub out: Vec<bool>,
}

impl Default for Plic {
    fn default() -> Self {
        Plic::new(1)
    }
}

impl Plic {
    pub fn new(harts: usize) -> Plic {
        let n = 2 * harts;
        Plic { priority: [0; 128], pending: 0, claimed: 0, enable: vec![0; n], threshold: vec![0; n], out: vec![false; n] }
    }

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
        for ctx in 0..self.out.len() {
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
            0x2000..=0x27ff => {
                let ctx = ((off - 0x2000) / 0x80) as usize;
                let w = (off & 0x7f) / 4;
                if ctx < self.enable.len() && w < 4 { Self::word(self.enable[ctx], w) } else { 0 }
            }
            0x20_0000..=0x21_ffff => {
                let ctx = ((off - 0x20_0000) / 0x1000) as usize;
                if ctx >= self.threshold.len() {
                    return 0;
                }
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
            0x2000..=0x27ff => {
                let ctx = ((off - 0x2000) / 0x80) as usize;
                let w = (off & 0x7f) / 4;
                if ctx < self.enable.len() && w < 4 {
                    let mask = 0xffff_ffffu128 << (32 * w);
                    self.enable[ctx] = (self.enable[ctx] & !mask) | ((v as u128) << (32 * w) & mask);
                    self.enable[ctx] &= !1; // source 0 does not exist
                }
            }
            0x20_0000..=0x21_ffff => {
                let ctx = ((off - 0x20_0000) / 0x1000) as usize;
                if ctx >= self.threshold.len() {
                    return;
                }
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
