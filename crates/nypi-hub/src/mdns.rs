//! The hub on the local network, by mDNS (decision D94): the hub answers questions for
//! `_nypi._tcp.local` with its address, port, the fingerprint of its key, and its epoch; a node
//! asks, and tries what it hears first. What it hears is only a hint: nothing is trusted until
//! the TLS handshake shows the pinned key (reach.rs).
//!
//! The network is behind `Lan`, so tests put several machines on simulated networks in one
//! process; the real one is the mDNS multicast group on UDP 5353.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, broadcast, mpsc};

pub const SERVICE: &str = "_nypi._tcp.local";
const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const PORT: u16 = 5353;
const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_AAAA: u16 = 28;
const TYPE_SRV: u16 = 33;
const TYPE_ANY: u16 = 255;

/// A local network, as mDNS uses it: every packet sent reaches every member.
#[async_trait]
pub trait Lan: Send + Sync {
    async fn send(&self, packet: Vec<u8>);
    /// The next packet and who sent it; None when this member can hear no more.
    async fn recv(&self) -> Option<(Vec<u8>, IpAddr)>;
}

/// The real network: the mDNS group on every interface's default route.
pub struct Multicast {
    socket: tokio::net::UdpSocket,
}

impl Multicast {
    pub fn open() -> std::io::Result<Multicast> {
        use socket2::{Domain, Protocol, Socket, Type};
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        // Other mDNS programs hold the port too (the system's own responder).
        socket.set_reuse_port(true)?;
        socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, PORT)).into())?;
        socket.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED)?;
        socket.set_multicast_loop_v4(true)?;
        socket.set_multicast_ttl_v4(255)?;
        socket.set_nonblocking(true)?;
        Ok(Multicast { socket: tokio::net::UdpSocket::from_std(socket.into())? })
    }
}

#[async_trait]
impl Lan for Multicast {
    async fn send(&self, packet: Vec<u8>) {
        let _ = self.socket.send_to(&packet, (GROUP, PORT)).await;
    }

    async fn recv(&self) -> Option<(Vec<u8>, IpAddr)> {
        let mut buffer = vec![0u8; 9000];
        let (n, from) = self.socket.recv_from(&mut buffer).await.ok()?;
        buffer.truncate(n);
        Some((buffer, from.ip()))
    }
}

/// A simulated network in one process, for tests: `member` gives each machine its own place on it.
#[derive(Clone)]
pub struct SimulatedNetwork {
    packets: broadcast::Sender<(Vec<u8>, IpAddr)>,
}

impl Default for SimulatedNetwork {
    fn default() -> Self {
        SimulatedNetwork { packets: broadcast::channel(256).0 }
    }
}

impl SimulatedNetwork {
    pub fn member(&self, address: IpAddr) -> Arc<dyn Lan> {
        Arc::new(Member { network: self.packets.clone(), heard: Mutex::new(self.packets.subscribe()), address })
    }
}

struct Member {
    network: broadcast::Sender<(Vec<u8>, IpAddr)>,
    heard: Mutex<broadcast::Receiver<(Vec<u8>, IpAddr)>>,
    address: IpAddr,
}

#[async_trait]
impl Lan for Member {
    async fn send(&self, packet: Vec<u8>) {
        let _ = self.network.send((packet, self.address));
    }

    async fn recv(&self) -> Option<(Vec<u8>, IpAddr)> {
        let mut heard = self.heard.lock().await;
        loop {
            match heard.recv().await {
                Ok(packet) => return Some(packet),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// What the hub says of itself on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announcement {
    pub fingerprint: String,
    pub epoch: u64,
    pub port: u16,
    pub addresses: Vec<IpAddr>,
}

/// A hub heard on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heard {
    pub fingerprint: String,
    pub epoch: u64,
    pub addresses: Vec<SocketAddr>,
}

/// Answer questions for the hub's service with what `now` says, until the network goes away.
/// Another hub's answer heard meanwhile goes to `heard` (one with this hub's key at a higher
/// epoch means this one was replaced, decision D95).
pub async fn announce(
    lan: Arc<dyn Lan>,
    heard: impl Fn(Heard) + Send + Sync,
    now: impl Fn() -> Option<Announcement> + Send + Sync,
) {
    while let Some((packet, from)) = lan.recv().await {
        if asks_for_us(&packet) {
            if let Some(announcement) = now() {
                lan.send(answer(&announcement)).await;
            }
        } else if let Some(hub) = read_answer(&packet, from) {
            heard(hub);
        }
    }
}

/// Ask the network for hubs; each one heard within `wait` goes to `heard`.
pub async fn browse(lan: Arc<dyn Lan>, wait: Duration, heard: mpsc::UnboundedSender<Heard>) {
    lan.send(question()).await;
    let _ = tokio::time::timeout(wait, async {
        while let Some((packet, from)) = lan.recv().await {
            if let Some(hub) = read_answer(&packet, from) {
                let _ = heard.send(hub);
            }
        }
    })
    .await;
}

fn name(out: &mut Vec<u8>, name: &str) {
    for label in name.split('.').filter(|label| !label.is_empty()) {
        out.push(label.len().min(63) as u8);
        out.extend_from_slice(&label.as_bytes()[..label.len().min(63)]);
    }
    out.push(0);
}

fn question() -> Vec<u8> {
    let mut out = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    name(&mut out, SERVICE);
    out.extend_from_slice(&TYPE_PTR.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out
}

fn record(out: &mut Vec<u8>, owner: &str, kind: u16, data: &[u8]) {
    name(out, owner);
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&120u32.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
}

fn answer(hub: &Announcement) -> Vec<u8> {
    let short: String = hub.fingerprint.chars().take(12).collect();
    let instance = format!("nypi-{short}.{SERVICE}");
    let host = format!("nypi-{short}.local");
    let count = 3 + hub.addresses.len() as u16;
    let mut out = vec![0, 0, 0x84, 0];
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    let mut target = Vec::new();
    name(&mut target, &instance);
    record(&mut out, SERVICE, TYPE_PTR, &target);
    let mut srv = vec![0, 0, 0, 0];
    srv.extend_from_slice(&hub.port.to_be_bytes());
    name(&mut srv, &host);
    record(&mut out, &instance, TYPE_SRV, &srv);
    let mut txt = Vec::new();
    for entry in [format!("fp={}", hub.fingerprint), format!("epoch={}", hub.epoch)] {
        txt.push(entry.len() as u8);
        txt.extend_from_slice(entry.as_bytes());
    }
    record(&mut out, &instance, TYPE_TXT, &txt);
    for address in &hub.addresses {
        match address {
            IpAddr::V4(v4) => record(&mut out, &host, TYPE_A, &v4.octets()),
            IpAddr::V6(v6) => record(&mut out, &host, TYPE_AAAA, &v6.octets()),
        }
    }
    out
}

/// A name at `at`, following compression pointers; and where the name ends in the packet.
fn read_name(packet: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut end = None;
    for _ in 0..64 {
        let length = *packet.get(at)? as usize;
        if length == 0 {
            return Some((labels.join("."), end.unwrap_or(at + 1)));
        }
        if length & 0xC0 == 0xC0 {
            let pointer = ((length & 0x3F) << 8) | *packet.get(at + 1)? as usize;
            end.get_or_insert(at + 2);
            at = pointer;
            continue;
        }
        labels.push(String::from_utf8_lossy(packet.get(at + 1..at + 1 + length)?).to_ascii_lowercase());
        at += 1 + length;
    }
    None
}

fn u16_at(packet: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*packet.get(at)?, *packet.get(at + 1)?]))
}

fn asks_for_us(packet: &[u8]) -> bool {
    let (Some(flags), Some(questions)) = (u16_at(packet, 2), u16_at(packet, 4)) else { return false };
    if flags & 0x8000 != 0 {
        return false;
    }
    let mut at = 12;
    for _ in 0..questions {
        let Some((asked, end)) = read_name(packet, at) else { return false };
        let kind = u16_at(packet, end).unwrap_or_default();
        if asked == SERVICE && matches!(kind, TYPE_PTR | TYPE_ANY) {
            return true;
        }
        at = end + 4;
    }
    false
}

fn read_answer(packet: &[u8], from: IpAddr) -> Option<Heard> {
    let flags = u16_at(packet, 2)?;
    if flags & 0x8000 == 0 {
        return None;
    }
    let questions = u16_at(packet, 4)?;
    let records = u16_at(packet, 6)? as usize + u16_at(packet, 8)? as usize + u16_at(packet, 10)? as usize;
    let mut at = 12;
    for _ in 0..questions {
        at = read_name(packet, at)?.1 + 4;
    }
    let (mut fingerprint, mut epoch, mut port, mut ours) = (None, 0u64, None, false);
    let mut addresses = Vec::new();
    for _ in 0..records {
        let (owner, end) = read_name(packet, at)?;
        let kind = u16_at(packet, end)?;
        let length = u16_at(packet, end + 8)? as usize;
        let data = packet.get(end + 10..end + 10 + length)?;
        match kind {
            TYPE_PTR if owner == SERVICE => ours = true,
            TYPE_SRV => port = u16_at(data, 4),
            TYPE_TXT => {
                let mut i = 0;
                while let Some(&n) = data.get(i) {
                    let entry = String::from_utf8_lossy(data.get(i + 1..i + 1 + n as usize).unwrap_or_default());
                    match entry.split_once('=') {
                        Some(("fp", value)) => fingerprint = Some(value.to_ascii_lowercase()),
                        Some(("epoch", value)) => epoch = value.parse().unwrap_or(0),
                        _ => {}
                    }
                    i += 1 + n as usize;
                }
            }
            TYPE_A if data.len() == 4 => addresses.push(IpAddr::from(<[u8; 4]>::try_from(data).ok()?)),
            TYPE_AAAA if data.len() == 16 => addresses.push(IpAddr::from(<[u8; 16]>::try_from(data).ok()?)),
            _ => {}
        }
        at = end + 10 + length;
    }
    let (fingerprint, port) = (fingerprint?, port?);
    if !ours {
        return None;
    }
    if addresses.is_empty() {
        addresses.push(from);
    }
    Some(Heard { fingerprint, epoch, addresses: addresses.into_iter().map(|ip| SocketAddr::new(ip, port)).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hub on one simulated network is heard there, with its key and epoch, and not on another.
    #[tokio::test]
    async fn a_hub_is_heard_on_its_own_network_only() {
        let (home, away) = (SimulatedNetwork::default(), SimulatedNetwork::default());
        let hub = home.member("192.168.1.5".parse().unwrap());
        let announcement = Announcement {
            fingerprint: "ab".repeat(32),
            epoch: 3,
            port: 7681,
            addresses: vec!["192.168.1.5".parse().unwrap(), "fd00::5".parse().unwrap()],
        };
        let said = announcement.clone();
        tokio::spawn(announce(hub, |_| {}, move || Some(said.clone())));
        let (heard, mut hearing) = mpsc::unbounded_channel();
        browse(home.member("192.168.1.9".parse().unwrap()), Duration::from_millis(300), heard).await;
        let found = hearing.recv().await.expect("heard at home");
        assert_eq!((found.fingerprint.as_str(), found.epoch), (announcement.fingerprint.as_str(), 3));
        assert_eq!(found.addresses, ["192.168.1.5:7681".parse().unwrap(), "[fd00::5]:7681".parse().unwrap()]);
        let (heard, mut hearing) = mpsc::unbounded_channel();
        browse(away.member("10.0.0.2".parse().unwrap()), Duration::from_millis(100), heard).await;
        assert!(hearing.try_recv().is_err(), "nothing heard on another network");
    }

    #[test]
    fn compressed_names_are_read_and_loops_end() {
        let mut packet = vec![0u8; 12];
        name(&mut packet, "a.local");
        packet.extend_from_slice(&[1, b'b', 0xC0, 12]);
        assert_eq!(read_name(&packet, 21).unwrap().0, "b.a.local");
        let looped = [0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xC0, 12];
        assert_eq!(read_name(&looped, 12), None);
    }
}
