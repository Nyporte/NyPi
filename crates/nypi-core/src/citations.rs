//! Citations (principles A4 and B5): every tool result the model sees starts with a reference
//! such as `[t7]`, numbered across the session. The model cites the results its claims rest on
//! by those references; after the run the harness resolves each one to the tool call behind it,
//! flags references that match nothing, and front ends show the sources under the answer.
//! A delegated specialist's references come back renamed (`[analyst:t2]`) inside its report, so
//! they stay distinct and can be cited too.

use std::collections::HashMap;

use nypi_ai::{ContentBlock, Message, text_of};
use serde::Serialize;

use crate::inspector::call_summary;
use crate::pulse::PulseId;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub reference: String,
    pub call_id: String,
    pub tool: String,
    pub summary: String,
    pub is_error: bool,
    /// The `toolFinished` pulse, for `nypi trace`.
    pub pulse: Option<PulseId>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Citation {
    /// A tool call of this session.
    Tool { source: Source },
    /// A specialist's reference quoted in a delegation report this session.
    Delegated { reference: String },
    /// Matches nothing: the claim it supports is unverified.
    Unknown { reference: String },
}

/// What a report cites, as checked by the harness.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Citations {
    pub cited: Vec<Citation>,
    /// Tool results the run produced; a report that cites none of them is uncited.
    pub tool_results: usize,
}

impl Citations {
    pub fn is_empty(&self) -> bool {
        self.cited.is_empty() && self.tool_results == 0
    }

    pub fn unknown(&self) -> usize {
        self.cited.iter().filter(|c| matches!(c, Citation::Unknown { .. })).count()
    }

    /// Lines to show under the answer, or None when there is nothing to say.
    pub fn footer(&self) -> Option<String> {
        if self.cited.is_empty() {
            return (self.tool_results > 0)
                .then(|| format!("Sources: none cited, although {} tool result(s) back this run.", self.tool_results));
        }
        let mut lines = vec!["Sources:".to_string()];
        for citation in &self.cited {
            lines.push(match citation {
                Citation::Tool { source } => {
                    let failed = if source.is_error { " (failed)" } else { "" };
                    let trace = source.pulse.as_ref().map(|p| format!(" · trace {p}")).unwrap_or_default();
                    format!("[{}] {} {}{failed}{trace}", source.reference, source.tool, one_line(&source.summary, 120))
                }
                Citation::Delegated { reference } => format!("[{reference}] cited from a specialist's report"),
                Citation::Unknown { reference } => {
                    format!("[{reference}] matches no tool result in this session: treat that claim as unverified")
                }
            });
        }
        Some(lines.join("\n"))
    }
}

/// Every reference a session has handed out, by reference.
#[derive(Default)]
pub struct CitationIndex {
    next: u64,
    sources: HashMap<String, Source>,
    by_call: HashMap<String, String>,
}

impl CitationIndex {
    /// Rebuild from a transcript, so references keep counting up across restarts.
    pub fn from_messages(messages: &[Message]) -> Self {
        let mut index = CitationIndex { next: 1, ..Default::default() };
        let mut calls: HashMap<String, (String, String)> = HashMap::new();
        for message in messages {
            match message {
                Message::System(_) => {}
                Message::Assistant(assistant) => {
                    for call in assistant.tool_calls() {
                        calls.insert(call.id.clone(), (call.name.clone(), call_summary(&call.name, &call.arguments)));
                    }
                }
                Message::ToolResult(result) => {
                    let Some(reference) = leading_reference(&text_of(&result.content)) else { continue };
                    if let Some(n) = reference.strip_prefix('t').and_then(|n| n.parse::<u64>().ok()) {
                        index.next = index.next.max(n + 1);
                    }
                    let (tool, summary) = calls.get(&result.tool_call_id).cloned().unwrap_or_default();
                    index.insert(Source {
                        reference,
                        call_id: result.tool_call_id.clone(),
                        tool,
                        summary,
                        is_error: result.is_error,
                        pulse: None,
                    });
                }
                Message::User(_) => {}
            }
        }
        index
    }

    fn insert(&mut self, source: Source) {
        self.by_call.insert(source.call_id.clone(), source.reference.clone());
        self.sources.insert(source.reference.clone(), source);
    }

    /// Hand out the next reference for a finished call.
    pub fn assign(&mut self, call_id: &str, tool: &str, summary: String, is_error: bool) -> String {
        let reference = format!("t{}", self.next.max(1));
        self.next = self.next.max(1) + 1;
        self.insert(Source {
            reference: reference.clone(),
            call_id: call_id.to_string(),
            tool: tool.to_string(),
            summary,
            is_error,
            pulse: None,
        });
        reference
    }

    pub fn set_pulse(&mut self, call_id: &str, pulse: PulseId) {
        if let Some(reference) = self.by_call.get(call_id)
            && let Some(source) = self.sources.get_mut(reference)
        {
            source.pulse = Some(pulse);
        }
    }

    pub fn get(&self, reference: &str) -> Option<&Source> {
        self.sources.get(reference)
    }

    /// Check what `answer` cites. `transcript` is the tool result text a delegated reference may
    /// have come from.
    pub fn resolve(&self, answer: &str, transcript: &[Message], tool_results: usize) -> Citations {
        let mut cited = Vec::new();
        let mut seen = Vec::new();
        for reference in references(answer) {
            if seen.contains(&reference) {
                continue;
            }
            seen.push(reference.clone());
            let citation = if let Some(source) = self.sources.get(&reference) {
                Citation::Tool { source: source.clone() }
            } else if reference.contains(':') && quoted_in_results(transcript, &format!("[{reference}]")) {
                Citation::Delegated { reference }
            } else {
                Citation::Unknown { reference }
            };
            cited.push(citation);
        }
        Citations { cited, tool_results }
    }
}

/// Put the reference in front of the result the model reads.
pub fn mark(content: &mut Vec<ContentBlock>, reference: &str) {
    let tag = format!("[{reference}]");
    match content.iter_mut().find_map(|b| match b {
        ContentBlock::Text { text, .. } => Some(text),
        _ => None,
    }) {
        Some(text) => *text = format!("{tag} {text}"),
        None => content.insert(0, ContentBlock::text(tag)),
    }
}

fn leading_reference(text: &str) -> Option<String> {
    let rest = text.strip_prefix('[')?;
    let (reference, _) = rest.split_once(']')?;
    is_reference(reference).then(|| reference.to_string())
}

/// `t12`, or a specialist's `analyst:t3`.
fn is_reference(text: &str) -> bool {
    let local = text.rsplit(':').next().unwrap_or_default();
    let prefix_ok = text.split(':').count() <= 2
        && text.split(':').next().is_some_and(|p| p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    prefix_ok && local.len() > 1 && local.starts_with('t') && local[1..].chars().all(|c| c.is_ascii_digit())
}

/// Every `[t12]` or `[name:t3]` in `text`, in order.
pub fn references(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        rest = &rest[start + 1..];
        if let Some(end) = rest.find(']') {
            // `[t1, t2]` and `[t1][t2]` both cite two results.
            for part in rest[..end].split(',') {
                let part = part.trim();
                if is_reference(part) {
                    found.push(part.to_string());
                }
            }
        }
    }
    found
}

/// Rename a specialist's references so they cannot be mistaken for the delegating agent's.
pub fn namespace(text: &str, name: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..=start]);
        rest = &rest[start + 1..];
        let Some(end) = rest.find(']') else { break };
        let inner = &rest[..end];
        let parts: Vec<String> = inner
            .split(',')
            .map(|part| {
                let trimmed = part.trim();
                if is_reference(trimmed) && !trimmed.contains(':') {
                    part.replace(trimmed, &format!("{name}:{trimmed}"))
                } else {
                    part.to_string()
                }
            })
            .collect();
        out.push_str(&parts.join(","));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn quoted_in_results(transcript: &[Message], needle: &str) -> bool {
    transcript.iter().any(|m| matches!(m, Message::ToolResult(r) if text_of(&r.content).contains(needle)))
}

fn one_line(text: &str, limit: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > limit { format!("{}…", line.chars().take(limit).collect::<String>()) } else { line }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_references_and_namespaces_them() {
        assert_eq!(
            references("Tests pass [t3], see [t1, t2] and [analyst:t4]; not [x] or [t] or [3]."),
            ["t3", "t1", "t2", "analyst:t4"]
        );
        assert_eq!(
            namespace("Read it [t1]; both [t2, t3]; keep [x:t9] and [note].", "coder"),
            "Read it [coder:t1]; both [coder:t2, coder:t3]; keep [x:t9] and [note]."
        );
    }

    #[test]
    fn resolves_known_delegated_and_unknown_references() {
        let mut index = CitationIndex::from_messages(&[]);
        let a = index.assign("c1", "bash", "cargo test".into(), false);
        let b = index.assign("c2", "read", "docs/x.md".into(), true);
        assert_eq!((a.as_str(), b.as_str()), ("t1", "t2"));
        index.set_pulse("c1", PulseId("p-1".into()));
        let report = nypi_ai::ToolResultMessage {
            tool_call_id: "c3".into(),
            tool_name: "delegate".into(),
            content: vec![ContentBlock::text("[t3] Specialist report ... [analyst:t1] read notes.md")],
            details: None,
            is_error: false,
            timestamp: 0,
            duration_ms: None,
        };
        let cited =
            index.resolve("Pass [t1]. Notes say so [analyst:t1]. Made up [t9].", &[Message::ToolResult(report)], 2);
        assert_eq!(cited.cited.len(), 3);
        assert_eq!(cited.unknown(), 1);
        let footer = cited.footer().unwrap();
        assert!(footer.contains("[t1] bash cargo test · trace p-1"), "{footer}");
        assert!(footer.contains("[analyst:t1] cited from a specialist's report"));
        assert!(footer.contains("[t9] matches no tool result"));
        assert_eq!(
            index.resolve("No evidence here.", &[], 2).footer().unwrap(),
            "Sources: none cited, although 2 tool result(s) back this run."
        );
        assert!(index.resolve("Hello.", &[], 0).footer().is_none());
    }

    #[test]
    fn numbering_continues_from_the_transcript() {
        let mut assistant = nypi_ai::AssistantMessage::empty("p", "p", "m");
        assistant.content = vec![ContentBlock::ToolCall {
            id: "c7".into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": "a.rs" }),
            arguments_error: None,
            thought_signature: None,
        }];
        let result = nypi_ai::ToolResultMessage {
            tool_call_id: "c7".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::text("[t7] fn main() {}")],
            details: None,
            is_error: false,
            timestamp: 0,
            duration_ms: None,
        };
        let mut index = CitationIndex::from_messages(&[Message::Assistant(assistant), Message::ToolResult(result)]);
        assert_eq!(index.get("t7").map(|s| (s.tool.as_str(), s.summary.as_str())), Some(("read", "a.rs")));
        assert_eq!(index.assign("c8", "bash", String::new(), false), "t8");
        let mut content = vec![ContentBlock::text("ok")];
        mark(&mut content, "t8");
        assert_eq!(text_of(&content), "[t8] ok");
    }
}
