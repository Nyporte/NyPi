//! The three planes as the board shows them (decision D86): chat's turns, the Principal's stack
//! and its activations step by step, the tasks under the goals they serve, and the vault. All
//! of it is read from what the planes keep on disk (`chat/turns.jsonl`, `principal/`, the
//! decision journal, the ledger, the goals file, the vault), so the board, a process of its
//! own, shows it whichever process runs them. The logs are scrubbed of secrets when written;
//! what is read from the vault here is scrubbed again before it is served.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use nypi_board_proto::{
    ActivationCard, ActivationDetail, ActivationRow, ActivationStep, BriefingItemView, BriefingView, ChatCard,
    ChatToolCall, ChatTurn, ChatView, GoalCard, GoalGroup, PinnedNote, Planes, PrincipalCard, PrincipalView,
    SignalPill, StimulusView, StoryStep, TaskDetail, TaskNode, VaultFolder, VaultNote, VaultNoteText, VaultView,
};
use nypi_core::config::{Paths, Settings};
use nypi_core::goals::{Goal, GoalsFile};
use nypi_core::ledger::{TaskId, TaskRecord};
use nypi_core::principal::{self, ActivationEvent, Decision, DecisionJournal};
use nypi_vault::secrets::SecretScrubber;

/// Turns `/api/chat` sends at most.
pub const CHAT_TURNS: usize = 200;
/// Notes listed per folder of the vault.
const NOTES_PER_FOLDER: usize = 200;

fn since_midnight(at: &str, midnight: DateTime<Utc>) -> bool {
    DateTime::parse_from_rfc3339(at).is_ok_and(|at| at.with_timezone(&Utc) >= midnight)
}

fn midnight(now: DateTime<Utc>) -> DateTime<Utc> {
    let local = now.with_timezone(&chrono::Local).date_naive();
    local
        .and_hms_opt(0, 0, 0)
        .and_then(|start| start.and_local_timezone(chrono::Local).earliest())
        .map_or(now, |start| start.with_timezone(&Utc))
}

/// Chat and the Principal for the home screen.
pub fn planes(paths: &Paths, settings: &Settings, now: DateTime<Utc>) -> Planes {
    Planes { chat: chat_card(paths, settings, now), principal: principal_card(paths, settings, now) }
}

fn chat_card(paths: &Paths, settings: &Settings, now: DateTime<Utc>) -> ChatCard {
    let turns = nypi_core::chat::read_turns(paths);
    let start = midnight(now);
    let mut card = ChatCard {
        on: settings.chat.model.is_some(),
        model: settings.chat.model.clone().unwrap_or_default(),
        ..Default::default()
    };
    for turn in turns.iter().filter(|turn| since_midnight(&turn.at, start)) {
        card.turns_today += 1;
        let reads = turn.tool_calls.iter().filter(|call| call.name != "signal_principal");
        for call in reads {
            card.reads_today += 1;
            card.timeouts_today += u32::from(call.timed_out);
        }
        card.signals_today += turn.signals_fired.len() as u32;
    }
    if let Some(last) = turns.iter().rev().find(|turn| !turn.skipped) {
        card.last_at = Some(last.at.clone());
        card.last_said = last
            .user_text
            .clone()
            .or_else(|| last.stimulus.as_ref().map(|stimulus| stimulus.headline.clone()))
            .unwrap_or_default();
        card.last_reply = last.reply_text.clone();
    }
    card
}

fn card_of(view: &principal::ActivationView) -> ActivationCard {
    ActivationCard {
        id: view.id.clone(),
        priority: view.priority.label().into(),
        kind: view.kind.clone(),
        topic: view.topic.clone(),
        steps: view.steps as u32,
        created: view.created.clone(),
    }
}

fn principal_card(paths: &Paths, settings: &Settings, now: DateTime<Utc>) -> PrincipalCard {
    let start = midnight(now);
    let status = principal::saved_status(paths).map(|saved| saved.status).unwrap_or_default();
    let today: Vec<Decision> =
        DecisionJournal::new(paths).load().into_iter().filter(|decision| since_midnight(&decision.at, start)).collect();
    PrincipalCard {
        on: settings.chat.model.is_some(),
        model: settings.principal.model.clone().or_else(|| settings.default_model.clone()).unwrap_or_default(),
        active: status.active.as_ref().map(card_of),
        suspended: status.suspended.iter().map(card_of).collect(),
        queued: status.queued.iter().map(card_of).collect(),
        decisions_today: today.len() as u32,
        cost_today: today.iter().map(|decision| decision.cost).sum(),
        live: None,
    }
}

/// `/api/chat`: the newest `limit` turns, oldest first.
pub fn chat_view(paths: &Paths, settings: &Settings, limit: usize) -> ChatView {
    let turns = nypi_core::chat::read_turns(paths);
    let from = turns.len().saturating_sub(limit);
    ChatView { model: settings.chat.model.clone(), turns: turns[from..].iter().map(chat_turn).collect(), live: None }
}

fn stimulus_view(stimulus: &principal::Stimulus) -> StimulusView {
    StimulusView {
        activation: stimulus.activation.clone(),
        kind: stimulus.kind.clone(),
        headline: stimulus.headline.clone(),
        must_convey: stimulus.must_convey.clone(),
        ask_user: stimulus.ask_user.clone(),
        vault_ref: stimulus.vault_ref.clone(),
    }
}

fn chat_turn(turn: &nypi_core::chat::TurnRecord) -> ChatTurn {
    ChatTurn {
        turn_id: turn.turn_id.clone(),
        at: turn.at.clone(),
        origin: turn.origin.clone(),
        focus: turn.focus.as_ref().map(|focus| focus.to_string()),
        user_text: turn.user_text.clone(),
        stimulus: turn.stimulus.as_ref().map(stimulus_view),
        recalled_memories: turn.recalled_memories.clone(),
        thinking: turn.thinking.clone(),
        tool_calls: turn
            .tool_calls
            .iter()
            .map(|call| ChatToolCall {
                name: call.name.clone(),
                arguments: serde_json::to_string_pretty(&call.arguments).unwrap_or_default(),
                duration_ms: call.duration_ms,
                output: call.output_excerpt.clone(),
                timed_out: call.timed_out,
                is_error: call.is_error,
            })
            .collect(),
        signals: turn
            .signals_fired
            .iter()
            .map(|signal| SignalPill {
                topic: signal.topic.clone(),
                urgency: signal.urgency.clone(),
                activation: signal.activation_id.clone(),
            })
            .collect(),
        reply_text: turn.reply_text.clone(),
        skipped: turn.skipped,
        tokens: turn.tokens,
        cost: turn.cost,
    }
}

/// Every activation, newest first: its course from the index, its decision from the journal.
/// One the index left running whose process is gone is `stopped`: it went with the process.
fn activation_rows(paths: &Paths) -> Vec<ActivationRow> {
    let live = principal::saved_status(paths).map(|saved| saved.status);
    let alive = |id: &str| {
        live.as_ref().is_some_and(|status| {
            status.active.iter().chain(&status.suspended).chain(&status.queued).any(|view| view.id == id)
        })
    };
    let mut rows: BTreeMap<String, ActivationRow> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for line in principal::activation_lines(paths) {
        let row = rows.entry(line.id.clone()).or_insert_with(|| {
            order.push(line.id.clone());
            ActivationRow { id: line.id.clone(), created: line.at.clone(), ..Default::default() }
        });
        if let Some(priority) = line.priority {
            row.priority = priority.label().into();
        }
        if !line.kind.is_empty() {
            row.kind = line.kind.clone();
        }
        if !line.topic.is_empty() {
            row.topic = line.topic.clone();
        }
        row.state = match line.state.as_str() {
            "created" => "queued",
            "started" | "resumed" => "active",
            other => other,
        }
        .to_string();
        if matches!(line.state.as_str(), "finished" | "joined") {
            row.finished = Some(line.at.clone());
            row.summary = line.summary.clone();
        }
    }
    for decision in DecisionJournal::new(paths).load() {
        if let Some(row) = rows.get_mut(&decision.activation_id) {
            row.summary = decision.summary;
            row.tokens = decision.tokens;
            row.cost = decision.cost;
            row.tasks = decision.tasks_touched;
            row.goals = decision.goals_touched;
            row.stimulated_chat = decision.stimulated_chat;
            row.disposition = decision.disposition;
        }
    }
    order
        .into_iter()
        .rev()
        .filter_map(|id| rows.remove(&id))
        .map(|mut row| {
            if matches!(row.state.as_str(), "queued" | "active" | "suspended") && !alive(&row.id) {
                row.state = "stopped".into();
            }
            row
        })
        .collect()
}

/// `/api/principal`.
pub fn principal_view(paths: &Paths, settings: &Settings, now: DateTime<Utc>) -> PrincipalView {
    PrincipalView { card: principal_card(paths, settings, now), activations: activation_rows(paths) }
}

/// The task a Principal's tool call started or acted on, for a link.
fn task_of(tool: &str, arguments: &serde_json::Value, output: &str) -> Option<String> {
    match tool {
        "start_task" => output
            .strip_prefix("Started task ")
            .and_then(|rest| rest.split_whitespace().next())
            .map(|id| id.trim_end_matches(['.', ',']).to_string()),
        "steer_task" | "cancel_task" | "task_status" | "continue_task" => {
            arguments["task_id"].as_str().map(str::to_string)
        }
        _ => None,
    }
}

/// `/api/activation/:id`.
pub fn activation_detail(paths: &Paths, id: &str) -> Option<ActivationDetail> {
    let events = principal::read_activation(paths, id);
    if events.is_empty() {
        return None;
    }
    let row = activation_rows(paths).into_iter().find(|row| row.id == id).unwrap_or_default();
    let mut detail = ActivationDetail { row, ..Default::default() };
    for event in events {
        let step = match event {
            ActivationEvent::Created { trigger, .. } => {
                detail.trigger = serde_json::to_string_pretty(&trigger).unwrap_or_default();
                continue;
            }
            ActivationEvent::Started { at } => {
                detail.started = Some(at);
                continue;
            }
            ActivationEvent::Nudged { at, text } => ActivationStep::Nudged { at, text },
            ActivationEvent::Briefed { pack, .. } => {
                detail.briefing = Some(BriefingView {
                    took_ms: pack.took_ms,
                    items: pack
                        .items
                        .into_iter()
                        .map(|item| BriefingItemView {
                            kind: item.kind,
                            title: item.title,
                            path: item.path,
                            text: item.text,
                            links: item.links,
                        })
                        .collect(),
                });
                continue;
            }
            ActivationEvent::AssistantTurn { at, thinking, text, tool_calls, stop_reason } => ActivationStep::Said {
                at,
                thinking,
                text,
                calls: tool_calls.iter().filter_map(|call| call["name"].as_str().map(str::to_string)).collect(),
                stopped: matches!(stop_reason.as_str(), "aborted" | "error"),
            },
            ActivationEvent::ToolResult { at, tool, arguments, output, duration_ms, is_error, .. } => {
                let task = task_of(&tool, &arguments, &output);
                ActivationStep::Tool {
                    at,
                    arguments: serde_json::to_string_pretty(&arguments).unwrap_or_default(),
                    name: tool,
                    output,
                    duration_ms,
                    is_error,
                    task,
                }
            }
            ActivationEvent::StimulatedChat { at, stimulus } => {
                ActivationStep::Stimulus { at, stimulus: stimulus_view(&stimulus) }
            }
            ActivationEvent::Suspended { at, by_activation, step_count, .. } => {
                ActivationStep::Suspended { at, by: by_activation, step_count: step_count as u32 }
            }
            ActivationEvent::Resumed { at, delta_summary } => ActivationStep::Resumed { at, delta: delta_summary },
            ActivationEvent::Joined { at, activation, topic } => ActivationStep::Joined { at, activation, topic },
            ActivationEvent::Finished { at, summary, tokens, cost } => {
                ActivationStep::Finished { at, summary, tokens, cost }
            }
        };
        detail.steps.push(step);
    }
    Some(detail)
}

/// A goal as the board shows it, with the tasks that served it.
fn goal_card(paths: &Paths, goal: &Goal, tasks: &BTreeMap<TaskId, TaskRecord>) -> GoalCard {
    let served: Vec<&TaskRecord> =
        tasks.values().filter(|task| task.goal_id.as_deref() == Some(goal.id.as_str())).collect();
    let project = goal.attribute("vault").map(|folder| folder.trim_end_matches('/').to_string());
    let plan =
        project.as_ref().map(|folder| format!("{folder}/plan.md")).filter(|plan| paths.vault().join(plan).is_file());
    GoalCard {
        id: goal.id.clone(),
        term: goal.term.label().to_lowercase(),
        text: goal.text.clone(),
        done: goal.done,
        due: goal.attribute("due").map(str::to_string),
        next: goal.attribute("next").map(str::to_string),
        project,
        plan,
        tasks: served.len() as u32,
        last_task: served.iter().max_by(|a, b| a.heartbeat_at.cmp(&b.heartbeat_at)).map(|task| task.id.0.clone()),
    }
}

/// The tasks of the home screen's tree under the goals they serve, in the tree's order; a
/// task working for another stays with its parent's goal. Those that serve none come last.
pub fn campaigns(paths: &Paths, graph: &[TaskNode], tasks: &BTreeMap<TaskId, TaskRecord>) -> Vec<GoalGroup> {
    let goals = GoalsFile::new(paths.goals()).goals();
    let mut groups: Vec<GoalGroup> = Vec::new();
    let mut loose = GoalGroup::default();
    let mut root_goal: Option<String> = None;
    for node in graph {
        if node.depth == 0 {
            root_goal = node.goal_id.clone();
        }
        let goal = if node.depth == 0 { node.goal_id.clone() } else { node.goal_id.clone().or(root_goal.clone()) };
        let Some(goal) = goal.and_then(|id| goals.iter().find(|known| known.id == id)) else {
            loose.tasks.push(node.clone());
            continue;
        };
        match groups.iter_mut().find(|group| group.goal.as_ref().is_some_and(|card| card.id == goal.id)) {
            Some(group) => group.tasks.push(node.clone()),
            None => groups.push(GoalGroup { goal: Some(goal_card(paths, goal, tasks)), tasks: vec![node.clone()] }),
        }
    }
    if !loose.tasks.is_empty() {
        groups.push(loose);
    }
    groups
}

/// `/api/vault`: every goal (open first), and the notes of each folder of the vault.
pub fn vault_view(paths: &Paths, tasks: &BTreeMap<TaskId, TaskRecord>) -> VaultView {
    let mut goals: Vec<GoalCard> =
        GoalsFile::new(paths.goals()).goals().iter().map(|goal| goal_card(paths, goal, tasks)).collect();
    goals.sort_by_key(|goal| goal.done);
    let vault = paths.vault();
    let folders = nypi_vault::vault::LAYOUT
        .iter()
        .filter(|folder| **folder != nypi_vault::vault::ATTACHMENTS)
        .map(|folder| {
            let mut notes = Vec::new();
            notes_in(&vault, &vault.join(folder), 0, &mut notes);
            notes.sort_by(|a: &VaultNote, b| a.path.cmp(&b.path));
            notes.truncate(NOTES_PER_FOLDER);
            VaultFolder { name: folder.to_string(), notes }
        })
        .collect();
    VaultView { goals, folders }
}

fn notes_in(vault: &Path, dir: &Path, depth: usize, found: &mut Vec<VaultNote>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        match entry.metadata() {
            Ok(meta) if meta.is_dir() && depth < 4 => notes_in(vault, &path, depth + 1, found),
            Ok(meta) if meta.is_file() && path.extension().is_some_and(|ext| ext == "md") => {
                found.push(VaultNote {
                    path: path.strip_prefix(vault).unwrap_or(&path).display().to_string(),
                    modified: meta.modified().map(|at| DateTime::<Utc>::from(at).to_rfc3339()).unwrap_or_default(),
                });
            }
            _ => {}
        }
    }
}

/// `/api/vault?path=…`: one note, as the vault's own checks allow it read.
pub fn vault_note(paths: &Paths, relative: &str, scrubber: &SecretScrubber) -> Option<VaultNoteText> {
    let vault = nypi_vault::vault::Vault::open(&paths.vault()).ok()?;
    let file = vault.resolve(relative).ok()?;
    let text = vault.read(relative).ok()?;
    let modified = std::fs::metadata(&file)
        .and_then(|meta| meta.modified())
        .map(|at| DateTime::<Utc>::from(at).to_rfc3339())
        .unwrap_or_default();
    Some(VaultNoteText { path: relative.trim().to_string(), modified, text: scrubber.scrub(&text) })
}

/// What a task's story adds since the planes (decisions D85, D86): the goal and activation it
/// is for, where it works, the plan and workspace it works under, and the Principal's
/// verdicts on its reports, each where it fell in the story.
pub fn task_extras(paths: &Paths, task: &TaskRecord, detail: &mut TaskDetail, scrubber: &SecretScrubber) {
    detail.goal_id = task.goal_id.clone();
    detail.activation = task.spawned_by_activation.clone();
    let folder = |path: &str| {
        Path::new(path).file_name().map_or_else(|| path.to_string(), |name| name.to_string_lossy().into_owned())
    };
    detail.place = match &task.remote {
        Some(remote) => format!("{} on {}", folder(&remote.folder), remote.device),
        None => task.cwd.display().to_string(),
    };
    detail.plan = task
        .goal_id
        .as_deref()
        .and_then(|goal| nypi_core::campaign::project_plan(paths, goal))
        .map(|plan| PinnedNote { path: plan.path, text: scrubber.scrub(&plan.text) });
    detail.workspace = task.workspace.clone();
    if let Some(workspace) = &task.workspace {
        let read = |name: &str| {
            std::fs::read_to_string(paths.vault().join(workspace).join(name)).ok().map(|text| scrubber.scrub(&text))
        };
        detail.brief = read("brief.md");
        detail.findings = read("findings.md");
        detail.report = read(nypi_core::evidence::REPORT_FILE);
    }
    // What the Principal reviews it by: the manifest the harness draws from its session (D87).
    if let Some(steps) = nypi_core::evidence::task_steps(paths, &task.id) {
        let answer = detail
            .steps
            .iter()
            .rev()
            .find_map(|step| match step {
                StoryStep::Ended { answer, .. } => Some(answer.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let cited_in = format!("{}\n{answer}", detail.report.as_deref().unwrap_or_default());
        let manifest = nypi_core::evidence::manifest(&steps, &cited_in);
        let done = task.state == nypi_core::ledger::TaskState::Done;
        detail.verified_refs = manifest.cited.iter().map(|step| step.reference.clone()).collect();
        detail.unknown_refs = manifest.unknown.clone();
        detail.unchecked_change = done && manifest.unchecked_change();
        detail.manifest = Some(scrubber.scrub(&manifest.render(done)));
    }
    // Reviews: the Principal's activations on this task's settlement, alone or with others.
    for decision in DecisionJournal::new(paths).load() {
        if !matches!(decision.trigger_kind.as_str(), "taskSettled" | "taskSettledBatch")
            || !decision.tasks_touched.contains(&task.id.0)
        {
            continue;
        }
        let sent_back = principal::read_activation(paths, &decision.activation_id).iter().any(|event| {
            matches!(event, ActivationEvent::ToolResult { tool, is_error: false, .. } if tool == "continue_task")
        });
        detail.rounds += u32::from(sent_back);
        let step = StoryStep::Review {
            at: decision.at.clone(),
            activation: decision.activation_id.clone(),
            accepted: !sent_back,
            summary: decision.summary.clone(),
        };
        let position = detail.steps.iter().position(|existing| at_of(existing) > decision.at.as_str());
        detail.steps.insert(position.unwrap_or(detail.steps.len()), step);
    }
}

fn at_of(step: &StoryStep) -> &str {
    match step {
        StoryStep::Prompt { at, .. }
        | StoryStep::Said { at, .. }
        | StoryStep::Tool { at, .. }
        | StoryStep::Note { at, .. }
        | StoryStep::Child { at, .. }
        | StoryStep::Ended { at, .. }
        | StoryStep::Steer { at, .. }
        | StoryStep::Review { at, .. } => at,
    }
}

/// The observer's steer, from its `thought` pulse (decision D78).
pub fn steer_step(at: &str, thought: &serde_json::Value) -> Option<StoryStep> {
    let text = thought["say"].as_str().filter(|say| !say.trim().is_empty())?;
    Some(StoryStep::Steer { at: at.to_string(), text: text.to_string() })
}
