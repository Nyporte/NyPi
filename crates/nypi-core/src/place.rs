//! The place a thread's device tools run at (docs/devices.md, decisions D66 and D67 part 5): a
//! node, and a folder on that node's device. Reading a file, running a command, and editing are
//! things a device does, so the agent does not hold those tools: it holds what the place's node
//! declares, and each call goes to the node, which carries it out and says what happened. The
//! tools that are Lucia's own (memory, schedules, goals, delegation, her self-prompt) are not a
//! device's and stay with the runtime.
//!
//! A place on this computer uses the node that lives in this process: the call is a function
//! call, and the tools are the ones in `tools/`, registered into the node from where they are
//! written. A place on another device uses a node the hub reaches over a link (`remote`): the
//! same `Call` and `FromNode` messages, and the tools that node declared.
//!
//! A place can be routed (decision D79). A runtime that was given the hub's devices
//! (`DeviceLookup`) works where its current focus is: a folder on this computer or on a device,
//! set for each run from where the owner was when they asked. Its device tools also take an
//! optional `device`, so one task can use a second device for a call, and `on_device` reaches
//! the tools only some device has. Whichever node carries a call out judges it by its own
//! rules; the hub's Inspector judges the same call against the same target (`CallTarget`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nypi_agent::{AgentEvent, ExecutionMode, Tool, ToolContext, ToolOutput};
use nypi_ai::{ContentBlock, ToolDefinition};
use nypi_node::proto::{Block, Call, CallResult, FromNode, Report, ToolDeclaration};
use nypi_node::{CallContext, Node, NodeLink, NodeTool};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::ledger::TaskId;

/// The tools every computer's node offers (docs/devices.md section 8), besides the command
/// tools its settings add.
pub const DEVICE_TOOLS: [&str; 7] = ["read", "write", "edit", "bash", "grep", "find", "ls"];

/// How long a finished tool's last partial outputs get to be passed on.
const FLUSH: std::time::Duration = std::time::Duration::from_millis(200);

/// A folder on another device, by the device's name: where a conversation at that device works.
/// Its tasks and schedules record it, so a resumed task or a fired prompt goes back to the device
/// and not to the folder where the hub keeps the conversation.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct DeviceFolder {
    pub device: String,
    pub folder: String,
}

impl std::fmt::Display for DeviceFolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} on {}", self.folder, self.device)
    }
}

/// A place on a node the hub reaches over a link: what a runtime is given to work there.
#[derive(Clone)]
pub struct Remote {
    pub node: Arc<dyn NodeLink>,
    /// The folder on that node's device.
    pub folder: String,
}

/// A device the hub knows, as a runtime is told of it.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub name: String,
    /// `computer`, `android`, `iphone`.
    pub kind: String,
    pub system: String,
    /// The user's home folder there, when its node said.
    pub home: Option<String>,
    pub connected: bool,
    pub tools: Vec<ToolDeclaration>,
}

/// The hub's devices, as a runtime reaches them. `nypi-hub` implements it on its register of
/// nodes; nothing here knows how a device is linked.
pub trait DeviceLookup: Send + Sync {
    /// Every device the hub knows besides the machine it runs on, connected or not.
    fn devices(&self) -> Vec<DeviceInfo>;
    /// The node of the device named `device`, or why it cannot be reached now.
    fn node(&self, device: &str) -> Result<Arc<dyn NodeLink>, String>;
}

/// Where a run works: a folder on this computer (`device` is None) or on a named device. It is
/// where the owner was when they asked (the interface's ambient focus), or what a brief names.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Focus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    pub folder: String,
}

impl Focus {
    pub fn here(folder: &Path) -> Focus {
        Focus { device: None, folder: folder.display().to_string() }
    }

    /// The same place as a task or a schedule records it, when it is on another device.
    pub fn remote(&self) -> Option<DeviceFolder> {
        self.device.as_ref().map(|device| DeviceFolder { device: device.clone(), folder: self.folder.clone() })
    }
}

impl std::fmt::Display for Focus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.device {
            Some(device) => write!(f, "{} on {device}", self.folder),
            None => write!(f, "{}", self.folder),
        }
    }
}

/// Where one call is carried out, for whoever judges it: the folder its paths are relative to,
/// and the device when that is not this computer (with the user's home there, when known).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CallTarget {
    pub device: Option<String>,
    pub home: Option<PathBuf>,
    pub folder: PathBuf,
}

/// The tool that reaches what only some device offers.
pub const ON_DEVICE: &str = "on_device";

pub struct Place {
    /// The place's own node: this computer in this process, or the device a remote place is on.
    node: Arc<dyn NodeLink>,
    folder: String,
    /// The task the thread is running, named on each call.
    task: Arc<Mutex<Option<TaskId>>>,
    /// The hub's other devices, when this place is routed.
    devices: Option<Arc<dyn DeviceLookup>>,
    /// This computer as a node that works in whichever folder a call names: what a routed
    /// place uses when its focus is a folder here other than its own.
    here: Option<Arc<dyn NodeLink>>,
    /// Where the current run works, when that is not the place's own node and folder.
    focus: Arc<Mutex<Option<Focus>>>,
    /// Whether the place's own node is the computer the hub runs on.
    local: bool,
}

/// Where a call goes.
struct Route {
    node: Arc<dyn NodeLink>,
    folder: String,
}

/// A target as the place reaches it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Where {
    /// The place's own node and folder.
    Own,
    /// Another folder of the computer the hub runs on.
    Here(String),
    /// A folder on a named device.
    Device(String, String),
}

/// How a call is to be judged (see `Place::judged`).
#[derive(Clone, Debug, PartialEq)]
pub struct Judged {
    pub target: Option<CallTarget>,
    pub inner: Option<(String, Value)>,
}

impl Place {
    /// This computer's node, in this process, at `folder`. Every tool in `tools` that is a
    /// device's (`DEVICE_TOOLS`, and the names in `also`) moves into the node, and its slot gets
    /// the tool the agent calls instead, which sends the call to the place.
    pub fn local(
        folder: &Path,
        tools: &mut [Arc<dyn Tool>],
        also: &HashSet<String>,
        task: Arc<Mutex<Option<TaskId>>>,
    ) -> Arc<Place> {
        let mut node = Node::this_computer();
        let mut moved = Vec::new();
        for (index, tool) in tools.iter().enumerate() {
            let name = tool.definition().name;
            if DEVICE_TOOLS.contains(&name.as_str()) || also.contains(&name) {
                node.register(Arc::new(OnNode(tool.clone())));
                moved.push(index);
            }
        }
        let declared = node.declarations();
        let place = Arc::new(Place {
            node: Arc::new(node),
            folder: folder.display().to_string(),
            task,
            devices: None,
            here: None,
            focus: Arc::default(),
            local: true,
        });
        for index in moved {
            let name = tools[index].definition().name;
            if let Some(declaration) = declared.iter().find(|declaration| declaration.name == name) {
                tools[index] = Arc::new(AtPlace { place: place.clone(), declaration: declaration.clone() });
            }
        }
        place
    }

    /// A folder on a node the hub reaches over a link, and the tools that node declared, as the
    /// agent calls them.
    pub fn remote(
        node: Arc<dyn NodeLink>,
        folder: &str,
        task: Arc<Mutex<Option<TaskId>>>,
    ) -> (Arc<Place>, Vec<Arc<dyn Tool>>) {
        let declared = node.report().tools;
        let place = Arc::new(Place {
            node,
            folder: folder.to_string(),
            task,
            devices: None,
            here: None,
            focus: Arc::default(),
            local: false,
        });
        let tools = declared
            .into_iter()
            .map(|declaration| Arc::new(AtPlace { place: place.clone(), declaration }) as Arc<dyn Tool>)
            .collect();
        (place, tools)
    }

    /// The name of the device this place is on.
    pub fn device(&self) -> String {
        self.node.report().identity.name
    }

    /// What the place's node says about itself: the hub's register of nodes is made of these.
    pub fn report(&self) -> Report {
        self.node.report()
    }

    /// The place's own folder, whatever the current focus.
    pub fn folder(&self) -> &str {
        &self.folder
    }

    /// The same place, routed: its tools (`tools`, as `local` or `remote` left them) take an
    /// optional `device` and follow the focus. `here` is this computer as a node for any folder.
    /// Returns the place and the `on_device` tool.
    pub fn routed(
        self: Arc<Self>,
        tools: &mut [Arc<dyn Tool>],
        devices: Arc<dyn DeviceLookup>,
        here: Option<Arc<dyn NodeLink>>,
    ) -> (Arc<Place>, Arc<dyn Tool>) {
        let place = Arc::new(Place {
            node: self.node.clone(),
            folder: self.folder.clone(),
            task: self.task.clone(),
            devices: Some(devices),
            here,
            focus: self.focus.clone(),
            local: self.local,
        });
        let own: HashSet<String> = place.node.report().tools.into_iter().map(|tool| tool.name).collect();
        for tool in tools.iter_mut() {
            let ToolDefinition { name, description, parameters } = tool.definition();
            if own.contains(&name) {
                let sequential = tool.execution_mode() == ExecutionMode::Sequential;
                let declaration = ToolDeclaration { name, description, parameters, sequential };
                *tool = Arc::new(AtPlace { place: place.clone(), declaration });
            }
        }
        let on_device = Arc::new(OnDevice { place: place.clone() });
        (place, on_device)
    }

    pub fn is_routed(&self) -> bool {
        self.devices.is_some()
    }

    /// How this place reaches the hub's devices, for a runtime that works for it (a native
    /// specialist delegated to at a device, decision D83).
    pub fn lookup(&self) -> Option<Arc<dyn DeviceLookup>> {
        self.devices.clone()
    }

    /// The hub's other devices, as this place knows them.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.devices.as_ref().map(|devices| devices.devices()).unwrap_or_default()
    }

    /// Work at `focus` from now on (None: back at the place's own folder). A place that is not
    /// routed stays where it is.
    pub fn set_focus(&self, focus: Option<Focus>) {
        if self.devices.is_none() {
            return;
        }
        let kept = focus.filter(|focus| self.at(focus) != Where::Own);
        *self.focus.lock().unwrap() = kept;
    }

    /// Where the current run works, when that is not the place's own folder.
    pub fn focus(&self) -> Option<Focus> {
        self.focus.lock().unwrap().clone()
    }

    /// What the folder the run works in says about how to work in it, when that folder is on
    /// another device: read there, by that device's node (decision D80). None for a folder on
    /// this computer (whose files the runtime reads itself), and when the device cannot be asked.
    pub async fn remote_context(&self) -> Option<nypi_node::proto::PlaceContext> {
        let on_a_device = match self.focus() {
            Some(focus) => matches!(self.at(&focus), Where::Device(..)),
            None => !self.local,
        };
        if !on_a_device {
            return None;
        }
        let route = self.route(None).ok()?;
        route.node.place_context(&route.folder).await
    }

    fn own_name(&self) -> String {
        self.node.report().identity.name
    }

    /// The name of the computer the hub runs on, when this place can reach it.
    pub fn here_name(&self) -> Option<String> {
        match (&self.here, self.local) {
            (Some(here), _) => Some(here.report().identity.name),
            (None, true) => Some(self.own_name()),
            (None, false) => None,
        }
    }

    /// Where `focus` is, as this place reaches it.
    fn at(&self, focus: &Focus) -> Where {
        let on_this_computer = match &focus.device {
            None => true,
            Some(device) => self.here_name().as_deref() == Some(device.as_str()),
        };
        if on_this_computer {
            return match self.local && focus.folder == self.folder {
                true => Where::Own,
                false => Where::Here(focus.folder.clone()),
            };
        }
        let device = focus.device.clone().unwrap_or_default();
        match !self.local && device == self.own_name() && focus.folder == self.folder {
            true => Where::Own,
            false => Where::Device(device, focus.folder.clone()),
        }
    }

    /// Where a call goes: to the device it names, else to the focus, else to the place itself.
    fn whereabouts(&self, device: Option<&str>) -> Where {
        let current = self.focus().map_or(Where::Own, |focus| self.at(&focus));
        let Some(device) = device.map(str::trim).filter(|device| !device.is_empty()) else { return current };
        let current_device = match &current {
            Where::Own => Some(self.own_name()),
            Where::Here(_) => self.here_name(),
            Where::Device(name, _) => Some(name.clone()),
        };
        if current_device.as_deref() == Some(device) {
            // Naming the device the run is on already changes nothing.
            return current;
        }
        if self.here_name().as_deref() == Some(device) {
            // This computer, from a run that works elsewhere: the place's own folder when it
            // is here, else the user's home.
            return match self.local {
                true => Where::Own,
                false => Where::Here(dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")).display().to_string()),
            };
        }
        if !self.local && device == self.own_name() {
            return Where::Own;
        }
        // Another device, at the user's home there (the call's paths should be absolute).
        let home = self.devices.as_ref().and_then(|devices| devices.node(device).ok()?.report().identity.home);
        Where::Device(device.to_string(), home.unwrap_or_else(|| "/".to_string()))
    }

    fn route(&self, device: Option<&str>) -> Result<Route, String> {
        match self.whereabouts(device) {
            Where::Own => Ok(Route { node: self.node.clone(), folder: self.folder.clone() }),
            Where::Here(folder) => match &self.here {
                Some(here) => Ok(Route { node: here.clone(), folder }),
                None => Err("this conversation cannot work in another folder of this computer".to_string()),
            },
            Where::Device(name, folder) if !self.local && name == self.own_name() => {
                Ok(Route { node: self.node.clone(), folder })
            }
            Where::Device(name, folder) => match &self.devices {
                Some(devices) => Ok(Route { node: devices.node(&name)?, folder }),
                None => Err(format!("no device named {name} is known here")),
            },
        }
    }

    /// How the Inspector is to judge a call of `tool` with `arguments`, when not simply against
    /// the place's own folder: the target the call is carried out at, and for `on_device` the
    /// tool and arguments it carries. None for a call that stays at the place itself.
    pub fn judged(&self, tool: &str, arguments: &Value) -> Option<Judged> {
        self.devices.as_ref()?;
        let inner = (tool == ON_DEVICE)
            .then(|| (arguments["tool"].as_str().unwrap_or_default().to_string(), arguments["arguments"].clone()));
        let target = match self.whereabouts(arguments["device"].as_str()) {
            Where::Own => None,
            Where::Here(folder) => Some(CallTarget { device: None, home: None, folder: PathBuf::from(folder) }),
            Where::Device(name, folder) => {
                let home = self.devices.as_ref().and_then(|devices| devices.node(&name).ok()?.report().identity.home);
                Some(CallTarget { device: Some(name), home: home.map(PathBuf::from), folder: PathBuf::from(folder) })
            }
        };
        // `on_device` may name a folder of its own.
        let target = match (target, inner.as_ref(), arguments["folder"].as_str().map(str::trim)) {
            (Some(mut target), Some(_), Some(folder)) if !folder.is_empty() => {
                target.folder = PathBuf::from(folder);
                Some(target)
            }
            (target, _, _) => target,
        };
        (target.is_some() || inner.is_some()).then_some(Judged { target, inner })
    }

    /// Send one call to wherever it goes and wait for its end.
    async fn carry_out(&self, route: Route, tool: &str, arguments: Value, ctx: &ToolContext) -> ToolOutput {
        // New for every call: a node carries out an id once, and a model may repeat its own.
        let id = format!("c-{}", uuid::Uuid::now_v7().simple());
        let call = Call {
            id: id.clone(),
            task: self.task.lock().unwrap().as_ref().map(|task| task.0.clone()),
            tool: tool.to_string(),
            arguments,
            folder: route.folder.clone(),
        };
        let (events, mut heard) = mpsc::unbounded_channel();
        let running = route.node.call(call, events);
        tokio::pin!(running);
        let mut ended = None;
        let mut withdrawn = false;
        let take = |event: FromNode, ended: &mut Option<FromNode>| match event {
            FromNode::Output { partial, .. } => ctx.update(output_of(partial)),
            FromNode::Started { .. } => {}
            end => *ended = Some(end),
        };
        loop {
            tokio::select! {
                _ = &mut running => break,
                Some(event) = heard.recv() => take(event, &mut ended),
                _ = ctx.cancel.cancelled(), if !withdrawn => {
                    withdrawn = true;
                    route.node.cancel(&id);
                }
            }
        }
        while let Ok(event) = heard.try_recv() {
            take(event, &mut ended);
        }
        match ended {
            Some(FromNode::Finished { result, .. }) => output_of(result),
            // The device's own rules said no: a result the model reads, like any refusal.
            Some(FromNode::Refused { reason, .. }) => {
                let device = route.node.report().identity.name;
                ToolOutput::error(format!("{tool} was not carried out on {device}: {reason}"))
            }
            _ => ToolOutput::error(format!("{tool} ended without a result")),
        }
    }
}

/// The `<devices>` section of the prompt: the hub's other devices and what each offers beyond
/// files and the shell. None when there is no other device to speak of.
pub fn devices_section(this_computer: &str, devices: &[DeviceInfo]) -> Option<String> {
    if devices.is_empty() {
        return None;
    }
    let mut lines = vec![format!(
        "Your devices besides this computer ({this_computer}). The file and shell tools take `device` to run one \
         call on a connected computer; `on_device` calls a tool only that device offers. Each device applies its own \
         rules to what you ask of it, and a device that is not connected cannot be reached until it is back."
    )];
    for device in devices {
        let system = if device.system.is_empty() { String::new() } else { format!(", {}", device.system) };
        let home = device.home.as_ref().map(|home| format!(", home {home}")).unwrap_or_default();
        let state = if device.connected { "connected" } else { "not connected" };
        lines.push(format!("- {} ({}{system}): {state}{home}", device.name, device.kind));
        for tool in device.tools.iter().filter(|tool| !DEVICE_TOOLS.contains(&tool.name.as_str())) {
            let description: String = tool.description.split_whitespace().collect::<Vec<_>>().join(" ");
            lines.push(format!("  - {}: {description} Takes: {}", tool.name, tool.parameters));
        }
    }
    Some(lines.join("\n"))
}

/// The `device` parameter a routed place's tools take.
fn with_device_parameter(mut parameters: Value) -> Value {
    if let Some(properties) = parameters.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert(
            "device".into(),
            serde_json::json!({
                "type": "string",
                "description": "Run this call on another connected device (a name from <devices>) instead of where \
                                this task works. Relative paths then start at the user's home there, so give absolute \
                                paths. Leave it out for the task's own place.",
            }),
        );
    }
    parameters
}

/// Take `device` out of a call's arguments: it says where the call goes and is not the tool's.
fn take_device(arguments: &mut Value) -> Option<String> {
    let device = arguments.as_object_mut()?.remove("device")?;
    device.as_str().map(str::to_string)
}

/// A tool written for this machine, as the node runs it.
pub(crate) struct OnNode(pub(crate) Arc<dyn Tool>);

#[async_trait]
impl NodeTool for OnNode {
    fn declaration(&self) -> ToolDeclaration {
        let ToolDefinition { name, description, parameters } = self.0.definition();
        let sequential = self.0.execution_mode() == ExecutionMode::Sequential;
        ToolDeclaration { name, description, parameters, sequential }
    }

    fn prepare(&self, arguments: Value) -> Value {
        self.0.prepare_arguments(arguments)
    }

    async fn run(&self, arguments: Value, call: CallContext) -> CallResult {
        // What the tool streams as partial output goes out as the call's output.
        let (sink, mut updates) = mpsc::unbounded_channel::<AgentEvent>();
        let name = self.0.definition().name;
        let context = ToolContext::new(call.id.clone(), name, call.cancel.clone(), sink);
        let relay = tokio::spawn({
            let call = call.clone();
            async move {
                while let Some(event) = updates.recv().await {
                    if let AgentEvent::ToolExecutionUpdate { partial, .. } = event {
                        call.output(result_of(partial));
                    }
                }
            }
        });
        let output = match self.0.execute(arguments, context).await {
            Ok(output) => output,
            Err(error) => ToolOutput::error(format!("{error:#}")),
        };
        // The tool is done and has let go of its context: what it sent last is passed on first.
        let _ = tokio::time::timeout(FLUSH, relay).await;
        result_of(output)
    }
}

/// A tool of the place, as the agent calls it.
struct AtPlace {
    place: Arc<Place>,
    declaration: ToolDeclaration,
}

#[async_trait]
impl Tool for AtPlace {
    fn definition(&self) -> ToolDefinition {
        let parameters = self.declaration.parameters.clone();
        ToolDefinition {
            name: self.declaration.name.clone(),
            description: self.declaration.description.clone(),
            parameters: if self.place.is_routed() { with_device_parameter(parameters) } else { parameters },
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        if self.declaration.sequential { ExecutionMode::Sequential } else { ExecutionMode::Parallel }
    }

    fn prepare_arguments(&self, mut arguments: Value) -> Value {
        // A node over a link cannot be asked without a round trip, so the device tools' own
        // fix-ups are applied here too; a node in this process applies its tool's as well.
        let name = &self.declaration.name;
        let device = if self.place.is_routed() { take_device(&mut arguments) } else { None };
        let mut prepared = self.place.node.prepare(name, crate::tools::prepare_device_arguments(name, arguments));
        if let (Some(device), Some(object)) = (device, prepared.as_object_mut()) {
            object.insert("device".into(), Value::String(device));
        }
        prepared
    }

    async fn execute(&self, mut arguments: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let place = &self.place;
        let device = if place.is_routed() { take_device(&mut arguments) } else { None };
        Ok(match place.route(device.as_deref()) {
            Ok(route) => place.carry_out(route, &self.declaration.name, arguments, &ctx).await,
            Err(why) => ToolOutput::error(format!("{} was not carried out: {why}", self.declaration.name)),
        })
    }
}

/// Any tool a connected device declares, called by the device's name: how the agent reaches
/// what only that device has (a phone's screen, its notifications).
struct OnDevice {
    place: Arc<Place>,
}

#[async_trait]
impl Tool for OnDevice {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: ON_DEVICE.into(),
            description: "Call a tool that a connected device offers, by the device's name. <devices> lists each \
                          device and the tools only it has, with what they take. For files and the shell on another \
                          device, use the usual tools with `device` instead."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "device": { "type": "string", "description": "The device's name, from <devices>" },
                    "tool": { "type": "string", "description": "The tool's name as that device declares it" },
                    "arguments": { "type": "object", "description": "The tool's arguments" },
                    "folder": { "type": "string", "description": "Folder on the device to work in (default: the user's home there)" }
                },
                "required": ["device", "tool"]
            }),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        // What a device's own tool does is not known here: one at a time.
        ExecutionMode::Sequential
    }

    async fn execute(&self, arguments: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let text = |key: &str| arguments[key].as_str().map(str::trim).filter(|text| !text.is_empty());
        let (Some(device), Some(tool)) = (text("device"), text("tool")) else {
            return Ok(ToolOutput::error("on_device needs `device` and `tool`"));
        };
        let mut route = match self.place.route(Some(device)) {
            Ok(route) => route,
            Err(why) => return Ok(ToolOutput::error(format!("{tool} was not carried out: {why}"))),
        };
        if !route.node.report().tools.iter().any(|declared| declared.name == tool) {
            let offered: Vec<String> = route.node.report().tools.into_iter().map(|declared| declared.name).collect();
            return Ok(ToolOutput::error(format!(
                "{device} offers no tool named {tool}. It offers: {}",
                offered.join(", ")
            )));
        }
        if let Some(folder) = text("folder") {
            route.folder = folder.to_string();
        }
        let inner = match arguments.get("arguments") {
            Some(Value::Object(inner)) => Value::Object(inner.clone()),
            _ => serde_json::json!({}),
        };
        Ok(self.place.carry_out(route, tool, inner, &ctx).await)
    }
}

fn result_of(output: ToolOutput) -> CallResult {
    let content = output
        .content
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(Block::Text { text }),
            ContentBlock::Image { data, mime_type } => Some(Block::Image { data, mime_type }),
            // A tool's result holds what it produced, never a model's own turn.
            ContentBlock::Thinking { .. } | ContentBlock::ToolCall { .. } => None,
        })
        .collect();
    CallResult { content, details: output.details, is_error: output.is_error, terminate: output.terminate }
}

fn output_of(result: CallResult) -> ToolOutput {
    let content = result
        .content
        .into_iter()
        .map(|block| match block {
            Block::Text { text } => ContentBlock::text(text),
            Block::Image { data, mime_type } => ContentBlock::Image { data, mime_type },
        })
        .collect();
    ToolOutput { content, details: result.details, is_error: result.is_error, terminate: result.terminate }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nypi_ai::text_of;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    /// A device tool that streams, fixes up its arguments, and can fail or wait.
    struct Count;

    #[async_trait]
    impl Tool for Count {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "ls".into(),
                description: "List a folder".into(),
                parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            }
        }

        fn execution_mode(&self) -> ExecutionMode {
            ExecutionMode::Sequential
        }

        fn prepare_arguments(&self, arguments: Value) -> Value {
            match arguments.get("dir").cloned() {
                Some(dir) => json!({ "path": dir }),
                None => arguments,
            }
        }

        async fn execute(&self, arguments: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
            match arguments["path"].as_str().unwrap_or_default() {
                "missing" => anyhow::bail!("no such folder"),
                "slow" => {
                    ctx.cancel.cancelled().await;
                    Ok(ToolOutput::error("stopped"))
                }
                path => {
                    ctx.update(ToolOutput::text("reading"));
                    Ok(ToolOutput::text(format!("2 entries in {path}")).with_details(json!({ "entries": 2 })))
                }
            }
        }
    }

    /// Lucia's own: not a device's, so it stays as it is.
    struct Remember;

    #[async_trait]
    impl Tool for Remember {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition { name: "memory".into(), description: String::new(), parameters: json!({}) }
        }

        async fn execute(&self, _: Value, _: ToolContext) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput::text("remembered"))
        }
    }

    fn context(cancel: CancellationToken) -> (ToolContext, mpsc::UnboundedReceiver<AgentEvent>) {
        let (sink, events) = mpsc::unbounded_channel();
        (ToolContext::new("t1".into(), "ls".into(), cancel, sink), events)
    }

    #[tokio::test]
    async fn a_device_tool_is_called_through_the_place_and_behaves_as_before() {
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Count), Arc::new(Remember)];
        let task = Arc::new(Mutex::new(Some(TaskId("t-1".into()))));
        let place = Place::local(Path::new("/site"), &mut tools, &HashSet::new(), task);

        // The node declares the device's tool; Lucia's own is not on the node.
        let report = place.report();
        assert_eq!(report.tools.len(), 1);
        assert_eq!((report.tools[0].name.as_str(), report.tools[0].sequential), ("ls", true));
        assert_eq!(place.folder(), "/site");

        // The agent sees the same tool: definition, mode, and the fix-up of near-miss arguments.
        let ls = &tools[0];
        assert_eq!(ls.definition().description, "List a folder");
        assert_eq!(ls.execution_mode(), ExecutionMode::Sequential);
        assert_eq!(ls.prepare_arguments(json!({ "dir": "src" })), json!({ "path": "src" }));
        assert_eq!(tools[1].definition().name, "memory");

        // A call streams its partial output and returns the result with its details.
        let (ctx, mut events) = context(CancellationToken::new());
        let output = ls.execute(json!({ "path": "src" }), ctx).await.unwrap();
        assert_eq!((text_of(&output.content).as_str(), output.is_error), ("2 entries in src", false));
        assert_eq!(output.details, Some(json!({ "entries": 2 })));
        let Some(AgentEvent::ToolExecutionUpdate { call_id, partial, .. }) = events.recv().await else { panic!() };
        assert_eq!((call_id.as_str(), text_of(&partial.content).as_str()), ("t1", "reading"));

        // A failure is a result the model reads; the same model call id twice is two calls.
        let (ctx, _events) = context(CancellationToken::new());
        let failed = ls.execute(json!({ "path": "missing" }), ctx).await.unwrap();
        assert_eq!((text_of(&failed.content).as_str(), failed.is_error), ("no such folder", true));

        // Cancelling the run withdraws the call from the node, and the tool ends it.
        let cancel = CancellationToken::new();
        let (ctx, _events) = context(cancel.clone());
        let waiting = tokio::spawn({
            let ls = ls.clone();
            async move { ls.execute(json!({ "path": "slow" }), ctx).await.unwrap() }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(place.report().running.len(), 1);
        cancel.cancel();
        let stopped = waiting.await.unwrap();
        assert_eq!(text_of(&stopped.content), "stopped");
        assert!(place.report().running.is_empty());
    }
}
