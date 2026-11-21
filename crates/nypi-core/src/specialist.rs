//! Specialists (docs/architecture.md §4.5, principle B2): long-lived expert agents Lucia hands
//! work to. A profile lives in `~/.nypi/specialists/<name>/` (or a project's
//! `.nypi/specialists/<name>/`): `profile.json`, `instructions.md`, and `experience.md`, a log
//! each run appends to and the next run reads.
//! - **Native**: a NyPi runtime under the specialist's name, model, and tools. Its name gives it
//!   its own self-prompt layer, which it edits with `self_prompt` as it learns.
//! - **External**: another agent CLI (Codex, Claude Code, ...) run as a subprocess from an argv
//!   template, never a shell. Its JSON event stream is parsed into progress, kept verbatim in a
//!   run log, and reduced to a final answer and status. NyPi sees its tool calls only as the CLI
//!   reports them; the CLI's own sandbox and permissions govern what it may do.
//!
//! Every run is a ledger task owned by the specialist.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Paths;
use crate::ledger::{self, Ledger, LedgerEvent, TaskId, TaskState};
use crate::runtime::{HEARTBEAT_INTERVAL, RuntimeOptions, SessionMode};
use crate::status::{self, RunStatus, StatusKind};
use crate::tools::command::{render, render_argv};

/// Set for external specialists; a NyPi run started with it records its task as this task's child.
pub const PARENT_TASK_ENV: &str = "NYPI_PARENT_TASK";

/// Experience entries put in front of a specialist; older ones stay in the file.
const EXPERIENCE_ENTRIES: usize = 40;
const DEFAULT_TIMEOUT_SECONDS: u64 = 3600;

/// Told to every external specialist, so its reply ends in a status NyPi can read (principle B6).
const EXTERNAL_PREAMBLE: &str = "You are working as a specialist for NyPi, the user's personal agent. Work the task to \
completion on your own. End your final reply with one status line starting with `Done:`, `Blocked:`, `Failed:`, or \
`Continuing:` (in English, with the colon), saying what you did and what comes next.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpecialistKind {
    #[default]
    Native,
    External,
}

/// How an external specialist reports on stdout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// Plain text; all of stdout is the answer.
    #[default]
    Text,
    /// `codex exec --json`.
    CodexJson,
    /// `claude -p --output-format stream-json --verbose`.
    ClaudeStreamJson,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Profile {
    pub kind: SpecialistKind,
    pub description: String,
    pub domain: String,
    /// Native: `provider/id`. External: passed to the template as `{model}`.
    pub model: Option<String>,
    pub thinking: Option<String>,
    /// Native: tool names (default: the usual set).
    pub tools: Option<Vec<String>>,
    /// External: program and arguments; `{task}`, `{cwd}`, and `{model}` are substituted.
    pub command: Vec<String>,
    /// External: template written to stdin.
    pub stdin: Option<String>,
    pub format: OutputFormat,
    pub timeout_seconds: Option<u64>,
    /// External: extra environment; values may reference `${NAME}` secrets.
    pub env: BTreeMap<String, String>,
    /// External: names of secret-looking variables (`OPENAI_API_KEY`) the CLI is started with
    /// from NyPi's own environment. Every other key and token is taken out (decision D76).
    pub pass_env: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Specialist {
    pub name: String,
    pub dir: PathBuf,
    pub profile: Profile,
    pub instructions: String,
}

impl Specialist {
    pub fn experience_path(&self) -> PathBuf {
        self.dir.join("experience.md")
    }

    /// The newest experience entries, for the prompt.
    pub fn experience(&self) -> String {
        let text = std::fs::read_to_string(self.experience_path()).unwrap_or_default();
        let entries: Vec<&str> = text.lines().filter(|l| l.starts_with("- ")).collect();
        entries[entries.len().saturating_sub(EXPERIENCE_ENTRIES)..].join("\n")
    }

    /// Instructions plus experience: what the specialist is told about itself.
    pub fn briefing(&self) -> String {
        let mut text = format!("## Specialist: {}\n", self.name);
        if !self.profile.domain.is_empty() {
            text.push_str(&format!("Domain: {}\n", self.profile.domain));
        }
        if !self.instructions.trim().is_empty() {
            text.push_str(&format!("\n{}\n", self.instructions.trim()));
        }
        let experience = self.experience();
        if !experience.is_empty() {
            text.push_str(&format!("\n## Experience from earlier tasks (newest last)\n{experience}\n"));
        }
        text
    }

    /// Runtime options for a native run in `cwd`, as a child of `parent` when delegated.
    pub fn native_options(&self, cwd: &Path, parent: Option<TaskId>) -> anyhow::Result<RuntimeOptions> {
        if self.profile.kind != SpecialistKind::Native {
            bail!("{} is an external specialist", self.name);
        }
        Ok(RuntimeOptions {
            cwd: cwd.to_path_buf(),
            model: self.profile.model.clone(),
            thinking: self.profile.thinking.as_deref().and_then(nypi_ai::ThinkingLevel::parse),
            tools: self.profile.tools.clone(),
            session: SessionMode::New,
            agent_name: Some(self.name.clone()),
            append_prompt: Some(self.briefing()),
            parent_task: parent,
            // Delegation is one level deep.
            no_delegation: true,
            ..Default::default()
        })
    }

    /// Append one line about a finished run; the next run reads it.
    pub fn record_experience(&self, task: &TaskId, status: &RunStatus) -> std::io::Result<()> {
        use std::io::Write;
        let date = chrono::Local::now().format("%Y-%m-%d");
        let summary: String = status.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(240).collect();
        let derived = if status.synthesized { " (status derived by NyPi)" } else { "" };
        let path = self.experience_path();
        let new_file = !path.exists();
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        if new_file {
            writeln!(file, "# Experience of {}\n", self.name)?;
        }
        writeln!(file, "- {date} {}: {summary}{derived} [task {task}]", status.kind.label())
    }
}

/// Global specialists, then the project's (a project one replaces a global one of the same
/// name). A project's profiles name programs to run, so they count only in a folder the user
/// trusts (decision D61).
pub fn specialist_dirs(paths: &Paths, cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![paths.home.join("specialists")];
    if crate::trust::TrustStore::new(paths).resolve(cwd).is_trusted() {
        dirs.push(cwd.join(".nypi").join("specialists"));
    }
    dirs
}

/// Every specialist, plus one message per profile that could not be read.
pub fn load_all(paths: &Paths, cwd: &Path) -> (Vec<Specialist>, Vec<String>) {
    let mut found: BTreeMap<String, Specialist> = BTreeMap::new();
    let mut errors = Vec::new();
    for root in specialist_dirs(paths, cwd) {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.join("profile.json").is_file() {
                continue;
            }
            match load(&dir) {
                Ok(specialist) => {
                    found.insert(specialist.name.clone(), specialist);
                }
                Err(error) => errors.push(format!("{}: {error:#}", dir.display())),
            }
        }
    }
    (found.into_values().collect(), errors)
}

pub fn find(paths: &Paths, cwd: &Path, name: &str) -> anyhow::Result<Specialist> {
    let (all, errors) = load_all(paths, cwd);
    all.into_iter().find(|s| s.name == name).ok_or_else(|| {
        let hint =
            if errors.is_empty() { String::new() } else { format!(" ({} profile(s) failed to load)", errors.len()) };
        anyhow!(
            "No specialist named {name}{hint}. nypi specialists lists them; nypi specialists new {name} creates one."
        )
    })
}

/// The specialist defined in `dir` (its `profile.json` and `instructions.md`).
pub(crate) fn load_one(dir: &Path) -> anyhow::Result<Specialist> {
    load(dir)
}

fn load(dir: &Path) -> anyhow::Result<Specialist> {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
    valid_name(&name)?;
    let text = std::fs::read_to_string(dir.join("profile.json"))?;
    let profile: Profile = serde_json::from_str(&text).context("profile.json is not a valid profile")?;
    if profile.kind == SpecialistKind::External && profile.command.first().is_none_or(|p| p.trim().is_empty()) {
        bail!("an external specialist needs `command`");
    }
    let instructions = std::fs::read_to_string(dir.join("instructions.md")).unwrap_or_default();
    Ok(Specialist { name, dir: dir.to_path_buf(), profile, instructions })
}

fn valid_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 40 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("specialist names use letters, digits, `-`, and `_`, up to 40 characters");
    }
    Ok(())
}

/// Starting points for `nypi specialists new`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    Native,
    Codex,
    Claude,
}

impl Template {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "native" => Some(Template::Native),
            "codex" => Some(Template::Codex),
            "claude" => Some(Template::Claude),
            _ => None,
        }
    }

    fn files(self, program: &str) -> (Value, &'static str) {
        match self {
            Template::Native => (
                json!({
                    "kind": "native",
                    "description": "Describe what this specialist is the expert for, in one sentence.",
                    "domain": "",
                    "thinking": "high",
                    "tools": ["read", "bash", "edit", "write", "grep", "find", "ls"]
                }),
                "Describe how a master of this field works: the method, the standards, the checks before calling \
                 something done.\n",
            ),
            Template::Codex => (
                json!({
                    "kind": "external",
                    "description": "Coding agent (OpenAI Codex CLI): writes, fixes, and reviews code in a repository and runs its tests.",
                    "domain": "software engineering",
                    "command": [program, "exec", "--json", "--skip-git-repo-check", "-s", "workspace-write", "-C", "{cwd}", "-"],
                    "stdin": "{task}",
                    "format": "codex-json",
                    "timeoutSeconds": 3600
                }),
                "Work like a senior engineer: read the code before changing it, keep changes small and in the \
                 repository's style, run the tests, and report what you changed and how you verified it.\n",
            ),
            Template::Claude => (
                json!({
                    "kind": "external",
                    "description": "Coding agent (Claude Code): writes, fixes, and reviews code in a repository.",
                    "domain": "software engineering",
                    "command": [program, "-p", "--output-format", "stream-json", "--verbose", "--permission-mode", "acceptEdits"],
                    "stdin": "{task}",
                    "format": "claude-stream-json",
                    "timeoutSeconds": 3600
                }),
                "Work like a senior engineer: read the code before changing it, keep changes small and in the \
                 repository's style, run the tests, and report what you changed and how you verified it.\n",
            ),
        }
    }
}

impl Profile {
    /// Whether this profile drives the CLI that `template` makes.
    pub fn format_is(&self, template: Template) -> bool {
        matches!(
            (self.format, template),
            (OutputFormat::CodexJson, Template::Codex) | (OutputFormat::ClaudeStreamJson, Template::Claude)
        )
    }
}

impl Template {
    fn program_name(self) -> &'static str {
        match self {
            Template::Native => "",
            Template::Codex => "codex",
            Template::Claude => "claude",
        }
    }
}

/// An agent CLI installed on this machine.
#[derive(Clone, Debug)]
pub struct DetectedAgent {
    pub template: Template,
    pub program: PathBuf,
    pub version: String,
    /// None when the CLI cannot say.
    pub signed_in: Option<bool>,
}

/// The newest copy of each known agent CLI: on PATH, or bundled with its desktop app (ChatGPT
/// ships a Codex CLI, Claude a Claude Code CLI), which is often newer than a standalone install.
pub fn detect_agents() -> Vec<DetectedAgent> {
    let home = dirs::home_dir().unwrap_or_default();
    let mut found = Vec::new();
    let codex: Vec<PathBuf> = path_lookup("codex")
        .into_iter()
        .chain([
            PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex"),
            PathBuf::from("/Applications/Codex.app/Contents/Resources/codex-cli/bin/codex"),
        ])
        .collect();
    if let Some((program, version)) = newest(codex) {
        let status = output(&program, &["login", "status"]);
        let signed_in =
            status.map(|s| s.to_lowercase().contains("logged in") && !s.to_lowercase().contains("not logged in"));
        found.push(DetectedAgent { template: Template::Codex, program, version, signed_in });
    }
    let mut claude: Vec<PathBuf> = path_lookup("claude");
    claude.push(home.join(".claude/local/claude"));
    let bundled = home.join("Library/Application Support/Claude/claude-code");
    for version in std::fs::read_dir(&bundled).into_iter().flatten().flatten() {
        for build in std::fs::read_dir(version.path()).into_iter().flatten().flatten() {
            claude.push(build.path().join("claude.app/Contents/MacOS/claude"));
        }
    }
    if let Some((program, version)) = newest(claude) {
        // Older versions have no `auth status`; their answer is not JSON.
        let signed_in = output(&program, &["auth", "status"])
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v["loggedIn"].as_bool());
        found.push(DetectedAgent { template: Template::Claude, program, version, signed_in });
    }
    found
}

fn path_lookup(program: &str) -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).map(|dir| dir.join(program)).filter(|p| p.is_file()).collect())
        .unwrap_or_default()
}

fn output(program: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).stdin(Stdio::null()).output().ok()?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Some(text.trim().to_string())
}

/// The candidate with the highest version, and that version.
fn newest(candidates: Vec<PathBuf>) -> Option<(PathBuf, String)> {
    candidates
        .into_iter()
        .filter(|p| p.is_file())
        .filter_map(|p| {
            let text = output(&p, &["--version"])?;
            let version = text.split_whitespace().find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
            Some((p, version.to_string()))
        })
        .max_by_key(|(_, version)| version_key(version))
}

/// `0.166.0-alpha.5.1` sorts as (0, 166, 0): pre-release tags do not order releases here.
fn version_key(version: &str) -> Vec<u64> {
    version.split(['-', '+']).next().unwrap_or_default().split('.').map(|n| n.parse().unwrap_or(0)).collect()
}

/// Create `~/.nypi/specialists/<name>/` from a template. Refuses to overwrite.
pub fn create(paths: &Paths, name: &str, template: Template) -> anyhow::Result<PathBuf> {
    valid_name(name)?;
    let dir = paths.home.join("specialists").join(name);
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    std::fs::create_dir_all(&dir)?;
    // The newest installed copy of the CLI, else its name on PATH.
    let detected = if template == Template::Native { Vec::new() } else { detect_agents() };
    let program = detected
        .into_iter()
        .find(|agent| agent.template == template)
        .map(|agent| agent.program.display().to_string())
        .unwrap_or_else(|| template.program_name().to_string());
    let (profile, instructions) = template.files(&program);
    std::fs::write(dir.join("profile.json"), serde_json::to_string_pretty(&profile)? + "\n")?;
    std::fs::write(dir.join("instructions.md"), instructions)?;
    Ok(dir)
}

/// Progress of an external run, for front ends.
#[derive(Clone, Debug, PartialEq)]
pub enum SpecialistEvent {
    Tool { name: String, summary: String },
    ToolDone { name: String, is_error: bool },
    Text(String),
    Note(String),
}

#[derive(Clone, Debug)]
pub struct SpecialistOutcome {
    pub task: TaskId,
    pub status: RunStatus,
    pub answer: String,
    /// The CLI's raw output, kept for traceability (principle A4).
    pub log: PathBuf,
    pub exit_code: Option<i32>,
}

/// Run an external specialist on `task` in `cwd`, as a ledger task (child of `parent`).
pub async fn run_external(
    specialist: &Specialist,
    paths: &Paths,
    cwd: &Path,
    task_text: &str,
    parent: Option<TaskId>,
    events: &mpsc::UnboundedSender<SpecialistEvent>,
    cancel: CancellationToken,
) -> anyhow::Result<SpecialistOutcome> {
    let profile = &specialist.profile;
    if profile.kind != SpecialistKind::External {
        bail!("{} is a native specialist", specialist.name);
    }
    let briefing = format!("{EXTERNAL_PREAMBLE}\n\n{}\n## Task\n{task_text}\n", specialist.briefing());
    let args = json!({ "task": briefing, "cwd": cwd.display().to_string(), "model": profile.model.clone().unwrap_or_default() });
    let argv = render_argv(&profile.command, &args).map_err(|e| anyhow!(e))?;
    let stdin = profile.stdin.as_deref().map(|t| render(t, &args)).transpose().map_err(|e| anyhow!(e))?;

    let ledger = Ledger::new(paths.ledger());
    let id = ledger::new_task_id();
    ledger.append(&LedgerEvent::Created {
        task: id.clone(),
        at: ledger::now(),
        goal: ledger::excerpt(task_text),
        prompt: task_text.to_string(),
        owner: specialist.name.clone(),
        origin: "specialist".into(),
        parent,
        session_id: format!("external:{}", specialist.name),
        session_path: None,
        cwd: cwd.to_path_buf(),
        remote: None,
        pid: std::process::id(),
        pulse: None,
    })?;
    let state = |state, note: String| LedgerEvent::State { task: id.clone(), at: ledger::now(), state, note };
    ledger.append(&state(TaskState::Running, String::new()))?;

    let runs = specialist.dir.join("runs");
    std::fs::create_dir_all(&runs)?;
    let log_path = runs.join(format!("{id}.log"));
    let mut log = tokio::fs::File::create(&log_path).await?;
    log.write_all(format!("$ {}\n", argv.join(" ")).as_bytes()).await?;

    let mut env = BTreeMap::new();
    // A NyPi started as the CLI files its own task under this one.
    env.insert(PARENT_TASK_ENV.to_string(), id.0.clone());
    for (key, value) in &profile.env {
        env.insert(key.clone(), crate::config::expand_secrets(value)?);
    }
    // The CLI gets the keys its profile names, and no others of the ones NyPi was started with.
    let mut command = std::process::Command::new(&argv[0]);
    crate::sandbox::scrub_env(&mut command, &profile.pass_env);
    let spawned = tokio::process::Command::from(command)
        .args(&argv[1..])
        .current_dir(cwd)
        .envs(&env)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            let note = format!("cannot start {}: {error}", argv[0]);
            ledger.append(&state(TaskState::Failed, note.clone()))?;
            bail!(note);
        }
    };
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes()).await?;
        drop(pipe);
    }
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut stderr = BufReader::new(child.stderr.take().expect("piped stderr")).lines();
    let mut parser = StreamParser::new(profile.format);
    let mut beat = tokio::time::interval(HEARTBEAT_INTERVAL);
    beat.tick().await;
    let timeout = tokio::time::sleep(Duration::from_secs(profile.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECONDS)));
    tokio::pin!(timeout);
    let mut stderr_tail: Vec<String> = Vec::new();
    let (mut out_done, mut err_done) = (false, false);
    let stopped: Option<&str> = loop {
        if out_done && err_done {
            break None;
        }
        tokio::select! {
            line = stdout.next_line(), if !out_done => match line {
                Ok(Some(line)) => {
                    log.write_all(format!("{line}\n").as_bytes()).await?;
                    for event in parser.line(&line) {
                        record_tool(&ledger, &id, &event);
                        let _ = events.send(event);
                    }
                }
                _ => out_done = true,
            },
            line = stderr.next_line(), if !err_done => match line {
                Ok(Some(line)) => {
                    log.write_all(format!("stderr: {line}\n").as_bytes()).await?;
                    stderr_tail.push(line);
                    if stderr_tail.len() > 20 {
                        stderr_tail.remove(0);
                    }
                }
                _ => err_done = true,
            },
            _ = beat.tick() => {
                let _ = ledger.append(&LedgerEvent::Heartbeat { task: id.clone(), at: ledger::now() });
            }
            _ = &mut timeout => break Some("timed out"),
            _ = cancel.cancelled() => break Some("cancelled"),
        }
    };
    if stopped.is_some() {
        let _ = child.start_kill();
    }
    let exit_code = child.wait().await.ok().and_then(|s| s.code());
    let (answer, failure) = parser.finish();
    let status = match (stopped, &failure) {
        (Some(reason), _) => RunStatus {
            kind: StatusKind::Failed,
            text: format!("the specialist was stopped: {reason}"),
            synthesized: true,
        },
        (None, Some(message)) => RunStatus { kind: StatusKind::Failed, text: message.clone(), synthesized: true },
        (None, None) => status::parse(&answer).unwrap_or_else(|| {
            if exit_code == Some(0) {
                RunStatus { kind: StatusKind::Done, text: "finished without a status line".into(), synthesized: true }
            } else {
                let detail = stderr_tail.last().cloned().unwrap_or_default();
                let text = format!("exited with code {}: {detail}", exit_code.map_or("?".into(), |c| c.to_string()));
                RunStatus { kind: StatusKind::Failed, text, synthesized: true }
            }
        }),
    };
    let end = match status.kind {
        _ if stopped == Some("cancelled") => TaskState::Cancelled,
        // The CLI has exited, so nothing of it is still running, whatever it says. Its report is
        // in the hands of whoever delegated, who is held to finishing the work (decision D59).
        StatusKind::Continuing => TaskState::Done,
        _ => TaskState::from_run(&status, false),
    };
    ledger.append(&state(end, ledger::excerpt(&status.to_string())))?;
    if let Err(error) = specialist.record_experience(&id, &status) {
        let _ = events.send(SpecialistEvent::Note(format!("could not update the experience log: {error}")));
    }
    Ok(SpecialistOutcome { task: id, status, answer, log: log_path, exit_code })
}

fn record_tool(ledger: &Ledger, task: &TaskId, event: &SpecialistEvent) {
    // External tools have no ids NyPi controls; the heartbeat is what matters here.
    if matches!(event, SpecialistEvent::Tool { .. } | SpecialistEvent::ToolDone { .. }) {
        let _ = ledger.append(&LedgerEvent::Heartbeat { task: task.clone(), at: ledger::now() });
    }
}

/// Reduces a CLI's output to progress events, a final answer, and a failure, if any.
pub(crate) struct StreamParser {
    format: OutputFormat,
    text: String,
    last_message: String,
    failure: Option<String>,
}

impl StreamParser {
    pub(crate) fn new(format: OutputFormat) -> Self {
        StreamParser { format, text: String::new(), last_message: String::new(), failure: None }
    }

    pub(crate) fn line(&mut self, line: &str) -> Vec<SpecialistEvent> {
        if self.format == OutputFormat::Text {
            self.text.push_str(line);
            self.text.push('\n');
            return vec![SpecialistEvent::Text(line.to_string())];
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else { return Vec::new() };
        match self.format {
            OutputFormat::CodexJson => self.codex(&event),
            OutputFormat::ClaudeStreamJson => self.claude(&event),
            OutputFormat::Text => Vec::new(),
        }
    }

    fn codex(&mut self, event: &Value) -> Vec<SpecialistEvent> {
        let item = &event["item"];
        let done = event["type"] == "item.completed";
        match (event["type"].as_str().unwrap_or_default(), item["type"].as_str().unwrap_or_default()) {
            ("item.started", "command_execution") => {
                vec![SpecialistEvent::Tool { name: "shell".into(), summary: str_of(&item["command"]) }]
            }
            ("item.completed", "command_execution") => vec![SpecialistEvent::ToolDone {
                name: "shell".into(),
                is_error: item["exit_code"].as_i64().is_some_and(|c| c != 0),
            }],
            ("item.completed", "file_change") => {
                let files: Vec<String> = item["changes"]
                    .as_array()
                    .map(|c| c.iter().map(|c| str_of(&c["path"])).collect())
                    .unwrap_or_default();
                vec![SpecialistEvent::Tool { name: "edit".into(), summary: files.join(", ") }]
            }
            ("item.started", "mcp_tool_call") => vec![SpecialistEvent::Tool {
                name: format!("{}:{}", str_of(&item["server"]), str_of(&item["tool"])),
                summary: String::new(),
            }],
            ("item.started", "web_search") => {
                vec![SpecialistEvent::Tool { name: "web_search".into(), summary: str_of(&item["query"]) }]
            }
            ("item.completed", "agent_message") => {
                self.last_message = str_of(&item["text"]);
                vec![SpecialistEvent::Text(self.last_message.clone())]
            }
            // Codex reports warnings as error items; they do not end the run.
            ("item.completed", "error") if done => vec![SpecialistEvent::Note(str_of(&item["message"]))],
            ("turn.failed", _) => {
                self.failure = Some(error_text(&event["error"]["message"]));
                Vec::new()
            }
            ("error", _) => {
                self.failure = Some(error_text(&event["message"]));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn claude(&mut self, event: &Value) -> Vec<SpecialistEvent> {
        let mut out = Vec::new();
        match event["type"].as_str().unwrap_or_default() {
            "assistant" | "user" => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str().unwrap_or_default() {
                        "tool_use" => {
                            let input = &block["input"];
                            let summary = ["command", "file_path", "path", "pattern", "url", "description"]
                                .iter()
                                .find_map(|k| input[*k].as_str())
                                .map(str::to_string)
                                .unwrap_or_default();
                            out.push(SpecialistEvent::Tool { name: str_of(&block["name"]), summary });
                        }
                        "tool_result" => out.push(SpecialistEvent::ToolDone {
                            name: String::new(),
                            is_error: block["is_error"] == true,
                        }),
                        "text" if event["type"] == "assistant" => {
                            self.last_message = str_of(&block["text"]);
                            out.push(SpecialistEvent::Text(self.last_message.clone()));
                        }
                        _ => {}
                    }
                }
            }
            "result" => {
                if let Some(result) = event["result"].as_str() {
                    self.last_message = result.to_string();
                }
                if event["is_error"] == true {
                    self.failure = Some(self.last_message.clone());
                }
            }
            _ => {}
        }
        out
    }

    pub(crate) fn finish(self) -> (String, Option<String>) {
        let answer = if self.format == OutputFormat::Text { self.text.trim().to_string() } else { self.last_message };
        (answer, self.failure)
    }
}

fn str_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// Codex nests the API's JSON error inside its message; show the innermost message.
fn error_text(value: &Value) -> String {
    let text = str_of(value);
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|inner| inner["error"]["message"].as_str().map(str::to_string))
        .unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_numerically() {
        assert!(version_key("0.166.0-alpha.5.1") > version_key("0.144.5"));
        assert!(version_key("2.1.309") > version_key("1.0.71"));
    }

    #[test]
    fn parses_codex_and_claude_streams() {
        // Shapes recorded from codex-cli 0.144 and Claude Code 1.0.
        let mut codex = StreamParser::new(OutputFormat::CodexJson);
        for line in [
            r#"{"type":"thread.started","thread_id":"t"}"#,
            r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Model metadata not found"}}"#,
            r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"cargo test","status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"cargo test","exit_code":1}}"#,
            r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"Fixed it.\n\nDone: tests pass."}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10}}"#,
        ] {
            codex.line(line);
        }
        assert_eq!(codex.finish(), ("Fixed it.\n\nDone: tests pass.".to_string(), None));

        let mut failed = StreamParser::new(OutputFormat::CodexJson);
        failed.line(r#"{"type":"turn.failed","error":{"message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The model requires a newer version of Codex.\"}}"}}"#);
        assert_eq!(failed.finish().1.as_deref(), Some("The model requires a newer version of Codex."));

        let mut claude = StreamParser::new(OutputFormat::ClaudeStreamJson);
        let events: Vec<SpecialistEvent> = [
            r#"{"type":"system","subtype":"init","session_id":"s"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu","is_error":false}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Listed."}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Listed.\n\nDone: listed."}"#,
        ]
        .iter()
        .flat_map(|l| claude.line(l))
        .collect();
        assert_eq!(events[0], SpecialistEvent::Tool { name: "Bash".into(), summary: "ls".into() });
        assert_eq!(claude.finish(), ("Listed.\n\nDone: listed.".to_string(), None));

        let mut login = StreamParser::new(OutputFormat::ClaudeStreamJson);
        login.line(
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key · Please run /login"}"#,
        );
        assert_eq!(login.finish().1.as_deref(), Some("Invalid API key · Please run /login"));
    }

    #[tokio::test]
    async fn external_runs_are_tasks_with_logs_and_experience() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let dir = create(&paths, "echoer", Template::Native).unwrap();
        // An external CLI that reads the task on stdin and answers with a status line.
        std::fs::write(
            dir.join("profile.json"),
            r#"{"kind":"external","description":"Echo","command":["sh","-c","grep -c Task; echo 'Done: echoed the task.'"],"stdin":"{task}"}"#,
        )
        .unwrap();
        let specialist = find(&paths, work.path(), "echoer").unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcome =
            run_external(&specialist, &paths, work.path(), "count", None, &tx, CancellationToken::new()).await.unwrap();
        assert_eq!((outcome.status.kind, outcome.status.synthesized), (StatusKind::Done, false));
        assert!(outcome.answer.starts_with("1\nDone:"), "{}", outcome.answer);
        assert!(std::fs::read_to_string(&outcome.log).unwrap().starts_with("$ sh -c"));
        let task = Ledger::new(paths.ledger()).get(&outcome.task).unwrap();
        assert_eq!((task.state, task.owner.as_str()), (TaskState::Done, "echoer"));
        assert!(specialist.experience().contains(&format!("Done: echoed the task. [task {}]", outcome.task)));
        assert!(matches!(rx.try_recv().unwrap(), SpecialistEvent::Text(_)));

        std::fs::write(dir.join("profile.json"), r#"{"kind":"external","command":["/nonexistent/cli"]}"#).unwrap();
        let broken = find(&paths, work.path(), "echoer").unwrap();
        let error =
            run_external(&broken, &paths, work.path(), "x", None, &tx, CancellationToken::new()).await.unwrap_err();
        assert!(format!("{error}").contains("cannot start"));
        assert!(create(&paths, "echoer", Template::Native).is_err(), "never overwrites");
        assert!(create(&paths, "bad name", Template::Native).is_err());
    }
}
