//! The app pump (§43/§44): ONE thread ("app-pump") owns the `Engine` and is
//! the only telemetry subscriber; the GPUI thread never touches a channel
//! or a hardware handle. The pump publishes AppState updates through a
//! `StatePublisher` bridge — that bridge is what moves ownership from
//! `Arc<RwLock<AppState>>` (lock-based, owned by pump) to GPUI's
//! `Entity<AppState>` (framework-managed, observed by the shell).
//!
//! Loop (up to 100 ms idle cadence):
//! 1. telemetry snapshots (drain to latest);
//! 2. UI messages (Dispatch / Shutdown); the pump waits on
//!    this channel, so a control request wakes it immediately;
//! 3. coalescer poll → dispatch through `ControlHandle` (Busy → backoff);
//! 4. in-flight outcome sweep → evidence + desired/observed refresh;
//! 5. ~2 s desired/observed refresh.
//!
//! Shutdown is the AR-12 load-bearing path: `AppHandle::shutdown()` blocks
//! until `Engine::shutdown()` has restored firmware automatic state.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, mpsc};
use std::time::{Duration, Instant};

use phelper_domain::command::{ControlCommand, ControlOutcome};
use phelper_domain::error::ControlError;

use crate::Engine;

use super::coalesce::{BusyVerdict, Coalescer};
use super::state::{AppState, EngineStatus, KnobId, KnobStatus};
use super::{now_epoch_ms, validate};

enum PumpMsg {
    Dispatch(KnobId, ControlCommand),
    Shutdown(mpsc::Sender<()>),
}

/// Bridge from the pump thread to whatever owns the live `AppState`. The
/// GPUI shell injects a GPUI-backed publisher that maps to `Entity<AppState>`;
/// tests inject an `RwLockStatePublisher` so they can drive the pump without
/// pulling in gpui. Either way the pump writes through this trait and never
/// touches a GPUI handle directly (phelper-core has no gpui dep).
pub trait StatePublisher: Send + Sync + 'static {
    /// Apply a batch of mutations and trigger observer notifications.
    /// Implementations must notify any registered observers iff the state
    /// actually changed (caller fingerprint) — for simplicity the pump's
    /// apply closure is opaque, so the publisher is expected to ALWAYS
    /// notify and let observer-side fingerprints filter.
    fn update(&self, apply: Box<dyn FnOnce(&mut AppState) + Send>);

    /// Snapshot the current state for profile validation and UI reads.
    fn snapshot(&self) -> AppState;
}

/// In-process publisher backed by `Arc<RwLock<AppState>>`. Used by tests
/// and any consumer that doesn't want a GPUI dependency. Notifies by
/// bumping an internal version counter — observers must poll it.
pub struct RwLockStatePublisher {
    state: Arc<RwLock<AppState>>,
    /// Bumped every successful `update`. Tests subscribe to it to confirm
    /// a notification fired without needing a real observer chain.
    version: Arc<std::sync::atomic::AtomicU64>,
}

impl RwLockStatePublisher {
    pub fn new(state: Arc<RwLock<AppState>>) -> Self {
        Self {
            state,
            version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Build two publishers that share a single version counter — mirrors
    /// the GPUI Entity<T> invariant that one entity has one dirty bit.
    pub fn new_with_version(
        state: Arc<RwLock<AppState>>,
        version: Arc<std::sync::atomic::AtomicU64>,
    ) -> Self {
        Self { state, version }
    }

    /// Internal version counter for tests. Each successful `update` bumps it.
    pub fn version(&self) -> u64 {
        self.version.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl StatePublisher for RwLockStatePublisher {
    fn update(&self, apply: Box<dyn FnOnce(&mut AppState) + Send>) {
        let mut guard = self.state.write().expect("appstate poisoned");
        apply(&mut guard);
        self.version
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn snapshot(&self) -> AppState {
        self.state.read().expect("appstate poisoned").clone()
    }
}

/// GPUI-thread handle to the pump. Clone freely; all clones talk to the
/// same pump (tray and window share it — §60.12). The publisher is shared
/// with whoever owns the live `AppState` (the shell in production, a test
/// harness in unit tests).
#[derive(Clone)]
pub struct AppHandle {
    publisher: Arc<dyn StatePublisher>,
    to_pump: mpsc::Sender<PumpMsg>,
}

impl AppHandle {
    /// Spawn the pump thread with a caller-provided state publisher. The
    /// publisher is shared with the pump; every `dispatch` / `set_*` path
    /// that used to mutate the AppState lock now flows through the publisher
    /// (which, in production, is a GPUI `Entity<AppState>` bridge).
    pub fn start_with_publisher(publisher: Arc<dyn StatePublisher>) -> Self {
        let (to_pump, rx) = mpsc::channel();
        let handle = Self {
            publisher: Arc::clone(&publisher),
            to_pump,
        };
        std::thread::Builder::new()
            .name("app-pump".into())
            .spawn(move || pump_main(publisher, rx))
            .expect("spawn app-pump thread");
        handle
    }

    /// The current immutable snapshot. The desktop shell keeps a local copy
    /// updated by the publisher and uses this once during construction.
    pub fn state(&self) -> AppState {
        self.publisher.snapshot()
    }

    /// Enqueue a user intent. Validation happens UI-side first (validate.rs),
    /// and the pump re-applies the command-level gates before dispatch.
    pub fn dispatch(&self, knob: KnobId, cmd: ControlCommand) {
        let _ = self.to_pump.send(PumpMsg::Dispatch(knob, cmd));
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

fn pump_main(publisher: Arc<dyn StatePublisher>, rx: mpsc::Receiver<PumpMsg>) {
    let engine = match Engine::start_without_ogh_scan() {
        Ok(e) => Some(e),
        Err(e) => {
            let message = e.to_string();
            publisher.update(Box::new(move |s| {
                s.engine = EngineStatus::Failed(message);
            }));
            None
        }
    };
    let Some(engine) = engine else {
        serve_shutdown_only(&rx);
        return;
    };
    let control = engine.control().cloned();
    // The remaining UI exposes built-ins only, so do not scan user profile
    // files a second time after Engine startup.
    let registry = crate::profiles::ProfileRegistry::with_builtins();

    // Initial state: clone every Arc-shaped handle into the closure so the
    // 'static-bound on `Box<dyn FnOnce + Send>` is satisfied without
    // borrowing the pump's stack-frames.
    let control_init = control.clone();
    let registry_init = registry.clone();
    publisher.update(Box::new(move |s| {
        if let Some(c) = control_init {
            let caps = c.capabilities().clone();
            s.caps = Some(caps);
            s.desired = c.desired();
            s.observed = c.observed();
            s.engine = EngineStatus::Running;
        } else {
            s.engine = EngineStatus::TelemetryOnly;
        }
        s.set_profiles(&registry_init);
    }));

    let snap_rx = engine.telemetry().subscribe();
    let mut engine = Some(engine);
    let mut coalescer = Coalescer::new();
    let mut in_flight: BTreeMap<u64, (KnobId, mpsc::Receiver<ControlOutcome>)> = BTreeMap::new();
    let mut last_state_refresh = Instant::now() - Duration::from_secs(60);
    let mut last_observed_reprobe = Instant::now();

    loop {
        // v0.2-e: stage timing. The M6 HIL saw ONE ~38 s window-close whose
        // root cause was never isolated (every call in this loop is
        // non-blocking by audit). Any iteration over SLOW_ITER logs its
        // per-stage durations — a recurrence names the stage in the log.
        const SLOW_ITER: Duration = Duration::from_secs(2);
        let iter_start = Instant::now();
        let mut stages = [Duration::ZERO; 4];

        // 1. Telemetry: drain to the latest snapshot only. Do not wait on
        // this channel: waiting here used to make a user command sit behind
        // the 100 ms telemetry timeout. The UI channel below is now the
        // blocking wait and wakes the pump as soon as a command arrives.
        match snap_rx.try_recv() {
            Ok(latest) => {
                let mut newest = latest;
                while let Ok(n) = snap_rx.try_recv() {
                    newest = n;
                }
                publisher.update(Box::new(move |s| {
                    s.apply_snapshot(newest);
                }));
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                publisher.update(Box::new(|s| {
                    s.engine = EngineStatus::Failed("遥测协调器意外断开".into());
                }));
                if let Some(e) = engine.take() {
                    drop(e);
                }
                serve_shutdown_only(&rx);
                return;
            }
        }
        stages[0] = iter_start.elapsed();
        let t = Instant::now();

        // 2. UI messages. Waiting on the UI channel keeps the idle loop
        // cheap while making Dispatch/Shutdown event-driven rather than
        // dependent on the telemetry cadence.
        let mut shutdown_ack = None;
        let ui_wait = if coalescer.has_work() {
            // While a command is active, keep outcome/next-value latency
            // below one frame without making the idle pump spin.
            Duration::from_millis(10)
        } else {
            Duration::from_millis(100)
        };
        match rx.recv_timeout(ui_wait) {
            Ok(msg) => {
                shutdown_ack =
                    handle_pump_msg(msg, &publisher, &registry, &mut coalescer, control.as_ref());
                if shutdown_ack.is_none() {
                    while let Ok(msg) = rx.try_recv() {
                        shutdown_ack = handle_pump_msg(
                            msg,
                            &publisher,
                            &registry,
                            &mut coalescer,
                            control.as_ref(),
                        );
                        if shutdown_ack.is_some() {
                            break;
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(e) = engine.take() {
                    e.shutdown();
                }
                return;
            }
        }
        if let Some(ack) = shutdown_ack {
            let t_sd = Instant::now();
            if let Some(e) = engine.take() {
                e.shutdown();
            }
            tracing::info!(
                elapsed_ms = t_sd.elapsed().as_millis(),
                "app-pump: engine.shutdown() completed (AR-12 restore path)"
            );
            let _ = ack.send(());
            return;
        }
        stages[1] = t.elapsed();
        let t = Instant::now();

        // 3. Dispatch whatever the coalescer releases.
        let now = Instant::now();
        for (knob, cmd) in coalescer.poll(now) {
            let Some(c) = &control else {
                let at = now_epoch_ms();
                publisher.update(Box::new(move |s| {
                    s.set_knob(
                        knob,
                        KnobStatus::Failed {
                            error: ControlError::BackendUnavailable {
                                what: "控制协调器（遥测-only 模式）".into(),
                            },
                            at_epoch_ms: at,
                        },
                    );
                }));
                continue;
            };
            match c.dispatch(cmd.clone()) {
                Ok((receipt, outcome_rx)) => {
                    coalescer.note_dispatched(knob, cmd, receipt, now);
                    in_flight.insert(receipt.0, (knob, outcome_rx));
                    publisher.update(Box::new(move |s| {
                        s.set_knob(knob, KnobStatus::InFlight(receipt));
                    }));
                }
                Err(ControlError::Busy) => match coalescer.note_busy(knob, cmd, now) {
                    BusyVerdict::Retry => {
                        publisher.update(Box::new(move |s| {
                            s.set_knob(knob, KnobStatus::Pending);
                        }));
                    }
                    BusyVerdict::TimedOut => {
                        let at = now_epoch_ms();
                        publisher.update(Box::new(move |s| {
                            s.set_knob(
                                knob,
                                KnobStatus::Failed {
                                    error: ControlError::Busy,
                                    at_epoch_ms: at,
                                },
                            );
                        }));
                    }
                },
                Err(e) => {
                    coalescer.note_dispatch_error(knob);
                    let at = now_epoch_ms();
                    publisher.update(Box::new(move |s| {
                        s.set_knob(
                            knob,
                            KnobStatus::Failed {
                                error: e,
                                at_epoch_ms: at,
                            },
                        );
                    }));
                }
            }
        }
        stages[2] = t.elapsed();
        let t = Instant::now();

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
            let knob_for_outcome = knob;
            let at = now_epoch_ms();
            publisher.update(Box::new(move |s| match maybe {
                Some(outcome) => s.apply_outcome(knob_for_outcome, outcome),
                None => {
                    s.set_knob(
                        knob_for_outcome,
                        KnobStatus::Failed {
                            error: ControlError::BackendUnavailable {
                                what: "控制协调器通道断开".into(),
                            },
                            at_epoch_ms: at,
                        },
                    );
                }
            }));
            if let Some(c) = control.clone() {
                publisher.update(Box::new(move |s| {
                    s.desired = c.desired();
                    s.observed = c.observed();
                }));
            }
        }
        // 5. Periodic desired/observed refresh (~2 s).
        if last_state_refresh.elapsed() >= Duration::from_secs(2) {
            last_state_refresh = Instant::now();
            if let Some(c) = control.clone() {
                publisher.update(Box::new(move |s| {
                    s.desired = c.desired();
                    s.observed = c.observed();
                }));
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
        stages[3] = t.elapsed();

        let total = iter_start.elapsed();
        if total > SLOW_ITER {
            tracing::warn!(
                total_ms = total.as_millis(),
                snap_ms = stages[0].as_millis(),
                ui_msgs_ms = stages[1].as_millis(),
                dispatch_ms = stages[2].as_millis(),
                state_ms = stages[3].as_millis(),
                "app-pump iteration exceeded 2 s — stage timings above"
            );
        }
    }
}

/// Apply one pump message. Returning a shutdown acknowledgement keeps the
/// engine-owning shutdown path in `pump_main`, while allowing the message
/// receive itself to be event-driven.
fn handle_pump_msg(
    msg: PumpMsg,
    publisher: &Arc<dyn StatePublisher>,
    registry: &crate::profiles::ProfileRegistry,
    coalescer: &mut Coalescer,
    control: Option<&crate::control::ControlHandle>,
) -> Option<mpsc::Sender<()>> {
    match msg {
        PumpMsg::Dispatch(knob, cmd) => {
            let fail = dispatch_gate(&cmd, registry, publisher, control.is_some());
            if let Some(err) = fail {
                publisher.update(Box::new(move |s| {
                    s.set_knob(
                        knob,
                        KnobStatus::Failed {
                            error: err,
                            at_epoch_ms: now_epoch_ms(),
                        },
                    );
                }));
            } else {
                if coalescer.enqueue(knob, cmd) {
                    publisher.update(Box::new(move |s| {
                        s.set_knob(knob, KnobStatus::Pending);
                    }));
                }
            }
            None
        }
        PumpMsg::Shutdown(ack) => Some(ack),
    }
}

/// Command-level client gates applied by the pump (per-value validation
/// already happened UI-side). `Some(err)` = reject before the coalescer.
fn dispatch_gate(
    cmd: &ControlCommand,
    registry: &crate::profiles::ProfileRegistry,
    publisher: &Arc<dyn StatePublisher>,
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
            let s = publisher.snapshot();
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

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::command::{ControlReceipt, ControlStatus, Verification};
    use phelper_domain::policy::ThermalMode;
    use std::time::Duration;

    fn outcome(status: ControlStatus) -> ControlOutcome {
        ControlOutcome {
            receipt: ControlReceipt(1),
            command: ControlCommand::SetThermalMode(ThermalMode::Balanced),
            status,
            steps: Vec::new(),
            duration: Duration::from_millis(5),
        }
    }

    #[test]
    fn publisher_update_bumps_version_and_applies() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let pub_ = RwLockStatePublisher::new(Arc::clone(&state));
        assert_eq!(pub_.version(), 0);
        pub_.update(Box::new(|s| {
            s.set_knob(KnobId::Profile, KnobStatus::InFlight(ControlReceipt(7)));
        }));
        assert_eq!(pub_.version(), 1);
        assert!(matches!(
            pub_.snapshot().knob_status(KnobId::Profile),
            KnobStatus::InFlight(_)
        ));
    }

    #[test]
    fn publisher_outcome_flow_observable_via_version() {
        let state = Arc::new(RwLock::new(AppState::default()));
        let pub_ = RwLockStatePublisher::new(Arc::clone(&state));
        let v0 = pub_.version();
        pub_.update(Box::new(|s| {
            s.apply_outcome(
                KnobId::Profile,
                outcome(ControlStatus::Applied {
                    verification: Verification::Verified,
                }),
            );
        }));
        // Notification contract: version strictly increased (the only thing
        // a UI observer can observe on this publisher).
        assert!(pub_.version() > v0, "publisher must notify on update");
        let v1 = pub_.version();
        pub_.update(Box::new(|s| {
            // A no-op apply (no field touched) still bumps the version — by
            // design. The observer's own fingerprint is the gate that
            // prevents wasted repaints, not the publisher.
            let _ = &s.engine;
        }));
        assert!(pub_.version() > v1);
    }

    #[test]
    fn snapshot_is_consistent_across_concurrent_writes() {
        // Two publisher handles over the same backing state must agree at
        // any instant (RwLock semantics). This is the contract the GPUI
        // publisher provides too: cross-thread reads see a consistent
        // snapshot. We share the version counter explicitly to mirror the
        // production publisher, where one Entity has one dirty bit.
        use std::sync::atomic::AtomicU64;
        let state = Arc::new(RwLock::new(AppState::default()));
        let version = Arc::new(AtomicU64::new(0));
        let a = RwLockStatePublisher::new_with_version(Arc::clone(&state), Arc::clone(&version));
        let b = RwLockStatePublisher::new_with_version(Arc::clone(&state), Arc::clone(&version));
        a.update(Box::new(|s| {
            s.engine = EngineStatus::Running;
        }));
        let snap_a = a.snapshot();
        let snap_b = b.snapshot();
        assert_eq!(snap_a.engine, snap_b.engine);
        assert_eq!(snap_a.engine, EngineStatus::Running);
        // Versions agree across handles (shared counter).
        assert_eq!(a.version(), b.version());
        assert_eq!(
            version.load(std::sync::atomic::Ordering::Acquire),
            a.version()
        );
    }
}
