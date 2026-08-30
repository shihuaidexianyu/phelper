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
}

impl StatePublisher for GpuiStatePublisher {
    fn update(&self, apply: Box<dyn FnOnce(&mut AppState) + Send>) {
        let updated = self
            .state
            .write()
            .map(|mut state| apply(&mut state))
            .is_ok();
        if updated && let Ok(mut tx) = self.wake_tx.lock() {
            let _ = tx.try_send(());
        }
    }

    fn snapshot(&self) -> AppState {
        self.state
            .read()
            .map(|state| state.clone())
            .unwrap_or_default()
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
