//! A node on a link (docs/devices.md section 7): the node says who it is and what it offers,
//! then takes the hub's calls, carries them out side by side, and says what became of each.
//! One message is one line of JSON, in both directions, no longer than `MAX_LINE`. The link is any pair of byte streams:
//! a Unix socket today, forwarded over SSH from another computer; an encrypted connection of
//! its own once devices are paired.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use nypi_node_proto::{FromNode, ToNode};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::Node;

/// How long calls go on after their link drops, for the hub to come back on another one (an
/// SSH tunnel that flapped, a laptop lid shut for a moment). Their results wait in the journal.
pub const LEASE: Duration = Duration::from_secs(120);
/// The longest line either end takes: a call's result with an image fits, a runaway does not.
pub const MAX_LINE: usize = 32 << 20;

/// One line of `reader`, without its newline; None at the end. A line longer than `limit`
/// ends the link with an error instead of filling memory.
pub async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R, limit: usize) -> std::io::Result<Option<String>> {
    let mut line = Vec::new();
    let read = (&mut *reader).take(limit as u64 + 1).read_until(b'\n', &mut line).await?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > limit {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "a line longer than the link takes"));
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    String::from_utf8(line).map(Some).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Serve `node` over the link until the hub closes it. Calls still running then go on for
/// `LEASE`; if no other link has come up by then, they are withdrawn.
pub async fn serve<R, W>(node: Arc<Node>, read: R, write: W) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_with_lease(node, read, write, LEASE).await
}

/// `serve`, with calls given `lease` to outlive the link.
pub async fn serve_with_lease<R, W>(node: Arc<Node>, read: R, mut write: W, lease: Duration) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let generation = node.links.fetch_add(1, Ordering::SeqCst) + 1;
    let (events, mut outgoing) = mpsc::unbounded_channel::<FromNode>();
    let _ = events.send(FromNode::Hello { report: node.report() });
    // What ended while no link was up, or before the hub acknowledged it.
    for result in node.unacknowledged() {
        let _ = events.send(result);
    }
    *node.current.lock().unwrap() = Some(events.clone());
    // One writer, so two calls' messages never interleave within a line.
    let writing = tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            let Ok(mut line) = serde_json::to_vec(&message) else { continue };
            line.push(b'\n');
            if write.write_all(&line).await.is_err() {
                break;
            }
        }
    });
    let mut reader = BufReader::new(read);
    let ended = loop {
        let line = match read_line(&mut reader, MAX_LINE).await {
            Ok(Some(line)) => line,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        };
        // A line this node does not understand is a newer hub's; the rest still is.
        let Ok(message) = serde_json::from_str::<ToNode>(&line) else { continue };
        match message {
            ToNode::Call(call) => {
                // A call's messages go to whichever link is up when they are said.
                let (relay, mut said) = mpsc::unbounded_channel::<FromNode>();
                let to_link = node.clone();
                tokio::spawn(async move {
                    while let Some(event) = said.recv().await {
                        if let Some(link) = to_link.current.lock().unwrap().as_ref() {
                            let _ = link.send(event);
                        }
                    }
                });
                let node = node.clone();
                tokio::spawn(async move { node.call(call, relay).await });
            }
            ToNode::Cancel { id } => node.cancel(&id),
            ToNode::Ack { id } => node.ack(&id),
            ToNode::Ping => {
                let _ = events.send(FromNode::Pong);
            }
            ToNode::Hub { epoch, direct } => node.hub_said(epoch, direct),
            ToNode::Update { id, version } => {
                let (node, events) = (node.clone(), events.clone());
                tokio::spawn(async move {
                    let (ok, message) = match node.update(version).await {
                        Ok(message) => (true, message),
                        Err(message) => (false, message),
                    };
                    let _ = events.send(FromNode::Updated { id, ok, message });
                });
            }
            ToNode::ReadPlaceContext { id, folder } => {
                // Reading files may take a moment; the link goes on meanwhile.
                let (node, events) = (node.clone(), events.clone());
                tokio::task::spawn_blocking(move || {
                    let _ = events.send(FromNode::PlaceContext { id, context: node.place_context(&folder) });
                });
            }
        }
    };
    {
        let mut current = node.current.lock().unwrap();
        if node.links.load(Ordering::SeqCst) == generation {
            *current = None;
        }
    }
    writing.abort();
    // Calls go on for the lease; withdrawn after it unless another link took this one's place.
    let waiting = node.clone();
    tokio::spawn(async move {
        tokio::time::sleep(lease).await;
        if waiting.links.load(Ordering::SeqCst) == generation {
            waiting.cancel_all();
        }
    });
    ended
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use nypi_node_proto::{Call, CallResult, ToolDeclaration};
    use serde_json::{Value, json};

    use super::*;
    use crate::{CallContext, NodeTool};

    struct Wait;

    #[async_trait]
    impl NodeTool for Wait {
        fn declaration(&self) -> ToolDeclaration {
            ToolDeclaration {
                name: "wait".into(),
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
            CallResult::text(format!("done in {}", call.folder))
        }
    }

    #[tokio::test]
    async fn a_node_on_a_link_says_hello_runs_calls_side_by_side_and_lets_go_when_the_hub_does() {
        let mut node = Node::this_computer().named("laptop");
        node.register(Arc::new(Wait));
        let node = Arc::new(node);
        let (hub, device) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(device);
        let serving = tokio::spawn(serve_with_lease(node.clone(), read, write, Duration::from_millis(100)));
        let (from_node, mut to_node) = tokio::io::split(hub);
        let mut heard = BufReader::new(from_node).lines();
        async fn say(to_node: &mut (impl AsyncWrite + Unpin), message: ToNode) {
            to_node.write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes()).await.unwrap();
        }
        async fn next(heard: &mut tokio::io::Lines<impl tokio::io::AsyncBufRead + Unpin>) -> FromNode {
            serde_json::from_str::<FromNode>(&heard.next_line().await.unwrap().unwrap()).unwrap()
        }

        let FromNode::Hello { report } = next(&mut heard).await else { panic!("a node says who it is first") };
        assert_eq!((report.identity.name.as_str(), report.tools[0].name.as_str()), ("laptop", "wait"));

        // A call that waits does not hold up the next one.
        let call = |id: &str, arguments: Value| {
            ToNode::Call(Call { id: id.into(), task: None, tool: "wait".into(), arguments, folder: "/site".into() })
        };
        say(&mut to_node, call("c-1", json!({ "forever": true }))).await;
        assert_eq!(next(&mut heard).await, FromNode::Started { id: "c-1".into() });
        say(&mut to_node, ToNode::Ping).await;
        assert_eq!(next(&mut heard).await, FromNode::Pong);
        say(&mut to_node, call("c-2", json!({}))).await;
        assert_eq!(next(&mut heard).await, FromNode::Started { id: "c-2".into() });
        let finished = FromNode::Finished { id: "c-2".into(), result: CallResult::text("done in /site") };
        assert_eq!(next(&mut heard).await, finished);
        to_node.write_all(b"{\"type\":\"something new\"}\n").await.unwrap();

        say(&mut to_node, ToNode::Cancel { id: "c-1".into() }).await;
        let withdrawn = FromNode::Finished { id: "c-1".into(), result: CallResult::error("withdrawn") };
        assert_eq!(next(&mut heard).await, withdrawn);

        // The hub goes away with a call under way: after the lease the call is withdrawn, and
        // the node is free.
        say(&mut to_node, call("c-3", json!({ "forever": true }))).await;
        assert_eq!(next(&mut heard).await, FromNode::Started { id: "c-3".into() });
        drop(to_node);
        drop(heard);
        serving.await.unwrap().unwrap();
        for _ in 0..100 {
            if node.report().running.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(node.report().running.is_empty());
    }

    /// The link drops while a call runs: the call goes on, and its result is handed over on the
    /// next link, until the hub acknowledges it. Asked again under the same id, the node answers
    /// with that result rather than running the call twice.
    #[tokio::test]
    async fn a_call_outlives_a_dropped_link_and_its_result_is_handed_over_on_the_next() {
        struct Slow;
        #[async_trait]
        impl NodeTool for Slow {
            fn declaration(&self) -> ToolDeclaration {
                ToolDeclaration {
                    name: "build".into(),
                    description: String::new(),
                    parameters: json!({}),
                    sequential: false,
                }
            }
            async fn run(&self, _arguments: Value, _call: CallContext) -> CallResult {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                CallResult::text("built")
            }
        }
        let mut node = Node::this_computer().named("laptop");
        node.register(Arc::new(Slow));
        let node = Arc::new(node);
        async fn link(
            node: &Arc<Node>,
        ) -> (
            tokio::io::WriteHalf<tokio::io::DuplexStream>,
            tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        ) {
            let (hub, device) = tokio::io::duplex(1 << 16);
            let (read, write) = tokio::io::split(device);
            tokio::spawn(serve_with_lease(node.clone(), read, write, std::time::Duration::from_secs(10)));
            let (from_node, to_node) = tokio::io::split(hub);
            (to_node, BufReader::new(from_node).lines())
        }
        async fn next(heard: &mut tokio::io::Lines<impl tokio::io::AsyncBufRead + Unpin>) -> FromNode {
            serde_json::from_str::<FromNode>(&heard.next_line().await.unwrap().unwrap()).unwrap()
        }
        async fn say(to_node: &mut (impl AsyncWrite + Unpin), message: ToNode) {
            to_node.write_all(format!("{}\n", serde_json::to_string(&message).unwrap()).as_bytes()).await.unwrap();
        }
        let call = ToNode::Call(Call {
            id: "c-1".into(),
            task: None,
            tool: "build".into(),
            arguments: json!({}),
            folder: "/site".into(),
        });

        let (mut to_node, mut heard) = link(&node).await;
        assert!(matches!(next(&mut heard).await, FromNode::Hello { .. }));
        say(&mut to_node, call.clone()).await;
        assert_eq!(next(&mut heard).await, FromNode::Started { id: "c-1".into() });
        drop(to_node);
        drop(heard);
        // The call ends while no link is up.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let finished = FromNode::Finished { id: "c-1".into(), result: CallResult::text("built") };

        let (mut to_node, mut heard) = link(&node).await;
        assert!(matches!(next(&mut heard).await, FromNode::Hello { .. }));
        assert_eq!(next(&mut heard).await, finished, "handed over on the new link");
        say(&mut to_node, call).await;
        assert_eq!(next(&mut heard).await, finished, "asked again, the answer and not a second run");
        say(&mut to_node, ToNode::Ack { id: "c-1".into() }).await;
        say(&mut to_node, ToNode::Ping).await;
        assert_eq!(next(&mut heard).await, FromNode::Pong);
        assert!(node.unacknowledged().is_empty(), "acknowledged, it is let go");
    }
}
