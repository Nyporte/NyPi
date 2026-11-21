//! `read`: text files with offset/limit and head truncation; images as attachments.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use async_trait::async_trait;
use base64::Engine;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::{ContentBlock, ToolDefinition};
use serde_json::{Value, json};

use super::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size, truncate_head};
use super::{readable, resolve_read_path};
use crate::protected::Protected;

pub struct ReadTool {
    pub cwd: PathBuf,
    pub protected: Arc<Protected>,
}

#[async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".into(),
            description: format!(
                "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
                DEFAULT_MAX_BYTES / 1024
            ),
            parameters: json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                    "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
                    "limit": { "type": "integer", "description": "Maximum number of lines to read" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let raw_path = args["path"].as_str().unwrap_or_default();
        let path = resolve_read_path(&self.cwd, raw_path);
        let real = readable(&self.protected, &path)?;
        let bytes = tokio::fs::read(&real).await.with_context(|| format!("Cannot read {}", path.display()))?;

        if let Some(mime_type) = image_mime(&path) {
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Ok(ToolOutput {
                content: vec![
                    ContentBlock::text(format!("Read image file [{mime_type}] ({})", format_size(bytes.len()))),
                    ContentBlock::Image { data, mime_type: mime_type.into() },
                ],
                ..Default::default()
            });
        }
        if bytes.iter().take(8192).any(|b| *b == 0) {
            bail!("{} appears to be a binary file", path.display());
        }

        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.split('\n').collect();
        let total = lines.len();
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        if offset > total {
            bail!("Offset {offset} is beyond end of file ({total} lines total)");
        }
        let start = offset - 1;
        let end = match args["limit"].as_u64() {
            // The model may send any number; a limit past the end is the end.
            Some(limit) => start.saturating_add(usize::try_from(limit).unwrap_or(usize::MAX)).min(total),
            None => total,
        };
        let selected = lines[start..end].join("\n");
        let truncation = truncate_head(&selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);

        let mut output = truncation.text;
        if truncation.kept_lines == 0 && !selected.is_empty() {
            output = format!(
                "[Line {offset} exceeds the {} limit. Use bash: sed -n '{offset}p' {raw_path} | head -c {DEFAULT_MAX_BYTES}]",
                format_size(DEFAULT_MAX_BYTES)
            );
        } else if truncation.truncated {
            let last = start + truncation.kept_lines;
            output.push_str(&format!(
                "\n\n[Showing lines {offset}-{last} of {total}. Use offset={} to continue.]",
                last + 1
            ));
        } else if end < total {
            output.push_str(&format!("\n\n[{} more lines in file. Use offset={} to continue.]", total - end, end + 1));
        }
        Ok(ToolOutput::text(output).with_details(json!({ "path": path, "totalLines": total })))
    }
}

fn image_mime(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    })
}
