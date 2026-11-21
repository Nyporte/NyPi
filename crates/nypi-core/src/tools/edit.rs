//! `edit`: one or more exact, unique, non-overlapping text replacements in a file.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use serde_json::{Value, json};

use super::{file_lock, resolve_path, writable};
use crate::protected::Protected;

pub struct EditTool {
    pub cwd: PathBuf,
    pub protected: Arc<Protected>,
}

#[async_trait]
impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".into(),
            description: "Edit a file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. Use multiple edits for separate changes in the same file.".into(),
            parameters: json!({
                "type": "object",
                "required": ["path", "edits"],
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                    "edits": {
                        "type": "array",
                        "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                        "items": {
                            "type": "object",
                            "required": ["oldText", "newText"],
                            "properties": {
                                "oldText": { "type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call." },
                                "newText": { "type": "string", "description": "Replacement text for this targeted edit." }
                            }
                        }
                    }
                }
            }),
        }
    }

    fn prepare_arguments(&self, args: Value) -> Value {
        prepare(args)
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let path = resolve_path(&self.cwd, args["path"].as_str().unwrap_or_default());
        let edits: Vec<(String, String)> = args["edits"]
            .as_array()
            .map(|edits| {
                edits
                    .iter()
                    .map(|e| {
                        (
                            normalize_lf(e["oldText"].as_str().unwrap_or_default()),
                            normalize_lf(e["newText"].as_str().unwrap_or_default()),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let real = writable(&self.protected, &path)?;
        let lock = file_lock(&real);
        let _guard = lock.lock().await;
        let raw = tokio::fs::read_to_string(&real).await.with_context(|| format!("Cannot read {}", path.display()))?;
        let (bom, body) = match raw.strip_prefix('\u{feff}') {
            Some(rest) => ("\u{feff}", rest),
            None => ("", raw.as_str()),
        };
        let crlf = body.contains("\r\n");
        let original = normalize_lf(body);
        let updated = apply_edits(&original, &edits, &path.display().to_string())?;

        let mut output = updated.clone();
        if crlf {
            output = output.replace('\n', "\r\n");
        }
        tokio::fs::write(&real, format!("{bom}{output}"))
            .await
            .with_context(|| format!("Cannot write {}", path.display()))?;

        let diff = similar::TextDiff::from_lines(&original, &updated);
        let first_changed_line = diff
            .ops()
            .iter()
            .find(|op| !matches!(op, similar::DiffOp::Equal { .. }))
            .map(|op| op.new_range().start + 1);
        let patch = diff.unified_diff().context_radius(3).header("original", "modified").to_string();
        let count = edits.len();
        Ok(ToolOutput::text(format!(
            "Successfully replaced {count} block{} in {}.",
            if count == 1 { "" } else { "s" },
            path.display()
        ))
        .with_details(json!({ "path": path, "diff": patch, "firstChangedLine": first_changed_line })))
    }
}

/// Accept the legacy single `oldText`/`newText` shape, a single edit object instead of an
/// array, and, as Pi does, edits sent as a JSON string (some models do). Applying it twice
/// changes nothing, so the hub and a node may both apply it.
pub(crate) fn prepare(mut args: Value) -> Value {
    if let Some(text) = args["edits"].as_str()
        && let Ok(parsed) = serde_json::from_str::<Value>(text)
    {
        args["edits"] = parsed;
    }
    if let Some(object) = args.as_object_mut() {
        if !object.contains_key("edits") {
            if let (Some(old), Some(new)) = (object.remove("oldText"), object.remove("newText")) {
                object.insert("edits".into(), json!([{ "oldText": old, "newText": new }]));
            }
        } else if object["edits"].is_object() {
            let single = object.remove("edits").unwrap();
            object.insert("edits".into(), json!([single]));
        }
    }
    args
}

fn normalize_lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Apply every edit against `content`, each matched in the original text.
pub(crate) fn apply_edits(content: &str, edits: &[(String, String)], label: &str) -> anyhow::Result<String> {
    if edits.is_empty() {
        bail!("No edits provided for {label}.");
    }
    let total = edits.len();
    let name = |i: usize, single: &str| if total == 1 { single.to_string() } else { format!("edits[{i}]") };
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    for (i, (old, _)) in edits.iter().enumerate() {
        if old.is_empty() {
            bail!(
                "{} must not be empty in {label}.",
                if total == 1 { "oldText".into() } else { format!("edits[{i}].oldText") }
            );
        }
        let (start, end) = match find_unique(content, old) {
            Match::Found(start, end) => (start, end),
            Match::Missing => bail!(
                "Could not find {} in {label}. The old text must match exactly including all whitespace and newlines.",
                name(i, "the exact text")
            ),
            Match::Duplicate(n) => bail!(
                "Found {n} occurrences of {} in {label}. The text must be unique. Please provide more context to make it unique.",
                name(i, "the text")
            ),
        };
        spans.push((start, end, i));
    }
    spans.sort();
    for pair in spans.windows(2) {
        if pair[1].0 < pair[0].1 {
            bail!("edits[{}] and edits[{}] overlap in {label}. Merge them into one edit.", pair[0].2, pair[1].2);
        }
    }
    let mut result = content.to_string();
    for (start, end, i) in spans.iter().rev() {
        result.replace_range(*start..*end, &edits[*i].1);
    }
    if result == content {
        bail!("No changes made to {label}. The replacement produced identical content.");
    }
    Ok(result)
}

enum Match {
    Found(usize, usize),
    Missing,
    Duplicate(usize),
}

/// Exact match first; then a fuzzy match that ignores trailing whitespace and Unicode
/// quote/dash/space variants, mapped back to the original byte range.
fn find_unique(content: &str, needle: &str) -> Match {
    let exact: Vec<usize> = content.match_indices(needle).map(|(i, _)| i).collect();
    match exact.len() {
        1 => return Match::Found(exact[0], exact[0] + needle.len()),
        n if n > 1 => return Match::Duplicate(n),
        _ => {}
    }
    let view = FuzzyView::new(content);
    let fuzzy_needle = FuzzyView::new(needle).text;
    if fuzzy_needle.is_empty() {
        return Match::Missing;
    }
    let found: Vec<usize> = view.text.match_indices(&fuzzy_needle).map(|(i, _)| i).collect();
    match found.len() {
        0 => Match::Missing,
        1 => {
            let start = view.starts[found[0]];
            let end = view.ends[found[0] + fuzzy_needle.len() - 1];
            Match::Found(start, end)
        }
        n => Match::Duplicate(n),
    }
}

/// Normalized text plus, for each normalized byte, the original byte range of its character.
struct FuzzyView {
    text: String,
    starts: Vec<usize>,
    ends: Vec<usize>,
}

impl FuzzyView {
    fn new(original: &str) -> Self {
        let mut view = FuzzyView { text: String::new(), starts: Vec::new(), ends: Vec::new() };
        let mut offset = 0;
        let mut lines = original.split('\n').peekable();
        while let Some(line) = lines.next() {
            for (i, c) in line.trim_end().char_indices() {
                view.push(fuzzy_char(c), offset + i, offset + i + c.len_utf8());
            }
            offset += line.len();
            if lines.peek().is_some() {
                view.push('\n', offset, offset + 1);
                offset += 1;
            }
        }
        view
    }

    fn push(&mut self, c: char, start: usize, end: usize) {
        self.text.push(c);
        for _ in 0..c.len_utf8() {
            self.starts.push(start);
            self.ends.push(end);
        }
    }
}

fn fuzzy_char(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn edit(old: &str, new: &str) -> (String, String) {
        (old.into(), new.into())
    }

    #[test]
    fn edits_sent_as_a_json_string_are_accepted() {
        let protected = Arc::new(Protected::from_parts(Path::new("/nowhere/.nypi"), None, Vec::new(), Vec::new()));
        let tool = EditTool { cwd: PathBuf::from("/work"), protected };
        let args = tool.prepare_arguments(json!({ "path": "f", "edits": r#"[{"oldText": "a", "newText": "b"}]"# }));
        assert_eq!(args["edits"], json!([{ "oldText": "a", "newText": "b" }]));
        let single = tool.prepare_arguments(json!({ "path": "f", "edits": r#"{"oldText": "a", "newText": "b"}"# }));
        assert_eq!(single["edits"], json!([{ "oldText": "a", "newText": "b" }]), "a single edit in a string too");
    }

    #[test]
    fn applies_multiple_disjoint_edits_against_the_original() {
        let out = apply_edits("a b c", &[edit("a", "A"), edit("c", "C")], "f").unwrap();
        assert_eq!(out, "A b C");
    }

    #[test]
    fn rejects_duplicates_missing_and_overlaps() {
        assert!(apply_edits("x x", &[edit("x", "y")], "f").unwrap_err().to_string().contains("2 occurrences"));
        assert!(apply_edits("abc", &[edit("zz", "y")], "f").unwrap_err().to_string().contains("Could not find"));
        let err = apply_edits("abcdef", &[edit("abc", "1"), edit("cde", "2")], "f").unwrap_err();
        assert!(err.to_string().contains("overlap"), "{err}");
    }

    #[test]
    fn fuzzy_match_ignores_trailing_space_and_smart_quotes() {
        let content = "say \u{201C}hi\u{201D}   \nnext";
        let out = apply_edits(content, &[edit("say \"hi\"\nnext", "done")], "f").unwrap();
        assert_eq!(out, "done");
    }
}
