//! What an interface and the hub say to each other, beyond Pi's RPC command records (decision
//! D79). The hub holds one conversation, the primary one. An interface attaches to it and says
//! where its owner is (`AmbientFocus`: a device and a folder), so that what they ask for is done
//! there; every record of the conversation comes back, whoever asked. Serde only, like
//! `nypi-node-proto`: an interface on a phone needs nothing else of NyPi to speak it.
//!
//! The framing is one JSON object per line, as before. `ToHub` names the lines that are NyPi's
//! own (the first line of a connection, a change of focus); every other line is an RPC command
//! record (`prompt`, `steer`, `abort`, `get_state`, ...), which `Command` reads the common part
//! of. A line of an older interface that does not say where it is still reads: its focus is the
//! one its connection attached with.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where an interface's owner is: the folder they have open, and the device it is on when that
/// is not the computer the hub runs on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AmbientFocus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    pub cwd: PathBuf,
}

impl std::fmt::Display for AmbientFocus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.device {
            Some(device) => write!(f, "{}:{}", device, self.cwd.display()),
            None => write!(f, "{}", self.cwd.display()),
        }
    }
}

/// What a connection is to the hub. An interface speaks as the owner (prompts, approvals); a
/// node carries out calls and never speaks for anyone. A connection's role is fixed when it is
/// made and cannot be changed by anything it sends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClientRole {
    #[default]
    Interface,
    Node,
}

fn yes() -> bool {
    true
}

/// A line of NyPi's own that an interface sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToHub {
    /// The first line of a connection: where the interface is, and whether somebody there can
    /// answer an approval. `node` names the device the folder is on (the older spelling of
    /// `AmbientFocus::device`, kept on the wire).
    Attach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
        #[serde(default = "yes")]
        answers: bool,
        #[serde(default)]
        role: ClientRole,
    },
    /// The owner moved: what this connection sends from now on is asked from here.
    Focus {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device: Option<String>,
        cwd: PathBuf,
    },
    /// The register of paired devices changed (`nypi devices remove`): links of devices no
    /// longer in it are closed now (decision D83). Only the first line of a connection to the
    /// owner's socket; the answer is `peers_checked`.
    PeersChanged,
}

impl ToHub {
    pub fn attach(focus: Option<&AmbientFocus>, answers: bool) -> ToHub {
        ToHub::Attach {
            cwd: focus.map(|focus| focus.cwd.clone()),
            node: focus.and_then(|focus| focus.device.clone()),
            answers,
            role: ClientRole::Interface,
        }
    }

    /// The focus an attach line names, when it names a folder.
    pub fn focus(&self) -> Option<AmbientFocus> {
        match self {
            ToHub::Attach { cwd: Some(cwd), node, .. } => Some(AmbientFocus { device: node.clone(), cwd: cwd.clone() }),
            ToHub::Attach { .. } => None,
            ToHub::Focus { device, cwd } => Some(AmbientFocus { device: device.clone(), cwd: cwd.clone() }),
            ToHub::PeersChanged => None,
        }
    }

    /// Read a line as one of NyPi's own, or None for an RPC command record.
    pub fn read(line: &Value) -> Option<ToHub> {
        match line["type"].as_str() {
            Some("attach" | "focus" | "peers_changed") => serde_json::from_value(line.clone()).ok(),
            _ => None,
        }
    }
}

/// The part of an RPC command record the hub reads for every command: its name, and where the
/// owner was when they sent it, when the line says.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Command {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub focus: Option<AmbientFocus>,
}

/// The commands that ask for work, which is done where their sender is.
pub const FOCUSED_COMMANDS: [&str; 4] = ["prompt", "steer", "follow_up", "bash"];

/// Put `focus` on a command record that asks for work and does not say where from.
pub fn with_focus(mut command: Value, focus: &AmbientFocus) -> Value {
    let asks = command["type"].as_str().is_some_and(|kind| FOCUSED_COMMANDS.contains(&kind));
    if asks && command.get("focus").is_none_or(Value::is_null) {
        command["focus"] = serde_json::to_value(focus).unwrap_or(Value::Null);
    }
    command
}

/// A task of the conversation, as the hub lists it for an interface.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskSummary {
    pub id: String,
    /// What it is for, in a line.
    pub goal: String,
    /// `running`, `waiting`, `blocked`, `done`, `failed`, `cancelled`, ...
    pub state: String,
    /// Where it works.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<AmbientFocus>,
    /// Its workspace in the vault, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Whether it works beside the conversation's own run.
    pub beside: bool,
}

/// A line of NyPi's own that the hub sends; every other line is a record of the conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromHub {
    /// The greeting: what the conversation is, and where this connection's owner is taken to be.
    /// The other fields of the record (model, session, channels, ...) are read as they are.
    DaemonHello {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focus: Option<AmbientFocus>,
        #[serde(default)]
        tasks: Vec<TaskSummary>,
        #[serde(flatten)]
        about: serde_json::Map<String, Value>,
    },
    DaemonRefused {
        reason: String,
    },
    /// Records were dropped because this interface read too slowly.
    DaemonLagged {
        missed: u64,
    },
    /// This connection's focus changed, as it asked.
    Focused {
        focus: AmbientFocus,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_lines_of_older_interfaces_still_read() {
        // The attach line as it was before the focus had a name.
        let old = json!({ "type": "attach", "cwd": "/code/site", "answers": false });
        let attach = ToHub::read(&old).unwrap();
        assert_eq!(
            attach,
            ToHub::Attach { cwd: Some("/code/site".into()), node: None, answers: false, role: ClientRole::Interface }
        );
        assert_eq!(attach.focus(), Some(AmbientFocus { device: None, cwd: "/code/site".into() }));
        let remote = ToHub::read(&json!({ "type": "attach", "cwd": "/srv/app", "node": "mini" })).unwrap();
        assert_eq!(remote.focus().unwrap().to_string(), "mini:/srv/app");
        assert!(matches!(remote, ToHub::Attach { answers: true, .. }), "somebody can answer unless it says not");
        assert_eq!(ToHub::read(&json!({ "type": "attach" })).unwrap().focus(), None);
        // What we send is what an older hub reads.
        let focus = AmbientFocus { device: Some("mini".into()), cwd: "/srv/app".into() };
        assert_eq!(
            serde_json::to_value(ToHub::attach(Some(&focus), true)).unwrap(),
            json!({ "type": "attach", "cwd": "/srv/app", "node": "mini", "answers": true, "role": "interface" })
        );
        // An RPC command is not one of NyPi's own lines; its focus rides along when it has one.
        let prompt = json!({ "type": "prompt", "message": "hi" });
        assert_eq!(ToHub::read(&prompt), None);
        let stamped = with_focus(prompt, &focus);
        let command: Command = serde_json::from_value(stamped.clone()).unwrap();
        assert_eq!((command.kind.as_str(), command.focus.as_ref()), ("prompt", Some(&focus)));
        assert_eq!(with_focus(stamped.clone(), &AmbientFocus { device: None, cwd: "/x".into() }), stamped, "kept");
        assert!(with_focus(json!({ "type": "get_state" }), &focus).get("focus").is_none(), "only what asks for work");
    }

    #[test]
    fn a_role_cannot_be_anything_but_the_two() {
        assert!(serde_json::from_value::<ClientRole>(json!("owner")).is_err());
        assert_eq!(serde_json::from_value::<ClientRole>(json!("node")).unwrap(), ClientRole::Node);
        let hello = json!({ "type": "daemon_hello", "model": "x/y", "focus": { "cwd": "/a" }, "busy": false });
        let FromHub::DaemonHello { focus, about, tasks } = serde_json::from_value(hello).unwrap() else { panic!() };
        assert_eq!(focus.unwrap().cwd, PathBuf::from("/a"));
        assert_eq!(about["model"], "x/y");
        assert!(tasks.is_empty());
    }
}
