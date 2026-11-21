//! The task ledger (docs/architecture.md §4.5, principle B6): every run of the agent is a task
//! with an owner, a state, and a heartbeat, kept in an append-only JSONL file. Following Pi's
//! durable harness, intent is written and synced before its effect: a task before its run, a
//! tool call before it executes. A process that dies mid-task leaves the task `Running` under
//! its pid; the next process that opens the same session finds it, counts the lost attempt, and
//! resumes it, until `RESTART_LIMIT` lost attempts end it as failed. The user's own verdict on a
//! task's result (decision D63) is a ledger event too. Only the harness writes the ledger; the
//! Inspector guard keeps the agent's tools away from it.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::place::DeviceFolder;
use crate::pulse::PulseId;
use crate::status::{RunStatus, StatusKind};

/// Lost attempts after which a task ends instead of resuming (Pi's `restartLimit` default).
pub const RESTART_LIMIT: u32 = 5;
/// Goals and results are kept this long in the ledger; the session has the full text.
const EXCERPT_CHARS: usize = 600;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskState {
    Planned,
    Running,
    BlockedOnUser,
    Done,
    Failed,
    Cancelled,
    /// No heartbeat within its window (set by the watchdog).
    Stalled,
    /// The run ended saying it continues, and a schedule it set will bring the agent back
    /// (decision D59). Nothing is running, by design; the watchdog checks the schedule instead.
    Waiting,
}

impl TaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Done | TaskState::Failed | TaskState::Cancelled)
    }

    pub fn label(self) -> &'static str {
        match self {
            TaskState::Planned => "planned",
            TaskState::Running => "running",
            TaskState::BlockedOnUser => "blocked",
            TaskState::Done => "done",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
            TaskState::Stalled => "stalled",
            TaskState::Waiting => "waiting",
        }
    }

    /// The state a finished run leaves its task in.
    pub fn from_run(status: &RunStatus, aborted: bool) -> Self {
        if aborted {
            return TaskState::Cancelled;
        }
        match status.kind {
            StatusKind::Done => TaskState::Done,
            StatusKind::Blocked => TaskState::BlockedOnUser,
            StatusKind::Failed => TaskState::Failed,
            // Only a run with status enforcement off ends here: with it on, a Continuing run
            // either named the schedule that brings it back (`LedgerEvent::Waiting`) or was
            // turned into Blocked (decision D59). The watchdog flags it if nothing follows.
            StatusKind::Continuing => TaskState::Running,
        }
    }
}

/// The user's judgement of a task's result (decision D63). Only the user gives one: it is what
/// the scorecard measures NyPi against, so the agent never rates itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Verdict {
    /// It did what was asked.
    Good,
    /// It got there only after the user pushed or corrected it.
    Fixed,
    /// What it reported was not true or not usable.
    Wrong,
    /// It stopped or went quiet without telling the user.
    Silent,
}

impl Verdict {
    pub const ALL: [Verdict; 4] = [Verdict::Good, Verdict::Fixed, Verdict::Wrong, Verdict::Silent];

    pub fn label(self) -> &'static str {
        match self {
            Verdict::Good => "good",
            Verdict::Fixed => "fixed",
            Verdict::Wrong => "wrong",
            Verdict::Silent => "silent",
        }
    }

    /// The single key that gives this verdict.
    pub fn key(self) -> char {
        self.label().chars().next().unwrap_or('?')
    }

    /// A verdict from its name or its key, in any case.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim().to_lowercase();
        Verdict::ALL.into_iter().find(|v| text == v.label() || text == v.key().to_string())
    }

    /// The verdicts as a line of choices: `g good · f fixed · w wrong · s silent`.
    pub fn choices() -> String {
        Verdict::ALL.map(|v| format!("{} {}", v.key(), v.label())).join(" · ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rating {
    pub verdict: Verdict,
    pub note: String,
    pub at: String,
}

/// One ledger line. The state of a task is the fold of its events in file order.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum LedgerEvent {
    Created {
        task: TaskId,
        at: String,
        goal: String,
        /// The full prompt, so a resume can send it again if the session never recorded it.
        prompt: String,
        owner: String,
        origin: String,
        parent: Option<TaskId>,
        session_id: String,
        session_path: Option<PathBuf>,
        cwd: PathBuf,
        /// The folder on another device the task works at; `cwd` is then where the hub keeps
        /// its conversation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote: Option<DeviceFolder>,
        pid: u32,
        pulse: Option<PulseId>,
    },
    /// A new process took the task over after the previous one died.
    Resumed {
        task: TaskId,
        at: String,
        pid: u32,
    },
    /// The watchdog started a process to resume the task; it is not dispatched again meanwhile.
    Dispatched {
        task: TaskId,
        at: String,
    },
    State {
        task: TaskId,
        at: String,
        state: TaskState,
        note: String,
    },
    /// The run ended `Continuing`; these schedules are its way back (decision D59).
    Waiting {
        task: TaskId,
        at: String,
        schedules: Vec<String>,
        note: String,
        /// What exactly brings the task back (decision D79). Lines from before it have none
        /// and are read by the older rule (`watchdog::check_waiting`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        on: Option<WaitPredicate>,
    },
    /// The task's workspace in the vault (`10-workspaces/…`, decision D78).
    Workspace {
        task: TaskId,
        at: String,
        workspace: String,
    },
    /// The goal the task serves and the Principal's activation that started it (decision D85).
    Campaign {
        task: TaskId,
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activation: Option<String>,
    },
    Heartbeat {
        task: TaskId,
        at: String,
    },
    ToolStarted {
        task: TaskId,
        at: String,
        call_id: String,
        tool: String,
        summary: String,
    },
    ToolFinished {
        task: TaskId,
        at: String,
        call_id: String,
        is_error: bool,
    },
    /// The user judged the result. A later verdict replaces an earlier one.
    Rated {
        task: TaskId,
        at: String,
        verdict: Verdict,
        note: String,
    },
}

impl LedgerEvent {
    fn task(&self) -> &TaskId {
        match self {
            LedgerEvent::Created { task, .. }
            | LedgerEvent::Resumed { task, .. }
            | LedgerEvent::Dispatched { task, .. }
            | LedgerEvent::State { task, .. }
            | LedgerEvent::Waiting { task, .. }
            | LedgerEvent::Workspace { task, .. }
            | LedgerEvent::Campaign { task, .. }
            | LedgerEvent::Heartbeat { task, .. }
            | LedgerEvent::ToolStarted { task, .. }
            | LedgerEvent::ToolFinished { task, .. }
            | LedgerEvent::Rated { task, .. } => task,
        }
    }
}

/// What a waiting task counts on to bring the agent back, as its wake id names it: a schedule
/// that fires, or a specialist working in the background (`d-…`, nypi-core/src/delegate.rs),
/// whose report comes back as a pulse. Kept as plain ids in the ledger, read typed here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Awaited<'a> {
    Schedule(&'a str),
    Delegation(&'a str),
}

impl<'a> Awaited<'a> {
    pub fn of(id: &'a str) -> Self {
        if id.starts_with(crate::delegate::DELEGATION_PREFIX) { Awaited::Delegation(id) } else { Awaited::Schedule(id) }
    }
}

/// One thing a waiting task counts on (decision D79).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "camelCase")]
pub enum WaitToken {
    /// A schedule that fires.
    Schedule(String),
    /// A specialist working in the background, by its delegation id (`d-…`).
    Delegation(String),
    /// Another task of the ledger reaching its end.
    Task(TaskId),
    /// An approval request being answered.
    Approval(String),
}

impl WaitToken {
    /// The token a plain wake id stands for.
    pub fn of(id: &str) -> WaitToken {
        match Awaited::of(id) {
            Awaited::Delegation(id) => WaitToken::Delegation(id.to_string()),
            Awaited::Schedule(id) => WaitToken::Schedule(id.to_string()),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            WaitToken::Schedule(id) | WaitToken::Delegation(id) | WaitToken::Approval(id) => id,
            WaitToken::Task(task) => &task.0,
        }
    }
}

/// What brings a waiting task back: the tasks of the ledger form a graph through these.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WaitPredicate {
    /// A join: every one of them has come through.
    AllOf {
        tokens: Vec<WaitToken>,
    },
    /// A race: the first that comes through without failing; the others are then stopped.
    AnyOf {
        tokens: Vec<WaitToken>,
    },
    Schedule {
        id: String,
    },
    Approval {
        id: String,
    },
}

/// What became of one thing a task waits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    Pending,
    /// It came through: the schedule fired, the specialist reported, the task ended well, the
    /// approval was answered.
    Fired,
    /// It never will: cancelled, failed, or never recorded.
    Gone,
}

/// Where a waiting task stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Waited {
    Pending,
    /// Through waiting, by these; `superseded` are the others of a race, which are still at
    /// work and no longer needed.
    Met {
        by: Vec<WaitToken>,
        superseded: Vec<WaitToken>,
    },
    /// Nothing it waits on can still come through.
    Abandoned,
}

impl WaitPredicate {
    /// The predicate plain wake ids stand for, where one predicate says it: a single schedule,
    /// or a join of specialists (decision D75). None for an empty or a mixed list, which the
    /// older rule still reads.
    pub fn of_wakes(ids: &[String]) -> Option<WaitPredicate> {
        let tokens: Vec<WaitToken> = ids.iter().map(|id| WaitToken::of(id)).collect();
        match tokens.as_slice() {
            [] => None,
            [WaitToken::Schedule(id)] => Some(WaitPredicate::Schedule { id: id.clone() }),
            all if all.iter().all(|token| matches!(token, WaitToken::Delegation(_))) => {
                Some(WaitPredicate::AllOf { tokens })
            }
            _ => None,
        }
    }

    pub fn tokens(&self) -> Vec<WaitToken> {
        match self {
            WaitPredicate::AllOf { tokens } | WaitPredicate::AnyOf { tokens } => tokens.clone(),
            WaitPredicate::Schedule { id } => vec![WaitToken::Schedule(id.clone())],
            WaitPredicate::Approval { id } => vec![WaitToken::Approval(id.clone())],
        }
    }

    /// Where the task stands, given what became of each token.
    pub fn judge(&self, state: impl Fn(&WaitToken) -> Wake) -> Waited {
        let tokens = self.tokens();
        let states: Vec<Wake> = tokens.iter().map(&state).collect();
        let with = |wanted: Wake| -> Vec<WaitToken> {
            tokens.iter().zip(&states).filter(|(_, state)| **state == wanted).map(|(token, _)| token.clone()).collect()
        };
        match self {
            WaitPredicate::AnyOf { .. } => match with(Wake::Fired).first() {
                Some(first) => Waited::Met { by: vec![first.clone()], superseded: with(Wake::Pending) },
                None if states.contains(&Wake::Pending) => Waited::Pending,
                None => Waited::Abandoned,
            },
            _ if states.contains(&Wake::Pending) => Waited::Pending,
            _ if !states.is_empty() && states.iter().all(|state| *state == Wake::Fired) => {
                Waited::Met { by: tokens, superseded: Vec::new() }
            }
            _ => Waited::Abandoned,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenTool {
    pub call_id: String,
    pub tool: String,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: TaskId,
    pub goal: String,
    pub prompt: String,
    pub owner: String,
    pub origin: String,
    pub parent: Option<TaskId>,
    pub session_id: String,
    pub session_path: Option<PathBuf>,
    pub cwd: PathBuf,
    /// The folder on another device the task works at, when it is not on this computer.
    pub remote: Option<DeviceFolder>,
    pub pulse: Option<PulseId>,
    pub created: String,
    /// Time of the task's latest event: the heartbeat the watchdog checks.
    pub heartbeat_at: String,
    pub state: TaskState,
    pub note: String,
    /// The process running the task, and how many processes have run it.
    pub pid: u32,
    pub attempts: u32,
    /// Tool calls that started and never finished.
    pub open_tools: Vec<OpenTool>,
    /// When the watchdog last started a process to resume it.
    pub dispatched_at: Option<String>,
    /// Background resumes started since a process last took the task over: resumes that never
    /// got as far as claiming it.
    pub dispatches: u32,
    /// The schedules a waiting task counts on to bring the agent back.
    pub wake: Vec<String>,
    /// What the user thought of the result, once they said.
    pub rating: Option<Rating>,
    /// The task's workspace in the vault, when it has one (decision D78).
    pub workspace: Option<String>,
    /// What exactly a waiting task waits on, when its line said (decision D79).
    pub waits: Option<WaitPredicate>,
    /// The goal (`g-…`) the task serves, when it serves one (decision D85).
    pub goal_id: Option<String>,
    /// The Principal's activation (`a-…`) that started it.
    pub spawned_by_activation: Option<String>,
}

impl TaskRecord {
    /// Not finished, and its process is gone: the task was cut off. A task that waits for the
    /// user or for a schedule has no process by design.
    pub fn is_interrupted(&self) -> bool {
        !self.state.is_terminal()
            && !matches!(self.state, TaskState::BlockedOnUser | TaskState::Waiting)
            && self.pid != std::process::id()
            && !process_alive(self.pid)
    }

    /// The run is over, with a result the user can judge: finished, or waiting for them.
    pub fn is_settled(&self) -> bool {
        self.state.is_terminal() || self.state == TaskState::BlockedOnUser
    }
}

pub fn process_alive(pid: u32) -> bool {
    // Signal 0 checks existence without delivering anything. EPERM means it exists but is not ours.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub struct Ledger {
    path: PathBuf,
    write: Mutex<()>,
    /// Every task as the log's events leave it, kept in step with the file (jsonl.rs).
    tasks: crate::jsonl::Folded<BTreeMap<TaskId, TaskRecord>>,
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn new_task_id() -> TaskId {
    // UUIDv7 sorts by creation time; the tail keeps it unique.
    let id = uuid::Uuid::now_v7().simple().to_string();
    TaskId(format!("t-{}", &id[id.len() - 12..]))
}

pub fn excerpt(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= EXCERPT_CHARS {
        return text.to_string();
    }
    format!("{}…", text.chars().take(EXCERPT_CHARS).collect::<String>())
}

impl Ledger {
    pub fn new(path: PathBuf) -> Self {
        let tasks = crate::jsonl::Folded::shared(&path);
        Ledger { path, write: Mutex::new(()), tasks }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event and sync it to disk before returning. Each event is a single write of
    /// one line, so processes sharing the file (bridge and terminal) do not interleave lines.
    pub fn append(&self, event: &LedgerEvent) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(event).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let _guard = self.write.lock().unwrap();
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new().create(true).read(true).append(true).open(&self.path)?;
        // A crash mid-write leaves a partial last line; start a fresh one so this event survives.
        let length = file.metadata()?.len();
        if length > 0 {
            let mut last = [0u8; 1];
            file.seek(SeekFrom::Start(length - 1))?;
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                line.insert(0, b'\n');
            }
        }
        file.write_all(&line)?;
        file.sync_data()
    }

    /// Look at every task, by id, without copying them. Only what was appended to the log since
    /// the last look is read. Unreadable lines (a crash mid-write) are skipped. `look` must not
    /// come back to the ledger.
    pub fn read<R>(&self, look: impl FnOnce(&BTreeMap<TaskId, TaskRecord>) -> R) -> R {
        let take_in = |tasks: &mut BTreeMap<TaskId, TaskRecord>, line: &str, _: u64| {
            if let Ok(event) = serde_json::from_str::<LedgerEvent>(line) {
                apply(tasks, event);
            }
        };
        self.tasks.read(take_in, look)
    }

    /// Every task, by id, as a copy to keep.
    pub fn load(&self) -> BTreeMap<TaskId, TaskRecord> {
        self.read(|tasks| tasks.clone())
    }

    pub fn get(&self, id: &TaskId) -> Option<TaskRecord> {
        self.read(|tasks| tasks.get(id).cloned())
    }

    /// Record the user's verdict on a task. Notes are kept short; the session has the detail.
    pub fn rate(&self, task: &TaskId, verdict: Verdict, note: &str) -> std::io::Result<()> {
        self.append(&LedgerEvent::Rated { task: task.clone(), at: now(), verdict, note: excerpt(note) })
    }

    /// Interrupted tasks of one session, oldest first.
    pub fn interrupted(&self, session_id: &str) -> Vec<TaskRecord> {
        self.read(|tasks| {
            tasks.values().filter(|t| t.session_id == session_id && t.is_interrupted()).cloned().collect()
        })
    }
}

/// A stalled task that shows life again is running.
fn recover(record: &mut TaskRecord) {
    if record.state == TaskState::Stalled {
        record.state = TaskState::Running;
    }
}

fn apply(tasks: &mut BTreeMap<TaskId, TaskRecord>, event: LedgerEvent) {
    if let LedgerEvent::Created {
        task,
        at,
        goal,
        prompt,
        owner,
        origin,
        parent,
        session_id,
        session_path,
        cwd,
        remote,
        pid,
        pulse,
    } = event
    {
        tasks.insert(
            task.clone(),
            TaskRecord {
                id: task,
                goal,
                prompt,
                owner,
                origin,
                parent,
                session_id,
                session_path,
                cwd,
                remote,
                pulse,
                created: at.clone(),
                heartbeat_at: at,
                state: TaskState::Planned,
                note: String::new(),
                pid,
                attempts: 1,
                open_tools: Vec::new(),
                dispatched_at: None,
                dispatches: 0,
                wake: Vec::new(),
                rating: None,
                workspace: None,
                waits: None,
                goal_id: None,
                spawned_by_activation: None,
            },
        );
        return;
    }
    let Some(record) = tasks.get_mut(event.task()) else { return };
    match event {
        LedgerEvent::Created { .. } => {}
        LedgerEvent::Resumed { at, pid, .. } => {
            record.heartbeat_at = at;
            record.pid = pid;
            record.attempts += 1;
            record.dispatches = 0;
            // Their process is gone; the resume answered them as interrupted.
            record.open_tools.clear();
        }
        LedgerEvent::State { at, state, note, .. } => {
            record.heartbeat_at = at;
            record.state = state;
            record.note = note;
            record.wake.clear();
            record.waits = None;
        }
        LedgerEvent::Waiting { at, schedules, note, on, .. } => {
            record.heartbeat_at = at;
            record.state = TaskState::Waiting;
            record.note = note;
            record.wake = schedules;
            record.waits = on;
        }
        LedgerEvent::Workspace { workspace, .. } => record.workspace = Some(workspace),
        LedgerEvent::Campaign { goal, activation, .. } => {
            record.goal_id = goal.or(record.goal_id.take());
            record.spawned_by_activation = activation.or(record.spawned_by_activation.take());
        }
        LedgerEvent::Dispatched { at, .. } => {
            record.dispatched_at = Some(at);
            record.dispatches += 1;
        }
        LedgerEvent::Heartbeat { at, .. } => {
            record.heartbeat_at = at;
            recover(record);
        }
        LedgerEvent::ToolStarted { at, call_id, tool, summary, .. } => {
            record.heartbeat_at = at;
            record.open_tools.push(OpenTool { call_id, tool, summary });
            recover(record);
        }
        LedgerEvent::ToolFinished { at, call_id, .. } => {
            record.heartbeat_at = at;
            record.open_tools.retain(|t| t.call_id != call_id);
            recover(record);
        }
        // Not a sign of life: the heartbeat stays where the work left it.
        LedgerEvent::Rated { at, verdict, note, .. } => record.rating = Some(Rating { verdict, note, at }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created(ledger: &Ledger, pid: u32) -> TaskId {
        let task = new_task_id();
        ledger
            .append(&LedgerEvent::Created {
                task: task.clone(),
                at: now(),
                goal: "count files".into(),
                prompt: "count files".into(),
                owner: "lucia".into(),
                origin: "cli".into(),
                parent: None,
                session_id: "s1".into(),
                session_path: None,
                cwd: "/".into(),
                remote: None,
                pid,
                pulse: None,
            })
            .unwrap();
        task
    }

    #[test]
    fn folds_events_and_finds_interrupted_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(dir.path().join("tasks/ledger.jsonl"));
        // A pid that cannot exist stands for a crashed process.
        let dead = created(&ledger, i32::MAX as u32 - 1);
        let state =
            |task: &TaskId, state| LedgerEvent::State { task: task.clone(), at: now(), state, note: String::new() };
        ledger.append(&state(&dead, TaskState::Running)).unwrap();
        let tool = |task: &TaskId, id: &str| LedgerEvent::ToolStarted {
            task: task.clone(),
            at: now(),
            call_id: id.into(),
            tool: "bash".into(),
            summary: "make".into(),
        };
        ledger.append(&tool(&dead, "c1")).unwrap();
        ledger.append(&tool(&dead, "c2")).unwrap();
        ledger
            .append(&LedgerEvent::ToolFinished { task: dead.clone(), at: now(), call_id: "c1".into(), is_error: false })
            .unwrap();
        let done = created(&ledger, i32::MAX as u32 - 1);
        ledger.append(&state(&done, TaskState::Done)).unwrap();
        let mine = created(&ledger, std::process::id());
        ledger.append(&state(&mine, TaskState::Running)).unwrap();
        std::fs::OpenOptions::new().append(true).open(ledger.path()).unwrap().write_all(b"{\"event\":\"sta").unwrap();

        let interrupted = ledger.interrupted("s1");
        assert_eq!(interrupted.len(), 1, "finished and live tasks are not interrupted");
        let task = &interrupted[0];
        assert_eq!(task.id, dead);
        assert_eq!(task.open_tools.iter().map(|t| t.call_id.as_str()).collect::<Vec<_>>(), ["c1", "c2"][1..]);
        ledger.append(&LedgerEvent::Resumed { task: dead.clone(), at: now(), pid: std::process::id() }).unwrap();
        let resumed = ledger.get(&dead).unwrap();
        assert_eq!((resumed.attempts, resumed.open_tools.len()), (2, 0));
        assert!(!resumed.is_interrupted(), "this process owns it now");
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn a_waiting_task_has_no_process_by_design_and_dispatches_count_until_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(dir.path().join("tasks/ledger.jsonl"));
        let dead = i32::MAX as u32 - 1;
        let waiting = created(&ledger, dead);
        ledger
            .append(&LedgerEvent::Waiting {
                task: waiting.clone(),
                at: now(),
                schedules: vec!["s-1".into()],
                note: "Continuing: the build runs until 18:00".into(),
                on: None,
            })
            .unwrap();
        let record = ledger.get(&waiting).unwrap();
        assert_eq!((record.state, record.wake.as_slice()), (TaskState::Waiting, &["s-1".to_string()][..]));
        assert!(!record.is_interrupted() && !record.state.is_terminal());
        let done = LedgerEvent::State { task: waiting.clone(), at: now(), state: TaskState::Done, note: String::new() };
        ledger.append(&done).unwrap();
        assert!(ledger.get(&waiting).unwrap().wake.is_empty(), "a later state ends the wait");

        let lost = created(&ledger, dead);
        for _ in 0..2 {
            ledger.append(&LedgerEvent::Dispatched { task: lost.clone(), at: now() }).unwrap();
        }
        assert_eq!(ledger.get(&lost).unwrap().dispatches, 2);
        ledger.append(&LedgerEvent::Resumed { task: lost.clone(), at: now(), pid: std::process::id() }).unwrap();
        assert_eq!(ledger.get(&lost).unwrap().dispatches, 0, "a process took it over");
    }

    #[test]
    fn a_rating_is_the_users_last_word_and_not_a_sign_of_life() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(dir.path().join("tasks/ledger.jsonl"));
        let task = created(&ledger, std::process::id());
        let done = LedgerEvent::State { task: task.clone(), at: now(), state: TaskState::Done, note: "Done: x".into() };
        ledger.append(&done).unwrap();
        let before = ledger.get(&task).unwrap();
        assert!(before.is_settled() && before.rating.is_none());

        ledger.rate(&task, Verdict::Good, "").unwrap();
        ledger.rate(&task, Verdict::Wrong, "the file was never written").unwrap();
        let after = ledger.get(&task).unwrap();
        let rating = after.rating.unwrap();
        assert_eq!((rating.verdict, rating.note.as_str()), (Verdict::Wrong, "the file was never written"));
        assert_eq!((after.state, &after.heartbeat_at), (TaskState::Done, &before.heartbeat_at));
        ledger.rate(&TaskId("t-nobody".into()), Verdict::Good, "").unwrap();
        assert_eq!(ledger.load().len(), 1, "a verdict on an unknown task makes no task");

        for (text, verdict) in
            [("g", Verdict::Good), ("Fixed", Verdict::Fixed), (" W ", Verdict::Wrong), ("silent", Verdict::Silent)]
        {
            assert_eq!(Verdict::parse(text), Some(verdict));
        }
        assert_eq!(Verdict::parse("great"), None);
        assert_eq!(Verdict::choices(), "g good · f fixed · w wrong · s silent");
        assert!(
            serde_json::to_string(&LedgerEvent::Rated {
                task,
                at: now(),
                verdict: Verdict::Fixed,
                note: String::new()
            })
            .unwrap()
            .contains(r#""event":"rated""#)
        );
    }

    #[test]
    fn a_write_torn_inside_a_character_costs_one_line_not_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(dir.path().join("tasks/ledger.jsonl"));
        let first = created(&ledger, std::process::id());
        // A crash mid-write, cut inside a multibyte character of a Chinese prompt.
        let torn = b"{\"event\":\"created\",\"task\":\"t-x\",\"goal\":\"\xe6\x8a";
        std::fs::OpenOptions::new().append(true).open(ledger.path()).unwrap().write_all(torn).unwrap();
        let second = created(&ledger, std::process::id());
        let ids: Vec<TaskId> = ledger.load().into_keys().collect();
        assert_eq!(ids.len(), 2, "the torn line is skipped; the tasks around it survive");
        assert!(ids.contains(&first) && ids.contains(&second));
    }

    #[test]
    fn a_long_ledger_is_read_once_and_then_only_what_is_new() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::new(dir.path().join("ledger.jsonl"));
        // A few months of use: tasks, and a heartbeat every half minute while each ran.
        let mut lines = String::new();
        let mut ids = Vec::new();
        for n in 0..400 {
            let task = TaskId(format!("t-{n:05}"));
            let created = LedgerEvent::Created {
                task: task.clone(),
                at: now(),
                goal: "build the report".into(),
                prompt: "build the report, with the numbers from last week".into(),
                owner: "lucia".into(),
                origin: "cli".into(),
                parent: None,
                session_id: "s".into(),
                session_path: None,
                cwd: "/".into(),
                remote: None,
                pid: std::process::id(),
                pulse: None,
            };
            lines.push_str(&serde_json::to_string(&created).unwrap());
            lines.push('\n');
            for _ in 0..40 {
                lines.push_str(
                    &serde_json::to_string(&LedgerEvent::Heartbeat { task: task.clone(), at: now() }).unwrap(),
                );
                lines.push('\n');
            }
            ids.push(task);
        }
        std::fs::write(ledger.path(), &lines).unwrap();

        let started = std::time::Instant::now();
        assert_eq!(ledger.read(|tasks| tasks.len()), 400);
        let first = started.elapsed();
        let started = std::time::Instant::now();
        for _ in 0..100 {
            assert_eq!(ledger.read(|tasks| tasks.len()), 400);
        }
        let later = started.elapsed() / 100;
        // What another process appends (its own Ledger, the same file) is taken in by the next look.
        let other = Ledger {
            path: ledger.path.clone(),
            write: Mutex::new(()),
            tasks: crate::jsonl::Folded::shared(dir.path()),
        };
        let done = LedgerEvent::State { task: ids[7].clone(), at: now(), state: TaskState::Done, note: "built".into() };
        other.append(&done).unwrap();
        assert_eq!(ledger.get(&ids[7]).unwrap().state, TaskState::Done);
        assert_eq!(ledger.get(&ids[8]).unwrap().state, TaskState::Planned);
        eprintln!("16,400 events: first read {first:?}, each later read {later:?}");
        assert!(later * 20 < first, "a later read does not read the file again: {later:?} against {first:?}");
    }
}
