//! Compare only profile-owned fields, not unrelated firmware state.
use phelper_domain::{profile::PerformanceProfile, state::ObservedState};
pub(super) fn matches(profile: &PerformanceProfile, observed: &ObservedState) -> bool {
    let cpu = &profile.cpu;
    macro_rules! owned {
        ($target:expr, $actual:expr) => {
            $target.is_none_or(|v| $actual.value() == Some(&v))
        };
    }
    let cpu_matches = owned!(cpu.epp_ac, observed.epp_ac)
        && owned!(cpu.epp_dc, observed.epp_dc)
        && owned!(cpu.epp1_ac, observed.epp1_ac)
        && owned!(cpu.epp1_dc, observed.epp1_dc)
        && owned!(cpu.max_freq_mhz_ac, observed.max_freq_ac)
        && owned!(cpu.max_freq_mhz_dc, observed.max_freq_dc)
        && owned!(cpu.min_performance_ac, observed.min_performance_ac)
        && owned!(cpu.min_performance_dc, observed.min_performance_dc)
        && owned!(cpu.max_performance_ac, observed.max_performance_ac)
        && owned!(cpu.max_performance_dc, observed.max_performance_dc)
        && owned!(cpu.boost_policy_ac.or(cpu.boost_policy), observed.boost_ac)
        && owned!(cpu.boost_policy_dc.or(cpu.boost_policy), observed.boost_dc);
    let gpu_matches = profile.gpu_policy.is_none_or(|patch| {
        observed.gpu_platform_policy.value().is_some_and(|actual| {
            patch.ctgp.is_none_or(|v| v == actual.ctgp)
                && patch.ppab.is_none_or(|v| v == actual.ppab)
                && patch.dstate.is_none_or(|v| v == actual.dstate)
                && patch
                    .slowdown_temp_c
                    .is_none_or(|v| v == actual.slowdown_temp_c)
        })
    });
    cpu_matches && gpu_matches
}
#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::{
        policy::GpuPlatformPolicy, profile::GpuPolicyPatch, state::ObservedValue,
    };
    #[test]
    fn unowned_dynamic_gpu_state_does_not_invalidate_profile() {
        let profile = PerformanceProfile {
            gpu_policy: Some(GpuPolicyPatch {
                ctgp: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut observed = ObservedState {
            gpu_platform_policy: ObservedValue::TrustedWrite {
                value: GpuPlatformPolicy {
                    ctgp: true,
                    ppab: false,
                    dstate: 9,
                    slowdown_temp_c: 0,
                },
                at: std::time::Instant::now(),
            },
            ..Default::default()
        };
        assert!(matches(&profile, &observed));
        observed.gpu_platform_policy = ObservedValue::Unknown;
        assert!(!matches(&profile, &observed));
    }
}
