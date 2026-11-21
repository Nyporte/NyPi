//! Full-screen terminal UI (`nypi --tui`): the conversation, a live task panel, and an editor that
//! stays usable while work runs (principles A1, A2, A5). It is an interface like the line-based
//! terminal (client.rs): it holds a link to one thread of the hub, sends what is typed as
//! commands, and draws the thread's records. It drives nothing itself.

mod app;
mod input;
mod view;

use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use nypi_core::config::Paths;
use serde_json::json;

use self::app::{App, Level};
use crate::client::{Link, Typed, message, new_id, typed};
use crate::commands;

const HELP: &str = "Keys: Enter send · Alt+Enter newline · ↑/↓ history · PgUp/PgDn scroll · Esc clear · \
Ctrl-C abort (quit when idle) · Ctrl-D quit.
While work runs, a message steers it (with chat on, chat decides). /followup <text> queues one for after.
/abort cancels the task · /task shows it · other commands wait until it is idle.";

/// Restores the terminal however the UI ends, including a panic.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
        ratatui::restore();
    }
}

pub async fn run(mut link: Link, paths: &Paths, first: Option<String>, show_thinking: bool) -> anyhow::Result<i32> {
    let hello = link.hello.clone();
    let mut app = App::new(
        hello["agent"].as_str().unwrap_or("Lucia"),
        hello["chat"].as_str().map(str::to_string),
        hello["model"].as_str().unwrap_or("?"),
        show_thinking,
    );
    app.focus = crate::client::focus_label(&hello);
    if hello["hasKey"] == false {
        app.notice(Level::Warn, format!("No API key found for {}.", app.worker_model));
    }
    // What nobody could be told while no interface was open is told now (decision D57).
    for notice in crate::notify::missed(paths, "the full-screen UI") {
        app.notice(Level::Warn, crate::notify::missed_line(&notice));
    }
    if hello["busy"] == true {
        app.notice(Level::Info, "Lucia is working on a task; what you type steers it.");
        app.on_record(&json!({ "type": "agent_start" }));
    }
    let mut typing = Typing::default();
    let mut terminal = ratatui::init();
    let guard = TerminalGuard;
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    if let Some(text) = first {
        say(&mut app, text);
    }

    let mut code = 0;
    while !app.quit {
        for command in app.out.drain(..) {
            link.send(command).await?;
        }
        terminal.draw(|frame| view::draw(frame, &app))?;
        tokio::select! {
            key = keys.next() => {
                match key {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => on_key(&mut app, key),
                    Some(Ok(Event::Paste(text))) => app.input.insert_str(&text),
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
                typing.edited(&mut app);
            },
            _ = tokio::time::sleep_until(typing.due.unwrap_or_else(tokio::time::Instant::now)), if typing.due.is_some() => {
                typing.publish(&mut app);
            },
            record = link.next() => match record? {
                Some(record) => app.on_record(&record),
                None => {
                    code = 1;
                    break;
                }
            },
            // Redraws the elapsed time while work runs.
            _ = tick.tick() => {}
        }
    }
    drop(guard);
    if code != 0 {
        println!("The conversation closed unexpectedly.");
    }
    for line in link.leave().await.lines() {
        println!("{line}");
    }
    Ok(code)
}

fn on_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT) || key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Char('c') if ctrl => {
            if app.busy() || app.approval.is_some() {
                // Stopping the run denies what it was waiting for.
                app.clear_approvals();
                app.out.push(json!({ "id": new_id(), "type": "abort" }));
                app.notice(Level::Warn, "Aborting");
            } else {
                app.quit = true;
            }
        }
        KeyCode::Char('d') if ctrl && app.input.is_empty() => app.quit = true,
        KeyCode::Char('u') if ctrl => app.input.clear(),
        KeyCode::Char('w') if ctrl => app.input.delete_word(),
        KeyCode::Char('j') if ctrl => app.input.insert('\n'),
        KeyCode::Enter if alt => app.input.insert('\n'),
        KeyCode::Enter => submit(app),
        KeyCode::Char(c) => app.input.insert(c),
        KeyCode::Backspace => app.input.backspace(),
        KeyCode::Delete => app.input.delete(),
        KeyCode::Left => app.input.left(),
        KeyCode::Right => app.input.right(),
        KeyCode::Home => app.input.home(),
        KeyCode::End => app.input.end(),
        KeyCode::Up => app.input.history_prev(),
        KeyCode::Down => app.input.history_next(),
        KeyCode::PageUp => app.scroll_up(10),
        KeyCode::PageDown => app.scroll_down(10),
        KeyCode::Esc => app.input.clear(),
        _ => {}
    }
}

fn submit(app: &mut App) {
    let text = app.input.take();
    // While a question waits, the editor answers it: y approves, anything else denies, and
    // what was typed is the reason.
    if let Some(asked) = app.approval.clone() {
        let answer = text.trim();
        let approved = matches!(answer.to_lowercase().as_str(), "y" | "yes");
        let reason =
            (!approved && !matches!(answer.to_lowercase().as_str(), "" | "n" | "no")).then(|| answer.to_string());
        app.out.push(json!({ "type": "approval_response", "id": asked.id, "approved": approved, "reason": reason }));
        app.resolved(&asked.id);
        app.notice(Level::Info, if approved { "Approved" } else { "Denied" });
        return;
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    match text.as_str() {
        "/help" => return app.notice(Level::Info, format!("{}\n{HELP}", commands::HELP)),
        "/task" => {
            let task = app.task.as_ref().map(|task| task.describe());
            return app.notice(Level::Info, task.unwrap_or_else(|| "No task is running.".into()));
        }
        _ => {}
    }
    // Behind chat, a command waits for the task to end by itself; a message goes to chat.
    let busy = crate::client::Busy::of(app.task.is_some(), app.chat_mode());
    match typed(&text, &app.waiting(), busy) {
        Typed::Leave => app.quit = true,
        Typed::Local(note) => app.notice(Level::Info, note),
        Typed::Command(command) => {
            match command["type"].as_str() {
                Some("abort") => {
                    app.clear_approvals();
                    app.notice(Level::Warn, if app.busy() { "Cancelling the task" } else { "No task is running" });
                }
                Some("follow_up") => {
                    app.user(command["message"].as_str().unwrap_or_default());
                    app.notice(Level::Info, "Queued for when the task ends");
                }
                Some("prompt") => {
                    let said = command["message"].as_str().unwrap_or_default();
                    match said.starts_with(['/', '!']) {
                        // A command's answer comes back as a notice; it is not something said.
                        true => {
                            if app.task.is_some() {
                                app.notice(Level::Info, "Runs when the current task ends");
                            }
                        }
                        false => {
                            app.sent(said);
                            app.replying = app.chat_mode();
                        }
                    }
                }
                _ => {}
            }
            app.out.push(command);
        }
    }
}

/// A message of the user's, to the thread: chat answers it, or it starts or steers the worker.
fn say(app: &mut App, text: String) {
    app.sent(&text);
    app.replying = app.chat_mode();
    app.out.push(message(&new_id(), &text));
}

/// Drafts, reported once the user pauses so recall runs on a settled draft, not every key
/// (decision D34): the thread answers with what Lucia remembers for it.
#[derive(Default)]
struct Typing {
    seen: String,
    sent: String,
    due: Option<tokio::time::Instant>,
}

const TYPING_PAUSE: Duration = Duration::from_millis(250);

impl Typing {
    fn edited(&mut self, app: &mut App) {
        let text = app.input.text();
        if text == self.seen {
            return;
        }
        if text.trim().is_empty() {
            app.hint = None;
        }
        self.seen = text;
        self.due = Some(tokio::time::Instant::now() + TYPING_PAUSE);
    }

    fn publish(&mut self, app: &mut App) {
        self.due = None;
        let draft = app.input.text();
        // Slash commands and approval answers are not messages to recall for.
        let command = draft.trim_start().starts_with(['/', '!']);
        if draft == self.sent || command || app.approval.is_some() {
            return;
        }
        self.sent = draft.clone();
        app.out.push(json!({ "type": "typing", "text": draft }));
    }
}
