//! Which machines and browsers the hub admits, and as what (decisions D80, D93). Each machine has
//! one key; the hub's register (`devices/peers.json`) keeps, by that key's fingerprint, the name
//! the machine was admitted under and its **trust**: `personal` (the user's own machine: it
//! carries out calls and talks to the hub as the user) or `exec-only` (it carries out calls and
//! nothing else). A browser admitted to the board is kept the same way, under the hash of its
//! session cookie.
//!
//! A link says what it is for in its first line (`Hello { purpose }`): `node` (the node
//! protocol), `talk` (the conversation, for a personal machine only), `join` (a key the hub does
//! not know, asking to be admitted), or `leave`. What a link may do is decided in
//! `daemon/remote.rs` by the trust the machine was admitted with and the purpose, never by
//! anything the machine says about itself.
//!
//! Joining is approved by the user: both machines show a six-digit code made from both keys and
//! the session (`identity::confirmation_code`), and the user admits the request whose code
//! matches the screen in front of them. Requests last ten minutes and are limited (three waiting
//! at once, ten a day from one address, thirty a day in all); they are notices, never a task of
//! Lucia's. An invite code (`nypi devices invite`) admits once with no question, and is kept only
//! as a hash.

use std::collections::BTreeMap;
use std::path::Path;

use nypi_core::config::Paths;
use nypi_node::identity::{Identity, write_private};
use serde::{Deserialize, Serialize};

/// How long a join request and an invite code (by default) last.
pub const REQUEST_MINUTES: i64 = 10;
/// Join requests waiting at once.
pub const MAX_WAITING: usize = 3;
/// Join requests a day from one address, and in all.
pub const MAX_PER_ADDRESS: u32 = 10;
pub const MAX_PER_DAY: u32 = 30;

/// As what a machine or browser was admitted. Fixed when it is admitted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trust {
    /// The user's own: carries out calls and talks to the hub as the user; may hold a standby
    /// copy of the hub and become the hub.
    #[default]
    Personal,
    /// Carries out calls only.
    ExecOnly,
}

impl Trust {
    pub fn label(self) -> &'static str {
        match self {
            Trust::Personal => "personal",
            Trust::ExecOnly => "exec-only",
        }
    }
}

/// A machine, or a browser admitted to the board.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerKind {
    #[default]
    Machine,
    Browser,
}

/// An admitted machine or browser, by its key's fingerprint (or session's hash) in the register.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Peer {
    pub name: String,
    pub trust: Trust,
    pub paired_at: String,
    pub instance_id: String,
    pub kind: PeerKind,
    /// The operating system and NyPi version it said when it joined (a browser: its user agent).
    pub system: String,
}

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(path, &serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?)
}

/// Run `change` with the devices folder's lock held, so the daemon, the board, and a command
/// answering a request never write the same file at once (a record lock, decision D83).
fn locked<T>(paths: &Paths, change: impl FnOnce() -> T) -> T {
    let file = paths.peers().with_file_name("devices.lock");
    let started = std::time::Instant::now();
    loop {
        if let Some(_held) = crate::round_lock::RoundLock::try_take(&file) {
            return change();
        }
        if started.elapsed() > std::time::Duration::from_secs(3) {
            // A holder that hangs must not stop the user's answer: write without it.
            return change();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// The hub's register of admitted machines and browsers (`devices/peers.json`).
pub struct Peers {
    path: std::path::PathBuf,
    paths: Paths,
}

impl Peers {
    /// The register, moved to D93's form first when it is still D80's (so a device paired only
    /// as a node is never read as a personal machine).
    pub fn open(paths: &Paths) -> Peers {
        migrate_peers(paths);
        Peers { path: paths.peers(), paths: paths.clone() }
    }

    pub fn list(&self) -> BTreeMap<String, Peer> {
        read_json(&self.path)
    }

    pub fn get(&self, fingerprint: &str) -> Option<Peer> {
        self.list().remove(fingerprint)
    }

    /// Admit the machine or browser whose key (or session hash) is `key`. A key already admitted
    /// keeps what it was admitted as; a name another key holds is refused.
    pub fn add(&self, key: &str, peer: Peer) -> Result<Peer, String> {
        locked(&self.paths, || {
            let mut all = self.list();
            if let Some(known) = all.get(key) {
                return Ok(known.clone());
            }
            if all.values().any(|other| other.name == peer.name) {
                return Err(format!(
                    "another machine is admitted as {} (nypi devices remove {} first, or join under another name)",
                    peer.name, peer.name
                ));
            }
            let peer = Peer { paired_at: chrono::Utc::now().to_rfc3339(), ..peer };
            all.insert(key.to_string(), peer.clone());
            write_json(&self.path, &all).map_err(|error| format!("the register was not written: {error}"))?;
            Ok(peer)
        })
    }

    /// Forget the machine or browser named `name`. Returns how many entries went.
    pub fn remove(&self, name: &str) -> std::io::Result<usize> {
        locked(&self.paths, || {
            let mut all = self.list();
            let before = all.len();
            all.retain(|_, peer| peer.name != name);
            if all.len() != before {
                write_json(&self.path, &all)?;
            }
            Ok(before - all.len())
        })
    }

    /// Forget the key `key` (a machine leaving by itself).
    pub fn remove_key(&self, key: &str) -> std::io::Result<bool> {
        locked(&self.paths, || {
            let mut all = self.list();
            let gone = all.remove(key).is_some();
            if gone {
                write_json(&self.path, &all)?;
            }
            Ok(gone)
        })
    }
}

/// A D80 register (`role: node | interface`, two entries for a device that was both) becomes
/// D93's, once: a node key stays, `personal` when the same name was also an interface, else
/// `exec-only`; an interface's key is forgotten where the device also had a node key, and kept as
/// `personal` where it had none.
pub fn migrate_peers(paths: &Paths) {
    let Ok(text) = std::fs::read_to_string(paths.peers()) else { return };
    if !text.contains("\"role\"") {
        return;
    }
    let Ok(old) = serde_json::from_str::<BTreeMap<String, serde_json::Value>>(&text) else { return };
    if !old.values().any(|peer| peer.get("role").is_some()) {
        return;
    }
    let names_with = |role: &str| -> Vec<String> {
        old.values().filter(|p| p["role"] == role).filter_map(|p| p["name"].as_str().map(str::to_string)).collect()
    };
    let (nodes, interfaces) = (names_with("node"), names_with("interface"));
    let mut new = BTreeMap::new();
    for (key, peer) in &old {
        let name = peer["name"].as_str().unwrap_or_default().to_string();
        let trust = match peer["role"].as_str() {
            Some("node") if interfaces.contains(&name) => Trust::Personal,
            Some("node") => Trust::ExecOnly,
            Some("interface") if nodes.contains(&name) => continue,
            _ => Trust::Personal,
        };
        let paired_at = peer["pairedAt"].as_str().unwrap_or_default().to_string();
        new.insert(key.clone(), Peer { name, trust, paired_at, ..Default::default() });
    }
    let _ = locked(paths, || write_json(&paths.peers(), &new));
}

/// An invite the hub gave out and nobody used yet. The code itself is not kept, only its hash.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Invite {
    token_sha256: String,
    name: String,
    trust: Trust,
    expires: String,
}

fn sha256_hex(text: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, text.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(bytes: usize) -> anyhow::Result<String> {
    let mut random = vec![0u8; bytes];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random)
        .map_err(|_| anyhow::anyhow!("no randomness"))?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

fn live_invites(invites: Vec<Invite>) -> Vec<Invite> {
    let now = chrono::Utc::now();
    invites
        .into_iter()
        .filter(|invite| chrono::DateTime::parse_from_rfc3339(&invite.expires).is_ok_and(|expires| expires > now))
        .collect()
}

/// Give out an invite: `nypi node up --code <code>` admits a machine under `name`, once, before
/// `expires` passes, with no question.
pub fn invite(paths: &Paths, name: &str, trust: Trust, expires: chrono::Duration) -> anyhow::Result<String> {
    let name = crate::instance::plain_name(name);
    anyhow::ensure!(!name.is_empty(), "a machine's name is letters, digits, `-`, `_`, and `.`");
    let code = format!("nypi-{}", random_hex(12)?);
    locked(paths, || -> anyhow::Result<()> {
        let mut invites = live_invites(read_json(&paths.pairing()));
        invites.push(Invite {
            token_sha256: sha256_hex(&code),
            name,
            trust,
            expires: (chrono::Utc::now() + expires).to_rfc3339(),
        });
        write_json(&paths.pairing(), &invites)?;
        Ok(())
    })?;
    Ok(code)
}

/// Use up the invite `code`: the name and trust it was given out for, once, while it is live.
pub fn redeem(paths: &Paths, code: &str) -> Option<(String, Trust)> {
    locked(paths, || {
        let invites = live_invites(read_json(&paths.pairing()));
        let hash = sha256_hex(code.trim());
        let at = invites.iter().position(|invite| invite.token_sha256 == hash)?;
        let mut rest = invites;
        let used = rest.remove(at);
        // Struck off before it counts: an invite that could not be struck off is not honoured.
        write_json(&paths.pairing(), &rest).ok()?;
        Some((used.name, used.trust))
    })
}

/// A join request (decision D93), as the places that answer it show it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JoinRequest {
    pub id: String,
    /// `machine` or `browser`.
    pub kind: PeerKind,
    pub name: String,
    pub system: String,
    /// The address it came from.
    pub from: String,
    /// The six-digit confirmation code.
    pub code: String,
    pub expires: String,
    /// The key (or session hash) asking, and its instance id.
    pub key: String,
    pub instance_id: String,
    /// The user's answer: admitted (with the trust), or refused.
    pub answer: Option<JoinAnswer>,
    /// The hub told the user of it. A machine's request is told at once by its link; a
    /// browser's, which the board records, by the daemon when it sees it (decision D94).
    #[serde(default)]
    pub announced: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "answer")]
pub enum JoinAnswer {
    Admit { trust: Trust },
    Refuse,
}

impl JoinRequest {
    fn live(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        chrono::DateTime::parse_from_rfc3339(&self.expires).is_ok_and(|expires| expires > now)
    }

    /// What the places that answer show: who asks, from where, and the code.
    pub fn question(&self) -> String {
        let what = match self.kind {
            PeerKind::Machine => self.name.clone(),
            PeerKind::Browser => format!("A browser ({})", self.system),
        };
        let system = if self.kind == PeerKind::Machine && !self.system.is_empty() {
            format!(" ({})", self.system)
        } else {
            String::new()
        };
        format!(
            "{what}{system} asks to join from {}. Code {}. Answer /admit {} (or /admit {} exec-only), or /refuse {}.",
            self.from,
            nypi_node::identity::spaced(&self.code),
            self.code,
            self.code,
            self.code
        )
    }

    /// The record interfaces show (`join_request`).
    pub fn record(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "join_request", "id": self.id, "kind": self.kind, "name": self.name, "system": self.system,
            "from": self.from, "code": self.code, "expires": self.expires, "text": self.question(),
        })
    }
}

/// `devices/requests.json`: the requests, and the day's counts that limit them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct RequestsFile {
    requests: Vec<JoinRequest>,
    /// The day the counts are for (UTC).
    day: String,
    per_address: BTreeMap<String, u32>,
    total: u32,
    /// The user was told today that requests are being refused.
    told: bool,
}

/// Why a request was not recorded, and whether the user should now hear that the limit is
/// reached (once a day).
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    pub reason: String,
    pub tell_user: bool,
}

/// What a new request says about itself.
pub struct NewRequest {
    pub kind: PeerKind,
    pub name: String,
    pub system: String,
    pub from: String,
    pub code: String,
    pub key: String,
    pub instance_id: String,
}

/// The address a request came from, without its port: what the per-address limit counts.
fn address_of(from: &str) -> String {
    match from.parse::<std::net::SocketAddr>() {
        Ok(socket) => socket.ip().to_string(),
        Err(_) => from.trim_matches(['[', ']']).to_string(),
    }
}

/// Record a join request, within the limits.
pub fn open_request(paths: &Paths, new: NewRequest) -> Result<JoinRequest, Refusal> {
    let now = chrono::Utc::now();
    locked(paths, || {
        let mut file: RequestsFile = read_json(&paths.join_requests());
        let today = now.format("%Y-%m-%d").to_string();
        if file.day != today {
            file = RequestsFile { requests: file.requests, day: today, ..Default::default() };
        }
        file.requests.retain(|request| request.live(now) && request.answer.is_none());
        let address = address_of(&new.from);
        let from_here = file.per_address.get(&address).copied().unwrap_or(0);
        let full = file.requests.len() >= MAX_WAITING;
        let over = from_here >= MAX_PER_ADDRESS || file.total >= MAX_PER_DAY;
        if full || over {
            let tell_user = over && !file.told;
            file.told |= over;
            let _ = write_json(&paths.join_requests(), &file);
            let reason = if over {
                "too many join requests today; try again tomorrow".to_string()
            } else {
                "too many join requests are waiting; try again in a few minutes".to_string()
            };
            return Err(Refusal { reason, tell_user });
        }
        *file.per_address.entry(address).or_default() += 1;
        file.total += 1;
        let request = JoinRequest {
            id: format!("j-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            kind: new.kind,
            name: new.name,
            system: new.system,
            from: new.from,
            code: new.code,
            expires: (now + chrono::Duration::minutes(REQUEST_MINUTES)).to_rfc3339(),
            key: new.key,
            instance_id: new.instance_id,
            answer: None,
            announced: new.kind == PeerKind::Machine,
        };
        file.requests.push(request.clone());
        write_json(&paths.join_requests(), &file)
            .map_err(|error| Refusal { reason: format!("the request was not recorded: {error}"), tell_user: false })?;
        Ok(request)
    })
}

/// Requests waiting for an answer.
pub fn pending(paths: &Paths) -> Vec<JoinRequest> {
    let now = chrono::Utc::now();
    let file: RequestsFile = read_json(&paths.join_requests());
    file.requests.into_iter().filter(|request| request.live(now) && request.answer.is_none()).collect()
}

/// A request as it stands now (its answer, once given). None once it is gone.
pub fn request(paths: &Paths, id: &str) -> Option<JoinRequest> {
    let file: RequestsFile = read_json(&paths.join_requests());
    file.requests.into_iter().find(|request| request.id == id)
}

/// The request of the key (or browser session) `key`, as it stands now.
pub fn request_of(paths: &Paths, key: &str) -> Option<JoinRequest> {
    let file: RequestsFile = read_json(&paths.join_requests());
    file.requests.into_iter().find(|request| request.key == key)
}

/// Answer the waiting request whose code (or id) is `code`: the first answer counts.
pub fn answer(paths: &Paths, code: &str, answer: JoinAnswer) -> Result<JoinRequest, String> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let now = chrono::Utc::now();
    locked(paths, || {
        let mut file: RequestsFile = read_json(&paths.join_requests());
        let Some(request) =
            file.requests.iter_mut().find(|request| (request.code == code || request.id == code) && request.live(now))
        else {
            return Err(format!("no join request with code {code} is waiting"));
        };
        if request.answer.is_some() {
            return Err(format!("the request with code {code} was answered already"));
        }
        if request.kind == PeerKind::Browser && answer == (JoinAnswer::Admit { trust: Trust::ExecOnly }) {
            return Err("a browser is admitted as the user's own or refused; it carries out no calls".into());
        }
        request.answer = Some(answer);
        let answered = request.clone();
        write_json(&paths.join_requests(), &file).map_err(|error| format!("the answer was not recorded: {error}"))?;
        Ok(answered)
    })
}

/// Browsers' requests nobody was told of yet, now marked as told.
pub fn untold(paths: &Paths) -> Vec<JoinRequest> {
    let now = chrono::Utc::now();
    locked(paths, || {
        let mut file: RequestsFile = read_json(&paths.join_requests());
        let mut told = Vec::new();
        for request in file.requests.iter_mut().filter(|r| !r.announced && r.answer.is_none() && r.live(now)) {
            request.announced = true;
            told.push(request.clone());
        }
        if !told.is_empty() {
            let _ = write_json(&paths.join_requests(), &file);
        }
        told
    })
}

/// The register's key for a browser session: the SHA-256 of its cookie, so the register holds
/// nothing a browser could be made from.
pub fn session_key(token: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, token.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The six digits a browser waiting to be admitted shows, tied to its pending session.
pub fn browser_code(token: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, format!("nypi browser\n{token}").as_bytes());
    let bytes = digest.as_ref();
    let number = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % 1_000_000;
    format!("{number:06}")
}

/// A plain name for a browser, from its user agent, that no one in the register holds yet.
pub fn browser_name(peers: &Peers, user_agent: &str) -> String {
    let agent = user_agent.to_ascii_lowercase();
    let base =
        [("iphone", "iphone"), ("ipad", "ipad"), ("android", "android"), ("mac os", "mac"), ("windows", "windows")]
            .iter()
            .chain([("linux", "linux")].iter())
            .find(|(needle, _)| agent.contains(needle))
            .map_or("browser", |(_, name)| name);
    let base = format!("{base}-browser");
    let taken: Vec<String> = peers.list().into_values().map(|peer| peer.name).collect();
    (1..)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
        .find(|name| !taken.contains(name))
        .unwrap_or(base)
}

/// Take a request off the file once its link has its answer.
pub fn forget_request(paths: &Paths, id: &str) {
    locked(paths, || {
        let mut file: RequestsFile = read_json(&paths.join_requests());
        file.requests.retain(|request| request.id != id);
        let _ = write_json(&paths.join_requests(), &file);
    })
}

/// What a link is for, in its first line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Purpose {
    /// The node protocol: calls carried out on this machine.
    #[default]
    Node,
    /// The conversation, as the user (a personal machine only).
    Talk,
    /// A key the hub does not know asks to be admitted.
    Join,
    /// This machine leaves: the hub forgets its key.
    Leave,
    /// Which epoch is the hub at (decision D94)? Answered with `hub`, and the link is closed:
    /// how a node picks among the ways to its hub without being registered on each.
    Probe,
    /// A personal machine's standby copy of the hub's state, kept current by the hub (D95).
    Standby,
    /// A personal machine takes the hub's place (`nypi hub here`, decision D95).
    Move,
    /// A personal machine has one line to tell the user through the hub's outbox (a hub that
    /// stepped down says where its state went).
    Notice,
}

/// What a machine says first on a link, and what the hub answers. One line each.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Hello {
    pub purpose: Purpose,
    /// An invite code, for a join with no question.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// For a join: what the machine calls itself, its system, its NyPi version, its instance id.
    pub name: String,
    pub system: String,
    pub version: String,
    pub instance_id: String,
    /// The hub's epoch this machine knows of (decision D95): a hub below it learns that another
    /// took its place.
    #[serde(skip_serializing_if = "is_zero")]
    pub epoch: u64,
}

fn is_zero(epoch: &u64) -> bool {
    *epoch == 0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Answer {
    Welcome {
        name: String,
        trust: Trust,
        epoch: u64,
        direct: Vec<String>,
    },
    /// A join waits for the user's answer until `expires`.
    Waiting {
        expires: String,
    },
    Refused {
        reason: String,
    },
    /// The answer to a probe.
    Hub {
        epoch: u64,
    },
}

/// What this machine's system is, as a join request says it.
pub fn system() -> String {
    format!("{} {}, NyPi {}", std::env::consts::OS, std::env::consts::ARCH, env!("CARGO_PKG_VERSION"))
}

/// Any byte stream a link runs over: a TCP connection, a relayed one, a test's pipe.
pub trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

pub type LinkStream = tokio_rustls::client::TlsStream<Box<dyn Io>>;

/// A way to the hub (decision D94): one of its direct addresses, with TLS straight over TCP, or
/// the entry, where the same TLS runs inside a WebSocket the relay passes on unread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Way {
    Direct(String),
    Relayed(String),
}

impl Way {
    pub fn is_direct(&self) -> bool {
        matches!(self, Way::Direct(_))
    }

    /// The address, or the entry, as the user reads it.
    pub fn label(&self) -> String {
        match self {
            Way::Direct(address) => address.clone(),
            Way::Relayed(entry) => {
                crate::ws::Entry::parse(entry).map(|entry| entry.url()).unwrap_or_else(|| entry.clone())
            }
        }
    }
}

/// The path a relayed link takes at the entry.
pub const LINK_PATH: &str = "/_nypi/link";

/// A link to the hub, after the hub said who this machine is to it.
pub struct Linked {
    pub read: tokio::io::BufReader<tokio::io::ReadHalf<LinkStream>>,
    pub write: tokio::io::WriteHalf<LinkStream>,
    pub name: String,
    pub trust: Trust,
    pub epoch: u64,
    pub direct: Vec<String>,
    /// The way it went.
    pub way: Way,
}

impl std::fmt::Debug for Linked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a link to the hub as the {} machine {} at {}", self.trust.label(), self.name, self.way.label())
    }
}

const WAIT: std::time::Duration = std::time::Duration::from_secs(15);

async fn open(identity: &Identity, way: &Way, pin: Option<&str>) -> anyhow::Result<LinkStream> {
    let label = way.label();
    let boxed: Box<dyn Io> = match way {
        Way::Direct(address) => {
            let tcp = tokio::time::timeout(WAIT, tokio::net::TcpStream::connect(address))
                .await
                .map_err(|_| anyhow::anyhow!("{label} did not answer"))??;
            tcp.set_nodelay(true).ok();
            Box::new(tcp)
        }
        Way::Relayed(entry) => {
            let entry = crate::ws::Entry::parse(entry).ok_or_else(|| anyhow::anyhow!("{entry} is not an entry"))?;
            let (read, write) = crate::ws::dial(&entry, LINK_PATH, &[]).await?;
            Box::new(crate::ws::into_stream(read, write))
        }
    };
    Ok(tokio::time::timeout(WAIT, identity.dial_over(boxed, pin))
        .await
        .map_err(|_| anyhow::anyhow!("{label} did not finish the handshake"))??)
}

async fn say_hello(
    stream: LinkStream,
    hello: &Hello,
) -> anyhow::Result<(tokio::io::BufReader<tokio::io::ReadHalf<LinkStream>>, tokio::io::WriteHalf<LinkStream>)> {
    use tokio::io::AsyncWriteExt;
    let (read, mut write) = tokio::io::split(stream);
    write.write_all(format!("{}\n", serde_json::to_string(hello)?).as_bytes()).await?;
    Ok((tokio::io::BufReader::new(read), write))
}

async fn answer_line(
    read: &mut tokio::io::BufReader<tokio::io::ReadHalf<LinkStream>>,
    wait: std::time::Duration,
) -> anyhow::Result<Answer> {
    let line = tokio::time::timeout(wait, nypi_node::link::read_line(read, 1 << 16)).await;
    let Ok(Ok(Some(line))) = line else { anyhow::bail!("the hub closed the link without a word") };
    Ok(serde_json::from_str(&line)?)
}

/// What joining gave this machine.
#[derive(Clone, Debug)]
pub struct Joined {
    pub hub_fingerprint: String,
    pub name: String,
    pub trust: Trust,
    pub epoch: u64,
    pub direct: Vec<String>,
    pub way: Way,
}

/// On a machine that knows no hub: ask the hub at the first of `ways` that answers to admit it
/// under `name`. Without an invite, `shown` is told the confirmation code to show the user, and
/// this waits for their answer (ten minutes at most).
pub async fn join(
    paths: &Paths,
    ways: &[Way],
    name: &str,
    invite: Option<&str>,
    instance_id: &str,
    shown: impl FnOnce(&str),
) -> anyhow::Result<Joined> {
    let identity = Identity::load_or_create(&paths.device_key())?;
    let mut opened = None;
    let mut last = anyhow::anyhow!("no way to reach a hub");
    for way in ways {
        match open(&identity, way, None).await {
            Ok(stream) => {
                opened = Some((stream, way.clone()));
                break;
            }
            Err(error) => last = error,
        }
    }
    let Some((stream, way)) = opened else { return Err(last) };
    let hub =
        nypi_node::identity::server_fingerprint(&stream).ok_or_else(|| anyhow::anyhow!("the hub showed no key"))?;
    let value = nypi_node::identity::join_value_client(&stream).ok_or_else(|| anyhow::anyhow!("no session value"))?;
    let code = nypi_node::identity::confirmation_code(&hub, &identity.fingerprint(), &value);
    let hello = Hello {
        purpose: Purpose::Join,
        token: invite.map(str::to_string),
        name: name.to_string(),
        system: system(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        instance_id: instance_id.to_string(),
        epoch: 0,
    };
    let (mut read, _write) = say_hello(stream, &hello).await?;
    let mut shown = Some(shown);
    let mut wait = WAIT;
    loop {
        match answer_line(&mut read, wait).await? {
            Answer::Welcome { name, trust, epoch, direct } => {
                return Ok(Joined { hub_fingerprint: hub, name, trust, epoch, direct, way });
            }
            Answer::Waiting { .. } => {
                if let Some(shown) = shown.take() {
                    shown(&code);
                }
                wait = std::time::Duration::from_secs(60 * REQUEST_MINUTES as u64 + 30);
            }
            Answer::Refused { reason } => anyhow::bail!("the hub did not admit this machine: {reason}"),
            Answer::Hub { .. } => anyhow::bail!("the hub answered a question this machine did not ask"),
        }
    }
}

/// On an admitted machine: link to its hub for `purpose`, trying `ways` in order. Only the hub
/// whose key is `fingerprint` is taken, and not one at an epoch below `known`.
pub async fn link(
    paths: &Paths,
    fingerprint: &str,
    ways: &[Way],
    purpose: Purpose,
    known: u64,
) -> anyhow::Result<Linked> {
    let identity = Identity::load_or_create(&paths.device_key())?;
    let mut last = anyhow::anyhow!("no address to reach the hub at");
    for way in ways {
        let stream = match open(&identity, way, Some(fingerprint)).await {
            Ok(stream) => stream,
            Err(error) => {
                last = error;
                continue;
            }
        };
        let (mut read, write) = say_hello(stream, &Hello { purpose, epoch: known, ..Default::default() }).await?;
        match answer_line(&mut read, WAIT).await? {
            Answer::Welcome { epoch, .. } if epoch < known => {
                last = anyhow::anyhow!("the hub at {} is at epoch {epoch}, older than {known}", way.label());
            }
            Answer::Welcome { name, trust, epoch, direct } => {
                return Ok(Linked { read, write, name, trust, epoch, direct, way: way.clone() });
            }
            Answer::Refused { reason } => return Err(anyhow::anyhow!("the hub refused this link: {reason}")),
            Answer::Waiting { .. } => return Err(anyhow::anyhow!("the hub does not know this machine")),
            Answer::Hub { .. } => last = anyhow::anyhow!("the hub at {} answered as to a probe", way.label()),
        }
    }
    Err(last)
}

/// Ask the hub at `way` for its epoch, with its key pinned to `fingerprint` (or any key, when
/// None), saying the epoch this machine knows: the key it showed and the epoch it said. Nothing
/// is registered at the hub.
pub async fn probe(paths: &Paths, way: &Way, fingerprint: Option<&str>, known: u64) -> anyhow::Result<(String, u64)> {
    let identity = Identity::load_or_create(&paths.device_key())?;
    let stream = open(&identity, way, fingerprint).await?;
    let shown = nypi_node::identity::server_fingerprint(&stream).unwrap_or_default();
    let hello = Hello { purpose: Purpose::Probe, epoch: known, ..Default::default() };
    let (mut read, _write) = say_hello(stream, &hello).await?;
    match answer_line(&mut read, WAIT).await? {
        Answer::Hub { epoch } => Ok((shown, epoch)),
        Answer::Refused { reason } => Err(anyhow::anyhow!("the hub refused: {reason}")),
        _ => Err(anyhow::anyhow!("the hub at {} did not answer the probe", way.label())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(from: &str, code: &str) -> NewRequest {
        NewRequest {
            kind: PeerKind::Machine,
            name: "laptop".into(),
            system: "macos aarch64".into(),
            from: from.into(),
            code: code.into(),
            key: "aa".into(),
            instance_id: "i".into(),
        }
    }

    #[test]
    fn an_invite_works_once_and_is_kept_as_a_hash() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let code = invite(&paths, "vps", Trust::ExecOnly, chrono::Duration::hours(1)).unwrap();
        let kept = std::fs::read_to_string(paths.pairing()).unwrap();
        assert!(!kept.contains(&code) && kept.contains("tokenSha256"), "{kept}");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(paths.pairing()).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(redeem(&paths, "nypi-000000000000000000000000"), None);
        assert_eq!(redeem(&paths, &code), Some(("vps".to_string(), Trust::ExecOnly)));
        assert_eq!(redeem(&paths, &code), None, "once");
        let old = invite(&paths, "old", Trust::Personal, chrono::Duration::seconds(-1)).unwrap();
        assert_eq!(redeem(&paths, &old), None, "expired");
    }

    /// The limits of decision D93: three waiting at once, ten a day from one address, thirty in
    /// all, and the user told once when requests start being refused.
    #[test]
    fn join_requests_are_limited_and_answered_once() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let first = open_request(&paths, new("203.0.113.8:5000", "111111")).unwrap();
        open_request(&paths, new("203.0.113.8:5001", "222222")).unwrap();
        open_request(&paths, new("203.0.113.9:5000", "333333")).unwrap();
        let full = open_request(&paths, new("203.0.113.10:1", "444444")).unwrap_err();
        assert!(full.reason.contains("waiting") && !full.tell_user, "{full:?}");
        assert_eq!(pending(&paths).len(), 3);
        let admitted = answer(&paths, "111 111", JoinAnswer::Admit { trust: Trust::Personal }).unwrap();
        assert_eq!(admitted.id, first.id);
        assert!(answer(&paths, "111111", JoinAnswer::Refuse).unwrap_err().contains("answered already"));
        assert_eq!(request(&paths, &first.id).unwrap().answer, Some(JoinAnswer::Admit { trust: Trust::Personal }));
        assert_eq!(pending(&paths).len(), 2);
        assert!(answer(&paths, "999999", JoinAnswer::Refuse).is_err());

        // Ten a day from one address, then refused, and the user is told once.
        for request in pending(&paths) {
            forget_request(&paths, &request.id);
        }
        forget_request(&paths, &first.id);
        let mut refused = None;
        for n in 0..12 {
            match open_request(&paths, new("198.51.100.1:9", &format!("5{n:05}"))) {
                Ok(request) => forget_request(&paths, &request.id),
                Err(refusal) => {
                    refused = Some(refusal);
                    break;
                }
            }
        }
        let refusal = refused.expect("refused after ten");
        assert!(refusal.reason.contains("too many join requests today") && refusal.tell_user);
        let again = open_request(&paths, new("198.51.100.1:9", "600000")).unwrap_err();
        assert!(!again.tell_user, "told once");
        // Thirty in all, from any address.
        let mut n = 0;
        let last = loop {
            n += 1;
            match open_request(&paths, new(&format!("192.0.2.{n}:1"), &format!("7{n:05}"))) {
                Ok(request) => forget_request(&paths, &request.id),
                Err(refusal) => break refusal,
            }
        };
        assert!(last.reason.contains("today"), "{last:?}");
        assert_eq!(n, 30 - 10 - 3 + 1, "thirty requests in all today");
    }

    #[test]
    fn a_d80_register_merges_into_one_peer_per_machine() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        std::fs::create_dir_all(paths.peers().parent().unwrap()).unwrap();
        let old = serde_json::json!({
            "aa": { "name": "laptop", "role": "node", "pairedAt": "2026-11-01T00:00:00Z" },
            "bb": { "name": "laptop", "role": "interface", "pairedAt": "2026-11-01T00:00:00Z" },
            "cc": { "name": "vps", "role": "node", "pairedAt": "2026-11-02T00:00:00Z" },
            "dd": { "name": "ipad", "role": "interface", "pairedAt": "2026-11-03T00:00:00Z" },
        });
        std::fs::write(paths.peers(), old.to_string()).unwrap();
        migrate_peers(&paths);
        let peers = Peers::open(&paths).list();
        let summary: Vec<(&str, &str, Trust)> =
            peers.iter().map(|(key, peer)| (key.as_str(), peer.name.as_str(), peer.trust)).collect();
        assert_eq!(
            summary,
            [("aa", "laptop", Trust::Personal), ("cc", "vps", Trust::ExecOnly), ("dd", "ipad", Trust::Personal)]
        );
    }
}
