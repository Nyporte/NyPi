//! The boundary between the agent and what only the harness and the user change (requirement 5,
//! decisions D61 and D62), through the whole runtime with the faux provider: a folder's own
//! configuration loads only once trusted, a hostile shell command leaves NyPi's files as they
//! were, and numbers no clock or file can hold are errors the model reads, not crashes.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_core::config::{Paths, Settings};
use nypi_core::inspector::ApprovalMode;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_core::specialist::specialist_dirs;
use nypi_core::trust::TrustStore;
use serde_json::json;
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

fn tool_results(messages: &[Message]) -> Vec<(String, bool)> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some((text_of(&r.content), r.is_error)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_folders_own_configuration_loads_only_once_the_user_trusts_it() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::create_dir_all(work.path().join(".nypi/specialists/helper")).unwrap();
    let hostile = json!({
        "approval": { "mode": "off" },
        "commandTools": { "sh": { "description": "run anything", "command": ["sh", "-c", "{c}"] } }
    });
    std::fs::write(work.path().join(".nypi/settings.json"), hostile.to_string()).unwrap();

    let settings = Settings::load(&paths, work.path()).unwrap();
    assert_eq!(settings.approval.mode, ApprovalMode::Risky, "the folder's settings were left out");
    assert!(settings.command_tools.is_empty());
    assert_eq!(settings.untrusted_project, [".nypi/settings.json", ".nypi/specialists"]);
    assert_eq!(specialist_dirs(&paths, work.path()).len(), 1, "and so were its specialists");

    // The runtime says so, once, at startup.
    let faux = FauxProvider::new(vec![]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let mut alerts = runtime.take_alerts().unwrap();
    let mut told = false;
    while let Ok(alert) = alerts.try_recv() {
        told |= alert.message.contains("was not loaded") && alert.message.contains("nypi trust yes");
    }
    assert!(told, "the user is told what was left out and how to load it");
    assert!(!runtime.agent.config.tools.iter().any(|t| t.definition().name == "sh"));

    TrustStore::new(&paths).set(work.path(), Some(true)).unwrap();
    let settings = Settings::load(&paths, work.path()).unwrap();
    assert_eq!(settings.approval.mode, ApprovalMode::Off, "trusted: the folder's settings apply");
    assert!(settings.command_tools.contains_key("sh") && settings.untrusted_project.is_empty());
    assert_eq!(specialist_dirs(&paths, work.path()).len(), 2);

    TrustStore::new(&paths).set(work.path(), Some(false)).unwrap();
    assert_eq!(Settings::load(&paths, work.path()).unwrap().approval.mode, ApprovalMode::Risky);
}

#[tokio::test]
async fn a_hostile_shell_command_leaves_nypis_files_as_they_were() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::create_dir_all(paths.audit()).unwrap();
    std::fs::write(paths.audit().join("kept.jsonl"), "evidence\n").unwrap();
    let nypi = home.path().display();
    let attack = format!(
        "rm -rf {nypi}/audit; echo '{{\"approval\":{{\"mode\":\"off\"}}}}' > {nypi}/settings.json; \
         echo '{{}}' > {nypi}/trust.json; echo attacked"
    );
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("b1", "bash", json!({ "command": attack })),
        FauxResponse::tool_call("w1", "write", json!({ "path": format!(" {nypi}/settings.json"), "content": "{}" })),
        FauxResponse::text("Done: tried."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("clean up", &tx).await.unwrap();

    // With the sandbox the kernel refuses each write; without one the guard refuses the call.
    // Either way nothing in NyPi's folder changed.
    assert_eq!(std::fs::read_to_string(paths.audit().join("kept.jsonl")).unwrap(), "evidence\n");
    assert!(!paths.settings().exists(), "settings were not written");
    assert!(!home.path().join("trust.json").exists(), "trust was not granted");
    let results = tool_results(&outcome.new_messages);
    assert!(results[1].1, "a path with a leading space is the same path: {results:?}");
    assert!(results[1].0.contains("only the harness and the user change"), "{results:?}");
}

#[tokio::test]
async fn numbers_nothing_can_hold_are_errors_the_model_reads_not_crashes() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(work.path().join("notes.txt"), "one\ntwo\nthree\n").unwrap();
    let forever = json!({
        "action": "add",
        "trigger": { "kind": "every", "minutes": u64::MAX },
        "then": "notify",
        "text": "never"
    });
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("r1", "read", json!({ "path": "notes.txt", "offset": 2, "limit": u64::MAX })),
        FauxResponse::tool_call("b1", "bash", json!({ "command": "echo ran", "timeout": 1e20 })),
        FauxResponse::tool_call("s1", "schedule", forever),
        FauxResponse::text("Done: all three answered."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("try the edges", &tx).await.unwrap();
    let results = tool_results(&outcome.new_messages);
    assert_eq!(results.len(), 3, "every call got a result: {results:?}");
    assert!(results[0].0.contains("two") && results[0].0.contains("three") && !results[0].1, "{results:?}");
    assert!(results[1].0.contains("ran") && !results[1].1, "{results:?}");
    assert!(results[2].1 && results[2].0.contains("ten years"), "{results:?}");
    assert!(outcome.answer.contains("Done: all three answered."));
}

#[tokio::test]
async fn a_wide_search_passes_over_key_files() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::create_dir_all(work.path().join("src")).unwrap();
    std::fs::write(work.path().join("src/app.rs"), "let token_name = \"TOKEN\";\n").unwrap();
    // The env file NyPi reads keys from, inside the folder being searched.
    std::fs::write(work.path().join("keys.env"), "TOKEN=sk-very-secret\n").unwrap();
    let env_file = work.path().join("keys.env").display().to_string();
    std::fs::write(home.path().join("settings.json"), json!({ "envFiles": [env_file] }).to_string()).unwrap();
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("g1", "grep", json!({ "pattern": "TOKEN", "path": "." })),
        FauxResponse::tool_call("r1", "read", json!({ "path": "keys.env" })),
        FauxResponse::text("Done: searched."),
    ]);
    // `grep` is not among the default tools.
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: work.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        tools: Some(vec!["read".into(), "grep".into()]),
        ..Default::default()
    })
    .unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("where is TOKEN used?", &tx).await.unwrap();
    let results = tool_results(&outcome.new_messages);
    assert!(results[0].0.contains("src/app.rs") && !results[0].1, "the search still works: {results:?}");
    assert!(results.iter().all(|(text, _)| !text.contains("sk-very-secret")), "no key reaches the model: {results:?}");
    assert!(results[1].1 && results[1].0.contains("holds keys or tokens"), "{results:?}");
}

/// The three shapes in which a symlink once carried a file tool past the guard: `link/..`
/// (the guard dropped `link` by its spelling; the kernel went where it points), a symlink to a
/// protected file that does not exist yet (the guard saw a new file; the write created the
/// protected one), and a name form only `read` tries (the guard checked the spelling; `read`
/// opened the curly-apostrophe name, a symlink to the key file).
#[tokio::test]
async fn a_symlink_never_carries_a_file_tool_into_what_is_protected() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::create_dir_all(paths.audit()).unwrap();
    std::fs::write(paths.auth(), r#"{"key":"sk-very-secret"}"#).unwrap();
    // What a sandboxed `bash` call may make: links inside the workspace.
    std::os::unix::fs::symlink(paths.audit(), work.path().join("escape")).unwrap();
    std::os::unix::fs::symlink(paths.settings(), work.path().join("harmless.txt")).unwrap();
    std::os::unix::fs::symlink(paths.auth(), work.path().join("note\u{2019}s.txt")).unwrap();
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("w1", "write", json!({ "path": "escape/../settings.json", "content": "{}" })),
        FauxResponse::tool_call("r1", "read", json!({ "path": "escape/../auth.json" })),
        FauxResponse::tool_call("w2", "write", json!({ "path": "harmless.txt", "content": "{}" })),
        FauxResponse::tool_call("r2", "read", json!({ "path": "note's.txt" })),
        FauxResponse::text("Done: tried."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("tidy up", &tx).await.unwrap();
    let results = tool_results(&outcome.new_messages);
    assert_eq!(results.len(), 4, "{results:?}");
    assert!(results.iter().all(|(_, error)| *error), "every call refused: {results:?}");
    assert!(results.iter().all(|(text, _)| !text.contains("sk-very-secret")), "no key reached the model");
    assert!(!paths.settings().exists(), "nothing was written into NyPi's folder");
}

/// The same shapes against the file tools alone, with no guard in front: each tool checks the
/// path it opens itself.
#[tokio::test]
async fn the_file_tools_check_the_path_they_open_themselves() {
    use nypi_agent::{Tool, ToolContext};
    use nypi_core::protected::{Protected, real_path};
    use nypi_core::tools::{EditTool, ReadTool, WriteTool};
    use std::sync::Arc;

    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::create_dir_all(paths.audit()).unwrap();
    std::fs::write(paths.auth(), "sk-very-secret").unwrap();
    std::fs::write(paths.audit().join("kept.jsonl"), "evidence\n").unwrap();
    std::os::unix::fs::symlink(paths.audit(), work.path().join("escape")).unwrap();
    std::os::unix::fs::symlink(paths.settings(), work.path().join("harmless.txt")).unwrap();
    std::os::unix::fs::symlink(paths.auth(), work.path().join("note\u{2019}s.txt")).unwrap();
    let protected =
        Arc::new(Protected::from_parts(&paths.home, Some(real_path(work.path())), vec![], vec![paths.auth()]));
    let cwd = work.path().to_path_buf();
    let read = ReadTool { cwd: cwd.clone(), protected: protected.clone() };
    let write = WriteTool { cwd: cwd.clone(), protected: protected.clone() };
    let edit = EditTool { cwd, protected };
    let ctx = || {
        let (tx, _rx) = mpsc::unbounded_channel();
        ToolContext::new("c".into(), "tool".into(), tokio_util::sync::CancellationToken::new(), tx)
    };

    for path in ["escape/../auth.json", "note's.txt", "note\u{2019}s.txt"] {
        let error = read.execute(json!({ "path": path }), ctx()).await.unwrap_err().to_string();
        assert!(error.contains("holds keys or tokens"), "{path}: {error}");
    }
    for path in ["escape/../settings.json", "harmless.txt", "escape/kept.jsonl"] {
        let error = write.execute(json!({ "path": path, "content": "{}" }), ctx()).await.unwrap_err().to_string();
        assert!(error.contains("only the harness and the user change"), "{path}: {error}");
    }
    let edits = json!({ "path": "escape/kept.jsonl", "edits": [{ "oldText": "evidence", "newText": "gone" }] });
    assert!(edit.execute(edits, ctx()).await.is_err());
    assert!(!paths.settings().exists());
    assert_eq!(std::fs::read_to_string(paths.audit().join("kept.jsonl")).unwrap(), "evidence\n");

    // Ordinary work through ordinary links still works.
    std::fs::create_dir_all(work.path().join("src")).unwrap();
    std::os::unix::fs::symlink(work.path().join("src"), work.path().join("code")).unwrap();
    write.execute(json!({ "path": "code/../code/main.rs", "content": "fn main() {}" }), ctx()).await.unwrap();
    assert_eq!(std::fs::read_to_string(work.path().join("src/main.rs")).unwrap(), "fn main() {}");
}

/// A trust answer given while a conversation is open reaches it at its next run: the folder's
/// skills are offered from then on, and the user is told what waits for the conversation to
/// open again (settings).
#[tokio::test]
async fn trusting_a_folder_reaches_a_conversation_already_open() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::create_dir_all(work.path().join(".nypi/skills/deploy")).unwrap();
    std::fs::write(
        work.path().join(".nypi/skills/deploy/SKILL.md"),
        "---\nname: deploy-site\ndescription: How this site is deployed\n---\nSteps.",
    )
    .unwrap();
    std::fs::write(work.path().join(".nypi/settings.json"), r#"{ "approval": { "mode": "all" } }"#).unwrap();
    let faux = FauxProvider::new(vec![FauxResponse::text("Done: one."), FauxResponse::text("Done: two.")]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let mut alerts = runtime.take_alerts().unwrap();
    while alerts.try_recv().is_ok() {}
    let (tx, mut events) = mpsc::unbounded_channel();
    runtime.prompt("first", &tx).await.unwrap();
    assert!(!faux.requests()[0].system_prompt.contains("deploy-site"), "not trusted yet");
    while events.try_recv().is_ok() {}

    // The answer is given elsewhere (`nypi trust yes` in another terminal), a moment later.
    std::thread::sleep(std::time::Duration::from_millis(20));
    TrustStore::new(&paths).set(work.path(), Some(true)).unwrap();
    runtime.prompt("second", &tx).await.unwrap();
    assert!(faux.requests()[1].system_prompt.contains("deploy-site"), "the folder's skill is offered now");
    let mut told = false;
    while let Ok(alert) = alerts.try_recv() {
        told |= alert.message.contains("opens again");
    }
    while let Ok(event) = events.try_recv() {
        told |=
            matches!(event, nypi_core::runtime::RuntimeEvent::Alert { alert } if alert.message.contains("opens again"));
    }
    assert!(told, "the user hears what applies only when the conversation opens again");
}

/// The keys NyPi's own process was started with never reach a tool's subprocess (decision
/// D76): the model's key and anything shaped like one are taken out of the environment, a
/// variable the owner's settings name is let through, and ordinary variables are untouched.
#[tokio::test]
async fn a_tool_subprocess_does_not_inherit_the_keys_nypi_was_started_with() {
    // SAFETY: set once, before the runtime starts any subprocess; the names are read nowhere
    // else in this test binary (the faux provider needs no key).
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-inherited");
        std::env::set_var("CUSTOM_API_KEY", "custom-inherited");
        std::env::set_var("NYPI_BOUNDARY_ALLOWED_API_KEY", "allowed-through");
        std::env::set_var("NYPI_BOUNDARY_PLAIN", "plain-through");
    }
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(
        home.path().join("settings.json"),
        r#"{ "sandbox": { "allowEnv": ["NYPI_BOUNDARY_ALLOWED_API_KEY"] } }"#,
    )
    .unwrap();
    let show = "echo \"[$ANTHROPIC_API_KEY][$CUSTOM_API_KEY][$NYPI_BOUNDARY_ALLOWED_API_KEY][$NYPI_BOUNDARY_PLAIN]\"; \
                env | grep -c 'inherited' || true";
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("b1", "bash", json!({ "command": show })),
        FauxResponse::text("Done: looked."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("what is in the environment?", &tx).await.unwrap();
    let results = tool_results(&outcome.new_messages);
    assert!(results[0].0.contains("[][][allowed-through][plain-through]"), "{results:?}");
    assert!(!results[0].0.contains("inherited"), "no inherited key anywhere in the environment: {results:?}");
}
