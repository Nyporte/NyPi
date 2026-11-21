//! `nypi rate` and `/rate`: the user's verdict on a task's result (decision D63), one key per
//! task. A verdict is a ledger event, and `nypi scorecard` judges NyPi on them. With a task and
//! a verdict the command records it; with nothing it walks through the tasks that have none
//! yet, so a week's work can be judged in a minute.

use std::io::{IsTerminal, Write};

use nypi_core::config::Paths;
use nypi_core::ledger::{Ledger, TaskId, TaskRecord, Verdict};
pub use nypi_hub::rating::hint;
use nypi_hub::rating::{one_line, ratable, state};

/// How far back the walk goes when nothing says otherwise.
pub const REVIEW_DAYS: i64 = 14;

fn local(at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|t| t.with_timezone(&chrono::Local).format("%a %-d %b %H:%M").to_string())
        .unwrap_or_else(|_| "?".into())
}

/// Top-level tasks of the last `days` days that can be judged and have no verdict, oldest first.
pub fn unrated(ledger: &Ledger, days: i64) -> Vec<TaskRecord> {
    let since = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    let mut tasks: Vec<TaskRecord> = ledger
        .load()
        .into_values()
        .filter(|task| ratable(task) && task.rating.is_none() && task.created.as_str() >= since.as_str())
        .collect();
    tasks.sort_by(|a, b| a.created.cmp(&b.created));
    tasks
}

/// `nypi rate <task> <verdict> [note]`.
pub fn rate(paths: &Paths, id: &str, verdict: &str, note: &str) -> anyhow::Result<i32> {
    let ledger = Ledger::new(paths.ledger());
    let Some(task) = ledger.get(&TaskId(id.to_string())) else {
        nypi_hub::errln!("No task {id} in the ledger. nypi rate lists the ones without a verdict.");
        return Ok(1);
    };
    let Some(verdict) = Verdict::parse(verdict) else {
        nypi_hub::errln!("Unknown verdict: {verdict}. Use one of: {}.", Verdict::choices());
        return Ok(1);
    };
    ledger.rate(&task.id, verdict, note)?;
    nypi_hub::outln!("Rated {} {}: {}", task.id, verdict.label(), one_line(&task.goal, 80));
    Ok(0)
}

/// What the walk did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Walked {
    pub rated: usize,
    pub skipped: usize,
}

/// Show each task and take one key for it: a verdict's key rates it, `q` (or no key at all)
/// stops, anything else leaves it for later.
pub fn walk(
    ledger: &Ledger,
    tasks: &[TaskRecord],
    mut key: impl FnMut() -> Option<char>,
    out: &mut impl Write,
) -> std::io::Result<Walked> {
    let mut walked = Walked::default();
    for (index, task) in tasks.iter().enumerate() {
        writeln!(
            out,
            "\n{}/{}  {}  {}  {}  {}",
            index + 1,
            tasks.len(),
            task.id,
            state(task),
            local(&task.created),
            task.origin
        )?;
        writeln!(out, "  Asked:  {}", one_line(&task.goal, 200))?;
        if !task.note.is_empty() {
            writeln!(out, "  Result: {}", one_line(&task.note, 300))?;
        }
        write!(out, "  {} · Enter skips · q stops > ", Verdict::choices())?;
        out.flush()?;
        let pressed = key();
        match pressed.and_then(|c| Verdict::parse(&c.to_string())) {
            Some(verdict) => {
                ledger.rate(&task.id, verdict, "")?;
                walked.rated += 1;
                writeln!(out, "{}", verdict.label())?;
            }
            None if matches!(pressed, None | Some('q' | 'Q')) => {
                writeln!(out, "stopped")?;
                break;
            }
            None => {
                walked.skipped += 1;
                writeln!(out, "skipped")?;
            }
        }
    }
    Ok(walked)
}

/// One key from the terminal, without waiting for Enter. None when the terminal is gone.
fn read_key() -> Option<char> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    /// Leaves raw mode however the read ends.
    struct Raw;
    impl Drop for Raw {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    crossterm::terminal::enable_raw_mode().ok()?;
    let _raw = Raw;
    loop {
        let Event::Key(key) = read().ok()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        return Some(match key.code {
            KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => 'q',
            KeyCode::Char(c) => c,
            KeyCode::Esc => 'q',
            _ => '\n',
        });
    }
}

/// `nypi rate` with nothing named: walk through what has no verdict yet. Without a terminal
/// there are no keys to take, so the tasks are listed with the command that rates each.
pub fn review(paths: &Paths, days: i64) -> anyhow::Result<i32> {
    let ledger = Ledger::new(paths.ledger());
    let tasks = unrated(&ledger, days);
    if tasks.is_empty() {
        nypi_hub::outln!("Every task of the last {days} days that can be judged has a verdict.");
        return Ok(0);
    }
    // Run for another machine (decision D96), it lists: that machine asks.
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive || nypi_hub::output::capturing() {
        for task in &tasks {
            nypi_hub::outln!(
                "{}  {:<11}  {}  {}",
                task.id,
                state(task),
                local(&task.created),
                one_line(&task.goal, 90)
            );
        }
        nypi_hub::outln!("\nRate one with: nypi rate <task> <verdict> [note]   ({})", Verdict::choices());
        return Ok(0);
    }
    nypi_hub::outln!("{} task(s) of the last {days} days have no verdict. One key each.", tasks.len());
    let walked = walk(&ledger, &tasks, read_key, &mut std::io::stdout())?;
    let left = tasks.len() - walked.rated;
    nypi_hub::outln!("\nRated {}, {} left. The week so far: nypi scorecard", walked.rated, left);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use nypi_core::ledger::{LedgerEvent, TaskState, new_task_id, now};

    use super::*;
    use nypi_hub::rating::in_session;

    /// A pid that cannot exist stands for a process that is gone.
    const DEAD: u32 = i32::MAX as u32 - 9;

    fn add(ledger: &Ledger, session: &str, goal: &str, state: TaskState, parent: Option<&TaskId>) -> TaskId {
        let task = new_task_id();
        ledger
            .append(&LedgerEvent::Created {
                task: task.clone(),
                at: now(),
                goal: goal.into(),
                prompt: goal.into(),
                owner: "lucia".into(),
                origin: "cli".into(),
                parent: parent.cloned(),
                session_id: session.into(),
                session_path: None,
                cwd: "/".into(),
                remote: None,
                pid: DEAD,
                pulse: None,
            })
            .unwrap();
        let note = format!("{}: {goal}", state.label());
        ledger.append(&LedgerEvent::State { task: task.clone(), at: now(), state, note }).unwrap();
        task
    }

    fn ledger() -> (tempfile::TempDir, Paths, Ledger) {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let ledger = Ledger::new(paths.ledger());
        (home, paths, ledger)
    }

    fn verdict(ledger: &Ledger, task: &TaskId) -> Option<Verdict> {
        ledger.get(task).unwrap().rating.map(|rating| rating.verdict)
    }

    #[test]
    fn the_walk_takes_one_key_per_task_and_stops_when_told() {
        let (_home, _paths, ledger) = ledger();
        let report = add(&ledger, "s1", "write the report", TaskState::Done, None);
        let sub = add(&ledger, "s2", "a specialist's part", TaskState::Done, Some(&report));
        let deploy = add(&ledger, "s1", "deploy", TaskState::Failed, None);
        let cut_off = add(&ledger, "s1", "import the data", TaskState::Running, None);
        let later = add(&ledger, "s1", "tidy up", TaskState::Done, None);
        let last = add(&ledger, "s1", "never reached", TaskState::Done, None);
        let waiting = add(&ledger, "s1", "waits on a schedule", TaskState::Waiting, None);

        let tasks = unrated(&ledger, REVIEW_DAYS);
        let offered: Vec<&TaskId> = tasks.iter().map(|task| &task.id).collect();
        assert_eq!(offered, [&report, &deploy, &cut_off, &later, &last], "no sub-tasks, nothing still under way");

        let mut keys = "gW\nsq".chars();
        let mut shown = Vec::new();
        let walked = walk(&ledger, &tasks, || keys.next(), &mut shown).unwrap();
        assert_eq!(walked, Walked { rated: 3, skipped: 1 });
        assert_eq!(verdict(&ledger, &report), Some(Verdict::Good));
        assert_eq!(verdict(&ledger, &deploy), Some(Verdict::Wrong));
        assert_eq!(verdict(&ledger, &cut_off), None, "Enter leaves it for later");
        assert_eq!(verdict(&ledger, &later), Some(Verdict::Silent));
        assert_eq!(verdict(&ledger, &last), None, "q stops before it");
        assert_eq!((verdict(&ledger, &sub), verdict(&ledger, &waiting)), (None, None));
        let shown = String::from_utf8(shown).unwrap();
        assert!(shown.contains("interrupted") && shown.contains("Result: failed: deploy"), "{shown}");
        assert!(shown.contains("g good · f fixed · w wrong · s silent"));

        let left: Vec<TaskId> = unrated(&ledger, REVIEW_DAYS).into_iter().map(|task| task.id).collect();
        assert_eq!(left, [cut_off, last], "rated tasks are not offered again");
        // A terminal that goes away ends the walk without a verdict.
        assert_eq!(walk(&ledger, &tasks, || None, &mut Vec::new()).unwrap(), Walked::default());
        assert!(unrated(&ledger, -1).is_empty(), "older than the window");
    }

    #[test]
    fn a_session_rates_its_latest_result_or_a_named_task() {
        let (_home, paths, ledger) = ledger();
        assert!(in_session(&paths, "s1", "g").starts_with("Nothing in this session has finished yet"));
        let first = add(&ledger, "s1", "first", TaskState::Done, None);
        let second = add(&ledger, "s1", "second\nwith a line break", TaskState::BlockedOnUser, None);
        let other = add(&ledger, "s2", "another session", TaskState::Done, None);
        let _running = add(&ledger, "s1", "still under way", TaskState::Waiting, None);

        let shown = in_session(&paths, "s1", "");
        assert!(shown.contains(&second.0) && shown.contains("no verdict yet"), "{shown}");
        assert_eq!(
            in_session(&paths, "s1", "F had to tell it twice"),
            format!("Rated {second} fixed: second with a line break")
        );
        let rating = ledger.get(&second).unwrap().rating.unwrap();
        assert_eq!((rating.verdict, rating.note.as_str()), (Verdict::Fixed, "had to tell it twice"));
        assert_eq!(verdict(&ledger, &first), None);

        assert!(in_session(&paths, "s1", &format!("{first} wrong")).starts_with(&format!("Rated {first} wrong")));
        assert!(
            in_session(&paths, "s1", &format!("{other} g")).contains("another session"),
            "a named task, any session"
        );
        assert!(in_session(&paths, "s1", "great").starts_with("Unknown verdict: great."));
        assert_eq!(in_session(&paths, "s1", "t-missing g"), "No task t-missing in the ledger.");
        assert!(in_session(&paths, "s1", "").contains("rated fixed"));
        assert_eq!(verdict(&ledger, &second), Some(Verdict::Fixed), "asking changes nothing");
    }

    #[test]
    fn the_command_names_what_it_rated_and_refuses_what_it_cannot_read() {
        let (_home, paths, ledger) = ledger();
        let task = add(&ledger, "s1", "book the room", TaskState::Done, None);
        assert_eq!(rate(&paths, &task.0, "good", "").unwrap(), 0);
        assert_eq!(verdict(&ledger, &task), Some(Verdict::Good));
        assert_eq!(rate(&paths, &task.0, "ok", "").unwrap(), 1);
        assert_eq!(rate(&paths, "t-none", "good", "").unwrap(), 1);
        assert_eq!(verdict(&ledger, &task), Some(Verdict::Good));
        assert!(hint().contains("/rate g good"));
    }
}
