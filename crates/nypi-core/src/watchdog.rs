//! The watchdog's rules (docs/architecture.md §4.5, principle B6): given the ledger and the time,
//! which tasks have gone silent, which a dead process left to resume, and which to give up on.
//! The decisions are pure so they can be tested; front ends run them periodically and act
//! (nypi-hub/src/watchdog.rs).

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ledger::{RESTART_LIMIT, TaskRecord, TaskState};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WatchdogSettings {
    pub enabled: bool,
    pub interval_seconds: u64,
    /// A running task whose process is alive but has written nothing for this long is stalled.
    pub stall_seconds: u64,
    /// Take up the tasks of dead processes again in the background (the hub does, as a job).
    pub resume: bool,
    /// Leave an interrupted task this long first, so a user restarting NyPi resumes it in place.
    pub resume_after_seconds: u64,
    /// Interrupted tasks older than this are given up instead of resumed.
    pub give_up_after_hours: u64,
}

impl Default for WatchdogSettings {
    fn default() -> Self {
        WatchdogSettings {
            enabled: true,
            interval_seconds: 60,
            stall_seconds: 300,
            resume: true,
            resume_after_seconds: 120,
            give_up_after_hours: 24,
        }
    }
}

/// A dispatched resume that has not taken over within this long may be dispatched again.
const DISPATCH_TIMEOUT_SECONDS: i64 = 600;
/// Background resumes that never took the task over before it is given up: the resume itself
/// cannot start (a broken binary, a missing model, a session that no longer opens).
pub const DISPATCH_LIMIT: u32 = 3;

#[derive(Clone, Debug, PartialEq)]
pub enum Finding {
    /// Alive but silent; `promised` when the run ended saying it would continue.
    Stalled { silent_seconds: i64, promised: bool },
    /// Its process died; resume it in the background.
    Resume,
    /// Its process died and it cannot or should not be resumed.
    GiveUp { reason: String },
    /// A waiting task's schedule fired: the work went on as that schedule's own task.
    Continued { schedule: String },
    /// A waiting task's race was won (`WaitPredicate::AnyOf`, decision D79): the work goes on
    /// with `by`, and the tasks still running for the others are stopped.
    Raced { by: String, superseded: Vec<crate::ledger::WaitToken> },
    /// A waiting task's schedules are all gone without having fired: nothing will bring it back.
    Abandoned,
}

/// What became of something a waiting task counts on (decisions D59 and D79).
pub use crate::ledger::Wake;

impl Wake {
    pub fn of(schedule: Option<&crate::schedule::Schedule>) -> Self {
        match schedule {
            Some(schedule) if schedule.fired > 0 => Wake::Fired,
            Some(schedule) if !schedule.finished => Wake::Pending,
            _ => Wake::Gone,
        }
    }
}

/// What to do about a task that waits, given what became of each wake (in the order of
/// `task.wake`). A schedule that fired brings the agent back at once. Specialists working in the
/// background are a join (decision D75): the task is through waiting when every one of them has
/// reported, not at the first, so a fan-out to several is followed to its end. While one is
/// pending, nothing: the scheduler or the specialist owns the next step.
pub fn check_waiting(task: &TaskRecord, wakes: &[Wake]) -> Option<Finding> {
    if task.state != TaskState::Waiting {
        return None;
    }
    // A line that says exactly what it waits on is judged by that (decision D79).
    if let Some(predicate) = &task.waits {
        let tokens = predicate.tokens();
        let state = |token: &crate::ledger::WaitToken| {
            let at = task.wake.iter().position(|id| id == token.id());
            at.and_then(|at| wakes.get(at)).copied().unwrap_or(Wake::Gone)
        };
        let ids = |tokens: &[crate::ledger::WaitToken]| {
            tokens.iter().map(|token| token.id().to_string()).collect::<Vec<_>>().join(", ")
        };
        return match predicate.judge(state) {
            crate::ledger::Waited::Pending => None,
            crate::ledger::Waited::Abandoned => Some(Finding::Abandoned),
            crate::ledger::Waited::Met { by, superseded }
                if matches!(predicate, crate::ledger::WaitPredicate::AnyOf { .. }) && tokens.len() > 1 =>
            {
                Some(Finding::Raced { by: ids(&by), superseded })
            }
            crate::ledger::Waited::Met { by, .. } => Some(Finding::Continued { schedule: ids(&by) }),
        };
    }
    let mut delegations = Vec::new();
    for (id, wake) in task.wake.iter().zip(wakes) {
        match crate::ledger::Awaited::of(id) {
            crate::ledger::Awaited::Schedule(_) if *wake == Wake::Fired => {
                return Some(Finding::Continued { schedule: id.clone() });
            }
            crate::ledger::Awaited::Schedule(_) => {}
            crate::ledger::Awaited::Delegation(_) => delegations.push((id, *wake)),
        }
    }
    if !delegations.is_empty() && delegations.iter().all(|(_, wake)| *wake == Wake::Fired) {
        let reported: Vec<&str> = delegations.iter().map(|(id, _)| id.as_str()).collect();
        return Some(Finding::Continued { schedule: reported.join(", ") });
    }
    (!wakes.contains(&Wake::Pending)).then_some(Finding::Abandoned)
}

fn age_seconds(at: &str, now: DateTime<Utc>) -> i64 {
    DateTime::parse_from_rfc3339(at).map(|t| (now - t.with_timezone(&Utc)).num_seconds()).unwrap_or(0)
}

/// What to do about one task now, if anything.
pub fn check(task: &TaskRecord, now: DateTime<Utc>, settings: &WatchdogSettings) -> Option<Finding> {
    // Finished tasks need nothing; a blocked one waits for the user, who was told; a waiting
    // one is judged by its schedules (`check_waiting`).
    if task.state.is_terminal() || matches!(task.state, TaskState::BlockedOnUser | TaskState::Waiting) {
        return None;
    }
    let silent = age_seconds(&task.heartbeat_at, now);
    // A run that ended saying "Continuing" without a way back (status enforcement off, or a
    // ledger from before D59). While its process lives, only that process can go on, so the
    // user is told; once the process is gone it is resumed like any interrupted task, and the
    // resume holds the agent to its word (the runtime's continuation rule).
    let promised = task.note.starts_with("Continuing");
    if !task.is_interrupted() {
        return (task.state == TaskState::Running && silent >= settings.stall_seconds as i64)
            .then_some(Finding::Stalled { silent_seconds: silent, promised });
    }
    if task.dispatched_at.as_deref().is_some_and(|at| age_seconds(at, now) < DISPATCH_TIMEOUT_SECONDS) {
        return None;
    }
    if silent < settings.resume_after_seconds as i64 {
        return None;
    }
    let give_up = |reason: String| Some(Finding::GiveUp { reason });
    if task.attempts >= RESTART_LIMIT {
        return give_up(format!("interrupted {} times (restart limit)", task.attempts));
    }
    if task.dispatches >= DISPATCH_LIMIT {
        return give_up(format!(
            "interrupted, and {} background resumes did not start (the hub's log, logs/hub.log, says why)",
            task.dispatches
        ));
    }
    if silent >= settings.give_up_after_hours as i64 * 3600 {
        return give_up(format!("interrupted and not resumed within {} h", settings.give_up_after_hours));
    }
    match &task.session_path {
        None => give_up("interrupted, and its session was not saved, so it cannot be resumed".into()),
        Some(path) if !Path::new(path).exists() => {
            give_up(format!("interrupted, and its session {} is gone", path.display()))
        }
        Some(_) if !settings.resume => Some(Finding::Stalled { silent_seconds: silent, promised: false }),
        Some(_) => Some(Finding::Resume),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{OpenTool, TaskId};

    fn task(pid: u32, state: TaskState, heartbeat_ago: i64, note: &str, session: Option<&Path>) -> TaskRecord {
        let at = (Utc::now() - chrono::Duration::seconds(heartbeat_ago)).to_rfc3339();
        TaskRecord {
            id: TaskId("t-1".into()),
            goal: "g".into(),
            prompt: "g".into(),
            owner: "lucia".into(),
            origin: "cli".into(),
            parent: None,
            session_id: "s".into(),
            session_path: session.map(Path::to_path_buf),
            cwd: "/".into(),
            remote: None,
            pulse: None,
            created: at.clone(),
            heartbeat_at: at,
            state,
            note: note.into(),
            pid,
            attempts: 1,
            open_tools: Vec::<OpenTool>::new(),
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
    fn rules() {
        let settings = WatchdogSettings::default();
        let now = Utc::now();
        let me = std::process::id();
        let dead = i32::MAX as u32 - 9;
        let file = tempfile::NamedTempFile::new().unwrap();
        let session = Some(file.path());

        assert_eq!(check(&task(me, TaskState::Running, 30, "", session), now, &settings), None, "heartbeat is fresh");
        assert!(matches!(
            check(&task(me, TaskState::Running, 400, "", session), now, &settings),
            Some(Finding::Stalled { promised: false, .. })
        ));
        assert_eq!(check(&task(me, TaskState::Stalled, 4000, "", session), now, &settings), None, "flagged once");
        assert_eq!(check(&task(dead, TaskState::Done, 4000, "", session), now, &settings), None);
        assert_eq!(check(&task(dead, TaskState::BlockedOnUser, 4000, "", session), now, &settings), None);

        assert_eq!(check(&task(dead, TaskState::Running, 60, "", session), now, &settings), None, "grace period");
        assert_eq!(check(&task(dead, TaskState::Running, 200, "", session), now, &settings), Some(Finding::Resume));
        let mut dispatched = task(dead, TaskState::Running, 200, "", session);
        dispatched.dispatched_at = Some(Utc::now().to_rfc3339());
        assert_eq!(check(&dispatched, now, &settings), None, "a resume is already on its way");
        let mut looping = task(dead, TaskState::Running, 200, "", session);
        looping.attempts = RESTART_LIMIT;
        assert!(
            matches!(check(&looping, now, &settings), Some(Finding::GiveUp { reason }) if reason.contains("restart limit"))
        );
        assert!(matches!(
            check(&task(dead, TaskState::Running, 200, "", None), now, &settings),
            Some(Finding::GiveUp { .. })
        ));
        assert!(matches!(
            check(&task(dead, TaskState::Running, 90_000, "", session), now, &settings),
            Some(Finding::GiveUp { .. })
        ));

        let mut unstartable = task(dead, TaskState::Running, 900, "", session);
        unstartable.dispatches = DISPATCH_LIMIT;
        assert!(
            matches!(check(&unstartable, now, &settings), Some(Finding::GiveUp { reason }) if reason.contains("did not start")),
            "a resume that cannot start is not dispatched forever"
        );

        // A promise without a way back: the user hears while its process lives, and it is
        // resumed (and held to its word) once the process is gone.
        let promised = task(me, TaskState::Running, 400, "Continuing: waiting on CI", session);
        assert!(matches!(check(&promised, now, &settings), Some(Finding::Stalled { promised: true, .. })));
        let orphaned = task(dead, TaskState::Running, 400, "Continuing: waiting on CI", session);
        assert_eq!(check(&orphaned, now, &settings), Some(Finding::Resume));
        let manual = WatchdogSettings { resume: false, ..settings };
        assert!(matches!(
            check(&task(dead, TaskState::Running, 200, "", session), now, &manual),
            Some(Finding::Stalled { .. })
        ));
    }

    #[test]
    fn a_waiting_task_is_judged_by_its_schedules() {
        let settings = WatchdogSettings::default();
        let dead = i32::MAX as u32 - 9;
        let mut waiting = task(dead, TaskState::Waiting, 90_000, "Continuing: back at 18:00", None);
        waiting.wake = vec!["s-1".into(), "s-2".into()];
        assert_eq!(check(&waiting, Utc::now(), &settings), None, "no process and old: still not interrupted");
        assert_eq!(check_waiting(&waiting, &[Wake::Pending, Wake::Gone]), None, "one way back is enough");
        assert_eq!(
            check_waiting(&waiting, &[Wake::Gone, Wake::Fired]),
            Some(Finding::Continued { schedule: "s-2".into() })
        );
        assert_eq!(check_waiting(&waiting, &[Wake::Gone, Wake::Gone]), Some(Finding::Abandoned));
        let running = task(dead, TaskState::Running, 10, "", None);
        assert_eq!(check_waiting(&running, &[]), None, "only waiting tasks");

        // Several specialists at work: through waiting when all have reported, not at the first.
        waiting.wake = vec!["d-1".into(), "d-2".into()];
        assert_eq!(check_waiting(&waiting, &[Wake::Fired, Wake::Pending]), None, "one still works");
        assert_eq!(
            check_waiting(&waiting, &[Wake::Fired, Wake::Fired]),
            Some(Finding::Continued { schedule: "d-1, d-2".into() })
        );
        assert_eq!(check_waiting(&waiting, &[Wake::Fired, Wake::Gone]), Some(Finding::Abandoned), "one never will");
        // A schedule set beside them still brings the agent back on its own.
        waiting.wake = vec!["d-1".into(), "s-9".into()];
        assert_eq!(
            check_waiting(&waiting, &[Wake::Pending, Wake::Fired]),
            Some(Finding::Continued { schedule: "s-9".into() })
        );
    }

    #[test]
    fn a_task_that_says_what_it_waits_on_is_judged_by_that() {
        use crate::ledger::{TaskId, WaitPredicate, WaitToken, Waited};
        let dead = i32::MAX as u32 - 9;
        let mut waiting = task(dead, TaskState::Waiting, 60, "Continuing: three probes are out", None);
        let probes = [WaitToken::Task(TaskId("t-a".into())), WaitToken::Task(TaskId("t-b".into()))];
        waiting.wake = vec!["t-a".into(), "t-b".into()];

        // A race: the first probe that comes through wins, and the one still at work is named
        // so it can be stopped; a failed probe does not win, and all failed is the end.
        waiting.waits = Some(WaitPredicate::AnyOf { tokens: probes.to_vec() });
        assert_eq!(check_waiting(&waiting, &[Wake::Pending, Wake::Pending]), None);
        assert_eq!(check_waiting(&waiting, &[Wake::Gone, Wake::Pending]), None, "the other may still come through");
        assert_eq!(
            check_waiting(&waiting, &[Wake::Pending, Wake::Fired]),
            Some(Finding::Raced { by: "t-b".into(), superseded: vec![probes[0].clone()] })
        );
        assert_eq!(check_waiting(&waiting, &[Wake::Gone, Wake::Gone]), Some(Finding::Abandoned));

        // A join: every one, and one that never will ends the wait.
        waiting.waits = Some(WaitPredicate::AllOf { tokens: probes.to_vec() });
        assert_eq!(check_waiting(&waiting, &[Wake::Fired, Wake::Pending]), None);
        assert_eq!(
            check_waiting(&waiting, &[Wake::Fired, Wake::Fired]),
            Some(Finding::Continued { schedule: "t-a, t-b".into() })
        );
        assert_eq!(check_waiting(&waiting, &[Wake::Fired, Wake::Gone]), Some(Finding::Abandoned));

        // Plain wake ids become a predicate where one says the same: a schedule, a join of
        // specialists. A mixed list keeps the older rule.
        assert_eq!(WaitPredicate::of_wakes(&["s-1".into()]), Some(WaitPredicate::Schedule { id: "s-1".into() }));
        assert_eq!(
            WaitPredicate::of_wakes(&["d-1".into(), "d-2".into()]),
            Some(WaitPredicate::AllOf {
                tokens: vec![WaitToken::Delegation("d-1".into()), WaitToken::Delegation("d-2".into())]
            })
        );
        assert_eq!(WaitPredicate::of_wakes(&["d-1".into(), "s-9".into()]), None);
        assert_eq!(WaitPredicate::of_wakes(&[]), None);
        let approval = WaitPredicate::Approval { id: "a1".into() };
        assert_eq!(approval.judge(|_| Wake::Pending), Waited::Pending);
        let line = serde_json::to_string(&WaitPredicate::AnyOf { tokens: probes.to_vec() }).unwrap();
        assert_eq!(line, r#"{"type":"anyOf","tokens":[{"kind":"task","id":"t-a"},{"kind":"task","id":"t-b"}]}"#);
    }
}
