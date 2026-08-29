#![windows_subsystem = "windows"]

//! phelper desktop — GPUI shell over `phelper_core::app` (M6).
//!
//! Process model: self-elevating (see Cargo.toml), one "app-pump" thread
//! owns the Engine (AR-12 shutdown on window close), the GPUI thread only
//! reads `AppState` snapshots and enqueues commands (AR-01). This file is
//! the shell bootstrap; pages live in `pages/`, widgets in `widgets/`.

mod fingerprint;
mod overlay;
mod pages;
mod resident;
mod shell;
mod tray;
mod widgets;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use gpui::*;
use gpui_component::*;

use phelper_core::app::runtime::AppHandle;
use phelper_core::app::state::AppState;

use overlay::OverlayController;
use resident::ResidentRuntime;
use shell::ShellView;

/// One pump-side request — either a state mutation (the pump updates
/// fields and expects observers to fire) or a snapshot read (the pump's
/// `dispatch_gate` validates a profile apply).
enum GpuiPublisherRequest {
    Update(Box<dyn FnOnce(&mut AppState) + Send>),
    Snapshot(futures::channel::oneshot::Sender<AppState>),
}

/// GPUI-thread bridge: the pump thread sends requests through an async
/// channel; a foreground task spawned in `application().run` drains the
/// channel and routes Updates through `entity.update(..., cx.notify())`,
/// Snapshots via `entity.read()`. `AsyncApp` is `!Send` (Rc-backed), so we
/// can't keep it in the publisher struct — the foreground task owns the
/// GPUI side; the publisher just owns a `Sender` (behind a Mutex so the
/// pump can `try_send` from any thread).
///
/// §CRITICAL — use `futures::channel::mpsc`, NOT `std::sync::mpsc`. The
/// GPUI foreground executor is single-threaded; a sync-blocking `recv()`
/// in an `cx.spawn` task freezes the entire UI (the executor never
/// reaches the render loop). Async `next().await` yields to the executor
/// while waiting and the UI stays interactive.
pub struct GpuiStatePublisher {
    tx: std::sync::Mutex<futures::channel::mpsc::Sender<GpuiPublisherRequest>>,
}

impl phelper_core::app::runtime::StatePublisher for GpuiStatePublisher {
    fn update(&self, apply: Box<dyn FnOnce(&mut AppState) + Send>) {
        // Best-effort: if the GPUI foreground is gone the app is shutting
        // down anyway — drop the update rather than block the pump.
        if let Ok(mut tx) = self.tx.lock() {
            let _ = tx.try_send(GpuiPublisherRequest::Update(apply));
        }
    }

    fn snapshot(&self) -> AppState {
        let (reply_tx, mut reply_rx) = futures::channel::oneshot::channel();
        let send_ok = self
            .tx
            .lock()
            .map(|mut tx| tx.try_send(GpuiPublisherRequest::Snapshot(reply_tx)).is_ok())
            .unwrap_or(false);
        if send_ok {
            // Block the pump briefly waiting for the foreground to answer.
            // In practice the bridge task answers within a few ms; if it
            // doesn't, we'd rather give the pump a stale default than
            // deadlock the dispatch path.
            match reply_rx.try_recv() {
                Ok(Some(snap)) => snap,
                _ => AppState::default(),
            }
        } else {
            AppState::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaunchMode {
    Normal,
    Background,
    SignalOmenKey,
}

fn launch_mode() -> LaunchMode {
    let mut mode = LaunchMode::Normal;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--signal-omen-key" => return LaunchMode::SignalOmenKey,
            "--background" => mode = LaunchMode::Background,
            _ => {}
        }
    }
    mode
}

/// GUI process has no console: tracing goes to
/// `%LOCALAPPDATA%\phelper\logs\phelper-desktop.log` (§60.14).
fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    let dir = phelper_core::persistence::data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let file = tracing_appender::rolling::never(dir, "phelper-desktop.log");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    guard
}

/// Elevation gate (see Cargo.toml): gpui.lib owns the single application
/// manifest slot, so we self-elevate — relaunch with `runas` and exit.
fn ensure_elevated() -> bool {
    use phelper_core::elevation::{Relaunch, is_elevated, relaunch_elevated};
    if is_elevated() {
        return true;
    }
    match relaunch_elevated() {
        Relaunch::Launched => false,
        Relaunch::Declined => {
            message_box(
                "phelper 需要管理员权限",
                "控制硬件需要提权运行。已取消启动。",
            );
            false
        }
        Relaunch::Failed(e) => {
            message_box("phelper 提权失败", &format!("无法启动提权实例：{e}"));
            false
        }
    }
}

#[cfg(target_os = "windows")]
fn message_box(title: &str, body: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::PCWSTR;
    let t: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let b: Vec<u16> = body.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(b.as_ptr()),
            PCWSTR(t.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// Single-instance guard (D12): minimize-to-tray makes "already running"
/// the common case, and a second instance would mean a SECOND
/// ControlCoordinator — two single-writers fighting over the same fans
/// (keepalive cross-re-assertion). Fail closed (AR-11): the second process
/// tells the user where the first one lives and exits.
#[cfg(target_os = "windows")]
fn single_instance_guard(background: bool) -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;
    let name: Vec<u16> = "Local\\phelper-desktop-8bab-single-instance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.ok()?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        if !background {
            message_box(
                "phelper 已在运行",
                "已有一个 phelper 实例在运行（可能最小化在系统托盘）。本实例将退出。",
            );
        }
        return None;
    }
    Some(h)
}

fn shutdown_app(
    cx: &mut App,
    app: &AppHandle,
    overlay: &OverlayController,
    resident: &Arc<Mutex<ResidentRuntime>>,
    tray_running: &AtomicBool,
    shutting_down: &AtomicBool,
    reason: &'static str,
) {
    if shutting_down.swap(true, Ordering::AcqRel) {
        return;
    }
    overlay.shutdown();
    app.set_overlay_visible(false);
    tray_running.store(false, Ordering::Release);
    if let Ok(mut runtime) = resident.lock() {
        runtime.stop();
    }
    let t = std::time::Instant::now();
    tracing::info!(%reason, "ui shutdown begin");
    app.shutdown(Duration::from_secs(40));
    tracing::info!(elapsed_ms = t.elapsed().as_millis(), %reason, "ui shutdown end");
    cx.quit();
}

fn main() {
    let mode = launch_mode();
    if mode == LaunchMode::SignalOmenKey {
        // The WMI consumer launches this tiny mode.  It must not initialize
        // GPUI, elevate, create a second engine, or touch hardware.
        let _ = phelper_core::resident::signal_omen_key();
        return;
    }
    if !ensure_elevated() {
        return;
    }
    let background = mode == LaunchMode::Background;
    let _single_instance = match single_instance_guard(background) {
        Some(h) => h,
        None => return,
    };
    let _log_guard = init_logging();
    tracing::info!("phelper-desktop starting");

    let (ui_settings, warn) = phelper_core::app::settings::UiSettings::load();
    if let Some(w) = warn {
        tracing::warn!("{w}");
    }
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            tracing::error!(%error, "cannot resolve phelper executable path");
            return;
        }
    };

    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);

        // §Phase 1: the GPUI-thread entity that owns the live AppState.
        // Created here so both the shell and the overlay can `cx.observe`
        // it; the pump thread writes through the `GpuiStatePublisher`
        // bridge, which mutates and notifies this entity.
        let app_state_entity: Entity<AppState> = cx.new(|_| AppState::default());
        // The bridge channel: bounded so a stuck foreground can't make the
        // pump grow unbounded memory. Updates are best-effort (try_send);
        // snapshots are replied via oneshot (not blocking — see below).
        //
        // CRITICAL: this is `futures::channel::mpsc`, NOT `std::sync::mpsc`.
        // A sync-blocking `recv()` in an `cx.spawn` task freezes the GPUI
        // foreground executor (single-threaded) and the UI never renders.
        // `next().await` yields to the executor while waiting.
        let (publisher_tx, mut publisher_rx) =
            futures::channel::mpsc::channel::<GpuiPublisherRequest>(64);
        let entity_for_bridge = app_state_entity.clone();
        cx.spawn(async move |async_app: &mut AsyncApp| {
            use futures::StreamExt;
            while let Some(req) = publisher_rx.next().await {
                match req {
                    GpuiPublisherRequest::Update(apply) => {
                        async_app.update_entity(&entity_for_bridge, |s, cx| {
                            apply(s);
                            cx.notify();
                        });
                    }
                    GpuiPublisherRequest::Snapshot(reply) => {
                        let snap =
                            async_app.read_entity(&entity_for_bridge, |s, _| s.clone());
                        let _ = reply.send(snap);
                    }
                }
                // Yield to the GPUI foreground executor between iterations.
                // Without this, a busy pump (telemetry at 4 Hz × 6 collectors
                // = 24 mut/sec, plus 2 s capability refresh, plus journal tail)
                // keeps this loop running continuously; `next().await` is
                // Ready immediately when the channel has more work, so the
                // executor never gets a chance to schedule a render frame.
                // A 0ms timer schedules a wake-up so other tasks get a turn.
                async_app.background_executor().timer(Duration::ZERO).await;
            }
        })
        .detach();
        let publisher = std::sync::Arc::new(GpuiStatePublisher {
            tx: std::sync::Mutex::new(publisher_tx),
        });
        let app = AppHandle::start_with_publisher(publisher, false);

        // Startup theme from the persisted pref (Settings page edits both
        // the TOML and the live theme; broken/missing file → Dark default).
        let theme = ui_settings.theme;
        let resident_settings = ui_settings.resident.clone();
        let ui_settings_for_view = ui_settings.clone();
        pages::settings::apply_pref(theme, cx);

        let (resident_runtime, resident_rx) =
            match ResidentRuntime::start(app.clone(), resident_settings.clone(), executable) {
                Ok(value) => value,
                Err(error) => {
                    tracing::error!(%error, "resident runtime unavailable");
                    return;
                }
            };
        let resident_owner = std::sync::Arc::new(std::sync::Mutex::new(resident_runtime));
        let resident_handle = resident_owner
            .lock()
            .expect("resident runtime poisoned")
            .handle();
        let overlay_controller = OverlayController::new(ui_settings.resident.overlay.position);
        let shutting_down = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Install the tray on the GPUI thread; tray-icon's Windows backend
        // owns a message window on the calling thread.
        let (tray_rx, tray_running) = tray::install();
        let tray_running_for_window = std::sync::Arc::clone(&tray_running);
        let app_w = app.clone();
        let overlay_for_shell = overlay_controller.clone();
        let bounds = WindowBounds::Windowed(Bounds {
            origin: point(px(180.), px(90.)),
            size: size(px(900.), px(600.)),
        });
        let main_window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(bounds),
                    focus: !background,
                    show: !background,
                    ..Default::default()
                },
                |window, cx| {
                    // The poll task drains TrayCmd and owns minimize-to-tray
                    // hiding; the tray itself was installed above on this
                    // GPUI thread.
                    let hwnd = {
                        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                        match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
                            Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
                            _ => 0,
                        }
                    };
                    let app_t = app_w.clone();
                    let overlay_t = overlay_controller.clone();
                    let resident_t = std::sync::Arc::clone(&resident_owner);
                    let resident_events = resident_rx;
                    let tray_running_t = tray_running_for_window;
                    let shutting_t = std::sync::Arc::clone(&shutting_down);
                    cx.spawn(async move |cx| {
                        loop {
                            cx.background_executor()
                                .timer(Duration::from_millis(250))
                                .await;
                            let mut quit = false;
                            while let Ok(cmd) = tray_rx.try_recv() {
                                match cmd {
                                    tray::TrayCmd::Show => tray::show_window(hwnd),
                                    tray::TrayCmd::ToggleOverlay => {
                                        let visible = overlay_t.toggle();
                                        app_t.set_overlay_visible(visible);
                                    }
                                    tray::TrayCmd::Quit => quit = true,
                                }
                            }
                            while let Ok(event) = resident_events.try_recv() {
                                match event {
                                    resident::ResidentUiEvent::ToggleOverlay => {
                                        let visible = overlay_t.toggle();
                                        app_t.set_overlay_visible(visible);
                                    }
                                }
                            }
                            if hwnd != 0 {
                                tray::hide_if_minimized(hwnd);
                            }
                            if quit {
                                cx.update(|cx| {
                                    shutdown_app(
                                        cx,
                                        &app_t,
                                        &overlay_t,
                                        &resident_t,
                                        &tray_running_t,
                                        &shutting_t,
                                        "tray quit",
                                    );
                                });
                                break;
                            }
                        }
                    })
                    .detach();
                    let app_state_for_view = app_state_entity.clone();
                    let view = cx.new(|cx| {
                        ShellView::new(
                            app_w,
                            app_state_for_view,
                            ui_settings_for_view,
                            resident_handle,
                            overlay_for_shell,
                            window,
                            cx,
                        )
                    });
                    cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
                },
            )
            .expect("open window");

        let overlay_app = app.clone();
        let overlay_controller_for_view = overlay_controller.clone();
        let app_state_for_overlay = app_state_entity.clone();
        let overlay_handle = cx
            .open_window(overlay::options(), move |window, cx| {
                let view = cx.new(|cx| {
                    overlay::OverlayView::new(
                        overlay_app,
                        app_state_for_overlay,
                        overlay_controller_for_view,
                        window,
                        cx,
                    )
                });
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("open overlay window");
        let _overlay_window_id = overlay_handle.window_id();
        if resident_settings.overlay.visible_on_start {
            overlay_controller.set_visible(true);
            app.set_overlay_visible(true);
        }

        // AR-12: closing the last window drives the full graceful engine
        // shutdown (firmware-auto restore) BEFORE process exit. Timed
        // (v0.2-e): the M6 HIL's one ~38 s close now has a begin/end
        // bracket on the UI side to pair with the pump's stage logs.
        let main_window_id = main_window.window_id();
        let app_c = app.clone();
        let overlay_c = overlay_controller.clone();
        let resident_c = Arc::clone(&resident_owner);
        let tray_running_c = Arc::clone(&tray_running);
        let shutting_c = Arc::clone(&shutting_down);
        cx.on_window_closed(move |cx, closed_window_id| {
            if closed_window_id == main_window_id {
                shutdown_app(
                    cx,
                    &app_c,
                    &overlay_c,
                    &resident_c,
                    &tray_running_c,
                    &shutting_c,
                    "window closed",
                );
            }
        })
        .detach();
    });
}
