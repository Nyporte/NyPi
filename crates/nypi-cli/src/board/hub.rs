//! The board's live picture (decision D56): one entry per connected process, kept current from
//! what its board link sends, and the way back to it for the owner's actions. Only this module
//! knows the link protocol; the rest of the board sees `nypi_board_proto` types.

use std::collections::BTreeMap;
use std::sync::Mutex;

use nypi_board_proto::{PlaneLive, Process, Step, TaskRef};
use nypi_core::board_link::{LinkDown, LinkTask, LinkUp};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

/// How much of the reply being written a live card keeps.
const TAIL: usize = 1200;

/// What changed, for the live stream.
#[derive(Clone, Debug)]
pub enum Change {
    /// Processes, steps, or approvals changed.
    Now,
    /// Stored state may have changed (a run settled, an alert, a task began).
    Stored,
    /// Text the process just wrote.
    Delta { process: String, text: String },
    /// Something happened in the process's run that its task's story shows.
    Story { process: String },
    /// What chat or the Principal is doing this moment (decision D88).
    Plane(PlaneLive),
}

/// A tool call waiting for the owner, as a link reported it.
#[derive(Clone, Debug)]
pub struct Approval {
    pub id: String,
    pub process: String,
    pub agent: String,
    pub tool: String,
    pub summary: String,
    pub reason: String,
    pub task: Option<TaskRef>,
    pub since: String,
}

struct Link {
    process: Process,
    down: mpsc::UnboundedSender<LinkDown>,
    approvals: BTreeMap<String, Approval>,
    /// The step an approval interrupted, restored when it closes.
    before_approval: Option<Step>,
    watchers: usize,
}

#[derive(Default)]
struct State {
    next: u64,
    links: BTreeMap<String, Link>,
    /// Viewers watching every process (the home screen).
    everyone: usize,
    /// What each plane is doing, while a turn or activation is in flight, and from which link.
    planes: BTreeMap<String, (String, PlaneLive)>,
}

impl State {
    fn watched(&self, link: &Link) -> bool {
        link.watchers > 0 || self.everyone > 0
    }
}

pub struct Hub {
    state: Mutex<State>,
    changes: broadcast::Sender<Change>,
}

/// How an action reached a process.
#[derive(Debug, PartialEq, Eq)]
pub enum Reach {
    Delivered,
    /// No connected process has it.
    Gone,
    /// The process is idle: there is no run to steer.
    Idle,
}

impl Default for Hub {
    fn default() -> Self {
        Hub { state: Mutex::default(), changes: broadcast::channel(4096).0 }
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl Hub {
    pub fn subscribe(&self) -> broadcast::Receiver<Change> {
        self.changes.subscribe()
    }

    /// Stored state changed outside any link (the owner acknowledged an item).
    pub fn nudge(&self) {
        self.changed(Change::Stored);
    }

    fn changed(&self, change: Change) {
        let _ = self.changes.send(change);
    }

    /// A process connected; `down` reaches it. Returns the board's name for it.
    pub fn connect(&self, down: mpsc::UnboundedSender<LinkDown>) -> String {
        let mut state = self.state.lock().unwrap();
        state.next += 1;
        let id = format!("p{}", state.next);
        let process = Process {
            id: id.clone(),
            pid: 0,
            front_end: String::new(),
            agent: String::new(),
            model: String::new(),
            cwd: String::new(),
            session_id: String::new(),
            connected_at: now(),
            task: None,
            step: Step::Idle,
            context_tokens: 0,
            context_window: 0,
            run_cost: 0.0,
            run_started: None,
            dropped: 0,
        };
        let link = Link { process, down, approvals: BTreeMap::new(), before_approval: None, watchers: 0 };
        state.links.insert(id.clone(), link);
        id
    }

    pub fn disconnect(&self, id: &str) {
        let mut state = self.state.lock().unwrap();
        state.links.remove(id);
        // A plane whose process went away is doing nothing any more.
        state.planes.retain(|_, (from, _)| from != id);
        drop(state);
        self.changed(Change::Now);
        self.changed(Change::Stored);
    }

    /// What a plane (`chat`, `principal`) is doing now, while it is in flight.
    pub fn plane(&self, plane: &str) -> Option<PlaneLive> {
        self.state.lock().unwrap().planes.get(plane).map(|(_, live)| live.clone())
    }

    pub fn processes(&self) -> Vec<Process> {
        self.state.lock().unwrap().links.values().map(|l| l.process.clone()).collect()
    }

    pub fn approvals(&self) -> Vec<Approval> {
        self.state.lock().unwrap().links.values().flat_map(|l| l.approvals.values().cloned()).collect()
    }

    /// The connected process working on this task.
    pub fn running(&self, task: &str) -> Option<String> {
        let state = self.state.lock().unwrap();
        state
            .links
            .values()
            .find(|l| l.process.task.as_ref().is_some_and(|t| t.id == task))
            .map(|l| l.process.id.clone())
    }

    /// Take in one message from a process's link.
    pub fn apply(&self, id: &str, message: LinkUp) {
        let mut changes = Vec::new();
        if let LinkUp::Plane { plane, id: activation, step, tail } = message {
            let live = PlaneLive { plane: plane.clone(), id: activation, step, tail, at: now() };
            let mut state = self.state.lock().unwrap();
            if live.step == "idle" {
                state.planes.remove(&plane);
            } else {
                state.planes.insert(plane, (id.to_string(), live.clone()));
            }
            drop(state);
            return self.changed(Change::Plane(live));
        }
        {
            let mut state = self.state.lock().unwrap();
            let everyone = state.everyone > 0;
            let Some(link) = state.links.get_mut(id) else { return };
            // Unwatched, a link sends only the first words of a reply: not worth showing.
            let watched = everyone || link.watchers > 0;
            match message {
                LinkUp::Hello { pid, front_end, agent, model, context_window, cwd, session_id, task } => {
                    let process = &mut link.process;
                    process.pid = pid;
                    process.front_end = front_end;
                    process.agent = agent;
                    process.model = model;
                    process.context_window = context_window;
                    process.cwd = cwd;
                    process.session_id = session_id;
                    set_task(link, task);
                    // A board that restarts while someone watches would otherwise stay blind.
                    if link.watchers > 0 || everyone {
                        let _ = link.down.send(LinkDown::Watch { deltas: true });
                    }
                    changes.push(Change::Now);
                }
                // Events come through the runtime's forwarder and can arrive after the approval
                // notice they precede; while an approval holds the step, they move what follows it.
                LinkUp::Event { event } if !link.approvals.is_empty() && link.before_approval.is_some() => {
                    let held =
                        std::mem::replace(&mut link.process.step, link.before_approval.take().unwrap_or_default());
                    changes.extend(step(link, &event, watched));
                    let after = std::mem::replace(&mut link.process.step, held);
                    link.before_approval = Some(after);
                }
                LinkUp::Event { event } => changes.extend(step(link, &event, watched)),
                LinkUp::Pulse { pulse } => {
                    let kind = pulse["payload"]["kind"].as_str().unwrap_or_default();
                    if matches!(kind, "runSettled" | "alert" | "triggered") {
                        changes.push(Change::Stored);
                    }
                    if link.process.task.is_some() {
                        changes.push(Change::Story { process: id.to_string() });
                    }
                }
                LinkUp::Task { task } => {
                    set_task(link, task);
                    changes.extend([Change::Now, Change::Stored]);
                }
                LinkUp::Model { model, context_window } => {
                    link.process.model = model;
                    link.process.context_window = context_window;
                    changes.push(Change::Now);
                }
                LinkUp::Session { session_id } => {
                    link.process.session_id = session_id;
                    changes.push(Change::Now);
                }
                LinkUp::ApprovalWaiting { id: approval, tool, summary, reason } => {
                    let since = now();
                    if link.before_approval.is_none() {
                        link.before_approval = Some(link.process.step.clone());
                    }
                    link.process.step =
                        Step::Approval { since: since.clone(), id: approval.clone(), tool: tool.clone() };
                    let entry = Approval {
                        id: approval.clone(),
                        process: id.to_string(),
                        agent: link.process.agent.clone(),
                        tool,
                        summary,
                        reason,
                        task: link.process.task.clone(),
                        since,
                    };
                    link.approvals.insert(approval, entry);
                    changes.push(Change::Now);
                }
                LinkUp::ApprovalClosed { id: approval } => {
                    link.approvals.remove(&approval);
                    if link.approvals.is_empty()
                        && let Some(step) = link.before_approval.take()
                    {
                        link.process.step = step;
                    }
                    changes.push(Change::Now);
                }
                LinkUp::Dropped { count } => {
                    link.process.dropped = count;
                    changes.push(Change::Now);
                }
                // The link's reader takes the card away (`board::link`).
                LinkUp::Bye => {}
                LinkUp::Plane { .. } => {}
            }
        }
        for change in changes {
            self.changed(change);
        }
    }

    /// Someone started (`true`) or stopped watching a process, or every process (None). A link
    /// sends reply text only while at least one viewer watches it.
    pub fn watch(&self, id: Option<&str>, watching: bool) {
        let mut state = self.state.lock().unwrap();
        let before: Vec<(String, bool)> = state.links.iter().map(|(k, l)| (k.clone(), state.watched(l))).collect();
        let count = |n: usize| if watching { n + 1 } else { n.saturating_sub(1) };
        match id {
            None => state.everyone = count(state.everyone),
            Some(id) => match state.links.get_mut(id) {
                Some(link) => link.watchers = count(link.watchers),
                None => return,
            },
        }
        for (id, was) in before {
            let link = &state.links[&id];
            let now = state.watched(link);
            if now != was {
                let _ = link.down.send(LinkDown::Watch { deltas: now });
            }
        }
    }

    /// Answer an approval wherever it waits.
    pub fn answer(&self, approval: &str, approved: bool, reason: Option<String>) -> Reach {
        let state = self.state.lock().unwrap();
        let Some(link) = state.links.values().find(|l| l.approvals.contains_key(approval)) else { return Reach::Gone };
        let command = LinkDown::Answer { id: approval.to_string(), approved, reason };
        if link.down.send(command).is_ok() { Reach::Delivered } else { Reach::Gone }
    }

    pub fn stop(&self, process: &str) -> Reach {
        let state = self.state.lock().unwrap();
        let Some(link) = state.links.get(process) else { return Reach::Gone };
        if link.process.run_started.is_none() {
            return Reach::Idle;
        }
        if link.down.send(LinkDown::Stop).is_ok() { Reach::Delivered } else { Reach::Gone }
    }

    pub fn message(&self, process: &str, text: String) -> Reach {
        let state = self.state.lock().unwrap();
        let Some(link) = state.links.get(process) else { return Reach::Gone };
        if link.process.run_started.is_none() {
            return Reach::Idle;
        }
        if link.down.send(LinkDown::Message { text }).is_ok() { Reach::Delivered } else { Reach::Gone }
    }
}

fn set_task(link: &mut Link, task: Option<LinkTask>) {
    link.process.task = task.map(|t| TaskRef { id: t.id, goal: t.goal });
}

/// Move the process's step along with one runtime event (`RuntimeEvent` JSON).
fn step(link: &mut Link, event: &Value, watched: bool) -> Vec<Change> {
    let id = link.process.id.clone();
    let process = &mut link.process;
    let story = Change::Story { process: id.clone() };
    let since = now();
    let waiting = || Step::Waiting { since: now() };
    match event["kind"].as_str().unwrap_or_default() {
        "agent" => {
            let event = &event["event"];
            match event["type"].as_str().unwrap_or_default() {
                "agent_start" => {
                    process.run_started = Some(since);
                    process.run_cost = 0.0;
                    process.step = waiting();
                    vec![Change::Now]
                }
                "message_start" if event["message"]["role"] == "assistant" => {
                    process.step = waiting();
                    vec![Change::Now]
                }
                "message_update" => {
                    let delta = &event["event"];
                    let text = delta["delta"].as_str().unwrap_or_default();
                    match delta["type"].as_str().unwrap_or_default() {
                        "thinking_delta" => {
                            if !matches!(process.step, Step::Thinking { .. }) {
                                process.step = Step::Thinking { since };
                                return vec![Change::Now];
                            }
                            vec![]
                        }
                        "text_delta" => {
                            let text = if watched { text } else { "" };
                            let mut changes = vec![Change::Delta { process: id, text: text.to_string() }];
                            match &mut process.step {
                                Step::Writing { tail, .. } => push_tail(tail, text),
                                step => {
                                    let mut tail = String::new();
                                    push_tail(&mut tail, text);
                                    *step = Step::Writing { since, tail };
                                    changes.push(Change::Now);
                                }
                            }
                            changes
                        }
                        _ => vec![],
                    }
                }
                "message_end" if event["message"]["role"] == "assistant" => {
                    let usage = &event["message"]["usage"];
                    let total = usage["totalTokens"].as_u64().unwrap_or(0);
                    let parts = ["input", "output", "cacheRead", "cacheWrite"];
                    let context = if total > 0 { total } else { parts.iter().filter_map(|k| usage[*k].as_u64()).sum() };
                    if context > 0 {
                        process.context_tokens = context;
                    }
                    process.run_cost += usage["cost"]["total"].as_f64().unwrap_or(0.0);
                    vec![Change::Now, story]
                }
                "tool_execution_start" => {
                    let name = event["tool_name"].as_str().unwrap_or_default().to_string();
                    let summary = argument_summary(&event["arguments"]);
                    process.step = Step::Tool { since, name, summary };
                    vec![Change::Now, story]
                }
                "tool_execution_end" => {
                    process.step = waiting();
                    vec![Change::Now, story]
                }
                "agent_end" => {
                    process.step = Step::Idle;
                    process.run_started = None;
                    link.before_approval = None;
                    vec![Change::Now, story, Change::Stored]
                }
                _ => vec![],
            }
        }
        "retry" => {
            let notice = &event["notice"];
            match notice["type"].as_str().unwrap_or_default() {
                "auto_retry_start" | "summarization_retry_scheduled" => {
                    process.step = Step::Retrying {
                        since,
                        attempt: notice["attempt"].as_u64().unwrap_or(0) as u32,
                        max_attempts: notice["maxAttempts"].as_u64().unwrap_or(0) as u32,
                        delay_ms: notice["delayMs"].as_u64().unwrap_or(0),
                        error: notice["errorMessage"].as_str().unwrap_or_default().to_string(),
                    };
                    vec![Change::Now, story]
                }
                "auto_retry_end" => {
                    if matches!(process.step, Step::Retrying { .. }) {
                        process.step = waiting();
                    }
                    vec![Change::Now, story]
                }
                _ => vec![],
            }
        }
        "compaction_start" => {
            let reason = event["reason"].as_str().unwrap_or_default().to_string();
            process.step = Step::Compacting { since, reason };
            vec![Change::Now, story]
        }
        "compaction_end" => {
            process.step = if process.run_started.is_some() { waiting() } else { Step::Idle };
            vec![Change::Now, story]
        }
        "alert" => vec![Change::Stored],
        _ => vec![],
    }
}

fn push_tail(tail: &mut String, text: &str) {
    tail.push_str(text);
    if tail.len() > TAIL {
        let mut cut = tail.len() - TAIL;
        while !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail.drain(..cut);
    }
}

/// A tool call's arguments in one line: the argument that says what it does.
pub fn argument_summary(arguments: &Value) -> String {
    const KEYS: [&str; 10] =
        ["command", "path", "file_path", "pattern", "query", "url", "task", "goal", "text", "name"];
    let text = KEYS.iter().find_map(|k| arguments[*k].as_str()).map(str::to_string).unwrap_or_else(|| {
        let compact = arguments.to_string();
        if compact == "{}" || compact == "null" { String::new() } else { compact }
    });
    let line = text.lines().next().unwrap_or_default();
    let mut short: String = line.chars().take(140).collect();
    if short.len() < text.len() {
        short.push('…');
    }
    short
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn agent(event: Value) -> LinkUp {
        LinkUp::Event { event: json!({ "kind": "agent", "event": event }) }
    }

    /// What a plane is doing this moment (decision D88): kept while it is in flight, sent to
    /// every viewer, and gone when it ends or its process does.
    #[test]
    fn a_planes_progress_is_kept_while_in_flight() {
        let hub = Hub::default();
        let mut changes = hub.subscribe();
        let (down, _sent) = mpsc::unbounded_channel();
        let id = hub.connect(down);
        let plane = |step: &str, tail: &str| LinkUp::Plane {
            plane: "principal".into(),
            id: "a-1".into(),
            step: step.into(),
            tail: tail.into(),
        };
        hub.apply(&id, plane("Writing", "Started the price"));
        let live = hub.plane("principal").unwrap();
        assert_eq!((live.id.as_str(), live.step.as_str(), live.tail.as_str()), ("a-1", "Writing", "Started the price"));
        assert!(matches!(changes.try_recv(), Ok(Change::Plane(sent)) if sent == live));
        assert!(hub.plane("chat").is_none());
        hub.apply(&id, plane("idle", ""));
        assert!(hub.plane("principal").is_none());
        assert!(matches!(changes.try_recv(), Ok(Change::Plane(sent)) if sent.step == "idle"));
        hub.apply(&id, plane("Calling start_task", ""));
        hub.disconnect(&id);
        assert!(hub.plane("principal").is_none(), "its process went away");
    }

    #[test]
    fn steps_follow_the_run_and_approvals_interrupt_it() {
        let hub = Hub::default();
        let (down, mut sent) = mpsc::unbounded_channel();
        let id = hub.connect(down);
        let step = |hub: &Hub| hub.processes()[0].step.clone();
        hub.apply(&id, agent(json!({ "type": "agent_start" })));
        assert!(matches!(step(&hub), Step::Waiting { .. }));
        assert_eq!(hub.stop(&id), Reach::Delivered);
        assert!(matches!(sent.try_recv(), Ok(LinkDown::Stop)));

        let delta = |kind: &str, text: &str| {
            agent(json!({ "type": "message_update", "event": { "type": kind, "index": 0, "delta": text } }))
        };
        hub.apply(&id, delta("thinking_delta", "hm"));
        assert!(matches!(step(&hub), Step::Thinking { .. }));
        hub.apply(&id, delta("text_delta", "Unwatched"));
        assert!(
            matches!(step(&hub), Step::Writing { tail, .. } if tail.is_empty()),
            "the first words alone say nothing"
        );
        hub.watch(Some(&id), true);
        assert!(matches!(sent.try_recv(), Ok(LinkDown::Watch { deltas: true })));
        hub.apply(&id, agent(json!({ "type": "message_start", "message": { "role": "assistant" } })));
        hub.apply(&id, delta("text_delta", "Hello "));
        hub.apply(&id, delta("text_delta", "there"));
        assert!(matches!(step(&hub), Step::Writing { tail, .. } if tail == "Hello there"));

        hub.apply(
            &id,
            agent(json!({ "type": "tool_execution_start", "call_id": "c1", "tool_name": "bash", "arguments": { "command": "rm -rf build\nls" } })),
        );
        assert!(matches!(step(&hub), Step::Tool { name, summary, .. } if name == "bash" && summary == "rm -rf build…"));
        let waiting = LinkUp::ApprovalWaiting {
            id: "a1".into(),
            tool: "bash".into(),
            summary: "rm".into(),
            reason: "deletes".into(),
        };
        hub.apply(&id, waiting);
        assert!(matches!(step(&hub), Step::Approval { .. }));
        assert_eq!(hub.approvals().len(), 1);
        assert_eq!(hub.answer("a1", false, Some("no".into())), Reach::Delivered);
        assert!(matches!(sent.try_recv(), Ok(LinkDown::Answer { approved: false, .. })));
        hub.apply(&id, LinkUp::ApprovalClosed { id: "a1".into() });
        assert!(matches!(step(&hub), Step::Tool { .. }), "the approval gives the step back");
        assert_eq!(hub.answer("a1", true, None), Reach::Gone);

        let usage = json!({ "input": 900, "output": 100, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0, "cost": { "total": 0.25 } });
        hub.apply(&id, agent(json!({ "type": "message_end", "message": { "role": "assistant", "usage": usage } })));
        assert_eq!(hub.processes()[0].context_tokens, 1000);
        hub.apply(&id, agent(json!({ "type": "agent_end" })));
        assert_eq!(step(&hub), Step::Idle);
        assert!((hub.processes()[0].run_cost - 0.25).abs() < 1e-9);
        assert_eq!(hub.message(&id, "hi".into()), Reach::Idle, "no run to steer");

        hub.watch(None, true);
        hub.watch(Some(&id), false);
        assert!(sent.try_recv().is_err(), "the home screen still watches");
        hub.watch(None, false);
        assert!(matches!(sent.try_recv(), Ok(LinkDown::Watch { deltas: false })));
        hub.disconnect(&id);
        assert!(hub.processes().is_empty());
    }
}
