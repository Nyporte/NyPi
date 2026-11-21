//! Append-only JSONL logs (ledger, schedules, memory, portrait, outbox): one event per line,
//! synced before returning, and a fresh line when a crash left the last one partial, so one torn
//! write never swallows the next event.
//!
//! A log's meaning is the fold of its events, and the hub asks for that many times a minute
//! (decision D67, part 6). `Folded` keeps the fold in memory and, on each read, takes in only
//! what was appended to the file since the last one. It follows the file, not this process's
//! own writes, so it stays right when another process appends too (a terminal with a session of
//! its own, `nypi rate`), and it starts over when the file is not the one it was reading.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub fn append(path: &Path, event: &impl serde::Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(event).map_err(std::io::Error::other)?;
    line.push(b'\n');
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = OpenOptions::new().create(true).read(true).append(true).open(path)?;
    let length = file.metadata()?.len();
    if length > 0 {
        let mut last = [0u8; 1];
        file.seek(SeekFrom::Start(length - 1))?;
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            line.insert(0, b'\n');
        }
    }
    file.write_all(&line)?;
    file.sync_data()
}

/// The whole log as text, or None when it does not exist. A write torn inside a multibyte
/// character (a crash, a full disk) must cost only its own line, never the log: invalid bytes
/// are replaced, and the reader skips the line they spoil.
pub fn read(path: &Path) -> Option<String> {
    std::fs::read(path).ok().map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// How much of the end of what was read is remembered, to tell an append from another file.
const FINGERPRINT: usize = 64;

/// A log folded into a state `S`, kept in step with its file.
struct Fold<S> {
    state: S,
    /// Bytes of the file taken in: always the end of a complete line.
    offset: u64,
    /// Lines taken in, the order an event has in the log.
    lines: u64,
    inode: u64,
    /// The last bytes taken in.
    tail: Vec<u8>,
}

impl<S: Default> Fold<S> {
    fn new() -> Self {
        Fold { state: S::default(), offset: 0, lines: 0, inode: 0, tail: Vec::new() }
    }

    /// Whether `file` still begins with what was taken in: the same file, at least as long, and
    /// ending, where the reading stopped, in the same bytes.
    fn continues(&self, file: &mut std::fs::File, inode: u64, length: u64) -> bool {
        if self.offset == 0 {
            return true;
        }
        if inode != self.inode || length < self.offset {
            return false;
        }
        let mut seen = vec![0u8; self.tail.len()];
        let from = self.offset - self.tail.len() as u64;
        file.seek(SeekFrom::Start(from)).and_then(|_| file.read_exact(&mut seen)).is_ok() && seen == self.tail
    }

    fn refresh(&mut self, path: &Path, apply: impl Fn(&mut S, &str, u64)) {
        let Ok(mut file) = std::fs::File::open(path) else {
            // No log (yet, or any more): nothing happened.
            *self = Fold::new();
            return;
        };
        let Ok(meta) = file.metadata() else { return };
        let (inode, length) = (meta.ino(), meta.len());
        if !self.continues(&mut file, inode, length) {
            *self = Fold::new();
        }
        self.inode = inode;
        if length == self.offset {
            return;
        }
        let mut added = Vec::new();
        if file.seek(SeekFrom::Start(self.offset)).and_then(|_| file.read_to_end(&mut added)).is_err() {
            return;
        }
        // A line still being written, or torn by a crash, waits: the writer after a crash starts
        // a fresh line, and then the torn one is a line that does not parse.
        let Some(end) = added.iter().rposition(|byte| *byte == b'\n') else { return };
        let complete = &added[..=end];
        for line in String::from_utf8_lossy(complete).lines() {
            apply(&mut self.state, line, self.lines);
            self.lines += 1;
        }
        self.offset += complete.len() as u64;
        self.tail.extend_from_slice(complete);
        let keep = self.tail.len().saturating_sub(FINGERPRINT);
        self.tail.drain(..keep);
    }
}

/// Every fold of this process, by the state it folds to and the file it follows.
type Folds = HashMap<(TypeId, PathBuf), Arc<dyn Any + Send + Sync>>;

/// A handle on the fold of one log. Every handle of the same state type on the same path in this
/// process shares one fold, so a store made anew at each call site still reads only what is new.
pub struct Folded<S> {
    path: PathBuf,
    fold: Arc<Mutex<Fold<S>>>,
}

impl<S: Default + Send + 'static> Folded<S> {
    pub fn shared(path: &Path) -> Self {
        static FOLDS: OnceLock<Mutex<Folds>> = OnceLock::new();
        let mut folds = FOLDS.get_or_init(Default::default).lock().unwrap();
        let entry = folds
            .entry((TypeId::of::<S>(), path.to_path_buf()))
            .or_insert_with(|| Arc::new(Mutex::new(Fold::<S>::new())) as Arc<dyn Any + Send + Sync>);
        let fold = entry.clone().downcast::<Mutex<Fold<S>>>().expect("one state type per key");
        Folded { path: path.to_path_buf(), fold }
    }

    /// Bring the state up to date with the file, then read it. `apply` takes in one line that
    /// was appended, with its place in the log; `read` must not come back to this store.
    pub fn read<R>(&self, apply: impl Fn(&mut S, &str, u64), read: impl FnOnce(&S) -> R) -> R {
        let mut fold = self.fold.lock().unwrap();
        fold.refresh(&self.path, apply);
        read(&fold.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines of a log, with the place each had.
    fn lines(folded: &Folded<Vec<(u64, String)>>) -> Vec<(u64, String)> {
        folded.read(|state, line, order| state.push((order, line.to_string())), |state| state.clone())
    }

    fn write(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new().create(true).append(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    #[test]
    fn a_fold_takes_in_only_what_was_appended_and_starts_over_for_another_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let folded = Folded::<Vec<(u64, String)>>::shared(&path);
        assert!(lines(&folded).is_empty(), "no file is an empty log");

        write(&path, b"one\ntwo\n");
        assert_eq!(lines(&folded), [(0, "one".to_string()), (1, "two".to_string())]);
        // Another handle on the same path is the same fold: nothing is taken in twice.
        let again = Folded::<Vec<(u64, String)>>::shared(&path);
        assert_eq!(lines(&again).len(), 2);

        // A line that is still being written waits for its end.
        write(&path, b"thr");
        assert_eq!(lines(&folded).len(), 2);
        write(&path, b"ee\nfour\n");
        let all = lines(&folded);
        assert_eq!(all.len(), 4);
        assert_eq!(all[2], (2, "three".to_string()));

        // The file was replaced by another (a restored copy, even a longer one): start over.
        std::fs::write(&path, "uno\ndos\ntres\ncuatro\ncinco\n").unwrap();
        let replaced = lines(&folded);
        assert_eq!(replaced.len(), 5);
        assert_eq!(replaced[0], (0, "uno".to_string()));
        // Gone altogether: an empty log again.
        std::fs::remove_file(&path).unwrap();
        assert!(lines(&folded).is_empty());
    }
}
