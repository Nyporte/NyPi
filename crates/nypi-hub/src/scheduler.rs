//! Runs timers and condition triggers (nypi-core/src/schedule.rs) and the `nypi schedule`
//! command. The scheduler is a module of the hub: the daemon, or a terminal's own hub, checks
//! the schedules every 30 s. More than one hub can be running (a terminal with a session of its
//! own beside the daemon), so an exclusive lock on `~/.nypi/run/scheduler.lock` lets one process
//! act per round, as for the watchdog (D23). A firing is recorded before it is acted on, so a
//! crash can lose a firing but never repeat one. A notice goes to the interfaces and the
//! notification channels. A prompt, and an idle pass that is due (nypi-core/src/dmn.rs), are
//! handed to the hub as orders (jobs.rs), which carries them out in its own process.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use nypi_core::config::Paths;
use nypi_core::dmn::DmnSettings;
use nypi_core::inspector::{Alert, AlertLevel, Inspector};
use nypi_core::pulse::{PulseBus, PulsePayload, PulseSource};
use nypi_core::schedule::{Action, CommandWhen, Firing, ScheduleEvent, ScheduleStore, Trigger};

use crate::jobs::{Jobs, Order};
use crate::notify::{self, Notice, Notifier};

pub const CHECK_EVERY: Duration = Duration::from_secs(30);

/// What a round came to: what to tell the user, and what the hub is to carry out.
#[derive(Debug, Default)]
pub struct Round {
    pub reports: Vec<(AlertLevel, String)>,
    pub orders: Vec<Order>,
}

/// Check the schedules in the background for as long as the hub runs. What fires is shown by
/// the interface named by `shown_in` and goes through the outbox to the channels; what is to be
/// done goes to `jobs`.
pub fn spawn(
    paths: Paths,
    dmn: DmnSettings,
    inspector: Arc<Inspector>,
    notifier: Option<Notifier>,
    bus: Option<PulseBus>,
    shown_in: Option<&'static str>,
    jobs: Jobs,
) -> tokio::task::JoinHandle<()> {
    spawn_with(paths, dmn, inspector, notifier, bus, shown_in, jobs, None)
}

/// `spawn`, for a hub that reaches other devices: a command condition set at a folder on one
/// of them is checked there (decision D80).
#[allow(clippy::too_many_arguments)]
pub fn spawn_with(
    paths: Paths,
    dmn: DmnSettings,
    inspector: Arc<Inspector>,
    notifier: Option<Notifier>,
    bus: Option<PulseBus>,
    shown_in: Option<&'static str>,
    jobs: Jobs,
    devices: Option<Arc<dyn nypi_core::place::DeviceLookup>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut checked = BTreeMap::new();
        let mut interval = tokio::time::interval(CHECK_EVERY);
        loop {
            interval.tick().await;
            let round = run_once_with(&paths, &dmn, &mut checked, bus.as_ref(), devices.clone());
            let round = tokio::time::timeout(crate::watchdog::ROUND_LIMIT, round).await.unwrap_or_default();
            for (level, message) in round.reports {
                inspector.alert(Alert::new(level, message.clone()));
                let notice = Notice { source: "scheduler", level, text: &message, task: None, shown_in };
                notify::post(&paths, notifier.as_ref(), notice).await;
            }
            for order in round.orders {
                jobs.take(order);
            }
        }
    })
}

/// One round: fire what is due. Empty when another process holds the lock this round.
pub async fn run_once(
    paths: &Paths,
    dmn: &DmnSettings,
    checked: &mut BTreeMap<String, chrono::DateTime<Utc>>,
    bus: Option<&PulseBus>,
) -> Round {
    run_once_with(paths, dmn, checked, bus, None).await
}

/// `run_once`, checking conditions set at other devices on those devices.
pub async fn run_once_with(
    paths: &Paths,
    dmn: &DmnSettings,
    checked: &mut BTreeMap<String, chrono::DateTime<Utc>>,
    bus: Option<&PulseBus>,
    devices: Option<Arc<dyn nypi_core::place::DeviceLookup>>,
) -> Round {
    let lock_path = paths.home.join("run").join("scheduler.lock");
    let Some(lock) = crate::round_lock::RoundLock::try_take(&lock_path) else {
        return Round::default();
    };
    // A command condition was written by the agent and runs here unattended: under the tool
    // sandbox, built from the user's own settings (never a project's).
    let settings = nypi_core::config::Settings::load(paths, &paths.home).unwrap_or_default();
    let protected = nypi_core::protected::Protected::new(paths, &settings.env_files);
    let (sandbox, _) = nypi_core::sandbox::Sandbox::detect(&settings.sandbox, &protected);
    let store = ScheduleStore::new(paths.schedules()).sandboxed(sandbox).with_devices(devices);
    let mut reports = Vec::new();
    let mut orders = Vec::new();
    for firing in store.due(Utc::now(), checked).await {
        if let Err(error) = store.record_fired(&firing) {
            reports.push((AlertLevel::Error, format!("Could not record schedule {}: {error}", firing.schedule.id)));
            continue;
        }
        // Said on the hub's bus: the run it leads to, in whichever thread, names it as its cause.
        let fired = bus.map(|bus| {
            bus.publish(
                source(&firing),
                None,
                PulsePayload::Triggered {
                    schedule: firing.schedule.id.clone(),
                    text: firing.schedule.action.text().to_string(),
                },
            )
        });
        reports.push(match &firing.schedule.action {
            Action::Notify { text } => (AlertLevel::Notice, format!("⏰ {text}")),
            Action::Prompt { text } => {
                let (cwd, prompt) = prompt_of(&firing.schedule);
                let schedule = firing.schedule.id.clone();
                let remote = firing.schedule.remote.clone();
                let source = source(&firing);
                orders.push(Order::Prompt { schedule, cwd, remote, text: prompt, source, cause: fired });
                (AlertLevel::Notice, format!("⏰ Started scheduled task {}: {text}", firing.schedule.id))
            }
        });
    }
    if let Ok(why) = nypi_core::dmn::due(dmn, paths, Utc::now()) {
        orders.push(Order::IdlePass(why));
    }
    drop(lock);
    Round { reports, orders }
}

pub fn source(firing: &Firing) -> PulseSource {
    if firing.schedule.trigger.is_condition() {
        PulseSource::Condition { condition: firing.schedule.id.clone() }
    } else {
        PulseSource::Timer { schedule: firing.schedule.id.clone() }
    }
}

/// The prompt a fired schedule runs, saying where it came from, and the folder it runs in.
fn prompt_of(schedule: &nypi_core::schedule::Schedule) -> (std::path::PathBuf, String) {
    let text = schedule.action.text();
    let set = schedule.created.get(..16).unwrap_or_default().replace('T', " ");
    let prompt = format!(
        "This task was scheduled ({}), set by {} on {set} UTC. Nobody is watching this run: do the work, then report \
         the result in a few lines; it is sent to the user as a notification.\n\n{text}",
        schedule.trigger.describe(),
        schedule.by
    );
    (schedule.cwd.clone(), prompt)
}

pub fn list(paths: &Paths, all: bool) -> anyhow::Result<i32> {
    let store = ScheduleStore::new(paths.schedules());
    let schedules = if all { store.load_all() } else { store.active() };
    if schedules.is_empty() {
        crate::outln!(
            "Nothing is scheduled. Ask Lucia (\"remind me at 9 to ...\"), or: nypi schedule add --at \"2026-11-02 09:00\" --notify \"...\""
        );
        return Ok(0);
    }
    for s in schedules {
        let fired = s
            .last_fired
            .as_deref()
            .map(|t| format!(", fired {}×, last {}", s.fired, t.get(..16).unwrap_or_default()))
            .unwrap_or_default();
        let finished = if s.finished { " [finished]" } else { "" };
        crate::outln!("{}{finished}{fired}", nypi_core::tools::schedule::line(&s));
    }
    Ok(0)
}

pub struct AddArgs {
    pub at: Option<String>,
    pub every: Option<u64>,
    pub daily: Option<String>,
    pub weekdays: Vec<String>,
    pub when_file: Option<String>,
    pub when_command: Option<String>,
    pub changes: bool,
    pub notify: Option<String>,
    pub prompt: Option<String>,
    pub repeat: bool,
}

pub fn add(paths: &Paths, cwd: &std::path::Path, args: AddArgs) -> anyhow::Result<i32> {
    let triggers = [
        args.at.map(|at| Trigger::At { at }),
        args.every.map(|minutes| Trigger::Every { minutes }),
        args.daily.map(|time| Trigger::Daily { time, weekdays: args.weekdays.clone() }),
        args.when_file.map(|path| Trigger::FileChanged { path }),
        args.when_command.map(|command| Trigger::Command {
            command,
            every_minutes: 5,
            when: if args.changes { CommandWhen::Changes } else { CommandWhen::Succeeds },
        }),
    ];
    let mut triggers: Vec<Trigger> = triggers.into_iter().flatten().collect();
    if triggers.len() != 1 {
        anyhow::bail!("give exactly one of --at, --every, --daily, --when-file, --when-command");
    }
    let action = match (args.notify, args.prompt) {
        (Some(text), None) => Action::Notify { text },
        (None, Some(text)) => Action::Prompt { text },
        _ => anyhow::bail!("give exactly one of --notify or --prompt"),
    };
    let trigger = triggers.remove(0);
    let id = ScheduleStore::new(paths.schedules()).add(trigger.clone(), action, "user", cwd, args.repeat)?;
    crate::outln!(
        "Scheduled {id}: {}. The hub checks every 30 s; `nypi schedule tick` checks once.",
        trigger.describe()
    );
    Ok(0)
}

pub fn cancel(paths: &Paths, id: &str) -> anyhow::Result<i32> {
    let store = ScheduleStore::new(paths.schedules());
    match store.get(id) {
        Some(s) if !s.finished => {
            store.append(&ScheduleEvent::Cancelled {
                id: id.into(),
                at: nypi_core::memory::now(),
                by: "user".into(),
            })?;
            crate::outln!("Cancelled {id}.");
            Ok(0)
        }
        Some(_) => {
            crate::errln!("{id} already finished.");
            Ok(1)
        }
        None => {
            crate::errln!("No schedule {id}.");
            Ok(1)
        }
    }
}

/// One round now, for launchd or cron when no hub is running. What fires is carried out here,
/// and the command returns when it is done.
pub async fn tick(paths: &Paths, dmn: &DmnSettings, notifier: Option<Notifier>) -> anyhow::Result<i32> {
    let round = run_once(paths, dmn, &mut BTreeMap::new(), None).await;
    // Run by hand, the terminal shows it; from cron or launchd only the outbox and channels do.
    let shown_in = std::io::IsTerminal::is_terminal(&std::io::stdout()).then_some("terminal");
    for (level, message) in &round.reports {
        println!("{message}");
        let notice = Notice { source: "scheduler", level: *level, text: message, task: None, shown_in };
        notify::post(paths, notifier.as_ref(), notice).await;
    }
    if round.reports.is_empty() && round.orders.is_empty() {
        println!("Nothing due.");
    }
    let jobs = Jobs::here(paths, PulseBus::new());
    for order in round.orders {
        jobs.take(order);
    }
    jobs.wait().await;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_due_notice_fires_once() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths { home: home.path().to_path_buf() };
        let store = ScheduleStore::new(paths.schedules());
        let past = (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let id = store
            .add(
                Trigger::At { at: past },
                Action::Notify { text: "Call the dentist".into() },
                "user",
                home.path(),
                false,
            )
            .unwrap();
        let bus = PulseBus::new();
        let mut triggered = bus.subscribe("test", &[nypi_core::pulse::PulseKind::Triggered]);

        let mut checked = BTreeMap::new();
        let off = DmnSettings::default();
        let round = run_once(&paths, &off, &mut checked, Some(&bus)).await;
        assert_eq!(round.reports, [(AlertLevel::Notice, "⏰ Call the dentist".to_string())]);
        assert!(round.orders.is_empty(), "a notice is nothing to carry out");
        let pulse = triggered.try_recv().unwrap();
        assert_eq!(pulse.source, PulseSource::Timer { schedule: id.clone() });
        assert!(run_once(&paths, &off, &mut checked, Some(&bus)).await.reports.is_empty(), "recorded, so never twice");
        assert!(store.get(&id).unwrap().finished);
    }

    #[tokio::test]
    async fn a_due_prompt_is_handed_over_as_an_order_and_no_process_is_started() {
        let (home, work) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        let store = ScheduleStore::new(paths.schedules());
        let past = (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let action = Action::Prompt { text: "run the backup".into() };
        let id = store.add(Trigger::At { at: past }, action, "user", work.path(), false).unwrap();

        let round = run_once(&paths, &DmnSettings::default(), &mut BTreeMap::new(), None).await;
        assert_eq!(round.reports.len(), 1);
        assert!(round.reports[0].1.contains("Started scheduled task"), "{:?}", round.reports);
        let [Order::Prompt { schedule, cwd, text, source, cause: None, .. }] = round.orders.as_slice() else {
            panic!("one prompt to carry out: {:?}", round.orders)
        };
        assert_eq!((schedule, cwd.as_path()), (&id, work.path()));
        assert!(text.starts_with("This task was scheduled") && text.ends_with("run the backup"), "{text}");
        assert_eq!(*source, PulseSource::Timer { schedule: id.clone() });
        assert!(!home.path().join("run").join("schedules").exists(), "no log of a helper process");
    }

    /// A command condition set at a folder on another device is run by that device's node, in
    /// that folder, and is left alone while the device is away.
    #[tokio::test]
    async fn a_command_condition_set_at_a_device_is_checked_on_that_device() {
        use nypi_core::place::{DeviceFolder, DeviceInfo, DeviceLookup};
        use nypi_core::schedule::{Action, CommandWhen, Trigger};
        struct One(Arc<dyn nypi_node::NodeLink>, std::sync::atomic::AtomicBool);
        impl DeviceLookup for One {
            fn devices(&self) -> Vec<DeviceInfo> {
                Vec::new()
            }
            fn node(&self, device: &str) -> Result<Arc<dyn nypi_node::NodeLink>, String> {
                match device == "mini" && !self.1.load(std::sync::atomic::Ordering::SeqCst) {
                    true => Ok(self.0.clone()),
                    false => Err(format!("{device} is not connected")),
                }
            }
        }
        let (home, mini_home, site) =
            (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let paths = Paths { home: home.path().to_path_buf() };
        std::fs::write(mini_home.path().join("settings.json"), r#"{ "sandbox": "off" }"#).unwrap();
        let mini =
            nypi_core::device::this_computer(&Paths { home: mini_home.path().to_path_buf() }, Some("mini")).unwrap();
        let devices = Arc::new(One(Arc::new(mini), std::sync::atomic::AtomicBool::new(false)));
        let lookup = || Some(devices.clone() as Arc<dyn DeviceLookup>);
        let remote = DeviceFolder { device: "mini".into(), folder: site.path().display().to_string() };
        let trigger =
            Trigger::Command { command: "test -f ready.flag".into(), every_minutes: 1, when: CommandWhen::Succeeds };
        let action = Action::Notify { text: "the build on mini is ready".into() };

        // A store that reaches no device does not take it: it would be checked in the wrong place.
        let plain = ScheduleStore::new(paths.schedules());
        let refused = plain.add_at(trigger.clone(), action.clone(), "Lucia", home.path(), Some(&remote), false);
        assert!(refused.unwrap_err().to_string().contains("checked on the hub's computer"));
        let store = ScheduleStore::new(paths.schedules()).with_devices(lookup());
        let id = store.add_at(trigger, action, "Lucia", home.path(), Some(&remote), false).unwrap();
        let watched = Trigger::FileChanged { path: "ready.flag".into() };
        let notify = Action::Notify { text: "x".into() };
        assert!(store.add_at(watched, notify, "Lucia", home.path(), Some(&remote), false).is_err());

        let off = DmnSettings::default();
        let mut checked = BTreeMap::new();
        // Not there yet, as mini's own folder says (the file exists on this side under no name).
        let round = run_once_with(&paths, &off, &mut checked, None, lookup()).await;
        assert!(round.reports.is_empty(), "{:?}", round.reports);
        assert!(store.get(&id).unwrap().observed.unwrap().starts_with("failed"));

        // While mini is away nothing is observed, and what was last seen stays.
        devices.1.store(true, std::sync::atomic::Ordering::SeqCst);
        std::fs::write(site.path().join("ready.flag"), "").unwrap();
        checked.clear();
        assert!(run_once_with(&paths, &off, &mut checked, None, lookup()).await.reports.is_empty());
        assert!(store.get(&id).unwrap().observed.unwrap().starts_with("failed"));

        // Back, and the file is there on mini: the schedule fires.
        devices.1.store(false, std::sync::atomic::Ordering::SeqCst);
        checked.clear();
        let round = run_once_with(&paths, &off, &mut checked, None, lookup()).await;
        assert!(
            round.reports.iter().any(|(_, text)| text.contains("the build on mini is ready")),
            "{:?}; last observed {:?}",
            round.reports,
            store.get(&id).map(|schedule| (schedule.observed, schedule.fired))
        );
    }
}
