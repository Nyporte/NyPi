//! The relay (`nypi relay run`, decision D94): a plain HTTP/1.1 and WebSocket server behind the
//! entry. Cloudflare (its proxy or a Tunnel) or a reverse proxy serves the entry's certificate
//! and passes requests here, so the machine it runs on needs no public port of its own and its
//! address can stay hidden. Three paths:
//!
//! - `GET /_nypi/hub`: the hub's control connection. The relay sends a random challenge; the hub
//!   signs it, with its epoch and instance id, with the hub key. The relay takes only the key
//!   `relay.hubKey` names (or, without one, the first to register), and tells the hub the address
//!   it sees it come from.
//! - `GET /_nypi/link`: a machine's link. The relay asks the hub, over the control connection, to
//!   open a WebSocket of its own to `/_nypi/data/<id>` with a one-time token, and passes binary
//!   frames between the two. What the frames carry is TLS between the machine's key and the
//!   hub's: the relay cannot read it, and does not look.
//! - Any other path: a browser, for the board. The relay asks the hub for a data WebSocket of
//!   kind `board` and passes the request and the streamed answer (server-sent events included,
//!   unbuffered) to the hub, which hands them to the board's public socket.
//!
//! The relay keeps one small file (`relay/state.json`): the hub key it takes, the epoch, and
//! the instance registered at that epoch. Nothing in it is secret. Its limits: connections per
//! address per minute, connections waiting for the hub, the size of a request it holds, and
//! `maxKbpsPerLink`. With no hub registered it refuses links and answers browsers 503.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use nypi_core::config::{Paths, RelaySettings};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::ws::{self, Head, Message};

pub const HUB_PATH: &str = "/_nypi/hub";
pub const DATA_PATH: &str = "/_nypi/data/";
pub const TOKEN_HEADER: &str = "X-NyPi-Token";
/// The browser's address, as the relay saw it, on a request passed to the board.
pub const CLIENT_HEADER: &str = "X-NyPi-Client";
/// How long a connection waits for the hub's data connection.
const DATA_WAIT: Duration = Duration::from_secs(10);
/// How long a request's head may take to arrive.
const HEAD_WAIT: Duration = Duration::from_secs(15);
/// The largest request body passed to the board.
const MAX_BODY: usize = 1 << 20;
/// Both ends of the control connection say something this often; one silent for three times as
/// long is gone.
const PING_EVERY: Duration = Duration::from_secs(25);

type Socket = (ws::Reader<BufReader<OwnedReadHalf>>, ws::Writer<OwnedWriteHalf>);

/// What the relay keeps across restarts (decision D94; the epoch rules are decision D95's).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub hub_key: Option<String>,
    pub epoch: u64,
    pub instance_id: String,
}

/// The control connection's messages, as JSON in text frames.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Control {
    /// Relay to hub, first: sign this.
    Challenge { nonce: String },
    /// Hub to relay: the hub key's public half, and its signature of `registration`.
    Register { public_key: String, epoch: u64, instance_id: String, signature: String },
    /// Relay to hub: taken; `seen` is the address the hub came from, and `max_kbps_per_link` the
    /// rate one link may take here (a standby copy takes at most half of it).
    Registered {
        seen: String,
        #[serde(default)]
        max_kbps_per_link: u64,
    },
    /// Relay to hub: not taken; `epoch` when another hub holds a higher one (decision D95).
    Refused {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epoch: Option<u64>,
    },
    /// Relay to hub: open `/_nypi/data/<id>` with `token`, for a `link` or a `board` request
    /// from `from`.
    Open { id: String, token: String, kind: String, from: String },
}

/// What the hub signs: the relay's challenge, its epoch, and its instance id.
pub fn registration(nonce: &str, epoch: u64, instance_id: &str) -> Vec<u8> {
    format!("nypi relay registration\n{nonce}\n{epoch}\n{instance_id}").into_bytes()
}

struct Waiting {
    token: String,
    socket: oneshot::Sender<Socket>,
}

struct HubLink {
    number: u64,
    say: mpsc::UnboundedSender<Message>,
    gone: CancellationToken,
}

/// A running relay.
pub struct Relay {
    settings: RelaySettings,
    state_file: PathBuf,
    state: Mutex<State>,
    hub: Mutex<Option<HubLink>>,
    waiting: Mutex<HashMap<String, Waiting>>,
    per_address: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
    next: AtomicU64,
    /// Data connections the hub opened, for tests and the log: how much went through here.
    pub carried: AtomicU64,
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut buffer);
    buffer.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn log(text: &str) {
    eprintln!("[{}] relay: {text}", chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
}

impl Relay {
    pub fn new(paths: &Paths, settings: RelaySettings) -> Arc<Relay> {
        let state_file = paths.relay_state();
        let state: State = std::fs::read_to_string(&state_file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Arc::new(Relay {
            settings,
            state_file,
            state: Mutex::new(state),
            hub: Mutex::default(),
            waiting: Mutex::default(),
            per_address: Mutex::default(),
            next: AtomicU64::new(1),
            carried: AtomicU64::new(0),
        })
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn has_hub(&self) -> bool {
        self.hub.lock().unwrap().is_some()
    }

    fn save(&self, state: &State) {
        if let Some(dir) = self.state_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_vec_pretty(state) {
            let _ = std::fs::write(&self.state_file, text);
        }
    }

    /// The client's address: what Cloudflare or the proxy says when they are trusted, else the
    /// connection's.
    /// Decision D99: the headers are believed only when the connection comes from loopback, where
    /// Nginx, Caddy, or `cloudflared` runs; from anywhere else a client could write them itself.
    /// A list's last address is the one the proxy added, so one a client put first is never
    /// taken.
    pub(crate) fn client(&self, head: &Head, peer: SocketAddr) -> IpAddr {
        if !self.settings.trust_proxy || !peer.ip().is_loopback() {
            return peer.ip();
        }
        let last = |value: &str| value.rsplit(',').find_map(|address| address.trim().parse::<IpAddr>().ok());
        let said = match self.settings.trusted_header.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => head.get(name).and_then(last),
            None => head
                .get("cf-connecting-ip")
                .and_then(last)
                .or_else(|| head.get("x-real-ip").and_then(last))
                .or_else(|| head.get("x-forwarded-for").and_then(last)),
        };
        said.unwrap_or(peer.ip())
    }

    /// One more connection from `address`, within the limit per minute.
    fn within_rate(&self, address: IpAddr) -> bool {
        let now = Instant::now();
        let mut table = self.per_address.lock().unwrap();
        if table.len() > 10_000 {
            table.retain(|_, times| {
                times.back().is_some_and(|last| now.duration_since(*last) < Duration::from_secs(60))
            });
        }
        let times = table.entry(address).or_default();
        while times.front().is_some_and(|first| now.duration_since(*first) >= Duration::from_secs(60)) {
            times.pop_front();
        }
        if times.len() >= self.settings.per_ip_per_minute as usize {
            return false;
        }
        times.push_back(now);
        true
    }

    fn bytes_per_second(&self) -> u64 {
        self.settings.max_kbps_per_link * 1000 / 8
    }

    /// Ask the hub for a data connection of `kind` for a client at `from`; the socket, or None.
    async fn data_connection(&self, kind: &str, from: IpAddr) -> Option<Socket> {
        let say = self.hub.lock().unwrap().as_ref().map(|hub| hub.say.clone())?;
        let (id, token) = (random_hex(12), random_hex(24));
        let (socket, arrives) = oneshot::channel();
        {
            let mut waiting = self.waiting.lock().unwrap();
            if waiting.len() >= self.settings.max_waiting {
                return None;
            }
            waiting.insert(id.clone(), Waiting { token: token.clone(), socket });
        }
        let open = Control::Open { id: id.clone(), token, kind: kind.to_string(), from: from.to_string() };
        let _ = say.send(Message::Text(serde_json::to_string(&open).unwrap_or_default()));
        let socket = tokio::time::timeout(DATA_WAIT, arrives).await.ok().and_then(Result::ok);
        self.waiting.lock().unwrap().remove(&id);
        if socket.is_some() {
            self.carried.fetch_add(1, Ordering::SeqCst);
        }
        socket
    }
}

fn pid_file(paths: &Paths) -> PathBuf {
    paths.home.join("run").join("relay.pid")
}

/// The running relay's process, if one runs.
pub fn running(paths: &Paths) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_file(paths)).ok()?.trim().parse().ok()?;
    nypi_core::ledger::process_alive(pid).then_some(pid)
}

/// `nypi relay run`: the relay in the foreground, with this machine's settings.
pub async fn run(paths: &Paths, settings: RelaySettings) -> anyhow::Result<i32> {
    let listener =
        tokio::net::TcpListener::bind(&settings.listen).await.map_err(|error| bind_error(&settings.listen, error))?;
    let file = pid_file(paths);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&file, std::process::id().to_string())?;
    let relay = Relay::new(paths, settings.clone());
    match settings.hub_key.as_deref().or(relay.state().hub_key.as_deref()) {
        Some(key) => log(&format!("listening on {}, for the hub key {key}", settings.listen)),
        None => log(&format!("listening on {}; the first hub to register claims this relay", settings.listen)),
    }
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = serve(listener, relay) => {}
        _ = terminate.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    log("stopped");
    let _ = std::fs::remove_file(file);
    Ok(0)
}

/// What to say when the relay's address cannot be opened.
pub fn bind_error(listen: &str, error: std::io::Error) -> anyhow::Error {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        anyhow::anyhow!(
            "{listen} could not be opened: a port below 1024 needs `sudo setcap cap_net_bind_service=+ep ~/.nypi/bin/nypi` on Linux; the relay is meant to listen on 127.0.0.1:7682 behind Cloudflare Tunnel or a reverse proxy"
        )
    } else {
        anyhow::anyhow!("{listen} could not be opened: {error}")
    }
}

/// Take connections on `listener` until dropped.
pub async fn serve(listener: tokio::net::TcpListener, relay: Arc<Relay>) {
    while let Ok((stream, peer)) = listener.accept().await {
        stream.set_nodelay(true).ok();
        let relay = relay.clone();
        tokio::spawn(async move { connection(relay, stream, peer).await });
    }
}

async fn answer(write: &mut OwnedWriteHalf, status: u16, reason: &str, text: &str) {
    let _ = write.write_all(&ws::plain_response(status, reason, text)).await;
    let _ = write.shutdown().await;
}

async fn connection(relay: Arc<Relay>, stream: TcpStream, peer: SocketAddr) {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let Ok(Ok(Some(head))) = tokio::time::timeout(HEAD_WAIT, ws::read_head(&mut read, true)).await else {
        return answer(&mut write, 400, "Bad Request", "").await;
    };
    let client = relay.client(&head, peer);
    let path = head.path.split('?').next().unwrap_or_default().to_string();
    if path == HUB_PATH && head.is_upgrade() {
        return control(relay, head, read, write, client).await;
    }
    if let Some(id) = path.strip_prefix(DATA_PATH)
        && head.is_upgrade()
    {
        let id = id.to_string();
        return data(relay, id, head, read, write).await;
    }
    if !relay.within_rate(client) {
        return answer(&mut write, 429, "Too Many Requests", "Too many connections from this address; try later.\n")
            .await;
    }
    if path == crate::pairing::LINK_PATH && head.is_upgrade() {
        return link(relay, head, read, write, client).await;
    }
    if path.starts_with("/_nypi/") {
        return answer(&mut write, 404, "Not Found", "").await;
    }
    board(relay, head, read, write, client).await;
}

/// The hub's control connection.
async fn control(
    relay: Arc<Relay>,
    head: Head,
    read: BufReader<OwnedReadHalf>,
    mut write: OwnedWriteHalf,
    seen: IpAddr,
) {
    if write.write_all(&ws::upgrade_response(&head)).await.is_err() {
        return;
    }
    let (mut reader, mut writer) = (ws::Reader::new(read), ws::Writer::new(write, false));
    let nonce = random_hex(32);
    let challenge = serde_json::to_string(&Control::Challenge { nonce: nonce.clone() }).unwrap_or_default();
    if writer.send(&Message::Text(challenge)).await.is_err() {
        return;
    }
    let said = tokio::time::timeout(Duration::from_secs(10), reader.next()).await;
    let Ok(Ok(Some(Message::Text(text)))) = said else { return };
    let decision = match serde_json::from_str::<Control>(&text) {
        Ok(Control::Register { public_key, epoch, instance_id, signature }) => {
            accept(&relay, &nonce, &public_key, epoch, &instance_id, &signature)
        }
        _ => Err(("the first message was not a registration".to_string(), None)),
    };
    if let Err((reason, epoch)) = decision {
        log(&format!("refused a hub from {seen}: {reason}"));
        let refused = serde_json::to_string(&Control::Refused { reason, epoch }).unwrap_or_default();
        let _ = writer.send(&Message::Text(refused)).await;
        let _ = writer.send(&Message::Close).await;
        return;
    }
    let max_kbps_per_link = relay.settings.max_kbps_per_link;
    let registered =
        serde_json::to_string(&Control::Registered { seen: seen.to_string(), max_kbps_per_link }).unwrap_or_default();
    if writer.send(&Message::Text(registered)).await.is_err() {
        return;
    }
    let (say, mut said) = mpsc::unbounded_channel::<Message>();
    let gone = CancellationToken::new();
    let number = relay.next.fetch_add(1, Ordering::SeqCst);
    // A new registration takes the place of the one before: that hub is gone, or moved.
    if let Some(earlier) = relay.hub.lock().unwrap().replace(HubLink { number, say: say.clone(), gone: gone.clone() }) {
        earlier.gone.cancel();
    }
    log(&format!("a hub registered from {seen}"));
    let writing = tokio::spawn({
        let gone = gone.clone();
        async move {
            let mut pings = tokio::time::interval(PING_EVERY);
            loop {
                let message = tokio::select! {
                    Some(message) = said.recv() => message,
                    _ = pings.tick() => Message::Ping(Vec::new()),
                    _ = gone.cancelled() => Message::Close,
                };
                let closing = message == Message::Close;
                if writer.send(&message).await.is_err() || closing {
                    break;
                }
            }
            writer.shutdown().await;
        }
    });
    loop {
        let heard = tokio::select! {
            heard = tokio::time::timeout(PING_EVERY * 3, reader.next()) => heard,
            _ = gone.cancelled() => break,
        };
        match heard {
            Ok(Ok(Some(Message::Ping(bytes)))) => {
                let _ = say.send(Message::Pong(bytes));
            }
            Ok(Ok(Some(Message::Close) | None)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(Some(_))) => {}
        }
    }
    gone.cancel();
    let _ = writing.await;
    let mut hub = relay.hub.lock().unwrap();
    if hub.as_ref().is_some_and(|hub| hub.number == number) {
        *hub = None;
        log("the hub's control connection closed");
    }
}

/// Whether a registration is taken (decisions D94, D95): the signature is the hub key's, the
/// key is the one this relay takes, and the epoch is above the one it holds, or the same from
/// the instance registered at it. Exactly one hub: one at a lower epoch is refused, and told the
/// epoch that holds the entry.
fn accept(
    relay: &Relay,
    nonce: &str,
    public_key: &str,
    epoch: u64,
    instance_id: &str,
    signature: &str,
) -> Result<(), (String, Option<u64>)> {
    let decode = |text: &str| base64::engine::general_purpose::STANDARD.decode(text).ok();
    let (Some(public_key), Some(signature)) = (decode(public_key), decode(signature)) else {
        return Err(("the registration did not decode".into(), None));
    };
    let fingerprint = nypi_node::identity::key_fingerprint(&public_key);
    let message = registration(nonce, epoch, instance_id);
    if !nypi_node::identity::verify(&fingerprint, &public_key, &message, &signature) {
        return Err(("the signature is not the key's".into(), None));
    }
    let mut state = relay.state.lock().unwrap();
    let pinned = relay.settings.hub_key.clone().or_else(|| state.hub_key.clone());
    match pinned {
        Some(pinned) if !same(&pinned.to_ascii_lowercase(), &fingerprint) => {
            return Err((format!("this relay takes the hub key {pinned} only"), None));
        }
        Some(_) => {}
        None => log(&format!("claimed by the hub key {fingerprint}: compare it with what `nypi hub up` showed")),
    }
    let held = state.epoch;
    let other_instance = !state.instance_id.is_empty() && state.instance_id != instance_id;
    if epoch < held || (epoch == held && other_instance) {
        return Err((format!("a hub at epoch {held} holds this entry"), Some(held)));
    }
    let next =
        State { hub_key: Some(fingerprint), epoch: epoch.max(state.epoch), instance_id: instance_id.to_string() };
    if next != *state {
        relay.save(&next);
        *state = next;
    }
    Ok(())
}

/// The hub's data connection, for a connection waiting under `id`.
async fn data(relay: Arc<Relay>, id: String, head: Head, read: BufReader<OwnedReadHalf>, mut write: OwnedWriteHalf) {
    let waiting = {
        let mut table = relay.waiting.lock().unwrap();
        let matches =
            table.get(&id).is_some_and(|waiting| same(head.get(TOKEN_HEADER).unwrap_or_default(), &waiting.token));
        if matches { table.remove(&id) } else { None }
    };
    let Some(waiting) = waiting else { return answer(&mut write, 404, "Not Found", "").await };
    if write.write_all(&ws::upgrade_response(&head)).await.is_err() {
        return;
    }
    let _ = waiting.socket.send((ws::Reader::new(read), ws::Writer::new(write, false)));
}

/// At most `rate` bytes a second through one direction of one link; 0 for no limit.
struct Pace {
    rate: u64,
    free_at: Instant,
}

impl Pace {
    fn new(rate: u64) -> Pace {
        Pace { rate, free_at: Instant::now() }
    }

    async fn carry(&mut self, bytes: usize) {
        if self.rate == 0 {
            return;
        }
        let now = Instant::now();
        let start = self.free_at.max(now);
        self.free_at = start + Duration::from_secs_f64(bytes as f64 / self.rate as f64);
        // A quarter second of bytes may go at once; past that, wait.
        if self.free_at > now + Duration::from_millis(250) {
            tokio::time::sleep_until(self.free_at - Duration::from_millis(250)).await;
        }
    }
}

/// Pass frames from one WebSocket to the other until either closes.
async fn pass<R, W>(from: &mut ws::Reader<R>, to: &mut ws::Writer<W>, mut pace: Pace)
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    while let Ok(Some(message)) = from.next().await {
        if let Message::Binary(bytes) = &message {
            pace.carry(bytes.len()).await;
        }
        let closing = message == Message::Close;
        if to.send(&message).await.is_err() || closing {
            break;
        }
    }
    let _ = to.send(&Message::Close).await;
    to.shutdown().await;
}

/// A machine's link: joined to a data connection of the hub's, frames passed both ways, unread.
async fn link(relay: Arc<Relay>, head: Head, read: BufReader<OwnedReadHalf>, mut write: OwnedWriteHalf, from: IpAddr) {
    if !relay.has_hub() {
        return answer(&mut write, 503, "Service Unavailable", "No hub is registered at this entry.\n").await;
    }
    let Some((mut hub_reader, mut hub_writer)) = relay.data_connection("link", from).await else {
        return answer(&mut write, 503, "Service Unavailable", "The hub did not answer.\n").await;
    };
    if write.write_all(&ws::upgrade_response(&head)).await.is_err() {
        return;
    }
    let (mut reader, mut writer) = (ws::Reader::new(read), ws::Writer::new(write, false));
    let rate = relay.bytes_per_second();
    tokio::join!(
        pass(&mut reader, &mut hub_writer, Pace::new(rate)),
        pass(&mut hub_reader, &mut writer, Pace::new(rate))
    );
}

const NO_HUB: &str = "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>NyPi</title><body style=\"font:16px system-ui;margin:3em auto;max-width:32em;padding:0 1em\"><h1>The hub is not here</h1><p>No NyPi hub is registered at this entry right now. If it is yours, start it with <code>nypi hub up</code>.</p>";

/// A browser's request for the board: passed to the hub with the browser's address, the answer
/// passed back as it comes. One request per connection.
async fn board(
    relay: Arc<Relay>,
    head: Head,
    mut read: BufReader<OwnedReadHalf>,
    mut write: OwnedWriteHalf,
    from: IpAddr,
) {
    if !relay.has_hub() {
        let page = format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{NO_HUB}",
            NO_HUB.len()
        );
        let _ = write.write_all(page.as_bytes()).await;
        let _ = write.shutdown().await;
        return;
    }
    if head.get("transfer-encoding").is_some() {
        return answer(&mut write, 411, "Length Required", "").await;
    }
    let length: usize = head.get("content-length").and_then(|n| n.parse().ok()).unwrap_or(0);
    if length > MAX_BODY {
        return answer(&mut write, 413, "Payload Too Large", "").await;
    }
    let mut body = vec![0u8; length];
    if tokio::time::timeout(HEAD_WAIT, read.read_exact(&mut body)).await.map_or(true, |read| read.is_err()) {
        return;
    }
    let Some((mut hub_reader, mut hub_writer)) = relay.data_connection("board", from).await else {
        return answer(&mut write, 503, "Service Unavailable", "The hub did not answer.\n").await;
    };
    let address = from.to_string();
    let mut request = head.request_bytes(
        &["connection", "keep-alive", "upgrade", "proxy-connection", CLIENT_HEADER],
        &[("Connection", "close"), (CLIENT_HEADER, &address)],
    );
    request.extend_from_slice(&body);
    if hub_writer.send(&Message::Binary(request)).await.is_err() {
        return;
    }
    let mut pace = Pace::new(relay.bytes_per_second());
    let mut ignored = [0u8; 512];
    loop {
        tokio::select! {
            message = hub_reader.next() => match message {
                Ok(Some(Message::Binary(bytes))) => {
                    pace.carry(bytes.len()).await;
                    if write.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Ok(Some(Message::Ping(bytes))) => {
                    let _ = hub_writer.send(&Message::Pong(bytes)).await;
                }
                Ok(Some(Message::Text(_) | Message::Pong(_))) => {}
                _ => break,
            },
            // The browser went away (a closed tab ends its event stream).
            gone = read.read(&mut ignored) => if matches!(gone, Ok(0) | Err(_)) { break },
        }
    }
    let _ = hub_writer.send(&Message::Close).await;
    hub_writer.shutdown().await;
    let _ = write.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(settings: RelaySettings) -> (tempfile::TempDir, Arc<Relay>) {
        let home = tempfile::tempdir().unwrap();
        let relay = Relay::new(&Paths { home: home.path().to_path_buf() }, settings);
        (home, relay)
    }

    fn head(headers: &[(&str, &str)]) -> Head {
        Head {
            method: "GET".into(),
            path: "/".into(),
            status: 0,
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    /// Decision D99: a forwarded address is believed only from the proxy on loopback, a list's last
    /// entry is the proxy's own, and a named header is the only one read.
    #[test]
    fn the_clients_address_is_taken_only_from_the_proxy() {
        let (_home, trusting) = relay(RelaySettings::default());
        let local: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let public: SocketAddr = "203.0.113.50:50000".parse().unwrap();
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        let cloudflare = head(&[("CF-Connecting-IP", "198.51.100.7")]);
        assert_eq!(trusting.client(&cloudflare, local), ip("198.51.100.7"));
        assert_eq!(trusting.client(&cloudflare, public), ip("203.0.113.50"), "not believed from the internet");
        let hops = head(&[("X-Forwarded-For", "1.2.3.4, 198.51.100.8")]);
        assert_eq!(trusting.client(&hops, local), ip("198.51.100.8"), "the proxy's entry, not the client's");
        assert_eq!(trusting.client(&head(&[("X-Real-IP", "198.51.100.9")]), local), ip("198.51.100.9"));
        assert_eq!(trusting.client(&head(&[("X-Forwarded-For", "nonsense")]), local), ip("127.0.0.1"));
        let ipv6: SocketAddr = "[::1]:50000".parse().unwrap();
        assert_eq!(trusting.client(&cloudflare, ipv6), ip("198.51.100.7"), "loopback over IPv6 too");

        let named = RelaySettings { trusted_header: Some("x-real-ip".into()), ..Default::default() };
        let (_home, named) = relay(named);
        let spoofed = head(&[("CF-Connecting-IP", "6.6.6.6"), ("X-Real-IP", "198.51.100.10")]);
        assert_eq!(named.client(&spoofed, local), ip("198.51.100.10"), "only the named header");
        assert_eq!(named.client(&head(&[("CF-Connecting-IP", "6.6.6.6")]), local), ip("127.0.0.1"));

        let (_home, plain) = relay(RelaySettings { trust_proxy: false, ..Default::default() });
        assert_eq!(plain.client(&cloudflare, local), ip("127.0.0.1"));
    }
}
