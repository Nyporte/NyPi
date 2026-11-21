//! The asset registry (decision D77): what Lucia looks after for the owner and for herself, as
//! records she can read: accounts, servers, domains, subscriptions, identities, deliverables,
//! with who they belong to, how they are named, when they renew, and which secrets go with
//! them. A secret is only ever named here, by its `cred://` handle; the registry refuses a
//! record that carries a raw key or password, so it is safe to show to a model and to keep as a
//! plain, append-only log (`assets.jsonl`, folded like the other stores).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::jsonl::Folded;
use crate::secrets::{Handle, looks_secret};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetKind {
    Account,
    Server,
    Domain,
    Subscription,
    Identity,
    Deliverable,
}

impl AssetKind {
    pub fn parse(text: &str) -> Option<AssetKind> {
        serde_json::from_value(serde_json::Value::String(text.trim().to_lowercase())).ok()
    }

    pub fn label(self) -> &'static str {
        match self {
            AssetKind::Account => "account",
            AssetKind::Server => "server",
            AssetKind::Domain => "domain",
            AssetKind::Subscription => "subscription",
            AssetKind::Identity => "identity",
            AssetKind::Deliverable => "deliverable",
        }
    }
}

/// Whose it is: the owner's, which Lucia was given to look after, or one of her own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetSteward {
    #[default]
    Owner,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetRecord {
    pub id: String,
    pub kind: AssetKind,
    #[serde(default)]
    pub steward: AssetSteward,
    pub name: String,
    /// What it is known by outside, never a secret: a domain, a user name, a host, a URL.
    #[serde(default)]
    pub identifier: String,
    /// The secrets that go with it, as handles only (`cred://github/lucia-bot`).
    #[serde(default)]
    pub secret_refs: Vec<String>,
    /// When it renews or expires, as a date (`YYYY-MM-DD`) or an RFC 3339 time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renews_at: Option<String>,
    #[serde(default)]
    pub notes: String,
    pub updated_at: String,
}

/// What is written to `assets.jsonl`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase")]
pub enum AssetEvent {
    Upserted { record: AssetRecord },
    Removed { id: String, at: String },
}

#[derive(Default)]
struct All {
    records: BTreeMap<String, AssetRecord>,
}

fn take_in(all: &mut All, line: &str, _: u64) {
    match serde_json::from_str::<AssetEvent>(line) {
        Ok(AssetEvent::Upserted { record }) => {
            all.records.insert(record.id.clone(), record);
        }
        Ok(AssetEvent::Removed { id, .. }) => {
            all.records.remove(&id);
        }
        // A line that does not read is skipped; the rest of the log still does.
        Err(_) => {}
    }
}

/// What a caller gives to add or change a record; what it leaves out stays as it was.
#[derive(Clone, Debug, Default)]
pub struct AssetChange {
    pub id: Option<String>,
    pub kind: Option<AssetKind>,
    pub steward: Option<AssetSteward>,
    pub name: Option<String>,
    pub identifier: Option<String>,
    pub secret_refs: Option<Vec<String>>,
    /// `Some(None)` clears the date.
    pub renews_at: Option<Option<String>>,
    pub notes: Option<String>,
}

pub struct AssetRegistry {
    path: PathBuf,
    all: Folded<All>,
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The day a date or time names, when it names one.
fn day_of(text: &str) -> Option<chrono::NaiveDate> {
    let text = text.trim();
    chrono::NaiveDate::parse_from_str(text.get(..10)?, "%Y-%m-%d").ok()
}

impl AssetRegistry {
    pub fn new(path: PathBuf) -> AssetRegistry {
        let all = Folded::shared(&path);
        AssetRegistry { path, all }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read<R>(&self, look: impl FnOnce(&All) -> R) -> R {
        self.all.read(take_in, look)
    }

    /// Every record, by id.
    pub fn list(&self) -> Vec<AssetRecord> {
        self.read(|all| all.records.values().cloned().collect())
    }

    pub fn get(&self, id: &str) -> Option<AssetRecord> {
        self.read(|all| all.records.get(id).cloned())
    }

    /// Add a record, or change the one `change.id` names. A new record needs a kind and a name.
    /// Refused when any field holds what looks like a raw secret, or a secret reference is not
    /// a handle: the registry is read by the model, and a secret in it would be in every prompt
    /// that lists it.
    pub fn upsert(&self, change: AssetChange) -> Result<AssetRecord, String> {
        let existing = change.id.as_deref().and_then(|id| self.get(id));
        if let (Some(id), None) = (&change.id, &existing) {
            return Err(format!("no asset {id}; leave `id` out to add a new one"));
        }
        let mut record = match existing {
            Some(record) => record,
            None => AssetRecord {
                id: format!("a-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
                kind: change.kind.ok_or("a new asset needs a kind")?,
                steward: AssetSteward::default(),
                name: String::new(),
                identifier: String::new(),
                secret_refs: Vec::new(),
                renews_at: None,
                notes: String::new(),
                updated_at: String::new(),
            },
        };
        if let Some(kind) = change.kind {
            record.kind = kind;
        }
        if let Some(steward) = change.steward {
            record.steward = steward;
        }
        if let Some(name) = change.name {
            record.name = name.trim().to_string();
        }
        if let Some(identifier) = change.identifier {
            record.identifier = identifier.trim().to_string();
        }
        if let Some(notes) = change.notes {
            record.notes = notes.trim().to_string();
        }
        if let Some(renews_at) = change.renews_at {
            record.renews_at = renews_at.map(|at| at.trim().to_string()).filter(|at| !at.is_empty());
        }
        if let Some(refs) = change.secret_refs {
            let mut handles = Vec::new();
            for reference in refs {
                handles.push(Handle::parse(&reference)?.to_string());
            }
            record.secret_refs = handles;
        }
        if record.name.is_empty() {
            return Err("an asset needs a name".into());
        }
        if let Some(at) = &record.renews_at
            && day_of(at).is_none()
        {
            return Err(format!("renewsAt {at} is not a date (YYYY-MM-DD)"));
        }
        for (field, text) in [("name", &record.name), ("identifier", &record.identifier), ("notes", &record.notes)] {
            if let Some(what) = looks_secret(text) {
                return Err(format!(
                    "`{field}` looks like it holds {what}. The registry never holds secrets: the user stores it \
                     with `nypi secrets set <scope>/<name>`, and the record names it in secretRefs as \
                     cred://<scope>/<name>"
                ));
            }
        }
        record.updated_at = now();
        crate::jsonl::append(&self.path, &AssetEvent::Upserted { record: record.clone() })
            .map_err(|error| format!("the registry could not be written: {error}"))?;
        Ok(record)
    }

    /// Take a record out. False when there was none.
    pub fn remove(&self, id: &str) -> Result<bool, String> {
        if self.get(id).is_none() {
            return Ok(false);
        }
        crate::jsonl::append(&self.path, &AssetEvent::Removed { id: id.to_string(), at: now() })
            .map_err(|error| format!("the registry could not be written: {error}"))?;
        Ok(true)
    }

    /// The records that renew or expire within `days` of `today` (those already past included),
    /// soonest first.
    pub fn expiring_within(&self, days: i64, today: chrono::NaiveDate) -> Vec<(AssetRecord, i64)> {
        let mut due: Vec<(AssetRecord, i64)> = self
            .list()
            .into_iter()
            .filter_map(|record| {
                let left = (day_of(record.renews_at.as_deref()?)? - today).num_days();
                (left <= days).then_some((record, left))
            })
            .collect();
        due.sort_by_key(|(record, left)| (*left, record.id.clone()));
        due
    }
}

/// One record on one line, for a list.
pub fn line(record: &AssetRecord) -> String {
    let mut text = format!("{} · {} · {}", record.id, record.kind.label(), record.name);
    if !record.identifier.is_empty() {
        text.push_str(&format!(" ({})", record.identifier));
    }
    if record.steward == AssetSteward::Agent {
        text.push_str(" · the agent's own");
    }
    if let Some(at) = &record.renews_at {
        text.push_str(&format!(" · renews {at}"));
    }
    if !record.secret_refs.is_empty() {
        text.push_str(&format!(" · {}", record.secret_refs.join(", ")));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (tempfile::TempDir, AssetRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let registry = AssetRegistry::new(dir.path().join("assets.jsonl"));
        (dir, registry)
    }

    #[test]
    fn records_are_added_changed_and_removed_and_only_ever_name_secrets() {
        let (dir, registry) = registry();
        let added = registry
            .upsert(AssetChange {
                kind: Some(AssetKind::Domain),
                name: Some("example.com".into()),
                identifier: Some("example.com at Namecheap".into()),
                secret_refs: Some(vec!["namecheap/api".into()]),
                renews_at: Some(Some("2027-03-01".into())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(added.secret_refs, ["cred://namecheap/api"], "written as a handle");
        let changed = registry
            .upsert(AssetChange {
                id: Some(added.id.clone()),
                notes: Some("auto-renew is on".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!((changed.name.as_str(), changed.notes.as_str()), ("example.com", "auto-renew is on"));
        assert_eq!(registry.list().len(), 1);

        // A raw secret is refused wherever it is put, and so is a reference that is not a handle.
        for change in [
            AssetChange {
                id: Some(added.id.clone()),
                notes: Some("key sk-live-9aF3kLmN0pQrStUv".into()),
                ..Default::default()
            },
            AssetChange {
                kind: Some(AssetKind::Account),
                name: Some("gh".into()),
                identifier: Some("ghp_abcdefghijklmnopqrstuvwx".into()),
                ..Default::default()
            },
            AssetChange {
                id: Some(added.id.clone()),
                secret_refs: Some(vec!["hunter2 password".into()]),
                ..Default::default()
            },
        ] {
            assert!(registry.upsert(change).is_err());
        }
        let log = std::fs::read_to_string(dir.path().join("assets.jsonl")).unwrap();
        assert!(!log.contains("sk-live") && !log.contains("ghp_") && !log.contains("hunter2"));
        assert!(registry.upsert(AssetChange { id: Some("a-none".into()), ..Default::default() }).is_err());
        assert!(
            registry.upsert(AssetChange { name: Some("x".into()), ..Default::default() }).is_err(),
            "a kind is needed"
        );

        assert!(registry.remove(&added.id).unwrap() && !registry.remove(&added.id).unwrap());
        assert!(registry.list().is_empty());
        // Another handle on the same file sees the same records.
        assert!(AssetRegistry::new(dir.path().join("assets.jsonl")).list().is_empty());
    }

    #[test]
    fn what_renews_soon_is_found_soonest_first() {
        let (_dir, registry) = registry();
        let add = |name: &str, at: &str| {
            registry
                .upsert(AssetChange {
                    kind: Some(AssetKind::Subscription),
                    name: Some(name.into()),
                    renews_at: Some(Some(at.into())),
                    ..Default::default()
                })
                .unwrap()
        };
        add("far", "2027-06-01");
        add("soon", "2026-11-25T09:00:00Z");
        add("past", "2026-11-01");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 11, 19).unwrap();
        let due = registry.expiring_within(14, today);
        assert_eq!(
            due.iter().map(|(r, left)| (r.name.as_str(), *left)).collect::<Vec<_>>(),
            [("past", -18), ("soon", 6)]
        );
        assert!(line(&due[1].0).contains("renews 2026-11-25"));
        assert!(
            registry
                .upsert(AssetChange {
                    kind: Some(AssetKind::Domain),
                    name: Some("x".into()),
                    renews_at: Some(Some("next spring".into())),
                    ..Default::default()
                })
                .is_err()
        );
    }
}
