//! AArch64 stage-1 address translation for the EL1&0 regime, 4 KiB granule,
//! up to 48-bit VA / PA, with a small ASID-tagged TLB.
//!
//! Faults are reported the way Linux expects: translation / access-flag / permission
//! faults with the level encoded in DFSC/IFSC; bus errors on the final access are
//! synchronous external aborts. Hardware A/D updates (FEAT_HAFDBS) are not
//! implemented (the ID registers say so), so the kernel manages AF/DBM in software.

use super::{Aarch64, Ex, EC_DABORT_LOWER, EC_IABORT_LOWER};
use crate::bus::Bus;

pub const TLB_SIZE: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Fetch = 0,
    Read = 1,
    Write = 2,
}

#[derive(Clone, Copy, Default)]
pub struct TlbEntry {
    /// VA >> 12 (full, so TTBR0 / TTBR1 regions never alias).
    vpn: u64,
    gen: u32,
    ppn: u64,
    asid: u16,
    ng: bool,
    /// Effective AP[2:1] after hierarchical APTable, UXN, PXN, and the leaf level.
    ap: u8,
    uxn: bool,
    pxn: bool,
    level: u8,
    /// Precomputed permissions: bit `el0 * 3 + access` (Fetch = 0, Read = 1, Write = 2).
    perm: u8,
}

// SCTLR_EL1 bits
pub const SCTLR_M: u64 = 1;
pub const SCTLR_WXN: u64 = 1 << 19;
pub const SCTLR_UWXN: u64 = 1 << 20;

impl Aarch64 {
    /// Invalidate every translation.
    pub(super) fn flush_tlb(&mut self) {
        self.tlb_gen = self.tlb_gen.wrapping_add(1);
        self.fetch_vpn = 0;
    }

    /// ASID currently in effect (TCR_EL1.A1 picks which TTBR holds it).
    pub(super) fn update_asid(&mut self) {
        let src = if self.tcr >> 22 & 1 == 1 { self.ttbr1 } else { self.ttbr0 };
        self.asid = (src >> 48) as u16;
        self.fetch_vpn = 0;
    }

    fn fault(&self, acc: Access, va: u64, fsc: u32) -> Ex {
        match acc {
            Access::Fetch => Ex::sync(EC_IABORT_LOWER, fsc, Some(va), false),
            Access::Read => Ex::sync(EC_DABORT_LOWER, fsc, Some(va), false),
            Access::Write => Ex::sync(EC_DABORT_LOWER, fsc | 1 << 6, Some(va), false),
        }
    }

    fn perm_ok(&self, e: &TlbEntry, el0: bool, acc: Access) -> bool {
        let (ap_el0, ap_ro) = (e.ap & 1 == 1, e.ap & 2 == 2);
        let writable = !ap_ro;
        match acc {
            Access::Fetch => {
                if el0 {
                    ap_el0 && !e.uxn
                } else {
                    // PXN, WXN (writable -> XN) and UWXN (EL0-writable -> not executable at EL1)
                    !e.pxn
                        && !(self.sctlr & SCTLR_WXN != 0 && writable)
                        && !(self.sctlr & SCTLR_UWXN != 0 && ap_el0 && writable)
                }
            }
            Access::Read => !el0 || ap_el0,
            Access::Write => writable && (!el0 || ap_el0),
        }
    }

    /// Translate `va` for an access performed at the current EL (or as EL0 when `el0`).
    #[inline(always)]
    pub(super) fn translate(&mut self, bus: &mut Bus, va: u64, acc: Access, el0: bool) -> Result<u64, Ex> {
        if self.sctlr & SCTLR_M == 0 {
            return Ok(va & 0x0000_ffff_ffff_ffff);
        }
        let el0 = el0 || self.el == 0;
        let vpn = va >> 12;
        let e = &self.tlb[(vpn as usize) & (TLB_SIZE - 1)];
        if e.vpn == vpn && e.gen == self.tlb_gen && (!e.ng || e.asid == self.asid) {
            if e.perm >> (el0 as u8 * 3 + acc as u8) & 1 != 0 {
                return Ok(e.ppn << 12 | (va & 0xfff));
            }
            let level = e.level;
            return Err(self.fault(acc, va, 0x0c + level as u32));
        }
        self.walk(bus, va, acc, el0)
    }

    #[cold]
    #[inline(never)]
    fn walk(&mut self, bus: &mut Bus, va: u64, acc: Access, el0: bool) -> Result<u64, Ex> {
        let tcr = self.tcr;
        // select the translation table by VA[55] (top-byte-ignore aware)
        let hi = (va >> 55) & 1 == 1;
        let tbi = if hi { tcr >> 38 & 1 == 1 } else { tcr >> 37 & 1 == 1 };
        let top = if tbi { 55 } else { 63 };
        let (tsz, epd, tg, ttbr) = if hi {
            ((tcr >> 16) & 63, tcr >> 23 & 1 == 1, (tcr >> 30) & 3, self.ttbr1)
        } else {
            (tcr & 63, tcr >> 7 & 1 == 1, (tcr >> 14) & 3, self.ttbr0)
        };
        let iasize = 64 - tsz.clamp(16, 39) as u32; // 25..=48
        // bits [top:iasize] must all equal the region select bit
        let ext = (va << (63 - top)) as i64 >> (63 - top); // sign-extend from `top`
        let want = if hi { u64::MAX } else { 0 };
        let upper_ok = (ext as u64 >> iasize) == (want >> iasize);
        let granule_ok = if hi { tg == 2 } else { tg == 0 };
        if !upper_ok || epd || !granule_ok {
            return Err(self.fault(acc, va, 0x04));
        }
        // the VA with the tag byte replaced by the region's sign bits
        let canon = if hi { va | (0xffu64 << 56) } else { va & 0x00ff_ffff_ffff_ffff };

        let start = match iasize {
            ..=30 => 2,
            31..=39 => 1,
            _ => 0,
        };
        let mut level = start;
        let start_shift = 12 + 9 * (3 - start);
        let tbl_bits = (iasize - start_shift + 3).max(5); // log2(size of the first-level table)
        let mut table = ttbr & 0x0000_ffff_ffff_ffff & !((1u64 << tbl_bits) - 1);
        let (mut aptable, mut uxnt, mut pxnt) = (0u8, false, false);
        let leaf = loop {
            let shift = 12 + 9 * (3 - level);
            let mask = if level == start { (1u64 << (iasize - shift)) - 1 } else { 0x1ff };
            let idx = (canon >> shift) & mask;
            let desc = bus
                .read(table + idx * 8, 8)
                .map_err(|_| self.fault(acc, va, 0x14 + level as u32))?; // external abort on walk
            if desc & 1 == 0 {
                return Err(self.fault(acc, va, 0x04 + level as u32));
            }
            let is_table = desc & 2 != 0;
            if level == 3 {
                if !is_table {
                    return Err(self.fault(acc, va, 0x04 + level as u32));
                }
                break (desc, level);
            }
            if is_table {
                aptable |= (desc >> 61 & 3) as u8;
                uxnt |= desc >> 60 & 1 == 1;
                pxnt |= desc >> 59 & 1 == 1;
                table = desc & 0x0000_ffff_ffff_f000;
                level += 1;
                continue;
            }
            if level == 0 {
                return Err(self.fault(acc, va, 0x04)); // no level-0 blocks with 4K
            }
            break (desc, level);
        };
        let (desc, level) = leaf;
        if desc >> 10 & 1 == 0 {
            return Err(self.fault(acc, va, 0x08 + level as u32)); // access flag
        }
        let shift = 12 + 9 * (3 - level);
        let pa_base = desc & 0x0000_ffff_ffff_f000 & !((1u64 << shift) - 1);
        let pa = pa_base | (canon & ((1u64 << shift) - 1));
        let mut ap = (desc >> 6 & 3) as u8;
        if aptable & 1 == 1 {
            ap &= !1; // APTable[0]: no EL0 access
        }
        if aptable & 2 == 2 {
            ap |= 2; // APTable[1]: read-only
        }
        let e = TlbEntry {
            vpn: canon >> 12,
            gen: self.tlb_gen,
            ppn: pa >> 12,
            asid: self.asid,
            ng: desc >> 11 & 1 == 1,
            ap,
            uxn: desc >> 54 & 1 == 1 || uxnt,
            pxn: desc >> 53 & 1 == 1 || pxnt,
            level: level as u8,
            perm: 0,
        };
        let mut e = e;
        for (k, a) in [Access::Fetch, Access::Read, Access::Write].into_iter().enumerate() {
            e.perm |= (self.perm_ok(&e, false, a) as u8) << k | (self.perm_ok(&e, true, a) as u8) << (3 + k);
        }
        if !self.perm_ok(&e, el0, acc) {
            return Err(self.fault(acc, va, 0x0c + level as u32));
        }
        let slot = (e.vpn as usize) & (TLB_SIZE - 1);
        self.tlb[slot] = e;
        Ok(pa)
    }

    /// Data read of `n` bytes (1/2/4/8) at virtual address `va`.
    pub(super) fn rd(&mut self, bus: &mut Bus, va: u64, n: usize) -> Result<u64, Ex> {
        if self.sctlr & SCTLR_M == 0 {
            return bus.read(va & 0x0000_ffff_ffff_ffff, n).map_err(|_| self.abort(va, false));
        }
        if (va & 0xfff) as usize + n > 0x1000 {
            let mut v = 0;
            for k in 0..n {
                v |= self.rd(bus, va.wrapping_add(k as u64), 1)? << (8 * k);
            }
            return Ok(v);
        }
        let pa = self.translate(bus, va, Access::Read, self.unpriv)?;
        bus.read(pa, n).map_err(|_| self.abort(va, false))
    }

    /// Data write of `n` bytes (1/2/4/8) at virtual address `va`.
    pub(super) fn wr(&mut self, bus: &mut Bus, va: u64, n: usize, v: u64) -> Result<(), Ex> {
        if self.sctlr & SCTLR_M == 0 {
            return bus.write(va & 0x0000_ffff_ffff_ffff, n, v).map_err(|_| self.abort(va, true));
        }
        if (va & 0xfff) as usize + n > 0x1000 {
            // translate every byte first so a fault leaves memory untouched
            let mut pas = [0u64; 8];
            for (k, pa) in pas.iter_mut().enumerate().take(n) {
                *pa = self.translate(bus, va.wrapping_add(k as u64), Access::Write, self.unpriv)?;
            }
            for (k, pa) in pas.iter().enumerate().take(n) {
                bus.write(*pa, 1, v >> (8 * k)).map_err(|_| self.abort(va, true))?;
            }
            return Ok(());
        }
        let pa = self.translate(bus, va, Access::Write, self.unpriv)?;
        bus.write(pa, n, v).map_err(|_| self.abort(va, true))
    }

    /// Fetch the instruction word at pc.
    pub(super) fn fetch(&mut self, bus: &mut Bus) -> Result<u32, Ex> {
        let pc = self.pc;
        let po = (pc & 0xfff) as usize;
        if self.fetch_vpn == (pc >> 12) + 1 && self.fetch_el == self.el {
            let off = self.fetch_off + po;
            return Ok(u32::from_le_bytes(bus.ram()[off..off + 4].try_into().unwrap()));
        }
        let pa = self.translate(bus, pc, Access::Fetch, false)?;
        let w = bus.read(pa, 4).map_err(|_| Ex::sync(EC_IABORT_LOWER, 0x10, Some(pc), false))?;
        if let Some(off) = bus.ram_off(pa & !0xfff, 0x1000) {
            self.fetch_vpn = (pc >> 12) + 1;
            self.fetch_off = off;
            self.fetch_el = self.el;
        }
        Ok(w as u32)
    }

    /// DC ZVA: zero the 64-byte block containing `va`.
    pub(super) fn dc_zva(&mut self, bus: &mut Bus, va: u64) -> Result<(), Ex> {
        let base = va & !63;
        let pa = self.translate(bus, base, Access::Write, false)?;
        for k in 0..8 {
            bus.write(pa + 8 * k, 8, 0).map_err(|_| self.abort(base, true))?;
        }
        Ok(())
    }
}
