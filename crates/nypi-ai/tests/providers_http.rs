//! The Responses, Gemini, and Bedrock adapters over real HTTP against a local mock server: the
//! request each one sends (path, auth headers, body) and the message it builds from the reply.

use std::sync::{Arc, Mutex};

use nypi_ai::{
    AssistantMessage, ContentBlock, Context, Message, Model, StopReason, StreamEvent, StreamOptions, stream,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Debug, Default)]
struct Captured {
    head: String,
    body: String,
}

/// Serve `reply` (with `content_type`) to one request and capture that request.
async fn mock(content_type: &'static str, reply: Vec<u8>) -> (String, Arc<Mutex<Captured>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Captured::default()));
    let sink = captured.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            buffer.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buffer).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if body.len() >= length {
                    *sink.lock().unwrap() = Captured { head: head.to_string(), body: body.to_string() };
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            reply.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(&reply).await.unwrap();
    });
    (format!("http://{address}"), captured)
}

fn model(api: &str, provider: &str, id: &str, base_url: &str) -> Model {
    serde_json::from_value(json!({
        "id": id, "name": id, "api": api, "provider": provider, "baseUrl": base_url,
        "reasoning": false, "contextWindow": 100000, "maxTokens": 1000
    }))
    .unwrap()
}

async fn run(model: &Model, opts: StreamOptions) -> AssistantMessage {
    let ctx = Context { system_prompt: "sys".into(), messages: vec![Message::user_text("hi")], tools: vec![] };
    let mut events = stream(model, &ctx, &opts);
    while let Some(event) = events.recv().await {
        if let StreamEvent::Done { message } = event {
            return message;
        }
    }
    panic!("stream ended without Done");
}

fn sse(events: &[Value]) -> Vec<u8> {
    events.iter().map(|e| format!("data: {e}\n\n")).collect::<String>().into_bytes()
}

fn text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn openai_responses_over_http() {
    let reply = sse(&[
        json!({ "type": "response.output_item.added", "output_index": 0, "item": { "type": "message", "id": "msg_1" } }),
        json!({ "type": "response.output_text.delta", "output_index": 0, "delta": "Hello" }),
        json!({ "type": "response.output_item.done", "output_index": 0, "item": { "type": "message", "id": "msg_1", "content": [{ "type": "output_text", "text": "Hello" }] } }),
        json!({ "type": "response.completed", "response": { "status": "completed", "usage": { "input_tokens": 3, "output_tokens": 1, "total_tokens": 4 } } }),
    ]);
    let (base, captured) = mock("text/event-stream", reply).await;
    let model = model("openai-responses", "openai", "gpt-x", &base);
    let message = run(&model, StreamOptions { api_key: Some("sk-test".into()), ..Default::default() }).await;
    assert_eq!(
        (message.stop_reason, text(&message).as_str()),
        (StopReason::Stop, "Hello"),
        "{:?}",
        message.error_message
    );
    let request = captured.lock().unwrap().clone();
    assert!(request.head.starts_with("POST /responses "));
    assert!(request.head.to_lowercase().contains("authorization: bearer sk-test"));
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["input"][0], json!({ "role": "system", "content": "sys" }));
    assert_eq!(body["store"], false);
}

#[tokio::test]
async fn gemini_over_http_keeps_the_key_out_of_the_url() {
    let reply = sse(&[
        json!({ "candidates": [{ "content": { "parts": [{ "text": "Hi " }] } }] }),
        json!({ "candidates": [{ "content": { "parts": [{ "text": "there" }] }, "finishReason": "STOP" }], "usageMetadata": { "promptTokenCount": 5, "candidatesTokenCount": 2, "totalTokenCount": 7 } }),
    ]);
    let (base, captured) = mock("text/event-stream", reply).await;
    let model = model("google-generative-ai", "google", "gemini-2.5-flash", &base);
    let message = run(&model, StreamOptions { api_key: Some("g-secret".into()), ..Default::default() }).await;
    assert_eq!(
        (message.stop_reason, text(&message).as_str()),
        (StopReason::Stop, "Hi there"),
        "{:?}",
        message.error_message
    );
    let request = captured.lock().unwrap().clone();
    let request_line = request.head.lines().next().unwrap().to_string();
    assert_eq!(request_line, "POST /models/gemini-2.5-flash:streamGenerateContent?alt=sse HTTP/1.1");
    assert!(!request_line.contains("g-secret"));
    assert!(request.head.to_lowercase().contains("x-goog-api-key: g-secret"));
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "sys");
    assert_eq!(body["contents"][0], json!({ "role": "user", "parts": [{ "text": "hi" }] }));
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn frame(message_type: &str, event: (&str, &str), payload: Value) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [(":message-type", message_type), event] {
        headers.push(name.len() as u8);
        headers.extend(name.as_bytes());
        headers.push(7);
        headers.extend((value.len() as u16).to_be_bytes());
        headers.extend(value.as_bytes());
    }
    let payload = payload.to_string().into_bytes();
    let mut out = Vec::new();
    out.extend(((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    out.extend((headers.len() as u32).to_be_bytes());
    out.extend(crc32(&out).to_be_bytes());
    out.extend(headers);
    out.extend(payload);
    out.extend(crc32(&out).to_be_bytes());
    out
}

#[tokio::test]
async fn bedrock_over_http_with_a_bearer_token_and_a_stream_exception() {
    let event = |name: &'static str| (":event-type", name);
    let mut reply = Vec::new();
    reply.extend(frame("event", event("messageStart"), json!({ "role": "assistant" })));
    reply.extend(frame(
        "event",
        event("contentBlockDelta"),
        json!({ "contentBlockIndex": 0, "delta": { "text": "Bonjour" } }),
    ));
    reply.extend(frame("event", event("contentBlockStop"), json!({ "contentBlockIndex": 0 })));
    reply.extend(frame("event", event("messageStop"), json!({ "stopReason": "end_turn" })));
    reply.extend(frame(
        "event",
        event("metadata"),
        json!({ "usage": { "inputTokens": 4, "outputTokens": 1, "totalTokens": 5 } }),
    ));
    let (base, captured) = mock("application/vnd.amazon.eventstream", reply).await;
    let model = model("bedrock-converse-stream", "amazon-bedrock", "anthropic.claude-sonnet-4-6:0", &base);
    let message = run(&model, StreamOptions { api_key: Some("bedrock-key".into()), ..Default::default() }).await;
    assert_eq!(
        (message.stop_reason, text(&message).as_str()),
        (StopReason::Stop, "Bonjour"),
        "{:?}",
        message.error_message
    );
    assert_eq!(message.usage.input, 4);
    let request = captured.lock().unwrap().clone();
    assert!(
        request.head.starts_with("POST /model/anthropic.claude-sonnet-4-6%3A0/converse-stream "),
        "{}",
        request.head
    );
    assert!(request.head.to_lowercase().contains("authorization: bearer bedrock-key"));
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["system"][0]["text"], "sys");
    assert_eq!(body["messages"][0]["content"][0]["text"], "hi");

    let mut failing = Vec::new();
    failing.extend(frame("event", event("messageStart"), json!({ "role": "assistant" })));
    failing.extend(frame("exception", (":exception-type", "throttlingException"), json!({ "message": "slow down" })));
    let (base, _) = mock("application/vnd.amazon.eventstream", failing).await;
    let model = model.clone();
    let model = Model { base_url: base, ..model };
    let message = run(&model, StreamOptions { api_key: Some("bedrock-key".into()), ..Default::default() }).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Throttling error: slow down"));
}

#[tokio::test]
async fn embeddings_over_http_come_back_in_order() {
    let reply = serde_json::to_vec(&json!({
        "data": [ { "index": 1, "embedding": [0.0, 1.0] }, { "index": 0, "embedding": [1.0, 0.0] } ]
    }))
    .unwrap();
    let (base, captured) = mock("application/json", reply).await;
    let embedder = nypi_ai::embeddings::RemoteEmbedder {
        base_url: base.clone(),
        model: "embed-1".into(),
        api_key: Some("k".into()),
    };
    let vectors = embedder.embed(&["first".into(), "second".into()]).await.unwrap();
    assert_eq!(vectors, [vec![1.0, 0.0], vec![0.0, 1.0]]);
    let request = captured.lock().unwrap().clone();
    assert!(request.head.starts_with("POST /embeddings "));
    assert!(request.head.to_lowercase().contains("authorization: bearer k"));
    assert_eq!(serde_json::from_str::<Value>(&request.body).unwrap()["input"], json!(["first", "second"]));
    assert!(embedder.reference().starts_with("embed-1@http://"));
}
