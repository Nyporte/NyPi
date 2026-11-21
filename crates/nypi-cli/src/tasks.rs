//! `nypi tasks`: the task ledger for people. Lists open (or all) tasks, shows one in full, and
//! checks that a task can be resumed before `nypi tasks resume` opens its session.

use anyhow::bail;
use nypi_core::config::Paths;
use nypi_core::ledger::{Ledger, LedgerEvent, RESTART_LIMIT, TaskId, TaskRecord, TaskState};

pub fn list(paths: &Paths, all: bool) -> anyhow::Result<i32> {
    let ledger = Ledger::new(paths.ledger());
    let mut tasks: Vec<TaskRecord> = ledger.load().into_values().collect();
    tasks.sort_by(|a, b| a.created.cmp(&b.created));
    let shown: Vec<&TaskRecord> = tasks.iter().filter(|t| all || !t.state.is_terminal()).collect();
    if shown.is_empty() {
        let scope = if all { "" } else { " open" };
        nypi_hub::outln!("No{scope} tasks in {}.", ledger.path().display());
        if !all && !tasks.is_empty() {
            nypi_hub::outln!("{} finished task(s); nypi tasks --all lists them.", tasks.len());
        }
        return Ok(0);
    }
    for task in shown.iter().rev().take(50).rev() {
        nypi_hub::outln!("{}", line(task));
    }
    if shown.iter().any(|t| t.is_interrupted()) {
        nypi_hub::outln!(
            "\nInterrupted tasks resume when their session is opened again, or with: nypi tasks resume <id>"
        );
    }
    Ok(0)
}

fn line(task: &TaskRecord) -> String {
    let state = if task.is_interrupted() { "interrupted".to_string() } else { task.state.label().to_string() };
    let goal: String = task.goal.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(90).collect();
    format!("{}  {:<11}  {}  {:<8}  {goal}", task.id, state, local(&task.heartbeat_at), task.origin)
}

fn local(at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| "?".into())
}

pub fn show(paths: &Paths, id: &str) -> anyhow::Result<i32> {
    let Some(task) = Ledger::new(paths.ledger()).get(&TaskId(id.to_string())) else {
        nypi_hub::errln!("No task {id} in the ledger.");
        return Ok(1);
    };
    let state =
        if task.is_interrupted() { format!("{} (interrupted)", task.state.label()) } else { task.state.label().into() };
    nypi_hub::outln!("Task {}  {state}", task.id);
    nypi_hub::outln!("Goal: {}", task.goal);
    nypi_hub::outln!("Owner: {} · origin {} · attempts {}/{RESTART_LIMIT}", task.owner, task.origin, task.attempts);
    nypi_hub::outln!("Created {} · last heartbeat {}", local(&task.created), local(&task.heartbeat_at));
    if let Some(parent) = &task.parent {
        nypi_hub::outln!("Part of task {parent}");
    }
    if let Some(goal) = &task.goal_id {
        nypi_hub::outln!("Serves goal {goal} (nypi goals)");
    }
    if let Some(activation) = &task.spawned_by_activation {
        nypi_hub::outln!("Started by the Principal's activation {activation}");
    }
    if let Some(workspace) = &task.workspace {
        nypi_hub::outln!("Workspace: {workspace}/ in the vault");
    }
    nypi_hub::outln!("Directory: {}", task.cwd.display());
    match &task.session_path {
        Some(path) => nypi_hub::outln!("Session: {}", path.display()),
        None => nypi_hub::outln!("Session: not saved (cannot be resumed)"),
    }
    if let Some(pulse) = &task.pulse {
        nypi_hub::outln!("Trace: nypi trace {pulse}");
    }
    if !task.note.is_empty() {
        nypi_hub::outln!("Status: {}", task.note);
    }
    if !task.wake.is_empty() {
        nypi_hub::outln!("Waiting on schedule {} to continue (nypi schedule lists them)", task.wake.join(", "));
    }
    for tool in &task.open_tools {
        nypi_hub::outln!("Cut off while running: {} {}", tool.tool, tool.summary);
    }
    match &task.rating {
        Some(rating) if rating.note.is_empty() => nypi_hub::outln!("Your verdict: {}", rating.verdict.label()),
        Some(rating) => nypi_hub::outln!("Your verdict: {} ({})", rating.verdict.label(), rating.note),
        None if task.is_settled() => nypi_hub::outln!("No verdict yet: nypi rate {} <verdict>", task.id),
        None => {}
    }
    Ok(0)
}

pub fn cancel(paths: &Paths, id: &str) -> anyhow::Result<i32> {
    let ledger = Ledger::new(paths.ledger());
    let Some(task) = ledger.get(&TaskId(id.to_string())) else {
        nypi_hub::errln!("No task {id} in the ledger.");
        return Ok(1);
    };
    if task.state.is_terminal() {
        nypi_hub::outln!("Task {id} is already {}.", task.state.label());
        return Ok(0);
    }
    ledger.append(&LedgerEvent::State {
        task: task.id.clone(),
        at: nypi_core::ledger::now(),
        state: TaskState::Cancelled,
        note: "cancelled by the user".into(),
    })?;
    nypi_hub::outln!("Cancelled task {id}.");
    if !task.is_interrupted() && nypi_core::ledger::process_alive(task.pid) && task.pid != std::process::id() {
        nypi_hub::outln!("Its process (pid {}) is still running; stop the run there (Ctrl-C or /abort).", task.pid);
    }
    Ok(0)
}

/// The task, if `nypi tasks resume` can continue it.
pub fn resumable(paths: &Paths, id: &str) -> anyhow::Result<TaskRecord> {
    let Some(task) = Ledger::new(paths.ledger()).get(&TaskId(id.to_string())) else {
        bail!("No task {id} in the ledger")
    };
    if !task.is_interrupted() {
        bail!("Task {id} is {} and was not interrupted; nothing to resume", task.state.label());
    }
    match &task.session_path {
        Some(path) if path.exists() => Ok(task),
        Some(path) => bail!("Task {id}'s session file {} no longer exists", path.display()),
        None => bail!("Task {id} ran without a saved session, so it cannot be resumed"),
    }
}
