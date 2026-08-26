//! HP-platform-specific data types returned by the HpPlatform port.
//! Wire encoding/decoding lives in phelper-core (platform/hp_wmi/commands.rs);
//! these are the parsed forms the rest of the world sees.

use serde::{Deserialize, Serialize};

/// 0x28 System Design Data. Raw bytes are retained (diagnostics + future
/// bit interpretation); derived fields are the community/kernel-cross-checked
/// ones only — undocumented bits stay raw (architecture.md section 24).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemDesignData {
    #[serde(with = "hex_bytes")]
    pub raw: Vec<u8>,
    /// byte 3: thermal policy version (OMEN detection path; 8BAB is
    /// statically V1 so this is a consistency check, not a selector).
    pub thermal_policy_version: u8,
    /// byte 4 bit 0: firmware declares software fan control support.
    pub sw_fan_control: bool,
    /// byte 5: default PL4 in watts (community-observed; informational).
    pub default_pl4_w: u8,
    /// byte 7: MUX capability byte.
    pub mux_byte: u8,
    /// byte 7 bit 3: "graphics switcher supported" (OmenHwCtl semantics).
    pub mux_supported: bool,
}

/// One 0x2F fan-table entry. Levels are in the board's scale unit
/// (V1: 100-RPM units); noise in dB as reported by firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanTableEntry {
    pub cpu: u8,
    pub gpu: u8,
    pub noise_db: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanTable {
    pub num_fans: u8,
    pub entries: Vec<FanTableEntry>,
    #[serde(with = "hex_bytes")]
    pub raw: Vec<u8>,
}

impl FanTable {
    /// Clamp range for manual fan levels, derived from the table.
    /// Returns None when the table looks implausible (caller falls back to
    /// the BoardProfile clamp — fail closed).
    pub fn clamp_range(&self) -> Option<(u16, u16)> {
        if self.entries.is_empty() {
            return None;
        }
        let mut lo = u16::MAX;
        let mut hi = 0u16;
        for e in &self.entries {
            for v in [e.cpu as u16, e.gpu as u16] {
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        // Sanity: V1 levels are 0..~60 (0..6000 RPM). Anything outside means
        // our layout guess is wrong — refuse to trust it.
        if hi == 0 || hi > 100 || lo >= hi {
            return None;
        }
        Some((lo, hi))
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() % 2 != 0 {
            return Err(serde::de::Error::custom("hex string must have even length"));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect()
    }
}
