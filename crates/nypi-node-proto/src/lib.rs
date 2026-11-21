//! What a hub and a node say to each other (docs/devices.md sections 7 and 8, decision D66).
//! The hub sends calls; the node carries them out on its device and answers with what happened.
//! When it connects, a node declares its tools and says what it is, and the hub offers the
//! model exactly those tools for a thread at that place. Only data lives here, so both ends,
//! and a phone's app, can depend on it without each other.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool a node offers: what the model is told about it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    /// JSON Schema of its arguments.
    pub parameters: Value,
    /// Its calls must not run beside other calls of the same reply.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sequential: bool,
}

/// One piece of what a call produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Block {
    Text {
        text: String,
    },
    Image {
        /// Base64-encoded bytes.
        data: String,
        mime_type: String,
    },
}

/// What a call came to, or has produced so far.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CallResult {
    /// For the model.
    pub content: Vec<Block>,
    /// For logs and interfaces; the model does not see it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub is_error: bool,
    /// A hint that the run may stop after this batch of calls.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub terminate: bool,
}

impl CallResult {
    pub fn text(text: impl Into<String>) -> Self {
        CallResult { content: vec![Block::Text { text: text.into() }], ..Default::default() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        CallResult { is_error: true, ..CallResult::text(text) }
    }
}

/// One call for a node to carry out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Call {
    /// New for every call: a node carries out an id once and never again.
    pub id: String,
    /// The ledger task it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub tool: String,
    pub arguments: Value,
    /// The folder of the thread it comes from, on the node's device.
    pub folder: String,
}

/// From the hub to a node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToNode {
    Call(Call),
    Cancel {
        id: String,
    },
    /// Are you there? A node answers `pong`; a link that stays silent is taken as lost.
    Ping,
    /// The hub has the last word on this call: the node may forget it (its journal keeps it
    /// until then, and hands it over again on every new link).
    Ack {
        id: String,
    },
    /// What does `folder` on your device say about how to work in it (decision D80)? A node
    /// answers `place_context` with the same id. Reading only: nothing is carried out.
    ReadPlaceContext {
        id: String,
        folder: String,
    },
    /// Where the hub can be reached directly now, and its epoch (decision D94): sent when the
    /// list changes. A node keeps it for its next connect; it carries out nothing.
    Hub {
        epoch: u64,
        direct: Vec<String>,
    },
    /// Update this machine's NyPi (decision D98): to `version`, or the latest release. The node
    /// lets its calls finish, checks the release itself against its own release key, answers
    /// `updated`, then starts again on the new version.
    Update {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

/// One context file of a folder (an `AGENTS.md` on the way down to it), as its device reads it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContextText {
    pub path: String,
    pub content: String,
}

/// A skill a folder offers, by name; its text stays on the device until it is read there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillNote {
    pub name: String,
    pub description: String,
    /// Where its file is on the device, to be read there (decision D83).
    #[serde(default)]
    pub path: String,
}

/// A specialist a folder defines, as the device declares it (decision D83). A hub uses a
/// native one (a model and instructions, working through the hub's tools at the device) and
/// never runs an external one's command: that would be a program written on one device run on
/// another.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SpecialistNote {
    pub name: String,
    pub description: String,
    /// `native` or `external`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    pub instructions: String,
}

/// What a folder on a device says about how to work in it. The device decides what of this
/// it gives: a folder's own settings only where its owner trusted the folder there.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlaceContext {
    pub context_files: Vec<ContextText>,
    /// The folder's own NyPi settings, when the folder is trusted on its device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trusted_settings: Option<serde_json::Value>,
    pub skills: Vec<SkillNote>,
    /// The names of the specialists the folder defines.
    pub specialists: Vec<String>,
    /// And what each is (decision D83).
    pub specialist_profiles: Vec<SpecialistNote>,
}

/// From a node to the hub: who it is, and what became of each call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromNode {
    /// The first thing a node says on a link, and again whenever what it offers changes.
    Hello {
        report: Report,
    },
    Pong,
    Started {
        id: String,
    },
    /// What the call has produced so far, as it is produced.
    Output {
        id: String,
        partial: CallResult,
    },
    Finished {
        id: String,
        result: CallResult,
    },
    /// The node will not carry it out, by its own device's rules or because it has seen the id.
    Refused {
        id: String,
        reason: String,
    },
    /// The answer to `read_place_context`.
    PlaceContext {
        id: String,
        context: PlaceContext,
    },
    /// The answer to `update`: whether the new version is in place, and what was done.
    Updated {
        id: String,
        ok: bool,
        message: String,
    },
}

impl FromNode {
    /// The call this is about, when it is about one.
    pub fn id(&self) -> Option<&str> {
        match self {
            FromNode::Started { id }
            | FromNode::Output { id, .. }
            | FromNode::Finished { id, .. }
            | FromNode::Refused { id, .. } => Some(id),
            // An answer to a question, not a word on a call.
            FromNode::Hello { .. } | FromNode::Pong | FromNode::PlaceContext { .. } | FromNode::Updated { .. } => None,
        }
    }

    /// Whether this is the last word on its call.
    pub fn ends_call(&self) -> bool {
        matches!(self, FromNode::Finished { .. } | FromNode::Refused { .. })
    }
}

/// What kind of device a node is on: it decides what the node can offer at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Computer,
    Android,
    Iphone,
}

/// Who a node is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    /// The device's name, as the owner knows it.
    pub name: String,
    pub kind: DeviceKind,
    /// Its operating system.
    pub system: String,
    /// The NyPi version the node was built from.
    #[serde(default)]
    pub version: String,
    /// The user's home folder on the device, where `~` points and its shell and security
    /// setup lives; the hub asks before a call changes those, as it does at home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
}

/// What a node tells the hub when it connects, and again when it changes: the hub's register of
/// nodes (devices.md section 8) is the last report of each.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub identity: Identity,
    pub tools: Vec<ToolDeclaration>,
    /// The calls it is carrying out now.
    #[serde(default)]
    pub running: Vec<String>,
    /// The machine runs the relay too: a fleet update leaves it alone unless asked (D98).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relay: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn calls_and_what_comes_back_keep_their_wire_shape() {
        let call = ToNode::Call(Call {
            id: "c-1".into(),
            task: Some("t-9".into()),
            tool: "bash".into(),
            arguments: json!({ "command": "ls" }),
            folder: "/Users/x/site".into(),
        });
        let wire = serde_json::to_value(&call).unwrap();
        assert_eq!(
            wire,
            json!({ "type": "call", "id": "c-1", "task": "t-9", "tool": "bash",
                    "arguments": { "command": "ls" }, "folder": "/Users/x/site" })
        );
        assert_eq!(serde_json::from_value::<ToNode>(wire).unwrap(), call);
        assert_eq!(
            serde_json::to_value(ToNode::Cancel { id: "c-1".into() }).unwrap(),
            json!({ "type": "cancel", "id": "c-1" })
        );

        let finished = FromNode::Finished { id: "c-1".into(), result: CallResult::text("a.txt") };
        assert_eq!(
            serde_json::to_value(&finished).unwrap(),
            json!({ "type": "finished", "id": "c-1",
                    "result": { "content": [{ "type": "text", "text": "a.txt" }], "isError": false } })
        );
        let refused = json!({ "type": "refused", "id": "c-2", "reason": "the phone is locked" });
        let refused: FromNode = serde_json::from_value(refused).unwrap();
        assert_eq!((refused.id(), refused.ends_call()), (Some("c-2"), true));
        assert_eq!(serde_json::to_value(ToNode::Ping).unwrap(), json!({ "type": "ping" }));
        assert_eq!(serde_json::from_value::<FromNode>(json!({ "type": "pong" })).unwrap().id(), None);
        // What an older node leaves out reads as its default.
        let bare: CallResult = serde_json::from_value(json!({ "content": [] })).unwrap();
        assert_eq!(bare, CallResult::default());
    }

    #[test]
    fn a_node_declares_what_it_is_and_what_it_can_do() {
        let report = Report {
            relay: false,
            identity: Identity {
                name: "laptop".into(),
                kind: DeviceKind::Computer,
                system: "macos".into(),
                version: "0.1.0".into(),
                home: None,
            },
            tools: vec![ToolDeclaration {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: json!({ "type": "object" }),
                sequential: false,
            }],
            running: vec![],
        };
        let wire = serde_json::to_value(&report).unwrap();
        assert_eq!(wire["identity"]["kind"], "computer");
        assert_eq!(
            wire["tools"][0],
            json!({ "name": "read", "description": "Read a file", "parameters": { "type": "object" } })
        );
        assert_eq!(serde_json::from_value::<Report>(wire).unwrap(), report);
    }
}
