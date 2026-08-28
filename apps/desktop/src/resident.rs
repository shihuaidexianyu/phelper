//! Desktop-side orchestration for the resident integrations.
//!
//! The worker is intentionally separate from GPUI.  It owns the slow Windows
//! reconciliation calls and turns a physical OMEN event into either a UI-only
//! overlay request or the existing profile command path.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use phelper_core::app::runtime::AppHandle;
use phelper_core::app::state::KnobId;
use phelper_core::resident::{
    AutostartState, OmenKeyAction, OmenKeyBridge, OmenKeyCapability, OmenKeyIntent, ResidentEvent,
    ResidentSettings, ResidentSnapshot, probe_omen_key, reconcile_autostart,
    remove_omen_key_subscription, resolve_omen_key, send_shortcut,
};
use phelper_domain::command::ControlCommand;

const EVENT_DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidentUiEvent {
    ToggleOverlay,
}

enum ResidentCommand {
    Settings(ResidentSettings),
    Stop,
}

#[derive(Clone)]
pub struct ResidentRuntimeHandle {
    tx: Sender<ResidentCommand>,
}

impl ResidentRuntimeHandle {
    pub fn update(&self, settings: ResidentSettings) {
        let _ = self.tx.send(ResidentCommand::Settings(settings));
    }
}

pub struct ResidentRuntime {
    handle: ResidentRuntimeHandle,
    join: Option<JoinHandle<()>>,
}

impl ResidentRuntime {
    pub fn start(
        app: AppHandle,
        settings: ResidentSettings,
        executable: PathBuf,
    ) -> Result<(Self, Receiver<ResidentUiEvent>), String> {
        let (command_tx, command_rx) = mpsc::channel();
        let (ui_tx, ui_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("resident-runtime".into())
            .spawn(move || run(app, settings, executable, command_rx, ui_tx))
            .map_err(|error| format!("启动常驻协调线程失败：{error}"))?;
        Ok((
            Self {
                handle: ResidentRuntimeHandle { tx: command_tx },
                join: Some(join),
            },
            ui_rx,
        ))
    }

    pub fn handle(&self) -> ResidentRuntimeHandle {
        self.handle.clone()
    }

    pub fn stop(&mut self) {
        let _ = self.handle.tx.send(ResidentCommand::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ResidentRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    app: AppHandle,
    mut settings: ResidentSettings,
    executable: PathBuf,
    command_rx: Receiver<ResidentCommand>,
    ui_tx: Sender<ResidentUiEvent>,
) {
    let mut snapshot = ResidentSnapshot {
        omen_key: OmenKeyCapability::Probing,
        ..Default::default()
    };
    publish(&app, &mut snapshot);

    let mut probe = probe_omen_key();
    snapshot.omen_key = probe.capability;
    snapshot.omen_key_detail = Some(probe.detail.clone());
    publish(&app, &mut snapshot);

    reconcile_autostart_state(&app, &mut snapshot, settings.autostart, &executable);

    let (mut bridge, mut event_rx) =
        configure_bridge(&app, &mut snapshot, &settings, &executable, &probe);
    publish(&app, &mut snapshot);

    let mut last_event = None;
    let mut next_bridge_retry = event_rx
        .is_none()
        .then_some(Instant::now() + Duration::from_secs(5));
    loop {
        match command_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(ResidentCommand::Stop) => break,
            Ok(ResidentCommand::Settings(next)) => {
                if next == settings {
                    continue;
                }
                let omen_changed = next.omen_key != settings.omen_key;
                let autostart_changed = next.autostart != settings.autostart;
                settings = next;
                if omen_changed {
                    probe = probe_omen_key();
                    snapshot.omen_key = probe.capability;
                    snapshot.omen_key_detail = Some(probe.detail.clone());
                    publish(&app, &mut snapshot);
                }
                if autostart_changed {
                    reconcile_autostart_state(&app, &mut snapshot, settings.autostart, &executable);
                }
                if omen_changed {
                    if let Some(mut current) = bridge.take() {
                        current.stop();
                    }
                    (bridge, event_rx) =
                        configure_bridge(&app, &mut snapshot, &settings, &executable, &probe);
                    next_bridge_retry = event_rx
                        .is_none()
                        .then_some(Instant::now() + Duration::from_secs(5));
                    publish(&app, &mut snapshot);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // A pipe/thread failure is recoverable.  Re-probe and rebuild the
        // bridge in this worker instead of permanently disabling the OMEN
        // key until the user edits the setting again.
        if event_rx.is_none()
            && settings.omen_key.action != OmenKeyAction::Default
            && next_bridge_retry.is_some_and(|at| Instant::now() >= at)
        {
            probe = probe_omen_key();
            (bridge, event_rx) =
                configure_bridge(&app, &mut snapshot, &settings, &executable, &probe);
            next_bridge_retry = event_rx
                .is_none()
                .then_some(Instant::now() + Duration::from_secs(5));
            publish(&app, &mut snapshot);
        }

        let Some(events) = event_rx.as_ref() else {
            continue;
        };
        let mut bridge_disconnected = false;
        loop {
            match events.try_recv() {
                Ok(ResidentEvent::OmenKeyPressed) => {
                    let now = Instant::now();
                    if last_event.is_some_and(|last| now.duration_since(last) < EVENT_DEBOUNCE) {
                        continue;
                    }
                    last_event = Some(now);
                    handle_event(&app, &ui_tx, &settings, &mut snapshot);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    snapshot.omen_key = OmenKeyCapability::Error;
                    snapshot.omen_key_detail = Some("OMEN 键桥接已断开".into());
                    publish(&app, &mut snapshot);
                    bridge_disconnected = true;
                    break;
                }
            }
        }
        if bridge_disconnected {
            event_rx = None;
            if let Some(mut current) = bridge.take() {
                current.stop();
            }
            next_bridge_retry = Some(Instant::now() + Duration::from_secs(2));
        }
    }

    if let Some(mut bridge) = bridge {
        bridge.stop();
    }
}

fn configure_bridge(
    app: &AppHandle,
    snapshot: &mut ResidentSnapshot,
    settings: &ResidentSettings,
    executable: &Path,
    probe: &phelper_core::resident::OmenKeyProbe,
) -> (Option<OmenKeyBridge>, Option<Receiver<ResidentEvent>>) {
    if settings.omen_key.action == OmenKeyAction::Default {
        snapshot.omen_key = probe.capability;
        // Cleanup is unconditional.  The provider probe can fail after a
        // previous run left our permanent subscription behind, and cleanup
        // must not depend on the current provider capability result.
        match remove_omen_key_subscription() {
            Ok(()) => snapshot.omen_key_detail = None,
            Err(error) if probe.capability == OmenKeyCapability::Unsupported => {
                // No provider means the stale subscription cannot fire; keep
                // the useful capability result while exposing cleanup as a
                // diagnostic rather than making the Default action look
                // like an active bridge failure.
                snapshot.omen_key_detail =
                    Some(format!("{}；旧桥接清理未完成：{error}", probe.detail));
            }
            Err(error) => {
                snapshot.omen_key = OmenKeyCapability::Error;
                snapshot.omen_key_detail = Some(format!("清理 OMEN 键桥接失败：{error}"));
            }
        }
        return (None, None);
    }
    if probe.capability != OmenKeyCapability::Supported {
        snapshot.omen_key = probe.capability;
        snapshot.omen_key_detail = Some(probe.detail.clone());
        return (None, None);
    }

    let current_profile = app.state().desired.profile;
    if let Err(error) = resolve_omen_key(settings, current_profile.as_deref()) {
        snapshot.omen_key = OmenKeyCapability::Error;
        snapshot.omen_key_detail = Some(error);
        return (None, None);
    }

    match OmenKeyBridge::start(executable) {
        Ok((bridge, events)) => {
            snapshot.omen_key = OmenKeyCapability::Supported;
            snapshot.omen_key_detail = Some("已启用".into());
            (Some(bridge), Some(events))
        }
        Err(error) => {
            snapshot.omen_key = OmenKeyCapability::Error;
            snapshot.omen_key_detail = Some(error);
            (None, None)
        }
    }
}

fn handle_event(
    app: &AppHandle,
    ui_tx: &Sender<ResidentUiEvent>,
    settings: &ResidentSettings,
    snapshot: &mut ResidentSnapshot,
) {
    let current_profile = app.state().desired.profile;
    match resolve_omen_key(settings, current_profile.as_deref()) {
        Ok(Some(OmenKeyIntent::ToggleOverlay)) => {
            let _ = ui_tx.send(ResidentUiEvent::ToggleOverlay);
        }
        Ok(Some(OmenKeyIntent::NextProfile { profile })) => {
            app.dispatch(KnobId::Profile, ControlCommand::ApplyProfile { profile });
        }
        Ok(Some(OmenKeyIntent::SendShortcut { shortcut })) => {
            if let Err(error) = send_shortcut(&shortcut) {
                snapshot.omen_key_detail = Some(error);
                publish(app, snapshot);
            }
        }
        Ok(None) => {}
        Err(error) => {
            snapshot.omen_key_detail = Some(error);
            publish(app, snapshot);
        }
    }
}

fn reconcile_autostart_state(
    app: &AppHandle,
    snapshot: &mut ResidentSnapshot,
    enabled: bool,
    executable: &Path,
) {
    snapshot.autostart = AutostartState::Unknown;
    snapshot.autostart_detail = None;
    publish(app, snapshot);
    match reconcile_autostart(enabled, executable) {
        Ok(state) => snapshot.autostart = state,
        Err(error) => {
            snapshot.autostart = AutostartState::Error;
            snapshot.autostart_detail = Some(error);
        }
    }
    publish(app, snapshot);
}

fn publish(app: &AppHandle, snapshot: &mut ResidentSnapshot) {
    // The overlay controller owns this bit.  Preserve it when the slower
    // resident worker publishes a capability/autostart update.
    snapshot.overlay_visible = app.state().resident.overlay_visible;
    app.set_resident_snapshot(snapshot.clone());
}
