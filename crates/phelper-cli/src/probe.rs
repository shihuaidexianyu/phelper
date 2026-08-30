use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use phelper_core::capability::{CapabilityService, ProbeReport, snapshot};
use phelper_core::persistence;
use phelper_domain::capability::Support;

#[derive(Args)]
pub struct ProbeArgs {
    /// Snapshot JSON output path (default: ./probe-out/capability-<ts>.json).
    #[arg(long)]
    json: Option<PathBuf>,
    /// Also emit the embedded BoardProfile as TOML at this path.
    #[arg(long)]
    emit_board_profile: Option<PathBuf>,
    /// Record raw probe buffers (SDD, fan table) as test fixtures here.
    #[arg(long)]
    record_fixtures: Option<PathBuf>,
}

fn support_str(s: Support) -> &'static str {
    match s {
        Support::Supported => "SUPPORTED",
        Support::Experimental => "EXPERIMENTAL",
        Support::Unsupported => "unsupported",
        Support::NotProbed => "not-probed",
    }
}

pub fn run(args: ProbeArgs) -> Result<()> {
    println!("phelper probe — read-only capability discovery");
    println!("==============================================");
    let report = CapabilityService::probe().context("capability probe failed")?;
    print_report(&report);

    let json_path = args.json.clone().unwrap_or_else(|| {
        snapshot::default_snapshot_path(&persistence::probe_out_dir(), report.taken_at_epoch_ms)
    });
    snapshot::write_snapshot(&report, &json_path)?;
    println!("\nsnapshot written: {}", json_path.display());

    if let Some(path) = &args.emit_board_profile {
        let profile = phelper_core::capability::load_board_profile(&report.identity.board_id)
            .context("no embedded profile for this board")?;
        phelper_core::capability::board_draft::write_board_profile(&profile, path)?;
        println!("board profile written: {}", path.display());
    }

    if let Some(dir) = &args.record_fixtures {
        record_fixtures(&report, dir)?;
    }

    Ok(())
}

fn print_report(r: &ProbeReport) {
    let id = &r.identity;
    println!("\n-- identity --");
    println!("  product : {}", id.product_name);
    println!(
        "  board   : {} {}",
        id.board_id,
        if r.known_board {
            "(KNOWN)"
        } else {
            "(UNKNOWN — read-only)"
        }
    );
    println!("  bios    : {}", id.bios_version);
    println!("  cpu     : {}", id.cpu.name);
    for g in &id.gpu {
        println!("  gpu     : {}", g.name);
    }
    println!("  elevated: {}", r.elevated);

    let c = &r.capabilities;
    println!("\n-- capabilities --");
    println!(
        "  thermal mode (0x1A)     : {}",
        support_str(c.thermal_mode)
    );
    println!(
        "  fan rpm read (0x2D)     : {}",
        support_str(c.fan_rpm_read)
    );
    println!(
        "  fan manual level (0x2E) : {}",
        support_str(c.fan_manual_level)
    );
    println!("  max fan (0x27)          : {}", support_str(c.max_fan));
    println!(
        "  gpu platform (0x21/22)  : {}",
        support_str(c.gpu_platform_policy)
    );
    println!("  mux (0x52)              : {}", support_str(c.mux));
    println!(
        "  power limits (0x29)     : {} (staged verification required)",
        support_str(c.power_limits)
    );
    println!(
        "  fan: count={} scale={:?} clamp={:?}-{:?} sw-declared={}",
        c.fan.count, c.fan.scale, c.fan.clamp_min, c.fan.clamp_max, c.fan.sw_control_declared
    );

    if let Some(sdd) = &r.sdd {
        println!("\n-- system design data (0x28) --");
        println!(
            "  tp_version={} sw_fan={} default_pl4={}W mux_byte=0x{:02x} mux_supported={}",
            sdd.thermal_policy_version,
            sdd.sw_fan_control,
            sdd.default_pl4_w,
            sdd.mux_byte,
            sdd.mux_supported
        );
    }
    if let Some(t) = &r.fan_table {
        println!("\n-- fan table (0x2F) --");
        println!("  num_fans={} entries={}", t.num_fans, t.entries.len());
        for (i, e) in t.entries.iter().enumerate() {
            println!(
                "    [{i}] left={} right={} noise={}dB",
                e.left, e.right, e.noise_db
            );
        }
        println!("  clamp: {:?}", t.clamp_range());
    }
    if let Some(l) = &r.fan_levels {
        println!("\n-- fan levels (0x2D) --");
        println!(
            "  left={} ({} RPM) right={} ({} RPM)",
            l.left,
            l.left_rpm(),
            l.right,
            l.right_rpm()
        );
    }
    if let Some(p) = &r.gpu_platform_policy {
        println!("\n-- gpu platform policy (0x21) --");
        println!(
            "  ctgp={} ppab={} dstate={} slowdown_temp={}C",
            p.ctgp, p.ppab, p.dstate, p.slowdown_temp_c
        );
    }
    if let Some(m) = &r.mux {
        println!("\n-- mux (0x52) --");
        println!("  current: {m:?}");
    }
    if let Some(v) = r.max_fan_diag {
        println!("\n-- 0x26 max fan (DIAGNOSTICS ONLY, unreliable) --");
        println!("  reads: {v}");
    }
    println!("\n-- ppm (PowrProf) --");
    match (r.epp_ac, r.epp_dc) {
        (Some(ac), Some(dc)) => {
            println!("  EPP: AC={ac}% DC={dc}% (0=max perf, 100=max efficiency)")
        }
        _ => println!("  EPP: unreadable"),
    }
    match r.max_freq_mhz {
        Some(0) => println!("  max freq ceiling: unlimited"),
        Some(mhz) => println!("  max freq ceiling: {mhz} MHz"),
        None => println!("  max freq ceiling: unreadable"),
    }

    println!("\n-- provider smoke (read-only) --");
    for row in phelper_core::smoke::run() {
        println!(
            "  {:<18} {:<5} {}",
            row.provider,
            row.status,
            row.detail.unwrap_or_default()
        );
    }

    if !r.notes.is_empty() {
        println!("\n-- notes --");
        for n in &r.notes {
            println!("  * {n}");
        }
    }
}

fn record_fixtures(r: &ProbeReport, dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    if let Some(sdd) = &r.sdd {
        std::fs::write(dir.join("sdd_8bab.bin"), &sdd.raw)?;
    }
    if let Some(t) = &r.fan_table {
        std::fs::write(dir.join("fan_table_8bab.bin"), &t.raw)?;
    }
    let manifest = serde_json::json!({
        "board": r.identity.board_id,
        "bios": r.identity.bios_version,
        "taken_at_epoch_ms": r.taken_at_epoch_ms,
        "files": ["sdd_8bab.bin", "fan_table_8bab.bin"],
    });
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    println!("fixtures recorded: {}", dir.display());
    Ok(())
}
