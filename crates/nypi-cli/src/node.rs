//! `nypi node run` (decisions D66, D93, D94): this machine as a node of its hub. It links to the
//! hub the machine joined (`instance.json`), directly when it can and through the entry's relay
//! otherwise (nypi-hub/src/reach.rs), says what it offers, and carries out the calls it is sent
//! under this machine's own rules. It keeps no conversation and no model key. The link is taken
//! up again whenever it is lost.

use std::sync::Arc;

use nypi_core::config::Paths;
use nypi_hub::instance::Instance;
use nypi_hub::reach::{RETRY_DIRECT, Reach, Serving, serve_node};
use serde::{Deserialize, Serialize};

async fn stopped() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async {
            match terminate.as_mut() {
                Some(signal) => { signal.recv().await; }
                None => std::future::pending().await,
            }
        } => {}
    }
}

/// How the node is linked, as `run/node.json` keeps it for `nypi status`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NodeStatus {
    pub pid: u32,
    pub connected: bool,
    /// Where the link goes: an address of the hub's, or the entry.
    pub address: String,
    /// `direct` or `relayed` (decision D94).
    pub path: String,
    pub since: String,
    /// Calls it is carrying out, written every few seconds: a machine with calls under way is
    /// not idle, and is not updated in the background (decision D98).
    #[serde(default)]
    pub running: usize,
}

fn write_status(paths: &Paths, status: &NodeStatus) {
    let file = paths.node_status();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_vec_pretty(status) {
        let _ = std::fs::write(file, text);
    }
}

pub fn read_status(paths: &Paths) -> Option<NodeStatus> {
    serde_json::from_str(&std::fs::read_to_string(paths.node_status()).ok()?).ok()
}

/// Serve this machine's node to its hub until stopped: directly when it can, else through the
/// entry, moving to a direct way when one works (decision D94).
pub async fn run(paths: &Paths) -> anyhow::Result<i32> {
    let Some(instance) = Instance::load(paths).filter(|instance| !instance.is_hub()) else {
        anyhow::bail!("this machine is not a node of a hub: nypi node up joins one");
    };
    let journal = paths.home.join("node");
    std::fs::create_dir_all(&journal)?;
    // The hub knows this machine by the name it was admitted under.
    let node = Arc::new(
        nypi_core::device::this_computer(paths, Some(&instance.name))?.with_journal(journal.join("journal.jsonl")),
    );
    // The hub may ask this machine to update (decision D98): it checks the release itself, against
    // its own release key, and starts again on it once it has answered.
    let settings = nypi_core::config::Settings::load(paths, &paths.home).unwrap_or_default();
    node.set_relay(settings.relay.enabled);
    let updating = paths.clone();
    node.on_update(Arc::new(move |version| {
        let paths = updating.clone();
        Box::pin(async move { crate::update::update_for_hub(&paths, version).await })
    }));
    let offered: Vec<String> = node.declarations().into_iter().map(|tool| tool.name).collect();
    println!("Node {} ({}): offers {}.", instance.name, instance.trust.label(), offered.join(", "));
    let reach = Reach::local();
    let told_away = std::sync::atomic::AtomicBool::new(false);
    let said = |serving: &Serving, why: Option<&str>| {
        let status = NodeStatus {
            pid: std::process::id(),
            connected: serving.connected,
            address: serving.address.clone(),
            path: if serving.connected {
                (if serving.relayed { "relayed" } else { "direct" }).into()
            } else {
                String::new()
            },
            since: chrono::Utc::now().to_rfc3339(),
            running: 0,
        };
        write_status(paths, &status);
        match (serving.connected, why) {
            (true, _) => {
                told_away.store(false, std::sync::atomic::Ordering::Relaxed);
                println!("Linked to the hub, {} ({}).", status.path, serving.address);
            }
            (false, Some(why)) if !told_away.swap(true, std::sync::atomic::Ordering::Relaxed) => {
                println!("The hub is not reachable ({why}); trying again until it is.")
            }
            _ => {}
        }
    };
    let counting = node.clone();
    let count_calls = async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let running = counting.report().running.len();
            if let Some(status) = read_status(paths).filter(|status| status.running != running) {
                write_status(paths, &NodeStatus { running, ..status });
            }
        }
    };
    tokio::select! {
        _ = serve_node(paths, node, &reach, RETRY_DIRECT, said) => {}
        _ = count_calls => {}
        _ = stopped() => {}
    }
    if let Some(status) = read_status(paths) {
        write_status(paths, &NodeStatus { connected: false, ..status });
    }
    Ok(0)
}
