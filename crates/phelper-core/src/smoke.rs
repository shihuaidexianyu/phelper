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

/// DEV-ONLY 0x29 byte-order arbitration spike (§57 Stage 2; M3 spike S2).
///
/// The kernel struct says `{pl1, pl2, 0xFF, 0xFF}`; the "OSH order" reading
/// says pl1/pl2 are swapped. Neither can be proven from literature (the
/// kernel only ever writes {0,0,FF,FF} on this board class; OSH only writes
/// pl1==pl2). This spike writes ASYMMETRIC limits (pl1 != pl2, both far
/// from the 55/130 baseline) under ONE candidate encoding and watches MSR
/// 0x610 (PawnIO, read-only): whichever 0x610 field moves to which value
/// settles the order on THIS firmware. Write → readback → restore is the
/// whole shape; the RAPL-under-load behavior check (runbook step 3) happens
/// separately under `control power-limits`.
///
/// The restore is the kernel's own AC/DC write: {0x00, 0x00, 0xFF, 0xFF} =
/// "firmware defaults, leave pl4/cc alone" — no hardcoded 55/130 needed.
#[cfg(all(feature = "experimental-hp-power-limits", feature = "pawnio"))]
pub fn power_limits_spike(
    kernel_order: bool,
    pl1: u8,
    pl2: u8,
) -> Result<String, phelper_domain::error::EngineError> {
    use phelper_domain::error::{EngineError, HpWmiError};

    use crate::platform::hp_wmi::HpWmiTransport;
    use crate::platform::hp_wmi::commands::{self, HpCommandGroup, cmd};

    fn w(e: HpWmiError) -> EngineError {
        EngineError::WmiUnavailable(e.to_string())
    }
    fn p(e: PlatformError) -> EngineError {
        EngineError::Config(format!("pawnio: {e}"))
    }

    if pl1 == pl2 {
        return Err(EngineError::Config(
            "arbitration needs ASYMMETRIC limits (pl1 != pl2) — equal values \
             cannot distinguish the byte order"
                .into(),
        ));
    }

    let t = HpWmiTransport::connect().map_err(w)?;
    let image = pawnio::intelmsr_image().map_err(p)?;
    let io = PawnIo::load_module(&image).map_err(p)?;
    let unit = pawnio::power_unit_w(pawnio::read_msr(&io, pawnio::MSR_RAPL_POWER_UNIT).map_err(p)?);

    let read_pl = |io: &PawnIo| -> Result<(f64, f64), PlatformError> {
        let raw = pawnio::read_msr(io, pawnio::MSR_PKG_POWER_LIMIT)?;
        Ok(pawnio::pkg_power_limits_w(raw, unit))
    };

    let mut out = String::new();
    let (b1, b2) = read_pl(&io).map_err(p)?;
    out.push_str(&format!(
        "0x610 baseline: PL1={b1:.1}W PL2={b2:.1}W (unit 1/{}W)\n",
        (1.0 / unit) as u32
    ));

    let payload = if kernel_order {
        commands::encode_power_limits_kernel(pl1, pl2)
    } else {
        commands::encode_power_limits_swapped(pl1, pl2)
    };
    out.push_str(&format!(
        "0x29 write [{} order]: {:02X?} (intent PL1={pl1}W PL2={pl2}W)\n",
        if kernel_order { "kernel" } else { "swapped" },
        payload
    ));

    let spike_result = (|| -> Result<String, HpWmiError> {
        t.write_execute(HpCommandGroup::Gaming, cmd::POWER_LIMITS, &payload)?;
        let mut s = String::from("write rc=0; 0x610 polls (250 ms):");
        let mut last = (b1, b2);
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            match read_pl(&io) {
                Ok((v1, v2)) => {
                    s.push_str(&format!(" ({v1:.1},{v2:.1})"));
                    last = (v1, v2);
                    // Settle early once BOTH fields moved away from baseline.
                    if (v1 - b1).abs() > 1.0 && (v2 - b2).abs() > 1.0 {
                        break;
                    }
                }
                Err(e) => s.push_str(&format!(" (read err: {e})")),
            }
        }
        let (f1, f2) = last;
        let near = |a: f64, b: f64| (a - b).abs() <= 2.0;
        let verdict = if near(f1, f64::from(pl1)) && near(f2, f64::from(pl2)) {
            format!(
                "VERDICT: encoding CORRECT — {} order puts PL1={pl1}W PL2={pl2}W",
                if kernel_order { "kernel" } else { "swapped" }
            )
        } else if near(f1, f64::from(pl2)) && near(f2, f64::from(pl1)) {
            format!(
                "VERDICT: bytes SWAPPED on this firmware — the {} order is WRONG, use the other",
                if kernel_order { "kernel" } else { "swapped" }
            )
        } else if near(f1, b1) && near(f2, b2) {
            "VERDICT: NO EFFECT — 0x29 explicit write ignored by this firmware (fail closed)".into()
        } else {
            format!("VERDICT: INCONCLUSIVE — final PL1={f1:.1}W PL2={f2:.1}W matches neither intent nor baseline")
        };
        s.push_str(&format!("\nfinal: PL1={f1:.1}W PL2={f2:.1}W\n{verdict}\n"));
        Ok(s)
    })();

    // ALWAYS restore firmware defaults afterwards (the kernel's own write).
    let restore_payload = commands::encode_power_limits_restore_default();
    let restore = t.write_execute(HpCommandGroup::Gaming, cmd::POWER_LIMITS, &restore_payload);
    out.push_str(&spike_result.unwrap_or_else(|e| format!("spike error: {e}\n")));
    match restore {
        Ok(()) => {
            out.push_str("0x29 restore {0,0,FF,FF} (firmware defaults) -> rc=0\n");
            std::thread::sleep(std::time::Duration::from_millis(500));
            match read_pl(&io) {
                Ok((r1, r2)) => {
                    out.push_str(&format!("post-restore 0x610: PL1={r1:.1}W PL2={r2:.1}W\n"))
                }
                Err(e) => out.push_str(&format!("post-restore 0x610 read failed: {e}\n")),
            }
        }
        Err(e) => out.push_str(&format!("RESTORE FAILED: {e} — power-cycle expectation: values may persist; check 0x610!\n")),
    }
    Ok(out)
}
