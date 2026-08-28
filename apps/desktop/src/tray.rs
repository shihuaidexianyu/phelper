//! System tray (D12; architecture.md §45 — GPUI ships no tray support, the
//! `tray-icon` crate fills it). tray-icon's Windows impl creates its hidden
//! message window on the CALLING thread and rides that thread's pump, so
//! `install()` MUST run on the GPUI main thread (the run closure). Events
//! arrive on muda's global channels; a forwarder thread relays them onto a
//! plain mpsc so the GPUI-side poll task can drain commands with the rest
//! of the shell's 250 ms tick — no foreign thread ever touches GPUI state.
//!
//! Menu: 显示主窗口 / 显示悬浮窗 / 退出. 退出 drives the SAME graceful shutdown path as
//! the window close button (AR-12) — the caller maps TrayCmd::Quit onto
//! `AppHandle::shutdown` + `cx.quit()`. The tray never writes hardware.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, TrayIconBuilder, TrayIconEvent};

/// Commands the tray can ask the shell for.
pub enum TrayCmd {
    /// Restore the (possibly minimized-to-tray) main window.
    Show,
    /// Toggle the compact read-only overlay.
    ToggleOverlay,
    /// Graceful quit — same path as the window close button (AR-12).
    Quit,
}

/// Install the tray icon + menu. Returns the command receiver the shell
/// polls. The TrayIcon itself is moved into the forwarder thread and lives
/// for the rest of the process (the shell removes it at exit).
pub fn install() -> (mpsc::Receiver<TrayCmd>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel::<TrayCmd>();
    let running = Arc::new(AtomicBool::new(true));

    let menu = Menu::new();
    let show_item = MenuItem::new("显示主窗口", true, None);
    let overlay_item = MenuItem::new("显示/隐藏悬浮窗", true, None);
    let quit_item = MenuItem::new("退出", true, None);
    let _ = menu.append(&show_item);
    let _ = menu.append(&overlay_item);
    let _ = menu.append(&quit_item);
    let show_id = show_item.id().clone();
    let overlay_id = overlay_item.id().clone();
    let quit_id = quit_item.id().clone();

    let icon = TrayIconBuilder::new()
        .with_id("phelper-main")
        .with_menu(Box::new(menu))
        .with_tooltip("phelper — OMEN 16-wf0032TX")
        .with_icon(make_icon())
        .build()
        .expect("tray icon");
    // TrayIcon is Rc-backed (main-thread-only, by design in tray-icon's
    // Windows impl) and process-lifetime — leak it on the main thread. The
    // shell removes dead tray icons on hover after exit.
    let _icon: &'static _ = Box::leak(Box::new(icon));

    let forward_running = Arc::clone(&running);
    std::thread::Builder::new()
        .name("tray-forward".into())
        .spawn(move || {
            // Only the 'static global event channels are touched here —
            // never the TrayIcon itself.
            while forward_running.load(Ordering::Acquire) {
                for ev in MenuEvent::receiver().try_iter() {
                    let command = if *ev.id() == quit_id {
                        TrayCmd::Quit
                    } else if *ev.id() == show_id {
                        TrayCmd::Show
                    } else if *ev.id() == overlay_id {
                        TrayCmd::ToggleOverlay
                    } else {
                        continue;
                    };
                    if tx.send(command).is_err() {
                        forward_running.store(false, Ordering::Release);
                        return;
                    }
                }
                for ev in TrayIconEvent::receiver().try_iter() {
                    if let TrayIconEvent::DoubleClick {
                        button: MouseButton::Left,
                        ..
                    } = ev
                    {
                        if tx.send(TrayCmd::Show).is_err() {
                            forward_running.store(false, Ordering::Release);
                            return;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
        .expect("tray forwarder");

    (rx, running)
}

/// 32×32 RGBA app mark, drawn at runtime so the tray has no external asset
/// dependency. The source-of-truth vector is `assets/phelper-icon.svg`.
fn make_icon() -> tray_icon::Icon {
    const N: usize = 32;
    let mut rgba = vec![0u8; N * N * 4];
    for y in 0..N {
        for x in 0..N {
            let i = (y * N + x) * 4;
            // Rounded-corner square (radius 7), transparent outside.
            let r = 7.0f32;
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let inside = {
                let cx = fx.clamp(r, N as f32 - r);
                let cy = fy.clamp(r, N as f32 - r);
                (fx - cx).powi(2) + (fy - cy).powi(2) <= r * r
            };
            if !inside {
                continue;
            }
            // Keep a one-pixel outline around the dark mark.
            let edge = {
                let ex = fx.clamp(r, N as f32 - r);
                let ey = fy.clamp(r, N as f32 - r);
                ((fx - ex).powi(2) + (fy - ey).powi(2) - (r - 1.0).powi(2)).abs() < 2.0
                    || x == 1
                    || y == 1
                    || x == N - 2
                    || y == N - 2
            };
            let stem = (7..=10).contains(&x) && (8..=25).contains(&y);
            let bowl = (7..=21).contains(&x) && (7..=11).contains(&y)
                || (18..=23).contains(&x) && (9..=21).contains(&y)
                || (7..=21).contains(&x) && (19..=22).contains(&y);
            let control_point = (20..=24).contains(&x) && (6..=10).contains(&y);
            let (rr, gg, bb) = if control_point {
                (255, 180, 84)
            } else if stem {
                (102, 230, 245)
            } else if bowl {
                (242, 246, 247)
            } else if edge {
                (46, 57, 68)
            } else {
                (16, 21, 27)
            };
            rgba[i] = rr;
            rgba[i + 1] = gg;
            rgba[i + 2] = bb;
            rgba[i + 3] = 255;
        }
    }
    tray_icon::Icon::from_rgba(rgba, N as u32, N as u32).expect("icon")
}

/// Bring the main window back from minimized/hidden (tray 显示 / double
/// click). Foreground stealing is best-effort by design — if Windows blocks
/// it the taskbar flash tells the user where the window is.
#[cfg(target_os = "windows")]
pub fn show_window(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{SW_RESTORE, SetForegroundWindow, ShowWindow};
    unsafe {
        let h = HWND(hwnd as *mut core::ffi::c_void);
        let _ = ShowWindow(h, SW_RESTORE);
        let _ = SetForegroundWindow(h);
    }
}

/// Minimize-to-tray: when the window is minimized, hide it so it lives only
/// in the tray (called on the shell's tick; cheap no-op otherwise).
#[cfg(target_os = "windows")]
pub fn hide_if_minimized(hwnd: isize) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, SW_HIDE, ShowWindow};
    unsafe {
        let h = HWND(hwnd as *mut core::ffi::c_void);
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}
