//! The three planes on the board (decision D86): chat, the Principal and its activations, the
//! tasks under the goals they serve, and the vault. What the service sends for `/api/chat`,
//! `/api/principal`, `/api/activation/:id`, `/api/vault`, and the planes on the home screen.

use serde::{Deserialize, Serialize};

use crate::TaskNode;

/// The chat module and the Principal, as the home screen shows them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Planes {
    pub chat: ChatCard,
    pub principal: PrincipalCard,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChatCard {
    /// A chat model is set, so the planes are in use.
    pub on: bool,
    pub model: String,
    /// The newest turn: when, what the owner said (or the stimulus's headline), and the reply.
    pub last_at: Option<String>,
    pub last_said: String,
    pub last_reply: String,
    pub turns_today: u32,
    /// Lookups chat made today (its read-only tool calls), and how many ran out of time.
    pub reads_today: u32,
    pub timeouts_today: u32,
    pub signals_today: u32,
    /// The turn in flight, when there is one (decision D88).
    pub live: Option<PlaneLive>,
}

/// What a plane is doing this moment, as its process reports it to the board (decision D88).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlaneLive {
    /// `chat` or `principal`.
    pub plane: String,
    /// The activation (`a-...`), or the pulse of chat's turn.
    pub id: String,
    /// `Thinking`, `Writing`, `Reading <tool>`, `Calling <tool>`, or `idle`.
    pub step: String,
    /// The end of the text being written.
    pub tail: String,
    pub at: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrincipalCard {
    pub on: bool,
    pub model: String,
    /// The activation running now.
    pub active: Option<ActivationCard>,
    /// Set aside for a higher-priority one, the top of the stack last.
    pub suspended: Vec<ActivationCard>,
    /// Waiting, in the order they will run.
    pub queued: Vec<ActivationCard>,
    pub decisions_today: u32,
    pub cost_today: f64,
    /// The activation in flight, as it is written (decision D88).
    pub live: Option<PlaneLive>,
}

/// One activation, in brief.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActivationCard {
    pub id: String,
    /// `P0 User`, `P1 Event`, `P2 Idle`.
    pub priority: String,
    /// `userSignal`, `taskSettled`, `inbound`, `schedulePrompt`, `idle`.
    pub kind: String,
    pub topic: String,
    /// Steps completed so far (model turns and tool results).
    pub steps: u32,
    pub created: String,
}

/// A goal with the tasks that serve it, for the home screen's tree (decision D85).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GoalGroup {
    /// None for the tasks that serve no goal.
    pub goal: Option<GoalCard>,
    pub tasks: Vec<TaskNode>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GoalCard {
    pub id: String,
    /// `short`, `medium`, `long`.
    pub term: String,
    pub text: String,
    pub done: bool,
    pub due: Option<String>,
    pub next: Option<String>,
    /// Its project folder in the vault (`10-projects/<slug>`).
    pub project: Option<String>,
    /// Its `plan.md`, when there is one.
    pub plan: Option<String>,
    /// Tasks that served it, and the newest of them.
    pub tasks: u32,
    pub last_task: Option<String>,
}

/// `/api/chat`: the conversation as chat's turns, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChatView {
    pub model: Option<String>,
    pub turns: Vec<ChatTurn>,
    /// The turn in flight, when there is one (decision D89): a page opened mid-turn shows it at once.
    pub live: Option<PlaneLive>,
}

/// One turn of chat's, step by step.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChatTurn {
    pub turn_id: String,
    pub at: String,
    /// `userSay` or `principalStimulus`.
    pub origin: String,
    /// Where the owner was: the device and folder.
    pub focus: Option<String>,
    pub user_text: Option<String>,
    pub stimulus: Option<StimulusView>,
    pub recalled_memories: Vec<String>,
    pub thinking: String,
    pub tool_calls: Vec<ChatToolCall>,
    pub signals: Vec<SignalPill>,
    pub reply_text: String,
    /// A stimulus the conversation had already covered: nothing was shown.
    pub skipped: bool,
    pub tokens: u64,
    pub cost: f64,
}

/// What the Principal asked chat to tell the owner.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StimulusView {
    pub activation: String,
    pub kind: String,
    pub headline: String,
    pub must_convey: Vec<String>,
    pub ask_user: Option<String>,
    pub vault_ref: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChatToolCall {
    pub name: String,
    /// The arguments, as indented JSON.
    pub arguments: String,
    pub duration_ms: u64,
    pub output: String,
    pub timed_out: bool,
    pub is_error: bool,
}

/// What chat handed up, and the activation it became.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SignalPill {
    pub topic: String,
    pub urgency: String,
    pub activation: String,
}

/// `/api/principal`: the stack and queue now, and every activation, newest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrincipalView {
    pub card: PrincipalCard,
    pub activations: Vec<ActivationRow>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActivationRow {
    pub id: String,
    pub priority: String,
    pub kind: String,
    pub topic: String,
    /// `queued`, `active`, `suspended`, `joined`, `finished`.
    pub state: String,
    pub created: String,
    pub finished: Option<String>,
    /// Its decision, once it finished.
    pub summary: String,
    pub tokens: u64,
    pub cost: f64,
    pub tasks: Vec<String>,
    pub goals: Vec<String>,
    pub stimulated_chat: bool,
    /// How it was disposed of (decision D87): `stimulated_chat, tasks_touched`, or a
    /// `conclude_activation`'s `noted_only: why`.
    pub disposition: String,
}

/// `/api/activation/:id`: one activation, step by step.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActivationDetail {
    pub row: ActivationRow,
    /// What woke it, as indented JSON: chat's hand-over, the settled task, the material, the
    /// schedule, or the digest of idle time.
    pub trigger: String,
    pub started: Option<String>,
    pub steps: Vec<ActivationStep>,
    /// What was looked up for it before its first model call (decision D88).
    pub briefing: Option<BriefingView>,
}

/// An activation's briefing pack: the plans, notes, memories, and assets put before it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BriefingView {
    pub took_ms: u64,
    pub items: Vec<BriefingItemView>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BriefingItemView {
    /// `plan`, `entity`, `playbook`, `memory`, `asset`, `findings`, `conversation`, `material`,
    /// `digest`, `inbox`, `budget`.
    pub kind: String,
    pub title: String,
    pub path: Option<String>,
    pub text: String,
    /// The notes one `[[link]]` away, for an excerpt.
    pub links: Vec<String>,
}

/// One step of an activation's story.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ActivationStep {
    /// A model turn: its reasoning, its text, and the tools it called.
    Said {
        at: String,
        thinking: String,
        text: String,
        calls: Vec<String>,
        stopped: bool,
    },
    /// A tool call and its result. `task` links a task the call started or acted on.
    Tool {
        at: String,
        name: String,
        arguments: String,
        output: String,
        duration_ms: u64,
        is_error: bool,
        task: Option<String>,
    },
    /// What it had chat tell the owner.
    Stimulus {
        at: String,
        stimulus: StimulusView,
    },
    /// Set aside for a higher-priority activation, after so many steps.
    Suspended {
        at: String,
        by: String,
        step_count: u32,
    },
    /// Taken up again, told what was decided meanwhile.
    Resumed {
        at: String,
        delta: String,
    },
    /// It stopped having disposed of nothing, and the harness asked it to (decision D87).
    Nudged {
        at: String,
        text: String,
    },
    /// More from the owner joined it.
    Joined {
        at: String,
        activation: String,
        topic: String,
    },
    Finished {
        at: String,
        summary: String,
        tokens: u64,
        cost: f64,
    },
}

/// `/api/vault`: the goals, and the notes of the vault by folder.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VaultView {
    pub goals: Vec<GoalCard>,
    pub folders: Vec<VaultFolder>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VaultFolder {
    pub name: String,
    pub notes: Vec<crate::VaultNote>,
}

/// `/api/vault?path=…`: one note.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VaultNoteText {
    pub path: String,
    pub modified: String,
    pub text: String,
}

/// A note a task works under, as it reads now.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PinnedNote {
    pub path: String,
    pub text: String,
}
