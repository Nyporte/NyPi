//! What the full-screen UI shows, updated from the records of the thread it is attached to. No
//! terminal I/O here, so every update can be tested.

use std::collections::VecDeque;
use std::time::Instant;

use nypi_agent::AgentEvent;
use nypi_ai::{Message, StopReason, StreamEvent};
use nypi_core::inspector::Alert;
use nypi_core::retry::RetryNotice;
use nypi_core::runtime::RuntimeEvent;
use nypi_core::worker::{ToolActivity, ToolState};
use serde_json::Value;

use super::input::Input;
use crate::client::{runtime_event, said};
use crate::render::{retry_note, tool_summary};

/// Lines kept for the activity panel.
const ACTIVITY_LIMIT: usize = 200;
/// Tool calls kept for the task panel.
const TOOLS_KEPT: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    /// The chat module, or the single agent when chat is off.
    Lucia,
    /// The worker, when chat is on.
    Task,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    User(String),
    Reply { speaker: Speaker, text: String, thinking: String },
    Tool { call_id: String, line: String, state: ToolState },
    Notice { text: String, level: Level },
}

/// The request waiting for the user's yes or no.
#[derive(Clone, Debug)]
pub struct ApprovalView {
    pub id: String,
    pub tool: String,
    pub summary: String,
    pub reason: String,
    pub waiting: usize,
}

/// The task under way, as the panel shows it.
#[derive(Clone, Debug)]
pub struct TaskView {
    pub task: String,
    pub started: Instant,
    pub tools: Vec<ToolActivity>,
}

impl TaskView {
    fn new(task: &str) -> Self {
        TaskView { task: task.to_string(), started: Instant::now(), tools: Vec::new() }
    }

    /// A few lines for `/task`.
    pub fn describe(&self) -> String {
        let mut text = format!("Running for {} s: {}", self.started.elapsed().as_secs(), self.task);
        for tool in &self.tools {
            let state = match tool.state {
                ToolState::Running => "running",
                ToolState::Ok => "ok",
                ToolState::Failed => "failed",
            };
            text.push_str(&format!("\n- {} {} ({state})", tool.tool, tool.summary));
        }
        text
    }
}

pub struct App {
    pub agent_name: String,
    pub chat_model: Option<String>,
    pub worker_model: String,
    /// Where this interface is, as the hub took it (decision D79): what is asked here is done there.
    pub focus: Option<String>,
    pub show_thinking: bool,
    pub entries: Vec<Entry>,
    pub input: Input,
    /// Lines scrolled up from the bottom of the conversation; 0 follows new output.
    pub scroll: usize,
    pub task: Option<TaskView>,
    pub activity: VecDeque<String>,
    /// The oldest question waiting.
    pub approval: Option<ApprovalView>,
    /// A chat turn is in progress.
    pub replying: bool,
    /// What Lucia recalls for the message being typed, shown under the input.
    pub hint: Option<String>,
    pub quit: bool,
    /// Commands for the thread, sent by the loop that owns the link.
    pub out: Vec<Value>,
    /// Questions waiting, oldest first.
    asked: VecDeque<ApprovalView>,
    /// What this interface sent and has not seen come back.
    mine: VecDeque<String>,
    /// Prompts that are nobody's words and were announced by a note.
    sourced: Vec<String>,
    /// The last thing somebody said: the name of a task that starts without one.
    last_said: String,
    open_chat: Option<usize>,
    open_worker: Option<usize>,
}

impl App {
    pub fn new(agent_name: &str, chat_model: Option<String>, worker_model: &str, show_thinking: bool) -> Self {
        App {
            agent_name: agent_name.to_string(),
            chat_model,
            worker_model: worker_model.to_string(),
            focus: None,
            show_thinking,
            entries: Vec::new(),
            input: Input::default(),
            scroll: 0,
            task: None,
            activity: VecDeque::new(),
            hint: None,
            approval: None,
            replying: false,
            quit: false,
            out: Vec::new(),
            asked: VecDeque::new(),
            mine: VecDeque::new(),
            sourced: Vec::new(),
            last_said: String::new(),
            open_chat: None,
            open_worker: None,
        }
    }

    pub fn chat_mode(&self) -> bool {
        self.chat_model.is_some()
    }

    pub fn busy(&self) -> bool {
        self.replying || self.task.is_some()
    }

    pub fn user(&mut self, text: &str) {
        self.entries.push(Entry::User(text.to_string()));
        self.scroll = 0;
    }

    /// A message this interface is sending: shown at once, and not shown again when it comes back.
    pub fn sent(&mut self, text: &str) {
        self.user(text);
        self.mine.push_back(text.to_string());
        self.hint = None;
    }

    pub fn notice(&mut self, level: Level, text: impl Into<String>) {
        self.entries.push(Entry::Notice { text: text.into(), level });
    }

    fn retry(&mut self, notice: &RetryNotice, prefix: &str) {
        if let Some(note) = retry_note(notice) {
            let level = if matches!(notice, RetryNotice::AutoRetryEnd { .. }) { Level::Error } else { Level::Warn };
            self.notice(level, format!("{prefix}{note}"));
        }
    }

    pub fn alert(&mut self, alert: &Alert) {
        let level = match alert.level {
            nypi_core::inspector::AlertLevel::Notice => Level::Info,
            nypi_core::inspector::AlertLevel::Warning => Level::Warn,
            nypi_core::inspector::AlertLevel::Error => Level::Error,
        };
        self.notice(level, format!("Alert: {}", alert.message));
        self.act(format!("alert: {}", one_line(&alert.message, 60)));
    }

    /// The ids of the questions waiting, oldest first.
    pub fn waiting(&self) -> Vec<String> {
        self.asked.iter().map(|view| view.id.clone()).collect()
    }

    /// A question is answered or gone. True when it was waiting here.
    pub fn resolved(&mut self, id: &str) -> bool {
        let before = self.asked.len();
        self.asked.retain(|view| view.id != id);
        self.show_oldest();
        self.asked.len() < before
    }

    pub fn clear_approvals(&mut self) {
        self.asked.clear();
        self.show_oldest();
    }

    fn show_oldest(&mut self) {
        let waiting = self.asked.len();
        self.approval = self.asked.front().cloned().map(|view| ApprovalView { waiting, ..view });
    }

    fn act(&mut self, what: String) {
        let time = chrono::Local::now().format("%H:%M:%S");
        self.activity.push_back(format!("{time} {what}"));
        while self.activity.len() > ACTIVITY_LIMIT {
            self.activity.pop_front();
        }
    }

    /// Take one record of the thread.
    pub fn on_record(&mut self, record: &Value) {
        let text = |key: &str| record[key].as_str().unwrap_or_default();
        // A task working beside the main one: said when it starts and when it ends; its steps
        // would only interleave with the main task's in the one conversation drawn here.
        if let Some(lane) = record["lane"].as_str() {
            match text("type") {
                "task_started" => {
                    self.notice(Level::Info, format!("Task {lane} started beside: {}", one_line(text("task"), 160)));
                }
                "agent_settled" => {
                    let kind = record["status"]["kind"].as_str().unwrap_or("ended");
                    let review = if record["review"] == "pending" { ", under review" } else { "" };
                    self.notice(Level::Info, format!("Task {lane} settled: {kind}{review}"));
                }
                _ => {}
            }
            return;
        }
        match text("type") {
            "task_reviewed" => {
                if record["accepted"] != true {
                    let round = record["round"].as_u64().unwrap_or(1);
                    self.notice(Level::Info, format!("The report fell short; sent back to the worker (round {round})"));
                } else if record["said"] == true && !text("task").is_empty() {
                    self.notice(Level::Info, format!("Full report: nypi tasks show {}", text("task")));
                }
            }
            "task_report" => {
                if !text("answer").trim().is_empty() {
                    self.notice(Level::Success, text("answer").trim().to_string());
                }
            }
            "agent_start" if record["module"] != "chat" => {
                if self.task.is_none() {
                    self.task = Some(TaskView::new(&self.last_said));
                }
            }
            "task_started" => {
                self.task = Some(TaskView::new(text("task")));
                self.notice(Level::Info, format!("Task started: {}", one_line(text("task"), 200)));
                self.act(format!("→ worker: {}", one_line(text("task"), 60)));
            }
            "chat_settled" => {
                self.replying = false;
                self.open_chat = None;
                self.act(format!("replied: {}", one_line(text("reply"), 60)));
            }
            "chat_failed" => {
                self.replying = false;
                self.open_chat = None;
                self.notice(Level::Error, format!("Chat model failed: {}", text("error")));
                let next = if record["delegated"] == true {
                    "Passed your message straight to the worker."
                } else {
                    "The worker is busy; say it again shortly, or /abort the task."
                };
                self.notice(Level::Warn, next);
            }
            "approval_request" => {
                let view = ApprovalView {
                    id: text("id").to_string(),
                    tool: text("tool").to_string(),
                    summary: text("summary").to_string(),
                    reason: text("reason").to_string(),
                    waiting: 0,
                };
                self.notice(Level::Warn, format!("Approval needed: {} · {}", view.tool, one_line(&view.summary, 200)));
                self.asked.push_back(view);
                self.show_oldest();
            }
            "approval_resolved" => {
                // Still waiting here: somebody else answered it (another terminal, Telegram).
                if self.resolved(text("id")) {
                    let verdict = if record["approved"] == true { "approved" } else { "denied" };
                    self.notice(Level::Info, format!("Approval {verdict} elsewhere"));
                }
            }
            "agent_settled" => self.settled(record),
            "response" if record["success"] != true => {
                self.notice(Level::Warn, record["error"].as_str().unwrap_or("refused"));
            }
            "response" => match record["data"]["disposition"].as_str() {
                Some("handled") => {
                    self.mine.pop_front();
                    let output = record["data"]["output"].as_str().unwrap_or_default();
                    if !output.trim().is_empty() {
                        self.notice(Level::Info, output);
                    }
                }
                Some("queued") => self.notice(Level::Info, "Steering: injected after the current step"),
                _ => {}
            },
            "memory_hint" => {
                let memories: Vec<String> = record["memories"]
                    .as_array()
                    .map(|memories| {
                        memories.iter().filter_map(|m| m["text"].as_str()).map(|t| one_line(t, 70)).collect()
                    })
                    .unwrap_or_default();
                // A hint belongs under the input: it changes as the user types.
                self.hint = (!memories.is_empty() && !self.input.is_empty())
                    .then(|| format!("remembers: {}", memories.join(" · ")));
            }
            "reflection" => self.notice(Level::Info, text("summary")),
            "sourced_prompt" => {
                // Nobody's words: said by a note, and not shown again when its message comes by.
                self.sourced.push(text("text").to_string());
                if let Some(note) = crate::client::show(record, true) {
                    self.notice(Level::Info, note.trim_matches(['[', ']']));
                }
            }
            "daemon_lagged" => {
                self.notice(Level::Warn, format!("{} records went by too fast to show", record["missed"]))
            }
            _ => self.on_event(record),
        }
    }

    /// A record that is an event of a module, or somebody's words.
    fn on_event(&mut self, record: &Value) {
        if let Some(text) = said(record) {
            // One's own words are on screen already; another interface's are shown as they arrive.
            if let Some(index) = self.sourced.iter().position(|known| *known == text) {
                self.sourced.remove(index);
            } else if self.mine.front() == Some(&text) {
                self.mine.pop_front();
            } else {
                self.user(&text);
            }
            self.act(format!("said: {}", one_line(&text, 60)));
            self.last_said = text;
        }
        let from_chat = record["module"] == "chat";
        match runtime_event(record) {
            // Chat's tool calls only steer the worker; the task panel shows their effect.
            Some(RuntimeEvent::Agent {
                event:
                    AgentEvent::ToolExecutionStart { .. }
                    | AgentEvent::ToolExecutionUpdate { .. }
                    | AgentEvent::ToolExecutionEnd { .. },
            }) if from_chat => {}
            Some(RuntimeEvent::Agent { event }) => {
                let speaker = if self.chat_mode() && !from_chat { Speaker::Task } else { Speaker::Lucia };
                self.tool_activity(&event);
                self.agent_event(&event, speaker);
            }
            Some(RuntimeEvent::Alert { alert }) => self.alert(&alert),
            Some(RuntimeEvent::CompactionStart { reason }) => {
                self.notice(Level::Info, format!("Compacting context: {reason}"))
            }
            Some(RuntimeEvent::CompactionEnd { error: Some(error), .. }) => {
                self.notice(Level::Error, format!("Compaction failed: {error}"))
            }
            Some(RuntimeEvent::CompactionEnd { .. }) | None => {}
            Some(RuntimeEvent::Retry { notice }) => self.retry(&notice, if from_chat { "Chat: " } else { "" }),
        }
    }

    /// Keep the task panel's tool list and the activity panel in step with the worker.
    fn tool_activity(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::ToolExecutionStart { call_id, tool_name, arguments } => {
                self.act(format!("{tool_name} started"));
                if let Some(task) = &mut self.task {
                    task.tools.push(ToolActivity {
                        call_id: call_id.clone(),
                        tool: tool_name.clone(),
                        summary: tool_summary(tool_name, arguments),
                        state: ToolState::Running,
                    });
                    let extra = task.tools.len().saturating_sub(TOOLS_KEPT);
                    task.tools.drain(..extra);
                }
            }
            AgentEvent::ToolExecutionEnd { call_id, tool_name, output, duration_ms } => {
                let state = if output.is_error { ToolState::Failed } else { ToolState::Ok };
                let result = if output.is_error { "failed" } else { "ok" };
                let took = duration_ms.map(|ms| format!(" {ms} ms")).unwrap_or_default();
                self.act(format!("{tool_name} {result}{took}"));
                if let Some(tool) =
                    self.task.iter_mut().flat_map(|task| &mut task.tools).find(|t| &t.call_id == call_id)
                {
                    tool.state = state;
                }
            }
            _ => {}
        }
    }

    fn settled(&mut self, record: &Value) {
        self.open_worker = None;
        self.clear_approvals();
        let elapsed = self.task.take().map(|task| task.started.elapsed().as_secs());
        let took = elapsed.map(|s| format!(" · {s} s")).unwrap_or_default();
        if let Some(error) = record["error"].as_str() {
            self.act("settled: error".into());
            return self.notice(Level::Error, format!("Task error: {error}"));
        }
        let kind = record["status"]["kind"].as_str().unwrap_or("done");
        let level = match kind {
            "done" => Level::Success,
            "continuing" => Level::Info,
            _ => Level::Warn,
        };
        // Behind chat the worker's words were shown as the task's; without it they are the reply.
        let mut label = kind.to_string();
        if let Some(first) = label.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        let mut text = format!("{label}{took}");
        if record["status"]["synthesized"] == true {
            let derived = record["status"]["text"].as_str().unwrap_or_default();
            text.push_str(&format!(" (derived by the harness: {derived})"));
        }
        if let Some(sources) = record["sources"].as_str() {
            self.notice(Level::Info, sources);
        }
        self.notice(level, text);
        self.act(format!("settled: {kind}"));
    }

    fn agent_event(&mut self, event: &AgentEvent, speaker: Speaker) {
        match event {
            AgentEvent::MessageUpdate { event: StreamEvent::TextDelta { delta, .. } } => {
                let index = self.open_reply(speaker);
                if let Entry::Reply { text, .. } = &mut self.entries[index] {
                    text.push_str(delta);
                }
            }
            AgentEvent::MessageUpdate { event: StreamEvent::ThinkingDelta { delta, .. } } if self.show_thinking => {
                let index = self.open_reply(speaker);
                if let Entry::Reply { thinking, .. } = &mut self.entries[index] {
                    thinking.push_str(delta);
                }
            }
            AgentEvent::MessageEnd { message: Message::Assistant(assistant) } => {
                self.close_reply(speaker);
                if let Some(error) = &assistant.error_message {
                    let label = if assistant.stop_reason == StopReason::Aborted { "Aborted" } else { "Error" };
                    self.notice(Level::Error, format!("{label}: {error}"));
                }
            }
            AgentEvent::ToolExecutionStart { call_id, tool_name, arguments } => {
                self.close_reply(speaker);
                let line = format!("{tool_name} {}", tool_summary(tool_name, arguments));
                self.entries.push(Entry::Tool { call_id: call_id.clone(), line, state: ToolState::Running });
            }
            AgentEvent::ToolExecutionEnd { call_id, output, .. } => {
                let found = self.entries.iter_mut().rev().find_map(|e| match e {
                    Entry::Tool { call_id: id, state, .. } if id == call_id => Some(state),
                    _ => None,
                });
                if let Some(state) = found {
                    *state = if output.is_error { ToolState::Failed } else { ToolState::Ok };
                }
            }
            _ => {}
        }
    }

    fn open_reply(&mut self, speaker: Speaker) -> usize {
        let slot = if speaker == Speaker::Lucia && self.chat_mode() { self.open_chat } else { self.open_worker };
        if let Some(index) = slot {
            return index;
        }
        self.entries.push(Entry::Reply { speaker, text: String::new(), thinking: String::new() });
        let index = self.entries.len() - 1;
        if speaker == Speaker::Lucia && self.chat_mode() {
            self.open_chat = Some(index);
        } else {
            self.open_worker = Some(index);
        }
        index
    }

    fn close_reply(&mut self, speaker: Speaker) {
        if speaker == Speaker::Lucia && self.chat_mode() {
            self.open_chat = None;
        } else {
            self.open_worker = None;
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_add(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }
}

pub fn one_line(text: &str, limit: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > limit { format!("{}…", line.chars().take(limit).collect::<String>()) } else { line }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delta(text: &str, module: Option<&str>) -> Value {
        json!({ "type": "message_update", "module": module, "event": { "type": "text_delta", "index": 0, "delta": text } })
    }

    fn end(module: Option<&str>) -> Value {
        let message = Message::Assistant(nypi_ai::AssistantMessage::empty("api", "p", "m"));
        json!({ "type": "message_end", "module": module, "message": message })
    }

    #[test]
    fn chat_and_worker_stream_into_separate_entries() {
        let mut app = App::new("Lucia", Some("fast".into()), "strong", false);
        app.sent("count the files");
        app.on_record(&json!({ "type": "user_said", "text": "count the files" }));
        app.on_record(&delta("On ", Some("chat")));
        app.on_record(&json!({ "type": "task_started", "task": "count the files" }));
        app.on_record(&delta("Counting", Some("worker")));
        app.on_record(&delta("it.", Some("chat")));
        app.on_record(&end(Some("chat")));
        app.on_record(
            &json!({ "type": "tool_execution_start", "module": "worker", "call_id": "t1", "tool_name": "bash",
                               "arguments": { "command": "ls | wc -l" } }),
        );
        assert_eq!(app.task.as_ref().unwrap().tools[0].state, ToolState::Running);
        app.on_record(&json!({ "type": "tool_execution_end", "module": "worker", "call_id": "t1", "tool_name": "bash",
                               "output": { "content": [{ "type": "text", "text": "boom" }], "isError": true } }));
        assert_eq!(
            app.entries,
            [
                Entry::User("count the files".into()),
                Entry::Reply { speaker: Speaker::Lucia, text: "On it.".into(), thinking: String::new() },
                Entry::Notice { text: "Task started: count the files".into(), level: Level::Info },
                Entry::Reply { speaker: Speaker::Task, text: "Counting".into(), thinking: String::new() },
                Entry::Tool { call_id: "t1".into(), line: "bash ls | wc -l".into(), state: ToolState::Failed },
            ],
            "one's own words are shown once"
        );
        assert_eq!(app.task.as_ref().unwrap().tools[0].state, ToolState::Failed);

        app.on_record(&json!({ "type": "agent_settled", "status": { "kind": "done", "text": "counted" } }));
        assert!(app.task.is_none());
        assert!(
            matches!(app.entries.last(), Some(Entry::Notice { text, level: Level::Success }) if text.starts_with("Done"))
        );
        assert!(app.activity.iter().any(|line| line.ends_with("bash failed")), "{:?}", app.activity);
    }

    #[test]
    fn without_chat_the_worker_speaks_as_lucia_and_others_words_are_shown() {
        let mut app = App::new("Lucia", None, "strong", false);
        let from_the_phone = Message::user_text("from the phone");
        app.on_record(&json!({ "type": "message_end", "message": from_the_phone }));
        app.on_record(&json!({ "type": "agent_start" }));
        app.on_record(&delta("Hello", None));
        assert_eq!(app.entries[0], Entry::User("from the phone".into()));
        assert!(matches!(&app.entries[1], Entry::Reply { speaker: Speaker::Lucia, text, .. } if text == "Hello"));
        assert_eq!(app.task.as_ref().unwrap().task, "from the phone", "a task is named for what started it");
    }

    #[test]
    fn questions_wait_in_order_and_go_when_anybody_answers() {
        let mut app = App::new("Lucia", None, "strong", false);
        for id in ["approval-1", "approval-2"] {
            app.on_record(&json!({ "type": "approval_request", "id": id, "tool": "bash", "summary": "git push", "reason": "remote" }));
        }
        let shown = app.approval.clone().unwrap();
        assert_eq!((shown.id.as_str(), shown.waiting), ("approval-1", 2));
        app.on_record(&json!({ "type": "approval_resolved", "id": "approval-1", "approved": true }));
        assert_eq!(app.approval.as_ref().unwrap().id, "approval-2");
        assert!(
            matches!(app.entries.last(), Some(Entry::Notice { text, .. }) if text == "Approval approved elsewhere")
        );
        // One answered here is taken off before the record comes back, and is not announced.
        assert!(app.resolved("approval-2"));
        let before = app.entries.len();
        app.on_record(&json!({ "type": "approval_resolved", "id": "approval-2", "approved": false }));
        assert_eq!((app.entries.len(), app.approval.is_none()), (before, true));
    }
}
