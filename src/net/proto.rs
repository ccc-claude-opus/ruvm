//! Ethernet / IPv4 / UDP / TCP encoding and parsing for the user-mode network stack.

pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;

/// Internet checksum (one's complement of the one's-complement sum), continuing from `init`.
pub fn checksum(data: &[u8], init: u32) -> u16 {
    let mut sum = init;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [b] = chunks.remainder() {
        sum += (*b as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn pseudo(src: [u8; 4], dst: [u8; 4], proto: u8, len: usize) -> u32 {
    let w = |a: [u8; 4]| (u16::from_be_bytes([a[0], a[1]]) as u32) + (u16::from_be_bytes([a[2], a[3]]) as u32);
    w(src) + w(dst) + proto as u32 + len as u32
}

pub struct Eth<'a> {
    pub dst: [u8; 6],
    pub src: [u8; 6],
    pub ethertype: u16,
    pub payload: &'a [u8],
}

impl<'a> Eth<'a> {
    pub fn parse(f: &'a [u8]) -> Option<Eth<'a>> {
        if f.len() < 14 {
            return None;
        }
        Some(Eth {
            dst: f[0..6].try_into().ok()?,
            src: f[6..12].try_into().ok()?,
            ethertype: u16::from_be_bytes([f[12], f[13]]),
            payload: &f[14..],
        })
    }
}

pub fn eth_frame(dst: [u8; 6], src: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(14 + payload.len().max(46));
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    if f.len() < 60 {
        f.resize(60, 0); // minimum Ethernet frame (without FCS)
    }
    f
}

pub struct Ip<'a> {
    pub src: [u8; 4],
    pub dst: [u8; 4],
    pub proto: u8,
    pub payload: &'a [u8],
}

impl<'a> Ip<'a> {
    pub fn parse(p: &'a [u8]) -> Option<Ip<'a>> {
        if p.len() < 20 || p[0] >> 4 != 4 {
            return None;
        }
        let ihl = (p[0] & 15) as usize * 4;
        let total = u16::from_be_bytes([p[2], p[3]]) as usize;
        if ihl < 20 || total < ihl || p.len() < total {
            return None;
        }
        if u16::from_be_bytes([p[6], p[7]]) & 0x3fff != 0 {
            return None; // fragments are not supported
        }
        Some(Ip { src: p[12..16].try_into().ok()?, dst: p[16..20].try_into().ok()?, proto: p[9], payload: &p[ihl..total] })
    }
}

pub fn ip_packet(src: [u8; 4], dst: [u8; 4], proto: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(20 + payload.len());
    p.extend_from_slice(&[0x45, 0]);
    p.extend_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0, 0x40, 0, 64, proto, 0, 0]); // id, DF, ttl, proto, checksum
    p.extend_from_slice(&src);
    p.extend_from_slice(&dst);
    let c = checksum(&p, 0);
    p[10..12].copy_from_slice(&c.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

pub struct Udp<'a> {
    pub sport: u16,
    pub dport: u16,
    pub payload: &'a [u8],
}

impl<'a> Udp<'a> {
    pub fn parse(p: &'a [u8]) -> Option<Udp<'a>> {
        if p.len() < 8 {
            return None;
        }
        let len = (u16::from_be_bytes([p[4], p[5]]) as usize).clamp(8, p.len());
        Some(Udp { sport: u16::from_be_bytes([p[0], p[1]]), dport: u16::from_be_bytes([p[2], p[3]]), payload: &p[8..len] })
    }
}

/// UDP datagram with a zero checksum (legal for IPv4).
pub fn udp_datagram(sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let mut u = Vec::with_capacity(8 + payload.len());
    u.extend_from_slice(&sport.to_be_bytes());
    u.extend_from_slice(&dport.to_be_bytes());
    u.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    u.extend_from_slice(&[0, 0]);
    u.extend_from_slice(payload);
    u
}

pub fn udp_datagram_ip(sport: u16, dport: u16, payload: &[u8], _src: [u8; 4], _dst: [u8; 4]) -> Vec<u8> {
    udp_datagram(sport, dport, payload)
}

pub struct TcpHdr<'a> {
    pub sport: u16,
    pub dport: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub payload: &'a [u8],
}

impl<'a> TcpHdr<'a> {
    pub fn parse(p: &'a [u8]) -> Option<TcpHdr<'a>> {
        if p.len() < 20 {
            return None;
        }
        let off = (p[12] >> 4) as usize * 4;
        if off < 20 || p.len() < off {
            return None;
        }
        Some(TcpHdr {
            sport: u16::from_be_bytes([p[0], p[1]]),
            dport: u16::from_be_bytes([p[2], p[3]]),
            seq: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
            ack: u32::from_be_bytes([p[8], p[9], p[10], p[11]]),
            flags: p[13],
            window: u16::from_be_bytes([p[14], p[15]]),
            payload: &p[off..],
        })
    }
}

/// A TCP segment from `src` (port `sport`) to `dst`; `mss` adds the MSS option (SYN only).
#[allow(clippy::too_many_arguments)]
pub fn tcp_segment(
    sport: u16,
    dport: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    payload: &[u8],
    mss: bool,
    src: [u8; 4],
    dst: [u8; 4],
) -> Vec<u8> {
    let hdr = if mss { 24 } else { 20 };
    let mut t = Vec::with_capacity(hdr + payload.len());
    t.extend_from_slice(&sport.to_be_bytes());
    t.extend_from_slice(&dport.to_be_bytes());
    t.extend_from_slice(&seq.to_be_bytes());
    t.extend_from_slice(&ack.to_be_bytes());
    t.push((hdr as u8 / 4) << 4);
    t.push(flags);
    t.extend_from_slice(&window.to_be_bytes());
    t.extend_from_slice(&[0, 0, 0, 0]); // checksum, urgent
    if mss {
        t.extend_from_slice(&[2, 4, (1460u16 >> 8) as u8, 1460u16 as u8]);
    }
    t.extend_from_slice(payload);
    let c = checksum(&t, pseudo(src, dst, 6, t.len()));
    t[16..18].copy_from_slice(&c.to_be_bytes());
    t
}
