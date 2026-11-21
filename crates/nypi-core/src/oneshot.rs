//! One tool-free model call that answers with JSON (reflection, portrait, Thinking): the reply
//! text, or the provider's error, never a panic. Transient errors are retried (decision D43).

use anyhow::bail;
use nypi_ai::retry::{RetryPolicy, retry_assistant_call};
use nypi_ai::{Context, Message, Model, StopReason, StreamFn, StreamOptions, ThinkingLevel};
use tokio_util::sync::CancellationToken;

#[allow(clippy::too_many_arguments)]
pub async fn complete(
    model: &Model,
    stream_fn: &StreamFn,
    api_key: Option<String>,
    system_prompt: &str,
    prompt: String,
    thinking: ThinkingLevel,
    retry: &RetryPolicy,
    cancel: CancellationToken,
) -> anyhow::Result<String> {
    let ctx =
        Context { system_prompt: system_prompt.into(), messages: vec![Message::user_text(prompt)], tools: vec![] };
    let opts = StreamOptions {
        api_key,
        // Reasoning tokens count against the limit, so a thinking call gets more room.
        max_tokens: Some(if thinking == ThinkingLevel::Off { 2000 } else { 16_000 }.min(model.max_tokens.max(1024))),
        thinking,
        cancel: cancel.clone(),
        ..Default::default()
    };
    // No retry notices: nobody waits on these calls, and the caller records a final failure.
    let message =
        retry_assistant_call(|| nypi_ai::complete(stream_fn, model, &ctx, &opts), retry, &cancel, |_| {}).await;
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        bail!("{}", message.error_message.unwrap_or_else(|| "unknown error".into()));
    }
    Ok(message.text())
}

/// The JSON object in a reply, fenced or not.
pub fn json_object(reply: &str) -> Option<&str> {
    let (start, end) = (reply.find('{')?, reply.rfind('}')?);
    (start <= end).then(|| &reply[start..=end])
}
