//! The chat/worker split, driven by two faux providers: chat delegates, the worker runs in the
//! background, results come back as pulses, and a failing chat model hands the message on.

use std::time::Duration;

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_core::chat::{self, ChatOptions, ModuleEvent};
use nypi_core::config::Paths;
use nypi_core::pulse::{Delivery, PulseKind, PulsePayload, PulseSource};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::status::StatusKind;
use nypi_core::worker::{self, WorkerEvent};
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
    tokio::time::timeout(Duration::from_secs(5), events.recv()).await.expect("event in time").expect("open")
}

/// Chat hands the request up and replies; the Principal starts the task; the worker's report
/// reaches chat as context (decision D84).
#[tokio::test]
async fn chat_signals_the_principal_which_starts_the_task_and_chat_learns_the_result() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let worker_model = FauxProvider::new(vec![
        FauxResponse::tool_call("w1", "ls", json!({})),
        FauxResponse::text("There are no files.\n\nDone: counted the files."),
    ]);
    let chat_model = FauxProvider::new(vec![
        FauxResponse::tool_call(
            "c1",
            "signal_principal",
            json!({ "topic": "Count the files in the working directory" }),
        ),
        FauxResponse::text("On it."),
        FauxResponse::text("You're welcome."),
    ]);
    let principal_model = FauxProvider::new(vec![
        FauxResponse::tool_call(
            "p1",
            "start_task",
            json!({ "topic": "Count the files", "instruction": "Count the files in the working directory." }),
        ),
        FauxResponse::text("Started a task to count the files."),
    ]);
    let runtime = runtime(home.path(), work.path(), &worker_model);
    let mut pulses = runtime.pulses().subscribe("test", &[]);
    let options = ChatOptions {
        model: Some("faux/faux-1".into()),
        stream_fn: Some(chat_model.stream_fn()),
        principal_stream_fn: Some(principal_model.stream_fn()),
    };
    let (chat, mut events) = chat::start(runtime, options).unwrap();

    chat.say("How many files are there?");
    let (mut reply, mut settled) = (None, None);
    while reply.is_none() || settled.is_none() {
        match next(&mut events).await {
            ModuleEvent::Replied { text, .. } => reply = Some(text),
            ModuleEvent::Worker(WorkerEvent::Settled { outcome, .. }) => settled = Some(outcome.unwrap()),
            _ => {}
        }
    }
    assert_eq!(reply.as_deref(), Some("On it."));
    assert_eq!(settled.unwrap().status.kind, StatusKind::Done);

    // The Principal was handed the conversation with chat's reply in it, and the worker got
    // the Principal's brief with the owner's own words.
    let handed = text_of(&match principal_model.requests()[0].messages.last().cloned().unwrap() {
        Message::User(u) => u.content,
        other => panic!("{other:?}"),
    });
    assert!(handed.contains("User: How many files are there?") && handed.contains("You: On it."), "{handed}");
    let task = match worker_model.requests()[0].messages.last().cloned().unwrap() {
        Message::User(u) => text_of(&u.content),
        other => panic!("{other:?}"),
    };
    assert!(
        task.contains("Count the files in the working directory.") && task.contains("User: How many files"),
        "{task}"
    );

    let mut all = Vec::new();
    while let Some(pulse) = pulses.try_recv() {
        all.push(pulse);
    }
    let root = all.iter().find(|p| p.payload.kind() == PulseKind::UserMessage).unwrap();
    assert_eq!(root.source, PulseSource::user("test"));
    let delegated = all.iter().find(|p| p.payload.kind() == PulseKind::Delegated).unwrap();
    assert_eq!(delegated.causation.as_ref(), Some(&root.id), "the task traces back to the owner's message");
    assert!(matches!(delegated.payload, PulsePayload::Delegated { delivery: Delivery::Prompt, .. }));
    let tool = all.iter().find(|p| p.payload.kind() == PulseKind::ToolStarted).unwrap();
    assert_eq!(tool.causation.as_ref(), Some(&delegated.id), "worker pulses trace back through the delegation");
    let reply_pulse = all.iter().find(|p| p.payload.kind() == PulseKind::Reply).unwrap();
    assert_eq!(reply_pulse.causation.as_ref(), Some(&root.id));

    chat.say("Thanks!");
    loop {
        if let ModuleEvent::Replied { text, .. } = next(&mut events).await {
            assert_eq!(text, "You're welcome.");
            break;
        }
    }
    let third = &chat_model.requests()[2];
    let note = third.messages.iter().rev().nth(1).unwrap();
    assert!(
        matches!(note, Message::User(u) if text_of(&u.content).starts_with("[task result, already shown to the user · Done")),
        "{note:?}"
    );
    assert!(third.system_prompt.contains("## Tasks\nIdle."));
    assert!(
        third.tools.iter().all(|tool| !["start_task", "bash", "write"].contains(&tool.name.as_str())),
        "chat acts on nothing"
    );
}

#[tokio::test]
async fn a_failing_chat_model_hands_the_message_to_the_principal() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let worker_model = FauxProvider::new(vec![FauxResponse::text("Done: renamed it.")]);
    let chat_model = FauxProvider::new(vec![FauxResponse::error("chat model unavailable")]);
    let principal_model = FauxProvider::new(vec![
        FauxResponse::tool_call("p1", "start_task", json!({ "topic": "Rename the report" })),
        FauxResponse::text("Started it."),
    ]);
    let runtime = runtime(home.path(), work.path(), &worker_model);
    let options = ChatOptions {
        model: Some("faux/faux-1".into()),
        stream_fn: Some(chat_model.stream_fn()),
        principal_stream_fn: Some(principal_model.stream_fn()),
    };
    let (chat, mut events) = chat::start(runtime, options).unwrap();
    chat.say("Rename the report.");
    let mut failed = false;
    loop {
        match next(&mut events).await {
            ModuleEvent::ChatFailed { error, delegated } => {
                assert!(error.contains("unavailable") && delegated);
                failed = true;
            }
            ModuleEvent::Worker(WorkerEvent::Settled { task, outcome, .. }) => {
                assert!(task.contains("Rename the report"), "{task}");
                assert_eq!(outcome.unwrap().status.kind, StatusKind::Done);
                break;
            }
            _ => {}
        }
    }
    assert!(failed);
    let handed = format!("{:?}", principal_model.requests()[0].messages);
    assert!(handed.contains("Topic: Rename the report."), "{handed}");
}

#[tokio::test]
async fn the_worker_refuses_a_second_task_and_runs_commands_when_idle() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let model = FauxProvider::new(vec![FauxResponse::text("Done: first.")]);
    let (worker, mut events) = worker::spawn(runtime(home.path(), work.path(), &model));
    let source = || PulseSource::module("test");
    worker.start("first", source(), None).unwrap();
    assert!(worker.start("second", source(), None).unwrap_err().contains("busy"));
    assert!(worker.instruct("also this", Delivery::Steer, source(), None).is_ok());
    let model_id = worker.with(|runtime| Box::pin(async move { runtime.model().id.clone() })).await;
    assert_eq!(model_id.as_deref(), Some("faux-1"));
    // The job ran after the task, so the task has settled by now.
    let mut settled = false;
    while let Ok(event) = events.try_recv() {
        settled |= matches!(event, WorkerEvent::Settled { .. });
    }
    assert!(settled);
    assert!(worker.instruct("late", Delivery::Steer, source(), None).is_err());
}

#[tokio::test]
async fn context_added_to_chat_reaches_its_next_request() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let chat_model = FauxProvider::new(vec![FauxResponse::text("It printed hello.")]);
    let runtime = runtime(home.path(), work.path(), &FauxProvider::new(vec![]));
    let options = ChatOptions {
        model: Some("faux/faux-1".into()),
        stream_fn: Some(chat_model.stream_fn()),
        ..Default::default()
    };
    let (chat, mut events) = chat::start(runtime, options).unwrap();

    // What a user's `!echo hello` adds, as the TUI and terminal chat mode hand it over.
    chat.add_context(Message::user_text("Ran `echo hello`\n```\nhello\n```"));
    chat.say("what did it print?");
    loop {
        if let ModuleEvent::Replied { .. } = next(&mut events).await {
            break;
        }
    }
    let sent = &chat_model.requests()[0].messages;
    assert!(sent.iter().any(|m| matches!(m, Message::User(u) if text_of(&u.content).starts_with("Ran `echo hello`"))));
}
