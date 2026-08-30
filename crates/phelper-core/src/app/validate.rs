//! Apply-time validation retained by the profile-only desktop control path.

use phelper_domain::capability::{CapabilitySet, Support};
use phelper_domain::profile::PerformanceProfile;

pub fn profile_apply_gate(
    profile: &PerformanceProfile,
    compiled: bool,
    caps: Option<&CapabilitySet>,
) -> Result<(), String> {
    let carries_power_limits = profile.power_limits.is_some() || profile.cpu.power_limits.is_some();
    if !carries_power_limits {
        return Ok(());
    }
    if !compiled {
        return Err(
            "该配置档包含实验性功耗墙（0x29），本构建未启用 experimental-hp-power-limits".into(),
        );
    }
    match caps {
        Some(capabilities) if capabilities.power_limits == Support::Experimental => Ok(()),
        _ => Err("本平台未将 0x29 功耗墙标记为 Experimental，拒绝应用".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_profile_needs_no_experimental_gate() {
        let profile: PerformanceProfile =
            toml::from_str("description = \"d\"\n[cpu]\nepp_ac = 80\n").unwrap();
        assert!(profile_apply_gate(&profile, false, None).is_ok());
    }

    #[test]
    fn power_limit_profile_requires_both_gates() {
        let profile: PerformanceProfile = toml::from_str(
            "description = \"d\"\n[power_limits]\npl1_w = 45\npl2_w = 90\npl4_w = 0\ncpu_gpu_concurrent_w = 0\n",
        )
        .unwrap();
        let capabilities = CapabilitySet {
            power_limits: Support::Experimental,
            ..Default::default()
        };
        assert!(profile_apply_gate(&profile, false, Some(&capabilities)).is_err());
        assert!(profile_apply_gate(&profile, true, Some(&capabilities)).is_ok());
    }
}
