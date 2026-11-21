//! The hub's direct addresses (decision D94): where a node can reach the hub without the relay.
//! Each interface's private IPv4 and global or unique-local IPv6 address with the listen port,
//! `hub.publicAddress` when the user forwarded a port on the router, and, with
//! `hub.portMapping`, a port the router mapped by NAT-PMP or UPnP-IGD at the public IP the relay
//! saw. The list goes to nodes in their welcome and again when it changes; to a node it is only a
//! hint, since the TLS handshake still has to show the pinned key.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// An address of this machine with its prefix length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interface {
    pub address: IpAddr,
    pub prefix: u8,
}

/// This machine's interface addresses that are up, loopback left out.
pub fn interfaces() -> Vec<Interface> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list that freeifaddrs releases below; each
    // entry's pointers are read only while the list is held.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut entry = list;
        while !entry.is_null() {
            let item = &*entry;
            entry = item.ifa_next;
            let up = item.ifa_flags & libc::IFF_UP as libc::c_uint != 0;
            let loopback = item.ifa_flags & libc::IFF_LOOPBACK as libc::c_uint != 0;
            if !up || loopback || item.ifa_addr.is_null() {
                continue;
            }
            let (address, prefix) = match (*item.ifa_addr).sa_family as i32 {
                libc::AF_INET => {
                    let ip =
                        Ipv4Addr::from(u32::from_be((*(item.ifa_addr as *const libc::sockaddr_in)).sin_addr.s_addr));
                    let mask = if item.ifa_netmask.is_null() {
                        32
                    } else {
                        u32::from_be((*(item.ifa_netmask as *const libc::sockaddr_in)).sin_addr.s_addr).count_ones()
                    };
                    (IpAddr::V4(ip), mask as u8)
                }
                libc::AF_INET6 => {
                    let ip = Ipv6Addr::from((*(item.ifa_addr as *const libc::sockaddr_in6)).sin6_addr.s6_addr);
                    let mask = if item.ifa_netmask.is_null() {
                        128
                    } else {
                        (*(item.ifa_netmask as *const libc::sockaddr_in6))
                            .sin6_addr
                            .s6_addr
                            .iter()
                            .map(|byte| byte.count_ones())
                            .sum()
                    };
                    (IpAddr::V6(ip), mask as u8)
                }
                _ => continue,
            };
            found.push(Interface { address, prefix });
        }
        libc::freeifaddrs(list);
    }
    found
}

/// An address worth offering: a private IPv4 (and the shared range tailnets use), or a global
/// or unique-local IPv6; never loopback, link-local, or the `198.18.0.0/15` range tunnels use.
pub fn worth_offering(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            let tunnel = a == 198 && (b & 0xFE) == 18;
            let shared = a == 100 && (b & 0xC0) == 64;
            !tunnel && (v4.is_private() || shared)
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            let global = (first & 0xE000) == 0x2000;
            let unique_local = (first & 0xFE00) == 0xFC00;
            global || unique_local
        }
    }
}

/// Whether `a` and `b` are on the same subnet of `prefix` bits.
pub fn same_subnet(a: IpAddr, b: IpAddr, prefix: u8) -> bool {
    match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix.min(32) as u32) };
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix.min(128) as u32) };
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

/// `addresses` with those on a subnet this machine shares first, in their order otherwise.
pub fn nearest_first(addresses: &[String], mine: &[Interface]) -> Vec<String> {
    let near = |address: &String| {
        address
            .parse::<SocketAddr>()
            .is_ok_and(|socket| mine.iter().any(|own| same_subnet(own.address, socket.ip(), own.prefix)))
    };
    let (mut first, rest): (Vec<String>, Vec<String>) = addresses.iter().cloned().partition(near);
    first.extend(rest);
    first
}

/// What the hub knows of how it can be reached directly, kept current by `keep_current`.
#[derive(Default)]
pub struct Directions {
    /// The list as nodes are told it.
    pub list: Mutex<Vec<String>>,
    /// The public IP the relay saw the hub come from.
    pub seen: Mutex<Option<IpAddr>>,
    /// The external port the router mapped, by NAT-PMP or UPnP-IGD.
    pub mapped: Mutex<Option<u16>>,
    /// The addresses on this machine's own networks: what mDNS announces.
    pub local: Mutex<Vec<String>>,
    /// A fixed list, for tests: what the interfaces say is not looked at.
    pub fixed: Mutex<Option<(Vec<String>, Vec<String>)>>,
}

impl Directions {
    /// Nodes are told `list`; mDNS announces `local`. Nothing else is looked at.
    pub fn fixed(list: Vec<String>, local: Vec<String>) -> Arc<Directions> {
        let directions = Directions::default();
        *directions.list.lock().unwrap() = list.clone();
        *directions.local.lock().unwrap() = local.clone();
        *directions.fixed.lock().unwrap() = Some((list, local));
        Arc::new(directions)
    }

    pub fn now(&self) -> Vec<String> {
        self.list.lock().unwrap().clone()
    }

    pub fn local(&self) -> Vec<String> {
        self.local.lock().unwrap().clone()
    }

    /// Work the list out again; true when it changed.
    pub fn refresh(&self, port: Option<u16>, public: Option<&str>) -> bool {
        let list = match self.fixed.lock().unwrap().clone() {
            Some((fixed, _)) => fixed,
            None => {
                let mut list: Vec<String> = Vec::new();
                if let Some(port) = port {
                    for interface in interfaces() {
                        if worth_offering(interface.address) {
                            list.push(SocketAddr::new(interface.address, port).to_string());
                        }
                    }
                }
                *self.local.lock().unwrap() = list.clone();
                if let Some(public) = public.map(str::trim).filter(|public| !public.is_empty()) {
                    list.push(public.to_string());
                }
                if let (Some(ip), Some(mapped)) = (*self.seen.lock().unwrap(), *self.mapped.lock().unwrap()) {
                    list.push(SocketAddr::new(ip, mapped).to_string());
                }
                list.dedup();
                list
            }
        };
        let mut held = self.list.lock().unwrap();
        let changed = *held != list;
        *held = list;
        changed
    }
}

/// The router's address: the default route's gateway.
pub fn gateway() -> Option<Ipv4Addr> {
    #[cfg(target_os = "linux")]
    {
        let table = std::fs::read_to_string("/proc/net/route").ok()?;
        for line in table.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.get(1) == Some(&"00000000") {
                let raw = u32::from_str_radix(fields.get(2)?, 16).ok()?;
                // The table is written in the machine's byte order, which is little-endian where NyPi runs.
                return Some(Ipv4Addr::from(raw.swap_bytes()));
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("/sbin/route").args(["-n", "get", "default"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.lines().find_map(|line| line.trim().strip_prefix("gateway:").and_then(|ip| ip.trim().parse().ok()))
    }
}

/// How long a mapping asked of the router lasts; it is asked again at half of it.
pub const MAPPING_SECONDS: u32 = 3600;

/// Ask the router at `gateway` (port 5351) for TCP `port` by NAT-PMP (RFC 6886). The external
/// port it gave, or None.
pub fn nat_pmp(gateway: SocketAddr, port: u16) -> Option<u16> {
    let socket = UdpSocket::bind(if gateway.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).ok()?;
    socket.set_read_timeout(Some(Duration::from_millis(750))).ok()?;
    let mut request = vec![0u8, 2, 0, 0];
    request.extend_from_slice(&port.to_be_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    request.extend_from_slice(&MAPPING_SECONDS.to_be_bytes());
    for _ in 0..3 {
        socket.send_to(&request, gateway).ok()?;
        let mut answer = [0u8; 16];
        let Ok((n, _)) = socket.recv_from(&mut answer) else { continue };
        let ok = n >= 16 && answer[1] == 130 && u16::from_be_bytes([answer[2], answer[3]]) == 0;
        return ok.then(|| u16::from_be_bytes([answer[10], answer[11]])).filter(|mapped| *mapped != 0);
    }
    None
}

/// Find the router's UPnP-IGD description by SSDP; its address.
async fn ssdp() -> Option<String> {
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await.ok()?;
    let search = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n";
    socket.send_to(search.as_bytes(), "239.255.255.250:1900").await.ok()?;
    let mut buffer = vec![0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_millis(1500), socket.recv_from(&mut buffer)).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&buffer[..n]).into_owned();
    text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case("location").then(|| value.trim().to_string())
    })
}

/// The control URL of the WAN connection service in a router's description.
pub fn control_url(description: &str, base: &str) -> Option<(String, String)> {
    for service in ["urn:schemas-upnp-org:service:WANIPConnection:1", "urn:schemas-upnp-org:service:WANPPPConnection:1"]
    {
        let Some(at) = description.find(service) else { continue };
        let rest = &description[at..];
        let start = rest.find("<controlURL>")? + "<controlURL>".len();
        let end = rest[start..].find("</controlURL>")? + start;
        let path = rest[start..end].trim();
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            let origin: String = base.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
            format!("{origin}/{}", path.trim_start_matches('/'))
        };
        return Some((url, service.to_string()));
    }
    None
}

/// The SOAP body that asks a router to forward `port` to `client`.
pub fn add_port_mapping(service: &str, port: u16, client: IpAddr) -> String {
    format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:AddPortMapping xmlns:u=\"{service}\"><NewRemoteHost></NewRemoteHost><NewExternalPort>{port}</NewExternalPort><NewProtocol>TCP</NewProtocol><NewInternalPort>{port}</NewInternalPort><NewInternalClient>{client}</NewInternalClient><NewEnabled>1</NewEnabled><NewPortMappingDescription>NyPi hub</NewPortMappingDescription><NewLeaseDuration>{MAPPING_SECONDS}</NewLeaseDuration></u:AddPortMapping></s:Body></s:Envelope>"
    )
}

/// Ask the router for `port` by UPnP-IGD, from this machine's address `client`.
async fn upnp(port: u16, client: IpAddr) -> Option<u16> {
    let location = ssdp().await?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(3)).build().ok()?;
    let description = http.get(&location).send().await.ok()?.text().await.ok()?;
    let (url, service) = control_url(&description, &location)?;
    let answer = http
        .post(url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPAction", format!("\"{service}#AddPortMapping\""))
        .body(add_port_mapping(&service, port, client))
        .send()
        .await
        .ok()?;
    answer.status().is_success().then_some(port)
}

/// Map `port` on the router, NAT-PMP first, then UPnP-IGD.
pub async fn map_port(port: u16) -> Option<u16> {
    if let Some(router) = gateway() {
        let mapped = tokio::task::spawn_blocking(move || nat_pmp(SocketAddr::new(IpAddr::V4(router), 5351), port));
        if let Ok(Some(mapped)) = mapped.await {
            return Some(mapped);
        }
    }
    let client = interfaces().into_iter().find(|i| i.address.is_ipv4() && worth_offering(i.address))?.address;
    upnp(port, client).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_addresses_worth_offering_are_offered() {
        for offered in ["192.168.1.5", "10.0.0.2", "172.20.1.1", "100.64.0.7", "2001:db8::5", "fd00::5"] {
            assert!(worth_offering(offered.parse().unwrap()), "{offered}");
        }
        for skipped in ["127.0.0.1", "169.254.1.1", "198.18.0.1", "198.19.255.1", "8.8.8.8", "fe80::1", "::1"] {
            assert!(!worth_offering(skipped.parse().unwrap()), "{skipped}");
        }
    }

    #[test]
    fn candidates_on_a_shared_subnet_come_first() {
        let mine = [Interface { address: "192.168.1.9".parse().unwrap(), prefix: 24 }];
        let list = vec!["10.0.0.5:7681".to_string(), "192.168.1.5:7681".into(), "[fd00::5]:7681".into()];
        assert_eq!(nearest_first(&list, &mine), ["192.168.1.5:7681", "10.0.0.5:7681", "[fd00::5]:7681"]);
        assert!(same_subnet("fd00::5".parse().unwrap(), "fd00::9".parse().unwrap(), 64));
        assert!(!same_subnet("192.168.2.5".parse().unwrap(), "192.168.1.5".parse().unwrap(), 24));
    }

    /// NAT-PMP against a router simulated on loopback: the request asks for the port, and the
    /// external port the router gives is taken.
    #[test]
    fn nat_pmp_takes_the_port_the_router_gives() {
        let router = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = router.local_addr().unwrap();
        let answering = std::thread::spawn(move || {
            let mut request = [0u8; 12];
            let (n, from) = router.recv_from(&mut request).unwrap();
            assert_eq!((n, request[1], u16::from_be_bytes([request[4], request[5]])), (12, 2, 7681));
            let mut answer = vec![0u8, 130, 0, 0, 0, 0, 0, 1];
            answer.extend_from_slice(&7681u16.to_be_bytes());
            answer.extend_from_slice(&40123u16.to_be_bytes());
            answer.extend_from_slice(&3600u32.to_be_bytes());
            router.send_to(&answer, from).unwrap();
        });
        assert_eq!(nat_pmp(address, 7681), Some(40123));
        answering.join().unwrap();
    }

    #[test]
    fn a_routers_description_gives_its_control_url() {
        let description = "<root><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ctl/IPConn</controlURL></service></root>";
        let (url, service) = control_url(description, "http://192.168.1.1:5000/rootDesc.xml").unwrap();
        assert_eq!(url, "http://192.168.1.1:5000/ctl/IPConn");
        let body = add_port_mapping(&service, 7681, "192.168.1.9".parse().unwrap());
        assert!(body.contains("<NewExternalPort>7681</NewExternalPort>") && body.contains("192.168.1.9"));
    }

    #[test]
    fn a_fixed_list_is_what_nodes_are_told() {
        let directions = Directions::fixed(vec!["127.0.0.1:9".into()], Vec::new());
        assert!(!directions.refresh(Some(7681), Some("203.0.113.5:7681")));
        assert_eq!(directions.now(), ["127.0.0.1:9"]);
        let open = Directions::default();
        *open.seen.lock().unwrap() = Some("203.0.113.5".parse().unwrap());
        *open.mapped.lock().unwrap() = Some(40123);
        assert!(open.refresh(None, Some("198.51.100.2:7681")));
        assert_eq!(open.now(), ["198.51.100.2:7681", "203.0.113.5:40123"]);
    }
}
