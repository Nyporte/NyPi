//! OpenAI Chat Completions adapter. Works with any OpenAI-compatible server through `base_url`
//! (OpenAI, OpenRouter, DeepSeek, Groq, Ollama, vLLM, llama.cpp, ...).

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::event::{EventSender, MessageBuilder};
use crate::models::Model;
use crate::provider::{Chunk, RequestFailure, StreamOptions, next_chunk, post_json};
use crate::sse::SseParser;
use crate::transform::prepare_for_model;
use crate::types::{ContentBlock, Context, Message, StopReason, text_of};

/// Fields different servers use for streamed reasoning text. The field a block arrived in is
/// stored as its signature so replay sends it back the same way.
const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

pub(crate) async fn run(model: Model, ctx: Context, opts: StreamOptions, tx: EventSender) {
    let mut builder = MessageBuilder::start(&model, tx);
    let mut headers: Vec<(String, String)> = model.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    match &opts.api_key {
        Some(key) => headers.push(("authorization".into(), format!("Bearer {key}"))),
        // Local servers (Ollama, llama.cpp) usually need no key.
        None if !is_local(&model.base_url) => {
            builder.finish(StopReason::Error, Some(format!("No API key for provider \"{}\"", model.provider)));
            return;
        }
        None => {}
    }
    let body = build_request(&model, &ctx, &opts);
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
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
            if event.data.trim() == "[DONE]" {
                return state.finish(builder);
            }
            let Ok(chunk) = serde_json::from_str::<Value>(&event.data) else { continue };
            if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
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

fn is_local(base_url: &str) -> bool {
    ["://localhost", "://127.0.0.1", "://[::1]", "://0.0.0.0"].iter().any(|host| base_url.contains(host))
}

#[derive(Default)]
struct StreamState {
    /// Stream tool index -> builder content index.
    tools: HashMap<u64, usize>,
    finish_reason: Option<String>,
    received_any: bool,
}

impl StreamState {
    fn apply(&mut self, builder: &mut MessageBuilder, chunk: &Value) {
        if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
            let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
            let cached = usage["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .or_else(|| usage["prompt_cache_hit_tokens"].as_u64())
                .unwrap_or(0);
            let out = builder.usage_mut();
            out.input = prompt.saturating_sub(cached);
            out.cache_read = cached;
            out.output = usage["completion_tokens"].as_u64().unwrap_or(0);
            out.total_tokens = usage["total_tokens"].as_u64().unwrap_or(prompt + out.output);
        }
        let Some(choice) = chunk["choices"].get(0) else { return };
        self.received_any = true;
        let delta = &choice["delta"];
        for field in REASONING_FIELDS {
            if let Some(text) = delta[field].as_str().filter(|t| !t.is_empty()) {
                if !builder.last_is_thinking() {
                    builder.begin_thinking(false, Some(field.to_string()));
                }
                builder.thinking(text);
                // Some servers mirror the same text in two fields; take the first only.
                break;
            }
        }
        if let Some(text) = delta["content"].as_str() {
            builder.text(text);
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let stream_index = call["index"].as_u64().unwrap_or(0);
                let id = call["id"].as_str();
                let name = call["function"]["name"].as_str();
                let index = match self.tools.get(&stream_index) {
                    Some(index) => {
                        builder.tool_call_identity(*index, id, name);
                        *index
                    }
                    None => {
                        let fallback_id = format!("call_{}", uuid::Uuid::new_v4().simple());
                        let index = builder.tool_call_start(id.unwrap_or(&fallback_id), name.unwrap_or_default());
                        self.tools.insert(stream_index, index);
                        index
                    }
                };
                if let Some(arguments) = call["function"]["arguments"].as_str() {
                    builder.tool_call_delta(index, arguments);
                }
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
    }

    fn finish(self, builder: MessageBuilder) {
        let (stop, error) = match self.finish_reason.as_deref() {
            Some("length") => (StopReason::Length, None),
            Some("tool_calls") | Some("function_call") => (StopReason::ToolUse, None),
            Some("content_filter") => (StopReason::Error, Some("Response blocked by content filter".into())),
            Some(_) if builder.has_tool_calls() => (StopReason::ToolUse, None),
            Some(_) => (StopReason::Stop, None),
            None if builder.has_tool_calls() => (StopReason::ToolUse, None),
            None if self.received_any => (StopReason::Stop, None),
            None => (StopReason::Error, Some("Stream ended without a response".into())),
        };
        builder.finish(stop, error);
    }
}

pub(crate) fn build_request(model: &Model, ctx: &Context, opts: &StreamOptions) -> Value {
    let ctx = &crate::transcript::prepare(ctx, model.compat.supports_mid_convo_system_messages);
    let compat = &model.compat;
    let mut messages = Vec::new();
    if !ctx.system_prompt.is_empty() {
        let role = if model.reasoning && compat.supports_developer_role { "developer" } else { "system" };
        messages.push(json!({ "role": role, "content": ctx.system_prompt }));
    }
    messages.extend(convert_messages(&prepare_for_model(&ctx.messages, model), model));

    let mut body = json!({ "model": model.id, "messages": messages, "stream": true });
    if compat.supports_usage_in_streaming != Some(false) {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if let Some(max_tokens) = opts.max_tokens {
        let field = compat.max_tokens_field.as_deref().unwrap_or("max_completion_tokens");
        body[field] = json!(max_tokens);
    }
    if !ctx.tools.is_empty() {
        body["tools"] = ctx
            .tools
            .iter()
            .map(|t| {
                json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.parameters } })
            })
            .collect();
    }
    if let Some(temperature) = opts.temperature {
        body["temperature"] = json!(temperature);
    }
    apply_reasoning(&mut body, model, opts);
    body
}

/// Request reasoning in the dialect the server understands (Pi's `thinkingFormat`).
fn apply_reasoning(body: &mut Value, model: &Model, opts: &StreamOptions) {
    if !model.reasoning {
        return;
    }
    let effort = model.native_effort(opts.thinking);
    let on = effort.is_some();
    let off_value = model.thinking_level_map.get("off").cloned().unwrap_or(Some("none".into()));
    let compat = &model.compat;
    let set_effort = |body: &mut Value| {
        if let Some(effort) = &effort
            && compat.supports_reasoning_effort
        {
            body["reasoning_effort"] = json!(effort);
        }
    };
    match compat.thinking_format.as_deref() {
        Some("openrouter") => {
            if let Some(effort) = &effort {
                body["reasoning"] = json!({ "effort": effort });
            } else if let Some(off) = off_value {
                body["reasoning"] = json!({ "effort": off });
            }
        }
        Some("deepseek") | Some("zai") => {
            body["thinking"] = json!({ "type": if on { "enabled" } else { "disabled" } });
            set_effort(body);
        }
        Some("qwen") => {
            body["enable_thinking"] = json!(on);
            set_effort(body);
        }
        Some("together") => {
            body["reasoning"] = json!({ "enabled": on });
            set_effort(body);
        }
        _ => {
            if on {
                set_effort(body);
            } else if compat.supports_reasoning_effort
                && let Some(off) = off_value
            {
                body["reasoning_effort"] = json!(off);
            }
        }
    }
}

fn convert_messages(messages: &[Message], model: &Model) -> Vec<Value> {
    let mut out = Vec::new();
    // OpenAI tool messages cannot carry images; they follow the tool batch as a user message.
    let mut pending_images: Vec<Value> = Vec::new();
    let flush_images = |out: &mut Vec<Value>, images: &mut Vec<Value>| {
        if !images.is_empty() {
            let mut content = vec![json!({ "type": "text", "text": "Images from the tool results above:" })];
            content.append(images);
            out.push(json!({ "role": "user", "content": content }));
        }
    };
    for message in messages {
        match message {
            Message::System(system) => {
                flush_images(&mut out, &mut pending_images);
                let text = crate::transcript::render_update(system);
                if !text.is_empty() {
                    let role =
                        if model.reasoning && model.compat.supports_developer_role { "developer" } else { "system" };
                    out.push(json!({ "role": role, "content": text }));
                }
            }
            Message::User(user) => {
                flush_images(&mut out, &mut pending_images);
                out.push(json!({ "role": "user", "content": user_content(&user.content) }));
            }
            Message::Assistant(assistant) => {
                flush_images(&mut out, &mut pending_images);
                let text = text_of(&assistant.content);
                let mut entry =
                    json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
                let mut reasoning: HashMap<&str, String> = HashMap::new();
                for block in &assistant.content {
                    if let ContentBlock::Thinking { thinking, signature: Some(field), .. } = block
                        && let Some(field) = REASONING_FIELDS.iter().find(|f| **f == field)
                    {
                        let text = reasoning.entry(field).or_default();
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(thinking);
                    }
                }
                for (field, text) in reasoning {
                    entry[field] = json!(text);
                }
                if model.compat.requires_reasoning_content_on_assistant_messages
                    && entry.get("reasoning_content").is_none()
                {
                    entry["reasoning_content"] = json!("");
                }
                let calls: Vec<Value> = assistant
                    .tool_calls()
                    .into_iter()
                    .map(|call| {
                        json!({ "id": call.id, "type": "function", "function": { "name": call.name, "arguments": call.arguments.to_string() } })
                    })
                    .collect();
                let has_calls = !calls.is_empty();
                if has_calls {
                    entry["tool_calls"] = Value::Array(calls);
                }
                if !text.is_empty() || has_calls {
                    out.push(entry);
                }
            }
            Message::ToolResult(result) => {
                let mut text = text_of(&result.content);
                if text.is_empty() {
                    text = "(empty result)".into();
                }
                out.push(json!({ "role": "tool", "tool_call_id": result.tool_call_id, "content": text }));
                for block in &result.content {
                    if let ContentBlock::Image { data, mime_type } = block {
                        pending_images.push(image_part(data, mime_type));
                    }
                }
            }
        }
    }
    flush_images(&mut out, &mut pending_images);
    out
}

fn user_content(blocks: &[ContentBlock]) -> Value {
    if blocks.iter().all(|b| matches!(b, ContentBlock::Text { .. })) {
        return json!(text_of(blocks));
    }
    Value::Array(
        blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(json!({ "type": "text", "text": text })),
                ContentBlock::Image { data, mime_type } => Some(image_part(data, mime_type)),
                _ => None,
            })
            .collect(),
    )
}

fn image_part(data: &str, mime_type: &str) -> Value {
    json!({ "type": "image_url", "image_url": { "url": format!("data:{mime_type};base64,{data}") } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ModelRegistry, ThinkingLevel};

    #[test]
    fn deepseek_reasoning_is_requested_and_replayed() {
        let registry = ModelRegistry::builtin();
        let model = registry.all().iter().find(|m| m.provider == "deepseek" && m.reasoning).unwrap().clone();
        let mut assistant = crate::types::AssistantMessage::empty("openai-completions", &model.provider, &model.id);
        assistant.content = vec![
            ContentBlock::Thinking {
                thinking: "plan".into(),
                signature: Some("reasoning_content".into()),
                redacted: false,
            },
            ContentBlock::text("answer"),
        ];
        let ctx = Context {
            system_prompt: "sys".into(),
            messages: vec![Message::user_text("q"), Message::Assistant(assistant), Message::user_text("q2")],
            tools: vec![],
        };
        let opts = StreamOptions { thinking: ThinkingLevel::High, ..Default::default() };
        let body = build_request(&model, &ctx, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["messages"][2]["reasoning_content"], "plan");
        assert_eq!(body["messages"][2]["content"], "answer");
    }

    #[test]
    fn prompt_updates_go_in_place_only_when_the_model_takes_them() {
        use crate::types::{Sections, SystemMessage};
        let registry = ModelRegistry::builtin();
        let mut model = registry.all().iter().find(|m| m.api == crate::models::Api::OpenAiCompletions).unwrap().clone();
        let section = |name: &str, text: &str| Sections(vec![(name.to_string(), Some(text.to_string()))]);
        let ctx = Context {
            system_prompt: String::new(),
            messages: vec![
                Message::System(SystemMessage {
                    sections: section("goals", "<goals>a</goals>"),
                    timestamp: 1,
                    ..Default::default()
                }),
                Message::user_text("q"),
                Message::System(SystemMessage {
                    sections: section("goals", "<goals>b</goals>"),
                    timestamp: 2,
                    ..Default::default()
                }),
                Message::user_text("q2"),
            ],
            tools: vec![],
        };
        let opts = StreamOptions::default();
        model.compat.supports_mid_convo_system_messages = false;
        let collapsed = build_request(&model, &ctx, &opts);
        assert_eq!(collapsed["messages"][0]["content"], "<goals>b</goals>");
        assert_eq!(collapsed["messages"].as_array().unwrap().len(), 3, "one leading prompt, then the turns");

        model.compat.supports_mid_convo_system_messages = true;
        let in_place = build_request(&model, &ctx, &opts);
        assert_eq!(in_place["messages"][0]["content"], "<goals>a</goals>", "the head stays as cached");
        assert_eq!(in_place["messages"][2]["content"], "Updated system prompt section \"goals\":\n\n<goals>b</goals>");
        assert_eq!(in_place["messages"][3]["role"], "user");
    }
}
