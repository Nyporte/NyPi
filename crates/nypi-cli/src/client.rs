//! Interfaces (decisions D64, D66, and D67 part 2): what a terminal is. Each holds a link to one
//! thread of the hub and speaks the RPC protocol over it; none holds a runtime or drives a run.
//! The link goes to the daemon's socket when the daemon runs, and otherwise to a hub this
//! process runs for itself (`daemon::Local`), so there is one way of talking to Lucia and it
//! behaves the same in both. `send` gives one message and, unless told not to wait, prints the
//! result when the run settles: `nypi -p` in a folder, `nypi send` to the home thread. `attach`
//! is a conversation: plain `nypi` in a folder, `nypi attach` to the home thread. Lines typed
//! go to Lucia (they steer her when she is working) and everything she does is shown; leaving a
//! daemon's thread leaves her running, leaving one's own ends it. The full-screen UI (tui/) is
//! the third interface on the same link. `deliver` hands the daemon a prompt that is not the
//! user's own words (a fired schedule, a goal review).

use std::collections::VecDeque;
use std::io::BufRead;
use std::time::{Duration, Instant};

use nypi_agent::AgentEvent;
use nypi_ai::{Message, text_of};
use nypi_core::config::{Paths, Settings};
use nypi_core::inspector::{Alert, call_summary};
use nypi_core::retry::RetryNotice;
use nypi_core::runtime::RuntimeEvent;
use serde_json::{Value, json};
use tokio::sync::mpsc;

pub use nypi_hub::link::{Left, Link, deliver, hub_running, message, new_id, thread_session, to_daemon};
pub use nypi_hub::records::{settled_note, show};

use crate::render::Renderer;

/// Lines from stdin on a dedicated thread, so input is read even while a run is in progress.
pub fn spawn_stdin_reader() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut buffer = String::new();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            // A trailing backslash continues the message on the next line.
            if let Some(stripped) = line.strip_suffix('\\') {
                buffer.push_str(stripped);
                buffer.push('\n');
                continue;
            }
            buffer.push_str(&line);
            if tx.send(std::mem::take(&mut buffer)).is_err() {
                break;
            }
        }
    });
    rx
}

/// Where the hub took this interface to be, in a few words: the folder's name, and the device
/// when it is another. None for a session of the terminal's own, which has one place.
pub fn focus_label(hello: &Value) -> Option<String> {
    let focus = hello.get("focus").filter(|focus| focus.is_object())?;
    let cwd = std::path::Path::new(focus["cwd"].as_str()?);
    let folder = cwd.file_name().map_or_else(|| cwd.display().to_string(), |name| name.to_string_lossy().into_owned());
    Some(match focus["device"].as_str() {
        Some(device) => format!("{folder} on {device}"),
        None => folder,
    })
}

fn greeting(hello: &Value) -> String {
    let models = match hello["chat"].as_str() {
        Some(chat) => format!("chat {chat}, worker {}", hello["model"].as_str().unwrap_or("?")),
        None => hello["model"].as_str().unwrap_or("?").to_string(),
    };
    if hello["local"] == true {
        // A session of this terminal's own: nothing was under way before it.
        return format!(
            "{} · {} · thinking {}",
            hello["agent"].as_str().unwrap_or("Lucia"),
            models,
            hello["thinking"].as_str().unwrap_or("off")
        );
    }
    let place = match hello["device"].as_str() {
        Some(device) => format!("{} on {device}", hello["cwd"].as_str().unwrap_or("?")),
        None => hello["cwd"].as_str().unwrap_or("?").to_string(),
    };
    // One conversation, whichever folder it is reached from: what is asked here is done here.
    format!(
        "{} · {} · you are at {} · {}",
        hello["agent"].as_str().unwrap_or("Lucia"),
        models,
        place,
        if hello["busy"] == true { "working on a task" } else { "idle" }
    )
}

/// Give the thread one message: exit 0 when it was taken (and, when waiting, the run did not fail).
pub async fn send(mut link: Link, text: &str, wait: bool) -> anyhow::Result<i32> {
    if text.trim().is_empty() {
        anyhow::bail!("a message is needed");
    }
    if link.hello["hasKey"] == false {
        eprintln!("warning: no API key found for {}", link.hello["model"].as_str().unwrap_or("the model"));
    }
    let code = send_over(&mut link, text, wait).await?;
    // A hub of this process ends here; what its last reflection learned is said before it goes.
    if let Left::Ended { reflections, .. } = link.leave().await {
        for summary in reflections {
            eprintln!("{summary}");
        }
    }
    Ok(code)
}

async fn send_over(link: &mut Link, text: &str, wait: bool) -> anyhow::Result<i32> {
    // With the chat module in front, chat answers first and a task may follow.
    let chat = link.chat();
    let local = link.is_local();
    let id = new_id();
    link.send(message(&id, text)).await?;
    let mut taken = false;
    while let Some(record) = link.next().await? {
        if record["type"] == "response" && record["id"] == id.as_str() {
            if record["success"] != true {
                eprintln!("Not taken: {}", record["error"].as_str().unwrap_or("no reason was given"));
                return Ok(1);
            }
            match record["data"]["disposition"].as_str() {
                // A slash command: answered at once, no run.
                Some("handled") => {
                    println!("{}", record["data"]["output"].as_str().unwrap_or_default());
                    return Ok(0);
                }
                Some("queued") => eprintln!("[Lucia is working; your message goes into the task she is on]"),
                _ => {}
            }
            taken = true;
            if !wait && !local {
                eprintln!("[taken; the result will reach you as a notice, or watch with: nypi attach]");
                return Ok(0);
            }
            continue;
        }
        if !taken {
            continue;
        }
        if chat && matches!(record["type"].as_str(), Some("chat_settled" | "chat_failed")) {
            let failed = record["type"] == "chat_failed";
            let said = match failed {
                true => {
                    format!("The chat model did not answer: {}", record["error"].as_str().unwrap_or("unknown error"))
                }
                false => record["reply"].as_str().unwrap_or_default().trim().to_string(),
            };
            if record["working"] == true {
                // Work was handed over; its report is the result.
                eprintln!("{said}");
                continue;
            }
            match failed {
                true => eprintln!("{said}"),
                false => println!("{said}"),
            }
            return Ok(i32::from(failed));
        }
        // Chat's own tool calls only hand work over; the worker's are the work.
        if record["module"] == "chat" {
            continue;
        }
        // The principal reviews this settlement first: its words, or the report it lets stand,
        // are the result; a task sent back goes on.
        if record["type"] == "agent_settled" && record["review"] == "pending" {
            continue;
        }
        if record["type"] == "task_reviewed" {
            if record["accepted"] != true {
                eprintln!("[the report fell short; sent back to the worker (round {})]", record["round"]);
            }
            continue;
        }
        if matches!(record["type"].as_str(), Some("agent_settled" | "task_report")) {
            if let Some(answer) = record["answer"].as_str().filter(|answer| !answer.trim().is_empty()) {
                println!("{}", answer.trim_end());
            }
            if let Some(sources) = record["sources"].as_str() {
                println!("\n{sources}");
            }
            if let Some(note) = settled_note(&record) {
                eprintln!("{note}");
            }
            return Ok(i32::from(record["error"].is_string() || record["status"]["kind"] == "failed"));
        }
        if let Some(line) = show(&record, false) {
            eprintln!("{line}");
            if record["type"] == "approval_request" && !local {
                eprintln!(
                    "[nobody can answer from here; answer it in a terminal attached to this folder, or on Telegram]"
                );
            }
        }
    }
    match local {
        true => eprintln!("The run stopped before it settled."),
        false => eprintln!("The hub closed the connection before the run ended. Check: nypi status"),
    }
    Ok(1)
}

/// What a typed line asks of the daemon.
#[derive(Debug, PartialEq)]
pub(crate) enum Typed {
    Leave,
    Command(Value),
    Local(String),
}

/// What the thread is doing, as a typed line is read against it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Busy {
    Idle,
    /// A run is under way: a command waits for it to end, plain text steers it.
    Working,
    /// The worker runs behind the chat module: a command waits for the task by itself in the
    /// thread, and a message goes to chat, but `/followup` is still for after the task.
    BehindChat,
}

impl Busy {
    pub(crate) fn of(working: bool, chat: bool) -> Busy {
        match (working, chat) {
            (false, _) => Busy::Idle,
            (true, false) => Busy::Working,
            (true, true) => Busy::BehindChat,
        }
    }
}

/// Turn a typed line into a command. `waiting` holds the ids of unanswered approvals, oldest
/// first.
pub(crate) fn typed(line: &str, waiting: &[String], busy: Busy) -> Typed {
    let (word, rest) = line.split_once(char::is_whitespace).map_or((line, ""), |(word, rest)| (word, rest.trim()));
    // While a question waits, a bare yes or no is its answer and not a message.
    let bare = (!waiting.is_empty() && rest.is_empty()).then(|| word.to_lowercase());
    let word = match bare.as_deref() {
        Some("y" | "yes") => "/approve",
        Some("n" | "no") => "/deny",
        _ => word,
    };
    match word {
        "/detach" | "/quit" | "/exit" => Typed::Leave,
        "/abort" | "/stop" => Typed::Command(json!({ "id": new_id(), "type": "abort" })),
        // For after the run; while idle it is simply the next message.
        "/followup" if !rest.is_empty() => match busy {
            Busy::Working | Busy::BehindChat => {
                Typed::Command(json!({ "id": new_id(), "type": "follow_up", "message": rest }))
            }
            Busy::Idle => Typed::Command(message(&new_id(), rest)),
        },
        "/approve" | "/yes" | "/y" | "/deny" | "/no" | "/n" => {
            let approved = matches!(word, "/approve" | "/yes" | "/y");
            // The approval named first, else the oldest one waiting.
            let (named, reason) = match rest.split_once(char::is_whitespace) {
                Some((first, more)) if waiting.iter().any(|id| id == first) => (Some(first), more.trim()),
                None if waiting.iter().any(|id| id == rest) => (Some(rest), ""),
                _ => (None, rest),
            };
            match named.map(str::to_string).or_else(|| waiting.first().cloned()) {
                Some(id) => {
                    let reason = (!reason.is_empty()).then(|| reason.to_string());
                    Typed::Command(
                        json!({ "type": "approval_response", "id": id, "approved": approved, "reason": reason }),
                    )
                }
                None => Typed::Local("No approval is waiting.".into()),
            }
        }
        _ if word.starts_with(['/', '!']) && busy == Busy::Working => {
            Typed::Local("Commands wait until the run ends; /abort stops it, plain text steers it.".into())
        }
        _ => Typed::Command(message(&new_id(), line)),
    }
}

/// A record of the thread as the event the terminal's renderer draws; None for records that are
/// the protocol's own (responses, approvals, the daemon's notes).
pub(crate) fn runtime_event(record: &Value) -> Option<RuntimeEvent> {
    let kind = record["type"].as_str()?;
    match kind {
        "response" | "agent_settled" | "approval_request" | "approval_resolved" | "memory_hint" | "user_said"
        | "task_started" | "chat_settled" | "chat_failed" | "reflection" | "sourced_prompt" | "task_reviewed"
        | "task_report" => None,
        _ if kind.starts_with("daemon_") || kind == "hub_refused" => None,
        "alert" => serde_json::from_value::<Alert>(record.clone()).ok().map(|alert| RuntimeEvent::Alert { alert }),
        "compaction_start" => {
            Some(RuntimeEvent::CompactionStart { reason: record["reason"].as_str().unwrap_or_default().to_string() })
        }
        "compaction_end" => Some(RuntimeEvent::CompactionEnd {
            tokens_before: record["tokensBefore"].as_u64().unwrap_or(0),
            error: record["error"].as_str().map(str::to_string),
        }),
        _ if kind.starts_with("auto_retry") || kind.starts_with("summarization_retry") => {
            serde_json::from_value::<RetryNotice>(record.clone()).ok().map(|notice| RuntimeEvent::Retry { notice })
        }
        _ => serde_json::from_value::<AgentEvent>(record.clone()).ok().map(|event| RuntimeEvent::Agent { event }),
    }
}

/// The text of a user message that just entered the thread, if this record is one.
fn user_text(record: &Value) -> Option<String> {
    if record["type"] != "message_end" {
        return None;
    }
    match serde_json::from_value::<Message>(record["message"].clone()).ok()? {
        Message::User(user) => Some(text_of(&user.content)),
        _ => None,
    }
}

/// What somebody said on the thread, if this record is that: behind chat the loop says so itself
/// (the modules' transcripts also hold notes that are nobody's words).
pub(crate) fn said(record: &Value) -> Option<String> {
    if record["type"] == "user_said" {
        return record["text"].as_str().map(str::to_string);
    }
    // What the harness tells the agent between turns is in the transcript as a message too.
    record["module"].is_null().then(|| user_text(record)).flatten().filter(|text| !text.starts_with("[NyPi harness]"))
}

pub(crate) fn excerpt(text: &str, limit: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > limit { format!("{}…", line.chars().take(limit).collect::<String>()) } else { line }
}

/// Draw a record of the chat module: her reply as it is written. Chat's own tool calls only
/// hand work to the worker, which "[task started]" already shows.
fn draw_chat(renderer: &mut Renderer, record: &Value) {
    match runtime_event(record) {
        Some(RuntimeEvent::Agent {
            event:
                AgentEvent::ToolExecutionStart { .. }
                | AgentEvent::ToolExecutionUpdate { .. }
                | AgentEvent::ToolExecutionEnd { .. }
                | AgentEvent::AgentEnd { .. },
        }) => {}
        Some(RuntimeEvent::Retry { notice }) => {
            if let Some(note) = crate::render::retry_note(&notice) {
                renderer.note("33", &format!("[chat: {note}]"));
            }
        }
        Some(event) => renderer.handle(&event),
        None => {}
    }
}

/// Draw a record of the worker behind chat: its activity as compact lines. What it writes stays
/// back until its report, which is shown whole when the task settles.
fn draw_worker(renderer: &mut Renderer, record: &Value) {
    match runtime_event(record) {
        Some(RuntimeEvent::Agent { event: AgentEvent::ToolExecutionStart { tool_name, arguments, .. } }) => {
            let summary = excerpt(&call_summary(&tool_name, &arguments), 140);
            renderer.note("2", &format!("  · task: {tool_name} {summary}"));
        }
        Some(RuntimeEvent::Agent { event: AgentEvent::ToolExecutionEnd { tool_name, output, .. } })
            if output.is_error =>
        {
            renderer.note("33", &format!("  ✗ task: {tool_name} failed"));
        }
        Some(RuntimeEvent::Agent { .. }) | None => {}
        Some(other) => renderer.handle(&other),
    }
}

/// What a terminal says when it opens a thread: who is there, how to use it, and what the user
/// was not told while no interface was open (decision D57).
fn open_lines(link: &Link, paths: &Paths, front_end: &str) -> Vec<(&'static str, String)> {
    let hello = &link.hello;
    let mut lines = Vec::new();
    let help = match link.is_local() {
        true => "Type to talk, /help for commands. While she works, what you type steers her and Ctrl-C stops the run.",
        false => {
            "Type to talk; while she works, what you type steers her and Ctrl-C stops the run. /approve and /deny \
             answer an approval, /detach leaves her running, /help lists the rest."
        }
    };
    lines.push(("2", help.to_string()));
    if hello["hasKey"] == false {
        lines.push(("33", format!("No API key found for {}.", hello["model"].as_str().unwrap_or("the model"))));
    }
    if let Some(channels) = hello["notifies"].as_array().filter(|channels| !channels.is_empty()) {
        let channels: Vec<&str> = channels.iter().filter_map(Value::as_str).collect();
        let text = format!("Long runs, stalls, and pending approvals are also sent to: {}.", channels.join(", "));
        lines.push(("2", text));
    }
    if link.is_local() && !crate::supervisor::alive(paths, chrono::Utc::now()) {
        let hint = "Tasks and schedules are only watched while NyPi is open. To keep watching after you close it: \
                    nypi hub up";
        lines.push(("2", hint.to_string()));
    }
    for notice in crate::notify::missed(paths, front_end) {
        lines.push(("33", format!("[{}]", crate::notify::missed_line(&notice))));
    }
    lines
}

/// A conversation over `link` until the user leaves; `first` is sent at once.
pub async fn attach(mut link: Link, paths: &Paths, first: Option<String>, show_thinking: bool) -> anyhow::Result<i32> {
    let mut renderer = Renderer::new(show_thinking);
    println!("{} {}", renderer.paint("1;35", "NyPi"), greeting(&link.hello));
    for (color, line) in open_lines(&link, paths, "the terminal") {
        println!("{}", renderer.paint(color, &line));
    }
    // A run long enough to be reported is work worth a verdict (decision D63).
    let cwd = link.hello["cwd"].as_str().map(std::path::PathBuf::from).unwrap_or_else(|| "/".into());
    let worth_rating = Settings::load(paths, &cwd)
        .map(|settings| Duration::from_secs(settings.notify.after_seconds))
        .unwrap_or(Duration::from_secs(120));
    // With the chat module in front, chat's replies are drawn as she writes them and the
    // worker's task as compact lines and a report.
    let chat = link.chat();
    let local = link.is_local();
    let mut replying = false;
    let mut busy = link.hello["busy"] == true;
    let mut started = busy.then(Instant::now);
    let mut waiting: Vec<String> = Vec::new();
    // What this terminal sent and has not seen come back, so only other interfaces' words are shown.
    let mut mine: VecDeque<String> = VecDeque::new();
    // Prompts that are nobody's words (a specialist's report, a schedule): said by a note, not echoed.
    let mut sourced: Vec<String> = Vec::new();
    let mut input = spawn_stdin_reader();
    let prompt = |renderer: &Renderer| {
        print!("{} ", renderer.paint("1;32", "›"));
        let _ = std::io::Write::flush(&mut std::io::stdout());
    };
    if let Some(text) = first.filter(|text| !text.trim().is_empty()) {
        mine.push_back(text.clone());
        link.send(message(&new_id(), &text)).await?;
        replying = chat;
    } else if !busy {
        prompt(&renderer);
    }
    let code = loop {
        tokio::select! {
            record = link.next() => {
                let Some(record) = record? else {
                    let gone = match local {
                        true => "This conversation stopped unexpectedly.",
                        false => "The hub closed the connection. Check: nypi status",
                    };
                    renderer.note("31", gone);
                    break 1;
                };
                // A task working beside the main one: its start and its end are said; what it
                // does on the way would only interleave with the main task's.
                if let Some(lane) = record["lane"].as_str() {
                    match record["type"].as_str().unwrap_or_default() {
                        "task_started" => {
                            let task = excerpt(record["task"].as_str().unwrap_or_default(), 120);
                            renderer.note("2", &format!("[task {lane} started beside] {task}"));
                        }
                        "agent_settled" => {
                            let kind = record["status"]["kind"].as_str().unwrap_or("ended");
                            let review = if record["review"] == "pending" { " · under review" } else { "" };
                            renderer.note("1;36", &format!("── task {lane} {kind}{review} ──"));
                            if review.is_empty()
                                && let Some(answer) = record["answer"].as_str().filter(|a| !a.trim().is_empty())
                            {
                                println!("{}", answer.trim_end());
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                match record["type"].as_str().unwrap_or_default() {
                    "agent_start" if record["module"] != "chat" => {
                        busy = true;
                        started.get_or_insert_with(Instant::now);
                    }
                    "task_reviewed" => {
                        if record["accepted"] != true {
                            renderer.note("2", &format!("[the report fell short; sent back to the worker (round {})]", record["round"]));
                        } else if record["said"] == true
                            && let Some(task) = record["task"].as_str()
                        {
                            renderer.note("2", &format!("[full report: nypi tasks show {task}]"));
                        }
                    }
                    "task_report" => {
                        // Accepted without words of chat's own: the worker's report, whole (D19).
                        if let Some(answer) = record["answer"].as_str().filter(|answer| !answer.trim().is_empty()) {
                            println!("{}", answer.trim_end());
                        }
                        if let Some(sources) = record["sources"].as_str() {
                            renderer.note("2", sources);
                        }
                        if !replying && !busy {
                            prompt(&renderer);
                        }
                    }
                    "task_started" => {
                        busy = true;
                        started = Some(Instant::now());
                        let task = excerpt(record["task"].as_str().unwrap_or_default(), 160);
                        renderer.note("2", &format!("[task started] {task}"));
                    }
                    "chat_settled" => {
                        replying = false;
                        renderer.end_block();
                        if !busy {
                            prompt(&renderer);
                        }
                    }
                    "chat_failed" => {
                        replying = false;
                        let error = record["error"].as_str().unwrap_or("unknown error");
                        renderer.note("31", &format!("[chat model failed: {error}]"));
                        let next = if record["delegated"] == true {
                            "[passed your message straight to the worker]"
                        } else {
                            "[the worker is busy; say it again once the chat model is back, or /abort the task]"
                        };
                        renderer.note("33", next);
                        if !busy {
                            prompt(&renderer);
                        }
                    }
                    "approval_request" => {
                        waiting.extend(record["id"].as_str().map(str::to_string));
                        if let Some(line) = show(&record, true) {
                            renderer.note("1;33", &line);
                            renderer.note("1;33", "  y approves, n denies (or /deny why not)");
                        }
                    }
                    "approval_resolved" => {
                        // Still in the list: somebody else answered it (another terminal, Telegram).
                        let before = waiting.len();
                        waiting.retain(|id| record["id"] != id.as_str());
                        if waiting.len() < before {
                            let verdict = if record["approved"] == true { "approved" } else { "denied" };
                            renderer.note("33", &format!("[{verdict} elsewhere]"));
                        }
                    }
                    "agent_settled" => {
                        busy = false;
                        waiting.clear();
                        renderer.end_block();
                        let elapsed = started.take().map(|at| at.elapsed()).unwrap_or_default();
                        let reviewing = record["review"] == "pending";
                        if chat {
                            let kind = record["status"]["kind"].as_str().unwrap_or("ended");
                            let review = if reviewing { " · under review" } else { "" };
                            renderer.note("1;36", &format!("── task {kind} · {} s{review} ──", elapsed.as_secs()));
                            // The worker's report, whole and as she wrote it (decision D19), unless
                            // the principal reads it first (D78): then its words, or the report it
                            // lets stand, follow.
                            if !reviewing
                                && let Some(answer) = record["answer"].as_str().filter(|answer| !answer.trim().is_empty())
                            {
                                println!("{}", answer.trim_end());
                            }
                        }
                        if let Some(sources) = record["sources"].as_str().filter(|_| !reviewing) {
                            renderer.note("2", sources);
                        }
                        if let Some(note) = settled_note(&record) {
                            renderer.note("33", &note);
                        }
                        if elapsed >= worth_rating && record["task"].is_string() {
                            renderer.note("2", &format!("[{}]", crate::rate::hint()));
                        }
                        if !replying {
                            prompt(&renderer);
                        }
                    }
                    "response" if record["success"] != true => {
                        renderer.note("33", &format!("[{}]", record["error"].as_str().unwrap_or("refused")));
                    }
                    "response" => match record["data"]["disposition"].as_str() {
                        Some("handled") => {
                            mine.pop_front();
                            renderer.note("2", record["data"]["output"].as_str().unwrap_or_default());
                            prompt(&renderer);
                        }
                        Some("queued") => renderer.note("2", "[queued: goes in after the current step]"),
                        _ => {}
                    },
                    "daemon_lagged" | "reflection" => {
                        if let Some(line) = show(&record, true) {
                            renderer.note("2", &line);
                        }
                    }
                    "sourced_prompt" => {
                        sourced.extend(record["text"].as_str().map(str::to_string));
                        if let Some(line) = show(&record, true) {
                            renderer.note("2", &line);
                        }
                    }
                    _ => {
                        if let Some(text) = said(&record) {
                            // Somebody else's words (another terminal, a chat) are shown; one's own are not echoed.
                            if let Some(index) = sourced.iter().position(|known| *known == text) {
                                sourced.remove(index);
                            } else if mine.front() == Some(&text) {
                                mine.pop_front();
                            } else {
                                renderer.note("2", &format!("‹ {text}"));
                            }
                        }
                        match record["module"].as_str() {
                            Some("chat") => draw_chat(&mut renderer, &record),
                            Some("worker") => draw_worker(&mut renderer, &record),
                            _ => {
                                if let Some(event) = runtime_event(&record) {
                                    renderer.handle(&event);
                                }
                            }
                        }
                    }
                }
            }
            line = input.recv() => {
                let Some(line) = line else { break 0 };
                let line = line.trim();
                if line.is_empty() {
                    if !busy {
                        prompt(&renderer);
                    }
                    continue;
                }
                // Behind chat, a command waits for the task to end by itself; a message goes to chat.
                match typed(line, &waiting, Busy::of(busy, chat)) {
                    Typed::Leave => break 0,
                    Typed::Local(text) => renderer.note("2", &text),
                    Typed::Command(command) => {
                        if command["type"] == "approval_response" {
                            waiting.retain(|id| command["id"] != id.as_str());
                        }
                        if command["type"] == "prompt"
                            && let Some(text) = command["message"].as_str()
                        {
                            mine.push_back(text.to_string());
                            replying = chat && !text.starts_with('/') && !text.starts_with('!');
                        }
                        link.send(command).await?;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                // Ctrl-C stops a run, and leaves when nothing runs.
                if !busy && !replying {
                    break 0;
                }
                waiting.clear();
                renderer.note("33", "[stopping the run; Ctrl-C again when idle leaves]");
                link.send(json!({ "id": new_id(), "type": "abort" })).await?;
            }
        }
    };
    for line in link.leave().await.lines() {
        renderer.note("2", &line);
    }
    Ok(code)
}

/// One of the hub's commands from this personal machine (decision D96): carried to the hub on a
/// talk link, run there, its text printed here and its exit code the hub's. `secrets set` reads
/// the value here, unseen, and sends it over the link; a `rate` walk-through asks here and sends
/// each verdict.
pub async fn hub_command_here(
    paths: &Paths,
    rate_walk: Option<i64>,
    argv: Vec<String>,
    input_needed: bool,
) -> anyhow::Result<i32> {
    use std::io::IsTerminal;
    let reach = nypi_hub::reach::Reach::local();
    let input = match input_needed {
        true => {
            let name = argv.get(2).cloned().unwrap_or_default();
            let handle = nypi_vault::secrets::Handle::parse(&name).map_err(anyhow::Error::msg)?;
            let value = crate::secrets::read_value(&handle)?;
            anyhow::ensure!(!value.is_empty(), "nothing was given; {handle} is unchanged");
            Some(value)
        }
        false => None,
    };
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if let Some(days) = rate_walk.filter(|_| interactive) {
        return rate_walk_here(paths, &reach, days).await;
    }
    let (text, code) = admin(paths, &reach, argv, input).await?;
    print!("{text}");
    Ok(code)
}

/// `nypi rate` on a personal node: the hub lists what has no verdict, this machine asks, and each
/// answer goes to the hub as `nypi rate <task> <verdict> [note]`.
async fn rate_walk_here(paths: &Paths, reach: &nypi_hub::reach::Reach, days: i64) -> anyhow::Result<i32> {
    let argv = vec!["rate".to_string(), "--days".to_string(), days.to_string()];
    let (text, code) = admin(paths, reach, argv, None).await?;
    let tasks: Vec<&str> = text.lines().filter(|line| line.starts_with("t-")).collect();
    if tasks.is_empty() {
        print!("{text}");
        return Ok(code);
    }
    println!("{} task(s) have no verdict. Type a verdict and a note, or Enter to skip.", tasks.len());
    let mut rated = 0;
    for line in tasks {
        let id = line.split_whitespace().next().unwrap_or_default().to_string();
        println!("\n{line}");
        print!("Verdict ({}): ", nypi_core::ledger::Verdict::choices());
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        let words: Vec<String> = answer.split_whitespace().map(str::to_string).collect();
        if words.is_empty() {
            continue;
        }
        let argv = ["rate".to_string(), id].into_iter().chain(words).collect();
        let (said, code) = admin(paths, reach, argv, None).await?;
        print!("{said}");
        rated += usize::from(code == 0);
    }
    println!("\nRated {rated}. The week so far: nypi scorecard");
    Ok(0)
}

/// One of the hub's commands typed on the hub that needs its daemon (`update --node`, `--all`):
/// asked of the daemon on its socket, its text printed here (decision D98).
pub async fn hub_command_on_hub(paths: &Paths, argv: Vec<String>) -> anyhow::Result<i32> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let stream = tokio::net::UnixStream::connect(paths.hub_socket())
        .await
        .map_err(|_| anyhow::anyhow!("the hub is not running (nypi hub up starts it)"))?;
    let (read, mut write) = stream.into_split();
    let line = format!("{}\n", json!({ "type": "admin", "argv": argv }));
    write.write_all(line.as_bytes()).await?;
    let mut lines = tokio::io::BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let record: Value = serde_json::from_str(&line).unwrap_or_default();
        if record["type"] == "admin_result" {
            print!("{}", record["output"].as_str().unwrap_or_default());
            return Ok(record["code"].as_i64().unwrap_or(1) as i32);
        }
    }
    anyhow::bail!("the hub closed the link without an answer")
}

/// Run `argv` on the hub over a talk link: its text and exit code.
pub async fn admin(
    paths: &Paths,
    reach: &nypi_hub::reach::Reach,
    argv: Vec<String>,
    input: Option<String>,
) -> anyhow::Result<(String, i32)> {
    use tokio::io::AsyncWriteExt;
    let settings = Settings::load(paths, &paths.home).unwrap_or_default();
    let mut linked = nypi_hub::reach::connect(paths, &settings, nypi_hub::pairing::Purpose::Talk, reach).await?;
    let line = format!("{}\n", json!({ "type": "admin", "argv": argv, "input": input }));
    linked.write.write_all(line.as_bytes()).await?;
    linked.write.flush().await?;
    loop {
        let Some(line) = nypi_node::link::read_line(&mut linked.read, 64 << 20).await? else {
            anyhow::bail!("the hub closed the link without an answer");
        };
        let record: Value = serde_json::from_str(&line).unwrap_or_default();
        if record["type"] == "admin_result" {
            let output = record["output"].as_str().unwrap_or_default().to_string();
            return Ok((output, record["code"].as_i64().unwrap_or(1) as i32));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_shown_as_a_person_would_want_them() {
        let tool = json!({ "type": "tool_execution_start", "call_id": "c1", "tool_name": "bash", "arguments": { "command": "ls -la" } });
        assert_eq!(show(&tool, false).unwrap(), "· bash ls -la");
        let failed = json!({ "type": "tool_execution_end", "tool_name": "bash", "output": { "isError": true } });
        assert_eq!(show(&failed, false).unwrap(), "· bash failed");
        assert_eq!(
            show(&json!({ "type": "tool_execution_end", "tool_name": "bash", "output": { "isError": false } }), true),
            None
        );

        let mut reply = nypi_ai::AssistantMessage::empty("faux", "faux", "faux-1");
        reply.content.push(nypi_ai::ContentBlock::text("On it."));
        let ended =
            json!({ "type": "message_end", "message": serde_json::to_value(Message::Assistant(reply)).unwrap() });
        assert_eq!(show(&ended, true).unwrap(), "On it.");
        assert_eq!(show(&ended, false), None, "send prints only the final answer");
        let user = json!({ "type": "message_end", "message": serde_json::to_value(Message::user_text("hi")).unwrap() });
        assert_eq!(show(&user, true), None, "what the user typed is not echoed");

        let approval = json!({ "type": "approval_request", "id": "approval-2", "tool": "bash", "summary": "git push", "reason": "pushes to a remote" });
        assert_eq!(
            show(&approval, true).unwrap(),
            "[approval needed · approval-2] bash: git push (pushes to a remote)"
        );
        assert_eq!(
            show(&json!({ "type": "alert", "level": "warning", "message": "blocked rm" }), true).unwrap(),
            "[warning] blocked rm"
        );
        // A prompt nobody typed is announced by where it came from, not by its text.
        let report =
            json!({ "type": "sourced_prompt", "origin": "report", "text": "The specialist ... has finished." });
        assert_eq!(show(&report, true).unwrap(), "[a specialist's report came back; Lucia is reviewing it]");
        assert!(runtime_event(&report).is_none());
        assert_eq!(show(&json!({ "type": "message_update" }), true), None);
        assert_eq!(show(&json!({ "no": "type" }), true), None);

        assert_eq!(settled_note(&json!({ "error": "no API key" })).unwrap(), "error: no API key");
        let derived = json!({ "status": { "kind": "done", "text": "no status line given", "synthesized": true } });
        assert_eq!(settled_note(&derived).unwrap(), "[status: done: no status line given]");
        assert_eq!(settled_note(&json!({ "status": { "kind": "done", "text": "ok", "synthesized": false } })), None);
        let hello = json!({ "agent": "Lucia", "model": "faux/faux-1", "cwd": "/Users/x", "busy": true });
        assert_eq!(greeting(&hello), "Lucia · faux/faux-1 · you are at /Users/x · working on a task");
    }

    #[test]
    fn records_become_the_events_the_terminal_draws() {
        let delta = json!({ "type": "message_update", "event": { "type": "text_delta", "index": 0, "delta": "Hel" } });
        assert!(matches!(
            runtime_event(&delta),
            Some(RuntimeEvent::Agent { event: AgentEvent::MessageUpdate { event: nypi_ai::StreamEvent::TextDelta { delta, .. } } })
                if delta == "Hel"
        ));
        let tool = json!({ "type": "tool_execution_end", "call_id": "c", "tool_name": "bash",
                           "output": { "content": [{ "type": "text", "text": "ok" }], "isError": false }, "duration_ms": 12 });
        assert!(matches!(
            runtime_event(&tool),
            Some(RuntimeEvent::Agent { event: AgentEvent::ToolExecutionEnd { duration_ms: Some(12), .. } })
        ));
        let alert = json!({ "type": "alert", "level": "warning", "message": "blocked rm", "at": "t" });
        assert!(matches!(runtime_event(&alert), Some(RuntimeEvent::Alert { alert }) if alert.message == "blocked rm"));
        let retry = json!({ "type": "auto_retry_start", "attempt": 1, "maxAttempts": 3, "delayMs": 2000, "errorMessage": "429" });
        assert!(matches!(runtime_event(&retry), Some(RuntimeEvent::Retry { .. })));
        let compacted = json!({ "type": "compaction_end", "tokensBefore": 9000, "error": null });
        assert!(matches!(
            runtime_event(&compacted),
            Some(RuntimeEvent::CompactionEnd { tokens_before: 9000, error: None })
        ));
        for own in ["response", "agent_settled", "approval_request", "daemon_hello", "daemon_lagged"] {
            assert!(runtime_event(&json!({ "type": own })).is_none(), "{own} is the protocol's own");
        }
        assert!(runtime_event(&json!({ "type": "nothing_known" })).is_none());

        let said = json!({ "type": "message_end", "message": serde_json::to_value(Message::user_text("from the phone")).unwrap() });
        assert_eq!(user_text(&said).as_deref(), Some("from the phone"));
        assert_eq!(user_text(&delta), None);
    }

    /// A runtime on a scripted model, for a hub of this process.
    fn faux_runtime(
        home: &std::path::Path,
        work: &std::path::Path,
        faux: &nypi_ai::providers::faux::FauxProvider,
    ) -> nypi_core::runtime::Runtime {
        nypi_core::runtime::Runtime::new(nypi_core::runtime::RuntimeOptions {
            cwd: work.to_path_buf(),
            paths: Some(Paths { home: home.to_path_buf() }),
            model: Some("faux/faux-1".into()),
            extra_model: Some(nypi_ai::providers::faux::faux_model()),
            stream_fn: Some(faux.stream_fn()),
            session: nypi_core::runtime::SessionMode::Ephemeral,
            ..Default::default()
        })
        .unwrap()
    }

    async fn next_of(link: &mut Link, kind: &str) -> Value {
        let wait = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let record = link.next().await.unwrap().expect("the thread closed");
                if record["type"] == kind {
                    return record;
                }
            }
        });
        wait.await.unwrap_or_else(|_| panic!("no {kind} record"))
    }

    #[tokio::test]
    async fn without_a_daemon_the_same_interface_talks_to_a_hub_of_its_own() {
        use nypi_ai::providers::faux::{FauxProvider, FauxResponse};
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let faux = FauxProvider::new(vec![
            FauxResponse::tool_call("t1", "bash", json!({ "command": "echo would git push" })),
            FauxResponse::text("Pushed.\n\nDone: pushed."),
        ]);
        let runtime = faux_runtime(home.path(), work.path(), &faux);
        let mut link = Link::local(runtime, true).await.unwrap();
        assert!(link.is_local() && !link.chat());
        assert_eq!((link.hello["local"].clone(), link.hello["busy"].clone()), (json!(true), json!(false)));
        assert_eq!(greeting(&link.hello), "Lucia · faux/faux-1 · thinking off");

        // The protocol is the daemon's: a message starts a run, its question is put to this
        // interface, and the answer is told to everyone showing it.
        link.send(message("m1", "push it")).await.unwrap();
        assert_eq!(next_of(&mut link, "response").await["data"]["disposition"], "started");
        let asked = next_of(&mut link, "approval_request").await;
        assert_eq!(asked["summary"], "echo would git push");
        link.send(json!({ "type": "approval_response", "id": asked["id"], "approved": true })).await.unwrap();
        let resolved = next_of(&mut link, "approval_resolved").await;
        assert_eq!((resolved["id"].clone(), resolved["approved"].clone()), (asked["id"].clone(), json!(true)));
        assert_eq!(next_of(&mut link, "agent_settled").await["status"]["text"], "pushed.");

        // Leaving ends a conversation of one's own.
        let Left::Ended { reflections, session } = link.leave().await else { panic!("the hub was this process's own") };
        assert!(reflections.is_empty() && session.is_none(), "nothing to reflect on, and no session file was kept");
    }

    #[tokio::test]
    async fn a_question_nobody_can_answer_is_denied_at_once_and_the_run_goes_on() {
        use nypi_ai::providers::faux::{FauxProvider, FauxResponse};
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let faux = FauxProvider::new(vec![
            FauxResponse::tool_call("t1", "bash", json!({ "command": "echo would git push" })),
            FauxResponse::text("I could not push.\n\nBlocked: nobody was there to approve the push."),
        ]);
        // `nypi -p` with no daemon: nobody is at the run, nobody can attach, and Telegram is not there.
        let mut link = Link::local(faux_runtime(home.path(), work.path(), &faux), false).await.unwrap();
        let code = send_over(&mut link, "push it", true).await.unwrap();
        assert_eq!(code, 0, "blocked is an answer, not a failure of the run");
        let requests = faux.requests();
        let Some(Message::ToolResult(result)) = requests[1].messages.last() else { panic!("no tool result") };
        assert!(result.is_error);
        assert!(text_of(&result.content).contains("no one can answer"), "{}", text_of(&result.content));
    }

    #[test]
    fn typed_lines_become_messages_answers_or_stay_local() {
        let waiting = vec!["approval-1".to_string(), "approval-2".to_string()];
        assert_eq!(typed("/detach", &[], Busy::Idle), Typed::Leave);
        let Typed::Command(said) = typed("book the room for ten", &[], Busy::Working) else { panic!() };
        assert_eq!((said["type"].clone(), said["message"].clone()), (json!("prompt"), json!("book the room for ten")));
        assert_eq!(said["streamingBehavior"], "steer", "a message to a working Lucia steers her");
        let Typed::Command(stop) = typed("/abort", &[], Busy::Working) else { panic!() };
        assert_eq!(stop["type"], "abort");

        let Typed::Command(yes) = typed("/approve", &waiting, Busy::Working) else { panic!() };
        assert_eq!((yes["id"].clone(), yes["approved"].clone()), (json!("approval-1"), json!(true)), "the oldest one");
        let Typed::Command(no) = typed("/deny approval-2 use a branch", &waiting, Busy::Working) else { panic!() };
        assert_eq!(
            (no["id"].clone(), no["approved"].clone(), no["reason"].clone()),
            (json!("approval-2"), json!(false), json!("use a branch"))
        );
        let Typed::Command(no) = typed("/n not now", &waiting, Busy::Working) else { panic!() };
        assert_eq!((no["id"].clone(), no["reason"].clone()), (json!("approval-1"), json!("not now")));
        assert_eq!(typed("/approve", &[], Busy::Working), Typed::Local("No approval is waiting.".into()));

        // While a question waits, a bare yes or no answers it; otherwise it is just a word.
        let Typed::Command(yes) = typed("y", &waiting, Busy::Working) else { panic!() };
        assert_eq!((yes["type"].clone(), yes["approved"].clone()), (json!("approval_response"), json!(true)));
        let Typed::Command(no) = typed("No", &waiting, Busy::Working) else { panic!() };
        assert_eq!((no["id"].clone(), no["approved"].clone()), (json!("approval-1"), json!(false)));
        let Typed::Command(word) = typed("y", &[], Busy::Idle) else { panic!() };
        assert_eq!(word["message"], "y");
        let Typed::Command(steer) = typed("yes please do", &waiting, Busy::Working) else { panic!() };
        assert_eq!(steer["type"], "prompt", "more than a bare yes is a message");

        let Typed::Command(after) = typed("/followup then run the tests", &[], Busy::Working) else { panic!() };
        assert_eq!(
            (after["type"].clone(), after["message"].clone()),
            (json!("follow_up"), json!("then run the tests"))
        );
        let Typed::Command(behind) = typed("/followup then run the tests", &[], Busy::BehindChat) else { panic!() };
        assert_eq!(behind["type"], "follow_up", "behind chat it is still for after the task");
        assert!(matches!(typed("/model", &[], Busy::BehindChat), Typed::Command(_)), "it waits in the thread");
        let Typed::Command(now) = typed("/followup then run the tests", &[], Busy::Idle) else { panic!() };
        assert_eq!((now["type"].clone(), now["message"].clone()), (json!("prompt"), json!("then run the tests")));
        assert!(
            matches!(typed("!ls", &[], Busy::Working), Typed::Local(_)),
            "a shell command is not steered into the model"
        );

        assert!(
            matches!(typed("/rate g", &[], Busy::Working), Typed::Local(_)),
            "a command is not steered into the model"
        );
        let Typed::Command(rate) = typed("/rate g", &[], Busy::Idle) else { panic!() };
        assert_eq!(rate["message"], "/rate g");
    }
}
