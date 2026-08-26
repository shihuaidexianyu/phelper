use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::ControlError;
use crate::policy::{CpuPolicy, CpuPowerLimits, FanMode, GpuPlatformPolicy, MuxMode, ThermalMode};

/// The only way anything ever asks for a hardware mutation (AR-03).
/// UI/CLI/future API dispatch these; nobody calls `set_pl1()` directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlCommand {
    ApplyProfile {
        profile: String,
    },
    SetCpuPolicy(CpuPolicy),
    SetThermalMode(ThermalMode),
    SetFanMode(FanMode),
    SetGpuPlatformPolicy(GpuPlatformPolicy),
    /// EXPERIMENTAL (0x29). Requires the `experimental-hp-power-limits`
    /// feature AND passes the staged verification runbook.
    SetPowerLimits(CpuPowerLimits),
    /// Reboot-required. Never part of a profile; always a standalone plan.
    SetMuxMode(MuxMode),
}

/// Handle returned on dispatch; correlates with the eventual outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ControlReceipt(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    Verified,
    /// No trustworthy readback exists; the value is keep-alive-maintained.
    TrustedNoReadback,
    Failed {
        expected: String,
        actual: String,
    },
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepOutcome {
    pub step: String,
    pub backend: String,
    /// Firmware/API return as reported by the backend (informational).
    pub firmware_return: Option<String>,
    /// §56 before/after evidence (human-readable observed values around
    /// the write; None when the step never ran). Self-contained in the
    /// JSONL journal entry — no external log needed to audit a write.
    #[serde(default)]
    pub before: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
    pub verification: Verification,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlStatus {
    Applied {
        verification: Verification,
    },
    Rejected {
        error: ControlError,
    },
    /// Some plan steps applied, a later one failed. Steps carry the detail.
    Partial,
}

/// Full record of one dispatched command (architecture.md section 48:
/// every write is journaled with firmware return + verification + duration).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlOutcome {
    pub receipt: ControlReceipt,
    pub command: ControlCommand,
    pub status: ControlStatus,
    pub steps: Vec<StepOutcome>,
    #[serde(with = "duration_millis")]
    pub duration: Duration,
}

mod duration_millis {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        d.as_millis().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}
