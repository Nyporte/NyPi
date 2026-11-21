//! The hub's commands from another machine (decision D96), over loopback with three NyPi homes:
//! a personal node's `goals`, `tasks`, and `secrets set` act on the hub's stores (the secret
//! works for `http_request` there and shows up in no output or log); an exec-only node is
//! refused; a command outside the allowlist, or one that would start a run, is refused.

use std::time::Duration;

use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_core::config::Paths;
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use nypi_hub::pairing::{self, Trust, Way};
use nypi_vault::secrets::{Handle, SecretStore};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SECRET: &str = "sk-carried-0123456789abcdef";

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

/// A machine joined to the hub at `address` with an invite, as `trust`.
async fn machine(hub: &Paths, address: &str, name: &str, trust: Trust) -> (tempfile::TempDir, Paths) {
    let home = tempfile::tempdir().unwrap();
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::write(paths.settings(), r#"{ "envFiles": [], "entry": "http://127.0.0.1:9" }"#).unwrap();
    let code = pairing::invite(hub, name, trust, chrono::Duration::hours(1)).unwrap();
    let ways = [Way::Direct(address.to_string())];
    let joined = pairing::join(&paths, &ways, name, Some(&code), &format!("i-{name}"), |_| {}).await.unwrap();
    nypi_hub::instance::Instance {
        mode: nypi_hub::instance::Mode::Node,
        name: joined.name,
        instance_id: format!("i-{name}"),
        epoch: joined.epoch,
        hub_fingerprint: joined.hub_fingerprint,
        trust: joined.trust,
        direct: vec![address.to_string()],
        down: false,
    }
    .save(&paths)
    .unwrap();
    (home, paths)
}

/// Answers one request and says what its `Authorization` header was.
async fn echo_once() -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let seen = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0u8; 8192];
        let n = stream.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..n]).into_owned();
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
        request
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase().starts_with("authorization:").then(|| line[14..].trim().to_string())
            })
            .unwrap_or_default()
    });
    (address, seen)
}

#[tokio::test]
async fn the_hubs_commands_run_on_the_hub_from_a_personal_machine_only() {
    nypi_hub::protocol::set_admin_runner(crate::admin_runner());
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let hub = Paths { home: home.path().to_path_buf() };
    let settings =
        json!({ "sandbox": "off", "envFiles": [], "approval": { "trustedCredentials": { "shop": ["127.0.0.1"] } } });
    std::fs::write(hub.settings(), settings.to_string()).unwrap();
    nypi_hub::instance::Instance::new_hub(&hub, "mini").unwrap().save(&hub).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let nodes = nypi_hub::nodes::Nodes::open(&hub);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let idle = FauxProvider::new(vec![]);
    tokio::spawn(nypi_hub::daemon::serve_hub(
        runtime(home.path(), work.path(), &idle),
        tokio::net::UnixListener::bind(home.path().join("d.sock")).unwrap(),
        shutdown.clone(),
        None,
        Some(nodes),
        nypi_hub::daemon::Network::direct_only(Some(listener)),
    ));
    let (_laptop_home, laptop) = machine(&hub, &address, "laptop", Trust::Personal).await;
    let (_vps_home, vps) = machine(&hub, &address, "vps", Trust::ExecOnly).await;
    let reach = nypi_hub::reach::Reach { lan: None, direct_wait: Duration::from_millis(500) };
    let words = |text: &str| text.split_whitespace().map(str::to_string).collect::<Vec<_>>();

    // Goals and tasks act on the hub's stores.
    let (said, code) =
        crate::client::admin(&laptop, &reach, words("goals add ship the site --term medium"), None).await.unwrap();
    assert_eq!(code, 0, "{said}");
    let (goals, code) = crate::client::admin(&laptop, &reach, words("goals"), None).await.unwrap();
    assert_eq!(code, 0);
    assert!(goals.contains("ship the site"), "{goals}");
    let hub_goals = std::fs::read_to_string(nypi_core::goals::goals_path(&hub.home)).unwrap_or_default();
    assert!(hub_goals.contains("ship the site"), "kept on the hub: {hub_goals}");
    assert!(!nypi_core::goals::goals_path(&laptop.home).exists(), "and not on the laptop");
    let (_, code) = crate::client::admin(&laptop, &reach, words("tasks"), None).await.unwrap();
    assert_eq!(code, 0);

    // A secret read on the laptop is stored on the hub, and works there; its value is in no
    // output or log.
    let (said, code) =
        crate::client::admin(&laptop, &reach, words("secrets set shop/key"), Some(SECRET.into())).await.unwrap();
    assert_eq!(code, 0, "{said}");
    assert!(said.contains("Stored cred://shop/key") && !said.contains(SECRET), "{said}");
    let handle = Handle::parse("shop/key").unwrap();
    assert_eq!(SecretStore::open(&hub.home).unwrap().get(&handle).unwrap().unwrap().expose(), SECRET);
    assert!(!laptop.secrets().exists(), "nothing stored on the laptop");
    let (listed, _) = crate::client::admin(&laptop, &reach, words("secrets list"), None).await.unwrap();
    assert!(listed.contains("cred://shop/key") && !listed.contains(SECRET), "{listed}");
    let (server, seen) = echo_once().await;
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call(
            "h1",
            "http_request",
            json!({ "url": format!("http://{server}/orders"), "auth": { "type": "bearer", "cred": "cred://shop/key" } }),
        ),
        FauxResponse::text("Done: asked the shop."),
    ]);
    let mut on_hub = runtime(home.path(), work.path(), &faux);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = on_hub.prompt("check the orders", &tx).await.unwrap();
    assert_eq!(seen.await.unwrap(), format!("Bearer {SECRET}"), "used by http_request on the hub");
    assert!(!serde_json::to_string(&outcome.new_messages).unwrap().contains(SECRET));
    for folder in ["logs", "audit", "run", "devices"] {
        let mut text = String::new();
        for entry in walk(&hub.home.join(folder)) {
            text.push_str(&String::from_utf8_lossy(&std::fs::read(entry).unwrap_or_default()));
        }
        assert!(!text.contains(SECRET), "the value in the hub's {folder}");
    }

    // An exec-only machine is refused, by the hub too; and so is what is not on the allowlist,
    // or would start a run.
    let refused = crate::client::admin(&vps, &reach, words("tasks"), None).await.unwrap_err();
    assert!(format!("{refused:#}").contains("exec-only"), "{refused:#}");
    let refused = crate::client::admin(&vps, &reach, words("update --all"), None).await.unwrap_err();
    assert!(format!("{refused:#}").contains("exec-only"), "a fleet update from an exec-only machine: {refused:#}");
    let (said, code) = crate::client::admin(&laptop, &reach, words("update"), None).await.unwrap();
    assert_eq!(code, 2, "{said}");
    let (said, code) = crate::client::admin(&laptop, &reach, words("hub here"), None).await.unwrap();
    assert_eq!(code, 2);
    assert!(said.contains("not one of the hub's commands"), "{said}");
    let (said, code) = crate::client::admin(&laptop, &reach, words("goals review"), None).await.unwrap();
    assert_eq!(code, 2, "{said}");
    let (said, code) = crate::client::admin(&laptop, &reach, words("secrets set shop/other"), None).await.unwrap();
    assert_eq!(code, 2, "a value read here, or nothing: {said}");
    shutdown.cancel();
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

#[test]
fn the_allowlist_names_the_hubs_commands() {
    let words = |text: &str| text.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    let allowed = nypi_hub::protocol::admin_allowed;
    assert!(allowed(&words("tasks"), false).is_ok());
    assert!(allowed(&words("devices pending"), false).is_ok());
    assert!(allowed(&words("eval --list"), false).is_ok());
    assert!(allowed(&words("eval"), false).is_err(), "a benchmark run stays here");
    assert!(allowed(&words("status"), false).is_err());
    assert!(allowed(&words("specialists run x"), false).is_err());
    assert!(allowed(&words("secrets set a/b"), true).is_ok());
    assert!(allowed(&words("update --node vps --version v0.4.1"), false).is_ok());
    assert!(allowed(&words("update --all"), false).is_ok());
    assert!(allowed(&words("update"), false).is_err(), "plain update is about the machine it is typed on");
    assert!(allowed(&words("update --check"), false).is_err());
}
