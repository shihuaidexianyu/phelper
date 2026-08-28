//! Engine assembly (M1: telemetry-only). This is the entry point the future
//! GPUI shell consumes — it never sees `wmi`/`windows`/NVAPI internals.
//!
//! Startup is provider-tolerant: every backend constructs independently; a
//! failure downgrades that provider to Unavailable in the snapshot instead
//! of aborting the engine (D3). The one hard failure is identity/board:
//! this build is single-machine (board 8BAB), and running the telemetry
//! engine against an unknown board is out of scope by design (§3).

use std::sync::Arc;

use phelper_domain::board::BoardProfile;
use phelper_domain::error::EngineError;
use phelper_domain::identity::DeviceIdentity;
use tracing::{info, warn};

use crate::automatic_scheduler::AutomaticSchedulerHandle;
use crate::capability::load_board_profile;
use crate::os_policy::OsPolicyHandle;
use crate::platform::hp_wmi::actor::{HpActor, HpHandle};
use crate::platform::identity::probe_identity;
use crate::telemetry::collectors::{BatteryCollector, HpFanCollector, PdhCollector, PpmCollector};
use crate::telemetry::{CollectorBox, TelemetryCoordinator, TelemetryHandle};

pub struct Engine {
    identity: DeviceIdentity,
    board: BoardProfile,
    telemetry: TelemetryHandle,
    /// Windows process/thread policy writer.  It is independent from the
    /// HP/EC coordinator but follows the same explicit restore-on-shutdown
    /// contract.
    os_policy: OsPolicyHandle,
    /// Power-aware automatic OS scheduler.  It starts idle and performs no
    /// process writes until the user selects an automatic mode.
    automatic_scheduler: AutomaticSchedulerHandle,
    /// Kept for shutdown ordering and for M2 (control + keep-alive will
    /// share this same actor handle).
    hp: Option<Arc<HpHandle>>,
    /// Second-writer scan results from startup (§33.1 supplement).
    ogh_findings: Vec<crate::platform::ogh_watch::OghFinding>,
    #[cfg(feature = "control")]
    control: Option<crate::control::ControlHandle>,
}

impl Engine {
    /// Probe identity, load the board profile, spawn providers and the
    /// telemetry coordinator.
    pub fn start() -> Result<Self, EngineError> {
        Self::start_inner(true)
    }

    /// UI startup variant: OGH detection is diagnostic-only and is completed
    /// by the app pump after the engine is available. It must not delay the
    /// first usable telemetry/control state.
    pub(crate) fn start_without_ogh_scan() -> Result<Self, EngineError> {
        Self::start_inner(false)
    }

    fn start_inner(scan_ogh: bool) -> Result<Self, EngineError> {
        let identity = probe_identity()?;
        let board = load_board_profile(&identity.board_id).ok_or_else(|| {
            EngineError::Config(format!(
                "no board profile for '{}' (this build targets 8BAB only)",
                identity.board_id
            ))
        })?;
        info!(board = %identity.board_id, model = %board.device.marketing_name, "engine starting");

        // Construction is intentionally lazy: CPU topology and process
        // enumeration happen only when the caller opens the scheduling
        // surface, not on the first-screen startup path.
        let os_policy = OsPolicyHandle::new();
        let automatic_scheduler = AutomaticSchedulerHandle::start(os_policy.clone());

        // §33.1 supplement: second-writer watch. Warn-only, never kills,
        // never blocks startup — a running OGH would fight our single
        // writer from outside the process.
        let ogh_findings = if scan_ogh {
            crate::platform::ogh_watch::scan()
        } else {
            Vec::new()
        };

        let mut collectors: Vec<CollectorBox> = Vec::new();
        let mut unavailable: Vec<(&'static str, String)> = Vec::new();

        // CPU silicon (PawnIO MSR) — feature-gated, driver may be absent.
        #[cfg(feature = "pawnio")]
        {
            use crate::telemetry::collectors::PawnioCollector;
            match PawnioCollector::open(board.cpu.tsc_mhz) {
                Ok(c) => collectors.push(Box::new(c)),
                Err(e) => {
                    warn!(%e, "pawnio provider unavailable");
                    unavailable.push(("pawnio/cpu-silicon", e.to_string()));
                }
            }
        }

        // GPU (NVAPI) — feature-gated.
        #[cfg(feature = "nvidia")]
        {
            use crate::telemetry::collectors::NvapiCollector;
            match NvapiCollector::open() {
                Ok(c) => collectors.push(Box::new(c)),
                Err(e) => {
                    warn!(%e, "nvapi provider unavailable");
                    unavailable.push(("nvapi/gpu", e.to_string()));
                }
            }
        }

        // Windows OS counters.
        match PdhCollector::open() {
            Ok(c) => collectors.push(Box::new(c)),
            Err(e) => {
                warn!(%e, "pdh provider unavailable");
                unavailable.push(("windows/pdh", e.to_string()));
            }
        }

        collectors.push(Box::new(BatteryCollector::new()));
        // PPM readbacks (EPP AC/DC): unconditional, unprivileged reads.
        collectors.push(Box::new(PpmCollector::new()));

        // HP WMI (fans at ≤1 Hz in M1; the actor is also the M2 control
        // and keep-alive serialization point).
        let hp = match HpActor::spawn() {
            Ok(handle) => {
                let hp = Arc::new(handle);
                collectors.push(Box::new(HpFanCollector::new(Arc::clone(&hp))));
                Some(hp)
            }
            Err(e) => {
                warn!(%e, "hp-wmi provider unavailable");
                unavailable.push(("hp-wmi/fans", e.to_string()));
                None
            }
        };

        let telemetry = TelemetryCoordinator::start(collectors, unavailable)?;

        // M2: capability probe THROUGH the running actor (never a second
        // WMI connection — R1), then the single-writer control coordinator.
        // A failure here downgrades to telemetry-only; it never aborts.
        #[cfg(feature = "control")]
        let control = {
            let report = crate::capability::CapabilityService::probe_runtime(
                identity.clone(),
                Some(&board),
                hp.as_deref()
                    .map(|h| h as &dyn phelper_domain::ports::HpPlatform),
            );
            for note in &report.capabilities.notes {
                info!(note = %note, "capability note");
            }
            match crate::control::ControlCoordinator::start({
                let mut cfg = crate::control::ControlConfig::new(
                    report.capabilities,
                    identity.clone(),
                    hp.as_deref().cloned(),
                    crate::platform::windows_ppm::PpmBackend,
                    crate::control::SnapshotFeed {
                        telemetry: telemetry.clone(),
                    },
                    crate::control::journal::ControlJournal::default_path(),
                );
                // CapabilityService already took the complete PowrProf
                // snapshot. Reuse it for the coordinator's initial
                // ObservedState instead of paying the same startup walk
                // twice.
                cfg.windows_ppm = report.windows_ppm.clone();
                cfg.fan_curve_path = Some(crate::persistence::fan_curve_path());
                let registry = crate::profiles::ProfileRegistry::load_default();
                for w in &registry.warnings {
                    warn!(warning = %w, "profile load warning");
                }
                cfg.profiles = registry;
                cfg
            }) {
                Ok(h) => Some(h),
                Err(e) => {
                    warn!(%e, "control coordinator unavailable — telemetry-only");
                    None
                }
            }
        };

        Ok(Self {
            identity,
            board,
            telemetry,
            os_policy,
            automatic_scheduler,
            hp,
            ogh_findings,
            #[cfg(feature = "control")]
            control,
        })
    }

    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    pub fn board(&self) -> &BoardProfile {
        &self.board
    }

    pub fn telemetry(&self) -> &TelemetryHandle {
        &self.telemetry
    }

    /// Windows process/thread scheduling policy service.
    pub fn os_policy(&self) -> &OsPolicyHandle {
        &self.os_policy
    }

    /// Power-aware automatic process scheduler.  It is idle by default;
    /// enabling a mode remains an explicit user action.
    pub fn automatic_scheduler(&self) -> &AutomaticSchedulerHandle {
        &self.automatic_scheduler
    }

    /// Second-writer scan findings from startup (empty = clean baseline).
    pub fn ogh_findings(&self) -> &[crate::platform::ogh_watch::OghFinding] {
        &self.ogh_findings
    }

    /// The single write path (None when the coordinator failed to start or
    /// the `control` feature is off — telemetry-only engine).
    #[cfg(feature = "control")]
    pub fn control(&self) -> Option<&crate::control::ControlHandle> {
        self.control.as_ref()
    }

    /// Graceful stop, AR-12 order: stop automatic OS scheduling and restore
    /// its targets, restore any remaining process/thread OS policies, then
    /// control (restores firmware automatic state: 0x2E{0,0} + 0x27 off +
    /// thermal Balanced), telemetry and the HP actor.
    pub fn shutdown(self) {
        // v0.2-e: per-stage timing — the M6 HIL saw one ~38 s window-close
        // that never got a root cause; if a stage ever stalls again, the
        // log names it instead of leaving a silent gap.
        let t = std::time::Instant::now();
        self.automatic_scheduler.shutdown();
        info!(
            elapsed_ms = t.elapsed().as_millis(),
            "shutdown stage: automatic scheduler stopped"
        );
        let t = std::time::Instant::now();
        if let Err(error) = self.os_policy.restore_all() {
            warn!(%error, "shutdown stage: OS policy restore had failures");
        }
        info!(
            elapsed_ms = t.elapsed().as_millis(),
            "shutdown stage: OS policy restore done"
        );
        let t = std::time::Instant::now();
        #[cfg(feature = "control")]
        if let Some(c) = &self.control {
            c.shutdown();
            info!(
                elapsed_ms = t.elapsed().as_millis(),
                "shutdown stage: control coordinator done"
            );
        }
        let t = std::time::Instant::now();
        self.telemetry.shutdown();
        info!(
            elapsed_ms = t.elapsed().as_millis(),
            "shutdown stage: telemetry done"
        );
        let t = std::time::Instant::now();
        if let Some(hp) = &self.hp {
            hp.shutdown();
        }
        info!(
            elapsed_ms = t.elapsed().as_millis(),
            "shutdown stage: hp actor done"
        );
        info!("engine stopped");
    }
}
