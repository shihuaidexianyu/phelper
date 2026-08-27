//! UI settings persistence: `UiSettings` TOML at
//! `%LOCALAPPDATA%\phelper\settings.toml`. Deliberately tiny — M6 carries
//! exactly one user preference (theme). Missing file = defaults silently;
//! broken file = defaults + a warning string the Settings page can show
//! (never panic, same discipline as the profile loader).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::persistence;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePref {
    Light,
    #[default]
    Dark,
    System,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiSettings {
    pub theme: ThemePref,
}

impl UiSettings {
    pub fn path() -> PathBuf {
        persistence::data_dir().join("settings.toml")
    }

    /// Load from disk: (settings, warning). Missing → (default, None);
    /// unparseable → (default, Some(warning)).
    pub fn load() -> (Self, Option<String>) {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> (Self, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (Self::default(), None);
            }
            Err(e) => {
                return (
                    Self::default(),
                    Some(format!("无法读取设置文件 {}：{e}", path.display())),
                );
            }
        };
        match toml::from_str(&text) {
            Ok(s) => (s, None),
            Err(e) => (
                Self::default(),
                Some(format!(
                    "设置文件损坏，已回退默认（{}）：{e}",
                    path.display()
                )),
            ),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| format!("序列化设置失败：{e}"))?;
        persistence::write_text(path, &text).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "phelper-uisettings-{name}-{}.toml",
            std::process::id()
        ))
    }

    #[test]
    fn default_is_dark() {
        assert_eq!(UiSettings::default().theme, ThemePref::Dark);
    }

    #[test]
    fn missing_file_is_silent_default() {
        let p = tmp("missing");
        let _ = std::fs::remove_file(&p);
        let (s, warn) = UiSettings::load_from(&p);
        assert_eq!(s, UiSettings::default());
        assert!(warn.is_none());
    }

    #[test]
    fn roundtrip() {
        let p = tmp("roundtrip");
        let _ = std::fs::remove_file(&p);
        let s = UiSettings {
            theme: ThemePref::Light,
        };
        s.save_to(&p).unwrap();
        let (back, warn) = UiSettings::load_from(&p);
        assert_eq!(back, s);
        assert!(warn.is_none());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn broken_file_warns_and_defaults() {
        let p = tmp("broken");
        std::fs::write(&p, "theme = \"purple\"\n").unwrap();
        let (s, warn) = UiSettings::load_from(&p);
        assert_eq!(s, UiSettings::default());
        assert!(warn.is_some());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unknown_fields_rejected() {
        let p = tmp("unknown");
        std::fs::write(&p, "theme = \"light\"\nautostart = true\n").unwrap();
        let (_s, warn) = UiSettings::load_from(&p);
        assert!(warn.is_some(), "deny_unknown_fields must catch stale keys");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn system_variant_roundtrips() {
        let s: UiSettings = toml::from_str("theme = \"system\"\n").unwrap();
        assert_eq!(s.theme, ThemePref::System);
    }
}
