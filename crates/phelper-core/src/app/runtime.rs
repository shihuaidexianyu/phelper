//! The app pump (§43/§44): ONE thread ("app-pump") owns the `Engine` and is
//! the only telemetry subscriber; the GPUI thread never touches a channel
//! or a hardware handle. It publishes an immutable `AppState` snapshot
//! behind a lock; the UI clones it once per tick.
//!
//! Loop (100 ms cadence):
//! 1. telemetry snapshots (drain to latest);
//! 2. UI messages (Dispatch / RefreshProfiles / Shutdown);
//! 3. coalescer poll → dispatch through `ControlHandle` (Busy → backoff);
//! 4. in-flight outcome sweep → evidence + desired/observed refresh;
//! 5. ~2 s desired/observed refresh; ~1 s journal tail.
//!
//! Shutdown is the AR-12 load-bearing path: `AppHandle::shutdown()` blocks
//! until `Engine::shutdown()` has restored firmware automatic state.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, mpsc};
use std::time::{Duration, Instant};

use phelper_domain::command::{ControlCommand, ControlOutcome};
use phelper_domain::error::ControlError;
use phelper_domain::telemetry::{MetricId, MetricSample, WindowStats};

use crate::Engine;
use crate::telemetry::TelemetryHandle;

use super::coalesce::{BusyVerdict, Coalescer};
use super::journal_tail::JournalTail;
use super::state::{AppState, EngineStatus, ExperimentalUi, KnobId, KnobStatus};
use super::{now_epoch_ms, validate};

enum PumpMsg {
    Dispatch(KnobId, ControlCommand),
    RefreshProfiles,
    Shutdown(mpsc::Sender<()>),
}

/// GPUI-thread handle to the pump. Clone freely; all clones talk to the
/// same pump (tray and window share it — §60.12).
#[derive(Clone)]
pub struct AppHandle {
    state: Arc<RwLock<AppState>>,
    to_pump: mpsc::Sender<PumpMsg>,
    telemetry: Arc<RwLock<Option<TelemetryHandle>>>,
}

impl AppHandle {
    /// Spawn the pump thread; returns immediately (engine startup happens
    /// on the pump — the UI renders the `Starting` state meanwhile).
    pub fn start() -> Self {
        let state = Arc::new(RwLock::new(AppState::default()));
        let (to_pump, rx) = mpsc::channel();
        let telemetry = Arc::new(RwLock::new(None));
        let handle = Self {
            state: Arc::clone(&state),
            to_pump,
            telemetry: Arc::clone(&telemetry),
        };
        std::thread::Builder::new()
            .name("app-pump".into())
            .spawn(move || pump_main(state, rx, telemetry))
            .expect("spawn app-pump thread");
        handle
    }

    /// The current immutable snapshot (one clone per UI tick).
    pub fn state(&self) -> AppState {
        self.state.read().expect("appstate poisoned").clone()
    }

    /// Enqueue a user intent. Validation happens UI-side first (validate.rs)
    /// for instant feedback; the pump re-applies the command-level gates.
    pub fn dispatch(&self, knob: KnobId, cmd: ControlCommand) {
        // Immediate feedback; the pump confirms or replaces this.
        self.state
            .write()
            .expect("appstate poisoned")
            .set_knob(knob, KnobStatus::Pending);
        let _ = self.to_pump.send(PumpMsg::Dispatch(knob, cmd));
    }

    pub fn refresh_profiles(&self) {
        let _ = self.to_pump.send(PumpMsg::RefreshProfiles);
    }

    /// §39 passthrough for charts (never a hardware call — the store).
    pub fn history(&self, id: MetricId, window: Duration) -> Vec<MetricSample> {
        self.telemetry
            .read()
            .expect("telemetry slot poisoned")
            .as_ref()
            .map(|t| t.history(id, window))
            .unwrap_or_default()
    }

    pub fn stats(&self, id: MetricId, window: Duration) -> Option<WindowStats> {
        self.telemetry
            .read()
            .expect("telemetry slot poisoned")
            .as_ref()
            .and_then(|t| t.stats(id, window))
    }

    /// Per-collector worst scheduling lateness (diagnostics page).
    pub fn scheduler_jitter(&self) -> std::collections::BTreeMap<&'static str, Duration> {
        self.telemetry
            .read()
            .expect("telemetry slot poisoned")
            .as_ref()
            .map(|t| t.scheduler_jitter())
            .unwrap_or_default()
    }

    /// AR-12 graceful shutdown: restores firmware automatic state, then
    /// acks. Blocks up to `timeout`; safe to call more than once.
    pub fn shutdown(&self, timeout: Duration) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.to_pump.send(PumpMsg::Shutdown(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(timeout);
        }
    }
}

fn pump_main(
    state: Arc<RwLock<AppState>>,
    rx: mpsc::Receiver<PumpMsg>,
    telemetry_slot: Arc<RwLock<Option<TelemetryHandle>>>,
) {
    let engine = match Engine::start() {
        Ok(e) => Some(e),
        Err(e) => {
            state.write().expect("appstate poisoned").engine = EngineStatus::Failed(e.to_string());
            None
        }
    };
    let Some(engine) = engine else {
        serve_shutdown_only(&rx);
        return;
    };
    let control = engine.control().cloned();
    *telemetry_slot.write().expect("telemetry slot poisoned") = Some(engine.telemetry().clone());
    let mut registry = crate::profiles::ProfileRegistry::load_default();

    {
        let mut s = state.write().expect("appstate poisoned");
        s.identity = Some(engine.identity().clone());
        s.ogh_findings = engine.ogh_findings().to_vec();
        if let Some(c) = &control {
            let caps = c.capabilities().clone();
            s.experimental = ExperimentalUi::compute(Some(&caps));
            s.caps = Some(caps);
            s.desired = c.desired();
            s.observed = c.observed();
            s.engine = EngineStatus::Running;
        } else {
            s.engine = EngineStatus::TelemetryOnly;
        }
        s.set_profiles(&registry);
    }

    let snap_rx = engine.telemetry().subscribe();
    let mut engine = Some(engine);
    let mut coalescer = Coalescer::new();
    let mut in_flight: BTreeMap<u64, (KnobId, mpsc::Receiver<ControlOutcome>)> = BTreeMap::new();
    let mut journal = JournalTail::default_journal();
    let mut last_state_refresh = Instant::now() - Duration::from_secs(60);
    let mut last_journal = Instant::now() - Duration::from_secs(60);
    let mut last_observed_reprobe = Instant::now();

    loop {
        // 1. Telemetry: drain to the latest snapshot only.
        match snap_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(latest) => {
                let mut newest = latest;
                while let Ok(n) = snap_rx.try_recv() {
                    newest = n;
                }
                state.write().expect("appstate poisoned").apply_snapshot(newest);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                state.write().expect("appstate poisoned").engine =
                    EngineStatus::Failed("遥测协调器意外断开".into());
                if let Some(e) = engine.take() {
                    drop(e);
                }
                serve_shutdown_only(&rx);
                return;
            }
        }

        // 2. UI messages.
        while let Ok(msg) = rx.try_recv() {
            match msg {
                PumpMsg::Dispatch(knob, cmd) => {
                    let fail = dispatch_gate(&cmd, &registry, &state, control.is_some());
                    if let Some(err) = fail {
                        state.write().expect("appstate poisoned").set_knob(
                            knob,
                            KnobStatus::Failed {
                                error: err,
                                at_epoch_ms: now_epoch_ms(),
                            },
                        );
                    } else {
                        coalescer.enqueue(knob, cmd);
                        state
                            .write()
                            .expect("appstate poisoned")
                            .set_knob(knob, KnobStatus::Pending);
                    }
                }
                PumpMsg::RefreshProfiles => {
                    registry = crate::profiles::ProfileRegistry::load_default();
                    // The display registry refreshes; the COORDINATOR's
                    // registry was loaded at engine start and does NOT hot-
                    // reload (known M6 limitation — a mid-session TOML edit
                    // can make apply answer UnknownProfile; surfaced as-is).
                    state.write().expect("appstate poisoned").set_profiles(&registry);
                }
                PumpMsg::Shutdown(ack) => {
                    if let Some(e) = engine.take() {
                        e.shutdown();
                    }
                    let _ = ack.send(());
                    return;
                }
            }
        }

        // 3. Dispatch whatever the coalescer releases.
        let now = Instant::now();
        for (knob, cmd) in coalescer.poll(now) {
            let Some(c) = &control else {
                state.write().expect("appstate poisoned").set_knob(
                    knob,
                    KnobStatus::Failed {
                        error: ControlError::BackendUnavailable {
                            what: "控制协调器（遥测-only 模式）".into(),
                        },
                        at_epoch_ms: now_epoch_ms(),
                    },
                );
                continue;
            };
            match c.dispatch(cmd.clone()) {
                Ok((receipt, outcome_rx)) => {
                    coalescer.note_dispatched(knob, cmd, receipt, now);
                    in_flight.insert(receipt.0, (knob, outcome_rx));
                    state
                        .write()
                        .expect("appstate poisoned")
                        .set_knob(knob, KnobStatus::InFlight(receipt));
                }
                Err(ControlError::Busy) => match coalescer.note_busy(knob, cmd, now) {
                    BusyVerdict::Retry => {
                        state
                            .write()
                            .expect("appstate poisoned")
                            .set_knob(knob, KnobStatus::Pending);
                    }
                    BusyVerdict::TimedOut => {
                        state.write().expect("appstate poisoned").set_knob(
                            knob,
                            KnobStatus::Failed {
                                error: ControlError::Busy,
                                at_epoch_ms: now_epoch_ms(),
                            },
                        );
                    }
                },
                Err(e) => {
                    coalescer.note_dispatch_error(knob);
                    state.write().expect("appstate poisoned").set_knob(
                        knob,
                        KnobStatus::Failed {
                            error: e,
                            at_epoch_ms: now_epoch_ms(),
                        },
                    );
                }
            }
        }

        // 4. Outcome sweep.
        let mut finished: Vec<(u64, KnobId, Option<ControlOutcome>)> = Vec::new();
        for (rid, (knob, outcome_rx)) in &in_flight {
            match outcome_rx.try_recv() {
                Ok(outcome) => finished.push((*rid, *knob, Some(outcome))),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => finished.push((*rid, *knob, None)),
            }
        }
        for (rid, knob, maybe) in finished {
            in_flight.remove(&rid);
            // Failed/rejected outcomes clear the coalescer's dedup memory
            // (note_completed false) so the user can retry the same value.
            let succeeded = matches!(
                &maybe,
                Some(o) if matches!(o.status, phelper_domain::command::ControlStatus::Applied { .. })
            );
            coalescer.note_completed(knob, succeeded);
            let mut s = state.write().expect("appstate poisoned");
            match maybe {
                Some(outcome) => s.apply_outcome(knob, outcome),
                None => s.set_knob(
                    knob,
                    KnobStatus::Failed {
                        error: ControlError::BackendUnavailable {
                            what: "控制协调器通道断开".into(),
                        },
                        at_epoch_ms: now_epoch_ms(),
                    },
                ),
            }
            if let Some(c) = &control {
                s.desired = c.desired();
                s.observed = c.observed();
            }
        }

        // 5. Periodic desired/observed refresh (~2 s).
        if last_state_refresh.elapsed() >= Duration::from_secs(2) {
            last_state_refresh = Instant::now();
            if let Some(c) = &control {
                let mut s = state.write().expect("appstate poisoned");
                s.desired = c.desired();
                s.observed = c.observed();
            }
        }

        // 5b. ~30 s: the COORDINATOR re-probes the hardware behind its
        // observed stamps (EPP/EPP1/0x21). Step 5 only copies the cache —
        // without this the UI could pose a startup read as live truth for
        // the whole session. Read-only.
        if last_observed_reprobe.elapsed() >= Duration::from_secs(30) {
            last_observed_reprobe = Instant::now();
            if let Some(c) = &control {
                c.refresh_observed();
            }
        }

        // 6. Journal live tail (~1 s; cross-process — CLI writes show up).
        if last_journal.elapsed() >= Duration::from_secs(1) {
            last_journal = Instant::now();
            let entries = journal.poll();
            if !entries.is_empty() {
                state
                    .write()
                    .expect("appstate poisoned")
                    .apply_journal(entries);
            }
        }
    }
}

/// Command-level client gates applied by the pump (per-value validation
/// already happened UI-side). `Some(err)` = reject before the coalescer.
fn dispatch_gate(
    cmd: &ControlCommand,
    registry: &crate::profiles::ProfileRegistry,
    state: &Arc<RwLock<AppState>>,
    has_control: bool,
) -> Option<ControlError> {
    if !has_control {
        return Some(ControlError::BackendUnavailable {
            what: "控制协调器（遥测-only 模式）".into(),
        });
    }
    if let ControlCommand::ApplyProfile { profile } = cmd {
        // Double-gate check against the display registry's copy of the
        // profile. Unknown name → let the coordinator answer honestly.
        if let Some((_, p, _)) = registry.iter().find(|(n, _, _)| n == profile) {
            let s = state.read().expect("appstate poisoned");
            if let Err(reason) =
                validate::profile_apply_gate(p, super::EXPERIMENTAL_COMPILED, s.caps.as_ref())
            {
                return Some(ControlError::UnsafeRequest { reason });
            }
        }
    }
    None
}

/// Engine never started (or died): keep answering Shutdown so the UI can
/// always exit cleanly.
fn serve_shutdown_only(rx: &mpsc::Receiver<PumpMsg>) {
    while let Ok(msg) = rx.recv() {
        if let PumpMsg::Shutdown(ack) = msg {
            let _ = ack.send(());
            return;
        }
    }
}
