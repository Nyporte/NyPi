//! Recall while the user types (decision D34): drafts get hints, unsent text stays out of the
//! audit log, and the message that follows reuses the draft's query vector.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nypi_ai::embeddings::{Embedder, Embeddings, RemoteEmbedder};
use nypi_core::memory::{MemoryKind, MemoryStore, NewMemory, RetrievalWeights};
use nypi_core::pulse::{PulseBus, PulseKind, PulsePayload, PulseSource};
use nypi_core::valve::{MemoryValve, ValveSettings, spawn_typing_recall};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// An embeddings endpoint that puts anything about bikes on one axis and everything else on the
/// other, counting requests.
async fn embeddings_server() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap_or(0);
                buffer.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buffer).to_string();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap_or(0usize);
                    if rest.len() >= length || n == 0 {
                        break;
                    }
                }
            }
            let request = String::from_utf8_lossy(&buffer).to_lowercase();
            let body = if request.contains("bike") {
                r#"{"data":[{"index":0,"embedding":[1.0,0.0]}]}"#
            } else {
                r#"{"data":[{"index":0,"embedding":[0.0,1.0]}]}"#
            };
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    (format!("http://{address}/v1"), count)
}

#[tokio::test]
async fn drafts_get_hints_and_the_sent_message_reuses_their_vector() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryStore::new(dir.path().join("memories.jsonl")));
    let (base_url, requests) = embeddings_server().await;
    let embedder = Embedder::Remote(RemoteEmbedder { base_url, model: "e".into(), api_key: None });
    let embeddings = Arc::new(Embeddings::new(vec![embedder]).unwrap());
    let reference = embeddings.active().unwrap().reference();
    let tyre = store
        .add(NewMemory {
            text: "The user's bike has a 700x28 front tyre.".into(),
            tags: vec!["bike".into(), "tyre".into()],
            kind: MemoryKind::Fact,
            importance: 0.7,
            embedding: Some((reference, vec![1.0, 0.0])),
            ..Default::default()
        })
        .unwrap();
    let valve =
        MemoryValve::new(store.clone(), Some(embeddings), RetrievalWeights::default(), ValveSettings::default());

    let bus = PulseBus::new();
    let mut hints = bus.subscribe("test", &[PulseKind::MemoryHint]);
    spawn_typing_recall(&bus, valve.clone());
    let typing = |draft: &str| {
        let payload = PulsePayload::Typing { draft: draft.into(), chars: draft.chars().count() };
        bus.publish(PulseSource::UserTyping { channel: "test".into() }, None, payload)
    };

    let pulse = typing("which inner tube for my bike tyre?");
    let hint = tokio::time::timeout(std::time::Duration::from_secs(5), hints.recv()).await.unwrap().unwrap();
    assert_eq!(hint.causation.as_ref(), Some(&pulse), "the hint is caused by the draft");
    let PulsePayload::MemoryHint { memories } = &hint.payload else { panic!("{hint:?}") };
    assert_eq!(memories.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), [tyre.as_str()]);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_eq!(store.get(&tyre).unwrap().recall_count, 0, "a draft is not a use");

    let decision = valve.consider("which inner tube for my bike tyre?", &[]).await.unwrap();
    assert!(decision.is_open());
    assert_eq!(requests.load(Ordering::SeqCst), 1, "the sent message reuses the draft's vector");
    assert_eq!(store.get(&tyre).unwrap().recall_count, 1);

    typing("hi");
    typing("haha nothing to do with anything");
    let hint = tokio::time::timeout(std::time::Duration::from_secs(5), hints.recv()).await.unwrap().unwrap();
    assert!(matches!(&hint.payload, PulsePayload::MemoryHint { memories } if memories.is_empty()), "the hint clears");

    let serialized = serde_json::to_string(&PulsePayload::Typing { draft: "secret draft".into(), chars: 12 }).unwrap();
    assert!(!serialized.contains("secret"), "unsent text never reaches the audit log: {serialized}");
}
