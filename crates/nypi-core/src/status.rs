//! The never-silent rule (principle B6) in code: read the status line every run must end with,
//! and derive one from the stop reason when the model's reply cannot supply it.

use nypi_ai::{AssistantMessage, StopReason};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusKind {
    Done,
    Blocked,
    Failed,
    Continuing,
}

impl StatusKind {
    pub fn label(self) -> &'static str {
        match self {
            StatusKind::Done => "Done",
            StatusKind::Blocked => "Blocked",
            StatusKind::Failed => "Failed",
            StatusKind::Continuing => "Continuing",
        }
    }

    /// Statuses that need the user's attention.
    pub fn needs_user(self) -> bool {
        matches!(self, StatusKind::Blocked | StatusKind::Failed)
    }
}

/// How a run ended, as reported by the agent or derived by the harness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RunStatus {
    pub kind: StatusKind,
    pub text: String,
    /// True when the harness derived the status because the reply had none.
    pub synthesized: bool,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.text.is_empty() {
            f.write_str(self.kind.label())
        } else {
            write!(f, "{}: {}", self.kind.label(), self.text)
        }
    }
}

/// Sent when a reply ends without a status line. One nudge per run, so a model that ignores
/// it cannot cause a loop.
pub const STATUS_NUDGE: &str = "[NyPi harness] Your last reply did not end with the required status line. Reply now \
with only the status line: start it with Done:, Blocked:, Failed:, or Continuing:, then give the next step. Do not \
repeat the earlier answer.";

/// Sent when a run ends `Continuing` with no schedule set to bring the agent back (decision
/// D59): the turn is over, so "continuing" is a promise nothing will keep. The agent either does
/// the work now, sets its own way back, or says how things really stand.
pub fn continue_nudge(can_schedule: bool) -> String {
    let way_back = if can_schedule {
        "If the rest truly has to wait (a long job, a time of day, an outside event), set a schedule with the \
         `schedule` tool that brings you back to it, a command condition where you can test for the thing you \
         wait on, then end with Continuing: and say when you will be back. "
    } else {
        ""
    };
    format!(
        "[NyPi harness] You ended with Continuing, but your turn is over: nothing runs from here unless you act, \
         and nobody will prompt you again. Do the remaining work now. {way_back}Otherwise finish, and end with \
         Done:, Blocked: (say exactly what you need from the user), or Failed:."
    )
}

/// The status of a run that kept saying `Continuing` without continuing or setting a way back.
pub fn unkept_promise(last: &RunStatus, nudges: u32) -> RunStatus {
    RunStatus {
        kind: StatusKind::Blocked,
        text: format!(
            "said it would continue ({}) but, asked {nudges} time(s), neither went on nor scheduled a way back; \
             tell it how to proceed",
            last.text
        ),
        synthesized: true,
    }
}

/// Only the tail is searched: the rule puts the status line last, followed at most by the next step.
const TAIL_LINES: usize = 12;

/// Find the status line near the end of a reply.
pub fn parse(text: &str) -> Option<RunStatus> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    for (index, line) in lines.iter().enumerate().skip(start).rev() {
        if let Some((kind, rest)) = classify(line) {
            let text = if rest.is_empty() { lines.get(index + 1).map(|l| clean(l)).unwrap_or_default() } else { rest };
            return Some(RunStatus { kind, text, synthesized: false });
        }
    }
    None
}

/// The status of a finished run when the reply did not state one.
pub fn synthesize(last: Option<&AssistantMessage>) -> RunStatus {
    let (kind, text) = match last {
        None => (StatusKind::Failed, "the run produced no reply".to_string()),
        Some(message) => match message.stop_reason {
            StopReason::Error => {
                (StatusKind::Failed, message.error_message.clone().unwrap_or_else(|| "the model request failed".into()))
            }
            StopReason::Aborted => (StatusKind::Blocked, "stopped by the user before it finished".into()),
            StopReason::Length => (StatusKind::Failed, "the reply was cut off at the output token limit".into()),
            StopReason::Stop | StopReason::ToolUse => {
                let first = message.text().lines().find(|l| !l.trim().is_empty()).map(clean).unwrap_or_default();
                let first: String = first.chars().take(200).collect();
                (StatusKind::Done, format!("no status line given; reply began: {first}"))
            }
        },
    };
    RunStatus { kind, text, synthesized: true }
}

fn classify(line: &str) -> Option<(StatusKind, String)> {
    let mut rest = strip_decoration(line);
    if let Some(after) = strip_word(rest, "status") {
        rest = strip_decoration(after);
    }
    for (word, kind) in [
        ("done", StatusKind::Done),
        ("blocked", StatusKind::Blocked),
        ("failed", StatusKind::Failed),
        ("continuing", StatusKind::Continuing),
    ] {
        if let Some(after) = strip_word(rest, word) {
            return Some((kind, clean(after)));
        }
    }
    None
}

/// Markdown bullets, emphasis, headings, and emoji before the keyword.
fn strip_decoration(text: &str) -> &str {
    text.trim_start_matches(|c: char| !c.is_alphanumeric())
}

/// `word` at the start, followed by a separator or the end of the line. "Failed tests: 3" is
/// not a status line, "**Failed:** tests" is.
fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let head = text.get(..word.len())?;
    if !head.eq_ignore_ascii_case(word) {
        return None;
    }
    let after = &text[word.len()..];
    let tail = after.trim_start_matches(['*', '_', ' ']);
    match tail.chars().next() {
        None => Some(""),
        // Not `|`: a table row such as `| Done | 3 |` is data, not the status line.
        Some(':' | '：' | '-' | '—' | '–' | '.') => Some(tail),
        _ => None,
    }
}

fn clean(text: &str) -> String {
    text.trim_start_matches([':', '：', '-', '—', '–', '.', '|', '*', '_', ' ']).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_status_lines_in_common_shapes() {
        let cases = [
            ("All good.\n\nDone: tests pass. Next: ship it.", StatusKind::Done, "tests pass. Next: ship it."),
            ("x\n**Blocked:** I need the API token.", StatusKind::Blocked, "I need the API token."),
            (
                "x\n- Failed — the build broke; retrying with a clean target",
                StatusKind::Failed,
                "the build broke; retrying with a clean target",
            ),
            ("x\n✅ Done: shipped", StatusKind::Done, "shipped"),
            ("x\nStatus: Continuing - the job is still running", StatusKind::Continuing, "the job is still running"),
            ("x\n## Done\nNext step: review the diff", StatusKind::Done, "Next step: review the diff"),
            ("Done: first\nmore\nBlocked: last wins", StatusKind::Blocked, "last wins"),
        ];
        for (text, kind, expected) in cases {
            let status = parse(text).unwrap_or_else(|| panic!("no status in {text:?}"));
            assert_eq!((status.kind, status.text.as_str()), (kind, expected), "{text:?}");
        }
    }

    #[test]
    fn ignores_prose_that_merely_starts_with_a_keyword() {
        let table = "Results:\n| Step | Tests |\n| Done | 3 |\n| Failed | 1 |";
        for text in ["Failed tests: 3 of 10", "Done with the refactor, see above", "Blockedness is rare", "", table] {
            assert!(parse(text).is_none(), "{text:?}");
        }
    }

    #[test]
    fn only_the_tail_counts() {
        let mut text = "Done: early\n".to_string();
        text.push_str(&"line\n".repeat(TAIL_LINES + 1));
        assert!(parse(&text).is_none());
    }
}
