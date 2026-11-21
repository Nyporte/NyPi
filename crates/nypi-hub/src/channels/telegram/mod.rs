//! Telegram as a channel inside the daemon (decision D66, step 1). A paired chat talks to the
//! same Lucia as every interface on the daemon's socket: the channel turns a chat's messages
//! into commands on the daemon's command stream and reads the same record stream back. It is
//! not a process and holds no runtime. Where no daemon runs, `nypi telegram` starts one in the
//! foreground with this channel on.
//!
//! What a chat is sent: a run the chat started or spoke into is relayed in full (tool calls in
//! batches, each reply, how it ended); a run started elsewhere is not, and reaches the chat only
//! as the notice everything gets when nobody watches. An approval question always goes to the
//! paired chats, whoever started the run, and other NyPi processes' questions arrive here too
//! (the bridge socket).
//!
//! A paired chat speaks as the owner, so pairing is guarded: the code is shown only on the
//! machine (`nypi telegram`), works once, is replaced after five wrong tries with an alert, and
//! pairing closes after fifteen until the daemon starts again.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nypi_core::config::Paths;
use nypi_core::inspector::{Alert, AlertLevel};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::bridge::{self, BridgeDecision, RemoteApproval};
use crate::daemon::{CHANNEL_ID, Port, log};
use api::{Access, Answer, Incoming, TelegramApi, approval_answer, recv_remote, spawn_poller};

pub mod api;

/// The name the channel has in the daemon's heartbeat and on the board's map.
pub const NAME: &str = "telegram";
/// Tool calls gathered before they are sent as one message.
const TOOL_LOG_LINES: usize = 8;
/// Wrong codes before the code is replaced, and before pairing closes.
const TRIES_PER_CODE: u32 = 5;
const TRIES_IN_ALL: u32 = 15;

/// What the channel needs besides its port.
pub struct Setup {
    pub api: TelegramApi,
    pub access_path: PathBuf,
    pub code_path: PathBuf,
    /// Approval requests from other NyPi processes.
    pub remote: Option<mpsc::UnboundedReceiver<RemoteApproval>>,
    pub agent_name: String,
    /// The first pairing code; None makes one.
    pub code: Option<String>,
}

fn code_file(paths: &Paths) -> PathBuf {
    paths.home.join("run").join("telegram.code")
}

/// The channel's setup when a bot token is set and no bridge of its own is running. Must be
/// called inside the daemon's runtime: it starts listening for other processes' approvals.
pub fn setup(paths: &Paths, settings: &nypi_core::config::Settings) -> Option<Setup> {
    let agent_name = &settings.agent_name;
    let api = match TelegramApi::from_settings(paths, settings) {
        Ok(api) => api?,
        Err(why) => {
            log(&format!("Telegram is off: {why}"));
            return None;
        }
    };
    let remote = match bridge::listen(&paths.bridge_socket()) {
        Ok(remote) => remote,
        Err(error) => {
            log(&format!("Telegram stays with the bridge that is already running ({error:#})"));
            return None;
        }
    };
    Some(Setup {
        api,
        access_path: paths.telegram_access(),
        code_path: code_file(paths),
        remote: Some(remote),
        agent_name: agent_name.to_string(),
        code: None,
    })
}

/// Take away what a stopped channel left in the run folder.
pub fn remove(paths: &Paths) {
    let _ = std::fs::remove_file(code_file(paths));
    let _ = std::fs::remove_file(paths.bridge_socket());
}

/// `nypi telegram` while the daemon runs the channel: how to pair a chat. None when it does not.
pub fn pairing_help(paths: &Paths) -> Option<String> {
    let beat = crate::daemon::heartbeat(paths).filter(|_| crate::daemon::alive(paths, chrono::Utc::now()))?;
    if !beat.channels.iter().any(|channel| channel == NAME) {
        return None;
    }
    let chats = Access::load(&paths.telegram_access()).allowed_chat_ids;
    let mut text = format!("Telegram is a channel of the running hub (pid {}).\n", beat.pid);
    text.push_str(&match chats.len() {
        0 => "No chat is paired yet.\n".to_string(),
        count => format!("Paired chats: {count}.\n"),
    });
    match std::fs::read_to_string(code_file(paths)).ok().filter(|code| !code.trim().is_empty()) {
        Some(code) => text.push_str(&format!("To pair a chat, send the bot:  /pair {}", code.trim())),
        None => text.push_str("Pairing is closed after too many wrong codes. It opens again when the hub restarts."),
    }
    Some(text)
}

fn new_code() -> String {
    format!("{:06}", uuid::Uuid::new_v4().as_u128() % 1_000_000)
}

/// What a `/pair` attempt came to.
#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    Paired,
    Wrong,
    /// Wrong, and the code was replaced: the owner should hear.
    WrongAndReplaced,
    /// Wrong, and pairing is closed from now on.
    WrongAndClosed,
    Closed,
}

/// The pairing code and how many wrong tries it has seen.
struct Pairing {
    file: PathBuf,
    /// None once pairing is closed.
    code: Option<String>,
    wrong: u32,
}

impl Pairing {
    fn open(file: PathBuf, code: Option<String>) -> Self {
        let mut pairing = Pairing { file, code: None, wrong: 0 };
        pairing.set(code.unwrap_or_else(new_code));
        pairing
    }

    fn replace(&mut self) {
        self.set(new_code());
    }

    /// Take `code` as the current one, written where only the owner (and never a tool) can read it.
    fn set(&mut self, code: String) {
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(error) = std::fs::write(&self.file, &code) {
            log(&format!("Telegram pairing code not written: {error}"));
        }
        self.code = Some(code);
    }

    fn attempt(&mut self, given: &str) -> Attempt {
        let Some(code) = &self.code else { return Attempt::Closed };
        if given == code {
            // A code pairs one chat; the next chat needs the next code.
            self.replace();
            return Attempt::Paired;
        }
        self.wrong += 1;
        if self.wrong >= TRIES_IN_ALL {
            self.code = None;
            let _ = std::fs::remove_file(&self.file);
            Attempt::WrongAndClosed
        } else if self.wrong.is_multiple_of(TRIES_PER_CODE) {
            self.replace();
            Attempt::WrongAndReplaced
        } else {
            Attempt::Wrong
        }
    }
}

/// An approval question put to the chats.
enum Ask {
    /// One of the daemon's own, by its id in the record stream.
    Hub(String),
    /// Another NyPi process's.
    Remote(RemoteApproval),
}

struct Channel {
    port: Port,
    api: TelegramApi,
    access: Access,
    access_path: PathBuf,
    pairing: Pairing,
    agent_name: String,
    /// The chat the run under way is relayed to: the one that started it or spoke into it.
    following: Option<i64>,
    /// Messages sent as commands and not answered yet, by command id.
    asked: HashMap<String, i64>,
    asks: Vec<(u64, Ask)>,
    next_ask: u64,
    tool_log: Vec<String>,
    busy: bool,
}

impl Channel {
    async fn everyone(&self, text: &str) {
        for chat in &self.access.allowed_chat_ids {
            self.api.send_text(*chat, text).await;
        }
    }

    async fn flush_log(&mut self) {
        if let (Some(chat), false) = (self.following, self.tool_log.is_empty()) {
            let text = std::mem::take(&mut self.tool_log).join("\n");
            self.api.send_text(chat, &text).await;
        }
    }

    fn waiting(&self, ask: &Ask) -> bool {
        match ask {
            Ask::Hub(id) => self.port.approval_waiting(id),
            Ask::Remote(request) => request.is_waiting(),
        }
    }

    /// Put a question to every paired chat under the next number.
    async fn ask(&mut self, ask: Ask, body: String) {
        self.next_ask += 1;
        let id = self.next_ask;
        self.asks.push((id, ask));
        self.everyone(&format!(
            "Approval #{id} needed{body}\n\nReply /approve {id}, or /deny {id} with an optional reason."
        ))
        .await;
    }

    /// Apply an answer to the named question, or to the oldest one still waiting.
    fn answer(&mut self, answer: Answer) -> String {
        let asks = std::mem::take(&mut self.asks);
        self.asks = asks.into_iter().filter(|(_, ask)| self.waiting(ask)).collect();
        let index = match answer.id {
            Some(id) => self.asks.iter().position(|(item, _)| *item == id),
            None => (!self.asks.is_empty()).then_some(0),
        };
        let Some(index) = index else {
            return match answer.id {
                Some(id) => format!("#{id} is not waiting (already answered, timed out, or cancelled)."),
                None => "Nothing is waiting for approval.".into(),
            };
        };
        let (id, ask) = self.asks.remove(index);
        let delivered = match ask {
            Ask::Hub(approval) => self.port.send(json!({
                "type": "approval_response", "id": approval, "approved": answer.approve, "reason": answer.reason,
            })),
            Ask::Remote(request) => request.answer(BridgeDecision { approved: answer.approve, reason: answer.reason }),
        };
        match (delivered, answer.approve) {
            (false, _) => format!("#{id} stopped waiting before your answer arrived."),
            (true, true) => format!("Approved #{id}."),
            (true, false) => format!("Denied #{id}."),
        }
    }

    async fn unpaired(&mut self, message: &Incoming) {
        let inspector = self.port.inspector().clone();
        let Some(given) = message.text.trim().strip_prefix("/pair").map(str::trim) else {
            inspector.record(NAME, json!({ "rejected": message.chat_id, "from": message.from }));
            // Nothing about the agent is told to a chat that is not paired.
            let text = "This chat is not paired. Send /pair <code>; the machine NyPi runs on shows the code with: nypi telegram";
            self.api.send_text(message.chat_id, text).await;
            return;
        };
        let attempt = self.pairing.attempt(given);
        inspector
            .record(NAME, json!({ "pairing": format!("{attempt:?}"), "chat": message.chat_id, "from": message.from }));
        let said = match attempt {
            Attempt::Paired => {
                let first = self.access.allowed_chat_ids.is_empty();
                self.access.allowed_chat_ids.insert(message.chat_id);
                match self.access.save(&self.access_path) {
                    Ok(()) => {
                        if first {
                            self.port.asks_approvals(true);
                        }
                        log("a Telegram chat was paired");
                        format!("Paired. I'm {}; what should we work on?", self.agent_name)
                    }
                    Err(error) => {
                        self.access.allowed_chat_ids.remove(&message.chat_id);
                        format!("Pairing failed: {error}")
                    }
                }
            }
            Attempt::WrongAndReplaced => {
                let text = "Five wrong Telegram pairing codes were sent to your bot; the code was replaced (nypi telegram shows the new one).";
                inspector.alert(Alert::new(AlertLevel::Warning, text));
                "That code is not right.".into()
            }
            Attempt::WrongAndClosed => {
                let text = "Fifteen wrong Telegram pairing codes were sent to your bot; pairing is closed until the hub restarts.";
                inspector.alert(Alert::new(AlertLevel::Error, text));
                "That code is not right.".into()
            }
            Attempt::Wrong | Attempt::Closed => "That code is not right.".into(),
        };
        self.api.send_text(message.chat_id, &said).await;
    }

    async fn message(&mut self, message: Incoming) {
        if !self.access.allowed_chat_ids.contains(&message.chat_id) {
            return self.unpaired(&message).await;
        }
        let chat = message.chat_id;
        let text = message.text.trim().to_string();
        self.port.inspector().record(NAME, json!({ "chat": chat, "from": message.from, "input": text }));
        if let Some(answer) = approval_answer(&text) {
            let reply = self.answer(answer);
            return self.api.send_text(chat, &reply).await;
        }
        // A join request's answer (decision D93): the file of paired chats is in the NyPi home.
        let home = self.access_path.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
        if let Some(reply) = crate::commands::join_command(&Paths { home }, &text) {
            return self.api.send_text(chat, &reply).await;
        }
        let reply = match text.as_str() {
            "" => return,
            "/start" => format!("Hi, I'm {}. Send me a task or a question. /help lists commands.", self.agent_name),
            "/abort" | "/stop" if self.busy => {
                self.port
                    .send(json!({ "id": format!("{CHANNEL_ID}{}", uuid::Uuid::now_v7().simple()), "type": "abort" }));
                "Stopping.".into()
            }
            "/abort" | "/stop" => "Nothing is running.".into(),
            // Behind chat, a command waits for the task to end by itself.
            _ if text.starts_with('/') && self.busy && !self.port.chat() => {
                "Commands wait until the run ends; /abort stops it, plain text steers it.".into()
            }
            _ => {
                // It starts a run when she is idle and steers the run when she is not.
                let id = format!("{CHANNEL_ID}{}", uuid::Uuid::now_v7().simple());
                self.asked.insert(id.clone(), chat);
                let command = json!({ "id": id, "type": "prompt", "message": text, "streamingBehavior": "steer" });
                if self.port.send(command) {
                    return;
                }
                "The hub is stopping; send that again in a moment.".into()
            }
        };
        self.api.send_text(chat, &reply).await;
    }

    /// The daemon's answer to a message this channel sent as a command.
    async fn response(&mut self, record: &Value) {
        let Some(chat) = record["id"].as_str().and_then(|id| self.asked.remove(id)) else { return };
        if record["success"] != true {
            let why = record["error"].as_str().unwrap_or("the hub gave no reason");
            return self.api.send_text(chat, &format!("Not taken: {why}")).await;
        }
        match record["data"]["disposition"].as_str() {
            Some("handled") => {
                let output = record["data"]["output"].as_str().unwrap_or_default();
                self.api.send_text(chat, if output.trim().is_empty() { "Done." } else { output }).await;
            }
            Some("queued") => {
                self.following.get_or_insert(chat);
                self.api.send_text(chat, "Got it. I'll take that into account right after the current step.").await;
            }
            _ => self.following = Some(chat),
        }
    }

    async fn record(&mut self, record: Value) {
        match record["type"].as_str().unwrap_or_default() {
            "response" => self.response(&record).await,
            "agent_start" if record["module"] != "chat" => self.busy = true,
            // Chat answered and no task runs: this exchange is over.
            "chat_settled" if record["working"] != true => {
                self.flush_log().await;
                self.following = None;
            }
            "chat_failed" => {
                if let Some(chat) = self.following {
                    let error = record["error"].as_str().unwrap_or("unknown error");
                    let next =
                        if record["delegated"] == true { " Your message went straight to the worker." } else { "" };
                    self.api.send_text(chat, &format!("The chat model did not answer ({error}).{next}")).await;
                }
                if record["working"] != true {
                    self.following = None;
                }
            }
            "approval_request" => {
                self.flush_log().await;
                let body = format!(
                    ".\n{}: {}\nWhy approval is needed: {}",
                    record["tool"].as_str().unwrap_or("?"),
                    record["summary"].as_str().unwrap_or_default(),
                    record["reason"].as_str().unwrap_or_default()
                );
                self.ask(Ask::Hub(record["id"].as_str().unwrap_or_default().to_string()), body).await;
            }
            // A task beside the main one is the pool's; its steps are not relayed.
            _ if record["lane"].is_string() => {}
            // The principal reads this report first: its words, or the report it lets stand
            // (`task_report`), are what the chat hears.
            "agent_settled" if record["review"] == "pending" => {
                self.busy = false;
                self.asks.retain(|(_, ask)| matches!(ask, Ask::Remote(_)));
                self.flush_log().await;
            }
            "agent_settled" | "task_report" => {
                self.busy = false;
                self.asks.retain(|(_, ask)| matches!(ask, Ask::Remote(_)));
                self.flush_log().await;
                if let Some(chat) = self.following.take() {
                    // Behind chat the worker's words were held back; its report is shown whole (D19).
                    if let (true, Some(answer)) = (self.port.chat(), record["answer"].as_str())
                        && !answer.trim().is_empty()
                    {
                        self.api.send_text(chat, answer.trim()).await;
                    }
                    if let Some(sources) = record["sources"].as_str() {
                        self.api.send_text(chat, sources).await;
                    }
                    if let Some(note) = crate::records::settled_note(&record) {
                        self.api.send_text(chat, &note).await;
                    }
                }
            }
            // Chat's own tool calls only hand work over; the worker's are the work.
            kind @ ("tool_execution_start" | "tool_execution_end")
                if self.following.is_some() && record["module"] != "chat" =>
            {
                if let Some(line) = crate::records::show(&record, false) {
                    self.tool_log.push(line);
                }
                if kind == "tool_execution_start" && self.tool_log.len() >= TOOL_LOG_LINES {
                    self.flush_log().await;
                }
            }
            // Behind chat, what the worker writes on the way waits for its report.
            "message_end" if record["module"] == "worker" => {}
            "message_end" | "auto_retry_start" | "compaction_start" => {
                if let (Some(chat), Some(text)) = (self.following, crate::records::show(&record, true)) {
                    self.flush_log().await;
                    self.api.send_text(chat, &text).await;
                }
            }
            _ => {}
        }
    }
}

/// Run the channel until `shutdown`.
/// What the hub asks of the channel while it runs (`channels::Channel`).
enum Request {
    Notice(String, tokio::sync::oneshot::Sender<bool>),
    Ask(crate::channels::ApprovalAsk, tokio::sync::oneshot::Sender<bool>),
}

/// Telegram as one of the hub's channels (decision D81): the chats the owner paired talk to
/// the conversation, hear notices, and answer approvals.
pub struct TelegramChannel {
    setup: Mutex<Option<Setup>>,
    requests: mpsc::UnboundedSender<Request>,
    waiting: Mutex<Option<mpsc::UnboundedReceiver<Request>>>,
}

impl TelegramChannel {
    pub fn new(setup: Setup) -> TelegramChannel {
        let (requests, waiting) = mpsc::unbounded_channel();
        TelegramChannel { setup: Mutex::new(Some(setup)), requests, waiting: Mutex::new(Some(waiting)) }
    }

    /// Ask the running channel for something; false when it is not running or does not answer.
    async fn request(&self, make: impl FnOnce(tokio::sync::oneshot::Sender<bool>) -> Request) -> bool {
        // Not started: there is nobody to pass it on.
        if self.waiting.lock().unwrap().is_some() {
            return false;
        }
        let (answer, answered) = tokio::sync::oneshot::channel();
        if self.requests.send(make(answer)).is_err() {
            return false;
        }
        tokio::time::timeout(Duration::from_secs(20), answered).await.ok().and_then(Result::ok).unwrap_or(false)
    }
}

#[async_trait::async_trait]
impl crate::channels::Channel for TelegramChannel {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn run(self: Arc<Self>, port: Port, shutdown: CancellationToken) {
        let (setup, requests) = (self.setup.lock().unwrap().take(), self.waiting.lock().unwrap().take());
        if let (Some(setup), Some(requests)) = (setup, requests) {
            serve_with(port, setup, shutdown, requests).await;
        }
    }

    async fn deliver_notice(&self, text: &str) -> bool {
        self.request(|answer| Request::Notice(text.to_string(), answer)).await
    }

    async fn ask_approval(&self, ask: &crate::channels::ApprovalAsk) -> bool {
        self.request(|answer| Request::Ask(ask.clone(), answer)).await
    }
}

pub async fn serve(port: Port, setup: Setup, shutdown: CancellationToken) {
    // Nobody asks this one for anything: the sender is kept so the stream stays open.
    let (_requests, none) = mpsc::unbounded_channel();
    serve_with(port, setup, shutdown, none).await
}

async fn serve_with(
    port: Port,
    setup: Setup,
    shutdown: CancellationToken,
    mut requests: mpsc::UnboundedReceiver<Request>,
) {
    let Setup { api, access_path, code_path, mut remote, agent_name, code } = setup;
    match api.call("getMe", json!({})).await {
        Ok(me) => log(&format!("Telegram bot @{} answers", me["username"].as_str().unwrap_or("?"))),
        Err(error) => {
            // Without the bot there is no channel; everything else in the daemon goes on.
            return log(&format!("Telegram is off: the bot did not answer ({error:#}); check TELEGRAM_BOT_TOKEN"));
        }
    }
    let access = Access::load(&access_path);
    port.joined(NAME);
    if !access.allowed_chat_ids.is_empty() {
        port.asks_approvals(true);
    }
    let mut records = port.records();
    let mut incoming = spawn_poller(api.clone(), shutdown.clone());
    let busy = port.busy();
    let mut channel = Channel {
        port,
        api,
        access,
        access_path,
        pairing: Pairing::open(code_path, code),
        agent_name,
        following: None,
        asked: HashMap::new(),
        asks: Vec::new(),
        next_ask: 0,
        tool_log: Vec::new(),
        busy,
    };
    let mut typing = tokio::time::interval(Duration::from_secs(4));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            message = incoming.recv() => match message {
                Some(message) => channel.message(message).await,
                None => break,
            },
            record = records.recv() => match record {
                Ok(line) => {
                    if let Ok(record) = serde_json::from_str::<Value>(&line) {
                        channel.record(record).await;
                    }
                }
                // Fell behind: what was missed is in the session; go on from here.
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            Some(request) = recv_remote(&mut remote) => {
                let body = format!(
                    " (from {}).\n{}: {}\nWhy approval is needed: {}",
                    request.origin, request.tool, request.summary, request.reason
                );
                channel.ask(Ask::Remote(request), body).await;
            }
            Some(request) = requests.recv() => {
                // Only to chats the owner paired; with none there is nobody to tell.
                let paired = !channel.access.allowed_chat_ids.is_empty();
                match request {
                    Request::Notice(text, answer) => {
                        if paired {
                            channel.everyone(&text).await;
                        }
                        let _ = answer.send(paired);
                    }
                    Request::Ask(ask, answer) => {
                        if paired {
                            let body = format!(".\n{}: {}\nWhy approval is needed: {}", ask.tool, ask.summary, ask.reason);
                            channel.ask(Ask::Hub(ask.id), body).await;
                        }
                        let _ = answer.send(paired);
                    }
                }
            }
            _ = typing.tick() => {
                if let Some(chat) = channel.following {
                    channel.api.call_quiet("sendChatAction", json!({ "chat_id": chat, "action": "typing" })).await;
                }
            }
        }
    }
    if !channel.access.allowed_chat_ids.is_empty() {
        channel.port.asks_approvals(false);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use nypi_ai::providers::faux::{FauxProvider, FauxResponse};
    use nypi_core::outbox::Outbox;
    use tokio::net::UnixListener;

    use super::api::tests::{mock_bot_api, update};
    use super::*;
    use crate::daemon::tests::{Client, runtime};

    type Sent = Arc<Mutex<Vec<(i64, String)>>>;

    fn write_code(file: &Path, code: &str) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, code).unwrap();
    }

    async fn until(sent: &Sent, what: &str) {
        for _ in 0..100 {
            if sent.lock().unwrap().iter().any(|(_, text)| text.contains(what)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("Telegram never got {what:?}: {:?}", sent.lock().unwrap());
    }

    fn setup_for(home: &Path, base: &str, code: &str) -> Setup {
        Setup {
            api: TelegramApi::new(base, "T"),
            access_path: home.join("telegram.json"),
            code_path: home.join("run/telegram.code"),
            remote: None,
            agent_name: "Lucia".into(),
            code: Some(code.into()),
        }
    }

    #[test]
    fn a_code_pairs_once_is_replaced_after_five_wrong_tries_and_pairing_closes_after_fifteen() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("run/telegram.code");
        let mut pairing = Pairing::open(file.clone(), None);
        let first = pairing.code.clone().unwrap();
        assert_eq!((first.len(), std::fs::read_to_string(&file).unwrap()), (6, first.clone()));

        assert_eq!(pairing.attempt(&first), Attempt::Paired);
        let second = pairing.code.clone().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), second, "the next chat needs the next code");
        if first != second {
            assert_eq!(pairing.attempt(&first), Attempt::Wrong, "a used code is dead");
        } else {
            pairing.wrong = 1;
        }

        let wrong = |pairing: &mut Pairing| pairing.attempt("not it");
        for _ in 0..3 {
            assert_eq!(wrong(&mut pairing), Attempt::Wrong);
        }
        assert_eq!(wrong(&mut pairing), Attempt::WrongAndReplaced, "the fifth wrong try");
        for _ in 0..9 {
            assert_ne!(wrong(&mut pairing), Attempt::Paired);
        }
        assert_eq!(wrong(&mut pairing), Attempt::WrongAndClosed, "the fifteenth");
        assert!(!file.exists(), "no code to read once pairing is closed");
        assert_eq!(pairing.attempt("123456"), Attempt::Closed);
    }

    /// Behind the trait, the hub can tell the owner something and ask them something on
    /// Telegram without knowing it is Telegram; a chat nobody paired hears neither.
    #[tokio::test]
    async fn as_a_channel_it_carries_a_notice_and_an_approval_to_paired_chats_only() {
        use crate::channels::{ApprovalAsk, Channel, Channels};
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let faux = FauxProvider::new(vec![]);
        let sent: Sent = Arc::new(Mutex::new(Vec::new()));
        let base = mock_bot_api(vec![("", update(1, 42, "/pair 123456"))], sent.clone()).await;
        let channel = Arc::new(TelegramChannel::new(setup_for(home.path(), &base, "123456")));
        assert_eq!(channel.name(), "telegram");
        assert!(!channel.deliver_notice("too early").await, "not running: nobody to tell");

        let primary = crate::daemon::PrimaryStream::new(runtime(home.path(), work.path(), &faux));
        let shutdown = CancellationToken::new();
        let channels = Channels::of(vec![channel.clone()]);
        let running = channels.spawn(&primary.thread().port(), &shutdown);
        until(&sent, "Paired").await;

        assert_eq!(channels.deliver_notice("The backup finished.").await, ["telegram"]);
        let ask = ApprovalAsk {
            id: "a1b2".into(),
            tool: "bash".into(),
            summary: "git push origin main".into(),
            reason: "pushing to a remote repository".into(),
        };
        assert_eq!(channels.ask_approval(&ask).await, ["telegram"]);
        until(&sent, "git push origin main").await;
        let sent = sent.lock().unwrap().clone();
        assert!(sent.iter().any(|(chat, text)| *chat == 42 && text == "The backup finished."), "{sent:?}");
        assert!(sent.iter().all(|(chat, _)| *chat == 42), "only the paired chat: {sent:?}");
        shutdown.cancel();
        for task in running {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_paired_chat_talks_to_the_daemons_lucia_and_a_stranger_learns_nothing() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let faux = FauxProvider::new(vec![
            FauxResponse::tool_call("t1", "bash", json!({ "command": "echo hi" })),
            FauxResponse::text("Hello from Lucia.\n\nDone: greeted you."),
        ]);
        let sent: Sent = Arc::new(Mutex::new(Vec::new()));
        let updates = vec![
            ("", update(1, 666, "hello?")),
            ("", update(2, 666, "/pair 000000")),
            ("", update(3, 42, "/pair 123456")),
            ("Paired", update(4, 42, "hi Lucia")),
        ];
        let base = mock_bot_api(updates, sent.clone()).await;
        let socket = home.path().join("d.sock");
        let shutdown = CancellationToken::new();
        let daemon = tokio::spawn(crate::daemon::serve(
            runtime(home.path(), work.path(), &faux),
            UnixListener::bind(&socket).unwrap(),
            shutdown.clone(),
            Some(setup_for(home.path(), &base, "123456")),
        ));
        until(&sent, "Hello from Lucia").await;
        // The run was relayed to the chat, so it is not also reported as a notice.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let mut terminal = Client::attach(&socket).await;
        assert_eq!(terminal.next_of("daemon_hello").await["channels"], json!(["telegram"]));
        shutdown.cancel();
        daemon.await.unwrap();

        let sent = sent.lock().unwrap().clone();
        let to =
            |chat: i64| sent.iter().filter(|(to, _)| *to == chat).map(|(_, text)| text.as_str()).collect::<Vec<_>>();
        assert!(to(666)[0].starts_with("This chat is not paired"), "{sent:?}");
        assert_eq!(to(666)[1], "That code is not right.");
        assert!(!to(666).iter().any(|text| text.contains("Lucia")), "a stranger learns nothing: {sent:?}");
        let mine = to(42);
        assert_eq!(mine[0], "Paired. I'm Lucia; what should we work on?");
        assert_eq!(mine[1], "· bash echo hi", "tool calls reach the chat that started the run");
        assert_eq!(mine[2], "Hello from Lucia.\n\nDone: greeted you.");
        assert_eq!(
            Access::load(&home.path().join("telegram.json")).allowed_chat_ids.into_iter().collect::<Vec<_>>(),
            [42]
        );
        assert_ne!(
            std::fs::read_to_string(home.path().join("run/telegram.code")).unwrap(),
            "123456",
            "the code worked once"
        );
        let paths = Paths { home: home.path().to_path_buf() };
        assert!(Outbox::new(paths.outbox()).load().is_empty(), "relayed, so not reported twice");
        assert_eq!(faux.requests().len(), 2);
    }

    #[tokio::test]
    async fn an_approval_for_a_run_started_elsewhere_is_answered_from_the_chat() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(home.path().join("telegram.json"), r#"{"allowedChatIds":[42]}"#).unwrap();
        let faux = FauxProvider::new(vec![
            FauxResponse::tool_call("t1", "bash", json!({ "command": "echo would git push" })),
            FauxResponse::text("Pushed.\n\nDone: pushed after approval."),
        ]);
        let sent: Sent = Arc::new(Mutex::new(Vec::new()));
        let updates = vec![
            ("Approval #1 needed", update(1, 42, "/deny 7")),
            ("#7 is not waiting", update(2, 42, "/approve")),
            ("Approved #1", update(3, 42, "/approve")),
        ];
        let base = mock_bot_api(updates, sent.clone()).await;
        let socket = home.path().join("d.sock");
        let shutdown = CancellationToken::new();
        let daemon = tokio::spawn(crate::daemon::serve(
            runtime(home.path(), work.path(), &faux),
            UnixListener::bind(&socket).unwrap(),
            shutdown.clone(),
            Some(setup_for(home.path(), &base, "123456")),
        ));
        let mut terminal = Client::attach(&socket).await;
        // The channel is up before the run starts, or it would miss the question.
        for _ in 0..50 {
            if terminal.next_of("daemon_hello").await["channels"] == json!(["telegram"]) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            terminal = Client::attach(&socket).await;
        }
        terminal.send(json!({ "id": "p", "type": "prompt", "message": "push it" })).await;
        let settled = terminal.next_of("agent_settled").await;
        assert_eq!(settled["status"]["text"], "pushed after approval.");
        until(&sent, "Nothing is waiting").await;
        shutdown.cancel();
        daemon.await.unwrap();

        let sent: Vec<String> = sent.lock().unwrap().iter().map(|(_, text)| text.clone()).collect();
        assert!(
            sent[0].starts_with("Approval #1 needed.\nbash: echo would git push\nWhy approval is needed:"),
            "{sent:?}"
        );
        assert!(sent[0].ends_with("Reply /approve 1, or /deny 1 with an optional reason."));
        assert_eq!(
            sent[1..],
            [
                "#7 is not waiting (already answered, timed out, or cancelled).",
                "Approved #1.",
                "Nothing is waiting for approval."
            ]
        );
        assert!(
            !sent.iter().any(|text| text.contains("Pushed.")),
            "a run started at the terminal is not relayed: {sent:?}"
        );
    }

    #[tokio::test]
    async fn with_the_chat_module_a_chat_gets_her_reply_and_no_notice() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(home.path().join("telegram.json"), r#"{"allowedChatIds":[42]}"#).unwrap();
        std::fs::write(home.path().join("settings.json"), r#"{ "chat": { "model": "faux/faux-1" } }"#).unwrap();
        let faux = FauxProvider::new(vec![FauxResponse::text("Nothing is running; the last task ended at noon.")]);
        let sent: Sent = Arc::new(Mutex::new(Vec::new()));
        let base = mock_bot_api(vec![("", update(1, 42, "anything running?"))], sent.clone()).await;
        let socket = home.path().join("d.sock");
        let shutdown = CancellationToken::new();
        let daemon = tokio::spawn(crate::daemon::serve(
            runtime(home.path(), work.path(), &faux),
            UnixListener::bind(&socket).unwrap(),
            shutdown.clone(),
            Some(setup_for(home.path(), &base, "123456")),
        ));
        until(&sent, "Nothing is running").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown.cancel();
        daemon.await.unwrap();
        let sent: Vec<String> = sent.lock().unwrap().iter().map(|(_, text)| text.clone()).collect();
        assert_eq!(sent, ["Nothing is running; the last task ended at noon."], "her reply, once, and nothing else");
        let paths = Paths { home: home.path().to_path_buf() };
        assert!(Outbox::new(paths.outbox()).load().is_empty(), "relayed, so not reported twice");
    }

    #[test]
    fn nypi_telegram_says_how_to_pair_when_the_daemon_runs_the_channel() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        assert_eq!(pairing_help(&paths), None, "no hub: nypi telegram runs the bridge itself");
        let beat = |channels: Value| {
            let now = chrono::Utc::now().to_rfc3339();
            let beat = json!({ "pid": std::process::id(), "started": now, "beat": now, "clients": 0, "busy": false,
                               "cwd": "/", "channels": channels });
            std::fs::create_dir_all(home.path().join("run")).unwrap();
            std::fs::write(home.path().join("run/hub.json"), beat.to_string()).unwrap();
        };
        beat(json!([]));
        assert_eq!(pairing_help(&paths), None, "a hub without the channel");
        beat(json!(["telegram"]));
        write_code(&code_file(&paths), "654321");
        let help = pairing_help(&paths).unwrap();
        assert!(help.contains("No chat is paired yet.") && help.ends_with("/pair 654321"), "{help}");
        std::fs::remove_file(code_file(&paths)).unwrap();
        assert!(pairing_help(&paths).unwrap().contains("Pairing is closed"));
    }
}
