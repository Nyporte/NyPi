//! A thread of the hub: one runtime put to work, with its record stream and its command
//! stream. The hub's conversation is one (primary.rs); a job nobody is talking to is another
//! (jobs.rs), and so is the one thread of a hub inside a terminal's process.

use super::*;

/// How long a thread that is closed may take to finish (its reflections get half a minute).
pub(super) const CLOSE_WAIT: Duration = Duration::from_secs(45);

/// What a thread is for.
#[derive(Clone, Copy)]
pub(crate) enum Role {
    /// A conversation the daemon holds, which interfaces attach to.
    Conversation,
    /// The one thread of a hub inside a terminal's process, named for its front end (`cli`, `print`).
    Local(&'static str),
    /// One job nobody is talking to (jobs.rs), named for where it came from. A quiet job's
    /// owner reports its result itself.
    Job { origin: &'static str, quiet: bool },
}

/// One conversation of the hub: a runtime at a folder, its record stream, and its command
/// stream. Dropping it stops its runtime at once; `close` lets it finish.
pub(crate) struct Thread {
    pub(super) cwd: PathBuf,
    pub(super) hub: Arc<Hub>,
    /// The only strong sender of the thread's input: taking it away closes the input.
    pub(super) commands: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// When an interface last attached or sent something.
    pub(super) used: Mutex<Instant>,
    /// The protocol loop; it hands back what its last reflections had to say.
    pub(super) serving: Mutex<Option<tokio::task::JoinHandle<Vec<String>>>>,
    pub(super) pumping: tokio::task::AbortHandle,
    /// Lets the records flow, where they were held for the first interface.
    pub(super) release: Mutex<Option<oneshot::Sender<()>>>,
    /// Runs this thread began by itself: the tasks its session's last process left unfinished.
    pub(crate) resuming: usize,
    /// Specialists working for this conversation in the background, not reported yet.
    pub(super) delegations: Arc<AtomicUsize>,
}

impl Thread {
    /// Put `runtime` to work as a thread: resume what its last process left, then take commands.
    /// The records of a thread nobody can attach to later (a terminal's own, a job) wait for
    /// `release`, so whoever started it sees them from the first.
    pub(crate) fn start(runtime: Runtime, role: Role) -> Thread {
        let cwd = runtime.cwd.clone();
        let (local, channel, quiet) = match role {
            Role::Conversation => (false, "hub", false),
            Role::Local(front_end) => (true, front_end, false),
            Role::Job { origin, quiet } => (true, origin, quiet),
        };
        // The chat module is part of Lucia wherever a chat model is named (the user's first
        // concept), not a mode of one front end. A job is not a conversation: the worker alone.
        let wanted = runtime.settings.chat.model.clone().filter(|_| !matches!(role, Role::Job { .. }));
        let chat = wanted.filter(|model| {
            let known = runtime.registry.find(model).is_some();
            if !known {
                log(&format!("chat.model {model} is not a known model; this conversation runs without a chat module"));
            }
            known
        });
        let about = json!({
            "pid": std::process::id(),
            "agent": runtime.settings.agent_name,
            "model": runtime.model().reference(),
            "thinking": runtime.agent.config.thinking.as_str(),
            "chat": chat,
            // The folder the conversation works in, and the device it is on when that is not this one.
            "cwd": match runtime.device() {
                Some(_) => json!(runtime.place().folder()),
                None => json!(runtime.cwd),
            },
            "device": runtime.device(),
            "session": runtime.session_id(),
            "sessionFile": runtime.session_path(),
            "hasKey": runtime.has_api_key(),
            "notifies": Notifier::from_runtime(&runtime).map(|notifier| notifier.channels()),
            "local": local,
        });
        let hub = Hub::new(about, runtime.inspector(), local, quiet);
        let (commands, inbox) = mpsc::unbounded_channel::<String>();
        // A conversation can have specialists working for it in the background (decision D67,
        // part 4). When one finishes, its report is a pulse in this thread; it is handed to the
        // conversation as a prompt that is nobody's words, so it waits its turn behind what runs
        // and is reviewed as a task of its own. The listener ends with the thread's bus.
        let delegations = match role {
            Role::Conversation | Role::Local("cli") => {
                let mut reports = runtime.pulses().subscribe("reports", &[PulseKind::Reported]);
                let conversation = commands.downgrade();
                tokio::spawn(async move {
                    while let Some(pulse) = reports.recv().await {
                        let PulsePayload::Reported { text, .. } = &pulse.payload else { continue };
                        let Some(conversation) = conversation.upgrade() else { break };
                        let report = json!({
                            "type": "prompt", "message": text, "source": pulse.source, "origin": "report", "cause": pulse.id,
                        });
                        let _ = conversation.send(report.to_string());
                    }
                });
                runtime.allow_background_delegation()
            }
            _ => Arc::default(),
        };
        let teller = Teller {
            paths: runtime.paths.clone(),
            notifier: Notifier::from_runtime(&runtime),
            notify: runtime.settings.notify.clone(),
            place: crate::notify::place(&runtime.cwd),
            commands: commands.downgrade(),
        };
        // Work this thread's last process left unfinished goes first (principle B6).
        let interrupted = runtime.interrupted_tasks();
        for task in &interrupted {
            log(&format!("resuming interrupted task {} (attempt {})", task.id, task.attempts + 1));
        }
        let (out, records) = mpsc::unbounded_channel::<Value>();
        let (release, gate) = oneshot::channel();
        let pumping = tokio::spawn(pump(hub.clone(), records, teller, local.then_some(gate))).abort_handle();
        // With chat in front, only the newest interrupted task is taken up again, by the worker.
        let resuming = if chat.is_some() { interrupted.len().min(1) } else { interrupted.len() };
        // The sender is kept in the thread, so the runtime never sees its input close when an
        // interface leaves.
        let serving = match chat {
            Some(_) => tokio::spawn(async move {
                let options = nypi_core::chat::ChatOptions::default();
                crate::chat_loop::serve(runtime, inbox, out, channel, interrupted, options).await
            }),
            None => tokio::spawn(async move {
                let first: Vec<Job> = interrupted.into_iter().map(|task| Job::Resume(task.id)).collect();
                crate::rpc::serve_as(runtime, inbox, out, channel, first).await
            }),
        };
        Thread {
            cwd,
            hub,
            commands: Mutex::new(Some(commands)),
            used: Mutex::new(Instant::now()),
            serving: Mutex::new(Some(serving)),
            pumping,
            release: Mutex::new(Some(release)),
            resuming,
            delegations,
        }
    }

    /// Every record of the thread from now on.
    pub(crate) fn records(&self) -> broadcast::Receiver<Arc<str>> {
        self.hub.lines.subscribe()
    }

    /// Let the records flow, where they were held back.
    pub(crate) fn release(&self) {
        if let Some(release) = self.release.lock().unwrap().take() {
            let _ = release.send(());
        }
    }

    /// Whether the chat module (and so the Principal) is in front of this thread's worker.
    pub(crate) fn has_chat(&self) -> bool {
        self.hub.about["chat"].is_string()
    }

    /// The conversation this thread holds (its session's id).
    pub(crate) fn session(&self) -> Option<&str> {
        self.hub.about["session"].as_str()
    }

    pub(super) fn touch(&self) {
        *self.used.lock().unwrap() = Instant::now();
    }

    pub(super) fn busy(&self) -> bool {
        self.hub.busy.load(Ordering::SeqCst) || self.hub.beside.load(Ordering::SeqCst) > 0
    }

    /// Specialists still working for this conversation in the background.
    pub(super) fn delegated(&self) -> usize {
        self.delegations.load(Ordering::SeqCst)
    }

    /// Give the runtime a command line. False when the thread is closed.
    pub(crate) fn send(&self, line: String) -> bool {
        self.commands.lock().unwrap().as_ref().is_some_and(|commands| commands.send(line).is_ok())
    }

    pub(crate) fn port(&self) -> Port {
        let commands = self.commands.lock().unwrap().as_ref().map(mpsc::UnboundedSender::downgrade);
        Port { hub: self.hub.clone(), commands: commands.unwrap_or_else(|| mpsc::unbounded_channel().0.downgrade()) }
    }

    /// Stop at once (decision D95): the run under way is cut off where it is, as by a restart,
    /// and the hub that starts from this state takes it up again.
    pub(crate) fn halt(&self) {
        self.commands.lock().unwrap().take();
        if let Some(serving) = self.serving.lock().unwrap().take() {
            serving.abort();
        }
        self.pumping.abort();
    }

    /// Close the thread's input and let it finish: a run under way is stopped, and reflections
    /// on the last tasks save what they learned. Returns what those reflections had to say.
    pub(crate) async fn close(&self) -> Vec<String> {
        self.commands.lock().unwrap().take();
        let serving = self.serving.lock().unwrap().take();
        match serving {
            Some(serving) => {
                tokio::time::timeout(CLOSE_WAIT, serving).await.ok().and_then(Result::ok).unwrap_or_default()
            }
            None => Vec::new(),
        }
    }
}

impl Drop for Thread {
    fn drop(&mut self) {
        self.pumping.abort();
        if let Some(serving) = self.serving.lock().unwrap().take() {
            serving.abort();
        }
    }
}
