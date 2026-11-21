//! A runtime's side of the task ledger: a task is written before its run, heartbeats and the
//! ending are recorded, and a task a dead process left is taken up again from its session.

use super::*;

impl Runtime {
    /// Set who owns this runtime's tasks in the ledger.
    pub fn set_task_owner(&mut self, owner: impl Into<String>) {
        self.owner = owner.into();
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Tasks of this session that a dead process left unfinished, oldest first.
    pub fn interrupted_tasks(&self) -> Vec<TaskRecord> {
        self.ledger.interrupted(&self.session_id())
    }

    /// Record intent before the run starts (principle B6): the task exists before its effects.
    pub(super) fn begin_task(&mut self, text: &str, pulse: &PulseId, sink: &RuntimeSink) -> Option<TaskId> {
        let id = self.next_task.take().unwrap_or_else(ledger::new_task_id);
        *self.goal.lock().unwrap() = text.to_string();
        self.turns.store(0, std::sync::atomic::Ordering::SeqCst);
        self.compacted.store(false, std::sync::atomic::Ordering::SeqCst);
        let session = self.session.lock().unwrap();
        let created = LedgerEvent::Created {
            task: id.clone(),
            at: ledger::now(),
            goal: ledger::excerpt(text),
            prompt: text.to_string(),
            owner: self.owner.clone(),
            origin: self.channel.clone(),
            parent: self.parent_task.clone(),
            session_id: session.header.id.clone(),
            session_path: session.is_persistent().then(|| session.path.clone()),
            cwd: self.working_folder(),
            remote: self.remote(),
            pid: std::process::id(),
            pulse: Some(pulse.clone()),
        };
        drop(session);
        let running =
            LedgerEvent::State { task: id.clone(), at: ledger::now(), state: TaskState::Running, note: String::new() };
        if let Err(error) = self.ledger.append(&created).and_then(|_| self.ledger.append(&running)) {
            self.raise(sink, Alert::new(AlertLevel::Error, format!("Task ledger write failed: {error}")));
            return None;
        }
        // The task's notes are found from its record (decision D79).
        if let Some(workspace) = self.workspace() {
            let linked = LedgerEvent::Workspace { task: id.clone(), at: ledger::now(), workspace };
            let _ = self.ledger.append(&linked);
        }
        // And the goal it serves, with the activation that started it (decision D85).
        if let (goal, activation) = self.campaign()
            && (goal.is_some() || activation.is_some())
        {
            let _ =
                self.ledger.append(&LedgerEvent::Campaign { task: id.clone(), at: ledger::now(), goal, activation });
        }
        *self.task.lock().unwrap() = Some(id.clone());
        self.report_task(Some(crate::board_link::LinkTask {
            id: id.0.clone(),
            goal: ledger::excerpt(text),
            pulse: Some(pulse.0.clone()),
        }));
        Some(id)
    }

    fn report_task(&self, task: Option<crate::board_link::LinkTask>) {
        if let Some(board) = &self.board {
            board.task(task);
        }
    }

    /// Record how the run left its task, and hand the outcome back.
    pub(super) fn end_task(
        &mut self,
        task: Option<TaskId>,
        outcome: anyhow::Result<RunOutcome>,
        sink: &RuntimeSink,
    ) -> anyhow::Result<RunOutcome> {
        *self.task.lock().unwrap() = None;
        self.report_task(None);
        let Some(id) = task else { return outcome };
        // Stopped for a move of the hub: left running, for the next hub to take up.
        let stopped = outcome.as_ref().is_ok_and(|o| o.final_stop == Some(StopReason::Aborted));
        if stopped && super::is_handing_over(&self.paths.home) {
            return outcome.map(|mut outcome| {
                outcome.task = Some(id);
                outcome
            });
        }
        let (state, note) = match &outcome {
            Ok(outcome) => (
                TaskState::from_run(&outcome.status, outcome.final_stop == Some(StopReason::Aborted)),
                ledger::excerpt(&outcome.status.to_string()),
            ),
            Err(error) => (TaskState::Failed, ledger::excerpt(&format!("{error:#}"))),
        };
        let wake = outcome.as_ref().map(|o| o.wake.clone()).unwrap_or_default();
        let event = if state == TaskState::Running && !wake.is_empty() {
            // Continuing, with a way back: the scheduler owns the next step.
            // A race's delegations are waited on for the first of them (decision D83).
            let on = self.delegations.predicate_for(&wake).or_else(|| ledger::WaitPredicate::of_wakes(&wake));
            LedgerEvent::Waiting { task: id.clone(), at: ledger::now(), schedules: wake, note, on }
        } else {
            LedgerEvent::State { task: id.clone(), at: ledger::now(), state, note }
        };
        if let Err(error) = self.ledger.append(&event) {
            self.raise(sink, Alert::new(AlertLevel::Error, format!("Task ledger write failed: {error}")));
        }
        outcome.map(|mut outcome| {
            outcome.task = Some(id);
            outcome
        })
    }

    /// Continue a task a dead process left unfinished (Pi durable's resume): the lost attempt is
    /// counted, tool calls that were cut off are answered as interrupted instead of run again,
    /// and the run picks up from the session's transcript. Past `RESTART_LIMIT` attempts the task
    /// fails instead, so work that keeps killing the process ends.
    pub async fn resume_task(&mut self, id: &TaskId, sink: &RuntimeSink) -> anyhow::Result<RunOutcome> {
        let record = self.ledger.get(id).ok_or_else(|| anyhow!("No task {id} in the ledger"))?;
        if record.session_id != self.session_id() {
            return Err(anyhow!("Task {id} belongs to another session; open that session to resume it"));
        }
        if !record.is_interrupted() {
            return Err(anyhow!("Task {id} is {} and not interrupted", record.state.label()));
        }
        if record.attempts >= RESTART_LIMIT {
            let note = format!("interrupted {} times; not resumed again (restart limit)", record.attempts);
            self.ledger.append(&LedgerEvent::State {
                task: id.clone(),
                at: ledger::now(),
                state: TaskState::Failed,
                note: note.clone(),
            })?;
            self.raise(sink, Alert::new(AlertLevel::Error, format!("Task {id} failed: {note}")));
            return Err(anyhow!("Task {id} failed: {note}"));
        }
        self.ledger.append(&LedgerEvent::Resumed { task: id.clone(), at: ledger::now(), pid: std::process::id() })?;
        // The task goes on where it was: its notes in the vault, and the campaign it serves, are
        // pinned again before the next turn's prompt is built (decisions D78, D85).
        self.set_workspace(record.workspace.clone());
        self.set_campaign(record.goal_id.clone(), record.spawned_by_activation.clone());
        let pulse = self.bus.publish(
            PulseSource::module("ledger"),
            record.pulse.clone(),
            PulsePayload::Delegated {
                delivery: Delivery::Prompt,
                text: format!("resume {id} (attempt {}): {}", record.attempts + 1, record.goal),
            },
        );
        self.inspector.record("harness", json!({ "taskResumed": id, "attempt": record.attempts + 1 }));
        *self.task.lock().unwrap() = Some(id.clone());
        self.report_task(Some(crate::board_link::LinkTask {
            id: id.0.clone(),
            goal: record.goal.clone(),
            pulse: Some(pulse.0.clone()),
        }));
        self.inspector.set_cause(Some(pulse.clone()));
        let outcome = self.continue_interrupted(&record, &pulse, sink).await;
        self.inspector.set_cause(None);
        let outcome = self.end_task(Some(id.clone()), outcome, sink);
        if let Ok(outcome) = &outcome {
            self.reflect_on(&record.prompt, outcome);
        }
        outcome
    }

    async fn continue_interrupted(
        &mut self,
        record: &TaskRecord,
        pulse: &PulseId,
        sink: &RuntimeSink,
    ) -> anyhow::Result<RunOutcome> {
        let prompt_seen =
            self.agent.messages.iter().any(|m| matches!(m, Message::User(u) if text_of(&u.content) == record.prompt));
        if !prompt_seen {
            // The process died before the prompt reached the session.
            return self.run_prompt(Some(Message::user_text(&record.prompt)), pulse, sink).await;
        }
        // A reply cut off mid-stream ends in an aborted or errored message; it is not replayed.
        if let Some(Message::Assistant(last)) = self.agent.messages.last()
            && matches!(last.stop_reason, StopReason::Error | StopReason::Aborted)
        {
            self.agent.messages.pop();
        }
        self.answer_interrupted_calls(record);
        match self.agent.messages.last() {
            // The reply was complete; only its end state was lost.
            Some(Message::Assistant(last)) if last.tool_calls().is_empty() => {
                let last = last.clone();
                let status = status::parse(&last.text()).unwrap_or_else(|| status::synthesize(Some(&last)));
                if status.kind == status::StatusKind::Continuing && self.settings.max_continuations > 0 {
                    // It promised to go on and its process died: hold it to its word (D59).
                    let nudge = Message::user_text(status::continue_nudge(self.can_schedule()));
                    return self.run_prompt(Some(nudge), pulse, sink).await;
                }
                Ok(RunOutcome {
                    final_stop: Some(last.stop_reason),
                    error: last.error_message.clone(),
                    answer: last.text(),
                    new_messages: Vec::new(),
                    status,
                    pulse: pulse.clone(),
                    task: None,
                    wake: Vec::new(),
                    citations: Default::default(),
                })
            }
            _ => self.run_prompt(None, pulse, sink).await,
        }
    }

    /// Give every unanswered call of the last reply a result: cut off while running, or never started.
    fn answer_interrupted_calls(&mut self, record: &TaskRecord) {
        let Some(Message::Assistant(last)) =
            self.agent.messages.iter().rev().find(|m| matches!(m, Message::Assistant(_))).cloned()
        else {
            return;
        };
        let answered: Vec<String> = self
            .agent
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult(r) => Some(r.tool_call_id.clone()),
                _ => None,
            })
            .collect();
        for call in last.tool_calls().into_iter().filter(|c| !answered.contains(&c.id)) {
            let text = if record.open_tools.iter().any(|t| t.call_id == call.id) {
                "Interrupted: NyPi stopped while this tool was running, and it was not run again. Its effects may be \
                 partial; check them before retrying."
            } else {
                "Not run: NyPi stopped before this tool started."
            };
            let result = Message::ToolResult(ToolResultMessage {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                content: vec![ContentBlock::text(text)],
                details: None,
                is_error: true,
                timestamp: nypi_ai::now_ms(),
                duration_ms: None,
            });
            if let Err(error) = self.session.lock().unwrap().append_message(result.clone()) {
                self.inspector.alert(Alert::new(AlertLevel::Error, format!("Session write failed: {error}")));
            }
            self.agent.messages.push(result);
        }
    }
}
