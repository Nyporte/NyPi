//! Which failed replies are worth retrying, and how long to wait first (Pi's `utils/retry.ts`).
//! The patterns are Pi's, plus the wording of NyPi's own HTTP stack, which differs from Node's.

use std::future::Future;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::types::{AssistantMessage, StopReason};

/// Cap on one agent-level backoff, as in Pi.
pub const DEFAULT_MAX_AGENT_RETRY_DELAY_MS: u64 = 60_000;

/// Retry budget and backoff: Pi's `RetryPolicy`, read from the `retry` settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetryPolicy {
    pub enabled: bool,
    /// Retries after the first attempt; 0 never retries.
    pub max_retries: u32,
    /// The first wait; each later one doubles it (2 s, 4 s, 8 s).
    pub base_delay_ms: u64,
    /// No single wait is longer than this.
    pub max_agent_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2000,
            max_agent_delay_ms: DEFAULT_MAX_AGENT_RETRY_DELAY_MS,
        }
    }
}

impl RetryPolicy {
    /// The wait before retry `attempt` (1-based): `base * 2^(attempt-1)`, capped.
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        let factor = 1u64.checked_shl(attempt.saturating_sub(1)).unwrap_or(u64::MAX);
        self.base_delay_ms.saturating_mul(factor).min(self.max_agent_delay_ms)
    }

    /// Retries allowed: none when retrying is off.
    pub fn budget(&self) -> u32 {
        if self.enabled { self.max_retries } else { 0 }
    }
}

/// What [`retry_assistant_call`] reports around each retry (Pi's `RetryCallbacks`).
#[derive(Clone, Debug, PartialEq)]
pub enum RetryProgress {
    /// Before the wait for retry `attempt` (1-based).
    Scheduled { attempt: u32, max_attempts: u32, delay_ms: u64, error_message: String },
    /// The wait is over; the retried call starts.
    AttemptStart,
    /// Once, when the call was retried: whether a later attempt came through.
    Finished { success: bool, attempt: u32, final_error: Option<String> },
}

/// Wait `delay_ms`, or until `abort` is cancelled. False when aborted.
pub async fn back_off(delay_ms: u64, abort: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        _ = abort.cancelled() => false,
        _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => true,
    }
}

/// Make one model call, retrying transient errors (Pi's `retryAssistantCall`). A reply that
/// comes through, an abort, a deterministic error, or the last allowed failure is returned as
/// it is. An abort during a wait returns the failed reply as aborted, so callers see one shape.
pub async fn retry_assistant_call<F, Fut>(
    mut produce: F,
    policy: &RetryPolicy,
    abort: &CancellationToken,
    mut progress: impl FnMut(RetryProgress),
) -> AssistantMessage
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AssistantMessage>,
{
    let mut attempt = 0;
    loop {
        let response = produce().await;
        let failed = match response.stop_reason {
            StopReason::Aborted => Some(None),
            StopReason::Error if attempt < policy.budget() && is_retryable_assistant_error(&response) => None,
            StopReason::Error => Some(response.error_message.clone()),
            _ => {
                if attempt > 0 {
                    progress(RetryProgress::Finished { success: true, attempt, final_error: None });
                }
                return response;
            }
        };
        if let Some(final_error) = failed {
            if attempt > 0 {
                progress(RetryProgress::Finished { success: false, attempt, final_error });
            }
            return response;
        }
        attempt += 1;
        let error_message = response.error_message.clone().unwrap_or_else(|| "Unknown error".into());
        let delay_ms = policy.delay_ms(attempt);
        progress(RetryProgress::Scheduled {
            attempt,
            max_attempts: policy.max_retries,
            delay_ms,
            error_message: error_message.clone(),
        });
        if !back_off(delay_ms, abort).await {
            progress(RetryProgress::Finished { success: false, attempt, final_error: Some(error_message) });
            return AssistantMessage {
                stop_reason: StopReason::Aborted,
                error_message: Some("Request aborted".into()),
                ..response
            };
        }
        progress(RetryProgress::AttemptStart);
    }
}

/// Subscription and quota limits: they arrive as 429s, but waiting seconds does not lift them.
const NON_RETRYABLE: &[&str] = &[
    "GoUsageLimitError",
    "FreeUsageLimitError",
    "Monthly usage limit reached",
    "available balance",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "billing",
    "subscription_sharing_usage_limit_exceeded",
    "subscription_sharing_workplace_individual_usage_limit_exceeded",
    // Zhipu/Z.ai plan limit (code 1308), which resets after hours.
    r#""code"\s*:\s*"?1308(?:[^0-9]|$)"#,
];

const RETRYABLE: &[&str] = &[
    // Provider load, HTTP status, and server-side failures.
    "overloaded",
    "server_busy",
    "servers are currently busy",
    "currently experiencing high demand",
    "model is at capacity",
    "rate.?limit",
    "too many requests",
    // A status code, not part of a longer word or number such as a request id. The regex crate
    // has no lookaround, so the boundaries are matched as characters.
    "(?:^|[^a-z0-9])(?:429|500|502|503|504|520|524)(?:[^a-z0-9]|$)",
    "service.?(?:is.?)?(?:temporarily.?)?unavailable",
    "server.?error",
    "internal.?error",
    // Gateways relaying an upstream failure.
    "provider.?returned.?(?:an.?)?error",
    "exceeded request buffer limit while retrying upstream",
    "unable to forward upstream",
    "upstream.?request.?failed",
    "upstream_stream_read_error",
    "stream was interrupted",
    "http/?2 stream failed",
    // Network, proxy, and transport failures.
    "network.?error",
    "connection.?error",
    "connection.?refused",
    "connection.?lost",
    "other side closed",
    "fetch failed",
    r"request to \S+ failed, reason:",
    "getaddrinfo",
    "ENOTFOUND",
    "EAI_AGAIN",
    "upstream.?connect",
    "reset before headers",
    "socket hang up",
    "socket connection was closed",
    "timed? out",
    "timeout",
    "terminated",
    "websocket.?closed",
    "websocket.?error",
    // Streams that end early.
    "ended without",
    "stream ended before message_stop",
    "stream ended before a terminal response event",
    "http2 request did not get a response",
    "stream disconnected before completion",
    "pending stream has been canceled",
    "retry delay",
    // Explicit retry guidance from the provider.
    "you can retry your request",
    "try your request again",
    "please retry your request",
    "ResourceExhausted",
    "subscription_sharing_usage_unavailable",
    "subscription_sharing_user_unavailable",
    // NyPi's HTTP stack (reqwest and hyper): a request that got no response, a body that broke
    // off, and the provider layer's own stall timeout.
    "error sending request",
    "error decoding response body",
    "error reading a body from connection",
    "connection closed before message completed",
    "connection reset",
    "broken pipe",
    "unexpected eof",
    "stream error",
    "stream stalled",
];

fn pattern(cell: &'static OnceLock<Regex>, alternatives: &[&str]) -> &'static Regex {
    cell.get_or_init(|| Regex::new(&format!("(?i){}", alternatives.join("|"))).expect("retry patterns compile"))
}

/// Whether `message` failed in a way that looks transient: a provider or transport error.
///
/// This is only the classifier. Callers handle context overflow first (compaction, not
/// retry), then apply their own budget and backoff.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    static NON: OnceLock<Regex> = OnceLock::new();
    static YES: OnceLock<Regex> = OnceLock::new();
    let Some(error) = message.error_message.as_deref().filter(|e| !e.is_empty()) else { return false };
    if message.stop_reason != StopReason::Error || pattern(&NON, NON_RETRYABLE).is_match(error) {
        return false;
    }
    pattern(&YES, RETRYABLE).is_match(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(error: &str) -> AssistantMessage {
        AssistantMessage {
            stop_reason: StopReason::Error,
            error_message: Some(error.into()),
            ..AssistantMessage::empty("faux", "faux", "faux-1")
        }
    }

    #[test]
    fn transient_failures_are_retryable() {
        for error in [
            "Request failed: error sending request for url (https://api.example.com/v1/chat/completions)",
            "Stream error: error decoding response body",
            "HTTP 503: upstream unavailable",
            "HTTP 429: Too Many Requests",
            "overloaded_error: Overloaded",
            "Stream stalled: no data for 300s",
            "fetch failed",
        ] {
            assert!(is_retryable_assistant_error(&failed(error)), "{error}");
        }
    }

    #[test]
    fn deterministic_failures_are_not() {
        for error in [
            "HTTP 400: invalid request",
            "HTTP 401: invalid api key",
            "HTTP 429: insufficient_quota",
            "HTTP 404: request id req_5003 not found",
            "No API key for provider \"x\"",
        ] {
            assert!(!is_retryable_assistant_error(&failed(error)), "{error}");
        }
        let aborted = AssistantMessage { stop_reason: StopReason::Aborted, ..failed("timeout") };
        assert!(!is_retryable_assistant_error(&aborted));
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let policy = RetryPolicy::default();
        let delays: Vec<u64> = (1..=6).map(|attempt| policy.delay_ms(attempt)).collect();
        assert_eq!(delays, [2000, 4000, 8000, 16_000, 32_000, 60_000]);
        assert_eq!(policy.delay_ms(200), 60_000);
    }

    fn scripted(replies: Vec<AssistantMessage>) -> impl FnMut() -> std::future::Ready<AssistantMessage> {
        let mut replies = replies.into_iter();
        move || std::future::ready(replies.next().expect("a scripted reply"))
    }

    fn quick(max_retries: u32) -> RetryPolicy {
        RetryPolicy { max_retries, base_delay_ms: 1, ..Default::default() }
    }

    #[tokio::test]
    async fn a_call_is_retried_until_it_comes_through() {
        let ok = AssistantMessage { stop_reason: StopReason::Stop, ..failed("") };
        let mut seen = Vec::new();
        let reply = retry_assistant_call(
            scripted(vec![failed("fetch failed"), ok.clone()]),
            &quick(3),
            &CancellationToken::new(),
            |p| seen.push(p),
        )
        .await;
        assert_eq!(reply, ok);
        assert!(matches!(seen[0], RetryProgress::Scheduled { attempt: 1, max_attempts: 3, delay_ms: 1, .. }));
        assert_eq!(
            seen[1..],
            [RetryProgress::AttemptStart, RetryProgress::Finished { success: true, attempt: 1, final_error: None }]
        );
    }

    #[tokio::test]
    async fn a_call_stops_at_the_budget_a_deterministic_error_or_an_abort() {
        let mut seen = Vec::new();
        let replies = vec![failed("HTTP 503: a"), failed("HTTP 503: b"), failed("HTTP 503: c")];
        let reply =
            retry_assistant_call(scripted(replies), &quick(2), &CancellationToken::new(), |p| seen.push(p)).await;
        assert_eq!(reply.error_message.as_deref(), Some("HTTP 503: c"));
        assert_eq!(
            seen.last(),
            Some(&RetryProgress::Finished { success: false, attempt: 2, final_error: Some("HTTP 503: c".into()) })
        );

        let mut seen = Vec::new();
        let reply =
            retry_assistant_call(scripted(vec![failed("HTTP 401: no")]), &quick(2), &CancellationToken::new(), |p| {
                seen.push(p)
            })
            .await;
        assert_eq!(reply.stop_reason, StopReason::Error);
        assert!(seen.is_empty());

        let abort = CancellationToken::new();
        abort.cancel();
        let reply = retry_assistant_call(scripted(vec![failed("fetch failed")]), &quick(2), &abort, |_| {}).await;
        assert_eq!(reply.stop_reason, StopReason::Aborted);
    }
}
