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
