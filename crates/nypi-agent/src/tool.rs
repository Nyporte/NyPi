//! Tool trait, tool results, and pre/post execution hooks.

use async_trait::async_trait;
use nypi_ai::{ContentBlock, ToolCall, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::events::{AgentEvent, EventSink};

/// Whether a tool may run concurrently with the other calls of the same assistant message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    #[default]
    Parallel,
    Sequential,
}

/// Result (or partial result) of a tool execution.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ToolOutput {
    /// Model-facing content.
    pub content: Vec<ContentBlock>,
    /// Structured data for logs and UI; not sent to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub is_error: bool,
    /// Hint to stop after this batch. Honored only when every result in the batch sets it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub terminate: bool,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        ToolOutput { content: vec![ContentBlock::text(text)], ..Default::default() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        ToolOutput { content: vec![ContentBlock::text(text)], is_error: true, ..Default::default() }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

/// Per-call context handed to `Tool::execute`.
#[derive(Clone)]
pub struct ToolContext {
    pub call_id: String,
    pub tool_name: String,
    pub cancel: CancellationToken,
    sink: EventSink,
}

impl ToolContext {
    pub fn new(call_id: String, tool_name: String, cancel: CancellationToken, sink: EventSink) -> Self {
        ToolContext { call_id, tool_name, cancel, sink }
    }

    /// Stream a partial result to observers (UI, Inspector).
    pub fn update(&self, partial: ToolOutput) {
        let _ = self.sink.send(AgentEvent::ToolExecutionUpdate {
            call_id: self.call_id.clone(),
            tool_name: self.tool_name.clone(),
            partial,
        });
    }
}

/// An executable tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    /// Compatibility shim run before validation, for models that send near-miss argument shapes.
    fn prepare_arguments(&self, arguments: Value) -> Value {
        arguments
    }

    /// Run the tool. Return `Err` (or an output with `is_error`) on failure.
    async fn execute(&self, arguments: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput>;
}

/// Why a call was blocked by a hook.
#[derive(Clone, Debug)]
pub struct Block {
    pub reason: String,
    pub terminate: bool,
}

/// Observes and may veto or rewrite tool calls. The Inspector's guard is one of these.
#[async_trait]
pub trait ToolHooks: Send + Sync {
    /// Called after argument validation. Return `Some` to prevent execution.
    async fn before_tool_call(&self, _call: &ToolCall, _arguments: &Value) -> Option<Block> {
        None
    }

    /// Called after execution; may replace the output.
    async fn after_tool_call(&self, _call: &ToolCall, _arguments: &Value, output: ToolOutput) -> ToolOutput {
        output
    }
}
