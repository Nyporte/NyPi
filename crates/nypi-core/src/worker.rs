//! The worker module: the full runtime (strong model, every tool, the Inspector's guard and
//! approvals) in a task of its own, so the chat module can keep talking while it works
//! (docs/architecture.md §4.2, principle A5). Other modules drive it through `WorkerHandle`;
//! each instruction is published as a pulse first (`delegated`, or `userMessage` when the user
//! drives the worker directly), so the run traces back to it.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::future::BoxFuture;
use nypi_agent::{AgentEvent, AgentHandle};
use nypi_ai::Message;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

use crate::inspector::call_summary;
use crate::ledger::TaskRecord;
use crate::pulse::{Delivery, PulseBus, PulseId, PulsePayload, PulseSource};
use crate::runtime::{Job, RunOutcome, Runtime, RuntimeEvent};

/// Recent tool calls kept for status reports.
const RECENT_TOOLS: usize = 8;

#[derive(Debug)]
pub enum WorkerEvent {
    Runtime(RuntimeEvent),
    Started {
        task: String,
        pulse: PulseId,
    },
    Settled {
        task: String,
        pulse: PulseId,
        outcome: Result<RunOutcome, String>,
    },
    /// An event of a task that runs beside the conversation's own worker, in a runtime of its
    /// own (the task pool, decision D78); `task` is the id the task was started under.
    Beside {
        task: String,
        event: Box<WorkerEvent>,
    },
}

/// What a run is started with besides its text.
#[derive(Clone, Debug, Default)]
pub struct RunSetup {
    /// The id the run's task is recorded under in the ledger, when it was named beforehand.
    pub id: Option<crate::ledger::TaskId>,
    /// The task's workspace in the vault, pinned into the prompt while it runs.
    pub workspace: Option<String>,
    /// Where the run works, when not at the runtime's own place (decision D79).
    pub focus: Option<crate::place::Focus>,
    /// Begin a new session first: the run is for another project than the runtime's last one
    /// (decision D82), whose transcript would only mislead it.
    pub fresh_session: bool,
    /// The goal the run's task serves, and the Principal's activation that started it (D85).
    pub goal: Option<String>,
    pub activation: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolState {
    Running,
    Ok,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolActivity {
    pub call_id: String,
    pub tool: String,
    pub summary: String,
    pub state: ToolState,
}

/// The task the worker is running.
#[derive(Clone, Debug)]
pub struct TaskInfo {
    /// The task's id in the ledger, when it was named before the run began.
    pub id: Option<crate::ledger::TaskId>,
    pub task: String,
    pub pulse: PulseId,
    pub started: Instant,
    pub tools: Vec<ToolActivity>,
}

impl TaskInfo {
    /// A few lines for the chat module's prompt and the `task_status` tool.
    pub fn describe(&self) -> String {
        let mut text = format!("Running for {} s: {}", self.started.elapsed().as_secs(), self.task);
        if self.tools.is_empty() {
            text.push_str("\nNo tool calls yet (the worker is thinking).");
        } else {
            text.push_str("\nRecent tool calls, oldest first:");
            for tool in &self.tools {
                let state = match tool.state {
                    ToolState::Running => "running",
                    ToolState::Ok => "ok",
                    ToolState::Failed => "failed",
                };
                text.push_str(&format!("\n- {} {} ({state})", tool.tool, tool.summary));
            }
        }
        text
    }
}

type WorkerJob = Box<dyn for<'a> FnOnce(&'a mut Runtime) -> BoxFuture<'a, ()> + Send>;

enum Command {
    Run { task: String, pulse: PulseId, setup: RunSetup },
    Resume { task: String, pulse: PulseId, id: crate::ledger::TaskId },
    With(WorkerJob),
}

#[derive(Clone)]
pub struct WorkerHandle {
    commands: mpsc::UnboundedSender<Command>,
    state: Arc<Mutex<Option<TaskInfo>>>,
    agent: AgentHandle,
    bus: PulseBus,
    /// A job (`with`) is running, such as a user's shell command or a compaction.
    job_running: Arc<std::sync::atomic::AtomicBool>,
    /// The runtime's session, as of its last command: a new session or a switch changes it.
    session: Arc<Mutex<String>>,
    /// Where every event of the worker goes; the task pool adds the events of the runs beside it.
    events: mpsc::UnboundedSender<WorkerEvent>,
    /// The worker's Inspector: runs beside it ask the same approver and raise their alerts here.
    inspector: Arc<crate::inspector::Inspector>,
    /// What a runtime working beside this one is made from (`Runtime::beside`).
    template: Arc<crate::runtime::RuntimeOptions>,
}

impl WorkerHandle {
    /// The running task, or None when idle.
    pub fn current(&self) -> Option<TaskInfo> {
        self.state.lock().unwrap().clone()
    }

    pub fn describe(&self) -> String {
        self.current().map(|t| t.describe()).unwrap_or_else(|| "Idle.".into())
    }

    /// The id of the worker's session (the conversation the chat module's history belongs to).
    pub fn session_id(&self) -> String {
        self.session.lock().unwrap().clone()
    }

    /// Start a task. Fails when one is already running, so a second request is never silently
    /// merged into the first; the caller steers or queues instead.
    pub fn start(&self, task: &str, source: PulseSource, cause: Option<PulseId>) -> Result<PulseId, String> {
        self.start_with(task, source, cause, RunSetup::default())
    }

    /// `start`, with the task's id and workspace given (the task pool names both beforehand).
    pub fn start_with(
        &self,
        task: &str,
        source: PulseSource,
        cause: Option<PulseId>,
        setup: RunSetup,
    ) -> Result<PulseId, String> {
        let mut state = self.state.lock().unwrap();
        if let Some(current) = state.as_ref() {
            return Err(format!("The worker is busy: {}", current.describe()));
        }
        let pulse = self.bus.publish(source.clone(), cause, instruction(&source, Delivery::Prompt, task));
        *state = Some(TaskInfo {
            id: setup.id.clone(),
            task: task.to_string(),
            pulse: pulse.clone(),
            started: Instant::now(),
            tools: Vec::new(),
        });
        drop(state);
        if self.commands.send(Command::Run { task: task.to_string(), pulse: pulse.clone(), setup }).is_err() {
            *self.state.lock().unwrap() = None;
            return Err("The worker has stopped".into());
        }
        Ok(pulse)
    }

    /// Resume a task a dead process left unfinished (see `Runtime::resume_task`). Fails while
    /// another task runs.
    pub fn resume(&self, record: &TaskRecord) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if let Some(current) = state.as_ref() {
            return Err(format!("The worker is busy: {}", current.describe()));
        }
        // The resumed run traces back to the pulse that started the task.
        let pulse = record.pulse.clone().unwrap_or_else(|| PulseId(record.id.0.clone()));
        let task = format!("(resumed) {}", record.goal);
        *state = Some(TaskInfo {
            id: Some(record.id.clone()),
            task: task.clone(),
            pulse: pulse.clone(),
            started: Instant::now(),
            tools: Vec::new(),
        });
        drop(state);
        if self.commands.send(Command::Resume { task, pulse, id: record.id.clone() }).is_err() {
            *self.state.lock().unwrap() = None;
            return Err("The worker has stopped".into());
        }
        Ok(())
    }

    /// Add an instruction to the running task: `Steer` lands after the current step, `FollowUp`
    /// when the task would otherwise finish. Fails when idle.
    pub fn instruct(
        &self,
        text: &str,
        delivery: Delivery,
        source: PulseSource,
        cause: Option<PulseId>,
    ) -> Result<PulseId, String> {
        if self.current().is_none() {
            return Err("No task is running; start one instead".into());
        }
        let pulse = self.bus.publish(source.clone(), cause, instruction(&source, delivery, text));
        let message = Message::user_text(text);
        match delivery {
            Delivery::FollowUp => self.agent.follow_up(message),
            _ => self.agent.steer(message),
        }
        Ok(pulse)
    }

    /// Where the worker's events go, for whoever adds events of runs beside it.
    pub(crate) fn events(&self) -> &mpsc::UnboundedSender<WorkerEvent> {
        &self.events
    }

    pub(crate) fn inspector(&self) -> &Arc<crate::inspector::Inspector> {
        &self.inspector
    }

    /// The options a runtime beside this one is made from, on a bus thread of its own.
    pub(crate) fn beside(&self, thread: &str) -> crate::runtime::RuntimeOptions {
        crate::runtime::RuntimeOptions { pulses: Some(self.bus.root().scoped(thread)), ..(*self.template).clone() }
    }

    /// Take back the instructions waiting for the running task: (steering, follow-ups).
    pub fn clear_queue(&self) -> (Vec<Message>, Vec<Message>) {
        (self.agent.steering.take_all(), self.agent.follow_up.take_all())
    }

    /// Cancel the running task, or the running job (a user's shell command, a compaction);
    /// returns false when there was neither.
    pub fn abort(&self) -> bool {
        let running = self.current().is_some() || self.job_running.load(std::sync::atomic::Ordering::SeqCst);
        if running {
            self.agent.abort();
        }
        running
    }

    /// Run `job` on the runtime once the worker is idle (commands such as /model or /compact).
    /// None when the worker has stopped.
    pub async fn with<R: Send + 'static>(
        &self,
        job: impl for<'a> FnOnce(&'a mut Runtime) -> BoxFuture<'a, R> + Send + 'static,
    ) -> Option<R> {
        let (tx, rx) = oneshot::channel();
        let job: WorkerJob = Box::new(move |runtime| {
            Box::pin(async move {
                let _ = tx.send(job(runtime).await);
            })
        });
        self.commands.send(Command::With(job)).ok()?;
        rx.await.ok()
    }
}

/// A user typing straight to the worker is a user message; anything else is a delegation.
fn instruction(source: &PulseSource, delivery: Delivery, text: &str) -> PulsePayload {
    let text = text.to_string();
    match source {
        PulseSource::UserInput { .. } => PulsePayload::UserMessage { delivery, text },
        _ => PulsePayload::Delegated { delivery, text },
    }
}

/// Resume the newest task a dead process left in this session; returns a note for the user, or
/// None when nothing was interrupted. Older interrupted tasks stay listed in `nypi tasks`.
pub fn resume_interrupted(worker: &WorkerHandle, interrupted: &[TaskRecord]) -> Option<String> {
    let newest = interrupted.last()?;
    let older = interrupted.len() - 1;
    let more = if older > 0 { format!(" ({older} older interrupted task(s): nypi tasks)") } else { String::new() };
    Some(match worker.resume(newest) {
        Ok(()) => {
            format!("Resuming interrupted task {} (attempt {}): {}{more}", newest.id, newest.attempts + 1, newest.goal)
        }
        Err(error) => format!("Could not resume interrupted task {}: {error}", newest.id),
    })
}

/// Move the runtime into the worker task. Events of every run arrive on the returned receiver.
pub fn spawn(runtime: Runtime) -> (WorkerHandle, mpsc::UnboundedReceiver<WorkerEvent>) {
    spawn_then(runtime, |_| Box::pin(async {}))
}

/// `spawn`, with `first` done on the runtime in the worker's own task before any command:
/// what may take long (connecting MCP servers) without the caller waiting for it. Commands
/// sent meanwhile wait their turn behind it (decision D82).
pub fn spawn_then(
    runtime: Runtime,
    first: impl for<'a> FnOnce(&'a mut Runtime) -> BoxFuture<'a, ()> + Send + 'static,
) -> (WorkerHandle, mpsc::UnboundedReceiver<WorkerEvent>) {
    let (commands, mut inbox) = mpsc::unbounded_channel();
    let (events, receiver) = mpsc::unbounded_channel();
    let state = Arc::new(Mutex::new(None));
    let job_running = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let session = Arc::new(Mutex::new(runtime.session_id()));
    let handle = WorkerHandle {
        commands,
        state: state.clone(),
        agent: runtime.handle(),
        bus: runtime.pulses().clone(),
        job_running: job_running.clone(),
        session: session.clone(),
        events: events.clone(),
        inspector: runtime.inspector(),
        template: Arc::new(runtime.beside("")),
    };
    tokio::spawn(async move {
        let mut runtime = runtime;
        job_running.store(true, std::sync::atomic::Ordering::SeqCst);
        first(&mut runtime).await;
        job_running.store(false, std::sync::atomic::Ordering::SeqCst);
        while let Some(command) = inbox.recv().await {
            match command {
                Command::Run { task, pulse, setup } => {
                    if setup.fresh_session
                        && let Err(error) = runtime.new_session()
                    {
                        let note = format!("a fresh session for this task could not be begun: {error:#}");
                        runtime
                            .inspector()
                            .alert(crate::inspector::Alert::new(crate::inspector::AlertLevel::Warning, note));
                    }
                    runtime.set_workspace(setup.workspace);
                    runtime.set_campaign(setup.goal, setup.activation);
                    runtime.set_focus(setup.focus);
                    if let Some(id) = setup.id {
                        runtime.preassign_task(id);
                    }
                    let _ = events.send(WorkerEvent::Started { task: task.clone(), pulse: pulse.clone() });
                    let outcome = run(&mut runtime, Work::Task(&task, pulse.clone()), &state, &events).await;
                    *state.lock().unwrap() = None;
                    let _ = events.send(WorkerEvent::Settled { task, pulse, outcome });
                }
                Command::Resume { task, pulse, id } => {
                    let _ = events.send(WorkerEvent::Started { task: task.clone(), pulse: pulse.clone() });
                    let outcome = run(&mut runtime, Work::Resume(&id), &state, &events).await;
                    *state.lock().unwrap() = None;
                    let _ = events.send(WorkerEvent::Settled { task, pulse, outcome });
                }
                Command::With(job) => {
                    job_running.store(true, std::sync::atomic::Ordering::SeqCst);
                    job(&mut runtime).await;
                    job_running.store(false, std::sync::atomic::Ordering::SeqCst);
                }
            }
            *session.lock().unwrap() = runtime.session_id();
        }
    });
    (handle, receiver)
}

enum Work<'a> {
    Task(&'a str, PulseId),
    Resume(&'a crate::ledger::TaskId),
}

async fn run(
    runtime: &mut Runtime,
    work: Work<'_>,
    state: &Mutex<Option<TaskInfo>>,
    events: &mpsc::UnboundedSender<WorkerEvent>,
) -> Result<RunOutcome, String> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let agent = runtime.handle();
    let run = async {
        match work {
            Work::Task(task, pulse) => runtime.prompt_caused(Message::user_text(task), pulse, &tx).await,
            Work::Resume(id) => runtime.run_job(&Job::Resume(id.clone()), &tx).await,
        }
    };
    tokio::pin!(run);
    let result = loop {
        tokio::select! {
            result = &mut run => break result,
            Some(event) = rx.recv() => {
                observe(state, &event);
                let _ = events.send(WorkerEvent::Runtime(event));
            }
        }
    };
    while let Ok(event) = rx.try_recv() {
        let _ = events.send(WorkerEvent::Runtime(event));
    }
    // Queued instructions that never reached the run are reported, not lost silently.
    let dropped = agent.take_queued();
    result.map_err(|e| format!("{e:#}")).map(|mut outcome| {
        if !dropped.is_empty() {
            outcome.answer.push_str(&format!("\n\n[{} queued instruction(s) were not delivered]", dropped.len()));
        }
        outcome
    })
}

fn observe(state: &Mutex<Option<TaskInfo>>, event: &RuntimeEvent) {
    let RuntimeEvent::Agent { event } = event else { return };
    let mut state = state.lock().unwrap();
    let Some(task) = state.as_mut() else { return };
    match event {
        AgentEvent::ToolExecutionStart { call_id, tool_name, arguments } => {
            task.tools.push(ToolActivity {
                call_id: call_id.clone(),
                tool: tool_name.clone(),
                summary: call_summary(tool_name, arguments).chars().take(120).collect(),
                state: ToolState::Running,
            });
            if task.tools.len() > RECENT_TOOLS {
                task.tools.remove(0);
            }
        }
        AgentEvent::ToolExecutionEnd { call_id, output, .. } => {
            if let Some(tool) = task.tools.iter_mut().find(|t| &t.call_id == call_id) {
                tool.state = if output.is_error { ToolState::Failed } else { ToolState::Ok };
            }
        }
        _ => {}
    }
}
