use serde::{Deserialize, Serialize};

/// What this machine is. Gathered once at startup (probe step 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub manufacturer: String,
    /// DMI product name, e.g. "OMEN by HP Gaming Laptop 16-wf0xxx".
    pub product_name: String,
    /// BaseBoard Product, e.g. "8BAB". This is the capability key — never
    /// match on product_name (HP recycles names/IDs across regions).
    pub board_id: String,
    pub bios_version: String,
    pub cpu: CpuIdentity,
    pub gpu: Vec<GpuIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuIdentity {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuIdentity {
    pub name: String,
}
