mod control;
mod gpuload;
mod probe;
mod telemetry;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "phelper-cli",
    about = "phelper dev/verification harness (OMEN 16-wf0032TX / board 8BAB only)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read-only capability probe (Phase 0). Performs ZERO writes.
    Probe(probe::ProbeArgs),
    /// Live telemetry view.
    Telemetry(telemetry::TelemetryArgs),
    /// M2 control plane: EPP/max-freq/boost (Windows PPM) and thermal/fan
    /// (HP WMI) with §56 before/command/after evidence.
    Control(control::ControlArgs),
    /// DEV ONLY: burn the dGPU for N seconds (verification load generator).
    GpuLoad(gpuload::GpuLoadArgs),
    /// DEV ONLY: saturate N CPU threads for N seconds (verification load
    /// generator for RAPL/PL1 clamp evidence; PowerShell jobs/runspaces
    /// proved unreliable — they decayed mid-run). Pure userspace spin,
    /// zero hardware access.
    CpuLoad {
        #[arg(long, default_value_t = 90)]
        seconds: u64,
        #[arg(long, default_value_t = 32)]
        threads: usize,
    },
    /// DEV ONLY (§57 Stage 2, spike S2): HP write-transport spike — 0x1A
    /// thermal round-trip + 0x2E manual fan with 0x2D readback, then
    /// restores firmware auto. First hardware write this project ever did.
    /// Pick a fan target clearly away from the auto baseline (~2500 RPM).
    HpSpike {
        /// CPU fan target, RPM (multiple of 100).
        #[arg(long, default_value_t = 5000)]
        cpu: u16,
        /// GPU fan target, RPM (multiple of 100).
        #[arg(long, default_value_t = 5000)]
        gpu: u16,
    },
    /// DEV ONLY (§57 Stage 2, M3 spike S2): 0x29 byte-order ARBITRATION.
    /// Writes asymmetric PL1/PL2 under one candidate encoding, watches MSR
    /// 0x610 (PawnIO read-only), then ALWAYS restores firmware defaults
    /// ({0,0,FF,FF} — the kernel's own AC/DC write). Only exists in
    /// `--features experimental` builds.
    #[cfg(feature = "experimental")]
    PowerSpike {
        /// Candidate byte order: kernel struct {pl1,pl2,FF,FF} or swapped.
        #[arg(long, value_enum, default_value_t = SpikeOrder::Kernel)]
        order: SpikeOrder,
        /// Intended PL1 in watts (must differ from --pl2 to arbitrate).
        #[arg(long, default_value_t = 45)]
        pl1: u8,
        /// Intended PL2 in watts (must differ from --pl1 to arbitrate).
        #[arg(long, default_value_t = 90)]
        pl2: u8,
    },
}

#[cfg(feature = "experimental")]
#[derive(Clone, Copy, clap::ValueEnum)]
enum SpikeOrder {
    /// Kernel struct order: {pl1, pl2, 0xFF, 0xFF}.
    Kernel,
    /// Swapped pl1/pl2: {pl2, pl1, 0xFF, 0xFF}.
    Swapped,
}

/// Shared Ctrl+C flag for loops that must shut the engine down gracefully
/// (AR-12 is load-bearing now that the CLI dispatches writes: an ungraceful
/// kill leaves fan/thermal state to the ~120 s firmware clawback).
pub(crate) fn ctrlc_flag() -> Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || {
        flag.store(true, Ordering::Relaxed);
    })?;
    Ok(stop)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Probe(args) => probe::run(args),
        Cmd::Telemetry(args) => telemetry::run(args),
        Cmd::Control(args) => control::run(args),
        Cmd::GpuLoad(args) => gpuload::run(args),
        Cmd::CpuLoad { seconds, threads } => {
            let stop = Arc::new(AtomicBool::new(false));
            let mut handles = Vec::with_capacity(threads);
            for _ in 0..threads {
                let stop = Arc::clone(&stop);
                handles.push(std::thread::spawn(move || {
                    // Black-boxed spin so the optimizer can't fold the loop away.
                    let mut acc = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        acc = acc.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                        std::hint::black_box(acc);
                    }
                }));
            }
            std::thread::sleep(std::time::Duration::from_secs(seconds));
            stop.store(true, Ordering::Relaxed);
            for h in handles {
                let _ = h.join();
            }
            println!("cpu-load done ({threads} threads × {seconds}s)");
            Ok(())
        }
        Cmd::HpSpike { cpu, gpu } => {
            if cpu % 100 != 0 || gpu % 100 != 0 {
                anyhow::bail!("fan targets must be multiples of 100 RPM (0x2E wire unit)");
            }
            println!("--- HP write-transport spike (S2) ---");
            let report = phelper_core::smoke::hp_write_spike(cpu / 100, gpu / 100)?;
            print!("{report}");
            Ok(())
        }
        #[cfg(feature = "experimental")]
        Cmd::PowerSpike { order, pl1, pl2 } => {
            if pl1 == pl2 {
                anyhow::bail!("--pl1 must differ from --pl2 (asymmetric limits arbitrate the byte order)");
            }
            if !(15..=130).contains(&pl1) {
                anyhow::bail!("--pl1 {pl1}W out of sane envelope 15..=130");
            }
            if !(15..=157).contains(&pl2) {
                anyhow::bail!("--pl2 {pl2}W out of sane envelope 15..=157");
            }
            if pl2 < pl1 {
                anyhow::bail!("--pl2 must be >= --pl1 (kernel-validated invariant)");
            }
            println!("--- 0x29 power-limits arbitration spike (M3 S2) ---");
            let report = phelper_core::smoke::power_limits_spike(
                matches!(order, SpikeOrder::Kernel),
                pl1,
                pl2,
            )?;
            print!("{report}");
            Ok(())
        }
    }
}
