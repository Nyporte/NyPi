//! The program as a map (decision D65): every module of NyPi, the connections between them,
//! and what each is doing now, for the board's System screen. The modules and connections are
//! written out here, by hand, because that structure lives in the code and nowhere a program
//! could read it; a test holds the list to the source tree, so a module that is renamed or
//! removed fails the build instead of staying on the map. The state of each module is read
//! fresh every time: from the processes linked to the board, the heartbeats in the run folder,
//! the settings, the stores, and the last day of the audit log.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use nypi_board_proto::{EdgeKind, Fact, NodeState, Process, Step, TopoEdge, TopoGroup, TopoNode, Topology};
use nypi_core::config::{Paths, Settings};
use nypi_core::ledger::Ledger;
use nypi_core::outbox::Outbox;
use nypi_core::portrait::PortraitMode;
use nypi_core::reflection::ReflectionMode;
use nypi_core::sandbox::SandboxMode;
use nypi_core::schedule::ScheduleStore;
use serde_json::Value;

use super::Board;

/// The regions of the map, in reading order.
const GROUPS: [(&str, &str, &str); 5] = [
    ("you", "You", "Where your words go in and her answers come out"),
    ("always", "Always on", "What keeps running with every window closed"),
    ("lucia", "Lucia", "Three planes: chat talks and reads, the Principal decides, workers act (decision D84)"),
    ("resources", "Tools and models", "What she works with and through"),
    ("stores", "Kept on disk", "Append-only files in NyPi's home; only the harness writes them"),
];

/// Every module: id, group, name, what it is for, where it lives.
const MODULES: [(&str, &str, &str, &str, &str); 40] = [
    ("owner", "you", "You", "The user. Everything here serves you and answers to you.", "docs/principles.md"),
    (
        "terminal",
        "you",
        "Terminal",
        "`nypi` in a folder, line by line or full-screen: that folder's conversation in the hub, or in a hub of its own without one.",
        "crates/nypi-cli/src/tui/mod.rs",
    ),
    (
        "attach",
        "you",
        "send / attach",
        "`nypi send` and `nypi attach`: talk to the hub from any terminal.",
        "crates/nypi-cli/src/client.rs",
    ),
    (
        "telegram",
        "you",
        "Telegram",
        "A bot you message from your phone: a channel inside the hub, talking to the same Lucia.",
        "crates/nypi-hub/src/channels/telegram/mod.rs",
    ),
    (
        "board",
        "you",
        "Board",
        "This page: watch, approve, stop, and message from a browser.",
        "crates/nypi-cli/src/board/mod.rs",
    ),
    (
        "programs",
        "you",
        "Other programs",
        "`nypi rpc` and `nypi --json`: scripts and apps drive her over JSON lines, or read one run's events.",
        "crates/nypi-hub/src/rpc.rs",
    ),
    (
        "webhook",
        "you",
        "Inbound webhook",
        "Material other programs hand in over HTTP: saved to the vault's inbox, then the Principal's to file.",
        "crates/nypi-hub/src/channels/webhook.rs",
    ),
    (
        "supervisor",
        "always",
        "Supervisor",
        "Keeps the hub alive and does the rounds that must not depend on it.",
        "crates/nypi-cli/src/supervisor.rs",
    ),
    (
        "hub",
        "always",
        "Hub",
        "Lucia as one process that is always there (`nypi hub run`), taking messages on a local socket.",
        "crates/nypi-hub/src/daemon/mod.rs",
    ),
    (
        "watchdog",
        "always",
        "Watchdog",
        "Checks the task ledger every minute for stalled, interrupted, and hopeless tasks.",
        "crates/nypi-core/src/watchdog.rs",
    ),
    (
        "scheduler",
        "always",
        "Scheduler",
        "Fires timers and condition triggers; a fired prompt becomes a run.",
        "crates/nypi-core/src/schedule.rs",
    ),
    (
        "dmn",
        "always",
        "Idle passes",
        "The DMN: while you are away she reviews goals, tasks, and memory, and may act.",
        "crates/nypi-core/src/dmn.rs",
    ),
    (
        "outbox",
        "always",
        "Outbox",
        "Every notice is written here before it is sent, and sent again until it reaches you.",
        "crates/nypi-core/src/outbox.rs",
    ),
    (
        "chat",
        "lucia",
        "Chat",
        "The one voice: a fast model that answers at once, reads everything, changes nothing, and signals the Principal.",
        "crates/nypi-core/src/chat.rs",
    ),
    (
        "principal",
        "lucia",
        "Principal",
        "Decides what is done: one activation at a time, a user's signal first; starts and reviews tasks, keeps the stores.",
        "crates/nypi-core/src/principal/mod.rs",
    ),
    (
        "worker",
        "lucia",
        "Worker",
        "The task pool's workers: the strong model and every tool, several tasks at once, each to a brief.",
        "crates/nypi-core/src/runtime/mod.rs",
    ),
    (
        "thinking",
        "lucia",
        "Thinking",
        "Watches a working run's course and steers it when it goes wrong.",
        "crates/nypi-core/src/thinking.rs",
    ),
    (
        "valve",
        "lucia",
        "Recall",
        "The memory valve: attaches the memories a message calls for, by situation.",
        "crates/nypi-core/src/valve.rs",
    ),
    (
        "prompt",
        "lucia",
        "Prompt",
        "The layered system prompt: core, her own lessons, the situation, goals, portrait, skills.",
        "crates/nypi-core/src/prompt.rs",
    ),
    (
        "reflection",
        "lucia",
        "Reflection",
        "After a task that can teach something, writes lessons into her own prompt.",
        "crates/nypi-core/src/reflection.rs",
    ),
    (
        "portrait",
        "lucia",
        "Portrait",
        "Learns lasting traits about you from your own words.",
        "crates/nypi-core/src/portrait.rs",
    ),
    (
        "goals",
        "lucia",
        "Goals",
        "Your goals; she records progress on them and advances them.",
        "crates/nypi-core/src/goals.rs",
    ),
    (
        "compaction",
        "lucia",
        "Compaction",
        "Summarizes older context when the model's window fills.",
        "crates/nypi-core/src/compaction.rs",
    ),
    (
        "bus",
        "lucia",
        "Pulse bus",
        "One bus for the whole hub: every message, tool call, and result is a pulse that names its conversation and the pulse that caused it.",
        "crates/nypi-core/src/pulse.rs",
    ),
    (
        "inspector",
        "lucia",
        "Inspector",
        "Guards every tool call, asks you before risky ones, and writes the audit log.",
        "crates/nypi-core/src/inspector.rs",
    ),
    (
        "models",
        "resources",
        "Models",
        "The providers: OpenAI-style, Responses, Gemini, Anthropic, and Bedrock through one client.",
        "crates/nypi-ai/src/providers/mod.rs",
    ),
    (
        "tools",
        "resources",
        "Tools",
        "read, bash, edit, write, grep, find, ls, and her own: memory, schedule, goals, delegate.",
        "crates/nypi-core/src/tools/mod.rs",
    ),
    (
        "sandbox",
        "resources",
        "Sandbox",
        "Every program a tool starts runs where it cannot touch NyPi's own files or sockets.",
        "crates/nypi-core/src/sandbox.rs",
    ),
    ("mcp", "resources", "MCP servers", "Tool servers from outside, named in settings.", "crates/nypi-core/src/mcp.rs"),
    (
        "devices",
        "resources",
        "Devices",
        "Where her file and shell tools run: this computer's node in the hub's process, and other computers' nodes (`nypi node`), each under its own guard.",
        "crates/nypi-hub/src/nodes.rs",
    ),
    (
        "specialists",
        "resources",
        "Specialists",
        "Experts she delegates to: native ones, or Codex and Claude Code.",
        "crates/nypi-core/src/specialist.rs",
    ),
    (
        "skills",
        "resources",
        "Skills",
        "Instructions for a kind of task, loaded when one applies.",
        "crates/nypi-core/src/skills.rs",
    ),
    (
        "sessions",
        "stores",
        "Sessions",
        "Each conversation, message by message, so a run can be resumed.",
        "crates/nypi-core/src/session.rs",
    ),
    (
        "ledger",
        "stores",
        "Task ledger",
        "Every run is a task, written before it starts, with your verdict on it.",
        "crates/nypi-core/src/ledger.rs",
    ),
    (
        "audit",
        "stores",
        "Audit log",
        "Every event, by day; only the Inspector writes it.",
        "crates/nypi-core/src/audit_log.rs",
    ),
    (
        "memory",
        "stores",
        "Memory",
        "What she remembers, with its importance, its emotion, and how often it was recalled.",
        "crates/nypi-vault/src/episodic.rs",
    ),
    (
        "vault",
        "stores",
        "Vault",
        "Her own Markdown notes: an inbox, a workspace per task, playbooks, entity pages, an archive; found by meaning, words, and links.",
        "crates/nypi-vault/src/vault.rs",
    ),
    (
        "secrets",
        "stores",
        "Assets and secrets",
        "A registry of what she looks after, which she can read, and the secrets themselves, sealed: named by handle, used for her, never shown to her.",
        "crates/nypi-vault/src/secrets.rs",
    ),
    (
        "journal",
        "stores",
        "Planes' logs",
        "Every chat turn, every step of the Principal's activations, and its decision journal.",
        "crates/nypi-core/src/principal/mod.rs",
    ),
    (
        "settings",
        "stores",
        "Settings and trust",
        "settings.json, the trust store, and the keys: yours alone, never a tool's.",
        "crates/nypi-core/src/config.rs",
    ),
];

use EdgeKind::{Message, Notice, Record, Watch, Work};

/// Every connection: from, to, what goes over it, of which kind.
const LINKS: [(&str, &str, &str, EdgeKind); 66] = [
    ("owner", "terminal", "you type", Message),
    ("owner", "attach", "you type", Message),
    ("owner", "telegram", "you message", Message),
    ("owner", "board", "you look, approve, stop", Message),
    ("terminal", "hub", "its folder's thread", Message),
    ("terminal", "worker", "a hub of its own, without the hub", Message),
    ("telegram", "hub", "a channel inside it", Message),
    ("programs", "worker", "commands and prompts", Message),
    ("attach", "hub", "the hub's socket", Message),
    ("hub", "worker", "hosts her", Message),
    ("board", "inspector", "your answer to an approval", Message),
    ("board", "worker", "stop, or a message into the run", Message),
    ("chat", "principal", "signals what needs doing", Message),
    ("principal", "chat", "what the user must hear", Message),
    ("principal", "worker", "starts, steers, reviews tasks", Message),
    ("thinking", "worker", "steers a run off course", Message),
    ("webhook", "hub", "material for the inbox", Message),
    ("scheduler", "principal", "a fired prompt, with chat in front", Work),
    ("dmn", "principal", "idle time by two clocks, with chat in front", Work),
    ("principal", "vault", "goals' plans and notes", Record),
    ("principal", "journal", "each decision and step", Record),
    ("chat", "journal", "each turn", Record),
    ("chat", "devices", "reads a file, at most 1.5 s", Work),
    ("valve", "worker", "attaches memories", Message),
    ("prompt", "worker", "the system prompt", Message),
    ("goals", "prompt", "your goals", Message),
    ("portrait", "prompt", "what she knows of you", Message),
    ("reflection", "prompt", "lessons learned", Message),
    ("skills", "prompt", "which skills exist", Message),
    ("worker", "models", "asks the model", Work),
    ("chat", "models", "asks the fast model", Work),
    ("worker", "tools", "calls a tool", Work),
    ("tools", "sandbox", "every program it starts", Work),
    ("tools", "devices", "runs on a device's node", Work),
    ("hub", "devices", "keeps the register of nodes", Watch),
    ("worker", "mcp", "calls a server's tool", Work),
    ("worker", "specialists", "delegates", Work),
    ("worker", "compaction", "when the context fills", Work),
    ("worker", "reflection", "after a task", Work),
    ("dmn", "worker", "an idle pass", Work),
    ("scheduler", "worker", "a fired prompt", Work),
    ("watchdog", "worker", "resumes a cut-off task", Work),
    ("supervisor", "hub", "keeps alive", Watch),
    ("supervisor", "watchdog", "runs its rounds", Watch),
    ("supervisor", "scheduler", "runs its rounds", Watch),
    ("supervisor", "outbox", "sends again", Watch),
    ("scheduler", "dmn", "starts a pass when due", Watch),
    ("inspector", "tools", "guards every call", Watch),
    ("worker", "bus", "every step is a pulse", Watch),
    ("bus", "inspector", "sees every pulse", Watch),
    ("bus", "valve", "what you type and say", Watch),
    ("bus", "thinking", "each turn of a run", Watch),
    ("watchdog", "ledger", "checks", Watch),
    ("worker", "sessions", "writes each message", Record),
    ("worker", "ledger", "task, heartbeat, tool calls", Record),
    ("inspector", "audit", "writes every event", Record),
    ("valve", "memory", "reads", Record),
    ("tools", "memory", "the memory tool", Record),
    ("tools", "vault", "reads and writes her notes", Record),
    ("tools", "secrets", "a credential used by its handle", Record),
    ("inspector", "secrets", "asks before a secret is used", Watch),
    ("settings", "inspector", "approval rules, trust", Record),
    ("board", "audit", "reads", Record),
    ("inspector", "owner", "asks your approval", Notice),
    ("outbox", "owner", "tells you", Notice),
    ("hub", "outbox", "results when nobody watches", Notice),
];

/// The last day's audit records, counted by what the map shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DayCounts {
    pub records: u32,
    pub pulses: u32,
    /// Pulses by the module that published them (chat, runtime, valve, thinking, ...).
    pub by_module: BTreeMap<String, u32>,
    pub tool_calls: u32,
    pub tool_failures: u32,
    pub replies: u32,
    pub retries: u32,
    pub alerts: u32,
    pub recalls: u32,
    pub compactions: u32,
    pub settled: (u32, u32, u32),
}

pub fn day_counts<'a>(records: impl Iterator<Item = &'a Value>) -> DayCounts {
    let mut counts = DayCounts::default();
    for record in records {
        counts.records += 1;
        let payload = &record["payload"];
        if payload["type"] == "message_end" && payload["message"]["role"] == "assistant" {
            counts.replies += 1;
        }
        if record["source"] == "harness" {
            if payload["retry"]["type"] == "auto_retry_start" && payload["retry"]["attempt"] == 1 {
                counts.retries += 1;
            }
            if payload.get("compaction").is_some() {
                counts.compactions += 1;
            }
        }
        if record["source"] != "pulse" {
            continue;
        }
        counts.pulses += 1;
        if let Some(module) = payload["source"]["module"].as_str() {
            *counts.by_module.entry(module.to_string()).or_default() += 1;
        }
        match payload["payload"]["kind"].as_str().unwrap_or_default() {
            "toolFinished" => {
                counts.tool_calls += 1;
                counts.tool_failures += (payload["payload"]["isError"] == true) as u32;
            }
            "alert" => counts.alerts += 1,
            "memoryValve" => counts.recalls += 1,
            "runSettled" => match payload["payload"]["status"]["kind"].as_str().unwrap_or_default() {
                "done" => counts.settled.0 += 1,
                "blocked" => counts.settled.1 += 1,
                "failed" => counts.settled.2 += 1,
                _ => {}
            },
            _ => {}
        }
    }
    counts
}

/// Everything the map's states are read from, gathered in one place so `build` is pure.
#[derive(Default)]
pub struct Inputs {
    pub settings: Settings,
    pub processes: Vec<Process>,
    /// Items in the board's inbox, and how many of them are approvals.
    pub waiting: usize,
    pub approvals: usize,
    pub supervisor_installed: bool,
    pub supervisor_alive: bool,
    pub daemon: Option<crate::daemon::Heartbeat>,
    pub daemon_alive: bool,
    pub telegram_chats: usize,
    pub tasks: usize,
    pub tasks_open: usize,
    pub tasks_interrupted: usize,
    pub schedules: usize,
    pub next_firing: Option<String>,
    pub notices: usize,
    pub notices_open: usize,
    pub memories: usize,
    pub traits: usize,
    pub goals_open: usize,
    pub specialists: Vec<String>,
    pub skills: usize,
    pub dmn_passes_today: usize,
    pub sandbox_here: bool,
    /// Notes in the vault, records in the asset registry, and secrets stored (counted, never read).
    pub vault_notes: usize,
    pub assets: usize,
    pub secrets: usize,
    /// The register of nodes: each device by name, and whether its node is connected.
    pub devices: Vec<(String, bool)>,
    pub day: DayCounts,
    /// Chat and the Principal as the home screen last showed them (decision D86).
    pub planes: nypi_board_proto::Planes,
}

fn fact(label: &str, value: impl ToString) -> Fact {
    Fact { label: label.to_string(), value: value.to_string() }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// A linked process that is not one of the named ways in is a terminal session.
fn is_terminal(process: &Process) -> bool {
    !matches!(
        process.front_end.as_str(),
        "telegram"
            | "rpc"
            | "print"
            | "hub"
            | "daemon"
            | "specialist"
            | "delegate"
            | "dmn"
            | "timer"
            | "goals"
            | "resume"
            | "report"
    )
}

/// The conversation thread of the hub; "daemon" is what a hub before decision D100 named it.
fn is_hub(process: &Process) -> bool {
    matches!(process.front_end.as_str(), "hub" | "daemon")
}

fn busy(process: &Process) -> bool {
    process.task.is_some()
}

/// The state of a way in that exists only while it is used.
fn presence(processes: &[&Process]) -> (NodeState, String) {
    match (processes.len(), processes.iter().filter(|p| busy(p)).count()) {
        (0, _) => (NodeState::Absent, "not open".into()),
        (open, 0) => (NodeState::Ready, format!("{open} open, idle")),
        (open, working) => (NodeState::Active, format!("{working} of {open} working")),
    }
}

/// What a module is doing now: state, a few words, facts for the detail panel, and a link.
fn describe(id: &str, input: &Inputs) -> (NodeState, String, Vec<Fact>, Option<String>) {
    let settings = &input.settings;
    let day = &input.day;
    let all: Vec<&Process> = input.processes.iter().collect();
    let of =
        |wanted: &dyn Fn(&Process) -> bool| -> Vec<&Process> { all.iter().copied().filter(|p| wanted(p)).collect() };
    let running: Vec<&Process> = of(&busy);
    let module = |name: &str| day.by_module.get(name).copied().unwrap_or(0);
    // Something long-lived is doing the watchdog's and the scheduler's rounds.
    let watching =
        input.supervisor_alive || input.daemon_alive || all.iter().any(|p| !matches!(p.front_end.as_str(), "print"));
    let (state, status, facts, link): (NodeState, String, Vec<Fact>, Option<String>) = match id {
        "owner" => match input.waiting {
            0 => (NodeState::Ready, "nothing waits for you".into(), vec![], Some("/".into())),
            waiting => (
                NodeState::Active,
                format!("{} for you", plural(waiting, "thing waits", "things wait")),
                vec![fact("Approvals among them", input.approvals)],
                Some("/".into()),
            ),
        },
        "terminal" => {
            let (state, status) = presence(&of(&is_terminal));
            (state, status, vec![], None)
        }
        "programs" => {
            let (state, status) = presence(&of(&|p| matches!(p.front_end.as_str(), "rpc" | "print")));
            (state, status, vec![], None)
        }
        "attach" => match (&input.daemon, input.daemon_alive) {
            (Some(beat), true) if beat.clients > 0 => {
                (NodeState::Active, format!("{} attached", beat.clients), vec![], None)
            }
            (_, true) => (NodeState::Ready, "nobody attached".into(), vec![], None),
            _ => (NodeState::Absent, "the hub is not running".into(), vec![], None),
        },
        "telegram" => {
            let chats = plural(input.telegram_chats, "chat paired", "chats paired");
            let facts = vec![fact("Paired chats", input.telegram_chats)];
            let in_hub = input.daemon_alive
                && input.daemon.as_ref().is_some_and(|beat| beat.channels.iter().any(|channel| channel == "telegram"));
            if in_hub && input.telegram_chats == 0 {
                (NodeState::Ready, "in the hub; pair a chat with nypi telegram".into(), facts, None)
            } else if in_hub {
                (NodeState::Ready, format!("in the hub; {chats}"), facts, None)
            } else if input.telegram_chats > 0 {
                (NodeState::Absent, "paired, but the hub is not running it".into(), facts, None)
            } else {
                (NodeState::Off, "not set up".into(), facts, None)
            }
        }
        "board" => (
            NodeState::Ready,
            format!("{} linked", plural(all.len(), "process", "processes")),
            all.iter().map(|p| fact(&p.front_end, format!("pid {}", p.pid))).collect(),
            Some("/activity".into()),
        ),
        "supervisor" => {
            if input.supervisor_alive {
                (NodeState::Ready, "doing its rounds".into(), vec![], None)
            } else if input.supervisor_installed {
                (NodeState::Down, "installed, but not running".into(), vec![], None)
            } else {
                (NodeState::Off, "not installed: nypi hub up".into(), vec![], None)
            }
        }
        "hub" => match (&input.daemon, input.daemon_alive) {
            (Some(beat), true) => {
                let facts = vec![
                    fact("Process", format!("pid {}", beat.pid)),
                    fact("Works in", beat.cwd.display()),
                    fact("Attached", beat.clients),
                ];
                // The heartbeat is a few seconds old; a linked process says so at once.
                if beat.busy || all.iter().any(|p| is_hub(p) && busy(p)) {
                    (NodeState::Active, "on a task".into(), facts, None)
                } else {
                    (NodeState::Ready, "idle, listening".into(), facts, None)
                }
            }
            _ if !settings.hub.enabled => (NodeState::Off, "turned off (hub.enabled)".into(), vec![], None),
            _ if input.supervisor_alive => {
                (NodeState::Down, "not running; the supervisor is starting it".into(), vec![], None)
            }
            _ => (NodeState::Absent, "not running: nothing keeps it up".into(), vec![], None),
        },
        "watchdog" => {
            let facts =
                vec![fact("Open tasks", input.tasks_open), fact("Cut off, waiting to resume", input.tasks_interrupted)];
            if !settings.watchdog.enabled {
                (NodeState::Off, "turned off (watchdog.enabled)".into(), facts, None)
            } else if !watching {
                (NodeState::Absent, "nothing is checking right now".into(), facts, None)
            } else if input.tasks_interrupted > 0 {
                (
                    NodeState::Active,
                    format!("{} to resume", plural(input.tasks_interrupted, "task", "tasks")),
                    facts,
                    None,
                )
            } else {
                (NodeState::Ready, format!("{} open", plural(input.tasks_open, "task", "tasks")), facts, None)
            }
        }
        "scheduler" => {
            let next = input.next_firing.clone().unwrap_or_else(|| "nothing scheduled".into());
            let facts = vec![fact("Active schedules", input.schedules), fact("Next", &next)];
            if !watching {
                (NodeState::Absent, "nothing fires right now".into(), facts, None)
            } else {
                (NodeState::Ready, plural(input.schedules, "schedule", "schedules"), facts, None)
            }
        }
        "dmn" => {
            let facts = vec![fact("Passes today", input.dmn_passes_today)];
            if !settings.dmn.enabled {
                (NodeState::Off, "turned off (dmn.enabled)".into(), facts, None)
            } else if of(&|p| p.front_end == "dmn").iter().any(|p| busy(p)) {
                (NodeState::Active, "a pass is running".into(), facts, None)
            } else {
                (NodeState::Ready, format!("{} today", plural(input.dmn_passes_today, "pass", "passes")), facts, None)
            }
        }
        "outbox" => {
            let mut channels: Vec<String> = settings
                .notify
                .channels
                .iter()
                .map(|channel| match channel {
                    nypi_core::config::NotifyChannel::Desktop => "desktop".to_string(),
                    nypi_core::config::NotifyChannel::Webhook { .. } => "webhook".to_string(),
                    nypi_core::config::NotifyChannel::Command { .. } => "command".to_string(),
                })
                .collect();
            if settings.notify.telegram && input.telegram_chats > 0 {
                channels.push("Telegram".into());
            }
            let via =
                if channels.is_empty() { "none: shown when you open NyPi".to_string() } else { channels.join(", ") };
            let facts = vec![fact("Channels", via), fact("Notices on file", input.notices)];
            match input.notices_open {
                0 => (NodeState::Ready, "everything reached you".into(), facts, None),
                open => (NodeState::Down, format!("{} not delivered", plural(open, "notice", "notices")), facts, None),
            }
        }
        "chat" => match &settings.chat.model {
            None => (NodeState::Off, "off: one agent does everything".into(), vec![], None),
            Some(model) => {
                let card = &input.planes.chat;
                let facts = vec![
                    fact("Turns today", card.turns_today),
                    fact("Lookups today", card.reads_today),
                    fact("Signals today", card.signals_today),
                ];
                (NodeState::Ready, model.clone(), facts, Some("/chat".into()))
            }
        },
        "principal" => match &settings.chat.model {
            None => (NodeState::Off, "off: one agent does everything".into(), vec![], None),
            Some(_) => {
                let card = &input.planes.principal;
                let facts = vec![
                    fact("Decisions today", card.decisions_today),
                    fact("Suspended", card.suspended.len()),
                    fact("Queued", card.queued.len()),
                ];
                match &card.active {
                    Some(active) => {
                        let topic: String = active.topic.chars().take(60).collect();
                        let status = format!("{}: {topic}", active.priority);
                        (NodeState::Active, status, facts, Some(format!("/activation/{}", active.id)))
                    }
                    None => {
                        let model = settings.principal.model.clone().unwrap_or_else(|| "the worker's model".into());
                        (NodeState::Ready, format!("idle; {model}"), facts, Some("/principal".into()))
                    }
                }
            }
        },
        "webhook" => {
            let webhook = &settings.channels.webhook;
            if webhook.enabled {
                (NodeState::Ready, format!("listening on {}", webhook.listen), vec![], None)
            } else {
                (NodeState::Off, "turned off (channels.webhook.enabled)".into(), vec![], None)
            }
        }
        "journal" => {
            let card = &input.planes.principal;
            let facts = vec![
                fact("Decisions today", card.decisions_today),
                fact("Chat turns today", input.planes.chat.turns_today),
            ];
            (NodeState::Ready, format!("{} decisions today", card.decisions_today), facts, Some("/principal".into()))
        }
        "worker" => {
            let (done, blocked, failed) = day.settled;
            let facts = vec![fact("Done today", done), fact("Blocked today", blocked), fact("Failed today", failed)];
            match running.first().and_then(|p| p.task.as_ref()) {
                Some(task) => {
                    let more = if running.len() > 1 { format!(" (+{})", running.len() - 1) } else { String::new() };
                    let goal: String = task.goal.chars().take(60).collect();
                    (NodeState::Active, format!("{goal}{more}"), facts, Some(format!("/task/{}", task.id)))
                }
                None => {
                    (NodeState::Ready, format!("idle; {done} done today"), facts, Some("/activity?kind=task".into()))
                }
            }
        }
        "thinking" => {
            if settings.thinking.enabled {
                (
                    NodeState::Ready,
                    format!("{} today", plural(module("thinking") as usize, "thought", "thoughts")),
                    vec![],
                    None,
                )
            } else {
                (NodeState::Off, "turned off (thinking.enabled)".into(), vec![], None)
            }
        }
        "valve" => {
            if settings.memory.valve.enabled {
                (NodeState::Ready, format!("{} today", plural(day.recalls as usize, "recall", "recalls")), vec![], None)
            } else {
                (NodeState::Off, "turned off (memory.valve.enabled)".into(), vec![], None)
            }
        }
        "prompt" => (NodeState::Ready, "rebuilt for every run".into(), vec![fact("Skills listed", input.skills)], None),
        "reflection" => match settings.reflection.mode {
            ReflectionMode::Off => (NodeState::Off, "turned off (reflection.mode)".into(), vec![], None),
            ReflectionMode::Auto => (NodeState::Ready, "after tasks that can teach".into(), vec![], None),
            ReflectionMode::Always => (NodeState::Ready, "after every task".into(), vec![], None),
        },
        "portrait" => match settings.portrait.mode {
            PortraitMode::Off => (NodeState::Off, "turned off (portrait.mode)".into(), vec![], None),
            _ => (NodeState::Ready, plural(input.traits, "trait", "traits"), vec![], None),
        },
        "goals" => (NodeState::Ready, format!("{} open", plural(input.goals_open, "goal", "goals")), vec![], None),
        "compaction" => {
            let compacting = all.iter().any(|p| matches!(p.step, Step::Compacting { .. }));
            let facts = vec![fact("Today", day.compactions)];
            if !settings.compaction.enabled {
                (NodeState::Off, "turned off (compaction.enabled)".into(), facts, None)
            } else if compacting {
                (NodeState::Active, "summarizing now".into(), facts, None)
            } else {
                (NodeState::Ready, format!("{} today", day.compactions), facts, None)
            }
        }
        "bus" => {
            let facts = day.by_module.iter().map(|(module, count)| fact(module, count)).collect();
            let state = if running.is_empty() { NodeState::Ready } else { NodeState::Active };
            (state, format!("{} today", plural(day.pulses as usize, "pulse", "pulses")), facts, None)
        }
        "inspector" => {
            let mode = serde_json::to_value(settings.approval.mode)
                .ok()
                .and_then(|mode| mode.as_str().map(str::to_string))
                .unwrap_or_default();
            let facts = vec![fact("Approvals", &mode), fact("Alerts today", day.alerts)];
            match input.approvals {
                0 => (
                    NodeState::Ready,
                    format!("approvals: {mode}; {} today", plural(day.alerts as usize, "alert", "alerts")),
                    facts,
                    Some("/activity?kind=alert".into()),
                ),
                asked => (
                    NodeState::Active,
                    format!("{} for you", plural(asked, "approval waits", "approvals wait")),
                    facts,
                    Some("/".into()),
                ),
            }
        }
        "models" => {
            let asking = all
                .iter()
                .any(|p| matches!(p.step, Step::Waiting { .. } | Step::Thinking { .. } | Step::Writing { .. }));
            let retrying = all.iter().any(|p| matches!(p.step, Step::Retrying { .. }));
            let default = settings.default_model.clone().unwrap_or_else(|| "no default model".into());
            let facts =
                vec![fact("Default", &default), fact("Replies today", day.replies), fact("Retries today", day.retries)];
            if retrying {
                (NodeState::Down, "retrying after a provider error".into(), facts, None)
            } else if asking {
                (NodeState::Active, "answering".into(), facts, None)
            } else {
                (NodeState::Ready, default, facts, None)
            }
        }
        "tools" => {
            let facts = vec![fact("Calls today", day.tool_calls), fact("Failed today", day.tool_failures)];
            let now = all.iter().find_map(|p| match &p.step {
                Step::Tool { name, summary, .. } => {
                    Some(format!("{name} {}", summary.chars().take(50).collect::<String>()))
                }
                _ => None,
            });
            match now {
                Some(call) => (NodeState::Active, call.trim().to_string(), facts, None),
                None => (
                    NodeState::Ready,
                    format!("{} today, {} failed", plural(day.tool_calls as usize, "call", "calls"), day.tool_failures),
                    facts,
                    None,
                ),
            }
        }
        "sandbox" => {
            if settings.sandbox.mode == SandboxMode::Off {
                (NodeState::Off, "turned off (sandbox)".into(), vec![], None)
            } else if input.sandbox_here {
                (NodeState::Ready, "the system's, around every tool program".into(), vec![], None)
            } else {
                (NodeState::Off, "none on this system: the guard's checks only".into(), vec![], None)
            }
        }
        "devices" => {
            let connected: Vec<&str> =
                input.devices.iter().filter(|(_, on)| *on).map(|(name, _)| name.as_str()).collect();
            let facts =
                input.devices.iter().map(|(name, on)| fact(name, if *on { "connected" } else { "away" })).collect();
            match (input.devices.len(), connected.len()) {
                (0, _) => (NodeState::Ready, "this computer only".into(), facts, None),
                (_, 0) => (NodeState::Ready, "this computer; the others are away".into(), facts, None),
                (_, _) => (NodeState::Active, format!("this computer and {}", connected.join(", ")), facts, None),
            }
        }
        "mcp" => {
            let names: Vec<&String> = settings.mcp_servers.keys().collect();
            if names.is_empty() {
                (NodeState::Off, "none configured".into(), vec![], None)
            } else {
                let facts = names.iter().map(|name| fact("Server", name)).collect();
                (NodeState::Ready, plural(names.len(), "server", "servers"), facts, None)
            }
        }
        "specialists" => {
            let facts: Vec<Fact> = input.specialists.iter().map(|name| fact("Specialist", name)).collect();
            let delegated = of(&|p| matches!(p.front_end.as_str(), "specialist" | "delegate"));
            if delegated.iter().any(|p| busy(p)) {
                (NodeState::Active, "one is working".into(), facts, None)
            } else if input.specialists.is_empty() {
                (NodeState::Off, "none yet: nypi specialists new".into(), facts, None)
            } else {
                (NodeState::Ready, input.specialists.join(", ").chars().take(60).collect(), facts, None)
            }
        }
        "skills" => match input.skills {
            0 => (NodeState::Off, "none installed".into(), vec![], None),
            count => (NodeState::Ready, plural(count, "skill", "skills"), vec![], None),
        },
        "sessions" => (NodeState::Ready, "one file per conversation".into(), vec![], None),
        "ledger" => (
            NodeState::Ready,
            format!("{}, {} open", plural(input.tasks, "task", "tasks"), input.tasks_open),
            vec![fact("Cut off", input.tasks_interrupted)],
            Some("/activity?kind=task".into()),
        ),
        "audit" => (
            NodeState::Ready,
            format!("{} in 24 h", plural(day.records as usize, "record", "records")),
            vec![],
            Some("/activity".into()),
        ),
        "memory" => (NodeState::Ready, plural(input.memories, "memory", "memories"), vec![], None),
        "vault" => (NodeState::Ready, plural(input.vault_notes, "note", "notes"), vec![], None),
        "secrets" => (
            NodeState::Ready,
            format!("{}, {}", plural(input.assets, "asset", "assets"), plural(input.secrets, "secret", "secrets")),
            vec![],
            None,
        ),
        "settings" => (NodeState::Ready, "read at every start".into(), vec![], None),
        _ => (NodeState::Ready, String::new(), vec![], None),
    };
    (state, status, facts, link)
}

/// Whether something is going over a connection at this moment.
fn flowing(from: &str, to: &str, input: &Inputs) -> bool {
    let any = |wanted: &dyn Fn(&Process) -> bool| input.processes.iter().any(wanted);
    let step = |wanted: &dyn Fn(&Step) -> bool| input.processes.iter().any(|p| wanted(&p.step));
    match (from, to) {
        ("worker", "models") => {
            step(&|s| matches!(s, Step::Waiting { .. } | Step::Thinking { .. } | Step::Writing { .. }))
        }
        ("worker", "tools") | ("tools", "sandbox") | ("inspector", "tools") => {
            step(&|s| matches!(s, Step::Tool { .. }))
        }
        ("worker", "compaction") => step(&|s| matches!(s, Step::Compacting { .. })),
        ("inspector", "owner") => input.approvals > 0,
        ("outbox", "owner") => input.notices_open > 0,
        ("terminal", "worker") => any(&|p| is_terminal(p) && busy(p)),
        ("programs", "worker") => any(&|p| matches!(p.front_end.as_str(), "rpc" | "print") && busy(p)),
        ("hub", "worker") => {
            input.daemon_alive
                && (input.daemon.as_ref().is_some_and(|beat| beat.busy) || any(&|p| is_hub(p) && busy(p)))
        }
        ("attach", "hub") | ("terminal", "hub") => {
            input.daemon_alive && input.daemon.as_ref().is_some_and(|beat| beat.clients > 0)
        }
        ("supervisor", "hub") => input.supervisor_alive && input.daemon_alive,
        ("worker", "specialists") => any(&|p| matches!(p.front_end.as_str(), "specialist" | "delegate") && busy(p)),
        ("dmn", "worker") => any(&|p| p.front_end == "dmn" && busy(p)),
        ("scheduler", "worker") => any(&|p| p.front_end == "timer" && busy(p)),
        ("worker", "bus")
        | ("bus", "inspector")
        | ("inspector", "audit")
        | ("worker", "ledger")
        | ("worker", "sessions") => any(&busy),
        _ => false,
    }
}

/// The map, with every module's state read from `input`.
pub fn build(input: &Inputs) -> Topology {
    let groups = GROUPS
        .iter()
        .map(|(id, title, note)| TopoGroup { id: id.to_string(), title: title.to_string(), note: note.to_string() })
        .collect();
    let nodes = MODULES
        .iter()
        .map(|(id, group, name, role, source)| {
            let (state, status, facts, link) = describe(id, input);
            TopoNode {
                id: id.to_string(),
                group: group.to_string(),
                name: name.to_string(),
                role: role.to_string(),
                source: source.to_string(),
                state,
                status,
                facts,
                link,
            }
        })
        .collect();
    let edges = LINKS
        .iter()
        .map(|(from, to, label, kind)| TopoEdge {
            from: from.to_string(),
            to: to.to_string(),
            label: label.to_string(),
            kind: *kind,
            active: flowing(from, to, input),
        })
        .collect();
    Topology { groups, nodes, edges }
}

/// Counts that mean reading whole stores, kept for a while: the map is asked for every few
/// seconds while someone looks at it.
#[derive(Clone, Default)]
pub struct Slow {
    memories: usize,
    traits: usize,
    goals_open: usize,
    specialists: Vec<String>,
    skills: usize,
}

const SLOW_KEPT: std::time::Duration = std::time::Duration::from_secs(30);

fn slow(board: &Board, home: &std::path::Path) -> Slow {
    let mut kept = board.slow.lock().unwrap();
    if let Some((at, slow)) = kept.as_ref()
        && at.elapsed() < SLOW_KEPT
    {
        return slow.clone();
    }
    let paths = &board.paths;
    let (specialists, _) = nypi_core::specialist::load_all(paths, home);
    let skill_dirs = nypi_core::skills::default_skill_dirs(&paths.skills(), None, &[]);
    let slow = Slow {
        memories: nypi_core::memory::MemoryStore::new(paths.memory()).load().len(),
        traits: nypi_core::portrait::Portrait::new(paths.portrait()).load().len(),
        goals_open: nypi_core::goals::GoalsFile::new(paths.goals()).goals().iter().filter(|goal| !goal.done).count(),
        specialists: specialists.into_iter().map(|specialist| specialist.name).collect(),
        skills: nypi_core::skills::load_skills(&skill_dirs).len(),
    };
    *kept = Some((std::time::Instant::now(), slow.clone()));
    slow
}

fn home_dir() -> std::path::PathBuf {
    dirs::home_dir().unwrap_or_default()
}

/// Read everything the map needs. Blocking: files and one socket probe.
pub fn gather(board: &Board, now: DateTime<Utc>) -> Inputs {
    let paths: &Paths = &board.paths;
    let home = home_dir();
    let settings = Settings::load(paths, &home).unwrap_or_default();
    let tasks = Ledger::new(paths.ledger()).load();
    let open: Vec<_> = tasks.values().filter(|task| !task.state.is_terminal()).collect();
    let schedules = ScheduleStore::new(paths.schedules()).active();
    let next_firing = super::records::upcoming(&schedules, now, 1).into_iter().next().map(|next| {
        let at = DateTime::parse_from_rfc3339(&next.at)
            .map(|t| t.with_timezone(&chrono::Local).format("%a %H:%M").to_string())
            .unwrap_or_default();
        format!("{at} {}", next.label).trim().to_string()
    });
    let notices = Outbox::new(paths.outbox()).load();
    let slow = slow(board, &home);
    let current = board.now.borrow().clone();
    let day = {
        let day = board.day.lock().unwrap();
        day_counts(day.records.iter())
    };
    Inputs {
        processes: board.hub.processes(),
        waiting: current.inbox.len(),
        approvals: board.hub.approvals().len(),
        supervisor_installed: crate::supervisor::installed(),
        supervisor_alive: crate::supervisor::alive(paths, now),
        daemon: crate::daemon::heartbeat(paths),
        daemon_alive: crate::daemon::alive(paths, now),
        telegram_chats: nypi_hub::channels::telegram::api::Access::load(&paths.telegram_access())
            .allowed_chat_ids
            .len(),
        tasks: tasks.len(),
        tasks_open: open.len(),
        tasks_interrupted: open.iter().filter(|task| task.is_interrupted()).count(),
        schedules: schedules.len(),
        next_firing,
        notices: notices.len(),
        notices_open: notices.iter().filter(|notice| notice.is_open()).count(),
        memories: slow.memories,
        traits: slow.traits,
        goals_open: slow.goals_open,
        specialists: slow.specialists,
        skills: slow.skills,
        planes: current.planes.clone(),
        dmn_passes_today: nypi_core::dmn::PassLog::new(paths.dmn_passes()).today(now).0,
        sandbox_here: cfg!(target_os = "macos")
            || (cfg!(target_os = "linux") && std::path::Path::new("/usr/bin/bwrap").exists()),
        vault_notes: nypi_vault::vault::Vault::open(&paths.vault()).map(|vault| vault.notes().len()).unwrap_or(0),
        assets: nypi_vault::assets::AssetRegistry::new(paths.assets()).list().len(),
        // Counted from the store's own file: its entries' names are not secret, and no key is needed.
        secrets: std::fs::read_to_string(paths.secrets())
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|file| file["entries"].as_object().map(|entries| entries.len()))
            .unwrap_or(0),
        devices: nypi_hub::nodes::read_register(paths)
            .into_iter()
            .map(|(name, known)| (name, known.connected && crate::daemon::alive(paths, now)))
            .collect(),
        settings,
        day,
    }
}

/// The module ids and sources, for the test that holds the map to the tree.
#[cfg(test)]
pub fn modules() -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    MODULES.iter().map(|(id, group, _, _, source)| (*id, *group, *source))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use nypi_board_proto::TaskRef;
    use serde_json::json;

    use super::*;

    fn node<'a>(map: &'a Topology, id: &str) -> &'a TopoNode {
        map.nodes.iter().find(|node| node.id == id).unwrap_or_else(|| panic!("no module {id}"))
    }

    fn edge<'a>(map: &'a Topology, from: &str, to: &str) -> &'a TopoEdge {
        map.edges.iter().find(|edge| edge.from == from && edge.to == to).unwrap_or_else(|| panic!("no {from} -> {to}"))
    }

    fn process(front_end: &str, step: Step, goal: Option<&str>) -> Process {
        Process {
            id: format!("p-{front_end}"),
            pid: 7,
            front_end: front_end.into(),
            agent: "lucia".into(),
            model: "faux/faux-1".into(),
            cwd: "/".into(),
            session_id: "s".into(),
            connected_at: "t".into(),
            task: goal.map(|goal| TaskRef { id: "t-1".into(), goal: goal.into() }),
            step,
            context_tokens: 0,
            context_window: 0,
            run_cost: 0.0,
            run_started: goal.map(|_| "t".into()),
            dropped: 0,
        }
    }

    #[test]
    fn the_map_matches_the_source_tree_and_hangs_together() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let groups: BTreeSet<&str> = GROUPS.iter().map(|(id, _, _)| *id).collect();
        let mut ids = BTreeSet::new();
        for (id, group, source) in modules() {
            assert!(ids.insert(id), "{id} is on the map twice");
            assert!(groups.contains(group), "{id} is in a group that does not exist: {group}");
            assert!(root.join(source).exists(), "{id} points at {source}, which is not in the tree");
        }
        let mut connected = BTreeSet::new();
        let mut seen = BTreeSet::new();
        for (from, to, label, _) in LINKS {
            assert!(ids.contains(from) && ids.contains(to), "{from} -> {to} names a module that is not on the map");
            assert!(seen.insert((from, to)), "{from} -> {to} is drawn twice");
            assert!(!label.is_empty() && from != to);
            connected.extend([from, to]);
        }
        assert_eq!(connected, ids, "every module is connected to something");

        // Every core source file is either a module on the map or part of one, listed here on
        // purpose, so a new module is not forgotten.
        let parts_of_others = [
            "lib.rs",
            "jsonl.rs",
            "html.rs",
            "export_html.rs",
            "trace.rs",
            "citations.rs",
            "retry.rs",
            "status.rs",
            "oneshot.rs",
            "worker.rs",
            // The tasks of one conversation, several at a time: part of the Worker.
            "task_pool.rs",
            "introspect.rs",
            // Goals' project plans and the two clocks of idle time: part of Goals (D85).
            "campaign.rs",
            // A settled task's report and the manifest the harness draws: part of the Principal (D87).
            "evidence.rs",
            // What an activation is handed before its first model call: part of the Principal (D88).
            "briefing.rs",
            "delegate.rs",
            "board_link.rs",
            "protected.rs",
            "trust.rs",
            "scorecard.rs",
            "active_tasks.rs",
            // Where the device tools run, and this computer as a node: parts of Devices.
            "place.rs",
            "device.rs",
            // Releases and their signatures: part of the supervisor, which applies updates (D98).
            "update.rs",
        ];
        let mapped: BTreeSet<String> = modules().map(|(_, _, source)| source.to_string()).collect();
        for entry in std::fs::read_dir(root.join("crates/nypi-core/src")).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if !name.ends_with(".rs") || parts_of_others.contains(&name.as_str()) {
                continue;
            }
            assert!(
                mapped.contains(&format!("crates/nypi-core/src/{name}")),
                "crates/nypi-core/src/{name} is neither on the board's map nor listed as part of another module"
            );
        }
    }

    #[test]
    fn a_conversation_named_by_an_older_hub_is_still_the_hubs() {
        let older = process("daemon", Step::Idle, Some("a task"));
        assert!(is_hub(&older) && is_hub(&process("hub", Step::Idle, None)));
        assert!(!is_hub(&process("cli", Step::Idle, None)));
        assert!(!is_terminal(&older), "an older hub's conversation is not a terminal");
    }

    #[test]
    fn a_quiet_machine_shows_what_is_off_and_what_is_missing() {
        let mut settings = Settings::default();
        settings.notify.telegram = false;
        let quiet = build(&Inputs { settings, sandbox_here: true, ..Default::default() });
        assert_eq!((quiet.groups.len(), quiet.nodes.len(), quiet.edges.len()), (5, 40, 66));
        let state = |id: &str| (node(&quiet, id).state, node(&quiet, id).status.as_str());
        assert_eq!(state("supervisor"), (NodeState::Off, "not installed: nypi hub up"));
        assert_eq!(state("hub"), (NodeState::Absent, "not running: nothing keeps it up"));
        assert_eq!(state("devices"), (NodeState::Ready, "this computer only"));
        assert_eq!(state("watchdog"), (NodeState::Absent, "nothing is checking right now"));
        assert_eq!(state("terminal"), (NodeState::Absent, "not open"));
        assert_eq!(state("telegram"), (NodeState::Off, "not set up"));
        assert_eq!(state("chat"), (NodeState::Off, "off: one agent does everything"));
        assert_eq!(state("principal"), (NodeState::Off, "off: one agent does everything"));
        assert_eq!(state("dmn"), (NodeState::Off, "turned off (dmn.enabled)"));
        assert_eq!(state("worker"), (NodeState::Ready, "idle; 0 done today"));
        assert_eq!(state("sandbox").0, NodeState::Ready);
        assert_eq!(state("owner"), (NodeState::Ready, "nothing waits for you"));
        assert!(quiet.edges.iter().all(|edge| !edge.active), "nothing moves on a quiet machine");
        assert_eq!(node(&quiet, "worker").source, "crates/nypi-core/src/runtime/mod.rs");
    }

    #[test]
    fn a_working_machine_lights_the_path_the_work_takes() {
        let beat = crate::daemon::Heartbeat {
            pid: 42,
            started: "t".into(),
            beat: "t".into(),
            clients: 1,
            busy: true,
            cwd: "/Users/x".into(),
            channels: Vec::new(),
            threads: 1,
        };
        let tool = Step::Tool { since: "t".into(), name: "bash".into(), summary: "cargo test".into() };
        let mut settings = Settings::default();
        settings.chat.model = Some("gateway/gemini-3-flash".into());
        let input = Inputs {
            settings,
            processes: vec![
                process("hub", tool, Some("run the tests and fix what fails")),
                process("cli", Step::Idle, None),
            ],
            waiting: 2,
            approvals: 1,
            supervisor_installed: true,
            supervisor_alive: true,
            daemon: Some(beat),
            daemon_alive: true,
            tasks: 12,
            tasks_open: 2,
            tasks_interrupted: 1,
            notices: 4,
            notices_open: 1,
            specialists: vec!["codex".into(), "analyst".into()],
            sandbox_here: true,
            day: day_counts(
                [
                    json!({ "source": "pulse", "payload": { "source": { "type": "module", "module": "runtime" },
                            "payload": { "kind": "toolFinished", "isError": true } } }),
                    json!({ "source": "pulse", "payload": { "source": { "type": "module", "module": "valve" },
                            "payload": { "kind": "memoryValve" } } }),
                    json!({ "source": "pulse", "payload": { "source": { "type": "module", "module": "runtime" },
                            "payload": { "kind": "runSettled", "status": { "kind": "done" } } } }),
                    json!({ "source": "agent", "payload": { "type": "message_end", "message": { "role": "assistant" } } }),
                ]
                .iter(),
            ),
            ..Default::default()
        };
        assert_eq!((input.day.pulses, input.day.tool_calls, input.day.tool_failures, input.day.replies), (3, 1, 1, 1));
        let map = build(&input);
        let state = |id: &str| (node(&map, id).state, node(&map, id).status.as_str());
        assert_eq!(state("worker"), (NodeState::Active, "run the tests and fix what fails"));
        assert_eq!(node(&map, "worker").link.as_deref(), Some("/task/t-1"));
        assert_eq!(state("tools"), (NodeState::Active, "bash cargo test"));
        assert_eq!(state("hub"), (NodeState::Active, "on a task"));
        assert_eq!(state("attach"), (NodeState::Active, "1 attached"));
        assert_eq!(state("terminal"), (NodeState::Ready, "1 open, idle"));
        assert_eq!(state("supervisor"), (NodeState::Ready, "doing its rounds"));
        assert_eq!(state("watchdog"), (NodeState::Active, "1 task to resume"));
        assert_eq!(state("outbox"), (NodeState::Down, "1 notice not delivered"));
        assert_eq!(state("inspector"), (NodeState::Active, "1 approval waits for you"));
        assert_eq!(state("owner"), (NodeState::Active, "2 things wait for you"));
        assert_eq!(state("chat"), (NodeState::Ready, "gateway/gemini-3-flash"));
        assert_eq!(state("specialists"), (NodeState::Ready, "codex, analyst"));
        assert_eq!(state("valve"), (NodeState::Ready, "1 recall today"));
        assert_eq!(state("models").0, NodeState::Ready, "a tool is running, the model is not being asked");
        assert_eq!(state("telegram"), (NodeState::Off, "not set up"));

        // The same hub with Telegram inside it and one chat paired.
        let mut with_channel = input;
        with_channel.daemon.as_mut().unwrap().channels = vec!["telegram".into()];
        with_channel.telegram_chats = 1;
        let map = build(&with_channel);
        assert_eq!(node(&map, "telegram").status, "in the hub; 1 chat paired");
        assert_eq!(edge(&map, "telegram", "hub").label, "a channel inside it");

        let active: BTreeSet<(&str, &str)> =
            map.edges.iter().filter(|edge| edge.active).map(|edge| (edge.from.as_str(), edge.to.as_str())).collect();
        for path in [
            ("attach", "hub"),
            ("hub", "worker"),
            ("worker", "tools"),
            ("tools", "sandbox"),
            ("inspector", "owner"),
            ("outbox", "owner"),
            ("supervisor", "hub"),
        ] {
            assert!(active.contains(&path), "{path:?} should be lit: {active:?}");
        }
        assert!(!edge(&map, "worker", "models").active && !edge(&map, "terminal", "worker").active);
        assert_eq!(edge(&map, "supervisor", "hub").kind, EdgeKind::Watch);
    }
}
