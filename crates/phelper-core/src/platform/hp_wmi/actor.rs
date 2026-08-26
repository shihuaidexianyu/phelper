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

use std::sync::mpsc;
use std::time::Duration;

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
    FanLevels(mpsc::Sender<Result<FanLevels, HpWmiError>>),
    GpuPlatformPolicy(mpsc::Sender<Result<GpuPlatformPolicy, HpWmiError>>),
    MuxMode(mpsc::Sender<Result<MuxMode, HpWmiError>>),
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

/// Cloneable handle to the actor. Safe to move across threads.
#[derive(Clone)]
pub(crate) struct HpHandle {
    tx: mpsc::Sender<HpRequest>,
}

pub(crate) struct HpActor {
    rx: mpsc::Receiver<HpRequest>,
    transport: HpWmiTransport,
}

impl HpActor {
    /// Spawn the actor thread. Fails if the transport can't connect (caller
    /// degrades: HP domain becomes Unavailable, everything else proceeds).
    pub(crate) fn spawn() -> Result<HpHandle, HpWmiError> {
        let transport = HpWmiTransport::connect()?;
        info!(insize = ?transport.insize_mode(), "HpActor transport up");
        let (tx, rx) = mpsc::channel::<HpRequest>();
        std::thread::Builder::new()
            .name("hp-actor".into())
            .spawn(move || {
                let actor = HpActor { rx, transport };
                actor.run();
            })
            .map_err(|e| HpWmiError::Transport(format!("spawn hp-actor: {e}")))?;
        Ok(HpHandle { tx })
    }

    fn run(self) {
        debug!("hp-actor running");
        while let Ok(req) = self.rx.recv() {
            match req {
                HpRequest::FanCount(reply) => {
                    let _ = reply.send(self.transport.fan_count());
                }
                HpRequest::SystemDesignData(reply) => {
                    let _ = reply.send(self.transport.system_design_data());
                }
                HpRequest::FanTable(reply) => {
                    let _ = reply.send(self.transport.fan_table());
                }
                HpRequest::FanLevels(reply) => {
                    let _ = reply.send(self.transport.fan_levels());
                }
                HpRequest::GpuPlatformPolicy(reply) => {
                    let _ = reply.send(self.transport.gpu_platform_policy());
                }
                HpRequest::MuxMode(reply) => {
                    let _ = reply.send(self.transport.mux_mode());
                }
                HpRequest::MaxFanReadbackDiagnostic(reply) => {
                    let _ = reply.send(self.transport.max_fan_readback_diagnostic());
                }
                #[cfg(feature = "control")]
                HpRequest::SetThermalMode(mode, reply) => {
                    use phelper_domain::ports::HpControl;
                    let _ = reply.send(self.transport.set_thermal_mode(mode));
                }
                #[cfg(feature = "control")]
                HpRequest::SetFanLevels(levels, reply) => {
                    use phelper_domain::ports::HpControl;
                    let _ = reply.send(self.transport.set_fan_levels(levels));
                }
                #[cfg(feature = "control")]
                HpRequest::SetMaxFan(on, reply) => {
                    use phelper_domain::ports::HpControl;
                    let _ = reply.send(self.transport.set_max_fan(on));
                }
                #[cfg(feature = "control")]
                HpRequest::SetGpuPlatformPolicy(p, reply) => {
                    use phelper_domain::ports::HpControl;
                    let _ = reply.send(self.transport.set_gpu_platform_policy(p));
                }
                #[cfg(feature = "control")]
                HpRequest::SetPowerLimits(l, reply) => {
                    use phelper_domain::ports::HpControl;
                    let _ = reply.send(self.transport.set_power_limits(l));
                }
                HpRequest::Shutdown(reply) => {
                    info!("hp-actor shutting down");
                    let _ = reply.send(());
                    return;
                }
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
        self.tx
            .send(build(tx))
            .map_err(|_| HpWmiError::NotAvailable("hp-actor gone"))?;
        rx.recv_timeout(CALL_TIMEOUT)
            .map_err(|_| HpWmiError::Timeout)?
    }

    pub(crate) fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(HpRequest::Shutdown(tx)).is_ok() {
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

#[cfg(feature = "control")]
impl phelper_domain::ports::HpControl for HpHandle {
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
