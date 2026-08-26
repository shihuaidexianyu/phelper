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

    pub fn new(cpu: u16, gpu: u16) -> Self {
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

/// Fan control priority list (architecture.md section 27):
/// FirmwareAuto > Thermal Profile > Max Fan > Manual (capability-confirmed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanMode {
    FirmwareAuto,
    Max,
    Manual(FanLevels),
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
    pub boost_policy: Option<BoostPolicy>,
    pub power_limits: Option<CpuPowerLimits>,
}
