//! The hub's heartbeat file (`run/hub.json`): written every few seconds, read by the supervisor and
//! `nypi status` to tell a hub that is there from one that is hung or gone.

use super::*;

/// A hub whose last beat is older than this is hung or gone.
pub const STALE_SECONDS: i64 = 90;

pub(super) const BEAT_EVERY: Duration = Duration::from_secs(15);

/// What the hub writes every few seconds, for the supervisor and `nypi status`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat {
    pub pid: u32,
    pub started: String,
    pub beat: String,
    /// Clients attached right now.
    pub clients: usize,
    /// A run is under way.
    pub busy: bool,
    pub cwd: PathBuf,
    /// The message channels running inside the hub (decision D66), such as `telegram`.
    #[serde(default)]
    pub channels: Vec<String>,
    /// Open threads, the home thread included.
    #[serde(default)]
    pub threads: usize,
}

pub(super) fn heartbeat_path(paths: &Paths) -> PathBuf {
    paths.home.join("run").join("hub.json")
}

pub fn heartbeat(paths: &Paths) -> Option<Heartbeat> {
    serde_json::from_str(&std::fs::read_to_string(heartbeat_path(paths)).ok()?).ok()
}

/// The process a hub of before decision D100 recorded in `run/daemon.json`, when there is no
/// `run/hub.json`: what an upgraded supervisor still has to stop once.
pub fn legacy_pid(paths: &Paths) -> Option<u32> {
    if heartbeat_path(paths).exists() {
        return None;
    }
    let text = std::fs::read_to_string(paths.home.join("run").join("daemon.json")).ok()?;
    let beat: Heartbeat = serde_json::from_str(&text).ok()?;
    process_alive(beat.pid).then_some(beat.pid)
}

/// The hub's process now: its heartbeat's, or a pre-D100 hub's.
pub fn running_pid(paths: &Paths) -> Option<u32> {
    heartbeat(paths).map(|beat| beat.pid).filter(|pid| process_alive(*pid)).or_else(|| legacy_pid(paths))
}

/// Whether the hub is there and beating.
pub fn alive(paths: &Paths, now: DateTime<Utc>) -> bool {
    heartbeat(paths).is_some_and(|beat| {
        let age = DateTime::parse_from_rfc3339(&beat.beat)
            .map(|t| (now - t.with_timezone(&Utc)).num_seconds())
            .unwrap_or(i64::MAX);
        process_alive(beat.pid) && age < STALE_SECONDS
    })
}

pub(super) fn write_heartbeat(paths: &Paths, beat: &Heartbeat) -> std::io::Result<()> {
    let path = heartbeat_path(paths);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Written whole and renamed, so a reader never sees half of it.
    let draft = path.with_extension("json.tmp");
    std::fs::write(&draft, serde_json::to_vec(beat).map_err(std::io::Error::other)?)?;
    std::fs::rename(draft, path)
}

/// Take away what says the hub is there.
pub fn remove(paths: &Paths) {
    let _ = std::fs::remove_file(paths.hub_socket());
    let _ = std::fs::remove_file(paths.nodes_socket());
    forget_heartbeat(paths);
}

/// Drop a heartbeat whose hub is gone, so the next one is not mistaken for it. The socket
/// is left alone: binding replaces a dead one and refuses to take a live one.
pub fn forget_heartbeat(paths: &Paths) {
    let _ = std::fs::remove_file(heartbeat_path(paths));
}
