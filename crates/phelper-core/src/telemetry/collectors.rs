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
        let mut degraded = None;
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
                degraded = Some(format!("therm read: {e}"));
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
                degraded = Some(format!("rapl read: {e}"));
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
                degraded = Some("mperf/aperf read failed".into());
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
                degraded = Some(format!("0x610 read: {e}"));
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

        self.status = degraded.map_or(ProviderStatus::Ok, ProviderStatus::Degraded);
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

/// Windows processor-policy readback. One PowrProf snapshot supplies all
/// current AC/DC indexes; this avoids re-resolving the active scheme once per
/// knob and keeps the telemetry path cheap enough for fast startup.
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
        let mut out = Vec::with_capacity(12);
        let state = match crate::platform::windows_ppm::read_windows_ppm_state() {
            Ok(state) => {
                self.status = ProviderStatus::Ok;
                state
            }
            Err(e) => {
                debug!(%e, "Windows PPM snapshot read failed");
                self.status = ProviderStatus::Degraded(format!("PPM read: {e}"));
                return out;
            }
        };
        let push_u8 = |out: &mut Vec<MetricSample>, id, value: Option<u8>| {
            if let Some(value) = value {
                out.push(fresh(
                    id,
                    V::U64(u64::from(value)),
                    MetricSource::WindowsPpm,
                ));
            }
        };
        let push_u32 = |out: &mut Vec<MetricSample>, id, value: Option<u32>| {
            if let Some(value) = value {
                out.push(fresh(
                    id,
                    V::U64(u64::from(value)),
                    MetricSource::WindowsPpm,
                ));
            }
        };
        for (values, ac) in [(&state.ac, true), (&state.dc, false)] {
            push_u8(
                &mut out,
                if ac { ids::CPU_EPP_AC } else { ids::CPU_EPP_DC },
                values.epp,
            );
            push_u8(
                &mut out,
                if ac {
                    ids::CPU_EPP1_AC
                } else {
                    ids::CPU_EPP1_DC
                },
                values.epp1,
            );
            push_u32(
                &mut out,
                if ac {
                    ids::CPU_MAX_FREQ_AC
                } else {
                    ids::CPU_MAX_FREQ_DC
                },
                values.max_freq_mhz,
            );
            push_u8(
                &mut out,
                if ac {
                    ids::CPU_MIN_PERF_AC
                } else {
                    ids::CPU_MIN_PERF_DC
                },
                values.min_performance,
            );
            push_u8(
                &mut out,
                if ac {
                    ids::CPU_MAX_PERF_AC
                } else {
                    ids::CPU_MAX_PERF_DC
                },
                values.max_performance,
            );
            push_u8(
                &mut out,
                if ac {
                    ids::CPU_BOOST_AC
                } else {
                    ids::CPU_BOOST_DC
                },
                values.boost_policy.map(u8::from),
            );
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

pub(crate) struct HpFanCollector<H> {
    hp: Arc<H>,
    last: Option<(Instant, phelper_domain::policy::FanLevels)>,
    status: ProviderStatus,
}

impl<H> HpFanCollector<H> {
    pub(crate) fn new(hp: Arc<H>) -> Self {
        Self {
            hp,
            last: None,
            status: ProviderStatus::Ok,
        }
    }
}

impl<H: HpPlatform + Sync> Collector for HpFanCollector<H> {
    fn name(&self) -> &'static str {
        "hp-wmi/fans"
    }

    fn cadence(&self) -> Duration {
        registry::meta(ids::FAN_LEFT_RPM)
            .expect("registry entry")
            .cadence
    }

    fn collect(&mut self) -> Vec<MetricSample> {
        let throttled = self
            .last
            .is_some_and(|(at, _)| at.elapsed() < FAN_MIN_INTERVAL);
        if throttled {
            // The store already holds the last real sample. Re-emitting it
            // with `fresh()` would forge a new timestamp and defeat the
            // sensor-freeze watchdog used by manual fan control.
            return Vec::new();
        }
        match self.hp.fan_levels() {
            Ok(levels) => {
                self.last = Some((Instant::now(), levels));
                self.status = ProviderStatus::Ok;
                let src = MetricSource::HpWmi;
                vec![
                    // The domain fields retain the upstream CPU/GPU wire
                    // names for compatibility. On 8BAB, channel 0/1 are
                    // presented to the user as the physical left/right fans.
                    fresh(ids::FAN_LEFT_RPM, f64::from(levels.left_rpm()).into(), src),
                    fresh(
                        ids::FAN_RIGHT_RPM,
                        f64::from(levels.right_rpm()).into(),
                        src,
                    ),
                ]
            }
            Err(e) => {
                warn!(%e, "fan levels read failed");
                self.status = ProviderStatus::Degraded(format!("0x2D read: {e}"));
                // Preserve the previous sample and its original timestamp in
                // the store. No data is more honest than freshly dated old
                // data, especially for the fail-closed watchdog.
                Vec::new()
            }
        }
    }

    fn status(&self) -> ProviderStatus {
        self.status.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::error::HpWmiError;
    use phelper_domain::hp::{FanTable, SystemDesignData};
    use phelper_domain::policy::{FanLevels, GpuPlatformPolicy, MuxMode};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeHp(Mutex<VecDeque<Result<FanLevels, HpWmiError>>>);

    impl HpPlatform for FakeHp {
        fn fan_count(&self) -> Result<u8, HpWmiError> {
            Ok(2)
        }
        fn system_design_data(&self) -> Result<SystemDesignData, HpWmiError> {
            Err(HpWmiError::NotAvailable("test"))
        }
        fn fan_table(&self) -> Result<FanTable, HpWmiError> {
            Err(HpWmiError::NotAvailable("test"))
        }
        fn fan_levels(&self) -> Result<FanLevels, HpWmiError> {
            self.0
                .lock()
                .expect("fake hp")
                .pop_front()
                .unwrap_or(Err(HpWmiError::NotAvailable("test")))
        }
        fn gpu_platform_policy(&self) -> Result<GpuPlatformPolicy, HpWmiError> {
            Err(HpWmiError::NotAvailable("test"))
        }
        fn mux_mode(&self) -> Result<MuxMode, HpWmiError> {
            Err(HpWmiError::NotAvailable("test"))
        }
        fn max_fan_readback_diagnostic(&self) -> Result<bool, HpWmiError> {
            Err(HpWmiError::NotAvailable("test"))
        }
    }

    #[test]
    fn failed_fan_read_does_not_forge_a_fresh_sample() {
        let hp = Arc::new(FakeHp(Mutex::new(VecDeque::from([
            Ok(FanLevels::new(30, 32)),
            Err(HpWmiError::Timeout),
        ]))));
        let mut collector = HpFanCollector::new(hp);
        let first = collector.collect();
        assert_eq!(first.len(), 2);

        let old_at = Instant::now() - FAN_MIN_INTERVAL;
        collector.last.as_mut().expect("last sample").0 = old_at;
        let failed = collector.collect();

        assert!(failed.is_empty());
        assert_eq!(collector.last.expect("cached sample").0, old_at);
        assert!(matches!(collector.status(), ProviderStatus::Degraded(_)));
    }
}
