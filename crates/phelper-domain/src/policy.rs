use serde::{Deserialize, Serialize};

/// Thermal/performance mode writable on 8BAB (V1: 0x30/0x31).
/// Cool (0x50) is NOT confirmed on 8BAB and is therefore not constructible
/// here (AR-06: unknown means unsupported).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThermalMode {
    Balanced,
    Performance,
}

/// Windows 11's user-selected power-mode overlay. This is deliberately a
/// read-only observation in phelper for now: it is a policy vote, not the
/// effective processor policy, and writing it behind the user's back would
/// compete with Windows Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsConfiguredPowerMode {
    BestEfficiency,
    Balanced,
    BestPerformance,
}

/// Effective Windows power mode reported by PowrProf's notification API.
/// It can differ from the user-configured overlay when another Windows
/// policy signal wins (battery saver, game mode, Modern Standby, and so on).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsEffectivePowerMode {
    BatterySaver,
    BetterBattery,
    Balanced,
    HighPerformance,
    MaxPerformance,
    GameMode,
    MixedReality,
}

/// Display MUX mode. Switching requires a reboot — never auto-applied by
/// profiles (architecture.md section on MUX; feasibility §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MuxMode {
    Hybrid,
    Discrete,
    Optimus,
}

/// Fan levels in units of 100 RPM (V1 "krpm" scale). `0` on both channels
/// means firmware-automatic. The type only ever represents the board's own
/// unit, so a percent value cannot be constructed for a V1 board (R2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanLevels {
    pub cpu: u16,
    pub gpu: u16,
}

impl FanLevels {
    pub const AUTO: Self = Self { cpu: 0, gpu: 0 };

    pub const fn new(cpu: u16, gpu: u16) -> Self {
        Self { cpu, gpu }
    }

    pub fn is_auto(self) -> bool {
        self.cpu == 0 && self.gpu == 0
    }

    pub fn cpu_rpm(self) -> u32 {
        self.cpu as u32 * 100
    }

    pub fn gpu_rpm(self) -> u32 {
        self.gpu as u32 * 100
    }
}

/// Number of control points in the user-facing software fan curve.
///
/// A fixed-size curve keeps the persisted profile format simple and avoids
/// making the UI a small spreadsheet. Four points are enough to describe the
/// useful part of a laptop fan response while still being easy to review.
pub const FAN_CURVE_POINT_COUNT: usize = 4;
pub const FAN_CURVE_MIN_TEMP_C: u8 = 30;
pub const FAN_CURVE_MAX_TEMP_C: u8 = 100;

/// One point in a software fan curve. Fan values use the board's native
/// scale (100-RPM levels on the V1 8BAB board), just like [`FanLevels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanCurvePoint {
    pub temp_c: u8,
    pub cpu: u16,
    pub gpu: u16,
}

impl FanCurvePoint {
    pub const fn new(temp_c: u8, cpu: u16, gpu: u16) -> Self {
        Self { temp_c, cpu, gpu }
    }

    pub const fn levels(self) -> FanLevels {
        FanLevels::new(self.cpu, self.gpu)
    }

    pub const fn cpu_rpm(self) -> u32 {
        self.cpu as u32 * 100
    }

    pub const fn gpu_rpm(self) -> u32 {
        self.gpu as u32 * 100
    }
}

/// A small, linearly interpolated software fan curve.
///
/// The curve is a policy, not a hardware command. The control coordinator
/// evaluates it against fresh telemetry and emits rate-limited `FanLevels`
/// writes through the existing single writer. A curve point never uses zero:
/// zero means firmware-auto on this board and is deliberately reserved for
/// the explicit auto escape hatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanCurve {
    pub points: [FanCurvePoint; FAN_CURVE_POINT_COUNT],
}

impl FanCurve {
    pub const fn new(points: [FanCurvePoint; FAN_CURVE_POINT_COUNT]) -> Self {
        Self { points }
    }

    /// Quiet preset. It still has a positive floor; the firmware-auto mode is
    /// the only mode allowed to stop the fans at idle.
    pub const fn quiet() -> Self {
        Self::new([
            FanCurvePoint::new(35, 20, 20),
            FanCurvePoint::new(55, 20, 20),
            FanCurvePoint::new(72, 30, 30),
            FanCurvePoint::new(88, 50, 50),
        ])
    }

    /// General-purpose preset for sustained mixed CPU/GPU work.
    pub const fn balanced() -> Self {
        Self::new([
            FanCurvePoint::new(35, 20, 20),
            FanCurvePoint::new(55, 26, 26),
            FanCurvePoint::new(72, 40, 42),
            FanCurvePoint::new(85, 55, 55),
        ])
    }

    /// Aggressive preset. It reaches the board's conservative fallback cap
    /// before the independent 90°C safety override is allowed to engage.
    pub const fn performance() -> Self {
        Self::new([
            FanCurvePoint::new(35, 25, 25),
            FanCurvePoint::new(50, 35, 35),
            FanCurvePoint::new(65, 48, 50),
            FanCurvePoint::new(80, 55, 55),
        ])
    }

    /// Validate shape and policy-level invariants. Hardware-specific clamp
    /// validation belongs to the capability-aware safety layer.
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut previous_temp = None;
        for point in self.points {
            if !(FAN_CURVE_MIN_TEMP_C..=FAN_CURVE_MAX_TEMP_C).contains(&point.temp_c) {
                return Err("curve temperatures must be within 30..=100°C");
            }
            if previous_temp.is_some_and(|temp| point.temp_c <= temp) {
                return Err("curve temperatures must be strictly increasing");
            }
            if point.cpu == 0 || point.gpu == 0 {
                return Err(
                    "curve fan levels must be positive; use automatic mode to allow fan-stop",
                );
            }
            previous_temp = Some(point.temp_c);
        }
        Ok(())
    }

    /// Evaluate the curve at a temperature in °C. Values outside the defined
    /// range hold the nearest endpoint. The result remains in the board's
    /// native integer level scale.
    pub fn target_at(&self, temp_c: f64) -> FanLevels {
        let temp_c = if temp_c.is_finite() {
            temp_c
        } else {
            self.points[0].temp_c as f64
        };
        if temp_c <= self.points[0].temp_c as f64 {
            return self.points[0].levels();
        }
        for pair in self.points.windows(2) {
            let left = pair[0];
            let right = pair[1];
            if temp_c <= right.temp_c as f64 {
                let span = (right.temp_c - left.temp_c) as f64;
                let ratio = (temp_c - left.temp_c as f64) / span;
                return FanLevels::new(
                    interpolate_level(left.cpu, right.cpu, ratio),
                    interpolate_level(left.gpu, right.gpu, ratio),
                );
            }
        }
        self.points[FAN_CURVE_POINT_COUNT - 1].levels()
    }
}

fn interpolate_level(left: u16, right: u16, ratio: f64) -> u16 {
    (left as f64 + (right as f64 - left as f64) * ratio).round() as u16
}

/// Fan control priority list (architecture.md section 27):
/// FirmwareAuto > Thermal Profile > Max Fan > Curve/Manual
/// (capability-confirmed). A curve is evaluated by the coordinator and is
/// never sent to firmware as a new wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanMode {
    FirmwareAuto,
    Max,
    Manual(FanLevels),
    Curve(FanCurve),
}

/// HP platform GPU power policy (0x21/0x22 payload). Distinct from NVIDIA
/// driver-level state (architecture.md section 26).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuPlatformPolicy {
    pub ctgp: bool,
    pub ppab: bool,
    /// 1=100%, 2=50%, 3=25%, 4=12.5% (kernel comment, hp-wmi.c).
    pub dstate: u8,
    /// GPU slowdown temperature threshold in °C as reported by firmware
    /// (observed values 0x4B=75, 0x57=87). Preserve on write.
    pub slowdown_temp_c: u8,
}

/// 0x29 power limits payload. EXPERIMENTAL on 8BAB: the pl1/pl2 byte order
/// was settled on-device (M3 S2, 2026-08-26 — firmware wants byte0=PL2,
/// byte1=PL1, OPPOSITE of the kernel struct). `pl4_w` (byte2) is writable
/// since M4.1 (write+readback verified via MCHBAR 0x59B0; envelope
/// 30..=200 W, 0 = not requested → wire 0xFF NO_CHANGE).
/// `cpu_gpu_concurrent_w` (byte3) remains permanently rejected: no readback
/// channel, no restore semantics — 0 is the only accepted value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuPowerLimits {
    pub pl1_w: u8,
    pub pl2_w: u8,
    pub pl4_w: u8,
    pub cpu_gpu_concurrent_w: u8,
}

/// Windows turbo boost policy (PERFBOOSTMODE, GUID
/// be337238-0d82-4146-a960-4f3749d470c7). Wire values match winnt.h
/// `PO_BOOST_*` 0..=6. MS PERFBOOSTMODE doc notes 3/4 alias 1/2 on
/// non-autonomous CPPC parts, and 5/6 ("at guaranteed") may be rejected on
/// platforms without a guaranteed performance level — write verification
/// (readback) is what settles it (AR-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoostPolicy {
    Disabled,
    Enabled,
    Aggressive,
    EfficientEnabled,
    EfficientAggressive,
    AggressiveGuaranteed,
    EfficientAggressiveGuaranteed,
}

/// Readback of the Windows processor-power-management settings for one
/// power-source rail. `None` means that this setting is absent or could not
/// be read on the current Windows/CPU combination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsPpmValues {
    pub epp: Option<u8>,
    pub epp1: Option<u8>,
    pub boost_policy: Option<BoostPolicy>,
    /// 0 = unlimited; otherwise MHz.
    pub max_freq_mhz: Option<u32>,
    /// PPM minimum performance percentage, 0..=100.
    pub min_performance: Option<u8>,
    /// PPM maximum performance percentage, 0..=100.
    pub max_performance: Option<u8>,
}

/// Read-only Windows software-policy snapshot. The active scheme is the
/// PowrProf scheme whose AC/DC indexes phelper writes; configured/effective
/// modes are kept alongside it so the UI can expose policy conflicts without
/// pretending that a high-level mode is the same thing as these parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowsPpmState {
    pub active_scheme_guid: String,
    pub active_scheme_name: String,
    pub configured_ac_mode: Option<WindowsConfiguredPowerMode>,
    pub configured_dc_mode: Option<WindowsConfiguredPowerMode>,
    pub effective_mode: Option<WindowsEffectivePowerMode>,
    pub ac: WindowsPpmValues,
    pub dc: WindowsPpmValues,
}

impl From<BoostPolicy> for u8 {
    fn from(p: BoostPolicy) -> u8 {
        match p {
            BoostPolicy::Disabled => 0,
            BoostPolicy::Enabled => 1,
            BoostPolicy::Aggressive => 2,
            BoostPolicy::EfficientEnabled => 3,
            BoostPolicy::EfficientAggressive => 4,
            BoostPolicy::AggressiveGuaranteed => 5,
            BoostPolicy::EfficientAggressiveGuaranteed => 6,
        }
    }
}

impl TryFrom<u8> for BoostPolicy {
    type Error = ();
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        Ok(match v {
            0 => Self::Disabled,
            1 => Self::Enabled,
            2 => Self::Aggressive,
            3 => Self::EfficientEnabled,
            4 => Self::EfficientAggressive,
            5 => Self::AggressiveGuaranteed,
            6 => Self::EfficientAggressiveGuaranteed,
            _ => return Err(()),
        })
    }
}

/// Full CPU policy (architecture.md section 16). `None` = leave unchanged.
/// `default` allows sparse TOML tables (profiles); `deny_unknown_fields`
/// catches hand-written config typos.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CpuPolicy {
    /// Energy Performance Preference 0..=100 (0 = favor performance).
    pub epp_ac: Option<u8>,
    pub epp_dc: Option<u8>,
    /// PERFEPP1: processor class 1 (E-core) EPP on heterogeneous CPUs.
    pub epp1_ac: Option<u8>,
    pub epp1_dc: Option<u8>,
    pub max_freq_mhz_ac: Option<u32>,
    pub max_freq_mhz_dc: Option<u32>,
    /// Legacy shorthand: apply the same boost policy to AC and DC. Kept for
    /// existing profiles; the rail-specific fields below take precedence.
    pub boost_policy: Option<BoostPolicy>,
    /// Independent AC/DC boost policy. Windows stores these as two indexes,
    /// so a profile or command can now change one rail without touching the
    /// other.
    #[serde(alias = "boost_ac")]
    pub boost_policy_ac: Option<BoostPolicy>,
    #[serde(alias = "boost_dc")]
    pub boost_policy_dc: Option<BoostPolicy>,
    /// PPM performance floor/ceiling, independently for AC/DC. These are
    /// percentages, not MHz: 0..=100. They are intentionally separate from
    /// EPP, which is a preference rather than a hard bound.
    #[serde(alias = "min_perf_ac")]
    pub min_performance_ac: Option<u8>,
    #[serde(alias = "min_perf_dc")]
    pub min_performance_dc: Option<u8>,
    #[serde(alias = "max_perf_ac")]
    pub max_performance_ac: Option<u8>,
    #[serde(alias = "max_perf_dc")]
    pub max_performance_dc: Option<u8>,
    pub power_limits: Option<CpuPowerLimits>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_interpolates_each_fan_channel() {
        let curve = FanCurve::new([
            FanCurvePoint::new(40, 20, 30),
            FanCurvePoint::new(60, 40, 50),
            FanCurvePoint::new(80, 50, 60),
            FanCurvePoint::new(90, 55, 63),
        ]);
        assert_eq!(curve.target_at(35.0), FanLevels::new(20, 30));
        assert_eq!(curve.target_at(50.0), FanLevels::new(30, 40));
        assert_eq!(curve.target_at(95.0), FanLevels::new(55, 63));
    }

    #[test]
    fn curve_rejects_zero_levels_and_unsorted_temperatures() {
        let zero = FanCurve::new([
            FanCurvePoint::new(35, 0, 20),
            FanCurvePoint::new(55, 26, 26),
            FanCurvePoint::new(72, 40, 42),
            FanCurvePoint::new(85, 55, 55),
        ]);
        assert!(zero.validate().is_err());

        let unsorted = FanCurve::new([
            FanCurvePoint::new(55, 20, 20),
            FanCurvePoint::new(35, 26, 26),
            FanCurvePoint::new(72, 40, 42),
            FanCurvePoint::new(85, 55, 55),
        ]);
        assert!(unsorted.validate().is_err());
    }

    #[test]
    fn presets_are_valid_and_positive() {
        for curve in [
            FanCurve::quiet(),
            FanCurve::balanced(),
            FanCurve::performance(),
        ] {
            assert!(curve.validate().is_ok());
            assert!(
                curve
                    .points
                    .iter()
                    .all(|point| point.cpu > 0 && point.gpu > 0)
            );
        }
    }
}
