//! Streaming protocol between providers and consumers, and the builder providers use to
//! emit it while assembling the final assistant message.

use std::collections::HashMap;
use std::time::Instant;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::models::Model;
use crate::types::{AssistantMessage, ContentBlock, StopReason, ToolCall, Usage};

/// One streamed provider event. Every stream ends with exactly one `Done`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Start,
    TextDelta {
        index: usize,
        delta: String,
    },
    ThinkingDelta {
        index: usize,
        delta: String,
    },
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolCallDelta {
        index: usize,
        delta: String,
    },
    ToolCallEnd {
        index: usize,
        call: ToolCall,
    },
    /// Final message. Failures arrive here with `stop_reason` Error or Aborted.
    Done {
        message: AssistantMessage,
    },
}

pub type EventSender = mpsc::UnboundedSender<StreamEvent>;
pub type EventReceiver = mpsc::UnboundedReceiver<StreamEvent>;

/// Assembles an assistant message from provider deltas and forwards each step as a `StreamEvent`.
pub struct MessageBuilder {
    message: AssistantMessage,
    tx: EventSender,
    started: Instant,
    tool_args: HashMap<usize, String>,
    open_tools: Vec<usize>,
    model: Model,
}

impl MessageBuilder {
    /// Start a response for `model` and emit `Start`.
    pub fn start(model: &Model, tx: EventSender) -> Self {
        let _ = tx.send(StreamEvent::Start);
        MessageBuilder {
            message: AssistantMessage::empty(model.api.as_str(), &model.provider, &model.id),
            tx,
            started: Instant::now(),
            tool_args: HashMap::new(),
            open_tools: Vec::new(),
            model: model.clone(),
        }
    }

    /// Open a new text block even if the previous block is text.
    pub fn begin_text(&mut self) {
        self.message.content.push(ContentBlock::Text { text: String::new(), text_signature: None });
    }

    /// Append text, continuing the last block when it is a text block.
    pub fn text(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if !matches!(self.message.content.last(), Some(ContentBlock::Text { .. })) {
            self.begin_text();
        }
        let index = self.message.content.len() - 1;
        if let Some(ContentBlock::Text { text, .. }) = self.message.content.last_mut() {
            text.push_str(delta);
        }
        let _ = self.tx.send(StreamEvent::TextDelta { index, delta: delta.to_string() });
    }

    /// Open a new thinking block (Anthropic signs each block, so blocks must not merge).
    pub fn begin_thinking(&mut self, redacted: bool, signature: Option<String>) {
        self.message.content.push(ContentBlock::Thinking { thinking: String::new(), signature, redacted });
    }

    /// Append reasoning text, continuing the last block when it is a thinking block.
    pub fn thinking(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if !matches!(self.message.content.last(), Some(ContentBlock::Thinking { .. })) {
            self.begin_thinking(false, None);
        }
        let index = self.message.content.len() - 1;
        if let Some(ContentBlock::Thinking { thinking, .. }) = self.message.content.last_mut() {
            thinking.push_str(delta);
        }
        let _ = self.tx.send(StreamEvent::ThinkingDelta { index, delta: delta.to_string() });
    }

    /// Set or extend the replay signature of the last thinking block.
    pub fn thinking_signature(&mut self, part: &str) {
        if !matches!(self.message.content.last(), Some(ContentBlock::Thinking { .. })) {
            self.begin_thinking(false, None);
        }
        if let Some(ContentBlock::Thinking { signature, .. }) = self.message.content.last_mut() {
            signature.get_or_insert_with(String::new).push_str(part);
        }
    }

    /// Open a tool-call block; returns its content index for later deltas.
    pub fn tool_call_start(&mut self, id: &str, name: &str) -> usize {
        self.message.content.push(ContentBlock::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: Value::Object(Default::default()),
            arguments_error: None,
            thought_signature: None,
        });
        let index = self.message.content.len() - 1;
        self.tool_args.insert(index, String::new());
        self.open_tools.push(index);
        let _ = self.tx.send(StreamEvent::ToolCallStart { index, id: id.to_string(), name: name.to_string() });
        index
    }

    /// Append raw JSON argument text to the tool call at `index`.
    pub fn tool_call_delta(&mut self, index: usize, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(args) = self.tool_args.get_mut(&index) {
            args.push_str(delta);
        }
        let _ = self.tx.send(StreamEvent::ToolCallDelta { index, delta: delta.to_string() });
    }

    /// Patch the id or name of an open tool call (OpenAI may send them after the first delta).
    pub fn tool_call_identity(&mut self, index: usize, new_id: Option<&str>, new_name: Option<&str>) {
        if let Some(ContentBlock::ToolCall { id, name, .. }) = self.message.content.get_mut(index) {
            if let Some(new_id) = new_id.filter(|s| !s.is_empty()) {
                *id = new_id.to_string();
            }
            // Most servers send the name once; some fragment it; a few repeat it in every chunk.
            if let Some(new_name) = new_name.filter(|s| !s.is_empty() && *s != name.as_str()) {
                name.push_str(new_name);
            }
        }
    }

    /// Parse the accumulated arguments of the tool call at `index` and emit `ToolCallEnd`.
    pub fn tool_call_end(&mut self, index: usize) {
        let Some(raw) = self.tool_args.remove(&index) else { return };
        self.open_tools.retain(|i| *i != index);
        let Some(ContentBlock::ToolCall { id, name, arguments, arguments_error, .. }) =
            self.message.content.get_mut(index)
        else {
            return;
        };
        match parse_tool_arguments(&raw) {
            Ok(value) => *arguments = value,
            Err(error) => *arguments_error = Some(error),
        }
        let call = ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
            arguments_error: arguments_error.clone(),
        };
        let _ = self.tx.send(StreamEvent::ToolCallEnd { index, call });
    }

    /// Append text to the text block at `index` (providers that address blocks by position).
    pub fn text_at(&mut self, index: usize, delta: &str) {
        if let Some(ContentBlock::Text { text, .. }) = self.message.content.get_mut(index)
            && !delta.is_empty()
        {
            text.push_str(delta);
            let _ = self.tx.send(StreamEvent::TextDelta { index, delta: delta.to_string() });
        }
    }

    /// Replace the text block at `index` with the provider's final text.
    pub fn set_text(&mut self, index: usize, final_text: String) {
        if let Some(ContentBlock::Text { text, .. }) = self.message.content.get_mut(index) {
            *text = final_text;
        }
    }

    /// Append reasoning text to the thinking block at `index`.
    pub fn thinking_at(&mut self, index: usize, delta: &str) {
        if let Some(ContentBlock::Thinking { thinking, .. }) = self.message.content.get_mut(index)
            && !delta.is_empty()
        {
            thinking.push_str(delta);
            let _ = self.tx.send(StreamEvent::ThinkingDelta { index, delta: delta.to_string() });
        }
    }

    /// Finalize the thinking block at `index`: its text, and the opaque item replayed later.
    pub fn set_thinking(&mut self, index: usize, final_text: Option<String>, final_signature: Option<String>) {
        if let Some(ContentBlock::Thinking { thinking, signature, .. }) = self.message.content.get_mut(index) {
            if let Some(text) = final_text {
                *thinking = text;
            }
            if final_signature.is_some() {
                *signature = final_signature;
            }
        }
    }

    /// Replace the accumulated arguments of the tool call at `index` with the provider's complete text.
    pub fn tool_call_set_arguments(&mut self, index: usize, raw: &str) {
        if let Some(args) = self.tool_args.get_mut(&index) {
            *args = raw.to_string();
        }
    }

    /// Mark the thinking block at `index` as redacted (encrypted reasoning) and show a placeholder.
    pub fn set_thinking_redacted(&mut self, index: usize, placeholder: &str) {
        if let Some(ContentBlock::Thinking { redacted, thinking, .. }) = self.message.content.get_mut(index)
            && !*redacted
        {
            *redacted = true;
            thinking.push_str(placeholder);
            let _ = self.tx.send(StreamEvent::ThinkingDelta { index, delta: placeholder.to_string() });
        }
    }

    /// Set the replay signature of the text block at `index`.
    pub fn set_text_signature(&mut self, index: usize, signature: String) {
        if let Some(ContentBlock::Text { text_signature, .. }) = self.message.content.get_mut(index) {
            *text_signature = Some(signature);
        }
    }

    /// Set the thought signature of the tool call at `index`.
    pub fn set_tool_call_signature(&mut self, index: usize, signature: String) {
        if let Some(ContentBlock::ToolCall { thought_signature, .. }) = self.message.content.get_mut(index) {
            *thought_signature = Some(signature);
        }
    }

    /// Content index the next block will get.
    pub fn next_index(&self) -> usize {
        self.message.content.len()
    }

    pub fn last_is_text(&self) -> bool {
        matches!(self.message.content.last(), Some(ContentBlock::Text { .. }))
    }

    pub fn last_is_thinking(&self) -> bool {
        matches!(self.message.content.last(), Some(ContentBlock::Thinking { .. }))
    }

    pub fn usage_mut(&mut self) -> &mut Usage {
        &mut self.message.usage
    }

    pub fn has_tool_calls(&self) -> bool {
        self.message.content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }))
    }

    /// Close open tool calls, finalize usage/cost/duration, and emit `Done`.
    pub fn finish(mut self, stop_reason: StopReason, error: Option<String>) {
        for index in self.open_tools.clone() {
            self.tool_call_end(index);
        }
        let usage = &mut self.message.usage;
        if usage.total_tokens == 0 {
            usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
        }
        self.model.apply_cost(&mut self.message.usage);
        self.message.stop_reason = stop_reason;
        self.message.error_message = error;
        self.message.duration_ms = Some(self.started.elapsed().as_millis() as u64);
        // Empty text blocks carry nothing and some providers reject them on replay, unless they
        // hold a signature the provider needs back (Gemini signs empty parts).
        self.message
            .content
            .retain(|b| !matches!(b, ContentBlock::Text { text, text_signature: None } if text.is_empty()));
        let _ = self.tx.send(StreamEvent::Done { message: self.message });
    }
}

/// Parse streamed tool arguments. Empty text means no arguments.
fn parse_tool_arguments(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(_) => Err(format!("Tool arguments must be a JSON object, got: {raw}")),
        Err(e) => Err(format!("Tool arguments are not valid JSON ({e}): {raw}")),
    }
}

/// Emit a terminal error response without any streamed content.
pub fn send_error(model: &Model, tx: EventSender, stop_reason: StopReason, error: String) {
    MessageBuilder::start(model, tx).finish(stop_reason, Some(error));
}
