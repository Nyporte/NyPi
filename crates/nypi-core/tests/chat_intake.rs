//! The three planes with scripted models (decision D84): chat reads everything and changes
//! nothing, hands what needs doing to the Principal after its reply, and is the only voice; the
//! Principal starts and reviews the tasks. A task handed over as a topic carries the conversation
//! verbatim and is reviewed against the brief as the worker left it; a device read never waits on
//! an approval or past its time; a user message stops a stimulus being phrased, several in a row
//! are one turn, and one sent while chat answers joins that answer; and the task pool neither
//! waits on a slow MCP server nor carries one project's transcript into the next.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, FauxStep, faux_model};
use nypi_ai::{Context, Message, StopReason, text_of};
use nypi_core::chat::{self, ChatHandle, ChatOptions, ModuleEvent};
use nypi_core::config::Paths;
use nypi_core::place::Focus;
use nypi_core::pulse::PulseSource;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::task_pool::{TaskBrief, TaskPool};
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
        session: SessionMode::New,
        ..Default::default()
    })
    .unwrap();
    runtime.set_channel("test");
    runtime
}

fn settings(home: &std::path::Path, json: serde_json::Value) {
    std::fs::write(home.join("settings.json"), json.to_string()).unwrap();
}

async fn next(events: &mut mpsc::UnboundedReceiver<ModuleEvent>) -> ModuleEvent {
    tokio::time::timeout(Duration::from_secs(20), events.recv()).await.expect("event in time").expect("open")
}

async fn replied(events: &mut mpsc::UnboundedReceiver<ModuleEvent>) -> String {
    loop {
        if let ModuleEvent::Replied { text, .. } = next(events).await {
            return text;
        }
    }
}

fn seen(ctx: &Context) -> String {
    format!("{}\n{:?}", ctx.system_prompt, ctx.messages)
}

fn said(message: &Message) -> Option<String> {
    match message {
        Message::User(user) => Some(text_of(&user.content)),
        _ => None,
    }
}

fn last_said(ctx: &Context) -> String {
    ctx.messages.iter().rev().find_map(said).unwrap_or_default()
}

/// What opened the activation (the Principal's transcript is its own per activation).
fn first_said(ctx: &Context) -> String {
    let opening = ctx.messages.iter().find_map(said).unwrap_or_default();
    nypi_core::principal::activation_text(&opening).to_string()
}

fn tool_results(ctx: &Context) -> usize {
    ctx.messages.iter().filter(|m| matches!(m, Message::ToolResult(_))).count()
}

fn after_a_tool(ctx: &Context) -> bool {
    matches!(ctx.messages.last(), Some(Message::ToolResult(_)))
}

fn workspace_in(text: &str) -> Option<String> {
    let at = text.find("10-workspaces/")?;
    let path: String = text[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '/')).collect();
    Some(path.trim_end_matches('/').to_string())
}

fn last_result(ctx: &Context) -> (String, bool) {
    match ctx.messages.last() {
        Some(Message::ToolResult(result)) => (text_of(&result.content), result.is_error),
        other => panic!("not after a tool: {other:?}"),
    }
}

fn chat_with(
    runtime: Runtime,
    model: &FauxProvider,
    principal: &FauxProvider,
) -> (ChatHandle, mpsc::UnboundedReceiver<ModuleEvent>) {
    let options = ChatOptions {
        model: Some("faux/faux-1".into()),
        stream_fn: Some(model.stream_fn()),
        principal_stream_fn: Some(principal.stream_fn()),
    };
    chat::start(runtime, options).unwrap()
}

/// A Principal that starts a task for the owner's signal, and on a review has chat tell the
/// owner what the worker reported.
fn principal_that_delegates() -> FauxProvider {
    FauxProvider::answering(|ctx| {
        let opened = first_said(ctx);
        Some(if after_a_tool(ctx) {
            FauxResponse::text("Handled.")
        } else if opened.starts_with("[A task settled") {
            let report = opened
                .split("The worker's report:\n")
                .nth(1)
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            FauxResponse::tool_call(
                "p2",
                "stimulate_chat",
                json!({ "kind": "result", "headline": "The task is done", "must_convey": [report] }),
            )
        } else {
            let topic =
                opened.lines().find_map(|line| line.strip_prefix("Topic: ")).unwrap_or("the request").to_string();
            FauxResponse::tool_call("p1", "start_task", json!({ "topic": topic }))
        })
    })
}

#[tokio::test]
async fn a_topic_carries_the_conversation_verbatim_and_the_review_reads_the_brief_the_worker_set() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    settings(home.path(), json!({ "chat": { "review": "always" }, "sandbox": "off" }));
    // A trace nobody should have to retype: long, and every frame different.
    let frames: Vec<String> =
        (0..60).map(|n| format!("  at importer::rows::merge_{n} (src/rows.rs:{})", 100 + n)).collect();
    let trace = format!("thread 'import' panicked at 'duplicate key 7731'\n{}", frames.join("\n"));
    let worker_model = FauxProvider::answering(|ctx| {
        let workspace = workspace_in(&seen(ctx)).expect("the worker is told its workspace");
        Some(match tool_results(ctx) {
            // On its first step it says what will show the task is done.
            0 => FauxResponse::tool_call(
                "w1",
                "vault_write",
                json!({
                    "path": format!("{workspace}/brief.md"),
                    "mode": "append",
                    "content": "\n## Done when\nthe test that imports the same file twice passes\n",
                }),
            ),
            _ => FauxResponse::text("Rows are keyed by source id now [t1].\n\nDone: the importer no longer panics."),
        })
    });
    let chat_model = FauxProvider::answering(|ctx| {
        let said = last_said(ctx);
        Some(if said.starts_with("[The Principal asks you") {
            FauxResponse::text("The importer is fixed: importing the same file twice passes now.")
        } else if after_a_tool(ctx) {
            FauxResponse::text("On it.")
        } else if said == "fix it" {
            FauxResponse::tool_call("c1", "signal_principal", json!({ "topic": "Fix the importer panic shown above" }))
        } else {
            FauxResponse::text("That trace points at the importer's merge.")
        })
    });
    let principal_model = principal_that_delegates();
    let (chat, mut events) = chat_with(runtime(home.path(), work.path(), &worker_model), &chat_model, &principal_model);
    chat.say(trace.clone());
    assert_eq!(replied(&mut events).await, "That trace points at the importer's merge.");
    chat.say("fix it");
    assert_eq!(replied(&mut events).await, "On it.");
    let (mut accepted, mut told) = (None, None);
    while accepted.is_none() || told.is_none() {
        match next(&mut events).await {
            ModuleEvent::Reviewed { accepted: yes, said, .. } => accepted = Some((yes, said)),
            ModuleEvent::Replied { text, .. } => told = Some(text),
            _ => {}
        }
    }
    assert_eq!(accepted, Some((true, true)), "accepted, and chat was asked to tell the owner");
    assert_eq!(told.as_deref(), Some("The importer is fixed: importing the same file twice passes now."));

    // The Principal was handed the conversation after chat's reply, chat's reply included.
    let handed = first_said(&principal_model.requests()[0]);
    assert!(handed.contains("Topic: Fix the importer panic shown above") && handed.contains("You: On it."), "{handed}");
    // The worker got the user's own words, the whole trace, under the Principal's one-line topic.
    let first = &worker_model.requests()[0];
    let given = last_said(first);
    assert!(given.starts_with("Topic: Fix the importer panic shown above"), "{given}");
    assert!(given.contains("<conversation_context>") && given.contains(&trace), "{given}");
    assert!(given.contains("You: That trace points at the importer's merge.") && given.contains("User: fix it"));
    assert!(given.contains("append a `## Done when` section"), "the worker is asked to set it: {given}");
    // The brief in the vault holds the conversation, and what the worker set.
    let workspace = workspace_in(&seen(first)).unwrap();
    let brief = std::fs::read_to_string(home.path().join("vault").join(&workspace).join("brief.md")).unwrap();
    assert!(brief.contains("## Conversation") && brief.contains("panicked at 'duplicate key 7731'"), "{brief}");
    assert_eq!(
        nypi_core::task_pool::done_when_in(&brief).as_deref(),
        Some("the test that imports the same file twice passes")
    );
    // The Principal reviewed against that, with no extra call to work it out.
    let review =
        principal_model.requests().into_iter().find(|ctx| first_said(ctx).starts_with("[A task settled")).unwrap();
    let note = first_said(&review);
    assert!(note.contains("Done when: the test that imports the same file twice passes"), "{note}");
    // Chat phrased the result from what the Principal said must be conveyed.
    let phrased =
        chat_model.requests().into_iter().find(|ctx| last_said(ctx).starts_with("[The Principal asks you")).unwrap();
    assert!(last_said(&phrased).contains("Rows are keyed by source id now [t1]."), "{}", last_said(&phrased));
    assert_eq!(chat_model.requests().len(), 4, "the trace, the signal, its reply, the result");
    // The review's story says what it had chat tell the owner.
    let paths = Paths { home: home.path().to_path_buf() };
    let decisions = nypi_core::principal::DecisionJournal::new(&paths).load();
    let review = decisions.iter().find(|decision| decision.trigger_kind == "taskSettled").unwrap();
    assert!(review.stimulated_chat && !review.tasks_touched.is_empty());
    let story = nypi_core::principal::read_activation(&paths, &review.activation_id);
    assert!(story.iter().any(|event| matches!(
        event,
        nypi_core::principal::ActivationEvent::StimulatedChat { stimulus, .. } if stimulus.kind == "result"
    )));
}

#[tokio::test]
async fn chat_reads_everything_changes_nothing_and_hands_up_what_it_found() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    settings(home.path(), json!({ "sandbox": "off", "memory": { "embeddings": [] } }));
    // A secret in the store: its value must not reach any log of the planes.
    let store = nypi_vault::secrets::SecretStore::open(home.path()).unwrap();
    store.set(&nypi_vault::secrets::Handle::parse("cred://atlas/token").unwrap(), "sk-atlas-7731-value").unwrap();
    std::fs::write(work.path().join("contract.txt"), "Atlas contract: renews 2027-03-01, token sk-atlas-7731-value")
        .unwrap();
    let worker_model = FauxProvider::new(vec![]);
    let runtime = runtime(home.path(), work.path(), &worker_model);
    let long = format!("# Atlas\n\nRenews on 2027-03-01.\n\n{}", "Notes on the account. ".repeat(300));
    runtime.vault().unwrap().write("30-entities/atlas.md", &long, nypi_vault::vault::WriteMode::Replace).unwrap();
    let chat_model = FauxProvider::new(vec![
        FauxResponse::tool_call("r1", "vault_read", json!({ "path": "30-entities/atlas.md" })),
        FauxResponse::tool_call("r2", "memory", json!({ "action": "remember", "text": "Atlas renews in March" })),
        FauxResponse::tool_call("r3", "schedule", json!({ "action": "add", "text": "renew Atlas" })),
        FauxResponse::tool_call("r4", "asset_manage", json!({ "action": "expiring_within_days", "days": 120 })),
        FauxResponse::tool_call("r5", "read", json!({ "path": "contract.txt" })),
        FauxResponse::tool_call("r6", "signal_principal", json!({ "topic": "Set a reminder to renew Atlas" })),
        FauxResponse::text("Atlas renews on 1 March 2027; I'll have a reminder set."),
    ]);
    let principal_model = FauxProvider::new(vec![FauxResponse::text("Nothing to start yet: noted the renewal.")]);
    let (chat, mut events) = chat_with(runtime, &chat_model, &principal_model);
    chat.say("when does atlas renew?");
    assert_eq!(replied(&mut events).await, "Atlas renews on 1 March 2027; I'll have a reminder set.");

    let requests = chat_model.requests();
    let mut tools: Vec<String> = requests[0].tools.iter().map(|tool| tool.name.clone()).collect();
    tools.sort();
    let mut expected = vec![
        "asset_manage",
        "find",
        "goals",
        "grep",
        "ls",
        "memory",
        "read",
        "schedule",
        "signal_principal",
        "tasks_inspect",
        "vault_list",
        "vault_read",
        "vault_search",
    ];
    expected.sort();
    assert_eq!(tools, expected, "lookups and one way up, and nothing that acts");
    let memory = requests[0].tools.iter().find(|tool| tool.name == "memory").unwrap();
    assert_eq!(memory.parameters["properties"]["action"]["enum"], json!(["recall"]));

    // An excerpt of the note, saying so.
    let (read, failed) = last_result(&requests[1]);
    assert!(!failed && read.contains("Renews on 2027-03-01."));
    assert!(read.contains("[excerpt: 2000 of") && read.chars().count() < 2200, "{}", read.chars().count());
    // Changes are refused, as results the model reads, and nothing was changed.
    let (remember, failed) = last_result(&requests[2]);
    assert!(failed && remember.contains(r#"must be one of ["recall"]"#), "{remember}");
    let (schedule, failed) = last_result(&requests[3]);
    assert!(failed && schedule.contains(r#"must be one of ["list"]"#), "{schedule}");
    let (expiring, failed) = last_result(&requests[4]);
    assert!(!failed, "{expiring}");
    // A file in the folder, read on the device.
    let (contract, failed) = last_result(&requests[5]);
    assert!(!failed && contract.contains("renews 2027-03-01"), "{contract}");
    let paths = Paths { home: home.path().to_path_buf() };
    assert!(std::fs::read_to_string(paths.memory()).unwrap_or_default().is_empty());
    assert!(nypi_core::schedule::ScheduleStore::new(paths.schedules()).active().is_empty());

    // After the reply, the Principal got the signal with what chat had looked up.
    let started = Instant::now();
    while nypi_core::principal::DecisionJournal::new(&paths).load().is_empty() {
        assert!(started.elapsed() < Duration::from_secs(20), "the Principal never decided");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let handed = first_said(&principal_model.requests()[0]);
    assert!(handed.contains("What chat looked up in that turn") && handed.contains("- vault_read:"), "{handed}");
    assert!(handed.contains("You: Atlas renews on 1 March 2027"), "{handed}");

    // Every plane kept its record, and no secret is in any of them.
    let turns = chat::read_turns(&paths);
    let turn = turns.last().unwrap();
    assert_eq!(turn.origin, "userSay");
    let names: Vec<&str> = turn.tool_calls.iter().map(|step| step.name.as_str()).collect();
    assert_eq!(names, ["vault_read", "memory", "schedule", "asset_manage", "read", "signal_principal"]);
    let activation = &turn.signals_fired[0].activation_id;
    assert!(activation.starts_with("a-"));
    let decisions = nypi_core::principal::DecisionJournal::new(&paths).load();
    assert_eq!(&decisions[0].activation_id, activation);
    assert!(!nypi_core::principal::read_activation(&paths, activation).is_empty());
    for file in [paths.chat_turns(), paths.decisions(), paths.activations_index(), paths.activation_log(activation)] {
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("sk-atlas-7731-value"), "{} holds the secret", file.display());
    }
    assert!(std::fs::read_to_string(paths.chat_turns()).unwrap().contains("Atlas contract"));
}

/// A secret set while the planes run (decision D87): there was no store when chat began, and
/// what it logs afterwards is scrubbed of the new secret all the same.
#[tokio::test]
async fn a_secret_set_after_the_planes_began_is_scrubbed_from_their_logs() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    settings(home.path(), json!({ "sandbox": "off", "memory": { "embeddings": [] } }));
    std::fs::write(work.path().join("token.txt"), "the shop token is tok-shop-5521-live").unwrap();
    let worker_model = FauxProvider::new(vec![]);
    let runtime = runtime(home.path(), work.path(), &worker_model);
    let chat_model = FauxProvider::new(vec![
        FauxResponse::text("Hello."),
        FauxResponse::tool_call("r1", "read", json!({ "path": "token.txt" })),
        FauxResponse::text("The file names the shop's token."),
    ]);
    let principal_model = FauxProvider::new(vec![]);
    let (chat, mut events) = chat_with(runtime, &chat_model, &principal_model);
    chat.say("hello");
    assert_eq!(replied(&mut events).await, "Hello.");
    let paths = Paths { home: home.path().to_path_buf() };
    assert!(!paths.secrets().exists());

    let store = nypi_vault::secrets::SecretStore::open(home.path()).unwrap();
    store.set(&nypi_vault::secrets::Handle::parse("cred://shop/token").unwrap(), "tok-shop-5521-live").unwrap();
    chat.say("what is in token.txt?");
    assert_eq!(replied(&mut events).await, "The file names the shop's token.");
    let turns = std::fs::read_to_string(paths.chat_turns()).unwrap();
    assert!(turns.contains("the shop token is"), "the read was logged");
    assert!(!turns.contains("tok-shop-5521-live") && turns.contains("[REDACTED:cred://shop/token]"), "{turns}");
}

#[tokio::test]
async fn a_device_read_never_waits_on_an_approval_and_never_reads_a_secret() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // `grep` needs the owner's yes; nobody will answer.
    settings(home.path(), json!({ "sandbox": "off", "approval": { "tools": ["grep"] } }));
    std::fs::write(home.path().join("auth.json"), r#"{ "key": "sk-live-0000" }"#).unwrap();
    let chat_model = FauxProvider::new(vec![
        FauxResponse::tool_call("r1", "grep", json!({ "pattern": "Atlas", "path": "." })),
        FauxResponse::tool_call("r2", "read", json!({ "path": home.path().join("auth.json").display().to_string() })),
        FauxResponse::text("I can't look at those myself."),
    ]);
    let runtime = runtime(home.path(), work.path(), &FauxProvider::new(vec![]));
    let (chat, mut events) = chat_with(runtime, &chat_model, &FauxProvider::new(vec![]));
    let started = Instant::now();
    chat.say("grep for Atlas, and show me the auth file");
    assert_eq!(replied(&mut events).await, "I can't look at those myself.");
    assert!(started.elapsed() < Duration::from_secs(5), "waited {:?}", started.elapsed());
    let requests = chat_model.requests();
    let (grep, failed) = last_result(&requests[1]);
    assert!(failed && grep.contains("requires approval; signal the Principal"), "{grep}");
    let (auth, failed) = last_result(&requests[2]);
    assert!(failed && auth.contains("not permitted") && !auth.contains("sk-live-0000"), "{auth}");
}

#[tokio::test]
async fn the_user_stops_a_stimulus_being_phrased_and_messages_in_a_row_are_one_turn() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    settings(home.path(), json!({ "chat": { "review": "always" }, "sandbox": "off" }));
    let worker_model = FauxProvider::new(vec![FauxResponse::text("Done: the thing is done.")]);
    let (stimuli, held) = (Arc::new(AtomicUsize::new(0)), Arc::new(tokio::sync::Notify::new()));
    let chat_model = FauxProvider::answering({
        let (stimuli, held) = (stimuli.clone(), held.clone());
        move |ctx| {
            let said = last_said(ctx);
            Some(if said.starts_with("[The Principal asks you") {
                match stimuli.fetch_add(1, Ordering::SeqCst) {
                    // The first is still being phrased when the user speaks.
                    0 => FauxResponse {
                        steps: vec![FauxStep::Hold(held.clone()), FauxStep::Text("never seen".into())],
                        stop_reason: StopReason::Stop,
                        error: None,
                    },
                    _ => FauxResponse::text("The thing is done, and checked."),
                }
            } else if after_a_tool(ctx) {
                FauxResponse::text("Started.")
            } else if said == "do the thing" {
                FauxResponse::tool_call("c1", "signal_principal", json!({ "topic": "Do the thing" }))
            } else {
                FauxResponse::text(&format!("Heard: {}", said.replace('\n', " / ")))
            })
        }
    });
    let principal_model = principal_that_delegates();
    let (chat, mut events) = chat_with(runtime(home.path(), work.path(), &worker_model), &chat_model, &principal_model);
    chat.say("do the thing");
    assert_eq!(replied(&mut events).await, "Started.");
    // The result is being phrased, and held.
    let started = Instant::now();
    while stimuli.load(Ordering::SeqCst) == 0 {
        assert!(started.elapsed() < Duration::from_secs(20), "the stimulus never came");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Two messages in a row: the phrasing stops, and both are answered in one turn.
    chat.say("buy a flight");
    chat.say("next Wednesday to Tokyo");
    assert_eq!(replied(&mut events).await, "Heard: buy a flight /  / next Wednesday to Tokyo");
    // Then the stimulus is phrased again, as if it had not begun.
    assert_eq!(replied(&mut events).await, "The thing is done, and checked.");
    let requests = chat_model.requests();
    assert_eq!(requests.iter().filter(|ctx| last_said(ctx).contains("buy a flight")).count(), 1, "one turn");
    let again = requests.last().unwrap();
    let notes = again
        .messages
        .iter()
        .filter(|m| matches!(m, Message::User(u) if text_of(&u.content).starts_with("[The Principal asks you")))
        .count();
    assert_eq!(notes, 1, "the stopped phrasing left nothing in the transcript");
    assert!(!format!("{:?}", again.messages).contains("never seen"));
    // Both turns are in the log: the owner's, and the stimulus the Principal sent.
    let turns = chat::read_turns(&Paths { home: home.path().to_path_buf() });
    let stimulus = turns.iter().rev().find(|turn| turn.origin == "principalStimulus").unwrap();
    assert_eq!(stimulus.reply_text, "The thing is done, and checked.");
    assert_eq!(stimulus.stimulus.as_ref().unwrap().must_convey, ["Done: the thing is done."]);
}

#[tokio::test]
async fn a_message_sent_while_chat_answers_joins_the_answer_and_the_signal() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    settings(home.path(), json!({ "sandbox": "off" }));
    let worker_model = FauxProvider::new(vec![FauxResponse::text("Done: booked.")]);
    let handle: Arc<OnceLock<ChatHandle>> = Arc::default();
    let chat_model = FauxProvider::answering({
        let handle = handle.clone();
        move |ctx| {
            Some(if !after_a_tool(ctx) && last_said(ctx) == "buy a flight" {
                // The second half arrives while chat is still on the first.
                handle.get().unwrap().say("next Wednesday, to Tokyo");
                FauxResponse::tool_call("c1", "signal_principal", json!({ "topic": "Buy a flight" }))
            } else {
                FauxResponse::text("Booking the flight to Tokyo for next Wednesday.")
            })
        }
    });
    let principal_model = principal_that_delegates();
    let (chat, mut events) = chat_with(runtime(home.path(), work.path(), &worker_model), &chat_model, &principal_model);
    handle.set(chat.clone()).ok().unwrap();
    chat.say("buy a flight");
    assert_eq!(replied(&mut events).await, "Booking the flight to Tokyo for next Wednesday.");
    let requests = chat_model.requests();
    assert_eq!(requests.len(), 2, "one turn: the signal and the answer");
    assert_eq!(last_said(&requests[1]), "next Wednesday, to Tokyo", "joined after the tool's result");
    // The Principal, and through it the task, was handed the second half too.
    loop {
        if let ModuleEvent::Worker(WorkerEvent::Settled { .. }) = next(&mut events).await {
            break;
        }
    }
    let given = last_said(&worker_model.requests()[0]);
    assert!(given.contains("User: buy a flight") && given.contains("User: next Wednesday, to Tokyo"), "{given}");
}

fn brief(text: &str, folder: &std::path::Path) -> TaskBrief {
    TaskBrief { instruction: text.into(), place: Some(Focus::here(folder)), ..Default::default() }
}

async fn settled(events: &mut mpsc::UnboundedReceiver<WorkerEvent>) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), events.recv()).await.unwrap().unwrap();
        if matches!(event, WorkerEvent::Settled { .. }) {
            return;
        }
    }
}

#[tokio::test]
async fn the_conversations_worker_begins_afresh_for_another_project_and_not_for_the_same() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (alpha, beta) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    settings(home.path(), json!({ "sandbox": "off" }));
    let worker_model = FauxProvider::answering(|_| Some(FauxResponse::text("Done: done.")));
    let (worker, mut events) = nypi_core::worker::spawn(runtime(home.path(), work.path(), &worker_model));
    let pool = TaskPool::new(worker.clone(), 4, None);
    let source = || PulseSource::module("test");
    let run = async |pool: &TaskPool, events: &mut mpsc::UnboundedReceiver<WorkerEvent>, brief: TaskBrief| {
        pool.start(brief, source(), None).await.unwrap();
        settled(events).await;
    };
    run(&pool, &mut events, brief("alpha instruction", alpha.path())).await;
    let first_session = worker.session_id();
    run(&pool, &mut events, brief("beta instruction", beta.path())).await;
    let second_session = worker.session_id();
    run(&pool, &mut events, brief("beta follow-up", beta.path())).await;
    let requests = worker_model.requests();
    let text = |n: usize| format!("{:?}", requests[n].messages);
    assert!(!text(1).contains("alpha instruction"), "another project's transcript stays behind");
    assert!(text(2).contains("beta instruction"), "the same project keeps its context");
    assert_ne!(first_session, second_session);
    assert_eq!(worker.session_id(), second_session);
}

#[tokio::test]
async fn starting_a_task_beside_another_does_not_wait_for_its_mcp_servers() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // A server that never answers: connecting to it waits out the whole startup timeout.
    settings(
        home.path(),
        json!({ "sandbox": "off", "mcpServers": { "silent": { "command": "sleep", "args": ["60"] } } }),
    );
    let held = Arc::new(tokio::sync::Notify::new());
    let worker_model = FauxProvider::answering({
        let held = held.clone();
        move |_| {
            Some(FauxResponse {
                steps: vec![FauxStep::Hold(held.clone()), FauxStep::Text("Done: done.".into())],
                stop_reason: StopReason::Stop,
                error: None,
            })
        }
    });
    let (worker, _events) = nypi_core::worker::spawn(runtime(home.path(), work.path(), &worker_model));
    let pool = TaskPool::new(worker.clone(), 4, None);
    let first = TaskBrief { instruction: "the first".into(), ..Default::default() };
    assert!(!pool.start(first, PulseSource::module("test"), None).await.unwrap().beside);
    let started = Instant::now();
    let second = TaskBrief { instruction: "the second".into(), ..Default::default() };
    let begun = pool.start(second, PulseSource::module("test"), None).await.unwrap();
    assert!(begun.beside);
    assert!(started.elapsed() < Duration::from_secs(5), "waited {:?} for the MCP server", started.elapsed());
    pool.cancel_all();
}
