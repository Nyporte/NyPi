//! `nypi specialists`: list, show, create, and run specialists (nypi-core/src/specialist.rs).
//! A native run is print mode under the specialist's profile; an external run streams the CLI's
//! progress to stderr and prints its answer.

use std::path::Path;

use anyhow::bail;
use nypi_core::config::Paths;
use nypi_core::runtime::{Job, Runtime, RuntimeOptions};
use nypi_core::specialist::{self, SpecialistEvent, SpecialistKind, Template};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub fn list(paths: &Paths, cwd: &Path) -> anyhow::Result<i32> {
    let (all, errors) = specialist::load_all(paths, cwd);
    if all.is_empty() {
        nypi_hub::outln!("No specialists yet. Create one, for example:\n  nypi specialists new coder --from codex");
    }
    for s in &all {
        let kind = match s.profile.kind {
            SpecialistKind::Native => "native".to_string(),
            SpecialistKind::External => format!("external: {}", s.profile.command.first().cloned().unwrap_or_default()),
        };
        nypi_hub::outln!("{:<16} {:<20} {}", s.name, kind, s.profile.description);
    }
    for error in errors {
        nypi_hub::errln!("warning: {error}");
    }
    Ok(0)
}

pub fn detect(paths: &Paths, cwd: &Path) -> anyhow::Result<i32> {
    let agents = specialist::detect_agents();
    if agents.is_empty() {
        nypi_hub::outln!(
            "No agent CLIs found (looked for Codex and Claude Code on PATH and inside their desktop apps)."
        );
    }
    for agent in &agents {
        let (label, template) = match agent.template {
            Template::Codex => ("Codex", "codex"),
            Template::Claude => ("Claude Code", "claude"),
            Template::Native => continue,
        };
        let state = match agent.signed_in {
            Some(true) => "signed in",
            Some(false) => "NOT signed in",
            None => "sign-in unknown",
        };
        nypi_hub::outln!(
            "{label} {} · {state}
  {}",
            agent.version,
            agent.program.display()
        );
        match agent.signed_in {
            Some(false) => {
                let login = if agent.template == Template::Claude { "auth login" } else { "login" };
                nypi_hub::outln!("  Sign in first: \"{}\" {login}", agent.program.display());
                nypi_hub::outln!("  Then: nypi specialists new <name> --from {template}");
            }
            _ => nypi_hub::outln!("  Use it: nypi specialists new <name> --from {template}"),
        }
    }
    // Profiles made earlier may point at an older copy, or one that is gone.
    let (all, _) = specialist::load_all(paths, cwd);
    for s in all.iter().filter(|s| s.profile.kind == SpecialistKind::External) {
        let program = s.profile.command.first().cloned().unwrap_or_default();
        let resolves = Path::new(&program).is_file()
            || std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(&program).is_file()));
        if !resolves {
            nypi_hub::outln!("Specialist {} runs {program}, which is not installed.", s.name);
            continue;
        }
        let newer = agents.iter().find(|a| {
            s.profile.format_is(a.template)
                && a.program.display().to_string() != program
                && !program.ends_with(&a.program.display().to_string())
        });
        if let Some(agent) = newer {
            nypi_hub::outln!(
                "Specialist {} runs {program}; a newer copy ({}) is at {}. Update `command` in {}.",
                s.name,
                agent.version,
                agent.program.display(),
                s.dir.join("profile.json").display()
            );
        }
    }
    Ok(0)
}

pub fn show(paths: &Paths, cwd: &Path, name: &str) -> anyhow::Result<i32> {
    let s = specialist::find(paths, cwd, name)?;
    nypi_hub::outln!("{} ({})", s.name, s.dir.display());
    nypi_hub::outln!("{}", serde_json::to_string_pretty(&s.profile)?);
    nypi_hub::outln!("\n{}", s.briefing());
    Ok(0)
}

pub fn new(paths: &Paths, name: &str, from: &str) -> anyhow::Result<i32> {
    let Some(template) = Template::parse(from) else { bail!("Unknown template {from}: use native, codex, or claude") };
    let dir = specialist::create(paths, name, template)?;
    nypi_hub::outln!("Created {}. Edit profile.json and instructions.md there, then:", dir.display());
    nypi_hub::outln!("  nypi specialists run {name} \"<task>\"");
    Ok(0)
}

pub async fn run(paths: &Paths, cwd: &Path, name: &str, task: &str, json: bool) -> anyhow::Result<i32> {
    if task.trim().is_empty() {
        bail!("Give the specialist a task: nypi specialists run {name} \"<task>\"");
    }
    let s = specialist::find(paths, cwd, name)?;
    if s.profile.kind == SpecialistKind::Native {
        let options = RuntimeOptions { paths: Some(paths.clone()), ..s.native_options(cwd, None)? };
        let mut runtime = Runtime::new(options)?;
        for error in runtime.connect_mcp().await {
            nypi_hub::errln!("warning: {error}");
        }
        runtime.set_task_owner(&s.name);
        runtime.set_channel("specialist");
        let (code, outcome) =
            crate::run_once::print_run(&mut runtime, Job::Prompt(task.to_string()), json, true).await?;
        if let Some(id) = &outcome.task
            && let Err(error) = s.record_experience(id, &outcome.status)
        {
            nypi_hub::errln!("warning: could not update the experience log: {error}");
        }
        return Ok(code);
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                SpecialistEvent::Tool { name, summary } => nypi_hub::errln!("[{name}] {}", one_line(&summary)),
                SpecialistEvent::ToolDone { name, is_error: true } => nypi_hub::errln!("[{name} failed]"),
                SpecialistEvent::Note(note) => nypi_hub::errln!("[note] {}", one_line(&note)),
                _ => {}
            }
        }
    });
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            stop.cancel();
        }
    });
    let outcome = specialist::run_external(&s, paths, cwd, task, None, &tx, cancel).await?;
    drop(tx);
    let _ = printer.await;
    if !outcome.answer.trim().is_empty() {
        nypi_hub::outln!("{}", outcome.answer.trim_end());
    }
    if outcome.status.synthesized {
        nypi_hub::errln!("status: {}", outcome.status);
    }
    nypi_hub::errln!("task {} · log {}", outcome.task, outcome.log.display());
    Ok(if outcome.status.kind == nypi_core::status::StatusKind::Failed { 1 } else { 0 })
}

fn one_line(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 160 { format!("{}…", line.chars().take(160).collect::<String>()) } else { line }
}
