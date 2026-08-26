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
        Cmd::HpSpike { cpu, gpu } => {
            if cpu % 100 != 0 || gpu % 100 != 0 {
                anyhow::bail!("fan targets must be multiples of 100 RPM (0x2E wire unit)");
            }
            println!("--- HP write-transport spike (S2) ---");
            let report = phelper_core::smoke::hp_write_spike(cpu / 100, gpu / 100)?;
            print!("{report}");
            Ok(())
        }
    }
}
