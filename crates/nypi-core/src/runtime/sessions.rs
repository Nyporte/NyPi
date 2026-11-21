//! The session a runtime holds: what it has used, switching to another, forking and cloning,
//! naming, reading its entries and tree, and exporting it.

use super::*;

impl Runtime {
    /// Write the session as a static HTML page (Pi's `exportToHtml`, rendered in Rust, D53), to
    /// `output` or to `nypi-session-<file>.html` in the working directory. Returns the path.
    pub fn export_html(&self, output: Option<&str>) -> anyhow::Result<PathBuf> {
        let tools: Vec<nypi_ai::ToolDefinition> = self.agent.config.tools.iter().map(|t| t.definition()).collect();
        let context = crate::export_html::ExportContext {
            system_prompt: Some(self.agent.config.system_prompt.as_str()),
            tools: &tools,
        };
        let output = output.map(expand_home);
        let path = crate::export_html::export(&self.session.lock().unwrap(), &context, output.as_deref(), &self.cwd)?;
        self.inspector.record("harness", json!({ "exportHtml": path }));
        Ok(path)
    }

    /// Message counts, token and cost totals, and how full the context is (Pi's
    /// `getSessionStats`).
    pub fn session_stats(&self) -> SessionStats {
        let session = self.session.lock().unwrap();
        let mut stats = SessionStats {
            session_file: session.is_persistent().then(|| session.path.clone()),
            session_id: session.header.id.clone(),
            ..Default::default()
        };
        for entry in session.entries() {
            let crate::session::Entry::Message { message, .. } = entry else { continue };
            stats.total_messages += 1;
            match message {
                Message::User(_) => stats.user_messages += 1,
                Message::ToolResult(_) => stats.tool_results += 1,
                Message::Assistant(assistant) => {
                    stats.assistant_messages += 1;
                    stats.tool_calls += assistant.tool_calls().len();
                    let usage = &assistant.usage;
                    stats.tokens.input += usage.input;
                    stats.tokens.output += usage.output;
                    stats.tokens.cache_read += usage.cache_read;
                    stats.tokens.cache_write += usage.cache_write;
                    stats.cost += usage.cost.total;
                }
                Message::System(_) => {}
            }
        }
        let tokens = &mut stats.tokens;
        tokens.total = tokens.input + tokens.output + tokens.cache_read + tokens.cache_write;
        stats.context_usage = self.context_usage(&session);
        stats
    }

    /// How full the context is. Unknown right after a compaction, until a reply reports usage
    /// again: the usage before it counted what the summary replaced (Pi's `getContextUsage`).
    fn context_usage(&self, session: &SessionStore) -> Option<ContextUsage> {
        let context_window = self.agent.config.model.context_window;
        if context_window == 0 {
            return None;
        }
        let branch = session.branch();
        if let Some(compaction) = branch.iter().rposition(|e| matches!(e, crate::session::Entry::Compaction { .. }))
            && !branch[compaction + 1..].iter().any(|e| {
                matches!(e, crate::session::Entry::Message { message: Message::Assistant(a), .. }
                    if !matches!(a.stop_reason, StopReason::Error | StopReason::Aborted) && a.usage.context_tokens() > 0)
            })
        {
            return Some(ContextUsage { tokens: None, context_window, percent: None });
        }
        let tokens = compaction::context_tokens(&session.context().messages);
        let percent = tokens as f64 / context_window as f64 * 100.0;
        Some(ContextUsage { tokens: Some(tokens), context_window, percent: Some(percent) })
    }

    /// Continue in the session file at `path`, with the model and thinking level it last used
    /// (Pi's `switchSession`).
    pub fn switch_session(&mut self, path: &std::path::Path) -> anyhow::Result<()> {
        self.adopt(SessionStore::open(path)?, json!({ "sessionSwitched": path }));
        Ok(())
    }

    /// Continue in `session`, with the model and thinking level it last used.
    fn adopt(&mut self, session: SessionStore, audit: serde_json::Value) {
        let context = session.context();
        if let Some(model) = context.model.as_deref().and_then(|r| self.registry.find(r)).cloned() {
            self.agent.config.api_key = resolve_model_key(&model, &self.paths.auth());
            self.agent.config.model = model;
        }
        if let Some(level) = context.thinking_level.as_deref().and_then(ThinkingLevel::parse) {
            self.agent.config.thinking = self.agent.config.model.clamp_thinking(level);
        }
        self.inspector.set_session(&session.header.id);
        self.inspector.record("harness", audit);
        self.agent.config.session_id = Some(session.header.id.clone());
        *self.citations.lock().unwrap() = crate::citations::CitationIndex::from_messages(&context.messages);
        self.agent.messages = context.messages;
        if let Some(board) = &self.board {
            let model = &self.agent.config.model;
            board.model(&model.reference(), model.context_window);
            board.session(&session.header.id);
        }
        *self.session.lock().unwrap() = session;
    }

    /// Continue from just before the user message `entry_id`, in a new session holding the path
    /// up to there (Pi's `fork`). Returns that message's text, for the user to edit and send.
    pub fn fork(&mut self, entry_id: &str) -> anyhow::Result<String> {
        let (session, text) = {
            let current = self.session.lock().unwrap();
            let Some(crate::session::Entry::Message { meta, message: Message::User(user) }) =
                current.entries().iter().find(|e| e.id() == entry_id)
            else {
                bail!("Invalid entry ID for forking");
            };
            let session = match &meta.parent_id {
                Some(parent) => current.branched(parent)?,
                None => {
                    let mut child = current.new_child();
                    let model = &self.agent.config.model;
                    child.append_model_change(&model.provider, &model.id)?;
                    child
                }
            };
            (session, text_of(&user.content))
        };
        self.adopt(session, json!({ "sessionForked": entry_id }));
        Ok(text)
    }

    /// Continue in a copy of the current branch (Pi's `clone`).
    pub fn clone_session(&mut self) -> anyhow::Result<()> {
        let session = {
            let current = self.session.lock().unwrap();
            let Some(leaf) = current.leaf() else { bail!("Cannot clone session: no current entry selected") };
            current.branched(leaf)?
        };
        self.adopt(session, json!({ "sessionCloned": true }));
        Ok(())
    }

    /// Every user message in the file with its entry id: the places a fork can start from.
    pub fn fork_messages(&self) -> Vec<(String, String)> {
        let session = self.session.lock().unwrap();
        session
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                crate::session::Entry::Message { meta, message: Message::User(user) } => {
                    Some((meta.id.clone(), text_of(&user.content))).filter(|(_, text)| !text.is_empty())
                }
                _ => None,
            })
            .collect()
    }

    pub fn set_session_name(&mut self, name: &str) -> anyhow::Result<()> {
        if name.trim().is_empty() {
            bail!("Session name cannot be empty");
        }
        self.session.lock().unwrap().append_session_info(name)?;
        Ok(())
    }

    pub fn session_name(&self) -> Option<String> {
        self.session.lock().unwrap().session_name()
    }

    /// The file's entries as stored, after the one with id `since` when given, and the leaf.
    pub fn session_entries(&self, since: Option<&str>) -> anyhow::Result<(Vec<serde_json::Value>, Option<String>)> {
        let session = self.session.lock().unwrap();
        let entries = session.entries();
        let start = match since {
            Some(id) => entries.iter().position(|e| e.id() == id).ok_or_else(|| anyhow!("Entry not found: {id}"))? + 1,
            None => 0,
        };
        Ok((entries[start..].iter().map(|e| e.to_json()).collect(), session.leaf().map(str::to_string)))
    }

    /// The session tree (Pi's `getTree`) and the leaf.
    pub fn session_tree(&self) -> (Vec<serde_json::Value>, Option<String>) {
        let session = self.session.lock().unwrap();
        (session.tree_json(), session.leaf().map(str::to_string))
    }

    /// Start a fresh session, keeping model and settings.
    pub fn new_session(&mut self) -> anyhow::Result<()> {
        let persistent = self.session.lock().unwrap().is_persistent();
        let mut session = if persistent {
            SessionStore::create(&self.paths.sessions(), &self.cwd)
        } else {
            SessionStore::ephemeral(&self.cwd)
        };
        let model = &self.agent.config.model;
        session.append_model_change(&model.provider, &model.id)?;
        self.inspector.set_session(&session.header.id);
        self.agent.config.session_id = Some(session.header.id.clone());
        if let Some(board) = &self.board {
            board.session(&session.header.id);
        }
        *self.session.lock().unwrap() = session;
        self.agent.messages.clear();
        *self.citations.lock().unwrap() = crate::citations::CitationIndex::from_messages(&[]);
        Ok(())
    }
}
