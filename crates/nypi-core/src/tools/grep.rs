//! `grep`: gitignore-aware content search with optional context lines.

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use regex::RegexBuilder;
use serde_json::{Value, json};

use super::resolve_path;
use super::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_line};

const DEFAULT_LIMIT: usize = 100;
const MAX_LINE_LENGTH: usize = 500;

pub struct GrepTool {
    pub cwd: PathBuf,
    /// Files holding keys are passed over, however wide the search (decision D62): the guard
    /// refuses a search aimed at one, but a search of a folder above it would read it too.
    pub protected: std::sync::Arc<crate::protected::Protected>,
}

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".into(),
            description: format!(
                "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} matches or {}KB (whichever is hit first). Long lines are truncated to {MAX_LINE_LENGTH} chars.",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: json!({
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
                    "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
                    "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
                    "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
                    "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
                    "context": { "type": "integer", "description": "Number of lines to show before and after each match (default: 0)" },
                    "limit": { "type": "integer", "description": "Maximum number of matches to return (default: 100)" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let (cwd, protected) = (self.cwd.clone(), self.protected.clone());
        tokio::task::spawn_blocking(move || search(&cwd, &protected, &args, &ctx)).await?
    }
}

fn search(
    cwd: &std::path::Path,
    protected: &crate::protected::Protected,
    args: &Value,
    ctx: &ToolContext,
) -> anyhow::Result<ToolOutput> {
    let pattern = args["pattern"].as_str().unwrap_or_default();
    let source = if args["literal"].as_bool().unwrap_or(false) { regex::escape(pattern) } else { pattern.to_string() };
    let regex = RegexBuilder::new(&source)
        .case_insensitive(args["ignoreCase"].as_bool().unwrap_or(false))
        .build()
        .with_context(|| format!("Invalid regex: {pattern}"))?;
    let root = resolve_path(cwd, args["path"].as_str().unwrap_or("."));
    if !root.exists() {
        bail!("Path not found: {}", root.display());
    }
    let glob = match args["glob"].as_str() {
        Some(glob) => Some(globset::Glob::new(glob)?.compile_matcher()),
        None => None,
    };
    let context = args["context"].as_u64().unwrap_or(0) as usize;
    let limit = args["limit"].as_u64().map(|l| l as usize).unwrap_or(DEFAULT_LIMIT).max(1);

    let mut out = String::new();
    let mut matches = 0;
    let mut notices = Vec::new();
    let base = if root.is_file() { root.parent().unwrap_or(&root).to_path_buf() } else { root.clone() };
    'files: for entry in ignore::WalkBuilder::new(&root).hidden(false).build().flatten() {
        if ctx.cancel.is_cancelled() {
            bail!("Operation aborted");
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let relative = entry.path().strip_prefix(&base).unwrap_or(entry.path());
        if let Some(glob) = &glob {
            let name_match = entry.path().file_name().is_some_and(|n| glob.is_match(n));
            if !glob.is_match(relative) && !name_match {
                continue;
            }
        }
        if protected.read_denied(entry.path()).is_some() {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path()) else { continue };
        if bytes.iter().take(8192).any(|b| *b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let mut last_printed: Option<usize> = None;
        for (i, line) in lines.iter().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            matches += 1;
            let from = i.saturating_sub(context);
            let to = (i + context).min(lines.len().saturating_sub(1));
            if context > 0 && last_printed.is_some_and(|p| from > p + 1) {
                out.push_str("--\n");
            }
            for (j, ctx_line) in lines.iter().enumerate().take(to + 1).skip(from) {
                if last_printed.is_some_and(|p| j <= p) {
                    continue;
                }
                let sep = if j == i { ':' } else { '-' };
                out.push_str(&format!(
                    "{}{sep}{}{sep} {}\n",
                    relative.display(),
                    j + 1,
                    truncate_line(ctx_line, MAX_LINE_LENGTH)
                ));
                last_printed = Some(j);
            }
            if out.len() > DEFAULT_MAX_BYTES {
                notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
                break 'files;
            }
            if matches >= limit {
                notices.push(format!("{limit} matches limit reached. Use limit or a narrower path/glob to see more."));
                break 'files;
            }
        }
    }
    if matches == 0 {
        return Ok(ToolOutput::text("No matches found"));
    }
    let mut text = out.trim_end().to_string();
    if !notices.is_empty() {
        text.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }
    Ok(ToolOutput::text(text).with_details(json!({ "matches": matches })))
}
