//! What the chat module may look up itself (decision D82): Lucia's own notes, memory, assets,
//! schedules, and goals, read in the hub's process from the same stores the worker uses. A
//! question about her own state ("when does the domain renew?", "what did we decide about the
//! importer?") is then answered in the turn it was asked, instead of as a task of its own.
//!
//! Only reading. Each tool here wraps the worker's own and lets through only the actions that
//! read; anything else is refused with a result that says the Principal does it. No tool here
//! reaches the network or changes a store, so the chat module still holds nothing that acts.
//!
//! Since decision D84 chat also reads files on the devices (`read`, `grep`, `find`, `ls`),
//! through the conversation's place and at the owner's focus, under three guards that keep a
//! read from ever holding the owner up: the Inspector's guard and approval policy are judged
//! before the call, and a read that would ask the owner is refused at once instead of asking;
//! the call has 1.5 seconds; and what it returns is cut to 4,000 characters.

use std::sync::Arc;

use async_trait::async_trait;
use nypi_agent::{ExecutionMode, Tool, ToolContext, ToolOutput};
use nypi_ai::{ContentBlock, ToolDefinition};
use serde_json::Value;

/// The longest excerpt of a vault note chat is shown; the worker reads whole notes.
pub const VAULT_EXCERPT_CHARS: usize = 2000;

/// The worker's tools chat gets, each with the actions it may use (none: the tool only reads)
/// and how much of its result it is shown.
const READ_ONLY: [(&str, &[&str], Option<usize>); 7] = [
    ("vault_search", &[], None),
    ("vault_list", &[], None),
    ("vault_read", &[], Some(VAULT_EXCERPT_CHARS)),
    ("memory", &["recall"], None),
    ("asset_manage", &["list", "get", "expiring_within_days"], None),
    ("schedule", &["list"], None),
    ("goals", &["list"], None),
];

/// Whether a call of store tool `name` with `arguments` only reads (decision D87: any other
/// call changes a store).
pub fn reads_only(name: &str, arguments: &Value) -> bool {
    READ_ONLY.iter().any(|(tool, actions, _)| {
        *tool == name && (actions.is_empty() || arguments["action"].as_str().is_some_and(|a| actions.contains(&a)))
    })
}

/// How long a device read of chat's may take.
pub const DEVICE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
/// What a device read of chat's returns, at most.
pub const DEVICE_READ_CHARS: usize = 4000;
/// The device tools chat reads with.
pub const DEVICE_READS: [&str; 4] = ["read", "grep", "find", "ls"];

/// Chat's time-boxed reads of `tools` (the worker's device tools), judged by `inspector`, at
/// the owner's `focus` (decision D84). `here` is the name of the computer the hub runs on, when
/// the place is routed: a call names its device, so it never follows the worker's focus.
pub fn device_reads(
    tools: &[Arc<dyn Tool>],
    inspector: Arc<crate::inspector::Inspector>,
    focus: Arc<std::sync::Mutex<Option<crate::place::Focus>>>,
    here: Option<String>,
) -> Vec<Arc<dyn Tool>> {
    DEVICE_READS
        .iter()
        .filter_map(|name| {
            let inner = tools.iter().find(|tool| tool.definition().name == *name)?.clone();
            let routed = inner.definition().parameters["properties"]["device"].is_object();
            Some(Arc::new(TimeBoxed {
                inner,
                inspector: inspector.clone(),
                focus: focus.clone(),
                here: here.clone().filter(|_| routed),
            }) as Arc<dyn Tool>)
        })
        .collect()
}

struct TimeBoxed {
    inner: Arc<dyn Tool>,
    inspector: Arc<crate::inspector::Inspector>,
    focus: Arc<std::sync::Mutex<Option<crate::place::Focus>>>,
    here: Option<String>,
}

impl TimeBoxed {
    /// The call as it is carried out: at the owner's focus, a relative path made absolute
    /// against it, and the device named.
    fn placed(&self, mut arguments: Value) -> Value {
        let Some(object) = arguments.as_object_mut() else { return arguments };
        let focus = self.focus.lock().unwrap().clone();
        let named = object.get("device").and_then(Value::as_str).is_some_and(|device| !device.trim().is_empty());
        if let Some(focus) = focus.filter(|_| !named) {
            let folder = std::path::Path::new(&focus.folder);
            let path = object.get("path").and_then(Value::as_str).map(str::to_string);
            let absolute = match path {
                Some(path) if std::path::Path::new(&path).is_absolute() || path.starts_with('~') => path,
                Some(path) => folder.join(path).display().to_string(),
                None => focus.folder.clone(),
            };
            object.insert("path".into(), Value::String(absolute));
            if let Some(device) = focus.device.or_else(|| self.here.clone()) {
                object.insert("device".into(), Value::String(device));
            }
        } else if !named && let Some(here) = &self.here {
            object.insert("device".into(), Value::String(here.clone()));
        }
        arguments
    }
}

#[async_trait]
impl Tool for TimeBoxed {
    fn definition(&self) -> ToolDefinition {
        let mut definition = self.inner.definition();
        definition.description = format!(
            "{} Read only, and quick: relative paths start at the folder the user is at; a read that takes over 1.5 \
             seconds is stopped, one that would need the user's approval is refused, and you are shown at most \
             {DEVICE_READ_CHARS} characters. For a longer search, signal the Principal.",
            definition.description
        );
        definition
    }

    fn prepare_arguments(&self, arguments: Value) -> Value {
        self.inner.prepare_arguments(arguments)
    }

    async fn execute(&self, arguments: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let name = self.inner.definition().name;
        let arguments = self.placed(arguments);
        // Judged before the call, never asked about: a read never waits on the owner.
        if let Err(why) = self.inspector.allowed_unasked(&name, &arguments) {
            return Ok(ToolOutput::error(why));
        }
        // Its own stop, so a read that runs over ends without ending chat's turn.
        let mut ctx = ctx;
        ctx.cancel = ctx.cancel.child_token();
        let cancel = ctx.cancel.clone();
        let run = self.inner.execute(arguments, ctx);
        tokio::pin!(run);
        tokio::select! {
            output = &mut run => {
                let mut output = output?;
                output.content = output.content.into_iter().map(|block| excerpt(block, DEVICE_READ_CHARS)).collect();
                Ok(output)
            }
            _ = tokio::time::sleep(DEVICE_READ_TIMEOUT) => {
                // Stopped, not dropped: a read on another device is told to stop there too, and
                // given a moment to do so.
                cancel.cancel();
                let _ = tokio::time::timeout(std::time::Duration::from_millis(250), &mut run).await;
                Ok(ToolOutput::error(
                    "Read timed out (>1.5s); manage the user's expectation and signal the Principal for background \
                     scanning",
                ))
            }
        }
    }
}

/// The tool names chat may look things up with.
pub fn names() -> Vec<&'static str> {
    READ_ONLY.iter().map(|(name, _, _)| *name).collect()
}

/// Chat's read-only view of `tools` (the worker's): the ones in `READ_ONLY` that the runtime
/// has, each limited to reading.
pub fn read_only_tools(tools: &[Arc<dyn Tool>]) -> Vec<Arc<dyn Tool>> {
    READ_ONLY
        .iter()
        .filter_map(|(name, actions, cap)| {
            let inner = tools.iter().find(|tool| tool.definition().name == *name)?.clone();
            Some(Arc::new(ReadOnly { inner, actions, cap: *cap }) as Arc<dyn Tool>)
        })
        .collect()
}

struct ReadOnly {
    inner: Arc<dyn Tool>,
    /// The actions let through; empty for a tool that has none and only reads.
    actions: &'static [&'static str],
    cap: Option<usize>,
}

#[async_trait]
impl Tool for ReadOnly {
    fn definition(&self) -> ToolDefinition {
        let mut definition = self.inner.definition();
        if !self.actions.is_empty() {
            if let Some(action) = definition.parameters.pointer_mut("/properties/action") {
                action["enum"] = Value::from(self.actions.to_vec());
            }
            definition.description = format!(
                "Read only here ({}): changes go to a worker with start_task. {}",
                self.actions.join(", "),
                definition.description
            );
        }
        if let Some(cap) = self.cap {
            definition.description = format!(
                "{} You are shown at most {cap} characters; a worker reads the whole note.",
                definition.description
            );
        }
        definition
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.inner.execution_mode()
    }

    async fn execute(&self, arguments: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let name = self.inner.definition().name;
        if !self.actions.is_empty() {
            let action = arguments["action"].as_str().unwrap_or_default();
            if !self.actions.contains(&action) {
                return Ok(ToolOutput::error(format!(
                    "{name} only reads here ({}). To {} anything, start a task: a worker has the tool in full.",
                    self.actions.join(", "),
                    if action.is_empty() { "change" } else { action }
                )));
            }
        }
        let mut output = self.inner.execute(arguments, ctx).await?;
        if let Some(cap) = self.cap {
            output.content = output.content.into_iter().map(|block| excerpt(block, cap)).collect();
        }
        Ok(output)
    }
}

/// A text block cut to `cap` characters, saying so.
fn excerpt(block: ContentBlock, cap: usize) -> ContentBlock {
    match block {
        ContentBlock::Text { text, .. } if text.chars().count() > cap => {
            let total = text.chars().count();
            let cut: String = text.chars().take(cap).collect();
            ContentBlock::text(format!("{cut}\n[excerpt: {cap} of {total} characters; a worker can read the rest]"))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use nypi_agent::ToolContext;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::config::Paths;
    use crate::inspector::{ApprovalSettings, Guard, Inspector};
    use crate::protected::Protected;

    /// A `read` that takes as long as it is told, and says whether it was stopped.
    struct SlowRead {
        takes: Duration,
        stopped: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl Tool for SlowRead {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "read".into(),
                description: "Read a file.".into(),
                parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
            }
        }

        async fn execute(&self, _arguments: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
            tokio::select! {
                _ = tokio::time::sleep(self.takes) => Ok(ToolOutput::text("x".repeat(9000))),
                _ = ctx.cancel.cancelled() => {
                    self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(ToolOutput::error("stopped"))
                }
            }
        }
    }

    fn reads(takes: Duration) -> (Arc<dyn Tool>, Arc<std::sync::atomic::AtomicBool>, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let protected = Protected::from_parts(home.path(), None, Vec::new(), vec![paths.auth()]);
        let guard = Guard::new(&paths, home.path(), protected, false);
        let (alerts, _) = tokio::sync::mpsc::unbounded_channel();
        let inspector = Arc::new(Inspector::new(&paths, "s", &ApprovalSettings::default(), alerts, guard));
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let slow: Arc<dyn Tool> = Arc::new(SlowRead { takes, stopped: stopped.clone() });
        let tool = device_reads(&[slow], inspector, Arc::default(), None).remove(0);
        (tool, stopped, home)
    }

    fn ctx() -> ToolContext {
        let (sink, _) = tokio::sync::mpsc::unbounded_channel();
        ToolContext::new("c1".into(), "read".into(), CancellationToken::new(), sink)
    }

    #[tokio::test]
    async fn a_read_that_runs_over_is_stopped_at_its_time_and_says_what_to_do() {
        let (tool, stopped, _home) = reads(Duration::from_secs(30));
        let started = Instant::now();
        let output = tool.execute(json!({ "path": "big.log" }), ctx()).await.unwrap();
        let took = started.elapsed();
        assert!(took >= DEVICE_READ_TIMEOUT && took < Duration::from_secs(3), "{took:?}");
        assert!(output.is_error);
        assert!(nypi_ai::text_of(&output.content).contains("Read timed out (>1.5s)"));
        // The read itself was stopped, not left running.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(stopped.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_quick_read_is_cut_to_what_chat_is_shown() {
        let (tool, _, _home) = reads(Duration::from_millis(10));
        let output = tool.execute(json!({ "path": "big.log" }), ctx()).await.unwrap();
        let text = nypi_ai::text_of(&output.content);
        assert!(!output.is_error && text.chars().count() < DEVICE_READ_CHARS + 100, "{}", text.chars().count());
    }
}
