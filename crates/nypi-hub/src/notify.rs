//! Proactive reports (principles A3, B6): run results, stalls, unanswered approvals, and watchdog
//! alerts reach the user out of band, so someone who walked away from the terminal still hears
//! about them. Every notice goes to every configured channel: paired Telegram chats (send-only, so
//! it works beside a running bridge), desktop notifications, webhooks, and commands such as
//! sendmail (`notify.channels`). What nobody is watching goes through the outbox first (`post`,
//! decision D57): it is recorded before it is sent, sent again when no channel took it, and shown
//! by the next front end the user opens.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::{Duration, Instant};

use nypi_agent::AgentEvent;
use nypi_core::config::{NotifyChannel, Paths, Settings, WebhookFormat, expand_secrets};
use nypi_core::inspector::AlertLevel;
use nypi_core::ledger::TaskId;
use nypi_core::outbox::Outbox;
use nypi_core::runtime::{RunOutcome, Runtime, RuntimeEvent};
use nypi_core::status::StatusKind;
use serde_json::{Value, json};

use crate::channels::telegram::api::{Access, TelegramApi};
use crate::records::tool_summary;

/// How often front ends check for stalls.
pub const STALL_CHECK_INTERVAL: Duration = Duration::from_secs(15);
/// Desktop notifications show a line or two; the rest is in the other channels.
const DESKTOP_CHARS: usize = 240;
const SEND_TIMEOUT: Duration = Duration::from_secs(15);

enum Channel {
    Telegram {
        api: TelegramApi,
        chats: Vec<i64>,
    },
    Desktop,
    /// The URL may hold a secret; it is never printed, only its host.
    Webhook {
        url: String,
        format: WebhookFormat,
        headers: BTreeMap<String, String>,
    },
    Command {
        command: Vec<String>,
        stdin: Option<String>,
    },
}

impl Channel {
    fn describe(&self) -> String {
        match self {
            Channel::Telegram { chats, .. } => format!("Telegram ({} paired chat(s))", chats.len()),
            Channel::Desktop => "desktop notification".into(),
            Channel::Webhook { url, format, .. } => {
                let host = url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or("?");
                format!("webhook to {host} ({format:?})")
            }
            Channel::Command { command, .. } => format!("command {}", command.first().cloned().unwrap_or_default()),
        }
    }
}

/// Sends every notice to every channel.
pub struct Notifier {
    label: String,
    /// The NyPi home, whose running channels (decision D83) are offered each notice first.
    home: std::path::PathBuf,
    channels: Vec<Channel>,
    /// Channels that could not be set up, for `nypi notify test`.
    pub problems: Vec<String>,
}

impl Notifier {
    /// Telegram (when enabled, with a token and a paired chat) plus `notify.channels`. None when
    /// no channel is usable.
    pub fn from_runtime(runtime: &Runtime) -> Option<Self> {
        Self::from_settings(&runtime.settings, &runtime.paths, &runtime.cwd, true)
    }

    pub fn from_settings(settings: &Settings, paths: &Paths, cwd: &Path, telegram: bool) -> Option<Self> {
        let notifier = Self::build(settings, paths, cwd, telegram);
        (!notifier.channels.is_empty()).then_some(notifier)
    }

    /// Every channel that could be set up, and what went wrong with the others.
    pub fn build(settings: &Settings, paths: &Paths, cwd: &Path, telegram: bool) -> Self {
        let place = place(cwd);
        let mut notifier = Notifier {
            label: format!("{} · {place}", settings.agent_name),
            home: paths.home.clone(),
            channels: Vec::new(),
            problems: Vec::new(),
        };
        if telegram && settings.notify.telegram {
            let chats: Vec<i64> = Access::load(&paths.telegram_access()).allowed_chat_ids.into_iter().collect();
            match TelegramApi::from_settings(paths, settings) {
                Ok(Some(api)) if !chats.is_empty() => notifier.channels.push(Channel::Telegram { api, chats }),
                Ok(Some(_)) => notifier.problems.push("Telegram: no chat is paired yet (run nypi telegram)".into()),
                Ok(None) => notifier
                    .problems
                    .push("Telegram: no bot token (TELEGRAM_BOT_TOKEN, or channels.telegram.botTokenSecret)".into()),
                Err(why) => notifier.problems.push(format!("Telegram: {why}")),
            }
        }
        for config in &settings.notify.channels {
            match config {
                NotifyChannel::Desktop => notifier.channels.push(Channel::Desktop),
                NotifyChannel::Webhook { url, format, headers } => {
                    let expanded = expand_secrets(url).and_then(|url| {
                        let headers = headers
                            .iter()
                            .map(|(k, v)| expand_secrets(v).map(|v| (k.clone(), v)))
                            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
                        Ok((url, headers))
                    });
                    match expanded {
                        Ok((url, headers)) => {
                            notifier.channels.push(Channel::Webhook { url, format: *format, headers })
                        }
                        Err(error) => notifier.problems.push(format!("webhook: {error:#}")),
                    }
                }
                NotifyChannel::Command { command, stdin } if !command.is_empty() => {
                    notifier.channels.push(Channel::Command { command: command.clone(), stdin: stdin.clone() })
                }
                NotifyChannel::Command { .. } => notifier.problems.push("command channel: `command` is empty".into()),
            }
        }
        notifier
    }

    pub fn channels(&self) -> Vec<String> {
        self.channels.iter().map(Channel::describe).collect()
    }

    /// Send `text` everywhere. Returns one message per channel that failed; callers that run
    /// in a terminal UI ignore them rather than print over the screen.
    pub async fn send(&self, text: &str) -> Vec<String> {
        self.send_all(text).await.failed
    }

    /// Send `text` everywhere and say which channels took it.
    pub async fn send_all(&self, text: &str) -> Sent {
        // The channels the daemon runs first; what one of them carried is not sent again on
        // that service from here.
        let mut sent = through_channels(&self.home, &format!("[{}] {text}", self.label)).await;
        for channel in &self.channels {
            if matches!(channel, Channel::Telegram { .. }) && sent.delivered.iter().any(|name| name == "telegram") {
                continue;
            }
            match tokio::time::timeout(SEND_TIMEOUT, self.send_one(channel, text))
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")))
            {
                Ok(()) => sent.delivered.push(channel.describe()),
                Err(error) => sent.failed.push(format!("{}: {error:#}", channel.describe())),
            }
        }
        sent
    }

    async fn send_one(&self, channel: &Channel, text: &str) -> anyhow::Result<()> {
        let title = format!("NyPi · {}", self.label);
        match channel {
            Channel::Telegram { api, chats } => {
                // One chat taking it is a delivery; the error is the last chat's that did not.
                let (mut reached, mut problem) = (false, None);
                for chat in chats {
                    match api.try_send_text(*chat, &format!("[{}] {text}", self.label)).await {
                        Ok(()) => reached = true,
                        Err(error) => problem = Some(error),
                    }
                }
                match problem {
                    Some(error) if !reached => Err(error),
                    _ => Ok(()),
                }
            }
            Channel::Desktop => desktop(&title, text).await,
            Channel::Webhook { url, format, headers } => {
                let client = reqwest::Client::new();
                let mut request = match format {
                    WebhookFormat::Slack => client.post(url).json(&json!({ "text": format!("*{title}*\n{text}") })),
                    WebhookFormat::Discord => {
                        client.post(url).json(&json!({ "content": format!("**{title}**\n{text}") }))
                    }
                    WebhookFormat::Json => {
                        client.post(url).json(&json!({ "title": title, "text": text, "source": "nypi" }))
                    }
                    WebhookFormat::Text => client.post(url).header("Title", title.clone()).body(text.to_string()),
                };
                for (key, value) in headers {
                    request = request.header(key, value);
                }
                let response = request.send().await?;
                if !response.status().is_success() {
                    anyhow::bail!("HTTP {}", response.status());
                }
                Ok(())
            }
            Channel::Command { command, stdin } => {
                let args = json!({ "title": title, "text": text });
                let argv = nypi_core::tools::command::render_argv(command, &args).map_err(anyhow::Error::msg)?;
                let input = stdin
                    .as_deref()
                    .map(|t| nypi_core::tools::command::render(t, &args))
                    .transpose()
                    .map_err(anyhow::Error::msg)?;
                run(&argv, input).await
            }
        }
    }
}

async fn desktop(title: &str, text: &str) -> anyhow::Result<()> {
    // The desktop channel is on by default; tests must not pop notices on the developer's screen.
    if cfg!(test) {
        return Ok(());
    }
    let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(DESKTOP_CHARS).collect();
    // Arguments, not string interpolation: the text needs no AppleScript escaping.
    let argv: Vec<String> = if cfg!(target_os = "macos") {
        [
            "osascript",
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            title,
            &text,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    } else if cfg!(target_os = "linux") {
        vec!["notify-send".into(), title.into(), text]
    } else {
        anyhow::bail!("desktop notifications are not supported on this system");
    };
    run(&argv, None).await
}

async fn run(argv: &[String], stdin: Option<String>) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot start {}: {e}", argv[0]))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes()).await?;
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).lines().last().unwrap_or_default().to_string();
        anyhow::bail!("{} exited with {}: {detail}", argv[0], output.status);
    }
    Ok(())
}

/// The working directory's name, for labels in reports.
pub fn place(cwd: &Path) -> String {
    cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into())
}

/// The outcome of one round of sending.
#[derive(Default, Debug)]
pub struct Sent {
    pub delivered: Vec<String>,
    pub failed: Vec<String>,
}

/// Something to tell the user that nobody may be watching for.
pub struct Notice<'a> {
    /// Which part of NyPi says it: `watchdog`, `scheduler`, `run`, `dmn`, `supervisor`.
    pub source: &'a str,
    pub level: AlertLevel,
    pub text: &'a str,
    pub task: Option<&'a TaskId>,
    /// The front end showing it to the user right now, if one is.
    pub shown_in: Option<&'a str>,
}

/// Record a notice in the outbox, then send it to every channel (decision D57). The record
/// comes first, so a process that dies mid-send, a channel that fails, or having no channel at
/// all leaves the notice open instead of lost. Returns the channels that failed.
pub async fn post(paths: &Paths, notifier: Option<&Notifier>, notice: Notice<'_>) -> Vec<String> {
    let outbox = Outbox::new(paths.outbox());
    let id = match outbox.queue(notice.level, notice.source, notice.text, notice.task) {
        Ok(id) => Some(id),
        Err(error) => {
            // Still try the channels: an unrecorded notice that arrives beats a silent one.
            eprintln!("warning: the notice could not be written to the outbox ({error}): {}", notice.text);
            None
        }
    };
    if let (Some(id), Some(front_end)) = (&id, notice.shown_in) {
        let _ = outbox.mark_seen(id, front_end);
    }
    let sent = match notifier {
        Some(notifier) => notifier.send_all(notice.text).await,
        None => through_channels(&paths.home, notice.text).await,
    };
    if let Some(id) = &id
        && (notifier.is_some() || !sent.delivered.is_empty())
    {
        let _ = outbox.record_attempt(id, sent.delivered, sent.failed.clone());
    }
    sent.failed
}

/// Offer `text` to the channels the daemon of `home` runs (decision D83): those that took it.
async fn through_channels(home: &Path, text: &str) -> Sent {
    let mut sent = Sent::default();
    if let Some(channels) = crate::channels::running_for(home) {
        sent.delivered = channels.deliver_notice(text).await.into_iter().map(str::to_string).collect();
    }
    sent
}

/// Send the open notices that are due again. Returns how many a channel took this time.
pub async fn flush(paths: &Paths, notifier: Option<&Notifier>) -> usize {
    if notifier.is_none() && crate::channels::running_for(&paths.home).is_none() {
        return 0;
    }
    let outbox = Outbox::new(paths.outbox());
    let now = chrono::Utc::now();
    let mut reached = 0;
    for notice in outbox.open().into_iter().filter(|n| n.retry_due(now)) {
        let sent = match notifier {
            Some(notifier) => notifier.send_all(&notice.text).await,
            None => through_channels(&paths.home, &notice.text).await,
        };
        reached += usize::from(!sent.delivered.is_empty());
        let _ = outbox.record_attempt(&notice.id, sent.delivered, sent.failed);
    }
    reached
}

/// The notices the user has not heard, marked as shown by `front_end`: what a front end prints
/// when it opens, so being away never means not being told.
pub fn missed(paths: &Paths, front_end: &str) -> Vec<nypi_core::outbox::Notice> {
    let outbox = Outbox::new(paths.outbox());
    let open = outbox.open();
    for notice in &open {
        let _ = outbox.mark_seen(&notice.id, front_end);
    }
    open
}

/// One line for a missed notice: when it was raised, and what it says.
pub fn missed_line(notice: &nypi_core::outbox::Notice) -> String {
    let when = chrono::DateTime::parse_from_rfc3339(&notice.at)
        .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| "?".into());
    format!("While you were away ({when}, {}): {}", notice.source, notice.text)
}

/// `nypi notices`: what NyPi tried to tell the user, and whether it arrived. Listing the open
/// ones in a terminal shows them, so they are marked seen.
pub fn list(paths: &Paths, all: bool) -> anyhow::Result<i32> {
    let outbox = Outbox::new(paths.outbox());
    let notices = outbox.load();
    let shown: Vec<_> = notices.iter().filter(|n| all || n.is_open()).collect();
    if shown.is_empty() {
        let scope = if all { "" } else { " undelivered" };
        crate::outln!("No{scope} notices in {}.", outbox.path().display());
        if !all && !notices.is_empty() {
            crate::outln!("{} notice(s) reached you; nypi notices --all lists them.", notices.len());
        }
        return Ok(0);
    }
    for notice in shown.iter().rev().take(50).rev() {
        let when = notice.at.get(..16).unwrap_or_default().replace('T', " ");
        crate::outln!("{}  {when} UTC  {:<9}  {}", notice.id, notice.source, notice.state());
        crate::outln!("    {}", notice.text.split_whitespace().collect::<Vec<_>>().join(" "));
        for failure in &notice.failed {
            crate::outln!("    failed: {failure}");
        }
        if notice.is_open() {
            let _ = outbox.mark_seen(&notice.id, "nypi notices");
        }
    }
    Ok(0)
}

/// How loudly a finished run's report should be raised.
pub fn level_of(outcome: &RunOutcome) -> AlertLevel {
    match outcome.status.kind {
        StatusKind::Failed => AlertLevel::Error,
        StatusKind::Blocked => AlertLevel::Warning,
        StatusKind::Done | StatusKind::Continuing => AlertLevel::Notice,
    }
}

/// What `nypi notify test` reports: channels, problems, and a test notice's result.
pub async fn test(settings: &Settings, paths: &Paths, cwd: &Path, text: &str) -> i32 {
    let notifier = Notifier::build(settings, paths, cwd, true);
    for problem in &notifier.problems {
        println!("not set up: {problem}");
    }
    if notifier.channels.is_empty() {
        println!("No notification channel is set up. Add one to notify.channels in settings, for example:");
        println!("  \"notify\": {{ \"channels\": [ {{ \"type\": \"desktop\" }} ] }}");
        return 1;
    }
    let failures = notifier.send(text).await;
    for channel in notifier.channels() {
        let failed = failures.iter().find(|f| f.starts_with(&channel));
        match failed {
            Some(failure) => println!("FAILED  {failure}"),
            None => println!("sent    {channel}"),
        }
    }
    i32::from(!failures.is_empty())
}

/// The report for a finished run.
pub fn run_report(outcome: &RunOutcome, elapsed: Duration) -> String {
    format!("{}\n(run took {})", outcome.status, format_duration(elapsed))
}

pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// A run still quiet is reported again at most this far apart.
const STALL_REPEAT_CAP: Duration = Duration::from_secs(2 * 3600);

/// Tracks when a run last made progress and what is running, to detect stalls.
pub struct Progress {
    last: Instant,
    running: HashMap<String, (String, Instant)>,
    /// Stall notices sent in this quiet period.
    reports: u32,
}

impl Default for Progress {
    fn default() -> Self {
        Progress { last: Instant::now(), running: HashMap::new(), reports: 0 }
    }
}

impl Progress {
    pub fn observe(&mut self, event: &RuntimeEvent) {
        self.moved();
        if let RuntimeEvent::Agent { event } = event {
            match event {
                AgentEvent::ToolExecutionStart { call_id, tool_name, arguments } => {
                    self.tool_started(call_id, tool_name, arguments)
                }
                AgentEvent::ToolExecutionEnd { call_id, .. } => {
                    self.running.remove(call_id);
                }
                _ => {}
            }
        }
    }

    /// The same, from a record of a thread (the hub watches every run this way).
    pub fn observe_record(&mut self, record: &Value) {
        self.moved();
        let call = record["call_id"].as_str().unwrap_or_default();
        match record["type"].as_str() {
            Some("tool_execution_start") => {
                self.tool_started(call, record["tool_name"].as_str().unwrap_or("?"), &record["arguments"])
            }
            Some("tool_execution_end") => {
                self.running.remove(call);
            }
            _ => {}
        }
    }

    fn moved(&mut self) {
        self.last = Instant::now();
        self.reports = 0;
    }

    fn tool_started(&mut self, call: &str, tool: &str, arguments: &Value) {
        let summary: String = tool_summary(tool, arguments).chars().take(120).collect();
        self.running.insert(call.to_string(), (format!("{tool} {summary}"), Instant::now()));
    }

    /// A run that last moved `quiet` ago.
    #[cfg(test)]
    pub fn quiet_for(quiet: Duration) -> Self {
        Progress { last: Instant::now() - quiet, ..Default::default() }
    }

    /// A stall notice when the run has been quiet for `limit`, and again each time the quiet
    /// has doubled (10 min, 20 min, 40 min, ... then every 2 h): one notice for a run that then
    /// hangs all night would be the silent stop B6 forbids.
    pub fn stall(&mut self, limit: Duration) -> Option<String> {
        self.stall_after(limit, self.last.elapsed())
    }

    fn stall_after(&mut self, limit: Duration, quiet: Duration) -> Option<String> {
        if limit.is_zero() {
            return None;
        }
        let mut next = limit;
        for _ in 0..self.reports {
            next = if next >= STALL_REPEAT_CAP { next + STALL_REPEAT_CAP } else { (next * 2).min(STALL_REPEAT_CAP) };
        }
        if quiet < next {
            return None;
        }
        self.reports += 1;
        let quiet = format_duration(quiet);
        let running: Vec<String> = self
            .running
            .values()
            .map(|(what, since)| format!("{what} (running for {})", format_duration(since.elapsed())))
            .collect();
        Some(if running.is_empty() {
            format!("Continuing: no progress for {quiet}; waiting on the model.")
        } else {
            format!("Continuing: no progress for {quiet}. Still running: {}", running.join("; "))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reports_a_stall_once_per_quiet_period() {
        let mut progress = Progress::default();
        progress.observe(&RuntimeEvent::Agent {
            event: AgentEvent::ToolExecutionStart {
                call_id: "c".into(),
                tool_name: "bash".into(),
                arguments: json!({ "command": "sleep 900" }),
            },
        });
        assert!(progress.stall(Duration::from_secs(60)).is_none());
        progress.last -= Duration::from_secs(61);
        let notice = progress.stall(Duration::from_secs(60)).unwrap();
        assert!(notice.contains("sleep 900"), "{notice}");
        assert!(progress.stall(Duration::from_secs(60)).is_none(), "reported once");
        // Still quiet: again when the quiet has doubled, then at most every two hours.
        let minute = Duration::from_secs(60);
        assert!(progress.stall_after(minute, Duration::from_secs(119)).is_none());
        assert!(progress.stall_after(minute, Duration::from_secs(120)).is_some());
        assert!(progress.stall_after(minute, Duration::from_secs(239)).is_none());
        assert!(progress.stall_after(minute, Duration::from_secs(240)).is_some());
        let mut long = Progress { reports: 20, ..Progress::default() };
        let quiet = STALL_REPEAT_CAP * 20;
        assert!(long.stall_after(minute, quiet).is_some(), "a run quiet for days is still reported");
        // Progress starts the count again.
        progress.observe(&RuntimeEvent::Alert { alert: nypi_core::inspector::Alert::new(AlertLevel::Notice, "x") });
        assert!(progress.stall_after(minute, Duration::from_secs(59)).is_none());
        assert!(progress.stall_after(minute, minute).is_some());
    }

    #[tokio::test]
    async fn a_notice_is_recorded_first_retried_when_no_channel_took_it_and_shown_when_none_ever_does() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths { home: dir.path().to_path_buf() };
        let outbox = Outbox::new(paths.outbox());
        let notice = |text: &'static str, shown_in| Notice {
            source: "watchdog",
            level: AlertLevel::Error,
            text,
            task: None,
            shown_in,
        };

        // No channel at all: nothing is sent, and nothing is lost.
        assert!(post(&paths, None, notice("Failed: task t-1 died", None)).await.is_empty());
        assert_eq!(outbox.open().len(), 1);

        // A channel that fails, then works: the supervisor's flush delivers both open notices.
        let marker = dir.path().join("channel-works");
        let inbox = dir.path().join("inbox.txt");
        let script = format!("test -e {} && cat >> {}", marker.display(), inbox.display());
        let mut settings = Settings::default();
        settings.notify.telegram = false;
        settings.notify.channels = vec![NotifyChannel::Command {
            command: vec!["sh".into(), "-c".into(), script],
            stdin: Some("{text}\n".into()),
        }];
        let notifier = Notifier::from_settings(&settings, &paths, Path::new("/"), false).unwrap();
        let failed = post(&paths, Some(&notifier), notice("Blocked: task t-2 needs you", None)).await;
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!(outbox.open().iter().map(|n| n.attempts).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(flush(&paths, Some(&notifier)).await, 0, "the first is tried and fails; the second is not due yet");
        std::fs::write(&marker, "").unwrap();
        assert_eq!(flush(&paths, Some(&notifier)).await, 0, "both wait out their backoff");
        // Backoff is over: both go out.
        let stale = std::fs::read_to_string(outbox.path()).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let aged: String = stale
            .lines()
            .map(|line| {
                let mut event: serde_json::Value = serde_json::from_str(line).unwrap();
                event["at"] = old.clone().into();
                format!("{event}\n")
            })
            .collect();
        std::fs::write(outbox.path(), aged).unwrap();
        assert_eq!(flush(&paths, Some(&notifier)).await, 2);
        assert!(outbox.open().is_empty());
        let delivered = std::fs::read_to_string(&inbox).unwrap();
        assert!(delivered.contains("task t-1 died") && delivered.contains("task t-2 needs you"), "{delivered}");

        // Shown live in a front end, or never delivered and shown when one opens: both count as told.
        post(&paths, None, notice("⏰ stretch", Some("terminal"))).await;
        assert!(outbox.open().is_empty());
        post(&paths, None, notice("Failed: the nightly import", None)).await;
        let away = missed(&paths, "terminal");
        assert_eq!(away.len(), 1);
        assert!(missed_line(&away[0]).starts_with("While you were away ("), "{}", missed_line(&away[0]));
        assert!(missed(&paths, "terminal").is_empty(), "shown once");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_duration(Duration::from_secs(7300)), "2h 1m");
    }
}

#[cfg(test)]
mod channel_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Accept requests and keep each one's head and body.
    async fn mock() -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    buffer.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buffer).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            sink.lock().unwrap().push((head.to_string(), body.to_string()));
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
            }
        });
        (format!("http://{address}"), seen)
    }

    #[tokio::test]
    async fn webhooks_and_commands_get_every_notice() {
        let (base, seen) = mock().await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("mail.txt");
        let mut settings = Settings::default();
        settings.notify.telegram = false;
        settings.notify.channels = vec![
            NotifyChannel::Webhook {
                url: format!("{base}/slack"),
                format: WebhookFormat::Slack,
                headers: BTreeMap::new(),
            },
            NotifyChannel::Webhook {
                url: format!("{base}/ntfy"),
                format: WebhookFormat::Text,
                headers: BTreeMap::new(),
            },
            NotifyChannel::Command {
                command: vec!["sh".into(), "-c".into(), format!("cat > {}", out.display())],
                stdin: Some("Subject: {title}\n\n{text}\n".into()),
            },
            NotifyChannel::Webhook {
                url: "https://hooks.example/${NYPI_TEST_UNSET_WEBHOOK_SECRET}".into(),
                format: WebhookFormat::Slack,
                headers: BTreeMap::new(),
            },
        ];
        let paths = Paths { home: dir.path().to_path_buf() };
        let notifier = Notifier::from_settings(&settings, &paths, Path::new("/work/site"), true).unwrap();
        assert_eq!(notifier.channels().len(), 3, "{:?}", notifier.channels());
        assert!(
            notifier.problems.iter().any(|p| p.contains("NYPI_TEST_UNSET_WEBHOOK_SECRET")),
            "{:?}",
            notifier.problems
        );
        assert!(notifier.channels().iter().all(|c| !c.contains("/slack")), "a webhook URL is never shown");

        let failures = notifier.send("Done: the deploy finished.").await;
        assert!(failures.is_empty(), "{failures:?}");
        let seen = seen.lock().unwrap().clone();
        let slack = seen.iter().find(|(head, _)| head.starts_with("POST /slack")).unwrap();
        let body: serde_json::Value = serde_json::from_str(&slack.1).unwrap();
        assert_eq!(body["text"], "*NyPi · Lucia · site*\nDone: the deploy finished.");
        let ntfy = seen.iter().find(|(head, _)| head.starts_with("POST /ntfy")).unwrap();
        assert!(ntfy.0.to_lowercase().contains("title: nypi · lucia · site") && ntfy.1 == "Done: the deploy finished.");
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            "Subject: NyPi · Lucia · site\n\nDone: the deploy finished.\n"
        );

        let mut broken = Settings::default();
        broken.notify.telegram = false;
        broken.notify.channels = vec![NotifyChannel::Command { command: vec!["false".into()], stdin: None }];
        let notifier = Notifier::from_settings(&broken, &paths, Path::new("/"), true).unwrap();
        assert!(notifier.send("x").await[0].contains("false exited"));
        broken.notify.channels.clear();
        assert!(Notifier::from_settings(&broken, &paths, Path::new("/"), true).is_none());
    }
}
