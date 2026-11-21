//! NyPi LLM layer: a provider-neutral message model, a streaming event protocol, and
//! provider adapters. Mirrors Pi's `@earendil-works/pi-ai`.

pub mod auth;
pub mod embeddings;
pub mod event;
pub mod models;
pub mod overflow;
pub mod provider;
pub mod providers;
pub mod retry;
pub mod sse;
pub mod transcript;
pub mod transform;
pub mod types;

pub use event::{EventReceiver, EventSender, MessageBuilder, StreamEvent};
pub use models::{Api, Model, ModelRegistry, ThinkingLevel};
pub use provider::{StreamFn, StreamOptions, complete, default_stream_fn, requested_max_tokens, stream};
pub use types::*;
