//! Incremental control-journal reader for the Diagnostics live tail (§48).
//! The journal is append-only JSONL written by THIS or ANOTHER process
//! (the CLI) — the Diagnostics page proving a CLI write shows up ≤2 s while
//! the app runs is an explicit HIL item. Reads are offset-incremental:
//!
//! - only bytes past the last offset are read;
//! - a trailing unterminated line (writer mid-append) is carried over and
//!   retried next round — torn lines are never parsed, never lost;
//! - a complete line that fails JSON parse is counted as skipped (the show
//!   must go on; the writer's flush+sync discipline makes this near-
//!   impossible outside actual corruption);
//! - file shrink (rotation/truncation) resets the reader to the start.

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use crate::control::journal::JournalEntry;

pub struct JournalTail {
    path: PathBuf,
    offset: u64,
    /// Unterminated trailing bytes carried over from the last poll.
    partial: Vec<u8>,
    /// Total complete-but-unparseable lines seen (Diagnostics shows this).
    skipped: u64,
}

impl JournalTail {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
            skipped: 0,
        }
    }

    pub fn default_journal() -> Self {
        Self::new(crate::control::journal::ControlJournal::default_path())
    }

    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Read all complete entries appended since the last poll. Missing file
    /// = no entries (the journal appears on the first write).
    pub fn poll(&mut self) -> Vec<JournalEntry> {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Rotated/truncated: start over.
            self.offset = 0;
            self.partial.clear();
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = std::mem::take(&mut self.partial);
        let start_len = buf.len();
        if file.read_to_end(&mut buf).is_err() {
            self.partial = buf;
            return Vec::new();
        }
        let consumed = buf.len() - start_len;
        self.offset += consumed as u64;

        let mut entries = Vec::new();
        let mut line_start = 0;
        for i in 0..buf.len() {
            if buf[i] != b'\n' {
                continue;
            }
            let line = &buf[line_start..i];
            line_start = i + 1;
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            match serde_json::from_slice::<JournalEntry>(line) {
                Ok(e) => entries.push(e),
                Err(_) => self.skipped += 1,
            }
        }
        // Keep the unterminated tail for the next round.
        self.partial = buf[line_start..].to_vec();
        entries
    }
}

impl Default for JournalTail {
    fn default() -> Self {
        Self::default_journal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::journal::{ControlJournal, JournalOrigin};
    use phelper_domain::command::{
        ControlCommand, ControlOutcome, ControlReceipt, ControlStatus, Verification,
    };
    use phelper_domain::policy::ThermalMode;
    use std::time::Duration;

    fn outcome(n: u64) -> ControlOutcome {
        ControlOutcome {
            receipt: ControlReceipt(n),
            command: ControlCommand::SetThermalMode(ThermalMode::Balanced),
            status: ControlStatus::Applied {
                verification: Verification::TrustedNoReadback,
            },
            steps: Vec::new(),
            duration: Duration::from_millis(1),
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("phelper-jtail-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("journal.jsonl")
    }

    #[test]
    fn missing_file_yields_nothing() {
        let mut t = JournalTail::new(tmp("missing"));
        assert!(t.poll().is_empty());
    }

    #[test]
    fn reads_entries_written_by_real_journal() {
        let path = tmp("real");
        let mut j = ControlJournal::open(&path, "8BAB", "F.30").unwrap();
        j.append(&j.new_entry(JournalOrigin::User, outcome(1)))
            .unwrap();
        let mut t = JournalTail::new(path.clone());
        let got = t.poll();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].board_id, "8BAB");
        assert_eq!(got[0].origin, JournalOrigin::User);
        // Second poll: nothing new.
        assert!(t.poll().is_empty());
        // Append more (cross-process simulation: a second writer handle).
        j.append(&j.new_entry(JournalOrigin::Shutdown, outcome(2)))
            .unwrap();
        j.append(&j.new_entry(JournalOrigin::Safety, outcome(3)))
            .unwrap();
        let got = t.poll();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].origin, JournalOrigin::Shutdown);
        assert_eq!(got[1].origin, JournalOrigin::Safety);
    }

    #[test]
    fn torn_trailing_line_is_retried_not_lost() {
        let path = tmp("torn");
        let j = ControlJournal::open(&path, "8BAB", "F.30").unwrap();
        let line = serde_json::to_string(&j.new_entry(JournalOrigin::User, outcome(9))).unwrap();
        drop(j);
        // Write half a line, poll, then the rest + newline.
        let cut = line.len() / 2;
        std::fs::write(&path, &line[..cut]).unwrap();
        let mut t = JournalTail::new(path.clone());
        assert!(t.poll().is_empty(), "torn line must not parse");
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(line[cut..].as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
        let got = t.poll();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].outcome.receipt.0, 9);
        assert_eq!(t.skipped(), 0, "torn-then-completed is not corruption");
    }

    #[test]
    fn broken_complete_line_is_skipped_not_fatal() {
        let path = tmp("broken");
        let j = ControlJournal::open(&path, "8BAB", "F.30").unwrap();
        let good = serde_json::to_string(&j.new_entry(JournalOrigin::User, outcome(1))).unwrap();
        drop(j);
        let mut content = String::new();
        content.push_str(&good);
        content.push('\n');
        content.push_str("{this is not json at all\n");
        content.push_str(&good);
        content.push('\n');
        std::fs::write(&path, content).unwrap();
        let mut t = JournalTail::new(path);
        let got = t.poll();
        assert_eq!(got.len(), 2, "good lines on both sides survive");
        assert_eq!(t.skipped(), 1);
    }

    #[test]
    fn truncated_file_resets_offset() {
        let path = tmp("truncate");
        let j = ControlJournal::open(&path, "8BAB", "F.30").unwrap();
        let good = serde_json::to_string(&j.new_entry(JournalOrigin::User, outcome(1))).unwrap();
        drop(j);
        std::fs::write(&path, format!("{good}\n{good}\n")).unwrap();
        let mut t = JournalTail::new(path.clone());
        assert_eq!(t.poll().len(), 2);
        // File rewritten shorter (rotation): next poll re-reads from start.
        std::fs::write(&path, format!("{good}\n")).unwrap();
        let got = t.poll();
        assert_eq!(got.len(), 1);
    }
}
