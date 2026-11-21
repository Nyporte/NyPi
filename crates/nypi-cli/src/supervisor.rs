//! The supervisor (principle B6, decisions D60, D64, D93): `nypi supervisor` is a small process
//! with no model and no session that keeps running whatever this machine is. Every round it
//! reads `instance.json`: on the hub it keeps the daemon (`nypi hub run`) and the board there,
//! starting one that is not running, killing and starting again one that hangs, and telling the
//! user when it had to (a service that keeps stopping is retried more slowly and reported as an
//! error); and it does the rounds that must not depend on the daemon being well (due schedules
//! and DMN passes, stalled and interrupted tasks, notices no channel has taken), sharing the
//! watchdog's and scheduler's locks with the daemon so nothing is reported twice. On a node it
//! keeps `nypi node run` there and does no rounds: they are the hub's. When the mode changes
//! between rounds, what the old mode ran is stopped before the new mode's services start.
//! `nypi hub up` and `nypi node up` install it as a service (a macOS LaunchAgent, or a systemd
//! user unit on Linux); the last `down` removes it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use nypi_core::config::{Paths, Settings};
use nypi_core::inspector::AlertLevel;
use nypi_core::ledger::{self, process_alive};
use nypi_core::outbox::Outbox;
use serde::{Deserialize, Serialize};

use crate::notify::{self, Notice, Notifier};
use crate::{daemon, scheduler, watchdog};

pub const LABEL: &str = "dev.nypi.supervisor";
/// A supervisor whose last round is older than this is hung or gone.
const STALE_SECONDS: i64 = 180;
/// launchd starts services with a bare PATH; tools the agent runs need the usual places.
const DEFAULT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// What the supervisor writes after every round, for `status` and for front ends that tell
/// the user when nothing is watching.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat {
    pub pid: u32,
    pub started: String,
    pub last_round: String,
    pub rounds: u64,
    /// Notices raised since it started.
    pub reports: u64,
}

fn heartbeat_path(paths: &Paths) -> PathBuf {
    paths.home.join("run").join("supervisor.json")
}

pub fn heartbeat(paths: &Paths) -> Option<Heartbeat> {
    serde_json::from_str(&std::fs::read_to_string(heartbeat_path(paths)).ok()?).ok()
}

/// Whether a supervisor is doing its rounds right now.
pub fn alive(paths: &Paths, now: DateTime<Utc>) -> bool {
    heartbeat(paths).is_some_and(|beat| {
        let age = DateTime::parse_from_rfc3339(&beat.last_round)
            .map(|t| (now - t.with_timezone(&Utc)).num_seconds())
            .unwrap_or(i64::MAX);
        process_alive(beat.pid) && age < STALE_SECONDS
    })
}

/// A daemon that was started this long ago and has no heartbeat yet is still starting.
const STARTUP_GRACE: Duration = Duration::from_secs(60);
/// This many starts within `CRASH_WINDOW` is a daemon that keeps stopping.
const CRASH_STARTS: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(600);
/// How long to leave a daemon that keeps stopping before trying again.
const CRASH_PAUSE: Duration = Duration::from_secs(300);

/// What the supervisor sees of the daemon in one look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Beating.
    Alive,
    /// Its process is there but has not beaten for too long.
    Hung { pid: u32 },
    /// No process.
    Down,
}

/// What to do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Start it (after killing `kill`, a hung one), and tell the user `say` when there is
    /// something to tell.
    Start {
        kill: Option<u32>,
        say: Option<(AlertLevel, String)>,
    },
}

/// A service the supervisor keeps running.
#[derive(Clone, Copy, Debug)]
pub struct Service {
    /// As the owner reads it at the start of a sentence.
    pub name: &'static str,
    /// What to add when telling the owner it was started again.
    pub after_restart: &'static str,
}

pub const DAEMON: Service = Service { name: "The hub", after_restart: " Any task it was on is resumed." };
pub const BOARD: Service = Service { name: "The board", after_restart: "" };
pub const NODE: Service = Service { name: "This machine's node", after_restart: "" };
pub const RELAY: Service = Service { name: "The relay", after_restart: "" };

/// What the supervisor keeps running on this machine (decisions D93, D94).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Services {
    /// The daemon, the board, and the hub's rounds.
    pub hub: bool,
    /// `nypi node run`.
    pub node: bool,
    /// `nypi relay run`, beside whichever of the two this machine is.
    pub relay: bool,
}

impl Services {
    pub fn any(self) -> bool {
        self.hub || self.node || self.relay
    }
}

/// What this machine runs, from its instance and `relay.enabled`: a hub runs the hub's services,
/// a node its node, and a stopped one (`down`) nothing; the relay runs beside either. A machine
/// that is neither runs the hub's, as every machine did before there were nodes, unless it runs
/// only a relay.
pub fn services(instance: Option<&nypi_hub::instance::Instance>, relay: bool) -> Services {
    let mut services = match instance {
        None => Services { hub: !relay, ..Default::default() },
        Some(instance) if instance.down => Services::default(),
        Some(instance) if instance.is_hub() => Services { hub: true, ..Default::default() },
        Some(_) => Services { node: true, ..Default::default() },
    };
    services.relay = relay;
    services
}

/// What the supervisor remembers about keeping one service alive.
#[derive(Default)]
pub struct Kept {
    /// The process this supervisor started, kept so its exit is seen the moment it happens.
    child: Option<tokio::process::Child>,
    /// When it was started, most recent last.
    starts: Vec<Instant>,
    /// No start is tried before this.
    not_before: Option<Instant>,
    /// It was seen alive since the last start.
    seen_alive: bool,
}

impl Kept {
    /// Decide from one look at the service. The first start is not news; a start after it was
    /// seen running is; a service that keeps stopping is an error, and is then left alone for a
    /// while so a broken setup does not spin.
    pub fn decide(&mut self, service: Service, health: Health, now: Instant, log: &Path) -> Step {
        if health == Health::Alive {
            self.seen_alive = true;
            return Step::Nothing;
        }
        let starting = self.starts.last().is_some_and(|at| now.duration_since(*at) < STARTUP_GRACE);
        if (starting && !self.seen_alive) || self.not_before.is_some_and(|at| now < at) {
            return Step::Nothing;
        }
        self.starts.retain(|at| now.duration_since(*at) < CRASH_WINDOW);
        let kill = match health {
            Health::Hung { pid } => Some(pid),
            _ => None,
        };
        let what = if kill.is_some() { "stopped responding" } else { "stopped" };
        let name = service.name;
        let say = if self.starts.len() + 1 >= CRASH_STARTS {
            self.not_before = Some(now + CRASH_PAUSE);
            Some((
                AlertLevel::Error,
                format!(
                    "Failed: {name} {what} {} times in {} minutes. It was started once more; the next try is in {} \
                     minutes. Its log: {}",
                    self.starts.len(),
                    CRASH_WINDOW.as_secs() / 60,
                    CRASH_PAUSE.as_secs() / 60,
                    log.display()
                ),
            ))
        } else if self.seen_alive || kill.is_some() {
            Some((AlertLevel::Warning, format!("{name} had {what} and was started again.{}", service.after_restart)))
        } else {
            None
        };
        self.starts.push(now);
        self.seen_alive = false;
        Step::Start { kill, say }
    }
}

/// What carries over between rounds.
#[derive(Default)]
pub struct State {
    /// When each condition trigger was last checked (the scheduler's own bookkeeping).
    checked: BTreeMap<String, DateTime<Utc>>,
    last_watchdog: Option<Instant>,
    daemon: Kept,
    board: Kept,
    node: Kept,
    relay: Kept,
    /// Since when every service of this machine has been up together, while an update is
    /// pending (decision D98).
    healthy_since: Option<Instant>,
    /// The supervisor ends after this round, so its service manager starts it again from the
    /// binary now installed (an update put in place, or rolled back).
    pub exit: bool,
    /// Carries out what the rounds find to do while no daemon is there to.
    jobs: Option<crate::jobs::Jobs>,
}

fn daemon_health(paths: &Paths, now: DateTime<Utc>) -> Health {
    if daemon::alive(paths, now) {
        return Health::Alive;
    }
    match daemon::heartbeat(paths) {
        Some(beat) if process_alive(beat.pid) => Health::Hung { pid: beat.pid },
        _ => Health::Down,
    }
}

/// Start `nypi <arguments>` as a child that logs to `log_file`. It stays in this process's
/// group, so stopping the service stops it too.
fn start_service(arguments: &[String], cwd: &Path, log_file: &Path) -> anyhow::Result<tokio::process::Child> {
    if let Some(dir) = log_file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log_file)?;
    tokio::process::Command::new(std::env::current_exe()?)
        .args(arguments)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .spawn()
        .with_context(|| format!("cannot start nypi {} in {}", arguments.join(" "), cwd.display()))
}

/// The board is there when the process that said it serves is alive, whoever started it. Its
/// socket file says nothing: a killed board leaves it behind.
fn board_health(paths: &Paths) -> Health {
    if crate::board::running(paths).is_some() { Health::Alive } else { Health::Down }
}

pub fn board_log(paths: &Paths) -> PathBuf {
    paths.home.join("logs").join("board.log")
}

/// The arguments the supervisor starts the board with.
fn board_arguments(settings: &Settings) -> Vec<String> {
    let mut arguments = vec!["board".to_string(), "--port".to_string(), settings.board.port.to_string()];
    if settings.board.local {
        arguments.push("--local".into());
    }
    arguments
}

fn signal(pid: u32, signal: libc::c_int) {
    // SAFETY: kill takes any pid and signal and reports failure through its return value.
    unsafe { libc::kill(pid as libc::pid_t, signal) };
}

/// Resolves when a service this supervisor started exits; never when it started none.
async fn exit(kept: &mut Kept) {
    match &mut kept.child {
        Some(child) => {
            let _ = child.wait().await;
        }
        None => std::future::pending().await,
    }
}

/// Resolves when a service this supervisor started exits.
async fn any_exit(state: &mut State) {
    let State { daemon, board, node, relay, .. } = state;
    tokio::select! {
        _ = exit(daemon) => {}
        _ = exit(board) => {}
        _ = exit(node) => {}
        _ = exit(relay) => {}
    }
}

/// Stop a service this mode no longer runs: the child this supervisor started, and one that
/// another process started (`pid`).
async fn stop(service: Service, kept: &mut Kept, pid: Option<u32>, why: &str) {
    if let Some(mut child) = kept.child.take() {
        if let Some(pid) = child.id() {
            signal(pid, libc::SIGTERM);
        }
        let _ = child.wait().await;
        log(&format!("{} stopped: {why}", service.name));
    } else if let Some(pid) = pid.filter(|pid| process_alive(*pid) && *pid != std::process::id()) {
        signal(pid, libc::SIGTERM);
        // Gone before anything is started in its place: a new daemon must find its socket free.
        for _ in 0..100 {
            if !process_alive(pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if process_alive(pid) {
            signal(pid, libc::SIGKILL);
        }
        log(&format!("{} (pid {pid}) stopped: {why}", service.name));
    }
    *kept = Kept::default();
}

/// The processes of this machine's services as their own files record them: those this
/// supervisor started, and those an earlier one did.
fn recorded_pids(paths: &Paths) -> [Option<u32>; 4] {
    [
        // A hub of before decision D100 recorded itself in `run/daemon.json`.
        daemon::running_pid(paths),
        crate::board::running(paths).map(|serving| serving.pid),
        crate::node::read_status(paths).map(|status| status.pid).filter(|pid| process_alive(*pid)),
        nypi_hub::relay::running(paths),
    ]
}

/// Stop every service of this machine (decision D99): the children this supervisor started, and
/// the processes an earlier supervisor left running, which its own children are not. An update
/// is not in place until nothing of the old binary runs.
async fn stop_everything(paths: &Paths, state: &mut State, why: &str) {
    let [daemon_pid, board_pid, node_pid, relay_pid] = recorded_pids(paths);
    stop(DAEMON, &mut state.daemon, daemon_pid, why).await;
    stop(BOARD, &mut state.board, board_pid, why).await;
    stop(NODE, &mut state.node, node_pid, why).await;
    stop(RELAY, &mut state.relay, relay_pid, why).await;
}

fn node_health(paths: &Paths) -> Health {
    match crate::node::read_status(paths) {
        Some(status) if process_alive(status.pid) => Health::Alive,
        _ => Health::Down,
    }
}

fn relay_health(paths: &Paths) -> Health {
    match nypi_hub::relay::running(paths) {
        Some(_) => Health::Alive,
        None => Health::Down,
    }
}

/// Keep one service there: look, decide, act. Returns what the user should hear.
fn keep(
    service: Service,
    kept: &mut Kept,
    enabled: bool,
    health: impl FnOnce() -> Health,
    log_file: &Path,
    start: impl FnOnce() -> anyhow::Result<tokio::process::Child>,
) -> Option<(AlertLevel, String)> {
    let name = service.name;
    if let Some(child) = &mut kept.child
        && let Ok(Some(status)) = child.try_wait()
    {
        log(&format!("{name} exited ({status})"));
        kept.child = None;
    }
    if !enabled {
        return None;
    }
    let Step::Start { kill, say } = kept.decide(service, health(), Instant::now(), log_file) else { return None };
    if let Some(pid) = kill {
        log(&format!("{name} (pid {pid}) is not responding; killing it"));
        signal(pid, libc::SIGKILL);
    }
    match start() {
        Ok(child) => {
            log(&format!("{name} started (pid {})", child.id().unwrap_or_default()));
            kept.child = Some(child);
            say
        }
        Err(error) => {
            log(&format!("{name} not started: {error:#}"));
            Some((AlertLevel::Error, format!("Failed: {name} could not be started: {error:#}")))
        }
    }
}

const MODE_CHANGED: &str = "this machine's mode changed";

/// Stop what this machine's mode no longer runs, before anything of the new mode starts.
async fn stop_others(paths: &Paths, wanted: Services, state: &mut State) {
    if !wanted.hub {
        let daemon_pid = daemon::running_pid(paths);
        if state.daemon.child.is_some() || daemon_pid.is_some_and(process_alive) {
            stop(DAEMON, &mut state.daemon, daemon_pid, MODE_CHANGED).await;
        }
        let board_pid = crate::board::running(paths).map(|serving| serving.pid);
        if state.board.child.is_some() || board_pid.is_some() {
            stop(BOARD, &mut state.board, board_pid, MODE_CHANGED).await;
        }
    }
    if !wanted.node {
        let node_pid = crate::node::read_status(paths).map(|status| status.pid);
        if state.node.child.is_some() || node_pid.is_some_and(process_alive) {
            stop(NODE, &mut state.node, node_pid, MODE_CHANGED).await;
        }
    }
    if !wanted.relay {
        let relay_pid = nypi_hub::relay::running(paths);
        if state.relay.child.is_some() || relay_pid.is_some() {
            stop(RELAY, &mut state.relay, relay_pid, MODE_CHANGED).await;
        }
    }
}

/// Keep this mode's services there.
fn keep_services(paths: &Paths, settings: &Settings, wanted: Services, state: &mut State) -> Vec<(AlertLevel, String)> {
    let home = dirs::home_dir().unwrap_or_else(|| paths.home.clone());
    let daemon_log = daemon::log_path(paths);
    let daemon = keep(
        DAEMON,
        &mut state.daemon,
        wanted.hub && settings.hub.enabled,
        || daemon_health(paths, Utc::now()),
        &daemon_log,
        || {
            // A dead daemon's heartbeat must not be read as the new one's.
            daemon::forget_heartbeat(paths);
            let run = ["hub".to_string(), "run".to_string()];
            start_service(&run, &daemon::working_dir(settings.hub.cwd.as_deref()), &daemon_log)
        },
    );
    let board_log = board_log(paths);
    let board = keep(
        BOARD,
        &mut state.board,
        wanted.hub && settings.board.enabled,
        || board_health(paths),
        &board_log,
        || start_service(&board_arguments(settings), &home, &board_log),
    );
    let node_log = paths.home.join("logs").join("node.log");
    let node = keep(
        NODE,
        &mut state.node,
        wanted.node,
        || node_health(paths),
        &node_log,
        || start_service(&["node".to_string(), "run".to_string()], &home, &node_log),
    );
    let relay_log = paths.home.join("logs").join("relay.log");
    let relay = keep(
        RELAY,
        &mut state.relay,
        wanted.relay,
        || relay_health(paths),
        &relay_log,
        || start_service(&["relay".to_string(), "run".to_string()], &home, &relay_log),
    );
    daemon.into_iter().chain(board).chain(node).chain(relay).collect()
}

/// Whether every service this machine runs is up now.
fn all_alive(paths: &Paths, settings: &Settings, wanted: Services) -> bool {
    let daemon = !(wanted.hub && settings.hub.enabled) || daemon_health(paths, Utc::now()) == Health::Alive;
    let board = !(wanted.hub && settings.board.enabled) || board_health(paths) == Health::Alive;
    let node = !wanted.node || node_health(paths) == Health::Alive;
    let relay = !wanted.relay || relay_health(paths) == Health::Alive;
    daemon && board && node && relay
}

/// A pending update this round: wait, confirm it, or roll it back and end the supervisor.
pub(crate) async fn watch_update(
    paths: &Paths,
    settings: &Settings,
    wanted: Services,
    state: &mut State,
    pending: &crate::update::Pending,
) -> Option<(&'static str, AlertLevel, String)> {
    let now = Instant::now();
    state.healthy_since = match all_alive(paths, settings, wanted) {
        true => Some(state.healthy_since.unwrap_or(now)),
        false => None,
    };
    let crashed =
        [&state.daemon, &state.board, &state.node, &state.relay].iter().any(|kept| kept.starts.len() >= CRASH_STARTS);
    let up = state.healthy_since.map(|since| now.duration_since(since));
    match crate::update::verdict(pending, Utc::now(), up, crashed) {
        crate::update::Verdict::Wait => None,
        crate::update::Verdict::Confirm => {
            crate::update::confirm(paths);
            state.healthy_since = None;
            log(&format!("the update to {} stayed up; it is kept", pending.to_version));
            pending.automatic.then(|| {
                ("update", AlertLevel::Notice, format!("NyPi {} is in place and running.", pending.to_version))
            })
        }
        crate::update::Verdict::RollBack if crate::update::can_roll_back(paths) => {
            stop_everything(paths, state, "an update did not stay up").await;
            match crate::update::roll_back_failed(paths) {
                Ok(said) => {
                    log(&said);
                    state.exit = true;
                    Some(("update", AlertLevel::Error, said))
                }
                Err(error) => {
                    Some(("update", AlertLevel::Error, format!("Failed: the rollback did not work: {error:#}")))
                }
            }
        }
        crate::update::Verdict::RollBack => {
            crate::update::clear_pending(paths);
            Some((
                "update",
                AlertLevel::Error,
                format!(
                    "Failed: the update to {} did not stay healthy, and there is no previous binary to put back.",
                    pending.to_version
                ),
            ))
        }
    }
}

/// Whether this machine is idle enough to be updated unasked: no activation of the Principal,
/// no task running, no call under way on its node.
fn idle(paths: &Paths) -> bool {
    let principal: serde_json::Value = std::fs::read_to_string(paths.principal_status())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let tasks_running = ledger::Ledger::new(paths.ledger())
        .load()
        .values()
        .any(|task| task.state == ledger::TaskState::Running && process_alive(task.pid));
    let calls =
        crate::node::read_status(paths).filter(|status| process_alive(status.pid)).map_or(0, |status| status.running);
    principal["active"].is_null() && !tasks_running && calls == 0
}

/// The background check of `update.mode` (decision D98).
async fn check_for_update(
    paths: &Paths,
    settings: &Settings,
    wanted: Services,
    state: &mut State,
) -> Option<(&'static str, AlertLevel, String)> {
    if !wanted.any() {
        return None;
    }
    let updater = crate::update::Updater::new(paths, settings).ok()?;
    let told = std::sync::Mutex::new(None);
    let checking = crate::update::background(
        paths,
        settings,
        &updater,
        || idle(paths),
        async |text: String| {
            *told.lock().unwrap() = Some(text);
        },
    );
    let applied = tokio::time::timeout(Duration::from_secs(120), checking).await.unwrap_or(false);
    if applied {
        log("an update was put in place; the supervisor starts again on it");
        stop_everything(paths, state, "an update was put in place").await;
        state.exit = true;
    }
    told.into_inner().unwrap().map(|text| ("update", AlertLevel::Notice, text))
}

/// One round: fire what is due, check the ledger when its interval has passed, post what was
/// found, and send again what is still open. Returns what was raised, for the log.
pub async fn round(
    paths: &Paths,
    settings: &Settings,
    notifier: Option<&Notifier>,
    state: &mut State,
) -> Vec<(&'static str, AlertLevel, String)> {
    let mut raised = Vec::new();
    let wanted = services(nypi_hub::instance::Instance::load(paths).as_ref(), settings.relay.enabled);
    stop_others(paths, wanted, state).await;
    restart_on_new_binary(paths, state).await;
    for (level, message) in keep_services(paths, settings, wanted, state) {
        raised.push(("supervisor", level, message));
    }
    // A new binary put in place (decision D98): confirmed once everything stays up, put back
    // when it does not.
    match crate::update::pending(paths) {
        Some(pending) => {
            if let Some(said) = watch_update(paths, settings, wanted, state, &pending).await {
                raised.push(said);
            }
        }
        None => {
            state.healthy_since = None;
            if let Some(said) = check_for_update(paths, settings, wanted, state).await {
                raised.push(said);
            }
        }
    }
    // The scheduler and the watchdog are modules of the daemon. The supervisor does their rounds
    // only on the hub, while no daemon is there (it is off, down, or hung), and then carries out
    // here what they find to do. A node does none: the stores are the hub's.
    if wanted.hub && !daemon::alive(paths, Utc::now()) {
        let jobs =
            state.jobs.get_or_insert_with(|| crate::jobs::Jobs::here(paths, nypi_core::pulse::PulseBus::new())).clone();
        let fired = scheduler::run_once(paths, &settings.dmn, &mut state.checked, None);
        let fired = tokio::time::timeout(watchdog::ROUND_LIMIT, fired).await.unwrap_or_default();
        raised.extend(fired.reports.into_iter().map(|(level, message)| ("scheduler", level, message)));
        fired.orders.into_iter().for_each(|order| jobs.take(order));
        let interval = Duration::from_secs(settings.watchdog.interval_seconds.max(5));
        if settings.watchdog.enabled && state.last_watchdog.is_none_or(|at| at.elapsed() >= interval) {
            state.last_watchdog = Some(Instant::now());
            let found = watchdog::run_once(paths, &settings.watchdog);
            let found = tokio::time::timeout(watchdog::ROUND_LIMIT, found).await.unwrap_or_default();
            raised.extend(found.reports.into_iter().map(|(level, message)| ("watchdog", level, message)));
            found.orders.into_iter().for_each(|order| jobs.take(order));
        }
    }
    for (source, level, message) in &raised {
        let notice = Notice { source, level: *level, text: message, task: None, shown_in: None };
        notify::post(paths, notifier, notice).await;
    }
    notify::flush(paths, notifier).await;
    raised
}

/// Run until stopped. Settings are read again every round, so a channel added to
/// `settings.json` takes effect without a restart.
pub async fn run(paths: &Paths, cwd: &Path) -> anyhow::Result<i32> {
    // A new binary that keeps the supervisor itself from getting to its rounds (it fails or panics
    // before them) is put back here, before anything else it does (decision D99).
    if let Some(said) = guard_starts(paths) {
        log(&said);
        let settings = Settings::load(paths, cwd).unwrap_or_default();
        let notifier = Notifier::from_settings(&settings, paths, cwd, true);
        let notice = Notice { source: "update", level: AlertLevel::Error, text: &said, task: None, shown_in: None };
        notify::post(paths, notifier.as_ref(), notice).await;
        notify::flush(paths, notifier.as_ref()).await;
        return Ok(75);
    }
    let started = Utc::now().to_rfc3339();
    let mut beat =
        Heartbeat { pid: std::process::id(), started: started.clone(), last_round: started, rounds: 0, reports: 0 };
    if alive(paths, Utc::now()) {
        let other = heartbeat(paths).map(|b| b.pid).unwrap_or_default();
        println!("A supervisor is already running (pid {other}); this one exits.");
        return Ok(0);
    }
    log(&format!("supervisor started (pid {}), home {}", beat.pid, paths.home.display()));
    let mut state = State::default();
    let mut ticks = tokio::time::interval(scheduler::CHECK_EVERY);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            // The daemon or the board went away: do not wait for the next round to bring it back.
            _ = any_exit(&mut state) => {}
            _ = terminate.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
        // A settings file that does not parse must not stop the watching; say so and go on.
        let settings = Settings::load(paths, cwd).unwrap_or_else(|error| {
            log(&format!("settings: {error:#}; using defaults this round"));
            Settings::default()
        });
        nypi_ai::auth::load_env_file(&paths.env_file());
        for file in &settings.env_files {
            nypi_ai::auth::load_env_file(&nypi_core::config::expand_home(file));
        }
        let notifier = Notifier::from_settings(&settings, paths, cwd, true);
        for (source, level, message) in round(paths, &settings, notifier.as_ref(), &mut state).await {
            beat.reports += 1;
            log(&format!("{source} [{level:?}] {}", message.split_whitespace().collect::<Vec<_>>().join(" ")));
        }
        beat.rounds += 1;
        beat.last_round = Utc::now().to_rfc3339();
        if let Err(error) = write_heartbeat(paths, &beat) {
            log(&format!("heartbeat not written: {error}"));
        }
        // A new binary is in place, or the old one back: end, and be started again on it.
        if state.exit {
            log("supervisor stops, to be started again on the binary now installed");
            let _ = std::fs::remove_file(heartbeat_path(paths));
            return Ok(75);
        }
    }
    // What the supervisor runs goes with it, an earlier supervisor's processes too: a stopped
    // service must not leave Lucia running with nothing to watch her. A task she was on is
    // resumed at the next start.
    stop_everything(paths, &mut state, "the supervisor stopped").await;
    log("supervisor stopped");
    let _ = std::fs::remove_file(heartbeat_path(paths));
    Ok(0)
}

/// A new binary in place (decision D99): what still runs the old one, started by this supervisor
/// or an earlier one, is stopped once, so what starts next, and what is judged healthy, is the
/// new binary; the update's clock starts again from here. True when it stopped them.
pub(crate) async fn restart_on_new_binary(paths: &Paths, state: &mut State) -> bool {
    let Some(mut pending) = crate::update::pending(paths).filter(|pending| !pending.restarted) else { return false };
    stop_everything(paths, state, "an update was put in place; it starts again on the new binary").await;
    pending.restarted = true;
    pending.started_at = Utc::now().to_rfc3339();
    crate::update::save_pending(paths, &pending);
    state.healthy_since = None;
    true
}

/// How many starts of the supervisor a pending update gets before it is put back: the first
/// start on the new binary, and two more for a service manager's own retries.
pub const PENDING_STARTS: u32 = 3;

/// Count this start against a pending update; past `PENDING_STARTS` with a previous binary to
/// go back to, put it back. What to tell the user then.
pub fn guard_starts(paths: &Paths) -> Option<String> {
    let mut pending = crate::update::pending(paths)?;
    pending.supervisor_starts += 1;
    crate::update::save_pending(paths, &pending);
    if pending.supervisor_starts <= PENDING_STARTS || !crate::update::can_roll_back(paths) {
        return None;
    }
    crate::update::roll_back_failed(paths).ok()
}

fn write_heartbeat(paths: &Paths, beat: &Heartbeat) -> std::io::Result<()> {
    let path = heartbeat_path(paths);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Written whole and renamed, so a reader never sees half of it.
    let draft = path.with_extension("json.tmp");
    std::fs::write(&draft, serde_json::to_vec(beat).map_err(std::io::Error::other)?)?;
    std::fs::rename(draft, path)
}

/// One timestamped line on stdout, which launchd sends to `logs/supervisor.log`.
fn log(line: &str) {
    println!("{} {line}", Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
}

fn plist_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join("Library").join("LaunchAgents").join(format!("{LABEL}.plist")))
}

/// The systemd user unit's file on Linux.
fn unit_path() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().context("no home directory")?;
    Ok(home.join(".config").join("systemd").join("user").join("nypi-supervisor.service"))
}

/// Whether the supervisor's service is installed for this user.
pub fn installed() -> bool {
    let file = if cfg!(target_os = "macos") { plist_path() } else { unit_path() };
    file.is_ok_and(|path| path.exists())
}

/// The systemd user unit: started with the user's session, restarted whenever it exits. Only
/// `PATH` (and `NYPI_HOME` when it is not the default) go into its environment.
pub fn unit(binary: &Path, working_dir: &Path, path_env: &str, nypi_home: Option<&Path>) -> String {
    let nypi_home = nypi_home.map(|home| format!("Environment=NYPI_HOME={}\n", home.display())).unwrap_or_default();
    format!(
        "[Unit]\nDescription=NyPi supervisor: keeps this machine's hub or node running\nAfter=network-online.target\n\n\
         [Service]\nExecStart={binary} supervisor\nWorkingDirectory={working_dir}\nEnvironment=PATH={path_env}\n\
         {nypi_home}Restart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n",
        binary = binary.display(),
        working_dir = working_dir.display(),
    )
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The LaunchAgent: start at login, restart when it exits, log to one file. Only `PATH` (and
/// `NYPI_HOME` when it is not the default) go into its environment; keys come from the env
/// files, as for every NyPi process.
pub fn plist(binary: &Path, working_dir: &Path, log: &Path, path_env: &str, nypi_home: Option<&Path>) -> String {
    let nypi_home = nypi_home
        .map(|home| format!("<key>NYPI_HOME</key><string>{}</string>", xml(&home.display().to_string())))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{binary}</string><string>supervisor</string></array>
  <key>WorkingDirectory</key><string>{working_dir}</string>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path_env}</string>{nypi_home}</dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        binary = xml(&binary.display().to_string()),
        working_dir = xml(&working_dir.display().to_string()),
        path_env = xml(path_env),
        log = xml(&log.display().to_string()),
    )
}

fn launchctl(args: &[&str]) -> anyhow::Result<std::process::Output> {
    std::process::Command::new("launchctl").args(args).output().context("cannot run launchctl")
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Whether `up` may install a service: `NYPI_SERVICE=off` keeps it from doing so (tests, and
/// machines whose service manager runs `nypi supervisor` itself).
pub fn service_wanted() -> bool {
    std::env::var("NYPI_SERVICE").map(|value| value != "off").unwrap_or(true)
}

/// Copy this binary to `~/.nypi/bin/nypi` (a path that survives `cargo clean`) and install the
/// supervisor as this user's service: a LaunchAgent on macOS, a systemd user unit on Linux.
/// Installing again replaces it, which is how a rebuilt NyPi is put in place.
pub fn install(paths: &Paths) -> anyhow::Result<()> {
    if !service_wanted() {
        return Ok(());
    }
    let binary = paths.home.join("bin").join("nypi");
    let log = paths.home.join("logs").join("supervisor.log");
    let current = std::env::current_exe()?;
    for dir in [binary.parent(), log.parent()].into_iter().flatten() {
        std::fs::create_dir_all(dir)?;
    }
    if current.canonicalize().ok() != binary.canonicalize().ok() {
        // Replace, never write into, a binary that may be running.
        let staged = binary.with_extension("new");
        std::fs::copy(&current, &staged).with_context(|| format!("cannot copy {}", current.display()))?;
        std::fs::rename(&staged, &binary)?;
    }
    let working_dir = dirs::home_dir().context("no home directory")?;
    let path_env = std::env::var("PATH").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| DEFAULT_PATH.into());
    let nypi_home = std::env::var_os("NYPI_HOME").map(|_| paths.home.clone());
    if cfg!(target_os = "macos") {
        let plist_file = plist_path()?;
        if let Some(dir) = plist_file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&plist_file, plist(&binary, &working_dir, &log, &path_env, nypi_home.as_deref()))?;
        let service = format!("{}/{LABEL}", domain());
        let _ = launchctl(&["bootout", &service]);
        let started = launchctl(&["bootstrap", &domain(), &plist_file.display().to_string()])?;
        if !started.status.success() {
            anyhow::bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&started.stderr).trim());
        }
    } else {
        let unit_file = unit_path()?;
        if let Some(dir) = unit_file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&unit_file, unit(&binary, &working_dir, &path_env, nypi_home.as_deref()))?;
        systemctl(&["daemon-reload"])?;
        let started = systemctl(&["enable", "--now", "nypi-supervisor.service"])?;
        if !started.status.success() {
            anyhow::bail!("systemctl --user enable failed: {}", String::from_utf8_lossy(&started.stderr).trim());
        }
        let user = std::env::var("USER").unwrap_or_else(|_| "<user>".into());
        println!("To keep it running while you are logged out: loginctl enable-linger {user}");
    }
    Ok(())
}

fn systemctl(args: &[&str]) -> anyhow::Result<std::process::Output> {
    std::process::Command::new("systemctl").arg("--user").args(args).output().context("cannot run systemctl --user")
}

/// Start the supervisor's service again (after `nypi update` put a new binary in place): it
/// stops what it ran, and starts it again from the binary now installed. Nothing when no service
/// is installed.
pub fn restart() -> anyhow::Result<()> {
    if !service_wanted() || !installed() {
        return Ok(());
    }
    let done = if cfg!(target_os = "macos") {
        launchctl(&["kickstart", "-k", &format!("{}/{LABEL}", domain())])?
    } else {
        systemctl(&["restart", "nypi-supervisor.service"])?
    };
    anyhow::ensure!(
        done.status.success(),
        "the supervisor was not restarted: {}",
        String::from_utf8_lossy(&done.stderr).trim()
    );
    Ok(())
}

/// Stop the supervisor's service and remove it; what it ran stops with it.
pub fn uninstall(paths: &Paths) -> anyhow::Result<()> {
    if !service_wanted() {
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
        let _ = std::fs::remove_file(plist_path()?);
    } else {
        let _ = systemctl(&["disable", "--now", "nypi-supervisor.service"]);
        let _ = std::fs::remove_file(unit_path()?);
        let _ = systemctl(&["daemon-reload"]);
    }
    // A daemon or node that outlived its supervisor (it was killed, not stopped) goes too.
    if let Some(pid) = daemon::running_pid(paths) {
        signal(pid, libc::SIGTERM);
    }
    if let Some(status) = crate::node::read_status(paths).filter(|status| process_alive(status.pid)) {
        signal(status.pid, libc::SIGTERM);
    }
    Ok(())
}

/// What `nypi status` says of the supervisor and the services on this machine.
pub fn report(paths: &Paths) -> Vec<String> {
    let now = Utc::now();
    let mut lines = Vec::new();
    lines.push(match (heartbeat(paths), alive(paths, now)) {
        (Some(beat), true) => format!(
            "Supervisor: running (pid {}), {} round(s), last at {} UTC{}.",
            beat.pid,
            beat.rounds,
            beat.last_round.get(11..19).unwrap_or_default(),
            if installed() { ", installed as a service" } else { "" }
        ),
        (_, false) if installed() => "Supervisor: installed but NOT running.".into(),
        _ => "Supervisor: not running (nypi hub up or nypi node up starts it).".into(),
    });
    let open = Outbox::new(paths.outbox()).open().len();
    if open > 0 {
        lines.push(format!("{open} notice(s) have not reached you: nypi notices"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Linux service (decision D93): a systemd user unit that runs the supervisor and is
    /// started again whenever it exits.
    #[test]
    fn the_linux_service_restarts_the_supervisor() {
        let unit = unit(Path::new("/home/u/.nypi/bin/nypi"), Path::new("/home/u"), "/usr/bin:/bin", None);
        assert!(unit.contains("ExecStart=/home/u/.nypi/bin/nypi supervisor"), "{unit}");
        assert!(unit.contains("Restart=always") && unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("NYPI_HOME"));
        let other = unit_for_home();
        assert!(other.contains("Environment=NYPI_HOME=/srv/nypi"), "{other}");
    }

    fn unit_for_home() -> String {
        unit(Path::new("/x/nypi"), Path::new("/x"), "/bin", Some(Path::new("/srv/nypi")))
    }
}
