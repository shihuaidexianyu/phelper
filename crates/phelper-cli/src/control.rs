//! `phelper-cli control` — M2 control-plane dev/verification commands.
//!
//! Every mutating command prints §56 evidence: BEFORE (observed state),
//! COMMAND (the dispatched ControlCommand), AFTER (outcome + fresh observed).
//!
//! Hold semantics: HP-state commands (thermal / fan) default to `--hold 120`
//! — the process stays alive so the coordinator's KeepAliveService heartbeat
//! stops the firmware from clawing the state back. Ctrl+C (or hold expiry)
//! triggers graceful engine shutdown, which restores firmware auto (AR-12).
//! `--hold 0` = fire-and-exit WITHOUT restore: the write stands and the
//! firmware clawback (~120 s, heartbeat stops with the process) is the
//! safety net — that path is exactly what HIL step 10 proves with taskkill.
//! PPM commands (epp / epp1 / max-freq / min-perf / max-perf / boost) are
//! Windows-native settings: they persist across process exit and need no hold.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use phelper_core::domain::capability::{CapabilitySet, Support};
use phelper_core::domain::command::{
    ControlCommand, ControlOutcome, ControlStatus, StepOutcome, Verification,
};
use phelper_core::domain::policy::{
    BoostPolicy, CpuPolicy, FanLevels, FanMode, GpuPlatformPolicy, ThermalMode,
};
use phelper_core::domain::state::ObservedState;
use phelper_core::{Engine, control::journal::ControlJournal};

use crate::ctrlc_flag;

#[derive(Args)]
pub struct ControlArgs {
    #[command(subcommand)]
    cmd: ControlCmd,
}

#[derive(Subcommand)]
enum ControlCmd {
    /// Capability surface + observed state + OGH findings + journal tail.
    Status,
    /// Set CPU Energy Performance Preference (0-100; 0 = max performance).
    Epp {
        /// AC value (percent).
        #[arg(long)]
        ac: Option<u8>,
        /// DC (battery) value (percent).
        #[arg(long)]
        dc: Option<u8>,
    },
    /// Set class-1 (E-core) EPP via PERFEPP1 (0-100; 0 = max performance).
    Epp1 {
        /// AC value (percent).
        #[arg(long)]
        ac: Option<u8>,
        /// DC (battery) value (percent).
        #[arg(long)]
        dc: Option<u8>,
    },
    /// Set CPU maximum frequency (MHz; 0 = no limit).
    MaxFreq {
        /// AC limit in MHz (0 = unlimited).
        #[arg(long)]
        ac: Option<u32>,
        /// DC limit in MHz (0 = unlimited).
        #[arg(long)]
        dc: Option<u32>,
    },
    /// Set PPM minimum processor performance percentage (0..=100).
    MinPerf {
        #[arg(long)]
        ac: Option<u8>,
        #[arg(long)]
        dc: Option<u8>,
    },
    /// Set PPM maximum processor performance percentage (0..=100).
    MaxPerf {
        #[arg(long)]
        ac: Option<u8>,
        #[arg(long)]
        dc: Option<u8>,
    },
    /// Set Windows turbo boost policy (PERFBOOSTMODE).
    Boost {
        /// Legacy shorthand: set both AC and DC to this value.
        #[arg(value_enum, required_unless_present_any = ["ac", "dc"])]
        mode: Option<BoostArg>,
        /// AC-only boost policy.
        #[arg(long, value_enum)]
        ac: Option<BoostArg>,
        /// Battery-only boost policy.
        #[arg(long, value_enum)]
        dc: Option<BoostArg>,
    },
    /// Set HP thermal mode (0x1A). Hold keeps the heartbeat alive.
    Thermal {
        #[arg(value_enum)]
        mode: ThermalArg,
        /// Seconds to keep the process (and heartbeat) alive; 0 = fire and
        /// exit WITHOUT restore (firmware clawback ~120 s is the net).
        #[arg(long, default_value_t = 120)]
        hold: u64,
    },
    /// Fan control (0x2E / 0x27). Hold keeps the heartbeat alive.
    Fan {
        #[command(subcommand)]
        mode: FanCmd,
    },
    /// Set CPU power limits PL1/PL2[/PL4] (0x29, EXPERIMENTAL — only in
    /// `--features experimental` builds; byte order S2-arbitrated on 8BAB,
    /// byte2=PL4 settled by the M4-mini MCHBAR spike). PL4 is optional;
    /// omitted = wire 0xFF NO_CHANGE. cpu_gpu_concurrent stays permanently
    /// rejected (no readback, no restore semantics). Hold keeps the
    /// heartbeat alive (AC/DC transitions can drop custom limits).
    #[cfg(feature = "experimental")]
    PowerLimits {
        /// PL1 (sustained) watts, 15..=130.
        #[arg(long)]
        pl1: u8,
        /// PL2 (turbo) watts, 15..=157, must be >= PL1.
        #[arg(long)]
        pl2: u8,
        /// PL4 (peak protection) watts, 30..=200 (factory ceiling, SDD
        /// byte5 — a protection limit is never raised above factory).
        /// Verified via MCHBAR 0x59B0 readback.
        #[arg(long)]
        pl4: Option<u8>,
        #[arg(long, default_value_t = 120)]
        hold: u64,
    },
    /// Performance profiles (§36 built-ins + user TOML in
    /// %LOCALAPPDATA%\phelper\profiles\*.toml).
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
    /// Set GPU platform policy (0x22: cTGP / PPAB / dstate / slowdown temp).
    /// Unspecified fields keep their current 0x21-readback values
    /// (read-modify-write; slowdown temp is preserved by default).
    GpuPolicy {
        /// Configurable TGP on/off.
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        ctgp: Option<bool>,
        /// PPAB (power budget balancing) on/off.
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        ppab: Option<bool>,
        /// GPU power state: 1=100%, 2=50%, 3=25%, 4=12.5%. NOTE (M5 HIL,
        /// 2026-08-26): dstate writes are INEFFECTIVE on 8BAB — 0x21 keeps
        /// reading the firmware's own live value (seen 1 and 3 on the same
        /// BIOS F.30, same day, unaffected by our writes); verification
        /// will honestly report Failed when the readback disagrees.
        #[arg(long)]
        dstate: Option<u8>,
        /// GPU slowdown temperature threshold (°C).
        #[arg(long)]
        slowdown_temp: Option<u8>,
        #[arg(long, default_value_t = 120)]
        hold: u64,
    },
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// List built-in and user profiles (no engine start).
    List,
    /// Show one profile's full definition (no engine start).
    Show {
        /// Profile name (see `profile list`).
        name: String,
    },
    /// Print a profile as TOML on stdout — a template for your own files
    /// (redirect into %LOCALAPPDATA%\phelper\profiles\<name>.toml).
    Export {
        /// Profile name (see `profile list`).
        name: String,
    },
    /// Apply a profile: validated whole, then executed as an ordered
    /// multi-step plan (PPM → power limits → GPU policy → thermal → fan).
    /// Hold keeps the heartbeat alive; exit restores HP-state (AR-12).
    Apply {
        /// Profile name (see `profile list`).
        name: String,
        #[arg(long, default_value_t = 120)]
        hold: u64,
    },
}

#[derive(Subcommand)]
enum FanCmd {
    /// Restore firmware-automatic fan control (0x2E {0,0} + 0x27 off).
    Auto,
    /// Max fan toggle (0x27).
    Max {
        /// Turn max fan on.
        #[arg(long, conflicts_with = "off", required_unless_present = "off")]
        on: bool,
        /// Turn max fan off.
        #[arg(long)]
        off: bool,
        #[arg(long, default_value_t = 120)]
        hold: u64,
    },
    /// Manual fan levels (0x2E), RPM multiples of 100.
    Manual {
        /// CPU fan target RPM (multiple of 100).
        #[arg(long)]
        cpu: u16,
        /// GPU fan target RPM (multiple of 100).
        #[arg(long)]
        gpu: u16,
        #[arg(long, default_value_t = 120)]
        hold: u64,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ThermalArg {
    Balanced,
    Performance,
}

impl From<ThermalArg> for ThermalMode {
    fn from(a: ThermalArg) -> Self {
        match a {
            ThermalArg::Balanced => ThermalMode::Balanced,
            ThermalArg::Performance => ThermalMode::Performance,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum BoostArg {
    Disabled,
    Enabled,
    Aggressive,
    EfficientEnabled,
    EfficientAggressive,
    AggressiveGuaranteed,
    EfficientAggressiveGuaranteed,
}

impl From<BoostArg> for BoostPolicy {
    fn from(a: BoostArg) -> Self {
        match a {
            BoostArg::Disabled => BoostPolicy::Disabled,
            BoostArg::Enabled => BoostPolicy::Enabled,
            BoostArg::Aggressive => BoostPolicy::Aggressive,
            BoostArg::EfficientEnabled => BoostPolicy::EfficientEnabled,
            BoostArg::EfficientAggressive => BoostPolicy::EfficientAggressive,
            BoostArg::AggressiveGuaranteed => BoostPolicy::AggressiveGuaranteed,
            BoostArg::EfficientAggressiveGuaranteed => BoostPolicy::EfficientAggressiveGuaranteed,
        }
    }
}

const DISPATCH_TIMEOUT: Duration = Duration::from_secs(30);

/// What one CLI invocation will do. Produced by pure validation (no engine,
/// no hardware touched) so argument errors reject before anything starts.
enum Plan {
    Status,
    /// Dispatch once, then graceful shutdown (PPM commands, `fan auto`).
    Change(ControlCommand),
    /// Dispatch, hold the process for the heartbeat, then graceful restore.
    HpState(ControlCommand, u64),
    /// 0x22 read-modify-write: partial fields, merged with the live 0x21
    /// readback after engine start, then treated as HpState.
    GpuPolicyMerge {
        ctgp: Option<bool>,
        ppab: Option<bool>,
        dstate: Option<u8>,
        slowdown_temp: Option<u8>,
        hold: u64,
    },
    /// Profile actions that never touch hardware (no engine start).
    ProfileList,
    ProfileShow(String),
    ProfileExport(String),
    /// Dispatch ApplyProfile, hold for the heartbeat, graceful restore.
    ProfileApply {
        name: String,
        hold: u64,
    },
}

/// Pure argument → command mapping. All rejections happen HERE, before
/// `Engine::start()` — a rejected command must leave zero hardware trace.
fn plan(args: &ControlArgs) -> Result<Plan> {
    Ok(match &args.cmd {
        ControlCmd::Status => Plan::Status,
        ControlCmd::Epp { ac, dc } => {
            if ac.is_none() && dc.is_none() {
                bail!("nothing to do: pass --ac and/or --dc");
            }
            Plan::Change(ControlCommand::SetCpuPolicy(CpuPolicy {
                epp_ac: *ac,
                epp_dc: *dc,
                ..CpuPolicy::default()
            }))
        }
        ControlCmd::Epp1 { ac, dc } => {
            if ac.is_none() && dc.is_none() {
                bail!("nothing to do: pass --ac and/or --dc");
            }
            Plan::Change(ControlCommand::SetCpuPolicy(CpuPolicy {
                epp1_ac: *ac,
                epp1_dc: *dc,
                ..CpuPolicy::default()
            }))
        }
        ControlCmd::MaxFreq { ac, dc } => {
            if ac.is_none() && dc.is_none() {
                bail!("nothing to do: pass --ac and/or --dc");
            }
            for (side, v) in [("ac", *ac), ("dc", *dc)] {
                if let Some(mhz) = v
                    && mhz != 0
                    && !(400..=6000).contains(&mhz)
                {
                    bail!("--{side}: {mhz} MHz out of sane range (0 = unlimited, else 400..=6000)");
                }
            }
            Plan::Change(ControlCommand::SetCpuPolicy(CpuPolicy {
                max_freq_mhz_ac: *ac,
                max_freq_mhz_dc: *dc,
                ..CpuPolicy::default()
            }))
        }
        ControlCmd::MinPerf { ac, dc } => {
            if ac.is_none() && dc.is_none() {
                bail!("nothing to do: pass --ac and/or --dc");
            }
            Plan::Change(ControlCommand::SetCpuPolicy(CpuPolicy {
                min_performance_ac: *ac,
                min_performance_dc: *dc,
                ..CpuPolicy::default()
            }))
        }
        ControlCmd::MaxPerf { ac, dc } => {
            if ac.is_none() && dc.is_none() {
                bail!("nothing to do: pass --ac and/or --dc");
            }
            Plan::Change(ControlCommand::SetCpuPolicy(CpuPolicy {
                max_performance_ac: *ac,
                max_performance_dc: *dc,
                ..CpuPolicy::default()
            }))
        }
        ControlCmd::Boost { mode, ac, dc } => {
            if mode.is_none() && ac.is_none() && dc.is_none() {
                bail!("nothing to do: pass MODE and/or --ac/--dc");
            }
            let legacy = mode.map(Into::into);
            Plan::Change(ControlCommand::SetCpuPolicy(CpuPolicy {
                boost_policy: legacy,
                boost_policy_ac: ac.map(Into::into),
                boost_policy_dc: dc.map(Into::into),
                ..CpuPolicy::default()
            }))
        }
        ControlCmd::Thermal { mode, hold } => {
            Plan::HpState(ControlCommand::SetThermalMode((*mode).into()), *hold)
        }
        #[cfg(feature = "experimental")]
        ControlCmd::PowerLimits {
            pl1,
            pl2,
            pl4,
            hold,
        } => {
            if !(15..=130).contains(pl1) {
                bail!("--pl1: {pl1}W out of 13900HX envelope 15..=130");
            }
            if !(15..=157).contains(pl2) {
                bail!("--pl2: {pl2}W out of 13900HX envelope 15..=157");
            }
            if pl2 < pl1 {
                bail!("--pl2 must be >= --pl1 (kernel-validated invariant)");
            }
            if let Some(p4) = pl4
                && !(30..=200).contains(p4)
            {
                bail!("--pl4: {p4}W outside envelope 30..=200 (factory ceiling, SDD byte5)");
            }
            Plan::HpState(
                ControlCommand::SetPowerLimits(phelper_core::domain::policy::CpuPowerLimits {
                    pl1_w: *pl1,
                    pl2_w: *pl2,
                    pl4_w: pl4.unwrap_or(0),
                    cpu_gpu_concurrent_w: 0,
                }),
                *hold,
            )
        }
        ControlCmd::GpuPolicy {
            ctgp,
            ppab,
            dstate,
            slowdown_temp,
            hold,
        } => {
            if ctgp.is_none() && ppab.is_none() && dstate.is_none() && slowdown_temp.is_none() {
                bail!("nothing to do: pass at least one of --ctgp/--ppab/--dstate/--slowdown-temp");
            }
            if let Some(d) = dstate
                && !(1..=4).contains(d)
            {
                bail!("--dstate: {d} out of range 1..=4 (100/50/25/12.5%)");
            }
            if let Some(t) = slowdown_temp
                && !(30..=110).contains(t)
            {
                bail!("--slowdown-temp: {t} outside plausible band 30..=110 °C");
            }
            Plan::GpuPolicyMerge {
                ctgp: *ctgp,
                ppab: *ppab,
                dstate: *dstate,
                slowdown_temp: *slowdown_temp,
                hold: *hold,
            }
        }
        ControlCmd::Fan { mode } => match mode {
            // Restoring the default needs no heartbeat: shutdown's restore
            // sequence converges to the same state.
            FanCmd::Auto => Plan::Change(ControlCommand::SetFanMode(FanMode::FirmwareAuto)),
            FanCmd::Max { on, hold, .. } => Plan::HpState(
                ControlCommand::SetFanMode(if *on {
                    FanMode::Max
                } else {
                    FanMode::FirmwareAuto
                }),
                *hold,
            ),
            FanCmd::Manual { cpu, gpu, hold } => {
                if cpu % 100 != 0 || gpu % 100 != 0 {
                    bail!("fan targets must be multiples of 100 RPM (0x2E wire unit)");
                }
                if *cpu == 0 || *gpu == 0 {
                    bail!(
                        "manual levels must be nonzero on both channels \
                         (0 = firmware-auto — use `fan auto`)"
                    );
                }
                Plan::HpState(
                    ControlCommand::SetFanMode(FanMode::Manual(FanLevels::new(
                        cpu / 100,
                        gpu / 100,
                    ))),
                    *hold,
                )
            }
        },
        ControlCmd::Profile { cmd } => match cmd {
            ProfileCmd::List => Plan::ProfileList,
            ProfileCmd::Show { name } => Plan::ProfileShow(name.clone()),
            ProfileCmd::Export { name } => Plan::ProfileExport(name.clone()),
            ProfileCmd::Apply { name, hold } => {
                // Everything rejectable is rejected HERE, before Engine::start.
                let registry = phelper_core::profiles::ProfileRegistry::load_default();
                let Some(profile) = registry.get(name) else {
                    let known: Vec<&str> = registry.iter().map(|(n, _, _)| n).collect();
                    bail!("unknown profile '{name}' (available: {})", known.join(", "));
                };
                if profile.cpu.power_limits.is_some() {
                    bail!(
                        "profile '{name}' sets cpu.power_limits — that route is R8-poisoned \
                         (0x29 never rides a CpuPolicy batch); use the top-level \
                         `power_limits` field instead"
                    );
                }
                if profile.power_limits.is_some() && !cfg!(feature = "experimental") {
                    bail!(
                        "profile '{name}' carries power_limits (0x29, EXPERIMENTAL) but this \
                         build lacks the feature — rebuild with `--features experimental` \
                         (double gate: feature + Experimental caps)"
                    );
                }
                if profile.os_policy.is_some() {
                    bail!(
                        "profile '{name}' carries os_policy; hardware `profile apply` does not \
                         guess a process target — use `os apply --profile {name} --pid <PID>`"
                    );
                }
                Plan::ProfileApply {
                    name: name.clone(),
                    hold: *hold,
                }
            }
        },
    })
}

pub fn run(args: ControlArgs) -> Result<()> {
    let plan = plan(&args)?;
    // No-engine plans first: list/show/export never touch hardware.
    match plan {
        Plan::ProfileList => return profile_list(),
        Plan::ProfileShow(name) => return profile_show(&name),
        Plan::ProfileExport(name) => return profile_export(&name),
        _ => {}
    }
    let engine = Engine::start().context("engine start")?;
    match plan {
        Plan::Status => {
            let r = status(&engine);
            engine.shutdown();
            r
        }
        Plan::Change(cmd) => {
            let r = change(&engine, cmd);
            engine.shutdown();
            r
        }
        Plan::HpState(cmd, hold) => run_hp_state(engine, cmd, hold),
        Plan::ProfileApply { name, hold } => {
            run_hp_state(engine, ControlCommand::ApplyProfile { profile: name }, hold)
        }
        Plan::ProfileList | Plan::ProfileShow(_) | Plan::ProfileExport(_) => {
            unreachable!("handled before engine start")
        }
        Plan::GpuPolicyMerge {
            ctgp,
            ppab,
            dstate,
            slowdown_temp,
            hold,
        } => {
            // Read-modify-write merge against the live 0x21 readback (the
            // coordinator populated observed.gpu_platform_policy at start).
            let merged = {
                let control = engine.control().context(
                    "control unavailable (engine is telemetry-only — see startup warnings)",
                )?;
                let cur = control
                    .observed()
                    .gpu_platform_policy
                    .value()
                    .copied()
                    .context(
                        "0x21 readback unavailable — cannot preserve unspecified fields \
                         (gpu_platform_policy observed is Unknown)",
                    )?;
                GpuPlatformPolicy {
                    ctgp: ctgp.unwrap_or(cur.ctgp),
                    ppab: ppab.unwrap_or(cur.ppab),
                    dstate: dstate.unwrap_or(cur.dstate),
                    slowdown_temp_c: slowdown_temp.unwrap_or(cur.slowdown_temp_c),
                }
            };
            run_hp_state(engine, ControlCommand::SetGpuPlatformPolicy(merged), hold)
        }
    }
}

fn profile_list() -> Result<()> {
    let registry = phelper_core::profiles::ProfileRegistry::load_default();
    println!(
        "profiles (user dir: {}):",
        phelper_core::profiles::profiles_dir().display()
    );
    for (name, p, builtin) in registry.iter() {
        let tag = if builtin { "built-in" } else { "user    " };
        let mut touches: Vec<&str> = Vec::new();
        if p.cpu != Default::default() {
            touches.push("ppm");
        }
        if p.power_limits.is_some() {
            touches.push("0x29!");
        }
        if p.gpu_policy.is_some() {
            touches.push("gpu");
        }
        if p.thermal_mode.is_some() {
            touches.push("thermal");
        }
        if p.fan.is_some() {
            touches.push("fan");
        }
        if p.os_policy.is_some() {
            touches.push("os");
        }
        println!(
            "  {name:<12} [{tag}] {:<24} {}",
            touches.join("+"),
            p.description
        );
    }
    for w in &registry.warnings {
        eprintln!("  warning: {w}");
    }
    Ok(())
}

fn profile_show(name: &str) -> Result<()> {
    let registry = phelper_core::profiles::ProfileRegistry::load_default();
    let Some(p) = registry.get(name) else {
        let known: Vec<&str> = registry.iter().map(|(n, _, _)| n).collect();
        bail!("unknown profile '{name}' (available: {})", known.join(", "));
    };
    let kind = if registry.is_builtin(name) {
        "built-in"
    } else {
        "user"
    };
    println!("profile '{name}' [{kind}]: {}", p.description);
    println!("{p:#?}");
    Ok(())
}

fn profile_export(name: &str) -> Result<()> {
    let registry = phelper_core::profiles::ProfileRegistry::load_default();
    let Some(p) = registry.get(name) else {
        let known: Vec<&str> = registry.iter().map(|(n, _, _)| n).collect();
        bail!("unknown profile '{name}' (available: {})", known.join(", "));
    };
    print!("{}", phelper_core::profiles::to_toml(p)?);
    Ok(())
}

/// HP-state change + optional heartbeat hold + graceful restore. The engine
/// is shut down (restoring firmware auto, AR-12) even when the change or
/// the hold fails — the only path that skips restore is the deliberate
/// `--hold 0` process exit.
fn run_hp_state(engine: Engine, cmd: ControlCommand, hold: u64) -> Result<()> {
    let r = change(&engine, cmd).and_then(|()| {
        if hold == 0 {
            eprintln!(
                "\n*** exiting WITHOUT restore (--hold 0): the write stands, \
                 heartbeat stops with this process, and the firmware clawback \
                 (~120 s) returns fans/thermal to automatic (AR-12) ***"
            );
            // Deliberately skip Engine::shutdown — its restore sequence
            // would undo the write immediately, which is not what --hold 0
            // means. The clawback is the documented safety net here.
            std::process::exit(0);
        }
        hold_loop(hold)
    });
    engine.shutdown();
    if r.is_ok() {
        eprintln!("engine stopped; firmware automatic state restored");
    }
    r
}

fn hold_loop(hold_secs: u64) -> Result<()> {
    let stop = ctrlc_flag()?;
    let deadline = Instant::now() + Duration::from_secs(hold_secs);
    eprintln!("\nholding {hold_secs} s (KeepAlive heartbeat active; Ctrl+C = graceful restore)…");
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now >= deadline {
            eprintln!("hold elapsed — restoring firmware auto");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200).min(deadline - now));
    }
    eprintln!("Ctrl+C — restoring firmware auto");
    Ok(())
}

/// §56 evidence wrapper: BEFORE / COMMAND / AFTER around one dispatch.
fn change(engine: &Engine, cmd: ControlCommand) -> Result<()> {
    let control = engine
        .control()
        .context("control unavailable (engine is telemetry-only — see startup warnings)")?;

    println!("--- BEFORE (observed state) ---");
    print_observed(&control.observed());

    println!("\n--- COMMAND ---");
    println!("{cmd:?}");

    let outcome = control
        .dispatch_blocking(cmd, DISPATCH_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("dispatch failed: {e}"))?;
    print_outcome(&outcome);

    println!("\n--- AFTER (observed state) ---");
    print_observed(&control.observed());
    Ok(())
}

fn status(engine: &Engine) -> Result<()> {
    let id = engine.identity();
    println!(
        "board {} | BIOS {} | {}",
        id.board_id, id.bios_version, id.product_name
    );

    match engine.control() {
        Some(control) => {
            println!("\n--- capabilities ---");
            print_capabilities(control.capabilities());
            println!("\n--- desired state ---");
            println!("{:?}", control.desired());
            println!("\n--- observed state ---");
            print_observed(&control.observed());
            println!("\n--- windows software policy ---");
            print_windows_ppm(control.windows_ppm_state().as_ref());
        }
        None => println!("\ncontrol: UNAVAILABLE (telemetry-only engine — see startup warnings)"),
    }

    println!("\n--- OGH / second-writer scan ---");
    let findings = engine.ogh_findings();
    if findings.is_empty() {
        println!("no findings (no OGH processes, services, or packages)");
    } else {
        for f in findings {
            println!("  {f}");
        }
    }

    println!("\n--- journal tail ---");
    print_journal_tail(10);
    Ok(())
}

fn print_capabilities(caps: &CapabilitySet) {
    let row = |name: &str, s: Support| println!("  {name:<22} {s:?}");
    println!("  known_board            {}", caps.known_board);
    row("thermal_mode (0x1A)", caps.thermal_mode);
    row("fan_rpm_read (0x2D)", caps.fan_rpm_read);
    row("fan_manual (0x2E)", caps.fan_manual_level);
    row("max_fan (0x27)", caps.max_fan);
    row("gpu_policy (0x21/22)", caps.gpu_platform_policy);
    row("mux (0x52)", caps.mux);
    row("power_limits (0x29)", caps.power_limits);
    row("ppm.epp", caps.ppm.epp);
    row("ppm.epp1", caps.ppm.epp1);
    row("ppm.max_freq", caps.ppm.max_freq);
    row("ppm.boost", caps.ppm.boost);
    row("ppm.min_performance", caps.ppm.min_performance);
    row("ppm.max_performance", caps.ppm.max_performance);
    println!("  ppm.write_privileged   {}", caps.ppm.write_privileged);
    println!(
        "  fan: count={} scale={:?} clamp={:?}..={:?} sw_declared={}",
        caps.fan.count,
        caps.fan.scale,
        caps.fan.clamp_min,
        caps.fan.clamp_max,
        caps.fan.sw_control_declared
    );
    for note in &caps.notes {
        println!("  note: {note}");
    }
}

fn print_observed(obs: &ObservedState) {
    println!("  thermal_mode: {}", fmt_obs(&obs.thermal_mode));
    println!("  fan_mode:     {}", fmt_obs(&obs.fan_mode));
    println!("  max_fan:      {}", fmt_obs(&obs.max_fan));
    println!("  epp_ac:       {}", fmt_obs(&obs.epp_ac));
    println!("  epp_dc:       {}", fmt_obs(&obs.epp_dc));
    println!("  epp1_ac:      {}", fmt_obs(&obs.epp1_ac));
    println!("  epp1_dc:      {}", fmt_obs(&obs.epp1_dc));
    println!("  max_freq_ac:  {}", fmt_obs(&obs.max_freq_ac));
    println!("  max_freq_dc:  {}", fmt_obs(&obs.max_freq_dc));
    println!("  boost_ac:     {}", fmt_obs(&obs.boost_ac));
    println!("  boost_dc:     {}", fmt_obs(&obs.boost_dc));
    println!("  min_perf_ac:  {}", fmt_obs(&obs.min_performance_ac));
    println!("  min_perf_dc:  {}", fmt_obs(&obs.min_performance_dc));
    println!("  max_perf_ac:  {}", fmt_obs(&obs.max_performance_ac));
    println!("  max_perf_dc:  {}", fmt_obs(&obs.max_performance_dc));
    println!("  gpu_policy:   {}", fmt_obs(&obs.gpu_platform_policy));
    println!("  mux:          {}", fmt_obs(&obs.mux));
    println!("  power_limits: {}", fmt_obs(&obs.power_limits));
}

fn print_windows_ppm(state: Option<&phelper_core::domain::policy::WindowsPpmState>) {
    let Some(state) = state else {
        println!("  unavailable");
        return;
    };
    println!(
        "  scheme: {} ({})",
        state.active_scheme_name, state.active_scheme_guid
    );
    println!(
        "  configured: ac={:?} dc={:?} effective={:?}",
        state.configured_ac_mode, state.configured_dc_mode, state.effective_mode
    );
    println!("  ac: {:?}", state.ac);
    println!("  dc: {:?}", state.dc);
}

fn fmt_obs<T: std::fmt::Debug>(v: &phelper_core::domain::state::ObservedValue<T>) -> String {
    use phelper_core::domain::state::ObservedValue as O;
    match v {
        O::Verified { value, at, source } => format!(
            "VERIFIED {value:?} (via {source}, {:.1}s ago)",
            at.elapsed().as_secs_f64()
        ),
        O::TrustedWrite { value, at } => format!(
            "trusted-write {value:?} ({:.1}s ago, keep-alive maintained)",
            at.elapsed().as_secs_f64()
        ),
        O::Unknown => "unknown".into(),
    }
}

fn print_outcome(o: &ControlOutcome) {
    println!(
        "\n--- OUTCOME (receipt {}, {} ms) ---",
        o.receipt.0,
        o.duration.as_millis()
    );
    match &o.status {
        ControlStatus::Applied { verification } => {
            println!("status: APPLIED ({})", fmt_verification(verification));
        }
        ControlStatus::Rejected { error } => println!("status: REJECTED — {error}"),
        ControlStatus::Partial => println!("status: PARTIAL — some steps applied, see below"),
    }
    for s in &o.steps {
        print_step(s);
    }
}

fn print_step(s: &StepOutcome) {
    println!("  step: {} via {}", s.step, s.backend);
    if let Some(fr) = &s.firmware_return {
        println!("    firmware return: {fr}");
    }
    if let Some(b) = &s.before {
        println!("    before: {b}");
    }
    if let Some(a) = &s.after {
        println!("    after:  {a}");
    }
    println!("    verification: {}", fmt_verification(&s.verification));
}

fn fmt_verification(v: &Verification) -> String {
    match v {
        Verification::Verified => "verified".into(),
        Verification::TrustedNoReadback => "trusted (no readback exists)".into(),
        Verification::Failed { expected, actual } => {
            format!("FAILED (expected {expected}, actual {actual})")
        }
        Verification::Skipped => "skipped".into(),
    }
}

fn print_journal_tail(n: usize) {
    let path = ControlJournal::default_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        println!("  (no journal yet at {})", path.display());
        return;
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    if start > 0 {
        println!("  ({} earlier entries omitted)", start);
    }
    for line in &lines[start..] {
        // Journal lines are self-contained JSONL (§56) — print raw.
        println!("  {line}");
    }
}
