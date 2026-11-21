//! The task ledger with the faux provider: runs become tasks, tool intents are recorded, and a
//! task a dead process left is resumed, with the call it cut off answered as interrupted.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{AssistantMessage, ContentBlock, Message, StopReason, text_of};
use nypi_core::config::Paths;
use nypi_core::ledger::{Ledger, LedgerEvent, RESTART_LIMIT, TaskState, new_task_id, now};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::session::SessionStore;
use serde_json::json;
use tokio::sync::mpsc;

/// A pid no process can have, standing for one that crashed.
const DEAD_PID: u32 = i32::MAX as u32 - 7;

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

#[tokio::test]
async fn a_run_is_a_task_with_its_tool_calls_recorded() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("t1", "bash", json!({ "command": "echo hi" })),
        FauxResponse::text("It printed hi.\n\nDone: ran the command."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux, SessionMode::Ephemeral);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("run echo", &tx).await.unwrap();
    let id = outcome.task.expect("the run has a task");
    let task = runtime.ledger().get(&id).unwrap();
    assert_eq!((task.state, task.goal.as_str(), task.attempts), (TaskState::Done, "run echo", 1));
    assert!(task.open_tools.is_empty());
    assert_eq!(task.pulse, Some(outcome.pulse));
    let ledger = std::fs::read_to_string(Paths { home: home.path().to_path_buf() }.ledger()).unwrap();
    let kinds: Vec<String> = ledger
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["event"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(kinds, ["created", "state", "toolStarted", "toolFinished", "state"]);
    assert!(runtime.interrupted_tasks().is_empty());
}

/// A session where the agent was mid-way through a tool call, and the ledger lines its dead
/// process wrote: the task, its running state, and the call's intent.
fn crashed_session(
    home: &std::path::Path,
    work: &std::path::Path,
    prompt: &str,
    in_session: bool,
) -> (std::path::PathBuf, nypi_core::ledger::TaskId) {
    let paths = Paths { home: home.to_path_buf() };
    let mut session = SessionStore::create(&paths.sessions(), work);
    session.append_model_change("faux", "faux-1").unwrap();
    if in_session {
        session.append_message(Message::user_text(prompt)).unwrap();
        let mut assistant = AssistantMessage::empty("faux", "faux", "faux-1");
        assistant.stop_reason = StopReason::ToolUse;
        assistant.content = vec![
            ContentBlock::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "make deploy" }),
                arguments_error: None,
                thought_signature: None,
            },
            ContentBlock::ToolCall {
                id: "c2".into(),
                name: "ls".into(),
                arguments: json!({}),
                arguments_error: None,
                thought_signature: None,
            },
        ];
        session.append_message(Message::Assistant(assistant)).unwrap();
    }
    let ledger = Ledger::new(paths.ledger());
    let task = new_task_id();
    ledger
        .append(&LedgerEvent::Created {
            task: task.clone(),
            at: now(),
            goal: prompt.into(),
            prompt: prompt.into(),
            owner: "lucia".into(),
            origin: "cli".into(),
            parent: None,
            session_id: session.header.id.clone(),
            session_path: Some(session.path.clone()),
            cwd: work.to_path_buf(),
            remote: None,
            pid: DEAD_PID,
            pulse: None,
        })
        .unwrap();
    ledger
        .append(&LedgerEvent::State { task: task.clone(), at: now(), state: TaskState::Running, note: String::new() })
        .unwrap();
    ledger
        .append(&LedgerEvent::ToolStarted {
            task: task.clone(),
            at: now(),
            call_id: "c1".into(),
            tool: "bash".into(),
            summary: "make deploy".into(),
        })
        .unwrap();
    (session.path.clone(), task)
}

#[tokio::test]
async fn an_interrupted_task_resumes_without_rerunning_the_cut_off_call() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (path, id) = crashed_session(home.path(), work.path(), "deploy the site", true);
    let faux = FauxProvider::new(vec![FauxResponse::text("The deploy was cut off; I checked it.\n\nDone: verified.")]);
    let mut runtime = runtime(home.path(), work.path(), &faux, SessionMode::Open(path));
    let interrupted = runtime.interrupted_tasks();
    assert_eq!(interrupted.iter().map(|t| &t.id).collect::<Vec<_>>(), [&id]);

    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.resume_task(&id, &tx).await.unwrap();
    assert_eq!(outcome.task.as_ref(), Some(&id));
    let request = &faux.requests()[0];
    let results: Vec<(String, String)> = request
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some((r.tool_call_id.clone(), text_of(&r.content))),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "no tool ran again; both calls got a result");
    assert!(results[0].0 == "c1" && results[0].1.starts_with("Interrupted:"), "{results:?}");
    assert!(results[1].0 == "c2" && results[1].1.starts_with("Not run:"), "{results:?}");
    let task = runtime.ledger().get(&id).unwrap();
    assert_eq!((task.state, task.attempts), (TaskState::Done, 2));
    assert!(runtime.interrupted_tasks().is_empty());
}

#[tokio::test]
async fn a_prompt_the_session_never_saw_is_sent_again_and_crash_loops_end() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (path, id) = crashed_session(home.path(), work.path(), "summarize the logs", false);
    let faux = FauxProvider::new(vec![FauxResponse::text("Summary.\n\nDone: summarized.")]);
    let mut first = runtime(home.path(), work.path(), &faux, SessionMode::Open(path.clone()));
    let (tx, _rx) = mpsc::unbounded_channel();
    first.resume_task(&id, &tx).await.unwrap();
    let last = faux.requests()[0].messages.last().cloned().unwrap();
    assert!(matches!(last, Message::User(u) if text_of(&u.content) == "summarize the logs"));

    let (path, id) = crashed_session(home.path(), work.path(), "crash me", true);
    let ledger = Ledger::new(Paths { home: home.path().to_path_buf() }.ledger());
    for _ in 1..RESTART_LIMIT {
        ledger.append(&LedgerEvent::Resumed { task: id.clone(), at: now(), pid: DEAD_PID }).unwrap();
    }
    let faux = FauxProvider::new(vec![]);
    let mut runtime = runtime(home.path(), work.path(), &faux, SessionMode::Open(path));
    let error = runtime.resume_task(&id, &tx).await.unwrap_err();
    assert!(format!("{error}").contains("restart limit"), "{error}");
    assert_eq!(ledger.get(&id).unwrap().state, TaskState::Failed);
    assert!(faux.requests().is_empty(), "a task past the limit makes no request");
}

/// A campaign task a dead process left (decisions D78, D85): resumed, it is on its workspace and
/// its goal again, so its plan and its notes are pinned in the prompt as before the crash.
#[tokio::test]
async fn a_resumed_campaign_task_still_sees_its_plan_and_workspace() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::create_dir_all(paths.vault().join("10-projects/shop")).unwrap();
    std::fs::write(
        paths.vault().join("10-projects/shop/plan.md"),
        "# Shop\n\n## Workstreams & Milestones\n- [~] the checkout page\n",
    )
    .unwrap();
    std::fs::create_dir_all(paths.vault().join("10-workspaces/261119-checkout")).unwrap();
    std::fs::write(
        paths.vault().join("10-workspaces/261119-checkout/brief.md"),
        "# Checkout\n\n## Done when\nit pays\n",
    )
    .unwrap();
    std::fs::write(paths.goals(), "## Short term\n- [ ] Launch the shop · vault: 10-projects/shop <!-- g-5a0b01 -->\n")
        .unwrap();
    let (path, id) = crashed_session(home.path(), work.path(), "build the checkout", true);
    let ledger = Ledger::new(paths.ledger());
    ledger
        .append(&LedgerEvent::Workspace {
            task: id.clone(),
            at: now(),
            workspace: "10-workspaces/261119-checkout".into(),
        })
        .unwrap();
    ledger
        .append(&LedgerEvent::Campaign {
            task: id.clone(),
            at: now(),
            goal: Some("g-5a0b01".into()),
            activation: Some("a-1".into()),
        })
        .unwrap();
    let faux = FauxProvider::new(vec![FauxResponse::text("Checked the page.\n\nDone: verified.")]);
    let mut runtime = runtime(home.path(), work.path(), &faux, SessionMode::Open(path));
    let (tx, _rx) = mpsc::unbounded_channel();
    runtime.resume_task(&id, &tx).await.unwrap();

    let prompt = &faux.requests()[0].system_prompt;
    let (plan_at, workspace_at) = (prompt.rfind("<project_plan>"), prompt.rfind("<task_workspace>"));
    assert!(plan_at.is_some() && workspace_at.is_some(), "{prompt}");
    assert!(plan_at < workspace_at && prompt.contains("- [~] the checkout page") && prompt.contains("it pays"));
    assert_eq!(runtime.campaign(), (Some("g-5a0b01".into()), Some("a-1".into())));
}
