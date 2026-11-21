//! Context compaction (Pi's algorithm): when the context nears the window, summarize older
//! entries into a structured checkpoint and keep about `keep_recent_tokens` verbatim.

use std::collections::BTreeSet;

use anyhow::bail;
use nypi_ai::retry::{RetryPolicy, RetryProgress, retry_assistant_call};
use nypi_ai::{ContentBlock, Context, Message, Model, StopReason, StreamFn, StreamOptions, ThinkingLevel, text_of};
use tokio_util::sync::CancellationToken;

use crate::config::CompactionSettings;
use crate::session::{Entry, SessionStore, entry_message};

/// Rough token estimate (chars / 4, as in Pi); images count as ~1200 tokens.
pub fn estimate_tokens(message: &Message) -> u64 {
    if let Message::System(system) = message {
        let tools: usize = system
            .tools_added
            .iter()
            .map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len())
            .sum();
        return ((nypi_ai::transcript::render(system).len() + tools) / 4) as u64;
    }
    let content: &[ContentBlock] = match message {
        Message::System(_) => unreachable!(),
        Message::User(m) => &m.content,
        Message::Assistant(m) => &m.content,
        Message::ToolResult(m) => &m.content,
    };
    let chars: usize = content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => text.len(),
            ContentBlock::Thinking { thinking, .. } => thinking.len(),
            ContentBlock::ToolCall { name, arguments, .. } => name.len() + arguments.to_string().len(),
            ContentBlock::Image { .. } => 4800,
        })
        .sum();
    (chars as u64).div_ceil(4)
}

/// Tokens the next request will occupy: the last successful response's reported usage plus
/// an estimate for messages appended after it. Usage reported before a compaction describes
/// the old, larger context, so it is only trusted from responses newer than the summary.
pub fn context_tokens(messages: &[Message]) -> u64 {
    let summary_time = messages.iter().find_map(|m| match m {
        Message::User(user) if crate::session::is_summary_message(m) => Some(user.timestamp),
        _ => None,
    });
    let last_usage = messages.iter().rposition(|m| {
        matches!(m, Message::Assistant(a)
            if !matches!(a.stop_reason, StopReason::Error | StopReason::Aborted)
                && a.usage.context_tokens() > 0
                && summary_time.is_none_or(|t| a.timestamp > t))
    });
    match last_usage {
        Some(index) => {
            let Message::Assistant(assistant) = &messages[index] else { unreachable!() };
            assistant.usage.context_tokens() + messages[index + 1..].iter().map(estimate_tokens).sum::<u64>()
        }
        None => messages.iter().map(estimate_tokens).sum(),
    }
}

pub fn should_compact(tokens: u64, model: &Model, settings: &CompactionSettings) -> bool {
    settings.enabled
        && model.context_window > 0
        && tokens > model.context_window.saturating_sub(settings.reserve_tokens)
}

/// What a compaction will summarize and keep.
pub struct CompactionPlan {
    pub to_summarize: Vec<Message>,
    pub previous_summary: Option<String>,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub read_files: BTreeSet<String>,
    pub modified_files: BTreeSet<String>,
}

/// Choose the cut point on the active branch. Returns None when there is nothing to summarize.
pub fn plan(store: &SessionStore, keep_recent_tokens: u64) -> Option<CompactionPlan> {
    let branch = store.branch();
    let edits = store.edits();
    let previous = branch.iter().rposition(|e| matches!(e, Entry::Compaction { .. }));
    let (start, previous_summary) = match previous {
        Some(index) => {
            let Entry::Compaction { summary, first_kept_entry_id, .. } = branch[index] else { unreachable!() };
            let kept = branch[..index].iter().position(|e| e.id() == first_kept_entry_id).unwrap_or(index + 1);
            (kept, Some(summary.clone()))
        }
        None => (0, None),
    };
    let candidates: Vec<(usize, &Entry)> = branch
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(_, e)| !matches!(e, Entry::Compaction { .. }))
        .map(|(i, e)| (i, *e))
        .collect();
    // Cut only before a user or assistant message, never between a tool call and its result.
    let is_cut_point =
        |e: &Entry| matches!(e, Entry::Message { message: Message::User(_) | Message::Assistant(_), .. });

    let mut accumulated = 0;
    let mut cut: Option<usize> = None;
    for (position, (_, entry)) in candidates.iter().enumerate().rev() {
        let Some(message) = entry_message(entry, &edits) else { continue };
        accumulated += estimate_tokens(&message);
        if accumulated >= keep_recent_tokens {
            cut = candidates[position..].iter().position(|(_, e)| is_cut_point(e)).map(|offset| position + offset);
            break;
        }
    }
    let cut = cut?;
    if cut == 0 {
        return None;
    }
    let to_summarize: Vec<Message> = candidates[..cut].iter().filter_map(|(_, e)| entry_message(e, &edits)).collect();
    if to_summarize.is_empty() {
        return None;
    }
    let (read_files, modified_files) = file_operations(&to_summarize);
    Some(CompactionPlan {
        tokens_before: context_tokens(&store.context().messages),
        first_kept_entry_id: candidates[cut].1.id().to_string(),
        to_summarize,
        previous_summary,
        read_files,
        modified_files,
    })
}

/// Generate the checkpoint summary with `model`.
#[allow(clippy::too_many_arguments)]
pub async fn summarize(
    plan: &CompactionPlan,
    model: &Model,
    stream_fn: &StreamFn,
    api_key: Option<String>,
    reserve_tokens: u64,
    retry: &RetryPolicy,
    cancel: CancellationToken,
    progress: impl FnMut(RetryProgress),
) -> anyhow::Result<String> {
    let mut prompt = format!("<conversation>\n{}\n</conversation>\n\n", serialize_conversation(&plan.to_summarize));
    match &plan.previous_summary {
        Some(previous) => {
            prompt.push_str(&format!(
                "<previous-summary>\n{previous}\n</previous-summary>\n\n{UPDATE_SUMMARIZATION_PROMPT}"
            ));
        }
        None => prompt.push_str(SUMMARIZATION_PROMPT),
    }
    let ctx = Context {
        system_prompt: SUMMARIZATION_SYSTEM_PROMPT.into(),
        messages: vec![Message::user_text(prompt)],
        tools: vec![],
    };
    let opts = StreamOptions {
        api_key,
        max_tokens: Some((reserve_tokens * 8 / 10).min(model.max_tokens.max(1024))),
        thinking: ThinkingLevel::Off,
        cancel: cancel.clone(),
        ..Default::default()
    };
    // Only the model call is retried (Pi's completeSummarization); a bad summary is not.
    let message =
        retry_assistant_call(|| nypi_ai::complete(stream_fn, model, &ctx, &opts), retry, &cancel, progress).await;
    match message.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            bail!("Compaction failed: {}", message.error_message.unwrap_or_else(|| "unknown error".into()))
        }
        StopReason::Length => bail!("Compaction failed: the summary hit the token cap and is incomplete"),
        _ => {}
    }
    let mut summary = message.text().trim().to_string();
    if summary.is_empty() {
        bail!("Compaction failed: the model returned an empty summary");
    }
    summary.push_str(&format_file_operations(&plan.read_files, &plan.modified_files));
    Ok(summary)
}

const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Plain-text transcript for the summarizer (Pi's serializeConversation).
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message {
            // The prompt is not part of the conversation being summarized.
            Message::System(_) => {}
            Message::User(user) => {
                let text = text_of(&user.content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let text = assistant.text();
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {text}"));
                }
                let calls: Vec<String> = assistant
                    .tool_calls()
                    .iter()
                    .map(|call| {
                        let args = call
                            .arguments
                            .as_object()
                            .map(|o| o.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "))
                            .unwrap_or_default();
                        format!("{}({args})", call.name)
                    })
                    .collect();
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let text = text_of(&result.content);
                if !text.is_empty() {
                    let shown = crate::tools::truncate::truncate_line(&text, TOOL_RESULT_MAX_CHARS);
                    parts.push(format!("[Tool result]: {shown}"));
                }
            }
        }
    }
    parts.join("\n\n")
}

fn file_operations(messages: &[Message]) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut read = BTreeSet::new();
    let mut modified = BTreeSet::new();
    for message in messages {
        if let Message::Assistant(assistant) = message {
            for call in assistant.tool_calls() {
                let Some(path) = call.arguments["path"].as_str() else { continue };
                match call.name.as_str() {
                    "read" => {
                        read.insert(path.to_string());
                    }
                    "edit" | "write" => {
                        modified.insert(path.to_string());
                    }
                    _ => {}
                }
            }
        }
    }
    read.retain(|p| !modified.contains(p));
    (read, modified)
}

fn format_file_operations(read: &BTreeSet<String>, modified: &BTreeSet<String>) -> String {
    let mut sections = Vec::new();
    if !read.is_empty() {
        sections.push(format!("<read-files>\n{}\n</read-files>", read.iter().cloned().collect::<Vec<_>>().join("\n")));
    }
    if !modified.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified.iter().cloned().collect::<Vec<_>>().join("\n")
        ));
    }
    if sections.is_empty() { String::new() } else { format!("\n\n{}", sections.join("\n\n")) }
}

const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

#[cfg(test)]
mod tests {
    use nypi_ai::AssistantMessage;

    use super::*;

    #[test]
    fn usage_from_before_a_compaction_is_ignored() {
        let mut old = AssistantMessage::empty("faux", "faux", "m");
        old.usage.total_tokens = 50_000;
        old.timestamp = 1;
        old.content = vec![ContentBlock::text("x".repeat(40))];
        let summary = crate::session::summary_message("short");
        let messages = vec![summary.clone(), Message::Assistant(old.clone())];
        assert!(context_tokens(&messages) < 100);
        let mut fresh = old;
        fresh.timestamp = i64::MAX;
        fresh.usage.total_tokens = 1234;
        assert_eq!(context_tokens(&[summary, Message::Assistant(fresh)]), 1234);
    }

    #[test]
    fn plan_keeps_recent_messages_and_cuts_at_a_turn_boundary() {
        let mut store = SessionStore::ephemeral(std::path::Path::new("/w"));
        for i in 0..10 {
            store.append_message(Message::user_text(format!("{i} {}", "x".repeat(400)))).unwrap();
        }
        let plan = plan(&store, 250).expect("something to summarize");
        // Each message is ~101 tokens, so the last three are kept.
        assert_eq!(plan.to_summarize.len(), 7);
        let ctx_ids: Vec<&str> = store.branch().iter().map(|e| e.id()).collect();
        assert_eq!(plan.first_kept_entry_id, ctx_ids[7]);
    }
}
