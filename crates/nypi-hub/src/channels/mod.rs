//! The hub's channels (decision D81): the ways things reach Lucia, and she the owner, that are
//! not an interface somebody sits at. A channel runs inside the daemon for as long as it does,
//! hands what comes in to the conversation through the same port an interface on the socket
//! uses, and may carry a notice or an approval question out. Telegram is one (two-way: the
//! owner talks, hears, and answers there); the inbound webhook is another (one-way: other
//! programs of the owner's hand something in to be filed).
//!
//! Adding a channel is implementing `Channel` and naming it in `Channels::from_settings`. A
//! channel holds no model, no session, and no tool: what it takes in becomes a command for the
//! conversation, and the conversation's own rules apply to everything that follows.
//!
//! The channels a daemon runs are where its notices go too (decision D83): every notice the
//! outbox records is offered to each of them (`deliver_notice`) before the notifier's own
//! ways out, and one a channel took is not sent a second time on that channel's service.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use nypi_core::config::{Paths, Settings};
use tokio_util::sync::CancellationToken;

use crate::daemon::{Port, log};

pub mod telegram;
pub mod webhook;

/// An approval a channel is asked to put to the owner.
#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalAsk {
    /// The Inspector's id for the request; the owner's answer names it.
    pub id: String,
    pub tool: String,
    pub summary: String,
    pub reason: String,
}

#[async_trait]
pub trait Channel: Send + Sync + 'static {
    /// The channel's name, as records, notices, and the heartbeat name it.
    fn name(&self) -> &'static str;

    /// Take what comes in on this channel and hand it to the conversation through `port`, until
    /// `shutdown`. A channel that cannot start says why in the log and returns.
    async fn run(self: Arc<Self>, port: Port, shutdown: CancellationToken);

    /// Tell the owner `text` on this channel. False when it cannot reach them now (it is not
    /// running, nobody is paired, it only takes things in).
    async fn deliver_notice(&self, text: &str) -> bool;

    /// Put an approval to the owner on this channel; their answer comes back through the port
    /// as any interface's would. False when it cannot.
    async fn ask_approval(&self, ask: &ApprovalAsk) -> bool;
}

/// The channels a hub runs.
#[derive(Clone, Default)]
pub struct Channels(Vec<Arc<dyn Channel>>);

impl Channels {
    /// The channels the settings turn on. `telegram` is the Telegram channel's setup, when its
    /// bot token is there and no other process holds the bridge.
    pub fn from_settings(paths: &Paths, settings: &Settings, telegram: Option<telegram::Setup>) -> Channels {
        let mut channels: Vec<Arc<dyn Channel>> = Vec::new();
        if let Some(setup) = telegram {
            channels.push(Arc::new(telegram::TelegramChannel::new(setup)));
        }
        if settings.channels.webhook.enabled {
            match webhook::IngestWebhookChannel::new(paths, &settings.channels.webhook) {
                Ok(channel) => channels.push(Arc::new(channel)),
                // Not started rather than started open: a webhook without its secret takes nothing.
                Err(error) => log(&format!("the inbound webhook is off: {error:#}")),
            }
        }
        Channels(channels)
    }

    pub fn of(channels: Vec<Arc<dyn Channel>>) -> Channels {
        Channels(channels)
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.0.iter().map(|channel| channel.name()).collect()
    }

    /// Run every channel on the conversation `port` reaches, until `shutdown`.
    pub fn spawn(&self, port: &Port, shutdown: &CancellationToken) -> Vec<tokio::task::JoinHandle<()>> {
        self.0.iter().map(|channel| tokio::spawn(channel.clone().run(port.clone(), shutdown.clone()))).collect()
    }

    /// Make these the channels that carry the notices of the NyPi home `home`, until the
    /// returned guard is dropped.
    pub fn register(&self, home: &Path) -> Registered {
        running().lock().unwrap().insert(home.to_path_buf(), self.clone());
        Registered(home.to_path_buf())
    }

    /// Tell the owner `text` on every channel that can; the names of those that did.
    pub async fn deliver_notice(&self, text: &str) -> Vec<&'static str> {
        let mut told = Vec::new();
        for channel in &self.0 {
            if channel.deliver_notice(text).await {
                told.push(channel.name());
            }
        }
        told
    }

    /// Put an approval to the owner on every channel that can; the names of those that did.
    pub async fn ask_approval(&self, ask: &ApprovalAsk) -> Vec<&'static str> {
        let mut asked = Vec::new();
        for channel in &self.0 {
            if channel.ask_approval(ask).await {
                asked.push(channel.name());
            }
        }
        asked
    }
}

/// The channels each daemon of this process runs, by its NyPi home.
fn running() -> &'static Mutex<HashMap<PathBuf, Channels>> {
    static RUNNING: OnceLock<Mutex<HashMap<PathBuf, Channels>>> = OnceLock::new();
    RUNNING.get_or_init(Mutex::default)
}

/// The channels running for the NyPi home `home`, when a daemon in this process runs any.
pub fn running_for(home: &Path) -> Option<Channels> {
    running().lock().unwrap().get(home).cloned()
}

/// Channels registered for a home; dropped, they are not offered notices any more.
pub struct Registered(PathBuf);

impl Drop for Registered {
    fn drop(&mut self) {
        running().lock().unwrap().remove(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nypi_core::inspector::AlertLevel;
    use nypi_core::outbox::Outbox;

    /// A channel that writes down what it is asked to tell.
    struct Recorder(Mutex<Vec<String>>);

    #[async_trait]
    impl Channel for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }

        async fn run(self: Arc<Self>, _port: Port, shutdown: CancellationToken) {
            shutdown.cancelled().await
        }

        async fn deliver_notice(&self, text: &str) -> bool {
            self.0.lock().unwrap().push(text.to_string());
            true
        }

        async fn ask_approval(&self, _ask: &ApprovalAsk) -> bool {
            false
        }
    }

    fn notice(text: &str) -> crate::notify::Notice<'_> {
        crate::notify::Notice { source: "watchdog", level: AlertLevel::Warning, text, task: None, shown_in: None }
    }

    #[tokio::test]
    async fn a_notice_goes_through_the_channels_the_daemon_runs() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let outbox = Outbox::new(paths.outbox());

        // No daemon runs channels here: the notice waits in the outbox.
        crate::notify::post(&paths, None, notice("the backup did not run")).await;
        assert_eq!(outbox.open().len(), 1);

        let recorder = Arc::new(Recorder(Mutex::default()));
        let registered = Channels::of(vec![recorder.clone()]).register(&paths.home);
        crate::notify::post(&paths, None, notice("the build is stuck")).await;
        assert_eq!(*recorder.0.lock().unwrap(), ["the build is stuck"]);
        // The one waiting is offered again when the outbox is flushed, and both are through.
        assert_eq!(crate::notify::flush(&paths, None).await, 1);
        assert!(recorder.0.lock().unwrap().contains(&"the backup did not run".to_string()));
        assert!(outbox.open().is_empty(), "every notice reached the owner");

        // Through a notifier too, labelled as the notifier labels it.
        // No service of its own (the desktop's is the default): only the running channels.
        let mut settings = nypi_core::config::Settings::default();
        settings.notify.channels = Vec::new();
        let notifier = crate::notify::Notifier::build(&settings, &paths, home.path(), false);
        crate::notify::post(&paths, Some(&notifier), notice("disk almost full")).await;
        assert!(recorder.0.lock().unwrap().last().unwrap().ends_with("] disk almost full"));

        // The daemon stopped: its channels are not offered notices any more.
        drop(registered);
        crate::notify::post(&paths, None, notice("after the hub")).await;
        assert_eq!(recorder.0.lock().unwrap().len(), 3);
        assert_eq!(outbox.open().len(), 1);
    }
}
