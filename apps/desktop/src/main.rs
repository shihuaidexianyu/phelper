#![windows_subsystem = "windows"]

//! phelper desktop — GPUI shell over `phelper_core::app` (M6).
//!
//! Process model: self-elevating (see Cargo.toml), one "app-pump" thread
//! owns the Engine (AR-12 shutdown on window close), the GPUI thread only
//! reads `AppState` snapshots and enqueues commands (AR-01). This file is
//! the shell bootstrap; pages live in `pages/`, widgets in `widgets/`.

mod fingerprint;
mod pages;
mod shell;
mod tray;
mod widgets;

use std::time::Duration;

use gpui::*;
use gpui_component::*;

use phelper_core::app::runtime::AppHandle;

use shell::ShellView;

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
fn single_instance_guard() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;
    let name: Vec<u16> = "Local\\phelper-desktop-8bab-single-instance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.ok()?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        message_box(
            "phelper 已在运行",
            "已有一个 phelper 实例在运行（可能最小化在系统托盘）。本实例将退出。",
        );
        return None;
    }
    Some(h)
}

fn main() {
    if !ensure_elevated() {
        return;
    }
    let _single_instance = match single_instance_guard() {
        Some(h) => h,
        None => return,
    };
    let _log_guard = init_logging();
    tracing::info!("phelper-desktop starting");

    let app = AppHandle::start_fast();

    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);
        // Startup theme from the persisted pref (Settings page edits both
        // the TOML and the live theme; broken/missing file → Dark default).
        let (ui_settings, warn) = phelper_core::app::settings::UiSettings::load();
        if let Some(w) = warn {
            tracing::warn!("{w}");
        }
        let theme = ui_settings.theme;
        pages::settings::apply_pref(theme, cx);

        let app_w = app.clone();
        let bounds = WindowBounds::Windowed(Bounds {
            origin: point(px(180.), px(90.)),
            size: size(px(900.), px(600.)),
        });
        cx.open_window(
            WindowOptions {
                window_bounds: Some(bounds),
                ..Default::default()
            },
            |window, cx| {
                // Tray (D12): install on THIS thread — tray-icon's
                // Windows impl rides the calling thread's message pump,
                // which is GPUI's main loop here. The poll task drains
                // TrayCmd and owns minimize-to-tray hiding.
                let hwnd = {
                    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                    match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
                        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
                        _ => 0,
                    }
                };
                let tray_rx = tray::install();
                let app_t = app_w.clone();
                cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(250))
                            .await;
                        let mut quit = false;
                        while let Ok(cmd) = tray_rx.try_recv() {
                            match cmd {
                                tray::TrayCmd::Show => tray::show_window(hwnd),
                                tray::TrayCmd::Quit => quit = true,
                            }
                        }
                        if hwnd != 0 {
                            tray::hide_if_minimized(hwnd);
                        }
                        if quit {
                            // Same graceful path as the window close
                            // button (AR-12): engine restore, then quit.
                            cx.update(|cx| {
                                let t = std::time::Instant::now();
                                tracing::info!("ui shutdown begin (tray quit)");
                                app_t.shutdown(Duration::from_secs(40));
                                tracing::info!(
                                    elapsed_ms = t.elapsed().as_millis(),
                                    "ui shutdown end (tray quit)"
                                );
                                cx.quit();
                            });
                            break;
                        }
                    }
                })
                .detach();
                let view = cx.new(|cx| ShellView::new(app_w, theme, window, cx));
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            },
        )
        .expect("open window");

        // AR-12: closing the last window drives the full graceful engine
        // shutdown (firmware-auto restore) BEFORE process exit. Timed
        // (v0.2-e): the M6 HIL's one ~38 s close now has a begin/end
        // bracket on the UI side to pair with the pump's stage logs.
        let app_c = app.clone();
        cx.on_window_closed(move |cx, _| {
            if cx.windows().is_empty() {
                let t = std::time::Instant::now();
                tracing::info!("ui shutdown begin (window closed)");
                app_c.shutdown(Duration::from_secs(40));
                tracing::info!(
                    elapsed_ms = t.elapsed().as_millis(),
                    "ui shutdown end (window closed)"
                );
                cx.quit();
            }
        })
        .detach();
    });
}
