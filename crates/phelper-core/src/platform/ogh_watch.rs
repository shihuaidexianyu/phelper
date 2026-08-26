//! OGH / second-writer detection (§33.1 supplement; task #9). The engine
//! scans at startup and WARNS — it never kills processes and never blocks
//! startup. A second writer to the HP WMI surface (OMEN Gaming Hub above
//! all) would race our single-writer invariant (AR-03) from OUTSIDE the
//! process; the user deserves a loud note, not silent coexistence.
//!
//! Three detection lanes:
//!   1. Win32_Process — known writer executables by image name.
//!   2. Win32_Service — HP services; the KNOWN-PASSIVE list (§33.1 daily-
//!      check conclusion) is reported as informational only.
//!   3. Appx package registry — the OGH store package being installed at
//!      all (not necessarily running).
//!
//! Known-writer vs known-passive lists are deliberately explicit: anything
//! not on either list is still just a process, never a finding.

use serde::Deserialize;
use tracing::warn;
use wmi::WMIConnection;

use super::wmi_util::query_typed;

/// What kind of thing was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OghFindingKind {
    /// Actively running process that can write the HP WMI surface.
    RunningWriter,
    /// Installed (not necessarily running) OGH Appx package.
    InstalledPackage,
    /// HP service present but known-passive (informational).
    PassiveService,
}

#[derive(Debug, Clone)]
pub struct OghFinding {
    pub kind: OghFindingKind,
    pub name: String,
    pub detail: String,
}

impl std::fmt::Display for OghFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self.kind {
            OghFindingKind::RunningWriter => "RUNNING-WRITER",
            OghFindingKind::InstalledPackage => "installed-package",
            OghFindingKind::PassiveService => "passive-service",
        };
        write!(f, "[{kind}] {} — {}", self.name, self.detail)
    }
}

/// Executables that write to the HP WMI gaming surface (OGH + its
/// background helper). Case-insensitive match on image name.
const KNOWN_WRITER_PROCESSES: &[&str] = &[
    "omencommandcenterbackground.exe",
    "omencommandcenter.exe",
    "omengaminghub.exe",
];

/// HP services verified passive on 8BAB (§33.1 daily-check): they serve
/// diagnostics/capability WMI, not the gaming control surface. Reported as
/// informational so the user knows we looked.
const KNOWN_PASSIVE_SERVICES: &[&str] = &[
    "hpomencap",
    "hpapphelpercap",
    "hpdiagscap",
    "hpnetworkcap",
    "hpsysinfocap",
    "hpqcaslwmiex",
];

/// Appx package family name fragment for OMEN Gaming Hub.
const OGH_APPX_FRAGMENT: &str = "AD2F1837.OMENGamingHub";

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Win32ProcessRow {
    name: Option<String>,
    process_id: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Win32ServiceRow {
    name: Option<String>,
    state: Option<String>,
    start_mode: Option<String>,
}

fn scan_processes(conn: &WMIConnection, out: &mut Vec<OghFinding>) {
    let rows: Vec<Win32ProcessRow> = match query_typed(
        conn,
        "ogh-watch",
        "SELECT Name, ProcessId FROM Win32_Process",
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(%e, "ogh-watch: process scan failed");
            return;
        }
    };
    for row in rows {
        let Some(name) = row.name else { continue };
        if KNOWN_WRITER_PROCESSES.contains(&name.to_ascii_lowercase().as_str()) {
            out.push(OghFinding {
                kind: OghFindingKind::RunningWriter,
                name: name.clone(),
                detail: format!(
                    "pid {} — second writer on the HP WMI surface; expect clawback fights",
                    row.process_id.unwrap_or(0)
                ),
            });
        }
    }
}

fn scan_services(conn: &WMIConnection, out: &mut Vec<OghFinding>) {
    let rows: Vec<Win32ServiceRow> = match query_typed(
        conn,
        "ogh-watch",
        "SELECT Name, State, StartMode FROM Win32_Service",
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(%e, "ogh-watch: service scan failed");
            return;
        }
    };
    for row in rows {
        let Some(name) = row.name else { continue };
        if KNOWN_PASSIVE_SERVICES.contains(&name.to_ascii_lowercase().as_str()) {
            out.push(OghFinding {
                kind: OghFindingKind::PassiveService,
                name: name.clone(),
                detail: format!(
                    "state={} start={} — known-passive (diagnostics/capability only)",
                    row.state.as_deref().unwrap_or("?"),
                    row.start_mode.as_deref().unwrap_or("?")
                ),
            });
        }
    }
}

/// Appx detection via the registry (PACKAGE repository). Falls back to a
/// WindowsApps directory listing when the registry read fails.
fn scan_appx(out: &mut Vec<OghFinding>) {
    use windows::Win32::System::Registry::{
        HKEY, KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
    };
    // Per-user package repository: SOFTWARE\Classes\Local Settings\Software\
    // Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages (HKCU).
    // HKLM equivalent covers system-staged packages. Either is sufficient
    // evidence of "OGH is installed".
    let subkey: Vec<u16> = "SOFTWARE\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppModel\\Repository\\Packages\0"
        .encode_utf16()
        .collect();
    unsafe {
        let mut key = HKEY::default();
        // HKCU first (store apps install per-user).
        let status = RegOpenKeyExW(
            windows::Win32::System::Registry::HKEY_CURRENT_USER,
            windows::core::PCWSTR(subkey.as_ptr()),
            Some(0),
            KEY_READ,
            &mut key,
        );
        if status.is_ok() {
            let mut index = 0u32;
            loop {
                let mut name_buf = [0u16; 256];
                let mut name_len = name_buf.len() as u32;
                let enum_status = RegEnumKeyExW(
                    key,
                    index,
                    Some(windows::core::PWSTR(name_buf.as_mut_ptr())),
                    &mut name_len,
                    None,
                    None,
                    None,
                    None,
                );
                if !enum_status.is_ok() {
                    break;
                }
                index += 1;
                let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
                if name.contains(OGH_APPX_FRAGMENT) {
                    out.push(OghFinding {
                        kind: OghFindingKind::InstalledPackage,
                        name: name.clone(),
                        detail: "OGH Appx package installed (may not be running)".into(),
                    });
                }
            }
            let _ = RegCloseKey(key);
            return;
        }
    }

    // Fallback: WindowsApps directory listing.
    let apps_dir = std::env::var("ProgramFiles")
        .map(|p| format!("{p}\\WindowsApps"))
        .unwrap_or_else(|_| "C:\\Program Files\\WindowsApps".into());
    if let Ok(read) = std::fs::read_dir(&apps_dir) {
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains(OGH_APPX_FRAGMENT) {
                out.push(OghFinding {
                    kind: OghFindingKind::InstalledPackage,
                    name,
                    detail: "OGH Appx folder present under WindowsApps".into(),
                });
            }
        }
    }
}

/// One full scan. Never fails the engine: every lane degrades to a warn.
pub fn scan() -> Vec<OghFinding> {
    let mut out = Vec::new();
    match WMIConnection::new() {
        Ok(conn) => {
            scan_processes(&conn, &mut out);
            scan_services(&conn, &mut out);
        }
        Err(e) => warn!(%e, "ogh-watch: cimv2 connect failed — process/service lanes skipped"),
    }
    scan_appx(&mut out);
    for f in &out {
        match f.kind {
            OghFindingKind::RunningWriter => warn!(%f, "SECOND WRITER DETECTED"),
            _ => warn!(%f, "ogh-watch finding"),
        }
    }
    out
}
