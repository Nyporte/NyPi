//! Agent lifecycle events. Every front end and the Inspector consume this one stream.

use nypi_ai::{AssistantMessage, Message, StreamEvent, ToolResultMessage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::tool::ToolOutput;

/// Read back from JSON by interfaces that follow a run in another process (decision D66).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    AgentStart,
    /// A turn is one assistant response plus its tool calls and results.
    TurnStart,
    MessageStart {
        message: Message,
    },
    /// Streaming delta of the assistant message in progress.
    MessageUpdate {
        event: StreamEvent,
    },
    MessageEnd {
        message: Message,
    },
    ToolExecutionStart {
        call_id: String,
        tool_name: String,
        arguments: Value,
    },
    ToolExecutionUpdate {
        call_id: String,
        tool_name: String,
        partial: ToolOutput,
    },
    ToolExecutionEnd {
        call_id: String,
        tool_name: String,
        output: ToolOutput,
        duration_ms: Option<u64>,
    },
    TurnEnd {
        message: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    AgentEnd {
        new_messages: Vec<Message>,
    },
}

pub type EventSink = mpsc::UnboundedSender<AgentEvent>;
