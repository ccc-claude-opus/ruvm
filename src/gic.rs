//! ARM GICv2 (distributor + per-CPU interfaces), up to `MAX_CPUS` CPUs and 128 interrupt
//! IDs (16 SGIs, 16 PPIs, 96 SPIs), placed like QEMU's `virt` board: distributor at `base`,
//! CPU interfaces at `base + 0x10000` (the accessing CPU is selected by the caller).
//!
//! SGIs and PPIs are banked per CPU; SPIs are shared and routed with ITARGETSR.
//! No security extensions and no virtualisation: all interrupts behave as non-secure
//! group 1 with a single enable bit per block.

use crate::devices::MAX_CPUS;

pub const NIRQ: usize = 128;
const SPURIOUS: u32 = 1023;

/// Per-CPU banked state for interrupt IDs 0..32 and the CPU interface registers.
#[derive(Clone)]
struct CpuIf {
    c_ctl: u32,
    pmr: u32,
    bpr: u32,
    enable: u32,
    /// Pending by software or an edge (PPIs; SGIs use `sgi_src`).
    pending: u32,
    active: u32,
    /// Current level of each PPI line.
    level: u32,
    prio: [u8; 32],
    /// For each SGI: the set of source CPUs with the SGI pending.
    sgi_src: [u8; 16],
}

impl Default for CpuIf {
    fn default() -> Self {
        CpuIf { c_ctl: 0, pmr: 0, bpr: 2, enable: 0, pending: 0, active: 0, level: 0, prio: [0; 32], sgi_src: [0; 16] }
    }
}

pub struct Gic {
    ncpu: usize,
    d_ctl: u32,
    cpu: Vec<CpuIf>,
    group: u128,
    // shared SPI state (bits 32..128)
    s_enable: u128,
    s_pending: u128,
    s_active: u128,
    s_level: u128,
    s_prio: [u8; NIRQ],
    s_target: [u8; NIRQ],
    /// 1 = edge-triggered (ICFGR), 0 = level-sensitive (shared for PPIs and SPIs).
    edge: u128,
    /// Output to each CPU's IRQ pin.
    pub irq_line: Vec<bool>,
}

impl Default for Gic {
    fn default() -> Self {
        Gic::new(1)
    }
}

impl Gic {
    pub fn new(ncpu: usize) -> Gic {
        let ncpu = ncpu.clamp(1, MAX_CPUS);
        Gic {
            ncpu,
            d_ctl: 0,
            cpu: vec![CpuIf::default(); ncpu],
            group: 0,
            s_enable: 0,
            s_pending: 0,
            s_active: 0,
            s_level: 0,
            s_prio: [0; NIRQ],
            // with a single CPU every SPI targets it by default (as in QEMU)
            s_target: [if ncpu == 1 { 1 } else { 0 }; NIRQ],
            edge: 0,
            irq_line: vec![false; ncpu],
        }
    }

    /// Drive a shared peripheral interrupt line (device SPIs).
    pub fn set_level(&mut self, id: u32, level: bool) {
        debug_assert!(id >= 32);
        let bit = 1u128 << id;
        let was = self.s_level & bit != 0;
        if level == was {
            return;
        }
        if level {
            self.s_level |= bit;
            if self.edge & bit != 0 {
                self.s_pending |= bit;
            }
        } else {
            self.s_level &= !bit;
        }
        self.update_all();
    }

    /// Drive a per-CPU peripheral interrupt line (PPI 16..32: the architected timers).
    pub fn set_ppi(&mut self, cpu: usize, id: u32, level: bool) {
        let bit = 1u32 << id;
        let c = &mut self.cpu[cpu];
        let was = c.level & bit != 0;
        if level == was {
            return;
        }
        if level {
            c.level |= bit;
            if self.edge >> id & 1 == 1 {
                c.pending |= bit;
            }
        } else {
            c.level &= !bit;
        }
        self.update(cpu);
    }

    fn enabled(&self, cpu: usize, id: usize) -> bool {
        if id < 32 { self.cpu[cpu].enable >> id & 1 == 1 } else { self.s_enable >> id & 1 == 1 }
    }

    fn active(&self, cpu: usize, id: usize) -> bool {
        if id < 32 { self.cpu[cpu].active >> id & 1 == 1 } else { self.s_active >> id & 1 == 1 }
    }

    fn prio(&self, cpu: usize, id: usize) -> u32 {
        (if id < 32 { self.cpu[cpu].prio[id] } else { self.s_prio[id] }) as u32
    }

    fn pending_eff(&self, cpu: usize, id: usize) -> bool {
        let edge = self.edge >> id & 1 == 1;
        if id < 16 {
            self.cpu[cpu].sgi_src[id] != 0
        } else if id < 32 {
            let c = &self.cpu[cpu];
            let level = if edge { 0 } else { c.level };
            (c.pending | level) >> id & 1 == 1
        } else {
            let (p, l) = (self.s_pending >> id & 1 == 1, self.s_level >> id & 1 == 1);
            p || (l && !edge)
        }
    }

    fn running_priority(&self, cpu: usize) -> u32 {
        (0..NIRQ).filter(|&i| self.active(cpu, i)).map(|i| self.prio(cpu, i)).min().unwrap_or(0x100)
    }

    /// Highest-priority pending, enabled, not-active interrupt that may preempt on `cpu`.
    fn highest_pending(&self, cpu: usize) -> Option<(u32, usize)> {
        let limit = self.cpu[cpu].pmr.min(self.running_priority(cpu));
        let mut best: Option<(u32, usize)> = None;
        for id in 0..NIRQ {
            if id >= 32 && self.s_target[id] >> cpu & 1 == 0 {
                continue;
            }
            if self.enabled(cpu, id) && !self.active(cpu, id) && self.pending_eff(cpu, id) {
                let p = self.prio(cpu, id);
                if p < limit && best.is_none_or(|(bp, _)| p < bp) {
                    best = Some((p, id));
                }
            }
        }
        best
    }

    fn update(&mut self, cpu: usize) {
        self.irq_line[cpu] =
            self.d_ctl & 1 == 1 && self.cpu[cpu].c_ctl & 1 == 1 && self.highest_pending(cpu).is_some();
    }

    fn update_all(&mut self) {
        for c in 0..self.ncpu {
            self.update(c);
        }
    }

    /// Read from the GIC (`cur` = the accessing CPU: selects the CPU interface and banked registers).
    pub fn read(&mut self, cur: usize, off: u64, _size: usize) -> u64 {
        if off < 0x1_0000 {
            self.dist_read(cur, off)
        } else {
            self.cpu_read(cur, off - 0x1_0000)
        }
    }

    pub fn write(&mut self, cur: usize, off: u64, size: usize, val: u64) {
        if off < 0x1_0000 {
            self.dist_write(cur, off, size, val)
        } else {
            self.cpu_write(cur, off - 0x1_0000, val)
        }
        self.update_all();
    }

    fn word(v: u128, n: u64) -> u64 {
        (v >> (32 * n)) as u32 as u64
    }

    /// 32 bits of a per-ID bit-vector register: word 0 is banked, the rest shared.
    fn bits_read(&self, cur: usize, n: u64, banked: impl Fn(&CpuIf) -> u32, shared: u128) -> u64 {
        if n == 0 { banked(&self.cpu[cur]) as u64 } else { Self::word(shared, n) }
    }

    fn pend_bits(&self, cur: usize, n: u64) -> u64 {
        let mut v = 0u64;
        for k in 0..32usize {
            if self.pending_eff(cur, 32 * n as usize + k) {
                v |= 1 << k;
            }
        }
        v
    }

    fn dist_read(&mut self, cur: usize, off: u64) -> u64 {
        match off {
            0x000 => self.d_ctl as u64,
            0x004 => 3 | ((self.ncpu as u64 - 1) << 5), // ITLinesNumber = 3 (128 IDs), CPUNumber
            0x008 => 0x0200_143b,
            0x080..=0x0ff => Self::word(self.group, (off - 0x080) / 4),
            0x100..=0x17f | 0x180..=0x1ff => {
                let n = (off & 0x7f) / 4;
                self.bits_read(cur, n, |c| c.enable, self.s_enable)
            }
            0x200..=0x27f | 0x280..=0x2ff => self.pend_bits(cur, (off & 0x7f) / 4),
            0x300..=0x37f | 0x380..=0x3ff => {
                let n = (off & 0x7f) / 4;
                self.bits_read(cur, n, |c| c.active, self.s_active)
            }
            0x400..=0x4ff => (0..4).fold(0u64, |a, k| {
                let id = (off - 0x400) as usize + k;
                a | (self.prio(cur, id) as u64) << (8 * k)
            }),
            0x800..=0x8ff => {
                // ITARGETSR: SGI/PPI read as the accessing CPU, SPIs as written
                let base = (off - 0x800) as usize;
                (0..4).fold(0u64, |a, k| {
                    let id = base + k;
                    let t = if id < 32 { 1 << cur } else { self.s_target[id] };
                    a | (t as u64) << (8 * k)
                })
            }
            0xc00..=0xc3f => {
                // ICFGR: 2 bits per ID, bit 1 of each pair = edge
                let first = ((off - 0xc00) / 4 * 16) as usize;
                (0..16).fold(0u64, |a, k| a | ((self.edge >> (first + k) & 1) as u64) << (2 * k + 1))
            }
            0xf10..=0xf1f | 0xf20..=0xf2f => {
                // CPENDSGIR / SPENDSGIR: source CPUs per SGI, 4 SGIs per word
                let first = ((off & 0xf) as usize / 4) * 4;
                (0..4).fold(0u64, |a, k| a | (self.cpu[cur].sgi_src[first + k] as u64) << (8 * k))
            }
            0xfe0 => 0x90,
            0xfe4 => 0xb4,
            0xfe8 => 0x2b, // GICv2 architecture revision
            0xff0 => 0x0d,
            0xff4 => 0xf0,
            0xff8 => 0x05,
            0xffc => 0xb1,
            _ => 0,
        }
    }

    fn dist_write(&mut self, cur: usize, off: u64, size: usize, val: u64) {
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
                if n == 0 {
                    self.cpu[cur].enable |= v32 as u32;
                } else {
                    self.s_enable |= v32 << (32 * n);
                }
            }
            0x180..=0x1ff => {
                let n = blk(off, 0x180);
                if n == 0 {
                    self.cpu[cur].enable &= !(v32 as u32);
                } else {
                    self.s_enable &= !(v32 << (32 * n));
                }
            }
            0x200..=0x27f => {
                let n = blk(off, 0x200);
                if n == 0 {
                    self.cpu[cur].pending |= (v32 as u32) & !0xffff; // SGIs are not writable here
                } else {
                    self.s_pending |= v32 << (32 * n);
                }
            }
            0x280..=0x2ff => {
                let n = blk(off, 0x280);
                if n == 0 {
                    self.cpu[cur].pending &= !((v32 as u32) & !0xffff);
                } else {
                    self.s_pending &= !(v32 << (32 * n));
                }
            }
            0x300..=0x37f => {
                let n = blk(off, 0x300);
                if n == 0 {
                    self.cpu[cur].active |= v32 as u32;
                } else {
                    self.s_active |= v32 << (32 * n);
                }
            }
            0x380..=0x3ff => {
                let n = blk(off, 0x380);
                if n == 0 {
                    self.cpu[cur].active &= !(v32 as u32);
                } else {
                    self.s_active &= !(v32 << (32 * n));
                }
            }
            0x400..=0x4ff => {
                for k in 0..size.min(4) {
                    let id = (off - 0x400) as usize + k;
                    let p = (val >> (8 * k)) as u8;
                    if id < 32 { self.cpu[cur].prio[id] = p } else { self.s_prio[id] = p }
                }
            }
            0x800..=0x8ff => {
                for k in 0..size.min(4) {
                    let id = (off - 0x800) as usize + k;
                    if id >= 32 {
                        self.s_target[id] = (val >> (8 * k)) as u8 & ((1u16 << self.ncpu) - 1) as u8;
                    }
                }
            }
            0xc00..=0xc3f => {
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
                let (filter, list, id) = (val >> 24 & 3, (val >> 16 & 0xff) as u8, (val & 15) as usize);
                let all = ((1u16 << self.ncpu) - 1) as u8;
                let targets = match filter {
                    0 => list & all,
                    1 => all & !(1 << cur),
                    2 => 1 << cur,
                    _ => 0,
                };
                for t in 0..self.ncpu {
                    if targets >> t & 1 == 1 {
                        self.cpu[t].sgi_src[id] |= 1 << cur;
                    }
                }
            }
            0xf10..=0xf1f => {
                // CPENDSGIR: clear the given source CPUs
                let first = ((off & 0xf) as usize / 4) * 4;
                for k in 0..4 {
                    self.cpu[cur].sgi_src[first + k] &= !((val >> (8 * k)) as u8);
                }
            }
            0xf20..=0xf2f => {
                let first = ((off & 0xf) as usize / 4) * 4;
                for k in 0..4 {
                    self.cpu[cur].sgi_src[first + k] |= (val >> (8 * k)) as u8;
                }
            }
            _ => {}
        }
    }

    fn cpu_read(&mut self, cur: usize, off: u64) -> u64 {
        match off {
            0x00 => self.cpu[cur].c_ctl as u64,
            0x04 => self.cpu[cur].pmr as u64,
            0x08 => self.cpu[cur].bpr as u64,
            0x0c | 0x20 => match self.highest_pending(cur) {
                // IAR / AIAR: acknowledge
                Some((_, id)) => {
                    let mut ret = id as u64;
                    if id < 16 {
                        // SGI: report (and consume) the lowest source CPU
                        let src = self.cpu[cur].sgi_src[id];
                        let s = src.trailing_zeros();
                        self.cpu[cur].sgi_src[id] &= !(1 << s);
                        ret |= (s as u64) << 10;
                        self.cpu[cur].active |= 1 << id;
                    } else if id < 32 {
                        self.cpu[cur].active |= 1 << id;
                        if self.edge >> id & 1 == 1 {
                            self.cpu[cur].pending &= !(1 << id);
                        }
                    } else {
                        self.s_active |= 1u128 << id;
                        if self.edge >> id & 1 == 1 {
                            self.s_pending &= !(1u128 << id);
                        }
                    }
                    self.update(cur);
                    ret
                }
                None => SPURIOUS as u64,
            },
            0x14 => self.running_priority(cur).min(0xff) as u64, // RPR (0xff = idle)
            0x18 | 0x28 => self.highest_pending(cur).map_or(SPURIOUS as u64, |(_, id)| id as u64),
            0x1c => 3,
            0xd0..=0xdf | 0xe0 => 0,
            0xfc => 0x0202_143b,
            _ => 0,
        }
    }

    fn cpu_write(&mut self, cur: usize, off: u64, val: u64) {
        match off {
            0x00 => self.cpu[cur].c_ctl = val as u32 & 0x201,
            0x04 => self.cpu[cur].pmr = val as u32 & 0xff,
            0x08 => self.cpu[cur].bpr = val as u32 & 7,
            0x10 | 0x24 => {
                // EOIR: priority drop + deactivate (EOImode 0)
                let id = (val & 0x3ff) as usize;
                if id < NIRQ && self.cpu[cur].c_ctl & 0x200 == 0 {
                    self.deactivate(cur, id);
                }
            }
            0x1000 => {
                let id = (val & 0x3ff) as usize; // DIR
                if id < NIRQ {
                    self.deactivate(cur, id);
                }
            }
            _ => {}
        }
    }

    fn deactivate(&mut self, cur: usize, id: usize) {
        if id < 32 {
            self.cpu[cur].active &= !(1 << id);
        } else {
            self.s_active &= !(1u128 << id);
        }
    }
}
