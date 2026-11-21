//! The board end to end: a real runtime (faux provider) links to a running board, the board
//! shows its approval, and the owner's answer and stop reach the run.

use std::time::Duration;

use futures::StreamExt;
use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_board_proto::{InboxItem, Now, Step, StoryStep, TaskDetail};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use serde_json::{Value, json};

use super::*;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct Client {
    http: reqwest::Client,
    address: String,
}

impl Client {
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http.get(format!("{}{path}", self.address)).header("cookie", format!("nypi_board={TOKEN}"))
    }

    async fn json<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        self.get(path).send().await.unwrap().json().await.unwrap()
    }

    async fn act(&self, csrf: &str, action: Value) -> Value {
        let request = self.http.post(format!("{}/api/action", self.address));
        let request = request.header("cookie", format!("nypi_board={TOKEN}")).header("x-board-csrf", csrf);
        request.json(&action).send().await.unwrap().json().await.unwrap()
    }

    /// The home screen once it shows what `ready` looks for.
    async fn now_until(&self, what: &str, ready: impl Fn(&Now) -> bool) -> Now {
        for _ in 0..100 {
            let now: Now = self.json("/api/now").await;
            if ready(&now) {
                return now;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the board never showed {what}");
    }
}

async fn start_board(home: &std::path::Path) -> Client {
    let paths = Paths { home: home.to_path_buf() };
    let links = crate::bridge::bind_owner_socket(&paths.board_socket(), "board").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(start(Board::new(paths, TOKEN.into()), links, listener));
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    Client { http, address }
}

fn runtime(home: &std::path::Path, work: &std::path::Path, faux: &FauxProvider) -> Runtime {
    Runtime::new(RuntimeOptions {
        cwd: work.to_path_buf(),
        paths: Some(Paths { home: home.to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        // Saved, so a task's evidence manifest can be drawn from its session.
        session: SessionMode::New,
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn the_owner_answers_an_approval_and_stops_a_run_from_the_board() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let client = start_board(home.path()).await;

    let locked = client.http.get(format!("{}/api/now", client.address)).send().await.unwrap();
    assert_eq!(locked.status(), 401);
    let linked = client.http.get(format!("{}/task/x?token={TOKEN}", client.address)).send().await.unwrap();
    assert_eq!(linked.status(), 303);
    assert_eq!(linked.headers()["location"], "/task/x", "the token leaves the address bar");

    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("t1", "bash", json!({ "command": "git push origin main" })),
        FauxResponse::text("Blocked: the push was not approved."),
        FauxResponse::tool_call("t2", "bash", json!({ "command": "sleep 30" })),
        FauxResponse::text("Failed: stopped."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    client.now_until("the process", |now| now.processes.len() == 1).await;

    let live = client.get("/api/live?watch=*").send().await.unwrap();
    assert_eq!(live.headers()["content-type"], "text/event-stream");
    let mut live = live.bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(5), live.next()).await.unwrap().unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).starts_with("event: now\ndata: {\"event\":\"now\""));

    let run = tokio::spawn(async move {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = runtime.prompt("push it", &tx).await.unwrap();
        (runtime, outcome)
    });
    let now = client.now_until("the approval", |now| !now.inbox.is_empty()).await;
    let InboxItem::Approval { id, summary, task, .. } = &now.inbox[0] else { panic!("{:?}", now.inbox) };
    assert_eq!(summary, "git push origin main");
    assert!(task.is_some(), "the approval names its task");
    assert!(matches!(now.processes[0].step, Step::Approval { .. }));

    let forged = client.act("wrong", json!({ "action": "approve", "id": id })).await;
    assert_eq!(forged["ok"], false, "an action without the page's CSRF value is refused");
    let hello: Value = client.json("/api/hello").await;
    let csrf = hello["csrf"].as_str().unwrap().to_string();
    let denied = client.act(&csrf, json!({ "action": "deny", "id": id, "reason": "keep it local" })).await;
    assert_eq!(denied["ok"], true, "{denied}");

    let (mut runtime, outcome) = tokio::time::timeout(Duration::from_secs(10), run).await.unwrap().unwrap();
    let result = outcome.new_messages.iter().find_map(|m| match m {
        Message::ToolResult(r) => Some(r.clone()),
        _ => None,
    });
    let result = result.unwrap();
    assert!(result.is_error && text_of(&result.content).contains("keep it local"));
    let again = client.act(&csrf, json!({ "action": "approve", "id": id })).await;
    assert_eq!(again["ok"], false, "an answered approval is gone");

    // The task's story, from the audit log.
    let tasks = Ledger::new(Paths { home: home.path().to_path_buf() }.ledger()).load();
    let task_id = tasks.keys().next().unwrap().0.clone();
    let detail: TaskDetail = client.json(&format!("/api/task/{task_id}")).await;
    let titles: Vec<String> = detail
        .steps
        .iter()
        .filter_map(|s| match s {
            StoryStep::Note { title, .. } => Some(title.clone()),
            StoryStep::Tool { name, .. } => Some(format!("tool {name}")),
            StoryStep::Ended { status, .. } => Some(format!("ended {status}")),
            _ => None,
        })
        .collect();
    assert!(
        titles.starts_with(&["tool bash".into(), "Asked you to approve bash".into(), "Denied".into()]),
        "{titles:?}"
    );
    assert_eq!(titles.last().map(String::as_str), Some("ended blocked"), "{titles:?}");
    let activity: Vec<Value> = client.json("/api/activity?kind=task").await;
    assert_eq!(activity.len(), 1);

    // Stop a run that would otherwise sleep for half a minute.
    let run = tokio::spawn(async move {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        runtime.prompt("wait a while", &tx).await.unwrap();
        runtime
    });
    let now = client.now_until("the sleeping tool", |now| matches!(now.processes[0].step, Step::Tool { .. })).await;
    let process = now.processes[0].id.clone();
    let stopped = client.act(&csrf, json!({ "action": "stop", "process": process })).await;
    assert_eq!(stopped["ok"], true, "{stopped}");
    let runtime = tokio::time::timeout(Duration::from_secs(10), run).await.expect("the stop reached the run").unwrap();
    client.now_until("the idle process", |now| now.processes[0].step == Step::Idle).await;
    let idle = client.act(&csrf, json!({ "action": "message", "process": process, "text": "hi" })).await;
    assert_eq!(idle["ok"], false, "nothing to steer");

    // A runtime that is gone leaves the board: its thread on the pulse bus ends with it, and
    // with that the link the bus was holding.
    drop(runtime);
    client.now_until("the process to leave", |now| now.processes.is_empty()).await;
}

#[tokio::test]
async fn the_system_map_is_served_and_follows_what_is_linked() {
    use nypi_board_proto::{NodeState, Topology};

    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let client = start_board(home.path()).await;
    let locked = client.http.get(format!("{}/api/topology", client.address)).send().await.unwrap();
    assert_eq!(locked.status(), 401, "the map says what runs here: it needs the token like everything else");

    let state = |map: &Topology, id: &str| {
        let node = map.nodes.iter().find(|node| node.id == id).unwrap();
        (node.state, node.status.clone())
    };
    let quiet: Topology = client.json("/api/topology").await;
    assert_eq!((quiet.groups.len(), quiet.nodes.len(), quiet.edges.len()), (5, 40, 66));
    assert_eq!(state(&quiet, "board"), (NodeState::Ready, "0 processes linked".into()));
    assert_eq!(state(&quiet, "terminal"), (NodeState::Absent, "not open".into()));
    assert_eq!(state(&quiet, "hub").0, NodeState::Absent);
    assert_eq!(state(&quiet, "ledger"), (NodeState::Ready, "0 tasks, 0 open".into()));

    // A runtime links; then it says it is the daemon, and the map follows the new name.
    let mut runtime = runtime(home.path(), work.path(), &FauxProvider::new(vec![]));
    client.now_until("the process", |now| now.processes.len() == 1).await;
    let linked: Topology = client.json("/api/topology").await;
    assert_eq!(state(&linked, "board"), (NodeState::Ready, "1 process linked".into()));
    assert_eq!(state(&linked, "terminal"), (NodeState::Ready, "1 open, idle".into()));
    assert_eq!(state(&linked, "watchdog").0, NodeState::Ready, "a long-lived process is checking");
    runtime.set_channel("hub");
    client.now_until("the renamed front end", |now| now.processes[0].front_end == "hub").await;
    let renamed: Topology = client.json("/api/topology").await;
    assert_eq!(state(&renamed, "terminal"), (NodeState::Absent, "not open".into()));
}

/// The three planes on the board (decision D86): a conversation with chat, the Principal, and
/// a worker on scripted models leaves its logs, and every new endpoint shows them step by step,
/// linked to each other, with no secret in any answer.
#[tokio::test]
async fn the_planes_are_shown_step_by_step_and_linked() {
    use nypi_board_proto::{
        ActivationDetail, ActivationStep, ChatView, GoalGroup, PrincipalView, Topology, VaultNoteText, VaultView,
    };
    use nypi_core::chat::{self, ChatOptions, ModuleEvent};

    const SECRET: &str = "sk-board-test-0000-secret";
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::write(
        home.path().join("settings.json"),
        json!({ "chat": { "model": "faux/faux-1", "review": "always" }, "sandbox": "off" }).to_string(),
    )
    .unwrap();
    let store = nypi_vault::secrets::SecretStore::open(home.path()).unwrap();
    store.set(&nypi_vault::secrets::Handle::parse("cred://shop/key").unwrap(), SECRET).unwrap();
    // A goal with its campaign in the vault.
    let goals = nypi_core::goals::GoalsFile::new(paths.goals());
    let goal = goals.add(nypi_core::goals::Term::Short, "Open the shop", &Default::default(), "test").unwrap();
    let change = nypi_core::goals::Change { vault: Some("10-projects/shop".into()), ..Default::default() };
    goals.update(&goal, &change, "test").unwrap();
    std::fs::create_dir_all(paths.vault().join("10-projects/shop")).unwrap();
    let plan = format!("# Shop\n\n## Workstreams & Milestones\n- [~] price list\n\nKey: {SECRET}\n");
    std::fs::write(paths.vault().join("10-projects/shop/plan.md"), plan).unwrap();

    let first_said = |ctx: &nypi_ai::Context| {
        let opening = ctx
            .messages
            .iter()
            .find_map(|m| match m {
                Message::User(user) => Some(text_of(&user.content)),
                _ => None,
            })
            .unwrap_or_default();
        nypi_core::principal::activation_text(&opening).to_string()
    };
    let last_said = |ctx: &nypi_ai::Context| {
        ctx.messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User(user) => Some(text_of(&user.content)),
                _ => None,
            })
            .unwrap_or_default()
    };
    let mid_turn = |ctx: &nypi_ai::Context| matches!(ctx.messages.last(), Some(Message::ToolResult(_)));
    let chat_model = FauxProvider::answering(move |ctx| {
        Some(if last_said(ctx).starts_with("[The Principal asks you") {
            FauxResponse::text("The price list is done.")
        } else if mid_turn(ctx) {
            FauxResponse::text("On it.")
        } else {
            FauxResponse::tool_call("c1", "signal_principal", json!({ "topic": "Write the price list" }))
        })
    });
    let goal_id = goal.clone();
    let principal_model = FauxProvider::answering(move |ctx| {
        let opened = first_said(ctx);
        Some(if mid_turn(ctx) {
            FauxResponse::text("Decided.")
        } else if opened.starts_with("[A task settled") {
            FauxResponse::tool_call(
                "p2",
                "stimulate_chat",
                json!({ "kind": "result", "headline": "Price list", "must_convey": ["The price list is done."] }),
            )
        } else {
            FauxResponse::tool_call(
                "p1",
                "start_task",
                json!({ "instruction": "Write the price list.", "done_when": "the list is in the vault", "goal": goal_id }),
            )
        })
    });
    let worker_model = FauxProvider::answering(|_| {
        Some(FauxResponse::text(&format!("Wrote it; the key is {SECRET}.\n\nDone: the price list is written.")))
    });
    let runtime = runtime(home.path(), work.path(), &worker_model);
    let options = ChatOptions {
        model: Some("faux/faux-1".into()),
        stream_fn: Some(chat_model.stream_fn()),
        principal_stream_fn: Some(principal_model.stream_fn()),
    };
    let (chat, mut events) = chat::start(runtime, options).unwrap();
    chat.say("Please write the price list for the shop.");
    let mut replies = 0;
    let mut reviewed = false;
    while replies < 2 || !reviewed {
        match tokio::time::timeout(Duration::from_secs(20), events.recv()).await.unwrap().unwrap() {
            ModuleEvent::Replied { .. } => replies += 1,
            ModuleEvent::Reviewed { .. } => reviewed = true,
            _ => {}
        }
    }
    // The decisions are written as the activations finish, just after these events.
    for _ in 0..100 {
        if nypi_core::principal::DecisionJournal::new(&paths).load().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let client = start_board(home.path()).await;
    let mut answers: Vec<String> = Vec::new();
    let mut fetch = async |path: &str| -> String {
        let text = client.get(path).send().await.unwrap().text().await.unwrap();
        answers.push(text.clone());
        text
    };

    // Chat: the owner's turn with its signal, linked to the activation; the stimulus turn.
    let view: ChatView = serde_json::from_str(&fetch("/api/chat").await).unwrap();
    assert_eq!(view.model.as_deref(), Some("faux/faux-1"));
    let owners = view.turns.iter().find(|turn| turn.origin == "userSay").unwrap();
    assert_eq!(owners.tool_calls[0].name, "signal_principal");
    let signalled = owners.signals[0].activation.clone();
    let told = view.turns.iter().find(|turn| turn.origin == "principalStimulus").unwrap();
    assert_eq!(told.stimulus.as_ref().unwrap().must_convey, ["The price list is done."]);
    assert_eq!(told.reply_text, "The price list is done.");

    // The Principal: two activations, the owner's and the review, both finished.
    let principal: PrincipalView = serde_json::from_str(&fetch("/api/principal").await).unwrap();
    assert!(principal.card.on && principal.card.active.is_none());
    assert_eq!(principal.card.decisions_today, 2);
    let kinds: Vec<(&str, &str)> =
        principal.activations.iter().map(|row| (row.kind.as_str(), row.state.as_str())).collect();
    assert_eq!(kinds, [("taskSettled", "finished"), ("userSignal", "finished")]);
    let owner_row = &principal.activations[1];
    assert_eq!((owner_row.id.as_str(), owner_row.goals.clone()), (signalled.as_str(), vec![goal.clone()]));
    // How each was disposed of (decision D87).
    assert_eq!(owner_row.disposition, "tasks_touched");
    assert!(principal.activations[0].disposition.contains("stimulated_chat"), "{:?}", principal.activations[0]);
    let task_id = owner_row.tasks[0].clone();

    // The owner's activation, step by step: its model turn, the start_task linking the task.
    let detail: ActivationDetail = serde_json::from_str(&fetch(&format!("/api/activation/{signalled}")).await).unwrap();
    assert!(detail.trigger.contains("Write the price list") && detail.started.is_some());
    assert!(detail.steps.iter().any(|step| matches!(
        step,
        ActivationStep::Tool { name, task: Some(task), .. } if name == "start_task" && *task == task_id
    )));
    assert!(matches!(detail.steps.last(), Some(ActivationStep::Finished { .. })));
    // What it was handed before its first call: the campaign the owner's words name (D88).
    let briefing = detail.briefing.as_ref().expect("a briefing pack");
    let plan = briefing.items.iter().find(|item| item.kind == "plan").expect("the shop's plan");
    assert_eq!(plan.path.as_deref(), Some("10-projects/shop/plan.md"));
    assert!(plan.text.contains("- [~] price list"));
    let review: ActivationDetail =
        serde_json::from_str(&fetch(&format!("/api/activation/{}", principal.activations[0].id)).await).unwrap();
    assert!(review.steps.iter().any(|step| matches!(step, ActivationStep::Stimulus { .. })));
    let missing = client.get("/api/activation/a-none").send().await.unwrap();
    assert_eq!(missing.status(), 404);

    // The task: up to its goal and activation, its plan and workspace, the review's verdict.
    let task: TaskDetail = serde_json::from_str(&fetch(&format!("/api/task/{task_id}")).await).unwrap();
    assert_eq!((task.goal_id.as_deref(), task.activation.as_deref()), (Some(goal.as_str()), Some(signalled.as_str())));
    assert!(task.plan.as_ref().unwrap().text.contains("- [~] price list"));
    assert!(task.brief.as_ref().unwrap().contains("Write the price list."));
    assert!(task.steps.iter().any(|step| matches!(step, StoryStep::Review { accepted: true, .. })), "{:?}", task.steps);
    assert_eq!(task.rounds, 0);
    // And what the Principal reviewed it by: no report.md, the manifest of its session (D87).
    assert!(task.report.is_none());
    let manifest = task.manifest.as_deref().expect("the manifest");
    assert!(
        manifest.starts_with("## Harness Evidence Manifest") && manifest.contains("cites no tool result"),
        "{manifest}"
    );
    assert!(task.verified_refs.is_empty() && !task.unchecked_change);

    // Now: the planes, and the task under its goal.
    let now: Now = serde_json::from_str(&fetch("/api/now").await).unwrap();
    assert!(now.planes.chat.on && now.planes.chat.signals_today == 1, "{:?}", now.planes.chat);
    let group: &GoalGroup = now.campaigns.iter().find(|group| group.goal.is_some()).unwrap();
    let card = group.goal.as_ref().unwrap();
    assert_eq!((card.id.as_str(), card.plan.as_deref()), (goal.as_str(), Some("10-projects/shop/plan.md")));
    assert_eq!(group.tasks[0].id, task_id);

    // The vault: the goal with its plan, the folders, and a note read through the vault.
    let vault: VaultView = serde_json::from_str(&fetch("/api/vault").await).unwrap();
    assert_eq!(vault.goals[0].tasks, 1);
    let projects = vault.folders.iter().find(|folder| folder.name == "10-projects").unwrap();
    assert!(projects.notes.iter().any(|note| note.path == "10-projects/shop/plan.md"));
    let note: VaultNoteText = serde_json::from_str(&fetch("/api/vault?path=10-projects/shop/plan.md").await).unwrap();
    assert!(note.text.contains("price list"));
    let outside = client.get("/api/vault?path=../settings.json").send().await.unwrap();
    assert_eq!(outside.status(), 404, "only notes of the vault");

    // The system map has the planes on it.
    let map: Topology = serde_json::from_str(&fetch("/api/system").await).unwrap();
    for id in ["chat", "principal", "worker", "thinking", "journal", "webhook", "devices", "inspector"] {
        assert!(map.nodes.iter().any(|node| node.id == id), "{id} is on the map");
    }

    // The live stream sends the conversation and the activation.
    let live = client.get(&format!("/api/live?chat=1&activation={signalled}")).send().await.unwrap();
    let mut live = live.bytes_stream();
    let mut seen = String::new();
    while !(seen.contains("event: chat") && seen.contains("event: activation")) {
        let chunk = tokio::time::timeout(Duration::from_secs(5), live.next()).await.unwrap().unwrap().unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    answers.push(seen);

    for answer in &answers {
        assert!(!answer.contains(SECRET), "a secret reached the board: {answer}");
    }

    // Saying something from the board goes to the daemon's conversation; without one, it says so.
    let hello: Value = client.json("/api/hello").await;
    let csrf = hello["csrf"].as_str().unwrap().to_string();
    let said = client.act(&csrf, json!({ "action": "say", "text": "hello" })).await;
    assert_eq!(said["ok"], false);
    assert!(said["message"].as_str().unwrap().contains("hub is not running"), "{said}");
}

/// A chat turn already in flight when the Chat screen opens is shown at once (decision D89):
/// `/api/chat` carries what chat is doing, as the process's link last said.
#[tokio::test]
async fn the_chat_screen_opens_on_a_turn_in_flight() {
    use tokio::io::AsyncWriteExt;
    let home = tempfile::tempdir().unwrap();
    let paths = Paths { home: home.path().to_path_buf() };
    let client = start_board(home.path()).await;
    let view: nypi_board_proto::ChatView = client.json("/api/chat").await;
    assert!(view.live.is_none(), "nothing in flight");

    let mut process = tokio::net::UnixStream::connect(paths.board_socket()).await.unwrap();
    let plane =
        json!({ "ch": "1", "op": "plane", "plane": "chat", "id": "p-1", "step": "Writing", "tail": "Atlas renews" });
    process.write_all(format!("{plane}\n").as_bytes()).await.unwrap();
    let mut live = None;
    for _ in 0..100 {
        let view: nypi_board_proto::ChatView = client.json("/api/chat").await;
        if view.live.is_some() {
            live = view.live;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let live = live.expect("the turn in flight");
    assert_eq!((live.step.as_str(), live.tail.as_str()), ("Writing", "Atlas renews"));
}

/// A join request is an inbox card on the board (decision D93), answered with its buttons.
#[tokio::test]
async fn a_join_request_is_answered_on_the_board() {
    use nypi_hub::pairing::{self, JoinAnswer, NewRequest, PeerKind, Trust};
    let home = tempfile::tempdir().unwrap();
    let paths = Paths { home: home.path().to_path_buf() };
    let request = pairing::open_request(
        &paths,
        NewRequest {
            kind: PeerKind::Machine,
            name: "macbook".into(),
            system: "macos aarch64".into(),
            from: "192.168.1.30:51000".into(),
            code: "482913".into(),
            key: "ab".into(),
            instance_id: "i".into(),
        },
    )
    .unwrap();
    let client = start_board(home.path()).await;
    let now = client.now_until("the request", |now| !now.inbox.is_empty()).await;
    let InboxItem::Join { name, code, asker, .. } = &now.inbox[0] else { panic!("{:?}", now.inbox) };
    assert_eq!((name.as_str(), code.as_str(), asker.as_str()), ("macbook", "482913", "machine"));
    let hello: Value = client.json("/api/hello").await;
    let csrf = hello["csrf"].as_str().unwrap().to_string();
    let admitted = client.act(&csrf, json!({ "action": "admit", "code": "482913", "execOnly": true })).await;
    assert_eq!(admitted["ok"], true, "{admitted}");
    assert_eq!(
        pairing::request(&paths, &request.id).unwrap().answer,
        Some(JoinAnswer::Admit { trust: Trust::ExecOnly })
    );
    let again = client.act(&csrf, json!({ "action": "refuse", "code": "482913" })).await;
    assert_eq!(again["ok"], false, "the first answer counts: {again}");
}

/// A browser through the entry (decision D94): the relay passes it to the hub, the hub to the
/// board's public socket. Without a session it gets only the page that asks to be admitted (the
/// local token is not taken there); the user is asked where join requests go; admitted, its
/// cookie opens the board and its live stream; what it sees is scrubbed; and removing it ends
/// the session at once.
#[tokio::test]
async fn a_browser_through_the_entry_is_admitted_like_a_machine() {
    use nypi_core::config::RelaySettings;
    use nypi_hub::pairing::{self, JoinAnswer, PeerKind, Peers, Trust};
    const SECRET: &str = "sk-entry-0123456789abcdef";
    let (home, work, relay_home) =
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::write(home.path().join("settings.json"), r#"{ "sandbox": "off" }"#).unwrap();
    let store = nypi_vault::secrets::SecretStore::open(home.path()).unwrap();
    store.set(&nypi_vault::secrets::Handle::parse("cred://shop/key").unwrap(), SECRET).unwrap();

    // The board, on its local port and on the public socket.
    let board = Board::new(paths.clone(), TOKEN.into());
    let public = crate::bridge::bind_owner_socket(&paths.board_public_socket(), "board for browsers").unwrap();
    tokio::spawn(web::serve_public(public, board.clone()));
    let links = crate::bridge::bind_owner_socket(&paths.board_socket(), "board").unwrap();
    tokio::spawn(start(board, links, TcpListener::bind("127.0.0.1:0").await.unwrap()));

    // The relay, and the hub registered at it.
    let relay = nypi_hub::relay::Relay::new(&Paths { home: relay_home.path().to_path_buf() }, RelaySettings::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let entry = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(nypi_hub::relay::serve(listener, relay.clone()));
    nypi_hub::instance::Instance::new_hub(&paths, "mini").unwrap().save(&paths).unwrap();
    let faux = FauxProvider::new(vec![]);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let network = nypi_hub::daemon::Network {
        listeners: Vec::new(),
        directions: nypi_hub::direct::Directions::fixed(Vec::new(), Vec::new()),
        lan: None,
        entry: Some(entry.clone()),
        follow_entry: false,
        public_address: None,
        port_mapping: false,
        board: None,
    };
    tokio::spawn(nypi_hub::daemon::serve_hub(
        runtime(home.path(), work.path(), &faux),
        UnixListener::bind(home.path().join("d.sock")).unwrap(),
        shutdown.clone(),
        None,
        Some(nypi_hub::nodes::Nodes::open(&paths)),
        network,
    ));
    for _ in 0..500 {
        if relay.has_hub() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(relay.has_hub());

    let browser = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let phone = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X)";
    let asked = browser
        .get(format!("{entry}/"))
        .header("user-agent", phone)
        .header("cf-connecting-ip", "203.0.113.20")
        .send()
        .await
        .unwrap();
    assert_eq!(asked.status(), 401);
    let set = asked.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
    assert!(set.contains("HttpOnly") && set.contains("Secure") && set.contains("SameSite=Strict"), "{set}");
    let cookie = set.split(';').next().unwrap().to_string();
    let page = asked.text().await.unwrap();
    let request = pairing::pending(&paths).pop().expect("a request");
    assert_eq!((request.kind, request.from.as_str()), (PeerKind::Browser, "203.0.113.20"));
    assert!(page.contains(&nypi_node::identity::spaced(&request.code)), "the page shows the code: {page}");

    // Nothing of the board without a session, and the local token is not taken here.
    for with in [cookie.clone(), format!("nypi_board={TOKEN}")] {
        let locked = browser.get(format!("{entry}/api/now")).header("cookie", with).send().await.unwrap();
        assert_eq!(locked.status(), 401);
        assert!(locked.text().await.unwrap().contains("not admitted"));
    }

    // The user is told where join requests go; a browser is never admitted as exec-only.
    for _ in 0..500 {
        if std::fs::read_to_string(paths.outbox()).unwrap_or_default().contains(&request.code) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(std::fs::read_to_string(paths.outbox()).unwrap_or_default().contains(&request.code));
    let exec_only = pairing::answer(&paths, &request.code, JoinAnswer::Admit { trust: Trust::ExecOnly });
    assert!(exec_only.unwrap_err().contains("browser"));
    pairing::answer(&paths, &request.code, JoinAnswer::Admit { trust: Trust::Personal }).unwrap();

    // Admitted: the page goes on, and the cookie opens the board.
    let get = |path: &str| browser.get(format!("{entry}{path}")).header("cookie", &cookie).header("user-agent", phone);
    let admitted = get("/chat?activation=a-1").send().await.unwrap();
    assert_eq!(admitted.status(), 303);
    assert_eq!(admitted.headers().get("location").unwrap(), "/chat?activation=a-1", "the query is kept (D99)");
    let peers = Peers::open(&paths).list();
    let admitted = peers.values().find(|peer| peer.kind == PeerKind::Browser).expect("in the register");
    assert_eq!((admitted.name.as_str(), admitted.trust), ("iphone-browser", Trust::Personal));
    assert!(!peers.keys().any(|key| cookie.contains(key.as_str())), "the register keeps a hash, not the cookie");
    assert_eq!(get("/api/now").send().await.unwrap().status(), 200);

    // What it sees is scrubbed: another browser whose user agent carries a secret asks, and the
    // admitted one sees the request without the secret.
    let other = browser.get(format!("{entry}/")).header("user-agent", format!("curl {SECRET}")).send().await.unwrap();
    assert_eq!(other.status(), 401);
    let mut shown = String::new();
    for _ in 0..100 {
        shown = get("/api/now").send().await.unwrap().text().await.unwrap();
        if shown.contains("curl") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(shown.contains("curl") && !shown.contains(SECRET), "{shown}");

    // The live stream works through the entry, and ends when the browser is removed.
    let mut live = get("/api/live").send().await.unwrap().bytes_stream();
    let first = tokio::time::timeout(Duration::from_secs(10), live.next()).await.unwrap().unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("data:"), "{first:?}");
    assert_eq!(Peers::open(&paths).remove("iphone-browser").unwrap(), 1);
    let ended = tokio::time::timeout(Duration::from_secs(10), async { while let Some(Ok(_)) = live.next().await {} });
    assert!(ended.await.is_ok(), "the stream ended");
    assert_eq!(get("/api/now").send().await.unwrap().status(), 401, "and the session with it");
    shutdown.cancel();
}

/// Decision D99: the page's files carry an `ETag` and are revalidated (`304 Not Modified` while the
/// browser has them, `private, no-cache`), while the API keeps `no-store`.
#[tokio::test]
async fn the_pages_files_are_revalidated_and_the_api_is_never_stored() {
    let fresh = web::file_response("board.js", b"console.log(1)", false, None);
    assert_eq!(fresh.status(), 200);
    let tag = fresh.headers().get("etag").unwrap().to_str().unwrap().to_string();
    assert_eq!(fresh.headers().get("cache-control").unwrap(), "private, no-cache");
    let again = web::file_response("board.js", b"console.log(1)", false, Some(&tag));
    assert_eq!(again.status(), 304);
    assert_eq!(again.headers().get("etag").unwrap().to_str().unwrap(), tag);
    let body = http_body_util::BodyExt::collect(again.into_body()).await.unwrap().to_bytes();
    assert!(body.is_empty(), "no body with a 304");
    let weak = format!("W/{tag}, \"other\"");
    assert_eq!(web::file_response("board.js", b"console.log(1)", false, Some(&weak)).status(), 304);
    let changed = web::file_response("board.js", b"console.log(2)", false, Some(&tag));
    assert_eq!(changed.status(), 200, "another file, another tag");

    let home = tempfile::tempdir().unwrap();
    let client = start_board(home.path()).await;
    let api = client.get("/api/now").send().await.unwrap();
    assert_eq!(api.headers().get("cache-control").unwrap(), "no-store");
    if web::ui_built() {
        let page = client.get("/").send().await.unwrap();
        let tag = page.headers().get("etag").expect("the page is tagged").clone();
        assert_eq!(page.headers().get("cache-control").unwrap(), "private, no-cache");
        let again = client.get("/").header("if-none-match", tag).send().await.unwrap();
        assert_eq!(again.status(), 304);
    }
}
