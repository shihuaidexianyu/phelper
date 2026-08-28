//! Resident desktop integration vocabulary.
//!
//! This module deliberately contains no Windows, WMI, HWND or GPUI types.
//! It describes user intent and the small read model needed by the desktop
//! shell for autostart, the OMEN key bridge and the optional overlay.

use serde::{Deserialize, Serialize};

/// Action to run when the supported OMEN event is received.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmenKeyAction {
    /// Do not install a phelper event bridge; preserve HP's default action.
    #[default]
    Default,
    ToggleOverlay,
    NextProfile,
    SendShortcut,
}

/// Persistent OMEN-key settings. The bridge is only installed when the
/// action is not `Default` and the platform capability probe passes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OmenKeySettings {
    pub action: OmenKeyAction,
    /// A normalized shortcut such as `Ctrl+Shift+F10`. The platform layer
    /// validates it before calling SendInput.
    pub shortcut: String,
    /// Explicit cycle order. An empty list means that NextProfile is not
    /// actionable and is treated as a failed configuration, not a guess.
    pub profile_cycle: Vec<String>,
}

/// Position of the compact overlay in the primary monitor work area.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayPosition {
    #[default]
    TopLeft,
    TopRight,
}

/// The first implementation intentionally supports the primary display only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayScreen {
    #[default]
    Primary,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OverlaySettings {
    /// Show the overlay when the resident process starts. Default is hidden.
    pub visible_on_start: bool,
    pub position: OverlayPosition,
    pub screen: OverlayScreen,
}

/// User-facing settings for the resident desktop layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResidentSettings {
    pub autostart: bool,
    pub omen_key: OmenKeySettings,
    pub overlay: OverlaySettings,
}

/// Result of the autostart reconciliation. `Unknown` is the startup value;
/// failures carry a short user-facing detail in `ResidentSnapshot`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutostartState {
    #[default]
    Unknown,
    Disabled,
    Enabled,
    Error,
}

/// Capability state for the physical OMEN event source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmenKeyCapability {
    #[default]
    Unknown,
    Probing,
    Supported,
    Unsupported,
    Error,
}

/// Read-only resident integration state published to `AppState`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResidentSnapshot {
    pub autostart: AutostartState,
    pub autostart_detail: Option<String>,
    pub omen_key: OmenKeyCapability,
    pub omen_key_detail: Option<String>,
    pub overlay_visible: bool,
}
