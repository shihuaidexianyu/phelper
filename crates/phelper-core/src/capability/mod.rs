//! Capability discovery (AR-05: capabilities are discovered, never assumed;
//! AR-06: unknown means unsupported).
//!
//! Probe order follows docs/feasibility-16-wf0032TX.md §6. Every step is
//! failure-tolerant: a failed probe downgrades that domain's Support and
//! adds a note, it never aborts the run. Probe results may only DOWNGRADE
//! the BoardProfile ceiling, never upgrade it.

pub mod board_draft;
pub mod snapshot;

use phelper_domain::board::BoardProfile;
use phelper_domain::capability::{CapabilitySet, FanScale, Support};
use phelper_domain::error::EngineError;
use phelper_domain::hp::{FanTable, SystemDesignData};
use phelper_domain::identity::DeviceIdentity;
use phelper_domain::policy::{FanLevels, GpuPlatformPolicy, MuxMode};
use phelper_domain::ports::HpPlatform;
use tracing::{info, warn};

use crate::platform::hp_wmi::HpWmiTransport;
use crate::platform::{elevation, identity, windows_ppm};

/// Embedded developer-maintained board profiles (persistence layer may add
/// user overrides later; an override can never RAISE a support level).
const BOARD_8BAB_TOML: &str = include_str!("../../boards/8bab.toml");

pub fn load_board_profile(board_id: &str) -> Option<BoardProfile> {
    match board_id {
        "8BAB" => toml::from_str(BOARD_8BAB_TOML).ok(),
        _ => None,
    }
}

/// Rank for ceiling comparison. Probe may only lower the ceiling.
fn rank(s: Support) -> u8 {
    match s {
        Support::Supported => 3,
        Support::Experimental => 2,
        Support::Unsupported => 1,
        Support::NotProbed => 0,
    }
}

/// merged = min(profile_ceiling, probe_observed).
fn merge(profile: Support, probe: Support) -> Support {
    if rank(probe) < rank(profile) {
        probe
    } else {
        profile
    }
}

/// Everything one probe run learned. Serde-serializable: this doubles as
/// the capability snapshot JSON (§35/§49).
#[derive(Debug, serde::Serialize)]
pub struct ProbeReport {
    pub schema_version: u32,
    pub taken_at_epoch_ms: u64,
    pub identity: DeviceIdentity,
    pub known_board: bool,
    pub elevated: bool,
    pub capabilities: CapabilitySet,
    /// Raw probe artifacts kept for diagnostics (SDD/fan table carry their
    /// own raw bytes).
    pub sdd: Option<SystemDesignData>,
    pub fan_table: Option<FanTable>,
    pub fan_levels: Option<FanLevels>,
    pub gpu_platform_policy: Option<GpuPlatformPolicy>,
    pub mux: Option<MuxMode>,
    /// 0x26 diagnostics value — explicitly NOT a capability input.
    pub max_fan_diag: Option<bool>,
    /// Current EPP from the active power scheme (AC/DC percent).
    pub epp_ac: Option<u8>,
    pub epp_dc: Option<u8>,
    /// Active scheme's frequency ceiling (MHz; 0 = unlimited).
    pub max_freq_mhz: Option<u32>,
    pub notes: Vec<String>,
}

pub struct CapabilityService;

impl CapabilityService {
    /// Full probe via the CLI path: probes identity itself and opens its
    /// OWN transient HP transport. The engine must NOT use this — it calls
    /// `probe_runtime` through the already-running HpActor instead, so all
    /// firmware traffic keeps exactly one serialization point (R1).
    pub fn probe() -> Result<ProbeReport, EngineError> {
        // Step 1 — identity. Failure here means basic WMI is broken; abort.
        let identity = identity::probe_identity()?;
        info!(board = %identity.board_id, bios = %identity.bios_version, "identity probed");

        // Step 2 — board profile. Unknown board → read-only diagnostics.
        let profile = load_board_profile(&identity.board_id);

        let (hp, connect_err) = match HpWmiTransport::connect() {
            Ok(t) => (Some(t), None),
            Err(e) => (None, Some(e)),
        };
        let mut report =
            Self::probe_runtime(identity, profile.as_ref(), hp.as_ref().map(|t| t as _));
        if let Some(e) = connect_err {
            let msg = format!("HP WMI transport unavailable: {e}");
            report.capabilities.notes.push(msg.clone());
            report.notes.push(msg);
        }
        Ok(report)
    }

    /// Shared probe body (read-only by construction — only `HpPlatform`
    /// reads). `hp = None` downgrades every HP domain (fail closed, AR-06).
    pub fn probe_runtime(
        identity: DeviceIdentity,
        profile: Option<&BoardProfile>,
        hp: Option<&dyn HpPlatform>,
    ) -> ProbeReport {
        let mut notes: Vec<String> = Vec::new();

        let known_board = profile.is_some();
        if !known_board {
            warn!(board = %identity.board_id, "unknown board — engine runs read-only diagnostics");
            notes.push(format!(
                "board '{}' has no profile: all write capabilities forced Unsupported",
                identity.board_id
            ));
        }

        let mut caps = CapabilitySet {
            known_board,
            ..Default::default()
        };

        // Ceiling from profile (only meaningful on a known board).
        if let Some(p) = profile {
            caps.thermal_mode = Support::Supported;
            caps.fan_rpm_read = Support::Supported;
            caps.fan_manual_level = Support::Supported;
            caps.max_fan = Support::Supported;
            caps.gpu_platform_policy = if p.hp.supports_gpu_power_mode {
                Support::Supported
            } else {
                Support::Unsupported
            };
            caps.mux = if p.hp.supports_mux {
                Support::Supported
            } else {
                Support::Unsupported
            };
            caps.power_limits = p.hp.power_limits;
            caps.fan.count = p.fan.count;
            caps.fan.scale = p.fan.scale;
            caps.fan.clamp_min = p.fan.clamp_min;
            caps.fan.clamp_max = p.fan.clamp_max;
        } else {
            caps.thermal_mode = Support::Unsupported;
            caps.fan_rpm_read = Support::Unsupported;
            caps.fan_manual_level = Support::Unsupported;
            caps.max_fan = Support::Unsupported;
            caps.gpu_platform_policy = Support::Unsupported;
            caps.mux = Support::Unsupported;
            caps.power_limits = Support::Unsupported;
        }

        let elevated = elevation::is_elevated();
        caps.ppm.write_privileged = elevated;
        if !elevated {
            notes.push("process is not elevated: PowrProf writes will be denied".into());
        }

        // PPM reads (PowrProf). Unprivileged reads; write capability is
        // gated on the elevated token above.
        let mut epp_ac = None;
        let mut epp_dc = None;
        let mut max_freq_mhz = None;
        match windows_ppm::read_epp() {
            Ok(epp) => {
                caps.ppm.epp = Support::Supported;
                epp_ac = Some(epp.ac);
                epp_dc = Some(epp.dc);
            }
            Err(e) => {
                caps.ppm.epp = Support::Unsupported;
                notes.push(format!("EPP read failed: {e}"));
            }
        }
        match windows_ppm::read_epp1() {
            Ok(_) => {
                caps.ppm.epp1 = Support::Supported;
            }
            Err(e) => {
                caps.ppm.epp1 = Support::Unsupported;
                notes.push(format!("EPP1 read failed: {e}"));
            }
        }
        match windows_ppm::read_max_freq_mhz() {
            Ok(mhz) => {
                caps.ppm.max_freq = Support::Supported;
                max_freq_mhz = Some(mhz);
            }
            Err(e) => {
                caps.ppm.max_freq = Support::Unsupported;
                notes.push(format!("max freq read failed: {e}"));
            }
        }

        // Steps 2-7 — HP WMI transport + typed reads.
        let mut sdd_out = None;
        let mut fan_table_out = None;
        let mut fan_levels_out = None;
        let mut gpu_policy_out = None;
        let mut mux_out = None;
        let mut max_fan_diag = None;

        match hp {
            None => {
                warn!("HP platform handle absent — HP domains forced Unsupported");
                for s in [
                    &mut caps.thermal_mode,
                    &mut caps.fan_rpm_read,
                    &mut caps.fan_manual_level,
                    &mut caps.max_fan,
                    &mut caps.gpu_platform_policy,
                    &mut caps.mux,
                    &mut caps.power_limits,
                ] {
                    *s = Support::Unsupported;
                }
            }
            Some(hp) => {
                // fan count (also the keep-alive op; read-only here).
                match hp.fan_count() {
                    Ok(n) => {
                        caps.fan.count = n;
                        caps.fan_rpm_read = merge(caps.fan_rpm_read, Support::Supported);
                        if known_board && n != caps.fan.count {
                            notes.push(format!("fan count {n} differs from profile"));
                        }
                    }
                    Err(e) => {
                        notes.push(format!("fan count read failed: {e}"));
                        caps.fan_rpm_read = merge(caps.fan_rpm_read, Support::Unsupported);
                        caps.fan_manual_level = merge(caps.fan_manual_level, Support::Unsupported);
                        caps.max_fan = merge(caps.max_fan, Support::Unsupported);
                    }
                }

                // SDD.
                match hp.system_design_data() {
                    Ok(sdd) => {
                        caps.fan.sw_control_declared = sdd.sw_fan_control;
                        if let Some(p) = profile {
                            let expect_v1 = matches!(
                                p.hp.thermal_policy,
                                phelper_domain::board::ThermalPolicyVersion::V1
                            );
                            let got_v1 = sdd.thermal_policy_version == 1;
                            if expect_v1 && !got_v1 {
                                notes.push(format!(
                                    "SDD thermal policy version {} does not match profile v1 — profile wins",
                                    sdd.thermal_policy_version
                                ));
                            }
                        }
                        if !sdd.sw_fan_control {
                            notes.push(
                                "SDD byte4 bit0: firmware does NOT declare sw fan control".into(),
                            );
                        }
                        if !sdd.mux_supported {
                            caps.mux = merge(caps.mux, Support::Unsupported);
                        }
                        sdd_out = Some(sdd);
                    }
                    Err(e) => notes.push(format!("SDD read failed: {e}")),
                }

                // Fan table → clamp.
                match hp.fan_table() {
                    Ok(t) => {
                        match t.clamp_range() {
                            Some((lo, hi)) => {
                                caps.fan.clamp_min = Some(lo);
                                caps.fan.clamp_max = Some(hi);
                            }
                            None => notes.push(
                                "fan table implausible; using BoardProfile clamp fallback".into(),
                            ),
                        }
                        fan_table_out = Some(t);
                    }
                    Err(e) => {
                        notes.push(format!("fan table read failed: {e}"));
                        caps.fan_manual_level = merge(caps.fan_manual_level, Support::Unsupported);
                    }
                }

                // Fan levels (proves 0x2D readback).
                match hp.fan_levels() {
                    Ok(l) => fan_levels_out = Some(l),
                    Err(e) => {
                        notes.push(format!("fan levels read failed: {e}"));
                        caps.fan_rpm_read = merge(caps.fan_rpm_read, Support::Unsupported);
                    }
                }

                // GPU platform policy (proves 0x21/0x22 family).
                match hp.gpu_platform_policy() {
                    Ok(p) => gpu_policy_out = Some(p),
                    Err(e) => {
                        notes.push(format!("GPU platform policy read failed: {e}"));
                        caps.gpu_platform_policy =
                            merge(caps.gpu_platform_policy, Support::Unsupported);
                    }
                }

                // MUX (proves 0x52 read; only if gated open).
                if caps.mux.is_usable() {
                    match hp.mux_mode() {
                        Ok(m) => mux_out = Some(m),
                        Err(e) => {
                            notes.push(format!("MUX read failed: {e}"));
                            caps.mux = merge(caps.mux, Support::Unsupported);
                        }
                    }
                }

                // 0x26 diagnostics (never a capability input).
                match hp.max_fan_readback_diagnostic() {
                    Ok(v) => max_fan_diag = Some(v),
                    Err(e) => notes.push(format!("0x26 diag read failed: {e}")),
                }
            }
        }

        // Manual fan requires the declared clamp.
        if caps.fan.clamp_max.is_none() {
            caps.fan_manual_level = merge(caps.fan_manual_level, Support::Unsupported);
        }
        // V1 board: scale is locked Krpm by profile (R2).
        debug_assert!(caps.fan.scale == FanScale::Krpm || !known_board);

        caps.notes = notes.clone();
        ProbeReport {
            schema_version: 1,
            taken_at_epoch_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            identity,
            known_board,
            elevated,
            capabilities: caps,
            sdd: sdd_out,
            fan_table: fan_table_out,
            fan_levels: fan_levels_out,
            gpu_platform_policy: gpu_policy_out,
            mux: mux_out,
            max_fan_diag,
            epp_ac,
            epp_dc,
            max_freq_mhz,
            notes,
        }
    }
}
