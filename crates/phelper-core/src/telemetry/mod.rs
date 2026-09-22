//! Telemetry engine (M1): per-cadence collectors, bounded store, single
//! provider workers and a coalescing publication thread.
//!
//! Providers collect independently; HP WMI stalls cannot delay CPU thermals.
//! The PawnIO worker alone is pinned to logical processor 0 for APERF/MPERF.
//! Shared state is locked only while publishing completed samples.
//!
//! Failure model (D3): collectors don't throw — failures downgrade
//! ProviderStatus and skip the metric; staleness is expressed by sample
//! timestamps, never by fabricated values.

pub mod collectors;
pub mod registry;
mod store;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use phelper_domain::error::EngineError;
use phelper_domain::telemetry::{
    MetricId, MetricSample, ProviderStatus, TelemetrySnapshot, WindowStats,
};
use tracing::{debug, warn};

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

/// Lock poisoning must never take this thread — or, via SnapshotFeed, the
/// CONTROL coordinator — down with it (2026-09 audit). A panic while a
/// guard was held can only have LOST updates in this plain-data store
/// (Rust keeps it memory-safe), so recovering the inner store is sound;
/// at worst a sample is stale, and the safety layer's freshness gates
/// already treat stale as absent (fail closed).
fn read_store(store: &RwLock<TelemetryStore>) -> std::sync::RwLockReadGuard<'_, TelemetryStore> {
    store.read().unwrap_or_else(|e| {
        warn!("telemetry store read-lock poisoned — recovering inner store");
        e.into_inner()
    })
}

fn write_store(store: &RwLock<TelemetryStore>) -> std::sync::RwLockWriteGuard<'_, TelemetryStore> {
    store.write().unwrap_or_else(|e| {
        warn!("telemetry store write-lock poisoned — recovering inner store");
        e.into_inner()
    })
}

enum Command {
    Collected,
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
        read_store(&self.store).snapshot()
    }

    pub fn history(&self, id: MetricId, window: Duration) -> Vec<MetricSample> {
        read_store(&self.store).history(id, window)
    }

    pub fn stats(&self, id: MetricId, window: Duration) -> Option<WindowStats> {
        read_store(&self.store).stats(id, window)
    }

    /// Per-collector worst scheduling lateness since start (M1 acceptance:
    /// 250 ms domain jitter must stay < 50 ms).
    pub fn scheduler_jitter(&self) -> BTreeMap<&'static str, Duration> {
        read_store(&self.store).scheduler_jitter().clone()
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

    /// Stop the coordinator thread. Waits (bounded) for the thread to exit.
    pub(crate) fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        if self.cmd.send(Command::Shutdown(tx)).is_ok() {
            // Engine teardown must not stop the HP actor while a collector
            // still owns an in-flight read against it. The wait is bounded:
            // a collector wedged in a firmware call must not hang process
            // exit forever (2026-09 audit) — the OS reaps the thread.
            if rx.recv_timeout(Duration::from_secs(10)).is_err() {
                warn!("telemetry coordinator did not ack shutdown within 10 s (wedged collector?)");
            }
        }
    }
}

pub(crate) struct TelemetryCoordinator {
    collectors: Vec<CollectorBox>,
    store: Arc<RwLock<TelemetryStore>>,
    rx: Receiver<Command>,
    heartbeat: Arc<AtomicU64>,
    notifications: Sender<Command>,
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
            let mut guard = write_store(&store);
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
            notifications: tx.clone(),
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

    fn run(self) {
        // Each provider owns its cadence and thread. In particular HP WMI
        // cannot stall CPU thermals, and only the MSR reader is core-pinned.
        let pending = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        for mut collector in self.collectors {
            let (tx, rx) = mpsc::sync_channel::<bool>(1);
            let store = Arc::clone(&self.store);
            let notify = self.notifications.clone();
            let pending = Arc::clone(&pending);
            let name = collector.name();
            // Typed pinning marker (a name-string match broke silently on
            // rename — 2026-09 review). Captured before the move below.
            let pin_core_zero = collector.pins_core_zero();
            let worker = std::thread::Builder::new()
                .name(format!("telemetry-{name}"))
                .spawn(move || {
                    #[cfg(windows)]
                    if pin_core_zero {
                        pin_to_core_zero();
                    }
                    let mut due = Instant::now();
                    loop {
                        match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
                            Ok(false) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                            Ok(true) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        let started = Instant::now();
                        // cadence() reads the registry with `.expect` — a
                        // forgotten entry must surface as an Unavailable
                        // provider row, not a silently dead worker thread
                        // (the GUI has no stderr to show the panic). Keep it
                        // inside the same unwind guard as collect().
                        let (samples, cadence) =
                            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (collector.collect(), collector.cadence())
                            })) {
                                Ok(pair) => pair,
                                Err(_) => {
                                    write_store(&store).set_provider(
                                        name,
                                        ProviderStatus::Unavailable("collector panicked".into()),
                                    );
                                    if !pending.swap(true, Ordering::AcqRel) {
                                        let _ = notify.send(Command::Collected);
                                    }
                                    return;
                                }
                            };
                        {
                            let mut guard = write_store(&store);
                            guard.note_jitter(name, started.saturating_duration_since(due));
                            for sample in samples {
                                guard.push(sample);
                            }
                            guard.set_provider(name, collector.status());
                        }
                        // Never make up missed samples or spin after a slow
                        // call. A provider resumes at its own normal cadence.
                        due = Instant::now() + cadence;
                        if !pending.swap(true, Ordering::AcqRel) {
                            let _ = notify.send(Command::Collected);
                        }
                    }
                });
            match worker {
                Ok(join) => workers.push((tx, join)),
                Err(error) => write_store(&self.store).set_provider(
                    name,
                    ProviderStatus::Unavailable(format!("collector thread: {error}")),
                ),
            }
        }
        let mut subscribers = Vec::new();
        loop {
            self.heartbeat.fetch_add(1, Ordering::Relaxed);
            match self.rx.recv() {
                Ok(Command::Collected) => {
                    pending.store(false, Ordering::Release);
                    Self::publish(&self.store, &mut subscribers);
                }
                Ok(Command::RefreshNow) => {
                    // Capacity one coalesces refreshes. Firmware collectors
                    // retain their own minimum-interval checks.
                    for (worker, _) in &workers {
                        let _ = worker.try_send(true);
                    }
                }
                Ok(Command::Subscribe(tx)) => {
                    let snap = Arc::new(read_store(&self.store).snapshot());
                    if !snap.samples.is_empty() {
                        let _ = tx.try_send(snap);
                    }
                    subscribers.push(tx);
                }
                Ok(Command::Shutdown(ack)) => {
                    let joins: Vec<_> = workers
                        .into_iter()
                        .map(|(tx, join)| {
                            let _ = tx.try_send(false);
                            drop(tx);
                            join
                        })
                        .collect();
                    let deadline = Instant::now() + Duration::from_secs(9);
                    while joins.iter().any(|join| !join.is_finished()) && Instant::now() < deadline
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    for join in joins {
                        if join.is_finished() {
                            let _ = join.join();
                        } else {
                            warn!("collector still stopping; backend may be stalled");
                        }
                    }
                    let _ = ack.send(());
                    return;
                }
                Err(_) => return,
            }
        }
    }

    fn publish(
        store: &Arc<RwLock<TelemetryStore>>,
        subscribers: &mut Vec<SyncSender<Arc<TelemetrySnapshot>>>,
    ) {
        if subscribers.is_empty() {
            return;
        }
        let snap = Arc::new(read_store(store).snapshot());
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
    #[test]
    fn slow_provider_does_not_delay_fast_samples() {
        struct Source {
            slow: bool,
            count: Arc<AtomicU64>,
        }
        impl Collector for Source {
            fn name(&self) -> &'static str {
                if self.slow { "test-slow" } else { "test-fast" }
            }
            fn cadence(&self) -> Duration {
                Duration::from_millis(10)
            }
            fn collect(&mut self) -> Vec<MetricSample> {
                if self.slow {
                    std::thread::sleep(Duration::from_millis(250));
                }
                self.count.fetch_add(1, Ordering::Relaxed);
                vec![]
            }
            fn status(&self) -> ProviderStatus {
                ProviderStatus::Ok
            }
        }
        let fast = Arc::new(AtomicU64::new(0));
        let slow = Arc::new(AtomicU64::new(0));
        let handle = TelemetryCoordinator::start(
            vec![
                Box::new(Source {
                    slow: true,
                    count: slow.clone(),
                }),
                Box::new(Source {
                    slow: false,
                    count: fast.clone(),
                }),
            ],
            vec![],
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while fast.load(Ordering::Relaxed) < 5 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let fast_count = fast.load(Ordering::Relaxed);
        let slow_count = slow.load(Ordering::Relaxed);
        handle.shutdown();
        assert!(fast_count >= 5);
        assert_eq!(
            slow_count, 0,
            "fast collection should advance during the first slow call"
        );
    }
}
