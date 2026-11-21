//! What every way of talking to Lucia needs, done once per thread (decision D67, part 2): the
//! records a thread emits go to every attached interface (`Hub`, `Port`), and the pump follows
//! them to tell the owner what nobody was there to see: a run's result, a run gone quiet, an
//! approval nobody answers (`Teller`, through the outbox and the notification channels).

use super::*;

/// Records a slow client may fall behind by before it is told it missed some.
pub(super) const BACKLOG: usize = 4096;

/// Command ids a channel sends start with this, so the hub can tell a run a channel is relaying
/// to the owner from one nobody is watching.
pub const CHANNEL_ID: &str = "ch:";

/// What every connection to one thread shares.
pub(super) struct Hub {
    /// Every record the runtime emits, as a line, to every client.
    pub(super) lines: broadcast::Sender<Arc<str>>,
    pub(super) clients: AtomicUsize,
    pub(super) busy: AtomicBool,
    /// Tasks working beside the conversation's own run (the task pool, decision D78).
    pub(super) beside: AtomicUsize,
    /// Approval requests nobody has answered, replayed to a client that attaches later.
    pub(super) approvals: Mutex<BTreeMap<String, Waiting>>,
    /// What a client is told when it attaches.
    pub(super) about: Value,
    pub(super) inspector: Arc<Inspector>,
    /// The message channels running inside the daemon.
    pub(super) channels: Mutex<Vec<String>>,
    /// A channel started or steered the run under way and is relaying it to the owner.
    pub(super) relaying: AtomicBool,
    /// Channels that put approval questions to the owner right now (a paired chat is there).
    pub(super) approvers: AtomicUsize,
    /// Attached interfaces that can answer an approval (a terminal can; `nypi -p` cannot).
    pub(super) answering: AtomicUsize,
    /// Nobody can attach to this thread later: it lives inside a terminal's own process, or it
    /// is one job.
    pub(super) local: bool,
    /// The thread's owner reports its results itself (an idle pass says only its note).
    pub(super) quiet: bool,
}

/// An approval request nobody has answered yet.
pub(super) struct Waiting {
    pub(super) record: Value,
    pub(super) since: Instant,
    /// The owner was told beyond the interfaces attached: a notice, or the question in Telegram.
    pub(super) escalated: bool,
    /// The question as it stands with another process's Telegram channel. Dropping it withdraws it.
    pub(super) asking: Option<AbortOnDrop>,
}

pub(super) struct AbortOnDrop(pub(super) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Hub {
    pub(super) fn new(about: Value, inspector: Arc<Inspector>, local: bool, quiet: bool) -> Arc<Self> {
        Arc::new(Hub {
            lines: broadcast::channel(BACKLOG).0,
            clients: AtomicUsize::new(0),
            busy: AtomicBool::new(false),
            beside: AtomicUsize::new(0),
            approvals: Mutex::new(BTreeMap::new()),
            about,
            inspector,
            channels: Mutex::new(Vec::new()),
            relaying: AtomicBool::new(false),
            approvers: AtomicUsize::new(0),
            answering: AtomicUsize::new(0),
            local,
            quiet,
        })
    }

    pub(super) fn attached(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    pub(super) fn hello(&self) -> Value {
        let mut hello = self.about.clone();
        hello["type"] = json!("daemon_hello");
        hello["busy"] = json!(self.busy.load(Ordering::SeqCst));
        hello["clients"] = json!(self.attached());
        hello["channels"] = json!(*self.channels.lock().unwrap());
        hello
    }

    /// A command on its way to the runtime: an answered approval is no longer waiting.
    pub(super) fn note_command(&self, command: &Value) {
        if command["type"] == "approval_response"
            && let Some(id) = command["id"].as_str()
        {
            self.approvals.lock().unwrap().remove(id);
        }
    }
}

/// What a message channel inside the daemon works through: the same command stream and record
/// stream as an interface on the socket, and a way to tell the hub what the channel does for
/// the owner, so nobody is told the same thing twice.
#[derive(Clone)]
pub struct Port {
    pub(super) hub: Arc<Hub>,
    /// Weak, so that closing a thread closes its input whoever still holds a port.
    pub(super) commands: mpsc::WeakUnboundedSender<String>,
}

impl Port {
    /// Send a command to the runtime. False when the daemon is stopping.
    pub fn send(&self, command: Value) -> bool {
        self.hub.note_command(&command);
        self.commands.upgrade().is_some_and(|commands| commands.send(command.to_string()).is_ok())
    }

    pub fn records(&self) -> broadcast::Receiver<Arc<str>> {
        self.hub.lines.subscribe()
    }

    pub fn busy(&self) -> bool {
        self.hub.busy.load(Ordering::SeqCst)
    }

    /// Whether this thread's Lucia has her chat module in front of the worker.
    pub fn chat(&self) -> bool {
        !self.hub.about["chat"].is_null()
    }

    pub fn approval_waiting(&self, id: &str) -> bool {
        self.hub.approvals.lock().unwrap().contains_key(id)
    }

    /// The channel is up, under this name.
    pub fn joined(&self, name: &str) {
        self.hub.channels.lock().unwrap().push(name.to_string());
        log(&format!("channel {name} is up"));
    }

    /// Whether this channel puts approval questions to the owner now. Call once per change.
    pub fn asks_approvals(&self, yes: bool) {
        if yes {
            self.hub.approvers.fetch_add(1, Ordering::SeqCst);
        } else {
            let _ = self.hub.approvers.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
        }
    }

    pub fn inspector(&self) -> &Arc<Inspector> {
        &self.hub.inspector
    }
}

/// How a thread tells its owner what no attached interface would show them.
pub(super) struct Teller {
    pub(super) paths: Paths,
    pub(super) notifier: Option<Notifier>,
    pub(super) notify: nypi_core::config::NotifySettings,
    /// The folder's name, for a question put to Telegram.
    pub(super) place: String,
    /// The thread's own input: an answer that came back from Telegram goes in here.
    pub(super) commands: mpsc::WeakUnboundedSender<String>,
}

impl Teller {
    #[cfg(test)]
    pub(super) fn new(paths: Paths, notifier: Option<Notifier>) -> Self {
        let commands = mpsc::unbounded_channel().0.downgrade();
        Teller { paths, notifier, notify: Default::default(), place: String::new(), commands }
    }

    /// Outbox first, then the channels: for what nobody is there to see.
    pub(super) async fn tell(&self, level: AlertLevel, text: &str, task: Option<&TaskId>) {
        let notice = Notice { source: "hub", level, text, task, shown_in: None };
        notify::post(&self.paths, self.notifier.as_ref(), notice).await;
    }

    /// The channels only: for what an attached interface shows, to an owner who may have looked away.
    pub(super) async fn nudge(&self, text: &str) {
        if let Some(notifier) = &self.notifier {
            notifier.send(text).await;
        }
    }

    /// Put an approval question to the Telegram channel of another process (the daemon's, when
    /// this hub is a terminal's own), and pass the answer on as if an interface had given it.
    pub(super) fn ask_elsewhere(&self, hub: &Arc<Hub>, record: &Value) -> AbortOnDrop {
        let request = BridgeRequest::Approval {
            origin: format!("terminal · {}", self.place),
            tool: record["tool"].as_str().unwrap_or("?").to_string(),
            summary: record["summary"].as_str().unwrap_or_default().to_string(),
            reason: record["reason"].as_str().unwrap_or_default().to_string(),
        };
        let id = record["id"].as_str().unwrap_or_default().to_string();
        let (socket, hub, commands) = (self.paths.bridge_socket(), hub.clone(), self.commands.clone());
        AbortOnDrop(tokio::spawn(async move {
            let decision = bridge::ask(&socket, &request).await;
            // Taken out first: removing the entry later would abort this very task.
            let Some(waiting) = hub.approvals.lock().unwrap().remove(&id) else { return };
            std::mem::forget(waiting.asking);
            let (approved, reason, note) = match decision {
                Some(decision) if decision.approved => (true, None, "Approved from Telegram."),
                Some(decision) => (false, decision.reason, "Denied from Telegram."),
                None => (false, Some(NOBODY_ANSWERS.to_string()), "Telegram went away before answering; denied."),
            };
            let answer = json!({ "type": "approval_response", "id": id, "approved": approved, "reason": reason });
            if let Some(commands) = commands.upgrade() {
                let _ = commands.send(answer.to_string());
            }
            let said = crate::rpc::alert_record(&Alert::new(AlertLevel::Notice, note));
            let _ = hub.lines.send(said.to_string().into());
        }))
    }
}

pub(super) const NOBODY_ANSWERS: &str = "approval is required, but no one can answer: nobody is at this run and Telegram is not \
                              connected";

/// A settled run as the line the user reads, and how loud it is.
pub(super) fn settled_report(record: &Value) -> (AlertLevel, String) {
    if let Some(error) = record["error"].as_str() {
        return (AlertLevel::Error, format!("Failed: {error}"));
    }
    let kind = record["status"]["kind"].as_str().unwrap_or("done");
    let level = match kind {
        "failed" => AlertLevel::Error,
        "blocked" => AlertLevel::Warning,
        _ => AlertLevel::Notice,
    };
    let mut label = kind.to_string();
    if let Some(first) = label.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    let text = record["status"]["text"].as_str().unwrap_or_default();
    (level, if text.is_empty() { label } else { format!("{label}: {text}") })
}

/// A run under way, as the pump follows it.
pub(super) struct Run {
    pub(super) started: Instant,
    pub(super) progress: Progress,
}

/// Whether a record is the worker moving: the protocol's own records and chat's turns are not.
pub(super) fn is_progress(record: &Value) -> bool {
    let kind = record["type"].as_str().unwrap_or_default();
    record["module"] != "chat"
        && !kind.starts_with("chat_")
        && !matches!(
            kind,
            "response"
                | "alert"
                | "memory_hint"
                | "user_said"
                | "approval_request"
                | "approval_resolved"
                | "reflection"
        )
}

/// Pass every record from the runtime to the clients, keep track of what is under way, and tell
/// the user what they would otherwise not see. `gate`, when there is one, holds the records
/// back until the first interface has attached.
pub(super) async fn pump(
    hub: Arc<Hub>,
    mut records: mpsc::UnboundedReceiver<Value>,
    teller: Teller,
    gate: Option<oneshot::Receiver<()>>,
) {
    if let Some(gate) = gate {
        let _ = gate.await;
    }
    let mut run: Option<Run> = None;
    let mut ticks = tokio::time::interval(STALL_CHECK_INTERVAL);
    loop {
        let record = tokio::select! {
            record = records.recv() => match record {
                Some(record) => record,
                None => break,
            },
            _ = ticks.tick() => {
                watch(&hub, &teller, run.as_mut()).await;
                continue;
            }
        };
        // A task working beside the conversation's own (`lane`) is counted, so the thread is not
        // taken for idle, but the main run's clock and stall watch are not its to move: its own
        // runtime stops it if it makes no progress.
        let beside = record["lane"].is_string();
        if let (Some(run), true, false) = (run.as_mut(), is_progress(&record), beside) {
            run.progress.observe_record(&record);
        }
        match record["type"].as_str() {
            Some("task_started") if beside => {
                hub.beside.fetch_add(1, Ordering::SeqCst);
            }
            Some("agent_start") if beside => {}
            // The principal reviews this settlement before the owner hears of it: its own words
            // (a `chat_settled`), or the report it lets stand (`task_report`), are what is told.
            Some("agent_settled") if record["review"] == "pending" => {
                if beside {
                    let _ = hub.beside.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
                } else {
                    hub.busy.store(false, Ordering::SeqCst);
                    hub.approvals.lock().unwrap().clear();
                    run = None;
                }
                log("run settled: under review");
            }
            Some("agent_settled") if beside => {
                let _ = hub.beside.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
                let (level, text) = settled_report(&record);
                if !hub.quiet && hub.attached() == 0 {
                    let task = record["task"].as_str().map(|id| TaskId(id.to_string()));
                    teller.tell(level, &text, task.as_ref()).await;
                }
            }
            Some("task_report") => {
                let (level, text) = settled_report(&record);
                if !hub.quiet && hub.attached() == 0 {
                    let task = record["task"].as_str().map(|id| TaskId(id.to_string()));
                    teller.tell(level, &text, task.as_ref()).await;
                }
            }
            // Chat's own turns are not work under way: only the worker's runs are.
            Some("agent_start") if record["module"] != "chat" => {
                hub.busy.store(true, Ordering::SeqCst);
                run.get_or_insert_with(|| Run { started: Instant::now(), progress: Progress::default() });
                log("run started");
            }
            // What the Principal had chat tell the owner (decision D84) is for them whatever
            // else runs: with nobody there to read it, it is a notice.
            Some("chat_settled") if record["stimulus"] == true => {
                let text = record["reply"].as_str().unwrap_or_default().trim().to_string();
                if !hub.quiet && hub.attached() == 0 && !text.is_empty() {
                    teller.tell(AlertLevel::Notice, &text, None).await;
                }
            }
            // Chat answered and nothing more is coming: this exchange is over. With nobody there
            // to read the answer, it goes to the owner as a notice.
            Some(kind @ ("chat_settled" | "chat_failed")) if record["working"] != true => {
                let relayed = hub.relaying.swap(false, Ordering::SeqCst);
                let (level, text) = match kind {
                    "chat_failed" => (
                        AlertLevel::Error,
                        format!(
                            "Failed: the chat model did not answer ({})",
                            record["error"].as_str().unwrap_or("unknown error")
                        ),
                    ),
                    _ => (AlertLevel::Notice, record["reply"].as_str().unwrap_or_default().trim().to_string()),
                };
                if hub.attached() == 0 && !relayed && !text.is_empty() {
                    teller.tell(level, &text, None).await;
                }
            }
            // A channel's message started or steered the run: the channel relays it from here.
            Some("response")
                if record["success"] == true
                    && record["id"].as_str().is_some_and(|id| id.starts_with(CHANNEL_ID))
                    && matches!(record["data"]["disposition"].as_str(), Some("started" | "queued")) =>
            {
                hub.relaying.store(true, Ordering::SeqCst);
            }
            Some("approval_request") => {
                let id = record["id"].as_str().unwrap_or_default().to_string();
                log(&format!("approval needed for {}", record["tool"].as_str().unwrap_or("?")));
                // An interface that can answer shows it; a channel with the owner on it asks them.
                // The board showing it (`listened`) can answer too, so it is not denied for want of one.
                let nobody = hub.answering.load(Ordering::SeqCst) == 0
                    && hub.approvers.load(Ordering::SeqCst) == 0
                    && record["listened"] != true;
                let waiting =
                    Waiting { record: record.clone(), since: Instant::now(), escalated: nobody, asking: None };
                hub.approvals.lock().unwrap().insert(id.clone(), waiting);
                if nobody && hub.local {
                    // Nobody can attach to a terminal's own hub: Telegram is asked, or it is denied.
                    if bridge::available(&teller.paths.bridge_socket()) {
                        let asking = teller.ask_elsewhere(&hub, &record);
                        if let Some(waiting) = hub.approvals.lock().unwrap().get_mut(&id) {
                            waiting.asking = Some(asking);
                        }
                    } else {
                        hub.approvals.lock().unwrap().remove(&id);
                        let answer = json!({ "type": "approval_response", "id": id, "approved": false, "reason": NOBODY_ANSWERS });
                        if let Some(commands) = teller.commands.upgrade() {
                            let _ = commands.send(answer.to_string());
                        }
                    }
                } else if nobody {
                    let text = format!(
                        "Lucia needs your approval to run {}: {}. Answer with: nypi attach",
                        record["tool"].as_str().unwrap_or("a tool"),
                        record["summary"].as_str().unwrap_or_default()
                    );
                    teller.tell(AlertLevel::Warning, &text, None).await;
                }
            }
            Some("approval_resolved") => {
                hub.approvals.lock().unwrap().remove(record["id"].as_str().unwrap_or_default());
            }
            Some("agent_settled") => {
                hub.busy.store(false, Ordering::SeqCst);
                hub.approvals.lock().unwrap().clear();
                let (level, text) = settled_report(&record);
                log(&format!("run settled: {}", text.split(':').next().unwrap_or_default()));
                let relayed = hub.relaying.swap(false, Ordering::SeqCst);
                let took = run.take().map(|run| run.started.elapsed()).unwrap_or_default();
                if hub.quiet {
                    // Whoever started this job reads the record and says what there is to say.
                } else if hub.attached() == 0 && !relayed {
                    let task = record["task"].as_str().map(|id| TaskId(id.to_string()));
                    teller.tell(level, &text, task.as_ref()).await;
                } else if !relayed && took >= Duration::from_secs(teller.notify.after_seconds) {
                    // Somebody is attached, but after a run this long they have probably looked away.
                    teller.nudge(&format!("{text}\n(run took {})", format_duration(took))).await;
                }
            }
            _ => {}
        }
        // No client is no error: the record is in the session and the audit log.
        let _ = hub.lines.send(record.to_string().into());
    }
}

/// What time alone brings: a run gone quiet, and an approval nobody has answered.
pub(super) async fn watch(hub: &Arc<Hub>, teller: &Teller, run: Option<&mut Run>) {
    if let Some(run) = run
        && let Some(text) = run.progress.stall(Duration::from_secs(teller.notify.stall_seconds))
    {
        let said = crate::rpc::alert_record(&Alert::new(AlertLevel::Warning, text.clone()));
        let _ = hub.lines.send(said.to_string().into());
        match hub.attached() {
            0 => teller.tell(AlertLevel::Warning, &text, None).await,
            _ => teller.nudge(&text).await,
        }
    }
    // The oldest question that has waited long enough goes beyond the interfaces showing it,
    // unless a channel has put it to the owner already.
    if hub.approvers.load(Ordering::SeqCst) > 0 {
        return;
    }
    let limit = Duration::from_secs(teller.notify.approval_seconds);
    let due = {
        let mut approvals = hub.approvals.lock().unwrap();
        let oldest = approvals.values_mut().filter(|w| !w.escalated).min_by_key(|w| w.since);
        oldest.filter(|w| w.since.elapsed() >= limit).map(|waiting| {
            waiting.escalated = true;
            waiting.record.clone()
        })
    };
    let Some(record) = due else { return };
    if hub.local && bridge::available(&teller.paths.bridge_socket()) {
        let asking = teller.ask_elsewhere(hub, &record);
        if let Some(waiting) = hub.approvals.lock().unwrap().get_mut(record["id"].as_str().unwrap_or_default()) {
            waiting.asking = Some(asking);
        }
        let note = Alert::new(AlertLevel::Notice, "Also asked in Telegram; answer here or there.");
        let _ = hub.lines.send(crate::rpc::alert_record(&note).to_string().into());
    } else {
        teller
            .nudge(&format!(
                "Blocked: waiting for your approval in the terminal.\n{}: {}",
                record["tool"].as_str().unwrap_or("a tool"),
                record["summary"].as_str().unwrap_or_default()
            ))
            .await;
    }
}
