//! Coding tools: read, write, edit, bash, grep, find, ls (ported from Pi's coding-agent tools).

pub(crate) mod bash;
pub mod command;
mod edit;
mod find;
mod goals;
mod grep;
pub mod http_request;
mod ls;
pub mod memory;
mod read;
pub mod schedule;
mod self_prompt;
pub mod truncate;
mod write;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use nypi_agent::Tool;

pub use bash::BashTool;
pub use edit::EditTool;
pub use find::FindTool;
pub use goals::GoalsTool;
pub use grep::GrepTool;
pub use ls::LsTool;
pub use read::ReadTool;
pub use self_prompt::SelfPromptTool;
pub use write::WriteTool;

/// Names of the tools enabled by default: Pi's default set plus the self-prompt layer and goals.
pub const DEFAULT_TOOLS: [&str; 11] =
    ["read", "bash", "edit", "write", "self_prompt", "goals", "memory", "schedule", "vault", "assets", "http_request"];
/// `memory` needs the runtime (store, task, embedder), so the runtime builds it, not `create_tools`.
/// `vault` stands for the six `vault_*` tools and `assets` for `asset_manage` (nypi-vault); the
/// runtime builds those and `http_request` too, since they need its stores.
pub const ALL_TOOLS: [&str; 14] = [
    "read",
    "bash",
    "edit",
    "write",
    "grep",
    "find",
    "ls",
    "self_prompt",
    "goals",
    "memory",
    "schedule",
    "vault",
    "assets",
    "http_request",
];

/// Build the named tools, rooted at `cwd`; `self_prompt_path` backs the `self_prompt` tool.
/// Unknown names are ignored.
pub fn create_tools(
    names: &[String],
    cwd: &Path,
    self_prompt_path: &Path,
    goals_path: &Path,
    sandbox: &crate::sandbox::Sandbox,
    protected: &Arc<crate::protected::Protected>,
) -> Vec<Arc<dyn Tool>> {
    names
        .iter()
        .filter_map(|name| -> Option<Arc<dyn Tool>> {
            let cwd = cwd.to_path_buf();
            Some(match name.as_str() {
                "read" => Arc::new(ReadTool { cwd, protected: protected.clone() }),
                "write" => Arc::new(WriteTool { cwd, protected: protected.clone() }),
                "edit" => Arc::new(EditTool { cwd, protected: protected.clone() }),
                "bash" => Arc::new(BashTool { cwd, sandbox: sandbox.clone() }),
                "grep" => Arc::new(GrepTool { cwd, protected: protected.clone() }),
                "find" => Arc::new(FindTool { cwd }),
                "ls" => Arc::new(LsTool { cwd }),
                "self_prompt" => Arc::new(SelfPromptTool { path: self_prompt_path.to_path_buf() }),
                // The goals file is in NyPi's home, and so is the vault its projects are in.
                "goals" => Arc::new(GoalsTool {
                    path: goals_path.to_path_buf(),
                    vault: goals_path.parent().map(|home| crate::config::Paths { home: home.to_path_buf() }.vault()),
                }),
                _ => return None,
            })
        })
        .collect()
}

/// The near-miss fix-ups of the device tools (`place::DEVICE_TOOLS`), for a call the hub sends
/// to a node on another device: they run before the hub checks the arguments against the
/// declared schema, where the node's own tool cannot be asked without a round trip.
pub fn prepare_device_arguments(tool: &str, arguments: serde_json::Value) -> serde_json::Value {
    match tool {
        "edit" => edit::prepare(arguments),
        _ => arguments,
    }
}

/// One-line description and prompt rules each tool contributes to the system prompt.
pub fn prompt_contribution(name: &str) -> Option<(&'static str, &'static [&'static str])> {
    Some(match name {
        "read" => ("Read file contents", &["Use read to examine files instead of cat or sed."]),
        "bash" => ("Execute bash commands (ls, grep, find, etc.)", &[]),
        "edit" => (
            "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
            &[
                "Use edit for targeted changes. Batch changes to one file in a single call.",
                "Each edits[].oldText must exactly and uniquely match the original file. Keep matches short and non-overlapping; merge nearby changes without including large unchanged regions.",
            ],
        ),
        "write" => ("Create or overwrite files", &["Use write only for new files or complete rewrites."]),
        "grep" => ("Search file contents for patterns (respects .gitignore)", &[]),
        "find" => ("Find files by glob pattern (respects .gitignore)", &[]),
        "ls" => ("List directory contents", &[]),
        "self_prompt" => (
            "View and evolve your own read-write self-prompt (versioned)",
            &[
                "When you learn something durable about the user or how to work better with them, record it with self_prompt; keep it concise and remove what is stale.",
            ],
        ),
        "memory" => (
            "Long-term memory across sessions: recall, remember, update, forget",
            &[
                "Before answering anything about the user, the people in their life, past decisions, or earlier work, recall from memory; when the user shares a durable fact, preference, decision, or commitment, remember it with helpful tags. Never store secrets. A message may carry <recalled-memories> that the harness attached: background to use where it helps, not to recite; if one is wrong or outdated, update or forget it.",
            ],
        ),
        "schedule" => (
            "Timers and condition triggers that notify the user or run a prompt later",
            &[
                "When the user asks to be reminded or for a follow-up, or something slow should be checked later, set it with schedule instead of waiting. To watch for text in a file or output, use a command condition that tests for it, not fileChanged with a prompt: a prompt action costs a model run on every firing.",
            ],
        ),
        "vault" => (
            "Your own Markdown vault: vault_search, vault_read, vault_write, vault_list, vault_attach, vault_archive",
            &[
                "Your vault is where what you know lasts: entity pages for projects, systems, and people (30-entities), playbooks for how something is done (20-playbooks), a workspace per substantial task (10-workspaces), and an inbox (00-inbox). Search it before working on something you may have notes on; write down what you verified and what turned out false while it is fresh; link related notes with [[name]]. Single facts and preferences belong in memory; anything with structure belongs in the vault.",
            ],
        ),
        "assets" => (
            "asset_manage: the registry of accounts, servers, domains, subscriptions, and deliverables you look after",
            &[
                "Keep what you are trusted to look after in the asset registry, with when it renews. Secrets never go into it, a note, a memory, or a command: refer to one by its cred://<scope>/<name> handle, and ask the user to store its value with `nypi secrets set`.",
            ],
        ),
        "http_request" => ("One HTTP request, with a credential used on your behalf by handle (auth), never seen", &[]),
        "goals" => (
            "Read and update the user's goals (versioned)",
            &[
                "Relate your work to the user's goals. After you advance one, record the new next step with goals; mark it done when it is done.",
            ],
        ),
        _ => return None,
    })
}

/// Resolve a model-supplied path against `cwd`, expanding `~`.
pub(crate) fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    resolve_path_in(cwd, path, &dirs::home_dir().unwrap_or_default())
}

/// `resolve_path` with `~` as `home`: the user's home on the device the path is on.
pub(crate) fn resolve_path_in(cwd: &Path, path: &str, home: &Path) -> PathBuf {
    // As Pi's `resolveToCwd`: copied text and models put Unicode spaces where a path has plain
    // ones, and a leading `@` is a mention, not part of the name.
    let path: String = path.trim().chars().map(|c| if is_unicode_space(c) { ' ' } else { c }).collect();
    let path = path.strip_prefix('@').unwrap_or(&path);
    let expanded = if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    };
    if expanded.is_absolute() { expanded } else { cwd.join(expanded) }
}

/// The space variants Pi's path handling reads as a plain space.
fn is_unicode_space(c: char) -> bool {
    matches!(c, '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
}

/// `resolve_path` for a file to read; when nothing is there, the forms macOS stores names in,
/// as Pi's `resolveReadPath` tries them: a narrow no-break space before AM or PM (screenshot
/// names), decomposed accents (NFD), a curly apostrophe ("Capture d’écran"), and the last two
/// together.
pub(crate) fn resolve_read_path(cwd: &Path, path: &str) -> PathBuf {
    use unicode_normalization::UnicodeNormalization;
    static AM_PM: OnceLock<regex::Regex> = OnceLock::new();
    let resolved = resolve_path(cwd, path);
    if resolved.exists() {
        return resolved;
    }
    let text = resolved.to_string_lossy().into_owned();
    let am_pm = AM_PM.get_or_init(|| regex::Regex::new(r"(?i) (AM|PM)\.").expect("pattern compiles"));
    let nfd: String = text.nfd().collect();
    let candidates = [
        am_pm.replace_all(&text, "\u{202F}$1.").into_owned(),
        nfd.clone(),
        text.replace('\'', "\u{2019}"),
        nfd.replace('\'', "\u{2019}"),
    ];
    candidates.into_iter().find(|c| *c != text && Path::new(c).exists()).map(PathBuf::from).unwrap_or(resolved)
}

/// The file a read reaches through `path`, symlinks followed, once `protected` agrees it may be
/// read. The guard checked the call before it ran; this is the same check where the file is
/// opened, on the path that is opened, so nothing (a symlink made by a parallel `bash` call, a
/// name form found only now) can come between the check and the read.
pub(crate) fn readable(protected: &crate::protected::Protected, path: &Path) -> anyhow::Result<PathBuf> {
    let real = crate::protected::real_path(path);
    match protected.read_denied(&real) {
        Some(why) => anyhow::bail!("Refused: {why}"),
        None => Ok(real),
    }
}

/// `readable` for a write: the file a write would create or change, once `protected` agrees.
pub(crate) fn writable(protected: &crate::protected::Protected, path: &Path) -> anyhow::Result<PathBuf> {
    let real = crate::protected::real_path(path);
    match protected.write_denied(&real) {
        Some(why) => anyhow::bail!("Refused: {why}"),
        None => Ok(real),
    }
}

/// Per-file lock so parallel edit/write calls to the same file cannot interleave.
pub(crate) fn file_lock(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    LOCKS.get_or_init(Default::default).lock().unwrap().entry(key).or_default().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_lose_mentions_and_unicode_spaces() {
        let cwd = Path::new("/work");
        assert_eq!(resolve_path(cwd, "@src/main.rs"), Path::new("/work/src/main.rs"));
        assert_eq!(resolve_path(cwd, "my\u{00A0}notes.txt"), Path::new("/work/my notes.txt"));
        assert_eq!(resolve_path(cwd, "/abs/file"), Path::new("/abs/file"));
    }

    #[test]
    fn reads_find_the_names_macos_stores() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let screenshot = "Screenshot 2026-11-04 at 10.00.00\u{202F}AM.png";
        let french = "Capture d\u{2019}écran.png";
        for name in [screenshot, french] {
            std::fs::write(cwd.join(name), "x").unwrap();
        }
        // Typed with a plain space and a straight apostrophe, as people type them.
        let found = resolve_read_path(cwd, "Screenshot 2026-11-04 at 10.00.00 AM.png");
        assert_eq!(found.file_name().unwrap().to_str(), Some(screenshot));
        assert!(resolve_read_path(cwd, "Capture d'écran.png").exists());
        assert_eq!(resolve_read_path(cwd, "missing.txt"), cwd.join("missing.txt"), "the plain path when none exists");
    }
}
