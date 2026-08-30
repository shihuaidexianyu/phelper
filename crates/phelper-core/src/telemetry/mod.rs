//! Telemetry engine (M1): per-cadence collectors, bounded store, single
//! coordinator thread.
//!
//! Threading (D2): one `telemetry-coord` thread runs every collector
//! sequentially, scheduled by per-collector cadence from the registry. The
//! HP fan collector's WMI call rides the HpActor's 5 s timeout — a wedged
//! firmware call can stall the loop for that bound; accepted for M1
//! (firmware round-trips measured on 8BAB are milliseconds) and documented
//! here so M2 doesn't "discover" it later.
//!
//! Failure model (D3): collectors don't throw — failures downgrade
//! ProviderStatus and skip the metric; staleness is expressed by sample
//! timestamps, never by fabricated values.

pub mod collectors;
pub mod registry;
mod store;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use phelper_domain::error::EngineError;
use phelper_domain::telemetry::{
    MetricId, MetricSample, ProviderStatus, TelemetrySnapshot, WindowStats,
};
use tracing::{debug, info, warn};

use collectors::Collector;
use store::TelemetryStore;

/// Pin the coordinator to logical processor 0 (always a P-core on the
/// 13900HX hybrid). APERF/MPERF are per-core MSRs: consecutive reads must
/// land on the SAME core or the ratio is garbage (the collector's clamp
/// discards those, starving the metric). Pinning makes same-core reads
/// deterministic; the telemetry load per tick is microseconds, so holding
/// one core is free. Side benefit: fewer migrations → tighter scheduling
/// jitter.
#[cfg(windows)]
fn pin_to_core_zero() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadAffinityMask};
    unsafe {
        let prev = SetThreadAffinityMask(GetCurrentThread(), 1usize);
        if prev == 0 {
            warn!("coordinator core-0 affinity pin failed (eff-clock may starve)");
        } else {
            debug!("coordinator pinned to logical processor 0");
        }
    }
}

/// Provider status is reported under this key in the snapshot.
pub(crate) type CollectorBox = Box<dyn Collector>;

enum Command {
    /// Out-of-cadence refresh of every collector (per-collector firmware
    /// guards still apply — the HP 1 Hz fan rule is not bypassable).
    RefreshNow,
    Subscribe(SyncSender<Arc<TelemetrySnapshot>>),
    Shutdown(Sender<()>),
}

/// Handle to the running telemetry engine. Cheap to clone (Arc + channel).
#[derive(Clone)]
pub struct TelemetryHandle {
    store: Arc<RwLock<TelemetryStore>>,
    cmd: Sender<Command>,
    /// Coordinator liveness: bumped by the thread each loop; a stalled
    /// coordinator (e.g. wedged firmware call) shows up as a frozen tick.
    heartbeat: Arc<AtomicU64>,
}

impl TelemetryHandle {
    pub fn snapshot(&self) -> TelemetrySnapshot {
        self.store.read().expect("store poisoned").snapshot()
    }

    pub fn history(&self, id: MetricId, window: Duration) -> Vec<MetricSample> {
        self.store
            .read()
            .expect("store poisoned")
            .history(id, window)
    }

    pub fn stats(&self, id: MetricId, window: Duration) -> Option<WindowStats> {
        self.store.read().expect("store poisoned").stats(id, window)
    }

    /// Per-collector worst scheduling lateness since start (M1 acceptance:
    /// 250 ms domain jitter must stay < 50 ms).
    pub fn scheduler_jitter(&self) -> BTreeMap<&'static str, Duration> {
        self.store
            .read()
            .expect("store poisoned")
            .scheduler_jitter()
            .clone()
    }

    /// Monotonic loop counter — freezes if the coordinator stalls.
    pub fn heartbeat(&self) -> u64 {
        self.heartbeat.load(Ordering::Relaxed)
    }

    /// Subscribe to snapshot broadcasts after each collection round.
    /// Slow/dead receivers are pruned automatically.
    pub fn subscribe(&self) -> Receiver<Arc<TelemetrySnapshot>> {
        // Snapshots are replaceable state, not an event log. Capacity one
        // bounds memory when a UI subscriber stalls.
        let (tx, rx) = mpsc::sync_channel(1);
        let _ = self.cmd.send(Command::Subscribe(tx));
        rx
    }

    /// Force one out-of-cadence collection round.
    pub fn request_fresh(&self) {
        let _ = self.cmd.send(Command::RefreshNow);
    }

    /// Stop the coordinator thread. Waits for the thread to exit.
    pub(crate) fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        if self.cmd.send(Command::Shutdown(tx)).is_ok() {
            // Engine teardown must not stop the HP actor while a collector
            // still owns an in-flight read against it.
            let _ = rx.recv();
        }
    }
}

pub(crate) struct TelemetryCoordinator {
    collectors: Vec<CollectorBox>,
    store: Arc<RwLock<TelemetryStore>>,
    rx: Receiver<Command>,
    heartbeat: Arc<AtomicU64>,
}

impl TelemetryCoordinator {
    /// Spawn the coordinator thread. Providers that failed to construct are
    /// pre-marked in the store by the caller (engine.rs) — a missing
    /// provider is a status row, never a panic.
    pub(crate) fn start(
        collectors: Vec<CollectorBox>,
        unavailable: Vec<(&'static str, String)>,
    ) -> Result<TelemetryHandle, EngineError> {
        let store = Arc::new(RwLock::new(TelemetryStore::default()));
        {
            let mut guard = store.write().expect("store poisoned");
            for (name, why) in unavailable {
                guard.set_provider(name, ProviderStatus::Unavailable(why));
            }
        }
        let (tx, rx) = mpsc::channel();
        let heartbeat = Arc::new(AtomicU64::new(0));
        let coord = Self {
            collectors,
            store: Arc::clone(&store),
            rx,
            heartbeat: Arc::clone(&heartbeat),
        };
        std::thread::Builder::new()
            .name("telemetry-coord".into())
            .spawn(move || coord.run())
            .map_err(|e| EngineError::Config(format!("spawn telemetry-coord: {e}")))?;
        Ok(TelemetryHandle {
            store,
            cmd: tx,
            heartbeat,
        })
    }

    fn run(mut self) {
        pin_to_core_zero();
        info!(
            collectors = self.collectors.len(),
            "telemetry coordinator running"
        );
        let mut next_due: Vec<Instant> = vec![Instant::now(); self.collectors.len()];
        let mut subscribers: Vec<SyncSender<Arc<TelemetrySnapshot>>> = Vec::new();

        loop {
            self.heartbeat.fetch_add(1, Ordering::Relaxed);
            let now = Instant::now();
            let wait = next_due
                .iter()
                .copied()
                .min()
                .map(|d| d.saturating_duration_since(now))
                .unwrap_or(Duration::from_secs(3600));

            match self.rx.recv_timeout(wait) {
                Ok(Command::Shutdown(ack)) => {
                    let jitter = self
                        .store
                        .read()
                        .expect("store poisoned")
                        .scheduler_jitter()
                        .clone();
                    info!(
                        ?jitter,
                        "telemetry coordinator shutting down (max jitter per collector)"
                    );
                    let _ = ack.send(());
                    return;
                }
                Ok(Command::RefreshNow) => {
                    debug!("refresh-now requested");
                    self.collect_where(&mut next_due, |_| true);
                    Self::publish(&self.store, &mut subscribers);
                }
                Ok(Command::Subscribe(tx)) => {
                    // The first collection round can finish before the app
                    // pump registers its subscriber. Deliver the current
                    // store immediately so it does not wait for the next
                    // cadence just to receive data that already exists.
                    let snap = Arc::new(self.store.read().expect("store poisoned").snapshot());
                    if !snap.samples.is_empty() {
                        let _ = tx.send(snap);
                    }
                    subscribers.push(tx);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let now = Instant::now();
                    self.collect_where(&mut next_due, |due| now >= *due);
                    Self::publish(&self.store, &mut subscribers);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    warn!("telemetry command channel closed without shutdown");
                    return;
                }
            }
        }
    }

    /// Run every collector whose `due` matches `fired`, push samples, update
    /// provider status, record scheduling jitter, and reschedule.
    fn collect_where(&mut self, next_due: &mut [Instant], fired: impl Fn(&Instant) -> bool) {
        for (i, c) in self.collectors.iter_mut().enumerate() {
            if !fired(&next_due[i]) {
                continue;
            }
            let start = Instant::now();
            let lateness = start.saturating_duration_since(next_due[i]);
            {
                let mut guard = self.store.write().expect("store poisoned");
                guard.note_jitter(c.name(), lateness);
            }
            let samples = c.collect();
            let status = c.status();
            {
                let mut guard = self.store.write().expect("store poisoned");
                for s in samples {
                    guard.push(s);
                }
                guard.set_provider(c.name(), status);
            }
            let elapsed = start.elapsed();
            if elapsed > c.cadence() / 2 {
                warn!(
                    collector = c.name(),
                    ?elapsed,
                    "collector consumed over half its cadence"
                );
            }
            next_due[i] = start + c.cadence();
        }
    }

    fn publish(
        store: &Arc<RwLock<TelemetryStore>>,
        subscribers: &mut Vec<SyncSender<Arc<TelemetrySnapshot>>>,
    ) {
        if subscribers.is_empty() {
            return;
        }
        let snap = Arc::new(store.read().expect("store poisoned").snapshot());
        subscribers.retain(|tx| match tx.try_send(Arc::clone(&snap)) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::telemetry::{MetricSource, MetricValue, ids};

    #[test]
    fn slow_snapshot_subscriber_is_bounded_and_pruned_on_disconnect() {
        let store = Arc::new(RwLock::new(TelemetryStore::default()));
        store.write().expect("store").push(MetricSample::fresh(
            ids::CPU_PKG_TEMP_C,
            MetricValue::F64(70.0),
            MetricSource::PawnIoMsr,
        ));
        let (tx, rx) = mpsc::sync_channel(1);
        let mut subscribers = vec![tx];

        TelemetryCoordinator::publish(&store, &mut subscribers);
        TelemetryCoordinator::publish(&store, &mut subscribers);
        assert_eq!(subscribers.len(), 1, "a full subscriber remains registered");
        assert_eq!(
            rx.try_iter().count(),
            1,
            "capacity one prevents backlog growth"
        );

        drop(rx);
        TelemetryCoordinator::publish(&store, &mut subscribers);
        assert!(subscribers.is_empty());
    }
}
