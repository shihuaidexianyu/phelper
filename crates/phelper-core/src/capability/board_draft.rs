//! Board profile draft emission from a probe report (used to validate /
//! update the embedded profile — the embedded one always wins at runtime).

use phelper_domain::board::BoardProfile;
use phelper_domain::error::EngineError;
use std::path::Path;

pub fn write_board_profile(profile: &BoardProfile, path: &Path) -> Result<(), EngineError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| EngineError::Persistence(format!("create {}: {e}", parent.display())))?;
    }
    let text = toml::to_string_pretty(profile)
        .map_err(|e| EngineError::Persistence(format!("serialize board profile: {e}")))?;
    std::fs::write(path, text)
        .map_err(|e| EngineError::Persistence(format!("write {}: {e}", path.display())))
}
