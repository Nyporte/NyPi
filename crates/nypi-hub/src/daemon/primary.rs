//! The hub's one conversation (decision D79). Lucia is one, and so is the conversation the
//! owner has with her: every interface attaches to the same thread, whichever folder or device
//! it was opened at, and every record of it reaches every interface. What differs between
//! interfaces is where their owner is, the ambient focus (a folder, and the device it is on):
//! it comes with the connection and with each message, and what the message asks for is done
//! there. The conversation is opened when the hub starts and stays open as long as it runs; its
//! session is kept in `sessions/primary`.

use super::*;
use nypi_hub_proto::AmbientFocus;

/// A folder on another device as one spelling: without `.` steps, repeated or trailing slashes.
pub(super) fn device_folder(device: &str, folder: &str) -> DeviceFolder {
    let path = Path::new(folder);
    let mut clean = PathBuf::new();
    for component in path.components() {
        if component != std::path::Component::CurDir {
            clean.push(component);
        }
    }
    let folder = if clean.as_os_str().is_empty() { folder.to_string() } else { clean.display().to_string() };
    DeviceFolder { device: device.to_string(), folder }
}

pub(crate) struct PrimaryStream {
    thread: Arc<Thread>,
    /// The hub's one pulse bus (decision D67, part 3): the conversation and every job are scopes of it.
    bus: PulseBus,
    /// The devices whose nodes are connected, for a focus on one of them.
    nodes: Mutex<Option<Arc<crate::nodes::Nodes>>>,
    paths: Paths,
    /// The folder the hub itself works in: the focus of an interface that names none.
    home: PathBuf,
}

impl PrimaryStream {
    /// Open the conversation on `runtime`, which holds it for as long as the hub runs.
    pub(crate) fn new(runtime: Runtime) -> Arc<Self> {
        // The conversation's bus is the hub's: jobs join it, and what belongs to no thread (a
        // timer firing) is recorded by the conversation's Inspector.
        let bus = runtime.pulses().root();
        bus.tap(runtime.inspector());
        let (paths, home) = (runtime.paths.clone(), nypi_core::protected::real_path(&runtime.cwd));
        let thread = Arc::new(Thread::start(runtime, Role::Conversation));
        Arc::new(PrimaryStream { thread, bus, nodes: Mutex::new(None), paths, home })
    }

    /// The register of nodes, when the hub has one.
    pub(crate) fn nodes(&self) -> Option<Arc<crate::nodes::Nodes>> {
        self.nodes.lock().unwrap().clone()
    }

    /// Let interfaces say they are at the devices in `nodes`.
    pub(crate) fn with_nodes(&self, nodes: Arc<crate::nodes::Nodes>) {
        *self.nodes.lock().unwrap() = Some(nodes);
    }

    /// `folder` on the device named `device` as a place a job's runtime can work at, when that
    /// device's node is connected now.
    pub(crate) fn remote_place(&self, remote: &DeviceFolder) -> Option<nypi_core::place::Remote> {
        let nodes = self.nodes.lock().unwrap().clone()?;
        nodes.place(&remote.device, &remote.folder)
    }

    /// Whether the device named `device` has a node connected now.
    pub(crate) fn connected(&self, device: &str) -> bool {
        self.nodes.lock().unwrap().as_ref().is_some_and(|nodes| nodes.get(device).is_some())
    }

    /// The hub's bus, as its root handle.
    pub(crate) fn bus(&self) -> PulseBus {
        self.bus.clone()
    }

    /// The conversation.
    pub(crate) fn thread(&self) -> Arc<Thread> {
        self.thread.touch();
        self.thread.clone()
    }

    /// Where an interface that says it is at `cwd` (on `device`, when not this computer) is
    /// taken to be: the hub's own folder without a word, a folder that exists here, or a folder
    /// on a device whose node is connected. Anything else is refused, with the reason.
    pub(crate) fn focus(&self, cwd: Option<&Path>, device: Option<&str>) -> anyhow::Result<AmbientFocus> {
        match (device, cwd) {
            (Some(device), None) => anyhow::bail!("a folder on {device} is needed"),
            (Some(device), Some(folder)) => {
                let connected = self.nodes.lock().unwrap().as_ref().is_some_and(|nodes| nodes.get(device).is_some());
                if !connected {
                    anyhow::bail!("no device named {device} is connected (nypi devices lists them)");
                }
                let remote = device_folder(device, &folder.display().to_string());
                Ok(AmbientFocus { device: Some(remote.device), cwd: PathBuf::from(remote.folder) })
            }
            (None, None) => Ok(AmbientFocus { device: None, cwd: self.home.clone() }),
            (None, Some(cwd)) => {
                let folder = nypi_core::protected::real_path(cwd);
                if !folder.is_dir() {
                    anyhow::bail!("{} is not a folder on this machine", folder.display());
                }
                Ok(AmbientFocus { device: None, cwd: folder })
            }
        }
    }

    /// The focus for work recorded at `cwd` (or at `remote`): a fired schedule, a task taken up
    /// again. Not checked: what cannot be reached is refused when the work calls a tool.
    pub(crate) fn focus_at(&self, cwd: Option<&Path>, remote: Option<&DeviceFolder>) -> AmbientFocus {
        match remote {
            Some(remote) => AmbientFocus { device: Some(remote.device.clone()), cwd: PathBuf::from(&remote.folder) },
            None => AmbientFocus {
                device: None,
                cwd: cwd.map_or_else(|| self.home.clone(), nypi_core::protected::real_path),
            },
        }
    }

    /// What is being worked on now, for an interface that attaches: the ledger's open tasks,
    /// newest first, each with where it works and where its notes are.
    pub(crate) fn tasks(&self) -> Vec<nypi_hub_proto::TaskSummary> {
        const SHOWN: usize = 20;
        let own = self.thread.session().map(str::to_string);
        let mut open: Vec<nypi_core::ledger::TaskRecord> = nypi_core::ledger::Ledger::new(self.paths.ledger())
            .load()
            .into_values()
            .filter(|task| !task.state.is_terminal() && !task.is_interrupted())
            .collect();
        open.sort_by(|a, b| b.heartbeat_at.cmp(&a.heartbeat_at));
        open.into_iter()
            .take(SHOWN)
            .map(|task| nypi_hub_proto::TaskSummary {
                focus: Some(self.focus_at(Some(&task.cwd), task.remote.as_ref())),
                beside: own.as_deref() != Some(task.session_id.as_str()),
                id: task.id.0,
                goal: task.goal,
                state: task.state.label().to_string(),
                workspace: task.workspace,
            })
            .collect()
    }

    /// Interfaces attached, whether anything runs, and how many conversations there are (one).
    pub(crate) fn totals(&self) -> (usize, bool, usize) {
        (self.thread.hub.attached(), self.thread.busy(), 1)
    }
}
