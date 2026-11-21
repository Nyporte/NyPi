//! How a machine reaches its hub (decision D94). On every connect it first tries the direct ways
//! all at once: the hub heard by mDNS on this network, and the hub's direct addresses it was last
//! told (those on a subnet it shares first, the last one that worked first of all). A direct way
//! wins when its handshake shows the pinned hub key, within 1.5 seconds. Then the entry, where the
//! same TLS runs inside the relay's WebSocket. A node linked through the relay keeps trying the
//! direct ways every minute and moves to one as soon as it works; calls under way are kept by
//! the node's journal and handed to the new link (decision D74).
//!
//! What mDNS and the address lists say is a hint only: the handshake decides. Among hubs that
//! show the key, the highest epoch wins, and one below the epoch this machine knows is refused.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use nypi_core::config::{Paths, Settings};
use tokio::sync::mpsc;

use crate::direct;
use crate::instance::Instance;
use crate::mdns::{self, Lan};
use crate::pairing::{self, Linked, Purpose, Way};

/// How long the direct ways are given before the entry is tried.
pub const DIRECT_WAIT: Duration = Duration::from_millis(1500);
/// How long the first hub to answer waits for another at a higher epoch.
const GRACE: Duration = Duration::from_millis(200);
/// How often a relayed node looks for a direct way.
pub const RETRY_DIRECT: Duration = Duration::from_secs(60);

/// What a machine has to look for its hub with.
#[derive(Clone)]
pub struct Reach {
    /// The local network, for mDNS; None to skip it.
    pub lan: Option<Arc<dyn Lan>>,
    pub direct_wait: Duration,
}

impl Reach {
    /// The real network: mDNS when the port can be opened.
    pub fn local() -> Reach {
        let lan = mdns::Multicast::open().ok().map(|lan| Arc::new(lan) as Arc<dyn Lan>);
        Reach { lan, direct_wait: DIRECT_WAIT }
    }

    pub fn on(lan: Option<Arc<dyn Lan>>) -> Reach {
        Reach { lan, direct_wait: DIRECT_WAIT }
    }
}

/// The best direct way to the hub `instance` names, and its epoch, or None when none answered
/// with the pinned key in time.
pub async fn best_direct(paths: &Paths, instance: &Instance, reach: &Reach) -> Option<(String, u64)> {
    let fingerprint = instance.hub_fingerprint.clone();
    let known = instance.epoch;
    let deadline = tokio::time::Instant::now() + reach.direct_wait;
    let (heard, mut hearing) = mpsc::unbounded_channel();
    if let Some(lan) = reach.lan.clone() {
        tokio::spawn(mdns::browse(lan, reach.direct_wait, heard));
    } else {
        drop(heard);
    }
    let mut tried: Vec<String> = Vec::new();
    let mut probes = FuturesUnordered::new();
    let probe = |address: String| {
        let (paths, fingerprint) = (paths.clone(), fingerprint.clone());
        async move {
            let way = Way::Direct(address.clone());
            let answered =
                tokio::time::timeout(DIRECT_WAIT, pairing::probe(&paths, &way, Some(&fingerprint), known)).await;
            (address, answered.ok().and_then(Result::ok).map(|(_, epoch)| epoch))
        }
    };
    for address in direct::nearest_first(&instance.direct, &direct::interfaces()) {
        tried.push(address.clone());
        probes.push(probe(address));
    }
    // Good enough to stop at once: a hub at an epoch above the one known, or at the highest one
    // mDNS told of. Otherwise the first answer gives the others a moment to show a higher one.
    let mut target = known + 1;
    let mut best: Option<(String, u64)> = None;
    let mut deadline = deadline;
    let mut listening = reach.lan.is_some();
    loop {
        if probes.is_empty() && !listening {
            break;
        }
        tokio::select! {
            Some((address, epoch)) = probes.next(), if !probes.is_empty() => {
                if let Some(epoch) = epoch.filter(|epoch| *epoch >= known)
                    && best.as_ref().is_none_or(|(_, held)| epoch > *held)
                {
                    if best.is_none() {
                        deadline = deadline.min(tokio::time::Instant::now() + GRACE);
                    }
                    best = Some((address, epoch));
                }
                if best.as_ref().is_some_and(|(_, epoch)| *epoch >= target) {
                    break;
                }
            }
            hub = hearing.recv(), if listening => match hub {
                Some(hub) if hub.fingerprint == fingerprint && hub.epoch >= known => {
                    target = target.max(hub.epoch);
                    for address in hub.addresses.iter().map(ToString::to_string) {
                        if !tried.contains(&address) {
                            tried.push(address.clone());
                            probes.push(probe(address));
                        }
                    }
                }
                Some(_) => {}
                None => listening = false,
            },
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
    best
}

/// What to say when no way to the hub answered and no entry is set (decision D97).
pub const NO_ENTRY: &str = "no hub answered on this network, and no entry is set: run this on the same network as \
                            the hub, or pass --entry <host:port>";

/// Link to the hub for `purpose`: the best direct way, else the entry. What the welcome says
/// (the hub's direct addresses, its epoch, the way that worked) is kept in `instance.json`.
pub async fn connect(paths: &Paths, settings: &Settings, purpose: Purpose, reach: &Reach) -> anyhow::Result<Linked> {
    let Some(instance) = Instance::load(paths).filter(|instance| !instance.is_hub()) else {
        anyhow::bail!("this machine is not a node of a hub: nypi node up joins one");
    };
    let mut ways = Vec::new();
    if let Some((address, _)) = best_direct(paths, &instance, reach).await {
        ways.push(Way::Direct(address));
    }
    ways.extend(settings.entry().map(Way::Relayed));
    anyhow::ensure!(!ways.is_empty(), NO_ENTRY);
    let linked = pairing::link(paths, &instance.hub_fingerprint, &ways, purpose, instance.epoch).await?;
    remember(paths, &linked);
    Ok(linked)
}

/// Keep what a welcome said: the hub's epoch, and its direct addresses with the one that worked
/// first.
pub fn remember(paths: &Paths, linked: &Linked) {
    remember_hub(paths, linked.epoch, &linked.direct, linked.way.is_direct().then(|| linked.way.label()));
}

/// Keep the hub's news of itself: its epoch and direct addresses, `worked` first.
pub fn remember_hub(paths: &Paths, epoch: u64, direct: &[String], worked: Option<String>) {
    let Some(mut instance) = Instance::load(paths).filter(|instance| !instance.is_hub()) else { return };
    let before = instance.clone();
    let mut list: Vec<String> = worked.into_iter().collect();
    let told = if direct.is_empty() { &instance.direct } else { direct };
    for address in told {
        if !list.contains(address) {
            list.push(address.clone());
        }
    }
    instance.direct = list;
    instance.epoch = instance.epoch.max(epoch);
    if instance != before {
        let _ = instance.save(paths);
    }
}

/// The ways a machine that knows no hub yet tries, in order (decisions D94, D97): the hub the
/// entry leads to, on this network if mDNS hears it with the same key, then through the entry.
/// When there is no entry, or it cannot be reached, the one hub heard on this network.
pub async fn ways_to_join(paths: &Paths, entry: Option<&str>, reach: &Reach) -> anyhow::Result<Vec<Way>> {
    let relayed = entry.map(|entry| Way::Relayed(entry.to_string()));
    let at_entry = relayed.clone().map(|relayed| {
        let paths = paths.clone();
        tokio::spawn(async move { pairing::probe(&paths, &relayed, None, 0).await })
    });
    let mut heard = Vec::new();
    if let Some(lan) = reach.lan.clone() {
        let (tell, mut hearing) = mpsc::unbounded_channel();
        mdns::browse(lan, reach.direct_wait, tell).await;
        while let Ok(hub) = hearing.try_recv() {
            heard.push(hub);
        }
    }
    heard.sort_by_key(|hub| std::cmp::Reverse(hub.epoch));
    let entry_hub = match at_entry {
        Some(probing) => probing.await.ok().and_then(Result::ok).map(|(fingerprint, _)| fingerprint),
        None => None,
    };
    let local = |fingerprint: &str| -> Vec<Way> {
        heard
            .iter()
            .filter(|hub| hub.fingerprint == fingerprint)
            .flat_map(|hub| hub.addresses.iter().map(|address| Way::Direct(address.to_string())))
            .collect()
    };
    match (entry_hub, relayed) {
        (Some(fingerprint), Some(relayed)) => {
            let mut ways = local(&fingerprint);
            ways.push(relayed);
            Ok(ways)
        }
        (_, relayed) => {
            let mut keys: Vec<&str> = heard.iter().map(|hub| hub.fingerprint.as_str()).collect();
            keys.sort();
            keys.dedup();
            let at = relayed.as_ref().map(Way::label);
            match (keys.as_slice(), at) {
                ([], Some(at)) => anyhow::bail!("no hub answered at {at} or on this network"),
                ([], None) => anyhow::bail!(NO_ENTRY),
                ([one], _) => Ok(local(one)),
                (_, Some(at)) => anyhow::bail!(
                    "{at} did not answer, and more than one hub is on this network: name the entry with --entry"
                ),
                (_, None) => anyhow::bail!("more than one hub is on this network: name the entry with --entry"),
            }
        }
    }
}

/// How a node is linked now, for `nypi status`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Serving {
    pub connected: bool,
    /// The address, or the entry.
    pub address: String,
    pub relayed: bool,
}

/// Serve `node` to the hub until the future is dropped: link, serve, move a relayed link to a
/// direct way when one works, and link again when the link is lost. `said` hears every change.
pub async fn serve_node(
    paths: &Paths,
    node: Arc<nypi_node::Node>,
    reach: &Reach,
    retry: Duration,
    said: impl Fn(&Serving, Option<&str>),
) {
    // What the hub says of itself between connects goes to `instance.json` too.
    let news_paths = paths.clone();
    node.on_hub_news(Arc::new(move |epoch, direct| remember_hub(&news_paths, epoch, &direct, None)));
    // A personal node keeps a standby copy of the hub's state (decision D95), on a link of its own.
    let personal = Instance::load(paths).is_some_and(|instance| instance.trust == crate::pairing::Trust::Personal);
    let _copying = personal.then(|| AbortOnDrop(tokio::spawn(keep_standby(paths.clone(), reach.clone()))));
    let mut wait = Duration::from_secs(1);
    loop {
        let settings = Settings::load(paths, &paths.home).unwrap_or_default();
        match connect(paths, &settings, Purpose::Node, reach).await {
            Ok(linked) => {
                wait = Duration::from_secs(1);
                let mut current = linked;
                crate::standby::saw_hub(paths);
                tell_pending(paths, reach).await;
                loop {
                    let serving_now =
                        Serving { connected: true, address: current.way.label(), relayed: !current.way.is_direct() };
                    said(&serving_now, None);
                    let serving = nypi_node::link::serve(node.clone(), current.read, current.write);
                    tokio::pin!(serving);
                    let relayed = serving_now.relayed;
                    let direct = async {
                        if !relayed {
                            return std::future::pending().await;
                        }
                        loop {
                            tokio::time::sleep(retry).await;
                            let Some(instance) = Instance::load(paths) else { continue };
                            let Some((address, _)) = best_direct(paths, &instance, reach).await else { continue };
                            let ways = [Way::Direct(address)];
                            let linked =
                                pairing::link(paths, &instance.hub_fingerprint, &ways, Purpose::Node, instance.epoch);
                            if let Ok(linked) = linked.await {
                                remember(paths, &linked);
                                return linked;
                            }
                        }
                    };
                    let seen = async {
                        loop {
                            tokio::time::sleep(Duration::from_secs(20)).await;
                            crate::standby::saw_hub(paths);
                        }
                    };
                    tokio::select! {
                        // It never ends.
                        _ = seen => unreachable!(),
                        ended = &mut serving => {
                            let why = match ended {
                                Ok(()) => "the hub closed the link".to_string(),
                                Err(error) => format!("the link was lost: {error}"),
                            };
                            said(&Serving { connected: false, ..serving_now }, Some(&why));
                            break;
                        }
                        moved = direct => current = moved,
                    }
                }
            }
            Err(error) => said(&Serving::default(), Some(&format!("{error:#}"))),
        }
        tokio::time::sleep(jittered(wait)).await;
        wait = (wait * 2).min(Duration::from_secs(30));
    }
}

/// Aborts a task when dropped: what a node runs beside its link ends with it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Keep the standby copy's link up, taking it up again when it is lost.
async fn keep_standby(paths: Paths, reach: Reach) {
    loop {
        let settings = Settings::load(&paths, &paths.home).unwrap_or_default();
        if let Ok(linked) = connect(&paths, &settings, Purpose::Standby, &reach).await {
            let _ = crate::standby::keep_copy(&paths, linked).await;
        }
        tokio::time::sleep(jittered(Duration::from_secs(30))).await;
    }
}

/// A note this machine keeps for the user (a hub that stepped down says where its state went),
/// told through the hub's outbox once it links, then forgotten.
async fn tell_pending(paths: &Paths, reach: &Reach) {
    use tokio::io::AsyncWriteExt;
    let Ok(text) = std::fs::read_to_string(paths.pending_notice()) else { return };
    let settings = Settings::load(paths, &paths.home).unwrap_or_default();
    let Ok(mut linked) = connect(paths, &settings, Purpose::Notice, reach).await else { return };
    let line = format!("{}\n", serde_json::json!({ "text": text.trim() }));
    if linked.write.write_all(line.as_bytes()).await.is_ok() && linked.write.flush().await.is_ok() {
        let _ = std::fs::remove_file(paths.pending_notice());
    }
}

/// `wait`, give or take a quarter: machines that lost the hub together do not come back together.
pub fn jittered(wait: Duration) -> Duration {
    let mut byte = [0u8; 1];
    let spread = match ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut byte) {
        Ok(()) => byte[0] as f64 / 255.0,
        Err(_) => 0.5,
    };
    wait.mul_f64(0.75 + spread * 0.5)
}
