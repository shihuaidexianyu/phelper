use serde::{Deserialize, Serialize};

use crate::policy::{CpuPolicy, CpuPowerLimits, FanMode, GpuPlatformPolicy, ThermalMode};

/// A named bundle of desired knob values (architecture.md §36). Every field
/// is optional: `None` = this profile does not touch that domain (the
/// current state is preserved). Reboot-required knobs (MUX) are NOT
/// representable here by construction — switching display pipelines from a
/// preset would be a nasty surprise.
///
/// Permanently separate from `BoardProfile` (developer-maintained hardware
/// facts): a PerformanceProfile is user-facing intent, never a capability
/// statement (§36).
///
/// TOML serialization note: field order matters — scalar values first,
/// tables last (toml rejects a value after a table). `fan` may serialize as
/// either depending on the variant; its position works for both.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PerformanceProfile {
    /// Human-readable one-liner shown by `profile list`.
    pub description: String,
    pub thermal_mode: Option<ThermalMode>,
    pub fan: Option<FanMode>,
    /// PPM knobs (Windows-native, persist after exit). `cpu.power_limits`
    /// stays R8-poisoned inside CpuPolicy — 0x29 rides ONLY the dedicated
    /// `power_limits` field below (isolated write, per the §25 runbook).
    pub cpu: CpuPolicy,
    /// 0x22 fields; unspecified fields merge from the live 0x21 readback at
    /// apply time (read-modify-write, same semantics as `gpu-policy`).
    pub gpu_policy: Option<GpuPolicyPatch>,
    /// EXPERIMENTAL (0x29). Applying a profile carrying this field requires
    /// the `experimental-hp-power-limits` feature AND Experimental caps —
    /// a stable build rejects the WHOLE profile before any write (AR-11:
    /// never apply half of a rejected intent).
    pub power_limits: Option<CpuPowerLimits>,
}

/// The 0x22 subset a profile may touch. All fields optional — None merges
/// the live 0x21 value (preserve).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GpuPolicyPatch {
    pub ctgp: Option<bool>,
    pub ppab: Option<bool>,
    /// 1=100%, 2=50%, 3=25%, 4=12.5%.
    pub dstate: Option<u8>,
    /// Explicit °C, or None to preserve. (0 is the board's "no knob" value
    /// and means preserve on 8BAB — prefer None in profiles.)
    pub slowdown_temp_c: Option<u8>,
}

impl GpuPolicyPatch {
    /// Read-modify-write merge over the live 0x21 readback.
    pub fn apply(self, base: GpuPlatformPolicy) -> GpuPlatformPolicy {
        GpuPlatformPolicy {
            ctgp: self.ctgp.unwrap_or(base.ctgp),
            ppab: self.ppab.unwrap_or(base.ppab),
            dstate: self.dstate.unwrap_or(base.dstate),
            slowdown_temp_c: self.slowdown_temp_c.unwrap_or(base.slowdown_temp_c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::BoostPolicy;

    #[test]
    fn patch_merges_only_set_fields() {
        let base = GpuPlatformPolicy {
            ctgp: true,
            ppab: true,
            dstate: 1,
            slowdown_temp_c: 0,
        };
        let merged = GpuPolicyPatch {
            ctgp: Some(false),
            ..Default::default()
        }
        .apply(base);
        assert!(!merged.ctgp);
        assert!(merged.ppab);
        assert_eq!(merged.dstate, 1);
        assert_eq!(merged.slowdown_temp_c, 0);
    }

    #[test]
    fn empty_profile_is_default() {
        let p = PerformanceProfile::default();
        assert!(p.thermal_mode.is_none());
        assert!(p.fan.is_none());
        assert_eq!(p.cpu, CpuPolicy::default());
        assert!(p.gpu_policy.is_none());
        assert!(p.power_limits.is_none());
    }

    #[test]
    fn profile_holds_cpu_policy() {
        let mut p = PerformanceProfile::default();
        p.cpu.epp_ac = Some(80);
        p.cpu.boost_policy = Some(BoostPolicy::EfficientAggressive);
        assert_eq!(p.cpu.epp_ac, Some(80));
        assert_eq!(p.cpu.boost_policy, Some(BoostPolicy::EfficientAggressive));
    }
}
