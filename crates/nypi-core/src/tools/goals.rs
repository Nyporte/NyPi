//! `goals`: the agent's versioned access to the user's goals file (nypi-core/src/goals.rs).
//! The guard blocks writing the file with other tools, so every change keeps its reason.

use std::path::PathBuf;

use async_trait::async_trait;
use nypi_agent::{ExecutionMode, Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use serde_json::{Value, json};

use super::file_lock;
use crate::goals::{Change, GoalsFile, Term};

pub struct GoalsTool {
    /// `~/.nypi/goals.md`
    pub path: PathBuf,
    /// Lucia's vault, where a goal's project folder is (`vault`).
    pub vault: Option<PathBuf>,
}

#[async_trait]
impl Tool for GoalsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "goals".into(),
            description: "Read or change the user's goals (short, medium, long term), which you keep advancing. Record \
                progress as you make it: update a goal's next step after you move it, mark it done when it is done. Add \
                a goal only when the user states one; never invent the user's goals. Every change is versioned with \
                your reason."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "enum": ["list", "add", "update", "done", "reopen"] },
                    "id": { "type": "string", "description": "Goal id, for update, done, reopen" },
                    "term": { "type": "string", "enum": ["short", "medium", "long"], "description": "For add" },
                    "text": { "type": "string", "description": "The goal (add), or its new wording (update)" },
                    "next": { "type": "string", "description": "The next concrete step; empty clears it" },
                    "due": { "type": "string", "description": "YYYY-MM-DD; empty clears it" },
                    "dir": { "type": "string", "description": "Project folder the goal is about" },
                    "vault": { "type": "string", "description": "The goal's campaign in the vault, 10-projects/<name>, whose plan.md tasks for it see; empty clears it" },
                    "note": { "type": "string", "description": "Short status note, for example what blocks it" },
                    "reason": { "type": "string", "description": "Why this change, recorded with the version" }
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
        let file = GoalsFile::new(self.path.clone());
        let field = |k: &str| args[k].as_str().map(str::to_string);
        let vault = match (field("vault"), &self.vault) {
            (None, _) => None,
            (Some(value), Some(root)) => match crate::goals::check_project(root, &value) {
                Ok(value) => Some(value),
                Err(error) => return Ok(ToolOutput::error(error)),
            },
            (Some(_), None) => return Ok(ToolOutput::error("There is no vault here to hold a project folder.")),
        };
        let change = Change {
            text: field("text"),
            next: field("next"),
            due: field("due"),
            dir: field("dir"),
            vault,
            note: field("note"),
            done: None,
        };
        let reason = field("reason").unwrap_or_default();
        let id = field("id").unwrap_or_default();
        let result = match field("action").as_deref().unwrap_or_default() {
            "list" => {
                let text = file.read();
                return Ok(ToolOutput::text(if text.trim().is_empty() { "(no goals yet)".into() } else { text }));
            }
            "add" => {
                let term = field("term").and_then(|t| Term::parse(&t)).unwrap_or(Term::Short);
                let text = change.text.clone().unwrap_or_default();
                file.add(term, &text, &Change { text: None, ..change }, &reason).map(|id| format!("Added goal {id}."))
            }
            "update" => file.update(&id, &change, &reason).map(|g| format!("Updated goal {}: {}", g.id, g.text)),
            "done" => file
                .update(&id, &Change { done: Some(true), ..change }, &reason)
                .map(|g| format!("Marked goal {} done: {}", g.id, g.text)),
            "reopen" => file
                .update(&id, &Change { done: Some(false), ..change }, &reason)
                .map(|g| format!("Reopened goal {}: {}", g.id, g.text)),
            other => return Ok(ToolOutput::error(format!("Unknown action: {other}"))),
        };
        Ok(match result {
            Ok(text) => ToolOutput::text(text).with_details(json!({ "path": self.path, "reason": reason })),
            Err(error) => ToolOutput::error(format!("{error:#}")),
        })
    }
}
