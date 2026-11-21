//! `nypi memory`: look at and manage long-term memory (nypi-core/src/memory.rs) by hand: the
//! newest memories, a search, adding one, forgetting one, re-embedding, and the local embedding
//! model. Changes go into the same log the agent writes, marked as made by the user.

use nypi_ai::embeddings::{EmbedKind, Embeddings};
use nypi_core::config::{Paths, Settings};
use nypi_core::memory::{Memory, MemoryEvent, MemoryKind, MemoryStore, NewMemory, Provenance, embedding_text, now};

fn line(m: &Memory) -> String {
    let date = m.created.get(..10).unwrap_or_default();
    let tags = if m.tags.is_empty() { String::new() } else { format!("  #{}", m.tags.join(" #")) };
    format!("{}  {date}  {:<10}  {}{tags}", m.id, m.kind.label(), m.text)
}

pub fn list(paths: &Paths, settings: &Settings, limit: usize) -> anyhow::Result<i32> {
    let store = MemoryStore::new(paths.memory());
    let memories = store.load();
    if memories.is_empty() {
        nypi_hub::outln!(
            "No memories yet ({}). Lucia remembers what you tell her that lasts; or: nypi memory add \"...\"",
            store.path().display()
        );
        return Ok(0);
    }
    nypi_hub::outln!("{} memories in {}; newest:", memories.len(), store.path().display());
    let (embeddings, _) = settings.memory.embeddings(paths);
    match embeddings.as_ref().and_then(Embeddings::active) {
        Some(active) => {
            let reference = active.reference();
            let missing = memories.iter().filter(|m| m.embedding.as_ref().is_none_or(|(r, _)| *r != reference)).count();
            let note =
                if missing > 0 { format!("; {missing} without a vector (nypi memory reembed)") } else { String::new() };
            nypi_hub::outln!("Semantic search with {reference}{note}.");
        }
        None => nypi_hub::outln!("Keyword search only: no embedding model (nypi memory model)."),
    }
    for memory in memories.iter().rev().take(limit) {
        nypi_hub::outln!("{}", line(memory));
    }
    Ok(0)
}

pub async fn search(
    paths: &Paths,
    settings: &Settings,
    query: &str,
    limit: usize,
    explain: bool,
) -> anyhow::Result<i32> {
    let store = MemoryStore::new(paths.memory());
    let (embeddings, _) = settings.memory.embeddings(paths);
    let vector = match &embeddings {
        Some(embeddings) => match embeddings.embed_one(query, EmbedKind::Query).await {
            Ok(vector) => Some(vector),
            Err(error) => {
                nypi_hub::errln!("Embedding failed, searching by keywords only: {error:#}");
                None
            }
        },
        None => None,
    };
    let query_embedding = vector.as_ref().map(|(model, v)| (model.as_str(), v.as_slice()));
    let hits = store.search(query, limit, query_embedding, &settings.memory.weights);
    if hits.is_empty() {
        nypi_hub::outln!("No memories match.");
    }
    for hit in hits {
        nypi_hub::outln!("{:.2}  {}", hit.score, line(&hit.memory));
        if explain {
            nypi_hub::outln!("      {}", hit.factors.explain());
        }
    }
    Ok(0)
}

pub async fn add(paths: &Paths, settings: &Settings, text: &str, tags: Vec<String>, kind: &str) -> anyhow::Result<i32> {
    let Some(kind) = MemoryKind::parse(kind) else {
        anyhow::bail!("Unknown kind {kind}: fact, preference, decision, event, person, or commitment")
    };
    let store = MemoryStore::new(paths.memory());
    let (embeddings, _) = settings.memory.embeddings(paths);
    let embedding = match &embeddings {
        Some(embeddings) => embeddings.embed_one(&embedding_text(text, &tags), EmbedKind::Document).await.ok(),
        None => None,
    };
    let provenance = Provenance { by: "user".into(), ..Default::default() };
    let id = store.add(NewMemory {
        text: text.into(),
        tags,
        kind,
        importance: 0.7,
        provenance,
        emotion: None,
        sensitive: false,
        embedding,
    })?;
    nypi_hub::outln!("Remembered as {id}.");
    Ok(0)
}

pub fn forget(paths: &Paths, id: &str) -> anyhow::Result<i32> {
    let store = MemoryStore::new(paths.memory());
    if store.get(id).is_none() {
        nypi_hub::errln!("No memory {id}.");
        return Ok(1);
    }
    store.append(&MemoryEvent::Forgotten { id: id.to_string(), at: now(), reason: "forgotten by the user".into() })?;
    nypi_hub::outln!("Forgot {id}.");
    Ok(0)
}

/// Give every memory a vector from the active embedding model.
pub async fn reembed(paths: &Paths, settings: &Settings) -> anyhow::Result<i32> {
    let (embeddings, skipped) = settings.memory.embeddings(paths);
    let Some(embeddings) = embeddings else {
        nypi_hub::errln!("No embedding model is available: {}", skipped.join("; "));
        return Ok(1);
    };
    let reference = embeddings.active().map(|e| e.reference()).unwrap_or_default();
    let store = MemoryStore::new(paths.memory());
    let started = std::time::Instant::now();
    let count = store.backfill(&embeddings, usize::MAX).await?;
    nypi_hub::outln!("Embedded {count} memories with {reference} in {:.1} s.", started.elapsed().as_secs_f64());
    Ok(0)
}

/// Where the local embedding model stands, and the sources in use.
pub fn model_status(paths: &Paths, settings: &Settings) -> anyhow::Result<i32> {
    for (i, source) in settings.memory.embeddings.iter().enumerate() {
        let name = source.local.clone().unwrap_or_else(|| format!("{} at {}", source.model, source.base_url));
        match source.embedder(paths) {
            Ok(_) => nypi_hub::outln!("{}. {name}: ready", i + 1),
            Err(why) => nypi_hub::outln!("{}. {name}: {why}", i + 1),
        }
    }
    if settings.memory.embeddings.is_empty() {
        nypi_hub::outln!("Embeddings are off (memory.embeddings is empty): keyword search only.");
    }
    Ok(0)
}

#[cfg(feature = "local-embeddings")]
pub async fn model_download(paths: &Paths, settings: &Settings) -> anyhow::Result<i32> {
    use nypi_ai::embeddings::local;
    use std::io::Write;
    let name = settings.memory.embeddings.iter().find_map(|s| s.local.clone());
    let model = name.as_deref().and_then(local::find).unwrap_or(&local::QWEN3_EMBEDDING_0_6B_Q4);
    let models_dir = paths.embedding_models();
    if model.is_installed(&models_dir) {
        nypi_hub::outln!("{} is already installed in {}.", model.name, model.dir(&models_dir).display());
        return Ok(0);
    }
    nypi_hub::outln!(
        "Downloading {} (about {} MB) from huggingface.co/{} into {}",
        model.name,
        model.approx_bytes / 1_000_000,
        model.gguf_repo,
        model.dir(&models_dir).display()
    );
    let last = std::sync::Mutex::new(0u64);
    let dir = model
        .download(&models_dir, |done, total| {
            let mut last = last.lock().unwrap();
            if done - *last >= 16_000_000 || done == total {
                *last = done;
                nypi_hub::errout!("\r  {} / {} MB", done / 1_000_000, total / 1_000_000);
                let _ = std::io::stderr().flush();
            }
        })
        .await?;
    nypi_hub::errln!();
    nypi_hub::outln!(
        "Installed in {}, checksum verified. Run `nypi memory reembed` to embed existing memories.",
        dir.display()
    );
    Ok(0)
}

#[cfg(not(feature = "local-embeddings"))]
pub async fn model_download(_paths: &Paths, _settings: &Settings) -> anyhow::Result<i32> {
    nypi_hub::errln!("This build has no local embeddings; rebuild with the local-embeddings feature.");
    Ok(1)
}
