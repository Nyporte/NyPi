//! The agent's hands on its own vault and asset registry (decision D77): `vault_search`,
//! `vault_read`, `vault_write`, `vault_list`, `vault_attach`, `vault_archive`, and
//! `asset_manage`. They run in the harness's process, because NyPi's home is closed to every
//! subprocess; the vault checks each path (`Vault::resolve`), and a write is in the index before
//! the call returns. A failure is a result the model reads, never a panic.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use nypi_ai::embeddings::{EmbedKind, Embeddings};
use serde_json::{Value, json};

use crate::assets::{AssetChange, AssetKind, AssetRegistry, AssetSteward, line};
use crate::index::{VaultHit, VaultIndex};
use crate::vault::{Vault, VaultError, WriteMode};

const DEFAULT_HITS: usize = 6;
const MAX_HITS: usize = 20;
/// Chunks embedded on the way into one search, so a vault written to in bulk catches up over a
/// few searches instead of holding one up.
const EMBED_PER_SEARCH: usize = 24;

/// Says why a file on this computer may not be read (a key file), or None when it may: the
/// runtime's `Protected`, which this crate does not know.
pub type ReadGuard = Arc<dyn Fn(&Path) -> Option<String> + Send + Sync>;

/// The vault with its index, as the tools and the runtime share it.
pub struct VaultStore {
    vault: Vault,
    index: Mutex<VaultIndex>,
    embeddings: Option<Arc<Embeddings>>,
}

impl VaultStore {
    /// Open the vault at `root` (made when missing) and read its index.
    pub fn open(root: &Path, embeddings: Option<Arc<Embeddings>>) -> Result<Arc<VaultStore>, VaultError> {
        let vault = Vault::open(root)?;
        let index = Mutex::new(VaultIndex::open(&vault));
        Ok(Arc::new(VaultStore { vault, index, embeddings }))
    }

    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// Write a note and take it into the index at once.
    pub fn write(&self, path: &str, content: &str, mode: WriteMode) -> Result<String, VaultError> {
        let written = self.vault.write(path, content, mode)?;
        self.index.lock().unwrap().note_written(&self.vault, &written);
        Ok(written)
    }

    pub fn read(&self, path: &str) -> Result<String, VaultError> {
        self.vault.read(path)
    }

    /// Move a workspace to the archive and let the index follow.
    pub fn archive(&self, workspace: &str, month: &str) -> Result<String, VaultError> {
        let archived = self.vault.archive(workspace, month)?;
        self.index.lock().unwrap().refresh(&self.vault);
        Ok(archived)
    }

    /// Look at the files again (something other than these tools may have written a note: the
    /// harness, an inbound message) and embed what has no vector yet, at most `limit` chunks.
    pub async fn catch_up(&self, limit: usize) -> usize {
        let todo = {
            let mut index = self.index.lock().unwrap();
            index.refresh(&self.vault);
            match self.embeddings.as_ref().and_then(|e| e.active()).map(|e| e.reference()) {
                Some(model) => index.unembedded(&model, limit),
                None => Vec::new(),
            }
        };
        let Some(embeddings) = self.embeddings.as_ref().filter(|_| !todo.is_empty()) else { return 0 };
        let mut done = 0;
        for batch in todo.chunks(8) {
            let texts: Vec<String> = batch.iter().map(|(_, text)| text.clone()).collect();
            // An embedding source that fails leaves the search to words; it is not the search's error.
            let Ok((model, vectors)) = embeddings.embed(&texts, EmbedKind::Document).await else { break };
            let mut index = self.index.lock().unwrap();
            for ((id, _), vector) in batch.iter().zip(vectors) {
                index.set_vector(id, &model, vector);
                done += 1;
            }
        }
        if done > 0 {
            let _ = self.index.lock().unwrap().save(&self.vault);
        }
        done
    }

    /// The best chunks for `query`, by meaning when an embedding source works and by words
    /// either way, with the notes one link from the best hits brought along.
    pub async fn search(&self, query: &str, limit: usize) -> Vec<VaultHit> {
        self.catch_up(EMBED_PER_SEARCH).await;
        let embedded = match &self.embeddings {
            Some(embeddings) => embeddings.embed_one(query, EmbedKind::Query).await.ok(),
            None => None,
        };
        let index = self.index.lock().unwrap();
        index.search(query, limit, embedded.as_ref().map(|(model, vector)| (model.as_str(), vector.as_slice())))
    }

    /// The best chunks for `query` by words alone, each with its whole text and the notes one
    /// link from it, without waiting: None while the index is in use (decision D88, a briefing
    /// put together before a model call must never hold it up).
    pub fn search_now(&self, query: &str, limit: usize) -> Option<Vec<(VaultHit, String, Vec<String>)>> {
        let index = self.index.try_lock().ok()?;
        let hits = index.search(query, limit, None);
        Some(
            hits.into_iter()
                .map(|hit| {
                    let text = index
                        .note(&hit.path)
                        .and_then(|note| note.chunks.iter().find(|chunk| chunk.id == hit.chunk))
                        .map(|chunk| chunk.text.clone())
                        .unwrap_or_else(|| hit.snippet.clone());
                    let links = index.neighbors(&hit.path).into_iter().collect();
                    (hit, text, links)
                })
                .collect(),
        )
    }

    /// One chunk of a note by its heading (as written, or as its slug), or None.
    pub fn section(&self, path: &str, heading: &str) -> Option<String> {
        let index = self.index.lock().unwrap();
        let note = index.note(path)?;
        let wanted = crate::vault::slug(heading);
        note.chunks
            .iter()
            .find(|chunk| {
                chunk.heading.eq_ignore_ascii_case(heading.trim()) || chunk.id.ends_with(&format!("#{wanted}"))
            })
            .map(|chunk| chunk.text.clone())
    }
}

#[derive(Clone, Copy)]
enum Action {
    Search,
    Read,
    Write,
    List,
    Attach,
    Archive,
}

struct VaultTool {
    action: Action,
    store: Arc<VaultStore>,
    read_guard: ReadGuard,
    /// Where a relative `source` of `vault_attach` is looked for.
    cwd: PathBuf,
}

/// The six vault tools. `read_guard` decides which files on this computer `vault_attach` may
/// copy in; `cwd` is where its relative paths start.
pub fn vault_tools(store: &Arc<VaultStore>, read_guard: ReadGuard, cwd: &Path) -> Vec<Arc<dyn Tool>> {
    [Action::Search, Action::Read, Action::Write, Action::List, Action::Attach, Action::Archive]
        .into_iter()
        .map(|action| {
            Arc::new(VaultTool { action, store: store.clone(), read_guard: read_guard.clone(), cwd: cwd.to_path_buf() })
                as Arc<dyn Tool>
        })
        .collect()
}

/// The names of the vault's tools, for a runtime that offers them as one set.
pub const VAULT_TOOLS: [&str; 6] =
    ["vault_search", "vault_read", "vault_write", "vault_list", "vault_attach", "vault_archive"];

fn refused(error: VaultError) -> ToolOutput {
    ToolOutput::error(error.to_string())
}

fn text_of(args: &Value, key: &str) -> Option<String> {
    args[key].as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_string)
}

#[async_trait]
impl Tool for VaultTool {
    fn definition(&self) -> ToolDefinition {
        let (name, description, parameters) = match self.action {
            Action::Search => (
                "vault_search",
                "Search your vault: your own Markdown notes (00-inbox, 10-workspaces, 20-playbooks, 30-entities, \
                 90-archive). Finds sections by meaning and by words, and brings along the notes linked to or from \
                 the best hits. Search before starting work on a project, system, or person you have notes on.",
                json!({
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer", "description": "How many results (default 6)" }
                    }
                }),
            ),
            Action::Read => (
                "vault_read",
                "Read a note of your vault by its path (relative to the vault), whole, or one section by `heading`.",
                json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": {
                        "path": { "type": "string", "description": "For example 30-entities/atlas.md" },
                        "heading": { "type": "string", "description": "Only the section under this heading" }
                    }
                }),
            ),
            Action::Write => (
                "vault_write",
                "Write a Markdown note in your vault (the only way to change it; shell commands cannot). `replace` \
                 writes the whole note, `append` adds to its end. Link related notes with [[note name]]; optional \
                 frontmatter between --- lines: title, kind, tags, status, aliases, updated. Never write a password, \
                 key, or token into a note: name it by its cred:// handle.",
                json!({
                    "type": "object",
                    "required": ["path", "content"],
                    "properties": {
                        "path": { "type": "string", "description": "Relative to the vault, ending in .md" },
                        "content": { "type": "string" },
                        "mode": { "type": "string", "enum": ["replace", "append"], "description": "Default replace" }
                    }
                }),
            ),
            Action::List => (
                "vault_list",
                "List a folder of your vault (the top without a path).",
                json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            ),
            Action::Attach => (
                "vault_attach",
                "Copy a file from this computer (a screenshot, a PDF, an export) into your vault's attachments/, \
                 so a note can refer to it after the original is gone.",
                json!({
                    "type": "object",
                    "required": ["source"],
                    "properties": {
                        "source": { "type": "string", "description": "Path of the file to copy in" },
                        "name": { "type": "string", "description": "Name to keep it under (default: its own)" }
                    }
                }),
            ),
            Action::Archive => (
                "vault_archive",
                "Move a finished workspace (a folder under 10-workspaces/) into 90-archive/<YYYY-MM>/.",
                json!({
                    "type": "object",
                    "required": ["workspace"],
                    "properties": {
                        "workspace": { "type": "string", "description": "The workspace's folder name" },
                        "month": { "type": "string", "description": "YYYY-MM (default: this month)" }
                    }
                }),
            ),
        };
        ToolDefinition { name: name.into(), description: description.into(), parameters }
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let store = &self.store;
        Ok(match self.action {
            Action::Search => {
                let Some(query) = text_of(&args, "query") else {
                    return Ok(ToolOutput::error("`query` must not be empty"));
                };
                let limit = args["limit"].as_u64().map_or(DEFAULT_HITS, |n| (n as usize).clamp(1, MAX_HITS));
                let hits = store.search(&query, limit).await;
                if hits.is_empty() {
                    return Ok(ToolOutput::text("Nothing in the vault matches."));
                }
                let lines: Vec<String> = hits
                    .iter()
                    .map(|hit| {
                        let via =
                            hit.linked_from.as_ref().map(|from| format!(" (linked from {from})")).unwrap_or_default();
                        format!("- {} · {}{via}\n  {}", hit.chunk, hit.title, hit.snippet)
                    })
                    .collect();
                ToolOutput::text(lines.join("\n")).with_details(json!({
                    "hits": hits.iter().map(|hit| json!({ "chunk": hit.chunk, "score": hit.score, "linkedFrom": hit.linked_from })).collect::<Vec<_>>()
                }))
            }
            Action::Read => {
                let Some(path) = text_of(&args, "path") else { return Ok(ToolOutput::error("`path` is required")) };
                // `path#heading`, as a search result names a section, reads that section.
                let (path, in_path) = match path.split_once('#') {
                    Some((path, heading)) => (path.to_string(), Some(heading.to_string())),
                    None => (path, None),
                };
                match text_of(&args, "heading").or(in_path) {
                    Some(heading) => match store.vault().resolve(&path) {
                        Err(error) => refused(error),
                        Ok(resolved) => match store.section(&store.vault().relative(&resolved), &heading) {
                            Some(section) => ToolOutput::text(section),
                            None => ToolOutput::error(format!("{path} has no section \"{heading}\"")),
                        },
                    },
                    None => match store.read(&path) {
                        Ok(text) => ToolOutput::text(text),
                        Err(error) => refused(error),
                    },
                }
            }
            Action::Write => {
                let Some(path) = text_of(&args, "path") else { return Ok(ToolOutput::error("`path` is required")) };
                let content = args["content"].as_str().unwrap_or_default();
                if let Some(what) = crate::secrets::looks_secret(content) {
                    return Ok(ToolOutput::error(format!(
                        "The note looks like it holds {what}. Notes never hold secrets: the user stores it with \
                         `nypi secrets set <scope>/<name>`, and the note names it as cred://<scope>/<name>."
                    )));
                }
                let mode = if args["mode"] == "append" { WriteMode::Append } else { WriteMode::Replace };
                match store.write(&path, content, mode) {
                    Ok(written) => ToolOutput::text(format!("Wrote {written} ({} bytes).", content.len()))
                        .with_details(json!({ "path": written })),
                    Err(error) => refused(error),
                }
            }
            Action::List => match store.vault().list(args["path"].as_str().unwrap_or_default()) {
                Ok(entries) if entries.is_empty() => ToolOutput::text("(empty)"),
                Ok(entries) => ToolOutput::text(
                    entries
                        .iter()
                        .map(|entry| {
                            if entry.is_dir {
                                format!("{}/", entry.path)
                            } else {
                                format!("{} ({} bytes)", entry.path, entry.size)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                Err(error) => refused(error),
            },
            Action::Attach => {
                let Some(source) = text_of(&args, "source") else {
                    return Ok(ToolOutput::error("`source` is required"));
                };
                let source = if let Some(rest) = source.strip_prefix("~/") {
                    dirs::home_dir().unwrap_or_default().join(rest)
                } else {
                    self.cwd.join(&source)
                };
                if let Some(why) = (self.read_guard)(&source) {
                    return Ok(ToolOutput::error(format!("Refused: {why}")));
                }
                let size = match std::fs::metadata(&source) {
                    Ok(meta) if meta.is_file() => meta.len(),
                    _ => return Ok(ToolOutput::error(format!("{} is not a file", source.display()))),
                };
                if size > crate::vault::MAX_ATTACHMENT_BYTES {
                    return Ok(ToolOutput::error(format!("{} is {size} bytes, too large to attach", source.display())));
                }
                let bytes = match std::fs::read(&source) {
                    Ok(bytes) => bytes,
                    Err(error) => return Ok(ToolOutput::error(format!("cannot read {}: {error}", source.display()))),
                };
                let name = text_of(&args, "name")
                    .or_else(|| source.file_name().map(|name| name.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                match store.vault().attach(&name, &bytes) {
                    Ok(kept) => ToolOutput::text(format!("Attached as {kept} ({size} bytes)."))
                        .with_details(json!({ "path": kept })),
                    Err(error) => refused(error),
                }
            }
            Action::Archive => {
                let Some(workspace) = text_of(&args, "workspace") else {
                    return Ok(ToolOutput::error("`workspace` is required"));
                };
                let month = text_of(&args, "month").unwrap_or_else(|| chrono::Local::now().format("%Y-%m").to_string());
                match store.archive(&workspace, &month) {
                    Ok(archived) => ToolOutput::text(format!("Archived to {archived}.")),
                    Err(error) => refused(error),
                }
            }
        })
    }
}

/// `asset_manage`: the registry of what Lucia looks after (assets.rs).
pub struct AssetTool {
    pub registry: Arc<AssetRegistry>,
}

#[async_trait]
impl Tool for AssetTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "asset_manage".into(),
            description: "The registry of what you look after for the user and for yourself: accounts, servers, \
                domains, subscriptions, identities, deliverables. list, get, upsert (add, or change the record `id` \
                names), remove, expiring_within_days. A record holds how the thing is named and when it renews, \
                never a secret: secrets are named by handle in secretRefs (cred://<scope>/<name>), and the owner \
                stores their values with `nypi secrets set`."
                .into(),
            parameters: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "upsert", "remove", "expiring_within_days"] },
                    "id": { "type": "string" },
                    "kind": { "type": "string", "enum": ["account", "server", "domain", "subscription", "identity", "deliverable"] },
                    "steward": { "type": "string", "enum": ["owner", "agent"], "description": "Whose it is (default owner)" },
                    "name": { "type": "string" },
                    "identifier": { "type": "string", "description": "Non-secret handle: a domain, user name, host, URL" },
                    "secretRefs": { "type": "array", "items": { "type": "string" }, "description": "cred:// handles only" },
                    "renewsAt": { "type": "string", "description": "YYYY-MM-DD; empty clears it" },
                    "notes": { "type": "string" },
                    "days": { "type": "integer", "description": "For expiring_within_days (default 30)" }
                }
            }),
        }
    }

    async fn execute(&self, args: Value, _ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let registry = &self.registry;
        Ok(match args["action"].as_str().unwrap_or_default() {
            "list" => {
                let records = registry.list();
                if records.is_empty() {
                    ToolOutput::text("The registry is empty.")
                } else {
                    ToolOutput::text(records.iter().map(line).collect::<Vec<_>>().join("\n"))
                }
            }
            "get" => match text_of(&args, "id").and_then(|id| registry.get(&id)) {
                Some(record) => {
                    let notes = if record.notes.is_empty() { String::new() } else { format!("\n{}", record.notes) };
                    ToolOutput::text(format!("{}{notes}\nupdated {}", line(&record), record.updated_at))
                }
                None => ToolOutput::error("No such asset (list shows the ids)"),
            },
            "upsert" => {
                let kind = match args["kind"].as_str() {
                    Some(kind) => match AssetKind::parse(kind) {
                        Some(kind) => Some(kind),
                        None => return Ok(ToolOutput::error(format!("Unknown kind: {kind}"))),
                    },
                    None => None,
                };
                let steward = match args["steward"].as_str() {
                    Some("agent") => Some(AssetSteward::Agent),
                    Some("owner") => Some(AssetSteward::Owner),
                    Some(other) => return Ok(ToolOutput::error(format!("Unknown steward: {other}"))),
                    None => None,
                };
                let change = AssetChange {
                    id: text_of(&args, "id"),
                    kind,
                    steward,
                    name: args["name"].as_str().map(str::to_string),
                    identifier: args["identifier"].as_str().map(str::to_string),
                    secret_refs: args["secretRefs"]
                        .as_array()
                        .map(|refs| refs.iter().filter_map(|r| r.as_str().map(str::to_string)).collect()),
                    renews_at: args["renewsAt"]
                        .as_str()
                        .map(|at| Some(at.to_string()).filter(|at| !at.trim().is_empty())),
                    notes: args["notes"].as_str().map(str::to_string),
                };
                match registry.upsert(change) {
                    Ok(record) => ToolOutput::text(format!("Saved: {}", line(&record))),
                    Err(error) => ToolOutput::error(error),
                }
            }
            "remove" => match text_of(&args, "id").map(|id| registry.remove(&id)) {
                Some(Ok(true)) => ToolOutput::text("Removed."),
                Some(Ok(false)) | None => ToolOutput::error("No such asset (list shows the ids)"),
                Some(Err(error)) => ToolOutput::error(error),
            },
            "expiring_within_days" => {
                let days = args["days"].as_i64().unwrap_or(30).clamp(0, 3650);
                let due = registry.expiring_within(days, chrono::Local::now().date_naive());
                if due.is_empty() {
                    ToolOutput::text(format!("Nothing renews or expires within {days} days."))
                } else {
                    let lines: Vec<String> = due
                        .iter()
                        .map(|(record, left)| match left {
                            left if *left < 0 => format!("{} · {} days past", line(record), -left),
                            left => format!("{} · in {left} days", line(record)),
                        })
                        .collect();
                    ToolOutput::text(lines.join("\n"))
                }
            }
            other => ToolOutput::error(format!("Unknown action: {other}")),
        })
    }
}
