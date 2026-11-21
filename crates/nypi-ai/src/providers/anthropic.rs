//! Anthropic Messages API adapter (streaming SSE).

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::event::{EventSender, MessageBuilder};
use crate::models::{Model, ThinkingLevel};
use crate::provider::{Chunk, RequestFailure, StreamOptions, next_chunk, post_json};
use crate::sse::{SseEvent, SseParser};
use crate::transform::prepare_for_model;
use crate::types::{ContentBlock, Context, Message, StopReason};

const API_VERSION: &str = "2023-06-01";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const ERROR_IMAGE_PLACEHOLDER: &str = "(image omitted: images are not accepted in error results)";

pub(crate) async fn run(model: Model, ctx: Context, opts: StreamOptions, tx: EventSender) {
    let mut builder = MessageBuilder::start(&model, tx);
    let Some(api_key) = opts.api_key.clone() else {
        builder.finish(StopReason::Error, Some(format!("No API key for provider \"{}\"", model.provider)));
        return;
    };
    let (body, uses_budget_thinking) = build_request(&model, &ctx, &opts);
    let mut headers =
        vec![("x-api-key".to_string(), api_key), ("anthropic-version".to_string(), API_VERSION.to_string())];
    if uses_budget_thinking {
        headers.push(("anthropic-beta".to_string(), INTERLEAVED_THINKING_BETA.to_string()));
    }
    headers.extend(model.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
    let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));

    let response = match post_json(&url, &headers, &body, &opts.cancel).await {
        Ok(response) => response,
        Err(failure) => {
            let (stop, message) = failure.into_parts();
            builder.finish(stop, Some(message));
            return;
        }
    };

    let mut body = response.bytes_stream();
    let mut parser = SseParser::default();
    let mut state = StreamState::default();
    loop {
        let (events, ended) = match next_chunk(&mut body, &opts.cancel).await {
            Chunk::Data(bytes) => (parser.push(&bytes), false),
            Chunk::End => (parser.finish().into_iter().collect(), true),
            Chunk::Aborted => return finish_failure(builder, RequestFailure::Aborted),
            Chunk::Failed(message) => return finish_failure(builder, RequestFailure::Failed(message)),
        };
        for event in events {
            if let Some((stop, error)) = state.handle(&mut builder, &event) {
                builder.finish(stop, error);
                return;
            }
        }
        if ended {
            break;
        }
    }
    builder.finish(StopReason::Error, Some("Stream ended before message_stop".into()));
}

fn finish_failure(builder: MessageBuilder, failure: RequestFailure) {
    let (stop, message) = failure.into_parts();
    builder.finish(stop, Some(message));
}

#[derive(Default)]
struct StreamState {
    /// Anthropic block index -> builder content index (tool calls only).
    tool_blocks: HashMap<u64, usize>,
    stop_reason: Option<String>,
}

impl StreamState {
    /// Apply one SSE event. Returns the final stop reason when the stream is complete.
    fn handle(&mut self, builder: &mut MessageBuilder, event: &SseEvent) -> Option<(StopReason, Option<String>)> {
        let data: Value = serde_json::from_str(&event.data).ok()?;
        let kind = data["type"].as_str().or(event.event.as_deref()).unwrap_or_default();
        match kind {
            "message_start" => apply_usage(builder, &data["message"]["usage"]),
            "content_block_start" => {
                let block = &data["content_block"];
                match block["type"].as_str().unwrap_or_default() {
                    "text" => {
                        builder.begin_text();
                        builder.text(block["text"].as_str().unwrap_or_default());
                    }
                    "thinking" => builder.begin_thinking(false, None),
                    "redacted_thinking" => {
                        builder.begin_thinking(true, block["data"].as_str().map(str::to_string));
                    }
                    "tool_use" => {
                        let index = builder.tool_call_start(
                            block["id"].as_str().unwrap_or_default(),
                            block["name"].as_str().unwrap_or_default(),
                        );
                        self.tool_blocks.insert(data["index"].as_u64().unwrap_or_default(), index);
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let delta = &data["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => builder.text(delta["text"].as_str().unwrap_or_default()),
                    "thinking_delta" => builder.thinking(delta["thinking"].as_str().unwrap_or_default()),
                    "signature_delta" => builder.thinking_signature(delta["signature"].as_str().unwrap_or_default()),
                    "input_json_delta" => {
                        if let Some(index) = self.tool_blocks.get(&data["index"].as_u64().unwrap_or_default()) {
                            builder.tool_call_delta(*index, delta["partial_json"].as_str().unwrap_or_default());
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(index) = self.tool_blocks.remove(&data["index"].as_u64().unwrap_or_default()) {
                    builder.tool_call_end(index);
                }
            }
            "message_delta" => {
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_string());
                }
                apply_usage(builder, &data["usage"]);
            }
            "message_stop" => return Some(map_stop_reason(self.stop_reason.as_deref())),
            "error" => {
                let message = data["error"]["message"].as_str().unwrap_or("unknown error");
                let kind = data["error"]["type"].as_str().unwrap_or("error");
                return Some((StopReason::Error, Some(format!("{kind}: {message}"))));
            }
            _ => {}
        }
        None
    }
}

fn apply_usage(builder: &mut MessageBuilder, usage: &Value) {
    let usage_out = builder.usage_mut();
    let read = |key: &str| usage[key].as_u64();
    if let Some(v) = read("input_tokens") {
        usage_out.input = v;
    }
    if let Some(v) = read("output_tokens") {
        usage_out.output = v;
    }
    if let Some(v) = read("cache_read_input_tokens") {
        usage_out.cache_read = v;
    }
    if let Some(v) = read("cache_creation_input_tokens") {
        usage_out.cache_write = v;
    }
    usage_out.total_tokens = usage_out.input + usage_out.output + usage_out.cache_read + usage_out.cache_write;
}

fn map_stop_reason(reason: Option<&str>) -> (StopReason, Option<String>) {
    match reason {
        Some("max_tokens") => (StopReason::Length, None),
        Some("tool_use") => (StopReason::ToolUse, None),
        Some("refusal") => (StopReason::Error, Some("The model declined to respond (refusal)".into())),
        Some("model_context_window_exceeded") => (StopReason::Error, Some("Context window exceeded".into())),
        _ => (StopReason::Stop, None),
    }
}

/// Budget for models that take `thinking.budget_tokens` instead of an effort level.
fn thinking_budget(level: ThinkingLevel) -> u64 {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 4096,
        ThinkingLevel::Medium => 10_000,
        ThinkingLevel::High => 20_000,
        ThinkingLevel::Xhigh | ThinkingLevel::Max => 32_000,
    }
}

/// Build the request body. Returns whether budget-based thinking is used (needs a beta header).
pub(crate) fn build_request(model: &Model, ctx: &Context, opts: &StreamOptions) -> (Value, bool) {
    let ctx = &crate::transcript::prepare(ctx, model.compat.supports_mid_convo_system_messages);
    let cache = json!({ "type": "ephemeral" });
    let mut body = json!({
        "model": model.id,
        "max_tokens": crate::provider::requested_max_tokens(model, opts.max_tokens).unwrap_or(model.max_tokens),
        "stream": true,
        "messages": convert_messages(&prepare_for_model(&ctx.messages, model)),
    });
    if !ctx.system_prompt.is_empty() {
        body["system"] = json!([{ "type": "text", "text": ctx.system_prompt, "cache_control": cache }]);
    }
    if !ctx.tools.is_empty() {
        let mut tools: Vec<Value> = ctx
            .tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.parameters }))
            .collect();
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = cache.clone();
        }
        body["tools"] = Value::Array(tools);
    }
    // Cache the conversation prefix up to the newest user turn.
    if let Some(Value::Array(messages)) = body.get_mut("messages")
        && let Some(last) = messages.last_mut().filter(|m| m["role"] == "user")
        && let Some(Value::Array(blocks)) = last.get_mut("content")
        && let Some(block) = blocks.last_mut()
    {
        block["cache_control"] = cache;
    }

    let level = if opts.thinking == ThinkingLevel::Off && !model.thinking_can_be_disabled() {
        ThinkingLevel::Low
    } else {
        opts.thinking
    };
    let effort = model.native_effort(level);
    let mut thinking_on = false;
    let mut uses_budget = false;
    if model.reasoning {
        match (&effort, model.compat.force_adaptive_thinking) {
            (Some(effort), true) => {
                body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
                body["output_config"] = json!({ "effort": effort });
                thinking_on = true;
            }
            (Some(_), false) => {
                let max_tokens = body["max_tokens"].as_u64().unwrap_or(model.max_tokens);
                let budget = thinking_budget(level).min(max_tokens.saturating_sub(1024).max(1024));
                body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
                thinking_on = true;
                uses_budget = true;
            }
            (None, true) => body["thinking"] = json!({ "type": "disabled" }),
            (None, false) => {}
        }
    }
    if let Some(temperature) = opts.temperature
        && !thinking_on
        && model.compat.supports_temperature != Some(false)
    {
        body["temperature"] = json!(temperature);
    }
    (body, uses_budget)
}

/// Anthropic requires tool ids matching `^[a-zA-Z0-9_-]{1,64}$`; ids from other providers may not.
fn normalize_tool_id(id: &str) -> String {
    let cleaned: String =
        id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(64).collect();
    if cleaned.is_empty() { "tool_call".to_string() } else { cleaned }
}

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();
    let flush = |out: &mut Vec<Value>, tool_results: &mut Vec<Value>| {
        if !tool_results.is_empty() {
            out.push(json!({ "role": "user", "content": std::mem::take(tool_results) }));
        }
    };
    // A system message between a tool call and its result is rejected, so updates wait until
    // after the next user message, as in Pi.
    let mut held_system: Vec<Value> = Vec::new();
    for message in messages {
        match message {
            Message::System(system) => {
                let text = crate::transcript::render_update(system);
                if !text.is_empty() {
                    held_system.push(json!({ "role": "system", "content": [{ "type": "text", "text": text }] }));
                }
            }
            Message::ToolResult(result) => {
                let mut content = convert_blocks(&result.content);
                if result.is_error {
                    for block in content.iter_mut() {
                        if block["type"] == "image" {
                            *block = json!({ "type": "text", "text": ERROR_IMAGE_PLACEHOLDER });
                        }
                    }
                }
                if content.is_empty() {
                    content.push(json!({ "type": "text", "text": "(empty result)" }));
                }
                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": normalize_tool_id(&result.tool_call_id),
                    "content": content,
                    "is_error": result.is_error,
                }));
            }
            Message::User(user) => {
                flush(&mut out, &mut tool_results);
                let content = convert_blocks(&user.content);
                if !content.is_empty() {
                    out.push(json!({ "role": "user", "content": content }));
                }
                out.append(&mut held_system);
            }
            Message::Assistant(assistant) => {
                flush(&mut out, &mut tool_results);
                let mut content = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text, .. } if !text.is_empty() => {
                            content.push(json!({ "type": "text", "text": text }));
                        }
                        ContentBlock::Thinking { redacted: true, signature: Some(data), .. } => {
                            content.push(json!({ "type": "redacted_thinking", "data": data }));
                        }
                        // Unsigned thinking cannot be verified by the API, so it is not replayed.
                        ContentBlock::Thinking { thinking, signature: Some(signature), .. } => {
                            content.push(json!({ "type": "thinking", "thinking": thinking, "signature": signature }));
                        }
                        ContentBlock::ToolCall { id, name, arguments, .. } => {
                            content.push(json!({
                                "type": "tool_use",
                                "id": normalize_tool_id(id),
                                "name": name,
                                "input": arguments,
                            }));
                        }
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    out.push(json!({ "role": "assistant", "content": content }));
                }
            }
        }
    }
    flush(&mut out, &mut tool_results);
    out.append(&mut held_system);
    out
}

fn convert_blocks(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } if !text.is_empty() => Some(json!({ "type": "text", "text": text })),
            ContentBlock::Image { data, mime_type } => Some(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": mime_type, "data": data },
            })),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ModelRegistry;
    use crate::types::{AssistantMessage, ToolDefinition, ToolResultMessage};

    fn sonnet() -> Model {
        ModelRegistry::builtin().find("anthropic/claude-sonnet-5-5").unwrap().clone()
    }

    #[test]
    fn request_groups_tool_results_and_sets_cache_breakpoints() {
        let model = sonnet();
        let mut assistant = AssistantMessage::empty("anthropic-messages", "anthropic", &model.id);
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![
            ContentBlock::ToolCall {
                id: "a".into(),
                name: "ls".into(),
                arguments: json!({}),
                arguments_error: None,
                thought_signature: None,
            },
            ContentBlock::ToolCall {
                id: "b".into(),
                name: "ls".into(),
                arguments: json!({}),
                arguments_error: None,
                thought_signature: None,
            },
        ];
        let result = |id: &str| {
            Message::ToolResult(ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: "ls".into(),
                content: vec![ContentBlock::text("ok")],
                details: None,
                is_error: false,
                timestamp: 0,
                duration_ms: None,
            })
        };
        let ctx = Context {
            system_prompt: "sys".into(),
            messages: vec![Message::user_text("hi"), Message::Assistant(assistant), result("a"), result("b")],
            tools: vec![ToolDefinition { name: "ls".into(), description: "list".into(), parameters: json!({}) }],
        };
        let opts = StreamOptions { thinking: ThinkingLevel::High, ..Default::default() };
        let (body, budget) = build_request(&model, &ctx, &opts);
        assert!(!budget);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(messages[2]["content"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn normalizes_foreign_tool_ids() {
        assert_eq!(normalize_tool_id("call:abc|1"), "call_abc_1");
    }
}
