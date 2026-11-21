//! `find`: gitignore-aware file search by glob.

use std::path::PathBuf;

use anyhow::bail;
use async_trait::async_trait;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use serde_json::{Value, json};

use super::resolve_path;
use super::truncate::{DEFAULT_MAX_BYTES, format_size};

const DEFAULT_LIMIT: usize = 1000;

pub struct FindTool {
    pub cwd: PathBuf,
}

#[async_trait]
impl Tool for FindTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "find".into(),
            description: format!(
                "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} results or {}KB (whichever is hit first).",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: json!({
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'" },
                    "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
                    "limit": { "type": "integer", "description": "Maximum number of results (default: 1000)" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let cwd = self.cwd.clone();
        tokio::task::spawn_blocking(move || find(&cwd, &args, &ctx)).await?
    }
}

fn find(cwd: &std::path::Path, args: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
    let pattern = args["pattern"].as_str().unwrap_or_default();
    let root = resolve_path(cwd, args["path"].as_str().unwrap_or("."));
    if !root.is_dir() {
        bail!("Directory not found: {}", root.display());
    }
    let matcher = globset::GlobBuilder::new(pattern).literal_separator(false).build()?.compile_matcher();
    // Like fd: a pattern without '/' matches file names, otherwise the relative path.
    let match_path = pattern.contains('/');
    let limit = args["limit"].as_u64().map(|l| l as usize).unwrap_or(DEFAULT_LIMIT).max(1);
    let mut results = Vec::new();
    let mut bytes = 0;
    let mut notice = None;
    for entry in ignore::WalkBuilder::new(&root).hidden(false).build().flatten() {
        if ctx.cancel.is_cancelled() {
            bail!("Operation aborted");
        }
        let Ok(relative) = entry.path().strip_prefix(&root) else { continue };
        if relative.as_os_str().is_empty() || relative.starts_with(".git") {
            continue;
        }
        let matched = if match_path {
            matcher.is_match(relative)
        } else {
            entry.path().file_name().is_some_and(|name| matcher.is_match(name))
        };
        if !matched {
            continue;
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        let line = format!("{}{}", relative.display(), if is_dir { "/" } else { "" });
        bytes += line.len() + 1;
        results.push(line);
        if results.len() >= limit {
            notice = Some(format!("{limit} results limit reached. Use limit or a narrower pattern to see more."));
            break;
        }
        if bytes > DEFAULT_MAX_BYTES {
            notice = Some(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
            break;
        }
    }
    if results.is_empty() {
        return Ok(ToolOutput::text("No files found matching pattern"));
    }
    results.sort();
    let mut text = results.join("\n");
    if let Some(notice) = notice {
        text.push_str(&format!("\n\n[{notice}]"));
    }
    Ok(ToolOutput::text(text).with_details(json!({ "count": results.len() })))
}
