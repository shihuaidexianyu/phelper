//! Presence of a named kernel object is the cross-process writer lease.
//! No thread-owned mutex acquisition: the handle can move with Engine.
use phelper_domain::error::EngineError;

#[cfg(windows)]
pub(crate) struct ControlLease(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl ControlLease {
    pub(crate) fn acquire() -> Result<Self, EngineError> {
        // Older desktop builds predate this lease. Conservatively detect
        // them during migration, including an elevated image whose command
        // line the current token cannot read. Never terminate another app.
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Process {
            process_id: u32,
            command_line: Option<String>,
        }
        let wmi = wmi::WMIConnection::new()
            .map_err(|e| EngineError::Config(format!("writer scan unavailable: {e}")))?;
        let processes: Vec<Process> = wmi.raw_query("SELECT ProcessId, CommandLine FROM Win32_Process WHERE Name = 'phelper-desktop.exe'")
            .map_err(|e| EngineError::Config(format!("writer scan unavailable: {e}")))?;
        if let Some(other) = processes.iter().find(|p| {
            p.process_id != std::process::id()
                && !p.command_line.as_deref().is_some_and(|c| {
                    c.split_whitespace()
                        .any(|arg| arg.trim_matches('"') == "--read-only")
                })
        }) {
            return Err(EngineError::Config(format!(
                "另一个 phelper 桌面进程 ({}) 正在运行，请先正常退出该实例",
                other.process_id
            )));
        }
        Self::acquire_named(windows::core::w!("Global\\Phelper.HardwareControl.8BAB.v1"))
    }

    fn acquire_named(name: windows::core::PCWSTR) -> Result<Self, EngineError> {
        use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows::Win32::System::Threading::CreateMutexW;
        unsafe {
            let handle = CreateMutexW(None, false, name).map_err(|e| {
                EngineError::Config(format!("hardware writer lease unavailable: {e}"))
            })?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                return Err(EngineError::Config(
                    "another phelper process owns hardware control".into(),
                ));
            }
            Ok(Self(handle))
        }
    }
}

#[cfg(windows)]
impl Drop for ControlLease {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(not(windows))]
pub(crate) struct ControlLease;
#[cfg(not(windows))]
impl ControlLease {
    pub(crate) fn acquire() -> Result<Self, EngineError> {
        Err(EngineError::Config(
            "hardware control requires Windows".into(),
        ))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn writer_lease_excludes_second_owner_and_releases_on_drop() {
        // Production name deliberately not used in tests: do not interfere
        // with a running desktop or operator-controlled hardware session.
        use windows::core::PCWSTR;
        let name: Vec<u16> = format!("Local\\Phelper.Lease.Test.{}", std::process::id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let name = PCWSTR(name.as_ptr());
        let first = ControlLease::acquire_named(name).unwrap();
        assert!(ControlLease::acquire_named(name).is_err());
        drop(first);
        assert!(ControlLease::acquire_named(name).is_ok());
    }
}
