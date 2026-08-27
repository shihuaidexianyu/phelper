//! zh-CN presentation for domain types (§34: human sentences at the UI
//! boundary, technical detail one expand away). No timezone dependency:
//! timestamps render as relative ages ("3 分钟前").

use phelper_domain::capability::Support;
use phelper_domain::command::Verification;
use phelper_domain::error::ControlError;
use phelper_domain::policy::{BoostPolicy, FanMode, ThermalMode};
use phelper_domain::telemetry::{MetricQuality, MetricSource, ProviderStatus};

use super::state::KnobId;

/// One human sentence per ControlError variant (§34). The banner shows
/// this; the expandable detail shows the Debug form + step table.
pub fn control_error_zh(e: &ControlError) -> String {
    match e {
        ControlError::Unsupported => "本机不支持该功能".into(),
        ControlError::UnsafeRequest { reason } => format!("安全校验拒绝：{reason}"),
        ControlError::PermissionDenied => "权限不足——需要以管理员身份运行".into(),
        ControlError::DriverUnavailable { what } => format!("驱动不可用：{what}"),
        ControlError::FirmwareRejected { detail } => format!("固件拒绝了请求：{detail}"),
        ControlError::VerificationFailed { expected, actual } => {
            format!("写入后回读不符：期望 {expected}，读回 {actual}")
        }
        ControlError::Timeout => "操作超时".into(),
        ControlError::BackendUnavailable { what } => format!("后端不可用：{what}"),
        ControlError::Busy => "另一条控制操作正在进行中".into(),
        ControlError::UnknownProfile { name } => format!("未知配置档：{name}"),
    }
}

pub fn verification_zh(v: &Verification) -> String {
    match v {
        Verification::Verified => "已回读验证".into(),
        Verification::TrustedNoReadback => "信任写入（无回读通道，心跳维持）".into(),
        Verification::Failed { expected, actual } => {
            format!("验证失败：期望 {expected}，读回 {actual}")
        }
        Verification::Skipped => "未验证（已跳过）".into(),
    }
}

pub fn support_zh(s: Support) -> &'static str {
    match s {
        Support::Supported => "支持",
        Support::Experimental => "实验性",
        Support::Unsupported => "不支持",
        Support::NotProbed => "未探测",
    }
}

pub fn quality_zh(q: MetricQuality) -> &'static str {
    match q {
        MetricQuality::Fresh => "实时",
        MetricQuality::Estimated => "估算",
        MetricQuality::Stale => "陈旧",
        MetricQuality::Unavailable => "不可用",
        MetricQuality::Unsupported => "不支持",
    }
}

pub fn source_zh(s: MetricSource) -> &'static str {
    match s {
        MetricSource::PawnIoMsr => "PawnIO MSR",
        MetricSource::NvapiPublic => "NVAPI",
        MetricSource::NvapiClientPowerTopology => "NVAPI 功率拓扑",
        MetricSource::NvmlPublic => "NVML",
        MetricSource::WindowsPdh => "Windows PDH",
        MetricSource::WindowsPower => "Windows 电源",
        MetricSource::WindowsPpm => "Windows PPM",
        MetricSource::HpWmi => "HP WMI",
        MetricSource::PresentMon => "PresentMon",
    }
}

pub fn provider_status_zh(s: &ProviderStatus) -> String {
    match s {
        ProviderStatus::Ok => "正常".into(),
        ProviderStatus::Degraded(d) => format!("降级：{d}"),
        ProviderStatus::Unavailable(d) => format!("不可用：{d}"),
        ProviderStatus::Unsupported(d) => format!("不支持：{d}"),
    }
}

/// AR-10 provenance tag for observed values (§43: UI never hides WHERE a
/// value came from).
pub fn observed_provenance_zh<T>(v: &phelper_domain::state::ObservedValue<T>) -> &'static str {
    use phelper_domain::state::ObservedValue as OV;
    match v {
        OV::Verified { .. } => "已验证",
        OV::TrustedWrite { .. } => "信任写入",
        OV::Unknown => "未知",
    }
}

/// Age of an observed stamp, human zh ("12 秒前" / "3 分钟前"; Unknown →
/// "—"). Displayed wherever a readback could go stale between re-probes.
pub fn observed_age_zh<T>(v: &phelper_domain::state::ObservedValue<T>) -> String {
    use phelper_domain::state::ObservedValue as OV;
    let at = match v {
        OV::Verified { at, .. } | OV::TrustedWrite { at, .. } => at,
        OV::Unknown => return "—".into(),
    };
    let s = at.elapsed().as_secs();
    if s < 60 {
        format!("{s} 秒前")
    } else {
        format!("{} 分钟前", s / 60)
    }
}

/// One-line summary of a ControlCommand for journal/evidence rows.
pub fn command_summary_zh(cmd: &phelper_domain::command::ControlCommand) -> String {
    use phelper_domain::command::ControlCommand as CC;
    match cmd {
        CC::ApplyProfile { profile } => format!("应用配置档 {profile}"),
        CC::SetCpuPolicy(p) => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(v) = p.epp_ac {
                parts.push(format!("EPP AC={v}"));
            }
            if let Some(v) = p.epp_dc {
                parts.push(format!("DC={v}"));
            }
            if let Some(v) = p.epp1_ac {
                parts.push(format!("EPP1 AC={v}"));
            }
            if let Some(v) = p.epp1_dc {
                parts.push(format!("DC={v}"));
            }
            if let Some(v) = p.max_freq_mhz_ac {
                parts.push(format!("上限AC={v}MHz"));
            }
            if let Some(v) = p.max_freq_mhz_dc {
                parts.push(format!("DC={v}MHz"));
            }
            if let Some(b) = p.boost_policy {
                parts.push(format!("睿频={}", boost_zh(b)));
            }
            if p.power_limits.is_some() {
                parts.push("功耗墙(!)".to_string());
            }
            format!("CPU 策略（{}）", parts.join(" "))
        }
        CC::SetThermalMode(m) => format!("散热模式 → {}", thermal_mode_zh(*m)),
        CC::SetFanMode(m) => format!("风扇模式 → {}", fan_mode_zh(m)),
        CC::SetGpuPlatformPolicy(p) => format!(
            "GPU 平台策略（cTGP={} PPAB={} dstate={} 降速={}°C）",
            if p.ctgp { "开" } else { "关" },
            if p.ppab { "开" } else { "关" },
            p.dstate,
            p.slowdown_temp_c
        ),
        CC::SetGpuPlatformPolicyPatch(p) => {
            let mut parts: Vec<String> = Vec::new();
            if let Some(v) = p.ctgp {
                parts.push(format!("cTGP→{}", if v { "开" } else { "关" }));
            }
            if let Some(v) = p.ppab {
                parts.push(format!("PPAB→{}", if v { "开" } else { "关" }));
            }
            if let Some(v) = p.dstate {
                parts.push(format!("dstate→{v}"));
            }
            if let Some(v) = p.slowdown_temp_c {
                parts.push(format!("降速→{v}°C"));
            }
            format!("GPU 平台策略（{}·其余字段取活回读）", parts.join(" "))
        }
        CC::SetPowerLimits(l) => {
            if l.pl4_w == 0 {
                format!("功耗墙 PL1={}W PL2={}W", l.pl1_w, l.pl2_w)
            } else {
                format!("功耗墙 PL1={}W PL2={}W PL4={}W", l.pl1_w, l.pl2_w, l.pl4_w)
            }
        }
        CC::SetMuxMode(_) => "MUX 显示模式（需重启）".into(),
    }
}

pub fn control_status_zh(s: &phelper_domain::command::ControlStatus) -> String {
    use phelper_domain::command::ControlStatus as CS;
    match s {
        CS::Applied { verification } => format!("已应用 · {}", verification_zh(verification)),
        CS::Rejected { error } => format!("已拒绝 · {}", control_error_zh(error)),
        CS::Partial => "部分完成（多步计划中途失败）".into(),
    }
}

#[cfg(feature = "control")]
pub fn journal_origin_zh(o: crate::control::journal::JournalOrigin) -> &'static str {
    use crate::control::journal::JournalOrigin as JO;
    match o {
        JO::User => "用户",
        JO::Keepalive => "心跳",
        JO::Safety => "安全",
        JO::Shutdown => "关机",
    }
}

pub fn boost_zh(b: BoostPolicy) -> &'static str {
    match b {
        BoostPolicy::Disabled => "禁用",
        BoostPolicy::Enabled => "启用",
        BoostPolicy::Aggressive => "激进",
        BoostPolicy::EfficientEnabled => "高效启用",
        BoostPolicy::EfficientAggressive => "高效激进",
        BoostPolicy::AggressiveGuaranteed => "激进（保证）",
        BoostPolicy::EfficientAggressiveGuaranteed => "高效激进（保证）",
    }
}

pub fn thermal_mode_zh(m: ThermalMode) -> &'static str {
    match m {
        ThermalMode::Balanced => "均衡",
        ThermalMode::Performance => "性能",
    }
}

pub fn fan_mode_zh(m: &FanMode) -> String {
    match m {
        FanMode::FirmwareAuto => "固件自动".into(),
        FanMode::Max => "最大转速".into(),
        FanMode::Manual(l) => format!("手动 {} / {} RPM", l.cpu * 100, l.gpu * 100),
        FanMode::Curve(_) => "温度曲线".into(),
    }
}

pub fn knob_zh(k: KnobId) -> &'static str {
    match k {
        KnobId::EppAc => "P 核能效偏好（交流）",
        KnobId::EppDc => "P 核能效偏好（电池）",
        KnobId::Epp1Ac => "E 核能效偏好（交流）",
        KnobId::Epp1Dc => "E 核能效偏好（电池）",
        KnobId::MaxFreqAc => "频率上限（交流）",
        KnobId::MaxFreqDc => "频率上限（电池）",
        KnobId::Boost => "睿频策略",
        KnobId::ThermalMode => "散热模式",
        KnobId::FanMode => "风扇模式",
        KnobId::GpuPolicy => "GPU 平台策略",
        KnobId::PowerLimits => "功耗墙",
        KnobId::Profile => "配置档",
    }
}

/// Relative age for timestamps ("刚刚" / "N 秒前" / "N 分钟前" / "N 小时前").
/// `now_ms`/`at_ms` are epoch milliseconds; a future `at_ms` renders "刚刚".
pub fn age_zh(now_ms: u64, at_ms: u64) -> String {
    let secs = now_ms.saturating_sub(at_ms) / 1000;
    if secs < 5 {
        "刚刚".into()
    } else if secs < 60 {
        format!("{secs} 秒前")
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else {
        format!("{} 小时前", secs / 3600)
    }
}

/// Fixed time-grid bucketing for trend charts: average (age_secs, value)
/// pairs into `buckets` equal buckets spanning [0, window_secs]. Returns
/// (bucket_index, average) with bucket 0 = NEWEST; empty buckets absent.
///
/// One entry per 2.5 s bucket (300 s / 120) means x labels derived from
/// bucket ages are ALWAYS unique — this matters because gpui-component's
/// `ScalePoint` looks a domain value up by FIRST match: duplicate x
/// labels collapse several points onto one x position and the polyline
/// draws vertical zigzags (the M6 "loopy chart" seen on the Thermals
/// page, worst right after startup when points/second > 1). Bucket
/// averaging also aligns mixed collector cadences (250 ms CPU vs 500 ms
/// GPU) by TIME and smooths sub-second jitter for free.
pub fn time_grid(points: &[(f64, f64)], window_secs: f64, buckets: usize) -> Vec<(usize, f64)> {
    if buckets == 0 || window_secs <= 0.0 {
        return Vec::new();
    }
    let width = window_secs / buckets as f64;
    let mut sums: std::collections::BTreeMap<usize, (f64, usize)> = Default::default();
    for (age, v) in points {
        if *age < 0.0 || *age >= window_secs {
            continue;
        }
        let e = sums.entry((*age / width) as usize).or_default();
        e.0 += v;
        e.1 += 1;
    }
    sums.into_iter()
        .map(|(b, (s, n))| (b, s / n as f64))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_grid_buckets_average_and_orient_newest_first() {
        // 10 s window, 5 buckets → 2 s each. Bucket 0 = newest [0,2).
        let pts = vec![
            (0.5, 10.0),
            (1.5, 20.0),  // bucket 0 → avg 15
            (2.5, 30.0),  // bucket 1
            (8.5, 40.0),  // bucket 4
            (10.5, 99.0), // out of window → dropped
        ];
        let g = time_grid(&pts, 10.0, 5);
        assert_eq!(g, vec![(0, 15.0), (1, 30.0), (4, 40.0)]);
        // Empty input / degenerate args.
        assert!(time_grid(&[], 10.0, 5).is_empty());
        assert!(time_grid(&pts, 10.0, 0).is_empty());
        assert!(time_grid(&pts, 0.0, 5).is_empty());
    }

    #[test]
    fn time_grid_label_ages_are_unique_at_second_precision() {
        // The whole point of the grid: one entry per 2.5 s bucket ⇒ the
        // "M:SS" labels derived from bucket-center ages never repeat (the
        // gpui-component ScalePoint collapses duplicate x values onto the
        // first match — the M6 loopy-chart bug). 300 s window, 120 buckets,
        // dense 250 ms input.
        let pts: Vec<(f64, f64)> = (0..1200).map(|i| (i as f64 * 0.25, 50.0)).collect();
        let g = time_grid(&pts, 300.0, 120);
        assert_eq!(g.len(), 120, "every bucket has data");
        let ages: Vec<u64> = g
            .iter()
            .map(|(b, _)| ((*b as f64 + 0.5) * (300.0 / 120.0)) as u64)
            .collect();
        let mut dedup = ages.clone();
        dedup.dedup();
        assert_eq!(
            ages, dedup,
            "bucket-center ages must be unique at 1 s precision"
        );
    }

    #[test]
    fn every_error_variant_has_human_text() {
        let errs = [
            ControlError::Unsupported,
            ControlError::UnsafeRequest { reason: "r".into() },
            ControlError::PermissionDenied,
            ControlError::DriverUnavailable { what: "w".into() },
            ControlError::FirmwareRejected { detail: "d".into() },
            ControlError::VerificationFailed {
                expected: "e".into(),
                actual: "a".into(),
            },
            ControlError::Timeout,
            ControlError::BackendUnavailable { what: "b".into() },
            ControlError::Busy,
            ControlError::UnknownProfile { name: "n".into() },
        ];
        assert_eq!(errs.len(), 10, "ControlError grew — map the new variant");
        for e in &errs {
            let s = control_error_zh(e);
            assert!(!s.is_empty(), "{e:?}");
            assert!(!s.contains("HRESULT"), "raw codes never cross §34: {s}");
        }
    }

    #[test]
    fn verification_and_support_text() {
        assert_eq!(verification_zh(&Verification::Verified), "已回读验证");
        assert!(verification_zh(&Verification::TrustedNoReadback).contains("无回读"));
        assert!(
            verification_zh(&Verification::Failed {
                expected: "1".into(),
                actual: "2".into()
            })
            .contains("验证失败")
        );
        assert_eq!(support_zh(Support::Experimental), "实验性");
        assert_eq!(support_zh(Support::NotProbed), "未探测");
    }

    #[test]
    fn policy_display_names() {
        assert_eq!(boost_zh(BoostPolicy::EfficientAggressive), "高效激进");
        assert_eq!(thermal_mode_zh(ThermalMode::Balanced), "均衡");
        assert_eq!(fan_mode_zh(&FanMode::FirmwareAuto), "固件自动");
        assert_eq!(fan_mode_zh(&FanMode::Max), "最大转速");
        let manual = FanMode::Manual(phelper_domain::policy::FanLevels { cpu: 50, gpu: 42 });
        assert_eq!(fan_mode_zh(&manual), "手动 5000 / 4200 RPM");
        // All 7 boost variants mapped.
        for b in [
            BoostPolicy::Disabled,
            BoostPolicy::Enabled,
            BoostPolicy::Aggressive,
            BoostPolicy::EfficientEnabled,
            BoostPolicy::EfficientAggressive,
            BoostPolicy::AggressiveGuaranteed,
            BoostPolicy::EfficientAggressiveGuaranteed,
        ] {
            assert!(!boost_zh(b).is_empty());
        }
    }

    #[test]
    fn telemetry_display_names() {
        for q in [
            MetricQuality::Fresh,
            MetricQuality::Estimated,
            MetricQuality::Stale,
            MetricQuality::Unavailable,
            MetricQuality::Unsupported,
        ] {
            assert!(!quality_zh(q).is_empty());
        }
        assert_eq!(quality_zh(MetricQuality::Fresh), "实时");
        assert_eq!(source_zh(MetricSource::HpWmi), "HP WMI");
        assert_eq!(source_zh(MetricSource::NvmlPublic), "NVML");
        assert_eq!(provider_status_zh(&ProviderStatus::Ok), "正常");
        assert!(provider_status_zh(&ProviderStatus::Unavailable("x".into())).contains("不可用"));
    }

    #[test]
    fn age_buckets() {
        let t = 1_000_000_000_000u64;
        assert_eq!(age_zh(t, t), "刚刚");
        assert_eq!(age_zh(t, t - 3_000), "刚刚");
        assert_eq!(age_zh(t, t - 30_000), "30 秒前");
        assert_eq!(age_zh(t, t - 5 * 60_000), "5 分钟前");
        assert_eq!(age_zh(t, t - 2 * 3600_000), "2 小时前");
        // Future timestamp: saturating, never underflows.
        assert_eq!(age_zh(t, t + 60_000), "刚刚");
    }
}
