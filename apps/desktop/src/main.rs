#![windows_subsystem = "windows"]

//! Minimal desktop shell over `phelper_core::app`.

mod pages;
mod resident;
mod shell;
mod state_store;
mod theme;
mod widgets;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use gpui::*;
use gpui_component::*;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, SW_HIDE, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
};

use phelper_core::app::runtime::{AppHandle, StatePublisher};
use phelper_core::app::state::AppState;

use shell::ShellView;
use state_store::GpuiStatePublisher;

fn native_hwnd(window: &Window) -> Option<HWND> {
    let handle = HasWindowHandle::window_handle(window).ok()?.as_raw();
    let RawWindowHandle::Win32(handle) = handle else {
        return None;
    };
    Some(HWND(handle.hwnd.get() as *mut core::ffi::c_void))
}

fn set_window_visible(window: &Window, visible: bool) {
    let Some(hwnd) = native_hwnd(window) else {
        return;
    };
    unsafe {
        if visible {
            let command = if IsIconic(hwnd).as_bool() {
                SW_RESTORE
            } else {
                SW_SHOW
            };
            let _ = ShowWindow(hwnd, command);
            let _ = SetForegroundWindow(hwnd);
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

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
        Relaunch::Failed(error) => {
            message_box("phelper 提权失败", &format!("无法启动提权实例：{error}"));
            false
        }
    }
}

#[cfg(target_os = "windows")]
fn message_box(title: &str, body: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::PCWSTR;
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let body: Vec<u16> = body.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(body.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn shutdown_app(cx: &mut App, app: &AppHandle, shutting_down: &AtomicBool, reason: &'static str) {
    if shutting_down.swap(true, Ordering::AcqRel) {
        return;
    }
    let started = std::time::Instant::now();
    tracing::info!(%reason, "ui shutdown begin");
    app.shutdown(Duration::from_secs(40));
    tracing::info!(elapsed_ms = started.elapsed().as_millis(), %reason, "ui shutdown end");
    cx.quit();
}

fn update_resident_autostart_state(
    state: &Arc<Mutex<resident::ResidentUiState>>,
    result: Result<bool, String>,
) {
    let mut state = state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.autostart_busy = false;
    match result {
        Ok(enabled) => {
            state.autostart = Some(enabled);
            state.autostart_error = None;
        }
        Err(error) => {
            tracing::warn!(%error, "autostart operation failed");
            state.autostart_error = Some(error);
        }
    }
}

fn main() {
    let launch_mode = resident::LaunchMode::current();
    if resident::signal_existing_instance() {
        return;
    }
    if !ensure_elevated() {
        return;
    }
    let _single_instance = match resident::InstanceGuard::acquire() {
        Ok(Some(guard)) => guard,
        Ok(None) => return,
        Err(error) => {
            message_box("phelper 启动失败", &error);
            return;
        }
    };
    let _log_guard = init_logging();
    tracing::info!("phelper-desktop starting");

    let shutting_down = Arc::new(AtomicBool::new(false));
    let (show_signal_tx, show_signal_rx) = std::sync::mpsc::channel();
    let show_listener =
        _single_instance.spawn_show_listener(show_signal_tx, Arc::clone(&shutting_down));
    let shutting_after_run = Arc::clone(&shutting_down);

    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);
        theme::apply(cx);

        let app_state_entity: Entity<AppState> = cx.new(|_| AppState::default());
        let (publisher, mut wake_rx) = GpuiStatePublisher::new();

        let publisher_for_bridge = Arc::clone(&publisher);
        let entity_for_bridge = app_state_entity.clone();
        cx.spawn(async move |async_app: &mut AsyncApp| {
            use futures::StreamExt;
            while wake_rx.next().await.is_some() {
                let snapshot = publisher_for_bridge.snapshot();
                async_app.update_entity(&entity_for_bridge, |state, cx| {
                    *state = snapshot;
                    cx.notify();
                });
                // The overview's fastest useful cadence is 250 ms. A short
                // pause caps repaint churn while queued wakes still collapse
                // onto the latest authoritative snapshot.
                async_app
                    .background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        })
        .detach();

        let app = AppHandle::start_with_publisher(publisher);
        let resident_state = Arc::new(Mutex::new(resident::ResidentUiState::default()));
        let (resident_command_tx, resident_command_rx) = std::sync::mpsc::channel();
        let window_visible = Arc::new(AtomicBool::new(matches!(
            launch_mode,
            resident::LaunchMode::Windowed
        )));

        let app_for_window = app.clone();
        let app_state_for_window = app_state_entity.clone();
        let resident_state_for_window = Arc::clone(&resident_state);
        let resident_commands_for_window = resident_command_tx.clone();
        let bounds = WindowBounds::Windowed(Bounds {
            origin: point(px(180.), px(90.)),
            size: size(px(840.), px(560.)),
        });

        let main_window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(bounds),
                    focus: matches!(launch_mode, resident::LaunchMode::Windowed),
                    show: matches!(launch_mode, resident::LaunchMode::Windowed),
                    ..TitleBar::window_options()
                },
                move |window, cx| {
                    let view = cx.new(|cx| {
                        ShellView::new(
                            app_for_window,
                            app_state_for_window,
                            resident_state_for_window,
                            resident_commands_for_window,
                            cx,
                        )
                    });
                    cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
                },
            )
            .expect("open window");

        let tray = match resident::TrayRuntime::new(Arc::clone(&window_visible)) {
            Ok(tray) => std::rc::Rc::new(tray),
            Err(error) => {
                message_box("phelper 托盘启动失败", &error);
                shutdown_app(cx, &app, &shutting_down, "tray initialization failed");
                return;
            }
        };

        // Covers Windows logoff/shutdown and other platform-level quit
        // requests. Tray exit sets the same flag before cx.quit(), so the
        // hardware restoration path runs exactly once.
        let app_for_platform_quit = app.clone();
        let shutting_for_platform_quit = Arc::clone(&shutting_down);
        cx.on_app_quit(move |_| {
            if !shutting_for_platform_quit.swap(true, Ordering::AcqRel) {
                app_for_platform_quit.shutdown(Duration::from_secs(40));
            }
            async {}
        })
        .detach();

        let shutting_for_intercept = Arc::clone(&shutting_down);
        let visible_for_intercept = Arc::clone(&window_visible);
        let tray_for_intercept = std::rc::Rc::clone(&tray);
        let _ = main_window.update(cx, move |_, window, app_cx| {
            window.on_window_should_close(app_cx, move |window, _| {
                if shutting_for_intercept.load(Ordering::Acquire) {
                    return true;
                }
                set_window_visible(window, false);
                visible_for_intercept.store(false, Ordering::Release);
                tray_for_intercept.set_window_visible(false);
                false
            });
        });

        let main_window_for_resident = main_window;
        let app_for_resident = app.clone();
        let shutting_for_resident = Arc::clone(&shutting_down);
        let tray_for_resident = std::rc::Rc::clone(&tray);
        let resident_state_for_task = Arc::clone(&resident_state);
        cx.spawn(async move |async_app: &mut AsyncApp| {
            let initial_autostart = async_app
                .background_executor()
                .spawn(async { resident::autostart::reconcile() })
                .await;
            update_resident_autostart_state(&resident_state_for_task, initial_autostart);
            let _ = main_window_for_resident.update(async_app, |_, _, cx| cx.notify());

            loop {
                async_app
                    .background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if shutting_for_resident.load(Ordering::Acquire) {
                    break;
                }

                let mut actions = tray_for_resident.drain_actions();
                if show_signal_rx.try_recv().is_ok() {
                    actions.push(resident::TrayAction::Show);
                }
                for action in actions {
                    match action {
                        resident::TrayAction::Show => {
                            let _ = main_window_for_resident.update(async_app, |_, window, _| {
                                set_window_visible(window, true);
                                window.activate_window();
                            });
                            tray_for_resident.set_window_visible(true);
                        }
                        resident::TrayAction::Hide => {
                            let _ = main_window_for_resident.update(async_app, |_, window, _| {
                                set_window_visible(window, false);
                            });
                            tray_for_resident.set_window_visible(false);
                        }
                        resident::TrayAction::Exit => {
                            async_app.update(|cx| {
                                shutdown_app(
                                    cx,
                                    &app_for_resident,
                                    &shutting_for_resident,
                                    "tray exit",
                                );
                            });
                            return;
                        }
                    }
                }

                while let Ok(command) = resident_command_rx.try_recv() {
                    let resident::ResidentCommand::SetAutostart(desired) = command;
                    let result = async_app
                        .background_executor()
                        .spawn(async move { resident::autostart::set_enabled(desired) })
                        .await;
                    update_resident_autostart_state(&resident_state_for_task, result);
                    let _ = main_window_for_resident.update(async_app, |_, _, cx| cx.notify());
                }
            }
        })
        .detach();

        let main_window_id = main_window.window_id();
        let app_for_close = app.clone();
        let shutting_for_close = Arc::clone(&shutting_down);
        cx.on_window_closed(move |cx, closed_window_id| {
            if closed_window_id == main_window_id {
                shutdown_app(cx, &app_for_close, &shutting_for_close, "window closed");
            }
        })
        .detach();
    });

    shutting_after_run.store(true, Ordering::Release);
    let _ = show_listener.join();
}
