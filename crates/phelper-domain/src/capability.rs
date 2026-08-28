use serde::{Deserialize, Serialize};

/// Four-state support level (feasibility R4: this board genuinely has four
/// states — e.g. 0x29 = Experimental, 0x26 readback = Unsupported).
///
/// Probe results may only ever *downgrade* the BoardProfile's level, never
/// upgrade it (AR-05/AR-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    /// Behind a compile-time experimental feature; usable only with explicit
    /// user intent (e.g. 0x29 power limits on 8BAB).
    Experimental,
    Unsupported,
    #[default]
    NotProbed,
}

impl Support {
    pub fn is_usable(self) -> bool {
        matches!(self, Support::Supported | Support::Experimental)
    }
}

/// Fan level scale (feasibility R2). V1 boards (8BAB, 2023) take levels in
/// units of 100 RPM; V2 boards (2024+) take percent. Sending percent to a V1
/// firmware is a known crash vector, so the scale is carried here and the
/// wire encoder only ever constructs the board's own unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanScale {
    /// level * 100 = RPM (V1).
    Krpm,
    /// 0-100 percent (V2; unused on 8BAB, modelled so it can never be sent
    /// by accident).
    Percent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanCapabilities {
    pub count: u8,
    pub scale: FanScale,
    /// Clamp range in the board's own scale unit (level or percent).
    /// From the 0x2F fan table probe, else the BoardProfile fallback.
    pub clamp_min: Option<u16>,
    pub clamp_max: Option<u16>,
    /// SDD byte 4 bit 0: firmware declares software fan control support.
    pub sw_control_declared: bool,
}

impl Default for FanCapabilities {
    fn default() -> Self {
        Self {
            count: 0,
            scale: FanScale::Krpm,
            clamp_min: None,
            clamp_max: None,
            sw_control_declared: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PpmCapabilities {
    /// PERFEPP read/write via PowrProf.
    pub epp: Support,
    /// PERFEPP1 (processor class 1 / E-core EPP) read/write via PowrProf.
    pub epp1: Support,
    /// PROCFREQMAX read/write via PowrProf.
    pub max_freq: Support,
    /// PERFBOOSTMODE read/write via PowrProf.
    pub boost: Support,
    /// PROCTHROTTLEMIN read/write via PowrProf.
    pub min_performance: Support,
    /// PROCTHROTTLEMAX read/write via PowrProf.
    pub max_performance: Support,
    /// Process token is elevated — PowrProf writes will succeed.
    pub write_privileged: bool,
}

impl PpmCapabilities {
    pub fn not_probed() -> Self {
        Self {
            epp: Support::NotProbed,
            epp1: Support::NotProbed,
            max_freq: Support::NotProbed,
            boost: Support::NotProbed,
            min_performance: Support::NotProbed,
            max_performance: Support::NotProbed,
            write_privileged: false,
        }
    }
}

/// The full capability surface. Built by CapabilityService::probe() from the
/// BoardProfile (upper bound) intersected with live probe results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilitySet {
    /// True when the board ID matched a known BoardProfile. False → the
    /// engine runs read-only diagnostics (AR-05/AR-06).
    pub known_board: bool,
    /// 0x1A thermal mode set.
    pub thermal_mode: Support,
    /// 0x2D fan RPM readback.
    pub fan_rpm_read: Support,
    /// 0x2E manual fan level set.
    pub fan_manual_level: Support,
    /// 0x27 max fan.
    pub max_fan: Support,
    /// 0x21/0x22 GPU platform policy (cTGP/PPAB/dstate).
    pub gpu_platform_policy: Support,
    /// 0x52 display MUX (reboot-required).
    pub mux: Support,
    /// 0x29 CPU power limits. Experimental on 8BAB until the three-step
    /// verification (WMI write → MSR 0x610 readback → RAPL under load) passes.
    pub power_limits: Support,
    pub fan: FanCapabilities,
    pub ppm: PpmCapabilities,
    /// Per-domain reason notes for diagnostics ("why is X unsupported").
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Default for CapabilitySet {
    fn default() -> Self {
        Self {
            known_board: false,
            thermal_mode: Support::NotProbed,
            fan_rpm_read: Support::NotProbed,
            fan_manual_level: Support::NotProbed,
            max_fan: Support::NotProbed,
            gpu_platform_policy: Support::NotProbed,
            mux: Support::NotProbed,
            power_limits: Support::NotProbed,
            fan: FanCapabilities::default(),
            ppm: PpmCapabilities::not_probed(),
            notes: Vec::new(),
        }
    }
}
