//! The system prompt carried in the transcript (decision D40): a new session leads with every
//! section, a changed section is appended as an update instead of rewriting the head, the
//! collapsed request equals the full prompt, and a reopened session carries on.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::transcript::{current_system, has_leading_system, prepare, render};
use nypi_ai::{Context, Message};
use nypi_core::config::Paths;
use nypi_core::goals::{Change, GoalsFile, Term};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use tokio::sync::mpsc;

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

fn systems(messages: &[Message]) -> Vec<&nypi_ai::SystemMessage> {
    messages.iter().filter_map(|m| if let Message::System(s) = m { Some(s) } else { None }).collect()
}

#[tokio::test]
async fn prompt_changes_append_to_the_transcript() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    let faux =
        FauxProvider::new((0..3).map(|i| FauxResponse::text(&format!("Reply {i}.\n\nDone: replied."))).collect());
    let mut first = runtime(home.path(), work.path(), &faux, SessionMode::New);
    let (tx, _rx) = mpsc::unbounded_channel();
    first.prompt("hello", &tx).await.unwrap();

    let requests = faux.requests();
    assert!(has_leading_system(&requests[0].messages));
    let Message::System(head) = &requests[0].messages[0] else { unreachable!() };
    assert_eq!(head.sections.0[0].0, "core");
    assert!(head.tools_added.iter().any(|t| t.name == "bash"), "the tools are declared in the head");

    GoalsFile::new(paths.goals()).add(Term::Short, "Ship the release", &Change::default(), "test").unwrap();
    first.prompt("what next?", &tx).await.unwrap();
    let requests = faux.requests();
    let second = &requests[1];
    assert_eq!(second.messages[0], requests[0].messages[0], "the head is unchanged");
    let updates = systems(&second.messages);
    assert_eq!(updates.len(), 2);
    let names: Vec<&str> = updates[1].sections.0.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["goals"], "only the changed section is sent");
    let collapsed = prepare(&Context { messages: second.messages.clone(), ..Default::default() }, false);
    // Folded, it holds the full prompt; a section that is new since the head comes last.
    let sorted = |text: &str| {
        let mut parts: Vec<String> = text.split("\n\n<").map(str::to_string).collect();
        parts.sort();
        parts
    };
    assert_eq!(sorted(&collapsed.system_prompt), sorted(&first.agent.config.system_prompt));
    assert!(collapsed.system_prompt.trim_end().ends_with("</goals>"));
    let session = first.session_path();
    drop(first);

    let mut reopened = runtime(home.path(), work.path(), &faux, SessionMode::Open(session));
    reopened.prompt("and now?", &tx).await.unwrap();
    let requests = faux.requests();
    let third = &requests[2].messages;
    assert_eq!(systems(third).len(), 2, "a reopened session neither leads again nor repeats an update");
    let replayed = render(&current_system(third).unwrap());
    let sorted = |text: &str| {
        let mut parts: Vec<String> = text.split("\n\n<").map(str::to_string).collect();
        parts.sort();
        parts
    };
    assert_eq!(sorted(&replayed), sorted(&reopened.agent.config.system_prompt));
}
