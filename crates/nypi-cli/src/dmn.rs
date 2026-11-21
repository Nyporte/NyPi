//! `nypi dmn`: the default mode network (nypi-core/src/dmn.rs). Shows whether a pass is due and
//! what recent passes cost and said; `nypi dmn run` runs one pass now as a task with source
//! `dmn`. A pass that is due is a job of the hub (jobs.rs), ordered by the scheduler's round;
//! either way it is recorded here, and its note (if any) goes to the notification channels.

use chrono::Utc;
use nypi_ai::Message;
use nypi_core::config::{Paths, Settings};
use nypi_core::dmn::{PassLog, Why, due};
use nypi_core::pulse::PulseSource;
use nypi_core::runtime::{Job, Runtime};

use nypi_hub::idle_pass::{Ran, record, use_own_model};

use crate::notify::Notifier;

pub fn status(paths: &Paths, settings: &Settings) -> anyhow::Result<i32> {
    let now = Utc::now();
    match due(&settings.dmn, paths, now) {
        Ok(why) => nypi_hub::outln!("A pass is due ({why:?})."),
        Err(why) => nypi_hub::outln!("No pass now: {why}."),
    }
    let log = PassLog::new(paths.dmn_passes());
    let (passes, tokens) = log.today(now);
    nypi_hub::outln!(
        "Today: {passes} of {} passes, {tokens} of {} tokens.",
        settings.dmn.max_passes_per_day,
        settings.dmn.daily_tokens
    );
    for pass in log.load().iter().rev().take(5) {
        let note =
            pass.note.as_deref().map(|n| format!(": {}", n.lines().next().unwrap_or_default())).unwrap_or_default();
        nypi_hub::outln!(
            "  {} {:?} → {:?}, {} tokens, ${:.4}{note}",
            pass.at.get(..16).unwrap_or_default(),
            pass.why,
            pass.outcome,
            pass.tokens,
            pass.cost
        );
    }
    if !settings.dmn.enabled {
        nypi_hub::outln!("Turn it on with {{ \"dmn\": {{ \"enabled\": true }} }} in ~/.nypi/settings.json.");
    }
    Ok(0)
}

/// `nypi dmn run`: one pass now, in this process, printed as it goes.
pub async fn run(mut runtime: Runtime, why: Why, json: bool) -> anyhow::Result<i32> {
    runtime.set_channel("dmn");
    if let Some(warning) = use_own_model(&mut runtime) {
        nypi_hub::errln!("{warning}");
    }
    let paths = runtime.paths.clone();
    let notifier = Notifier::from_runtime(&runtime);
    let agent = runtime.settings.agent_name.clone();
    // print_run reports long runs; a pass reports only its note.
    runtime.settings.notify.after_seconds = u64::MAX;
    let now = Utc::now();
    let vault = nypi_core::dmn::consolidate(&paths, runtime.settings.vault.archive_after_days, now);
    let text = nypi_core::dmn::build_prompt_with(&paths, now, &vault);
    let (code, outcome) = crate::run_once::print_run(
        &mut runtime,
        Job::Sourced { text, source: PulseSource::Dmn, cause: None },
        json,
        false,
    )
    .await?;
    let (mut tokens, mut cost) = (0, 0.0);
    for message in &outcome.new_messages {
        if let Message::Assistant(a) = message {
            tokens += a.usage.input + a.usage.output + a.usage.cache_read;
            cost += a.usage.cost.total;
        }
    }
    let ran =
        Ran { error: outcome.error.clone(), answer: outcome.answer.clone(), tokens, cost, task: outcome.task.clone() };
    record(&paths, &agent, notifier.as_ref(), why, ran).await?;
    Ok(code)
}
