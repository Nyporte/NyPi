//! Machines reaching the hub (decision D94), over loopback with several NyPi homes, a relay, and
//! two simulated local networks: direct links found by mDNS or a candidate, relayed links whose
//! pinned TLS runs inside the relay's WebSocket, a relayed node moving to a direct way with a
//! call under way, epochs, a key that is not the hub's, and the relay's own rules.

use std::sync::atomic::AtomicBool;

use nypi_ai::providers::faux::FauxProvider;
use nypi_core::config::RelaySettings;
use nypi_node::identity::{Identity, confirmation_code, join_value_client, server_fingerprint};
use nypi_node::proto::{Call, FromNode};
use tempfile::TempDir;

use super::tests::routed;
use super::*;
use crate::direct::Directions;
use crate::instance::{Instance, Mode};
use crate::mdns::SimulatedNetwork;
use crate::pairing::{self, JoinAnswer, Purpose, Trust, Way};
use crate::reach::{self, Reach, Serving};
use crate::relay::{self, Relay};
use crate::ws::{self, Entry, Message};

pub(super) async fn until(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..1500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} never happened");
}

pub(super) fn home_with(settings: &str) -> (TempDir, Paths) {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("settings.json"), settings).unwrap();
    let paths = Paths { home: home.path().to_path_buf() };
    (home, paths)
}

struct Hub {
    _home: TempDir,
    _work: TempDir,
    paths: Paths,
    nodes: Arc<crate::nodes::Nodes>,
    address: String,
    fingerprint: String,
    shutdown: CancellationToken,
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// A hub at `epoch`, with the key in `key` when given (the same hub key in another home), its
/// direct listener on loopback, and `network` made from that listener's address.
async fn start_hub(
    epoch: u64,
    key: Option<&Path>,
    network: impl FnOnce(tokio::net::TcpListener, &str) -> Network,
) -> Hub {
    let (home, paths) = home_with(r#"{ "sandbox": "off", "envFiles": [] }"#);
    let work = tempfile::tempdir().unwrap();
    if let Some(key) = key {
        std::fs::copy(key, paths.hub_key()).unwrap();
    }
    let mut instance = Instance::new_hub(&paths, "mini").unwrap();
    instance.epoch = epoch;
    instance.save(&paths).unwrap();
    let fingerprint = instance.hub_fingerprint.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let network = network(listener, &address);
    let nodes = crate::nodes::Nodes::open(&paths);
    let shutdown = CancellationToken::new();
    let faux = FauxProvider::new(vec![]);
    tokio::spawn(serve_hub(
        routed(home.path(), work.path(), &faux, &nodes),
        UnixListener::bind(home.path().join("d.sock")).unwrap(),
        shutdown.clone(),
        None,
        Some(nodes.clone()),
        network,
    ));
    Hub { _home: home, _work: work, paths, nodes, address, fingerprint, shutdown }
}

fn plain_network(listener: tokio::net::TcpListener, address: &str) -> Network {
    Network {
        directions: Directions::fixed(vec![address.to_string()], vec![address.to_string()]),
        ..Network::direct_only(Some(listener))
    }
}

/// A relay on loopback; its entry, as `http://`.
pub(super) async fn start_relay(settings: RelaySettings) -> (Arc<Relay>, String, TempDir) {
    let (home, paths) = home_with("{}");
    let relay = Relay::new(&paths, settings);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let entry = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(relay::serve(listener, relay.clone()));
    (relay, entry, home)
}

/// An address that refuses until it is opened, then leads to `to`: a way to the hub that is
/// out of reach from a network away until it is not.
struct Gate {
    address: String,
    open: Arc<AtomicBool>,
    to: Arc<Mutex<String>>,
}

async fn gate() -> Gate {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (open, to) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(String::new())));
    let (opened, target) = (open.clone(), to.clone());
    tokio::spawn(async move {
        while let Ok((mut incoming, _)) = listener.accept().await {
            if !opened.load(Ordering::SeqCst) {
                continue;
            }
            let to = target.lock().unwrap().clone();
            tokio::spawn(async move {
                if let Ok(mut outgoing) = tokio::net::TcpStream::connect(&to).await {
                    let _ = tokio::io::copy_bidirectional(&mut incoming, &mut outgoing).await;
                }
            });
        }
    });
    Gate { address, open, to }
}

pub(super) fn node_instance(paths: &Paths, joined: &pairing::Joined) {
    Instance {
        mode: Mode::Node,
        name: joined.name.clone(),
        instance_id: crate::instance::new_id(),
        epoch: joined.epoch,
        hub_fingerprint: joined.hub_fingerprint.clone(),
        trust: joined.trust,
        direct: joined.direct.clone(),
        down: false,
    }
    .save(paths)
    .unwrap();
}

/// Serve this home's node to its hub, keeping every state it went through.
pub(super) fn serve(paths: &Paths, reach: Reach) -> Arc<Mutex<Vec<Serving>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (paths, kept) = (paths.clone(), seen.clone());
    tokio::spawn(async move {
        let name = Instance::load(&paths).unwrap().name;
        let node = Arc::new(nypi_core::device::this_computer(&paths, Some(&name)).unwrap());
        let said = move |serving: &Serving, _: Option<&str>| kept.lock().unwrap().push(serving.clone());
        reach::serve_node(&paths, node, &reach, Duration::from_millis(300), said).await
    });
    seen
}

pub(super) fn last(seen: &Arc<Mutex<Vec<Serving>>>) -> Option<Serving> {
    seen.lock().unwrap().last().cloned()
}

fn link_of(hub: &Hub, name: &str) -> Option<Via> {
    links_up(&hub.paths).into_iter().find(|link| link.name == name && link.purpose == Purpose::Node).map(|l| l.path)
}

/// A node on the hub's network links directly, found by mDNS, and never through the relay; a
/// node elsewhere joins through the relay with the same code on both ends, links through it,
/// and moves to a direct address once one is reachable, while a call it is carrying out
/// finishes across the switch.
#[tokio::test]
async fn a_node_at_home_links_directly_and_one_away_moves_from_the_relay_to_a_direct_way() {
    let settings = RelaySettings { trust_proxy: false, ..Default::default() };
    let (relay, entry, _relay_home) = start_relay(settings).await;
    let (home_net, away_net) = (SimulatedNetwork::default(), SimulatedNetwork::default());
    // The way a node away is told of: closed for now.
    let way_in = gate().await;
    let told = way_in.address.clone();
    let hub = start_hub(1, None, |listener, address| Network {
        listeners: vec![listener],
        directions: Directions::fixed(vec![told], vec![address.to_string()]),
        lan: Some(home_net.member("192.168.1.5".parse().unwrap())),
        entry: Some(entry.clone()),
        follow_entry: false,
        public_address: None,
        port_mapping: false,
        board: None,
    })
    .await;
    *way_in.to.lock().unwrap() = hub.address.clone();
    until("the hub registered at the relay", || relay.has_hub()).await;
    assert_eq!(relay.state().hub_key.as_deref(), Some(hub.fingerprint.as_str()), "the first hub claimed it");

    // At home: the hub the entry leads to is heard on this network, so the join goes direct.
    let (_laptop_home, laptop) = home_with(&format!(r#"{{ "sandbox": "off", "envFiles": [], "entry": "{entry}" }}"#));
    let at_home = Reach { lan: Some(home_net.member("192.168.1.9".parse().unwrap())), direct_wait: DIRECT };
    let ways = reach::ways_to_join(&laptop, Some(&entry), &at_home).await.unwrap();
    assert_eq!(ways, [Way::Direct(hub.address.clone()), Way::Relayed(entry.clone())]);
    let code = pairing::invite(&hub.paths, "laptop", Trust::Personal, chrono::Duration::hours(1)).unwrap();
    let joined = pairing::join(&laptop, &ways, "x", Some(&code), "i-laptop", |_| {}).await.unwrap();
    assert!(joined.way.is_direct());
    node_instance(&laptop, &joined);
    let carried = relay.carried.load(Ordering::SeqCst);
    let laptop_seen = serve(&laptop, at_home.clone());
    until("the laptop linked", || link_of(&hub, "laptop").is_some()).await;
    assert_eq!(link_of(&hub, "laptop"), Some(Via::Direct));
    until("the laptop says it is linked directly", || {
        last(&laptop_seen).map(|s| (s.connected, s.relayed)) == Some((true, false))
    })
    .await;
    assert_eq!(relay.carried.load(Ordering::SeqCst), carried, "the laptop's link never went through the relay");

    // Away: nothing on this network, so the join goes through the relay, inner TLS and all,
    // and both ends show the same code.
    let (_away_home, away) = home_with(&format!(r#"{{ "sandbox": "off", "envFiles": [], "entry": "{entry}" }}"#));
    let elsewhere = Reach { lan: Some(away_net.member("10.0.0.2".parse().unwrap())), direct_wait: DIRECT };
    let ways = reach::ways_to_join(&away, Some(&entry), &elsewhere).await.unwrap();
    assert_eq!(ways, [Way::Relayed(entry.clone())]);
    let shown = Arc::new(Mutex::new(String::new()));
    let joining = tokio::spawn({
        let (away, shown) = (away.clone(), shown.clone());
        async move {
            pairing::join(&away, &ways, "phone", None, "i-away", |code| *shown.lock().unwrap() = code.into()).await
        }
    });
    until("the request", || !pairing::pending(&hub.paths).is_empty()).await;
    until("the code on the machine", || !shown.lock().unwrap().is_empty()).await;
    let request = pairing::pending(&hub.paths).pop().unwrap();
    assert_eq!(request.code, *shown.lock().unwrap(), "the same code through the relay");
    pairing::answer(&hub.paths, &request.code, JoinAnswer::Admit { trust: Trust::Personal }).unwrap();
    let joined = joining.await.unwrap().unwrap();
    assert!(!joined.way.is_direct());
    assert_eq!(joined.direct, std::slice::from_ref(&way_in.address), "the welcome told it the direct way");
    node_instance(&away, &joined);
    let away_seen = serve(&away, elsewhere);
    until("the away node linked", || link_of(&hub, "phone").is_some()).await;
    assert_eq!(link_of(&hub, "phone"), Some(Via::Relayed));
    until("the node away says it is relayed", || {
        last(&away_seen).map(|s| (s.connected, s.relayed)) == Some((true, true))
    })
    .await;

    // A call under way on the relayed link; the direct way opens meanwhile.
    let site = tempfile::tempdir().unwrap();
    until("the node's hello", || hub.nodes.get("phone").is_some()).await;
    let node = hub.nodes.get("phone").unwrap();
    let (events, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let call = Call {
        id: "c-move".into(),
        task: None,
        tool: "bash".into(),
        arguments: json!({ "command": "sleep 1.5; echo carried across" }),
        folder: site.path().display().to_string(),
    };
    let calling = tokio::spawn(async move { nypi_node::NodeLink::call(&*node, call, events).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    way_in.open.store(true, Ordering::SeqCst);
    until("the move to the direct way", || link_of(&hub, "phone") == Some(Via::Direct)).await;
    until("the node away says it is direct", || {
        last(&away_seen).map(|s| (s.connected, s.relayed)) == Some((true, false))
    })
    .await;
    let finished = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = heard.recv().await {
            if let FromNode::Finished { result, .. } = event {
                return result;
            }
        }
        panic!("the call never finished");
    })
    .await
    .unwrap();
    assert!(serde_json::to_string(&finished).unwrap().contains("carried across"), "{finished:?}");
    calling.await.unwrap();
    let away_instance = Instance::load(&away).unwrap();
    assert_eq!(away_instance.direct.first(), Some(&way_in.address), "the way that worked is tried first next time");
}

const DIRECT: Duration = Duration::from_millis(600);

fn lan_network(net: &SimulatedNetwork, ip: &str) -> impl FnOnce(tokio::net::TcpListener, &str) -> Network {
    let lan = net.member(ip.parse().unwrap());
    move |listener, address| Network { lan: Some(lan), ..plain_network(listener, address) }
}

/// Among hubs that show the pinned key, the highest epoch wins, whether the node was told their
/// addresses or heard them by mDNS; a hub with another key is refused at the handshake; and a
/// hub below the epoch a node knows is refused even when it shows the key.
#[tokio::test]
async fn a_key_that_is_not_the_hubs_is_refused_and_the_higher_epoch_wins() {
    let net = SimulatedNetwork::default();
    let first = start_hub(1, None, lan_network(&net, "192.168.1.5")).await;
    let second = start_hub(2, Some(&first.paths.hub_key()), lan_network(&net, "192.168.1.6")).await;
    let impostor = start_hub(5, None, lan_network(&net, "192.168.1.7")).await;
    assert_eq!(first.fingerprint, second.fingerprint);
    let (_node_home, node) = home_with(r#"{ "envFiles": [] }"#);
    let mut instance = Instance {
        mode: Mode::Node,
        name: "n".into(),
        epoch: 1,
        hub_fingerprint: first.fingerprint.clone(),
        trust: Trust::Personal,
        direct: vec![impostor.address.clone(), first.address.clone(), second.address.clone()],
        ..Default::default()
    };
    instance.save(&node).unwrap();
    let told = reach::best_direct(&node, &instance, &Reach::on(None)).await;
    assert_eq!(told, Some((second.address.clone(), 2)), "told of all three");
    instance.direct = vec![impostor.address.clone()];
    let heard = Reach { lan: Some(net.member("192.168.1.9".parse().unwrap())), direct_wait: DIRECT };
    assert_eq!(reach::best_direct(&node, &instance, &heard).await, Some((second.address.clone(), 2)), "heard by mDNS");

    let other = pairing::probe(&node, &Way::Direct(impostor.address.clone()), Some(&first.fingerprint), 1).await;
    let other = other.unwrap_err();
    assert!(format!("{other:#}").contains("not the hub this machine trusts"), "{other:#}");

    // The first hub heard the second answer with its key at a higher epoch (decision D95): it
    // stepped down, and is a node at that epoch.
    until("the first hub stepped down", || Instance::load(&first.paths).is_some_and(|i| i.mode == Mode::Node)).await;
    assert_eq!(Instance::load(&first.paths).unwrap().epoch, 2);
    assert!(Instance::load(&second.paths).unwrap().is_hub(), "the second is the hub still");
}

/// Copies what passes between a machine and the relay, both ways.
async fn tap(to: String) -> (String, Arc<Mutex<Vec<u8>>>) {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let entry = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let kept = seen.clone();
    tokio::spawn(async move {
        while let Ok((incoming, _)) = listener.accept().await {
            let Ok(outgoing) = tokio::net::TcpStream::connect(&to).await else { continue };
            let (a_read, a_write) = incoming.into_split();
            let (b_read, b_write) = outgoing.into_split();
            type Read = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
            type Write = Box<dyn AsyncWrite + Unpin + Send>;
            let both: [(Read, Write); 2] =
                [(Box::new(a_read), Box::new(b_write)), (Box::new(b_read), Box::new(a_write))];
            for (mut from, mut to) in both {
                let kept = kept.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 16 * 1024];
                    while let Ok(n) = from.read(&mut buffer).await {
                        if n == 0 || to.write_all(&buffer[..n]).await.is_err() {
                            break;
                        }
                        kept.lock().unwrap().extend_from_slice(&buffer[..n]);
                    }
                    let _ = to.shutdown().await;
                });
            }
        }
    });
    (entry, seen)
}

/// End-to-end TLS through the relay: what crosses it is the WebSocket's frames around TLS
/// records, and nothing of the join is readable there; and a relay that answers with a key of
/// its own, and joins the hub itself, makes the two codes differ.
#[tokio::test]
async fn the_relay_sees_only_ciphertext_and_a_swapped_key_shows_another_code() {
    let (relay, entry, _relay_home) = start_relay(RelaySettings { trust_proxy: false, ..Default::default() }).await;
    let hub = start_hub(1, None, |listener, address| Network {
        entry: Some(entry.clone()),
        follow_entry: false,
        ..plain_network(listener, address)
    })
    .await;
    until("the hub registered at the relay", || relay.has_hub()).await;
    let relay_address = entry.trim_start_matches("http://").to_string();

    // Through a tap on the way to the relay: the same code on both ends, and no plain text.
    let (tapped, seen) = tap(relay_address.clone()).await;
    let (_laptop_home, laptop) = home_with(r#"{ "envFiles": [] }"#);
    let shown = Arc::new(Mutex::new(String::new()));
    let joining = tokio::spawn({
        let (laptop, shown, ways) = (laptop.clone(), shown.clone(), vec![Way::Relayed(tapped.clone())]);
        async move {
            pairing::join(&laptop, &ways, "tapped-laptop", None, "i-t", |code| *shown.lock().unwrap() = code.into())
                .await
        }
    });
    until("the request", || !pairing::pending(&hub.paths).is_empty()).await;
    until("the code", || !shown.lock().unwrap().is_empty()).await;
    let request = pairing::pending(&hub.paths).pop().unwrap();
    assert_eq!(request.code, *shown.lock().unwrap());
    pairing::answer(&hub.paths, &request.code, JoinAnswer::Refuse).unwrap();
    assert!(joining.await.unwrap().is_err());
    let seen = seen.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&seen);
    assert!(text.contains("Upgrade: websocket"), "the tap saw the relay's own handshake");
    for plain in ["\"purpose\"", "tapped-laptop", "\"join\"", "i-t\"", request.code.as_str()] {
        assert!(!text.contains(plain), "{plain} crossed the relay readable");
    }

    // A relay in the middle with a key of its own, which joins the hub itself.
    let middle = Identity::generate().unwrap();
    let in_the_middle = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let middle_entry = format!("http://{}", in_the_middle.local_addr().unwrap());
    let toward_hub = entry.clone();
    tokio::spawn(async move {
        let (tcp, _) = in_the_middle.accept().await.unwrap();
        let (read, mut write) = tcp.into_split();
        let mut read = BufReader::new(read);
        let head = ws::read_head(&mut read, true).await.unwrap().unwrap();
        write.write_all(&ws::upgrade_response(&head)).await.unwrap();
        let toward_device = ws::into_stream(ws::Reader::new(read), ws::Writer::new(write, false));
        let _device_side = middle.acceptor().unwrap().accept(toward_device).await.unwrap();
        let (r, w) = ws::dial(&Entry::parse(&toward_hub).unwrap(), pairing::LINK_PATH, &[]).await.unwrap();
        let mut hub_side = middle.dial_over(ws::into_stream(r, w), None).await.unwrap();
        hub_side.write_all(b"{\"purpose\":\"join\",\"name\":\"middle\"}\n").await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let device = Identity::generate().unwrap();
    let (r, w) = ws::dial(&Entry::parse(&middle_entry).unwrap(), pairing::LINK_PATH, &[]).await.unwrap();
    let stream = device.dial_over(ws::into_stream(r, w), None).await.unwrap();
    let device_code = confirmation_code(
        &server_fingerprint(&stream).unwrap(),
        &device.fingerprint(),
        &join_value_client(&stream).unwrap(),
    );
    until("the middle's request", || pairing::pending(&hub.paths).iter().any(|r| r.name == "middle")).await;
    let hub_code = pairing::pending(&hub.paths).into_iter().find(|r| r.name == "middle").unwrap().code;
    assert_ne!(server_fingerprint(&stream).unwrap(), hub.fingerprint, "the device saw the middle's key");
    assert_ne!(device_code, hub_code, "and the user sees two different codes");
}

/// Register at the relay at `entry` as the hub with `key`, with `headers` on the request: the
/// relay's answer, and the control connection's reader for what it asks next.
pub(super) async fn register(
    entry: &str,
    key: &Identity,
    epoch: u64,
    instance_id: &str,
    headers: &[(&str, &str)],
) -> (relay::Control, ws::SocketReader, ws::SocketWriter) {
    use base64::Engine;
    let (mut reader, mut writer) = ws::dial(&Entry::parse(entry).unwrap(), relay::HUB_PATH, headers).await.unwrap();
    let Some(Message::Text(first)) = reader.next().await.unwrap() else { panic!("no challenge") };
    let relay::Control::Challenge { nonce } = serde_json::from_str(&first).unwrap() else { panic!("no challenge") };
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let register = relay::Control::Register {
        public_key: encode(&key.public_key()),
        epoch,
        instance_id: instance_id.into(),
        signature: encode(&key.sign(&relay::registration(&nonce, epoch, instance_id))),
    };
    writer.send(&Message::Text(serde_json::to_string(&register).unwrap())).await.unwrap();
    let Some(Message::Text(answer)) = reader.next().await.unwrap() else { panic!("no answer") };
    (serde_json::from_str(&answer).unwrap(), reader, writer)
}

/// The next `open` the relay asks of a registered hub.
pub(super) async fn next_open(reader: &mut ws::SocketReader) -> (String, String, String, String) {
    loop {
        match reader.next().await.unwrap() {
            Some(Message::Text(text)) => {
                if let Ok(relay::Control::Open { id, token, kind, from }) = serde_json::from_str(&text) {
                    return (id, token, kind, from);
                }
            }
            Some(_) => {}
            None => panic!("the relay closed the control connection"),
        }
    }
}

/// The relay's own rules: a control connection signed by a key it does not take is refused;
/// with no hub, links are refused and browsers get 503; one address gets so many connections a
/// minute; the hub is told the address it comes from; a browser's address is taken from
/// `CF-Connecting-IP` only when the proxy is trusted; and a link is held to its rate.
#[tokio::test]
async fn the_relay_holds_to_its_key_its_limits_and_the_address_it_is_given() {
    let hub_key = Identity::generate().unwrap();
    let settings = RelaySettings {
        hub_key: Some(hub_key.fingerprint()),
        per_ip_per_minute: 4,
        max_kbps_per_link: 160,
        ..Default::default()
    };
    let (relay, entry, _home) = start_relay(settings).await;
    let http = reqwest::Client::new();

    // No hub yet: links refused, browsers told so.
    let Err(link) = ws::dial(&Entry::parse(&entry).unwrap(), pairing::LINK_PATH, &[]).await else {
        panic!("a link with no hub was taken")
    };
    assert!(link.to_string().contains("503"), "{link}");
    let page = http.get(format!("{entry}/")).send().await.unwrap();
    assert_eq!(page.status(), 503);
    assert!(page.text().await.unwrap().contains("The hub is not here"));

    // Another key is refused, whatever it signs.
    let stranger = Identity::generate().unwrap();
    let (refused, ..) = register(&entry, &stranger, 1, "i-s", &[]).await;
    assert!(
        matches!(&refused, relay::Control::Refused { reason, .. } if reason.contains("takes the hub key")),
        "{refused:?}"
    );
    assert!(!relay.has_hub());

    // The hub's key is taken, and told the address it came from (the proxy's word, trusted).
    let (registered, mut control, _keep) =
        register(&entry, &hub_key, 1, "i-h", &[("CF-Connecting-IP", "198.51.100.7")]).await;
    assert!(matches!(&registered, relay::Control::Registered { seen, .. } if seen == "198.51.100.7"), "{registered:?}");
    assert_eq!(relay.state().hub_key, Some(hub_key.fingerprint()));

    // A browser's request reaches the hub with the address Cloudflare gave, and its answer comes
    // back at no more than the link's rate (160 kbit/s: 20 kB a second).
    let body = "x".repeat(50_000);
    let answering = tokio::spawn({
        let (entry, body) = (entry.clone(), body.clone());
        async move {
            let (id, token, kind, from) = next_open(&mut control).await;
            let path = format!("{}{id}", relay::DATA_PATH);
            let (mut r, mut w) =
                ws::dial(&Entry::parse(&entry).unwrap(), &path, &[(relay::TOKEN_HEADER, &token)]).await.unwrap();
            let Some(Message::Binary(request)) = r.next().await.unwrap() else { panic!("no request") };
            let answer =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            w.send(&Message::Binary(answer.into_bytes())).await.unwrap();
            w.send(&Message::Close).await.unwrap();
            (kind, from, String::from_utf8_lossy(&request).into_owned(), control)
        }
    });
    let started = tokio::time::Instant::now();
    let page = http.get(format!("{entry}/board")).header("CF-Connecting-IP", "203.0.113.9").send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(page.text().await.unwrap(), body);
    assert!(started.elapsed() >= Duration::from_millis(1800), "held to the rate: {:?}", started.elapsed());
    let (kind, from, request, _control) = answering.await.unwrap();
    assert_eq!((kind.as_str(), from.as_str()), ("board", "203.0.113.9"));
    assert!(request.starts_with("GET /board HTTP/1.1\r\n"), "{request}");
    assert!(request.contains("X-NyPi-Client: 203.0.113.9") && request.contains("Connection: close"), "{request}");

    // Four connections a minute from one address, then 429.
    let mut statuses = Vec::new();
    for _ in 0..3 {
        statuses.push(http.get(format!("{entry}/_nypi/none")).send().await.unwrap().status().as_u16());
    }
    assert_eq!(statuses, [404, 404, 429], "after the link and the page from here, two more, then refused");

    // Without trusting the proxy, the header is not believed.
    let (relay, entry, _home) = start_relay(RelaySettings { trust_proxy: false, ..Default::default() }).await;
    let (registered, mut control, _keep) =
        register(&entry, &hub_key, 1, "i-h", &[("CF-Connecting-IP", "198.51.100.7")]).await;
    assert!(matches!(&registered, relay::Control::Registered { seen, .. } if seen == "127.0.0.1"), "{registered:?}");
    assert!(relay.has_hub());
    let asking = tokio::spawn({
        let entry = entry.clone();
        async move { reqwest::Client::new().get(format!("{entry}/")).header("CF-Connecting-IP", "203.0.113.9").send().await }
    });
    let (_, _, _, from) = next_open(&mut control).await;
    assert_eq!(from, "127.0.0.1");
    asking.abort();
}

/// Without an entry (decision D97): the hub stays off the relay and says nothing; an entry saved
/// in its settings later is taken without a restart. A machine joining with no entry finds the
/// hub on its network by mDNS, and is told what to do when none answers.
#[tokio::test]
async fn without_an_entry_the_hub_stays_on_its_network_until_one_is_saved() {
    let (relay, entry, _relay_home) = start_relay(RelaySettings { trust_proxy: false, ..Default::default() }).await;
    let net = SimulatedNetwork::default();
    let lan = net.member("192.168.1.5".parse().unwrap());
    let hub = start_hub(1, None, move |listener, address| Network {
        lan: Some(lan),
        follow_entry: true,
        ..plain_network(listener, address)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!relay.has_hub(), "no entry: no relay is tried");
    let (_home, joining) = home_with(r#"{ "envFiles": [] }"#);
    let here = Reach { lan: Some(net.member("192.168.1.9".parse().unwrap())), direct_wait: DIRECT };
    let ways = reach::ways_to_join(&joining, None, &here).await.unwrap();
    assert_eq!(ways, [Way::Direct(hub.address.clone())], "found on this network");
    let away =
        Reach { lan: Some(SimulatedNetwork::default().member("10.0.0.2".parse().unwrap())), direct_wait: DIRECT };
    let none = reach::ways_to_join(&joining, None, &away).await.unwrap_err();
    assert!(none.to_string().contains("--entry"), "{none}");

    nypi_core::config::Settings::save_global(&hub.paths, &["entry"], json!(entry)).unwrap();
    until("the hub registered at the entry saved later", || relay.has_hub()).await;
}
