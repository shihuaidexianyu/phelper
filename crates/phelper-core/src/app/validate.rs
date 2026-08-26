//! Client-side pre-dispatch validation (§44: the Application layer rejects
//! obviously-bad input before it ever becomes a ControlCommand). Mirrors
//! the CLI's envelopes exactly (crates/phelper-cli/src/control.rs) so the
//! UI and CLI agree on what is even worth sending to the safety layer —
//! which re-checks everything regardless (fail-closed, AR-11).

use phelper_domain::capability::CapabilitySet;
use phelper_domain::policy::CpuPowerLimits;
use phelper_domain::profile::PerformanceProfile;

/// EPP / EPP1: 0..=100 (0 = max performance).
pub fn epp(v: i64) -> Result<u8, String> {
    if (0..=100).contains(&v) {
        Ok(v as u8)
    } else {
        Err(format!("EPP {v} 超出范围 0..=100"))
    }
}

/// Max frequency MHz: 0 = unlimited, else 400..=6000.
pub fn max_freq(mhz: i64) -> Result<u32, String> {
    if mhz == 0 || (400..=6000).contains(&mhz) {
        Ok(mhz as u32)
    } else {
        Err(format!("频率上限 {mhz} MHz 超出合理范围（0 = 不限制，否则 400..=6000）"))
    }
}

/// Manual fan RPM: multiple of 100 (0x2E wire unit), and within the board
/// clamp (×100 RPM units) when the probe reported one.
pub fn fan_rpm(rpm: i64, caps: Option<&CapabilitySet>) -> Result<u16, String> {
    if rpm % 100 != 0 || rpm < 0 {
        return Err(format!("风扇目标必须是 100 的倍数（0x2E 线协议单位）：{rpm}"));
    }
    let level = rpm / 100;
    if let Some(c) = caps
        && let (Some(lo), Some(hi)) = (c.fan.clamp_min, c.fan.clamp_max)
        && !(i64::from(lo)..=i64::from(hi)).contains(&level)
    {
        return Err(format!(
            "风扇目标 {} RPM 超出机身夹取范围 {}..={} RPM",
            rpm,
            u32::from(lo) * 100,
            u32::from(hi) * 100
        ));
    }
    Ok(rpm as u16)
}

/// 0x29 power limits (experimental): 13900HX envelope, PL2 ≥ PL1, PL4 is
/// optional (0 = NO_CHANGE), cc permanently rejected (=0 here).
pub fn power_limits(pl1: i64, pl2: i64, pl4: i64) -> Result<CpuPowerLimits, String> {
    if !(15..=130).contains(&pl1) {
        return Err(format!("PL1 {pl1}W 超出 13900HX 包络 15..=130"));
    }
    if !(15..=157).contains(&pl2) {
        return Err(format!("PL2 {pl2}W 超出 13900HX 包络 15..=157"));
    }
    if pl2 < pl1 {
        return Err(format!("PL2（{pl2}W）不得小于 PL1（{pl1}W）"));
    }
    if pl4 != 0 && !(30..=200).contains(&pl4) {
        return Err(format!("PL4 {pl4}W 超出包络 30..=200（出厂上限，SDD byte5；0 = 不修改）"));
    }
    Ok(CpuPowerLimits {
        pl1_w: pl1 as u8,
        pl2_w: pl2 as u8,
        pl4_w: pl4 as u8,
        cpu_gpu_concurrent_w: 0,
    })
}

/// 0x22 dstate: 1..=4 (100/50/25/12.5%). Note: writes are INEFFECTIVE on
/// 8BAB (M5 HIL) — the UI renders it read-only; this exists for the
/// completeness of the envelope mirror.
pub fn dstate(d: i64) -> Result<u8, String> {
    if (1..=4).contains(&d) {
        Ok(d as u8)
    } else {
        Err(format!("dstate {d} 超出范围 1..=4（100/50/25/12.5%）"))
    }
}

/// 0x22 slowdown temperature: plausible band 30..=110 °C.
pub fn slowdown_temp(t: i64) -> Result<u8, String> {
    if (30..=110).contains(&t) {
        Ok(t as u8)
    } else {
        Err(format!("降速温度 {t} °C 超出合理区间 30..=110"))
    }
}

/// Apply-time gate for profile dispatch (mirrors the CLI apply path and
/// the M5 loader rule: the LOADER never rejects `power_limits`, the APPLY
/// gate does). A profile carrying `power_limits` — top-level or the
/// R8-poisoned `cpu.power_limits` field — requires the double gate:
/// compiled feature AND runtime Experimental capability.
pub fn profile_apply_gate(
    profile: &PerformanceProfile,
    compiled: bool,
    caps: Option<&CapabilitySet>,
) -> Result<(), String> {
    let carries = profile.power_limits.is_some() || profile.cpu.power_limits.is_some();
    if !carries {
        return Ok(());
    }
    if !compiled {
        return Err("该配置档包含实验性功耗墙（0x29），本构建未启用 experimental-hp-power-limits".into());
    }
    match caps {
        Some(c) if c.power_limits == phelper_domain::capability::Support::Experimental => Ok(()),
        _ => Err("本平台未将 0x29 功耗墙标记为 Experimental，拒绝应用".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::capability::{FanCapabilities, FanScale, Support};

    fn caps_with_clamp(lo: u16, hi: u16) -> CapabilitySet {
        let mut c = CapabilitySet::default();
        c.fan = FanCapabilities {
            count: 2,
            scale: FanScale::Krpm,
            clamp_min: Some(lo),
            clamp_max: Some(hi),
            sw_control_declared: true,
        };
        c
    }

    #[test]
    fn epp_envelope() {
        assert_eq!(epp(0).unwrap(), 0);
        assert_eq!(epp(100).unwrap(), 100);
        assert!(epp(101).is_err());
        assert!(epp(-1).is_err());
    }

    #[test]
    fn max_freq_envelope() {
        assert_eq!(max_freq(0).unwrap(), 0);
        assert_eq!(max_freq(400).unwrap(), 400);
        assert_eq!(max_freq(6000).unwrap(), 6000);
        assert!(max_freq(399).is_err());
        assert!(max_freq(6001).is_err());
    }

    #[test]
    fn fan_rpm_multiple_and_clamp() {
        let caps = caps_with_clamp(5, 55); // 500..=5500 RPM
        assert_eq!(fan_rpm(5000, Some(&caps)).unwrap(), 5000);
        assert!(fan_rpm(5050, Some(&caps)).is_err(), "not a multiple of 100");
        assert!(fan_rpm(400, Some(&caps)).is_err(), "below clamp");
        assert!(fan_rpm(5600, Some(&caps)).is_err(), "above clamp");
        // No caps → multiple-of-100 only (safety layer re-checks anyway).
        assert!(fan_rpm(5000, None).is_ok());
        assert!(fan_rpm(-100, None).is_err());
    }

    #[test]
    fn power_limits_envelope() {
        let ok = power_limits(45, 90, 150).unwrap();
        assert_eq!((ok.pl1_w, ok.pl2_w, ok.pl4_w), (45, 90, 150));
        assert_eq!(ok.cpu_gpu_concurrent_w, 0, "cc permanently 0/rejected");
        assert_eq!(power_limits(55, 130, 0).unwrap().pl4_w, 0, "pl4 0 = NO_CHANGE");
        assert!(power_limits(14, 90, 0).is_err());
        assert!(power_limits(45, 158, 0).is_err());
        assert!(power_limits(90, 45, 0).is_err(), "pl2 < pl1");
        assert!(power_limits(45, 90, 29).is_err());
        assert!(power_limits(45, 90, 201).is_err());
    }

    #[test]
    fn gpu_policy_envelopes() {
        assert!(dstate(1).is_ok() && dstate(4).is_ok());
        assert!(dstate(0).is_err() && dstate(5).is_err());
        assert!(slowdown_temp(30).is_ok() && slowdown_temp(110).is_ok());
        assert!(slowdown_temp(29).is_err() && slowdown_temp(111).is_err());
    }

    #[test]
    fn profile_gate_double_lock() {
        let clean: PerformanceProfile =
            toml::from_str("description = \"d\"\n[cpu]\nepp_ac = 80\n").unwrap();
        assert!(profile_apply_gate(&clean, false, None).is_ok());

        let exp: PerformanceProfile = toml::from_str(
            "description = \"d\"\n[power_limits]\npl1_w = 45\npl2_w = 90\npl4_w = 0\ncpu_gpu_concurrent_w = 0\n",
        )
        .unwrap();
        // Stable build: rejected even with an Experimental board.
        let mut caps = CapabilitySet::default();
        caps.power_limits = Support::Experimental;
        assert!(profile_apply_gate(&exp, false, Some(&caps)).is_err());
        // Experimental build but board not Experimental: rejected.
        caps.power_limits = Support::NotProbed;
        assert!(profile_apply_gate(&exp, true, Some(&caps)).is_err());
        assert!(profile_apply_gate(&exp, true, None).is_err());
        // Both halves: allowed.
        caps.power_limits = Support::Experimental;
        assert!(profile_apply_gate(&exp, true, Some(&caps)).is_ok());

        // R8: the cpu-inline field poisons exactly the same way.
        let r8: PerformanceProfile = toml::from_str(
            "description = \"d\"\n[cpu]\nepp_ac = 80\n[cpu.power_limits]\npl1_w = 45\npl2_w = 90\npl4_w = 0\ncpu_gpu_concurrent_w = 0\n",
        )
        .unwrap();
        assert!(profile_apply_gate(&r8, false, Some(&caps)).is_err());
        assert!(profile_apply_gate(&r8, true, Some(&caps)).is_ok());
    }
}
