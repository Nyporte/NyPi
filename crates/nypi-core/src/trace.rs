//! Rebuild the causal chain behind a pulse from the audit log (principle A4): the pulses that led
//! to it, the pulses it caused, and every audit record written while it was the active cause.

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::audit_log::load;

pub struct Trace {
    /// Root first, ending with the pulse itself.
    pub chain: Vec<Value>,
    /// Pulses whose causation is this pulse.
    pub children: Vec<Value>,
    /// Non-pulse records (agent events, approvals, harness notes) written with this pulse as cause.
    pub effects: Vec<Value>,
}

fn pulse_id(record: &Value) -> Option<&str> {
    (record["source"] == "pulse").then(|| record["payload"]["id"].as_str()).flatten()
}

/// The most recent pulse records, newest first.
pub fn recent(audit_dir: &Path, days: usize, limit: usize) -> Vec<Value> {
    let mut pulses: Vec<Value> = load(audit_dir, days).into_iter().filter(|r| pulse_id(r).is_some()).collect();
    pulses.reverse();
    pulses.truncate(limit);
    pulses
}

/// None when no pulse with this ID is in the searched files.
pub fn find(audit_dir: &Path, id: &str, days: usize) -> Option<Trace> {
    let records = load(audit_dir, days);
    let pulses: HashMap<&str, &Value> = records.iter().filter_map(|r| Some((pulse_id(r)?, r))).collect();
    let target = *pulses.get(id)?;

    let mut chain = vec![target.clone()];
    let mut current = target;
    // The bound guards against a corrupted log that links pulses in a cycle.
    while chain.len() <= pulses.len() {
        let Some(parent) = current["payload"]["causation"].as_str().and_then(|p| pulses.get(p)) else { break };
        chain.push((*parent).clone());
        current = parent;
    }
    chain.reverse();

    let children =
        records.iter().filter(|r| pulse_id(r).is_some() && r["payload"]["causation"] == id).cloned().collect();
    let effects = records.iter().filter(|r| pulse_id(r).is_none() && r["cause"] == id).cloned().collect();
    Some(Trace { chain, children, effects })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pulse(id: &str, causation: Option<&str>, kind: &str) -> Value {
        json!({ "source": "pulse", "payload": { "id": id, "causation": causation, "payload": { "kind": kind } } })
    }

    #[test]
    fn follows_causation_across_days() {
        let dir = tempfile::tempdir().unwrap();
        let day1 = [pulse("a-1", None, "userMessage"), json!({ "source": "agent", "cause": "a-1", "payload": {} })];
        let day2 = [
            pulse("a-2", Some("a-1"), "toolStarted"),
            pulse("a-3", Some("a-2"), "toolFinished"),
            pulse("a-4", Some("a-1"), "runSettled"),
            json!({ "source": "approval", "cause": "a-1", "payload": {} }),
        ];
        let write = |name: &str, records: &[Value]| {
            let text: String = records.iter().map(|r| format!("{r}\n")).collect();
            std::fs::write(dir.path().join(name), text + "{\"partial").unwrap();
        };
        write("2026-10-31.jsonl", &day1);
        write("2026-11-01.jsonl", &day2);

        let trace = find(dir.path(), "a-3", 7).unwrap();
        let ids: Vec<&str> = trace.chain.iter().filter_map(pulse_id).collect();
        assert_eq!(ids, ["a-1", "a-2", "a-3"]);

        let root = find(dir.path(), "a-1", 7).unwrap();
        assert_eq!(root.children.iter().filter_map(pulse_id).collect::<Vec<_>>(), ["a-2", "a-4"]);
        assert_eq!(root.effects.len(), 2);

        assert!(find(dir.path(), "a-1", 1).is_none(), "only the newest day is searched");
        assert_eq!(recent(dir.path(), 7, 2).iter().filter_map(pulse_id).collect::<Vec<_>>(), ["a-4", "a-3"]);
    }
}
