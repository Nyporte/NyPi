//! Runtime behavior added in M2: status-line enforcement, approval requests, command tools, and pulses,
//! driven by the faux provider.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_core::config::Paths;
use nypi_core::pulse::{Delivery, Priority, PulseKind, PulsePayload};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::status::{STATUS_NUDGE, StatusKind};
use serde_json::json;
use tokio::sync::mpsc;

fn runtime(home: &std::path::Path, work: &std::path::Path, faux: &FauxProvider) -> Runtime {
    Runtime::new(RuntimeOptions {
        cwd: work.to_path_buf(),
        paths: Some(Paths { home: home.to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn a_reply_without_status_line_gets_one_nudge() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::text("The answer is 42."),
        FauxResponse::text("Done: answered the question. Next: none needed."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("question", &tx).await.unwrap();
    assert_eq!(outcome.status.kind, StatusKind::Done);
    assert!(!outcome.status.synthesized);
    assert!(outcome.answer.starts_with("The answer is 42.") && outcome.answer.contains("Done: answered"));
    let requests = faux.requests();
    assert_eq!(requests.len(), 2);
    let nudge = requests[1].messages.last().unwrap();
    assert!(matches!(nudge, Message::User(u) if text_of(&u.content) == STATUS_NUDGE));
}

#[tokio::test]
async fn a_stated_status_needs_no_nudge_and_an_ignored_nudge_is_not_repeated() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::text("Checked it.\n\nBlocked: I need the deploy key."),
        FauxResponse::text("Here you go."),
        FauxResponse::text("Still no status."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("deploy", &tx).await.unwrap();
    assert_eq!((outcome.status.kind, outcome.status.text.as_str()), (StatusKind::Blocked, "I need the deploy key."));
    assert_eq!(faux.requests().len(), 1);

    let outcome = runtime.prompt("again", &tx).await.unwrap();
    assert!(outcome.status.synthesized);
    assert_eq!(faux.requests().len(), 3, "one nudge, then the harness derives the status");
}

#[tokio::test]
async fn risky_calls_wait_for_approval() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("t1", "bash", json!({ "command": "git push origin main" })),
        FauxResponse::text("Blocked: the push was not approved."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let mut approvals = runtime.enable_approvals();
    let approver = tokio::spawn(async move {
        let Some(nypi_core::inspector::Approval::Asked(request)) = approvals.recv().await else { panic!("not asked") };
        assert_eq!(request.summary, "git push origin main");
        request.deny(Some("keep it local".into()));
    });
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("push it", &tx).await.unwrap();
    approver.await.unwrap();
    let result = outcome
        .new_messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) => Some(r.clone()),
            _ => None,
        })
        .unwrap();
    assert!(result.is_error);
    assert!(text_of(&result.content).contains("keep it local"));
    assert_eq!(outcome.status.kind, StatusKind::Blocked);
}

#[tokio::test]
async fn command_tools_from_settings_are_callable_and_respect_tool_restrictions() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(
        home.path().join("settings.json"),
        r#"{"commandTools": {"greet": {
            "description": "Greet someone",
            "command": ["echo", "hello", "{name}"],
            "parameters": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}
        }}}"#,
    )
    .unwrap();
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("t1", "greet", json!({ "name": "Lucia" })),
        FauxResponse::text("Done: greeted."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    assert!(runtime.agent.config.system_prompt.contains("- greet: Greet someone (runs `echo`)"));
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("greet", &tx).await.unwrap();
    let result = outcome
        .new_messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(r) => Some(r.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(text_of(&result.content), "[t1] hello Lucia");

    let restricted = Runtime::new(RuntimeOptions {
        cwd: work.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        tools: Some(vec!["read".into()]),
        session: SessionMode::Ephemeral,
        ..Default::default()
    })
    .unwrap();
    assert!(!restricted.agent.config.tools.iter().any(|t| t.definition().name == "greet"));
}

#[tokio::test]
async fn a_run_publishes_causally_linked_pulses_that_the_audit_log_can_trace() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("t1", "ls", json!({})),
        FauxResponse::text("Done: listed the directory."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    runtime.set_channel("test");
    let mut all = runtime.pulses().subscribe("all", &[]);
    let mut settled = runtime.pulses().subscribe("settled", &[PulseKind::RunSettled]);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("list it", &tx).await.unwrap();

    let mut pulses = Vec::new();
    while let Some(pulse) = all.try_recv() {
        pulses.push(pulse);
    }
    let kinds: Vec<PulseKind> = pulses.iter().map(|p| p.payload.kind()).collect();
    // Each turn's end is a pulse too, for whoever watches the run's course (decision D78).
    assert_eq!(
        kinds,
        [
            PulseKind::UserMessage,
            PulseKind::ToolStarted,
            PulseKind::ToolFinished,
            PulseKind::TurnCompleted,
            PulseKind::TurnCompleted,
            PulseKind::RunSettled
        ]
    );
    let nypi_core::pulse::PulsePayload::TurnCompleted { turn_index, recent_calls_digest, .. } = &pulses[3].payload
    else {
        panic!("a turn's end")
    };
    assert_eq!(*turn_index, 1);
    // The digest says what the call was and how it came out (`ls` is not among the default tools).
    assert!(
        recent_calls_digest[0].starts_with("- ls") && recent_calls_digest[0].contains("-> FAILED"),
        "{recent_calls_digest:?}"
    );
    let root = &pulses[0];
    assert_eq!(root.id, outcome.pulse);
    assert_eq!(root.priority, Priority::User);
    assert!(
        matches!(&root.payload, PulsePayload::UserMessage { delivery: Delivery::Prompt, text } if text == "list it")
    );
    assert!(pulses[1..].iter().all(|p| p.causation.as_ref() == Some(&outcome.pulse)));
    assert_eq!(settled.try_recv().unwrap().payload.kind(), PulseKind::RunSettled);

    // Steering is a user pulse too, even between runs.
    runtime.input().steer(Message::user_text("also count them"));
    let steer = all.try_recv().unwrap();
    assert!(matches!(steer.payload, PulsePayload::UserMessage { delivery: Delivery::Steer, .. }));

    let trace = nypi_core::trace::find(&home.path().join("audit"), &outcome.pulse.0, 1).unwrap();
    assert_eq!(trace.chain.len(), 1);
    assert_eq!(trace.children.len(), 5, "the tool's two pulses, a turn pulse per turn, and the settlement");
    let effects: Vec<&str> = trace.effects.iter().filter_map(|r| r["payload"]["type"].as_str()).collect();
    assert!(effects.contains(&"tool_execution_start") && effects.contains(&"message_end"), "{effects:?}");
    let tool = &trace.children[0];
    let tool_trace =
        nypi_core::trace::find(&home.path().join("audit"), tool["payload"]["id"].as_str().unwrap(), 1).unwrap();
    assert_eq!(tool_trace.chain.len(), 2, "a tool pulse traces back to the user's message");
}

/// A run in which nothing at all happens for `stuckMinutes` is stopped and ends Failed, saying
/// why, instead of keeping its conversation busy for good (principle B6).
#[tokio::test]
async fn a_run_that_makes_no_progress_is_stopped_and_says_so() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(home.path().join("settings.json"), r#"{ "stuckMinutes": 0.02, "sandbox": "off" }"#).unwrap();
    let faux = FauxProvider::new(vec![FauxResponse::tool_call("t1", "bash", json!({ "command": "sleep 60" }))]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    let started = std::time::Instant::now();
    let outcome = runtime.prompt("wait for it", &tx).await.unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(20), "stopped long before the command ended");
    assert_eq!(outcome.status.kind, StatusKind::Failed);
    assert!(outcome.status.text.contains("no progress"), "{}", outcome.status.text);
}
