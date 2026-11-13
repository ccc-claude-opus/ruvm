//! A user-mode network stack ("slirp-lite") for the virtio-net device: the guest sits on
//! 10.0.2.0/24 behind a virtual gateway (10.0.2.2, which is also the *host* for
//! connections made to it) and a virtual DNS server (10.0.2.3). No privileges needed.
//!
//! Supported: ARP, a DHCP server (guest = 10.0.2.15), ICMP echo to the gateway / DNS,
//! UDP (NAT through host sockets; DNS is forwarded to the host's resolver) and TCP
//! (every guest connection becomes a host `TcpStream`, with a real TCP state machine
//! toward the guest), plus host-to-guest TCP port forwarding (`hostfwd`).
//!
//! The stack is a pure state machine driven by `guest_tx` (frames from the guest) and
//! `next_frame` (frames for the guest); host sockets run in helper threads that talk to it
//! over channels, so all protocol logic runs on the emulator thread.

pub mod proto;

use proto::*;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const GW_IP: [u8; 4] = [10, 0, 2, 2];
pub const DNS_IP: [u8; 4] = [10, 0, 2, 3];
pub const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
pub const GW_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];
const MSS: usize = 1460;

/// One `hostfwd=tcp::HOST-:GUEST` rule.
#[derive(Clone, Debug)]
pub struct HostFwd {
    pub host_port: u16,
    pub guest_port: u16,
}

type Accepted = Arc<Mutex<VecDeque<(TcpStream, u16)>>>;

/// Network configuration shared by all incarnations of the stack (it survives a reboot,
/// so the listening sockets are created only once).
#[derive(Clone)]
pub struct NetConfig {
    pub mac: [u8; 6],
    accepted: Accepted,
}

impl NetConfig {
    pub fn new(mac: [u8; 6], fwds: &[HostFwd]) -> Result<NetConfig, String> {
        let accepted: Accepted = Arc::default();
        for f in fwds {
            let l = TcpListener::bind(("127.0.0.1", f.host_port)).map_err(|e| format!("hostfwd port {}: {e}", f.host_port))?;
            let (q, gp) = (accepted.clone(), f.guest_port);
            std::thread::spawn(move || {
                for s in l.incoming().flatten() {
                    q.lock().unwrap().push_back((s, gp));
                }
            });
        }
        Ok(NetConfig { mac, accepted })
    }
}

enum Event {
    Connected(u64, Sender<Vec<u8>>),
    ConnectFailed(u64),
    Data(u64, Vec<u8>),
    Eof(u64),
    Udp(u64, Vec<u8>),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct FlowKey {
    gport: u16,
    rip: [u8; 4],
    rport: u16,
}

#[derive(PartialEq, Eq, Debug)]
enum St {
    /// Guest SYN received, host connect in progress.
    Connecting,
    /// We sent a SYN to the guest (port forward), waiting for its SYN-ACK.
    SynSent,
    Established,
}

struct Tcp {
    id: u64,
    st: St,
    /// next sequence number expected from the guest
    rcv_nxt: u32,
    /// next sequence number to send / oldest unacknowledged / guest's window
    snd_nxt: u32,
    snd_una: u32,
    snd_wnd: u32,
    to_host: Option<Sender<Vec<u8>>>,
    /// bytes from the host not yet sent to the guest
    pending: VecDeque<u8>,
    host_eof: bool,
    fin_sent: bool,
    guest_fin: bool,
    /// data the guest sent before the host connection was up
    early: Vec<Vec<u8>>,
}

struct UdpFlow {
    sock: Arc<UdpSocket>,
    dest: SocketAddr,
    orig_ip: [u8; 4],
    orig_port: u16,
    gport: u16,
}

pub struct UserNet {
    cfg: NetConfig,
    out: VecDeque<Vec<u8>>,
    events_tx: Sender<Event>,
    events_rx: Receiver<Event>,
    tcp: HashMap<FlowKey, Tcp>,
    by_id: HashMap<u64, FlowKey>,
    udp: HashMap<(u16, [u8; 4], u16), u64>,
    udp_flows: HashMap<u64, UdpFlow>,
    next_id: u64,
    next_port: u16,
    iss: u32,
    guest_mac: [u8; 6],
    /// Every frame seen, for tests (`Some` = recording).
    pub log: Option<Vec<(bool, Vec<u8>)>>,
}

impl UserNet {
    pub fn new(cfg: NetConfig) -> UserNet {
        let (events_tx, events_rx) = channel();
        UserNet {
            guest_mac: cfg.mac,
            cfg,
            out: VecDeque::new(),
            events_tx,
            events_rx,
            tcp: HashMap::new(),
            by_id: HashMap::new(),
            udp: HashMap::new(),
            udp_flows: HashMap::new(),
            next_id: 1,
            next_port: 49152,
            iss: 0x1000_0000,
            log: None,
        }
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn send_eth(&mut self, dst: [u8; 6], ethertype: u16, payload: &[u8]) {
        let f = eth_frame(dst, GW_MAC, ethertype, payload);
        if let Some(l) = &mut self.log {
            l.push((false, f.clone()));
        }
        self.out.push_back(f);
    }

    fn send_ip(&mut self, src: [u8; 4], dst: [u8; 4], proto: u8, payload: &[u8]) {
        let p = ip_packet(src, dst, proto, payload);
        let mac = self.guest_mac;
        self.send_eth(mac, 0x0800, &p);
    }

    /// Process one Ethernet frame sent by the guest.
    pub fn guest_tx(&mut self, frame: &[u8]) {
        if let Some(l) = &mut self.log {
            l.push((true, frame.to_vec()));
        }
        let Some(eth) = Eth::parse(frame) else { return };
        if eth.src != [0; 6] {
            self.guest_mac = eth.src;
        }
        match eth.ethertype {
            0x0806 => self.arp(eth.payload),
            0x0800 => self.ipv4(eth.payload),
            _ => {}
        }
    }

    /// Next frame for the guest, after handling any events from host sockets.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        self.drain_events();
        self.accept_forwards();
        self.out.pop_front()
    }

    // ------------------------------------------------------------------ ARP / ICMP / DHCP

    fn arp(&mut self, p: &[u8]) {
        // htype(2) ptype(2) hlen(1) plen(1) op(2) sha(6) spa(4) tha(6) tpa(4)
        if p.len() < 28 || u16::from_be_bytes([p[6], p[7]]) != 1 {
            return;
        }
        let (sha, spa, tpa) = (&p[8..14], &p[14..18], &p[24..28]);
        if tpa == GW_IP || tpa == DNS_IP {
            let mut r = Vec::with_capacity(28);
            r.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]);
            r.extend_from_slice(&GW_MAC);
            r.extend_from_slice(tpa);
            r.extend_from_slice(sha);
            r.extend_from_slice(spa);
            let mut dst = [0u8; 6];
            dst.copy_from_slice(sha);
            self.send_eth(dst, 0x0806, &r);
        }
    }

    fn ipv4(&mut self, p: &[u8]) {
        let Some(ip) = Ip::parse(p) else { return };
        match ip.proto {
            1 => self.icmp(&ip),
            6 => self.tcp_in(&ip),
            17 => self.udp_in(&ip),
            _ => {}
        }
    }

    fn icmp(&mut self, ip: &Ip) {
        if ip.payload.len() >= 8 && ip.payload[0] == 8 && (ip.dst == GW_IP || ip.dst == DNS_IP) {
            let mut r = ip.payload.to_vec();
            r[0] = 0;
            r[2] = 0;
            r[3] = 0;
            let c = checksum(&r, 0);
            r[2..4].copy_from_slice(&c.to_be_bytes());
            self.send_ip(ip.dst, ip.src, 1, &r);
        }
    }

    fn dhcp(&mut self, payload: &[u8]) {
        // BOOTP: op htype hlen hops xid(4) secs flags ciaddr yiaddr siaddr giaddr chaddr(16) sname(64) file(128) magic options
        if payload.len() < 240 || payload[0] != 1 || payload[236..240] != [0x63, 0x82, 0x53, 0x63] {
            return;
        }
        let mut mtype = 0;
        let mut i = 240;
        while i + 1 < payload.len() {
            let (code, len) = (payload[i], payload[i + 1] as usize);
            if code == 255 {
                break;
            }
            if code == 0 {
                i += 1;
                continue;
            }
            if code == 53 && len >= 1 && i + 2 < payload.len() {
                mtype = payload[i + 2];
            }
            i += 2 + len;
        }
        let reply = match mtype {
            1 => 2, // DISCOVER -> OFFER
            3 => 5, // REQUEST -> ACK
            _ => return,
        };
        let mut r = vec![0u8; 240];
        r[0] = 2;
        r[1] = 1;
        r[2] = 6;
        r[4..8].copy_from_slice(&payload[4..8]); // xid
        r[10..12].copy_from_slice(&payload[10..12]); // flags
        r[16..20].copy_from_slice(&GUEST_IP); // yiaddr
        r[20..24].copy_from_slice(&GW_IP); // siaddr
        r[28..44].copy_from_slice(&payload[28..44]); // chaddr
        r[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
        let opts: &[&[u8]] = &[
            &[53, 1, reply],
            &[54, 4, 10, 0, 2, 2],
            &[51, 4, 0, 1, 0x51, 0x80], // lease 86400 s
            &[1, 4, 255, 255, 255, 0],
            &[3, 4, 10, 0, 2, 2],
            &[6, 4, 10, 0, 2, 3],
            &[28, 4, 10, 0, 2, 255],
            &[255],
        ];
        for o in opts {
            r.extend_from_slice(o);
        }
        let udp = udp_datagram(67, 68, &r);
        let ip = ip_packet(GW_IP, [255; 4], 17, &udp);
        self.send_eth([0xff; 6], 0x0800, &ip);
    }

    // ------------------------------------------------------------------ UDP

    fn udp_in(&mut self, ip: &Ip) {
        let Some(u) = Udp::parse(ip.payload) else { return };
        if u.dport == 67 && ip.dst == [255; 4] || (u.dport == 67 && ip.dst == GW_IP) {
            self.dhcp(u.payload);
            return;
        }
        let key = (u.sport, ip.dst, u.dport);
        let id = match self.udp.get(&key) {
            Some(&id) => id,
            None => {
                let dest = if ip.dst == DNS_IP && u.dport == 53 {
                    SocketAddr::from((resolver(), 53))
                } else if ip.dst == GW_IP {
                    SocketAddr::from(([127, 0, 0, 1], u.dport))
                } else {
                    SocketAddr::from((Ipv4Addr::from(ip.dst), u.dport))
                };
                let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return };
                let sock = Arc::new(sock);
                let id = self.id();
                let (s2, tx) = (sock.clone(), self.events_tx.clone());
                std::thread::spawn(move || {
                    s2.set_read_timeout(Some(Duration::from_secs(120))).ok();
                    let mut buf = vec![0u8; 65536];
                    while let Ok((n, _)) = s2.recv_from(&mut buf) {
                        if tx.send(Event::Udp(id, buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                });
                self.udp.insert(key, id);
                self.udp_flows.insert(id, UdpFlow { sock, dest, orig_ip: ip.dst, orig_port: u.dport, gport: u.sport });
                id
            }
        };
        let f = &self.udp_flows[&id];
        let _ = f.sock.send_to(u.payload, f.dest);
    }

    // ------------------------------------------------------------------ TCP

    fn tcp_seg(&mut self, k: &FlowKey, seq: u32, ack: u32, flags: u8, window: u16, payload: &[u8], mss: bool) {
        let t = tcp_segment(k.rport, k.gport, seq, ack, flags, window, payload, mss, k.rip, GUEST_IP);
        self.send_ip(k.rip, GUEST_IP, 6, &t);
    }

    fn rst_for(&mut self, ip: &Ip, t: &TcpHdr) {
        let k = FlowKey { gport: t.sport, rip: ip.dst, rport: t.dport };
        if t.flags & RST != 0 {
            return;
        }
        if t.flags & ACK != 0 {
            self.tcp_seg(&k, t.ack, 0, RST, 0, &[], false);
        } else {
            let seg_len = t.payload.len() as u32 + (t.flags & SYN != 0) as u32 + (t.flags & FIN != 0) as u32;
            self.tcp_seg(&k, 0, t.seq.wrapping_add(seg_len), RST | ACK, 0, &[], false);
        }
    }

    fn tcp_in(&mut self, ip: &Ip) {
        let Some(t) = TcpHdr::parse(ip.payload) else { return };
        let k = FlowKey { gport: t.sport, rip: ip.dst, rport: t.dport };
        if !self.tcp.contains_key(&k) {
            if t.flags & SYN != 0 && t.flags & ACK == 0 {
                self.tcp_syn(ip, &t, k);
            } else {
                self.rst_for(ip, &t);
            }
            return;
        }
        let c = self.tcp.get_mut(&k).unwrap();
        if t.flags & RST != 0 {
            self.remove(&k);
            return;
        }
        match c.st {
            St::Connecting => {
                // retransmitted SYN while the host connect is pending: ignore
            }
            St::SynSent => {
                if t.flags & SYN != 0 && t.flags & ACK != 0 && t.ack == c.snd_nxt {
                    c.rcv_nxt = t.seq.wrapping_add(1);
                    c.snd_una = t.ack;
                    c.snd_wnd = t.window as u32;
                    c.st = St::Established;
                    let (seq, ack) = (c.snd_nxt, c.rcv_nxt);
                    self.tcp_seg(&k, seq, ack, ACK, 0xffff, &[], false);
                    self.pump(&k);
                }
            }
            St::Established => self.established(&k, &t),
        }
    }

    fn tcp_syn(&mut self, ip: &Ip, t: &TcpHdr, k: FlowKey) {
        let id = self.id();
        self.iss = self.iss.wrapping_add(0x0001_0000);
        let c = Tcp {
            id,
            st: St::Connecting,
            rcv_nxt: t.seq.wrapping_add(1),
            snd_nxt: self.iss,
            snd_una: self.iss,
            snd_wnd: t.window as u32,
            to_host: None,
            pending: VecDeque::new(),
            host_eof: false,
            fin_sent: false,
            guest_fin: false,
            early: Vec::new(),
        };
        self.tcp.insert(k, c);
        self.by_id.insert(id, k);
        let addr = if ip.dst == GW_IP || ip.dst == DNS_IP {
            SocketAddr::from(([127, 0, 0, 1], t.dport))
        } else {
            SocketAddr::from((Ipv4Addr::from(ip.dst), t.dport))
        };
        let tx = self.events_tx.clone();
        std::thread::spawn(move || match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
            Ok(s) => attach_stream(id, s, tx),
            Err(_) => {
                let _ = tx.send(Event::ConnectFailed(id));
            }
        });
    }

    fn established(&mut self, k: &FlowKey, t: &TcpHdr) {
        let Some(c) = self.tcp.get_mut(k) else { return };
        if t.flags & ACK != 0 {
            // advance snd_una (sequence-space compare)
            if (t.ack.wrapping_sub(c.snd_una) as i32) > 0 && (c.snd_nxt.wrapping_sub(t.ack) as i32) >= 0 {
                c.snd_una = t.ack;
            }
            c.snd_wnd = t.window as u32;
        }
        let mut need_ack = false;
        // payload (trim anything already received)
        let mut data = t.payload;
        let mut seq = t.seq;
        let behind = c.rcv_nxt.wrapping_sub(seq) as i32;
        if behind > 0 {
            let skip = (behind as usize).min(data.len());
            data = &data[skip..];
            seq = seq.wrapping_add(skip as u32);
            need_ack = true;
        }
        if !data.is_empty() {
            if seq == c.rcv_nxt {
                c.rcv_nxt = c.rcv_nxt.wrapping_add(data.len() as u32);
                match &c.to_host {
                    Some(w) => {
                        let _ = w.send(data.to_vec());
                    }
                    None => c.early.push(data.to_vec()),
                }
            }
            need_ack = true;
        }
        if t.flags & FIN != 0 && t.seq.wrapping_add(t.payload.len() as u32) == c.rcv_nxt && !c.guest_fin {
            c.guest_fin = true;
            c.rcv_nxt = c.rcv_nxt.wrapping_add(1);
            c.to_host = None; // writer thread shuts the socket down
            need_ack = true;
        } else if t.flags & FIN != 0 {
            need_ack = true;
        }
        let (snd_nxt, rcv_nxt) = (c.snd_nxt, c.rcv_nxt);
        if need_ack {
            self.tcp_seg(k, snd_nxt, rcv_nxt, ACK, 0xffff, &[], false);
        }
        self.pump(k);
        self.maybe_close(k);
    }

    /// Send as much pending host data (and a FIN) to the guest as its window allows.
    fn pump(&mut self, k: &FlowKey) {
        loop {
            let Some(c) = self.tcp.get_mut(k) else { return };
            if c.st != St::Established {
                return;
            }
            let inflight = c.snd_nxt.wrapping_sub(c.snd_una);
            let room = c.snd_wnd.saturating_sub(inflight) as usize;
            let (seq, ack) = (c.snd_nxt, c.rcv_nxt);
            if !c.pending.is_empty() && room > 0 {
                let n = c.pending.len().min(MSS).min(room);
                let chunk: Vec<u8> = c.pending.drain(..n).collect();
                c.snd_nxt = c.snd_nxt.wrapping_add(n as u32);
                self.tcp_seg(k, seq, ack, ACK | PSH, 0xffff, &chunk, false);
            } else if c.pending.is_empty() && c.host_eof && !c.fin_sent {
                c.fin_sent = true;
                c.snd_nxt = c.snd_nxt.wrapping_add(1);
                self.tcp_seg(k, seq, ack, ACK | FIN, 0xffff, &[], false);
                return;
            } else {
                return;
            }
        }
    }

    fn maybe_close(&mut self, k: &FlowKey) {
        if let Some(c) = self.tcp.get(k) {
            if c.guest_fin && c.fin_sent && c.snd_una == c.snd_nxt {
                self.remove(k);
            }
        }
    }

    fn remove(&mut self, k: &FlowKey) {
        if let Some(c) = self.tcp.remove(k) {
            self.by_id.remove(&c.id);
        }
    }

    // ------------------------------------------------------------------ events from host threads

    fn drain_events(&mut self) {
        while let Ok(ev) = self.events_rx.try_recv() {
            match ev {
                Event::Connected(id, writer) => {
                    let Some(&k) = self.by_id.get(&id) else { continue };
                    let c = self.tcp.get_mut(&k).unwrap();
                    if c.st == St::Connecting {
                        for d in std::mem::take(&mut c.early) {
                            let _ = writer.send(d);
                        }
                        c.to_host = if c.guest_fin { None } else { Some(writer) };
                        c.st = St::Established;
                        let (seq, ack) = (c.snd_nxt, c.rcv_nxt);
                        c.snd_nxt = c.snd_nxt.wrapping_add(1); // our SYN
                        self.tcp_seg(&k, seq, ack, SYN | ACK, 0xffff, &[], true);
                    } else {
                        // port-forwarded stream: the writer is attached once, early data flows out
                        c.to_host = Some(writer);
                    }
                }
                Event::ConnectFailed(id) => {
                    if let Some(&k) = self.by_id.get(&id) {
                        let ack = self.tcp[&k].rcv_nxt;
                        self.tcp_seg(&k, 0, ack, RST | ACK, 0, &[], false);
                        self.remove(&k);
                    }
                }
                Event::Data(id, d) => {
                    if let Some(&k) = self.by_id.get(&id) {
                        self.tcp.get_mut(&k).unwrap().pending.extend(d);
                        self.pump(&k);
                    }
                }
                Event::Eof(id) => {
                    if let Some(&k) = self.by_id.get(&id) {
                        self.tcp.get_mut(&k).unwrap().host_eof = true;
                        self.pump(&k);
                        self.maybe_close(&k);
                    }
                }
                Event::Udp(id, d) => {
                    if let Some(f) = self.udp_flows.get(&id) {
                        let (oip, oport, gport) = (f.orig_ip, f.orig_port, f.gport);
                        let u = udp_datagram_ip(oport, gport, &d, oip, GUEST_IP);
                        self.send_ip(oip, GUEST_IP, 17, &u);
                    }
                }
            }
        }
    }

    /// Start connections to the guest for streams accepted by the host listeners.
    fn accept_forwards(&mut self) {
        loop {
            let next = self.cfg.accepted.lock().unwrap().pop_front();
            let Some((stream, gport)) = next else { return };
            let id = self.id();
            self.next_port = if self.next_port >= 60000 { 49152 } else { self.next_port + 1 };
            self.iss = self.iss.wrapping_add(0x0001_0000);
            let k = FlowKey { gport, rip: GW_IP, rport: self.next_port };
            let c = Tcp {
                id,
                st: St::SynSent,
                rcv_nxt: 0,
                snd_nxt: self.iss.wrapping_add(1),
                snd_una: self.iss,
                snd_wnd: 0,
                to_host: None,
                pending: VecDeque::new(),
                host_eof: false,
                fin_sent: false,
                guest_fin: false,
                early: Vec::new(),
            };
            let iss = self.iss;
            self.tcp.insert(k, c);
            self.by_id.insert(id, k);
            attach_stream(id, stream, self.events_tx.clone());
            self.tcp_seg(&k, iss, 0, SYN, 0xffff, &[], true);
        }
    }
}

/// Spawn the reader / writer threads for a connected host stream.
fn attach_stream(id: u64, stream: TcpStream, ev: Sender<Event>) {
    stream.set_nodelay(true).ok();
    let (wtx, wrx) = channel::<Vec<u8>>();
    let mut w = stream.try_clone().unwrap();
    std::thread::spawn(move || {
        for chunk in wrx {
            if w.write_all(&chunk).is_err() {
                break;
            }
        }
        let _ = w.shutdown(Shutdown::Write);
    });
    let _ = ev.send(Event::Connected(id, wtx));
    let mut r = stream;
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 16384];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = ev.send(Event::Eof(id));
                    break;
                }
                Ok(n) => {
                    if ev.send(Event::Data(id, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// First IPv4 nameserver of the host (`/etc/resolv.conf`), else 8.8.8.8.
fn resolver() -> Ipv4Addr {
    std::fs::read_to_string("/etc/resolv.conf")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                let mut it = l.split_whitespace();
                (it.next() == Some("nameserver")).then(|| it.next()?.parse::<Ipv4Addr>().ok()).flatten()
            })
        })
        .unwrap_or(Ipv4Addr::new(8, 8, 8, 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack() -> UserNet {
        UserNet::new(NetConfig::new([0x52, 0x54, 0, 0x12, 0x34, 0x56], &[]).unwrap())
    }

    fn wait_frame(n: &mut UserNet) -> Option<Vec<u8>> {
        for _ in 0..200 {
            if let Some(f) = n.next_frame() {
                return Some(f);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    fn guest_frame(proto: u8, dst: [u8; 4], l4: &[u8]) -> Vec<u8> {
        let ip = ip_packet(GUEST_IP, dst, proto, l4);
        eth_frame(GW_MAC, [0x52, 0x54, 0, 0x12, 0x34, 0x56], 0x0800, &ip)
    }

    #[test]
    fn arp_for_gateway() {
        let mut n = stack();
        let mut a = vec![0, 1, 8, 0, 6, 4, 0, 1];
        a.extend_from_slice(&[0x52, 0x54, 0, 0x12, 0x34, 0x56]);
        a.extend_from_slice(&GUEST_IP);
        a.extend_from_slice(&[0; 6]);
        a.extend_from_slice(&GW_IP);
        n.guest_tx(&eth_frame([0xff; 6], [0x52, 0x54, 0, 0x12, 0x34, 0x56], 0x0806, &a));
        let r = n.next_frame().expect("ARP reply");
        let e = Eth::parse(&r).unwrap();
        assert_eq!(e.ethertype, 0x0806);
        assert_eq!(&e.payload[6..8], &[0, 2]); // reply
        assert_eq!(&e.payload[8..14], &GW_MAC);
    }

    #[test]
    fn icmp_echo_to_gateway() {
        let mut n = stack();
        let mut icmp = vec![8, 0, 0, 0, 0x12, 0x34, 0, 1, b'h', b'i'];
        let c = checksum(&icmp, 0);
        icmp[2..4].copy_from_slice(&c.to_be_bytes());
        n.guest_tx(&guest_frame(1, GW_IP, &icmp));
        let r = n.next_frame().expect("echo reply");
        let ip = Ip::parse(Eth::parse(&r).unwrap().payload).unwrap();
        assert_eq!((ip.src, ip.dst, ip.payload[0]), (GW_IP, GUEST_IP, 0));
        assert_eq!(checksum(ip.payload, 0), 0, "valid ICMP checksum");
    }

    #[test]
    fn udp_is_forwarded_to_the_host_and_back() {
        let srv = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = srv.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut b = [0u8; 100];
            let (k, from) = srv.recv_from(&mut b).unwrap();
            srv.send_to(&b[..k], from).unwrap();
        });
        let mut n = stack();
        n.guest_tx(&guest_frame(17, GW_IP, &udp_datagram(40000, port, b"ping")));
        let r = wait_frame(&mut n).expect("UDP reply");
        let ip = Ip::parse(Eth::parse(&r).unwrap().payload).unwrap();
        let u = Udp::parse(ip.payload).unwrap();
        assert_eq!((ip.src, ip.proto, u.sport, u.dport, u.payload), (GW_IP, 17, port, 40000, &b"ping"[..]));
    }

    #[test]
    fn tcp_connect_send_receive_close() {
        let srv = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = srv.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = srv.accept().unwrap();
            let mut b = [0u8; 16];
            let k = s.read(&mut b).unwrap();
            s.write_all(&b[..k]).unwrap(); // echo, then close
        });
        let mut n = stack();
        let syn = tcp_segment(50000, port, 1000, 0, SYN, 0xffff, &[], true, GUEST_IP, GW_IP);
        n.guest_tx(&guest_frame(6, GW_IP, &syn));
        let r = wait_frame(&mut n).expect("SYN-ACK");
        let ip = Ip::parse(Eth::parse(&r).unwrap().payload).unwrap();
        let t = TcpHdr::parse(ip.payload).unwrap();
        assert_eq!(t.flags, SYN | ACK);
        assert_eq!(t.ack, 1001);
        let (their_seq, mut mine) = (t.seq.wrapping_add(1), 1001u32);
        let data = tcp_segment(50000, port, mine, their_seq, ACK | PSH, 0xffff, b"abc", false, GUEST_IP, GW_IP);
        n.guest_tx(&guest_frame(6, GW_IP, &data));
        mine += 3;
        let (mut got, mut fin) = (Vec::new(), false);
        for _ in 0..50 {
            let Some(r) = wait_frame(&mut n) else { break };
            let ip = Ip::parse(Eth::parse(&r).unwrap().payload).unwrap();
            let t = TcpHdr::parse(ip.payload).unwrap();
            assert_eq!(checksum(ip.payload, pseudo_for_test(ip.src, ip.dst, ip.payload.len())), 0, "TCP checksum");
            got.extend_from_slice(t.payload);
            fin |= t.flags & FIN != 0;
            if got == b"abc" && fin {
                break;
            }
        }
        assert_eq!(got, b"abc");
        assert!(fin, "host close must reach the guest as FIN");
        let _ = mine;
    }

    fn pseudo_for_test(src: [u8; 4], dst: [u8; 4], len: usize) -> u32 {
        let w = |a: [u8; 4]| (u16::from_be_bytes([a[0], a[1]]) as u32) + (u16::from_be_bytes([a[2], a[3]]) as u32);
        w(src) + w(dst) + 6 + len as u32
    }
}
