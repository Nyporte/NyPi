//! `write`: create or overwrite a file, creating parent directories.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use serde_json::{Value, json};

use super::{file_lock, resolve_path, writable};
use crate::protected::Protected;

pub struct WriteTool {
    pub cwd: PathBuf,
    pub protected: Arc<Protected>,
}

#[async_trait]
impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".into(),
            description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".into(),
            parameters: json!({
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                    "content": { "type": "string", "description": "Content to write to the file" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let path = resolve_path(&self.cwd, args["path"].as_str().unwrap_or_default());
        let content = args["content"].as_str().unwrap_or_default();
        let real = writable(&self.protected, &path)?;
        let lock = file_lock(&real);
        let _guard = lock.lock().await;
        if let Some(parent) = real.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| format!("Cannot create {}", parent.display()))?;
        }
        let existed = real.exists();
        tokio::fs::write(&real, content).await.with_context(|| format!("Cannot write {}", path.display()))?;
        Ok(ToolOutput::text(format!("Successfully wrote {} bytes to {}", content.len(), path.display()))
            .with_details(json!({ "path": path, "created": !existed })))
    }
}
