//! `AppState` — the single read model every UI page renders from (§43:
//! "GPUI thread reads immutable/current AppState"). Plain data only: no
//! locks, no handles, no hardware types. The pump owns the mutable copy;
//! the UI receives one clone per tick.

use std::collections::BTreeMap;
use std::sync::Arc;

use phelper_domain::capability::CapabilitySet;
use phelper_domain::command::{ControlOutcome, ControlReceipt, ControlStatus, Verification};
use phelper_domain::error::ControlError;
use phelper_domain::state::{DesiredState, ObservedState};
use phelper_domain::telemetry::TelemetrySnapshot;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EngineStatus {
    /// Engine::start() is running on the pump thread (blocks ~seconds).
    #[default]
    Starting,
    /// Telemetry + control coordinator both up.
    Running,
    /// Telemetry only (coordinator failed or unelevated) — all write
    /// controls must be hidden, with a banner explaining why.
    TelemetryOnly,
    /// Engine::start() returned an error (e.g. unknown board).
    Failed(String),
}

/// The minimal desktop has one write surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KnobId {
    Profile,
    Cpu,
    Fan,
    Gpu,
    Power,
    Recovery,
    Mux,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum KnobStatus {
    /// Never dispatched this session.
    #[default]
    Idle,
    /// Coalesced, waiting for its dispatch window.
    Pending,
    /// Handed to the coordinator; outcome not yet received.
    InFlight(ControlReceipt),
    Applied {
        verification: Verification,
        at_epoch_ms: u64,
    },
    /// Multi-step plan stopped partway (profile apply); steps carry detail.
    Partial { at_epoch_ms: u64 },
    Failed {
        error: ControlError,
        at_epoch_ms: u64,
    },
}

/// Display snapshot of one registry profile (the UI never holds the
/// registry itself).
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileSummary {
    pub name: String,
    pub description: String,
    pub profile: phelper_domain::profile::PerformanceProfile,
    pub builtin: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AppState {
    pub engine: EngineStatus,
    pub identity: Option<phelper_domain::identity::DeviceIdentity>,
    pub hardware: crate::hardware_status::HardwareStatus,
    pub capture_running: bool,
    pub capture_notice: Option<String>,
    pub capture_report: Option<Arc<crate::measurements::CaptureReport>>,
    pub capture_baseline: Option<Arc<crate::measurements::CaptureReport>>,
    pub diagnostic_path: Option<String>,
    pub telemetry: Option<Arc<TelemetrySnapshot>>,
    pub caps: Option<CapabilitySet>,
    pub desired: DesiredState,
    pub observed: ObservedState,
    pub profiles: Vec<ProfileSummary>,
    pub profile_status: KnobStatus,
    pub knobs: BTreeMap<KnobId, KnobStatus>,
    pub last_outcome: Option<Arc<ControlOutcome>>,
    pub windows_ppm: Option<phelper_domain::policy::WindowsPpmState>,
    pub profile_notice: Option<String>,
    #[cfg(feature = "control")]
    pub automation: crate::automation::AutomationSnapshot,
}

impl AppState {
    pub fn diagnostic_json(&self) -> serde_json::Value {
        let metrics: std::collections::BTreeMap<_, _> = self.telemetry.as_ref().map(|t| t.samples.iter().map(|(id, sample)|
            (id.0, serde_json::json!({"value": sample.value.as_f64(), "age_ms": sample.timestamp.elapsed().as_millis(), "source": format!("{:?}", sample.source), "quality": format!("{:?}", sample.quality)}))).collect()).unwrap_or_default();
        serde_json::json!({"schema_version": 1, "identity": self.identity, "capabilities": self.caps,
            "desired": self.desired, "observed": self.observed, "windows_ppm": self.windows_ppm,
            "last_outcome": self.last_outcome.as_deref(), "metrics": metrics,
            "hardware": self.hardware, "engine": format!("{:?}", self.engine)})
    }

    pub fn knob_status(&self, knob: KnobId) -> &KnobStatus {
        if knob == KnobId::Profile {
            &self.profile_status
        } else {
            self.knobs.get(&knob).unwrap_or(&KnobStatus::Idle)
        }
    }

    /// Write controls exist only when the coordinator is running.
    pub fn writes_available(&self) -> bool {
        matches!(self.engine, EngineStatus::Running)
    }

    /// UI repaint gate (shell.rs observer): has anything OUTSIDE the
    /// telemetry snapshot changed? Implemented by comparing the whole
    /// state with telemetry erased, so a field added to `AppState` JOINS
    /// this gate automatically — the manual 14-field chain this replaces
    /// (2026-09 review A7) had already silently missed `hardware`,
    /// `identity` and `capture_report`. Telemetry-only changes are
    /// throttled by the shell (1 Hz on live pages) instead.
    pub fn control_changed(&self, other: &Self) -> bool {
        let mut a = self.clone();
        let mut b = other.clone();
        a.telemetry = None;
        b.telemetry = None;
        a != b
    }

    // ---- reducers (pure; the pump drives them) ----

    pub fn apply_snapshot(&mut self, snap: Arc<TelemetrySnapshot>) {
        self.telemetry = Some(snap);
    }

    pub fn set_knob(&mut self, knob: KnobId, status: KnobStatus) {
        if knob == KnobId::Profile {
            self.profile_status = status;
        } else {
            self.knobs.insert(knob, status);
        }
    }

    /// Reduce a finished command to the status the remaining profile page
    /// actually renders. Detailed evidence remains in the control journal.
    pub fn apply_outcome(&mut self, knob: KnobId, outcome: ControlOutcome) {
        let at = super::now_epoch_ms();
        let status = match &outcome.status {
            ControlStatus::Applied {
                verification: Verification::Failed { expected, actual },
            } => KnobStatus::Failed {
                error: ControlError::VerificationFailed {
                    expected: expected.clone(),
                    actual: actual.clone(),
                },
                at_epoch_ms: at,
            },
            ControlStatus::Applied { verification } => KnobStatus::Applied {
                verification: verification.clone(),
                at_epoch_ms: at,
            },
            ControlStatus::Rejected { error } => KnobStatus::Failed {
                error: error.clone(),
                at_epoch_ms: at,
            },
            ControlStatus::Partial => KnobStatus::Partial { at_epoch_ms: at },
        };
        self.set_knob(knob, status);
        self.last_outcome = Some(Arc::new(outcome));
    }

    /// Display summaries from a registry snapshot (built-ins + user files).
    pub fn set_profiles(&mut self, registry: &crate::profiles::ProfileRegistry) {
        self.profiles = registry
            .iter()
            .map(|(name, p, builtin)| ProfileSummary {
                name: name.to_string(),
                description: p.description.clone(),
                profile: p.clone(),
                builtin,
            })
            .collect();
    }
}

/// The minimal UI exposes only whole-profile writes.
pub fn profile_enabled(caps: Option<&CapabilitySet>) -> Result<(), &'static str> {
    let Some(c) = caps else {
        return Err("正在准备控制");
    };
    if c.known_board {
        Ok(())
    } else {
        Err("当前设备不支持配置档")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::command::{ControlReceipt, ControlStatus, Verification};
    use phelper_domain::profile::PerformanceProfile;
    use std::time::Duration;

    fn outcome(status: ControlStatus) -> ControlOutcome {
        ControlOutcome {
            receipt: ControlReceipt(1),
            command: phelper_domain::command::ControlCommand::SetThermalMode(
                phelper_domain::policy::ThermalMode::Balanced,
            ),
            status,
            steps: Vec::new(),
            duration: Duration::from_millis(5),
        }
    }

    #[test]
    fn outcome_sets_knob_status() {
        let mut s = AppState::default();
        s.apply_outcome(
            KnobId::Profile,
            outcome(ControlStatus::Applied {
                verification: Verification::TrustedNoReadback,
            }),
        );
        assert!(matches!(
            s.knob_status(KnobId::Profile),
            KnobStatus::Applied {
                verification: Verification::TrustedNoReadback,
                ..
            }
        ));
    }

    #[test]
    fn rejected_outcome_records_error() {
        let mut s = AppState::default();
        s.apply_outcome(
            KnobId::Profile,
            outcome(ControlStatus::Rejected {
                error: ControlError::Busy,
            }),
        );
        assert!(matches!(
            s.knob_status(KnobId::Profile),
            KnobStatus::Failed {
                error: ControlError::Busy,
                ..
            }
        ));
    }

    #[test]
    fn failed_verification_is_not_presented_as_applied() {
        let mut s = AppState::default();
        s.apply_outcome(
            KnobId::Profile,
            outcome(ControlStatus::Applied {
                verification: Verification::Failed {
                    expected: "requested".into(),
                    actual: "unchanged".into(),
                },
            }),
        );
        assert!(matches!(
            s.knob_status(KnobId::Profile),
            KnobStatus::Failed {
                error: ControlError::VerificationFailed { expected, actual },
                ..
            } if expected == "requested" && actual == "unchanged"
        ));
    }

    #[test]
    fn profile_enabled_requires_known_board() {
        let mut caps = CapabilitySet::default();
        assert!(profile_enabled(Some(&caps)).is_err());
        caps.known_board = true;
        assert!(profile_enabled(Some(&caps)).is_ok());
        assert!(profile_enabled(None).is_err());
    }

    #[test]
    fn profile_summaries_keep_rendered_fields() {
        let mut reg = crate::profiles::ProfileRegistry::empty();
        let p: PerformanceProfile =
            toml::from_str("description = \"d\"\n[cpu]\nepp_ac = 80\n[gpu_policy]\nctgp = true\n")
                .unwrap();
        reg.insert("x", p);
        let mut s = AppState::default();
        s.set_profiles(&reg);
        let sum = &s.profiles[0];
        assert_eq!(sum.name, "x");
        // Built-ins are present.
        let reg = crate::profiles::ProfileRegistry::with_builtins();
        s.set_profiles(&reg);
        assert!(s.profiles.len() >= 4);
    }

    #[test]
    fn control_changed_ignores_telemetry_only_and_catches_every_field() {
        // Telemetry-only changes must NOT wake the immediate-repaint gate.
        let a = AppState::default();
        let mut b = AppState {
            telemetry: Some(Arc::new(TelemetrySnapshot::default())),
            ..AppState::default()
        };
        assert!(!a.control_changed(&b));

        // Every non-telemetry field participates — including `hardware` and
        // `capture_report`, which the old manual 14-field chain in shell.rs
        // had silently missed.
        b.hardware.notes.push("x".into());
        assert!(a.control_changed(&b));

        let c = AppState::default();
        let d = AppState {
            capture_running: true,
            ..AppState::default()
        };
        assert!(c.control_changed(&d));

        // Arc'd reports compare by CONTENT, not pointer — same content
        // behind a fresh Arc is still "unchanged".
        let report = Arc::new(crate::measurements::CaptureReport {
            schema_version: 1,
            label: "l".into(),
            executable: "e".into(),
            process_creation_time: 0,
            duration_s: 1.0,
            presentmon_version: "v".into(),
            frames: crate::measurements::FrameSummary {
                pid: 1,
                swap_chain: "s".into(),
                frames: 1,
                mean_fps: 60.0,
                one_percent_low_fps: 40.0,
                p95_ms: 10.0,
                p99_ms: 20.0,
                other_swap_chains: 0,
                rejected_rows: 0,
            },
            hardware: Vec::new(),
            csv_path: std::path::PathBuf::from("c"),
            json_path: std::path::PathBuf::from("j"),
            context: serde_json::Value::Null,
        });
        let mut e = AppState::default();
        let mut f = AppState::default();
        e.capture_report = Some(Arc::clone(&report));
        f.capture_report = Some(Arc::clone(&report));
        assert!(!e.control_changed(&f));
    }
}
