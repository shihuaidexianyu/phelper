//! Resident integration application logic.
//!
//! The domain types describe persisted intent. This module adds the small
//! amount of platform-neutral decision logic that turns an OMEN event into a
//! safe application action. Windows task/WMI/pipe details stay behind the
//! platform adapter.

pub use phelper_domain::resident::{
    AutostartState, OmenKeyAction, OmenKeyCapability, OmenKeySettings, OverlayPosition,
    OverlayScreen, OverlaySettings, ResidentSettings, ResidentSnapshot,
};

#[cfg(windows)]
pub use crate::platform::windows_resident::{
    OmenKeyBridge, OmenKeyProbe, ResidentEvent, probe_omen_key, reconcile_autostart,
    remove_omen_key_subscription, send_shortcut, signal_omen_key,
};

#[cfg(not(windows))]
mod non_windows {
    use std::path::Path;

    use super::{AutostartState, OmenKeyCapability, ResidentSettings};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct OmenKeyProbe {
        pub capability: OmenKeyCapability,
        pub detail: String,
    }

    pub struct OmenKeyBridge;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ResidentEvent {
        OmenKeyPressed,
    }

    pub fn probe_omen_key() -> OmenKeyProbe {
        OmenKeyProbe {
            capability: OmenKeyCapability::Unsupported,
            detail: "OMEN 键事件只支持 Windows".into(),
        }
    }

    pub fn reconcile_autostart(
        _enabled: bool,
        _executable: &Path,
    ) -> Result<AutostartState, String> {
        Ok(AutostartState::Disabled)
    }

    pub fn signal_omen_key() -> bool {
        false
    }

    pub fn remove_omen_key_subscription() -> Result<(), String> {
        Ok(())
    }

    pub fn send_shortcut(_shortcut: &str) -> Result<(), String> {
        Err("快捷键注入只支持 Windows".into())
    }

    impl OmenKeyBridge {
        pub fn start(
            _executable: &Path,
        ) -> Result<(Self, std::sync::mpsc::Receiver<ResidentEvent>), String> {
            Err("OMEN 键事件只支持 Windows".into())
        }

        pub fn stop(&mut self) {}
    }

    #[allow(dead_code)]
    pub fn reconcile_settings(_settings: &ResidentSettings) {}
}

#[cfg(not(windows))]
pub use non_windows::{
    OmenKeyBridge, OmenKeyProbe, ResidentEvent, probe_omen_key, reconcile_autostart,
    remove_omen_key_subscription, send_shortcut, signal_omen_key,
};

/// Action resolved after one validated OMEN event. The UI/desktop shell owns
/// the actual overlay operation; profile changes still enter AppHandle's
/// existing command path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OmenKeyIntent {
    ToggleOverlay,
    NextProfile { profile: String },
    SendShortcut { shortcut: String },
}

/// Resolve one event without touching hardware or UI. `None` means the
/// configured action is the safe default (no custom bridge action).
pub fn resolve_omen_key(
    settings: &ResidentSettings,
    current_profile: Option<&str>,
) -> Result<Option<OmenKeyIntent>, String> {
    match settings.omen_key.action {
        OmenKeyAction::Default => Ok(None),
        OmenKeyAction::ToggleOverlay => Ok(Some(OmenKeyIntent::ToggleOverlay)),
        OmenKeyAction::NextProfile => {
            let cycle = &settings.omen_key.profile_cycle;
            if cycle.is_empty() {
                return Err("未配置可循环的 profile".into());
            }
            let next = current_profile
                .and_then(|current| {
                    cycle
                        .iter()
                        .position(|profile| profile.eq_ignore_ascii_case(current))
                })
                .map(|index| (index + 1) % cycle.len())
                .unwrap_or(0);
            Ok(Some(OmenKeyIntent::NextProfile {
                profile: cycle[next].clone(),
            }))
        }
        OmenKeyAction::SendShortcut => {
            let shortcut = settings.omen_key.shortcut.trim();
            if shortcut.is_empty() {
                return Err("未配置快捷键".into());
            }
            Ok(Some(OmenKeyIntent::SendShortcut {
                shortcut: shortcut.to_string(),
            }))
        }
    }
}

/// Make the nested setting type available without making callers depend on
/// the alias used above for the platform re-export.
pub type OmenSettings = OmenKeySettings;

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::resident::OmenKeyAction;

    fn settings(action: OmenKeyAction) -> ResidentSettings {
        ResidentSettings {
            omen_key: OmenKeySettings {
                action,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn default_does_not_create_an_intent() {
        assert_eq!(
            resolve_omen_key(&settings(OmenKeyAction::Default), None).unwrap(),
            None
        );
    }

    #[test]
    fn profile_cycle_advances_and_wraps() {
        let mut s = settings(OmenKeyAction::NextProfile);
        s.omen_key.profile_cycle = vec!["balanced".into(), "gaming".into()];
        assert_eq!(
            resolve_omen_key(&s, Some("balanced")).unwrap(),
            Some(OmenKeyIntent::NextProfile {
                profile: "gaming".into()
            })
        );
        assert_eq!(
            resolve_omen_key(&s, Some("gaming")).unwrap(),
            Some(OmenKeyIntent::NextProfile {
                profile: "balanced".into()
            })
        );
    }

    #[test]
    fn invalid_custom_actions_fail_closed() {
        assert!(resolve_omen_key(&settings(OmenKeyAction::NextProfile), None).is_err());
        assert!(resolve_omen_key(&settings(OmenKeyAction::SendShortcut), None).is_err());
    }
}
