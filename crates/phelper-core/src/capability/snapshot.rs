//! Capability snapshot JSON (§35/§49 diagnostic export).

use phelper_domain::error::EngineError;
use std::path::{Path, PathBuf};

use super::ProbeReport;

pub fn write_snapshot(report: &ProbeReport, path: &Path) -> Result<(), EngineError> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| EngineError::Persistence(format!("serialize snapshot: {e}")))?;
    crate::persistence::write_text(path, &json)
}

/// Default snapshot path under ./probe-out/ with epoch-millis name.
pub fn default_snapshot_path(base: &Path, epoch_ms: u64) -> PathBuf {
    base.join(format!("capability-{epoch_ms}.json"))
}
