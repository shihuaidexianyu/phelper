//! Process token elevation check + self-elevation relaunch. PowrProf/HP WMI
//! writes require elevation; the engine is elevation-AWARE (degrades
//! gracefully), the CLI stays asInvoker (the operator elevates the shell),
//! and the desktop shell self-elevates at startup via [`relaunch_elevated`]
//! (M6: gpui.lib force-embeds its own asInvoker RT_MANIFEST — Windows allows
//! exactly one application manifest resource, so a static requireAdministrator
//! manifest cannot coexist with gpui; runas relaunch achieves the same
//! UAC-at-launch outcome without forking the dependency).

use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        );
        let _ = CloseHandle(token);
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

/// Outcome of [`relaunch_elevated`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relaunch {
    /// The elevated instance was launched; THIS process should exit now.
    Launched,
    /// The user declined the UAC prompt (or Windows denied the verb).
    Declined,
    /// The relaunch itself failed (code from ShellExecuteW, or no exe path).
    Failed(String),
}

/// Re-launch the current executable with the `runas` verb (UAC prompt),
/// forwarding the current command-line arguments and setting the working
/// directory to the executable's directory (ShellExecute defaults to
/// system32 otherwise). The caller exits on [`Relaunch::Launched`].
pub fn relaunch_elevated() -> Relaunch {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::core::PCWSTR;

    let Ok(exe) = std::env::current_exe() else {
        return Relaunch::Failed("current_exe unavailable".into());
    };
    let dir: PathBuf = exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    // Skip argv[0]; quote every argument (ShellExecute reparses the line).
    let params = std::env::args()
        .skip(1)
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(" ");

    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let file: Vec<u16> = exe
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let args: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();
    let cwd: Vec<u16> = dir
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let rc = ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(args.as_ptr()),
            PCWSTR(cwd.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        );
        // ShellExecuteW returns an HINSTANCE: >32 = success, else an error
        // code (SE_ERR_ACCESSDENIED = 5, ERROR_CANCELLED = 1223 = UAC said no).
        let code = rc.0 as isize;
        if code > 32 {
            Relaunch::Launched
        } else if code == 5 || code == 1223 {
            Relaunch::Declined
        } else {
            Relaunch::Failed(format!("ShellExecuteW code {code}"))
        }
    }
}
