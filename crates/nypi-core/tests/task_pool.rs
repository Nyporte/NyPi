//! Principal, observer, worker (decisions D78, D84), with scripted models that answer by what
//! they are asked: a task with a brief gets a workspace that stays in the worker's view; a
//! second task works beside the first; the observer of a run's course steers it once and is
//! otherwise silent; and the Principal sends a report that falls short back before the user
//! hears of it. Chat, in front, only hands the owner's message up and phrases what comes down.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, FauxStep, faux_model};
use nypi_ai::{Context, Message, StopReason, text_of};
use nypi_core::chat::{self, ChatOptions, ModuleEvent};
use nypi_core::config::Paths;
use nypi_core::pulse::{PulseKind, PulsePayload};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::status::StatusKind;
use nypi_core::worker::WorkerEvent;
use serde_json::json;
use tokio::sync::mpsc;

fn runtime(home: &std::path::Path, work: &std::path::Path, faux: &FauxProvider) -> Runtime {
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: work.to_path_buf(),
        paths: Some(Paths { home: home.to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        ..Default::default()
    })
    .unwrap();
    runtime.set_channel("test");
    runtime
}

async fn next(events: &mut mpsc::UnboundedReceiver<ModuleEvent>) -> ModuleEvent {
    tokio::time::timeout(Duration::from_secs(20), events.recv()).await.expect("event in time").expect("open")
}

/// Everything a request holds, as text: the prompt, its sections, and every message.
fn seen(ctx: &Context) -> String {
    format!("{}\n{:?}", ctx.system_prompt, ctx.messages)
}

/// The text of the last message a user (or the harness, as the user) said.
fn last_said(ctx: &Context) -> String {
    ctx.messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User(user) => Some(text_of(&user.content)),
            _ => None,
        })
        .unwrap_or_default()
}

/// What opened a Principal's activation.
fn first_said(ctx: &Context) -> String {
    let opening = ctx
        .messages
        .iter()
        .find_map(|m| match m {
            Message::User(user) => Some(text_of(&user.content)),
            _ => None,
        })
        .unwrap_or_default();
    nypi_core::principal::activation_text(&opening).to_string()
}

/// Chat that hands every message up, says "On it.", and tells the owner the first fact of what
/// the Principal sends down.
fn relaying_chat() -> FauxProvider {
    FauxProvider::answering(|ctx| {
        let said = last_said(ctx);
        Some(if said.starts_with("[The Principal asks you") {
            let fact = said.split("What they must hear:\n- ").nth(1).and_then(|rest| rest.lines().next());
            FauxResponse::text(fact.unwrap_or("Something happened."))
        } else if after_a_tool(ctx) {
            FauxResponse::text("On it.")
        } else {
            FauxResponse::tool_call("c0", "signal_principal", json!({ "topic": said }))
        })
    })
}

fn chat_options(chat: &FauxProvider, principal: &FauxProvider) -> ChatOptions {
    ChatOptions {
        model: Some("faux/faux-1".into()),
        stream_fn: Some(chat.stream_fn()),
        principal_stream_fn: Some(principal.stream_fn()),
    }
}

fn tool_results(ctx: &Context) -> usize {
    ctx.messages.iter().filter(|m| matches!(m, Message::ToolResult(_))).count()
}

/// Whether the request's last message is a tool's result: the model is mid-turn.
fn after_a_tool(ctx: &Context) -> bool {
    matches!(ctx.messages.last(), Some(Message::ToolResult(_)))
}

fn workspace_in(text: &str) -> Option<String> {
    let at = text.find("10-workspaces/")?;
    let path: String = text[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '/')).collect();
    Some(path.trim_end_matches('/').to_string())
}

#[tokio::test]
async fn a_task_with_a_brief_gets_a_workspace_that_stays_in_view() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let worker_model = FauxProvider::answering(|ctx| {
        let workspace = workspace_in(&seen(ctx)).expect("the worker is told its workspace");
        Some(match tool_results(ctx) {
            0 => FauxResponse::tool_call(
                "w1",
                "vault_write",
                json!({
                    "path": format!("{workspace}/findings.md"),
                    "mode": "append",
                    "content": "- Falsified: the duplicate rows do not come from the retry storm.\n",
                }),
            ),
            _ => {
                FauxResponse::text("The importer keys rows by source id now [t1].\n\nDone: the importer is idempotent.")
            }
        })
    });
    let principal_model = FauxProvider::answering(|ctx| {
        Some(if after_a_tool(ctx) {
            FauxResponse::text("Started it.")
        } else {
            FauxResponse::tool_call(
                "c1",
                "start_task",
                json!({
                    "title": "Idempotent importer",
                    "instruction": "Make the nightly importer idempotent: running it twice must not duplicate rows.",
                    "done_when": "the reproduction test that runs the import twice passes",
                    "bounds": "do not touch the production database",
                }),
            )
        })
    });
    let runtime = runtime(home.path(), work.path(), &worker_model);
    let (chat, mut events) = chat::start(runtime, chat_options(&relaying_chat(), &principal_model)).unwrap();
    chat.say("The importer duplicates rows when it runs twice. Fix it.");
    let outcome = loop {
        if let ModuleEvent::Worker(WorkerEvent::Settled { outcome, .. }) = next(&mut events).await {
            break outcome.unwrap();
        }
    };
    assert_eq!(outcome.status.kind, StatusKind::Done);

    // The workspace is in the vault, with the brief as the principal wrote it.
    let workspaces = home.path().join("vault/10-workspaces");
    let folder = std::fs::read_dir(&workspaces).unwrap().next().unwrap().unwrap().path();
    assert!(folder.file_name().unwrap().to_string_lossy().ends_with("-idempotent-importer"), "{folder:?}");
    let brief = std::fs::read_to_string(folder.join("brief.md")).unwrap();
    assert!(brief.contains("## Done when\nthe reproduction test that runs the import twice passes"), "{brief}");
    assert!(brief.contains("## Stop before\ndo not touch the production database"));
    let findings = std::fs::read_to_string(folder.join("findings.md")).unwrap();
    assert!(findings.contains("Falsified: the duplicate rows do not come from the retry storm."));

    // The worker was given the contract, and saw the workspace pinned in its prompt: the brief
    // from the first turn, and what it wrote to findings.md from the turn after it wrote it.
    let requests = worker_model.requests();
    let first = seen(&requests[0]);
    assert!(first.contains("Done when: the reproduction test that runs the import twice passes"), "{first}");
    assert!(first.contains("<task_workspace>") && first.contains("## brief.md") && first.contains("## findings.md"));
    assert!(!first.contains("retry storm"));
    assert!(seen(&requests[1]).contains("Falsified: the duplicate rows do not come from the retry storm."));
    assert!(chat.pool().views().is_empty(), "without a review the task is through when it settles");
}

#[tokio::test]
async fn a_second_task_works_beside_the_first_up_to_the_limit() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(home.path().join("settings.json"), r#"{ "tasks": { "maxConcurrent": 2 }, "sandbox": "off" }"#)
        .unwrap();
    let worker_model = FauxProvider::answering(|ctx| {
        let task = seen(ctx);
        Some(if task.contains("the slow one") {
            match tool_results(ctx) {
                0 => FauxResponse::tool_call("s1", "bash", json!({ "command": "sleep 1.5; echo slow" })),
                _ => FauxResponse::text("Done: the slow one is through."),
            }
        } else {
            FauxResponse::text("Done: the quick one is through.")
        })
    });
    // One activation starts three tasks: the first takes the conversation's own worker, the
    // second gets a runtime beside it, the third is over the limit.
    let principal_model = FauxProvider::answering(|ctx| {
        Some(if after_a_tool(ctx) {
            FauxResponse::text("Started what fits.")
        } else {
            FauxResponse {
                steps: ["Do the slow one.", "Do the quick one.", "Do a third one."]
                    .iter()
                    .enumerate()
                    .map(|(n, task)| FauxStep::ToolCall {
                        id: format!("c{n}"),
                        name: "start_task".into(),
                        arguments: json!({ "instruction": format!("{task} (the {} one)", ["slow", "quick", "third"][n]) }),
                    })
                    .collect(),
                stop_reason: StopReason::ToolUse,
                error: None,
            }
        })
    });
    let runtime = runtime(home.path(), work.path(), &worker_model);
    let (chat, mut events) = chat::start(runtime, chat_options(&relaying_chat(), &principal_model)).unwrap();
    chat.say("Three things, please.");
    let mut order = Vec::new();
    while order.len() < 2 {
        match next(&mut events).await {
            ModuleEvent::Worker(WorkerEvent::Settled { outcome, .. }) => {
                assert_eq!(outcome.unwrap().status.text, "the slow one is through.");
                order.push("own");
            }
            ModuleEvent::Worker(WorkerEvent::Beside { task, event }) => {
                if let WorkerEvent::Settled { outcome, .. } = *event {
                    assert!(task.starts_with("t-"));
                    assert_eq!(outcome.unwrap().status.text, "the quick one is through.");
                    order.push("beside");
                }
            }
            _ => {}
        }
    }
    assert_eq!(order, ["beside", "own"], "the quick task did not wait for the slow one");
    // The third was refused with the reason, as a result the Principal could read.
    let refused = principal_model.requests().iter().map(seen).find(|text| text.contains("which is the limit"));
    assert!(refused.is_some(), "the limit is said to the Principal");
    // Each ran as a task of its own in the ledger, in a session of its own.
    let tasks = nypi_core::ledger::Ledger::new(Paths { home: home.path().to_path_buf() }.ledger()).load();
    assert_eq!(tasks.len(), 2);
    let sessions: std::collections::BTreeSet<&str> = tasks.values().map(|task| task.session_id.as_str()).collect();
    assert_eq!(sessions.len(), 2);
}

#[tokio::test]
async fn the_observer_steers_a_run_in_a_rut_and_is_silent_otherwise() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let settings = json!({ "thinking": { "enabled": true, "workerCadenceTurns": 2 }, "sandbox": "off" });
    std::fs::write(home.path().join("settings.json"), settings.to_string()).unwrap();
    let looks = Arc::new(AtomicUsize::new(0));
    let model = FauxProvider::answering({
        let looks = looks.clone();
        move |ctx| {
            // The observer's question, told from the worker's by its system prompt.
            if ctx.system_prompt.contains("watches a working agent's course") {
                let look = looks.fetch_add(1, Ordering::SeqCst);
                let asked = last_said(ctx);
                assert!(asked.contains("curl -sf http://127.0.0.1:9/feed -> FAILED"), "the digest: {asked}");
                return Some(FauxResponse::text(&match look {
                    0 => json!({
                        "assessment": "It keeps fetching the same dead endpoint.",
                        "steer": "The endpoint has refused twice. Read the cached copy in feed.json instead of fetching again."
                    })
                    .to_string(),
                    _ => json!({ "assessment": "It changed course.", "steer": "" }).to_string(),
                }));
            }
            let noted = seen(ctx).contains("[Metacognitive note]");
            Some(match (noted, tool_results(ctx)) {
                (true, _) if !seen(ctx).contains("cat feed.json") => {
                    FauxResponse::tool_call("r1", "bash", json!({ "command": "cat feed.json" }))
                }
                (true, _) => {
                    FauxResponse::text("The feed has 3 items [t9].\n\nDone: read the feed from its cached copy.")
                }
                (false, n) if n < 40 => FauxResponse::tool_call(
                    &format!("f{n}"),
                    "bash",
                    json!({ "command": "curl -sf http://127.0.0.1:9/feed" }),
                ),
                _ => FauxResponse::text("Failed: the endpoint never answered."),
            })
        }
    });
    std::fs::write(work.path().join("feed.json"), r#"{"items":[1,2,3]}"#).unwrap();
    let mut runtime = runtime(home.path(), work.path(), &model);
    let mut thoughts = runtime.pulses().subscribe("test", &[PulseKind::Thought]);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("How many items are in the feed?", &tx).await.unwrap();
    assert_eq!(outcome.status.kind, StatusKind::Done, "{}", outcome.answer);
    assert!(outcome.answer.contains("cached copy"));

    // One steer, published as a thought and read by the worker as a note that is not the user's.
    let thought = thoughts.try_recv().expect("the steer is a pulse");
    let PulsePayload::Thought { thought } = thought.payload else { panic!() };
    assert!(thought.say.contains("feed.json"));
    assert!(thoughts.try_recv().is_none(), "a sound course gets no note");
    let note = outcome.new_messages.iter().find_map(|m| match m {
        Message::User(user) if text_of(&user.content).starts_with("[Metacognitive note]") => {
            Some(text_of(&user.content))
        }
        _ => None,
    });
    assert!(note.unwrap().contains("not the user speaking"));
    assert!(looks.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn the_principal_sends_a_shallow_report_back_before_the_user_hears_of_it() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let settings = json!({ "chat": { "model": "faux/faux-1", "review": "always" }, "sandbox": "off" });
    std::fs::write(home.path().join("settings.json"), settings.to_string()).unwrap();
    let worker_model = FauxProvider::answering(|ctx| {
        let sent_back = seen(ctx).contains("[Review of your report on this task");
        Some(match (sent_back, after_a_tool(ctx)) {
            // The first report claims Done with nothing run.
            (false, _) => FauxResponse::text("I changed the parser.\n\nDone: the bug is fixed."),
            (true, false) => {
                FauxResponse::tool_call("t1", "bash", json!({ "command": "echo 'test result: ok. 12 passed'" }))
            }
            (true, true) => {
                FauxResponse::text("The suite passes [t1].\n\nDone: the fix is verified by the test suite.")
            }
        })
    });
    let principal_model = FauxProvider::answering(|ctx| {
        let opened = first_said(ctx);
        Some(if opened.starts_with("[A task settled") {
            let task = opened
                .split("Task: ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or_default()
                .to_string();
            match (opened.contains("Sent back so far: 0 time(s)"), after_a_tool(ctx)) {
                (true, false) => FauxResponse::tool_call(
                    "c2",
                    "continue_task",
                    json!({ "task_id": task, "critique": "Nothing was run. Run the test suite and cite its result." }),
                ),
                (false, false) => FauxResponse::tool_call(
                    "c3",
                    "stimulate_chat",
                    json!({
                        "kind": "result",
                        "headline": "The parser is fixed",
                        "must_convey": ["The parser bug is fixed, and the test suite passes."],
                    }),
                ),
                (_, true) => FauxResponse::text("Reviewed."),
            }
        } else if after_a_tool(ctx) {
            FauxResponse::text("Started it.")
        } else {
            FauxResponse::tool_call(
                "c1",
                "start_task",
                json!({ "instruction": "Fix the date parser bug.", "done_when": "the test suite passes" }),
            )
        })
    });
    let runtime = runtime(home.path(), work.path(), &worker_model);
    let (chat, mut events) = chat::start(runtime, chat_options(&relaying_chat(), &principal_model)).unwrap();
    chat.say("The date parser is broken, fix it.");

    let (mut replies, mut verdicts, mut settled) = (Vec::new(), Vec::new(), 0);
    while verdicts.len() < 2 || replies.len() < 2 {
        match next(&mut events).await {
            ModuleEvent::Replied { text, .. } => replies.push(text),
            ModuleEvent::Reviewed { accepted, said, round, .. } => verdicts.push((accepted, said, round)),
            ModuleEvent::Worker(WorkerEvent::Settled { .. }) => settled += 1,
            _ => {}
        }
    }
    // Sent back once, in silence; then accepted, and told in chat's voice.
    assert_eq!(verdicts, [(false, false, 1), (true, true, 1)]);
    assert_eq!(settled, 2, "the worker ran twice");
    assert_eq!(
        replies,
        ["On it.", "The parser bug is fixed, and the test suite passes."],
        "the user never saw the shallow report"
    );
    // The review was shown the brief and what the harness could verify of the report.
    let first_review =
        principal_model.requests().iter().map(first_said).find(|said| said.starts_with("[A task settled")).unwrap();
    assert!(first_review.contains("Done when: the test suite passes"), "{first_review}");
    assert!(first_review.contains("Status the worker reported: Done: the bug is fixed."));
    assert!(first_review.contains("The report cites no tool result."), "{first_review}");
    // The worker kept its context for the second round, in the same session; two ledger tasks.
    let tasks = nypi_core::ledger::Ledger::new(Paths { home: home.path().to_path_buf() }.ledger()).load();
    assert_eq!(tasks.len(), 2);
    assert!(chat.pool().views().is_empty(), "accepted, the task leaves the pool");
}

/// The worker's dossier (decision D87): a task with a workspace that reports Done without a
/// report.md is sent back once to write one; the review reads report.md, with the manifest the
/// harness drew from the session beside it (the cited call, and the file change nothing
/// checked), and the Principal reads a step in full with inspect_task_evidence.
#[tokio::test]
async fn a_task_ends_with_its_report_and_the_review_sees_the_manifest_beside_it() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let settings = json!({
        "chat": { "model": "faux/faux-1", "review": "always" },
        "tasks": { "requireReport": "always" },
        "sandbox": "off",
    });
    std::fs::write(home.path().join("settings.json"), settings.to_string()).unwrap();
    let worker_model = FauxProvider::answering(|ctx| {
        let workspace = workspace_in(&seen(ctx)).expect("the worker is told its workspace");
        let nudged = last_said(ctx).starts_with("[NyPi harness] Before this task settles");
        Some(match (nudged, after_a_tool(ctx)) {
            (false, false) => {
                FauxResponse::tool_call("w1", "write", json!({ "path": "notes.txt", "content": "renews 2027-03-01" }))
            }
            (false, true) => FauxResponse::text("Wrote notes.txt [t1].\n\nDone: the note is written."),
            (true, false) => FauxResponse::tool_call(
                "w2",
                "vault_write",
                json!({
                    "path": format!("{workspace}/report.md"),
                    "content": "# Report\n\n## Contract Check\n- notes.txt holds the renewal date [t1]\n\n## Key Findings & \
                                Deliverables\n- notes.txt [t1]\n\n## Falsified Paths & Residual Risks\n- none\n",
                }),
            ),
            (true, true) => FauxResponse::text("Report written.\n\nDone: the note is written [t1]."),
        })
    });
    let principal_model = FauxProvider::answering(|ctx| {
        let opened = first_said(ctx);
        Some(if opened.starts_with("[A task settled") {
            let task =
                opened.split("Task: ").nth(1).and_then(|rest| rest.split_whitespace().next()).unwrap_or_default();
            match tool_results(ctx) {
                0 => FauxResponse::tool_call("p2", "inspect_task_evidence", json!({ "task_id": task, "refs": ["t1"] })),
                1 => FauxResponse::tool_call(
                    "p3",
                    "stimulate_chat",
                    json!({ "kind": "result", "headline": "Noted", "must_convey": ["The renewal date is in notes.txt."] }),
                ),
                _ => FauxResponse::text("Reviewed."),
            }
        } else if after_a_tool(ctx) {
            FauxResponse::text("Started it.")
        } else {
            FauxResponse::tool_call(
                "p1",
                "start_task",
                json!({ "instruction": "Write the renewal date to notes.txt.", "done_when": "notes.txt holds it" }),
            )
        })
    });
    let runtime = Runtime::new(RuntimeOptions {
        cwd: work.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(worker_model.stream_fn()),
        session: SessionMode::New,
        ..Default::default()
    })
    .unwrap();
    let (chat, mut events) = chat::start(runtime, chat_options(&relaying_chat(), &principal_model)).unwrap();
    chat.say("Note down when Atlas renews.");
    loop {
        if let ModuleEvent::Reviewed { accepted, .. } = next(&mut events).await {
            assert!(accepted);
            break;
        }
    }
    let nudges =
        worker_model.requests().iter().filter(|ctx| last_said(ctx).contains("Before this task settles")).count();
    assert_eq!(nudges, 2, "asked once, and the vault_write it answered with");
    let review =
        principal_model.requests().iter().map(first_said).find(|said| said.starts_with("[A task settled")).unwrap();
    assert!(review.contains("The worker's report:\n# Report\n\n## Contract Check"), "{review}");
    assert!(
        review.contains("## Harness Evidence Manifest") && review.contains("- [t1] write notes.txt · ok"),
        "{review}"
    );
    assert!(review.contains("WARNING: the task changed a file ([t1] write"), "{review}");
    let inspected = principal_model
        .requests()
        .iter()
        .find_map(|ctx| match ctx.messages.last() {
            Some(Message::ToolResult(result)) if result.tool_name == "inspect_task_evidence" => {
                Some(text_of(&result.content))
            }
            _ => None,
        })
        .unwrap();
    assert!(inspected.contains("## [t1] write · ok") && inspected.contains("renews 2027-03-01"), "{inspected}");
    let decisions = nypi_core::principal::DecisionJournal::new(&Paths { home: home.path().to_path_buf() }).load();
    assert!(decisions.iter().any(|d| d.disposition.contains("stimulated_chat")), "{decisions:?}");
}

/// Settlements that go together are reviewed together (decision D87): a `batch_unless_failed`
/// task that settles Done while its sibling for the same goal still works waits for it, and the
/// two come to the Principal as one review.
#[tokio::test]
async fn sibling_tasks_for_one_goal_are_reviewed_in_one_pass() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let settings = json!({ "chat": { "model": "faux/faux-1", "review": "always" }, "sandbox": "off" });
    std::fs::write(home.path().join("settings.json"), settings.to_string()).unwrap();
    std::fs::write(home.path().join("goals.md"), "## Short term\n- [ ] Launch the shop <!-- g-5a0b01 -->\n").unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let worker_model = FauxProvider::answering({
        let release = release.clone();
        move |ctx| {
            Some(if seen(ctx).contains("Topic: price the products") {
                FauxResponse::text("Priced.\n\nDone: the prices are set.")
            } else {
                // The sibling works until the test lets it finish.
                FauxResponse {
                    steps: vec![
                        FauxStep::Hold(release.clone()),
                        FauxStep::Text("Shipped.\n\nDone: shipping is set.".into()),
                    ],
                    stop_reason: StopReason::Stop,
                    error: None,
                }
            })
        }
    });
    let batch = |topic: &str| json!({ "topic": topic, "goal": "g-5a0b01", "review_policy": "batch_unless_failed" });
    let principal_model = FauxProvider::answering(move |ctx| {
        let opened = first_said(ctx);
        Some(if opened.starts_with("[2 tasks settled that go together") {
            if after_a_tool(ctx) {
                FauxResponse::text("Both reviewed.")
            } else {
                FauxResponse::tool_call(
                    "p3",
                    "stimulate_chat",
                    json!({ "kind": "result", "headline": "Shop", "must_convey": ["Prices and shipping are set."] }),
                )
            }
        } else if opened.starts_with("[A task settled") {
            FauxResponse::text("Reviewed alone, which should not happen.")
        } else {
            match tool_results(ctx) {
                0 => FauxResponse::tool_call("p1", "start_task", batch("set up shipping")),
                1 => FauxResponse::tool_call("p2", "start_task", batch("price the products")),
                _ => FauxResponse::text("Started both."),
            }
        })
    });
    let runtime = runtime(home.path(), work.path(), &worker_model);
    let (chat, mut events) = chat::start(runtime, chat_options(&relaying_chat(), &principal_model)).unwrap();
    chat.say("Get the shop ready.");
    // The pricing settles first, while shipping still works: it is held, not reviewed.
    let mut settled = 0;
    while settled < 1 {
        match next(&mut events).await {
            ModuleEvent::Worker(WorkerEvent::Settled { .. }) => settled += 1,
            ModuleEvent::Worker(WorkerEvent::Beside { event, .. }) if matches!(*event, WorkerEvent::Settled { .. }) => {
                settled += 1
            }
            _ => {}
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !principal_model
            .requests()
            .iter()
            .any(|ctx| first_said(ctx).contains("tasks settled") || first_said(ctx).starts_with("[A task settled")),
        "the Done waits for its sibling"
    );
    release.notify_one();
    let mut reviewed = Vec::new();
    while reviewed.len() < 2 {
        if let ModuleEvent::Reviewed { task, accepted, said, .. } = next(&mut events).await {
            reviewed.push((task, accepted, said));
        }
    }
    assert!(reviewed.iter().all(|(_, accepted, said)| *accepted && *said), "{reviewed:?}");
    let paths = Paths { home: home.path().to_path_buf() };
    let decisions = nypi_core::principal::DecisionJournal::new(&paths).load();
    let review = decisions.iter().find(|d| d.trigger_kind == "taskSettledBatch").expect("one review of both");
    assert_eq!(review.tasks_touched.len(), 2, "{review:?}");
    assert!(!decisions.iter().any(|d| d.trigger_kind == "taskSettled"), "{decisions:?}");
}
