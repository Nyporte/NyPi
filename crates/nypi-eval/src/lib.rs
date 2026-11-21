//! NyPi's offline benchmark (decision D78, vision.md section 6): what the agent reports is a
//! claim, so capability is measured by tasks whose outcome a program can check. Each scenario
//! puts one hurdle of the kind real work has in the way of a plain task (an endpoint that
//! answers 403, a README that lies, a tool that is not there, a payment only the owner may
//! make) and judges the run by what is on disk and what reached the scenario's own server
//! afterwards, not by the agent's status line.
//!
//! Everything runs in a temporary NyPi home and work folder, against a server on the loopback
//! address; nothing leaves the machine but the model's own requests. From the results come the
//! measures the redesign is held to: how often the task was really done unaided, how often the
//! agent stopped although a way existed (premature Blocked), how often it said Done when it was
//! not (false Done), how often it walked a dead end again, and whether it ever crossed a
//! boundary that is the user's.
//!
//! A scenario runs either on a worker alone, as the agent ran before D84, or through all three
//! planes (decision D90): chat gets the task as the user's message, may hand it to the
//! Principal, which starts and reviews tasks, and the run ends when every plane is quiet. The
//! same checks judge both, from the same disk and the same server.

pub mod planes;
pub mod scenarios;
pub mod server;

use std::path::PathBuf;
use std::time::Instant;

use nypi_ai::{ContentBlock, Message, Model, StreamFn};
use nypi_core::chat::{ChatOptions, ModuleEvent};
use nypi_core::config::Paths;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::status::StatusKind;
use serde::Serialize;
use serde_json::json;

use crate::server::{FaultServer, Hit, Respond};

/// The sandbox a scenario runs in.
pub struct Fixture {
    /// The temporary NyPi home.
    pub home: PathBuf,
    /// The folder the agent works in.
    pub work: PathBuf,
    /// The scenario's server, `http://127.0.0.1:<port>`.
    pub base: String,
}

impl Fixture {
    /// Write a file in the work folder, making its folders.
    pub fn write(&self, path: &str, content: impl AsRef<[u8]>) -> anyhow::Result<()> {
        let file = self.work.join(path);
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(file, content)?;
        Ok(())
    }

    /// Write an executable script in the work folder.
    pub fn script(&self, path: &str, content: &str) -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        self.write(path, content)?;
        std::fs::set_permissions(self.work.join(path), std::fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    /// A file of the work folder as text, empty when it is not there.
    pub fn read(&self, path: &str) -> String {
        std::fs::read(self.work.join(path))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }

    /// Run a shell command in the work folder: whether it succeeded, and what it printed. A
    /// command still running after `CHECK_SECONDS` is killed and counts as failed, so a script
    /// the agent left hanging cannot hang the check.
    pub fn sh(&self, command: &str) -> (bool, String) {
        let out = self.work.join(".check.out");
        let Ok(file) = std::fs::File::create(&out) else { return (false, String::new()) };
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.work)
            .stdin(std::process::Stdio::null())
            .stdout(file)
            .stderr(std::process::Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return (false, String::new()) };
        let deadline = Instant::now() + std::time::Duration::from_secs(CHECK_SECONDS);
        let ok = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.success(),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
            }
        };
        let printed = std::fs::read(&out).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()).unwrap_or_default();
        let _ = std::fs::remove_file(&out);
        (ok, printed)
    }

    /// A file's content without surrounding space: how a one-value answer file is read.
    pub fn value(&self, path: &str) -> String {
        self.read(path).trim().to_string()
    }
}

/// The longest a check's own command may run.
const CHECK_SECONDS: u64 = 60;

/// One tool call of a run.
#[derive(Clone, Debug)]
pub struct Call {
    pub tool: String,
    pub arguments: String,
    pub failed: bool,
}

/// What a run left behind, for a scenario's check.
pub struct RunRecord {
    pub status: StatusKind,
    pub answer: String,
    pub calls: Vec<Call>,
    /// References in the answer the harness verified, and ones that matched nothing.
    pub cited: usize,
    pub unverified: usize,
    /// Every request that reached the scenario's server, in order.
    pub hits: Vec<Hit>,
    /// What the planes did, when the run went through them.
    pub planes: Option<PlaneCounts>,
}

/// What the three planes did in a run (decision D90).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaneCounts {
    /// Chat's turns: answers to the user and messages it phrased for the Principal.
    pub chat_turns: u32,
    /// Times chat handed something to the Principal.
    pub signals_fired: u32,
    /// The Principal's activations that ran to a decision.
    pub activations: u32,
    /// Tasks the Principal started.
    pub tasks_started: u32,
    /// Times the Principal sent a task back with `continue_task`.
    pub review_rounds: u32,
}

impl RunRecord {
    /// Requests to `path` with `method`.
    pub fn hits_to(&self, method: &str, path: &str) -> usize {
        self.hits.iter().filter(|hit| hit.method == method && hit.path == path).count()
    }

    /// Requests to `path`, whatever the method.
    pub fn hits_at(&self, path: &str) -> usize {
        self.hits.iter().filter(|hit| hit.path == path).count()
    }

    pub fn called(&self, tool: &str) -> bool {
        self.calls.iter().any(|call| call.tool == tool)
    }
}

/// What a scenario's check found.
#[derive(Clone, Debug, Default)]
pub struct Verdict {
    /// The task is really done (or, where the task ends at the owner's boundary, really stopped there).
    pub passed: bool,
    /// Times an approach already shown not to work was tried again.
    pub dead_end_repeats: u32,
    /// Times something only the owner may do was done.
    pub authority_violations: u32,
    pub note: String,
}

impl Verdict {
    pub fn pass(note: impl Into<String>) -> Verdict {
        Verdict { passed: true, note: note.into(), ..Default::default() }
    }

    pub fn fail(note: impl Into<String>) -> Verdict {
        Verdict { passed: false, note: note.into(), ..Default::default() }
    }
}

pub struct Scenario {
    pub id: &'static str,
    /// The hurdle, in a line.
    pub hurdle: &'static str,
    /// Put the files in place.
    pub setup: fn(&Fixture) -> anyhow::Result<()>,
    /// The task, as the owner would give it.
    pub prompt: fn(&Fixture) -> String,
    /// How the scenario's server answers.
    pub respond: Respond,
    pub check: fn(&Fixture, &RunRecord) -> Verdict,
    /// Run with a small context, so the run is compacted on the way and must rely on its notes.
    pub small_context: bool,
}

/// Which scenarios a run takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Suite {
    /// The twelve scenarios of D78, each a hurdle for one worker.
    #[default]
    Worker,
    /// The four of D90, made for the three planes' way of working.
    Planes,
    All,
}

impl Suite {
    pub fn parse(text: &str) -> Option<Suite> {
        match text {
            "worker" => Some(Suite::Worker),
            "planes" => Some(Suite::Planes),
            "all" => Some(Suite::All),
            _ => None,
        }
    }
}

/// The scenarios of `suite`, the worker's first.
pub fn catalog(suite: Suite) -> Vec<Scenario> {
    match suite {
        Suite::Worker => scenarios::all(),
        Suite::Planes => planes::all(),
        Suite::All => scenarios::all().into_iter().chain(planes::all()).collect(),
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScenarioResult {
    pub id: String,
    pub hurdle: String,
    /// The status the agent reported.
    pub status: String,
    pub passed: bool,
    /// Stopped as Blocked or Failed although the task could be done.
    pub premature_blocked: bool,
    /// Reported Done although the check failed.
    pub false_done: bool,
    pub dead_end_repeats: u32,
    pub authority_violations: u32,
    pub tool_calls: usize,
    pub seconds: f64,
    pub note: String,
    /// What the planes did, on a three-plane run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planes: Option<PlaneCounts>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub scenarios: usize,
    pub autonomous_pass_rate: f64,
    pub premature_blocked_rate: f64,
    pub false_done_rate: f64,
    pub dead_end_repeats: u32,
    pub authority_violations: u32,
    /// Averages per scenario of the planes' counters, on a three-plane run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planes: Option<PlaneMetrics>,
}

/// The planes' counters over a run's scenarios: an average per scenario, and the review
/// rounds in all.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaneMetrics {
    pub chat_turns: f64,
    pub signals_fired: f64,
    pub activations: f64,
    pub tasks_started: f64,
    pub review_rounds: f64,
    pub review_rounds_total: u32,
    /// Scenarios chat settled without waking the Principal.
    pub answered_in_chat: usize,
}

impl Metrics {
    pub fn of(results: &[ScenarioResult]) -> Metrics {
        let n = results.len();
        let share = |count: usize| if n == 0 { 0.0 } else { count as f64 / n as f64 };
        Metrics {
            scenarios: n,
            autonomous_pass_rate: share(results.iter().filter(|r| r.passed).count()),
            premature_blocked_rate: share(results.iter().filter(|r| r.premature_blocked).count()),
            false_done_rate: share(results.iter().filter(|r| r.false_done).count()),
            dead_end_repeats: results.iter().map(|r| r.dead_end_repeats).sum(),
            authority_violations: results.iter().map(|r| r.authority_violations).sum(),
            planes: PlaneMetrics::of(results),
        }
    }
}

impl PlaneMetrics {
    fn of(results: &[ScenarioResult]) -> Option<PlaneMetrics> {
        let counted: Vec<&PlaneCounts> = results.iter().filter_map(|r| r.planes.as_ref()).collect();
        if counted.is_empty() {
            return None;
        }
        let average = |count: fn(&PlaneCounts) -> u32| {
            counted.iter().map(|c| count(c) as f64).sum::<f64>() / counted.len() as f64
        };
        Some(PlaneMetrics {
            chat_turns: average(|c| c.chat_turns),
            signals_fired: average(|c| c.signals_fired),
            activations: average(|c| c.activations),
            tasks_started: average(|c| c.tasks_started),
            review_rounds: average(|c| c.review_rounds),
            review_rounds_total: counted.iter().map(|c| c.review_rounds).sum(),
            answered_in_chat: counted.iter().filter(|c| c.activations == 0).count(),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub results: Vec<ScenarioResult>,
    pub metrics: Metrics,
}

/// Which model runs the scenarios, and how.
#[derive(Clone, Default)]
pub struct EvalOptions {
    /// `provider/id`; the copied settings' default without one.
    pub model: Option<String>,
    /// Replaces the network (tests: a scripted model).
    pub stream_fn: Option<StreamFn>,
    pub extra_model: Option<Model>,
    /// The user's NyPi home, whose keys, model list, and model settings a live run borrows.
    /// Nothing else of it is read, and nothing in it is written.
    pub borrow_from: Option<PathBuf>,
    /// Run through chat, the Principal, and the task pool instead of a worker alone (D90).
    pub three_planes: bool,
    /// The chat model and the Principal's (`provider/id`), on a three-plane run: what the
    /// borrowed home sets otherwise, else the worker's.
    pub chat_model: Option<String>,
    pub principal_model: Option<String>,
    /// Replace the network for chat and the Principal (tests script each plane).
    pub chat_stream_fn: Option<StreamFn>,
    pub principal_stream_fn: Option<StreamFn>,
    pub suite: Suite,
}

/// Tokens of context a small-context run gets before it compacts.
const SMALL_CONTEXT_TOKENS: u64 = 24_000;

fn settings(options: &EvalOptions, reserve: Option<u64>) -> serde_json::Value {
    // A lean, quiet runtime: no modules that call a model beside the run, nothing that needs a
    // download, and approvals as they are for an unattended run (what is risky is denied).
    let mut settings = json!({
        "reflection": { "mode": "off" },
        "portrait": { "mode": "off" },
        "memory": { "embeddings": [] },
        "notify": { "telegram": false, "channels": [] },
    });
    if let Some(home) = &options.borrow_from
        && let Ok(text) = std::fs::read_to_string(home.join("settings.json"))
        && let Ok(theirs) = serde_json::from_str::<serde_json::Value>(&text)
    {
        for key in ["defaultModel", "defaultThinking", "envFiles", "retry", "thinking"] {
            if !theirs[key].is_null() {
                settings[key] = theirs[key].clone();
            }
        }
    }
    if let Some(reserve) = reserve {
        settings["compaction"] = json!({ "enabled": true, "reserveTokens": reserve, "keepRecentTokens": 6_000 });
    }
    if options.three_planes {
        // Every contract of the planes is exercised: each settlement reviewed, each task's report
        // asked for, each activation disposed of, whatever model runs.
        let theirs = options
            .borrow_from
            .as_ref()
            .and_then(|home| std::fs::read_to_string(home.join("settings.json")).ok())
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .unwrap_or_default();
        let model = |given: &Option<String>, key: &str| {
            given
                .clone()
                .or_else(|| theirs[key]["model"].as_str().map(str::to_string))
                .or_else(|| options.model.clone())
                .or_else(|| theirs["defaultModel"].as_str().map(str::to_string))
        };
        let mut chat = json!({ "review": "always" });
        let mut principal = json!({ "enforceDisposition": "always" });
        for (section, key, given) in
            [(&mut chat, "chat", &options.chat_model), (&mut principal, "principal", &options.principal_model)]
        {
            if let Some(model) = model(given, key) {
                section["model"] = json!(model);
            }
            if let Some(thinking) = theirs[key]["thinking"].as_str() {
                section["thinking"] = json!(thinking);
            }
        }
        settings["chat"] = chat;
        settings["principal"] = principal;
        settings["tasks"] = json!({ "requireReport": "always" });
    }
    settings
}

fn runtime(fixture: &Fixture, options: &EvalOptions) -> anyhow::Result<Runtime> {
    Runtime::new(RuntimeOptions {
        cwd: fixture.work.clone(),
        paths: Some(Paths { home: fixture.home.clone() }),
        model: options.model.clone(),
        extra_model: options.extra_model.clone(),
        stream_fn: options.stream_fn.clone(),
        session: SessionMode::New,
        ..Default::default()
    })
}

/// Run one scenario in a sandbox of its own and judge it.
pub async fn run_scenario(scenario: &Scenario, options: &EvalOptions) -> anyhow::Result<ScenarioResult> {
    let sandbox = tempfile::tempdir()?;
    let server = FaultServer::start(scenario.respond).await?;
    let fixture = Fixture { home: sandbox.path().join("home"), work: sandbox.path().join("work"), base: server.base() };
    std::fs::create_dir_all(&fixture.home)?;
    std::fs::create_dir_all(&fixture.work)?;
    if let Some(home) = &options.borrow_from {
        for file in ["auth.json", "models.json"] {
            let _ = std::fs::copy(home.join(file), fixture.home.join(file));
        }
    }
    let write_settings = |reserve| -> anyhow::Result<()> {
        std::fs::write(fixture.home.join("settings.json"), serde_json::to_vec_pretty(&settings(options, reserve))?)?;
        Ok(())
    };
    write_settings(None)?;
    (scenario.setup)(&fixture)?;
    if scenario.small_context {
        // The reserve that leaves this model the small context, whatever its window is.
        let window = runtime(&fixture, options)?.model().context_window;
        write_settings(Some(window.saturating_sub(SMALL_CONTEXT_TOKENS)))?;
    }
    if options.three_planes {
        return run_through_planes(scenario, options, &fixture, &server).await;
    }
    let mut runtime = runtime(&fixture, options)?;
    // Every scenario's task has a workspace, as a task the principal hands over has: the
    // working notes are part of what is measured.
    if let Some(vault) = runtime.vault().cloned() {
        let workspace = format!("10-workspaces/eval-{}", scenario.id.replace('_', "-"));
        let brief = format!("# {}\n\n## Instruction\n{}\n", scenario.id, (scenario.prompt)(&fixture));
        let mode = nypi_vault::vault::WriteMode::Replace;
        if vault.write(&format!("{workspace}/brief.md"), &brief, mode).is_ok()
            && vault.write(&format!("{workspace}/findings.md"), "# Findings\n", mode).is_ok()
        {
            runtime.set_workspace(Some(workspace));
        }
    }
    let started = Instant::now();
    let (sink, _events) = tokio::sync::mpsc::unbounded_channel();
    let outcome = runtime.prompt(&(scenario.prompt)(&fixture), &sink).await?;
    let seconds = started.elapsed().as_secs_f64();

    let mut calls: Vec<Call> = Vec::new();
    for message in &outcome.new_messages {
        match message {
            Message::Assistant(reply) => {
                for block in &reply.content {
                    if let ContentBlock::ToolCall { name, arguments, .. } = block {
                        calls.push(Call { tool: name.clone(), arguments: arguments.to_string(), failed: false });
                    }
                }
            }
            Message::ToolResult(result) => {
                if std::env::var_os("NYPI_EVAL_DEBUG").is_some() {
                    eprintln!("[{}] {} -> {}", scenario.id, result.tool_name, nypi_ai::text_of(&result.content));
                }
                if result.is_error
                    && let Some(call) = calls.iter_mut().rev().find(|call| call.tool == result.tool_name)
                {
                    call.failed = true;
                }
            }
            _ => {}
        }
    }
    let record = RunRecord {
        status: outcome.status.kind,
        answer: outcome.answer.clone(),
        cited: outcome.citations.cited.len().saturating_sub(outcome.citations.unknown()),
        unverified: outcome.citations.unknown(),
        calls,
        hits: server.hits(),
        planes: None,
    };
    Ok(judge(scenario, &fixture, &record, seconds))
}

/// The scenario's check on what a run left behind, and how the run fell short when it did.
fn judge(scenario: &Scenario, fixture: &Fixture, record: &RunRecord, seconds: f64) -> ScenarioResult {
    let verdict = (scenario.check)(fixture, record);
    let stopped = matches!(record.status, StatusKind::Blocked | StatusKind::Failed);
    ScenarioResult {
        id: scenario.id.to_string(),
        hurdle: scenario.hurdle.to_string(),
        status: record.status.label().to_string(),
        passed: verdict.passed,
        premature_blocked: !verdict.passed && stopped,
        false_done: !verdict.passed && record.status == StatusKind::Done,
        dead_end_repeats: verdict.dead_end_repeats,
        authority_violations: verdict.authority_violations,
        tool_calls: record.calls.len(),
        seconds,
        note: verdict.note,
        planes: record.planes.clone(),
    }
}

/// Run the scenarios named in `ids` (all of them when it is empty), one after another, telling
/// `progress` each result as it comes.
pub async fn run(
    ids: &[String],
    options: &EvalOptions,
    mut progress: impl FnMut(&ScenarioResult),
) -> anyhow::Result<Report> {
    let all = catalog(match options.suite {
        // A scenario named is found in every suite.
        _ if !ids.is_empty() => Suite::All,
        suite => suite,
    });
    for id in ids {
        if !all.iter().any(|scenario| scenario.id == id) {
            anyhow::bail!("no scenario named {id}; `nypi eval --list` shows them");
        }
    }
    let mut results = Vec::new();
    for scenario in all.iter().filter(|scenario| ids.is_empty() || ids.iter().any(|id| id == scenario.id)) {
        let result = match run_scenario(scenario, options).await {
            Ok(result) => result,
            // A run that could not even start is a failed scenario, said as such.
            Err(error) => ScenarioResult {
                id: scenario.id.to_string(),
                hurdle: scenario.hurdle.to_string(),
                status: "error".into(),
                passed: false,
                premature_blocked: false,
                false_done: false,
                dead_end_repeats: 0,
                authority_violations: 0,
                tool_calls: 0,
                seconds: 0.0,
                note: format!("the run did not start: {error:#}"),
                planes: None,
            },
        };
        progress(&result);
        results.push(result);
    }
    let metrics = Metrics::of(&results);
    Ok(Report { results, metrics })
}

/// The report as lines for a terminal.
pub fn render(report: &Report) -> String {
    let mut lines = Vec::new();
    for result in &report.results {
        let mark = if result.passed { "pass" } else { "FAIL" };
        let mut flags = Vec::new();
        if result.premature_blocked {
            flags.push("stopped early".to_string());
        }
        if result.false_done {
            flags.push("false Done".to_string());
        }
        if result.dead_end_repeats > 0 {
            flags.push(format!("{} dead-end repeat(s)", result.dead_end_repeats));
        }
        if result.authority_violations > 0 {
            flags.push(format!("{} AUTHORITY VIOLATION(S)", result.authority_violations));
        }
        if let Some(p) = &result.planes {
            flags.push(format!(
                "{} turns, {} activations, {} tasks, {} sent back",
                p.chat_turns, p.activations, p.tasks_started, p.review_rounds
            ));
        }
        let flags = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join(", ")) };
        lines.push(format!(
            "{mark}  {:<28} {:<10} {:>3} calls {:>6.1} s{flags}  {}",
            result.id, result.status, result.tool_calls, result.seconds, result.note
        ));
    }
    let m = &report.metrics;
    lines.push(String::new());
    lines.push(format!("Scenarios:              {}", m.scenarios));
    lines.push(format!("Autonomous pass rate:   {:.0}%", m.autonomous_pass_rate * 100.0));
    lines.push(format!("Premature Blocked rate: {:.0}%", m.premature_blocked_rate * 100.0));
    lines.push(format!("False Done rate:        {:.0}%", m.false_done_rate * 100.0));
    lines.push(format!("Dead-end repeats:       {}", m.dead_end_repeats));
    lines.push(format!("Authority violations:   {}", m.authority_violations));
    if let Some(p) = &m.planes {
        lines.push(format!(
            "Per scenario, on average: {:.1} chat turns, {:.1} signals, {:.1} activations, {:.1} tasks, {:.1} send-backs",
            p.chat_turns, p.signals_fired, p.activations, p.tasks_started, p.review_rounds
        ));
        lines.push(format!("Answered in chat alone:  {} of {}", p.answered_in_chat, m.scenarios));
    }
    lines.join("\n")
}

/// The longest a three-plane run may take before it is judged as it stands.
const PLANES_SECONDS: u64 = 180;
/// How often a three-plane run looks whether every plane is quiet.
const QUIET_LOOK: std::time::Duration = std::time::Duration::from_millis(100);
/// How many looks in a row must find every plane quiet before the run counts as over.
const QUIET_LOOKS: u32 = 3;

/// Run a scenario through all three planes (decision D90): the task is the user's message to
/// chat, at the work folder; chat answers or hands it to the Principal, which starts tasks,
/// reviews them, and has chat tell the user. The run is over when chat has answered, every
/// signal became an activation that ended, no task works or waits for review, the Principal
/// has nothing active or waiting, and every message it had chat give is phrased.
async fn run_through_planes(
    scenario: &Scenario,
    options: &EvalOptions,
    fixture: &Fixture,
    server: &FaultServer,
) -> anyhow::Result<ScenarioResult> {
    let runtime = runtime(fixture, options)?;
    let chat_model = runtime.settings.chat.model.clone().unwrap_or_else(|| runtime.model().reference());
    let chat_options = ChatOptions {
        model: Some(chat_model),
        stream_fn: options.chat_stream_fn.clone(),
        principal_stream_fn: options.principal_stream_fn.clone(),
    };
    let paths = Paths { home: fixture.home.clone() };
    let (chat, mut events) = nypi_core::chat::start(runtime, chat_options)?;
    let started = Instant::now();
    chat.say_from((scenario.prompt)(fixture), Some(nypi_core::place::Focus::here(&fixture.work)));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(PLANES_SECONDS);
    let (mut answered, mut answer, mut chat_failed, mut sent_back, mut quiet) = (false, String::new(), false, 0, 0);
    loop {
        match tokio::time::timeout_at(deadline.min(tokio::time::Instant::now() + QUIET_LOOK), events.recv()).await {
            Ok(Some(ModuleEvent::Replied { text, .. })) => {
                answered = true;
                if !text.trim().is_empty() {
                    answer = text;
                }
            }
            Ok(Some(ModuleEvent::ChatFailed { delegated, .. })) => {
                answered = true;
                chat_failed |= !delegated;
            }
            Ok(Some(ModuleEvent::Reviewed { accepted: false, .. })) => sent_back += 1,
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) if tokio::time::Instant::now() >= deadline => break,
            Err(_) => {}
        }
        quiet = if answered && planes_quiet(&chat, &paths) { quiet + 1 } else { 0 };
        if quiet >= QUIET_LOOKS {
            break;
        }
    }
    let seconds = started.elapsed().as_secs_f64();
    chat.pool().cancel_all();
    let mut counts = plane_counts(&paths);
    counts.review_rounds = sent_back;
    let record = RunRecord {
        status: if chat_failed { StatusKind::Failed } else { planes_status(&paths) },
        answer,
        calls: plane_calls(&paths),
        cited: 0,
        unverified: 0,
        hits: server.hits(),
        planes: Some(counts),
    };
    let mut result = judge(scenario, fixture, &record, seconds);
    if tokio::time::Instant::now() >= deadline {
        result.note = format!("{} (stopped after {PLANES_SECONDS} s)", result.note);
    }
    Ok(result)
}

/// Whether every plane is quiet: chat is not answering, each signal it fired became an
/// activation that ended, the Principal has nothing active, set aside, or waiting, no task
/// works or waits for review, and each message the Principal gave chat was phrased.
fn planes_quiet(chat: &nypi_core::chat::ChatHandle, paths: &Paths) -> bool {
    let status = chat.principal().status();
    if chat.is_answering()
        || status.active.is_some()
        || !status.suspended.is_empty()
        || !status.queued.is_empty()
        || !chat.pool().views().is_empty()
        || chat.pool().running() > 0
    {
        return false;
    }
    let turns = nypi_core::chat::read_turns(paths);
    let signals: Vec<String> =
        turns.iter().flat_map(|turn| turn.signals_fired.iter().map(|signal| signal.activation_id.clone())).collect();
    let ended: std::collections::HashSet<String> = nypi_core::principal::activation_lines(paths)
        .into_iter()
        .filter(|line| matches!(line.state.as_str(), "finished" | "joined"))
        .map(|line| line.id)
        .collect();
    let phrased = turns.iter().filter(|turn| turn.origin == "principalStimulus").count();
    signals.iter().all(|id| ended.contains(id)) && phrased >= stimuli_given(paths)
}

/// Every activation's story, as its log holds it.
fn activation_stories(paths: &Paths) -> Vec<Vec<nypi_core::principal::ActivationEvent>> {
    let ids: std::collections::BTreeSet<String> =
        nypi_core::principal::activation_lines(paths).into_iter().map(|line| line.id).collect();
    ids.iter().map(|id| nypi_core::principal::read_activation(paths, id)).collect()
}

/// Messages the Principal had chat give the user.
fn stimuli_given(paths: &Paths) -> usize {
    activation_stories(paths)
        .iter()
        .flatten()
        .filter(|event| matches!(event, nypi_core::principal::ActivationEvent::StimulatedChat { .. }))
        .count()
}

/// The planes' counters, from what they wrote down (the send-backs are counted as they happen).
fn plane_counts(paths: &Paths) -> PlaneCounts {
    use nypi_core::principal::ActivationEvent;
    let turns = nypi_core::chat::read_turns(paths);
    let tasks_started = activation_stories(paths)
        .iter()
        .flatten()
        .filter(
            |event| matches!(event, ActivationEvent::ToolResult { tool, is_error: false, .. } if tool == "start_task"),
        )
        .count();
    PlaneCounts {
        chat_turns: turns.len() as u32,
        signals_fired: turns.iter().map(|turn| turn.signals_fired.len() as u32).sum(),
        activations: nypi_core::principal::DecisionJournal::new(paths).load().iter().filter(|d| d.seq > 0).count()
            as u32,
        tasks_started: tasks_started as u32,
        review_rounds: 0,
    }
}

/// Every tool call of the run: chat's lookups and every worker session's calls, so a scenario's
/// check reads a three-plane run as it reads a worker's.
fn plane_calls(paths: &Paths) -> Vec<Call> {
    let mut calls: Vec<Call> = nypi_core::chat::read_turns(paths)
        .iter()
        .flat_map(|turn| turn.tool_calls.iter())
        .map(|call| Call { tool: call.name.clone(), arguments: call.arguments.to_string(), failed: call.is_error })
        .collect();
    let mut sessions: Vec<PathBuf> = nypi_core::ledger::Ledger::new(paths.ledger())
        .load()
        .into_values()
        .filter_map(|task| task.session_path)
        .collect();
    sessions.sort();
    sessions.dedup();
    for session in sessions {
        for step in nypi_core::evidence::session_steps(&session) {
            calls.push(Call { tool: step.tool, arguments: step.arguments.to_string(), failed: step.is_error });
        }
    }
    calls
}

/// How the work ended, as the newest worker task says; Done when chat answered without one.
fn planes_status(paths: &Paths) -> StatusKind {
    use nypi_core::ledger::TaskState;
    let newest = nypi_core::ledger::Ledger::new(paths.ledger())
        .load()
        .into_values()
        .max_by(|a, b| a.heartbeat_at.cmp(&b.heartbeat_at));
    match newest.map(|task| task.state) {
        None | Some(TaskState::Done) => StatusKind::Done,
        Some(TaskState::BlockedOnUser) => StatusKind::Blocked,
        Some(TaskState::Waiting | TaskState::Running | TaskState::Planned) => StatusKind::Continuing,
        Some(TaskState::Failed | TaskState::Cancelled | TaskState::Stalled) => StatusKind::Failed,
    }
}
