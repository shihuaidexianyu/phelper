//! Capability snapshot JSON (§35/§49 diagnostic export).

use phelper_domain::error::EngineError;
use std::path::{Path, PathBuf};

use super::ProbeReport;

pub fn write_snapshot(report: &ProbeReport, path: &Path) -> Result<(), EngineError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| EngineError::Persistence(format!("create {}: {e}", parent.display())))?;
    }
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| EngineError::Persistence(format!("serialize snapshot: {e}")))?;
    std::fs::write(path, json)
        .map_err(|e| EngineError::Persistence(format!("write {}: {e}", path.display())))
}

/// Default snapshot path under ./probe-out/ with epoch-millis name.
pub fn default_snapshot_path(base: &Path, epoch_ms: u64) -> PathBuf {
    base.join(format!("capability-{epoch_ms}.json"))
}
