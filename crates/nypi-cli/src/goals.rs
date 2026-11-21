//! `nypi goals`: show and edit the goals file by hand, and run a goal review
//! (nypi-core/src/goals.rs). Edits from here are versioned like the agent's.

use anyhow::bail;
use nypi_core::config::Paths;
use nypi_core::goals::{Change, GoalsFile, Term};

pub fn show(paths: &Paths) -> anyhow::Result<i32> {
    let file = GoalsFile::new(paths.goals());
    let text = file.read();
    if text.trim().is_empty() {
        nypi_hub::outln!("No goals yet ({}). Add one:", file.path.display());
        nypi_hub::outln!("  nypi goals add --term short \"Ship the M3 release\" --next \"write the release notes\"");
        return Ok(0);
    }
    nypi_hub::out!("{text}");
    let open = file.goals().iter().filter(|g| !g.done).count();
    nypi_hub::errln!("\n{open} open goal(s) · {}", file.path.display());
    Ok(0)
}

pub fn add(
    paths: &Paths,
    term: &str,
    text: &str,
    next: Option<String>,
    due: Option<String>,
    dir: Option<String>,
) -> anyhow::Result<i32> {
    let Some(term) = Term::parse(term) else { bail!("Unknown term {term}: use short, medium, or long") };
    let change = Change { next, due, dir, ..Default::default() };
    let id = GoalsFile::new(paths.goals()).add(term, text, &change, "added by the user")?;
    nypi_hub::outln!("Added {id} ({} term).", term.label());
    Ok(0)
}

pub fn set_done(paths: &Paths, id: &str, done: bool) -> anyhow::Result<i32> {
    let reason = if done { "marked done by the user" } else { "reopened by the user" };
    let goal = GoalsFile::new(paths.goals()).update(id, &Change { done: Some(done), ..Default::default() }, reason)?;
    nypi_hub::outln!("{} {}: {}", if done { "Done" } else { "Reopened" }, goal.id, goal.text);
    Ok(0)
}
