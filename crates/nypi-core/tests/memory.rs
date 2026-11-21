//! Long-term memory with the faux provider: one session remembers, a later one recalls, the
//! store cannot be written around the tool, and the valve attaches what bears on a message.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_core::config::Paths;
use nypi_core::memory::{Emotion, MemoryKind, MemoryStore, NewMemory};
use nypi_core::pulse::{PulseKind, PulsePayload};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::valve::HoldReason;
use serde_json::json;
use tokio::sync::mpsc;

fn runtime(home: &std::path::Path, work: &std::path::Path, faux: &FauxProvider) -> Runtime {
    Runtime::new(RuntimeOptions {
        cwd: work.to_path_buf(),
        paths: Some(Paths { home: home.to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::New,
        ..Default::default()
    })
    .unwrap()
}

fn results(messages: &[Message]) -> Vec<(String, bool)> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some((text_of(&r.content), r.is_error)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_later_session_recalls_what_an_earlier_one_remembered() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    let first = FauxProvider::new(vec![
        FauxResponse::tool_call(
            "m1",
            "memory",
            json!({ "action": "remember", "text": "The user's sister Mei lives in Chengdu.", "tags": ["family", "妹妹", "成都"], "kind": "person", "importance": 0.7 }),
        ),
        FauxResponse::tool_call("w1", "write", json!({ "path": paths.memory().display().to_string(), "content": "" })),
        FauxResponse::text("Noted.\n\nDone: remembered your sister."),
    ]);
    let mut runtime1 = runtime(home.path(), work.path(), &first);
    assert!(runtime1.agent.config.system_prompt.contains("recall from memory"));
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime1.prompt("My sister Mei lives in Chengdu.", &tx).await.unwrap();
    let results1 = results(&outcome.new_messages);
    assert!(results1[0].0.contains("Remembered as m-"), "{results1:?}");
    assert!(
        results1[1].1 && results1[1].0.contains("only the harness and the user change"),
        "the store is written only through the tool: {results1:?}"
    );

    let store = MemoryStore::new(paths.memory());
    let memory = store.load().pop().unwrap();
    assert_eq!(memory.provenance.by, "Lucia");
    assert_eq!(memory.provenance.task, outcome.task.as_ref().map(|t| t.0.clone()));

    let second = FauxProvider::new(vec![
        FauxResponse::tool_call("r1", "memory", json!({ "action": "recall", "query": "我妹妹住在哪里" })),
        FauxResponse::text("Mei lives in Chengdu [t1].\n\nDone: answered from memory."),
    ]);
    let mut runtime2 = runtime(home.path(), work.path(), &second);
    let outcome = runtime2.prompt("我妹妹住在哪里?", &tx).await.unwrap();
    let recalled = results(&outcome.new_messages);
    assert!(recalled[0].0.contains("The user's sister Mei lives in Chengdu."), "{recalled:?}");
    assert_eq!(store.get(&memory.id).unwrap().recall_count, 2, "the valve attached it, then the tool recalled it");
}

/// The text of the last user message the model received.
fn last_user(faux: &FauxProvider) -> String {
    let requests = faux.requests();
    let context = requests.last().unwrap();
    context
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User(u) => Some(text_of(&u.content)),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn the_valve_attaches_relevant_memories_and_holds_back_sensitive_ones() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = MemoryStore::new(Paths { home: home.path().to_path_buf() }.memory());
    let add = |text: &str, tags: &[&str], kind, emotion| {
        let tags = tags.iter().map(|t| t.to_string()).collect();
        store.add(NewMemory { text: text.into(), tags, kind, importance: 0.8, emotion, ..Default::default() }).unwrap()
    };
    let tyre = add("The user's bike has a 700x28 front tyre.", &["bike", "tyre", "tire"], MemoryKind::Fact, None);
    let grief = Some(Emotion { valence: -0.9, arousal: 0.8 });
    let rosa = add(
        "The user's grandmother Rosa died in October; the user misses her.",
        &["grandmother", "Rosa", "family"],
        MemoryKind::Event,
        grief,
    );
    let faux =
        FauxProvider::new((0..4).map(|i| FauxResponse::text(&format!("Reply {i}.\n\nDone: replied."))).collect());
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let mut valve = runtime.pulses().subscribe("test", &[PulseKind::MemoryValve]);
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut decision = || match valve.try_recv().map(|p| p.payload) {
        Some(PulsePayload::MemoryValve { decision }) => decision,
        other => panic!("no valve pulse: {other:?}"),
    };

    runtime.prompt("Which inner tube size do I need for my bike tyre?", &tx).await.unwrap();
    let seen = last_user(&faux);
    assert!(seen.contains("<recalled-memories>") && seen.contains(&tyre), "{seen}");
    assert!(!seen.contains(&rosa), "unrelated memories stay out: {seen}");
    assert!(decision().is_open());

    runtime.prompt("haha my grandmother Rosa would have loved this bike", &tx).await.unwrap();
    let seen = last_user(&faux);
    assert!(!seen.contains(&rosa), "no grief brought up in a joke: {seen}");
    let light = decision();
    assert!(light.held.iter().any(|h| h.id == rosa && h.reason == HoldReason::Sensitive), "{light:?}");

    runtime.prompt("Which tyre size was it again?", &tx).await.unwrap();
    assert!(!last_user(&faux).contains("<recalled-memories>"), "already in the conversation");
    assert_eq!(decision().held[0].reason, HoldReason::AlreadyShown);

    runtime.prompt("Can you help me write a few lines about my grandmother Rosa?", &tx).await.unwrap();
    assert!(last_user(&faux).contains(&rosa), "asked about directly, it is brought in");
    assert!(decision().is_open());
    assert_eq!(store.get(&tyre).unwrap().recall_count, 1, "held-back memories are not counted as used");
}
