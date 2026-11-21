//! `ls`: list one directory, sorted, with '/' on directories and dotfiles included.

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use serde_json::{Value, json};

use super::resolve_path;
use super::truncate::DEFAULT_MAX_BYTES;

const DEFAULT_LIMIT: usize = 500;

pub struct LsTool {
    pub cwd: PathBuf,
}

#[async_trait]
impl Tool for LsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ls".into(),
            description: format!(
                "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to {DEFAULT_LIMIT} entries or {}KB (whichever is hit first).",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list (default: current directory)" },
                    "limit": { "type": "integer", "description": "Maximum number of entries to return (default: 500)" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let dir = resolve_path(&self.cwd, args["path"].as_str().unwrap_or("."));
        if !dir.is_dir() {
            bail!("Not a directory: {}", dir.display());
        }
        let limit = args["limit"].as_u64().map(|l| l as usize).unwrap_or(DEFAULT_LIMIT).max(1);
        let mut entries = Vec::new();
        let mut reader = tokio::fs::read_dir(&dir).await.with_context(|| format!("Cannot list {}", dir.display()))?;
        while let Some(entry) = reader.next_entry().await? {
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            entries.push(format!("{}{}", entry.file_name().to_string_lossy(), if is_dir { "/" } else { "" }));
        }
        if entries.is_empty() {
            return Ok(ToolOutput::text("(empty directory)"));
        }
        entries.sort_by_key(|e| e.to_lowercase());
        let total = entries.len();
        let mut shown = Vec::new();
        let mut bytes = 0;
        for entry in entries.into_iter().take(limit) {
            bytes += entry.len() + 1;
            if bytes > DEFAULT_MAX_BYTES {
                break;
            }
            shown.push(entry);
        }
        let mut text = shown.join("\n");
        if shown.len() < total {
            text.push_str(&format!("\n\n[Showing {} of {total} entries]", shown.len()));
        }
        Ok(ToolOutput::text(text))
    }
}
