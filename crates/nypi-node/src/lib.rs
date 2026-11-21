//! The node (docs/devices.md, decision D66): what carries out a hub's calls on one device. It
//! holds the tools that device offers, declares them to the hub, and runs each call it is sent,
//! once, under the device's own rules. It has no model, no session, and no provider code, so it
//! builds for a phone as well as a computer: a device's tools register into it from wherever
//! they are written.
//!
//! On the machine the hub runs on, the hub's node is in the hub's process, and calls reach it
//! by a function call (`Node::call`). A node that is a process of its own serves the same
//! messages over a link (`link::serve`), one JSON line each. Either way the hub sees a
//! `NodeLink`. Its journal (`journal`) keeps every call id it took and each result until the
//! hub has it, so a call outlives a dropped link and its result is handed over on the next one.
//! Not here yet: pairing over a network.

pub mod identity;
mod journal;
pub mod link;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
pub use nypi_node_proto as proto;
use nypi_node_proto::{Call, CallResult, FromNode, Identity, Report, ToolDeclaration};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// What a tool gets with a call besides its arguments.
#[derive(Clone)]
pub struct CallContext {
    pub id: String,
    /// The folder of the thread the call comes from.
    pub folder: String,
    /// Cancelled when the hub withdraws the call.
    pub cancel: CancellationToken,
    events: mpsc::UnboundedSender<FromNode>,
}

impl CallContext {
    /// Say what the call has produced so far.
    pub fn output(&self, partial: CallResult) {
        let _ = self.events.send(FromNode::Output { id: self.id.clone(), partial });
    }
}

/// A tool of a device. A failure is a result the model reads, never a panic.
#[async_trait]
pub trait NodeTool: Send + Sync {
    fn declaration(&self) -> ToolDeclaration;

    /// Put near-miss arguments into the declared shape, before the hub checks them against it.
    fn prepare(&self, arguments: Value) -> Value {
        arguments
    }

    async fn run(&self, arguments: Value, call: CallContext) -> CallResult;
}

/// A node as the hub reaches it: in the hub's own process, or over a link.
#[async_trait]
pub trait NodeLink: Send + Sync {
    /// What the node last said about itself.
    fn report(&self) -> Report;

    /// A tool's own fix-up of near-miss arguments, where the node can be asked without a round trip.
    fn prepare(&self, _tool: &str, arguments: Value) -> Value {
        arguments
    }

    /// Have the node carry out `call`; what happens comes through `events`, ending with
    /// `finished` or `refused`. Returns when the call is over.
    async fn call(&self, call: Call, events: mpsc::UnboundedSender<FromNode>);

    /// Withdraw a call that is running.
    fn cancel(&self, id: &str);

    /// What `folder` on the node's device says about how to work in it (decision D80). None
    /// when the node cannot be asked now.
    async fn place_context(&self, _folder: &str) -> Option<proto::PlaceContext> {
        None
    }
}

#[async_trait]
impl NodeLink for Node {
    fn report(&self) -> Report {
        Node::report(self)
    }

    fn prepare(&self, tool: &str, arguments: Value) -> Value {
        Node::prepare(self, tool, arguments)
    }

    async fn call(&self, call: Call, events: mpsc::UnboundedSender<FromNode>) {
        Node::call(self, call, events).await
    }

    fn cancel(&self, id: &str) {
        Node::cancel(self, id)
    }

    async fn place_context(&self, folder: &str) -> Option<proto::PlaceContext> {
        Some(Node::place_context(self, folder))
    }
}

/// The device's own last word on a call: Some(why) refuses it. The hub asks the owner; a node
/// only ever refuses.
pub type Gate = Arc<dyn Fn(&Call) -> Option<String> + Send + Sync>;

/// What a folder on this device says about how to work in it (its context files, and what
/// else the device chooses to tell), read by whoever built the node.
pub type ContextReader = Arc<dyn Fn(&str) -> proto::PlaceContext + Send + Sync>;

pub struct Node {
    identity: Identity,
    tools: Vec<Arc<dyn NodeTool>>,
    gate: Option<Gate>,
    context: Option<ContextReader>,
    /// Every call id this node has taken (an id is carried out at most once) and the results
    /// the hub has not acknowledged.
    journal: Mutex<journal::Journal>,
    running: Mutex<HashMap<String, CancellationToken>>,
    /// Where a call's messages go now: the link that is up, which may not be the one the call
    /// came on (`link::serve`).
    current: Mutex<Option<mpsc::UnboundedSender<FromNode>>>,
    /// Counts the links served, so a link that ended can tell whether another took its place.
    links: std::sync::atomic::AtomicU64,
    /// Told what the hub says of itself (`ToNode::Hub`): its epoch and direct addresses.
    hub_news: Mutex<Option<HubNews>>,
    /// How this machine updates itself when the hub asks (`ToNode::Update`, decision D98).
    updater: Mutex<Option<Updater>>,
    /// This machine runs the relay too.
    relay: std::sync::atomic::AtomicBool,
}

/// What a node does when the hub asks it to update: the program checks and puts in place the
/// release (`version`, or the latest), and says what it did, or why not.
pub type Updater = Arc<
    dyn Fn(Option<String>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// What a node does with the hub's news of itself: the program keeps it in `instance.json`.
pub type HubNews = Arc<dyn Fn(u64, Vec<String>) + Send + Sync>;

impl Node {
    pub fn new(identity: Identity) -> Self {
        Node {
            identity,
            tools: Vec::new(),
            gate: None,
            context: None,
            journal: Mutex::default(),
            running: Mutex::default(),
            current: Mutex::default(),
            hub_news: Mutex::default(),
            updater: Mutex::default(),
            relay: std::sync::atomic::AtomicBool::new(false),
            links: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// This computer, as the node in the hub's own process names itself.
    pub fn this_computer() -> Self {
        let name =
            std::env::var("HOSTNAME").ok().filter(|name| !name.is_empty()).unwrap_or_else(|| "this computer".into());
        Node::new(Identity {
            name,
            kind: proto::DeviceKind::Computer,
            system: std::env::consts::OS.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            home: std::env::var("HOME").ok().filter(|home| !home.is_empty()),
        })
    }

    /// Offer `tool`. A later tool of the same name replaces an earlier one.
    pub fn register(&mut self, tool: Arc<dyn NodeTool>) {
        let name = tool.declaration().name;
        self.tools.retain(|known| known.declaration().name != name);
        self.tools.push(tool);
    }

    /// Keep the journal in `file`, so taken ids and unacknowledged results outlive the process.
    pub fn with_journal(self, file: std::path::PathBuf) -> Self {
        *self.journal.lock().unwrap() = journal::Journal::open(file);
        self
    }

    /// Hand the hub's news of itself to `news` (decision D94).
    pub fn on_hub_news(&self, news: HubNews) {
        *self.hub_news.lock().unwrap() = Some(news);
    }

    /// Update this machine with `updater` when the hub asks (decision D98).
    pub fn on_update(&self, updater: Updater) {
        *self.updater.lock().unwrap() = Some(updater);
    }

    /// Say in the report that this machine runs the relay too.
    pub fn set_relay(&self, relay: bool) {
        self.relay.store(relay, std::sync::atomic::Ordering::SeqCst);
    }

    /// Update as the hub asked: once the calls under way have ended (or after a minute), with
    /// the program's updater. Its answer, or why there is none.
    pub(crate) async fn update(&self, version: Option<String>) -> Result<String, String> {
        let updater = self.updater.lock().unwrap().clone();
        let Some(updater) = updater else { return Err("this node cannot update itself".into()) };
        for _ in 0..600 {
            if self.running.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        updater(version).await
    }

    pub(crate) fn hub_said(&self, epoch: u64, direct: Vec<String>) {
        let news = self.hub_news.lock().unwrap().clone();
        if let Some(news) = news {
            news(epoch, direct);
        }
    }

    /// The hub has the result of call `id`.
    pub fn ack(&self, id: &str) {
        self.journal.lock().unwrap().ack(id);
    }

    /// The results the hub has not acknowledged, for a new link to hand over.
    pub fn unacknowledged(&self) -> Vec<FromNode> {
        self.journal.lock().unwrap().unacknowledged()
    }

    pub fn with_gate(mut self, gate: Gate) -> Self {
        self.gate = Some(gate);
        self
    }

    pub fn declarations(&self) -> Vec<ToolDeclaration> {
        self.tools.iter().map(|tool| tool.declaration()).collect()
    }

    /// What this node tells the hub about itself.
    pub fn report(&self) -> Report {
        let mut running: Vec<String> = self.running.lock().unwrap().keys().cloned().collect();
        running.sort();
        let relay = self.relay.load(std::sync::atomic::Ordering::SeqCst);
        Report { identity: self.identity.clone(), tools: self.declarations(), running, relay }
    }

    fn tool(&self, name: &str) -> Option<&Arc<dyn NodeTool>> {
        self.tools.iter().find(|tool| tool.declaration().name == name)
    }

    /// A tool's own fix-up of near-miss arguments; unknown tools leave them as they are.
    pub fn prepare(&self, tool: &str, arguments: Value) -> Value {
        match self.tool(tool) {
            Some(tool) => tool.prepare(arguments),
            None => arguments,
        }
    }

    /// Carry out `call` and say what happened through `events`: `started`, any `output`, then
    /// `finished`; or `refused`, when the id was seen before, the tool is not offered, or the
    /// device's own rules say no. Returns when the call is over.
    pub async fn call(&self, call: Call, events: mpsc::UnboundedSender<FromNode>) {
        // The last word on a call is journaled before it is said, so it is never lost.
        let last = |event: FromNode| {
            self.journal.lock().unwrap().ended(&event);
            let _ = events.send(event);
        };
        let refuse = |reason: String| last(FromNode::Refused { id: call.id.clone(), reason });
        let taken = self.journal.lock().unwrap().take(&call.id);
        if !taken {
            // Asked again about a call that ended (the hub lost the link before the answer):
            // the answer, not a second run.
            let known = self.journal.lock().unwrap().result(&call.id);
            if let Some(result) = known {
                let _ = events.send(result);
                return;
            }
            return refuse("this call was already taken; a call is carried out at most once".into());
        }
        let Some(tool) = self.tool(&call.tool) else {
            return refuse(format!("{} is not a tool of {}", call.tool, self.identity.name));
        };
        if let Some(reason) = self.gate.as_ref().and_then(|gate| gate(&call)) {
            return refuse(reason);
        }
        let cancel = CancellationToken::new();
        self.running.lock().unwrap().insert(call.id.clone(), cancel.clone());
        let _ = events.send(FromNode::Started { id: call.id.clone() });
        let context = CallContext { id: call.id.clone(), folder: call.folder.clone(), cancel, events: events.clone() };
        // The hub may not have known this tool's fix-ups (a newer node, a phone's own tool).
        let result = tool.run(tool.prepare(call.arguments.clone()), context).await;
        self.running.lock().unwrap().remove(&call.id);
        last(FromNode::Finished { id: call.id.clone(), result });
    }

    /// Withdraw a call that is running. The tool is told, and ends the call in its own time.
    pub fn cancel(&self, id: &str) {
        if let Some(cancel) = self.running.lock().unwrap().get(id) {
            cancel.cancel();
        }
    }

    /// Withdraw every call that is running (the link to the hub is gone, and nothing is kept
    /// of a result nobody can collect yet).
    pub fn cancel_all(&self) {
        for cancel in self.running.lock().unwrap().values() {
            cancel.cancel();
        }
    }

    /// Answer the hub's questions about a folder with `reader`.
    pub fn with_context(mut self, reader: ContextReader) -> Self {
        self.context = Some(reader);
        self
    }

    /// What `folder` on this device says about how to work in it; nothing, for a node that
    /// was given no way to read it.
    pub fn place_context(&self, folder: &str) -> proto::PlaceContext {
        self.context.as_ref().map(|read| read(folder)).unwrap_or_default()
    }

    /// The name the device goes by.
    pub fn name(&self) -> &str {
        &self.identity.name
    }

    /// Take another name than the machine's own (the owner's word for this device).
    pub fn named(mut self, name: &str) -> Self {
        self.identity.name = name.to_string();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Echoes its argument, in two pieces; `wait` holds it until it is cancelled.
    struct Echo;

    #[async_trait]
    impl NodeTool for Echo {
        fn declaration(&self) -> ToolDeclaration {
            ToolDeclaration {
                name: "echo".into(),
                description: "Say it back".into(),
                parameters: json!({ "type": "object", "properties": { "text": { "type": "string" } } }),
                sequential: false,
            }
        }

        fn prepare(&self, arguments: Value) -> Value {
            match arguments {
                Value::String(text) => json!({ "text": text }),
                other => other,
            }
        }

        async fn run(&self, arguments: Value, call: CallContext) -> CallResult {
            let text = arguments["text"].as_str().unwrap_or_default().to_string();
            if arguments["wait"] == true {
                call.cancel.cancelled().await;
                return CallResult::error("cancelled");
            }
            call.output(CallResult::text("working"));
            CallResult::text(format!("{text} (in {})", call.folder))
        }
    }

    fn call(id: &str, tool: &str, arguments: Value) -> Call {
        Call { id: id.into(), task: None, tool: tool.into(), arguments, folder: "/site".into() }
    }

    async fn all(node: &Node, call: Call) -> Vec<FromNode> {
        let (events, mut heard) = mpsc::unbounded_channel();
        node.call(call, events).await;
        std::iter::from_fn(|| heard.try_recv().ok()).collect()
    }

    #[tokio::test]
    async fn a_node_declares_its_tools_and_carries_out_a_call_once() {
        let mut node = Node::this_computer();
        node.register(Arc::new(Echo));
        node.register(Arc::new(Echo));
        let report = node.report();
        assert_eq!(report.tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["echo"]);
        assert_eq!(report.identity.kind, proto::DeviceKind::Computer);
        assert_eq!(node.prepare("echo", json!("hi")), json!({ "text": "hi" }));

        let heard = all(&node, call("c-1", "echo", json!({ "text": "hi" }))).await;
        assert_eq!(
            heard,
            [
                FromNode::Started { id: "c-1".into() },
                FromNode::Output { id: "c-1".into(), partial: CallResult::text("working") },
                FromNode::Finished { id: "c-1".into(), result: CallResult::text("hi (in /site)") },
            ]
        );
        // The same id again is answered from the journal while the hub has not acknowledged it,
        // and refused after, whatever it asks for; so is a tool the device does not have.
        let again = all(&node, call("c-1", "echo", json!({ "text": "twice" }))).await;
        assert_eq!(again, [FromNode::Finished { id: "c-1".into(), result: CallResult::text("hi (in /site)") }]);
        node.ack("c-1");
        let again = all(&node, call("c-1", "echo", json!({ "text": "twice" }))).await;
        assert!(matches!(&again[..], [FromNode::Refused { reason, .. }] if reason.contains("at most once")));
        let unknown = all(&node, call("c-2", "tap", json!({}))).await;
        assert!(matches!(&unknown[..], [FromNode::Refused { reason, .. }] if reason.contains("tap is not a tool")));
        assert!(node.report().running.is_empty());
    }

    #[tokio::test]
    async fn the_device_has_the_last_word_and_a_call_can_be_withdrawn() {
        let gate: Gate =
            Arc::new(|call| (call.arguments["text"] == "secret").then(|| "not on this device".to_string()));
        let mut node = Node::this_computer().with_gate(gate);
        node.register(Arc::new(Echo));
        let refused = all(&node, call("c-1", "echo", json!({ "text": "secret" }))).await;
        assert_eq!(refused, [FromNode::Refused { id: "c-1".into(), reason: "not on this device".into() }]);

        let node = Arc::new(node);
        let (events, mut heard) = mpsc::unbounded_channel();
        let running = tokio::spawn({
            let node = node.clone();
            async move { node.call(call("c-2", "echo", json!({ "wait": true })), events).await }
        });
        assert_eq!(heard.recv().await, Some(FromNode::Started { id: "c-2".into() }));
        assert_eq!(node.report().running, ["c-2"]);
        node.cancel("c-2");
        running.await.unwrap();
        assert_eq!(
            heard.recv().await,
            Some(FromNode::Finished { id: "c-2".into(), result: CallResult::error("cancelled") })
        );
    }
}
