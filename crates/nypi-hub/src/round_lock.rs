//! The lock a round of the scheduler or the watchdog takes, so that two processes (the daemon,
//! and `nypi schedule tick` or `nypi watchdog` run by cron) never do the same round at once
//! (decision D83). One that finds it taken skips its round.
//!
//! It is a POSIX record lock (`fcntl`), not `flock`. An `flock` lock belongs to the open file,
//! and a child that another thread forks while a round holds the lock gets a copy of that file
//! until it execs: the lock then outlives the round, and the next round finds it taken although
//! nobody is doing one. A record lock belongs to the process and is never inherited by a child,
//! so it is released exactly when the round lets go. Because a record lock does not keep two
//! rounds of the same process apart, the process also notes which locks it holds.

use std::collections::HashSet;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The round locks this process holds.
fn held() -> &'static Mutex<HashSet<PathBuf>> {
    static HELD: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    HELD.get_or_init(Mutex::default)
}

/// A round's hold on its lock file, until dropped.
pub struct RoundLock {
    path: PathBuf,
    /// Closing it releases the record lock.
    file: Option<File>,
}

impl RoundLock {
    /// Take the lock at `path`, made if it is not there. None when a round holds it, in this
    /// process or another, or when it cannot be opened.
    pub fn try_take(path: &Path) -> Option<RoundLock> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut ours = held().lock().unwrap();
        // Before the file is opened: closing a second handle on it here would release the lock.
        if ours.contains(path) {
            return None;
        }
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok()?;
        // SAFETY: an all-zero `flock` is a valid value; the fields that matter are set below.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as _;
        lock.l_whence = libc::SEEK_SET as _;
        // SAFETY: `file` is open for the call, and `lock` is a valid `flock` that outlives it.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) } != 0 {
            return None;
        }
        ours.insert(path.to_path_buf());
        Some(RoundLock { path: path.to_path_buf(), file: Some(file) })
    }
}

impl Drop for RoundLock {
    fn drop(&mut self) {
        // Noted as free only once the file is closed, so no round of ours opens it meanwhile.
        let mut ours = held().lock().unwrap();
        drop(self.file.take());
        ours.remove(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_round_at_a_time_and_a_child_never_keeps_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run/scheduler.lock");
        let first = RoundLock::try_take(&path).expect("free");
        assert!(RoundLock::try_take(&path).is_none(), "a round of this process holds it");
        drop(first);
        let again = RoundLock::try_take(&path).expect("free once the round let go");

        // A child that has a copy of the lock's file (as one forked meanwhile by another thread
        // has, until it execs) does not keep the lock once the round lets go.
        // SAFETY: plain syscall on an open descriptor; it only clears close-on-exec.
        unsafe { libc::fcntl(again.file.as_ref().unwrap().as_raw_fd(), libc::F_SETFD, 0) };
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        drop(again);
        let taken = RoundLock::try_take(&path);
        let _ = child.kill();
        let _ = child.wait();
        assert!(taken.is_some(), "the child's copy of the file kept the lock");
    }
}
