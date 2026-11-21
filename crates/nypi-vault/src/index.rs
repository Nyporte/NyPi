//! The vault's index (decision D77): every Markdown note split along its headings into chunks
//! that can be found and cited one by one (`path#heading`), the `[[links]]` between notes kept
//! as a graph, and a search that ranks chunks by meaning and by words as the episodic memory
//! does, then follows links one step: a note linked to or from a strong hit comes up with it,
//! so asking about a project also brings its servers, people, and playbooks.
//!
//! The notes are the truth and the index is rebuilt from them. What is kept on disk, in
//! `.index.jsonl` inside the vault, is only what is costly to make again: each chunk's vector
//! (with the model that made it and a hash of the text it was made from) and each file's
//! fingerprint. A chunk is embedded again only when its text changed.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::fs::MetadataExt;
use std::sync::OnceLock;

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::episodic::{cosine, tokens};
use crate::vault::{Vault, slug};

/// A chunk longer than this is split at paragraph breaks.
pub const MAX_CHUNK_CHARS: usize = 1200;
/// The index's own file inside the vault; no tool may name it.
pub const INDEX_FILE: &str = ".index.jsonl";

const K1: f64 = 1.2;
const B: f64 = 0.75;
/// As for memories: IDF as if this many unrelated chunks existed, so a small vault scores like a
/// large one, and one solid keyword match is worth about half.
const IDF_PRIOR_DOCS: f64 = 20.0;
const KAPPA: f64 = 2.0;
/// Similarity of unrelated and of clearly related text, for the local model after centering.
const EMBEDDING_BASELINE: f64 = 0.17;
const EMBEDDING_FULL: f64 = 0.65;
const KEYWORD_SHARE: f64 = 0.5;
/// Below this a chunk does not count as found.
const FLOOR: f64 = 0.15;
/// Direct hits whose notes are followed one link out.
const SEEDS: usize = 5;
/// How much a link to a seed multiplies a chunk's own relevance, at a seed relevance of 1.
const GRAPH_BOOST: f64 = 0.6;
/// A linked chunk needs at least this relevance of its own to be boosted.
const GRAPH_FLOOR: f64 = 0.03;
/// A linked note with no relevance of its own comes up at this share of its seed's score.
const GRAPH_SHARE: f64 = 0.35;

/// One addressable piece of a note: a heading and what is under it.
#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    /// `path#heading-slug`, numbered when a note repeats a heading or a section was split.
    pub id: String,
    pub heading: String,
    pub text: String,
}

/// A note as the index knows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Note {
    /// Relative to the vault.
    pub path: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub updated: String,
    pub tags: Vec<String>,
    pub aliases: Vec<String>,
    pub chunks: Vec<Chunk>,
    /// The targets of its `[[links]]`, as written, lowercased.
    pub links: Vec<String>,
}

/// One result of a search.
#[derive(Clone, Debug, PartialEq)]
pub struct VaultHit {
    pub chunk: String,
    pub path: String,
    pub title: String,
    pub heading: String,
    pub snippet: String,
    pub score: f64,
    /// The note whose link brought this one up, when it did not come up by itself.
    pub linked_from: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fingerprint {
    mtime_ns: i64,
    size: u64,
    sha256: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Vector {
    /// Hash of the text the vector was made from.
    sha: String,
    model: String,
    vector: Vec<f32>,
}

/// A line of `.index.jsonl`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Line {
    File { path: String, print: Fingerprint },
    Chunk { id: String, vector: Vector },
}

#[derive(Default)]
pub struct VaultIndex {
    notes: BTreeMap<String, Note>,
    prints: BTreeMap<String, Fingerprint>,
    vectors: HashMap<String, Vector>,
    /// Every name a link may use for a note (path, file name, title, alias), lowercased.
    names: HashMap<String, String>,
    forward: BTreeMap<String, BTreeSet<String>>,
    back: BTreeMap<String, BTreeSet<String>>,
}

fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn link_pattern() -> &'static regex::Regex {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"\[\[([^\[\]|#]+)(?:#[^\[\]|]*)?(?:\|[^\[\]]*)?\]\]").expect("pattern"))
}

/// A link's target as names are compared: trimmed, lowercased, without `.md`.
fn link_name(target: &str) -> String {
    let name = target.trim().to_lowercase();
    name.strip_suffix(".md").unwrap_or(&name).trim_matches('/').to_string()
}

/// `key: value` lines between the leading `---` lines, and the rest of the note. Lists are
/// `[a, b]`, `a, b`, or `- item` lines under the key.
fn frontmatter(source: &str) -> (BTreeMap<String, Vec<String>>, &str) {
    let mut fields: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Some(rest) = source.strip_prefix("---\n").or_else(|| source.strip_prefix("---\r\n")) else {
        return (fields, source);
    };
    let Some(end) = rest.find("\n---") else { return (fields, source) };
    let (head, body) = rest.split_at(end);
    let body = body["\n---".len()..].trim_start_matches(['-']).trim_start_matches(['\r', '\n']);
    let mut key = String::new();
    for line in head.lines() {
        let trimmed = line.trim();
        if let Some(item) = trimmed.strip_prefix("- ").filter(|_| line.starts_with([' ', '\t', '-']) && !key.is_empty())
        {
            fields.entry(key.clone()).or_default().push(unquote(item));
            continue;
        }
        let Some((name, value)) = trimmed.split_once(':') else { continue };
        key = name.trim().to_lowercase();
        let value = value.trim();
        let values: Vec<String> = match value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            Some(list) => list.split(',').map(unquote).filter(|v| !v.is_empty()).collect(),
            None if value.is_empty() => Vec::new(),
            None if matches!(key.as_str(), "tags" | "aliases") => {
                value.split(',').map(unquote).filter(|v| !v.is_empty()).collect()
            }
            None => vec![unquote(value)],
        };
        fields.entry(key.clone()).or_default().extend(values);
    }
    (fields, body)
}

fn unquote(text: &str) -> String {
    text.trim().trim_matches(['"', '\'']).trim().to_string()
}

/// Split `text` at paragraph breaks into pieces of at most `MAX_CHUNK_CHARS` characters; a
/// single paragraph longer than that is cut at the limit.
fn split_long(text: &str) -> Vec<String> {
    if text.chars().count() <= MAX_CHUNK_CHARS {
        return vec![text.to_string()];
    }
    let mut pieces: Vec<String> = Vec::new();
    let mut current = String::new();
    for paragraph in text.split("\n\n") {
        let would = current.chars().count() + paragraph.chars().count() + 2;
        if !current.is_empty() && would > MAX_CHUNK_CHARS {
            pieces.push(std::mem::take(&mut current));
        }
        if paragraph.chars().count() > MAX_CHUNK_CHARS {
            let chars: Vec<char> = paragraph.chars().collect();
            for part in chars.chunks(MAX_CHUNK_CHARS) {
                pieces.push(part.iter().collect());
            }
            continue;
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(paragraph);
    }
    if !current.trim().is_empty() {
        pieces.push(current);
    }
    pieces
}

/// Read a note: its frontmatter, its chunks along `#`, `##`, and `###` headings, and its links.
pub fn parse_note(path: &str, source: &str) -> Note {
    let (fields, body) = frontmatter(source);
    let first = |key: &str| fields.get(key).and_then(|values| values.first()).cloned().unwrap_or_default();
    let all = |key: &str| fields.get(key).cloned().unwrap_or_default();

    // Where each heading of the first three levels starts, and what it says.
    let mut headings: Vec<(usize, String)> = Vec::new();
    let mut open: Option<(usize, String)> = None;
    for (event, range) in Parser::new(body).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level: HeadingLevel::H1 | HeadingLevel::H2 | HeadingLevel::H3, .. }) => {
                open = Some((range.start, String::new()));
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, heading)) = open.as_mut() {
                    heading.push_str(&text);
                }
            }
            Event::End(TagEnd::Heading(_)) => headings.extend(open.take()),
            _ => {}
        }
    }
    let stem = path.rsplit('/').next().unwrap_or(path);
    let stem = stem.strip_suffix(".md").unwrap_or(stem).to_string();
    let title = Some(first("title"))
        .filter(|t| !t.is_empty())
        .or_else(|| headings.first().map(|(_, heading)| heading.trim().to_string()).filter(|h| !h.is_empty()))
        .unwrap_or(stem);

    let mut sections: Vec<(String, &str)> = Vec::new();
    let lead_end = headings.first().map_or(body.len(), |(start, _)| *start);
    if !body[..lead_end].trim().is_empty() {
        sections.push((String::new(), &body[..lead_end]));
    }
    for (index, (start, heading)) in headings.iter().enumerate() {
        let end = headings.get(index + 1).map_or(body.len(), |(next, _)| *next);
        sections.push((heading.trim().to_string(), &body[*start..end]));
    }
    let mut used: HashMap<String, usize> = HashMap::new();
    let mut chunks = Vec::new();
    for (heading, text) in sections {
        let base = if heading.is_empty() { "top".to_string() } else { slug(&heading) };
        for piece in split_long(text.trim()) {
            let count = used.entry(base.clone()).or_insert(0);
            *count += 1;
            let id = if *count == 1 { format!("{path}#{base}") } else { format!("{path}#{base}-{count}") };
            chunks.push(Chunk { id, heading: heading.clone(), text: piece });
        }
    }
    let mut links: Vec<String> =
        link_pattern().captures_iter(body).map(|found| link_name(&found[1])).filter(|l| !l.is_empty()).collect();
    links.sort();
    links.dedup();
    Note {
        path: path.to_string(),
        title,
        kind: first("kind"),
        status: first("status"),
        updated: first("updated"),
        tags: all("tags"),
        aliases: all("aliases"),
        chunks,
        links,
    }
}

impl VaultIndex {
    /// The index of `vault`: what was kept of it on disk, brought up to date with the notes.
    pub fn open(vault: &Vault) -> VaultIndex {
        let mut index = VaultIndex::default();
        if let Ok(text) = std::fs::read_to_string(vault.root().join(INDEX_FILE)) {
            for line in text.lines() {
                // Kept vectors are only trusted against the text they name by hash; fingerprints
                // are read again from the files, since nothing is in memory yet.
                if let Ok(Line::Chunk { id, vector }) = serde_json::from_str::<Line>(line) {
                    index.vectors.insert(id, vector);
                }
            }
        }
        index.refresh(vault);
        index
    }

    pub fn notes(&self) -> impl Iterator<Item = &Note> {
        self.notes.values()
    }

    pub fn note(&self, path: &str) -> Option<&Note> {
        self.notes.get(path)
    }

    /// Take in what changed in the vault's files since the last look. Returns how many notes
    /// were read again or dropped.
    pub fn refresh(&mut self, vault: &Vault) -> usize {
        let listed = vault.notes();
        let mut changed = 0;
        for path in &listed {
            let file = vault.root().join(path);
            let Ok(meta) = std::fs::metadata(&file) else { continue };
            let (mtime_ns, size) =
                (meta.mtime().saturating_mul(1_000_000_000).saturating_add(meta.mtime_nsec()), meta.len());
            if self.prints.get(path).is_some_and(|print| print.mtime_ns == mtime_ns && print.size == size) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&file) else { continue };
            let source = String::from_utf8_lossy(&bytes);
            let sha256 = sha(&source);
            let same = self.prints.get(path).is_some_and(|print| print.sha256 == sha256);
            self.prints.insert(path.clone(), Fingerprint { mtime_ns, size, sha256 });
            if !same || !self.notes.contains_key(path) {
                self.notes.insert(path.clone(), parse_note(path, &source));
                changed += 1;
            }
        }
        let gone: Vec<String> = self.notes.keys().filter(|path| !listed.contains(path)).cloned().collect();
        for path in gone {
            self.notes.remove(&path);
            self.prints.remove(&path);
            changed += 1;
        }
        if changed > 0 {
            self.relink();
        }
        changed
    }

    /// A note the vault's own tool just wrote: taken in at once, without waiting for a look at
    /// the files.
    pub fn note_written(&mut self, vault: &Vault, path: &str) {
        self.prints.remove(path);
        self.refresh(vault);
    }

    /// Rebuild the names links may use and the graph of links, and let go of vectors whose
    /// chunks are gone or have other text now.
    fn relink(&mut self) {
        self.names.clear();
        for note in self.notes.values() {
            let without = link_name(&note.path);
            let stem = without.rsplit('/').next().unwrap_or(&without).to_string();
            // A full path always names its own note; shorter names go to the first note that has them.
            self.names.insert(without, note.path.clone());
            for name in
                std::iter::once(stem).chain([link_name(&note.title)]).chain(note.aliases.iter().map(|a| link_name(a)))
            {
                if !name.is_empty() {
                    self.names.entry(name).or_insert_with(|| note.path.clone());
                }
            }
        }
        self.forward.clear();
        self.back.clear();
        for note in self.notes.values() {
            for link in &note.links {
                let Some(target) = self.names.get(link).filter(|target| **target != note.path) else { continue };
                self.forward.entry(note.path.clone()).or_default().insert(target.clone());
                self.back.entry(target.clone()).or_default().insert(note.path.clone());
            }
        }
        let live: HashMap<&str, String> = self
            .notes
            .values()
            .flat_map(|note| &note.chunks)
            .map(|chunk| (chunk.id.as_str(), sha(&chunk.text)))
            .collect();
        self.vectors.retain(|id, vector| live.get(id.as_str()).is_some_and(|text| *text == vector.sha));
    }

    /// The notes one link away from `path`, in either direction.
    pub fn neighbors(&self, path: &str) -> BTreeSet<String> {
        let mut near = self.forward.get(path).cloned().unwrap_or_default();
        near.extend(self.back.get(path).cloned().unwrap_or_default());
        near
    }

    /// Write the fingerprints and vectors to the vault's index file, through a draft.
    pub fn save(&self, vault: &Vault) -> std::io::Result<()> {
        let mut lines = Vec::new();
        for (path, print) in &self.prints {
            lines.push(serde_json::to_string(&Line::File { path: path.clone(), print: print.clone() })?);
        }
        let mut ids: Vec<&String> = self.vectors.keys().collect();
        ids.sort();
        for id in ids {
            lines.push(serde_json::to_string(&Line::Chunk { id: id.clone(), vector: self.vectors[id].clone() })?);
        }
        let path = vault.root().join(INDEX_FILE);
        let draft = vault.root().join(".index.jsonl.tmp");
        std::fs::write(&draft, lines.join("\n") + "\n")?;
        std::fs::rename(&draft, &path)
    }

    /// The chunks that have no vector from `model` for their present text, at most `limit`:
    /// what to embed, as `(chunk id, text)`.
    pub fn unembedded(&self, model: &str, limit: usize) -> Vec<(String, String)> {
        self.notes
            .values()
            .flat_map(|note| note.chunks.iter().map(move |chunk| (note, chunk)))
            .filter(|(_, chunk)| {
                !self.vectors.get(&chunk.id).is_some_and(|kept| kept.model == model && kept.sha == sha(&chunk.text))
            })
            .take(limit)
            .map(|(note, chunk)| (chunk.id.clone(), embedding_text(note, chunk)))
            .collect()
    }

    /// Keep `vector`, made by `model`, for the chunk `id` as it reads now.
    pub fn set_vector(&mut self, id: &str, model: &str, vector: Vec<f32>) {
        let text = self.notes.values().flat_map(|note| &note.chunks).find(|chunk| chunk.id == id).map(|c| sha(&c.text));
        if let Some(sha) = text {
            self.vectors.insert(id.to_string(), Vector { sha, model: model.to_string(), vector });
        }
    }

    /// Embed the chunks that need it with the active source, at most `limit`, and save the
    /// index. Returns how many were embedded; none without an embedding source.
    pub async fn embed_missing(
        &mut self,
        vault: &Vault,
        embeddings: &nypi_ai::embeddings::Embeddings,
        limit: usize,
    ) -> anyhow::Result<usize> {
        let Some(model) = embeddings.active().map(|embedder| embedder.reference()) else { return Ok(0) };
        let todo = self.unembedded(&model, limit);
        let mut done = 0;
        for batch in todo.chunks(8) {
            let texts: Vec<String> = batch.iter().map(|(_, text)| text.clone()).collect();
            let (reference, vectors) = embeddings.embed(&texts, nypi_ai::embeddings::EmbedKind::Document).await?;
            for ((id, _), vector) in batch.iter().zip(vectors) {
                self.set_vector(id, &reference, vector);
                done += 1;
            }
        }
        if done > 0 {
            self.save(vault)?;
        }
        Ok(done)
    }

    /// The best `limit` chunks for `query`, best first. `query_embedding` is the query's vector
    /// and the model that made it; without one, words alone decide. One chunk per note is
    /// shown, its best, so a long note does not crowd out the others.
    pub fn search(&self, query: &str, limit: usize, query_embedding: Option<(&str, &[f32])>) -> Vec<VaultHit> {
        let mut terms = tokens(query);
        terms.sort();
        terms.dedup();
        let chunks: Vec<(&Note, &Chunk)> =
            self.notes.values().flat_map(|note| note.chunks.iter().map(move |chunk| (note, chunk))).collect();
        let docs: Vec<Vec<String>> = chunks.iter().map(|(note, chunk)| tokens(&embedding_text(note, chunk))).collect();
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
        // Each chunk's relevance, and each note's best chunk.
        let mut best: BTreeMap<&str, (f64, usize)> = BTreeMap::new();
        for (index, ((note, chunk), doc)) in chunks.iter().zip(&docs).enumerate() {
            let length = doc.len() as f64;
            let bm25: f64 = terms
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
            let lexical = bm25 / (bm25 + KAPPA);
            let semantic = query_embedding.and_then(|(model, vector)| {
                let kept = self.vectors.get(&chunk.id).filter(|kept| kept.model == model)?;
                Some(
                    ((cosine(vector, &kept.vector) - EMBEDDING_BASELINE) / (EMBEDDING_FULL - EMBEDDING_BASELINE))
                        .clamp(0.0, 1.0),
                )
            });
            let relevance = match semantic {
                Some(similarity) => similarity + (1.0 - similarity) * KEYWORD_SHARE * lexical,
                None => lexical,
            };
            let kept = best.entry(note.path.as_str()).or_insert((relevance, index));
            if relevance > kept.0 {
                *kept = (relevance, index);
            }
        }
        let hit = |index: usize, score: f64, linked_from: Option<String>| {
            let (note, chunk) = chunks[index];
            VaultHit {
                chunk: chunk.id.clone(),
                path: note.path.clone(),
                title: note.title.clone(),
                heading: chunk.heading.clone(),
                snippet: snippet(&chunk.text),
                score,
                linked_from,
            }
        };
        let mut direct: Vec<(&str, f64, usize)> = best
            .iter()
            .filter(|(_, (relevance, _))| *relevance >= FLOOR)
            .map(|(path, (r, i))| (*path, *r, *i))
            .collect();
        direct.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(b.0)));
        let mut scores: BTreeMap<&str, (f64, usize, Option<String>)> =
            direct.iter().map(|(path, relevance, index)| (*path, (*relevance, *index, None))).collect();
        // One link out from the strongest hits: a linked note with some relevance of its own is
        // lifted in proportion to the hit it is linked to; one with none still comes up, at a
        // share of that hit's score, because the link itself says they belong together.
        for (seed, seed_score, _) in direct.iter().take(SEEDS) {
            for neighbor in self.neighbors(seed) {
                let Some((path, (own, index))) = best.get_key_value(neighbor.as_str()) else { continue };
                let lifted =
                    if *own >= GRAPH_FLOOR { own * (1.0 + GRAPH_BOOST * seed_score) } else { GRAPH_SHARE * seed_score };
                let lifted = lifted.max(GRAPH_SHARE * seed_score).min(1.0);
                let via = (*own < FLOOR).then(|| seed.to_string());
                match scores.get_mut(path) {
                    Some(kept) if kept.0 >= lifted => {}
                    Some(kept) => kept.0 = lifted,
                    None => {
                        scores.insert(path, (lifted, *index, via));
                    }
                }
            }
        }
        let mut hits: Vec<VaultHit> =
            scores.into_values().map(|(score, index, linked_from)| hit(index, score, linked_from)).collect();
        hits.sort_by(|a, b| {
            b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.path.cmp(&b.path))
        });
        hits.truncate(limit);
        hits
    }
}

/// What a chunk is found by and embedded from: its note's title and tags, its heading, its text.
fn embedding_text(note: &Note, chunk: &Chunk) -> String {
    let mut text = note.title.clone();
    if !chunk.heading.is_empty() && chunk.heading != note.title {
        text.push_str(" / ");
        text.push_str(&chunk.heading);
    }
    if !note.tags.is_empty() || !note.aliases.is_empty() {
        text.push_str(&format!(" ({})", note.tags.iter().chain(&note.aliases).cloned().collect::<Vec<_>>().join(", ")));
    }
    text.push('\n');
    text.push_str(&chunk.text);
    text
}

/// The start of a chunk, on one line, for a list of results.
fn snippet(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut short: String = flat.chars().take(240).collect();
    if short.len() < flat.len() {
        short.push('…');
    }
    short
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::WriteMode;

    fn vault() -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(&dir.path().join("vault")).unwrap();
        (dir, vault)
    }

    #[test]
    fn a_note_is_read_into_frontmatter_heading_chunks_and_links() {
        let source = "---\ntitle: Atlas site\nkind: project\ntags: [web, deploy]\naliases:\n  - atlas\n---\n\
            Intro about the site, hosted on [[srv-mini]].\n\n# Deploy\nRun the script. See [[Deploy playbook|the playbook]].\n\n\
            ## Rollback\nRestore the last release.\n\n# Deploy\nA second section of the same name.\n";
        let note = parse_note("30-entities/atlas.md", source);
        assert_eq!((note.title.as_str(), note.kind.as_str()), ("Atlas site", "project"));
        assert_eq!(
            (note.tags.clone(), note.aliases.clone()),
            (vec!["web".to_string(), "deploy".into()], vec!["atlas".to_string()])
        );
        let ids: Vec<&str> = note.chunks.iter().map(|chunk| chunk.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "30-entities/atlas.md#top",
                "30-entities/atlas.md#deploy",
                "30-entities/atlas.md#rollback",
                "30-entities/atlas.md#deploy-2"
            ]
        );
        assert_eq!(note.links, ["deploy playbook", "srv-mini"]);
        assert!(note.chunks[1].text.starts_with("# Deploy"));

        // A long section is split at paragraph breaks, each piece its own chunk.
        let long =
            format!("# Log\n{}", vec!["A paragraph of some length that fills space.".repeat(12); 5].join("\n\n"));
        let split = parse_note("a.md", &long);
        assert!(split.chunks.len() >= 2 && split.chunks.iter().all(|c| c.text.chars().count() <= MAX_CHUNK_CHARS));
        assert_eq!(split.chunks[1].id, "a.md#log-2");
        // Without frontmatter the first heading, or the file's name, is the title.
        assert_eq!(parse_note("x/plain.md", "No heading here.").title, "plain");
    }

    #[test]
    fn a_search_finds_chunks_by_words_and_brings_linked_notes_along() {
        let (_dir, vault) = vault();
        let write = |path: &str, text: &str| vault.write(path, text, WriteMode::Replace).unwrap();
        write(
            "30-entities/atlas.md",
            "---\naliases: [atlas]\n---\n# Atlas site\nThe marketing site. Deployed from the main branch to [[srv-mini]].\n",
        );
        write("30-entities/srv-mini.md", "# srv-mini\nA Mac mini in the office, 10.0.0.5, runs Caddy.\n");
        write("20-playbooks/release.md", "# Release checklist\nFor [[atlas]]: tag, build, upload, verify.\n");
        write("30-entities/garden.md", "# Garden\nTomatoes are planted in May.\n");
        let index = VaultIndex::open(&vault);

        let hits = index.search("atlas marketing site deploy", 5, None);
        assert_eq!(hits[0].path, "30-entities/atlas.md");
        let paths: Vec<&str> = hits.iter().map(|hit| hit.path.as_str()).collect();
        // The server page says nothing about the site, and comes up because the site links to it;
        // the playbook links to the site. The garden is neither relevant nor linked.
        assert!(paths.contains(&"30-entities/srv-mini.md") && paths.contains(&"20-playbooks/release.md"), "{paths:?}");
        assert!(!paths.contains(&"30-entities/garden.md"));
        let server = hits.iter().find(|hit| hit.path == "30-entities/srv-mini.md").unwrap();
        assert_eq!(server.linked_from.as_deref(), Some("30-entities/atlas.md"));
        assert!(server.score < hits[0].score, "a linked note never outranks the hit that brought it");
        assert!(index.search("quantum chromodynamics", 5, None).is_empty(), "nothing relevant, nothing linked");
        assert_eq!(index.neighbors("30-entities/atlas.md").len(), 2);
    }

    #[test]
    fn the_index_follows_the_files_and_keeps_vectors_only_for_unchanged_text() {
        let (_dir, vault) = vault();
        vault.write("00-inbox/a.md", "# A\nFirst text.\n", WriteMode::Replace).unwrap();
        let mut index = VaultIndex::open(&vault);
        assert_eq!(index.unembedded("model-1", 10).len(), 1);
        index.set_vector("00-inbox/a.md#a", "model-1", vec![1.0, 0.0]);
        assert!(index.unembedded("model-1", 10).is_empty());
        assert_eq!(index.unembedded("model-2", 10).len(), 1, "another model's vectors do not count");
        index.save(&vault).unwrap();

        // Opened again, the vector is still there; a query vector from the same model ranks by it.
        let again = VaultIndex::open(&vault);
        assert!(again.unembedded("model-1", 10).is_empty());
        let by_meaning = again.search("unrelated words", 5, Some(("model-1", &[1.0, 0.0])));
        assert_eq!(by_meaning.len(), 1, "found by its vector alone");

        // Changed text is embedded again; a removed note leaves the index.
        vault.write("00-inbox/a.md", "# A\nOther text.\n", WriteMode::Replace).unwrap();
        index.note_written(&vault, "00-inbox/a.md");
        assert_eq!(index.unembedded("model-1", 10).len(), 1);
        std::fs::remove_file(vault.root().join("00-inbox/a.md")).unwrap();
        assert_eq!(index.refresh(&vault), 1);
        assert!(index.notes().next().is_none());
        // The index file is the vault's own: no tool path reaches it.
        assert!(vault.read(INDEX_FILE).is_err());
    }
}
