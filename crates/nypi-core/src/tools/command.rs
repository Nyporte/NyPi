//! Command tools: external programs declared in `settings.commandTools` and exposed as agent
//! tools, for example another agent CLI acting as a specialist (principles A7, B2). Arguments are
//! substituted into argv without a shell, so a value can never inject a second command.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::bail;
use async_trait::async_trait;
use nypi_agent::{Tool, ToolContext, ToolOutput};
use nypi_ai::ToolDefinition;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::bash::run_process;

/// One entry of `settings.commandTools`, keyed by tool name.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CommandToolConfig {
    pub description: String,
    /// Program and arguments. `{name}` is replaced by the argument `name`. An element that is
    /// exactly `{name}` is dropped when the argument is absent and expands to several elements
    /// when it is an array. `{{` and `}}` are literal braces.
    pub command: Vec<String>,
    /// JSON Schema of the arguments; defaults to no arguments.
    pub parameters: Option<Value>,
    /// Template written to the program's stdin, with the same substitution.
    pub stdin: Option<String>,
    /// Working directory; defaults to the session's.
    pub cwd: Option<String>,
    /// Extra environment; values may reference `${NAME}` secrets.
    pub env: BTreeMap<String, String>,
    pub timeout_seconds: Option<u64>,
    pub disabled: bool,
}

/// Long enough for another agent to finish a real task; the stall notice covers the wait.
const DEFAULT_TIMEOUT_SECONDS: u64 = 3600;

pub struct CommandTool {
    name: String,
    config: CommandToolConfig,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    /// The program runs under this (decision D62).
    sandbox: crate::sandbox::Sandbox,
}

impl CommandTool {
    pub fn new(name: &str, config: &CommandToolConfig, session_cwd: &Path) -> anyhow::Result<Self> {
        if name.is_empty()
            || name.len() > 64
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("tool names may use letters, digits, `_`, and `-`, up to 64 characters");
        }
        if super::ALL_TOOLS.contains(&name) || name.starts_with("mcp__") {
            bail!("the name is taken by a built-in or MCP tool");
        }
        if config.command.first().is_none_or(|program| program.trim().is_empty()) {
            bail!("`command` needs at least the program to run");
        }
        let mut env = BTreeMap::new();
        for (key, value) in &config.env {
            env.insert(key.clone(), crate::config::expand_secrets(value)?);
        }
        let cwd = config.cwd.as_deref().map(crate::config::expand_home).unwrap_or_else(|| session_cwd.to_path_buf());
        Ok(CommandTool { name: name.to_string(), config: config.clone(), cwd, env, sandbox: Default::default() })
    }

    pub fn sandboxed(mut self, sandbox: &crate::sandbox::Sandbox) -> Self {
        self.sandbox = sandbox.clone();
        self
    }
}

#[async_trait]
impl Tool for CommandTool {
    fn definition(&self) -> ToolDefinition {
        let parameters = match &self.config.parameters {
            Some(schema) if schema.is_object() => schema.clone(),
            _ => json!({ "type": "object", "properties": {} }),
        };
        let description = if self.config.description.is_empty() {
            format!("Run {}", self.config.command[0])
        } else {
            self.config.description.clone()
        };
        ToolDefinition { name: self.name.clone(), description, parameters }
    }

    async fn execute(&self, args: Value, ctx: ToolContext) -> anyhow::Result<ToolOutput> {
        let argv = match render_argv(&self.config.command, &args) {
            Ok(argv) if !argv.is_empty() && !argv[0].is_empty() => argv,
            Ok(_) => return Ok(ToolOutput::error("The command template produced no program to run")),
            Err(error) => return Ok(ToolOutput::error(error)),
        };
        let stdin = match &self.config.stdin {
            Some(template) => match render(template, &args) {
                Ok(text) => Some(text),
                Err(error) => return Ok(ToolOutput::error(error)),
            },
            None => None,
        };
        let mut cmd = self.sandbox.command(&argv[0]);
        cmd.args(&argv[1..]).current_dir(&self.cwd).envs(&self.env);
        let timeout = self.config.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS) as f64;
        let result = match run_process(cmd, stdin, Some(timeout), &ctx).await {
            Ok(result) => result,
            Err(error) => return Ok(ToolOutput::error(format!("{error:#}"))),
        };
        let mut output = ToolOutput::text(result.text).with_details(json!({
            "argv": argv,
            "exitCode": result.exit_code,
            "fullOutputPath": result.full_output_path,
        }));
        output.is_error = result.is_error;
        Ok(output)
    }
}

/// Build the enabled command tools. Returns them plus one message per invalid entry.
pub fn create_command_tools(
    configs: &BTreeMap<String, CommandToolConfig>,
    cwd: &Path,
    sandbox: &crate::sandbox::Sandbox,
) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut errors = Vec::new();
    for (name, config) in configs.iter().filter(|(_, c)| !c.disabled) {
        match CommandTool::new(name, config, cwd) {
            Ok(tool) => tools.push(Arc::new(tool.sandboxed(sandbox))),
            Err(error) => errors.push(format!("Command tool {name}: {error:#}")),
        }
    }
    (tools, errors)
}

pub fn render_argv(template: &[String], args: &Value) -> Result<Vec<String>, String> {
    let mut argv = Vec::new();
    for element in template {
        if let Some(name) = whole_placeholder(element) {
            match &args[name] {
                Value::Null => {}
                Value::Array(items) => argv.extend(items.iter().map(scalar)),
                value => argv.push(scalar(value)),
            }
            continue;
        }
        argv.push(render(element, args)?);
    }
    Ok(argv)
}

/// `{name}` and nothing else.
fn whole_placeholder(element: &str) -> Option<&str> {
    let name = element.strip_prefix('{')?.strip_suffix('}')?;
    is_identifier(name).then_some(name)
}

pub fn render(template: &str, args: &Value) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(index) = rest.find(['{', '}']) {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        if let Some(end) = tail.find('}')
            && tail.starts_with('{')
            && is_identifier(&tail[1..end])
        {
            let value = &args[&tail[1..end]];
            match value {
                Value::Null => {}
                Value::Array(_) | Value::Object(_) => out.push_str(&value.to_string()),
                value => out.push_str(&scalar(value)),
            }
            rest = &tail[end + 1..];
            continue;
        }
        // A brace that is not a placeholder, for example inside a JSON literal.
        out.push_str(&tail[..1]);
        rest = &tail[1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_arguments_without_a_shell() {
        let template: Vec<String> =
            ["git", "log", "-n", "{count}", "{paths}", "--format={{%h}} {label}", "{missing}", "{\"k\":1}"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let args = json!({ "count": 3, "paths": ["a.rs", "b.rs"], "label": "x; rm -rf ~" });
        assert_eq!(
            render_argv(&template, &args).unwrap(),
            ["git", "log", "-n", "3", "a.rs", "b.rs", "--format={%h} x; rm -rf ~", "{\"k\":1}"]
        );
    }

    #[test]
    fn rejects_bad_definitions() {
        let cwd = Path::new("/");
        let ok = CommandToolConfig { command: vec!["echo".into()], ..Default::default() };
        assert!(CommandTool::new("bash", &ok, cwd).is_err());
        assert!(CommandTool::new("has space", &ok, cwd).is_err());
        assert!(CommandTool::new("echoer", &CommandToolConfig::default(), cwd).is_err());
        assert!(CommandTool::new("echoer", &ok, cwd).is_ok());
    }

    #[tokio::test]
    async fn runs_the_program_with_arguments_and_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let config = CommandToolConfig {
            description: "Count words".into(),
            command: vec![
                "sh".into(),
                "-c".into(),
                "printf '%s:' \"$1\"; wc -w | tr -d ' '".into(),
                "sh".into(),
                "{tag}".into(),
            ],
            parameters: Some(
                json!({ "type": "object", "properties": { "tag": { "type": "string" }, "text": { "type": "string" } } }),
            ),
            stdin: Some("{text}".into()),
            ..Default::default()
        };
        let tool = CommandTool::new("wordcount", &config, dir.path()).unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolContext::new("c".into(), "wordcount".into(), tokio_util::sync::CancellationToken::new(), tx);
        let output = tool.execute(json!({ "tag": "n", "text": "one two three" }), ctx.clone()).await.unwrap();
        assert!(!output.is_error);
        assert_eq!(nypi_ai::text_of(&output.content), "n:3");

        let failing = CommandToolConfig { command: vec!["false".into()], ..Default::default() };
        let output =
            CommandTool::new("fails", &failing, dir.path()).unwrap().execute(json!({}), ctx.clone()).await.unwrap();
        assert!(output.is_error);
        let missing = CommandToolConfig { command: vec!["/nonexistent/program".into()], ..Default::default() };
        let output = CommandTool::new("missing", &missing, dir.path()).unwrap().execute(json!({}), ctx).await.unwrap();
        assert!(output.is_error && nypi_ai::text_of(&output.content).contains("Cannot start"));
    }
}
