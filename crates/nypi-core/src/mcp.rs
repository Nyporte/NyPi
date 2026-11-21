//! Model Context Protocol client: starts or connects to configured servers (stdio, Streamable
//! HTTP, or the legacy HTTP+SSE transport), lists their tools and resources, and exposes each as an agent tool named
//! `mcp__<server>__<tool>` (principle A7). Prompt templates become `/mcp:<server>:<prompt>`
//! commands. Servers that announce list changes are re-listed before the next prompt. Over HTTP,
//! an expired session is re-initialized and the request retried, and the server-to-client GET
//! stream stays open, reconnecting as Pi's transport does.

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use async_trait::async_trait;
use futures::StreamExt;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::sse::SseParser;
use nypi_ai::{ContentBlock, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, oneshot};

const PROTOCOL_VERSION: &str = "2025-06-18";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// Resource listings can be huge; the model gets the first pages and is told there are more.
const MAX_RESOURCE_PAGES: usize = 5;

/// One entry of `settings.mcpServers` (the format Claude Desktop and Cursor use). A `url`
/// selects the Streamable HTTP transport; otherwise `command` is started over stdio. Values in
/// `env` and `headers` may reference secrets as `${NAME}`, resolved like API keys.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub disabled: bool,
    /// For a `url`: `http` (Streamable HTTP only), `sse` (the legacy HTTP+SSE transport only),
    /// or unset to try Streamable HTTP and fall back to legacy SSE.
    pub transport: Option<String>,
}

type Pending = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// Reconnecting a dropped SSE stream (the GET stream, a broken response stream): exponential
/// backoff unless the server sent `retry`, as Pi does.
const RECONNECT_INITIAL: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const RECONNECT_TRIES: u32 = 5;
/// A read-only request that failed transiently is tried once more after this.
const RETRY_DELAY: Duration = Duration::from_millis(250);

enum Transport {
    Stdio {
        stdin: Mutex<ChildStdin>,
        pending: Pending,
        _child: Mutex<Child>,
    },
    Http(HttpTransport),
    /// The 2024-11-05 HTTP+SSE transport: a GET stream that names a POST endpoint and carries
    /// every reply (decision D39).
    Legacy(LegacyTransport),
}

struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: reqwest::header::HeaderMap,
    session: std::sync::Mutex<Option<String>>,
    protocol: std::sync::Mutex<Option<String>>,
}

struct LegacyTransport {
    client: reqwest::Client,
    url: String,
    headers: reqwest::header::HeaderMap,
    /// Where messages are POSTed, announced by the stream's `endpoint` event.
    endpoint: std::sync::Mutex<Option<String>>,
    pending: Pending,
    /// The stream is open; when it drops, the session is gone with it.
    open: AtomicBool,
}

/// Why an HTTP exchange failed, so the client can decide to re-initialize or retry.
#[derive(Debug)]
enum HttpFailure {
    /// 404 on a session the server no longer knows (it restarted, or the session timed out): it
    /// did not run the request.
    SessionExpired,
    Status(reqwest::StatusCode, String),
    /// The legacy stream closed, taking the session with it.
    StreamClosed,
}

impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpFailure::SessionExpired => write!(f, "the MCP session expired (HTTP 404)"),
            HttpFailure::Status(status, body) => write!(f, "HTTP {status}: {body}"),
            HttpFailure::StreamClosed => write!(f, "the MCP server closed its event stream"),
        }
    }
}

impl std::error::Error for HttpFailure {}

/// Network failures and overloaded or restarting servers, worth another attempt.
fn is_transient(error: &anyhow::Error) -> bool {
    if let Some(HttpFailure::Status(status, _)) = error.downcast_ref::<HttpFailure>() {
        let code = status.as_u16();
        return code == 408 || code == 429 || (code >= 500 && code != 501);
    }
    error.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>().is_some_and(|e| e.is_connect() || e.is_timeout() || e.is_request())
    })
}

fn needs_new_session(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<HttpFailure>(), Some(HttpFailure::SessionExpired | HttpFailure::StreamClosed))
}

/// Methods that change nothing on the server, so a transient failure can be retried; a tool
/// call cannot, since it may have run.
fn read_only(method: &str) -> bool {
    matches!(
        method,
        "ping"
            | "tools/list"
            | "prompts/list"
            | "prompts/get"
            | "resources/list"
            | "resources/read"
            | "resources/templates/list"
    )
}

fn backoff(attempt: u32, server_ms: Option<u64>) -> Duration {
    match server_ms {
        Some(ms) => Duration::from_millis(ms),
        None => (RECONNECT_INITIAL * 2u32.saturating_pow(attempt)).min(RECONNECT_MAX),
    }
}

fn is_event_stream(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
}

/// A connected MCP server.
pub struct McpClient {
    pub name: String,
    transport: Transport,
    next_id: AtomicU64,
    capabilities: std::sync::Mutex<Value>,
    tools_changed: AtomicBool,
    prompts_changed: AtomicBool,
    /// Counts handshakes, so concurrent requests that saw the same expired session start one
    /// new session, not one each.
    generation: AtomicU64,
    reinit: Mutex<()>,
    /// The server-to-client stream (Streamable HTTP) or the legacy event stream.
    background: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(task) = self.background.lock().unwrap().take() {
            task.abort();
        }
    }
}

/// A prompt template a server offers, run as `/mcp:<server>:<name> [arguments]`.
#[derive(Clone, Debug, Serialize)]
pub struct McpPrompt {
    pub server: String,
    pub name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArgument>,
}

#[derive(Clone, Debug, Serialize)]
pub struct McpPromptArgument {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// One JSON-RPC message from a server.
enum Inbound {
    Response { id: u64, result: Result<Value, String> },
    Request { id: Value, method: String },
    Notification { method: String },
    Other,
}

fn classify(message: &Value) -> Inbound {
    let method = message.get("method").and_then(Value::as_str).map(str::to_string);
    match (message.get("id"), method) {
        (Some(id), Some(method)) => Inbound::Request { id: id.clone(), method },
        (None, Some(method)) => Inbound::Notification { method },
        (Some(id), None) => match id.as_u64() {
            Some(id) => Inbound::Response {
                id,
                result: match message.get("error") {
                    Some(error) => Err(error["message"].as_str().unwrap_or("MCP error").to_string()),
                    None => Ok(message["result"].clone()),
                },
            },
            None => Inbound::Other,
        },
        _ => Inbound::Other,
    }
}

impl McpClient {
    /// Start or reach the server and complete the `initialize` handshake. A URL is tried as
    /// Streamable HTTP first; a server that refuses the POST with 400, 404, or 405 is tried as
    /// a legacy HTTP+SSE server, as the spec's backwards compatibility asks.
    pub async fn connect(name: &str, config: &McpServerConfig) -> anyhow::Result<Arc<Self>> {
        let Some(url) = &config.url else {
            let client = McpClient::spawn(name, config)?;
            client.handshake().await.with_context(|| format!("MCP server {name} did not initialize"))?;
            return Ok(client);
        };
        if config.transport.as_deref() == Some("sse") {
            return McpClient::connect_legacy(name, url, config).await;
        }
        let client = Arc::new(McpClient::new(name, Transport::Http(HttpTransport::new(url, config)?)));
        match client.handshake().await {
            Ok(()) => Ok(client),
            Err(error)
                if config.transport.is_none()
                    && matches!(
                        error.downcast_ref::<HttpFailure>(),
                        Some(HttpFailure::Status(status, _)) if matches!(status.as_u16(), 400 | 404 | 405)
                    ) =>
            {
                McpClient::connect_legacy(name, url, config)
                    .await
                    .with_context(|| format!("MCP server {name} refused Streamable HTTP ({error}) and legacy SSE"))
            }
            Err(error) => Err(error.context(format!("MCP server {name} did not initialize"))),
        }
    }

    async fn connect_legacy(name: &str, url: &str, config: &McpServerConfig) -> anyhow::Result<Arc<Self>> {
        let client = Arc::new(McpClient::new(name, Transport::Legacy(LegacyTransport::new(url, config)?)));
        client.open_legacy_stream().await?;
        client.handshake().await.with_context(|| format!("MCP server {name} did not initialize (legacy SSE)"))?;
        Ok(client)
    }

    fn new(name: &str, transport: Transport) -> Self {
        McpClient {
            name: name.to_string(),
            transport,
            next_id: AtomicU64::new(1),
            capabilities: std::sync::Mutex::new(Value::Null),
            tools_changed: AtomicBool::new(false),
            prompts_changed: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            reinit: Mutex::new(()),
            background: std::sync::Mutex::new(None),
        }
    }

    /// `initialize` and `notifications/initialized`; on Streamable HTTP, then open the
    /// server-to-client stream, which the spec allows only once the session is initialized.
    async fn handshake(self: &Arc<Self>) -> anyhow::Result<()> {
        let result = self
            .request_once(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "nypi", "version": env!("CARGO_PKG_VERSION") }
                }),
                STARTUP_TIMEOUT,
            )
            .await?;
        if let Transport::Http(http) = &self.transport {
            let version = result["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION).to_string();
            *http.protocol.lock().unwrap() = Some(version);
        }
        *self.capabilities.lock().unwrap() = result["capabilities"].clone();
        self.notify("notifications/initialized", json!({})).await?;
        self.generation.fetch_add(1, Ordering::SeqCst);
        if matches!(self.transport, Transport::Http(_)) {
            let task = tokio::spawn(get_stream(Arc::downgrade(self)));
            if let Some(old) = self.background.lock().unwrap().replace(task) {
                old.abort();
            }
        }
        Ok(())
    }

    /// A new session after the old one expired (or the legacy stream dropped). One handshake
    /// per expiry, however many requests noticed it.
    async fn reinitialize(self: &Arc<Self>, seen: u64) -> anyhow::Result<()> {
        let _one = self.reinit.lock().await;
        if self.generation.load(Ordering::SeqCst) != seen {
            return Ok(());
        }
        match &self.transport {
            Transport::Http(http) => {
                *http.session.lock().unwrap() = None;
                *http.protocol.lock().unwrap() = None;
            }
            Transport::Legacy(_) => self.open_legacy_stream().await?,
            Transport::Stdio { .. } => bail!("MCP server {} exited", self.name),
        }
        self.handshake().await.with_context(|| format!("MCP server {} did not start a new session", self.name))
    }

    fn spawn(name: &str, config: &McpServerConfig) -> anyhow::Result<Arc<Self>> {
        let mut env = BTreeMap::new();
        for (key, value) in &config.env {
            env.insert(key.clone(), crate::config::expand_secrets(value).with_context(|| format!("env {key}"))?);
        }
        // A server gets the keys its own `env` names, and none of the ones NyPi was started with
        // (decision D76): a tool server has no business with the model's key.
        let mut plain = std::process::Command::new(&config.command);
        crate::sandbox::scrub_env(&mut plain, &[]);
        let mut command = tokio::process::Command::from(plain);
        command
            .args(&config.args)
            .envs(&env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            command.current_dir(crate::config::expand_home(cwd));
        }
        let mut child =
            command.spawn().with_context(|| format!("cannot start MCP server {name} ({})", config.command))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let pending: Pending = Arc::default();
        let transport =
            Transport::Stdio { stdin: Mutex::new(stdin), pending: pending.clone(), _child: Mutex::new(child) };
        let client = Arc::new(McpClient::new(name, transport));
        tokio::spawn(read_loop(stdout, pending, Arc::downgrade(&client)));
        Ok(client)
    }

    /// Open the legacy event stream and wait for its `endpoint` event.
    async fn open_legacy_stream(self: &Arc<Self>) -> anyhow::Result<()> {
        let Transport::Legacy(legacy) = &self.transport else { return Ok(()) };
        *legacy.endpoint.lock().unwrap() = None;
        let response = legacy
            .client
            .get(&legacy.url)
            .headers(legacy.headers.clone())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .with_context(|| format!("cannot reach {}", legacy.url))?;
        if !response.status().is_success() || !is_event_stream(&response) {
            bail!("{} did not open an event stream (HTTP {})", legacy.url, response.status());
        }
        let (ready_tx, ready_rx) = oneshot::channel();
        legacy.open.store(true, Ordering::SeqCst);
        let task = tokio::spawn(legacy_stream(response, Arc::downgrade(self), ready_tx));
        if let Some(old) = self.background.lock().unwrap().replace(task) {
            old.abort();
        }
        match tokio::time::timeout(STARTUP_TIMEOUT, ready_rx).await {
            Ok(Ok(())) => Ok(()),
            _ => bail!("{} sent no endpoint event", legacy.url),
        }
    }

    /// Whether the server announced a changed tool list since the last call.
    pub fn take_tools_changed(&self) -> bool {
        self.tools_changed.swap(false, Ordering::SeqCst)
    }

    /// Whether the server announced a changed prompt list since the last call.
    pub fn take_prompts_changed(&self) -> bool {
        self.prompts_changed.swap(false, Ordering::SeqCst)
    }

    /// The server's prompt templates; empty when it offers none.
    pub async fn list_prompts(self: &Arc<Self>) -> anyhow::Result<Vec<McpPrompt>> {
        if !self.has_capability("prompts") {
            return Ok(Vec::new());
        }
        let (prompts, _) = self.list_all("prompts/list", "prompts", usize::MAX).await?;
        Ok(prompts
            .iter()
            .map(|p| McpPrompt {
                server: self.name.clone(),
                name: p["name"].as_str().unwrap_or_default().to_string(),
                description: p["description"].as_str().unwrap_or_default().to_string(),
                arguments: p["arguments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|a| McpPromptArgument {
                        name: a["name"].as_str().unwrap_or_default().to_string(),
                        description: a["description"].as_str().unwrap_or_default().to_string(),
                        required: a["required"].as_bool().unwrap_or(false),
                    })
                    .collect(),
            })
            .collect())
    }

    /// Render a prompt into the text of one user message.
    pub async fn get_prompt(
        self: &Arc<Self>,
        name: &str,
        arguments: &BTreeMap<String, String>,
    ) -> anyhow::Result<String> {
        let result =
            self.request("prompts/get", json!({ "name": name, "arguments": arguments }), STARTUP_TIMEOUT).await?;
        let parts: Vec<String> = result["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|message| {
                let content = &message["content"];
                let text = match content["type"].as_str() {
                    Some("text") => content["text"].as_str().unwrap_or_default().to_string(),
                    Some("resource") => content["resource"]["text"].as_str().unwrap_or_default().to_string(),
                    Some(other) => format!("[{other} content omitted]"),
                    None => String::new(),
                };
                // The prompt arrives as one user message, so other roles are labeled inline.
                match message["role"].as_str() {
                    Some("user") | None => text,
                    Some(role) => format!("[{role}] {text}"),
                }
            })
            .filter(|t| !t.is_empty())
            .collect();
        if parts.is_empty() {
            bail!("MCP prompt {name} returned no text");
        }
        Ok(parts.join("\n\n"))
    }

    fn has_capability(&self, name: &str) -> bool {
        self.capabilities.lock().unwrap().get(name).is_some_and(|v| !v.is_null())
    }

    /// All tools the server offers (follows pagination).
    pub async fn list_tools(self: &Arc<Self>) -> anyhow::Result<Vec<Value>> {
        self.list_all("tools/list", "tools", usize::MAX).await.map(|(items, _)| items)
    }

    /// Items of a paginated list method, and whether more pages were left unread.
    async fn list_all(
        self: &Arc<Self>,
        method: &str,
        key: &str,
        max_pages: usize,
    ) -> anyhow::Result<(Vec<Value>, bool)> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..max_pages {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let page = self.request(method, params, STARTUP_TIMEOUT).await?;
            items.extend(page[key].as_array().cloned().unwrap_or_default());
            match page["nextCursor"].as_str() {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok((items, false)),
            }
        }
        Ok((items, true))
    }

    /// A request, recovering as Pi does: on an expired session (the server did not run it), once
    /// more on a new session; a read-only request, once more after a transient failure.
    pub async fn request(self: &Arc<Self>, method: &str, params: Value, timeout: Duration) -> anyhow::Result<Value> {
        let generation = self.generation.load(Ordering::SeqCst);
        match self.request_once(method, params.clone(), timeout).await {
            Err(error) if needs_new_session(&error) => {
                self.reinitialize(generation).await?;
                self.request_once(method, params, timeout).await
            }
            Err(error) if read_only(method) && is_transient(&error) => {
                tokio::time::sleep(RETRY_DELAY).await;
                self.request_once(method, params, timeout).await
            }
            result => result,
        }
    }

    async fn request_once(&self, method: &str, params: Value, timeout: Duration) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let routed = |pending: &Pending| {
            let (tx, rx) = oneshot::channel();
            pending.lock().unwrap().insert(id, tx);
            rx
        };
        let exchange = async {
            match &self.transport {
                Transport::Stdio { pending, .. } => {
                    let rx = routed(pending);
                    self.send(message).await?;
                    match rx.await {
                        Ok(result) => result.map_err(|e| anyhow!(e)),
                        Err(_) => bail!("MCP server {} exited", self.name),
                    }
                }
                Transport::Legacy(legacy) => {
                    if !legacy.open.load(Ordering::SeqCst) {
                        return Err(HttpFailure::StreamClosed.into());
                    }
                    let rx = routed(&legacy.pending);
                    self.send(message).await?;
                    match rx.await {
                        Ok(result) => result.map_err(|e| anyhow!(e)),
                        Err(_) => Err(HttpFailure::StreamClosed.into()),
                    }
                }
                Transport::Http(http) => self.http_exchange(http, id, &message).await,
            }
        };
        let result = tokio::time::timeout(timeout, exchange).await;
        match &self.transport {
            Transport::Stdio { pending, .. } => drop(pending.lock().unwrap().remove(&id)),
            Transport::Legacy(legacy) => drop(legacy.pending.lock().unwrap().remove(&id)),
            Transport::Http(_) => {}
        }
        match result {
            Ok(result) => result,
            Err(_) => {
                let _ = self.notify("notifications/cancelled", json!({ "requestId": id, "reason": "timeout" })).await;
                bail!("MCP {method} timed out after {}s", timeout.as_secs())
            }
        }
    }

    /// POST one request; the reply is either a JSON body or an SSE stream that may carry server
    /// requests and notifications before the response. A stream that breaks before the response
    /// is resumed with GET and `Last-Event-ID` when the server numbered its events.
    async fn http_exchange(&self, http: &HttpTransport, id: u64, message: &Value) -> anyhow::Result<Value> {
        let response = http.post(message).await?;
        if !is_event_stream(&response) {
            let body: Value = response.json().await.context("MCP server sent invalid JSON")?;
            let messages = match body {
                Value::Array(messages) => messages,
                single => vec![single],
            };
            for message in messages {
                if let Some(result) = self.handle_inbound(&message, Some(id)).await {
                    return result.map_err(|e| anyhow!(e));
                }
            }
            bail!("MCP server {} sent no response", self.name);
        }
        let mut parser = SseParser::default();
        let mut response = response;
        let mut attempt = 0;
        loop {
            let mut stream = response.bytes_stream();
            let mut received = false;
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else { break };
                for event in parser.push(&chunk) {
                    received = true;
                    let Ok(message) = serde_json::from_str::<Value>(&event.data) else { continue };
                    if let Some(result) = self.handle_inbound(&message, Some(id)).await {
                        return result.map_err(|e| anyhow!(e));
                    }
                }
            }
            let Some(last_id) = parser.last_event_id().map(str::to_string) else {
                bail!("MCP server {} closed the stream without a response", self.name)
            };
            if received {
                attempt = 0;
            }
            if attempt >= RECONNECT_TRIES {
                bail!("MCP server {} kept dropping the response stream", self.name);
            }
            tokio::time::sleep(backoff(attempt, parser.retry_ms())).await;
            attempt += 1;
            match http.get_stream(Some(&last_id)).await? {
                Some(resumed) => response = resumed,
                None => bail!("MCP server {} closed the stream and cannot resume it", self.name),
            }
        }
    }

    /// Act on a server message. Returns the result when it is the response to `expected`.
    async fn handle_inbound(&self, message: &Value, expected: Option<u64>) -> Option<Result<Value, String>> {
        match classify(message) {
            Inbound::Response { id, result } if Some(id) == expected => return Some(result),
            Inbound::Request { id, method } => {
                let reply = match method.as_str() {
                    "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                    "roots/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "roots": [] } }),
                    _ => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": { "code": -32601, "message": "Method not supported by NyPi" }
                    }),
                };
                let _ = self.send(reply).await;
            }
            Inbound::Notification { method } if method == "notifications/tools/list_changed" => {
                self.tools_changed.store(true, Ordering::SeqCst);
            }
            Inbound::Notification { method } if method == "notifications/prompts/list_changed" => {
                self.prompts_changed.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
        None
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    /// Send a message that expects no response (notifications, replies to server requests).
    async fn send(&self, message: Value) -> anyhow::Result<()> {
        match &self.transport {
            Transport::Stdio { stdin, .. } => {
                let mut line = serde_json::to_vec(&message)?;
                line.push(b'\n');
                let mut stdin = stdin.lock().await;
                stdin
                    .write_all(&line)
                    .await
                    .map_err(|e| anyhow!("MCP server {} is not accepting input: {e}", self.name))?;
                stdin.flush().await?;
                Ok(())
            }
            Transport::Http(http) => http.post(&message).await.map(drop),
            Transport::Legacy(legacy) => legacy.post(&message).await,
        }
    }
}

fn config_headers(config: &McpServerConfig) -> anyhow::Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (key, value) in &config.headers {
        let value = crate::config::expand_secrets(value).with_context(|| format!("header {key}"))?;
        let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
            .with_context(|| format!("invalid header name {key}"))?;
        let mut value =
            reqwest::header::HeaderValue::from_str(&value).with_context(|| format!("invalid value for {key}"))?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    Ok(headers)
}

impl HttpTransport {
    fn new(url: &str, config: &McpServerConfig) -> anyhow::Result<Self> {
        Ok(HttpTransport {
            client: reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).build()?,
            url: url.to_string(),
            headers: config_headers(config)?,
            session: std::sync::Mutex::new(None),
            protocol: std::sync::Mutex::new(None),
        })
    }

    fn with_session(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request = request.headers(self.headers.clone());
        if let Some(session) = self.session.lock().unwrap().clone() {
            request = request.header("Mcp-Session-Id", session);
        }
        if let Some(version) = self.protocol.lock().unwrap().clone() {
            request = request.header("MCP-Protocol-Version", version);
        }
        request
    }

    async fn check(&self, response: reqwest::Response) -> anyhow::Result<reqwest::Response> {
        if let Some(session) = response.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *self.session.lock().unwrap() = Some(session.to_string());
        }
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body: String = response.text().await.unwrap_or_default().chars().take(300).collect();
        if status == reqwest::StatusCode::NOT_FOUND && self.session.lock().unwrap().is_some() {
            return Err(HttpFailure::SessionExpired.into());
        }
        Err(HttpFailure::Status(status, body).into())
    }

    async fn post(&self, message: &Value) -> anyhow::Result<reqwest::Response> {
        let request = self
            .with_session(self.client.post(&self.url))
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
            .json(message);
        let response = request.send().await.with_context(|| format!("cannot reach {}", self.url))?;
        self.check(response).await
    }

    /// Open a GET event stream, resuming after `last_id` when given. None when the server
    /// offers no such stream (405, or not an event stream).
    async fn get_stream(&self, last_id: Option<&str>) -> anyhow::Result<Option<reqwest::Response>> {
        let mut request =
            self.with_session(self.client.get(&self.url)).header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(id) = last_id {
            request = request.header("Last-Event-ID", id);
        }
        let response = request.send().await.with_context(|| format!("cannot reach {}", self.url))?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Ok(None);
        }
        let response = self.check(response).await?;
        Ok(is_event_stream(&response).then_some(response))
    }
}

impl LegacyTransport {
    fn new(url: &str, config: &McpServerConfig) -> anyhow::Result<Self> {
        Ok(LegacyTransport {
            // No overall timeout: the event stream stays open for the whole session.
            client: reqwest::Client::builder().connect_timeout(Duration::from_secs(15)).build()?,
            url: url.to_string(),
            headers: config_headers(config)?,
            endpoint: std::sync::Mutex::new(None),
            pending: Arc::default(),
            open: AtomicBool::new(false),
        })
    }

    async fn post(&self, message: &Value) -> anyhow::Result<()> {
        let Some(endpoint) = self.endpoint.lock().unwrap().clone() else {
            return Err(HttpFailure::StreamClosed.into());
        };
        let response = self
            .client
            .post(&endpoint)
            .headers(self.headers.clone())
            .json(message)
            .send()
            .await
            .with_context(|| format!("cannot reach {endpoint}"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body: String = response.text().await.unwrap_or_default().chars().take(300).collect();
        // The endpoint belongs to one stream; once the server forgets it, so is the session.
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(HttpFailure::StreamClosed.into());
        }
        Err(HttpFailure::Status(status, body).into())
    }
}

/// The Streamable HTTP server-to-client stream: notifications and requests the server sends
/// outside any request. Reconnects with backoff and `Last-Event-ID`; ends when the server offers
/// no stream, the session is gone, or the client is dropped.
async fn get_stream(client: std::sync::Weak<McpClient>) {
    let mut parser = SseParser::default();
    let mut attempt = 0;
    loop {
        let Some(strong) = client.upgrade() else { return };
        let Transport::Http(http) = &strong.transport else { return };
        let opened = match http.get_stream(parser.last_event_id()).await {
            Ok(Some(response)) => Some(response),
            Ok(None) => return,
            Err(error) if is_transient(&error) => None,
            Err(_) => return,
        };
        drop(strong);
        if let Some(response) = opened {
            let started = std::time::Instant::now();
            let mut received = false;
            let mut stream = response.bytes_stream();
            while let Some(Ok(chunk)) = stream.next().await {
                for event in parser.push(&chunk) {
                    received = true;
                    if event.event.as_deref().is_some_and(|e| e != "message") || event.data.trim().is_empty() {
                        continue;
                    }
                    let Ok(message) = serde_json::from_str::<Value>(&event.data) else { continue };
                    let Some(strong) = client.upgrade() else { return };
                    strong.handle_inbound(&message, None).await;
                }
            }
            // A stream that stayed up for a while counts as healthy, even if it was idle.
            if received || started.elapsed() > RECONNECT_MAX {
                attempt = 0;
            }
        }
        if attempt >= RECONNECT_TRIES {
            return;
        }
        tokio::time::sleep(backoff(attempt, parser.retry_ms())).await;
        attempt += 1;
    }
}

/// The legacy transport's event stream: the `endpoint` event first, then every message.
/// Responses go to the waiting request; the rest through `handle_inbound`. When it ends, the
/// session ends with it: waiting requests fail, and the next one opens a new stream.
async fn legacy_stream(response: reqwest::Response, client: std::sync::Weak<McpClient>, ready: oneshot::Sender<()>) {
    let mut ready = Some(ready);
    let mut parser = SseParser::default();
    let mut stream = response.bytes_stream();
    let base = response_url(&client);
    while let Some(Ok(chunk)) = stream.next().await {
        for event in parser.push(&chunk) {
            let Some(strong) = client.upgrade() else { return };
            let Transport::Legacy(legacy) = &strong.transport else { return };
            if event.event.as_deref() == Some("endpoint") {
                let endpoint = match reqwest::Url::parse(&base).and_then(|b| b.join(event.data.trim())) {
                    Ok(url) => url.to_string(),
                    Err(_) => event.data.trim().to_string(),
                };
                *legacy.endpoint.lock().unwrap() = Some(endpoint);
                if let Some(ready) = ready.take() {
                    let _ = ready.send(());
                }
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&event.data) else { continue };
            if let Inbound::Response { id, result } = classify(&message) {
                if let Some(sender) = legacy.pending.lock().unwrap().remove(&id) {
                    let _ = sender.send(result);
                }
                continue;
            }
            strong.handle_inbound(&message, None).await;
        }
    }
    if let Some(strong) = client.upgrade()
        && let Transport::Legacy(legacy) = &strong.transport
    {
        legacy.open.store(false, Ordering::SeqCst);
        legacy.pending.lock().unwrap().clear();
    }
}

fn response_url(client: &std::sync::Weak<McpClient>) -> String {
    match client.upgrade().as_deref().map(|c| &c.transport) {
        Some(Transport::Legacy(legacy)) => legacy.url.clone(),
        _ => String::new(),
    }
}

/// Route stdio responses to waiting requests; everything else goes through `handle_inbound`.
async fn read_loop(stdout: tokio::process::ChildStdout, pending: Pending, client: std::sync::Weak<McpClient>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        if let Inbound::Response { id, result } = classify(&message) {
            let sender = pending.lock().unwrap().remove(&id);
            if let Some(sender) = sender {
                let _ = sender.send(result);
            }
            continue;
        }
        let Some(client) = client.upgrade() else { return };
        client.handle_inbound(&message, None).await;
    }
}

/// An MCP server tool adapted to the agent's `Tool` trait.
pub struct McpTool {
    client: Arc<McpClient>,
    remote_name: String,
    definition: ToolDefinition,
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let call =
            self.client.request("tools/call", json!({ "name": self.remote_name, "arguments": args }), CALL_TIMEOUT);
        let result = tokio::select! {
            result = call => result?,
            _ = ctx.cancel.cancelled() => bail!("Operation aborted"),
        };
        Ok(to_output(&result))
    }
}

#[derive(Clone, Copy)]
enum ResourceAction {
    List,
    Read,
}

/// `list_resources` and `read_resource` for a server that offers resources.
pub struct McpResourceTool {
    client: Arc<McpClient>,
    action: ResourceAction,
    definition: ToolDefinition,
}

#[async_trait]
impl Tool for McpResourceTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let work = async {
            match self.action {
                ResourceAction::List => {
                    let (resources, more) =
                        self.client.list_all("resources/list", "resources", MAX_RESOURCE_PAGES).await?;
                    let mut lines: Vec<String> = resources
                        .iter()
                        .map(|r| {
                            let mut line = format!(
                                "{} — {}",
                                r["uri"].as_str().unwrap_or_default(),
                                r["name"].as_str().unwrap_or_default()
                            );
                            if let Some(mime) = r["mimeType"].as_str() {
                                line.push_str(&format!(" ({mime})"));
                            }
                            if let Some(description) = r["description"].as_str() {
                                line.push_str(&format!(": {description}"));
                            }
                            line
                        })
                        .collect();
                    if lines.is_empty() {
                        lines.push("(no resources)".into());
                    }
                    if more {
                        lines.push(format!(
                            "[more resources exist; only the first {MAX_RESOURCE_PAGES} pages are shown]"
                        ));
                    }
                    anyhow::Ok(ToolOutput::text(lines.join("\n")))
                }
                ResourceAction::Read => {
                    let uri = args["uri"].as_str().unwrap_or_default();
                    let result = self.client.request("resources/read", json!({ "uri": uri }), CALL_TIMEOUT).await?;
                    Ok(resource_output(&result))
                }
            }
        };
        tokio::select! {
            output = work => output,
            _ = ctx.cancel.cancelled() => bail!("Operation aborted"),
        }
    }
}

fn resource_output(result: &Value) -> ToolOutput {
    let mut content = Vec::new();
    for item in result["contents"].as_array().into_iter().flatten() {
        let mime = item["mimeType"].as_str().unwrap_or_default();
        if let Some(text) = item["text"].as_str() {
            content.push(ContentBlock::text(text));
        } else if let Some(blob) = item["blob"].as_str() {
            if mime.starts_with("image/") {
                content.push(ContentBlock::Image { data: blob.into(), mime_type: mime.into() });
            } else {
                content.push(ContentBlock::text(format!(
                    "[binary resource {} ({mime}, {} bytes base64)]",
                    item["uri"].as_str().unwrap_or_default(),
                    blob.len()
                )));
            }
        }
    }
    if content.is_empty() {
        content.push(ContentBlock::text("(empty resource)"));
    }
    ToolOutput { content, ..Default::default() }
}

/// Convert an MCP `CallToolResult` to model content.
fn to_output(result: &Value) -> ToolOutput {
    let mut content = Vec::new();
    for block in result["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => content.push(ContentBlock::text(block["text"].as_str().unwrap_or_default())),
            Some("image") => content.push(ContentBlock::Image {
                data: block["data"].as_str().unwrap_or_default().into(),
                mime_type: block["mimeType"].as_str().unwrap_or("image/png").into(),
            }),
            Some("resource") => {
                let resource = &block["resource"];
                let text = resource["text"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("[binary resource {}]", resource["uri"].as_str().unwrap_or_default()));
                content.push(ContentBlock::text(text));
            }
            Some("resource_link") => content.push(ContentBlock::text(format!(
                "[resource link: {} {}]",
                block["name"].as_str().unwrap_or_default(),
                block["uri"].as_str().unwrap_or_default()
            ))),
            Some(other) => content.push(ContentBlock::text(format!("[{other} content omitted]"))),
            None => {}
        }
    }
    if content.is_empty() {
        let structured = result.get("structuredContent").filter(|v| !v.is_null());
        content.push(ContentBlock::text(structured.map_or("(no output)".into(), |v| v.to_string())));
    }
    ToolOutput {
        content,
        details: result.get("structuredContent").cloned(),
        is_error: result["isError"].as_bool().unwrap_or(false),
        terminate: false,
    }
}

/// A connected server and the agent tools it provides.
pub type McpServer = (Arc<McpClient>, Vec<Arc<dyn Tool>>);

/// Connected servers, and one message per server that failed.
pub struct McpConnections {
    pub servers: Vec<McpServer>,
    pub errors: Vec<String>,
}

/// Connect every enabled server.
pub async fn connect_all(servers: &BTreeMap<String, McpServerConfig>) -> McpConnections {
    let attempts = servers.iter().filter(|(_, c)| !c.disabled).map(|(name, config)| async move {
        let connected = async {
            let client = McpClient::connect(name, config).await?;
            let tools = server_tools(&client).await?;
            anyhow::Ok((client, tools))
        };
        (name.clone(), tokio::time::timeout(STARTUP_TIMEOUT, connected).await)
    });
    let mut connections = McpConnections { servers: Vec::new(), errors: Vec::new() };
    for (name, outcome) in futures::future::join_all(attempts).await {
        match outcome {
            Ok(Ok(server)) => connections.servers.push(server),
            Ok(Err(error)) => connections.errors.push(format!("MCP server {name}: {error:#}")),
            Err(_) => {
                connections.errors.push(format!("MCP server {name}: no response within {}s", STARTUP_TIMEOUT.as_secs()))
            }
        }
    }
    connections
}

/// Every agent tool a server provides: its tools, plus resource tools when it has resources.
pub async fn server_tools(client: &Arc<McpClient>) -> anyhow::Result<Vec<Arc<dyn Tool>>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for tool in client.list_tools().await? {
        tools.push(Arc::new(adapt(client, &tool)));
    }
    if client.has_capability("resources") {
        let server = &client.name;
        tools.push(Arc::new(McpResourceTool {
            client: client.clone(),
            action: ResourceAction::List,
            definition: ToolDefinition {
                name: tool_name(server, "list_resources"),
                description: format!("List the resources (files, records, documents) MCP server {server} offers."),
                parameters: json!({ "type": "object", "properties": {} }),
            },
        }));
        tools.push(Arc::new(McpResourceTool {
            client: client.clone(),
            action: ResourceAction::Read,
            definition: ToolDefinition {
                name: tool_name(server, "read_resource"),
                description: format!("Read one resource from MCP server {server} by URI."),
                parameters: json!({
                    "type": "object",
                    "properties": { "uri": { "type": "string", "description": "Resource URI from list_resources" } },
                    "required": ["uri"]
                }),
            },
        }));
    }
    Ok(tools)
}

/// Arguments for a prompt from command text: `key=value` pairs (quotes allowed), or the whole
/// text when the prompt takes exactly one argument.
pub fn parse_prompt_arguments(prompt: &McpPrompt, raw: &str) -> Result<BTreeMap<String, String>, String> {
    let raw = raw.trim();
    let mut arguments = BTreeMap::new();
    if !raw.is_empty() {
        let tokens = split_words(raw);
        let pairs: Vec<(String, String)> = tokens
            .iter()
            .filter_map(|t| t.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .filter(|(k, _)| prompt.arguments.iter().any(|a| &a.name == k))
            .collect();
        if pairs.len() == tokens.len() {
            arguments.extend(pairs);
        } else if prompt.arguments.len() == 1 {
            let whole = if tokens.len() == 1 { tokens[0].clone() } else { raw.to_string() };
            arguments.insert(prompt.arguments[0].name.clone(), whole);
        } else {
            return Err(format!("Give the arguments as key=value. {}", argument_help(prompt)));
        }
    }
    let missing: Vec<&str> = prompt
        .arguments
        .iter()
        .filter(|a| a.required && !arguments.contains_key(&a.name))
        .map(|a| a.name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(format!("Missing required argument(s): {}. {}", missing.join(", "), argument_help(prompt)));
    }
    Ok(arguments)
}

fn argument_help(prompt: &McpPrompt) -> String {
    if prompt.arguments.is_empty() {
        return format!("/mcp:{}:{} takes no arguments.", prompt.server, prompt.name);
    }
    let list: Vec<String> =
        prompt.arguments.iter().map(|a| format!("{}{}", a.name, if a.required { " (required)" } else { "" })).collect();
    format!("/mcp:{}:{} takes: {}", prompt.server, prompt.name, list.join(", "))
}

/// Split on whitespace, keeping quoted runs together (quotes removed).
fn split_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => quote = Some(c),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn adapt(client: &Arc<McpClient>, tool: &Value) -> McpTool {
    let remote_name = tool["name"].as_str().unwrap_or_default().to_string();
    let mut parameters = tool["inputSchema"].clone();
    if !parameters.is_object() {
        parameters = json!({ "type": "object", "properties": {} });
    }
    McpTool {
        client: client.clone(),
        definition: ToolDefinition {
            name: tool_name(&client.name, &remote_name),
            description: tool["description"].as_str().unwrap_or_default().to_string(),
            parameters,
        },
        remote_name,
    }
}

/// `mcp__<server>__<tool>`, restricted to the characters and length providers accept.
fn tool_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| {
        s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>()
    };
    let name = format!("mcp__{}__{}", clean(server), clean(tool));
    name.chars().take(64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny MCP server in Python: one `add` tool.
    const MOCK_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    if "id" not in msg:
        continue
    m = msg["method"]
    if m == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "mock", "version": "1"}}
    elif m == "tools/list":
        result = {"tools": [{"name": "add", "description": "Add two numbers", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}}]}
    elif m == "tools/call":
        a = msg["params"]["arguments"]
        result = {"content": [{"type": "text", "text": str(a["a"] + a["b"])}]}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "nope"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
"#;

    #[tokio::test]
    async fn connects_lists_and_calls_tools() {
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            eprintln!("python3 not available; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("server.py");
        std::fs::write(&script, MOCK_SERVER).unwrap();
        let mut servers = BTreeMap::new();
        servers.insert(
            "calc".to_string(),
            McpServerConfig {
                command: "python3".into(),
                args: vec![script.display().to_string()],
                ..Default::default()
            },
        );
        servers.insert(
            "broken".to_string(),
            McpServerConfig { command: "/nonexistent/server".into(), ..Default::default() },
        );
        let connections = connect_all(&servers).await;
        let errors = connections.errors;
        let tools: Vec<_> = connections.servers.into_iter().flat_map(|(_, tools)| tools).collect();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].definition().name, "mcp__calc__add");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolContext::new("c".into(), "add".into(), tokio_util::sync::CancellationToken::new(), tx);
        let output = tools[0].execute(json!({ "a": 2, "b": 3 }), ctx).await.unwrap();
        assert_eq!(nypi_ai::text_of(&output.content), "5");
    }

    /// Mock Streamable HTTP server. Records (method or "reply", session header, protocol header).
    async fn mock_http_server(seen: Arc<std::sync::Mutex<Vec<(String, String, String)>>>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let listed = Arc::new(AtomicU64::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let seen = seen.clone();
                let listed = listed.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head, body) = loop {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buffer).to_string();
                        if let Some((head, body)) = text.split_once("\r\n\r\n") {
                            let length = head
                                .lines()
                                .find_map(|l| {
                                    l.to_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse().unwrap_or(0))
                                })
                                .unwrap_or(0usize);
                            if body.len() >= length {
                                break (head.to_lowercase(), body.to_string());
                            }
                        }
                    };
                    let header = |name: &str| {
                        head.lines()
                            .find_map(|l| l.strip_prefix(&format!("{name}:")).map(|v| v.trim().to_string()))
                            .unwrap_or_default()
                    };
                    let message: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let method = message["method"].as_str().unwrap_or("reply").to_string();
                    seen.lock().unwrap().push((
                        method.clone(),
                        header("mcp-session-id"),
                        header("mcp-protocol-version"),
                    ));
                    let id = message["id"].clone();
                    let respond = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
                    let sse = |events: Vec<String>| {
                        let body: String = events.iter().map(|e| format!("event: message\ndata: {e}\n\n")).collect();
                        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
                    };
                    let json_reply = |payload: String, extra: &str| {
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        )
                    };
                    let tool =
                        |name: &str| json!({ "name": name, "inputSchema": { "type": "object", "properties": {} } });
                    let response = match method.as_str() {
                        "initialize" => json_reply(
                            respond(json!({
                                "protocolVersion": "2025-06-18",
                                "capabilities": { "tools": { "listChanged": true }, "resources": {}, "prompts": {} },
                                "serverInfo": { "name": "mock", "version": "1" }
                            })),
                            "Mcp-Session-Id: s-1\r\n",
                        ),
                        "tools/list" => {
                            let tools = if listed.fetch_add(1, Ordering::SeqCst) == 0 {
                                vec![tool("add")]
                            } else {
                                vec![tool("add"), tool("mul")]
                            };
                            sse(vec![respond(json!({ "tools": tools }))])
                        }
                        "tools/call" => sse(vec![
                            json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }).to_string(),
                            json!({ "jsonrpc": "2.0", "id": "srv-1", "method": "ping" }).to_string(),
                            respond(json!({ "content": [{ "type": "text", "text": "5" }] })),
                        ]),
                        "resources/list" => json_reply(
                            respond(
                                json!({ "resources": [{ "uri": "file:///notes.txt", "name": "notes", "mimeType": "text/plain" }] }),
                            ),
                            "",
                        ),
                        "prompts/list" => json_reply(
                            respond(json!({ "prompts": [{
                                "name": "review", "description": "Review a file",
                                "arguments": [{ "name": "file", "required": true }]
                            }] })),
                            "",
                        ),
                        "prompts/get" => json_reply(
                            respond(json!({ "messages": [
                                { "role": "user", "content": { "type": "text",
                                  "text": format!("Please review {}", message["params"]["arguments"]["file"].as_str().unwrap_or("?")) } }
                            ] })),
                            "",
                        ),
                        "resources/read" => json_reply(
                            respond(json!({ "contents": [{ "uri": "file:///notes.txt", "text": "hello notes" }] })),
                            "",
                        ),
                        _ => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{addr}/mcp")
    }

    #[tokio::test]
    async fn streamable_http_sessions_streams_resources_and_tool_changes() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let url = mock_http_server(seen.clone()).await;
        let mut servers = BTreeMap::new();
        servers.insert("remote".to_string(), McpServerConfig { url: Some(url), ..Default::default() });
        let connections = connect_all(&servers).await;
        assert!(connections.errors.is_empty(), "{:?}", connections.errors);
        let (client, tools) = connections.servers.into_iter().next().unwrap();
        let names: Vec<String> = tools.iter().map(|t| t.definition().name).collect();
        assert_eq!(names, ["mcp__remote__add", "mcp__remote__list_resources", "mcp__remote__read_resource"]);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = || ToolContext::new("c".into(), "t".into(), tokio_util::sync::CancellationToken::new(), tx.clone());
        let output = tools[0].execute(json!({}), ctx()).await.unwrap();
        assert_eq!(nypi_ai::text_of(&output.content), "5");
        assert!(client.take_tools_changed(), "list_changed seen in the call's stream");
        let listing = tools[1].execute(json!({}), ctx()).await.unwrap();
        assert!(nypi_ai::text_of(&listing.content).contains("file:///notes.txt — notes (text/plain)"));
        let read = tools[2].execute(json!({ "uri": "file:///notes.txt" }), ctx()).await.unwrap();
        assert_eq!(nypi_ai::text_of(&read.content), "hello notes");
        let prompts = client.list_prompts().await.unwrap();
        assert_eq!(prompts.len(), 1);
        let arguments = parse_prompt_arguments(&prompts[0], "src/main.rs").unwrap();
        assert_eq!(client.get_prompt("review", &arguments).await.unwrap(), "Please review src/main.rs");
        let refreshed: Vec<String> = server_tools(&client).await.unwrap().iter().map(|t| t.definition().name).collect();
        assert!(refreshed.contains(&"mcp__remote__mul".to_string()));

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0], ("initialize".into(), String::new(), String::new()));
        assert!(seen[1..].iter().all(|(_, session, version)| session == "s-1" && version == "2025-06-18"), "{seen:?}");
        assert!(seen.iter().any(|(method, _, _)| method == "reply"), "ping answered: {seen:?}");
    }

    /// One HTTP request as the mock sees it.
    struct Req {
        method: String,
        path: String,
        headers: HashMap<String, String>,
        body: Value,
    }

    enum Reply {
        Raw(String),
        /// An SSE response kept open, fed with ready-made event text until the sender drops.
        Stream(tokio::sync::mpsc::UnboundedReceiver<String>),
    }

    type Handler = Arc<dyn Fn(Req) -> Reply + Send + Sync>;

    async fn serve(handler: Handler) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head, body) = loop {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buffer).to_string();
                        if let Some((head, body)) = text.split_once("\r\n\r\n") {
                            let length = head
                                .lines()
                                .find_map(|l| {
                                    l.to_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse().unwrap_or(0))
                                })
                                .unwrap_or(0usize);
                            if body.len() >= length {
                                break (head.to_string(), body.to_string());
                            }
                        }
                    };
                    let mut lines = head.lines();
                    let mut first = lines.next().unwrap_or_default().split_whitespace();
                    let (method, path) =
                        (first.next().unwrap_or_default().to_string(), first.next().unwrap_or_default().to_string());
                    let headers = lines
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                        .collect();
                    let body = serde_json::from_str(&body).unwrap_or(Value::Null);
                    match handler(Req { method, path, headers, body }) {
                        Reply::Raw(raw) => {
                            let _ = socket.write_all(raw.as_bytes()).await;
                        }
                        Reply::Stream(mut events) => {
                            let head =
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
                            let _ = socket.write_all(head.as_bytes()).await;
                            while let Some(event) = events.recv().await {
                                if socket.write_all(event.as_bytes()).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{addr}/mcp")
    }

    fn json_reply(body: &Value, extra: &str) -> Reply {
        let body = body.to_string();
        Reply::Raw(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
    }

    fn status(code: &str) -> Reply {
        Reply::Raw(format!("HTTP/1.1 {code}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"))
    }

    fn init_result(id: &Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "result": {
            "protocolVersion": "2025-06-18", "capabilities": { "tools": {} }, "serverInfo": { "name": "mock", "version": "1" }
        } })
    }

    fn tools_result(id: &Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [{ "name": "add", "inputSchema": { "type": "object" } }] } })
    }

    #[tokio::test]
    async fn an_expired_session_is_reinitialized_and_the_request_retried() {
        let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sessions = Arc::new(AtomicU64::new(0));
        let expired_once = Arc::new(AtomicBool::new(false));
        let (seen, made, once) = (log.clone(), sessions.clone(), expired_once.clone());
        let url = serve(Arc::new(move |req: Req| {
            let session = req.headers.get("mcp-session-id").cloned().unwrap_or_default();
            if req.method == "GET" {
                return status("405 Method Not Allowed");
            }
            let method = req.body["method"].as_str().unwrap_or("reply").to_string();
            seen.lock().unwrap().push(format!("{method} {session}"));
            match method.as_str() {
                "initialize" => {
                    let n = made.fetch_add(1, Ordering::SeqCst) + 1;
                    json_reply(&init_result(&req.body["id"]), &format!("Mcp-Session-Id: s-{n}\r\n"))
                }
                "tools/list" if session == "s-1" && !once.swap(true, Ordering::SeqCst) => status("404 Not Found"),
                "tools/list" => json_reply(&tools_result(&req.body["id"]), ""),
                _ => status("202 Accepted"),
            }
        }))
        .await;
        let config = McpServerConfig { url: Some(url), ..Default::default() };
        let client = McpClient::connect("remote", &config).await.unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools[0]["name"], "add");
        let log = log.lock().unwrap().clone();
        assert_eq!(
            log,
            [
                "initialize ",
                "notifications/initialized s-1",
                "tools/list s-1",
                "initialize ",
                "notifications/initialized s-2",
                "tools/list s-2"
            ],
            "a new session, then the same request once more"
        );
    }

    #[tokio::test]
    async fn the_get_stream_delivers_notifications_and_resumes() {
        let gets = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = gets.clone();
        let url = serve(Arc::new(move |req: Req| {
            if req.method == "GET" {
                let mut gets = seen.lock().unwrap();
                gets.push(req.headers.get("last-event-id").cloned().unwrap_or_default());
                if gets.len() > 1 {
                    return status("405 Method Not Allowed");
                }
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let note = json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" });
                tx.send(format!("retry: 50\nid: 7\nevent: message\ndata: {note}\n\n")).unwrap();
                return Reply::Stream(rx);
            }
            match req.body["method"].as_str() {
                Some("initialize") => json_reply(&init_result(&req.body["id"]), "Mcp-Session-Id: s-1\r\n"),
                _ => status("202 Accepted"),
            }
        }))
        .await;
        let config = McpServerConfig { url: Some(url), ..Default::default() };
        let client = McpClient::connect("remote", &config).await.unwrap();
        for _ in 0..100 {
            if gets.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(client.take_tools_changed(), "a notification outside any request");
        assert_eq!(gets.lock().unwrap().clone(), ["", "7"], "the reconnect resumes after the last event");
    }

    #[tokio::test]
    async fn a_broken_response_stream_resumes_where_it_stopped() {
        let resumed = Arc::new(std::sync::Mutex::new(None::<String>));
        let asked = Arc::new(std::sync::Mutex::new(Value::Null));
        let (seen, request) = (resumed.clone(), asked.clone());
        let url = serve(Arc::new(move |req: Req| {
            if req.method == "GET" {
                let Some(last) = req.headers.get("last-event-id").cloned() else {
                    return status("405 Method Not Allowed");
                };
                *seen.lock().unwrap() = Some(last);
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let id = request.lock().unwrap().clone();
                tx.send(format!("id: 4\nevent: message\ndata: {}\n\n", tools_result(&id))).unwrap();
                return Reply::Stream(rx);
            }
            match req.body["method"].as_str() {
                Some("initialize") => json_reply(&init_result(&req.body["id"]), "Mcp-Session-Id: s-1\r\n"),
                Some("tools/list") => {
                    *request.lock().unwrap() = req.body["id"].clone();
                    // A progress event, then the stream drops before the response.
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    let note = json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": {} });
                    tx.send(format!("retry: 10\nid: 3\nevent: message\ndata: {note}\n\n")).unwrap();
                    Reply::Stream(rx)
                }
                _ => status("202 Accepted"),
            }
        }))
        .await;
        let config = McpServerConfig { url: Some(url), ..Default::default() };
        let client = McpClient::connect("remote", &config).await.unwrap();
        assert_eq!(client.list_tools().await.unwrap()[0]["name"], "add");
        assert_eq!(resumed.lock().unwrap().as_deref(), Some("3"));
    }

    #[tokio::test]
    async fn a_legacy_sse_server_is_found_and_reconnected() {
        let streams = Arc::new(std::sync::Mutex::new(Vec::<tokio::sync::mpsc::UnboundedSender<String>>::new()));
        let posts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let opened = Arc::new(AtomicU64::new(0));
        let (open, seen) = (streams.clone(), posts.clone());
        let url = serve(Arc::new(move |req: Req| match (req.method.as_str(), req.path.as_str()) {
            ("POST", "/mcp") => status("405 Method Not Allowed"),
            ("GET", "/mcp") => {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let sid = opened.fetch_add(1, Ordering::SeqCst) + 1;
                tx.send(format!("event: endpoint\ndata: /messages?sid={sid}\n\n")).unwrap();
                open.lock().unwrap().push(tx);
                Reply::Stream(rx)
            }
            ("POST", path) => {
                seen.lock().unwrap().push(format!("{path} {}", req.body["method"].as_str().unwrap_or("reply")));
                let reply = match req.body["method"].as_str() {
                    Some("initialize") => Some(init_result(&req.body["id"])),
                    Some("tools/list") => Some(tools_result(&req.body["id"])),
                    _ => None,
                };
                if let Some(reply) = reply
                    && let Some(stream) = open.lock().unwrap().last()
                {
                    let _ = stream.send(format!("event: message\ndata: {reply}\n\n"));
                }
                status("202 Accepted")
            }
            _ => status("404 Not Found"),
        }))
        .await;
        let config = McpServerConfig { url: Some(url), ..Default::default() };
        let client = McpClient::connect("old", &config).await.unwrap();
        assert_eq!(client.list_tools().await.unwrap()[0]["name"], "add");

        streams.lock().unwrap().remove(0);
        for _ in 0..100 {
            if let Transport::Legacy(legacy) = &client.transport
                && !legacy.open.load(Ordering::SeqCst)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(client.list_tools().await.unwrap()[0]["name"], "add", "a new stream and session");
        let posts = posts.lock().unwrap().clone();
        assert_eq!(
            posts,
            [
                "/messages?sid=1 initialize",
                "/messages?sid=1 notifications/initialized",
                "/messages?sid=1 tools/list",
                "/messages?sid=2 initialize",
                "/messages?sid=2 notifications/initialized",
                "/messages?sid=2 tools/list"
            ]
        );
    }

    #[test]
    fn expands_secrets_in_headers() {
        // SAFETY: test-only variable with a unique name; no other thread reads it.
        unsafe { std::env::set_var("NYPI_TEST_MCP_TOKEN", "abc") };
        assert_eq!(crate::config::expand_secrets("Bearer ${NYPI_TEST_MCP_TOKEN}").unwrap(), "Bearer abc");
        assert!(crate::config::expand_secrets("${NYPI_TEST_MCP_UNSET_VAR}").is_err());
        assert_eq!(crate::config::expand_secrets("plain").unwrap(), "plain");
    }

    #[test]
    fn parses_prompt_arguments() {
        let prompt = |names: &[(&str, bool)]| McpPrompt {
            server: "s".into(),
            name: "p".into(),
            description: String::new(),
            arguments: names
                .iter()
                .map(|(n, r)| McpPromptArgument { name: n.to_string(), description: String::new(), required: *r })
                .collect(),
        };
        let two = prompt(&[("file", true), ("focus", false)]);
        let parsed = parse_prompt_arguments(&two, r#"file=a.rs focus="error handling""#).unwrap();
        assert_eq!(parsed["focus"], "error handling");
        assert!(parse_prompt_arguments(&two, "a.rs").unwrap_err().contains("key=value"));
        assert!(parse_prompt_arguments(&two, "focus=x").unwrap_err().contains("Missing required argument(s): file"));
        let one = prompt(&[("topic", true)]);
        assert_eq!(parse_prompt_arguments(&one, "the release plan").unwrap()["topic"], "the release plan");
        assert!(parse_prompt_arguments(&prompt(&[]), "").unwrap().is_empty());
    }
}
