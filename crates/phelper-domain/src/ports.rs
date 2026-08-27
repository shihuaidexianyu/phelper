//! Ports — the hexagonal boundary (architecture.md section 7: backends
//! implement these traits; nothing above `platform/` knows they exist).
//!
//! The operation sets are closed and typed: there is deliberately NO generic
//! `(cmd_type, raw_bytes)` entry point (architecture.md section 50 — no
//! arbitrary payload construction from config/UI).
//!
//! Write traits (`HpControl`, `CpuPolicyBackend`) are DEFINED here
//! unconditionally (pure types, no platform deps); the IMPLEMENTATIONS in
//! phelper-core are gated behind the `control` cargo feature, so default
//! builds are still read-only BY CONSTRUCTION (no impl exists to call).

use crate::error::{HpWmiError, PlatformError};
use crate::hp::{FanTable, SystemDesignData};
use crate::policy::{BoostPolicy, FanLevels, GpuPlatformPolicy, MuxMode, ThermalMode};
use crate::telemetry::{CpuSiliconSample, GpuSample, PowerSample, ProviderStatus, SystemSample};

/// HP platform read surface (implemented by the HpActor handle; the actor
/// thread owns the WMI connection — COM apartment affinity + non-reentrant
/// firmware AML make a single serialization point the correct shape).
pub trait HpPlatform: Send {
    /// 0x10 fan count. ALSO the keep-alive heartbeat op: calling it
    /// maintains user-defined thermal/fan states (hp-wmi.c comment,
    /// manual-fan series c203c59fb5).
    fn fan_count(&self) -> Result<u8, HpWmiError>;
    /// 0x28 system design data.
    fn system_design_data(&self) -> Result<SystemDesignData, HpWmiError>;
    /// 0x2F fan table (input: 4 zero bytes).
    fn fan_table(&self) -> Result<FanTable, HpWmiError>;
    /// 0x2D current fan levels (100-RPM units on V1).
    fn fan_levels(&self) -> Result<FanLevels, HpWmiError>;
    /// 0x21 GPU platform policy read.
    fn gpu_platform_policy(&self) -> Result<GpuPlatformPolicy, HpWmiError>;
    /// 0x52 MUX read (command group 0x01).
    fn mux_mode(&self) -> Result<MuxMode, HpWmiError>;
    /// 0x26 max-fan readback. DIAGNOSTICS ONLY — unreliable on this firmware
    /// family (hp-wmi.c commit 46be1453e6); max-fan state is app-tracked.
    fn max_fan_readback_diagnostic(&self) -> Result<bool, HpWmiError>;
}

/// CPU silicon telemetry via PawnIO read-only MSR (AR-09: no write_msr
/// exists anywhere).
pub trait CpuSiliconTelemetry: Send {
    fn sample(&mut self) -> Result<CpuSiliconSample, PlatformError>;
    fn status(&self) -> ProviderStatus;
}

/// GPU telemetry via NVAPI (public surface + ClientPowerTopology for power).
pub trait GpuTelemetry: Send {
    fn sample(&mut self) -> Result<GpuSample, PlatformError>;
    fn status(&self) -> ProviderStatus;
}

/// Windows OS-level counters (PDH / memory status).
pub trait SystemCounters: Send {
    fn sample(&mut self) -> Result<SystemSample, PlatformError>;
    fn status(&self) -> ProviderStatus;
}

/// AC/battery state.
pub trait PowerStatus: Send {
    fn sample(&mut self) -> Result<PowerSample, PlatformError>;
    fn status(&self) -> ProviderStatus;
}

/// HP platform WRITE surface (M2). Only the ControlCoordinator holds and
/// uses an implementation (AR-03 single-writer). Wire layouts per hp-wmi.c
/// (Tier A); every method maps to exactly one typed firmware op — no raw
/// payload channel exists (§50).
pub trait HpControl: Send {
    /// 0x1A thermal mode set, payload `{0xFF, mode}` (V1: 0x30/0x31),
    /// outsize=0 (hp-wmi.c `HPWMI_SET_PERFORMANCE_MODE` via HPWMI_GM).
    fn set_thermal_mode(&self, mode: ThermalMode) -> Result<(), HpWmiError>;
    /// 0x2E manual fan levels `{cpu, gpu}` in 100-RPM units (V1 krpm),
    /// `{0,0}` = firmware automatic, outsize=0 (hp-wmi.c
    /// `HPWMI_VICTUS_S_FAN_SPEED_SET_QUERY`).
    fn set_fan_levels(&self, levels: FanLevels) -> Result<(), HpWmiError>;
    /// 0x27 max fan on/off, payload = 4-byte LE int 1/0, outsize=0
    /// (hp-wmi.c `HPWMI_FAN_SPEED_MAX_SET_QUERY`).
    fn set_max_fan(&self, on: bool) -> Result<(), HpWmiError>;
    /// 0x22 GPU platform policy set, payload = 4 bytes
    /// `{ctgp, ppab, dstate, gpu_slowdown_temp}`, outsize=0 (hp-wmi.c
    /// `HPWMI_SET_GPU_THERMAL_MODES_QUERY`). Full-structure write — callers
    /// read-modify-write via 0x21 to preserve untouched fields.
    fn set_gpu_platform_policy(
        &self,
        p: crate::policy::GpuPlatformPolicy,
    ) -> Result<(), HpWmiError>;
    /// 0x29 CPU power limits (PL1/PL2). On 8BAB the wire order is
    /// `{PL2, PL1, 0xFF, 0xFF}` (S2-arbitrated 2026-08-26 — NOT the kernel
    /// struct order). pl4/cc are not writable yet: implementations reject
    /// nonzero `pl4_w`/`cpu_gpu_concurrent_w` (0 = NO_CHANGE). AUTHORIZATION
    /// (Experimental caps + cargo feature) lives in the safety layer —
    /// domain traits carry no cargo-feature gates.
    fn set_power_limits(&self, l: crate::policy::CpuPowerLimits) -> Result<(), HpWmiError>;
}

/// The full HP backend: read port + write port (what the coordinator holds).
pub trait HpBackend: HpPlatform + HpControl {}
impl<T: HpPlatform + HpControl> HpBackend for T {}

/// Windows CPU policy backend (AR-08: PowrProf ONLY — never powercfg.exe as
/// a backend, never MSR dual-write). Reads return `(AC, DC)`; writes take
/// `Option` per rail (`None` = leave unchanged) and commit via
/// `PowerSetActiveScheme` on the active scheme.
pub trait CpuPolicyBackend: Send {
    /// EPP 0..=100 (0 = favor performance) as (AC, DC).
    fn read_epp(&self) -> Result<(u8, u8), PlatformError>;
    /// PERFEPP1 (processor class 1 / E-core EPP) as (AC, DC).
    fn read_epp1(&self) -> Result<(u8, u8), PlatformError>;
    /// Max frequency ceiling in MHz as (AC, DC); 0 = unlimited.
    fn read_max_freq_mhz(&self) -> Result<(u32, u32), PlatformError>;
    /// PERFBOOSTMODE as (AC, DC).
    fn read_boost_policy(&self) -> Result<(BoostPolicy, BoostPolicy), PlatformError>;
    fn write_epp(&self, ac: Option<u8>, dc: Option<u8>) -> Result<(), PlatformError>;
    fn write_epp1(&self, ac: Option<u8>, dc: Option<u8>) -> Result<(), PlatformError>;
    fn write_max_freq_mhz(&self, ac: Option<u32>, dc: Option<u32>) -> Result<(), PlatformError>;
    /// The domain models ONE boost value; Windows stores it per rail, so
    /// the implementation writes the same mode to both AC and DC.
    fn write_boost_policy(&self, mode: BoostPolicy) -> Result<(), PlatformError>;
}
