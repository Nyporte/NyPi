//! The `delegate` tool (principle B2, docs/architecture.md §4.5): the agent hands a task to a
//! specialist and gets its report back, to review before reporting it. Each delegation is a
//! child task in the ledger. A native specialist runs as a nested runtime, a thread of its own
//! on the hub's pulse bus, whose approval requests go to the same person as the delegating
//! agent's; an external one runs its CLI. Specialists cannot delegate further, so delegation is
//! one level deep.
//!
//! In a conversation of the hub a delegation runs in the background (decision D67, part 4): the
//! call returns at once, so the conversation is free for the user and for other work, and
//! several specialists work at the same time. When one finishes, its report is published as a
//! `reported` pulse in the delegating thread, and the hub hands it to the conversation as a
//! task of its own, for the agent to review and pass on. `wait: true` keeps the old behaviour
//! for a result the run needs now; so does a runtime that is not a conversation (one printed
//! answer, a job), since nothing would be there to take the report.
//!
//! Several delegations may race on one question (`race`, decision D83): they start together,
//! the first to come through without failing is reported and stops the others, and only when
//! every one has failed is that reported instead. A run that ends waiting on a race waits on
//! the first of it (`WaitPredicate::AnyOf`). A delegation works where the delegating run does:
//! a native specialist at the run's focus, on another device too, through the same tools; an
//! external one is a program of this computer and is not run for a folder on another device.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nypi_agent::{AgentEvent, Tool, ToolContext, ToolOutput};
use nypi_ai::{Model, StreamFn, ToolDefinition};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Paths;
use crate::inspector::{Alert, Inspector, call_summary};
use crate::ledger::TaskId;
use crate::pulse::{PulseBus, PulsePayload, PulseSource};
use crate::runtime::{Runtime, RuntimeEvent};
use crate::specialist::{Specialist, SpecialistEvent, SpecialistKind, run_external};
use crate::status::{RunStatus, StatusKind};

/// Tool calls listed in the report, newest last.
const ACTIVITY_LINES: usize = 12;
/// How much of its own instruction the agent is shown again with a background report.
const TASK_REMINDER_CHARS: usize = 600;

/// What a runtime and its `delegate` tool share about delegations that outlive the run that
/// started them. Dropped with the runtime, which stops the specialists still working for it.
pub struct Background {
    /// This runtime is a conversation, and something brings reports back to it.
    allowed: AtomicBool,
    /// How many are started and not reported yet.
    pending: Arc<AtomicUsize>,
    /// Which: the way back of a run that ends `Continuing` while they work (decision D59).
    working: Arc<Mutex<Vec<String>>>,
    cancel: CancellationToken,
    /// The races started, each as its delegations' ids.
    races: Mutex<Vec<Vec<String>>>,
}

/// Delegations racing on one question (decision D83).
struct Race {
    members: Vec<(String, CancellationToken)>,
    won: AtomicBool,
    /// Members not finished yet.
    left: AtomicUsize,
    /// What the ones that failed said, for the report when all of them have.
    failures: Mutex<Vec<String>>,
}

impl Background {
    pub fn new() -> Arc<Self> {
        Arc::new(Background {
            allowed: AtomicBool::new(false),
            pending: Arc::default(),
            working: Arc::default(),
            cancel: CancellationToken::new(),
            races: Mutex::default(),
        })
    }

    /// What a run that ends waiting on `wake` waits for, when its delegations are one race:
    /// the first of them. None otherwise (the ledger's own rule then applies).
    pub fn predicate_for(&self, wake: &[String]) -> Option<crate::ledger::WaitPredicate> {
        if wake.is_empty() {
            return None;
        }
        let races = self.races.lock().unwrap();
        if !races.iter().any(|race| wake.iter().all(|id| race.contains(id))) {
            return None;
        }
        Some(crate::ledger::WaitPredicate::AnyOf {
            tokens: wake.iter().map(|id| crate::ledger::WaitToken::Delegation(id.clone())).collect(),
        })
    }

    /// Reports have somewhere to go from now on: delegations may run in the background.
    pub fn allow(&self) {
        self.allowed.store(true, Ordering::SeqCst);
    }

    /// The count of delegations started and not reported yet, to watch from outside.
    pub fn pending(&self) -> Arc<AtomicUsize> {
        self.pending.clone()
    }

    /// The delegations still working, whichever run started them: any of them brings the agent
    /// back when it reports.
    pub fn working(&self) -> Vec<String> {
        self.working.lock().unwrap().clone()
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// How a delegation's id begins: a waiting task's wake that is a specialist, not a schedule.
pub const DELEGATION_PREFIX: &str = "d-";

pub struct DelegateTool {
    pub(crate) paths: Paths,
    pub(crate) cwd: PathBuf,
    /// The roster, shared with the runtime, which reloads it when the folder's profiles change.
    pub(crate) specialists: Arc<std::sync::RwLock<Vec<Specialist>>>,
    /// The delegating runtime's current task: the parent of each delegation.
    pub(crate) task: Arc<Mutex<Option<TaskId>>>,
    pub(crate) inspector: Arc<Inspector>,
    /// Passed on to native specialists (tests use a scripted stream).
    pub(crate) stream_fn: Option<StreamFn>,
    pub(crate) extra_model: Option<Model>,
    /// The delegating thread's handle on the hub's bus.
    pub(crate) bus: PulseBus,
    pub(crate) background: Arc<Background>,
    /// The delegating runtime's place, when it is routed: a delegation works at its focus.
    pub(crate) place: Option<Arc<crate::place::Place>>,
}

#[async_trait]
impl Tool for DelegateTool {
    fn definition(&self) -> ToolDefinition {
        let specialists = self.specialists.read().unwrap();
        let names: Vec<&str> = specialists.iter().map(|s| s.name.as_str()).collect();
        let roster: Vec<String> =
            specialists.iter().map(|s| format!("{}: {}", s.name, s.profile.description)).collect();
        let how = match self.background.allowed.load(Ordering::SeqCst) {
            true => {
                "Hand a task to a specialist. It works in the background: this call returns at once, and its report \
                 comes to you later as a message of its own, so start every specialist you need now and go on with \
                 other work, or end your turn with `Continuing:` and what is under way. Set `wait` only when you \
                 cannot go on without the result."
            }
            false => "Hand a task to a specialist and wait for its report.",
        };
        let mut properties = json!({
            "specialist": { "type": "string", "enum": names },
            "task": { "type": "string", "description": "Complete, self-contained instruction" },
            "cwd": { "type": "string", "description": "Working directory for the specialist (default: yours)" }
        });
        if self.background.allowed.load(Ordering::SeqCst) {
            properties["wait"] =
                json!({ "type": "boolean", "description": "Wait for the report in this call (default false)" });
            properties["race"] = json!({
                "type": "array",
                "description": "Instead of one specialist: two or more ways to the same answer, started together. \
                                The first to come through without failing is reported and the others are stopped.",
                "items": {
                    "type": "object",
                    "required": ["specialist", "task"],
                    "properties": { "specialist": { "type": "string", "enum": names }, "task": { "type": "string" } }
                }
            });
        }
        ToolDefinition {
            name: "delegate".into(),
            description: format!(
                "{how} Professional work in a specialist's domain goes to that specialist. It does not see this \
                 conversation: give a complete, self-contained task with the goal, constraints, files, and what done \
                 means. You stay accountable: review its report before you report it. A small change you already \
                 understand (about 20 lines or fewer in one or two files, nothing left to design or investigate) is \
                 faster done yourself: a delegation takes minutes. Specialists: {}",
                roster.join("; ")
            ),
            parameters: json!({ "type": "object", "properties": properties }),
        }
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        if let Some(probes) = args["race"].as_array() {
            return Ok(self.race(probes, &args));
        }
        let name = args["specialist"].as_str().unwrap_or_default();
        let task = args["task"].as_str().map(str::trim).unwrap_or_default();
        let work = match self.work(name, task, &args) {
            Ok(work) => work,
            Err(error) => return Ok(ToolOutput::error(error)),
        };
        let wait = args["wait"].as_bool().unwrap_or(false) || !self.background.allowed.load(Ordering::SeqCst);
        if wait {
            let mut activity: Vec<String> = Vec::new();
            return Ok(match work.run(&ctx.cancel, Some(&ctx), &mut activity).await {
                Ok(report) => render(name, &report, &activity),
                Err(error) => ToolOutput::error(format!("Delegation to {name} failed: {error:#}")),
            });
        }
        let id = new_delegation();
        self.spawn_background(work, id.clone(), self.background.cancel.clone(), None);
        Ok(ToolOutput::text(format!(
            "Started {name} in the background ({id}). Its report will come to you as a message of its own when it \
             finishes; do not wait for it and do not ask about it. Go on with other work, or end your turn with \
             `Continuing:` and say what is under way."
        ))
        .with_details(json!({ "specialist": name, "delegation": id, "background": true })))
    }
}

fn new_delegation() -> String {
    format!("{DELEGATION_PREFIX}{}", &uuid::Uuid::new_v4().simple().to_string()[..10])
}

impl DelegateTool {
    /// One delegation of `task` to the specialist `name`, ready to run where the delegating run
    /// works, or why it cannot be.
    fn work(&self, name: &str, task: &str, args: &Value) -> Result<Work, String> {
        let found = self.specialists.read().unwrap().iter().find(|s| s.name == name).cloned();
        let Some(specialist) = found else {
            let specialists = self.specialists.read().unwrap();
            let names: Vec<&str> = specialists.iter().map(|s| s.name.as_str()).collect();
            return Err(format!("No specialist named {name:?}. Available: {}", names.join(", ")));
        };
        if task.is_empty() {
            return Err("`task` must be a complete instruction for the specialist".into());
        }
        let focus = self.place.as_ref().and_then(|place| place.focus());
        if let Some(device) = focus.as_ref().and_then(|focus| focus.device.as_deref())
            && specialist.profile.kind == SpecialistKind::External
        {
            return Err(format!(
                "{name} is a program of this computer, and this run works on {device}; it is not run for a folder \
                 there. Use a native specialist, or do it with the tools here."
            ));
        }
        // Where the run works, when that is a folder of this computer other than the runtime's own.
        let here = match &focus {
            Some(crate::place::Focus { device: None, folder }) => PathBuf::from(folder),
            _ => self.cwd.clone(),
        };
        let cwd = match args["cwd"].as_str().filter(|c| !c.trim().is_empty()) {
            // Resolved as the guard resolved it, then followed through symlinks: an external
            // specialist runs outside the sandbox, so it must start in the folder that was checked.
            Some(dir) => crate::protected::real_path(&crate::tools::resolve_path(&here, dir)),
            None => here,
        };
        if !cwd.is_dir() {
            return Err(format!("{} is not a directory", cwd.display()));
        }
        Ok(Work {
            paths: self.paths.clone(),
            specialist,
            cwd,
            task: task.to_string(),
            parent: self.task.lock().unwrap().clone(),
            inspector: self.inspector.clone(),
            stream_fn: self.stream_fn.clone(),
            extra_model: self.extra_model.clone(),
            bus: self.bus.clone(),
            devices: self.place.as_ref().and_then(|place| place.lookup()),
            focus,
        })
    }

    /// Start a race (decision D83): every probe in the background at once.
    fn race(&self, probes: &[Value], args: &Value) -> ToolOutput {
        if !self.background.allowed.load(Ordering::SeqCst) {
            return ToolOutput::error("a race runs in the background, which this run cannot wait for: delegate one");
        }
        if probes.len() < 2 {
            return ToolOutput::error("a race needs two or more probes; for one, give `specialist` and `task`");
        }
        let mut works = Vec::new();
        for probe in probes {
            let name = probe["specialist"].as_str().unwrap_or_default();
            let task = probe["task"].as_str().map(str::trim).unwrap_or_default();
            match self.work(name, task, args) {
                Ok(work) => works.push(work),
                Err(error) => return ToolOutput::error(format!("The race was not started: {error}")),
            }
        }
        let members: Vec<(String, CancellationToken)> =
            works.iter().map(|_| (new_delegation(), self.background.cancel.child_token())).collect();
        let race = Arc::new(Race {
            members: members.clone(),
            won: AtomicBool::new(false),
            left: AtomicUsize::new(works.len()),
            failures: Mutex::default(),
        });
        let ids: Vec<String> = members.iter().map(|(id, _)| id.clone()).collect();
        self.background.races.lock().unwrap().push(ids.clone());
        let mut started = Vec::new();
        for (work, (id, cancel)) in works.into_iter().zip(members) {
            started.push(format!("{} ({id})", work.specialist.name));
            self.spawn_background(work, id, cancel, Some(race.clone()));
        }
        ToolOutput::text(format!(
            "Started a race of {}: {}. The first to come through without failing is reported to you as a message of \
             its own, and the others are stopped; you hear of a failure only if every one fails. Go on with other \
             work, or end your turn with `Continuing:`.",
            started.len(),
            started.join(", ")
        ))
        .with_details(json!({ "race": ids, "background": true }))
    }

    /// Run `work` in the background as delegation `id`, until `cancel`, and report it: alone,
    /// or as a member of `race`.
    fn spawn_background(&self, work: Work, id: String, cancel: CancellationToken, race: Option<Arc<Race>>) {
        let name = work.specialist.name.clone();
        self.background.working.lock().unwrap().push(id.clone());
        self.background.pending.fetch_add(1, Ordering::SeqCst);
        self.inspector.record("delegate", json!({ "background": &id, "specialist": name, "race": race.is_some() }));
        let (pending, cause) = (self.background.pending(), self.inspector.cause());
        let (delegation, working) = (id, self.background.working.clone());
        tokio::spawn(async move {
            let mut activity: Vec<String> = Vec::new();
            let name = work.specialist.name.clone();
            let (source, status, report) = match work.run(&cancel, None, &mut activity).await {
                Ok(report) => (
                    PulseSource::TaskEvent { task: report.task.0.clone() },
                    report.status.clone(),
                    render_text(&name, &report, &activity),
                ),
                Err(error) => (
                    PulseSource::module("delegate"),
                    RunStatus { kind: StatusKind::Failed, text: format!("{error:#}"), synthesized: true },
                    format!("Delegation to {name} failed: {error:#}"),
                ),
            };
            let asked: String = work.task.chars().take(TASK_REMINDER_CHARS).collect();
            let mut text = format!(
                "The specialist {name} you started in the background ({delegation}) has finished. This is its report, \
                 not a message from the user. You gave it this task:\n{asked}\n\n{report}\n\nThen tell the user what \
                 came of it."
            );
            working.lock().unwrap().retain(|id| *id != delegation);
            pending.fetch_sub(1, Ordering::SeqCst);
            if let Some(race) = race {
                let last = race.left.fetch_sub(1, Ordering::SeqCst) == 1;
                let failed = status.kind == StatusKind::Failed;
                if !failed && !race.won.swap(true, Ordering::SeqCst) {
                    // The first through: the others are no longer needed.
                    let others: Vec<&str> = race
                        .members
                        .iter()
                        .filter(|(other, _)| *other != delegation)
                        .map(|(other, cancel)| {
                            cancel.cancel();
                            other.as_str()
                        })
                        .collect();
                    text.push_str(&format!(
                        "\n\nIt was the first of a race to come through; the others ({}) were stopped.",
                        others.join(", ")
                    ));
                } else if failed && last && !race.won.load(Ordering::SeqCst) {
                    race.failures.lock().unwrap().push(format!("{name} ({delegation}): {status}"));
                    let all = race.failures.lock().unwrap().join("\n");
                    text = format!(
                        "Every delegation of the race you started has failed. This is not a message from the user. \
                         What each said:\n{all}\n\nThe last one's report:\n{report}\n\nThen tell the user what came \
                         of it, and what you would try next."
                    );
                } else {
                    if failed {
                        race.failures.lock().unwrap().push(format!("{name} ({delegation}): {status}"));
                    }
                    // Stopped, failed with others still at it, or through after the winner.
                    return;
                }
            }
            // Said in the delegating thread: the hub hands it to the conversation as a task of its own.
            work.bus.publish(source, cause, PulsePayload::Reported { delegation, specialist: name, status, text });
        });
    }
}

/// One delegation, with everything it needs to run apart from the call that started it.
struct Work {
    paths: Paths,
    specialist: Specialist,
    cwd: PathBuf,
    task: String,
    parent: Option<TaskId>,
    inspector: Arc<Inspector>,
    stream_fn: Option<StreamFn>,
    extra_model: Option<Model>,
    bus: PulseBus,
    /// How the delegating run reaches the hub's devices, and where it works (decision D83).
    devices: Option<Arc<dyn crate::place::DeviceLookup>>,
    focus: Option<crate::place::Focus>,
}

impl Work {
    /// Run the specialist to its report. `ctx` is the waiting call, shown the progress.
    async fn run(
        &self,
        cancel: &CancellationToken,
        ctx: Option<&ToolContext>,
        activity: &mut Vec<String>,
    ) -> anyhow::Result<Report> {
        let specialist = &self.specialist;
        match specialist.profile.kind {
            SpecialistKind::Native => self.run_native(cancel, ctx, activity).await,
            SpecialistKind::External => {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let parent = self.parent.clone();
                let run = run_external(specialist, &self.paths, &self.cwd, &self.task, parent, &tx, cancel.clone());
                tokio::pin!(run);
                let outcome = loop {
                    tokio::select! {
                        outcome = &mut run => break outcome,
                        Some(event) = rx.recv() => progress(ctx, &specialist.name, &event, activity),
                    }
                };
                while let Ok(event) = rx.try_recv() {
                    progress(ctx, &specialist.name, &event, activity);
                }
                outcome.map(|o| Report {
                    task: o.task,
                    status: o.status,
                    answer: o.answer,
                    log: Some(o.log.display().to_string()),
                    sources: Vec::new(),
                })
            }
        }
    }
}

struct Report {
    task: TaskId,
    status: RunStatus,
    answer: String,
    log: Option<String>,
    /// The specialist's own citations, already renamed (`[analyst:t2] read notes.md`).
    sources: Vec<String>,
}

impl Work {
    async fn run_native(
        &self,
        cancel: &CancellationToken,
        ctx: Option<&ToolContext>,
        activity: &mut Vec<String>,
    ) -> anyhow::Result<Report> {
        let specialist = &self.specialist;
        let mut options = specialist.native_options(&self.cwd, self.parent.clone())?;
        options.paths = Some(self.paths.clone());
        options.stream_fn = self.stream_fn.clone();
        options.extra_model = self.extra_model.clone();
        // A thread of its own on the hub's bus, so its pulses are in the same sequence as everyone's.
        options.pulses = Some(self.bus.root().scoped(&format!("delegate:{}", self.cwd.display())));
        // It works where the delegating run works, on another device too, through the same tools.
        options.devices = self.devices.clone();
        let mut runtime = Runtime::new(options)?;
        runtime.set_focus(self.focus.clone());
        runtime.set_task_owner(&specialist.name);
        runtime.set_channel("delegate");
        // The person who answers the delegating agent's approvals answers the specialist's too.
        if let Some(approver) = self.inspector.approver() {
            runtime.inspector().set_approver(approver);
        }
        let mut alerts = runtime.take_alerts();
        let (tx, mut rx) = mpsc::unbounded_channel::<RuntimeEvent>();
        let handle = runtime.handle();
        let run = runtime.prompt(&self.task, &tx);
        tokio::pin!(run);
        let outcome = loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                Some(event) = rx.recv() => {
                    if let RuntimeEvent::Agent { event: AgentEvent::ToolExecutionStart { tool_name, arguments, .. } } = &event {
                        let line = format!("{tool_name} {}", call_summary(tool_name, arguments));
                        progress(ctx, &specialist.name, &SpecialistEvent::Tool { name: line, summary: String::new() }, activity);
                    }
                }
                Some(alert) = recv(&mut alerts) => {
                    self.inspector.alert(Alert::new(alert.level, format!("{}: {}", specialist.name, alert.message)));
                }
                _ = cancel.cancelled() => handle.abort(),
            }
        }?;
        let id = outcome.task.clone().unwrap_or_else(|| TaskId("-".into()));
        if let Err(error) = specialist.record_experience(&id, &outcome.status) {
            self.inspector.alert(Alert::new(
                crate::inspector::AlertLevel::Warning,
                format!("{}: could not update the experience log: {error}", specialist.name),
            ));
        }
        // Its references are its own session's; renamed, they cannot pass for the delegating agent's.
        let sources = outcome
            .citations
            .cited
            .iter()
            .filter_map(|c| match c {
                crate::citations::Citation::Tool { source } => Some(format!(
                    "[{}:{}] {} {}{}",
                    specialist.name,
                    source.reference,
                    source.tool,
                    source
                        .summary
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(120)
                        .collect::<String>(),
                    if source.is_error { " (failed)" } else { "" }
                )),
                _ => None,
            })
            .collect();
        let answer = crate::citations::namespace(&outcome.answer, &specialist.name);
        Ok(Report { task: id, status: outcome.status, answer, log: None, sources })
    }
}

async fn recv(alerts: &mut Option<mpsc::UnboundedReceiver<Alert>>) -> Option<Alert> {
    match alerts {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Show the specialist's tool calls as the delegate call's partial output, and keep them for the report.
fn progress(ctx: Option<&ToolContext>, specialist: &str, event: &SpecialistEvent, activity: &mut Vec<String>) {
    let line = match event {
        SpecialistEvent::Tool { name, summary } if summary.is_empty() => name.clone(),
        SpecialistEvent::Tool { name, summary } => format!("{name} {summary}"),
        SpecialistEvent::ToolDone { is_error: true, .. } => {
            if let Some(last) = activity.last_mut() {
                last.push_str(" (failed)");
            }
            return;
        }
        _ => return,
    };
    let line: String = line.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect();
    if let Some(ctx) = ctx {
        ctx.update(ToolOutput::text(format!("{specialist}: {line}")));
    }
    activity.push(line);
}

fn render(name: &str, report: &Report, activity: &[String]) -> ToolOutput {
    let mut output = ToolOutput::text(render_text(name, report, activity)).with_details(json!({
        "specialist": name,
        "task": report.task,
        "status": report.status,
        "log": report.log,
    }));
    output.is_error = report.status.kind == StatusKind::Failed;
    output
}

fn render_text(name: &str, report: &Report, activity: &[String]) -> String {
    let mut text = format!("Specialist {name} finished task {}: {}", report.task, report.status);
    if report.status.synthesized {
        text.push_str(" (NyPi derived this status; the specialist gave none)");
    }
    if !activity.is_empty() {
        let shown = &activity[activity.len().saturating_sub(ACTIVITY_LINES)..];
        text.push_str(&format!("\nActivity ({} tool calls, latest {}):", activity.len(), shown.len()));
        for line in shown {
            text.push_str(&format!("\n- {line}"));
        }
    }
    if let Some(log) = &report.log {
        text.push_str(&format!("\nFull log: {log}"));
    }
    text.push_str(&format!("\n\n--- Report from {name} ---\n{}\n--- End of report ---", report.answer.trim()));
    if !report.sources.is_empty() {
        text.push_str(&format!("\nSources cited by {name} (cite them as written):"));
        for source in &report.sources {
            text.push_str(&format!("\n- {source}"));
        }
    }
    text.push_str(
        "\n\nReview this before you report it: check the claims you can verify yourself (read changed files, rerun \
         tests or commands), and tell the user what you checked.",
    );
    text
}
