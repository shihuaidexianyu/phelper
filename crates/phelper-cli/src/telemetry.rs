//! `phelper-cli telemetry` — live metric table (dev/verification harness).
//!
//! Renders the canonical registry every --interval-ms. ANSI clear-screen is
//! used instead of a TUI crate: this is a verification harness, not the
//! product UI (GPUI lands in Phase 3). Ctrl+C now shuts the engine down
//! GRACEFULLY — since M2 the engine includes the control coordinator, and
//! an ungraceful kill would leave fan/thermal state to the ~120 s firmware
//! clawback (AR-12).

use std::io::Write as _;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Args;
use phelper_core::domain::telemetry::{
    MetricQuality, MetricSample, MetricValue, ProviderStatus, ids,
};
use phelper_core::{Engine, telemetry::registry};

use crate::ctrlc_flag;

#[derive(Args)]
pub struct TelemetryArgs {
    /// Refresh interval in milliseconds.
    #[arg(long, default_value_t = 500)]
    interval_ms: u64,
    /// Stop after this many seconds (0 = run until Ctrl+C).
    #[arg(long, default_value_t = 0)]
    duration: u64,
    /// Only show metrics whose id contains this substring (repeatable).
    #[arg(long)]
    metrics: Vec<String>,
}

pub fn run(args: TelemetryArgs) -> Result<()> {
    let stop = ctrlc_flag()?;
    let engine = Engine::start()?;
    let handle = engine.telemetry().clone();
    let interval = Duration::from_millis(args.interval_ms.max(50));
    let deadline = (args.duration > 0).then(|| Instant::now() + Duration::from_secs(args.duration));

    eprintln!(
        "engine up; rendering every {} ms (Ctrl+C to quit)",
        interval.as_millis()
    );
    let mut stdout = std::io::stdout();
    let run_start = Instant::now();
    loop {
        let snap = handle.snapshot();
        let mut out = String::with_capacity(4096);
        out.push_str("\x1b[2J\x1b[H"); // clear + home
        out.push_str("phelper telemetry — board 8BAB (M1, read-only)\r\n");
        out.push_str("metric                      value        qual       age   owner\r\n");
        out.push_str(
            "--------------------------  -----------  ---------  ------  ------------------\r\n",
        );
        for meta in registry::all() {
            if !args.metrics.is_empty()
                && !args.metrics.iter().any(|m| meta.id.0.contains(m.as_str()))
            {
                continue;
            }
            let (value, qual, age) = match snap.samples.get(&meta.id) {
                Some(s) => (
                    fmt_value(s),
                    fmt_quality(s.quality),
                    fmt_age(s, meta.cadence),
                ),
                None => ("—".into(), "no data".into(), String::new()),
            };
            out.push_str(&format!(
                "{:<26}  {:>11}  {:<9}  {:>6}  {:?}\r\n",
                meta.id.0, value, qual, age, meta.owner
            ));
        }

        out.push_str("\r\nproviders\r\n");
        for (name, status) in &snap.providers {
            out.push_str(&format!("  {:<20} {}\r\n", name, fmt_provider(status)));
        }
        let jitter = handle.scheduler_jitter();
        if !jitter.is_empty() {
            out.push_str("\r\nmax scheduler jitter\r\n");
            for (name, j) in &jitter {
                out.push_str(&format!(
                    "  {:<20} {:.1} ms\r\n",
                    name,
                    j.as_secs_f64() * 1e3
                ));
            }
        }
        stdout.write_all(out.as_bytes())?;
        stdout.flush()?;

        if stop.load(Ordering::Relaxed) {
            eprintln!("\nCtrl+C — shutting the engine down gracefully");
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        std::thread::sleep(interval);
    }

    engine.shutdown();

    // End-of-run window stats: min/max/avg over the whole run for the
    // metrics the acceptance list calls out.
    let window = deadline
        .map(|_| Duration::from_secs(args.duration.max(1)))
        .unwrap_or_else(|| run_start.elapsed().max(Duration::from_secs(1)));
    eprintln!("\n-- run stats ({} s window) --", window.as_secs());
    for id in [
        ids::CPU_PKG_TEMP_C,
        ids::CPU_PKG_POWER_W,
        ids::CPU_EFFECTIVE_CLOCK_MHZ,
        ids::GPU_TEMP_C,
        ids::GPU_POWER_W,
        ids::GPU_CORE_CLOCK_MHZ,
        ids::FAN_LEFT_RPM,
        ids::FAN_RIGHT_RPM,
    ] {
        match handle.stats(id, window) {
            Some(st) => eprintln!(
                "  {:<26} n={:<5} min={:.1} avg={:.1} max={:.1}",
                id.0, st.count, st.min, st.avg, st.max
            ),
            None => eprintln!("  {:<26} no samples", id.0),
        }
    }
    eprintln!("engine stopped cleanly");
    Ok(())
}

fn fmt_value(s: &MetricSample) -> String {
    match s.value {
        MetricValue::F64(v) => {
            let id = s.id.0;
            if id.ends_with("_bytes") {
                human_bytes(v)
            } else if id.ends_with("_bps") {
                format!("{}/s", human_bytes(v))
            } else if id.ends_with("_rpm") || id.ends_with("_mhz") || id.ends_with("_percent") {
                format!("{v:.0}")
            } else {
                format!("{v:.1}")
            }
        }
        MetricValue::U64(v) => {
            if s.id.0.ends_with("_raw") {
                format!("0x{v:x}")
            } else if s.id.0.ends_with("_bytes") {
                human_bytes(v as f64)
            } else {
                format!("{v}")
            }
        }
        MetricValue::Bool(v) => if v { "yes" } else { "no" }.into(),
    }
}

fn fmt_quality(q: MetricQuality) -> String {
    match q {
        MetricQuality::Fresh => String::new(),
        other => format!("{other:?}").to_lowercase(),
    }
}

fn fmt_age(s: &MetricSample, cadence: Duration) -> String {
    let age = s.timestamp.elapsed();
    let stale = age > cadence * 3;
    format!(
        "{:.1}s{}",
        age.as_secs_f64(),
        if stale { " STALE" } else { "" }
    )
}

fn fmt_provider(status: &ProviderStatus) -> String {
    match status {
        ProviderStatus::Ok => "ok".into(),
        ProviderStatus::Degraded(d) => format!("DEGRADED: {d}"),
        ProviderStatus::Unavailable(d) => format!("UNAVAILABLE: {d}"),
        ProviderStatus::Unsupported(d) => format!("UNSUPPORTED: {d}"),
    }
}

fn human_bytes(v: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = v;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}
