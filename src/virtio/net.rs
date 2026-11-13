//! virtio-net back-end (stub; the network stack arrives in v0.9).

use super::{Backend, Desc, Done};
use crate::bus::Dma;

#[derive(Clone, Default)]
pub struct NetSpec {
    pub mac: [u8; 6],
}

pub struct VirtioNet {
    spec: NetSpec,
}

impl VirtioNet {
    pub fn new(spec: NetSpec) -> Self {
        VirtioNet { spec }
    }
}

impl Backend for VirtioNet {
    fn device_id(&self) -> u32 {
        1
    }
    fn num_queues(&self) -> usize {
        2
    }
    fn config(&self) -> Vec<u8> {
        self.spec.mac.to_vec()
    }
    fn process(&mut self, q: usize, _mem: &mut Dma, _chain: &[Desc]) -> Done {
        if q == 1 { Done::Used(0) } else { Done::NotReady }
    }
}
