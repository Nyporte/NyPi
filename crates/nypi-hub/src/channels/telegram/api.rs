//! Telegram's Bot API, as the channel in the daemon (`channels::telegram`) and the notifier use
//! it: the bot token, a small client, the long-poll that turns updates into messages, the file
//! of paired chats, and how a chat's approval answer is read.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use anyhow::bail;
use nypi_ai::auth::lookup_env;
use nypi_core::config::{Paths, Settings};
use nypi_vault::secrets::{Handle, SecretStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bridge::RemoteApproval;

/// Telegram's hard limit is 4096 characters per message.
const MAX_MESSAGE_CHARS: usize = 4000;

pub(crate) async fn recv_remote(
    remote: &mut Option<mpsc::UnboundedReceiver<RemoteApproval>>,
) -> Option<RemoteApproval> {
    match remote {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

pub(crate) struct Answer {
    pub(crate) approve: bool,
    pub(crate) id: Option<u64>,
    pub(crate) reason: Option<String>,
}

/// Parse `/approve [n]`, `/deny [n] [reason]`, and their short forms.
pub(crate) fn approval_answer(text: &str) -> Option<Answer> {
    let (command, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let approve = match command.to_lowercase().as_str() {
        "/approve" | "/yes" | "/y" => true,
        "/deny" | "/no" | "/n" => false,
        _ => return None,
    };
    let rest = rest.trim();
    let (first, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let (id, reason) = match first.trim_start_matches('#').parse::<u64>() {
        Ok(id) => (Some(id), after.trim()),
        Err(_) => (None, rest),
    };
    let reason = (!approve && !reason.is_empty()).then(|| reason.to_string());
    Some(Answer { approve, id, reason })
}

/// A text message from a chat.
#[derive(Clone, Debug)]
pub struct Incoming {
    pub chat_id: i64,
    pub from: String,
    pub text: String,
}

pub(crate) fn spawn_poller(api: TelegramApi, shutdown: CancellationToken) -> mpsc::UnboundedReceiver<Incoming> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut offset: i64 = 0;
        loop {
            let poll =
                api.call("getUpdates", json!({ "offset": offset, "timeout": 50, "allowed_updates": ["message"] }));
            let result = tokio::select! {
                _ = shutdown.cancelled() => return,
                result = poll => result,
            };
            let updates = match result {
                Ok(Value::Array(updates)) => updates,
                Ok(_) => Vec::new(),
                Err(error) => {
                    eprintln!("telegram: getUpdates failed: {error:#}; retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            for update in updates {
                offset = offset.max(update["update_id"].as_i64().unwrap_or(0) + 1);
                let message = &update["message"];
                let (Some(chat_id), Some(text)) = (message["chat"]["id"].as_i64(), message["text"].as_str()) else {
                    continue;
                };
                let from = message["from"]["username"]
                    .as_str()
                    .or(message["from"]["first_name"].as_str())
                    .unwrap_or_default()
                    .to_string();
                if tx.send(Incoming { chat_id, from, text: text.to_string() }).is_err() {
                    return;
                }
            }
        }
    });
    rx
}

/// Minimal Telegram Bot API client.
#[derive(Clone)]
pub struct TelegramApi {
    base: String,
    client: reqwest::Client,
}

/// The bot's token (decision D91): from the secret store when `channels.telegram.botTokenSecret`
/// names a handle, so the token is sealed on disk and scrubbed from everything NyPi writes;
/// otherwise from `TELEGRAM_BOT_TOKEN` (the process environment or env files). `Err` says why a
/// handle that was named could not be used: the channel then stays off.
pub fn bot_token(paths: &Paths, settings: &Settings) -> Result<Option<String>, String> {
    let named = settings.channels.telegram.bot_token_secret.as_deref().map(str::trim).filter(|h| !h.is_empty());
    let Some(named) = named else { return Ok(lookup_env("TELEGRAM_BOT_TOKEN")) };
    let handle = Handle::parse(named).map_err(|error| format!("channels.telegram.botTokenSecret: {error}"))?;
    let missing = || {
        format!("no secret is stored under {handle} (set one with: nypi secrets set {}/{})", handle.scope, handle.name)
    };
    // Without a store there is nothing to read, and no key to make.
    if !paths.secrets().exists() {
        return Err(missing());
    }
    let store = SecretStore::open(&paths.home).map_err(|error| format!("the secret store: {error:#}"))?;
    match store.get(&handle).map_err(|error| format!("the secret store: {error:#}"))? {
        Some(secret) => Ok(Some(secret.expose().to_string())),
        None => Err(missing()),
    }
}

impl TelegramApi {
    /// The client for the bot the settings name, or None without a token. `NYPI_TELEGRAM_API`
    /// overrides the server.
    pub fn from_settings(paths: &Paths, settings: &Settings) -> Result<Option<Self>, String> {
        let base = std::env::var("NYPI_TELEGRAM_API").unwrap_or_else(|_| "https://api.telegram.org".into());
        Ok(bot_token(paths, settings)?.map(|token| TelegramApi::new(&base, &token)))
    }

    pub fn new(base: &str, token: &str) -> Self {
        TelegramApi {
            base: format!("{}/bot{token}", base.trim_end_matches('/')),
            client: reqwest::Client::builder().timeout(Duration::from_secs(90)).build().expect("HTTP client builds"),
        }
    }

    /// Call `method`; returns the `result` field.
    pub async fn call(&self, method: &str, body: Value) -> anyhow::Result<Value> {
        let response = self.client.post(format!("{}/{method}", self.base)).json(&body).send().await?;
        let value: Value = response.json().await?;
        if value["ok"].as_bool() != Some(true) {
            bail!("Telegram {method} failed: {}", value["description"].as_str().unwrap_or("unknown error"));
        }
        Ok(value["result"].clone())
    }

    pub(crate) async fn call_quiet(&self, method: &str, body: Value) {
        let _ = self.call(method, body).await;
    }

    /// Send plain text, split into Telegram-sized chunks. Failures are logged, not fatal.
    pub async fn send_text(&self, chat_id: i64, text: &str) {
        if let Err(error) = self.try_send_text(chat_id, text).await {
            eprintln!("telegram: sendMessage failed: {error:#}");
        }
    }

    /// Send, and say whether Telegram took all of it (the outbox counts a delivery only then).
    pub async fn try_send_text(&self, chat_id: i64, text: &str) -> anyhow::Result<()> {
        for chunk in split_message(text, MAX_MESSAGE_CHARS) {
            self.call("sendMessage", json!({ "chat_id": chat_id, "text": chunk })).await?;
        }
        Ok(())
    }
}

/// Split at line boundaries where possible.
fn split_message(text: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if current.chars().count() + line.chars().count() > max && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        if line.chars().count() > max {
            let chars: Vec<char> = line.chars().collect();
            for piece in chars.chunks(max) {
                chunks.push(piece.iter().collect());
            }
            continue;
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push("(empty)".into());
    }
    chunks
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Access {
    pub allowed_chat_ids: BTreeSet<i64>,
}

impl Access {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
    }

    pub(crate) fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Mock Bot API: serves queued updates once and records sent messages. An update with a
    /// trigger is held back until a sent message contains the trigger.
    pub(crate) async fn mock_bot_api(
        updates: Vec<(&'static str, Value)>,
        sent: Arc<Mutex<Vec<(i64, String)>>>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let updates = Arc::new(Mutex::new(updates));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let updates = updates.clone();
                let sent = sent.clone();
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
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if body.len() >= length {
                                break (head.to_string(), body.to_string());
                            }
                        }
                    };
                    let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    let result = if head.contains("/getMe") {
                        json!({ "username": "test_bot" })
                    } else if head.contains("/getUpdates") {
                        let batch: Vec<Value> = {
                            let sent = sent.lock().unwrap();
                            let mut updates = updates.lock().unwrap();
                            let (ready, held): (Vec<_>, Vec<_>) =
                                std::mem::take(&mut *updates).into_iter().partition(|(trigger, _)| {
                                    trigger.is_empty() || sent.iter().any(|(_, t)| t.contains(trigger))
                                });
                            *updates = held;
                            ready.into_iter().map(|(_, update)| update).collect()
                        };
                        if batch.is_empty() {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                        Value::Array(batch)
                    } else {
                        if head.contains("/sendMessage") {
                            sent.lock().unwrap().push((
                                body["chat_id"].as_i64().unwrap_or(0),
                                body["text"].as_str().unwrap_or_default().to_string(),
                            ));
                        }
                        json!(true)
                    };
                    let payload = json!({ "ok": true, "result": result }).to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    pub(crate) fn update(id: i64, chat: i64, text: &str) -> Value {
        json!({ "update_id": id, "message": { "chat": { "id": chat }, "from": { "username": "u" }, "text": text } })
    }

    #[test]
    fn parses_approval_answers() {
        let parse = |t: &str| approval_answer(t).map(|a| (a.approve, a.id, a.reason));
        assert_eq!(parse("/approve"), Some((true, None, None)));
        assert_eq!(parse("/approve 3"), Some((true, Some(3), None)));
        assert_eq!(parse("/deny too risky"), Some((false, None, Some("too risky".into()))));
        assert_eq!(parse("/deny #2 not now"), Some((false, Some(2), Some("not now".into()))));
        assert_eq!(parse("/no"), Some((false, None, None)));
        assert!(approval_answer("approve the plan").is_none());
    }

    #[test]
    fn splits_long_messages() {
        let text = format!("{}\n{}", "a".repeat(3000), "b".repeat(3000));
        let chunks = split_message(&text, 4000);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|c| c.chars().count() <= 4000));
    }

    /// The bot token named by handle is read from the secret store, with no environment
    /// variable (decision D91); a handle that is malformed or holds nothing keeps the channel off.
    #[test]
    fn the_bot_token_is_read_from_the_secret_store_by_handle() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let mut settings = Settings::default();
        settings.channels.telegram.bot_token_secret = Some("cred://channels/telegram".into());
        let missing = bot_token(&paths, &settings).unwrap_err();
        assert!(missing.contains("nypi secrets set channels/telegram"), "{missing}");
        assert!(!paths.secrets_key().exists(), "no key is made while there is no store");

        SecretStore::open(home.path())
            .unwrap()
            .set(&Handle::parse("cred://channels/telegram").unwrap(), "123456:sealed-bot-token")
            .unwrap();
        assert_eq!(bot_token(&paths, &settings).unwrap().as_deref(), Some("123456:sealed-bot-token"));
        let api = TelegramApi::from_settings(&paths, &settings).unwrap().unwrap();
        assert!(api.base.ends_with("/bot123456:sealed-bot-token"));

        settings.channels.telegram.bot_token_secret = Some("not a handle".into());
        assert!(bot_token(&paths, &settings).unwrap_err().contains("botTokenSecret"));
    }
}
