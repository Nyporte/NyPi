//! `schedule`: the agent sets timers and condition triggers (nypi-core/src/schedule.rs) that
//! notify the user or run a prompt later. A command condition passes the guard and the approval
//! policy as a bash call would when it is set (inspector.rs), since it runs unattended later.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use nypi_agent::{ExecutionMode, Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use serde_json::{Value, json};

use crate::schedule::{Action, CommandWhen, Schedule, ScheduleEvent, ScheduleStore, Trigger};

pub struct ScheduleTool {
    pub store: Arc<ScheduleStore>,
    pub agent: String,
    pub cwd: PathBuf,
    /// The conversation's folder on another device, when its place is not this computer.
    pub remote: Option<crate::place::DeviceFolder>,
    /// The runtime's place, when it is routed: a schedule is set where the run works (decision
    /// D79), which is where its prompt runs and its condition is checked when it fires.
    pub place: Option<Arc<crate::place::Place>>,
}

impl ScheduleTool {
    /// Where a schedule set now belongs: the run's focus, else the tool's own place.
    fn at(&self) -> (PathBuf, Option<crate::place::DeviceFolder>) {
        match self.place.as_ref().and_then(|place| place.focus()) {
            Some(crate::place::Focus { device: None, folder }) => (PathBuf::from(folder), None),
            Some(focus) => (self.cwd.clone(), focus.remote()),
            None => (self.cwd.clone(), self.remote.clone()),
        }
    }
}

#[async_trait]
impl Tool for ScheduleTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "schedule".into(),
            description: "Set something to happen later or when something changes, list what is set, or cancel it. \
                A trigger is a time (at, every, daily) or a condition (a file changed, a command succeeded or its \
                output changed). The action notifies the user, or runs a prompt as a task for you in this working \
                directory, so write a prompt that stands on its own. Use it when the user asks for a reminder or a \
                follow-up, or to check back on something slow (a build, a deploy) instead of waiting. Pick the \
                cheapest trigger and action that do the job. To wait for text in a file or a command's output, use a \
                command condition that tests for it (for example `grep -q FAILED build.log`, when succeeds): it \
                costs nothing until it fires. fileChanged fires on every write, so it is for changes themselves. A \
                notify costs nothing; a prompt runs a full model task on every firing, so use it only when the \
                firing needs your judgment or work."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "enum": ["add", "list", "cancel"] },
                    "trigger": {
                        "type": "object",
                        "description": "For add. kind at: {at: 'YYYY-MM-DD HH:MM' local or RFC 3339}; every: {minutes}; daily: {time: 'HH:MM', weekdays?: ['mon',...]}; fileChanged: {path}; command: {command, everyMinutes?, when: 'succeeds'|'changes'}",
                        "properties": {
                            "kind": { "type": "string", "enum": ["at", "every", "daily", "fileChanged", "command"] }
                        },
                        "required": ["kind"]
                    },
                    "then": { "type": "string", "enum": ["notify", "prompt"], "description": "For add" },
                    "text": { "type": "string", "description": "For add: the notice, or the prompt to run" },
                    "repeat": { "type": "boolean", "description": "For a condition: fire every time, not just once" },
                    "id": { "type": "string", "description": "For cancel" }
                }
            }),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        Ok(match args["action"].as_str().unwrap_or_default() {
            "list" => {
                let active = self.store.active();
                if active.is_empty() {
                    ToolOutput::text("Nothing is scheduled.")
                } else {
                    ToolOutput::text(active.iter().map(line).collect::<Vec<_>>().join("\n"))
                }
            }
            "add" => {
                let trigger: Trigger = match serde_json::from_value(args["trigger"].clone()) {
                    Ok(trigger) => trigger,
                    Err(error) => return Ok(ToolOutput::error(format!("trigger: {error}"))),
                };
                let text = args["text"].as_str().unwrap_or_default().trim().to_string();
                let action = match args["then"].as_str() {
                    Some("notify") => Action::Notify { text },
                    Some("prompt") => Action::Prompt { text },
                    _ => return Ok(ToolOutput::error("add needs `then`: notify or prompt")),
                };
                let repeat = args["repeat"].as_bool().unwrap_or(false);
                let note = cost_note(&trigger, &action, repeat).map(|n| format!("\n{n}")).unwrap_or_default();
                let (cwd, remote) = self.at();
                match self.store.add_at(trigger.clone(), action, &self.agent, &cwd, remote.as_ref(), repeat) {
                    Ok(id) => ToolOutput::text(format!("Scheduled {id}: {}.{note}", trigger.describe())),
                    Err(error) => ToolOutput::error(format!("Not scheduled: {error:#}")),
                }
            }
            "cancel" => {
                let id = args["id"].as_str().unwrap_or_default();
                match self.store.get(id) {
                    Some(s) if !s.finished => {
                        self.store.append(&ScheduleEvent::Cancelled {
                            id: id.to_string(),
                            at: crate::memory::now(),
                            by: self.agent.clone(),
                        })?;
                        ToolOutput::text(format!("Cancelled {id}."))
                    }
                    Some(_) => ToolOutput::error(format!("{id} already finished")),
                    None => ToolOutput::error(format!("No schedule {id}")),
                }
            }
            other => ToolOutput::error(format!("Unknown action: {other}")),
        })
    }
}

/// A prompt on a trigger that can fire often costs a model run each time, which only shows up
/// later on the bill; the agent hears it while it can still choose otherwise.
fn cost_note(trigger: &Trigger, action: &Action, repeat: bool) -> Option<&'static str> {
    let often = match trigger {
        Trigger::Every { minutes } => *minutes < 60,
        Trigger::FileChanged { .. } | Trigger::Command { when: CommandWhen::Changes, .. } => repeat,
        Trigger::At { .. } | Trigger::Daily { .. } | Trigger::Command { when: CommandWhen::Succeeds, .. } => false,
    };
    (often && matches!(action, Action::Prompt { .. })).then_some(
        "Note: this can fire often, and each firing runs a full model task. If the user only needs to hear about \
         it, cancel this and use notify; to wait for text to appear, a command condition that tests for it (when: \
         succeeds) fires once and costs nothing until then.",
    )
}

pub fn line(s: &Schedule) -> String {
    let kind = match s.action {
        Action::Notify { .. } => "notify",
        Action::Prompt { .. } => "prompt",
    };
    let repeat = if s.repeat { ", repeating" } else { "" };
    format!("{} {}{repeat} → {kind}: {} (set by {})", s.id, s.trigger.describe(), s.action.text(), s.by)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_about_prompts_that_can_fire_often() {
        let prompt = Action::Prompt { text: "look".into() };
        let notify = Action::Notify { text: "look".into() };
        let file = Trigger::FileChanged { path: "build.log".into() };
        let grep = |when| Trigger::Command { command: "grep -q FAILED build.log".into(), every_minutes: 1, when };
        assert!(cost_note(&file, &prompt, true).is_some(), "a prompt on every write");
        assert!(cost_note(&file, &prompt, false).is_none(), "fires once");
        assert!(cost_note(&file, &notify, true).is_none(), "notify is free");
        assert!(cost_note(&grep(CommandWhen::Succeeds), &prompt, true).is_none());
        assert!(cost_note(&grep(CommandWhen::Changes), &prompt, true).is_some());
        assert!(cost_note(&Trigger::Every { minutes: 5 }, &prompt, false).is_some());
        assert!(cost_note(&Trigger::Every { minutes: 120 }, &prompt, false).is_none());
    }
}
