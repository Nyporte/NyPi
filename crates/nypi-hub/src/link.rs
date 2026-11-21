//! A link to one thread of the hub: the connection an interface holds, speaking the RPC
//! protocol over it. It goes to the daemon's socket when the daemon runs, and otherwise to a hub
//! the same process runs for itself (`daemon::Local`), so there is one way of talking to Lucia.
//! What an interface draws and how it reads its keys is not here (nypi-cli/src/client.rs).
//! `deliver` and `thread_session` are for a process that is not an interface: it hands the
//! daemon a prompt that is nobody's words, or asks which conversation a folder's thread holds.

use std::path::Path;

use nypi_core::config::Paths;
use nypi_core::place::DeviceFolder;
use nypi_core::runtime::Runtime;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::daemon::Local;

const NOT_RUNNING: &str = "The hub is not running. Start it for good with: nypi hub up\n\
                           (or run it in a terminal of its own: nypi hub run)";

/// An interface's connection to one thread.
pub struct Link {
    lines: tokio::io::Lines<BufReader<Box<dyn AsyncRead + Send + Unpin>>>,
    write: Box<dyn AsyncWrite + Send + Unpin>,
    /// What the thread said about itself when this interface attached.
    pub hello: Value,
    /// The hub this process runs for itself, when the link does not go to the daemon.
    own: Option<Local>,
}

impl Link {
    fn over(
        read: impl AsyncRead + Send + Unpin + 'static,
        write: impl AsyncWrite + Send + Unpin + 'static,
        own: Option<Local>,
    ) -> Link {
        let read: Box<dyn AsyncRead + Send + Unpin> = Box::new(read);
        Link { lines: BufReader::new(read).lines(), write: Box::new(write), hello: Value::Null, own }
    }

    /// Attach to the daemon's thread at `cwd` (the home thread without one). `answers` says
    /// whether somebody at this interface can answer an approval. None when no daemon listens.
    pub async fn hub(paths: &Paths, cwd: Option<&Path>, answers: bool) -> anyhow::Result<Option<Link>> {
        Link::attach(paths, json!({ "type": "attach", "cwd": cwd, "answers": answers })).await
    }

    /// Attach to the daemon's thread whose place is `folder` on the device named `device`: the
    /// conversation is the hub's, and its files and shell are that device's.
    pub async fn at_device(paths: &Paths, device: &str, folder: &str, answers: bool) -> anyhow::Result<Option<Link>> {
        Link::attach(paths, json!({ "type": "attach", "cwd": folder, "node": device, "answers": answers })).await
    }

    /// Attach to the hub this machine is a personal node of (decision D93), the way a node links
    /// (decision D94): directly when it can, else through the entry. `cwd` is a folder on this machine:
    /// work asked from here is done here. None when this machine is no node of a hub; an error
    /// when it is an exec-only one, which carries out calls and cannot talk.
    pub async fn paired(paths: &Paths, cwd: Option<&Path>, answers: bool) -> anyhow::Result<Option<Link>> {
        let Some(instance) = crate::instance::Instance::load(paths).filter(|instance| !instance.is_hub()) else {
            return Ok(None);
        };
        anyhow::ensure!(
            instance.trust == crate::pairing::Trust::Personal,
            "this machine was admitted as exec-only: it carries out calls, and cannot talk to the hub (nypi \
             --standalone runs a session here)"
        );
        let settings = nypi_core::config::Settings::load(paths, &paths.home).unwrap_or_default();
        let reach = crate::reach::Reach::local();
        let linked = crate::reach::connect(paths, &settings, crate::pairing::Purpose::Talk, &reach).await?;
        let mut link = Link::over(linked.read, linked.write, None);
        link.send(json!({ "type": "attach", "cwd": cwd, "answers": answers })).await?;
        link.greeted().await?;
        Ok(Some(link))
    }

    async fn attach(paths: &Paths, attach: Value) -> anyhow::Result<Option<Link>> {
        let Ok(stream) = UnixStream::connect(paths.hub_socket()).await else { return Ok(None) };
        let (read, write) = stream.into_split();
        let mut link = Link::over(read, write, None);
        link.send(attach).await?;
        link.greeted().await?;
        Ok(Some(link))
    }

    /// Run the hub in this process on `runtime` and attach to its one thread.
    pub async fn local(runtime: Runtime, answers: bool) -> anyhow::Result<Link> {
        let (own, pipe) = Local::start(runtime, answers);
        let (read, write) = tokio::io::split(pipe);
        let mut link = Link::over(read, write, Some(own));
        link.greeted().await?;
        Ok(link)
    }

    async fn greeted(&mut self) -> anyhow::Result<()> {
        match self.next().await? {
            Some(first) if first["type"] == "daemon_hello" => self.hello = first,
            // `daemon_refused` is what a hub of before decision D100 says.
            Some(first) if first["type"] == "hub_refused" || first["type"] == "daemon_refused" => {
                anyhow::bail!(
                    "The hub refused this connection: {}",
                    first["reason"].as_str().unwrap_or("no reason given")
                )
            }
            _ => anyhow::bail!("The connection closed before the greeting"),
        }
        Ok(())
    }

    /// Whether the link goes to a hub in this process, which ends when the interface leaves.
    pub fn is_local(&self) -> bool {
        self.own.is_some()
    }

    /// Whether this thread's Lucia has her chat module in front of the worker.
    pub fn chat(&self) -> bool {
        !self.hello["chat"].is_null()
    }

    pub async fn next(&mut self) -> anyhow::Result<Option<Value>> {
        loop {
            let Some(line) = self.lines.next_line().await? else { return Ok(None) };
            // A line that is not a record is skipped, not fatal: the next one may be the result.
            if let Ok(record) = serde_json::from_str(&line) {
                return Ok(Some(record));
            }
        }
    }

    pub async fn send(&mut self, command: Value) -> anyhow::Result<()> {
        self.write.write_all(format!("{command}\n").as_bytes()).await?;
        Ok(())
    }

    /// Leave the thread. A hub of this process is closed: what runs is stopped, reflections on
    /// the last tasks finish, and what they had to say comes back with the session's file.
    pub async fn leave(self) -> Left {
        let session = self.hello["sessionFile"].as_str().filter(|path| !path.is_empty()).map(str::to_string);
        match self.own {
            Some(own) => Left::Ended { reflections: own.close().await, session },
            None => Left::Detached,
        }
    }
}

/// What leaving a thread came to.
pub enum Left {
    /// The daemon keeps the conversation.
    Detached,
    /// The conversation was this process's own and is over.
    Ended { reflections: Vec<String>, session: Option<String> },
}

impl Left {
    /// The closing lines for a terminal.
    pub fn lines(&self) -> Vec<String> {
        match self {
            Left::Detached => {
                vec!["Detached. Lucia keeps this conversation; come back with nypi in this folder.".into()]
            }
            Left::Ended { reflections, session } => {
                let mut lines: Vec<String> = reflections.iter().map(|summary| format!("[{summary}]")).collect();
                lines.extend(session.iter().map(|path| format!("Session saved: {path}")));
                lines
            }
        }
    }
}

/// A message to Lucia: it starts a run when she is idle and steers the run when she is not.
pub fn message(id: &str, text: &str) -> Value {
    json!({ "id": id, "type": "prompt", "message": text, "streamingBehavior": "steer" })
}

pub fn new_id() -> String {
    format!("c-{}", uuid::Uuid::now_v7().simple())
}

/// Whether a daemon is there to attach to.
pub fn hub_running(paths: &Paths) -> bool {
    crate::daemon::alive(paths, chrono::Utc::now())
}

/// The link `nypi send` and `nypi attach` need: to the daemon, or a word on how to start it.
pub async fn to_daemon(paths: &Paths, answers: bool) -> anyhow::Result<Option<Link>> {
    let link = Link::hub(paths, None, answers).await?;
    if link.is_none() {
        eprintln!("{NOT_RUNNING}");
    }
    Ok(link)
}

/// Hand the thread at `cwd` a prompt that is not the user's own words, named for where it came
/// from. It runs after whatever that thread is doing. True when the daemon took it.
pub async fn deliver(
    paths: &Paths,
    cwd: Option<&Path>,
    text: &str,
    source: &Value,
    origin: &str,
) -> anyhow::Result<bool> {
    deliver_at(paths, cwd, None, text, source, origin).await
}

/// `deliver`, to the thread at `remote` (a folder on another device) when there is one.
pub async fn deliver_at(
    paths: &Paths,
    cwd: Option<&Path>,
    remote: Option<&DeviceFolder>,
    text: &str,
    source: &Value,
    origin: &str,
) -> anyhow::Result<bool> {
    let link = match remote {
        Some(remote) => Link::at_device(paths, &remote.device, &remote.folder, false).await?,
        None => Link::hub(paths, cwd, false).await?,
    };
    let Some(mut link) = link else { return Ok(false) };
    let id = new_id();
    link.send(json!({ "id": id, "type": "prompt", "message": text, "source": source, "origin": origin })).await?;
    while let Some(record) = link.next().await? {
        if record["type"] == "response" && record["id"] == id.as_str() {
            if record["success"] != true {
                anyhow::bail!("the hub did not take it: {}", record["error"].as_str().unwrap_or("no reason given"));
            }
            return Ok(true);
        }
    }
    anyhow::bail!("the hub closed the connection before taking it")
}

/// The conversation the thread at `cwd` holds (its session id), opening the thread if needed:
/// a thread that opens resumes what its conversation's last process left unfinished.
/// At `remote`, the thread is the one at that folder on that device.
pub async fn thread_session(
    paths: &Paths,
    cwd: &Path,
    remote: Option<&DeviceFolder>,
) -> anyhow::Result<Option<String>> {
    let link = match remote {
        Some(remote) => Link::at_device(paths, &remote.device, &remote.folder, false).await?,
        None => Link::hub(paths, Some(cwd), false).await?,
    };
    Ok(link.and_then(|link| link.hello["session"].as_str().map(str::to_string)))
}

/// Tell the running daemon that the register of paired devices changed, so the links of a
/// device no longer in it close now (decision D83). How many it closed; None when no daemon
/// listens.
pub async fn peers_changed(paths: &Paths) -> anyhow::Result<Option<u64>> {
    peers_changed_at(&paths.hub_socket()).await
}

/// `peers_changed`, for the daemon listening on `socket`.
pub async fn peers_changed_at(socket: &Path) -> anyhow::Result<Option<u64>> {
    let Ok(stream) = UnixStream::connect(socket).await else { return Ok(None) };
    let (read, mut write) = stream.into_split();
    write.write_all(format!("{}\n", json!({ "type": "peers_changed" })).as_bytes()).await?;
    let mut lines = BufReader::new(read).lines();
    let answer = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line()).await;
    let Ok(Ok(Some(line))) = answer else { anyhow::bail!("the hub did not answer") };
    let answer: Value = serde_json::from_str(&line)?;
    Ok(answer["closed"].as_u64())
}
