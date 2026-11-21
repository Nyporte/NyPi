//! A local embedding model: a quantized GGUF run in-process with candle, so memory gets semantic
//! search with no service, no per-call cost, and nothing leaving the machine. The model is loaded
//! once per process and shared; inference runs on a blocking thread. Downloading is a separate,
//! explicit step (`nypi memory model download`), verified against the published checksums.
//!
//! Vectors are centered: the mean embedding of generic memory sentences is subtracted before
//! normalizing. Every memory reads "the user's ...", which pulls all similarities up together
//! (unrelated pairs scored 0.3 to 0.4); centering removes that shared direction, so a relevant
//! memory stands out from the rest (decision D33).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, anyhow, bail};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::EmbedKind;
use super::qwen3::Encoder;

/// Generic memories whose mean embedding is subtracted from every vector. Fixed, not the store's
/// own mean, so a store of one memory is centered the same way as a store of a thousand.
const CENTERING_TEXTS: &[&str] = &[
    "The user's brother works as a nurse in Berlin.",
    "The user prefers dark mode in every editor.",
    "The user decided to move the team meeting to Thursdays.",
    "The user has a doctor's appointment next week.",
    "The user's daughter starts school in September.",
    "The user is allergic to cats.",
    "The user likes jazz and old films.",
    "The user's car is a blue Volvo estate.",
    "The user wants to learn Spanish this year.",
    "The user's password manager is Bitwarden.",
    "The user's mother lives in Lisbon.",
    "The user drinks coffee without sugar.",
    "The user promised to send the report by Friday.",
    "The user went hiking in the Alps last summer.",
    "The user's company uses Kubernetes in production.",
    "The user is vegetarian.",
    "用户每天早上跑步。",
    "用户的老板叫王经理。",
    "用户喜欢吃辣的川菜。",
    "用户下个月要去上海出差。",
    "用户在学习钢琴。",
    "用户的生日是五月三日。",
    "The user's favourite book is Dune.",
    "The user lost their job in March and is looking for work.",
];
const CENTROID_FILE: &str = "centroid-v1.json";

/// Longer inputs are cut; memories are a sentence or two, and the cost grows with the square.
const MAX_TOKENS: usize = 1024;

/// A model NyPi knows how to fetch and run.
#[derive(Debug, PartialEq, Eq)]
pub struct LocalModel {
    /// What settings call it.
    pub name: &'static str,
    pub gguf_repo: &'static str,
    pub gguf_file: &'static str,
    /// The repo's checksum list, in `sha256sum` format; None when it publishes none.
    pub checksums_file: Option<&'static str>,
    pub tokenizer_repo: &'static str,
    /// The SHA-256 of `tokenizer.json`, pinned here because its repo publishes no checksum
    /// list (decision D91): a changed or tampered file is refused.
    pub tokenizer_sha256: Option<&'static str>,
    /// The task instruction queries get (documents get none).
    pub query_instruction: &'static str,
    pub approx_bytes: u64,
}

/// Qwen3-Embedding-0.6B in Q4_K_M: 397 MB, mean cosine 0.975 against f16 per the publisher's
/// drift check, strong in Chinese and English (decision D33).
pub const QWEN3_EMBEDDING_0_6B_Q4: LocalModel = LocalModel {
    name: "qwen3-embedding-0.6b-q4",
    gguf_repo: "smarttasks/Qwen3-Embedding-0.6B-GGUF",
    gguf_file: "Qwen3-Embedding-0.6B-Q4_K_M.gguf",
    checksums_file: Some("SHA256SUMS"),
    tokenizer_repo: "Qwen/Qwen3-Embedding-0.6B",
    tokenizer_sha256: Some("def76fb086971c7867b829c23a26261e38d9d74e02139253b38aeb9df8b4b50a"),
    query_instruction: "Given a message from the user, retrieve the memories about them that are relevant to it",
    approx_bytes: 408_000_000,
};

pub const MODELS: &[&LocalModel] = &[&QWEN3_EMBEDDING_0_6B_Q4];

pub fn find(name: &str) -> Option<&'static LocalModel> {
    MODELS.iter().copied().find(|m| m.name.eq_ignore_ascii_case(name.trim()))
}

impl LocalModel {
    pub fn dir(&self, models_dir: &Path) -> PathBuf {
        models_dir.join(self.name)
    }

    pub fn is_installed(&self, models_dir: &Path) -> bool {
        let dir = self.dir(models_dir);
        dir.join(self.gguf_file).is_file() && dir.join("tokenizer.json").is_file()
    }

    /// Fetch the model and tokenizer into `models_dir`, reporting `(done, total)` bytes.
    pub async fn download(&self, models_dir: &Path, progress: impl Fn(u64, u64)) -> anyhow::Result<PathBuf> {
        let dir = self.dir(models_dir);
        tokio::fs::create_dir_all(&dir).await?;
        let client = reqwest::Client::builder().user_agent(concat!("nypi/", env!("CARGO_PKG_VERSION"))).build()?;
        let expected = match self.checksums_file {
            Some(file) => {
                let text = client.get(hf_url(self.gguf_repo, file)).send().await?.error_for_status()?.text().await?;
                let sum = text.lines().find_map(|line| {
                    let (sum, name) = line.split_once(char::is_whitespace)?;
                    (name.trim().trim_start_matches('*') == self.gguf_file).then(|| sum.to_lowercase())
                });
                Some(sum.ok_or_else(|| anyhow!("{file} has no checksum for {}", self.gguf_file))?)
            }
            None => None,
        };
        let pinned = self.tokenizer_sha256.map(str::to_string);
        fetch(&client, &hf_url(self.tokenizer_repo, "tokenizer.json"), &dir.join("tokenizer.json"), pinned, &|_, _| {})
            .await
            .context("tokenizer")?;
        fetch(&client, &hf_url(self.gguf_repo, self.gguf_file), &dir.join(self.gguf_file), expected, &progress)
            .await
            .context("model")?;
        Ok(dir)
    }
}

fn hf_url(repo: &str, file: &str) -> String {
    format!("https://huggingface.co/{repo}/resolve/main/{file}")
}

/// Download to a `.part` file and rename once complete (and verified), so a cut-off download
/// never looks installed.
async fn fetch(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    sha256: Option<String>,
    progress: &dyn Fn(u64, u64),
) -> anyhow::Result<()> {
    let response = client.get(url).send().await?.error_for_status()?;
    let total = response.content_length().unwrap_or(0);
    let part = path.with_extension("part");
    let mut file = tokio::fs::File::create(&part).await?;
    let mut hasher = Sha256::new();
    let mut done = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        progress(done, total);
    }
    file.flush().await?;
    drop(file);
    if let Some(expected) = sha256 {
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected {
            let _ = tokio::fs::remove_file(&part).await;
            bail!("checksum mismatch for {url}: expected {expected}, got {actual}");
        }
    }
    tokio::fs::rename(&part, path).await?;
    Ok(())
}

struct Loaded {
    encoder: Encoder,
    tokenizer: tokenizers::Tokenizer,
    eos: Option<u32>,
    centroid: Vec<f32>,
}

/// One copy of each model per process, whichever runtime or command asks first.
fn loaded() -> &'static Mutex<HashMap<PathBuf, Arc<Loaded>>> {
    static LOADED: OnceLock<Mutex<HashMap<PathBuf, Arc<Loaded>>>> = OnceLock::new();
    LOADED.get_or_init(Default::default)
}

#[derive(Clone, Debug)]
pub struct LocalEmbedder {
    pub model: &'static LocalModel,
    pub models_dir: PathBuf,
}

impl LocalEmbedder {
    pub fn reference(&self) -> String {
        format!("{}@local-centered", self.model.name)
    }

    fn load(&self) -> anyhow::Result<Arc<Loaded>> {
        let dir = self.model.dir(&self.models_dir);
        // Held while loading, so concurrent first calls load once.
        let mut cache = loaded().lock().unwrap();
        if let Some(model) = cache.get(&dir) {
            return Ok(model.clone());
        }
        if !self.model.is_installed(&self.models_dir) {
            bail!("{} is not installed: run `nypi memory model download`", self.model.name);
        }
        let tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).map_err(|e| anyhow!("{e}"))?;
        let eos = tokenizer.token_to_id("<|endoftext|>");
        let encoder = Encoder::load(&dir.join(self.model.gguf_file))?;
        let mut model = Loaded { encoder, tokenizer, eos, centroid: Vec::new() };
        model.centroid =
            match std::fs::read_to_string(dir.join(CENTROID_FILE)).ok().and_then(|t| serde_json::from_str(&t).ok()) {
                Some(centroid) => centroid,
                None => {
                    let centroid = model.centroid()?;
                    let _ = std::fs::write(dir.join(CENTROID_FILE), serde_json::to_string(&centroid)?);
                    centroid
                }
            };
        let model = Arc::new(model);
        cache.insert(dir, model.clone());
        Ok(model)
    }

    pub fn warm(&self) {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let _ = this.load();
        });
    }

    /// Returns once the model is loaded, or failed to load; at once when it already is.
    pub async fn ready(&self) {
        let this = self.clone();
        let _ = tokio::task::spawn_blocking(move || this.load()).await;
    }

    pub async fn embed(&self, inputs: &[String], kind: EmbedKind) -> anyhow::Result<Vec<Vec<f32>>> {
        let this = self.clone();
        let texts: Vec<String> = inputs
            .iter()
            .map(|text| match kind {
                EmbedKind::Query => format!("Instruct: {}\nQuery:{text}", self.model.query_instruction),
                EmbedKind::Document => text.clone(),
            })
            .collect();
        tokio::task::spawn_blocking(move || {
            let model = this.load()?;
            texts.iter().map(|text| Ok(center(model.embed(text)?, &model.centroid))).collect()
        })
        .await?
    }
}

fn center(mut vector: Vec<f32>, centroid: &[f32]) -> Vec<f32> {
    if centroid.len() == vector.len() {
        vector.iter_mut().zip(centroid).for_each(|(x, c)| *x -= c);
    }
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    vector.iter_mut().for_each(|x| *x /= norm);
    vector
}

impl Loaded {
    fn centroid(&self) -> anyhow::Result<Vec<f32>> {
        let mut sum = Vec::new();
        for text in CENTERING_TEXTS {
            let vector = self.embed(text)?;
            sum.resize(vector.len(), 0.0);
            sum.iter_mut().zip(&vector).for_each(|(s, x)| *s += x);
        }
        Ok(sum.into_iter().map(|s| s / CENTERING_TEXTS.len() as f32).collect())
    }

    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let encoding = self.tokenizer.encode(text, true).map_err(|e| anyhow!("{e}"))?;
        let mut ids = encoding.get_ids().to_vec();
        ids.truncate(MAX_TOKENS - 1);
        // The model pools the last token, which it was trained to be the end-of-text marker.
        if let Some(eos) = self.eos
            && ids.last() != Some(&eos)
        {
            ids.push(eos);
        }
        self.encoder.embed(&ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centering_removes_the_shared_direction() {
        let centroid = [0.6, 0.0];
        let (a, b) = (center(vec![0.8, 0.6], &centroid), center(vec![0.8, -0.6], &centroid));
        let cosine: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!(cosine < 0.0, "0.28 before centering, opposite after: {cosine}");
        assert!((a.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-6, "still unit length");
        assert_eq!(find("Qwen3-Embedding-0.6B-Q4"), Some(&QWEN3_EMBEDDING_0_6B_Q4));
    }

    /// A download whose SHA-256 is not the pinned one is refused and leaves nothing behind
    /// (decision D91); the right one is kept. Served from this machine: no network.
    #[tokio::test]
    async fn a_download_with_the_wrong_checksum_is_refused_and_its_part_removed() {
        use tokio::io::AsyncReadExt;
        const BODY: &str = r#"{"model": "not the pinned tokenizer"}"#;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request).await;
                    let response =
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{BODY}", BODY.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        let dir = std::env::temp_dir().join(format!("nypi-fetch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tokenizer.json");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{address}/tokenizer.json");
        let pinned = QWEN3_EMBEDDING_0_6B_Q4.tokenizer_sha256.map(str::to_string);
        let error = fetch(&client, &url, &path, pinned, &|_, _| {}).await.unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"), "{error}");
        assert!(!path.exists() && !path.with_extension("part").exists(), "nothing is left behind");

        let right = format!("{:x}", Sha256::digest(BODY.as_bytes()));
        fetch(&client, &url, &path, Some(right), &|_, _| {}).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), BODY);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
