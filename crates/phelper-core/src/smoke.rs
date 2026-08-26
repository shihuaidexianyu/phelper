//! Provider smoke checks (M0.4): one-shot read of each telemetry backend.
//! This is a DEV HARNESS surface for the probe CLI — M1 turns these into
//! scheduled collectors behind the TelemetryCoordinator.

use phelper_domain::error::PlatformError;
#[cfg(feature = "nvidia")]
use phelper_domain::ports::GpuTelemetry;

#[cfg(feature = "pawnio")]
use crate::platform::pawnio::{self, PawnIo};

pub struct SmokeRow {
    pub provider: &'static str,
    pub status: String,
    pub detail: Option<String>,
}

impl SmokeRow {
    fn ok(provider: &'static str, detail: String) -> Self {
        Self {
            provider,
            status: "OK".into(),
            detail: Some(detail),
        }
    }
    fn fail(provider: &'static str, e: impl std::fmt::Display) -> Self {
        Self {
            provider,
            status: "FAIL".into(),
            detail: Some(e.to_string()),
        }
    }
}

#[cfg(feature = "pawnio")]
fn pawnio_smoke() -> SmokeRow {
    match pawnio_smoke_inner() {
        Ok(detail) => SmokeRow::ok("pawnio/intel-msr", detail),
        Err(e) => SmokeRow::fail("pawnio/intel-msr", e),
    }
}

#[cfg(feature = "pawnio")]
fn pawnio_smoke_inner() -> Result<String, PlatformError> {
    let image = pawnio::intelmsr_image()?;
    let io = PawnIo::load_module(&image)?;

    let tjmax_raw = pawnio::read_msr(&io, pawnio::MSR_TEMPERATURE_TARGET)?;
    let therm = pawnio::read_msr(&io, pawnio::MSR_THERM_STATUS)?;
    let unit = pawnio::read_msr(&io, pawnio::MSR_RAPL_POWER_UNIT)?;

    let tjmax = ((tjmax_raw >> 16) & 0xFF) as i32;
    let temp = pawnio::pkg_temp_c(tjmax_raw, therm);

    // RAPL power over a short window.
    let e0 = pawnio::read_msr(&io, pawnio::MSR_PKG_ENERGY_STATUS)? as u32;
    let t0 = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_millis(120));
    let e1 = pawnio::read_msr(&io, pawnio::MSR_PKG_ENERGY_STATUS)? as u32;
    let dt = t0.elapsed().as_secs_f64();
    let power = pawnio::delta_energy(e0, e1) as f64 * pawnio::energy_unit_j(unit) / dt;

    Ok(format!(
        "tjmax={tjmax}C pkg_temp={:?}C pkg_power={power:.1}W unit=1/{}J",
        temp.map(|t| t as i32),
        1u32 << ((unit >> 8) & 0x1F)
    ))
}

#[cfg(feature = "nvidia")]
fn nvidia_smoke() -> SmokeRow {
    match nvidia_smoke_inner() {
        Ok(detail) => SmokeRow::ok("nvapi", detail),
        Err(e) => SmokeRow::fail("nvapi", e),
    }
}

#[cfg(feature = "nvidia")]
fn nvidia_smoke_inner() -> Result<String, PlatformError> {
    use crate::platform::nvidia::NvidiaGpu;
    let mut gpu = NvidiaGpu::open()?;
    let name = gpu.name().to_string();
    let s = gpu.sample()?;
    Ok(format!(
        "{name} temp={:?}C power={:?}W util={:?}% core={:?}MHz mem={:?}MHz pstate={:?}",
        s.temp_c.map(|v| v as i32),
        s.power_w.map(|v| (v * 10.0) as i32 as f64 / 10.0),
        s.util_percent.map(|v| v as i32),
        s.core_clock_mhz.map(|v| v as i32),
        s.mem_clock_mhz.map(|v| v as i32),
        s.pstate
    ))
}

// cfg-gated pushes can't be expressed as a vec![] literal.
#[allow(clippy::vec_init_then_push)]
pub fn run() -> Vec<SmokeRow> {
    #[allow(unused_mut)]
    let mut rows = Vec::new();
    #[cfg(feature = "pawnio")]
    rows.push(pawnio_smoke());
    #[cfg(feature = "nvidia")]
    rows.push(nvidia_smoke());
    rows
}

/// DEV-ONLY on-device write spike (§57 Stage 2; plan spike S2): proves the
/// HP write transport BEFORE the ControlCoordinator is built on top of it.
///
/// Sequence (every step logged, firmware auto restored at the end no
/// matter how it goes):
/// 1. 0x1A thermal: Balanced → Performance → Balanced (TrustedWrite op —
///    transport rc=0 is the proof point here).
/// 2. Fan: 0x2D baseline → 0x2E {cpu,gpu} → poll 0x2D up to 5× 1 s
///    (§38 1 Hz rule binds this too) → 0x2E {0,0} restore auto.
///    The 0x2D tach moving from the auto baseline to the commanded level
///    is the strongest possible end-to-end proof: real write, real
///    hardware response, real readback. Pick a target CLEARLY distinct
///    from the idle baseline (~2500 RPM) — a 3000 RPM target sits inside
///    the tach tolerance of a 2500/2800 baseline and proves nothing.
#[cfg(feature = "control")]
pub fn hp_write_spike(cpu: u16, gpu: u16) -> Result<String, phelper_domain::error::EngineError> {
    use phelper_domain::error::{EngineError, HpWmiError};
    use phelper_domain::policy::{FanLevels, ThermalMode};
    use phelper_domain::ports::{HpControl, HpPlatform};

    use crate::platform::hp_wmi::HpWmiTransport;

    fn w(e: HpWmiError) -> EngineError {
        EngineError::WmiUnavailable(e.to_string())
    }

    let t = HpWmiTransport::connect().map_err(w)?;
    let mut out = String::new();

    // --- 1. thermal mode round-trip (no readback exists; rc=0 is the proof)
    t.set_thermal_mode(ThermalMode::Balanced).map_err(w)?;
    out.push_str("0x1A Balanced -> rc=0\n");
    t.set_thermal_mode(ThermalMode::Performance).map_err(w)?;
    out.push_str("0x1A Performance -> rc=0\n");
    t.set_thermal_mode(ThermalMode::Balanced).map_err(w)?;
    out.push_str("0x1A Balanced (restore) -> rc=0\n");

    // --- 2. manual fan with real readback
    let before = t.fan_levels().map_err(w)?;
    out.push_str(&format!(
        "0x2D baseline: cpu={} gpu={} (x100 RPM)\n",
        before.cpu, before.gpu
    ));

    let spike_result = (|| -> Result<String, HpWmiError> {
        t.set_fan_levels(FanLevels::new(cpu, gpu))?;
        let mut s = format!("0x2E {{{cpu},{gpu}}} written; 0x2D polls:");
        let mut converged = false;
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let l = t.fan_levels()?;
            s.push_str(&format!(" ({},{})", l.cpu, l.gpu));
            // Tach hunts; accept within ±10 units (±1000 RPM). The target
            // MUST sit further than this from the baseline or the check is
            // vacuous (see fn docs).
            if (l.cpu as i32 - cpu as i32).abs() <= 10
                && (l.gpu as i32 - gpu as i32).abs() <= 10
            {
                converged = true;
                break;
            }
        }
        s.push_str(if converged { " CONVERGED\n" } else { " NOT-CONVERGED\n" });
        Ok(s)
    })();

    // Restore firmware auto regardless of how the spike went (AR-12 habit).
    let restore = t.set_fan_levels(FanLevels::AUTO);
    out.push_str(&spike_result.unwrap_or_else(|e| format!("spike error: {e}\n")));
    match restore {
        Ok(()) => out.push_str("0x2E {0,0} restore auto -> rc=0\n"),
        Err(e) => out.push_str(&format!("RESTORE FAILED: {e}\n")),
    }
    Ok(out)
}
