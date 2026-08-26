use std::collections::BTreeMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Canonical metric identifier (architecture.md section 11). Collectors never
/// invent ad-hoc names — every metric is declared in `ids` and registered in
/// the MetricRegistry with its unit/domain/cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MetricId(pub &'static str);

pub mod ids {
    use super::MetricId;

    // CPU silicon (PawnIO / Intel MSR)
    pub const CPU_PKG_TEMP_C: MetricId = MetricId("cpu.pkg_temp_c");
    pub const CPU_TJ_MAX_C: MetricId = MetricId("cpu.tj_max_c");
    pub const CPU_PKG_POWER_W: MetricId = MetricId("cpu.pkg_power_w");
    pub const CPU_EFFECTIVE_CLOCK_MHZ: MetricId = MetricId("cpu.effective_clock_mhz");
    pub const CPU_THERMAL_STATUS_RAW: MetricId = MetricId("cpu.thermal_status_raw");
    /// MSR_PKG_POWER_LIMIT (0x610) decoded PL1/PL2 (x power unit 0x606[3:0]).
    /// Read-only readback of the CURRENT firmware/OS-set limits — this is
    /// also step 2 of the 0x29 three-step verification runbook (§25).
    pub const CPU_PL1_W: MetricId = MetricId("cpu.pl1_w");
    pub const CPU_PL2_W: MetricId = MetricId("cpu.pl2_w");
    pub const CPU_POWER_LIMIT_RAW: MetricId = MetricId("cpu.power_limit_raw");
    // NOTE: MSR_CORE_PERF_LIMIT_REASONS (0x64F) is NOT exposed — verified
    // on-device 2026-08-25: the signed IntelMSR module's allow-list rejects
    // 0x64F with 0x80070005 (access denied). There is no read-only path for
    // throttle-reason bits on this machine; PL1/PL2 (0x610) remain the
    // power-limit evidence.

    // CPU policy (Windows PowrProf readback — the AR-10 verification source
    // for EPP writes; reads do not require elevation)
    pub const CPU_EPP_AC: MetricId = MetricId("cpu.epp_ac");
    pub const CPU_EPP_DC: MetricId = MetricId("cpu.epp_dc");
    /// PERFEPP1 — processor class 1 (E-core) EPP on the 13900HX.
    pub const CPU_EPP1_AC: MetricId = MetricId("cpu.epp1_ac");
    pub const CPU_EPP1_DC: MetricId = MetricId("cpu.epp1_dc");

    // CPU/system (Windows)
    pub const CPU_UTIL_PERCENT: MetricId = MetricId("cpu.util_percent");
    pub const MEM_USED_BYTES: MetricId = MetricId("mem.used_bytes");
    pub const MEM_TOTAL_BYTES: MetricId = MetricId("mem.total_bytes");
    pub const DISK_READ_BPS: MetricId = MetricId("disk.read_bps");
    pub const DISK_WRITE_BPS: MetricId = MetricId("disk.write_bps");
    pub const NET_RX_BPS: MetricId = MetricId("net.rx_bps");
    pub const NET_TX_BPS: MetricId = MetricId("net.tx_bps");

    // GPU (NVAPI)
    pub const GPU_TEMP_C: MetricId = MetricId("gpu.temp_c");
    /// GPU power. Authoritative source: NVML `nvmlDeviceGetPowerUsage` —
    /// the feasibility research claimed NVML is NOT_SUPPORTED on AD107
    /// (R5), but on-device verification (8BAB, driver 581.x) disproved
    /// that: NVML reports a continuous, plausible power curve (1.8 W sleep
    /// → 61.7 W memset load). Declared fallback: NVAPI
    /// ClientPowerTopology, which on this machine reports num_entries=0
    /// (idle AND under load — the driver unreports a laptop dGPU).
    pub const GPU_POWER_W: MetricId = MetricId("gpu.power_w");
    pub const GPU_UTIL_PERCENT: MetricId = MetricId("gpu.util_percent");
    pub const GPU_CORE_CLOCK_MHZ: MetricId = MetricId("gpu.core_clock_mhz");
    pub const GPU_MEM_CLOCK_MHZ: MetricId = MetricId("gpu.mem_clock_mhz");
    pub const GPU_PSTATE: MetricId = MetricId("gpu.pstate");
    pub const GPU_THROTTLE_REASONS_RAW: MetricId = MetricId("gpu.throttle_reasons_raw");
    pub const GPU_VRAM_USED_BYTES: MetricId = MetricId("gpu.vram_used_bytes");
    /// Current GPU power-management limit (TGP incl. Dynamic Boost) via NVML
    /// `nvmlDeviceGetPowerManagementLimit`. Quasi-static; absent when the
    /// driver reports NOT_SUPPORTED.
    pub const GPU_POWER_LIMIT_W: MetricId = MetricId("gpu.power_limit_w");

    // Fans (HP WMI 0x2D, ≤ 1 Hz — architecture.md section 38 hard rule)
    pub const FAN_CPU_RPM: MetricId = MetricId("fan.cpu_rpm");
    pub const FAN_GPU_RPM: MetricId = MetricId("fan.gpu_rpm");

    // Power source
    pub const POWER_AC_ONLINE: MetricId = MetricId("power.ac_online");
    pub const POWER_BATTERY_PERCENT: MetricId = MetricId("power.battery_percent");
}

/// Where a sample came from. Part of the canonical model so the UI can show
/// provenance and the diagnostics page can render the source-ownership map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricSource {
    PawnIoMsr,
    NvapiPublic,
    NvapiClientPowerTopology,
    NvmlPublic,
    WindowsPdh,
    WindowsPower,
    WindowsPpm,
    HpWmi,
    PresentMon,
}

/// Which backend produced a GPU power reading (§12 ownership + declared
/// fallback; the sample's MetricSource is derived from this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuPowerSource {
    /// nvmlDeviceGetPowerUsage. Verified working on 8BAB/AD107 (driver
    /// 581.x) — overrides the R5 research finding.
    Nvml,
    /// NVAPI ClientPowerTopology. Reports zero entries on this machine;
    /// kept as the declared fallback for other environments.
    NvapiTopology,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricQuality {
    Fresh,
    /// Reading is real but known-shaky in this regime (e.g. GPU power at
    /// idle on AD107 — R5).
    Estimated,
    Stale,
    Unavailable,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MetricValue {
    F64(f64),
    U64(u64),
    Bool(bool),
}

impl MetricValue {
    pub fn as_f64(self) -> Option<f64> {
        match self {
            MetricValue::F64(v) => Some(v),
            MetricValue::U64(v) => Some(v as f64),
            MetricValue::Bool(v) => Some(if v { 1.0 } else { 0.0 }),
        }
    }
}

impl From<f64> for MetricValue {
    fn from(v: f64) -> Self {
        MetricValue::F64(v)
    }
}

impl From<u64> for MetricValue {
    fn from(v: u64) -> Self {
        MetricValue::U64(v)
    }
}

/// The canonical sample every collector must produce (architecture.md
/// section 11). No collector writes to UI; everything flows through this.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSample {
    pub id: MetricId,
    pub value: MetricValue,
    pub source: MetricSource,
    pub timestamp: Instant,
    pub quality: MetricQuality,
}

impl MetricSample {
    pub fn fresh(id: MetricId, value: MetricValue, source: MetricSource) -> Self {
        Self {
            id,
            value,
            source,
            timestamp: Instant::now(),
            quality: MetricQuality::Fresh,
        }
    }

    pub fn with_quality(mut self, quality: MetricQuality) -> Self {
        self.quality = quality;
        self
    }
}

/// Per-provider health for the diagnostics surface (telemetry failures are
/// quality downgrades, never thrown errors — D3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStatus {
    Ok,
    Degraded(String),
    Unavailable(String),
    Unsupported(String),
}

impl ProviderStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, ProviderStatus::Ok)
    }
}

/// Point-in-time view of the whole telemetry state.
#[derive(Debug, Clone, Default)]
pub struct TelemetrySnapshot {
    pub samples: BTreeMap<MetricId, MetricSample>,
    pub providers: BTreeMap<&'static str, ProviderStatus>,
    pub at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WindowStats {
    pub min: f64,
    pub max: f64,
    pub avg: f64,
    pub count: usize,
}

// ---- Typed per-provider samples (ports.rs returns these; collectors map
// them into canonical MetricSamples) ----

#[derive(Debug, Clone, Default)]
pub struct CpuSiliconSample {
    pub pkg_temp_c: Option<f64>,
    pub tj_max_c: Option<f64>,
    pub pkg_power_w: Option<f64>,
    pub effective_clock_mhz: Option<f64>,
    /// IA32_THERM_STATUS / IA32_PACKAGE_THERM_STATUS raw bits.
    pub thermal_status_raw: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct GpuSample {
    pub temp_c: Option<f64>,
    pub power_w: Option<f64>,
    /// Which backend produced `power_w` (NVML primary, topology fallback).
    pub power_source: Option<GpuPowerSource>,
    pub util_percent: Option<f64>,
    pub core_clock_mhz: Option<f64>,
    pub mem_clock_mhz: Option<f64>,
    pub pstate: Option<u32>,
    /// NvAPI_GPU_GetPerfDecreaseInfo bitmask.
    pub throttle_reasons: Option<u32>,
    pub vram_used_bytes: Option<u64>,
    /// NVML nvmlDeviceGetPowerManagementLimit (current TGP limit). Quasi-
    /// static; None when NVML or the call is unsupported.
    pub power_limit_w: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct SystemSample {
    pub cpu_util_percent: Option<f64>,
    pub mem_used_bytes: Option<u64>,
    pub mem_total_bytes: Option<u64>,
    pub disk_read_bps: Option<f64>,
    pub disk_write_bps: Option<f64>,
    pub net_rx_bps: Option<f64>,
    pub net_tx_bps: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct PowerSample {
    pub ac_online: Option<bool>,
    pub battery_percent: Option<f64>,
}
