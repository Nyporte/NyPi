//! Running: a prompt becomes a task, the agent loop runs with recall, retries, recovery and
//! compaction around it, the status line is enforced, and a reflection looks back afterwards.

use super::*;

impl Runtime {
    /// Run one user prompt to completion, persisting and auditing every message.
    pub async fn prompt(&mut self, text: &str, sink: &RuntimeSink) -> anyhow::Result<RunOutcome> {
        self.prompt_message(Message::user_text(text), sink).await
    }

    pub async fn prompt_message(&mut self, message: Message, sink: &RuntimeSink) -> anyhow::Result<RunOutcome> {
        let pulse = self.bus.publish(
            PulseSource::user(&self.channel),
            None,
            PulsePayload::UserMessage { delivery: Delivery::Prompt, text: message_text(&message) },
        );
        self.prompt_caused(message, pulse, sink).await
    }

    /// Run a prompt whose pulse the caller already published (a module delegating to this runtime).
    pub async fn prompt_caused(
        &mut self,
        message: Message,
        pulse: PulseId,
        sink: &RuntimeSink,
    ) -> anyhow::Result<RunOutcome> {
        let text = message_text(&message);
        let mut message = message;
        self.open_valve(&mut message, &text, &pulse).await;
        let task = self.begin_task(&text, &pulse, sink);
        self.inspector.set_cause(Some(pulse.clone()));
        let outcome = self.run_prompt(Some(message), &pulse, sink).await;
        self.inspector.set_cause(None);
        let outcome = self.end_task(task, outcome, sink);
        if let Ok(outcome) = &outcome {
            self.reflect_on(&text, outcome);
        }
        if let Some(portrait) = &self.portrait {
            portrait.after_turn();
        }
        outcome
    }

    /// The portrait keeper, for a conversational module in front of this runtime (chat).
    pub fn portrait(&self) -> Option<Arc<crate::portrait::PortraitKeeper>> {
        self.portrait.clone()
    }

    /// The memory valve, for a conversational module in front of this runtime (chat).
    pub fn valve(&self) -> Option<crate::valve::MemoryValve> {
        self.valve.clone()
    }

    /// Let the memory valve attach what bears on `text` (decision D32), and publish its decision.
    async fn open_valve(&mut self, message: &mut Message, text: &str, pulse: &PulseId) {
        let Some(valve) = &self.valve else { return };
        let Some(decision) = valve.consider(text, &self.agent.messages).await else { return };
        crate::valve::attach(message, &decision);
        self.bus.publish(PulseSource::module("memory"), Some(pulse.clone()), PulsePayload::MemoryValve { decision });
    }

    /// Look back on a finished task in the background when it is likely to teach something
    /// (principle A8). The run's answer never waits for it.
    pub(super) fn reflect_on(&mut self, goal: &str, outcome: &RunOutcome) {
        let Some(task) = outcome.task.clone() else { return };
        let review = crate::reflection::RunReview {
            task: &task,
            goal,
            status: &outcome.status,
            answer: &outcome.answer,
            messages: &outcome.new_messages,
        };
        let settings = &self.settings.reflection;
        // Scripted test models reflect only when asked: a reflection would eat the next scripted reply.
        let scripted = self.agent.config.model.api == nypi_ai::Api::Faux;
        if !crate::reflection::qualifies(settings, &review)
            || (scripted && settings.mode != crate::reflection::ReflectionMode::Always)
        {
            return;
        }
        let model = settings
            .model
            .as_deref()
            .and_then(|r| self.registry.find(r).cloned())
            .unwrap_or_else(|| self.agent.config.model.clone());
        let api_key = if model == self.agent.config.model {
            self.agent.config.api_key.clone()
        } else {
            resolve_model_key(&model, &self.paths.auth())
        };
        let agent_name = self.settings.agent_name.clone();
        let self_prompt = std::fs::read_to_string(self.paths.self_prompt(&agent_name)).unwrap_or_default();
        let prompt = crate::reflection::build_prompt(&review, &self_prompt);
        let (stream_fn, paths, inspector, goal) =
            (self.agent.config.stream_fn.clone(), self.paths.clone(), self.inspector.clone(), goal.to_string());
        let retry = self.settings.retry.clone();
        self.reflections.push(tokio::spawn(async move {
            let reflection =
                match crate::reflection::reflect(&model, &stream_fn, api_key, prompt, &retry, CancellationToken::new())
                    .await
                {
                    Ok(reflection) => reflection,
                    Err(error) => {
                        inspector.record("reflection", json!({ "task": task, "error": format!("{error:#}") }));
                        return None;
                    }
                };
            match crate::reflection::apply(&paths, &agent_name, &task, &goal, &reflection).await {
                Ok(applied) => {
                    inspector
                        .record("reflection", json!({ "task": task, "reflection": reflection, "applied": applied }));
                    applied.summary(&task)
                }
                Err(error) => {
                    inspector.alert(Alert::new(
                        AlertLevel::Warning,
                        format!("Could not save the reflection on {task}: {error:#}"),
                    ));
                    None
                }
            }
        }));
    }

    /// Summaries of reflections that have finished, without waiting for the others.
    pub fn finished_reflections(&mut self) -> Vec<String> {
        let (done, running): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.reflections).into_iter().partition(|h| h.is_finished());
        self.reflections = running;
        done.into_iter().filter_map(futures::FutureExt::now_or_never).filter_map(|r| r.ok().flatten()).collect()
    }

    /// Wait up to `limit` for running reflections (a process about to exit).
    pub async fn wait_reflections(&mut self, limit: std::time::Duration) -> Vec<String> {
        let handles = std::mem::take(&mut self.reflections);
        let mut summaries = Vec::new();
        let deadline = tokio::time::Instant::now() + limit;
        // A portrait pass on the last messages is background work of the same kind.
        if let Some(portrait) = &self.portrait {
            portrait.wait(limit).await;
        }
        for handle in handles {
            if let Ok(Ok(Some(summary))) = tokio::time::timeout_at(deadline, handle).await {
                summaries.push(summary);
            }
        }
        summaries
    }

    /// Run a front end's job: a prompt, or the resumption of an interrupted task.
    pub async fn run_job(&mut self, job: &Job, sink: &RuntimeSink) -> anyhow::Result<RunOutcome> {
        match job {
            Job::Prompt(text) => self.prompt(text, sink).await,
            Job::Resume(task) => self.resume_task(task, sink).await,
            Job::Sourced { text, source, cause } => {
                let pulse = self.bus.publish(
                    source.clone(),
                    cause.clone(),
                    PulsePayload::UserMessage { delivery: Delivery::Prompt, text: text.clone() },
                );
                self.prompt_caused(Message::user_text(text), pulse, sink).await
            }
        }
    }

    pub(super) async fn run_prompt(
        &mut self,
        message: Option<Message>,
        pulse: &PulseId,
        sink: &RuntimeSink,
    ) -> anyhow::Result<RunOutcome> {
        self.refresh_mcp_tools(sink).await;
        self.ensure_observer();
        self.refresh_folder(sink);
        // A run at a folder on another device reads that folder's context files there.
        *self.place_context.lock().unwrap() = self.place.remote_context().await;
        self.apply_place_context();
        self.refresh_system_prompt();
        // As in Pi, an abort during this compaction is an abort of the prompt.
        if self.compact_if_needed("context nearly full", sink).await {
            self.agent.abort_next_run();
        }
        self.sync_transcript();
        let schedules_before = self.active_schedules();
        let mut new_messages = self.run_agent(message, sink).await;
        if let Some(recovery) = self.recovery_for(&new_messages) {
            new_messages.extend(self.recover(recovery, sink).await);
        }
        let main_answer = last_text(&new_messages);
        let mut answer = main_answer.clone();
        let mut status = last_assistant(&new_messages)
            .filter(|a| a.stop_reason == StopReason::Stop)
            .and_then(|a| status::parse(&a.text()));
        if status.is_none()
            && self.settings.enforce_status_line
            && last_assistant(&new_messages).is_some_and(|a| a.stop_reason == StopReason::Stop)
        {
            self.inspector.record("harness", json!({ "statusNudge": true }));
            let nudged = self.run_agent(Some(Message::user_text(STATUS_NUDGE)), sink).await;
            if let Some(reply) = last_assistant(&nudged).filter(|a| a.stop_reason == StopReason::Stop) {
                status = status::parse(&reply.text());
                let reply = reply.text();
                if !reply.trim().is_empty() {
                    answer = if main_answer.trim().is_empty() { reply } else { format!("{main_answer}\n\n{reply}") };
                }
            }
            new_messages.extend(nudged);
        }
        // Continuing is a promise, and the turn that made it is over (decision D59): it stands
        // only with a schedule set during this run to bring the agent back. Otherwise the agent
        // is told to go on, a bounded number of times, and then the user is told it is stuck.
        let mut wake = Vec::new();
        let mut nudges = 0;
        while let Some(promise) = status.clone().filter(|s| s.kind == status::StatusKind::Continuing) {
            wake = self.active_schedules().difference(&schedules_before).cloned().collect();
            // A specialist working in the background brings the agent back as surely as a schedule,
            // whichever run started it.
            wake.extend(self.delegations.working());
            if !wake.is_empty() || self.settings.max_continuations == 0 {
                break;
            }
            if nudges == self.settings.max_continuations {
                let unkept = status::unkept_promise(&promise, nudges);
                self.raise(sink, Alert::new(AlertLevel::Warning, format!("{unkept}")));
                status = Some(unkept);
                break;
            }
            nudges += 1;
            self.inspector.record("harness", json!({ "continueNudge": nudges, "promise": &promise.text }));
            let nudge = Message::user_text(status::continue_nudge(self.can_schedule()));
            let continued = self.run_agent(Some(nudge), sink).await;
            let reply = last_assistant(&continued).cloned();
            new_messages.extend(continued);
            match reply {
                Some(reply) if reply.stop_reason == StopReason::Stop => {
                    let text = last_text(&new_messages);
                    if !text.trim().is_empty() && !answer.ends_with(text.trim_end()) {
                        answer = format!("{}\n\n{text}", answer.trim_end());
                    }
                    // A reply without a status line leaves the promise standing, to be asked again.
                    status = status::parse(&reply.text()).or(Some(promise));
                }
                // An error, an abort, or a cut-off reply: the stop reason says how the run ended.
                _ => status = None,
            }
        }
        // A task with a workspace ends with its dossier (decision D87): once, a Done or Blocked
        // without a report.md that cites what the run did is sent back to write it.
        if let Some(nudge) = self.report_missing(status.as_ref(), &new_messages) {
            self.inspector.record("harness", json!({ "reportNudge": true }));
            let written = self.run_agent(Some(Message::user_text(nudge)), sink).await;
            if let Some(reply) = last_assistant(&written).filter(|a| a.stop_reason == StopReason::Stop) {
                let text = reply.text();
                if let Some(again) = status::parse(&text) {
                    status = Some(again);
                }
                if !text.trim().is_empty() && !answer.ends_with(text.trim_end()) {
                    answer = format!("{}\n\n{}", answer.trim_end(), text.trim());
                }
            }
            new_messages.extend(written);
        }
        // As in Pi, an aborted run is not followed by compaction: the user asked for a stop.
        if last_assistant(&new_messages).is_none_or(|a| a.stop_reason != StopReason::Aborted) {
            self.compact_if_needed("context nearly full", sink).await;
        }

        let last = last_assistant(&new_messages).cloned();
        // Stopped for making no progress: that, not "stopped by the user", is how it ended.
        if let Some(quiet) = self.stuck.lock().unwrap().take() {
            status = Some(status::RunStatus {
                kind: status::StatusKind::Failed,
                text: format!(
                    "it made no progress for {} minutes (a model stream, MCP call, or tool that never answered) and \
                     was stopped; the task can be resumed with nypi tasks",
                    quiet.as_secs() / 60
                ),
                synthesized: true,
            });
        }
        let status = status.unwrap_or_else(|| {
            let derived = status::synthesize(last.as_ref());
            if derived.kind == status::StatusKind::Done {
                self.raise(sink, Alert::new(AlertLevel::Warning, "The run ended without a status line"));
            }
            derived
        });
        self.inspector.record("harness", json!({ "runStatus": &status }));
        let tool_results = new_messages.iter().filter(|m| matches!(m, Message::ToolResult(_))).count();
        let citations = self.citations.lock().unwrap().resolve(&answer, &self.agent.messages, tool_results);
        if !citations.is_empty() {
            self.inspector.record("harness", json!({ "citations": &citations }));
        }
        self.bus.publish(
            PulseSource::module(RUNTIME_MODULE),
            Some(pulse.clone()),
            PulsePayload::RunSettled { status: status.clone(), answer: answer.clone() },
        );
        let outcome = RunOutcome {
            final_stop: last.as_ref().map(|a| a.stop_reason),
            error: last.as_ref().and_then(|a| a.error_message.clone()),
            new_messages,
            status,
            answer,
            pulse: pulse.clone(),
            task: None,
            wake,
            citations,
        };
        if outcome.final_stop == Some(StopReason::Error) {
            self.raise(
                sink,
                Alert::new(AlertLevel::Error, format!("Run failed: {}", outcome.error.clone().unwrap_or_default())),
            );
        }
        Ok(outcome)
    }

    /// Run the agent, retrying a reply that failed with a transient provider error, as Pi does:
    /// wait with exponential backoff (an abort ends the wait), drop the failed reply from the
    /// transcript, and continue from where it stopped. The failed reply stays in the session file
    /// and in the returned messages; providers never replay an errored reply (transform.rs).
    async fn run_agent(&mut self, prompt: Option<Message>, sink: &RuntimeSink) -> Vec<Message> {
        let mut added = self.run_agent_once(prompt, sink).await;
        let mut batch = 0;
        loop {
            let retry = self.retry.clone();
            let next = retry.after_run(&self.settings.retry, &added[batch..], |notice| {
                report_retry(&self.inspector, self.board.as_deref(), sink, notice)
            });
            let Next::Retry { delay_ms } = next else { break };
            let heartbeat = self.spawn_heartbeat();
            let waited = nypi_ai::retry::back_off(delay_ms, &self.agent.abort_signal()).await;
            if let Some(heartbeat) = heartbeat {
                heartbeat.abort();
            }
            if !waited {
                if let Some(notice) = retry.cancelled() {
                    report_retry(&self.inspector, self.board.as_deref(), sink, notice);
                }
                break;
            }
            crate::retry::drop_failed_reply(&mut self.agent);
            batch = added.len();
            added.extend(self.run_agent_once(None, sink).await);
        }
        added
    }

    /// The nudge to write `report.md`, when this run should have: it is on a workspace, it ended
    /// Done or Blocked, and the workspace has no report, or one that cites none of the run's
    /// tool results although it ran tools (decision D87).
    fn report_missing(&self, status: Option<&RunStatus>, added: &[Message]) -> Option<String> {
        let kind = status?.kind;
        if !matches!(kind, status::StatusKind::Done | status::StatusKind::Blocked)
            || !self.settings.tasks.require_report.applies(self.agent.config.model.api)
        {
            return None;
        }
        let workspace = self.workspace()?;
        let vault = self.vault.as_ref()?;
        let path = format!("{workspace}/{}", crate::evidence::REPORT_FILE);
        let report = vault.read(&path).unwrap_or_default();
        let ran = added.iter().any(|m| matches!(m, Message::ToolResult(_)));
        let index = self.citations.lock().unwrap();
        let verified = crate::citations::references(&report).iter().any(|reference| index.get(reference).is_some());
        drop(index);
        if !report.trim().is_empty() && (verified || !ran) {
            return None;
        }
        let why = if report.trim().is_empty() { "there is no report.md" } else { "report.md cites no tool result" };
        Some(format!(
            "[NyPi harness] Before this task settles, write {path} with vault_write ({why}). Three sections: \
             `## Contract Check` (each item of brief.md's Done when, met or not, with its [tN]); `## Key Findings & \
             Deliverables` (exact paths and numbers, with the [tN] and findings.md entries they rest on); `## \
             Falsified Paths & Residual Risks` (dead ends with their [tN], what is still uncertain, and when Blocked, \
             the options). Cite only results that show what you claim: the harness checks every [tN]. Then end with \
             your status line again."
        ))
    }

    /// Ids of the schedules that can still fire.
    fn active_schedules(&self) -> std::collections::BTreeSet<String> {
        crate::schedule::ScheduleStore::new(self.paths.schedules()).active().into_iter().map(|s| s.id).collect()
    }

    /// Whether the agent has the `schedule` tool, its only way to come back later on its own.
    pub(super) fn can_schedule(&self) -> bool {
        self.agent.config.tools.iter().any(|tool| tool.definition().name == "schedule")
    }

    /// Tell the ledger the running task is alive until the handle is aborted; none without a task.
    fn spawn_heartbeat(&self) -> Option<tokio::task::JoinHandle<()>> {
        self.task.lock().unwrap().clone().map(|task| {
            let (ledger, inspector) = (self.ledger.clone(), self.inspector.clone());
            tokio::spawn(async move {
                let mut beat = tokio::time::interval(HEARTBEAT_INTERVAL);
                beat.tick().await;
                loop {
                    beat.tick().await;
                    let event = LedgerEvent::Heartbeat { task: task.clone(), at: ledger::now() };
                    if let Err(error) = ledger.append(&event) {
                        inspector.alert(Alert::new(AlertLevel::Error, format!("Task ledger write failed: {error}")));
                    }
                }
            })
        })
    }

    async fn run_agent_once(&mut self, prompt: Option<Message>, sink: &RuntimeSink) -> Vec<Message> {
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let session = self.session.clone();
        let inspector = self.inspector.clone();
        let outer = sink.clone();
        let bus = self.bus.clone();
        let citations = self.citations.clone();
        let cause = self.inspector.cause();
        let board = self.board.clone();
        let retry = self.retry.clone();
        let heartbeat = self.spawn_heartbeat();
        let (task, turns, compacted) = (self.task.clone(), self.turns.clone(), self.compacted.clone());
        let progress = Arc::new(Mutex::new(tokio::time::Instant::now()));
        let stuck = self.spawn_stuck_watch(&progress);
        *self.turn_run.lock().unwrap() = Some(TurnSettings { sink: sink.clone(), prompt: self.prompt_sources() });
        let forwarder = tokio::spawn(async move {
            // A tool's partial output is progress only when it changed: a tool may repeat what
            // it has so far on a timer (`bash` does, twice a second) while nothing happens.
            let mut partials: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
            // The turn's tool calls and what each came to, for the observer of the run's course.
            let mut asked: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            let mut digest: Vec<String> = Vec::new();
            while let Some(event) = rx.recv().await {
                match &event {
                    AgentEvent::ToolExecutionStart { call_id, tool_name, arguments } => {
                        let summary: String = call_summary(tool_name, arguments).chars().take(160).collect();
                        asked.insert(call_id.clone(), format!("{tool_name} {summary}"));
                    }
                    AgentEvent::ToolExecutionEnd { call_id, output, .. } => {
                        let call = asked.remove(call_id).unwrap_or_default();
                        let came: String =
                            nypi_ai::text_of(&output.content).split_whitespace().collect::<Vec<_>>().join(" ");
                        let came: String = came.chars().take(200).collect();
                        let state = if output.is_error { "FAILED" } else { "ok" };
                        digest.push(format!("- {call} -> {state}: {came}"));
                    }
                    AgentEvent::TurnEnd { .. } => {
                        let turn_index = turns.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        bus.publish(
                            PulseSource::module(RUNTIME_MODULE),
                            cause.clone(),
                            PulsePayload::TurnCompleted {
                                task: task.lock().unwrap().as_ref().map(|task| task.0.clone()),
                                turn_index,
                                recent_calls_digest: std::mem::take(&mut digest),
                                compacted: compacted.swap(false, std::sync::atomic::Ordering::SeqCst),
                            },
                        );
                    }
                    _ => {}
                }
                let moved = match &event {
                    AgentEvent::ToolExecutionUpdate { call_id, partial, .. } => {
                        use std::hash::{Hash, Hasher};
                        let mut hasher = std::collections::hash_map::DefaultHasher::new();
                        serde_json::to_string(partial).unwrap_or_default().hash(&mut hasher);
                        partials.insert(call_id.clone(), hasher.finish()) != Some(hasher.finish())
                    }
                    _ => true,
                };
                if moved {
                    *progress.lock().unwrap() = tokio::time::Instant::now();
                }
                inspector.record_event(&event);
                if let Some(payload) = tool_pulse(&event) {
                    let finished = match &payload {
                        PulsePayload::ToolFinished { call_id, .. } => Some(call_id.clone()),
                        _ => None,
                    };
                    let id = bus.publish(PulseSource::module(RUNTIME_MODULE), cause.clone(), payload);
                    if let Some(call_id) = finished {
                        citations.lock().unwrap().set_pulse(&call_id, id);
                    }
                }
                // Persist at message end, so a crash mid-run keeps everything completed so far.
                if let AgentEvent::MessageEnd { message } = &event
                    && let Err(error) = session.lock().unwrap().append_message(message.clone())
                {
                    inspector.alert(Alert::new(AlertLevel::Error, format!("Session write failed: {error}")));
                }
                let ended = retry.observe(&event);
                let event = RuntimeEvent::Agent { event };
                if let Some(board) = &board {
                    board.event(&event);
                }
                let _ = outer.send(event);
                if let Some(notice) = ended {
                    report_retry(&inspector, board.as_deref(), &outer, notice);
                }
            }
        });
        let added = match prompt {
            Some(message) => self.agent.prompt(vec![message], &tx).await,
            None => self.agent.continue_run(&tx).await,
        };
        *self.turn_run.lock().unwrap() = None;
        drop(tx);
        let _ = forwarder.await;
        if let Some(heartbeat) = heartbeat {
            heartbeat.abort();
        }
        if let Some(stuck) = stuck {
            stuck.abort();
        }
        added
    }

    /// Stop the run once nothing at all has happened in it for `stuckMinutes`: the heartbeat
    /// says the process lives, not that the run moves, so without this a hung stream or tool
    /// would keep its conversation busy, and everything queued behind it waiting, for good.
    fn spawn_stuck_watch(&self, progress: &Arc<Mutex<tokio::time::Instant>>) -> Option<tokio::task::JoinHandle<()>> {
        let limit = std::time::Duration::try_from_secs_f64(self.settings.stuck_minutes * 60.0).ok()?;
        if limit.is_zero() {
            return None;
        }
        let (progress, stuck, agent, inspector) =
            (progress.clone(), self.stuck.clone(), self.agent.handle(), self.inspector.clone());
        let check = (limit / 10).clamp(std::time::Duration::from_millis(50), std::time::Duration::from_secs(30));
        Some(tokio::spawn(async move {
            loop {
                tokio::time::sleep(check).await;
                if inspector.is_asking() {
                    // Waiting for the user is the approval timeout's to end, not this.
                    *progress.lock().unwrap() = tokio::time::Instant::now();
                    continue;
                }
                let quiet = progress.lock().unwrap().elapsed();
                if quiet >= limit {
                    *stuck.lock().unwrap() = Some(quiet);
                    let minutes = quiet.as_secs().div_ceil(60);
                    inspector.record("harness", json!({ "stuck": { "quietSeconds": quiet.as_secs() } }));
                    inspector.alert(Alert::new(
                        AlertLevel::Error,
                        format!("The run made no progress for {minutes} minutes and was stopped"),
                    ));
                    agent.abort();
                    return;
                }
            }
        }))
    }

    /// Whether the run's final reply calls for context recovery (Pi's `_checkCompaction`, cases 1
    /// and 2): a context overflow, or a reply cut off below the model's output limit. Only a reply
    /// from the current model counts: another model's limits say nothing about this one.
    fn recovery_for(&self, messages: &[Message]) -> Option<Recovery> {
        let Some(Message::Assistant(last)) = messages.last() else { return None };
        let model = &self.agent.config.model;
        if !self.settings.compaction.enabled
            || last.stop_reason == StopReason::Aborted
            || last.provider != model.provider
            || last.model != model.id
        {
            return None;
        }
        let overflow = nypi_ai::overflow::is_context_overflow(last, Some(model.context_window));
        // Only a stop below the limit the request asked for says the reply was cut short; when the
        // server's default limit applied, the stop may be that default.
        let asked = nypi_ai::requested_max_tokens(model, self.agent.config.max_tokens).unwrap_or(0);
        if !overflow && !nypi_ai::overflow::is_recoverable_length(last, asked) {
            return None;
        }
        Some(Recovery { failed: last.clone(), overflow })
    }

    /// Compact, then continue the run once from before the failed reply. A completed reply that
    /// overflowed silently cannot be continued, so it is only compacted. The failed reply leaves
    /// model context for good through a context edit, as Pi's `_omitRecoveryAttempt` does, so a
    /// cut-off reply is never replayed; it stays in the session file.
    async fn recover(&mut self, recovery: Recovery, sink: &RuntimeSink) -> Vec<Message> {
        let reason = if recovery.overflow { "context overflow" } else { "truncated reply" };
        if recovery.failed.stop_reason == StopReason::Stop {
            let _ = self.compact(reason, sink).await;
            return Vec::new();
        }
        let failed = Message::Assistant(recovery.failed);
        let omitted = {
            let mut session = self.session.lock().unwrap();
            session.entry_id_of(&failed).map(|id| session.append_context_edit(&id, None))
        };
        if let Some(Err(error)) = omitted {
            self.raise(sink, Alert::new(AlertLevel::Error, format!("Session write failed: {error:#}")));
        }
        self.agent.messages.retain(|m| *m != failed);
        if self.compact(reason, sink).await.is_err() {
            return Vec::new();
        }
        let added = self.run_agent(None, sink).await;
        if self.recovery_for(&added).is_some_and(|again| again.failed.stop_reason != StopReason::Stop) {
            let message = if recovery.overflow {
                "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or \
                 switching to a larger-context model."
            } else {
                "Truncated response recovery failed after one compact-and-retry attempt."
            };
            self.raise(sink, Alert::new(AlertLevel::Error, message));
        }
        added
    }

    /// Compact when the context is nearly full. True when an abort stopped it.
    async fn compact_if_needed(&mut self, reason: &str, sink: &RuntimeSink) -> bool {
        let tokens = compaction::context_tokens(&self.agent.messages);
        if !compaction::should_compact(tokens, &self.agent.config.model, &self.settings.compaction) {
            return false;
        }
        // `compact` opens its abort scope first, so this is its own, never a stale one.
        self.compact(reason, sink).await.is_err() && self.agent.abort_signal().is_cancelled()
    }

    /// Summarize older history now. Errors are reported as alerts and returned.
    pub async fn compact(&mut self, reason: &str, sink: &RuntimeSink) -> anyhow::Result<()> {
        let abort = self.agent.new_abort_scope();
        let messages = self
            .compactor()
            .compact(&self.agent.config, &self.settings.compaction, &self.settings.retry, reason, sink, abort)
            .await?;
        self.agent.messages = messages;
        Ok(())
    }

    pub(super) fn compactor(&self) -> Compactor {
        Compactor {
            session: self.session.clone(),
            inspector: self.inspector.clone(),
            bus: self.bus.clone(),
            board: self.board.clone(),
            compacted: self.compacted.clone(),
        }
    }
}
