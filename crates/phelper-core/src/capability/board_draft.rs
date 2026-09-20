//! Board profile draft emission from a probe report (used to validate /
//! update the embedded profile — the embedded one always wins at runtime).

use phelper_domain::board::BoardProfile;
use phelper_domain::error::EngineError;
use std::path::Path;

pub fn write_board_profile(profile: &BoardProfile, path: &Path) -> Result<(), EngineError> {
    let text = toml::to_string_pretty(profile)
        .map_err(|e| EngineError::Persistence(format!("serialize board profile: {e}")))?;
    crate::persistence::write_text(path, &text)
}
