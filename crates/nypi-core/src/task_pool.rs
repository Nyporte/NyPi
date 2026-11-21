//! The task pool (decision D78): the background tasks of one conversation, several at a time.
//! The principal (the chat module, in front) hands a task over as a brief: what to do, what
//! counts as done, and where to stop. A brief that says what counts as done gets a workspace in
//! the vault (`10-workspaces/YYMMDD-<slug>/` with `brief.md` and `findings.md`), which the
//! working agent sees on every turn, so its notes outlive any compaction.
//!
//! The first task runs in the conversation's own worker, which keeps the conversation's working
//! context. A task started while that one is busy gets a runtime of its own beside it (the same
//! home, folder, model, and tools; a session and a bus thread of its own), up to
//! `tasks.maxConcurrent` at once. A task that settled stays in the pool until the principal has
//! reviewed it, so a report that falls short can be sent back to the agent that wrote it, with
//! its context intact (`continue_task`).
//!
//! A brief may be a topic (decision D82): a one-line anchor, with what the owner said in the
//! conversation attached verbatim by the harness instead of retold by the principal. The worker
//! then writes what counts as done into the workspace's `brief.md` on its first step, and the
//! principal reviews against the brief as it reads then. A runtime beside the conversation's
//! connects its MCP servers in its own task, so starting a task never waits on them; and the
//! conversation's own worker begins a fresh session when a task is for another place than its
//! last one, so one project's transcript does not lead the next astray.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::ledger::{self, TaskId};
use crate::place::Focus;
use crate::pulse::{Delivery, PulseId, PulseSource};
use crate::runtime::{RunOutcome, Runtime};
use crate::status::{RunStatus, StatusKind};
use crate::worker::{RunSetup, WorkerEvent, WorkerHandle};

/// A task's contract, as the principal writes it when handing the task over.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskBrief {
    pub title: String,
    /// The task in one sentence, as the principal anchors it when the conversation says the
    /// rest (decision D82). Stands for the instruction when there is none.
    pub topic: String,
    pub instruction: String,
    /// The proof this task needs before it may be called done (a passing reproduction, data
    /// checked against its primary source, a verified HTTP status). Empty for a one-step action.
    pub done_when: String,
    /// Where to stop, beyond the fixed conduct rules.
    pub bounds: Option<String>,
    /// The device and folder to work at: where the owner was when they asked, unless the
    /// principal names another. The conversation's own place without one.
    pub place: Option<Focus>,
    /// The recent exchanges of the conversation, verbatim, attached by the harness.
    pub conversation: Option<String>,
    /// The goal (`g-…`) the task serves: its project plan is pinned into the worker's prompt
    /// (decision D85).
    pub goal: Option<String>,
    /// The Principal's activation (`a-…`) that started it.
    pub spawned_by_activation: Option<String>,
    /// When the Principal reviews it (decision D87).
    #[serde(skip_serializing_if = "ReviewPolicy::is_immediate")]
    pub review_policy: ReviewPolicy,
}

/// When a settled task is reviewed (decision D87).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPolicy {
    /// As soon as it settles.
    #[default]
    Immediate,
    /// Settled Done while tasks for the same goal (or started by the same activation) still
    /// work: with them, once they settle or the hold runs out. Blocked or Failed: at once.
    BatchUnlessFailed,
}

impl ReviewPolicy {
    fn is_immediate(&self) -> bool {
        *self == ReviewPolicy::Immediate
    }
}

/// The heading of the section of `brief.md` that says what counts as done.
const DONE_WHEN: &str = "## Done when";
/// What that section says until the worker has set it.
const DONE_WHEN_UNSET: &str = "(not set yet: the worker sets it on its first step, from the conversation)";

/// What counts as done, as the workspace's `brief.md` says now: its last `## Done when`
/// section (the worker may have appended one), or None while it is unset or empty.
pub fn done_when_in(brief: &str) -> Option<String> {
    let at = brief.rfind(DONE_WHEN)?;
    let rest = &brief[at + DONE_WHEN.len()..];
    let section = match rest.find("\n## ") {
        Some(end) => &rest[..end],
        None => rest,
    };
    let section = section.trim();
    (!section.is_empty() && section != DONE_WHEN_UNSET).then(|| section.to_string())
}

impl TaskBrief {
    /// A short name for lists and for the workspace's folder.
    pub fn name(&self) -> String {
        let source = [&self.title, &self.topic, &self.instruction]
            .into_iter()
            .find(|text| !text.trim().is_empty())
            .unwrap_or(&self.instruction);
        let words: Vec<&str> = source.split_whitespace().take(8).collect();
        words.join(" ")
    }

    /// What the task is, in the principal's words: the instruction, else the topic.
    pub fn what(&self) -> &str {
        if self.instruction.trim().is_empty() { self.topic.trim() } else { self.instruction.trim() }
    }

    /// The task as the working agent is given it.
    pub fn for_worker(&self, workspace: Option<&str>) -> String {
        let mut text = match (self.topic.trim(), self.instruction.trim()) {
            (topic, "") => format!("Topic: {topic}"),
            ("", instruction) => instruction.to_string(),
            (topic, instruction) => format!("Topic: {topic}\n\n{instruction}"),
        };
        if let Some(conversation) = self.conversation.as_deref().filter(|c| !c.trim().is_empty()) {
            text.push_str(&format!(
                "\n\n<conversation_context>\n{}\n</conversation_context>\nThat is what was said in the conversation, \
                 verbatim and newest last; the topic is the principal's one-line anchor for it. Work from the user's \
                 own words: every detail, constraint, and preference in them stands.",
                conversation.trim()
            ));
        }
        if !self.done_when.trim().is_empty() {
            text.push_str(&format!("\n\nDone when: {}", self.done_when.trim()));
        }
        if let Some(bounds) = self.bounds.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            text.push_str(&format!("\nStop before: {bounds}"));
        }
        if let Some(workspace) = workspace {
            text.push_str(&format!(
                "\n\nThis task has a workspace in your vault, {workspace}/ (brief.md is this brief; findings.md is \
                 yours to keep as you go)."
            ));
            if self.done_when.trim().is_empty() {
                text.push_str(&format!(
                    " What counts as done is not set yet. On your first step, before the work, append a `{DONE_WHEN}` \
                     section to {workspace}/brief.md (vault_write, mode append) that says, from the conversation, \
                     what will show this task is done (a passing check, data confirmed at its source, a verified \
                     response), or `one step` for a single action. Your report is reviewed against it."
                ));
            }
        }
        text
    }

    fn brief_note(&self, task: &TaskId) -> String {
        let mut note = format!(
            "---\ntitle: {}\nkind: brief\ntask: {task}\nstatus: open\nupdated: {}\n---\n# {}\n\n## Instruction\n{}\n",
            self.name().replace('\n', " "),
            chrono::Local::now().format("%Y-%m-%d"),
            self.name(),
            self.what()
        );
        let done_when = if self.done_when.trim().is_empty() { DONE_WHEN_UNSET } else { self.done_when.trim() };
        note.push_str(&format!("\n{DONE_WHEN}\n{done_when}\n"));
        if let Some(bounds) = self.bounds.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
            note.push_str(&format!("\n## Stop before\n{bounds}\n"));
        }
        if let Some(place) = &self.place {
            note.push_str(&format!("\n## Place\n{place}\n"));
        }
        if let Some(conversation) = self.conversation.as_deref().filter(|c| !c.trim().is_empty()) {
            note.push_str(&format!("\n## Conversation\n{}\n", conversation.trim()));
        }
        note
    }
}

const FINDINGS_START: &str = "# Findings\n\nWhat this task has established so far: facts checked against their source \
(with the [tN] that shows it), approaches that turned out false and why, and dead ends not to walk again.\n";

/// How a task came out, as the principal reviews it.
#[derive(Clone, Debug)]
pub struct Settlement {
    pub status: RunStatus,
    pub answer: String,
    /// What the report cites, checked by the harness, in one line.
    pub evidence: String,
}

impl Settlement {
    pub fn of(outcome: &Result<RunOutcome, String>) -> Settlement {
        match outcome {
            Ok(outcome) => {
                let sources = outcome.citations.footer().unwrap_or_default();
                let evidence = if sources.trim().is_empty() {
                    "The report cites no tool result.".to_string()
                } else {
                    sources.trim().to_string()
                };
                Settlement { status: outcome.status.clone(), answer: outcome.answer.clone(), evidence }
            }
            Err(error) => Settlement {
                status: RunStatus { kind: StatusKind::Failed, text: error.clone(), synthesized: true },
                answer: String::new(),
                evidence: "The run ended in an error before it could report.".into(),
            },
        }
    }
}

enum Lane {
    /// The conversation's own worker.
    Own,
    /// A runtime of the task's own, beside it.
    Beside(WorkerHandle),
}

struct Run {
    brief: TaskBrief,
    workspace: Option<String>,
    lane: Lane,
    /// The ledger task of the run under way or last ended (a continuation is a task of its own).
    current: TaskId,
    /// Times the principal sent it back.
    round: u32,
    /// The pulse that started the run under way or last ended.
    pulse: PulseId,
    settled: Option<Settlement>,
}

/// What `start` gives back.
#[derive(Clone, Debug)]
pub struct Started {
    pub task: TaskId,
    pub workspace: Option<String>,
    pub pulse: PulseId,
    /// Whether it runs in a runtime of its own beside the conversation's worker.
    pub beside: bool,
}

/// A task as the principal sees it in a list.
#[derive(Clone, Debug)]
pub struct TaskView {
    pub task: String,
    pub name: String,
    /// The goal it serves.
    pub goal: Option<String>,
    pub workspace: Option<String>,
    pub round: u32,
    /// None while it runs.
    pub settled: Option<StatusKind>,
    pub describe: String,
}

struct Inner {
    own: WorkerHandle,
    max: usize,
    vault: Option<Arc<nypi_vault::tools::VaultStore>>,
    runs: Mutex<BTreeMap<String, Run>>,
    /// Where the last task the pool ran on the conversation's own worker worked; None before
    /// the first (what the worker held then is the conversation's own).
    own_place: Mutex<Option<Option<Focus>>>,
}

#[derive(Clone)]
pub struct TaskPool(Arc<Inner>);

impl TaskPool {
    /// The pool of the conversation whose own worker is `own`. `max` tasks run at once; `vault`
    /// is where workspaces are made, when the conversation has one.
    pub fn new(own: WorkerHandle, max: usize, vault: Option<Arc<nypi_vault::tools::VaultStore>>) -> TaskPool {
        TaskPool(Arc::new(Inner { own, max: max.max(1), vault, runs: Mutex::default(), own_place: Mutex::default() }))
    }

    /// The conversation's own worker.
    pub fn own(&self) -> &WorkerHandle {
        &self.0.own
    }

    fn lane<'a>(&'a self, run: &'a Run) -> &'a WorkerHandle {
        match &run.lane {
            Lane::Own => &self.0.own,
            Lane::Beside(worker) => worker,
        }
    }

    /// Tasks working now.
    pub fn running(&self) -> usize {
        let runs = self.0.runs.lock().unwrap();
        let tracked = runs.values().filter(|run| run.settled.is_none()).count();
        // The own worker may be on work the pool did not start (a schedule's prompt, a resume).
        let own_tracked = runs.values().any(|run| run.settled.is_none() && matches!(run.lane, Lane::Own));
        tracked + usize::from(self.0.own.current().is_some() && !own_tracked)
    }

    /// Make the task's workspace in the vault: `10-workspaces/YYMMDD-<slug>/` with the brief and
    /// an empty findings note. None without a vault, or when the folder cannot be made (the
    /// task then runs without one).
    fn provision(&self, brief: &TaskBrief, task: &TaskId) -> Option<String> {
        let vault = self.0.vault.as_ref()?;
        let day = chrono::Local::now().format("%y%m%d");
        let base = format!("{}/{day}-{}", nypi_vault::vault::WORKSPACES, nypi_vault::vault::slug(&brief.name()));
        let mut path = base.clone();
        let mut number = 1;
        while vault.vault().resolve(&path).is_ok_and(|resolved| resolved.exists()) {
            number += 1;
            path = format!("{base}-{number}");
        }
        vault
            .write(&format!("{path}/brief.md"), &brief.brief_note(task), nypi_vault::vault::WriteMode::Replace)
            .ok()?;
        vault.write(&format!("{path}/findings.md"), FINDINGS_START, nypi_vault::vault::WriteMode::Replace).ok()?;
        Some(path)
    }

    /// Hand a task over. It runs in the conversation's own worker when that is free, and
    /// otherwise in a runtime of its own beside it; refused when `tasks.maxConcurrent` are
    /// already working.
    pub async fn start(
        &self,
        brief: TaskBrief,
        source: PulseSource,
        cause: Option<PulseId>,
    ) -> Result<Started, String> {
        if brief.what().is_empty() {
            return Err("a task needs a topic or an instruction".into());
        }
        let working = self.running();
        if working >= self.0.max {
            return Err(format!(
                "{working} tasks are working, which is the limit (tasks.maxConcurrent). Wait for one to settle, \
                 steer one, or cancel one."
            ));
        }
        let task = ledger::new_task_id();
        // A brief that says what counts as done is a task worth a workspace, and so is one the
        // conversation describes: its worker writes there what counts as done.
        let worth = !brief.done_when.trim().is_empty() || brief.conversation.is_some();
        let workspace = if worth { self.provision(&brief, &task) } else { None };
        let text = brief.for_worker(workspace.as_deref());
        let mut setup = RunSetup {
            id: Some(task.clone()),
            workspace: workspace.clone(),
            focus: brief.place.clone(),
            fresh_session: false,
            goal: brief.goal.clone(),
            activation: brief.spawned_by_activation.clone(),
        };
        let (lane, pulse) = if self.0.own.current().is_none() {
            // Another place than the last task's: what the worker held is another project's.
            let mut last = self.0.own_place.lock().unwrap();
            setup.fresh_session = last.as_ref().is_some_and(|before| *before != brief.place);
            *last = Some(brief.place.clone());
            drop(last);
            (Lane::Own, self.0.own.start_with(&text, source, cause, setup)?)
        } else {
            let worker = self.beside(&task).await?;
            let pulse = worker.start_with(&text, source, cause, setup)?;
            (Lane::Beside(worker), pulse)
        };
        let beside = matches!(lane, Lane::Beside(_));
        let run = Run {
            brief,
            workspace: workspace.clone(),
            lane,
            current: task.clone(),
            round: 0,
            pulse: pulse.clone(),
            settled: None,
        };
        self.0.runs.lock().unwrap().insert(task.0.clone(), run);
        Ok(Started { task, workspace, pulse, beside })
    }

    /// A worker of the task's own: a runtime like the conversation's, asking the same approver
    /// and raising its alerts in the same place, its events passed on marked with the task.
    async fn beside(&self, task: &TaskId) -> Result<WorkerHandle, String> {
        let own = &self.0.own;
        let mut runtime = Runtime::new(own.beside(&format!("task:{task}")))
            .map_err(|error| format!("a runtime for the task could not be made: {error:#}"))?;
        runtime.set_task_owner("worker");
        if let Some(approver) = own.inspector().approver() {
            runtime.inspector().set_approver(approver);
        }
        if let Some(mut alerts) = runtime.take_alerts() {
            // What was raised while it was made is not this task's news.
            while alerts.try_recv().is_ok() {}
            let inspector = own.inspector().clone();
            tokio::spawn(async move {
                while let Some(alert) = alerts.recv().await {
                    inspector.alert(alert);
                }
            });
        }
        // Its MCP servers are connected in its own task: a slow or unreachable server delays this
        // task's start, never the conversation that started it.
        let alerts = own.inspector().clone();
        let (worker, mut events) = crate::worker::spawn_then(runtime, move |runtime| {
            Box::pin(async move {
                for error in runtime.connect_mcp().await {
                    alerts.alert(crate::inspector::Alert::new(crate::inspector::AlertLevel::Warning, error));
                }
            })
        });
        let (out, key) = (own.events().clone(), task.0.clone());
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if out.send(WorkerEvent::Beside { task: key.clone(), event: Box::new(event) }).is_err() {
                    break;
                }
            }
        });
        Ok(worker)
    }

    /// The task a call means: the one named, or the only one working when none is.
    fn find(&self, task: Option<&str>) -> Result<String, String> {
        let runs = self.0.runs.lock().unwrap();
        match task.map(str::trim).filter(|t| !t.is_empty()) {
            Some(task) if runs.contains_key(task) => Ok(task.to_string()),
            Some(task) => Err(format!("No task {task} here. {}", list(&runs))),
            None => {
                let working: Vec<&String> =
                    runs.iter().filter(|(_, run)| run.settled.is_none()).map(|(id, _)| id).collect();
                match working.as_slice() {
                    [only] => Ok((*only).clone()),
                    [] => Err("No task is working.".into()),
                    _ => Err(format!("Several tasks are working; name one with task_id. {}", list(&runs))),
                }
            }
        }
    }

    /// Add an instruction to a working task.
    pub fn instruct(
        &self,
        task: Option<&str>,
        text: &str,
        delivery: Delivery,
        source: PulseSource,
        cause: Option<PulseId>,
    ) -> Result<String, String> {
        // Work the pool did not start (a resumed task, a schedule's prompt) is the own worker's.
        let key = match self.find(task) {
            Ok(key) => key,
            Err(error) if task.is_none() && self.0.own.current().is_some() => {
                let _ = error;
                return self.0.own.instruct(text, delivery, source, cause).map(|_| "the running task".to_string());
            }
            Err(error) => return Err(error),
        };
        let runs = self.0.runs.lock().unwrap();
        let run = runs.get(&key).ok_or("the task is gone")?;
        if run.settled.is_some() {
            return Err(format!("Task {key} has settled; use continue_task to send it back, or start a new one."));
        }
        self.lane(run).instruct(text, delivery, source, cause)?;
        Ok(key)
    }

    /// Stop a working task. Returns which one.
    pub fn cancel(&self, task: Option<&str>) -> Result<String, String> {
        let key = match self.find(task) {
            Ok(key) => key,
            Err(error) if task.is_none() => {
                return if self.0.own.abort() { Ok("the running task".into()) } else { Err(error) };
            }
            Err(error) => return Err(error),
        };
        let runs = self.0.runs.lock().unwrap();
        let run = runs.get(&key).ok_or("the task is gone")?;
        if self.lane(run).abort() { Ok(key) } else { Err(format!("Task {key} is not working.")) }
    }

    /// Stop everything that works. Returns whether anything did.
    pub fn cancel_all(&self) -> bool {
        let runs = self.0.runs.lock().unwrap();
        let mut any = self.0.own.abort();
        for run in runs.values() {
            if let Lane::Beside(worker) = &run.lane {
                any |= worker.abort();
            }
        }
        any
    }

    /// What one task, or every task, is doing.
    pub fn describe(&self, task: Option<&str>) -> String {
        let runs = self.0.runs.lock().unwrap();
        if let Some(task) = task.map(str::trim).filter(|t| !t.is_empty()) {
            return match runs.get(task) {
                Some(run) => describe(task, run, self.lane(run)),
                None => format!("No task {task} here. {}", list(&runs)),
            };
        }
        let mut lines: Vec<String> = runs.iter().map(|(id, run)| describe(id, run, self.lane(run))).collect();
        let own_tracked = runs.values().any(|run| run.settled.is_none() && matches!(run.lane, Lane::Own));
        if !own_tracked && self.0.own.current().is_some() {
            lines.insert(0, self.0.own.describe());
        }
        if lines.is_empty() { "Idle.".into() } else { lines.join("\n\n") }
    }

    pub fn views(&self) -> Vec<TaskView> {
        let runs = self.0.runs.lock().unwrap();
        runs.iter()
            .map(|(id, run)| TaskView {
                task: id.clone(),
                name: run.brief.name(),
                goal: run.brief.goal.clone(),
                workspace: run.workspace.clone(),
                round: run.round,
                settled: run.settled.as_ref().map(|settled| settled.status.kind),
                describe: describe(id, run, self.lane(run)),
            })
            .collect()
    }

    /// The task whose run was recorded in the ledger as `ledger_task`, when the pool started it.
    pub fn key_of(&self, ledger_task: &TaskId) -> Option<String> {
        let runs = self.0.runs.lock().unwrap();
        runs.iter().find(|(_, run)| run.current == *ledger_task).map(|(key, _)| key.clone())
    }

    /// The task running in the conversation's own worker, when the pool started it.
    pub fn own_task(&self) -> Option<String> {
        let runs = self.0.runs.lock().unwrap();
        runs.iter().find(|(_, run)| run.settled.is_none() && matches!(run.lane, Lane::Own)).map(|(key, _)| key.clone())
    }

    /// A task's run ended. Returns what the principal reviews: the brief, the workspace, the
    /// round, and how it came out. None for a task the pool does not know.
    pub fn settled(&self, task: &str, outcome: &Result<RunOutcome, String>) -> Option<Review> {
        let mut runs = self.0.runs.lock().unwrap();
        let run = runs.get_mut(task)?;
        let settlement = Settlement::of(outcome);
        run.settled = Some(settlement.clone());
        Some(Review {
            task: task.to_string(),
            ledger_task: run.current.clone(),
            brief: run.brief.clone(),
            workspace: run.workspace.clone(),
            round: run.round,
            pulse: run.pulse.clone(),
            settlement,
        })
    }

    /// Send a settled task back to the agent that reported on it, with what its report lacks.
    /// The run continues in the same runtime, so the agent still has what it learned; it is a
    /// new task in the ledger. Returns the round this makes.
    pub fn continue_task(
        &self,
        task: &str,
        critique: &str,
        max_rounds: u32,
        source: PulseSource,
        cause: Option<PulseId>,
    ) -> Result<u32, String> {
        let mut runs = self.0.runs.lock().unwrap();
        let Some(run) = runs.get_mut(task.trim()) else { return Err(format!("No task {task} here. {}", list(&runs))) };
        if run.settled.is_none() {
            return Err(format!("Task {task} is still working; steer it instead."));
        }
        if run.round >= max_rounds {
            return Err(format!(
                "Task {task} was already sent back {} times (tasks.maxReviewRounds). Tell the user candidly what \
                 was achieved and what is still open.",
                run.round
            ));
        }
        if critique.trim().is_empty() {
            return Err("`critique` must say what the report lacks".into());
        }
        let round = run.round + 1;
        let next = ledger::new_task_id();
        let text = format!(
            "[Review of your report on this task, round {round} of {max_rounds}. This is your principal, not the \
             user.] The report is not accepted yet:\n{}\n\nGo on with the task. Its brief stands{}. Report again \
             when what counts as done is shown, or when you are at something only the user can do, with what you \
             tried, what you verified, and the options.",
            critique.trim(),
            run.workspace.as_deref().map(|w| format!(" ({w}/brief.md)")).unwrap_or_default()
        );
        let setup = RunSetup {
            id: Some(next.clone()),
            workspace: run.workspace.clone(),
            focus: run.brief.place.clone(),
            fresh_session: false,
            goal: run.brief.goal.clone(),
            activation: run.brief.spawned_by_activation.clone(),
        };
        let worker = match &run.lane {
            Lane::Own => &self.0.own,
            Lane::Beside(worker) => worker,
        };
        run.pulse = worker.start_with(&text, source, cause, setup)?;
        run.round = round;
        run.current = next;
        run.settled = None;
        Ok(round)
    }

    /// Whether a task other than `task` that goes with it (serves the same goal, or was started
    /// by the same activation) is working.
    pub fn siblings_working(&self, task: &str, brief: &TaskBrief) -> bool {
        let same = |a: &Option<String>, b: &Option<String>| a.is_some() && a == b;
        self.0.runs.lock().unwrap().iter().any(|(id, run)| {
            id != task
                && run.settled.is_none()
                && (same(&run.brief.goal, &brief.goal)
                    || same(&run.brief.spawned_by_activation, &brief.spawned_by_activation))
        })
    }

    /// Whether the task is working (again): it was started, or sent back, and has not settled.
    pub fn is_working(&self, task: &str) -> bool {
        self.0.runs.lock().unwrap().get(task).is_some_and(|run| run.settled.is_none())
    }

    /// The principal is through with a task: its runtime beside the conversation's is let go.
    pub fn release(&self, task: &str) {
        let mut runs = self.0.runs.lock().unwrap();
        if runs.get(task).is_some_and(|run| run.settled.is_some()) {
            runs.remove(task);
        }
    }
}

/// What the principal is given to review when a task settles.
#[derive(Clone, Debug)]
pub struct Review {
    pub task: String,
    /// The ledger task of the run that just ended (`nypi tasks show` reads its report).
    pub ledger_task: TaskId,
    /// The pulse that started that run.
    pub pulse: PulseId,
    pub brief: TaskBrief,
    pub workspace: Option<String>,
    /// Times it was sent back before this settlement.
    pub round: u32,
    pub settlement: Settlement,
}

fn describe(id: &str, run: &Run, worker: &WorkerHandle) -> String {
    let head = format!("Task {id}: {}", run.brief.name());
    let workspace = run.workspace.as_deref().map(|w| format!("\nWorkspace: {w}/")).unwrap_or_default();
    match &run.settled {
        Some(settled) => format!("{head}\nSettled ({}), waiting for review.{workspace}", settled.status),
        None => {
            let state = worker.current().map(|info| info.describe()).unwrap_or_else(|| "Starting.".into());
            let round = if run.round > 0 { format!(" (sent back {} time(s))", run.round) } else { String::new() };
            format!("{head}{round}\n{state}{workspace}")
        }
    }
}

fn list(runs: &BTreeMap<String, Run>) -> String {
    if runs.is_empty() {
        return "No tasks are known here.".into();
    }
    let known: Vec<String> = runs.iter().map(|(id, run)| format!("{id} ({})", run.brief.name())).collect();
    format!("Known tasks: {}.", known.join(", "))
}
