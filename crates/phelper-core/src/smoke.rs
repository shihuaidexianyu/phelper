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
            if (l.cpu as i32 - cpu as i32).abs() <= 10 && (l.gpu as i32 - gpu as i32).abs() <= 10 {
                converged = true;
                break;
            }
        }
        s.push_str(if converged {
            " CONVERGED\n"
        } else {
            " NOT-CONVERGED\n"
        });
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

/// DEV-ONLY read-only MCHBAR cross-check probe (§57 Stage 1; M4-mini).
/// ZERO writes of any kind — this probe only READS. Verifies the PL4
/// readback channel end-to-end:
///   A. IntelMCHBAR loads on this CPU (module CPU allow-list) and reports a
///      sane base (0xFED1xxxx typical on Intel client);
///   B. MMIO 0x59A0 qword mirrors MSR 0x610 (known-good on 8BAB: PL1/PL2
///      already verified) — cross-validates the whole physical-read path;
///   C. the SA power block contains a dword whose bits 14:0 decode to the
///      factory PL4 (SDD 0x28 byte5 = 200 W on 8BAB, M0 probe record §24) —
///      identifies the PL4 register offset EMPIRICALLY instead of trusting
///      a register map.
/// The raw sweep is printed either way: a mismatch is evidence to record,
/// not an error to hide (fail closed = don't build on it).
#[cfg(feature = "pawnio")]
pub fn mchbar_probe() -> Result<String, phelper_domain::error::EngineError> {
    use phelper_domain::error::EngineError;

    fn p(e: PlatformError) -> EngineError {
        EngineError::Config(format!("pawnio: {e}"))
    }

    // MSR reads happen FIRST: whether multiple loaded modules coexist on
    // separate handles is itself unverified, so don't touch the IntelMSR
    // handle after the IntelMCHBAR one is loaded.
    let msr_image = pawnio::intelmsr_image().map_err(p)?;
    let msr = PawnIo::load_module(&msr_image).map_err(p)?;
    let raw_unit = pawnio::read_msr(&msr, pawnio::MSR_RAPL_POWER_UNIT).map_err(p)?;
    let unit = pawnio::power_unit_w(raw_unit);
    let raw610 = pawnio::read_msr(&msr, pawnio::MSR_PKG_POWER_LIMIT).map_err(p)?;
    let (pl1, pl2) = pawnio::pkg_power_limits_w(raw610, unit);
    drop(msr);

    let mut out = String::new();
    out.push_str(&format!(
        "MSR 0x610 = 0x{raw610:016X} → PL1={pl1:.1}W PL2={pl2:.1}W (unit 1/{}W)\n",
        (1.0 / unit) as u32
    ));

    let mch_image = pawnio::mchbar_image().map_err(p)?;
    let mch = PawnIo::load_module(&mch_image).map_err(p)?;

    // --- A. base address sanity
    let base = pawnio::mchbar_base_addr(&mch).map_err(p)?;
    let base_sane = (0xF000_0000..=0xFFFF_FFFF).contains(&base);
    out.push_str(&format!(
        "A. MCHBAR base = 0x{base:08X} — {}\n",
        if base_sane {
            "sane (PCI MMIO window)"
        } else {
            "UNEXPECTED"
        }
    ));

    // --- B. 0x59A0 qword vs MSR 0x610
    let q = pawnio::mchbar_read_qword(&mch, 0x59A0).map_err(p)?;
    let (mpl1, mpl2) = pawnio::pkg_power_limits_w(q, unit);
    let fields_match = (mpl1 - pl1).abs() <= 1.0 && (mpl2 - pl2).abs() <= 1.0;
    out.push_str(&format!(
        "B. MMIO 0x59A0 = 0x{q:016X} → PL1={mpl1:.1}W PL2={mpl2:.1}W — {}\n",
        if q == raw610 {
            "EXACT MATCH vs MSR 0x610"
        } else if fields_match {
            "PL1/PL2 fields match 0x610 (enable/lock bits differ)"
        } else {
            "MISMATCH vs 0x610"
        }
    ));

    // --- C. SA power-block sweep for the factory PL4 (SDD byte5, §24)
    const FACTORY_PL4_W: f64 = 200.0;
    out.push_str(&format!(
        "C. sweep 0x5800..0x5B00, factory PL4 = {FACTORY_PL4_W:.0}W (SDD 0x28 byte5, M0 probe §24):\n"
    ));
    let mut pl4_candidates: Vec<u32> = Vec::new();
    let mut unit_anchor: Option<u32> = None;
    let unit_raw32 = raw_unit as u32;
    let mut nonzero = 0u32;
    for off in (0x5800u32..0x5B00).step_by(4) {
        let d = match pawnio::mchbar_read_dword(&mch, off) {
            Ok(d) => d,
            Err(_) => continue, // unreadable sub-region — skip silently
        };
        if d == 0 || d == u32::MAX {
            continue;
        }
        if d == unit_raw32 {
            unit_anchor = Some(off);
        }
        let w = pawnio::power_limit_field_w(d, unit);
        if w <= 0.0 {
            continue;
        }
        nonzero += 1;
        let tag = if (w - FACTORY_PL4_W).abs() <= 1.0 {
            pl4_candidates.push(off);
            "  <== PL4 candidate"
        } else if (w - pl1).abs() <= 1.0 || (w - pl2).abs() <= 1.0 {
            "  (= PL1/PL2 value)"
        } else if unit_anchor == Some(off) {
            "  <== RAPL power-unit anchor (= MSR 0x606)"
        } else {
            ""
        };
        out.push_str(&format!(
            "  +0x{off:04X} = 0x{d:08X} → field={w:.1}W{tag}\n"
        ));
    }
    if nonzero == 0 {
        out.push_str("  (entire sweep read 0/0xFFFFFFFF — no power registers visible here)\n");
    }
    // The 0x59A0==0x610 mirror is an ASSUMPTION this probe tests, not a
    // requirement: the RAPL block self-identifies via the power-unit anchor
    // (0xA0E03 appears exactly once in a sane map) plus the PL1/PL2/PL4
    // value triple. A failed mirror with a live anchor = different layout,
    // not a dead channel.
    let verdict = match (unit_anchor, fields_match, pl4_candidates.as_slice()) {
        (Some(anchor), _, [first, ..]) => format!(
            "VERDICT: RAPL MMIO block LIVE (power-unit anchor at +0x{anchor:04X}; \
             0x59A0 mirror assumption {}). PL4 candidate at +0x{first:04X} ({} hit(s)) \
             — decisive test = 0x29 byte2 write spike watching this offset.",
            if fields_match { "confirmed" } else { "WRONG (layout differs)" },
            pl4_candidates.len()
        ),
        (None, true, _) => "VERDICT: 0x59A0 mirrors 0x610 but no power-unit anchor — partial map; treat as unverified".into(),
        _ => "VERDICT: no RAPL anchor in this window — channel NOT established; do not build on it".into(),
    };
    out.push_str(&format!("{verdict}\n"));
    Ok(out)
}

/// DEV-ONLY PL4 readback-channel decisive spike (§57 Stage 2; M4-mini).
///
/// The MCHBAR read probe (mchbar_probe) found the RAPL MMIO block live on
/// 8BAB and a PL4 CANDIDATE register at 0x59B0 (reads 200 W = SDD byte5
/// factory PL4). This spike settles whether that register is the readback
/// for 0x29 byte2: write {FF,FF,pl4,FF} — DOWNWARD ONLY (must be below the
/// factory 200 W; lowering a protection limit is the safe direction) — then
/// poll 0x59B0 at 250 ms. MSR 0x610 is polled alongside: bytes 0/1 carry
/// 0xFF = NO_CHANGE, so PL1/PL2 must not move — the first dedicated
/// NO_CHANGE-semantics check (M3 never isolated it). Restore writes byte2
/// back to the MEASURED baseline explicitly (the {0,0} DEFAULT restore was
/// proven ineffective on bytes 0/1 — never rely on it here either).
#[cfg(all(feature = "experimental-hp-power-limits", feature = "pawnio"))]
pub fn pl4_spike(pl4_w: u8) -> Result<String, phelper_domain::error::EngineError> {
    use phelper_domain::error::{EngineError, HpWmiError};

    use crate::platform::hp_wmi::HpWmiTransport;
    use crate::platform::hp_wmi::commands::{self, HpCommandGroup, cmd};

    fn w(e: HpWmiError) -> EngineError {
        EngineError::WmiUnavailable(e.to_string())
    }
    fn p(e: PlatformError) -> EngineError {
        EngineError::Config(format!("pawnio: {e}"))
    }

    const FACTORY_PL4_W: u8 = 200; // SDD 0x28 byte5 (M0 probe, §24)
    const PL4_OFFSET: u32 = 0x59B0; // mchbar_probe candidate (2026-08-26)
    if pl4_w >= FACTORY_PL4_W {
        return Err(EngineError::Config(format!(
            "spike is DOWNWARD ONLY: --pl4 {pl4_w}W >= factory {FACTORY_PL4_W}W refused \
             (raising a protection limit is not an experiment, it's a bet)"
        )));
    }
    if pl4_w < 30 {
        return Err(EngineError::Config(
            "--pl4 below 30W is outside any plausible PL4 envelope".into(),
        ));
    }

    let t = HpWmiTransport::connect().map_err(w)?;
    // MSR reads first (module-coexistence is unverified — see mchbar_probe).
    let msr = PawnIo::load_module(&pawnio::intelmsr_image().map_err(p)?).map_err(p)?;
    let raw_unit = pawnio::read_msr(&msr, pawnio::MSR_RAPL_POWER_UNIT).map_err(p)?;
    let unit = pawnio::power_unit_w(raw_unit);
    let raw610_base = pawnio::read_msr(&msr, pawnio::MSR_PKG_POWER_LIMIT).map_err(p)?;
    let (b1, b2) = pawnio::pkg_power_limits_w(raw610_base, unit);
    drop(msr);

    let mch = PawnIo::load_module(&pawnio::mchbar_image().map_err(p)?).map_err(p)?;
    let read_pl4 = |mch: &PawnIo| -> Result<f64, PlatformError> {
        let d = pawnio::mchbar_read_dword(mch, PL4_OFFSET)?;
        Ok(pawnio::power_limit_field_w(d, unit))
    };

    let mut out = String::new();
    let baseline_w = read_pl4(&mch).map_err(p)?;
    out.push_str(&format!(
        "baseline: 0x610 PL1={b1:.1}W PL2={b2:.1}W; MCHBAR+0x{PL4_OFFSET:04X} = {baseline_w:.1}W\n"
    ));
    let restore_w = baseline_w.round() as u8;
    if (baseline_w - f64::from(FACTORY_PL4_W)).abs() > 2.0 {
        out.push_str(&format!(
            "WARNING: baseline {baseline_w:.1}W != factory {FACTORY_PL4_W}W — restoring the MEASURED value {restore_w}W\n"
        ));
    }

    let payload = commands::encode_power_limits_pl4_only(pl4_w);
    out.push_str(&format!(
        "0x29 write {payload:02X?} (intent PL4={pl4_w}W, bytes 0/1/3 = NO_CHANGE)\n"
    ));

    let spike_result = (|| -> Result<String, HpWmiError> {
        // Second module on a second handle, alive ALONGSIDE the MCHBAR one —
        // the polls below succeeding double as the module-coexistence proof.
        let msr2 = PawnIo::load_module(
            &pawnio::intelmsr_image()
                .map_err(|e| HpWmiError::Transport(format!("intelmsr reload: {e}")))?,
        )
        .map_err(|e| HpWmiError::Transport(format!("intelmsr reload: {e}")))?;
        t.write_execute(HpCommandGroup::Gaming, cmd::POWER_LIMITS, &payload)?;
        let mut s = String::from("write rc=0; polls (250 ms) [pl4_reg | pl1,pl2]:");
        let mut moved = false;
        let mut nc_violated = false;
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let w4 = read_pl4(&mch).unwrap_or(f64::NAN);
            let cur = pawnio::read_msr(&msr2, pawnio::MSR_PKG_POWER_LIMIT)
                .map(|raw| pawnio::pkg_power_limits_w(raw, unit))
                .unwrap_or((f64::NAN, f64::NAN));
            s.push_str(&format!(" [{w4:.1} | {:.1},{:.1}]", cur.0, cur.1));
            if (cur.0 - b1).abs() > 1.0 || (cur.1 - b2).abs() > 1.0 {
                nc_violated = true;
            }
            if (w4 - f64::from(pl4_w)).abs() <= 2.0 {
                moved = true;
                break;
            }
        }
        s.push('\n');
        if nc_violated {
            s.push_str("ALARM: PL1/PL2 MOVED during a byte2-only write — 0xFF NO_CHANGE semantics VIOLATED on this firmware!\n");
        }
        s.push_str(&if moved {
            format!("VERDICT: READBACK VERIFIED — MCHBAR+0x{PL4_OFFSET:04X} IS the PL4 register for 0x29 byte2")
        } else {
            format!("VERDICT: NO EFFECT at +0x{PL4_OFFSET:04X} — byte2 write ignored or wrong register; fail closed")
        });
        s.push('\n');
        Ok(s)
    })();

    // ALWAYS restore byte2 to the measured baseline explicitly.
    let restore_payload = commands::encode_power_limits_pl4_only(restore_w);
    let restore = t.write_execute(HpCommandGroup::Gaming, cmd::POWER_LIMITS, &restore_payload);
    out.push_str(&spike_result.unwrap_or_else(|e| format!("spike error: {e}\n")));
    match restore {
        Ok(()) => {
            out.push_str(&format!(
                "0x29 restore {{FF,FF,{restore_w},FF}} (measured baseline) -> rc=0\n"
            ));
            std::thread::sleep(std::time::Duration::from_millis(500));
            match read_pl4(&mch) {
                Ok(w4) => out.push_str(&format!(
                    "post-restore +0x{PL4_OFFSET:04X}: {w4:.1}W {}\n",
                    if (w4 - f64::from(restore_w)).abs() <= 2.0 {
                        "(restore confirmed)"
                    } else {
                        "(NOT back at baseline — inspect!)"
                    }
                )),
                Err(e) => out.push_str(&format!("post-restore read failed: {e}\n")),
            }
        }
        Err(e) => out.push_str(&format!(
            "RESTORE FAILED: {e} — check +0x{PL4_OFFSET:04X} and re-write {restore_w}W manually!\n"
        )),
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
            format!(
                "VERDICT: INCONCLUSIVE — final PL1={f1:.1}W PL2={f2:.1}W matches neither intent nor baseline"
            )
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
        Err(e) => out.push_str(&format!(
            "RESTORE FAILED: {e} — power-cycle expectation: values may persist; check 0x610!\n"
        )),
    }
    Ok(out)
}
