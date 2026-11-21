//! The Principal's tools beside the stores': `stimulate_chat`, the task pool's controls,
//! `conclude_activation`, `inspect_task_evidence`, the store wrapper that records what each
//! change was for, and chat's `tasks_inspect`.

use std::sync::Arc;

use async_trait::async_trait;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use serde_json::{Value, json};

use crate::config::Paths;
use crate::place::Focus;
use crate::pulse::{Delivery, PulseSource};
use crate::schedule::ScheduleStore;
use crate::task_pool::{TaskBrief, TaskPool};

use super::*;

/// `stimulate_chat`: the Principal's one way to the user, through the chat module's voice.
pub(super) struct StimulateChat {
    pub(super) stimulate: Arc<dyn Fn(Stimulus) + Send + Sync>,
    pub(super) current: CurrentSlot,
}

#[async_trait]
impl Tool for StimulateChat {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "stimulate_chat".into(),
            description: "Have the chat module tell the user something. You never speak to the user yourself: chat \
                          does, in Lucia's one voice, keeping every fact you put in must_convey (results, numbers, \
                          options, [tN] citations, links). Use it for an answer, a result, a question only the user \
                          can answer, or something they must know now."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["kind", "headline", "must_convey"],
                "properties": {
                    "kind": { "type": "string", "enum": ["result", "question", "update", "alert"] },
                    "headline": { "type": "string", "description": "What it is about, in a line" },
                    "must_convey": { "type": "array", "items": { "type": "string" }, "description": "Every fact the user must hear" },
                    "ask_user": { "type": "string", "description": "A question only the user can answer, when there is one" },
                    "vault_ref": { "type": "string", "description": "A vault note with the details" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let text = |key: &str| args[key].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
        let must_convey: Vec<String> = args["must_convey"]
            .as_array()
            .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let Some(headline) = text("headline") else { return Ok(ToolOutput::error("`headline` is required")) };
        let current = self.current.lock().unwrap().clone().unwrap_or_default();
        let stimulus = Stimulus {
            activation: current.id.clone(),
            kind: text("kind").unwrap_or_else(|| "update".into()),
            headline,
            must_convey,
            ask_user: text("ask_user"),
            vault_ref: text("vault_ref"),
        };
        current.work.lock().unwrap().stimulated = true;
        (self.stimulate)(stimulus.clone());
        Ok(ToolOutput::text("Chat tells the user, in its own words, as soon as it is free.")
            .with_details(serde_json::to_value(&stimulus).unwrap_or(Value::Null)))
    }
}

pub(super) fn task_tools(
    pool: &TaskPool,
    started: &Started,
    max_rounds: u32,
    current: &CurrentSlot,
    paths: &Paths,
) -> Vec<Arc<dyn Tool>> {
    [TaskAction::Start, TaskAction::Steer, TaskAction::Cancel, TaskAction::Status, TaskAction::Continue]
        .into_iter()
        .map(|action| {
            Arc::new(TaskTool {
                action,
                pool: pool.clone(),
                started: started.clone(),
                max_rounds,
                current: current.clone(),
                goals: paths.goals(),
            }) as Arc<dyn Tool>
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(super) enum TaskAction {
    Start,
    Steer,
    Cancel,
    Status,
    Continue,
}

/// The task pool's controls, the Principal's (they were chat's before D84).
pub(super) struct TaskTool {
    pub(super) action: TaskAction,
    pub(super) pool: TaskPool,
    pub(super) started: Started,
    pub(super) max_rounds: u32,
    pub(super) current: CurrentSlot,
    /// The goals file, to check the goal a task is started for.
    pub(super) goals: std::path::PathBuf,
}

#[async_trait]
impl Tool for TaskTool {
    fn definition(&self) -> ToolDefinition {
        let task_id =
            json!({ "type": "string", "description": "The task (t-...); may be left out when only one is working" });
        let (name, description, parameters) = match self.action {
            TaskAction::Start => (
                "start_task",
                "Hand a task to a worker, which has a strong model and every tool (files, the shell, the network, \
                 devices). Give it in one sentence as `topic`: the user's conversation handed to you is attached \
                 verbatim, with where they are. Add `instruction` for what the conversation does not say, \
                 `done_when` when you know what will show it is done (otherwise the worker sets it in the task's \
                 workspace and you review against that), and `bounds`, `folder`, `device` only when needed. Several \
                 tasks may work at once. Returns the task's id.",
                json!({
                    "type": "object",
                    "properties": {
                        "topic": { "type": "string", "description": "The task in one sentence" },
                        "title": { "type": "string", "description": "A few words naming the task" },
                        "instruction": { "type": "string", "description": "What the conversation does not say; optional" },
                        "done_when": { "type": "string", "description": "The proof needed before it counts as done; optional" },
                        "bounds": { "type": "string", "description": "Where to stop, beyond the usual boundaries" },
                        "folder": { "type": "string", "description": "Folder to work in, when not where the user is" },
                        "device": { "type": "string", "description": "Device that folder is on, when not this computer" },
                        "goal": { "type": "string", "description": "The goal (g-...) this task advances: its project plan is shown to the worker" },
                        "review_policy": {
                            "type": "string",
                            "enum": ["immediate", "batch_unless_failed"],
                            "description": "immediate (the default): review it as soon as it settles. batch_unless_failed: when it settles Done while tasks for the same goal (or started by this activation) still work, review it with them (a Blocked or Failed one is reviewed at once)"
                        }
                    }
                }),
            ),
            TaskAction::Steer => (
                "steer_task",
                "Add an instruction to a working task. when=now: after the worker's current step. when=after: once the \
                 task would otherwise finish.",
                json!({
                    "type": "object",
                    "properties": {
                        "task_id": task_id,
                        "message": { "type": "string" },
                        "when": { "type": "string", "enum": ["now", "after"] }
                    }
                }),
            ),
            TaskAction::Cancel => (
                "cancel_task",
                "Stop a working task.",
                json!({ "type": "object", "properties": { "task_id": task_id } }),
            ),
            TaskAction::Status => (
                "task_status",
                "What a task is doing (how long it has run, its recent tool calls), or every task without task_id.",
                json!({ "type": "object", "properties": { "task_id": task_id } }),
            ),
            TaskAction::Continue => (
                "continue_task",
                "Send a settled task back to its worker because its report falls short of what counts as done. The \
                 worker keeps what it learned. Say in critique what is missing and a path to try.",
                json!({
                    "type": "object",
                    "required": ["task_id", "critique"],
                    "properties": { "task_id": { "type": "string" }, "critique": { "type": "string" } }
                }),
            ),
        };
        ToolDefinition { name: name.into(), description: description.into(), parameters }
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let current = self.current.lock().unwrap().clone().unwrap_or_default();
        let source = PulseSource::module(PRINCIPAL_MODULE);
        let text = |key: &str| args[key].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
        let task_id = text("task_id");
        let touched = |task: &str| {
            let mut work = current.work.lock().unwrap();
            work.tasks_acted = true;
            if !work.tasks.iter().any(|known| known == task) {
                work.tasks.push(task.to_string());
            }
        };
        Ok(match self.action {
            TaskAction::Start => match (text("topic"), text("instruction")) {
                (None, None) => ToolOutput::error("give the task in one sentence as `topic`"),
                (topic, instruction) => {
                    let goal = text("goal");
                    if let Some(goal) = &goal
                        && crate::goals::GoalsFile::new(self.goals.clone()).find(goal).is_none()
                    {
                        return Ok(ToolOutput::error(format!(
                            "No goal {goal}: the goals and their ids are in goals (action list)."
                        )));
                    }
                    if let Some(goal) = &goal {
                        let mut work = current.work.lock().unwrap();
                        if !work.goals.contains(goal) {
                            work.goals.push(goal.clone());
                        }
                    }
                    let review_policy = match args["review_policy"].as_str() {
                        None | Some("immediate") => crate::task_pool::ReviewPolicy::Immediate,
                        Some("batch_unless_failed") => crate::task_pool::ReviewPolicy::BatchUnlessFailed,
                        Some(other) => {
                            return Ok(ToolOutput::error(format!(
                                "review_policy is immediate or batch_unless_failed, not {other}"
                            )));
                        }
                    };
                    let brief = TaskBrief {
                        review_policy,
                        goal,
                        spawned_by_activation: Some(current.id.clone()).filter(|id| !id.is_empty()),
                        title: text("title").unwrap_or_default(),
                        conversation: current.conversation.clone(),
                        topic: topic.unwrap_or_default(),
                        instruction: instruction.unwrap_or_default(),
                        done_when: text("done_when").unwrap_or_default(),
                        bounds: text("bounds"),
                        place: match (text("device"), text("folder")) {
                            (device, Some(folder)) => Some(Focus { device, folder }),
                            (Some(device), None) => {
                                return Ok(ToolOutput::error(format!(
                                    "`device` needs `folder`: the folder on {device} the task works in"
                                )));
                            }
                            (None, None) => current.focus.clone(),
                        },
                    };
                    match self.pool.start(brief, source, current.cause.clone()).await {
                        Ok(begun) => {
                            self.started.lock().unwrap().insert(begun.pulse.clone());
                            touched(&begun.task.0);
                            let workspace = begun
                                .workspace
                                .as_ref()
                                .map(|w| format!(" Its workspace is {w}/ in the vault."))
                                .unwrap_or_default();
                            let beside = if begun.beside { " It works beside the task already running." } else { "" };
                            ToolOutput::text(format!(
                                "Started task {} (pulse {}).{workspace}{beside} You review it when it settles.",
                                begun.task, begun.pulse
                            ))
                            .with_details(json!({ "task": begun.task, "workspace": begun.workspace }))
                        }
                        Err(error) => ToolOutput::error(error),
                    }
                }
            },
            TaskAction::Steer => match text("message").or_else(|| text("instruction")) {
                None => ToolOutput::error("`message` must not be empty"),
                Some(message) => {
                    let delivery = if args["when"] == "after" { Delivery::FollowUp } else { Delivery::Steer };
                    match self.pool.instruct(task_id.as_deref(), &message, delivery, source, current.cause.clone()) {
                        Ok(key) => {
                            touched(&key);
                            ToolOutput::text(if delivery == Delivery::FollowUp {
                                "Queued for when the task ends."
                            } else {
                                "Sent; the worker sees it after its current step."
                            })
                        }
                        Err(error) => ToolOutput::error(error),
                    }
                }
            },
            TaskAction::Cancel => match self.pool.cancel(task_id.as_deref()) {
                Ok(key) => {
                    touched(&key);
                    ToolOutput::text("Cancelling the task.")
                }
                Err(error) => ToolOutput::error(error),
            },
            TaskAction::Status => ToolOutput::text(self.pool.describe(task_id.as_deref())),
            TaskAction::Continue => match (task_id, text("critique")) {
                (Some(task), Some(critique)) => {
                    match self.pool.continue_task(&task, &critique, self.max_rounds, source, current.cause.clone()) {
                        Ok(round) => {
                            touched(&task);
                            ToolOutput::text(format!("Sent back (round {round}). You review it when it settles again."))
                        }
                        Err(error) => ToolOutput::error(error),
                    }
                }
                _ => ToolOutput::error("`task_id` and `critique` are required"),
            },
        })
    }
}

/// `tasks_inspect`: what the chat module may read of the work (decision D84): the pool, the
/// ledger's recent tasks, the Principal's stack and queue, and its recent decisions.
pub struct TasksInspect {
    pub pool: TaskPool,
    pub principal: PrincipalHandle,
}

#[async_trait]
impl Tool for TasksInspect {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tasks_inspect".into(),
            description: "Read what is being worked on: the tasks running and waiting for review, the recent tasks of \
                          the ledger, what the Principal is doing (active, suspended, queued), and its recent decisions."
                .into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn execute(&self, _args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let paths = self.principal.paths();
        let mut recent: Vec<crate::ledger::TaskRecord> = crate::ledger::Ledger::new(paths.ledger())
            .load()
            .into_values()
            .filter(|task| task.parent.is_none())
            .collect();
        recent.sort_by(|a, b| b.heartbeat_at.cmp(&a.heartbeat_at));
        let tasks: Vec<String> = recent
            .iter()
            .take(10)
            .map(|task| {
                format!(
                    "- {} {} {}: {}",
                    task.id,
                    task.state.label(),
                    task.heartbeat_at.get(..16).unwrap_or_default(),
                    one_line(&task.goal, 120)
                )
            })
            .collect();
        let decisions: Vec<String> = self
            .principal
            .journal()
            .recent(5)
            .iter()
            .map(|decision| {
                format!(
                    "- #{} [{}] {}: {}",
                    decision.seq,
                    decision.priority,
                    decision.topic,
                    one_line(&decision.summary, 200)
                )
            })
            .collect();
        Ok(ToolOutput::text(format!(
            "## Task pool\n{}\n\n## Recent tasks\n{}\n\n## Principal\n{}\n\n## Recent decisions\n{}",
            self.pool.describe(None),
            if tasks.is_empty() { "(none)".into() } else { tasks.join("\n") },
            self.principal.status().describe(),
            if decisions.is_empty() { "(none)".into() } else { decisions.join("\n") },
        )))
    }
}

/// A store tool of the Principal's, with what each change was for (decision D85): a schedule
/// it adds records its `goal` and the activation that set it, and a goal it changes is one the
/// activation touched.
pub(super) struct Attributing {
    pub(super) inner: Arc<dyn Tool>,
    pub(super) current: CurrentSlot,
    pub(super) schedules: ScheduleStore,
}

#[async_trait]
impl Tool for Attributing {
    fn definition(&self) -> ToolDefinition {
        let mut definition = self.inner.definition();
        if definition.name == "schedule"
            && let Some(properties) = definition.parameters.pointer_mut("/properties").and_then(Value::as_object_mut)
        {
            properties.insert(
                "goal".into(),
                json!({ "type": "string", "description": "For add: the goal (g-...) this schedule serves" }),
            );
        }
        definition
    }

    fn execution_mode(&self) -> nypi_agent::ExecutionMode {
        self.inner.execution_mode()
    }

    fn prepare_arguments(&self, arguments: Value) -> Value {
        self.inner.prepare_arguments(arguments)
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let name = self.inner.definition().name;
        let output = self.inner.execute(args.clone(), ctx).await?;
        if output.is_error {
            return Ok(output);
        }
        let current = self.current.lock().unwrap().clone().unwrap_or_default();
        if !crate::introspect::reads_only(&name, &args) {
            current.work.lock().unwrap().stores_changed = true;
        }
        let text = nypi_ai::text_of(&output.content);
        let goal = args["goal"].as_str().map(str::trim).filter(|goal| !goal.is_empty()).map(str::to_string);
        match (name.as_str(), args["action"].as_str()) {
            ("schedule", Some("add")) => {
                // "Scheduled s-…: …": the id the store gave it.
                let id = text.strip_prefix("Scheduled ").and_then(|rest| rest.split(':').next()).map(str::trim);
                let activation = Some(current.id.clone()).filter(|id| !id.is_empty());
                if let Some(id) = id
                    && let Err(error) = self.schedules.attribute(id, goal.clone(), activation)
                {
                    return Ok(ToolOutput::text(format!("{text}\n(Its goal could not be recorded: {error:#})")));
                }
                if let Some(goal) = goal {
                    touch_goal(&current, &goal);
                }
            }
            ("goals", Some("list")) | ("schedule", _) => {}
            ("goals", _) => {
                // "Added goal g-…." for a new one, else the id it was given.
                let added = text.strip_prefix("Added goal ").map(|rest| rest.trim_end_matches('.').trim().to_string());
                if let Some(goal) = added.or_else(|| args["id"].as_str().map(str::to_string)) {
                    touch_goal(&current, &goal);
                }
            }
            _ => {}
        }
        Ok(output)
    }
}

pub(super) fn touch_goal(current: &Current, goal: &str) {
    let mut work = current.work.lock().unwrap();
    if !work.goals.iter().any(|known| known == goal) {
        work.goals.push(goal.to_string());
    }
}

/// `conclude_activation`: the Principal says, with a reason, that what woke it needs nothing
/// more (decision D87), so no activation ends having silently done nothing.
pub(super) struct ConcludeActivation {
    pub(super) current: CurrentSlot,
}

pub(super) const DISPOSITIONS: [&str; 3] = ["already_handled", "waiting_on_running_task", "noted_only"];

#[async_trait]
impl Tool for ConcludeActivation {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "conclude_activation".into(),
            description: "Say that this activation needs nothing more, and why: already_handled (it was done \
                          before, or by another activation), waiting_on_running_task (a task under way will settle \
                          it; name the task), noted_only (nothing to do, and the user need not hear of it). Use it \
                          only when no task, store change, or stimulate_chat is called for."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["disposition", "reason"],
                "properties": {
                    "disposition": { "type": "string", "enum": DISPOSITIONS },
                    "reason": { "type": "string", "description": "Why, in a sentence" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let disposition = args["disposition"].as_str().unwrap_or_default().trim();
        if !DISPOSITIONS.contains(&disposition) {
            return Ok(ToolOutput::error(format!("disposition is one of {}", DISPOSITIONS.join(", "))));
        }
        let Some(reason) = args["reason"].as_str().map(str::trim).filter(|r| !r.is_empty()) else {
            return Ok(ToolOutput::error("`reason` must say why"));
        };
        let current = self.current.lock().unwrap().clone().unwrap_or_default();
        current.work.lock().unwrap().concluded = Some((disposition.to_string(), reason.to_string()));
        Ok(ToolOutput::text(format!("Concluded ({disposition}). End with your short summary."))
            .with_details(json!({ "disposition": disposition, "reason": reason })))
    }
}

/// `inspect_task_evidence`: any step of a task's session in full (decision D87), read in
/// process from the session file its ledger record names, scrubbed of secrets.
pub(super) struct InspectTaskEvidence {
    pub(super) paths: Paths,
    pub(super) scrubber: Arc<nypi_vault::secrets::LiveScrubber>,
}

#[async_trait]
impl Tool for InspectTaskEvidence {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "inspect_task_evidence".into(),
            description: "Read what a task's tool calls returned, in full: the steps named in refs (t3, t8), or \
                          every step in a line each without refs. For a claim in a report or the manifest that \
                          looks thin: the raw output, the diff, the exit status. Up to 8,000 characters a call."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["task_id"],
                "properties": {
                    "task_id": { "type": "string", "description": "The task (t-...)" },
                    "refs": { "type": "array", "items": { "type": "string" }, "description": "References such as t3" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let Some(task) = args["task_id"].as_str().map(str::trim).filter(|t| !t.is_empty()) else {
            return Ok(ToolOutput::error("`task_id` is required"));
        };
        let refs: Vec<String> = args["refs"]
            .as_array()
            .map(|refs| refs.iter().filter_map(|r| r.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let (paths, task_id) = (self.paths.clone(), crate::ledger::TaskId(task.to_string()));
        let steps = tokio::task::spawn_blocking(move || crate::evidence::task_steps(&paths, &task_id)).await?;
        Ok(match steps {
            None => ToolOutput::error(format!(
                "No session of task {task} can be read (it is not in the ledger, or ran without one)."
            )),
            Some(steps) => ToolOutput::text(self.scrubber.scrub(&crate::evidence::inspect(&steps.steps, &refs))),
        })
    }
}
