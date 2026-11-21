//! The static HTML export (decision D53): the branch as a page with no script, whatever the model
//! or a tool wrote. Faux provider only.

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_core::config::Paths;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use serde_json::json;
use tokio::sync::mpsc;

fn runtime(home: &std::path::Path, work: &std::path::Path, faux: &FauxProvider, session: SessionMode) -> Runtime {
    std::fs::write(home.join("settings.json"), json!({ "enforceStatusLine": false }).to_string()).unwrap();
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

#[tokio::test]
async fn a_session_exports_as_a_page_without_script() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(work.path().join("notes.txt"), "<b>file text</b>").unwrap();
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("t1", "read", json!({ "path": "notes.txt" })),
        FauxResponse::text(
            "**Done.** <script>alert(1)</script> [click](javascript:alert(2)) [docs](https://example.com) \
             <img src=x onerror=alert(3)>",
        ),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux, SessionMode::New);
    let (tx, _rx) = mpsc::unbounded_channel();
    runtime.prompt("read notes.txt", &tx).await.unwrap();
    runtime.run_user_bash("echo from-the-user", false, Default::default()).await.unwrap();

    let path = runtime.export_html(None).unwrap();
    assert_eq!(path.parent(), Some(work.path()));
    assert!(path.file_name().unwrap().to_string_lossy().starts_with("nypi-session-"));
    let html = std::fs::read_to_string(&path).unwrap();

    assert!(html.contains("<strong>Done.</strong>"), "Markdown is rendered");
    assert!(html.contains(r#"<a href="https://example.com">docs</a>"#));
    assert!(!html.contains("<script"), "raw HTML is shown as text");
    assert!(!html.contains("javascript:alert"), "unsafe link targets are dropped");
    assert!(!html.contains("<img src=x"), "no live attributes from model text");
    assert!(html.contains("Content-Security-Policy"));
    let call = html.find("<b>read</b>").expect("the tool call");
    let result = html.find("&lt;b&gt;file text&lt;/b&gt;").expect("its result, escaped");
    assert!(call < result, "the result sits under its call");
    assert!(html.contains("$ echo from-the-user") && html.contains("User ran"));

    assert_eq!(runtime.export_html(Some("chosen.html")).unwrap(), work.path().join("chosen.html"));
}

#[tokio::test]
async fn an_in_memory_session_cannot_be_exported() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let runtime = runtime(home.path(), work.path(), &FauxProvider::new(vec![]), SessionMode::Ephemeral);
    let error = runtime.export_html(None).unwrap_err().to_string();
    assert_eq!(error, "Cannot export in-memory session to HTML");
}
