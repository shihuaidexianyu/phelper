//! HpActor — the single thread that owns the HP WMI transport.
//!
//! Why an actor: COM apartment affinity (the wmi connection is bound to
//! the thread that created it in practice) + firmware AML is not reentrant.
//! Every firmware call — telemetry reads now, control writes in M2, the
//! 60 s keep-alive heartbeat — flows through this one serialization point.
//!
//! The request set is a CLOSED typed enum: there is no raw-payload channel
//! (§50). `HpHandle` implements the domain `HpPlatform` port, so callers
//! cannot tell the transport sits on another thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use phelper_domain::error::HpWmiError;
use phelper_domain::hp::{FanTable, SystemDesignData};
#[cfg(feature = "control")]
use phelper_domain::policy::{CpuPowerLimits, ThermalMode};
use phelper_domain::policy::{FanLevels, GpuPlatformPolicy, MuxMode};
use phelper_domain::ports::HpPlatform;
use tracing::{debug, info, warn};

use super::HpWmiTransport;

/// One typed firmware operation. Response rides back on a oneshot.
enum HpRequest {
    FanCount(mpsc::Sender<Result<u8, HpWmiError>>),
    SystemDesignData(mpsc::Sender<Result<SystemDesignData, HpWmiError>>),
    FanTable(mpsc::Sender<Result<FanTable, HpWmiError>>),
    FanLevels(mpsc::Sender<Result<(FanLevels, Instant), HpWmiError>>),
    GpuPlatformPolicy(mpsc::Sender<Result<GpuPlatformPolicy, HpWmiError>>),
    MuxMode(mpsc::Sender<Result<MuxMode, HpWmiError>>),
    #[cfg(feature = "experimental-mux")]
    SetMuxMode(MuxMode, mpsc::Sender<Result<(), HpWmiError>>),
    MaxFanReadbackDiagnostic(mpsc::Sender<Result<bool, HpWmiError>>),
    #[cfg(feature = "control")]
    SetThermalMode(ThermalMode, mpsc::Sender<Result<(), HpWmiError>>),
    #[cfg(feature = "control")]
    SetFanLevels(FanLevels, mpsc::Sender<Result<(), HpWmiError>>),
    #[cfg(feature = "control")]
    SetMaxFan(bool, mpsc::Sender<Result<(), HpWmiError>>),
    #[cfg(feature = "control")]
    SetGpuPlatformPolicy(GpuPlatformPolicy, mpsc::Sender<Result<(), HpWmiError>>),
    #[cfg(feature = "control")]
    SetPowerLimits(CpuPowerLimits, mpsc::Sender<Result<(), HpWmiError>>),
    Shutdown(mpsc::Sender<()>),
}

/// A request plus its cancellation flag (2026-09 audit). A caller whose
/// round-trip TIMED OUT sets the flag; the actor then skips the request
/// instead of applying a STALE operation the caller already recorded as
/// failed — an uncompensated late write would bypass the M8 shutdown
/// ledger (e.g. restore wrote fan-auto, then the stale manual write
/// lands afterwards, leaving the machine held with no dirty flag).
struct Envelope {
    req: HpRequest,
    cancelled: Arc<AtomicBool>,
}

/// Cloneable handle to the actor. Safe to move across threads.
#[derive(Clone)]
pub(crate) struct HpHandle {
    tx: mpsc::Sender<Envelope>,
}

pub(crate) struct HpActor {
    rx: mpsc::Receiver<Envelope>,
    transport: HpWmiTransport,
}

impl HpActor {
    /// Spawn the actor thread. Fails if the transport can't connect (caller
    /// degrades: HP domain becomes Unavailable, everything else proceeds).
    ///
    /// The transport connects INSIDE the actor thread (2026-09 review): the
    /// wmi crate deliberately forces `!Send` on connections (COM apartment
    /// affinity — its own docs say "each thread must initialize COM and a
    /// separate connection"). The previous version connected on the
    /// spawner's thread and moved the transport across, relying on the
    /// process-wide MTA argument; connecting here removes that hop
    /// entirely, so create-and-use-on-one-thread is now literal.
    pub(crate) fn spawn() -> Result<HpHandle, HpWmiError> {
        let (tx, rx) = mpsc::channel::<Envelope>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), HpWmiError>>();
        std::thread::Builder::new()
            .name("hp-actor".into())
            .spawn(move || {
                match HpWmiTransport::connect() {
                    Ok(transport) => {
                        let _ = ready_tx.send(Ok(()));
                        info!(insize = ?transport.insize_mode(), "HpActor transport up");
                        let actor = HpActor { rx, transport };
                        actor.run();
                    }
                    Err(e) => {
                        // rx drops with it — any straggler sends on tx fail
                        // with NotAvailable, matching a never-started actor.
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| HpWmiError::Transport(format!("spawn hp-actor: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            // Every sender dropped: the thread died before signaling
            // (or the connection itself panicked before the send).
            Err(_) => {
                return Err(HpWmiError::Transport(
                    "hp-actor exited before the transport was up".into(),
                ));
            }
        }
        Ok(HpHandle { tx })
    }

    /// Run one firmware op with panic isolation (2026-09 audit): a WMI-layer
    /// panic must neither kill the actor thread — it is the AR-12 restore
    /// path — nor vanish without a trace (the GUI app has no stderr).
    /// The caller gets a structured error instead of a dropped channel.
    fn dispatch<T>(
        op: impl FnOnce() -> Result<T, HpWmiError>,
        reply: mpsc::Sender<Result<T, HpWmiError>>,
    ) {
        let out = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)) {
            Ok(r) => r,
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string payload>");
                warn!(panic = %msg, "hp-actor: firmware call panicked — actor stays alive");
                Err(HpWmiError::Transport(format!(
                    "firmware call panicked: {msg}"
                )))
            }
        };
        let _ = reply.send(out);
    }

    fn run(self) {
        debug!("hp-actor running");
        let mut fans = FanCache::default();
        while let Ok(env) = self.rx.recv() {
            // Shutdown is never cancellable: engine teardown must be able
            // to stop this thread even after caller timeouts.
            if matches!(env.req, HpRequest::Shutdown(_)) {
                let HpRequest::Shutdown(reply) = env.req else {
                    unreachable!()
                };
                info!("hp-actor shutting down");
                let _ = reply.send(());
                return;
            }
            if env.cancelled.load(Ordering::SeqCst) {
                debug!("hp-actor: dropping request cancelled by caller timeout");
                continue;
            }
            match env.req {
                HpRequest::FanCount(reply) => Self::dispatch(|| self.transport.fan_count(), reply),
                HpRequest::SystemDesignData(reply) => {
                    Self::dispatch(|| self.transport.system_design_data(), reply)
                }
                HpRequest::FanTable(reply) => Self::dispatch(|| self.transport.fan_table(), reply),
                HpRequest::FanLevels(reply) => Self::dispatch(
                    || fans.read(Instant::now(), || self.transport.fan_levels()),
                    reply,
                ),
                HpRequest::GpuPlatformPolicy(reply) => {
                    Self::dispatch(|| self.transport.gpu_platform_policy(), reply)
                }
                HpRequest::MuxMode(reply) => Self::dispatch(|| self.transport.mux_mode(), reply),
                #[cfg(feature = "experimental-mux")]
                HpRequest::SetMuxMode(mode, reply) => {
                    use phelper_domain::ports::HpControl;
                    Self::dispatch(|| self.transport.set_mux_mode(mode), reply);
                }
                HpRequest::MaxFanReadbackDiagnostic(reply) => {
                    Self::dispatch(|| self.transport.max_fan_readback_diagnostic(), reply)
                }
                #[cfg(feature = "control")]
                HpRequest::SetThermalMode(mode, reply) => {
                    use phelper_domain::ports::HpControl;
                    Self::dispatch(|| self.transport.set_thermal_mode(mode), reply);
                }
                #[cfg(feature = "control")]
                HpRequest::SetFanLevels(levels, reply) => {
                    use phelper_domain::ports::HpControl;
                    Self::dispatch(|| self.transport.set_fan_levels(levels), reply);
                }
                #[cfg(feature = "control")]
                HpRequest::SetMaxFan(on, reply) => {
                    use phelper_domain::ports::HpControl;
                    Self::dispatch(|| self.transport.set_max_fan(on), reply);
                }
                #[cfg(feature = "control")]
                HpRequest::SetGpuPlatformPolicy(p, reply) => {
                    use phelper_domain::ports::HpControl;
                    Self::dispatch(|| self.transport.set_gpu_platform_policy(p), reply);
                }
                #[cfg(feature = "control")]
                HpRequest::SetPowerLimits(l, reply) => {
                    use phelper_domain::ports::HpControl;
                    Self::dispatch(|| self.transport.set_power_limits(l), reply);
                }
                HpRequest::Shutdown(_) => unreachable!("handled above"),
            }
        }
        warn!("hp-actor channel closed without shutdown");
    }
}

/// Round-trip timeout. Firmware calls are fast; a wedged AML call should
/// not stall the telemetry scheduler forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

impl HpHandle {
    fn call<T>(
        &self,
        build: impl FnOnce(mpsc::Sender<Result<T, HpWmiError>>) -> HpRequest,
    ) -> Result<T, HpWmiError> {
        let (tx, rx) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        self.tx
            .send(Envelope {
                req: build(tx),
                cancelled: Arc::clone(&cancelled),
            })
            .map_err(|_| HpWmiError::NotAvailable("hp-actor gone"))?;
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The actor may still hold the request in its FIFO; flag it
                // so a stale op never executes after the caller recorded
                // the failure. (A request already mid-execution cannot be
                // recalled — that residual race is inherent to the wire.)
                cancelled.store(true, Ordering::SeqCst);
                Err(HpWmiError::Timeout)
            }
            // The reply sender only drops without a send if the actor
            // thread is gone (it never silently drops requests).
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(HpWmiError::NotAvailable("hp-actor gone"))
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        let env = Envelope {
            req: HpRequest::Shutdown(tx),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        if self.tx.send(env).is_ok() {
            let _ = rx.recv_timeout(CALL_TIMEOUT);
        }
    }
}

impl HpPlatform for HpHandle {
    fn fan_count(&self) -> Result<u8, HpWmiError> {
        self.call(HpRequest::FanCount)
    }
    fn system_design_data(&self) -> Result<SystemDesignData, HpWmiError> {
        self.call(HpRequest::SystemDesignData)
    }
    fn fan_table(&self) -> Result<FanTable, HpWmiError> {
        self.call(HpRequest::FanTable)
    }
    fn fan_levels(&self) -> Result<FanLevels, HpWmiError> {
        self.fan_levels_sample().map(|(levels, _)| levels)
    }
    fn fan_levels_sample(&self) -> Result<(FanLevels, Instant), HpWmiError> {
        self.call(HpRequest::FanLevels)
    }
    fn gpu_platform_policy(&self) -> Result<GpuPlatformPolicy, HpWmiError> {
        self.call(HpRequest::GpuPlatformPolicy)
    }
    fn mux_mode(&self) -> Result<MuxMode, HpWmiError> {
        self.call(HpRequest::MuxMode)
    }
    fn max_fan_readback_diagnostic(&self) -> Result<bool, HpWmiError> {
        self.call(HpRequest::MaxFanReadbackDiagnostic)
    }
}

#[derive(Default)]
struct FanCache {
    attempted: Option<Instant>,
    sample: Option<(FanLevels, Instant)>,
}
impl FanCache {
    fn read(
        &mut self,
        now: Instant,
        read: impl FnOnce() -> Result<FanLevels, HpWmiError>,
    ) -> Result<(FanLevels, Instant), HpWmiError> {
        if self
            .attempted
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(1))
        {
            return self
                .sample
                .ok_or(HpWmiError::NotAvailable("fan read throttled after failure"));
        }
        self.attempted = Some(now);
        self.sample = None;
        let value = read()?;
        let sample = (value, now);
        self.sample = Some(sample);
        Ok(sample)
    }
}

#[cfg(feature = "control")]
impl phelper_domain::ports::HpControl for HpHandle {
    #[cfg(feature = "experimental-mux")]
    fn set_mux_mode(&self, mode: MuxMode) -> Result<(), HpWmiError> {
        self.call(|tx| HpRequest::SetMuxMode(mode, tx))
    }
    fn set_thermal_mode(&self, mode: ThermalMode) -> Result<(), HpWmiError> {
        self.call(|tx| HpRequest::SetThermalMode(mode, tx))
    }
    fn set_fan_levels(&self, levels: FanLevels) -> Result<(), HpWmiError> {
        self.call(|tx| HpRequest::SetFanLevels(levels, tx))
    }
    fn set_max_fan(&self, on: bool) -> Result<(), HpWmiError> {
        self.call(|tx| HpRequest::SetMaxFan(on, tx))
    }
    fn set_gpu_platform_policy(&self, p: GpuPlatformPolicy) -> Result<(), HpWmiError> {
        self.call(|tx| HpRequest::SetGpuPlatformPolicy(p, tx))
    }
    fn set_power_limits(&self, l: CpuPowerLimits) -> Result<(), HpWmiError> {
        self.call(|tx| HpRequest::SetPowerLimits(l, tx))
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    #[test]
    fn shared_cache_preserves_time_and_throttles_errors() {
        let mut cache = FanCache::default();
        let now = Instant::now();
        let first = cache.read(now, || Ok(FanLevels::new(20, 20))).unwrap();
        let second = cache
            .read(now + Duration::from_millis(500), || {
                panic!("extra firmware read")
            })
            .unwrap();
        assert_eq!(first, second);
        assert!(
            cache
                .read(now + Duration::from_secs(1), || Err(HpWmiError::Timeout))
                .is_err()
        );
        assert!(
            cache
                .read(now + Duration::from_millis(1100), || panic!("retry burst"))
                .is_err()
        );
    }
}
