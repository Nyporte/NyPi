//! Filesystem layout and settings. Global state lives in `~/.nypi` (override with `NYPI_HOME`);
//! a project may add `.nypi/settings.json`, which overrides global keys.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Locations of NyPi's files.
#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn discover() -> Self {
        let home = std::env::var_os("NYPI_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".nypi"));
        Paths { home }
    }

    pub fn settings(&self) -> PathBuf {
        self.home.join("settings.json")
    }
    pub fn models(&self) -> PathBuf {
        self.home.join("models.json")
    }
    pub fn auth(&self) -> PathBuf {
        self.home.join("auth.json")
    }
    pub fn env_file(&self) -> PathBuf {
        self.home.join(".env")
    }
    /// Telegram chats allowed to talk to the agent (guarded by the Inspector).
    pub fn telegram_access(&self) -> PathBuf {
        self.home.join("telegram.json")
    }
    /// Control socket of a running Telegram bridge (guarded by the Inspector).
    pub fn bridge_socket(&self) -> PathBuf {
        self.home.join("run").join("telegram.sock")
    }
    /// Where runtimes find the board service (`nypi board`).
    pub fn board_socket(&self) -> PathBuf {
        self.home.join("run").join("board.sock")
    }
    pub fn sessions(&self) -> PathBuf {
        self.home.join("sessions")
    }
    /// Long-term memory; the agent changes it only through the `memory` tool.
    pub fn memory(&self) -> PathBuf {
        self.home.join("memory").join("memories.jsonl")
    }
    /// Notices for the user, kept until one reaches them (decision D57); only the harness writes it.
    /// Where the hub takes messages (`run/hub.sock`, decision D100); whoever can connect speaks
    /// as the user.
    pub fn hub_socket(&self) -> PathBuf {
        self.home.join("run").join("hub.sock")
    }
    /// Where nodes that are processes of their own reach the hub (`nypi node`, decision D66).
    /// It carries the node's protocol only: nothing that connects there can give Lucia a task.
    pub fn nodes_socket(&self) -> PathBuf {
        self.home.join("run").join("nodes.sock")
    }
    /// The hub's register of nodes: what each last reported, and whether it is connected.
    pub fn devices(&self) -> PathBuf {
        self.home.join("devices.json")
    }
    /// What the planes keep (decision D84): the decision journal and the index of activations.
    pub fn stores(&self) -> PathBuf {
        self.home.join("stores")
    }
    pub fn decisions(&self) -> PathBuf {
        self.stores().join("decisions.jsonl")
    }
    pub fn activations_index(&self) -> PathBuf {
        self.stores().join("activations.jsonl")
    }
    /// The step-by-step story of one activation of the Principal.
    pub fn activation_log(&self, id: &str) -> PathBuf {
        let name: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        self.home.join("principal").join("activations").join(format!("{name}.jsonl"))
    }
    /// What the Principal is doing now (`PrincipalStatus` with the process's pid), for the
    /// board, which runs in a process of its own (decision D86).
    pub fn principal_status(&self) -> PathBuf {
        self.home.join("principal").join("status.json")
    }
    /// Every turn of the chat module, step by step.
    pub fn chat_turns(&self) -> PathBuf {
        self.home.join("chat").join("turns.jsonl")
    }
    /// This hub's own key, which a pairing code names and a paired device checks (decision D80).
    pub fn hub_key(&self) -> PathBuf {
        self.home.join("hub-id.key")
    }
    /// This machine's one key (decision D93): what the hub knows it by when it joins.
    pub fn device_key(&self) -> PathBuf {
        self.home.join("device-id.key")
    }
    /// The second key of a D80 device that was also paired as an interface; forgotten when the
    /// machine's files are moved to D93's.
    pub fn interface_key(&self) -> PathBuf {
        self.home.join("interface-id.key")
    }
    /// The machines and browsers admitted to this hub, each with its trust.
    pub fn peers(&self) -> PathBuf {
        self.home.join("devices").join("peers.json")
    }
    /// Invite codes this hub gave out that were not used yet (kept as hashes).
    pub fn pairing(&self) -> PathBuf {
        self.home.join("devices").join("pairing.json")
    }
    /// Join requests waiting for the user's answer, and the day's counts that limit them.
    pub fn join_requests(&self) -> PathBuf {
        self.home.join("devices").join("requests.json")
    }
    /// The D80 file that named the hub a device was paired with; read once, by the migration.
    pub fn paired_hub(&self) -> PathBuf {
        self.home.join("devices").join("hub.json")
    }
    /// What this machine is (decision D93): a hub or a node, of which hub, at which epoch.
    pub fn instance(&self) -> PathBuf {
        self.home.join("instance.json")
    }
    /// Where a node's link to the hub says how it is linked, for `nypi status`.
    pub fn node_status(&self) -> PathBuf {
        self.home.join("run").join("node.json")
    }
    /// The hub's links up now, each direct (with its address) or relayed (decision D94).
    pub fn links(&self) -> PathBuf {
        self.home.join("run").join("links.json")
    }
    /// Where the hub hands the board browsers that came through the entry; the board asks every
    /// request there for a browser session and never takes its local token (decision D94).
    pub fn board_public_socket(&self) -> PathBuf {
        self.home.join("run").join("board-public.sock")
    }
    /// A personal node's standby copy of the hub's state (decision D95): `home/` as the hub has
    /// it, the master key, and `manifest.json`. No tool reads it.
    pub fn standby(&self) -> PathBuf {
        self.home.join("standby")
    }
    /// A hub's state set aside when another hub took its place at a higher epoch (decision D95).
    pub fn diverged(&self) -> PathBuf {
        self.home.join("diverged")
    }
    /// Env files carried from the hub's machine when they were outside its NyPi home (decision D95).
    pub fn env_copies(&self) -> PathBuf {
        self.home.join("env")
    }
    /// What this machine has to tell the user through the hub when it next links (a hub that
    /// stepped down says where its state went).
    pub fn pending_notice(&self) -> PathBuf {
        self.home.join("run").join("pending-notice.txt")
    }
    /// What the relay keeps (decision D94): the hub key it takes, the epoch, and the instance
    /// registered at that epoch. Nothing in it is secret.
    pub fn relay_state(&self) -> PathBuf {
        self.home.join("relay").join("state.json")
    }
    /// Lucia's own vault of notes (decision D77); she changes it only through the `vault_*` tools.
    pub fn vault(&self) -> PathBuf {
        self.home.join("vault")
    }
    /// The registry of what she looks after; changed only through `asset_manage`.
    pub fn assets(&self) -> PathBuf {
        self.home.join("assets.jsonl")
    }
    /// The sealed secrets, and the master key where it is kept in a file. No tool reads either.
    pub fn secrets(&self) -> PathBuf {
        self.home.join("secrets.enc")
    }
    pub fn secrets_key(&self) -> PathBuf {
        self.home.join("secrets.key")
    }
    pub fn outbox(&self) -> PathBuf {
        self.home.join("outbox").join("notices.jsonl")
    }
    /// The DMN's passes and their cost; only the harness writes it.
    pub fn dmn_passes(&self) -> PathBuf {
        self.home.join("dmn").join("passes.jsonl")
    }
    /// Timers and condition triggers; only the harness writes it.
    pub fn schedules(&self) -> PathBuf {
        self.home.join("schedules").join("schedules.jsonl")
    }
    /// The user portrait; only the harness writes it.
    pub fn portrait(&self) -> PathBuf {
        self.home.join("portrait").join("portrait.jsonl")
    }
    /// Local embedding models, one directory each.
    pub fn embedding_models(&self) -> PathBuf {
        self.home.join("models").join("embeddings")
    }
    /// The user's goals; the agent changes it only through the `goals` tool.
    pub fn goals(&self) -> PathBuf {
        crate::goals::goals_path(&self.home)
    }
    /// The task ledger (harness-owned; protected from the agent's tools).
    pub fn ledger(&self) -> PathBuf {
        self.home.join("tasks").join("ledger.jsonl")
    }
    pub fn audit(&self) -> PathBuf {
        self.home.join("audit")
    }
    pub fn skills(&self) -> PathBuf {
        self.home.join("skills")
    }
    /// Read-only prompt layer files (guarded by the Inspector).
    pub fn core_prompt_dir(&self) -> PathBuf {
        self.home.join("prompt")
    }
    /// The agent's own read-write prompt layer.
    pub fn self_prompt(&self, agent_name: &str) -> PathBuf {
        self.home.join("agents").join(agent_name.to_lowercase()).join("self.md")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactionSettings {
    pub enabled: bool,
    /// Compact when context tokens exceed `contextWindow - reserveTokens`.
    pub reserve_tokens: u64,
    /// Approximate recent tokens kept verbatim after compaction.
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        CompactionSettings { enabled: true, reserve_tokens: 16_384, keep_recent_tokens: 20_000 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// The agent's name; differs per user.
    pub agent_name: String,
    /// The user's name or how the agent should address them, if known.
    pub user_name: Option<String>,
    /// Default model as `provider/id`.
    pub default_model: Option<String>,
    pub default_thinking: String,
    pub tools: Vec<String>,
    pub compaction: CompactionSettings,
    /// Retries of replies that failed with a transient provider error.
    pub retry: nypi_ai::retry::RetryPolicy,
    /// Models to cycle through, as Pi's `enabledModels`: `provider/id`, a bare id, or a glob,
    /// each with an optional `:level` (for example `gateway/gpt-*:high`). Empty: every model with a key.
    pub enabled_models: Vec<String>,
    /// How many steering messages one drain point takes (Pi's setting).
    pub steering_mode: nypi_agent::QueueMode,
    /// How many follow-up messages one drain point takes (Pi's setting).
    pub follow_up_mode: nypi_agent::QueueMode,
    /// Extra `KEY=value` files loaded into the environment at startup (never overriding).
    pub env_files: Vec<String>,
    /// Extra skill directories.
    pub skill_paths: Vec<String>,
    /// Instructions appended to the situational prompt layer.
    pub append_system_prompt: Option<String>,
    /// MCP servers to start, keyed by name (Claude Desktop / Cursor format).
    pub mcp_servers: std::collections::BTreeMap<String, crate::mcp::McpServerConfig>,
    /// Ask once for the status line when a reply ends without one (principle B6).
    pub enforce_status_line: bool,
    /// How often a run that ends `Continuing` without a schedule to bring it back is told to go
    /// on, before it is reported as Blocked (decision D59). 0 accepts the word as given.
    pub max_continuations: u32,
    /// A run with no progress at all (no reply text, no tool call starting, ending, or writing
    /// output) for this many minutes is stopped and ends `Failed`, so a hung model stream, MCP
    /// call, or tool cannot hold its conversation forever (principle B6). 0 never stops one.
    pub stuck_minutes: f64,
    /// Run tool subprocesses in the operating system's sandbox when it has one (decision D62).
    pub sandbox: crate::sandbox::SandboxSettings,
    /// Set by `load`, never read from a file: the configuration found in the working directory
    /// that was left out because the user has not trusted the folder (decision D61).
    #[serde(skip)]
    pub untrusted_project: Vec<String>,
    /// Which tool calls need the user's approval before they run.
    pub approval: crate::inspector::ApprovalSettings,
    /// Reports pushed to the user without being asked.
    pub notify: NotifySettings,
    /// External programs exposed as tools, keyed by tool name.
    pub command_tools: std::collections::BTreeMap<String, crate::tools::command::CommandToolConfig>,
    /// A fast chat module in front of the worker agent.
    pub chat: ChatSettings,
    /// The executive of the conversation (decision D84): its model and how many activations wait.
    pub principal: PrincipalSettings,
    pub tasks: TaskSettings,
    /// Interactive sessions use the full-screen UI.
    pub tui: bool,
    /// The periodic ledger check for stalled and interrupted tasks.
    pub watchdog: crate::watchdog::WatchdogSettings,
    pub goals: GoalSettings,
    /// Post-task reflection: when to look back on a task and what to learn from it.
    pub reflection: crate::reflection::ReflectionSettings,
    pub memory: MemorySettings,
    /// The user portrait: lasting traits learned from the user's words (decision D35).
    pub portrait: crate::portrait::PortraitSettings,
    /// Idle-time reflection over goals, tasks, schedules, and memory (decision D37); off by default.
    pub dmn: crate::dmn::DmnSettings,
    /// The observer of working runs, which steers one that goes wrong (decision D78); off by default.
    pub thinking: crate::thinking::ThinkingSettings,
    /// The bars `nypi scorecard` judges a week against (decision D63).
    pub scorecard: crate::scorecard::ScorecardSettings,
    /// The hub: whether the supervisor keeps it running, where it works, and how machines reach
    /// it directly (decisions D64, D80, D94, D100).
    pub hub: HubSettings,
    /// The relay this machine runs behind the entry, if any (decision D94).
    pub relay: RelaySettings,
    /// Where updates come from, and whether the supervisor checks for them or applies them
    /// (decision D98).
    pub update: UpdateSettings,
    /// The entry (`host:port`, `https://…`, or `http://host:port`; decisions D93, D97), empty for
    /// none; `NYPI_ENTRY` takes precedence, and `--entry` on `hub up` or `node up` saves one here.
    pub entry: String,
    /// The vault's upkeep in idle time (decision D81).
    pub vault: VaultSettings,
    /// Inbound channels besides the interfaces (decision D81). `notify.channels` are the
    /// outbound ones.
    pub channels: ChannelSettings,
    /// The board as a service the supervisor keeps alive (decision D66, step 1).
    pub board: BoardSettings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BoardSettings {
    /// The supervisor starts the board and starts it again when it stops.
    pub enabled: bool,
    pub port: u16,
    /// Serve on this machine only. A service that starts at login should not be on whatever
    /// network the machine joins unless the owner says so.
    pub local: bool,
}

impl Default for BoardSettings {
    fn default() -> Self {
        BoardSettings { enabled: true, port: 7878, local: true }
    }
}

/// The channels things reach Lucia through besides the interfaces (decision D81).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChannelSettings {
    pub webhook: WebhookSettings,
    pub telegram: TelegramChannelSettings,
}

/// The Telegram channel (decisions D91, D100): whether the hub runs it, and where its bot token
/// comes from.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TelegramChannelSettings {
    /// The hub runs Telegram as a channel, when a bot token is set (decision D66, step 1).
    pub enabled: bool,
    /// The handle of the bot token in the secret store (`cred://channels/telegram`, set with
    /// `nypi secrets set channels/telegram`). Without one, `TELEGRAM_BOT_TOKEN` is read.
    pub bot_token_secret: Option<String>,
}

impl Default for TelegramChannelSettings {
    fn default() -> Self {
        TelegramChannelSettings { enabled: true, bot_token_secret: None }
    }
}

/// The inbound webhook: other programs of the owner's (a share sheet shortcut, a mail hook, a
/// chat bridge) hand Lucia something to file. Off unless turned on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebhookSettings {
    pub enabled: bool,
    /// Where it listens: this machine only, or its address in the tailnet.
    pub listen: String,
    /// The handle of the secret a caller must show (`nypi secrets set channels/webhook`).
    pub secret: String,
}

impl Default for WebhookSettings {
    fn default() -> Self {
        // 7681 is the hub's, 7682 the relay's (decision D100).
        WebhookSettings { enabled: false, listen: "127.0.0.1:7683".into(), secret: "cred://channels/webhook".into() }
    }
}

/// What the idle-time consolidator does with the vault (decision D81).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VaultSettings {
    /// A task's workspace is moved to the archive this many days after the task settled.
    pub archive_after_days: u64,
}

impl Default for VaultSettings {
    fn default() -> Self {
        VaultSettings { archive_after_days: 7 }
    }
}

/// An entry built into the program (decisions D93, D97): where a machine first finds its hub,
/// and where a browser reaches the board. Unset in public builds, which work on the local
/// network until the user names an entry; someone building NyPi for their own domain sets
/// `NYPI_DEFAULT_ENTRY=host:port` when building.
pub const DEFAULT_ENTRY: Option<&str> = option_env!("NYPI_DEFAULT_ENTRY");

/// The port a hub takes machines on directly (decision D94).
pub const HUB_PORT: u16 = 7681;
/// Where the relay listens unless told otherwise: behind Cloudflare Tunnel or a reverse proxy
/// on the same machine (decision D94).
pub const RELAY_LISTEN: &str = "127.0.0.1:7682";

/// How machines reach this hub (decisions D80, D93, D94).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HubSettings {
    /// The supervisor starts the hub (`nypi hub run`) and starts it again when it stops or hangs.
    pub enabled: bool,
    /// The folder the hub works in; the user's home directory unless set.
    pub cwd: Option<String>,
    /// The addresses the hub takes machines on directly, with pinned TLS. `[]` opens no port:
    /// machines then come through the entry only. One address as a string is taken too.
    #[serde(deserialize_with = "addresses")]
    pub listen: Vec<String>,
    /// `host:port` of a port forwarded on the router to this hub, offered to nodes as a direct
    /// address.
    pub public_address: Option<String>,
    /// Ask the router for a port by UPnP-IGD or NAT-PMP, and offer it to nodes.
    pub port_mapping: bool,
    /// Announce the hub on the local network by mDNS (`_nypi._tcp`).
    pub lan_discovery: bool,
    /// How often a linked personal node's standby copy is brought up to date, in minutes
    /// (decision D95); a decision of the Principal brings it forward, at most once a minute.
    pub standby_minutes: f64,
    /// How long a planned move waits for the Principal's activation to end before it is cut off
    /// and taken up again by the new hub.
    pub handover_wait_seconds: u64,
    /// How long the hub must have been silent before `nypi hub here --force` takes over.
    pub force_after_seconds: u64,
}

impl Default for HubSettings {
    fn default() -> Self {
        HubSettings {
            enabled: true,
            cwd: None,
            listen: vec![format!("0.0.0.0:{HUB_PORT}"), format!("[::]:{HUB_PORT}")],
            public_address: None,
            port_mapping: false,
            lan_discovery: true,
            standby_minutes: 5.0,
            handover_wait_seconds: 60,
            force_after_seconds: 120,
        }
    }
}

/// The relay this machine runs for the hub behind the entry (decision D94): HTTP and WebSocket,
/// meant to sit behind Cloudflare or a reverse proxy, which serves the entry's certificate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RelaySettings {
    /// The supervisor keeps `nypi relay run` going.
    pub enabled: bool,
    pub listen: String,
    /// The fingerprint of the only hub key the relay takes. None: the first hub to register
    /// claims the relay.
    pub hub_key: Option<String>,
    /// The most one relayed link may carry, in kilobits per second each way; 0 for no limit.
    pub max_kbps_per_link: u64,
    /// Take the client's address from what the proxy in front says: true when the relay is
    /// reached only through Cloudflare or a reverse proxy. It is believed only from a connection
    /// on loopback, where that proxy is (decision D99).
    pub trust_proxy: bool,
    /// The one header the proxy sets the client's address in (`X-Real-IP`, say). None: the first
    /// of `CF-Connecting-IP`, `X-Real-IP`, and the last address of `X-Forwarded-For`.
    pub trusted_header: Option<String>,
    /// New connections one address may open in a minute.
    pub per_ip_per_minute: u32,
    /// Connections waiting for the hub to answer them, at most.
    pub max_waiting: usize,
}

impl Default for RelaySettings {
    fn default() -> Self {
        RelaySettings {
            enabled: false,
            listen: RELAY_LISTEN.to_string(),
            hub_key: None,
            max_kbps_per_link: 2048,
            trust_proxy: true,
            trusted_header: None,
            per_ip_per_minute: 120,
            max_waiting: 64,
        }
    }
}

/// What the supervisor does about new releases (decision D98).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    /// No checks in the background; `nypi update` still works by hand.
    Off,
    /// Check now and then, and tell the user once of each new release.
    #[default]
    Notify,
    /// Check, and apply a new release when this machine is idle (and within the window).
    Auto,
}

/// `update` in `settings.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateSettings {
    /// `<owner>/<repo>` on GitHub whose releases this machine takes.
    pub repo: String,
    pub mode: UpdateMode,
    pub check_interval_hours: f64,
    /// Local hours an automatic update may be applied in, such as `"02:00-05:00"`.
    pub auto_apply_window: Option<String>,
    /// Apply automatically on a machine that runs the relay too; otherwise it is only told.
    pub auto_relay: bool,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        UpdateSettings {
            repo: crate::update::default_repo(),
            mode: UpdateMode::Notify,
            check_interval_hours: 6.0,
            auto_apply_window: None,
            auto_relay: false,
        }
    }
}

impl UpdateSettings {
    /// The mode in effect on a machine that runs the relay when `relay` is true: a relay never
    /// restarts by itself mid-rollout unless `autoRelay` says it may.
    pub fn effective_mode(&self, relay: bool) -> UpdateMode {
        match self.mode {
            UpdateMode::Auto if relay && !self.auto_relay => UpdateMode::Notify,
            mode => mode,
        }
    }
}

/// A settings file written before decision D100 says `"daemon": { "enabled", "cwd", "telegram" }`:
/// those are `hub.enabled`, `hub.cwd`, and `channels.telegram.enabled` now, taken where the same
/// file does not set the new key itself.
fn fold_daemon(file: &mut Value) {
    let Some(daemon) = file.as_object_mut().and_then(|root| root.remove("daemon")) else { return };
    let Some(daemon) = daemon.as_object() else { return };
    let mut put = |path: &[&str], value: &Value| {
        let mut at = &mut *file;
        for part in &path[..path.len() - 1] {
            if !at[*part].is_object() {
                at[*part] = Value::Object(Default::default());
            }
            at = &mut at[*part];
        }
        let last = path[path.len() - 1];
        if at.get(last).is_none() {
            at[last] = value.clone();
        }
    };
    for (old, new) in [
        ("enabled", &["hub", "enabled"][..]),
        ("cwd", &["hub", "cwd"]),
        ("telegram", &["channels", "telegram", "enabled"]),
    ] {
        if let Some(value) = daemon.get(old) {
            put(new, value);
        }
    }
}

fn addresses<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Many(Vec<String>),
        One(String),
        None(()),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::Many(addresses) => addresses,
        OneOrMany::One(address) if address.trim().is_empty() => Vec::new(),
        OneOrMany::One(address) => vec![address],
        OneOrMany::None(()) => Vec::new(),
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MemorySettings {
    /// Embedding sources in order of preference (decision D33): an endpoint first when one is
    /// named, the local model as the fallback. A single object is accepted too; `[]` turns
    /// embeddings off, leaving keyword search.
    #[serde(deserialize_with = "one_or_many")]
    pub embeddings: Vec<EmbeddingSource>,
    /// How recall weighs relevance, importance, emotion, and activation (decision D31).
    pub weights: crate::memory::RetrievalWeights,
    /// When memories are attached to a message unasked (decision D32).
    pub valve: crate::valve::ValveSettings,
}

impl Default for MemorySettings {
    fn default() -> Self {
        MemorySettings {
            embeddings: vec![EmbeddingSource::default_local()],
            weights: Default::default(),
            valve: Default::default(),
        }
    }
}

/// An OpenAI-compatible endpoint (`baseUrl`, `model`, `apiKeyEnv`), or a local model (`local`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EmbeddingSource {
    /// A local model by name, for example `qwen3-embedding-0.6b-q4`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    /// For example `https://api.openai.com/v1`, or Ollama's `http://localhost:11434/v1`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Variable holding the key (read through the private lookup); none for local servers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

const DEFAULT_LOCAL_EMBEDDINGS: &str = "qwen3-embedding-0.6b-q4";

impl EmbeddingSource {
    pub fn default_local() -> Self {
        EmbeddingSource { local: Some(DEFAULT_LOCAL_EMBEDDINGS.into()), ..Default::default() }
    }

    /// The embedder this source names, or why there is none.
    pub fn embedder(&self, paths: &Paths) -> Result<nypi_ai::embeddings::Embedder, String> {
        match &self.local {
            Some(name) => local_embedder(name, paths),
            None if self.base_url.is_empty() || self.model.is_empty() => {
                Err("an embeddings endpoint needs baseUrl and model".into())
            }
            None => Ok(nypi_ai::embeddings::Embedder::Remote(nypi_ai::embeddings::RemoteEmbedder {
                base_url: self.base_url.clone(),
                model: self.model.clone(),
                api_key: self.api_key_env.as_deref().and_then(nypi_ai::auth::lookup_env),
            })),
        }
    }
}

#[cfg(feature = "local-embeddings")]
fn local_embedder(name: &str, paths: &Paths) -> Result<nypi_ai::embeddings::Embedder, String> {
    use nypi_ai::embeddings::local;
    let model = local::find(name).ok_or_else(|| format!("unknown local embedding model {name}"))?;
    let models_dir = paths.embedding_models();
    if !model.is_installed(&models_dir) {
        return Err(format!("{name} is not installed (nypi memory model download)"));
    }
    Ok(nypi_ai::embeddings::Embedder::Local(local::LocalEmbedder { model, models_dir }))
}

#[cfg(not(feature = "local-embeddings"))]
fn local_embedder(name: &str, _paths: &Paths) -> Result<nypi_ai::embeddings::Embedder, String> {
    Err(format!("{name}: this build has no local embeddings (the local-embeddings feature)"))
}

impl MemorySettings {
    /// The usable sources, in order, and a note for each one skipped.
    pub fn embeddings(&self, paths: &Paths) -> (Option<nypi_ai::embeddings::Embeddings>, Vec<String>) {
        let (mut sources, mut skipped) = (Vec::new(), Vec::new());
        for source in &self.embeddings {
            match source.embedder(paths) {
                Ok(embedder) => sources.push(embedder),
                Err(note) => skipped.push(note),
            }
        }
        (nypi_ai::embeddings::Embeddings::new(sources), skipped)
    }
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<EmbeddingSource>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Many(Vec<EmbeddingSource>),
        One(EmbeddingSource),
        None(()),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::Many(sources) => sources,
        OneOrMany::One(source) => vec![source],
        OneOrMany::None(()) => Vec::new(),
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GoalSettings {
    /// The hub runs a goal review this often while idle, as the Principal's idle-time activation
    /// (decision D85); 0 never (the default).
    pub review_every_hours: f64,
}

impl GoalSettings {
    pub fn review_interval(&self) -> Option<std::time::Duration> {
        (self.review_every_hours > 0.0).then(|| std::time::Duration::from_secs_f64(self.review_every_hours * 3600.0))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChatSettings {
    /// Model of the chat module as `provider/id`. Unset: one agent does everything.
    pub model: Option<String>,
    pub thinking: String,
    /// Chat messages kept; older ones are dropped at a user-turn boundary.
    pub history_messages: usize,
    /// Whether the Principal reviews a task's settlement against its brief before the user
    /// hears of it (decisions D78, D84): `auto` (scripted test models only with `always`),
    /// `always`, or `off`.
    pub review: ReviewMode,
}

/// The Principal (decision D84): the singleton executive behind the chat module, on the
/// strongest model, that advances the owner's goals and never speaks to them itself.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrincipalSettings {
    /// `provider/id`; the worker's model without one.
    pub model: Option<String>,
    pub thinking: String,
    /// Activations that may wait; past this, the newest idle-time one is dropped.
    pub max_activations_queued: usize,
    /// Whether an owner's or an event's activation that ends having done nothing (no task
    /// touched, no store changed, nothing told the owner, no `conclude_activation`) is nudged
    /// once to dispose of it (decision D87): `auto` (scripted test models only with
    /// `always`), `always`, or `off`.
    pub enforce_disposition: ReviewMode,
}

impl Default for PrincipalSettings {
    fn default() -> Self {
        PrincipalSettings {
            model: None,
            thinking: "high".into(),
            max_activations_queued: 32,
            enforce_disposition: ReviewMode::Auto,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewMode {
    #[default]
    Auto,
    Always,
    Off,
}

/// How many tasks run at once, and how often a settlement may be sent back (decision D78).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskSettings {
    /// Background tasks of one conversation working at the same time.
    pub max_concurrent: usize,
    /// How many times the principal may send a settled task back before reporting it as it is.
    pub max_review_rounds: u32,
    /// Whether a task with a workspace that ends Done or Blocked without a `report.md` citing
    /// its tool results is nudged once to write one (decision D87): `auto` (scripted test
    /// models only with `always`), `always`, or `off`.
    pub require_report: ReviewMode,
    /// How long a `batch_unless_failed` task that settled Done waits for its siblings before
    /// it is reviewed anyway.
    pub batch_hold_seconds: u64,
}

impl Default for TaskSettings {
    fn default() -> Self {
        TaskSettings {
            max_concurrent: 4,
            max_review_rounds: 2,
            require_report: ReviewMode::Auto,
            batch_hold_seconds: 60,
        }
    }
}

impl ReviewMode {
    /// Whether it applies to a run on `api`: a scripted test model only when asked by name.
    pub fn applies(self, api: nypi_ai::Api) -> bool {
        match self {
            ReviewMode::Off => false,
            ReviewMode::Always => true,
            ReviewMode::Auto => api != nypi_ai::Api::Faux,
        }
    }
}

impl Default for ChatSettings {
    fn default() -> Self {
        ChatSettings { model: None, thinking: "off".into(), history_messages: 80, review: ReviewMode::Auto }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NotifySettings {
    /// Push reports to paired Telegram chats from terminal sessions too.
    pub telegram: bool,
    /// Report a finished run that took at least this long (the user has probably looked away).
    pub after_seconds: u64,
    /// Report a run that has shown no progress for this long.
    pub stall_seconds: u64,
    /// Report an approval request still unanswered after this long.
    pub approval_seconds: u64,
    /// Channels besides Telegram that receive the same reports and alerts. A desktop
    /// notification unless set: being told must not depend on configuration (decision D57).
    /// `[]` turns them off.
    pub channels: Vec<NotifyChannel>,
}

/// An out-of-band channel for reports and alerts (principle A3).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum NotifyChannel {
    /// A desktop notification (macOS Notification Center, or notify-send on Linux).
    Desktop,
    /// POST to a URL; `${NAME}` in the URL or headers resolves through the private key lookup.
    Webhook {
        url: String,
        #[serde(default)]
        format: WebhookFormat,
        #[serde(default)]
        headers: std::collections::BTreeMap<String, String>,
    },
    /// Run a program (no shell) with `{title}` and `{text}` substituted, for example sendmail.
    Command {
        command: Vec<String>,
        #[serde(default)]
        stdin: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookFormat {
    /// `{"text": ...}` (Slack, Mattermost, Google Chat).
    #[default]
    Slack,
    /// `{"content": ...}`.
    Discord,
    /// The text as the body, the title in a `Title` header (ntfy).
    Text,
    /// `{"title", "text", "source"}`.
    Json,
}

impl Default for NotifySettings {
    fn default() -> Self {
        NotifySettings {
            telegram: true,
            after_seconds: 120,
            stall_seconds: 600,
            approval_seconds: 60,
            channels: vec![NotifyChannel::Desktop],
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            agent_name: "Lucia".into(),
            user_name: None,
            default_model: None,
            default_thinking: "medium".into(),
            tools: crate::tools::DEFAULT_TOOLS.iter().map(|s| s.to_string()).collect(),
            compaction: CompactionSettings::default(),
            retry: Default::default(),
            enabled_models: Vec::new(),
            steering_mode: Default::default(),
            follow_up_mode: Default::default(),
            env_files: vec!["~/.env".into()],
            skill_paths: Vec::new(),
            append_system_prompt: None,
            mcp_servers: Default::default(),
            enforce_status_line: true,
            max_continuations: 3,
            stuck_minutes: 30.0,
            sandbox: Default::default(),
            untrusted_project: Vec::new(),
            approval: Default::default(),
            notify: NotifySettings::default(),
            command_tools: Default::default(),
            chat: ChatSettings::default(),
            principal: PrincipalSettings::default(),
            tasks: TaskSettings::default(),
            tui: false,
            watchdog: Default::default(),
            goals: Default::default(),
            reflection: Default::default(),
            memory: Default::default(),
            portrait: Default::default(),
            dmn: Default::default(),
            thinking: Default::default(),
            scorecard: Default::default(),
            hub: Default::default(),
            relay: Default::default(),
            update: Default::default(),
            entry: String::new(),
            vault: Default::default(),
            channels: Default::default(),
            board: Default::default(),
        }
    }
}

impl Settings {
    /// Global settings merged with the project's `.nypi/settings.json` (project keys win).
    ///
    /// The project's file is read only from a folder the user trusts (decision D61): it can
    /// turn approvals off, add programs as tools, and start MCP servers. From any other folder
    /// it is left out, and `untrusted_project` says what was.
    pub fn load(paths: &Paths, cwd: &Path) -> anyhow::Result<Self> {
        let store = crate::trust::TrustStore::new(paths);
        let trust = store.resolve(cwd);
        let mut files = vec![paths.settings()];
        if trust.is_trusted() {
            files.push(cwd.join(".nypi").join("settings.json"));
        }
        let mut merged = serde_json::to_value(Settings::default())?;
        for file in files {
            if let Ok(text) = std::fs::read_to_string(&file) {
                let mut value: Value = serde_json::from_str(&text)
                    .map_err(|e| anyhow::anyhow!("invalid settings file {}: {e}", file.display()))?;
                fold_daemon(&mut value);
                merge(&mut merged, value);
            }
        }
        let mut settings: Settings = serde_json::from_value(merged)?;
        if !trust.is_trusted() {
            settings.untrusted_project = store.found(cwd);
        }
        Ok(settings)
    }

    /// The entry this machine uses (decision D97): `NYPI_ENTRY` when set, else the settings', else
    /// the one built in, if any. None: the local network only. `--entry` on a command saves one.
    pub fn entry(&self) -> Option<String> {
        self.entry_with(std::env::var("NYPI_ENTRY").ok(), DEFAULT_ENTRY)
    }

    /// The entry, given what `NYPI_ENTRY` holds and what was built in.
    pub fn entry_with(&self, variable: Option<String>, built_in: Option<&str>) -> Option<String> {
        let given = |text: &str| Some(text.trim().to_string()).filter(|entry| !entry.is_empty());
        variable.as_deref().and_then(given).or_else(|| given(&self.entry)).or_else(|| built_in.and_then(given))
    }

    /// Set one key, such as `["retry", "enabled"]`, in the global settings file and leave the
    /// rest of it as it is, as Pi's settings manager does for a toggle changed at runtime.
    pub fn save_global(paths: &Paths, key: &[&str], value: Value) -> anyhow::Result<()> {
        let file = paths.settings();
        let mut root = match std::fs::read_to_string(&file) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("invalid settings file {}: {e}", file.display()))?,
            Err(_) => Value::Object(Default::default()),
        };
        if !root.is_object() {
            anyhow::bail!("settings file {} is not a JSON object", file.display());
        }
        let Some((last, parents)) = key.split_last() else { return Ok(()) };
        let mut node = &mut root;
        for part in parents {
            if !node[*part].is_object() {
                node[*part] = Value::Object(Default::default());
            }
            node = &mut node[*part];
        }
        node[*last] = value;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Written whole beside the file, then moved over it, so a crash never leaves half a file.
        let partial = file.with_extension("json.tmp");
        std::fs::write(&partial, format!("{}\n", serde_json::to_string_pretty(&root)?))?;
        std::fs::rename(&partial, &file)?;
        Ok(())
    }
}

fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Expand a leading `~/`.
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    }
}

/// Replace `${NAME}` with the value from the environment or env files. Unset names are errors,
/// so a missing token fails at startup instead of as a confusing 401 later.
pub fn expand_secrets(value: &str) -> anyhow::Result<String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let name = &after[..end];
        out.push_str(&nypi_ai::auth::lookup_env(name).ok_or_else(|| anyhow::anyhow!("${{{name}}} is not set"))?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod embedding_source_tests {
    use super::*;

    #[test]
    fn embedding_sources_accept_one_many_or_none() {
        let parse = |json: &str| serde_json::from_str::<MemorySettings>(json).unwrap().embeddings;
        assert_eq!(parse("{}"), [EmbeddingSource::default_local()], "the local model by default");
        let one = parse(r#"{"embeddings": {"baseUrl": "http://localhost:11434/v1", "model": "bge-m3"}}"#);
        assert_eq!((one.len(), one[0].model.as_str()), (1, "bge-m3"), "the old single-object form");
        let many = parse(
            r#"{"embeddings": [{"baseUrl": "https://g/v1", "model": "e"}, {"local": "qwen3-embedding-0.6b-q4"}]}"#,
        );
        assert_eq!(
            (many[0].base_url.as_str(), many[1].local.as_deref()),
            ("https://g/v1", Some("qwen3-embedding-0.6b-q4"))
        );
        assert!(parse(r#"{"embeddings": []}"#).is_empty());
        assert!(parse(r#"{"embeddings": null}"#).is_empty());

        let paths = Paths { home: std::path::PathBuf::from("/nonexistent") };
        let (embeddings, skipped) = MemorySettings::default().embeddings(&paths);
        assert!(embeddings.is_none(), "an uninstalled model is skipped");
        assert!(skipped[0].contains("not installed") || skipped[0].contains("no local"), "{skipped:?}");
    }
}

#[cfg(test)]
mod entry_tests {
    use super::*;

    /// The entry (decisions D93, D97): none by default; `settings.json` goes before one built in,
    /// and `NYPI_ENTRY` before both.
    #[test]
    fn the_entry_is_optional_and_can_be_set() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let loaded = |paths: &Paths| Settings::load(paths, &paths.home).unwrap();
        assert_eq!(loaded(&paths).entry_with(None, None), None, "the local network only");
        assert_eq!(
            loaded(&paths).entry_with(None, Some("built.example.com:443")).as_deref(),
            Some("built.example.com:443")
        );
        std::fs::write(paths.settings(), r#"{ "entry": "hub.example.org:443" }"#).unwrap();
        let entry = |variable: Option<&str>| {
            loaded(&paths).entry_with(variable.map(str::to_string), Some("built.example.com:443"))
        };
        assert_eq!(entry(None).as_deref(), Some("hub.example.org:443"));
        assert_eq!(entry(Some("127.0.0.1:9")).as_deref(), Some("127.0.0.1:9"));
        assert_eq!(entry(Some("  ")).as_deref(), Some("hub.example.org:443"));
    }

    /// Decision D100: a settings file of before, with `daemon`, reads as one with `hub` and
    /// `channels.telegram`; a new key set in the same file wins.
    #[test]
    fn a_daemon_section_reads_as_the_hubs() {
        let load = |text: &str| {
            let home = tempfile::tempdir().unwrap();
            let paths = Paths { home: home.path().to_path_buf() };
            std::fs::write(paths.settings(), text).unwrap();
            let settings = Settings::load(&paths, &paths.home).unwrap();
            (settings.hub.enabled, settings.hub.cwd.clone(), settings.channels.telegram.enabled)
        };
        let old = load(r#"{ "daemon": { "enabled": false, "cwd": "~/work", "telegram": false } }"#);
        let new = load(
            r#"{ "hub": { "enabled": false, "cwd": "~/work" }, "channels": { "telegram": { "enabled": false } } }"#,
        );
        assert_eq!(old, new);
        assert_eq!(old, (false, Some("~/work".to_string()), false));
        assert_eq!(load("{}"), (true, None, true), "on by default");
        let both = load(r#"{ "daemon": { "enabled": false }, "hub": { "enabled": true } }"#);
        assert!(both.0, "the new key wins");
        let defaults = Settings::default();
        assert_eq!(defaults.channels.webhook.listen, "127.0.0.1:7683");
        assert_ne!(defaults.channels.webhook.listen, RELAY_LISTEN, "the webhook and the relay never share a port");
    }

    /// `hub.listen` (decision D94): both address families on 7681 by default; one address as a
    /// string, as D80 wrote it, is still read; `[]` opens no port.
    #[test]
    fn the_hub_listens_on_both_families_unless_told_otherwise() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let loaded = |paths: &Paths| Settings::load(paths, &paths.home).unwrap();
        assert_eq!(loaded(&paths).hub.listen, ["0.0.0.0:7681", "[::]:7681"]);
        assert!(loaded(&paths).hub.lan_discovery && !loaded(&paths).hub.port_mapping);
        std::fs::write(paths.settings(), r#"{ "hub": { "listen": "100.64.0.7:7681" } }"#).unwrap();
        assert_eq!(loaded(&paths).hub.listen, ["100.64.0.7:7681"]);
        std::fs::write(paths.settings(), r#"{ "hub": { "listen": [] } }"#).unwrap();
        assert!(loaded(&paths).hub.listen.is_empty());
        let update = loaded(&paths).update;
        assert_eq!((update.mode, update.check_interval_hours), (UpdateMode::Notify, 6.0));
        let auto = UpdateSettings { mode: UpdateMode::Auto, ..update.clone() };
        assert_eq!(auto.effective_mode(true), UpdateMode::Notify, "a relay is only told");
        assert_eq!(UpdateSettings { auto_relay: true, ..auto.clone() }.effective_mode(true), UpdateMode::Auto);
        assert_eq!(auto.effective_mode(false), UpdateMode::Auto);
        let relay = loaded(&paths).relay;
        assert_eq!((relay.listen.as_str(), relay.trust_proxy, relay.enabled), ("127.0.0.1:7682", true, false));
    }
}
