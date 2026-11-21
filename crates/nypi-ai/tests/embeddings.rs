//! Embedding sources in order, against local mock servers: a failing endpoint falls through to the
//! next source and is skipped for a while, so a gateway without embeddings costs one request.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nypi_ai::embeddings::{EmbedKind, Embedder, Embeddings, RemoteEmbedder};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Answer every request with `status` and `body`; returns the base URL and a request counter.
async fn server(status: &'static str, body: &'static str) -> (String, Arc<AtomicUsize>) {
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
                    if rest.len() >= length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    (format!("http://{address}/v1"), count)
}

fn remote(base_url: String) -> Embedder {
    Embedder::Remote(RemoteEmbedder { base_url, model: "embed-1".into(), api_key: None })
}

#[tokio::test]
async fn a_failing_source_falls_through_and_is_skipped() {
    let (gateway, gateway_calls) =
        server("404 Not Found", r#"{"error":{"message":"Embeddings API is not supported for this platform"}}"#).await;
    let (fallback, fallback_calls) = server("200 OK", r#"{"data":[{"index":0,"embedding":[0.6,0.8]}]}"#).await;
    let embeddings = Embeddings::new(vec![remote(gateway.clone()), remote(fallback.clone())]).unwrap();
    assert_eq!(embeddings.active().unwrap().reference(), format!("embed-1@{gateway}"));

    let (reference, vector) = embeddings.embed_one("hello", EmbedKind::Query).await.unwrap();
    assert_eq!((reference.as_str(), vector), (format!("embed-1@{fallback}").as_str(), vec![0.6, 0.8]));
    assert_eq!(embeddings.active().unwrap().reference(), format!("embed-1@{fallback}"), "the failed one steps aside");

    embeddings.embed_one("again", EmbedKind::Document).await.unwrap();
    assert_eq!(gateway_calls.load(Ordering::SeqCst), 1, "a failed source is not retried at once");
    assert_eq!(fallback_calls.load(Ordering::SeqCst), 2);

    let only_failing = Embeddings::new(vec![remote(gateway)]).unwrap();
    let error = only_failing.embed_one("x", EmbedKind::Query).await.unwrap_err();
    assert!(format!("{error:#}").contains("not supported for this platform"), "{error:#}");
    assert!(Embeddings::new(Vec::new()).is_none());
}
