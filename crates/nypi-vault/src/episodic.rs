//! Episodic long-term memory (docs/architecture.md §4.4, requirement R6; in this crate since
//! decision D77, used by the runtime as `nypi_core::memory`): durable facts, preferences,
//! decisions, and events that outlive sessions, kept in `~/.nypi/memory/memories.jsonl` as an
//! append-only event log (added, recalled, updated, forgotten). Relevance is semantic first: the
//! cosine similarity of embeddings (a local model by default, decision D33), with BM25 keyword
//! evidence over the text and the agent's tags adding to it, so names and numbers still match
//! exactly. Without a vector, keywords alone decide, so memory works with no model. Ranking is gated: relevance
//! decides whether a memory counts, and importance, emotional intensity, and activation (how
//! recently and often it was used) only re-rank relevant ones (decision D31). Only the harness
//! writes the log; the agent goes through the `memory` tool.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// BM25 parameters (the usual defaults).
const K1: f64 = 1.2;
const B: f64 = 0.75;
/// Background documents assumed in IDF, so a small store scores matches like a large one would.
const IDF_PRIOR_DOCS: f64 = 20.0;
/// Recalls kept per memory for activation; older ones add almost nothing.
const RECALLS_KEPT: usize = 100;
/// Activation treats anything more recent than this as this old, so a fresh memory is not infinite.
const MIN_AGE_DAYS: f64 = 1.0 / 24.0;

/// How a memory felt: valence -1 (painful) to 1 (joyful), arousal 0 (calm) to 1 (intense).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Emotion {
    pub valence: f64,
    pub arousal: f64,
}

impl Emotion {
    pub fn clamped(self) -> Self {
        Emotion { valence: self.valence.clamp(-1.0, 1.0), arousal: self.arousal.clamp(0.0, 1.0) }
    }
}

/// The weights of the gated score `R × (relevance + importance·I + emotion·E + activation·A)`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetrievalWeights {
    pub relevance: f64,
    pub importance: f64,
    pub emotion: f64,
    pub activation: f64,
    /// Keyword relevance is `s / (s + kappa)` for BM25 score `s`; kappa is about one solid match.
    pub kappa: f64,
    /// Memories with relevance below this are not returned.
    pub floor: f64,
    /// Cosine similarity of unrelated text for the embedding model; it maps to relevance 0.
    pub embedding_baseline: f64,
    /// Cosine similarity of a clearly relevant memory; it maps to relevance 1.
    pub embedding_full: f64,
    /// When a memory has a vector, keyword relevance `k` adds to semantic relevance `s` as
    /// `s + (1 - s)·keywords·k`: exact names and numbers help, but never pull a meaning down.
    pub keywords: f64,
    /// Activation time constant in days, by kind label; kinds left out keep their default.
    pub tau_days: BTreeMap<String, f64>,
}

/// How long a memory of each kind stays fresh without use: events go stale fast, preferences
/// and people barely at all.
fn default_tau_days(kind: MemoryKind) -> f64 {
    match kind {
        MemoryKind::Preference | MemoryKind::Person | MemoryKind::Decision => 365.0,
        MemoryKind::Fact | MemoryKind::Commitment => 90.0,
        MemoryKind::Event => 14.0,
    }
}

impl Default for RetrievalWeights {
    fn default() -> Self {
        RetrievalWeights {
            relevance: 0.55,
            importance: 0.20,
            emotion: 0.10,
            activation: 0.15,
            kappa: 2.0,
            floor: 0.15,
            // Calibrated on the centered local model: relevant memories score 0.27 to 0.6,
            // the best unrelated one at most 0.23 (decision D33).
            embedding_baseline: 0.17,
            embedding_full: 0.65,
            keywords: 0.5,
            tau_days: BTreeMap::new(),
        }
    }
}

impl RetrievalWeights {
    /// The four weights scaled to sum to 1, so their meaning does not depend on their units.
    fn normalized(&self) -> [f64; 4] {
        let raw = [self.relevance, self.importance, self.emotion, self.activation].map(|w| w.max(0.0));
        let sum: f64 = raw.iter().sum();
        if sum <= 0.0 { [1.0, 0.0, 0.0, 0.0] } else { raw.map(|w| w / sum) }
    }

    fn tau(&self, kind: MemoryKind) -> f64 {
        self.tau_days.get(kind.label()).copied().filter(|t| *t > 0.0).unwrap_or_else(|| default_tau_days(kind))
    }
}

/// What `MemoryStore::add` stores.
#[derive(Clone, Debug, Default)]
pub struct NewMemory {
    pub text: String,
    pub tags: Vec<String>,
    pub kind: MemoryKind,
    pub importance: f64,
    pub provenance: Provenance,
    pub emotion: Option<Emotion>,
    pub sensitive: bool,
    pub embedding: Option<(String, Vec<f32>)>,
}

/// Why a memory ranked where it did (principle A2): each factor from 0 to 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct Factors {
    pub relevance: f64,
    pub importance: f64,
    pub emotion: f64,
    pub activation: f64,
}

impl Factors {
    pub fn explain(&self) -> String {
        format!("R {:.2} · I {:.2} · E {:.2} · A {:.2}", self.relevance, self.importance, self.emotion, self.activation)
    }
}
/// Chinese particles and pronouns that, alone, match almost any sentence ("的" joined a question
/// about a dentist to a memory about a cat). Pairs containing them are still indexed.
const CJK_STOP_CHARS: [char; 20] = [
    '的', '了', '是', '我', '你', '他', '她', '在', '吗', '呢', '吧', '啊', '和', '有', '也', '都', '就', '这', '那',
    '个',
];

const STOPWORDS: [&str; 32] = [
    "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "with", "is", "are", "was", "were", "be", "it",
    "that", "this", "at", "by", "as", "from", "i", "my", "me", "you", "your", "we", "our", "do", "does",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    #[default]
    Fact,
    Preference,
    Decision,
    Event,
    Person,
    Commitment,
}

impl MemoryKind {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "fact" => MemoryKind::Fact,
            "preference" => MemoryKind::Preference,
            "decision" => MemoryKind::Decision,
            "event" => MemoryKind::Event,
            "person" => MemoryKind::Person,
            "commitment" => MemoryKind::Commitment,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Preference => "preference",
            MemoryKind::Decision => "decision",
            MemoryKind::Event => "event",
            MemoryKind::Person => "person",
            MemoryKind::Commitment => "commitment",
        }
    }
}

/// Where a memory came from (principle A4).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Provenance {
    pub session: Option<String>,
    pub task: Option<String>,
    /// Who recorded it: the agent's name, or "user" for `nypi memory add`.
    pub by: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum MemoryEvent {
    Added {
        id: String,
        at: String,
        text: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        kind: MemoryKind,
        /// 0 to 1: how much it matters that this is remembered.
        #[serde(default = "default_importance")]
        importance: f64,
        #[serde(default)]
        provenance: Provenance,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        emotion: Option<Emotion>,
        /// Health, money, loss, conflict, and the like: never surfaced unasked in a light moment.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        sensitive: bool,
        /// Present when an embedding model was configured: (model reference, vector).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        embedding: Option<(String, Vec<f32>)>,
    },
    Updated {
        id: String,
        at: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        tags: Option<Vec<String>>,
        #[serde(default)]
        importance: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        emotion: Option<Emotion>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sensitive: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        embedding: Option<(String, Vec<f32>)>,
    },
    Recalled {
        id: String,
        at: String,
    },
    Forgotten {
        id: String,
        at: String,
        reason: String,
    },
}

fn default_importance() -> f64 {
    0.5
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub id: String,
    pub text: String,
    pub tags: Vec<String>,
    pub kind: MemoryKind,
    pub importance: f64,
    pub provenance: Provenance,
    pub created: String,
    pub last_recalled: Option<String>,
    pub recall_count: u32,
    pub emotion: Option<Emotion>,
    pub sensitive: bool,
    /// Recent recall times, for activation.
    #[serde(skip)]
    pub recalls: Vec<String>,
    #[serde(skip)]
    pub embedding: Option<(String, Vec<f32>)>,
}

impl Memory {
    /// Marked sensitive, or remembered as painful and intense.
    pub fn is_sensitive(&self) -> bool {
        self.sensitive || self.emotion.is_some_and(|e| e.valence <= -0.3 && e.arousal >= 0.4)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hit {
    pub memory: Memory,
    pub score: f64,
    pub factors: Factors,
}

pub struct MemoryStore {
    path: PathBuf,
    write: Mutex<()>,
    live: crate::jsonl::Folded<Live>,
}

/// The live memories as the log's events leave them, in the order they were added.
#[derive(Default)]
struct Live {
    /// Shared with whoever is reading; copied only when an event arrives while someone still is.
    list: Arc<Vec<Memory>>,
    /// Where each id is in `list`.
    place: std::collections::HashMap<String, usize>,
}

impl Live {
    fn remove(&mut self, id: &str) {
        let Some(index) = self.place.remove(id) else { return };
        Arc::make_mut(&mut self.list).remove(index);
        for place in self.place.values_mut().filter(|place| **place > index) {
            *place -= 1;
        }
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Memory> {
        let index = *self.place.get(id)?;
        Arc::make_mut(&mut self.list).get_mut(index)
    }
}

/// Take one line of the log into the fold. Unreadable lines are skipped.
fn take_in(live: &mut Live, line: &str, _: u64) {
    let Ok(event) = serde_json::from_str::<MemoryEvent>(line) else { return };
    match event {
        MemoryEvent::Added { id, at, text, tags, kind, importance, provenance, emotion, sensitive, embedding } => {
            let memory = Memory {
                id: id.clone(),
                text,
                tags,
                kind,
                importance: importance.clamp(0.0, 1.0),
                provenance,
                created: at,
                last_recalled: None,
                recall_count: 0,
                emotion: emotion.map(Emotion::clamped),
                sensitive,
                recalls: Vec::new(),
                embedding,
            };
            // Added again under the same id, it is the newer one, in the newer place.
            live.remove(&id);
            live.place.insert(id, live.list.len());
            Arc::make_mut(&mut live.list).push(memory);
        }
        MemoryEvent::Updated { id, text, tags, importance, emotion, sensitive, embedding, .. } => {
            if let Some(memory) = live.get_mut(&id) {
                if let Some(text) = text {
                    memory.text = text;
                }
                if let Some(tags) = tags {
                    memory.tags = tags;
                }
                if let Some(importance) = importance {
                    memory.importance = importance.clamp(0.0, 1.0);
                }
                if let Some(emotion) = emotion {
                    memory.emotion = Some(emotion.clamped());
                }
                if let Some(sensitive) = sensitive {
                    memory.sensitive = sensitive;
                }
                if embedding.is_some() {
                    memory.embedding = embedding;
                }
            }
        }
        MemoryEvent::Recalled { id, at } => {
            if let Some(memory) = live.get_mut(&id) {
                memory.recalls.push(at.clone());
                if memory.recalls.len() > RECALLS_KEPT {
                    memory.recalls.remove(0);
                }
                memory.last_recalled = Some(at);
                memory.recall_count += 1;
            }
        }
        MemoryEvent::Forgotten { id, .. } => live.remove(&id),
    }
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl MemoryStore {
    pub fn new(path: PathBuf) -> Self {
        let live = crate::jsonl::Folded::shared(&path);
        MemoryStore { path, write: Mutex::new(()), live }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Append one event, synced to disk; a partial last line from a crash is closed first.
    pub fn append(&self, event: &MemoryEvent) -> std::io::Result<()> {
        let _guard = self.write.lock().unwrap();
        crate::jsonl::append(&self.path, event)
    }

    /// Every live memory, oldest first, as a snapshot nothing has to copy: recall happens on
    /// every message and on every pause in typing, and each memory carries a vector. Only what
    /// was appended to the log since the last look is read (jsonl.rs).
    pub fn snapshot(&self) -> Arc<Vec<Memory>> {
        self.live.read(take_in, |live| live.list.clone())
    }

    /// Every live memory, oldest first, as a copy to keep.
    pub fn load(&self) -> Vec<Memory> {
        self.snapshot().as_ref().clone()
    }

    pub fn get(&self, id: &str) -> Option<Memory> {
        self.live.read(take_in, |live| live.place.get(id).map(|index| live.list[*index].clone()))
    }

    /// Store a memory; returns its id.
    pub fn add(&self, memory: NewMemory) -> std::io::Result<String> {
        let id = format!("m-{}", &uuid::Uuid::now_v7().simple().to_string()[20..]);
        self.append(&MemoryEvent::Added {
            id: id.clone(),
            at: now(),
            text: memory.text.trim().to_string(),
            tags: memory.tags.into_iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
            kind: memory.kind,
            importance: memory.importance.clamp(0.0, 1.0),
            provenance: memory.provenance,
            emotion: memory.emotion.map(Emotion::clamped),
            sensitive: memory.sensitive,
            embedding: memory.embedding,
        })?;
        Ok(id)
    }

    /// The best `limit` memories for `query`, best first. `query_embedding` blends in semantic
    /// similarity for memories embedded with the same model. Records the recall of each hit.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        query_embedding: Option<(&str, &[f32])>,
        weights: &RetrievalWeights,
    ) -> Vec<Hit> {
        let memories = self.snapshot();
        let hits = rank(&memories, query, limit, query_embedding, weights, chrono::Utc::now());
        self.record_recalls(hits.iter().map(|h| h.memory.id.as_str()));
        hits
    }

    /// Embed memories that have no vector from the active embedding model (none yet, or another
    /// model's), at most `limit`, so a new or changed model covers the whole store. Returns how
    /// many were embedded.
    pub async fn backfill(&self, embeddings: &nypi_ai::embeddings::Embeddings, limit: usize) -> anyhow::Result<usize> {
        let Some(active) = embeddings.active().map(|e| e.reference()) else { return Ok(0) };
        let stale: Vec<Memory> = self
            .snapshot()
            .iter()
            .filter(|m| m.embedding.as_ref().is_none_or(|(model, _)| *model != active))
            .take(limit)
            .cloned()
            .collect();
        let mut done = 0;
        for batch in stale.chunks(8) {
            let texts: Vec<String> = batch.iter().map(|m| embedding_text(&m.text, &m.tags)).collect();
            let (reference, vectors) = embeddings.embed(&texts, nypi_ai::embeddings::EmbedKind::Document).await?;
            for (memory, vector) in batch.iter().zip(vectors) {
                self.append(&MemoryEvent::Updated {
                    id: memory.id.clone(),
                    at: now(),
                    text: None,
                    tags: None,
                    importance: None,
                    emotion: None,
                    sensitive: None,
                    embedding: Some((reference.clone(), vector)),
                })?;
                done += 1;
            }
        }
        Ok(done)
    }

    /// Note that these memories were used, which keeps their activation up.
    pub fn record_recalls<'a>(&self, ids: impl IntoIterator<Item = &'a str>) {
        for id in ids {
            let _ = self.append(&MemoryEvent::Recalled { id: id.to_string(), at: now() });
        }
    }
}

/// Words for the index: lowercase alphanumeric runs (stopwords dropped, English words stemmed),
/// and every CJK character plus each pair of neighbors, since CJK text has no spaces between words.
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut cjk: Vec<char> = Vec::new();
    let flush_word = |word: &mut String, out: &mut Vec<String>| {
        if !word.is_empty() && !STOPWORDS.contains(&word.as_str()) {
            out.push(stem(word));
        }
        word.clear();
    };
    let flush_cjk = |cjk: &mut Vec<char>, out: &mut Vec<String>| {
        for (i, c) in cjk.iter().enumerate() {
            if !CJK_STOP_CHARS.contains(c) {
                out.push(c.to_string());
            }
            if let Some(next) = cjk.get(i + 1) {
                out.push(format!("{c}{next}"));
            }
        }
        cjk.clear();
    };
    for c in text.chars().flat_map(char::to_lowercase) {
        if is_cjk(c) {
            flush_word(&mut word, &mut out);
            cjk.push(c);
        } else if c.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut out);
            word.push(c);
        } else {
            flush_word(&mut word, &mut out);
            flush_cjk(&mut cjk, &mut out);
        }
    }
    flush_word(&mut word, &mut out);
    flush_cjk(&mut cjk, &mut out);
    out
}

/// Fold English inflections (plurals, -ed, -ing) so "bikes" finds "bike" and "booked" finds
/// "book". Porter's step 1, without its later steps: those merge words of different meaning
/// ("editor" and "edit"), which hurts a small personal store more than a missed match. Both the
/// memories and the query go through it, so a stem only has to be consistent, not a real word.
pub fn stem(word: &str) -> String {
    if word.len() <= 3 || !word.bytes().all(|b| b.is_ascii_lowercase()) {
        return word.to_string();
    }
    let w = word;
    // Plurals and third person.
    let w = if let Some(base) = w.strip_suffix("sses") {
        format!("{base}ss")
    } else if let Some(base) = w.strip_suffix("ies").filter(|b| b.len() >= 2) {
        format!("{base}y")
    } else if w.ends_with("ss") || w.ends_with("us") || w.ends_with("is") {
        w.to_string()
    } else if let Some(base) = ["ches", "shes", "xes", "zes"].iter().find_map(|s| w.strip_suffix(s).map(|b| (b, s))) {
        format!("{}{}", base.0, &base.1[..base.1.len() - 2])
    } else if let Some(base) = w.strip_suffix('s') {
        base.to_string()
    } else {
        w.to_string()
    };
    // Past tense and participles.
    if w.ends_with("eed") {
        return w;
    }
    if let Some(base) = w.strip_suffix("ied").filter(|b| b.len() >= 2) {
        return format!("{base}y");
    }
    let base = w.strip_suffix("ing").filter(|b| b.len() >= 3).or_else(|| w.strip_suffix("ed").filter(|b| b.len() >= 3));
    match base {
        Some(base) if base.bytes().any(is_vowel) => restore(base),
        _ => w,
    }
}

fn is_vowel(b: u8) -> bool {
    matches!(b, b'a' | b'e' | b'i' | b'o' | b'u' | b'y')
}

/// Undo what removing -ed or -ing leaves behind: a doubled consonant ("running"), or a missing
/// silent e ("making", "loved").
fn restore(base: &str) -> String {
    let b = base.as_bytes();
    let n = b.len();
    if ["at", "bl", "iz"].iter().any(|s| base.ends_with(s)) {
        return format!("{base}e");
    }
    if n >= 2 && b[n - 1] == b[n - 2] && !is_vowel(b[n - 1]) && !matches!(b[n - 1], b'l' | b's' | b'z') {
        return base[..n - 1].to_string();
    }
    // A short stem, consonant-vowel-consonant ("mak", "lov"), had an e; "w", "x", "y" never end one.
    let cvc = n >= 3
        && !is_vowel(b[n - 3])
        && is_vowel(b[n - 2])
        && !is_vowel(b[n - 1])
        && !matches!(b[n - 1], b'w' | b'x' | b'y');
    if cvc && b[..n - 2].iter().filter(|c| is_vowel(**c)).count() == 0 {
        return format!("{base}e");
    }
    base.to_string()
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff)
}

/// What a memory's vector is computed from: its sentence and its tags.
pub fn embedding_text(text: &str, tags: &[String]) -> String {
    if tags.is_empty() { text.to_string() } else { format!("{text} ({})", tags.join(", ")) }
}

fn document(memory: &Memory) -> Vec<String> {
    tokens(&format!("{} {}", memory.text, memory.tags.join(" ")))
}

/// Rank memories for `query` (decision D31): `score = R × (wr + wi·I + we·E + wa·A)`, with the
/// weights summing to 1. R is relevance: embedding similarity plus keyword evidence when both
/// sides have vectors from the same model, keywords alone otherwise; memories below the relevance floor are dropped, so
/// importance or freshness can never surface an unrelated memory.
pub fn rank(
    memories: &[Memory],
    query: &str,
    limit: usize,
    query_embedding: Option<(&str, &[f32])>,
    weights: &RetrievalWeights,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<Hit> {
    let query_terms: Vec<String> = {
        let mut terms = tokens(query);
        terms.sort();
        terms.dedup();
        terms
    };
    let docs: Vec<Vec<String>> = memories.iter().map(document).collect();
    let n = docs.len() as f64;
    let average = docs.iter().map(Vec::len).sum::<usize>() as f64 / n.max(1.0);
    let mut frequency: HashMap<&str, usize> = HashMap::new();
    for doc in &docs {
        let mut seen: Vec<&str> = doc.iter().map(String::as_str).collect();
        seen.sort();
        seen.dedup();
        for term in seen {
            *frequency.entry(term).or_default() += 1;
        }
    }
    let [wr, wi, we, wa] = weights.normalized();
    let mut hits: Vec<Hit> = memories
        .iter()
        .zip(&docs)
        .filter_map(|(memory, doc)| {
            let length = doc.len() as f64;
            let bm25: f64 = query_terms
                .iter()
                .map(|term| {
                    let tf = doc.iter().filter(|t| *t == term).count() as f64;
                    if tf == 0.0 {
                        return 0.0;
                    }
                    let df = *frequency.get(term.as_str()).unwrap_or(&0) as f64;
                    let idf = (1.0 + (n + IDF_PRIOR_DOCS - df + 0.5) / (df + 0.5)).ln();
                    idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * length / average.max(1.0)))
                })
                .sum();
            let lexical = bm25 / (bm25 + weights.kappa.max(0.01));
            let semantic = query_embedding.and_then(|(model, vector)| match &memory.embedding {
                Some((m, v)) if m == model => {
                    let baseline = weights.embedding_baseline.clamp(-1.0, 0.99);
                    let full = weights.embedding_full.clamp(baseline + 0.01, 1.0);
                    Some(((cosine(vector, v) - baseline) / (full - baseline)).clamp(0.0, 1.0))
                }
                _ => None,
            });
            let relevance = match semantic {
                Some(similarity) => similarity + (1.0 - similarity) * weights.keywords.clamp(0.0, 1.0) * lexical,
                None => lexical,
            };
            if relevance < weights.floor {
                return None;
            }
            let factors = Factors {
                relevance,
                importance: memory.importance,
                emotion: memory.emotion.map_or(0.0, |e| e.arousal),
                activation: activation(memory, weights.tau(memory.kind), now),
            };
            let score = relevance * (wr + wi * factors.importance + we * factors.emotion + wa * factors.activation);
            Some(Hit { memory: memory.clone(), score, factors })
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(b.memory.created.cmp(&a.memory.created))
    });
    hits.truncate(limit);
    hits
}

/// ACT-R-style base-level activation, squashed to 0..1: `ln(1 + Σ (age_j / tau)^-0.5)` over the
/// creation and every recall, so a memory in use stays strong and an unused one fades.
pub fn activation(memory: &Memory, tau_days: f64, now: chrono::DateTime<chrono::Utc>) -> f64 {
    let age_days = |at: &str| {
        chrono::DateTime::parse_from_rfc3339(at)
            .map(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds() as f64 / 86_400.0)
            .unwrap_or(tau_days)
            .max(MIN_AGE_DAYS)
    };
    let sum: f64 = std::iter::once(memory.created.as_str())
        .chain(memory.recalls.iter().map(String::as_str))
        .map(|at| (age_days(at) / tau_days).powf(-0.5))
        .sum();
    let raw = (1.0 + sum).ln();
    raw / (raw + 1.0)
}

pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a.iter().zip(b).map(|(x, y)| f64::from(*x) * f64::from(*y)).sum();
    let norm = |v: &[f32]| v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
    let denominator = norm(a) * norm(b);
    if denominator == 0.0 { 0.0 } else { dot / denominator }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_split_words_and_cjk() {
        assert_eq!(tokens("The user prefers Rust, not Go."), ["user", "prefer", "rust", "not", "go"]);
        assert_eq!(tokens("喜欢 Rust"), ["喜", "喜欢", "欢", "rust"]);
        assert_eq!(tokens("我的猫"), ["我的", "的猫", "猫"], "particles alone are not keywords");
    }

    #[test]
    fn stems_fold_inflections_consistently() {
        let pairs = [
            ("bikes", "bike"),
            ("grandmothers", "grandmother"),
            ("tyres", "tyre"),
            ("boxes", "box"),
            ("matches", "match"),
            ("stories", "story"),
            ("classes", "class"),
            ("booked", "book"),
            ("running", "run"),
            ("making", "make"),
            ("loved", "love"),
            ("biking", "bike"),
            ("studied", "study"),
            ("hopped", "hop"),
            ("hoped", "hope"),
            ("related", "relate"),
        ];
        for (word, base) in pairs {
            assert_eq!(stem(word), stem(base), "{word} and {base}");
        }
        for unchanged in ["bus", "virus", "analysis", "need", "thing", "sing", "editor", "rust", "café"] {
            assert_eq!(stem(unchanged), unchanged);
        }
        assert_ne!(stem("editor"), stem("edit"), "derivations that change meaning stay apart");
    }

    #[test]
    fn stores_ranks_and_forgets() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("memory/memories.jsonl"));
        let add = |text: &str, tags: &[&str], kind, importance| {
            let tags = tags.iter().map(|t| t.to_string()).collect();
            let provenance = Provenance { by: "lucia".into(), ..Default::default() };
            store
                .add(NewMemory { text: text.into(), tags, kind, importance, provenance, ..Default::default() })
                .unwrap()
        };
        let rust = add(
            "The user prefers Rust for new projects.",
            &["programming language", "编程语言"],
            MemoryKind::Preference,
            0.8,
        );
        let cat = add("The user's cat is called Miso.", &["pet"], MemoryKind::Fact, 0.4);
        add("用户住在成都。", &["city", "home"], MemoryKind::Fact, 0.5);

        let w = RetrievalWeights::default();
        let hits = store.search("which programming language should I use", 5, None, &w);
        assert_eq!(hits.first().map(|h| h.memory.id.as_str()), Some(rust.as_str()));
        assert!(hits.iter().all(|h| h.memory.id != cat), "unrelated memories are left out");
        assert_eq!(store.search("编程语言", 5, None, &w)[0].memory.id, rust, "a tag in another language matches");
        assert!(
            store.search("成都", 5, None, &w)[0].memory.text.contains("成都"),
            "Chinese text matches without spaces"
        );
        assert_eq!(store.search("my cats were called what", 5, None, &w)[0].memory.id, cat, "word forms match");
        let recalled = store.get(&rust).unwrap();
        assert_eq!((recalled.recall_count, recalled.recalls.len()), (2, 2));

        store.append(&MemoryEvent::Forgotten { id: cat.clone(), at: now(), reason: "wrong".into() }).unwrap();
        assert!(store.get(&cat).is_none());
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(store.path()).unwrap().write_all(b"{\"event\":\"add").unwrap();
        store
            .append(&MemoryEvent::Updated {
                id: rust.clone(),
                at: now(),
                text: None,
                tags: None,
                importance: Some(1.0),
                emotion: Some(Emotion { valence: 0.5, arousal: 3.0 }),
                sensitive: None,
                embedding: None,
            })
            .unwrap();
        let updated = store.get(&rust).unwrap();
        assert_eq!(updated.importance, 1.0, "a crash's partial line does not swallow later events");
        assert_eq!(updated.emotion, Some(Emotion { valence: 0.5, arousal: 1.0 }), "emotion is clamped");
    }

    #[test]
    fn embeddings_blend_in_and_match_without_shared_words() {
        let memory = |id: &str, text: &str, vector: Vec<f32>| Memory {
            id: id.into(),
            text: text.into(),
            tags: vec![],
            kind: MemoryKind::Fact,
            importance: 0.5,
            provenance: Provenance::default(),
            created: now(),
            last_recalled: None,
            recall_count: 0,
            emotion: None,
            sensitive: false,
            recalls: vec![],
            embedding: Some(("m".into(), vector)),
        };
        let memories =
            vec![memory("a", "Dentist on Friday", vec![1.0, 0.0]), memory("b", "Groceries list", vec![0.0, 1.0])];
        let (w, at) = (RetrievalWeights::default(), chrono::Utc::now());
        let hits = rank(&memories, "teeth appointment", 5, Some(("m", &[0.9, 0.1])), &w, at);
        assert_eq!(hits.iter().map(|h| h.memory.id.as_str()).collect::<Vec<_>>(), ["a"]);
        assert!(rank(&memories, "teeth appointment", 5, Some(("other-model", &[0.9, 0.1])), &w, at).is_empty());
    }

    /// The ranking's contract on a fixed set of memories (decision D31).
    #[test]
    fn evaluation_fixture() {
        let at = chrono::Utc::now();
        let ago = |days: f64| (at - chrono::Duration::seconds((days * 86_400.0) as i64)).to_rfc3339();
        let memory = |id: &str, text: &str, kind: MemoryKind, importance: f64, arousal: f64, age: f64| Memory {
            id: id.into(),
            text: text.into(),
            tags: vec![],
            kind,
            importance,
            provenance: Provenance::default(),
            created: ago(age),
            last_recalled: None,
            recall_count: 0,
            emotion: (arousal > 0.0).then_some(Emotion { valence: -0.8, arousal }),
            sensitive: false,
            recalls: vec![],
            embedding: None,
        };
        let w = RetrievalWeights::default();
        let ids = |memories: &[Memory], query: &str| -> Vec<String> {
            rank(memories, query, 10, None, &w, at).into_iter().map(|h| h.memory.id).collect()
        };

        // A vivid, important memory never surfaces for an unrelated query.
        let memories = vec![
            memory("mother", "The user's mother died in March; it still hurts.", MemoryKind::Event, 1.0, 1.0, 1.0),
            memory("editor", "The user's code editor is Helix.", MemoryKind::Preference, 0.3, 0.0, 300.0),
        ];
        assert_eq!(ids(&memories, "which editor do I use"), ["editor"]);
        assert!(ids(&memories, "what is the weather like").is_empty());

        // A single memory in the store still matches (the IDF prior).
        assert_eq!(ids(&memories[1..], "editor"), ["editor"]);

        // At equal relevance, importance and then emotion break the tie.
        let memories = vec![
            memory("minor", "The user has a meeting with Anna on Monday.", MemoryKind::Event, 0.3, 0.0, 2.0),
            memory("major", "The user has a meeting with Anna on Friday.", MemoryKind::Event, 0.9, 0.0, 2.0),
            memory("tense", "The user has a meeting with Anna on Sunday.", MemoryKind::Event, 0.3, 0.9, 2.0),
        ];
        assert_eq!(ids(&memories, "meeting with Anna"), ["major", "tense", "minor"]);

        // A clearly better match beats a weaker one with every other factor at its maximum.
        let memories = vec![
            memory("exact", "The user's passport expires in June 2027.", MemoryKind::Fact, 0.2, 0.0, 200.0),
            memory("loose", "The user dislikes June weather.", MemoryKind::Preference, 1.0, 1.0, 0.0),
        ];
        assert_eq!(ids(&memories, "when does my passport expire")[0], "exact");

        // Activation: recent use keeps a memory strong; events fade faster than preferences.
        let mut used = memory("used", "The user's dentist is Dr. Weber.", MemoryKind::Person, 0.5, 0.0, 400.0);
        used.recalls = vec![ago(3.0), ago(10.0), ago(30.0)];
        let unused = memory("unused", "The user's doctor is Dr. Weber's sister.", MemoryKind::Person, 0.5, 0.0, 400.0);
        let hits = rank(&[unused.clone(), used.clone()], "Dr. Weber", 10, None, &w, at);
        assert_eq!(hits[0].memory.id, "used", "{:?}", hits.iter().map(|h| h.factors).collect::<Vec<_>>());
        assert!(hits[0].factors.activation > hits[1].factors.activation);
        let event = memory("e", "x", MemoryKind::Event, 0.5, 0.0, 60.0);
        let preference = memory("p", "x", MemoryKind::Preference, 0.5, 0.0, 60.0);
        assert!(
            activation(&event, w.tau(MemoryKind::Event), at)
                < activation(&preference, w.tau(MemoryKind::Preference), at)
        );

        // Settings can override single weights and time constants.
        let custom: RetrievalWeights =
            serde_json::from_value(serde_json::json!({ "importance": 0.3, "tauDays": { "event": 30 } })).unwrap();
        assert_eq!((custom.importance, custom.relevance), (0.3, 0.55));
        assert_eq!((custom.tau(MemoryKind::Event), custom.tau(MemoryKind::Person)), (30.0, 365.0));

        // Every factor stays in 0..1, and the score is relevance times their weighted mix.
        for hit in rank(&[used, unused], "Weber dentist doctor", 10, None, &w, at) {
            let f = hit.factors;
            assert!([f.relevance, f.importance, f.emotion, f.activation].iter().all(|x| (0.0..=1.0).contains(x)));
            assert!(hit.score <= f.relevance + 1e-9);
        }
    }

    #[test]
    fn a_large_memory_is_parsed_once_and_a_recall_shares_it_without_copying() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("memory.jsonl"));
        // A year of memories, each with the vector the local model gives it.
        let vector: Vec<f32> = (0..768).map(|n| n as f32 / 768.0).collect();
        let mut lines = String::new();
        for n in 0..1500 {
            let added = MemoryEvent::Added {
                id: format!("m-{n:05}"),
                at: now(),
                text: format!("The owner prefers option {n} for the quarterly report"),
                tags: vec!["report".into(), "preference".into()],
                kind: MemoryKind::Preference,
                importance: 0.5,
                provenance: Default::default(),
                emotion: None,
                sensitive: false,
                embedding: Some(("local/model".into(), vector.clone())),
            };
            lines.push_str(&serde_json::to_string(&added).unwrap());
            lines.push('\n');
        }
        std::fs::write(store.path(), &lines).unwrap();

        let started = std::time::Instant::now();
        let first = store.snapshot();
        let parsed = started.elapsed();
        assert_eq!(first.len(), 1500);
        let started = std::time::Instant::now();
        for _ in 0..50 {
            assert!(Arc::ptr_eq(&store.snapshot(), &first), "the same snapshot, not a copy");
        }
        let later = started.elapsed() / 50;
        eprintln!("1,500 memories with vectors: first read {parsed:?}, each later read {later:?}");
        assert!(later * 20 < parsed, "{later:?} against {parsed:?}");

        // What happens next is taken in from the end of the log: a recall, an update, a forgetting.
        store.record_recalls(["m-00007"]);
        store.append(&MemoryEvent::Forgotten { id: "m-00003".into(), at: now(), reason: "outdated".into() }).unwrap();
        let after = store.snapshot();
        assert_eq!(after.len(), 1499);
        assert_eq!(store.get("m-00007").unwrap().recall_count, 1);
        assert!(store.get("m-00003").is_none());
        assert_eq!(after[3].id, "m-00004", "the order of the rest stands");
        assert_eq!(first.len(), 1500, "a reader's snapshot is not changed under it");
    }
}
