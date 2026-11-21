//! Provider dispatch, request options, and the HTTP plumbing shared by adapters.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::{EventReceiver, StreamEvent, send_error};
use crate::models::{Api, Model, ThinkingLevel};
use crate::providers::{anthropic, bedrock, google, openai, responses};
use crate::types::{AssistantMessage, Context, StopReason};

/// Per-request options. `Debug` is written by hand so the key never reaches a log.
#[derive(Clone, Default)]
pub struct StreamOptions {
    pub api_key: Option<String>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub thinking: ThinkingLevel,
    pub cancel: CancellationToken,
    /// Stable id for provider-side session affinity and caching.
    pub session_id: Option<String>,
}

impl std::fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamOptions")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("max_tokens", &self.max_tokens)
            .field("temperature", &self.temperature)
            .field("thinking", &self.thinking)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

/// A function that starts one streamed model request. The agent loop is generic over it so
/// tests can inject scripted responses.
pub type StreamFn = Arc<dyn Fn(&Model, &Context, &StreamOptions) -> EventReceiver + Send + Sync>;

/// The real network-backed stream function.
pub fn default_stream_fn() -> StreamFn {
    Arc::new(stream)
}

/// The output limit a request to `model` asks for: `max_tokens` when set, else the model's own
/// limit for the APIs whose adapters always send one (Anthropic, Claude on Bedrock). None means
/// the server's default applies, whatever it is.
pub fn requested_max_tokens(model: &Model, max_tokens: Option<u64>) -> Option<u64> {
    match model.api {
        Api::AnthropicMessages => Some(max_tokens.unwrap_or(model.max_tokens)),
        Api::BedrockConverseStream => max_tokens.or(bedrock::is_claude(model).then_some(model.max_tokens)),
        _ => max_tokens,
    }
}

/// Run one request to its final message: the `Done` message, or an error when the stream ends
/// without one.
pub async fn complete(stream_fn: &StreamFn, model: &Model, ctx: &Context, opts: &StreamOptions) -> AssistantMessage {
    let mut events = stream_fn(model, ctx, opts);
    while let Some(event) = events.recv().await {
        if let StreamEvent::Done { message } = event {
            return message;
        }
    }
    AssistantMessage {
        stop_reason: StopReason::Error,
        error_message: Some("The stream ended without a reply".into()),
        ..AssistantMessage::empty(model.api.as_str(), &model.provider, &model.id)
    }
}

/// Start a streamed request for `model`. Never fails out of band: errors arrive as the final
/// `Done` message with `StopReason::Error`. Must be called inside a tokio runtime.
pub fn stream(model: &Model, ctx: &Context, opts: &StreamOptions) -> EventReceiver {
    let (tx, rx) = mpsc::unbounded_channel();
    let (model, ctx, opts) = (model.clone(), ctx.clone(), opts.clone());
    match model.api {
        Api::AnthropicMessages => {
            tokio::spawn(anthropic::run(model, ctx, opts, tx));
        }
        Api::OpenAiCompletions => {
            tokio::spawn(openai::run(model, ctx, opts, tx));
        }
        Api::OpenAiResponses => {
            tokio::spawn(responses::run(model, ctx, opts, tx));
        }
        Api::GoogleGenerativeAi => {
            tokio::spawn(google::run(model, ctx, opts, tx));
        }
        Api::BedrockConverseStream => {
            tokio::spawn(bedrock::run(model, ctx, opts, tx));
        }
        Api::Faux => {
            send_error(&model, tx, StopReason::Error, "faux models only work with a faux stream function".into())
        }
    }
    rx
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .user_agent(concat!("nypi/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("HTTP client builds")
    })
}

/// Why a request produced no usable response.
pub(crate) enum RequestFailure {
    Aborted,
    Failed(String),
}

impl RequestFailure {
    pub(crate) fn into_parts(self) -> (StopReason, String) {
        match self {
            RequestFailure::Aborted => (StopReason::Aborted, "Request aborted".into()),
            RequestFailure::Failed(message) => (StopReason::Error, message),
        }
    }
}

const MAX_RETRIES: u32 = 3;

/// POST `body` to `url` with `headers`. A request that never reached a server (the connection
/// failed or timed out) is sent again here, up to `MAX_RETRIES` times with backoff. A status the
/// server answered with (429, 5xx) is returned at once: the one retry layer above (`retry.rs`,
/// Pi's `retryAssistantCall`) decides on it, where the user sees each retry and can stop it, and
/// where quota and billing errors are known not to be worth a wait. Retrying statuses here as
/// well multiplied the attempts (up to sixteen) and slept through quota errors unseen.
pub(crate) async fn post_json(
    url: &str,
    headers: &[(String, String)],
    body: &serde_json::Value,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, RequestFailure> {
    let mut attempt = 0;
    loop {
        let mut request = client().post(url).json(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let outcome = tokio::select! {
            _ = cancel.cancelled() => return Err(RequestFailure::Aborted),
            result = request.send() => result,
        };
        let message = match outcome {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) => {
                let status = response.status().as_u16();
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                let body = response.text().await.unwrap_or_default();
                let after = retry_after.map(|seconds| format!(" (retry after {seconds} s)")).unwrap_or_default();
                return Err(RequestFailure::Failed(format!("HTTP {status}{after}: {}", truncate(&body, 2000))));
            }
            Err(error) if (error.is_connect() || error.is_timeout()) && attempt < MAX_RETRIES => {
                format!("Request failed: {error}")
            }
            Err(error) => return Err(RequestFailure::Failed(format!("Request failed: {error}"))),
        };
        let _ = message;
        let wait = Duration::from_secs(1 << attempt);
        attempt += 1;
        tokio::select! {
            _ = cancel.cancelled() => return Err(RequestFailure::Aborted),
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

/// A body chunk, or why there are no more.
pub(crate) enum Chunk {
    Data(bytes::Bytes),
    End,
    Aborted,
    Failed(String),
}

/// A stream that sends nothing for this long is treated as dead so a run can never hang silently.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) async fn next_chunk(
    body: &mut (impl Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin),
    cancel: &CancellationToken,
) -> Chunk {
    tokio::select! {
        _ = cancel.cancelled() => Chunk::Aborted,
        next = tokio::time::timeout(IDLE_TIMEOUT, body.next()) => match next {
            Err(_) => Chunk::Failed(format!("Stream stalled: no data for {}s", IDLE_TIMEOUT.as_secs())),
            Ok(None) => Chunk::End,
            Ok(Some(Ok(bytes))) => Chunk::Data(bytes),
            Ok(Some(Err(error))) => Chunk::Failed(format!("Stream error: {error}")),
        },
    }
}

pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}
