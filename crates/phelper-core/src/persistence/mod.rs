//! Persistence: config/state directories and writers.
//! Layout follows architecture.md section 47 (per-responsibility files, no
//! monolithic dump).

use phelper_domain::error::EngineError;
use std::path::PathBuf;

/// Runtime data directory: %LOCALAPPDATA%\phelper (fallback: ./probe-out).
pub fn data_dir() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(|p| PathBuf::from(p).join("phelper"))
        .unwrap_or_else(|_| PathBuf::from("./probe-out"))
}

/// Dev-harness probe output directory (repo-local).
pub fn probe_out_dir() -> PathBuf {
    PathBuf::from("./probe-out")
}

pub fn write_text(path: &std::path::Path, text: &str) -> Result<(), EngineError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| EngineError::Persistence(format!("create {}: {e}", parent.display())))?;
    }
    std::fs::write(path, text)
        .map_err(|e| EngineError::Persistence(format!("write {}: {e}", path.display())))
}
