use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::policy::{CpuPolicy, FanMode, GpuPlatformPolicy, MuxMode, ThermalMode};

/// What the user wants the machine to be. All fields optional — unset means
/// "no intent recorded", not "firmware default".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DesiredState {
    pub profile: Option<String>,
    pub cpu_policy: Option<CpuPolicy>,
    pub thermal_mode: Option<ThermalMode>,
    pub fan_mode: Option<FanMode>,
    pub gpu_platform_policy: Option<GpuPlatformPolicy>,
    pub power_limits: Option<crate::policy::CpuPowerLimits>,
}

/// Observed configuration state, per-field provenance (AR-10).
///
/// On 8BAB the honest per-field mapping is:
/// - `Verified`: a real readback exists (fan levels via 0x2D, GPU policy via
///   0x21, MUX via 0x52, EPP via PowrProf).
/// - `TrustedWrite`: no trustworthy readback exists (thermal mode — BIOS has
///   no query, EC 0x59 is diagnostics-only; max fan — 0x26 is unreliable).
///   Maintained by the KeepAliveService re-asserting the last written value.
/// - `Unknown`: never written/never read. (0x29 power limits sit here until
///   a write verifies against the MSR 0x610 telemetry readback — the §25
///   three-step runbook's step 2.)
#[derive(Debug, Clone, PartialEq)]
pub enum ObservedValue<T> {
    Verified {
        value: T,
        at: Instant,
        source: &'static str,
    },
    TrustedWrite {
        value: T,
        at: Instant,
    },
    Unknown,
}

impl<T> ObservedValue<T> {
    pub fn unknown() -> Self {
        ObservedValue::Unknown
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            ObservedValue::Verified { value, .. } => Some(value),
            ObservedValue::TrustedWrite { value, .. } => Some(value),
            ObservedValue::Unknown => None,
        }
    }

    pub fn is_verified(&self) -> bool {
        matches!(self, ObservedValue::Verified { .. })
    }
}

// Manual impl: derive(Default) would add a bogus `T: Default` bound.
#[allow(clippy::derivable_impls)]
impl<T> Default for ObservedValue<T> {
    fn default() -> Self {
        ObservedValue::Unknown
    }
}

#[derive(Debug, Clone, Default)]
pub struct ObservedState {
    pub thermal_mode: ObservedValue<ThermalMode>,
    pub fan_mode: ObservedValue<FanMode>,
    pub max_fan: ObservedValue<bool>,
    pub gpu_platform_policy: ObservedValue<GpuPlatformPolicy>,
    pub mux: ObservedValue<MuxMode>,
    pub epp_ac: ObservedValue<u8>,
    pub epp_dc: ObservedValue<u8>,
    pub epp1_ac: ObservedValue<u8>,
    pub epp1_dc: ObservedValue<u8>,
    pub power_limits: ObservedValue<crate::policy::CpuPowerLimits>,
}
