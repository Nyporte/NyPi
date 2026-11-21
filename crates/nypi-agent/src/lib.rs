//! NyPi agent kernel: the turn loop, tool trait, hooks, and steering/follow-up queues.
//! Mirrors Pi's `@earendil-works/pi-agent-core`.

pub mod agent;
pub mod events;
pub mod tool;
pub mod validate;

pub use agent::{Agent, AgentConfig, AgentHandle, MessageQueue, QueueMode, TurnHooks, TurnUpdate};
pub use events::{AgentEvent, EventSink};
pub use tool::{Block, ExecutionMode, Tool, ToolContext, ToolHooks, ToolOutput};
