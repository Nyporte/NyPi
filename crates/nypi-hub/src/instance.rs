//! What this machine is (decision D93): `~/.nypi/instance.json` says whether it is the hub or a
//! node of one, which hub it trusts (the fingerprint of the hub's key, which stays the same when
//! the hub moves), the hub's term it knows of (`epoch`), the trust it was admitted with, and the
//! hub's direct addresses last heard. Only NyPi's own commands write it; no tool can (it is in
//! the NyPi home, which `Protected` keeps from every tool). The supervisor reads it every round
//! to know what to keep running.
//!
//! A home set up before D93 is moved to this file once, when it is first read: a device paired
//! with a hub (`devices/hub.json`) becomes a node of that hub, and a machine with a hub key that
//! ran a daemon becomes the hub.

use nypi_core::config::{Paths, Settings};
use nypi_node::identity::{Identity, write_private};
use serde::{Deserialize, Serialize};

use crate::pairing::{self, Trust};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    Hub,
    #[default]
    Node,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Hub => "hub",
            Mode::Node => "node",
        }
    }
}

/// `instance.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Instance {
    pub mode: Mode,
    /// What the hub calls this machine.
    pub name: String,
    /// Tells this machine apart from another of the same name; made once.
    pub instance_id: String,
    /// The hub's term this machine knows of (decision D95).
    pub epoch: u64,
    /// The hub key this machine trusts.
    pub hub_fingerprint: String,
    /// As what this machine was admitted; the hub's own is `personal`.
    pub trust: Trust,
    /// The hub's direct addresses last heard (decision D94).
    pub direct: Vec<String>,
    /// The service is stopped (`nypi hub down`, `nypi node down`); its data is kept.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub down: bool,
}

impl Instance {
    /// This machine's instance, moving a home set up before D93 to it first. None for a machine
    /// that is neither (it has not run `nypi hub up` or `nypi node up`).
    pub fn load(paths: &Paths) -> Option<Instance> {
        if let Some(instance) = Self::read(paths) {
            return Some(instance);
        }
        migrate(paths)
    }

    fn read(paths: &Paths) -> Option<Instance> {
        serde_json::from_str(&std::fs::read_to_string(paths.instance()).ok()?).ok()
    }

    pub fn save(&self, paths: &Paths) -> std::io::Result<()> {
        if let Some(dir) = paths.instance().parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_private(&paths.instance(), &serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)
    }

    pub fn is_hub(&self) -> bool {
        self.mode == Mode::Hub
    }

    /// The hub's own instance, made for `nypi hub up`: its key's fingerprint, epoch 1.
    pub fn new_hub(paths: &Paths, name: &str) -> std::io::Result<Instance> {
        let key = Identity::load_or_create(&paths.hub_key())?;
        Ok(Instance {
            mode: Mode::Hub,
            name: name.to_string(),
            instance_id: new_id(),
            epoch: 1,
            hub_fingerprint: key.fingerprint(),
            trust: Trust::Personal,
            direct: Vec::new(),
            down: false,
        })
    }
}

/// The ways to the hub in order, without looking for the best one (reach.rs does): the hub's
/// direct addresses last heard, then the entry when there is one.
pub fn ways(instance: &Instance, settings: &Settings) -> Vec<crate::pairing::Way> {
    let mut ways: Vec<crate::pairing::Way> = instance.direct.iter().cloned().map(crate::pairing::Way::Direct).collect();
    ways.extend(settings.entry().map(crate::pairing::Way::Relayed));
    ways
}

/// A random id, made once per machine.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// This machine's name as the hub first hears it: its host name, plainly.
pub fn machine_name() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: gethostname writes at most `len` bytes into the buffer it is given.
    let named = unsafe { libc::gethostname(buffer.as_mut_ptr() as *mut libc::c_char, buffer.len()) } == 0;
    let raw = if named {
        let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..end]).into_owned()
    } else {
        String::new()
    };
    let plain = plain_name(raw.split('.').next().unwrap_or_default());
    if plain.is_empty() { "machine".into() } else { plain }
}

/// A name as the register keeps it: letters, digits, `-`, `_`, `.`.
pub fn plain_name(text: &str) -> String {
    text.trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Move a home set up before D93 to `instance.json`, once. The old files go only after the new
/// one is written.
fn migrate(paths: &Paths) -> Option<Instance> {
    pairing::migrate_peers(paths);
    // A device paired with a hub (D80): a node of that hub, `personal` when it was also paired
    // as an interface (it spoke as the user), `exec-only` when it only carried out calls.
    if let Ok(text) = std::fs::read_to_string(paths.paired_hub()) {
        let old: serde_json::Value = serde_json::from_str(&text).ok()?;
        let hub = if old["node"].is_object() { &old["node"] } else { &old["interface"] };
        let fingerprint = hub["fingerprint"].as_str()?.to_ascii_lowercase();
        let trust = if old["interface"].is_object() { Trust::Personal } else { Trust::ExecOnly };
        let instance = Instance {
            mode: Mode::Node,
            name: hub["name"].as_str().map(plain_name).unwrap_or_else(machine_name),
            instance_id: new_id(),
            epoch: 1,
            hub_fingerprint: fingerprint,
            trust,
            direct: hub["address"].as_str().map(|address| vec![address.to_string()]).unwrap_or_default(),
            down: false,
        };
        // The interface key is forgotten: one key per machine from now on.
        if !paths.device_key().exists() && paths.interface_key().exists() {
            let _ = std::fs::rename(paths.interface_key(), paths.device_key());
        }
        instance.save(paths).ok()?;
        let _ = std::fs::remove_file(paths.paired_hub());
        let _ = std::fs::remove_file(paths.interface_key());
        return Some(instance);
    }
    // A machine that was the hub: it has the hub's key, and either took devices on `hub.listen`
    // or ran the daemon.
    if paths.hub_key().exists() {
        // D80 set `hub.listen` by hand; it has a default now, so the file says whether it was.
        let written: serde_json::Value = std::fs::read_to_string(paths.settings())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let listened = written["hub"]["listen"].is_string();
        let ran_daemon = crate::daemon::heartbeat(paths).is_some();
        if listened || ran_daemon {
            let instance = Instance::new_hub(paths, &machine_name()).ok()?;
            instance.save(paths).ok()?;
            return Some(instance);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A D80 home (decision D93): a device paired as a node and as an interface, under one name,
    /// with two keys, becomes one node of that hub, `personal`, with one key; the hub's register
    /// merges the two peers into one.
    #[test]
    fn a_d80_device_becomes_a_personal_node_with_one_key() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let node_key = Identity::load_or_create(&paths.device_key()).unwrap().fingerprint();
        Identity::load_or_create(&paths.interface_key()).unwrap();
        std::fs::create_dir_all(paths.paired_hub().parent().unwrap()).unwrap();
        let hub = "ab".repeat(32);
        std::fs::write(
            paths.paired_hub(),
            serde_json::json!({
                "node": { "address": "100.64.0.7:7681", "fingerprint": hub, "name": "laptop" },
                "interface": { "address": "100.64.0.7:7681", "fingerprint": hub, "name": "laptop" },
            })
            .to_string(),
        )
        .unwrap();
        let instance = Instance::load(&paths).unwrap();
        assert_eq!(
            (instance.mode, instance.name.as_str(), instance.trust, instance.epoch),
            (Mode::Node, "laptop", Trust::Personal, 1)
        );
        assert_eq!(instance.hub_fingerprint, hub);
        assert_eq!(instance.direct, ["100.64.0.7:7681"]);
        assert!(!paths.paired_hub().exists() && !paths.interface_key().exists());
        assert_eq!(Identity::load_or_create(&paths.device_key()).unwrap().fingerprint(), node_key, "the node's key");
        assert_eq!(Instance::load(&paths), Some(instance), "once, then read as it is");
    }

    #[test]
    fn a_home_that_is_neither_is_nothing_yet() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(Instance::load(&Paths { home: home.path().to_path_buf() }), None);
    }
}
