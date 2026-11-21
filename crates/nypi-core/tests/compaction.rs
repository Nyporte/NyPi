//! Compaction in the middle of a run (Pi parity, decision D45): once the context crosses the
//! threshold between turns, the session is compacted before the next request. Faux provider only.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, StopReason, text_of};
use nypi_core::config::Paths;
use nypi_core::runtime::{Runtime, RuntimeEvent, RuntimeOptions, SessionMode};
use serde_json::json;
use tokio::sync::mpsc;

fn contains(messages: &[Message], needle: &str) -> bool {
    messages.iter().any(|m| match m {
        Message::ToolResult(r) => text_of(&r.content).contains(needle),
        Message::User(u) => text_of(&u.content).contains(needle),
        _ => false,
    })
}

#[tokio::test]
async fn a_long_run_compacts_between_turns() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // a.txt is about 100 tokens and b.txt about 2000 (in lines, since `read` cuts long ones), so
    // only the second read crosses the 1000-token threshold. Keeping 2050 recent tokens keeps
    // that read and summarizes everything before it.
    std::fs::write(work.path().join("a.txt"), format!("{}\n", "a".repeat(39)).repeat(10)).unwrap();
    std::fs::write(work.path().join("b.txt"), format!("{}\n", "b".repeat(39)).repeat(200)).unwrap();
    let settings = json!({
        "enforceStatusLine": false,
        "compaction": { "reserveTokens": 99_000, "keepRecentTokens": 2050 },
    });
    std::fs::write(home.path().join("settings.json"), settings.to_string()).unwrap();
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("t1", "read", json!({ "path": "a.txt" })),
        FauxResponse::tool_call("t2", "read", json!({ "path": "b.txt" })),
        FauxResponse::text("The user asked for both files; a.txt holds only the letter a."),
        FauxResponse::text("Both files hold one repeated letter."),
    ]);
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: work.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        ..Default::default()
    })
    .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("read a.txt, then b.txt", &tx).await.unwrap();

    assert_eq!(outcome.final_stop, Some(StopReason::Stop));
    assert_eq!(outcome.answer, "Both files hold one repeated letter.");
    let requests = faux.requests();
    assert_eq!(requests.len(), 4, "two reads, the summary, and the final reply");
    let last = &requests[3].messages;
    assert!(contains(last, "<summary>"), "the next request starts from the summary");
    assert!(!contains(last, "aaaa"), "the first read was summarized");
    assert!(contains(last, "bbbb"), "the second read was kept");

    let reasons: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|event| match event {
            RuntimeEvent::CompactionStart { reason } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(reasons, ["context nearly full"]);
}
