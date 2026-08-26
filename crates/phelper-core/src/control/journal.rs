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

impl ControlJournal {
    /// Default location: `<data_dir>/state/control-journal.jsonl`.
    pub fn default_path() -> PathBuf {
        crate::persistence::data_dir()
            .join("state")
            .join("control-journal.jsonl")
    }

    pub fn open(path: &Path, board_id: &str, bios_version: &str) -> Result<Self, EngineError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| EngineError::Persistence(format!("create {}: {e}", parent.display())))?;
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
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.write_all(b"\n"))
            .and_then(|()| self.file.flush())
            .and_then(|()| self.file.sync_data())
            .map_err(|e| {
                EngineError::Persistence(format!("append {}: {e}", self.path.display()))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::command::{ControlReceipt, ControlStatus, StepOutcome, Verification};
    use phelper_domain::policy::ThermalMode;
    use phelper_domain::command::ControlCommand;
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
}
