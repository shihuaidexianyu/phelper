//! Metric registry: every canonical metric with unit, owner (§12 single
//! authoritative source), and cadence. Collectors never invent ids; the
//! UI/diagnostics render from this table.

use phelper_domain::telemetry::{MetricId, MetricSource, ids};
use std::time::Duration;

pub struct MetricMeta {
    pub id: MetricId,
    pub unit: &'static str,
    pub owner: MetricSource,
    pub cadence: Duration,
    pub note: &'static str,
}

macro_rules! m {
    ($id:expr, $unit:expr, $owner:expr, $ms:expr, $note:expr) => {
        MetricMeta {
            id: $id,
            unit: $unit,
            owner: $owner,
            cadence: Duration::from_millis($ms),
            note: $note,
        }
    };
}

/// The full registry. Order is display order.
pub(crate) const REGISTRY: &[MetricMeta] = &[
    m!(
        ids::CPU_PKG_TEMP_C,
        "°C",
        MetricSource::PawnIoMsr,
        250,
        "IA32_PACKAGE_THERM_STATUS via PawnIO"
    ),
    m!(
        ids::CPU_TJ_MAX_C,
        "°C",
        MetricSource::PawnIoMsr,
        60_000,
        "MSR_TEMPERATURE_TARGET, quasi-static"
    ),
    m!(
        ids::CPU_PKG_POWER_W,
        "W",
        MetricSource::PawnIoMsr,
        250,
        "RAPL 0x611 delta × unit(0x606[12:8]) / dt"
    ),
    m!(
        ids::CPU_EFFECTIVE_CLOCK_MHZ,
        "MHz",
        MetricSource::PawnIoMsr,
        250,
        "tsc × ΔAPERF/ΔMPERF"
    ),
    m!(
        ids::CPU_THERMAL_STATUS_RAW,
        "raw",
        MetricSource::PawnIoMsr,
        1000,
        "0x1B1 bits (prochot/thresholds)"
    ),
    m!(
        ids::CPU_PL1_W,
        "W",
        MetricSource::PawnIoMsr,
        250,
        "MSR 0x610[14:0] × power unit — PL1 readback (0x29 runbook step 2)"
    ),
    m!(
        ids::CPU_PL2_W,
        "W",
        MetricSource::PawnIoMsr,
        250,
        "MSR 0x610[46:32] × power unit — PL2 readback"
    ),
    m!(
        ids::CPU_PL4_W,
        "W",
        MetricSource::PawnIoMsr,
        250,
        "MCHBAR 0x59B0[14:0] × power unit — PL4 readback (0x29 byte2 verification; absent without IntelMCHBAR)"
    ),
    m!(
        ids::CPU_POWER_LIMIT_RAW,
        "raw",
        MetricSource::PawnIoMsr,
        250,
        "MSR 0x610 full register (enable bits + limits)"
    ),
    m!(
        ids::CPU_EPP_AC,
        "%",
        MetricSource::WindowsPpm,
        5000,
        "PERFEPP AC readback — current value of the M2 EPP write knob"
    ),
    m!(
        ids::CPU_EPP_DC,
        "%",
        MetricSource::WindowsPpm,
        5000,
        "PERFEPP DC readback"
    ),
    m!(
        ids::CPU_EPP1_AC,
        "%",
        MetricSource::WindowsPpm,
        5000,
        "PERFEPP1 (class-1 / E-core) AC readback"
    ),
    m!(
        ids::CPU_EPP1_DC,
        "%",
        MetricSource::WindowsPpm,
        5000,
        "PERFEPP1 (class-1 / E-core) DC readback"
    ),
    m!(
        ids::CPU_MAX_FREQ_AC,
        "MHz",
        MetricSource::WindowsPpm,
        5000,
        "PROCFREQMAX AC readback — 0 means unlimited"
    ),
    m!(
        ids::CPU_MAX_FREQ_DC,
        "MHz",
        MetricSource::WindowsPpm,
        5000,
        "PROCFREQMAX DC readback — 0 means unlimited"
    ),
    m!(
        ids::CPU_MIN_PERF_AC,
        "%",
        MetricSource::WindowsPpm,
        5000,
        "PROCTHROTTLEMIN AC readback"
    ),
    m!(
        ids::CPU_MIN_PERF_DC,
        "%",
        MetricSource::WindowsPpm,
        5000,
        "PROCTHROTTLEMIN DC readback"
    ),
    m!(
        ids::CPU_MAX_PERF_AC,
        "%",
        MetricSource::WindowsPpm,
        5000,
        "PROCTHROTTLEMAX AC readback"
    ),
    m!(
        ids::CPU_MAX_PERF_DC,
        "%",
        MetricSource::WindowsPpm,
        5000,
        "PROCTHROTTLEMAX DC readback"
    ),
    m!(
        ids::CPU_BOOST_AC,
        "mode",
        MetricSource::WindowsPpm,
        5000,
        "PERFBOOSTMODE AC wire value 0..=6"
    ),
    m!(
        ids::CPU_BOOST_DC,
        "mode",
        MetricSource::WindowsPpm,
        5000,
        "PERFBOOSTMODE DC wire value 0..=6"
    ),
    m!(
        ids::CPU_UTIL_PERCENT,
        "%",
        MetricSource::WindowsPdh,
        1000,
        "PDH Processor Information"
    ),
    m!(
        ids::MEM_USED_BYTES,
        "B",
        MetricSource::WindowsPdh,
        1000,
        "GlobalMemoryStatusEx"
    ),
    m!(
        ids::MEM_TOTAL_BYTES,
        "B",
        MetricSource::WindowsPdh,
        60_000,
        "GlobalMemoryStatusEx"
    ),
    m!(
        ids::DISK_READ_BPS,
        "B/s",
        MetricSource::WindowsPdh,
        1000,
        "PDH PhysicalDisk _Total"
    ),
    m!(
        ids::DISK_WRITE_BPS,
        "B/s",
        MetricSource::WindowsPdh,
        1000,
        "PDH PhysicalDisk _Total"
    ),
    m!(
        ids::NET_RX_BPS,
        "B/s",
        MetricSource::WindowsPdh,
        1000,
        "PDH Network Interface sum"
    ),
    m!(
        ids::NET_TX_BPS,
        "B/s",
        MetricSource::WindowsPdh,
        1000,
        "PDH Network Interface sum"
    ),
    m!(
        ids::GPU_TEMP_C,
        "°C",
        MetricSource::NvapiPublic,
        500,
        "NVAPI GetThermalSettings"
    ),
    m!(
        ids::GPU_POWER_W,
        "W",
        MetricSource::NvmlPublic,
        500,
        "NVML nvmlDeviceGetPowerUsage (verified on 8BAB/driver 581.x — R5 \
         superseded). Fallback: ClientPowerTopology (0 entries here)."
    ),
    m!(
        ids::GPU_UTIL_PERCENT,
        "%",
        MetricSource::NvapiPublic,
        500,
        "NVAPI DynamicPstatesInfoEx"
    ),
    m!(
        ids::GPU_CORE_CLOCK_MHZ,
        "MHz",
        MetricSource::NvapiPublic,
        500,
        "NVAPI AllClockFrequencies graphics domain"
    ),
    m!(
        ids::GPU_MEM_CLOCK_MHZ,
        "MHz",
        MetricSource::NvapiPublic,
        500,
        "NVAPI AllClockFrequencies memory domain"
    ),
    m!(
        ids::GPU_PSTATE,
        "pstate",
        MetricSource::NvapiPublic,
        1000,
        "NVAPI GetCurrentPstate"
    ),
    m!(
        ids::GPU_THROTTLE_REASONS_RAW,
        "raw",
        MetricSource::NvapiPublic,
        1000,
        "NVAPI GetPerfDecreaseInfo mask"
    ),
    m!(
        ids::GPU_VRAM_USED_BYTES,
        "B",
        MetricSource::NvapiPublic,
        1000,
        "NVAPI GetMemoryInfoEx (GPU handle)"
    ),
    m!(
        ids::GPU_POWER_LIMIT_W,
        "W",
        MetricSource::NvmlPublic,
        500,
        "NVML nvmlDeviceGetEnforcedPowerLimit (TGP cap, quasi-static — \
         re-pushed ~60 s; GetPowerManagementLimit is NOT_SUPPORTED on AD107)"
    ),
    m!(
        ids::FAN_LEFT_RPM,
        "RPM",
        MetricSource::HpWmi,
        1000,
        "0x2D channel 0 × 100 (8BAB left fan). §38: never faster than 1 Hz"
    ),
    m!(
        ids::FAN_RIGHT_RPM,
        "RPM",
        MetricSource::HpWmi,
        1000,
        "0x2D channel 1 × 100 (8BAB right fan). §38: never faster than 1 Hz"
    ),
    m!(
        ids::POWER_AC_ONLINE,
        "bool",
        MetricSource::WindowsPower,
        5000,
        "GetSystemPowerStatus"
    ),
    m!(
        ids::POWER_BATTERY_PERCENT,
        "%",
        MetricSource::WindowsPower,
        5000,
        "GetSystemPowerStatus"
    ),
];

pub fn meta(id: MetricId) -> Option<&'static MetricMeta> {
    REGISTRY.iter().find(|m| m.id == id)
}

/// The whole table, in display order (CLI/GPUI render from this).
pub fn all() -> &'static [MetricMeta] {
    REGISTRY
}
