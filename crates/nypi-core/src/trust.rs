//! Project trust (decision D61), ported from Pi's `core/trust-manager.ts` and
//! `core/project-trust.ts`: a folder's own NyPi configuration (`.nypi/settings.json`,
//! `.nypi/specialists`, `.nypi/skills`, `.agents/skills`) can turn approvals off, add programs
//! as tools, and start MCP servers, so it is loaded only from folders the user has said they
//! trust. Decisions live in `~/.nypi/trust.json`, a map from a canonical path to true or false;
//! the nearest ancestor with a decision wins, as in Pi. Only the user writes it (`nypi trust`,
//! or the question a terminal asks at startup); the Inspector guard and the tool sandbox keep
//! the agent away from it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Paths;

/// What in a folder needs the user's trust before NyPi loads it, relative to the folder.
const TRUST_REQUIRING: [&str; 4] = [".nypi/settings.json", ".nypi/specialists", ".nypi/skills", ".agents/skills"];

/// Whether a folder's own configuration may be loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// The user trusts this folder or one above it.
    Trusted,
    /// The user declined this folder or one above it.
    Declined,
    /// Nobody has decided, and the folder has configuration that would be loaded: it is not.
    Undecided { found: Vec<String> },
}

impl Trust {
    pub fn is_trusted(&self) -> bool {
        *self == Trust::Trusted
    }
}

pub struct TrustStore {
    path: PathBuf,
}

/// The path trust is recorded under: symlinks resolved, so one folder has one name, also when
/// it does not exist yet (a declined parent covers a folder created later).
fn canonical(dir: &Path) -> PathBuf {
    crate::protected::real_path(dir)
}

/// The trust-requiring configuration that exists in `cwd`, whoever it belongs to.
fn project_resources(cwd: &Path) -> Vec<String> {
    TRUST_REQUIRING.iter().filter(|entry| cwd.join(entry).exists()).map(|entry| entry.to_string()).collect()
}

impl TrustStore {
    pub fn new(paths: &Paths) -> Self {
        TrustStore { path: paths.home.join("trust.json") }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The configuration in `cwd` that is that folder's own and needs the user's trust. What
    /// NyPi loads anyway is not a project's: in the user's home directory `.nypi` is NyPi's own
    /// home and `.agents/skills` is the user-level skill folder, so a daemon working there has
    /// nothing to ask about.
    pub fn found(&self, cwd: &Path) -> Vec<String> {
        let own = self.path.parent().map(crate::protected::real_path);
        let at_home = dirs::home_dir().is_some_and(|home| canonical(&home) == canonical(cwd));
        project_resources(cwd)
            .into_iter()
            .filter(|entry| {
                let inside_own = own.as_ref().is_some_and(|home| canonical(&cwd.join(entry)).starts_with(home));
                let user_level = at_home && entry == ".agents/skills";
                !(inside_own || user_level)
            })
            .collect()
    }

    /// The decisions on file. A store that cannot be read counts as empty: nothing is trusted.
    fn read(&self) -> BTreeMap<PathBuf, bool> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return BTreeMap::new() };
        let entries: BTreeMap<String, Option<bool>> = serde_json::from_str(&text).unwrap_or_default();
        entries.into_iter().filter_map(|(path, decision)| Some((PathBuf::from(path), decision?))).collect()
    }

    /// The decision that applies to `cwd` and the folder it was made for: its own, or the
    /// nearest ancestor's.
    pub fn decision(&self, cwd: &Path) -> Option<(PathBuf, bool)> {
        let decisions = self.read();
        let cwd = canonical(cwd);
        cwd.ancestors().find_map(|dir| decisions.get(dir).map(|decision| (dir.to_path_buf(), *decision)))
    }

    /// Whether `cwd`'s configuration may be loaded. As in Pi, a folder with nothing to load is
    /// trusted unless the user declined it, and an undecided folder with something to load is
    /// not: the caller asks the user, or leaves the configuration out.
    pub fn resolve(&self, cwd: &Path) -> Trust {
        match self.decision(cwd) {
            Some((_, true)) => Trust::Trusted,
            Some((_, false)) => Trust::Declined,
            None => match self.found(cwd) {
                found if found.is_empty() => Trust::Trusted,
                found => Trust::Undecided { found },
            },
        }
    }

    /// Record (or with `None`, forget) the user's decision for `dir`.
    pub fn set(&self, dir: &Path, decision: Option<bool>) -> anyhow::Result<()> {
        let mut decisions = self.read();
        let dir = canonical(dir);
        match decision {
            Some(decision) => decisions.insert(dir, decision),
            None => decisions.remove(&dir),
        };
        let entries: BTreeMap<String, bool> =
            decisions.into_iter().map(|(path, decision)| (path.display().to_string(), decision)).collect();
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Written whole and renamed, so a crash never leaves half a trust store.
        let draft = self.path.with_extension("json.tmp");
        std::fs::write(&draft, format!("{}\n", serde_json::to_string_pretty(&entries)?))?;
        std::fs::rename(draft, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, TrustStore) {
        let home = tempfile::tempdir().unwrap();
        let store = TrustStore::new(&Paths { home: home.path().to_path_buf() });
        (home, store)
    }

    #[test]
    fn a_folder_with_configuration_is_not_loaded_until_the_user_decides() {
        let (_home, store) = store();
        let work = tempfile::tempdir().unwrap();
        assert_eq!(store.resolve(work.path()), Trust::Trusted, "nothing to load, nothing to decide");

        std::fs::create_dir_all(work.path().join(".nypi/specialists")).unwrap();
        std::fs::write(work.path().join(".nypi/settings.json"), "{}").unwrap();
        let found = vec![".nypi/settings.json".to_string(), ".nypi/specialists".to_string()];
        assert_eq!(store.resolve(work.path()), Trust::Undecided { found });
        assert!(!store.resolve(work.path()).is_trusted());

        store.set(work.path(), Some(true)).unwrap();
        assert_eq!(store.resolve(work.path()), Trust::Trusted);
        store.set(work.path(), Some(false)).unwrap();
        assert_eq!(store.resolve(work.path()), Trust::Declined);
        store.set(work.path(), None).unwrap();
        assert!(matches!(store.resolve(work.path()), Trust::Undecided { .. }), "forgotten: undecided again");
    }

    #[test]
    fn nypis_own_home_is_not_a_projects_configuration() {
        // The daemon works in the user's home, where `.nypi` is NyPi itself.
        let user = tempfile::tempdir().unwrap();
        let home = user.path().join(".nypi");
        std::fs::create_dir_all(home.join("specialists")).unwrap();
        std::fs::write(home.join("settings.json"), "{}").unwrap();
        let store = TrustStore::new(&Paths { home: home.clone() });
        assert!(store.found(user.path()).is_empty());
        assert_eq!(store.resolve(user.path()), Trust::Trusted, "nothing of a project's to load, nothing to ask");

        // Another folder's `.nypi` is still that folder's own, and still asked about.
        let project = user.path().join("code/site");
        std::fs::create_dir_all(project.join(".nypi/specialists")).unwrap();
        std::fs::create_dir_all(project.join(".agents/skills")).unwrap();
        assert_eq!(store.found(&project), [".nypi/specialists", ".agents/skills"]);
        assert!(matches!(store.resolve(&project), Trust::Undecided { .. }));
        // A link to NyPi's home from elsewhere is NyPi's home too.
        std::os::unix::fs::symlink(&home, user.path().join("code/.nypi")).unwrap();
        assert!(store.found(&user.path().join("code")).is_empty());
    }

    #[test]
    fn the_nearest_decision_above_a_folder_applies_to_it() {
        let (_home, store) = store();
        let parent = tempfile::tempdir().unwrap();
        let project = parent.path().join("code/site");
        std::fs::create_dir_all(project.join(".agents/skills")).unwrap();
        store.set(parent.path(), Some(true)).unwrap();
        assert_eq!(store.resolve(&project), Trust::Trusted, "a trusted parent covers what is below it");
        store.set(&parent.path().join("code"), Some(false)).unwrap();
        assert_eq!(store.resolve(&project), Trust::Declined, "the nearer decision wins");
        let (decided_for, decision) = store.decision(&project).unwrap();
        assert!(decided_for.ends_with("code") && !decision);
        // A declined folder stays declined even with nothing in it to load.
        assert_eq!(store.resolve(&parent.path().join("code/empty")), Trust::Declined);
    }

    #[test]
    fn a_broken_store_trusts_nothing_and_a_symlink_is_the_folder_it_names() {
        let (home, store) = store();
        let work = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(work.path().join(".nypi")).unwrap();
        std::fs::write(work.path().join(".nypi/settings.json"), "{}").unwrap();
        std::fs::write(home.path().join("trust.json"), "not json").unwrap();
        assert!(matches!(store.resolve(work.path()), Trust::Undecided { .. }));

        store.set(work.path(), Some(true)).unwrap();
        let link = home.path().join("link");
        std::os::unix::fs::symlink(work.path(), &link).unwrap();
        assert_eq!(store.resolve(&link), Trust::Trusted, "trust follows the real folder, however it is reached");
    }
}
