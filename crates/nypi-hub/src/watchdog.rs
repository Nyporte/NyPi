//! Runs the watchdog (nypi-core/src/watchdog.rs) and acts on what it finds: a stalled task is
//! marked and alerted, a task a dead process left is handed to the hub to be taken up again
//! (an order, jobs.rs), one that cannot be resumed is failed with an alert, and a task waiting
//! on a schedule is closed when the schedule fires or handed to the user when it is cancelled.
//! The watchdog is a module of the hub; the supervisor runs it while no daemon does. An
//! exclusive lock on `~/.nypi/run/watchdog.lock` lets one process act per round, so findings
//! are not reported twice when more than one hub is running. What it finds goes through the
//! outbox (decision D57), so it is never only printed.

use std::sync::Arc;
use std::time::Duration;

use nypi_core::config::Paths;
use nypi_core::inspector::{Alert, AlertLevel, Inspector};
use nypi_core::ledger::{self, Ledger, LedgerEvent, RESTART_LIMIT, TaskId, TaskRecord, TaskState, process_alive};
use nypi_core::schedule::ScheduleStore;
use nypi_core::watchdog::{Finding, Wake, WatchdogSettings, check, check_waiting};

use crate::jobs::{Jobs, Order};
use crate::notify::{self, Notice, Notifier};
use crate::scheduler::Round;

/// A round that takes longer than this is abandoned, releasing the lock: one stuck round must
/// not stop every other process from checking.
pub const ROUND_LIMIT: Duration = Duration::from_secs(120);

/// Check the ledger every `interval_seconds` in the background. Alerts go through the
/// Inspector, so the front end named by `shown_in` shows them (the Telegram bridge pushes them
/// to its chats), and through the outbox to the notifier's channels. Each round also sends
/// again what no channel took before.
pub fn spawn(
    paths: Paths,
    settings: WatchdogSettings,
    inspector: Arc<Inspector>,
    notifier: Option<Notifier>,
    shown_in: Option<&'static str>,
    jobs: Jobs,
) -> Option<tokio::task::JoinHandle<()>> {
    if !settings.enabled {
        return None;
    }
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(settings.interval_seconds.max(5)));
        loop {
            interval.tick().await;
            let round = tokio::time::timeout(ROUND_LIMIT, run_once(&paths, &settings)).await.unwrap_or_default();
            for (level, message) in round.reports {
                inspector.alert(Alert::new(level, message.clone()));
                let notice = Notice { source: "watchdog", level, text: &message, task: None, shown_in };
                notify::post(&paths, notifier.as_ref(), notice).await;
            }
            for order in round.orders {
                jobs.take(order);
            }
            notify::flush(&paths, notifier.as_ref()).await;
        }
    }))
}

/// One round: act on every finding. Empty when another process holds the lock this round.
pub async fn run_once(paths: &Paths, settings: &WatchdogSettings) -> Round {
    let lock_path = paths.home.join("run").join("watchdog.lock");
    let Some(lock) = crate::round_lock::RoundLock::try_take(&lock_path) else {
        return Round::default();
    };
    let ledger = Ledger::new(paths.ledger());
    let now = chrono::Utc::now();
    let mut reports = Vec::new();
    let mut orders = Vec::new();
    // Read once, and only when a task waits on a schedule.
    let mut schedules = None;
    let tasks = ledger.load();
    for task in tasks.values() {
        let finding = if task.state == TaskState::Waiting {
            let all = schedules.get_or_insert_with(|| ScheduleStore::new(paths.schedules()).load_all());
            // What a line names exactly is read as named (decision D79); a plain id is a
            // specialist's (`d-…`) or a schedule's.
            let named = task.waits.as_ref().map(|predicate| predicate.tokens()).unwrap_or_default();
            let wake = |id: &String| {
                let token = named.iter().find(|token| token.id() == id).cloned();
                match token.unwrap_or_else(|| nypi_core::ledger::WaitToken::of(id)) {
                    nypi_core::ledger::WaitToken::Delegation(id) => delegation_wake(&id, task, &tasks),
                    nypi_core::ledger::WaitToken::Schedule(id) => Wake::of(all.iter().find(|s| s.id == id)),
                    nypi_core::ledger::WaitToken::Task(id) => match tasks.get(&id).map(|other| other.state) {
                        Some(TaskState::Done) => Wake::Fired,
                        Some(state) if !state.is_terminal() => Wake::Pending,
                        // Failed, cancelled, or never recorded: it will not come through.
                        _ => Wake::Gone,
                    },
                    // An approval is asked by a live process; with that process gone, nobody
                    // can answer it any more.
                    nypi_core::ledger::WaitToken::Approval(_) => {
                        if process_alive(task.pid) {
                            Wake::Pending
                        } else {
                            Wake::Gone
                        }
                    }
                }
            };
            let wakes: Vec<Wake> = task.wake.iter().map(wake).collect();
            check_waiting(task, &wakes)
        } else {
            check(task, now, settings)
        };
        let Some(finding) = finding else { continue };
        if matches!(finding, Finding::Resume) {
            orders.push(Order::Resume(Box::new(task.clone())));
        }
        match act(&ledger, task, finding) {
            Ok(Some(report)) => reports.push(report),
            Ok(None) => {}
            Err(error) => {
                reports.push((AlertLevel::Error, format!("Watchdog could not handle task {}: {error:#}", task.id)))
            }
        }
    }
    drop(lock);
    Round { reports, orders }
}

/// What became of the specialist a task waits on (decision D67, part 4). Its report comes back to
/// the conversation as a task of its own, with origin `report`, which says the delegation's id.
fn delegation_wake(id: &str, waiting: &TaskRecord, tasks: &std::collections::BTreeMap<TaskId, TaskRecord>) -> Wake {
    // The report reached the conversation: reviewing it is a task that reports for itself.
    if tasks.values().any(|task| task.origin == "report" && task.prompt.contains(id)) {
        return Wake::Fired;
    }
    let children: Vec<&TaskRecord> = tasks.values().filter(|task| task.parent.as_ref() == Some(&waiting.id)).collect();
    // Still at work. One whose process died is taken up again by its own finding.
    if children.iter().any(|task| !task.state.is_terminal()) {
        return Wake::Pending;
    }
    // Taken up again after a restart, it ran as a job and told the owner its result itself.
    if children.iter().any(|task| task.attempts > 1) {
        return Wake::Fired;
    }
    // The hub that started it is still there: it is starting, or its report is waiting its turn.
    // Otherwise the hub went away with the report unread, and nothing will bring this task back.
    if process_alive(waiting.pid) { Wake::Pending } else { Wake::Gone }
}

/// Record a finding in the ledger, and return what to tell the user, if anything.
fn act(ledger: &Ledger, task: &TaskRecord, finding: Finding) -> anyhow::Result<Option<(AlertLevel, String)>> {
    let goal: String = task.goal.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(160).collect();
    let state = |state, note: String| LedgerEvent::State { task: task.id.clone(), at: ledger::now(), state, note };
    Ok(Some(match finding {
        Finding::Raced { by, superseded } => {
            // The race is decided: what still runs for the others is no longer needed. Their
            // tasks are closed here; a process still on one finds its task cancelled.
            let all = ledger.load();
            for token in &superseded {
                let stopped: Vec<&TaskRecord> = match token {
                    nypi_core::ledger::WaitToken::Task(id) => all.get(id).into_iter().collect(),
                    nypi_core::ledger::WaitToken::Delegation(_) => {
                        all.values().filter(|other| other.parent.as_ref() == Some(&task.id)).collect()
                    }
                    _ => Vec::new(),
                };
                for other in stopped.into_iter().filter(|other| !other.state.is_terminal()) {
                    ledger.append(&LedgerEvent::State {
                        task: other.id.clone(),
                        at: ledger::now(),
                        state: TaskState::Cancelled,
                        note: format!("superseded: {by} came through first for task {}", task.id),
                    })?;
                }
            }
            ledger.append(&state(TaskState::Done, format!("continued with the first to come through ({by})")))?;
            return Ok(None);
        }
        Finding::Continued { schedule } => {
            // The work went on as a task of its own (the schedule's, or the review of the
            // specialist's report), which reports for itself.
            let note = match nypi_core::ledger::Awaited::of(&schedule) {
                nypi_core::ledger::Awaited::Delegation(_) => {
                    format!("continued when its specialists reported ({schedule})")
                }
                nypi_core::ledger::Awaited::Schedule(_) => format!("continued by schedule {schedule}, which fired"),
            };
            ledger.append(&state(TaskState::Done, note))?;
            return Ok(None);
        }
        Finding::Abandoned
            if task
                .wake
                .iter()
                .all(|id| matches!(nypi_core::ledger::Awaited::of(id), nypi_core::ledger::Awaited::Delegation(_))) =>
        {
            ledger.append(&state(
                TaskState::BlockedOnUser,
                "NyPi stopped before its specialist's report was reviewed".to_string(),
            ))?;
            (
                AlertLevel::Warning,
                format!(
                    "Blocked: task {} handed work to a specialist in the background ({}) and waited for the report, \
                     but NyPi stopped before Lucia reviewed it, so nothing will bring it back. Goal: {goal}. The \
                     specialist's own task is in nypi tasks --all; ask Lucia to look at its result, or close this \
                     with nypi tasks cancel {}.",
                    task.id,
                    task.note.trim_start_matches("Continuing:").trim(),
                    task.id
                ),
            )
        }
        Finding::Abandoned => {
            let waited_on = task.wake.join(", ");
            ledger.append(&state(
                TaskState::BlockedOnUser,
                format!("its way back (schedule {waited_on}) was cancelled before it fired"),
            ))?;
            (
                AlertLevel::Warning,
                format!(
                    "Blocked: task {} said it would continue ({}) and waited on schedule {waited_on}, which was \
                     cancelled before it fired, so nothing will bring it back. Goal: {goal}. Ask Lucia to pick it up, \
                     or close it with nypi tasks cancel {}.",
                    task.id,
                    task.note.trim_start_matches("Continuing:").trim(),
                    task.id
                ),
            )
        }
        Finding::Stalled { silent_seconds, promised } => {
            let minutes = (silent_seconds / 60).max(1);
            let (note, message) = if promised {
                (
                    format!("said it would continue, then nothing ran for {minutes} min"),
                    format!(
                        "Blocked: task {} said it would continue ({}), but nothing has run for {minutes} min. Goal: {goal}. \
                         Ask Lucia to pick it up, or close it with nypi tasks cancel {}.",
                        task.id,
                        task.note.trim_start_matches("Continuing:").trim(),
                        task.id
                    ),
                )
            } else {
                (
                    format!("no heartbeat for {minutes} min"),
                    format!(
                        "Task {} has stalled: its process (pid {}) has written nothing for {minutes} min and may be hung. \
                         Goal: {goal}. It recovers on its own if it was only slow; otherwise abort it and it resumes.",
                        task.id, task.pid
                    ),
                )
            };
            ledger.append(&state(TaskState::Stalled, note))?;
            (AlertLevel::Warning, message)
        }
        Finding::Resume => {
            // Recorded before it is handed over: a resume that never starts is counted, and
            // after three the task fails instead of being dispatched for ever.
            ledger.append(&LedgerEvent::Dispatched { task: task.id.clone(), at: ledger::now() })?;
            (
                AlertLevel::Warning,
                format!(
                    "Continuing: task {} was interrupted (its process died), so it is being taken up again in the \
                     background (attempt {}/{RESTART_LIMIT}). Goal: {goal}.",
                    task.id,
                    task.attempts + 1
                ),
            )
        }
        Finding::GiveUp { reason } => {
            ledger.append(&state(TaskState::Failed, reason.clone()))?;
            (
                AlertLevel::Error,
                format!("Failed: task {} was {reason}. Goal: {goal}. Start it again if it still matters.", task.id),
            )
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nypi_core::ledger::{TaskId, new_task_id};

    fn add(ledger: &Ledger, pid: u32, minutes_ago: i64, session: Option<std::path::PathBuf>) -> TaskId {
        let at = (chrono::Utc::now() - chrono::Duration::minutes(minutes_ago)).to_rfc3339();
        let task = new_task_id();
        ledger
            .append(&LedgerEvent::Created {
                task: task.clone(),
                at: at.clone(),
                goal: "build the report".into(),
                prompt: "build the report".into(),
                owner: "lucia".into(),
                origin: "cli".into(),
                parent: None,
                session_id: "s".into(),
                session_path: session,
                cwd: "/".into(),
                remote: None,
                pid,
                pulse: None,
            })
            .unwrap();
        ledger
            .append(&LedgerEvent::State { task: task.clone(), at, state: TaskState::Running, note: String::new() })
            .unwrap();
        task
    }

    #[tokio::test]
    async fn a_race_is_won_by_the_first_probe_through_and_the_others_are_closed() {
        use nypi_core::ledger::{WaitPredicate, WaitToken};
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let ledger = Ledger::new(paths.ledger());
        let me = std::process::id();
        let dead = i32::MAX as u32 - 7;
        // Three probes of one question, started by a task that then waits for the first.
        let (slow, quick, failed) = (add(&ledger, me, 0, None), add(&ledger, me, 0, None), add(&ledger, me, 0, None));
        let parent = add(&ledger, dead, 1, None);
        let tokens: Vec<WaitToken> = [&slow, &quick, &failed].map(|task| WaitToken::Task(task.clone())).to_vec();
        ledger
            .append(&LedgerEvent::Waiting {
                task: parent.clone(),
                at: ledger::now(),
                schedules: tokens.iter().map(|token| token.id().to_string()).collect(),
                note: "Continuing: three ways are being tried".into(),
                on: Some(WaitPredicate::AnyOf { tokens }),
            })
            .unwrap();
        let end = |task: &TaskId, state| {
            ledger.append(&LedgerEvent::State { task: task.clone(), at: ledger::now(), state, note: String::new() })
        };
        let settings = WatchdogSettings::default();

        // A probe that failed decides nothing while others still work.
        end(&failed, TaskState::Failed).unwrap();
        run_once(&paths, &settings).await;
        assert_eq!(ledger.get(&parent).unwrap().state, TaskState::Waiting);

        // The first one through wins: the parent goes on, and the probe still at work is closed.
        end(&quick, TaskState::Done).unwrap();
        run_once(&paths, &settings).await;
        let parent = ledger.get(&parent).unwrap();
        assert_eq!(parent.state, TaskState::Done);
        assert!(parent.note.contains(&quick.0), "{}", parent.note);
        let slow = ledger.get(&slow).unwrap();
        assert_eq!(slow.state, TaskState::Cancelled);
        assert!(slow.note.contains("superseded"), "{}", slow.note);
        assert_eq!(ledger.get(&failed).unwrap().state, TaskState::Failed, "what had ended stays as it ended");
    }

    #[tokio::test]
    async fn stalls_fails_and_takes_turns() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let ledger = Ledger::new(paths.ledger());
        let hung = add(&ledger, std::process::id(), 10, None);
        let lost = add(&ledger, i32::MAX as u32 - 3, 10, None);
        let settings = WatchdogSettings::default();

        let reports = run_once(&paths, &settings).await.reports;
        assert_eq!(reports.len(), 2, "{reports:?}");
        assert!(reports.iter().any(|(_, m)| m.contains(&hung.0) && m.contains("stalled")));
        assert!(
            reports
                .iter()
                .any(|(level, m)| *level == AlertLevel::Error && m.contains(&lost.0) && m.contains("not saved"))
        );
        assert_eq!(ledger.get(&hung).unwrap().state, TaskState::Stalled);
        assert_eq!(ledger.get(&lost).unwrap().state, TaskState::Failed);
        assert!(run_once(&paths, &settings).await.reports.is_empty(), "each finding is reported once");

        // Life after a stall puts the task back to running.
        ledger.append(&LedgerEvent::Heartbeat { task: hung.clone(), at: ledger::now() }).unwrap();
        assert_eq!(ledger.get(&hung).unwrap().state, TaskState::Running);

        let session = home.path().join("session.jsonl");
        std::fs::write(&session, "").unwrap();
        let waiting = add(&ledger, i32::MAX as u32 - 4, 10, Some(session));
        let manual = WatchdogSettings { resume: false, ..settings.clone() };
        let held = crate::round_lock::RoundLock::try_take(&paths.home.join("run/watchdog.lock")).unwrap();
        assert!(run_once(&paths, &manual).await.reports.is_empty(), "another round holds the lock");
        drop(held);
        let reports = run_once(&paths, &manual).await.reports;
        assert!(reports.iter().any(|(_, m)| m.contains(&waiting.0)), "{reports:?}");
    }

    #[tokio::test]
    async fn a_waiting_task_ends_when_its_schedule_fires_and_goes_to_the_user_when_it_is_cancelled() {
        use nypi_core::schedule::{Action, Firing, ScheduleEvent, Trigger};
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let ledger = Ledger::new(paths.ledger());
        let store = ScheduleStore::new(paths.schedules());
        let dead = i32::MAX as u32 - 5;
        let wait = |note: &str| {
            let action = Action::Prompt { text: "check the build".into() };
            let schedule =
                store.add(Trigger::At { at: "2099-01-01 09:00".into() }, action, "Lucia", home.path(), false).unwrap();
            let task = add(&ledger, dead, 600, None);
            let waiting = LedgerEvent::Waiting {
                task: task.clone(),
                at: ledger::now(),
                schedules: vec![schedule.clone()],
                note: note.into(),
                on: None,
            };
            ledger.append(&waiting).unwrap();
            (task, schedule)
        };
        let (pending, _) = wait("Continuing: back when the build ends");
        let (fired, fired_schedule) = wait("Continuing: back at nine");
        let (cancelled, cancelled_schedule) = wait("Continuing: back after the review");
        let settings = WatchdogSettings::default();
        assert!(
            run_once(&paths, &settings).await.reports.is_empty(),
            "every schedule is pending: the scheduler owns it"
        );

        let schedule = store.get(&fired_schedule).unwrap();
        store.record_fired(&Firing { schedule, observed: None, at: chrono::Utc::now() }).unwrap();
        let gone = ScheduleEvent::Cancelled { id: cancelled_schedule.clone(), at: ledger::now(), by: "user".into() };
        store.append(&gone).unwrap();
        let reports = run_once(&paths, &settings).await.reports;
        assert_eq!(
            reports.len(),
            1,
            "a fired schedule reports for itself; a cancelled one needs the user: {reports:?}"
        );
        assert!(reports[0].1.starts_with("Blocked:") && reports[0].1.contains(&cancelled.0), "{reports:?}");
        assert!(reports[0].1.contains("back after the review") && reports[0].1.contains(&cancelled_schedule));
        assert_eq!(ledger.get(&pending).unwrap().state, TaskState::Waiting);
        let done = ledger.get(&fired).unwrap();
        assert_eq!(done.state, TaskState::Done);
        assert!(done.note.contains(&fired_schedule), "{}", done.note);
        assert_eq!(ledger.get(&cancelled).unwrap().state, TaskState::BlockedOnUser);
        assert!(run_once(&paths, &settings).await.reports.is_empty(), "each finding is reported once");
    }

    #[tokio::test]
    async fn a_task_waiting_on_a_specialist_is_left_alone_while_it_works_and_goes_to_the_user_when_the_report_is_lost()
    {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let ledger = Ledger::new(paths.ledger());
        let settings = WatchdogSettings::default();
        let child_of = |parent: &TaskId, pid: u32, state: TaskState| {
            let (task, at) = (new_task_id(), ledger::now());
            ledger
                .append(&LedgerEvent::Created {
                    task: task.clone(),
                    at: at.clone(),
                    goal: "build the site".into(),
                    prompt: "build the site".into(),
                    owner: "builder".into(),
                    origin: "specialist".into(),
                    parent: Some(parent.clone()),
                    session_id: "external:builder".into(),
                    session_path: None,
                    cwd: "/".into(),
                    remote: None,
                    pid,
                    pulse: None,
                })
                .unwrap();
            ledger.append(&LedgerEvent::State { task: task.clone(), at, state, note: String::new() }).unwrap();
            task
        };
        let waiting_on = |pid: u32, delegation: &str| {
            let task = add(&ledger, pid, 1, None);
            let waiting = LedgerEvent::Waiting {
                task: task.clone(),
                at: ledger::now(),
                schedules: vec![delegation.into()],
                note: "Continuing: the builder is building the site".into(),
                on: None,
            };
            ledger.append(&waiting).unwrap();
            task
        };
        let (here, gone) = (std::process::id(), i32::MAX as u32 - 6);

        // Its specialist is still working, in a hub that is still there: nothing to do.
        let working = waiting_on(here, "d-working");
        child_of(&working, here, TaskState::Running);
        // Its specialist finished and the hub holds the report: the review waits its turn.
        let queued = waiting_on(here, "d-queued");
        child_of(&queued, here, TaskState::Done);
        // Its specialist finished, and the hub went away before the report was reviewed.
        let lost = waiting_on(gone, "d-lost");
        child_of(&lost, gone, TaskState::Done);

        let reports = run_once(&paths, &settings).await.reports;
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(reports[0].1.starts_with("Blocked:") && reports[0].1.contains(&lost.0), "{reports:?}");
        assert!(reports[0].1.contains("before Lucia reviewed it"), "{reports:?}");
        assert_eq!(ledger.get(&working).unwrap().state, TaskState::Waiting);
        assert_eq!(ledger.get(&queued).unwrap().state, TaskState::Waiting);
        assert_eq!(ledger.get(&lost).unwrap().state, TaskState::BlockedOnUser);
        assert!(run_once(&paths, &settings).await.reports.is_empty(), "said once");
    }
}
