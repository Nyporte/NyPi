//! `nypi eval`: run the offline benchmark (nypi-eval, decisions D78 and D90) with the user's
//! model and print what was measured. Each scenario runs in a temporary NyPi home and folder of
//! its own, against a server on the loopback address; of the user's home only the keys, the
//! model list, and the model settings are read, and nothing in it is written. The model's own
//! requests are the only thing that leaves the machine, and they cost what such requests cost.
//! With `--three-planes` each scenario goes through chat, the Principal, and the task pool.

use nypi_core::config::Paths;
use nypi_eval::{EvalOptions, Suite, catalog};

/// What `nypi eval` was asked for.
pub struct Options {
    pub model: Option<String>,
    pub three_planes: bool,
    pub chat_model: Option<String>,
    pub principal_model: Option<String>,
    pub suite: Option<String>,
}

pub fn list() -> anyhow::Result<i32> {
    for (name, suite) in [("worker", Suite::Worker), ("planes", Suite::Planes)] {
        nypi_hub::outln!("Suite {name}:");
        for scenario in catalog(suite) {
            nypi_hub::outln!("  {:<32} {}", scenario.id, scenario.hurdle);
        }
    }
    nypi_hub::outln!("\nWithout --suite: worker, or all with --three-planes.");
    Ok(0)
}

/// Exit code 0 when every scenario run passed, 1 otherwise.
pub async fn run(paths: &Paths, ids: &[String], asked: Options, json: bool) -> anyhow::Result<i32> {
    let suite = match asked.suite.as_deref() {
        Some(name) => {
            Suite::parse(name).ok_or_else(|| anyhow::anyhow!("--suite is worker, planes, or all, not {name}"))?
        }
        None if asked.three_planes => Suite::All,
        None => Suite::Worker,
    };
    let options = EvalOptions {
        model: asked.model,
        borrow_from: Some(paths.home.clone()),
        three_planes: asked.three_planes,
        chat_model: asked.chat_model,
        principal_model: asked.principal_model,
        suite,
        ..Default::default()
    };
    let report = nypi_eval::run(ids, &options, |result| {
        if !json {
            let mark = if result.passed { "pass" } else { "FAIL" };
            nypi_hub::errln!("{mark}  {} ({}, {:.0} s)", result.id, result.status, result.seconds);
        }
    })
    .await?;
    match json {
        true => nypi_hub::outln!("{}", serde_json::to_string_pretty(&report)?),
        false => nypi_hub::outln!("\n{}", nypi_eval::render(&report)),
    }
    Ok(if report.results.iter().all(|result| result.passed) { 0 } else { 1 })
}
