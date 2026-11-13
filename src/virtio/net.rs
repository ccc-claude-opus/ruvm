//! virtio-net back-end on top of the user-mode network stack (`crate::net`).
//!
//! Features: MAC + STATUS only (no checksum/TSO offload, so frames are plain MTU-sized
//! Ethernet frames). With VIRTIO_F_VERSION_1 every packet carries a 12-byte header.

use super::{Backend, Desc, Done};
use crate::bus::Dma;
use crate::net::{NetConfig, UserNet};

const F_MAC: u64 = 1 << 5;
const F_STATUS: u64 = 1 << 16;
const HDR: usize = 12;

#[derive(Clone)]
pub struct NetSpec {
    pub cfg: NetConfig,
}

pub struct VirtioNet {
    mac: [u8; 6],
    net: UserNet,
}

impl VirtioNet {
    pub fn new(spec: NetSpec) -> Self {
        VirtioNet { mac: spec.cfg.mac, net: UserNet::new(spec.cfg) }
    }
}

impl Backend for VirtioNet {
    fn device_id(&self) -> u32 {
        1
    }
    fn features(&self) -> u64 {
        F_MAC | F_STATUS
    }
    fn num_queues(&self) -> usize {
        2
    }
    fn config(&self) -> Vec<u8> {
        let mut c = self.mac.to_vec();
        c.extend_from_slice(&1u16.to_le_bytes()); // status: link up
        c
    }

    fn process(&mut self, q: usize, mem: &mut Dma, chain: &[Desc]) -> Done {
        if q == 1 {
            // transmit: gather the guest's buffers, drop the virtio-net header
            let mut frame = Vec::new();
            for d in chain.iter().filter(|d| !d.write) {
                if let Some(b) = mem.read(d.addr, d.len as usize) {
                    frame.extend_from_slice(b);
                }
            }
            if frame.len() > HDR {
                self.net.guest_tx(&frame[HDR..]);
            }
            return Done::Used(0);
        }
        // receive: fill the posted buffer if a frame is waiting
        let Some(frame) = self.net.next_frame() else { return Done::NotReady };
        let mut data = vec![0u8; HDR];
        data[10] = 1; // num_buffers = 1
        data.extend_from_slice(&frame);
        let mut off = 0;
        for d in chain.iter().filter(|d| d.write) {
            if off >= data.len() {
                break;
            }
            let n = (d.len as usize).min(data.len() - off);
            if mem.write(d.addr, &data[off..off + n]).is_none() {
                break;
            }
            off += n;
        }
        Done::Used(off as u32)
    }
}
