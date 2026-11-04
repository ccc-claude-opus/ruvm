//! virtio-mmio (version 2, "modern") block device, as on QEMU's `virt` board
//! (`-global virtio-mmio.force-legacy=false -device virtio-blk-device`).
//!
//! One virtqueue (split ring). Requests are processed synchronously when the
//! driver writes QueueNotify; the device then raises its interrupt line.

use crate::bus::{Device, Dma};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

const MAGIC: u32 = 0x7472_6976; // "virt"
const VENDOR: u32 = 0x554d_4551; // "QEMU"
const QUEUE_MAX: u32 = 256;
const SECTOR: u64 = 512;

const VIRTIO_F_VERSION_1: u64 = 1 << 32;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

const DESC_F_NEXT: u16 = 1;

const BLK_T_IN: u32 = 0;
const BLK_T_OUT: u32 = 1;
const BLK_T_FLUSH: u32 = 4;
const BLK_T_GET_ID: u32 = 8;
const BLK_S_OK: u8 = 0;
const BLK_S_IOERR: u8 = 1;
const BLK_S_UNSUPP: u8 = 2;

pub struct VirtioBlk {
    disk: Vec<u8>,
    file: Option<File>, // write-through backing file
    status: u32,
    dev_features_sel: u32,
    drv_features_sel: u32,
    drv_features: u64,
    queue_num: u32,
    queue_ready: bool,
    desc: u64,
    avail: u64,
    used: u64,
    last_avail: u16,
    int_status: u32,
    notified: bool,
}

impl VirtioBlk {
    /// Open a raw disk image. Guest writes go straight back to the file.
    pub fn open(path: &str) -> Result<VirtioBlk, String> {
        let mut f = File::options().read(true).write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
        let mut disk = Vec::new();
        f.read_to_end(&mut disk).map_err(|e| format!("{path}: {e}"))?;
        disk.resize(disk.len().div_ceil(SECTOR as usize) * SECTOR as usize, 0);
        Ok(Self::with_image(disk, Some(f)))
    }

    /// An in-memory disk (writes are not persisted).
    pub fn with_image(disk: Vec<u8>, file: Option<File>) -> VirtioBlk {
        VirtioBlk {
            disk,
            file,
            status: 0,
            dev_features_sel: 0,
            drv_features_sel: 0,
            drv_features: 0,
            queue_num: 0,
            queue_ready: false,
            desc: 0,
            avail: 0,
            used: 0,
            last_avail: 0,
            int_status: 0,
            notified: false,
        }
    }

    fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_BLK_F_FLUSH
    }

    fn reset(&mut self) {
        let (disk, file) = (std::mem::take(&mut self.disk), self.file.take());
        *self = Self::with_image(disk, file);
    }

    fn set_lo(r: &mut u64, v: u64) {
        *r = (*r & !0xffff_ffff) | (v & 0xffff_ffff);
    }
    fn set_hi(r: &mut u64, v: u64) {
        *r = (*r & 0xffff_ffff) | (v << 32);
    }

    /// Execute one request (descriptor chain starting at `head`); returns bytes written.
    fn request(&mut self, mem: &mut Dma, head: u16) -> Option<u32> {
        let num = self.queue_num as u64;
        // collect the chain: (addr, len, flags)
        let mut chain = Vec::new();
        let mut i = head as u64;
        loop {
            let d = self.desc + 16 * (i % num);
            let (addr, len, flags, next) = (mem.r64(d)?, mem.r32(d + 8)?, mem.r16(d + 12)?, mem.r16(d + 14)?);
            chain.push((addr, len as u64, flags));
            if flags & DESC_F_NEXT == 0 || chain.len() > num as usize {
                break;
            }
            i = next as u64;
        }
        if chain.len() < 2 {
            return None;
        }
        let (hdr, _, _) = chain[0];
        let (typ, sector) = (mem.r32(hdr)?, mem.r64(hdr + 8)?);
        let (status_addr, _, _) = *chain.last().unwrap();
        let data = &chain[1..chain.len() - 1];

        let mut pos = sector * SECTOR;
        let mut written = 1u32; // the status byte
        let mut st = BLK_S_OK;
        match typ {
            BLK_T_IN | BLK_T_OUT => {
                for &(addr, len, _) in data {
                    let end = pos + len;
                    if end > self.disk.len() as u64 {
                        st = BLK_S_IOERR;
                        break;
                    }
                    let range = pos as usize..end as usize;
                    if typ == BLK_T_IN {
                        mem.write(addr, &self.disk[range])?;
                        written += len as u32;
                    } else {
                        let buf = mem.read(addr, len as usize)?;
                        self.disk[range].copy_from_slice(buf);
                        if let Some(f) = &mut self.file {
                            if f.seek(SeekFrom::Start(pos)).and_then(|_| f.write_all(buf)).is_err() {
                                st = BLK_S_IOERR;
                            }
                        }
                    }
                    pos = end;
                }
            }
            BLK_T_FLUSH => {
                if let Some(f) = &mut self.file {
                    let _ = f.flush();
                }
            }
            BLK_T_GET_ID => {
                if let Some(&(addr, len, _)) = data.first() {
                    let id = *b"ruvm-virtio-blk\0\0\0\0\0";
                    let n = (len as usize).min(id.len());
                    mem.write(addr, &id[..n])?;
                    written += n as u32;
                }
            }
            _ => st = BLK_S_UNSUPP,
        }
        mem.write(status_addr, &[st])?;
        Some(written)
    }

    fn process(&mut self, mem: &mut Dma) {
        if !self.queue_ready || self.queue_num == 0 {
            return;
        }
        let num = self.queue_num as u64;
        let Some(avail_idx) = mem.r16(self.avail + 2) else { return };
        while self.last_avail != avail_idx {
            let Some(head) = mem.r16(self.avail + 4 + 2 * (self.last_avail as u64 % num)) else { return };
            let len = self.request(mem, head).unwrap_or(0);
            let Some(used_idx) = mem.r16(self.used + 2) else { return };
            let e = self.used + 4 + 8 * (used_idx as u64 % num);
            mem.w32(e, head as u32);
            mem.w32(e + 4, len);
            mem.w16(self.used + 2, used_idx.wrapping_add(1));
            self.last_avail = self.last_avail.wrapping_add(1);
            self.int_status |= 1;
        }
    }
}

impl Device for VirtioBlk {
    fn read(&mut self, off: u64, _size: usize) -> u64 {
        (match off {
            0x000 => MAGIC,
            0x004 => 2, // version
            0x008 => 2, // device id: block
            0x00c => VENDOR,
            0x010 => (self.features() >> (32 * (self.dev_features_sel & 1))) as u32,
            0x034 => QUEUE_MAX,
            0x038 => self.queue_num,
            0x044 => self.queue_ready as u32,
            0x060 => self.int_status,
            0x070 => self.status,
            0x080 => self.desc as u32,
            0x084 => (self.desc >> 32) as u32,
            0x090 => self.avail as u32,
            0x094 => (self.avail >> 32) as u32,
            0x0a0 => self.used as u32,
            0x0a4 => (self.used >> 32) as u32,
            0x0fc => 0, // config generation
            // config space: capacity (u64, sectors), then blk_size etc.
            0x100..=0x107 => {
                let cap = self.disk.len() as u64 / SECTOR;
                return if off == 0x100 && _size == 8 { cap } else { (cap >> (8 * (off - 0x100))) & 0xffff_ffff };
            }
            _ => 0,
        }) as u64
    }

    fn write(&mut self, off: u64, _size: usize, val: u64) {
        let v = val & 0xffff_ffff;
        match off {
            0x014 => self.dev_features_sel = v as u32,
            0x020 => {
                let sh = 32 * (self.drv_features_sel & 1);
                self.drv_features = (self.drv_features & !(0xffff_ffffu64 << sh)) | (v << sh);
            }
            0x024 => self.drv_features_sel = v as u32,
            0x030 => {} // queue select: only queue 0 exists
            0x038 => self.queue_num = (v as u32).min(QUEUE_MAX),
            0x044 => self.queue_ready = v & 1 == 1,
            0x050 => self.notified = true,
            0x064 => self.int_status &= !(v as u32),
            0x070 => {
                if v == 0 {
                    self.reset();
                } else {
                    self.status = v as u32;
                }
            }
            0x080 => Self::set_lo(&mut self.desc, v),
            0x084 => Self::set_hi(&mut self.desc, v),
            0x090 => Self::set_lo(&mut self.avail, v),
            0x094 => Self::set_hi(&mut self.avail, v),
            0x0a0 => Self::set_lo(&mut self.used, v),
            0x0a4 => Self::set_hi(&mut self.used, v),
            _ => {}
        }
    }

    fn dma(&mut self, mem: &mut Dma) {
        if std::mem::take(&mut self.notified) {
            self.process(mem);
        }
    }

    fn irq(&mut self) -> bool {
        self.int_status != 0
    }
}
