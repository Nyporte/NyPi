//! What the agent's tools are kept away from (requirement 5, decision D62), stated once so the
//! Inspector guard (which checks tool arguments) and the tool sandbox (which the operating
//! system enforces on subprocesses) protect the same things:
//! - NyPi's home (`~/.nypi`): the audit log, ledger, outbox, prompts, settings, trust store,
//!   specialists, and everything else there is written by the harness or the user only;
//! - any `.nypi` folder under the user's home directory: a project's own NyPi configuration,
//!   which can turn approvals off or add programs as tools;
//! - the supervisor's LaunchAgent;
//! - secrets, which may not be read either: `auth.json`, the env files, and `run/` (tokens).

use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};

use crate::config::{Paths, expand_home};

#[derive(Clone, Debug)]
pub struct Protected {
    /// NyPi's home, with symlinks resolved.
    pub home: PathBuf,
    /// The user's home directory, with symlinks resolved; None when it is unknown.
    pub user_home: Option<PathBuf>,
    /// Single files outside NyPi's home that only the user changes: the supervisor's service
    /// files (a LaunchAgent, a systemd user unit).
    pub files: Vec<PathBuf>,
    /// Folders outside NyPi's home that only the user changes: the user-level skill folders,
    /// whose skills load into every prompt without a trust question, and the user's service
    /// folders, whose units run outside every sandbox (decision D99). They may be read.
    pub folders: Vec<PathBuf>,
    /// Files and folders whose contents are secrets.
    pub secrets: Vec<PathBuf>,
}

/// How many symlinks one path may pass through before it counts as a loop (Linux's limit).
const MAX_SYMLINK_HOPS: usize = 40;

/// The file a tool would really reach through `path`, whether or not it exists yet: each
/// component is looked up in turn as the kernel does, a symlink replaced by its target (read
/// relative to the folder it is in), and `..` applied to the folder reached so far, never to
/// the spelling. So `link/..` is the parent of where `link` points, and a symlink whose target
/// does not exist yet is its target, which a write would create. A component that does not
/// exist ends the lookups: what follows is joined as spelled. Last, the deepest existing folder
/// is canonicalized, which on macOS also gives every name the case it has on disk.
pub fn real_path(path: &Path) -> PathBuf {
    let absolute;
    let path = if path.is_relative() {
        match std::env::current_dir() {
            Ok(dir) => {
                absolute = dir.join(path);
                absolute.as_path()
            }
            Err(_) => path,
        }
    } else {
        path
    };
    let mut pending: VecDeque<Part> = parts(path);
    let mut resolved = PathBuf::new();
    let mut hops = 0;
    let mut missing = false;
    while let Some(part) = pending.pop_front() {
        match part {
            Part::Root(root) => resolved = PathBuf::from(root),
            Part::Parent => {
                resolved.pop();
            }
            Part::Name(name) => {
                let candidate = resolved.join(&name);
                if missing {
                    resolved = candidate;
                    continue;
                }
                match std::fs::symlink_metadata(&candidate) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        hops += 1;
                        match std::fs::read_link(&candidate) {
                            // A loop resolves to nothing; the spelling stays, and opening it fails.
                            Ok(target) if hops <= MAX_SYMLINK_HOPS => {
                                for part in parts(&target).into_iter().rev() {
                                    pending.push_front(part);
                                }
                            }
                            _ => resolved = candidate,
                        }
                    }
                    Ok(_) => resolved = candidate,
                    Err(_) => {
                        missing = true;
                        resolved = candidate;
                    }
                }
            }
        }
    }
    canonical_prefix(&resolved)
}

/// `path` without `.` and with each `..` taking off the step before it, by spelling alone: for
/// a path on another device, whose links this computer cannot follow (that device's own guard
/// follows them).
pub fn lexical_path(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other),
        }
    }
    clean
}

/// One step of a path, owned, so a symlink's target can be spliced into the steps still to go.
enum Part {
    Root(std::ffi::OsString),
    Parent,
    Name(std::ffi::OsString),
}

fn parts(path: &Path) -> VecDeque<Part> {
    let mut parts = VecDeque::new();
    let mut root = std::ffi::OsString::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => root.push(prefix.as_os_str()),
            Component::RootDir => {
                root.push(component.as_os_str());
                parts.push_back(Part::Root(std::mem::take(&mut root)));
            }
            Component::CurDir => {}
            Component::ParentDir => parts.push_back(Part::Parent),
            Component::Normal(name) => parts.push_back(Part::Name(name.to_os_string())),
        }
    }
    parts
}

/// `path` (already free of symlinks and `..`) with its deepest existing ancestor canonicalized.
fn canonical_prefix(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            return tail.iter().rev().fold(real, |path, name| path.join(name));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Whether a name is `.nypi` as a case-insensitive file system would find it.
fn is_nypi_folder(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| name.eq_ignore_ascii_case(".nypi"))
}

impl Protected {
    pub fn new(paths: &Paths, env_files: &[String]) -> Self {
        let user_home = dirs::home_dir().map(|home| real_path(&home));
        let mut secrets = vec![
            paths.auth(),
            paths.env_file(),
            paths.home.join("run"),
            // The secret store and its key (decision D77): used by the harness for the agent,
            // never read by a tool.
            paths.secrets(),
            paths.secrets_key(),
            // Who this machine is to the owner's other devices, and the pairing codes it gave
            // out (decision D80): whoever reads a key can be this device.
            paths.hub_key(),
            paths.device_key(),
            paths.interface_key(),
            paths.pairing(),
            paths.join_requests(),
            // The hub's state as a standby copy, a hub's state set aside, and env files carried
            // from another machine (decision D95): the secrets and keys of the hub are in them.
            paths.standby(),
            paths.diverged(),
            paths.env_copies(),
        ];
        secrets.extend(env_files.iter().map(|file| expand_home(file)));
        let services = service_folders();
        let files = services
            .iter()
            .zip(["dev.nypi.supervisor.plist", "nypi-supervisor.service"])
            .map(|(folder, name)| folder.join(name))
            .collect();
        let mut protected = Protected::from_parts(&paths.home, user_home, files, secrets);
        // Skills load into every prompt; a service unit runs, outside every sandbox, at the next
        // login or reload (decision D99): only the user puts either there.
        protected.folders =
            crate::skills::user_skill_dirs().iter().chain(&services).map(|dir| real_path(dir)).collect();
        protected
    }

    pub fn from_parts(home: &Path, user_home: Option<PathBuf>, files: Vec<PathBuf>, secrets: Vec<PathBuf>) -> Self {
        Protected {
            home: real_path(home),
            user_home,
            files: files.iter().map(|file| real_path(file)).collect(),
            folders: Vec::new(),
            secrets: secrets.iter().map(|secret| real_path(secret)).collect(),
        }
    }

    /// Why a tool may not write to `path` (absolute), or None when it may.
    pub fn write_denied(&self, path: &Path) -> Option<String> {
        let path = real_path(path);
        if path.starts_with(&self.home) {
            return Some(format!(
                "{} is inside NyPi's own folder, which only the harness and the user change",
                path.display()
            ));
        }
        if self.files.contains(&path) {
            return Some(format!(
                "{} is the supervisor's service definition, which only the user changes",
                path.display()
            ));
        }
        if let Some(folder) = self.folders.iter().find(|folder| path.starts_with(folder)) {
            let service = folder.ends_with("Library/LaunchAgents") || folder.ends_with(".config/systemd/user");
            let why = if service {
                "where a service file runs at the user's next login or reload, outside every sandbox"
            } else {
                "whose skills load into every prompt"
            };
            return Some(format!("{} is in {}, {why}, so only the user changes it", path.display(), folder.display()));
        }
        let under_user_home = self.user_home.as_ref().is_none_or(|home| path.starts_with(home));
        if under_user_home && path.components().any(|c| is_nypi_folder(c.as_os_str())) {
            return Some(format!(
                "{} is a folder's NyPi configuration (.nypi), which only the user changes",
                path.display()
            ));
        }
        None
    }

    /// Why a tool may not read `path` (absolute), or None when it may.
    pub fn read_denied(&self, path: &Path) -> Option<String> {
        let path = real_path(path);
        self.secrets
            .iter()
            .any(|secret| path.starts_with(secret))
            .then(|| format!("{} holds keys or tokens, which tools never read", path.display()))
    }
}

/// The user's service folders: launchd's agents on macOS, systemd's user units on Linux. Both are
/// protected on every system, so a tool cannot leave a unit for the other to find. On Linux the
/// unit folder is made when missing: bubblewrap can only keep a folder read-only that exists.
pub fn service_folders() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let units = home.join(".config").join("systemd").join("user");
    if cfg!(target_os = "linux") && !units.exists() {
        let _ = std::fs::create_dir_all(&units);
    }
    vec![home.join("Library").join("LaunchAgents"), units]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protected() -> (tempfile::TempDir, Protected) {
        let user = tempfile::tempdir().unwrap();
        let home = user.path().join(".nypi");
        std::fs::create_dir_all(home.join("audit")).unwrap();
        std::fs::write(user.path().join(".env"), "KEY=1").unwrap();
        let secrets = vec![home.join("auth.json"), user.path().join(".env"), home.join("run")];
        let plist = user.path().join("Library/LaunchAgents/dev.nypi.supervisor.plist");
        let protected = Protected::from_parts(&home, Some(real_path(user.path())), vec![plist], secrets);
        (user, protected)
    }

    #[test]
    fn every_spelling_of_a_protected_path_is_the_same_path() {
        let (user, protected) = protected();
        let home = user.path().join(".nypi");
        std::fs::create_dir_all(user.path().join("work")).unwrap();
        std::os::unix::fs::symlink(&home, user.path().join("work/innocent")).unwrap();
        for path in [
            home.join("audit/2026-11-10.jsonl"),
            home.join("settings.json"),
            home.join("not/yet/there.json"),
            user.path().join("work/../.nypi/telegram.json"),
            user.path().join("work/./innocent/trust.json"),
            user.path().join("work/innocent/specialists/x/profile.json"),
        ] {
            assert!(protected.write_denied(&path).is_some(), "{} must be protected", path.display());
        }
        assert!(protected.write_denied(&user.path().join("work/notes.md")).is_none());
        assert!(protected.write_denied(&user.path().join("work/nypi/settings.json")).is_none(), "only .nypi folders");
    }

    #[test]
    fn a_projects_nypi_folder_is_the_users_but_only_under_their_home() {
        let (user, protected) = protected();
        let project = user.path().join("code/site/.nypi/settings.json");
        assert!(protected.write_denied(&project).unwrap().contains("only the user changes"));
        assert!(protected.write_denied(&user.path().join("code/site/.nypi/specialists/a/profile.json")).is_some());
        // Outside the user's home (a scratch or test directory) a `.nypi` folder is just a folder:
        // it is not trusted, so nothing loads from it.
        let scratch = tempfile::tempdir().unwrap();
        assert!(protected.write_denied(&scratch.path().join(".nypi/settings.json")).is_none());
        let plist = user.path().join("Library/LaunchAgents/dev.nypi.supervisor.plist");
        assert!(protected.write_denied(&plist).unwrap().contains("service definition"));
    }

    #[test]
    fn secrets_are_not_read_however_they_are_reached() {
        let (user, protected) = protected();
        let home = user.path().join(".nypi");
        std::os::unix::fs::symlink(user.path().join(".env"), user.path().join("config")).unwrap();
        for path in [
            user.path().join(".env"),
            user.path().join("config"),
            home.join("auth.json"),
            home.join("run/board.token"),
            home.join("audit/../run/observe.token"),
        ] {
            assert!(protected.read_denied(&path).is_some(), "{} must not be read", path.display());
        }
        assert!(protected.read_denied(&home.join("audit/2026-11-10.jsonl")).is_none(), "the audit log may be read");
        assert!(protected.read_denied(&user.path().join("notes.env")).is_none());
    }

    #[test]
    fn a_path_is_followed_as_the_kernel_follows_it() {
        let root = tempfile::tempdir().unwrap();
        let root_path = real_path(root.path());
        let deep = root_path.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root_path.join("work")).unwrap();
        let link = root_path.join("work/link");
        std::os::unix::fs::symlink(&deep, &link).unwrap();
        // `link/..` is the parent of where the link points, not `work`.
        assert_eq!(real_path(&link.join("../x.json")), root_path.join("a/x.json"));
        // A relative target is read from the folder the link is in.
        std::os::unix::fs::symlink("../a", root_path.join("work/up")).unwrap();
        assert_eq!(real_path(&root_path.join("work/up/b/new.txt")), root_path.join("a/b/new.txt"));
        // A link to a file that does not exist yet is that file.
        std::os::unix::fs::symlink(root_path.join("a/later.json"), root_path.join("work/soon")).unwrap();
        assert_eq!(real_path(&root_path.join("work/soon")), root_path.join("a/later.json"));
        // A chain of links, and a loop, which resolves to nothing and keeps its spelling.
        std::os::unix::fs::symlink(root_path.join("work/soon"), root_path.join("work/chain")).unwrap();
        assert_eq!(real_path(&root_path.join("work/chain")), root_path.join("a/later.json"));
        std::os::unix::fs::symlink(root_path.join("work/loop"), root_path.join("work/loop")).unwrap();
        assert_eq!(real_path(&root_path.join("work/loop")), root_path.join("work/loop"));
        // Below a missing folder, `..` is lexical: there is nothing to follow.
        assert_eq!(real_path(&root_path.join("work/gone/../link/y")), root_path.join("a/b/y"));
    }

    #[test]
    fn a_link_to_a_protected_file_not_yet_written_is_protected() {
        let (user, protected) = protected();
        let home = user.path().join(".nypi");
        std::fs::create_dir_all(user.path().join("work")).unwrap();
        std::os::unix::fs::symlink(home.join("settings.json"), user.path().join("work/harmless.txt")).unwrap();
        std::os::unix::fs::symlink(home.join("audit"), user.path().join("work/escape")).unwrap();
        assert!(protected.write_denied(&user.path().join("work/harmless.txt")).is_some());
        assert!(protected.write_denied(&user.path().join("work/escape/../settings.json")).is_some());
        assert!(protected.read_denied(&user.path().join("work/escape/../auth.json")).is_some());
        assert!(protected.write_denied(&user.path().join("code/site/.NYPI/settings.json")).is_some(), "any case");
    }

    /// The user's service folders on both systems (decision D99): the supervisor's own unit and
    /// plist, and any new unit a tool would plant, may not be written; they may be read.
    #[test]
    fn no_tool_plants_a_service_unit() {
        let home = tempfile::tempdir().unwrap();
        let protected = Protected::new(&Paths { home: home.path().to_path_buf() }, &[]);
        let user = dirs::home_dir().unwrap();
        for path in [
            user.join(".config/systemd/user/nypi-supervisor.service"),
            user.join(".config/systemd/user/evil.service"),
            user.join("Library/LaunchAgents/dev.nypi.supervisor.plist"),
            user.join("Library/LaunchAgents/evil.plist"),
        ] {
            let why = protected.write_denied(&path).unwrap_or_else(|| panic!("{} may be written", path.display()));
            assert!(why.contains("only the user changes it") || why.contains("service definition"), "{why}");
            assert!(protected.read_denied(&path).is_none(), "{} may still be read", path.display());
        }
        let unit = protected.write_denied(&user.join(".config/systemd/user/evil.service")).unwrap();
        assert!(unit.contains("outside every sandbox"), "{unit}");
        assert!(protected.write_denied(&user.join(".config/other/file")).is_none(), "the rest of ~/.config is not");
    }
}
