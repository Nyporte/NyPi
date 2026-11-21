//! Context recovery (Pi parity, decision D44): an overflowing or cut-off reply is compacted away
//! and the run continues once, the failed reply leaving model context for good. Faux provider only.

use std::path::Path;

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, FauxStep, faux_model};
use nypi_ai::{Message, StopReason, text_of};
use nypi_core::config::Paths;
use nypi_core::runtime::{Runtime, RuntimeEvent, RuntimeOptions, SessionMode};
use nypi_core::session::SessionStore;
use serde_json::json;
use tokio::sync::mpsc;

fn runtime(home: &Path, work: &Path, faux: &FauxProvider) -> Runtime {
    // Compaction keeps only the newest message, so the second prompt always has history to compact.
    let settings = json!({ "enforceStatusLine": false, "compaction": { "keepRecentTokens": 1 } });
    std::fs::write(home.join("settings.json"), settings.to_string()).unwrap();
    Runtime::new(RuntimeOptions {
        cwd: work.to_path_buf(),
        paths: Some(Paths { home: home.to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::New,
        ..Default::default()
    })
    .map(|mut runtime| {
        // A cut-off reply is recoverable only below the limit the request asked for.
        runtime.agent.config.max_tokens = Some(4096);
        runtime
    })
    .unwrap()
}

/// A reply the provider cut off: a length stop well below the faux model's 4096-token limit.
fn cut_off(text: &str) -> FauxResponse {
    FauxResponse { steps: vec![FauxStep::Text(text.into())], stop_reason: StopReason::Length, error: None }
}

fn mentions(messages: &[Message], needle: &str) -> bool {
    messages.iter().any(|m| match m {
        Message::Assistant(a) => a.text().contains(needle),
        Message::User(u) => text_of(&u.content).contains(needle),
        _ => false,
    })
}

fn alerts(rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> Vec<String> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            RuntimeEvent::Alert { alert } => Some(alert.message),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_cut_off_reply_is_compacted_away_and_the_run_continues() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::text("Hello."),
        cut_off("The plan has three parts: first"),
        FauxResponse::text("The user said hello."),
        FauxResponse::text("The plan has three parts: one, two, three."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    runtime.prompt("hello", &tx).await.unwrap();
    let outcome = runtime.prompt("what is the plan?", &tx).await.unwrap();

    assert_eq!(outcome.final_stop, Some(StopReason::Stop));
    assert_eq!(outcome.answer, "The plan has three parts: one, two, three.");
    let requests = faux.requests();
    assert_eq!(requests.len(), 4, "the reply, the summary, and the continued run");
    assert!(!mentions(&requests[3].messages, "parts: first"), "the cut-off reply is not sent again");
    assert!(mentions(&requests[3].messages, "what is the plan?"), "the prompt is answered again");

    let reopened = SessionStore::open(&runtime.session_path()).unwrap();
    assert!(!mentions(&reopened.context().messages, "parts: first"), "omitted for good");
    let in_file = std::fs::read_to_string(runtime.session_path()).unwrap();
    assert!(in_file.contains("parts: first") && in_file.contains("context_edit"));
}

#[tokio::test]
async fn recovery_is_tried_once() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::text("Hello."),
        cut_off("Cut"),
        FauxResponse::text("The user said hello."),
        cut_off("Cut again"),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, mut rx) = mpsc::unbounded_channel();
    runtime.prompt("hello", &tx).await.unwrap();
    let outcome = runtime.prompt("go on", &tx).await.unwrap();

    assert_eq!(outcome.final_stop, Some(StopReason::Length));
    assert_eq!(faux.requests().len(), 4, "no second compact-and-retry");
    let alerts = alerts(&mut rx);
    assert!(alerts.iter().any(|a| a.starts_with("Truncated response recovery failed")), "{alerts:?}");
}

#[tokio::test]
async fn a_provider_overflow_error_is_recovered() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::text("Hello."),
        // Gemini's wording, which the six phrases NyPi checked before did not match.
        FauxResponse::error(
            "HTTP 400: The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
        ),
        FauxResponse::text("The user said hello."),
        FauxResponse::text("Answered."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    runtime.prompt("hello", &tx).await.unwrap();
    let outcome = runtime.prompt("question", &tx).await.unwrap();

    assert_eq!(outcome.answer, "Answered.");
    assert_eq!(faux.requests().len(), 4);
}

#[tokio::test]
async fn a_reply_at_the_output_limit_is_not_recovered() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut model = faux_model();
    // The faux provider reports 5 output tokens: at this limit, the stop is the model's own.
    model.max_tokens = 5;
    let faux = FauxProvider::new(vec![FauxResponse::text("Hello."), cut_off("Long answer")]);
    let settings = json!({ "enforceStatusLine": false, "compaction": { "keepRecentTokens": 1 } });
    std::fs::write(home.path().join("settings.json"), settings.to_string()).unwrap();
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: work.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(model),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        ..Default::default()
    })
    .unwrap();
    runtime.agent.config.max_tokens = Some(5);
    let (tx, _rx) = mpsc::unbounded_channel();
    runtime.prompt("hello", &tx).await.unwrap();
    let outcome = runtime.prompt("write a lot", &tx).await.unwrap();

    assert_eq!(outcome.final_stop, Some(StopReason::Length));
    assert_eq!(faux.requests().len(), 2);
}

#[tokio::test]
async fn a_length_stop_is_not_recovered_when_the_server_default_limit_applied() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![FauxResponse::text("Hello."), cut_off("A long answer the server cut")]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    // As with OpenAI-compatible gateways, nothing asks for a limit: the stop may be the
    // server's own default, so the reply stays as it is.
    runtime.agent.config.max_tokens = None;
    let (tx, _rx) = mpsc::unbounded_channel();
    runtime.prompt("hello", &tx).await.unwrap();
    let outcome = runtime.prompt("write a lot", &tx).await.unwrap();

    assert_eq!(outcome.final_stop, Some(StopReason::Length));
    assert_eq!(outcome.answer, "A long answer the server cut");
    assert_eq!(faux.requests().len(), 2, "no compaction, no retry");
}
