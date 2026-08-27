//! ControlCoordinator — the single writer (AR-03/AR-04). One
//! `control-coord` thread owns every hardware write: user commands
//! (FIFO queue), safety actions, keep-alive re-assertions, and the
//! shutdown restore sequence. Mirrors the TelemetryCoordinator pattern:
//! named thread + closed request enum + recv_timeout(next_due).
//!
//! Not pinned to a core — pinning is the telemetry thread's APERF/MPERF
//! requirement (R9). The 0x64F core-0 view comes from the pinned
//! telemetry thread, not from here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use phelper_domain::capability::CapabilitySet;
use phelper_domain::command::{
    ControlCommand, ControlOutcome, ControlReceipt, ControlStatus, StepOutcome, Verification,
};
use phelper_domain::error::{ControlError, EngineError, HpWmiError, PlatformError};
use phelper_domain::identity::DeviceIdentity;
use phelper_domain::policy::{
    CpuPolicy, CpuPowerLimits, FanCurve, FanLevels, FanMode, GpuPlatformPolicy, ThermalMode,
};
use phelper_domain::ports::{CpuPolicyBackend, HpBackend};
use phelper_domain::profile::GpuPolicyPatch;
use phelper_domain::state::{DesiredState, ObservedState, ObservedValue};
use phelper_domain::telemetry::ids;
use tracing::{debug, info, warn};

use crate::telemetry::TelemetryHandle;

use super::fan_curve::{FanCurveController, effective_temperature};
use super::journal::{ControlJournal, JournalOrigin};
use super::keepalive::{KeepAliveService, ReAssert};
use super::safety::{SafetyAction, SafetySupervisor, ThermalFeed};

/// Queue depth. A full queue rejects with Busy — the Application layer
/// coalesces slider spam before it ever gets here (AR-03).
const QUEUE_DEPTH: usize = 32;
/// Idle recv_timeout when nothing is tracked and no fan is user-held.
const IDLE_WAIT: Duration = Duration::from_secs(3600);
/// Tick floor while manual/max fan (or a thermal override) is active —
/// the safety net's evaluation cadence.
const SAFETY_TICK: Duration = Duration::from_secs(1);
/// Fan readback verification: 0x2D polls after a 0x2E write (§38's 1 Hz
/// firmware rule binds these too), tolerance ±1000 RPM for tach hunt.
/// 8 polls: HIL-13 showed a max-fan→manual ramp-DOWN takes ~6-9 s on 8BAB
/// (0x2D still read 3500/3900 RPM against a 2000 target at +5 s); user-path
/// ramps from auto converge in ~4 s and exit early, so the extra polls cost
/// nothing there.
const FAN_VERIFY_POLLS: u32 = 8;
const FAN_VERIFY_TOLERANCE_LEVEL: i32 = 10;

enum ControlRequest {
    Dispatch {
        receipt: ControlReceipt,
        cmd: ControlCommand,
        reply: mpsc::Sender<ControlOutcome>,
    },
    /// Read-only re-probe of the read-backable observed fields. NOT a
    /// ControlCommand: no write, no journal entry, no safety gate — the
    /// coordinator simply re-reads and re-stamps ObservedState so a
    /// startup stamp never poses as live truth for the whole session.
    RefreshObserved,
    Shutdown(mpsc::Sender<()>),
}

/// Everything the coordinator needs at construction. `hp = None` runs
/// PPM-only (capability probe already forced HP domains Unsupported).
pub(crate) struct ControlConfig<H, P, F> {
    pub caps: CapabilitySet,
    pub identity: DeviceIdentity,
    pub hp: Option<H>,
    pub ppm: P,
    pub feed: F,
    pub journal_path: std::path::PathBuf,
    /// Optional path for the last explicitly applied software fan curve.
    /// Tests leave this unset so they never touch the user's state directory.
    pub fan_curve_path: Option<std::path::PathBuf>,
    /// Profile registry for ApplyProfile expansion (built-ins + user TOML;
    /// empty = profiles unresolvable, ApplyProfile rejects UnknownProfile).
    pub profiles: crate::profiles::ProfileRegistry,
    /// Test knobs — prod code leaves these at the defaults.
    pub verify_polls: u32,
    pub verify_poll_interval: Duration,
    pub keepalive_period: Duration,
    pub safety_tick: Duration,
}

impl<H, P, F> ControlConfig<H, P, F> {
    pub(crate) fn new(
        caps: CapabilitySet,
        identity: DeviceIdentity,
        hp: Option<H>,
        ppm: P,
        feed: F,
        journal_path: std::path::PathBuf,
    ) -> Self {
        Self {
            caps,
            identity,
            hp,
            ppm,
            feed,
            journal_path,
            fan_curve_path: None,
            profiles: crate::profiles::ProfileRegistry::empty(),
            verify_polls: FAN_VERIFY_POLLS,
            verify_poll_interval: Duration::from_secs(1),
            keepalive_period: super::keepalive::PERIOD,
            safety_tick: SAFETY_TICK,
        }
    }
}

/// Handle to the running coordinator. Cheap to clone. This is the ONLY
/// write path in the process — UI/CLI dispatch through here, never around.
pub struct ControlHandle {
    tx: SyncSender<ControlRequest>,
    receipt_next: Arc<AtomicU64>,
    caps: Arc<CapabilitySet>,
    desired: Arc<RwLock<DesiredState>>,
    observed: Arc<RwLock<ObservedState>>,
    last_saved_fan_curve: Arc<RwLock<Option<FanCurve>>>,
}

impl Clone for ControlHandle {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            receipt_next: Arc::clone(&self.receipt_next),
            caps: Arc::clone(&self.caps),
            desired: Arc::clone(&self.desired),
            observed: Arc::clone(&self.observed),
            last_saved_fan_curve: Arc::clone(&self.last_saved_fan_curve),
        }
    }
}

impl ControlHandle {
    /// Async dispatch: enqueue and return the receipt + a receiver for the
    /// eventual outcome (the future UI's mode).
    pub fn dispatch(
        &self,
        cmd: ControlCommand,
    ) -> Result<(ControlReceipt, Receiver<ControlOutcome>), ControlError> {
        let receipt = ControlReceipt(self.receipt_next.fetch_add(1, Ordering::Relaxed));
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .try_send(ControlRequest::Dispatch {
                receipt,
                cmd,
                reply: reply_tx,
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => ControlError::Busy,
                mpsc::TrySendError::Disconnected(_) => ControlError::BackendUnavailable {
                    what: "control coordinator gone".into(),
                },
            })?;
        Ok((receipt, reply_rx))
    }

    /// Blocking dispatch (CLI mode): enqueue and wait for the outcome.
    pub fn dispatch_blocking(
        &self,
        cmd: ControlCommand,
        timeout: Duration,
    ) -> Result<ControlOutcome, ControlError> {
        let (_, rx) = self.dispatch(cmd)?;
        rx.recv_timeout(timeout).map_err(|e| match e {
            mpsc::RecvTimeoutError::Timeout => ControlError::Timeout,
            mpsc::RecvTimeoutError::Disconnected => ControlError::BackendUnavailable {
                what: "control coordinator gone".into(),
            },
        })
    }

    pub fn capabilities(&self) -> &CapabilitySet {
        &self.caps
    }

    pub fn desired(&self) -> DesiredState {
        self.desired.read().expect("desired poisoned").clone()
    }

    pub fn observed(&self) -> ObservedState {
        self.observed.read().expect("observed poisoned").clone()
    }

    /// The last curve successfully applied by phelper, if one was saved.
    /// This is an editing source only; it does not claim that the firmware
    /// is still running the curve after the process has exited.
    pub fn last_saved_fan_curve(&self) -> Option<FanCurve> {
        *self
            .last_saved_fan_curve
            .read()
            .expect("saved fan curve poisoned")
    }

    /// Ask the coordinator to re-probe the read-backable observed fields
    /// (EPP/EPP1 via PPM, 0x21 gpu policy) and re-stamp ObservedState.
    /// Read-only and fire-and-forget: a dropped refresh (full queue)
    /// just keeps the previous stamp, whose Instant keeps the staleness
    /// visible. (0x610 power limits are deliberately NOT re-stamped here:
    /// their observed value drives the keepalive byte2 re-assert — M4.1 —
    /// and their 250 ms live truth is already on the telemetry feed.)
    pub fn refresh_observed(&self) {
        let _ = self.tx.try_send(ControlRequest::RefreshObserved);
    }

    /// Stop the coordinator; it restores firmware automatic state first
    /// (AR-12), then acks. Called by Engine::shutdown — control stops
    /// BEFORE telemetry and the HP actor.
    pub(crate) fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(ControlRequest::Shutdown(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(15));
        }
    }
}

pub(crate) struct ControlCoordinator<H, P, F> {
    rx: Receiver<ControlRequest>,
    caps: CapabilitySet,
    hp: Option<H>,
    ppm: P,
    feed: F,
    journal: ControlJournal,
    safety: SafetySupervisor,
    fan_curve: FanCurveController,
    keepalive: KeepAliveService,
    profiles: crate::profiles::ProfileRegistry,
    verify_polls: u32,
    verify_poll_interval: Duration,
    safety_tick: Duration,
    desired: Arc<RwLock<DesiredState>>,
    observed: Arc<RwLock<ObservedState>>,
    /// 0x21 readback captured at engine start — the restore point for
    /// shutdown (only written back when this session changed the policy).
    gpu_policy_startup: Option<GpuPlatformPolicy>,
    gpu_policy_dirty: bool,
    /// 0x610 + MCHBAR feed values captured right BEFORE this session's
    /// first 0x29 write — the shutdown restore point (pl1, pl2, pl4).
    /// (The {0,0,FF,FF} DEFAULT write was observed NOT to take effect on
    /// this firmware within 500 ms in the S2 spike, so restore = explicit
    /// write-back of captured values. pl4 = 0 means the MCHBAR channel was
    /// absent at capture time → byte2 stays NO_CHANGE on restore: never
    /// touch a field we never measured.)
    power_limits_baseline: Option<(u8, u8, u8)>,
    power_limits_dirty: bool,
    /// True only after this coordinator has successfully changed fan
    /// control. A read-only session must never write `{0,0}` on shutdown.
    fan_control_dirty: bool,
    /// Thermal mode has no reliable readback, so restore it only when this
    /// session actually wrote it.
    thermal_mode_dirty: bool,
    fan_curve_path: Option<std::path::PathBuf>,
    last_saved_fan_curve: Arc<RwLock<Option<FanCurve>>>,
}

impl<H: HpBackend + 'static, P: CpuPolicyBackend + 'static, F: ThermalFeed + Send + 'static>
    ControlCoordinator<H, P, F>
{
    pub(crate) fn start(cfg: ControlConfig<H, P, F>) -> Result<ControlHandle, EngineError> {
        let journal = ControlJournal::open(
            &cfg.journal_path,
            &cfg.identity.board_id,
            &cfg.identity.bios_version,
        )?;
        let (tx, rx) = mpsc::sync_channel(QUEUE_DEPTH);
        let desired = Arc::new(RwLock::new(DesiredState::default()));
        // 0x21 readback at start: populates ObservedState (Verified, AR-10)
        // AND is the shutdown restore point if this session writes 0x22.
        let gpu_policy_startup = cfg.hp.as_ref().and_then(|hp| hp.gpu_platform_policy().ok());
        let observed = Arc::new(RwLock::new(Self::initial_observed(
            &cfg.ppm,
            gpu_policy_startup,
        )));
        let last_saved_fan_curve = Arc::new(RwLock::new(match cfg.fan_curve_path.as_deref() {
            Some(path) => match crate::persistence::load_fan_curve(path) {
                Ok(curve) => curve,
                Err(e) => {
                    warn!(path = %path.display(), %e, "saved fan curve ignored");
                    None
                }
            },
            None => None,
        }));
        let caps = Arc::new(cfg.caps);
        let handle = ControlHandle {
            tx,
            receipt_next: Arc::new(AtomicU64::new(1)),
            caps: Arc::clone(&caps),
            desired: Arc::clone(&desired),
            observed: Arc::clone(&observed),
            last_saved_fan_curve: Arc::clone(&last_saved_fan_curve),
        };
        let coord = Self {
            rx,
            caps: (*caps).clone(),
            hp: cfg.hp,
            ppm: cfg.ppm,
            feed: cfg.feed,
            journal,
            safety: SafetySupervisor::new(),
            fan_curve: FanCurveController::new(),
            keepalive: KeepAliveService::with_period(cfg.keepalive_period),
            profiles: cfg.profiles,
            verify_polls: cfg.verify_polls,
            verify_poll_interval: cfg.verify_poll_interval,
            safety_tick: cfg.safety_tick,
            desired,
            observed,
            gpu_policy_startup,
            gpu_policy_dirty: false,
            power_limits_baseline: None,
            power_limits_dirty: false,
            fan_control_dirty: false,
            thermal_mode_dirty: false,
            fan_curve_path: cfg.fan_curve_path,
            last_saved_fan_curve,
        };
        std::thread::Builder::new()
            .name("control-coord".into())
            .spawn(move || coord.run())
            .map_err(|e| EngineError::Config(format!("spawn control-coord: {e}")))?;
        Ok(handle)
    }

    /// EPP/EPP1 (and the GPU platform policy when the HP backend is up) are
    /// read back at start (Verified); everything else is Unknown until
    /// written or proven (AR-10: we don't claim states we never saw).
    fn initial_observed(ppm: &P, gpu_policy: Option<GpuPlatformPolicy>) -> ObservedState {
        let mut o = ObservedState::default();
        if let Ok((ac, dc)) = ppm.read_epp() {
            o.epp_ac = ObservedValue::Verified {
                value: ac,
                at: Instant::now(),
                source: "powrprof PERFEPP",
            };
            o.epp_dc = ObservedValue::Verified {
                value: dc,
                at: Instant::now(),
                source: "powrprof PERFEPP",
            };
        }
        if let Ok((ac, dc)) = ppm.read_epp1() {
            o.epp1_ac = ObservedValue::Verified {
                value: ac,
                at: Instant::now(),
                source: "powrprof PERFEPP1",
            };
            o.epp1_dc = ObservedValue::Verified {
                value: dc,
                at: Instant::now(),
                source: "powrprof PERFEPP1",
            };
        }
        if let Some(p) = gpu_policy {
            o.gpu_platform_policy = ObservedValue::Verified {
                value: p,
                at: Instant::now(),
                source: "hp-wmi 0x21",
            };
        }
        o
    }

    /// Read-only re-probe behind `ControlRequest::RefreshObserved` (M6 —
    /// keeps the UI's observed stamps from going minutes stale between
    /// writes). Every source fails independently: a failed
    /// re-read leaves the old stamp in place, its Instant keeping the
    /// age honest — refresh never ERASES knowledge.
    fn refresh_observed(&mut self) {
        debug!("re-probing observed readbacks");
        if let Ok((ac, dc)) = self.ppm.read_epp() {
            self.set_observed(|o| {
                o.epp_ac = ObservedValue::Verified {
                    value: ac,
                    at: Instant::now(),
                    source: "powrprof PERFEPP",
                };
                o.epp_dc = ObservedValue::Verified {
                    value: dc,
                    at: Instant::now(),
                    source: "powrprof PERFEPP",
                };
            });
        }
        if let Ok((ac, dc)) = self.ppm.read_epp1() {
            self.set_observed(|o| {
                o.epp1_ac = ObservedValue::Verified {
                    value: ac,
                    at: Instant::now(),
                    source: "powrprof PERFEPP1",
                };
                o.epp1_dc = ObservedValue::Verified {
                    value: dc,
                    at: Instant::now(),
                    source: "powrprof PERFEPP1",
                };
            });
        }
        if let Some(hp) = &self.hp
            && let Ok(p) = hp.gpu_platform_policy()
        {
            self.set_observed(|o| {
                o.gpu_platform_policy = ObservedValue::Verified {
                    value: p,
                    at: Instant::now(),
                    source: "hp-wmi 0x21",
                };
            });
        }
    }

    fn run(mut self) {
        info!("control coordinator running");
        loop {
            let wait = self.compute_wait();
            match self.rx.recv_timeout(wait) {
                Ok(ControlRequest::Dispatch {
                    receipt,
                    cmd,
                    reply,
                }) => {
                    let outcome = self.execute(receipt, cmd);
                    let _ = reply.send(outcome);
                }
                Ok(ControlRequest::Shutdown(ack)) => {
                    info!("control coordinator shutting down — restoring firmware auto");
                    self.restore_firmware_auto(JournalOrigin::Shutdown);
                    let _ = ack.send(());
                    return;
                }
                Ok(ControlRequest::RefreshObserved) => self.refresh_observed(),
                Err(mpsc::RecvTimeoutError::Timeout) => self.tick(),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // All handles dropped without shutdown(): still restore.
                    warn!("control channel closed without shutdown — restoring firmware auto");
                    self.restore_firmware_auto(JournalOrigin::Shutdown);
                    return;
                }
            }
        }
    }

    fn compute_wait(&self) -> Duration {
        let now = Instant::now();
        let observed = self.observed();
        let fan_held = matches!(
            observed.fan_mode.value(),
            Some(FanMode::Manual(_) | FanMode::Curve(_))
        ) || matches!(observed.max_fan.value(), Some(true))
            || self.safety.override_active();
        let mut wait = self.keepalive.until_due(now, IDLE_WAIT);
        if fan_held {
            wait = wait.min(self.safety_tick);
        }
        wait
    }

    /// Timeout path: safety net first, keep-alive second.
    fn tick(&mut self) {
        let now = Instant::now();
        let observed = self.observed();
        if let Some(action) = self.safety.evaluate(&self.feed, &observed, now) {
            self.run_safety_action(action);
        } else if !self.safety.override_active() {
            self.run_fan_curve(now);
        }
        if self.keepalive.is_due(now) {
            self.run_heartbeat(now);
        }
    }

    // ------------------------------------------------------------ dispatch

    fn execute(&mut self, receipt: ControlReceipt, cmd: ControlCommand) -> ControlOutcome {
        let started = Instant::now();
        info!(receipt = receipt.0, ?cmd, "control dispatch");

        // 1. Expand into an ordered plan (a single command is a 1-step plan;
        //    a profile resolves into its concrete Set* sequence). Expansion
        //    failures (unknown name, unmergeable 0x21) reject pre-write.
        let plan: Vec<ControlCommand> = match &cmd {
            ControlCommand::ApplyProfile { profile } => match self.expand_profile(profile) {
                Ok(p) => p,
                Err(error) => {
                    let outcome = ControlOutcome {
                        receipt,
                        command: cmd,
                        status: ControlStatus::Rejected { error },
                        steps: Vec::new(),
                        duration: started.elapsed(),
                    };
                    self.journal(JournalOrigin::User, &outcome);
                    return outcome;
                }
            },
            _ => vec![cmd.clone()],
        };

        // 2. Validate ALL plan steps upfront (safety layer; capability +
        //    range + freshness gates). A profile whose any field fails its
        //    per-command gate is rejected WHOLE — never apply half of a
        //    rejected intent (AR-11).
        let observed = self.observed();
        for step_cmd in &plan {
            if let Err(error) = self
                .safety
                .validate(step_cmd, &self.caps, &self.feed, &observed)
            {
                let outcome = ControlOutcome {
                    receipt,
                    command: cmd,
                    status: ControlStatus::Rejected { error },
                    steps: Vec::new(),
                    duration: started.elapsed(),
                };
                self.journal(JournalOrigin::User, &outcome);
                return outcome;
            }
        }

        // 3. Execute + verify in plan order; stop at the first failing step.
        //    A failure on the FIRST step rejects the whole command (nothing
        //    applied); a failure later leaves earlier steps applied — Partial
        //    carries the truth, the journal has the per-step evidence; no
        //    M2 rollback.
        let mut steps = Vec::new();
        let mut weakest = Verification::Skipped;
        let mut status = ControlStatus::Applied {
            verification: Verification::Skipped,
        };
        for (i, step_cmd) in plan.iter().enumerate() {
            match self.exec_one(step_cmd, &mut steps) {
                ControlStatus::Applied { verification } => {
                    weakest = weakest_verification(&weakest, &verification);
                    status = ControlStatus::Applied {
                        verification: weakest.clone(),
                    };
                    self.record_desired(step_cmd);
                }
                ControlStatus::Rejected { error } => {
                    status = if i == 0 {
                        ControlStatus::Rejected { error }
                    } else {
                        ControlStatus::Partial
                    };
                    break;
                }
                ControlStatus::Partial => {
                    status = ControlStatus::Partial;
                    break;
                }
            }
        }
        if matches!(status, ControlStatus::Applied { .. })
            && let ControlCommand::ApplyProfile { profile } = &cmd
        {
            self.desired.write().expect("desired poisoned").profile = Some(profile.clone());
        }

        // 4. Reschedule keep-alive against the new observed state.
        self.keepalive
            .reschedule_tracked(&self.tracked_set(), Instant::now());

        let outcome = ControlOutcome {
            receipt,
            command: cmd,
            status,
            steps,
            duration: started.elapsed(),
        };
        self.journal(JournalOrigin::User, &outcome);
        outcome
    }

    /// Per-command execution dispatch (one plan step).
    fn exec_one(&mut self, cmd: &ControlCommand, steps: &mut Vec<StepOutcome>) -> ControlStatus {
        match cmd {
            ControlCommand::SetThermalMode(mode) => self.exec_thermal(*mode, steps),
            ControlCommand::SetFanMode(mode) => self.exec_fan_mode(*mode, steps),
            ControlCommand::SetCpuPolicy(policy) => self.exec_cpu_policy(policy, steps),
            ControlCommand::SetGpuPlatformPolicy(p) => self.exec_gpu_policy(*p, steps),
            ControlCommand::SetGpuPlatformPolicyPatch(patch) => {
                self.exec_gpu_policy_patch(*patch, steps)
            }
            ControlCommand::SetPowerLimits(l) => self.exec_power_limits(*l, steps),
            // validate() rejects these; a plan never contains them.
            _ => ControlStatus::Rejected {
                error: ControlError::Unsupported,
            },
        }
    }

    /// Resolve a profile name into its ordered concrete plan (M5):
    /// PPM policy → 0x29 power limits → 0x22 GPU policy → thermal → fan.
    /// Fan runs LAST by design: manual/max fan suspends the firmware
    /// thermal curve, so everything else must already be in place. The 0x22
    /// step merges unspecified fields from the LIVE 0x21 readback
    /// (read-modify-write, same semantics as the gpu-policy command).
    fn expand_profile(&mut self, name: &str) -> Result<Vec<ControlCommand>, ControlError> {
        let p = self
            .profiles
            .get(name)
            .ok_or_else(|| ControlError::UnknownProfile { name: name.into() })?
            .clone();
        let mut plan = Vec::new();
        if p.cpu != CpuPolicy::default() {
            plan.push(ControlCommand::SetCpuPolicy(p.cpu));
        }
        if let Some(l) = p.power_limits {
            plan.push(ControlCommand::SetPowerLimits(l));
        }
        if let Some(patch) = p.gpu_policy {
            let hp = self
                .hp
                .as_ref()
                .ok_or_else(|| ControlError::BackendUnavailable {
                    what: "HP platform (0x21 merge source)".into(),
                })?;
            let current = hp.gpu_platform_policy().map_err(map_hp_error)?;
            plan.push(ControlCommand::SetGpuPlatformPolicy(patch.apply(current)));
        }
        if let Some(m) = p.thermal_mode {
            plan.push(ControlCommand::SetThermalMode(m));
        }
        if let Some(f) = p.fan {
            plan.push(ControlCommand::SetFanMode(f));
        }
        if plan.is_empty() {
            return Err(ControlError::UnsafeRequest {
                reason: format!("profile '{name}' has no actions"),
            });
        }
        Ok(plan)
    }

    fn record_desired(&mut self, cmd: &ControlCommand) {
        let mut d = self.desired.write().expect("desired poisoned");
        match cmd {
            ControlCommand::SetThermalMode(m) => d.thermal_mode = Some(*m),
            ControlCommand::SetFanMode(m) => d.fan_mode = Some(*m),
            ControlCommand::SetCpuPolicy(p) => d.cpu_policy = Some(p.clone()),
            ControlCommand::SetGpuPlatformPolicy(p) => d.gpu_platform_policy = Some(*p),
            // Desired is stamped inside exec_gpu_policy_patch — the
            // merged full value only exists there. (The profile-clearing
            // below still applies: a direct patch clears the name stamp.)
            ControlCommand::SetGpuPlatformPolicyPatch(_) => {}
            ControlCommand::SetPowerLimits(l) => d.power_limits = Some(*l),
            _ => {}
        }
        // A direct knob change means the desired state no longer matches
        // whatever named profile set it up. (ApplyProfile re-stamps the
        // name after its plan completes — see execute().)
        if !matches!(cmd, ControlCommand::ApplyProfile { .. }) {
            d.profile = None;
        }
    }

    // ------------------------------------------------------------ thermal

    fn exec_thermal(&mut self, mode: ThermalMode, steps: &mut Vec<StepOutcome>) -> ControlStatus {
        let before = format!("thermal={:?}", self.observed().thermal_mode);
        let Some(hp) = &self.hp else {
            return ControlStatus::Rejected {
                error: ControlError::BackendUnavailable {
                    what: "HP platform".into(),
                },
            };
        };
        match hp.set_thermal_mode(mode) {
            Ok(()) => {
                // No trustworthy readback exists for 0x1A (AR-10): the write
                // is trusted and keep-alive-maintained.
                self.set_observed(|o| {
                    o.thermal_mode = ObservedValue::TrustedWrite {
                        value: mode,
                        at: Instant::now(),
                    };
                });
                self.thermal_mode_dirty = true;
                steps.push(StepOutcome {
                    step: "set_thermal_mode".into(),
                    backend: "hp-wmi 0x1A".into(),
                    firmware_return: Some("rc=0".into()),
                    before: Some(before),
                    after: Some(format!("thermal={mode:?}(trusted)")),
                    verification: Verification::TrustedNoReadback,
                });
                ControlStatus::Applied {
                    verification: Verification::TrustedNoReadback,
                }
            }
            Err(e) => {
                steps.push(failed_step("set_thermal_mode", "hp-wmi 0x1A", &e, before));
                ControlStatus::Rejected {
                    error: map_hp_error(e),
                }
            }
        }
    }

    // ------------------------------------------------------------ fan

    fn exec_fan_mode(&mut self, mode: FanMode, steps: &mut Vec<StepOutcome>) -> ControlStatus {
        let Some(hp) = &self.hp else {
            return ControlStatus::Rejected {
                error: ControlError::BackendUnavailable {
                    what: "HP platform".into(),
                },
            };
        };
        let current = self.observed();
        let before = format!(
            "fan_mode={:?} max_fan={:?}",
            current.fan_mode, current.max_fan
        );
        match mode {
            FanMode::FirmwareAuto => {
                // §27 restore sequence: manual → 0x2E{0,0}; max → 0x27 off;
                // unknown → both (fail closed toward auto).
                let mut ok = true;
                let mut wrote_any = false;
                if !matches!(current.fan_mode.value(), Some(FanMode::FirmwareAuto)) {
                    let wrote = Self::fan_write(
                        steps,
                        "fan->auto (0x2E {0,0})",
                        "hp-wmi 0x2E",
                        &before,
                        || hp.set_fan_levels(FanLevels::AUTO),
                    );
                    ok &= wrote;
                    wrote_any |= wrote;
                }
                if !matches!(current.max_fan.value(), Some(false)) {
                    let wrote = Self::fan_write(
                        steps,
                        "max-fan off (0x27 0)",
                        "hp-wmi 0x27",
                        &before,
                        || hp.set_max_fan(false),
                    );
                    ok &= wrote;
                    wrote_any |= wrote;
                }
                if ok {
                    self.set_observed(|o| {
                        o.fan_mode = ObservedValue::TrustedWrite {
                            value: FanMode::FirmwareAuto,
                            at: Instant::now(),
                        };
                        o.max_fan = ObservedValue::TrustedWrite {
                            value: false,
                            at: Instant::now(),
                        };
                    });
                    self.fan_curve.clear();
                    self.fan_control_dirty = false;
                    self.safety.note_user_fan_mode(FanMode::FirmwareAuto);
                    ControlStatus::Applied {
                        verification: Verification::Skipped, // firmware retakes control
                    }
                } else {
                    // A partial release still changed hardware and must be
                    // attempted again during shutdown/fail-closed handling.
                    self.fan_control_dirty |= wrote_any;
                    ControlStatus::Partial
                }
            }
            FanMode::Max => {
                // Max fan is an overlay on the current 0x2E target. Do not
                // reset that target to {0,0} first: on this firmware that
                // hands control back to the BIOS/EC and causes a visible
                // fan dip before 0x27 can ramp the fans back up. This is
                // also how OmenSuperHub switches to its max mode.
                if Self::fan_write(steps, "max-fan on (0x27 1)", "hp-wmi 0x27", &before, || {
                    hp.set_max_fan(true)
                }) {
                    self.fan_control_dirty = true;
                    self.set_observed(|o| {
                        // Preserve the logical max mode while leaving the
                        // underlying manual/curve target intact.
                        o.fan_mode = ObservedValue::TrustedWrite {
                            value: FanMode::Max,
                            at: Instant::now(),
                        };
                        o.max_fan = ObservedValue::TrustedWrite {
                            value: true,
                            at: Instant::now(),
                        };
                    });
                    self.fan_curve.clear();
                    self.safety.note_user_fan_mode(FanMode::Max);
                    ControlStatus::Applied {
                        verification: Verification::TrustedNoReadback, // 0x26 unreliable
                    }
                } else {
                    ControlStatus::Partial
                }
            }
            FanMode::Manual(target) => {
                let released_max = matches!(current.max_fan.value(), Some(true));
                if released_max
                    && !Self::fan_write(
                        steps,
                        "max-fan off before manual",
                        "hp-wmi 0x27",
                        &before,
                        || hp.set_max_fan(false),
                    )
                {
                    return ControlStatus::Partial;
                }
                if released_max {
                    self.set_observed(|o| {
                        o.max_fan = ObservedValue::TrustedWrite {
                            value: false,
                            at: Instant::now(),
                        };
                    });
                    self.fan_control_dirty = true;
                }
                if !Self::fan_write(
                    steps,
                    "set manual fan levels",
                    "hp-wmi 0x2E",
                    &before,
                    || hp.set_fan_levels(target),
                ) {
                    if released_max {
                        self.restore_firmware_auto(JournalOrigin::Safety);
                    }
                    return ControlStatus::Rejected {
                        error: ControlError::FirmwareRejected {
                            detail: "0x2E write failed".into(),
                        },
                    };
                }
                self.fan_control_dirty = true;
                // Delayed readback verification (0x2D, 1 Hz — §38 binds).
                let verification = self.verify_fan_levels(hp, target);
                let ok = matches!(verification, Verification::Verified);
                self.set_observed(|o| {
                    o.fan_mode = if ok {
                        ObservedValue::Verified {
                            value: FanMode::Manual(target),
                            at: Instant::now(),
                            source: "hp-wmi 0x2D",
                        }
                    } else {
                        // Honest AR-10: record the TrustedWrite anyway; the
                        // outcome's Verification::Failed carries the truth.
                        ObservedValue::TrustedWrite {
                            value: FanMode::Manual(target),
                            at: Instant::now(),
                        }
                    };
                    o.max_fan = ObservedValue::TrustedWrite {
                        value: false,
                        at: Instant::now(),
                    };
                });
                self.fan_curve.clear();
                self.safety.note_user_fan_mode(FanMode::Manual(target));
                let after = format!("fan={verification:?}");
                steps.push(StepOutcome {
                    step: "verify fan levels (0x2D)".into(),
                    backend: "hp-wmi 0x2D".into(),
                    firmware_return: None,
                    before: Some(before),
                    after: Some(after),
                    verification: verification.clone(),
                });
                if ok {
                    ControlStatus::Applied {
                        verification: Verification::Verified,
                    }
                } else {
                    ControlStatus::Applied {
                        verification: verification.clone(),
                    }
                }
            }
            FanMode::Curve(curve) => {
                let now = Instant::now();
                let Some(temp_c) =
                    effective_temperature(self.feed.pkg_temp_c(), self.feed.gpu_temp_c(), now)
                else {
                    return ControlStatus::Rejected {
                        error: ControlError::UnsafeRequest {
                            reason: "no fresh CPU temperature sample (≤5s); refusing blind fan curve control".into(),
                        },
                    };
                };
                let target = curve.target_at(temp_c);
                let released_max = matches!(current.max_fan.value(), Some(true));
                if released_max
                    && !Self::fan_write(
                        steps,
                        "max-fan off before curve",
                        "hp-wmi 0x27",
                        &before,
                        || hp.set_max_fan(false),
                    )
                {
                    return ControlStatus::Partial;
                }
                if released_max {
                    self.set_observed(|o| {
                        o.max_fan = ObservedValue::TrustedWrite {
                            value: false,
                            at: Instant::now(),
                        };
                    });
                    self.fan_control_dirty = true;
                }
                if !Self::fan_write(
                    steps,
                    "set fan curve target",
                    "hp-wmi 0x2E",
                    &before,
                    || hp.set_fan_levels(target),
                ) {
                    self.fan_curve.record_failure(now);
                    self.restore_firmware_auto(JournalOrigin::Safety);
                    return ControlStatus::Rejected {
                        error: ControlError::FirmwareRejected {
                            detail: "0x2E curve write failed".into(),
                        },
                    };
                }
                self.fan_control_dirty = true;
                // The curve itself has no firmware readback. The following
                // telemetry tick is the live evidence for the resulting RPM;
                // do not block the coordinator for the eight-second manual
                // readback loop on every curve activation/update.
                self.set_observed(|o| {
                    o.fan_mode = ObservedValue::TrustedWrite {
                        value: FanMode::Curve(curve),
                        at: now,
                    };
                    o.max_fan = ObservedValue::TrustedWrite {
                        value: false,
                        at: now,
                    };
                });
                self.fan_curve.reset(target, temp_c, now);
                self.remember_fan_curve(curve);
                self.safety.note_user_fan_mode(FanMode::Curve(curve));
                steps.push(StepOutcome {
                    step: "activate fan curve".into(),
                    backend: "phelper curve + hp-wmi 0x2E".into(),
                    firmware_return: None,
                    before: Some(before),
                    after: Some(format!(
                        "temperature={temp_c:.1}C target_cpu={} target_gpu={} (x100 RPM)",
                        target.cpu, target.gpu
                    )),
                    verification: Verification::TrustedNoReadback,
                });
                ControlStatus::Applied {
                    verification: Verification::TrustedNoReadback,
                }
            }
        }
    }

    /// One HP fan write with uniform error mapping. Returns success.
    /// Associated fn (not &self) so callers can hold `&self.hp` across it.
    fn fan_write(
        steps: &mut Vec<StepOutcome>,
        step: &str,
        backend: &str,
        before: &str,
        write: impl FnOnce() -> Result<(), HpWmiError>,
    ) -> bool {
        match write() {
            Ok(()) => {
                steps.push(StepOutcome {
                    step: step.into(),
                    backend: backend.into(),
                    firmware_return: Some("rc=0".into()),
                    before: Some(before.into()),
                    after: None,
                    verification: Verification::TrustedNoReadback,
                });
                true
            }
            Err(e) => {
                steps.push(failed_step(step, backend, &e, before.into()));
                false
            }
        }
    }

    /// 0x2D readback after a 0x2E write: up to verify_polls × interval,
    /// Verified when every non-auto channel sits within ±1000 RPM of target.
    fn verify_fan_levels(&self, hp: &H, target: FanLevels) -> Verification {
        let mut last = String::new();
        for _ in 0..self.verify_polls {
            std::thread::sleep(self.verify_poll_interval);
            match hp.fan_levels() {
                Ok(actual) => {
                    last = format!("cpu={} gpu={} (x100 RPM)", actual.cpu, actual.gpu);
                    let cpu_ok = target.cpu == 0
                        || (actual.cpu as i32 - target.cpu as i32).abs()
                            <= FAN_VERIFY_TOLERANCE_LEVEL;
                    let gpu_ok = target.gpu == 0
                        || (actual.gpu as i32 - target.gpu as i32).abs()
                            <= FAN_VERIFY_TOLERANCE_LEVEL;
                    if cpu_ok && gpu_ok {
                        return Verification::Verified;
                    }
                }
                Err(e) => last = format!("readback error: {e}"),
            }
        }
        Verification::Failed {
            expected: format!("cpu={} gpu={} (x100 RPM)", target.cpu, target.gpu),
            actual: last,
        }
    }

    // ------------------------------------------------------------ GPU policy

    /// 0x22 write with 0x21 readback verification (a real readback exists
    /// here, unlike 0x1A/0x27 — so this path is Verified, not TrustedWrite).
    fn exec_gpu_policy(
        &mut self,
        p: GpuPlatformPolicy,
        steps: &mut Vec<StepOutcome>,
    ) -> ControlStatus {
        let Some(hp) = &self.hp else {
            return ControlStatus::Rejected {
                error: ControlError::BackendUnavailable {
                    what: "HP platform".into(),
                },
            };
        };
        let before = match hp.gpu_platform_policy() {
            Ok(cur) => format!(
                "ctgp={} ppab={} dstate={} slowdown={}C",
                cur.ctgp, cur.ppab, cur.dstate, cur.slowdown_temp_c
            ),
            Err(e) => format!("0x21 unreadable: {e}"),
        };
        match hp.set_gpu_platform_policy(p) {
            Ok(()) => {
                let verification = self.verify_gpu_policy(hp, p);
                let ok = matches!(verification, Verification::Verified);
                self.set_observed(|o| {
                    o.gpu_platform_policy = if ok {
                        ObservedValue::Verified {
                            value: p,
                            at: Instant::now(),
                            source: "hp-wmi 0x21",
                        }
                    } else {
                        // Honest AR-10: the write happened; the Failed
                        // verdict in the outcome carries the truth.
                        ObservedValue::TrustedWrite {
                            value: p,
                            at: Instant::now(),
                        }
                    };
                });
                if ok {
                    // Only a proven write counts as "this session changed
                    // the policy" for the shutdown restore.
                    self.gpu_policy_dirty = true;
                }
                steps.push(StepOutcome {
                    step: "verify gpu policy (0x21)".into(),
                    backend: "hp-wmi 0x21".into(),
                    firmware_return: None,
                    before: Some(before.clone()),
                    after: Some(format!("{verification:?}")),
                    verification: verification.clone(),
                });
                ControlStatus::Applied {
                    verification: if ok {
                        Verification::Verified
                    } else {
                        verification
                    },
                }
            }
            Err(e) => {
                steps.push(failed_step("set gpu policy", "hp-wmi 0x22", &e, before));
                ControlStatus::Rejected {
                    error: map_hp_error(e),
                }
            }
        }
    }

    /// 0x22 patch write (M6): the merge base is a FRESH 0x21 read taken
    /// here inside the single writer — never the cached ObservedState,
    /// which can sit minutes stale between re-probes (a UI-side merge
    /// over it would silently clobber fields the user never touched).
    /// Write + delayed-poll verify + observed stamping reuse
    /// the full-struct path; the plan-level safety gate has already
    /// range-checked every `Some` field.
    fn exec_gpu_policy_patch(
        &mut self,
        patch: GpuPolicyPatch,
        steps: &mut Vec<StepOutcome>,
    ) -> ControlStatus {
        let current = match &self.hp {
            Some(hp) => match hp.gpu_platform_policy() {
                Ok(c) => c,
                Err(e) => {
                    steps.push(failed_step(
                        "read gpu policy (0x21)",
                        "hp-wmi 0x21",
                        &e,
                        "—".into(),
                    ));
                    return ControlStatus::Rejected {
                        error: map_hp_error_ref(&e),
                    };
                }
            },
            None => {
                return ControlStatus::Rejected {
                    error: ControlError::BackendUnavailable {
                        what: "HP platform".into(),
                    },
                };
            }
        };
        let merged = patch.apply(current);
        let status = self.exec_gpu_policy(merged, steps);
        if matches!(status, ControlStatus::Applied { .. }) {
            // record_desired() can't fill this in — the merged value only
            // exists here — so the patch arm stamps desired itself.
            self.desired
                .write()
                .expect("desired poisoned")
                .gpu_platform_policy = Some(merged);
        }
        status
    }

    /// 0x21 readback after a 0x22 write: all four bytes must match.
    fn verify_gpu_policy(&self, hp: &H, target: GpuPlatformPolicy) -> Verification {
        let mut last = String::new();
        for _ in 0..self.verify_polls {
            std::thread::sleep(self.verify_poll_interval);
            match hp.gpu_platform_policy() {
                Ok(actual) => {
                    last = format!(
                        "ctgp={} ppab={} dstate={} slowdown={}C",
                        actual.ctgp, actual.ppab, actual.dstate, actual.slowdown_temp_c
                    );
                    if actual == target {
                        return Verification::Verified;
                    }
                }
                Err(e) => last = format!("readback error: {e}"),
            }
        }
        Verification::Failed {
            expected: format!(
                "ctgp={} ppab={} dstate={} slowdown={}C",
                target.ctgp, target.ppab, target.dstate, target.slowdown_temp_c
            ),
            actual: last,
        }
    }

    // ------------------------------------------------------------ power limits

    /// 0x29 write (EXPERIMENTAL — safety already double-gated feature +
    /// caps). Verification is runbook step 2: the MSR 0x610 telemetry
    /// readback must converge to the written values (runbook step 3, the
    /// RAPL-under-load behavior check, is a HIL activity, not an in-engine
    /// one). The baseline for shutdown-restore is captured from the feed
    /// right before our FIRST write — the firmware DEFAULT write
    /// ({0,0,FF,FF}) was observed not to take effect promptly on 8BAB.
    fn exec_power_limits(
        &mut self,
        l: CpuPowerLimits,
        steps: &mut Vec<StepOutcome>,
    ) -> ControlStatus {
        let Some(hp) = &self.hp else {
            return ControlStatus::Rejected {
                error: ControlError::BackendUnavailable {
                    what: "HP platform".into(),
                },
            };
        };
        let mut before = match self.feed.power_limits_w() {
            Some((p1, p2, at)) => format!(
                "pl1={p1:.1}W pl2={p2:.1}W (0x610, {:.1}s ago)",
                at.elapsed().as_secs_f64()
            ),
            None => "0x610 readback unavailable".to_string(),
        };
        if let Some((p4, at)) = self.feed.pl4_w() {
            before.push_str(&format!(
                "; pl4={p4:.1}W (MCHBAR, {:.1}s ago)",
                at.elapsed().as_secs_f64()
            ));
        }
        if self.power_limits_baseline.is_none()
            && let Some((p1, p2, _)) = self.feed.power_limits_w()
        {
            let p4 = self.feed.pl4_w().map(|(v, _)| v.round() as u8).unwrap_or(0);
            self.power_limits_baseline = Some((p1.round() as u8, p2.round() as u8, p4));
        }
        let written_at = Instant::now();
        match hp.set_power_limits(l) {
            Ok(()) => {
                let verification = self.verify_power_limits(l, written_at);
                let ok = matches!(verification, Verification::Verified);
                self.set_observed(|o| {
                    o.power_limits = if ok {
                        ObservedValue::Verified {
                            value: l,
                            at: Instant::now(),
                            source: "msr 0x610 (telemetry)",
                        }
                    } else {
                        // Honest AR-10: the write happened; the Failed
                        // verdict in the outcome carries the truth.
                        ObservedValue::TrustedWrite {
                            value: l,
                            at: Instant::now(),
                        }
                    };
                });
                if ok {
                    self.power_limits_dirty = true;
                }
                steps.push(StepOutcome {
                    step: if l.pl4_w != 0 {
                        "verify power limits (MSR 0x610 + MCHBAR 0x59B0)".into()
                    } else {
                        "verify power limits (MSR 0x610)".into()
                    },
                    backend: "pawnio telemetry feed".into(),
                    firmware_return: None,
                    before: Some(before.clone()),
                    after: Some(format!("{verification:?}")),
                    verification: verification.clone(),
                });
                ControlStatus::Applied {
                    verification: if ok {
                        Verification::Verified
                    } else {
                        verification
                    },
                }
            }
            Err(e) => {
                steps.push(failed_step("set power limits", "hp-wmi 0x29", &e, before));
                ControlStatus::Rejected {
                    error: map_hp_error(e),
                }
            }
        }
    }

    /// Runbook step 2: poll the 0x610 telemetry feed (250 ms cadence) until
    /// a sample TAKEN AFTER the write sits within ±1 W of the targets. PL4
    /// joins the verdict only when the write requested it (pl4_w != 0); a
    /// missing MCHBAR channel then never converges — honest Failed, not a
    /// silent skip of byte2.
    fn verify_power_limits(&self, l: CpuPowerLimits, written_at: Instant) -> Verification {
        let mut last = String::new();
        for _ in 0..self.verify_polls {
            std::thread::sleep(self.verify_poll_interval);
            match self.feed.power_limits_w() {
                Some((p1, p2, at)) => {
                    last = format!("pl1={p1:.1}W pl2={p2:.1}W");
                    if at < written_at {
                        last.push_str(" (pre-write sample)");
                        continue;
                    }
                    let pl12_ok = (p1 - f64::from(l.pl1_w)).abs() <= 1.0
                        && (p2 - f64::from(l.pl2_w)).abs() <= 1.0;
                    if l.pl4_w == 0 {
                        if pl12_ok {
                            return Verification::Verified;
                        }
                        continue;
                    }
                    match self.feed.pl4_w() {
                        Some((p4, p4_at)) => {
                            last.push_str(&format!(" pl4={p4:.1}W"));
                            if p4_at < written_at {
                                last.push_str(" (pre-write pl4 sample)");
                                continue;
                            }
                            if pl12_ok && (p4 - f64::from(l.pl4_w)).abs() <= 1.0 {
                                return Verification::Verified;
                            }
                        }
                        None => last.push_str(" pl4 feed unavailable"),
                    }
                }
                None => last = "0x610 feed unavailable".into(),
            }
        }
        Verification::Failed {
            expected: if l.pl4_w != 0 {
                format!("pl1={}W pl2={}W pl4={}W", l.pl1_w, l.pl2_w, l.pl4_w)
            } else {
                format!("pl1={}W pl2={}W", l.pl1_w, l.pl2_w)
            },
            actual: last,
        }
    }

    // ------------------------------------------------------------ CPU policy

    /// §32 order: EPP → EPP1 → max-freq → boost. Steps are independent
    /// settings — a later failure leaves earlier steps applied (Partial; no
    /// M2 rollback, journal carries the evidence).
    fn exec_cpu_policy(&mut self, p: &CpuPolicy, steps: &mut Vec<StepOutcome>) -> ControlStatus {
        let mut applied = false;
        let mut failed = false;

        if p.epp_ac.is_some() || p.epp_dc.is_some() {
            let before = self
                .ppm
                .read_epp()
                .map(|(ac, dc)| format!("epp ac={ac} dc={dc}"))
                .unwrap_or_else(|e| format!("epp unreadable: {e}"));
            match self.ppm.write_epp(p.epp_ac, p.epp_dc) {
                Ok(()) => {
                    let verification = match self.ppm.read_epp() {
                        Ok((ac, dc)) => {
                            let ac_ok = p.epp_ac.is_none_or(|v| v == ac);
                            let dc_ok = p.epp_dc.is_none_or(|v| v == dc);
                            if ac_ok && dc_ok {
                                self.set_observed(|o| {
                                    o.epp_ac = ObservedValue::Verified {
                                        value: ac,
                                        at: Instant::now(),
                                        source: "powrprof PERFEPP",
                                    };
                                    o.epp_dc = ObservedValue::Verified {
                                        value: dc,
                                        at: Instant::now(),
                                        source: "powrprof PERFEPP",
                                    };
                                });
                                Verification::Verified
                            } else {
                                Verification::Failed {
                                    expected: format!("ac={:?} dc={:?}", p.epp_ac, p.epp_dc),
                                    actual: format!("ac={ac} dc={dc}"),
                                }
                            }
                        }
                        Err(e) => Verification::Failed {
                            expected: format!("ac={:?} dc={:?}", p.epp_ac, p.epp_dc),
                            actual: format!("readback error: {e}"),
                        },
                    };
                    if !matches!(verification, Verification::Verified) {
                        failed = true;
                    }
                    steps.push(StepOutcome {
                        step: "write EPP".into(),
                        backend: "powrprof PERFEPP".into(),
                        firmware_return: Some("ok".into()),
                        before: Some(before),
                        after: Some(format!("{verification:?}")),
                        verification,
                    });
                    applied = true;
                }
                Err(e) => {
                    failed = true;
                    steps.push(platform_failed_step(
                        "write EPP",
                        "powrprof PERFEPP",
                        &e,
                        before,
                    ));
                }
            }
        }

        if p.epp1_ac.is_some() || p.epp1_dc.is_some() {
            let before = self
                .ppm
                .read_epp1()
                .map(|(ac, dc)| format!("epp1 ac={ac} dc={dc}"))
                .unwrap_or_else(|e| format!("epp1 unreadable: {e}"));
            match self.ppm.write_epp1(p.epp1_ac, p.epp1_dc) {
                Ok(()) => {
                    let verification = match self.ppm.read_epp1() {
                        Ok((ac, dc)) => {
                            let ac_ok = p.epp1_ac.is_none_or(|v| v == ac);
                            let dc_ok = p.epp1_dc.is_none_or(|v| v == dc);
                            if ac_ok && dc_ok {
                                self.set_observed(|o| {
                                    o.epp1_ac = ObservedValue::Verified {
                                        value: ac,
                                        at: Instant::now(),
                                        source: "powrprof PERFEPP1",
                                    };
                                    o.epp1_dc = ObservedValue::Verified {
                                        value: dc,
                                        at: Instant::now(),
                                        source: "powrprof PERFEPP1",
                                    };
                                });
                                Verification::Verified
                            } else {
                                Verification::Failed {
                                    expected: format!("ac={:?} dc={:?}", p.epp1_ac, p.epp1_dc),
                                    actual: format!("ac={ac} dc={dc}"),
                                }
                            }
                        }
                        Err(e) => Verification::Failed {
                            expected: format!("ac={:?} dc={:?}", p.epp1_ac, p.epp1_dc),
                            actual: format!("readback error: {e}"),
                        },
                    };
                    if !matches!(verification, Verification::Verified) {
                        failed = true;
                    }
                    steps.push(StepOutcome {
                        step: "write EPP1 (class-1)".into(),
                        backend: "powrprof PERFEPP1".into(),
                        firmware_return: Some("ok".into()),
                        before: Some(before),
                        after: Some(format!("{verification:?}")),
                        verification,
                    });
                    applied = true;
                }
                Err(e) => {
                    failed = true;
                    steps.push(platform_failed_step(
                        "write EPP1 (class-1)",
                        "powrprof PERFEPP1",
                        &e,
                        before,
                    ));
                }
            }
        }

        if p.max_freq_mhz_ac.is_some() || p.max_freq_mhz_dc.is_some() {
            let before = self
                .ppm
                .read_max_freq_mhz()
                .map(|(ac, dc)| format!("maxfreq ac={ac} dc={dc}"))
                .unwrap_or_else(|e| format!("maxfreq unreadable: {e}"));
            match self
                .ppm
                .write_max_freq_mhz(p.max_freq_mhz_ac, p.max_freq_mhz_dc)
            {
                Ok(()) => {
                    let verification = match self.ppm.read_max_freq_mhz() {
                        Ok((ac, dc)) => {
                            let ac_ok = p.max_freq_mhz_ac.is_none_or(|v| v == ac);
                            let dc_ok = p.max_freq_mhz_dc.is_none_or(|v| v == dc);
                            if ac_ok && dc_ok {
                                Verification::Verified
                            } else {
                                Verification::Failed {
                                    expected: format!(
                                        "ac={:?} dc={:?}",
                                        p.max_freq_mhz_ac, p.max_freq_mhz_dc
                                    ),
                                    actual: format!("ac={ac} dc={dc}"),
                                }
                            }
                        }
                        Err(e) => Verification::Failed {
                            expected: format!(
                                "ac={:?} dc={:?}",
                                p.max_freq_mhz_ac, p.max_freq_mhz_dc
                            ),
                            actual: format!("readback error: {e}"),
                        },
                    };
                    if !matches!(verification, Verification::Verified) {
                        failed = true;
                    }
                    steps.push(StepOutcome {
                        step: "write max frequency".into(),
                        backend: "powrprof PROCFREQMAX".into(),
                        firmware_return: Some("ok".into()),
                        before: Some(before),
                        after: Some(format!("{verification:?}")),
                        verification,
                    });
                    applied = true;
                }
                Err(e) => {
                    failed = true;
                    steps.push(platform_failed_step(
                        "write max frequency",
                        "powrprof PROCFREQMAX",
                        &e,
                        before,
                    ));
                }
            }
        }

        if let Some(mode) = p.boost_policy {
            let before = self
                .ppm
                .read_boost_policy()
                .map(|(ac, dc)| format!("boost ac={ac:?} dc={dc:?}"))
                .unwrap_or_else(|e| format!("boost unreadable: {e}"));
            match self.ppm.write_boost_policy(mode) {
                Ok(()) => {
                    let verification = match self.ppm.read_boost_policy() {
                        Ok((ac, dc)) if ac == mode && dc == mode => Verification::Verified,
                        Ok((ac, dc)) => Verification::Failed {
                            expected: format!("{mode:?}"),
                            actual: format!("ac={ac:?} dc={dc:?}"),
                        },
                        Err(e) => Verification::Failed {
                            expected: format!("{mode:?}"),
                            actual: format!("readback error: {e}"),
                        },
                    };
                    if !matches!(verification, Verification::Verified) {
                        failed = true;
                    }
                    steps.push(StepOutcome {
                        step: "write boost policy".into(),
                        backend: "powrprof PERFBOOSTMODE".into(),
                        firmware_return: Some("ok".into()),
                        before: Some(before),
                        after: Some(format!("{verification:?}")),
                        verification,
                    });
                    applied = true;
                }
                Err(e) => {
                    failed = true;
                    steps.push(platform_failed_step(
                        "write boost policy",
                        "powrprof PERFBOOSTMODE",
                        &e,
                        before,
                    ));
                }
            }
        }

        match (applied, failed) {
            (false, false) => ControlStatus::Applied {
                verification: Verification::Skipped, // empty policy = no-op
            },
            (_, false) => ControlStatus::Applied {
                verification: Verification::Verified,
            },
            (true, true) => ControlStatus::Partial,
            (false, true) => ControlStatus::Rejected {
                error: ControlError::VerificationFailed {
                    expected: "cpu policy write".into(),
                    actual: "all steps failed".into(),
                },
            },
        }
    }

    // ------------------------------------------------------------ safety

    /// Evaluate the active software curve on the coordinator thread. This is
    /// intentionally separate from `exec_fan_mode`: a curve tick is an
    /// internal policy update, not a new user command, and must not block the
    /// single writer on the eight-second manual readback verification loop.
    fn run_fan_curve(&mut self, now: Instant) {
        let Some(FanMode::Curve(curve)) = self.observed().fan_mode.value().copied() else {
            self.fan_curve.clear();
            return;
        };
        let Some((target, temp_c)) =
            self.fan_curve
                .next_target(&curve, self.feed.pkg_temp_c(), self.feed.gpu_temp_c(), now)
        else {
            return;
        };
        let Some(hp) = &self.hp else {
            self.fan_curve.record_failure(now);
            self.restore_firmware_auto(JournalOrigin::Safety);
            return;
        };
        let before = format!(
            "curve_temp={temp_c:.1}C target={:?}",
            self.fan_curve.last_target()
        );
        let mut steps = Vec::new();
        if !Self::fan_write(
            &mut steps,
            "update fan curve target",
            "hp-wmi 0x2E",
            &before,
            || hp.set_fan_levels(target),
        ) {
            self.fan_curve.record_failure(now);
            self.journal(
                JournalOrigin::Safety,
                &ControlOutcome {
                    receipt: ControlReceipt(0),
                    command: ControlCommand::SetFanMode(FanMode::Curve(curve)),
                    status: ControlStatus::Rejected {
                        error: ControlError::FirmwareRejected {
                            detail: "0x2E curve update failed".into(),
                        },
                    },
                    steps,
                    duration: Duration::ZERO,
                },
            );
            // A failed update means the application can no longer prove it
            // is maintaining the selected curve. Return ownership to the
            // firmware immediately instead of retrying a blind controller.
            self.restore_firmware_auto(JournalOrigin::Safety);
            return;
        }
        self.fan_curve.record_write(target, now);
        self.journal(
            JournalOrigin::Safety,
            &ControlOutcome {
                receipt: ControlReceipt(0),
                command: ControlCommand::SetFanMode(FanMode::Curve(curve)),
                status: ControlStatus::Applied {
                    verification: Verification::TrustedNoReadback,
                },
                steps,
                duration: Duration::ZERO,
            },
        );
    }

    fn run_safety_action(&mut self, action: SafetyAction) {
        warn!(?action, "safety action");
        let started = Instant::now();
        let mut steps = Vec::new();
        match action {
            SafetyAction::ForceMaxFan => {
                if let Some(hp) = &self.hp {
                    let ok = Self::fan_write(
                        &mut steps,
                        "SAFETY max-fan on",
                        "hp-wmi 0x27",
                        "thermal override",
                        || hp.set_max_fan(true),
                    );
                    if ok {
                        self.fan_control_dirty = true;
                        self.set_observed(|o| {
                            o.max_fan = ObservedValue::TrustedWrite {
                                value: true,
                                at: Instant::now(),
                            };
                        });
                    }
                }
                self.journal(
                    JournalOrigin::Safety,
                    &ControlOutcome {
                        receipt: ControlReceipt(0),
                        command: ControlCommand::SetFanMode(FanMode::Max),
                        status: ControlStatus::Applied {
                            verification: Verification::TrustedNoReadback,
                        },
                        steps,
                        duration: started.elapsed(),
                    },
                );
            }
            SafetyAction::ReleaseTo(mode) => {
                // Max fan is still ON here (ForceMaxFan engaged it). 0x27
                // must be released explicitly BEFORE re-applying the saved
                // mode. HIL-13 evidence: without this, the 0x2E write races
                // the max-fan ramp-down (verify read 3500/3900 RPM against a
                // 2000 target and honestly Failed), and — worse — observed
                // kept a stale max_fan=TrustedWrite(true) that the keepalive
                // would have re-asserted at the next 60 s tick, re-engaging
                // max fan the hysteresis had just released.
                if matches!(
                    self.observed().max_fan,
                    ObservedValue::TrustedWrite { value: true, .. }
                ) && let Some(hp) = &self.hp
                {
                    let _ = Self::fan_write(
                        &mut steps,
                        "SAFETY max-fan off",
                        "hp-wmi 0x27",
                        "release override",
                        || hp.set_max_fan(false),
                    );
                    self.set_observed(|o| {
                        o.max_fan = ObservedValue::TrustedWrite {
                            value: false,
                            at: Instant::now(),
                        };
                    });
                }
                let status = self.exec_fan_mode(mode, &mut steps);
                self.journal(
                    JournalOrigin::Safety,
                    &ControlOutcome {
                        receipt: ControlReceipt(0),
                        command: ControlCommand::SetFanMode(mode),
                        status,
                        steps,
                        duration: started.elapsed(),
                    },
                );
            }
            SafetyAction::WatchdogRestoreAuto => {
                self.restore_firmware_auto(JournalOrigin::Safety);
            }
        }
    }

    // ------------------------------------------------------------ keepalive

    /// The full keep-alive set: observed-derived items (thermal/fan/max)
    /// plus coordinator-state items (power limits while dirty).
    fn tracked_set(&self) -> Vec<ReAssert> {
        let mut t = KeepAliveService::tracked(&self.observed());
        if self.power_limits_dirty {
            t.push(ReAssert::PowerLimits);
        }
        t
    }

    /// Heartbeat tick: 0x10 fan-count-get keeps the firmware's
    /// user-defined states alive; then re-assert every non-default
    /// TrustedWrite (clawback repair). Steady-state success is NOT
    /// journaled (R6) — only failures are.
    fn run_heartbeat(&mut self, now: Instant) {
        let Some(hp) = &self.hp else {
            self.keepalive.record_success(now); // nothing to heartbeat against
            return;
        };
        let tracked = self.tracked_set();
        if tracked.is_empty() {
            self.keepalive.reschedule_tracked(&tracked, now);
            return;
        }

        let mut failed: Option<HpWmiError> = hp.fan_count().err();
        if failed.is_none() {
            let observed = self.observed();
            for what in tracked {
                let r = match what {
                    ReAssert::ThermalMode => match observed.thermal_mode.value() {
                        Some(mode) => hp.set_thermal_mode(*mode),
                        None => Ok(()),
                    },
                    ReAssert::FanLevels => match observed.fan_mode.value() {
                        Some(FanMode::Manual(levels)) => hp.set_fan_levels(*levels),
                        Some(FanMode::Curve(_)) => self
                            .fan_curve
                            .last_target()
                            .map_or(Ok(()), |levels| hp.set_fan_levels(levels)),
                        _ => Ok(()),
                    },
                    ReAssert::MaxFan => hp.set_max_fan(true),
                    ReAssert::PowerLimits => match observed.power_limits.value() {
                        Some(l) => hp.set_power_limits(*l),
                        None => Ok(()),
                    },
                };
                if let Err(e) = r {
                    failed = Some(e);
                    break;
                }
            }
        }

        match failed {
            None => {
                debug!("keep-alive heartbeat ok");
                self.keepalive.record_success(now);
            }
            Some(e) => {
                warn!(%e, "keep-alive heartbeat failed");
                let fail_closed = self.keepalive.record_failure(now);
                self.journal(
                    JournalOrigin::Keepalive,
                    &ControlOutcome {
                        receipt: ControlReceipt(0),
                        command: ControlCommand::SetFanMode(FanMode::FirmwareAuto),
                        status: ControlStatus::Rejected {
                            error: map_hp_error_ref(&e),
                        },
                        steps: Vec::new(),
                        duration: Duration::ZERO,
                    },
                );
                if fail_closed {
                    warn!("keep-alive failing repeatedly — failing closed to firmware auto");
                    self.restore_firmware_auto(JournalOrigin::Safety);
                }
            }
        }
    }

    // ------------------------------------------------------------ restore

    /// AR-12 restore: return only the domains this session actually changed
    /// to their safe baseline. In particular, a read-only session must not
    /// write 0x2E{0,0}; on 8BAB that releases the fan to cold idle fan-stop
    /// and was the source of the startup-at-zero symptom.
    ///
    /// Every requested step is best-effort and attempted even if an earlier
    /// step fails; the firmware clawback (~120 s) remains the ultimate
    /// backstop for a process that disappears without a graceful shutdown.
    /// EPP/max-freq/boost are deliberately NOT restored: they are
    /// Windows-native settings with no firmware-session semantics.
    fn restore_firmware_auto(&mut self, origin: JournalOrigin) {
        let restore_fan = self.fan_control_dirty;
        let restore_thermal = self.thermal_mode_dirty;
        let restore_gpu = self.gpu_policy_dirty;
        let restore_power = self.power_limits_dirty;
        if !restore_fan && !restore_thermal && !restore_gpu && !restore_power {
            return;
        }

        let started = Instant::now();
        let mut steps = Vec::new();
        let mut fan_auto_ok = !restore_fan;
        let mut max_fan_off_ok = !restore_fan;
        let mut thermal_ok = !restore_thermal;
        let mut gpu_ok = !restore_gpu;
        let mut power_ok = !restore_power;

        if let Some(hp) = &self.hp {
            if restore_fan {
                fan_auto_ok = Self::fan_write(
                    &mut steps,
                    "restore fan auto",
                    "hp-wmi 0x2E",
                    "restore",
                    || hp.set_fan_levels(FanLevels::AUTO),
                );
                max_fan_off_ok = Self::fan_write(
                    &mut steps,
                    "restore max-fan off",
                    "hp-wmi 0x27",
                    "restore",
                    || hp.set_max_fan(false),
                );
            }
            if restore_gpu && let Some(startup) = self.gpu_policy_startup {
                match hp.set_gpu_platform_policy(startup) {
                    Ok(()) => {
                        gpu_ok = true;
                        steps.push(StepOutcome {
                            step: "restore gpu policy (startup value)".into(),
                            backend: "hp-wmi 0x22".into(),
                            firmware_return: Some("rc=0".into()),
                            before: Some("restore".into()),
                            after: None,
                            verification: Verification::TrustedNoReadback,
                        });
                    }
                    Err(e) => {
                        gpu_ok = false;
                        steps.push(failed_step(
                            "restore gpu policy (startup value)",
                            "hp-wmi 0x22",
                            &e,
                            "restore".into(),
                        ));
                    }
                }
            }
            if restore_power && let Some((b1, b2, b4)) = self.power_limits_baseline {
                let baseline = CpuPowerLimits {
                    pl1_w: b1,
                    pl2_w: b2,
                    pl4_w: b4,
                    cpu_gpu_concurrent_w: 0,
                };
                match hp.set_power_limits(baseline) {
                    Ok(()) => {
                        power_ok = true;
                        steps.push(StepOutcome {
                            step: "restore power limits (captured baseline)".into(),
                            backend: "hp-wmi 0x29".into(),
                            firmware_return: Some("rc=0".into()),
                            before: Some("restore".into()),
                            after: Some(if b4 != 0 {
                                format!("pl1={b1}W pl2={b2}W pl4={b4}W")
                            } else {
                                format!("pl1={b1}W pl2={b2}W (pl4 untouched)")
                            }),
                            verification: Verification::TrustedNoReadback,
                        });
                    }
                    Err(e) => {
                        power_ok = false;
                        steps.push(failed_step(
                            "restore power limits (captured baseline)",
                            "hp-wmi 0x29",
                            &e,
                            "restore".into(),
                        ));
                    }
                }
            }
            if restore_thermal {
                match hp.set_thermal_mode(ThermalMode::Balanced) {
                    Ok(()) => {
                        thermal_ok = true;
                        steps.push(StepOutcome {
                            step: "restore thermal balanced".into(),
                            backend: "hp-wmi 0x1A".into(),
                            firmware_return: Some("rc=0".into()),
                            before: Some("restore".into()),
                            after: None,
                            verification: Verification::TrustedNoReadback,
                        });
                    }
                    Err(e) => {
                        thermal_ok = false;
                        steps.push(failed_step(
                            "restore thermal balanced",
                            "hp-wmi 0x1A",
                            &e,
                            "restore".into(),
                        ));
                    }
                }
            }
        }

        let fan_restored = !restore_fan || (fan_auto_ok && max_fan_off_ok);
        self.set_observed(|o| {
            if restore_fan && fan_auto_ok {
                o.fan_mode = ObservedValue::TrustedWrite {
                    value: FanMode::FirmwareAuto,
                    at: Instant::now(),
                };
            }
            if restore_fan && max_fan_off_ok {
                o.max_fan = ObservedValue::TrustedWrite {
                    value: false,
                    at: Instant::now(),
                };
            }
            if restore_thermal && thermal_ok {
                o.thermal_mode = ObservedValue::TrustedWrite {
                    value: ThermalMode::Balanced,
                    at: Instant::now(),
                };
            }
            if restore_gpu
                && gpu_ok
                && let Some(startup) = self.gpu_policy_startup
            {
                o.gpu_platform_policy = ObservedValue::TrustedWrite {
                    value: startup,
                    at: Instant::now(),
                };
            }
            if restore_power
                && power_ok
                && let Some((b1, b2, b4)) = self.power_limits_baseline
            {
                o.power_limits = ObservedValue::TrustedWrite {
                    value: CpuPowerLimits {
                        pl1_w: b1,
                        pl2_w: b2,
                        pl4_w: b4,
                        cpu_gpu_concurrent_w: 0,
                    },
                    at: Instant::now(),
                };
            }
        });

        if fan_restored {
            self.fan_control_dirty = false;
            self.fan_curve.clear();
            if restore_fan {
                self.safety.note_user_fan_mode(FanMode::FirmwareAuto);
            }
        }
        if thermal_ok {
            self.thermal_mode_dirty = false;
        }
        if gpu_ok {
            self.gpu_policy_dirty = false;
        }
        if power_ok {
            self.power_limits_dirty = false;
        }
        self.keepalive
            .reschedule_tracked(&self.tracked_set(), Instant::now());
        let restored = fan_restored && thermal_ok && gpu_ok && power_ok;
        self.journal(
            origin,
            &ControlOutcome {
                receipt: ControlReceipt(0),
                command: ControlCommand::SetFanMode(FanMode::FirmwareAuto),
                status: if restored {
                    ControlStatus::Applied {
                        verification: Verification::Skipped,
                    }
                } else {
                    ControlStatus::Partial
                },
                steps,
                duration: started.elapsed(),
            },
        );
    }

    // ------------------------------------------------------------ helpers

    fn observed(&self) -> ObservedState {
        self.observed.read().expect("observed poisoned").clone()
    }

    fn set_observed(&self, f: impl FnOnce(&mut ObservedState)) {
        f(&mut self.observed.write().expect("observed poisoned"));
    }

    fn remember_fan_curve(&self, curve: FanCurve) {
        *self
            .last_saved_fan_curve
            .write()
            .expect("saved fan curve poisoned") = Some(curve);
        if let Some(path) = &self.fan_curve_path
            && let Err(e) = crate::persistence::save_fan_curve(path, &curve)
        {
            // Disk persistence must never turn a successful hardware write
            // into a failed control command. The in-memory copy remains
            // available to the UI for this session.
            warn!(path = %path.display(), %e, "could not persist fan curve");
        }
    }

    fn journal(&mut self, origin: JournalOrigin, outcome: &ControlOutcome) {
        let entry = self.journal.new_entry(origin, outcome.clone());
        if let Err(e) = self.journal.append(&entry) {
            warn!(%e, "control journal append failed");
        }
    }
}

// ------------------------------------------------------------ error mapping

/// Aggregate verification for a multi-step plan: the plan is only as
/// verified as its weakest step. `Skipped` is neutral (a step that needed
/// no verification doesn't weaken the verdict).
fn weakest_verification(a: &Verification, b: &Verification) -> Verification {
    fn rank(v: &Verification) -> u8 {
        match v {
            Verification::Failed { .. } => 0,
            Verification::TrustedNoReadback => 1,
            Verification::Verified => 2,
            Verification::Skipped => 3,
        }
    }
    match (rank(a), rank(b)) {
        (3, _) => b.clone(),
        (_, 3) => a.clone(),
        (ra, rb) if ra <= rb => a.clone(),
        _ => b.clone(),
    }
}

fn map_hp_error(e: HpWmiError) -> ControlError {
    match e {
        HpWmiError::FirmwareReturnCode { code } => ControlError::FirmwareRejected {
            detail: format!("bios rc={code}"),
        },
        HpWmiError::Timeout => ControlError::Timeout,
        HpWmiError::InvalidInput(reason) => ControlError::UnsafeRequest {
            reason: reason.into(),
        },
        other => ControlError::BackendUnavailable {
            what: other.to_string(),
        },
    }
}

fn map_hp_error_ref(e: &HpWmiError) -> ControlError {
    map_hp_error(HpWmiError::Transport(e.to_string()))
}

fn failed_step(step: &str, backend: &str, e: &HpWmiError, before: String) -> StepOutcome {
    StepOutcome {
        step: step.into(),
        backend: backend.into(),
        firmware_return: Some(e.to_string()),
        before: Some(before),
        after: None,
        verification: Verification::Failed {
            expected: "firmware accept".into(),
            actual: e.to_string(),
        },
    }
}

fn platform_failed_step(
    step: &str,
    backend: &str,
    e: &PlatformError,
    before: String,
) -> StepOutcome {
    StepOutcome {
        step: step.into(),
        backend: backend.into(),
        firmware_return: Some(e.to_string()),
        before: Some(before),
        after: None,
        verification: Verification::Failed {
            expected: "API success".into(),
            actual: e.to_string(),
        },
    }
}

// ------------------------------------------------------------ telemetry feed

/// Production ThermalFeed over the telemetry snapshot (coordinator reads
/// the pinned telemetry thread's store; it never touches hardware itself).
pub(crate) struct SnapshotFeed {
    pub telemetry: TelemetryHandle,
}

impl ThermalFeed for SnapshotFeed {
    fn pkg_temp_c(&self) -> Option<(f64, Instant)> {
        let snap = self.telemetry.snapshot();
        let s = snap.samples.get(&ids::CPU_PKG_TEMP_C)?;
        Some((s.value.as_f64()?, s.timestamp))
    }

    fn gpu_temp_c(&self) -> Option<(f64, Instant)> {
        let snap = self.telemetry.snapshot();
        let s = snap.samples.get(&ids::GPU_TEMP_C)?;
        Some((s.value.as_f64()?, s.timestamp))
    }

    fn fan_levels(&self) -> Option<(FanLevels, Instant)> {
        let snap = self.telemetry.snapshot();
        let cpu = snap.samples.get(&ids::FAN_CPU_RPM)?;
        let gpu = snap.samples.get(&ids::FAN_GPU_RPM)?;
        let cpu_level = (cpu.value.as_f64()? / 100.0).round() as u16;
        let gpu_level = (gpu.value.as_f64()? / 100.0).round() as u16;
        // The pair's freshness is its older sample.
        let at = cpu.timestamp.min(gpu.timestamp);
        Some((FanLevels::new(cpu_level, gpu_level), at))
    }

    fn power_limits_w(&self) -> Option<(f64, f64, Instant)> {
        let snap = self.telemetry.snapshot();
        let pl1 = snap.samples.get(&ids::CPU_PL1_W)?;
        let pl2 = snap.samples.get(&ids::CPU_PL2_W)?;
        let at = pl1.timestamp.min(pl2.timestamp);
        Some((pl1.value.as_f64()?, pl2.value.as_f64()?, at))
    }

    fn pl4_w(&self) -> Option<(f64, Instant)> {
        let snap = self.telemetry.snapshot();
        let s = snap.samples.get(&ids::CPU_PL4_W)?;
        Some((s.value.as_f64()?, s.timestamp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::capability::{FanScale, Support};
    use phelper_domain::error::PlatformError;
    use phelper_domain::hp::{FanTable, SystemDesignData};
    use phelper_domain::identity::{CpuIdentity, DeviceIdentity};
    use phelper_domain::policy::{BoostPolicy, FanCurve, GpuPlatformPolicy, MuxMode};
    use phelper_domain::ports::{HpControl, HpPlatform};
    use phelper_domain::profile::{GpuPolicyPatch, PerformanceProfile};
    use std::sync::Mutex;

    // ------------------------------------------------------------ mocks

    struct MockHpState {
        fan_levels: FanLevels,
        fan_writes: Vec<FanLevels>,
        max_fan_writes: Vec<bool>,
        thermal_writes: Vec<ThermalMode>,
        fan_count_calls: u32,
        fail_next_thermal: bool,
        /// Scripted 0x2D readbacks for verification (popped front; last
        /// value repeats when exhausted).
        readback_script: Vec<FanLevels>,
        /// Current 0x21 readback value; None = read fails (NotAvailable).
        gpu_policy: Option<GpuPlatformPolicy>,
        gpu_policy_writes: Vec<GpuPlatformPolicy>,
        /// When set, 0x21 reads return THIS instead of the last write —
        /// scripts a readback mismatch for the verification path.
        gpu_policy_pin: Option<GpuPlatformPolicy>,
        power_limits_writes: Vec<CpuPowerLimits>,
        /// When set, a set_power_limits write also moves this shared
        /// "MSR 0x610 + MCHBAR PL4" triple — the test's ThermalFeed reads
        /// it, simulating the firmware applying the write (the verification
        /// converges). pl4 moves only when the write requests it
        /// (pl4_w != 0) — mirroring the wire-level NO_CHANGE semantics.
        msr_link: Option<std::sync::Arc<Mutex<(f64, f64, f64)>>>,
    }

    impl Default for MockHpState {
        fn default() -> Self {
            Self {
                fan_levels: FanLevels::AUTO,
                fan_writes: Vec::new(),
                max_fan_writes: Vec::new(),
                thermal_writes: Vec::new(),
                fan_count_calls: 0,
                fail_next_thermal: false,
                readback_script: Vec::new(),
                gpu_policy: Some(GpuPlatformPolicy {
                    ctgp: true,
                    ppab: true,
                    dstate: 1,
                    slowdown_temp_c: 87,
                }),
                gpu_policy_writes: Vec::new(),
                gpu_policy_pin: None,
                power_limits_writes: Vec::new(),
                msr_link: None,
            }
        }
    }

    #[derive(Clone)]
    struct MockHp(std::sync::Arc<Mutex<MockHpState>>);

    impl Default for MockHp {
        fn default() -> Self {
            Self(std::sync::Arc::new(Mutex::new(MockHpState::default())))
        }
    }

    impl MockHp {
        fn state(&self) -> std::sync::MutexGuard<'_, MockHpState> {
            self.0.lock().unwrap()
        }
    }

    impl HpPlatform for MockHp {
        fn fan_count(&self) -> Result<u8, HpWmiError> {
            self.state().fan_count_calls += 1;
            Ok(2)
        }
        fn system_design_data(&self) -> Result<SystemDesignData, HpWmiError> {
            Err(HpWmiError::NotAvailable("mock"))
        }
        fn fan_table(&self) -> Result<FanTable, HpWmiError> {
            Err(HpWmiError::NotAvailable("mock"))
        }
        fn fan_levels(&self) -> Result<FanLevels, HpWmiError> {
            let mut s = self.state();
            if s.readback_script.len() > 1 {
                return Ok(s.readback_script.remove(0));
            }
            Ok(s.readback_script.first().copied().unwrap_or(s.fan_levels))
        }
        fn gpu_platform_policy(&self) -> Result<GpuPlatformPolicy, HpWmiError> {
            let s = self.state();
            match (s.gpu_policy_pin, s.gpu_policy) {
                (Some(pin), _) => Ok(pin),
                (None, Some(p)) => Ok(p),
                (None, None) => Err(HpWmiError::NotAvailable("mock")),
            }
        }
        fn mux_mode(&self) -> Result<MuxMode, HpWmiError> {
            Err(HpWmiError::NotAvailable("mock"))
        }
        fn max_fan_readback_diagnostic(&self) -> Result<bool, HpWmiError> {
            Err(HpWmiError::NotAvailable("mock"))
        }
    }

    impl HpControl for MockHp {
        fn set_thermal_mode(&self, mode: ThermalMode) -> Result<(), HpWmiError> {
            let mut s = self.state();
            s.thermal_writes.push(mode);
            if s.fail_next_thermal {
                s.fail_next_thermal = false;
                return Err(HpWmiError::FirmwareReturnCode { code: 5 });
            }
            Ok(())
        }
        fn set_fan_levels(&self, levels: FanLevels) -> Result<(), HpWmiError> {
            let mut s = self.state();
            s.fan_writes.push(levels);
            s.fan_levels = levels;
            Ok(())
        }
        fn set_max_fan(&self, on: bool) -> Result<(), HpWmiError> {
            self.state().max_fan_writes.push(on);
            Ok(())
        }
        fn set_gpu_platform_policy(&self, p: GpuPlatformPolicy) -> Result<(), HpWmiError> {
            let mut s = self.state();
            s.gpu_policy_writes.push(p);
            s.gpu_policy = Some(p);
            Ok(())
        }
        fn set_power_limits(&self, l: CpuPowerLimits) -> Result<(), HpWmiError> {
            let mut s = self.state();
            s.power_limits_writes.push(l);
            if let Some(link) = &s.msr_link {
                let mut g = link.lock().unwrap();
                g.0 = f64::from(l.pl1_w);
                g.1 = f64::from(l.pl2_w);
                if l.pl4_w != 0 {
                    g.2 = f64::from(l.pl4_w);
                }
            }
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct MockPpm(std::sync::Arc<Mutex<(u8, u8, u8, u8)>>);

    impl CpuPolicyBackend for MockPpm {
        fn read_epp(&self) -> Result<(u8, u8), PlatformError> {
            let g = self.0.lock().unwrap();
            Ok((g.0, g.1))
        }
        fn read_epp1(&self) -> Result<(u8, u8), PlatformError> {
            let g = self.0.lock().unwrap();
            Ok((g.2, g.3))
        }
        fn read_max_freq_mhz(&self) -> Result<(u32, u32), PlatformError> {
            Ok((0, 0))
        }
        fn read_boost_policy(&self) -> Result<(BoostPolicy, BoostPolicy), PlatformError> {
            Ok((BoostPolicy::Aggressive, BoostPolicy::Aggressive))
        }
        fn write_epp(&self, ac: Option<u8>, dc: Option<u8>) -> Result<(), PlatformError> {
            let mut g = self.0.lock().unwrap();
            if let Some(v) = ac {
                g.0 = v;
            }
            if let Some(v) = dc {
                g.1 = v;
            }
            Ok(())
        }
        fn write_epp1(&self, ac: Option<u8>, dc: Option<u8>) -> Result<(), PlatformError> {
            let mut g = self.0.lock().unwrap();
            if let Some(v) = ac {
                g.2 = v;
            }
            if let Some(v) = dc {
                g.3 = v;
            }
            Ok(())
        }
        fn write_max_freq_mhz(
            &self,
            _ac: Option<u32>,
            _dc: Option<u32>,
        ) -> Result<(), PlatformError> {
            Ok(())
        }
        fn write_boost_policy(&self, _mode: BoostPolicy) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    struct FreshFeed;
    impl ThermalFeed for FreshFeed {
        fn pkg_temp_c(&self) -> Option<(f64, Instant)> {
            Some((70.0, Instant::now()))
        }
        fn fan_levels(&self) -> Option<(FanLevels, Instant)> {
            Some((FanLevels::new(30, 30), Instant::now()))
        }
        fn power_limits_w(&self) -> Option<(f64, f64, Instant)> {
            Some((55.0, 130.0, Instant::now()))
        }
    }

    // ------------------------------------------------------------ harness

    fn test_identity(tag: &str) -> DeviceIdentity {
        DeviceIdentity {
            manufacturer: "HP".into(),
            product_name: format!("test-{tag}"),
            board_id: "8BAB".into(),
            bios_version: "F.21".into(),
            cpu: CpuIdentity { name: "i9".into() },
            gpu: vec![],
        }
    }

    fn caps_full() -> CapabilitySet {
        let mut c = CapabilitySet {
            known_board: true,
            ..Default::default()
        };
        c.thermal_mode = Support::Supported;
        c.fan_rpm_read = Support::Supported;
        c.fan_manual_level = Support::Supported;
        c.max_fan = Support::Supported;
        c.fan.scale = FanScale::Krpm;
        c.fan.clamp_min = Some(5);
        c.fan.clamp_max = Some(55);
        c.ppm.epp = Support::Supported;
        c.ppm.epp1 = Support::Supported;
        c.ppm.max_freq = Support::Supported;
        c.ppm.write_privileged = true;
        c.gpu_platform_policy = Support::Supported;
        c.power_limits = Support::Experimental;
        c
    }

    struct TestRig {
        handle: ControlHandle,
        hp: MockHp,
        journal_path: std::path::PathBuf,
    }

    impl TestRig {
        fn start(tag: &str) -> Self {
            Self::start_with_hp(tag, MockHp::default())
        }

        fn start_with_hp(tag: &str, hp: MockHp) -> Self {
            Self::start_with(tag, hp, crate::profiles::ProfileRegistry::empty())
        }

        fn start_with(tag: &str, hp: MockHp, profiles: crate::profiles::ProfileRegistry) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("phelper-coord-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let journal_path = dir.join("journal.jsonl");
            let ppm = MockPpm(std::sync::Arc::new(Mutex::new((50, 50, 50, 50))));
            let mut cfg = ControlConfig::new(
                caps_full(),
                test_identity(tag),
                Some(hp.clone()),
                ppm,
                FreshFeed,
                journal_path.clone(),
            );
            // Fast knobs for tests: verification polls are 8x1s in prod.
            cfg.verify_poll_interval = Duration::from_millis(5);
            cfg.keepalive_period = Duration::from_millis(120);
            cfg.safety_tick = Duration::from_millis(20);
            cfg.profiles = profiles;
            let handle = ControlCoordinator::start(cfg).unwrap();
            Self {
                handle,
                hp,
                journal_path,
            }
        }

        fn journal_text(&self) -> String {
            std::fs::read_to_string(&self.journal_path).unwrap_or_default()
        }
    }

    fn block(h: &ControlHandle, cmd: ControlCommand) -> ControlOutcome {
        h.dispatch_blocking(cmd, Duration::from_secs(10)).unwrap()
    }

    // ------------------------------------------------------------ tests

    #[test]
    fn fifo_order_and_receipts() {
        let rig = TestRig::start("fifo");
        let o1 = block(
            &rig.handle,
            ControlCommand::SetThermalMode(ThermalMode::Performance),
        );
        let o2 = block(
            &rig.handle,
            ControlCommand::SetThermalMode(ThermalMode::Balanced),
        );
        let o3 = block(&rig.handle, ControlCommand::SetFanMode(FanMode::Max));
        assert_eq!(o1.receipt, ControlReceipt(1));
        assert_eq!(o2.receipt, ControlReceipt(2));
        assert_eq!(o3.receipt, ControlReceipt(3));
        assert!(matches!(o1.status, ControlStatus::Applied { .. }));
        assert_eq!(rig.handle.observed().fan_mode.value(), Some(&FanMode::Max));
        rig.handle.shutdown();
    }

    #[test]
    fn manual_fan_happy_path_verified() {
        let rig = TestRig::start("fanok");
        // Readback converges onto the target on the second poll.
        rig.hp.state().readback_script = vec![FanLevels::new(12, 12), FanLevels::new(30, 30)];
        let o = block(
            &rig.handle,
            ControlCommand::SetFanMode(FanMode::Manual(FanLevels::new(30, 30))),
        );
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        assert_eq!(rig.hp.state().fan_writes, vec![FanLevels::new(30, 30)]);
        let observed = rig.handle.observed();
        assert!(observed.fan_mode.is_verified());
        assert_eq!(
            observed.fan_mode.value(),
            Some(&FanMode::Manual(FanLevels::new(30, 30)))
        );
        // Journal carries the user-origin entry with before/after evidence.
        let j = rig.journal_text();
        assert!(j.contains("\"origin\":\"user\""));
        assert!(j.contains("\"before\":"));
        rig.handle.shutdown();
    }

    #[test]
    fn max_fan_preserves_manual_target_without_auto_reset() {
        let rig = TestRig::start("max-preserve-manual");
        let target = FanLevels::new(30, 30);
        rig.hp.state().readback_script = vec![target];
        let manual = block(
            &rig.handle,
            ControlCommand::SetFanMode(FanMode::Manual(target)),
        );
        assert!(matches!(
            manual.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        let fan_writes_before_max = rig.hp.state().fan_writes.len();

        let max = block(&rig.handle, ControlCommand::SetFanMode(FanMode::Max));
        assert!(matches!(
            max.status,
            ControlStatus::Applied {
                verification: Verification::TrustedNoReadback
            }
        ));
        let state = rig.hp.state();
        assert_eq!(state.fan_writes.len(), fan_writes_before_max);
        assert_eq!(state.max_fan_writes, vec![true]);
        assert_eq!(rig.handle.observed().fan_mode.value(), Some(&FanMode::Max));
        rig.handle.shutdown();
    }

    #[test]
    fn max_fan_switches_directly_to_manual() {
        let rig = TestRig::start("max-to-manual");
        block(&rig.handle, ControlCommand::SetFanMode(FanMode::Max));

        let target = FanLevels::new(30, 30);
        rig.hp.state().readback_script = vec![target];
        let outcome = block(
            &rig.handle,
            ControlCommand::SetFanMode(FanMode::Manual(target)),
        );

        assert!(matches!(
            outcome.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        assert_eq!(rig.hp.state().max_fan_writes, vec![true, false]);
        assert_eq!(
            rig.handle.observed().fan_mode.value(),
            Some(&FanMode::Manual(target))
        );
        rig.handle.shutdown();
    }

    #[test]
    fn fan_curve_activation_writes_one_initial_target_without_blocking_readback() {
        let rig = TestRig::start("curve-start");
        let curve = FanCurve::balanced();
        let expected = curve.target_at(70.0);
        let o = block(
            &rig.handle,
            ControlCommand::SetFanMode(FanMode::Curve(curve)),
        );
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::TrustedNoReadback
            }
        ));
        assert_eq!(rig.hp.state().fan_writes, vec![expected]);
        assert_eq!(
            rig.handle.observed().fan_mode.value(),
            Some(&FanMode::Curve(curve))
        );
        assert_eq!(rig.handle.last_saved_fan_curve(), Some(curve));
        rig.handle.shutdown();
    }

    #[test]
    fn verification_failure_records_actual() {
        let rig = TestRig::start("fanfail");
        // Tach never reaches the target.
        rig.hp.state().readback_script = vec![FanLevels::new(10, 10)];
        let o = block(
            &rig.handle,
            ControlCommand::SetFanMode(FanMode::Manual(FanLevels::new(50, 50))),
        );
        let ControlStatus::Applied { verification } = o.status else {
            panic!("expected Applied, got {:?}", o.status);
        };
        let Verification::Failed { expected, actual } = verification else {
            panic!("expected Failed");
        };
        assert!(expected.contains("50"));
        assert!(actual.contains("10"));
        // Observed state is honest: TrustedWrite, NOT verified (AR-10).
        let observed = rig.handle.observed();
        assert!(!observed.fan_mode.is_verified());
        assert!(observed.fan_mode.value().is_some());
        rig.handle.shutdown();
    }

    #[test]
    fn firmware_rejection_leaves_state_untouched() {
        let rig = TestRig::start("rc5");
        rig.hp.state().fail_next_thermal = true;
        let o = block(
            &rig.handle,
            ControlCommand::SetThermalMode(ThermalMode::Performance),
        );
        let ControlStatus::Rejected { error } = o.status else {
            panic!("expected Rejected, got {:?}", o.status);
        };
        assert!(matches!(error, ControlError::FirmwareRejected { .. }));
        assert!(rig.handle.observed().thermal_mode.value().is_none());
        // Desired state not recorded for rejected commands.
        assert!(rig.handle.desired().thermal_mode.is_none());
        rig.handle.shutdown();
    }

    #[test]
    fn keepalive_reasserts_tracked_writes() {
        let rig = TestRig::start("kalive");
        let _ = block(
            &rig.handle,
            ControlCommand::SetThermalMode(ThermalMode::Performance),
        );
        let writes_before = rig.hp.state().thermal_writes.len();
        let beats_before = rig.hp.state().fan_count_calls;
        // keepalive_period = 120 ms; wait for ~3 beats.
        std::thread::sleep(Duration::from_millis(400));
        let beats = rig.hp.state().fan_count_calls - beats_before;
        let reasserts = rig.hp.state().thermal_writes.len() - writes_before;
        assert!(beats >= 2, "expected >=2 heartbeats, got {beats}");
        assert!(
            reasserts >= 2,
            "expected >=2 thermal re-assertions, got {reasserts}"
        );
        rig.handle.shutdown();
    }

    #[test]
    fn restore_auto_on_shutdown() {
        let rig = TestRig::start("restore");
        let _ = block(&rig.handle, ControlCommand::SetFanMode(FanMode::Max));
        let _ = block(
            &rig.handle,
            ControlCommand::SetThermalMode(ThermalMode::Performance),
        );
        rig.handle.shutdown();
        let s = rig.hp.state();
        assert!(s.fan_writes.contains(&FanLevels::AUTO));
        assert!(s.max_fan_writes.contains(&false));
        assert_eq!(s.thermal_writes.last(), Some(&ThermalMode::Balanced));
        drop(s);
        let j = rig.journal_text();
        assert!(j.contains("\"origin\":\"shutdown\""));
    }

    #[test]
    fn read_only_shutdown_does_not_touch_fan_or_thermal_registers() {
        let rig = TestRig::start("read-only-shutdown");
        rig.handle.shutdown();
        let state = rig.hp.state();
        assert!(
            state.fan_writes.is_empty(),
            "a session that never owned fan control must not write 0x2E"
        );
        assert!(
            state.max_fan_writes.is_empty(),
            "a session that never owned fan control must not write 0x27"
        );
        assert!(
            state.thermal_writes.is_empty(),
            "a session that never changed thermal mode must not restore it"
        );
        assert!(
            !rig.journal_text().contains("\"origin\":\"shutdown\""),
            "a no-op shutdown should not create a fake restore entry"
        );
    }

    #[test]
    fn queue_full_reports_busy() {
        let rig = TestRig::start("busy");
        // Occupy the coordinator thread with a slow verification (5 polls
        // x 5 ms, readback never converges) so the queue can fill.
        rig.hp.state().readback_script = vec![FanLevels::new(10, 10)];
        let (r1, _rx1) = rig
            .handle
            .dispatch(ControlCommand::SetFanMode(FanMode::Manual(FanLevels::new(
                50, 50,
            ))))
            .unwrap();
        assert_eq!(r1, ControlReceipt(1));
        let mut busy = false;
        for _ in 0..(QUEUE_DEPTH + 4) {
            match rig
                .handle
                .dispatch(ControlCommand::SetFanMode(FanMode::Max))
            {
                Ok(_) => {}
                Err(ControlError::Busy) => busy = true,
                Err(e) => panic!("unexpected dispatch error: {e}"),
            }
        }
        assert!(busy, "queue never reported Busy");
        rig.handle.shutdown();
    }

    #[test]
    fn empty_cpu_policy_is_noop_applied() {
        let rig = TestRig::start("noop");
        let o = block(
            &rig.handle,
            ControlCommand::SetCpuPolicy(CpuPolicy::default()),
        );
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Skipped
            }
        ));
        rig.handle.shutdown();
    }

    #[test]
    fn epp_write_readback_verified() {
        let rig = TestRig::start("epp");
        let o = block(
            &rig.handle,
            ControlCommand::SetCpuPolicy(CpuPolicy {
                epp_ac: Some(20),
                ..Default::default()
            }),
        );
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        let observed = rig.handle.observed();
        assert_eq!(observed.epp_ac.value(), Some(&20));
        assert!(observed.epp_ac.is_verified());
        rig.handle.shutdown();
    }

    #[test]
    fn epp1_write_readback_verified() {
        let rig = TestRig::start("epp1");
        let o = block(
            &rig.handle,
            ControlCommand::SetCpuPolicy(CpuPolicy {
                epp1_ac: Some(30),
                epp1_dc: Some(50),
                ..Default::default()
            }),
        );
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        let observed = rig.handle.observed();
        assert_eq!(observed.epp1_ac.value(), Some(&30));
        assert_eq!(observed.epp1_dc.value(), Some(&50));
        assert!(observed.epp1_ac.is_verified());
        rig.handle.shutdown();
    }

    #[test]
    fn gpu_policy_write_readback_verified() {
        let rig = TestRig::start("gpu-policy");
        let target = GpuPlatformPolicy {
            ctgp: false,
            ppab: true,
            dstate: 2,
            slowdown_temp_c: 87,
        };
        let o = block(&rig.handle, ControlCommand::SetGpuPlatformPolicy(target));
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        let observed = rig.handle.observed();
        assert_eq!(observed.gpu_platform_policy.value(), Some(&target));
        assert!(observed.gpu_platform_policy.is_verified());
        assert_eq!(rig.hp.state().gpu_policy_writes, vec![target]);
        rig.handle.shutdown();
    }

    #[test]
    fn gpu_policy_readback_mismatch_is_honest_failure() {
        let rig = TestRig::start("gpu-policy-mismatch");
        // Firmware "ignores" the write: pin 0x21 to the startup value.
        let startup = rig.hp.state().gpu_policy.unwrap();
        rig.hp.state().gpu_policy_pin = Some(startup);
        let target = GpuPlatformPolicy {
            ctgp: false,
            ppab: false,
            dstate: 3,
            slowdown_temp_c: 87,
        };
        let o = block(&rig.handle, ControlCommand::SetGpuPlatformPolicy(target));
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Failed { .. }
            }
        ));
        // AR-10 honesty: observed records the TrustedWrite, not Verified.
        let observed = rig.handle.observed();
        assert_eq!(observed.gpu_platform_policy.value(), Some(&target));
        assert!(!observed.gpu_platform_policy.is_verified());
        rig.handle.shutdown();
    }

    #[test]
    fn gpu_policy_patch_merges_over_fresh_read() {
        let rig = TestRig::start("gpu-policy-patch");
        // Simulate change SINCE engine start (what this command exists
        // for): the startup stamp holds the default mock value, but the
        // "firmware" now reads something else entirely.
        rig.hp.state().gpu_policy = Some(GpuPlatformPolicy {
            ctgp: true,
            ppab: false,
            dstate: 3,
            slowdown_temp_c: 0,
        });
        let o = block(
            &rig.handle,
            ControlCommand::SetGpuPlatformPolicyPatch(GpuPolicyPatch {
                ctgp: Some(false),
                ..Default::default()
            }),
        );
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        // The merge base was the FRESH read: only ctgp moved; the changed
        // ppab/dstate/slowdown were preserved — NOT the startup values.
        let merged = GpuPlatformPolicy {
            ctgp: false,
            ppab: false,
            dstate: 3,
            slowdown_temp_c: 0,
        };
        assert_eq!(rig.hp.state().gpu_policy_writes, vec![merged]);
        let observed = rig.handle.observed();
        assert_eq!(observed.gpu_platform_policy.value(), Some(&merged));
        assert!(observed.gpu_platform_policy.is_verified());
        assert_eq!(rig.handle.desired().gpu_platform_policy, Some(merged));
        rig.handle.shutdown();
    }

    #[test]
    fn gpu_policy_patch_empty_rejected_pre_write() {
        let rig = TestRig::start("gpu-policy-patch-empty");
        let o = block(
            &rig.handle,
            ControlCommand::SetGpuPlatformPolicyPatch(GpuPolicyPatch::default()),
        );
        assert!(matches!(o.status, ControlStatus::Rejected { .. }));
        assert!(rig.hp.state().gpu_policy_writes.is_empty());
        rig.handle.shutdown();
    }

    #[test]
    fn refresh_observed_restamps_drifted_readbacks() {
        let rig = TestRig::start("refresh-observed");
        // Startup stamp holds the default mock value; now the "firmware"
        // reads something else with zero writes (a stale cache).
        let drifted = GpuPlatformPolicy {
            ctgp: false,
            ppab: false,
            dstate: 3,
            slowdown_temp_c: 0,
        };
        rig.hp.state().gpu_policy_pin = Some(drifted);
        rig.handle.refresh_observed();
        // Fire-and-forget: poll the cache until the re-probe lands.
        let deadline = Instant::now() + Duration::from_secs(5);
        while rig.handle.observed().gpu_platform_policy.value() != Some(&drifted) {
            assert!(Instant::now() < deadline, "refresh never landed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(rig.handle.observed().gpu_platform_policy.is_verified());
        // Read-only: no writes, and nothing journaled (pre-shutdown).
        assert!(rig.hp.state().gpu_policy_writes.is_empty());
        assert!(rig.journal_text().is_empty());
        rig.handle.shutdown();
    }

    #[test]
    fn shutdown_restores_startup_gpu_policy() {
        let rig = TestRig::start("gpu-policy-restore");
        let startup = rig.hp.state().gpu_policy;
        let target = GpuPlatformPolicy {
            ctgp: false,
            ppab: true,
            dstate: 2,
            slowdown_temp_c: 87,
        };
        let o = block(&rig.handle, ControlCommand::SetGpuPlatformPolicy(target));
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        rig.handle.shutdown();
        let writes = rig.hp.state().gpu_policy_writes.clone();
        // write #1 = the user's change, write #2 = the shutdown restore.
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1], startup.unwrap());
    }

    // ---------------------------- 0x29 power limits (feature-gated) ----

    #[cfg(feature = "experimental-hp-power-limits")]
    struct PlFeed {
        link: std::sync::Arc<Mutex<(f64, f64, f64)>>,
        /// false = the MCHBAR channel is absent (pl4_w() → None): a pl4
        /// write can then never verify, and a pl4-less write must not care.
        has_pl4: bool,
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    impl ThermalFeed for PlFeed {
        fn pkg_temp_c(&self) -> Option<(f64, Instant)> {
            Some((70.0, Instant::now()))
        }
        fn fan_levels(&self) -> Option<(FanLevels, Instant)> {
            Some((FanLevels::new(30, 30), Instant::now()))
        }
        fn power_limits_w(&self) -> Option<(f64, f64, Instant)> {
            let v = *self.link.lock().unwrap();
            Some((v.0, v.1, Instant::now()))
        }
        fn pl4_w(&self) -> Option<(f64, Instant)> {
            self.has_pl4
                .then(|| (*self.link.lock().unwrap()).2)
                .map(|v| (v, Instant::now()))
        }
    }

    /// Custom rig with a live "MSR 0x610 + MCHBAR" triple the feed reads;
    /// with_link wires MockHp's 0x29 writes into it (simulating firmware
    /// applying the write so verification can converge).
    #[cfg(feature = "experimental-hp-power-limits")]
    fn start_pl_rig(
        tag: &str,
        link: std::sync::Arc<Mutex<(f64, f64, f64)>>,
        with_link: bool,
        has_pl4: bool,
    ) -> (ControlHandle, MockHp) {
        let dir =
            std::env::temp_dir().join(format!("phelper-coord-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let journal_path = dir.join("journal.jsonl");
        let hp = MockHp::default();
        if with_link {
            hp.state().msr_link = Some(std::sync::Arc::clone(&link));
        }
        let mut cfg = ControlConfig::new(
            caps_full(),
            test_identity(tag),
            Some(hp.clone()),
            MockPpm::default(),
            PlFeed { link, has_pl4 },
            journal_path,
        );
        cfg.verify_poll_interval = Duration::from_millis(5);
        cfg.keepalive_period = Duration::from_millis(120);
        cfg.safety_tick = Duration::from_millis(20);
        let handle = ControlCoordinator::start(cfg).unwrap();
        (handle, hp)
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    fn pl(pl1: u8, pl2: u8) -> CpuPowerLimits {
        CpuPowerLimits {
            pl1_w: pl1,
            pl2_w: pl2,
            pl4_w: 0,
            cpu_gpu_concurrent_w: 0,
        }
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    fn pl4(pl1: u8, pl2: u8, pl4: u8) -> CpuPowerLimits {
        CpuPowerLimits {
            pl4_w: pl4,
            ..pl(pl1, pl2)
        }
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    #[test]
    fn power_limits_write_verified_via_0610_feed() {
        let link = std::sync::Arc::new(Mutex::new((55.0_f64, 130.0_f64, 200.0_f64)));
        let (handle, hp) = start_pl_rig("pl-verified", std::sync::Arc::clone(&link), true, true);
        let o = block(&handle, ControlCommand::SetPowerLimits(pl(45, 90)));
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        let observed = handle.observed();
        assert_eq!(observed.power_limits.value(), Some(&pl(45, 90)));
        assert!(observed.power_limits.is_verified());
        assert_eq!(hp.state().power_limits_writes, vec![pl(45, 90)]);
        handle.shutdown();
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    #[test]
    fn power_limits_no_convergence_is_honest_failure() {
        // Firmware "ignores" the write: feed never moves off the baseline.
        let link = std::sync::Arc::new(Mutex::new((55.0_f64, 130.0_f64, 200.0_f64)));
        let (handle, hp) = start_pl_rig("pl-noconv", std::sync::Arc::clone(&link), false, true);
        let o = block(&handle, ControlCommand::SetPowerLimits(pl(45, 90)));
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Failed { .. }
            }
        ));
        let observed = handle.observed();
        assert_eq!(observed.power_limits.value(), Some(&pl(45, 90)));
        assert!(!observed.power_limits.is_verified());
        handle.shutdown();
        // Not dirty → shutdown does NOT write a power-limits restore.
        assert_eq!(hp.state().power_limits_writes.len(), 1);
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    #[test]
    fn shutdown_restores_power_limits_baseline() {
        let link = std::sync::Arc::new(Mutex::new((55.0_f64, 130.0_f64, 200.0_f64)));
        let (handle, hp) = start_pl_rig("pl-restore", std::sync::Arc::clone(&link), true, true);
        let o = block(&handle, ControlCommand::SetPowerLimits(pl(45, 90)));
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        handle.shutdown();
        let writes = hp.state().power_limits_writes.clone();
        // write #1 = the user's change, write #2 = baseline restore
        // (55/130/200 captured from the feeds BEFORE our first write; the
        // MCHBAR channel was live, so pl4 is restored explicitly too).
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1], pl4(55, 130, 200));
        assert_eq!(*link.lock().unwrap(), (55.0, 130.0, 200.0));
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    #[test]
    fn keepalive_reasserts_power_limits() {
        let link = std::sync::Arc::new(Mutex::new((55.0_f64, 130.0_f64, 200.0_f64)));
        let (handle, hp) = start_pl_rig("pl-keepalive", std::sync::Arc::clone(&link), true, true);
        let o = block(&handle, ControlCommand::SetPowerLimits(pl(45, 90)));
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        // keepalive period is 120 ms in this rig — let ~3 ticks pass.
        std::thread::sleep(Duration::from_millis(400));
        let n = hp.state().power_limits_writes.len();
        handle.shutdown();
        assert!(n >= 2, "expected keepalive re-asserts, got {n} writes");
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    #[test]
    fn power_limits_pl4_write_verified_via_mchbar_feed() {
        let link = std::sync::Arc::new(Mutex::new((55.0_f64, 130.0_f64, 200.0_f64)));
        let (handle, hp) = start_pl_rig("pl4-verified", std::sync::Arc::clone(&link), true, true);
        let o = block(&handle, ControlCommand::SetPowerLimits(pl4(45, 90, 150)));
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        let observed = handle.observed();
        assert_eq!(observed.power_limits.value(), Some(&pl4(45, 90, 150)));
        assert!(observed.power_limits.is_verified());
        assert_eq!(hp.state().power_limits_writes, vec![pl4(45, 90, 150)]);
        // keepalive re-asserts must carry byte2 (the observed value
        // includes pl4_w) — let ~3 ticks pass and check the last write.
        std::thread::sleep(Duration::from_millis(400));
        let n = hp.state().power_limits_writes.len();
        let last = hp.state().power_limits_writes.last().copied();
        handle.shutdown();
        assert!(n >= 2, "expected keepalive re-asserts, got {n} writes");
        assert_eq!(last, Some(pl4(45, 90, 150)));
        // Restore covered all three fields (baseline incl. pl4=200).
        let writes = hp.state().power_limits_writes.clone();
        assert_eq!(writes.last(), Some(&pl4(55, 130, 200)));
        assert_eq!(*link.lock().unwrap(), (55.0, 130.0, 200.0));
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    #[test]
    fn power_limits_pl4_without_mchbar_feed_is_honest_failure() {
        // The write requests pl4 but the PL4 readback channel is absent:
        // byte2 can never be verified → honest Failed, never Verified.
        let link = std::sync::Arc::new(Mutex::new((55.0_f64, 130.0_f64, 200.0_f64)));
        let (handle, hp) = start_pl_rig("pl4-nofeed", std::sync::Arc::clone(&link), true, false);
        let o = block(&handle, ControlCommand::SetPowerLimits(pl4(45, 90, 150)));
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Failed { .. }
            }
        ));
        assert!(!handle.observed().power_limits.is_verified());
        handle.shutdown();
        // Not dirty → no restore write.
        assert_eq!(hp.state().power_limits_writes.len(), 1);
    }

    #[cfg(feature = "experimental-hp-power-limits")]
    #[test]
    fn shutdown_restore_leaves_pl4_untouched_when_baseline_unknown() {
        // No MCHBAR channel at capture time → the baseline's pl4 component
        // is 0 = "unknown" → the restore payload keeps byte2 = NO_CHANGE
        // (never write a field we never measured).
        let link = std::sync::Arc::new(Mutex::new((55.0_f64, 130.0_f64, 200.0_f64)));
        let (handle, hp) = start_pl_rig("pl4-nobase", std::sync::Arc::clone(&link), true, false);
        let o = block(&handle, ControlCommand::SetPowerLimits(pl(45, 90)));
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::Verified
            }
        ));
        handle.shutdown();
        let writes = hp.state().power_limits_writes.clone();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1], pl(55, 130));
    }

    /// HIL-13 regression: after the hysteresis release, observed.max_fan
    /// must not stay TrustedWrite(true) — the keepalive would re-assert max
    /// fan at the next tick. The release sequence must write 0x27-off
    /// BEFORE re-applying the saved manual mode.
    #[test]
    fn hysteresis_release_clears_max_fan_first() {
        struct MutableFeed(std::sync::Arc<Mutex<f64>>);
        impl ThermalFeed for MutableFeed {
            fn pkg_temp_c(&self) -> Option<(f64, Instant)> {
                Some((*self.0.lock().unwrap(), Instant::now()))
            }
            fn fan_levels(&self) -> Option<(FanLevels, Instant)> {
                Some((FanLevels::new(20, 20), Instant::now()))
            }
            fn power_limits_w(&self) -> Option<(f64, f64, Instant)> {
                Some((55.0, 130.0, Instant::now()))
            }
        }

        let dir =
            std::env::temp_dir().join(format!("phelper-coord-test-hyst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let journal_path = dir.join("journal.jsonl");
        let temp = std::sync::Arc::new(Mutex::new(70.0_f64));
        let hp = MockHp::default();
        let ppm = MockPpm(std::sync::Arc::new(Mutex::new((50, 50, 50, 50))));
        let mut cfg = ControlConfig::new(
            caps_full(),
            test_identity("hyst"),
            Some(hp.clone()),
            ppm,
            MutableFeed(std::sync::Arc::clone(&temp)),
            journal_path.clone(),
        );
        cfg.verify_poll_interval = Duration::from_millis(5);
        cfg.keepalive_period = Duration::from_millis(120);
        cfg.safety_tick = Duration::from_millis(20);
        let handle = ControlCoordinator::start(cfg).unwrap();

        // User holds manual fans within clamp; readback converges at once.
        hp.state().readback_script = vec![FanLevels::new(20, 20)];
        let o = block(
            &handle,
            ControlCommand::SetFanMode(FanMode::Manual(FanLevels::new(20, 20))),
        );
        assert!(matches!(o.status, ControlStatus::Applied { .. }));

        // Heat past the hysteresis threshold → ForceMaxFan.
        *temp.lock().unwrap() = 92.0;
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            hp.state().max_fan_writes.contains(&true),
            "ForceMaxFan never wrote 0x27-on"
        );

        // Cool back below release → ReleaseTo(Manual) must first clear 0x27.
        *temp.lock().unwrap() = 80.0;
        std::thread::sleep(Duration::from_millis(400));
        let s = hp.state();
        let writes = &s.max_fan_writes;
        let on = writes.iter().position(|w| *w).expect("0x27-on missing");
        let off_after = writes.iter().skip(on + 1).any(|w| !*w);
        assert!(
            off_after,
            "release never wrote 0x27-off (writes: {writes:?}) — keepalive would re-assert max fan"
        );
        drop(s);
        let observed = handle.observed();
        assert_eq!(observed.max_fan.value(), Some(&false));
        let j = std::fs::read_to_string(&journal_path).unwrap_or_default();
        assert!(j.contains("SAFETY max-fan off"));
        handle.shutdown();
    }

    #[test]
    fn hysteresis_release_restores_max_user_mode() {
        struct MutableFeed(std::sync::Arc<Mutex<f64>>);
        impl ThermalFeed for MutableFeed {
            fn pkg_temp_c(&self) -> Option<(f64, Instant)> {
                Some((*self.0.lock().unwrap(), Instant::now()))
            }
            fn fan_levels(&self) -> Option<(FanLevels, Instant)> {
                Some((FanLevels::new(20, 20), Instant::now()))
            }
            fn power_limits_w(&self) -> Option<(f64, f64, Instant)> {
                Some((55.0, 130.0, Instant::now()))
            }
        }

        let dir = std::env::temp_dir().join(format!(
            "phelper-coord-test-hyst-max-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let journal_path = dir.join("journal.jsonl");
        let temp = std::sync::Arc::new(Mutex::new(70.0_f64));
        let hp = MockHp::default();
        let ppm = MockPpm(std::sync::Arc::new(Mutex::new((50, 50, 50, 50))));
        let mut cfg = ControlConfig::new(
            caps_full(),
            test_identity("hyst-max"),
            Some(hp.clone()),
            ppm,
            MutableFeed(std::sync::Arc::clone(&temp)),
            journal_path,
        );
        cfg.verify_poll_interval = Duration::from_millis(5);
        cfg.keepalive_period = Duration::from_millis(120);
        cfg.safety_tick = Duration::from_millis(20);
        let handle = ControlCoordinator::start(cfg).unwrap();

        let o = block(&handle, ControlCommand::SetFanMode(FanMode::Max));
        assert!(matches!(o.status, ControlStatus::Applied { .. }));
        assert_eq!(handle.observed().fan_mode.value(), Some(&FanMode::Max));

        *temp.lock().unwrap() = 92.0;
        std::thread::sleep(Duration::from_millis(300));
        *temp.lock().unwrap() = 80.0;
        std::thread::sleep(Duration::from_millis(400));

        let s = hp.state();
        let writes = &s.max_fan_writes;
        let first_on = writes.iter().position(|w| *w).expect("0x27-on missing");
        let off = writes
            .iter()
            .enumerate()
            .skip(first_on + 1)
            .find_map(|(index, on)| (!*on).then_some(index))
            .expect("release never wrote 0x27-off");
        assert!(
            writes.iter().skip(off + 1).any(|on| *on),
            "release did not restore max fan after clearing the override: {writes:?}"
        );
        drop(s);
        let observed = handle.observed();
        assert_eq!(observed.fan_mode.value(), Some(&FanMode::Max));
        assert_eq!(observed.max_fan.value(), Some(&true));
        handle.shutdown();
    }

    // ---------------------------- profiles (M5) ----

    fn registry_with(name: &str, p: PerformanceProfile) -> crate::profiles::ProfileRegistry {
        let mut r = crate::profiles::ProfileRegistry::empty();
        r.insert(name, p);
        r
    }

    #[test]
    fn profile_apply_expands_and_runs_all_steps_in_order() {
        let mut p = PerformanceProfile::default();
        p.cpu.epp_ac = Some(33);
        p.gpu_policy = Some(GpuPolicyPatch {
            ctgp: Some(false),
            ..Default::default()
        });
        p.thermal_mode = Some(ThermalMode::Performance);
        p.fan = Some(FanMode::Max);
        let rig = TestRig::start_with("prof-happy", MockHp::default(), registry_with("mix", p));

        let o = block(
            &rig.handle,
            ControlCommand::ApplyProfile {
                profile: "mix".into(),
            },
        );
        // Overall verdict can never exceed the weakest step (thermal + max
        // fan have no readback → TrustedNoReadback).
        assert!(matches!(
            o.status,
            ControlStatus::Applied {
                verification: Verification::TrustedNoReadback
            }
        ));
        // Plan order: PPM → 0x22 → thermal → fan (fan last).
        let step_names: Vec<&str> = o.steps.iter().map(|s| s.step.as_str()).collect();
        let pos = |needle: &str| {
            step_names
                .iter()
                .position(|s| s.contains(needle))
                .unwrap_or_else(|| panic!("step '{needle}' missing: {step_names:?}"))
        };
        assert!(pos("EPP") < pos("gpu policy (0x21)"));
        assert!(pos("gpu policy (0x21)") < pos("set_thermal_mode"));
        assert!(pos("set_thermal_mode") < step_names.len() - 1);

        // Effects landed on the mock backends.
        let s = rig.hp.state();
        assert_eq!(s.thermal_writes, vec![ThermalMode::Performance]);
        assert_eq!(s.max_fan_writes, vec![true]);
        assert_eq!(s.gpu_policy_writes.len(), 1);
        assert!(
            !s.gpu_policy_writes[0].ctgp,
            "patch must merge over live 0x21"
        );
        assert!(
            s.gpu_policy_writes[0].ppab,
            "unset fields preserve live 0x21"
        );
        drop(s);
        // Observed + desired stamped; profile name recorded.
        let observed = rig.handle.observed();
        assert_eq!(observed.epp_ac.value(), Some(&33));
        assert_eq!(observed.max_fan.value(), Some(&true));
        assert_eq!(rig.handle.desired().profile.as_deref(), Some("mix"));
        rig.handle.shutdown();
    }

    #[test]
    fn profile_unknown_name_rejects_without_writes() {
        let rig = TestRig::start("prof-unknown");
        let o = block(
            &rig.handle,
            ControlCommand::ApplyProfile {
                profile: "nope".into(),
            },
        );
        assert!(matches!(
            o.status,
            ControlStatus::Rejected {
                error: ControlError::UnknownProfile { .. }
            }
        ));
        let s = rig.hp.state();
        assert!(s.thermal_writes.is_empty() && s.max_fan_writes.is_empty());
        drop(s);
        rig.handle.shutdown();
    }

    #[test]
    fn profile_partial_failure_stops_at_failing_step() {
        // Plan order PPM → … → thermal → fan: with thermal failing, EPP is
        // already applied (Partial carries it) and fan is NEVER written.
        let hp = MockHp::default();
        hp.state().fail_next_thermal = true;
        let mut p = PerformanceProfile::default();
        p.cpu.epp_ac = Some(21);
        p.thermal_mode = Some(ThermalMode::Performance);
        p.fan = Some(FanMode::Max);
        let rig = TestRig::start_with("prof-partial", hp, registry_with("boom", p));

        let o = block(
            &rig.handle,
            ControlCommand::ApplyProfile {
                profile: "boom".into(),
            },
        );
        assert!(matches!(o.status, ControlStatus::Partial));
        let s = rig.hp.state();
        assert!(
            s.max_fan_writes.is_empty(),
            "steps after the failure must not run"
        );
        drop(s);
        assert_eq!(rig.handle.observed().epp_ac.value(), Some(&21));
        // Partial apply does NOT stamp the profile name.
        assert_eq!(rig.handle.desired().profile, None);
        rig.handle.shutdown();
    }

    #[test]
    fn profile_field_failing_safety_rejects_whole_pre_write() {
        // EPP 101 is invalid: the whole profile must reject before ANY
        // step touches hardware (AR-11 — no half-applied intent).
        let mut p = PerformanceProfile::default();
        p.cpu.epp_ac = Some(101);
        p.thermal_mode = Some(ThermalMode::Performance);
        let rig = TestRig::start_with("prof-unsafe", MockHp::default(), registry_with("bad", p));
        let o = block(
            &rig.handle,
            ControlCommand::ApplyProfile {
                profile: "bad".into(),
            },
        );
        assert!(matches!(
            o.status,
            ControlStatus::Rejected {
                error: ControlError::UnsafeRequest { .. }
            }
        ));
        assert!(o.steps.is_empty(), "rejected pre-write: no steps may run");
        let s = rig.hp.state();
        assert!(s.thermal_writes.is_empty());
        drop(s);
        rig.handle.shutdown();
    }

    #[test]
    fn direct_knob_change_clears_profile_stamp() {
        let mut p = PerformanceProfile::default();
        p.thermal_mode = Some(ThermalMode::Performance);
        let rig = TestRig::start_with("prof-clear", MockHp::default(), registry_with("t", p));
        block(
            &rig.handle,
            ControlCommand::ApplyProfile {
                profile: "t".into(),
            },
        );
        assert_eq!(rig.handle.desired().profile.as_deref(), Some("t"));
        block(
            &rig.handle,
            ControlCommand::SetThermalMode(ThermalMode::Balanced),
        );
        assert_eq!(rig.handle.desired().profile, None);
        rig.handle.shutdown();
    }

    /// Stable build: a profile carrying power_limits must reject the WHOLE
    /// profile as Unsupported (the experimental feature is compiled out).
    #[cfg(not(feature = "experimental-hp-power-limits"))]
    #[test]
    fn profile_with_power_limits_rejected_in_stable_build() {
        let mut p = PerformanceProfile::default();
        p.cpu.epp_ac = Some(33);
        p.power_limits = Some(CpuPowerLimits {
            pl1_w: 45,
            pl2_w: 90,
            pl4_w: 0,
            cpu_gpu_concurrent_w: 0,
        });
        let rig = TestRig::start_with("prof-exp-gate", MockHp::default(), registry_with("x", p));
        let o = block(
            &rig.handle,
            ControlCommand::ApplyProfile {
                profile: "x".into(),
            },
        );
        assert!(matches!(
            o.status,
            ControlStatus::Rejected {
                error: ControlError::Unsupported
            }
        ));
        // Whole-profile rejection: even the (valid) EPP step never ran.
        assert_eq!(rig.handle.observed().epp_ac.value(), Some(&50)); // MockPpm initial
        rig.handle.shutdown();
    }

    /// Experimental build: the same profile passes the gate and applies
    /// all steps (the 0x29 step converges via the linked feed).
    #[cfg(feature = "experimental-hp-power-limits")]
    #[test]
    fn profile_with_power_limits_applies_in_experimental_build() {
        let link = std::sync::Arc::new(Mutex::new((55.0_f64, 130.0_f64, 200.0_f64)));
        let hp = MockHp::default();
        hp.state().msr_link = Some(std::sync::Arc::clone(&link));
        let mut p = PerformanceProfile::default();
        p.cpu.epp_ac = Some(33);
        p.power_limits = Some(CpuPowerLimits {
            pl1_w: 45,
            pl2_w: 90,
            pl4_w: 150,
            cpu_gpu_concurrent_w: 0,
        });
        p.thermal_mode = Some(ThermalMode::Performance);
        let mut reg = crate::profiles::ProfileRegistry::empty();
        reg.insert("x", p);

        let dir = std::env::temp_dir().join(format!(
            "phelper-coord-test-prof-exp-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let journal_path = dir.join("journal.jsonl");
        let ppm = MockPpm(std::sync::Arc::new(Mutex::new((50, 50, 50, 50))));
        let mut cfg = ControlConfig::new(
            caps_full(),
            test_identity("prof-exp"),
            Some(hp.clone()),
            ppm,
            PlFeed {
                link: std::sync::Arc::clone(&link),
                has_pl4: true,
            },
            journal_path,
        );
        cfg.verify_poll_interval = Duration::from_millis(5);
        cfg.keepalive_period = Duration::from_millis(120);
        cfg.safety_tick = Duration::from_millis(20);
        cfg.profiles = reg;
        let handle = ControlCoordinator::start(cfg).unwrap();

        let o = block(
            &handle,
            ControlCommand::ApplyProfile {
                profile: "x".into(),
            },
        );
        assert!(
            matches!(o.status, ControlStatus::Applied { .. }),
            "expected Applied, got {:?}",
            o.status
        );
        assert_eq!(handle.observed().epp_ac.value(), Some(&33));
        assert!(handle.observed().power_limits.is_verified());
        assert_eq!(handle.desired().profile.as_deref(), Some("x"));
        handle.shutdown();
        // Restore wrote the captured baseline (55/130/200) back.
        let writes = hp.state().power_limits_writes.clone();
        assert!(writes.len() >= 2);
        assert_eq!(
            writes.last(),
            Some(&CpuPowerLimits {
                pl1_w: 55,
                pl2_w: 130,
                pl4_w: 200,
                cpu_gpu_concurrent_w: 0,
            })
        );
    }
}
