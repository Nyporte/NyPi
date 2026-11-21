//! The Inspector: an append-only audit log of every agent event, a guard that blocks
//! catastrophic tool calls, and an approval gate that asks the user before risky ones. It lives
//! in the harness; no tool exposes it, and the guard denies tool writes to its own files, so the
//! agent cannot modify it (docs/architecture.md §3.5).

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use nypi_agent::{AgentEvent, Block, ToolHooks};
use nypi_ai::ToolCall;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::config::Paths;
use crate::protected::{Protected, lexical_path, real_path};
use crate::pulse::{Pulse, PulseId, PulseTap};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertLevel {
    /// Something the user asked to be told (a reminder, a scheduled result), not a problem.
    Notice,
    Warning,
    Error,
}

/// Something the user must hear about promptly (principle A3).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Alert {
    pub level: AlertLevel,
    pub message: String,
    pub at: String,
}

impl Alert {
    pub fn new(level: AlertLevel, message: impl Into<String>) -> Self {
        Alert { level, message: message.into(), at: chrono::Utc::now().to_rfc3339() }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Never ask.
    Off,
    /// Ask before destructive or outward-facing calls (the policy's rules plus `commands` and `tools`).
    #[default]
    Risky,
    /// Ask before every call except read-only tools.
    All,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApprovalSettings {
    pub mode: ApprovalMode,
    /// Extra regular expressions; a matching bash command needs approval.
    pub commands: Vec<String>,
    /// Tool name globs that always need approval, for example `mcp__gmail__*`.
    pub tools: Vec<String>,
    /// Deny when nobody answers within this many seconds.
    pub timeout_seconds: u64,
    /// Credentials `http_request` may use unasked: a secret's scope, and the hosts it is trusted
    /// for (`"github": ["api.github.com"]`; `*.example.com` covers a domain). Any other use of a
    /// credential is asked about, whatever `mode` says (decision D77).
    pub trusted_credentials: std::collections::BTreeMap<String, Vec<String>>,
}

impl Default for ApprovalSettings {
    fn default() -> Self {
        ApprovalSettings {
            mode: ApprovalMode::Risky,
            commands: Vec::new(),
            tools: Vec::new(),
            timeout_seconds: 900,
            trusted_credentials: Default::default(),
        }
    }
}

/// A tool call waiting for the user's decision. Answer with `approve` or `deny`; dropping it denies.
#[derive(Debug)]
pub struct ApprovalRequest {
    /// Names the request wherever it is shown, so any of them can answer it (decision D56).
    pub id: String,
    pub tool: String,
    /// The command, path, or arguments, shortened for display.
    pub summary: String,
    /// Why the policy wants approval.
    pub reason: String,
    /// A listener (the board) shows the request too and can answer it, so it may wait for an
    /// answer even where nobody attached can give one.
    pub listened: bool,
    respond: Responder,
}

/// What the approver hears: a call that waits, and the end of a wait, however it ended (the
/// approver's own answer, the board's, a timeout). Every interface showing the question learns
/// from `Closed` that it is gone, so an answer given on the board dismisses it everywhere.
#[derive(Debug)]
pub enum Approval {
    Asked(ApprovalRequest),
    Closed { id: String, approved: bool },
}

/// The answer to one request, shared by everyone who may give it; the first answer counts.
type Responder = Arc<Mutex<Option<oneshot::Sender<Decision>>>>;

struct Pending {
    tool: String,
    summary: String,
    reason: String,
    responder: Responder,
}

fn respond(responder: &Responder, decision: Decision) -> bool {
    match responder.lock().unwrap().take() {
        Some(sender) => sender.send(decision).is_ok(),
        None => false,
    }
}

/// Approval requests as they come and go, for a listener such as the board (decision D56).
#[derive(Clone, Debug)]
pub enum ApprovalEvent {
    Waiting { id: String, tool: String, summary: String, reason: String },
    Closed { id: String },
}

impl ApprovalRequest {
    pub fn approve(self) {
        respond(&self.respond, Decision::Approve);
    }

    /// False once the request was answered elsewhere, or the Inspector stopped waiting.
    pub fn is_waiting(&self) -> bool {
        self.respond.lock().unwrap().as_ref().is_some_and(|sender| !sender.is_closed())
    }

    /// Deny, optionally with the user's explanation, which the agent is shown.
    pub fn deny(self, reason: Option<String>) {
        respond(&self.respond, Decision::Deny(reason));
    }

    /// One-paragraph description for prompts in any front end.
    pub fn describe(&self) -> String {
        format!("{}: {}\nWhy approval is needed: {}", self.tool, self.summary, self.reason)
    }
}

#[derive(Debug)]
enum Decision {
    Approve,
    Deny(Option<String>),
}

pub struct Inspector {
    audit_dir: PathBuf,
    session_id: Mutex<String>,
    /// The pulse whose run is in progress; every record written meanwhile names it.
    cause: Mutex<Option<PulseId>>,
    seq: AtomicU64,
    file_lock: Mutex<()>,
    guard: Guard,
    policy: Policy,
    approval_timeout: Duration,
    approver: Mutex<Option<mpsc::UnboundedSender<Approval>>>,
    /// Requests waiting for an answer, by id, so a listener can answer them too.
    pending: Mutex<HashMap<String, Pending>>,
    approval_listener: Mutex<Option<mpsc::UnboundedSender<ApprovalEvent>>>,
    alerts: mpsc::UnboundedSender<Alert>,
    /// What it was made from, for judging a call that is carried out somewhere else.
    paths: Paths,
    approval: ApprovalSettings,
    /// Says where a call goes when that is not the runtime's own place (decision D79).
    router: Mutex<Option<CallRouter>>,
    /// The guard and policy for each target a call was routed to so far.
    judges: Mutex<HashMap<crate::place::CallTarget, Arc<(Guard, Policy)>>>,
    /// Tools a trusted folder on another device asks approval for while the run works there
    /// (decision D91): added to the hub's list, never taking anything from it.
    added_tools: std::sync::RwLock<globset::GlobSet>,
}

/// Where a call of a tool with these arguments is carried out, when not at the runtime's own
/// place (`Place::judged`).
pub type CallRouter = Arc<dyn Fn(&str, &Value) -> Option<crate::place::Judged> + Send + Sync>;

/// The approval policy for the tools `guard` judges: their folder, and the home and disk of
/// the machine they run on.
fn policy_for(approval: &ApprovalSettings, guard: &Guard) -> (Policy, Vec<String>) {
    let (mut policy, invalid) = Policy::new(approval);
    policy.cwd = guard.cwd.clone();
    if let Disk::Device { home } = &guard.disk {
        // `~` is the device's home. Unknown, it is a name of its own rather than this
        // computer's home: `~/.ssh` and `$HOME/.bashrc` still ask, by their spelling.
        policy.home = home.clone().unwrap_or_else(|| PathBuf::from("/~device-home"));
        policy.sensitive_files = sensitive_files(&policy.home);
        policy.local = false;
    }
    (policy, invalid)
}

impl Inspector {
    /// Create an Inspector writing under `paths.audit()`; alerts go to `alerts`.
    pub fn new(
        paths: &Paths,
        session_id: &str,
        approval: &ApprovalSettings,
        alerts: mpsc::UnboundedSender<Alert>,
        guard: Guard,
    ) -> Self {
        let (policy, invalid) = policy_for(approval, &guard);
        for pattern in invalid {
            let _ = alerts.send(Alert::new(AlertLevel::Warning, format!("Ignoring invalid approval rule: {pattern}")));
        }
        Inspector {
            audit_dir: paths.audit(),
            session_id: Mutex::new(session_id.to_string()),
            cause: Mutex::new(None),
            seq: AtomicU64::new(0),
            file_lock: Mutex::new(()),
            guard,
            policy,
            approval_timeout: Duration::from_secs(approval.timeout_seconds.max(1)),
            approver: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            approval_listener: Mutex::new(None),
            alerts,
            paths: paths.clone(),
            approval: approval.clone(),
            router: Mutex::new(None),
            judges: Mutex::new(HashMap::new()),
            added_tools: std::sync::RwLock::new(globset::GlobSet::empty()),
        }
    }

    /// Ask approval for these tool globs too, until the next call replaces them (an empty list
    /// takes them away). Globs that do not parse are left out.
    pub fn set_added_approval_tools(&self, globs: &[String]) {
        let mut set = globset::GlobSetBuilder::new();
        for glob in globs {
            if let Ok(glob) = globset::Glob::new(glob) {
                set.add(glob);
            }
        }
        *self.added_tools.write().unwrap() = set.build().unwrap_or_else(|_| globset::GlobSet::empty());
    }

    /// Why a tool needs approval because the folder the run works at asked for it.
    fn added_reason(&self, tool: &str) -> Option<String> {
        self.added_tools
            .read()
            .unwrap()
            .is_match(tool)
            .then(|| format!("{tool} needs approval in this folder (its .nypi/settings.json asks for it)"))
    }

    /// Have calls that are carried out elsewhere judged against where they go: the folder
    /// there, and that device's home and disk instead of this computer's.
    pub fn set_router(&self, router: CallRouter) {
        *self.router.lock().unwrap() = Some(router);
    }

    /// The guard and policy for calls carried out at `target`, made once per target.
    fn judges_at(&self, target: &crate::place::CallTarget) -> Arc<(Guard, Policy)> {
        let mut judges = self.judges.lock().unwrap();
        if let Some(judges) = judges.get(target) {
            return judges.clone();
        }
        let mut guard = Guard::new(&self.paths, &target.folder, self.guard.protected.clone(), self.guard.sandboxed);
        if target.device.is_some() {
            guard = guard.at_device(target.home.clone());
        }
        let (policy, _) = policy_for(&self.approval, &guard);
        let made = Arc::new((guard, policy));
        judges.insert(target.clone(), made.clone());
        made
    }

    /// Route approval requests to a front end. Without one, calls that need approval are denied.
    pub fn set_approver(&self, approver: mpsc::UnboundedSender<Approval>) {
        *self.approver.lock().unwrap() = Some(approver);
    }

    /// Where approval requests go, so a nested runtime (a delegated specialist) can ask the same person.
    pub fn approver(&self) -> Option<mpsc::UnboundedSender<Approval>> {
        self.approver.lock().unwrap().clone()
    }

    /// Tell `listener` about every approval request, waiting and closed. A listener can answer
    /// with `answer`; while one is connected, requests wait for it even without a front end.
    pub fn set_approval_listener(&self, listener: Option<mpsc::UnboundedSender<ApprovalEvent>>) {
        *self.approval_listener.lock().unwrap() = listener;
    }

    /// Answer the waiting request `id`, as its front end would; false when it is no longer waiting.
    pub fn answer(&self, id: &str, approved: bool, reason: Option<String>) -> bool {
        let Some(responder) = self.pending.lock().unwrap().get(id).map(|p| p.responder.clone()) else { return false };
        respond(&responder, if approved { Decision::Approve } else { Decision::Deny(reason) })
    }

    /// Whether a call is waiting for the user's answer: a run that waits for a person is not stuck.
    pub fn is_asking(&self) -> bool {
        !self.pending.lock().unwrap().is_empty()
    }

    /// The requests waiting now, as `Waiting` events (for a listener that just arrived).
    pub fn waiting_approvals(&self) -> Vec<ApprovalEvent> {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| ApprovalEvent::Waiting {
                id: id.clone(),
                tool: p.tool.clone(),
                summary: p.summary.clone(),
                reason: p.reason.clone(),
            })
            .collect()
    }

    fn notify(&self, event: ApprovalEvent) -> bool {
        let mut listener = self.approval_listener.lock().unwrap();
        let sent = listener.as_ref().is_some_and(|l| l.send(event).is_ok());
        if !sent {
            *listener = None;
        }
        sent
    }

    async fn ask(&self, tool: &str, summary: String, reason: String) -> Decision {
        let (sender, answer) = oneshot::channel();
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let responder: Responder = Arc::new(Mutex::new(Some(sender)));
        let pending = Pending {
            tool: tool.to_string(),
            summary: summary.clone(),
            reason: reason.clone(),
            responder: responder.clone(),
        };
        self.pending.lock().unwrap().insert(id.clone(), pending);
        let listened = self.notify(ApprovalEvent::Waiting {
            id: id.clone(),
            tool: tool.to_string(),
            summary: summary.clone(),
            reason: reason.clone(),
        });
        let request =
            ApprovalRequest { id: id.clone(), tool: tool.to_string(), summary, reason, listened, respond: responder };
        let sent = match self.approver.lock().unwrap().as_ref() {
            Some(approver) => approver.send(Approval::Asked(request)).is_ok(),
            None => false,
        };
        let decision = self.wait(tool, sent || listened, answer).await;
        self.pending.lock().unwrap().remove(&id);
        self.notify(ApprovalEvent::Closed { id: id.clone() });
        if sent && let Some(approver) = self.approver.lock().unwrap().as_ref() {
            let approved = matches!(decision, Decision::Approve);
            let _ = approver.send(Approval::Closed { id, approved });
        }
        decision
    }

    async fn wait(&self, tool: &str, asked: bool, answer: oneshot::Receiver<Decision>) -> Decision {
        if !asked {
            self.alert(Alert::new(
                AlertLevel::Warning,
                format!("{tool} needed approval, but no one can answer here; denied"),
            ));
            return Decision::Deny(Some(
                "approval is required, but no one can answer in this mode (for example --print); run interactively \
                 or through Telegram, or change approval.mode in settings"
                    .into(),
            ));
        }
        match tokio::time::timeout(self.approval_timeout, answer).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => Decision::Deny(None),
            Err(_) => {
                let why = format!("no answer within {} s", self.approval_timeout.as_secs());
                self.alert(Alert::new(AlertLevel::Warning, format!("Approval for {tool} timed out; denied")));
                Decision::Deny(Some(why))
            }
        }
    }

    pub fn set_session(&self, session_id: &str) {
        *self.session_id.lock().unwrap() = session_id.to_string();
    }

    /// Link the records that follow to `cause` (principle A4), until set to None.
    pub fn set_cause(&self, cause: Option<PulseId>) {
        *self.cause.lock().unwrap() = cause;
    }

    pub fn cause(&self) -> Option<PulseId> {
        self.cause.lock().unwrap().clone()
    }

    /// Append one agent event. Streaming deltas are skipped: the final message carries the same content.
    pub fn record_event(&self, event: &AgentEvent) {
        if matches!(event, AgentEvent::MessageUpdate { .. }) {
            return;
        }
        self.record("agent", serde_json::to_value(event).unwrap_or(Value::Null));
    }

    /// Append an agent event of another module (the chat agent), linked to that module's own cause:
    /// it runs concurrently with the worker, whose cause is the shared one.
    pub fn record_module_event(&self, source: &str, event: &AgentEvent, cause: Option<&PulseId>) {
        if matches!(event, AgentEvent::MessageUpdate { .. }) {
            return;
        }
        self.write(source, serde_json::to_value(event).unwrap_or(Value::Null), cause);
    }

    /// Append an arbitrary harness record (alerts, compactions, model changes, front-end input).
    pub fn record(&self, source: &str, payload: Value) {
        let cause = self.cause.lock().unwrap().clone();
        self.write(source, payload, cause.as_ref());
    }

    fn write(&self, source: &str, payload: Value, cause: Option<&PulseId>) {
        let mut record = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "session": *self.session_id.lock().unwrap(),
            "seq": self.seq.fetch_add(1, Ordering::SeqCst),
            "source": source,
            "payload": payload,
        });
        if let Some(cause) = cause {
            record["cause"] = json!(cause);
        }
        let _guard = self.file_lock.lock().unwrap();
        let file = self.audit_dir.join(format!("{}.jsonl", chrono::Local::now().format("%Y-%m-%d")));
        let written = std::fs::create_dir_all(&self.audit_dir)
            .and_then(|_| OpenOptions::new().create(true).append(true).open(&file))
            .and_then(|mut f| writeln!(f, "{record}"));
        if let Err(error) = written {
            // Losing the audit trail is itself an alert-worthy event.
            let _ = self.alerts.send(Alert::new(AlertLevel::Error, format!("Audit log write failed: {error}")));
        }
    }

    /// Raise an alert: recorded, then delivered to the front end.
    pub fn alert(&self, alert: Alert) {
        self.record("alert", serde_json::to_value(&alert).unwrap_or(Value::Null));
        let _ = self.alerts.send(alert);
    }
}

/// The Inspector sees every pulse, synchronously and in publish order (decision D17).
impl PulseTap for Inspector {
    fn observe(&self, pulse: &Pulse) {
        self.record("pulse", serde_json::to_value(pulse).unwrap_or(Value::Null));
    }
}

impl Inspector {
    /// Whether a call may run with nobody asked (decision D84): `Err` with a reason the model
    /// reads when the guard refuses it or when the approval policy would ask the owner. For the
    /// chat module's reads, which must never wait on a person.
    pub fn allowed_unasked(&self, tool: &str, arguments: &Value) -> Result<(), String> {
        let router = self.router.lock().unwrap().clone();
        let routed = router.and_then(|router| router(tool, arguments));
        let elsewhere = routed.as_ref().and_then(|judged| judged.target.as_ref());
        let judges = elsewhere.map(|target| self.judges_at(target));
        let (guard, policy) = match &judges {
            Some(judges) => (&judges.0, &judges.1),
            None => (&self.guard, &self.policy),
        };
        if let Some(reason) = guard.check(tool, arguments) {
            return Err(format!("{tool} is not permitted: {reason}"));
        }
        if policy.check(tool, arguments).or_else(|| self.added_reason(tool)).is_some() {
            return Err("Reading this path requires approval; signal the Principal to inspect it in a task".into());
        }
        Ok(())
    }
}

#[async_trait]
impl ToolHooks for Inspector {
    async fn before_tool_call(&self, call: &ToolCall, arguments: &Value) -> Option<Block> {
        // A call that is carried out elsewhere is judged for where it goes: the same rules,
        // seated at that folder and that device. `on_device` is judged as the call it carries.
        let router = self.router.lock().unwrap().clone();
        let routed = router.and_then(|router| router(&call.name, arguments));
        let elsewhere = routed.as_ref().and_then(|judged| judged.target.as_ref());
        let judges = elsewhere.map(|target| self.judges_at(target));
        let (guard, policy) = match &judges {
            Some(judges) => (&judges.0, &judges.1),
            None => (&self.guard, &self.policy),
        };
        let carried = routed.as_ref().and_then(|judged| judged.inner.as_ref());
        let (tool, judged_arguments) = match carried {
            Some((tool, inner)) => (tool.as_str(), inner),
            None => (call.name.as_str(), arguments),
        };
        let on = elsewhere.and_then(|target| target.device.as_ref()).map(|device| format!(" on {device}"));
        let on = on.unwrap_or_default();
        if let Some(reason) = guard.check(tool, judged_arguments) {
            let message = format!("Inspector blocked {tool} call{on}: {reason}");
            self.alert(Alert::new(AlertLevel::Warning, message.clone()));
            return Some(Block {
                reason: format!(
                    "{message}. This action is not permitted. If it is truly required, explain why to the user and ask them to run it themselves."
                ),
                terminate: false,
            });
        }
        let reason = policy.check(tool, judged_arguments).or_else(|| self.added_reason(tool))?;
        let summary = format!("{}{on}", call_summary(tool, judged_arguments));
        self.record(
            "approval",
            json!({ "request": { "callId": call.id, "tool": call.name, "summary": summary, "reason": reason } }),
        );
        let decision = self.ask(&call.name, summary, reason.clone()).await;
        let (approved, note) = match &decision {
            Decision::Approve => (true, None),
            Decision::Deny(note) => (false, note.clone()),
        };
        self.record("approval", json!({ "decision": { "callId": call.id, "approved": approved, "note": note } }));
        if approved {
            return None;
        }
        let note = note.map(|n| format!(" Reason: {n}.")).unwrap_or_default();
        Some(Block {
            reason: format!(
                "The user did not approve this {} call ({reason}).{note} Do not retry it unchanged; tell the user what you \
                 wanted to do and ask how to proceed.",
                call.name
            ),
            terminate: false,
        })
    }
}

/// The guard: what never runs. Its rules for commands are deliberately conservative: they
/// target what is catastrophic and never routine, so normal development is not interrupted.
/// Its rules for files are exact for the tools that name a path (`write`, `edit`, `read`, and
/// the search tools): the path is resolved as the tool resolves it and followed through
/// symlinks, then checked against `Protected`. A `bash` command only has text to check, which a
/// shell can spell endlessly; where the tool sandbox is active the kernel does that job and the
/// text is not second-guessed, elsewhere a mention of a protected folder beside anything that
/// writes is refused.
pub struct Guard {
    dangerous_commands: Vec<(Regex, &'static str)>,
    /// Where the tools it checks run. On another device, paths are that device's and its own
    /// guard (the node's gate) judges them on its own disk; here only commands are judged.
    disk: Disk,
    protected: Protected,
    /// The directory the tools resolve relative paths against.
    cwd: PathBuf,
    /// The tool sandbox enforces `protected` on subprocesses.
    sandboxed: bool,
    /// NyPi's home as a command may spell it: as configured, and with symlinks resolved.
    home_spellings: Vec<String>,
    /// Secret files and folders as an unsandboxed command may spell them.
    secret_spellings: Vec<String>,
    agents_dir: PathBuf,
    goals: PathBuf,
}

/// Program names and shell syntax that change files, for the unsandboxed `bash` check.
const WRITERS: [&str; 20] = [
    ">", "rm ", "mv ", "cp ", "tee", "sed -i", "perl -i", "truncate", "chmod", "chown", "ln ", "dd ", "install ",
    "touch ", "mkdir", "rmdir", "unlink", "rsync", "-delete", "open(",
];

/// Which machine's disk a guard's paths are on.
#[derive(Clone, Debug, PartialEq)]
enum Disk {
    Here,
    /// Another device, with the user's home there when its node said.
    Device {
        home: Option<PathBuf>,
    },
}

impl Guard {
    /// The guard for tools that run on another device (`cwd` is a folder there): it refuses
    /// what is never acceptable anywhere, and leaves paths to that device's own guard, since
    /// following them on this computer's disk would judge the wrong files. The approval policy
    /// takes the device's home for `~` and its shell and security setup.
    pub fn at_device(mut self, home: Option<PathBuf>) -> Self {
        self.disk = Disk::Device { home };
        self
    }

    pub fn new(paths: &Paths, cwd: &Path, protected: Protected, sandboxed: bool) -> Self {
        let rules: [(&str, &str); 9] = [
            (
                r#"\brm\s+(-[a-zA-Z]*\s+)*-[a-zA-Z]*[rR][a-zA-Z]*\s+(-[a-zA-Z]+\s+)*["']?(/|/\*|~|~/|~/\*|\$HOME/?|\$\{HOME\}/?)["']?(\s|$|;|&|\|)"#,
                "recursive delete of the root or home directory",
            ),
            (r"\bmkfs(\.\w+)?\b", "formatting a filesystem"),
            (r"\bdd\b[^|;&]*\bof=/dev/", "writing raw data to a device"),
            (r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;\s*:", "fork bomb"),
            (r"\bchmod\s+(-[a-zA-Z]*R[a-zA-Z]*\s+)0?777\s+/(\s|$)", "making the whole filesystem world-writable"),
            (r">\s*/dev/(sd[a-z]|disk\d|nvme\d)", "overwriting a block device"),
            // As a command, not as a word in one (`grep -r shutdown src` is fine).
            (
                r"(^|[;&|]\s*|\bsudo\s+)(shutdown|reboot|halt|poweroff)\b(\s|$)",
                "shutting down or rebooting the machine",
            ),
            (r"\bdiskutil\s+(erase|zeroDisk|secureErase)", "erasing a disk"),
            (
                // `nypi` by any path, with global flags (and their values) before the subcommand
                // (decision D99): `~/.nypi/bin/nypi --mode text update` is still NyPi's own.
                r#"(^|[\s/;&|(`'"])nypi(\s+--?[A-Za-z][\w-]*(=\S+|\s+[^\s\-;&|][^\s;&|]*)?)*\s+(trust|rate|send|attach|node|hub|relay|devices|pair|secrets|supervisor|update)\b"#,
                "deciding what NyPi trusts, rating its work, speaking to the hub as the user, admitting \
                 machines, handling its secrets, starting, stopping, or moving its hub, node, or relay, or \
                 replacing its own program (`nypi update`), which only the user does",
            ),
        ];
        let dangerous_commands = rules.iter().map(|(pattern, why)| (Regex::new(pattern).unwrap(), *why)).collect();
        let secret_spellings = secret_spellings(paths, &protected);
        Guard {
            dangerous_commands,
            disk: Disk::Here,
            protected,
            cwd: cwd.to_path_buf(),
            sandboxed,
            home_spellings: vec![paths.home.display().to_string(), real_path(&paths.home).display().to_string()],
            secret_spellings,
            agents_dir: real_path(&paths.home.join("agents")),
            goals: real_path(&paths.goals()),
        }
    }

    /// The path a tool would reach for this argument.
    fn target(&self, arguments: &Value, key: &str) -> PathBuf {
        let raw = arguments[key].as_str().unwrap_or(".");
        real_path(&crate::tools::resolve_path(&self.cwd, raw))
    }

    /// The path `read` would reach: it also tries the name forms macOS stores, so the guard
    /// must look at the file those find, not at the spelling.
    fn read_target(&self, arguments: &Value) -> PathBuf {
        let raw = arguments["path"].as_str().unwrap_or(".");
        real_path(&crate::tools::resolve_read_path(&self.cwd, raw))
    }

    /// Why this call must not run, or None when it is allowed.
    pub fn check(&self, tool: &str, arguments: &Value) -> Option<String> {
        if let Some(command) = scheduled_command(tool, arguments) {
            return self.check("bash", &json!({ "command": command }));
        }
        match tool {
            "bash" => {
                let command = arguments["command"].as_str().unwrap_or_default();
                if let Some((_, why)) = self.dangerous_commands.iter().find(|(re, _)| re.is_match(command)) {
                    return Some(format!("{why} (`{command}`)"));
                }
                if self.sandboxed || self.disk != Disk::Here {
                    return None;
                }
                // No sandbox: all there is to go on is the text. `.nypi` covers `~/.nypi`,
                // `$HOME/.nypi`, `cd ~/.nypi`, and a project's folder alike.
                if mentions_secret(command, &self.secret_spellings) {
                    return Some("a command that names keys or tokens, which tools never read".to_string());
                }
                let mentions =
                    command.contains(".nypi") || self.home_spellings.iter().any(|home| command.contains(home));
                (mentions && WRITERS.iter().any(|writer| command.contains(writer))).then(|| {
                    "a command that names NyPi's own folder (or a .nypi folder) and can write: those are changed only \
                     by the harness and the user"
                        .to_string()
                })
            }
            _ if self.disk != Disk::Here => None,
            "write" | "edit" => {
                let path = self.target(arguments, "path");
                if path.file_name().is_some_and(|n| n == "self.md") && path.starts_with(&self.agents_dir) {
                    return Some(
                        "the self prompt changes only through the self_prompt tool, which versions every edit".into(),
                    );
                }
                if path == self.goals {
                    return Some(
                        "the goals file changes only through the goals tool, which versions every edit".into(),
                    );
                }
                self.protected.write_denied(&path)
            }
            "read" => self.protected.read_denied(&self.read_target(arguments)),
            "grep" | "find" | "ls" => self.protected.read_denied(&self.target(arguments, "path")),
            // A specialist works where it is sent; NyPi's own folders are no workspace.
            "delegate" => arguments["cwd"].as_str().and_then(|_| {
                self.protected
                    .write_denied(&self.target(arguments, "cwd"))
                    .map(|why| format!("a specialist cannot be given that folder to work in: {why}"))
            }),
            _ => None,
        }
    }
}

/// The command a `schedule` call would run unattended later, checked now as if it ran now.
fn scheduled_command<'a>(tool: &str, arguments: &'a Value) -> Option<&'a str> {
    (tool == "schedule" && arguments["action"] == "add" && arguments["trigger"]["kind"] == "command")
        .then(|| arguments["trigger"]["command"].as_str())
        .flatten()
}

fn secret_spellings(paths: &Paths, protected: &Protected) -> Vec<String> {
    let mut out = Vec::new();
    for secret in &protected.secrets {
        out.push(secret.display().to_string());
        if let Ok(rest) = secret.strip_prefix("/private") {
            out.push(Path::new("/").join(rest).display().to_string());
        }
        if let Ok(rel) = secret.strip_prefix(&protected.home) {
            out.push(paths.home.join(rel).display().to_string());
            out.push(format!(".nypi/{}", rel.display()));
        }
        if let Some(user) = &protected.user_home
            && let Ok(rel) = secret.strip_prefix(user)
        {
            out.push(format!("~/{}", rel.display()));
            out.push(format!("$HOME/{}", rel.display()));
            out.push(format!("${{HOME}}/{}", rel.display()));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn mentions_secret(command: &str, spellings: &[String]) -> bool {
    spellings.iter().any(|spelling| {
        !spelling.is_empty()
            && command.match_indices(spelling.as_str()).any(|(idx, _)| {
                let before = command[..idx].chars().next_back();
                let after = command[idx + spelling.len()..].chars().next();
                let left_ok = before.is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
                let right_ok = after.is_none_or(|c| {
                    matches!(c, '/' | ' ' | '\t' | '\n' | '\r' | '\'' | '"' | '`' | ';' | '&' | '|' | ')' | '<' | '>')
                });
                left_ok && right_ok
            })
    })
}

/// Tools that never need approval in `all` mode: they only read, or (self_prompt) version every edit.
const UNGATED_TOOLS: [&str; 8] =
    ["read", "grep", "find", "ls", "self_prompt", "vault_search", "vault_read", "vault_list"];

/// Which calls need the user's approval. Unlike the guard, a match is a question, not a refusal,
/// so the rules may cover routine-but-consequential actions (pushing, publishing, sudo).
pub struct Policy {
    mode: ApprovalMode,
    commands: Vec<(Regex, String)>,
    tools: globset::GlobSet,
    /// Absolute path plus the home-relative form commands usually spell.
    sensitive_files: Vec<(PathBuf, String)>,
    /// The directory the tools resolve relative paths against.
    cwd: PathBuf,
    /// The user's home on the machine the tools run on, where `~` points.
    home: PathBuf,
    /// Credential scopes trusted for hosts (`approval.trustedCredentials`).
    trusted_credentials: std::collections::BTreeMap<String, Vec<String>>,
    /// The tools run on this computer, so paths are followed on its disk.
    local: bool,
}

/// The files and folders that hold a user's shell and security setup, under their home.
const SENSITIVE_FILES: [&str; 10] = [
    ".ssh",
    ".gnupg",
    ".zshrc",
    ".zprofile",
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".gitconfig",
    "Library/LaunchAgents",
    ".config/fish",
];

fn sensitive_files(home: &Path) -> Vec<(PathBuf, String)> {
    SENSITIVE_FILES.iter().map(|rel| (home.join(rel), rel.to_string())).collect()
}

impl Policy {
    /// The policy plus any user rules that did not parse.
    pub fn new(settings: &ApprovalSettings) -> (Self, Vec<String>) {
        let builtin: [(&str, &str); 12] = [
            (r"\bgit\s+push\b", "pushing to a remote repository"),
            (
                r"\bgit\s+(reset\s+--hard|clean\s+(-\w+\s+)*-\w*f\w*|branch\s+-D|stash\s+(drop|clear)|filter-branch|filter-repo)\b",
                "discarding git history or uncommitted work",
            ),
            (r"(^|[;&|]\s*|\s)sudo\s", "running a command as root"),
            (r"\b(curl|wget)\b[^|;&]*\|\s*(sudo\s+)?(ba|z|da)?sh\b", "running a script downloaded from the internet"),
            (r"\b(npm|pnpm|yarn|cargo|gem|twine|poetry)\s+(publish|upload)\b", "publishing a package"),
            (
                r"\bgh\s+(pr\s+(create|merge|close|comment|review)|issue\s+(create|close|comment)|release\s+create|repo\s+(create|delete|edit))\b",
                "acting on GitHub in the user's name",
            ),
            (r"\b(pkill|killall)\b|\bkill\s+-(9|KILL)\b", "killing processes"),
            (
                r"\b(docker\s+(system|volume|image)\s+prune|docker\s+volume\s+rm|kubectl\s+delete|terraform\s+(apply|destroy)|helm\s+(uninstall|delete))\b",
                "changing or deleting infrastructure",
            ),
            (r"(?i)\b(drop\s+(table|database|schema)|truncate\s+table)\b", "destroying database data"),
            (
                r"\blaunchctl\s+(load|bootstrap|enable)\b|\bcrontab\s+(-[er]|[^-\s])|\bsystemctl\s+(enable|start)\b",
                "installing or removing a background job",
            ),
            (r"\b(sendmail|mutt)\b|\bmail\s+-s\b", "sending e-mail"),
            (r"\bscp\s|\brsync\b[^;&|]*\s[\w.-]+@?[\w.-]+:", "copying files to another machine"),
        ];
        let mut commands: Vec<(Regex, String)> =
            builtin.iter().map(|(pattern, why)| (Regex::new(pattern).unwrap(), why.to_string())).collect();
        let mut invalid = Vec::new();
        for pattern in &settings.commands {
            match Regex::new(pattern) {
                Ok(re) => commands.push((re, format!("matches your approval rule `{pattern}`"))),
                Err(_) => invalid.push(pattern.clone()),
            }
        }
        let mut tools = globset::GlobSetBuilder::new();
        for pattern in &settings.tools {
            match globset::Glob::new(pattern) {
                Ok(glob) => {
                    tools.add(glob);
                }
                Err(_) => invalid.push(pattern.clone()),
            }
        }
        let home = dirs::home_dir().unwrap_or_default();
        let policy = Policy {
            mode: settings.mode,
            commands,
            tools: tools.build().unwrap_or_else(|_| globset::GlobSet::empty()),
            sensitive_files: sensitive_files(&home),
            cwd: PathBuf::from("/"),
            home,
            trusted_credentials: settings.trusted_credentials.clone(),
            local: true,
        };
        (policy, invalid)
    }

    /// Why this call needs approval, or None when it may run unasked.
    pub fn check(&self, tool: &str, arguments: &Value) -> Option<String> {
        // Using a secret is asked about whatever the mode, unless the owner trusted that secret's
        // scope for this host: turning approvals off for commands is not handing over the keys.
        if tool == "http_request"
            && let Some(why) = self.credential_use(arguments)
        {
            return Some(why);
        }
        if self.mode == ApprovalMode::Off {
            return None;
        }
        if self.tools.is_match(tool) {
            return Some(format!("{tool} is on your approval list"));
        }
        if self.mode == ApprovalMode::All && !UNGATED_TOOLS.contains(&tool) {
            return Some("approval mode is `all`".into());
        }
        if let Some(command) = scheduled_command(tool, arguments) {
            return self
                .check("bash", &json!({ "command": command }))
                .map(|why| format!("{why} (scheduled to run later)"));
        }
        match tool {
            "bash" => {
                let command = arguments["command"].as_str().unwrap_or_default();
                if let Some((_, why)) = self.commands.iter().find(|(re, _)| re.is_match(command)) {
                    return Some(why.clone());
                }
                if let Some(target) = recursive_delete_outside_project(command) {
                    return Some(format!("recursively deleting {target}, outside the working directory"));
                }
                if !writes_to(command) {
                    return None;
                }
                self.sensitive_files
                    .iter()
                    .find(|(abs, rel)| {
                        command.contains(&abs.display().to_string())
                            || command.contains(&format!("~/{rel}"))
                            || command.contains(&format!("$HOME/{rel}"))
                    })
                    .map(|(abs, _)| format!("changing {}, which affects your shell or security setup", abs.display()))
            }
            "write" | "edit" => {
                // The file the tool would really reach, as the guard resolves it; on another
                // device, as spelled there (its links are on its own disk).
                let raw = arguments["path"].as_str().unwrap_or_default();
                let spelled = crate::tools::resolve_path_in(&self.cwd, raw, &self.home);
                let real = |path: &Path| if self.local { real_path(path) } else { lexical_path(path) };
                let path = real(&spelled);
                self.sensitive_files
                    .iter()
                    .find(|(abs, _)| path.starts_with(real(abs)))
                    .map(|(abs, _)| format!("changing {}, which affects your shell or security setup", abs.display()))
            }
            _ => None,
        }
    }
}

impl Policy {
    /// Why an `http_request` call's use of a credential needs the owner's yes, or None when it
    /// names no credential or the settings trust it for the host. A call that names one badly
    /// is left to the tool, which refuses it with a message the model can act on.
    fn credential_use(&self, arguments: &Value) -> Option<String> {
        let auth = crate::tools::http_request::Auth::read(arguments).ok().flatten()?;
        let url = arguments["url"].as_str().unwrap_or_default();
        let host = crate::tools::http_request::host_of(url)?;
        let handle = auth.handle();
        if crate::tools::http_request::scope_trusted(&self.trusted_credentials, &handle.scope, &host) {
            return None;
        }
        Some(format!("using the secret {handle} in a request to {host}"))
    }
}

fn writes_to(command: &str) -> bool {
    [">", "rm ", "mv ", "cp ", "tee ", "sed -i", "truncate ", "chmod ", "ln "].iter().any(|w| command.contains(w))
}

/// The first target of an `rm -r` that leaves the project: absolute (other than temp
/// directories), home-relative, or a parent directory.
fn recursive_delete_outside_project(command: &str) -> Option<String> {
    for segment in command.split(['&', ';', '|', '\n']) {
        let mut words = segment.split_whitespace().skip_while(|w| *w != "rm");
        if words.next().is_none() {
            continue;
        }
        let words: Vec<&str> = words.collect();
        let recursive = words
            .iter()
            .any(|w| (w.starts_with('-') && !w.starts_with("--") && w.contains(['r', 'R'])) || *w == "--recursive");
        if !recursive {
            continue;
        }
        let temp = ["/tmp/", "/private/tmp/", "/var/folders/", "/private/var/folders/"];
        if let Some(target) = words.iter().find(|w| {
            !w.starts_with('-')
                && (w.starts_with('~')
                    || w.starts_with("$HOME")
                    || w.starts_with("${HOME}")
                    || w.starts_with("..")
                    || (w.starts_with('/') && !temp.iter().any(|t| w.starts_with(t))))
        }) {
            return Some(target.to_string());
        }
    }
    None
}

/// Short, human-readable form of a call for approval prompts.
pub fn call_summary(tool: &str, arguments: &Value) -> String {
    let text = match tool {
        "bash" => arguments["command"].as_str().unwrap_or_default().to_string(),
        "write" | "edit" | "read" => arguments["path"].as_str().unwrap_or_default().to_string(),
        "http_request" => format!(
            "{} {}{}",
            arguments["method"].as_str().unwrap_or("GET"),
            arguments["url"].as_str().unwrap_or_default(),
            arguments["auth"]["cred"]
                .as_str()
                .or(arguments["auth"]["passwordCred"].as_str())
                .map(|cred| format!(" with {cred}"))
                .unwrap_or_default()
        ),
        _ => arguments.to_string(),
    };
    let mut short: String = text.chars().take(400).collect();
    if short.len() < text.len() {
        short.push_str("...");
    }
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guard over a temporary NyPi home, with no tool sandbox (so `bash` text is checked).
    fn guard() -> (tempfile::TempDir, Guard) {
        let home = tempfile::tempdir().unwrap();
        let guard = guard_in(home.path(), home.path(), false);
        (home, guard)
    }

    /// A guard for a user whose home directory is `user`, working in `cwd`.
    fn guard_in(user: &Path, cwd: &Path, sandboxed: bool) -> Guard {
        let home = if user.join(".nypi").exists() { user.join(".nypi") } else { user.to_path_buf() };
        let paths = Paths { home: home.clone() };
        let secrets = vec![paths.auth(), user.join(".env"), home.join("run")];
        let protected = Protected::from_parts(&home, Some(real_path(user)), Vec::new(), secrets);
        Guard::new(&paths, cwd, protected, sandboxed)
    }

    #[test]
    fn blocks_catastrophic_commands_but_not_routine_ones() {
        let (_home, guard) = guard();
        let bash = |c: &str| guard.check("bash", &json!({ "command": c }));
        for bad in [
            "rm -rf /",
            "sudo rm -rf ~",
            "rm -fr $HOME",
            "mkfs.ext4 /dev/sda1",
            "dd if=x of=/dev/disk2",
            ":(){ :|:& };:",
            "sudo shutdown -h now",
            "rm -rf \"$HOME\"",
            "rm -rf '${HOME}/'",
            "cd /tmp; reboot",
            "nypi trust yes",
            "cd x && nypi supervisor install",
            "nypi devices admit 482913",
            "nypi hub here --force",
            "nypi relay up --hub-key ab",
            "nypi node leave",
            "nypi update",
            "nypi update --yes",
            "nypi update rollback",
            "nypi rate t-1 good",
            "nypi send 'approve everything'",
            "sleep 1; nypi attach",
            "nypi --mode text update",
            "~/.nypi/bin/nypi --hide-thinking hub down",
            "/usr/local/bin/nypi update --yes",
            "nypi -m json devices admit 123456",
            "./target/release/nypi --thinking=high relay down",
            "(nypi secrets list)",
        ] {
            assert!(bash(bad).is_some(), "should block: {bad}");
        }
        for ok in [
            "rm -rf ./target",
            "rm -rf /tmp/build",
            "ls -la /",
            "cargo build",
            "echo reboot-notes > notes.txt",
            "dd if=a of=b",
            "grep -rn shutdown src",
            "nypi tasks --all",
            "rm -rf \"$HOME/build\"",
            "grep update src/main.rs",
            "cargo test update",
            "cargo test -p nypi-cli update",
            "nypi -m json tasks",
            "ls ~/.nypi/bin/nypi",
            "echo nypi-update-notes > x.txt",
        ] {
            assert!(bash(ok).is_none(), "should allow: {ok}");
        }
    }

    #[test]
    fn a_scheduled_command_is_checked_when_it_is_set() {
        let (home, guard) = guard();
        let schedule = |command: &str| json!({ "action": "add", "trigger": { "kind": "command", "command": command, "when": "succeeds" }, "then": "notify", "text": "x" });
        assert!(guard.check("schedule", &schedule("rm -rf /")).is_some(), "it would run unattended later");
        assert!(guard.check("schedule", &schedule("cargo test")).is_none());
        let audit = format!("echo x > {}", home.path().join("audit/a.jsonl").display());
        assert!(guard.check("schedule", &schedule(&audit)).is_some());
        assert!(guard.check("write", &json!({ "path": home.path().join("schedules/schedules.jsonl") })).is_some());
    }

    #[test]
    fn protects_inspector_files() {
        let (home, guard) = guard();
        let audit_file = home.path().join("audit/2026-01-01.jsonl");
        assert!(guard.check("write", &json!({ "path": audit_file })).is_some());
        assert!(guard.check("edit", &json!({ "path": home.path().join("prompt/core.md") })).is_some());
        let own = guard.check("write", &json!({ "path": home.path().join("agents/lucia/self.md") })).unwrap();
        assert!(own.contains("self_prompt tool"), "{own}");
        let goals = guard.check("edit", &json!({ "path": home.path().join("goals.md") })).unwrap();
        assert!(goals.contains("goals tool"), "{goals}");
        // Everything in NyPi's home is the harness's or the user's, not only the files listed once.
        for inside in ["agents/lucia/notes.md", "settings.json", "trust.json", "specialists/x/profile.json"] {
            assert!(guard.check("write", &json!({ "path": home.path().join(inside) })).is_some(), "{inside}");
        }
        let rm = format!("rm {}", audit_file.display());
        assert!(guard.check("bash", &json!({ "command": rm })).is_some());
    }

    #[test]
    fn a_path_is_judged_as_the_tool_would_resolve_it() {
        let user = tempfile::tempdir().unwrap();
        let work = user.path().join("code/site");
        std::fs::create_dir_all(user.path().join(".nypi/audit")).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::os::unix::fs::symlink(user.path().join(".nypi"), work.join("vendor")).unwrap();
        let guard = guard_in(user.path(), &work, true);
        let write = |path: &str| guard.check("write", &json!({ "path": path }));
        let auth = user.path().join(".nypi/auth.json").display().to_string();
        for sneaky in [
            format!(" {auth}"), // the tool trims; so does the guard
            format!("{}/../../.nypi/telegram.json", work.display()),
            "../../.nypi/settings.json".to_string(), // relative to the tool's directory
            "vendor/trust.json".to_string(),         // through a symlink
            ".nypi/settings.json".to_string(),       // the project's own configuration
            "./.nypi/specialists/x/profile.json".to_string(),
        ] {
            assert!(write(&sneaky).is_some(), "must be refused: {sneaky:?}");
        }
        assert!(write("src/main.rs").is_none());
        assert!(write("docs/nypi-notes.md").is_none());
    }

    #[test]
    fn secrets_are_not_read_and_nypi_folders_are_no_workspace() {
        let user = tempfile::tempdir().unwrap();
        let work = user.path().join("code");
        std::fs::create_dir_all(user.path().join(".nypi/run")).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(user.path().join(".env"), "KEY=1").unwrap();
        let guard = guard_in(user.path(), &work, true);
        assert!(guard.check("read", &json!({ "path": "../.env" })).is_some());
        let auth = user.path().join(".nypi/auth.json");
        assert!(guard.check("read", &json!({ "path": auth })).is_some());
        assert!(guard.check("grep", &json!({ "pattern": "token", "path": user.path().join(".nypi/run") })).is_some());
        assert!(guard.check("read", &json!({ "path": "notes.md" })).is_none());
        assert!(guard.check("ls", &json!({})).is_none(), "no path means the working directory");

        let home = user.path().join(".nypi");
        let refused = guard.check("delegate", &json!({ "specialist": "coder", "task": "x", "cwd": home })).unwrap();
        assert!(refused.contains("cannot be given that folder"), "{refused}");
        assert!(guard.check("delegate", &json!({ "specialist": "coder", "task": "x", "cwd": work })).is_none());
        assert!(guard.check("delegate", &json!({ "specialist": "coder", "task": "x" })).is_none());
    }

    #[test]
    fn without_a_sandbox_a_command_naming_a_nypi_folder_may_not_write() {
        let user = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(user.path().join(".nypi")).unwrap();
        let open = guard_in(user.path(), user.path(), false);
        let bash = |guard: &Guard, c: &str| guard.check("bash", &json!({ "command": c }));
        for sneaky in [
            "rm -f ~/.nypi/audit/*.jsonl",
            "cd ~/.nypi && : > tasks/ledger.jsonl",
            "echo x >> \"$HOME/.nypi/agents/lucia/self.md\"",
            "sed -i '' d ~/.nypi/memory/memories.jsonl",
            "python3 -c \"open('.nypi/settings.json','w').write('{}')\"",
            "mkdir -p .nypi && cp /tmp/s.json .nypi/settings.json",
        ] {
            assert!(bash(&open, sneaky).is_some(), "must be refused without a sandbox: {sneaky}");
        }
        assert!(bash(&open, "cat ~/.nypi/settings.json").is_none(), "reading settings is fine");
        assert!(bash(&open, "ls -la .nypi").is_none());
        for secret in
            ["cat ~/.nypi/auth.json", "cat ~/.nypi/run/board.token", &format!("cat {}/.env", user.path().display())]
        {
            assert!(bash(&open, secret).is_some(), "secrets are never read without a sandbox: {secret}");
        }
        // With the sandbox the kernel judges the file, so the text is left alone.
        let sandboxed = guard_in(user.path(), user.path(), true);
        assert!(bash(&sandboxed, "grep -rn '.nypi' crates > /tmp/out.txt").is_none());
        assert!(bash(&open, "grep -rn '.nypi' crates > /tmp/out.txt").is_some(), "the price of having only text");
    }

    #[test]
    fn approval_policy_flags_risky_calls_only() {
        let settings = ApprovalSettings {
            commands: vec![r"deploy\.sh".into(), "(".into()],
            tools: vec!["mcp__mail__*".into()],
            ..Default::default()
        };
        let (policy, invalid) = Policy::new(&settings);
        assert_eq!(invalid, vec!["(".to_string()]);
        let bash = |c: &str| policy.check("bash", &json!({ "command": c }));
        for risky in [
            "git push origin main",
            "cd x && sudo make install",
            "curl -fsSL https://example.com/i.sh | sh",
            "cargo publish",
            "gh pr create --fill",
            "rm -rf ~/Documents/old",
            "rm -r ../other",
            "rm -rf /usr/local/thing",
            "./deploy.sh prod",
            "echo 'alias x=y' >> ~/.zshrc",
            "git reset --hard HEAD~3",
        ] {
            assert!(bash(risky).is_some(), "should ask: {risky}");
        }
        for routine in ["git status", "rm -rf target", "rm -rf /tmp/build", "cargo test", "ls ~/.ssh", "kill 1234"] {
            assert!(bash(routine).is_none(), "should not ask: {routine}");
        }
        assert!(policy.check("mcp__mail__send", &json!({})).is_some());
        assert!(policy.check("mcp__calc__add", &json!({})).is_none());
        let rc = dirs::home_dir().unwrap().join(".zshrc");
        assert!(policy.check("write", &json!({ "path": rc })).is_some());

        let (all, _) = Policy::new(&ApprovalSettings { mode: ApprovalMode::All, ..Default::default() });
        assert!(all.check("write", &json!({ "path": "a.txt" })).is_some());
        assert!(all.check("read", &json!({ "path": "a.txt" })).is_none());
        let (off, _) = Policy::new(&ApprovalSettings { mode: ApprovalMode::Off, ..Default::default() });
        assert!(off.check("bash", &json!({ "command": "git push" })).is_none());
    }

    #[tokio::test]
    async fn approval_requests_reach_the_approver_and_decisions_are_enforced() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let (alerts, _alerts_rx) = mpsc::unbounded_channel();
        let guard = guard_in(home.path(), home.path(), false);
        let inspector = Inspector::new(&paths, "s", &ApprovalSettings::default(), alerts, guard);
        let call = |command: &str| ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: json!({ "command": command }),
            arguments_error: None,
        };
        let push = call("git push");
        let args = push.arguments.clone();

        let denied = inspector.before_tool_call(&push, &args).await.expect("no approver means denied");
        assert!(denied.reason.contains("no one can answer"), "{}", denied.reason);

        let (tx, mut rx) = mpsc::unbounded_channel::<Approval>();
        inspector.set_approver(tx);
        let asked = |approval: Option<Approval>| match approval {
            Some(Approval::Asked(request)) => request,
            other => panic!("expected a request: {other:?}"),
        };
        let answers = tokio::spawn(async move {
            let first = asked(rx.recv().await);
            assert_eq!(first.summary, "git push");
            let id = first.id.clone();
            first.approve();
            assert!(matches!(rx.recv().await, Some(Approval::Closed { id: closed, approved: true }) if closed == id));
            asked(rx.recv().await).deny(Some("not today".into()));
            assert!(matches!(rx.recv().await, Some(Approval::Closed { approved: false, .. })));
            rx
        });
        assert!(inspector.before_tool_call(&push, &args).await.is_none(), "approved");
        let block = inspector.before_tool_call(&push, &args).await.expect("denied");
        assert!(block.reason.contains("not today"), "{}", block.reason);
        let mut rx = answers.await.unwrap();

        // Answered on the board (a listener), the approver learns the question is gone, under
        // the same id it was asked with.
        let (board, mut shown) = mpsc::unbounded_channel();
        inspector.set_approval_listener(Some(board));
        let inspector = Arc::new(inspector);
        let board_answer = {
            let inspector = inspector.clone();
            tokio::spawn(async move {
                let Some(ApprovalEvent::Waiting { id, .. }) = shown.recv().await else { panic!("not shown") };
                assert!(inspector.answer(&id, true, None));
                id
            })
        };
        assert!(inspector.before_tool_call(&push, &args).await.is_none(), "approved on the board");
        let id = board_answer.await.unwrap();
        let request = asked(rx.recv().await);
        assert!(request.listened && request.id == id, "one id everywhere: {request:?}");
        assert!(matches!(rx.recv().await, Some(Approval::Closed { id: closed, approved: true }) if closed == id));

        let routine = call("cargo test");
        assert!(inspector.before_tool_call(&routine, &routine.arguments.clone()).await.is_none());
        let audit = std::fs::read_dir(paths.audit()).unwrap().next().unwrap().unwrap().path();
        assert_eq!(std::fs::read_to_string(audit).unwrap().matches("\"approved\":").count(), 4);
    }

    /// At another device the paths are that device's: this computer's disk and home say
    /// nothing about them, the device's home decides what is its shell and security setup, and
    /// commands that are never acceptable are still refused here.
    #[tokio::test]
    async fn at_another_device_paths_are_judged_by_that_devices_home() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let guard = guard_in(home.path(), Path::new("/srv/site"), false).at_device(Some("/home/ubuntu".into()));
        assert!(guard.check("bash", &json!({ "command": "mkfs.ext4 /dev/sda1" })).is_some());
        let here = home.path().join("settings.json").display().to_string();
        assert!(guard.check("write", &json!({ "path": here })).is_none(), "the device's own guard judges its paths");
        let (alerts, _rx) = mpsc::unbounded_channel();
        let inspector = Inspector::new(&paths, "s", &ApprovalSettings::default(), alerts, guard);
        for path in ["~/.ssh/authorized_keys", "/home/ubuntu/.bashrc", "../../home/ubuntu/.profile"] {
            let why = inspector.policy.check("write", &json!({ "path": path }));
            assert!(why.is_some_and(|why| why.contains("/home/ubuntu/")), "{path} is the device's setup");
        }
        assert!(inspector.policy.check("write", &json!({ "path": "notes.md" })).is_none());
        assert!(inspector.policy.check("bash", &json!({ "command": "echo key >> ~/.ssh/authorized_keys" })).is_some());
    }
}
