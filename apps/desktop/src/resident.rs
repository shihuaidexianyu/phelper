//! Windows resident-app plumbing: startup mode, single-instance wake-up,
//! tray menu, and the one Task Scheduler entry owned by phelper.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;

use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows::core::PCWSTR;

const MUTEX_NAME: &str = "Local\\phelper-desktop-8bab-single-instance";
const SHOW_EVENT_NAME: &str = "Local\\phelper-desktop-8bab-show-window";
const AUTOSTART_TASK_NAME: &str = "phelper-user-logon";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    Windowed,
    Background,
}

impl LaunchMode {
    pub fn from_args(args: impl IntoIterator<Item = OsString>) -> Self {
        if args
            .into_iter()
            .any(|arg| arg.to_string_lossy().eq_ignore_ascii_case("--background"))
        {
            Self::Background
        } else {
            Self::Windowed
        }
    }

    pub fn current() -> Self {
        Self::from_args(std::env::args_os().skip(1))
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Wake an already-running elevated instance before this process asks for
/// elevation. This keeps clicking the shortcut while phelper is in the tray
/// from producing a redundant UAC prompt.
pub fn signal_existing_instance() -> bool {
    let name = wide(SHOW_EVENT_NAME);
    let Ok(event) =
        (unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR::from_raw(name.as_ptr())) })
    else {
        return false;
    };
    let signalled = unsafe { SetEvent(event) }.is_ok();
    let _ = unsafe { CloseHandle(event) };
    signalled
}

pub struct InstanceGuard {
    mutex: HANDLE,
    show_event: HANDLE,
}

impl InstanceGuard {
    pub fn acquire() -> Result<Option<Self>, String> {
        let mutex_name = wide(MUTEX_NAME);
        let mutex = unsafe {
            CreateMutexW(None, false, PCWSTR::from_raw(mutex_name.as_ptr()))
                .map_err(|error| format!("无法创建单实例互斥量：{error}"))?
        };
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            let _ = signal_existing_instance();
            let _ = unsafe { CloseHandle(mutex) };
            return Ok(None);
        }

        let event_name = wide(SHOW_EVENT_NAME);
        let show_event = match unsafe {
            CreateEventW(None, false, false, PCWSTR::from_raw(event_name.as_ptr()))
        } {
            Ok(event) => event,
            Err(error) => {
                let _ = unsafe { CloseHandle(mutex) };
                return Err(format!("无法创建窗口唤醒事件：{error}"));
            }
        };

        Ok(Some(Self { mutex, show_event }))
    }

    pub fn spawn_show_listener(
        &self,
        sender: mpsc::Sender<()>,
        stopping: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        let raw_event = self.show_event.0 as usize;
        thread::Builder::new()
            .name("phelper-instance-wake".into())
            .spawn(move || {
                let event = HANDLE(raw_event as *mut core::ffi::c_void);
                while !stopping.load(Ordering::Acquire) {
                    match unsafe { WaitForSingleObject(event, 250) } {
                        WAIT_OBJECT_0 => {
                            if sender.send(()).is_err() {
                                break;
                            }
                        }
                        WAIT_TIMEOUT => {}
                        _ => break,
                    }
                }
            })
            .expect("spawn instance wake listener")
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.show_event) };
        let _ = unsafe { CloseHandle(self.mutex) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Show,
    Hide,
    Exit,
}

#[derive(Debug, Clone, Default)]
pub struct ResidentUiState {
    pub autostart: Option<bool>,
    pub autostart_busy: bool,
    pub autostart_error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum ResidentCommand {
    SetAutostart(bool),
}

pub struct TrayRuntime {
    _icon: TrayIcon,
    show_item: MenuItem,
    exit_item: MenuItem,
    window_visible: Arc<AtomicBool>,
}

impl TrayRuntime {
    pub fn new(window_visible: Arc<AtomicBool>) -> Result<Self, String> {
        let show_item = MenuItem::with_id("phelper.show", "显示 phelper", true, None);
        let separator = PredefinedMenuItem::separator();
        let exit_item = MenuItem::with_id("phelper.exit", "退出", true, None);
        let menu = Menu::with_items(&[&show_item, &separator, &exit_item])
            .map_err(|error| format!("无法创建托盘菜单：{error}"))?;
        let icon = tray_icon_image()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .with_tooltip("phelper")
            .with_icon(icon)
            .build()
            .map_err(|error| format!("无法创建系统托盘：{error}"))?;

        let runtime = Self {
            _icon: tray,
            show_item,
            exit_item,
            window_visible,
        };
        runtime.refresh_window_label();
        Ok(runtime)
    }

    pub fn set_window_visible(&self, visible: bool) {
        self.window_visible.store(visible, Ordering::Release);
        self.refresh_window_label();
    }

    fn refresh_window_label(&self) {
        let text = if self.window_visible.load(Ordering::Acquire) {
            "隐藏 phelper"
        } else {
            "显示 phelper"
        };
        self.show_item.set_text(text);
    }

    pub fn drain_actions(&self) -> Vec<TrayAction> {
        self.refresh_window_label();
        let mut actions = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == *self.show_item.id() {
                actions.push(if self.window_visible.load(Ordering::Acquire) {
                    TrayAction::Hide
                } else {
                    TrayAction::Show
                });
            } else if event.id == *self.exit_item.id() {
                actions.push(TrayAction::Exit);
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                actions.push(TrayAction::Show);
            }
        }
        actions
    }
}

fn tray_icon_image() -> Result<Icon, String> {
    Icon::from_resource(1, Some((32, 32)))
        .map_err(|error| format!("无法读取内嵌的 phelper 图标：{error}"))
}

pub mod autostart {
    use super::*;
    use std::os::windows::process::CommandExt;
    use windows::Win32::System::Threading::CREATE_NO_WINDOW;

    pub fn query() -> Result<bool, String> {
        let Some(xml) = query_xml()? else {
            return Ok(false);
        };
        task_xml_enabled(&xml)
    }

    /// Repair tasks created by older phelper builds without changing the
    /// user's enabled/disabled choice.
    pub fn reconcile() -> Result<bool, String> {
        let Some(xml) = query_xml()? else {
            return Ok(false);
        };
        if !task_xml_enabled(&xml)? {
            return Ok(false);
        }
        if task_settings_need_repair(&xml) {
            allow_battery_start()?;
        }
        query()
    }

    fn query_xml() -> Result<Option<String>, String> {
        let output = run_schtasks(&["/Query", "/TN", AUTOSTART_TASK_NAME, "/XML"])?;
        if !output.status.success() {
            // schtasks uses exit code 1 when the named task does not exist.
            return Ok(None);
        }
        Ok(Some(decode_output(&output.stdout)))
    }

    pub fn set_enabled(enabled: bool) -> Result<bool, String> {
        if enabled {
            enable()?;
        } else {
            disable()?;
        }
        let actual = query()?;
        if actual != enabled {
            return Err("任务计划程序返回的开机启动状态与请求不一致".into());
        }
        Ok(actual)
    }

    fn enable() -> Result<(), String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("无法确定 phelper 可执行文件路径：{error}"))?;
        let action = task_action(&executable)?;
        let output = run_schtasks(&[
            "/Create",
            "/F",
            "/TN",
            AUTOSTART_TASK_NAME,
            "/SC",
            "ONLOGON",
            "/RL",
            "HIGHEST",
            "/IT",
            "/TR",
            &action,
        ])?;
        command_succeeded("创建开机启动任务", output)?;
        if let Err(error) = allow_battery_start() {
            // Do not leave behind a task whose behavior differs from the
            // promise made by the UI. The next toggle can retry cleanly.
            let _ = run_schtasks(&["/Delete", "/F", "/TN", AUTOSTART_TASK_NAME]);
            return Err(error);
        }
        Ok(())
    }

    fn disable() -> Result<(), String> {
        if !query()? {
            return Ok(());
        }
        let output = run_schtasks(&["/Delete", "/F", "/TN", AUTOSTART_TASK_NAME])?;
        command_succeeded("删除开机启动任务", output)
    }

    fn task_action(executable: &Path) -> Result<String, String> {
        let text = executable
            .to_str()
            .ok_or_else(|| "phelper 可执行文件路径不是有效的 Unicode".to_string())?;
        if text.contains('"') {
            return Err("phelper 可执行文件路径包含无法用于任务计划程序的引号".into());
        }
        Ok(format!("\"{text}\" --background"))
    }

    fn run_schtasks(args: &[&str]) -> Result<Output, String> {
        let executable = schtasks_path();
        let mut command = Command::new(&executable);
        command.args(args).creation_flags(CREATE_NO_WINDOW.0);
        command
            .output()
            .map_err(|error| format!("无法运行 {}：{error}", executable.display()))
    }

    fn allow_battery_start() -> Result<(), String> {
        const UPDATE_SETTINGS: &str = "$settings = New-ScheduledTaskSettingsSet \
            -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries \
            -ExecutionTimeLimit ([TimeSpan]::Zero); \
            Set-ScheduledTask -TaskName 'phelper-user-logon' -Settings $settings | Out-Null";
        let executable = powershell_path();
        let mut command = Command::new(&executable);
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                UPDATE_SETTINGS,
            ])
            .creation_flags(CREATE_NO_WINDOW.0);
        let output = command
            .output()
            .map_err(|error| format!("无法运行 {}：{error}", executable.display()))?;
        command_succeeded("配置开机启动任务", output)
    }

    fn schtasks_path() -> PathBuf {
        std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("System32")
            .join("schtasks.exe")
    }

    fn powershell_path() -> PathBuf {
        std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe")
    }

    fn task_xml_enabled(xml: &str) -> Result<bool, String> {
        if !xml.contains("<Task") {
            return Err("任务计划程序返回了无法识别的任务定义".into());
        }
        // Enabled defaults to true in the Task Scheduler schema, so Windows
        // normally omits the element entirely for an enabled task.
        Ok(!xml.contains("<Enabled>false</Enabled>"))
    }

    fn task_settings_need_repair(xml: &str) -> bool {
        !xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>")
            || !xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>")
            || !xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>")
    }

    fn decode_output(bytes: &[u8]) -> String {
        let (pairs, _) = bytes.as_chunks::<2>();
        let utf16_le = bytes.starts_with(&[0xff, 0xfe])
            || (bytes.len() >= 4 && pairs.iter().take(32).filter(|pair| pair[1] == 0).count() >= 8);
        if utf16_le {
            let start = usize::from(bytes.starts_with(&[0xff, 0xfe])) * 2;
            let (pairs, _) = bytes[start..].as_chunks::<2>();
            let words = pairs
                .iter()
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>();
            String::from_utf16_lossy(&words)
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        }
    }

    fn command_succeeded(operation: &str, output: Output) -> Result<(), String> {
        if output.status.success() {
            Ok(())
        } else {
            let detail = [decode_output(&output.stderr), decode_output(&output.stdout)]
                .into_iter()
                .find(|text| !text.trim().is_empty())
                .unwrap_or_else(|| format!("exit={:?}", output.status.code()));
            Err(format!("{operation}失败：{}", detail.trim()))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn task_action_quotes_the_executable_and_uses_background_mode() {
            let action = task_action(Path::new(r"C:\Program Files\phelper\phelper-desktop.exe"))
                .expect("action");
            assert_eq!(
                action,
                r#""C:\Program Files\phelper\phelper-desktop.exe" --background"#
            );
        }

        #[test]
        fn enabled_defaults_to_true_when_xml_omits_the_element() {
            let xml = r#"<?xml version="1.0"?><Task><Settings /></Task>"#;
            assert!(task_xml_enabled(xml).expect("task xml"));
            assert!(
                !task_xml_enabled(r#"<Task><Settings><Enabled>false</Enabled></Settings></Task>"#)
                    .expect("task xml")
            );
        }

        #[test]
        fn old_default_task_settings_are_repaired() {
            assert!(task_settings_need_repair(
                "<Task><Settings><DisallowStartIfOnBatteries>true</DisallowStartIfOnBatteries></Settings></Task>"
            ));
            assert!(!task_settings_need_repair(
                "<Task><Settings><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings></Task>"
            ));
        }

        #[test]
        fn utf16_task_xml_is_decoded_before_state_detection() {
            let xml = r#"<?xml version="1.0"?><Task><Settings /></Task>"#;
            let mut bytes = vec![0xff, 0xfe];
            bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
            let decoded = decode_output(&bytes);
            assert!(task_xml_enabled(&decoded).expect("task xml"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_icon_is_embedded_as_windows_resource_one() {
        tray_icon_image().expect("embedded phelper icon");
    }

    #[test]
    fn background_argument_is_explicit_and_case_insensitive() {
        assert_eq!(
            LaunchMode::from_args([OsString::from("--background")]),
            LaunchMode::Background
        );
        assert_eq!(
            LaunchMode::from_args([OsString::from("--BACKGROUND")]),
            LaunchMode::Background
        );
        assert_eq!(
            LaunchMode::from_args([OsString::from("--other")]),
            LaunchMode::Windowed
        );
    }
}
