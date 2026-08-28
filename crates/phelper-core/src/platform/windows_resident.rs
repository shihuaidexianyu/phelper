//! Windows resident integrations.
//!
//! This module is deliberately narrow.  It owns only phelper's scheduled task,
//! its own WMI permanent subscription and its own named pipe.  It never scans
//! or removes another application's subscription and it never touches the
//! hardware coordinator.  The desktop shell consumes the resulting event and
//! decides which already-existing application command to enqueue.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use phelper_domain::resident::{AutostartState, OmenKeyCapability};
use wmi::{IWbemClassWrapper, WMIConnection, WMIError};

const WMI_ROOT: &str = "ROOT\\WMI";
const WMI_SUBSCRIPTION_ROOT: &str = "ROOT\\subscription";
const OMEN_PROVIDER_CLASS: &str = "hpqBEvnt";
const OMEN_EVENT_QUERY: &str = "SELECT * FROM hpqBEvnt WHERE EventData = 8613 AND EventID = 29";

// These names are intentionally product-specific.  In particular, do not
// reuse OmenSuperHub/OmenHwCtl names: stale third-party subscriptions are
// never ours to delete.
const FILTER_NAME: &str = "phelper-8bab-OmenKeyFilter";
const CONSUMER_NAME: &str = "phelper-8bab-OmenKeyConsumer";
const PIPE_NAME: &str = r"\\.\pipe\phelper-8bab-omen-key";
const SIGNAL: &[u8] = b"phelper-omen-key-v1";
const SIGNAL_ARGUMENT: &str = "--signal-omen-key";
const BACKGROUND_ARGUMENT: &str = "--background";
// Keep the task flat: `schtasks.exe` can create this task without requiring
// a separately provisioned Task Scheduler folder.
const TASK_NAME: &str = r"\phelperUserLogon";

const ERROR_FILE_NOT_FOUND_CODE: u32 = 2;
const ERROR_PATH_NOT_FOUND_CODE: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmenKeyProbe {
    pub capability: OmenKeyCapability,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidentEvent {
    OmenKeyPressed,
}

/// Probe only the read-only HP event provider.  A `Supported` result means
/// the provider/class exists and exposes the event fields; it does not claim
/// that a physical key has been pressed or that phelper owns a subscription.
pub fn probe_omen_key() -> OmenKeyProbe {
    let connection = match WMIConnection::with_namespace_path(WMI_ROOT) {
        Ok(connection) => connection,
        Err(error) => {
            return OmenKeyProbe {
                capability: classify_probe_error(&error),
                detail: format!("无法访问 {WMI_ROOT}：{error}"),
            };
        }
    };

    let provider = match connection.get_object(OMEN_PROVIDER_CLASS) {
        Ok(provider) => provider,
        Err(error) => {
            return OmenKeyProbe {
                capability: classify_probe_error(&error),
                detail: format!("未找到 {OMEN_PROVIDER_CLASS} 事件提供程序：{error}"),
            };
        }
    };

    let properties = match provider.list_properties() {
        Ok(properties) => properties,
        Err(error) => {
            return OmenKeyProbe {
                capability: OmenKeyCapability::Error,
                detail: format!("读取 {OMEN_PROVIDER_CLASS} 字段失败：{error}"),
            };
        }
    };

    let has_event_data = has_property(&properties, "EventData");
    let has_event_id = has_property(&properties, "EventID");
    if !(has_event_data && has_event_id) {
        return OmenKeyProbe {
            capability: OmenKeyCapability::Unsupported,
            detail: format!(
                "{OMEN_PROVIDER_CLASS} 存在，但缺少 EventData/EventID 字段，不能安全订阅"
            ),
        };
    }

    OmenKeyProbe {
        capability: OmenKeyCapability::Supported,
        detail: format!(
            "已检测到 {OMEN_PROVIDER_CLASS}；OMEN 键候选事件为 EventData=8613、EventID=29，尚未启用桥接"
        ),
    }
}

/// Remove only phelper's own permanent subscription. This is also called
/// when the user returns the OMEN key action to `Default`, so a crashed or
/// previously enabled instance cannot leave a signal consumer behind.
pub fn remove_omen_key_subscription() -> Result<(), String> {
    let connection = WMIConnection::with_namespace_path(WMI_SUBSCRIPTION_ROOT)
        .map_err(|error| format!("打开 WMI 订阅命名空间失败：{error}"))?;
    remove_own_subscription(&connection)
}

/// Reconcile phelper's one current-user logon task.  `schtasks.exe` is
/// launched directly with argument boundaries intact; no shell or user
/// supplied command string is evaluated.
pub fn reconcile_autostart(enabled: bool, executable: &Path) -> Result<AutostartState, String> {
    let schtasks = schtasks_path();

    if enabled {
        let executable = validate_executable(executable)?;
        let task_run = autostart_task_run(&executable);
        let output = Command::new(&schtasks)
            .args([
                "/Create", "/TN", TASK_NAME, "/SC", "ONLOGON", "/RL", "HIGHEST", "/IT", "/TR",
                &task_run, "/F",
            ])
            .output()
            .map_err(|error| format!("启动任务计划程序失败：{error}"))?;
        if output.status.success() {
            Ok(AutostartState::Enabled)
        } else {
            Err(format!(
                "创建开机启动任务失败（{}）：{}",
                output.status,
                command_output(&output)
            ))
        }
    } else {
        let output = Command::new(&schtasks)
            .args(["/Delete", "/TN", TASK_NAME, "/F"])
            .output()
            .map_err(|error| format!("启动任务计划程序失败：{error}"))?;
        if output.status.success() || task_missing(&output) {
            Ok(AutostartState::Disabled)
        } else {
            Err(format!(
                "删除开机启动任务失败（{}）：{}",
                output.status,
                command_output(&output)
            ))
        }
    }
}

fn autostart_task_run(executable: &Path) -> String {
    format!(r#""{}" {BACKGROUND_ARGUMENT}"#, executable.display())
}

/// Connect to the fixed, ACL-protected signal pipe and write one constant
/// payload.  This is the only operation performed by the WMI-launched
/// `--signal-omen-key` process; it does not initialize the engine or elevate.
pub fn signal_omen_key() -> bool {
    use windows::Win32::Foundation::{ERROR_PIPE_BUSY, GENERIC_WRITE, GetLastError};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, OPEN_EXISTING, WriteFile,
    };
    use windows::Win32::System::Pipes::WaitNamedPipeW;
    use windows::core::PCWSTR;

    let pipe = wide(PIPE_NAME);
    for _ in 0..4 {
        let handle = unsafe {
            CreateFileW(
                PCWSTR(pipe.as_ptr()),
                GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };

        match handle {
            Ok(handle) => {
                let mut written = 0u32;
                let result = unsafe { WriteFile(handle, Some(SIGNAL), Some(&mut written), None) };
                unsafe {
                    let _ = windows::Win32::Foundation::CloseHandle(handle);
                }
                return result.is_ok() && written == SIGNAL.len() as u32;
            }
            Err(error) => {
                let last_error = unsafe { GetLastError().0 };
                if last_error != ERROR_PIPE_BUSY.0
                    && error.code().0 != win32_hresult(ERROR_PIPE_BUSY.0)
                {
                    return false;
                }
                unsafe {
                    let _ = WaitNamedPipeW(PCWSTR(pipe.as_ptr()), 200);
                }
            }
        }
    }
    false
}

/// Inject one validated shortcut through Windows' documented input queue.
/// The grammar is deliberately small: `Ctrl`, `Shift`, `Alt`, `Win` followed
/// by exactly one key (`A`-`Z`, `0`-`9`, `F1`-`F24`, or the named navigation
/// keys).  No text, shell command or executable path is accepted here.
pub fn send_shortcut(shortcut: &str) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
    };

    let (modifiers, key) = parse_shortcut(shortcut)?;
    let mut inputs = Vec::with_capacity((modifiers.len() + 1) * 2);
    for modifier in &modifiers {
        inputs.push(key_input(*modifier, false));
    }
    inputs.push(key_input(key, false));
    inputs.push(key_input(key, true));
    for modifier in modifiers.iter().rev() {
        inputs.push(key_input(*modifier, true));
    }

    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent == inputs.len() as u32 {
        return Ok(());
    } else {
        return Err(format!(
            "Windows 只注入了 {sent}/{} 个按键事件",
            inputs.len()
        ));
    }

    fn key_input(vk: u16, key_up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: if key_up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
}

fn parse_shortcut(shortcut: &str) -> Result<(Vec<u16>, u16), String> {
    let tokens = shortcut
        .split('+')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if tokens.is_empty() || tokens.len() > 5 {
        return Err("快捷键格式应为 Ctrl+Shift+F10".into());
    }

    let mut modifiers = Vec::new();
    let mut key = None;
    for token in tokens {
        let normalized = token.to_ascii_lowercase();
        let modifier = match normalized.as_str() {
            "ctrl" | "control" => Some(17),
            "shift" => Some(16),
            "alt" => Some(18),
            "win" | "windows" => Some(91),
            _ => None,
        };
        if let Some(modifier) = modifier {
            if modifiers.contains(&modifier) {
                return Err("快捷键包含重复修饰键".into());
            }
            modifiers.push(modifier);
            continue;
        }
        if key.is_some() {
            return Err("快捷键只能包含一个主按键".into());
        }
        key = Some(parse_main_key(&normalized)?);
    }
    let key = key.ok_or_else(|| "快捷键缺少主按键".to_string())?;
    Ok((modifiers, key))
}

fn parse_main_key(token: &str) -> Result<u16, String> {
    if token.len() == 1 {
        let byte = token.as_bytes()[0];
        if byte.is_ascii_alphanumeric() {
            return Ok(byte.to_ascii_uppercase() as u16);
        }
    }
    if let Some(number) = token
        .strip_prefix('f')
        .and_then(|value| value.parse::<u8>().ok())
        && (1..=24).contains(&number)
    {
        return Ok(111 + u16::from(number));
    }
    match token {
        "space" => Ok(32),
        "enter" | "return" => Ok(13),
        "esc" | "escape" => Ok(27),
        "tab" => Ok(9),
        "backspace" => Ok(8),
        "delete" | "del" => Ok(46),
        "insert" | "ins" => Ok(45),
        "home" => Ok(36),
        "end" => Ok(35),
        "pageup" | "pgup" => Ok(33),
        "pagedown" | "pgdn" => Ok(34),
        "up" => Ok(38),
        "down" => Ok(40),
        "left" => Ok(37),
        "right" => Ok(39),
        _ => Err(format!("不支持的主按键：{token}")),
    }
}

/// A running WMI-to-pipe bridge.  The WMI COM objects stay on their worker
/// thread (`WMIConnection` is intentionally !Send); the desktop only gets a
/// channel of fixed, validated events.
pub struct OmenKeyBridge {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl OmenKeyBridge {
    pub fn start(executable: &Path) -> Result<(Self, mpsc::Receiver<ResidentEvent>), String> {
        // The WMI consumer runs under LocalSystem.  A desktop/temp path
        // would let a standard user replace the binary and gain a SYSTEM
        // launch, so the privileged bridge is restricted to a canonical
        // Program Files installation.
        let executable = validate_resident_executable(executable)?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (event_tx, event_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        let join = thread::Builder::new()
            .name("omen-key-bridge".into())
            .spawn(move || {
                let setup = install_subscription(&executable).and_then(|subscription| {
                    let first_pipe = match create_named_pipe() {
                        Ok(pipe) => pipe,
                        Err(error) => {
                            subscription.remove();
                            return Err(error);
                        }
                    };
                    let _ = ready_tx.send(Ok(()));
                    run_pipe_loop(&worker_stop, &event_tx, first_pipe);
                    // A transient pipe recreation failure must not tear down
                    // the WMI subscription permanently.  Keep the same
                    // subscription and retry the server endpoint until stop.
                    while !worker_stop.load(Ordering::Acquire) {
                        thread::sleep(Duration::from_secs(1));
                        if worker_stop.load(Ordering::Acquire) {
                            break;
                        }
                        match create_named_pipe() {
                            Ok(pipe) => run_pipe_loop(&worker_stop, &event_tx, pipe),
                            Err(error) => {
                                tracing::warn!(%error, "retry create OMEN key pipe failed")
                            }
                        }
                    }
                    subscription.remove();
                    Ok(())
                });
                if let Err(error) = setup {
                    let _ = ready_tx.send(Err(error));
                    worker_stop.store(true, Ordering::Release);
                }
            })
            .map_err(|error| format!("启动 OMEN 键桥接线程失败：{error}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok((
                Self {
                    stop,
                    join: Some(join),
                },
                event_rx,
            )),
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = join.join();
                Err(error)
            }
            Err(error) => {
                stop.store(true, Ordering::Release);
                let _ = join.join();
                Err(format!("等待 OMEN 键桥接就绪超时：{error}"))
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for OmenKeyBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Subscription {
    connection: WMIConnection,
}

impl Subscription {
    fn remove(self) {
        if let Err(error) = remove_own_subscription(&self.connection) {
            tracing::warn!(%error, "remove phelper OMEN key subscription failed");
        }
        // Drop the WMI connection after deleting the permanent objects.
    }
}

fn install_subscription(executable: &Path) -> Result<Subscription, String> {
    let connection = WMIConnection::with_namespace_path(WMI_SUBSCRIPTION_ROOT)
        .map_err(|error| format!("打开 WMI 订阅命名空间失败：{error}"))?;

    reject_foreign_subscription(&connection)?;
    remove_own_subscription(&connection)?;

    let install_result = (|| {
        let consumer_class = connection
            .get_object("CommandLineEventConsumer")
            .map_err(|error| format!("读取 CommandLineEventConsumer 类失败：{error}"))?;
        let consumer = consumer_class
            .spawn_instance()
            .map_err(|error| format!("创建 OMEN 键 consumer 失败：{error}"))?;
        consumer
            .put_property("Name", CONSUMER_NAME)
            .and_then(|_| {
                consumer.put_property("ExecutablePath", executable.to_string_lossy().to_string())
            })
            .and_then(|_| consumer.put_property("CommandLineTemplate", SIGNAL_ARGUMENT))
            .and_then(|_| connection.put_instance(&consumer))
            .map_err(|error| format!("写入 OMEN 键 consumer 失败：{error}"))?;

        let filter_class = connection
            .get_object("__EventFilter")
            .map_err(|error| format!("读取 __EventFilter 类失败：{error}"))?;
        let filter = filter_class
            .spawn_instance()
            .map_err(|error| format!("创建 OMEN 键 filter 失败：{error}"))?;
        filter
            .put_property("Name", FILTER_NAME)
            .and_then(|_| filter.put_property("EventNameSpace", WMI_ROOT))
            .and_then(|_| filter.put_property("QueryLanguage", "WQL"))
            .and_then(|_| filter.put_property("Query", OMEN_EVENT_QUERY))
            .and_then(|_| connection.put_instance(&filter))
            .map_err(|error| format!("写入 OMEN 键 filter 失败：{error}"))?;

        let consumer_path =
            find_named_object(&connection, "CommandLineEventConsumer", CONSUMER_NAME)?
                .ok_or_else(|| "WMI consumer 写入后无法重新读取路径".to_string())?
                .path()
                .map_err(|error| format!("读取 OMEN 键 consumer 路径失败：{error}"))?;
        let filter_path = find_named_object(&connection, "__EventFilter", FILTER_NAME)?
            .ok_or_else(|| "WMI filter 写入后无法重新读取路径".to_string())?
            .path()
            .map_err(|error| format!("读取 OMEN 键 filter 路径失败：{error}"))?;

        let binding_class = connection
            .get_object("__FilterToConsumerBinding")
            .map_err(|error| format!("读取 __FilterToConsumerBinding 类失败：{error}"))?;
        let binding = binding_class
            .spawn_instance()
            .map_err(|error| format!("创建 OMEN 键 binding 失败：{error}"))?;
        binding
            .put_property("Filter", filter_path)
            .and_then(|_| binding.put_property("Consumer", consumer_path))
            .and_then(|_| connection.put_instance(&binding))
            .map_err(|error| format!("写入 OMEN 键 binding 失败：{error}"))?;

        Ok::<(), String>(())
    })();

    if let Err(error) = install_result {
        let _ = remove_own_subscription(&connection);
        return Err(error);
    }

    Ok(Subscription { connection })
}

fn reject_foreign_subscription(connection: &WMIConnection) -> Result<(), String> {
    let filters = query_objects(connection, "__EventFilter")?;
    for filter in filters {
        let namespace = property_string(&filter, "EventNameSpace").unwrap_or_default();
        let query = property_string(&filter, "Query").unwrap_or_default();
        if is_omen_candidate(&namespace, &query) {
            let name = property_string(&filter, "Name").unwrap_or_default();
            if !name.eq_ignore_ascii_case(FILTER_NAME) {
                return Err(format!(
                    "检测到已有 OMEN 键 WMI 订阅（{name}），为避免与其他工具争用，phelper 不会启用桥接"
                ));
            }
        }
    }
    Ok(())
}

fn remove_own_subscription(connection: &WMIConnection) -> Result<(), String> {
    let bindings = query_objects(connection, "__FilterToConsumerBinding")?;
    let mut binding_paths = Vec::new();
    for binding in bindings {
        let filter = property_string(&binding, "Filter").unwrap_or_default();
        let consumer = property_string(&binding, "Consumer").unwrap_or_default();
        let owns_filter = contains_name(&filter, FILTER_NAME);
        let owns_consumer = contains_name(&consumer, CONSUMER_NAME);
        if owns_filter || owns_consumer {
            if !(owns_filter && owns_consumer) {
                return Err("phelper 的 WMI 订阅资源处于不完整绑定状态，拒绝删除未知绑定".into());
            }
            if let Ok(path) = binding.path() {
                binding_paths.push(path);
            }
        }
    }

    for path in binding_paths {
        connection
            .delete_instance(&path)
            .map_err(|error| format!("删除 phelper OMEN 键 binding 失败：{error}"))?;
    }

    for (class, name) in [
        ("__EventFilter", FILTER_NAME),
        ("CommandLineEventConsumer", CONSUMER_NAME),
    ] {
        if let Some(object) = find_named_object(connection, class, name)? {
            let path = object
                .path()
                .map_err(|error| format!("读取 phelper WMI 资源路径失败：{error}"))?;
            connection
                .delete_instance(&path)
                .map_err(|error| format!("删除 phelper WMI 资源失败：{error}"))?;
        }
    }
    Ok(())
}

fn run_pipe_loop(stop: &AtomicBool, event_tx: &mpsc::Sender<ResidentEvent>, mut pipe: PipeHandle) {
    while !stop.load(Ordering::Acquire) {
        let connected = match wait_for_pipe_client(pipe.0) {
            Ok(connected) => connected,
            Err(error) => {
                tracing::warn!(%error, "OMEN key pipe connection failed");
                close_pipe(pipe);
                if stop.load(Ordering::Acquire) {
                    return;
                }
                match create_named_pipe() {
                    Ok(next) => {
                        pipe = next;
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "recreate OMEN key pipe failed");
                        return;
                    }
                }
            }
        };

        if !connected {
            // The non-blocking connect has not completed yet.
            thread::sleep(Duration::from_millis(20));
            continue;
        }

        if let Some(payload) = read_pipe_message(pipe.0, stop)
            && payload == SIGNAL
        {
            let _ = event_tx.send(ResidentEvent::OmenKeyPressed);
        }
        unsafe {
            let _ = windows::Win32::System::Pipes::DisconnectNamedPipe(pipe.0);
            let _ = windows::Win32::Foundation::CloseHandle(pipe.0);
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
        match create_named_pipe() {
            Ok(next) => pipe = next,
            Err(error) => {
                tracing::warn!(%error, "recreate OMEN key pipe failed");
                return;
            }
        }
    }
    close_pipe(pipe);
}

fn wait_for_pipe_client(handle: windows::Win32::Foundation::HANDLE) -> Result<bool, String> {
    use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, GetLastError};
    use windows::Win32::System::Pipes::ConnectNamedPipe;

    match unsafe { ConnectNamedPipe(handle, None) } {
        Ok(()) => Ok(true),
        Err(error) => {
            let code = unsafe { GetLastError().0 };
            if code == ERROR_PIPE_CONNECTED.0 {
                Ok(true)
            } else if code == ERROR_PIPE_LISTENING.0 {
                Ok(false)
            } else {
                Err(format!("ConnectNamedPipe failed ({code}): {error}"))
            }
        }
    }
}

fn read_pipe_message(
    handle: windows::Win32::Foundation::HANDLE,
    stop: &AtomicBool,
) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA, GetLastError};
    use windows::Win32::Storage::FileSystem::ReadFile;
    use windows::Win32::System::Pipes::PeekNamedPipe;

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
            return None;
        }
        let mut available = 0u32;
        if let Err(error) =
            unsafe { PeekNamedPipe(handle, None, 0, None, Some(&mut available), None) }
        {
            let code = unsafe { GetLastError().0 };
            if code == ERROR_NO_DATA.0 || code == ERROR_BROKEN_PIPE.0 {
                return None;
            }
            tracing::debug!(%error, code, "PeekNamedPipe failed");
            return None;
        }
        if available == 0 {
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        let mut buffer = vec![0u8; available.min(256) as usize];
        let mut read = 0u32;
        if unsafe { ReadFile(handle, Some(&mut buffer), Some(&mut read), None) }.is_ok() {
            buffer.truncate(read as usize);
            return Some(buffer);
        }
        return None;
    }
}

#[derive(Clone, Copy)]
struct PipeHandle(windows::Win32::Foundation::HANDLE);

fn create_named_pipe() -> Result<PipeHandle, String> {
    use windows::Win32::Foundation::{GetLastError, HLOCAL};
    use windows::Win32::Security::PSECURITY_DESCRIPTOR;
    use windows::Win32::Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, SECURITY_ATTRIBUTES,
    };
    use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_INBOUND};
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_NOWAIT, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_MESSAGE,
    };
    use windows::core::PCWSTR;

    let user_sid = current_user_sid()
        .map_err(|error| format!("读取当前用户 SID 失败，拒绝创建 OMEN 键 pipe：{error}"))?;
    // SYSTEM is required by the LocalSystem WMI event consumer.  The
    // interactive-users (IU) ACE is deliberately absent: it would expose
    // the signal endpoint to every logged-in user.  The current user gets
    // access for diagnostics/manual signaling only.
    let descriptor_text = wide(&format!("D:P(A;;GA;;;SY)(A;;GRGW;;;{user_sid})"));
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(descriptor_text.as_ptr()),
            1,
            &mut descriptor,
            None,
        )
        .map_err(|error| format!("创建 OMEN 键 pipe 安全描述符失败：{error}"))?;
    }

    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let name = wide(PIPE_NAME);
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            PIPE_ACCESS_INBOUND | FILE_FLAGS_AND_ATTRIBUTES(0),
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            256,
            256,
            0,
            Some(&attributes),
        )
    };
    unsafe {
        let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(descriptor.0)));
    }
    if handle.is_invalid() {
        let code = unsafe { GetLastError().0 };
        return Err(format!("创建 OMEN 键 pipe 失败（{code}）"));
    }
    Ok(PipeHandle(handle))
}

fn close_pipe(pipe: PipeHandle) {
    unsafe {
        let _ = windows::Win32::System::Pipes::DisconnectNamedPipe(pipe.0);
        let _ = windows::Win32::Foundation::CloseHandle(pipe.0);
    }
}

fn current_user_sid() -> Result<String, String> {
    use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::PWSTR;

    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|error| format!("OpenProcessToken: {error}"))?;

    let result = (|| {
        let mut required = 0u32;
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut required) };
        if required == 0 {
            let error = unsafe { GetLastError() };
            return Err(format!("GetTokenInformation(size): Win32 {}", error.0));
        }
        let mut buffer = vec![0u8; required as usize];
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                required,
                &mut required,
            )
        }
        .map_err(|error| format!("GetTokenInformation: {error}"))?;
        let user = unsafe { &*(buffer.as_ptr() as *const TOKEN_USER) };
        let mut sid_string = PWSTR::default();
        unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid_string) }
            .map_err(|error| format!("ConvertSidToStringSidW: {error}"))?;
        let result = unsafe { sid_string.to_string() }
            .map_err(|error| format!("SID 字符串转换失败：{error}"));
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(sid_string.0.cast())));
        }
        result
    })();
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

fn query_objects(
    connection: &WMIConnection,
    class: &str,
) -> Result<Vec<IWbemClassWrapper>, String> {
    let query = format!("SELECT * FROM {class}");
    let iterator = connection
        .exec_query(query)
        .map_err(|error| format!("查询 WMI {class} 失败：{error}"))?;
    iterator
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("读取 WMI {class} 结果失败：{error}"))
}

fn find_named_object(
    connection: &WMIConnection,
    class: &str,
    name: &str,
) -> Result<Option<IWbemClassWrapper>, String> {
    Ok(query_objects(connection, class)?
        .into_iter()
        .find(|object| {
            property_string(object, "Name")
                .is_ok_and(|candidate| candidate.eq_ignore_ascii_case(name))
        }))
}

fn property_string(object: &IWbemClassWrapper, name: &str) -> Result<String, String> {
    object
        .get_property(name)
        .map_err(|error| format!("读取 WMI {name} 失败：{error}"))?
        .try_into()
        .map_err(|error| format!("转换 WMI {name} 失败：{error:?}"))
}

fn classify_probe_error(error: &WMIError) -> OmenKeyCapability {
    match error {
        WMIError::HResultError { hres }
            if *hres == 0x8004_1002u32 as i32 || *hres == 0x8004_100eu32 as i32 =>
        {
            OmenKeyCapability::Unsupported
        }
        _ => OmenKeyCapability::Error,
    }
}

fn is_omen_candidate(namespace: &str, query: &str) -> bool {
    namespace.eq_ignore_ascii_case(WMI_ROOT)
        && query
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase()
            .contains("hpqbevnt")
        && normalized_query(query).contains("eventdata=8613")
        && normalized_query(query).contains("eventid=29")
}

fn normalized_query(query: &str) -> String {
    query
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn contains_name(value: &str, name: &str) -> bool {
    let value = value.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    value.contains(&format!("name='{name}'")) || value.contains(&format!("name=\"{name}\""))
}

fn has_property(properties: &[String], expected: &str) -> bool {
    properties
        .iter()
        .any(|property| property.eq_ignore_ascii_case(expected))
}

fn validate_executable(executable: &Path) -> Result<PathBuf, String> {
    if !executable.is_absolute() {
        return Err(format!(
            "可执行文件路径必须是绝对路径：{}",
            executable.display()
        ));
    }
    if !executable.is_file() {
        return Err(format!("可执行文件不存在：{}", executable.display()));
    }
    let path = executable
        .to_str()
        .ok_or_else(|| "可执行文件路径不是有效 Unicode".to_string())?;
    if path.contains('"') {
        return Err("可执行文件路径包含不允许的引号".into());
    }
    std::fs::canonicalize(executable).map_err(|error| format!("解析可执行文件路径失败：{error}"))
}

fn validate_resident_executable(executable: &Path) -> Result<PathBuf, String> {
    let executable = validate_executable(executable)?;
    let roots = ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect::<Vec<_>>();
    let path = executable.to_string_lossy().to_ascii_lowercase();
    let under_program_files = roots.iter().any(|root| {
        let root = root.to_string_lossy().to_ascii_lowercase();
        path == root || path.starts_with(&format!("{root}\\"))
    });
    if !under_program_files {
        return Err(format!(
            "OMEN 键桥接要求程序安装在 Program Files 下（当前路径：{}）",
            executable.display()
        ));
    }
    Ok(executable)
}

fn schtasks_path() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .map(|root| root.join("System32").join("schtasks.exe"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("schtasks.exe"))
}

fn task_missing(output: &std::process::Output) -> bool {
    let text = command_output(output).to_ascii_lowercase();
    let code = output.status.code().unwrap_or_default();
    code == ERROR_FILE_NOT_FOUND_CODE as i32
        || code == ERROR_PATH_NOT_FOUND_CODE as i32
        || text.contains("does not exist")
        || text.contains("cannot find")
        || text.contains("not found")
        || text.contains("不存在")
        || text.contains("找不到")
}

fn command_output(output: &std::process::Output) -> String {
    let stdout = decode_command_bytes(&output.stdout);
    let stderr = decode_command_bytes(&output.stderr);
    let text = format!("{stdout}{stderr}").trim().to_string();
    if text.is_empty() {
        "无附加信息".into()
    } else {
        text
    }
}

/// `schtasks.exe` uses the active Windows console code page when its output
/// is redirected.  On a Chinese Windows installation that is commonly GBK,
/// not UTF-8; decoding it as UTF-8 turns useful diagnostics into `�` and can
/// also hide the localized "task not found" marker used by `task_missing`.
fn decode_command_bytes(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) && bytes.len() % 2 == 0 {
        let units = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .take_while(|unit| *unit != 0)
            .collect::<Vec<_>>();
        if let Ok(text) = String::from_utf16(&units) {
            return text;
        }
    }
    if let Ok(text) = std::str::from_utf8(bytes)
        && !text.contains('\0')
        && !text.contains('\u{FFFD}')
    {
        return text.to_string();
    }

    use windows::Win32::Globalization::{CP_ACP, CP_OEMCP};

    [CP_OEMCP, CP_ACP]
        .into_iter()
        .filter_map(|code_page| decode_code_page(code_page, bytes))
        .min_by_key(|text| text.chars().filter(|c| *c == '\u{FFFD}').count())
        .unwrap_or_else(|| format!("Windows 输出不可解码（{} 字节）", bytes.len()))
}

fn decode_code_page(code_page: u32, bytes: &[u8]) -> Option<String> {
    use windows::Win32::Globalization::MultiByteToWideChar;

    if bytes.is_empty() {
        return Some(String::new());
    }
    let flags = Default::default();
    let required = unsafe { MultiByteToWideChar(code_page, flags, bytes, None) };
    if required <= 0 {
        return None;
    }
    let mut wide = vec![0u16; required as usize];
    let written = unsafe { MultiByteToWideChar(code_page, flags, bytes, Some(&mut wide)) };
    if written <= 0 {
        return None;
    }
    wide.truncate(written as usize);
    String::from_utf16(&wide).ok()
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn win32_hresult(code: u32) -> i32 {
    (0x8007_0000u32 | code) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::process::ExitStatusExt;

    #[test]
    fn candidate_query_is_strict() {
        assert!(is_omen_candidate(
            "root\\wmi",
            "SELECT * FROM hpqBEvnt WHERE EventData = 8613 AND EventID = 29"
        ));
        assert!(!is_omen_candidate(
            "root\\wmi",
            "SELECT * FROM hpqBEvnt WHERE EventData = 8613 AND EventID = 30"
        ));
        assert!(!is_omen_candidate(
            "root\\cimv2",
            "SELECT * FROM hpqBEvnt WHERE EventData = 8613 AND EventID = 29"
        ));
    }

    #[test]
    fn shortcut_parser_requires_one_main_key() {
        assert_eq!(
            parse_shortcut("Ctrl+Shift+F10").unwrap(),
            (vec![17, 16], 121)
        );
        assert!(parse_shortcut("Ctrl+Shift").is_err());
        assert!(parse_shortcut("Ctrl+A+B").is_err());
        assert!(parse_shortcut("Run+cmd").is_err());
    }

    #[test]
    fn task_missing_does_not_treat_access_denied_as_missing() {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(5),
            stdout: Vec::new(),
            stderr: b"Access is denied".to_vec(),
        };
        assert!(!task_missing(&output));
    }

    #[test]
    fn autostart_uses_background_mode_not_signal_only_mode() {
        let run = autostart_task_run(Path::new(r"C:\phelper\phelper-desktop.exe"));
        assert!(run.ends_with("--background"));
        assert!(!run.contains(SIGNAL_ARGUMENT));
    }

    #[test]
    fn utf8_command_output_stays_utf8() {
        let bytes = "任务不存在".as_bytes();
        assert_eq!(decode_command_bytes(bytes), "任务不存在");
    }
}
