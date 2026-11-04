//! ARM GICv2 (distributor + CPU interface), one CPU, 128 interrupt IDs
//! (16 SGIs, 16 PPIs, 96 SPIs), placed like QEMU's `virt` board:
//! distributor at `base`, CPU interface at `base + 0x10000`.
//!
//! No security extensions and no virtualisation: all interrupts behave as
//! non-secure group 1 with a single enable bit per block.

pub const NIRQ: usize = 128;
const SPURIOUS: u32 = 1023;

pub struct Gic {
    d_ctl: u32,
    c_ctl: u32,
    pmr: u32,
    bpr: u32,
    group: u128,
    enable: u128,
    /// Pending set by software or by an edge.
    pending: u128,
    active: u128,
    /// Current level of each interrupt line.
    level: u128,
    /// 1 = edge-triggered (ICFGR), 0 = level-sensitive.
    edge: u128,
    prio: [u8; NIRQ],
    target: [u8; NIRQ],
    /// Output to the CPU's IRQ pin.
    pub irq_line: bool,
}

impl Default for Gic {
    fn default() -> Self {
        Gic {
            d_ctl: 0,
            c_ctl: 0,
            pmr: 0,
            bpr: 2,
            group: 0,
            enable: 0,
            pending: 0,
            active: 0,
            level: 0,
            edge: 0,
            prio: [0; NIRQ],
            target: [0; NIRQ],
            irq_line: false,
        }
    }
}

impl Gic {
    /// Drive interrupt line `id` (PPI timer outputs, device SPIs).
    pub fn set_level(&mut self, id: u32, level: bool) {
        let bit = 1u128 << id;
        let was = self.level & bit != 0;
        if level == was {
            return;
        }
        if level {
            self.level |= bit;
            if self.edge & bit != 0 {
                self.pending |= bit; // rising edge latches
            }
        } else {
            self.level &= !bit;
        }
        self.update();
    }

    fn eff_pending(&self) -> u128 {
        self.pending | (self.level & !self.edge)
    }

    fn running_priority(&self) -> u32 {
        (0..NIRQ).filter(|&i| self.active >> i & 1 == 1).map(|i| self.prio[i] as u32).min().unwrap_or(0x100)
    }

    /// Highest-priority pending, enabled, not-active interrupt that may preempt.
    fn highest_pending(&self) -> Option<(u32, usize)> {
        let cand = self.eff_pending() & self.enable & !self.active;
        if cand == 0 {
            return None;
        }
        let limit = self.pmr.min(self.running_priority());
        let mut best: Option<(u32, usize)> = None;
        for id in 0..NIRQ {
            if cand >> id & 1 == 1 {
                let p = self.prio[id] as u32;
                if p < limit && best.is_none_or(|(bp, _)| p < bp) {
                    best = Some((p, id));
                }
            }
        }
        best
    }

    fn update(&mut self) {
        self.irq_line = self.d_ctl & 1 == 1 && self.c_ctl & 1 == 1 && self.highest_pending().is_some();
    }

    pub fn read(&mut self, off: u64, _size: usize) -> u64 {
        if off < 0x1_0000 {
            self.dist_read(off)
        } else {
            self.cpu_read(off - 0x1_0000)
        }
    }

    pub fn write(&mut self, off: u64, size: usize, val: u64) {
        if off < 0x1_0000 {
            self.dist_write(off, size, val)
        } else {
            self.cpu_write(off - 0x1_0000, val)
        }
        self.update();
    }

    fn word(v: u128, n: u64) -> u64 {
        (v >> (32 * n)) as u32 as u64
    }

    fn dist_read(&mut self, off: u64) -> u64 {
        match off {
            0x000 => self.d_ctl as u64,
            0x004 => 3, // ITLinesNumber = 3 -> 128 IDs, one CPU, no security extension
            0x008 => 0x0200_143b,
            0x080..=0x0ff => Self::word(self.group, (off - 0x080) / 4),
            0x100..=0x17f => Self::word(self.enable, (off - 0x100) / 4),
            0x180..=0x1ff => Self::word(self.enable, (off - 0x180) / 4),
            0x200..=0x27f => Self::word(self.eff_pending(), (off - 0x200) / 4),
            0x280..=0x2ff => Self::word(self.eff_pending(), (off - 0x280) / 4),
            0x300..=0x37f => Self::word(self.active, (off - 0x300) / 4),
            0x380..=0x3ff => Self::word(self.active, (off - 0x380) / 4),
            0x400..=0x4ff => self.bytes(&self.prio, off - 0x400),
            0x800..=0x8ff => {
                // ITARGETSR: SGI/PPI read as the (only) CPU, SPIs as written
                let base = (off - 0x800) as usize;
                (0..4).fold(0u64, |a, k| {
                    let id = base + k;
                    let t = if id < 32 { 1 } else { self.target[id] };
                    a | (t as u64) << (8 * k)
                })
            }
            0xc00..=0xc1f => {
                // ICFGR: 2 bits per ID, bit 1 of each pair = edge
                let first = ((off - 0xc00) / 4 * 16) as usize;
                (0..16).fold(0u64, |a, k| a | ((self.edge >> (first + k) & 1) as u64) << (2 * k + 1))
            }
            0xfe0 => 0x90,
            0xfe4 => 0xb4,
            0xfe8 => 0x2b, // GICv2 architecture revision
            0xfd0..=0xfdc | 0xfec => 0,
            0xff0 => 0x0d,
            0xff4 => 0xf0,
            0xff8 => 0x05,
            0xffc => 0xb1,
            _ => 0,
        }
    }

    fn bytes(&self, arr: &[u8; NIRQ], off: u64) -> u64 {
        (0..4).fold(0u64, |a, k| a | (arr[off as usize + k] as u64) << (8 * k))
    }

    fn dist_write(&mut self, off: u64, size: usize, val: u64) {
        let v32 = val as u32 as u128;
        let blk = |o: u64, base: u64| ((o - base) / 4) as u32;
        match off {
            0x000 => self.d_ctl = val as u32 & 1,
            0x080..=0x0ff => {
                let n = blk(off, 0x080);
                self.group = (self.group & !(0xffff_ffffu128 << (32 * n))) | v32 << (32 * n);
            }
            0x100..=0x17f => {
                let n = blk(off, 0x100);
                self.enable |= v32 << (32 * n);
                self.enable &= !0u128 >> (128 - NIRQ);
            }
            0x180..=0x1ff => self.enable &= !(v32 << (32 * blk(off, 0x180))),
            0x200..=0x27f => {
                // ISPENDR: SGIs (0..16) are not writable here
                let n = blk(off, 0x200);
                let m = if n == 0 { v32 & !0xffff } else { v32 };
                self.pending |= m << (32 * n);
            }
            0x280..=0x2ff => {
                let n = blk(off, 0x280);
                let m = if n == 0 { v32 & !0xffff } else { v32 };
                self.pending &= !(m << (32 * n));
            }
            0x300..=0x37f => self.active |= v32 << (32 * blk(off, 0x300)),
            0x380..=0x3ff => self.active &= !(v32 << (32 * blk(off, 0x380))),
            0x400..=0x4ff => {
                for k in 0..size.min(4) {
                    self.prio[(off - 0x400) as usize + k] = (val >> (8 * k)) as u8;
                }
            }
            0x800..=0x8ff => {
                for k in 0..size.min(4) {
                    let id = (off - 0x800) as usize + k;
                    if id >= 32 {
                        self.target[id] = (val >> (8 * k)) as u8 & 1;
                    }
                }
            }
            0xc00..=0xc1f => {
                let first = ((off - 0xc00) / 4 * 16) as usize;
                if first >= 32 {
                    for k in 0..16 {
                        let bit = 1u128 << (first + k);
                        if val >> (2 * k + 1) & 1 == 1 {
                            self.edge |= bit;
                        } else {
                            self.edge &= !bit;
                        }
                    }
                }
            }
            0xf00 => {
                // SGIR: [25:24] filter, [23:16] target list, [3:0] id
                let (filter, list, id) = (val >> 24 & 3, val >> 16 & 0xff, val & 15);
                if filter == 2 || (filter == 0 && list & 1 == 1) {
                    self.pending |= 1u128 << id;
                }
            }
            0xf10..=0xf1f => {
                // CPENDSGIR
                let sh = (off - 0xf10) * 8;
                self.pending &= !(((val & 0xffff_ffff) as u128) << sh & 0xffff);
            }
            0xf20..=0xf2f => {
                let sh = (off - 0xf20) * 8;
                self.pending |= ((val & 0xffff_ffff) as u128) << sh & 0xffff;
            }
            _ => {}
        }
    }

    fn cpu_read(&mut self, off: u64) -> u64 {
        match off {
            0x00 => self.c_ctl as u64,
            0x04 => self.pmr as u64,
            0x08 => self.bpr as u64,
            0x0c | 0x20 => match self.highest_pending() {
                // IAR / AIAR: acknowledge
                Some((_, id)) => {
                    let bit = 1u128 << id;
                    self.active |= bit;
                    if self.edge & bit != 0 || id < 16 {
                        self.pending &= !bit;
                    }
                    self.update();
                    id as u64
                }
                None => SPURIOUS as u64,
            },
            0x14 => self.running_priority().min(0xff) as u64, // RPR (0xff = idle)
            0x18 | 0x28 => self.highest_pending().map_or(SPURIOUS as u64, |(_, id)| id as u64),
            0x1c => 3,
            0xd0..=0xdf | 0xe0 => 0,
            0xfc => 0x0202_143b,
            _ => 0,
        }
    }

    fn cpu_write(&mut self, off: u64, val: u64) {
        match off {
            0x00 => self.c_ctl = val as u32 & 0x201,
            0x04 => self.pmr = val as u32 & 0xff,
            0x08 => self.bpr = val as u32 & 7,
            0x10 | 0x24 => {
                // EOIR: priority drop + deactivate (EOImode 0)
                let id = (val & 0x3ff) as usize;
                if id < NIRQ && self.c_ctl & 0x200 == 0 {
                    self.active &= !(1u128 << id);
                }
            }
            0x1000 => {
                let id = (val & 0x3ff) as usize; // DIR
                if id < NIRQ {
                    self.active &= !(1u128 << id);
                }
            }
            _ => {}
        }
    }
}
