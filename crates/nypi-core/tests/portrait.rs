//! The user portrait with the faux provider: a turn's words become a trait in the background,
//! the next prompt carries it, and the agent cannot write the portrait itself.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_core::config::Paths;
use nypi_core::portrait::Portrait;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use serde_json::json;
use tokio::sync::mpsc;

#[tokio::test]
async fn a_turn_teaches_a_trait_that_the_next_prompt_carries() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::write(home.path().join("settings.json"), r#"{ "portrait": { "mode": "always" } }"#).unwrap();
    let faux = FauxProvider::new(vec![
        FauxResponse::text("Here it is.\n\nDone: answered."),
        FauxResponse::text(
            r#"{"changes": [{"op": "add", "category": "style", "text": "Wants answers as short bullet lists", "evidence": ["short bullet lists, no essays"]}]}"#,
        ),
        FauxResponse::tool_call(
            "w1",
            "write",
            json!({ "path": paths.portrait().display().to_string(), "content": "" }),
        ),
        FauxResponse::text("Ok.\n\nDone: replied."),
    ]);
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: work.path().to_path_buf(),
        paths: Some(paths.clone()),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::New,
        ..Default::default()
    })
    .unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    runtime.prompt("Summarize the release notes. I like short bullet lists, no essays.", &tx).await.unwrap();
    runtime.wait_reflections(std::time::Duration::from_secs(5)).await;

    let traits = Portrait::new(paths.portrait()).load();
    assert_eq!(traits.len(), 1, "{traits:?}");
    assert_eq!(traits[0].evidence[0].quote, "short bullet lists, no essays");
    let requests = faux.requests();
    let asked = &requests[1].messages[0];
    assert!(matches!(asked, Message::User(u) if text_of(&u.content).contains("1. Summarize the release notes")));

    let outcome = runtime.prompt("thanks", &tx).await.unwrap();
    let requests = faux.requests();
    let system = &requests.last().unwrap().system_prompt;
    assert!(system.contains("<portrait>") && system.contains("Wants answers as short bullet lists (style, shown 1×)"));
    let blocked = outcome
        .new_messages
        .iter()
        .any(|m| matches!(m, Message::ToolResult(r) if r.is_error && text_of(&r.content).contains("only the harness and the user change")));
    assert!(blocked, "the portrait is written only by the harness");
}
