//! Diagnostic report export (§60.14 supplement): one self-contained JSON
//! document for bug reports — identity, capabilities, provider health +
//! scheduler jitter, the §12 metric ownership map, OGH findings, and the
//! journal tail. Written to `<data_dir>/reports/diag-<epoch_ms>.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use super::state::AppState;

pub fn build_report(state: &AppState, jitter: &BTreeMap<&'static str, Duration>) -> Value {
    let identity = state.identity.as_ref().map(|i| {
        json!({
            "manufacturer": i.manufacturer,
            "product_name": i.product_name,
            "board_id": i.board_id,
            "bios_version": i.bios_version,
            "cpu": i.cpu.name,
            "gpu": i.gpu.iter().map(|g| &g.name).collect::<Vec<_>>(),
        })
    });

    let caps = state.caps.as_ref().map(|c| {
        json!({
            "known_board": c.known_board,
            "thermal_mode": format!("{:?}", c.thermal_mode),
            "fan_rpm_read": format!("{:?}", c.fan_rpm_read),
            "fan_manual_level": format!("{:?}", c.fan_manual_level),
            "max_fan": format!("{:?}", c.max_fan),
            "gpu_platform_policy": format!("{:?}", c.gpu_platform_policy),
            "mux": format!("{:?}", c.mux),
            "power_limits": format!("{:?}", c.power_limits),
            "fan": {
                "count": c.fan.count,
                "scale": format!("{:?}", c.fan.scale),
                "clamp_min": c.fan.clamp_min,
                "clamp_max": c.fan.clamp_max,
                "sw_control_declared": c.fan.sw_control_declared,
            },
            "ppm": {
                "epp": format!("{:?}", c.ppm.epp),
                "epp1": format!("{:?}", c.ppm.epp1),
                "max_freq": format!("{:?}", c.ppm.max_freq),
                "write_privileged": c.ppm.write_privileged,
            },
            "notes": c.notes,
        })
    });

    let (metrics, providers) = match &state.telemetry {
        Some(snap) => (
            snap.samples
                .values()
                .map(|s| {
                    json!({
                        "id": s.id.0,
                        "value": s.value.as_f64(),
                        "quality": format!("{:?}", s.quality),
                        "source": format!("{:?}", s.source),
                    })
                })
                .collect::<Vec<_>>(),
            snap.providers
                .iter()
                .map(|(name, st)| {
                    json!({
                        "name": name,
                        "status": format!("{st:?}"),
                        "worst_jitter_ms": jitter.get(name).map(|d| d.as_millis() as u64),
                    })
                })
                .collect::<Vec<_>>(),
        ),
        None => (Vec::new(), Vec::new()),
    };

    json!({
        "report_version": 1,
        "generated_epoch_ms": super::now_epoch_ms(),
        "app": {
            "version": env!("CARGO_PKG_VERSION"),
            "experimental_compiled": super::EXPERIMENTAL_COMPILED,
            "engine": format!("{:?}", state.engine),
        },
        "identity": identity,
        "capabilities": caps,
        "experimental_ui": {
            "power_limits_drawer": state.experimental.power_limits_drawer,
            "gpu_policy_drawer": state.experimental.gpu_policy_drawer,
        },
        "ogh_findings": state.ogh_findings.iter().map(|f| f.to_string()).collect::<Vec<_>>(),
        "profile_warnings": state.profile_warnings,
        "metrics": metrics,
        "providers": providers,
        "desired": serde_json::to_value(&state.desired).unwrap_or(Value::Null),
        "journal_tail": serde_json::to_value(&state.journal_tail).unwrap_or(Value::Null),
    })
}

/// Build + write; returns the written path.
pub fn write_report(state: &AppState, jitter: &BTreeMap<&'static str, Duration>) -> Result<PathBuf, String> {
    let report = build_report(state, jitter);
    let path = crate::persistence::data_dir()
        .join("reports")
        .join(format!("diag-{}.json", super::now_epoch_ms()));
    let text = serde_json::to_string_pretty(&report).map_err(|e| format!("序列化报告失败：{e}"))?;
    crate::persistence::write_text(&path, &text).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::journal::{ControlJournal, JournalOrigin};
    use phelper_domain::command::{ControlCommand, ControlOutcome, ControlReceipt, ControlStatus, Verification};
    use phelper_domain::policy::ThermalMode;

    #[test]
    fn report_has_all_sections_even_empty() {
        let state = AppState::default();
        let jitter = BTreeMap::new();
        let r = build_report(&state, &jitter);
        for key in [
            "report_version",
            "generated_epoch_ms",
            "app",
            "identity",
            "capabilities",
            "ogh_findings",
            "metrics",
            "providers",
            "desired",
            "journal_tail",
        ] {
            assert!(r.get(key).is_some(), "missing section {key}");
        }
        assert_eq!(r["metrics"].as_array().unwrap().len(), 0);
        // Round-trips through the JSON printer without panic.
        let _ = serde_json::to_string_pretty(&r).unwrap();
    }

    #[test]
    fn report_serializes_journal_entries() {
        let mut state = AppState::default();
        let j = ControlJournal::open(
            &std::env::temp_dir().join(format!("phelper-report-test-{}.jsonl", std::process::id())),
            "8BAB",
            "F.30",
        )
        .unwrap();
        let entry = j.new_entry(
            JournalOrigin::User,
            ControlOutcome {
                receipt: ControlReceipt(3),
                command: ControlCommand::SetThermalMode(ThermalMode::Performance),
                status: ControlStatus::Applied {
                    verification: Verification::TrustedNoReadback,
                },
                steps: Vec::new(),
                duration: Duration::from_millis(12),
            },
        );
        state.apply_journal([entry]);
        let r = build_report(&state, &BTreeMap::new());
        let tail = r["journal_tail"].as_array().unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0]["board_id"], "8BAB");
        assert_eq!(tail[0]["outcome"]["duration"], 12);
    }
}
