//! The hub's register of nodes (docs/devices.md section 8, decision D66): which devices have a
//! node, what each last said about itself, and which are connected now. A node that is a
//! process of its own (`nypi node`) connects to a Unix socket in NyPi's run folder, says who it
//! is, and then answers the hub's calls; a thread whose place is on that device sends its
//! device tools' calls there (nypi-core/src/place.rs).
//!
//! The socket carries the node's protocol and nothing else: what connects there can answer
//! calls, and cannot give Lucia a task or answer an approval. It can still feed her false
//! results, so the agent's own tools must not reach it: the sandbox denies sockets in the run
//! folder, the guard refuses `nypi node` in a command, and a peer that is a process the hub
//! started is turned away, as on the daemon's socket.
//!
//! A node on another computer links over a machine's link (`daemon/remote.rs`), directly or
//! through the relay, under the name its key was admitted with. A call whose link drops is not
//! given up at once: the node keeps it running and journals its result
//! (nypi-node/src/journal.rs), and the call waits up to the lease for the device to come back on
//! a new link and hand the result over, which the hub then acknowledges (decision D74).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use nypi_core::config::Paths;
use nypi_node::NodeLink;
use nypi_node::link::{MAX_LINE, read_line};
use nypi_node::proto::{Call, FromNode, Report, ToNode};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::daemon::log;

/// A node says who it is within this long, or the connection is dropped.
const HELLO_WAIT: Duration = Duration::from_secs(10);
/// How often a connected node is asked whether it is there.
const PING_EVERY: Duration = Duration::from_secs(20);
/// A link silent for this long is lost.
const SILENT_LIMIT: Duration = Duration::from_secs(65);
/// When a node of a connected name says hello again, the link it had gets this long to show
/// it is still there; a link that says nothing is a dead one (a dropped SSH tunnel) and the new
/// one takes its place, instead of waiting out `SILENT_LIMIT`.
const PROBE_WAIT: Duration = Duration::from_secs(3);

/// Calls under way, by id (new for every call, so unique across devices).
type Calls = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<FromNode>>>>;

/// One device in the register.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Known {
    /// What its node last said about itself.
    pub report: Report,
    pub connected: bool,
    /// Connected since then, or last seen then.
    pub since: String,
}

/// A node the hub reaches over a link.
pub struct RemoteNode {
    report: Mutex<Report>,
    to_node: mpsc::UnboundedSender<ToNode>,
    /// The calls under way on any link, each with where its events go: shared by every link of
    /// the register, so a result handed over on a new link reaches the call that waits for it.
    waiting: Calls,
    /// How long a call waits for its device to come back after its link drops.
    lease: Duration,
    /// Cancelled when the link is gone.
    gone: CancellationToken,
    /// When the node last said anything on its link.
    heard: Mutex<tokio::time::Instant>,
    /// Questions about a folder that wait for the node's answer, by id.
    asked: Mutex<HashMap<String, tokio::sync::oneshot::Sender<nypi_node::proto::PlaceContext>>>,
    /// Updates asked of the node that wait for its answer, by id (decision D98).
    updates: Mutex<HashMap<String, tokio::sync::oneshot::Sender<(bool, String)>>>,
    /// It came over a machine's link, under the name its key was admitted with.
    paired: bool,
}

/// How long a node gets to say what a folder holds.
const CONTEXT_WAIT: Duration = Duration::from_secs(5);

impl RemoteNode {
    fn name(&self) -> String {
        self.report.lock().unwrap().identity.name.clone()
    }

    /// Pass on what the node said about a call to whoever made it. The last word on a call is
    /// acknowledged, so the node's journal lets it go, whether or not anyone still waits.
    /// Ask the node to update to `version` (or the latest release), and wait up to `wait` for
    /// its answer: what it did, or why not (decision D98).
    pub async fn update(&self, version: Option<String>, wait: Duration) -> Result<String, String> {
        let id = format!("u-{}", uuid::Uuid::now_v7().simple());
        let (answer, answered) = tokio::sync::oneshot::channel();
        self.updates.lock().unwrap().insert(id.clone(), answer);
        if self.to_node.send(ToNode::Update { id: id.clone(), version }).is_err() {
            self.updates.lock().unwrap().remove(&id);
            return Err("the node's link is gone".into());
        }
        let said = tokio::time::timeout(wait, answered).await;
        self.updates.lock().unwrap().remove(&id);
        match said {
            Ok(Ok((true, message))) => Ok(message),
            Ok(Ok((false, message))) => Err(message),
            Ok(Err(_)) => Err("the node went away before it answered".into()),
            Err(_) => Err("the node did not answer in time".into()),
        }
    }

    fn deliver(&self, event: FromNode) {
        if let FromNode::Updated { id, ok, message } = event {
            if let Some(waiting) = self.updates.lock().unwrap().remove(&id) {
                let _ = waiting.send((ok, message));
            }
            return;
        }
        if let FromNode::PlaceContext { id, context } = event {
            if let Some(waiting) = self.asked.lock().unwrap().remove(&id) {
                let _ = waiting.send(context);
            }
            return;
        }
        let Some(id) = event.id().map(str::to_string) else { return };
        let last = event.ends_call();
        if let Some(caller) = self.waiting.lock().unwrap().get(&id) {
            let _ = caller.send(event);
        }
        if last {
            let _ = self.to_node.send(ToNode::Ack { id });
        }
    }
}

#[async_trait]
impl NodeLink for RemoteNode {
    fn report(&self) -> Report {
        self.report.lock().unwrap().clone()
    }

    async fn call(&self, call: Call, events: mpsc::UnboundedSender<FromNode>) {
        let id = call.id.clone();
        let away = |why: &str| FromNode::Refused { id: id.clone(), reason: format!("{} {why}", self.name()) };
        let (answers, mut heard) = mpsc::unbounded_channel();
        self.waiting.lock().unwrap().insert(id.clone(), answers);
        if self.gone.is_cancelled() || self.to_node.send(ToNode::Call(call)).is_err() {
            let _ = events.send(away("is not connected"));
        } else {
            loop {
                tokio::select! {
                    event = heard.recv() => {
                        let Some(event) = event else { break };
                        let last = event.ends_call();
                        let _ = events.send(event);
                        if last {
                            break;
                        }
                    }
                    // The node keeps the call going and its result in its journal: wait for it
                    // to come back on a new link. Past the lease, say so, and let the model
                    // decide what to do about a step that may or may not have happened.
                    _ = self.gone.cancelled() => {
                        let back = tokio::time::timeout(self.lease, async {
                            while let Some(event) = heard.recv().await {
                                let last = event.ends_call();
                                let _ = events.send(event);
                                if last {
                                    return true;
                                }
                            }
                            false
                        });
                        if !back.await.unwrap_or(false) {
                            let _ = events.send(away("went away during this call; whether it was carried out is not known"));
                        }
                        break;
                    }
                }
            }
        }
        self.waiting.lock().unwrap().remove(&id);
    }

    fn cancel(&self, id: &str) {
        let _ = self.to_node.send(ToNode::Cancel { id: id.to_string() });
    }

    async fn place_context(&self, folder: &str) -> Option<nypi_node::proto::PlaceContext> {
        let id = format!("x-{}", uuid::Uuid::now_v7().simple());
        let (answer, answered) = tokio::sync::oneshot::channel();
        self.asked.lock().unwrap().insert(id.clone(), answer);
        let sent = self.to_node.send(ToNode::ReadPlaceContext { id: id.clone(), folder: folder.to_string() });
        let context = match sent {
            Ok(()) => tokio::time::timeout(CONTEXT_WAIT, answered).await.ok().and_then(Result::ok),
            Err(_) => None,
        };
        self.asked.lock().unwrap().remove(&id);
        context
    }
}

/// A device by its name, whichever link its node is on at the moment: what a thread whose place
/// is on that device holds, so the thread outlives the node going away and coming back.
struct ByName {
    nodes: Arc<Nodes>,
    name: String,
}

#[async_trait]
impl NodeLink for ByName {
    /// What the node says now, or what it last said before it went away.
    fn report(&self) -> Report {
        match self.nodes.get(&self.name) {
            Some(node) => node.report(),
            None => {
                self.nodes.known.lock().unwrap().get(&self.name).map(|known| known.report.clone()).unwrap_or(Report {
                    identity: nypi_node::proto::Identity {
                        name: self.name.clone(),
                        kind: nypi_node::proto::DeviceKind::Computer,
                        system: String::new(),
                        version: String::new(),
                        home: None,
                    },
                    tools: Vec::new(),
                    running: Vec::new(),
                    relay: false,
                })
            }
        }
    }

    async fn call(&self, call: Call, events: mpsc::UnboundedSender<FromNode>) {
        match self.nodes.get(&self.name) {
            Some(node) => node.call(call, events).await,
            None => {
                let reason = format!("{} is not connected (nypi devices shows when it was last seen)", self.name);
                let _ = events.send(FromNode::Refused { id: call.id, reason });
            }
        }
    }

    fn cancel(&self, id: &str) {
        if let Some(node) = self.nodes.get(&self.name) {
            node.cancel(id);
        }
    }

    async fn place_context(&self, folder: &str) -> Option<nypi_node::proto::PlaceContext> {
        self.nodes.get(&self.name)?.place_context(folder).await
    }
}

/// The register: every device the hub has heard from, and the links that are open now.
pub struct Nodes {
    paths: Paths,
    connected: Mutex<HashMap<String, Arc<RemoteNode>>>,
    known: Mutex<BTreeMap<String, Known>>,
    waiting: Calls,
    /// How long a call waits for its device after the link drops (the node's own lease).
    lease: Duration,
    /// The register itself, for the links it hands out by name.
    me: std::sync::Weak<Nodes>,
}

/// A runtime reaches the hub's devices through the register (decision D79): each by its name,
/// on whichever link its node is on when a call is made.
impl nypi_core::place::DeviceLookup for Nodes {
    fn devices(&self) -> Vec<nypi_core::place::DeviceInfo> {
        self.list()
            .into_values()
            .map(|known| nypi_core::place::DeviceInfo {
                name: known.report.identity.name,
                kind: format!("{:?}", known.report.identity.kind).to_lowercase(),
                system: known.report.identity.system,
                home: known.report.identity.home,
                connected: known.connected,
                tools: known.report.tools,
            })
            .collect()
    }

    fn node(&self, device: &str) -> Result<Arc<dyn NodeLink>, String> {
        let Some(nodes) = self.me.upgrade() else { return Err("the hub is stopping".to_string()) };
        if self.get(device).is_some() {
            return Ok(Arc::new(ByName { nodes, name: device.to_string() }));
        }
        match self.known.lock().unwrap().contains_key(device) {
            true => Err(format!("{device} is not connected (nypi devices shows when it was last seen)")),
            false => Err(format!("no device named {device} is known to this hub (nypi devices lists them)")),
        }
    }
}

/// The register as it was last written: what `nypi devices` reads.
pub fn read_register(paths: &Paths) -> BTreeMap<String, Known> {
    std::fs::read_to_string(paths.devices()).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

impl Nodes {
    /// Open the register. Whatever it says was connected is not, until its node says so again.
    pub fn open(paths: &Paths) -> Arc<Nodes> {
        Nodes::open_with(paths, nypi_node::link::LEASE)
    }

    /// The register for a daemon that is about to start: read, and not written until `started`,
    /// so a second `nypi hub run` that finds one running leaves that one's register as it is.
    pub fn starting(paths: &Paths) -> Arc<Nodes> {
        Nodes::read(paths, nypi_node::link::LEASE)
    }

    /// The daemon holds its socket: the register on disk is this one from now on.
    pub fn started(&self) {
        self.save();
    }

    /// `open`, with calls waiting `lease` for a device whose link dropped.
    pub(crate) fn open_with(paths: &Paths, lease: Duration) -> Arc<Nodes> {
        let nodes = Nodes::read(paths, lease);
        nodes.save();
        nodes
    }

    fn read(paths: &Paths, lease: Duration) -> Arc<Nodes> {
        let mut known = read_register(paths);
        for device in known.values_mut() {
            device.connected = false;
        }
        Arc::new_cyclic(|me| Nodes {
            paths: paths.clone(),
            connected: Mutex::default(),
            known: Mutex::new(known),
            waiting: Calls::default(),
            lease,
            me: me.clone(),
        })
    }

    /// The node of the device named `name`, when it is connected.
    pub fn get(&self, name: &str) -> Option<Arc<RemoteNode>> {
        self.connected.lock().unwrap().get(name).cloned()
    }

    /// The device named `name`, for a thread to work at. None when no node of that name is
    /// connected now: a conversation is not opened at a device that is away.
    pub fn place(self: &Arc<Self>, name: &str, folder: &str) -> Option<nypi_core::place::Remote> {
        self.get(name)?;
        let node = Arc::new(ByName { nodes: self.clone(), name: name.to_string() });
        Some(nypi_core::place::Remote { node, folder: folder.to_string() })
    }

    pub fn list(&self) -> BTreeMap<String, Known> {
        self.known.lock().unwrap().clone()
    }

    fn save(&self) {
        let known = self.known.lock().unwrap().clone();
        let path = self.paths.devices();
        let draft = path.with_extension("json.tmp");
        let written = serde_json::to_vec_pretty(&known)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&draft, bytes))
            .and_then(|()| std::fs::rename(&draft, &path));
        if let Err(error) = written {
            log(&format!("the register of devices was not written: {error}"));
        }
    }

    fn note(&self, report: Report, connected: bool) {
        let name = report.identity.name.clone();
        let since = chrono::Utc::now().to_rfc3339();
        self.known.lock().unwrap().insert(name, Known { report, connected, since });
        self.save();
    }

    /// One node's link, from its hello to its end.
    pub async fn link<R, W>(self: &Arc<Self>, read: R, write: W)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        self.link_as(read, write, None).await
    }

    /// `link`, for a node that was paired under `paired` (decision D80): it is that device and
    /// no other, whatever name it gives itself.
    pub async fn link_as<R, W>(self: &Arc<Self>, read: R, write: W, paired: Option<&str>)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        self.link_until(read, write, paired, CancellationToken::new()).await
    }

    /// `link_as`, ending when `stop` is cancelled (the device was unpaired, decision D83), with
    /// the register told as for any link that ends.
    pub async fn link_until<R, W>(
        self: &Arc<Self>,
        read: R,
        mut write: W,
        paired: Option<&str>,
        stop: CancellationToken,
    ) where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut lines = BufReader::new(read);
        let hello = tokio::time::timeout(HELLO_WAIT, read_line(&mut lines, MAX_LINE)).await;
        let Ok(Ok(Some(line))) = hello else { return };
        let Ok(FromNode::Hello { mut report }) = serde_json::from_str::<FromNode>(&line) else {
            return log("a connection to the node socket did not begin with a node's hello");
        };
        if let Some(paired) = paired
            && report.identity.name != paired
        {
            log(&format!("the node paired as {paired} calls itself {}; it is {paired} here", report.identity.name));
            report.identity.name = paired.to_string();
        }
        let name = report.identity.name.clone();
        let (to_node, mut outgoing) = mpsc::unbounded_channel::<ToNode>();
        let node = Arc::new(RemoteNode {
            report: Mutex::new(report.clone()),
            to_node: to_node.clone(),
            waiting: self.waiting.clone(),
            lease: self.lease,
            gone: CancellationToken::new(),
            heard: Mutex::new(tokio::time::Instant::now()),
            asked: Mutex::default(),
            updates: Mutex::default(),
            paired: paired.is_some(),
        });
        // One node per name. A machine's link is that machine's (its key says so), so a new one
        // takes the earlier one's place at once: the machine moved to another way to the hub
        // (decision D94). On the local socket, where a node names itself, a link of that name
        // that still answers is that device, and the newcomer is not; one that stays silent is a
        // dead link its device has come back from.
        if let Some(earlier) = self.get(&name).filter(|_| paired.is_some()) {
            log(&format!("node {name} linked again; its earlier link is closed"));
            earlier.gone.cancel();
        } else if let Some(earlier) = self.get(&name) {
            let asked = tokio::time::Instant::now();
            let _ = earlier.to_node.send(ToNode::Ping);
            let answered = || *earlier.heard.lock().unwrap() >= asked && !earlier.gone.is_cancelled();
            while !answered() && asked.elapsed() < PROBE_WAIT {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if answered() {
                return log(&format!("refused a second node named {name}: one is connected"));
            }
            log(&format!("node {name} came back; its silent link is dropped"));
            earlier.gone.cancel();
        }
        self.connected.lock().unwrap().insert(name.clone(), node.clone());
        log(&format!("node {name} connected ({} tools)", report.tools.len()));
        self.note(report, true);
        let writing = tokio::spawn(async move {
            while let Some(message) = outgoing.recv().await {
                let Ok(mut line) = serde_json::to_vec(&message) else { continue };
                line.push(b'\n');
                if write.write_all(&line).await.is_err() {
                    break;
                }
            }
        });
        let pinging = tokio::spawn(async move {
            let mut ticks = tokio::time::interval(PING_EVERY);
            loop {
                ticks.tick().await;
                if to_node.send(ToNode::Ping).is_err() {
                    break;
                }
            }
        });
        // A node answers a ping, so a link that says nothing for this long is not there; one
        // whose device came back on a new link ends at once.
        loop {
            let line = tokio::select! {
                read = tokio::time::timeout(SILENT_LIMIT, read_line(&mut lines, MAX_LINE)) => match read {
                    Ok(Ok(Some(line))) => line,
                    _ => break,
                },
                _ = node.gone.cancelled() => break,
                _ = stop.cancelled() => break,
            };
            *node.heard.lock().unwrap() = tokio::time::Instant::now();
            match serde_json::from_str::<FromNode>(&line) {
                Ok(FromNode::Hello { report }) if report.identity.name == name => {
                    *node.report.lock().unwrap() = report.clone();
                    self.note(report, true);
                }
                Ok(event) => node.deliver(event),
                // A line the hub does not understand is a newer node's; the rest still is.
                Err(_) => {}
            }
        }
        node.gone.cancel();
        writing.abort();
        pinging.abort();
        // A link replaced by its device's new one leaves the register to the new one.
        let replaced = {
            let mut connected = self.connected.lock().unwrap();
            let current = connected.get(&name).is_some_and(|now| Arc::ptr_eq(now, &node));
            if current {
                connected.remove(&name);
            }
            !current
        };
        if !replaced {
            let last = node.report();
            self.note(last, false);
            log(&format!("node {name} went away"));
        }
    }

    /// The nodes connected over machines' links, by name, with what each last said.
    pub fn paired(&self) -> Vec<(String, Arc<RemoteNode>)> {
        let mut nodes: Vec<(String, Arc<RemoteNode>)> = self
            .connected
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, node)| node.paired)
            .map(|(name, node)| (name.clone(), node.clone()))
            .collect();
        nodes.sort_by(|a, b| a.0.cmp(&b.0));
        nodes
    }

    /// Tell every node that came over a machine's link the hub's epoch and direct addresses
    /// (decision D94).
    pub fn tell_paired(&self, epoch: u64, direct: &[String]) {
        for node in self.connected.lock().unwrap().values().filter(|node| node.paired) {
            let _ = node.to_node.send(ToNode::Hub { epoch, direct: direct.to_vec() });
        }
    }

    /// Take nodes' connections until the listener closes.
    pub async fn serve(self: Arc<Self>, listener: UnixListener) {
        while let Ok((stream, _)) = listener.accept().await {
            let nodes = self.clone();
            tokio::spawn(async move {
                if crate::daemon::started_by_this_process(&stream).await {
                    return log("refused a node connection from a program Lucia started");
                }
                let (read, write) = stream.into_split();
                nodes.link(read, write).await;
            });
        }
    }
}

/// Connect to the hub's node socket (the hub's own path, or one forwarded from another machine).
pub async fn dial(socket: &std::path::Path) -> std::io::Result<UnixStream> {
    UnixStream::connect(socket).await
}

#[cfg(test)]
mod tests {
    use nypi_node::proto::{CallResult, ToolDeclaration};
    use nypi_node::{CallContext, Node, NodeTool};
    use serde_json::{Value, json};

    use super::*;

    struct Echo;

    #[async_trait]
    impl NodeTool for Echo {
        fn declaration(&self) -> ToolDeclaration {
            ToolDeclaration {
                name: "echo".into(),
                description: String::new(),
                parameters: json!({}),
                sequential: false,
            }
        }

        async fn run(&self, arguments: Value, call: CallContext) -> CallResult {
            if arguments["forever"] == true {
                call.cancel.cancelled().await;
                return CallResult::error("withdrawn");
            }
            call.output(CallResult::text("working"));
            CallResult::text(format!("{} in {}", arguments["text"].as_str().unwrap_or_default(), call.folder))
        }
    }

    fn laptop() -> Arc<Node> {
        let mut node = Node::this_computer().named("laptop");
        node.register(Arc::new(Echo));
        Arc::new(node)
    }

    async fn until(what: &str, ready: impl Fn() -> bool) {
        for _ in 0..200 {
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never: {what}");
    }

    async fn all(node: &RemoteNode, id: &str, arguments: Value) -> Vec<FromNode> {
        let (events, mut heard) = mpsc::unbounded_channel();
        let call = Call { id: id.into(), task: None, tool: "echo".into(), arguments, folder: "/site".into() };
        node.call(call, events).await;
        std::iter::from_fn(|| heard.try_recv().ok()).collect()
    }

    #[tokio::test]
    async fn a_node_registers_over_a_socket_answers_calls_and_is_kept_in_the_register_when_away() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let socket = home.path().join("n.sock");
        // A device that does not come back is given up on after the lease.
        let nodes = Nodes::open_with(&paths, Duration::from_millis(300));
        let serving = tokio::spawn(nodes.clone().serve(UnixListener::bind(&socket).unwrap()));

        // A node that is another process connects and says who it is. It gets a runtime of its
        // own here, so that it can die as a process does: everything of it at once.
        let device = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let at = socket.clone();
        device.spawn(async move {
            let (read, write) = dial(&at).await.unwrap().into_split();
            nypi_node::link::serve(laptop(), read, write).await
        });
        until("the node is registered", || nodes.get("laptop").is_some()).await;
        let node = nodes.get("laptop").unwrap();
        assert_eq!(node.report().tools[0].name, "echo");
        let register = read_register(&paths);
        assert!(register["laptop"].connected, "{register:?}");

        // A call goes over the link, and its output and result come back.
        let heard = all(&node, "c-1", json!({ "text": "hello" })).await;
        assert_eq!(
            heard,
            [
                FromNode::Started { id: "c-1".into() },
                FromNode::Output { id: "c-1".into(), partial: CallResult::text("working") },
                FromNode::Finished { id: "c-1".into(), result: CallResult::text("hello in /site") },
            ]
        );
        // The node's own "at most once" holds over the link too.
        let again = all(&node, "c-1", json!({ "text": "twice" })).await;
        assert!(matches!(&again[..], [FromNode::Refused { reason, .. }] if reason.contains("at most once")));

        // A second device claiming the same name is not let in beside the first.
        let (read, write) = dial(&socket).await.unwrap().into_split();
        let imposter = tokio::spawn(nypi_node::link::serve(laptop(), read, write));
        imposter.await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&nodes.get("laptop").unwrap(), &node));

        // The device goes away in the middle of a call: the caller is told, and the register
        // keeps what the device last said, as away.
        let waiting = tokio::spawn({
            let node = node.clone();
            async move { all(&node, "c-2", json!({ "forever": true })).await }
        });
        until("the call is under way", || node.waiting.lock().unwrap().contains_key("c-2")).await;
        device.shutdown_background();
        let heard = waiting.await.unwrap();
        let Some(FromNode::Refused { reason, .. }) = heard.last() else { panic!("{heard:?}") };
        assert!(reason.starts_with("laptop went away during this call"), "{reason}");
        until("the node is marked away", || nodes.get("laptop").is_none()).await;
        let register = read_register(&paths);
        assert_eq!((register["laptop"].connected, register["laptop"].report.tools.len()), (false, 1));
        let late = all(&node, "c-3", json!({ "text": "anyone?" })).await;
        assert!(matches!(&late[..], [FromNode::Refused { reason, .. }] if reason == "laptop is not connected"));

        // A hub that starts again does not believe an old register about who is connected.
        std::fs::write(paths.devices(), serde_json::to_vec(&nodes.list()).unwrap().as_slice()).unwrap();
        let mut stale = nodes.list();
        stale.get_mut("laptop").unwrap().connected = true;
        std::fs::write(paths.devices(), serde_json::to_vec(&stale).unwrap()).unwrap();
        assert!(!Nodes::open(&paths).list()["laptop"].connected);
        serving.abort();
    }

    /// A device whose old link died without a word (a dropped SSH tunnel) is let back in at
    /// once on its new link, not after the old one has been silent for a minute.
    #[tokio::test]
    async fn a_device_back_on_a_new_link_replaces_its_silent_one() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let socket = home.path().join("n.sock");
        let nodes = Nodes::open(&paths);
        let serving = tokio::spawn(nodes.clone().serve(UnixListener::bind(&socket).unwrap()));

        // The old link: it says hello, then nothing ever again, and keeps the socket open.
        let mut dead = dial(&socket).await.unwrap();
        let hello = FromNode::Hello { report: laptop().report() };
        let mut line = serde_json::to_vec(&hello).unwrap();
        line.push(b'\n');
        dead.write_all(&line).await.unwrap();
        until("the old link is registered", || nodes.get("laptop").is_some()).await;
        let old = nodes.get("laptop").unwrap();

        let started = std::time::Instant::now();
        let (read, write) = dial(&socket).await.unwrap().into_split();
        let back = tokio::spawn(nypi_node::link::serve(laptop(), read, write));
        while nodes.get("laptop").is_none_or(|now| Arc::ptr_eq(&now, &old)) {
            assert!(started.elapsed() < Duration::from_secs(10), "the new link never replaced the silent one");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "well before the silent limit");
        assert!(old.gone.is_cancelled());
        // The old link's end leaves the new one in the register, connected.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(nodes.get("laptop").is_some() && read_register(&paths)["laptop"].connected);
        back.abort();
        serving.abort();
        drop(dead);
    }

    /// A fleet update (decision D98): the hub asks a node to update, the node's own updater runs
    /// once its calls are done, and its answer comes back; the version a node says it runs is in
    /// the register; a node with no updater says so.
    #[tokio::test]
    async fn a_node_updates_itself_when_the_hub_asks() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let socket = home.path().join("n.sock");
        let nodes = Nodes::open(&paths);
        tokio::spawn(nodes.clone().serve(UnixListener::bind(&socket).unwrap()));
        let node = laptop();
        let asked = Arc::new(Mutex::new(None::<Option<String>>));
        let seen = asked.clone();
        node.on_update(Arc::new(move |version| {
            *seen.lock().unwrap() = Some(version.clone());
            Box::pin(async move { Ok(format!("{} in place", version.unwrap_or_default())) })
        }));
        node.set_relay(true);
        let (read, write) = dial(&socket).await.unwrap().into_split();
        tokio::spawn(nypi_node::link::serve(node, read, write));
        until("the node is registered", || nodes.get("laptop").is_some()).await;
        let remote = nodes.get("laptop").unwrap();
        let report = remote.report();
        assert_eq!(report.identity.version, env!("CARGO_PKG_VERSION"), "the version it runs");
        assert!(report.relay, "and that it runs the relay");
        assert_eq!(read_register(&paths)["laptop"].report.identity.version, env!("CARGO_PKG_VERSION"));
        let said = remote.update(Some("v0.4.1".into()), Duration::from_secs(5)).await.unwrap();
        assert_eq!(said, "v0.4.1 in place");
        assert_eq!(*asked.lock().unwrap(), Some(Some("v0.4.1".to_string())));

        // A node with no updater says it cannot.
        let plain = Node::this_computer().named("other");
        let (read, write) = dial(&socket).await.unwrap().into_split();
        tokio::spawn(nypi_node::link::serve(Arc::new(plain), read, write));
        until("the other node", || nodes.get("other").is_some()).await;
        let refused = nodes.get("other").unwrap().update(None, Duration::from_secs(5)).await.unwrap_err();
        assert!(refused.contains("cannot update itself"), "{refused}");
    }
}
