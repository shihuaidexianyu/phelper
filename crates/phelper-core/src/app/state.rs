//! `AppState` — the single read model every UI page renders from (§43:
//! "GPUI thread reads immutable/current AppState"). Plain data only: no
//! locks, no handles, no hardware types. The pump owns the mutable copy;
//! the UI receives one clone per tick.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use phelper_domain::capability::{CapabilitySet, Support};
use phelper_domain::command::{ControlOutcome, ControlReceipt, ControlStatus, Verification};
use phelper_domain::error::ControlError;
use phelper_domain::identity::DeviceIdentity;
use phelper_domain::policy::{FanCurve, FanMode};
use phelper_domain::profile::PerformanceProfile;
use phelper_domain::state::{DesiredState, ObservedState};
use phelper_domain::telemetry::TelemetrySnapshot;

use crate::OghFinding;

/// Max user-origin outcomes kept for the evidence strip.
pub const EVIDENCE_CAP: usize = 64;
/// Max journal entries kept for the Diagnostics tail view.
#[cfg(feature = "control")]
pub const JOURNAL_CAP: usize = 200;

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

/// One coalescing slot per concrete knob (§44). Knob granularity follows
/// what the user perceives as ONE control, not the wire commands: all four
/// EPP values share per-field knobs so AC and DC drags never eat each
/// other's dispatches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KnobId {
    EppAc,
    EppDc,
    Epp1Ac,
    Epp1Dc,
    MaxFreqAc,
    MaxFreqDc,
    Boost,
    ThermalMode,
    FanMode,
    GpuPolicy,
    PowerLimits,
    Profile,
}

impl KnobId {
    pub const ALL: [KnobId; 12] = [
        KnobId::EppAc,
        KnobId::EppDc,
        KnobId::Epp1Ac,
        KnobId::Epp1Dc,
        KnobId::MaxFreqAc,
        KnobId::MaxFreqDc,
        KnobId::Boost,
        KnobId::ThermalMode,
        KnobId::FanMode,
        KnobId::GpuPolicy,
        KnobId::PowerLimits,
        KnobId::Profile,
    ];
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

/// One user-origin outcome kept for the evidence strip (§56 in UI form).
#[derive(Debug, Clone)]
pub struct OutcomeRecord {
    pub at_epoch_ms: u64,
    pub knob: KnobId,
    pub outcome: ControlOutcome,
}

/// Display snapshot of one registry profile (the UI never holds the
/// registry itself).
#[derive(Debug, Clone)]
pub struct ProfileSummary {
    pub name: String,
    pub description: String,
    pub is_builtin: bool,
    /// Which domains the profile touches, same tags as the CLI list view:
    /// "ppm" / "0x29!" / "gpu" / "thermal" / "fan".
    pub touches: Vec<&'static str>,
    /// Carries `power_limits` (top-level or the R8-poisoned cpu field) —
    /// rendered with the experimental badge; apply-time gates decide.
    pub has_experimental_fields: bool,
    /// The app-side fan intent carried by this profile. Hardware does not
    /// expose a readable temperature curve, so the UI uses this persisted
    /// profile data as the curve source instead of inventing a live value.
    pub fan_mode: Option<FanMode>,
}

/// §57 stage-4 drawer visibility. `compiled` is the cargo-feature half of
/// the double gate; the drawers combine it with live capabilities.
#[derive(Debug, Clone, Default)]
pub struct ExperimentalUi {
    pub compiled: bool,
    pub power_limits_drawer: bool,
    pub gpu_policy_drawer: bool,
}

impl ExperimentalUi {
    pub fn compute(caps: Option<&CapabilitySet>) -> Self {
        let compiled = crate::app::EXPERIMENTAL_COMPILED;
        Self {
            compiled,
            power_limits_drawer: compiled
                && caps.is_some_and(|c| c.power_limits == Support::Experimental),
            gpu_policy_drawer: caps.is_some_and(|c| c.gpu_platform_policy.is_usable()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub engine: EngineStatus,
    pub identity: Option<DeviceIdentity>,
    pub telemetry: Option<Arc<TelemetrySnapshot>>,
    pub caps: Option<CapabilitySet>,
    pub desired: DesiredState,
    pub observed: ObservedState,
    /// Last software curve successfully applied by phelper. This is a
    /// recoverable editing source, not an active-control assertion.
    pub last_saved_fan_curve: Option<FanCurve>,
    pub profiles: Vec<ProfileSummary>,
    pub profile_warnings: Vec<String>,
    pub ogh_findings: Vec<OghFinding>,
    pub knobs: BTreeMap<KnobId, KnobStatus>,
    /// Arc-wrapped (v0.2): the UI receives one AppState clone per 50 ms
    /// tick — with a plain VecDeque that clone deep-copies up to
    /// EVIDENCE_CAP records 4×/s even though the strip changes only when a
    /// command finishes. Steady ticks are now pointer bumps; the reducers
    /// pay one real copy (Arc::make_mut) per actual change.
    pub evidence: Arc<VecDeque<OutcomeRecord>>,
    /// Arc-wrapped for the same reason (cap JOURNAL_CAP = 200 full
    /// entries with step evidence — the single heaviest field).
    #[cfg(feature = "control")]
    pub journal_tail: Arc<VecDeque<crate::control::journal::JournalEntry>>,
    pub experimental: ExperimentalUi,
}

impl AppState {
    pub fn knob_status(&self, knob: KnobId) -> &KnobStatus {
        static IDLE: KnobStatus = KnobStatus::Idle;
        self.knobs.get(&knob).unwrap_or(&IDLE)
    }

    /// Write controls exist only when the coordinator is running.
    pub fn writes_available(&self) -> bool {
        matches!(self.engine, EngineStatus::Running)
    }

    // ---- reducers (pure; the pump drives them) ----

    pub fn apply_snapshot(&mut self, snap: Arc<TelemetrySnapshot>) {
        self.telemetry = Some(snap);
    }

    pub fn set_knob(&mut self, knob: KnobId, status: KnobStatus) {
        self.knobs.insert(knob, status);
    }

    /// Record a finished command: knob status from its outcome + evidence
    /// entry (capped). Called for user-origin commands only — keepalive /
    /// safety / shutdown traffic is the journal's job, not the knob strip's.
    pub fn apply_outcome(&mut self, knob: KnobId, outcome: ControlOutcome) {
        let at = super::now_epoch_ms();
        let status = match &outcome.status {
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
        self.knobs.insert(knob, status);
        let ev = Arc::make_mut(&mut self.evidence);
        if ev.len() >= EVIDENCE_CAP {
            ev.pop_front();
        }
        ev.push_back(OutcomeRecord {
            at_epoch_ms: at,
            knob,
            outcome,
        });
    }

    #[cfg(feature = "control")]
    pub fn apply_journal(
        &mut self,
        entries: impl IntoIterator<Item = crate::control::journal::JournalEntry>,
    ) {
        let tail = Arc::make_mut(&mut self.journal_tail);
        for e in entries {
            if tail.len() >= JOURNAL_CAP {
                tail.pop_front();
            }
            tail.push_back(e);
        }
    }

    /// Display summaries from a registry snapshot (built-ins + user files).
    pub fn set_profiles(&mut self, registry: &crate::profiles::ProfileRegistry) {
        self.profiles = registry
            .iter()
            .map(|(name, p, is_builtin)| ProfileSummary {
                name: name.to_string(),
                description: p.description.clone(),
                is_builtin,
                touches: touches_of(p),
                has_experimental_fields: p.power_limits.is_some() || p.cpu.power_limits.is_some(),
                fan_mode: p.fan,
            })
            .collect();
        self.profile_warnings = registry.warnings.clone();
    }

    /// Latest evidence record for a knob (drives the transient row badge).
    pub fn latest_evidence(&self, knob: KnobId) -> Option<&OutcomeRecord> {
        self.evidence.iter().rev().find(|r| r.knob == knob)
    }
}

fn touches_of(p: &PerformanceProfile) -> Vec<&'static str> {
    let mut t = Vec::new();
    let c = &p.cpu;
    if c.epp_ac.is_some()
        || c.epp_dc.is_some()
        || c.epp1_ac.is_some()
        || c.epp1_dc.is_some()
        || c.max_freq_mhz_ac.is_some()
        || c.max_freq_mhz_dc.is_some()
        || c.boost_policy.is_some()
    {
        t.push("ppm");
    }
    if p.power_limits.is_some() || c.power_limits.is_some() {
        t.push("0x29!");
    }
    if p.gpu_policy.is_some() {
        t.push("gpu");
    }
    if p.thermal_mode.is_some() {
        t.push("thermal");
    }
    if p.fan.is_some() {
        t.push("fan");
    }
    t
}

/// Per-knob enable check for the UI: `Ok(())` = interactive, `Err(reason)`
/// = disabled with a short user-facing explanation. The UI additionally
/// hides ALL write controls when `!state.writes_available()`.
pub fn knob_enabled(
    caps: Option<&CapabilitySet>,
    knob: KnobId,
    experimental: &ExperimentalUi,
) -> Result<(), &'static str> {
    let Some(c) = caps else {
        return Err("正在准备控制");
    };
    let ppm_priv = |what: &'static str| -> Result<(), &'static str> {
        if !c.ppm.write_privileged {
            return Err("需要管理员权限");
        }
        match what {
            "epp" if c.ppm.epp != Support::Supported => Err("当前设备不支持此控制"),
            "epp1" if c.ppm.epp1 != Support::Supported => Err("当前设备不支持此控制"),
            "max_freq" if c.ppm.max_freq != Support::Supported => Err("当前设备不支持此控制"),
            _ => Ok(()),
        }
    };
    match knob {
        KnobId::EppAc | KnobId::EppDc => ppm_priv("epp"),
        KnobId::Epp1Ac | KnobId::Epp1Dc => ppm_priv("epp1"),
        KnobId::MaxFreqAc | KnobId::MaxFreqDc => ppm_priv("max_freq"),
        KnobId::Boost => {
            if c.ppm.write_privileged {
                Ok(())
            } else {
                Err("需要管理员权限")
            }
        }
        KnobId::ThermalMode => {
            if c.thermal_mode.is_usable() {
                Ok(())
            } else {
                Err("当前设备不支持散热控制")
            }
        }
        KnobId::FanMode => {
            if c.fan_manual_level.is_usable() || c.max_fan.is_usable() {
                Ok(())
            } else {
                Err("当前设备不支持风扇控制")
            }
        }
        KnobId::GpuPolicy => {
            if experimental.gpu_policy_drawer {
                Ok(())
            } else {
                Err("当前设备不支持此 GPU 控制")
            }
        }
        KnobId::PowerLimits => {
            if !experimental.compiled {
                Err("实验功能未启用")
            } else if !experimental.power_limits_drawer {
                Err("当前设备不支持此实验功能")
            } else {
                Ok(())
            }
        }
        KnobId::Profile => {
            if c.known_board {
                Ok(())
            } else {
                Err("当前设备不支持配置档")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::command::{ControlReceipt, ControlStatus, Verification};
    use phelper_domain::policy::BoostPolicy;
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
    fn outcome_sets_knob_status_and_caps_evidence() {
        let mut s = AppState::default();
        s.apply_outcome(
            KnobId::ThermalMode,
            outcome(ControlStatus::Applied {
                verification: Verification::TrustedNoReadback,
            }),
        );
        assert!(matches!(
            s.knob_status(KnobId::ThermalMode),
            KnobStatus::Applied {
                verification: Verification::TrustedNoReadback,
                ..
            }
        ));
        assert_eq!(s.evidence.len(), 1);

        for _ in 0..(EVIDENCE_CAP + 10) {
            s.apply_outcome(KnobId::Boost, outcome(ControlStatus::Partial));
        }
        assert_eq!(s.evidence.len(), EVIDENCE_CAP, "evidence must cap");
        assert!(matches!(
            s.knob_status(KnobId::Boost),
            KnobStatus::Partial { .. }
        ));
        // Oldest evicted: every remaining record is the Boost knob.
        assert!(s.evidence.iter().all(|r| r.knob == KnobId::Boost));
    }

    #[test]
    fn rejected_outcome_records_error() {
        let mut s = AppState::default();
        s.apply_outcome(
            KnobId::EppAc,
            outcome(ControlStatus::Rejected {
                error: ControlError::Busy,
            }),
        );
        assert!(matches!(
            s.knob_status(KnobId::EppAc),
            KnobStatus::Failed {
                error: ControlError::Busy,
                ..
            }
        ));
        assert!(matches!(s.knob_status(KnobId::FanMode), KnobStatus::Idle));
    }

    #[test]
    fn latest_evidence_finds_newest_for_knob() {
        let mut s = AppState::default();
        s.apply_outcome(KnobId::EppAc, outcome(ControlStatus::Partial));
        s.apply_outcome(
            KnobId::EppAc,
            outcome(ControlStatus::Applied {
                verification: Verification::Verified,
            }),
        );
        let r = s.latest_evidence(KnobId::EppAc).unwrap();
        assert!(matches!(
            r.outcome.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        assert!(s.latest_evidence(KnobId::Profile).is_none());
    }

    #[test]
    fn experimental_double_gate() {
        let mut caps = CapabilitySet::default();
        caps.power_limits = Support::Experimental;
        let ui = ExperimentalUi::compute(Some(&caps));
        assert_eq!(ui.power_limits_drawer, crate::app::EXPERIMENTAL_COMPILED);
        assert_eq!(ui.compiled, crate::app::EXPERIMENTAL_COMPILED);
        // Not Experimental (e.g. NotProbed) → drawer closed regardless.
        caps.power_limits = Support::NotProbed;
        assert!(!ExperimentalUi::compute(Some(&caps)).power_limits_drawer);
        // No caps at all → closed, never assumed (AR-06).
        assert!(!ExperimentalUi::compute(None).power_limits_drawer);
    }

    #[test]
    fn knob_enabled_matrix() {
        let mut caps = CapabilitySet::default();
        caps.known_board = true;
        caps.ppm.epp = Support::Supported;
        caps.ppm.epp1 = Support::Supported;
        caps.ppm.max_freq = Support::Supported;
        caps.ppm.write_privileged = true;
        caps.thermal_mode = Support::Supported;
        caps.fan_manual_level = Support::Supported;
        let exp = ExperimentalUi {
            compiled: true,
            power_limits_drawer: true,
            gpu_policy_drawer: true,
        };
        for k in KnobId::ALL {
            assert!(knob_enabled(Some(&caps), k, &exp).is_ok(), "{k:?}");
        }
        // Unelevated: PPM knobs die with the privilege reason, HP knobs live.
        caps.ppm.write_privileged = false;
        assert!(knob_enabled(Some(&caps), KnobId::EppAc, &exp).is_err());
        assert!(knob_enabled(Some(&caps), KnobId::ThermalMode, &exp).is_ok());
        // Experimental drawer closed → PowerLimits knob disabled.
        let exp_off = ExperimentalUi::default();
        assert!(knob_enabled(Some(&caps), KnobId::PowerLimits, &exp_off).is_err());
        // No caps → everything disabled.
        for k in KnobId::ALL {
            assert!(knob_enabled(None, k, &exp).is_err(), "{k:?}");
        }
    }

    #[test]
    fn profile_summaries_tag_touches() {
        let mut reg = crate::profiles::ProfileRegistry::empty();
        let p: PerformanceProfile =
            toml::from_str("description = \"d\"\n[cpu]\nepp_ac = 80\n[gpu_policy]\nctgp = true\n")
                .unwrap();
        reg.insert("x", p);
        let mut s = AppState::default();
        s.set_profiles(&reg);
        let sum = &s.profiles[0];
        assert_eq!(sum.name, "x");
        assert!(!sum.is_builtin);
        assert_eq!(sum.touches, vec!["ppm", "gpu"]);
        assert!(!sum.has_experimental_fields);
        // Built-ins are present and stable-clean.
        let reg = crate::profiles::ProfileRegistry::with_builtins();
        s.set_profiles(&reg);
        assert!(s.profiles.len() >= 4);
        assert!(s.profiles.iter().all(|p| !p.has_experimental_fields));
        assert!(s.profiles.iter().any(|p| p.touches.contains(&"fan")));
        let _ = BoostPolicy::Aggressive; // keep import used
    }
}
