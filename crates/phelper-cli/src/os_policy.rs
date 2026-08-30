//! `phelper-cli os` — Windows process/thread scheduling controls.
//!
//! This command intentionally does not start the hardware engine.  OS policy
//! is a separate layer and can be tested or used even when HP/WMI telemetry
//! is unavailable.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use phelper_core::automatic_scheduler::AutomaticSchedulerHandle;
use phelper_core::os_policy::{
    AffinityMask, CpuPlacement, GpuPreference, MemoryPriority, OsPolicyHandle, OsPolicyTarget,
    OsSchedulingPolicy, ProcessPriority, ProcessorRef, QosLevel, ThreadPriority,
};

use crate::ctrlc_flag;

#[derive(Args)]
pub struct OsPolicyArgs {
    #[command(subcommand)]
    cmd: OsPolicyCmd,
}

#[derive(Subcommand)]
enum OsPolicyCmd {
    /// List the CPU Sets Windows exposes, including the P/E grouping used by
    /// phelper.
    Topology,
    /// List running processes.  Protected processes may omit their path.
    Processes,
    /// Apply one or more OS policies to a running process or thread.
    Apply(ApplyArgs),
    /// Inspect or run the explicit power-aware automatic scheduler.
    Auto(AutoArgs),
}

#[derive(Args)]
struct AutoArgs {
    #[command(subcommand)]
    cmd: AutoCmd,
}

#[derive(Subcommand)]
enum AutoCmd {
    /// Print the current Windows power context and idle scheduler state.
    Status,
    /// Run BatteryEfficiency for a bounded verification session.  The
    /// policy is restored when the command exits.
    Battery(AutoHoldArgs),
}

#[derive(Args)]
struct AutoHoldArgs {
    /// Keep the automatic mode for N seconds.  Zero waits for Ctrl+C.
    #[arg(long, default_value_t = 120)]
    hold: u64,
}

#[derive(Args)]
struct ApplyArgs {
    #[arg(long, conflicts_with = "tid", required_unless_present = "tid")]
    pid: Option<u32>,
    #[arg(long, conflicts_with = "pid", required_unless_present = "pid")]
    tid: Option<u32>,
    /// Use an OS policy from a user/built-in profile instead of individual
    /// flags.  The profile's OS policy still needs an explicit target.
    #[arg(long, conflicts_with_all = ["cpu", "cpu_set", "affinity_group", "affinity_mask", "qos", "process_priority", "thread_priority", "memory_priority", "ideal_group", "ideal_number", "gpu"])]
    profile: Option<String>,
    /// CPU placement: all, performance (P), or efficiency (E).
    #[arg(long, value_enum)]
    cpu: Option<CpuArg>,
    /// Explicit comma-delimited Windows CPU Set IDs, for example 0,2,4.
    #[arg(long, value_delimiter = ',')]
    cpu_set: Option<Vec<u32>>,
    #[arg(long)]
    affinity_group: Option<u16>,
    /// Affinity mask; decimal or 0x-prefixed hexadecimal.
    #[arg(long, value_parser = parse_u64)]
    affinity_mask: Option<u64>,
    #[arg(long, value_enum)]
    qos: Option<QosArg>,
    #[arg(long, value_enum)]
    process_priority: Option<ProcessPriorityArg>,
    #[arg(long, value_enum)]
    thread_priority: Option<ThreadPriorityArg>,
    #[arg(long, value_enum)]
    memory_priority: Option<MemoryPriorityArg>,
    #[arg(long)]
    ideal_group: Option<u16>,
    #[arg(long)]
    ideal_number: Option<u8>,
    #[arg(long, value_enum)]
    gpu: Option<GpuArg>,
    /// Keep the target policy alive in this process for N seconds.  Zero
    /// waits for Ctrl+C, which is useful for manual verification.
    #[arg(long, default_value_t = 120)]
    hold: u64,
}

#[derive(Clone, Copy, ValueEnum)]
enum CpuArg {
    All,
    Performance,
    Efficiency,
}

#[derive(Clone, Copy, ValueEnum)]
enum QosArg {
    System,
    High,
    Eco,
}

#[derive(Clone, Copy, ValueEnum)]
enum ProcessPriorityArg {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
}

#[derive(Clone, Copy, ValueEnum)]
enum ThreadPriorityArg {
    Idle,
    Lowest,
    BelowNormal,
    Normal,
    AboveNormal,
    Highest,
}

#[derive(Clone, Copy, ValueEnum)]
enum MemoryPriorityArg {
    VeryLow,
    Low,
    Medium,
    BelowNormal,
    Normal,
}

#[derive(Clone, Copy, ValueEnum)]
enum GpuArg {
    System,
    PowerSaving,
    HighPerformance,
}

fn parse_u64(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if let Some(value) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u64::from_str_radix(value, 16).map_err(|e| e.to_string())
    } else {
        value.parse::<u64>().map_err(|e| e.to_string())
    }
}

fn target(pid: Option<u32>, tid: Option<u32>) -> Result<OsPolicyTarget> {
    match (pid, tid) {
        (Some(pid), None) => Ok(OsPolicyTarget::Process { pid }),
        (None, Some(tid)) => Ok(OsPolicyTarget::Thread { tid }),
        _ => bail!("exactly one of --pid/--tid is required"),
    }
}

fn policy(args: &ApplyArgs, target: OsPolicyTarget) -> Result<OsSchedulingPolicy> {
    if let (Some(group), Some(mask)) = (args.affinity_group, args.affinity_mask) {
        // handled below
        let _ = (group, mask);
    } else if args.affinity_group.is_some() || args.affinity_mask.is_some() {
        bail!("--affinity-group and --affinity-mask must be provided together");
    }
    if args.ideal_group.is_some() != args.ideal_number.is_some() {
        bail!("--ideal-group and --ideal-number must be provided together");
    }
    let mut policy = if let Some(name) = &args.profile {
        let registry = phelper_core::profiles::ProfileRegistry::load_default();
        let Some(profile) = registry.get(name) else {
            bail!("unknown profile '{name}'");
        };
        profile
            .os_policy
            .clone()
            .context("该配置档没有 os_policy 表")?
    } else {
        OsSchedulingPolicy::default()
    };
    if args.profile.is_none() {
        policy.cpu_placement = args.cpu.map(|cpu| match cpu {
            CpuArg::All => CpuPlacement::All,
            CpuArg::Performance => CpuPlacement::Performance,
            CpuArg::Efficiency => CpuPlacement::Efficiency,
        });
        if let Some(ids) = &args.cpu_set {
            policy.cpu_placement = Some(CpuPlacement::Custom(ids.clone()));
        }
        policy.affinity = args
            .affinity_group
            .zip(args.affinity_mask)
            .map(|(group, mask)| AffinityMask { group, mask });
        policy.qos = args.qos.map(|qos| match qos {
            QosArg::System => QosLevel::System,
            QosArg::High => QosLevel::High,
            QosArg::Eco => QosLevel::Eco,
        });
        policy.process_priority = args.process_priority.map(|priority| match priority {
            ProcessPriorityArg::Idle => ProcessPriority::Idle,
            ProcessPriorityArg::BelowNormal => ProcessPriority::BelowNormal,
            ProcessPriorityArg::Normal => ProcessPriority::Normal,
            ProcessPriorityArg::AboveNormal => ProcessPriority::AboveNormal,
            ProcessPriorityArg::High => ProcessPriority::High,
        });
        policy.thread_priority = args.thread_priority.map(|priority| match priority {
            ThreadPriorityArg::Idle => ThreadPriority::Idle,
            ThreadPriorityArg::Lowest => ThreadPriority::Lowest,
            ThreadPriorityArg::BelowNormal => ThreadPriority::BelowNormal,
            ThreadPriorityArg::Normal => ThreadPriority::Normal,
            ThreadPriorityArg::AboveNormal => ThreadPriority::AboveNormal,
            ThreadPriorityArg::Highest => ThreadPriority::Highest,
        });
        policy.memory_priority = args.memory_priority.map(|priority| match priority {
            MemoryPriorityArg::VeryLow => MemoryPriority::VeryLow,
            MemoryPriorityArg::Low => MemoryPriority::Low,
            MemoryPriorityArg::Medium => MemoryPriority::Medium,
            MemoryPriorityArg::BelowNormal => MemoryPriority::BelowNormal,
            MemoryPriorityArg::Normal => MemoryPriority::Normal,
        });
        policy.ideal_processor = args
            .ideal_group
            .zip(args.ideal_number)
            .map(|(group, number)| ProcessorRef { group, number });
        policy.gpu_preference = args.gpu.map(|gpu| match gpu {
            GpuArg::System => GpuPreference::System,
            GpuArg::PowerSaving => GpuPreference::PowerSaving,
            GpuArg::HighPerformance => GpuPreference::HighPerformance,
        });
    }
    policy
        .validate_for(&target)
        .map_err(|reason| anyhow::anyhow!(reason))?;
    Ok(policy)
}

pub fn run(args: OsPolicyArgs) -> Result<()> {
    let OsPolicyArgs { cmd } = args;
    let handle = OsPolicyHandle::new();
    match cmd {
        OsPolicyCmd::Topology => {
            let topology = handle.topology()?;
            println!("CPU Sets: {}", topology.cpu_sets.len());
            println!("performance (P): {:?}", topology.performance_ids);
            println!("efficiency (E): {:?}", topology.efficiency_ids);
            for cpu in topology.cpu_sets {
                println!(
                    "  id={:<4} group={} lp={} core={} efficiency={}{}",
                    cpu.id,
                    cpu.group,
                    cpu.logical_processor_index,
                    cpu.core_index,
                    cpu.efficiency_class,
                    if cpu.parked { " parked" } else { "" }
                );
            }
            Ok(())
        }
        OsPolicyCmd::Processes => {
            for process in handle.list_processes()? {
                println!(
                    "{:>6} {:>3} {:<28} {}",
                    process.pid,
                    process.thread_count,
                    process.name,
                    process.executable.as_deref().unwrap_or("<protected>")
                );
            }
            Ok(())
        }
        OsPolicyCmd::Apply(args) => {
            let target = target(args.pid, args.tid)?;
            let policy = policy(&args, target)?;
            // Install the graceful-exit path before the first mutation.  If
            // handler installation fails, no process/thread state has yet
            // been changed and therefore no restore obligation is lost.
            let stop = ctrlc_flag()?;
            let result = handle.apply(target, policy)?;
            println!(
                "applied {:?} to {}",
                result.target,
                result.executable.as_deref().unwrap_or("<unknown>")
            );
            if result.gpu_requires_restart {
                println!("GPU preference is a next-launch setting; restart the target process");
            }
            if args.hold == 0 {
                eprintln!("holding until Ctrl+C (policy will be restored on exit)…");
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                }
            } else {
                eprintln!("holding {} s; Ctrl+C restores early…", args.hold);
                let deadline = std::time::Instant::now() + Duration::from_secs(args.hold);
                while !stop.load(std::sync::atomic::Ordering::Relaxed)
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            handle.restore(target)?;
            println!("restored");
            Ok(())
        }
        OsPolicyCmd::Auto(args) => {
            let scheduler = AutomaticSchedulerHandle::start(handle);
            match args.cmd {
                AutoCmd::Status => {
                    // Give the worker one scheduling turn to publish its
                    // initial GetSystemPowerStatus snapshot.
                    std::thread::sleep(Duration::from_millis(150));
                    print_automatic_status(&scheduler);
                }
                AutoCmd::Battery(args) => {
                    scheduler.set_mode(phelper_domain::automatic::AutomaticMode::BatteryEfficiency);
                    let stop = ctrlc_flag()?;
                    if args.hold == 0 {
                        eprintln!("holding BatteryEfficiency until Ctrl+C…");
                        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    } else {
                        let deadline = std::time::Instant::now() + Duration::from_secs(args.hold);
                        while !stop.load(std::sync::atomic::Ordering::Relaxed)
                            && std::time::Instant::now() < deadline
                        {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                    print_automatic_status(&scheduler);
                }
            }
            scheduler.shutdown();
            Ok(())
        }
    }
}

fn print_automatic_status(scheduler: &AutomaticSchedulerHandle) {
    let snapshot = scheduler.snapshot();
    let power = snapshot.power.as_ref().map(|power| {
        format!(
            "{:?} {}%",
            power.source,
            power
                .battery_percent
                .map_or_else(|| "?".to_string(), |value| value.to_string())
        )
    });
    println!(
        "mode={:?} phase={:?} power={} managed={} skipped_manual={}",
        snapshot.mode,
        snapshot.phase,
        power.as_deref().unwrap_or("unknown"),
        snapshot.managed_processes,
        snapshot.skipped_manual
    );
    if let Some(error) = snapshot.last_error {
        eprintln!("automatic scheduler: {error}");
    }
}
