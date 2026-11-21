//! Timers and condition triggers (docs/architecture.md §4.1, requirement R3, decision D36): things
//! that should happen later or when something changes, set by the user or the agent. A trigger is
//! a time (`at`, `every`, `daily`) or a condition (a file changed, a command succeeded or its
//! output changed); its action notifies the user or runs a prompt as a task. Schedules live in
//! `~/.nypi/schedules/schedules.jsonl`, an append-only log only the harness writes; front ends
//! check it (nypi-hub/src/scheduler.rs) and publish `timer` and `condition` pulses when one fires.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::memory::now;
use crate::place::DeviceFolder;

/// A command condition is given this long per check.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// Conditions are not checked more often than this.
const MIN_CHECK_MINUTES: u64 = 1;
/// The longest interval a schedule may name: ten years.
const MAX_MINUTES: u64 = 10 * 366 * 24 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandWhen {
    /// The command exits 0 (it did not before).
    Succeeds,
    /// Its output differs from the last check.
    Changes,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Trigger {
    /// Once, at this time (RFC 3339, or local `YYYY-MM-DD HH:MM`).
    At {
        at: String,
    },
    Every {
        minutes: u64,
    },
    /// Every day at `HH:MM` local time, or only on `weekdays` (mon, tue, ...).
    Daily {
        time: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        weekdays: Vec<String>,
    },
    FileChanged {
        path: String,
    },
    Command {
        command: String,
        #[serde(default = "default_check_minutes")]
        every_minutes: u64,
        when: CommandWhen,
    },
}

fn default_check_minutes() -> u64 {
    5
}

impl Trigger {
    pub fn is_condition(&self) -> bool {
        matches!(self, Trigger::FileChanged { .. } | Trigger::Command { .. })
    }

    pub fn describe(&self) -> String {
        match self {
            Trigger::At { at } => format!("at {at}"),
            Trigger::Every { minutes } => format!("every {minutes} min"),
            Trigger::Daily { time, weekdays } if weekdays.is_empty() => format!("daily at {time}"),
            Trigger::Daily { time, weekdays } => format!("{} at {time}", weekdays.join(",")),
            Trigger::FileChanged { path } => format!("when {path} changes"),
            Trigger::Command { command, every_minutes, when } => {
                let when = match when {
                    CommandWhen::Succeeds => "succeeds",
                    CommandWhen::Changes => "output changes",
                };
                format!("when `{command}` {when} (checked every {every_minutes} min)")
            }
        }
    }

    /// Why this trigger cannot work, checked when it is added.
    pub fn problem(&self) -> Option<String> {
        match self {
            Trigger::At { at } => parse_time(at).is_none().then(|| format!("cannot read the time {at:?}")),
            Trigger::Every { minutes } => (*minutes == 0 || *minutes > MAX_MINUTES)
                .then(|| format!("every needs minutes between 1 and {MAX_MINUTES} (ten years)")),
            Trigger::Daily { time, weekdays } => {
                if NaiveTime::parse_from_str(time, "%H:%M").is_err() {
                    return Some(format!("daily needs HH:MM, not {time:?}"));
                }
                weekdays
                    .iter()
                    .find(|d| weekday(d).is_none())
                    .map(|d| format!("unknown weekday {d:?} (mon, tue, wed, thu, fri, sat, sun)"))
            }
            Trigger::FileChanged { path } => path.trim().is_empty().then(|| "fileChanged needs a path".into()),
            Trigger::Command { command, every_minutes, .. } => {
                if command.trim().is_empty() {
                    return Some("command needs a command".into());
                }
                (*every_minutes > MAX_MINUTES).then(|| format!("everyMinutes may be at most {MAX_MINUTES} (ten years)"))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Action {
    /// Tell the user, through the front end and the notification channels.
    Notify { text: String },
    /// Run this as a task for the agent, in its own process.
    Prompt { text: String },
}

impl Action {
    pub fn text(&self) -> &str {
        match self {
            Action::Notify { text } | Action::Prompt { text } => text,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ScheduleEvent {
    Added {
        id: String,
        at: String,
        trigger: Trigger,
        action: Action,
        /// Who set it: "user" or the agent's name.
        by: String,
        /// Where a prompt or a command runs.
        cwd: PathBuf,
        /// The folder on another device a prompt runs at; `cwd` is then where the hub keeps
        /// that conversation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote: Option<DeviceFolder>,
        /// Conditions fire once unless this is set.
        #[serde(default)]
        repeat: bool,
    },
    /// A condition's observed state changed without firing (its first check).
    Observed {
        id: String,
        at: String,
        observed: String,
    },
    Fired {
        id: String,
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed: Option<String>,
    },
    Cancelled {
        id: String,
        at: String,
        by: String,
    },
    /// The goal the schedule serves and the Principal's activation that set it (decision D85).
    Campaign {
        id: String,
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        activation: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: String,
    pub trigger: Trigger,
    pub action: Action,
    pub by: String,
    pub cwd: PathBuf,
    /// The folder on another device a prompt runs at, when it is not on this computer.
    pub remote: Option<DeviceFolder>,
    pub repeat: bool,
    pub created: String,
    pub last_fired: Option<String>,
    pub fired: usize,
    pub observed: Option<String>,
    /// A one-shot that fired, or cancelled.
    pub finished: bool,
    /// The goal (`g-…`) it serves, when it serves one (decision D85).
    pub goal: Option<String>,
    /// The Principal's activation (`a-…`) that set it.
    pub spawned_by_activation: Option<String>,
}

impl Schedule {
    pub fn is_one_shot(&self) -> bool {
        matches!(self.trigger, Trigger::At { .. }) || (self.trigger.is_condition() && !self.repeat)
    }
}

/// A schedule that is due now.
#[derive(Clone, Debug)]
pub struct Firing {
    pub schedule: Schedule,
    pub observed: Option<String>,
    /// The round that found it due; recorded as the firing time.
    pub at: DateTime<Utc>,
}

/// Every schedule as the log's events leave it, in the order they were added.
#[derive(Default)]
struct All {
    list: Vec<Schedule>,
    /// Where each id is in `list`.
    place: std::collections::HashMap<String, usize>,
}

/// Take one line of the log into the fold. Unreadable lines are skipped.
fn take_in(all: &mut All, line: &str, _: u64) {
    let Ok(event) = serde_json::from_str::<ScheduleEvent>(line) else { return };
    let known = |all: &mut All, id: &str| all.place.get(id).copied();
    match event {
        ScheduleEvent::Added { id, at, trigger, action, by, cwd, remote, repeat } => {
            let schedule = Schedule {
                id: id.clone(),
                trigger,
                action,
                by,
                cwd,
                remote,
                repeat,
                created: at,
                last_fired: None,
                fired: 0,
                observed: None,
                finished: false,
                goal: None,
                spawned_by_activation: None,
            };
            match known(all, &id) {
                Some(index) => all.list[index] = schedule,
                None => {
                    all.place.insert(id, all.list.len());
                    all.list.push(schedule);
                }
            }
        }
        ScheduleEvent::Observed { id, observed, .. } => {
            if let Some(index) = known(all, &id) {
                all.list[index].observed = Some(observed);
            }
        }
        ScheduleEvent::Fired { id, at, observed } => {
            if let Some(index) = known(all, &id) {
                let s = &mut all.list[index];
                s.last_fired = Some(at);
                s.fired += 1;
                if observed.is_some() {
                    s.observed = observed;
                }
                s.finished |= s.is_one_shot();
            }
        }
        ScheduleEvent::Cancelled { id, .. } => {
            if let Some(index) = known(all, &id) {
                all.list[index].finished = true;
            }
        }
        ScheduleEvent::Campaign { id, goal, activation, .. } => {
            if let Some(index) = known(all, &id) {
                let schedule = &mut all.list[index];
                schedule.goal = goal.or(schedule.goal.take());
                schedule.spawned_by_activation = activation.or(schedule.spawned_by_activation.take());
            }
        }
    }
}

pub struct ScheduleStore {
    path: PathBuf,
    write: Mutex<()>,
    schedules: crate::jsonl::Folded<All>,
    /// Command conditions run under this: the agent wrote them, and they run unattended.
    sandbox: crate::sandbox::Sandbox,
    /// The hub's devices, when this store is the hub's: a command condition set at a folder on
    /// a device is checked there, by that device's node (decision D80).
    devices: Option<std::sync::Arc<dyn crate::place::DeviceLookup>>,
}

impl ScheduleStore {
    pub fn new(path: PathBuf) -> Self {
        let schedules = crate::jsonl::Folded::shared(&path);
        ScheduleStore { path, write: Mutex::new(()), schedules, sandbox: Default::default(), devices: None }
    }

    /// Check command conditions set at another device on that device.
    pub fn with_devices(mut self, devices: Option<std::sync::Arc<dyn crate::place::DeviceLookup>>) -> Self {
        self.devices = devices;
        self
    }

    pub fn sandboxed(mut self, sandbox: crate::sandbox::Sandbox) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, event: &ScheduleEvent) -> std::io::Result<()> {
        let _guard = self.write.lock().unwrap();
        crate::jsonl::append(&self.path, event)
    }

    /// Look at every schedule ever added, finished ones included, oldest first, without copying
    /// them. Only what was appended to the log since the last look is read.
    fn read<R>(&self, look: impl FnOnce(&[Schedule]) -> R) -> R {
        self.schedules.read(take_in, |all| look(&all.list))
    }

    /// Every schedule ever added, finished ones included, oldest first.
    pub fn load_all(&self) -> Vec<Schedule> {
        self.read(<[Schedule]>::to_vec)
    }

    /// Schedules still waiting to fire.
    pub fn active(&self) -> Vec<Schedule> {
        self.read(|all| all.iter().filter(|s| !s.finished).cloned().collect())
    }

    pub fn get(&self, id: &str) -> Option<Schedule> {
        self.read(|all| all.iter().find(|s| s.id == id).cloned())
    }

    pub fn add(&self, trigger: Trigger, action: Action, by: &str, cwd: &Path, repeat: bool) -> anyhow::Result<String> {
        self.add_at(trigger, action, by, cwd, None, repeat)
    }

    /// `add`, for a conversation whose place is `remote`, a folder on another device. A command
    /// condition is checked on that device where this store reaches it (the hub's); a watched
    /// file would be looked for on this computer, so that one is not taken.
    pub fn add_at(
        &self,
        trigger: Trigger,
        action: Action,
        by: &str,
        cwd: &Path,
        remote: Option<&DeviceFolder>,
        repeat: bool,
    ) -> anyhow::Result<String> {
        if let Some(remote) = remote
            && trigger.is_condition()
            && !(matches!(trigger, Trigger::Command { .. }) && self.devices.is_some())
        {
            anyhow::bail!(
                "this condition would be checked on the hub's computer, not on {}; use a command condition (it is \
                 run on {}), or a time, and check when it fires",
                remote.device,
                remote.device
            );
        }
        if let Some(problem) = trigger.problem() {
            anyhow::bail!("{problem}");
        }
        if action.text().trim().is_empty() {
            anyhow::bail!("the action needs text");
        }
        let id = format!("s-{}", &uuid::Uuid::now_v7().simple().to_string()[20..]);
        self.append(&ScheduleEvent::Added {
            id: id.clone(),
            at: now(),
            trigger,
            action,
            by: by.to_string(),
            cwd: cwd.to_path_buf(),
            remote: remote.cloned(),
            repeat,
        })?;
        Ok(id)
    }

    /// Record the goal `id` serves and the activation that set it.
    pub fn attribute(&self, id: &str, goal: Option<String>, activation: Option<String>) -> anyhow::Result<()> {
        if goal.is_none() && activation.is_none() {
            return Ok(());
        }
        anyhow::ensure!(self.get(id).is_some(), "no schedule {id}");
        self.append(&ScheduleEvent::Campaign { id: id.to_string(), at: now(), goal, activation })?;
        Ok(())
    }

    /// Check every active schedule once: times against `now`, conditions by looking (a file's
    /// fingerprint, a command's exit and output). Records first observations and returns what
    /// fires; the caller records each firing once it has acted on it. `checked` holds when each
    /// condition was last looked at, so commands run at their own interval.
    pub async fn due(&self, now: DateTime<Utc>, checked: &mut BTreeMap<String, DateTime<Utc>>) -> Vec<Firing> {
        let mut firings = Vec::new();
        for schedule in self.active() {
            let since = schedule.last_fired.as_deref().unwrap_or(&schedule.created);
            let since = parse_time(since).unwrap_or(now);
            match &schedule.trigger {
                Trigger::At { at } => {
                    if parse_time(at).is_some_and(|at| now >= at) {
                        firings.push(Firing { schedule, observed: None, at: now });
                    }
                }
                Trigger::Every { minutes } => {
                    if after(since, *minutes).is_some_and(|due| now >= due) {
                        firings.push(Firing { schedule, observed: None, at: now });
                    }
                }
                Trigger::Daily { time, weekdays } => {
                    if next_daily(since, time, weekdays).is_some_and(|next| now >= next) {
                        firings.push(Firing { schedule, observed: None, at: now });
                    }
                }
                Trigger::FileChanged { .. } | Trigger::Command { .. } => {
                    let every = match &schedule.trigger {
                        Trigger::Command { every_minutes, .. } => (*every_minutes).max(MIN_CHECK_MINUTES),
                        _ => MIN_CHECK_MINUTES,
                    };
                    // An interval no date can hold never comes due: skipped, not a crash.
                    let next = checked.get(&schedule.id).map(|at| after(*at, every));
                    if next.is_some_and(|next| next.is_none_or(|next| now < next)) {
                        continue;
                    }
                    checked.insert(schedule.id.clone(), now);
                    // A device that is away is not looked at: what it would say is not known,
                    // and a guess must neither fire the schedule nor become its last observation.
                    let Some(observed) = observe(&schedule, &self.sandbox, self.devices.as_deref()).await else {
                        continue;
                    };
                    let fires = match (&schedule.trigger, &schedule.observed) {
                        (Trigger::Command { when: CommandWhen::Succeeds, .. }, before) => {
                            observed.starts_with("ok") && before.as_deref().is_none_or(|b| !b.starts_with("ok"))
                        }
                        (_, None) => false,
                        (_, Some(before)) => *before != observed,
                    };
                    if fires {
                        firings.push(Firing { schedule, observed: Some(observed), at: now });
                    } else if schedule.observed.as_deref() != Some(observed.as_str()) {
                        let _ = self.append(&ScheduleEvent::Observed {
                            id: schedule.id.clone(),
                            at: now.to_rfc3339(),
                            observed,
                        });
                    }
                }
            }
        }
        firings
    }

    pub fn record_fired(&self, firing: &Firing) -> std::io::Result<()> {
        let at = firing.at.to_rfc3339();
        self.append(&ScheduleEvent::Fired { id: firing.schedule.id.clone(), at, observed: firing.observed.clone() })
    }
}

/// A condition's current state, as a short fingerprint: the file's size and modification time,
/// or the command's outcome and a hash of its output.
/// `start` plus `minutes`, or None when no date can hold it. The numbers come from the model and
/// from a file on disk, so a schedule the checks at `add` never saw must still not panic the
/// scheduler every round.
fn after(start: DateTime<Utc>, minutes: u64) -> Option<DateTime<Utc>> {
    let minutes = chrono::Duration::try_minutes(i64::try_from(minutes).ok()?)?;
    start.checked_add_signed(minutes)
}

/// Run a command condition on the device it was set at, through that device's node: under that
/// device's own guard and sandbox, like any call. None while the device cannot be reached.
async fn observe_on_device(
    devices: &dyn crate::place::DeviceLookup,
    remote: &DeviceFolder,
    command: &str,
) -> Option<String> {
    use nypi_node::proto::{Block, Call, FromNode};
    let node = devices.node(&remote.device).ok()?;
    let id = format!("c-{}", uuid::Uuid::now_v7().simple());
    let call = Call {
        id: id.clone(),
        task: None,
        tool: "bash".into(),
        arguments: serde_json::json!({ "command": command }),
        folder: remote.folder.clone(),
    };
    let (events, mut heard) = tokio::sync::mpsc::unbounded_channel();
    if tokio::time::timeout(COMMAND_TIMEOUT, node.call(call, events)).await.is_err() {
        node.cancel(&id);
        return Some("timeout".into());
    }
    let mut last = None;
    while let Ok(event) = heard.try_recv() {
        if event.ends_call() {
            last = Some(event);
        }
    }
    Some(match last? {
        FromNode::Finished { result, .. } => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for block in &result.content {
                if let Block::Text { text } = block {
                    text.hash(&mut hasher);
                }
            }
            let state = if result.is_error { "failed" } else { "ok" };
            format!("{state}:{:016x}", hasher.finish())
        }
        // The device's own rules said no: an observation of its own, which is never "ok".
        FromNode::Refused { reason, .. } => format!("refused:{reason}"),
        _ => return None,
    })
}

async fn observe(
    schedule: &Schedule,
    sandbox: &crate::sandbox::Sandbox,
    devices: Option<&dyn crate::place::DeviceLookup>,
) -> Option<String> {
    if let (Some(remote), Trigger::Command { command, .. }) = (&schedule.remote, &schedule.trigger) {
        // Never on this computer: the command was written for that device's files.
        return observe_on_device(devices?, remote, command).await;
    }
    Some(match &schedule.trigger {
        Trigger::FileChanged { path } => {
            let path = crate::config::expand_home(path);
            let path = if path.is_absolute() { path } else { schedule.cwd.join(path) };
            match std::fs::metadata(&path) {
                Ok(meta) => {
                    let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
                    format!("{}:{}", meta.len(), modified.map_or(0, |d| d.as_nanos()))
                }
                Err(_) => "missing".into(),
            }
        }
        Trigger::Command { command, .. } => {
            let mut cmd = tokio::process::Command::from(sandbox.command("bash"));
            cmd.arg("-c").arg(command).current_dir(&schedule.cwd).stdin(std::process::Stdio::null()).kill_on_drop(true);
            match tokio::time::timeout(COMMAND_TIMEOUT, cmd.output()).await {
                Ok(Ok(output)) => {
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    output.stdout.hash(&mut hasher);
                    let state = if output.status.success() { "ok" } else { "failed" };
                    format!("{state}:{:016x}", hasher.finish())
                }
                Ok(Err(error)) => format!("error:{error}"),
                Err(_) => "timeout".into(),
            }
        }
        _ => String::new(),
    })
}

/// RFC 3339, or a local `YYYY-MM-DD HH:MM` as people write it.
pub fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    if let Ok(t) = DateTime::parse_from_rfc3339(text.trim()) {
        return Some(t.with_timezone(&Utc));
    }
    let naive = chrono::NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M").ok()?;
    Local.from_local_datetime(&naive).earliest().map(|t| t.with_timezone(&Utc))
}

fn weekday(text: &str) -> Option<chrono::Weekday> {
    text.trim().get(..3)?.to_lowercase().parse::<chrono::Weekday>().ok()
}

/// The first `HH:MM` (local) on an allowed weekday strictly after `after`. Missed occurrences
/// while nothing ran collapse into one: the next is counted from the last firing.
pub fn next_daily(after: DateTime<Utc>, time: &str, weekdays: &[String]) -> Option<DateTime<Utc>> {
    let time = NaiveTime::parse_from_str(time, "%H:%M").ok()?;
    let allowed: Vec<chrono::Weekday> = weekdays.iter().filter_map(|d| weekday(d)).collect();
    let local = after.with_timezone(&Local);
    for offset in 0..=8 {
        let day = local.date_naive() + chrono::Duration::days(offset);
        if !allowed.is_empty() && !allowed.contains(&day.weekday()) {
            continue;
        }
        let Some(candidate) = Local.from_local_datetime(&day.and_time(time)).earliest() else { continue };
        let candidate = candidate.with_timezone(&Utc);
        if candidate > after {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ScheduleStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path().join("schedules.jsonl"));
        (dir, store)
    }

    #[tokio::test]
    async fn times_fire_when_due_and_one_shots_finish() {
        let (dir, store) = store();
        let notify = |t: &str| Action::Notify { text: t.into() };
        let soon = (Utc::now() + chrono::Duration::minutes(30)).to_rfc3339();
        let at = store.add(Trigger::At { at: soon }, notify("dentist"), "user", dir.path(), false).unwrap();
        let every = store.add(Trigger::Every { minutes: 60 }, notify("stretch"), "Lucia", dir.path(), false).unwrap();
        assert!(store.add(Trigger::Every { minutes: 0 }, notify("x"), "user", dir.path(), false).is_err());
        assert!(
            store
                .add(Trigger::Daily { time: "25:00".into(), weekdays: vec![] }, notify("x"), "user", dir.path(), false)
                .is_err()
        );

        let mut checked = BTreeMap::new();
        assert!(store.due(Utc::now(), &mut checked).await.is_empty(), "nothing is due yet");
        let later = Utc::now() + chrono::Duration::minutes(61);
        let firings = store.due(later, &mut checked).await;
        let ids: Vec<&str> = firings.iter().map(|f| f.schedule.id.as_str()).collect();
        assert_eq!(ids, [at.as_str(), every.as_str()]);
        for firing in &firings {
            store.record_fired(firing).unwrap();
        }
        assert!(store.get(&at).unwrap().finished, "a one-shot is done once it fired");
        assert_eq!(store.active().len(), 1);
        assert!(store.due(later, &mut checked).await.is_empty(), "a repeating timer waits a full interval again");
    }

    #[test]
    fn daily_skips_to_the_next_allowed_day() {
        let monday_noon = Local.with_ymd_and_hms(2026, 11, 2, 12, 0, 0).unwrap().with_timezone(&Utc);
        let next = next_daily(monday_noon, "09:30", &[]).unwrap().with_timezone(&Local);
        assert_eq!((next.day(), next.format("%H:%M").to_string()), (3, "09:30".into()), "tomorrow, since 09:30 passed");
        let next = next_daily(monday_noon, "09:30", &["fri".into()]).unwrap().with_timezone(&Local);
        assert_eq!(next.weekday(), chrono::Weekday::Fri);
        assert_eq!(
            parse_time("2026-11-02 08:15").map(|t| t.with_timezone(&Local).format("%H:%M").to_string()),
            Some("08:15".into())
        );
    }

    #[tokio::test]
    async fn conditions_fire_on_change_after_a_first_look() {
        let (dir, store) = store();
        let file = dir.path().join("build.log");
        std::fs::write(&file, "running").unwrap();
        let watch = store
            .add(
                Trigger::FileChanged { path: file.display().to_string() },
                Action::Notify { text: "log changed".into() },
                "user",
                dir.path(),
                false,
            )
            .unwrap();
        let flag = dir.path().join("done");
        let command = format!("test -f {}", flag.display());
        let build = store
            .add(
                Trigger::Command { command, every_minutes: 1, when: CommandWhen::Succeeds },
                Action::Prompt { text: "report the build".into() },
                "Lucia",
                dir.path(),
                false,
            )
            .unwrap();

        let mut t = Utc::now();
        let mut checked = BTreeMap::new();
        assert!(store.due(t, &mut checked).await.is_empty(), "the first look only records the state");
        assert!(store.get(&watch).unwrap().observed.is_some());
        std::fs::write(&file, "running... finished").unwrap();
        std::fs::write(&flag, "").unwrap();
        t += chrono::Duration::seconds(30);
        assert!(store.due(t, &mut checked).await.is_empty(), "not looked at again within the interval");
        t += chrono::Duration::minutes(1);
        let firings = store.due(t, &mut checked).await;
        let mut ids: Vec<&str> = firings.iter().map(|f| f.schedule.id.as_str()).collect();
        ids.sort();
        let mut expected = [watch.as_str(), build.as_str()];
        expected.sort();
        assert_eq!(ids, expected);
        for firing in &firings {
            store.record_fired(firing).unwrap();
        }
        assert!(store.active().is_empty(), "conditions fire once unless they repeat");
    }
    #[tokio::test]
    async fn an_interval_no_date_can_hold_never_comes_due_and_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path().join("schedules.jsonl"));
        let notify = |text: &str| Action::Notify { text: text.into() };
        assert!(store.add(Trigger::Every { minutes: u64::MAX }, notify("x"), "Lucia", dir.path(), false).is_err());
        // At another device a condition would be checked here, on the wrong machine; a time is fine.
        let laptop = DeviceFolder { device: "laptop".into(), folder: "/srv/site".into() };
        let changed = Trigger::FileChanged { path: "build.log".into() };
        let refused = store.add_at(changed, notify("x"), "Lucia", dir.path(), Some(&laptop), false).unwrap_err();
        assert!(refused.to_string().contains("not on laptop"), "{refused}");
        let at_laptop = store
            .add_at(Trigger::Every { minutes: 60 }, notify("x"), "Lucia", dir.path(), Some(&laptop), false)
            .unwrap();
        assert_eq!(store.get(&at_laptop).unwrap().remote, Some(laptop));
        // One written before the check existed (or by hand) must not stop the scheduler.
        let old = ScheduleEvent::Added {
            id: "s-old".into(),
            at: Utc::now().to_rfc3339(),
            trigger: Trigger::Every { minutes: u64::MAX },
            action: notify("never"),
            by: "Lucia".into(),
            cwd: dir.path().to_path_buf(),
            remote: None,
            repeat: false,
        };
        store.append(&old).unwrap();
        let soon = store.add(Trigger::Every { minutes: 1 }, notify("soon"), "user", dir.path(), false).unwrap();
        let due = store.due(Utc::now() + chrono::Duration::minutes(2), &mut BTreeMap::new()).await;
        assert_eq!(due.iter().map(|f| f.schedule.id.as_str()).collect::<Vec<_>>(), [soon.as_str()]);
        assert_eq!(after(Utc::now(), u64::MAX), None);
    }
}
