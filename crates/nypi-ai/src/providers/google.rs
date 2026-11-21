//! Google Gemini adapter (`google-generative-ai`), ported from Pi's google-generative-ai.ts and
//! google-shared.ts, speaking the REST streaming endpoint directly instead of the SDK. Thought
//! signatures are kept on the part they arrived with (text, thinking, or function call) and
//! echoed back only to the same model, as Gemini requires. The key goes in a header, never the
//! URL. Not ported: Vertex AI, custom thinking budgets, tool choice, strict (VALIDATED) tools.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};

use crate::event::{EventSender, MessageBuilder};
use crate::models::{Model, ThinkingLevel};
use crate::provider::{Chunk, RequestFailure, StreamOptions, next_chunk, post_json};
use crate::sse::SseParser;
use crate::transform::prepare_for_model;
use crate::types::{ContentBlock, Context, Message, StopReason, text_of};

/// Makes generated tool call ids unique within the process, as Pi's counter does.
static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) async fn run(model: Model, ctx: Context, opts: StreamOptions, tx: EventSender) {
    let mut builder = MessageBuilder::start(&model, tx);
    let Some(key) = &opts.api_key else {
        builder.finish(StopReason::Error, Some(format!("No API key for provider \"{}\"", model.provider)));
        return;
    };
    let mut headers: Vec<(String, String)> = model.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    headers.push(("x-goog-api-key".into(), key.clone()));
    let body = build_request(&model, &ctx, &opts);
    let url = format!("{}/models/{}:streamGenerateContent?alt=sse", model.base_url.trim_end_matches('/'), model.id);
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
            let Ok(chunk) = serde_json::from_str::<Value>(&event.data) else { continue };
            if let Some(error) = chunk.get("error").filter(|e| e.is_object()) {
                let message = error["message"].as_str().map(str::to_string).unwrap_or_else(|| error.to_string());
                builder.finish(StopReason::Error, Some(message));
                return;
            }
            state.apply(&mut builder, &chunk);
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

#[derive(Default)]
struct StreamState {
    finish_reason: Option<String>,
    tool_ids: Vec<String>,
}

impl StreamState {
    fn apply(&mut self, builder: &mut MessageBuilder, chunk: &Value) {
        let candidate = &chunk["candidates"][0];
        for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
            let signature = part["thoughtSignature"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
            if let Some(text) = part["text"].as_str() {
                let index = if part["thought"] == true {
                    if !builder.last_is_thinking() {
                        builder.begin_thinking(false, None);
                    }
                    let index = builder.next_index() - 1;
                    builder.thinking_at(index, text);
                    if signature.is_some() {
                        builder.set_thinking(index, None, signature.clone());
                    }
                    index
                } else {
                    if !builder.last_is_text() {
                        builder.begin_text();
                    }
                    let index = builder.next_index() - 1;
                    builder.text_at(index, text);
                    index
                };
                // A later delta without a signature must not erase the block's signature.
                if let Some(signature) = signature.clone()
                    && part["thought"] != true
                {
                    builder.set_text_signature(index, signature);
                }
            }
            if let Some(call) = part.get("functionCall").filter(|c| c.is_object()) {
                let name = call["name"].as_str().unwrap_or_default();
                let id = match call["id"].as_str().filter(|id| !id.is_empty() && !self.tool_ids.iter().any(|t| t == id))
                {
                    Some(id) => id.to_string(),
                    None => {
                        let n = TOOL_CALL_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
                        format!("{name}_{}_{n}", chrono::Utc::now().timestamp_millis())
                    }
                };
                self.tool_ids.push(id.clone());
                let index = builder.tool_call_start(&id, name);
                let arguments = if call["args"].is_object() { call["args"].clone() } else { json!({}) };
                builder.tool_call_delta(index, &arguments.to_string());
                builder.tool_call_end(index);
                if let Some(signature) = signature {
                    builder.set_tool_call_signature(index, signature);
                }
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
        if let Some(usage) = chunk.get("usageMetadata").filter(|u| u.is_object()) {
            let cached = usage["cachedContentTokenCount"].as_u64().unwrap_or(0);
            let out = builder.usage_mut();
            out.input = usage["promptTokenCount"].as_u64().unwrap_or(0).saturating_sub(cached);
            out.output =
                usage["candidatesTokenCount"].as_u64().unwrap_or(0) + usage["thoughtsTokenCount"].as_u64().unwrap_or(0);
            out.cache_read = cached;
            out.total_tokens = usage["totalTokenCount"].as_u64().unwrap_or(0);
        }
    }

    fn finish(self, builder: MessageBuilder) {
        let (stop, error) = match self.finish_reason.as_deref() {
            None => (StopReason::Error, Some("Google stream ended without a finish reason".to_string())),
            Some("STOP") if builder.has_tool_calls() => (StopReason::ToolUse, None),
            Some("STOP") => (StopReason::Stop, None),
            Some("MAX_TOKENS") => (StopReason::Length, None),
            Some(other) => (StopReason::Error, Some(format!("Provider stopped with: {other}"))),
        };
        builder.finish(stop, error);
    }
}

pub(crate) fn build_request(model: &Model, ctx: &Context, opts: &StreamOptions) -> Value {
    // Gemini has no system messages inside the conversation: always one leading instruction.
    let ctx = &crate::transcript::prepare(ctx, false);
    let mut body = json!({ "contents": convert_messages(model, ctx) });
    if !ctx.system_prompt.is_empty() {
        body["systemInstruction"] = json!({ "parts": [{ "text": ctx.system_prompt }] });
    }
    if !ctx.tools.is_empty() {
        let declarations: Vec<Value> = ctx
            .tools
            .iter()
            .map(|t| {
                json!({ "name": t.name, "description": t.description, "parametersJsonSchema": const_as_enum(&t.parameters, false) })
            })
            .collect();
        body["tools"] = json!([{ "functionDeclarations": declarations }]);
    }
    let mut generation = Map::new();
    if let Some(temperature) = opts.temperature {
        generation.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = opts.max_tokens {
        generation.insert("maxOutputTokens".into(), json!(max_tokens));
    }
    if let Some(thinking) = thinking_config(model, opts.thinking) {
        generation.insert("thinkingConfig".into(), thinking);
    }
    if !generation.is_empty() {
        body["generationConfig"] = Value::Object(generation);
    }
    body
}

/// Gemini 3 and later take a discrete `thinkingLevel`; Gemini 2.5 takes a token budget.
fn thinking_config(model: &Model, level: ThinkingLevel) -> Option<Value> {
    if !model.reasoning {
        return None;
    }
    let discrete = uses_thinking_level(&model.id);
    let Some(effort) = model.native_effort(level) else {
        if !discrete || model.thinking_can_be_disabled() {
            return Some(json!({ "thinkingBudget": 0 }));
        }
        // The model always thinks; use its lowest supported level.
        let lowest = model.native_effort(ThinkingLevel::Minimal).map(|e| google_level(&e)).unwrap_or("low");
        return Some(json!({ "thinkingLevel": lowest.to_uppercase() }));
    };
    let level = google_level(&effort);
    if discrete {
        return Some(json!({ "includeThoughts": true, "thinkingLevel": level.to_uppercase() }));
    }
    Some(json!({ "includeThoughts": true, "thinkingBudget": thinking_budget(&model.id, level) }))
}

/// Google has four levels; the higher NyPi levels map to its highest.
fn google_level(effort: &str) -> &'static str {
    match effort.to_lowercase().as_str() {
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        _ => "high",
    }
}

fn thinking_budget(id: &str, level: &str) -> i64 {
    let table: [i64; 4] = if id.contains("2.5-pro") {
        [128, 2048, 8192, 32768]
    } else if id.contains("2.5-flash-lite") {
        [512, 2048, 8192, 24576]
    } else if id.contains("2.5-flash") {
        [128, 2048, 8192, 24576]
    } else {
        return -1;
    };
    match level {
        "minimal" => table[0],
        "low" => table[1],
        "medium" => table[2],
        _ => table[3],
    }
}

/// Pi's `usesGoogleThinkingLevel`, without a regex engine.
fn uses_thinking_level(id: &str) -> bool {
    let id = id.to_lowercase();
    if id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
        || id.contains("gemma4")
        || id.contains("gemma-4")
    {
        return true;
    }
    let mut rest = id.as_str();
    while let Some(at) = rest.find("gemini-") {
        let after = &rest[at + "gemini-".len()..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(major) = digits.parse::<u32>() {
            if major >= 4 && !digits.starts_with('0') {
                return true;
            }
            if major == 3 && digits.len() == 1 {
                let tail = &after[1..];
                let tail =
                    tail.strip_prefix('.').map(|t| t.trim_start_matches(|c: char| c.is_ascii_digit())).unwrap_or(tail);
                if tail.starts_with("-pro") || tail.starts_with("-flash") {
                    return true;
                }
            }
        }
        rest = after;
    }
    false
}

fn gemini_major(id: &str) -> Option<u32> {
    let id = id.to_lowercase();
    let rest = id.strip_prefix("gemini-")?;
    let rest = rest.strip_prefix("live-").unwrap_or(rest);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Gemini 3, and Claude or gpt-oss served through Google, require ids on calls and responses.
fn requires_tool_call_id(id: &str) -> bool {
    id.starts_with("claude-") || id.starts_with("gpt-oss-") || gemini_major(id).is_some_and(|major| major >= 3)
}

fn normalize_id(model: &Model, id: &str) -> String {
    if !requires_tool_call_id(&model.id) {
        return id.to_string();
    }
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).take(64).collect()
}

/// Signatures are bytes on Google's side, so anything that is not base64 is dropped.
fn valid_signature(signature: &Option<String>) -> Option<&str> {
    let signature = signature.as_deref()?;
    let body = signature.trim_end_matches('=');
    let padding = signature.len() - body.len();
    let ok = !signature.is_empty()
        && signature.len() % 4 == 0
        && padding <= 2
        && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/');
    ok.then_some(signature)
}

fn convert_messages(model: &Model, ctx: &Context) -> Vec<Value> {
    let mut contents: Vec<Value> = Vec::new();
    let multimodal_responses = gemini_major(&model.id).is_none_or(|major| major >= 3);
    for message in prepare_for_model(&ctx.messages, model) {
        match message {
            Message::System(_) => {}
            Message::User(user) => {
                let parts: Vec<Value> = user
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(json!({ "text": text })),
                        ContentBlock::Image { data, mime_type } => {
                            Some(json!({ "inlineData": { "mimeType": mime_type, "data": data } }))
                        }
                        _ => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
            Message::Assistant(assistant) => {
                let same_model = assistant.provider == model.provider && assistant.model == model.id;
                let signed = |part: &mut Value, signature: &Option<String>| {
                    if same_model && let Some(signature) = valid_signature(signature) {
                        part["thoughtSignature"] = json!(signature);
                    }
                };
                let mut parts = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text, text_signature } => {
                            let mut part = json!({ "text": text });
                            signed(&mut part, text_signature);
                            // An empty part that carries a signature must be echoed back, or Gemini
                            // can lose its reasoning chain and stop mid-task.
                            if !text.trim().is_empty() || part.get("thoughtSignature").is_some() {
                                parts.push(part);
                            }
                        }
                        ContentBlock::Thinking { thinking, signature, .. } => {
                            let mut part = json!({ "thought": true, "text": thinking });
                            signed(&mut part, signature);
                            if !thinking.trim().is_empty() || part.get("thoughtSignature").is_some() {
                                parts.push(part);
                            }
                        }
                        ContentBlock::ToolCall { id, name, arguments, thought_signature, .. } => {
                            let mut call = json!({ "name": name, "args": arguments });
                            if requires_tool_call_id(&model.id) {
                                call["id"] = json!(normalize_id(model, id));
                            }
                            let mut part = json!({ "functionCall": call });
                            signed(&mut part, thought_signature);
                            parts.push(part);
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            Message::ToolResult(result) => {
                let text = text_of(&result.content);
                let images: Vec<Value> = if model.supports_images() {
                    result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Image { data, mime_type } => {
                                Some(json!({ "inlineData": { "mimeType": mime_type, "data": data } }))
                            }
                            _ => None,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let value = if !text.trim().is_empty() {
                    text
                } else if !images.is_empty() {
                    "(see attached image)".into()
                } else {
                    String::new()
                };
                let key = if result.is_error { "error" } else { "output" };
                let mut response = json!({ "name": result.tool_name, "response": { key: value } });
                if !images.is_empty() && multimodal_responses {
                    response["parts"] = json!(images);
                }
                if requires_tool_call_id(&model.id) {
                    response["id"] = json!(normalize_id(model, &result.tool_call_id));
                }
                let part = json!({ "functionResponse": response });
                // All responses to one batch of calls go in a single user turn.
                match contents.last_mut() {
                    Some(last)
                        if last["role"] == "user"
                            && last["parts"]
                                .as_array()
                                .is_some_and(|p| p.iter().any(|p| p.get("functionResponse").is_some())) =>
                    {
                        last["parts"].as_array_mut().expect("checked array").push(part);
                    }
                    _ => contents.push(json!({ "role": "user", "parts": [part] })),
                }
                if !images.is_empty() && !multimodal_responses {
                    let mut parts = vec![json!({ "text": "Tool result image:" })];
                    parts.extend(images);
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
        }
    }
    contents
}

/// Gemini ignores `const` in `parametersJsonSchema` but honors a one-value `enum`.
fn const_as_enum(schema: &Value, name_map: bool) -> Value {
    const NAME_MAPS: [&str; 6] =
        ["properties", "patternProperties", "$defs", "definitions", "dependentSchemas", "dependencies"];
    const SUBSCHEMAS: [&str; 16] = [
        "additionalProperties",
        "unevaluatedProperties",
        "propertyNames",
        "items",
        "prefixItems",
        "additionalItems",
        "unevaluatedItems",
        "contains",
        "allOf",
        "anyOf",
        "oneOf",
        "not",
        "if",
        "then",
        "else",
        "contentSchema",
    ];
    match schema {
        Value::Array(items) => Value::Array(items.iter().map(|i| const_as_enum(i, false)).collect()),
        Value::Object(object) => {
            let mut out: Map<String, Value> = object
                .iter()
                .map(|(key, value)| {
                    let value = if name_map || SUBSCHEMAS.contains(&key.as_str()) {
                        const_as_enum(value, false)
                    } else if NAME_MAPS.contains(&key.as_str()) {
                        const_as_enum(value, true)
                    } else {
                        value.clone()
                    };
                    (key.clone(), value)
                })
                .collect();
            if !name_map
                && !out.contains_key("enum")
                && let Some(value) = out.get("const").filter(|v| v.is_string() || v.is_number()).cloned()
            {
                out.remove("const");
                out.insert("enum".into(), json!([value]));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::StreamEvent;
    use crate::models::Api;
    use crate::types::{AssistantMessage, ToolDefinition, ToolResultMessage};

    fn model(id: &str) -> Model {
        let mut model: Model = serde_json::from_value(json!({
            "id": id, "name": id, "api": "google-generative-ai", "provider": "google",
            "baseUrl": "http://localhost", "reasoning": true, "input": ["text", "image"],
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap();
        model.api = Api::GoogleGenerativeAi;
        model
    }

    #[test]
    fn thinking_level_detection_matches_pi() {
        for id in [
            "gemini-3-flash-preview",
            "gemini-3.1-pro-preview",
            "gemini-3.8-flash",
            "gemini-4-argon",
            "gemma-4-27b",
            "gemini-flash-latest",
        ] {
            assert!(uses_thinking_level(id), "{id}");
        }
        for id in ["gemini-2.5-flash", "gemini-2.5-pro", "gemini-3-nano", "gemma-3-27b"] {
            assert!(!uses_thinking_level(id), "{id}");
        }
        let flash = model("gemini-2.5-flash");
        assert_eq!(
            thinking_config(&flash, ThinkingLevel::High).unwrap(),
            json!({ "includeThoughts": true, "thinkingBudget": 24576 })
        );
        assert_eq!(thinking_config(&flash, ThinkingLevel::Off).unwrap(), json!({ "thinkingBudget": 0 }));
        let three = model("gemini-3-pro-preview");
        assert_eq!(thinking_config(&three, ThinkingLevel::Xhigh).unwrap()["thinkingLevel"], "HIGH");
    }

    #[test]
    fn replay_keeps_signatures_for_the_same_model_and_merges_responses() {
        let model = model("gemini-3-pro-preview");
        let mut assistant = AssistantMessage::empty("google-generative-ai", "google", "gemini-3-pro-preview");
        assistant.stop_reason = StopReason::ToolUse;
        let call = |id: &str, sig: Option<&str>| ContentBlock::ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: json!({ "path": "a" }),
            arguments_error: None,
            thought_signature: sig.map(str::to_string),
        };
        assistant.content = vec![
            ContentBlock::Text { text: String::new(), text_signature: Some("c2ln".into()) },
            call("call|1", Some("AAAA")),
            call("call|2", Some("not base64!")),
        ];
        let result = |id: &str| ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::text("ok")],
            details: None,
            is_error: false,
            timestamp: 0,
            duration_ms: None,
        };
        let ctx = Context {
            system_prompt: "sys".into(),
            messages: vec![
                Message::user_text("go"),
                Message::Assistant(assistant),
                Message::ToolResult(result("call|1")),
                Message::ToolResult(result("call|2")),
            ],
            tools: vec![ToolDefinition {
                name: "read".into(),
                description: "Read".into(),
                parameters: json!({ "type": "object", "properties": { "mode": { "const": "fast" } } }),
            }],
        };
        let body = build_request(&model, &ctx, &StreamOptions::default());
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        let parts = &contents[1]["parts"];
        assert_eq!(parts[0], json!({ "text": "", "thoughtSignature": "c2ln" }));
        assert_eq!(parts[1]["functionCall"]["id"], "call_1");
        assert_eq!(parts[1]["thoughtSignature"], "AAAA");
        assert!(parts[2].get("thoughtSignature").is_none(), "invalid signature dropped");
        assert_eq!(contents[2]["parts"].as_array().unwrap().len(), 2, "responses share one user turn");
        assert_eq!(contents[2]["parts"][1]["functionResponse"]["id"], "call_2");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "sys");
        assert_eq!(
            body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["properties"]["mode"],
            json!({ "enum": ["fast"] })
        );
    }

    #[test]
    fn stream_separates_thoughts_text_and_calls() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut builder = MessageBuilder::start(&model("gemini-3-pro-preview"), tx);
        let mut state = StreamState::default();
        for chunk in [
            json!({ "candidates": [{ "content": { "parts": [{ "text": "Plan", "thought": true, "thoughtSignature": "QUFB" }] } }] }),
            json!({ "candidates": [{ "content": { "parts": [{ "text": "Hel" }, { "text": "lo", "thoughtSignature": "QkJC" }] } }] }),
            json!({ "candidates": [{ "content": { "parts": [{ "functionCall": { "name": "ls", "args": { "path": "." } }, "thoughtSignature": "Q0ND" }] }, "finishReason": "STOP" }],
                    "usageMetadata": { "promptTokenCount": 50, "cachedContentTokenCount": 10, "candidatesTokenCount": 5, "thoughtsTokenCount": 7, "totalTokenCount": 62 } }),
        ] {
            state.apply(&mut builder, &chunk);
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
            matches!(&message.content[0], ContentBlock::Thinking { thinking, signature: Some(s), .. } if thinking == "Plan" && s == "QUFB")
        );
        assert!(
            matches!(&message.content[1], ContentBlock::Text { text, text_signature: Some(s) } if text == "Hello" && s == "QkJC")
        );
        assert!(
            matches!(&message.content[2], ContentBlock::ToolCall { name, arguments, thought_signature: Some(s), .. } if name == "ls" && arguments["path"] == "." && s == "Q0ND")
        );
        assert_eq!((message.usage.input, message.usage.output, message.usage.cache_read), (40, 12, 10));
    }
}
