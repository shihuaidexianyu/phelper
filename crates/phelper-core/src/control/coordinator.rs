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
use phelper_domain::policy::{CpuPolicy, FanLevels, FanMode, ThermalMode};
use phelper_domain::ports::{CpuPolicyBackend, HpBackend};
use phelper_domain::state::{DesiredState, ObservedState, ObservedValue};
use phelper_domain::telemetry::ids;
use tracing::{debug, info, warn};

use crate::telemetry::TelemetryHandle;

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
}

impl Clone for ControlHandle {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            receipt_next: Arc::clone(&self.receipt_next),
            caps: Arc::clone(&self.caps),
            desired: Arc::clone(&self.desired),
            observed: Arc::clone(&self.observed),
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
    keepalive: KeepAliveService,
    verify_polls: u32,
    verify_poll_interval: Duration,
    safety_tick: Duration,
    desired: Arc<RwLock<DesiredState>>,
    observed: Arc<RwLock<ObservedState>>,
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
        let observed = Arc::new(RwLock::new(Self::initial_observed(&cfg.ppm)));
        let caps = Arc::new(cfg.caps);
        let handle = ControlHandle {
            tx,
            receipt_next: Arc::new(AtomicU64::new(1)),
            caps: Arc::clone(&caps),
            desired: Arc::clone(&desired),
            observed: Arc::clone(&observed),
        };
        let coord = Self {
            rx,
            caps: (*caps).clone(),
            hp: cfg.hp,
            ppm: cfg.ppm,
            feed: cfg.feed,
            journal,
            safety: SafetySupervisor::new(),
            keepalive: KeepAliveService::with_period(cfg.keepalive_period),
            verify_polls: cfg.verify_polls,
            verify_poll_interval: cfg.verify_poll_interval,
            safety_tick: cfg.safety_tick,
            desired,
            observed,
        };
        std::thread::Builder::new()
            .name("control-coord".into())
            .spawn(move || coord.run())
            .map_err(|e| EngineError::Config(format!("spawn control-coord: {e}")))?;
        Ok(handle)
    }

    /// EPP is read back at start (Verified); everything else is Unknown
    /// until written or proven (AR-10: we don't claim states we never saw).
    fn initial_observed(ppm: &P) -> ObservedState {
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
        o
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
        let fan_held = matches!(observed.fan_mode.value(), Some(FanMode::Manual(_)))
            || matches!(observed.max_fan.value(), Some(true))
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
        }
        if self.keepalive.is_due(now) {
            self.run_heartbeat(now);
        }
    }

    // ------------------------------------------------------------ dispatch

    fn execute(&mut self, receipt: ControlReceipt, cmd: ControlCommand) -> ControlOutcome {
        let started = Instant::now();
        info!(receipt = receipt.0, ?cmd, "control dispatch");

        // 1. Validate (safety layer; capability + range + freshness gates).
        let observed = self.observed();
        if let Err(error) = self
            .safety
            .validate(&cmd, &self.caps, &self.feed, &observed)
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

        // 2. Execute + verify (per command kind).
        let mut steps = Vec::new();
        let status = match &cmd {
            ControlCommand::SetThermalMode(mode) => self.exec_thermal(*mode, &mut steps),
            ControlCommand::SetFanMode(mode) => self.exec_fan_mode(*mode, &mut steps),
            ControlCommand::SetCpuPolicy(policy) => self.exec_cpu_policy(policy, &mut steps),
            // validate() already rejected these; unreachable in practice.
            _ => ControlStatus::Rejected {
                error: ControlError::Unsupported,
            },
        };

        // 3. Desired state records intent for accepted commands.
        if !matches!(status, ControlStatus::Rejected { .. }) {
            self.record_desired(&cmd);
        }

        // 4. Reschedule keep-alive against the new observed state.
        self.keepalive.reschedule(&self.observed(), Instant::now());

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

    fn record_desired(&mut self, cmd: &ControlCommand) {
        let mut d = self.desired.write().expect("desired poisoned");
        match cmd {
            ControlCommand::SetThermalMode(m) => d.thermal_mode = Some(*m),
            ControlCommand::SetFanMode(m) => d.fan_mode = Some(*m),
            ControlCommand::SetCpuPolicy(p) => d.cpu_policy = Some(p.clone()),
            _ => {}
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
                steps.push(failed_step(
                    "set_thermal_mode",
                    "hp-wmi 0x1A",
                    &e,
                    before,
                ));
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
                if !matches!(current.fan_mode.value(), Some(FanMode::FirmwareAuto)) {
                    ok &= Self::fan_write(
                        steps,
                        "fan->auto (0x2E {0,0})",
                        "hp-wmi 0x2E",
                        &before,
                        || hp.set_fan_levels(FanLevels::AUTO),
                    );
                }
                if !matches!(current.max_fan.value(), Some(false)) {
                    ok &= Self::fan_write(
                        steps,
                        "max-fan off (0x27 0)",
                        "hp-wmi 0x27",
                        &before,
                        || hp.set_max_fan(false),
                    );
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
                    self.safety.note_user_fan_mode(FanMode::FirmwareAuto);
                    ControlStatus::Applied {
                        verification: Verification::Skipped, // firmware retakes control
                    }
                } else {
                    ControlStatus::Partial
                }
            }
            FanMode::Max => {
                // Manual → Max goes through auto first (§27 priority).
                if matches!(current.fan_mode.value(), Some(FanMode::Manual(_)))
                    && !Self::fan_write(
                        steps,
                        "manual->auto before max (0x2E {0,0})",
                        "hp-wmi 0x2E",
                        &before,
                        || hp.set_fan_levels(FanLevels::AUTO),
                    )
                {
                    return ControlStatus::Partial;
                }
                if Self::fan_write(steps, "max-fan on (0x27 1)", "hp-wmi 0x27", &before, || {
                    hp.set_max_fan(true)
                }) {
                    self.set_observed(|o| {
                        o.fan_mode = ObservedValue::TrustedWrite {
                            value: FanMode::FirmwareAuto,
                            at: Instant::now(),
                        };
                        o.max_fan = ObservedValue::TrustedWrite {
                            value: true,
                            at: Instant::now(),
                        };
                    });
                    self.safety.note_user_fan_mode(FanMode::Max);
                    ControlStatus::Applied {
                        verification: Verification::TrustedNoReadback, // 0x26 unreliable
                    }
                } else {
                    ControlStatus::Partial
                }
            }
            FanMode::Manual(target) => {
                if !Self::fan_write(
                    steps,
                    "set manual fan levels",
                    "hp-wmi 0x2E",
                    &before,
                    || hp.set_fan_levels(target),
                ) {
                    return ControlStatus::Rejected {
                        error: ControlError::FirmwareRejected {
                            detail: "0x2E write failed".into(),
                        },
                    };
                }
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

    // ------------------------------------------------------------ CPU policy

    /// §32 order: EPP → max-freq → boost. Steps are independent settings —
    /// a later failure leaves earlier steps applied (Partial; no M2
    /// rollback, journal carries the evidence).
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
                                    expected: format!(
                                        "ac={:?} dc={:?}",
                                        p.epp_ac, p.epp_dc
                                    ),
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
                    steps.push(platform_failed_step("write EPP", "powrprof PERFEPP", &e, before));
                }
            }
        }

        if p.max_freq_mhz_ac.is_some() || p.max_freq_mhz_dc.is_some() {
            let before = self
                .ppm
                .read_max_freq_mhz()
                .map(|(ac, dc)| format!("maxfreq ac={ac} dc={dc}"))
                .unwrap_or_else(|e| format!("maxfreq unreadable: {e}"));
            match self.ppm.write_max_freq_mhz(p.max_freq_mhz_ac, p.max_freq_mhz_dc) {
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

    fn run_safety_action(&mut self, action: SafetyAction) {
        warn!(?action, "safety action");
        let started = Instant::now();
        let mut steps = Vec::new();
        match action {
            SafetyAction::ForceMaxFan => {
                if let Some(hp) = &self.hp {
                    let _ = Self::fan_write(
                        &mut steps,
                        "SAFETY max-fan on",
                        "hp-wmi 0x27",
                        "thermal override",
                        || hp.set_max_fan(true),
                    );
                    self.set_observed(|o| {
                        o.max_fan = ObservedValue::TrustedWrite {
                            value: true,
                            at: Instant::now(),
                        };
                    });
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

    /// Heartbeat tick: 0x10 fan-count-get keeps the firmware's
    /// user-defined states alive; then re-assert every non-default
    /// TrustedWrite (clawback repair). Steady-state success is NOT
    /// journaled (R6) — only failures are.
    fn run_heartbeat(&mut self, now: Instant) {
        let Some(hp) = &self.hp else {
            self.keepalive.record_success(now); // nothing to heartbeat against
            return;
        };
        let tracked = KeepAliveService::tracked(&self.observed());
        if tracked.is_empty() {
            self.keepalive.reschedule(&self.observed(), now);
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
                        _ => Ok(()),
                    },
                    ReAssert::MaxFan => hp.set_max_fan(true),
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

    /// AR-12 restore: 0x2E{0,0} + 0x27 off + thermal Balanced. Best-effort —
    /// every step is attempted even if earlier ones fail; the firmware
    /// clawback (~120 s) is the ultimate backstop regardless.
    /// EPP/max-freq/boost are deliberately NOT restored: they are
    /// Windows-native settings with no firmware-session semantics.
    fn restore_firmware_auto(&mut self, origin: JournalOrigin) {
        let started = Instant::now();
        let mut steps = Vec::new();
        if let Some(hp) = &self.hp {
            let _ = Self::fan_write(&mut steps, "restore fan auto", "hp-wmi 0x2E", "restore", || {
                hp.set_fan_levels(FanLevels::AUTO)
            });
            let _ = Self::fan_write(&mut steps, "restore max-fan off", "hp-wmi 0x27", "restore", || {
                hp.set_max_fan(false)
            });
            match hp.set_thermal_mode(ThermalMode::Balanced) {
                Ok(()) => steps.push(StepOutcome {
                    step: "restore thermal balanced".into(),
                    backend: "hp-wmi 0x1A".into(),
                    firmware_return: Some("rc=0".into()),
                    before: Some("restore".into()),
                    after: None,
                    verification: Verification::TrustedNoReadback,
                }),
                Err(e) => steps.push(failed_step(
                    "restore thermal balanced",
                    "hp-wmi 0x1A",
                    &e,
                    "restore".into(),
                )),
            }
        }
        self.set_observed(|o| {
            o.fan_mode = ObservedValue::TrustedWrite {
                value: FanMode::FirmwareAuto,
                at: Instant::now(),
            };
            o.max_fan = ObservedValue::TrustedWrite {
                value: false,
                at: Instant::now(),
            };
            o.thermal_mode = ObservedValue::TrustedWrite {
                value: ThermalMode::Balanced,
                at: Instant::now(),
            };
        });
        self.safety.note_user_fan_mode(FanMode::FirmwareAuto);
        self.keepalive.reschedule(&self.observed(), Instant::now());
        self.journal(
            origin,
            &ControlOutcome {
                receipt: ControlReceipt(0),
                command: ControlCommand::SetFanMode(FanMode::FirmwareAuto),
                status: ControlStatus::Applied {
                    verification: Verification::Skipped,
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

    fn journal(&mut self, origin: JournalOrigin, outcome: &ControlOutcome) {
        let entry = self.journal.new_entry(origin, outcome.clone());
        if let Err(e) = self.journal.append(&entry) {
            warn!(%e, "control journal append failed");
        }
    }
}

// ------------------------------------------------------------ error mapping

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::capability::{FanScale, Support};
    use phelper_domain::error::PlatformError;
    use phelper_domain::hp::{FanTable, SystemDesignData};
    use phelper_domain::identity::{CpuIdentity, DeviceIdentity};
    use phelper_domain::policy::{BoostPolicy, GpuPlatformPolicy, MuxMode};
    use phelper_domain::ports::{HpControl, HpPlatform};
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
            Err(HpWmiError::NotAvailable("mock"))
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
    }

    #[derive(Clone, Default)]
    struct MockPpm(std::sync::Arc<Mutex<(u8, u8)>>);

    impl CpuPolicyBackend for MockPpm {
        fn read_epp(&self) -> Result<(u8, u8), PlatformError> {
            Ok(*self.0.lock().unwrap())
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
        c.ppm.max_freq = Support::Supported;
        c.ppm.write_privileged = true;
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
            let dir = std::env::temp_dir()
                .join(format!("phelper-coord-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let journal_path = dir.join("journal.jsonl");
            let ppm = MockPpm(std::sync::Arc::new(Mutex::new((50, 50))));
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
    fn queue_full_reports_busy() {
        let rig = TestRig::start("busy");
        // Occupy the coordinator thread with a slow verification (5 polls
        // x 5 ms, readback never converges) so the queue can fill.
        rig.hp.state().readback_script = vec![FanLevels::new(10, 10)];
        let (r1, _rx1) = rig
            .handle
            .dispatch(ControlCommand::SetFanMode(FanMode::Manual(
                FanLevels::new(50, 50),
            )))
            .unwrap();
        assert_eq!(r1, ControlReceipt(1));
        let mut busy = false;
        for _ in 0..(QUEUE_DEPTH + 4) {
            match rig.handle.dispatch(ControlCommand::SetFanMode(FanMode::Max)) {
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
        }

        let dir = std::env::temp_dir().join(format!("phelper-coord-test-hyst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let journal_path = dir.join("journal.jsonl");
        let temp = std::sync::Arc::new(Mutex::new(70.0_f64));
        let hp = MockHp::default();
        let ppm = MockPpm(std::sync::Arc::new(Mutex::new((50, 50))));
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
}
