//! The pulse bus: typed, causation-linked messages between Lucia's modules (docs/architecture.md
//! §4.1, principles A4 and A5). A pulse first goes to the taps, synchronously and in publish order
//! (the Inspector writes it to the audit log, so nothing is lost or reordered there), then to the
//! inbox of every subscriber that wants its kind. Inboxes deliver by priority, so the user's pulses
//! overtake queued internal work. Priority follows from the source, so no module can claim the
//! user's.
//!
//! A hub has one bus for everything in it (decision D67, part 3). Each thread works through a
//! handle scoped to it (`scoped`): what it publishes carries the thread's name, and its
//! subscribers and taps get that thread's pulses only, so one conversation's modules never hear
//! another's. The hub itself works through the root handle: what it publishes belongs to no
//! thread, and what it subscribes to is every pulse of every thread. One sequence numbers them
//! all, so a pulse in one thread can name its cause in another or in the hub.

use std::collections::BinaryHeap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::inspector::Alert;
use crate::status::RunStatus;

/// Unique across processes writing the same audit log: a random bus prefix plus a sequence.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PulseId(pub String);

impl fmt::Display for PulseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum PulseSource {
    /// A message from the user; `channel` is the front end (cli, print, telegram, rpc).
    UserInput {
        channel: String,
    },
    UserTyping {
        channel: String,
    },
    Timer {
        schedule: String,
    },
    Condition {
        condition: String,
    },
    TaskEvent {
        task: String,
    },
    Dmn,
    Module {
        module: String,
    },
}

impl PulseSource {
    pub fn user(channel: impl Into<String>) -> Self {
        PulseSource::UserInput { channel: channel.into() }
    }

    pub fn module(module: impl Into<String>) -> Self {
        PulseSource::Module { module: module.into() }
    }

    pub fn priority(&self) -> Priority {
        match self {
            PulseSource::UserInput { .. } | PulseSource::UserTyping { .. } => Priority::User,
            PulseSource::Dmn => Priority::Background,
            _ => Priority::Normal,
        }
    }
}

/// Delivery order within an inbox; higher first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Background,
    Normal,
    User,
}

/// How a user message enters the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Delivery {
    /// Starts a run.
    Prompt,
    /// Injected into the running turn.
    Steer,
    /// Queued until the run would stop.
    FollowUp,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum PulsePayload {
    UserMessage {
        delivery: Delivery,
        text: String,
    },
    /// An instruction from one module to the worker (chat starting or steering a task).
    Delegated {
        delivery: Delivery,
        text: String,
    },
    /// What a conversational module said to the user.
    Reply {
        text: String,
    },
    ToolStarted {
        call_id: String,
        tool: String,
    },
    ToolFinished {
        call_id: String,
        tool: String,
        is_error: bool,
        duration_ms: Option<u64>,
    },
    RunSettled {
        status: RunStatus,
        answer: String,
    },
    Alert {
        alert: Alert,
    },
    /// The memory valve's decision on a message that matched memories.
    MemoryValve {
        decision: crate::valve::ValveDecision,
    },
    /// The user paused while typing. The draft is not serialized: text typed and deleted never
    /// reaches the audit log, only its length does.
    Typing {
        #[serde(skip)]
        draft: String,
        chars: usize,
    },
    /// Memories that bear on what the user is typing, recalled before they send it.
    MemoryHint {
        memories: Vec<crate::valve::Surfaced>,
    },
    /// A timer or condition fired (its source says which).
    Triggered {
        schedule: String,
        text: String,
    },
    /// The Thinking module's deeper look at a user message (its causation).
    Thought {
        thought: crate::thinking::Thought,
    },
    /// A turn of a run ended: what its tool calls were and how they came out, for whoever
    /// watches the run's course (the Thinking observer, decision D78).
    TurnCompleted {
        task: Option<String>,
        turn_index: u32,
        recent_calls_digest: Vec<String>,
        /// The context was compacted since the turn before.
        compacted: bool,
    },
    /// A specialist that was working in the background has finished (its source names its
    /// task). `text` is the report as the delegating agent is to read it.
    Reported {
        delegation: String,
        specialist: String,
        status: RunStatus,
        text: String,
    },
}

/// What subscribers filter on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PulseKind {
    UserMessage,
    Delegated,
    Reply,
    ToolStarted,
    ToolFinished,
    RunSettled,
    Alert,
    MemoryValve,
    Typing,
    MemoryHint,
    Triggered,
    Thought,
    TurnCompleted,
    Reported,
}

impl PulsePayload {
    pub fn kind(&self) -> PulseKind {
        match self {
            PulsePayload::UserMessage { .. } => PulseKind::UserMessage,
            PulsePayload::Delegated { .. } => PulseKind::Delegated,
            PulsePayload::Reply { .. } => PulseKind::Reply,
            PulsePayload::ToolStarted { .. } => PulseKind::ToolStarted,
            PulsePayload::ToolFinished { .. } => PulseKind::ToolFinished,
            PulsePayload::RunSettled { .. } => PulseKind::RunSettled,
            PulsePayload::Alert { .. } => PulseKind::Alert,
            PulsePayload::MemoryValve { .. } => PulseKind::MemoryValve,
            PulsePayload::Typing { .. } => PulseKind::Typing,
            PulsePayload::MemoryHint { .. } => PulseKind::MemoryHint,
            PulsePayload::Triggered { .. } => PulseKind::Triggered,
            PulsePayload::Thought { .. } => PulseKind::Thought,
            PulsePayload::TurnCompleted { .. } => PulseKind::TurnCompleted,
            PulsePayload::Reported { .. } => PulseKind::Reported,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pulse {
    pub id: PulseId,
    /// The thread it was published in; None for the hub's own (a timer firing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    pub source: PulseSource,
    pub priority: Priority,
    pub at: String,
    /// The pulse that led to this one; None for roots such as user input.
    pub causation: Option<PulseId>,
    pub payload: PulsePayload,
}

/// Sees every pulse before any subscriber, in publish order. Must not publish (the bus holds its
/// publish lock while calling taps).
pub trait PulseTap: Send + Sync {
    fn observe(&self, pulse: &Pulse);
}

/// Pulses a subscriber may hold before the oldest low-priority one is dropped. User pulses are never dropped.
pub const DEFAULT_INBOX_CAPACITY: usize = 1024;

/// A thread on the bus: a handle's own name, compared by identity. None is the hub.
type Scope = Option<Arc<str>>;
/// A tap with the scope it was registered in.
type ScopedTap = (Scope, Arc<dyn PulseTap>);

/// A handle on a bus: the root handle, or one scoped to a thread.
#[derive(Clone)]
pub struct PulseBus {
    inner: Arc<BusInner>,
    /// The thread this handle publishes and listens in; None for the root handle.
    thread: Option<Arc<str>>,
}

struct BusInner {
    prefix: String,
    seq: AtomicU64,
    publish: Mutex<()>,
    taps: Mutex<Vec<ScopedTap>>,
    inboxes: Mutex<Vec<Weak<Inbox>>>,
}

impl Drop for BusInner {
    fn drop(&mut self) {
        for inbox in self.inboxes.get_mut().unwrap().iter().filter_map(Weak::upgrade) {
            inbox.state.lock().unwrap().closed = true;
            inbox.notify.notify_one();
        }
    }
}

impl Default for PulseBus {
    fn default() -> Self {
        Self::new()
    }
}

impl PulseBus {
    pub fn new() -> Self {
        let prefix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        PulseBus {
            inner: Arc::new(BusInner {
                prefix,
                seq: AtomicU64::new(1),
                publish: Mutex::new(()),
                taps: Mutex::new(Vec::new()),
                inboxes: Mutex::new(Vec::new()),
            }),
            thread: None,
        }
    }

    /// A handle on the same bus for a new thread named `thread`. Every call makes a thread of
    /// its own, even under a name already in use (a conversation closed and opened again is a
    /// new thread); clones of the handle share it.
    pub fn scoped(&self, thread: &str) -> PulseBus {
        PulseBus { inner: self.inner.clone(), thread: Some(thread.into()) }
    }

    /// The root handle of the same bus: the hub's.
    pub fn root(&self) -> PulseBus {
        PulseBus { inner: self.inner.clone(), thread: None }
    }

    /// The thread this handle is scoped to.
    pub fn thread(&self) -> Option<&str> {
        self.thread.as_deref()
    }

    /// See every pulse published through a handle of this scope: a thread's own, or, on the root
    /// handle, the ones that belong to no thread. (Each thread's Inspector records its own, so
    /// one pulse is in the audit log once.)
    pub fn tap(&self, tap: Arc<dyn PulseTap>) {
        self.inner.taps.lock().unwrap().push((self.thread.clone(), tap));
    }

    /// End this handle's thread on the bus: its subscribers are told the bus is gone once they
    /// have read what is waiting, so the modules that listen there stop, and its taps are let
    /// go. On the root handle, the same for the whole bus.
    pub fn close(&self) {
        let mine = |thread: &Option<Arc<str>>| self.thread.is_none() || same_thread(thread, &self.thread);
        self.inner.taps.lock().unwrap().retain(|(thread, _)| !mine(thread));
        for inbox in self.inner.inboxes.lock().unwrap().iter().filter_map(Weak::upgrade) {
            if mine(&inbox.thread) {
                inbox.state.lock().unwrap().closed = true;
                inbox.notify.notify_one();
            }
        }
    }

    /// Receive pulses of `kinds` (all kinds when empty) published from now on: this handle's
    /// thread's, or on the root handle every thread's and the hub's own.
    pub fn subscribe(&self, name: &str, kinds: &[PulseKind]) -> Subscription {
        self.subscribe_with_capacity(name, kinds, DEFAULT_INBOX_CAPACITY)
    }

    pub fn subscribe_with_capacity(&self, name: &str, kinds: &[PulseKind], capacity: usize) -> Subscription {
        let inbox = Arc::new(Inbox {
            name: name.to_string(),
            thread: self.thread.clone(),
            kinds: kinds.to_vec(),
            capacity: capacity.max(1),
            state: Mutex::new(InboxState::default()),
            notify: Notify::new(),
        });
        self.inner.inboxes.lock().unwrap().push(Arc::downgrade(&inbox));
        Subscription { inbox }
    }

    pub fn publish(&self, source: PulseSource, causation: Option<PulseId>, payload: PulsePayload) -> PulseId {
        let _order = self.inner.publish.lock().unwrap();
        let id = PulseId(format!("{}-{}", self.inner.prefix, self.inner.seq.fetch_add(1, Ordering::SeqCst)));
        let pulse = Pulse {
            id: id.clone(),
            thread: self.thread.as_deref().map(str::to_string),
            priority: source.priority(),
            source,
            at: chrono::Utc::now().to_rfc3339(),
            causation,
            payload,
        };
        let taps = self.inner.taps.lock().unwrap().clone();
        for (thread, tap) in &taps {
            if same_thread(thread, &self.thread) {
                tap.observe(&pulse);
            }
        }
        let kind = pulse.payload.kind();
        let mut inboxes = self.inner.inboxes.lock().unwrap();
        inboxes.retain(|weak| weak.strong_count() > 0);
        for inbox in inboxes.iter().filter_map(Weak::upgrade) {
            // A thread's subscriber hears its own thread; the hub's hears everything.
            let hears = inbox.thread.is_none() || same_thread(&inbox.thread, &self.thread);
            if hears && (inbox.kinds.is_empty() || inbox.kinds.contains(&kind)) {
                inbox.push(pulse.clone());
            }
        }
        id
    }
}

/// Whether two scopes are the same thread: the same handle's, not merely the same name.
fn same_thread(a: &Option<Arc<str>>, b: &Option<Arc<str>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

struct Inbox {
    name: String,
    /// The thread it listens in; None hears every thread.
    thread: Option<Arc<str>>,
    kinds: Vec<PulseKind>,
    capacity: usize,
    state: Mutex<InboxState>,
    notify: Notify,
}

#[derive(Default)]
struct InboxState {
    queue: BinaryHeap<Queued>,
    order: u64,
    dropped: u64,
    closed: bool,
}

struct Queued {
    priority: Priority,
    order: u64,
    pulse: Pulse,
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Queued {
    /// Max-heap: higher priority first, then the earlier pulse.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority).then_with(|| other.order.cmp(&self.order))
    }
}

impl Inbox {
    fn push(&self, pulse: Pulse) {
        let mut state = self.state.lock().unwrap();
        // Its thread has ended: whoever still listens is only reading out what was waiting.
        if state.closed {
            return;
        }
        let queued = Queued { priority: pulse.priority, order: state.order, pulse };
        state.order += 1;
        if state.queue.len() >= self.capacity {
            // The stalest of the least urgent pulses goes; the user's are kept even past capacity.
            let mut items = std::mem::take(&mut state.queue).into_vec();
            let victim = items
                .iter()
                .enumerate()
                .filter(|(_, q)| q.priority < Priority::User)
                .min_by_key(|(_, q)| (q.priority, q.order))
                .map(|(index, q)| (index, q.priority));
            match victim {
                Some((index, priority)) if priority <= queued.priority => {
                    items.swap_remove(index);
                    items.push(queued);
                    state.dropped += 1;
                }
                _ if queued.priority == Priority::User => items.push(queued),
                _ => state.dropped += 1,
            }
            state.queue = BinaryHeap::from(items);
        } else {
            state.queue.push(queued);
        }
        drop(state);
        self.notify.notify_one();
    }
}

/// One subscriber's inbox. Dropping it unsubscribes.
pub struct Subscription {
    inbox: Arc<Inbox>,
}

impl Subscription {
    pub fn name(&self) -> &str {
        &self.inbox.name
    }

    /// The most urgent waiting pulse; None once the bus is gone and the inbox is empty.
    pub async fn recv(&mut self) -> Option<Pulse> {
        loop {
            if let Some(pulse) = self.try_recv() {
                return Some(pulse);
            }
            if self.inbox.state.lock().unwrap().closed {
                return None;
            }
            self.inbox.notify.notified().await;
        }
    }

    pub fn try_recv(&mut self) -> Option<Pulse> {
        self.inbox.state.lock().unwrap().queue.pop().map(|q| q.pulse)
    }

    /// Pulses discarded because the inbox was full.
    pub fn dropped(&self) -> u64 {
        self.inbox.state.lock().unwrap().dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str) -> PulsePayload {
        PulsePayload::UserMessage { delivery: Delivery::Steer, text: text.into() }
    }

    fn text(pulse: &Pulse) -> &str {
        match &pulse.payload {
            PulsePayload::UserMessage { text, .. } => text,
            _ => "",
        }
    }

    struct Recorder(Mutex<Vec<String>>);
    impl PulseTap for Recorder {
        fn observe(&self, pulse: &Pulse) {
            self.0.lock().unwrap().push(pulse.id.0.clone());
        }
    }

    #[tokio::test]
    async fn user_pulses_overtake_queued_internal_work() {
        let bus = PulseBus::new();
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        bus.tap(recorder.clone());
        let mut inbox = bus.subscribe("chat", &[]);
        let ids = [
            bus.publish(PulseSource::Dmn, None, message("reflect")),
            bus.publish(PulseSource::module("worker"), None, message("progress 1")),
            bus.publish(PulseSource::module("worker"), None, message("progress 2")),
            bus.publish(PulseSource::user("cli"), None, message("stop that")),
        ];
        let order: Vec<String> = (0..4).map(|_| inbox.try_recv().map(|p| text(&p).to_string()).unwrap()).collect();
        assert_eq!(order, ["stop that", "progress 1", "progress 2", "reflect"]);
        assert!(inbox.try_recv().is_none());
        let seen = recorder.0.lock().unwrap().clone();
        assert_eq!(seen, ids.iter().map(|id| id.0.clone()).collect::<Vec<_>>(), "taps see publish order");
    }

    #[tokio::test]
    async fn subscribers_filter_by_kind_and_keep_causation() {
        let bus = PulseBus::new();
        let mut settled = bus.subscribe("watchdog", &[PulseKind::RunSettled]);
        let root = bus.publish(PulseSource::user("cli"), None, message("go"));
        let status = crate::status::synthesize(None);
        bus.publish(
            PulseSource::module("runtime"),
            Some(root.clone()),
            PulsePayload::RunSettled { status, answer: String::new() },
        );
        let pulse = settled.recv().await.unwrap();
        assert_eq!(pulse.payload.kind(), PulseKind::RunSettled);
        assert_eq!(pulse.causation, Some(root));
        assert!(settled.try_recv().is_none());
    }

    #[tokio::test]
    async fn a_full_inbox_drops_stale_low_priority_pulses_but_never_the_users() {
        let bus = PulseBus::new();
        let mut inbox = bus.subscribe_with_capacity("slow", &[], 2);
        bus.publish(PulseSource::Dmn, None, message("old reflection"));
        bus.publish(PulseSource::module("worker"), None, message("progress"));
        bus.publish(PulseSource::module("worker"), None, message("newer progress"));
        bus.publish(PulseSource::Dmn, None, message("new reflection"));
        bus.publish(PulseSource::user("cli"), None, message("a"));
        bus.publish(PulseSource::user("cli"), None, message("b"));
        bus.publish(PulseSource::user("cli"), None, message("c"));
        let mut texts = Vec::new();
        while let Some(pulse) = inbox.try_recv() {
            texts.push(text(&pulse).to_string());
        }
        assert_eq!(texts, ["a", "b", "c"]);
        assert_eq!(inbox.dropped(), 4);
    }

    #[tokio::test]
    async fn recv_waits_and_ends_with_the_bus() {
        let bus = PulseBus::new();
        let mut inbox = bus.subscribe("chat", &[]);
        let waiter = tokio::spawn(async move {
            let first = inbox.recv().await.map(|p| text(&p).to_string());
            (first, inbox.recv().await.is_none())
        });
        tokio::task::yield_now().await;
        bus.publish(PulseSource::user("telegram"), None, message("hello"));
        drop(bus);
        let (first, ended) = waiter.await.unwrap();
        assert_eq!(first.as_deref(), Some("hello"));
        assert!(ended);
    }

    #[test]
    fn one_bus_keeps_each_threads_pulses_apart_and_the_hub_hears_them_all() {
        let bus = PulseBus::new();
        let (site, api) = (bus.scoped("site"), bus.scoped("api"));
        let recorder = || Arc::new(Recorder(Mutex::new(Vec::new())));
        let (hub_tap, site_tap) = (recorder(), recorder());
        bus.tap(hub_tap.clone());
        site.tap(site_tap.clone());
        let mut hub = bus.subscribe("hub", &[]);
        let mut in_site = site.subscribe("chat", &[]);
        let mut in_api = api.subscribe("chat", &[]);

        let fired = bus.publish(PulseSource::Timer { schedule: "s-1".into() }, None, message("backup"));
        let ran = site.publish(PulseSource::module("worker"), Some(fired.clone()), message("backing up"));
        let other = api.publish(PulseSource::user("cli"), None, message("hello"));

        // One sequence across the bus, and a cause can be in another scope.
        assert_eq!(fired.0.rsplit_once('-').unwrap().0, other.0.rsplit_once('-').unwrap().0);
        let heard: Vec<(PulseId, Option<String>)> =
            std::iter::from_fn(|| hub.try_recv()).map(|pulse| (pulse.id, pulse.thread)).collect();
        // The user's pulse overtakes the others in an inbox; all three are there.
        assert_eq!(heard.len(), 3);
        assert!(heard.contains(&(fired.clone(), None)) && heard.contains(&(ran.clone(), Some("site".into()))));
        let in_site_heard = in_site.try_recv().unwrap();
        assert_eq!((in_site_heard.id, in_site_heard.causation), (ran.clone(), Some(fired.clone())));
        assert!(in_site.try_recv().is_none(), "neither the hub's pulse nor the other thread's");
        assert_eq!(in_api.try_recv().unwrap().id, other);
        assert!(in_api.try_recv().is_none());
        // Each is recorded once: by the tap of the scope it was published in.
        assert_eq!(*hub_tap.0.lock().unwrap(), std::slice::from_ref(&fired.0));
        assert_eq!(*site_tap.0.lock().unwrap(), std::slice::from_ref(&ran.0));

        // The same name again is another thread: it hears nothing of the first.
        let mut reopened = bus.scoped("site").subscribe("chat", &[]);

        // A thread that ends takes its listeners and taps with it, and nobody else's.
        site.publish(PulseSource::module("worker"), None, message("last words"));
        site.close();
        site.publish(PulseSource::module("worker"), None, message("after the end"));
        assert_eq!(site_tap.0.lock().unwrap().len(), 2, "its tap was let go");
        api.publish(PulseSource::user("cli"), None, message("still here"));
        assert!(in_api.try_recv().is_some());
        assert!(reopened.try_recv().is_none());
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            assert!(in_site.recv().await.is_some(), "what was waiting is still read");
            assert!(in_site.recv().await.is_none(), "then the listener is told the thread is gone");
        });
    }
}
