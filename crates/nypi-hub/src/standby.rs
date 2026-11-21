//! Standby copies and moves over a machine's link (decision D95). After the welcome on a
//! `standby` or `move` link, the node says what its copy has (`have`); the hub sends batches,
//! each a header line, then one line and its bytes per file, and the node answers `applied` with
//! its manifest, or `rejected`, after which the hub sends what differs from an empty copy. A
//! move ends with the hub's `released` (this machine is the hub now) or `abandoned`.
//!
//! The hub sends a batch every `hub.standbyMinutes`, or a minute after a decision of the
//! Principal when one came sooner. Over a relayed link a batch goes at most every 30 minutes, at
//! no more than half the relay's per-link rate, so standby copies never crowd the relay.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

use crate::state::{Entry, Manifest, Piece};

/// The longest message line: a manifest of many files.
const MAX_LINE: usize = 32 << 20;
/// The largest single file a batch carries.
const MAX_FILE: u64 = 1 << 30;
/// Between two batches over a relayed link, at least.
pub const RELAYED_EVERY: Duration = Duration::from_secs(30 * 60);
/// A decision of the Principal brings a batch forward, but not sooner than this after the last.
pub const AFTER_A_DECISION: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum Said {
    /// Node to hub: what its copy has; in a move, also this machine's own direct addresses.
    Have {
        manifest: Manifest,
        #[serde(default)]
        direct: Vec<String>,
    },
    /// Hub to node: `count` files follow.
    Batch {
        epoch: u64,
        count: usize,
        last: bool,
    },
    Applied {
        manifest: Manifest,
    },
    Rejected {
        reason: String,
    },
    /// Hub to node, in a move: the old hub stepped down; this machine is the hub at `epoch`.
    Released {
        epoch: u64,
    },
    /// Hub to node, in a move: the move did not happen.
    Abandoned {
        reason: String,
    },
}

pub async fn say<W: AsyncWrite + Unpin>(write: &mut W, said: &Said) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(said).map_err(std::io::Error::other)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await
}

pub async fn hear<R: AsyncBufRead + Unpin>(read: &mut R) -> std::io::Result<Said> {
    let line = nypi_node::link::read_line(read, MAX_LINE)
        .await?
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the link closed"))?;
    serde_json::from_str(&line).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Send `pieces` as one batch at `epoch`, at no more than `rate` bytes a second (0: no limit).
pub async fn send_batch<W: AsyncWrite + Unpin>(
    write: &mut W,
    epoch: u64,
    last: bool,
    pieces: Vec<Piece>,
    rate: u64,
) -> std::io::Result<()> {
    say(write, &Said::Batch { epoch, count: pieces.len(), last }).await?;
    let started = Instant::now();
    let mut sent = 0u64;
    for piece in pieces {
        let mut header = serde_json::to_vec(&piece.entry).map_err(std::io::Error::other)?;
        header.push(b'\n');
        write.write_all(&header).await?;
        for chunk in piece.bytes.chunks(64 * 1024) {
            write.write_all(chunk).await?;
            sent += chunk.len() as u64;
            if rate > 0 {
                let due = started + Duration::from_secs_f64(sent as f64 / rate as f64);
                tokio::time::sleep_until(due).await;
            }
        }
    }
    write.flush().await
}

/// Read the `count` files of a batch whose header was heard.
pub async fn read_batch<R: AsyncBufRead + Unpin>(read: &mut R, count: usize) -> std::io::Result<Vec<Piece>> {
    let mut pieces = Vec::with_capacity(count);
    for _ in 0..count {
        let line = nypi_node::link::read_line(read, 64 * 1024)
            .await?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the batch ended early"))?;
        let entry: Entry =
            serde_json::from_str(&line).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        if entry.bytes > MAX_FILE {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "a file larger than a copy takes"));
        }
        let mut bytes = vec![0u8; entry.bytes as usize];
        read.read_exact(&mut bytes).await?;
        pieces.push(Piece { entry, bytes });
    }
    Ok(pieces)
}

/// When the hub sends a node its next batch.
#[derive(Clone, Debug)]
pub struct Schedule {
    pub every: Duration,
    pub relayed: bool,
    pub last: Option<Instant>,
}

impl Schedule {
    pub fn new(minutes: f64, relayed: bool) -> Schedule {
        let every = Duration::from_secs_f64((minutes.max(0.0) * 60.0).max(0.1));
        Schedule { every, relayed, last: None }
    }

    /// Whether a batch is due at `now`; `decided` when the Principal decided since the last one.
    pub fn due(&self, now: Instant, decided: bool) -> bool {
        let Some(last) = self.last else { return true };
        let since = now.saturating_duration_since(last);
        if self.relayed {
            return since >= RELAYED_EVERY.max(self.every);
        }
        since >= self.every || (decided && since >= AFTER_A_DECISION)
    }
}

/// The rate a batch goes at: none on a direct link, half the relay's per-link rate on a relayed
/// one (`kbps` is the relay's, as it said when the hub registered).
pub fn rate(relayed: bool, kbps: u64) -> u64 {
    if relayed && kbps > 0 { kbps * 1000 / 8 / 2 } else { 0 }
}

/// On a personal node: keep this machine's standby copy as the hub sends it, until the link
/// ends.
pub async fn keep_copy(paths: &nypi_core::config::Paths, mut linked: crate::pairing::Linked) -> std::io::Result<()> {
    let standby = crate::state::Standby::at(paths);
    say(&mut linked.write, &Said::Have { manifest: standby.manifest(), direct: Vec::new() }).await?;
    loop {
        let Said::Batch { epoch, count, .. } = hear(&mut linked.read).await? else { continue };
        let pieces = read_batch(&mut linked.read, count).await?;
        let reply = apply(paths, epoch, pieces).await;
        say(&mut linked.write, &reply).await?;
    }
}

async fn apply(paths: &nypi_core::config::Paths, epoch: u64, pieces: Vec<Piece>) -> Said {
    let paths = paths.clone();
    let applied = tokio::task::spawn_blocking(move || {
        let standby = crate::state::Standby::at(&paths);
        let applied = standby.apply(epoch, pieces);
        if applied.is_err() {
            // The hub sends what differs from an empty copy next time.
            standby.forget();
        }
        applied
    })
    .await;
    match applied {
        Ok(Ok(manifest)) => Said::Applied { manifest },
        Ok(Err(reason)) => Said::Rejected { reason },
        Err(error) => Said::Rejected { reason: error.to_string() },
    }
}

/// `nypi hub here`: this personal node takes the hub's place (decision D95). It sends what its
/// copy has and its own direct addresses, applies the hub's last difference, and, once the old
/// hub has stepped down and said `released`, makes the copy its live home at the new epoch.
/// Until then nothing here changes but the standby copy.
pub async fn take_over(
    paths: &nypi_core::config::Paths,
    settings: &nypi_core::config::Settings,
    reach: &crate::reach::Reach,
    direct: Vec<String>,
) -> anyhow::Result<crate::instance::Instance> {
    let instance = crate::instance::Instance::load(paths).filter(|instance| !instance.is_hub());
    let Some(instance) = instance else { anyhow::bail!("this machine is not a node of a hub") };
    anyhow::ensure!(
        instance.trust == crate::pairing::Trust::Personal,
        "this machine was admitted as exec-only: the hub's state never moves to it"
    );
    let mut linked = crate::reach::connect(paths, settings, crate::pairing::Purpose::Move, reach).await?;
    let standby = crate::state::Standby::at(paths);
    say(&mut linked.write, &Said::Have { manifest: standby.manifest(), direct }).await?;
    loop {
        let heard = match hear(&mut linked.read).await {
            Ok(heard) => heard,
            Err(_) => anyhow::bail!("the hub went away before the move was done; nothing changed here"),
        };
        match heard {
            Said::Batch { epoch, count, .. } => {
                let pieces = read_batch(&mut linked.read, count).await?;
                let reply = apply(paths, epoch, pieces).await;
                say(&mut linked.write, &reply).await?;
            }
            Said::Released { epoch } => {
                let paths = paths.clone();
                return tokio::task::spawn_blocking(move || crate::state::promote(&paths, epoch)).await?;
            }
            Said::Abandoned { reason } => anyhow::bail!("the move did not happen: {reason}"),
            _ => {}
        }
    }
}

/// When this machine last heard from its hub, as a node keeps it.
pub fn hub_seen(paths: &nypi_core::config::Paths) -> Option<chrono::DateTime<chrono::Utc>> {
    let text = std::fs::read_to_string(paths.home.join("run").join("hub-seen")).ok()?;
    chrono::DateTime::parse_from_rfc3339(text.trim()).ok().map(|at| at.with_timezone(&chrono::Utc))
}

pub fn saw_hub(paths: &nypi_core::config::Paths) {
    let _ = std::fs::create_dir_all(paths.home.join("run"));
    let _ = std::fs::write(paths.home.join("run").join("hub-seen"), chrono::Utc::now().to_rfc3339());
}

/// Why `nypi hub here --force` may not take over now, or None when it may: this machine is a
/// personal node with a standby copy, and its hub has not answered for `hub.forceAfterSeconds`.
pub async fn force_refused(
    paths: &nypi_core::config::Paths,
    settings: &nypi_core::config::Settings,
    reach: &crate::reach::Reach,
) -> Option<String> {
    let Some(instance) = crate::instance::Instance::load(paths).filter(|instance| !instance.is_hub()) else {
        return Some("this machine is not a node of a hub".into());
    };
    if instance.trust != crate::pairing::Trust::Personal {
        return Some("this machine was admitted as exec-only: the hub's state never moves to it".into());
    }
    if !crate::state::Standby::at(paths).exists() {
        return Some("this machine has no standby copy of the hub's state".into());
    }
    let wait = settings.hub.force_after_seconds as i64;
    if let Some(seen) = hub_seen(paths)
        && (chrono::Utc::now() - seen).num_seconds() < wait
    {
        return Some(format!("the hub answered less than {wait} seconds ago: nypi hub here moves it without --force"));
    }
    let answers = crate::reach::best_direct(paths, &instance, reach).await.is_some() || {
        match settings.entry() {
            Some(entry) => {
                let way = crate::pairing::Way::Relayed(entry);
                let probe = crate::pairing::probe(paths, &way, Some(&instance.hub_fingerprint), instance.epoch);
                matches!(tokio::time::timeout(Duration::from_secs(5), probe).await, Ok(Ok(_)))
            }
            None => false,
        }
    };
    answers.then(|| "the hub answers: nypi hub here moves it without --force".into())
}

/// `nypi hub here --force`: make the standby copy the live home at the epoch after the one this
/// machine knows. Anything the old hub did after the copy is not here.
pub fn force(paths: &nypi_core::config::Paths) -> anyhow::Result<crate::instance::Instance> {
    let known = crate::instance::Instance::load(paths).map_or(1, |instance| instance.epoch.max(1));
    crate::state::promote(paths, known + 1)
}

/// How old this machine's standby copy is, for `nypi status`.
pub fn copy_age(paths: &nypi_core::config::Paths) -> Option<chrono::Duration> {
    let at = crate::state::Standby::at(paths).manifest().at;
    chrono::DateTime::parse_from_rfc3339(&at).ok().map(|at| chrono::Utc::now() - at.with_timezone(&chrono::Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A direct link: every `standbyMinutes`, or a minute after a decision; a relayed one: every
    /// 30 minutes whatever is decided, at half the relay's rate.
    #[test]
    fn batches_keep_to_their_interval_and_rate() {
        let start = Instant::now();
        let mut direct = Schedule::new(5.0, false);
        assert!(direct.due(start, false), "the first batch at once");
        direct.last = Some(start);
        assert!(!direct.due(start + Duration::from_secs(30), true), "not within a minute of the last");
        assert!(direct.due(start + Duration::from_secs(61), true), "a decision brings it forward");
        assert!(!direct.due(start + Duration::from_secs(61), false));
        assert!(direct.due(start + Duration::from_secs(300), false));
        let mut relayed = Schedule::new(5.0, true);
        relayed.last = Some(start);
        assert!(!relayed.due(start + Duration::from_secs(301), true), "not every five minutes through the relay");
        assert!(relayed.due(start + RELAYED_EVERY, false));
        assert_eq!(rate(true, 2048), 128_000, "half of 2048 kbit/s");
        assert_eq!(rate(false, 2048), 0);
    }

    /// A batch at a limited rate takes as long as its bytes need.
    #[tokio::test]
    async fn a_batch_keeps_to_its_rate() {
        let piece = Piece {
            entry: Entry {
                path: "a".into(),
                change: crate::state::Change::Whole,
                size: 20_000,
                sha: crate::state::sha(&[0u8; 20_000]),
                bytes: 20_000,
            },
            bytes: vec![0u8; 20_000],
        };
        let (mut write, read) = tokio::io::duplex(1 << 20);
        let started = Instant::now();
        send_batch(&mut write, 1, false, vec![piece], 40_000).await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(450), "{:?}", started.elapsed());
        let mut read = tokio::io::BufReader::new(read);
        let Said::Batch { count, .. } = hear(&mut read).await.unwrap() else { panic!("no batch") };
        let pieces = read_batch(&mut read, count).await.unwrap();
        assert_eq!(pieces[0].bytes.len(), 20_000);
    }
}
