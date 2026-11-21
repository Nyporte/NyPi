//! The Thinking module (decisions D78, D84, D91): the observer of a working run. A strong
//! reasoning model looks at a run's course every few turns and, when it has gone wrong, steers
//! it and publishes a `thought` pulse. `thinking.enabled` turns it on.

use std::collections::VecDeque;
use std::sync::Arc;

use nypi_ai::{Model, StreamFn, ThinkingLevel};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::inspector::Inspector;
use crate::pulse::{PulseBus, PulseKind, PulsePayload, PulseSource};

/// How much of a task's goal the observer quotes.
const TURN_CHARS: usize = 1500;

/// An older `observeWorker` key in a settings file is ignored: `enabled` is the one switch.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThinkingSettings {
    /// Watch working runs and steer one when it goes wrong; one reasoning call per
    /// `workerCadenceTurns` turns of a run.
    pub enabled: bool,
    /// Model as `provider/id`; defaults to the worker's (the strong one).
    pub model: Option<String>,
    /// How hard it reasons: off, minimal, low, medium, high, xhigh, max.
    pub level: String,
    /// Turns of a run between two looks at its course.
    pub worker_cadence_turns: usize,
}

impl Default for ThinkingSettings {
    fn default() -> Self {
        ThinkingSettings { enabled: false, model: None, level: "high".into(), worker_cadence_turns: 5 }
    }
}

/// What the observer published when it steered a run: its reading of the course, and what
/// it told the working agent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Thought {
    pub approach: String,
    pub say: String,
}

fn clip(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > TURN_CHARS {
        format!("{}…", text.chars().take(TURN_CHARS).collect::<String>())
    } else {
        text
    }
}

/// What a look at a working run's course came to (decision D78).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Observation {
    pub assessment: String,
    /// What the working agent should hear; empty when its course is sound, which is the rule.
    pub steer: String,
}

/// The task a run is on, as the observer reads it at the moment of a look.
#[derive(Clone, Debug, Default)]
pub struct ObservedTask {
    pub goal: String,
    /// The task's contract and working notes, when it has a workspace in the vault.
    pub brief: Option<String>,
    pub findings: Option<String>,
}

pub struct ObserverOptions {
    pub settings: ThinkingSettings,
    pub model: Model,
    pub stream_fn: StreamFn,
    pub api_key: Option<String>,
    pub retry: nypi_ai::retry::RetryPolicy,
    pub inspector: Arc<Inspector>,
    /// The run's agent: a steer goes into its queue and is read after its current step.
    pub agent: nypi_agent::AgentHandle,
    pub task: Arc<dyn Fn() -> ObservedTask + Send + Sync>,
}

/// Tool calls of a run the observer keeps in view.
const DIGEST_LINES: usize = 40;

const OBSERVER_SYSTEM: &str = "You are the reasoning module that watches a working agent's course from outside. You \
see its task, its notes, and a digest of its recent tool calls. You do not do its work, and you do not second-guess \
sound work. Reply with JSON only.";

const OBSERVER_INSTRUCTIONS: &str = "Judge where this run is heading:\n\
- assessment: one or two sentences on what the agent is doing and whether that serves the task\n\
- steer: what the agent should hear, in one to three sentences at the level of approach (not a command to run). \
  Leave it empty whenever the agent is making sensible progress or legitimately probing a hypothesis: most of the \
  time it is, and an unneeded note costs it attention. Write one only when the digest shows the agent repeating an \
  approach that already failed, trusting a document over the source, schema, or raw data it could inspect, \
  overlooking a tool, specialist, or note of its own that would settle the question, about to stop at an obstacle \
  it could go around, or drifting from its brief.\n\
Reply with this JSON and nothing else:\n\
{\"assessment\": \"...\", \"steer\": \"\"}";

/// The observer's question for one look.
pub fn observer_prompt(task: &ObservedTask, digest: &[String], compacted: bool) -> String {
    let mut prompt = format!("{OBSERVER_INSTRUCTIONS}\n\nThe task:\n{}", clip(&task.goal));
    if let Some(brief) = task.brief.as_deref().filter(|b| !b.trim().is_empty()) {
        prompt.push_str(&format!("\n\nIts brief (brief.md):\n{}", brief.trim()));
    }
    match task.findings.as_deref().filter(|f| !f.trim().is_empty()) {
        Some(findings) => {
            prompt.push_str(&format!("\n\nThe agent's own notes so far (findings.md):\n{}", findings.trim()))
        }
        None => prompt.push_str("\n\nThe agent has written no notes yet."),
    }
    if compacted {
        prompt.push_str(
            "\n\nThe agent's context was just compacted: it now remembers its earlier steps only through a summary \
             and its notes. Check that it is not about to retry something the digest or the notes show already failed.",
        );
    }
    prompt.push_str(&format!("\n\nIts recent tool calls, oldest first:\n{}", digest.join("\n")));
    prompt
}

pub fn parse_observation(reply: &str) -> anyhow::Result<Observation> {
    let object = crate::oneshot::json_object(reply).ok_or_else(|| anyhow::anyhow!("no JSON object in the reply"))?;
    let mut observation: Observation = serde_json::from_str(object)?;
    observation.steer = observation.steer.trim().to_string();
    Ok(observation)
}

/// Watch the run on `bus` (decision D78): every `workerCadenceTurns` turns, and at once after
/// a compaction, take one tool-free look at the task, its notes, and what its recent calls came
/// to. Silence is the rule; when the look finds the run in a rut, the steer is published as a
/// `thought` pulse and put into the agent's queue as a metacognitive note. No counter of errors
/// decides this and no rule about tools: the judgment is the model's, over the whole course. A
/// look still under way when the run settles is cancelled. Returns when the bus closes.
pub fn spawn_observer(bus: &PulseBus, options: ObserverOptions) -> tokio::task::JoinHandle<()> {
    let mut heard = bus.subscribe("observer", &[PulseKind::TurnCompleted, PulseKind::RunSettled]);
    let bus = bus.clone();
    let level = ThinkingLevel::parse(&options.settings.level).unwrap_or(ThinkingLevel::High);
    let cadence = options.settings.worker_cadence_turns.max(1);
    tokio::spawn(async move {
        let mut digest: VecDeque<String> = VecDeque::new();
        let mut since = 0usize;
        let mut current: Option<(CancellationToken, tokio::task::JoinHandle<()>)> = None;
        while let Some(pulse) = heard.recv().await {
            let (compacted, turn) = match &pulse.payload {
                PulsePayload::TurnCompleted { recent_calls_digest, compacted, turn_index, .. } => {
                    digest.extend(recent_calls_digest.iter().cloned());
                    while digest.len() > DIGEST_LINES {
                        digest.pop_front();
                    }
                    (*compacted, *turn_index)
                }
                // The run is over: nothing is left to steer, and the next run starts afresh.
                PulsePayload::RunSettled { .. } => {
                    if let Some((cancel, _)) = current.take() {
                        cancel.cancel();
                    }
                    digest.clear();
                    since = 0;
                    continue;
                }
                _ => continue,
            };
            since += 1;
            let looking = current.as_ref().is_some_and(|(_, look)| !look.is_finished());
            if looking || digest.is_empty() || (since < cadence && !compacted) {
                continue;
            }
            since = 0;
            let cancel = CancellationToken::new();
            let lines: Vec<String> = digest.iter().cloned().collect();
            let prompt = observer_prompt(&(options.task)(), &lines, compacted);
            let (bus, inspector, model, stream_fn, api_key, retry, token, cause, agent) = (
                bus.clone(),
                options.inspector.clone(),
                options.model.clone(),
                options.stream_fn.clone(),
                options.api_key.clone(),
                options.retry.clone(),
                cancel.clone(),
                pulse.id.clone(),
                options.agent.clone(),
            );
            let look = tokio::spawn(async move {
                let reply = crate::oneshot::complete(
                    &model,
                    &stream_fn,
                    api_key,
                    OBSERVER_SYSTEM,
                    prompt,
                    level,
                    &retry,
                    token.clone(),
                )
                .await;
                if token.is_cancelled() {
                    return;
                }
                match reply.and_then(|r| parse_observation(&r)) {
                    Ok(observation) => {
                        let steered = !observation.steer.is_empty();
                        inspector.record(
                            "thinking",
                            json!({ "observed": { "turn": turn, "cause": cause, "assessment": observation.assessment, "steered": steered } }),
                        );
                        if steered {
                            let thought = Thought { approach: observation.assessment, say: observation.steer.clone() };
                            bus.publish(
                                PulseSource::module("thinking"),
                                Some(cause),
                                PulsePayload::Thought { thought },
                            );
                            agent.steer(nypi_ai::Message::user_text(format!(
                                "[Metacognitive note]: {} (From your own reasoning module, which watches the course of \
                                 this task. It is not the user speaking.)",
                                observation.steer
                            )));
                        }
                    }
                    Err(error) => inspector
                        .record("thinking", json!({ "observed": { "cause": cause, "error": format!("{error:#}") } })),
                }
            });
            current = Some((cancel, look));
        }
        if let Some((cancel, _)) = current {
            cancel.cancel();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A settings file written before D91 still loads: `enabled` alone decides.
    #[test]
    fn an_older_observe_worker_key_is_ignored() {
        let settings: ThinkingSettings =
            serde_json::from_str(r#"{ "enabled": true, "observeWorker": false }"#).unwrap();
        assert!(settings.enabled);
    }
}
