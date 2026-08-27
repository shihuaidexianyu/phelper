//! Collectors: typed provider sample → canonical MetricSamples.
//!
//! Failure model (D3): a collector NEVER throws across this boundary. A
//! failed read is a skipped metric + a ProviderStatus downgrade; the ring
//! keeps the last good sample and its honest timestamp.

use std::sync::Arc;
use std::time::{Duration, Instant};

use phelper_domain::ports::{GpuTelemetry, HpPlatform, PowerStatus, SystemCounters};
use phelper_domain::telemetry::{MetricQuality, MetricSample, MetricSource, ProviderStatus, ids};
use tracing::{debug, warn};

use super::registry;
use crate::platform::hp_wmi::actor::HpHandle;
use crate::platform::presentmon::{
    FrameEvent, FrameWindow, PresentMonSource, is_application_frame,
};

/// One scheduled data source. Object-safe; the coordinator boxes these.
pub(crate) trait Collector: Send {
    fn name(&self) -> &'static str;
    fn cadence(&self) -> Duration;
    /// One collection round. Samples for metrics whose read failed are
    /// simply absent.
    fn collect(&mut self) -> Vec<MetricSample>;
    fn status(&self) -> ProviderStatus;
}

fn fresh(
    id: phelper_domain::telemetry::MetricId,
    value: phelper_domain::telemetry::MetricValue,
    source: MetricSource,
) -> MetricSample {
    MetricSample::fresh(id, value, source)
}

// ---------------------------------------------------------------------------
// CPU silicon (PawnIO MSR)
// ---------------------------------------------------------------------------

#[cfg(feature = "pawnio")]
pub(crate) struct PawnioCollector {
    io: crate::platform::pawnio::PawnIo,
    /// Second signed module (IntelMCHBAR) for the PL4 readback. Optional:
    /// if the module is missing or the CPU is not in its allow-list, the
    /// cpu.pl4_w metric is simply absent — the MSR provider status is NOT
    /// affected (same rule as EPP1: a supplementary channel never flaps
    /// the primary provider).
    mchbar: Option<crate::platform::pawnio::PawnIo>,
    tsc_mhz: u32,
    tj_max_raw: u64,
    energy_unit_j: f64,
    /// RAPL power unit (0x606[3:0]) for the 0x610 limit registers.
    power_unit_w: f64,
    prev_energy: Option<(u32, Instant)>,
    /// (mperf, aperf) of the previous tick.
    prev_perf: Option<(u64, u64)>,
    /// TjMax is quasi-static: pushed on the first tick and re-pushed every
    /// TJMAX_REPUSH_TICKS (~60 s at 250 ms) so late subscribers see it.
    ticks: u32,
    status: ProviderStatus,
}

#[cfg(feature = "pawnio")]
impl PawnioCollector {
    pub(crate) fn open(tsc_mhz: u32) -> Result<Self, phelper_domain::error::PlatformError> {
        use crate::platform::pawnio::{self, PawnIo};
        let image = pawnio::intelmsr_image()?;
        let io = PawnIo::load_module(&image)?;
        let tj_max_raw = pawnio::read_msr(&io, pawnio::MSR_TEMPERATURE_TARGET)?;
        let unit_raw = pawnio::read_msr(&io, pawnio::MSR_RAPL_POWER_UNIT)?;
        // Best-effort second module for cpu.pl4_w (MCHBAR 0x59B0, M4.1).
        // Dual-module coexistence verified on-device 2026-08-26 (separate
        // handles polling simultaneously without interference).
        let mchbar = match pawnio::mchbar_image().map(|img| PawnIo::load_module(&img)) {
            Ok(Ok(io)) => Some(io),
            Ok(Err(e)) => {
                warn!(%e, "IntelMCHBAR module load failed — cpu.pl4_w unavailable");
                None
            }
            Err(e) => {
                warn!(%e, "IntelMCHBAR module image missing — cpu.pl4_w unavailable");
                None
            }
        };
        Ok(Self {
            io,
            mchbar,
            tsc_mhz,
            tj_max_raw,
            energy_unit_j: pawnio::energy_unit_j(unit_raw),
            power_unit_w: pawnio::power_unit_w(unit_raw),
            prev_energy: None,
            prev_perf: None,
            ticks: 0,
            status: ProviderStatus::Ok,
        })
    }
}

/// Re-push TjMax every ~60 s (at 250 ms cadence).
#[cfg(feature = "pawnio")]
const TJMAX_REPUSH_TICKS: u32 = 240;

#[cfg(feature = "pawnio")]
impl Collector for PawnioCollector {
    fn name(&self) -> &'static str {
        "pawnio/cpu-silicon"
    }

    fn cadence(&self) -> Duration {
        registry::meta(ids::CPU_PKG_TEMP_C)
            .expect("registry entry")
            .cadence
    }

    fn collect(&mut self) -> Vec<MetricSample> {
        use crate::platform::pawnio as p;
        let src = MetricSource::PawnIoMsr;
        let mut out = Vec::with_capacity(12);
        let now = Instant::now();
        self.ticks = self.ticks.wrapping_add(1);

        let read = |msr| p::read_msr(&self.io, msr);

        // Package therm status (0x1B1 primary, 0x19C fallback).
        match read(p::MSR_PKG_THERM_STATUS).or_else(|_| read(p::MSR_THERM_STATUS)) {
            Ok(raw) => {
                if let Some(t) = p::pkg_temp_c(self.tj_max_raw, raw) {
                    out.push(fresh(ids::CPU_PKG_TEMP_C, (t as f64).into(), src));
                }
                if raw != 0 {
                    out.push(fresh(ids::CPU_THERMAL_STATUS_RAW, raw.into(), src));
                }
            }
            Err(e) => {
                debug!(%e, "pkg therm read failed");
                self.status = ProviderStatus::Degraded(format!("therm read: {e}"));
            }
        }

        // RAPL package power (delta of 0x611 over wall time).
        match read(p::MSR_PKG_ENERGY_STATUS) {
            Ok(raw) => {
                let energy = raw as u32;
                if let Some((prev, prev_at)) = self.prev_energy {
                    let dt = now.duration_since(prev_at).as_secs_f64();
                    if dt > 0.0 {
                        let w = p::delta_energy(prev, energy) as f64 * self.energy_unit_j / dt;
                        out.push(fresh(ids::CPU_PKG_POWER_W, w.into(), src));
                    }
                }
                self.prev_energy = Some((energy, now));
            }
            Err(e) => {
                debug!(%e, "rapl read failed");
                self.status = ProviderStatus::Degraded(format!("rapl read: {e}"));
            }
        }

        // Effective clock (ΔAPERF/ΔMPERF; migration outliers discarded).
        match (read(p::MSR_MPERF), read(p::MSR_APERF)) {
            (Ok(mperf), Ok(aperf)) => {
                if let Some(prev) = self.prev_perf
                    && let Some(mhz) = p::effective_clock_mhz(self.tsc_mhz, prev, (mperf, aperf))
                {
                    out.push(fresh(ids::CPU_EFFECTIVE_CLOCK_MHZ, mhz.into(), src));
                }
                self.prev_perf = Some((mperf, aperf));
            }
            _ => {
                debug!("mperf/aperf read failed");
                self.status = ProviderStatus::Degraded("mperf/aperf read failed".into());
            }
        }

        if self.ticks == 1 || self.ticks.is_multiple_of(TJMAX_REPUSH_TICKS) {
            let tjmax = ((self.tj_max_raw >> 16) & 0xFF) as f64;
            if tjmax > 0.0 {
                out.push(fresh(ids::CPU_TJ_MAX_C, tjmax.into(), src));
            }
        }

        // 0x610 package power limits (PL1/PL2 readback — also the 0x29
        // verification runbook's step-2 evidence).
        match read(p::MSR_PKG_POWER_LIMIT) {
            Ok(raw) => {
                let (pl1, pl2) = p::pkg_power_limits_w(raw, self.power_unit_w);
                out.push(fresh(ids::CPU_PL1_W, pl1.into(), src));
                out.push(fresh(ids::CPU_PL2_W, pl2.into(), src));
                out.push(fresh(ids::CPU_POWER_LIMIT_RAW, raw.into(), src));
            }
            Err(e) => {
                debug!(%e, "0x610 power-limit read failed");
                self.status = ProviderStatus::Degraded(format!("0x610 read: {e}"));
            }
        }

        // PL4 readback via MCHBAR 0x59B0 (the AR-10 verification source for
        // 0x29 byte2 writes). Supplementary channel: a read failure here
        // alone never flaps the provider status (same rule as EPP1).
        if let Some(mchbar) = &self.mchbar {
            match p::mchbar_read_dword(mchbar, p::MCHBAR_PL4_OFFSET) {
                Ok(raw) => {
                    let pl4 = p::power_limit_field_w(raw, self.power_unit_w);
                    out.push(fresh(ids::CPU_PL4_W, pl4.into(), src));
                }
                Err(e) => {
                    debug!(%e, "MCHBAR PL4 read failed");
                }
            }
        }

        if !matches!(self.status, ProviderStatus::Ok) && !out.is_empty() {
            // A later read succeeded after an earlier degradation.
            self.status = ProviderStatus::Ok;
        }
        out
    }

    fn status(&self) -> ProviderStatus {
        self.status.clone()
    }
}

// ---------------------------------------------------------------------------
// GPU (NVAPI)
// ---------------------------------------------------------------------------

#[cfg(feature = "nvidia")]
pub(crate) struct NvapiCollector {
    gpu: crate::platform::nvidia::NvidiaGpu,
    /// power_limit_w is quasi-static: pushed on the first tick and
    /// re-pushed every POWER_LIMIT_REPUSH_TICKS (~60 s at 500 ms).
    ticks: u32,
}

/// Re-push the GPU power limit every ~60 s (at 500 ms cadence).
#[cfg(feature = "nvidia")]
const POWER_LIMIT_REPUSH_TICKS: u32 = 120;

#[cfg(feature = "nvidia")]
impl NvapiCollector {
    pub(crate) fn open() -> Result<Self, phelper_domain::error::PlatformError> {
        Ok(Self {
            gpu: crate::platform::nvidia::NvidiaGpu::open()?,
            ticks: 0,
        })
    }
}

#[cfg(feature = "nvidia")]
impl Collector for NvapiCollector {
    fn name(&self) -> &'static str {
        "nvapi/gpu"
    }

    fn cadence(&self) -> Duration {
        registry::meta(ids::GPU_TEMP_C)
            .expect("registry entry")
            .cadence
    }

    fn collect(&mut self) -> Vec<MetricSample> {
        use phelper_domain::telemetry::MetricValue as V;
        let mut out = Vec::with_capacity(9);
        self.ticks = self.ticks.wrapping_add(1);
        let Ok(s) = self.gpu.sample() else {
            return out; // provider status carries the failure
        };
        let pub_src = MetricSource::NvapiPublic;
        if let Some(v) = s.temp_c {
            out.push(fresh(ids::GPU_TEMP_C, v.into(), pub_src));
        }
        if let Some(v) = s.power_w {
            use phelper_domain::telemetry::GpuPowerSource;
            // NVML readings are continuous and plausible on this machine →
            // Fresh. The topology fallback is the shaky one → Estimated.
            let (src, qual) = match s.power_source {
                Some(GpuPowerSource::Nvml) => (MetricSource::NvmlPublic, MetricQuality::Fresh),
                _ => (
                    MetricSource::NvapiClientPowerTopology,
                    MetricQuality::Estimated,
                ),
            };
            out.push(fresh(ids::GPU_POWER_W, v.into(), src).with_quality(qual));
        }
        if let Some(v) = s.util_percent {
            out.push(fresh(ids::GPU_UTIL_PERCENT, v.into(), pub_src));
        }
        if let Some(v) = s.core_clock_mhz {
            out.push(fresh(ids::GPU_CORE_CLOCK_MHZ, v.into(), pub_src));
        }
        if let Some(v) = s.mem_clock_mhz {
            out.push(fresh(ids::GPU_MEM_CLOCK_MHZ, v.into(), pub_src));
        }
        if let Some(v) = s.pstate {
            out.push(fresh(ids::GPU_PSTATE, V::U64(u64::from(v)), pub_src));
        }
        if let Some(v) = s.throttle_reasons {
            out.push(fresh(
                ids::GPU_THROTTLE_REASONS_RAW,
                V::U64(u64::from(v)),
                pub_src,
            ));
        }
        if let Some(v) = s.vram_used_bytes {
            out.push(fresh(ids::GPU_VRAM_USED_BYTES, V::U64(v), pub_src));
        }
        if let Some(v) = s.power_limit_w
            && (self.ticks == 1 || self.ticks.is_multiple_of(POWER_LIMIT_REPUSH_TICKS))
        {
            out.push(fresh(
                ids::GPU_POWER_LIMIT_W,
                v.into(),
                MetricSource::NvmlPublic,
            ));
        }
        out
    }

    fn status(&self) -> ProviderStatus {
        GpuTelemetry::status(&self.gpu)
    }
}

// ---------------------------------------------------------------------------
// Windows OS counters (PDH + memory status)
// ---------------------------------------------------------------------------

pub(crate) struct PdhCollector {
    pdh: crate::platform::windows_pdh::WindowsPdh,
}

impl PdhCollector {
    pub(crate) fn open() -> Result<Self, phelper_domain::error::PlatformError> {
        Ok(Self {
            pdh: crate::platform::windows_pdh::WindowsPdh::new()?,
        })
    }
}

impl Collector for PdhCollector {
    fn name(&self) -> &'static str {
        "windows/pdh"
    }

    fn cadence(&self) -> Duration {
        registry::meta(ids::CPU_UTIL_PERCENT)
            .expect("registry entry")
            .cadence
    }

    fn collect(&mut self) -> Vec<MetricSample> {
        use phelper_domain::telemetry::MetricValue as V;
        let mut out = Vec::with_capacity(7);
        let Ok(s) = self.pdh.sample() else {
            return out;
        };
        let src = MetricSource::WindowsPdh;
        if let Some(v) = s.cpu_util_percent {
            out.push(fresh(ids::CPU_UTIL_PERCENT, v.into(), src));
        }
        if let Some(v) = s.mem_used_bytes {
            out.push(fresh(ids::MEM_USED_BYTES, V::U64(v), src));
        }
        if let Some(v) = s.mem_total_bytes {
            out.push(fresh(ids::MEM_TOTAL_BYTES, V::U64(v), src));
        }
        if let Some(v) = s.disk_read_bps {
            out.push(fresh(ids::DISK_READ_BPS, v.into(), src));
        }
        if let Some(v) = s.disk_write_bps {
            out.push(fresh(ids::DISK_WRITE_BPS, v.into(), src));
        }
        if let Some(v) = s.net_rx_bps {
            out.push(fresh(ids::NET_RX_BPS, v.into(), src));
        }
        if let Some(v) = s.net_tx_bps {
            out.push(fresh(ids::NET_TX_BPS, v.into(), src));
        }
        out
    }

    fn status(&self) -> ProviderStatus {
        self.pdh.status()
    }
}

// ---------------------------------------------------------------------------
// PresentMon frame telemetry (optional, read-only)
// ---------------------------------------------------------------------------

pub(crate) struct PresentMonCollector {
    source: PresentMonSource,
    frame_window: FrameWindow,
    status: ProviderStatus,
}

impl PresentMonCollector {
    pub(crate) fn open() -> Result<Self, phelper_domain::error::PlatformError> {
        Ok(Self {
            source: PresentMonSource::open()?,
            frame_window: FrameWindow::default(),
            status: ProviderStatus::Ok,
        })
    }
}

fn average_frame_field(
    events: &[FrameEvent],
    field: impl Fn(&FrameEvent) -> Option<f64>,
) -> Option<f64> {
    let mut count = 0usize;
    let mut sum = 0.0;
    for event in events
        .iter()
        .filter(|event| is_application_frame(event.frame_type))
    {
        let Some(value) = field(event) else { continue };
        if value.is_finite() && value >= 0.0 {
            count += 1;
            sum += value;
        }
    }
    (count > 0).then_some(sum / count as f64)
}

impl Collector for PresentMonCollector {
    fn name(&self) -> &'static str {
        "presentmon/frames"
    }

    fn cadence(&self) -> Duration {
        registry::meta(ids::FRAME_DISPLAYED_FPS)
            .expect("registry entry")
            .cadence
    }

    fn collect(&mut self) -> Vec<MetricSample> {
        let batch = match self.source.poll() {
            Ok(batch) => batch,
            Err(e) => {
                debug!(%e, "PresentMon frame query failed");
                self.status = ProviderStatus::Degraded(e.to_string());
                return Vec::new();
            }
        };
        self.status = ProviderStatus::Ok;
        let now = Instant::now();
        self.frame_window.push_batch(now, &batch.events);

        let app_frames = batch
            .events
            .iter()
            .filter(|event| is_application_frame(event.frame_type))
            .count();
        let mut out = Vec::with_capacity(6);
        if app_frames > 0 {
            let elapsed_s = batch.elapsed.as_secs_f64();
            if elapsed_s > 0.0 {
                out.push(fresh(
                    ids::FRAME_DISPLAYED_FPS,
                    (app_frames as f64 / elapsed_s).into(),
                    MetricSource::PresentMon,
                ));
            }
        }
        if let Some(value) =
            average_frame_field(&batch.events, |event| event.displayed_frame_time_ms)
        {
            out.push(fresh(
                ids::FRAME_TIME_MS,
                value.into(),
                MetricSource::PresentMon,
            ));
        }
        if let Some(value) = average_frame_field(&batch.events, |event| event.cpu_busy_ms) {
            out.push(fresh(
                ids::FRAME_CPU_BUSY_MS,
                value.into(),
                MetricSource::PresentMon,
            ));
        }
        if let Some(value) = average_frame_field(&batch.events, |event| event.gpu_time_ms) {
            out.push(fresh(
                ids::FRAME_GPU_TIME_MS,
                value.into(),
                MetricSource::PresentMon,
            ));
        }
        if let Some(value) = average_frame_field(&batch.events, |event| event.display_latency_ms) {
            out.push(fresh(
                ids::FRAME_DISPLAY_LATENCY_MS,
                value.into(),
                MetricSource::PresentMon,
            ));
        }
        if let Some(value) = self.frame_window.one_percent_low_fps() {
            out.push(
                fresh(
                    ids::FRAME_ONE_PERCENT_LOW_FPS,
                    value.into(),
                    MetricSource::PresentMon,
                )
                .with_quality(MetricQuality::Estimated),
            );
        }
        out
    }

    fn status(&self) -> ProviderStatus {
        self.status.clone()
    }
}

// ---------------------------------------------------------------------------
// AC / battery
// ---------------------------------------------------------------------------

pub(crate) struct BatteryCollector {
    power: crate::platform::windows_power::WindowsPower,
}

impl BatteryCollector {
    pub(crate) fn new() -> Self {
        Self {
            power: crate::platform::windows_power::WindowsPower::new(),
        }
    }
}

impl Collector for BatteryCollector {
    fn name(&self) -> &'static str {
        "windows/power"
    }

    fn cadence(&self) -> Duration {
        registry::meta(ids::POWER_AC_ONLINE)
            .expect("registry entry")
            .cadence
    }

    fn collect(&mut self) -> Vec<MetricSample> {
        use phelper_domain::telemetry::MetricValue as V;
        let mut out = Vec::with_capacity(2);
        let Ok(s) = self.power.sample() else {
            return out;
        };
        let src = MetricSource::WindowsPower;
        if let Some(v) = s.ac_online {
            out.push(fresh(ids::POWER_AC_ONLINE, V::Bool(v), src));
        }
        if let Some(v) = s.battery_percent {
            out.push(fresh(ids::POWER_BATTERY_PERCENT, v.into(), src));
        }
        out
    }

    fn status(&self) -> ProviderStatus {
        self.power.status()
    }
}

// ---------------------------------------------------------------------------
// Windows PPM readbacks (PowrProf; unconditional — reads work unelevated)
// ---------------------------------------------------------------------------

/// EPP + frequency-ceiling readback. These are the CURRENT values of the
/// M2 write knobs — the telemetry half of the write-verification chain
/// (AR-10: after a PPM write the metric must move within one cadence).
pub(crate) struct PpmCollector {
    status: ProviderStatus,
}

impl PpmCollector {
    pub(crate) fn new() -> Self {
        Self {
            status: ProviderStatus::Ok,
        }
    }
}

impl Collector for PpmCollector {
    fn name(&self) -> &'static str {
        "windows/ppm"
    }

    fn cadence(&self) -> Duration {
        registry::meta(ids::CPU_EPP_AC)
            .expect("registry entry")
            .cadence
    }

    fn collect(&mut self) -> Vec<MetricSample> {
        use phelper_domain::telemetry::MetricValue as V;
        let mut out = Vec::with_capacity(4);
        match crate::platform::windows_ppm::read_epp() {
            Ok(epp) => {
                out.push(fresh(
                    ids::CPU_EPP_AC,
                    V::U64(u64::from(epp.ac)),
                    MetricSource::WindowsPpm,
                ));
                out.push(fresh(
                    ids::CPU_EPP_DC,
                    V::U64(u64::from(epp.dc)),
                    MetricSource::WindowsPpm,
                ));
                self.status = ProviderStatus::Ok;
            }
            Err(e) => {
                debug!(%e, "EPP read failed");
                self.status = ProviderStatus::Degraded(format!("EPP read: {e}"));
            }
        }
        match crate::platform::windows_ppm::read_epp1() {
            Ok(epp1) => {
                out.push(fresh(
                    ids::CPU_EPP1_AC,
                    V::U64(u64::from(epp1.ac)),
                    MetricSource::WindowsPpm,
                ));
                out.push(fresh(
                    ids::CPU_EPP1_DC,
                    V::U64(u64::from(epp1.dc)),
                    MetricSource::WindowsPpm,
                ));
            }
            Err(e) => {
                // Class-1 EPP is absent on homogeneous CPUs; a failure here
                // alone must not flap the provider status.
                debug!(%e, "EPP1 read failed");
            }
        }
        out
    }

    fn status(&self) -> ProviderStatus {
        self.status.clone()
    }
}

// ---------------------------------------------------------------------------
// Fans (HP WMI 0x2D via the actor)
// ---------------------------------------------------------------------------

/// Firmware floor between fan reads (architecture.md §38: HP firmware is
/// never polled faster than ~1 Hz). The coordinator's own cadence is 1 s;
/// this guard is the hard backstop (RefreshNow bursts, future control path).
const FAN_MIN_INTERVAL: Duration = Duration::from_millis(900);

pub(crate) struct HpFanCollector {
    hp: Arc<HpHandle>,
    last: Option<(Instant, phelper_domain::policy::FanLevels)>,
    status: ProviderStatus,
}

impl HpFanCollector {
    pub(crate) fn new(hp: Arc<HpHandle>) -> Self {
        Self {
            hp,
            last: None,
            status: ProviderStatus::Ok,
        }
    }
}

impl Collector for HpFanCollector {
    fn name(&self) -> &'static str {
        "hp-wmi/fans"
    }

    fn cadence(&self) -> Duration {
        registry::meta(ids::FAN_CPU_RPM)
            .expect("registry entry")
            .cadence
    }

    fn collect(&mut self) -> Vec<MetricSample> {
        let throttled = self
            .last
            .is_some_and(|(at, _)| at.elapsed() < FAN_MIN_INTERVAL);
        if !throttled || self.last.is_none() {
            match self.hp.fan_levels() {
                Ok(levels) => {
                    self.last = Some((Instant::now(), levels));
                    self.status = ProviderStatus::Ok;
                }
                Err(e) => {
                    warn!(%e, "fan levels read failed");
                    self.status = ProviderStatus::Degraded(format!("0x2D read: {e}"));
                }
            }
        }
        // Throttled ticks re-publish the cached levels: the value is the
        // latest known from firmware and the 1 Hz rule outranks per-tick
        // freshness here.
        match self.last {
            Some((_, levels)) => {
                let src = MetricSource::HpWmi;
                vec![
                    fresh(ids::FAN_CPU_RPM, f64::from(levels.cpu_rpm()).into(), src),
                    fresh(ids::FAN_GPU_RPM, f64::from(levels.gpu_rpm()).into(), src),
                ]
            }
            None => Vec::new(),
        }
    }

    fn status(&self) -> ProviderStatus {
        self.status.clone()
    }
}
