//! One job in a runtime of this process, run to its end and printed as it goes: what the
//! commands a person or cron runs by name use (`nypi tasks resume`, `nypi dmn run`, `nypi goals
//! review` without a daemon, `nypi specialists run`) and what `--json` prints from. A person's
//! own message never comes this way: it goes to a thread of the hub through an interface
//! (client.rs). Nor does work the hub finds to do by itself: that is a job of the hub (jobs.rs).

use nypi_core::inspector::Approval;
use nypi_core::runtime::{Job, Runtime, RuntimeEvent};
use tokio::sync::mpsc;

use crate::{bridge, notify, render};

/// Run one job and print the final answer (or every event with `--json`). Approval requests go
/// to the user through the daemon's Telegram channel and are denied without one; long runs and
/// stalls are reported to the notification channels.
pub(crate) async fn print_mode(
    runtime: &mut Runtime,
    job: Job,
    json: bool,
    show_thinking: bool,
) -> anyhow::Result<i32> {
    print_run(runtime, job, json, show_thinking).await.map(|(code, _)| code)
}

/// Print mode, also handing back the outcome (specialist runs record it in their experience).
pub(crate) async fn print_run(
    runtime: &mut Runtime,
    job: Job,
    json: bool,
    _show_thinking: bool,
) -> anyhow::Result<(i32, nypi_core::runtime::RunOutcome)> {
    if !json && !runtime.has_api_key() {
        eprintln!("warning: no API key found for {}", runtime.model().reference());
    }
    let notifier = notify::Notifier::from_runtime(runtime).map(std::sync::Arc::new);
    let stall_limit = std::time::Duration::from_secs(runtime.settings.notify.stall_seconds);
    let (tx, mut rx) = mpsc::unbounded_channel::<RuntimeEvent>();
    let mut alerts = runtime.take_alerts();
    let stall_notifier = notifier.clone();
    let stall_paths = runtime.paths.clone();
    let printer = tokio::spawn(async move {
        let mut progress = notify::Progress::default();
        let mut stall_check = tokio::time::interval(notify::STALL_CHECK_INTERVAL);
        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Some(event) => {
                        progress.observe(&event);
                        if json {
                            println!("{}", serde_json::to_string(&event).unwrap_or_default());
                        } else if let RuntimeEvent::Alert { alert } = &event {
                            eprintln!("[{:?}] {}", alert.level, alert.message);
                        } else if let RuntimeEvent::Retry { notice } = &event
                            && let Some(note) = render::retry_note(notice)
                        {
                            eprintln!("[{note}]");
                        }
                    }
                    None => break,
                },
                Some(alert) = crate::rpc::recv_alert(&mut alerts) => {
                    if json {
                        println!("{}", serde_json::to_string(&RuntimeEvent::Alert { alert }).unwrap_or_default());
                    } else {
                        eprintln!("[{:?}] {}", alert.level, alert.message);
                    }
                }
                _ = stall_check.tick() => {
                    if let Some(text) = progress.stall(stall_limit) {
                        eprintln!("[{text}]");
                        if crate::unattended() {
                            let level = nypi_core::inspector::AlertLevel::Warning;
                            let notice = notify::Notice { source: "run", level, text: &text, task: None, shown_in: None };
                            notify::post(&stall_paths, stall_notifier.as_deref(), notice).await;
                        } else if let Some(notifier) = &stall_notifier {
                            notifier.send(&text).await;
                        }
                    }
                }
            }
        }
    });
    // Nobody can answer here, but a running Telegram bridge can ask the user.
    let mut approvals = runtime.enable_approvals();
    let socket = runtime.paths.bridge_socket();
    let origin = format!("print mode · {}", notify::place(&runtime.cwd));
    tokio::spawn(async move {
        while let Some(approval) = approvals.recv().await {
            let Approval::Asked(request) = approval else { continue };
            eprintln!("[approval] {}: {} (asking through the Telegram bridge)", request.tool, request.summary);
            let (socket, origin) = (socket.clone(), origin.clone());
            tokio::spawn(async move {
                let listened = request.listened;
                if let Some(unanswered) = bridge::forward(socket, origin, request).await {
                    // The board shows it and can answer: the Inspector waits for that, or times out.
                    if listened {
                        eprintln!("[approval] waiting for an answer on the board");
                        return;
                    }
                    eprintln!("[approval] denied: the Telegram bridge is not running");
                    unanswered.deny(Some(
                        "approval is required, but no one can answer: this is print mode and the Telegram bridge \
                         is not running"
                            .into(),
                    ));
                }
            });
        }
    });
    // A caller that named its channel (a specialist run) keeps it.
    if runtime.channel() == "cli" {
        runtime.set_channel("print");
    }
    let prompt = match &job {
        Job::Prompt(text) | Job::Sourced { text, .. } => text.clone(),
        Job::Resume(task) => format!("(resume {task})"),
    };
    runtime.audit("cli", serde_json::json!({ "mode": if json { "json" } else { "print" }, "prompt": prompt }));
    let started = std::time::Instant::now();
    let outcome = runtime.run_job(&job, &tx).await?;
    drop(tx);
    let _ = printer.await;
    if !json {
        if !outcome.answer.is_empty() {
            println!("{}", outcome.answer.trim_end());
            if let Some(sources) = outcome.citations.footer() {
                println!("\n{sources}");
            }
        }
        if outcome.status.synthesized {
            eprintln!("status: {}", outcome.status);
        }
        if let Some(error) = &outcome.error {
            eprintln!("error: {error}");
        }
    }
    // A caller that reports for itself (a DMN pass) sets the threshold out of reach.
    let after = runtime.settings.notify.after_seconds;
    let report = notify::run_report(&outcome, started.elapsed());
    if crate::unattended() && after != u64::MAX {
        // Nobody is watching: however short the run, its result goes out, outbox first.
        let notice = notify::Notice {
            source: "run",
            level: notify::level_of(&outcome),
            text: &report,
            task: outcome.task.as_ref(),
            shown_in: None,
        };
        notify::post(&runtime.paths, notifier.as_deref(), notice).await;
    } else if let Some(notifier) = &notifier
        && started.elapsed() >= std::time::Duration::from_secs(after)
    {
        notifier.send(&report).await;
    }
    // The process is about to exit; give a reflection a minute to save what it learned.
    for summary in runtime.wait_reflections(std::time::Duration::from_secs(60)).await {
        eprintln!("{summary}");
    }
    Ok((if outcome.error.is_some() { 1 } else { 0 }, outcome))
}
