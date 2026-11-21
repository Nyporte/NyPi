//! Retries of transient provider errors (Pi parity, decisions D42 and D43): runs, compaction
//! summaries, one-shot calls, and the chat module, driven by the faux provider.

use std::path::Path;
use std::time::Duration;

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::retry::RetryPolicy;
use nypi_ai::{Message, StopReason, ThinkingLevel};
use nypi_core::chat::{self, ChatOptions, ModuleEvent};
use nypi_core::config::Paths;
use nypi_core::retry::RetryNotice;
use nypi_core::runtime::{Runtime, RuntimeEvent, RuntimeOptions, SessionMode};
use serde_json::json;
use tokio::sync::mpsc;

/// The error a proxy that drops the connection produces (reqwest's wording).
const DROPPED: &str = "Request failed: error sending request for url (http://127.0.0.1:9/v1/chat/completions)";

/// A runtime whose settings are `settings` plus no status-line nudges.
fn runtime(home: &Path, work: &Path, faux: &FauxProvider, mut settings: serde_json::Value) -> Runtime {
    settings["enforceStatusLine"] = json!(false);
    std::fs::write(home.join("settings.json"), settings.to_string()).unwrap();
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

/// A notice in short form: `start n/max`, `end ok n`, `end failed n: error`, `summary n/max`, ...
fn short(notice: RetryNotice) -> String {
    match notice {
        RetryNotice::AutoRetryStart { attempt, max_attempts, .. } => format!("start {attempt}/{max_attempts}"),
        RetryNotice::AutoRetryEnd { success: true, attempt, .. } => format!("end ok {attempt}"),
        RetryNotice::AutoRetryEnd { success: false, attempt, final_error } => {
            format!("end failed {attempt}: {}", final_error.unwrap_or_default())
        }
        RetryNotice::SummarizationRetryScheduled { attempt, max_attempts, .. } => {
            format!("summary {attempt}/{max_attempts}")
        }
        RetryNotice::SummarizationRetryAttemptStart { source, reason } => format!("summary again: {source}, {reason}"),
        RetryNotice::SummarizationRetryFinished => "summary finished".into(),
    }
}

/// The runtime's retry notices so far, in order.
fn retry_events(rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> Vec<String> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            RuntimeEvent::Retry { notice } => Some(short(notice)),
            _ => None,
        })
        .collect()
}

fn audit_log(home: &Path) -> String {
    let mut text = String::new();
    for entry in std::fs::read_dir(home.join("audit")).unwrap() {
        text.push_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    text
}

fn failed_replies(messages: &[Message]) -> usize {
    messages.iter().filter(|m| matches!(m, Message::Assistant(a) if a.stop_reason == StopReason::Error)).count()
}

#[tokio::test]
async fn a_dropped_connection_is_retried_and_the_run_succeeds() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![FauxResponse::error(DROPPED), FauxResponse::text("The answer is 42.")]);
    let mut runtime = runtime(home.path(), work.path(), &faux, json!({ "retry": { "baseDelayMs": 1 } }));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("question", &tx).await.unwrap();

    assert_eq!(outcome.final_stop, Some(StopReason::Stop));
    assert_eq!(outcome.answer, "The answer is 42.");
    assert_eq!(retry_events(&mut rx), ["start 1/3", "end ok 1"]);
    let requests = faux.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(failed_replies(&requests[1].messages), 0, "the failed reply is not sent again");
    assert!(matches!(requests[1].messages.last(), Some(Message::User(_))), "the retry repeats the same request");
    assert_eq!(failed_replies(&runtime.agent.messages), 0);
    let audit = audit_log(home.path());
    assert!(audit.contains("auto_retry_start") && audit.contains("auto_retry_end"), "{audit}");
}

#[tokio::test]
async fn deterministic_errors_and_disabled_retries_fail_at_once() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![FauxResponse::error("HTTP 401: invalid api key"), FauxResponse::text("unused")]);
    let mut runtime = runtime(home.path(), work.path(), &faux, json!({ "retry": { "baseDelayMs": 1 } }));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("question", &tx).await.unwrap();
    assert_eq!(outcome.final_stop, Some(StopReason::Error));
    assert_eq!(faux.requests().len(), 1);
    assert!(retry_events(&mut rx).is_empty());

    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![FauxResponse::error(DROPPED), FauxResponse::text("unused")]);
    let mut runtime =
        self::runtime(home.path(), work.path(), &faux, json!({ "retry": { "enabled": false, "baseDelayMs": 1 } }));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("question", &tx).await.unwrap();
    assert_eq!(outcome.final_stop, Some(StopReason::Error));
    assert_eq!(faux.requests().len(), 1);
    assert!(retry_events(&mut rx).is_empty());
}

#[tokio::test]
async fn a_context_overflow_is_left_to_compaction() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // Retryable wording, but an overflow: retrying the same request cannot succeed.
    let faux = FauxProvider::new(vec![FauxResponse::error("HTTP 503: prompt is too long"), FauxResponse::text("x")]);
    let mut runtime = runtime(home.path(), work.path(), &faux, json!({ "retry": { "baseDelayMs": 1 } }));
    let (tx, mut rx) = mpsc::unbounded_channel();
    runtime.prompt("question", &tx).await.unwrap();
    assert!(retry_events(&mut rx).is_empty());
    assert_eq!(faux.requests().len(), 1, "nothing to compact yet, so nothing is sent again");
}

#[tokio::test]
async fn retries_stop_when_the_budget_is_spent() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::error("HTTP 503: unavailable"),
        FauxResponse::error("HTTP 503: unavailable"),
        FauxResponse::error("HTTP 503: still unavailable"),
    ]);
    let mut runtime =
        runtime(home.path(), work.path(), &faux, json!({ "retry": { "maxRetries": 2, "baseDelayMs": 1 } }));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("question", &tx).await.unwrap();

    assert_eq!(outcome.final_stop, Some(StopReason::Error));
    assert_eq!(outcome.error.as_deref(), Some("HTTP 503: still unavailable"));
    assert_eq!(faux.requests().len(), 3, "the first attempt and two retries");
    assert_eq!(retry_events(&mut rx), ["start 1/2", "start 2/2", "end failed 2: HTTP 503: still unavailable"]);

    // The next prompt starts with a fresh budget.
    faux.push(FauxResponse::error(DROPPED));
    faux.push(FauxResponse::text("Back."));
    let outcome = runtime.prompt("again", &tx).await.unwrap();
    assert_eq!(outcome.answer, "Back.");
    assert_eq!(retry_events(&mut rx), ["start 1/2", "end ok 1"]);
}

#[tokio::test]
async fn a_reply_that_comes_through_resets_the_budget_mid_run() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(work.path().join("notes.txt"), "hello").unwrap();
    let faux = FauxProvider::new(vec![
        FauxResponse::error(DROPPED),
        FauxResponse::tool_call("t1", "read", json!({ "path": "notes.txt" })),
        FauxResponse::error("Stream error: error decoding response body"),
        FauxResponse::text("It says hello."),
    ]);
    let mut runtime =
        runtime(home.path(), work.path(), &faux, json!({ "retry": { "maxRetries": 1, "baseDelayMs": 1 } }));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("read notes.txt", &tx).await.unwrap();

    assert_eq!(outcome.answer, "It says hello.");
    assert_eq!(faux.requests().len(), 4);
    assert_eq!(retry_events(&mut rx), ["start 1/1", "end ok 1", "start 1/1", "end ok 1"]);
    let last = faux.requests().pop().unwrap();
    assert!(matches!(last.messages.last(), Some(Message::ToolResult(r)) if r.tool_call_id == "t1"));
}

#[tokio::test]
async fn an_abort_ends_the_backoff() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![FauxResponse::error(DROPPED), FauxResponse::text("unused")]);
    let mut runtime = runtime(home.path(), work.path(), &faux, json!({ "retry": { "baseDelayMs": 60_000 } }));
    let handle = runtime.agent.handle();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (seen_tx, mut seen) = mpsc::unbounded_channel();
    let watcher = async move {
        while let Some(event) = rx.recv().await {
            if matches!(event, RuntimeEvent::Retry { notice: RetryNotice::AutoRetryStart { .. } }) {
                handle.abort();
            }
            let _ = seen_tx.send(event);
        }
    };
    let run = async move {
        let outcome = runtime.prompt("question", &tx).await.unwrap();
        drop(tx);
        outcome
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(run, watcher) })
        .await
        .expect("the abort ends the 60 s wait");

    assert_eq!(outcome.final_stop, Some(StopReason::Error));
    assert_eq!(faux.requests().len(), 1);
    assert_eq!(retry_events(&mut seen), ["start 1/3", "end failed 1: Retry cancelled"]);
}

#[tokio::test]
async fn a_compaction_summary_is_retried() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::text("Hello."),
        FauxResponse::error("Stream error: error decoding response body"),
        FauxResponse::text("The user said hello."),
    ]);
    let settings = json!({ "retry": { "baseDelayMs": 1 }, "compaction": { "keepRecentTokens": 1 } });
    let mut runtime = runtime(home.path(), work.path(), &faux, settings);
    let (tx, mut rx) = mpsc::unbounded_channel();
    runtime.prompt("hello", &tx).await.unwrap();
    runtime.compact("manual", &tx).await.unwrap();

    assert_eq!(faux.requests().len(), 3);
    assert_eq!(retry_events(&mut rx), ["summary 1/3", "summary again: compaction, manual", "summary finished"]);
    assert!(audit_log(home.path()).contains("summarization_retry_scheduled"));
}

#[tokio::test]
async fn an_abort_ends_a_compaction_summary_backoff() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![FauxResponse::text("Hello."), FauxResponse::error(DROPPED)]);
    let settings = json!({ "retry": { "baseDelayMs": 60_000 }, "compaction": { "keepRecentTokens": 1 } });
    let mut runtime = runtime(home.path(), work.path(), &faux, settings);
    let (tx, _rx) = mpsc::unbounded_channel();
    runtime.prompt("hello", &tx).await.unwrap();

    let handle = runtime.agent.handle();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let watcher = async move {
        while let Some(event) = rx.recv().await {
            if matches!(event, RuntimeEvent::Retry { notice: RetryNotice::SummarizationRetryScheduled { .. } }) {
                handle.abort();
            }
        }
    };
    let compact = async move {
        let result = runtime.compact("manual", &tx).await;
        drop(tx);
        result
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(compact, watcher) })
        .await
        .expect("the abort ends the 60 s wait");
    let error = result.unwrap_err().to_string();
    assert!(error.contains("aborted"), "{error}");
    assert_eq!(faux.requests().len(), 2);
}

#[tokio::test]
async fn one_shot_calls_retry_transient_errors_only() {
    let model = faux_model();
    let policy = RetryPolicy { base_delay_ms: 1, ..Default::default() };
    let call = |faux: &FauxProvider| {
        let (stream_fn, model, policy) = (faux.stream_fn(), model.clone(), policy.clone());
        async move {
            nypi_core::oneshot::complete(
                &model,
                &stream_fn,
                None,
                "Answer in JSON.",
                "Anything?".into(),
                ThinkingLevel::Off,
                &policy,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
        }
    };

    let faux = FauxProvider::new(vec![FauxResponse::error(DROPPED), FauxResponse::text("{}")]);
    assert_eq!(call(&faux).await.unwrap(), "{}");
    assert_eq!(faux.requests().len(), 2);

    let faux = FauxProvider::new(vec![FauxResponse::error("HTTP 400: bad request"), FauxResponse::text("{}")]);
    assert!(call(&faux).await.unwrap_err().to_string().contains("HTTP 400"));
    assert_eq!(faux.requests().len(), 1);
}

#[tokio::test]
async fn the_chat_module_retries_its_reply() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let worker_model = FauxProvider::new(vec![]);
    let chat_model = FauxProvider::new(vec![FauxResponse::error(DROPPED), FauxResponse::text("Hello!")]);
    let runtime = runtime(home.path(), work.path(), &worker_model, json!({ "retry": { "baseDelayMs": 1 } }));
    let options = ChatOptions {
        model: Some("faux/faux-1".into()),
        stream_fn: Some(chat_model.stream_fn()),
        ..Default::default()
    };
    let (chat, mut events) = chat::start(runtime, options).unwrap();

    chat.say("hi");
    let mut notices = Vec::new();
    let reply = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap().unwrap();
        match event {
            ModuleEvent::Retry { notice } => notices.push(short(notice)),
            ModuleEvent::Replied { text, .. } => break text,
            ModuleEvent::ChatFailed { error, .. } => panic!("chat failed: {error}"),
            _ => {}
        }
    };
    assert_eq!(reply, "Hello!");
    assert_eq!(notices, ["start 1/3", "end ok 1"]);
    assert_eq!(chat_model.requests().len(), 2);
    assert!(worker_model.requests().is_empty(), "nothing went to the worker");
}

#[tokio::test]
async fn an_abort_during_compaction_before_a_prompt_stops_the_prompt() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::text("Hello."),
        // The compaction after the first prompt fails for good, so the next one has work.
        FauxResponse::error("HTTP 400: no summary"),
        FauxResponse::error(DROPPED),
        FauxResponse::text("never requested"),
    ]);
    // A tiny threshold compacts around every prompt.
    let settings = json!({
        "retry": { "baseDelayMs": 60_000 },
        "compaction": { "reserveTokens": 99_990, "keepRecentTokens": 1 },
    });
    let mut runtime = runtime(home.path(), work.path(), &faux, settings);
    let (tx, _rx) = mpsc::unbounded_channel();
    runtime.prompt("hello", &tx).await.unwrap();
    assert_eq!(faux.requests().len(), 2);

    let handle = runtime.agent.handle();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let watcher = async move {
        while let Some(event) = rx.recv().await {
            if matches!(event, RuntimeEvent::Retry { notice: RetryNotice::SummarizationRetryScheduled { .. } }) {
                handle.abort();
            }
        }
    };
    let run = async move {
        let outcome = runtime.prompt("second", &tx).await.unwrap();
        drop(tx);
        (outcome, runtime)
    };
    let ((outcome, runtime), ()) = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(run, watcher) })
        .await
        .expect("the abort ends the 60 s wait");

    assert_eq!(outcome.final_stop, Some(StopReason::Aborted));
    assert_eq!(faux.requests().len(), 3, "the prompt itself is never sent");
    let recorded = runtime
        .agent
        .messages
        .iter()
        .any(|m| matches!(m, Message::User(u) if nypi_ai::text_of(&u.content) == "second"));
    assert!(recorded, "the prompt stays in the transcript");
}
