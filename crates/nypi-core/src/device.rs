//! This computer as a node that is a process of its own (`nypi node`, docs/devices.md): what it
//! offers a hub on another machine, and its own last word on each call. It offers the tools of
//! `tools/` (files, the shell), made for the folder each call names, under this machine's own
//! sandbox and protected paths, read from this machine's own settings. Its gate is this
//! machine's guard: what may never be touched here is decided here, whatever the hub asked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nypi_agent::Tool;
use nypi_node::proto::{Call, CallResult, ToolDeclaration};
use nypi_node::{CallContext, Gate, Node, NodeTool};
use serde_json::Value;

use crate::config::{Paths, Settings};
use crate::inspector::Guard;
use crate::place::{DEVICE_TOOLS, OnNode};
use crate::protected::Protected;
use crate::sandbox::Sandbox;

/// What every call's tool is made from: this machine's rules.
struct Rules {
    paths: Paths,
    sandbox: Sandbox,
    protected: Arc<Protected>,
}

impl Rules {
    /// The device tool `name`, working in `folder`.
    fn tool(&self, name: &str, folder: &Path) -> Option<Arc<dyn Tool>> {
        // Lucia's own tools are not a device's: their files are named, never used.
        let unused = self.paths.home.join("unused");
        let sandbox = self.sandbox.in_folder(folder);
        crate::tools::create_tools(&[name.to_string()], folder, &unused, &unused, &sandbox, &self.protected).pop()
    }
}

/// A device tool that works in whichever folder the call names.
struct InFolder {
    name: &'static str,
    rules: Arc<Rules>,
    /// The tool as made for the user's home: what it declares does not depend on the folder.
    shape: Arc<dyn Tool>,
}

#[async_trait]
impl NodeTool for InFolder {
    fn declaration(&self) -> ToolDeclaration {
        OnNode(self.shape.clone()).declaration()
    }

    fn prepare(&self, arguments: Value) -> Value {
        self.shape.prepare_arguments(arguments)
    }

    async fn run(&self, arguments: Value, call: CallContext) -> CallResult {
        match self.rules.tool(self.name, Path::new(&call.folder)) {
            Some(tool) => OnNode(tool).run(arguments, call).await,
            None => CallResult::error(format!("{} is not offered here", self.name)),
        }
    }
}

/// This computer's node, named `name` (the machine's own name without one). Settings that do
/// not read are an error, not the defaults: they name the env files whose keys it protects.
pub fn this_computer(paths: &Paths, name: Option<&str>) -> anyhow::Result<Node> {
    // The user's own settings on this machine, never a project's.
    let settings = Settings::load(paths, &paths.home)
        .map_err(|error| anyhow::anyhow!("{} does not read: {error:#}", paths.settings().display()))?;
    let protected = Arc::new(Protected::new(paths, &settings.env_files));
    let (sandbox, _) = Sandbox::detect(&settings.sandbox, &protected);
    let rules = Arc::new(Rules { paths: paths.clone(), sandbox, protected });
    let mut node = match name {
        Some(name) => Node::this_computer().named(name),
        None => Node::this_computer(),
    };
    let home = dirs::home_dir().unwrap_or_else(|| paths.home.clone());
    for name in DEVICE_TOOLS {
        if let Some(shape) = rules.tool(name, &home) {
            node.register(Arc::new(InFolder { name, rules: rules.clone(), shape }));
        }
    }
    Ok(node.with_gate(gate(rules)).with_context(context_reader(paths.clone())))
}

/// The longest context file a device hands over; a longer one is cut, and says so.
const CONTEXT_FILE_BYTES: usize = 64 * 1024;

/// What a folder on this machine says about how to work in it, for a hub that asks (decision
/// D80): the context files on the way down to it, always; the folder's own settings, skills,
/// and specialists only where this machine's owner trusted the folder (decision D61). This
/// machine's own NyPi home is not a folder's context, and nothing of it is given.
fn context_reader(paths: Paths) -> nypi_node::ContextReader {
    Arc::new(move |folder: &str| {
        let folder = Path::new(folder);
        if !folder.is_dir() {
            return Default::default();
        }
        let home = crate::protected::real_path(&paths.home);
        let context_files = crate::prompt::load_context_files(&paths, folder)
            .into_iter()
            .filter(|file| !crate::protected::real_path(&file.path).starts_with(&home))
            .map(|file| {
                let mut content = file.content;
                if content.len() > CONTEXT_FILE_BYTES {
                    let mut cut = CONTEXT_FILE_BYTES;
                    while !content.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    content.truncate(cut);
                    content.push_str("\n[cut here: the file is longer than a device hands over]");
                }
                nypi_node::proto::ContextText { path: file.path.display().to_string(), content }
            })
            .collect();
        let own = folder.join(".nypi");
        let trusted =
            own.is_dir() && crate::trust::TrustStore::new(&paths).decision(folder).is_some_and(|(_, yes)| yes);
        if !trusted {
            return nypi_node::proto::PlaceContext { context_files, ..Default::default() };
        }
        let trusted_settings =
            std::fs::read_to_string(own.join("settings.json")).ok().and_then(|text| serde_json::from_str(&text).ok());
        let skills = crate::skills::load_skills(&[own.join("skills")])
            .into_iter()
            .map(|skill| nypi_node::proto::SkillNote {
                name: skill.name,
                description: skill.description,
                path: skill.file_path.display().to_string(),
            })
            .collect();
        let mut specialist_profiles: Vec<nypi_node::proto::SpecialistNote> = std::fs::read_dir(own.join("specialists"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| crate::specialist::load_one(&entry.path()).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
            .into_iter()
            .map(|specialist: crate::specialist::Specialist| nypi_node::proto::SpecialistNote {
                kind: match specialist.profile.kind {
                    crate::specialist::SpecialistKind::Native => "native".into(),
                    crate::specialist::SpecialistKind::External => "external".into(),
                },
                description: specialist.profile.description,
                model: specialist.profile.model,
                thinking: specialist.profile.thinking,
                tools: specialist.profile.tools,
                instructions: specialist.instructions,
                name: specialist.name,
            })
            .collect();
        specialist_profiles.sort_by(|a, b| a.name.cmp(&b.name));
        let specialists = specialist_profiles.iter().map(|note| note.name.clone()).collect();
        nypi_node::proto::PlaceContext { context_files, trusted_settings, skills, specialists, specialist_profiles }
    })
}

/// The device's own refusals: a folder that is not there, and whatever this machine's guard
/// never allows (its protected files, catastrophic commands).
fn gate(rules: Arc<Rules>) -> Gate {
    let guards: Mutex<HashMap<PathBuf, Arc<Guard>>> = Mutex::default();
    Arc::new(move |call: &Call| {
        let folder = PathBuf::from(&call.folder);
        if !folder.is_dir() {
            return Some(format!("{} is not a folder on this device", call.folder));
        }
        let guard = guards
            .lock()
            .unwrap()
            .entry(folder.clone())
            .or_insert_with(|| {
                let protected = (*rules.protected).clone();
                Arc::new(Guard::new(&rules.paths, &folder, protected, rules.sandbox.judges_every_path()))
            })
            .clone();
        guard.check(&call.tool, &call.arguments)
    })
}

#[cfg(test)]
mod tests {
    use nypi_node::proto::{Block, FromNode};
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;

    async fn call(node: &Node, id: &str, tool: &str, arguments: Value, folder: &Path) -> FromNode {
        let (events, mut heard) = mpsc::unbounded_channel();
        let call =
            Call { id: id.into(), task: None, tool: tool.into(), arguments, folder: folder.display().to_string() };
        node.call(call, events).await;
        std::iter::from_fn(|| heard.try_recv().ok()).last().expect("an answer")
    }

    fn text(event: &FromNode) -> String {
        match event {
            FromNode::Finished { result, .. } => result
                .content
                .iter()
                .map(|block| match block {
                    Block::Text { text } => text.as_str(),
                    Block::Image { .. } => "",
                })
                .collect(),
            other => panic!("the call did not finish: {other:?}"),
        }
    }

    #[tokio::test]
    async fn this_computer_works_in_the_folder_a_call_names_and_keeps_its_own_rules() {
        let (home, site, api) =
            (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        std::fs::write(home.path().join("settings.json"), r#"{ "sandbox": "off" }"#).unwrap();
        std::fs::write(site.path().join("notes.txt"), "from the site").unwrap();
        std::fs::write(api.path().join("notes.txt"), "from the api").unwrap();
        let node = this_computer(&paths, Some("laptop")).unwrap();
        let report = node.report();
        assert_eq!(report.identity.name, "laptop");
        let offered: Vec<&str> = report.tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(offered, DEVICE_TOOLS, "the device's tools, and none of Lucia's own");

        // The same tool, in the folder each call names.
        let read = json!({ "path": "notes.txt" });
        assert!(text(&call(&node, "c-1", "read", read.clone(), site.path()).await).contains("from the site"));
        assert!(text(&call(&node, "c-2", "read", read.clone(), api.path()).await).contains("from the api"));

        // An edit in the legacy shape is put right on the node, as the tool would in-process.
        let legacy = json!({ "path": "notes.txt", "oldText": "from the site", "newText": "edited on the site" });
        assert!(text(&call(&node, "c-5", "edit", legacy, site.path()).await).contains("Successfully replaced 1 block"));
        assert_eq!(std::fs::read_to_string(site.path().join("notes.txt")).unwrap(), "edited on the site");

        // The device's own last word: a folder that is not here, and its protected files.
        let missing = call(&node, "c-3", "read", read, &site.path().join("gone")).await;
        assert!(matches!(&missing, FromNode::Refused { reason, .. } if reason.contains("not a folder on this device")));
        std::fs::write(home.path().join("auth.json"), "{}").unwrap();
        let secret = json!({ "path": home.path().join("auth.json") });
        let refused = call(&node, "c-4", "read", secret, site.path()).await;
        assert!(matches!(refused, FromNode::Refused { .. }), "the key file is this device's to protect: {refused:?}");
    }

    #[test]
    fn a_folders_context_is_given_and_its_settings_only_where_it_is_trusted_here() {
        let (home, site) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        std::fs::write(home.path().join("settings.json"), r#"{ "sandbox": "off" }"#).unwrap();
        std::fs::write(home.path().join("AGENTS.md"), "This machine's own notes.").unwrap();
        std::fs::write(site.path().join("AGENTS.md"), "Deploys go through make.").unwrap();
        std::fs::create_dir_all(site.path().join(".nypi/specialists/builder")).unwrap();
        std::fs::write(site.path().join(".nypi/settings.json"), r#"{ "approval": { "mode": "off" } }"#).unwrap();
        std::fs::write(site.path().join(".nypi/specialists/builder/profile.json"), "{}").unwrap();
        let node = this_computer(&paths, Some("laptop")).unwrap();
        let folder = site.path().display().to_string();

        // The context files of the folder, never this machine's own; nothing of its settings
        // while nobody here trusted it.
        let context = node.place_context(&folder);
        let texts: Vec<&str> = context.context_files.iter().map(|file| file.content.as_str()).collect();
        assert!(
            texts.contains(&"Deploys go through make.") && !texts.contains(&"This machine's own notes."),
            "{texts:?}"
        );
        assert_eq!((context.trusted_settings.clone(), context.specialists.len()), (None, 0), "{context:?}");

        crate::trust::TrustStore::new(&paths).set(site.path(), Some(true)).unwrap();
        let context = node.place_context(&folder);
        assert_eq!(context.trusted_settings, Some(json!({ "approval": { "mode": "off" } })));
        assert_eq!(context.specialists, ["builder"]);
        // A folder that is not there says nothing.
        assert_eq!(node.place_context("/no/such/folder"), Default::default());
    }
}
