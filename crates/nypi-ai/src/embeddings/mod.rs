//! Text embeddings for long-term memory (nypi-vault/src/episodic.rs, decision D33): from any
//! OpenAI-compatible `/embeddings` endpoint, or from a quantized model run in-process. Sources are
//! tried in order, so a configured endpoint wins and the local model is the fallback. Errors are
//! returned, not raised: memory still works by keywords when no source does.

#[cfg(feature = "local-embeddings")]
pub mod local;
#[cfg(feature = "local-embeddings")]
mod qwen3;
mod remote;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::anyhow;

pub use remote::RemoteEmbedder;

/// How long a source that failed is skipped before it is tried again.
const RETRY_AFTER: Duration = Duration::from_secs(600);

/// Retrieval embeddings are asymmetric: a query gets the model's task instruction, a stored
/// memory does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmbedKind {
    Query,
    Document,
}

#[derive(Clone, Debug)]
pub enum Embedder {
    Remote(RemoteEmbedder),
    #[cfg(feature = "local-embeddings")]
    Local(local::LocalEmbedder),
}

impl Embedder {
    /// Stored with each vector, so vectors from different models are never compared.
    pub fn reference(&self) -> String {
        match self {
            Embedder::Remote(remote) => remote.reference(),
            #[cfg(feature = "local-embeddings")]
            Embedder::Local(local) => local.reference(),
        }
    }

    pub async fn embed(&self, inputs: &[String], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
        match self {
            // An endpoint's model has its own query conventions, unknown here; text goes as is.
            Embedder::Remote(remote) => {
                let _ = kind;
                remote.embed(inputs).await
            }
            #[cfg(feature = "local-embeddings")]
            Embedder::Local(local) => local.embed(inputs, kind).await,
        }
    }
}

/// Embedding sources in order of preference.
pub struct Embeddings {
    sources: Vec<Embedder>,
    failed: Mutex<Vec<Option<Instant>>>,
}

impl Embeddings {
    pub fn new(sources: Vec<Embedder>) -> Option<Self> {
        (!sources.is_empty()).then(|| Embeddings { failed: Mutex::new(vec![None; sources.len()]), sources })
    }

    pub fn sources(&self) -> &[Embedder] {
        &self.sources
    }

    /// The source that would be used now: the first that has not failed recently.
    pub fn active(&self) -> Option<&Embedder> {
        let failed = self.failed.lock().unwrap();
        self.sources.iter().zip(failed.iter()).find(|(_, f)| !recently(f)).map(|(s, _)| s)
    }

    /// Embed with the first source that works; returns its reference with the vectors.
    pub async fn embed(&self, inputs: &[String], kind: EmbedKind) -> anyhow::Result<(String, Vec<Vec<f32>>)> {
        let mut errors = Vec::new();
        for (index, source) in self.sources.iter().enumerate() {
            if recently(&self.failed.lock().unwrap()[index]) {
                continue;
            }
            match source.embed(inputs, kind).await {
                Ok(vectors) => return Ok((source.reference(), vectors)),
                Err(error) => {
                    self.failed.lock().unwrap()[index] = Some(Instant::now());
                    errors.push(format!("{}: {error:#}", source.reference()));
                }
            }
        }
        Err(if errors.is_empty() {
            anyhow!("every embedding source failed in the last {} minutes", RETRY_AFTER.as_secs() / 60)
        } else {
            anyhow!("{}", errors.join("; "))
        })
    }

    pub async fn embed_one(&self, input: &str, kind: EmbedKind) -> anyhow::Result<(String, Vec<f32>)> {
        let (reference, mut vectors) = self.embed(&[input.to_string()], kind).await?;
        vectors.pop().map(|v| (reference, v)).ok_or_else(|| anyhow!("no vector returned"))
    }

    /// Wait for a local model that is still loading; at once for an endpoint or a loaded model.
    pub async fn ready(&self) {
        #[cfg(feature = "local-embeddings")]
        if let Some(Embedder::Local(local)) = self.active() {
            local.clone().ready().await;
        }
    }

    /// Start loading a local model in the background, so the first message does not wait for it.
    pub fn warm(&self) {
        #[cfg(feature = "local-embeddings")]
        if let Some(Embedder::Local(local)) = self.active() {
            local.warm();
        }
    }
}

fn recently(failed: &Option<Instant>) -> bool {
    failed.is_some_and(|at| at.elapsed() < RETRY_AFTER)
}
