//! A routed place (decision D79): one runtime works where each run's focus is (another folder of
//! this computer, a folder on another device), a single call can name a second device, and
//! `on_device` reaches a tool only one device has. Whatever goes elsewhere is judged for where
//! it goes: the target folder's own `.nypi` is as closed as this one's, and each device keeps
//! its last word. Driven by a scripted model; the "devices" are nodes in this process.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
use nypi_ai::{Message, text_of};
use nypi_core::config::Paths;
use nypi_core::place::{DeviceInfo, DeviceLookup, Focus};
use nypi_core::runtime::{RunOutcome, Runtime, RuntimeOptions, SessionMode};
use nypi_node::proto::{CallResult, ToolDeclaration};
use nypi_node::{CallContext, Node, NodeLink, NodeTool};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// What only the phone has.
struct Screen;

#[async_trait]
impl NodeTool for Screen {
    fn declaration(&self) -> ToolDeclaration {
        ToolDeclaration {
            name: "screen".into(),
            description: "Say what is on the screen".into(),
            parameters: json!({ "type": "object", "properties": { "app": { "type": "string" } } }),
            sequential: true,
        }
    }

    async fn run(&self, arguments: Value, call: CallContext) -> CallResult {
        CallResult::text(format!(
            "{} is open (asked in {})",
            arguments["app"].as_str().unwrap_or("nothing"),
            call.folder
        ))
    }
}

/// The hub's register, as a runtime sees it: a computer and a phone, and one that is away.
struct Devices {
    nodes: Vec<Arc<dyn NodeLink>>,
    away: Mutex<Vec<String>>,
}

impl DeviceLookup for Devices {
    fn devices(&self) -> Vec<DeviceInfo> {
        let away = self.away.lock().unwrap();
        self.nodes
            .iter()
            .map(|node| {
                let report = node.report();
                DeviceInfo {
                    connected: !away.contains(&report.identity.name),
                    name: report.identity.name,
                    kind: format!("{:?}", report.identity.kind).to_lowercase(),
                    system: report.identity.system,
                    home: report.identity.home,
                    tools: report.tools,
                }
            })
            .collect()
    }

    fn node(&self, device: &str) -> Result<Arc<dyn NodeLink>, String> {
        if self.away.lock().unwrap().iter().any(|name| name == device) {
            return Err(format!("{device} is not connected"));
        }
        self.nodes
            .iter()
            .find(|node| node.report().identity.name == device)
            .cloned()
            .ok_or_else(|| format!("no device named {device} is known"))
    }
}

fn results(outcome: &RunOutcome) -> Vec<(String, bool)> {
    outcome
        .new_messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some((text_of(&result.content), result.is_error)),
            _ => None,
        })
        .collect()
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::create_dir_all(dir.join(name).parent().unwrap()).unwrap();
    std::fs::write(dir.join(name), text).unwrap();
}

#[tokio::test]
async fn one_runtime_works_where_its_focus_is_and_reaches_other_devices_by_name() {
    let (home, own, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (mini_home, site) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for dir in [home.path(), mini_home.path()] {
        write(dir, "settings.json", r#"{ "sandbox": "off" }"#);
    }
    write(own.path(), "notes.txt", "from the hub's own folder");
    write(other.path(), "notes.txt", "from the other folder");
    write(other.path(), "AGENTS.md", "In this project, tabs are four spaces.");
    write(site.path(), "notes.txt", "from the site on mini");
    write(site.path(), "AGENTS.md", "On mini, deploys go through make.");
    write(own.path(), "AGENTS.md", "In the hub's own folder, nothing is deployed.");
    write(mini_home.path(), "auth.json", "{}");

    // Another computer, with its own NyPi home and rules, and a phone with a tool of its own.
    let mini = nypi_core::device::this_computer(&Paths { home: mini_home.path().to_path_buf() }, Some("mini")).unwrap();
    let mut pixel = Node::this_computer().named("pixel");
    pixel.register(Arc::new(Screen));
    let devices = Arc::new(Devices { nodes: vec![Arc::new(mini), Arc::new(pixel)], away: Mutex::default() });

    let read = |n: usize| FauxResponse::tool_call(&format!("r{n}"), "read", json!({ "path": "notes.txt" }));
    let site_notes = site.path().join("notes.txt").display().to_string();
    // NyPi's own settings, as a full path and as a path relative to the focus folder (the
    // temporary folders are siblings).
    let own_settings = home.path().join("settings.json").display().to_string();
    let from_other = format!("../{}/settings.json", home.path().file_name().unwrap().to_string_lossy());
    let mini_keys = mini_home.path().join("auth.json").display().to_string();
    let faux = FauxProvider::new(vec![
        // 1. No focus: the runtime's own folder.
        read(1),
        FauxResponse::text("Done: read it."),
        // 2. Focus on another folder of this computer; NyPi's own home is as closed from there.
        read(2),
        FauxResponse::tool_call("w2", "write", json!({ "path": from_other, "content": "{}" })),
        FauxResponse::tool_call("w3", "write", json!({ "path": own_settings, "content": "{}" })),
        FauxResponse::text("Done: read it there."),
        // 3. Focus on a folder of another device; that device keeps its own key file closed.
        read(3),
        FauxResponse::tool_call("k3", "read", json!({ "path": mini_keys })),
        FauxResponse::text("Done: read it on mini."),
        // 4. Back at its own folder, one call names the device, and one a tool only the phone has.
        FauxResponse::tool_call("r4", "read", json!({ "path": site_notes, "device": "mini" })),
        FauxResponse::tool_call(
            "s4",
            "on_device",
            json!({ "device": "pixel", "tool": "screen", "arguments": { "app": "Maps" }, "folder": "/sdcard" }),
        ),
        FauxResponse::tool_call("x4", "on_device", json!({ "device": "pixel", "tool": "bash", "arguments": {} })),
        FauxResponse::tool_call("a4", "read", json!({ "path": "/tmp/x", "device": "mini" })),
        FauxResponse::text("Done: used both devices."),
    ]);
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: own.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        devices: Some(devices.clone()),
        ..Default::default()
    })
    .unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();

    // The device tools take a device, and what only a device offers is in the prompt.
    let read_tool = runtime.agent.config.tools.iter().find(|tool| tool.definition().name == "read").unwrap();
    assert!(read_tool.definition().parameters["properties"]["device"].is_object());
    assert!(runtime.agent.config.tools.iter().any(|tool| tool.definition().name == "on_device"));

    // 1. Its own place.
    let outcome = runtime.prompt("read the notes", &tx).await.unwrap();
    assert!(results(&outcome)[0].0.contains("from the hub's own folder"), "{:?}", results(&outcome));
    let prompt = runtime.agent.config.system_prompt.clone();
    assert!(prompt.contains("<devices>") && prompt.contains("- mini (computer"), "{prompt}");
    assert!(prompt.contains("- screen: Say what is on the screen"), "what only the phone has is listed: {prompt}");
    assert!(!prompt.contains("- read:  Takes"), "the usual tools are not repeated per device");

    // 2. Another folder of this computer: its files, its context file, and its own rules.
    runtime.set_focus(Some(Focus::here(other.path())));
    let outcome = runtime.prompt("read the notes here", &tx).await.unwrap();
    let got = results(&outcome);
    assert!(got[0].0.contains("from the other folder"), "{got:?}");
    assert!(
        got[1].1 && got[2].1,
        "NyPi's home stays closed, by a path from the focus folder and by a full one: {got:?}"
    );
    assert_eq!(std::fs::read_to_string(home.path().join("settings.json")).unwrap(), r#"{ "sandbox": "off" }"#);
    let prompt = runtime.agent.config.system_prompt.clone();
    assert!(prompt.contains("tabs are four spaces"), "the focus folder's context file is read");
    assert!(prompt.contains(&other.path().display().to_string()), "the prompt names where the run works");
    let task = runtime.ledger().get(outcome.task.as_ref().unwrap()).unwrap();
    assert_eq!((task.cwd.as_path(), &task.remote), (other.path(), &None), "the task records where it worked");

    // 3. A folder on another device.
    let on_mini = Focus { device: Some("mini".into()), folder: site.path().display().to_string() };
    runtime.set_focus(Some(on_mini.clone()));
    let outcome = runtime.prompt("read the notes on mini", &tx).await.unwrap();
    let got = results(&outcome);
    assert!(got[0].0.contains("from the site on mini"), "{got:?}");
    assert!(got[1].1 && got[1].0.contains("not carried out on mini"), "the device's own last word: {got:?}");
    let prompt = runtime.agent.config.system_prompt.clone();
    assert!(prompt.contains("mini"), "the prompt names the device");
    // The folder's context file is the device's, read there; the hub folder's is not this run's.
    assert!(prompt.contains("On mini, deploys go through make."), "{prompt}");
    assert!(!prompt.contains("nothing is deployed"), "{prompt}");
    let task = runtime.ledger().get(outcome.task.as_ref().unwrap()).unwrap();
    assert_eq!(task.remote, on_mini.remote(), "a resumed task goes back to the device");

    // 4. Back home; single calls go out by name.
    runtime.set_focus(None);
    devices.away.lock().unwrap().push("nobody".into());
    let outcome = runtime.prompt("use the devices", &tx).await.unwrap();
    let got = results(&outcome);
    assert!(got[0].0.contains("from the site on mini") && !got[0].1, "{got:?}");
    assert!(got[1].0.contains("Maps is open (asked in /sdcard)"), "{got:?}");
    assert!(got[2].1 && got[2].0.contains("pixel offers no tool named bash"), "{got:?}");
    assert!(!got[3].0.is_empty());

    // A device that is away is a result the model reads, not a failure of the run.
    devices.away.lock().unwrap().push("mini".into());
    faux.push(FauxResponse::tool_call("g5", "read", json!({ "path": "/tmp/x", "device": "mini" })));
    faux.push(FauxResponse::text("Blocked: mini is away."));
    let outcome = runtime.prompt("try mini", &tx).await.unwrap();
    let got = results(&outcome);
    assert!(got[0].1 && got[0].0.contains("mini is not connected"), "{got:?}");
    assert!(runtime.agent.config.system_prompt.contains("- mini (computer"));
    assert!(runtime.agent.config.system_prompt.contains("not connected"));
}

#[tokio::test]
async fn a_runtime_without_the_hubs_devices_is_as_before() {
    let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let faux = FauxProvider::new(vec![]);
    let runtime = Runtime::new(RuntimeOptions {
        cwd: work.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        ..Default::default()
    })
    .unwrap();
    let read = runtime.agent.config.tools.iter().find(|tool| tool.definition().name == "read").unwrap();
    assert!(read.definition().parameters["properties"]["device"].is_null(), "no device to name");
    assert!(!runtime.agent.config.tools.iter().any(|tool| tool.definition().name == "on_device"));
    // A focus is nothing to a place that is not routed.
    runtime.set_focus(Some(Focus::here(Path::new("/somewhere/else"))));
    assert_eq!(runtime.focus(), None);
    assert_eq!(runtime.working_folder(), work.path());
}

/// With the chat module in front, a message says where its sender is: chat reads there itself,
/// and the task the Principal starts for it works there without naming the folder (D79, D84).
#[tokio::test]
async fn a_task_started_through_chat_works_where_the_message_came_from() {
    use nypi_core::chat::{self, ChatOptions, ModuleEvent};
    use nypi_core::worker::WorkerEvent;
    let (home, own, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write(home.path(), "settings.json", r#"{ "sandbox": "off" }"#);
    write(own.path(), "notes.txt", "from the hub's own folder");
    write(other.path(), "notes.txt", "from where the owner is");
    let devices = Arc::new(Devices { nodes: Vec::new(), away: Mutex::default() });
    let worker_model = FauxProvider::new(vec![
        FauxResponse::tool_call("r1", "read", json!({ "path": "notes.txt" })),
        FauxResponse::text("Done: read the notes."),
    ]);
    let chat_model = FauxProvider::new(vec![
        FauxResponse::tool_call("c0", "read", json!({ "path": "notes.txt" })),
        FauxResponse::tool_call("c1", "signal_principal", json!({ "topic": "Summarize what notes.txt holds" })),
        FauxResponse::text("They say where you are; I'll have them summarized."),
    ]);
    let principal_model = FauxProvider::new(vec![
        FauxResponse::tool_call("p1", "start_task", json!({ "instruction": "Read notes.txt and say what it holds." })),
        FauxResponse::text("Started a task to read the notes."),
    ]);
    let runtime = Runtime::new(RuntimeOptions {
        cwd: own.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(worker_model.stream_fn()),
        session: SessionMode::Ephemeral,
        devices: Some(devices),
        ..Default::default()
    })
    .unwrap();
    let options = ChatOptions {
        model: Some("faux/faux-1".into()),
        stream_fn: Some(chat_model.stream_fn()),
        principal_stream_fn: Some(principal_model.stream_fn()),
    };
    let (chat, mut events) = chat::start(runtime, options).unwrap();
    chat.say_from("What do the notes say?", Some(Focus::here(other.path())));
    let outcome = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(20), events.recv()).await.unwrap().unwrap();
        if let ModuleEvent::Worker(WorkerEvent::Settled { outcome, .. }) = event {
            break outcome.unwrap();
        }
    };
    assert!(results(&outcome)[0].0.contains("from where the owner is"), "{:?}", results(&outcome));
    // Chat read the file where the owner is, itself.
    let read = format!("{:?}", chat_model.requests()[1].messages.last().unwrap());
    assert!(read.contains("from where the owner is"), "{read}");
    // Both are told where the owner is, so the Principal can send a task elsewhere on purpose.
    let asked = &chat_model.requests()[0].system_prompt;
    assert!(asked.contains("## Where") && asked.contains(&other.path().display().to_string()), "{asked}");
    // The Principal's is in its activation's situational radar: its system prompt stays the same.
    let decided = format!("{:?}", principal_model.requests()[0].messages[0]);
    assert!(decided.contains("## Where") && decided.contains(&other.path().display().to_string()), "{decided}");
    assert!(!principal_model.requests()[0].system_prompt.contains("## Where"));
}

/// A trusted folder on a device brings its own skills and native specialists to the runs that
/// work there (decision D83): the skill is read at the device, the specialist works there
/// through the same tools, and an external one's program is never run for it. Back at a
/// folder of the hub, they are gone again.
#[tokio::test]
async fn a_trusted_folder_on_a_device_lends_its_skills_and_native_specialists_to_runs_there() {
    let (home, own, mini_home, site) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    for dir in [home.path(), mini_home.path()] {
        write(dir, "settings.json", r#"{ "sandbox": "off" }"#);
    }
    write(site.path(), "notes.txt", "from the site on mini");
    write(
        site.path(),
        ".nypi/skills/deploy/SKILL.md",
        "---\nname: deploy\ndescription: How this site is deployed.\n---\nmake deploy",
    );
    write(
        site.path(),
        ".nypi/specialists/builder/profile.json",
        r#"{ "kind": "native", "description": "Builds this site.", "model": "faux/faux-1", "tools": ["read"] }"#,
    );
    write(site.path(), ".nypi/specialists/builder/instructions.md", "Build carefully.");
    write(
        site.path(),
        ".nypi/specialists/shell/profile.json",
        r#"{ "kind": "external", "description": "Runs a script.", "command": ["sh", "-c", "touch ran"] }"#,
    );
    let mini_paths = Paths { home: mini_home.path().to_path_buf() };
    nypi_core::trust::TrustStore::new(&mini_paths).set(site.path(), Some(true)).unwrap();
    let mini = nypi_core::device::this_computer(&mini_paths, Some("mini")).unwrap();
    let devices = Arc::new(Devices { nodes: vec![Arc::new(mini)], away: Mutex::default() });

    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("d1", "delegate", json!({ "specialist": "builder", "task": "Read notes.txt." })),
        // The specialist, at mini's folder.
        FauxResponse::tool_call("r1", "read", json!({ "path": "notes.txt" })),
        FauxResponse::text("It says: from the site on mini [t1].\n\nDone: read it."),
        FauxResponse::text("Done: the builder read the notes."),
        FauxResponse::tool_call("x1", "delegate", json!({ "specialist": "shell", "task": "Run it." })),
        FauxResponse::text("Done: nothing to run."),
    ]);
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: own.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(faux_model()),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        devices: Some(devices),
        ..Default::default()
    })
    .unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    assert!(!runtime.agent.config.tools.iter().any(|tool| tool.definition().name == "delegate"), "nobody to ask yet");

    runtime.set_focus(Some(Focus { device: Some("mini".into()), folder: site.path().display().to_string() }));
    let outcome = runtime.prompt("have the builder read the notes", &tx).await.unwrap();
    let requests = faux.requests();
    let prompt = &requests[0].system_prompt;
    let skill = site.path().join(".nypi/skills/deploy/SKILL.md").display().to_string();
    assert!(prompt.contains("<name>deploy</name>") && prompt.contains(&skill), "{prompt}");
    assert!(prompt.contains("How this site is deployed. (on mini)"), "{prompt}");
    let delegate = requests[0].tools.iter().find(|tool| tool.name == "delegate").expect("offered at mini's folder");
    assert_eq!(delegate.parameters["properties"]["specialist"]["enum"], json!(["builder"]), "native ones only");
    // The builder, with its instructions from mini, read the file at mini's folder through mini's node.
    assert!(requests[1].system_prompt.contains("Build carefully."), "{}", requests[1].system_prompt);
    let (read, failed) = match requests[2].messages.last() {
        Some(Message::ToolResult(result)) => (text_of(&result.content), result.is_error),
        other => panic!("the specialist's read: {other:?}"),
    };
    assert!(!failed && read.contains("from the site on mini"), "{read}");
    assert_eq!(outcome.status.kind, nypi_core::status::StatusKind::Done);

    // Back at the hub's own folder: neither the skill nor the builder.
    runtime.set_focus(None);
    let _ = runtime.prompt("and now?", &tx).await.unwrap();
    let last = faux.requests().last().unwrap().clone();
    assert!(!last.system_prompt.contains("<name>deploy</name>"));
    assert!(!last.tools.iter().any(|tool| tool.name == "delegate"), "nobody to delegate to here");
    assert!(!site.path().join("ran").exists(), "the device's program never ran");
}

/// A trusted folder on a device sets what shapes a run there, and nothing that weakens the
/// hub (decision D91): its thinking level, retries, and tools to ask approval for take effect
/// while the run works there and are gone when it works elsewhere; its approval mode and env
/// files are never read.
#[tokio::test]
async fn a_trusted_device_folder_sets_safe_settings_for_runs_there_only() {
    let (home, own, mini_home, site) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    for dir in [home.path(), mini_home.path()] {
        write(dir, "settings.json", r#"{ "sandbox": "off", "defaultThinking": "high" }"#);
    }
    write(
        site.path(),
        ".nypi/settings.json",
        r#"{ "defaultThinking": "low", "retry": { "maxRetries": 7 }, "compaction": { "keepRecentTokens": 1234 },
            "approval": { "mode": "off", "tools": ["write"] }, "envFiles": ["/etc/secrets.env"] }"#,
    );
    let mini_paths = Paths { home: mini_home.path().to_path_buf() };
    nypi_core::trust::TrustStore::new(&mini_paths).set(site.path(), Some(true)).unwrap();
    let mini = nypi_core::device::this_computer(&mini_paths, Some("mini")).unwrap();
    let devices = Arc::new(Devices { nodes: vec![Arc::new(mini)], away: Mutex::default() });
    let faux = FauxProvider::new(vec![
        FauxResponse::tool_call("w1", "write", json!({ "path": "out.txt", "content": "x" })),
        FauxResponse::text("Done: tried."),
        FauxResponse::tool_call("w2", "write", json!({ "path": "out.txt", "content": "x" })),
        FauxResponse::text("Done: written."),
    ]);
    let mut model = faux_model();
    model.reasoning = true;
    let mut runtime = Runtime::new(RuntimeOptions {
        cwd: own.path().to_path_buf(),
        paths: Some(Paths { home: home.path().to_path_buf() }),
        model: Some("faux/faux-1".into()),
        extra_model: Some(model),
        stream_fn: Some(faux.stream_fn()),
        session: SessionMode::Ephemeral,
        devices: Some(devices),
        ..Default::default()
    })
    .unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let hub_thinking = runtime.agent.config.thinking;
    assert_eq!(hub_thinking, nypi_ai::ThinkingLevel::High);
    let (hub_retries, hub_keep) = (runtime.settings.retry.max_retries, runtime.settings.compaction.keep_recent_tokens);
    let hub_env_files = runtime.settings.env_files.clone();

    runtime.set_focus(Some(Focus { device: Some("mini".into()), folder: site.path().display().to_string() }));
    let outcome = runtime.prompt("write out.txt", &tx).await.unwrap();
    let (asked, failed) = results(&outcome)[0].clone();
    assert!(failed && asked.contains("needs approval in this folder"), "{asked}");
    assert!(!site.path().join("out.txt").exists(), "nobody approved it");
    assert_eq!(runtime.agent.config.thinking, nypi_ai::ThinkingLevel::Low);
    assert_eq!((runtime.settings.retry.max_retries, runtime.settings.compaction.keep_recent_tokens), (7, 1234));
    assert_eq!(runtime.settings.env_files, hub_env_files, "a device's env files are never taken");
    assert_ne!(runtime.settings.approval.mode, nypi_core::inspector::ApprovalMode::Off, "nor its approval mode");

    // At the hub's own folder: the hub's values, and no added approval.
    runtime.set_focus(None);
    let outcome = runtime.prompt("write out.txt here", &tx).await.unwrap();
    assert!(!results(&outcome)[0].1, "{:?}", results(&outcome));
    assert!(own.path().join("out.txt").exists());
    assert_eq!(runtime.agent.config.thinking, hub_thinking);
    assert_eq!(
        (runtime.settings.retry.max_retries, runtime.settings.compaction.keep_recent_tokens),
        (hub_retries, hub_keep)
    );
}
