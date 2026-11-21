//! OpenAI Responses adapter (`openai-responses`), ported from Pi's openai-responses.ts and
//! openai-responses-shared.ts. Requests are stateless (`store: false`): reasoning items come
//! back with their encrypted content, are kept as the thinking block's signature, and are sent
//! again verbatim on the next turn. Not ported: grammar (custom) tools, `additional_tools` and
//! tool search, configuration_update items, service tiers, GitHub Copilot and ChatGPT sign-in.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::event::{EventSender, MessageBuilder};
use crate::models::Model;
use crate::provider::{Chunk, RequestFailure, StreamOptions, next_chunk, post_json};
use crate::sse::SseParser;
use crate::transform::prepare_for_model;
use crate::types::{AssistantMessage, ContentBlock, Context, Message, StopReason, text_of};

/// Providers whose tool call ids carry a Responses item id (`call_id|item_id`) worth replaying.
const ITEM_ID_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];
/// OpenAI rejects `max_output_tokens` below 16 (Pi issue 6265).
const MIN_OUTPUT_TOKENS: u64 = 16;
const PROMPT_CACHE_KEY_MAX_CHARS: usize = 64;

pub(crate) async fn run(model: Model, ctx: Context, opts: StreamOptions, tx: EventSender) {
    let mut builder = MessageBuilder::start(&model, tx);
    let mut headers: Vec<(String, String)> = model.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let has_auth_header = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
    match &opts.api_key {
        Some(key) => headers.push(("authorization".into(), format!("Bearer {key}"))),
        None if !has_auth_header => {
            builder.finish(StopReason::Error, Some(format!("No API key for provider \"{}\"", model.provider)));
            return;
        }
        None => {}
    }
    if let Some(session) = &opts.session_id {
        if model.provider == "openrouter" || model.base_url.contains("openrouter.ai") {
            headers.push(("x-session-id".into(), session.clone()));
        } else {
            headers.push(("session_id".into(), session.clone()));
            headers.push(("x-client-request-id".into(), session.clone()));
        }
    }
    let body = build_request(&model, &ctx, &opts);
    let url = format!("{}/responses", model.base_url.trim_end_matches('/'));
    let response = match post_json(&url, &headers, &body, &opts.cancel).await {
        Ok(response) => response,
        Err(failure) => return finish_failure(builder, failure),
    };

    let mut stream = response.bytes_stream();
    let mut parser = SseParser::default();
    let mut state = StreamState::default();
    loop {
        let (events, ended) = match next_chunk(&mut stream, &opts.cancel).await {
            Chunk::Data(bytes) => (parser.push(&bytes), false),
            Chunk::End => (parser.finish().into_iter().collect(), true),
            Chunk::Aborted => return finish_failure(builder, RequestFailure::Aborted),
            Chunk::Failed(message) => return finish_failure(builder, RequestFailure::Failed(message)),
        };
        for event in events {
            let Ok(event) = serde_json::from_str::<Value>(&event.data) else { continue };
            if let Err(message) = state.apply(&mut builder, &event) {
                builder.finish(StopReason::Error, Some(message));
                return;
            }
        }
        if ended {
            break;
        }
    }
    state.finish(builder);
}

fn finish_failure(builder: MessageBuilder, failure: RequestFailure) {
    let (stop, message) = failure.into_parts();
    builder.finish(stop, Some(message));
}

#[derive(Clone, Copy, PartialEq)]
enum Slot {
    Thinking(usize),
    Text(usize),
    ToolCall(usize),
}

#[derive(Default)]
struct StreamState {
    /// Output index -> content block, for items still streaming.
    slots: HashMap<u64, Slot>,
    /// Tool calls whose `output_item.done` has not arrived.
    unfinished_tools: Vec<(usize, String)>,
    terminal: Option<(StopReason, Option<String>)>,
}

impl StreamState {
    fn apply(&mut self, builder: &mut MessageBuilder, event: &Value) -> Result<(), String> {
        let output_index = event["output_index"].as_u64().unwrap_or(0);
        let delta = event["delta"].as_str().unwrap_or_default();
        match event["type"].as_str().unwrap_or_default() {
            "response.output_item.added" => self.open(builder, output_index, &event["item"]),
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(Slot::Thinking(index)) = self.slots.get(&output_index) {
                    builder.thinking_at(*index, delta);
                }
            }
            "response.reasoning_summary_part.done" => {
                if let Some(Slot::Thinking(index)) = self.slots.get(&output_index) {
                    builder.thinking_at(*index, "\n\n");
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(Slot::Text(index)) = self.slots.get(&output_index) {
                    builder.text_at(*index, delta);
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(Slot::ToolCall(index)) = self.slots.get(&output_index) {
                    builder.tool_call_delta(*index, delta);
                }
            }
            "response.function_call_arguments.done" => {
                if let (Some(Slot::ToolCall(index)), Some(arguments)) =
                    (self.slots.get(&output_index), event["arguments"].as_str())
                {
                    builder.tool_call_set_arguments(*index, arguments);
                }
            }
            "response.output_item.done" => self.close(builder, output_index, &event["item"]),
            "response.completed" | "response.incomplete" => self.complete(builder, &event["response"]),
            "error" => {
                let code = event["code"].as_str().unwrap_or("unknown");
                let message = event["message"].as_str().unwrap_or("Unknown error");
                return Err(format!("Error Code {code}: {message}"));
            }
            "response.failed" => {
                let response = &event["response"];
                let error = &response["error"];
                let message = if error.is_object() {
                    format!(
                        "{}: {}",
                        error["code"].as_str().unwrap_or("unknown"),
                        error["message"].as_str().unwrap_or("no message")
                    )
                } else if let Some(reason) = response["incomplete_details"]["reason"].as_str() {
                    format!("incomplete: {reason}")
                } else {
                    "Unknown error (no error details in response)".into()
                };
                return Err(message);
            }
            _ => {}
        }
        Ok(())
    }

    fn open(&mut self, builder: &mut MessageBuilder, output_index: u64, item: &Value) {
        let slot = match item["type"].as_str().unwrap_or_default() {
            "reasoning" => {
                let index = builder.next_index();
                builder.begin_thinking(false, None);
                Slot::Thinking(index)
            }
            "message" => {
                let index = builder.next_index();
                builder.begin_text();
                Slot::Text(index)
            }
            "function_call" => {
                let id = format!("{}|{}", str_of(&item["call_id"]), str_of(&item["id"]));
                let name = str_of(&item["name"]);
                let index = builder.tool_call_start(&id, name);
                if let Some(arguments) = item["arguments"].as_str() {
                    builder.tool_call_delta(index, arguments);
                }
                self.unfinished_tools.push((index, name.to_string()));
                Slot::ToolCall(index)
            }
            _ => return,
        };
        self.slots.insert(output_index, slot);
    }

    fn close(&mut self, builder: &mut MessageBuilder, output_index: u64, item: &Value) {
        let slot = match self.slots.remove(&output_index) {
            Some(slot) => slot,
            // A done item that was never announced: open it now so its content is kept.
            None => {
                self.open(builder, output_index, item);
                match self.slots.remove(&output_index) {
                    Some(slot) => slot,
                    None => return,
                }
            }
        };
        match (slot, item["type"].as_str().unwrap_or_default()) {
            (Slot::Thinking(index), "reasoning") => {
                let join = |key: &str| {
                    item[key]
                        .as_array()
                        .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n\n"))
                        .filter(|text| !text.is_empty())
                };
                builder.set_thinking(index, join("summary").or_else(|| join("content")), Some(item.to_string()));
            }
            (Slot::Text(index), "message") => {
                let text: String = item["content"]
                    .as_array()
                    .map(|parts| {
                        parts.iter().filter_map(|p| p["text"].as_str().or_else(|| p["refusal"].as_str())).collect()
                    })
                    .unwrap_or_default();
                builder.set_text(index, text);
                // Pi's TextSignatureV1: the item id and phase, replayed with the text.
                let mut signature = json!({ "v": 1, "id": str_of(&item["id"]) });
                if let Some(phase) = item["phase"].as_str() {
                    signature["phase"] = json!(phase);
                }
                if !str_of(&item["id"]).is_empty() {
                    builder.set_text_signature(index, signature.to_string());
                }
            }
            (Slot::ToolCall(index), "function_call") => {
                if let Some(arguments) = item["arguments"].as_str().filter(|a| !a.is_empty()) {
                    builder.tool_call_set_arguments(index, arguments);
                }
                builder.tool_call_end(index);
                self.unfinished_tools.retain(|(i, _)| *i != index);
            }
            _ => {}
        }
    }

    fn complete(&mut self, builder: &mut MessageBuilder, response: &Value) {
        if let Some(usage) = response.get("usage").filter(|u| u.is_object()) {
            let cached = usage["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
            let cache_write = usage["input_tokens_details"]["cache_write_tokens"].as_u64().unwrap_or(0);
            let out = builder.usage_mut();
            // OpenAI counts cached and cache-write tokens inside input_tokens.
            out.input = usage["input_tokens"].as_u64().unwrap_or(0).saturating_sub(cached + cache_write);
            out.output = usage["output_tokens"].as_u64().unwrap_or(0);
            out.cache_read = cached;
            out.cache_write = cache_write;
            out.total_tokens = usage["total_tokens"].as_u64().unwrap_or(0);
        }
        let reason = response["incomplete_details"]["reason"].as_str();
        let (mut stop, error) = match response["status"].as_str() {
            Some("incomplete") if reason == Some("max_output_tokens") => (StopReason::Length, None),
            Some("incomplete") => (
                StopReason::Error,
                Some(match reason {
                    Some(reason) => format!("Response incomplete: {reason}"),
                    None => "Response incomplete without a provider reason".into(),
                }),
            ),
            Some("failed") | Some("cancelled") => (StopReason::Error, None),
            _ => (StopReason::Stop, None),
        };
        if stop == StopReason::Stop && builder.has_tool_calls() {
            stop = StopReason::ToolUse;
        }
        self.terminal = Some((stop, error));
    }

    fn finish(self, builder: MessageBuilder) {
        let Some((stop, error)) = self.terminal else {
            builder.finish(
                StopReason::Error,
                Some("OpenAI Responses stream ended before a terminal response event".into()),
            );
            return;
        };
        // The agent runs every tool call it is handed; one whose item never finished may carry
        // cut-off arguments.
        if stop == StopReason::ToolUse
            && let Some((_, name)) = self.unfinished_tools.first()
        {
            let message = format!("OpenAI Responses stream completed with an unfinished tool call: {name}");
            builder.finish(StopReason::Error, Some(message));
            return;
        }
        builder.finish(stop, error);
    }
}

fn str_of(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

pub(crate) fn build_request(model: &Model, ctx: &Context, opts: &StreamOptions) -> Value {
    let ctx = &crate::transcript::prepare(ctx, model.compat.supports_mid_convo_system_messages);
    let mut body = json!({
        "model": model.id,
        "input": convert_messages(model, ctx),
        "stream": true,
        "store": false,
    });
    if let Some(session) = &opts.session_id {
        body["prompt_cache_key"] = json!(session.chars().take(PROMPT_CACHE_KEY_MAX_CHARS).collect::<String>());
    }
    if let Some(max_tokens) = opts.max_tokens {
        body["max_output_tokens"] = json!(max_tokens.max(MIN_OUTPUT_TOKENS));
    }
    if let Some(temperature) = opts.temperature {
        body["temperature"] = json!(temperature);
    }
    if !ctx.tools.is_empty() {
        body["tools"] = ctx
            .tools
            .iter()
            .map(|t| json!({ "type": "function", "name": t.name, "description": t.description, "parameters": t.parameters }))
            .collect();
    }
    if model.reasoning {
        match model.native_effort(opts.thinking) {
            Some(effort) => {
                body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
                body["include"] = json!(["reasoning.encrypted_content"]);
            }
            None => {
                if let Some(off) = model.thinking_level_map.get("off").cloned().unwrap_or(Some("none".into())) {
                    body["reasoning"] = json!({ "effort": off });
                }
            }
        }
        if model.provider == "xai" {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
    }
    body
}

fn convert_messages(model: &Model, ctx: &Context) -> Vec<Value> {
    let mut input = Vec::new();
    if !ctx.system_prompt.is_empty() {
        let role = if model.reasoning { "developer" } else { "system" };
        input.push(json!({ "role": role, "content": ctx.system_prompt }));
    }
    // Tool call ids as sent, so a result names its call the same way.
    let mut call_ids: HashMap<String, String> = HashMap::new();
    let messages = prepare_for_model(&ctx.messages, model);
    for (position, message) in messages.iter().enumerate() {
        match message {
            Message::System(system) => {
                let text = crate::transcript::render_update(system);
                if !text.is_empty() {
                    let role = if model.reasoning { "developer" } else { "system" };
                    input.push(json!({ "role": role, "content": text }));
                }
            }
            Message::User(user) => {
                let content: Vec<Value> = user
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(json!({ "type": "input_text", "text": text })),
                        ContentBlock::Image { data, mime_type } => Some(input_image(data, mime_type)),
                        _ => None,
                    })
                    .collect();
                if !content.is_empty() {
                    input.push(json!({ "role": "user", "content": content }));
                }
            }
            Message::Assistant(assistant) => {
                input.extend(convert_assistant(model, assistant, position, &mut call_ids));
            }
            Message::ToolResult(result) => {
                let id =
                    call_ids.get(&result.tool_call_id).cloned().unwrap_or_else(|| normalize_part(&result.tool_call_id));
                let call_id = id.split('|').next().unwrap_or_default().to_string();
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": tool_output(model, &result.content),
                }));
            }
        }
    }
    input
}

fn convert_assistant(
    model: &Model,
    assistant: &AssistantMessage,
    position: usize,
    call_ids: &mut HashMap<String, String>,
) -> Vec<Value> {
    let same_api = assistant.provider == model.provider && assistant.api == model.api.as_str();
    let different_model = same_api && assistant.model != model.id;
    let mut output = Vec::new();
    let mut text_blocks = 0;
    for block in &assistant.content {
        match block {
            ContentBlock::Thinking { signature: Some(signature), .. } => {
                if let Ok(item) = serde_json::from_str::<Value>(signature)
                    && item["type"] == "reasoning"
                {
                    output.push(item);
                }
            }
            ContentBlock::Text { text, text_signature } => {
                let (signed_id, phase) = parse_text_signature(text_signature.as_deref());
                let id = match signed_id {
                    Some(id) if id.chars().count() > 64 => format!("msg_{}", short_hash(&id)),
                    Some(id) => id,
                    None if text_blocks == 0 => format!("msg_pi_{position}"),
                    None => format!("msg_pi_{position}_{text_blocks}"),
                };
                text_blocks += 1;
                let mut message = json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                    "status": "completed",
                    "id": id,
                });
                if let Some(phase) = phase {
                    message["phase"] = json!(phase);
                }
                output.push(message);
            }
            ContentBlock::ToolCall { id, name, arguments, .. } => {
                let normalized = normalize_call_id(model, assistant, id);
                call_ids.insert(id.clone(), normalized.clone());
                let mut parts = normalized.splitn(2, '|');
                let call_id = parts.next().unwrap_or_default();
                // OpenAI pairs fc_ item ids with the reasoning items of the model that made them; an
                // id from another model, or one that is not an fc_ id, is omitted.
                let item_id = parts.next().filter(|item| !different_model && item.starts_with("fc_"));
                let mut call = json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments.to_string(),
                });
                if let Some(item_id) = item_id {
                    call["id"] = json!(item_id);
                }
                output.push(call);
            }
            _ => {}
        }
    }
    output
}

/// Read Pi's text signature: `{"v":1,"id":..,"phase":..}`, or a legacy plain id.
fn parse_text_signature(signature: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(signature) = signature.filter(|s| !s.is_empty()) else { return (None, None) };
    if let Ok(parsed) = serde_json::from_str::<Value>(signature)
        && parsed["v"] == 1
        && let Some(id) = parsed["id"].as_str()
    {
        let phase = parsed["phase"].as_str().filter(|p| matches!(*p, "commentary" | "final_answer"));
        return (Some(id.to_string()), phase.map(str::to_string));
    }
    if signature.starts_with('{') {
        return (None, None);
    }
    (Some(signature.to_string()), None)
}

/// Pi's id rules: keep `call|item` pairs only for providers that accept item ids, and give
/// calls from another provider a stable `fc_` item id.
fn normalize_call_id(model: &Model, source: &AssistantMessage, id: &str) -> String {
    if !ITEM_ID_PROVIDERS.contains(&model.provider.as_str()) || !id.contains('|') {
        return normalize_part(id);
    }
    let (call_id, item_id) = id.split_once('|').unwrap_or((id, ""));
    let foreign = source.provider != model.provider || source.api != model.api.as_str();
    let mut item = if foreign {
        format!("fc_{}", short_hash(item_id)).chars().take(64).collect()
    } else {
        normalize_part(item_id)
    };
    if !item.starts_with("fc_") {
        item = normalize_part(&format!("fc_{item}"));
    }
    format!("{}|{item}", normalize_part(call_id))
}

fn normalize_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .take(64)
        .collect();
    sanitized.trim_end_matches('_').to_string()
}

fn tool_output(model: &Model, content: &[ContentBlock]) -> Value {
    let text = text_of(content);
    let images: Vec<Value> = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image { data, mime_type } => Some(input_image(data, mime_type)),
            _ => None,
        })
        .collect();
    if images.is_empty() || !model.supports_images() {
        let text = if !text.trim().is_empty() {
            text
        } else if images.is_empty() {
            "(no tool output)".into()
        } else {
            "(see attached image)".into()
        };
        return json!(text);
    }
    let mut parts = Vec::new();
    if !text.trim().is_empty() {
        parts.push(json!({ "type": "input_text", "text": text }));
    }
    parts.extend(images);
    Value::Array(parts)
}

fn input_image(data: &str, mime_type: &str) -> Value {
    json!({ "type": "input_image", "detail": "auto", "image_url": format!("data:{mime_type};base64,{data}") })
}

/// Pi's `shortHash` (utils/hash.ts), kept identical so ids match what Pi would send.
pub(crate) fn short_hash(text: &str) -> String {
    let (mut h1, mut h2): (u32, u32) = (0xdeadbeef, 0x41c6ce57);
    for unit in text.encode_utf16() {
        let ch = u32::from(unit);
        h1 = (h1 ^ ch).wrapping_mul(2654435761);
        h2 = (h2 ^ ch).wrapping_mul(1597334677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2246822507) ^ (h2 ^ (h2 >> 13)).wrapping_mul(3266489909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2246822507) ^ (h1 ^ (h1 >> 13)).wrapping_mul(3266489909);
    format!("{}{}", radix36(h2), radix36(h1))
}

fn radix36(mut n: u32) -> String {
    if n == 0 {
        return "0".into();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("ascii digits")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::StreamEvent;
    use crate::models::{Api, ThinkingLevel};

    fn model() -> Model {
        let mut model: Model = serde_json::from_value(json!({
            "id": "gpt-x", "name": "GPT X", "api": "openai-responses", "provider": "openai",
            "baseUrl": "http://localhost", "reasoning": true, "input": ["text", "image"],
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap();
        model.api = Api::OpenAiResponses;
        model
    }

    #[test]
    fn short_hash_matches_pi() {
        // Values computed with Pi's shortHash in Node.
        assert_eq!(short_hash(""), "k4n83c7h0j2b");
        assert_eq!(short_hash("fc_abc"), "1qaoofs1vxahbg");
        assert_eq!(short_hash("你好|x"), "1paesfc15l5l43");
    }

    #[test]
    fn request_replays_reasoning_and_pairs_tool_results() {
        let model = model();
        let mut assistant = AssistantMessage::empty("openai-responses", "openai", "gpt-x");
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![
            ContentBlock::Thinking {
                thinking: "plan".into(),
                signature: Some(json!({ "type": "reasoning", "id": "rs_1", "encrypted_content": "E" }).to_string()),
                redacted: false,
            },
            ContentBlock::text("Looking."),
            ContentBlock::ToolCall {
                id: "call_1|fc_1".into(),
                name: "ls".into(),
                arguments: json!({ "path": "." }),
                arguments_error: None,
                thought_signature: None,
            },
        ];
        let result = crate::types::ToolResultMessage {
            tool_call_id: "call_1|fc_1".into(),
            tool_name: "ls".into(),
            content: vec![ContentBlock::text("a.rs")],
            details: None,
            is_error: false,
            timestamp: 0,
            duration_ms: None,
        };
        let ctx = Context {
            system_prompt: "sys".into(),
            messages: vec![Message::user_text("list"), Message::Assistant(assistant), Message::ToolResult(result)],
            tools: vec![],
        };
        let opts =
            StreamOptions { thinking: ThinkingLevel::High, session_id: Some("s".repeat(80)), ..Default::default() };
        let body = build_request(&model, &ctx, &opts);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[2]["encrypted_content"], "E");
        assert_eq!(input[3]["id"], "msg_pi_1");
        assert_eq!(input[4]["id"], "fc_1");
        assert_eq!(input[4]["call_id"], "call_1");
        assert_eq!(input[5], json!({ "type": "function_call_output", "call_id": "call_1", "output": "a.rs" }));
        assert_eq!(body["reasoning"], json!({ "effort": "high", "summary": "auto" }));
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["prompt_cache_key"].as_str().unwrap().len(), 64);
        assert_eq!(body["store"], false);
    }

    fn run_events(events: &[Value]) -> AssistantMessage {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut builder = MessageBuilder::start(&model(), tx);
        let mut state = StreamState::default();
        for event in events {
            if let Err(message) = state.apply(&mut builder, event) {
                builder.finish(StopReason::Error, Some(message));
                return last_message(&mut rx);
            }
        }
        state.finish(builder);
        last_message(&mut rx)
    }

    fn last_message(rx: &mut tokio::sync::mpsc::UnboundedReceiver<StreamEvent>) -> AssistantMessage {
        let mut last = None;
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::Done { message } = event {
                last = Some(message);
            }
        }
        last.expect("done event")
    }

    #[test]
    fn stream_builds_reasoning_text_and_tool_calls() {
        let reasoning =
            json!({ "type": "reasoning", "id": "rs_1", "summary": [{ "text": "Think." }], "encrypted_content": "E" });
        let message = run_events(&[
            json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "reasoning", "id": "rs_1" } }),
            json!({ "type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "Thi" }),
            json!({ "type": "response.output_item.done", "output_index": 0, "item": reasoning }),
            json!({ "type": "response.output_item.added", "output_index": 1, "item": { "type": "message", "id": "msg_1" } }),
            json!({ "type": "response.output_text.delta", "output_index": 1, "delta": "Hi" }),
            json!({ "type": "response.output_item.done", "output_index": 1, "item": { "type": "message", "id": "msg_1", "phase": "final_answer", "content": [{ "type": "output_text", "text": "Hi there" }] } }),
            json!({ "type": "response.output_item.added", "output_index": 2, "item": { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "ls" } }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 2, "delta": "{\"pa" }),
            json!({ "type": "response.function_call_arguments.done", "output_index": 2, "arguments": "{\"path\":\".\"}" }),
            json!({ "type": "response.output_item.done", "output_index": 2, "item": { "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "ls", "arguments": "{\"path\":\".\"}" } }),
            json!({ "type": "response.completed", "response": { "status": "completed", "usage": { "input_tokens": 100, "output_tokens": 20, "total_tokens": 120, "input_tokens_details": { "cached_tokens": 40 } } } }),
        ]);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert!(
            matches!(&message.content[0], ContentBlock::Thinking { thinking, signature: Some(s), .. } if thinking == "Think." && s.contains("\"E\""))
        );
        assert!(
            matches!(&message.content[1], ContentBlock::Text { text, text_signature: Some(s) } if text == "Hi there" && s.contains("final_answer"))
        );
        assert_eq!(
            parse_text_signature(Some(r#"{"v":1,"id":"msg_1","phase":"final_answer"}"#)),
            (Some("msg_1".into()), Some("final_answer".into()))
        );
        assert!(
            matches!(&message.content[2], ContentBlock::ToolCall { id, arguments, .. } if id == "call_1|fc_1" && arguments["path"] == ".")
        );
        assert_eq!((message.usage.input, message.usage.cache_read, message.usage.output), (60, 40, 20));
    }

    #[test]
    fn failures_and_truncation_map_to_stop_reasons() {
        let incomplete = run_events(&[
            json!({ "type": "response.incomplete", "response": { "status": "incomplete", "incomplete_details": { "reason": "max_output_tokens" } } }),
        ]);
        assert_eq!(incomplete.stop_reason, StopReason::Length);
        let failed = run_events(&[
            json!({ "type": "response.failed", "response": { "error": { "code": "server_error", "message": "boom" } } }),
        ]);
        assert_eq!(
            (failed.stop_reason, failed.error_message.as_deref()),
            (StopReason::Error, Some("server_error: boom"))
        );
        let cut = run_events(&[
            json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "message" } }),
        ]);
        assert!(cut.error_message.unwrap().contains("before a terminal response event"));
        let unfinished = run_events(&[
            json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "function_call", "id": "fc_1", "call_id": "c", "name": "bash" } }),
            json!({ "type": "response.completed", "response": { "status": "completed" } }),
        ]);
        assert!(unfinished.error_message.unwrap().contains("unfinished tool call: bash"));
    }
}
