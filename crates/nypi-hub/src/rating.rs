//! The user's verdict on a task's result, as a conversation takes it (`/rate`, decision D63):
//! which results can be judged, and the command's answer. `nypi rate`, which walks through the
//! tasks one key each, is the command line's (nypi-cli/src/rate.rs).

use nypi_core::config::Paths;
use nypi_core::ledger::{Ledger, TaskId, TaskRecord, Verdict};

/// A result the user can judge: the run is over, or it was cut off and nothing picked it up.
pub fn ratable(task: &TaskRecord) -> bool {
    task.parent.is_none() && (task.is_settled() || task.is_interrupted())
}

pub fn one_line(text: &str, chars: usize) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(chars).collect()
}

pub fn state(task: &TaskRecord) -> &'static str {
    if task.is_interrupted() { "interrupted" } else { task.state.label() }
}

/// What a front end says after a result worth judging.
pub fn hint() -> String {
    format!("How was this? /rate {}", Verdict::choices())
}

/// `/rate [task] <verdict> [note]` inside a session: without a task, the session's latest
/// result. Returns what to tell the user.
pub fn in_session(paths: &Paths, session_id: &str, arg: &str) -> String {
    let ledger = Ledger::new(paths.ledger());
    let mut words = arg.split_whitespace().peekable();
    let named = words.next_if(|word| word.starts_with("t-")).map(|id| TaskId(id.to_string()));
    let task = match &named {
        Some(id) => ledger.get(id),
        None => ledger
            .load()
            .into_values()
            .filter(|task| task.session_id == session_id && ratable(task))
            .max_by(|a, b| a.created.cmp(&b.created)),
    };
    let Some(task) = task else {
        return match named {
            Some(id) => format!("No task {id} in the ledger."),
            None => "Nothing in this session has finished yet. nypi rate goes through earlier tasks.".into(),
        };
    };
    let usage = format!("/rate [task] <verdict> [note]   ({})", Verdict::choices());
    let Some(word) = words.next() else {
        let rated = match &task.rating {
            Some(rating) => format!("rated {}", rating.verdict.label()),
            None => "no verdict yet".into(),
        };
        return format!(
            "Latest result: {} ({}), {rated}: {}\n{usage}",
            task.id,
            state(&task),
            one_line(&task.goal, 80)
        );
    };
    let Some(verdict) = Verdict::parse(word) else {
        return format!("Unknown verdict: {word}.\n{usage}");
    };
    let note = words.collect::<Vec<_>>().join(" ");
    match ledger.rate(&task.id, verdict, &note) {
        Ok(()) => format!("Rated {} {}: {}", task.id, verdict.label(), one_line(&task.goal, 80)),
        Err(error) => format!("Not rated: the ledger could not be written ({error})."),
    }
}
