//! The board's wire types (decision D56): what the board service sends its page, and the
//! actions the page sends back. The service (nypi-cli) and the page (nypi-board-ui, compiled to
//! WebAssembly) both use these, so they agree by construction. Times are RFC 3339 strings.

use serde::{Deserialize, Serialize};

mod planes;
pub use planes::*;

/// The home screen: what needs the owner, what runs, and the day so far.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Now {
    pub inbox: Vec<InboxItem>,
    pub processes: Vec<Process>,
    pub today: Today,
    pub upcoming: Vec<Upcoming>,
    /// What is being worked on, as a tree: a task under the task it works for (decision D80).
    #[serde(default)]
    pub graph: Vec<TaskNode>,
    /// The hub's devices: the computers and phones whose nodes carry out calls.
    #[serde(default)]
    pub devices: Vec<DeviceCard>,
    /// What changed last in Lucia's vault: the inbox and the workspaces of tasks.
    #[serde(default)]
    pub vault: Vec<VaultNote>,
    /// Chat and the Principal (decision D86).
    #[serde(default)]
    pub planes: Planes,
    /// The open tasks and the newest settled ones, under the goals they serve (decision D85).
    #[serde(default)]
    pub campaigns: Vec<GoalGroup>,
}

/// One task in the tree of what is being worked on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskNode {
    pub id: String,
    pub goal: String,
    /// `running`, `waiting`, `blocked`, `done`, ...
    pub state: String,
    /// How deep under the task it works for: 0 for a task of its own.
    pub depth: u32,
    /// Where it works: a folder's name, and the device when it is another.
    pub place: String,
    /// What a waiting task waits on, in a few words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waits: Option<String>,
    /// Its workspace in the vault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The card of the process working on it now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
    /// The goal it serves, and the Principal's activation that started it (decision D85).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DeviceCard {
    pub name: String,
    /// `computer`, `android`, `iphone`.
    pub kind: String,
    pub system: String,
    /// The NyPi version it runs (decision D98).
    pub version: String,
    pub connected: bool,
    /// Connected since then, or last seen then.
    pub since: String,
    pub tools: Vec<String>,
    /// What it is paired as over the network (`node`, `interface`); empty for one that
    /// comes over a forwarded socket.
    pub paired: Vec<String>,
    /// Calls it was carrying out when it last reported.
    pub running: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VaultNote {
    /// Relative to the vault.
    pub path: String,
    pub modified: String,
}

/// Something that waits on the owner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum InboxItem {
    /// A tool call waits for a yes or no.
    Approval {
        id: String,
        process: String,
        agent: String,
        tool: String,
        summary: String,
        reason: String,
        task: Option<TaskRef>,
        since: String,
    },
    /// A task ended asking the owner something.
    Blocked { task: TaskRef, question: String, since: String },
    /// A task stopped making progress, or its process died before it finished.
    Stuck { task: TaskRef, state: String, note: String, since: String },
    /// An alert the owner has not acknowledged.
    Alert { key: String, level: String, message: String, since: String },
    /// A machine or browser asks to join the hub (decision D93): admit it if its code is the one
    /// on its screen.
    /// `asker` is `machine` or `browser`.
    Join { id: String, asker: String, name: String, system: String, from: String, code: String, expires: String },
}

impl InboxItem {
    /// What identifies the item across refreshes.
    pub fn key(&self) -> String {
        match self {
            InboxItem::Approval { id, .. } => format!("approval:{id}"),
            InboxItem::Blocked { task, .. } => format!("blocked:{}", task.id),
            InboxItem::Stuck { task, .. } => format!("stuck:{}", task.id),
            InboxItem::Alert { key, .. } => format!("alert:{key}"),
            InboxItem::Join { id, .. } => format!("join:{id}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRef {
    pub id: String,
    pub goal: String,
}

/// A NyPi process connected to the board.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    /// The board's name for the connection.
    pub id: String,
    pub pid: u32,
    /// Where it runs: tui, cli, telegram, print, rpc, ...
    pub front_end: String,
    pub agent: String,
    pub model: String,
    pub cwd: String,
    pub session_id: String,
    pub connected_at: String,
    /// The task it works on; None when idle.
    pub task: Option<TaskRef>,
    pub step: Step,
    /// Tokens the last request filled, and the model's window.
    pub context_tokens: u64,
    pub context_window: u64,
    /// The current run's cost so far.
    pub run_cost: f64,
    pub run_started: Option<String>,
    /// Events the process could not send because the board fell behind.
    pub dropped: u64,
}

/// What a process is doing at this moment.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Step {
    #[default]
    Idle,
    /// Waiting for the model's first words.
    Waiting {
        since: String,
    },
    Thinking {
        since: String,
    },
    /// The end of the reply so far (only while someone watches; else empty).
    Writing {
        since: String,
        tail: String,
    },
    Tool {
        since: String,
        name: String,
        summary: String,
    },
    Retrying {
        since: String,
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: String,
    },
    Compacting {
        since: String,
        reason: String,
    },
    Approval {
        since: String,
        id: String,
        tool: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Today {
    pub done: u32,
    pub blocked: u32,
    pub failed: u32,
    pub continuing: u32,
    pub tool_calls: u32,
    pub tool_failures: u32,
    pub retries: u32,
    pub retries_recovered: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost: f64,
    /// The average daily cost over the seven days before today.
    pub cost_week_average: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Upcoming {
    pub at: String,
    pub label: String,
}

/// One task as a story: its header and its steps, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDetail {
    pub id: String,
    pub goal: String,
    pub prompt: String,
    /// done, blocked, failed, continuing, running, stalled, cancelled, planned.
    pub state: String,
    pub note: String,
    pub owner: String,
    pub origin: String,
    pub created: String,
    pub updated: String,
    pub attempts: u32,
    pub parent: Option<TaskRef>,
    pub children: Vec<TaskRef>,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost: f64,
    pub steps: Vec<StoryStep>,
    /// The connected process running it, when it runs now.
    pub live: Option<String>,
    /// The goal it serves and the activation that started it (decision D85).
    #[serde(default)]
    pub goal_id: Option<String>,
    #[serde(default)]
    pub activation: Option<String>,
    /// Where it works: the folder, and the device when it is another.
    #[serde(default)]
    pub place: String,
    /// What the worker works under, as it reads now: the goal's plan, and the workspace's
    /// brief and findings (decision D86).
    #[serde(default)]
    pub plan: Option<PinnedNote>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub brief: Option<String>,
    #[serde(default)]
    pub findings: Option<String>,
    /// How many times the Principal sent it back.
    #[serde(default)]
    pub rounds: u32,
    /// The worker's `report.md` and the evidence manifest the harness drew from the task's
    /// session, as the Principal reviews them (decision D87).
    #[serde(default)]
    pub report: Option<String>,
    #[serde(default)]
    pub manifest: Option<String>,
    /// The references the report cites that resolve to a call of the task (`t3`), and those
    /// that match none.
    #[serde(default)]
    pub verified_refs: Vec<String>,
    #[serde(default)]
    pub unknown_refs: Vec<String>,
    /// The task changed a file and reported Done with nothing checking it afterwards.
    #[serde(default)]
    pub unchecked_change: bool,
}

/// One step of a task's story.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum StoryStep {
    /// What the owner (or whoever started it) asked.
    Prompt { at: String, text: String, from: String },
    /// The model's reply in one turn.
    Said {
        at: String,
        model: String,
        text: String,
        thinking: String,
        tokens_in: u64,
        tokens_out: u64,
        cost: f64,
        error: Option<String>,
    },
    /// A tool call and, once finished, its result.
    Tool {
        at: String,
        call_id: String,
        name: String,
        summary: String,
        arguments: String,
        output: Option<String>,
        truncated: bool,
        is_error: bool,
        duration_ms: Option<u64>,
        /// The device the call was carried out on, when it was not this computer.
        #[serde(default)]
        device: Option<String>,
    },
    /// The observer of the run's course steered it (decision D78).
    Steer { at: String, text: String },
    /// The Principal's verdict on the report (decision D84): accepted, or sent back.
    Review { at: String, activation: String, accepted: bool, summary: String },
    /// Something the harness did or saw: a retry, a compaction, an approval, a steering message.
    Note { at: String, tone: Tone, title: String, detail: String },
    /// Work handed to a specialist, as a child task.
    Child { at: String, task: TaskRef, state: String },
    /// How the run ended.
    Ended { at: String, status: String, text: String, answer: String },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Tone {
    #[default]
    Plain,
    Good,
    Warning,
    Bad,
}

/// A row of the activity list: a task or a notable event, newest first.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityRow {
    pub at: String,
    /// task, alert, approval, trigger, reflection, dmn.
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub task: Option<TaskRef>,
    pub state: Option<String>,
    pub agent: String,
    pub origin: String,
}

/// The whole program as a map (decision D65): every module, how they connect, and what each
/// one is doing now. The service says what exists and its state; the page decides where to draw
/// it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Topology {
    pub groups: Vec<TopoGroup>,
    pub nodes: Vec<TopoNode>,
    pub edges: Vec<TopoEdge>,
}

/// A region of the map: modules that belong together.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopoGroup {
    pub id: String,
    pub title: String,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopoNode {
    pub id: String,
    pub group: String,
    pub name: String,
    /// One sentence: what it is for.
    pub role: String,
    /// Where it lives in the code, relative to the repository.
    pub source: String,
    pub state: NodeState,
    /// What it is doing or holds right now, in a few words.
    pub status: String,
    /// More of the same, for the detail panel.
    pub facts: Vec<Fact>,
    /// A screen of the board that shows more (a task, a filtered activity list).
    pub link: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fact {
    pub label: String,
    pub value: String,
}

/// How a module is right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NodeState {
    /// Working at this moment.
    Active,
    /// There and waiting for work.
    #[default]
    Ready,
    /// Not there now, and that is fine: it exists only while it is used.
    Absent,
    /// Turned off, or not set up.
    Off,
    /// Should be there and is not, or holds something that has not reached the owner.
    Down,
}

/// A connection: `from` acts on `to`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopoEdge {
    pub from: String,
    pub to: String,
    pub label: String,
    pub kind: EdgeKind,
    /// Something is going over it at this moment.
    pub active: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EdgeKind {
    /// What the owner and the agent say to each other.
    Message,
    /// Work handed on: a model call, a tool call, a delegation, a fired prompt.
    Work,
    /// Supervision: keeping alive, checking, guarding.
    Watch,
    /// Reading or writing what is kept on disk.
    Record,
    /// Telling the owner something they did not ask for at that moment.
    Notice,
}

/// What the owner asks the board to do.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Action {
    Approve {
        id: String,
    },
    Deny {
        id: String,
        reason: Option<String>,
    },
    /// Abort the process's current run.
    Stop {
        process: String,
    },
    /// Send a message into the process's run (steering), or start one when idle.
    Message {
        process: String,
        text: String,
    },
    /// Take an item out of the inbox.
    Acknowledge {
        key: String,
    },
    /// Say something to Lucia, as the owner, in the hub's one conversation (decision D86).
    Say {
        text: String,
    },
    /// Answer a join request (decision D93): admit as the user's own, as exec-only, or refuse.
    Admit {
        code: String,
        exec_only: bool,
    },
    Refuse {
        code: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResult {
    pub ok: bool,
    pub message: String,
}

/// What the live stream carries (server-sent event names: `now`, `task`, `delta`, `chat`,
/// `activation`).
// An event is made, serialized, and sent (or read and taken apart) at once; none is kept in a
// collection, where the size of the largest variant would cost.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum LiveEvent {
    Now {
        now: Now,
    },
    Task {
        task: Box<TaskDetail>,
    },
    /// Text the watched process just wrote.
    Delta {
        process: String,
        text: String,
    },
    /// The conversation's turns, when one was added (`chat=1`).
    Chat {
        chat: Box<ChatView>,
    },
    /// An activation's story as it grows (`activation=<id>`), and the Principal's stack with it.
    Activation {
        activation: Box<ActivationDetail>,
        principal: Box<PrincipalCard>,
    },
    /// What chat or the Principal is doing this moment, while a turn or an activation is in
    /// flight (decision D88); `step` is `idle` when it ended.
    Plane {
        live: PlaneLive,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_and_steps_keep_their_wire_shape() {
        let action: Action = serde_json::from_str(r#"{"action":"deny","id":"a1","reason":"not now"}"#).unwrap();
        assert_eq!(action, Action::Deny { id: "a1".into(), reason: Some("not now".into()) });
        let step = Step::Retrying { since: "t".into(), attempt: 1, max_attempts: 3, delay_ms: 2000, error: "e".into() };
        assert_eq!(
            serde_json::to_value(&step).unwrap(),
            serde_json::json!({ "kind": "retrying", "since": "t", "attempt": 1, "maxAttempts": 3, "delayMs": 2000, "error": "e" })
        );
    }
}
