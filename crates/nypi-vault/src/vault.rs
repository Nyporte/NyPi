//! Lucia's own vault of Markdown notes (decision D77): a folder in NyPi's home, apart from any
//! notes the owner keeps for themselves, with a fixed layout (an inbox, task workspaces,
//! playbooks, entity pages, an archive, attachments). NyPi's home is closed to every tool's
//! subprocess, so nothing a shell command does can damage it; the agent reads and writes it only
//! through the vault's own tools, in process, and every path they are given is checked here:
//! relative to the vault, one component at a time, with no `..`, no symlink on the way, and no
//! reach into the index's own files.

use std::path::{Component, Path, PathBuf};

/// The vault's folders, made when it is opened.
pub const INBOX: &str = "00-inbox";
pub const WORKSPACES: &str = "10-workspaces";
/// A goal's campaign: `10-projects/<slug>/plan.md` and the notes it links (decision D85).
pub const PROJECTS: &str = "10-projects";
pub const PLAYBOOKS: &str = "20-playbooks";
pub const ENTITIES: &str = "30-entities";
pub const ARCHIVE: &str = "90-archive";
pub const ATTACHMENTS: &str = "attachments";
pub const LAYOUT: [&str; 7] = [INBOX, WORKSPACES, PROJECTS, PLAYBOOKS, ENTITIES, ARCHIVE, ATTACHMENTS];

/// The largest note a tool writes or reads whole.
pub const MAX_NOTE_BYTES: usize = 256 * 1024;
/// The largest file copied in as an attachment.
pub const MAX_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

/// Why the vault would not do something. Each is a sentence the model can read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VaultError {
    /// The path leaves the vault, names the index, or goes through a symlink.
    Refused(String),
    NotFound(String),
    Io(String),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::Refused(why) | VaultError::NotFound(why) | VaultError::Io(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for VaultError {}

fn io(action: &str, path: &Path, error: std::io::Error) -> VaultError {
    VaultError::Io(format!("cannot {action} {}: {error}", path.display()))
}

/// How a write treats what is already in the note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    Replace,
    Append,
}

/// One entry of a folder listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Relative to the vault, `/`-separated.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct Vault {
    /// The vault's folder, with symlinks resolved, so a path below it can be checked by prefix.
    root: PathBuf,
}

impl Vault {
    /// Open the vault at `root`, making the folder and its layout when they are missing.
    pub fn open(root: &Path) -> Result<Vault, VaultError> {
        for folder in LAYOUT {
            let path = root.join(folder);
            std::fs::create_dir_all(&path).map_err(|error| io("create", &path, error))?;
        }
        let root = std::fs::canonicalize(root).map_err(|error| io("open", root, error))?;
        Ok(Vault { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `path` as the vault names it: relative, `/`-separated.
    pub fn relative(&self, path: &Path) -> String {
        let relative = path.strip_prefix(&self.root).unwrap_or(path);
        relative.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
    }

    /// The file or folder a vault path names, once it is known to stay inside the vault.
    /// `relative` is taken from the vault's root whether or not it begins with a slash; a path
    /// that spells out the vault's own folder is accepted too. Nothing on the way may be a
    /// symlink, since a link is how a path that looks inside would end up outside.
    pub fn resolve(&self, relative: &str) -> Result<PathBuf, VaultError> {
        let text = relative.trim();
        let given = Path::new(text);
        let inside = given.strip_prefix(&self.root).unwrap_or(given);
        let mut path = self.root.clone();
        for component in inside.components() {
            match component {
                Component::RootDir | Component::CurDir => continue,
                Component::ParentDir => {
                    return Err(VaultError::Refused(format!("{text} leaves the vault (`..` is not followed)")));
                }
                Component::Prefix(_) => {
                    return Err(VaultError::Refused(format!("{text} is not a path inside the vault")));
                }
                Component::Normal(name) => {
                    let name_text = name.to_string_lossy();
                    if name_text.starts_with(".index") {
                        return Err(VaultError::Refused(format!(
                            "{name_text} is the vault's own index, which only the harness writes"
                        )));
                    }
                    path.push(name);
                    if let Ok(meta) = std::fs::symlink_metadata(&path)
                        && meta.file_type().is_symlink()
                    {
                        return Err(VaultError::Refused(format!(
                            "{} is a symlink; the vault follows none",
                            self.relative(&path)
                        )));
                    }
                }
            }
        }
        Ok(path)
    }

    /// A note's text.
    pub fn read(&self, relative: &str) -> Result<String, VaultError> {
        let path = self.resolve(relative)?;
        let meta = std::fs::metadata(&path)
            .map_err(|_| VaultError::NotFound(format!("no note at {}", self.relative(&path))))?;
        if meta.is_dir() {
            return Err(VaultError::Refused(format!("{} is a folder; list it instead", self.relative(&path))));
        }
        if meta.len() > MAX_NOTE_BYTES as u64 {
            return Err(VaultError::Refused(format!(
                "{} is {} bytes, too large to read whole (an attachment, not a note)",
                self.relative(&path),
                meta.len()
            )));
        }
        let bytes = std::fs::read(&path).map_err(|error| io("read", &path, error))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Write a note, making its folders. Returns the path written, relative to the vault.
    pub fn write(&self, relative: &str, content: &str, mode: WriteMode) -> Result<String, VaultError> {
        let path = self.resolve(relative)?;
        if path == self.root {
            return Err(VaultError::Refused("a note needs a name".into()));
        }
        if path.is_dir() {
            return Err(VaultError::Refused(format!("{} is a folder", self.relative(&path))));
        }
        let text = match mode {
            WriteMode::Replace => content.to_string(),
            WriteMode::Append => {
                let mut existing = match std::fs::read(&path) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(_) => String::new(),
                };
                if !existing.is_empty() && !existing.ends_with('\n') {
                    existing.push('\n');
                }
                existing.push_str(content);
                existing
            }
        };
        if text.len() > MAX_NOTE_BYTES {
            return Err(VaultError::Refused(format!(
                "the note would be {} bytes; split it into notes under {} bytes each",
                text.len(),
                MAX_NOTE_BYTES
            )));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| io("create", parent, error))?;
        }
        // Through a draft, so a crash never leaves half a note.
        let draft = path.with_extension(format!(
            "{}.tmp",
            path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default()
        ));
        std::fs::write(&draft, text).map_err(|error| io("write", &draft, error))?;
        std::fs::rename(&draft, &path).map_err(|error| io("write", &path, error))?;
        Ok(self.relative(&path))
    }

    /// What is in a folder of the vault (the root without a path), names in order.
    pub fn list(&self, relative: &str) -> Result<Vec<Entry>, VaultError> {
        let dir = self.resolve(relative)?;
        let entries = std::fs::read_dir(&dir)
            .map_err(|_| VaultError::NotFound(format!("no folder at {}", self.relative(&dir))))?;
        let mut listed: Vec<Entry> = entries
            .filter_map(Result::ok)
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .filter_map(|entry| {
                // A symlink someone put here by hand is not followed, and not shown.
                let meta = std::fs::symlink_metadata(entry.path()).ok()?;
                (!meta.file_type().is_symlink()).then(|| Entry {
                    path: self.relative(&entry.path()),
                    is_dir: meta.is_dir(),
                    size: meta.len(),
                })
            })
            .collect();
        listed.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(listed)
    }

    /// Copy `bytes` into the vault's attachments under `name` (its folders removed, so a name
    /// cannot place it elsewhere). An existing attachment of that name is kept and the new one
    /// numbered. Returns the path, relative to the vault.
    pub fn attach(&self, name: &str, bytes: &[u8]) -> Result<String, VaultError> {
        if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
            return Err(VaultError::Refused(format!("{} bytes is too large to attach", bytes.len())));
        }
        let name = Path::new(name).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let name = name.trim_start_matches('.').to_string();
        if name.is_empty() {
            return Err(VaultError::Refused("an attachment needs a file name".into()));
        }
        let (stem, extension) = match name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => (stem.to_string(), format!(".{extension}")),
            _ => (name.clone(), String::new()),
        };
        let mut candidate = format!("{ATTACHMENTS}/{name}");
        let mut number = 1;
        while self.resolve(&candidate)?.exists() {
            number += 1;
            candidate = format!("{ATTACHMENTS}/{stem}-{number}{extension}");
        }
        let path = self.resolve(&candidate)?;
        std::fs::write(&path, bytes).map_err(|error| io("write", &path, error))?;
        Ok(candidate)
    }

    /// Move a workspace (a folder under `10-workspaces`) into the archive, under the month
    /// `month` names (`YYYY-MM`). Returns where it went, relative to the vault.
    pub fn archive(&self, workspace: &str, month: &str) -> Result<String, VaultError> {
        let name = workspace.trim().trim_matches('/');
        let name = name.strip_prefix(&format!("{WORKSPACES}/")).unwrap_or(name);
        if name.is_empty() || name.contains('/') {
            return Err(VaultError::Refused(format!("{workspace} is not a workspace under {WORKSPACES}/")));
        }
        if month.len() != 7 || !month.chars().all(|c| c.is_ascii_digit() || c == '-') {
            return Err(VaultError::Refused(format!("{month} is not a month (YYYY-MM)")));
        }
        let from = self.resolve(&format!("{WORKSPACES}/{name}"))?;
        if !from.is_dir() {
            return Err(VaultError::NotFound(format!("no workspace {name} under {WORKSPACES}/")));
        }
        let target = format!("{ARCHIVE}/{month}/{name}");
        let to = self.resolve(&target)?;
        if to.exists() {
            return Err(VaultError::Refused(format!("{target} is already in the archive")));
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|error| io("create", parent, error))?;
        }
        std::fs::rename(&from, &to).map_err(|error| io("archive", &from, error))?;
        Ok(target)
    }

    /// Every Markdown note in the vault, relative paths in order. Dot files, the index, and
    /// symlinks are passed over.
    pub fn notes(&self) -> Vec<String> {
        let mut found = Vec::new();
        let mut pending = vec![self.root.clone()];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.filter_map(Result::ok) {
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let path = entry.path();
                let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
                if meta.file_type().is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")) {
                    found.push(self.relative(&path));
                }
            }
        }
        found.sort();
        found
    }
}

/// A folder name from free text: lowercase letters, digits, and single hyphens, at most 48
/// characters; other scripts are kept as they are (a Chinese title stays readable).
pub fn slug(text: &str) -> String {
    let mut slug = String::new();
    for c in text.trim().chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.chars().count() >= 48 {
            break;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() { "note".to_string() } else { slug }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(&dir.path().join("vault")).unwrap();
        (dir, vault)
    }

    #[test]
    fn the_layout_is_made_and_notes_are_written_read_and_listed() {
        let (_dir, vault) = vault();
        for folder in LAYOUT {
            assert!(vault.root().join(folder).is_dir(), "{folder}");
        }
        let written = vault.write("30-entities/atlas.md", "# Atlas\n", WriteMode::Replace).unwrap();
        assert_eq!(written, "30-entities/atlas.md");
        vault.write("/30-entities/atlas.md", "More.", WriteMode::Append).unwrap();
        assert_eq!(vault.read("30-entities/atlas.md").unwrap(), "# Atlas\nMore.");
        // The vault's own folder spelled out is the same note.
        let spelled = vault.root().join("30-entities/atlas.md").display().to_string();
        assert_eq!(vault.read(&spelled).unwrap(), "# Atlas\nMore.");
        let listed = vault.list("30-entities").unwrap();
        assert_eq!(listed.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["30-entities/atlas.md"]);
        assert_eq!(vault.notes(), ["30-entities/atlas.md"]);
        assert!(matches!(vault.read("30-entities/none.md"), Err(VaultError::NotFound(_))));
    }

    #[test]
    fn no_path_leaves_the_vault_or_touches_its_index() {
        let (dir, vault) = vault();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        for path in ["../outside/secret.txt", "00-inbox/../../outside/secret.txt", ".index.jsonl", "a/.index.tmp"] {
            assert!(matches!(vault.read(path), Err(VaultError::Refused(_))), "{path}");
            assert!(matches!(vault.write(path, "x", WriteMode::Replace), Err(VaultError::Refused(_))), "{path}");
        }
        // An absolute path elsewhere is read as a path inside the vault, where it does not exist.
        let elsewhere = outside.join("secret.txt").display().to_string();
        assert!(vault.read(&elsewhere).is_err());
        assert!(!vault.read(&elsewhere).is_ok_and(|text| text.contains("secret")));

        // A symlink inside the vault, to a file or a folder outside, is never followed.
        std::os::unix::fs::symlink(outside.join("secret.txt"), vault.root().join("00-inbox/door.md")).unwrap();
        std::os::unix::fs::symlink(&outside, vault.root().join("00-inbox/way-out")).unwrap();
        assert!(matches!(vault.read("00-inbox/door.md"), Err(VaultError::Refused(_))));
        assert!(matches!(
            vault.write("00-inbox/way-out/planted.md", "x", WriteMode::Replace),
            Err(VaultError::Refused(_))
        ));
        assert!(!outside.join("planted.md").exists());
        assert!(vault.list("00-inbox").unwrap().is_empty(), "links are not listed");
        assert!(vault.notes().is_empty(), "nor indexed");
    }

    #[test]
    fn attachments_keep_their_names_apart_and_workspaces_move_to_the_archive() {
        let (_dir, vault) = vault();
        assert_eq!(vault.attach("../../shot.png", b"one").unwrap(), "attachments/shot.png");
        assert_eq!(vault.attach("shot.png", b"two").unwrap(), "attachments/shot-2.png");
        assert_eq!(std::fs::read(vault.root().join("attachments/shot.png")).unwrap(), b"one");

        vault.write("10-workspaces/261119-atlas/findings.md", "# Findings\n", WriteMode::Replace).unwrap();
        let archived = vault.archive("10-workspaces/261119-atlas", "2026-11").unwrap();
        assert_eq!(archived, "90-archive/2026-11/261119-atlas");
        assert!(vault.read("90-archive/2026-11/261119-atlas/findings.md").is_ok());
        assert!(matches!(vault.archive("261119-atlas", "2026-11"), Err(VaultError::NotFound(_))));
        assert!(matches!(vault.archive("../30-entities", "2026-11"), Err(VaultError::Refused(_))));
        assert_eq!(slug("Fix the Atlas deploy (v2)!"), "fix-the-atlas-deploy-v2");
        assert_eq!(slug("   "), "note");
    }
}
