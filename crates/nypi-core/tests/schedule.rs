//! Timers and condition triggers with the faux provider: the agent schedules through its tool,
//! and a dangerous command condition is refused when it is set.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_core::config::Paths;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::schedule::{Action, ScheduleStore, Trigger};
use serde_json::json;
use tokio::sync::mpsc;

#[tokio::test]
async fn the_agent_schedules_and_the_guard_checks_commands() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call(
            "s1",
            "schedule",
            json!({ "action": "add", "trigger": { "kind": "at", "at": "2030-01-02 09:00" }, "then": "notify", "text": "Call the dentist" }),
        ),
        FauxResponse::tool_call(
            "s2",
            "schedule",
            json!({ "action": "add", "trigger": { "kind": "command", "command": "rm -rf ~", "when": "succeeds" }, "then": "prompt", "text": "x" }),
        ),
        FauxResponse::tool_call("s3", "schedule", json!({ "action": "list" })),
        FauxResponse::text("Set for 9:00 on 2 January [t1].\n\nDone: reminder set."),
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
    let outcome = runtime.prompt("Remind me to call the dentist on 2 January 2030 at 9.", &tx).await.unwrap();
    let results: Vec<(String, bool)> = outcome
        .new_messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some((text_of(&r.content), r.is_error)),
            _ => None,
        })
        .collect();
    assert!(results[0].0.contains("Scheduled s-") && !results[0].1, "{results:?}");
    assert!(results[1].1, "a dangerous command condition is refused: {results:?}");
    assert!(results[2].0.contains("Call the dentist (set by Lucia)"), "{results:?}");

    let active = ScheduleStore::new(paths.schedules()).active();
    assert_eq!(active.len(), 1);
    assert!(matches!(&active[0].trigger, Trigger::At { at } if at == "2030-01-02 09:00"));
    assert_eq!(active[0].action, Action::Notify { text: "Call the dentist".into() });
    assert_eq!(active[0].cwd, work.path());
}
