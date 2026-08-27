//! Persistence: config/state directories and writers.
//! Layout follows architecture.md section 47 (per-responsibility files, no
//! monolithic dump).

use phelper_domain::error::EngineError;
use phelper_domain::policy::FanCurve;
use serde::{Deserialize, Serialize};
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

/// The last software curve explicitly applied by phelper. This is a cached
/// editing source, not a claim that the firmware is still running it after
/// the process exits. Keeping it separate from the profile registry lets a
/// user recover a custom curve without turning runtime state into a large
/// config dump.
pub fn fan_curve_path() -> PathBuf {
    data_dir().join("state").join("fan_curve.toml")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFanCurve {
    curve: FanCurve,
}

pub fn load_fan_curve(path: &std::path::Path) -> Result<Option<FanCurve>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let stored: StoredFanCurve =
        toml::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    stored
        .curve
        .validate()
        .map_err(|e| format!("validate {}: {e}", path.display()))?;
    Ok(Some(stored.curve))
}

pub fn save_fan_curve(path: &std::path::Path, curve: &FanCurve) -> Result<(), String> {
    curve.validate().map_err(str::to_owned)?;
    let text = toml::to_string_pretty(&StoredFanCurve { curve: *curve })
        .map_err(|e| format!("serialize fan curve: {e}"))?;
    write_text(path, &text).map_err(|e| e.to_string())
}

pub fn write_text(path: &std::path::Path, text: &str) -> Result<(), EngineError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| EngineError::Persistence(format!("create {}: {e}", parent.display())))?;
    }
    std::fs::write(path, text)
        .map_err(|e| EngineError::Persistence(format!("write {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::policy::FanCurve;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "phelper-fan-curve-{name}-{}.toml",
            std::process::id()
        ))
    }

    #[test]
    fn fan_curve_roundtrips() {
        let path = tmp("roundtrip");
        let _ = std::fs::remove_file(&path);
        let curve = FanCurve::performance();
        save_fan_curve(&path, &curve).unwrap();
        assert_eq!(load_fan_curve(&path).unwrap(), Some(curve));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_fan_curve_is_not_an_error() {
        let path = tmp("missing");
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_fan_curve(&path).unwrap(), None);
    }

    #[test]
    fn invalid_fan_curve_is_rejected() {
        let path = tmp("invalid");
        std::fs::write(
            &path,
            "[curve]\npoints = [{ temp_c = 35, cpu = 0, gpu = 20 }, { temp_c = 55, cpu = 26, gpu = 26 }, { temp_c = 72, cpu = 40, gpu = 42 }, { temp_c = 85, cpu = 55, gpu = 55 }]\n",
        )
        .unwrap();
        assert!(load_fan_curve(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
