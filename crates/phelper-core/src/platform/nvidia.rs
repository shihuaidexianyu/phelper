//! NVAPI hand-rolled FFI (no external crate — nvapi-sys is stale).
//!
//! Mechanism (stable since forever): LoadLibrary nvapi64.dll →
//! GetProcAddress("nvapi_QueryInterface") → per-function IDs.
//!
//! GPU power: NVML `nvmlDeviceGetPowerUsage` is the authoritative source —
//! the R5 research finding ("NVML NOT_SUPPORTED on AD107") was disproved
//! on-device (8BAB, driver 581.x: 1.8 W sleep → 61.7 W load, nvidia-smi
//! cross-checked). ClientPowerTopologyGetStatus reports num_entries=0 on
//! this machine (idle AND load) and remains only as the declared fallback.
//! See the Nvml block below and architecture.md §12/§29.
//!
//! Function IDs and struct layouts below were cross-checked against
//! LibreHardwareMonitor NvApi.cs and verified by on-device sample runs.

#![cfg(feature = "nvidia")]
// The q!() macro transmutes type-erased interface-table pointers into the
// per-function pointer type of each struct field — the target type is
// fixed by the field being initialized, so inline annotations add noise,
// not safety.
#![allow(clippy::missing_transmute_annotations)]

use std::sync::OnceLock;

use phelper_domain::error::PlatformError;
use phelper_domain::ports::GpuTelemetry;
use phelper_domain::telemetry::{GpuSample, ProviderStatus};
use tracing::debug;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::PCWSTR;

// nvapi function IDs (u32 tags into the driver's interface table).
// Verified: LibreHardwareMonitor NvApi.cs, NVIDIA nvapi docs, and the
// nvPstate/nvPerf/nvGeneral spec tables (0x927DA4F6 / 0x7F7F4600 / 0x07F9B368).
const ID_INITIALIZE: u32 = 0x0150_E828; // NvAPI_Initialize (== QueryInterface id)
const ID_ENUM_PHYSICAL_GPUS: u32 = 0xE5AC_921F;
const ID_GET_FULL_NAME: u32 = 0xCEEE_8E9F;
const ID_GET_THERMAL_SETTINGS: u32 = 0xE364_0A56;
const ID_GET_ALL_CLOCK_FREQUENCIES: u32 = 0xDCB6_16C3;
const ID_GET_DYNAMIC_PSTATES_INFO_EX: u32 = 0x60DE_D2ED;
const ID_GET_CURRENT_PSTATE: u32 = 0x927D_A4F6;
const ID_GET_PERF_DECREASE_INFO: u32 = 0x7F7F_4600;
const ID_GET_MEMORY_INFO_EX: u32 = 0xC059_9498;
const ID_CLIENT_POWER_TOPOLOGY_GET_STATUS: u32 = 0xEDCF_624E;

const MAX_GPUS: usize = 64;
const SHORT_STRING: usize = 64;
const NVAPI_GENERIC_OK: i32 = 0;

type NvStatus = i32;

macro_rules! nv_version {
    ($ty:ty, $major:expr) => {
        (std::mem::size_of::<$ty>() as u32) | ($major << 16)
    };
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ThermalSensor {
    controller: i32,
    default_min_temp: i32,
    default_max_temp: i32,
    current_temp: i32,
    target: i32,
}

#[repr(C)]
struct ThermalSettingsV2 {
    version: u32,
    count: u32,
    // NVAPI_MAX_THERMAL_SENSORS_PER_GPU = 3 (a 32-entry array here changes
    // sizeof, and the version field embeds sizeof — the call then fails).
    sensor: [ThermalSensor; 3],
}

impl Default for ThermalSettingsV2 {
    fn default() -> Self {
        Self {
            version: nv_version!(Self, 2),
            count: 0,
            sensor: [ThermalSensor {
                controller: 0,
                default_min_temp: 0,
                default_max_temp: 0,
                current_temp: 0,
                target: 0,
            }; 3],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ClockDomain {
    is_present: u32,
    frequency_khz: u32,
}

#[repr(C)]
struct ClockFrequenciesV2 {
    version: u32,
    reserved: u32,
    domain: [ClockDomain; 32],
}

impl Default for ClockFrequenciesV2 {
    fn default() -> Self {
        Self {
            version: nv_version!(Self, 2),
            reserved: 0,
            domain: [ClockDomain::default(); 32],
        }
    }
}

const CLOCK_DOMAIN_GRAPHICS: usize = 0;
const CLOCK_DOMAIN_MEMORY: usize = 4;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UtilizationEntry {
    is_present: u32,
    percentage: u32,
}

#[repr(C)]
struct DynamicPstatesInfoEx {
    version: u32,
    flags: u32,
    utilization: [UtilizationEntry; 8],
}

impl Default for DynamicPstatesInfoEx {
    fn default() -> Self {
        Self {
            version: nv_version!(Self, 1),
            flags: 0,
            utilization: [UtilizationEntry::default(); 8],
        }
    }
}

const UTIL_DOMAIN_GPU: usize = 0;

/// ClientPowerTopology entry (undocumented; layout as mirrored by LHM):
/// { Domain, reserved, PowerUsage(mW), reserved }.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PowerTopologyEntry {
    domain: u32,
    _reserved0: u32,
    power_usage_mw: u32,
    _reserved1: u32,
}

const POWER_TOPOLOGY_DOMAIN_GPU: u32 = 0;

#[repr(C)]
struct PowerTopologyStatusV1 {
    version: u32,
    num_entries: u32,
    entries: [PowerTopologyEntry; 4],
}

impl Default for PowerTopologyStatusV1 {
    fn default() -> Self {
        Self {
            version: nv_version!(Self, 1),
            num_entries: 0,
            entries: [PowerTopologyEntry::default(); 4],
        }
    }
}

/// GetMemoryInfoEx (u64 fields, sizes in BYTES, takes the GPU handle —
/// unlike GetMemoryInfo which needs a display handle, which a hybrid-mode
/// dGPU does not have).
#[repr(C)]
#[derive(Default)]
struct MemoryInfoExV1 {
    version: u32,
    dedicated_video_memory: u64,
    available_dedicated_video_memory: u64,
    system_video_memory: u64,
    shared_system_memory: u64,
    current_available_dedicated_video_memory: u64,
    dedicated_video_memory_evictions_size: u64,
    dedicated_video_memory_eviction_count: u64,
    dedicated_video_memory_promotions_size: u64,
    dedicated_video_memory_promotion_count: u64,
}

impl MemoryInfoExV1 {
    fn new() -> Self {
        Self {
            version: nv_version!(Self, 1),
            ..Default::default()
        }
    }
}

type QueryInterface = unsafe extern "C" fn(id: u32) -> *const core::ffi::c_void;

// ---------------------------------------------------------------------------
// NVML mini-surface — power reading only.
//
// The feasibility research (R5) claimed NVML power is NOT_SUPPORTED on
// AD107. On-device verification on 8BAB (driver 581.x) disproved it:
// nvmlDeviceGetPowerUsage reports a continuous plausible curve (1.8 W sleep
// → 61.7 W memset load → 8.4 W settle). NVML is therefore the AUTHORITATIVE
// gpu.power source here; ClientPowerTopology (which reports num_entries=0
// on this machine, idle and load alike) stays as the declared fallback.
// ---------------------------------------------------------------------------

struct Nvml {
    _lib: HMODULE,
    init: Option<unsafe extern "C" fn() -> i32>,
    handle_by_index: Option<unsafe extern "C" fn(u32, *mut *mut core::ffi::c_void) -> i32>,
    get_power_usage: Option<unsafe extern "C" fn(*const core::ffi::c_void, *mut u32) -> i32>,
    // S4 finding (on-device, driver 581.x): GetPowerManagementLimit returns
    // NOT_SUPPORTED (rc=3) on AD107 Laptop; GetEnforcedPowerLimit works and
    // matches nvidia-smi's "Current Power Limit" (80.00 W) exactly.
    get_enforced_power_limit:
        Option<unsafe extern "C" fn(*const core::ffi::c_void, *mut u32) -> i32>,
}

// SAFETY: NVML entry points are thread-safe per NVIDIA docs; the library
// handle is process-global.
unsafe impl Send for Nvml {}
unsafe impl Sync for Nvml {}

static NVML: OnceLock<Result<Nvml, String>> = OnceLock::new();

impl Nvml {
    fn load() -> Result<Self, PlatformError> {
        unsafe {
            let lib = LoadLibraryW(PCWSTR(
                "nvml.dll\0".encode_utf16().collect::<Vec<u16>>().as_ptr(),
            ))
            .map_err(|_| PlatformError::NotAvailable("nvml.dll not found"))?;
            macro_rules! g {
                ($name:literal) => {{ GetProcAddress(lib, windows::core::s!($name)).map(|p| std::mem::transmute(p)) }};
            }
            let nvml = Self {
                _lib: lib,
                init: g!("nvmlInit_v2"),
                handle_by_index: g!("nvmlDeviceGetHandleByIndex_v2"),
                get_power_usage: g!("nvmlDeviceGetPowerUsage"),
                get_enforced_power_limit: g!("nvmlDeviceGetEnforcedPowerLimit"),
            };
            let init = nvml
                .init
                .ok_or(PlatformError::NotAvailable("nvmlInit_v2 missing"))?;
            if init() != 0 {
                return Err(PlatformError::Driver("nvmlInit failed".into()));
            }
            Ok(nvml)
        }
    }

    fn get() -> Result<&'static Nvml, PlatformError> {
        NVML.get_or_init(|| Nvml::load().map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| PlatformError::Driver(format!("nvml init: {e}")))
    }
}

/// NVML device 0 (the single dGPU on the reference machine).
struct NvmlDevice {
    handle: *mut core::ffi::c_void,
}

// SAFETY: NVML device handles are process-global opaque references, safe to
// use from any thread per NVIDIA docs.
unsafe impl Send for NvmlDevice {}

impl NvmlDevice {
    fn open() -> Option<Self> {
        let nvml = Nvml::get().ok()?;
        unsafe {
            let mut handle = std::ptr::null_mut();
            (nvml.handle_by_index?(0, &mut handle) == 0 && !handle.is_null())
                .then_some(Self { handle })
        }
    }

    /// Board power in watts (mW → W). Returns None on any NVML error —
    /// the caller falls back to ClientPowerTopology.
    fn power_w(&self) -> Option<f64> {
        let nvml = Nvml::get().ok()?;
        unsafe {
            let mut mw: u32 = 0;
            (nvml.get_power_usage?(self.handle, &mut mw) == 0 && mw > 0)
                .then_some(mw as f64 / 1000.0)
        }
    }

    /// Enforced power limit (TGP cap) in watts. Quasi-static — the
    /// collector re-pushes it on the TjMax pattern. None when the driver
    /// errors (Option-missing metric, never a fake value).
    fn power_limit_w(&self) -> Option<f64> {
        let nvml = Nvml::get().ok()?;
        unsafe {
            let mut mw: u32 = 0;
            (nvml.get_enforced_power_limit?(self.handle, &mut mw) == 0 && mw > 0)
                .then_some(mw as f64 / 1000.0)
        }
    }
}

struct NvApi {
    _lib: HMODULE,
    initialize: Option<unsafe extern "C" fn() -> NvStatus>,
    enum_gpus: Option<unsafe extern "C" fn(*mut *mut core::ffi::c_void, *mut u32) -> NvStatus>,
    get_full_name: Option<unsafe extern "C" fn(*const core::ffi::c_void, *mut u8) -> NvStatus>,
    get_thermal_settings: Option<
        unsafe extern "C" fn(*const core::ffi::c_void, u32, *mut ThermalSettingsV2) -> NvStatus,
    >,
    get_clock_frequencies:
        Option<unsafe extern "C" fn(*const core::ffi::c_void, *mut ClockFrequenciesV2) -> NvStatus>,
    get_dynamic_pstates: Option<
        unsafe extern "C" fn(*const core::ffi::c_void, *mut DynamicPstatesInfoEx) -> NvStatus,
    >,
    get_current_pstate:
        Option<unsafe extern "C" fn(*const core::ffi::c_void, *mut u32) -> NvStatus>,
    get_perf_decrease_info:
        Option<unsafe extern "C" fn(*const core::ffi::c_void, *mut u32) -> NvStatus>,
    get_memory_info_ex:
        Option<unsafe extern "C" fn(*const core::ffi::c_void, *mut MemoryInfoExV1) -> NvStatus>,
    client_power_topology_get_status: Option<
        unsafe extern "C" fn(*const core::ffi::c_void, *mut PowerTopologyStatusV1) -> NvStatus,
    >,
}

// SAFETY: NVAPI entry points are thread-safe per NVIDIA docs; the handle
// table is process-global. The struct is only ever moved as a whole.
unsafe impl Send for NvApi {}
unsafe impl Sync for NvApi {}

impl NvApi {
    fn load() -> Result<Self, PlatformError> {
        unsafe {
            let lib = LoadLibraryW(PCWSTR(
                "nvapi64.dll\0"
                    .encode_utf16()
                    .collect::<Vec<u16>>()
                    .as_ptr(),
            ))
            .map_err(|e| {
                PlatformError::NotAvailable(if e.code().is_ok() {
                    "nvapi64.dll load failed"
                } else {
                    "nvapi64.dll not found (no NVIDIA driver?)"
                })
            })?;

            let query: QueryInterface = {
                let p = GetProcAddress(lib, windows::core::s!("nvapi_QueryInterface"))
                    .ok_or(PlatformError::NotAvailable("nvapi_QueryInterface missing"))?;
                std::mem::transmute(p)
            };

            macro_rules! q {
                ($id:expr) => {{
                    let p = query($id);
                    if p.is_null() {
                        None
                    } else {
                        Some(std::mem::transmute(p))
                    }
                }};
            }

            let api = Self {
                _lib: lib,
                initialize: q!(ID_INITIALIZE),
                enum_gpus: q!(ID_ENUM_PHYSICAL_GPUS),
                get_full_name: q!(ID_GET_FULL_NAME),
                get_thermal_settings: q!(ID_GET_THERMAL_SETTINGS),
                get_clock_frequencies: q!(ID_GET_ALL_CLOCK_FREQUENCIES),
                get_dynamic_pstates: q!(ID_GET_DYNAMIC_PSTATES_INFO_EX),
                get_current_pstate: q!(ID_GET_CURRENT_PSTATE),
                get_perf_decrease_info: q!(ID_GET_PERF_DECREASE_INFO),
                get_memory_info_ex: q!(ID_GET_MEMORY_INFO_EX),
                client_power_topology_get_status: q!(ID_CLIENT_POWER_TOPOLOGY_GET_STATUS),
            };

            let init = api
                .initialize
                .ok_or(PlatformError::NotAvailable("NvAPI_Initialize missing"))?;
            let rc = init();
            if rc != NVAPI_GENERIC_OK {
                return Err(PlatformError::Driver(format!("NvAPI_Initialize rc={rc}")));
            }
            Ok(api)
        }
    }
}

/// NVAPI GPU handle (opaque).
pub(crate) struct NvidiaGpu {
    api: &'static NvApi,
    handle: *mut core::ffi::c_void,
    name: String,
    /// NVML power source (primary; verified working on 8BAB — see Nvml
    /// docs). None when NVML is absent/unsupported.
    nvml: Option<NvmlDevice>,
    degraded: Vec<String>,
}

// SAFETY: NVAPI entry points are documented thread-safe; the opaque GPU
// handle is process-global state, not a thread-bound resource. The M1
// coordinator keeps the collector on one thread regardless.
unsafe impl Send for NvidiaGpu {}

static NVAPI: OnceLock<Result<NvApi, String>> = OnceLock::new();

fn api() -> Result<&'static NvApi, PlatformError> {
    NVAPI
        .get_or_init(|| NvApi::load().map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| PlatformError::Driver(format!("nvapi init: {e}")))
}

impl NvidiaGpu {
    /// Enumerate physical GPUs; take the first NVIDIA dGPU (single-GPU
    /// target machine — anything else is a profile violation, noted).
    pub(crate) fn open() -> Result<Self, PlatformError> {
        let api = api()?;
        unsafe {
            let enum_fn = api
                .enum_gpus
                .ok_or(PlatformError::NotAvailable("EnumPhysicalGPUs missing"))?;
            let mut handles: [*mut core::ffi::c_void; MAX_GPUS] = [std::ptr::null_mut(); MAX_GPUS];
            let mut count: u32 = 0;
            let rc = enum_fn(handles.as_mut_ptr(), &mut count);
            if rc != NVAPI_GENERIC_OK {
                return Err(PlatformError::Driver(format!("EnumPhysicalGPUs rc={rc}")));
            }
            let handle = handles
                .iter()
                .take(count as usize)
                .copied()
                .find(|h| !h.is_null())
                .ok_or(PlatformError::NotAvailable("no physical NVIDIA GPU"))?;

            let mut name = String::new();
            if let Some(get_name) = api.get_full_name {
                let mut buf = [0u8; SHORT_STRING];
                if get_name(handle, buf.as_mut_ptr()) == NVAPI_GENERIC_OK {
                    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                    name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
                }
            }

            Ok(Self {
                api,
                handle,
                name,
                nvml: NvmlDevice::open(),
                degraded: Vec::new(),
            })
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    fn thermal(&self) -> Option<f64> {
        unsafe {
            let f = self.api.get_thermal_settings?;
            let mut s = ThermalSettingsV2::default();
            // sensor index 0, target 0 (= current)
            if f(self.handle, 0, &mut s) != NVAPI_GENERIC_OK {
                return None;
            }
            (s.count > 0).then_some(s.sensor[0].current_temp as f64)
        }
    }

    fn clocks(&self) -> (Option<f64>, Option<f64>) {
        unsafe {
            let Some(f) = self.api.get_clock_frequencies else {
                return (None, None);
            };
            // LHM probes struct versions 1..=3; try the modern one first.
            for ver in [2u32, 1, 3] {
                let mut c = ClockFrequenciesV2 {
                    version: nv_version!(ClockFrequenciesV2, ver),
                    ..Default::default()
                };
                if f(self.handle, &mut c) == NVAPI_GENERIC_OK {
                    let pick = |d: usize| {
                        (c.domain[d].is_present != 0)
                            .then_some(c.domain[d].frequency_khz as f64 / 1000.0)
                    };
                    return (pick(CLOCK_DOMAIN_GRAPHICS), pick(CLOCK_DOMAIN_MEMORY));
                }
            }
            (None, None)
        }
    }

    fn utilization(&self) -> Option<f64> {
        unsafe {
            let f = self.api.get_dynamic_pstates?;
            let mut p = DynamicPstatesInfoEx::default();
            if f(self.handle, &mut p) != NVAPI_GENERIC_OK {
                return None;
            }
            (p.utilization[UTIL_DOMAIN_GPU].is_present != 0)
                .then_some(p.utilization[UTIL_DOMAIN_GPU].percentage as f64)
        }
    }

    fn pstate(&self) -> Option<u32> {
        unsafe {
            let f = self.api.get_current_pstate?;
            let mut p: u32 = 0;
            (f(self.handle, &mut p) == NVAPI_GENERIC_OK).then_some(p)
        }
    }

    fn perf_decrease(&self) -> Option<u32> {
        unsafe {
            let f = self.api.get_perf_decrease_info?;
            let mut mask: u32 = 0;
            (f(self.handle, &mut mask) == NVAPI_GENERIC_OK).then_some(mask)
        }
    }

    /// GPU power in watts. Sole source on AD107 (see module docs). Takes
    /// the Gpu-domain topology entry (Board would double-count it). A 0 mW
    /// reading is published as 0.0 — the collector stamps it Estimated, so
    /// the idle-shakiness is carried by quality, not by hiding the metric.
    fn power_w(&self) -> Option<f64> {
        unsafe {
            let Some(f) = self.api.client_power_topology_get_status else {
                debug!("power: ClientPowerTopologyGetStatus not exposed by driver");
                return None;
            };
            let mut s = PowerTopologyStatusV1::default();
            let rc = f(self.handle, &mut s);
            if rc != NVAPI_GENERIC_OK {
                debug!(rc, "power: ClientPowerTopologyGetStatus call failed");
                return None;
            }
            let n = (s.num_entries as usize).min(s.entries.len());
            let gpu = s.entries[..n]
                .iter()
                .find(|e| e.domain == POWER_TOPOLOGY_DOMAIN_GPU);
            match gpu {
                Some(e) => Some(e.power_usage_mw as f64 / 1000.0),
                None => {
                    debug!(
                        num_entries = s.num_entries,
                        "power: no Gpu-domain topology entry"
                    );
                    None
                }
            }
        }
    }

    fn vram_used(&self) -> Option<u64> {
        unsafe {
            let f = self.api.get_memory_info_ex?;
            let mut m = MemoryInfoExV1::new();
            if f(self.handle, &mut m) != NVAPI_GENERIC_OK {
                return None;
            }
            let used = m
                .dedicated_video_memory
                .saturating_sub(m.current_available_dedicated_video_memory);
            (used > 0).then_some(used)
        }
    }
}

impl GpuTelemetry for NvidiaGpu {
    fn sample(&mut self) -> Result<GpuSample, PlatformError> {
        use phelper_domain::telemetry::GpuPowerSource;
        let (core_clock_mhz, mem_clock_mhz) = self.clocks();
        // Power: NVML primary (verified on 8BAB), topology fallback.
        let (power_w, power_source) = match self.nvml.as_ref().and_then(|n| n.power_w()) {
            Some(w) => (Some(w), Some(GpuPowerSource::Nvml)),
            None => match self.power_w() {
                Some(w) => (Some(w), Some(GpuPowerSource::NvapiTopology)),
                None => (None, None),
            },
        };
        let mut s = GpuSample {
            temp_c: self.thermal(),
            power_w,
            power_source,
            util_percent: self.utilization(),
            core_clock_mhz,
            mem_clock_mhz,
            pstate: self.pstate(),
            throttle_reasons: self.perf_decrease(),
            vram_used_bytes: self.vram_used(),
            // TGP cap readback (M2 write-verification chain). Quasi-static;
            // the collector re-pushes on the TjMax pattern.
            power_limit_w: self.nvml.as_ref().and_then(|n| n.power_limit_w()),
        };
        if s.temp_c.is_none() && s.util_percent.is_none() {
            self.degraded.push("thermal+util both unreadable".into());
            return Err(PlatformError::Driver("NVAPI sample empty".into()));
        }
        if s.power_w.is_none() {
            // Known on this machine when the dGPU is in deep sleep AND the
            // only working source (NVML) is momentarily unreadable — the
            // collector marks the metric, never fails the provider.
            s.power_w = None;
        }
        Ok(s)
    }

    fn status(&self) -> ProviderStatus {
        if self.degraded.is_empty() {
            ProviderStatus::Ok
        } else {
            ProviderStatus::Degraded(self.degraded.join("; "))
        }
    }
}
