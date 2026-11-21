//! `nypi update` end to end (decision D98) against a release server on loopback: a signed
//! manifest, its signature, and an archive holding a stand-in `nypi` that answers `--version`.
//! Nothing here reaches the network.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use nypi_core::config::{Paths, Settings, UpdateMode};
use nypi_core::update::{Keypair, Source, Version};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::update::{self, Applied, Options, Updater, Verdict};

const TARGET: &str = "test-target";

/// A release server: files by path, and every path asked for.
#[derive(Clone, Default)]
struct Server {
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Server {
    async fn start(&self) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = self.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let server = server.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") && stream.read_exact(&mut byte).await.is_ok() {
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or_default().to_string();
                    let etag_asked = head.lines().find_map(|line| {
                        line.to_ascii_lowercase().starts_with("if-none-match:").then(|| line[14..].trim().to_string())
                    });
                    server.asked.lock().unwrap().push(path.clone());
                    let body = server.files.lock().unwrap().get(&path).cloned();
                    let answer = match body {
                        Some(body) => {
                            let etag = format!("\"{}\"", nypi_hub::state::sha(&body));
                            if etag_asked.as_deref() == Some(etag.as_str()) {
                                b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_vec()
                            } else {
                                let mut answer = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {etag}\r\nConnection: close\r\n\r\n",
                                    body.len()
                                )
                                .into_bytes();
                                answer.extend_from_slice(&body);
                                answer
                            }
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = stream.write_all(&answer).await;
                });
            }
        });
        base
    }

    fn put(&self, path: &str, bytes: Vec<u8>) {
        self.files.lock().unwrap().insert(path.to_string(), bytes);
    }

    fn asked_for(&self, end: &str) -> bool {
        self.asked.lock().unwrap().iter().any(|path| path.ends_with(end))
    }
}

/// A stand-in `nypi` that says `version` when asked.
fn stub(says: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho 'nypi {says} (test stub)'\n").into_bytes()
}

/// A `.tar.gz` of `(name, bytes)`; a name is written as it is, `..` included.
fn tarball(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for (name, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        let raw = &mut header.as_gnu_mut().unwrap().name;
        raw[..name.len()].copy_from_slice(name.as_bytes());
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, bytes.as_slice()).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// Publish `version` with `archive` on `server`, signed with `pair`; `signed_sha` stands for the
/// archive's hash in the manifest when given.
fn publish(server: &Server, pair: &Keypair, version: &str, archive: &[u8], signed_sha: Option<&str>) {
    let file = format!("nypi-{version}-{TARGET}.tar.gz");
    let sha = signed_sha.map(str::to_string).unwrap_or_else(|| nypi_hub::state::sha(archive));
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "version": version, "tag": format!("v{version}"), "publishedAt": "2026-11-20T00:00:00Z",
        "notesUrl": "", "assets": { TARGET: { "file": file, "sha256": sha, "size": archive.len() } },
    }))
    .unwrap();
    let signature = nypi_core::update::sign(&pair.private_pkcs8, &manifest).unwrap().into_bytes();
    for at in ["/latest/download".to_string(), format!("/download/v{version}")] {
        server.put(&format!("{at}/manifest.json"), manifest.clone());
        server.put(&format!("{at}/manifest.json.sig"), signature.clone());
    }
    server.put(&format!("/download/v{version}/{file}"), archive.to_vec());
}

/// A NyPi home with an installed binary that says `old`.
fn home() -> (tempfile::TempDir, Paths) {
    let home = tempfile::tempdir().unwrap();
    let paths = Paths { home: home.path().to_path_buf() };
    std::fs::create_dir_all(paths.home.join("bin")).unwrap();
    std::fs::write(update::binary(&paths), b"old").unwrap();
    (home, paths)
}

fn updater(paths: &Paths, base: &str, key: &str, current: &str) -> Updater {
    let source = Source::loopback(base).unwrap();
    Updater::with(paths, source, Some(key.to_string()), TARGET, Version::parse(current).unwrap())
}

fn installed(paths: &Paths) -> Vec<u8> {
    std::fs::read(update::binary(paths)).unwrap()
}

/// The happy path, then a rollback by hand.
#[tokio::test]
async fn a_signed_release_is_checked_and_put_in_place_and_can_be_rolled_back() {
    let pair = nypi_core::update::generate().unwrap();
    let server = Server::default();
    let base = server.start().await;
    publish(&server, &pair, "0.4.1", &tarball(&[("nypi", stub("0.4.1"))]), None);
    let (_home, paths) = home();
    let updater = updater(&paths, &base, &pair.public, "0.4.0");
    let checked = updater.check(false).await.unwrap();
    assert!(checked.newer && checked.latest.version == "0.4.1");
    let applied = updater.apply(&Options::default(), false).await.unwrap();
    assert_eq!(applied, Applied::Swapped { from: "0.4.0".into(), to: "0.4.1".into() });
    assert_eq!(installed(&paths), stub("0.4.1"));
    assert_eq!(std::fs::read(paths.home.join("bin/nypi.prev")).unwrap(), b"old");
    let pending = update::pending(&paths).unwrap();
    assert_eq!((pending.from_version.as_str(), pending.to_version.as_str()), ("0.4.0", "0.4.1"));
    assert!(!paths.home.join("bin/.staging").exists() && !paths.home.join("bin/nypi.new").exists());

    let back = update::rollback(&paths).unwrap();
    assert_eq!(back, "0.4.0");
    assert_eq!(installed(&paths), b"old");
    assert!(update::pending(&paths).is_none());
}

/// Every check refuses before the installed binary changes: a signature by another key (before
/// the archive is even fetched), an archive that is not the signed one, a path that leaves the
/// archive, a binary that does not say the version.
#[tokio::test]
async fn nothing_unchecked_replaces_the_installed_binary() {
    let pair = nypi_core::update::generate().unwrap();
    let other = nypi_core::update::generate().unwrap();
    // What is wrong, the archive, the hash the manifest names (None: its own), who signs, what is said.
    type Case<'a> = (&'a str, Vec<u8>, Option<&'a str>, &'a Keypair, &'a str);
    let cases: Vec<Case> = vec![
        ("signed by another key", tarball(&[("nypi", stub("0.4.1"))]), None, &other, "not signed by the release key"),
        ("not the signed archive", tarball(&[("nypi", stub("0.4.1"))]), Some("00"), &pair, "SHA-256"),
        (
            "a path that leaves it",
            tarball(&[("../evil", stub("0.4.1")), ("nypi", stub("0.4.1"))]),
            None,
            &pair,
            "leaves",
        ),
        ("a binary that says another version", tarball(&[("nypi", stub("9.9.9"))]), None, &pair, "not nypi 0.4.1"),
        ("no nypi in it", tarball(&[("README", b"hi".to_vec())]), None, &pair, "holds no nypi"),
    ];
    for (what, archive, signed_sha, signer, said) in cases {
        let server = Server::default();
        let base = server.start().await;
        publish(&server, signer, "0.4.1", &archive, signed_sha);
        let (_home, paths) = home();
        let error = updater(&paths, &base, &pair.public, "0.4.0").apply(&Options::default(), false).await.unwrap_err();
        assert!(format!("{error:#}").contains(said), "{what}: {error:#}");
        assert_eq!(installed(&paths), b"old", "{what}: the installed binary is untouched");
        assert!(update::pending(&paths).is_none() && !paths.home.join("bin/nypi.new").exists(), "{what}");
        if what == "signed by another key" {
            assert!(!server.asked_for(".tar.gz"), "refused before the archive was fetched");
        }
    }
}

/// An older release is not taken unless named.
#[tokio::test]
async fn a_downgrade_needs_the_version_named() {
    let pair = nypi_core::update::generate().unwrap();
    let server = Server::default();
    let base = server.start().await;
    publish(&server, &pair, "0.4.1", &tarball(&[("nypi", stub("0.4.1"))]), None);
    let (_home, paths) = home();
    let updater = updater(&paths, &base, &pair.public, "0.5.0");
    assert_eq!(updater.apply(&Options::default(), false).await.unwrap(), Applied::UpToDate { version: "0.5.0".into() });
    assert_eq!(installed(&paths), b"old");
    let named = Options { version: Some("v0.4.1".into()), allow_unsigned: false };
    assert!(matches!(updater.apply(&named, false).await.unwrap(), Applied::Swapped { .. }));
    assert_eq!(installed(&paths), stub("0.4.1"));
}

/// The supervisor's verdicts on a pending update: kept once everything stays up; put back when
/// a service stays down past the deadline, the version then blocked for background updates.
#[tokio::test]
async fn the_supervisor_keeps_an_update_that_stays_up_and_puts_back_one_that_does_not() {
    let pair = nypi_core::update::generate().unwrap();
    let server = Server::default();
    let base = server.start().await;
    publish(&server, &pair, "0.4.1", &tarball(&[("nypi", stub("0.4.1"))]), None);
    let (_home, paths) = home();
    let updater = updater(&paths, &base, &pair.public, "0.4.0");
    updater.apply(&Options::default(), false).await.unwrap();
    let pending = update::pending(&paths).unwrap();
    let now = chrono::Utc::now();
    let up = |seconds: u64| Some(std::time::Duration::from_secs(seconds));
    assert_eq!(update::verdict(&pending, now, up(5), false), Verdict::Wait);
    assert_eq!(update::verdict(&pending, now, up(31), false), Verdict::Confirm);
    assert_eq!(update::verdict(&pending, now, None, true), Verdict::RollBack, "a service that keeps stopping");
    let later = now + chrono::Duration::seconds(91);
    assert_eq!(update::verdict(&pending, later, None, false), Verdict::RollBack, "never up within 90 seconds");
    assert_eq!(update::verdict(&pending, now, None, false), Verdict::Wait);

    let said = update::roll_back_failed(&paths).unwrap();
    assert_eq!(said, "Failed: update to v0.4.1 did not stay healthy; rolled back to v0.4.0.");
    assert_eq!(installed(&paths), b"old");
    assert!(update::pending(&paths).is_none());
    assert_eq!(update::state(&paths).blocked_version.as_deref(), Some("0.4.1"));

    // The background check never takes the blocked version again, even in auto mode.
    let mut settings = Settings::default();
    settings.update.mode = UpdateMode::Auto;
    let told = Mutex::new(None);
    let applied = update::background(
        &paths,
        &settings,
        &updater,
        || true,
        async |text: String| {
            *told.lock().unwrap() = Some(text);
        },
    )
    .await;
    assert!(!applied && told.lock().unwrap().is_none());
    assert_eq!(installed(&paths), b"old");

    // A second update that stays up is kept.
    let mut state = update::state(&paths);
    state.blocked_version = None;
    update::save_state(&paths, &state);
    updater.apply(&Options::default(), false).await.unwrap();
    assert!(update::confirm(&paths).is_some());
    assert!(update::pending(&paths).is_none());
    assert_eq!(update::state(&paths).last_applied.as_deref(), Some("0.4.1"));
}

/// The background check: told once per version; a relay is told, never updated unasked; an idle
/// machine in auto mode is updated; a busy one waits.
#[tokio::test]
async fn background_checks_tell_once_and_leave_a_relay_alone() {
    let pair = nypi_core::update::generate().unwrap();
    let server = Server::default();
    let base = server.start().await;
    publish(&server, &pair, "0.4.1", &tarball(&[("nypi", stub("0.4.1"))]), None);
    let run = async |settings: &Settings, paths: &Paths, idle: bool| {
        let told = Mutex::new(None);
        let applied = update::background(
            paths,
            settings,
            &updater(paths, &base, &pair.public, "0.4.0"),
            || idle,
            async |text: String| {
                *told.lock().unwrap() = Some(text);
            },
        )
        .await;
        (applied, told.into_inner().unwrap())
    };
    let (_notify_home, notify) = home();
    let settings = Settings::default();
    let (applied, told) = run(&settings, &notify, true).await;
    assert!(!applied);
    assert_eq!(told.as_deref(), Some("NyPi v0.4.1 is available — run nypi update"));
    let mut state = update::state(&notify);
    state.last_check = None;
    update::save_state(&notify, &state);
    assert_eq!(run(&settings, &notify, true).await.1, None, "told once per version");

    let (_relay_home, relay) = home();
    let mut on_relay = Settings::default();
    on_relay.update.mode = UpdateMode::Auto;
    on_relay.relay.enabled = true;
    let (applied, told) = run(&on_relay, &relay, true).await;
    assert!(!applied && told.is_some(), "a relay is told, not updated");
    assert_eq!(installed(&relay), b"old");

    let (_busy_home, busy) = home();
    let mut auto = Settings::default();
    auto.update.mode = UpdateMode::Auto;
    let (applied, _) = run(&auto, &busy, false).await;
    assert!(!applied && installed(&busy) == b"old", "a busy machine waits");
    let (_idle_home, idle) = home();
    let (applied, told) = run(&auto, &idle, true).await;
    assert!(applied && told.is_none(), "an idle machine is updated");
    assert_eq!(installed(&idle), stub("0.4.1"));
    assert!(update::pending(&idle).unwrap().automatic);
}

/// A pending update whose node never came up: the supervisor puts the old binary back, raises
/// an error that goes to the outbox, and ends so it is started again on the old binary.
#[tokio::test]
async fn the_supervisor_rolls_back_an_update_whose_service_stays_down() {
    let pair = nypi_core::update::generate().unwrap();
    let server = Server::default();
    let base = server.start().await;
    publish(&server, &pair, "0.4.1", &tarball(&[("nypi", stub("0.4.1"))]), None);
    let (_home, paths) = home();
    updater(&paths, &base, &pair.public, "0.4.0").apply(&Options::default(), false).await.unwrap();
    let mut pending = update::pending(&paths).unwrap();
    pending.started_at = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
    let mut state = crate::supervisor::State::default();
    let wanted = crate::supervisor::Services { node: true, ..Default::default() };
    let raised = crate::supervisor::watch_update(&paths, &Settings::default(), wanted, &mut state, &pending).await;
    let (source, level, text) = raised.expect("an alert");
    assert_eq!((source, level), ("update", nypi_core::inspector::AlertLevel::Error));
    assert!(state.exit, "the supervisor ends, to start again on the old binary");
    assert_eq!(installed(&paths), b"old");
    let notice = nypi_hub::notify::Notice { source, level, text: &text, task: None, shown_in: None };
    nypi_hub::notify::post(&paths, None, notice).await;
    let outbox = std::fs::read_to_string(paths.outbox()).unwrap();
    assert!(outbox.contains("did not stay healthy; rolled back to v0.4.0"), "{outbox}");
}

/// `keygen` writes an owner-only key; `sign` signs a manifest the release key verifies.
#[test]
fn keygen_and_sign_make_a_signature_the_release_key_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("nypi-release.key");
    let public = update::keygen(&key).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(update::keygen(&key).is_err(), "never overwritten");
    let manifest = dir.path().join("manifest.json");
    let mut file = std::fs::File::create(&manifest).unwrap();
    file.write_all(br#"{ "version": "0.4.1", "tag": "v0.4.1", "assets": {} }"#).unwrap();
    let signed = update::sign(&manifest, Some(&key)).unwrap();
    let signature = std::fs::read_to_string(signed).unwrap();
    let bytes = std::fs::read(&manifest).unwrap();
    assert_eq!(nypi_core::update::verify(&bytes, &signature, &public).unwrap().version, "0.4.1");
}

/// Decision D99: once an update is in place, what an earlier supervisor left running on the old
/// binary is stopped once, and the update's clock starts again; a supervisor that keeps being
/// started without getting to its rounds puts the old binary back after three starts.
#[tokio::test]
async fn a_new_binary_restarts_what_ran_the_old_one_and_a_supervisor_that_cannot_start_rolls_back() {
    let pair = nypi_core::update::generate().unwrap();
    let server = Server::default();
    let base = server.start().await;
    publish(&server, &pair, "0.4.1", &tarball(&[("nypi", stub("0.4.1"))]), None);
    let (_home, paths) = home();
    updater(&paths, &base, &pair.public, "0.4.0").apply(&Options::default(), false).await.unwrap();
    let mut pending = update::pending(&paths).unwrap();
    pending.started_at = "2026-01-01T00:00:00Z".into();
    update::save_pending(&paths, &pending);

    // A node an earlier supervisor started, on the old binary.
    let mut old = std::process::Command::new("sleep").arg("60").spawn().unwrap();
    let pid = old.id();
    std::thread::spawn(move || old.wait());
    let status = serde_json::json!({ "pid": pid, "connected": true, "address": "", "path": "direct", "since": "" });
    std::fs::create_dir_all(paths.node_status().parent().unwrap()).unwrap();
    std::fs::write(paths.node_status(), status.to_string()).unwrap();
    let mut state = crate::supervisor::State::default();
    assert!(crate::supervisor::restart_on_new_binary(&paths, &mut state).await);
    for _ in 0..50 {
        if !nypi_core::ledger::process_alive(pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(!nypi_core::ledger::process_alive(pid), "the old node was stopped");
    let pending = update::pending(&paths).unwrap();
    assert!(pending.restarted && pending.started_at != "2026-01-01T00:00:00Z", "its clock starts again");
    assert!(!crate::supervisor::restart_on_new_binary(&paths, &mut state).await, "once");

    for start in 1..=crate::supervisor::PENDING_STARTS {
        assert_eq!(crate::supervisor::guard_starts(&paths), None, "start {start} is the new binary's to take");
    }
    assert_eq!(update::pending(&paths).unwrap().supervisor_starts, crate::supervisor::PENDING_STARTS);
    let said = crate::supervisor::guard_starts(&paths).expect("put back");
    assert!(said.contains("rolled back to v0.4.0"), "{said}");
    assert_eq!(installed(&paths), b"old");
    assert!(update::pending(&paths).is_none());
    assert_eq!(crate::supervisor::guard_starts(&paths), None, "nothing pending: nothing to count");
}

/// Decision D99: an automatic update that fails is said once per version; a build without a key
/// checks an unsigned release only when asked to by hand.
#[tokio::test]
async fn a_failed_automatic_update_is_told_and_an_unsigned_check_needs_asking() {
    let pair = nypi_core::update::generate().unwrap();
    let server = Server::default();
    let base = server.start().await;
    publish(&server, &pair, "0.4.1", &tarball(&[("nypi", stub("0.4.1"))]), Some("00"));
    let (_home, paths) = home();
    let mut settings = Settings::default();
    settings.update.mode = UpdateMode::Auto;
    let run = async || {
        let told = Mutex::new(None);
        let updater = updater(&paths, &base, &pair.public, "0.4.0");
        update::background(
            &paths,
            &settings,
            &updater,
            || true,
            async |text: String| {
                *told.lock().unwrap() = Some(text);
            },
        )
        .await;
        told.into_inner().unwrap()
    };
    let told = run().await.expect("told");
    assert!(told.contains("automatic update failed") && told.contains("run nypi update"), "{told}");
    assert_eq!(installed(&paths), b"old");
    let mut state = update::state(&paths);
    state.last_check = None;
    update::save_state(&paths, &state);
    assert_eq!(run().await, None, "once per version");

    let unsigned =
        Updater::with(&paths, Source::loopback(&base).unwrap(), None, TARGET, Version::parse("0.4.0").unwrap());
    let refused = unsigned.check(false).await.unwrap_err();
    assert!(format!("{refused:#}").contains("no release key"), "{refused:#}");
    assert_eq!(unsigned.check(true).await.unwrap().latest.version, "0.4.1", "by hand, with --allow-unsigned");
}
