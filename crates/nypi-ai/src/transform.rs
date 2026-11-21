//! Make a transcript safe to replay to a given model (ported from Pi's transform-messages.ts).

use std::collections::HashSet;

use crate::models::Model;
use crate::types::{ContentBlock, Message, StopReason, ToolResultMessage, now_ms};

const NON_VISION_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";

/// Rules, each needed to avoid provider rejections or misleading the model:
/// - errored or aborted assistant messages are dropped: they are incomplete turns;
/// - thinking from a different model is dropped: signatures only verify for their producer;
/// - images become placeholders for non-vision models;
/// - tool calls without results get synthetic error results, since providers require pairs.
pub fn prepare_for_model(messages: &[Message], model: &Model) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut answered: HashSet<String> = HashSet::new();

    for message in messages {
        match message {
            Message::Assistant(assistant) => {
                close_pending(&mut out, &mut pending, &mut answered);
                if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted) {
                    continue;
                }
                let same_model = assistant.provider == model.provider
                    && assistant.api == model.api.as_str()
                    && assistant.model == model.id;
                let mut assistant = assistant.clone();
                assistant.content.retain(|block| match block {
                    ContentBlock::Thinking { thinking, signature, redacted } => {
                        same_model && (*redacted || signature.is_some() || !thinking.trim().is_empty())
                    }
                    _ => true,
                });
                for block in &assistant.content {
                    if let ContentBlock::ToolCall { id, name, .. } = block {
                        pending.push((id.clone(), name.clone()));
                    }
                }
                out.push(Message::Assistant(assistant));
            }
            Message::ToolResult(result) => {
                answered.insert(result.tool_call_id.clone());
                let mut result = result.clone();
                downgrade_images(&mut result.content, model);
                out.push(Message::ToolResult(result));
            }
            Message::System(system) => out.push(Message::System(system.clone())),
            Message::User(user) => {
                close_pending(&mut out, &mut pending, &mut answered);
                let mut user = user.clone();
                downgrade_images(&mut user.content, model);
                out.push(Message::User(user));
            }
        }
    }
    close_pending(&mut out, &mut pending, &mut answered);
    out
}

fn close_pending(out: &mut Vec<Message>, pending: &mut Vec<(String, String)>, answered: &mut HashSet<String>) {
    for (id, name) in pending.drain(..) {
        if !answered.contains(&id) {
            out.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: id,
                tool_name: name,
                content: vec![ContentBlock::text("No result provided")],
                details: None,
                is_error: true,
                timestamp: now_ms(),
                duration_ms: None,
            }));
        }
    }
    answered.clear();
}

fn downgrade_images(content: &mut [ContentBlock], model: &Model) {
    if model.supports_images() {
        return;
    }
    for block in content.iter_mut() {
        if matches!(block, ContentBlock::Image { .. }) {
            *block = ContentBlock::text(NON_VISION_IMAGE_PLACEHOLDER);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ModelRegistry;
    use crate::types::AssistantMessage;
    use serde_json::json;

    #[test]
    fn drops_failed_turns_and_closes_orphaned_calls() {
        let model = ModelRegistry::builtin().find("anthropic/claude-sonnet-5-5").unwrap().clone();
        let mut failed = AssistantMessage::empty("anthropic-messages", "anthropic", &model.id);
        failed.stop_reason = StopReason::Error;
        let mut calling = AssistantMessage::empty("anthropic-messages", "other", "x");
        calling.stop_reason = StopReason::ToolUse;
        calling.content = vec![
            ContentBlock::Thinking { thinking: "secret".into(), signature: Some("s".into()), redacted: false },
            ContentBlock::ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: json!({}),
                arguments_error: None,
                thought_signature: None,
            },
        ];
        let messages = vec![
            Message::user_text("a"),
            Message::Assistant(failed),
            Message::Assistant(calling),
            Message::user_text("b"),
        ];
        let out = prepare_for_model(&messages, &model);
        let roles: Vec<&str> = out.iter().map(Message::role).collect();
        assert_eq!(roles, ["user", "assistant", "toolResult", "user"]);
        let Message::Assistant(kept) = &out[1] else { panic!() };
        assert_eq!(kept.content.len(), 1, "foreign thinking dropped");
    }
}
