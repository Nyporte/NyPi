//! What the board reads from NyPi's stores (decision D56): the task ledger, the audit log, and
//! the schedules, turned into the page's types. Read-only; nothing here writes.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Utc};
use nypi_board_proto::{
    ActivityRow, DeviceCard, InboxItem, StoryStep, TaskDetail, TaskNode, TaskRef, Today, Tone, Upcoming, VaultNote,
};
use nypi_core::ledger::{TaskId, TaskRecord, TaskState};
use nypi_core::schedule::{Schedule, Trigger, next_daily, parse_time};
use serde_json::Value;

use super::hub::{Approval, argument_summary};

/// Tool output kept in a story step; the evidence view has the rest.
const OUTPUT_LIMIT: usize = 20_000;

/// Records at or after `since` (RFC 3339; the log's own format, so text order is time order).
pub fn since<'a>(records: &'a [Value], since: &str) -> impl Iterator<Item = &'a Value> {
    records.iter().filter(move |r| r["ts"].as_str().is_some_and(|ts| ts >= since))
}

fn pulse_kind(record: &Value) -> &str {
    if record["source"] != "pulse" {
        return "";
    }
    record["payload"]["payload"]["kind"].as_str().unwrap_or_default()
}

/// The day so far (the last 24 hours of records), against the seven days before.
pub fn today(day: &[&Value], week_cost: f64) -> Today {
    let mut today = Today { cost_week_average: week_cost / 7.0, ..Today::default() };
    for record in day {
        let payload = &record["payload"];
        if payload["type"] == "message_end" && payload["message"]["role"] == "assistant" {
            let usage = &payload["message"]["usage"];
            today.tokens_in += usage["input"].as_u64().unwrap_or(0) + usage["cacheRead"].as_u64().unwrap_or(0);
            today.tokens_out += usage["output"].as_u64().unwrap_or(0);
            today.cost += usage["cost"]["total"].as_f64().unwrap_or(0.0);
        }
        match pulse_kind(record) {
            "runSettled" => match payload["payload"]["status"]["kind"].as_str().unwrap_or_default() {
                "done" => today.done += 1,
                "blocked" => today.blocked += 1,
                "failed" => today.failed += 1,
                _ => today.continuing += 1,
            },
            "toolFinished" => {
                today.tool_calls += 1;
                if payload["payload"]["isError"] == true {
                    today.tool_failures += 1;
                }
            }
            _ => {}
        }
        if record["source"] == "harness" {
            match payload["retry"]["type"].as_str() {
                Some("auto_retry_start") if payload["retry"]["attempt"] == 1 => today.retries += 1,
                Some("auto_retry_end") if payload["retry"]["success"] == true => today.retries_recovered += 1,
                _ => {}
            }
        }
    }
    today
}

/// The model cost of these records.
pub fn cost(records: impl Iterator<Item = impl std::ops::Deref<Target = Value>>) -> f64 {
    records
        .filter(|r| r["payload"]["type"] == "message_end" && r["payload"]["message"]["role"] == "assistant")
        .filter_map(|r| r["payload"]["message"]["usage"]["cost"]["total"].as_f64())
        .sum()
}

/// What waits on the owner: approvals first, then blocked and stuck tasks, then alerts.
pub fn inbox(
    approvals: &[Approval],
    tasks: &BTreeMap<TaskId, TaskRecord>,
    day: &[&Value],
    acknowledged: &HashSet<String>,
    alive: impl Fn(u32) -> bool,
) -> Vec<InboxItem> {
    let mut items: Vec<InboxItem> = approvals
        .iter()
        .map(|a| InboxItem::Approval {
            id: a.id.clone(),
            process: a.process.clone(),
            agent: a.agent.clone(),
            tool: a.tool.clone(),
            summary: a.summary.clone(),
            reason: a.reason.clone(),
            task: a.task.clone(),
            since: a.since.clone(),
        })
        .collect();
    let mut tasks: Vec<&TaskRecord> = tasks.values().collect();
    tasks.sort_by(|a, b| b.heartbeat_at.cmp(&a.heartbeat_at));
    for task in tasks {
        let task_ref = TaskRef { id: task.id.0.clone(), goal: headline(&task.goal) };
        let item = match task.state {
            TaskState::BlockedOnUser => Some(InboxItem::Blocked {
                task: task_ref,
                question: task.note.clone(),
                since: task.heartbeat_at.clone(),
            }),
            TaskState::Stalled => Some(InboxItem::Stuck {
                task: task_ref,
                state: "stalled".into(),
                note: task.note.clone(),
                since: task.heartbeat_at.clone(),
            }),
            // A task waiting on a schedule has no process by design.
            TaskState::Waiting => None,
            state if !state.is_terminal() && task.pid != 0 && !alive(task.pid) => Some(InboxItem::Stuck {
                task: task_ref,
                state: "interrupted".into(),
                note: "Its process ended before the task did.".into(),
                since: task.heartbeat_at.clone(),
            }),
            _ => None,
        };
        if let Some(item) = item.filter(|i| !acknowledged.contains(&i.key())) {
            items.push(item);
        }
    }
    // The same alert raised again shows once, at its newest.
    let mut seen = HashSet::new();
    for record in day.iter().rev() {
        let Some((level, message, at)) = alert(record) else { continue };
        if level == "notice" || !seen.insert(message.to_string()) {
            continue;
        }
        let key = record_key(record);
        let item = InboxItem::Alert { key, level, message: message.into(), since: at.into() };
        if !acknowledged.contains(&item.key()) {
            items.push(item);
        }
    }
    items
}

/// An alert record's level, message, and time, whichever way it was recorded.
fn alert(record: &Value) -> Option<(String, &str, &str)> {
    let alert = match record["source"].as_str()? {
        "alert" => &record["payload"],
        "pulse" if pulse_kind(record) == "alert" => &record["payload"]["payload"]["alert"],
        _ => return None,
    };
    let level = alert["level"].as_str()?.to_lowercase();
    let at = alert["at"].as_str().or(record["ts"].as_str()).unwrap_or_default();
    Some((level, alert["message"].as_str()?, at))
}

fn record_key(record: &Value) -> String {
    format!("{}:{}", record["session"].as_str().unwrap_or_default(), record["seq"])
}

/// The next firings, soonest first. Conditions have no time; they are listed after.
pub fn upcoming(schedules: &[Schedule], now: DateTime<Utc>, limit: usize) -> Vec<Upcoming> {
    let mut timed = Vec::new();
    let mut waiting = Vec::new();
    for schedule in schedules {
        let since = schedule.last_fired.as_deref().unwrap_or(&schedule.created);
        let since = parse_time(since).unwrap_or(now);
        let next = match &schedule.trigger {
            Trigger::At { at } => parse_time(at),
            Trigger::Every { minutes } => Some(since + chrono::Duration::minutes(*minutes as i64)),
            Trigger::Daily { time, weekdays } => {
                next_daily(since.max(now - chrono::Duration::minutes(1)), time, weekdays)
            }
            Trigger::FileChanged { .. } | Trigger::Command { .. } => None,
        };
        let label = format!("{} — {}", schedule.trigger.describe(), schedule.action.text());
        match next {
            Some(at) => timed.push((at, label)),
            None => waiting.push(Upcoming { at: String::new(), label }),
        }
    }
    timed.sort();
    let mut list: Vec<Upcoming> =
        timed.into_iter().map(|(at, label)| Upcoming { at: at.max(now).to_rfc3339(), label }).collect();
    list.extend(waiting);
    list.truncate(limit);
    list
}

/// Tasks ended longer ago than this are not in the tree.
const GRAPH_RECENT_MINUTES: i64 = 60;
const GRAPH_SHOWN: usize = 40;

/// The tree of what is being worked on: every open task and what ended within the hour, each
/// task under the task it works for, newest first. `running` names the process on a task.
pub fn graph(
    tasks: &BTreeMap<TaskId, TaskRecord>,
    now: DateTime<Utc>,
    alive: impl Fn(u32) -> bool,
    running: impl Fn(&str) -> Option<String>,
) -> Vec<TaskNode> {
    let recent = |task: &TaskRecord| {
        parse_time(&task.heartbeat_at).is_some_and(|at| (now - at).num_minutes() < GRAPH_RECENT_MINUTES)
    };
    let shown = |task: &TaskRecord| match task.state {
        TaskState::Waiting | TaskState::BlockedOnUser => true,
        state if state.is_terminal() => recent(task),
        _ => alive(task.pid) || recent(task),
    };
    let folder = |path: &std::path::Path| {
        path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
    };
    let node = |task: &TaskRecord, depth: u32| TaskNode {
        id: task.id.0.clone(),
        goal: headline(&task.goal),
        state: task.state.label().to_string(),
        depth,
        place: match &task.remote {
            Some(remote) => format!("{} on {}", folder(std::path::Path::new(&remote.folder)), remote.device),
            None => folder(&task.cwd),
        },
        waits: task.waits.as_ref().map(|waits| match waits {
            nypi_core::ledger::WaitPredicate::AllOf { tokens } => format!("all of {}", tokens.len()),
            nypi_core::ledger::WaitPredicate::AnyOf { tokens } => format!("the first of {}", tokens.len()),
            nypi_core::ledger::WaitPredicate::Schedule { id } => format!("schedule {id}"),
            nypi_core::ledger::WaitPredicate::Approval { .. } => "an approval".to_string(),
        }),
        workspace: task.workspace.clone(),
        process: running(&task.id.0),
        goal_id: task.goal_id.clone(),
        activation: task.spawned_by_activation.clone(),
    };
    let mut children: BTreeMap<&TaskId, Vec<&TaskRecord>> = BTreeMap::new();
    for task in tasks.values().filter(|task| shown(task)) {
        if let Some(parent) = &task.parent {
            children.entry(parent).or_default().push(task);
        }
    }
    // A task whose parent is not shown (it ended long ago) stands on its own.
    let mut roots: Vec<&TaskRecord> = tasks
        .values()
        .filter(|task| {
            shown(task) && task.parent.as_ref().is_none_or(|parent| tasks.get(parent).is_none_or(|p| !shown(p)))
        })
        .collect();
    roots.sort_by(|a, b| b.heartbeat_at.cmp(&a.heartbeat_at));
    let mut tree = Vec::new();
    let mut open: Vec<(&TaskRecord, u32)> = roots.into_iter().rev().map(|task| (task, 0)).collect();
    while let Some((task, depth)) = open.pop() {
        if tree.len() == GRAPH_SHOWN {
            break;
        }
        tree.push(node(task, depth));
        if let Some(under) = children.get(&task.id) {
            let mut under = under.clone();
            under.sort_by(|a, b| a.created.cmp(&b.created));
            open.extend(under.into_iter().rev().map(|child| (child, depth + 1)));
        }
    }
    tree
}

/// The hub's devices, from its register and its record of who is paired as what.
pub fn devices(
    register: &BTreeMap<String, nypi_hub::nodes::Known>,
    peers: &BTreeMap<String, nypi_hub::pairing::Peer>,
    hub_running: bool,
) -> Vec<DeviceCard> {
    let mut cards: Vec<DeviceCard> = register
        .iter()
        .map(|(name, known)| DeviceCard {
            name: name.clone(),
            kind: format!("{:?}", known.report.identity.kind).to_lowercase(),
            system: known.report.identity.system.clone(),
            version: known.report.identity.version.clone(),
            // What the register last wrote is only true while the hub that wrote it runs.
            connected: known.connected && hub_running,
            since: known.since.clone(),
            tools: known.report.tools.iter().map(|tool| tool.name.clone()).collect(),
            paired: Vec::new(),
            running: known.report.running.len() as u32,
        })
        .collect();
    for peer in peers.values() {
        // What it was admitted as (decision D93): personal, exec-only, or a browser.
        let role = match peer.kind {
            nypi_hub::pairing::PeerKind::Browser => "browser".to_string(),
            nypi_hub::pairing::PeerKind::Machine => peer.trust.label().to_string(),
        };
        match cards.iter_mut().find(|card| card.name == peer.name) {
            Some(card) => card.paired.push(role),
            // Admitted, and never connected as a node: a browser, or a node not started yet.
            None => cards.push(DeviceCard {
                name: peer.name.clone(),
                since: peer.paired_at.clone(),
                paired: vec![role],
                ..Default::default()
            }),
        }
    }
    for card in &mut cards {
        card.paired.sort();
        card.paired.dedup();
    }
    cards
}

/// The notes of the vault's inbox and of its task workspaces that changed last.
pub fn vault_activity(vault: &std::path::Path, limit: usize) -> Vec<VaultNote> {
    fn walk(dir: &std::path::Path, depth: usize, found: &mut Vec<(std::time::SystemTime, std::path::PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            match entry.metadata() {
                Ok(meta) if meta.is_dir() && depth < 3 => walk(&path, depth + 1, found),
                Ok(meta) if meta.is_file() => {
                    if let Ok(modified) = meta.modified() {
                        found.push((modified, path));
                    }
                }
                _ => {}
            }
        }
    }
    let mut found = Vec::new();
    for folder in [nypi_vault::vault::INBOX, nypi_vault::vault::WORKSPACES] {
        walk(&vault.join(folder), 0, &mut found);
    }
    found.sort_by_key(|note| std::cmp::Reverse(note.0));
    found
        .into_iter()
        .take(limit)
        .map(|(modified, path)| VaultNote {
            path: path.strip_prefix(vault).unwrap_or(&path).display().to_string(),
            modified: DateTime::<Utc>::from(modified).to_rfc3339(),
        })
        .collect()
}

/// Filters for the activity list.
#[derive(Default)]
pub struct ActivityFilter {
    pub text: Option<String>,
    pub origin: Option<String>,
    pub state: Option<String>,
    pub kind: Option<String>,
}

impl ActivityFilter {
    pub fn from_query(query: &str) -> Self {
        let mut filter = ActivityFilter::default();
        for (key, value) in super::web::pairs(query) {
            let value = Some(value).filter(|v| !v.is_empty());
            match key.as_str() {
                "q" => filter.text = value.map(|v| v.to_lowercase()),
                "origin" => filter.origin = value,
                "state" => filter.state = value,
                "kind" => filter.kind = value,
                _ => {}
            }
        }
        filter
    }

    fn admits(&self, row: &ActivityRow) -> bool {
        let text = self
            .text
            .as_ref()
            .is_none_or(|t| row.title.to_lowercase().contains(t) || row.detail.to_lowercase().contains(t));
        let origin = self.origin.as_ref().is_none_or(|o| row.origin.contains(o.as_str()));
        let state = self.state.as_ref().is_none_or(|s| row.state.as_deref() == Some(s.as_str()));
        let kind = self.kind.as_ref().is_none_or(|k| row.kind == *k);
        text && origin && state && kind
    }
}

/// Tasks and notable events since `oldest`, newest first.
pub fn activity(
    tasks: &BTreeMap<TaskId, TaskRecord>,
    records: &[&Value],
    oldest: &str,
    filter: &ActivityFilter,
    alive: impl Fn(u32) -> bool,
    limit: usize,
) -> Vec<ActivityRow> {
    let mut rows = Vec::new();
    for task in tasks.values().filter(|t| t.heartbeat_at.as_str() >= oldest) {
        rows.push(ActivityRow {
            at: task.heartbeat_at.clone(),
            kind: "task".into(),
            title: task.goal.clone(),
            detail: task.note.clone(),
            task: Some(TaskRef { id: task.id.0.clone(), goal: headline(&task.goal) }),
            state: Some(task_state(task, &alive)),
            agent: task.owner.clone(),
            origin: task.origin.clone(),
        });
    }
    let by_pulse: HashMap<&str, &TaskRecord> =
        tasks.values().filter_map(|t| t.pulse.as_ref().map(|p| (p.0.as_str(), t))).collect();
    for record in records {
        let at = record["ts"].as_str().unwrap_or_default().to_string();
        let task = record["cause"]
            .as_str()
            .and_then(|c| by_pulse.get(c))
            .map(|t| TaskRef { id: t.id.0.clone(), goal: headline(&t.goal) });
        let row = |kind: &str, title: String, detail: String, origin: &str| ActivityRow {
            at: at.clone(),
            kind: kind.into(),
            title,
            detail,
            task: task.clone(),
            state: None,
            agent: String::new(),
            origin: origin.into(),
        };
        let payload = &record["payload"];
        if let Some((level, message, _)) = alert(record) {
            rows.push(row("alert", format!("{} alert", capitalize(&level)), message.to_string(), ""));
            continue;
        }
        match record["source"].as_str().unwrap_or_default() {
            "approval" if payload["request"].is_object() => {
                let request = &payload["request"];
                let title = format!("Asked to approve {}", request["tool"].as_str().unwrap_or_default());
                let detail = format!(
                    "{} — {}",
                    request["summary"].as_str().unwrap_or_default(),
                    request["reason"].as_str().unwrap_or_default()
                );
                rows.push(row("approval", title, detail, ""));
            }
            "approval" if payload["decision"].is_object() => {
                let decision = &payload["decision"];
                let title = if decision["approved"] == true { "Approved" } else { "Denied" };
                rows.push(row("approval", title.into(), decision["note"].as_str().unwrap_or_default().into(), ""));
            }
            "board" => {
                let title = if payload["stop"] == true {
                    "You stopped a run from the board".to_string()
                } else if payload["steer"].is_string() {
                    "You sent a message from the board".to_string()
                } else {
                    let approved = payload["approved"] == true;
                    format!("You {} from the board", if approved { "approved" } else { "denied" })
                };
                let detail = payload["steer"].as_str().or(payload["reason"].as_str()).unwrap_or_default();
                rows.push(row("board", title, detail.into(), "board"));
            }
            "reflection" if payload["reflection"].is_object() => {
                rows.push(row("reflection", "Reflected on a task".into(), compact(&payload["reflection"]), ""));
            }
            "pulse" if pulse_kind(record) == "triggered" => {
                let inner = &payload["payload"];
                let title = format!("Schedule {} fired", inner["schedule"].as_str().unwrap_or_default());
                rows.push(row("trigger", title, inner["text"].as_str().unwrap_or_default().into(), "schedule"));
            }
            _ => {}
        }
    }
    rows.retain(|r| filter.admits(r));
    rows.sort_by(|a, b| b.at.cmp(&a.at));
    rows.truncate(limit);
    rows
}

fn task_state(task: &TaskRecord, alive: impl Fn(u32) -> bool) -> String {
    let has_no_process_by_design = matches!(task.state, TaskState::BlockedOnUser | TaskState::Waiting);
    if !task.state.is_terminal() && !has_no_process_by_design && task.pid != 0 && !alive(task.pid) {
        return "interrupted".into();
    }
    task.state.label().into()
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn compact(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    text.chars().take(300).collect()
}

/// The pulses a task's records hang from: its own, and every pulse they led to. Records come in
/// log order, so a pulse is seen before what it causes.
struct Scope(HashSet<String>);

impl Scope {
    fn admit(&mut self, record: &Value) -> bool {
        if record["source"] == "pulse" {
            let id = record["payload"]["id"].as_str().unwrap_or_default();
            let inside =
                self.0.contains(id) || record["payload"]["causation"].as_str().is_some_and(|c| self.0.contains(c));
            if inside {
                self.0.insert(id.to_string());
            }
            return inside;
        }
        record["cause"].as_str().is_some_and(|c| self.0.contains(c))
    }
}

/// One task as a story, from the records since it was created.
pub fn task_detail(
    tasks: &BTreeMap<TaskId, TaskRecord>,
    id: &str,
    records: &[Value],
    alive: impl Fn(u32) -> bool,
) -> Option<TaskDetail> {
    let task = tasks.get(&TaskId(id.to_string()))?;
    let reference = |t: &TaskRecord| TaskRef { id: t.id.0.clone(), goal: headline(&t.goal) };
    let children: Vec<&TaskRecord> = tasks.values().filter(|t| t.parent.as_ref() == Some(&task.id)).collect();
    // A delegated child keeps its own story; its records stay out of this one.
    let child_sessions: HashSet<&str> =
        children.iter().map(|c| c.session_id.as_str()).filter(|s| *s != task.session_id).collect();
    let mut detail = TaskDetail {
        id: task.id.0.clone(),
        goal: headline(&task.goal),
        prompt: task.prompt.clone(),
        state: task_state(task, &alive),
        note: task.note.clone(),
        owner: task.owner.clone(),
        origin: task.origin.clone(),
        created: task.created.clone(),
        updated: task.heartbeat_at.clone(),
        attempts: task.attempts,
        parent: task.parent.as_ref().and_then(|p| tasks.get(p)).map(reference),
        children: children.iter().map(|c| reference(c)).collect(),
        ..TaskDetail::default()
    };
    let mut steps = Vec::new();
    if let Some(pulse) = &task.pulse {
        let mut scope = Scope(HashSet::from([pulse.0.clone()]));
        let mut story = Story { steps: Vec::new(), calls: HashMap::new(), early: HashMap::new() };
        // The prompt's pulse is recorded a moment before the ledger entry it leads to.
        let from = chrono::DateTime::parse_from_rfc3339(&task.created)
            .map(|t| (t - chrono::Duration::minutes(5)).to_rfc3339())
            .unwrap_or_default();
        for record in since(records, &from) {
            if !scope.admit(record) || child_sessions.contains(record["session"].as_str().unwrap_or_default()) {
                continue;
            }
            story.step(record, &mut detail);
        }
        steps = story.steps;
        steps.extend(story.early.into_values().flatten());
    }
    if steps.is_empty() {
        steps.push(StoryStep::Prompt {
            at: task.created.clone(),
            text: task.prompt.clone(),
            from: task.origin.clone(),
        });
    }
    for child in children {
        let at = child.created.clone();
        let position = steps.iter().position(|s| step_at(s) > at.as_str()).unwrap_or(steps.len());
        steps.insert(position, StoryStep::Child { at, task: reference(child), state: task_state(child, &alive) });
    }
    detail.steps = steps;
    Some(detail)
}

/// A task's name as the board shows it: its first paragraph, on one line. A task started for
/// the owner's conversation carries that conversation after its topic (decision D82); the
/// story shows it whole, a title does not.
pub fn headline(goal: &str) -> String {
    const LONGEST: usize = 200;
    let first = goal.trim().split("\n\n").next().unwrap_or_default();
    let line = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > LONGEST { format!("{}…", line.chars().take(LONGEST).collect::<String>()) } else { line }
}

fn step_at(step: &StoryStep) -> &str {
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

fn note(at: &str, tone: Tone, title: impl Into<String>, detail: impl Into<String>) -> StoryStep {
    StoryStep::Note { at: at.to_string(), tone, title: title.into(), detail: detail.into() }
}

fn texts(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        content => {
            content.as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")
        }
    }
}

/// A story being told, record by record.
struct Story {
    steps: Vec<StoryStep>,
    /// Where each tool call's step is.
    calls: HashMap<String, usize>,
    /// Notes about a tool call recorded before the call itself (an approval asked while the
    /// call's message was still on its way to the log).
    early: HashMap<String, Vec<StoryStep>>,
}

impl Story {
    /// A note about tool call `call`: after the call when it is known, else once it is.
    fn about_call(&mut self, call: &str, step: StoryStep) {
        if call.is_empty() || self.calls.contains_key(call) {
            self.steps.push(step);
        } else {
            self.early.entry(call.to_string()).or_default().push(step);
        }
    }

    fn step(&mut self, record: &Value, detail: &mut TaskDetail) {
        let Story { steps, calls, .. } = self;
        story_step(record, steps, calls, detail);
        if record["source"] == "approval" {
            let payload = &record["payload"];
            let call =
                payload["request"]["callId"].as_str().or(payload["decision"]["callId"].as_str()).unwrap_or_default();
            if let Some(note) = approval_note(record) {
                self.about_call(call, note);
            }
        }
        if record["payload"]["type"] == "message_end" {
            let ids: Vec<String> = self.early.keys().filter(|c| self.calls.contains_key(*c)).cloned().collect();
            for id in ids {
                let notes = self.early.remove(&id).unwrap_or_default();
                let at = self.calls[&id] + 1;
                for (offset, note) in notes.into_iter().enumerate() {
                    self.steps.insert(at + offset, note);
                }
                // Steps after the insert moved; so did the calls among them.
                let moved: Vec<(String, usize)> = self.calls.iter().map(|(k, v)| (k.clone(), *v)).collect();
                for (call, index) in moved {
                    if index >= at {
                        self.calls.insert(call, index + 1);
                    }
                }
            }
        }
    }
}

fn approval_note(record: &Value) -> Option<StoryStep> {
    let at = record["ts"].as_str().unwrap_or_default();
    let payload = &record["payload"];
    if let Some(request) = payload.get("request") {
        let title = format!("Asked you to approve {}", request["tool"].as_str().unwrap_or_default());
        let detail = format!(
            "{} — {}",
            request["summary"].as_str().unwrap_or_default(),
            request["reason"].as_str().unwrap_or_default()
        );
        return Some(note(at, Tone::Warning, title, detail));
    }
    let decision = payload.get("decision")?;
    let (tone, title) = if decision["approved"] == true { (Tone::Good, "Approved") } else { (Tone::Bad, "Denied") };
    Some(note(at, tone, title, decision["note"].as_str().unwrap_or_default()))
}

fn story_step(record: &Value, steps: &mut Vec<StoryStep>, calls: &mut HashMap<String, usize>, detail: &mut TaskDetail) {
    let at = record["ts"].as_str().unwrap_or_default();
    let payload = &record["payload"];
    match record["source"].as_str().unwrap_or_default() {
        "pulse" => {
            let inner = &payload["payload"];
            let from =
                payload["source"]["channel"].as_str().or(payload["source"]["module"].as_str()).unwrap_or_default();
            let text = inner["text"].as_str().unwrap_or_default();
            match pulse_kind(record) {
                "userMessage" | "delegated" if inner["delivery"] == "steer" => {
                    steps.push(note(at, Tone::Plain, format!("Steered from {from}"), text));
                }
                "userMessage" | "delegated" if inner["delivery"] == "followUp" => {
                    steps.push(note(at, Tone::Plain, format!("Follow-up from {from}"), text));
                }
                "userMessage" | "delegated" => {
                    steps.push(StoryStep::Prompt { at: at.into(), text: text.into(), from: from.into() });
                }
                "runSettled" => {
                    let status = &inner["status"];
                    steps.push(StoryStep::Ended {
                        at: at.into(),
                        status: status["kind"].as_str().unwrap_or_default().into(),
                        text: status["text"].as_str().unwrap_or_default().into(),
                        answer: inner["answer"].as_str().unwrap_or_default().into(),
                    });
                }
                "toolFinished" => {
                    if let Some(&index) = inner["callId"].as_str().and_then(|c| calls.get(c))
                        && let StoryStep::Tool { duration_ms, .. } = &mut steps[index]
                    {
                        *duration_ms = inner["durationMs"].as_u64();
                    }
                }
                "thought" => steps.extend(super::planes::steer_step(at, &inner["thought"])),
                "memoryValve" => {
                    let decision = &inner["decision"];
                    if decision["attached"].as_array().is_some_and(|a| !a.is_empty()) {
                        steps.push(note(at, Tone::Plain, "Memories attached", compact(decision)));
                    }
                }
                _ => {}
            }
        }
        "agent" => {
            if payload["type"] != "message_end" {
                return;
            }
            let message = &payload["message"];
            match message["role"].as_str().unwrap_or_default() {
                "assistant" => said(at, message, steps, calls, detail),
                "toolResult" => {
                    let call = message["toolCallId"].as_str().unwrap_or_default();
                    if let Some(&index) = calls.get(call)
                        && let StoryStep::Tool { output, truncated, is_error, .. } = &mut steps[index]
                    {
                        let mut text = texts(&message["content"]);
                        if text.len() > OUTPUT_LIMIT {
                            let mut cut = OUTPUT_LIMIT;
                            while !text.is_char_boundary(cut) {
                                cut -= 1;
                            }
                            text.truncate(cut);
                            *truncated = true;
                        }
                        *output = Some(text);
                        *is_error = message["isError"] == true;
                    }
                }
                _ => {}
            }
        }
        "harness" => {
            if let Some(retry) = payload.get("retry") {
                let attempt = &retry["attempt"];
                let error = retry["errorMessage"].as_str().or(retry["finalError"].as_str()).unwrap_or_default();
                match retry["type"].as_str().unwrap_or_default() {
                    "auto_retry_start" | "summarization_retry_scheduled" => steps.push(note(
                        at,
                        Tone::Warning,
                        format!(
                            "Retry {attempt}/{} in {:.0} s",
                            retry["maxAttempts"],
                            retry["delayMs"].as_f64().unwrap_or_default() / 1000.0
                        ),
                        error,
                    )),
                    "auto_retry_end" if retry["success"] == true => {
                        steps.push(note(at, Tone::Good, format!("Came through after {attempt} retries"), ""));
                    }
                    "auto_retry_end" => {
                        steps.push(note(at, Tone::Bad, format!("Gave up after {attempt} retries"), error))
                    }
                    _ => {}
                }
            } else if let Some(compaction) = payload.get("compaction") {
                let title = format!("Compacted about {} tokens", compaction["tokensBefore"]);
                steps.push(note(at, Tone::Plain, title, compaction["reason"].as_str().unwrap_or_default()));
            } else if let Some(model) = payload["modelChange"].as_str() {
                steps.push(note(at, Tone::Plain, "Model changed", model));
            } else if payload.get("taskResumed").is_some() {
                steps.push(note(at, Tone::Warning, format!("Resumed (attempt {})", payload["attempt"]), ""));
            } else if payload.get("contextRecovery").is_some() {
                steps.push(note(
                    at,
                    Tone::Warning,
                    "Recovered from a full context",
                    compact(&payload["contextRecovery"]),
                ));
            }
        }
        "alert" => {
            if let Some((level, message, _)) = alert(record) {
                let tone = if level == "error" { Tone::Bad } else { Tone::Warning };
                steps.push(note(at, tone, format!("{} alert", capitalize(&level)), message));
            }
        }
        "board" if payload["stop"] == true => {
            steps.push(note(at, Tone::Warning, "You stopped the run from the board", ""));
        }
        _ => {}
    }
}

fn said(
    at: &str,
    message: &Value,
    steps: &mut Vec<StoryStep>,
    calls: &mut HashMap<String, usize>,
    detail: &mut TaskDetail,
) {
    let usage = &message["usage"];
    let tokens_in = usage["input"].as_u64().unwrap_or(0) + usage["cacheRead"].as_u64().unwrap_or(0);
    let tokens_out = usage["output"].as_u64().unwrap_or(0);
    let cost = usage["cost"]["total"].as_f64().unwrap_or(0.0);
    detail.tokens_in += tokens_in;
    detail.tokens_out += tokens_out;
    detail.cost += cost;
    let (mut text, mut thinking) = (Vec::new(), Vec::new());
    let mut tools = Vec::new();
    for block in message["content"].as_array().into_iter().flatten() {
        match block["type"].as_str().unwrap_or_default() {
            "text" => text.extend(block["text"].as_str()),
            "thinking" => thinking.extend(block["thinking"].as_str()),
            "toolCall" => tools.push(block),
            _ => {}
        }
    }
    let stop = message["stopReason"].as_str().unwrap_or_default();
    let error =
        (stop == "error" || stop == "aborted").then(|| message["errorMessage"].as_str().unwrap_or(stop).to_string());
    let text = text.join("\n\n");
    let thinking = thinking.join("\n\n");
    if !text.trim().is_empty() || !thinking.trim().is_empty() || error.is_some() || tools.is_empty() {
        steps.push(StoryStep::Said {
            at: at.into(),
            model: message["model"].as_str().unwrap_or_default().into(),
            text,
            thinking,
            tokens_in,
            tokens_out,
            cost,
            error,
        });
    }
    for call in tools {
        let call_id = call["id"].as_str().unwrap_or_default().to_string();
        calls.insert(call_id.clone(), steps.len());
        steps.push(StoryStep::Tool {
            at: at.into(),
            call_id,
            name: call["name"].as_str().unwrap_or_default().into(),
            summary: argument_summary(&call["arguments"]),
            arguments: serde_json::to_string_pretty(&call["arguments"]).unwrap_or_default(),
            output: None,
            truncated: false,
            is_error: false,
            duration_ms: None,
            device: call["arguments"]["device"].as_str().filter(|device| !device.is_empty()).map(str::to_string),
        });
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn record(seq: u64, ts: &str, source: &str, payload: Value, cause: Option<&str>) -> Value {
        let mut record = json!({ "ts": ts, "session": "s1", "seq": seq, "source": source, "payload": payload });
        if let Some(cause) = cause {
            record["cause"] = json!(cause);
        }
        record
    }

    fn pulse(id: &str, causation: Option<&str>, payload: Value) -> Value {
        json!({ "id": id, "causation": causation, "source": { "type": "userInput", "channel": "tui" }, "payload": payload })
    }

    fn task(id: &str, state: TaskState, parent: Option<&str>, session: &str) -> TaskRecord {
        TaskRecord {
            id: TaskId(id.into()),
            goal: format!("goal {id}"),
            prompt: "fix the build".into(),
            owner: "lucia".into(),
            origin: "tui".into(),
            parent: parent.map(|p| TaskId(p.into())),
            session_id: session.into(),
            session_path: None,
            cwd: "/tmp".into(),
            remote: None,
            pulse: Some(nypi_core::pulse::PulseId(format!("pulse-{id}"))),
            created: "2026-11-05T10:00:00+00:00".into(),
            heartbeat_at: "2026-11-05T10:05:00+00:00".into(),
            state,
            note: "Which branch?".into(),
            pid: 0,
            attempts: 1,
            open_tools: vec![],
            dispatched_at: None,
            dispatches: 0,
            wake: Vec::new(),
            rating: None,
            workspace: None,
            waits: None,
            goal_id: None,
            spawned_by_activation: None,
        }
    }

    #[test]
    fn a_task_reads_as_a_story_with_tool_results_and_its_child() {
        let mut tasks = BTreeMap::new();
        tasks.insert(TaskId("t1".into()), task("t1", TaskState::Done, None, "s1"));
        let mut child = task("t2", TaskState::Done, Some("t1"), "s2");
        child.created = "2026-11-05T10:00:30+00:00".into();
        tasks.insert(TaskId("t2".into()), child);
        let root = "pulse-t1";
        let usage = json!({ "input": 100, "output": 20, "cacheRead": 0, "cost": { "total": 0.01 } });
        let records = vec![
            record(
                0,
                "2026-11-05T09:00:00+00:00",
                "pulse",
                pulse("old", None, json!({ "kind": "userMessage", "text": "earlier" })),
                None,
            ),
            record(
                1,
                "2026-11-05T09:59:59+00:00",
                "pulse",
                pulse(root, None, json!({ "kind": "userMessage", "delivery": "prompt", "text": "fix the build" })),
                None,
            ),
            record(
                2,
                "2026-11-05T10:00:01+00:00",
                "agent",
                json!({ "type": "message_end", "message": { "role": "assistant", "model": "m", "usage": usage, "stopReason": "toolUse",
                "content": [ { "type": "thinking", "thinking": "look first" }, { "type": "toolCall", "id": "c1", "name": "bash", "arguments": { "command": "cargo build" } } ] } }),
                Some(root),
            ),
            record(
                3,
                "2026-11-05T10:00:02+00:00",
                "pulse",
                pulse(
                    "p3",
                    Some(root),
                    json!({ "kind": "toolFinished", "callId": "c1", "tool": "bash", "isError": true, "durationMs": 1500 }),
                ),
                None,
            ),
            record(
                4,
                "2026-11-05T10:00:02+00:00",
                "agent",
                json!({ "type": "message_end", "message": { "role": "toolResult", "toolCallId": "c1", "toolName": "bash", "isError": true, "content": [ { "type": "text", "text": "error[E0425]" } ] } }),
                Some(root),
            ),
            record(
                5,
                "2026-11-05T10:00:03+00:00",
                "harness",
                json!({ "retry": { "type": "auto_retry_start", "attempt": 1, "maxAttempts": 3, "delayMs": 2000, "errorMessage": "overloaded" } }),
                Some(root),
            ),
            json!({ "ts": "2026-11-05T10:01:00+00:00", "session": "s2", "seq": 1, "source": "agent", "cause": root,
                "payload": { "type": "message_end", "message": { "role": "assistant", "content": [ { "type": "text", "text": "child work" } ] } } }),
            record(
                6,
                "2026-11-05T10:02:00+00:00",
                "pulse",
                pulse(
                    "p6",
                    Some(root),
                    json!({ "kind": "runSettled", "status": { "kind": "done", "text": "built" }, "answer": "Fixed." }),
                ),
                None,
            ),
        ];
        let detail = task_detail(&tasks, "t1", &records, |_| true).unwrap();
        let kinds: Vec<&str> = detail
            .steps
            .iter()
            .map(|s| match s {
                StoryStep::Prompt { .. } => "prompt",
                StoryStep::Said { .. } => "said",
                StoryStep::Tool { .. } => "tool",
                StoryStep::Note { .. } => "note",
                StoryStep::Child { .. } => "child",
                StoryStep::Ended { .. } => "ended",
                StoryStep::Steer { .. } => "steer",
                StoryStep::Review { .. } => "review",
            })
            .collect();
        assert_eq!(kinds, ["prompt", "said", "tool", "note", "child", "ended"], "{:#?}", detail.steps);
        let StoryStep::Tool { summary, output, is_error, duration_ms, .. } = &detail.steps[2] else { unreachable!() };
        assert_eq!(
            (summary.as_str(), output.as_deref(), *is_error, *duration_ms),
            ("cargo build", Some("error[E0425]"), true, Some(1500))
        );
        assert_eq!(detail.tokens_in, 100);
        assert_eq!(detail.children.len(), 1);
    }

    #[test]
    fn the_inbox_holds_what_waits_and_forgets_what_was_acknowledged() {
        let mut tasks = BTreeMap::new();
        tasks.insert(TaskId("t1".into()), task("t1", TaskState::BlockedOnUser, None, "s1"));
        let mut running = task("t2", TaskState::Running, None, "s1");
        running.pid = 999_999;
        tasks.insert(TaskId("t2".into()), running);
        let alert = |seq, message: &str| {
            record(
                seq,
                "2026-11-05T10:00:00+00:00",
                "alert",
                json!({ "level": "warning", "message": message, "at": "x" }),
                None,
            )
        };
        let records = [alert(1, "disk"), alert(2, "disk"), alert(3, "network")];
        let day: Vec<&Value> = records.iter().collect();
        let items = inbox(&[], &tasks, &day, &HashSet::new(), |_| false);
        let keys: Vec<String> = items.iter().map(|i| i.key()).collect();
        assert_eq!(keys, ["blocked:t1", "stuck:t2", "alert:s1:3", "alert:s1:2"]);
        let acknowledged = HashSet::from(["alert:s1:2".to_string(), "stuck:t2".to_string()]);
        assert_eq!(inbox(&[], &tasks, &day, &acknowledged, |_| false).len(), 2);

        let today = today(&day, 7.0);
        assert!((today.cost_week_average - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_hub_at_a_look_is_a_tree_of_tasks_its_devices_and_the_vaults_last_notes() {
        let now = Utc::now();
        let at = |minutes: i64| (now - chrono::Duration::minutes(minutes)).to_rfc3339();
        let mut parent = task("t-parent", TaskState::Waiting, None, "s1");
        parent.heartbeat_at = at(5);
        parent.waits = Some(nypi_core::ledger::WaitPredicate::AllOf {
            tokens: vec![
                nypi_core::ledger::WaitToken::Delegation("d-1".into()),
                nypi_core::ledger::WaitToken::Delegation("d-2".into()),
            ],
        });
        parent.workspace = Some("10-workspaces/261119-deploy".into());
        let mut child = task("t-child", TaskState::Running, Some("t-parent"), "s2");
        child.heartbeat_at = at(1);
        child.remote = Some(nypi_core::place::DeviceFolder { device: "mini".into(), folder: "/srv/app".into() });
        let mut done = task("t-done", TaskState::Done, None, "s1");
        done.heartbeat_at = at(10);
        let mut old = task("t-old", TaskState::Done, None, "s1");
        old.heartbeat_at = at(600);
        let mut orphan = task("t-orphan", TaskState::Running, Some("t-old"), "s3");
        orphan.heartbeat_at = at(2);
        let tasks: BTreeMap<TaskId, TaskRecord> =
            [parent, child, done, old, orphan].into_iter().map(|task| (task.id.clone(), task)).collect();
        let tree = graph(&tasks, now, |_| true, |id| (id == "t-child").then(|| "p2".to_string()));
        let shape: Vec<(&str, u32)> = tree.iter().map(|node| (node.id.as_str(), node.depth)).collect();
        // Newest first, a task under the one it works for; what ended long ago is gone, and a
        // task whose parent is gone stands on its own.
        assert_eq!(shape, [("t-orphan", 0), ("t-parent", 0), ("t-child", 1), ("t-done", 0)]);
        assert_eq!(tree[1].waits.as_deref(), Some("all of 2"));
        assert_eq!(tree[1].workspace.as_deref(), Some("10-workspaces/261119-deploy"));
        assert_eq!((tree[2].place.as_str(), tree[2].process.as_deref()), ("app on mini", Some("p2")));

        let report = nypi_node::proto::Report {
            relay: false,
            identity: nypi_node::proto::Identity {
                name: "mini".into(),
                kind: nypi_node::proto::DeviceKind::Computer,
                system: "linux".into(),
                version: "0.1.0".into(),
                home: None,
            },
            tools: vec![],
            running: vec!["c-1".into()],
        };
        let register =
            BTreeMap::from([("mini".to_string(), nypi_hub::nodes::Known { report, connected: true, since: at(3) })]);
        use nypi_hub::pairing::{Peer, PeerKind, Trust};
        let peer =
            |name: &str, trust, kind| Peer { name: name.into(), trust, kind, paired_at: at(60), ..Default::default() };
        let peers = BTreeMap::from([
            ("aa".to_string(), peer("mini", Trust::Personal, PeerKind::Machine)),
            ("bb".to_string(), peer("phone", Trust::Personal, PeerKind::Browser)),
        ]);
        let cards = devices(&register, &peers, true);
        assert_eq!(cards.len(), 2);
        assert_eq!((cards[0].name.as_str(), cards[0].connected, cards[0].running), ("mini", true, 1));
        assert_eq!((cards[0].kind.as_str(), cards[0].paired.as_slice()), ("computer", &["personal".to_string()][..]));
        assert_eq!((cards[1].name.as_str(), cards[1].paired.as_slice()), ("phone", &["browser".to_string()][..]));
        assert!(!devices(&register, &peers, false)[0].connected, "a register nobody keeps is not believed");

        let vault = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(vault.path().join("00-inbox")).unwrap();
        std::fs::create_dir_all(vault.path().join("10-workspaces/261119-deploy")).unwrap();
        std::fs::create_dir_all(vault.path().join("20-playbooks")).unwrap();
        std::fs::write(vault.path().join("00-inbox/note.md"), "x").unwrap();
        std::fs::write(vault.path().join("10-workspaces/261119-deploy/findings.md"), "x").unwrap();
        std::fs::write(vault.path().join("20-playbooks/deploy.md"), "x").unwrap();
        let mut notes: Vec<String> = vault_activity(vault.path(), 6).into_iter().map(|note| note.path).collect();
        notes.sort();
        assert_eq!(notes, ["00-inbox/note.md", "10-workspaces/261119-deploy/findings.md"]);
    }
}
