//! The notice outbox (principles A3 and B6, decision D57): everything NyPi has to tell the user
//! when nobody is watching is written here before it is sent, in an append-only JSONL file. A
//! notice stays open until a channel accepts it or a front end shows it, so a failed send, a
//! missing channel, or a process that exits early cannot lose it: the supervisor sends it again,
//! and the next front end the user opens shows what they missed. Only the harness writes the
//! outbox; the Inspector guard keeps the agent's tools away from it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::inspector::AlertLevel;
use crate::ledger::TaskId;

/// Sends tried before a notice is left for the next front end to show.
pub const MAX_ATTEMPTS: u32 = 8;
/// The first retry waits this long; each later one twice as long, up to `RETRY_CAP_SECONDS`.
const RETRY_BASE_SECONDS: i64 = 60;
const RETRY_CAP_SECONDS: i64 = 3600;
/// A notice older than this is no longer pushed; it is still shown when a front end opens.
const RETRY_DAYS: i64 = 3;
/// Failure texts are kept this long; they may quote a server's reply.
const FAILURE_CHARS: usize = 200;

/// One outbox line. A notice's state is the fold of its events in file order.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum OutboxEvent {
    Queued {
        notice: String,
        at: String,
        level: AlertLevel,
        /// Which part of NyPi raised it: `watchdog`, `scheduler`, `run`, `dmn`, `supervisor`.
        source: String,
        text: String,
        task: Option<TaskId>,
    },
    /// One round of sending: the channels that accepted it and the ones that failed.
    Attempt { notice: String, at: String, delivered: Vec<String>, failed: Vec<String> },
    /// A front end showed it to the user.
    Seen { notice: String, at: String, by: String },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub id: String,
    pub at: String,
    pub level: AlertLevel,
    pub source: String,
    pub text: String,
    pub task: Option<TaskId>,
    /// Channels that accepted it.
    pub delivered: Vec<String>,
    /// What went wrong in the latest round of sending.
    pub failed: Vec<String>,
    pub attempts: u32,
    pub last_attempt: Option<String>,
    /// The front end that showed it, if one did.
    pub seen_by: Option<String>,
}

impl Notice {
    /// No channel took it and no front end showed it: the user has not heard.
    pub fn is_open(&self) -> bool {
        self.delivered.is_empty() && self.seen_by.is_none()
    }

    /// Open, and time to send it again.
    pub fn retry_due(&self, now: DateTime<Utc>) -> bool {
        if !self.is_open() || self.attempts >= MAX_ATTEMPTS || age_seconds(&self.at, now) > RETRY_DAYS * 86_400 {
            return false;
        }
        match &self.last_attempt {
            None => true,
            Some(at) => {
                let wait = (RETRY_BASE_SECONDS << (self.attempts.saturating_sub(1)).min(10)).min(RETRY_CAP_SECONDS);
                age_seconds(at, now) >= wait
            }
        }
    }

    /// How the user came to know, in a few words.
    pub fn state(&self) -> String {
        match (&self.seen_by, self.delivered.is_empty()) {
            (_, false) => format!("sent ({})", self.delivered.join(", ")),
            (Some(by), true) => format!("shown in {by}"),
            (None, true) if self.attempts == 0 => "not sent yet".into(),
            (None, true) => format!("NOT DELIVERED after {} attempt(s)", self.attempts),
        }
    }
}

fn age_seconds(at: &str, now: DateTime<Utc>) -> i64 {
    DateTime::parse_from_rfc3339(at).map(|t| (now - t.with_timezone(&Utc)).num_seconds()).unwrap_or(0)
}

pub struct Outbox {
    path: PathBuf,
    write: Mutex<()>,
    /// Every notice as the log's events leave it, kept in step with the file (jsonl.rs).
    notices: crate::jsonl::Folded<Vec<Notice>>,
}

impl Outbox {
    pub fn new(path: PathBuf) -> Self {
        let notices = crate::jsonl::Folded::shared(&path);
        Outbox { path, write: Mutex::new(()), notices }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn append(&self, event: &OutboxEvent) -> std::io::Result<()> {
        let _guard = self.write.lock().unwrap();
        crate::jsonl::append(&self.path, event)
    }

    /// Record a notice before anything tries to send it. Returns its id.
    pub fn queue(&self, level: AlertLevel, source: &str, text: &str, task: Option<&TaskId>) -> std::io::Result<String> {
        let id = uuid::Uuid::now_v7().simple().to_string();
        let id = format!("n-{}", &id[id.len() - 12..]);
        self.append(&OutboxEvent::Queued {
            notice: id.clone(),
            at: crate::ledger::now(),
            level,
            source: source.to_string(),
            text: text.to_string(),
            task: task.cloned(),
        })?;
        Ok(id)
    }

    /// Record one round of sending. A round with no channel at all is not an attempt.
    pub fn record_attempt(&self, notice: &str, delivered: Vec<String>, failed: Vec<String>) -> std::io::Result<()> {
        if delivered.is_empty() && failed.is_empty() {
            return Ok(());
        }
        let failed = failed.into_iter().map(|f| f.chars().take(FAILURE_CHARS).collect()).collect();
        self.append(&OutboxEvent::Attempt { notice: notice.to_string(), at: crate::ledger::now(), delivered, failed })
    }

    pub fn mark_seen(&self, notice: &str, by: &str) -> std::io::Result<()> {
        self.append(&OutboxEvent::Seen { notice: notice.to_string(), at: crate::ledger::now(), by: by.to_string() })
    }

    /// Look at every notice, oldest first, without copying them. Only what was appended to the
    /// log since the last look is read. Unreadable lines (a crash mid-write) are skipped.
    fn read<R>(&self, look: impl FnOnce(&[Notice]) -> R) -> R {
        self.notices.read(take_in, |notices| look(notices))
    }

    /// Every notice, oldest first.
    pub fn load(&self) -> Vec<Notice> {
        self.read(<[Notice]>::to_vec)
    }

    /// Notices the user has not heard, oldest first.
    pub fn open(&self) -> Vec<Notice> {
        self.read(|notices| notices.iter().filter(|notice| notice.is_open()).cloned().collect())
    }
}

/// Take one line of the log into the fold.
fn take_in(notices: &mut Vec<Notice>, line: &str, _: u64) {
    let Ok(event) = serde_json::from_str::<OutboxEvent>(line) else { return };
    match event {
        OutboxEvent::Queued { notice, at, level, source, text, task } => notices.push(Notice {
            id: notice,
            at,
            level,
            source,
            text,
            task,
            delivered: Vec::new(),
            failed: Vec::new(),
            attempts: 0,
            last_attempt: None,
            seen_by: None,
        }),
        OutboxEvent::Attempt { notice, at, delivered, failed } => {
            if let Some(found) = notices.iter_mut().rev().find(|n| n.id == notice) {
                found.attempts += 1;
                found.last_attempt = Some(at);
                for channel in delivered {
                    if !found.delivered.contains(&channel) {
                        found.delivered.push(channel);
                    }
                }
                found.failed = failed;
            }
        }
        OutboxEvent::Seen { notice, by, .. } => {
            if let Some(found) = notices.iter_mut().rev().find(|n| n.id == notice) {
                found.seen_by.get_or_insert(by);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outbox() -> (tempfile::TempDir, Outbox) {
        let dir = tempfile::tempdir().unwrap();
        let outbox = Outbox::new(dir.path().join("outbox").join("notices.jsonl"));
        (dir, outbox)
    }

    #[test]
    fn a_notice_is_open_until_a_channel_takes_it_or_a_front_end_shows_it() {
        let (_dir, outbox) = outbox();
        let task = TaskId("t-1".into());
        let failed = outbox.queue(AlertLevel::Error, "watchdog", "Failed: task t-1 died", Some(&task)).unwrap();
        let sent = outbox.queue(AlertLevel::Notice, "scheduler", "⏰ stretch", None).unwrap();
        let shown = outbox.queue(AlertLevel::Warning, "run", "Blocked: need a token", None).unwrap();
        assert_eq!(outbox.open().len(), 3, "queued is not delivered");

        outbox.record_attempt(&failed, vec![], vec!["webhook to example.com (Slack): HTTP 500".into()]).unwrap();
        outbox.record_attempt(&sent, vec!["desktop notification".into()], vec!["webhook: timed out".into()]).unwrap();
        outbox.mark_seen(&shown, "terminal").unwrap();

        let open = outbox.open();
        assert_eq!(open.len(), 1, "one channel taking it is enough; so is a front end showing it");
        assert_eq!((open[0].id.as_str(), open[0].attempts), (failed.as_str(), 1));
        assert_eq!(open[0].task, Some(task));
        assert!(open[0].state().contains("NOT DELIVERED"), "{}", open[0].state());
        let all = outbox.load();
        assert_eq!(all[1].state(), "sent (desktop notification)");
        assert_eq!(all[2].state(), "shown in terminal");
    }

    #[test]
    fn having_no_channel_is_not_an_attempt_and_retries_back_off_then_stop() {
        let (_dir, outbox) = outbox();
        let id = outbox.queue(AlertLevel::Warning, "watchdog", "stalled", None).unwrap();
        outbox.record_attempt(&id, vec![], vec![]).unwrap();
        let now = Utc::now();
        let notice = outbox.open().remove(0);
        assert_eq!(notice.attempts, 0);
        assert!(notice.retry_due(now), "never tried: due at once");

        outbox.record_attempt(&id, vec![], vec!["webhook: HTTP 503".into()]).unwrap();
        let notice = outbox.open().remove(0);
        assert!(!notice.retry_due(now + chrono::Duration::seconds(30)), "the first retry waits a minute");
        assert!(notice.retry_due(now + chrono::Duration::seconds(61)));

        for _ in 1..MAX_ATTEMPTS {
            outbox.record_attempt(&id, vec![], vec!["webhook: HTTP 503".into()]).unwrap();
        }
        let notice = outbox.open().remove(0);
        assert!(!notice.retry_due(now + chrono::Duration::days(1)), "sending stops after the limit");
        assert!(notice.is_open(), "but it stays open for the next front end to show");
    }

    #[test]
    fn a_torn_line_costs_only_itself() {
        let (_dir, outbox) = outbox();
        let first = outbox.queue(AlertLevel::Notice, "run", "完成：报告已生成", None).unwrap();
        // A crash mid-write, cut inside a multibyte character.
        let mut bytes = std::fs::read(outbox.path()).unwrap();
        bytes.extend_from_slice(b"{\"event\":\"queued\",\"notice\":\"n-x\",\"text\":\"\xe6\x8a");
        std::fs::write(outbox.path(), bytes).unwrap();
        let second = outbox.queue(AlertLevel::Notice, "run", "after the crash", None).unwrap();
        let ids: Vec<String> = outbox.load().into_iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![first, second]);
    }
}
