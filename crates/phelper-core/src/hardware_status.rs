//! Read-only prerequisites, separate from a validated voltage/OC backend.
//! Finding an OEM DLL is not permission or proof that its ABI is callable.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HardwareStatus {
    pub xtu_service_running: Option<bool>,
    pub oem_intel_sdk: Option<String>,
    pub vbs_status: Option<u32>,
    pub undervolt_backend: bool,
    pub cpu_overclock_backend: bool,
    pub memory_overclock_backend: bool,
    pub charge_limit_backend: bool,
    pub notes: Vec<String>,
}

impl HardwareStatus {
    pub fn probe() -> Self {
        let mut result = Self::default();
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Service {
            name: String,
            state: String,
            path_name: Option<String>,
        }
        if let Ok(wmi) = wmi::WMIConnection::new() {
            match wmi.raw_query::<Service>("SELECT Name, State, PathName FROM Win32_Service WHERE Name = 'XTU3SERVICE' OR Name = 'HPOmenCap'") {
                Ok(services) => {
                    result.xtu_service_running = Some(services.iter().any(|s| s.name.eq_ignore_ascii_case("XTU3SERVICE") && s.state == "Running"));
                    if let Some(path) = services.iter().find(|s| s.name == "HPOmenCap").and_then(|s| s.path_name.as_deref()) {
                        let executable = path.trim_matches('"');
                        if let Some(parent) = std::path::Path::new(executable).parent() {
                            let sdk = parent.join("IntelOverclockingSDK.dll");
                            if sdk.is_file() { result.oem_intel_sdk = Some(sdk.display().to_string()); }
                        }
                    }
                }
                Err(e) => result.notes.push(format!("服务状态无法读取：{e}")),
            }
        }
        #[derive(Deserialize)]
        struct DeviceGuard {
            #[serde(rename = "VirtualizationBasedSecurityStatus")]
            status: u32,
        }
        if let Ok(wmi) =
            wmi::WMIConnection::with_namespace_path("root\\Microsoft\\Windows\\DeviceGuard")
        {
            result.vbs_status = wmi
                .raw_query::<DeviceGuard>(
                    "SELECT VirtualizationBasedSecurityStatus FROM Win32_DeviceGuard",
                )
                .ok()
                .and_then(|rows| rows.into_iter().next())
                .map(|row| row.status);
        }
        result.notes.push("UVP 与电压偏移尚无经过验证的读取接口；降压、CPU/内存超频与充电上限的硬件支持均未确定。".into());
        result
    }
}
