//! Pure vocabulary for power-aware automatic scheduling.
//!
//! The domain deliberately does not contain Windows notification handles or
//! process-enumeration policy.  It only describes the power context and the
//! read model that the core can expose to the UI/CLI.

use serde::{Deserialize, Serialize};

/// The power source reported by Windows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerSource {
    #[default]
    Unknown,
    Ac,
    Battery,
}

/// The complete, cheap-to-refresh power context used by automatic policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerContext {
    pub source: PowerSource,
    pub battery_percent: Option<u8>,
    pub battery_saver: Option<bool>,
    /// Active Windows power-plan GUID, when PowrProf returns one.
    pub active_scheme: Option<String>,
    pub observed_at_epoch_ms: u64,
}

/// Automatic scheduling modes exposed by phelper.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomaticMode {
    /// No process policy is owned by the automatic scheduler.
    #[default]
    Off,
    /// On battery, eligible user processes receive E-core CPU Sets and
    /// EcoQoS.  On AC, the automatic scheduler owns no process policy.
    BatteryEfficiency,
}

/// Lifecycle phase of the automatic scheduler.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomaticPhase {
    #[default]
    Disabled,
    /// The mode is enabled, but the power source is not battery or is not
    /// known well enough to make a safe decision.
    Waiting,
    /// A process snapshot is being reconciled.  This is transient and should
    /// not be rendered as an error by the UI.
    Applying,
    /// The desired automatic policy is stable for the current context.
    Active,
    Error,
}

/// Immutable read model for the core automatic scheduler.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutomaticSchedulerSnapshot {
    pub mode: AutomaticMode,
    pub phase: AutomaticPhase,
    pub power: Option<PowerContext>,
    pub managed_processes: u32,
    pub skipped_manual: u32,
    pub last_reconcile_at_epoch_ms: Option<u64>,
    pub last_error: Option<String>,
}
