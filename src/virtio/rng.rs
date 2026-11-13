//! virtio-rng back-end: fills the guest's buffers with random bytes (xorshift64*,
//! seeded from the host unless a fixed seed is given for reproducible runs).

use super::{Backend, Desc, Done};
use crate::bus::Dma;

pub struct VirtioRng {
    state: u64,
}

impl VirtioRng {
    pub fn new(seed: Option<u64>) -> Self {
        let seed = seed.unwrap_or_else(|| {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
            t ^ (std::process::id() as u64) << 32
        });
        VirtioRng { state: seed | 1 }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

impl Backend for VirtioRng {
    fn device_id(&self) -> u32 {
        4
    }

    fn process(&mut self, _q: usize, mem: &mut Dma, chain: &[Desc]) -> Done {
        let mut total = 0;
        for d in chain.iter().filter(|d| d.write) {
            let bytes: Vec<u8> = (0..d.len).map(|_| self.next() as u8).collect();
            if mem.write(d.addr, &bytes).is_some() {
                total += d.len;
            }
        }
        Done::Used(total)
    }
}
