//! Handing the hub over (decision D95). There is one hub at a time: its key moves with it, and
//! its epoch says which is the current one.
//!
//! A planned move (`nypi hub here` on a personal node, a `move` link): the hub stops taking new
//! work (the scheduler, idle passes, and goal reviews pause), lets the Principal's activation
//! finish for up to `hub.handoverWaitSeconds`, and stops what runs at its next tool boundary,
//! leaving it in the ledger as interrupted. It sends the last difference on top of the target's
//! standby copy at epoch N; when the target has applied it, the old hub becomes a node at N+1,
//! tells every linked node where the hub is now, stops its board, and only then says
//! `released`. Anything that fails before that leaves the old hub to start again at epoch N.
//!
//! Stepping down: a hub that learns of a higher epoch (the relay refused it, a node or mDNS said
//! so) stops at once, sets its state aside under `diverged/`, becomes a node at that epoch, and
//! tells the user, through the new hub when it next links, where its state went.

use super::*;
use crate::pairing::{Peer, PeerKind, Peers, Trust};
use crate::standby::{self, Said};

/// How the hub's work ended when it was handed over.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Another machine is the hub at `epoch`; this one is its node.
    Moved { epoch: u64 },
    /// The move did not happen: the hub starts again at its epoch.
    Resume { reason: String },
    /// Another hub holds `epoch`; this one's state is under `aside`.
    SteppedDown { epoch: u64, aside: PathBuf },
}

/// How the hub's service ended.
pub enum Ended {
    /// It was stopped.
    Stopped,
    /// It was handed over, or stepped down; `outcome` says how it turned out.
    Handover(Arc<Handover>),
}

#[derive(Default)]
pub struct Handover {
    /// The scheduler, idle passes, and goal reviews stop.
    pub(crate) pause: CancellationToken,
    /// All work stops.
    pub(crate) stop: CancellationToken,
    started: AtomicBool,
    ended: tokio::sync::Notify,
    has_ended: AtomicBool,
    higher: Mutex<Option<u64>>,
    outcome: Mutex<Option<Outcome>>,
    done: tokio::sync::Notify,
}

impl Handover {
    /// A move begins; false when one is under way already.
    pub(crate) fn begin(&self) -> bool {
        !self.started.swap(true, Ordering::SeqCst)
    }

    /// Another hub holds `epoch`: stop, and step down.
    pub(crate) fn step_down(&self, epoch: u64) {
        {
            let mut higher = self.higher.lock().unwrap();
            *higher = Some(higher.unwrap_or(0).max(epoch));
        }
        log(&format!("another hub holds epoch {epoch}: this one steps down"));
        self.stop.cancel();
    }

    pub(crate) fn stepping_down(&self) -> Option<u64> {
        *self.higher.lock().unwrap()
    }

    pub(crate) fn work_has_ended(&self) {
        self.has_ended.store(true, Ordering::SeqCst);
        self.ended.notify_waiters();
    }

    async fn work_ended(&self) {
        loop {
            let notified = self.ended.notified();
            if self.has_ended.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn finish(&self, outcome: Outcome) {
        *self.outcome.lock().unwrap() = Some(outcome);
        self.done.notify_waiters();
    }

    /// How the handover turned out, once it has.
    pub async fn outcome(&self) -> Outcome {
        loop {
            let notified = self.done.notified();
            if let Some(outcome) = self.outcome.lock().unwrap().clone() {
                return outcome;
            }
            notified.await;
        }
    }
}

/// This machine's own key in the register as a personal machine, so that it can link to the hub
/// as a node once another machine is the hub.
pub(crate) fn ensure_own_peer(paths: &Paths) {
    let Some(instance) = crate::instance::Instance::load(paths) else { return };
    let Ok(key) = nypi_node::identity::Identity::load_or_create(&paths.device_key()) else { return };
    let peers = Peers::open(paths);
    if peers.get(&key.fingerprint()).is_some() {
        return;
    }
    let me = Peer {
        name: instance.name.clone(),
        trust: Trust::Personal,
        instance_id: instance.instance_id.clone(),
        kind: PeerKind::Machine,
        system: crate::pairing::system(),
        ..Default::default()
    };
    if let Err(why) = peers.add(&key.fingerprint(), me) {
        log(&format!("the hub's own machine is not in the register: {why}"));
    }
}

/// Wait for the Principal's activation to end, for at most `wait`.
async fn principal_idle(paths: &Paths, wait: Duration) {
    let until = tokio::time::Instant::now() + wait;
    while tokio::time::Instant::now() < until {
        let status: Value = std::fs::read_to_string(paths.principal_status())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        if status["active"].is_null() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    log("the Principal's activation did not end in time; it is cut off and taken up again by the new hub");
}

/// Stop every piece of work, as a restart would, and wait until it has stopped.
async fn stop_work(remote: &Arc<remote::Remote>) {
    let handover = &remote.handover;
    handover.pause.cancel();
    let settings = nypi_core::config::Settings::load(&remote.paths, &remote.paths.home).unwrap_or_default();
    principal_idle(&remote.paths, Duration::from_secs(settings.hub.handover_wait_seconds)).await;
    nypi_core::runtime::handing_over(&remote.paths.home, true);
    let thread = remote.primary.thread();
    thread.send(json!({ "id": "handover", "type": "abort" }).to_string());
    for _ in 0..100 {
        if !thread.busy() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    handover.stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(30), handover.work_ended()).await;
}

/// The board stops with the hub: what it shows is the new hub's now.
fn stop_board(paths: &Paths) {
    let serving: Value = std::fs::read_to_string(paths.home.join("run").join("board.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    if let Some(pid) = serving["pid"].as_u64().map(|pid| pid as i32).filter(|pid| *pid > 1)
        && pid as u32 != std::process::id()
    {
        // SAFETY: kill only sends a signal to the process the board's own file names.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

/// A personal machine takes the hub's place (`move` link). Nothing on this side changes until
/// the target has applied the last difference; then this hub becomes its node.
pub(super) async fn move_out<R, W>(
    remote: &Arc<remote::Remote>,
    peer: &Peer,
    read: &mut BufReader<R>,
    write: &mut W,
    via: remote::Via,
) -> Result<(), String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let handover = remote.handover.clone();
    if !handover.begin() {
        return Err("a move of the hub is under way already".into());
    }
    let paths = &remote.paths;
    let heard = tokio::time::timeout(Duration::from_secs(30), standby::hear(read)).await;
    let Ok(Ok(Said::Have { manifest, direct })) = heard else {
        let reason = "the machine did not say what its copy has".to_string();
        stop_work(remote).await;
        handover.finish(Outcome::Resume { reason: reason.clone() });
        return Err(reason);
    };
    log(&format!("{} takes the hub's place; stopping work", peer.name));
    stop_work(remote).await;
    let failed = |reason: String| {
        nypi_core::runtime::handing_over(&paths.home, false);
        log(&format!("the move to {} did not happen ({reason}); the hub starts again", peer.name));
        handover.finish(Outcome::Resume { reason: reason.clone() });
        reason
    };
    ensure_own_peer(paths);
    let instance = crate::instance::Instance::load(paths).unwrap_or_default();
    let epoch = instance.epoch.max(1);
    let settings = nypi_core::config::Settings::load(paths, &paths.home).unwrap_or_default();
    let pieces = {
        let (paths, settings) = (paths.clone(), settings.clone());
        tokio::task::spawn_blocking(move || crate::state::difference(&paths, &settings, &manifest))
            .await
            .map_err(|error| failed(error.to_string()))?
    };
    let rate = standby::rate(via == remote::Via::Relayed, remote.relay_kbps.load(Ordering::SeqCst));
    standby::send_batch(write, epoch, true, pieces, rate).await.map_err(|error| failed(error.to_string()))?;
    match tokio::time::timeout(Duration::from_secs(600), standby::hear(read)).await {
        Ok(Ok(Said::Applied { .. })) => {}
        Ok(Ok(Said::Rejected { reason })) => {
            let _ = standby::say(write, &Said::Abandoned { reason: reason.clone() }).await;
            return Err(failed(format!("the copy did not check out: {reason}")));
        }
        _ => return Err(failed("the machine went away before it applied the copy".into())),
    }
    let next = epoch + 1;
    let node = crate::instance::Instance {
        mode: crate::instance::Mode::Node,
        epoch: next,
        trust: Trust::Personal,
        direct: direct.clone(),
        down: false,
        ..instance
    };
    node.save(paths).map_err(|error| failed(error.to_string()))?;
    remote.nodes.tell_paired(next, &direct);
    // Told where the hub is now, they link to it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    remote.live.close_all();
    stop_board(paths);
    nypi_core::runtime::handing_over(&paths.home, false);
    let _ = standby::say(write, &Said::Released { epoch: next }).await;
    log(&format!("{} is the hub now, at epoch {next}; this machine is its node", peer.name));
    handover.finish(Outcome::Moved { epoch: next });
    Ok(())
}

/// Keep a personal node's standby copy current (`standby` link) until the link ends.
pub(super) async fn serve_copy<R, W>(
    remote: &Arc<remote::Remote>,
    read: &mut BufReader<R>,
    write: &mut W,
    via: remote::Via,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let paths = &remote.paths;
    let Ok(Ok(Said::Have { mut manifest, .. })) =
        tokio::time::timeout(Duration::from_secs(30), standby::hear(read)).await
    else {
        return;
    };
    let relayed = via == remote::Via::Relayed;
    let settings = nypi_core::config::Settings::load(paths, &paths.home).unwrap_or_default();
    let mut schedule = standby::Schedule::new(settings.hub.standby_minutes, relayed);
    let decisions_size = || std::fs::metadata(paths.decisions()).map(|meta| meta.len()).unwrap_or(0);
    let mut decisions = decisions_size();
    let mut cache = crate::state::StateCache::default();
    loop {
        if remote.handover.stop.is_cancelled() {
            return;
        }
        let now = tokio::time::Instant::now();
        let decided = decisions_size() != decisions;
        if schedule.due(now, decided) {
            decisions = decisions_size();
            let epoch = crate::instance::Instance::load(paths).map_or(1, |instance| instance.epoch.max(1));
            let settings = nypi_core::config::Settings::load(paths, &paths.home).unwrap_or_default();
            // What the hub knew of each file at the last batch goes in and comes back (D99).
            let pieces = {
                let (paths, have, mut held) = (paths.clone(), manifest.clone(), std::mem::take(&mut cache));
                let made = tokio::task::spawn_blocking(move || {
                    let pieces = crate::state::difference_cached(&paths, &settings, &have, &mut held);
                    (pieces, held)
                });
                match made.await {
                    Ok((pieces, held)) => {
                        cache = held;
                        pieces
                    }
                    Err(_) => return,
                }
            };
            let rate = standby::rate(relayed, remote.relay_kbps.load(Ordering::SeqCst));
            if standby::send_batch(write, epoch, false, pieces, rate).await.is_err() {
                return;
            }
            match standby::hear(read).await {
                Ok(Said::Applied { manifest: now }) => manifest = now,
                // Not applied: what differs from an empty copy goes next time.
                Ok(Said::Rejected { .. }) => manifest = Default::default(),
                _ => return,
            }
            schedule.last = Some(tokio::time::Instant::now());
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            _ = remote.handover.stop.cancelled() => return,
        }
    }
}

/// After the work stopped for a step-down: set the state aside, become a node at the higher
/// epoch, and keep a note for the user, which goes through the new hub when this machine links.
pub(super) fn step_down(paths: &Paths, higher: u64) -> Outcome {
    let settings = nypi_core::config::Settings::load(paths, &paths.home).unwrap_or_default();
    let instance = crate::instance::Instance::load(paths).unwrap_or_default();
    let aside = match crate::state::set_aside(paths, &settings, instance.epoch) {
        Ok(aside) => aside,
        Err(error) => {
            log(&format!("the hub's state could not be set aside ({error:#}); it stays where it is"));
            paths.home.clone()
        }
    };
    let node = crate::instance::Instance {
        mode: crate::instance::Mode::Node,
        epoch: higher,
        trust: Trust::Personal,
        direct: Vec::new(),
        down: false,
        ..instance.clone()
    };
    if let Err(error) = node.save(paths) {
        log(&format!("instance.json was not written: {error}"));
    }
    let note = format!(
        "{} was the hub at epoch {} and another machine holds the hub at epoch {higher}, so it stepped down and is a \
         node now. What it held is kept there under {}; nothing was merged.",
        instance.name,
        instance.epoch,
        aside.display()
    );
    let _ = std::fs::create_dir_all(paths.home.join("run"));
    let _ = std::fs::write(paths.pending_notice(), note);
    log(&format!("stepped down: a node at epoch {higher}; the state is under {}", aside.display()));
    Outcome::SteppedDown { epoch: higher, aside }
}
