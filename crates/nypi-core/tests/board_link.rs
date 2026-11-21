//! The board link is one connection per process (decision D80): every runtime in it reports on
//! a channel of its own, and what the owner does on the board reaches the runtime it is for.

use std::time::Duration;

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_core::board_link::{Framed, LinkDown, LinkUp};
use nypi_core::config::Paths;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
async fn every_runtime_of_a_process_reports_on_one_connection() {
    let (home, first_folder, second_folder) =
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::create_dir_all(paths.board_socket().parent().unwrap()).unwrap();
    let board = tokio::net::UnixListener::bind(paths.board_socket()).unwrap();
    let faux =
        FauxProvider::new(vec![FauxResponse::tool_call("t1", "bash", json!({ "command": "git push origin main" }))]);

    let mut first = runtime(home.path(), first_folder.path(), &faux);
    let (stream, _) = tokio::time::timeout(Duration::from_secs(5), board.accept()).await.unwrap().unwrap();
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut next = async || -> Framed<LinkUp> {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await.unwrap().unwrap().unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let hello = next().await;
    let LinkUp::Hello { cwd, .. } = &hello.message else { panic!("a hello first: {hello:?}") };
    assert_eq!(cwd, &first_folder.path().display().to_string());

    // A second runtime of the same process is a second channel, not a second connection.
    let second = runtime(home.path(), second_folder.path(), &faux);
    let other = next().await;
    let LinkUp::Hello { cwd, .. } = &other.message else { panic!("the second runtime's hello: {other:?}") };
    assert_eq!(cwd, &second_folder.path().display().to_string());
    assert_ne!(other.ch, hello.ch);
    assert!(
        tokio::time::timeout(Duration::from_millis(300), board.accept()).await.is_err(),
        "no second connection was made"
    );

    // What the board says on a channel reaches that runtime: the first one's approval is
    // answered there, and the run goes on.
    let _approvals = first.enable_approvals();
    let (sink, _events) = mpsc::unbounded_channel();
    faux.push(FauxResponse::text("Blocked: the push was not approved."));
    let running = tokio::spawn(async move { first.prompt("push it", &sink).await.unwrap() });
    let asked = loop {
        let framed = next().await;
        if let LinkUp::ApprovalWaiting { id, summary, .. } = &framed.message {
            assert_eq!((framed.ch.as_str(), summary.as_str()), (hello.ch.as_str(), "git push origin main"));
            break id.clone();
        }
    };
    let answer =
        Framed { ch: hello.ch.clone(), message: LinkDown::Answer { id: asked, approved: false, reason: None } };
    write.write_all(format!("{}\n", serde_json::to_string(&answer).unwrap()).as_bytes()).await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(20), running).await.unwrap().unwrap();
    assert_eq!(outcome.status.kind, nypi_core::status::StatusKind::Blocked);

    // A runtime that ends says so on its channel, and the connection stays for the others: the
    // first went with its run above, the second goes now.
    drop(second);
    let mut gone = Vec::new();
    while gone.len() < 2 {
        let framed = next().await;
        if matches!(framed.message, LinkUp::Bye) {
            gone.push(framed.ch);
        }
    }
    assert_eq!(gone, [hello.ch.clone(), other.ch.clone()]);
}

/// Chat says what it is doing while its turn is in flight (decision D88): its steps and the
/// text it writes go up the process's link as plane progress, and `idle` when the turn ends.
#[tokio::test]
async fn a_chat_turn_in_flight_is_reported_to_the_board() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::create_dir_all(paths.board_socket().parent().unwrap()).unwrap();
    let board = tokio::net::UnixListener::bind(paths.board_socket()).unwrap();
    let worker = FauxProvider::new(vec![]);
    let runtime = runtime(home.path(), work.path(), &worker);
    let (stream, _) = tokio::time::timeout(Duration::from_secs(5), board.accept()).await.unwrap().unwrap();
    let (read, _write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let chat_model = FauxProvider::new(vec![FauxResponse::text("Good morning to you.")]);
    let principal_model = FauxProvider::new(vec![]);
    let options = nypi_core::chat::ChatOptions {
        model: Some("faux/faux-1".into()),
        stream_fn: Some(chat_model.stream_fn()),
        principal_stream_fn: Some(principal_model.stream_fn()),
    };
    let (chat, _events) = nypi_core::chat::start(runtime, options).unwrap();
    // The link says hello first; then the turn.
    let mut steps: Vec<(String, String)> = Vec::new();
    let mut said = false;
    while steps.last().is_none_or(|(step, _)| step != "idle") {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await.unwrap().unwrap().unwrap();
        let framed: Framed<LinkUp> = serde_json::from_str(&line).unwrap();
        match framed.message {
            LinkUp::Hello { .. } if !said => {
                said = true;
                chat.say("good morning");
            }
            LinkUp::Plane { plane, step, tail, .. } if plane == "chat" => steps.push((step, tail)),
            _ => {}
        }
    }
    assert!(steps.iter().any(|(step, tail)| step == "Writing" && tail.contains("Good morning")), "{steps:?}");
    assert_eq!(steps.last().unwrap().0, "idle");
}
