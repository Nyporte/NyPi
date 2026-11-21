//! Recording an idle pass (the DMN, nypi-core/src/dmn.rs): what it cost, and what it has to
//! say. A pass that is due runs as a quiet job of the hub (jobs.rs); `nypi dmn run` runs one
//! by hand. Either way it is recorded here, and its note, if any, goes to the owner. In the
//! daemon with chat in front, the pass is the Principal's idle activation instead (decision
//! D85), recorded from its decision when it finishes, and what it has to say goes through chat.

use chrono::Utc;
use nypi_core::config::Paths;
use nypi_core::dmn::{Outcome, Pass, PassLog, Why, read_answer};
use nypi_core::inspector::AlertLevel;
use nypi_core::ledger::TaskId;
use nypi_core::runtime::Runtime;
use serde_json::Value;

use crate::notify::{Notice, Notifier};

/// Run the pass on `dmn.model`, when one is set. Returns a warning when it cannot be.
pub fn use_own_model(runtime: &mut Runtime) -> Option<String> {
    let model = runtime.settings.dmn.model.clone()?;
    let error = runtime.set_model(&model).err()?;
    Some(format!("warning: dmn.model: {error:#}; using {}", runtime.model().reference()))
}

/// How a pass's run went, as far as recording it needs.
pub struct Ran {
    pub error: Option<String>,
    pub answer: String,
    pub tokens: u64,
    pub cost: f64,
    pub task: Option<TaskId>,
}

impl Ran {
    /// From the `agent_settled` record of a pass that ran as a job of the hub. None: it never settled.
    pub fn of_record(record: Option<&Value>) -> Ran {
        let Some(record) = record else {
            return Ran {
                error: Some("the pass ended without a result".into()),
                answer: String::new(),
                tokens: 0,
                cost: 0.0,
                task: None,
            };
        };
        Ran {
            error: record["error"].as_str().map(str::to_string),
            answer: record["answer"].as_str().unwrap_or_default().to_string(),
            tokens: record["usage"]["tokens"].as_u64().unwrap_or(0),
            cost: record["usage"]["cost"].as_f64().unwrap_or(0.0),
            task: record["task"].as_str().map(|id| TaskId(id.to_string())),
        }
    }
}

/// Record a pass the Principal ran as an idle activation (decision D85), from its decision.
/// Nothing is said here: what the owner should hear, the Principal had chat tell them.
pub fn record_decision(paths: &Paths, why: Why, decision: &nypi_core::principal::Decision) -> std::io::Result<()> {
    let ran = decision.seq > 0;
    let note = ran.then(|| read_answer(&decision.summary)).flatten();
    let pass = Pass {
        at: Utc::now().to_rfc3339(),
        why,
        outcome: match (ran, &note) {
            (false, _) => Outcome::Failed,
            (true, Some(_)) => Outcome::Note,
            (true, None) => Outcome::Nothing,
        },
        tokens: decision.tokens,
        cost: decision.cost,
        task: decision.tasks_touched.first().cloned(),
        note,
    };
    PassLog::new(paths.dmn_passes()).append(&pass)
}

/// Record a pass with what it cost, and say what it has to say: its note, or that it failed
/// (nobody watches a pass, so a failed one is said too).
pub async fn record(paths: &Paths, agent: &str, notifier: Option<&Notifier>, why: Why, ran: Ran) -> anyhow::Result<()> {
    let note = if ran.error.is_some() { None } else { read_answer(&ran.answer) };
    let pass = Pass {
        at: Utc::now().to_rfc3339(),
        why,
        outcome: match (&ran.error, &note) {
            (Some(_), _) => Outcome::Failed,
            (None, Some(_)) => Outcome::Note,
            (None, None) => Outcome::Nothing,
        },
        tokens: ran.tokens,
        cost: ran.cost,
        task: ran.task.as_ref().map(|t| t.0.clone()),
        note: note.clone(),
    };
    PassLog::new(paths.dmn_passes()).append(&pass)?;
    let said = match (&ran.error, &note) {
        (Some(error), _) => Some((AlertLevel::Error, format!("Failed: {agent}'s idle pass could not run: {error}"))),
        (None, Some(note)) => Some((AlertLevel::Notice, format!("{agent} (while idle): {note}"))),
        (None, None) => None,
    };
    if let Some((level, text)) = said {
        let notice = Notice { source: "dmn", level, text: &text, task: ran.task.as_ref(), shown_in: None };
        crate::notify::post(paths, notifier, notice).await;
    }
    Ok(())
}
