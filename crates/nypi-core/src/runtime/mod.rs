//! The runtime every front end drives: agent + session persistence + compaction + Inspector,
//! publishing what happens as pulses (docs/architecture.md §4.1).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, anyhow, bail};
use async_trait::async_trait;
use nypi_agent::{Agent, AgentConfig, AgentEvent, AgentHandle, Block, ExecutionMode, ToolHooks, ToolOutput};
use nypi_ai::auth::{load_env_file, resolve_model_key};
use nypi_ai::{
    ContentBlock, Message, Model, ModelRegistry, StopReason, StreamFn, ThinkingLevel, ToolCall, ToolResultMessage,
    default_stream_fn, text_of,
};
use serde::Serialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::compaction;
use crate::config::{Paths, Settings, expand_home};
use crate::inspector::{Alert, AlertLevel, Approval, Inspector, call_summary};
use crate::ledger::{self, Ledger, LedgerEvent, RESTART_LIMIT, TaskId, TaskRecord, TaskState};
use crate::mcp::{McpClient, McpPrompt, parse_prompt_arguments};
use crate::prompt::{PromptInputs, load_context_files};
use crate::pulse::{Delivery, PulseBus, PulseId, PulsePayload, PulseSource};
use crate::retry::{Next, RetryNotice, RetryState};
use crate::session::SessionStore;
use crate::skills::{Skill, default_skill_dirs, load_skills};
use crate::status::{self, RunStatus, STATUS_NUDGE};
use crate::tools::command::create_command_tools;
use crate::tools::create_tools;

mod controls;
mod mcp;
mod run;
mod sessions;
mod tasks;

/// NyPi homes whose hub is handing over to another machine (decision D95).
static HANDING_OVER: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Mark the hub of `home` as handing over, or no longer. While it is, a run stopped there stays
/// running in the ledger: the hub that takes over finds it interrupted and resumes it, as after
/// a restart (decision D22), instead of seeing a task the user stopped.
pub fn handing_over(home: &std::path::Path, on: bool) {
    let mut homes = HANDING_OVER.lock().unwrap();
    homes.retain(|held| held != home);
    if on {
        homes.push(home.to_path_buf());
    }
}

fn is_handing_over(home: &std::path::Path) -> bool {
    HANDING_OVER.lock().unwrap().iter().any(|held| held == home)
}

/// Events delivered to front ends.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Agent {
        event: AgentEvent,
    },
    Alert {
        alert: Alert,
    },
    CompactionStart {
        reason: String,
    },
    CompactionEnd {
        tokens_before: u64,
        error: Option<String>,
    },
    /// A retry of a failed reply or compaction summary (Pi's `auto_retry_*` and
    /// `summarization_retry_*` events).
    Retry {
        notice: RetryNotice,
    },
}

pub type RuntimeSink = mpsc::UnboundedSender<RuntimeEvent>;

/// Which session to open.
#[derive(Clone, Debug, Default)]
pub enum SessionMode {
    #[default]
    New,
    /// Most recent session for the working directory, or a new one.
    Continue,
    Open(PathBuf),
    /// Never written to disk.
    Ephemeral,
    /// The hub's one conversation, kept in `sessions/primary` (decision D79).
    Primary,
}

/// Memories embedded in the background when a runtime starts; `nypi memory reembed` does the rest.
const BACKFILL_PER_START: usize = 200;

#[derive(Clone, Default)]
pub struct RuntimeOptions {
    pub cwd: PathBuf,
    pub paths: Option<Paths>,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    /// Model patterns to cycle through, replacing `enabledModels` (Pi's `--models`).
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub session: SessionMode,
    /// Replaces the network stream function (tests).
    pub stream_fn: Option<StreamFn>,
    /// Extra model registered before resolution (tests, ad-hoc endpoints).
    pub extra_model: Option<Model>,
    /// Run as another agent: its name selects its own self-prompt layer (a native specialist).
    pub agent_name: Option<String>,
    /// Instructions appended to the situational prompt layer (a specialist's profile).
    pub append_prompt: Option<String>,
    /// The task this runtime's tasks belong to (work delegated by another agent).
    pub parent_task: Option<TaskId>,
    /// No `delegate` tool, even when specialists exist (a specialist's own runtime).
    pub no_delegation: bool,
    /// Tell the agent what else it is working on (a conversation of the hub, decision D79).
    pub active_tasks: bool,
    /// The hub's devices (decision D79). With them the runtime's place is routed: it works at
    /// the focus set for each run, its device tools take a `device`, and `on_device` reaches
    /// what only some device offers.
    pub devices: Option<Arc<dyn crate::place::DeviceLookup>>,
    /// The hub's pulse bus, as a handle scoped to this runtime's thread (`PulseBus::scoped`).
    /// Without one the runtime has a bus of its own, scoped to its working directory.
    pub pulses: Option<PulseBus>,
    /// The thread's place, when it is on another device (docs/devices.md): its file and shell
    /// tools are the ones that device's node declares, and `cwd` is only where the hub keeps
    /// this conversation. Without one the place is this computer, at `cwd`.
    pub place: Option<crate::place::Remote>,
}

/// How a prompt ended, for front ends that report status.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    pub new_messages: Vec<Message>,
    pub final_stop: Option<StopReason>,
    pub error: Option<String>,
    /// Always present: stated by the agent, or derived by the harness (principle B6).
    pub status: RunStatus,
    /// The reply to show as the run's answer, including a status reply given after a nudge.
    pub answer: String,
    /// The user-message pulse that started the run; `nypi trace <id>` shows everything it caused.
    pub pulse: PulseId,
    /// The run's task in the ledger.
    pub task: Option<TaskId>,
    /// Schedules set during a run that ended `Continuing`: its way back (decision D59).
    pub wake: Vec<String>,
    /// What the answer cites, checked against the session's tool results (principle A4).
    pub citations: crate::citations::Citations,
}

/// Closes a runtime's thread on the pulse bus when the runtime is dropped, so the modules that
/// listen there (thinking, recall on drafts, chat) stop instead of waiting on a bus that other
/// threads keep alive.
struct ThreadEnd(PulseBus);

impl Drop for ThreadEnd {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// What a front end asks the runtime to run: a new prompt, or an interrupted task to resume.
#[derive(Clone, Debug)]
pub enum Job {
    Prompt(String),
    Resume(TaskId),
    /// A prompt that did not come from the user (a timer, a condition, a goal review, the DMN):
    /// its pulse names where it came from, so it is never mistaken for the user's own words.
    Sourced {
        text: String,
        source: PulseSource,
        /// The pulse that led to it (a timer firing in the hub), for the causal chain.
        cause: Option<PulseId>,
    },
}

/// How often a running task tells the ledger its process is alive, whatever the run is doing
/// (streaming, a long tool, waiting for approval). The watchdog treats silence as a hang.
pub const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// The models `patterns` name, in order, each once, with the patterns that name none (Pi's
/// `resolveModelScopeFromModels`). A glob matches `provider/id` or the bare id, ignoring case;
/// a pattern without one names a model as `--model` does. A `:level` suffix sets the model's
/// thinking level.
fn resolve_model_scope(
    patterns: &[String],
    registry: &ModelRegistry,
) -> (Vec<(Model, Option<ThinkingLevel>)>, Vec<String>) {
    let mut scope: Vec<(Model, Option<ThinkingLevel>)> = Vec::new();
    let mut unmatched = Vec::new();
    for pattern in patterns {
        let (base, level) = match pattern.rsplit_once(':') {
            Some((base, suffix)) if ThinkingLevel::parse(suffix).is_some() => (base, ThinkingLevel::parse(suffix)),
            _ => (pattern.as_str(), None),
        };
        let matches: Vec<Model> = if base.contains(['*', '?', '[']) && registry.find(base).is_none() {
            let Ok(glob) = globset::GlobBuilder::new(base).case_insensitive(true).build() else { continue };
            let glob = glob.compile_matcher();
            registry.all().iter().filter(|m| glob.is_match(m.reference()) || glob.is_match(&m.id)).cloned().collect()
        } else {
            registry.find(base).cloned().into_iter().collect()
        };
        if matches.is_empty() {
            unmatched.push(pattern.clone());
        }
        for model in matches {
            if !scope.iter().any(|(m, _)| m.provider == model.provider && m.id == model.id) {
                scope.push((model, level));
            }
        }
    }
    (scope, unmatched)
}

/// Pi's session statistics, for front ends (RPC `get_session_stats`).
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    /// None for a session never written to disk.
    pub session_file: Option<PathBuf>,
    pub session_id: String,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub total_messages: usize,
    pub tokens: TokenTotals,
    pub cost: f64,
    pub context_usage: Option<ContextUsage>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

/// How full the context window is; `tokens` and `percent` are unknown right after a compaction.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

/// A final reply that context recovery handles: compacting, then continuing the run once.
struct Recovery {
    failed: nypi_ai::AssistantMessage,
    /// An overflow, rather than a reply cut off by the length limit.
    overflow: bool,
}

/// Gives every finished tool result its reference (`[t7]`), which the model cites.
struct CitationHooks {
    index: Arc<Mutex<crate::citations::CitationIndex>>,
}

#[async_trait]
impl ToolHooks for CitationHooks {
    async fn after_tool_call(
        &self,
        call: &ToolCall,
        arguments: &serde_json::Value,
        mut output: ToolOutput,
    ) -> ToolOutput {
        let summary = call_summary(&call.name, arguments);
        let reference = self.index.lock().unwrap().assign(&call.id, &call.name, summary, output.is_error);
        crate::citations::mark(&mut output.content, &reference);
        output
    }
}

/// Writes tool intents to the ledger after the Inspector allowed them and before they run, and
/// their completion after (decision D22).
struct TaskHooks {
    ledger: Arc<Ledger>,
    task: Arc<Mutex<Option<TaskId>>>,
    inspector: Arc<Inspector>,
}

impl TaskHooks {
    fn write(&self, event: impl FnOnce(TaskId) -> LedgerEvent) {
        let Some(task) = self.task.lock().unwrap().clone() else { return };
        if let Err(error) = self.ledger.append(&event(task)) {
            self.inspector.alert(Alert::new(AlertLevel::Error, format!("Task ledger write failed: {error}")));
        }
    }
}

#[async_trait]
impl ToolHooks for TaskHooks {
    async fn before_tool_call(&self, call: &ToolCall, arguments: &serde_json::Value) -> Option<Block> {
        let summary: String = call_summary(&call.name, arguments).chars().take(200).collect();
        self.write(|task| LedgerEvent::ToolStarted {
            task,
            at: ledger::now(),
            call_id: call.id.clone(),
            tool: call.name.clone(),
            summary,
        });
        None
    }

    async fn after_tool_call(&self, call: &ToolCall, _arguments: &serde_json::Value, output: ToolOutput) -> ToolOutput {
        self.write(|task| LedgerEvent::ToolFinished {
            task,
            at: ledger::now(),
            call_id: call.id.clone(),
            is_error: output.is_error,
        });
        output
    }
}

/// The module name on pulses the runtime publishes about its runs.
pub const RUNTIME_MODULE: &str = "runtime";

/// Mid-run input from the user. It is published as a pulse before it reaches the agent, so the
/// audit log and other modules see it (principle A5).
#[derive(Clone)]
pub struct InputHandle {
    handle: AgentHandle,
    bus: PulseBus,
    channel: String,
}

impl InputHandle {
    /// The front end its messages are published as coming from.
    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn steer(&self, message: Message) {
        self.publish(Delivery::Steer, &message);
        self.handle.steer(message);
    }

    pub fn follow_up(&self, message: Message) {
        self.publish(Delivery::FollowUp, &message);
        self.handle.follow_up(message);
    }

    fn publish(&self, delivery: Delivery, message: &Message) {
        self.bus.publish(
            PulseSource::user(&self.channel),
            None,
            PulsePayload::UserMessage { delivery, text: message_text(message) },
        );
    }
}

pub struct Runtime {
    pub paths: Paths,
    pub settings: Settings,
    pub cwd: PathBuf,
    pub registry: ModelRegistry,
    pub skills: Vec<Skill>,
    pub agent: Agent,
    session: Arc<Mutex<SessionStore>>,
    inspector: Arc<Inspector>,
    alerts: Option<mpsc::UnboundedReceiver<Alert>>,
    tool_names: Vec<String>,
    /// Connected MCP servers and the agent tool names each one provides.
    mcp: Vec<(Arc<McpClient>, Vec<String>)>,
    mcp_prompts: Vec<McpPrompt>,
    bus: PulseBus,
    /// Ends this runtime's thread on the bus when the runtime goes.
    #[allow(dead_code)]
    thread_end: ThreadEnd,
    /// Delegations that outlive the run that started them (decision D67, part 4).
    delegations: Arc<crate::delegate::Background>,
    /// Where this thread's device tools run: a node, and this folder on it.
    place: Arc<crate::place::Place>,
    /// The device read tools the worker was not given, which chat reads with (decision D84).
    quiet_reads: Vec<Arc<dyn nypi_agent::Tool>>,
    /// The device the place is on, when it is not this computer.
    device: Option<String>,
    /// The vault workspace of the task this runtime is on (relative to the vault): its brief
    /// and findings are pinned into the prompt on every turn (decision D78).
    workspace: Arc<Mutex<Option<String>>>,
    /// The goal the task serves and the activation that started it (decision D85): the goal's
    /// project plan is pinned above the workspace.
    campaign: Arc<Mutex<(Option<String>, Option<String>)>>,
    /// What the folder of a run on another device says about itself, read there when the run began.
    place_context: Arc<Mutex<Option<nypi_node::proto::PlaceContext>>>,
    /// What this runtime was made from, so a task run beside it can be made alike (`beside`).
    made_from: RuntimeOptions,
    /// The id the next task is recorded under, when whoever starts it has already named it.
    next_task: Option<TaskId>,
    /// What the running task was asked to do, turns of it so far, and whether its context was
    /// compacted since the last turn: what the observer of its course goes by.
    goal: Arc<Mutex<String>>,
    turns: Arc<std::sync::atomic::AtomicU32>,
    compacted: Arc<std::sync::atomic::AtomicBool>,
    observing: bool,
    /// Front end named on user pulses.
    channel: String,
    ledger: Arc<Ledger>,
    /// The task being run, shared with the ledger's tool hooks.
    task: Arc<Mutex<Option<TaskId>>>,
    /// Who owns this runtime's tasks in the ledger (the agent, or "worker" behind chat).
    owner: String,
    parent_task: Option<TaskId>,
    /// The prompt carries what the agent's other conversations are doing.
    active_tasks: bool,
    /// Specialists the `delegate` tool can reach; listed in the system prompt.
    specialists: Vec<crate::specialist::Specialist>,
    /// Lucia's vault of notes, when the runtime offers it.
    vault: Option<Arc<nypi_vault::tools::VaultStore>>,
    /// The `delegate` tool's roster, when it has one: reloaded with the folder's profiles.
    roster: Option<Arc<std::sync::RwLock<Vec<crate::specialist::Specialist>>>>,
    /// The `delegate` tool, offered while the roster has someone in it.
    delegate_tool: Option<Arc<dyn nypi_agent::Tool>>,
    /// The skills and specialists of a device's folder added for the current run (decision
    /// D83), by name: taken out again before the next run's are added.
    remote_added: (Vec<String>, Vec<String>),
    /// The hub's own per-run settings while a trusted device folder's replace them (decision
    /// D91): put back before the next run's place is applied.
    place_base: Option<PlaceBase>,
    /// What the folder's own configuration looked like when last loaded (`refresh_folder`).
    folder_seen: Vec<(PathBuf, Option<std::time::SystemTime>)>,
    /// The session's tool result references.
    citations: Arc<Mutex<crate::citations::CitationIndex>>,
    /// Reflections running in the background; each yields a summary when it saved something.
    reflections: Vec<tokio::task::JoinHandle<Option<String>>>,
    /// Attaches relevant memories to prompts; present when the memory tool is on.
    valve: Option<crate::valve::MemoryValve>,
    /// Learns the user portrait from what they say; the agent the user talks to only.
    portrait: Option<Arc<crate::portrait::PortraitKeeper>>,
    /// The system prompt by section, as last built.
    prompt_parts: crate::prompt::PromptParts,
    /// Retries of the reply that keeps failing (crate::retry).
    retry: RetryState,
    /// The current run's settings for the turn hook (`TurnPreparation`).
    turn_run: Arc<Mutex<Option<TurnSettings>>>,
    /// Set when the run was stopped for making no progress (`stuckMinutes`): how long it was quiet.
    stuck: Arc<Mutex<Option<std::time::Duration>>>,
    /// The link to the board service, which reports this runtime while a board runs (D56).
    board: Option<Arc<crate::board_link::BoardLink>>,
}

impl Runtime {
    pub fn new(options: RuntimeOptions) -> anyhow::Result<Self> {
        let made_from = options.clone();
        let paths = options.paths.clone().unwrap_or_else(Paths::discover);
        let cwd = options.cwd.clone();
        let mut settings = Settings::load(&paths, &cwd)?;
        if let Some(name) = &options.agent_name {
            settings.agent_name = name.clone();
        }
        if let Some(models) = &options.models {
            settings.enabled_models = models.clone();
        }
        if let Some(extra) = options.append_prompt.as_deref().filter(|s| !s.trim().is_empty()) {
            let joined = match settings.append_system_prompt.take() {
                Some(existing) if !existing.trim().is_empty() => format!("{existing}\n\n{extra}"),
                _ => extra.to_string(),
            };
            settings.append_system_prompt = Some(joined);
        }
        load_env_file(&paths.env_file());
        for file in &settings.env_files {
            load_env_file(&expand_home(file));
        }
        let mut registry = ModelRegistry::load(&paths.models())?;
        if let Some(model) = options.extra_model.clone() {
            registry.add(model);
        }

        let session = match &options.session {
            SessionMode::New => SessionStore::create(&paths.sessions(), &cwd),
            SessionMode::Ephemeral => SessionStore::ephemeral(&cwd),
            SessionMode::Open(path) => SessionStore::open(path)?,
            SessionMode::Primary => SessionStore::primary(&paths.sessions(), &cwd)?,
            SessionMode::Continue => match SessionStore::most_recent(&paths.sessions(), &cwd) {
                Some(path) => SessionStore::open(&path)?,
                None => SessionStore::create(&paths.sessions(), &cwd),
            },
        };
        let restored = session.context();

        // As in Pi, the user's new session without a chosen model starts in the scope: on the
        // default model when the scope holds it, else on the scope's first usable model. A
        // specialist or delegated runtime keeps the default model; the scope is for cycling.
        let own = options.agent_name.is_none() && options.parent_task.is_none();
        let scope_start = if own && options.model.is_none() && restored.model.is_none() {
            let auth = paths.auth();
            let (scope, _) = resolve_model_scope(&settings.enabled_models, &registry);
            let usable: Vec<_> = scope.into_iter().filter(|(m, _)| resolve_model_key(m, &auth).is_some()).collect();
            let default = settings.default_model.as_deref().and_then(|r| registry.find(r));
            let in_scope = |m: &Model| default.is_some_and(|d| d.provider == m.provider && d.id == m.id);
            usable.iter().find(|(m, _)| in_scope(m)).or(usable.first()).cloned()
        } else {
            None
        };
        let model_ref = options
            .model
            .clone()
            .or(restored.model.clone())
            .or(scope_start.as_ref().map(|(m, _)| m.reference()))
            .or(settings.default_model.clone())
            .ok_or_else(|| {
                anyhow!(
                    "No model selected. Pass --model provider/id or set \"defaultModel\" in {}",
                    paths.settings().display()
                )
            })?;
        let model = registry.find(&model_ref).cloned().ok_or_else(|| anyhow!("Unknown model: {model_ref}"))?;
        let thinking = options
            .thinking
            .or_else(|| restored.thinking_level.as_deref().and_then(ThinkingLevel::parse))
            .or_else(|| scope_start.as_ref().and_then(|(_, level)| *level))
            .or_else(|| ThinkingLevel::parse(&settings.default_thinking))
            .map(|level| model.clamp_thinking(level))
            .unwrap_or_default();

        let extra_skill_dirs: Vec<PathBuf> = settings.skill_paths.iter().map(|p| expand_home(p)).collect();
        // A folder's own skills, like its settings, load only when the user trusts it (D61).
        let project = settings.untrusted_project.is_empty().then_some(cwd.as_path());
        let project = project.filter(|cwd| crate::trust::TrustStore::new(&paths).resolve(cwd).is_trusted());
        // A skill is read with `read`, which at another device runs there, where this computer's
        // skill files are not: the index is offered only where the tools can open it.
        let skills = match &options.place {
            None => load_skills(&default_skill_dirs(&paths.skills(), project, &extra_skill_dirs)),
            Some(_) => Vec::new(),
        };
        // What tools are kept away from, enforced twice: by the guard on tool arguments, and by
        // the operating system on every subprocess a tool starts (decision D62).
        let protected = Arc::new(crate::protected::Protected::new(&paths, &settings.env_files));
        let (sandbox, sandbox_problem) = crate::sandbox::Sandbox::detect(&settings.sandbox, &protected);
        // Where the sandbox mounts over what exists (Linux), this folder's own `.nypi` is covered too.
        let sandbox = sandbox.in_folder(&cwd);
        let mut tool_names = options.tools.clone().unwrap_or_else(|| settings.tools.clone());
        // On another device, that device's node offers the device tools; none are made here.
        let wanted = tool_names.clone();
        if options.place.is_some() {
            tool_names.retain(|name| !crate::place::DEVICE_TOOLS.contains(&name.as_str()));
        }
        // Chat reads files on the devices whatever the worker is given (decision D84): the read
        // tools are always made for the node, and those the worker was not given are kept apart.
        let quiet_reads: Vec<String> = match &options.place {
            None => crate::introspect::DEVICE_READS
                .iter()
                .filter(|name| !tool_names.iter().any(|wanted| wanted == *name))
                .map(|name| name.to_string())
                .collect(),
            Some(_) => Vec::new(),
        };
        let mut tools = create_tools(
            &[tool_names.clone(), quiet_reads.clone()].concat(),
            &cwd,
            &paths.self_prompt(&settings.agent_name),
            &paths.goals(),
            &sandbox,
            &protected,
        );
        // An explicit --tools list restricts command tools too (read-only runs stay read-only).
        // Command tools are this machine's settings: they are not offered at another device.
        let (command_tools, command_errors) = match &options.place {
            None => create_command_tools(&settings.command_tools, &cwd, &sandbox),
            Some(_) => (Vec::new(), Vec::new()),
        };
        let mut command_names = std::collections::HashSet::new();
        for tool in command_tools {
            let name = tool.definition().name;
            if options.tools.as_ref().is_none_or(|only| only.contains(&name)) {
                if !tool_names.contains(&name) {
                    tool_names.push(name.clone());
                }
                command_names.insert(name);
                tools.push(tool);
            }
        }

        let (alert_tx, alert_rx) = mpsc::unbounded_channel();
        // The guard resolves a tool's paths against the folder the tools work in. At another
        // device that folder is the device's, and what is protected there is the device's own
        // guard's to say (its node's gate); this one still refuses what is never acceptable.
        let guarded = options.place.as_ref().map_or_else(|| cwd.clone(), |place| PathBuf::from(&place.folder));
        let mut guard =
            crate::inspector::Guard::new(&paths, &guarded, (*protected).clone(), sandbox.judges_every_path());
        if let Some(place) = &options.place {
            guard = guard.at_device(place.node.report().identity.home.map(PathBuf::from));
        }
        let inspector = Arc::new(Inspector::new(&paths, &session.header.id, &settings.approval, alert_tx, guard));
        if let Some(why) = sandbox_problem {
            let message = format!(
                "The tool sandbox is not available ({why}), so NyPi's own folders are protected only by checks on \
                 what commands say, which a shell can get around. Set \"sandbox\": \"off\" to silence this."
            );
            inspector.alert(Alert::new(AlertLevel::Warning, message));
        }
        if !settings.untrusted_project.is_empty() {
            let message = format!(
                "This folder's own NyPi configuration ({}) was not loaded, because you have not trusted the folder. \
                 It can turn approvals off and add programs as tools. To load it: nypi trust yes",
                settings.untrusted_project.join(", ")
            );
            inspector.alert(Alert::new(AlertLevel::Warning, message));
        }
        for pattern in resolve_model_scope(&settings.enabled_models, &registry).1 {
            inspector.alert(Alert::new(AlertLevel::Warning, format!("No models match pattern \"{pattern}\"")));
        }
        // One bus per hub, each thread on a handle of its own; a runtime nobody gave a handle
        // is a hub of one thread.
        // A root handle is the hub's, never a thread's: this runtime's end must not end the bus.
        let scope = options.cwd.display().to_string();
        let bus = match options.pulses.clone() {
            Some(bus) if bus.thread().is_some() => bus,
            Some(bus) => bus.scoped(&scope),
            None => PulseBus::new().scoped(&scope),
        };
        bus.tap(inspector.clone());
        let ledger = Arc::new(Ledger::new(paths.ledger()));
        let task = Arc::new(Mutex::new(None));
        let task_hooks =
            Arc::new(TaskHooks { ledger: ledger.clone(), task: task.clone(), inspector: inspector.clone() });
        let citations = Arc::new(Mutex::new(crate::citations::CitationIndex::from_messages(&restored.messages)));
        // The tools made so far are this device's (files, the shell, its command tools). They
        // belong to this computer's node; the agent calls them through the place (D67, part 5).
        // What is added from here on is Lucia's own.
        let device = options.place.as_ref().map(|place| place.node.report().identity.name);
        let place = match options.place.clone() {
            None => crate::place::Place::local(&cwd, &mut tools, &command_names, task.clone()),
            Some(remote) => {
                let (place, declared) = crate::place::Place::remote(remote.node, &remote.folder, task.clone());
                for tool in declared {
                    // A computer's tools are offered as this machine's would be (settings,
                    // `--tools`); what only that device has (a phone's) is offered as declared.
                    let name = tool.definition().name;
                    let known = crate::place::DEVICE_TOOLS.contains(&name.as_str());
                    if !known || wanted.contains(&name) {
                        tool_names.push(name);
                        tools.push(tool);
                    } else if crate::introspect::DEVICE_READS.contains(&name.as_str()) {
                        // Kept for chat's reads, like a local node's.
                        tools.push(tool);
                    }
                }
                place
            }
        };
        // With the hub's devices the place is routed (decision D79): it follows the focus of
        // each run, and a call may name another device. This computer is then a node for any
        // folder, under this machine's own rules; without it only the place's own folder is here.
        let place = match options.devices.clone() {
            None => place,
            Some(devices) => {
                let here = match crate::device::this_computer(&paths, None) {
                    Ok(node) => Some(Arc::new(node) as Arc<dyn nypi_node::NodeLink>),
                    Err(error) => {
                        inspector.record("place", json!({ "thisComputerUnavailable": format!("{error:#}") }));
                        None
                    }
                };
                let (place, on_device) = place.routed(&mut tools, devices, here);
                if !tool_names.iter().any(|name| name == crate::place::ON_DEVICE) {
                    tool_names.push(crate::place::ON_DEVICE.to_string());
                    tools.push(on_device);
                }
                let judged = place.clone();
                inspector.set_router(Arc::new(move |tool, arguments| judged.judged(tool, arguments)));
                place
            }
        };
        // The read tools the worker was not given leave its list, for chat alone.
        let (quiet, offered): (Vec<_>, Vec<_>) = std::mem::take(&mut tools).into_iter().partition(|tool| {
            let name = tool.definition().name;
            crate::introspect::DEVICE_READS.contains(&name.as_str()) && !tool_names.contains(&name)
        });
        tools = offered;
        let quiet_reads = quiet;
        let mut valve = None;
        let wants = |name: &str| tool_names.iter().any(|t| t == name);
        // One set of embedding sources for memory and the vault: the same model loaded once.
        let embeddings = if wants("memory") || wants("vault") {
            let (embeddings, skipped) = settings.memory.embeddings(&paths);
            if !skipped.is_empty() {
                inspector.record("memory", json!({ "embeddingSourcesSkipped": skipped }));
            }
            embeddings.map(Arc::new)
        } else {
            None
        };
        if wants("memory") {
            let store = Arc::new(crate::memory::MemoryStore::new(paths.memory()));
            if let Some(embeddings) = &embeddings
                && let Ok(handle) = tokio::runtime::Handle::try_current()
            {
                // Load the model before the first message needs it, then cover memories that
                // have no vector from it yet (decision D33).
                embeddings.warm();
                let (embeddings, store, inspector) = (embeddings.clone(), store.clone(), inspector.clone());
                handle.spawn(async move {
                    match store.backfill(&embeddings, BACKFILL_PER_START).await {
                        Ok(0) => {}
                        Ok(count) => inspector.record("memory", json!({ "embedded": count })),
                        Err(error) => inspector.record("memory", json!({ "backfillError": format!("{error:#}") })),
                    }
                });
            }
            valve = Some(crate::valve::MemoryValve::new(
                store.clone(),
                embeddings.clone(),
                settings.memory.weights.clone(),
                settings.memory.valve.clone(),
            ));
            tools.push(Arc::new(crate::tools::memory::MemoryTool {
                store,
                embeddings: embeddings.clone(),
                weights: settings.memory.weights.clone(),
                agent: settings.agent_name.clone(),
                session: session.header.id.clone(),
                task: task.clone(),
            }));
        }
        // Lucia's own vault, registry, and brokered requests (decision D77). They are hers, not a
        // device's: they stay with the runtime wherever the thread's place is.
        let mut vault = None;
        if wants("vault") {
            match nypi_vault::tools::VaultStore::open(&paths.vault(), embeddings.clone()) {
                Ok(store) => {
                    let guard = protected.clone();
                    let read_guard: nypi_vault::tools::ReadGuard =
                        Arc::new(move |path| guard.read_denied(&crate::protected::real_path(path)));
                    tools.extend(nypi_vault::tools::vault_tools(&store, read_guard, &cwd));
                    vault = Some(store);
                }
                Err(error) => {
                    inspector.alert(Alert::new(AlertLevel::Warning, format!("The vault could not be opened: {error}")));
                }
            }
        }
        if wants("assets") {
            let registry = Arc::new(nypi_vault::assets::AssetRegistry::new(paths.assets()));
            tools.push(Arc::new(nypi_vault::tools::AssetTool { registry }));
        }
        if wants("http_request") {
            tools.push(Arc::new(crate::tools::http_request::HttpRequestTool { home: paths.home.clone(), store: None }));
        }
        if tool_names.iter().any(|t| t == "schedule") {
            tools.push(Arc::new(crate::tools::schedule::ScheduleTool {
                store: Arc::new(
                    crate::schedule::ScheduleStore::new(paths.schedules()).with_devices(options.devices.clone()),
                ),
                agent: settings.agent_name.clone(),
                cwd: cwd.clone(),
                remote: options.place.as_ref().map(|place| crate::place::DeviceFolder {
                    device: place.node.report().identity.name,
                    folder: place.folder.clone(),
                }),
                place: place.is_routed().then(|| place.clone()),
            }));
        }
        let (specialists, specialist_errors) = crate::specialist::load_all(&paths, &cwd);
        for error in specialist_errors {
            inspector.alert(Alert::new(AlertLevel::Warning, format!("Specialist profile skipped: {error}")));
        }
        let delegations = crate::delegate::Background::new();
        let wants_delegate = options.tools.as_ref().is_none_or(|only| only.iter().any(|t| t == "delegate"));
        // A specialist would work in this machine's stand-in folder, not at the device: not offered there yet.
        let specialists =
            if options.no_delegation || !wants_delegate || device.is_some() { Vec::new() } else { specialists };
        let mut roster = None;
        let mut delegate_tool: Option<Arc<dyn nypi_agent::Tool>> = None;
        // A routed runtime may meet specialists at a folder of a device (decision D83): its
        // tool is made now, and offered whenever the roster has someone in it.
        let offers = !options.no_delegation && wants_delegate && device.is_none();
        if offers && (!specialists.is_empty() || place.is_routed()) {
            let shared = Arc::new(std::sync::RwLock::new(specialists.clone()));
            roster = Some(shared.clone());
            let tool: Arc<dyn nypi_agent::Tool> = Arc::new(crate::delegate::DelegateTool {
                paths: paths.clone(),
                cwd: cwd.clone(),
                specialists: shared,
                task: task.clone(),
                inspector: inspector.clone(),
                stream_fn: options.stream_fn.clone(),
                extra_model: options.extra_model.clone(),
                bus: bus.clone(),
                background: delegations.clone(),
                place: place.is_routed().then(|| place.clone()),
            });
            delegate_tool = Some(tool.clone());
            if !specialists.is_empty() {
                tools.push(tool);
                if !tool_names.iter().any(|t| t == "delegate") {
                    tool_names.push("delegate".into());
                }
            }
        }
        for error in command_errors {
            inspector.alert(Alert::new(AlertLevel::Warning, error));
        }
        let config = AgentConfig {
            api_key: resolve_model_key(&model, &paths.auth()),
            model,
            thinking,
            system_prompt: String::new(),
            tools,
            // The Inspector decides first; only calls it lets through are recorded as intents.
            hooks: vec![
                inspector.clone() as Arc<dyn ToolHooks>,
                task_hooks,
                Arc::new(CitationHooks { index: citations.clone() }),
            ],
            // Installed once the runtime exists: it compacts the runtime's session.
            turn_hooks: None,
            stream_fn: options.stream_fn.clone().unwrap_or_else(default_stream_fn),
            tool_execution: ExecutionMode::Parallel,
            max_tokens: None,
            temperature: None,
            session_id: Some(session.header.id.clone()),
        };
        let parent_task = options
            .parent_task
            .clone()
            .or_else(|| std::env::var(crate::specialist::PARENT_TASK_ENV).ok().filter(|v| !v.is_empty()).map(TaskId));
        // The portrait is of the user, so only the agent the user talks to keeps it, not a
        // specialist working on a delegated task.
        let portrait = (parent_task.is_none()
            && options.agent_name.is_none()
            && settings.portrait.mode != crate::portrait::PortraitMode::Off)
            .then(|| {
                let model = settings
                    .portrait
                    .model
                    .as_deref()
                    .and_then(|r| registry.find(r).cloned())
                    .unwrap_or_else(|| config.model.clone());
                let api_key = if model == config.model {
                    config.api_key.clone()
                } else {
                    resolve_model_key(&model, &paths.auth())
                };
                Arc::new(crate::portrait::PortraitKeeper::new(
                    &bus,
                    crate::portrait::KeeperOptions {
                        portrait: Arc::new(crate::portrait::Portrait::new(paths.portrait())),
                        settings: settings.portrait.clone(),
                        model,
                        stream_fn: config.stream_fn.clone(),
                        api_key,
                        retry: settings.retry.clone(),
                        session: session.header.id.clone(),
                        inspector: inspector.clone(),
                    },
                ))
            });
        let owner = settings.agent_name.to_lowercase();
        let folder_seen = folder_fingerprint(&paths, &cwd);
        let mut runtime = Runtime {
            agent: Agent::new(config, restored.messages),
            paths,
            settings,
            cwd,
            registry,
            skills,
            session: Arc::new(Mutex::new(session)),
            inspector,
            alerts: Some(alert_rx),
            tool_names,
            mcp: Vec::new(),
            mcp_prompts: Vec::new(),
            thread_end: ThreadEnd(bus.clone()),
            bus,
            delegations,
            place,
            quiet_reads,
            channel: "cli".into(),
            owner,
            ledger,
            task,
            // Started by an external-specialist run: file this runtime's tasks under it.
            specialists,
            vault,
            roster,
            delegate_tool,
            remote_added: Default::default(),
            place_base: None,
            folder_seen,
            citations,
            reflections: Vec::new(),
            valve,
            portrait,
            prompt_parts: Default::default(),
            retry: RetryState::default(),
            turn_run: Arc::default(),
            stuck: Arc::default(),
            board: None,
            parent_task,
            active_tasks: options.active_tasks,
            device,
            workspace: Arc::default(),
            campaign: Arc::default(),
            place_context: Arc::default(),
            made_from,
            next_task: None,
            goal: Arc::default(),
            turns: Arc::default(),
            compacted: Arc::default(),
            observing: false,
        };
        let handle = runtime.agent.handle();
        handle.steering.set_mode(runtime.settings.steering_mode);
        handle.follow_up.set_mode(runtime.settings.follow_up_mode);
        let board = crate::board_link::BoardLink::start(crate::board_link::LinkParts {
            socket: runtime.paths.board_socket(),
            agent_name: runtime.settings.agent_name.clone(),
            model: runtime.agent.config.model.reference(),
            context_window: runtime.agent.config.model.context_window,
            cwd: runtime.cwd.display().to_string(),
            session_id: runtime.session_id(),
            agent: runtime.agent.handle(),
            input: runtime.input(),
            inspector: runtime.inspector.clone(),
        });
        runtime.bus.tap(board.clone());
        runtime.board = Some(board);
        runtime.agent.config.turn_hooks =
            Some(Arc::new(TurnPreparation { compactor: runtime.compactor(), run: runtime.turn_run.clone() }));
        runtime.refresh_system_prompt();
        if runtime.session.lock().unwrap().context().model.is_none() {
            let model = runtime.agent.config.model.clone();
            runtime.session.lock().unwrap().append_model_change(&model.provider, &model.id)?;
        }
        Ok(runtime)
    }

    pub fn handle(&self) -> AgentHandle {
        self.agent.handle()
    }

    pub fn model(&self) -> &Model {
        &self.agent.config.model
    }

    pub fn session_path(&self) -> PathBuf {
        self.session.lock().unwrap().path.clone()
    }

    pub fn session_id(&self) -> String {
        self.session.lock().unwrap().header.id.clone()
    }

    pub fn has_api_key(&self) -> bool {
        self.agent.config.api_key.is_some()
    }

    /// Alerts raised by the Inspector. Taken once by the front end.
    pub fn take_alerts(&mut self) -> Option<mpsc::UnboundedReceiver<Alert>> {
        self.alerts.take()
    }

    /// Put back an alert receiver taken with `take_alerts`.
    pub fn restore_alerts(&mut self, alerts: mpsc::UnboundedReceiver<Alert>) {
        self.alerts = Some(alerts);
    }

    /// Route approval requests to the caller. Front ends that can ask the user call this once;
    /// without it, calls that need approval are denied.
    pub fn enable_approvals(&self) -> mpsc::UnboundedReceiver<Approval> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inspector.set_approver(tx);
        rx
    }

    pub fn inspector(&self) -> Arc<Inspector> {
        self.inspector.clone()
    }

    /// The pulse bus; subscribe to follow runs, tools, and alerts.
    /// This runtime is a conversation, and whoever holds it brings a background specialist's
    /// report back to it (a `reported` pulse on its bus): `delegate` may return at once.
    /// Returns the count of delegations started and not reported yet.
    pub fn allow_background_delegation(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        self.delegations.allow();
        self.delegations.pending()
    }

    /// The device this thread's place is on, when it is not this computer.
    pub fn device(&self) -> Option<&str> {
        self.device.as_deref()
    }

    /// The folder on another device this thread works at, when its place is not this computer.
    pub fn remote(&self) -> Option<crate::place::DeviceFolder> {
        if let Some(focus) = self.place.focus() {
            return focus.remote();
        }
        let device = self.device.clone()?;
        Some(crate::place::DeviceFolder { device, folder: self.place.folder().to_string() })
    }

    /// Work at `focus` from the next run on: where the owner was when they asked, or what a
    /// brief names (decision D79). None, or a runtime that was not given the hub's devices,
    /// works at its own place.
    pub fn set_focus(&self, focus: Option<crate::place::Focus>) {
        self.place.set_focus(focus);
    }

    /// Where the next run works, when that is not the runtime's own place.
    pub fn focus(&self) -> Option<crate::place::Focus> {
        self.place.focus()
    }

    /// The folder on this computer the run works in: the focus when it is here, else the
    /// runtime's own (for a focus on another device, where the hub keeps the conversation).
    pub fn working_folder(&self) -> PathBuf {
        match self.place.focus() {
            Some(crate::place::Focus { device: None, folder }) => PathBuf::from(folder),
            _ => self.cwd.clone(),
        }
    }

    /// Put the runtime on a task's workspace in the vault (None takes it off): `brief.md` and
    /// `findings.md` there are shown to the agent on every turn, whatever was compacted away.
    pub fn set_workspace(&self, workspace: Option<String>) {
        *self.workspace.lock().unwrap() = workspace;
    }

    /// Put the runtime's next task on a goal and an activation (None, None takes it off).
    pub fn set_campaign(&self, goal: Option<String>, activation: Option<String>) {
        *self.campaign.lock().unwrap() = (goal, activation);
    }

    /// The goal and activation the runtime's task is on.
    pub fn campaign(&self) -> (Option<String>, Option<String>) {
        self.campaign.lock().unwrap().clone()
    }

    pub fn workspace(&self) -> Option<String> {
        self.workspace.lock().unwrap().clone()
    }

    /// The options for a runtime that works beside this one on a task of its own (decision
    /// D78): the same home, folder, place, model, and tools, a new session, and a thread of its
    /// own, named `thread`, on the same pulse bus.
    pub fn beside(&self, thread: &str) -> RuntimeOptions {
        RuntimeOptions {
            session: SessionMode::New,
            model: Some(self.model().reference()),
            pulses: Some(self.bus.root().scoped(thread)),
            parent_task: None,
            ..self.made_from.clone()
        }
    }

    /// Record the next task under `id`: whoever starts it has already told someone that name.
    pub fn preassign_task(&mut self, id: TaskId) {
        self.next_task = Some(id);
    }

    /// The brief and findings of the workspace the runtime is on, as they read now.
    fn workspace_view(
        vault: Option<&Arc<nypi_vault::tools::VaultStore>>,
        workspace: &Mutex<Option<String>>,
    ) -> Option<crate::prompt::TaskWorkspace> {
        let path = workspace.lock().unwrap().clone()?;
        let vault = vault?;
        let read = |name: &str| vault.read(&format!("{path}/{name}")).unwrap_or_default();
        Some(crate::prompt::TaskWorkspace {
            path: path.clone(),
            brief: read("brief.md"),
            findings: read("findings.md"),
        })
    }

    /// Start the observer of this runtime's runs, once, where Thinking is on (decision D78).
    pub(super) fn ensure_observer(&mut self) {
        let thinking = &self.settings.thinking;
        if self.observing || !thinking.enabled {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        self.observing = true;
        let model = thinking
            .model
            .as_deref()
            .and_then(|reference| self.registry.find(reference).cloned())
            .unwrap_or_else(|| self.agent.config.model.clone());
        let api_key = if model == self.agent.config.model {
            self.agent.config.api_key.clone()
        } else {
            resolve_model_key(&model, &self.paths.auth())
        };
        let (goal, vault, workspace) = (self.goal.clone(), self.vault.clone(), self.workspace.clone());
        let task = Arc::new(move || {
            let view = Runtime::workspace_view(vault.as_ref(), &workspace);
            crate::thinking::ObservedTask {
                goal: goal.lock().unwrap().clone(),
                brief: view.as_ref().map(|view| view.brief.clone()),
                findings: view.map(|view| view.findings),
            }
        });
        crate::thinking::spawn_observer(
            &self.bus,
            crate::thinking::ObserverOptions {
                settings: thinking.clone(),
                model,
                stream_fn: self.agent.config.stream_fn.clone(),
                api_key,
                retry: self.settings.retry.clone(),
                inspector: self.inspector.clone(),
                agent: self.agent.handle(),
                task,
            },
        );
    }

    /// Lucia's vault of notes, when this runtime offers it.
    /// The process's link to the board, on this runtime's channel: the planes in front of it
    /// report what they are doing through it (decision D88).
    pub fn board_link(&self) -> Option<Arc<crate::board_link::BoardLink>> {
        self.board.clone()
    }

    pub fn vault(&self) -> Option<&Arc<nypi_vault::tools::VaultStore>> {
        self.vault.as_ref()
    }

    /// The place this thread's device tools run at.
    pub fn place(&self) -> &Arc<crate::place::Place> {
        &self.place
    }

    /// The device read tools (`read`, `grep`, `find`, `ls`) as the agent calls them, through
    /// the place, whether or not the worker was given them: what chat reads with (D84).
    pub fn device_read_tools(&self) -> Vec<Arc<dyn nypi_agent::Tool>> {
        self.agent
            .config
            .tools
            .iter()
            .chain(&self.quiet_reads)
            .filter(|tool| crate::introspect::DEVICE_READS.contains(&tool.definition().name.as_str()))
            .cloned()
            .collect()
    }

    pub fn pulses(&self) -> &PulseBus {
        &self.bus
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// Name the front end on user pulses (cli, print, telegram, rpc).
    /// Name where the runs that follow come from (a fired schedule's `timer`), for the ledger
    /// and their pulses, without renaming this conversation on the board: it is the same
    /// conversation, and a name that changed for one run would relabel it mid-flight.
    pub fn set_origin(&mut self, origin: impl Into<String>) {
        self.channel = origin.into();
    }

    pub fn set_channel(&mut self, channel: impl Into<String>) {
        self.channel = channel.into();
        if let Some(board) = &self.board {
            board.front_end(self.input());
        }
    }

    /// Steering and follow-ups for the current run, published as user pulses.
    pub fn input(&self) -> InputHandle {
        InputHandle { handle: self.agent.handle(), bus: self.bus.clone(), channel: self.channel.clone() }
    }

    /// Bring the transcript's system prompt up to date (decision D40): a new session starts with
    /// a leading system message holding every section and tool; later, only sections that
    /// changed are appended as an update, so the cached head of the conversation stays the same.
    /// A session restored from before transcript prompts keeps the prompt in the request.
    fn sync_transcript(&mut self) {
        let parts = self.prompt_parts.clone();
        let messages = &self.agent.messages;
        let update = if messages.is_empty() {
            Some(nypi_ai::SystemMessage {
                sections: nypi_ai::Sections(parts.sections.iter().map(|(n, t)| (n.clone(), Some(t.clone()))).collect()),
                tools_added: self.agent.config.tools.iter().map(|t| t.definition()).collect(),
                timestamp: nypi_ai::now_ms(),
                ..Default::default()
            })
        } else {
            prompt_update(messages, &parts)
        };
        let Some(update) = update else { return };
        record_prompt_update(&self.inspector, &update);
        let message = Message::System(update);
        if let Err(error) = self.session.lock().unwrap().append_message(message.clone()) {
            self.inspector.alert(Alert::new(AlertLevel::Error, format!("Session write failed: {error}")));
        }
        self.agent.messages.push(message);
    }

    /// Rebuild the system prompt (re-reads self prompt, context files, and skills index).
    /// Pick up what changed in this folder's configuration since it was loaded: a trust answer
    /// given after the conversation opened, skills or specialist profiles added or removed.
    /// Skills and the roster apply at once; settings that made the tools and the approval
    /// policy apply when the conversation opens again, and the user is told so.
    /// What a trusted folder on another device offers, for the run working there (decision
    /// D83): its skills, read at the device, and its native specialists, which work through
    /// this runtime's tools at that folder. What the last run added is taken out first, and an
    /// external specialist's command is never run here. One of this machine's own of the same
    /// name keeps its place.
    pub(super) fn apply_place_context(&mut self) {
        if let Some(base) = self.place_base.take() {
            self.agent.config.thinking = base.thinking;
            self.settings.compaction = base.compaction;
            self.settings.retry = base.retry;
        }
        self.inspector.set_added_approval_tools(&[]);
        let (skills, specialists) = std::mem::take(&mut self.remote_added);
        self.skills.retain(|skill| !skills.contains(&skill.name));
        self.specialists.retain(|specialist| !specialists.contains(&specialist.name));
        let context = self.place_context.lock().unwrap().clone();
        let device = self.place.focus().and_then(|focus| focus.device);
        if let (Some(context), Some(device)) = (context, device) {
            if let Some(trusted) = &context.trusted_settings {
                self.apply_trusted_settings(trusted);
            }
            for note in context.skills {
                if !self.skills.iter().any(|skill| skill.name == note.name) && !note.path.is_empty() {
                    self.remote_added.0.push(note.name.clone());
                    self.skills.push(crate::skills::Skill {
                        name: note.name,
                        description: format!("{} (on {device})", note.description),
                        file_path: PathBuf::from(note.path),
                        disable_model_invocation: false,
                    });
                }
            }
            let plain = |text: &str| -> String {
                text.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).collect()
            };
            for note in context.specialist_profiles.into_iter().filter(|note| note.kind == "native") {
                let name = plain(&note.name);
                if name.is_empty() || self.specialists.iter().any(|specialist| specialist.name == name) {
                    continue;
                }
                // Its experience log is kept here, beside the device's other places.
                let dir = self.paths.home.join("places").join(plain(&device)).join("specialists").join(&name);
                if std::fs::create_dir_all(&dir).is_err() {
                    continue;
                }
                self.remote_added.1.push(name.clone());
                self.specialists.push(crate::specialist::Specialist {
                    name,
                    dir,
                    profile: crate::specialist::Profile {
                        description: format!("{} (from {device})", note.description),
                        model: note.model,
                        thinking: note.thinking,
                        tools: note.tools,
                        ..Default::default()
                    },
                    instructions: note.instructions,
                });
            }
        }
        if let Some(roster) = &self.roster {
            *roster.write().unwrap() = self.specialists.clone();
        }
        // `delegate` is offered while there is someone to delegate to.
        let wanted = !self.specialists.is_empty() && self.delegate_tool.is_some();
        let offered = self.agent.config.tools.iter().any(|tool| tool.definition().name == "delegate");
        match (wanted, offered) {
            (true, false) => {
                self.agent.config.tools.extend(self.delegate_tool.clone());
                self.tool_names.push("delegate".into());
            }
            (false, true) => {
                self.agent.config.tools.retain(|tool| tool.definition().name != "delegate");
                self.tool_names.retain(|name| name != "delegate");
            }
            _ => {}
        }
    }

    /// A trusted device folder's settings, for the runs working there (decision D91). Only what
    /// shapes a run and cannot weaken the hub is taken: the thinking level (within what the
    /// model supports), compaction, retries, and tools to ask approval for, added to the hub's.
    /// The approval mode, env files, MCP servers, the sandbox, channels, the hub, the board,
    /// and command tools are the hub's own and are never read from a device.
    fn apply_trusted_settings(&mut self, trusted: &serde_json::Value) {
        self.place_base = Some(PlaceBase {
            thinking: self.agent.config.thinking,
            compaction: self.settings.compaction.clone(),
            retry: self.settings.retry.clone(),
        });
        if let Some(level) = trusted["defaultThinking"].as_str().and_then(ThinkingLevel::parse) {
            self.agent.config.thinking = self.agent.config.model.clamp_thinking(level);
        }
        let number = |section: &str, key: &str| trusted[section][key].as_u64();
        let flag = |section: &str| trusted[section]["enabled"].as_bool();
        let compaction = &mut self.settings.compaction;
        compaction.enabled = flag("compaction").unwrap_or(compaction.enabled);
        compaction.reserve_tokens = number("compaction", "reserveTokens").unwrap_or(compaction.reserve_tokens);
        compaction.keep_recent_tokens =
            number("compaction", "keepRecentTokens").unwrap_or(compaction.keep_recent_tokens);
        let retry = &mut self.settings.retry;
        retry.enabled = flag("retry").unwrap_or(retry.enabled);
        retry.max_retries = number("retry", "maxRetries").map_or(retry.max_retries, |n| n.min(u32::MAX as u64) as u32);
        retry.base_delay_ms = number("retry", "baseDelayMs").unwrap_or(retry.base_delay_ms);
        retry.max_agent_delay_ms = number("retry", "maxAgentDelayMs").unwrap_or(retry.max_agent_delay_ms);
        let tools: Vec<String> = trusted["approval"]["tools"]
            .as_array()
            .map(|tools| tools.iter().filter_map(|tool| tool.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        self.inspector.set_added_approval_tools(&tools);
    }

    pub(super) fn refresh_folder(&mut self, sink: &RuntimeSink) {
        if self.device.is_some() {
            return;
        }
        let seen = folder_fingerprint(&self.paths, &self.cwd);
        if seen == self.folder_seen {
            return;
        }
        self.folder_seen = seen;
        let Ok(settings) = Settings::load(&self.paths, &self.cwd) else { return };
        let extra: Vec<PathBuf> = settings.skill_paths.iter().map(|p| expand_home(p)).collect();
        let trusted = crate::trust::TrustStore::new(&self.paths).resolve(&self.cwd).is_trusted();
        let project = (settings.untrusted_project.is_empty() && trusted).then_some(self.cwd.as_path());
        self.skills = load_skills(&default_skill_dirs(&self.paths.skills(), project, &extra));
        if let Some(roster) = &self.roster {
            let (specialists, _) = crate::specialist::load_all(&self.paths, &self.cwd);
            *roster.write().unwrap() = specialists.clone();
            self.specialists = specialists;
        }
        if settings.untrusted_project != self.settings.untrusted_project {
            self.settings.untrusted_project = settings.untrusted_project;
            self.raise(
                sink,
                Alert::new(
                    AlertLevel::Notice,
                    "This folder's trust or configuration changed: its skills and specialists apply now; its \
                     settings (approvals, command tools, MCP servers) apply when this conversation opens again",
                ),
            );
        }
    }

    pub fn refresh_system_prompt(&mut self) {
        self.prompt_parts = self.prompt_sources().parts();
        self.agent.config.system_prompt = self.prompt_parts.render();
    }

    fn prompt_sources(&self) -> PromptSources {
        // The prompt names where the run works: the focus when there is one, else the place.
        let shown = match self.place.focus() {
            Some(crate::place::Focus { device: Some(device), folder }) => {
                let system = self.place.devices().into_iter().find(|known| known.name == device);
                Some(Shown {
                    folder: PathBuf::from(folder),
                    device,
                    system: system.map(|known| known.system).unwrap_or_default(),
                })
            }
            Some(_) => None,
            None => self.device.as_ref().map(|device| Shown {
                folder: PathBuf::from(self.place.folder()),
                device: device.clone(),
                system: self.place.report().identity.system,
            }),
        };
        PromptSources {
            settings: self.settings.clone(),
            paths: self.paths.clone(),
            cwd: self.working_folder(),
            shown,
            tool_names: self.tool_names.clone(),
            skills: self.skills.clone(),
            specialists: self.specialists.clone(),
            active_tasks: self.active_tasks.then(|| self.active_tasks_now()).flatten(),
            devices: self.devices_now(),
            vault: self.vault.clone(),
            workspace: self.workspace.clone(),
            campaign: self.campaign.clone(),
            place_context: self.place_context.clone(),
        }
    }

    /// The hub's other devices, for a runtime that can reach them.
    fn devices_now(&self) -> Option<String> {
        if !self.place.is_routed() {
            return None;
        }
        let this_computer = self.place.here_name().unwrap_or_else(|| "the hub's".to_string());
        crate::place::devices_section(&this_computer, &self.place.devices())
    }

    /// What else is being worked on at this moment, from the ledger.
    fn active_tasks_now(&self) -> Option<String> {
        let tasks: Vec<TaskRecord> = self.ledger.load().into_values().collect();
        crate::active_tasks::section(&tasks, &self.session_id(), chrono::Utc::now(), ledger::process_alive)
    }

    /// Publish an alert (the pulse is its audit record) and deliver it to the front end.
    fn raise(&self, sink: &RuntimeSink, alert: Alert) {
        raise(&self.bus, &self.inspector, sink, alert);
    }

    /// Record a front-end action (input source, command) in the audit log.
    pub fn audit(&self, source: &str, payload: serde_json::Value) {
        self.inspector.record(source, payload);
    }
}

fn message_text(message: &Message) -> String {
    match message {
        Message::System(system) => nypi_ai::transcript::render(system),
        Message::User(user) => text_of(&user.content),
        Message::Assistant(assistant) => assistant.text(),
        Message::ToolResult(result) => text_of(&result.content),
    }
}

/// Publish an alert (the pulse is its audit record) and deliver it to the front end.
fn raise(bus: &PulseBus, inspector: &Inspector, sink: &RuntimeSink, alert: Alert) {
    bus.publish(PulseSource::module(RUNTIME_MODULE), inspector.cause(), PulsePayload::Alert { alert: alert.clone() });
    let _ = sink.send(RuntimeEvent::Alert { alert });
}

/// What compacting the session needs, shared by the runtime and its mid-run turn hook.
#[derive(Clone)]
struct Compactor {
    session: Arc<Mutex<SessionStore>>,
    inspector: Arc<Inspector>,
    bus: PulseBus,
    board: Option<Arc<crate::board_link::BoardLink>>,
    /// Set when a compaction went through, for the observer of the run's course.
    compacted: Arc<std::sync::atomic::AtomicBool>,
}

impl Compactor {
    /// Tell the front end and the board.
    fn emit(&self, sink: &RuntimeSink, event: RuntimeEvent) {
        if let Some(board) = &self.board {
            board.event(&event);
        }
        let _ = sink.send(event);
    }

    /// Summarize older history and return the model context that follows. Errors are reported as
    /// alerts and returned.
    async fn compact(
        &self,
        config: &AgentConfig,
        settings: &crate::config::CompactionSettings,
        retry: &nypi_ai::retry::RetryPolicy,
        reason: &str,
        sink: &RuntimeSink,
        abort: CancellationToken,
    ) -> anyhow::Result<Vec<Message>> {
        let plan = {
            let session = self.session.lock().unwrap();
            compaction::plan(&session, settings.keep_recent_tokens)
        };
        let Some(plan) = plan else { return Err(anyhow!("Nothing to compact yet")) };
        self.emit(sink, RuntimeEvent::CompactionStart { reason: reason.to_string() });
        let result = compaction::summarize(
            &plan,
            &config.model,
            &config.stream_fn,
            config.api_key.clone(),
            settings.reserve_tokens,
            retry,
            abort,
            |progress| {
                let notice = RetryNotice::summarization(progress, reason);
                report_retry(&self.inspector, self.board.as_deref(), sink, notice)
            },
        )
        .await;
        let outcome = result.and_then(|summary| {
            let mut session = self.session.lock().unwrap();
            session
                .append_compaction(summary, plan.first_kept_entry_id.clone(), plan.tokens_before, None)
                .context("Cannot persist compaction")?;
            Ok(session.context().messages)
        });
        match outcome {
            Ok(messages) => {
                self.inspector.record(
                    "harness",
                    json!({ "compaction": { "tokensBefore": plan.tokens_before, "reason": reason } }),
                );
                self.emit(sink, RuntimeEvent::CompactionEnd { tokens_before: plan.tokens_before, error: None });
                self.compacted.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(messages)
            }
            Err(error) => {
                let message = format!("{error:#}");
                self.emit(
                    sink,
                    RuntimeEvent::CompactionEnd { tokens_before: plan.tokens_before, error: Some(message.clone()) },
                );
                raise(&self.bus, &self.inspector, sink, Alert::new(AlertLevel::Error, message.clone()));
                Err(anyhow!(message))
            }
        }
    }

    /// Wait until the session holds `message`. The event forwarder persists messages as they end,
    /// a moment after the agent loop moves on; a compaction planned before then would drop them.
    async fn persisted(&self, message: &Message) -> bool {
        for _ in 0..PERSIST_WAIT_TRIES {
            if self.session.lock().unwrap().entry_id_of(message).is_some() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        false
    }
}

/// Up to 5 s for the forwarder to persist the newest message before a mid-run compaction.
const PERSIST_WAIT_TRIES: usize = 500;

/// What the system prompt is built from (settings, files on disk, tools, skills, specialists).
/// When the files that make a folder's configuration last changed: the trust store, the
/// settings, and the skill and specialist folders, NyPi's own and the folder's.
fn folder_fingerprint(paths: &Paths, cwd: &std::path::Path) -> Vec<(PathBuf, Option<std::time::SystemTime>)> {
    [
        paths.home.join("trust.json"),
        paths.settings(),
        paths.skills(),
        paths.home.join("specialists"),
        cwd.join(".nypi").join("settings.json"),
        cwd.join(".nypi").join("skills"),
        cwd.join(".nypi").join("specialists"),
        cwd.join(".agents").join("skills"),
    ]
    .into_iter()
    .map(|path| {
        let changed = std::fs::metadata(&path).and_then(|meta| meta.modified()).ok();
        (path, changed)
    })
    .collect()
}

/// Where a conversation at another device works, as its prompt names it.
#[derive(Clone)]
struct Shown {
    folder: PathBuf,
    device: String,
    /// That device's operating system, as its node reported it.
    system: String,
}

#[derive(Clone)]
struct PromptSources {
    settings: Settings,
    paths: Paths,
    cwd: PathBuf,
    /// The folder and device the prompt names, when the place is on another device.
    shown: Option<Shown>,
    tool_names: Vec<String>,
    skills: Vec<Skill>,
    specialists: Vec<crate::specialist::Specialist>,
    /// Read once when a run starts, so the sections do not change between its turns.
    active_tasks: Option<String>,
    devices: Option<String>,
    /// The task's workspace in the vault, read again on every turn.
    vault: Option<Arc<nypi_vault::tools::VaultStore>>,
    workspace: Arc<Mutex<Option<String>>>,
    campaign: Arc<Mutex<(Option<String>, Option<String>)>>,
    /// What the folder of a run on another device says about itself, read there when the run began.
    place_context: Arc<Mutex<Option<nypi_node::proto::PlaceContext>>>,
}

impl PromptSources {
    /// The prompt's sections now: the self prompt, context files, and goals are read again.
    fn parts(&self) -> crate::prompt::PromptParts {
        // At a folder on another device: the user's own global file from here, and the folder's
        // files as its device read them. Otherwise the files on the way down to the folder.
        let context_files = match self.place_context.lock().unwrap().as_ref() {
            Some(context) => {
                let home = crate::protected::real_path(&self.paths.home);
                let mut files: Vec<crate::prompt::ContextFile> = load_context_files(&self.paths, &self.paths.home)
                    .into_iter()
                    .filter(|file| crate::protected::real_path(&file.path).starts_with(&home))
                    .collect();
                files.extend(context.context_files.iter().map(|file| crate::prompt::ContextFile {
                    path: PathBuf::from(&file.path),
                    content: file.content.clone(),
                }));
                files
            }
            None => load_context_files(&self.paths, &self.cwd),
        };
        let workspace = Runtime::workspace_view(self.vault.as_ref(), &self.workspace);
        // A task for a goal sees the goal's plan as it reads now; an errand carries nothing.
        let goal = self.campaign.lock().unwrap().0.clone();
        let plan = goal.and_then(|goal| crate::campaign::project_plan(&self.paths, &goal));
        crate::prompt::build_prompt_parts(&PromptInputs {
            workspace: workspace.as_ref(),
            project_plan: plan.as_ref(),
            settings: &self.settings,
            paths: &self.paths,
            cwd: self.shown.as_ref().map_or(&self.cwd, |shown| &shown.folder),
            device: self.shown.as_ref().map(|shown| (shown.device.as_str(), shown.system.as_str())),
            tools: &self.tool_names,
            context_files: &context_files,
            skills: &self.skills,
            specialists: &self.specialists,
            active_tasks: self.active_tasks.as_deref(),
            devices: self.devices.as_deref(),
        })
    }
}

/// The update that brings the transcript's system prompt to `parts`: only the sections that
/// changed (decision D40). None when nothing changed, or when the transcript carries no prompt.
fn prompt_update(messages: &[Message], parts: &crate::prompt::PromptParts) -> Option<nypi_ai::SystemMessage> {
    if !nypi_ai::transcript::has_leading_system(messages) {
        return None;
    }
    let held = nypi_ai::transcript::current_system(messages).map(|s| s.sections).unwrap_or_default();
    let sections = nypi_ai::transcript::section_changes(&held, &parts.sections);
    (!sections.is_empty()).then(|| nypi_ai::SystemMessage {
        sections,
        timestamp: nypi_ai::now_ms(),
        ..Default::default()
    })
}

fn record_prompt_update(inspector: &Inspector, update: &nypi_ai::SystemMessage) {
    let changed: Vec<&str> = update.sections.0.iter().map(|(n, _)| n.as_str()).collect();
    inspector.record("harness", json!({ "systemPrompt": { "sections": changed, "tools": update.tools_added.len() } }));
}

/// What a run's turn hook works with: where its events go, and what the prompt is built from.
#[derive(Clone)]
struct TurnSettings {
    sink: RuntimeSink,
    prompt: PromptSources,
}

/// Prepares each turn after the first as Pi's `prepareNextTurn` does: compacts once the context
/// crosses the threshold (decision D45), so a long run of tool calls compacts instead of
/// overflowing, then brings the prompt's sections up to date (D47), so an edit made during the
/// run (the self prompt, goals, an AGENTS.md) reaches the next request.
struct TurnPreparation {
    compactor: Compactor,
    /// Set for each run by `run_agent_once`.
    run: Arc<Mutex<Option<TurnSettings>>>,
}

impl TurnPreparation {
    async fn compacted(
        &self,
        run: &TurnSettings,
        config: &AgentConfig,
        messages: &[Message],
        cancel: &CancellationToken,
    ) -> Option<Vec<Message>> {
        let settings = &run.prompt.settings;
        let tokens = compaction::context_tokens(messages);
        if !compaction::should_compact(tokens, &config.model, &settings.compaction) {
            return None;
        }
        if !self.compactor.persisted(messages.last()?).await {
            return None;
        }
        let reason = "context nearly full";
        self.compactor
            .compact(config, &settings.compaction, &settings.retry, reason, &run.sink, cancel.clone())
            .await
            .ok()
    }
}

#[async_trait]
impl nypi_agent::TurnHooks for TurnPreparation {
    async fn before_next_turn(
        &self,
        config: &AgentConfig,
        messages: &[Message],
        cancel: &CancellationToken,
    ) -> nypi_agent::TurnUpdate {
        let Some(run) = self.run.lock().unwrap().clone() else { return Default::default() };
        let transcript = self.compacted(&run, config, messages, cancel).await;
        let current = transcript.as_deref().unwrap_or(messages);
        let update = prompt_update(current, &run.prompt.parts());
        if let Some(update) = &update {
            record_prompt_update(&self.compactor.inspector, update);
        }
        nypi_agent::TurnUpdate { transcript, messages: update.map(Message::System).into_iter().collect() }
    }
}

/// Record a retry in the audit log and tell the front end and the board.
fn report_retry(
    inspector: &Inspector,
    board: Option<&crate::board_link::BoardLink>,
    sink: &RuntimeSink,
    notice: RetryNotice,
) {
    inspector.record("harness", json!({ "retry": &notice }));
    let event = RuntimeEvent::Retry { notice };
    if let Some(board) = board {
        board.event(&event);
    }
    let _ = sink.send(event);
}

fn tool_pulse(event: &AgentEvent) -> Option<PulsePayload> {
    match event {
        AgentEvent::ToolExecutionStart { call_id, tool_name, .. } => {
            Some(PulsePayload::ToolStarted { call_id: call_id.clone(), tool: tool_name.clone() })
        }
        AgentEvent::ToolExecutionEnd { call_id, tool_name, output, duration_ms } => Some(PulsePayload::ToolFinished {
            call_id: call_id.clone(),
            tool: tool_name.clone(),
            is_error: output.is_error,
            duration_ms: *duration_ms,
        }),
        _ => None,
    }
}

fn last_assistant(messages: &[Message]) -> Option<&nypi_ai::AssistantMessage> {
    messages.iter().rev().find_map(|m| match m {
        Message::Assistant(a) => Some(a),
        _ => None,
    })
}

/// The last non-empty assistant text: a run may end with an empty message after tool calls.
fn last_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) if !a.text().trim().is_empty() => Some(a.text()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The hub's own values of the settings a trusted device folder may set for its runs.
struct PlaceBase {
    thinking: ThinkingLevel,
    compaction: crate::config::CompactionSettings,
    retry: nypi_ai::retry::RetryPolicy,
}
