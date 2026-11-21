//! `self_prompt`: the agent's versioned, audited access to its own read-write prompt layer
//! (requirement R4). The core layer stays read-only; see docs/architecture.md §4.3.

use std::path::PathBuf;

use anyhow::bail;
use async_trait::async_trait;
use nypi_agent::{ExecutionMode, Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use serde_json::{Value, json};

use super::edit::apply_edits;
use super::file_lock;

/// Keeps the self layer small enough to stay a guide rather than a second system prompt.
const MAX_SELF_PROMPT_BYTES: usize = 16 * 1024;

pub struct SelfPromptTool {
    /// `~/.nypi/agents/<name>/self.md`
    pub path: PathBuf,
}

#[async_trait]
impl Tool for SelfPromptTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "self_prompt".into(),
            description: "View or change your own self-prompt: the read-write layer of your system prompt where you keep durable lessons, working preferences, and knowledge about the user that should shape future sessions. Changes apply from the next user message. Every change is versioned and audited. The core layer (identity and conduct) cannot be changed.".into(),
            parameters: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "enum": ["view", "append", "replace", "rewrite", "history"], "description": "view: show current text; append: add text at the end; replace: exact oldText -> newText; rewrite: replace everything; history: list saved versions" },
                    "text": { "type": "string", "description": "Text to append (append) or the full new content (rewrite)" },
                    "oldText": { "type": "string", "description": "Exact text to replace (replace)" },
                    "newText": { "type": "string", "description": "Replacement text (replace)" },
                    "reason": { "type": "string", "description": "Why this change makes you more effective; recorded with the version" }
                }
            }),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let lock = file_lock(&self.path);
        let _guard = lock.lock().await;
        let current = tokio::fs::read_to_string(&self.path).await.unwrap_or_default();
        let field = |k: &str| args[k].as_str().unwrap_or_default().to_string();
        let action = field("action");
        let updated = match action.as_str() {
            "view" => {
                let text = if current.trim().is_empty() { "(empty)".to_string() } else { current };
                return Ok(ToolOutput::text(text));
            }
            "history" => {
                return Ok(
                    ToolOutput::text(self.history().join("\n")).with_details(json!({ "dir": self.history_dir() }))
                );
            }
            "append" => {
                let text = field("text");
                if text.trim().is_empty() {
                    bail!("append needs non-empty text");
                }
                let separator = if current.is_empty() || current.ends_with("\n\n") {
                    ""
                } else if current.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                format!("{current}{separator}{}\n", text.trim_end())
            }
            "replace" => apply_edits(&current, &[(field("oldText"), field("newText"))], "self prompt")?,
            "rewrite" => format!("{}\n", field("text").trim_end()),
            other => bail!("Unknown action: {other}"),
        };
        if updated.len() > MAX_SELF_PROMPT_BYTES {
            bail!(
                "Self prompt would be {} bytes (limit {MAX_SELF_PROMPT_BYTES}). Condense it: keep durable, high-value guidance only.",
                updated.len()
            );
        }
        let version = self.save_version(&current, &field("reason"))?;
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&self.path, &updated).await?;
        Ok(ToolOutput::text(format!(
            "Self prompt updated ({action}); {} bytes. Previous version saved as {}. Takes effect from the next user message.",
            updated.len(),
            version.display()
        ))
        .with_details(json!({ "path": self.path, "previousVersion": version, "reason": field("reason") })))
    }
}

impl SelfPromptTool {
    fn history_dir(&self) -> PathBuf {
        self.path.with_file_name("history")
    }

    /// Save the pre-change text with its reason, so every edit can be inspected and rolled back.
    fn save_version(&self, previous: &str, reason: &str) -> anyhow::Result<PathBuf> {
        let dir = self.history_dir();
        std::fs::create_dir_all(&dir)?;
        // The random suffix keeps two edits within the same millisecond from overwriting each other.
        let suffix = &uuid::Uuid::new_v4().simple().to_string()[..6];
        let path = dir.join(format!("{}-{suffix}.md", chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ")));
        let header = format!(
            "<!-- replaced at {} | reason: {} -->\n",
            chrono::Utc::now().to_rfc3339(),
            reason.replace("-->", "")
        );
        std::fs::write(&path, format!("{header}{previous}"))?;
        Ok(path)
    }

    fn history(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.history_dir())
            .map(|dir| dir.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        names.sort();
        if names.is_empty() {
            names.push("(no previous versions)".into());
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nypi_agent::ToolContext;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ToolContext::new("c".into(), "self_prompt".into(), CancellationToken::new(), tx)
    }

    #[tokio::test]
    async fn edits_are_versioned() {
        let dir = tempfile::tempdir().unwrap();
        let tool = SelfPromptTool { path: dir.path().join("lucia/self.md") };
        tool.execute(json!({ "action": "append", "text": "User prefers Rust.", "reason": "stated" }), ctx())
            .await
            .unwrap();
        tool.execute(json!({ "action": "replace", "oldText": "Rust", "newText": "Rust and Go" }), ctx()).await.unwrap();
        let view = tool.execute(json!({ "action": "view" }), ctx()).await.unwrap();
        assert_eq!(nypi_ai::text_of(&view.content).trim(), "User prefers Rust and Go.");
        assert_eq!(std::fs::read_dir(dir.path().join("lucia/history")).unwrap().count(), 2);
    }
}
