use serde::{Deserialize, Serialize};

/// Thermal policy payload family (architecture.md section 23).
/// 8BAB is statically V1 — no runtime version detection is performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThermalPolicyVersion {
    V0,
    V1,
}

/// Developer-maintained board profile (architecture.md section 36).
/// Answers: what is this machine, what has been verified.
/// Embedded per-board TOML in phelper-core (`boards/<id>.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardProfile {
    pub device: BoardDevice,
    pub hp: BoardHp,
    #[serde(default)]
    pub fan: BoardFan,
    #[serde(default)]
    pub cpu: BoardCpu,
    #[serde(default)]
    pub ec: BoardEc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardDevice {
    /// BaseBoard Product ID this profile matches, e.g. "8BAB".
    pub board_id: String,
    pub marketing_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardHp {
    pub thermal_policy: ThermalPolicyVersion,
    pub supports_gpu_power_mode: bool,
    pub supports_mux: bool,
    /// Support level for 0x29 power limits — "experimental" on 8BAB.
    #[serde(default = "default_power_limits")]
    pub power_limits: crate::capability::Support,
}

fn default_power_limits() -> crate::capability::Support {
    crate::capability::Support::Unsupported
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardFan {
    pub count: u8,
    pub scale: crate::capability::FanScale,
    /// Fallback clamp (in scale units) when the 0x2F table cannot be parsed.
    #[serde(default)]
    pub clamp_min: Option<u16>,
    #[serde(default)]
    pub clamp_max: Option<u16>,
}

impl Default for BoardFan {
    fn default() -> Self {
        Self {
            count: 2,
            scale: crate::capability::FanScale::Krpm,
            clamp_min: None,
            clamp_max: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardCpu {
    /// Invariant TSC frequency in MHz, base of the APERF/MPERF effective
    /// clock derivation. i9-13900HX P-core base = 2200.
    #[serde(default = "default_tsc")]
    pub tsc_mhz: u32,
}

fn default_tsc() -> u32 {
    2200
}

impl Default for BoardCpu {
    fn default() -> Self {
        Self {
            tsc_mhz: default_tsc(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoardEc {
    /// Feasibility R3: 8BAB is on the EC max-fan-freeze blacklist. No EC
    /// write function exists anywhere in the codebase; this flag documents
    /// the prohibition as data so diagnostics can surface *why*.
    #[serde(default = "default_true")]
    pub write_forbidden: bool,
}

fn default_true() -> bool {
    true
}

impl Default for BoardEc {
    fn default() -> Self {
        Self {
            write_forbidden: true,
        }
    }
}
