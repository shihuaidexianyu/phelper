//! Authoritative desktop state store and coalesced repaint signal.

use std::sync::{Arc, Mutex, RwLock};

use futures::channel::mpsc;
use phelper_core::app::runtime::StatePublisher;
use phelper_core::app::state::AppState;

pub struct GpuiStatePublisher {
    state: RwLock<AppState>,
    wake_tx: Mutex<mpsc::Sender<()>>,
}

impl GpuiStatePublisher {
    pub fn new() -> (Arc<Self>, mpsc::Receiver<()>) {
        let (wake_tx, wake_rx) = mpsc::channel(1);
        (
            Arc::new(Self {
                state: RwLock::new(AppState::default()),
                wake_tx: Mutex::new(wake_tx),
            }),
            wake_rx,
        )
    }

    /// Poisoned-read recovery: keep the LAST snapshot instead of flashing
    /// back to a default AppState (engine = Starting, empty telemetry) —
    /// a UI that briefly shows "starting up" mid-session misleads triage.
    /// Same soundness argument as the telemetry store: a panic under the
    /// guard can only have LOST updates in this plain-data state.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, AppState> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }
}

impl StatePublisher for GpuiStatePublisher {
    fn update(&self, apply: Box<dyn FnOnce(&mut AppState) + Send>) {
        // Poisoned-write recovery, same rule: recover the inner state and
        // apply — at worst one stale field, never a silently dropped wake
        // (the UI would freeze on pre-update data).
        let mut guard = self.state.write().unwrap_or_else(|e| e.into_inner());
        apply(&mut guard);
        drop(guard);
        if let Ok(mut tx) = self.wake_tx.lock() {
            let _ = tx.try_send(());
        }
    }

    fn snapshot(&self) -> AppState {
        self.read().clone()
    }
}

#[cfg(test)]
mod tests {
    use phelper_core::app::EngineStatus;

    use super::*;

    #[test]
    fn update_is_visible_before_gpui_drains_the_wake() {
        let (publisher, mut wake_rx) = GpuiStatePublisher::new();

        publisher.update(Box::new(|state| state.engine = EngineStatus::Running));

        assert_eq!(publisher.snapshot().engine, EngineStatus::Running);
        assert!(matches!(wake_rx.try_recv(), Ok(())));
    }

    #[test]
    fn full_wake_channel_does_not_drop_state() {
        let (publisher, _wake_rx) = GpuiStatePublisher::new();

        for index in 0..100 {
            publisher.update(Box::new(move |state| {
                state.desired.profile = Some(format!("profile-{index}"));
            }));
        }

        assert_eq!(
            publisher.snapshot().desired.profile.as_deref(),
            Some("profile-99")
        );
    }
}
