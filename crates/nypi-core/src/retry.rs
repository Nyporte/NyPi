//! Retries of transient provider errors as Pi's `AgentSession` does them (decisions D42, D43):
//! the run-level rules the runtime and the chat module share, and the notices front ends show.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use nypi_agent::{Agent, AgentEvent};
use nypi_ai::retry::{RetryPolicy, RetryProgress, is_retryable_assistant_error};
use nypi_ai::{AssistantMessage, Message, StopReason};
use serde::{Deserialize, Serialize};

/// The final error of retries an abort ended, as Pi words it.
pub const RETRY_CANCELLED: &str = "Retry cancelled";

/// A retry as front ends and the audit log see it. Serialized with Pi's session event names
/// and fields, so RPC sends it as it is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum RetryNotice {
    /// A reply failed with a transient error; the run continues after `delay_ms`.
    AutoRetryStart { attempt: u32, max_attempts: u32, delay_ms: u64, error_message: String },
    /// The retries are over: a reply came through, they ran out, or an abort ended them.
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_error: Option<String>,
    },
    /// The compaction summary failed with a transient error and is retried after `delay_ms`.
    SummarizationRetryScheduled { attempt: u32, max_attempts: u32, delay_ms: u64, error_message: String },
    /// The wait is over and the summary is requested again; `reason` is the compaction's.
    SummarizationRetryAttemptStart { source: String, reason: String },
    /// The summary's retries are over; the compaction's own end says how it went.
    SummarizationRetryFinished,
}

impl RetryNotice {
    /// The notice for a step of a compaction summary's retries.
    pub fn summarization(progress: RetryProgress, reason: &str) -> Self {
        match progress {
            RetryProgress::Scheduled { attempt, max_attempts, delay_ms, error_message } => {
                RetryNotice::SummarizationRetryScheduled { attempt, max_attempts, delay_ms, error_message }
            }
            RetryProgress::AttemptStart => {
                RetryNotice::SummarizationRetryAttemptStart { source: "compaction".into(), reason: reason.into() }
            }
            RetryProgress::Finished { .. } => RetryNotice::SummarizationRetryFinished,
        }
    }
}

/// What to do after a run.
pub enum Next {
    /// Wait this long, drop the failed reply, and continue the run.
    Retry {
        delay_ms: u64,
    },
    Stop,
}

/// Retries so far of the reply that keeps failing; 0 when none is being retried. Cloned into
/// event forwarders, so a reply that comes through ends the retries at once.
#[derive(Clone, Default)]
pub struct RetryState(Arc<AtomicU32>);

impl RetryState {
    /// The first reply that comes through ends the retries, even in the middle of a run.
    pub fn observe(&self, event: &AgentEvent) -> Option<RetryNotice> {
        match event {
            AgentEvent::MessageEnd { message: Message::Assistant(reply) } if came_through(reply) => {
                self.end(true, None)
            }
            _ => None,
        }
    }

    /// Decide after a run that added `batch`: retry its failed reply, or end the retries.
    pub fn after_run(&self, policy: &RetryPolicy, batch: &[Message], mut notify: impl FnMut(RetryNotice)) -> Next {
        // For callers without a forwarder that observes events.
        if batch.iter().any(|m| matches!(m, Message::Assistant(a) if came_through(a)))
            && let Some(notice) = self.end(true, None)
        {
            notify(notice);
        }
        let Some(Message::Assistant(last)) = batch.last() else { return Next::Stop };
        let final_error = match last.stop_reason {
            StopReason::Aborted => Some(RETRY_CANCELLED.to_string()),
            StopReason::Error => {
                let attempt = self.0.load(Ordering::SeqCst) + 1;
                // A context overflow is compaction's to recover from, not a retry's.
                if attempt <= policy.budget()
                    && !nypi_ai::overflow::is_context_overflow(last, None)
                    && is_retryable_assistant_error(last)
                {
                    self.0.store(attempt, Ordering::SeqCst);
                    let delay_ms = policy.delay_ms(attempt);
                    notify(RetryNotice::AutoRetryStart {
                        attempt,
                        max_attempts: policy.max_retries,
                        delay_ms,
                        error_message: last.error_message.clone().unwrap_or_else(|| "Unknown error".into()),
                    });
                    return Next::Retry { delay_ms };
                }
                last.error_message.clone()
            }
            _ => None,
        };
        if let Some(notice) = self.end(false, final_error) {
            notify(notice);
        }
        Next::Stop
    }

    /// An abort ended the wait.
    pub fn cancelled(&self) -> Option<RetryNotice> {
        self.end(false, Some(RETRY_CANCELLED.to_string()))
    }

    fn end(&self, success: bool, final_error: Option<String>) -> Option<RetryNotice> {
        let attempt = self.0.swap(0, Ordering::SeqCst);
        (attempt > 0).then_some(RetryNotice::AutoRetryEnd { success, attempt, final_error })
    }
}

/// Drop the failed reply the transcript ends with, so the run continues from the same request.
/// The reply stays in the session file; providers never replay an errored reply (transform.rs).
pub fn drop_failed_reply(agent: &mut Agent) {
    if matches!(agent.messages.last(), Some(Message::Assistant(a)) if a.stop_reason == StopReason::Error) {
        agent.messages.pop();
    }
}

fn came_through(reply: &AssistantMessage) -> bool {
    !matches!(reply.stop_reason, StopReason::Error | StopReason::Aborted)
}
