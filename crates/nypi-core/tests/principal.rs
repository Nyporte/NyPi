//! The Principal's scheduling of its activations (decision D84), with scripted models: the
//! owner's signal suspends an event's activation with the steps it has done, runs first, and the
//! suspended one resumes told what was decided meanwhile; waiting activations run by priority,
//! the owner's first and idle time last.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, FauxStep, faux_model};
use nypi_ai::{Context, Message, StopReason, text_of};
use nypi_core::chat::{self, ChatOptions, ModuleEvent};
use nypi_core::config::Paths;
use nypi_core::principal::{ActivationEvent, DecisionJournal, Priority, Trigger, activation_lines, read_activation};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use serde_json::json;
use tokio::sync::Notify;

fn runtime(home: &std::path::Path, work: &std::path::Path) -> Runtime {
    std::fs::write(home.join("settings.json"), r#"{ "sandbox": "off" }"#).unwrap();
    runtime_as_set(home, work)
}

/// A runtime under the settings already in `home`.
fn runtime_as_set(home: &std::path::Path, work: &std::path::Path) -> Runtime {
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: work.to_path_buf(),
        paths: Some(Paths { home: home.to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(FauxProvider::new(vec![]).stream_fn()),
        session: SessionMode::Ephemeral,
        ..Default::default()
    })
    .unwrap();
    runtime.set_channel("test");
    runtime
}

fn said(message: &Message) -> Option<String> {
    match message {
        Message::User(user) => Some(text_of(&user.content)),
        _ => None,
    }
}

fn first_said(ctx: &Context) -> String {
    let opening = ctx.messages.iter().find_map(said).unwrap_or_default();
    nypi_core::principal::activation_text(&opening).to_string()
}

fn last_said(ctx: &Context) -> String {
    ctx.messages.iter().rev().find_map(said).unwrap_or_default()
}

fn tool_results(ctx: &Context) -> usize {
    ctx.messages.iter().filter(|m| matches!(m, Message::ToolResult(_))).count()
}

fn held(notify: &Arc<Notify>, then: &str) -> FauxResponse {
    FauxResponse {
        steps: vec![FauxStep::Hold(notify.clone()), FauxStep::Text(then.into())],
        stop_reason: StopReason::Stop,
        error: None,
    }
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < Duration::from_secs(20), "{what} never happened");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn options(chat: &FauxProvider, principal: &FauxProvider) -> ChatOptions {
    ChatOptions {
        model: Some("faux/faux-1".into()),
        stream_fn: Some(chat.stream_fn()),
        principal_stream_fn: Some(principal.stream_fn()),
    }
}

#[tokio::test]
async fn the_owners_signal_suspends_an_event_which_resumes_told_what_was_decided() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    let hold = Arc::new(Notify::new());
    let principal_model = FauxProvider::answering({
        let hold = hold.clone();
        move |ctx| {
            let opened = first_said(ctx);
            Some(if opened.starts_with("[Schedule") {
                if last_said(ctx).starts_with("<resumption_delta>\n[Resumed") {
                    FauxResponse::text("Reviewed the goals after the owner's request.")
                } else if tool_results(ctx) == 0 {
                    FauxResponse::tool_call("g1", "goals", json!({ "action": "list" }))
                } else {
                    // Its second step is under way when the owner speaks.
                    held(&hold, "never finished")
                }
            } else {
                FauxResponse::text("Decided: the flight waits until the owner names a date.")
            })
        }
    });
    let chat_model = FauxProvider::answering(|ctx| {
        Some(if matches!(ctx.messages.last(), Some(Message::ToolResult(_))) {
            FauxResponse::text("I'll look into it.")
        } else {
            FauxResponse::tool_call("c1", "signal_principal", json!({ "topic": "Plan the flight" }))
        })
    });
    let (chat, mut events) =
        chat::start(runtime(home.path(), work.path()), options(&chat_model, &principal_model)).unwrap();
    let principal = chat.principal().clone();
    let event = principal.wake(Trigger::SchedulePrompt {
        schedule: "s-1".into(),
        text: "Review the goals.".into(),
        goal: None,
        focus: None,
    });
    until("the event's second step", || principal_model.requests().len() == 2).await;
    let active = principal.status().active.unwrap();
    assert_eq!((active.id.as_str(), active.priority, active.steps), (event.as_str(), Priority::P1, 2));

    chat.say("Plan me a flight");
    loop {
        if let ModuleEvent::Replied { .. } =
            tokio::time::timeout(Duration::from_secs(20), events.recv()).await.unwrap().unwrap()
        {
            break;
        }
    }
    // The owner's activation ran and was decided; the event came back and finished.
    until("both decisions", || DecisionJournal::new(&paths).load().len() == 2).await;
    let decisions = DecisionJournal::new(&paths).load();
    assert_eq!(decisions[0].priority, "P0 User");
    assert!(decisions[0].summary.contains("the flight waits"));
    assert_eq!((decisions[1].activation_id.as_str(), decisions[1].priority.as_str()), (event.as_str(), "P1 Event"));
    assert!(decisions[1].summary.contains("Reviewed the goals"));

    // Suspended with its steps, and resumed told what the owner's activation decided.
    let story = read_activation(&paths, &event);
    let suspended = story.iter().find_map(|event| match event {
        ActivationEvent::Suspended { by_activation, step_count, .. } => Some((by_activation.clone(), *step_count)),
        _ => None,
    });
    let (by, steps) = suspended.expect("suspended");
    assert_eq!(by, decisions[0].activation_id);
    assert_eq!(steps, 2, "the tool call and its result, not the request that was stopped");
    let delta = story.iter().find_map(|event| match event {
        ActivationEvent::Resumed { delta_summary, .. } => Some(delta_summary.clone()),
        _ => None,
    });
    assert!(delta.unwrap().contains("the flight waits"));
    // What it had done was kept: the resumed request carries the goals call and its result,
    // and nothing of the request that was stopped.
    let resumed = principal_model
        .requests()
        .into_iter()
        .find(|ctx| last_said(ctx).starts_with("<resumption_delta>\n[Resumed"))
        .unwrap();
    assert_eq!(tool_results(&resumed), 1);
    assert!(!format!("{:?}", resumed.messages).contains("never finished"));
    assert!(last_said(&resumed).contains("Plan the flight (P0 User)"), "{}", last_said(&resumed));
    let states: Vec<String> =
        activation_lines(&paths).into_iter().filter(|line| line.id == event).map(|line| line.state).collect();
    assert_eq!(states, ["created", "started", "suspended", "resumed", "finished"]);
    assert!(principal.status().suspended.is_empty() && principal.status().active.is_none());
}

#[tokio::test]
async fn waiting_activations_run_by_priority() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    let hold = Arc::new(Notify::new());
    let principal_model = FauxProvider::answering({
        let hold = hold.clone();
        move |ctx| {
            let opened = first_said(ctx);
            Some(if opened.contains("Topic: first") {
                held(&hold, "Handled the first.")
            } else if opened.starts_with("[Idle") {
                FauxResponse::text("NOTHING")
            } else {
                FauxResponse::text("Handled the event.")
            })
        }
    });
    let (chat, _events) =
        chat::start(runtime(home.path(), work.path()), options(&FauxProvider::new(vec![]), &principal_model)).unwrap();
    let principal = chat.principal().clone();
    let signal = |topic: &str| Trigger::UserSignal {
        topic: topic.into(),
        urgency: "normal".into(),
        conversation: format!("User: {topic}"),
        gathered: String::new(),
        focus: None,
    };
    let first = principal.wake(signal("first"));
    until("the first activation", || principal.status().active.is_some()).await;
    // Idle time, then an event, while the owner's runs: neither preempts it.
    let idle = principal.wake(Trigger::Idle { why: "quiet".into(), digest: "Nothing open.".into() });
    let fired = principal.wake(Trigger::SchedulePrompt {
        schedule: "s-2".into(),
        text: "Check the backup.".into(),
        goal: None,
        focus: None,
    });
    until("both waiting", || principal.status().queued.len() == 2).await;
    let queued: Vec<Priority> = principal.status().queued.iter().map(|view| view.priority).collect();
    assert_eq!(queued, [Priority::P1, Priority::P2]);
    assert!(principal.status().describe().contains("Queued: "));
    hold.notify_one();
    until("all three decided", || DecisionJournal::new(&paths).load().len() == 3).await;
    let order: Vec<String> = DecisionJournal::new(&paths).load().into_iter().map(|d| d.activation_id).collect();
    assert_eq!(order, [first, fired, idle]);
}

/// What the Principal sets for a goal says so (decision D85): a schedule records its goal and
/// the activation that set it, and the decision counts the goal as touched.
#[tokio::test]
async fn a_schedule_the_principal_sets_for_a_goal_records_both() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    let goal = nypi_core::goals::GoalsFile::new(paths.goals())
        .add(nypi_core::goals::Term::Short, "Renew the passport", &Default::default(), "test")
        .unwrap();
    let principal_model = FauxProvider::new(vec![
        FauxResponse::tool_call(
            "p1",
            "schedule",
            json!({
                "action": "add", "trigger": { "kind": "at", "at": "2099-01-01 09:00" }, "then": "notify",
                "text": "Book the passport appointment", "goal": goal,
            }),
        ),
        FauxResponse::text("Set a reminder for the passport."),
    ]);
    let (chat, _events) =
        chat::start(runtime(home.path(), work.path()), options(&FauxProvider::new(vec![]), &principal_model)).unwrap();
    let activation = chat.principal().wake(Trigger::UserSignal {
        topic: "Remind me about the passport".into(),
        urgency: "normal".into(),
        conversation: "User: remind me about the passport".into(),
        gathered: String::new(),
        focus: None,
    });
    until("the decision", || !DecisionJournal::new(&paths).load().is_empty()).await;
    let schedule = nypi_core::schedule::ScheduleStore::new(paths.schedules()).active().pop().unwrap();
    assert_eq!(schedule.goal.as_deref(), Some(goal.as_str()));
    assert_eq!(schedule.spawned_by_activation.as_deref(), Some(activation.as_str()));
    let requests = principal_model.requests();
    let schedule_tool = requests[0].tools.iter().find(|tool| tool.name == "schedule").unwrap();
    assert!(schedule_tool.parameters["properties"]["goal"].is_object());
    assert_eq!(DecisionJournal::new(&paths).load()[0].goals_touched, [goal]);
}

/// The four-ring context's first two (decision D87): the system prompt is the charter, the
/// owner's portrait, and who Lucia is, the same for every activation and turn, while what
/// changes (the date, the goals, the tasks) opens each activation. And nothing wakes the
/// Principal for nothing: an owner's request it ended doing nothing about is nudged once, and
/// `conclude_activation` disposes of it with a reason; idle time is not nudged.
#[tokio::test]
async fn the_system_prompt_stays_the_same_and_an_activation_that_did_nothing_is_nudged_once() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::write(
        home.path().join("settings.json"),
        json!({ "sandbox": "off", "principal": { "enforceDisposition": "always" }, "portrait": { "mode": "always" } })
            .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(paths.portrait().parent().unwrap()).unwrap();
    let evidence =
        json!([{ "quote": "keep it short please", "pulse": "p-1", "session": "s", "at": "2026-11-19T08:00:00Z" }]);
    std::fs::write(
        paths.portrait(),
        format!(
            "{}\n",
            json!({ "event": "added", "id": "r-1", "at": "2026-11-19T08:00:00Z", "category": "style",
                    "text": "Wants answers short", "evidence": evidence })
        ),
    )
    .unwrap();
    let principal_model = FauxProvider::answering(|ctx| {
        let opened = first_said(ctx);
        let nudged = last_said(ctx).starts_with("[NyPi harness] You ended this activation");
        Some(if opened.starts_with("[Idle") {
            FauxResponse::text("NOTHING")
        } else if nudged && tool_results(ctx) == 0 {
            FauxResponse::tool_call(
                "c1",
                "conclude_activation",
                json!({ "disposition": "noted_only", "reason": "the owner only shared the news" }),
            )
        } else if nudged {
            FauxResponse::text("Noted: nothing to do.")
        } else {
            FauxResponse::text("The owner shared news.")
        })
    });
    let chat_model = FauxProvider::new(vec![]);
    let runtime = runtime_as_set(home.path(), work.path());
    let (chat, _events) = chat::start(runtime, options(&chat_model, &principal_model)).unwrap();
    let principal = chat.principal().clone();
    let told = principal.wake(Trigger::UserSignal {
        topic: "The owner's sister had a baby".into(),
        urgency: "normal".into(),
        conversation: "Owner: my sister had a baby!".into(),
        gathered: String::new(),
        focus: None,
    });
    let idle = principal.wake(Trigger::Idle { why: "idle pass".into(), digest: "Nothing is due.".into() });
    until("both decisions", || DecisionJournal::new(&paths).load().len() == 2).await;

    let requests = principal_model.requests();
    assert_eq!(requests.len(), 4, "the owner's: the reply, the nudge, the conclusion; idle time: one");
    let system = &requests[0].system_prompt;
    assert!(requests.iter().all(|request| &request.system_prompt == system), "one system prompt for all");
    assert!(system.contains("<portrait>") && system.contains("Wants answers short"), "{system}");
    assert!(system.contains("behind the user's chief of staff") && !system.contains("{{"), "{system}");
    assert!(!system.contains("Today is") && !system.contains("## Tasks"), "{system}");
    let opening = text_of(&match &requests[0].messages[0] {
        Message::User(user) => user.content.clone(),
        other => panic!("{other:?}"),
    });
    assert!(opening.starts_with("<situational_radar>\nToday is ") && opening.contains("## Tasks"), "{opening}");

    let decisions = DecisionJournal::new(&paths).load();
    let decided = decisions.iter().find(|d| d.activation_id == told).unwrap();
    assert_eq!(decided.disposition, "noted_only: the owner only shared the news");
    assert!(read_activation(&paths, &told).iter().any(|event| matches!(event, ActivationEvent::Nudged { .. })));
    assert!(!read_activation(&paths, &idle).iter().any(|event| matches!(event, ActivationEvent::Nudged { .. })));
}

/// Material that came in is disposed of too (decision D89): an inbound activation that ends
/// having done nothing about the note is nudged once, like the user's request.
#[tokio::test]
async fn inbound_material_left_unhandled_is_nudged() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::write(
        home.path().join("settings.json"),
        json!({ "sandbox": "off", "principal": { "enforceDisposition": "always" } }).to_string(),
    )
    .unwrap();
    let principal_model = FauxProvider::answering(|ctx| {
        let nudged = last_said(ctx).starts_with("[NyPi harness] You ended this activation");
        Some(if nudged && tool_results(ctx) == 0 {
            FauxResponse::tool_call(
                "c1",
                "conclude_activation",
                json!({ "disposition": "noted_only", "reason": "a newsletter, nothing to file" }),
            )
        } else if nudged {
            FauxResponse::text("Noted.")
        } else {
            FauxResponse::text("Looked at the material.")
        })
    });
    let chat_model = FauxProvider::new(vec![]);
    let runtime = runtime_as_set(home.path(), work.path());
    let (chat, _events) = chat::start(runtime, options(&chat_model, &principal_model)).unwrap();
    let material = chat.principal().wake(Trigger::Inbound {
        source: "the inbound webhook".into(),
        note: "00-inbox/2611-newsletter.md".into(),
        text: "This week's newsletter.".into(),
    });
    until("the decision", || !DecisionJournal::new(&paths).load().is_empty()).await;
    assert!(read_activation(&paths, &material).iter().any(|event| matches!(event, ActivationEvent::Nudged { .. })));
    assert_eq!(DecisionJournal::new(&paths).load()[0].disposition, "noted_only: a newsletter, nothing to file");
}
