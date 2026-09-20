use phelper_domain::error::{ControlError, EngineError};
use phelper_domain::policy::{CpuPowerLimits, GpuPlatformPolicy};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecoveryRecord {
    pub board: String,
    pub bios: String,
    pub fan: bool,
    pub thermal: bool,
    pub gpu: Option<GpuPlatformPolicy>,
    pub power: Option<CpuPowerLimits>,
    #[serde(default)]
    pub scoped: Option<ScopedSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ScopedSession {
    pub id: u64,
    pub baseline: phelper_domain::profile::PerformanceProfile,
    pub previous_profile: Option<String>,
    pub scheme: Option<String>,
}

impl RecoveryRecord {
    pub fn pending(&self) -> bool {
        self.fan
            || self.thermal
            || self.gpu.is_some()
            || self.power.is_some()
            || self.scoped.is_some()
    }
}

pub(super) struct RecoveryStore {
    path: PathBuf,
    pub board: String,
    pub bios: String,
}

impl RecoveryStore {
    pub fn open(
        journal: &Path,
        board: &str,
        bios: &str,
    ) -> Result<(Self, Option<RecoveryRecord>), EngineError> {
        let path = journal.with_extension("recovery.json");
        let record: Option<RecoveryRecord> = match std::fs::read_to_string(&path) {
            Ok(text) => Some(serde_json::from_str(&text).map_err(|e| {
                EngineError::Persistence(format!(
                    "unreadable recovery ledger {}: {e}",
                    path.display()
                ))
            })?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(EngineError::Persistence(e.to_string())),
        };
        Ok((
            Self {
                path,
                board: board.into(),
                bios: bios.into(),
            },
            record.filter(RecoveryRecord::pending),
        ))
    }

    pub fn save(&self, record: &RecoveryRecord) -> Result<(), ControlError> {
        let text = serde_json::to_string_pretty(record)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                crate::persistence::write_atomic(&self.path, &text).map_err(|e| e.to_string())
            });
        text.map_err(|what| ControlError::BackendUnavailable {
            what: format!("恢复记录无法保存：{what}"),
        })
    }
}
