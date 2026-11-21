//! The daemon (decisions D64, D66, and D79): Lucia as one process that is always there, the hub.
//! It holds her one conversation (primary.rs), open for as long as it runs. Interfaces reach it
//! over a Unix socket in NyPi's run folder, speaking the RPC protocol (rpc.rs) and saying where
//! their owner is (a folder, and the device it is on): what they ask for is done there. Every
//! interface sees every record of the conversation, and one that leaves does not stop its work.
//! The supervisor starts the daemon and starts it again when it stops or hangs; the heartbeat
//! file is how it tells.
//!
//! What every way of talking to Lucia needs is done here once, per thread, and not by each
//! interface (decision D67, part 2): a run that ends or waits for an approval with nobody there
//! goes to the user through the outbox (principle B6); a long run's result, a run that has gone
//! quiet, and an approval nobody answers go to the notification channels as well.
//!
//! Where no daemon runs, a terminal runs the same thing inside its own process (`Local`): one
//! thread, reached over a pipe instead of the socket, by the same interface code (client.rs).
//!
//! Whoever can connect speaks as the user: they can give tasks and answer approvals. So the
//! agent's own tools must not. The tool sandbox denies connecting to sockets in the run folder,
//! the guard refuses `nypi send` and `nypi attach` in a command, and the daemon turns away any
//! peer that is a process it started itself.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use nypi_core::config::Paths;
use nypi_core::inspector::{Alert, AlertLevel, Inspector};
use nypi_core::ledger::{TaskId, process_alive};
use nypi_core::place::DeviceFolder;
use nypi_core::pulse::{PulseBus, PulseKind, PulsePayload};
use nypi_core::runtime::{Job, Runtime};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use nypi_hub_proto::{AmbientFocus, ToHub};

use crate::bridge::{self, BridgeRequest};
use crate::notify::{self, Notice, Notifier, Progress, STALL_CHECK_INTERVAL, format_duration};

mod entry;
mod handover;
mod heartbeat;
mod local;
mod peer;
mod primary;
mod pump;
mod remote;
mod threads;

pub use handover::{Ended, Handover, Outcome};
pub use remote::{LinkRecord, Via, links_up};

pub use heartbeat::*;
pub use local::*;
pub(crate) use peer::*;
pub(crate) use primary::*;
pub use pump::*;
pub(crate) use threads::*;

/// How long a new connection may take to say where it is before it is taken to be at the hub's
/// own folder.
const ATTACH_WAIT: Duration = Duration::from_secs(3);

/// Set by the daemon; a hub inside a terminal's process keeps its stdout for the conversation.
static LOGGING: AtomicBool = AtomicBool::new(false);

/// One timestamped line on stdout, which the supervisor sends to `logs/hub.log`. Never a
/// message's text: the session and the audit log hold those.
pub fn log(line: &str) {
    if LOGGING.load(Ordering::Relaxed) {
        println!("{} {line}", Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    }
}

async fn write_record(write: &mut (impl AsyncWrite + Unpin), record: &str) -> std::io::Result<()> {
    write.write_all(record.as_bytes()).await?;
    write.write_all(b"\n").await
}

/// One connection to the daemon's socket, until it leaves. A process the daemon started itself
/// is turned away. The first line may say where the interface is
/// (`{"type":"attach","cwd":...,"answers":...}`, nypi-hub-proto); without one its focus is the
/// hub's own folder.
async fn client(primary: Arc<PrimaryStream>, stream: UnixStream, live: Arc<remote::LivePeers>, paths: Paths) {
    let peer = peer_of(&stream);
    let from_a_tool = started_by_this_process(&stream).await;
    let (read, mut write) = stream.into_split();
    if from_a_tool {
        // A process the agent started is asking to speak as the user.
        let pid = peer.unwrap_or_default();
        let inspector = primary.thread().hub.inspector.clone();
        log(&format!("refused a connection from pid {pid}: started by this hub"));
        inspector.record("hub", json!({ "refusedPeer": pid }));
        let message = format!("A program Lucia started (pid {pid}) tried to send her messages as you; it was refused.");
        inspector.alert(Alert::new(AlertLevel::Warning, message));
        let refused = json!({ "type": "hub_refused", "reason": "only the user talks to the hub, not its own tools" });
        let _ = write_record(&mut write, &refused.to_string()).await;
        return;
    }
    let mut lines = BufReader::new(read).lines();
    let first = match tokio::time::timeout(ATTACH_WAIT, lines.next_line()).await {
        Ok(Ok(Some(line))) => Some(line),
        Ok(_) => return,
        Err(_) => None,
    };
    // The owner unpaired a device: its links close now (decision D83). Only this socket asks.
    let peers_changed = first
        .as_deref()
        .and_then(|line| serde_json::from_str::<Value>(line).ok())
        .is_some_and(|line| matches!(ToHub::read(&line), Some(ToHub::PeersChanged)));
    if peers_changed {
        let closed = live.close_unpaired(&crate::pairing::Peers::open(&paths));
        let answer = json!({ "type": "peers_checked", "closed": closed });
        let _ = write_record(&mut write, &answer.to_string()).await;
        return;
    }
    // One of the hub's own commands typed on the hub, that needs the daemon's register of nodes
    // (`nypi update --node`, `--all`; decision D98). This socket is the user's.
    let command = first.as_deref().and_then(|line| serde_json::from_str::<Value>(line).ok());
    if let Some(crate::protocol::HubCommand::Admin { argv, input }) = command.as_ref().map(crate::protocol::parse) {
        let (output, code) = match crate::protocol::admin_allowed(&argv, input.is_some()) {
            Ok(()) => {
                let context = crate::protocol::AdminContext { nodes: primary.nodes() };
                crate::protocol::run_admin(&paths, argv, input, context).await
            }
            Err(why) => (why, 2),
        };
        let result = json!({ "type": "admin_result", "output": output, "code": code });
        let _ = write_record(&mut write, &result.to_string()).await;
        return;
    }
    attach_from(primary, first, lines, write, None, None).await;
}

/// An interface from its first line on (read already): where it says it is, then the
/// conversation. `own_device` is the device the interface is on, when it came over the network
/// (decision D80): a folder it names is a folder there, as far as that device has a node
/// connected. `stop` ends the link from outside (the device was unpaired).
async fn attach_from<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    primary: Arc<PrimaryStream>,
    first: Option<String>,
    lines: tokio::io::Lines<BufReader<R>>,
    mut write: W,
    own_device: Option<String>,
    stop: Option<CancellationToken>,
) {
    let attach = first.as_deref().and_then(|line| serde_json::from_str::<Value>(line).ok()).and_then(|line| {
        match ToHub::read(&line) {
            Some(attach @ ToHub::Attach { .. }) => Some(attach),
            _ => None,
        }
    });
    let (cwd, device, answers, role) = match &attach {
        Some(ToHub::Attach { cwd, node, answers, role }) => (cwd.clone(), node.clone(), *answers, *role),
        _ => (None, None, true, nypi_hub_proto::ClientRole::Interface),
    };
    // A folder an interface on another device names is on that device. Where that device has
    // no node, there is no such folder to work in, and the interface is at the hub's own.
    let (cwd, device) = match (device, own_device) {
        (Some(device), _) => (cwd, Some(device)),
        // Its own device, when it named a folder there and that device's node is connected.
        (None, Some(own)) if cwd.is_some() && primary.connected(&own) => (cwd, Some(own)),
        (None, Some(_)) => (None, None),
        (None, None) => (cwd, None),
    };
    // Interfaces come here: whoever says it is a node is in the wrong place, and is not given
    // the conversation.
    let focus = match role {
        nypi_hub_proto::ClientRole::Interface => primary.focus(cwd.as_deref(), device.as_deref()),
        nypi_hub_proto::ClientRole::Node => Err(anyhow::anyhow!("a node does not attach to the conversation")),
    };
    let focus = match focus {
        Ok(focus) => focus,
        Err(error) => {
            log(&format!("an interface could not be attached: {error:#}"));
            let refused = json!({ "type": "hub_refused", "reason": format!("{error:#}") });
            let _ = write_record(&mut write, &refused.to_string()).await;
            return;
        }
    };
    let thread = primary.thread();
    // Subscribed before the greeting, so nothing emitted in between is missed.
    let records = thread.hub.lines.subscribe();
    // A connection that began with a command and no attach line: that command is its first.
    let carried = if attach.is_none() { first } else { None };
    let place = Some((primary, focus));
    converse(thread, records, lines, write, answers, carried, place, stop).await;
}

/// One interface on the conversation, until it leaves: its lines go to the runtime as commands,
/// each marked with where the interface is, and every record of the conversation comes back.
/// The same for a socket and for a pipe inside one process (which has one place, and no focus
/// to name). `answers` says whether somebody there can answer an approval.
#[allow(clippy::too_many_arguments)]
async fn converse<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    thread: Arc<Thread>,
    mut records: broadcast::Receiver<Arc<str>>,
    mut lines: tokio::io::Lines<BufReader<R>>,
    mut write: W,
    answers: bool,
    mut carried: Option<String>,
    place: Option<(Arc<PrimaryStream>, AmbientFocus)>,
    stop: Option<CancellationToken>,
) {
    let stop = stop.unwrap_or_default();
    let hub = &thread.hub;
    let count = hub.clients.fetch_add(1, Ordering::SeqCst) + 1;
    if answers {
        hub.answering.fetch_add(1, Ordering::SeqCst);
    }
    let (primary, mut focus) = match place {
        Some((primary, focus)) => (Some(primary), Some(focus)),
        None => (None, None),
    };
    let at = |focus: &Option<AmbientFocus>| {
        focus.as_ref().map_or_else(|| thread.cwd.display().to_string(), |f| f.to_string())
    };
    log(&format!("interface attached at {} ({count} there now)", at(&focus)));
    let waiting: Vec<Value> = hub.approvals.lock().unwrap().values().map(|waiting| waiting.record.clone()).collect();
    let mut hello = hub.hello();
    if let Some(focus) = &focus {
        // The greeting names where this interface is, which is where its work is done.
        hello["focus"] = json!(focus);
        hello["cwd"] = json!(focus.cwd);
        hello["device"] = json!(focus.device);
    }
    if let Some(primary) = &primary {
        hello["tasks"] = json!(primary.tasks());
    }
    let mut open = write_record(&mut write, &hello.to_string()).await.is_ok();
    for record in waiting {
        open = open && write_record(&mut write, &record.to_string()).await.is_ok();
    }
    while open {
        let line = match carried.take() {
            Some(line) => Ok(Some(line)),
            None => tokio::select! {
                _ = stop.cancelled() => break,
                line = lines.next_line() => line,
                record = records.recv() => {
                    open = match record {
                        Ok(record) => write_record(&mut write, &record).await.is_ok(),
                        Err(broadcast::error::RecvError::Lagged(missed)) => {
                            let note = json!({ "type": "daemon_lagged", "missed": missed });
                            write_record(&mut write, &note.to_string()).await.is_ok()
                        }
                        Err(broadcast::error::RecvError::Closed) => false,
                    };
                    continue;
                }
            },
        };
        match line {
            Ok(Some(line)) => {
                thread.touch();
                let Ok(command) = serde_json::from_str::<Value>(&line) else {
                    // Not a record: the runtime says so, as it does to any interface.
                    open = thread.send(line);
                    continue;
                };
                // The owner moved: what this interface asks from now on is asked from there.
                if let (Some(ToHub::Focus { device, cwd }), Some(primary)) = (ToHub::read(&command), &primary) {
                    let answer = match primary.focus(Some(&cwd), device.as_deref()) {
                        Ok(moved) => {
                            let answer = json!({ "type": "focused", "focus": moved });
                            focus = Some(moved);
                            answer
                        }
                        Err(error) => json!({ "type": "hub_refused", "reason": format!("{error:#}") }),
                    };
                    open = write_record(&mut write, &answer.to_string()).await.is_ok();
                    continue;
                }
                hub.note_command(&command);
                open = match &focus {
                    Some(focus) => thread.send(nypi_hub_proto::with_focus(command, focus).to_string()),
                    None => thread.send(line),
                };
            }
            _ => open = false,
        }
    }
    let count = hub.clients.fetch_sub(1, Ordering::SeqCst) - 1;
    if answers {
        hub.answering.fetch_sub(1, Ordering::SeqCst);
    }
    thread.touch();
    log(&format!("interface detached at {} ({count} there now)", at(&focus)));
}

/// Serve on `listener` until `shutdown`: `home` is the runtime of the hub's one conversation,
/// and Telegram is a channel of it when it is set up. The part of `run` that tests can drive.
pub async fn serve(
    home: Runtime,
    listener: UnixListener,
    shutdown: CancellationToken,
    telegram: Option<crate::channels::telegram::Setup>,
) {
    serve_with(home, listener, shutdown, telegram, None).await
}

/// The same, with the register of nodes at whose devices an interface's owner may be.
pub async fn serve_with(
    home: Runtime,
    listener: UnixListener,
    shutdown: CancellationToken,
    telegram: Option<crate::channels::telegram::Setup>,
    nodes: Option<Arc<crate::nodes::Nodes>>,
) {
    serve_all(home, listener, shutdown, telegram, nodes, None).await
}

/// The same, also taking machines' links on `peers` (decisions D80, D93), directly only.
pub async fn serve_all(
    home: Runtime,
    listener: UnixListener,
    shutdown: CancellationToken,
    telegram: Option<crate::channels::telegram::Setup>,
    nodes: Option<Arc<crate::nodes::Nodes>>,
    peers: Option<tokio::net::TcpListener>,
) {
    let _ = serve_hub(home, listener, shutdown, telegram, nodes, Network::direct_only(peers)).await;
}

/// How other machines reach the hub (decision D94).
pub struct Network {
    /// The direct listeners (`hub.listen`).
    pub listeners: Vec<tokio::net::TcpListener>,
    /// The hub's direct addresses, as nodes are told them.
    pub directions: Arc<crate::direct::Directions>,
    /// Where the hub announces itself by mDNS; None for nowhere.
    pub lan: Option<Arc<dyn crate::mdns::Lan>>,
    /// The entry whose relay the hub registers with; None to stay off it.
    pub entry: Option<String>,
    /// Take the entry from `settings.json` as it changes, instead of `entry` (decision D97).
    pub follow_entry: bool,
    /// `hub.publicAddress`.
    pub public_address: Option<String>,
    /// `hub.portMapping`.
    pub port_mapping: bool,
    /// Where browsers that came through the entry are handed to the board.
    pub board: Option<PathBuf>,
}

impl Network {
    /// Machines on `listener` only: no relay, no mDNS, and the list nodes are told stays empty.
    pub fn direct_only(listener: Option<tokio::net::TcpListener>) -> Network {
        Network {
            listeners: listener.into_iter().collect(),
            directions: crate::direct::Directions::fixed(Vec::new(), Vec::new()),
            lan: None,
            entry: None,
            follow_entry: false,
            public_address: None,
            port_mapping: false,
            board: None,
        }
    }
}

/// Keep the hub's direct addresses current: every ten seconds, so a change of network is seen
/// soon, with the router asked for a port every half hour when port mapping is on. Linked nodes
/// are told when the list changes.
async fn keep_directions(paths: Paths, network: Arc<NetworkHeld>, nodes: Arc<crate::nodes::Nodes>) {
    let mut asked: Option<tokio::time::Instant> = None;
    loop {
        if network.port_mapping
            && let Some(port) = network.port
            && asked.is_none_or(|at| at.elapsed() > Duration::from_secs(crate::direct::MAPPING_SECONDS as u64 / 2))
        {
            asked = Some(tokio::time::Instant::now());
            let mapped = crate::direct::map_port(port).await;
            if mapped.is_none() {
                log("the router gave no port (NAT-PMP and UPnP-IGD); nodes away reach the hub through the entry");
            }
            *network.directions.mapped.lock().unwrap() = mapped;
        }
        if network.directions.refresh(network.port, network.public_address.as_deref()) {
            let epoch = crate::instance::Instance::load(&paths).map_or(1, |instance| instance.epoch.max(1));
            nodes.tell_paired(epoch, &network.directions.now());
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// What of `Network` the tasks share once the listeners are taken.
struct NetworkHeld {
    directions: Arc<crate::direct::Directions>,
    port: Option<u16>,
    public_address: Option<String>,
    port_mapping: bool,
}

/// Tell the user of browsers asking to be admitted, which the board records (decision D94):
/// the same places a machine's request goes, and never the conversation.
async fn tell_of_browsers(paths: Paths, primary: Arc<PrimaryStream>) {
    loop {
        for request in crate::pairing::untold(&paths) {
            log(&format!("a browser asks to be admitted, from {}", request.from));
            let _ = primary.thread().hub.lines.send(request.record().to_string().into());
            remote::tell(&paths, &request.question()).await;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The hub's service: the conversation on `listener`, and the machines and browsers `network`
/// brings, until it is stopped or handed over (decision D95).
pub async fn serve_hub(
    home: Runtime,
    listener: UnixListener,
    shutdown: CancellationToken,
    telegram: Option<crate::channels::telegram::Setup>,
    nodes: Option<Arc<crate::nodes::Nodes>>,
    network: Network,
) -> Ended {
    let handover = Arc::new(Handover::default());
    let paths = home.paths.clone();
    let cwd = home.cwd.clone();
    let channel_settings = home.settings.clone();
    let review_every = home.settings.goals.review_interval();
    // What the scheduler and the watchdog need of the home runtime, taken before it becomes a thread.
    let (watchdog, dmn, inspector) = (home.settings.watchdog.clone(), home.settings.dmn.clone(), home.inspector());
    let notifiers = [Notifier::from_runtime(&home), Notifier::from_runtime(&home)];
    let threads = PrimaryStream::new(home);
    let paired_nodes = nodes.clone();
    let devices = nodes.clone().map(|nodes| nodes as Arc<dyn nypi_core::place::DeviceLookup>);
    if let Some(nodes) = nodes {
        threads.with_nodes(nodes);
    }
    // The scheduler and the watchdog are modules of the hub: what they find to do, the hub does
    // in its own threads (jobs.rs).
    let bus = threads.bus();
    let jobs = crate::jobs::Jobs::new(paths.clone(), Some(threads.clone()), crate::jobs::real_runtimes(), bus.clone());
    let [for_watchdog, for_scheduler] = notifiers;
    let watching = crate::watchdog::spawn(paths.clone(), watchdog, inspector.clone(), for_watchdog, None, jobs.clone());
    let scheduling = crate::scheduler::spawn_with(
        paths.clone(),
        dmn,
        inspector,
        for_scheduler,
        Some(bus),
        None,
        jobs.clone(),
        devices,
    );
    let home = threads.thread();
    let port = home.port();
    // Proactive goal reviews (principle B4), when configured: handed to the home thread like any
    // prompt that is not the owner's words, so one waits behind a task under way.
    let reviewing = tokio::spawn({
        let port = home.port();
        async move {
            let Some(every) = review_every else { return std::future::pending().await };
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
            loop {
                ticks.tick().await;
                log("goal review due");
                port.send(json!({
                    "type": "prompt",
                    "message": nypi_core::goals::REVIEW_PROMPT,
                    "source": nypi_core::pulse::PulseSource::module("goals"),
                    "origin": "goals",
                }));
            }
        }
    });
    // The channels (decision D81): Telegram when its bot is set up, the inbound webhook when
    // it is turned on. Each hands what comes in to the conversation through the same port.
    let channels = crate::channels::Channels::from_settings(&paths, &channel_settings, telegram);
    // Notices of this home go through them too, as long as the daemon runs.
    let _notices = channels.register(&paths.home);
    let channel = tokio::spawn({
        let running = channels.spawn(&port, &shutdown);
        async move {
            for channel in running {
                let _ = channel.await;
            }
            std::future::pending::<()>().await
        }
    });
    let live = Arc::new(remote::LivePeers::recorded(&paths));
    let accepting = tokio::spawn({
        let (threads, live, paths) = (threads.clone(), live.clone(), paths.clone());
        async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(client(threads.clone(), stream, live.clone(), paths.clone()));
            }
        }
    });
    // Machines, each held to the trust it was admitted with, directly and through the entry;
    // browsers through the entry. Without a register of nodes there is nowhere for a node to go,
    // and nothing is served.
    let pairing = tokio::spawn({
        let (paths, primary, live, handover) = (paths.clone(), threads.clone(), live.clone(), handover.clone());
        async move {
            let Some(nodes) = paired_nodes else { return std::future::pending().await };
            handover::ensure_own_peer(&paths);
            let directions = network.directions.clone();
            let made = remote::Remote::new(paths.clone(), primary.clone(), nodes.clone(), live, directions, handover);
            let remote = match made {
                Ok(remote) => remote,
                Err(error) => {
                    log(&format!("the hub's key could not be read ({error}); machines are not taken"));
                    return std::future::pending().await;
                }
            };
            let port = network.listeners.iter().find_map(|listener| listener.local_addr().ok()).map(|at| at.port());
            let held = Arc::new(NetworkHeld {
                directions: network.directions.clone(),
                port,
                public_address: network.public_address.clone(),
                port_mapping: network.port_mapping,
            });
            // Dropped with this task, which stops them all.
            let mut tasks = tokio::task::JoinSet::new();
            tasks.spawn(keep_directions(paths.clone(), held.clone(), nodes));
            tasks.spawn(tell_of_browsers(paths.clone(), primary));
            if let Some(lan) = network.lan.clone() {
                let fingerprint = nypi_node::identity::Identity::load_or_create(&paths.hub_key())
                    .map(|key| key.fingerprint())
                    .unwrap_or_default();
                let (paths, held, handover) = (paths.clone(), held.clone(), remote.handover.clone());
                let (ours, paths_heard) = (fingerprint.clone(), paths.clone());
                let heard = move |hub: crate::mdns::Heard| {
                    let epoch = crate::instance::Instance::load(&paths_heard).map_or(1, |i| i.epoch.max(1));
                    if hub.fingerprint == ours && hub.epoch > epoch {
                        handover.step_down(hub.epoch);
                    }
                };
                tasks.spawn(crate::mdns::announce(lan, heard, move || {
                    let epoch = crate::instance::Instance::load(&paths).map_or(1, |instance| instance.epoch.max(1));
                    let sockets: Vec<std::net::SocketAddr> =
                        held.directions.local().iter().filter_map(|address| address.parse().ok()).collect();
                    let port = held.port.or_else(|| sockets.first().map(|socket| socket.port()))?;
                    let addresses =
                        sockets.iter().filter(|socket| socket.port() == port).map(|socket| socket.ip()).collect();
                    Some(crate::mdns::Announcement { fingerprint: fingerprint.clone(), epoch, port, addresses })
                }));
            }
            let board = network.board.clone().unwrap_or_else(|| paths.board_public_socket());
            tasks.spawn(entry::follow(remote.clone(), board, network.entry.clone(), network.follow_entry));
            remote::serve(network.listeners, remote).await;
        }
    });
    let beating = tokio::spawn({
        let (threads, home, paths) = (threads.clone(), home.clone(), paths.clone());
        let started = Utc::now().to_rfc3339();
        async move {
            let mut ticks = tokio::time::interval(BEAT_EVERY);
            loop {
                ticks.tick().await;
                let (clients, busy, open) = threads.totals();
                let beat = Heartbeat {
                    pid: std::process::id(),
                    started: started.clone(),
                    beat: Utc::now().to_rfc3339(),
                    clients,
                    busy,
                    cwd: cwd.clone(),
                    channels: home.hub.channels.lock().unwrap().clone(),
                    threads: open,
                };
                if let Err(error) = write_heartbeat(&paths, &beat) {
                    log(&format!("heartbeat not written: {error}"));
                }
            }
        }
    });
    // A move pauses the rounds first, then stops everything.
    let mut paused = false;
    let mut watching = watching;
    let handed = loop {
        tokio::select! {
            _ = shutdown.cancelled() => break false,
            _ = handover.stop.cancelled() => break true,
            _ = handover.pause.cancelled(), if !paused => {
                paused = true;
                scheduling.abort();
                reviewing.abort();
                if let Some(task) = watching.take() {
                    task.abort();
                }
                log("handing over: the scheduler, idle passes, and goal reviews are paused");
            }
        }
    };
    for task in [accepting, beating, channel, reviewing, scheduling, pairing].into_iter().chain(watching) {
        task.abort();
    }
    if !handed {
        // Machines link again when the hub is back.
        live.close_all();
        // Dropping the conversation stops its runtime; a task cut off here is resumed at the next start.
        return Ended::Stopped;
    }
    // Cut off where it is, as a restart would: the hub that starts from this state resumes it.
    threads.thread().halt();
    jobs.halt();
    remove(&paths);
    handover.work_has_ended();
    if let Some(higher) = handover.stepping_down() {
        live.close_all();
        let outcome = handover::step_down(&paths, higher);
        handover.finish(outcome);
    }
    Ended::Handover(handover)
}

/// `nypi hub run`: run until stopped. A task cut off by the stop is resumed by the next start.
/// `telegram` runs the Telegram channel whatever the settings say (`nypi telegram`). `nodes` is
/// the register of devices `runtime` was made with (`RuntimeOptions::devices`), so that the
/// conversation reaches the devices whose nodes connect here.
pub async fn run(runtime: Runtime, telegram: bool, nodes: Arc<crate::nodes::Nodes>) -> anyhow::Result<i32> {
    let paths = runtime.paths.clone();
    let socket = paths.hub_socket();
    let listener = crate::bridge::bind_owner_socket(&socket, "hub")?;
    LOGGING.store(true, Ordering::Relaxed);
    // Nodes that are processes of their own reach the hub on a socket of their own, which
    // carries the node's protocol only.
    nodes.started();
    let node_listener = crate::bridge::bind_owner_socket(&paths.nodes_socket(), "hub for nodes")?;
    let registering = tokio::spawn(nodes.clone().serve(node_listener));
    log(&format!(
        "hub started (pid {}), model {}, working in {}",
        std::process::id(),
        runtime.model().reference(),
        runtime.cwd.display()
    ));
    if !runtime.has_api_key() {
        log(&format!("no API key found for {}", runtime.model().reference()));
    }
    // Machines reach the hub directly on `hub.listen`, and through the entry's relay.
    let hub = runtime.settings.hub.clone();
    let mut listeners = Vec::new();
    for address in &hub.listen {
        match listen(address) {
            Ok(listener) => {
                log(&format!("taking machines directly on {address}"));
                listeners.push(listener);
            }
            Err(error) => log(&format!("hub.listen {address} could not be opened ({error})")),
        }
    }
    let lan = match hub.lan_discovery {
        true => match crate::mdns::Multicast::open() {
            Ok(lan) => Some(Arc::new(lan) as Arc<dyn crate::mdns::Lan>),
            Err(error) => {
                log(&format!(
                    "mDNS could not be opened ({error}); nodes on this network use the addresses they were told"
                ));
                None
            }
        },
        false => None,
    };
    let network = Network {
        listeners,
        directions: Arc::new(crate::direct::Directions::default()),
        lan,
        // None: the local network only, and no connection to a relay is tried (decision D97).
        entry: runtime.settings.entry(),
        follow_entry: true,
        public_address: hub.public_address.clone(),
        port_mapping: hub.port_mapping,
        board: None,
    };
    let telegram = match telegram || runtime.settings.channels.telegram.enabled {
        true => crate::channels::telegram::setup(&paths, &runtime.settings),
        false => None,
    };
    let shutdown = CancellationToken::new();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let serving = serve_hub(runtime, listener, shutdown.clone(), telegram, Some(nodes.clone()), network);
    tokio::pin!(serving);
    let ended = tokio::select! {
        ended = &mut serving => ended,
        _ = terminate.recv() => { shutdown.cancel(); serving.await }
        _ = tokio::signal::ctrl_c() => { shutdown.cancel(); serving.await }
    };
    registering.abort();
    let code = match ended {
        Ended::Stopped => 0,
        // The supervisor runs this machine as what the handover made it.
        Ended::Handover(handover) => match handover.outcome().await {
            Outcome::Moved { epoch } => {
                log(&format!("handed over: another machine is the hub at epoch {epoch}"));
                0
            }
            Outcome::SteppedDown { epoch, .. } => {
                log(&format!("stepped down for the hub at epoch {epoch}"));
                0
            }
            // Started again by the supervisor, at the same epoch, resuming what was cut off.
            Outcome::Resume { reason } => {
                log(&format!("the move did not happen ({reason}); the hub starts again"));
                75
            }
        },
    };
    log("hub stopped");
    remove(&paths);
    crate::channels::telegram::remove(&paths);
    Ok(code)
}

/// A direct listener on `address`. An IPv6 one takes IPv6 only, so `0.0.0.0` and `[::]` on one
/// port can both be open.
fn listen(address: &str) -> std::io::Result<tokio::net::TcpListener> {
    let at: std::net::SocketAddr = address.parse().map_err(std::io::Error::other)?;
    let socket = socket2::Socket::new(socket2::Domain::for_address(at), socket2::Type::STREAM, None)?;
    if at.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.bind(&at.into())?;
    socket.listen(128)?;
    socket.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(socket.into())
}

/// The log the supervisor gives the daemon.
pub fn log_path(paths: &Paths) -> PathBuf {
    paths.home.join("logs").join("hub.log")
}

/// Where the daemon works: the folder named in settings, or the user's home.
pub fn working_dir(setting: Option<&str>) -> PathBuf {
    setting.map(nypi_core::config::expand_home).or_else(dirs::home_dir).unwrap_or_else(|| Path::new("/").to_path_buf())
}

#[cfg(test)]
mod handover_tests;
#[cfg(test)]
mod network_tests;

#[cfg(test)]
pub(crate) mod tests {
    use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
    use nypi_core::outbox::Outbox;
    use nypi_core::runtime::{RuntimeOptions, SessionMode};

    use super::*;

    pub(crate) fn runtime(home: &Path, work: &Path, faux: &FauxProvider) -> Runtime {
        runtime_on(home, work, faux, None)
    }

    /// The same, as a thread on a hub's bus.
    pub(crate) fn runtime_on(home: &Path, work: &Path, faux: &FauxProvider, pulses: Option<PulseBus>) -> Runtime {
        runtime_at(home, work, faux, pulses, None)
    }

    /// The same, with its place on another device.
    pub(crate) fn runtime_at(
        home: &Path,
        work: &Path,
        faux: &FauxProvider,
        pulses: Option<PulseBus>,
        place: Option<nypi_core::place::Remote>,
    ) -> Runtime {
        Runtime::new(RuntimeOptions {
            pulses,
            place,
            cwd: work.to_path_buf(),
            paths: Some(Paths { home: home.to_path_buf() }),
            model: Some("faux/faux-1".into()),
            extra_model: Some(faux_model()),
            stream_fn: Some(faux.stream_fn()),
            session: SessionMode::Ephemeral,
            active_tasks: true,
            ..Default::default()
        })
        .unwrap()
    }

    /// The runtime of a hub's conversation: it reaches the devices in `nodes` and works where
    /// each message's sender is.
    pub(crate) fn routed(home: &Path, work: &Path, faux: &FauxProvider, nodes: &Arc<crate::nodes::Nodes>) -> Runtime {
        Runtime::new(RuntimeOptions {
            cwd: work.to_path_buf(),
            paths: Some(Paths { home: home.to_path_buf() }),
            model: Some("faux/faux-1".into()),
            extra_model: Some(faux_model()),
            stream_fn: Some(faux.stream_fn()),
            session: SessionMode::Primary,
            active_tasks: true,
            devices: Some(nodes.clone()),
            ..Default::default()
        })
        .unwrap()
    }

    pub(crate) struct Client {
        pub(crate) lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        write: tokio::net::unix::OwnedWriteHalf,
    }

    impl Client {
        /// Attach to the home thread.
        pub(crate) async fn attach(socket: &Path) -> Self {
            Self::attach_with(socket, json!({ "type": "attach" })).await
        }

        pub(crate) async fn attach_with(socket: &Path, attach: Value) -> Self {
            let (read, write) = UnixStream::connect(socket).await.unwrap().into_split();
            let mut client = Client { lines: BufReader::new(read).lines(), write };
            client.send(attach).await;
            client
        }

        pub(crate) async fn send(&mut self, command: Value) {
            write_record(&mut self.write, &command.to_string()).await.unwrap();
        }

        pub(crate) async fn next_of(&mut self, kind: &str) -> Value {
            let wait = tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    let line = self.lines.next_line().await.unwrap().expect("the hub closed the connection");
                    let record: Value = serde_json::from_str(&line).unwrap();
                    if record["type"] == kind {
                        return record;
                    }
                }
            });
            wait.await.unwrap_or_else(|_| panic!("no {kind} record"))
        }
    }

    #[tokio::test]
    async fn every_client_sees_the_work_and_leaving_does_not_stop_it() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let faux = FauxProvider::new(vec![
            FauxResponse::text("Noted.\n\nDone: noted."),
            FauxResponse::tool_call("t1", "bash", json!({ "command": "echo would git push" })),
            FauxResponse::text("Pushed.\n\nDone: pushed."),
        ]);
        let socket = home.path().join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let shutdown = CancellationToken::new();
        let daemon = tokio::spawn(serve(runtime(home.path(), work.path(), &faux), listener, shutdown.clone(), None));

        let mut terminal = Client::attach(&socket).await;
        let hello = terminal.next_of("daemon_hello").await;
        assert_eq!((hello["busy"].clone(), hello["clients"].clone()), (json!(false), json!(1)));
        assert_eq!((hello["agent"].clone(), hello["model"].clone()), (json!("Lucia"), json!("faux/faux-1")));
        let mut second = Client::attach(&socket).await;
        assert_eq!(second.next_of("daemon_hello").await["clients"], 2);

        // One asks; both see the answer.
        terminal.send(json!({ "id": "a", "type": "prompt", "message": "note this" })).await;
        assert_eq!(terminal.next_of("response").await["data"]["disposition"], "started");
        let seen = second.next_of("agent_settled").await;
        assert_eq!(
            (seen["answer"].clone(), seen["status"]["kind"].clone()),
            (json!("Noted.\n\nDone: noted."), json!("done"))
        );
        assert_eq!(terminal.next_of("agent_settled").await["task"], seen["task"]);

        // A run that waits for an approval outlives the client that started it; whoever
        // attaches next is shown the question and can answer it.
        terminal.send(json!({ "id": "b", "type": "prompt", "message": "push it" })).await;
        let asked = terminal.next_of("approval_request").await;
        drop(terminal);
        drop(second);
        let mut phone = Client::attach(&socket).await;
        assert_eq!(phone.next_of("daemon_hello").await["busy"], true);
        let replayed = phone.next_of("approval_request").await;
        assert_eq!(
            (replayed["id"].clone(), replayed["summary"].clone()),
            (asked["id"].clone(), json!("echo would git push"))
        );
        phone.send(json!({ "type": "approval_response", "id": replayed["id"], "approved": true })).await;
        assert_eq!(phone.next_of("agent_settled").await["status"]["text"], "pushed.");

        // The answered question is not asked again, and a message while idle starts a new run.
        let mut later = Client::attach(&socket).await;
        let hello = later.next_of("daemon_hello").await;
        assert_eq!((hello["busy"].clone(), hello["clients"].clone()), (json!(false), json!(2)));
        later.send(json!({ "id": "s", "type": "get_state" })).await;
        assert_eq!(later.next_of("response").await["data"]["isStreaming"], false, "no approval was replayed first");
        assert_eq!(faux.requests().len(), 3);

        shutdown.cancel();
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn every_interface_is_on_the_one_conversation_and_work_is_done_where_each_is() {
        let home = tempfile::tempdir().unwrap();
        let (hub_folder, site, api) = (home.path().join("hub"), home.path().join("site"), home.path().join("api"));
        for folder in [&hub_folder, &site, &api] {
            std::fs::create_dir_all(folder).unwrap();
        }
        std::fs::write(home.path().join("settings.json"), r#"{ "sandbox": "off" }"#).unwrap();
        std::fs::write(site.join("notes.txt"), "the site's notes").unwrap();
        std::fs::write(api.join("notes.txt"), "the api's notes").unwrap();
        let read = |id: &str| FauxResponse::tool_call(id, "read", json!({ "path": "notes.txt" }));
        let faux = FauxProvider::new(vec![
            read("r1"),
            FauxResponse::text("Read them.\n\nDone: read the site's notes."),
            read("r2"),
            FauxResponse::text("Read them too.\n\nDone: read the api's notes."),
        ]);
        let paths = Paths { home: home.path().to_path_buf() };
        let nodes = crate::nodes::Nodes::open(&paths);
        let socket = home.path().join("d.sock");
        let shutdown = CancellationToken::new();
        let daemon = tokio::spawn(serve_with(
            routed(home.path(), &hub_folder, &faux, &nodes),
            UnixListener::bind(&socket).unwrap(),
            shutdown.clone(),
            None,
            Some(nodes.clone()),
        ));

        // Terminals in two folders and one that names none: one conversation, each greeted
        // with where it is.
        let mut in_site = Client::attach_with(&socket, json!({ "type": "attach", "cwd": site })).await;
        let hello = in_site.next_of("daemon_hello").await;
        assert!(hello["cwd"].as_str().unwrap().ends_with("/site"), "{hello}");
        assert!(hello["focus"]["cwd"].as_str().unwrap().ends_with("/site"), "{hello}");
        assert!(hello["sessionFile"].as_str().unwrap().contains("/sessions/primary/"), "{hello}");
        assert_eq!(hello["tasks"], json!([]), "nothing is being worked on yet");
        let mut at_home = Client::attach(&socket).await;
        let home_hello = at_home.next_of("daemon_hello").await;
        assert_eq!(home_hello["session"], hello["session"], "there is one conversation");
        assert!(home_hello["cwd"].as_str().unwrap().ends_with("/hub"), "no word: the hub's own folder");
        let mut in_api = Client::attach_with(&socket, json!({ "type": "attach", "cwd": api })).await;
        let api_hello = in_api.next_of("daemon_hello").await;
        assert_eq!((api_hello["session"].clone(), api_hello["clients"].clone()), (hello["session"].clone(), json!(3)));

        // What one asks is done where that one is, and all of them see it.
        in_site.send(json!({ "id": "a", "type": "prompt", "message": "read the notes" })).await;
        for interface in [&mut in_site, &mut in_api, &mut at_home] {
            assert_eq!(interface.next_of("agent_settled").await["status"]["text"], "read the site's notes.");
        }
        in_api.send(json!({ "id": "b", "type": "prompt", "message": "and these" })).await;
        assert_eq!(in_site.next_of("agent_settled").await["status"]["text"], "read the api's notes.");
        let requests = faux.requests();
        let result = |request: &nypi_ai::Context| match request.messages.last() {
            Some(nypi_ai::Message::ToolResult(result)) => nypi_ai::text_of(&result.content),
            other => panic!("no tool result: {other:?}"),
        };
        assert!(result(&requests[1]).contains("the site's notes"), "{}", result(&requests[1]));
        assert!(result(&requests[3]).contains("the api's notes"), "{}", result(&requests[3]));
        // One conversation: the second request holds the first exchange.
        assert!(format!("{:?}", requests[2].messages).contains("read the site's notes"));
        // Each task records where it worked.
        let tasks = nypi_core::ledger::Ledger::new(paths.ledger()).load();
        let folders: Vec<String> = tasks.values().map(|task| task.cwd.display().to_string()).collect();
        assert!(
            folders.iter().any(|f| f.ends_with("/site")) && folders.iter().any(|f| f.ends_with("/api")),
            "{folders:?}"
        );

        // An interface that moves says so, and its next message is done there.
        at_home.send(json!({ "type": "focus", "cwd": site })).await;
        assert!(at_home.next_of("focused").await["focus"]["cwd"].as_str().unwrap().ends_with("/site"));
        at_home.send(json!({ "type": "focus", "cwd": home.path().join("gone") })).await;
        assert!(at_home.next_of("hub_refused").await["reason"].as_str().unwrap().contains("is not a folder"));

        // A folder that does not exist is refused, and so is a connection that says it is a node.
        let mut nowhere =
            Client::attach_with(&socket, json!({ "type": "attach", "cwd": home.path().join("gone") })).await;
        assert!(nowhere.next_of("hub_refused").await["reason"].as_str().unwrap().contains("is not a folder"));
        let mut node = Client::attach_with(&socket, json!({ "type": "attach", "role": "node" })).await;
        let refused = node.next_of("hub_refused").await;
        assert!(refused["reason"].as_str().unwrap().contains("a node does not attach"), "{refused}");
        shutdown.cancel();
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn an_interface_that_cannot_answer_does_not_count_as_someone_to_ask() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let faux = FauxProvider::new(vec![
            FauxResponse::tool_call("t1", "bash", json!({ "command": "echo would git push" })),
            FauxResponse::text("Blocked: the push was not approved."),
        ]);
        let socket = home.path().join("d.sock");
        let shutdown = CancellationToken::new();
        let daemon = tokio::spawn(serve(
            runtime(home.path(), work.path(), &faux),
            UnixListener::bind(&socket).unwrap(),
            shutdown.clone(),
            None,
        ));
        // `nypi -p` waits for the result but has nobody at it to answer.
        let mut print = Client::attach_with(&socket, json!({ "type": "attach", "answers": false })).await;
        print.next_of("daemon_hello").await;
        print.send(json!({ "id": "p", "type": "prompt", "message": "push it" })).await;
        let asked = print.next_of("approval_request").await;
        let paths = Paths { home: home.path().to_path_buf() };
        let mut told = Vec::new();
        for _ in 0..50 {
            told = Outbox::new(paths.outbox()).load();
            if !told.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(told.len(), 1, "the owner is told an approval waits");
        assert!(told[0].text.starts_with("Lucia needs your approval to run bash"), "{}", told[0].text);
        print.send(json!({ "type": "approval_response", "id": asked["id"], "approved": false })).await;
        assert_eq!(print.next_of("agent_settled").await["status"]["kind"], "blocked");
        assert_eq!(Outbox::new(paths.outbox()).load().len(), 1, "the result was seen by the waiting interface");
        shutdown.cancel();
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn a_thread_with_a_chat_model_answers_through_chat() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(home.path().join("settings.json"), r#"{ "chat": { "model": "faux/faux-1" } }"#).unwrap();
        let faux = FauxProvider::new(vec![FauxResponse::text("Good morning! Nothing is running.")]);
        let socket = home.path().join("d.sock");
        let shutdown = CancellationToken::new();
        let daemon = tokio::spawn(serve(
            runtime(home.path(), work.path(), &faux),
            UnixListener::bind(&socket).unwrap(),
            shutdown.clone(),
            None,
        ));
        let mut terminal = Client::attach(&socket).await;
        let hello = terminal.next_of("daemon_hello").await;
        assert_eq!(hello["chat"], "faux/faux-1", "the interface is told chat is in front");
        terminal.send(json!({ "id": "a", "type": "prompt", "message": "good morning" })).await;
        let mut streamed = false;
        let settled = loop {
            let line = terminal.lines.next_line().await.unwrap().unwrap();
            let record: Value = serde_json::from_str(&line).unwrap();
            streamed |= record["type"] == "message_update" && record["module"] == "chat";
            assert_ne!(record["type"], "task_started", "small talk is not handed to the worker");
            if record["type"] == "chat_settled" {
                break record;
            }
        };
        assert!(streamed, "her reply reaches the interface as she writes it");
        assert_eq!(
            (settled["reply"].clone(), settled["working"].clone()),
            (json!("Good morning! Nothing is running."), json!(false))
        );
        terminal.send(json!({ "id": "s", "type": "get_state" })).await;
        assert_eq!(terminal.next_of("response").await["data"]["chat"], true);
        shutdown.cancel();
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn a_chat_reply_nobody_saw_reaches_the_user_and_one_followed_by_work_does_not() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        let hub = Hub::new(
            json!({}),
            runtime(home.path(), work.path(), &FauxProvider::new(vec![])).inspector(),
            false,
            false,
        );
        let (out, records) = mpsc::unbounded_channel();
        let pumping = tokio::spawn(pump(hub.clone(), records, Teller::new(paths.clone(), None), None));
        let mut watching = hub.lines.subscribe();
        let mut feed = async |record: Value| {
            out.send(record).unwrap();
            watching.recv().await.unwrap();
        };
        // Chat's own turn is not work under way.
        feed(json!({ "type": "agent_start", "module": "chat" })).await;
        assert!(!hub.busy.load(Ordering::SeqCst));
        // She handed work over: the task's report will be the result, so the reply is not a notice.
        feed(json!({ "type": "chat_settled", "reply": "On it.", "working": true })).await;
        feed(json!({ "type": "agent_start", "module": "worker" })).await;
        assert!(hub.busy.load(Ordering::SeqCst));
        assert!(Outbox::new(paths.outbox()).load().is_empty());
        // She answered and nothing runs: that answer is all there is, and nobody was there.
        feed(json!({ "type": "chat_settled", "reply": "The backup ran at 02:00.", "working": false })).await;
        feed(json!({ "type": "chat_failed", "error": "429", "delegated": false, "working": false })).await;
        // A channel relayed this one itself.
        feed(json!({ "type": "response", "id": "ch:1", "success": true, "data": { "disposition": "started" } })).await;
        feed(json!({ "type": "chat_settled", "reply": "Sent to the chat already.", "working": false })).await;
        // What the Principal had chat say is told even while a task runs.
        feed(json!({ "type": "chat_settled", "reply": "The report is filed.", "working": true, "stimulus": true }))
            .await;
        let told: Vec<(AlertLevel, String)> =
            Outbox::new(paths.outbox()).load().into_iter().map(|notice| (notice.level, notice.text)).collect();
        assert_eq!(
            told,
            [
                (AlertLevel::Notice, "The backup ran at 02:00.".to_string()),
                (AlertLevel::Error, "Failed: the chat model did not answer (429)".to_string()),
                (AlertLevel::Notice, "The report is filed.".to_string()),
            ]
        );
        pumping.abort();
    }

    #[tokio::test]
    async fn with_nobody_attached_results_and_questions_go_to_the_user() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        let hub = Hub::new(
            json!({}),
            runtime(home.path(), work.path(), &FauxProvider::new(vec![])).inspector(),
            false,
            false,
        );
        let (out, records) = mpsc::unbounded_channel();
        let pumping = tokio::spawn(pump(hub.clone(), records, Teller::new(paths.clone(), None), None));
        let mut watching = hub.lines.subscribe();
        let mut feed = async |record: Value| {
            out.send(record).unwrap();
            watching.recv().await.unwrap();
        };

        // Somebody is attached: they see it, so nothing is posted.
        hub.clients.store(1, Ordering::SeqCst);
        feed(json!({ "type": "agent_start" })).await;
        assert!(hub.busy.load(Ordering::SeqCst));
        feed(json!({ "type": "agent_settled", "status": { "kind": "done", "text": "seen live" }, "task": "t-1" }))
            .await;
        assert!(!hub.busy.load(Ordering::SeqCst));
        assert!(Outbox::new(paths.outbox()).load().is_empty());

        // Nobody is: the question and the result go to the outbox.
        hub.clients.store(0, Ordering::SeqCst);
        feed(json!({ "type": "approval_request", "id": "approval-1", "tool": "bash", "summary": "git push" })).await;
        assert_eq!(hub.approvals.lock().unwrap().len(), 1);
        feed(json!({ "type": "agent_settled", "status": { "kind": "blocked", "text": "need the token" }, "task": "t-2" }))
            .await;
        feed(json!({ "type": "agent_settled", "aborted": false, "error": "no API key" })).await;
        assert!(hub.approvals.lock().unwrap().is_empty(), "a settled run has no open questions");

        let notices = Outbox::new(paths.outbox()).load();
        let told: Vec<(&str, AlertLevel, &str)> =
            notices.iter().map(|n| (n.source.as_str(), n.level, n.text.as_str())).collect();
        assert_eq!(
            told,
            [
                (
                    "hub",
                    AlertLevel::Warning,
                    "Lucia needs your approval to run bash: git push. Answer with: nypi attach"
                ),
                ("hub", AlertLevel::Warning, "Blocked: need the token"),
                ("hub", AlertLevel::Error, "Failed: no API key"),
            ]
        );
        assert_eq!(notices[1].task, Some(TaskId("t-2".into())));
        assert!(notices.iter().all(|notice| notice.is_open()), "no channel here: they wait for the next front end");
        pumping.abort();
    }

    #[tokio::test]
    async fn a_run_gone_quiet_and_a_question_nobody_answers_reach_the_owner() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        let hub = Hub::new(
            json!({}),
            runtime(home.path(), work.path(), &FauxProvider::new(vec![])).inspector(),
            false,
            false,
        );
        let mut teller = Teller::new(paths.clone(), None);
        teller.notify.stall_seconds = 600;
        teller.notify.approval_seconds = 60;
        let mut watching = hub.lines.subscribe();

        // A run that has shown nothing for longer than the limit: the interfaces attached are
        // shown, and with nobody attached it goes to the outbox.
        let mut run = Run { started: Instant::now(), progress: Progress::quiet_for(Duration::from_secs(601)) };
        run.progress.observe_record(&json!({ "type": "tool_execution_start", "call_id": "c", "tool_name": "bash",
                                             "arguments": { "command": "sleep 900" } }));
        watch(&hub, &teller, Some(&mut run)).await;
        assert!(watching.try_recv().is_err(), "it moved a moment ago");
        let mut run = Run { started: Instant::now(), progress: Progress::quiet_for(Duration::from_secs(601)) };
        watch(&hub, &teller, Some(&mut run)).await;
        let shown: Value = serde_json::from_str(&watching.try_recv().unwrap()).unwrap();
        assert_eq!((shown["type"].clone(), shown["level"].clone()), (json!("alert"), json!("warning")));
        assert!(shown["message"].as_str().unwrap().starts_with("Continuing: no progress for 10m"), "{shown}");
        let told = Outbox::new(paths.outbox()).load();
        assert_eq!(told.len(), 1);
        assert_eq!(told[0].text, shown["message"]);
        watch(&hub, &teller, Some(&mut run)).await;
        assert!(watching.try_recv().is_err(), "said once per quiet period");

        // A question waits in a terminal: after a minute the owner is nudged, once.
        hub.clients.store(1, Ordering::SeqCst);
        hub.answering.store(1, Ordering::SeqCst);
        let ask = |since: Duration| Waiting {
            record: json!({ "type": "approval_request", "id": "approval-1", "tool": "bash", "summary": "git push" }),
            since: Instant::now() - since,
            escalated: false,
            asking: None,
        };
        hub.approvals.lock().unwrap().insert("approval-1".into(), ask(Duration::from_secs(5)));
        watch(&hub, &teller, None).await;
        assert!(!hub.approvals.lock().unwrap()["approval-1"].escalated, "not yet");
        hub.approvals.lock().unwrap().insert("approval-1".into(), ask(Duration::from_secs(61)));
        // A channel with the owner on it has put the question to them already.
        hub.approvers.store(1, Ordering::SeqCst);
        watch(&hub, &teller, None).await;
        assert!(!hub.approvals.lock().unwrap()["approval-1"].escalated);
        hub.approvers.store(0, Ordering::SeqCst);
        watch(&hub, &teller, None).await;
        assert!(hub.approvals.lock().unwrap()["approval-1"].escalated);

        // Whoever answers, the question stops waiting for everyone.
        let (out, records) = mpsc::unbounded_channel();
        let pumping = tokio::spawn(pump(hub.clone(), records, Teller::new(paths.clone(), None), None));
        out.send(json!({ "type": "approval_resolved", "id": "approval-1", "approved": true })).unwrap();
        watching.recv().await.unwrap();
        assert!(hub.approvals.lock().unwrap().is_empty());
        pumping.abort();
    }

    #[tokio::test]
    async fn a_specialist_works_in_the_background_and_the_conversation_reviews_its_report_later() {
        use nypi_core::ledger::{Ledger, TaskState};
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        let dir = nypi_core::specialist::create(&paths, "builder", nypi_core::specialist::Template::Native).unwrap();
        std::fs::write(
            dir.join("profile.json"),
            r#"{"kind":"external","description":"Builds things.","command":["sh","-c","cat >/dev/null; sleep 0.3; echo 'Done: the build passed.'"],"stdin":"{task}"}"#,
        )
        .unwrap();
        let faux = FauxProvider::new(vec![
            FauxResponse::tool_call("d1", "delegate", json!({ "specialist": "builder", "task": "Build the site." })),
            FauxResponse::text("The builder is on it.\n\nContinuing: the builder is building the site."),
            FauxResponse::text("The builder reports that the build passed.\n\nDone: the site is built."),
        ]);
        let socket = home.path().join("d.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let shutdown = CancellationToken::new();
        let daemon = tokio::spawn(serve(runtime(home.path(), work.path(), &faux), listener, shutdown.clone(), None));
        let mut terminal = Client::attach(&socket).await;
        terminal.next_of("daemon_hello").await;

        // The run that delegates ends at once: the conversation is free while the specialist works.
        terminal.send(json!({ "id": "a", "type": "prompt", "message": "build the site" })).await;
        let first = terminal.next_of("agent_settled").await;
        assert_eq!(first["status"]["kind"], "continuing", "{first}");
        let ledger = Ledger::new(paths.ledger());
        let parent = TaskId(first["task"].as_str().unwrap().to_string());
        assert_eq!(ledger.get(&parent).unwrap().state, TaskState::Waiting);

        // The report comes back by itself, as a task of its own that the agent reviews.
        let second = terminal.next_of("agent_settled").await;
        assert_eq!(second["status"]["text"], "the site is built.", "{second}");
        let review = ledger.get(&TaskId(second["task"].as_str().unwrap().to_string())).unwrap();
        assert_eq!((review.origin.as_str(), review.state), ("report", TaskState::Done));
        let requests = faux.requests();
        assert_eq!(requests.len(), 3);
        let Some(nypi_ai::Message::User(report)) = requests[2].messages.last() else { panic!("no report") };
        let report = nypi_ai::text_of(&report.content);
        assert!(report.starts_with("The specialist builder you started in the background (d-"), "{report}");
        assert!(report.contains("Done: the build passed."), "{report}");

        // The task that waited is closed by the watchdog: the review reported for it.
        let round = crate::watchdog::run_once(&paths, &Default::default()).await;
        assert!(round.reports.is_empty(), "{:?}", round.reports);
        let waited = ledger.get(&parent).unwrap();
        assert_eq!(waited.state, TaskState::Done);
        assert!(waited.note.starts_with("continued when its specialists reported (d-"), "{}", waited.note);

        shutdown.cancel();
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn an_interface_at_another_device_has_its_work_done_on_that_devices_files() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (laptop_home, site) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        std::fs::write(home.path().join("settings.json"), r#"{ "sandbox": "off" }"#).unwrap();
        std::fs::write(laptop_home.path().join("settings.json"), r#"{ "sandbox": "off" }"#).unwrap();
        std::fs::write(site.path().join("notes.txt"), "written on the laptop").unwrap();
        let faux = FauxProvider::new(vec![
            FauxResponse::tool_call("r1", "read", json!({ "path": "notes.txt" })),
            FauxResponse::text("The notes say: written on the laptop.\n\nDone: read the notes."),
        ]);

        // The hub, with its register of nodes on a socket of its own.
        let nodes = crate::nodes::Nodes::open(&paths);
        let node_socket = home.path().join("n.sock");
        let registering = tokio::spawn(nodes.clone().serve(UnixListener::bind(&node_socket).unwrap()));
        let socket = home.path().join("d.sock");
        let shutdown = CancellationToken::new();
        let daemon = tokio::spawn(serve_with(
            routed(home.path(), work.path(), &faux, &nodes),
            UnixListener::bind(&socket).unwrap(),
            shutdown.clone(),
            None,
            Some(nodes.clone()),
        ));

        // No such device yet: nobody is at a device that is away.
        let mut early =
            Client::attach_with(&socket, json!({ "type": "attach", "cwd": site.path(), "node": "laptop" })).await;
        let refused = early.next_of("hub_refused").await;
        assert!(refused["reason"].as_str().unwrap().contains("no device named laptop is connected"), "{refused}");

        // The laptop's node: another machine's rules and files, a process of its own.
        let laptop = Arc::new(
            nypi_core::device::this_computer(&Paths { home: laptop_home.path().to_path_buf() }, Some("laptop"))
                .unwrap(),
        );
        let (read, write) = crate::nodes::dial(&node_socket).await.unwrap().into_split();
        let device = tokio::spawn(nypi_node::link::serve(laptop, read, write));
        for _ in 0..200 {
            if nodes.get("laptop").is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let attach = json!({ "type": "attach", "cwd": site.path(), "node": "laptop" });
        let mut terminal = Client::attach_with(&socket, attach).await;
        let hello = terminal.next_of("daemon_hello").await;
        assert_eq!((hello["device"].clone(), hello["cwd"].clone()), (json!("laptop"), json!(site.path())));
        terminal.send(json!({ "id": "a", "type": "prompt", "message": "what do the notes say?" })).await;
        let settled = terminal.next_of("agent_settled").await;
        assert_eq!(settled["status"]["text"], "read the notes.", "{settled}");

        // She was told where she works and what devices there are, and the file she read was
        // the laptop's.
        let requests = faux.requests();
        let prompt = &requests[0].system_prompt;
        let folder = site.path().display().to_string();
        assert!(prompt.contains(&format!("Working directory: {folder} on the device \"laptop\"")), "{prompt}");
        assert!(prompt.contains("<devices>") && prompt.contains("- laptop (computer"), "{prompt}");
        assert!(requests[0].tools.iter().any(|tool| tool.name == "read"));
        let Some(nypi_ai::Message::ToolResult(result)) = requests[1].messages.last() else { panic!("no tool result") };
        assert!(nypi_ai::text_of(&result.content).contains("written on the laptop"));
        // The conversation is the hub's one, kept with the hub.
        assert!(hello["sessionFile"].as_str().unwrap().contains("/sessions/primary/"), "{hello}");
        // The task says where it worked, so a resume or a fired prompt goes back to the laptop.
        let tasks = nypi_core::ledger::Ledger::new(paths.ledger()).load();
        let task = tasks.values().find(|task| task.remote.is_some()).expect("the task at the laptop");
        let remote = task.remote.clone().unwrap();
        assert_eq!((remote.device.as_str(), remote.folder.as_str()), ("laptop", folder.as_str()));

        device.abort();
        registering.abort();
        shutdown.cancel();
        daemon.await.unwrap();
    }

    /// Wait until `check` holds, for at most ten seconds.
    async fn until(what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{what} never happened");
    }

    /// Joining with approval (decision D93), over loopback TLS: the machine and the hub show the
    /// same code; admitted as personal it links as a node and talks; admitted as exec-only it
    /// links as a node and is refused when it asks to talk; refused and expired requests end the
    /// join; an invite admits once; and a removal closes its links at once.
    #[tokio::test]
    async fn a_machine_joins_with_the_users_approval_and_is_held_to_its_trust() {
        use crate::pairing::{self, JoinAnswer, Peers, Purpose, Trust};
        use tokio::io::AsyncWriteExt;
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (laptop_home, site) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        let laptop_paths = Paths { home: laptop_home.path().to_path_buf() };
        std::fs::write(home.path().join("settings.json"), r#"{ "sandbox": "off" }"#).unwrap();
        std::fs::write(laptop_home.path().join("settings.json"), r#"{ "sandbox": "off" }"#).unwrap();
        std::fs::write(site.path().join("notes.txt"), "written on the laptop").unwrap();
        let faux = FauxProvider::new(vec![
            FauxResponse::tool_call("r1", "read", json!({ "path": "notes.txt" })),
            FauxResponse::text("Done: read the laptop's notes."),
        ]);
        let nodes = crate::nodes::Nodes::open(&paths);
        let socket = home.path().join("d.sock");
        let network = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = network.local_addr().unwrap().to_string();
        let shutdown = CancellationToken::new();
        crate::instance::Instance::new_hub(&paths, "mini").unwrap().save(&paths).unwrap();
        let daemon = tokio::spawn(serve_all(
            routed(home.path(), work.path(), &faux, &nodes),
            UnixListener::bind(&socket).unwrap(),
            shutdown.clone(),
            None,
            Some(nodes.clone()),
            Some(network),
        ));
        let hub_key = nypi_node::identity::Identity::load_or_create(&paths.hub_key()).unwrap().fingerprint();
        let mut owner = Client::attach(&socket).await;
        owner.next_of("daemon_hello").await;

        // A machine nobody admitted is told so, whatever it asks for.
        let stranger = nypi_node::identity::Identity::generate().unwrap();
        let mut link = stranger.dial(&address, &hub_key).await.unwrap();
        link.write_all(b"{\"purpose\":\"talk\"}\n").await.unwrap();
        let mut answer = BufReader::new(link).lines();
        let refused: Value = serde_json::from_str(&answer.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(refused["type"], "refused");
        assert!(refused["reason"].as_str().unwrap().contains("not admitted to this hub"), "{refused}");

        // The laptop asks to join. Both screens show the same code; the user admits it as theirs.
        let shown = Arc::new(Mutex::new(String::new()));
        let joining = tokio::spawn({
            let (laptop_paths, address, shown) = (laptop_paths.clone(), address.clone(), shown.clone());
            async move {
                pairing::join(
                    &laptop_paths,
                    &[crate::pairing::Way::Direct(address.clone())],
                    "laptop",
                    None,
                    "i-laptop",
                    |code| *shown.lock().unwrap() = code.to_string(),
                )
                .await
            }
        });
        until("the request", || !pairing::pending(&paths).is_empty()).await;
        let request = pairing::pending(&paths).pop().unwrap();
        until("the code on the machine", || !shown.lock().unwrap().is_empty()).await;
        assert_eq!(request.code, *shown.lock().unwrap(), "the same code on both screens");
        assert_eq!(owner.next_of("join_request").await["code"], json!(request.code), "shown where approvals go");
        let notices = std::fs::read_to_string(paths.outbox()).unwrap_or_default();
        assert!(notices.contains(&request.code), "and as a notice for the channels");
        pairing::answer(&paths, &request.code, JoinAnswer::Admit { trust: Trust::Personal }).unwrap();
        let joined = joining.await.unwrap().unwrap();
        assert_eq!((joined.trust, joined.hub_fingerprint.as_str()), (Trust::Personal, hub_key.as_str()));
        let laptop_key =
            nypi_node::identity::Identity::load_or_create(&laptop_paths.device_key()).unwrap().fingerprint();
        assert_eq!(Peers::open(&paths).get(&laptop_key).unwrap().name, "laptop");
        assert!(faux.requests().is_empty(), "a join never reached the conversation");

        // As a node, under the name it was admitted as, whatever it calls itself.
        let node = Arc::new(nypi_core::device::this_computer(&laptop_paths, Some("some-other-name")).unwrap());
        let addresses = vec![crate::pairing::Way::Direct(address.clone())];
        let linked = pairing::link(&laptop_paths, &hub_key, &addresses, Purpose::Node, 0).await.unwrap();
        let serving = tokio::spawn(nypi_node::link::serve(node, linked.read, linked.write));
        until("the node", || nodes.get("laptop").is_some()).await;
        assert!(nodes.get("some-other-name").is_none());

        // Its node link is read as a node's messages only: a prompt there runs nothing.
        let mut forged = pairing::link(&laptop_paths, &hub_key, &addresses, Purpose::Node, 0).await.unwrap();
        let prompt = json!({ "type": "prompt", "message": "delete everything" });
        forged.write.write_all(format!("{prompt}\n").as_bytes()).await.unwrap();
        let said = tokio::time::timeout(Duration::from_secs(10), forged.read.lines().next_line()).await;
        assert!(!matches!(said, Ok(Ok(Some(_))) | Err(_)), "the link is closed without an answer: {said:?}");
        assert!(faux.requests().is_empty());

        // As personal it talks; a folder it names is on the laptop, and the work is done there.
        let mut terminal = pairing::link(&laptop_paths, &hub_key, &addresses, Purpose::Talk, 0).await.unwrap();
        let attach = json!({ "type": "attach", "cwd": site.path() });
        terminal.write.write_all(format!("{attach}\n").as_bytes()).await.unwrap();
        let mut records = terminal.read.lines();
        let hello: Value = serde_json::from_str(&records.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!((hello["type"].clone(), hello["device"].clone()), (json!("daemon_hello"), json!("laptop")));
        let prompt = json!({ "id": "a", "type": "prompt", "message": "what do the notes say?" });
        terminal.write.write_all(format!("{prompt}\n").as_bytes()).await.unwrap();
        let settled = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let record: Value = serde_json::from_str(&records.next_line().await.unwrap().unwrap()).unwrap();
                if record["type"] == "agent_settled" {
                    return record;
                }
            }
        })
        .await
        .expect("the run settles");
        assert_eq!(settled["status"]["text"], "read the laptop's notes.", "{settled}");

        // A second machine is admitted as exec-only, from a conversation (`/admit … exec-only`):
        // it links as a node, and is refused when it asks to talk.
        let vps_home = tempfile::tempdir().unwrap();
        let vps_paths = Paths { home: vps_home.path().to_path_buf() };
        let joining = tokio::spawn({
            let (vps_paths, address) = (vps_paths.clone(), address.clone());
            async move {
                pairing::join(&vps_paths, &[crate::pairing::Way::Direct(address.clone())], "vps", None, "i-vps", |_| {})
                    .await
            }
        });
        until("the vps's request", || !pairing::pending(&paths).is_empty()).await;
        let code = pairing::pending(&paths).pop().unwrap().code;
        let said = crate::commands::join_command(&paths, &format!("/admit {code} exec-only")).unwrap();
        assert_eq!(said, "Admitted vps as exec-only.");
        assert_eq!(joining.await.unwrap().unwrap().trust, Trust::ExecOnly);
        let vps_key = nypi_node::identity::Identity::load_or_create(&vps_paths.device_key()).unwrap().fingerprint();
        let talk = pairing::link(&vps_paths, &hub_key, &addresses, Purpose::Talk, 0).await.unwrap_err();
        assert!(talk.to_string().contains("exec-only"), "{talk:#}");
        assert!(pairing::link(&vps_paths, &hub_key, &addresses, Purpose::Node, 0).await.is_ok());
        assert_eq!(Peers::open(&paths).get(&vps_key).unwrap().trust, Trust::ExecOnly);

        // Refused, and expired: the join ends, and nothing is admitted.
        let refused_home = tempfile::tempdir().unwrap();
        let refused_paths = Paths { home: refused_home.path().to_path_buf() };
        let joining = tokio::spawn({
            let (refused_paths, address) = (refused_paths.clone(), address.clone());
            async move {
                pairing::join(
                    &refused_paths,
                    &[crate::pairing::Way::Direct(address.clone())],
                    "stranger",
                    None,
                    "i-s",
                    |_| {},
                )
                .await
            }
        });
        until("the third request", || !pairing::pending(&paths).is_empty()).await;
        let code = pairing::pending(&paths).pop().unwrap().code;
        assert_eq!(crate::commands::join_command(&paths, &format!("/refuse {code}")).unwrap(), "Refused stranger.");
        let error = joining.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("refused"), "{error:#}");
        let joining = tokio::spawn({
            let (refused_paths, address) = (refused_paths.clone(), address.clone());
            async move {
                pairing::join(
                    &refused_paths,
                    &[crate::pairing::Way::Direct(address.clone())],
                    "stranger",
                    None,
                    "i-s",
                    |_| {},
                )
                .await
            }
        });
        until("the fourth request", || !pairing::pending(&paths).is_empty()).await;
        let mut file: Value = serde_json::from_str(&std::fs::read_to_string(paths.join_requests()).unwrap()).unwrap();
        file["requests"][0]["expires"] = json!((chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
        std::fs::write(paths.join_requests(), file.to_string()).unwrap();
        let error = joining.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("nobody answered"), "{error:#}");
        assert_eq!(Peers::open(&paths).list().len(), 3, "the laptop, the vps, and the hub's own machine only");

        // An invite admits once, with no question; used again it is refused.
        let code = pairing::invite(&paths, "builder", Trust::ExecOnly, chrono::Duration::hours(1)).unwrap();
        let invited_home = tempfile::tempdir().unwrap();
        let invited = Paths { home: invited_home.path().to_path_buf() };
        let joined = pairing::join(
            &invited,
            &[crate::pairing::Way::Direct(address.clone())],
            "whatever",
            Some(&code),
            "i-b",
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!((joined.name.as_str(), joined.trust), ("builder", Trust::ExecOnly));
        let again_home = tempfile::tempdir().unwrap();
        let again = Paths { home: again_home.path().to_path_buf() };
        let error =
            pairing::join(&again, &[crate::pairing::Way::Direct(address.clone())], "x", Some(&code), "i-x", |_| {})
                .await
                .unwrap_err();
        assert!(error.to_string().contains("invite code is unknown, used, or expired"), "{error:#}");

        // Removing the laptop closes its node and talk links at once (decision D83).
        assert_eq!(Peers::open(&paths).remove("laptop").unwrap(), 1);
        let changed = crate::link::peers_changed_at(&socket).await.unwrap();
        assert!(changed.is_some_and(|closed| closed >= 2), "{changed:?}");
        let ended = tokio::time::timeout(Duration::from_secs(10), async {
            while let Ok(Some(_)) = records.next_line().await {}
        });
        assert!(ended.await.is_ok(), "the talk link was closed");
        assert!(tokio::time::timeout(Duration::from_secs(10), serving).await.is_ok(), "the node link was closed");
        let gone = pairing::link(&laptop_paths, &hub_key, &addresses, Purpose::Talk, 0).await.unwrap_err();
        assert!(gone.to_string().contains("not admitted"), "{gone:#}");

        drop(owner);
        shutdown.cancel();
        daemon.await.unwrap();
    }

    /// A D80 home on both sides (decision D93): the hub's register with the laptop paired as a node
    /// and an interface, the laptop with two keys and `devices/hub.json`. After the move to D93's
    /// files the laptop links again, as a personal node, without joining.
    #[tokio::test]
    async fn a_d80_device_links_again_without_joining() {
        use crate::pairing::{Purpose, Trust};
        let (home, work, laptop_home) =
            (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        let laptop = Paths { home: laptop_home.path().to_path_buf() };
        std::fs::write(home.path().join("settings.json"), r#"{ "sandbox": "off" }"#).unwrap();
        let nodes = crate::nodes::Nodes::open(&paths);
        let network = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = network.local_addr().unwrap().to_string();
        let hub_key = nypi_node::identity::Identity::load_or_create(&paths.hub_key()).unwrap().fingerprint();
        let node_key = nypi_node::identity::Identity::load_or_create(&laptop.device_key()).unwrap().fingerprint();
        let interface_key =
            nypi_node::identity::Identity::load_or_create(&laptop.interface_key()).unwrap().fingerprint();
        std::fs::create_dir_all(paths.peers().parent().unwrap()).unwrap();
        let old = json!({
            node_key.clone(): { "name": "laptop", "role": "node", "pairedAt": "2026-11-01T00:00:00Z" },
            interface_key: { "name": "laptop", "role": "interface", "pairedAt": "2026-11-01T00:00:00Z" },
        });
        std::fs::write(paths.peers(), old.to_string()).unwrap();
        std::fs::create_dir_all(laptop.paired_hub().parent().unwrap()).unwrap();
        let paired = json!({
            "node": { "address": address, "fingerprint": hub_key, "name": "laptop" },
            "interface": { "address": address, "fingerprint": hub_key, "name": "laptop" },
        });
        std::fs::write(laptop.paired_hub(), paired.to_string()).unwrap();
        let shutdown = CancellationToken::new();
        let faux = FauxProvider::new(vec![]);
        let daemon = tokio::spawn(serve_all(
            routed(home.path(), work.path(), &faux, &nodes),
            UnixListener::bind(home.path().join("d.sock")).unwrap(),
            shutdown.clone(),
            None,
            Some(nodes.clone()),
            Some(network),
        ));
        let instance = crate::instance::Instance::load(&laptop).unwrap();
        assert_eq!((instance.trust, instance.direct.clone()), (Trust::Personal, vec![address.clone()]));
        let ways: Vec<crate::pairing::Way> = instance.direct.iter().cloned().map(crate::pairing::Way::Direct).collect();
        let linked = crate::pairing::link(&laptop, &instance.hub_fingerprint, &ways, Purpose::Talk, 0).await;
        let linked = linked.expect("it talks as the personal machine it was");
        assert_eq!((linked.name.as_str(), linked.trust), ("laptop", Trust::Personal));
        let peers = crate::pairing::Peers::open(&paths).list();
        assert_eq!(peers.len(), 1, "one peer per machine");
        assert_eq!(peers.get(&node_key).unwrap().trust, Trust::Personal);
        shutdown.cancel();
        daemon.await.unwrap();
    }

    /// Someone in the middle (decision D93): a machine that holds the link to the joining machine
    /// with its own key, and its own link to the hub, makes the two codes differ, so the user
    /// does not admit it.
    #[tokio::test]
    async fn a_machine_in_the_middle_makes_the_codes_differ() {
        use nypi_node::identity::{
            Identity, confirmation_code, join_value_client, join_value_server, peer_fingerprint, server_fingerprint,
        };
        let hub = Identity::generate().unwrap();
        let middle = Identity::generate().unwrap();
        let device = Identity::generate().unwrap();
        let hub_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub_address = hub_listener.local_addr().unwrap().to_string();
        let middle_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let middle_address = middle_listener.local_addr().unwrap().to_string();
        let hub_fingerprint = hub.fingerprint();
        let hub_side = tokio::spawn(async move {
            let (tcp, _) = hub_listener.accept().await.unwrap();
            let stream = hub.acceptor().unwrap().accept(tcp).await.unwrap();
            let device = peer_fingerprint(&stream).unwrap();
            confirmation_code(&hub.fingerprint(), &device, &join_value_server(&stream).unwrap())
        });
        // The middle answers the device with its own key, and joins the hub as a machine itself.
        let middle_side = tokio::spawn(async move {
            let (tcp, _) = middle_listener.accept().await.unwrap();
            let _toward_device = middle.acceptor().unwrap().accept(tcp).await.unwrap();
            let _toward_hub = middle.dial_any(&hub_address).await.unwrap();
        });
        let stream = device.dial_any(&middle_address).await.unwrap();
        let seen = server_fingerprint(&stream).unwrap();
        let device_code = confirmation_code(&seen, &device.fingerprint(), &join_value_client(&stream).unwrap());
        middle_side.await.unwrap();
        let hub_code = hub_side.await.unwrap();
        assert_ne!(seen, hub_fingerprint, "the device saw the middle's key");
        assert_ne!(device_code, hub_code, "and the user sees two different codes");
    }

    #[test]
    fn a_process_the_daemon_started_is_told_apart_from_the_users() {
        // 10 is the daemon; 20 is a shell it started, 30 a program that shell started; 40 is
        // the user's terminal, started by 2.
        let parents = |pid: u32| match pid {
            30 => Some(20),
            20 => Some(10),
            10 => Some(1),
            40 => Some(2),
            2 => Some(1),
            _ => None,
        };
        assert!(descends(20, 10, parents) && descends(30, 10, parents));
        assert!(!descends(40, 10, parents), "the user's own terminal");
        assert!(!descends(10, 10, parents), "the hub is not its own child");
        assert!(!descends(99, 10, parents), "a process that is already gone");
        assert!(!descends(7, 10, Some), "a table that loops ends");
        assert_eq!(parent_of(std::process::id()), Some(std::os::unix::process::parent_id()));
    }

    #[test]
    fn the_heartbeat_says_whether_the_daemon_is_there() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let now = Utc::now();
        assert!(!alive(&paths, now), "never started");
        let mut beat = Heartbeat {
            pid: std::process::id(),
            started: now.to_rfc3339(),
            beat: now.to_rfc3339(),
            clients: 0,
            busy: false,
            cwd: "/".into(),
            channels: Vec::new(),
            threads: 1,
        };
        write_heartbeat(&paths, &beat).unwrap();
        assert!(alive(&paths, now));
        assert!(!alive(&paths, now + chrono::Duration::seconds(STALE_SECONDS + 1)), "a hung hub is not alive");
        beat.pid = i32::MAX as u32 - 3;
        write_heartbeat(&paths, &beat).unwrap();
        assert!(!alive(&paths, now), "its process is gone");
        remove(&paths);
        assert!(heartbeat(&paths).is_none());
        assert_eq!(working_dir(Some("/srv/lucia")), Path::new("/srv/lucia"));

        let (level, text) = settled_report(&json!({ "status": { "kind": "done", "text": "" } }));
        assert_eq!((level, text.as_str()), (AlertLevel::Notice, "Done"));
    }

    #[test]
    fn a_hub_from_before_d100_is_still_found_by_its_old_heartbeat() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let now = Utc::now().to_rfc3339();
        let beat = |pid: u32| Heartbeat {
            pid,
            started: now.clone(),
            beat: now.clone(),
            clients: 0,
            busy: false,
            cwd: "/".into(),
            channels: Vec::new(),
            threads: 1,
        };
        assert_eq!(running_pid(&paths), None, "nothing ever ran");
        let old = paths.home.join("run").join("daemon.json");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, serde_json::to_string(&beat(std::process::id())).unwrap()).unwrap();
        assert_eq!((legacy_pid(&paths), running_pid(&paths)), (Some(std::process::id()), Some(std::process::id())));
        assert!(heartbeat(&paths).is_none(), "the old file is not the hub's heartbeat");

        std::fs::write(&old, serde_json::to_string(&beat(i32::MAX as u32 - 3)).unwrap()).unwrap();
        assert_eq!(running_pid(&paths), None, "an old hub whose process is gone");

        // Once a hub of this version has written its own, the old file is not looked at.
        std::fs::write(&old, serde_json::to_string(&beat(std::process::id())).unwrap()).unwrap();
        write_heartbeat(&paths, &beat(i32::MAX as u32 - 3)).unwrap();
        assert_eq!((legacy_pid(&paths), running_pid(&paths)), (None, None));
        assert!(paths.home.join("run").join("hub.json").exists());
    }

    #[tokio::test]
    async fn a_focus_is_a_folder_that_exists_or_a_device_that_is_connected() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let faux = FauxProvider::new(vec![]);
        let primary = PrimaryStream::new(runtime(home.path(), work.path(), &faux));
        let own = nypi_core::protected::real_path(work.path());
        assert_eq!(primary.focus(None, None).unwrap(), AmbientFocus { device: None, cwd: own.clone() });
        // One spelling per folder: the real path, without `.` steps.
        assert_eq!(primary.focus(Some(&work.path().join("./")), None).unwrap().cwd, own);
        assert!(primary.focus(Some(&work.path().join("gone")), None).is_err());
        let refused = primary.focus(Some(Path::new("/srv/app")), Some("mini")).unwrap_err().to_string();
        assert!(refused.contains("no device named mini is connected"), "{refused}");
        assert!(primary.focus(None, Some("mini")).unwrap_err().to_string().contains("a folder on mini is needed"));
        assert_eq!(device_folder("laptop", "/srv/site/./"), device_folder("laptop", "/srv//site"));
        // Work recorded at a place goes back there, connected or not: its tools say if they cannot.
        let remote = DeviceFolder { device: "mini".into(), folder: "/srv/app".into() };
        assert_eq!(primary.focus_at(None, Some(&remote)).to_string(), "mini:/srv/app");
        assert_eq!(primary.focus_at(Some(Path::new("/code/site")), None).to_string(), "/code/site");
        assert_eq!(primary.totals(), (0, false, 1));
    }
}
