//! Thinking levels follow what the model accepts, as in Pi: setting one clamps it, a model switch
//! starts from the default level, and only a change is recorded. Faux provider only.

use nypi_ai::providers::faux::{FauxProvider, faux_model};
use nypi_ai::{Model, ThinkingLevel};
use nypi_core::config::Paths;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::session::{Entry, SessionStore};

fn model(id: &str, reasoning: bool) -> Model {
    Model { id: id.into(), reasoning, ..faux_model() }
}

fn thinking_changes(runtime: &Runtime) -> Vec<String> {
    SessionStore::open(&runtime.session_path())
        .unwrap()
        .entries()
        .iter()
        .filter_map(|e| match e {
            Entry::ThinkingLevelChange { thinking_level, .. } => Some(thinking_level.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn thinking_levels_are_clamped_to_the_model() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // A second model the registry knows: one without reasoning.
    std::fs::write(home.path().join("models.json"), serde_json::to_string(&[model("plain-1", false)]).unwrap())
        .unwrap();
    let faux = FauxProvider::new(vec![]);
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: work.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/thinker-1".into()),
        extra_model: Some(model("thinker-1", true)),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::New,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(runtime.agent.config.thinking, ThinkingLevel::Medium, "the default level");

    assert_eq!(runtime.set_thinking(ThinkingLevel::Max).unwrap(), ThinkingLevel::High, "max needs a mapping");
    assert_eq!(runtime.set_thinking(ThinkingLevel::High).unwrap(), ThinkingLevel::High);
    assert_eq!(thinking_changes(&runtime), ["high"], "setting the same level records nothing");

    runtime.set_model("faux/plain-1").unwrap();
    assert_eq!(runtime.agent.config.thinking, ThinkingLevel::Off, "a model without reasoning");
    runtime.set_model("faux/thinker-1").unwrap();
    assert_eq!(runtime.agent.config.thinking, ThinkingLevel::Medium, "back to the default, not stuck at off");
    assert_eq!(thinking_changes(&runtime), ["high", "off", "medium"]);
}

#[tokio::test]
async fn models_cycle_through_the_enabled_scope_as_in_pi() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let models = [model("plain-1", false), model("plain-2", false), model("thinker-1", true), model("other-1", false)];
    std::fs::write(home.path().join("models.json"), serde_json::to_string(&models).unwrap()).unwrap();
    // A key for the faux provider, and no env files, so only these models count as usable.
    std::fs::write(home.path().join("auth.json"), r#"{ "faux": "test-key" }"#).unwrap();
    let settings = serde_json::json!({ "envFiles": [], "enabledModels": ["faux/plain-*", "faux/thinker-1:high"] });
    std::fs::write(home.path().join("settings.json"), settings.to_string()).unwrap();
    let faux = FauxProvider::new(vec![]);
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

    let mut seen = Vec::new();
    for _ in 0..4 {
        let (model, level, scoped) = runtime.cycle_model().unwrap().expect("a model to move to");
        assert!(scoped);
        seen.push(format!("{}:{}", model.id, level.as_str()));
    }
    // As in Pi, a current model outside the scope counts as its first, so cycling starts at the second.
    assert_eq!(
        seen,
        ["plain-2:off", "thinker-1:high", "plain-1:off", "plain-2:off"],
        "the scope's order, with its level"
    );
}

#[tokio::test]
async fn a_new_session_starts_in_the_model_scope_as_in_pi() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let models = [model("plain-1", false), model("plain-2", false), model("thinker-1", true), model("other-1", false)];
    std::fs::write(home.path().join("models.json"), serde_json::to_string(&models).unwrap()).unwrap();
    std::fs::write(home.path().join("auth.json"), r#"{ "faux": "test-key" }"#).unwrap();
    let start = |default_model: Option<&str>, agent_name: Option<&str>| {
        let settings = serde_json::json!({ "envFiles": [], "defaultModel": default_model });
        std::fs::write(home.path().join("settings.json"), settings.to_string()).unwrap();
        let runtime = Runtime::new(RuntimeOptions {
            cwd: work.path().to_path_buf(),
            paths: Some(Paths { home: home.path().to_path_buf() }),
            models: Some(vec!["faux/thinker-1:high".into(), "faux/plain-*".into()]),
            agent_name: agent_name.map(str::to_string),
            stream_fn: Some(FauxProvider::new(vec![]).stream_fn()),
            session: SessionMode::Ephemeral,
            ..Default::default()
        })
        .unwrap();
        (runtime.agent.config.model.id.clone(), runtime.agent.config.thinking)
    };
    assert_eq!(
        start(None, None),
        ("thinker-1".to_string(), ThinkingLevel::High),
        "the scope's first model, at its level"
    );
    assert_eq!(start(Some("faux/plain-2"), None).0, "plain-2", "the default model when the scope holds it");
    assert_eq!(start(Some("faux/other-1"), Some("helper")).0, "other-1", "a specialist keeps the default model");
}
