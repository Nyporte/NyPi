//! Amazon Bedrock adapter (`bedrock-converse-stream`), ported from Pi's
//! bedrock-converse-stream.ts. Pi uses the AWS SDK; NyPi calls the Converse stream REST endpoint
//! itself: requests are signed with SigV4 (or carry a Bedrock API key as a bearer token), and the
//! response is AWS's binary event stream, decoded and CRC-checked here.
//!
//! Credentials, all read through the private env lookup: `AWS_BEARER_TOKEN_BEDROCK`; else
//! `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optional `AWS_SESSION_TOKEN`; else the static
//! keys of `AWS_PROFILE` (or `default`) in `~/.aws/credentials`. Not supported yet: SSO, assumed
//! roles, and instance or container credentials. Region: an ARN model id's region, then
//! `AWS_REGION`, `AWS_DEFAULT_REGION`, the profile's region in `~/.aws/config`, the catalog
//! endpoint's region, and finally us-east-1.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{Hmac, Mac};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::auth::lookup_env;
use crate::event::{EventSender, MessageBuilder};
use crate::models::{Model, ThinkingLevel};
use crate::provider::{Chunk, RequestFailure, StreamOptions, next_chunk, post_json};
use crate::transform::prepare_for_model;
use crate::types::{ContentBlock, Context, Message, StopReason};

const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";
const REDACTED_THINKING_PLACEHOLDER: &str = "[Reasoning redacted]";
const THINKING_BINDING_CONTROLS_BETA: &str = "thinking-binding-controls-2026-08-01";
const ERROR_TOOL_IMAGE_PLACEHOLDER: &str = "(image omitted: see below)";

pub(crate) async fn run(model: Model, ctx: Context, opts: StreamOptions, tx: EventSender) {
    let mut builder = MessageBuilder::start(&model, tx);
    let region = resolve_region(&model);
    let endpoint = endpoint(&model, &region);
    let path = format!("/model/{}/converse-stream", uri_encode(&model.id, true));
    let url = format!("{endpoint}{path}");
    let body = build_request(&model, &ctx, &opts, &region);
    let payload = body.to_string();

    let mut headers: Vec<(String, String)> = model
        .headers
        .iter()
        // SigV4 and the bearer path own these; a model header must not override them.
        .filter(|(k, _)| {
            let k = k.to_lowercase();
            !(k.starts_with("x-amz-") || k == "authorization" || k == "host")
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let skip_auth = lookup_env("AWS_BEDROCK_SKIP_AUTH").as_deref() == Some("1");
    if let Some(token) = opts.api_key.as_ref().filter(|_| !skip_auth) {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    } else if !skip_auth {
        let Some(credentials) = credentials() else {
            builder.finish(
                StopReason::Error,
                Some(
                    "No AWS credentials: set AWS_BEARER_TOKEN_BEDROCK, or AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, \
                     or AWS_PROFILE with static keys in ~/.aws/credentials (SSO and instance roles are not supported yet)"
                        .into(),
                ),
            );
            return;
        };
        let host = url_host(&endpoint);
        let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        headers.extend(sign(
            &credentials,
            &region,
            "bedrock",
            "POST",
            &path,
            "",
            &host,
            "application/json",
            &payload,
            &now,
        ));
    }

    let response = match post_json(&url, &headers, &body, &opts.cancel).await {
        Ok(response) => response,
        Err(failure) => return finish_failure(builder, failure),
    };
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    let mut state = StreamState::default();
    loop {
        match next_chunk(&mut stream, &opts.cancel).await {
            Chunk::Data(bytes) => buffer.extend_from_slice(&bytes),
            Chunk::End => break,
            Chunk::Aborted => return finish_failure(builder, RequestFailure::Aborted),
            Chunk::Failed(message) => return finish_failure(builder, RequestFailure::Failed(message)),
        }
        let frames = match decode_frames(&mut buffer) {
            Ok(frames) => frames,
            Err(message) => return builder.finish(StopReason::Error, Some(message)),
        };
        for frame in frames {
            if let Err(message) = state.apply(&mut builder, &frame) {
                state.flush(&mut builder);
                return builder.finish(StopReason::Error, Some(message));
            }
        }
    }
    state.finish(builder);
}

fn finish_failure(builder: MessageBuilder, failure: RequestFailure) {
    let (stop, message) = failure.into_parts();
    builder.finish(stop, Some(message));
}

// ---------------------------------------------------------------------------------------------
// Request

fn matches_any(model: &Model, needles: &[&str]) -> bool {
    [model.id.as_str(), model.name.as_str()].iter().any(|value| {
        let lower = value.to_lowercase();
        let dashed: String = lower.chars().map(|c| if " _.:".contains(c) { '-' } else { c }).collect();
        needles.iter().any(|n| lower.contains(n) || dashed.contains(n))
    })
}

pub(crate) fn is_claude(model: &Model) -> bool {
    let (id, name) = (model.id.to_lowercase(), model.name.to_lowercase());
    id.contains("anthropic.claude") || id.contains("anthropic/claude") || name.contains("claude")
}

const CLAUDE_5: [&str; 4] = ["opus-5", "sonnet-5", "haiku-5", "fable-5"];

fn supports_adaptive_thinking(model: &Model) -> bool {
    matches_any(model, &["opus-4-6", "opus-4-7", "opus-4-8", "sonnet-4-6"]) || matches_any(model, &CLAUDE_5)
}

fn supports_block_binding(model: &Model) -> bool {
    matches_any(model, &["opus-4-7", "opus-4-8"]) || matches_any(model, &CLAUDE_5)
}

fn supports_prompt_caching(model: &Model) -> bool {
    if !matches_any(model, &["claude"]) {
        return lookup_env("AWS_BEDROCK_FORCE_CACHE").as_deref() == Some("1");
    }
    matches_any(model, &CLAUDE_5) || matches_any(model, &["-4-", "claude-3-7-sonnet", "claude-3-5-haiku"])
}

fn claude_effort(model: &Model, level: ThinkingLevel) -> String {
    if level == ThinkingLevel::Xhigh && (matches_any(model, &["opus-4-7", "opus-4-8"]) || matches_any(model, &CLAUDE_5))
    {
        return "xhigh".into();
    }
    if let Some(Some(mapped)) = model.thinking_level_map.get(level.as_str()) {
        return mapped.clone();
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    }
    .into()
}

/// Pi's default budgets for budget-based Claude thinking (simple-options.ts).
fn thinking_budget(level: ThinkingLevel) -> u64 {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        _ => 16384,
    }
}

pub(crate) fn build_request(model: &Model, ctx: &Context, opts: &StreamOptions, region: &str) -> Value {
    // Converse takes one system block list, not system messages in the conversation.
    let ctx = &crate::transcript::prepare(ctx, false);
    let reasoning = (model.reasoning && opts.thinking != ThinkingLevel::Off).then_some(opts.thinking);
    let claude = is_claude(model);
    let mut max_tokens = crate::provider::requested_max_tokens(model, opts.max_tokens);
    let mut extra: Option<Value> = None;
    if let Some(level) = reasoning {
        let gov_cloud =
            region.starts_with("us-gov-") || model.id.starts_with("us-gov.") || model.id.starts_with("arn:aws-us-gov:");
        if claude {
            let mut fields = if supports_adaptive_thinking(model) {
                let binding = !gov_cloud && supports_block_binding(model);
                let mut thinking = json!({ "type": "adaptive" });
                if !gov_cloud {
                    thinking["display"] = json!("summarized");
                }
                if binding {
                    thinking["block_binding"] = json!({ "prefix_mismatch_behavior": "drop_block" });
                }
                let mut fields =
                    json!({ "thinking": thinking, "output_config": { "effort": claude_effort(model, level) } });
                if binding {
                    fields["anthropic_beta"] = json!([THINKING_BINDING_CONTROLS_BETA]);
                }
                fields
            } else {
                // The answer needs room beside the budget, as in Pi's adjustMaxTokensForThinking.
                let mut budget = thinking_budget(level);
                let ceiling = match opts.max_tokens {
                    Some(base) => (base + budget).min(model.max_tokens),
                    None => model.max_tokens,
                };
                max_tokens = Some(ceiling);
                budget = budget.min(ceiling.saturating_sub(1024));
                let mut thinking = json!({ "type": "enabled", "budget_tokens": budget });
                if !gov_cloud {
                    thinking["display"] = json!("summarized");
                }
                json!({ "thinking": thinking, "anthropic_beta": ["interleaved-thinking-2025-05-14"] })
            };
            if fields.get("anthropic_beta").is_none() {
                fields.as_object_mut().expect("object").remove("anthropic_beta");
            }
            extra = Some(fields);
        } else if matches_any(model, &["gpt-oss"]) {
            let effort = match level {
                ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
                ThinkingLevel::Medium => "medium",
                _ => "high",
            };
            extra = Some(json!({ "reasoning_effort": effort }));
        } else if matches_any(model, &["gpt-"]) {
            let effort = match model.thinking_level_map.get(level.as_str()) {
                Some(Some(mapped)) => mapped.clone(),
                _ if level == ThinkingLevel::Minimal => "low".into(),
                _ => level.as_str().into(),
            };
            extra = Some(json!({ "reasoning": { "effort": effort } }));
        }
    }

    let caching = supports_prompt_caching(model);
    let mut body = json!({ "messages": convert_messages(model, ctx, caching) });
    if !ctx.system_prompt.is_empty() {
        let mut system = vec![json!({ "text": ctx.system_prompt })];
        if caching {
            system.push(json!({ "cachePoint": { "type": "default" } }));
        }
        body["system"] = Value::Array(system);
    }
    let mut inference = Map::new();
    if let Some(max_tokens) = max_tokens {
        inference.insert("maxTokens".into(), json!(max_tokens));
    }
    if let Some(temperature) = opts.temperature {
        inference.insert("temperature".into(), json!(temperature));
    }
    if !inference.is_empty() {
        body["inferenceConfig"] = Value::Object(inference);
    }
    if !ctx.tools.is_empty() {
        let tools: Vec<Value> = ctx
            .tools
            .iter()
            .map(|t| json!({ "toolSpec": { "name": t.name, "description": t.description, "inputSchema": { "json": t.parameters } } }))
            .collect();
        body["toolConfig"] = json!({ "tools": tools });
    }
    if let Some(extra) = extra {
        body["additionalModelRequestFields"] = extra;
    }
    body
}

fn normalize_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(64).collect()
}

fn text_block(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({ "text": text }))
}

fn image_block(mime_type: &str, data: &str) -> Value {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    json!({ "image": { "format": format, "source": { "bytes": data } } })
}

/// Bedrock rejects empty keys in tool input documents.
fn sanitize_document(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(sanitize_document).collect()),
        Value::Object(object) => Value::Object(
            object.iter().filter(|(k, _)| !k.is_empty()).map(|(k, v)| (k.clone(), sanitize_document(v))).collect(),
        ),
        other => other.clone(),
    }
}

fn convert_messages(model: &Model, ctx: &Context, caching: bool) -> Vec<Value> {
    let claude = is_claude(model);
    let messages = prepare_for_model(&ctx.messages, model);
    let mut out: Vec<Value> = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::System(_) => {}
            Message::User(user) => {
                let mut content: Vec<Value> = user
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => text_block(text),
                        ContentBlock::Image { data, mime_type } => Some(image_block(mime_type, data)),
                        _ => None,
                    })
                    .collect();
                if content.is_empty() {
                    content.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
                }
                out.push(json!({ "role": "user", "content": content }));
            }
            Message::Assistant(assistant) => {
                let mut content = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text, .. } => content.extend(text_block(text)),
                        ContentBlock::ToolCall { id, name, arguments, .. } => content.push(json!({
                            "toolUse": { "toolUseId": normalize_id(id), "name": name, "input": sanitize_document(arguments) }
                        })),
                        ContentBlock::Thinking { redacted: true, signature, .. } => {
                            // Encrypted reasoning is replayed as the opaque payload it arrived as.
                            if let Some(signature) = signature.as_deref().filter(|s| BASE64.decode(s).is_ok_and(|b| !b.is_empty())) {
                                content.push(json!({ "reasoningContent": { "redactedContent": signature } }));
                            }
                        }
                        ContentBlock::Thinking { thinking, signature, .. } => {
                            if thinking.trim().is_empty() {
                                continue;
                            }
                            // Only Claude accepts a signature on reasoning text; an unsigned Claude
                            // block would be rejected, so it goes back as plain text.
                            if !claude {
                                content.push(json!({ "reasoningContent": { "reasoningText": { "text": thinking } } }));
                            } else if let Some(signature) = signature.as_deref().filter(|s| !s.trim().is_empty()) {
                                content.push(json!({
                                    "reasoningContent": { "reasoningText": { "text": thinking, "signature": signature } }
                                }));
                            } else {
                                content.push(json!({ "text": thinking }));
                            }
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if !content.is_empty() {
                    out.push(json!({ "role": "assistant", "content": content }));
                }
            }
            Message::ToolResult(_) => {
                // Bedrock wants every result of a turn in one user message.
                let mut results = Vec::new();
                let mut error_images = Vec::new();
                while let Some(Message::ToolResult(result)) = messages.get(index) {
                    let split = claude
                        && result.is_error
                        && result.content.iter().any(|b| matches!(b, ContentBlock::Image { .. }));
                    let mut content = Vec::new();
                    let mut previous_placeholder = false;
                    for block in &result.content {
                        match block {
                            // Claude rejects images in error results; they follow all results instead.
                            ContentBlock::Image { data, mime_type } if split => {
                                if !previous_placeholder {
                                    content.push(json!({ "text": ERROR_TOOL_IMAGE_PLACEHOLDER }));
                                }
                                previous_placeholder = true;
                                error_images.push(image_block(mime_type, data));
                            }
                            ContentBlock::Image { data, mime_type } => content.push(image_block(mime_type, data)),
                            ContentBlock::Text { text, .. } => {
                                previous_placeholder = text == ERROR_TOOL_IMAGE_PLACEHOLDER;
                                content.extend(text_block(text));
                            }
                            _ => {}
                        }
                    }
                    if content.is_empty() {
                        content.push(json!({ "text": EMPTY_TEXT_PLACEHOLDER }));
                    }
                    if split {
                        let caption = format!(
                            "Images from the error result of tool call {} ({}):",
                            normalize_id(&result.tool_call_id),
                            result.tool_name
                        );
                        let at = error_images.len()
                            - result.content.iter().filter(|b| matches!(b, ContentBlock::Image { .. })).count();
                        error_images.insert(at, json!({ "text": caption }));
                    }
                    results.push(json!({ "toolResult": {
                        "toolUseId": normalize_id(&result.tool_call_id),
                        "content": content,
                        "status": if result.is_error { "error" } else { "success" },
                    } }));
                    index += 1;
                }
                results.extend(error_images);
                out.push(json!({ "role": "user", "content": results }));
                continue;
            }
        }
        index += 1;
    }
    if caching
        && let Some(last) = out.last_mut().filter(|m| m["role"] == "user")
        && let Some(content) = last["content"].as_array_mut()
    {
        content.push(json!({ "cachePoint": { "type": "default" } }));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Stream

#[derive(Default)]
struct StreamState {
    /// Bedrock content block index -> our content index.
    open: HashMap<u64, usize>,
    /// Signature text and decoded redacted bytes per open thinking block.
    signatures: HashMap<usize, String>,
    redacted: HashMap<usize, Vec<u8>>,
    stop: Option<(StopReason, Option<String>)>,
}

impl StreamState {
    fn apply(&mut self, builder: &mut MessageBuilder, frame: &Frame) -> Result<(), String> {
        let payload: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
        if frame.header(":message-type") == Some("exception") {
            let kind = frame.header(":exception-type").unwrap_or("UnknownException");
            let message = payload["message"].as_str().or_else(|| payload["Message"].as_str()).unwrap_or("no message");
            return Err(format!("{}: {message}", error_prefix(kind)));
        }
        let block_index = payload["contentBlockIndex"].as_u64().unwrap_or(0);
        match frame.header(":event-type").unwrap_or_default() {
            "messageStart" if payload["role"] != "assistant" => {
                return Err("Unexpected assistant message start but got user message start instead".into());
            }
            "contentBlockStart" => {
                if let Some(tool) = payload["start"].get("toolUse").filter(|t| t.is_object()) {
                    let index = builder.tool_call_start(
                        tool["toolUseId"].as_str().unwrap_or_default(),
                        tool["name"].as_str().unwrap_or_default(),
                    );
                    self.open.insert(block_index, index);
                }
            }
            "contentBlockDelta" => self.delta(builder, block_index, &payload["delta"]),
            "contentBlockStop" => {
                if let Some(index) = self.open.remove(&block_index) {
                    self.close(builder, index);
                }
            }
            "messageStop" => {
                let reason = payload["stopReason"].as_str().unwrap_or_default();
                self.stop = Some(match reason {
                    "end_turn" | "stop_sequence" => (StopReason::Stop, None),
                    "max_tokens" | "model_context_window_exceeded" => (StopReason::Length, None),
                    "tool_use" => (StopReason::ToolUse, None),
                    "" => (StopReason::Error, None),
                    other => (StopReason::Error, Some(format!("Provider stopped with: {other}"))),
                });
            }
            "metadata" => {
                if let Some(usage) = payload.get("usage").filter(|u| u.is_object()) {
                    let out = builder.usage_mut();
                    out.input = usage["inputTokens"].as_u64().unwrap_or(0);
                    out.output = usage["outputTokens"].as_u64().unwrap_or(0);
                    out.cache_read = usage["cacheReadInputTokens"].as_u64().unwrap_or(0);
                    out.cache_write = usage["cacheWriteInputTokens"].as_u64().unwrap_or(0);
                    out.total_tokens = usage["totalTokens"].as_u64().unwrap_or(out.input + out.output);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn delta(&mut self, builder: &mut MessageBuilder, block_index: u64, delta: &Value) {
        if let Some(text) = delta["text"].as_str() {
            // Text blocks have no start event.
            let index = *self.open.entry(block_index).or_insert_with(|| {
                let index = builder.next_index();
                builder.begin_text();
                index
            });
            builder.text_at(index, text);
        } else if let Some(input) = delta["toolUse"]["input"].as_str() {
            if let Some(index) = self.open.get(&block_index) {
                builder.tool_call_delta(*index, input);
            }
        } else if let Some(reasoning) = delta.get("reasoningContent").filter(|r| r.is_object()) {
            let index = *self.open.entry(block_index).or_insert_with(|| {
                let index = builder.next_index();
                builder.begin_thinking(false, None);
                index
            });
            if let Some(text) = reasoning["text"].as_str() {
                builder.thinking_at(index, text);
            }
            // A block holds either a Claude signature or an encrypted payload, never both.
            if let Some(signature) = reasoning["signature"].as_str()
                && !self.redacted.contains_key(&index)
            {
                self.signatures.entry(index).or_default().push_str(signature);
            }
            if let Some(bytes) =
                reasoning["redactedContent"].as_str().and_then(|b| BASE64.decode(b).ok()).filter(|b| !b.is_empty())
            {
                if !self.redacted.contains_key(&index) {
                    builder.set_thinking_redacted(index, REDACTED_THINKING_PLACEHOLDER);
                    self.signatures.remove(&index);
                }
                self.redacted.entry(index).or_default().extend(bytes);
            }
        }
    }

    fn close(&mut self, builder: &mut MessageBuilder, index: usize) {
        if let Some(bytes) = self.redacted.remove(&index) {
            builder.set_thinking(index, None, Some(BASE64.encode(bytes)));
        } else if let Some(signature) = self.signatures.remove(&index) {
            builder.set_thinking(index, None, Some(signature));
        }
        builder.tool_call_end(index);
    }

    /// A stream can settle without stopping every block.
    fn flush(&mut self, builder: &mut MessageBuilder) {
        let open: Vec<usize> = self.open.drain().map(|(_, index)| index).collect();
        for index in open {
            self.close(builder, index);
        }
    }

    fn finish(mut self, mut builder: MessageBuilder) {
        self.flush(&mut builder);
        match self.stop.take() {
            Some((stop, error)) => builder.finish(stop, error),
            None => builder.finish(StopReason::Error, Some("Bedrock stream ended without a stop reason".into())),
        }
    }
}

/// Pi keeps these prefixes so retry and context-overflow checks can match on them.
fn error_prefix(kind: &str) -> String {
    let mut name = kind.to_string();
    if let Some(first) = name.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    match name.as_str() {
        "InternalServerException" => "Internal server error".into(),
        "ModelStreamErrorException" => "Model stream error".into(),
        "ValidationException" => "Validation error".into(),
        "ThrottlingException" => "Throttling error".into(),
        "ServiceUnavailableException" => "Service unavailable".into(),
        _ => name,
    }
}

// ---------------------------------------------------------------------------------------------
// AWS event stream framing

pub(crate) struct Frame {
    headers: Vec<(String, String)>,
    payload: Vec<u8>,
}

impl Frame {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Take every complete frame off the front of `buffer`. Each frame is
/// `total_len | headers_len | prelude_crc | headers | payload | message_crc`, big-endian.
pub(crate) fn decode_frames(buffer: &mut Vec<u8>) -> Result<Vec<Frame>, String> {
    let mut frames = Vec::new();
    loop {
        if buffer.len() < 12 {
            return Ok(frames);
        }
        let be = |at: usize| u32::from_be_bytes(buffer[at..at + 4].try_into().expect("4 bytes"));
        let (total, headers_len, prelude_crc) = (be(0) as usize, be(4) as usize, be(8));
        if crc32(&buffer[..8]) != prelude_crc {
            return Err("Bedrock event stream is corrupt (prelude checksum)".into());
        }
        if total < 16 + headers_len {
            return Err("Bedrock event stream is corrupt (frame length)".into());
        }
        if buffer.len() < total {
            return Ok(frames);
        }
        if crc32(&buffer[..total - 4]) != be(total - 4) {
            return Err("Bedrock event stream is corrupt (message checksum)".into());
        }
        let headers = parse_headers(&buffer[12..12 + headers_len])?;
        let payload = buffer[12 + headers_len..total - 4].to_vec();
        buffer.drain(..total);
        frames.push(Frame { headers, payload });
    }
}

fn parse_headers(mut bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    let corrupt = || "Bedrock event stream is corrupt (headers)".to_string();
    let mut headers = Vec::new();
    while !bytes.is_empty() {
        let name_len = *bytes.first().ok_or_else(corrupt)? as usize;
        let name = String::from_utf8_lossy(bytes.get(1..1 + name_len).ok_or_else(corrupt)?).into_owned();
        bytes = &bytes[1 + name_len..];
        let kind = *bytes.first().ok_or_else(corrupt)?;
        bytes = &bytes[1..];
        let fixed = |n: usize, bytes: &mut &[u8]| -> Result<Vec<u8>, String> {
            let value = bytes.get(..n).ok_or_else(corrupt)?.to_vec();
            *bytes = &bytes[n..];
            Ok(value)
        };
        let value = match kind {
            0 => "true".to_string(),
            1 => "false".to_string(),
            2 => fixed(1, &mut bytes)?[0].to_string(),
            3 => i16::from_be_bytes(fixed(2, &mut bytes)?.try_into().expect("2 bytes")).to_string(),
            4 => i32::from_be_bytes(fixed(4, &mut bytes)?.try_into().expect("4 bytes")).to_string(),
            5 | 8 => i64::from_be_bytes(fixed(8, &mut bytes)?.try_into().expect("8 bytes")).to_string(),
            6 | 7 => {
                let len = u16::from_be_bytes(fixed(2, &mut bytes)?.try_into().expect("2 bytes")) as usize;
                String::from_utf8_lossy(&fixed(len, &mut bytes)?).into_owned()
            }
            9 => fixed(16, &mut bytes)?.iter().map(|b| format!("{b:02x}")).collect(),
            _ => return Err(corrupt()),
        };
        headers.push((name, value));
    }
    Ok(headers)
}

/// CRC-32 (IEEE), as the event stream uses.
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

// ---------------------------------------------------------------------------------------------
// Credentials, region, endpoint, and SigV4

pub(crate) struct Credentials {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
}

fn credentials() -> Option<Credentials> {
    if let (Some(access_key), Some(secret_key)) = (lookup_env("AWS_ACCESS_KEY_ID"), lookup_env("AWS_SECRET_ACCESS_KEY"))
    {
        return Some(Credentials { access_key, secret_key, session_token: lookup_env("AWS_SESSION_TOKEN") });
    }
    let profile = lookup_env("AWS_PROFILE").unwrap_or_else(|| "default".into());
    let path = lookup_env("AWS_SHARED_CREDENTIALS_FILE")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".aws/credentials")))?;
    let section = ini_section(&std::fs::read_to_string(path).ok()?, &profile);
    Some(Credentials {
        access_key: section.get("aws_access_key_id")?.clone(),
        secret_key: section.get("aws_secret_access_key")?.clone(),
        session_token: section.get("aws_session_token").cloned(),
    })
}

fn ini_section(text: &str, name: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let header = header.trim();
            inside = header == name || header.strip_prefix("profile ").is_some_and(|h| h.trim() == name);
            continue;
        }
        if inside && let Some((key, value)) = line.split_once('=') {
            values.insert(key.trim().to_lowercase(), value.trim().to_string());
        }
    }
    values
}

/// The region in `bedrock-runtime.<region>.amazonaws.com`, if the base URL is a standard endpoint.
fn endpoint_region(base_url: &str) -> Option<String> {
    let host = url_host(base_url).to_lowercase();
    let rest = host.strip_prefix("bedrock-runtime-fips.").or_else(|| host.strip_prefix("bedrock-runtime."))?;
    let region = rest.strip_suffix(".amazonaws.com").or_else(|| rest.strip_suffix(".amazonaws.com.cn"))?;
    (!region.contains('.')).then(|| region.to_string())
}

fn configured_region() -> Option<String> {
    lookup_env("AWS_REGION").or_else(|| lookup_env("AWS_DEFAULT_REGION")).or_else(|| {
        let profile = lookup_env("AWS_PROFILE").unwrap_or_else(|| "default".into());
        let path = lookup_env("AWS_CONFIG_FILE")
            .map(std::path::PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".aws/config")))?;
        ini_section(&std::fs::read_to_string(path).ok()?, &profile).get("region").cloned()
    })
}

fn resolve_region(model: &Model) -> String {
    // arn:aws[-partition]:bedrock:<region>:...
    let arn_region = model.id.strip_prefix("arn:").and_then(|rest| {
        let parts: Vec<&str> = rest.split(':').collect();
        (parts.len() > 3 && parts[0].starts_with("aws") && parts[1] == "bedrock").then(|| parts[2].to_string())
    });
    arn_region
        .or_else(configured_region)
        .or_else(|| endpoint_region(&model.base_url))
        .unwrap_or_else(|| "us-east-1".into())
}

/// A standard catalog endpoint follows the configured region; a custom one (VPC, proxy) is kept.
fn endpoint(model: &Model, region: &str) -> String {
    match endpoint_region(&model.base_url) {
        Some(_) if configured_region().is_some() => format!("https://bedrock-runtime.{region}.amazonaws.com"),
        Some(_) | None if !model.base_url.is_empty() => model.base_url.trim_end_matches('/').to_string(),
        _ => format!("https://bedrock-runtime.{region}.amazonaws.com"),
    }
}

fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    rest.split('/').next().unwrap_or_default().to_string()
}

/// RFC 3986 encoding as SigV4 wants it: unreserved characters stay, everything else is %XX.
fn uri_encode(text: &str, encode_slash: bool) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || "-_.~".contains(c) || (c == '/' && !encode_slash) {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SigV4 headers for a JSON request. `path` is already URI-encoded once; non-S3 services sign
/// each segment encoded a second time.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sign(
    credentials: &Credentials,
    region: &str,
    service: &str,
    method: &str,
    path: &str,
    query: &str,
    host: &str,
    content_type: &str,
    payload: &str,
    amz_date: &str,
) -> Vec<(String, String)> {
    let date = &amz_date[..8];
    let mut headers: Vec<(String, String)> = vec![
        ("content-type".into(), content_type.to_string()),
        ("host".into(), host.to_string()),
        ("x-amz-date".into(), amz_date.to_string()),
    ];
    if let Some(token) = &credentials.session_token {
        headers.push(("x-amz-security-token".into(), token.clone()));
    }
    headers.sort();
    let canonical_uri: String = path.split('/').map(|segment| uri_encode(segment, true)).collect::<Vec<_>>().join("/");
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{query}\n{canonical_headers}\n{signed_headers}\n{}",
        hex(&Sha256::digest(payload.as_bytes()))
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign =
        format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex(&Sha256::digest(canonical_request.as_bytes())));
    let key = [region, service, "aws4_request"]
        .iter()
        .fold(hmac(format!("AWS4{}", credentials.secret_key).as_bytes(), date), |key, part| hmac(&key, part));
    let signature = hex(&hmac(&key, &string_to_sign));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key
    );
    // reqwest sets host and content-type itself; sending them twice would break the signature.
    headers.retain(|(k, _)| k != "host" && k != "content-type");
    headers.push(("authorization".into(), authorization));
    headers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::StreamEvent;
    use crate::models::Api;
    use crate::types::{AssistantMessage, ToolResultMessage};

    fn model(id: &str) -> Model {
        let mut model: Model = serde_json::from_value(json!({
            "id": id, "name": id, "api": "bedrock-converse-stream", "provider": "amazon-bedrock",
            "baseUrl": "https://bedrock-runtime.us-east-1.amazonaws.com", "reasoning": true,
            "input": ["text", "image"], "contextWindow": 200000, "maxTokens": 64000
        }))
        .unwrap();
        model.api = Api::BedrockConverseStream;
        model
    }

    #[test]
    fn crc32_and_sigv4_match_reference_vectors() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        // AWS's documented example (IAM ListUsers, 2015-08-30).
        let credentials = Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let key = ["us-east-1", "iam", "aws4_request"]
            .iter()
            .fold(hmac(format!("AWS4{}", credentials.secret_key).as_bytes(), "20150830"), |k, p| hmac(&k, p));
        assert_eq!(hex(&key), "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9");
        let headers = sign(
            &credentials,
            "us-east-1",
            "iam",
            "GET",
            "/",
            "Action=ListUsers&Version=2010-05-08",
            "iam.amazonaws.com",
            "application/x-www-form-urlencoded; charset=utf-8",
            "",
            "20150830T123600Z",
        );
        let auth = &headers.iter().find(|(k, _)| k == "authorization").unwrap().1;
        assert!(auth.ends_with("Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"), "{auth}");
    }

    #[test]
    fn sigv4_double_encodes_the_model_path() {
        let credentials =
            Credentials { access_key: "AK".into(), secret_key: "SK".into(), session_token: Some("T".into()) };
        let path = format!("/model/{}/converse-stream", uri_encode("anthropic.claude-sonnet-4-6:0", true));
        assert_eq!(path, "/model/anthropic.claude-sonnet-4-6%3A0/converse-stream");
        let headers = sign(
            &credentials,
            "us-east-1",
            "bedrock",
            "POST",
            &path,
            "",
            "h",
            "application/json",
            "{}",
            "20261101T120000Z",
        );
        let auth = &headers.iter().find(|(k, _)| k == "authorization").unwrap().1;
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AK/20261101/us-east-1/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, Signature="));
        assert!(headers.iter().any(|(k, v)| k == "x-amz-security-token" && v == "T"));
        assert!(!headers.iter().any(|(k, _)| k == "host"));
    }

    fn frame(event: &str, payload: Value) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [(":event-type", event), (":message-type", "event"), (":content-type", "application/json")]
        {
            headers.push(name.len() as u8);
            headers.extend(name.as_bytes());
            headers.push(7);
            headers.extend((value.len() as u16).to_be_bytes());
            headers.extend(value.as_bytes());
        }
        let payload = payload.to_string().into_bytes();
        let total = (16 + headers.len() + payload.len()) as u32;
        let mut out = Vec::new();
        out.extend(total.to_be_bytes());
        out.extend((headers.len() as u32).to_be_bytes());
        out.extend(crc32(&out).to_be_bytes());
        out.extend(headers);
        out.extend(payload);
        out.extend(crc32(&out).to_be_bytes());
        out
    }

    #[test]
    fn decodes_frames_across_chunk_boundaries_and_builds_the_message() {
        let mut bytes = Vec::new();
        for (event, payload) in [
            ("messageStart", json!({ "role": "assistant" })),
            (
                "contentBlockDelta",
                json!({ "contentBlockIndex": 0, "delta": { "reasoningContent": { "text": "Plan" } } }),
            ),
            (
                "contentBlockDelta",
                json!({ "contentBlockIndex": 0, "delta": { "reasoningContent": { "signature": "SIG" } } }),
            ),
            ("contentBlockStop", json!({ "contentBlockIndex": 0 })),
            ("contentBlockDelta", json!({ "contentBlockIndex": 1, "delta": { "text": "Hi" } })),
            ("contentBlockStop", json!({ "contentBlockIndex": 1 })),
            (
                "contentBlockStart",
                json!({ "contentBlockIndex": 2, "start": { "toolUse": { "toolUseId": "tu_1", "name": "ls" } } }),
            ),
            ("contentBlockDelta", json!({ "contentBlockIndex": 2, "delta": { "toolUse": { "input": "{\"path\":" } } })),
            ("contentBlockDelta", json!({ "contentBlockIndex": 2, "delta": { "toolUse": { "input": "\".\"}" } } })),
            ("contentBlockStop", json!({ "contentBlockIndex": 2 })),
            ("messageStop", json!({ "stopReason": "tool_use" })),
            (
                "metadata",
                json!({ "usage": { "inputTokens": 30, "outputTokens": 9, "cacheReadInputTokens": 5, "totalTokens": 44 } }),
            ),
        ] {
            bytes.extend(frame(event, payload));
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut builder = MessageBuilder::start(&model("anthropic.claude-sonnet-4-6"), tx);
        let mut state = StreamState::default();
        let mut buffer = Vec::new();
        for chunk in bytes.chunks(7) {
            buffer.extend_from_slice(chunk);
            for frame in decode_frames(&mut buffer).unwrap() {
                state.apply(&mut builder, &frame).unwrap();
            }
        }
        state.finish(builder);
        let mut message = None;
        while let Ok(event) = rx.try_recv() {
            if let StreamEvent::Done { message: m } = event {
                message = Some(m);
            }
        }
        let message = message.unwrap();
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert!(
            matches!(&message.content[0], ContentBlock::Thinking { thinking, signature: Some(s), .. } if thinking == "Plan" && s == "SIG")
        );
        assert!(matches!(&message.content[1], ContentBlock::Text { text, .. } if text == "Hi"));
        assert!(matches!(&message.content[2], ContentBlock::ToolCall { arguments, .. } if arguments["path"] == "."));
        assert_eq!((message.usage.input, message.usage.cache_read), (30, 5));

        let mut corrupt = frame("metadata", json!({}));
        let last = corrupt.len() - 1;
        corrupt[last] ^= 1;
        assert!(decode_frames(&mut corrupt).is_err());
    }

    #[test]
    fn request_groups_results_caches_and_asks_for_adaptive_thinking() {
        let model = model("us.anthropic.claude-sonnet-4-6");
        let mut assistant = AssistantMessage::empty("bedrock-converse-stream", "amazon-bedrock", &model.id);
        assistant.stop_reason = StopReason::ToolUse;
        let call = |id: &str| ContentBlock::ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: json!({ "path": "a", "": 1 }),
            arguments_error: None,
            thought_signature: None,
        };
        assistant.content = vec![
            ContentBlock::Thinking { thinking: "unsigned".into(), signature: None, redacted: false },
            call("t:1"),
            call("t:2"),
        ];
        let result = |id: &str, is_error: bool, content: Vec<ContentBlock>| {
            Message::ToolResult(ToolResultMessage {
                tool_call_id: id.into(),
                tool_name: "read".into(),
                content,
                details: None,
                is_error,
                timestamp: 0,
                duration_ms: None,
            })
        };
        let image = ContentBlock::Image { data: "AAAA".into(), mime_type: "image/png".into() };
        let ctx = Context {
            system_prompt: "sys".into(),
            messages: vec![
                Message::user_text("go"),
                Message::Assistant(assistant),
                result("t:1", false, vec![ContentBlock::text("ok")]),
                result("t:2", true, vec![ContentBlock::text("bad"), image]),
            ],
            tools: vec![],
        };
        let opts = StreamOptions { thinking: ThinkingLevel::High, ..Default::default() };
        let body = build_request(&model, &ctx, &opts, "us-east-1");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[1]["content"][0],
            json!({ "text": "unsigned" }),
            "unsigned Claude thinking goes back as text"
        );
        assert_eq!(messages[1]["content"][1]["toolUse"]["toolUseId"], "t_1");
        assert_eq!(messages[1]["content"][1]["toolUse"]["input"], json!({ "path": "a" }));
        let results = messages[2]["content"].as_array().unwrap();
        assert_eq!(results[1]["toolResult"]["status"], "error");
        assert_eq!(results[1]["toolResult"]["content"][1]["text"], ERROR_TOOL_IMAGE_PLACEHOLDER);
        assert!(results[2]["text"].as_str().unwrap().starts_with("Images from the error result of tool call t_2"));
        assert_eq!(results[3]["image"]["format"], "png");
        assert_eq!(results[4], json!({ "cachePoint": { "type": "default" } }));
        assert_eq!(body["system"][1], json!({ "cachePoint": { "type": "default" } }));
        let extra = &body["additionalModelRequestFields"];
        assert_eq!(extra["thinking"]["type"], "adaptive");
        assert_eq!(extra["output_config"]["effort"], "high");
        assert!(extra.get("anthropic_beta").is_none(), "Sonnet 4.6 rejects block binding");
        assert_eq!(body["inferenceConfig"]["maxTokens"], 64000);
    }
}
