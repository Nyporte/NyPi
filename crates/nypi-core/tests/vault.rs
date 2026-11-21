//! Lucia's vault, registry, and brokered secrets through the whole runtime (decision D77), with
//! the faux provider and a local HTTP server: the vault's tools never leave the vault, no tool
//! reads the secret store, and a credential used by handle reaches the server and is scrubbed
//! from everything that comes back.

use std::convert::Infallible;
use std::net::SocketAddr;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_core::config::Paths;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_vault::secrets::{Handle, SecretStore};
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

/// A server that says back what it was sent: the credential headers in its body and in a
/// header of its own, as a debugging endpoint or a careless API would.
async fn echo_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let service = service_fn(|request: Request<hyper::body::Incoming>| async move {
                    let header = |name: &str| {
                        request.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("-").to_string()
                    };
                    let (authorization, key) = (header("authorization"), header("x-api-key"));
                    let body = format!("path={} authorization={authorization} x-api-key={key}", request.uri().path());
                    let response = Response::builder()
                        .header("x-echo-authorization", authorization)
                        .body(Full::new(Bytes::from(body)))
                        .unwrap();
                    Ok::<_, Infallible>(response)
                });
                let _ =
                    hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
            });
        }
    });
    address
}

#[tokio::test]
async fn the_vault_is_written_searched_and_never_left_through_its_tools() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(work.path().join("report.pdf"), b"%PDF-1.4 the report").unwrap();
    std::fs::write(home.path().join("auth.json"), r#"{"key":"sk-very-secret"}"#).unwrap();
    let outside = work.path().join("outside.md").display().to_string();
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call(
            "v1",
            "vault_write",
            json!({ "path": "30-entities/atlas.md", "content": "# Atlas\nThe site. Runs on [[srv-mini]].\n" }),
        ),
        FauxResponse::tool_call(
            "v2",
            "vault_write",
            json!({ "path": "30-entities/srv-mini.md", "content": "# srv-mini\nA Mac mini in the office.\n" }),
        ),
        FauxResponse::tool_call("v3", "vault_search", json!({ "query": "atlas site" })),
        FauxResponse::tool_call("v4", "vault_write", json!({ "path": "../../escaped.md", "content": "x" })),
        FauxResponse::tool_call("v5", "vault_read", json!({ "path": outside })),
        FauxResponse::tool_call("v6", "vault_write", json!({ "path": ".index.jsonl", "content": "x" })),
        FauxResponse::tool_call("v7", "vault_attach", json!({ "source": "report.pdf" })),
        FauxResponse::tool_call("v8", "vault_attach", json!({ "source": home.path().join("auth.json") })),
        FauxResponse::tool_call(
            "v9",
            "vault_write",
            json!({ "path": "00-inbox/key.md", "content": "The key is sk-live-9aF3kLmN0pQrStUvWx" }),
        ),
        FauxResponse::tool_call("v10", "vault_read", json!({ "path": "30-entities/atlas.md#atlas" })),
        FauxResponse::text("Done: tried."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("keep notes", &tx).await.unwrap();
    let results = tool_results(&outcome.new_messages);
    assert_eq!(results.len(), 10, "{results:?}");
    assert!(!results[0].1 && !results[1].1, "{results:?}");
    // The search finds the site, and the server page comes along through the link.
    assert!(results[2].0.contains("30-entities/atlas.md#atlas") && results[2].0.contains("srv-mini.md"), "{results:?}");
    assert!(results[2].0.contains("linked from 30-entities/atlas.md"), "{results:?}");
    // Nothing leaves the vault, reaches its index, or brings a key file in.
    assert!(results[3].1 && results[3].0.contains("leaves the vault"), "{results:?}");
    assert!(results[4].1, "a path elsewhere is not a note: {results:?}");
    assert!(results[5].1 && results[5].0.contains("index"), "{results:?}");
    assert!(!results[6].1 && results[6].0.contains("attachments/report.pdf"), "{results:?}");
    assert!(results[7].1 && results[7].0.contains("keys or tokens"), "{results:?}");
    assert!(results[8].1 && results[8].0.contains("never hold secrets"), "{results:?}");
    assert!(results[9].0.contains("Runs on [[srv-mini]]"), "a section by its search id: {results:?}");
    assert!(!work.path().join("escaped.md").exists() && !home.path().join("escaped.md").exists());
    assert!(home.path().join("vault/attachments/report.pdf").is_file());
    assert!(!home.path().join("vault/00-inbox/key.md").exists());
    assert!(results.iter().all(|(text, _)| !text.contains("sk-very-secret")));
}

#[tokio::test]
async fn no_tool_reads_the_secret_store_or_its_key() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let handle = Handle::parse("api/main").unwrap();
    SecretStore::open(home.path()).unwrap().set(&handle, "tok-4f9a-very-secret").unwrap();
    let key = std::fs::read_to_string(home.path().join("secrets.key")).unwrap();
    let (enc, key_file) = (home.path().join("secrets.enc"), home.path().join("secrets.key"));
    let cat = format!("cat {} {}", enc.display(), key_file.display());
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("r1", "read", json!({ "path": enc })),
        FauxResponse::tool_call("r2", "read", json!({ "path": key_file })),
        FauxResponse::tool_call("b1", "bash", json!({ "command": cat })),
        FauxResponse::tool_call("w1", "write", json!({ "path": enc, "content": "{}" })),
        FauxResponse::tool_call("b2", "bash", json!({ "command": "nypi secrets list" })),
        FauxResponse::text("Done: tried."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let sandboxed = !runtime.agent.config.tools.is_empty() && cfg!(target_os = "macos");
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("look for keys", &tx).await.unwrap();
    let results = tool_results(&outcome.new_messages);
    assert!(results[0].1 && results[1].1, "the guard refuses both reads: {results:?}");
    assert!(results[0].0.contains("keys or tokens"), "{results:?}");
    if sandboxed && !results[2].0.contains(key.trim()) {
        // Under the system's sandbox the shell cannot read them either.
        assert!(results[2].1 || results[2].0.contains("not permitted"), "{results:?}");
    }
    assert!(!results[2].0.contains(key.trim()), "the master key never reaches the model: {results:?}");
    assert!(results[3].1 && results[4].1, "nor are they written, or handled through the command: {results:?}");
    assert!(results.iter().all(|(text, _)| !text.contains("tok-4f9a")));
}

#[tokio::test]
async fn a_credential_is_used_by_handle_and_scrubbed_from_what_comes_back() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let server = echo_server().await;
    let store = SecretStore::open(home.path()).unwrap();
    store.set(&Handle::parse("api/main").unwrap(), "tok-4f9a-very-secret").unwrap();
    store.set(&Handle::parse("other/unused").unwrap(), "pw-77-not-asked-for").unwrap();
    // The owner trusts the `api` scope for this host; nothing else.
    let settings = json!({ "approval": { "trustedCredentials": { "api": ["127.0.0.1"] } } });
    std::fs::write(home.path().join("settings.json"), settings.to_string()).unwrap();
    let url = |path: &str| format!("http://{server}/{path}");
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call(
            "h1",
            "http_request",
            json!({ "url": url("bearer"), "auth": { "type": "bearer", "cred": "cred://api/main" } }),
        ),
        FauxResponse::tool_call(
            "h2",
            "http_request",
            json!({ "url": url("header"), "auth": { "type": "header", "name": "X-Api-Key", "cred": "api/main" } }),
        ),
        FauxResponse::tool_call(
            "h3",
            "http_request",
            json!({ "url": url("basic"), "auth": { "type": "basic", "username": "root", "passwordCred": "api/main" } }),
        ),
        // Not trusted for this host: asked about, and here nobody can answer.
        FauxResponse::tool_call(
            "h4",
            "http_request",
            json!({ "url": url("other"), "auth": { "type": "bearer", "cred": "cred://other/unused" } }),
        ),
        FauxResponse::tool_call(
            "h5",
            "http_request",
            json!({ "url": url("raw"), "headers": { "Authorization": "Bearer tok-4f9a-very-secret-raw" } }),
        ),
        FauxResponse::tool_call(
            "h6",
            "http_request",
            json!({ "url": url("none"), "auth": { "type": "bearer", "cred": "cred://api/missing" } }),
        ),
        FauxResponse::tool_call("h7", "http_request", json!({ "url": url("plain") })),
        FauxResponse::text("Done: asked."),
    ]);
    let mut runtime = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = mpsc::unbounded_channel();
    let outcome = runtime.prompt("call the API", &tx).await.unwrap();
    let results = tool_results(&outcome.new_messages);
    assert_eq!(results.len(), 7, "{results:?}");
    // The server got the credential (it echoes a redacted mark where the value was), in each form.
    let redacted = "[REDACTED:cred://api/main]";
    assert!(results[0].0.contains(&format!("authorization=Bearer {redacted}")), "{results:?}");
    assert!(results[0].0.contains(&format!("x-echo-authorization: Bearer {redacted}")), "headers too: {results:?}");
    assert!(results[1].0.contains(&format!("x-api-key={redacted}")), "{results:?}");
    assert!(results[2].0.contains(&format!("authorization=Basic {redacted}")), "{results:?}");
    assert!(results[3].1 && results[3].0.contains("did not approve"), "an untrusted use is asked about: {results:?}");
    assert!(results[4].1 && results[4].0.contains("never written into a call"), "{results:?}");
    assert!(results[5].1 && results[5].0.contains("nypi secrets set api/missing"), "{results:?}");
    assert!(!results[6].1 && results[6].0.contains("path=/plain authorization=-"), "{results:?}");

    // No secret's value is in any result, in the session's messages, or in the audit log.
    let everything = format!("{:?}", outcome.new_messages);
    let audit: String = std::fs::read_dir(home.path().join("audit"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
        .collect();
    use base64::Engine;
    let basic = base64::engine::general_purpose::STANDARD.encode("root:tok-4f9a-very-secret");
    for leaked in ["tok-4f9a-very-secret\"", "tok-4f9a-very-secret ", "pw-77-not-asked-for", basic.as_str()] {
        assert!(!everything.contains(leaked), "in the transcript: {leaked}");
        assert!(!audit.contains(leaked), "in the audit log: {leaked}");
    }
    assert!(audit.contains("cred://api/main"), "the audit log names what was used");
}
