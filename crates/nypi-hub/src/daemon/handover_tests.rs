//! Moving the hub (decision D95), over loopback with several NyPi homes and a relay: a planned
//! move carries the state and leaves the old hub a node; a failed one changes nothing; a forced
//! takeover makes the old hub step down when it comes back, by the relay's word or a node's;
//! and the relay holds to one hub by its epochs.

use nypi_ai::providers::faux::FauxProvider;
use nypi_core::config::RelaySettings;
use nypi_node::identity::Identity;
use nypi_vault::secrets::{Handle, SecretStore};

use super::network_tests::{home_with, node_instance, register, serve, start_relay, until};
use super::tests::routed;
use super::*;
use crate::direct::Directions;
use crate::instance::{Instance, Mode};
use crate::pairing::{self, Purpose, Trust, Way};
use crate::reach::Reach;
use crate::standby::{self, Said};

const SECRET: &str = "sk-moved-0123456789abcdef";

/// A hub's service on `paths`, linked to the relay at `entry`: its end, its stop, its register of
/// nodes, and its direct address.
async fn run_hub(
    paths: &Paths,
    entry: Option<&str>,
) -> (tokio::task::JoinHandle<Ended>, CancellationToken, Arc<crate::nodes::Nodes>, String, tempfile::TempDir) {
    let work = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let nodes = crate::nodes::Nodes::open(paths);
    let shutdown = CancellationToken::new();
    let faux = FauxProvider::new(vec![]);
    let socket = paths.home.join("d.sock");
    let _ = std::fs::remove_file(&socket);
    let network = Network {
        listeners: vec![listener],
        directions: Directions::fixed(vec![address.clone()], vec![address.clone()]),
        lan: None,
        entry: entry.map(str::to_string),
        follow_entry: false,
        public_address: None,
        port_mapping: false,
        board: None,
    };
    let daemon = tokio::spawn(serve_hub(
        routed(&paths.home, work.path(), &faux, &nodes),
        UnixListener::bind(&socket).unwrap(),
        shutdown.clone(),
        None,
        Some(nodes.clone()),
        network,
    ));
    (daemon, shutdown, nodes, address, work)
}

/// A hub home with things worth carrying: a session, a vault note, goals, the ledger, and a
/// secret, with standby copies sent often.
fn hub_home(entry: &str) -> (tempfile::TempDir, Paths) {
    let settings = format!(
        r#"{{ "sandbox": "off", "envFiles": [], "entry": "{entry}", "hub": {{ "standbyMinutes": 0.005, "handoverWaitSeconds": 1, "forceAfterSeconds": 0 }} }}"#
    );
    let (home, paths) = home_with(&settings);
    let mut instance = Instance::new_hub(&paths, "mini").unwrap();
    instance.epoch = 1;
    instance.save(&paths).unwrap();
    let write = |path: &str, text: &str| {
        let file = paths.home.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    };
    write("chat/turns.jsonl", "{\"turn\":1}\n");
    write("vault/10-projects/site/plan.md", "# Ship the site\n");
    write("goals.md", "- ship the site\n");
    write("ledger.jsonl", "{\"event\":\"created\"}\n");
    let store = SecretStore::open(&paths.home).unwrap();
    store.set(&Handle::parse("cred://shop/key").unwrap(), SECRET).unwrap();
    (home, paths)
}

/// Join `name` to the hub at `address` with an invite as `trust`.
async fn joined(hub: &Paths, address: &str, entry: &str, name: &str, trust: Trust) -> (tempfile::TempDir, Paths) {
    let (home, paths) = home_with(&format!(r#"{{ "sandbox": "off", "envFiles": [], "entry": "{entry}" }}"#));
    let code = pairing::invite(hub, name, trust, chrono::Duration::hours(1)).unwrap();
    let ways = [Way::Direct(address.to_string())];
    let joined = pairing::join(&paths, &ways, name, Some(&code), &format!("i-{name}"), |_| {}).await.unwrap();
    node_instance(&paths, &joined);
    (home, paths)
}

/// The standby copy has the hub's goals, by its manifest (written once the batch is applied).
fn has_copy(paths: &Paths) -> bool {
    crate::state::Standby::at(paths).manifest().files.contains_key("goals.md")
        && read(paths, "standby/home/goals.md") == "- ship the site\n"
}

fn read(paths: &Paths, path: &str) -> String {
    std::fs::read_to_string(paths.home.join(path)).unwrap_or_default()
}

/// Every file under `dir`, read as text.
fn all_text(dir: &Path) -> String {
    let mut text = String::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return text };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            text.push_str(&all_text(&path));
        } else if let Ok(bytes) = std::fs::read(&path) {
            text.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    text
}

/// A planned move: the copy is kept current on the personal node and never sent to an
/// exec-only one; `nypi hub here` carries sessions, the vault, the ledger, goals, the secret
/// store with its key, and the hub key; the old hub stops its work before the last difference and
/// is a node at epoch 2 after; the new hub registers at the relay at epoch 2; and the other
/// machines reach it without joining again.
#[tokio::test]
async fn a_planned_move_carries_the_state_and_leaves_one_hub() {
    let (relay, entry, _relay_home) = start_relay(RelaySettings { trust_proxy: false, ..Default::default() }).await;
    let (_mini_home, mini) = hub_home(&entry);
    let fingerprint = Instance::load(&mini).unwrap().hub_fingerprint;
    let (old, _stop_old, old_nodes, address, _work) = run_hub(&mini, Some(&entry)).await;
    until("the hub registered at epoch 1", || relay.has_hub() && relay.state().epoch == 1).await;
    let (_laptop_home, laptop) = joined(&mini, &address, &entry, "laptop", Trust::Personal).await;
    let (_vps_home, vps) = joined(&mini, &address, &entry, "vps", Trust::ExecOnly).await;
    let reach = Reach::on(None);
    serve(&laptop, reach.clone());
    serve(&vps, reach.clone());
    until("the laptop's standby copy", || has_copy(&laptop)).await;
    until("the vps linked", || old_nodes.get("vps").is_some()).await;
    assert!(!vps.standby().exists(), "an exec-only node never gets a copy");
    let asked = pairing::link(&vps, &fingerprint, &[Way::Direct(address.clone())], Purpose::Standby, 1).await;
    assert!(asked.unwrap_err().to_string().contains("exec-only"));

    // Something new after the last copy: it comes with the last difference.
    std::fs::write(mini.home.join("goals.md"), "- ship the site\n- move the hub\n").unwrap();
    let laptop_settings = nypi_core::config::Settings::load(&laptop, &laptop.home).unwrap();
    let moved = standby::take_over(&laptop, &laptop_settings, &reach, vec!["192.0.2.10:7681".into()]).await.unwrap();
    assert!(old.is_finished(), "the old hub stopped its work before this machine became the hub");
    let Ended::Handover(handover) = old.await.unwrap() else { panic!("the old hub was not handed over") };
    assert_eq!(handover.outcome().await, Outcome::Moved { epoch: 2 });
    assert_eq!((moved.mode, moved.epoch, moved.name.as_str()), (Mode::Hub, 2, "laptop"));
    let old_instance = Instance::load(&mini).unwrap();
    assert_eq!((old_instance.mode, old_instance.epoch, old_instance.trust), (Mode::Node, 2, Trust::Personal));
    assert_eq!(old_instance.direct, ["192.0.2.10:7681"], "told where the hub is now");

    // What was carried.
    assert_eq!(read(&laptop, "goals.md"), "- ship the site\n- move the hub\n");
    assert_eq!(read(&laptop, "vault/10-projects/site/plan.md"), "# Ship the site\n");
    assert_eq!(read(&laptop, "chat/turns.jsonl"), "{\"turn\":1}\n");
    let sessions = |paths: &Paths| {
        crate::state::carried(paths, &nypi_core::config::Settings::default())
            .into_iter()
            .filter(|file| file.path.starts_with("sessions/"))
            .map(|file| (file.path.clone(), std::fs::read(&file.source).unwrap()))
            .collect::<Vec<_>>()
    };
    assert!(!sessions(&mini).is_empty() && sessions(&laptop) == sessions(&mini), "the conversation's session");
    assert_eq!(read(&laptop, "ledger.jsonl"), "{\"event\":\"created\"}\n");
    assert_eq!(Identity::load_or_create(&laptop.hub_key()).unwrap().fingerprint(), fingerprint, "the same hub key");
    let store = SecretStore::open(&laptop.home).unwrap();
    assert_eq!(store.get(&Handle::parse("cred://shop/key").unwrap()).unwrap().unwrap().expose(), SECRET);
    let key = nypi_vault::secrets::key_to_hex(&nypi_vault::secrets::master_key(&laptop.home).unwrap());
    for home in [&mini, &laptop] {
        for folder in ["logs", "audit", "run"] {
            assert!(!all_text(&home.home.join(folder)).contains(&key), "the master key in {folder}");
        }
    }
    assert!(!laptop.standby().exists(), "the copy became the home");

    // The new hub starts (as the supervisor would), registers at epoch 2, and the others link to
    // it without joining again: the vps through the entry, the old hub as a node.
    let (_new, _stop_new, new_nodes, _new_address, _new_work) = run_hub(&laptop, Some(&entry)).await;
    until("the new hub registered at epoch 2", || relay.has_hub() && relay.state().epoch == 2).await;
    until("the vps linked to the new hub", || new_nodes.get("vps").is_some()).await;
    serve(&mini, reach.clone());
    until("the old hub linked as a node", || new_nodes.get("mini").is_some()).await;
}

/// A move that fails before `released`: the target refuses the last difference. The old hub
/// starts again at epoch 1 (the relay takes it), and the target is unchanged.
#[tokio::test]
async fn a_move_that_fails_changes_nothing() {
    let (relay, entry, _relay_home) = start_relay(RelaySettings { trust_proxy: false, ..Default::default() }).await;
    let (_mini_home, mini) = hub_home(&entry);
    let fingerprint = Instance::load(&mini).unwrap().hub_fingerprint;
    let (old, _stop, _nodes, address, _work) = run_hub(&mini, Some(&entry)).await;
    until("registered", || relay.has_hub()).await;
    let (_laptop_home, laptop) = joined(&mini, &address, &entry, "laptop", Trust::Personal).await;
    let before = Instance::load(&laptop).unwrap();
    let mut linked =
        pairing::link(&laptop, &fingerprint, &[Way::Direct(address.clone())], Purpose::Move, 1).await.unwrap();
    standby::say(&mut linked.write, &Said::Have { manifest: Default::default(), direct: Vec::new() }).await.unwrap();
    let Said::Batch { count, last, .. } = standby::hear(&mut linked.read).await.unwrap() else { panic!("no batch") };
    assert!(last);
    standby::read_batch(&mut linked.read, count).await.unwrap();
    standby::say(&mut linked.write, &Said::Rejected { reason: "a test refuses it".into() }).await.unwrap();
    assert!(matches!(standby::hear(&mut linked.read).await.unwrap(), Said::Abandoned { .. }));
    let Ended::Handover(handover) = old.await.unwrap() else { panic!("not handed over") };
    assert!(matches!(handover.outcome().await, Outcome::Resume { .. }));
    let instance = Instance::load(&mini).unwrap();
    assert_eq!((instance.mode, instance.epoch), (Mode::Hub, 1), "still the hub at epoch 1");
    assert_eq!(Instance::load(&laptop).unwrap(), before, "the target unchanged");
    // Started again, as the supervisor would: the relay takes it at the same epoch.
    let (_again, _stop_again, _nodes_again, _a, _w) = run_hub(&mini, Some(&entry)).await;
    until("registered again", || relay.has_hub()).await;
    assert_eq!(relay.state().epoch, 1);
}

/// A forced takeover from the standby copy, then the old hub coming back: the relay refuses it,
/// it steps down with its state kept under `diverged/`, links to the new hub as a node, and tells
/// the user there where its state went.
#[tokio::test]
async fn a_forced_takeover_makes_the_old_hub_step_down_when_it_comes_back() {
    let (relay, entry, _relay_home) = start_relay(RelaySettings { trust_proxy: false, ..Default::default() }).await;
    let (_mini_home, mini) = hub_home(&entry);
    let (old, stop_old, _nodes, address, _work) = run_hub(&mini, Some(&entry)).await;
    until("registered", || relay.has_hub()).await;
    let (_laptop_home, laptop) = joined(&mini, &address, &entry, "laptop", Trust::Personal).await;
    let reach = Reach::on(None);
    serve(&laptop, reach.clone());
    until("the standby copy", || has_copy(&laptop)).await;
    // The hub goes away; something it did after the last copy stays with it.
    stop_old.cancel();
    assert!(matches!(old.await.unwrap(), Ended::Stopped));
    std::fs::write(mini.home.join("goals.md"), "- written after the copy\n").unwrap();
    let mut settings = nypi_core::config::Settings::load(&laptop, &laptop.home).unwrap();
    settings.hub.force_after_seconds = 0;
    assert_eq!(standby::force_refused(&laptop, &settings, &reach).await, None);
    let forced = standby::force(&laptop).unwrap();
    assert_eq!((forced.mode, forced.epoch), (Mode::Hub, 2));
    let (_new, _stop_new, new_nodes, _a, _w) = run_hub(&laptop, Some(&entry)).await;
    until("the new hub at epoch 2", || relay.state().epoch == 2 && relay.has_hub()).await;

    // The old hub comes back at epoch 1: refused, it steps down.
    let (back, _stop_back, _n, _b, _w2) = run_hub(&mini, Some(&entry)).await;
    let Ended::Handover(handover) = tokio::time::timeout(Duration::from_secs(20), back).await.unwrap().unwrap() else {
        panic!("it did not step down")
    };
    let Outcome::SteppedDown { epoch, aside } = handover.outcome().await else { panic!("not stepped down") };
    assert_eq!(epoch, 2);
    assert_eq!(
        std::fs::read_to_string(aside.join("goals.md")).unwrap(),
        "- written after the copy\n",
        "kept, not lost"
    );
    assert!(aside.starts_with(mini.diverged()));
    let instance = Instance::load(&mini).unwrap();
    assert_eq!((instance.mode, instance.epoch), (Mode::Node, 2));
    assert_eq!(relay.state().epoch, 2, "the relay still holds the new hub");

    // As a node of the new hub, it says where its state went.
    serve(&mini, reach);
    until("the old hub linked as a node", || new_nodes.get("mini").is_some()).await;
    until("the user told through the new hub", || {
        std::fs::read_to_string(laptop.outbox()).unwrap_or_default().contains("stepped down")
    })
    .await;
    assert!(!mini.pending_notice().exists());
}

/// With the relay out of reach, an old hub learns of the higher epoch from a node that knows it,
/// and steps down.
#[tokio::test]
async fn an_old_hub_learns_of_the_higher_epoch_from_a_node() {
    let (_mini_home, mini) = hub_home("http://127.0.0.1:9");
    let fingerprint = Instance::load(&mini).unwrap().hub_fingerprint;
    let (old, _stop, _nodes, address, _work) = run_hub(&mini, None).await;
    let (_node_home, node) = home_with(r#"{ "envFiles": [] }"#);
    let probed = pairing::probe(&node, &Way::Direct(address), Some(&fingerprint), 2).await;
    assert!(probed.unwrap_err().to_string().contains("replaced by one at epoch 2"));
    let Ended::Handover(handover) = tokio::time::timeout(Duration::from_secs(20), old).await.unwrap().unwrap() else {
        panic!("it did not step down")
    };
    assert!(matches!(handover.outcome().await, Outcome::SteppedDown { epoch: 2, .. }));
    assert_eq!(Instance::load(&mini).unwrap().epoch, 2);
}

/// The relay takes a higher epoch and closes the old control connection; it refuses a lower
/// epoch, and the same epoch from another instance, and says which epoch holds the entry.
#[tokio::test]
async fn the_relay_holds_one_hub_by_its_epoch() {
    let key = Identity::generate().unwrap();
    let (relay, entry, relay_home) = start_relay(RelaySettings::default()).await;
    let (first, mut first_control, _keep) = register(&entry, &key, 2, "i-a", &[]).await;
    assert!(matches!(first, crate::relay::Control::Registered { .. }), "{first:?}");
    for (epoch, instance) in [(1, "i-b"), (2, "i-b")] {
        let (refused, ..) = register(&entry, &key, epoch, instance, &[]).await;
        assert!(
            matches!(&refused, crate::relay::Control::Refused { epoch: Some(2), reason } if reason.contains("epoch 2")),
            "{epoch} {instance}: {refused:?}"
        );
    }
    let (again, ..) = register(&entry, &key, 2, "i-a", &[]).await;
    assert!(matches!(again, crate::relay::Control::Registered { .. }), "the same instance comes back");
    let (higher, _control, _keep_higher) = register(&entry, &key, 3, "i-c", &[]).await;
    assert!(matches!(higher, crate::relay::Control::Registered { .. }));
    assert_eq!((relay.state().epoch, relay.state().instance_id.as_str()), (3, "i-c"));
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match first_control.next().await {
                Ok(Some(crate::ws::Message::Close) | None) | Err(_) => return,
                Ok(Some(_)) => {}
            }
        }
    });
    assert!(closed.await.is_ok(), "the hub at the lower epoch lost its control connection");
    let kept: crate::relay::State =
        serde_json::from_str(&std::fs::read_to_string(relay_home.path().join("relay/state.json")).unwrap()).unwrap();
    assert_eq!((kept.epoch, kept.instance_id.as_str()), (3, "i-c"), "kept across restarts");
}
