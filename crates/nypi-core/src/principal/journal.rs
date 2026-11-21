//! What the Principal writes down (decision D84): the decision journal (`stores/decisions.jsonl`),
//! each activation's story (`principal/activations/<id>.jsonl`) and its index
//! (`stores/activations.jsonl`), and the status file the board reads (`principal/status.json`),
//! all through the secret scrubber.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Paths;

use super::*;

/// One entry of the decision journal: how an activation ended.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Decision {
    pub seq: u64,
    pub activation_id: String,
    pub at: String,
    pub priority: String,
    pub trigger_kind: String,
    pub topic: String,
    pub summary: String,
    pub tasks_touched: Vec<String>,
    pub goals_touched: Vec<String>,
    pub stimulated_chat: bool,
    /// How it was disposed of (decision D87): what it did (`stimulated_chat`, `tasks_touched`,
    /// `stores_changed`), or the disposition it concluded with and why.
    pub disposition: String,
    pub tokens: u64,
    pub cost: f64,
}

/// `stores/decisions.jsonl`: what the Principal decided, in order. Read through a fold shared
/// by the process, so a read takes in only what was appended since the last.
#[derive(Clone)]
pub struct DecisionJournal {
    pub(super) path: std::path::PathBuf,
    pub(super) write: Arc<Mutex<()>>,
}

impl DecisionJournal {
    pub fn new(paths: &Paths) -> Self {
        DecisionJournal { path: paths.decisions(), write: Arc::default() }
    }

    pub fn load(&self) -> Vec<Decision> {
        crate::jsonl::Folded::<Vec<Decision>>::shared(&self.path).read(
            |decisions, line, _| {
                if let Ok(decision) = serde_json::from_str(line) {
                    decisions.push(decision);
                }
            },
            Vec::clone,
        )
    }

    pub fn last_seq(&self) -> u64 {
        self.load().last().map_or(0, |decision| decision.seq)
    }

    /// The newest `n`, oldest first.
    pub fn recent(&self, n: usize) -> Vec<Decision> {
        let all = self.load();
        all[all.len().saturating_sub(n)..].to_vec()
    }

    /// Those after `seq`.
    pub fn after(&self, seq: u64) -> Vec<Decision> {
        self.load().into_iter().filter(|decision| decision.seq > seq).collect()
    }

    /// Append `decision` under the next number; returns it.
    pub fn append(&self, mut decision: Decision) -> std::io::Result<u64> {
        let _held = self.write.lock().unwrap();
        decision.seq = self.last_seq() + 1;
        crate::jsonl::append(&self.path, &decision)?;
        Ok(decision.seq)
    }
}

/// One step of an activation's story, as its log keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ActivationEvent {
    Created {
        at: String,
        priority: Priority,
        topic: String,
        trigger: Box<Trigger>,
    },
    Started {
        at: String,
    },
    AssistantTurn {
        at: String,
        thinking: String,
        text: String,
        tool_calls: Vec<Value>,
        stop_reason: String,
    },
    ToolResult {
        at: String,
        tool: String,
        call_id: String,
        arguments: Value,
        output: String,
        duration_ms: u64,
        is_error: bool,
    },
    /// A further signal of the user's joined this activation.
    Joined {
        at: String,
        activation: String,
        topic: String,
    },
    Suspended {
        at: String,
        by_activation: String,
        step_count: usize,
        at_decision_seq: u64,
    },
    Resumed {
        at: String,
        delta_summary: String,
    },
    StimulatedChat {
        at: String,
        stimulus: Stimulus,
    },
    /// It stopped having disposed of nothing, and the harness asked it to (decision D87).
    Nudged {
        at: String,
        text: String,
    },
    /// What was looked up for it before its first model call (decision D88).
    Briefed {
        at: String,
        pack: crate::briefing::BriefingPack,
    },
    Finished {
        at: String,
        summary: String,
        tokens: u64,
        cost: f64,
    },
}

/// The story of activation `id`, as its log holds it.
pub fn read_activation(paths: &Paths, id: &str) -> Vec<ActivationEvent> {
    std::fs::read_to_string(paths.activation_log(id))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// One line of `stores/activations.jsonl`: an activation's life in brief.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ActivationLine {
    pub id: String,
    pub at: String,
    /// `created`, `started`, `suspended`, `resumed`, `joined`, `finished`.
    pub state: String,
    pub priority: Option<Priority>,
    pub kind: String,
    pub topic: String,
    pub summary: String,
}

/// Every line of the index, oldest first.
pub fn activation_lines(paths: &Paths) -> Vec<ActivationLine> {
    std::fs::read_to_string(paths.activations_index())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Writes what an activation does, scrubbed of secrets.
#[derive(Clone)]
pub(super) struct Logs {
    pub(super) paths: Paths,
    pub(super) scrubber: Arc<nypi_vault::secrets::LiveScrubber>,
}

impl Logs {
    pub(super) fn event(&self, id: &str, event: &ActivationEvent) {
        let line = serde_json::to_string(event).unwrap_or_default();
        let clean: Value = serde_json::from_str(&self.scrubber.scrub(&line)).unwrap_or(Value::Null);
        let _ = crate::jsonl::append(&self.paths.activation_log(id), &clean);
    }

    pub(super) fn line(&self, line: ActivationLine) {
        let text = serde_json::to_string(&line).unwrap_or_default();
        let clean: Value = serde_json::from_str(&self.scrubber.scrub(&text)).unwrap_or(Value::Null);
        let _ = crate::jsonl::append(&self.paths.activations_index(), &clean);
    }

    /// Write what the Principal is doing, whole, so a reader never sees half of it.
    pub(super) fn status(&self, status: &PrincipalStatus) {
        let shown = SavedStatus { pid: std::process::id(), at: now(), status: status.clone() };
        let text = self.scrubber.scrub(&serde_json::to_string(&shown).unwrap_or_default());
        let path = self.paths.principal_status();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let partial = path.with_extension("json.part");
        if std::fs::write(&partial, text).is_ok() {
            let _ = std::fs::rename(&partial, &path);
        }
    }
}

/// `principal/status.json`: the Principal's status as its process last wrote it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SavedStatus {
    pub pid: u32,
    pub at: String,
    pub status: PrincipalStatus,
}

/// What the Principal was doing as its process last said, or None when that process is gone
/// (its activations went with it) or never wrote.
pub fn saved_status(paths: &Paths) -> Option<SavedStatus> {
    let saved: SavedStatus = serde_json::from_str(&std::fs::read_to_string(paths.principal_status()).ok()?).ok()?;
    crate::ledger::process_alive(saved.pid).then_some(saved)
}
