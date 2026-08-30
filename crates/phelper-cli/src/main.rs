mod control;
mod gpuload;
mod os_policy;
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
    /// Control plane: Windows PPM parameters and HP WMI thermal/fan controls
    /// with §56 before/command/after evidence.
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
    /// Windows process/thread scheduling controls (CPU Sets, QoS, priority,
    /// memory priority and graphics preference).
    Os(os_policy::OsPolicyArgs),
    /// DEV ONLY (§57 Stage 1, M4-mini): READ-ONLY MCHBAR cross-check probe.
    /// Zero writes. Cross-validates the PL4 readback channel: MMIO 0x59A0
    /// vs MSR 0x610, then sweeps the SA power block for the factory PL4
    /// (SDD 0x28 byte5 = 200 W). Prerequisite for any future PL4 write.
    MchbarProbe,
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
        Cmd::MchbarProbe => {
            println!("--- MCHBAR read-only cross-check probe (M4-mini) ---");
            let report = phelper_core::smoke::mchbar_probe()?;
            print!("{report}");
            Ok(())
        }
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
        Cmd::Os(args) => os_policy::run(args),
    }
}
