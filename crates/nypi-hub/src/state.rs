//! The hub's state (decision D95): everything in the NyPi home except what belongs to one
//! machine, as it moves to a standby copy on a personal node and, in a move, becomes the new
//! hub's home.
//!
//! Carried: the settings, models, keys of the model providers, the context and goals files,
//! sessions, the planes' logs, the stores, the vault, the audit log, reflections, the register of
//! machines and browsers, the hub key, the secret store with its master key, and every env file
//! the settings name (one outside the NyPi home goes as `env/<name>`). Not carried: `run/`,
//! `logs/`, `bin/`, `models/`, `node/`, `standby/`, `diverged/`, `relay/`, the machine's own key,
//! and `instance.json`.
//!
//! A copy is brought up to date by differences: an append-only `.jsonl` file by the bytes after
//! the length the copy has, any other file whole when its length or SHA-256 differs, and files
//! gone are deleted. The copy applies a batch only when every file checks out.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use nypi_core::config::{Paths, Settings, expand_home};
use serde::{Deserialize, Serialize};

/// Top-level names of the NyPi home that belong to one machine.
const NOT_CARRIED: &[&str] = &[
    "run",
    "logs",
    "bin",
    "models",
    "node",
    "standby",
    "diverged",
    "relay",
    "device-id.key",
    "interface-id.key",
    "instance.json",
    "secrets.key",
];
/// The master key, carried as an entry of its own (it may live in the keychain, not a file).
pub const MASTER_KEY: &str = "@master-key";
/// Where each env file from outside the hub's home went, carried as an entry of its own.
pub const ENV_MAP: &str = "@env-map";
/// Files kept in place when a hub steps down: the machine needs them to run as a node.
const KEPT_ON_STEP_DOWN: &[&str] = &["settings.json", "models.json", "auth.json"];

/// What a copy has of one file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub size: u64,
    pub sha: String,
}

/// What a standby copy holds, as `standby/manifest.json` keeps it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Manifest {
    pub epoch: u64,
    /// When the last batch was applied.
    pub at: String,
    pub files: BTreeMap<String, FileState>,
}

/// One file of the hub's state: where it is carried to, and where it is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Carried {
    pub path: String,
    pub source: PathBuf,
}

pub fn sha(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn walk(root: &Path, at: &Path, out: &mut Vec<Carried>) {
    let Ok(entries) = std::fs::read_dir(at) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        if at == root && NOT_CARRIED.contains(&name.as_str()) {
            continue;
        }
        // Locks and half-written files are the moment's, not the state's.
        if name.ends_with(".lock") || name.ends_with(".tmp") || name.ends_with(".sock") {
            continue;
        }
        if kind.is_dir() {
            walk(root, &path, out);
        } else if kind.is_file() {
            out.push(Carried { path: relative, source: path });
        }
    }
}

/// Env files the settings name outside the NyPi home, and the name each is carried under.
pub fn outside_env_files(paths: &Paths, settings: &Settings) -> Vec<(String, String)> {
    let home = nypi_core::protected::real_path(&paths.home);
    let mut taken: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for named in &settings.env_files {
        let file = expand_home(named);
        if nypi_core::protected::real_path(&file).starts_with(&home) || !file.is_file() {
            continue;
        }
        let base = file.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "env".into());
        let mut name = base.clone();
        let mut n = 2;
        while taken.contains(&name) {
            name = format!("{n}-{base}");
            n += 1;
        }
        taken.push(name.clone());
        out.push((named.clone(), format!("env/{name}")));
    }
    out
}

/// Every file of the hub's state now.
pub fn carried(paths: &Paths, settings: &Settings) -> Vec<Carried> {
    let mut out = Vec::new();
    walk(&paths.home, &paths.home, &mut out);
    for (named, path) in outside_env_files(paths, settings) {
        out.retain(|carried| carried.path != path);
        out.push(Carried { path, source: expand_home(&named) });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// What to send to bring a copy that has `have` up to the hub's state now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    /// Bytes to put after the copy's `offset` bytes.
    Append {
        offset: u64,
    },
    /// The whole file.
    Whole,
    Delete,
}

/// One entry of a batch, as its header line says it; the bytes follow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub path: String,
    #[serde(flatten)]
    pub change: Change,
    /// The file's size and SHA-256 once applied.
    pub size: u64,
    pub sha: String,
    /// How many bytes follow the header.
    pub bytes: u64,
}

/// One entry and its bytes.
pub struct Piece {
    pub entry: Entry,
    pub bytes: Vec<u8>,
}

/// The differences between the hub's state now and a copy that has `have`. The master key and
/// the map of env files come as entries of their own, only when they changed too.
pub fn difference(paths: &Paths, settings: &Settings, have: &Manifest) -> Vec<Piece> {
    difference_cached(paths, settings, have, &mut StateCache::default())
}

/// What the hub knew of a file when it last sent it (decision D99): its length and time, its
/// SHA-256, and, for a log, the digest so far, so that a tail is hashed on from where it was.
#[derive(Clone)]
pub struct CachedFile {
    pub len: u64,
    pub modified: std::time::SystemTime,
    pub sha: String,
    digest: Option<ring::digest::Context>,
}

/// What the hub keeps between a copy's batches, so a batch reads only what changed.
#[derive(Default)]
pub struct StateCache {
    files: std::collections::HashMap<PathBuf, CachedFile>,
    /// Files read whole, and tails read, since the cache was made: what a test counts.
    pub whole_reads: u64,
    pub tail_reads: u64,
}

fn hash(bytes: &[u8]) -> (String, ring::digest::Context) {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(bytes);
    (hex(context.clone().finish().as_ref()), context)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The bytes of `file` from `offset` on.
fn tail_of(file: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut handle = std::fs::File::open(file)?;
    handle.seek(SeekFrom::Start(offset))?;
    let mut tail = Vec::new();
    handle.read_to_end(&mut tail)?;
    Ok(tail)
}

/// `difference`, with what the hub knew of each file from its last batch (decision D99): a file
/// whose length and time are as they were, and which the copy has as the hub sent it, is not
/// read at all; a log that only grew since is read from where the copy ends, its SHA-256 hashed
/// on from the digest kept, never from the start again.
pub fn difference_cached(paths: &Paths, settings: &Settings, have: &Manifest, cache: &mut StateCache) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut present: std::collections::HashSet<String> = std::collections::HashSet::new();
    let put = |pieces: &mut Vec<Piece>, path: &str, change: Change, size: u64, sha: String, bytes: Vec<u8>| {
        let entry = Entry { path: path.to_string(), change, size, sha, bytes: bytes.len() as u64 };
        pieces.push(Piece { entry, bytes });
    };
    for file in carried(paths, settings) {
        let Ok(meta) = std::fs::metadata(&file.source) else { continue };
        present.insert(file.path.clone());
        let (len, modified) = (meta.len(), meta.modified().unwrap_or(std::time::UNIX_EPOCH));
        let held = have.files.get(&file.path);
        let known = cache.files.get(&file.source).cloned();
        let copy_has_known =
            |known: &CachedFile| held.is_some_and(|held| held.size == known.len && held.sha == known.sha);
        if let Some(known) = &known
            && known.len == len
            && known.modified == modified
            && copy_has_known(known)
        {
            continue;
        }
        if file.path.ends_with(".jsonl")
            && let Some(known) = known.as_ref().filter(|known| copy_has_known(known) && len > known.len)
            && let Some(mut digest) = known.digest.clone()
            && let Ok(tail) = tail_of(&file.source, known.len)
        {
            cache.tail_reads += 1;
            digest.update(&tail);
            let sha = hex(digest.clone().finish().as_ref());
            let size = known.len + tail.len() as u64;
            cache.files.insert(
                file.source.clone(),
                CachedFile { len: size, modified, sha: sha.clone(), digest: Some(digest) },
            );
            put(&mut pieces, &file.path, Change::Append { offset: known.len }, size, sha, tail);
            continue;
        }
        let Ok(bytes) = std::fs::read(&file.source) else { continue };
        cache.whole_reads += 1;
        let (sha, digest) = hash(&bytes);
        let size = bytes.len() as u64;
        let log = file.path.ends_with(".jsonl");
        cache.files.insert(
            file.source.clone(),
            CachedFile { len: size, modified, sha: sha.clone(), digest: log.then_some(digest) },
        );
        if held.is_some_and(|held| held.size == size && held.sha == sha) {
            continue;
        }
        let tail = held.filter(|held| log && held.size < size && self::sha(&bytes[..held.size as usize]) == held.sha);
        match tail {
            Some(held) => {
                let offset = held.size;
                put(&mut pieces, &file.path, Change::Append { offset }, size, sha, bytes[offset as usize..].to_vec())
            }
            None => put(&mut pieces, &file.path, Change::Whole, size, sha, bytes),
        }
    }
    let mut small: Vec<(String, Vec<u8>)> = Vec::new();
    if paths.secrets().exists()
        && let Ok(key) = nypi_vault::secrets::master_key(&paths.home)
    {
        small.push((MASTER_KEY.into(), nypi_vault::secrets::key_to_hex(&key).into_bytes()));
    }
    let env_map: Vec<serde_json::Value> = outside_env_files(paths, settings)
        .into_iter()
        .map(|(named, path)| serde_json::json!({ "original": named, "copy": path }))
        .collect();
    small.push((ENV_MAP.into(), serde_json::to_vec(&env_map).unwrap_or_default()));
    for (path, bytes) in small {
        present.insert(path.clone());
        let state = FileState { size: bytes.len() as u64, sha: sha(&bytes) };
        if have.files.get(&path) != Some(&state) {
            put(&mut pieces, &path, Change::Whole, state.size, state.sha, bytes);
        }
    }
    for path in have.files.keys().filter(|path| !present.contains(*path)) {
        put(&mut pieces, path, Change::Delete, 0, String::new(), Vec::new());
    }
    pieces
}

/// A file's new bytes, and whether they go after what it has.
type Ready = (Vec<u8>, bool);

/// The standby copy on this machine.
pub struct Standby {
    root: PathBuf,
}

/// A path a batch names is taken only when it stays inside the copy.
fn plain(path: &str) -> Option<&str> {
    let ok = !path.is_empty()
        && !path.starts_with('/')
        && path.split('/').all(|part| !part.is_empty() && part != "." && part != "..");
    ok.then_some(path)
}

impl Standby {
    pub fn at(paths: &Paths) -> Standby {
        Standby { root: paths.standby() }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn file(&self, path: &str) -> Option<PathBuf> {
        match path {
            MASTER_KEY => Some(self.root.join("master.key")),
            ENV_MAP => Some(self.root.join("env-map.json")),
            path => plain(path).map(|path| self.home().join(path)),
        }
    }

    pub fn manifest(&self) -> Manifest {
        std::fs::read_to_string(self.root.join("manifest.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Written whole and renamed, so the copy always has a manifest to read.
    fn save(&self, manifest: &Manifest) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let draft = self.root.join("manifest.json.draft");
        nypi_node::identity::write_private(&draft, &serde_json::to_vec_pretty(manifest)?)?;
        std::fs::rename(draft, self.root.join("manifest.json"))
    }

    pub fn exists(&self) -> bool {
        self.root.join("manifest.json").exists()
    }

    /// Apply a batch made against this copy's manifest, at the hub's `epoch`: all of it when
    /// every file checks out, none of it otherwise. The manifest after, or why it was refused.
    pub fn apply(&self, epoch: u64, pieces: Vec<Piece>) -> Result<Manifest, String> {
        let mut manifest = self.manifest();
        // Each file, and its new bytes with whether they are appended (none: it is deleted).
        let mut ready: Vec<(PathBuf, Option<Ready>)> = Vec::new();
        for Piece { entry, bytes } in pieces {
            let file = self.file(&entry.path).ok_or_else(|| format!("{} is not a path of the copy", entry.path))?;
            match entry.change {
                Change::Delete => {
                    manifest.files.remove(&entry.path);
                    ready.push((file, None));
                }
                Change::Whole => {
                    if bytes.len() as u64 != entry.size || sha(&bytes) != entry.sha {
                        return Err(format!("{} does not check out", entry.path));
                    }
                    manifest.files.insert(entry.path.clone(), FileState { size: entry.size, sha: entry.sha });
                    ready.push((file, Some((bytes, false))));
                }
                Change::Append { offset } => {
                    let held = std::fs::read(&file).unwrap_or_default();
                    if held.len() as u64 != offset {
                        return Err(format!("{} is not at the length the hub has for it", entry.path));
                    }
                    let mut whole = held;
                    whole.extend_from_slice(&bytes);
                    if whole.len() as u64 != entry.size || sha(&whole) != entry.sha {
                        return Err(format!("{} does not check out", entry.path));
                    }
                    manifest.files.insert(entry.path.clone(), FileState { size: entry.size, sha: entry.sha });
                    ready.push((file, Some((bytes, true))));
                }
            }
        }
        for (file, change) in ready {
            match change {
                None => {
                    let _ = std::fs::remove_file(&file);
                }
                Some((bytes, append)) => {
                    if let Some(dir) = file.parent() {
                        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                    }
                    if append {
                        let mut handle =
                            std::fs::OpenOptions::new().append(true).open(&file).map_err(|e| e.to_string())?;
                        handle.write_all(&bytes).map_err(|e| e.to_string())?;
                    } else {
                        let draft = file.with_extension("standby-tmp");
                        nypi_node::identity::write_private(&draft, &bytes).map_err(|e| e.to_string())?;
                        std::fs::rename(&draft, &file).map_err(|e| e.to_string())?;
                    }
                }
            }
        }
        manifest.epoch = epoch;
        manifest.at = chrono::Utc::now().to_rfc3339();
        self.save(&manifest).map_err(|e| e.to_string())?;
        Ok(manifest)
    }

    /// Forget the copy: the hub found it does not check out, and sends it whole next time.
    pub fn forget(&self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn now_label() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::rename(from, to).or_else(|_| std::fs::copy(from, to).and_then(|_| std::fs::remove_file(from)))
}

/// Make the standby copy this machine's live NyPi home, at the hub's new `epoch` (decision D95).
/// What this machine had of its own state is set aside under `diverged/node-<time>/`; its own
/// key, instance, call journal, and models stay. Env files from outside the hub's home land under
/// `env/`, the settings name them there, and this machine's own env files follow them. Nothing
/// outside the NyPi home is written. The master key goes where `nypi secrets` keeps one.
pub fn promote(paths: &Paths, epoch: u64) -> anyhow::Result<crate::instance::Instance> {
    let standby = Standby::at(paths);
    anyhow::ensure!(standby.exists(), "this machine has no standby copy of the hub's state");
    let own = crate::instance::Instance::load(paths).ok_or_else(|| anyhow::anyhow!("this machine is no node"))?;
    let own_settings = Settings::load(paths, &paths.home).unwrap_or_default();
    let aside = paths.diverged().join(format!("node-{}", now_label()));
    for file in carried(paths, &own_settings).into_iter().filter(|file| file.source.starts_with(&paths.home)) {
        move_file(&file.source, &aside.join(&file.path))?;
    }
    let copied = {
        let mut list = Vec::new();
        walk(&standby.home(), &standby.home(), &mut list);
        list
    };
    for file in copied {
        move_file(&file.source, &paths.home.join(&file.path))?;
    }
    let map: Vec<serde_json::Value> = std::fs::read_to_string(standby.root.join("env-map.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let mut settings: serde_json::Value = std::fs::read_to_string(paths.settings())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if settings.is_object() {
        let mut env_files: Vec<String> = settings["envFiles"]
            .as_array()
            .map(|list| list.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        for named in env_files.iter_mut() {
            let copy = map.iter().find(|entry| entry["original"].as_str() == Some(named.as_str()));
            if let Some(copy) = copy.and_then(|entry| entry["copy"].as_str()) {
                *named = paths.home.join(copy).display().to_string();
            }
        }
        for mine in own_settings.env_files {
            if !env_files.contains(&mine) {
                env_files.push(mine);
            }
        }
        settings["envFiles"] = serde_json::json!(env_files);
        std::fs::write(paths.settings(), serde_json::to_vec_pretty(&settings)?)?;
    }
    if let Some(key) = std::fs::read_to_string(standby.root.join("master.key"))
        .ok()
        .and_then(|text| nypi_vault::secrets::key_from_text(&text))
    {
        nypi_vault::secrets::install_master_key(&paths.home, &key)?;
    }
    standby.forget();
    let instance = crate::instance::Instance {
        mode: crate::instance::Mode::Hub,
        epoch,
        trust: crate::pairing::Trust::Personal,
        direct: Vec::new(),
        down: false,
        ..own
    };
    instance.save(paths)?;
    Ok(instance)
}

/// Set this hub's state aside under `diverged/<old epoch>-<time>/` (decision D95): another hub
/// holds a higher epoch, and nothing of this one is lost, nor merged. The settings, the model
/// list, and the model keys stay in place too (copied), so the machine runs on as a node; the
/// master key is copied beside the store it opens. Where it went.
pub fn set_aside(paths: &Paths, settings: &Settings, old_epoch: u64) -> anyhow::Result<PathBuf> {
    let aside = paths.diverged().join(format!("{old_epoch}-{}", now_label()));
    std::fs::create_dir_all(&aside)?;
    if paths.secrets().exists()
        && let Ok(key) = nypi_vault::secrets::master_key(&paths.home)
    {
        nypi_node::identity::write_private(
            &aside.join("master.key"),
            nypi_vault::secrets::key_to_hex(&key).as_bytes(),
        )?;
    }
    for file in carried(paths, settings).into_iter().filter(|file| file.source.starts_with(&paths.home)) {
        let target = aside.join(&file.path);
        if KEPT_ON_STEP_DOWN.contains(&file.path.as_str()) {
            if let Some(dir) = target.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::copy(&file.source, &target)?;
        } else {
            move_file(&file.source, &target)?;
        }
    }
    Ok(aside)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub_home() -> (tempfile::TempDir, Paths) {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        (home, paths)
    }

    fn write(paths: &Paths, path: &str, text: &str) {
        let file = paths.home.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }

    /// Settings naming no env file: a test never reads the user's own.
    fn no_env() -> Settings {
        Settings { env_files: Vec::new(), ..Default::default() }
    }

    fn send(paths: &Paths, standby: &Standby, epoch: u64) -> Result<Manifest, String> {
        standby.apply(epoch, difference(paths, &no_env(), &standby.manifest()))
    }

    /// What is carried and what is not (decision D95).
    #[test]
    fn the_state_is_the_home_without_what_belongs_to_one_machine() {
        let (_home, paths) = hub_home();
        for path in [
            "settings.json",
            "sessions/primary/s.jsonl",
            "vault/10-projects/x/plan.md",
            "devices/peers.json",
            "hub-id.key",
            "run/hub.json",
            "logs/hub.log",
            "node/journal.jsonl",
            "device-id.key",
            "instance.json",
            "standby/manifest.json",
            "devices/devices.lock",
        ] {
            write(&paths, path, "x");
        }
        let carried: Vec<String> = carried(&paths, &no_env()).into_iter().map(|c| c.path).collect();
        assert_eq!(
            carried,
            [
                "devices/peers.json",
                "hub-id.key",
                "sessions/primary/s.jsonl",
                "settings.json",
                "vault/10-projects/x/plan.md"
            ]
        );
    }

    /// Differences: a log's tail is appended, a changed file is replaced, a file gone is deleted,
    /// and a batch with a bad checksum is not applied at all.
    #[test]
    fn a_copy_takes_tails_changes_and_deletions_and_refuses_a_bad_batch() {
        let (_home, paths) = hub_home();
        let (_node, node) = hub_home();
        let standby = Standby::at(&node);
        write(&paths, "sessions/s.jsonl", "{\"a\":1}\n");
        write(&paths, "goals.md", "first");
        write(&paths, "vault/old.md", "old");
        send(&paths, &standby, 1).unwrap();
        assert_eq!(std::fs::read_to_string(node.standby().join("home/goals.md")).unwrap(), "first");

        write(&paths, "sessions/s.jsonl", "{\"a\":1}\n{\"b\":2}\n");
        write(&paths, "goals.md", "second");
        std::fs::remove_file(paths.home.join("vault/old.md")).unwrap();
        let pieces = difference(&paths, &no_env(), &standby.manifest());
        let changes: Vec<(&str, &Change, u64)> =
            pieces.iter().map(|p| (p.entry.path.as_str(), &p.entry.change, p.entry.bytes)).collect();
        assert!(changes.contains(&("sessions/s.jsonl", &Change::Append { offset: 8 }, 8)), "{changes:?}");
        assert!(changes.contains(&("goals.md", &Change::Whole, 6)));
        assert!(changes.contains(&("vault/old.md", &Change::Delete, 0)));
        standby.apply(1, pieces).unwrap();
        assert_eq!(
            std::fs::read_to_string(node.standby().join("home/sessions/s.jsonl")).unwrap(),
            "{\"a\":1}\n{\"b\":2}\n"
        );
        assert!(!node.standby().join("home/vault/old.md").exists());

        write(&paths, "goals.md", "third");
        let mut pieces = difference(&paths, &no_env(), &standby.manifest());
        pieces.iter_mut().for_each(|piece| piece.bytes = b"tampered".to_vec());
        let refused = standby.apply(2, pieces).unwrap_err();
        assert!(refused.contains("does not check out"), "{refused}");
        assert_eq!(std::fs::read_to_string(node.standby().join("home/goals.md")).unwrap(), "second", "nothing applied");
        assert_eq!(standby.manifest().epoch, 1);
        let escape = Piece {
            entry: Entry { path: "../escape".into(), change: Change::Whole, size: 1, sha: sha(b"x"), bytes: 1 },
            bytes: b"x".to_vec(),
        };
        assert!(standby.apply(2, vec![escape]).is_err(), "a path outside the copy");
    }

    /// The cache of decision D99: a file unchanged on disk and in the copy is not read again; a
    /// log that grew is read from where the copy ends, with the same pieces and checksums as a
    /// batch made from nothing.
    #[test]
    fn a_batch_reads_only_what_changed_since_the_last() {
        let (_home, paths) = hub_home();
        let (_node, node) = hub_home();
        let standby = Standby::at(&node);
        write(&paths, "sessions/s.jsonl", "{\"a\":1}\n");
        write(&paths, "goals.md", "first");
        let mut cache = StateCache::default();
        standby.apply(1, difference_cached(&paths, &no_env(), &standby.manifest(), &mut cache)).unwrap();
        assert_eq!((cache.whole_reads, cache.tail_reads), (2, 0));
        let again = difference_cached(&paths, &no_env(), &standby.manifest(), &mut cache);
        assert!(again.is_empty(), "nothing changed");
        assert_eq!((cache.whole_reads, cache.tail_reads), (2, 0), "and nothing was read");

        let mut log = std::fs::OpenOptions::new().append(true).open(paths.home.join("sessions/s.jsonl")).unwrap();
        log.write_all(b"{\"b\":2}\n").unwrap();
        drop(log);
        let fresh = difference(&paths, &no_env(), &standby.manifest());
        let cached = difference_cached(&paths, &no_env(), &standby.manifest(), &mut cache);
        assert_eq!((cache.whole_reads, cache.tail_reads), (2, 1), "only the tail was read");
        let view = |pieces: &[Piece]| -> Vec<(Entry, Vec<u8>)> {
            pieces.iter().map(|piece| (piece.entry.clone(), piece.bytes.clone())).collect()
        };
        assert_eq!(view(&cached), view(&fresh), "the same pieces as from nothing");
        assert_eq!(cached[0].entry.change, Change::Append { offset: 8 });
        assert_eq!(cached[0].entry.sha, sha(b"{\"a\":1}\n{\"b\":2}\n"));
        standby.apply(1, cached).unwrap();
        assert_eq!(
            std::fs::read_to_string(node.standby().join("home/sessions/s.jsonl")).unwrap(),
            "{\"a\":1}\n{\"b\":2}\n"
        );
        assert!(difference_cached(&paths, &no_env(), &standby.manifest(), &mut cache).is_empty());
    }

    /// Env files outside the hub's home land under `env/`, the settings name them there, the
    /// receiving machine's own follow, and nothing outside its NyPi home is written.
    #[test]
    fn a_promoted_copy_takes_env_files_into_the_home_and_keeps_the_machines_own() {
        let outside = tempfile::tempdir().unwrap();
        let (_home, paths) = hub_home();
        let env = outside.path().join(".env");
        std::fs::write(&env, "TOKEN=abc\n").unwrap();
        write(&paths, "settings.json", &serde_json::json!({ "envFiles": [env.display().to_string()] }).to_string());
        write(&paths, "goals.md", "the hub's goals");
        let settings = Settings::load(&paths, &paths.home).unwrap();

        let (_node_home, node) = hub_home();
        let mine = tempfile::tempdir().unwrap();
        let my_env = mine.path().join("work.env");
        std::fs::write(&my_env, "MINE=1\n").unwrap();
        write(&node, "settings.json", &serde_json::json!({ "envFiles": [my_env.display().to_string()] }).to_string());
        write(&node, "goals.md", "the node's own");
        crate::instance::Instance {
            mode: crate::instance::Mode::Node,
            name: "macbook".into(),
            epoch: 1,
            hub_fingerprint: "ab".into(),
            ..Default::default()
        }
        .save(&node)
        .unwrap();
        let standby = Standby::at(&node);
        standby.apply(1, difference(&paths, &settings, &standby.manifest())).unwrap();
        let before_outside = std::fs::read_to_string(&my_env).unwrap();
        let promoted = promote(&node, 2).unwrap();
        assert_eq!((promoted.mode, promoted.epoch, promoted.name.as_str()), (crate::instance::Mode::Hub, 2, "macbook"));
        assert_eq!(std::fs::read_to_string(node.home.join("env/.env")).unwrap(), "TOKEN=abc\n");
        let now = Settings::load(&node, &node.home).unwrap();
        assert_eq!(now.env_files, [node.home.join("env/.env").display().to_string(), my_env.display().to_string()]);
        assert_eq!(std::fs::read_to_string(node.home.join("goals.md")).unwrap(), "the hub's goals");
        let aside = std::fs::read_dir(node.diverged()).unwrap().next().unwrap().unwrap().path();
        assert_eq!(std::fs::read_to_string(aside.join("goals.md")).unwrap(), "the node's own", "set aside, not lost");
        assert_eq!(std::fs::read_to_string(&my_env).unwrap(), before_outside, "nothing outside the home written");
        assert_eq!(std::fs::read_to_string(&env).unwrap(), "TOKEN=abc\n");
        assert!(!node.standby().exists());
    }
}
