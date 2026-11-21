//! Terminal rendering of runtime events: streamed text, dimmed thinking, compact tool
//! activity, alerts, and a usage footer.

use std::io::{IsTerminal, Write};

use nypi_agent::{AgentEvent, ToolOutput};
use nypi_ai::{Message, StreamEvent, Usage, text_of};
use nypi_core::inspector::{Alert, AlertLevel};
use nypi_core::retry::RetryNotice;
use nypi_core::runtime::RuntimeEvent;
pub use nypi_hub::records::tool_summary;

#[derive(PartialEq)]
enum Block {
    None,
    Text,
    Thinking,
}

pub struct Renderer {
    color: bool,
    show_thinking: bool,
    block: Block,
    usage: Usage,
}

const TOOL_PREVIEW_LINES: usize = 6;

impl Renderer {
    pub fn new(show_thinking: bool) -> Self {
        Renderer { color: std::io::stdout().is_terminal(), show_thinking, block: Block::None, usage: Usage::default() }
    }

    pub fn paint(&self, code: &str, text: &str) -> String {
        if self.color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
    }

    pub fn handle(&mut self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::Agent { event } => self.agent_event(event),
            RuntimeEvent::Alert { alert } => self.alert(alert),
            RuntimeEvent::CompactionStart { reason } => {
                self.break_block();
                println!("{}", self.paint("2", &format!("[compacting context: {reason}]")));
            }
            RuntimeEvent::CompactionEnd { tokens_before, error } => {
                if error.is_none() {
                    println!("{}", self.paint("2", &format!("[compacted; was ~{tokens_before} tokens]")))
                }
            }
            RuntimeEvent::Retry { notice } => {
                if let Some(note) = retry_note(notice) {
                    self.break_block();
                    println!("{}", self.paint("33", &format!("[{note}]")));
                }
            }
        }
        let _ = std::io::stdout().flush();
    }

    pub fn alert(&mut self, alert: &Alert) {
        self.break_block();
        let (code, label) = match alert.level {
            AlertLevel::Notice => ("1;36", "NOTICE"),
            AlertLevel::Warning => ("1;33", "WARNING"),
            AlertLevel::Error => ("1;31", "ALERT"),
        };
        eprintln!("{}", self.paint(code, &format!("[{label}] {}", alert.message)));
    }

    fn agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageUpdate { event } => match event {
                StreamEvent::TextDelta { delta, .. } => {
                    if self.block != Block::Text {
                        self.break_block();
                        self.block = Block::Text;
                    }
                    print!("{delta}");
                }
                StreamEvent::ThinkingDelta { delta, .. } if self.show_thinking => {
                    if self.block != Block::Thinking {
                        self.break_block();
                        self.block = Block::Thinking;
                    }
                    print!("{}", self.paint("2;3", delta));
                }
                _ => {}
            },
            AgentEvent::MessageEnd { message: Message::Assistant(assistant) } => {
                self.usage.add(&assistant.usage);
                if let Some(error) = &assistant.error_message {
                    self.break_block();
                    let label = if assistant.stop_reason == nypi_ai::StopReason::Aborted { "aborted" } else { "error" };
                    println!("{}", self.paint("31", &format!("[{label}] {error}")));
                }
            }
            AgentEvent::ToolExecutionStart { tool_name, arguments, .. } => {
                self.break_block();
                println!(
                    "{} {}",
                    self.paint("1;36", &format!("● {tool_name}")),
                    self.paint("36", &tool_summary(tool_name, arguments))
                );
            }
            AgentEvent::ToolExecutionEnd { output, duration_ms, .. } => self.tool_end(output, *duration_ms),
            AgentEvent::AgentEnd { .. } => {
                self.break_block();
                let u = &self.usage;
                let cost = if u.cost.total > 0.0 { format!(", ${:.4}", u.cost.total) } else { String::new() };
                println!(
                    "{}",
                    self.paint("2", &format!("[{} in / {} out / {} cached{cost}]", u.input, u.output, u.cache_read))
                );
                self.usage = Usage::default();
            }
            _ => {}
        }
    }

    fn tool_end(&mut self, output: &ToolOutput, duration_ms: Option<u64>) {
        let text = text_of(&output.content);
        let lines: Vec<&str> = text.lines().collect();
        let code = if output.is_error { "31" } else { "2" };
        for line in lines.iter().take(TOOL_PREVIEW_LINES) {
            let line: String = line.chars().take(160).collect();
            println!("{}", self.paint(code, &format!("  │ {line}")));
        }
        let more = lines.len().saturating_sub(TOOL_PREVIEW_LINES);
        let timing = duration_ms.map(|ms| format!(" {:.1}s", ms as f64 / 1000.0)).unwrap_or_default();
        let tail = if more > 0 { format!("  └ … {more} more lines{timing}") } else { format!("  └{timing}") };
        println!("{}", self.paint(code, &tail));
    }

    /// Print one line of `code` style, ending any streamed block first (output from another
    /// module can arrive mid-reply).
    pub fn note(&mut self, code: &str, text: &str) {
        self.break_block();
        println!("{}", self.paint(code, text));
        let _ = std::io::stdout().flush();
    }

    pub fn end_block(&mut self) {
        self.break_block();
    }

    fn break_block(&mut self) {
        if self.block != Block::None {
            println!();
            if self.block == Block::Thinking {
                println!();
            }
        }
        self.block = Block::None;
    }
}

/// What front ends say about a retry. None when there is nothing to say: a reply or summary
/// that came through speaks for itself, and a compaction reports its own failure.
pub fn retry_note(notice: &RetryNotice) -> Option<String> {
    match notice {
        RetryNotice::AutoRetryStart { attempt, max_attempts, delay_ms, error_message } => Some(format!(
            "{}; retrying ({attempt}/{max_attempts}) in {} s",
            short_error(error_message),
            delay_ms.div_ceil(1000)
        )),
        RetryNotice::SummarizationRetryScheduled { attempt, max_attempts, delay_ms, error_message } => Some(format!(
            "{}; retrying the compaction summary ({attempt}/{max_attempts}) in {} s",
            short_error(error_message),
            delay_ms.div_ceil(1000)
        )),
        RetryNotice::AutoRetryEnd { success: false, attempt, final_error } => Some(format!(
            "Gave up after {attempt} {}: {}",
            if *attempt == 1 { "retry" } else { "retries" },
            short_error(final_error.as_deref().unwrap_or("Unknown error"))
        )),
        _ => None,
    }
}

/// An error's first line, cut short: an HTTP error carries up to 2000 characters of body.
fn short_error(error: &str) -> String {
    let line = error.lines().next().unwrap_or_default();
    match line.char_indices().nth(200) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None => line.to_string(),
    }
}
