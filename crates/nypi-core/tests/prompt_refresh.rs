//! The system prompt is brought up to date between turns, as in Pi (decision D47): an edit made
//! during a run reaches the next request as a section update. Faux provider only.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, transcript};
use nypi_core::config::Paths;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use serde_json::json;
use tokio::sync::mpsc;

/// The prompt updates after the leading system message, rendered.
fn updates(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::System(system) => Some(transcript::render(system)),
            _ => None,
        })
        .skip(1)
        .collect()
}

#[tokio::test]
async fn an_edit_during_a_run_reaches_the_next_request() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(home.path().join("settings.json"), json!({ "enforceStatusLine": false }).to_string()).unwrap();
    std::fs::write(work.path().join("notes.txt"), "nothing to see").unwrap();
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("t1", "read", json!({ "path": "notes.txt" })),
        FauxResponse::tool_call("t2", "write", json!({ "path": "AGENTS.md", "content": "Always answer in haiku." })),
        FauxResponse::text("Noted."),
        FauxResponse::text("Haiku follows."),
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
    let (tx, _rx) = mpsc::unbounded_channel();
    runtime.prompt("set up the project rules", &tx).await.unwrap();

    let requests = faux.requests();
    assert_eq!(requests.len(), 3);
    assert!(updates(&requests[1].messages).is_empty(), "a turn that changed nothing adds nothing");
    let after_edit = updates(&requests[2].messages);
    assert_eq!(after_edit.len(), 1, "{after_edit:?}");
    assert!(after_edit[0].contains("Always answer in haiku."), "{}", after_edit[0]);

    // The update is part of the transcript, so the next prompt does not send it again.
    runtime.prompt("next", &tx).await.unwrap();
    assert_eq!(updates(&faux.requests()[3].messages).len(), 1);
}
