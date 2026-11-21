//! A run that ends `Continuing` is held to its word (principle B6, decision D59), with the faux
//! provider: it goes on when told its turn is over, it may wait only on a schedule it set, and a
//! promise repeated without either becomes Blocked, so the user hears instead of waiting.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_core::config::Paths;
use nypi_core::ledger::{LedgerEvent, TaskState, now};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::schedule::ScheduleStore;
use nypi_core::status::{StatusKind, continue_nudge};
use serde_json::json;
use tokio::sync::mpsc;

fn runtime(home: &std::path::Path, work: &std::path::Path, faux: &FauxProvider, session: SessionMode) -> Runtime {
    Runtime::new(RuntimeOptions {
        cwd: work.to_path_buf(),
        paths: Some(Paths { home: home.to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session,
        ..Default::default()
    })
    .unwrap()
}

fn user_texts(faux: &FauxProvider, request: usize) -> Vec<String> {
    faux.requests()[request]
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::User(u) => Some(text_of(&u.content)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_promise_with_no_way_back_is_told_to_go_on_and_then_does() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::text("I looked at half of the logs.\n\nContinuing: I will read the rest next."),
        FauxResponse::text("Read the rest: two errors, both timeouts.\n\nDone: all logs read."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux, SessionMode::Ephemeral);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("read the logs", &tx).await.unwrap();

    assert_eq!((outcome.status.kind, outcome.status.synthesized), (StatusKind::Done, false));
    assert!(outcome.answer.contains("half of the logs") && outcome.answer.contains("two errors"), "{}", outcome.answer);
    assert!(outcome.wake.is_empty());
    assert_eq!(faux.requests().len(), 2);
    assert_eq!(user_texts(&faux, 1).last().unwrap(), &continue_nudge(true), "the agent has the schedule tool");
    let task = runtime.ledger().get(&outcome.task.unwrap()).unwrap();
    assert_eq!(task.state, TaskState::Done, "one task, ended by the work, not by the promise");
}

#[tokio::test]
async fn a_promise_stands_when_a_schedule_set_in_the_run_brings_the_agent_back() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let schedule = json!({
        "action": "add",
        "trigger": { "kind": "at", "at": "2099-01-01 09:00" },
        "then": "prompt",
        "text": "Check whether the build finished and report."
    });
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("s1", "schedule", schedule),
        FauxResponse::text("The build is running.\n\nContinuing: I set a schedule and will report when it fires."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux, SessionMode::Ephemeral);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("start the build and tell me how it went", &tx).await.unwrap();

    assert_eq!(outcome.status.kind, StatusKind::Continuing);
    assert_eq!(faux.requests().len(), 2, "no nudge: the promise has a way back");
    let scheduled: Vec<String> = ScheduleStore::new(Paths { home: home.path().to_path_buf() }.schedules())
        .active()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(outcome.wake, scheduled);
    let task = runtime.ledger().get(&outcome.task.unwrap()).unwrap();
    assert_eq!((task.state, &task.wake), (TaskState::Waiting, &scheduled));
    assert!(task.note.starts_with("Continuing:"), "{}", task.note);
    assert!(runtime.interrupted_tasks().is_empty(), "a waiting task is not a cut-off one");
}

#[tokio::test]
async fn a_promise_repeated_without_acting_becomes_blocked() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let stuck = "Continuing: waiting for the deploy to finish.";
    let faux = FauxProvider::new((0..4).map(|_| FauxResponse::text(stuck)).collect());
    let mut runtime = runtime(home.path(), work.path(), &faux, SessionMode::Ephemeral);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("deploy", &tx).await.unwrap();

    assert_eq!(faux.requests().len(), 4, "the first reply and three nudges (maxContinuations)");
    assert_eq!((outcome.status.kind, outcome.status.synthesized), (StatusKind::Blocked, true));
    assert!(outcome.status.text.contains("waiting for the deploy to finish"), "{}", outcome.status.text);
    let task = runtime.ledger().get(&outcome.task.unwrap()).unwrap();
    assert_eq!(task.state, TaskState::BlockedOnUser, "it waits for the user, who is told, not for nothing");
    let mut warned = false;
    while let Ok(event) = rx.try_recv() {
        let line = serde_json::to_string(&event).unwrap();
        warned |= line.contains("\"alert\"") && line.contains("neither went on nor scheduled");
    }
    assert!(warned, "the front end gets an alert");
}

#[tokio::test]
async fn the_word_is_taken_as_given_when_the_rule_is_off_and_a_resume_then_holds_it() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(home.path().join("settings.json"), r#"{ "maxContinuations": 0 }"#).unwrap();
    let faux = FauxProvider::new(vec![FauxResponse::text("Started.\n\nContinuing: the import is running.")]);
    let mut first = runtime(home.path(), work.path(), &faux, SessionMode::New);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = first.prompt("import the data", &tx).await.unwrap();
    assert_eq!(outcome.status.kind, StatusKind::Continuing);
    assert_eq!(faux.requests().len(), 1);
    let (id, session) = (outcome.task.unwrap(), first.session_path());
    assert_eq!(first.ledger().get(&id).unwrap().state, TaskState::Running);

    // Its process dies; the watchdog's background resume opens the session again, rule on.
    let dead_pid = i32::MAX as u32 - 11;
    first.ledger().append(&LedgerEvent::Resumed { task: id.clone(), at: now(), pid: dead_pid }).unwrap();
    drop(first);
    std::fs::remove_file(home.path().join("settings.json")).unwrap();
    let faux = FauxProvider::new(vec![FauxResponse::text("The import finished: 1,204 rows.\n\nDone: imported.")]);
    let mut second = runtime(home.path(), work.path(), &faux, SessionMode::Open(session));
    let outcome = second.resume_task(&id, &tx).await.unwrap();

    assert_eq!(outcome.status.kind, StatusKind::Done);
    assert_eq!(user_texts(&faux, 0).last().unwrap(), &continue_nudge(true), "the resume asks it to go on");
    assert_eq!(second.ledger().get(&id).unwrap().state, TaskState::Done);
}
