//! The node's journal of calls (docs/devices.md section 7, decision D74): the ids it has taken,
//! so a call is carried out at most once even across a restart of the node, and the result of
//! every call that ended, kept until the hub says it has it (`ack`). A link that drops in the
//! middle of a call therefore loses nothing: the call goes on, and its result is handed over
//! when the hub is back. Without a file the journal lives only as long as the process.
//!
//! On disk it is one JSON line per entry (`taken`, `ended`, `acked`), appended as things happen
//! and rewritten without what is settled once it has grown.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::Write;
use std::path::PathBuf;

use nypi_node_proto::FromNode;
use serde::{Deserialize, Serialize};

/// Call ids remembered as taken, newest kept: a hub never repeats an id older than these.
const REMEMBERED: usize = 10_000;
/// Entries on disk past which the file is rewritten with only what still matters.
const REWRITE_AFTER: usize = 4 * REMEMBERED;

#[derive(Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
enum Entry {
    Taken { id: String },
    Ended { event: FromNode },
    Acked { id: String },
}

#[derive(Default)]
pub(crate) struct Journal {
    file: Option<PathBuf>,
    taken: HashSet<String>,
    order: VecDeque<String>,
    /// The last word on each call that ended and the hub has not acknowledged, by id.
    ended: BTreeMap<String, FromNode>,
    written: usize,
}

impl Journal {
    /// The journal kept in `file`, with what an earlier run of the node left there.
    pub(crate) fn open(file: PathBuf) -> Journal {
        let mut journal = Journal::default();
        if let Ok(text) = std::fs::read_to_string(&file) {
            for line in text.lines() {
                // A line cut off by a crash is the last one; it is skipped.
                let Ok(entry) = serde_json::from_str::<Entry>(line) else { continue };
                journal.apply(entry);
                journal.written += 1;
            }
        }
        journal.file = Some(file);
        journal
    }

    fn apply(&mut self, entry: Entry) {
        match entry {
            Entry::Taken { id } => {
                if self.taken.insert(id.clone()) {
                    self.order.push_back(id);
                    if self.order.len() > REMEMBERED
                        && let Some(old) = self.order.pop_front()
                    {
                        self.taken.remove(&old);
                    }
                }
            }
            Entry::Ended { event } => {
                if let Some(id) = event.id() {
                    self.ended.insert(id.to_string(), event);
                }
            }
            Entry::Acked { id } => {
                self.ended.remove(&id);
            }
        }
    }

    fn record(&mut self, entry: Entry) {
        let line = serde_json::to_string(&entry).ok();
        self.apply(entry);
        let (Some(file), Some(line)) = (&self.file, line) else { return };
        if self.written >= REWRITE_AFTER {
            self.rewrite();
            return;
        }
        let appended = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
            .and_then(|mut handle| writeln!(handle, "{line}"));
        if appended.is_ok() {
            self.written += 1;
        }
    }

    /// The file again, with only the ids remembered and the results not acknowledged.
    fn rewrite(&mut self) {
        let Some(file) = &self.file else { return };
        let mut lines = Vec::new();
        for id in &self.order {
            lines.push(serde_json::to_string(&Entry::Taken { id: id.clone() }).unwrap_or_default());
        }
        for event in self.ended.values() {
            lines.push(serde_json::to_string(&Entry::Ended { event: event.clone() }).unwrap_or_default());
        }
        let draft = file.with_extension("jsonl.tmp");
        let written = std::fs::write(&draft, lines.join("\n") + "\n").and_then(|()| std::fs::rename(&draft, file));
        if written.is_ok() {
            self.written = lines.len();
        }
    }

    /// Take `id`, or say it was taken before.
    pub(crate) fn take(&mut self, id: &str) -> bool {
        if self.taken.contains(id) {
            return false;
        }
        self.record(Entry::Taken { id: id.to_string() });
        true
    }

    /// The last word on a call, kept until the hub acknowledges it.
    pub(crate) fn ended(&mut self, event: &FromNode) {
        if event.ends_call() {
            self.record(Entry::Ended { event: event.clone() });
        }
    }

    /// What became of a call that ended and is not acknowledged yet.
    pub(crate) fn result(&self, id: &str) -> Option<FromNode> {
        self.ended.get(id).cloned()
    }

    pub(crate) fn ack(&mut self, id: &str) {
        if self.ended.contains_key(id) {
            self.record(Entry::Acked { id: id.to_string() });
        }
    }

    /// Every result the hub has not acknowledged, oldest call first.
    pub(crate) fn unacknowledged(&self) -> Vec<FromNode> {
        self.ended.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use nypi_node_proto::CallResult;

    use super::*;

    #[test]
    fn what_was_taken_and_not_acknowledged_outlives_the_process() {
        let dir = tempfile_dir();
        let file = dir.join("journal.jsonl");
        let mut journal = Journal::open(file.clone());
        assert!(journal.take("c-1") && journal.take("c-2"));
        assert!(!journal.take("c-1"), "an id is taken once");
        journal.ended(&FromNode::Started { id: "c-1".into() });
        journal.ended(&FromNode::Finished { id: "c-1".into(), result: CallResult::text("built") });
        journal.ended(&FromNode::Refused { id: "c-2".into(), reason: "no".into() });
        journal.ack("c-2");

        let again = Journal::open(file);
        assert!(!again.taken.is_empty() && again.taken.contains("c-1"), "taken ids are remembered");
        assert_eq!(
            again.unacknowledged(),
            [FromNode::Finished { id: "c-1".into(), result: CallResult::text("built") }]
        );
        assert!(again.result("c-2").is_none(), "an acknowledged result is let go");
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn tempfile_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nypi-journal-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
