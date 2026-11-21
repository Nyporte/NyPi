//! Reading the audit log without touching it: whole days of records, and the records appended
//! since the last look (the board, decision D56). Only the Inspector writes the log.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Audit files of the last `days` days, newest first.
pub fn audit_files(audit_dir: &Path, days: usize) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(audit_dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
                .collect()
        })
        .unwrap_or_default();
    // Names are dates (YYYY-MM-DD), so name order is date order.
    files.sort();
    files.reverse();
    files.truncate(days);
    files
}

/// Records in file order, oldest file first. Unparsable lines are skipped: the log is append-only
/// and a crash can leave a partial last line.
pub fn load(audit_dir: &Path, days: usize) -> Vec<Value> {
    let mut files = audit_files(audit_dir, days);
    files.reverse();
    files
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .flat_map(|text| text.lines().filter_map(|line| serde_json::from_str::<Value>(line).ok()).collect::<Vec<_>>())
        .collect()
}

/// Follows the newest audit file from where it was last read. Opens the file read-only for
/// each look and keeps nothing open, so the writer is never held up.
pub struct AuditTail {
    dir: PathBuf,
    file: Option<PathBuf>,
    offset: u64,
    /// The start of a line whose end has not been written yet.
    partial: Vec<u8>,
}

impl AuditTail {
    /// Start at the end of the newest file: only records written from now on.
    pub fn from_now(dir: &Path) -> Self {
        let file = audit_files(dir, 1).into_iter().next();
        let offset = file.as_ref().and_then(|f| std::fs::metadata(f).ok()).map_or(0, |m| m.len());
        AuditTail { dir: dir.to_path_buf(), file, offset, partial: Vec::new() }
    }

    /// Start in the audit file named `name` at `offset`: where an earlier read stopped. Only a
    /// plain `.jsonl` file name inside the audit folder is accepted.
    pub fn at(dir: &Path, name: &str, offset: u64) -> Option<Self> {
        let plain =
            !name.is_empty() && name.ends_with(".jsonl") && !name.contains(['/', '\\']) && !name.starts_with('.');
        let file = dir.join(name);
        (plain && file.is_file()).then(|| AuditTail {
            dir: dir.to_path_buf(),
            file: Some(file),
            offset,
            partial: Vec::new(),
        })
    }

    /// The newest file's name and length now: where a reader that loads everything up to this
    /// moment can hand over to a tail.
    pub fn mark(dir: &Path) -> Option<(String, u64)> {
        let file = audit_files(dir, 1).into_iter().next()?;
        let len = std::fs::metadata(&file).ok()?.len();
        Some((file.file_name()?.to_string_lossy().into_owned(), len))
    }

    /// The records appended since the last call, in order. A new day's file is picked up once
    /// the previous one is read to its end.
    pub fn poll(&mut self) -> Vec<Value> {
        let mut records = Vec::new();
        if let Some(file) = self.file.clone() {
            self.read_from(&file, &mut records);
        }
        let newest = audit_files(&self.dir, 1).into_iter().next();
        if newest.is_some() && newest != self.file {
            self.file = newest.clone();
            self.offset = 0;
            self.partial.clear();
            if let Some(file) = newest {
                self.read_from(&file, &mut records);
            }
        }
        records
    }

    fn read_from(&mut self, file: &Path, records: &mut Vec<Value>) {
        let Ok(mut handle) = std::fs::File::open(file) else { return };
        let len = handle.metadata().map_or(0, |m| m.len());
        if len < self.offset {
            // Not an append-only file any more (replaced by hand): start over.
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset || handle.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut bytes = Vec::new();
        if handle.take(len - self.offset).read_to_end(&mut bytes).is_err() {
            return;
        }
        self.offset += bytes.len() as u64;
        self.partial.extend_from_slice(&bytes);
        let Some(end) = self.partial.iter().rposition(|b| *b == b'\n') else { return };
        let complete: Vec<u8> = self.partial.drain(..=end).collect();
        records.extend(
            complete
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .filter_map(|line| serde_json::from_slice::<Value>(line).ok()),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn the_tail_reads_whole_new_lines_and_follows_the_next_day() {
        let dir = tempfile::tempdir().unwrap();
        let day1 = dir.path().join("2026-11-04.jsonl");
        std::fs::write(&day1, "{\"seq\":0}\n").unwrap();
        let mut tail = AuditTail::from_now(dir.path());
        assert!(tail.poll().is_empty(), "what was there before is not new");

        let mut file = std::fs::OpenOptions::new().append(true).open(&day1).unwrap();
        file.write_all(b"{\"seq\":1}\n{\"seq\":").unwrap();
        let seqs = |records: Vec<Value>| records.iter().map(|r| r["seq"].as_i64().unwrap()).collect::<Vec<_>>();
        assert_eq!(seqs(tail.poll()), [1], "a half-written line waits");
        file.write_all(b"2}\n").unwrap();
        assert_eq!(seqs(tail.poll()), [2]);

        std::fs::write(dir.path().join("2026-11-05.jsonl"), "{\"seq\":3}\n").unwrap();
        file.write_all(b"{\"seq\":4}\n").unwrap();
        assert_eq!(seqs(tail.poll()), [4, 3], "the old day is finished first");
        assert!(tail.poll().is_empty());

        assert!(AuditTail::at(dir.path(), "../secrets.jsonl", 0).is_none(), "only files in the folder");
        assert!(AuditTail::at(dir.path(), "2026-11-05.jsonl", 0).is_some());
    }
}
