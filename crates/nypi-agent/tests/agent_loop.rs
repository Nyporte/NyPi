//! Agent loop behavior against the faux provider.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use nypi_agent::{Agent, AgentConfig, AgentEvent, Block, ExecutionMode, Tool, ToolContext, ToolHooks, ToolOutput};
use nypi_ai::providers::faux::{FauxProvider, FauxResponse, FauxStep, faux_model};
use nypi_ai::{Message, StopReason, ThinkingLevel, ToolCall, ToolDefinition, text_of};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Echoes `text` after sleeping `delay_ms`.
struct Echo;

#[async_trait]
impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".into(),
            description: "Echo text".into(),
            parameters: json!({
                "type": "object",
                "required": ["text"],
                "properties": { "text": { "type": "string" }, "delay_ms": { "type": "integer" } }
            }),
        }
    }

    async fn execute(&self, arguments: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let delay = arguments["delay_ms"].as_u64().unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        Ok(ToolOutput::text(arguments["text"].as_str().unwrap_or_default()))
    }
}

struct BlockAll;

#[async_trait]
impl ToolHooks for BlockAll {
    async fn before_tool_call(&self, _call: &ToolCall, _arguments: &Value) -> Option<Block> {
        Some(Block { reason: "blocked by policy".into(), terminate: false })
    }
}

fn agent(faux: &FauxProvider, hooks: Vec<Arc<dyn ToolHooks>>) -> Agent {
    Agent::new(
        AgentConfig {
            model: faux_model(),
            thinking: ThinkingLevel::Off,
            system_prompt: "test".into(),
            tools: vec![Arc::new(Echo)],
            hooks,
            turn_hooks: None,
            stream_fn: faux.stream_fn(),
            api_key: None,
            tool_execution: ExecutionMode::Parallel,
            max_tokens: None,
            temperature: None,
            session_id: None,
        },
        vec![],
    )
}

fn calls(steps: Vec<(&str, Value)>) -> FauxResponse {
    FauxResponse {
        steps: steps
            .into_iter()
            .map(|(id, arguments)| FauxStep::ToolCall { id: id.into(), name: "echo".into(), arguments })
            .collect(),
        stop_reason: StopReason::ToolUse,
        error: None,
    }
}

fn tool_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(r) => Some(text_of(&r.content)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn tool_round_trip_then_final_answer() {
    let faux = FauxProvider::new(vec![calls(vec![("c1", json!({ "text": "hi" }))]), FauxResponse::text("done")]);
    let mut agent = agent(&faux, vec![]);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let added = agent.prompt(vec![Message::user_text("go")], &tx).await;
    let roles: Vec<&str> = added.iter().map(Message::role).collect();
    assert_eq!(roles, ["user", "assistant", "toolResult", "assistant"]);
    assert_eq!(tool_texts(&added), ["hi"]);
    // The second request saw the tool result.
    assert_eq!(faux.requests()[1].messages.len(), 3);
    drop(tx);
    let mut kinds = Vec::new();
    while let Some(event) = rx.recv().await {
        kinds.push(match event {
            AgentEvent::AgentStart => "agent_start",
            AgentEvent::TurnStart => "turn_start",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::AgentEnd { .. } => "agent_end",
            _ => continue,
        });
    }
    assert_eq!(kinds, ["agent_start", "turn_start", "turn_end", "turn_start", "turn_end", "agent_end"]);
}

#[tokio::test]
async fn parallel_results_keep_source_order() {
    let faux = FauxProvider::new(vec![
        calls(vec![("slow", json!({ "text": "slow", "delay_ms": 80 })), ("fast", json!({ "text": "fast" }))]),
        FauxResponse::text("ok"),
    ]);
    let mut agent = agent(&faux, vec![]);
    let (tx, _rx) = mpsc::unbounded_channel();
    let started = std::time::Instant::now();
    let added = agent.prompt(vec![Message::user_text("go")], &tx).await;
    assert_eq!(tool_texts(&added), ["slow", "fast"]);
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn invalid_arguments_and_blocked_calls_become_error_results() {
    let faux = FauxProvider::new(vec![calls(vec![("c1", json!({}))]), FauxResponse::text("ok")]);
    let mut agent1 = agent(&faux, vec![]);
    let (tx, _rx) = mpsc::unbounded_channel();
    let added = agent1.prompt(vec![Message::user_text("go")], &tx).await;
    assert!(tool_texts(&added)[0].contains("text: missing required property"));

    let faux = FauxProvider::new(vec![calls(vec![("c1", json!({ "text": "x" }))]), FauxResponse::text("ok")]);
    let mut agent2 = agent(&faux, vec![Arc::new(BlockAll)]);
    let added = agent2.prompt(vec![Message::user_text("go")], &tx).await;
    assert_eq!(tool_texts(&added), ["blocked by policy"]);
}

#[tokio::test]
async fn steering_is_injected_before_the_next_request_and_follow_up_after_stop() {
    let faux = FauxProvider::new(vec![
        calls(vec![("c1", json!({ "text": "a", "delay_ms": 50 }))]),
        FauxResponse::text("after steer"),
        FauxResponse::text("after follow-up"),
    ]);
    let mut agent = agent(&faux, vec![]);
    let handle = agent.handle();
    handle.steer(Message::user_text("STEER"));
    handle.follow_up(Message::user_text("FOLLOW"));
    let (tx, _rx) = mpsc::unbounded_channel();
    let added = agent.prompt(vec![Message::user_text("go")], &tx).await;
    let users: Vec<String> = added
        .iter()
        .filter_map(|m| match m {
            Message::User(u) => Some(text_of(&u.content)),
            _ => None,
        })
        .collect();
    assert_eq!(users, ["go", "STEER", "FOLLOW"]);
    assert_eq!(faux.requests().len(), 3);
    assert!(!handle.is_running());
}

#[tokio::test]
async fn length_stop_fails_tool_calls_and_error_ends_run() {
    let mut truncated = calls(vec![("c1", json!({ "text": "x" }))]);
    truncated.stop_reason = StopReason::Length;
    let faux = FauxProvider::new(vec![truncated, FauxResponse::error("boom")]);
    let mut agent = agent(&faux, vec![]);
    let (tx, _rx) = mpsc::unbounded_channel();
    let added = agent.prompt(vec![Message::user_text("go")], &tx).await;
    assert!(tool_texts(&added)[0].contains("output token limit"));
    let Some(Message::Assistant(last)) = added.last() else { panic!() };
    assert_eq!(last.stop_reason, StopReason::Error);
    assert_eq!(last.error_message.as_deref(), Some("boom"));
}

#[tokio::test]
async fn abort_cancels_running_tools_after_the_grace_period() {
    let faux = FauxProvider::new(vec![calls(vec![("c1", json!({ "text": "x", "delay_ms": 5000 }))])]);
    let mut agent = agent(&faux, vec![]);
    let handle = agent.handle();
    let (tx, _rx) = mpsc::unbounded_channel();
    let aborter = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
    });
    let started = std::time::Instant::now();
    let added = agent.prompt(vec![Message::user_text("go")], &tx).await;
    aborter.await.unwrap();
    // Echo ignores cancellation, so it is dropped after the 2 s grace period instead of running 5 s.
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(tool_texts(&added), ["Operation aborted"]);
}

#[tokio::test]
async fn a_tool_added_mid_session_is_declared_as_a_change() {
    let faux = FauxProvider::new(vec![FauxResponse::text("one"), FauxResponse::text("two")]);
    let mut agent = agent(&faux, vec![]);
    agent.config.tools = vec![];
    // A transcript that declares its own (empty) tool set.
    agent.messages.push(Message::System(nypi_ai::SystemMessage { content: "test".into(), ..Default::default() }));
    let (tx, _rx) = mpsc::unbounded_channel();
    agent.prompt(vec![Message::user_text("a")], &tx).await;
    assert!(!agent.messages.iter().skip(1).any(|m| matches!(m, Message::System(_))), "no change, no message");

    agent.config.tools = vec![Arc::new(Echo)];
    let added = agent.prompt(vec![Message::user_text("b")], &tx).await;
    let Some(Message::System(update)) = added.iter().find(|m| matches!(m, Message::System(_))) else {
        panic!("a tool declaration: {added:?}")
    };
    assert_eq!(update.tools_added[0].name, "echo");
    let requests = faux.requests();
    let head = &requests[1].messages[0];
    assert!(matches!(head, Message::System(s) if s.tools_added.is_empty()), "the head is not rewritten");
    assert_eq!(nypi_ai::transcript::current_tools(&requests[1].messages)[0].name, "echo");
}

/// Holds the call named `wait` until the call named `go` has finished, as a person answering an
/// approval only after they have seen the other call's result would.
struct WaitForOther(Arc<tokio::sync::Notify>);

#[async_trait]
impl ToolHooks for WaitForOther {
    async fn before_tool_call(&self, call: &ToolCall, _arguments: &Value) -> Option<Block> {
        if call.id == "wait" {
            tokio::time::timeout(Duration::from_secs(5), self.0.notified()).await.ok()?;
        }
        None
    }

    async fn after_tool_call(&self, call: &ToolCall, _arguments: &Value, output: ToolOutput) -> ToolOutput {
        if call.id == "go" {
            self.0.notify_one();
        }
        output
    }
}

#[tokio::test]
async fn a_call_waiting_for_approval_holds_up_none_of_the_others() {
    let faux = FauxProvider::new(vec![
        calls(vec![("wait", json!({ "text": "pushed" })), ("go", json!({ "text": "read" }))]),
        FauxResponse::text("ok"),
    ]);
    let mut agent = agent(&faux, vec![Arc::new(WaitForOther(Arc::default()))]);
    let (tx, _rx) = mpsc::unbounded_channel();
    let started = std::time::Instant::now();
    let added = agent.prompt(vec![Message::user_text("go")], &tx).await;
    // Had the second call waited for the first one's answer, the first would have timed out.
    assert_eq!(tool_texts(&added), ["pushed", "read"], "results keep the reply's order");
    assert!(started.elapsed() < Duration::from_secs(4));
}
