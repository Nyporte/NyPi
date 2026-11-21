//! Work that nobody asked for in a conversation: a fired schedule's prompt, a task a dead
//! process left unfinished, an idle pass (decision D67, part 3). The scheduler's and the
//! watchdog's rounds find such work and hand it over as an `Order`; `Jobs` carries it out in
//! this process, as a thread of the hub. No second `nypi` is started for it.
//!
//! Where the daemon's conversations are at hand, a fired prompt joins its folder's conversation
//! and an interrupted task is taken up by the conversation it belongs to. Anything else runs as
//! a job: a thread of its own on a runtime of its own, closed when its run has settled. A job is
//! not a conversation, so nobody attaches to it; its result reaches the owner as a notice, as
//! every run nobody watches does. An idle pass is a quiet job: it says only its note.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use nypi_core::config::{Paths, Settings};
use nypi_core::dmn::Why;
use nypi_core::inspector::AlertLevel;
use nypi_core::ledger::TaskRecord;
use nypi_core::place::{DeviceFolder, Remote};
use nypi_core::pulse::{PulseBus, PulseId, PulseSource};
use nypi_core::runtime::{Runtime, RuntimeOptions, SessionMode};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

use crate::daemon::{PrimaryStream, Role, Thread};
use crate::notify::{self, Notice, Notifier};

/// Something to carry out that no conversation asked for.
#[derive(Debug)]
pub enum Order {
    /// A fired schedule's prompt, for the folder it was set in (on another device: `remote`).
    Prompt {
        schedule: String,
        cwd: PathBuf,
        remote: Option<DeviceFolder>,
        text: String,
        source: PulseSource,
        cause: Option<PulseId>,
    },
    /// A task a dead process left unfinished.
    Resume(Box<TaskRecord>),
    /// An idle pass is due.
    IdlePass(Why),
}

impl Order {
    fn what(&self) -> String {
        match self {
            Order::Prompt { schedule, .. } => format!("scheduled task {schedule}"),
            Order::Resume(task) => format!("resuming task {}", task.id),
            Order::IdlePass(_) => "an idle pass".into(),
        }
    }
}

/// Makes the runtime of a job: at a folder, on a session, on its own handle of the hub's bus;
/// with a place on another device, the folder is where the hub keeps that conversation.
pub type MakeRuntime = Arc<
    dyn Fn(PathBuf, SessionMode, PulseBus, Option<Remote>) -> BoxFuture<'static, anyhow::Result<Runtime>> + Send + Sync,
>;

/// Runtimes as a job gets them outside tests: the folder's own settings, its MCP servers connected.
pub fn real_runtimes() -> MakeRuntime {
    Arc::new(|cwd, session, pulses, place| {
        Box::pin(async move {
            let options = RuntimeOptions { cwd, session, pulses: Some(pulses), place, ..Default::default() };
            let mut runtime = Runtime::new(options)?;
            for error in runtime.connect_mcp().await {
                crate::daemon::log(&format!("warning: {error}"));
            }
            // Startup alerts are not this job's news; it needs the ones raised from here on.
            if let Some(mut alerts) = runtime.take_alerts() {
                while alerts.try_recv().is_ok() {}
                runtime.restore_alerts(alerts);
            }
            Ok(runtime)
        })
    })
}

/// Carries out orders in this process. Cheap to clone; every clone is the same runner.
#[derive(Clone)]
pub struct Jobs(Arc<Inner>);

struct Inner {
    paths: Paths,
    /// The daemon's conversations, when this process is the daemon.
    threads: Option<Arc<PrimaryStream>>,
    make: MakeRuntime,
    /// The hub's bus: every job is a thread on it.
    bus: PulseBus,
    /// Session files a job is working in, so two orders never open one session twice.
    sessions: Mutex<HashSet<PathBuf>>,
    idle_pass: AtomicBool,
    running: AtomicUsize,
    settled: Notify,
    /// The job threads working now, so a hub handed over stops them where they are.
    live: Mutex<Vec<std::sync::Weak<Thread>>>,
    /// The hub was handed over (decision D95): no job starts any more.
    halted: AtomicBool,
}

impl Jobs {
    pub(crate) fn new(paths: Paths, threads: Option<Arc<PrimaryStream>>, make: MakeRuntime, bus: PulseBus) -> Jobs {
        Jobs(Arc::new(Inner {
            paths,
            threads,
            make,
            bus: bus.root(),
            sessions: Mutex::new(HashSet::new()),
            idle_pass: AtomicBool::new(false),
            running: AtomicUsize::new(0),
            settled: Notify::new(),
            live: Mutex::new(Vec::new()),
            halted: AtomicBool::new(false),
        }))
    }

    /// A runner for a process that is not the daemon: a terminal's own hub, the supervisor, a
    /// one-shot command. `bus` is that hub's.
    pub fn here(paths: &Paths, bus: PulseBus) -> Jobs {
        Jobs::new(paths.clone(), None, real_runtimes(), bus)
    }

    /// Stop every job where it is, as a restart would, and start none from now on (decision D95):
    /// the hub that takes over resumes what they were doing.
    pub(crate) fn halt(&self) {
        self.0.halted.store(true, Ordering::SeqCst);
        for thread in self.0.live.lock().unwrap().drain(..).filter_map(|held| held.upgrade()) {
            thread.halt();
        }
    }

    /// Make a job's runtime, as a thread of its own on the hub's bus.
    async fn runtime(&self, origin: &str, cwd: PathBuf, session: SessionMode) -> anyhow::Result<Runtime> {
        let pulses = self.0.bus.scoped(&format!("{origin}:{}", cwd.display()));
        (self.0.make)(cwd, session, pulses, None).await
    }

    /// A job's runtime at a folder on another device: only the daemon reaches devices, and only
    /// while the device's node is connected.
    async fn runtime_at(
        &self,
        origin: &str,
        cwd: PathBuf,
        remote: &DeviceFolder,
        session: SessionMode,
    ) -> anyhow::Result<Runtime> {
        let place = self.0.threads.as_ref().and_then(|threads| threads.remote_place(remote));
        let Some(place) = place else {
            anyhow::bail!("{} is not connected to this hub", remote.device);
        };
        let pulses = self.0.bus.scoped(&format!("{origin}:{}:{}", remote.device, remote.folder));
        (self.0.make)(cwd, session, pulses, Some(place)).await
    }

    /// Carry out `order` in the background. One that cannot even start is told to the owner
    /// (principle B6): nobody is watching it.
    pub fn take(&self, order: Order) {
        if self.0.halted.load(Ordering::SeqCst) {
            return crate::daemon::log(&format!("job not started, the hub is handed over: {}", order.what()));
        }
        let jobs = self.clone();
        jobs.0.running.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let what = order.what();
            crate::daemon::log(&format!("job: {what}"));
            if let Err(error) = jobs.carry_out(order).await {
                let text = format!("Failed: {what} could not run: {error:#}.");
                crate::daemon::log(&text);
                let paths = &jobs.0.paths;
                let settings = Settings::load(paths, &paths.home).unwrap_or_default();
                let notifier = Notifier::from_settings(&settings, paths, &paths.home, true);
                let notice =
                    Notice { source: "jobs", level: AlertLevel::Error, text: &text, task: None, shown_in: None };
                notify::post(paths, notifier.as_ref(), notice).await;
            }
            jobs.0.running.fetch_sub(1, Ordering::SeqCst);
            jobs.0.settled.notify_waiters();
        });
    }

    /// Orders taken and not carried out yet.
    pub fn running(&self) -> usize {
        self.0.running.load(Ordering::SeqCst)
    }

    /// Wait until every order taken has been carried out (a one-shot command, before it exits).
    pub async fn wait(&self) {
        loop {
            let settled = self.0.settled.notified();
            if self.running() == 0 {
                return;
            }
            settled.await;
        }
    }

    async fn carry_out(&self, order: Order) -> anyhow::Result<()> {
        let inner = &self.0;
        match order {
            Order::Prompt { cwd, remote, text, source, cause, .. } => {
                let command =
                    json!({ "type": "prompt", "message": text, "source": source, "origin": "timer", "cause": cause });
                // The hub's conversation takes it, after whatever runs, and works where the
                // schedule was set: in that folder, or at that device's folder.
                if let Some(primary) = &inner.threads {
                    let mut command = command;
                    command["focus"] = json!(primary.focus_at(Some(&cwd), remote.as_ref()));
                    anyhow::ensure!(primary.thread().send(command.to_string()), "the conversation is closed");
                    return Ok(());
                }
                // Another process is the daemon: the same, over its socket.
                let source = serde_json::to_value(&source)?;
                let delivered =
                    crate::link::deliver_at(&inner.paths, Some(&cwd), remote.as_ref(), &text, &source, "timer").await;
                if delivered.unwrap_or(false) {
                    return Ok(());
                }
                if let Some(remote) = &remote {
                    anyhow::bail!("it works at {remote}, which only the hub reaches, and the hub is not running");
                }
                let runtime = self.runtime("timer", cwd, SessionMode::New).await?;
                self.job(runtime, Role::Job { origin: "timer", quiet: false }, Some(command), Some(1)).await;
                Ok(())
            }
            Order::Resume(task) => {
                let Some(session) = task.session_path.clone() else {
                    anyhow::bail!("it ran without a saved session");
                };
                // The conversation the task belongs to takes it up as it opens.
                let remote = task.remote.as_ref();
                let owner = match &inner.threads {
                    Some(primary) => primary.thread().session().map(str::to_string),
                    None => crate::link::thread_session(&inner.paths, &task.cwd, remote).await.ok().flatten(),
                };
                if owner.as_deref() == Some(task.session_id.as_str()) {
                    return Ok(());
                }
                // One job per session: it takes up every task that session left.
                if !inner.sessions.lock().unwrap().insert(session.clone()) {
                    return Ok(());
                }
                let ran = async {
                    let opened = SessionMode::Open(session.clone());
                    // A task at another device is taken up there, never with this computer's tools.
                    let runtime = match remote {
                        Some(remote) => self.runtime_at("resume", task.cwd.clone(), remote, opened).await?,
                        None => self.runtime("resume", task.cwd.clone(), opened).await?,
                    };
                    self.job(runtime, Role::Job { origin: "resume", quiet: false }, None, None).await;
                    anyhow::Ok(())
                }
                .await;
                inner.sessions.lock().unwrap().remove(&session);
                ran
            }
            Order::IdlePass(why) => {
                // A pass that takes longer than the gap between rounds is not started twice.
                if inner.idle_pass.swap(true, Ordering::SeqCst) {
                    return Ok(());
                }
                let ran = self.idle_pass(why).await;
                inner.idle_pass.store(false, Ordering::SeqCst);
                ran
            }
        }
    }

    /// One idle pass (nypi-core/src/dmn.rs): a quiet job in the user's home folder, recorded
    /// with what it cost, whose note, if it has one, is all the owner hears.
    async fn idle_pass(&self, why: Why) -> anyhow::Result<()> {
        let paths = &self.0.paths;
        // With chat in front of the conversation, idle time is the Principal's (decision D85):
        // it gets the two clocks of the goals and the state, and decides; its activation is
        // recorded as the pass when it finishes (chat_loop.rs).
        if let Some(primary) = &self.0.threads
            && primary.thread().has_chat()
        {
            let now = chrono::Utc::now();
            let settings = Settings::load(paths, &paths.home).unwrap_or_default();
            let vault = nypi_core::dmn::consolidate(paths, settings.vault.archive_after_days, now);
            for (from, to) in &vault.archived {
                crate::daemon::log(&format!("vault: {from} archived to {to}"));
            }
            let digest = format!(
                "{}\n\n{}",
                nypi_core::campaign::goal_digest(paths, now),
                nypi_core::dmn::state_digest(paths, now, &vault)
            );
            let why = serde_json::to_value(why)?;
            let command = json!({
                "type": "prompt", "message": digest, "source": PulseSource::Dmn, "origin": "dmn", "why": why,
            });
            anyhow::ensure!(primary.thread().send(command.to_string()), "the conversation is closed");
            return Ok(());
        }
        let home = dirs::home_dir().unwrap_or_else(|| paths.home.clone());
        let mut runtime = self.runtime("dmn", home, SessionMode::New).await?;
        if let Some(warning) = crate::idle_pass::use_own_model(&mut runtime) {
            crate::daemon::log(&warning);
        }
        let notifier = Notifier::from_runtime(&runtime);
        let agent = runtime.settings.agent_name.clone();
        // The vault's upkeep is part of a pass (decision D81): what needs no judgement is done
        // now, and the rest is in the pass's prompt.
        let now = chrono::Utc::now();
        let vault = nypi_core::dmn::consolidate(paths, runtime.settings.vault.archive_after_days, now);
        for (from, to) in &vault.archived {
            crate::daemon::log(&format!("vault: {from} archived to {to}"));
        }
        let text = nypi_core::dmn::build_prompt_with(paths, now, &vault);
        let command = json!({ "type": "prompt", "message": text, "source": PulseSource::Dmn, "origin": "dmn" });
        let settled = self.job(runtime, Role::Job { origin: "dmn", quiet: true }, Some(command), Some(1)).await;
        let ran = crate::idle_pass::Ran::of_record(settled.as_ref());
        crate::idle_pass::record(paths, &agent, notifier.as_ref(), why, ran).await
    }

    /// Run `runtime` as a job thread: give it `command`, if there is one, wait for `settles`
    /// runs to end (without a number: the runs the thread began by itself, the tasks its session
    /// left unfinished), and close it. Returns how the last run ended.
    async fn job(&self, runtime: Runtime, role: Role, command: Option<Value>, settles: Option<usize>) -> Option<Value> {
        let thread = Arc::new(Thread::start(runtime, role));
        {
            let mut live = self.0.live.lock().unwrap();
            live.retain(|held| held.strong_count() > 0);
            live.push(Arc::downgrade(&thread));
        }
        // Subscribed before the records are let go, so the job is seen from its first record.
        let mut records = thread.records();
        thread.release();
        let mut left = settles.unwrap_or(thread.resuming);
        if let Some(command) = command
            && !thread.send(command.to_string())
        {
            left = 0;
        }
        let mut last = None;
        while left > 0 {
            let line = match records.recv().await {
                Ok(line) => line,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            };
            // Most records are a run's stream; only two kinds matter here.
            if !line.contains("\"agent_settled\"") && !line.contains("\"response\"") {
                continue;
            }
            let Ok(record) = serde_json::from_str::<Value>(&line) else { continue };
            match record["type"].as_str() {
                Some("agent_settled") => {
                    left -= 1;
                    last = Some(record);
                }
                // The command was refused: no run will follow.
                Some("response") if record["success"] != true => {
                    last = Some(json!({ "type": "agent_settled", "error": record["error"] }));
                    break;
                }
                _ => {}
            }
        }
        thread.close().await;
        last
    }
}

#[cfg(test)]
mod tests {
    use nypi_ai::providers::faux::{FauxProvider, FauxResponse, faux_model};
    use nypi_core::ledger::{Ledger, TaskState};
    use nypi_core::outbox::Outbox;

    use super::*;

    /// Job runtimes on a scripted model, with sessions kept in `home`.
    fn faux_runtimes(home: &std::path::Path, faux: &FauxProvider) -> MakeRuntime {
        let (home, faux) = (home.to_path_buf(), faux.clone());
        Arc::new(move |cwd, session, pulses, place| {
            let (home, faux) = (home.clone(), faux.clone());
            Box::pin(async move {
                Runtime::new(RuntimeOptions {
                    cwd,
                    pulses: Some(pulses),
                    place,
                    paths: Some(Paths { home }),
                    model: Some("faux/faux-1".into()),
                    extra_model: Some(faux_model()),
                    stream_fn: Some(faux.stream_fn()),
                    session,
                    ..Default::default()
                })
            })
        })
    }

    #[tokio::test]
    async fn a_fired_prompt_runs_as_a_job_here_and_its_result_reaches_the_owner() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        let faux = FauxProvider::new(vec![FauxResponse::text("Backed up.\n\nDone: the backup ran.")]);
        let jobs = Jobs::new(paths.clone(), None, faux_runtimes(home.path(), &faux), PulseBus::new());
        jobs.take(Order::Prompt {
            schedule: "s-1".into(),
            cwd: work.path().to_path_buf(),
            remote: None,
            text: "run the backup".into(),
            source: PulseSource::Timer { schedule: "s-1".into() },
            cause: None,
        });
        assert_eq!(jobs.running(), 1);
        tokio::time::timeout(std::time::Duration::from_secs(20), jobs.wait()).await.expect("the job settled");

        // It was a task of its own, named for where it came from, and nobody watched it.
        let tasks = Ledger::new(paths.ledger()).load();
        let task = tasks.values().next().expect("the run is in the ledger");
        assert_eq!((task.state, task.origin.as_str()), (TaskState::Done, "timer"));
        let told = Outbox::new(paths.outbox()).load();
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].text, "Done: the backup ran.");
        assert_eq!(faux.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_job_that_cannot_start_is_told_to_the_owner() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        std::fs::write(home.path().join("settings.json"), r#"{ "notify": { "channels": [] } }"#).unwrap();
        let broken: MakeRuntime = Arc::new(|_, _, _, _| Box::pin(async { anyhow::bail!("no model is set up") }));
        let jobs = Jobs::new(paths.clone(), None, broken, PulseBus::new());
        jobs.take(Order::IdlePass(Why::Idle));
        tokio::time::timeout(std::time::Duration::from_secs(20), jobs.wait()).await.expect("it ended");
        let told = Outbox::new(paths.outbox()).load();
        assert_eq!(told.len(), 1);
        assert_eq!(told[0].text, "Failed: an idle pass could not run: no model is set up.");
        assert_eq!(jobs.running(), 0);
    }

    /// A dead process's id: a child that has exited and been reaped.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[tokio::test]
    async fn in_the_daemon_a_fired_prompt_joins_the_conversation_and_works_in_its_folder() {
        let home = tempfile::tempdir().unwrap();
        let site = home.path().join("site");
        std::fs::create_dir_all(&site).unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let faux = FauxProvider::new(vec![FauxResponse::text("Checked.\n\nDone: the links are fine.")]);
        let nodes = crate::nodes::Nodes::open(&paths);
        let threads = PrimaryStream::new(crate::daemon::tests::routed(home.path(), home.path(), &faux, &nodes));
        // No job runtime is made: the hub's conversation is where it runs.
        let none: MakeRuntime = Arc::new(|_, _, _, _| Box::pin(async { anyhow::bail!("a job runtime was asked for") }));
        let jobs = Jobs::new(paths.clone(), Some(threads.clone()), none, threads.bus());
        let mut hub = threads.bus().subscribe("test", &[]);
        let fired = threads.bus().publish(
            PulseSource::Timer { schedule: "s-2".into() },
            None,
            nypi_core::pulse::PulsePayload::Triggered { schedule: "s-2".into(), text: "check the links".into() },
        );
        jobs.take(Order::Prompt {
            schedule: "s-2".into(),
            cwd: site.clone(),
            remote: None,
            text: "check the links".into(),
            source: PulseSource::Timer { schedule: "s-2".into() },
            cause: Some(fired.clone()),
        });
        jobs.wait().await;
        // Handing it over is the job; the conversation runs it and tells the owner.
        let ledger = Ledger::new(paths.ledger());
        for _ in 0..200 {
            if ledger.load().values().any(|task| task.state == TaskState::Done) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let tasks = ledger.load();
        let task = tasks.values().next().expect("the run is in the ledger");
        assert_eq!((task.state, task.origin.as_str()), (TaskState::Done, "timer"));
        assert_eq!(task.cwd, nypi_core::protected::real_path(&site));
        for _ in 0..100 {
            if !Outbox::new(paths.outbox()).load().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(Outbox::new(paths.outbox()).load()[0].text, "Done: the links are fine.");

        // The run's first pulse is the conversation's and names the timer's firing, which
        // belongs to no thread, as its cause: one chain across the hub.
        let heard: Vec<_> = std::iter::from_fn(|| hub.try_recv()).collect();
        let started = heard
            .iter()
            .find(|pulse| matches!(pulse.payload, nypi_core::pulse::PulsePayload::UserMessage { .. }))
            .expect("the run began with a message pulse");
        assert!(started.thread.is_some());
        assert_eq!(started.causation.as_ref(), Some(&fired));
        assert!(heard.iter().any(|pulse| pulse.id == fired && pulse.thread.is_none()));
        assert!(heard.iter().any(|pulse| matches!(pulse.payload, nypi_core::pulse::PulsePayload::RunSettled { .. })));
    }

    #[tokio::test]
    async fn an_interrupted_task_is_taken_up_again_in_its_own_session_once() {
        use nypi_core::ledger::{LedgerEvent, new_task_id};
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        let faux = FauxProvider::new(vec![
            FauxResponse::text("Started on it.\n\nDone: first part."),
            FauxResponse::text("Finished the report.\n\nDone: the report is built."),
        ]);
        let make = faux_runtimes(home.path(), &faux);

        // A session with one finished run, and then a task its process died in the middle of.
        let (session_id, session_path) = {
            let mut first = make(work.path().to_path_buf(), SessionMode::New, PulseBus::new(), None).await.unwrap();
            let (events, _keep) = tokio::sync::mpsc::unbounded_channel();
            first.run_job(&nypi_core::runtime::Job::Prompt("start the report".into()), &events).await.unwrap();
            (first.session_id(), first.session_path())
        };
        let ledger = Ledger::new(paths.ledger());
        let task = new_task_id();
        let at = chrono::Utc::now().to_rfc3339();
        ledger
            .append(&LedgerEvent::Created {
                task: task.clone(),
                at: at.clone(),
                goal: "build the report".into(),
                prompt: "build the report".into(),
                owner: "lucia".into(),
                origin: "cli".into(),
                parent: None,
                session_id,
                session_path: Some(session_path),
                cwd: work.path().to_path_buf(),
                remote: None,
                pid: dead_pid(),
                pulse: None,
            })
            .unwrap();
        ledger
            .append(&LedgerEvent::State { task: task.clone(), at, state: TaskState::Running, note: String::new() })
            .unwrap();
        let interrupted = ledger.get(&task).unwrap();
        assert!(interrupted.is_interrupted());

        // The watchdog may hand the same session over twice before the first job has it open.
        let jobs = Jobs::new(paths.clone(), None, make, PulseBus::new());
        jobs.take(Order::Resume(Box::new(interrupted.clone())));
        jobs.take(Order::Resume(Box::new(interrupted)));
        tokio::time::timeout(std::time::Duration::from_secs(20), jobs.wait()).await.expect("the job settled");

        let resumed = ledger.get(&task).unwrap();
        assert_eq!(resumed.state, TaskState::Done, "{resumed:?}");
        assert_eq!(faux.requests().len(), 2, "the task ran once more, not twice");
        let told = Outbox::new(paths.outbox()).load();
        assert!(told.iter().any(|notice| notice.text == "Done: the report is built."), "{told:?}");
    }

    #[tokio::test]
    async fn an_idle_pass_is_recorded_with_its_cost_and_says_only_its_note() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let faux = FauxProvider::new(vec![
            FauxResponse::text("The tax form is due on Friday and nothing is started.\n\nDone: looked over the goals."),
            FauxResponse::text("nothing\n\nDone: nothing needs you."),
        ]);
        let jobs = Jobs::new(paths.clone(), None, faux_runtimes(home.path(), &faux), PulseBus::new());
        jobs.take(Order::IdlePass(Why::Idle));
        jobs.wait().await;
        jobs.take(Order::IdlePass(Why::Interval));
        jobs.wait().await;

        let passes = nypi_core::dmn::PassLog::new(paths.dmn_passes()).load();
        let outcomes: Vec<_> = passes.iter().map(|pass| (pass.why, pass.outcome, pass.task.is_some())).collect();
        use nypi_core::dmn::Outcome;
        assert_eq!(outcomes, [(Why::Idle, Outcome::Note, true), (Why::Interval, Outcome::Nothing, true)]);
        // The pass with something to say says it, once; its status line is not also a notice.
        let told: Vec<String> = Outbox::new(paths.outbox()).load().into_iter().map(|notice| notice.text).collect();
        assert_eq!(told, ["Lucia (while idle): The tax form is due on Friday and nothing is started."]);
        let tasks = Ledger::new(paths.ledger()).load();
        assert!(tasks.values().all(|task| task.origin == "dmn" && task.state == TaskState::Done), "{tasks:?}");
    }
}
