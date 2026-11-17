//! virtio-mmio (version 2, "modern") transport with split virtqueues, as on QEMU's
//! `virt` boards (`-global virtio-mmio.force-legacy=false`), plus device back-ends:
//! block (`blk`), entropy (`rng`) and network (`net`).
//!
//! Requests are processed synchronously when the driver writes QueueNotify, and again
//! whenever the bus polls the device (so back-ends such as the network can deliver
//! packets asynchronously by completing buffers the driver posted earlier).

pub mod blk;
pub mod net;
pub mod rng;

use crate::bus::{Device, Dma};
use std::sync::atomic::{fence, Ordering};

const MAGIC: u32 = 0x7472_6976; // "virt"
const VENDOR: u32 = 0x554d_4551; // "QEMU"
const QUEUE_MAX: u32 = 256;
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;

/// One descriptor of a chain.
#[derive(Clone, Copy, Debug)]
pub struct Desc {
    pub addr: u64,
    pub len: u32,
    /// Device-writable (guest -> driver) buffer.
    pub write: bool,
}

pub enum Done {
    /// The chain was consumed; this many bytes were written into device-writable buffers.
    Used(u32),
    /// Nothing to do yet (e.g. a receive buffer while no packet is waiting): leave it posted.
    NotReady,
}

/// A virtio device back-end.
pub trait Backend: Send {
    fn device_id(&self) -> u32;
    /// Device feature bits (VIRTIO_F_VERSION_1 is added by the transport).
    fn features(&self) -> u64 {
        0
    }
    fn num_queues(&self) -> usize {
        1
    }
    /// Device-specific configuration space.
    fn config(&self) -> Vec<u8> {
        Vec::new()
    }
    fn process(&mut self, queue: usize, mem: &mut Dma, chain: &[Desc]) -> Done;
    /// Called when the driver resets the device.
    fn reset(&mut self) {}
}

#[derive(Default, Clone, Copy)]
struct Queue {
    num: u32,
    ready: bool,
    desc: u64,
    avail: u64,
    used: u64,
    last_avail: u16,
}

pub struct VirtioMmio {
    backend: Box<dyn Backend>,
    queues: Vec<Queue>,
    queue_sel: usize,
    status: u32,
    dev_features_sel: u32,
    drv_features_sel: u32,
    drv_features: u64,
    int_status: u32,
    notified: bool,
}

impl VirtioMmio {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        let n = backend.num_queues();
        VirtioMmio {
            backend,
            queues: vec![Queue::default(); n],
            queue_sel: 0,
            status: 0,
            dev_features_sel: 0,
            drv_features_sel: 0,
            drv_features: 0,
            int_status: 0,
            notified: false,
        }
    }

    fn features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | self.backend.features()
    }

    fn reset(&mut self) {
        self.queues.iter_mut().for_each(|q| *q = Queue::default());
        self.queue_sel = 0;
        self.status = 0;
        self.drv_features = 0;
        self.int_status = 0;
        self.backend.reset();
    }

    fn q(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.queue_sel)
    }

    /// Walk the descriptor chain starting at `head`.
    fn chain(q: &Queue, mem: &Dma, head: u16) -> Option<Vec<Desc>> {
        let num = q.num as u64;
        let mut out = Vec::new();
        let mut i = head as u64;
        loop {
            let d = q.desc + 16 * (i % num);
            let (addr, len, flags, next) = (mem.r64(d)?, mem.r32(d + 8)?, mem.r16(d + 12)?, mem.r16(d + 14)?);
            out.push(Desc { addr, len, write: flags & DESC_F_WRITE != 0 });
            if flags & DESC_F_NEXT == 0 || out.len() > num as usize {
                return Some(out);
            }
            i = next as u64;
        }
    }

    /// Consume as many posted buffers as the back-end can complete.
    fn run_queues(&mut self, mem: &mut Dma) {
        for qi in 0..self.queues.len() {
            let q = self.queues[qi];
            if !q.ready || q.num == 0 {
                continue;
            }
            let num = q.num as u64;
            let mut last = q.last_avail;
            let Some(avail_idx) = mem.r16(q.avail + 2) else { continue };
            // with a hardware accelerator the driver runs concurrently on other host cores:
            // read the ring entries and descriptors only after the index that published them
            fence(Ordering::Acquire);
            while last != avail_idx {
                let Some(head) = mem.r16(q.avail + 4 + 2 * (last as u64 % num)) else { break };
                let Some(chain) = Self::chain(&q, mem, head) else { break };
                let len = match self.backend.process(qi, mem, &chain) {
                    Done::Used(n) => n,
                    Done::NotReady => break,
                };
                let Some(used_idx) = mem.r16(q.used + 2) else { break };
                let e = q.used + 4 + 8 * (used_idx as u64 % num);
                mem.w32(e, head as u32);
                mem.w32(e + 4, len);
                fence(Ordering::Release); // the data and the used element before the index
                mem.w16(q.used + 2, used_idx.wrapping_add(1));
                last = last.wrapping_add(1);
                self.int_status |= 1;
            }
            self.queues[qi].last_avail = last;
        }
    }
}

impl Device for VirtioMmio {
    fn read(&mut self, off: u64, size: usize) -> u64 {
        if off >= 0x100 {
            let cfg = self.backend.config();
            let mut v = 0u64;
            for k in 0..size {
                v |= (*cfg.get((off - 0x100) as usize + k).unwrap_or(&0) as u64) << (8 * k);
            }
            return v;
        }
        let sel = self.queue_sel;
        let q = self.queues.get(sel).copied().unwrap_or_default();
        (match off {
            0x000 => MAGIC,
            0x004 => 2, // version
            0x008 => self.backend.device_id(),
            0x00c => VENDOR,
            0x010 => (self.features() >> (32 * (self.dev_features_sel & 1))) as u32,
            0x034 => if sel < self.queues.len() { QUEUE_MAX } else { 0 },
            0x038 => q.num,
            0x044 => q.ready as u32,
            0x060 => self.int_status,
            0x070 => self.status,
            0x080 => q.desc as u32,
            0x084 => (q.desc >> 32) as u32,
            0x090 => q.avail as u32,
            0x094 => (q.avail >> 32) as u32,
            0x0a0 => q.used as u32,
            0x0a4 => (q.used >> 32) as u32,
            0x0fc => 0, // config generation
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
            0x030 => self.queue_sel = v as usize,
            0x038 => {
                if let Some(q) = self.q() {
                    q.num = (v as u32).min(QUEUE_MAX);
                }
            }
            0x044 => {
                if let Some(q) = self.q() {
                    q.ready = v & 1 == 1;
                    if !q.ready {
                        q.last_avail = 0;
                    }
                }
            }
            0x050 => self.notified = true,
            0x064 => self.int_status &= !(v as u32),
            0x070 => {
                if v == 0 {
                    self.reset();
                } else {
                    self.status = v as u32;
                }
            }
            0x080 | 0x084 | 0x090 | 0x094 | 0x0a0 | 0x0a4 => {
                if let Some(q) = self.q() {
                    let (r, hi) = match off {
                        0x080 | 0x084 => (&mut q.desc, off == 0x084),
                        0x090 | 0x094 => (&mut q.avail, off == 0x094),
                        _ => (&mut q.used, off == 0x0a4),
                    };
                    *r = if hi { (*r & 0xffff_ffff) | v << 32 } else { (*r & !0xffff_ffff) | v };
                }
            }
            _ => {}
        }
    }

    fn dma(&mut self, mem: &mut Dma) {
        if std::mem::take(&mut self.notified) {
            self.run_queues(mem);
        }
    }

    fn poll(&mut self, mem: &mut Dma) {
        if self.status & 4 != 0 {
            // DRIVER_OK: the back-end may have asynchronous work (network receive)
            self.run_queues(mem);
        }
    }

    fn irq(&mut self) -> bool {
        self.int_status != 0
    }
}
