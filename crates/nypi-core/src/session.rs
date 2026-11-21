//! JSONL session files in Pi's v3 format: a header line, then entries that form a tree via
//! `parentId`. The active branch is the path from the root to the current leaf.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use nypi_ai::{ContentBlock, Message, UserMessage, content_from_string_or_blocks, now_ms, text_of};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SESSION_VERSION: u32 = 3;

const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// Fields every tree entry carries.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Entry {
    Message {
        #[serde(flatten)]
        meta: Meta,
        message: Message,
    },
    ModelChange {
        #[serde(flatten)]
        meta: Meta,
        provider: String,
        model_id: String,
    },
    ThinkingLevelChange {
        #[serde(flatten)]
        meta: Meta,
        thinking_level: String,
    },
    Compaction {
        #[serde(flatten)]
        meta: Meta,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    /// Pi's edit of an earlier message in model context: `None` omits it, a replacement swaps its
    /// content. The message itself stays in the file.
    ContextEdit {
        #[serde(flatten)]
        meta: Meta,
        target_id: String,
        replacement: Option<Replacement>,
    },
    /// A session's display name; an empty name clears it (Pi's `session_info`).
    SessionInfo {
        #[serde(flatten)]
        meta: Meta,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Entry types NyPi does not interpret (Pi labels, custom entries, ...). Kept for tree integrity.
    #[serde(skip)]
    Other { meta: Meta, raw: Value },
}

impl Entry {
    pub fn meta(&self) -> &Meta {
        match self {
            Entry::Message { meta, .. }
            | Entry::ModelChange { meta, .. }
            | Entry::ThinkingLevelChange { meta, .. }
            | Entry::Compaction { meta, .. }
            | Entry::ContextEdit { meta, .. }
            | Entry::SessionInfo { meta, .. }
            | Entry::Other { meta, .. } => meta,
        }
    }

    pub fn id(&self) -> &str {
        &self.meta().id
    }

    /// The entry as it is stored, including the fields of entry types NyPi does not interpret.
    pub fn to_json(&self) -> Value {
        match self {
            Entry::Other { raw, .. } => raw.clone(),
            entry => serde_json::to_value(entry).unwrap_or(Value::Null),
        }
    }

    fn set_parent(&mut self, parent_id: Option<String>) {
        if let Entry::Other { raw, .. } = self {
            raw["parentId"] = parent_id.clone().map_or(Value::Null, Value::String);
        }
        match self {
            Entry::Message { meta, .. }
            | Entry::ModelChange { meta, .. }
            | Entry::ThinkingLevelChange { meta, .. }
            | Entry::Compaction { meta, .. }
            | Entry::ContextEdit { meta, .. }
            | Entry::SessionInfo { meta, .. }
            | Entry::Other { meta, .. } => meta.parent_id = parent_id,
        }
    }

    /// The type name in the file.
    fn kind(&self) -> &str {
        match self {
            Entry::Other { raw, .. } => raw["type"].as_str().unwrap_or_default(),
            Entry::Message { .. } => "message",
            Entry::ModelChange { .. } => "model_change",
            Entry::ThinkingLevelChange { .. } => "thinking_level_change",
            Entry::Compaction { .. } => "compaction",
            Entry::ContextEdit { .. } => "context_edit",
            Entry::SessionInfo { .. } => "session_info",
        }
    }
}

/// New content for an edited message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Replacement {
    #[serde(deserialize_with = "content_from_string_or_blocks")]
    pub content: Vec<ContentBlock>,
}

/// The context edits on a branch by target entry, the latest one winning (Pi's projection).
pub(crate) type Edits = HashMap<String, Option<Replacement>>;

/// What the active branch implies for the next request.
#[derive(Clone, Debug, Default)]
pub struct SessionContext {
    pub messages: Vec<Message>,
    /// Last model selected on the branch, as `provider/id`.
    pub model: Option<String>,
    pub thinking_level: Option<String>,
}

/// Summary row for session pickers.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub id: String,
    pub modified: std::time::SystemTime,
    pub first_message: String,
    pub message_count: usize,
}

pub struct SessionStore {
    pub path: PathBuf,
    pub header: Header,
    entries: Vec<Entry>,
    by_id: HashMap<String, usize>,
    leaf: Option<String>,
    header_written: bool,
}

impl SessionStore {
    /// New session for `cwd` under `sessions_dir`. The file is created on the first append.
    pub fn create(sessions_dir: &Path, cwd: &Path) -> Self {
        let id = uuid::Uuid::now_v7().to_string();
        let now = chrono::Utc::now();
        let file = format!("{}_{}.jsonl", now.format("%Y-%m-%dT%H-%M-%S-%3fZ"), id);
        SessionStore {
            path: session_dir(sessions_dir, cwd).join(file),
            header: Header {
                kind: "session".into(),
                version: SESSION_VERSION,
                id,
                timestamp: now.to_rfc3339(),
                cwd: cwd.display().to_string(),
                parent_session: None,
            },
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf: None,
            header_written: false,
        }
    }

    /// In-memory session that never touches disk (`--no-session`).
    pub fn ephemeral(cwd: &Path) -> Self {
        let mut store = Self::create(Path::new(""), cwd);
        store.path = PathBuf::new();
        store
    }

    pub fn is_persistent(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }

    /// Load a session file; the leaf is the last entry.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("Cannot read session {}", path.display()))?;
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header: Header = serde_json::from_str(lines.next().unwrap_or_default())
            .with_context(|| format!("Invalid session header in {}", path.display()))?;
        if header.kind != "session" {
            bail!("{} is not a session file", path.display());
        }
        let mut store = SessionStore {
            path: path.to_path_buf(),
            header,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf: None,
            header_written: true,
        };
        for line in lines {
            if let Some(entry) = parse_entry(line) {
                store.index(entry);
            }
        }
        Ok(store)
    }

    /// Most recently modified session for `cwd`.
    /// The hub's one conversation (decision D79): the newest session in `sessions/primary`,
    /// whatever folder the hub was started in, or a new one there.
    pub fn primary(sessions_dir: &Path, cwd: &Path) -> anyhow::Result<Self> {
        let dir = sessions_dir.join(PRIMARY_DIR);
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
            .unwrap_or_default();
        files.retain(|path| path.extension().is_some_and(|x| x == "jsonl"));
        // A session's file name begins with when it was made.
        files.sort();
        match files.pop() {
            Some(newest) => SessionStore::open(&newest),
            None => {
                let mut store = SessionStore::create(sessions_dir, cwd);
                if let Some(name) = store.path.file_name().map(|name| name.to_os_string()) {
                    store.path = dir.join(name);
                }
                Ok(store)
            }
        }
    }

    pub fn most_recent(sessions_dir: &Path, cwd: &Path) -> Option<PathBuf> {
        Self::list(sessions_dir, cwd).into_iter().next().map(|info| info.path)
    }

    /// Sessions for `cwd`, newest first.
    pub fn list(sessions_dir: &Path, cwd: &Path) -> Vec<SessionInfo> {
        let Ok(dir) = std::fs::read_dir(session_dir(sessions_dir, cwd)) else { return Vec::new() };
        let mut infos: Vec<SessionInfo> = dir
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .filter_map(|e| {
                let store = SessionStore::open(&e.path()).ok()?;
                let messages: Vec<&Message> = store
                    .entries
                    .iter()
                    .filter_map(|entry| match entry {
                        Entry::Message { message, .. } => Some(message),
                        _ => None,
                    })
                    .collect();
                let first_message = messages
                    .iter()
                    .find_map(|m| match m {
                        Message::User(u) => Some(text_of(&u.content)),
                        _ => None,
                    })
                    .unwrap_or_default();
                Some(SessionInfo {
                    path: e.path(),
                    id: store.header.id.clone(),
                    modified: e.metadata().and_then(|m| m.modified()).ok()?,
                    first_message,
                    message_count: messages.len(),
                })
            })
            .collect();
        infos.sort_by_key(|info| std::cmp::Reverse(info.modified));
        infos
    }

    pub fn leaf(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn append_message(&mut self, message: Message) -> anyhow::Result<String> {
        let meta = self.next_meta();
        self.append(Entry::Message { meta, message })
    }

    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> anyhow::Result<String> {
        let meta = self.next_meta();
        self.append(Entry::ModelChange { meta, provider: provider.into(), model_id: model_id.into() })
    }

    pub fn append_thinking_change(&mut self, level: &str) -> anyhow::Result<String> {
        let meta = self.next_meta();
        self.append(Entry::ThinkingLevelChange { meta, thinking_level: level.into() })
    }

    pub fn append_compaction(
        &mut self,
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
        details: Option<Value>,
    ) -> anyhow::Result<String> {
        let meta = self.next_meta();
        self.append(Entry::Compaction { meta, summary, first_kept_entry_id, tokens_before, details })
    }

    /// Omit (`None`) or replace the content of an earlier message on the active branch in model
    /// context (Pi's `appendContextEdit`). Only user, assistant, and tool result messages can be
    /// edited.
    pub fn append_context_edit(&mut self, target_id: &str, replacement: Option<Replacement>) -> anyhow::Result<String> {
        let branch = self.branch();
        let Some(target) = branch.iter().find(|e| e.id() == target_id) else {
            bail!("Entry {target_id} is not on the active branch")
        };
        if !matches!(
            target,
            Entry::Message { message: Message::User(_) | Message::Assistant(_) | Message::ToolResult(_), .. }
        ) {
            bail!("Entry {target_id} does not contribute editable model content");
        }
        let meta = self.next_meta();
        self.append(Entry::ContextEdit { meta, target_id: target_id.into(), replacement })
    }

    /// The id of the entry holding `message`, searched from the leaf back.
    pub fn entry_id_of(&self, message: &Message) -> Option<String> {
        self.branch().into_iter().rev().find_map(|entry| match entry {
            Entry::Message { meta, message: m } if m == message => Some(meta.id.clone()),
            _ => None,
        })
    }

    /// The context edits on the active branch.
    pub(crate) fn edits(&self) -> Edits {
        self.branch()
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::ContextEdit { target_id, replacement, .. } => Some((target_id.clone(), replacement.clone())),
                _ => None,
            })
            .collect()
    }

    /// Entries from the root to the current leaf.
    pub fn branch(&self) -> Vec<&Entry> {
        self.path_to(self.leaf.as_deref())
    }

    /// Entries from the root to `leaf`.
    fn path_to(&self, leaf: Option<&str>) -> Vec<&Entry> {
        let mut path = Vec::new();
        let mut cursor = leaf.map(str::to_string);
        // A malformed file could hold a cycle; no path is longer than the file.
        while let Some(id) = cursor
            && path.len() < self.entries.len()
        {
            let Some(&index) = self.by_id.get(&id) else { break };
            let entry = &self.entries[index];
            cursor = entry.meta().parent_id.clone();
            path.push(entry);
        }
        path.reverse();
        path
    }

    /// Record a shell command the user ran, as Pi's `bashExecution` message entry.
    pub fn append_bash_execution(&mut self, run: &BashExecution) -> anyhow::Result<String> {
        let meta = self.next_meta();
        let mut message = serde_json::to_value(run)?;
        message["role"] = Value::String("bashExecution".into());
        let raw = serde_json::json!({
            "type": "message", "id": meta.id, "parentId": meta.parent_id, "timestamp": meta.timestamp,
            "message": message,
        });
        self.append(Entry::Other { meta, raw })
    }

    /// Name the session (Pi's `appendSessionInfo`); line breaks become spaces.
    pub fn append_session_info(&mut self, name: &str) -> anyhow::Result<String> {
        let name = name.split(['\r', '\n']).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ");
        let meta = self.next_meta();
        self.append(Entry::SessionInfo { meta, name: Some(name.trim().to_string()) })
    }

    /// The name from the newest `session_info` entry; an empty one clears it.
    pub fn session_name(&self) -> Option<String> {
        self.entries
            .iter()
            .rev()
            .find_map(|e| match e {
                Entry::SessionInfo { name, .. } => Some(name.clone().unwrap_or_default()),
                _ => None,
            })
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
    }

    /// The newest label on each entry, with when it was set; a label without text clears it.
    fn labels(&self) -> HashMap<String, (String, String)> {
        let mut labels = HashMap::new();
        for entry in &self.entries {
            if let Entry::Other { meta, raw } = entry
                && raw["type"] == "label"
                && let Some(target) = raw["targetId"].as_str()
            {
                match raw["label"].as_str().filter(|l| !l.is_empty()) {
                    Some(label) => labels.insert(target.to_string(), (label.to_string(), meta.timestamp.clone())),
                    None => labels.remove(target),
                };
            }
        }
        labels
    }

    /// The whole tree as Pi's `getTree` gives it: each node's entry, its children oldest first,
    /// and its label. Built from the leaves up, without recursion: an unbranched session is one
    /// chain as deep as it is long.
    pub fn tree_json(&self) -> Vec<Value> {
        let labels = self.labels();
        let mut children: Vec<Vec<(String, Value)>> = vec![Vec::new(); self.entries.len()];
        let mut roots = Vec::new();
        for (index, entry) in self.entries.iter().enumerate().rev() {
            let mut kids = std::mem::take(&mut children[index]);
            kids.sort_by(|a, b| a.0.cmp(&b.0));
            let mut node = serde_json::Map::new();
            node.insert("entry".into(), entry.to_json());
            node.insert("children".into(), Value::Array(kids.into_iter().map(|(_, n)| n).collect()));
            if let Some((label, at)) = labels.get(entry.id()) {
                node.insert("label".into(), Value::String(label.clone()));
                node.insert("labelTimestamp".into(), Value::String(at.clone()));
            }
            let item = (entry.meta().timestamp.clone(), Value::Object(node));
            match entry.meta().parent_id.as_deref().filter(|p| *p != entry.id()).and_then(|p| self.by_id.get(p)) {
                // A parent always comes earlier in the file, so it has not been built yet.
                Some(&parent) if parent < index => children[parent].push(item),
                _ => roots.push(item.1),
            }
        }
        roots.reverse();
        roots
    }

    /// A new session holding only the path from the root to `leaf_id`, naming this one as its
    /// parent (Pi's `createBranchedSession`). Labels on the path come along, re-created at its
    /// end, so the entries that followed a label are re-chained past it.
    pub fn branched(&self, leaf_id: &str) -> anyhow::Result<SessionStore> {
        if !self.by_id.contains_key(leaf_id) {
            bail!("Entry {leaf_id} not found");
        }
        let mut store = if self.is_persistent() {
            let mut store = Self::create(Path::new(""), Path::new(&self.header.cwd));
            store.path = self.path.with_file_name(store.path.file_name().unwrap_or_default());
            store.header.parent_session = Some(self.path.display().to_string());
            store
        } else {
            Self::ephemeral(Path::new(&self.header.cwd))
        };
        let mut parent: Option<String> = None;
        let mut skipped_labels: Vec<String> = Vec::new();
        let mut moved: HashMap<String, String> = HashMap::new();
        for entry in self.path_to(Some(leaf_id)) {
            if entry.kind() == "label" {
                skipped_labels.push(entry.id().to_string());
                continue;
            }
            for label in skipped_labels.drain(..) {
                moved.insert(label, entry.id().to_string());
            }
            let mut entry = entry.clone();
            entry.set_parent(parent.clone());
            if let Entry::Compaction { first_kept_entry_id, .. } = &mut entry
                && let Some(next) = moved.get(first_kept_entry_id)
            {
                *first_kept_entry_id = next.clone();
            }
            parent = Some(entry.id().to_string());
            store.append(entry)?;
        }
        let mut labels: Vec<(String, (String, String))> =
            self.labels().into_iter().filter(|(target, _)| store.by_id.contains_key(target)).collect();
        labels.sort_by(|a, b| a.1.1.cmp(&b.1.1));
        for (target, (label, at)) in labels {
            let meta = Meta { timestamp: at, ..store.next_meta() };
            let raw = serde_json::json!({
                "type": "label", "id": meta.id, "parentId": meta.parent_id, "timestamp": meta.timestamp,
                "targetId": target, "label": label,
            });
            store.append(Entry::Other { meta, raw })?;
        }
        Ok(store)
    }

    /// A new, empty session naming this one as its parent (Pi's fork before the first message).
    pub fn new_child(&self) -> SessionStore {
        let mut store = Self::create(Path::new(""), Path::new(&self.header.cwd));
        if self.is_persistent() {
            store.path = self.path.with_file_name(store.path.file_name().unwrap_or_default());
            store.header.parent_session = Some(self.path.display().to_string());
        } else {
            store.path = PathBuf::new();
        }
        store
    }

    /// Model context for the active branch, honoring the latest compaction and context edits.
    pub fn context(&self) -> SessionContext {
        let branch = self.branch();
        let edits = self.edits();
        let entry_message = |entry: &&Entry| entry_message(entry, &edits);
        let mut ctx = SessionContext::default();
        for entry in &branch {
            match entry {
                Entry::ModelChange { provider, model_id, .. } => ctx.model = Some(format!("{provider}/{model_id}")),
                Entry::ThinkingLevelChange { thinking_level, .. } => ctx.thinking_level = Some(thinking_level.clone()),
                _ => {}
            }
        }
        let compaction = branch.iter().rposition(|e| matches!(e, Entry::Compaction { .. }));
        let Some(compaction_index) = compaction else {
            ctx.messages = branch.iter().filter_map(entry_message).collect();
            return ctx;
        };
        let Entry::Compaction { summary, first_kept_entry_id, .. } = branch[compaction_index] else { unreachable!() };
        let kept_from = branch[..compaction_index].iter().position(|e| e.id() == first_kept_entry_id);
        // The summarized part held the leading system message and any updates to it: their
        // replay leads the compacted transcript, so the prompt and tools survive compaction.
        let dropped: Vec<Message> =
            branch[..kept_from.unwrap_or(compaction_index)].iter().filter_map(entry_message).collect();
        if nypi_ai::transcript::has_leading_system(&dropped)
            && let Some(head) = nypi_ai::transcript::current_system(&dropped)
        {
            ctx.messages.push(Message::System(head));
        }
        ctx.messages.push(summary_message(summary));
        if let Some(start) = kept_from {
            ctx.messages.extend(branch[start..compaction_index].iter().filter_map(entry_message));
        }
        ctx.messages.extend(branch[compaction_index + 1..].iter().filter_map(entry_message));
        ctx
    }

    fn next_meta(&self) -> Meta {
        let mut id = short_id();
        while self.by_id.contains_key(&id) {
            id = short_id();
        }
        Meta { id, parent_id: self.leaf.clone(), timestamp: chrono::Utc::now().to_rfc3339() }
    }

    fn append(&mut self, entry: Entry) -> anyhow::Result<String> {
        if self.is_persistent() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
            if !self.header_written {
                writeln!(file, "{}", serde_json::to_string(&self.header)?)?;
                self.header_written = true;
            }
            // Typed entries keep their field order (Pi's); the rest are written as they were read.
            let line = match &entry {
                Entry::Other { raw, .. } => raw.to_string(),
                typed => serde_json::to_string(typed)?,
            };
            writeln!(file, "{line}")?;
        }
        let id = entry.id().to_string();
        self.index(entry);
        Ok(id)
    }

    fn index(&mut self, entry: Entry) {
        self.leaf = Some(entry.id().to_string());
        self.by_id.insert(entry.id().to_string(), self.entries.len());
        self.entries.push(entry);
    }
}

fn parse_entry(line: &str) -> Option<Entry> {
    let value: Value = serde_json::from_str(line).ok()?;
    match value["type"].as_str()? {
        "message" | "model_change" | "thinking_level_change" | "compaction" | "context_edit" | "session_info"
            if let Ok(entry) = serde_json::from_value(value.clone()) =>
        {
            Some(entry)
        }
        // Kept as written, so the tree stays whole: Pi's other message roles (`bashExecution`,
        // `custom`, `branchSummary`) are not provider messages, and other entry types are Pi's.
        _ => {
            let meta: Meta = serde_json::from_value(value.clone()).ok()?;
            Some(Entry::Other { meta, raw: value })
        }
    }
}

/// A shell command the user ran, as Pi records it (`!command`; `!!command` keeps it out of
/// model context).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecution {
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exclude_from_context: bool,
}

impl BashExecution {
    /// What the model sees (Pi's `bashExecutionToText`).
    pub fn context_text(&self) -> String {
        let mut text = format!("Ran `{}`\n", self.command);
        if self.output.is_empty() {
            text.push_str("(no output)");
        } else {
            text.push_str(&format!("```\n{}\n```", self.output));
        }
        if self.cancelled {
            text.push_str("\n\n(command cancelled)");
        } else if let Some(code) = self.exit_code.filter(|c| *c != 0) {
            text.push_str(&format!("\n\nCommand exited with code {code}"));
        }
        if let (true, Some(path)) = (self.truncated, &self.full_output_path) {
            text.push_str(&format!("\n\n[Output truncated. Full output: {path}]"));
        }
        text
    }

    /// The user-role message the model sees, or None when it is kept out of context.
    pub fn context_message(&self) -> Option<Message> {
        (!self.exclude_from_context)
            .then(|| user_message(vec![ContentBlock::text(self.context_text())], self.timestamp))
    }
}

const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

fn user_message(content: Vec<ContentBlock>, timestamp: i64) -> Message {
    Message::User(UserMessage { content, timestamp })
}

/// Content Pi stores as a string or as blocks.
#[derive(Deserialize)]
struct PiContent(#[serde(deserialize_with = "content_from_string_or_blocks")] Vec<ContentBlock>);

/// The user-role message a Pi entry NyPi keeps as written puts in context, as Pi's
/// `convertToLlm` turns its non-provider messages into user messages.
fn pi_context_message(raw: &Value) -> Option<Message> {
    let content = |value: &Value| serde_json::from_value::<PiContent>(value.clone()).ok().map(|c| c.0);
    let timestamp = |value: &Value| value.as_i64().unwrap_or_else(now_ms);
    let branch_summary = |summary: &str, at: i64| {
        user_message(vec![ContentBlock::text(format!("{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"))], at)
    };
    match raw["type"].as_str()? {
        "message" => {
            let message = &raw["message"];
            match message["role"].as_str()? {
                "bashExecution" => serde_json::from_value::<BashExecution>(message.clone()).ok()?.context_message(),
                "custom" => Some(user_message(content(&message["content"])?, timestamp(&message["timestamp"]))),
                "branchSummary" => Some(branch_summary(message["summary"].as_str()?, timestamp(&message["timestamp"]))),
                _ => None,
            }
        }
        "custom_message" => Some(user_message(content(&raw["content"]).unwrap_or_default(), now_ms())),
        "branch_summary" => raw["summary"].as_str().filter(|s| !s.is_empty()).map(|s| branch_summary(s, now_ms())),
        _ => None,
    }
}

/// What `entry` puts in model context, after its context edit.
pub(crate) fn entry_message(entry: &Entry, edits: &Edits) -> Option<Message> {
    let (meta, message) = match entry {
        Entry::Message { meta, message } => (meta, message.clone()),
        Entry::Other { meta, raw } => (meta, pi_context_message(raw)?),
        Entry::Compaction { summary, .. } => return Some(summary_message(summary)),
        _ => return None,
    };
    match edits.get(&meta.id) {
        None => Some(message),
        Some(None) => None,
        Some(Some(replacement)) => {
            let mut message = message;
            match &mut message {
                Message::User(m) => m.content = replacement.content.clone(),
                Message::Assistant(m) => m.content = replacement.content.clone(),
                Message::ToolResult(m) => m.content = replacement.content.clone(),
                Message::System(_) => {}
            }
            Some(message)
        }
    }
}

#[cfg(test)]
fn message_content(message: &Message) -> Vec<ContentBlock> {
    match message {
        Message::User(m) => m.content.clone(),
        Message::Assistant(m) => m.content.clone(),
        Message::ToolResult(m) => m.content.clone(),
        Message::System(_) => Vec::new(),
    }
}

/// The user-role message a compaction summary becomes in model context.
pub fn summary_message(summary: &str) -> Message {
    Message::User(UserMessage {
        content: vec![ContentBlock::text(format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"))],
        timestamp: now_ms(),
    })
}

/// True for the user-role message produced by `summary_message`.
pub fn is_summary_message(message: &Message) -> bool {
    matches!(message, Message::User(user)
        if matches!(user.content.first(), Some(ContentBlock::Text { text, .. }) if text.starts_with(COMPACTION_SUMMARY_PREFIX)))
}

/// `sessions/--Users-me-project--/`, Pi's per-cwd directory naming.
/// The folder under `sessions/` that holds the hub's one conversation.
pub const PRIMARY_DIR: &str = "primary";

fn session_dir(sessions_dir: &Path, cwd: &Path) -> PathBuf {
    let slug = cwd.display().to_string().trim_start_matches('/').replace(['/', '\\', ':'], "-");
    sessions_dir.join(format!("--{slug}--"))
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_reloads_and_applies_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionStore::create(dir.path(), Path::new("/work/proj"));
        store.append_model_change("anthropic", "claude-sonnet-5-5").unwrap();
        store.append_message(Message::user_text("one")).unwrap();
        let kept = store.append_message(Message::user_text("two")).unwrap();
        store.append_compaction("SUMMARY".into(), kept, 100, None).unwrap();
        store.append_message(Message::user_text("three")).unwrap();

        let reopened = SessionStore::open(&store.path).unwrap();
        let ctx = reopened.context();
        let texts: Vec<String> = ctx
            .messages
            .iter()
            .map(|m| match m {
                Message::User(u) => text_of(&u.content),
                _ => String::new(),
            })
            .collect();
        assert_eq!(texts.len(), 3);
        assert!(texts[0].contains("SUMMARY"));
        assert_eq!(&texts[1..], ["two", "three"]);
        assert_eq!(ctx.model.as_deref(), Some("anthropic/claude-sonnet-5-5"));
        assert_eq!(SessionStore::most_recent(dir.path(), Path::new("/work/proj")), Some(store.path.clone()));
    }

    fn texts(store: &SessionStore) -> Vec<String> {
        store.context().messages.iter().map(|m| text_of(&super::message_content(m))).collect()
    }

    #[test]
    fn context_edits_omit_or_replace_messages_and_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionStore::create(dir.path(), Path::new("/work/proj"));
        let first = store.append_message(Message::user_text("one")).unwrap();
        let second = store.append_message(Message::user_text("two")).unwrap();
        store.append_message(Message::user_text("three")).unwrap();
        store.append_context_edit(&first, None).unwrap();
        let replacement = Replacement { content: vec![ContentBlock::text("TWO")] };
        store.append_context_edit(&second, Some(replacement)).unwrap();
        assert_eq!(texts(&store), ["TWO", "three"]);

        let reopened = SessionStore::open(&store.path).unwrap();
        assert_eq!(texts(&reopened), ["TWO", "three"]);
        assert_eq!(reopened.entries().len(), 5, "the edited messages stay in the file");
        assert!(store.append_context_edit("missing", None).is_err());
    }

    #[test]
    fn reads_pi_context_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"session","version":3,"id":"s","timestamp":"t","cwd":"/"}"#, "\n",
                r#"{"type":"message","id":"a","parentId":null,"timestamp":"t","message":{"role":"user","content":"hi","timestamp":1}}"#, "\n",
                r#"{"type":"message","id":"b","parentId":"a","timestamp":"t","message":{"role":"user","content":"secret","timestamp":2}}"#, "\n",
                r#"{"type":"context_edit","id":"c","parentId":"b","timestamp":"t","targetId":"a","replacement":{"content":"hello"}}"#, "\n",
                r#"{"type":"context_edit","id":"d","parentId":"c","timestamp":"t","targetId":"b","replacement":null}"#, "\n",
            ),
        )
        .unwrap();
        let store = SessionStore::open(&path).unwrap();
        assert_eq!(texts(&store), ["hello"]);
    }

    /// Pi entries by hand: id, parent, type, and the rest of the fields.
    fn pi_session(dir: &Path, lines: &[serde_json::Value]) -> SessionStore {
        let path = dir.join("pi.jsonl");
        let mut text = r#"{"type":"session","version":3,"id":"s","timestamp":"t","cwd":"/work"}"#.to_string() + "\n";
        for line in lines {
            text.push_str(&format!("{line}\n"));
        }
        std::fs::write(&path, text).unwrap();
        SessionStore::open(&path).unwrap()
    }

    fn user(id: &str, parent: Option<&str>, at: &str, text: &str) -> serde_json::Value {
        serde_json::json!({ "type": "message", "id": id, "parentId": parent, "timestamp": at,
            "message": { "role": "user", "content": text, "timestamp": 1 } })
    }

    #[test]
    fn the_tree_has_pi_shape_with_branches_oldest_first_and_labels() {
        let dir = tempfile::tempdir().unwrap();
        let store = pi_session(
            dir.path(),
            &[
                user("a", None, "2026-01-01T00:00:00Z", "root"),
                user("c", Some("a"), "2026-01-01T00:00:02Z", "newer branch"),
                user("b", Some("a"), "2026-01-01T00:00:01Z", "older branch"),
                serde_json::json!({ "type": "label", "id": "l", "parentId": "c", "timestamp": "2026-01-01T00:00:03Z",
                "targetId": "b", "label": "keep" }),
            ],
        );
        let tree = store.tree_json();
        assert_eq!(tree.len(), 1);
        let children = tree[0]["children"].as_array().unwrap();
        let ids: Vec<&str> = children.iter().map(|n| n["entry"]["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["b", "c"], "children oldest first");
        assert_eq!(children[0]["label"], "keep");
        assert_eq!(children[1]["children"][0]["entry"]["type"], "label", "unknown entries keep their fields");
    }

    #[test]
    fn a_tree_as_deep_as_a_long_session_is_built_without_recursion() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionStore::create(dir.path(), Path::new("/work"));
        store.path = PathBuf::new();
        for i in 0..50_000 {
            store.append_message(Message::user_text(i.to_string())).unwrap();
        }
        // Built on a small stack, so a recursive build would overflow; written and freed on a
        // large one, as the RPC writer does.
        let tree = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(move || store.tree_json())
            .unwrap()
            .join()
            .expect("built without overflowing a 256 KB stack");
        let size = std::thread::Builder::new()
            .stack_size(512 << 20)
            .spawn(move || serde_json::Value::Array(tree).to_string().len())
            .unwrap()
            .join()
            .unwrap();
        assert!(size > 50_000 * 50);
    }

    #[test]
    fn a_branched_session_keeps_the_path_and_its_labels() {
        let dir = tempfile::tempdir().unwrap();
        let store = pi_session(
            dir.path(),
            &[
                user("a", None, "t1", "one"),
                serde_json::json!({ "type": "label", "id": "l", "parentId": "a", "timestamp": "t2", "targetId": "a", "label": "start" }),
                user("b", Some("l"), "t3", "two"),
                serde_json::json!({ "type": "compaction", "id": "k", "parentId": "b", "timestamp": "t4",
                "summary": "S", "firstKeptEntryId": "l", "tokensBefore": 10 }),
                user("x", Some("a"), "t5", "elsewhere"),
            ],
        );
        let branched = store.branched("k").unwrap();
        assert_eq!(branched.header.parent_session.as_deref(), Some(store.path.to_str().unwrap()));
        let entries = branched.entries();
        let ids: Vec<&str> = entries.iter().map(|e| e.id()).collect();
        assert_eq!(&ids[..3], ["a", "b", "k"], "the other branch and the label are not in the path");
        assert_eq!(entries[1].meta().parent_id.as_deref(), Some("a"), "re-chained past the label");
        assert!(matches!(&entries[2], Entry::Compaction { first_kept_entry_id, .. } if first_kept_entry_id == "b"));
        assert_eq!(entries[3].to_json()["targetId"], "a", "the label is re-created at the end");
        let reopened = SessionStore::open(&branched.path).unwrap();
        assert_eq!(reopened.entries().len(), 4);
    }

    #[test]
    fn session_names_come_from_the_newest_info_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionStore::create(dir.path(), Path::new("/work"));
        assert_eq!(store.session_name(), None);
        store.append_session_info("Trip\nplanning ").unwrap();
        assert_eq!(store.session_name().as_deref(), Some("Trip planning"));
        store.append_session_info("").unwrap();
        assert_eq!(SessionStore::open(&store.path).unwrap().session_name(), None, "an empty name clears it");
    }

    #[test]
    fn pi_messages_beyond_provider_roles_reach_context_as_pi_converts_them() {
        let dir = tempfile::tempdir().unwrap();
        let bash = |id: &str, parent: &str, command: &str, exclude: bool| {
            serde_json::json!({ "type": "message", "id": id, "parentId": parent, "timestamp": "t",
                "message": { "role": "bashExecution", "command": command, "output": "hi", "exitCode": 2,
                    "cancelled": false, "truncated": false, "timestamp": 5, "excludeFromContext": exclude } })
        };
        let store = pi_session(
            dir.path(),
            &[
                user("a", None, "t", "start"),
                bash("b", "a", "echo hi", false),
                bash("c", "b", "cat secrets", true),
                serde_json::json!({ "type": "custom_message", "id": "d", "parentId": "c", "timestamp": "t",
                "customType": "note", "content": "from an extension", "display": true }),
                serde_json::json!({ "type": "branch_summary", "id": "e", "parentId": "d", "timestamp": "t",
                "fromId": "x", "summary": "tried another way" }),
                user("f", Some("e"), "t", "end"),
            ],
        );
        let texts: Vec<String> = store.context().messages.iter().map(|m| text_of(&super::message_content(m))).collect();
        assert_eq!(texts.len(), 5, "{texts:?}");
        assert_eq!(texts[1], "Ran `echo hi`\n```\nhi\n```\n\nCommand exited with code 2");
        assert!(!texts.iter().any(|t| t.contains("secrets")), "!! output stays out of context");
        assert_eq!(texts[2], "from an extension");
        assert!(
            texts[3].starts_with("The following is a summary of a branch") && texts[3].contains("tried another way")
        );
        assert_eq!(texts[4], "end", "the entries after them stay on the branch");
    }

    #[test]
    fn keeps_unknown_pi_entries_in_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"session","version":3,"id":"s","timestamp":"t","cwd":"/"}"#, "\n",
                r#"{"type":"message","id":"a","parentId":null,"timestamp":"t","message":{"role":"user","content":"hi","timestamp":1}}"#, "\n",
                r#"{"type":"label","id":"b","parentId":"a","timestamp":"t","label":"x"}"#, "\n",
                r#"{"type":"message","id":"c","parentId":"b","timestamp":"t","message":{"role":"user","content":"there","timestamp":2}}"#, "\n",
            ),
        )
        .unwrap();
        let store = SessionStore::open(&path).unwrap();
        assert_eq!(store.context().messages.len(), 2);
    }
}
