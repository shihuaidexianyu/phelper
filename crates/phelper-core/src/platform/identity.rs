//! Device identity via standard CIM (root\cimv2). Probe step 1.

use phelper_domain::error::EngineError;
use phelper_domain::identity::{CpuIdentity, DeviceIdentity, GpuIdentity};
use serde::Deserialize;
use wmi::WMIConnection;

use super::wmi_util::query_typed;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Win32BaseBoard {
    #[serde(rename = "Manufacturer")]
    _manufacturer: Option<String>,
    product: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Win32Bios {
    #[serde(rename = "SMBIOSBIOSVersion")]
    smbios_bios_version: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Win32ComputerSystem {
    manufacturer: Option<String>,
    model: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Win32Processor {
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Win32VideoController {
    name: Option<String>,
}

fn trim(s: Option<String>) -> String {
    s.unwrap_or_default().trim().to_string()
}

/// Gather the machine identity. Board ID (BaseBoard Product) is THE
/// capability key — HP recycles product names, never match on those
/// (feasibility §1).
pub(crate) fn probe_identity() -> Result<DeviceIdentity, EngineError> {
    let conn = WMIConnection::new()
        .map_err(|e| EngineError::IdentityFailed(format!("connect cimv2: {e}")))?;

    let board: Vec<Win32BaseBoard> =
        query_typed(&conn, "identity", "SELECT Manufacturer, Product FROM Win32_BaseBoard")?;
    let board = board
        .into_iter()
        .next()
        .ok_or_else(|| EngineError::IdentityFailed("no Win32_BaseBoard".into()))?;

    let bios: Vec<Win32Bios> =
        query_typed(&conn, "identity", "SELECT SMBIOSBIOSVersion FROM Win32_BIOS")?;
    let bios_version = bios
        .into_iter()
        .next()
        .and_then(|b| b.smbios_bios_version)
        .unwrap_or_default();

    let cs: Vec<Win32ComputerSystem> = query_typed(
        &conn,
        "identity",
        "SELECT Manufacturer, Model FROM Win32_ComputerSystem",
    )?;
    let cs = cs
        .into_iter()
        .next()
        .ok_or_else(|| EngineError::IdentityFailed("no Win32_ComputerSystem".into()))?;

    let cpu: Vec<Win32Processor> =
        query_typed(&conn, "identity", "SELECT Name FROM Win32_Processor")?;
    let cpu_name = cpu
        .into_iter()
        .next()
        .and_then(|c| c.name)
        .unwrap_or_default();

    let gpus: Vec<Win32VideoController> =
        query_typed(&conn, "identity", "SELECT Name FROM Win32_VideoController")?;

    Ok(DeviceIdentity {
        manufacturer: trim(cs.manufacturer),
        product_name: trim(cs.model),
        board_id: trim(board.product),
        bios_version: bios_version.trim().to_string(),
        cpu: CpuIdentity {
            name: cpu_name.trim().to_string(),
        },
        gpu: gpus
            .into_iter()
            .filter_map(|g| g.name)
            .map(|n| GpuIdentity {
                name: n.trim().to_string(),
            })
            .collect(),
    })
}
