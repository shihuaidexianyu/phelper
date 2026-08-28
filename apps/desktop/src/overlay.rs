//! Compact, read-only performance overlay.
//!
//! The overlay is a normal GPUI window with a small Windows style adjustment:
//! topmost, no activation and disabled input.  It is created once, hidden by
//! default and repainted from the existing `AppHandle` snapshot at most four
//! times per second.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicIsize, Ordering},
};
use std::time::Duration;

use gpui::{
    Bounds, Context, InteractiveElement, IntoElement, ParentElement, Render, Styled, Task, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, div, point, px, rgba,
    size,
};
use gpui_component::{ActiveTheme, StyledExt};
use phelper_core::app::runtime::AppHandle;
use phelper_domain::resident::OverlayPosition;
use phelper_domain::telemetry::{MetricId, ids};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

pub const WIDTH: f32 = 292.;
pub const HEIGHT: f32 = 96.;

#[derive(Clone)]
pub struct OverlayController {
    hwnd: Arc<AtomicIsize>,
    visible: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    position: Arc<Mutex<OverlayPosition>>,
}

impl OverlayController {
    pub fn new(position: OverlayPosition) -> Self {
        Self {
            hwnd: Arc::new(AtomicIsize::new(0)),
            visible: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            position: Arc::new(Mutex::new(position)),
        }
    }

    pub fn bind(&self, hwnd: isize) {
        self.hwnd.store(hwnd, Ordering::Release);
        if hwnd != 0 {
            configure_window(hwnd, self.position());
            set_raw_visible(hwnd, self.visible());
        }
    }

    pub fn set_visible(&self, visible: bool) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        self.visible.store(visible, Ordering::Release);
        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            set_raw_visible(hwnd, visible);
        }
    }

    pub fn toggle(&self) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        let visible = !self.visible.load(Ordering::Acquire);
        self.set_visible(visible);
        visible
    }

    pub fn visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    /// Stop the overlay's repaint task before the owning GPUI windows are
    /// destroyed.  Without an explicit terminal bit, the 250 ms task can
    /// wake after `quit()` and try to update a window that no longer exists.
    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::Release);
        self.visible.store(false, Ordering::Release);
        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            set_raw_visible(hwnd, false);
        }
    }

    pub fn set_position(&self, position: OverlayPosition) {
        if let Ok(mut current) = self.position.lock() {
            *current = position;
        }
        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            configure_window(hwnd, position);
        }
    }

    fn position(&self) -> OverlayPosition {
        self.position
            .lock()
            .map(|position| *position)
            .unwrap_or_default()
    }
}

pub struct OverlayView {
    app: AppHandle,
    last_frame: Option<(usize, Option<String>)>,
    _tick: Task<()>,
}

impl OverlayView {
    pub fn new(
        app: AppHandle,
        controller: OverlayController,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let hwnd = raw_hwnd(window);
        controller.bind(hwnd);
        let tick_controller = controller.clone();
        let _tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if tick_controller.stopped.load(Ordering::Acquire) {
                    break;
                }
                if !tick_controller.visible() {
                    continue;
                }
                if this
                    .update(cx, |this, cx| {
                        let state = this.app.state();
                        let telemetry = state
                            .telemetry
                            .as_ref()
                            .map(|snapshot| Arc::as_ptr(snapshot) as usize)
                            .unwrap_or_default();
                        let frame = (telemetry, state.desired.profile.clone());
                        if this.last_frame.as_ref() != Some(&frame) {
                            this.last_frame = Some(frame);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            app,
            last_frame: None,
            _tick,
        }
    }
}

impl Render for OverlayView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let state = self.app.state();
        let snap = state.telemetry.as_deref();
        let value = |id: MetricId, decimals: usize| {
            snap.and_then(|snapshot| snapshot.samples.get(&id))
                .and_then(|sample| sample.value.as_f64())
                .map(|value| format_value(value, decimals))
                .unwrap_or_else(|| "—".into())
        };
        let profile = state
            .desired
            .profile
            .as_deref()
            .map(profile_name)
            .unwrap_or("未选择")
            .to_string();
        let cpu = format!(
            "{}°C  {}W  {}%",
            value(ids::CPU_PKG_TEMP_C, 0),
            value(ids::CPU_PKG_POWER_W, 0),
            value(ids::CPU_UTIL_PERCENT, 0)
        );
        let gpu = format!(
            "{}°C  {}W  {}%",
            value(ids::GPU_TEMP_C, 0),
            value(ids::GPU_POWER_W, 0),
            value(ids::GPU_UTIL_PERCENT, 0)
        );
        let fans = format!(
            "{} / {} RPM",
            value(ids::FAN_CPU_RPM, 0),
            value(ids::FAN_GPU_RPM, 0)
        );
        let ac_online = snap
            .and_then(|snapshot| snapshot.samples.get(&ids::POWER_AC_ONLINE))
            .and_then(|sample| sample.value.as_f64())
            .map(|value| value > 0.5);
        let battery = snap
            .and_then(|snapshot| snapshot.samples.get(&ids::POWER_BATTERY_PERCENT))
            .and_then(|sample| sample.value.as_f64());
        let power = match (ac_online, battery) {
            (Some(true), Some(value)) => format!("交流  电池 {}%", format_value(value, 0)),
            (Some(true), None) => "交流".to_string(),
            (Some(false), Some(value)) => format!("电池 {}%", format_value(value, 0)),
            _ => "电源 —".to_string(),
        };

        div()
            .id("phelper-overlay")
            .w(px(WIDTH))
            .h(px(HEIGHT))
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(rgba(0x0b1016ee))
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .w_full()
                    .child(
                        div()
                            .h_flex()
                            .justify_between()
                            .child(div().text_xs().font_semibold().child(profile))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{power}  ·  {fans}")),
                            ),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.warning)
                                    .child(format!("CPU  {cpu}")),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.chart_4)
                                    .child(format!("GPU  {gpu}")),
                            ),
                    ),
            )
    }
}

pub fn options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(24.), px(48.)),
            size: size(px(WIDTH), px(HEIGHT)),
        })),
        titlebar: None,
        focus: false,
        show: false,
        kind: WindowKind::PopUp,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    }
}

fn format_value(value: f64, decimals: usize) -> String {
    match decimals {
        0 => format!("{value:.0}"),
        1 => format!("{value:.1}"),
        _ => format!("{value:.2}"),
    }
}

fn profile_name(name: &str) -> &str {
    match name {
        "silent" => "安静",
        "balanced" => "均衡",
        "gaming" => "游戏",
        "cpu-max" => "极致",
        _ => name,
    }
}

fn raw_hwnd(window: &Window) -> isize {
    match HasWindowHandle::window_handle(window).map(|handle| handle.as_raw()) {
        Ok(RawWindowHandle::Win32(handle)) => handle.hwnd.get(),
        _ => 0,
    }
}

#[cfg(target_os = "windows")]
fn configure_window(hwnd: isize, position: OverlayPosition) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetSystemMetrics, GetWindowLongPtrW, HWND_TOPMOST, SM_CXSCREEN,
        SWP_NOACTIVATE, SWP_NOSIZE, SetWindowLongPtrW, SetWindowPos, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TRANSPARENT,
    };
    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let style = style | WS_EX_LAYERED.0 | WS_EX_TRANSPARENT.0 | WS_EX_NOACTIVATE.0;
    unsafe {
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style as isize);
        // Disabled windows cannot take focus or keyboard/mouse activation;
        // WS_EX_TRANSPARENT keeps the overlay visually passive as well.
        let _ = EnableWindow(hwnd, false);
        let width = GetSystemMetrics(SM_CXSCREEN);
        let x = match position {
            OverlayPosition::TopLeft => 24,
            OverlayPosition::TopRight => (width - WIDTH as i32 - 24).max(24),
        };
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            48,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn configure_window(_hwnd: isize, _position: OverlayPosition) {}

#[cfg(target_os = "windows")]
fn set_raw_visible(hwnd: isize, visible: bool) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow};
    unsafe {
        let _ = ShowWindow(
            HWND(hwnd as *mut core::ffi::c_void),
            if visible { SW_SHOWNOACTIVATE } else { SW_HIDE },
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn set_raw_visible(_hwnd: isize, _visible: bool) {}
