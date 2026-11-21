//! The commands a thread of the hub takes, typed once where they arrive (decision D72). An
//! interface sends one JSON object per line (Pi's RPC command records, plus NyPi's own); `parse`
//! turns it into a `HubCommand`, and both conversation loops (`rpc`, worker only, and
//! `chat_loop`, with the chat module in front) match on the same enum, so a command one of them
//! takes cannot silently go missing from the other. The runtime's own commands (models, the
//! session tree, compaction, statistics) are carried out by one function, `RuntimeCommand::run`,
//! on whichever runtime the loop holds: directly between runs, or on the worker once it is idle.
//! JSON stays at the edge: records going out are still built where they are emitted.

use std::path::Path;

use nypi_agent::QueueMode;
use nypi_ai::auth::resolve_model_key;
use nypi_ai::{Message, ThinkingLevel};
use nypi_core::place::Focus;
use nypi_core::runtime::Runtime;
use serde_json::{Value, json};

use crate::rpc::Sourced;

/// How a prompt sent while a run is under way joins it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Behavior {
    /// Into the next turn.
    Steer,
    /// When the run would otherwise end.
    FollowUp,
}

/// One command, as the loops handle it.
#[derive(Debug)]
pub(crate) enum HubCommand {
    /// The owner's words, and where they were when they said them (decision D79).
    Prompt {
        text: String,
        behavior: Option<Behavior>,
        focus: Option<Focus>,
    },
    /// A prompt that is not the owner's words (`"source"`): a fired schedule, a goal review.
    Sourced(Result<Sourced, String>),
    Steer(String),
    FollowUp(String),
    ClearQueue,
    Abort,
    AbortRetry,
    AbortBash,
    /// The user's own shell command (`bash`), shown to the model unless excluded.
    Bash {
        command: Option<String>,
        exclude: bool,
        /// Where its sender is: the command runs there.
        focus: Option<Focus>,
    },
    Answer {
        id: String,
        approved: bool,
        reason: Option<String>,
    },
    /// The draft being typed, for recall.
    Typing(String),
    GetState,
    Runtime(RuntimeCommand),
    /// One of the hub's own commands from a personal machine (decision D96), on a talk link of
    /// its own: `daemon/remote.rs` runs it before anything reaches the conversation, and a loop
    /// that gets one refuses it.
    Admin {
        argv: Vec<String>,
        input: Option<Hidden>,
    },
    Unknown,
}

/// What an admin command reads in place of a terminal (the value of `secrets set`): never shown,
/// not even by `Debug`.
#[derive(Clone)]
pub struct Hidden(pub String);

impl std::fmt::Debug for Hidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hidden(..)")
    }
}

/// The hub's commands a personal machine may have run on the hub (decision D96), by the name
/// after `nypi`. Each runs there with the same code as on the hub, its text sent back.
pub const ADMIN_COMMANDS: &[&str] = &[
    "tasks",
    "goals",
    "memory",
    "portrait",
    "schedule",
    "specialists",
    "reflections",
    "dmn",
    "rate",
    "scorecard",
    "notices",
    "trace",
    "eval",
    "devices",
    "secrets",
    "update",
];

/// What of those starts a run of its own, or works on this machine rather than the stores: not
/// carried.
const NOT_CARRIED: &[(&str, &str)] = &[
    ("goals", "review"),
    ("specialists", "run"),
    ("specialists", "detect"),
    ("schedule", "tick"),
    ("dmn", "run"),
    ("tasks", "resume"),
    ("memory", "model"),
];

/// Whether `argv` (the words after `nypi`) is a command the hub runs for a personal machine,
/// with what it needs: why not, when it is not.
pub fn admin_allowed(argv: &[String], input: bool) -> Result<(), String> {
    let first = argv.first().map(String::as_str).unwrap_or_default();
    if !ADMIN_COMMANDS.contains(&first) {
        return Err(format!("`nypi {first}` is not one of the hub's commands; it runs on the machine it is about"));
    }
    let second = argv.get(1).map(String::as_str).unwrap_or_default();
    if NOT_CARRIED.contains(&(first, second)) {
        return Err(format!("`nypi {first} {second}` runs on the hub itself, not from another machine"));
    }
    if first == "eval" && !argv.iter().any(|word| word == "--list") {
        return Err("a benchmark run uses this machine's model keys: run it here, or on the hub".into());
    }
    if first == "secrets" && second == "set" && !input {
        return Err("`secrets set` sends the value it read on this machine".into());
    }
    // Plain `nypi update` is about the machine it is typed on; the hub updates others (D98).
    if first == "update" && !argv.iter().any(|word| word == "--all" || word == "--node" || word.starts_with("--node="))
    {
        return Err("`nypi update` updates the machine it is typed on; --node <name> or --all go to the hub".into());
    }
    Ok(())
}

/// What the hub gives a command it runs: the register of its nodes, for `update --node` and
/// `update --all` (decision D98).
#[derive(Clone, Default)]
pub struct AdminContext {
    pub nodes: Option<std::sync::Arc<crate::nodes::Nodes>>,
}

/// How the hub runs an admin command: the program registers it at start (it holds the
/// commands' code), with the hub's NyPi home. Its text and exit code.
pub type AdminRunner = std::sync::Arc<
    dyn Fn(
            nypi_core::config::Paths,
            Vec<String>,
            Option<Hidden>,
            AdminContext,
        ) -> futures::future::BoxFuture<'static, (String, i32)>
        + Send
        + Sync,
>;

static ADMIN_RUNNER: std::sync::OnceLock<AdminRunner> = std::sync::OnceLock::new();

/// Register how admin commands run (once, at the program's start).
pub fn set_admin_runner(runner: AdminRunner) {
    let _ = ADMIN_RUNNER.set(runner);
}

/// Run an admin command with the hub's home `paths`, after `admin_allowed`.
pub async fn run_admin(
    paths: &nypi_core::config::Paths,
    argv: Vec<String>,
    input: Option<Hidden>,
    context: AdminContext,
) -> (String, i32) {
    match ADMIN_RUNNER.get() {
        Some(runner) => runner(paths.clone(), argv, input, context).await,
        None => ("this hub cannot run commands for other machines".into(), 1),
    }
}

/// A command the runtime itself carries out, with nothing running.
#[derive(Debug)]
pub(crate) enum RuntimeCommand {
    NewSession,
    SetModel { provider: String, model_id: String },
    GetAvailableModels,
    SetThinkingLevel(Option<ThinkingLevel>),
    GetAvailableThinkingLevels,
    Compact,
    SetAutoRetry(Option<bool>),
    SetAutoCompaction(Option<bool>),
    SetQueueMode { follow_up: bool, mode: Option<QueueMode> },
    CycleModel,
    CycleThinkingLevel,
    GetSessionStats,
    ExportHtml(Option<String>),
    SwitchSession(Option<String>),
    Fork(Option<String>),
    Clone,
    GetForkMessages,
    GetEntries(Option<String>),
    GetTree,
    SetSessionName(String),
    GetMessages,
    GetLastAssistantText,
    GetCommands,
}

impl RuntimeCommand {
    /// Whether it leaves the runtime on another conversation, which a chat module in front must
    /// not go on remembering as the same one.
    pub(crate) fn changes_conversation(&self) -> bool {
        matches!(
            self,
            RuntimeCommand::NewSession
                | RuntimeCommand::SwitchSession(_)
                | RuntimeCommand::Fork(_)
                | RuntimeCommand::Clone
        )
    }

    /// Carry it out; the response's data, or the error the interface is told.
    pub(crate) async fn run(self, runtime: &mut Runtime) -> Result<Option<Value>, String> {
        let failed = |error: anyhow::Error| format!("{error:#}");
        match self {
            RuntimeCommand::NewSession => {
                runtime.new_session().map_err(failed)?;
                Ok(Some(json!({ "cancelled": false })))
            }
            RuntimeCommand::SetModel { provider, model_id } => {
                match runtime.set_model(&format!("{provider}/{model_id}")) {
                    Ok(()) => Ok(Some(json!(runtime.model()))),
                    Err(_) => Err(format!("Model not found: {provider}/{model_id}")),
                }
            }
            RuntimeCommand::GetAvailableModels => {
                let auth = runtime.paths.auth();
                let models: Vec<_> =
                    runtime.registry.all().iter().filter(|m| resolve_model_key(m, &auth).is_some()).collect();
                Ok(Some(json!({ "models": models })))
            }
            RuntimeCommand::SetThinkingLevel(Some(level)) => runtime.set_thinking(level).map(|_| None).map_err(failed),
            RuntimeCommand::SetThinkingLevel(None) => Err("Unknown thinking level".into()),
            RuntimeCommand::GetAvailableThinkingLevels => {
                let levels: Vec<&str> =
                    runtime.model().supported_thinking_levels().iter().map(|l| l.as_str()).collect();
                Ok(Some(json!({ "levels": levels })))
            }
            RuntimeCommand::Compact => {
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                runtime.compact("rpc", &tx).await.map_err(failed)?;
                Ok(Some(json!({ "messageCount": runtime.agent.messages.len() })))
            }
            RuntimeCommand::SetAutoRetry(Some(enabled)) => {
                runtime.set_auto_retry(enabled).map(|()| None).map_err(failed)
            }
            RuntimeCommand::SetAutoCompaction(Some(enabled)) => {
                runtime.set_auto_compaction(enabled).map(|()| None).map_err(failed)
            }
            RuntimeCommand::SetAutoRetry(None) | RuntimeCommand::SetAutoCompaction(None) => {
                Err("enabled must be true or false".into())
            }
            RuntimeCommand::SetQueueMode { follow_up, mode: Some(mode) } => {
                runtime.set_queue_mode(follow_up, mode).map(|()| None).map_err(failed)
            }
            RuntimeCommand::SetQueueMode { mode: None, .. } => Err("mode must be \"all\" or \"one-at-a-time\"".into()),
            RuntimeCommand::CycleModel => {
                let cycled = runtime.cycle_model().map_err(failed)?;
                Ok(Some(cycled.map_or(Value::Null, |(model, level, scoped)| {
                    json!({ "model": model, "thinkingLevel": level.as_str(), "isScoped": scoped })
                })))
            }
            RuntimeCommand::CycleThinkingLevel => {
                let level = runtime.cycle_thinking().map_err(failed)?;
                Ok(Some(level.map_or(Value::Null, |l| json!({ "level": l.as_str() }))))
            }
            RuntimeCommand::GetSessionStats => Ok(Some(json!(runtime.session_stats()))),
            RuntimeCommand::ExportHtml(path) => {
                let path = runtime.export_html(path.as_deref()).map_err(failed)?;
                Ok(Some(json!({ "path": path })))
            }
            RuntimeCommand::SwitchSession(Some(path)) => {
                runtime.switch_session(Path::new(&path)).map_err(failed)?;
                Ok(Some(json!({ "cancelled": false })))
            }
            RuntimeCommand::SwitchSession(None) => Err("sessionPath is required".into()),
            RuntimeCommand::Fork(Some(entry)) => {
                let text = runtime.fork(&entry).map_err(failed)?;
                Ok(Some(json!({ "text": text, "cancelled": false })))
            }
            RuntimeCommand::Fork(None) => Err("entryId is required".into()),
            RuntimeCommand::Clone => {
                runtime.clone_session().map_err(failed)?;
                Ok(Some(json!({ "cancelled": false })))
            }
            RuntimeCommand::GetForkMessages => {
                let messages: Vec<Value> = runtime
                    .fork_messages()
                    .into_iter()
                    .map(|(entry, text)| json!({ "entryId": entry, "text": text }))
                    .collect();
                Ok(Some(json!({ "messages": messages })))
            }
            RuntimeCommand::GetEntries(since) => {
                let (entries, leaf) = runtime.session_entries(since.as_deref()).map_err(failed)?;
                Ok(Some(json!({ "entries": entries, "leafId": leaf })))
            }
            RuntimeCommand::GetTree => {
                let (tree, leaf) = runtime.session_tree();
                // Built by hand: `json!` would copy the tree, recursively.
                let mut data = serde_json::Map::new();
                data.insert("tree".into(), Value::Array(tree));
                data.insert("leafId".into(), json!(leaf));
                Ok(Some(Value::Object(data)))
            }
            RuntimeCommand::SetSessionName(name) => runtime.set_session_name(&name).map(|()| None).map_err(failed),
            RuntimeCommand::GetMessages => Ok(Some(json!({ "messages": runtime.agent.messages }))),
            RuntimeCommand::GetLastAssistantText => {
                let text = runtime.agent.messages.iter().rev().find_map(|m| match m {
                    Message::Assistant(a) if !a.text().trim().is_empty() => Some(a.text()),
                    _ => None,
                });
                Ok(Some(json!({ "text": text })))
            }
            RuntimeCommand::GetCommands => {
                let mut commands: Vec<Value> = runtime
                    .skills
                    .iter()
                    .map(|s| json!({ "name": format!("skill:{}", s.name), "description": s.description, "source": "skill" }))
                    .collect();
                commands.extend(runtime.mcp_prompts().iter().map(|p| {
                    json!({
                        "name": format!("mcp:{}:{}", p.server, p.name),
                        "description": p.description,
                        "source": "mcp",
                        "arguments": p.arguments,
                    })
                }));
                Ok(Some(json!({ "commands": commands })))
            }
        }
    }
}

fn text(command: &Value, key: &str) -> Option<String> {
    command[key].as_str().map(str::to_string)
}

/// Where a command was sent from (`"focus"`, nypi-hub-proto), as the place a run works at.
pub(crate) fn focus_of(command: &Value) -> Option<Focus> {
    let focus: nypi_hub_proto::AmbientFocus = serde_json::from_value(command.get("focus")?.clone()).ok()?;
    Some(Focus { device: focus.device, folder: focus.cwd.display().to_string() })
}

/// The command in a record an interface sent. Its `type` is the name a response repeats.
pub(crate) fn parse(command: &Value) -> HubCommand {
    let kind = command["type"].as_str().unwrap_or_default();
    let message = || text(command, "message").unwrap_or_default();
    match kind {
        "prompt" if command.get("source").is_some() => HubCommand::Sourced(Sourced::read(command)),
        "prompt" => HubCommand::Prompt {
            text: message(),
            behavior: match command["streamingBehavior"].as_str() {
                Some("steer") => Some(Behavior::Steer),
                Some("followUp") => Some(Behavior::FollowUp),
                _ => None,
            },
            focus: focus_of(command),
        },
        "steer" => HubCommand::Steer(message()),
        "follow_up" => HubCommand::FollowUp(message()),
        "clear_queue" => HubCommand::ClearQueue,
        "abort" => HubCommand::Abort,
        "abort_retry" => HubCommand::AbortRetry,
        "abort_bash" => HubCommand::AbortBash,
        "bash" => HubCommand::Bash {
            command: text(command, "command"),
            exclude: command["excludeFromContext"].as_bool().unwrap_or(false),
            focus: focus_of(command),
        },
        "approval_response" => HubCommand::Answer {
            id: text(command, "id").unwrap_or_default(),
            approved: command["approved"].as_bool() == Some(true),
            reason: text(command, "reason"),
        },
        "typing" => HubCommand::Typing(text(command, "text").unwrap_or_default()),
        "get_state" => HubCommand::GetState,
        "admin" => HubCommand::Admin {
            argv: command["argv"]
                .as_array()
                .map(|words| words.iter().filter_map(|word| word.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            input: text(command, "input").map(Hidden),
        },
        _ => match runtime_command(kind, command) {
            Some(command) => HubCommand::Runtime(command),
            None => HubCommand::Unknown,
        },
    }
}

fn runtime_command(kind: &str, command: &Value) -> Option<RuntimeCommand> {
    let enabled = || command["enabled"].as_bool();
    Some(match kind {
        "new_session" => RuntimeCommand::NewSession,
        "set_model" => RuntimeCommand::SetModel {
            provider: text(command, "provider").unwrap_or_default(),
            model_id: text(command, "modelId").unwrap_or_default(),
        },
        "get_available_models" => RuntimeCommand::GetAvailableModels,
        "set_thinking_level" => {
            RuntimeCommand::SetThinkingLevel(command["level"].as_str().and_then(ThinkingLevel::parse))
        }
        "get_available_thinking_levels" => RuntimeCommand::GetAvailableThinkingLevels,
        "compact" => RuntimeCommand::Compact,
        "set_auto_retry" => RuntimeCommand::SetAutoRetry(enabled()),
        "set_auto_compaction" => RuntimeCommand::SetAutoCompaction(enabled()),
        "set_steering_mode" | "set_follow_up_mode" => RuntimeCommand::SetQueueMode {
            follow_up: kind == "set_follow_up_mode",
            mode: serde_json::from_value::<QueueMode>(command["mode"].clone()).ok(),
        },
        "cycle_model" => RuntimeCommand::CycleModel,
        "cycle_thinking_level" => RuntimeCommand::CycleThinkingLevel,
        "get_session_stats" => RuntimeCommand::GetSessionStats,
        "export_html" => RuntimeCommand::ExportHtml(text(command, "outputPath")),
        "switch_session" => RuntimeCommand::SwitchSession(text(command, "sessionPath")),
        "fork" => RuntimeCommand::Fork(text(command, "entryId")),
        "clone" => RuntimeCommand::Clone,
        "get_fork_messages" => RuntimeCommand::GetForkMessages,
        "get_entries" => RuntimeCommand::GetEntries(text(command, "since")),
        "get_tree" => RuntimeCommand::GetTree,
        "set_session_name" => RuntimeCommand::SetSessionName(text(command, "name").unwrap_or_default()),
        "get_messages" => RuntimeCommand::GetMessages,
        "get_last_assistant_text" => RuntimeCommand::GetLastAssistantText,
        "get_commands" => RuntimeCommand::GetCommands,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_become_commands_once_at_the_edge() {
        let prompt = parse(&json!({ "type": "prompt", "message": "hi", "streamingBehavior": "followUp" }));
        assert!(
            matches!(prompt, HubCommand::Prompt { text, behavior: Some(Behavior::FollowUp), focus: None } if text == "hi")
        );
        let from = json!({ "type": "prompt", "message": "hi", "focus": { "device": "mini", "cwd": "/srv/app" } });
        let HubCommand::Prompt { focus: Some(focus), .. } = parse(&from) else { panic!("a prompt with its focus") };
        assert_eq!(focus.to_string(), "/srv/app on mini");
        let forged =
            parse(&json!({ "type": "prompt", "message": "x", "source": { "type": "userInput", "channel": "cli" } }));
        assert!(matches!(forged, HubCommand::Sourced(Err(why)) if why.contains("cannot claim")));
        let answer = parse(&json!({ "type": "approval_response", "id": "a1b2", "approved": true }));
        assert!(matches!(answer, HubCommand::Answer { id, approved: true, reason: None } if id == "a1b2"));
        assert!(matches!(
            parse(&json!({ "type": "set_follow_up_mode", "mode": "all" })),
            HubCommand::Runtime(RuntimeCommand::SetQueueMode { follow_up: true, mode: Some(QueueMode::All) })
        ));
        assert!(matches!(parse(&json!({ "type": "fork" })), HubCommand::Runtime(RuntimeCommand::Fork(None))));
        assert!(matches!(parse(&json!({ "type": "nonsense" })), HubCommand::Unknown));
        assert!(RuntimeCommand::Clone.changes_conversation() && !RuntimeCommand::GetTree.changes_conversation());
    }
}
