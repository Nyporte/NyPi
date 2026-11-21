//! What can be changed on a running runtime: its model and thinking level, retry, compaction
//! and queue settings, and the user's own shell commands (`!`).

use super::*;

impl Runtime {
    pub fn set_model(&mut self, reference: &str) -> anyhow::Result<()> {
        let model = self.registry.find(reference).cloned().ok_or_else(|| anyhow!("Unknown model: {reference}"))?;
        self.switch_model(model, None)
    }

    /// Use `model` from the next request. As in Pi, it starts at `thinking` when given, else at
    /// the default level, clamped to what it accepts.
    fn switch_model(&mut self, model: Model, thinking: Option<ThinkingLevel>) -> anyhow::Result<()> {
        self.agent.config.api_key = resolve_model_key(&model, &self.paths.auth());
        self.session.lock().unwrap().append_model_change(&model.provider, &model.id)?;
        self.inspector.record("harness", json!({ "modelChange": model.reference() }));
        if let Some(board) = &self.board {
            board.model(&model.reference(), model.context_window);
        }
        self.agent.config.model = model;
        let level = thinking
            .or_else(|| ThinkingLevel::parse(&self.settings.default_thinking))
            .unwrap_or(self.agent.config.thinking);
        self.set_thinking(level)?;
        Ok(())
    }

    /// Move to the next model (Pi's `cycleModel`): through `enabledModels` when it names more than
    /// one model with a key, else through every model with a key. Returns the model, its
    /// thinking level, and whether the scope was used; None when there is nothing to move to.
    pub fn cycle_model(&mut self) -> anyhow::Result<Option<(Model, ThinkingLevel, bool)>> {
        let auth = self.paths.auth();
        let available = |m: &Model| resolve_model_key(m, &auth).is_some();
        let scope = self.model_scope();
        // As in Pi, patterns that name no model at all leave every model with a key to cycle.
        let is_scoped = !scope.is_empty();
        let choices: Vec<(Model, Option<ThinkingLevel>)> = if is_scoped {
            scope.into_iter().filter(|(m, _)| available(m)).collect()
        } else {
            self.registry.all().iter().filter(|m| available(m)).map(|m| (m.clone(), None)).collect()
        };
        if choices.len() <= 1 {
            return Ok(None);
        }
        let current = &self.agent.config.model;
        let index = choices.iter().position(|(m, _)| m.provider == current.provider && m.id == current.id);
        // As in Pi, a current model outside the choices counts as the first.
        let (model, thinking) = choices[(index.unwrap_or(0) + 1) % choices.len()].clone();
        self.switch_model(model, thinking)?;
        Ok(Some((self.agent.config.model.clone(), self.agent.config.thinking, is_scoped)))
    }

    /// The models `enabledModels` names (Pi's `resolveModelScope`). Patterns that name none were
    /// reported once, at startup.
    fn model_scope(&self) -> Vec<(Model, Option<ThinkingLevel>)> {
        resolve_model_scope(&self.settings.enabled_models, &self.registry).0
    }

    /// Set the thinking level, clamped to what the model accepts (as Pi does), and return the
    /// level in effect. Only a change is recorded in the session.
    pub fn set_thinking(&mut self, level: ThinkingLevel) -> anyhow::Result<ThinkingLevel> {
        let level = self.agent.config.model.clamp_thinking(level);
        if level != self.agent.config.thinking {
            self.agent.config.thinking = level;
            self.session.lock().unwrap().append_thinking_change(level.as_str())?;
        }
        Ok(level)
    }

    /// Run a shell command the user typed and record it in the session (Pi's `executeBash`).
    /// Its output reaches the model with the next prompt, unless `exclude_from_context` (Pi's
    /// `!!`). This is the user's own command, so the Inspector's guard does not apply; the audit
    /// log records it.
    pub async fn run_user_bash(
        &mut self,
        command: &str,
        exclude_from_context: bool,
        cancel: CancellationToken,
    ) -> anyhow::Result<crate::session::BashExecution> {
        // The owner's own command runs where they are, when that is on this computer: it is
        // their shell here, not a tool of the agent's that a device's node could carry out.
        if let Some(crate::place::Focus { device: Some(device), folder }) = self.place.focus() {
            anyhow::bail!(
                "your own shell commands run on the hub's computer, and this interface is at {folder} on {device}; \
                 ask Lucia to run it there, or run it in a terminal on {device}"
            );
        }
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c").arg(command).current_dir(self.working_folder());
        let captured = crate::tools::bash::capture(cmd, None, None, &cancel, true, |_| {}).await?;
        let exit_code = captured.exit_code();
        let run = crate::session::BashExecution {
            command: command.to_string(),
            output: captured.output,
            exit_code,
            cancelled: matches!(captured.outcome, crate::tools::bash::Outcome::Aborted),
            truncated: captured.truncated,
            full_output_path: captured.full_output_path.map(|p| p.display().to_string()),
            timestamp: nypi_ai::now_ms(),
            exclude_from_context,
        };
        self.inspector.record(
            "user",
            json!({ "bash": { "command": command, "exitCode": run.exit_code, "cancelled": run.cancelled,
                "excludeFromContext": exclude_from_context } }),
        );
        self.session.lock().unwrap().append_bash_execution(&run)?;
        if let Some(message) = run.context_message() {
            self.agent.messages.push(message);
        }
        Ok(run)
    }

    /// Turn auto-retry on or off, here and in the global settings file, as Pi does.
    pub fn set_auto_retry(&mut self, enabled: bool) -> anyhow::Result<()> {
        self.settings.retry.enabled = enabled;
        Settings::save_global(&self.paths, &["retry", "enabled"], json!(enabled))
    }

    /// Turn automatic compaction on or off, here and in the global settings file, as Pi does.
    pub fn set_auto_compaction(&mut self, enabled: bool) -> anyhow::Result<()> {
        self.settings.compaction.enabled = enabled;
        Settings::save_global(&self.paths, &["compaction", "enabled"], json!(enabled))
    }

    /// How many steering (or, with `follow_up`, follow-up) messages a drain point takes, saved
    /// in the global settings file as Pi does.
    pub fn set_queue_mode(&mut self, follow_up: bool, mode: nypi_agent::QueueMode) -> anyhow::Result<()> {
        let handle = self.agent.handle();
        let (queue, key, field) = if follow_up {
            (&handle.follow_up, "followUpMode", &mut self.settings.follow_up_mode)
        } else {
            (&handle.steering, "steeringMode", &mut self.settings.steering_mode)
        };
        queue.set_mode(mode);
        *field = mode;
        Settings::save_global(&self.paths, &[key], json!(mode))
    }

    /// Move to the next thinking level the model accepts, wrapping around (Pi's
    /// `cycleThinkingLevel`). None for a model without reasoning.
    pub fn cycle_thinking(&mut self) -> anyhow::Result<Option<ThinkingLevel>> {
        let model = &self.agent.config.model;
        if !model.reasoning {
            return Ok(None);
        }
        let levels = model.supported_thinking_levels();
        let next = match levels.iter().position(|l| *l == self.agent.config.thinking) {
            Some(index) => levels[(index + 1) % levels.len()],
            None => levels.first().copied().unwrap_or_default(),
        };
        self.set_thinking(next).map(Some)
    }
}
