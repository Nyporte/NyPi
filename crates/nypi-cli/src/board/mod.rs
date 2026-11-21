//! `nypi board`: the owner's board (decision D56, design in docs/board.md). A long-lived
//! service: every NyPi runtime links to it over `~/.nypi/run/board.sock` and reports what it
//! does; the board joins that with what NyPi stores (ledger, audit log, schedules) and serves it
//! to a page compiled from Rust to WebAssembly (`crates/nypi-board-ui`), live, with the owner's
//! actions (approve, deny, stop, message) going back the same way.

mod hub;
mod planes;
mod public;
mod records;
mod topology;
mod web;

use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nypi_board_proto::Now;
use nypi_core::audit_log::{self, AuditTail};
use nypi_core::board_link::{LinkDown, LinkUp};
use nypi_core::config::Paths;
use nypi_core::ledger::{self, Ledger};
use nypi_core::schedule::ScheduleStore;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, watch};

use hub::{Change, Hub};

/// How long changes gather before the home screen is sent again.
const SETTLE: Duration = Duration::from_millis(200);
/// How often the board looks at the audit log when nothing is linked.
const LOOK: Duration = Duration::from_secs(2);
/// Inbox items the owner took out, remembered across restarts.
const ACKNOWLEDGED_KEPT: usize = 500;

pub struct Board {
    paths: Paths,
    token: String,
    /// Required on every action, so only the board's own page can act.
    csrf: String,
    hub: Hub,
    day: Mutex<Day>,
    acknowledged: Mutex<VecDeque<String>>,
    now: watch::Sender<Arc<Now>>,
    /// Store counts for the System screen, and when they were read.
    slow: Mutex<Option<(Instant, topology::Slow)>>,
    /// Every secret's value, to take out of whatever the board sends (decision D86), current
    /// with the store (decision D87).
    scrubber: Arc<nypi_vault::secrets::LiveScrubber>,
}

/// The last 24 hours of audit records, kept current by following the log.
struct Day {
    records: VecDeque<Value>,
    keys: HashSet<String>,
    tail: Option<AuditTail>,
    /// The model cost of the seven days before, and when it was counted.
    week_cost: f64,
    week_at: Option<Instant>,
}

fn key(record: &Value) -> String {
    format!("{}:{}", record["session"].as_str().unwrap_or_default(), record["seq"])
}

impl Day {
    fn new() -> Self {
        Day { records: VecDeque::new(), keys: HashSet::new(), tail: None, week_cost: 0.0, week_at: None }
    }

    fn refresh(&mut self, dir: &std::path::Path) {
        let fresh = match self.tail.as_mut() {
            Some(tail) => tail.poll(),
            None => {
                // Marked before reading, so a record written meanwhile is read twice, not missed.
                let mark = AuditTail::mark(dir);
                let records = audit_log::load(dir, 2);
                self.tail = Some(match mark.and_then(|(file, offset)| AuditTail::at(dir, &file, offset)) {
                    Some(tail) => tail,
                    None => AuditTail::from_now(dir),
                });
                records
            }
        };
        for record in fresh {
            if self.keys.insert(key(&record)) {
                self.records.push_back(record);
            }
        }
        let oldest = (chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
        while self.records.front().is_some_and(|r| r["ts"].as_str().is_none_or(|ts| ts < oldest.as_str())) {
            if let Some(record) = self.records.pop_front() {
                self.keys.remove(&key(&record));
            }
        }
        if self.week_at.is_none_or(|at| at.elapsed() > Duration::from_secs(3600)) {
            let now = chrono::Utc::now();
            let start = (now - chrono::Duration::days(8)).to_rfc3339();
            let end = (now - chrono::Duration::days(1)).to_rfc3339();
            let week = audit_log::load(dir, 9);
            self.week_cost =
                records::cost(records::since(&week, &start).filter(|r| r["ts"].as_str() < Some(end.as_str())));
            self.week_at = Some(Instant::now());
        }
    }
}

impl Board {
    fn new(paths: Paths, token: String) -> Arc<Self> {
        let acknowledged = std::fs::read_to_string(acknowledged_file(&paths))
            .ok()
            .and_then(|text| serde_json::from_str::<VecDeque<String>>(&text).ok())
            .unwrap_or_default();
        let scrubber = Arc::new(nypi_vault::secrets::LiveScrubber::new(&paths.home));
        Arc::new(Board {
            paths,
            token,
            csrf: uuid::Uuid::new_v4().simple().to_string(),
            hub: Hub::default(),
            day: Mutex::new(Day::new()),
            acknowledged: Mutex::new(acknowledged),
            now: watch::Sender::new(Arc::new(Now::default())),
            slow: Mutex::new(None),
            scrubber,
        })
    }

    /// `text` with every secret of the store taken out, as the store reads now.
    fn scrub(&self, text: &str) -> String {
        self.scrubber.scrub(text)
    }

    /// The secrets as the store reads now, for work done off the board's thread.
    fn secrets(&self) -> Arc<nypi_vault::secrets::SecretScrubber> {
        self.scrubber.current()
    }

    /// The home screen as it is now.
    fn compute_now(&self) -> Now {
        let tasks = Ledger::new(self.paths.ledger()).load();
        let mut day = self.day.lock().unwrap();
        day.refresh(&self.paths.audit());
        let records: Vec<&Value> = day.records.iter().collect();
        let acknowledged: HashSet<String> = self.acknowledged.lock().unwrap().iter().cloned().collect();
        let mut processes = self.hub.processes();
        processes.sort_by(|a, b| b.run_started.is_some().cmp(&a.run_started.is_some()).then(a.id.cmp(&b.id)));
        let schedules = ScheduleStore::new(self.paths.schedules()).active();
        let now = chrono::Utc::now();
        let settings = nypi_core::config::Settings::load(&self.paths, &self.paths.home).unwrap_or_default();
        let graph = records::graph(&tasks, now, ledger::process_alive, |task| self.hub.running(task));
        Now {
            inbox: {
                let mut inbox =
                    records::inbox(&self.hub.approvals(), &tasks, &records, &acknowledged, ledger::process_alive);
                // Join requests first: someone is waiting at a screen for them (decision D93).
                let joins = nypi_hub::pairing::pending(&self.paths).into_iter().map(|request| {
                    nypi_board_proto::InboxItem::Join {
                        asker: serde_json::to_value(request.kind)
                            .ok()
                            .and_then(|kind| kind.as_str().map(str::to_string))
                            .unwrap_or_default(),
                        id: request.id,
                        name: request.name,
                        system: request.system,
                        from: request.from,
                        code: request.code,
                        expires: request.expires,
                    }
                });
                joins.chain(inbox.drain(..)).collect()
            },
            processes,
            today: records::today(&records, day.week_cost),
            upcoming: records::upcoming(&schedules, now, 3),
            // The whole hub at a look (decision D80): the tree of tasks, the devices, and
            // what last changed in the vault, read from where the hub keeps them.
            campaigns: planes::campaigns(&self.paths, &graph, &tasks),
            planes: {
                let mut planes = planes::planes(&self.paths, &settings, now);
                planes.chat.live = self.hub.plane("chat");
                planes.principal.live = self.hub.plane("principal");
                planes
            },
            graph,
            devices: records::devices(
                &nypi_hub::nodes::read_register(&self.paths),
                &nypi_hub::pairing::Peers::open(&self.paths).list(),
                nypi_hub::daemon::alive(&self.paths, now),
            ),
            vault: records::vault_activity(&self.paths.vault(), 6),
        }
    }

    fn acknowledge(&self, key: String) {
        let mut acknowledged = self.acknowledged.lock().unwrap();
        if !acknowledged.contains(&key) {
            acknowledged.push_back(key);
        }
        while acknowledged.len() > ACKNOWLEDGED_KEPT {
            acknowledged.pop_front();
        }
        if let Ok(text) = serde_json::to_string(&*acknowledged) {
            let _ = std::fs::write(acknowledged_file(&self.paths), text);
        }
        self.hub.nudge();
    }
}

fn acknowledged_file(paths: &Paths) -> std::path::PathBuf {
    paths.home.join("run").join("board-acknowledged.json")
}

/// Serve until interrupted. `local` keeps the page on this machine.
pub async fn run(paths: &Paths, local: bool, port: u16, new_token: bool) -> anyhow::Result<i32> {
    let host: IpAddr = if local { [127, 0, 0, 1].into() } else { [0, 0, 0, 0].into() };
    // The port first: a board that cannot serve must not take the socket runtimes link to.
    let listener = TcpListener::bind(SocketAddr::new(host, port)).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            anyhow::anyhow!("port {port} is in use (another board, or an old `nypi observe`?); stop it or pass --port")
        } else {
            error.into()
        }
    })?;
    let links = crate::bridge::bind_owner_socket(&paths.board_socket(), "board")?;
    // Browsers that came through the entry, handed over by the hub (decision D94).
    let public = crate::bridge::bind_owner_socket(&paths.board_public_socket(), "board for browsers")?;
    // Only a board that will run may replace the token.
    let token = web::token(paths, new_token)?;
    let port = listener.local_addr()?.port();
    println!("NyPi board for {}. Open:", paths.home.display());
    println!("  http://127.0.0.1:{port}/?token={token}");
    if !local {
        for (address, what) in web::reachable_addresses() {
            println!("  http://{address}:{port}/?token={token}   ({what})");
        }
        println!("The board can approve and stop work. Plain HTTP: use it on networks you trust.");
    }
    if !web::ui_built() {
        println!("The page is not built yet: run scripts/build-board.sh, then rebuild nypi.");
    }
    let board = Board::new(paths.clone(), token);
    // Where this board serves, for `nypi board --link` and the supervisor's status.
    let serving = Serving { pid: std::process::id(), port, local };
    let _ = std::fs::write(serving_file(paths), serde_json::to_vec(&serving).unwrap_or_default());
    tokio::spawn(web::serve_public(public, board.clone()));
    let served = start(board.clone(), links, listener);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = served => {}
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    let _ = std::fs::remove_file(paths.board_socket());
    let _ = std::fs::remove_file(paths.board_public_socket());
    let _ = std::fs::remove_file(serving_file(paths));
    Ok(0)
}

/// Where a running board serves.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Serving {
    pub pid: u32,
    pub port: u16,
    pub local: bool,
}

fn serving_file(paths: &Paths) -> std::path::PathBuf {
    paths.home.join("run").join("board.json")
}

/// The board that is running now, if one is.
pub fn running(paths: &Paths) -> Option<Serving> {
    let serving: Serving = serde_json::from_str(&std::fs::read_to_string(serving_file(paths)).ok()?).ok()?;
    ledger::process_alive(serving.pid).then_some(serving)
}

/// `nypi board --link`: the addresses of the board that is running, with its token.
pub fn print_link(paths: &Paths) -> anyhow::Result<i32> {
    let Some(serving) = running(paths) else {
        println!("No board is running. The supervisor starts one on the hub (nypi hub up), or run: nypi board");
        return Ok(1);
    };
    let token = web::token(paths, false)?;
    println!("NyPi board (pid {}). Open:", serving.pid);
    println!("  http://127.0.0.1:{}/?token={token}", serving.port);
    if serving.local {
        println!(
            "It serves this machine only. For your phone, set \"board\": {{ \"local\": false }} in settings.json."
        );
    } else {
        for (address, what) in web::reachable_addresses() {
            println!("  http://{address}:{}/?token={token}   ({what})", serving.port);
        }
        println!("The board can approve and stop work. Plain HTTP: use it on networks you trust.");
    }
    Ok(0)
}

/// Take links, keep the home screen current, and serve the page.
async fn start(board: Arc<Board>, links: UnixListener, listener: TcpListener) {
    let first = board.clone();
    if let Ok(now) = tokio::task::spawn_blocking(move || first.compute_now()).await {
        board.now.send_replace(Arc::new(now));
    }
    tokio::spawn(accept_links(board.clone(), links));
    tokio::spawn(publish_now(board.clone()));
    web::serve(listener, board).await;
}

async fn accept_links(board: Arc<Board>, listener: UnixListener) {
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(link(board.clone(), stream));
    }
}

/// One process's link, from its first hello until it goes away. Every runtime of the process
/// reports on a channel of its own (decision D80), and each channel is a card on the board.
async fn link(board: Arc<Board>, stream: UnixStream) {
    use nypi_core::board_link::Framed;
    let (read, mut write) = stream.into_split();
    let (down, mut commands) = mpsc::unbounded_channel::<Framed<LinkDown>>();
    // Channel to the board's name for the runtime on it.
    let mut channels: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut lines = BufReader::new(read).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let Ok(framed) = serde_json::from_str::<Framed<LinkUp>>(&line) else { continue };
                if matches!(framed.message, LinkUp::Bye) {
                    if let Some(id) = channels.remove(&framed.ch) {
                        board.hub.disconnect(&id);
                    }
                    continue;
                }
                let id = channels.entry(framed.ch.clone()).or_insert_with(|| {
                    // What the owner does to this card goes back on its channel.
                    let (to_channel, mut from_board) = mpsc::unbounded_channel::<LinkDown>();
                    let (down, ch) = (down.clone(), framed.ch.clone());
                    tokio::spawn(async move {
                        while let Some(message) = from_board.recv().await {
                            if down.send(Framed { ch: ch.clone(), message }).is_err() {
                                break;
                            }
                        }
                    });
                    board.hub.connect(to_channel)
                });
                board.hub.apply(id, framed.message);
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                let Ok(mut line) = serde_json::to_vec(&command) else { continue };
                line.push(b'\n');
                if write.write_all(&line).await.is_err() {
                    break;
                }
            }
        }
    }
    for id in channels.values() {
        board.hub.disconnect(id);
    }
}

/// Send the home screen again when something on it changed, a moment after the change so a
/// burst of events costs one update.
async fn publish_now(board: Arc<Board>) {
    let mut changes = board.hub.subscribe();
    loop {
        let wait = tokio::time::sleep(LOOK);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                change = changes.recv() => match change {
                    Ok(Change::Now | Change::Stored) | Err(broadcast::error::RecvError::Lagged(_)) => break,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = &mut wait => break,
            }
        }
        tokio::time::sleep(SETTLE).await;
        while changes.try_recv().is_ok() {}
        let worker = board.clone();
        let Ok(now) = tokio::task::spawn_blocking(move || worker.compute_now()).await else { continue };
        board.now.send_if_modified(|current| {
            if **current == now {
                return false;
            }
            *current = Arc::new(now);
            true
        });
    }
}

#[cfg(test)]
mod tests;
