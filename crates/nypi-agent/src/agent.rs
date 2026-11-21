//! The agent turn loop (port of Pi's agent-loop.ts) and the handle used to steer, queue
//! follow-ups, and abort while a run is in progress.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use nypi_ai::{
    AssistantMessage, Context, Message, Model, StopReason, StreamEvent, StreamFn, StreamOptions, ThinkingLevel,
    ToolCall, ToolResultMessage, now_ms,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::events::{AgentEvent, EventSink};
use crate::tool::{ExecutionMode, Tool, ToolContext, ToolHooks, ToolOutput};
use crate::validate::validate_arguments;

/// Everything that shapes a provider request and tool execution.
#[derive(Clone)]
pub struct AgentConfig {
    pub model: Model,
    pub thinking: ThinkingLevel,
    pub system_prompt: String,
    pub tools: Vec<Arc<dyn Tool>>,
    pub hooks: Vec<Arc<dyn ToolHooks>>,
    /// Prepares each turn after the first (compaction mid-run).
    pub turn_hooks: Option<Arc<dyn TurnHooks>>,
    pub stream_fn: StreamFn,
    pub api_key: Option<String>,
    pub tool_execution: ExecutionMode,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub session_id: Option<String>,
}

/// What a turn hook changes before the next request (Pi's `AgentLoopTurnUpdate`).
#[derive(Debug, Default)]
pub struct TurnUpdate {
    /// A transcript to continue with in place of the current one, such as a compacted one.
    pub transcript: Option<Vec<Message>>,
    /// Messages added to the run before queued ones, such as a system prompt update.
    pub messages: Vec<Message>,
}

/// Runs between turns, before the next request (Pi's `prepareNextTurn`).
#[async_trait::async_trait]
pub trait TurnHooks: Send + Sync {
    /// What to change before the next turn; `cancel` is the run's abort.
    async fn before_next_turn(
        &self,
        config: &AgentConfig,
        messages: &[Message],
        cancel: &CancellationToken,
    ) -> TurnUpdate;
}

/// How many queued messages one drain point takes, named as in Pi's settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

/// Thread-safe FIFO of user messages waiting for a drain point.
#[derive(Clone, Default)]
pub struct MessageQueue {
    inner: Arc<Mutex<VecDeque<Message>>>,
    /// Shared by every clone, so a front end can change it while a run drains the queue.
    mode: Arc<Mutex<QueueMode>>,
}

impl MessageQueue {
    pub fn new(mode: QueueMode) -> Self {
        MessageQueue { inner: Arc::default(), mode: Arc::new(Mutex::new(mode)) }
    }

    pub fn mode(&self) -> QueueMode {
        *self.mode.lock().unwrap()
    }

    pub fn set_mode(&self, mode: QueueMode) {
        *self.mode.lock().unwrap() = mode;
    }

    pub fn push(&self, message: Message) {
        self.inner.lock().unwrap().push_back(message);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take the messages this drain point is allowed to inject.
    pub fn drain(&self) -> Vec<Message> {
        let mode = self.mode();
        let mut queue = self.inner.lock().unwrap();
        match mode {
            QueueMode::All => queue.drain(..).collect(),
            QueueMode::OneAtATime => queue.pop_front().into_iter().collect(),
        }
    }

    pub fn take_all(&self) -> Vec<Message> {
        self.inner.lock().unwrap().drain(..).collect()
    }
}

/// Cloneable control surface for a running agent.
#[derive(Clone)]
pub struct AgentHandle {
    pub steering: MessageQueue,
    pub follow_up: MessageQueue,
    cancel: Arc<Mutex<CancellationToken>>,
    running: Arc<AtomicBool>,
}

impl AgentHandle {
    /// Inject `message` after the current turn's tool calls finish.
    pub fn steer(&self, message: Message) {
        self.steering.push(message);
    }

    /// Process `message` once the agent would otherwise stop.
    pub fn follow_up(&self, message: Message) {
        self.follow_up.push(message);
    }

    /// Cancel the in-flight request or tools.
    pub fn abort(&self) {
        self.cancel.lock().unwrap().cancel();
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Remove and return every queued message (used to hand them back after an abort).
    pub fn take_queued(&self) -> Vec<Message> {
        let mut all = self.steering.take_all();
        all.extend(self.follow_up.take_all());
        all
    }
}

pub struct Agent {
    pub config: AgentConfig,
    /// Conversation transcript sent to the model.
    pub messages: Vec<Message>,
    handle: AgentHandle,
    /// The next run starts aborted (see `abort_next_run`).
    abort_next: bool,
}

impl Agent {
    pub fn new(config: AgentConfig, messages: Vec<Message>) -> Self {
        let handle = AgentHandle {
            steering: MessageQueue::new(QueueMode::OneAtATime),
            follow_up: MessageQueue::new(QueueMode::OneAtATime),
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
            running: Arc::new(AtomicBool::new(false)),
        };
        Agent { config, messages, handle, abort_next: false }
    }

    pub fn handle(&self) -> AgentHandle {
        self.handle.clone()
    }

    /// What `AgentHandle::abort` cancels until the next run starts, so a caller's wait between
    /// runs (a retry's backoff) is cancelled by the same abort. Already cancelled when the
    /// user aborted after the last run ended.
    pub fn abort_signal(&self) -> CancellationToken {
        self.handle.cancel.lock().unwrap().clone()
    }

    /// A new abort scope for work the caller does between runs (compaction): what
    /// `AgentHandle::abort` cancels until the next run starts.
    pub fn new_abort_scope(&self) -> CancellationToken {
        let token = CancellationToken::new();
        *self.handle.cancel.lock().unwrap() = token.clone();
        token
    }

    /// Start the next run aborted, for an abort that came while the caller prepared it
    /// (compaction before a prompt): its prompt is recorded, and nothing is requested.
    pub fn abort_next_run(&mut self) {
        self.abort_next = true;
    }

    /// Run with new prompt messages. Returns every message this run added.
    pub async fn prompt(&mut self, prompts: Vec<Message>, sink: &EventSink) -> Vec<Message> {
        self.run(prompts, sink).await
    }

    /// Run from the current transcript (for retries). The last message must not be an assistant message.
    pub async fn continue_run(&mut self, sink: &EventSink) -> Vec<Message> {
        self.run(Vec::new(), sink).await
    }

    async fn run(&mut self, prompts: Vec<Message>, sink: &EventSink) -> Vec<Message> {
        let cancel = CancellationToken::new();
        if std::mem::take(&mut self.abort_next) {
            cancel.cancel();
        }
        *self.handle.cancel.lock().unwrap() = cancel.clone();
        self.handle.running.store(true, Ordering::SeqCst);
        let mut new_messages = Vec::new();
        emit(sink, AgentEvent::AgentStart);
        emit(sink, AgentEvent::TurnStart);
        for message in prompts {
            self.append(message, &mut new_messages, sink);
        }
        self.run_loop(&mut new_messages, &cancel, sink).await;
        self.handle.running.store(false, Ordering::SeqCst);
        emit(sink, AgentEvent::AgentEnd { new_messages: new_messages.clone() });
        new_messages
    }

    async fn run_loop(&mut self, new_messages: &mut Vec<Message>, cancel: &CancellationToken, sink: &EventSink) {
        // The user may have typed while the run was being set up.
        let mut pending = self.handle.steering.drain();
        let mut first_turn = true;
        loop {
            let mut has_more_tool_calls = true;
            while has_more_tool_calls || !pending.is_empty() {
                if !first_turn {
                    let update = match self.config.turn_hooks.clone() {
                        Some(hooks) => hooks.before_next_turn(&self.config, &self.messages, cancel).await,
                        None => TurnUpdate::default(),
                    };
                    if let Some(transcript) = update.transcript {
                        self.messages = transcript;
                    }
                    // Preparing can take a while (compaction): pick up what the user typed meanwhile.
                    if pending.is_empty() {
                        pending = self.handle.steering.drain();
                    }
                    pending.splice(0..0, update.messages);
                    emit(sink, AgentEvent::TurnStart);
                }
                first_turn = false;
                for message in pending.drain(..) {
                    self.append(message, new_messages, sink);
                }

                self.declare_tool_changes(new_messages, sink);
                let assistant = self.stream_response(cancel, sink).await;
                self.messages.push(Message::Assistant(assistant.clone()));
                new_messages.push(Message::Assistant(assistant.clone()));
                if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                    emit(sink, AgentEvent::TurnEnd { message: assistant, tool_results: vec![] });
                    return;
                }

                let calls = assistant.tool_calls();
                let mut tool_results = Vec::new();
                has_more_tool_calls = false;
                if !calls.is_empty() {
                    let (results, terminate) = if assistant.stop_reason == StopReason::Length {
                        (self.fail_truncated_calls(&calls, sink), false)
                    } else {
                        self.execute_tool_calls(&calls, cancel, sink).await
                    };
                    has_more_tool_calls = !terminate;
                    for result in results {
                        self.messages.push(Message::ToolResult(result.clone()));
                        new_messages.push(Message::ToolResult(result.clone()));
                        tool_results.push(result);
                    }
                }
                emit(sink, AgentEvent::TurnEnd { message: assistant, tool_results });
                if cancel.is_cancelled() {
                    return;
                }
                pending = self.handle.steering.drain();
            }
            let follow_ups = self.handle.follow_up.drain();
            if follow_ups.is_empty() {
                return;
            }
            pending = follow_ups;
        }
    }

    /// Declare tool set changes to the model, as Pi's agent loop does: when the transcript carries
    /// its own tools (a leading system message), the difference between what it declares and
    /// what can run now becomes a system message, so replay always yields the current tools and
    /// the request's head stays unchanged (decision D40).
    fn declare_tool_changes(&mut self, new_messages: &mut Vec<Message>, sink: &EventSink) {
        if !nypi_ai::transcript::has_leading_system(&self.messages) {
            return;
        }
        let declared = nypi_ai::transcript::current_tools(&self.messages);
        let runnable: Vec<_> = self.config.tools.iter().map(|t| t.definition()).collect();
        let (tools_added, tools_removed) = nypi_ai::transcript::tool_changes(&declared, &runnable);
        if tools_added.is_empty() && tools_removed.is_empty() {
            return;
        }
        let update =
            nypi_ai::SystemMessage { tools_added, tools_removed, timestamp: nypi_ai::now_ms(), ..Default::default() };
        self.append(Message::System(update), new_messages, sink);
    }

    fn append(&mut self, message: Message, new_messages: &mut Vec<Message>, sink: &EventSink) {
        emit(sink, AgentEvent::MessageStart { message: message.clone() });
        emit(sink, AgentEvent::MessageEnd { message: message.clone() });
        self.messages.push(message.clone());
        new_messages.push(message);
    }

    async fn stream_response(&self, cancel: &CancellationToken, sink: &EventSink) -> AssistantMessage {
        let model = &self.config.model;
        let partial = AssistantMessage::empty(model.api.as_str(), &model.provider, &model.id);
        if cancel.is_cancelled() {
            return finish_without_stream(partial, StopReason::Aborted, "Request aborted", sink);
        }
        let ctx = Context {
            system_prompt: self.config.system_prompt.clone(),
            messages: self.messages.clone(),
            tools: self.config.tools.iter().map(|t| t.definition()).collect(),
        };
        let opts = StreamOptions {
            api_key: self.config.api_key.clone(),
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            thinking: self.config.thinking,
            cancel: cancel.clone(),
            session_id: self.config.session_id.clone(),
        };
        let mut events = (self.config.stream_fn)(model, &ctx, &opts);
        let mut started = false;
        while let Some(event) = events.recv().await {
            match event {
                StreamEvent::Start => {
                    started = true;
                    emit(sink, AgentEvent::MessageStart { message: Message::Assistant(partial.clone()) });
                }
                StreamEvent::Done { message } => {
                    if !started {
                        emit(sink, AgentEvent::MessageStart { message: Message::Assistant(partial.clone()) });
                    }
                    emit(sink, AgentEvent::MessageEnd { message: Message::Assistant(message.clone()) });
                    return message;
                }
                other => emit(sink, AgentEvent::MessageUpdate { event: other }),
            }
        }
        finish_without_stream(partial, StopReason::Error, "Provider stream closed without a final message", sink)
    }

    /// A `length` stop may have cut tool arguments short, so none of the calls are safe to run.
    fn fail_truncated_calls(&self, calls: &[ToolCall], sink: &EventSink) -> Vec<ToolResultMessage> {
        calls
            .iter()
            .map(|call| {
                emit_start(call, sink);
                let output = ToolOutput::error(format!(
                    "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                    call.name
                ));
                emit_end(call, &output, None, sink);
                let result = tool_result_message(call, output, None);
                emit_message(&result, sink);
                result
            })
            .collect()
    }

    /// Execute one assistant message's tool calls. Returns results in source order and whether
    /// every result asked to terminate.
    async fn execute_tool_calls(
        &self,
        calls: &[ToolCall],
        cancel: &CancellationToken,
        sink: &EventSink,
    ) -> (Vec<ToolResultMessage>, bool) {
        let sequential = self.config.tool_execution == ExecutionMode::Sequential
            || calls.iter().any(|call| {
                self.find_tool(&call.name).is_some_and(|t| t.execution_mode() == ExecutionMode::Sequential)
            });
        let mut outcomes: Vec<(ToolCall, ToolOutput, Option<u64>)> = Vec::new();
        if sequential {
            for call in calls {
                emit_start(call, sink);
                let outcome = match self.prepare(call, cancel).await {
                    Prepared::Immediate(output) => {
                        emit_end(call, &output, None, sink);
                        (call.clone(), output, None)
                    }
                    Prepared::Ready { tool, arguments } => {
                        run_prepared(
                            tool,
                            arguments,
                            call.clone(),
                            self.config.hooks.clone(),
                            cancel.clone(),
                            sink.clone(),
                        )
                        .await
                    }
                };
                outcomes.push(outcome);
                if cancel.is_cancelled() {
                    // Every call of the reply gets its result, as in the parallel branch: the
                    // ones not reached were aborted.
                    for call in &calls[outcomes.len()..] {
                        emit_start(call, sink);
                        let output = ToolOutput::error("Operation aborted");
                        emit_end(call, &output, None, sink);
                        outcomes.push((call.clone(), output, None));
                    }
                    break;
                }
            }
        } else {
            // Each call is prepared and run on its own, side by side: a call whose hook waits for
            // the user's approval holds up nothing but itself.
            for call in calls {
                emit_start(call, sink);
            }
            let pending = calls.iter().map(|call| async move {
                match self.prepare(call, cancel).await {
                    Prepared::Immediate(output) => {
                        emit_end(call, &output, None, sink);
                        (call.clone(), output, None)
                    }
                    Prepared::Ready { tool, arguments } => {
                        run_prepared(
                            tool,
                            arguments,
                            call.clone(),
                            self.config.hooks.clone(),
                            cancel.clone(),
                            sink.clone(),
                        )
                        .await
                    }
                }
            });
            outcomes = futures::future::join_all(pending).await;
        }
        let terminate = !outcomes.is_empty() && outcomes.iter().all(|(_, output, _)| output.terminate);
        let results = outcomes
            .into_iter()
            .map(|(call, output, duration)| {
                let result = tool_result_message(&call, output, duration);
                emit_message(&result, sink);
                result
            })
            .collect();
        (results, terminate)
    }

    fn find_tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.config.tools.iter().find(|t| t.definition().name == name).cloned()
    }

    /// Resolve the tool, validate arguments, and run `before_tool_call` hooks.
    async fn prepare(&self, call: &ToolCall, cancel: &CancellationToken) -> Prepared {
        let Some(tool) = self.find_tool(&call.name) else {
            return Prepared::Immediate(ToolOutput::error(format!("Tool {} not found", call.name)));
        };
        if let Some(error) = &call.arguments_error {
            return Prepared::Immediate(ToolOutput::error(error.clone()));
        }
        let mut arguments = tool.prepare_arguments(call.arguments.clone());
        if let Err(error) = validate_arguments(&tool.definition().parameters, &mut arguments) {
            return Prepared::Immediate(ToolOutput::error(error));
        }
        for hook in &self.config.hooks {
            // A hook may wait on a person (approval), so an abort must not wait for it.
            let verdict = tokio::select! {
                verdict = hook.before_tool_call(call, &arguments) => verdict,
                _ = cancel.cancelled() => return Prepared::Immediate(ToolOutput::error("Operation aborted")),
            };
            if let Some(block) = verdict {
                let mut output = ToolOutput::error(block.reason);
                output.terminate = block.terminate;
                return Prepared::Immediate(output);
            }
        }
        if cancel.is_cancelled() {
            return Prepared::Immediate(ToolOutput::error("Operation aborted"));
        }
        Prepared::Ready { tool, arguments }
    }
}

enum Prepared {
    Immediate(ToolOutput),
    Ready { tool: Arc<dyn Tool>, arguments: Value },
}

const ABORT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

async fn run_prepared(
    tool: Arc<dyn Tool>,
    arguments: Value,
    call: ToolCall,
    hooks: Vec<Arc<dyn ToolHooks>>,
    cancel: CancellationToken,
    sink: EventSink,
) -> (ToolCall, ToolOutput, Option<u64>) {
    let started = Instant::now();
    let ctx = ToolContext::new(call.id.clone(), call.name.clone(), cancel.clone(), sink.clone());
    let into_output = |result: anyhow::Result<ToolOutput>| match result {
        Ok(output) => output,
        Err(error) => ToolOutput::error(format!("{error:#}")),
    };
    let execution = tool.execute(arguments.clone(), ctx);
    tokio::pin!(execution);
    let mut output = tokio::select! {
        result = &mut execution => into_output(result),
        // Tools get a grace period to clean up (kill processes) before being dropped.
        _ = cancel.cancelled() => match tokio::time::timeout(ABORT_GRACE, &mut execution).await {
            Ok(result) => into_output(result),
            Err(_) => ToolOutput::error("Operation aborted"),
        },
    };
    let duration = started.elapsed().as_millis() as u64;
    for hook in &hooks {
        output = hook.after_tool_call(&call, &arguments, output).await;
    }
    emit_end(&call, &output, Some(duration), &sink);
    (call, output, Some(duration))
}

fn finish_without_stream(
    mut message: AssistantMessage,
    stop_reason: StopReason,
    error: &str,
    sink: &EventSink,
) -> AssistantMessage {
    message.stop_reason = stop_reason;
    message.error_message = Some(error.to_string());
    emit(sink, AgentEvent::MessageStart { message: Message::Assistant(message.clone()) });
    emit(sink, AgentEvent::MessageEnd { message: Message::Assistant(message.clone()) });
    message
}

fn tool_result_message(call: &ToolCall, output: ToolOutput, duration_ms: Option<u64>) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: output.content,
        details: output.details,
        is_error: output.is_error,
        timestamp: now_ms(),
        duration_ms,
    }
}

fn emit(sink: &EventSink, event: AgentEvent) {
    let _ = sink.send(event);
}

fn emit_start(call: &ToolCall, sink: &EventSink) {
    emit(
        sink,
        AgentEvent::ToolExecutionStart {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
        },
    );
}

fn emit_end(call: &ToolCall, output: &ToolOutput, duration_ms: Option<u64>, sink: &EventSink) {
    emit(
        sink,
        AgentEvent::ToolExecutionEnd {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            output: output.clone(),
            duration_ms,
        },
    );
}

fn emit_message(result: &ToolResultMessage, sink: &EventSink) {
    let message = Message::ToolResult(result.clone());
    emit(sink, AgentEvent::MessageStart { message: message.clone() });
    emit(sink, AgentEvent::MessageEnd { message });
}
