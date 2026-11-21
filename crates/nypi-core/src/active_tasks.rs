//! The `<active_tasks>` section of the prompt (decision D79, in place of D66's "elsewhere"):
//! what else Lucia is working on right now, and what recently ended, read from the task ledger.
//! The hub holds one conversation, but work goes on in several places at once: a task beside
//! this one, a job a schedule started, a run in another program. This is how whoever answers
//! knows of all of it, with where each task works and where its notes are.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::ledger::{TaskRecord, TaskState};

/// Finished tasks older than this are not mentioned.
const RECENT_HOURS: i64 = 6;
/// At most this many lines, so a busy day does not crowd the prompt.
const MAX_LINES: usize = 8;
const GOAL_CHARS: usize = 110;

const INTRO: &str = "Your other work: tasks you have under way besides this one (beside it, in other folders, on \
other devices, started by a schedule), and what recently ended. They share your memory, vault, goals, and schedules, \
but not this run's messages. Use this to answer questions about that work and to avoid doing it twice; a task's \
workspace, where it has one, holds its brief and findings. Do not continue their work from here unless asked.";

fn one_line(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= GOAL_CHARS {
        return line;
    }
    format!("{}…", line.chars().take(GOAL_CHARS).collect::<String>())
}

fn folder_name(cwd: &Path) -> String {
    cwd.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| cwd.display().to_string())
}

/// Where a task works, in a few words: the folder's name, and the device when it is another.
fn place(task: &TaskRecord) -> String {
    match &task.remote {
        Some(remote) => format!("{} on {}", folder_name(Path::new(&remote.folder)), remote.device),
        None => folder_name(&task.cwd),
    }
}

/// The section's text for the conversation `own_session`, or None when nothing is going on
/// elsewhere. `alive` says whether a process still exists.
pub fn section(
    tasks: &[TaskRecord],
    own_session: &str,
    now: DateTime<Utc>,
    alive: impl Fn(u32) -> bool,
) -> Option<String> {
    let age_hours = |at: &str| {
        DateTime::parse_from_rfc3339(at).map(|t| (now - t.with_timezone(&Utc)).num_hours()).unwrap_or(i64::MAX)
    };
    let clock = |at: &str| {
        DateTime::parse_from_rfc3339(at)
            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default()
    };
    let mut others: Vec<&TaskRecord> =
        tasks.iter().filter(|task| task.parent.is_none() && task.session_id != own_session).collect();
    // Newest first.
    others.sort_by(|a, b| b.heartbeat_at.cmp(&a.heartbeat_at));
    let mut lines = Vec::new();
    for task in &others {
        let goal = one_line(&task.goal);
        let (at, origin) = (place(task), &task.origin);
        let notes = task.workspace.as_ref().map(|workspace| format!(" [{workspace}/]")).unwrap_or_default();
        let line = match task.state {
            TaskState::Running | TaskState::Stalled | TaskState::Planned if alive(task.pid) => {
                format!("- working now in {at} ({origin}): {goal}{notes}")
            }
            TaskState::Waiting => {
                let on = match &task.waits {
                    Some(crate::ledger::WaitPredicate::AllOf { tokens }) => format!("{} to finish", tokens.len()),
                    Some(crate::ledger::WaitPredicate::AnyOf { tokens }) => format!("the first of {}", tokens.len()),
                    Some(crate::ledger::WaitPredicate::Approval { .. }) => "an approval".to_string(),
                    _ => "a schedule".to_string(),
                };
                format!("- waiting on {on} in {at}: {goal}{notes}")
            }
            TaskState::BlockedOnUser if age_hours(&task.heartbeat_at) < 24 => {
                format!("- waiting for the user in {at}: {goal} ({})", one_line(&task.note))
            }
            TaskState::Done | TaskState::Failed | TaskState::Cancelled
                if age_hours(&task.heartbeat_at) < RECENT_HOURS =>
            {
                let said = if task.note.is_empty() { task.state.label().to_string() } else { one_line(&task.note) };
                format!("- ended {} in {at}: {goal} ({said}){notes}", clock(&task.heartbeat_at))
            }
            _ => continue,
        };
        lines.push(line);
        if lines.len() == MAX_LINES {
            break;
        }
    }
    (!lines.is_empty()).then(|| format!("{INTRO}\n{}", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::TaskId;

    fn task(id: &str, session: &str, cwd: &str, state: TaskState, hours_ago: i64, now: DateTime<Utc>) -> TaskRecord {
        let at = (now - chrono::Duration::hours(hours_ago)).to_rfc3339();
        TaskRecord {
            id: TaskId(id.into()),
            goal: format!("goal of {id}"),
            prompt: String::new(),
            owner: "lucia".into(),
            origin: "hub".into(),
            parent: None,
            session_id: session.into(),
            session_path: None,
            cwd: cwd.into(),
            remote: None,
            pulse: None,
            created: at.clone(),
            heartbeat_at: at,
            state,
            note: if state == TaskState::Done { "Done: shipped".into() } else { String::new() },
            pid: 7,
            attempts: 1,
            open_tools: Vec::new(),
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
    fn a_thread_is_told_what_the_others_are_doing_and_what_just_ended() {
        let now = Utc::now();
        let mut blocked = task("t-ask", "s-site", "/code/site", TaskState::BlockedOnUser, 2, now);
        blocked.note = "Blocked: which branch?".into();
        let mut child = task("t-child", "s-site", "/code/site", TaskState::Running, 0, now);
        child.parent = Some(TaskId("t-run".into()));
        let mut joined = task("t-join", "s-phone", "/srv/app", TaskState::Waiting, 0, now);
        joined.remote = Some(crate::place::DeviceFolder { device: "mini".into(), folder: "/srv/app".into() });
        joined.workspace = Some("10-workspaces/261119-deploy".into());
        joined.waits = Some(crate::ledger::WaitPredicate::AllOf {
            tokens: vec![
                crate::ledger::WaitToken::Delegation("d-1".into()),
                crate::ledger::WaitToken::Delegation("d-2".into()),
            ],
        });
        let text = section(std::slice::from_ref(&joined), "s-here", now, |_| true).unwrap();
        assert_eq!(
            text.lines().nth(1).unwrap(),
            "- waiting on 2 to finish in app on mini: goal of t-join [10-workspaces/261119-deploy/]",
            "where it works, what it waits on, and where its notes are"
        );
        let tasks = vec![
            task("t-mine", "s-here", "/home/me", TaskState::Running, 0, now),
            task("t-run", "s-site", "/code/site", TaskState::Running, 0, now),
            child,
            blocked,
            task("t-done", "s-phone", "/home/me", TaskState::Done, 1, now),
            task("t-old", "s-phone", "/home/me", TaskState::Done, 30, now),
            task("t-dead", "s-api", "/code/api", TaskState::Running, 3, now),
        ];
        let alive = |pid: u32| pid == 7;
        let text = section(&tasks, "s-here", now, alive).unwrap();
        let lines: Vec<&str> = text.lines().skip(1).collect();
        assert!(text.starts_with("Your other work:"));
        assert_eq!(lines[0], "- working now in site (hub): goal of t-run");
        assert!(
            lines.iter().any(|l| l.starts_with("- ended ") && l.ends_with("in me: goal of t-done (Done: shipped)"))
        );
        assert!(lines.contains(&"- waiting for the user in site: goal of t-ask (Blocked: which branch?)"));
        assert!(lines.contains(&"- working now in api (hub): goal of t-dead"));
        assert_eq!(lines.len(), 4, "not this conversation's own task, a sub-task, or yesterday's: {lines:?}");

        // A process that is gone is not working; with nothing else to say there is no section.
        let only_dead = vec![task("t-dead", "s-api", "/code/api", TaskState::Running, 3, now)];
        assert_eq!(section(&only_dead, "s-here", now, |_| false), None);
        assert_eq!(section(&tasks[..1], "s-here", now, alive), None, "its own work is not elsewhere");

        let many: Vec<TaskRecord> =
            (0..20).map(|i| task(&format!("t-{i}"), "s-x", "/x", TaskState::Done, 1, now)).collect();
        assert_eq!(section(&many, "s-here", now, alive).unwrap().lines().count(), 1 + MAX_LINES);
    }
}
