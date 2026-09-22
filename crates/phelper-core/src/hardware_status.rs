//! Read-only prerequisites, separate from a validated voltage/OC backend.
//! Finding an OEM DLL is not permission or proof that its ABI is callable.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
        // 2026-09-22 实测结论（hpqBIntM Gaming 0x20008 与 LegacyRead 0x1 两
        // 组 commandtype 0x00–0x5F 只读全扫，证据见 ogh-milestones.md 当日
        // 记录）：
        // - 电池充电上限：两组命令空间均无阈值接口；Linux 主线 hp-wmi 亦无
        //   实现（TLP 电池阈值支持列表不含 HP）→ 本机固件不支持，已从
        //   "未确定"升级为"已探测不存在"。
        // - 内存超频：同一扫描无任何内存相关命令 → 本机固件未暴露。
        // - CPU 降压/超频：XTU3SERVICE 与 HPOmenCap 在运行（OEM 驱动栈在
        //   位），但 VBS 正在运行，Intel 文档将其列为运行时降压限制条件 →
        //   后端存在、可用性受阻，仍属 §57 五阶段调研范围。
        // 附带发现：LegacyRead 0x07 电池信息查询真实可用（电芯电压/组电压
        // 12.756V/序列号 1963 与 powercfg 报告对照一致，0x10 返回制造日期
        // 20230226）——登记 backlog，完整字段解码需另行交叉验证。
        result.notes.push("电池充电上限与内存超频：本机固件未暴露接口（hpqBIntM 两组命令空间 0x00–0x5F 已实测）。CPU 降压/超频：XTU 服务在运行，但本机 VBS 已激活，Intel 将其列为降压限制条件；待 v0.4.0 立项。".into());
        result
    }
}
