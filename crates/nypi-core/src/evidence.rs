//! A task's evidence (decision D87): what the Principal reviews a settled task by, beside the
//! worker's own words. The worker writes `report.md` in its workspace (a contract check, its
//! findings and deliverables, the paths it ruled out); the harness draws the **evidence
//! manifest** from the task's session, which the worker cannot write: every `[tN]` the report
//! cites, resolved to the call behind it (tool, arguments, status, time, an excerpt of what came
//! back), and whether a check ran after the task's last change to a file. The Principal reads
//! any step in full with `inspect_task_evidence`, from the same session file, in process.

use std::path::Path;

use nypi_ai::{Message, text_of};
use serde_json::Value;

use crate::config::Paths;
use crate::ledger::{Ledger, TaskId};

/// An excerpt of a cited step's output, in the manifest.
pub const EXCERPT_CHARS: usize = 400;
/// What `inspect_task_evidence` returns at most, per call.
pub const INSPECT_CHARS: usize = 8000;
/// The report the Principal is shown, at most.
pub const REPORT_CHARS: usize = 6000;
/// The worker's report, in its workspace.
pub const REPORT_FILE: &str = "report.md";
/// Tools that change files.
const FILE_CHANGES: [&str; 2] = ["write", "edit"];
/// Tools that look at what is there. `bash` is both: it changes files and it runs the checks.
const CHECKS: [&str; 5] = ["read", "grep", "find", "ls", "bash"];

/// One tool call of a task's session, by the reference its result was given.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub reference: String,
    pub tool: String,
    pub arguments: Value,
    pub is_error: bool,
    pub duration_ms: Option<u64>,
    /// What came back, without its leading reference.
    pub output: String,
    /// Its place among the session's messages.
    pub position: usize,
}

impl Step {
    /// The call in a line: the tool and what it was given.
    pub fn call(&self) -> String {
        let summary = one_line(&crate::inspector::call_summary(&self.tool, &self.arguments), 200);
        if summary.is_empty() { self.tool.clone() } else { format!("{} {summary}", self.tool) }
    }

    fn status(&self) -> String {
        let state = if self.is_error { "error" } else { "ok" };
        match self.duration_ms {
            Some(ms) => format!("{state} · {}", duration(ms)),
            None => state.to_string(),
        }
    }
}

fn duration(ms: u64) -> String {
    if ms < 1000 { format!("{ms} ms") } else { format!("{:.1} s", ms as f64 / 1000.0) }
}

/// Every message of a session file, in the order written, compacted ones included.
fn messages(session: &Path) -> Vec<Message> {
    let Ok(store) = crate::session::SessionStore::open(session) else { return Vec::new() };
    store
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            crate::session::Entry::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// Every referenced tool call of a session, in order.
pub fn session_steps(session: &Path) -> Vec<Step> {
    steps_of(&messages(session))
}

fn steps_of(messages: &[Message]) -> Vec<Step> {
    let mut calls: std::collections::HashMap<String, (String, Value)> = Default::default();
    let mut steps = Vec::new();
    for (position, message) in messages.iter().enumerate() {
        match message {
            Message::Assistant(reply) => {
                for call in reply.tool_calls() {
                    calls.insert(call.id.clone(), (call.name.clone(), call.arguments.clone()));
                }
            }
            Message::ToolResult(result) => {
                let text = text_of(&result.content);
                let Some((reference, output)) = leading_reference(&text) else { continue };
                let (tool, arguments) =
                    calls.get(&result.tool_call_id).cloned().unwrap_or_else(|| (result.tool_name.clone(), Value::Null));
                steps.push(Step {
                    reference,
                    tool,
                    arguments,
                    is_error: result.is_error,
                    duration_ms: result.duration_ms,
                    output: output.to_string(),
                    position,
                });
            }
            _ => {}
        }
    }
    steps
}

fn leading_reference(text: &str) -> Option<(String, &str)> {
    let rest = text.strip_prefix("[t")?;
    let (number, output) = rest.split_once(']')?;
    number.chars().all(|c| c.is_ascii_digit()).then(|| (format!("t{number}"), output.trim_start()))
}

/// A task's steps, and where its latest run began among them (the prompt of that run, when
/// it is found; the start of the session otherwise).
pub struct TaskSteps {
    pub steps: Vec<Step>,
    pub run_start: usize,
}

/// The steps of ledger task `task`, read from its session.
pub fn task_steps(paths: &Paths, task: &TaskId) -> Option<TaskSteps> {
    let record = Ledger::new(paths.ledger()).get(task)?;
    let session = record.session_path.as_ref()?;
    let messages = messages(session);
    let anchor: String = record.prompt.chars().take(200).collect();
    let run_start = messages
        .iter()
        .rposition(|message| matches!(message, Message::User(user) if !anchor.is_empty() && text_of(&user.content).contains(&anchor)))
        .unwrap_or(0);
    Some(TaskSteps { steps: steps_of(&messages), run_start })
}

/// What the harness found behind a report.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Manifest {
    /// The cited steps, in the order first cited.
    pub cited: Vec<Step>,
    /// References that match no step of the task's session.
    pub unknown: Vec<String>,
    /// Calls of the latest run that changed files (`write`, `edit`) or ran commands (`bash`).
    pub changes: Vec<String>,
    /// The latest run's last change to a file, and the check that ran after it, if any.
    pub last_change: Option<String>,
    pub checked_after: Option<String>,
    /// Tool results of the latest run.
    pub run_steps: usize,
}

impl Manifest {
    /// The task changed a file and nothing looked at the result afterwards.
    pub fn unchecked_change(&self) -> bool {
        self.last_change.is_some() && self.checked_after.is_none()
    }

    /// The manifest as the Principal reads it. `done` says the worker reported Done.
    pub fn render(&self, done: bool) -> String {
        let mut lines = vec![
            "## Harness Evidence Manifest".to_string(),
            "(Drawn by the harness from the task's session; the worker did not write this.)".to_string(),
        ];
        if self.cited.is_empty() && self.unknown.is_empty() {
            lines.push(format!(
                "- The report cites no tool result, although the latest run has {} step(s).",
                self.run_steps
            ));
        }
        for step in &self.cited {
            lines.push(format!("- [{}] {} · {}", step.reference, step.call(), step.status()));
            let excerpt = excerpt(&step.output, EXCERPT_CHARS);
            if !excerpt.is_empty() {
                lines.push(format!("  > {excerpt}"));
            }
        }
        for reference in &self.unknown {
            lines.push(format!("- [{reference}] matches no tool result of this task: treat that claim as unverified"));
        }
        if self.changes.is_empty() {
            lines.push("Changes: the latest run changed no file and ran no command.".into());
        } else {
            lines.push(format!("Changes in the latest run: {}", self.changes.join("; ")));
        }
        match (&self.last_change, &self.checked_after) {
            (Some(change), Some(check)) => {
                lines.push(format!("Checked after the last file change ({change}): {check}"))
            }
            (Some(change), None) if done => lines.push(format!(
                "WARNING: the task changed a file ({change}) and reported Done with no check after it (no read, grep, \
                 find, ls, or bash ran afterwards)."
            )),
            (Some(change), None) => lines.push(format!("Nothing checked the last file change ({change}).")),
            (None, _) => {}
        }
        lines.join("\n")
    }
}

/// The manifest behind `cited_in` (the report and the final reply), for `steps`.
pub fn manifest(steps: &TaskSteps, cited_in: &str) -> Manifest {
    let mut manifest = Manifest::default();
    let mut seen = Vec::new();
    for reference in crate::citations::references(cited_in) {
        if reference.contains(':') || seen.contains(&reference) {
            continue;
        }
        seen.push(reference.clone());
        match steps.steps.iter().find(|step| step.reference == reference) {
            Some(step) => manifest.cited.push(step.clone()),
            None => manifest.unknown.push(reference),
        }
    }
    let run: Vec<&Step> = steps.steps.iter().filter(|step| step.position >= steps.run_start).collect();
    manifest.run_steps = run.len();
    let mut last_change_at = None;
    for (index, step) in run.iter().enumerate() {
        if FILE_CHANGES.contains(&step.tool.as_str()) || step.tool == "bash" {
            manifest.changes.push(format!("[{}] {}", step.reference, one_line(&step.call(), 100)));
        }
        if FILE_CHANGES.contains(&step.tool.as_str()) && !step.is_error {
            last_change_at = Some(index);
            manifest.last_change = Some(format!("[{}] {}", step.reference, one_line(&step.call(), 100)));
        }
    }
    if let Some(at) = last_change_at {
        manifest.checked_after = run[at + 1..]
            .iter()
            .find(|step| CHECKS.contains(&step.tool.as_str()))
            .map(|step| format!("[{}] {}", step.reference, one_line(&step.call(), 100)));
    }
    manifest
}

/// A settled task as the Principal reviews it: the brief as it reads now, the worker's report
/// (`report.md`, else its final reply), and the manifest.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dossier {
    pub brief: Option<String>,
    pub findings: Option<String>,
    /// `report.md`, when the worker wrote one.
    pub report: Option<String>,
    pub manifest: String,
    pub unchecked_change: bool,
}

/// The dossier of ledger task `task`, whose workspace is `workspace` and final reply `answer`.
pub fn dossier(
    paths: &Paths,
    vault: Option<&nypi_vault::tools::VaultStore>,
    task: &TaskId,
    workspace: Option<&str>,
    answer: &str,
    done: bool,
) -> Dossier {
    let read = |name: &str| {
        let (vault, workspace) = (vault?, workspace?);
        vault.read(&format!("{workspace}/{name}")).ok().filter(|text| !text.trim().is_empty())
    };
    let report = read(REPORT_FILE);
    let cited_in = format!("{}\n{answer}", report.as_deref().unwrap_or_default());
    let (manifest, unchecked_change) = match task_steps(paths, task) {
        Some(steps) => {
            let manifest = manifest(&steps, &cited_in);
            (manifest.render(done), manifest.unchecked_change() && done)
        }
        None => ("## Harness Evidence Manifest\n(The task's session could not be read.)".to_string(), false),
    };
    Dossier { brief: read("brief.md"), findings: read("findings.md"), report, manifest, unchecked_change }
}

/// `inspect_task_evidence`'s answer: the steps `refs` names in full, or every step in a line
/// each without any, cut at `INSPECT_CHARS`.
pub fn inspect(steps: &[Step], refs: &[String]) -> String {
    let mut text = String::new();
    if refs.is_empty() {
        text.push_str(&format!("{} step(s); name refs to read any in full.\n", steps.len()));
        for step in steps {
            text.push_str(&format!("- [{}] {} · {}\n", step.reference, step.call(), step.status()));
        }
    } else {
        let share = INSPECT_CHARS / refs.len().max(1);
        for reference in refs {
            let reference = reference.trim().trim_start_matches('[').trim_end_matches(']');
            match steps.iter().find(|step| step.reference == reference) {
                Some(step) => {
                    let arguments = serde_json::to_string_pretty(&step.arguments).unwrap_or_default();
                    let mut output: String =
                        step.output.chars().take(share.saturating_sub(arguments.len() + 120)).collect();
                    if output.len() < step.output.len() {
                        output.push_str("… [cut]");
                    }
                    text.push_str(&format!(
                        "## [{}] {} · {}\nArguments:\n{arguments}\nOutput:\n{output}\n\n",
                        step.reference,
                        step.tool,
                        step.status()
                    ));
                }
                None => text.push_str(&format!("## [{reference}]\nNo step of this task has that reference.\n\n")),
            }
        }
    }
    if text.chars().count() > INSPECT_CHARS {
        text = format!("{}… [cut at {INSPECT_CHARS} characters]", text.chars().take(INSPECT_CHARS).collect::<String>());
    }
    text
}

fn excerpt(text: &str, chars: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= chars { line } else { format!("{}…", line.chars().take(chars).collect::<String>()) }
}

fn one_line(text: &str, chars: usize) -> String {
    excerpt(text, chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nypi_ai::{AssistantMessage, ContentBlock, StopReason, ToolResultMessage};
    use serde_json::json;

    fn call(id: &str, name: &str, arguments: Value) -> Message {
        let mut reply = AssistantMessage::empty("faux", "faux", "faux-1");
        reply.stop_reason = StopReason::ToolUse;
        reply.content = vec![ContentBlock::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            arguments_error: None,
            thought_signature: None,
        }];
        Message::Assistant(reply)
    }

    fn result(id: &str, name: &str, text: &str, is_error: bool) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: name.into(),
            content: vec![ContentBlock::text(text)],
            details: None,
            is_error,
            timestamp: 0,
            duration_ms: Some(1200),
        })
    }

    fn steps(messages: Vec<Message>) -> TaskSteps {
        TaskSteps { steps: steps_of(&messages), run_start: 0 }
    }

    #[test]
    fn cited_steps_are_resolved_and_an_unchecked_change_is_flagged() {
        let session = vec![
            Message::user_text("fix the importer"),
            call("c1", "read", json!({ "path": "src/import.rs" })),
            result("c1", "read", "[t1] fn import() {}", false),
            call("c2", "edit", json!({ "path": "src/import.rs" })),
            result("c2", "edit", "[t2] Edited src/import.rs", false),
        ];
        let manifest = manifest(&steps(session.clone()), "Read it [t1], fixed it [t2], tests pass [t9].");
        assert_eq!(manifest.cited.iter().map(|s| s.reference.as_str()).collect::<Vec<_>>(), ["t1", "t2"]);
        assert_eq!(manifest.unknown, ["t9"]);
        assert!(manifest.unchecked_change());
        let shown = manifest.render(true);
        assert!(
            shown.contains("- [t1] read src/import.rs · ok · 1.2 s") && shown.contains("> fn import() {}"),
            "{shown}"
        );
        assert!(shown.contains("[t9] matches no tool result") && shown.contains("WARNING"), "{shown}");

        let mut checked = session;
        checked.push(call("c3", "bash", json!({ "command": "cargo test" })));
        checked.push(result("c3", "bash", "[t3] test result: ok", false));
        let after = super::manifest(&steps(checked.clone()), "Fixed [t2], tests pass [t3].");
        assert!(!after.unchecked_change());
        assert!(after.render(true).contains("Checked after the last file change ([t2] edit"));
        let full = inspect(&steps(checked).steps, &["t3".into(), "[t7]".into()]);
        assert!(full.contains("## [t3] bash · ok") && full.contains("test result: ok"), "{full}");
        assert!(full.contains("## [t7]\nNo step"), "{full}");
    }
}
