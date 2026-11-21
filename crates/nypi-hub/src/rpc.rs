//! RPC mode: other programs drive NyPi with JSONL commands on stdin and read responses and
//! events on stdout (principle A7). Command and response records follow Pi's RPC mode; event
//! payloads are NyPi's `AgentEvent`s, plus `approval_request` and a status on `agent_settled`
//! (docs/architecture.md, decision D16).

use std::collections::HashMap;
use std::io::{BufRead, Write};

use nypi_agent::{AgentEvent, AgentHandle, MessageQueue};
use nypi_ai::{Message, StopReason, text_of};
use nypi_core::inspector::{Alert, Approval, ApprovalRequest};
use nypi_core::pulse::{Pulse, PulseBus, PulseKind, PulsePayload, PulseSource, Subscription};
use nypi_core::retry::RetryNotice;
use nypi_core::runtime::{InputHandle, Job, Runtime, RuntimeEvent};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::commands::{self, CommandResult};
use crate::protocol::{self, Behavior, HubCommand};

/// Room for a `get_tree` record of a session hundreds of thousands of entries long.
const WRITER_STACK: usize = 256 << 20;

/// How long reflections on the last tasks get to finish once the input has closed.
const REFLECTIONS_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Read commands from stdin and write records to stdout until stdin closes.
pub async fn run(runtime: Runtime) -> anyhow::Result<()> {
    let (out, mut records) = mpsc::unbounded_channel::<Value>();
    // Writing and freeing a record recurse through it, and a `get_tree` record nests as deep as
    // the session is long, so the writer gets a stack that holds very long sessions.
    let writer = std::thread::Builder::new().name("rpc-writer".into()).stack_size(WRITER_STACK).spawn(move || {
        let stdout = std::io::stdout();
        while let Some(record) = records.blocking_recv() {
            let mut lock = stdout.lock();
            // One record per line; stdout carries nothing else in this mode.
            if writeln!(lock, "{record}").and_then(|_| lock.flush()).is_err() {
                return;
            }
        }
    })?;
    serve(runtime, spawn_line_reader(), out).await;
    let _ = tokio::task::spawn_blocking(move || writer.join()).await;
    Ok(())
}

/// The protocol loop, separated from stdio so tests can drive it.
pub async fn serve(runtime: Runtime, commands: mpsc::UnboundedReceiver<String>, out: mpsc::UnboundedSender<Value>) {
    serve_as(runtime, commands, out, "rpc", Vec::new()).await;
}

/// The protocol loop for a front end named `channel`. `first` runs before any command is read:
/// a thread's interrupted tasks, resumed before it takes new work (principle B6). When the
/// command stream closes it lets reflections on the last tasks finish, and returns what they said.
pub async fn serve_as(
    mut runtime: Runtime,
    mut commands: mpsc::UnboundedReceiver<String>,
    out: mpsc::UnboundedSender<Value>,
    channel: &str,
    first: Vec<Job>,
) -> Vec<String> {
    runtime.set_channel(channel);
    let bus = runtime.pulses().clone();
    if let Some(valve) = runtime.valve() {
        nypi_core::valve::spawn_typing_recall(&bus, valve);
    }
    let mut session = Session {
        hints: bus.subscribe("rpc", &[PulseKind::MemoryHint]),
        bus,
        out,
        approvals: runtime.enable_approvals(),
        alerts: runtime.take_alerts(),
        pending_approvals: HashMap::new(),
        later: std::collections::VecDeque::new(),
    };
    for job in first {
        if !session.run(&mut runtime, &job, &mut commands).await {
            return runtime.wait_reflections(REFLECTIONS_WAIT).await;
        }
    }
    loop {
        // What Lucia learned from a finished task, once its reflection is through.
        for summary in runtime.finished_reflections() {
            session.emit(json!({ "type": "reflection", "summary": summary }));
        }
        // What a schedule or a module sent while a run was under way goes first, in order.
        if let Some(sourced) = session.later.pop_front() {
            if !session.run_sourced(&mut runtime, sourced, &mut commands).await {
                break;
            }
            continue;
        }
        let line = tokio::select! {
            line = commands.recv() => line,
            Some(alert) = recv_alert(&mut session.alerts) => {
                session.emit(alert_record(&alert));
                continue;
            }
            Some(hint) = session.hints.recv() => {
                session.emit(hint_record(&hint));
                continue;
            }
            // A specialist working in the background may ask while no run of this one is going.
            Some(approval) = session.approvals.recv() => {
                session.approval(approval);
                continue;
            }
        };
        let Some(line) = line else { break };
        let Some((id, command)) = session.parse(&line) else { continue };
        let kind = command["type"].as_str().unwrap_or_default().to_string();
        match protocol::parse(&command) {
            HubCommand::Sourced(Ok(sourced)) => {
                session.success(&id, "prompt", Some(json!({ "disposition": "started" })));
                if !session.run_sourced(&mut runtime, sourced, &mut commands).await {
                    break;
                }
            }
            HubCommand::Sourced(Err(error)) => session.error(&id, "prompt", error),
            HubCommand::Prompt { text, focus, .. } => {
                // The run works where its sender is; nothing changes for a runtime with one place.
                runtime.set_focus(focus);
                if let Some(output) = crate::commands::join_command(&runtime.paths, &text) {
                    session.success(&id, "prompt", Some(json!({ "disposition": "handled", "output": output })));
                    continue;
                }
                let text = if text.starts_with('/') || text.starts_with('!') {
                    match commands::run_command(&mut runtime, &text).await {
                        (_, CommandResult::Prompt(expanded)) => expanded,
                        (output, _) => {
                            session.success(&id, "prompt", Some(json!({ "disposition": "handled", "output": output })));
                            continue;
                        }
                    }
                } else {
                    text
                };
                session.success(&id, "prompt", Some(json!({ "disposition": "started" })));
                if !session.run(&mut runtime, &Job::Prompt(text), &mut commands).await {
                    break;
                }
            }
            HubCommand::Bash { command, exclude, focus } => {
                runtime.set_focus(focus);
                if !session.bash(&mut runtime, &id, command, exclude, &mut commands).await {
                    break;
                }
            }
            HubCommand::Answer { id: answered, approved, reason } => {
                session.answer_approval(&answered, approved, reason)
            }
            HubCommand::Typing(draft) => session.typing(&id, draft),
            HubCommand::GetState => session.success(&id, &kind, Some(state(&runtime, false))),
            HubCommand::Runtime(command) => {
                let result = command.run(&mut runtime).await;
                session.answer(&id, &kind, result);
            }
            // Nothing waits between runs, and no bash command is running.
            HubCommand::AbortRetry | HubCommand::AbortBash => session.success(&id, &kind, None),
            other => session.queue_command(&runtime.input(), &runtime.handle(), &id, &kind, other, false),
        }
    }
    // Input closed: let reflections on the last tasks finish saving what they learned.
    runtime.wait_reflections(REFLECTIONS_WAIT).await
}

struct Session {
    bus: PulseBus,
    /// Memories recalled for the client's drafts (`typing`), sent as `memory_hint`.
    hints: Subscription,
    out: mpsc::UnboundedSender<Value>,
    approvals: mpsc::UnboundedReceiver<Approval>,
    alerts: Option<mpsc::UnboundedReceiver<Alert>>,
    /// Waiting requests by the Inspector's own id, the one the board and every interface use.
    pending_approvals: HashMap<String, ApprovalRequest>,
    /// Sourced prompts that arrived during a run, in order.
    later: std::collections::VecDeque<Sourced>,
}

/// A prompt that is not the user's own words: a fired schedule, a goal review, a module's
/// (`"source"` on a `prompt` command). It runs as a task of its own, named for where it came
/// from, after whatever is running.
#[derive(Debug)]
pub(crate) struct Sourced {
    pub(crate) text: String,
    pub(crate) source: PulseSource,
    /// The task's origin in the ledger (`timer`, `goals`), when it differs from the front end's.
    pub(crate) origin: Option<String>,
    /// The pulse that led to it (`"cause"`): the timer firing in the hub, for the causal chain.
    pub(crate) cause: Option<nypi_core::pulse::PulseId>,
    /// Where it works (`"focus"`): the place its schedule or task recorded.
    pub(crate) focus: Option<nypi_core::place::Focus>,
}

impl Sourced {
    pub(crate) fn read(command: &Value) -> Result<Self, String> {
        let source = serde_json::from_value::<PulseSource>(command["source"].clone())
            .map_err(|error| format!("source is not a pulse source: {error}"))?;
        if matches!(source, PulseSource::UserInput { .. } | PulseSource::UserTyping { .. }) {
            return Err("a sourced prompt cannot claim to be the user's".into());
        }
        let text = command["message"].as_str().unwrap_or_default().to_string();
        if text.trim().is_empty() {
            return Err("message is required".into());
        }
        let cause = command["cause"].as_str().map(|id| nypi_core::pulse::PulseId(id.to_string()));
        let focus = protocol::focus_of(command);
        Ok(Sourced { text, source, origin: command["origin"].as_str().map(str::to_string), cause, focus })
    }
}

impl Session {
    fn emit(&self, record: Value) {
        let _ = self.out.send(record);
    }

    fn success(&self, id: &Option<Value>, command: &str, data: Option<Value>) {
        let mut record = json!({ "type": "response", "command": command, "success": true });
        if let Some(id) = id {
            record["id"] = id.clone();
        }
        if let Some(data) = data {
            record["data"] = data;
        }
        self.emit(record);
    }

    fn error(&self, id: &Option<Value>, command: &str, message: impl Into<String>) {
        let mut record = json!({ "type": "response", "command": command, "success": false, "error": message.into() });
        if let Some(id) = id {
            record["id"] = id.clone();
        }
        self.emit(record);
    }

    fn reply(&self, id: &Option<Value>, command: &str, result: anyhow::Result<Option<Value>>) {
        match result {
            Ok(data) => self.success(id, command, data),
            Err(error) => self.error(id, command, format!("{error:#}")),
        }
    }

    /// The command's id and body, or None after reporting a parse error.
    fn parse(&self, line: &str) -> Option<(Option<Value>, Value)> {
        match serde_json::from_str::<Value>(line) {
            Ok(command) if command.is_object() => Some((command.get("id").cloned(), command)),
            Ok(_) => {
                self.error(&None, "parse", "Failed to parse command: expected a JSON object");
                None
            }
            Err(error) => {
                self.error(&None, "parse", format!("Failed to parse command: {error}"));
                None
            }
        }
    }

    /// Run a sourced prompt as a task named for its origin.
    async fn run_sourced(
        &mut self,
        runtime: &mut Runtime,
        sourced: Sourced,
        commands: &mut mpsc::UnboundedReceiver<String>,
    ) -> bool {
        let front_end = runtime.channel().to_string();
        // Said before the run: interfaces show where it came from instead of its text, which is
        // nobody's words.
        self.emit(sourced_record(&sourced));
        if let Some(origin) = &sourced.origin {
            runtime.set_origin(origin.clone());
        }
        runtime.set_focus(sourced.focus.clone());
        let job = Job::Sourced { text: sourced.text, source: sourced.source, cause: sourced.cause };
        let open = self.run(runtime, &job, commands).await;
        runtime.set_origin(front_end);
        open
    }

    /// Run one job to the end, handling commands that arrive meanwhile. Returns false when
    /// stdin closed during the run (the run is aborted first).
    async fn run(&mut self, runtime: &mut Runtime, job: &Job, commands: &mut mpsc::UnboundedReceiver<String>) -> bool {
        let state = state(runtime, true);
        let handle = runtime.handle();
        let input = runtime.input();
        let (tx, mut events) = mpsc::unbounded_channel::<RuntimeEvent>();
        let mut abort_ids: Vec<Option<Value>> = Vec::new();
        let mut input_open = true;
        // Between a retry's start and the retried run: `abort_retry` ends only this wait.
        let mut retry_waiting = false;
        let run = runtime.run_job(job, &tx);
        tokio::pin!(run);
        let result = loop {
            tokio::select! {
                result = &mut run => break result,
                Some(event) = events.recv() => {
                    match &event {
                        RuntimeEvent::Retry { notice: RetryNotice::AutoRetryStart { .. } } => retry_waiting = true,
                        RuntimeEvent::Retry { notice: RetryNotice::AutoRetryEnd { .. } }
                        | RuntimeEvent::Agent { event: AgentEvent::AgentStart } => retry_waiting = false,
                        _ => {}
                    }
                    self.emit(event_record(&event));
                }
                Some(alert) = recv_alert(&mut self.alerts) => self.emit(alert_record(&alert)),
                Some(hint) = self.hints.recv() => self.emit(hint_record(&hint)),
                Some(approval) = self.approvals.recv() => self.approval(approval),
                line = commands.recv(), if input_open => {
                    let Some(line) = line else {
                        input_open = false;
                        handle.abort();
                        continue;
                    };
                    let Some((id, command)) = self.parse(&line) else { continue };
                    let kind = command["type"].as_str().unwrap_or_default().to_string();
                    match protocol::parse(&command) {
                        HubCommand::Abort => {
                            self.pending_approvals.clear();
                            handle.abort();
                            abort_ids.push(id);
                        }
                        HubCommand::AbortRetry => {
                            if retry_waiting {
                                handle.abort();
                            }
                            self.success(&id, "abort_retry", None);
                        }
                        HubCommand::Answer { id: answered, approved, reason } => {
                            self.answer_approval(&answered, approved, reason)
                        }
                        HubCommand::Typing(draft) => self.typing(&id, draft),
                        HubCommand::GetState => self.success(&id, "get_state", Some(state.clone())),
                        // Not the owner's words: it waits its turn and never steers their run.
                        HubCommand::Sourced(Ok(sourced)) => {
                            self.later.push_back(sourced);
                            self.success(&id, "prompt", Some(json!({ "disposition": "scheduled" })));
                        }
                        HubCommand::Sourced(Err(error)) => self.error(&id, "prompt", error),
                        HubCommand::Prompt { text, behavior: Some(behavior), .. } => {
                            match behavior {
                                Behavior::Steer => input.steer(Message::user_text(text)),
                                Behavior::FollowUp => input.follow_up(Message::user_text(text)),
                            }
                            self.success(&id, "prompt", Some(json!({ "disposition": "queued" })));
                        }
                        HubCommand::Prompt { behavior: None, .. } => self.error(
                            &id,
                            "prompt",
                            "Agent is already running; set streamingBehavior to \"steer\" or \"followUp\" to queue",
                        ),
                        other => self.queue_command(&input, &handle, &id, &kind, other, true),
                    }
                }
            }
        };
        while let Ok(event) = events.try_recv() {
            self.emit(event_record(&event));
        }
        self.pending_approvals.clear();
        self.emit(settled_record(&result.map_err(|error| format!("{error:#}"))));
        for id in abort_ids {
            self.success(&id, "abort", None);
        }
        input_open
    }

    /// Run the user's shell command (Pi's RPC `bash`), taking `abort_bash` meanwhile. Returns
    /// false when stdin closed while it ran (the command is killed first).
    async fn bash(
        &mut self,
        runtime: &mut Runtime,
        id: &Option<Value>,
        text: Option<String>,
        exclude: bool,
        commands: &mut mpsc::UnboundedReceiver<String>,
    ) -> bool {
        let Some(text) = text else {
            self.error(id, "bash", "command is required");
            return true;
        };
        let cancel = tokio_util::sync::CancellationToken::new();
        let run = runtime.run_user_bash(&text, exclude, cancel.clone());
        tokio::pin!(run);
        let mut input_open = true;
        let result = loop {
            tokio::select! {
                result = &mut run => break result,
                line = commands.recv(), if input_open => {
                    let Some(line) = line else {
                        input_open = false;
                        cancel.cancel();
                        continue;
                    };
                    let Some((other, command)) = self.parse(&line) else { continue };
                    let kind = command["type"].as_str().unwrap_or_default();
                    match protocol::parse(&command) {
                        HubCommand::AbortBash => {
                            cancel.cancel();
                            self.success(&other, "abort_bash", None);
                        }
                        HubCommand::Typing(draft) => self.typing(&other, draft),
                        _ => self.error(&other, kind, format!("{kind} is not available while a bash command runs")),
                    }
                }
            }
        };
        self.reply(
            id,
            "bash",
            result.map(|run| {
                Some(json!({
                    "output": run.output, "exitCode": run.exit_code, "cancelled": run.cancelled,
                    "truncated": run.truncated, "fullOutputPath": run.full_output_path,
                }))
            }),
        );
        input_open
    }

    /// The client's current draft (decision D34): recall runs on it before it is sent.
    fn typing(&self, id: &Option<Value>, draft: String) {
        let chars = draft.chars().count();
        self.bus.publish(
            PulseSource::UserTyping { channel: "rpc".into() },
            None,
            PulsePayload::Typing { draft, chars },
        );
        if id.is_some() {
            self.success(id, "typing", None);
        }
    }

    /// A request is shown under the Inspector's id; one closed elsewhere (on the board, by a
    /// timeout) is resolved for every interface still showing it.
    fn approval(&mut self, approval: Approval) {
        match approval {
            Approval::Asked(request) => {
                self.emit(approval_record(&request));
                self.pending_approvals.insert(request.id.clone(), request);
            }
            Approval::Closed { id, approved } => {
                if self.pending_approvals.remove(&id).is_some() {
                    self.emit(json!({ "type": "approval_resolved", "id": id, "approved": approved }));
                }
            }
        }
    }

    fn answer_approval(&mut self, id: &str, approved: bool, reason: Option<String>) {
        let Some(request) = self.pending_approvals.remove(id) else { return };
        if approved {
            request.approve();
        } else {
            request.deny(reason);
        }
        // Every interface showing the question learns it is answered, whoever answered.
        self.emit(json!({ "type": "approval_resolved", "id": id, "approved": approved }));
    }

    fn answer(&self, id: &Option<Value>, command: &str, result: Result<Option<Value>, String>) {
        match result {
            Ok(data) => self.success(id, command, data),
            Err(error) => self.error(id, command, error),
        }
    }

    /// Queue commands work in both states; anything else is refused while a run is active.
    fn queue_command(
        &self,
        input: &InputHandle,
        handle: &AgentHandle,
        id: &Option<Value>,
        kind: &str,
        command: HubCommand,
        running: bool,
    ) {
        match command {
            HubCommand::Steer(text) => {
                input.steer(Message::user_text(text));
                self.success(id, kind, Some(json!({ "disposition": "queued" })));
            }
            HubCommand::FollowUp(text) => {
                input.follow_up(Message::user_text(text));
                self.success(id, kind, Some(json!({ "disposition": "queued" })));
            }
            HubCommand::ClearQueue => {
                let texts = |queue: &MessageQueue| -> Vec<String> {
                    queue
                        .take_all()
                        .iter()
                        .map(|m| match m {
                            Message::User(user) => text_of(&user.content),
                            _ => String::new(),
                        })
                        .collect()
                };
                let data = json!({ "steering": texts(&handle.steering), "followUp": texts(&handle.follow_up) });
                self.success(id, kind, Some(data));
            }
            HubCommand::Abort => self.success(id, kind, None),
            _ if running => self.error(id, kind, format!("{kind} is not available while a run is active")),
            _ => self.error(id, kind, format!("Unknown command: {kind}")),
        }
    }
}

/// A prompt that is not the user's own words is about to run: where it came from, and its text.
pub(crate) fn sourced_record(sourced: &Sourced) -> Value {
    json!({ "type": "sourced_prompt", "origin": sourced.origin, "source": sourced.source, "text": sourced.text })
}

/// How a run ended, as the record every front end and the daemon read.
pub(crate) fn settled_record(result: &Result<nypi_core::runtime::RunOutcome, String>) -> Value {
    match result {
        Ok(outcome) => json!({
            "type": "agent_settled",
            // What the run cost, for whoever accounts for it (an idle pass has a daily budget).
            "usage": usage_of(outcome),
            "aborted": outcome.final_stop == Some(StopReason::Aborted),
            "status": outcome.status,
            "pulse": outcome.pulse,
            "citations": outcome.citations,
            // Beyond Pi's record: the reply and its task, for clients that only want the result.
            "answer": outcome.answer,
            "task": outcome.task,
            "sources": outcome.citations.footer(),
        }),
        Err(error) => json!({ "type": "agent_settled", "aborted": false, "error": error }),
    }
}

/// Tokens and cost of a run's own messages.
fn usage_of(outcome: &nypi_core::runtime::RunOutcome) -> Value {
    let (mut tokens, mut cost) = (0u64, 0.0);
    for message in &outcome.new_messages {
        if let Message::Assistant(reply) = message {
            tokens += reply.usage.input + reply.usage.output + reply.usage.cache_read;
            cost += reply.usage.cost.total;
        }
    }
    json!({ "tokens": tokens, "cost": cost })
}

fn state(runtime: &Runtime, streaming: bool) -> Value {
    let handle = runtime.handle();
    json!({
        "model": runtime.model(),
        "thinkingLevel": runtime.agent.config.thinking.as_str(),
        "isStreaming": streaming,
        "isCompacting": false,
        "steeringMode": handle.steering.mode(),
        "followUpMode": handle.follow_up.mode(),
        "sessionFile": runtime.session_path(),
        "sessionId": runtime.session_id(),
        "sessionName": runtime.session_name(),
        "autoCompactionEnabled": runtime.settings.compaction.enabled,
        "messageCount": runtime.agent.messages.len(),
        "pendingMessageCount": handle.steering.len() + handle.follow_up.len(),
    })
}

pub(crate) fn event_record(event: &RuntimeEvent) -> Value {
    match event {
        RuntimeEvent::Agent { event } => serde_json::to_value(event).unwrap_or(Value::Null),
        RuntimeEvent::Alert { alert } => alert_record(alert),
        RuntimeEvent::CompactionStart { reason } => json!({ "type": "compaction_start", "reason": reason }),
        RuntimeEvent::CompactionEnd { tokens_before, error } => {
            json!({ "type": "compaction_end", "tokensBefore": tokens_before, "error": error })
        }
        // Already in the shape of Pi's session events.
        RuntimeEvent::Retry { notice } => serde_json::to_value(notice).unwrap_or(Value::Null),
    }
}

pub(crate) fn hint_record(pulse: &Pulse) -> Value {
    let memories = match &pulse.payload {
        PulsePayload::MemoryHint { memories } => json!(memories),
        _ => json!([]),
    };
    json!({ "type": "memory_hint", "pulse": pulse.id, "memories": memories })
}

/// An approval request as every interface and channel receives it. `listened` says the board
/// shows it too, so a hub with nobody attached need not deny it at once.
pub(crate) fn approval_record(request: &ApprovalRequest) -> Value {
    json!({
        "type": "approval_request", "id": request.id, "tool": request.tool, "summary": request.summary,
        "reason": request.reason, "listened": request.listened,
    })
}

pub(crate) fn alert_record(alert: &Alert) -> Value {
    json!({ "type": "alert", "level": alert.level, "message": alert.message, "at": alert.at })
}

/// Receive from an optional alert channel; pends forever when there is none.
pub async fn recv_alert(alerts: &mut Option<mpsc::UnboundedReceiver<Alert>>) -> Option<Alert> {
    match alerts {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Stdin split on LF only: Unicode line separators are valid inside JSON strings.
fn spawn_line_reader() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut line = Vec::new();
        loop {
            line.clear();
            match stdin.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    while matches!(line.last(), Some(b'\n' | b'\r')) {
                        line.pop();
                    }
                    if line.is_empty() {
                        continue;
                    }
                    if tx.send(String::from_utf8_lossy(&line).into_owned()).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
    use nypi_core::config::Paths;
    use nypi_core::runtime::{RuntimeOptions, SessionMode};

    async fn next_of(records: &mut mpsc::UnboundedReceiver<Value>, kind: &str) -> Value {
        loop {
            let record = tokio::time::timeout(std::time::Duration::from_secs(5), records.recv())
                .await
                .expect("record in time")
                .expect("stream open");
            if record["type"] == kind {
                return record;
            }
        }
    }

    fn faux_runtime(home: &std::path::Path, work: &std::path::Path, faux: &FauxProvider) -> Runtime {
        Runtime::new(RuntimeOptions {
            cwd: work.to_path_buf(),
            paths: Some(Paths { home: home.to_path_buf() }),
            model: Some("faux/faux-1".into()),
            extra_model: Some(faux_model()),
            stream_fn: Some(faux.stream_fn()),
            session: SessionMode::Ephemeral,
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn session_controls_save_settings_and_report_stats() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(home.path().join("settings.json"), r#"{ "enforceStatusLine": false }"#).unwrap();
        let faux = FauxProvider::new(vec![FauxResponse::text("Hello.")]);
        let (commands, input) = mpsc::unbounded_channel();
        let (out, mut records) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve(faux_runtime(home.path(), work.path(), &faux), input, out));
        let send = |v: Value| commands.send(v.to_string()).unwrap();

        send(json!({ "type": "set_auto_retry", "enabled": false }));
        send(json!({ "type": "set_auto_compaction", "enabled": false }));
        send(json!({ "type": "set_steering_mode", "mode": "all" }));
        send(json!({ "type": "set_follow_up_mode", "mode": "sideways" }));
        for _ in 0..3 {
            assert_eq!(next_of(&mut records, "response").await["success"], true);
        }
        assert_eq!(next_of(&mut records, "response").await["success"], false, "an unknown mode is refused");
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(home.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(
            saved,
            json!({
                "enforceStatusLine": false,
                "retry": { "enabled": false },
                "compaction": { "enabled": false },
                "steeringMode": "all",
            })
        );
        send(json!({ "type": "get_state" }));
        let state = next_of(&mut records, "response").await;
        assert_eq!(
            (state["data"]["steeringMode"].clone(), state["data"]["autoCompactionEnabled"].clone()),
            (json!("all"), json!(false))
        );
        send(json!({ "type": "cycle_thinking_level" }));
        assert_eq!(next_of(&mut records, "response").await["data"], Value::Null, "the faux model has no reasoning");

        send(json!({ "type": "prompt", "message": "hi" }));
        next_of(&mut records, "agent_settled").await;
        send(json!({ "type": "get_session_stats" }));
        let stats = next_of(&mut records, "response").await["data"].clone();
        assert_eq!((stats["userMessages"].clone(), stats["assistantMessages"].clone()), (json!(1), json!(1)));
        assert_eq!((stats["tokens"]["input"].clone(), stats["tokens"]["output"].clone()), (json!(10), json!(5)));
        assert_eq!(stats["contextUsage"]["contextWindow"], 100_000);
        assert!(stats["contextUsage"]["tokens"].as_u64().is_some(), "{stats}");

        let mut other = nypi_core::session::SessionStore::create(&home.path().join("sessions"), work.path());
        other.append_message(Message::user_text("from another session")).unwrap();
        send(json!({ "type": "switch_session", "sessionPath": other.path }));
        assert_eq!(next_of(&mut records, "response").await["data"]["cancelled"], false);
        send(json!({ "type": "get_state" }));
        assert_eq!(next_of(&mut records, "response").await["data"]["sessionId"], json!(other.header.id));
        send(json!({ "type": "get_last_assistant_text" }));
        assert_eq!(
            next_of(&mut records, "response").await["data"]["text"],
            Value::Null,
            "the other session's transcript"
        );
        drop(commands);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn the_session_tree_can_be_read_forked_cloned_and_named() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(home.path().join("settings.json"), r#"{ "enforceStatusLine": false }"#).unwrap();
        let faux = FauxProvider::new(vec![FauxResponse::text("One."), FauxResponse::text("Two.")]);
        let (commands, input) = mpsc::unbounded_channel();
        let (out, mut records) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve(faux_runtime(home.path(), work.path(), &faux), input, out));
        let send = |v: Value| commands.send(v.to_string()).unwrap();
        for prompt in ["first", "second"] {
            send(json!({ "type": "prompt", "message": prompt }));
            next_of(&mut records, "agent_settled").await;
        }

        send(json!({ "type": "get_fork_messages" }));
        let forks = next_of(&mut records, "response").await["data"]["messages"].clone();
        assert_eq!((forks[0]["text"].clone(), forks[1]["text"].clone()), (json!("first"), json!("second")));
        send(json!({ "type": "get_tree" }));
        let tree = next_of(&mut records, "response").await["data"].clone();
        assert!(tree["leafId"].is_string() && tree["tree"][0]["entry"]["id"].is_string(), "{tree}");
        send(json!({ "type": "get_entries", "since": "no-such-entry" }));
        assert_eq!(next_of(&mut records, "response").await["error"], "Entry not found: no-such-entry");

        send(json!({ "type": "fork", "entryId": forks[1]["entryId"] }));
        assert_eq!(next_of(&mut records, "response").await["data"], json!({ "text": "second", "cancelled": false }));
        send(json!({ "type": "get_messages" }));
        let texts: Vec<String> = next_of(&mut records, "response").await["data"]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "user")
            .map(|m| m["content"][0]["text"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(texts, ["first"], "the fork continues from before the chosen message");

        send(json!({ "type": "set_session_name", "name": "Trip" }));
        send(json!({ "type": "clone" }));
        send(json!({ "type": "set_session_name", "name": "  " }));
        assert_eq!(next_of(&mut records, "response").await["success"], true);
        assert_eq!(next_of(&mut records, "response").await["data"]["cancelled"], false);
        assert_eq!(next_of(&mut records, "response").await["error"], "Session name cannot be empty");
        send(json!({ "type": "get_state" }));
        assert_eq!(next_of(&mut records, "response").await["data"]["sessionName"], "Trip", "the clone keeps the name");
        drop(commands);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn user_bash_output_reaches_the_model_unless_excluded() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(home.path().join("settings.json"), r#"{ "enforceStatusLine": false }"#).unwrap();
        let faux = FauxProvider::new(vec![FauxResponse::text("Seen.")]);
        let (commands, input) = mpsc::unbounded_channel();
        let (out, mut records) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve(faux_runtime(home.path(), work.path(), &faux), input, out));
        let send = |v: Value| commands.send(v.to_string()).unwrap();

        send(json!({ "type": "bash", "command": "echo visible; exit 3" }));
        let result = next_of(&mut records, "response").await["data"].clone();
        assert_eq!((result["output"].clone(), result["exitCode"].clone()), (json!("visible"), json!(3)));
        send(json!({ "type": "bash", "command": "echo hidden", "excludeFromContext": true }));
        next_of(&mut records, "response").await;
        send(json!({ "type": "bash", "command": r"printf '\033[32mgreen\033[0m\r\n'", "excludeFromContext": true }));
        assert_eq!(next_of(&mut records, "response").await["data"]["output"], "green", "colors and CR are cleaned");
        send(json!({ "type": "prompt", "message": "what did I run?" }));
        next_of(&mut records, "agent_settled").await;

        let sent: Vec<String> = faux.requests()[0]
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User(u) => Some(text_of(&u.content)),
                _ => None,
            })
            .collect();
        assert_eq!(sent[0], "Ran `echo visible; exit 3`\n```\nvisible\n```\n\nCommand exited with code 3");
        assert!(!sent.iter().any(|t| t.contains("hidden")), "{sent:?}");
        drop(commands);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn abort_bash_stops_the_command() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let faux = FauxProvider::new(vec![]);
        let (commands, input) = mpsc::unbounded_channel();
        let (out, mut records) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve(faux_runtime(home.path(), work.path(), &faux), input, out));
        commands.send(json!({ "id": "b", "type": "bash", "command": "sleep 30" }).to_string()).unwrap();
        commands.send(json!({ "id": "a", "type": "abort_bash" }).to_string()).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(next_of(&mut records, "response").await["command"], "abort_bash");
        let result = next_of(&mut records, "response").await;
        assert_eq!((result["command"].clone(), result["data"]["cancelled"].clone()), (json!("bash"), json!(true)));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        drop(commands);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn abort_retry_ends_only_the_wait() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(home.path().join("settings.json"), r#"{ "retry": { "baseDelayMs": 60000 } }"#).unwrap();
        let faux = FauxProvider::new(vec![FauxResponse::error("Request failed: error sending request for url (x)")]);
        let (commands, input) = mpsc::unbounded_channel();
        let (out, mut records) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve(faux_runtime(home.path(), work.path(), &faux), input, out));
        commands.send(json!({ "type": "prompt", "message": "hi" }).to_string()).unwrap();
        next_of(&mut records, "auto_retry_start").await;
        commands.send(json!({ "id": "r", "type": "abort_retry" }).to_string()).unwrap();
        let end = next_of(&mut records, "auto_retry_end").await;
        assert_eq!((end["success"].clone(), end["finalError"].clone()), (json!(false), json!("Retry cancelled")));
        next_of(&mut records, "agent_settled").await;
        assert_eq!(faux.requests().len(), 1);
        drop(commands);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn typing_drafts_come_back_as_memory_hints() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        nypi_core::memory::MemoryStore::new(paths.memory())
            .add(nypi_core::memory::NewMemory {
                text: "The user's dentist is Dr. Lin.".into(),
                tags: vec!["dentist".into()],
                importance: 0.7,
                ..Default::default()
            })
            .unwrap();
        let faux = FauxProvider::new(vec![]);
        let runtime = Runtime::new(RuntimeOptions {
            cwd: work.path().to_path_buf(),
            paths: Some(paths),
            model: Some("faux/faux-1".into()),
            extra_model: Some(faux_model()),
            stream_fn: Some(faux.stream_fn()),
            session: SessionMode::Ephemeral,
            ..Default::default()
        })
        .unwrap();
        let (commands, input) = mpsc::unbounded_channel();
        let (out, mut records) = mpsc::unbounded_channel();
        let _server = tokio::spawn(serve(runtime, input, out));
        commands.send(json!({ "type": "typing", "text": "can you book the dentist?" }).to_string()).unwrap();
        let hint = next_of(&mut records, "memory_hint").await;
        assert_eq!(hint["memories"][0]["text"], "The user's dentist is Dr. Lin.", "{hint}");
    }

    #[tokio::test]
    async fn prompts_queue_approvals_and_state() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let faux = FauxProvider::new(vec![
            FauxResponse::tool_call("t1", "bash", json!({ "command": "echo would git push" })),
            FauxResponse::text("Done: pushed."),
        ]);
        let runtime = Runtime::new(RuntimeOptions {
            cwd: work.path().to_path_buf(),
            paths: Some(Paths { home: home.path().to_path_buf() }),
            model: Some("faux/faux-1".into()),
            extra_model: Some(faux_model()),
            stream_fn: Some(faux.stream_fn()),
            session: SessionMode::Ephemeral,
            ..Default::default()
        })
        .unwrap();
        let (commands, input) = mpsc::unbounded_channel();
        let (out, mut records) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve(runtime, input, out));
        let send = |v: Value| commands.send(v.to_string()).unwrap();

        send(json!({ "id": 1, "type": "get_state" }));
        let state = next_of(&mut records, "response").await;
        assert_eq!((state["id"].clone(), state["data"]["isStreaming"].clone()), (json!(1), json!(false)));
        commands.send("not json".into()).unwrap();
        assert_eq!(next_of(&mut records, "response").await["command"], "parse");

        send(json!({ "id": "p", "type": "prompt", "message": "push it" }));
        let accepted = next_of(&mut records, "response").await;
        assert_eq!(accepted["data"]["disposition"], "started");
        let request = next_of(&mut records, "approval_request").await;
        assert_eq!(request["summary"], "echo would git push");
        send(json!({ "id": "busy", "type": "prompt", "message": "another" }));
        let refused = next_of(&mut records, "response").await;
        assert_eq!((refused["id"].clone(), refused["success"].clone()), (json!("busy"), json!(false)));
        send(json!({ "type": "approval_response", "id": request["id"], "approved": true }));
        let end = next_of(&mut records, "tool_execution_end").await;
        assert_eq!(end["output"]["isError"], false);
        let settled = next_of(&mut records, "agent_settled").await;
        assert_eq!(settled["status"]["kind"], "done");
        assert_eq!(settled["answer"], "Done: pushed.");
        assert!(settled["task"].as_str().is_some_and(|task| task.starts_with("t-")), "{settled}");

        send(json!({ "id": 2, "type": "get_last_assistant_text" }));
        assert_eq!(next_of(&mut records, "response").await["data"]["text"], "Done: pushed.");
        send(json!({ "id": 3, "type": "steer", "message": "later" }));
        send(json!({ "id": 4, "type": "clear_queue" }));
        next_of(&mut records, "response").await;
        assert_eq!(next_of(&mut records, "response").await["data"]["steering"], json!(["later"]));
        send(json!({ "id": 5, "type": "no_such_command" }));
        assert_eq!(next_of(&mut records, "response").await["error"], "Unknown command: no_such_command");
        send(json!({ "id": 6, "type": "prompt", "message": "x", "source": { "type": "userInput", "channel": "cli" } }));
        let forged = next_of(&mut records, "response").await;
        assert_eq!(forged["error"], "a sourced prompt cannot claim to be the user's");
        drop(commands);
        server.await.unwrap();
    }

    /// Answered on the board, an approval is resolved on the hub's stream at once, under the
    /// id the board used, so no interface keeps showing (or answering) a question already gone.
    #[tokio::test]
    async fn an_approval_answered_on_the_board_is_resolved_for_every_interface() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let faux = FauxProvider::new(vec![
            FauxResponse::tool_call("t1", "bash", json!({ "command": "echo would git push" })),
            FauxResponse::text("Done: pushed."),
        ]);
        let runtime = faux_runtime(home.path(), work.path(), &faux);
        let inspector = runtime.inspector();
        let (board, mut shown) = mpsc::unbounded_channel();
        inspector.set_approval_listener(Some(board));
        let (commands, input) = mpsc::unbounded_channel();
        let (out, mut records) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve(runtime, input, out));
        commands.send(json!({ "type": "prompt", "message": "push it" }).to_string()).unwrap();
        let request = next_of(&mut records, "approval_request").await;
        let Some(nypi_core::inspector::ApprovalEvent::Waiting { id, .. }) = shown.recv().await else { panic!() };
        assert_eq!(request["id"], json!(id), "one id on the board and on the stream");
        assert_eq!(request["listened"], true);
        assert!(inspector.answer(&id, true, None));
        let resolved = next_of(&mut records, "approval_resolved").await;
        assert_eq!((resolved["id"].clone(), resolved["approved"].clone()), (json!(id), json!(true)));
        assert_eq!(next_of(&mut records, "agent_settled").await["answer"], "Done: pushed.");
        drop(commands);
        server.await.unwrap();
    }
}

#[cfg(test)]
mod sourced_tests {
    use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
    use nypi_core::config::Paths;
    use nypi_core::ledger::TaskState;
    use nypi_core::runtime::{RuntimeOptions, SessionMode};

    use super::*;

    #[tokio::test]
    async fn a_fired_schedule_waits_its_turn_and_is_a_task_of_its_own() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let faux = FauxProvider::new(vec![
            FauxResponse::tool_call("t1", "bash", json!({ "command": "echo would git push" })),
            FauxResponse::text("Pushed.\n\nDone: pushed."),
            FauxResponse::text("The build passed.\n\nDone: checked the build."),
        ]);
        let paths = Paths { home: home.path().to_path_buf() };
        let runtime = Runtime::new(RuntimeOptions {
            cwd: work.path().to_path_buf(),
            paths: Some(paths.clone()),
            model: Some("faux/faux-1".into()),
            extra_model: Some(faux_model()),
            stream_fn: Some(faux.stream_fn()),
            session: SessionMode::Ephemeral,
            ..Default::default()
        })
        .unwrap();
        let ledger = nypi_core::ledger::Ledger::new(paths.ledger());
        let (commands, input) = mpsc::unbounded_channel();
        let (out, mut records) = mpsc::unbounded_channel();
        let server = tokio::spawn(serve_as(runtime, input, out, "hub", Vec::new()));
        let send = |v: Value| commands.send(v.to_string()).unwrap();
        async fn next_of(records: &mut mpsc::UnboundedReceiver<Value>, kind: &str) -> Value {
            loop {
                let record = tokio::time::timeout(std::time::Duration::from_secs(20), records.recv())
                    .await
                    .unwrap_or_else(|_| panic!("no {kind} record"))
                    .expect("the loop ended");
                if record["type"] == kind {
                    return record;
                }
            }
        }

        // The user's run waits for an approval; a schedule fires meanwhile.
        send(json!({ "id": "u", "type": "prompt", "message": "push it" }));
        let asked = next_of(&mut records, "approval_request").await;
        let fired = json!({
            "id": "s", "type": "prompt", "message": "Check whether the build finished and report.",
            "source": { "type": "timer", "schedule": "s-1" }, "origin": "timer",
        });
        send(fired);
        let taken = next_of(&mut records, "response").await;
        assert_eq!((taken["id"].clone(), taken["data"]["disposition"].clone()), (json!("s"), json!("scheduled")));
        send(json!({ "type": "approval_response", "id": asked["id"], "approved": true }));
        let first = next_of(&mut records, "agent_settled").await;
        assert_eq!(first["status"]["text"], "pushed.", "the user's run was not steered by the schedule");
        let second = next_of(&mut records, "agent_settled").await;
        assert_eq!(second["status"]["text"], "checked the build.");

        let (user_task, timer_task) = (first["task"].as_str().unwrap(), second["task"].as_str().unwrap());
        assert_ne!(user_task, timer_task, "a task of its own");
        let tasks = ledger.load();
        let origin =
            |id: &str| tasks.values().find(|task| task.id.0 == id).map(|task| (task.origin.clone(), task.state));
        assert_eq!(origin(user_task), Some(("hub".into(), TaskState::Done)));
        assert_eq!(origin(timer_task), Some(("timer".into(), TaskState::Done)), "named for where it came from");
        // The schedule's words reached the model as their own prompt, after the user's run.
        let last = faux.requests().pop().unwrap();
        let said = last.messages.iter().rev().find_map(|message| match message {
            Message::User(user) => Some(text_of(&user.content)),
            _ => None,
        });
        assert_eq!(said.as_deref(), Some("Check whether the build finished and report."));
        drop(commands);
        server.await.unwrap();
    }
}
