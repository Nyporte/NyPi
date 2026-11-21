//! The chat module (decision D84): the information and interaction plane of a conversation,
//! and the only part of Lucia that speaks to the user. It runs on the fast model, reads
//! everything (Lucia's stores, the work under way, and files on the devices, quickly and
//! never waiting on an approval, crate::introspect), and acts on nothing. What needs acting,
//! deciding, or thinking through goes up to the Principal (crate::principal) with its one
//! upward tool, `signal_principal`; the Principal comes back with a stimulus, which chat
//! tells the user in Lucia's one voice.
//!
//! The user comes first (decision D82). A message that arrives while chat is answering joins
//! that answer (the agent's steering), several that arrive together are one turn, and one that
//! arrives while chat phrases a stimulus, which the user does not see yet, stops that turn,
//! is answered, and the stopped one runs again after it. A signal is handed to the Principal
//! when chat's reply is out, with the conversation as it then stands, chat's reply included,
//! so the Principal is never blind to what the user was told.
//!
//! Every turn is written down step by step in `chat/turns.jsonl` (origin, focus, memories
//! recalled, reasoning, each read with its arguments and time, signals with the activation
//! they began, the reply), through the secret scrubber.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use async_trait::async_trait;
use nypi_agent::{
    Agent, AgentConfig, AgentEvent, AgentHandle, EventSink, ExecutionMode, Tool, ToolContext, ToolOutput,
};
use nypi_ai::auth::resolve_model_key;
use nypi_ai::{ContentBlock, Message, StopReason, StreamFn, ThinkingLevel, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::config::Settings;
use crate::inspector::Inspector;
use crate::principal::{PrincipalHandle, Started, Stimulus, Trigger};
use crate::pulse::{Delivery, PulseBus, PulseId, PulseKind, PulsePayload, PulseSource, Subscription};
use crate::retry::{Next, RetryNotice, RetryState};
use crate::runtime::{RunOutcome, Runtime};
use crate::task_pool::TaskPool;
use crate::worker::{self, WorkerEvent, WorkerHandle};

const CHAT_PROMPT: &str = include_str!("../prompts/chat.md");
/// The module name on pulses chat publishes.
pub const CHAT_MODULE: &str = "chat";
/// A long worker report is cut to this many characters in chat's context; the user saw all of it.
const RESULT_CONTEXT_CHARS: usize = 4000;
/// The recent conversation a signal is handed with: this many messages, the newest last.
const CONVERSATION_MESSAGES: usize = 12;
/// One message of it is cut past this many characters (a long stack trace fits), and all of
/// it past `CONVERSATION_CHARS`, the oldest first.
const CONVERSATION_MESSAGE_CHARS: usize = 4000;
const CONVERSATION_CHARS: usize = 12_000;
/// A read's output, as the turn log and a signal's digest keep it.
const LOGGED_OUTPUT_CHARS: usize = 2000;
const GATHERED_CHARS: usize = 600;

#[derive(Debug)]
pub enum ModuleEvent {
    Chat {
        event: AgentEvent,
    },
    /// Chat finished its turn.
    Replied {
        text: String,
        pulse: PulseId,
        /// The turn handed something up to the Principal: more may follow.
        signalled: bool,
        /// The turn told the user what the Principal asked (decision D84): a message for them
        /// whatever else is running.
        stimulus: bool,
    },
    /// The chat model failed; `delegated` says whether the message went to the Principal.
    ChatFailed {
        error: String,
        delegated: bool,
    },
    /// The Principal reviewed a task's settlement (decisions D78 and D84). `accepted`: the task
    /// is through, and `said` tells whether chat was asked to tell the user of it (otherwise
    /// the worker's report is the answer, as before). Not accepted: it was sent back for round
    /// `round`, which the user is not interrupted for.
    Reviewed {
        task: String,
        accepted: bool,
        said: bool,
        round: u32,
    },
    /// One of the Principal's activations finished (or never ran: `seq` 0), with its decision.
    ActivationFinished {
        decision: crate::principal::Decision,
    },
    /// Chat's reply failed with a transient error and is retried, or the retries are over.
    Retry {
        notice: RetryNotice,
    },
    Worker(WorkerEvent),
}

#[derive(Default)]
pub struct ChatOptions {
    /// `provider/id`; defaults to `settings.chat.model`.
    pub model: Option<String>,
    /// Replaces the network stream function (tests).
    pub stream_fn: Option<StreamFn>,
    /// The Principal's stream function; the worker's without one (tests replace it).
    pub principal_stream_fn: Option<StreamFn>,
}

enum ChatInput {
    /// The user's words, and where they were when they said them.
    Say(String, Option<crate::place::Focus>),
    /// The Principal asks chat to tell the user something.
    Stimulus(Stimulus),
    Reset,
    Context(Box<Message>),
}

/// A signal chat raised in the turn under way, handed up once its reply is out.
#[derive(Clone, Debug)]
struct Signal {
    activation: String,
    topic: String,
    urgency: String,
}

/// What the user's side and chat's turns share (decision D82): whether a turn the user sees
/// is running, so a message joins it, the conversation as it stands, and the signals raised.
struct Live {
    /// Held while a message is placed, so a turn that is ending cannot miss it.
    running: Mutex<bool>,
    agent: AgentHandle,
    /// The messages of the conversation up to now, and those that joined the running turn.
    conversation: Mutex<Vec<Message>>,
    focus: Arc<Mutex<Option<crate::place::Focus>>>,
    bus: PulseBus,
    channel: String,
    signals: Mutex<Vec<Signal>>,
}

/// Front ends talk to Lucia through this when the chat module is on.
#[derive(Clone)]
pub struct ChatHandle {
    inputs: mpsc::UnboundedSender<ChatInput>,
    agent: AgentHandle,
    worker: WorkerHandle,
    pool: TaskPool,
    principal: PrincipalHandle,
    /// Whether the Principal reviews a task's settlement before the user hears of it.
    reviews: bool,
    live: Arc<Live>,
}

impl ChatHandle {
    /// Queue a user message; turns run one at a time, in order.
    pub fn say(&self, text: impl Into<String>) {
        self.say_from(text, None);
    }

    /// The same, from an interface whose ambient focus is `focus`: the device and folder the
    /// user is at (decision D79). While chat is answering, the message joins that answer
    /// instead of waiting for a turn of its own.
    pub fn say_from(&self, text: impl Into<String>, focus: Option<crate::place::Focus>) {
        let text = text.into();
        let running = self.live.running.lock().unwrap();
        if *running {
            if focus.is_some() {
                *self.live.focus.lock().unwrap() = focus;
            }
            let message = Message::user_text(text.clone());
            self.live.conversation.lock().unwrap().push(message.clone());
            self.live.bus.publish(
                PulseSource::user(&self.live.channel),
                None,
                PulsePayload::UserMessage { delivery: Delivery::Steer, text },
            );
            self.live.agent.steer(message);
            return;
        }
        let _ = self.inputs.send(ChatInput::Say(text, focus));
    }

    /// Add `message` to chat's context without a turn: what the user's own shell command printed,
    /// so the next message is answered with it in view.
    pub fn add_context(&self, message: Message) {
        let _ = self.inputs.send(ChatInput::Context(Box::new(message)));
    }

    /// Forget the chat history (with a new worker session).
    pub fn reset(&self) {
        let _ = self.inputs.send(ChatInput::Reset);
    }

    /// Stop the chat turn and every working task. Returns whether a task was running.
    pub fn abort(&self) -> bool {
        self.agent.abort();
        self.pool.cancel_all()
    }

    /// The conversation's tasks.
    /// Whether chat is answering the user now (a stimulus being phrased is not counted).
    pub fn is_answering(&self) -> bool {
        *self.live.running.lock().unwrap()
    }

    pub fn pool(&self) -> &TaskPool {
        &self.pool
    }

    /// The conversation's executive.
    pub fn principal(&self) -> &PrincipalHandle {
        &self.principal
    }

    /// The task whose run just ended with `outcome`, when the Principal is going to review it
    /// before the user hears of it.
    pub fn review_pending(&self, outcome: &Result<RunOutcome, String>) -> Option<String> {
        let task = outcome.as_ref().ok()?.task.as_ref()?;
        self.reviews.then(|| self.pool.key_of(task)).flatten()
    }

    pub fn worker(&self) -> &WorkerHandle {
        &self.worker
    }
}

/// Split the runtime into its three planes (decision D84): the runtime becomes the worker, the
/// Principal decides on the strongest model, and the chat module on the fast model talks.
pub fn start(
    runtime: Runtime,
    options: ChatOptions,
) -> anyhow::Result<(ChatHandle, mpsc::UnboundedReceiver<ModuleEvent>)> {
    let reference = options
        .model
        .or_else(|| runtime.settings.chat.model.clone())
        .ok_or_else(|| anyhow!("No chat model: set chat.model in settings"))?;
    let model = runtime.registry.find(&reference).cloned().ok_or_else(|| anyhow!("Unknown chat model: {reference}"))?;
    let settings = runtime.settings.clone();
    let config = AgentConfig {
        api_key: resolve_model_key(&model, &runtime.paths.auth()),
        model,
        thinking: ThinkingLevel::parse(&settings.chat.thinking).unwrap_or_default(),
        system_prompt: String::new(),
        tools: Vec::new(),
        hooks: Vec::new(),
        turn_hooks: None,
        stream_fn: options.stream_fn.unwrap_or_else(|| runtime.agent.config.stream_fn.clone()),
        tool_execution: ExecutionMode::Sequential,
        max_tokens: None,
        temperature: None,
        session_id: None,
    };
    // The Principal: the strongest model, at high effort unless told otherwise.
    let principal_model = settings
        .principal
        .model
        .as_deref()
        .and_then(|reference| runtime.registry.find(reference).cloned())
        .unwrap_or_else(|| runtime.agent.config.model.clone());
    let principal_config = AgentConfig {
        api_key: if principal_model == runtime.agent.config.model {
            runtime.agent.config.api_key.clone()
        } else {
            resolve_model_key(&principal_model, &runtime.paths.auth())
        },
        model: principal_model,
        thinking: ThinkingLevel::parse(&settings.principal.thinking).unwrap_or(ThinkingLevel::High),
        system_prompt: String::new(),
        tools: Vec::new(),
        // Its calls are judged and recorded like the worker's: a schedule's command is asked about.
        hooks: vec![runtime.inspector() as Arc<dyn nypi_agent::ToolHooks>],
        turn_hooks: None,
        stream_fn: options.principal_stream_fn.unwrap_or_else(|| runtime.agent.config.stream_fn.clone()),
        tool_execution: ExecutionMode::Sequential,
        max_tokens: None,
        temperature: None,
        session_id: None,
    };
    let self_prompt = std::fs::read_to_string(runtime.paths.self_prompt(&settings.agent_name)).unwrap_or_default();
    let channel = runtime.channel().to_string();
    let bus = runtime.pulses().clone();
    let inspector = runtime.inspector();
    let settled = bus.subscribe(CHAT_MODULE, &[PulseKind::RunSettled]);
    let valve = runtime.valve();
    let portrait = runtime.portrait();
    let board = runtime.board_link();
    let portrait_file = crate::portrait::Portrait::new(runtime.paths.portrait());
    let vault = runtime.vault().cloned();
    let paths = runtime.paths.clone();
    let place = runtime.place().clone();
    // One scrubber for both planes, current with the store (decision D87).
    let scrubber = Arc::new(nypi_vault::secrets::LiveScrubber::new(&paths.home));
    let focus: Arc<Mutex<Option<crate::place::Focus>>> = Arc::default();
    // What chat may read itself, and the Principal's stores, from the worker's own tools,
    // before they move with it.
    let mut reads = crate::introspect::read_only_tools(&runtime.agent.config.tools);
    reads.extend(crate::introspect::device_reads(
        &runtime.device_read_tools(),
        runtime.inspector(),
        focus.clone(),
        place.is_routed().then(|| place.here_name()).flatten(),
    ));
    let store_tools: Vec<Arc<dyn Tool>> = runtime
        .agent
        .config
        .tools
        .iter()
        .filter(|tool| {
            let name = tool.definition().name;
            nypi_vault::tools::VAULT_TOOLS.contains(&name.as_str())
                || ["goals", "memory", "asset_manage", "schedule"].contains(&name.as_str())
        })
        .cloned()
        .collect();
    // The Principal's review costs a call per settled task. A scripted test model would spend
    // its next scripted reply on it, so with one it runs only when asked for by name.
    let reviews = match settings.chat.review {
        crate::config::ReviewMode::Off => false,
        crate::config::ReviewMode::Always => true,
        crate::config::ReviewMode::Auto => config.model.api != nypi_ai::Api::Faux,
    };
    let max_rounds = settings.tasks.max_review_rounds;
    // Chat's side of the conversation is kept beside the worker's session, so a restart of the
    // hub or a thread closed when idle does not cost the user what was said.
    let history_dir = paths.home.join("chat");
    let earlier = load_history(&history_dir, &runtime.session_id());
    let mut runtime = runtime;
    runtime.set_task_owner("worker");
    let (worker, mut worker_events) = worker::spawn(runtime);

    let pool = TaskPool::new(worker.clone(), settings.tasks.max_concurrent, vault.clone());
    let started: Started = Arc::default();
    let (inputs, mut inbox) = mpsc::unbounded_channel();
    let (out, events) = mpsc::unbounded_channel();
    let principal_api = principal_config.model.api;
    let principal = crate::principal::start(crate::principal::PrincipalParts {
        config: principal_config,
        store_tools,
        pool: pool.clone(),
        started: started.clone(),
        stimulate: {
            let inputs = inputs.clone();
            Arc::new(move |stimulus| {
                let _ = inputs.send(ChatInput::Stimulus(stimulus));
            })
        },
        reviewed: {
            let out = out.clone();
            Arc::new(move |task, accepted, said, round| {
                let _ = out.send(ModuleEvent::Reviewed { task, accepted, said, round });
            })
        },
        finished: {
            let out = out.clone();
            Arc::new(move |decision| {
                let _ = out.send(ModuleEvent::ActivationFinished { decision });
            })
        },
        paths: paths.clone(),
        agent_name: settings.agent_name.clone(),
        user_ref: user_ref(&settings),
        self_prompt: self_prompt.clone(),
        place: place.clone(),
        vault,
        scrubber: scrubber.clone(),
        portrait: (settings.portrait.mode != crate::portrait::PortraitMode::Off)
            .then(|| crate::portrait::Portrait::new(paths.portrait())),
        max_rounds,
        max_queued: settings.principal.max_activations_queued,
        enforce_disposition: settings.principal.enforce_disposition.applies(principal_api),
        board: board.clone(),
        valve: valve.clone(),
        dmn: settings.dmn.clone(),
        batch_hold: std::time::Duration::from_secs(settings.tasks.batch_hold_seconds),
    });
    let mut agent = Agent::new(config, earlier);
    let live = Arc::new(Live {
        running: Mutex::new(false),
        agent: agent.handle(),
        conversation: Mutex::default(),
        focus: focus.clone(),
        bus: bus.clone(),
        channel: channel.clone(),
        signals: Mutex::default(),
    });
    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(SignalPrincipal { live: live.clone() }),
        Arc::new(crate::principal::TasksInspect { pool: pool.clone(), principal: principal.clone() }),
    ];
    tools.extend(reads);
    agent.config.tools = tools;
    let handle = ChatHandle {
        inputs,
        agent: agent.handle(),
        worker: worker.clone(),
        pool: pool.clone(),
        principal: principal.clone(),
        reviews,
        live: live.clone(),
    };

    // Every event of the tasks goes on to whoever draws them. A settlement of a task the pool
    // started is the Principal's to review (P1), or, without reviews, the end of that task.
    let forward = out.clone();
    tokio::spawn({
        let (pool, principal) = (pool.clone(), principal.clone());
        async move {
            while let Some(event) = worker_events.recv().await {
                let review = match &event {
                    WorkerEvent::Settled { outcome, .. } => {
                        pool.own_task().and_then(|task| pool.settled(&task, outcome))
                    }
                    WorkerEvent::Beside { task, event } => match &**event {
                        WorkerEvent::Settled { outcome, .. } => pool.settled(task, outcome),
                        _ => None,
                    },
                    _ => None,
                };
                if forward.send(ModuleEvent::Worker(event)).is_err() {
                    break;
                }
                match review {
                    Some(review) if reviews => {
                        principal.wake(Trigger::settled(&review));
                    }
                    Some(review) => pool.release(&review.task),
                    None => {}
                }
            }
        }
    });
    let mut module = Module {
        base_prompt: base_prompt(&settings, &self_prompt),
        history: settings.chat.history_messages.max(4),
        channel,
        bus,
        inspector,
        settled,
        worker,
        cause: Arc::default(),
        out,
        valve,
        portrait,
        portrait_file: (settings.portrait.mode != crate::portrait::PortraitMode::Off).then_some(portrait_file),
        retry: settings.retry.clone(),
        history_dir,
        pool,
        started,
        reviews,
        focus,
        live,
        principal,
        place,
        paths,
        scrubber,
        board,
        preempted: Arc::default(),
    };
    tokio::spawn(async move {
        // What the user sent and no turn has taken yet, and stimuli a message stopped.
        let mut pending: VecDeque<ChatInput> = VecDeque::new();
        let mut deferred: VecDeque<Stimulus> = VecDeque::new();
        let mut open = true;
        loop {
            // The user first, whatever else waits.
            while let Ok(input) = inbox.try_recv() {
                pending.push_back(input);
            }
            let says = pending.iter().position(|input| !matches!(input, ChatInput::Stimulus(_)));
            if let Some(at) = says {
                let input = pending.remove(at).expect("found");
                match input {
                    ChatInput::Say(text, focus) => {
                        // Several messages that arrived together are one turn.
                        let (mut texts, mut focus) = (vec![text], focus);
                        while let Some(ChatInput::Say(..)) = pending.get(at) {
                            let Some(ChatInput::Say(text, more)) = pending.remove(at) else { break };
                            texts.push(text);
                            focus = more.or(focus);
                        }
                        *module.focus.lock().unwrap() = focus;
                        let left = module.turn(&mut agent, &texts.join("\n\n")).await;
                        // What joined the answer too late for it is the next turn.
                        for text in left.into_iter().rev() {
                            pending.push_front(ChatInput::Say(text, None));
                        }
                    }
                    ChatInput::Reset => agent.messages.clear(),
                    ChatInput::Context(message) => agent.messages.push(*message),
                    ChatInput::Stimulus(_) => unreachable!("taken below"),
                }
                continue;
            }
            // Then what the Principal asked chat to say: stopped ones first.
            let stimulus = deferred.pop_front().or_else(|| match pending.pop_front() {
                Some(ChatInput::Stimulus(stimulus)) => Some(stimulus),
                Some(other) => {
                    pending.push_front(other);
                    None
                }
                None => None,
            });
            if let Some(stimulus) = stimulus {
                if let Some(again) =
                    convey_unseen(&mut module, &mut agent, stimulus, &mut inbox, &mut pending, &mut open).await
                {
                    deferred.push_front(again);
                }
                continue;
            }
            if !open {
                break;
            }
            match inbox.recv().await {
                Some(input) => pending.push_back(input),
                None => open = false,
            }
        }
    });
    Ok((handle, events))
}

/// Phrase a stimulus for the user, watching for them meanwhile. A message from them stops the
/// turn: the stimulus is handed back (Some) to be phrased after the user has been answered.
async fn convey_unseen(
    module: &mut Module,
    agent: &mut Agent,
    stimulus: Stimulus,
    inbox: &mut mpsc::UnboundedReceiver<ChatInput>,
    pending: &mut VecDeque<ChatInput>,
    open: &mut bool,
) -> Option<Stimulus> {
    let (preempted, abort) = (module.preempted.clone(), agent.handle());
    preempted.store(false, Ordering::SeqCst);
    let turn = module.convey(agent, stimulus);
    tokio::pin!(turn);
    loop {
        tokio::select! {
            again = &mut turn => return again,
            input = inbox.recv(), if *open => match input {
                Some(input) => {
                    if matches!(input, ChatInput::Say(..)) && !preempted.swap(true, Ordering::SeqCst) {
                        abort.abort();
                    }
                    pending.push_back(input);
                }
                None => *open = false,
            },
        }
    }
}

async fn run_agent(agent: &mut Agent, prompts: Option<Vec<Message>>, tx: &EventSink) -> Vec<Message> {
    match prompts {
        Some(prompts) => agent.prompt(prompts, tx).await,
        None => agent.continue_run(tx).await,
    }
}

struct Module {
    base_prompt: String,
    history: usize,
    channel: String,
    bus: PulseBus,
    inspector: Arc<Inspector>,
    settled: Subscription,
    worker: WorkerHandle,
    cause: Arc<Mutex<Option<PulseId>>>,
    out: mpsc::UnboundedSender<ModuleEvent>,
    /// Chat is where conversation happens, so it gets relevant memories too (decision D32).
    valve: Option<crate::valve::MemoryValve>,
    /// And it is where the user says the most about themselves (decision D35).
    portrait: Option<Arc<crate::portrait::PortraitKeeper>>,
    portrait_file: Option<crate::portrait::Portrait>,
    retry: nypi_ai::retry::RetryPolicy,
    /// Where chat's history is kept, one file per worker session.
    history_dir: std::path::PathBuf,
    pool: TaskPool,
    /// The pulses of the runs the Principal started: their settlements are reviewed, not noted.
    started: Started,
    reviews: bool,
    /// Where the user was when they said the message being answered.
    focus: Arc<Mutex<Option<crate::place::Focus>>>,
    live: Arc<Live>,
    principal: PrincipalHandle,
    place: Arc<crate::place::Place>,
    paths: crate::config::Paths,
    scrubber: Arc<nypi_vault::secrets::LiveScrubber>,
    /// Where chat says what it is doing while a turn is in flight (decision D88).
    board: Option<Arc<crate::board_link::BoardLink>>,
    /// Set when a user message stopped the unseen turn under way.
    preempted: Arc<AtomicBool>,
}

/// One turn of chat's, as `chat/turns.jsonl` keeps it (decision D84).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TurnRecord {
    pub turn_id: String,
    pub at: String,
    /// `userSay` or `principalStimulus`.
    pub origin: String,
    pub focus: Option<crate::place::Focus>,
    pub user_text: Option<String>,
    pub stimulus: Option<Stimulus>,
    pub recalled_memories: Vec<String>,
    pub thinking: String,
    pub tool_calls: Vec<ToolStep>,
    pub signals_fired: Vec<SignalFired>,
    pub reply_text: String,
    pub skipped: bool,
    pub tokens: u64,
    pub cost: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolStep {
    pub name: String,
    pub arguments: Value,
    pub duration_ms: u64,
    pub output_excerpt: String,
    pub timed_out: bool,
    pub is_error: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SignalFired {
    pub topic: String,
    pub urgency: String,
    pub activation_id: String,
}

/// Every turn of the log, oldest first.
pub fn read_turns(paths: &crate::config::Paths) -> Vec<TurnRecord> {
    std::fs::read_to_string(paths.chat_turns())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

impl Module {
    /// Run a chat turn, retrying a reply that failed with a transient error as worker runs do
    /// (decision D43). A `live` turn's events and retry notices go out as they happen; a turn
    /// held back shows no notices. Every event is returned, for the turn log and to show a
    /// held turn after all.
    async fn run_turn(
        &self,
        agent: &mut Agent,
        prompts: Vec<Message>,
        cause: &PulseId,
        live: bool,
    ) -> (Vec<Message>, Vec<AgentEvent>) {
        let retry = RetryState::default();
        let mut seen = Vec::new();
        let mut added = self.run_once(agent, Some(prompts), cause, live, &mut seen).await;
        let mut batch = 0;
        loop {
            let notify = |notice| {
                if live {
                    self.retry_notice(notice, cause);
                }
            };
            let Next::Retry { delay_ms } = retry.after_run(&self.retry, &added[batch..], notify) else { break };
            if !nypi_ai::retry::back_off(delay_ms, &agent.abort_signal()).await {
                if let Some(notice) = retry.cancelled().filter(|_| live) {
                    self.retry_notice(notice, cause);
                }
                break;
            }
            crate::retry::drop_failed_reply(agent);
            batch = added.len();
            added.extend(self.run_once(agent, None, cause, live, &mut seen).await);
        }
        (added, seen)
    }

    /// One agent run: from `prompts`, or continued from the transcript when there are none.
    async fn run_once(
        &self,
        agent: &mut Agent,
        prompts: Option<Vec<Message>>,
        cause: &PulseId,
        live: bool,
        seen: &mut Vec<AgentEvent>,
    ) -> Vec<Message> {
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        if !live {
            let added = run_agent(agent, prompts, &tx).await;
            drop(tx);
            seen.extend(std::iter::from_fn(|| rx.try_recv().ok()));
            return added;
        }
        let (out, inspector, cause) = (self.out.clone(), self.inspector.clone(), cause.clone());
        let mut plane = crate::board_link::PlaneReporter::new(self.board.clone(), "chat", cause.0.clone());
        let forwarder = tokio::spawn(async move {
            let mut kept = Vec::new();
            while let Some(event) = rx.recv().await {
                inspector.record_module_event(CHAT_MODULE, &event, Some(&cause));
                plane.observe(&event);
                kept.push(event.clone());
                let _ = out.send(ModuleEvent::Chat { event });
            }
            plane.finish();
            kept
        });
        let added = run_agent(agent, prompts, &tx).await;
        drop(tx);
        seen.extend(forwarder.await.unwrap_or_default());
        added
    }

    fn retry_notice(&self, notice: RetryNotice, cause: &PulseId) {
        self.inspector.record(CHAT_MODULE, json!({ "retry": &notice, "cause": cause }));
        let _ = self.out.send(ModuleEvent::Retry { notice });
    }

    /// The system prompt for a turn: who chat is, the portrait, where the user is, the devices,
    /// and what is being worked on.
    fn system_prompt(&self) -> String {
        let portrait = self
            .portrait_file
            .as_ref()
            .and_then(crate::portrait::Portrait::prompt_section)
            .map(|p| format!("\n\n<portrait>\n{}\n{p}\n</portrait>", crate::prompt::PORTRAIT_INTRO))
            .unwrap_or_default();
        let at = self
            .focus
            .lock()
            .unwrap()
            .as_ref()
            .map(|focus| format!("\n\n## Where\nThe user is at {focus}: relative paths you read start there."))
            .unwrap_or_default();
        let here = self.place.here_name().unwrap_or_else(|| "this computer".into());
        let devices = crate::place::devices_section(&here, &self.place.devices())
            .map(|devices| format!("\n\n<devices>\n{devices}\n</devices>"))
            .unwrap_or_default();
        format!(
            "{}{portrait}{at}{devices}\n\n## Tasks\n{}\n\n## Principal\n{}",
            self.base_prompt,
            self.pool.describe(None),
            self.principal.status().describe()
        )
    }

    /// A turn the user sees. Messages that arrive meanwhile join it; what arrived too late for
    /// it is returned, to be the next turn.
    async fn turn(&mut self, agent: &mut Agent, text: &str) -> Vec<String> {
        {
            let mut running = self.live.running.lock().unwrap();
            let mut conversation = agent.messages.clone();
            conversation.push(Message::user_text(text));
            *self.live.conversation.lock().unwrap() = conversation;
            self.live.signals.lock().unwrap().clear();
            *running = true;
        }
        self.answer(agent, text).await;
        let mut running = self.live.running.lock().unwrap();
        *running = false;
        agent
            .handle()
            .steering
            .take_all()
            .iter()
            .filter_map(|message| match message {
                Message::User(user) => Some(nypi_ai::text_of(&user.content)),
                _ => None,
            })
            .collect()
    }

    async fn answer(&mut self, agent: &mut Agent, text: &str) {
        let pulse = self.bus.publish(
            PulseSource::user(&self.channel),
            None,
            PulsePayload::UserMessage { delivery: Delivery::Prompt, text: text.to_string() },
        );
        *self.cause.lock().unwrap() = Some(pulse.clone());
        // A settlement the Principal reviews comes to the user through its stimulus.
        let reviewed = |pulse: &crate::pulse::Pulse| {
            self.reviews && pulse.causation.as_ref().is_some_and(|cause| self.started.lock().unwrap().contains(cause))
        };
        let mut prompts: Vec<Message> = std::iter::from_fn(|| self.settled.try_recv())
            .filter(|pulse| !reviewed(pulse))
            .filter_map(result_note)
            .collect();
        let mut message = Message::user_text(text);
        let mut recalled = Vec::new();
        if let Some(valve) = &self.valve
            && let Some(decision) = valve.consider(text, &agent.messages).await
        {
            recalled = decision.surfaced.iter().map(|memory| format!("{}: {}", memory.id, memory.text)).collect();
            crate::valve::attach(&mut message, &decision);
            self.bus.publish(
                PulseSource::module("memory"),
                Some(pulse.clone()),
                PulsePayload::MemoryValve { decision },
            );
        }
        prompts.push(message);
        agent.config.system_prompt = self.system_prompt();

        let (added, seen) = self.run_turn(agent, prompts, &pulse, true).await;
        trim_history(&mut agent.messages, self.history);
        self.keep(&agent.messages);

        let last = added.iter().rev().find_map(|m| match m {
            Message::Assistant(a) => Some(a),
            _ => None,
        });
        let focus = self.focus.lock().unwrap().clone();
        let signals: Vec<Signal> = std::mem::take(&mut *self.live.signals.lock().unwrap());
        if let Some(failed) = last.filter(|a| a.stop_reason == StopReason::Error) {
            // The chat model being down must not cost the user their request (principle B6):
            // it goes up to the Principal as it was said.
            let error = failed.error_message.clone().unwrap_or_else(|| "unknown error".into());
            let conversation = conversation_slice(&self.live.conversation.lock().unwrap()).unwrap_or_default();
            let activation = crate::principal::new_activation_id();
            self.principal.wake_caused(
                activation.clone(),
                Trigger::UserSignal {
                    topic: text.to_string(),
                    urgency: "normal".into(),
                    conversation,
                    gathered: String::new(),
                    focus: focus.clone(),
                },
                Some(pulse.clone()),
            );
            self.log_turn(
                TurnRecord {
                    origin: "userSay".into(),
                    focus,
                    user_text: Some(text.to_string()),
                    recalled_memories: recalled,
                    signals_fired: vec![SignalFired {
                        topic: text.into(),
                        urgency: "normal".into(),
                        activation_id: activation,
                    }],
                    reply_text: format!("(the chat model failed: {error})"),
                    ..Default::default()
                },
                &added,
                &seen,
            );
            let _ = self.out.send(ModuleEvent::ChatFailed { error, delegated: true });
            return;
        }
        if let Some(portrait) = &self.portrait {
            portrait.after_turn();
        }
        let reply = last.map(|a| a.text()).unwrap_or_default();
        self.bus.publish(
            PulseSource::module(CHAT_MODULE),
            Some(pulse.clone()),
            PulsePayload::Reply { text: reply.clone() },
        );
        let _ = self.out.send(ModuleEvent::Replied {
            text: reply.clone(),
            pulse: pulse.clone(),
            signalled: !signals.is_empty(),
            stimulus: false,
        });
        // The reply is out: now the Principal gets what chat raised, with everything the user
        // was told, chat's reply included.
        let conversation = conversation_slice(&agent.messages).unwrap_or_default();
        let gathered = gathered(&seen);
        let mut fired = Vec::new();
        for signal in signals {
            self.principal.wake_caused(
                signal.activation.clone(),
                Trigger::UserSignal {
                    topic: signal.topic.clone(),
                    urgency: signal.urgency.clone(),
                    conversation: conversation.clone(),
                    gathered: gathered.clone(),
                    focus: focus.clone(),
                },
                Some(pulse.clone()),
            );
            fired.push(SignalFired { topic: signal.topic, urgency: signal.urgency, activation_id: signal.activation });
        }
        self.log_turn(
            TurnRecord {
                origin: "userSay".into(),
                focus,
                user_text: Some(text.to_string()),
                recalled_memories: recalled,
                signals_fired: fired,
                reply_text: reply,
                ..Default::default()
            },
            &added,
            &seen,
        );
    }

    /// Tell the user what the Principal asked (decision D84), in chat's own words. The turn
    /// runs unseen and is shown only when chat has something to say, not `SKIP`. Returns the
    /// stimulus when an user's message stopped the turn.
    async fn convey(&mut self, agent: &mut Agent, stimulus: Stimulus) -> Option<Stimulus> {
        let cause = self.bus.publish(
            PulseSource::module(crate::principal::PRINCIPAL_MODULE),
            None,
            PulsePayload::Delegated { delivery: Delivery::Prompt, text: stimulus.headline.clone() },
        );
        *self.cause.lock().unwrap() = Some(cause.clone());
        agent.config.system_prompt = self.system_prompt();
        let before = agent.messages.clone();
        let note = Message::user_text(stimulus_note(&stimulus));
        let (added, seen) = self.run_turn(agent, vec![note], &cause, false).await;
        if self.preempted.load(Ordering::SeqCst) {
            agent.messages = before;
            return Some(stimulus);
        }
        trim_history(&mut agent.messages, self.history);
        self.keep(&agent.messages);
        let reply = added
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant(a) if a.stop_reason != StopReason::Error => Some(a.text()),
                _ => None,
            })
            .unwrap_or_default();
        let skipped = reply.trim().is_empty()
            || reply.trim().trim_matches(|c: char| !c.is_alphanumeric()).eq_ignore_ascii_case("SKIP");
        self.log_turn(
            TurnRecord {
                origin: "principalStimulus".into(),
                stimulus: Some(stimulus.clone()),
                reply_text: reply.clone(),
                skipped,
                ..Default::default()
            },
            &added,
            &seen,
        );
        if skipped {
            return None;
        }
        for event in seen {
            self.inspector.record_module_event(CHAT_MODULE, &event, Some(&cause));
            let _ = self.out.send(ModuleEvent::Chat { event });
        }
        let pulse = self.bus.publish(
            PulseSource::module(CHAT_MODULE),
            Some(cause),
            PulsePayload::Reply { text: reply.clone() },
        );
        let _ = self.out.send(ModuleEvent::Replied { text: reply, pulse, signalled: false, stimulus: true });
        None
    }

    /// Append a turn's record to `chat/turns.jsonl`, its steps read from `seen`, scrubbed.
    fn log_turn(&self, mut record: TurnRecord, added: &[Message], seen: &[AgentEvent]) {
        record.turn_id = format!("c-{}", &uuid::Uuid::now_v7().simple().to_string()[16..]);
        record.at = chrono::Utc::now().to_rfc3339();
        let mut started: HashMap<String, Value> = HashMap::new();
        for event in seen {
            match event {
                AgentEvent::ToolExecutionStart { call_id, arguments, .. } => {
                    started.insert(call_id.clone(), arguments.clone());
                }
                AgentEvent::ToolExecutionEnd { call_id, tool_name, output, duration_ms } => {
                    let text = nypi_ai::text_of(&output.content);
                    record.tool_calls.push(ToolStep {
                        name: tool_name.clone(),
                        arguments: started.remove(call_id).unwrap_or(Value::Null),
                        duration_ms: duration_ms.unwrap_or(0),
                        timed_out: text.contains("Read timed out"),
                        output_excerpt: clip(&text, LOGGED_OUTPUT_CHARS),
                        is_error: output.is_error,
                    });
                }
                _ => {}
            }
        }
        let mut thinking = Vec::new();
        for message in added {
            if let Message::Assistant(reply) = message {
                record.tokens += reply.usage.input + reply.usage.output;
                record.cost += reply.usage.cost.total;
                thinking.extend(reply.content.iter().filter_map(|block| match block {
                    ContentBlock::Thinking { thinking, .. } => Some(thinking.clone()),
                    _ => None,
                }));
            }
        }
        record.thinking = thinking.join("\n\n");
        let line = serde_json::to_string(&record).unwrap_or_default();
        let clean: Value = serde_json::from_str(&self.scrubber.scrub(&line)).unwrap_or(Value::Null);
        let _ = crate::jsonl::append(&self.paths.chat_turns(), &clean);
    }
}

/// What chat is asked to tell the user, as its turn begins.
fn stimulus_note(stimulus: &Stimulus) -> String {
    let facts: Vec<String> = stimulus.must_convey.iter().map(|fact| format!("- {fact}")).collect();
    let ask = stimulus.ask_user.as_ref().map(|ask| format!("\nAsk the user: {ask}")).unwrap_or_default();
    let note = stimulus
        .vault_ref
        .as_ref()
        .map(|note| format!("\nThe details are in the vault at {note}; name it."))
        .unwrap_or_default();
    format!(
        "[The Principal asks you to tell the user something ({}, activation {}). This is not the user speaking.]\n\
         {}\nWhat they must hear:\n{}{ask}{note}\n\nTell them now in Lucia's voice, briefly and naturally, keeping every \
         fact, number, option, and [tN] citation above. If the conversation has already told them all of it, reply with \
         exactly SKIP.",
        stimulus.kind,
        stimulus.activation,
        stimulus.headline,
        facts.join("\n"),
    )
}

/// What chat read in a turn, in brief, for the Principal.
fn gathered(seen: &[AgentEvent]) -> String {
    let lines: Vec<String> = seen
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolExecutionEnd { tool_name, output, .. } if tool_name != "signal_principal" => {
                Some(format!("- {tool_name}: {}", clip(&nypi_ai::text_of(&output.content), GATHERED_CHARS)))
            }
            _ => None,
        })
        .collect();
    lines.join("\n")
}

/// `signal_principal`: chat's one upward tool (decision D84).
struct SignalPrincipal {
    live: Arc<Live>,
}

#[async_trait]
impl Tool for SignalPrincipal {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "signal_principal".into(),
            description: "Hand the user's request to the Principal, the part of Lucia that plans, decides, and acts: \
                          anything that needs a task, a change to notes, goals, or schedules, a decision, or deep \
                          analysis. It gets the conversation, your reply included, once your reply is out. Put the \
                          user's goal, constraints, and any open possibilities in `topic`."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["topic"],
                "properties": {
                    "topic": { "type": "string", "description": "What the user wants: the goal, its constraints, and the open possibilities" },
                    "urgency": {
                        "type": "string",
                        "enum": ["normal", "interrupt"],
                        "description": "normal (the default) already runs ahead of background work and joins a user request that is under way; interrupt stops a user request under way, for when the user cancels, says stop or wait, or changes direction"
                    }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let Some(topic) = args["topic"].as_str().map(str::trim).filter(|topic| !topic.is_empty()) else {
            return Ok(ToolOutput::error("`topic` must say what the user wants"));
        };
        let urgency = if args["urgency"] == "interrupt" { "interrupt" } else { "normal" };
        let activation = crate::principal::new_activation_id();
        self.live.signals.lock().unwrap().push(Signal {
            activation: activation.clone(),
            topic: topic.to_string(),
            urgency: urgency.to_string(),
        });
        Ok(ToolOutput::text(format!(
            "Handed to the Principal as activation {activation}; it starts as soon as your reply is out. Tell the user \
             in a sentence what will happen, and promise no results."
        ))
        .with_details(json!({ "activation": activation, "topic": topic, "urgency": urgency })))
    }
}

/// The conversation as the Principal is handed it (decisions D82 and D84): the last messages
/// the user and chat exchanged, verbatim, newest last. Notes of the harness's (a task result,
/// a stimulus) and chat's `SKIP` are left out; so are memories recalled for a message.
fn conversation_slice(messages: &[Message]) -> Option<String> {
    let said: Vec<String> = messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => {
                // The first block is what was said; the valve adds recalled memories after it.
                let text = user.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(text.trim().to_string()),
                    _ => None,
                })?;
                let note = ["[task result", "[A task settled", "[The Principal asks you"]
                    .iter()
                    .any(|prefix| text.starts_with(prefix));
                (!note && !text.is_empty()).then(|| format!("User: {}", clip(&text, CONVERSATION_MESSAGE_CHARS)))
            }
            Message::Assistant(reply) => {
                let text = reply.text();
                let text = text.trim();
                let skip = text.trim_matches(|c: char| !c.is_alphanumeric()).eq_ignore_ascii_case("SKIP");
                (!text.is_empty() && !skip).then(|| format!("You: {}", clip(text, CONVERSATION_MESSAGE_CHARS)))
            }
            _ => None,
        })
        .collect();
    let mut kept: Vec<String> = Vec::new();
    let mut size = 0;
    for line in said.iter().rev().take(CONVERSATION_MESSAGES) {
        if size + line.len() > CONVERSATION_CHARS && !kept.is_empty() {
            break;
        }
        size += line.len();
        kept.push(line.clone());
    }
    kept.reverse();
    (!kept.is_empty()).then(|| kept.join("\n\n"))
}

fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_string();
    }
    format!("{}… [cut]", text.chars().take(chars).collect::<String>())
}

/// How the prompts name the user: "Ana's", or "the user's" without a name.
fn user_ref(settings: &Settings) -> String {
    match &settings.user_name {
        Some(name) => format!("{name}'s"),
        None => "the user's".to_string(),
    }
}

fn base_prompt(settings: &Settings, self_prompt: &str) -> String {
    let mut prompt = CHAT_PROMPT
        .replace("{{agent_name}}", &settings.agent_name)
        .replace("{{user_ref}}", &user_ref(settings))
        .trim()
        .to_string();
    if !self_prompt.trim().is_empty() {
        prompt.push_str(&format!("\n\n<self>\n{}\n</self>", self_prompt.trim()));
    }
    prompt.push_str(&format!("\n\nToday is {}.", chrono::Local::now().format("%Y-%m-%d %A")));
    prompt
}

fn result_note(pulse: crate::pulse::Pulse) -> Option<Message> {
    let PulsePayload::RunSettled { status, answer } = pulse.payload else { return None };
    let mut answer: String = answer.trim().to_string();
    if answer.chars().count() > RESULT_CONTEXT_CHARS {
        answer =
            format!("{}… [cut; the user saw all of it]", answer.chars().take(RESULT_CONTEXT_CHARS).collect::<String>());
    }
    Some(Message::user_text(format!(
        "[task result, already shown to the user · {}: {}]\n{answer}",
        status.kind.label(),
        status.text
    )))
}

impl Module {
    /// Write chat's history for the worker's current session. It is bounded by `historyMessages`,
    /// so the whole file is written each time, through a draft, never half.
    fn keep(&self, messages: &[Message]) {
        let path = history_file(&self.history_dir, &self.worker.session_id());
        let draft = path.with_extension("json.tmp");
        let written = std::fs::create_dir_all(&self.history_dir)
            .and_then(|()| serde_json::to_vec(messages).map_err(std::io::Error::other))
            .and_then(|bytes| std::fs::write(&draft, bytes))
            .and_then(|()| std::fs::rename(&draft, &path));
        if let Err(error) = written {
            self.inspector.record(CHAT_MODULE, json!({ "historyNotKept": error.to_string() }));
        }
    }
}

/// The file chat's history for `session` is kept in. A session id is NyPi's own, but only
/// letters, digits, and `-` are let through to a file name all the same.
fn history_file(dir: &std::path::Path, session: &str) -> std::path::PathBuf {
    let name: String = session.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    dir.join(format!("{name}.json"))
}

/// What chat said in `session` before, when it was kept; nothing when there is none or it does
/// not read.
fn load_history(dir: &std::path::Path, session: &str) -> Vec<Message> {
    std::fs::read(history_file(dir, session))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Keep the last `limit` messages, starting at a user message so tool calls keep their results.
fn trim_history(messages: &mut Vec<Message>, limit: usize) {
    if messages.len() <= limit {
        return;
    }
    let mut start = messages.len() - limit;
    while start < messages.len() && !matches!(messages[start], Message::User(_)) {
        start += 1;
    }
    messages.drain(..start);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimming_keeps_tool_calls_with_their_results() {
        let mut messages = vec![
            Message::user_text("a"),
            Message::user_text("b"),
            Message::Assistant(nypi_ai::AssistantMessage::empty("api", "p", "m")),
            Message::user_text("c"),
        ];
        trim_history(&mut messages, 2);
        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0], Message::User(u) if nypi_ai::text_of(&u.content) == "c"));
    }
}
