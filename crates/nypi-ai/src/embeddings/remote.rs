//! Embeddings from an OpenAI-compatible `/embeddings` endpoint (OpenAI, a gateway, Ollama).

use std::time::Duration;

use anyhow::{anyhow, bail};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct RemoteEmbedder {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

/// Written out by hand so the key never reaches a log or an error message.
impl std::fmt::Debug for RemoteEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteEmbedder")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl RemoteEmbedder {
    pub fn reference(&self) -> String {
        format!("{}@{}", self.model, self.base_url.trim_end_matches('/'))
    }

    pub async fn embed(&self, inputs: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("nypi/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let mut request = client
            .post(format!("{}/embeddings", self.base_url.trim_end_matches('/')))
            .json(&json!({ "model": self.model, "input": inputs }));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let message = body["error"]["message"].as_str().unwrap_or("no message");
            bail!("embeddings request failed (HTTP {status}): {message}");
        }
        let mut data: Vec<(u64, Vec<f32>)> = body["data"]
            .as_array()
            .ok_or_else(|| anyhow!("embeddings response has no data"))?
            .iter()
            .map(|item| {
                let vector = item["embedding"]
                    .as_array()
                    .map(|v| v.iter().filter_map(|x| x.as_f64()).map(|x| x as f32).collect());
                (item["index"].as_u64().unwrap_or(0), vector.unwrap_or_default())
            })
            .collect();
        data.sort_by_key(|(index, _)| *index);
        if data.len() != inputs.len() || data.iter().any(|(_, v)| v.is_empty()) {
            bail!("embeddings response has {} vectors for {} inputs", data.len(), inputs.len());
        }
        Ok(data.into_iter().map(|(_, v)| v).collect())
    }
}
