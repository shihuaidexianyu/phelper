//! Control journal (architecture.md §48): every hardware write is
//! journaled as one self-contained JSONL entry — board/BIOS context,
//! origin, full ControlOutcome with per-step before/after evidence.
//! Append-only, flush + sync per entry: a crash must never take the
//! record of the last write with it.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use phelper_domain::command::ControlOutcome;
use phelper_domain::error::EngineError;
use serde::{Deserialize, Serialize};

/// Who caused this journal entry. Steady-state keep-alive ticks are NOT
/// journaled (they would flood the log at 1/min for life); only failures,
/// drift re-assertions, and the three non-user origins below are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalOrigin {
    /// A dispatched ControlCommand from UI/CLI.
    User,
    /// KeepAliveService re-assertion after a detected clawback/drift, or a
    /// heartbeat failure record.
    Keepalive,
    /// SafetySupervisor action (thermal override, watchdog restore).
    Safety,
    /// Engine shutdown restore sequence.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub schema_version: u32,
    pub at_epoch_ms: u64,
    pub board_id: String,
    pub bios_version: String,
    pub origin: JournalOrigin,
    pub outcome: ControlOutcome,
}

pub struct ControlJournal {
    file: std::fs::File,
    path: PathBuf,
    board_id: String,
    bios_version: String,
}

/// Rotate once the journal outgrows this: the active file becomes the
/// one-generation backup `<name>.1.jsonl` and a fresh file starts. ONE
/// rotation path (fsync → rename → reopen) serves both open-time and
/// mid-session checkpoints (2026-09 review: open renamed while append
/// copy+truncated — two paths, two subtle behaviors). The rename keeps
/// the ~8 MB copy off the control thread.
#[cfg(not(test))]
const MAX_JOURNAL_BYTES: u64 = 8 * 1024 * 1024;
/// Test builds shrink the threshold so the mid-session path is reachable
/// without writing megabytes through fsync-per-entry appends.
#[cfg(test)]
const MAX_JOURNAL_BYTES: u64 = 1024;

impl ControlJournal {
    /// Default location: `<data_dir>/state/control-journal.jsonl`.
    pub fn default_path() -> PathBuf {
        crate::persistence::data_dir()
            .join("state")
            .join("control-journal.jsonl")
    }

    pub fn open(path: &Path, board_id: &str, bios_version: &str) -> Result<Self, EngineError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                EngineError::Persistence(format!("create {}: {e}", parent.display()))
            })?;
        }
        if let Ok(meta) = std::fs::metadata(path)
            && meta.len() >= MAX_JOURNAL_BYTES
        {
            // One generation of history is enough for a single-machine
            // tool. If the rotation fails the file is opened in append mode
            // anyway — keeping evidence beats keeping tidy.
            let _ = Self::rotate_to_backup(path, None);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| EngineError::Persistence(format!("open {}: {e}", path.display())))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            board_id: board_id.to_string(),
            bios_version: bios_version.to_string(),
        })
    }

    /// The single rotation primitive (see MAX_JOURNAL_BYTES). `open_file` is
    /// `Some` for mid-session checkpoints: it is flushed first, and the
    /// CALLER MUST replace its handle with the returned one — writing
    /// through the old handle after the rename would target the backup.
    fn rotate_to_backup(
        path: &Path,
        open_file: Option<&std::fs::File>,
    ) -> Result<std::fs::File, EngineError> {
        if let Some(file) = open_file {
            file.sync_all()
                .map_err(|e| EngineError::Persistence(format!("journal fsync: {e}")))?;
        }
        let backup = path.with_extension("1.jsonl");
        std::fs::rename(path, &backup).map_err(|e| {
            EngineError::Persistence(format!("journal rotate to {}: {e}", backup.display()))
        })?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| {
                EngineError::Persistence(format!("journal reopen {}: {e}", path.display()))
            })
    }

    pub fn new_entry(&self, origin: JournalOrigin, outcome: ControlOutcome) -> JournalEntry {
        JournalEntry {
            schema_version: 1,
            at_epoch_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            board_id: self.board_id.clone(),
            bios_version: self.bios_version.clone(),
            origin,
            outcome,
        }
    }

    /// Append one entry (JSONL), flush + sync. A journaling failure is
    /// reported to the caller but must not abort the control flow — the
    /// write already happened; the log is the evidence of it.
    pub fn append(&mut self, entry: &JournalEntry) -> Result<(), EngineError> {
        let line = serde_json::to_string(entry)
            .map_err(|e| EngineError::Persistence(format!("journal serialize: {e}")))?;
        if self
            .file
            .metadata()
            .is_ok_and(|m| m.len() >= MAX_JOURNAL_BYTES)
        {
            // Same rotation primitive as open-time. A checkpoint failure is
            // an error (the entry is NOT written) — never truncate without
            // the previous contents being durable in the backup.
            self.file = Self::rotate_to_backup(&self.path, Some(&self.file))?;
        }
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.write_all(b"\n"))
            .and_then(|()| self.file.flush())
            .and_then(|()| self.file.sync_data())
            .map_err(|e| EngineError::Persistence(format!("append {}: {e}", self.path.display())))
    }
}

/// Bounded evidence export. An incomplete first/last JSONL record is
/// reported as a parse error rather than represented as a completed write.
pub fn diagnostic_tail(path: &Path) -> serde_json::Value {
    use std::io::{Read, Seek, SeekFrom};
    let read = || -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let length = file.metadata()?.len();
        let start = length.saturating_sub(512 * 1024);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(512 * 1024).read_to_end(&mut bytes)?;
        if start != 0 {
            if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                bytes.drain(..=end);
            } else {
                bytes.clear();
            }
        }
        String::from_utf8(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    };
    match read() {
        Ok(text) => {
            let mut entries: Vec<serde_json::Value> = text
                .lines()
                .rev()
                .take(100)
                .map(|line| {
                    serde_json::from_str::<JournalEntry>(line)
                        .and_then(serde_json::to_value)
                        .unwrap_or_else(|e| serde_json::json!({"parse_error": e.to_string()}))
                })
                .collect();
            entries.reverse();
            serde_json::json!(entries)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!([]),
        Err(e) => serde_json::json!({"read_error": e.to_string()}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::command::ControlCommand;
    use phelper_domain::command::{ControlReceipt, ControlStatus, StepOutcome, Verification};
    use phelper_domain::policy::ThermalMode;
    use std::time::Duration;

    fn sample_outcome() -> ControlOutcome {
        ControlOutcome {
            receipt: ControlReceipt(7),
            command: ControlCommand::SetThermalMode(ThermalMode::Performance),
            status: ControlStatus::Applied {
                verification: Verification::TrustedNoReadback,
            },
            steps: vec![StepOutcome {
                step: "set_thermal_mode".into(),
                backend: "hp-wmi 0x1A".into(),
                firmware_return: Some("rc=0".into()),
                before: Some("thermal=Balanced(trusted)".into()),
                after: None,
                verification: Verification::TrustedNoReadback,
            }],
            duration: Duration::from_millis(12),
        }
    }

    #[test]
    fn jsonl_roundtrip() {
        let dir = std::env::temp_dir().join(format!("phelper-journal-test-{}", std::process::id()));
        let path = dir.join("control-journal.jsonl");
        let _ = std::fs::remove_file(&path);
        {
            let mut j = ControlJournal::open(&path, "8BAB", "F.21").unwrap();
            let e1 = j.new_entry(JournalOrigin::User, sample_outcome());
            j.append(&e1).unwrap();
            let e2 = j.new_entry(JournalOrigin::Shutdown, sample_outcome());
            j.append(&e2).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let back: JournalEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(back.schema_version, 1);
        assert_eq!(back.board_id, "8BAB");
        assert_eq!(back.origin, JournalOrigin::User);
        assert_eq!(back.outcome.receipt, ControlReceipt(7));
        assert_eq!(back.outcome.duration, Duration::from_millis(12));
        assert_eq!(
            back.outcome.steps[0].before.as_deref(),
            Some("thermal=Balanced(trusted)")
        );
        let back2: JournalEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(back2.origin, JournalOrigin::Shutdown);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn step_outcome_before_after_default_on_old_lines() {
        // Lines written before the before/after fields existed must still
        // parse (#[serde(default)]).
        let v: StepOutcome = serde_json::from_str(
            r#"{"step":"s","backend":"b","firmware_return":null,"verification":"verified"}"#,
        )
        .unwrap();
        assert_eq!(v.before, None);
        assert_eq!(v.after, None);
    }

    #[test]
    fn oversized_journal_rotates_at_open() {
        let dir =
            std::env::temp_dir().join(format!("phelper-journal-rotate-{}", std::process::id()));
        let path = dir.join("control-journal.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, vec![b'x'; (MAX_JOURNAL_BYTES + 1) as usize]).unwrap();
        {
            let mut j = ControlJournal::open(&path, "8BAB", "F.30").unwrap();
            let e = j.new_entry(JournalOrigin::User, sample_outcome());
            j.append(&e).unwrap();
        }
        // The old content moved aside intact; the new file holds exactly
        // the fresh entry.
        let rotated = path.with_extension("1.jsonl");
        assert_eq!(
            std::fs::metadata(&rotated).unwrap().len(),
            MAX_JOURNAL_BYTES + 1
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undersized_journal_is_not_rotated() {
        let dir =
            std::env::temp_dir().join(format!("phelper-journal-norotate-{}", std::process::id()));
        let path = dir.join("control-journal.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"{\"old\":true}\n").unwrap();
        {
            let _j = ControlJournal::open(&path, "8BAB", "F.30").unwrap();
        }
        assert!(!path.with_extension("1.jsonl").exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"old\":true}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mid_session_checkpoint_rotates_via_the_same_path() {
        // Test builds shrink MAX_JOURNAL_BYTES to 1 KiB, so a handful of
        // appends cross the threshold mid-session. The backup must hold
        // everything written before the rotation, and the active file
        // must start fresh — the exact contract the old copy+truncate
        // path (and the rename path at open) must now share.
        let dir =
            std::env::temp_dir().join(format!("phelper-journal-midsession-{}", std::process::id()));
        let path = dir.join("control-journal.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut j = ControlJournal::open(&path, "8BAB", "F.30").unwrap();
        let mut appended = 0usize;
        while std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) < MAX_JOURNAL_BYTES {
            let e = j.new_entry(JournalOrigin::User, sample_outcome());
            j.append(&e).unwrap();
            appended += 1;
            assert!(appended < 64, "test entries never crossed the threshold");
        }
        let pre_count = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(pre_count, appended, "no rotation happened yet");

        // The next append crosses the threshold and rotates.
        let e = j.new_entry(JournalOrigin::User, sample_outcome());
        j.append(&e).unwrap();

        let backup = path.with_extension("1.jsonl");
        let backup_text = std::fs::read_to_string(&backup).unwrap();
        assert_eq!(
            backup_text.lines().count(),
            appended,
            "backup holds exactly the pre-rotation entries"
        );
        let active = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            active.lines().count(),
            1,
            "the fresh active file holds only the post-rotation entry"
        );
        assert!(active.contains("\"origin\":\"user\""));

        // And the journal keeps appending to the FRESH handle, not the backup.
        let e = j.new_entry(JournalOrigin::Safety, sample_outcome());
        j.append(&e).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
        assert_eq!(backup_text.lines().count(), appended);

        drop(j);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
