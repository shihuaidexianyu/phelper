#![windows_subsystem = "windows"]

//! Minimal desktop shell over `phelper_core::app`.

mod pages;
mod shell;
mod state_store;
mod theme;
mod widgets;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use gpui::*;
use gpui_component::*;

use phelper_core::app::runtime::{AppHandle, StatePublisher};
use phelper_core::app::state::AppState;

use shell::ShellView;
use state_store::GpuiStatePublisher;

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

#[cfg(target_os = "windows")]
fn single_instance_guard() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;
    let name: Vec<u16> = "Local\\phelper-desktop-8bab-single-instance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.ok()?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        message_box("phelper 已在运行", "已有一个 phelper 实例正在运行。");
        return None;
    }
    Some(handle)
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

fn main() {
    if !ensure_elevated() {
        return;
    }
    let _single_instance = match single_instance_guard() {
        Some(handle) => handle,
        None => return,
    };
    let _log_guard = init_logging();
    tracing::info!("phelper-desktop starting");

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
        let shutting_down = Arc::new(AtomicBool::new(false));

        let app_for_window = app.clone();
        let app_state_for_window = app_state_entity.clone();
        let bounds = WindowBounds::Windowed(Bounds {
            origin: point(px(180.), px(90.)),
            size: size(px(840.), px(560.)),
        });

        let main_window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(bounds),
                    focus: true,
                    show: true,
                    ..TitleBar::window_options()
                },
                move |window, cx| {
                    let view =
                        cx.new(|cx| ShellView::new(app_for_window, app_state_for_window, cx));
                    cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
                },
            )
            .expect("open window");

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
}
