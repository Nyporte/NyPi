//! Scripted provider for tests: each request pops the next scripted response, or, for a
//! provider made with `answering`, gets the response a function chooses from the request, so
//! several agents sharing one model (a worker, its observer, the chat module) are scripted
//! each by what it was asked, whatever order their requests arrive in.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::mpsc;

use crate::event::MessageBuilder;
use crate::models::{Api, Model};
use crate::provider::StreamFn;
use crate::types::{Context, StopReason};

/// One step of a scripted response.
#[derive(Clone, Debug)]
pub enum FauxStep {
    Thinking(String),
    Text(String),
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    /// The rest of the response waits until the gate is opened, or forever: a request still
    /// streaming, for tests of what happens meanwhile (an abort ends it as it would a real one).
    Hold(Arc<tokio::sync::Notify>),
}

/// A scripted assistant response.
#[derive(Clone, Debug)]
pub struct FauxResponse {
    pub steps: Vec<FauxStep>,
    pub stop_reason: StopReason,
    pub error: Option<String>,
}

impl FauxResponse {
    pub fn text(text: &str) -> Self {
        FauxResponse { steps: vec![FauxStep::Text(text.into())], stop_reason: StopReason::Stop, error: None }
    }

    pub fn tool_call(id: &str, name: &str, arguments: Value) -> Self {
        FauxResponse {
            steps: vec![FauxStep::ToolCall { id: id.into(), name: name.into(), arguments }],
            stop_reason: StopReason::ToolUse,
            error: None,
        }
    }

    pub fn error(message: &str) -> Self {
        FauxResponse { steps: vec![], stop_reason: StopReason::Error, error: Some(message.into()) }
    }
}

/// Chooses the response to a request. None falls back to the script.
type Responder = Arc<dyn Fn(&Context) -> Option<FauxResponse> + Send + Sync>;

/// Shared script plus a log of every request context the provider received.
#[derive(Clone, Default)]
pub struct FauxProvider {
    script: Arc<Mutex<VecDeque<FauxResponse>>>,
    requests: Arc<Mutex<Vec<Context>>>,
    responder: Option<Responder>,
}

impl FauxProvider {
    pub fn new(responses: Vec<FauxResponse>) -> Self {
        FauxProvider { script: Arc::new(Mutex::new(responses.into())), requests: Arc::default(), responder: None }
    }

    /// A provider whose response to each request is what `responder` says; where it says
    /// nothing, the next response pushed with `push`.
    pub fn answering(responder: impl Fn(&Context) -> Option<FauxResponse> + Send + Sync + 'static) -> Self {
        FauxProvider { responder: Some(Arc::new(responder)), ..Default::default() }
    }

    pub fn push(&self, response: FauxResponse) {
        self.script.lock().unwrap().push_back(response);
    }

    /// Contexts of all requests so far.
    pub fn requests(&self) -> Vec<Context> {
        self.requests.lock().unwrap().clone()
    }

    pub fn stream_fn(&self) -> StreamFn {
        let provider = self.clone();
        Arc::new(move |model: &Model, ctx: &Context, opts| {
            provider.requests.lock().unwrap().push(ctx.clone());
            let chosen = provider.responder.as_ref().and_then(|responder| responder(ctx));
            let next = chosen.or_else(|| provider.script.lock().unwrap().pop_front());
            let (tx, rx) = mpsc::unbounded_channel();
            // A held response is built into a channel of its own and passed on once its gate opens.
            let gate = next.as_ref().and_then(|response| {
                response.steps.iter().find_map(|step| match step {
                    FauxStep::Hold(gate) => Some(gate.clone()),
                    _ => None,
                })
            });
            let (tx, rx) = match gate {
                None => (tx, rx),
                Some(gate) => {
                    let (held_tx, mut held_rx) = mpsc::unbounded_channel();
                    let (out, cancel, model) = (tx, opts.cancel.clone(), model.clone());
                    tokio::spawn(async move {
                        // Aborted while held, it ends as a real request does: aborted.
                        tokio::select! {
                            _ = gate.notified() => {}
                            _ = cancel.cancelled() => {
                                MessageBuilder::start(&model, out).finish(StopReason::Aborted, Some("Request aborted".into()));
                                return;
                            }
                        }
                        while let Some(event) = held_rx.recv().await {
                            if out.send(event).is_err() {
                                break;
                            }
                        }
                    });
                    (held_tx, rx)
                }
            };
            let mut builder = MessageBuilder::start(model, tx);
            let Some(response) = next else {
                builder.finish(StopReason::Error, Some("faux script exhausted".into()));
                return rx;
            };
            for step in response.steps {
                match step {
                    FauxStep::Thinking(text) => builder.thinking(&text),
                    FauxStep::Text(text) => builder.text(&text),
                    FauxStep::ToolCall { id, name, arguments } => {
                        let index = builder.tool_call_start(&id, &name);
                        builder.tool_call_delta(index, &arguments.to_string());
                        builder.tool_call_end(index);
                    }
                    FauxStep::Hold(_) => {}
                }
            }
            builder.usage_mut().input = 10;
            builder.usage_mut().output = 5;
            builder.finish(response.stop_reason, response.error);
            rx
        })
    }
}

/// A model served by the faux provider.
pub fn faux_model() -> Model {
    Model {
        id: "faux-1".into(),
        name: "Faux".into(),
        api: Api::Faux,
        provider: "faux".into(),
        base_url: String::new(),
        reasoning: false,
        input: vec!["text".into(), "image".into()],
        cost: Default::default(),
        context_window: 100_000,
        max_tokens: 4096,
        thinking_level_map: Default::default(),
        compat: Default::default(),
        headers: Default::default(),
        api_key_env: None,
    }
}
