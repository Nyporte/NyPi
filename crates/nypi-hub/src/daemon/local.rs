//! The hub inside a terminal's own process, where no daemon runs: one thread, reached over a
//! pipe instead of the socket, by the same interface code.

use super::*;

/// The hub inside a terminal's own process, where no daemon runs (or the terminal asked for a
/// session of its own): one thread on the runtime the terminal was started with, and the pipe
/// its interface talks through. Everything else is the daemon's own code path.
pub struct Local {
    pub(super) thread: Arc<Thread>,
    /// What the scheduler and the watchdog hand over while this terminal is open.
    pub(super) jobs: crate::jobs::Jobs,
}

impl Local {
    /// Start the thread and attach its one interface; `answers` says whether somebody there can
    /// answer an approval. The pipe's far end speaks the daemon's protocol, greeting first.
    pub fn start(runtime: Runtime, answers: bool) -> (Local, tokio::io::DuplexStream) {
        // The ledger and the schedules are checked from here for as long as a terminal is open. One
        // printed answer is too short a life to take those rounds on.
        // This runtime's bus is the hub's: jobs join it, and what belongs to no thread is recorded
        // by this thread's Inspector.
        let bus = runtime.pulses().root();
        bus.tap(runtime.inspector());
        let jobs = crate::jobs::Jobs::here(&runtime.paths, bus.clone());
        if answers {
            crate::watchdog::spawn(
                runtime.paths.clone(),
                runtime.settings.watchdog.clone(),
                runtime.inspector(),
                Notifier::from_runtime(&runtime),
                Some("the terminal"),
                jobs.clone(),
            );
            crate::scheduler::spawn(
                runtime.paths.clone(),
                runtime.settings.dmn.clone(),
                runtime.inspector(),
                Notifier::from_runtime(&runtime),
                Some(bus),
                Some("the terminal"),
                jobs.clone(),
            );
        }
        let thread = Arc::new(Thread::start(runtime, Role::Local(if answers { "cli" } else { "print" })));
        let (near, far) = tokio::io::duplex(PIPE_BYTES);
        let (read, write) = tokio::io::split(near);
        // Subscribed before the records are let go, so the interface sees the thread from its start.
        let records = thread.hub.lines.subscribe();
        tokio::spawn(converse(thread.clone(), records, BufReader::new(read).lines(), write, answers, None, None, None));
        thread.release();
        (Local { thread, jobs }, far)
    }

    /// End the conversation: stop what runs, let reflections finish, and hand back what they
    /// said. A job this hub was carrying out ends with the process; it is said, and the
    /// watchdog of whatever hub runs next takes it up again.
    pub async fn close(self) -> Vec<String> {
        let delegated = self.thread.delegated();
        let mut said = self.thread.close().await;
        if delegated > 0 {
            said.push(format!(
                "{delegated} specialist(s) were still working in the background and were stopped; their tasks are \
                 taken up again when NyPi next runs"
            ));
        }
        match self.jobs.running() {
            0 => {}
            cut => said.push(format!(
                "{cut} background job(s) of this terminal were cut off; they are taken up again when NyPi next runs"
            )),
        }
        said
    }
}

/// Room in the pipe between a terminal and its own hub before the writer waits.
pub(super) const PIPE_BYTES: usize = 1 << 20;
