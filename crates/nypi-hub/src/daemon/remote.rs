//! Machines' links to the hub (decisions D80, D93, D94): the one place where what a link may do
//! is decided. A link is TLS 1.3 with the hub's own key, which the machine pinned when it
//! joined, and the machine's key, which the hub looks up in its register. It comes straight over
//! TCP to `hub.listen` (direct), or inside a WebSocket the relay passes on unread (relayed); the
//! handshake and everything after it are the same either way. What the link may carry is
//! decided by the trust that key was admitted with and the purpose its first line names, and by
//! nothing else the machine says: `node` goes to the register of nodes and speaks the node
//! protocol only, for any admitted machine; `talk` goes to the conversation, for a `personal`
//! machine only; `join` is a key the hub does not know asking to be admitted, which waits for the
//! user's answer (or shows an invite code) and never reaches the conversation or the Principal;
//! `probe` is answered with the hub's epoch, for anyone who completed the handshake, and nothing
//! else; `standby` (the hub's state, kept current), `move` (`nypi hub here`), and `notice` (a
//! line for the user's outbox) are for a `personal` machine only (decision D95). A `talk` link
//! whose first line is an admin command runs one of the hub's own commands for a personal
//! machine, when the allowlist in `protocol.rs` names it, and never reaches the conversation
//! (decision D96). A hello that
//! names an epoch above the hub's own tells it that another hub took its place: it steps down.

use super::*;
use crate::pairing::{self, Answer, Hello, JoinAnswer, Peer, PeerKind, Peers, Purpose, Trust};
use nypi_node::identity::{Identity, peer_fingerprint};
use std::collections::HashMap;

/// How often the register of paired devices is looked at for a change made by hand.
const REGISTER_CHECK: Duration = Duration::from_secs(5);

/// How a link reached the hub (decision D94).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    /// Straight to `hub.listen`.
    Direct,
    /// Through the relay at the entry.
    Relayed,
}

/// One link up now, as `run/links.json` keeps it for `nypi status` and `nypi devices`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkRecord {
    pub key: String,
    pub name: String,
    pub purpose: Purpose,
    pub path: Via,
    /// Where it came from: the machine's address, as the hub or the relay saw it.
    pub address: String,
    pub since: String,
}

/// The links up now, from `run/links.json`.
pub fn links_up(paths: &Paths) -> Vec<LinkRecord> {
    std::fs::read_to_string(paths.links()).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// The links of paired devices that are up, by the key on them (decision D83): what unpairing
/// a device closes at once, instead of when it next connects.
#[derive(Default)]
pub(crate) struct LivePeers {
    links: Mutex<HashMap<String, Vec<(u64, CancellationToken)>>>,
    next: std::sync::atomic::AtomicU64,
    /// Each link's record, by its number, and the file they are written to.
    records: Mutex<std::collections::BTreeMap<u64, LinkRecord>>,
    file: Option<PathBuf>,
}

impl LivePeers {
    /// Links that are written to `run/links.json` as they come and go.
    pub(crate) fn recorded(paths: &Paths) -> Self {
        let live = LivePeers { file: Some(paths.links()), ..Default::default() };
        live.write();
        live
    }

    /// A link of the key `fingerprint` is up; it ends when the token is cancelled. The number
    /// names it to `forget`.
    fn register(&self, fingerprint: &str, record: Option<LinkRecord>) -> (u64, CancellationToken) {
        let (id, token) = (self.next.fetch_add(1, Ordering::SeqCst), CancellationToken::new());
        self.links.lock().unwrap().entry(fingerprint.to_string()).or_default().push((id, token.clone()));
        if let Some(record) = record {
            self.records.lock().unwrap().insert(id, record);
            self.write();
        }
        (id, token)
    }

    fn forget(&self, fingerprint: &str, id: u64) {
        let mut links = self.links.lock().unwrap();
        if let Some(tokens) = links.get_mut(fingerprint) {
            tokens.retain(|(held, _)| *held != id);
            if tokens.is_empty() {
                links.remove(fingerprint);
            }
        }
        drop(links);
        if self.records.lock().unwrap().remove(&id).is_some() {
            self.write();
        }
    }

    fn write(&self) {
        let Some(file) = &self.file else { return };
        let records: Vec<LinkRecord> = self.records.lock().unwrap().values().cloned().collect();
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_vec_pretty(&records) {
            let _ = std::fs::write(file, text);
        }
    }

    /// Close every machine's node and talk links: this hub was handed over (decision D95), and
    /// they link again to the hub that took its place.
    pub(crate) fn close_all(&self) {
        for (_, tokens) in self.links.lock().unwrap().drain() {
            tokens.into_iter().for_each(|(_, token)| token.cancel());
        }
    }

    /// Close every link whose key `peers` no longer holds. Returns how many were closed.
    pub(crate) fn close_unpaired(&self, peers: &Peers) -> usize {
        let paired = peers.list();
        let mut links = self.links.lock().unwrap();
        let gone: Vec<String> = links.keys().filter(|key| !paired.contains_key(*key)).cloned().collect();
        let mut closed = 0;
        for key in gone {
            for (_, token) in links.remove(&key).unwrap_or_default() {
                token.cancel();
                closed += 1;
            }
        }
        if closed > 0 {
            log(&format!("closed {closed} link(s) of devices no longer paired"));
        }
        closed
    }
}

/// What every machine's link is handled with.
pub(crate) struct Remote {
    pub(crate) paths: Paths,
    pub(crate) primary: Arc<PrimaryStream>,
    pub(crate) nodes: Arc<crate::nodes::Nodes>,
    pub(crate) live: Arc<LivePeers>,
    /// The hub's direct addresses, for welcomes.
    pub(crate) directions: Arc<crate::direct::Directions>,
    /// A move or a step-down under way (decision D95).
    pub(crate) handover: Arc<super::Handover>,
    /// The relay's per-link rate, as it said when the hub registered; half of it is what a
    /// standby copy may take over a relayed link.
    pub(crate) relay_kbps: std::sync::atomic::AtomicU64,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl Remote {
    pub(crate) fn new(
        paths: Paths,
        primary: Arc<PrimaryStream>,
        nodes: Arc<crate::nodes::Nodes>,
        live: Arc<LivePeers>,
        directions: Arc<crate::direct::Directions>,
        handover: Arc<super::Handover>,
    ) -> std::io::Result<Arc<Remote>> {
        let acceptor = Identity::load_or_create(&paths.hub_key())?.acceptor()?;
        let relay_kbps = std::sync::atomic::AtomicU64::new(0);
        Ok(Arc::new(Remote { paths, primary, nodes, live, directions, handover, relay_kbps, acceptor }))
    }

    /// Take one machine's connection, from `from`, that came `via`: its handshake, then `peer`.
    pub(crate) async fn take<S>(self: &Arc<Self>, stream: S, from: String, via: Via)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let handshake = tokio::time::timeout(FIRST_WAIT, self.acceptor.accept(stream)).await;
        let stream = match handshake {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => return log(&format!("a link from {from} was not taken: its handshake failed ({error})")),
            Err(_) => return log(&format!("a link from {from} was not taken: its handshake took too long")),
        };
        if let Err(why) = peer(stream, from.clone(), via, self).await {
            log(&format!("a link from {from} was not taken: {why}"));
        }
    }
}

fn register_stamp(paths: &Paths) -> Option<std::time::SystemTime> {
    std::fs::metadata(paths.peers()).and_then(|meta| meta.modified()).ok()
}

/// How long a new link may take over its handshake and its first line.
const FIRST_WAIT: Duration = Duration::from_secs(10);
/// The longest first line: a hello with a token.
const HELLO_BYTES: usize = 1 << 12;

/// Take machines' links on the direct `listeners` until dropped, and close the links of a
/// machine removed from the register.
pub(super) async fn serve(listeners: Vec<tokio::net::TcpListener>, remote: Arc<Remote>) {
    // Dropped with this future: when the hub's service ends, no listener takes another link.
    let mut accepting = tokio::task::JoinSet::new();
    for listener in listeners {
        let remote = remote.clone();
        accepting.spawn(async move {
            while let Ok((stream, from)) = listener.accept().await {
                stream.set_nodelay(true).ok();
                let remote = remote.clone();
                tokio::spawn(async move { remote.take(stream, from.to_string(), Via::Direct).await });
            }
        });
    }
    // `nypi devices remove` says so on the user's socket; a register edited by hand is noticed here.
    let mut checks = tokio::time::interval(REGISTER_CHECK);
    let mut stamp = register_stamp(&remote.paths);
    loop {
        checks.tick().await;
        let now = register_stamp(&remote.paths);
        if now != stamp {
            stamp = now;
            remote.live.close_unpaired(&Peers::open(&remote.paths));
        }
    }
}

async fn peer<S>(
    stream: tokio_rustls::server::TlsStream<S>,
    from: String,
    via: Via,
    remote: &Arc<Remote>,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (paths, primary, nodes, live) = (&remote.paths, &remote.primary, &remote.nodes, &remote.live);
    let fingerprint = peer_fingerprint(&stream).ok_or("it showed no key")?;
    let join_value = nypi_node::identity::join_value_server(&stream);
    let (read, mut write) = tokio::io::split(stream);
    let mut read = BufReader::new(read);
    let first = tokio::time::timeout(FIRST_WAIT, nypi_node::link::read_line(&mut read, HELLO_BYTES)).await;
    let Ok(Ok(Some(line))) = first else { return Err("it said nothing".to_string()) };
    let hello: Hello = serde_json::from_str(&line).map_err(|_| "its first line was not a hello".to_string())?;

    let instance = crate::instance::Instance::load(paths).unwrap_or_default();
    let epoch = instance.epoch.max(1);
    if hello.epoch > epoch {
        remote.handover.step_down(hello.epoch);
        let refused = Answer::Refused { reason: format!("this hub was replaced by one at epoch {}", hello.epoch) };
        let _ = write_record(&mut write, &serde_json::to_string(&refused).unwrap_or_default()).await;
        return Err(format!("a machine knows of epoch {}, above this hub's {epoch}", hello.epoch));
    }
    let peers = Peers::open(paths);
    let known = peers.get(&fingerprint).filter(|peer| peer.kind == PeerKind::Machine);
    let outcome: Result<Peer, String> = match (hello.purpose, known) {
        // The epoch is no secret (mDNS says it too), and nothing is registered.
        (Purpose::Probe, _) => {
            let answer = serde_json::to_string(&Answer::Hub { epoch }).unwrap_or_default();
            let _ = write_record(&mut write, &answer).await;
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut write).await;
            return Ok(());
        }
        (Purpose::Leave, Some(peer)) => {
            let _ = peers.remove_key(&fingerprint);
            live.close_unpaired(&peers);
            log(&format!("{} left", peer.name));
            let left = Answer::Refused { reason: "this machine left the hub".into() };
            let _ = write_record(&mut write, &serde_json::to_string(&left).unwrap_or_default()).await;
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut write).await;
            return Ok(());
        }
        (Purpose::Talk, Some(peer)) if peer.trust != Trust::Personal => Err(format!(
            "{} was admitted as exec-only: it carries out calls, and cannot talk to the hub (nypi --standalone runs \
             a session of its own there)",
            peer.name
        )),
        (Purpose::Standby | Purpose::Move | Purpose::Notice, Some(peer)) if peer.trust != Trust::Personal => Err(
            format!("{} was admitted as exec-only: the hub's state is never sent to it, nor moved there", peer.name),
        ),
        (_, Some(peer)) => Ok(peer),
        (Purpose::Join, None) => {
            let code =
                join_value.map(|value| nypi_node::identity::confirmation_code(&hub_key(paths), &fingerprint, &value));
            match code {
                Some(code) => admit(paths, primary, &fingerprint, &hello, &from, &code, &mut read, &mut write).await,
                None => Err("no session value to make a code from".into()),
            }
        }
        (_, None) => Err("this machine is not admitted to this hub (nypi node up asks to join)".to_string()),
    };
    let peer = match outcome {
        Ok(peer) => peer,
        Err(reason) => {
            let refused = serde_json::to_string(&Answer::Refused { reason: reason.clone() }).unwrap_or_default();
            let _ = write_record(&mut write, &refused).await;
            // Said, then closed in order, so the machine reads the reason and not a broken link.
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut write).await;
            return Err(reason);
        }
    };
    // Registered before the welcome, so a removal from now on reaches this link.
    let record = LinkRecord {
        key: fingerprint.clone(),
        name: peer.name.clone(),
        purpose: hello.purpose,
        path: via,
        address: from.clone(),
        since: Utc::now().to_rfc3339(),
    };
    let (link, stop) = live.register(&fingerprint, Some(record));
    let welcome =
        Answer::Welcome { name: peer.name.clone(), trust: peer.trust, epoch, direct: remote.directions.now() };
    write_record(&mut write, &serde_json::to_string(&welcome).unwrap_or_default())
        .await
        .map_err(|error| error.to_string())?;
    let path = match via {
        Via::Direct => "directly",
        Via::Relayed => "through the relay",
    };
    log(&format!("{} ({}) linked {path} for {:?}", peer.name, peer.trust.label(), hello.purpose));
    match hello.purpose {
        // The node protocol, under the name it was admitted as: a node cannot say it is another
        // machine, and nothing it sends is read as a command.
        Purpose::Node => nodes.link_until(read, write, Some(&peer.name), stop.clone()).await,
        // Only a personal machine gets here (checked above): it talks as the user.
        Purpose::Talk => {
            let mut lines = read.lines();
            let first = match tokio::time::timeout(ATTACH_WAIT, lines.next_line()).await {
                Ok(Ok(Some(line))) => Some(line),
                Ok(_) => None,
                Err(_) => Some(String::new()),
            };
            let command = first.as_deref().and_then(|line| serde_json::from_str::<Value>(line).ok());
            match command.as_ref().map(crate::protocol::parse) {
                // One of the hub's own commands (decision D96): a personal machine only, and only
                // what the allowlist names; it never reaches the conversation.
                Some(crate::protocol::HubCommand::Admin { argv, input }) => {
                    let (output, code) = match crate::protocol::admin_allowed(&argv, input.is_some()) {
                        Ok(()) if peer.trust == Trust::Personal => {
                            log(&format!("{} runs nypi {}", peer.name, argv.first().cloned().unwrap_or_default()));
                            let context = crate::protocol::AdminContext { nodes: Some(nodes.clone()) };
                            crate::protocol::run_admin(paths, argv, input, context).await
                        }
                        Ok(()) => ("the hub's commands are for the user's own machines".to_string(), 1),
                        Err(why) => (why, 2),
                    };
                    let result = json!({ "type": "admin_result", "output": output, "code": code });
                    let _ = write_record(&mut write, &result.to_string()).await;
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut write).await;
                }
                _ if first.is_none() => {}
                _ => {
                    let first = first.filter(|line| !line.is_empty());
                    attach_from(primary.clone(), first, lines, write, Some(peer.name.clone()), Some(stop.clone())).await
                }
            }
        }
        // Only a personal machine gets here for these three (checked above).
        Purpose::Standby => {
            tokio::select! {
                _ = super::handover::serve_copy(remote, &mut read, &mut write, via) => {}
                _ = stop.cancelled() => {}
            }
        }
        Purpose::Move => {
            if let Err(why) = super::handover::move_out(remote, &peer, &mut read, &mut write, via).await {
                log(&format!("the move to {} did not happen: {why}", peer.name));
            }
        }
        Purpose::Notice => {
            let line = tokio::time::timeout(FIRST_WAIT, nypi_node::link::read_line(&mut read, 4096)).await;
            if let Ok(Ok(Some(line))) = line {
                let text: Value = serde_json::from_str(&line).unwrap_or_default();
                if let Some(text) = text["text"].as_str() {
                    tell(paths, &format!("From {}: {text}", peer.name)).await;
                }
            }
        }
        // A join of a key already admitted: the welcome was its answer.
        Purpose::Join | Purpose::Leave | Purpose::Probe => {}
    }
    live.forget(&fingerprint, link);
    Ok(())
}

fn hub_key(paths: &Paths) -> String {
    Identity::load_or_create(&paths.hub_key()).map(|key| key.fingerprint()).unwrap_or_default()
}

/// How often a waiting join looks for the user's answer.
const ANSWER_LOOK: Duration = Duration::from_millis(200);

/// A key the hub does not know asks to join (decision D93). An invite code admits it at once;
/// otherwise a request is recorded (within its limits), the user is asked wherever approvals go,
/// and this waits for their answer while the link stays open. Nothing here reaches the
/// conversation or wakes the Principal: a request is a notice.
#[allow(clippy::too_many_arguments)]
async fn admit<R, W>(
    paths: &Paths,
    primary: &Arc<PrimaryStream>,
    fingerprint: &str,
    hello: &Hello,
    from: &str,
    code: &str,
    read: &mut BufReader<R>,
    write: &mut W,
) -> Result<Peer, String>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let name = crate::instance::plain_name(&hello.name);
    let name = if name.is_empty() { "machine".to_string() } else { name };
    let peer = |name: String, trust: Trust| Peer {
        name,
        trust,
        instance_id: hello.instance_id.clone(),
        kind: PeerKind::Machine,
        system: hello.system.chars().take(120).collect(),
        ..Default::default()
    };
    if let Some(token) = hello.token.as_deref() {
        return match pairing::redeem(paths, token) {
            Some((invited, trust)) => Peers::open(paths).add(fingerprint, peer(invited, trust)),
            None => Err("this invite code is unknown, used, or expired".to_string()),
        };
    }
    let request = pairing::open_request(
        paths,
        pairing::NewRequest {
            kind: PeerKind::Machine,
            name: name.clone(),
            system: hello.system.chars().take(120).collect(),
            from: from.to_string(),
            code: code.to_string(),
            key: fingerprint.to_string(),
            instance_id: hello.instance_id.clone(),
        },
    );
    let request = match request {
        Ok(request) => request,
        Err(refusal) => {
            if refusal.tell_user {
                let text = "Join requests are being refused until tomorrow: more came today than NyPi takes. If you \
                            did not ask for them, someone else is trying the entry.";
                tell(paths, text).await;
            }
            return Err(refusal.reason);
        }
    };
    log(&format!("join request from {from}: {name}"));
    let _ = primary.thread().hub.lines.send(request.record().to_string().into());
    tell(paths, &request.question()).await;
    let waiting = Answer::Waiting { expires: request.expires.clone() };
    write_record(write, &serde_json::to_string(&waiting).unwrap_or_default()).await.map_err(|e| e.to_string())?;
    let mut look = tokio::time::interval(ANSWER_LOOK);
    let mut ignored = String::new();
    let answer = loop {
        tokio::select! {
            _ = look.tick() => {}
            // The machine gave up (Ctrl-C): nothing more to wait for.
            gone = tokio::io::AsyncBufReadExt::read_line(read, &mut ignored) => {
                if matches!(gone, Ok(0) | Err(_)) {
                    pairing::forget_request(paths, &request.id);
                    return Err("the machine went away before an answer".into());
                }
                ignored.clear();
            }
        }
        // Read from the file each time: the answer, and the expiry, are what it says now.
        match pairing::request(paths, &request.id) {
            Some(now) if now.answer.is_some() => break now.answer,
            Some(now) if expired(&now.expires) => break None,
            Some(_) => {}
            None => break None,
        }
    };
    pairing::forget_request(paths, &request.id);
    let _ = primary.thread().hub.lines.send(json!({ "type": "join_resolved", "id": request.id }).to_string().into());
    match answer {
        Some(JoinAnswer::Admit { trust }) => {
            let admitted = Peers::open(paths).add(fingerprint, peer(name, trust))?;
            log(&format!("{} admitted as {}", admitted.name, trust.label()));
            Ok(admitted)
        }
        Some(JoinAnswer::Refuse) => Err("the user refused this machine".into()),
        None => Err(format!("nobody answered within {} minutes", pairing::REQUEST_MINUTES)),
    }
}

fn expired(expires: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(expires).map_or(true, |expires| chrono::Utc::now() > expires)
}

/// Tell the user through the outbox, which every running channel is offered.
pub(super) async fn tell(paths: &Paths, text: &str) {
    let notice = crate::notify::Notice {
        source: "devices",
        level: nypi_core::inspector::AlertLevel::Warning,
        text,
        task: None,
        shown_in: None,
    };
    crate::notify::post(paths, None, notice).await;
}
