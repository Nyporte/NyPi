//! A process's link to the board service (decisions D56 and D80): while `nypi board` runs, a
//! NyPi process holds one connection to `~/.nypi/run/board.sock`, and every runtime in it (the
//! hub's conversation, a task beside it, a job) reports on that one connection, each on a
//! channel of its own: what happens in it goes up, and the owner's actions for it come back.
//! Watching must never slow a run: events go through a bounded queue only while connected, a
//! full queue drops and counts them, and reply text is sent only while someone watches.
//! Without a board the link costs one look for the socket every 30 seconds.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use nypi_agent::{AgentEvent, AgentHandle};
use nypi_ai::{Message, StreamEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::inspector::{ApprovalEvent, Inspector};
use crate::pulse::{Pulse, PulseTap};
use crate::runtime::{InputHandle, RuntimeEvent};

/// How long the link waits before looking for the board again.
const RECONNECT: Duration = Duration::from_secs(30);
/// Events the link holds for a slow board before it drops them.
const QUEUE: usize = 4096;
/// Kinds of reply delta, so an unwatched link sends one of each in a row.
const DELTA_OTHER: u8 = 0;
const DELTA_THINKING: u8 = 1;
const DELTA_TEXT: u8 = 2;
const DELTA_TOOL: u8 = 3;

/// What a process tells the board.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum LinkUp {
    Hello {
        pid: u32,
        front_end: String,
        agent: String,
        model: String,
        context_window: u64,
        cwd: String,
        session_id: String,
        task: Option<LinkTask>,
    },
    /// A runtime event as front ends get it (`RuntimeEvent` JSON).
    Event {
        event: Value,
    },
    /// A pulse, as the Inspector records it.
    Pulse {
        pulse: Value,
    },
    /// The run's task started (Some) or ended (None).
    Task {
        task: Option<LinkTask>,
    },
    Model {
        model: String,
        context_window: u64,
    },
    Session {
        session_id: String,
    },
    ApprovalWaiting {
        id: String,
        tool: String,
        summary: String,
        reason: String,
    },
    ApprovalClosed {
        id: String,
    },
    /// What chat or the Principal is doing this moment (decision D88): its step, and the end
    /// of the text it is writing. `idle` when the turn or activation ended.
    Plane {
        plane: String,
        id: String,
        step: String,
        tail: String,
    },
    /// Events dropped so far because the board fell behind.
    Dropped {
        count: u64,
    },
    /// The runtime on this channel is gone; the process, and its other channels, go on.
    Bye,
}

/// A frame on the connection: a message, and the channel (one runtime of the process) it is
/// from or for. A frame without a channel is the whole process's, as before channels.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Framed<T> {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ch: String,
    #[serde(flatten)]
    pub message: T,
}

/// The process's one connection to a board, shared by its runtimes.
struct Mux {
    socket: PathBuf,
    links: Mutex<HashMap<String, Weak<BoardLink>>>,
    out: mpsc::Sender<(String, LinkUp)>,
    connected: AtomicBool,
    next: AtomicU64,
}

/// The connection for each board socket this process reports to (one, outside tests).
fn muxes() -> &'static Mutex<HashMap<PathBuf, Arc<Mux>>> {
    static MUXES: OnceLock<Mutex<HashMap<PathBuf, Arc<Mux>>>> = OnceLock::new();
    MUXES.get_or_init(Mutex::default)
}

impl Mux {
    /// The connection to the board at `socket`, started when the first runtime asks for it.
    fn of(socket: &PathBuf, runtime: &tokio::runtime::Handle) -> Arc<Mux> {
        let mut all = muxes().lock().unwrap();
        if let Some(mux) = all.get(socket) {
            return mux.clone();
        }
        let (out, queue) = mpsc::channel(QUEUE);
        let mux = Arc::new(Mux {
            socket: socket.clone(),
            links: Mutex::default(),
            out,
            connected: AtomicBool::new(false),
            next: AtomicU64::new(0),
        });
        all.insert(socket.clone(), mux.clone());
        runtime.spawn(keep_linked(mux.clone(), queue));
        mux
    }

    fn live(&self) -> Vec<(String, Arc<BoardLink>)> {
        let mut links = self.links.lock().unwrap();
        links.retain(|_, link| link.strong_count() > 0);
        links.iter().filter_map(|(channel, link)| Some((channel.clone(), link.upgrade()?))).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkTask {
    pub id: String,
    pub goal: String,
    /// The pulse that started it, which everything it causes names.
    pub pulse: Option<String>,
}

/// What the board asks a process to do.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum LinkDown {
    /// Send reply text as it is written, or stop.
    Watch {
        deltas: bool,
    },
    Answer {
        id: String,
        approved: bool,
        reason: Option<String>,
    },
    Stop,
    /// Steer the running run with the owner's message.
    Message {
        text: String,
    },
}

/// What the link reports in its hello, kept current so a reconnect starts from the truth.
struct Identity {
    front_end: String,
    agent: String,
    model: String,
    context_window: u64,
    cwd: String,
    session_id: String,
    task: Option<LinkTask>,
}

pub struct BoardLink {
    /// The process's connection, and this runtime's channel on it. None outside a Tokio runtime.
    mux: Option<Arc<Mux>>,
    channel: String,
    watching: AtomicBool,
    dropped: AtomicU64,
    last_delta: AtomicU8,
    identity: Mutex<Identity>,
    agent: AgentHandle,
    input: Mutex<InputHandle>,
    inspector: Arc<Inspector>,
}

/// The parts of a runtime a link reports and acts through.
pub struct LinkParts {
    pub socket: PathBuf,
    pub agent_name: String,
    pub model: String,
    pub context_window: u64,
    pub cwd: String,
    pub session_id: String,
    pub agent: AgentHandle,
    pub input: InputHandle,
    pub inspector: Arc<Inspector>,
}

impl BoardLink {
    /// Start looking for the board in the background. Outside a Tokio runtime the link stays idle.
    pub fn start(parts: LinkParts) -> Arc<BoardLink> {
        let mux = tokio::runtime::Handle::try_current().ok().map(|runtime| Mux::of(&parts.socket, &runtime));
        let channel = mux.as_ref().map(|mux| mux.next.fetch_add(1, Ordering::Relaxed) + 1).unwrap_or_default();
        let link = Arc::new(BoardLink {
            mux,
            channel: channel.to_string(),
            watching: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            last_delta: AtomicU8::new(DELTA_OTHER),
            identity: Mutex::new(Identity {
                front_end: parts.input.channel().to_string(),
                agent: parts.agent_name,
                model: parts.model,
                context_window: parts.context_window,
                cwd: parts.cwd,
                session_id: parts.session_id,
                task: None,
            }),
            agent: parts.agent,
            input: Mutex::new(parts.input),
            inspector: parts.inspector.clone(),
        });
        if let Some(mux) = &link.mux {
            mux.links.lock().unwrap().insert(link.channel.clone(), Arc::downgrade(&link));
            // A board that is there already hears of this runtime now, not at its next connect.
            if mux.connected.load(Ordering::Relaxed) {
                link.announce();
            }
        }
        link
    }

    /// Whether a board is connected now.
    pub fn connected(&self) -> bool {
        self.mux.as_ref().is_some_and(|mux| mux.connected.load(Ordering::Relaxed))
    }

    fn send(&self, message: LinkUp) {
        let Some(mux) = self.mux.as_ref().filter(|mux| mux.connected.load(Ordering::Relaxed)) else { return };
        if mux.out.try_send((self.channel.clone(), message)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Say who this runtime is and what waits in it, and have its approvals follow: at each
    /// connect, and when a runtime starts while a board is connected. Approvals wait for the
    /// board only while it is connected, so a run with no one to ask is not kept waiting.
    fn announce(self: &Arc<Self>) {
        let Some(mux) = self.mux.clone() else { return };
        let (approvals, mut events) = mpsc::unbounded_channel();
        self.inspector.set_approval_listener(Some(approvals));
        self.send(self.hello());
        for waiting in self.inspector.waiting_approvals() {
            self.send(approval_message(waiting));
        }
        let channel = self.channel.clone();
        tokio::spawn(async move {
            // Through the same queue as events: an approval must not overtake its task.
            while let Some(event) = events.recv().await {
                if mux.out.send((channel.clone(), approval_message(event))).await.is_err() {
                    break;
                }
            }
        });
    }

    /// A runtime event, if the board wants it. Unwatched, a reply's deltas are reduced to the
    /// first of each kind (thinking, text, tool call), which is all the step needs.
    pub fn event(&self, event: &RuntimeEvent) {
        if !self.connected() {
            return;
        }
        let watching = self.watching.load(Ordering::Relaxed);
        match event {
            RuntimeEvent::Agent { event: AgentEvent::MessageUpdate { event: delta } } => {
                let kind = match delta {
                    StreamEvent::ThinkingDelta { .. } => DELTA_THINKING,
                    StreamEvent::TextDelta { .. } => DELTA_TEXT,
                    StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallDelta { .. } => DELTA_TOOL,
                    _ => DELTA_OTHER,
                };
                let before = self.last_delta.swap(kind, Ordering::Relaxed);
                if !watching && (kind == before || kind == DELTA_OTHER) {
                    return;
                }
            }
            RuntimeEvent::Agent { event: AgentEvent::MessageStart { .. } } => {
                self.last_delta.store(DELTA_OTHER, Ordering::Relaxed);
            }
            RuntimeEvent::Agent { event: AgentEvent::ToolExecutionUpdate { .. } } if !watching => return,
            // The run's messages were each sent as they ended.
            RuntimeEvent::Agent { event: AgentEvent::AgentEnd { .. } } => {
                return self.send(LinkUp::Event {
                    event: serde_json::json!({ "kind": "agent", "event": { "type": "agent_end" } }),
                });
            }
            _ => {}
        }
        if let Ok(event) = serde_json::to_value(event) {
            self.send(LinkUp::Event { event });
        }
    }

    /// What a plane is doing this moment; nothing while no board is connected.
    pub fn plane(&self, plane: &str, id: &str, step: &str, tail: &str) {
        self.send(LinkUp::Plane { plane: plane.into(), id: id.into(), step: step.into(), tail: tail.into() });
    }

    pub fn task(&self, task: Option<LinkTask>) {
        self.identity.lock().unwrap().task = task.clone();
        self.send(LinkUp::Task { task });
    }

    pub fn model(&self, model: &str, context_window: u64) {
        {
            let mut identity = self.identity.lock().unwrap();
            identity.model = model.to_string();
            identity.context_window = context_window;
        }
        self.send(LinkUp::Model { model: model.to_string(), context_window });
    }

    pub fn session(&self, session_id: &str) {
        self.identity.lock().unwrap().session_id = session_id.to_string();
        self.send(LinkUp::Session { session_id: session_id.to_string() });
    }

    /// The front end changed its name (`Runtime::set_channel`); steering follows it, and a
    /// board that already heard the old name hears the new one.
    pub fn front_end(&self, input: InputHandle) {
        self.identity.lock().unwrap().front_end = input.channel().to_string();
        *self.input.lock().unwrap() = input;
        self.send(self.hello());
    }

    fn hello(&self) -> LinkUp {
        let identity = self.identity.lock().unwrap();
        LinkUp::Hello {
            pid: std::process::id(),
            front_end: identity.front_end.clone(),
            agent: identity.agent.clone(),
            model: identity.model.clone(),
            context_window: identity.context_window,
            cwd: identity.cwd.clone(),
            session_id: identity.session_id.clone(),
            task: identity.task.clone(),
        }
    }

    fn act(&self, command: LinkDown) {
        match command {
            LinkDown::Watch { deltas } => self.watching.store(deltas, Ordering::Relaxed),
            LinkDown::Answer { id, approved, reason } => {
                self.inspector
                    .record("board", serde_json::json!({ "approval": id, "approved": approved, "reason": &reason }));
                self.inspector.answer(&id, approved, reason);
            }
            LinkDown::Stop => {
                self.inspector.record("board", serde_json::json!({ "stop": true }));
                self.agent.abort();
            }
            LinkDown::Message { text } => {
                self.inspector.record("board", serde_json::json!({ "steer": &text }));
                self.input.lock().unwrap().steer(Message::user_text(text));
            }
        }
    }
}

impl Drop for BoardLink {
    fn drop(&mut self) {
        // The process goes on without this runtime: the board takes its card away.
        self.send(LinkUp::Bye);
        if let Some(mux) = &self.mux {
            mux.links.lock().unwrap().remove(&self.channel);
        }
    }
}

impl PulseTap for BoardLink {
    fn observe(&self, pulse: &Pulse) {
        if self.connected()
            && let Ok(pulse) = serde_json::to_value(pulse)
        {
            self.send(LinkUp::Pulse { pulse });
        }
    }
}

fn approval_message(event: ApprovalEvent) -> LinkUp {
    match event {
        ApprovalEvent::Waiting { id, tool, summary, reason } => LinkUp::ApprovalWaiting { id, tool, summary, reason },
        ApprovalEvent::Closed { id } => LinkUp::ApprovalClosed { id },
    }
}

/// Connect whenever a board listens, for as long as the process has a runtime to report.
async fn keep_linked(mux: Arc<Mux>, mut queue: mpsc::Receiver<(String, LinkUp)>) {
    loop {
        if mux.live().is_empty() {
            // Nothing left to report: the connection is given up, and made again on demand.
            let mut all = muxes().lock().unwrap();
            if mux.links.lock().unwrap().values().all(|link| link.strong_count() == 0) {
                all.remove(&mux.socket);
                return;
            }
        }
        if let Ok(stream) = UnixStream::connect(&mux.socket).await {
            linked(&mux, stream, &mut queue).await;
        }
        tokio::time::sleep(RECONNECT).await;
    }
}

async fn linked(mux: &Arc<Mux>, stream: UnixStream, queue: &mut mpsc::Receiver<(String, LinkUp)>) {
    let (read, mut write) = stream.into_split();
    // Whatever waited from an earlier connection is stale: each hello carries the current state.
    while queue.try_recv().is_ok() {}
    mux.connected.store(true, Ordering::Relaxed);
    for (_, link) in mux.live() {
        link.announce();
    }
    let mut reported_drops: HashMap<String, u64> = HashMap::new();
    let mut lines = BufReader::new(read).lines();
    loop {
        tokio::select! {
            biased;
            framed = queue.recv() => {
                let Some((ch, message)) = framed else { break };
                if write_line(&mut write, &Framed { ch: ch.clone(), message }).await.is_err() {
                    break;
                }
                let link = mux.links.lock().unwrap().get(&ch).and_then(Weak::upgrade);
                let dropped = link.map_or(0, |link| link.dropped.load(Ordering::Relaxed));
                if dropped != reported_drops.get(&ch).copied().unwrap_or(0) {
                    reported_drops.insert(ch.clone(), dropped);
                    let _ = write_line(&mut write, &Framed { ch, message: LinkUp::Dropped { count: dropped } }).await;
                }
            }
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let Ok(framed) = serde_json::from_str::<Framed<LinkDown>>(&line) else { continue };
                let link = mux.links.lock().unwrap().get(&framed.ch).and_then(Weak::upgrade);
                if let Some(link) = link {
                    link.act(framed.message);
                }
            }
        }
    }
    mux.connected.store(false, Ordering::Relaxed);
    for (_, link) in mux.live() {
        link.watching.store(false, Ordering::Relaxed);
        link.inspector.set_approval_listener(None);
    }
}

async fn write_line(write: &mut tokio::net::unix::OwnedWriteHalf, framed: &Framed<LinkUp>) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(framed).map_err(std::io::Error::other)?;
    line.push(b'\n');
    write.write_all(&line).await
}

/// How much of the text being written a plane reports.
const PLANE_TAIL: usize = 600;
/// How often a plane's growing text is reported, at most.
const PLANE_EVERY: Duration = Duration::from_millis(250);
/// Tools that only look: a plane "reads" with them and "calls" the rest.
const LOOKS: [&str; 11] = [
    "read",
    "grep",
    "find",
    "ls",
    "vault_read",
    "vault_search",
    "vault_list",
    "tasks_inspect",
    "task_status",
    "inspect_task_evidence",
    "memory",
];

/// Follows a plane's agent events and tells the board what it is doing (decision D88): a
/// change of step at once, the text being written at most four times a second, nothing while
/// no board is connected.
pub struct PlaneReporter {
    link: Option<Arc<BoardLink>>,
    plane: &'static str,
    id: String,
    step: String,
    tail: String,
    sent: Option<std::time::Instant>,
}

impl PlaneReporter {
    pub fn new(link: Option<Arc<BoardLink>>, plane: &'static str, id: impl Into<String>) -> PlaneReporter {
        let link = link.filter(|link| link.mux.is_some());
        PlaneReporter { link, plane, id: id.into(), step: String::new(), tail: String::new(), sent: None }
    }

    pub fn observe(&mut self, event: &AgentEvent) {
        let Some(link) = self.link.clone() else { return };
        if !link.connected() {
            return;
        }
        let step = match event {
            AgentEvent::MessageStart { .. } => {
                self.tail.clear();
                "Thinking".to_string()
            }
            AgentEvent::MessageUpdate { event: StreamEvent::ThinkingDelta { .. } } => "Thinking".into(),
            AgentEvent::MessageUpdate { event: StreamEvent::TextDelta { delta, .. } } => {
                self.tail.push_str(delta);
                let chars = self.tail.chars().count();
                if chars > PLANE_TAIL {
                    self.tail = self.tail.chars().skip(chars - PLANE_TAIL).collect();
                }
                "Writing".into()
            }
            AgentEvent::MessageUpdate { event: StreamEvent::ToolCallStart { name, .. } }
            | AgentEvent::ToolExecutionStart { tool_name: name, .. } => {
                if LOOKS.contains(&name.as_str()) {
                    format!("Reading {name}")
                } else {
                    format!("Calling {name}")
                }
            }
            AgentEvent::ToolExecutionEnd { .. } => "Thinking".into(),
            _ => return,
        };
        let changed = step != self.step;
        let due = self.sent.is_none_or(|sent| sent.elapsed() >= PLANE_EVERY);
        if changed || (step == "Writing" && due) {
            self.step = step;
            self.sent = Some(std::time::Instant::now());
            link.plane(self.plane, &self.id, &self.step, &self.tail);
        }
    }

    /// The turn or activation ended.
    pub fn finish(&mut self) {
        if let Some(link) = &self.link {
            link.plane(self.plane, &self.id, "idle", "");
        }
        self.step.clear();
        self.tail.clear();
    }
}
