//! HP WMI wire protocol — the ONLY place in the codebase with byte-level
//! protocol knowledge. Everything here is a pure function over bytes so the
//! entire wire format is unit-testable without WMI.
//!
//! Sources (cross-validated, see docs/feasibility-16-wf0032TX.md §2):
//! - Linux hp-wmi.c (master 2026-08-25), incl. 8BAB support commit 13fa3aaf02
//! - OmenMon (Hardware/Bios*.cs), OmenHwCtl (OmenHwCtl.ps1), OmenSuperHub
//!
//! Uncertain-width items are centralized here and validated by on-device
//! probe + fixtures before any stable claim (AR-06).

use phelper_domain::error::HpWmiError;
use phelper_domain::hp::{FanTable, FanTableEntry, SystemDesignData};
use phelper_domain::policy::{CpuPowerLimits, FanLevels, GpuPlatformPolicy, MuxMode, ThermalMode};

/// "SECU" little-endian (hp-wmi.c args->signature = 0x55434553).
pub(crate) const SIGNATURE: u32 = u32::from_le_bytes(*b"SECU");

/// Command groups (hp-wmi.c enum hp_wmi_command).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HpCommandGroup {
    /// 0x01 — legacy read path (MUX get).
    LegacyRead = 0x01,
    /// 0x02 — GPU mode write path (MUX set).
    #[allow(dead_code)] // M3
    GpuModeWrite = 0x02,
    /// 0x20008 — main gaming command group.
    Gaming = 0x20008,
}

/// Command types within the gaming group (hp-wmi.c enum, names from kernel).
pub(crate) mod cmd {
    /// Get fan count; also maintains user-defined thermal/fan states
    /// (keep-alive heartbeat op).
    pub(crate) const FAN_COUNT_GET: u8 = 0x10;
    /// Thermal/performance mode set, payload {0xFF, mode}, outsize=0
    /// (hp-wmi.c HPWMI_SET_PERFORMANCE_MODE via HPWMI_GM).
    #[allow(dead_code)] // call sites live behind `control` (W5); kept unconditional for tests
    pub(crate) const SET_PERFORMANCE_MODE: u8 = 0x1A;
    /// GPU platform policy (cTGP/PPAB/dstate/slowdown) get.
    pub(crate) const GPU_POLICY_GET: u8 = 0x21;
    /// GPU platform policy set. M3.
    #[allow(dead_code)]
    pub(crate) const GPU_POLICY_SET: u8 = 0x22;
    /// Max fan get — DIAGNOSTICS ONLY, unreliable (hp-wmi.c 46be1453e6).
    pub(crate) const MAX_FAN_GET: u8 = 0x26;
    /// Max fan set, payload = 4-byte LE int 1/0, outsize=0
    /// (hp-wmi.c HPWMI_FAN_SPEED_MAX_SET_QUERY: `int enabled`).
    #[allow(dead_code)] // call sites live behind `control` (W5)
    pub(crate) const MAX_FAN_SET: u8 = 0x27;
    /// System design data.
    pub(crate) const SYSTEM_DESIGN_DATA: u8 = 0x28;
    /// Power limits {pl1,pl2,pl4,concurrent} — EXPERIMENTAL, M3.
    #[allow(dead_code)]
    pub(crate) const POWER_LIMITS: u8 = 0x29;
    /// Fan levels get (128-byte buffer; per-fan byte * 100 = RPM on V1).
    pub(crate) const FAN_LEVELS_GET: u8 = 0x2D;
    /// Fan levels set, payload `u8[2] {channel0, channel1}` in 100-RPM units,
    /// presented as `{left, right}` on 8BAB; 0 = auto,
    /// outsize=0 (hp-wmi.c HPWMI_VICTUS_S_FAN_SPEED_SET_QUERY).
    #[allow(dead_code)] // call sites live behind `control` (W5)
    pub(crate) const FAN_LEVELS_SET: u8 = 0x2E;
    /// Fan table get (4 zero bytes in, 128-byte table out).
    pub(crate) const FAN_TABLE_GET: u8 = 0x2F;
    /// Graphics MUX (LegacyRead get / GpuModeWrite set; reboot-required).
    pub(crate) const MUX: u8 = 0x52;
}

/// Output buffer size → hpqBIOSInt{0,4,128,1024,4096} method name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputSize {
    /// Write ops whose response is empty (hpqBIOSInt0) — 0x1A/0x27/0x2E all
    /// use outsize=0 (hp-wmi.c call sites).
    #[allow(dead_code)] // write call sites live behind `control` (W5)
    Zero,
    Small4,
    Medium128,
    #[allow(dead_code)] // reserved
    Large1024,
    #[allow(dead_code)] // reserved
    XLarge4096,
}

impl OutputSize {
    pub(crate) fn method_name(self) -> &'static str {
        match self {
            OutputSize::Zero => "hpqBIOSInt0",
            OutputSize::Small4 => "hpqBIOSInt4",
            OutputSize::Medium128 => "hpqBIOSInt128",
            OutputSize::Large1024 => "hpqBIOSInt1024",
            OutputSize::XLarge4096 => "hpqBIOSInt4096",
        }
    }
}

/// Wire input for one BIOS method call (MOF class hpqBDataIn).
/// `size` semantics: newer firmware requires 0 for reads (kernel
/// `zero_if_sup`, auto-detected at transport init); writes carry input len.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BiosArgs {
    pub(crate) signature: u32,
    pub(crate) command: u32,
    pub(crate) command_type: u32,
    pub(crate) size: u32,
    pub(crate) data: Vec<u8>,
}

impl BiosArgs {
    pub(crate) fn read(
        group: HpCommandGroup,
        cmd_type: u8,
        input: &[u8],
        zero_insize: bool,
    ) -> Self {
        Self {
            signature: SIGNATURE,
            command: group as u32,
            command_type: cmd_type as u32,
            size: if zero_insize { 0 } else { input.len() as u32 },
            data: input.to_vec(),
        }
    }

    /// Write args: `datasize` is ALWAYS the real input length. The
    /// insize=0 mode (`zero_if_sup`) is a read-side probe only — hp-wmi.c
    /// fills `args->datasize = insize` for every set call (verified
    /// 2026-08-26 against master: 0x1A insize=2, 0x2E insize=2, 0x27
    /// insize=4, all outsize=0).
    #[allow(dead_code)] // call sites live behind `control` (W5); unconditional for tests
    pub(crate) fn write(group: HpCommandGroup, cmd_type: u8, input: &[u8]) -> Self {
        Self {
            signature: SIGNATURE,
            command: group as u32,
            command_type: cmd_type as u32,
            size: input.len() as u32,
            data: input.to_vec(),
        }
    }
}

// ---------------------------------------------------------------- decoders

fn need(buf: &[u8], n: usize, what: &'static str) -> Result<(), HpWmiError> {
    if buf.len() < n {
        return Err(HpWmiError::InvalidResponse(what));
    }
    Ok(())
}

/// 0x10 → fan count in byte 0 (hp-wmi.c).
pub(crate) fn decode_fan_count(buf: &[u8]) -> Result<u8, HpWmiError> {
    need(buf, 1, "fan count needs >= 1 byte")?;
    Ok(buf[0])
}

/// 0x28 → SystemDesignData. Derived fields limited to cross-validated bytes
/// (3 tp-version, 4b0 sw-fan, 5 default PL4, 7 MUX byte); the rest stays raw.
pub(crate) fn decode_sdd(buf: &[u8]) -> Result<SystemDesignData, HpWmiError> {
    need(buf, 8, "system design data needs >= 8 bytes")?;
    let mux_byte = buf[7];
    Ok(SystemDesignData {
        raw: buf.to_vec(),
        thermal_policy_version: buf[3],
        sw_fan_control: buf[4] & 0x01 != 0,
        default_pl4_w: buf[5],
        mux_byte,
        mux_supported: mux_byte & 0x08 != 0,
    })
}

/// 0x2F → discrete fan level/noise table: {num_fans, unknown} + 3-byte
/// entries {channel0, channel1, noise_db} (hp-wmi.c victus fan table). It does not
/// contain firmware temperature thresholds or a temperature-to-speed curve.
pub(crate) fn decode_fan_table(buf: &[u8]) -> Result<FanTable, HpWmiError> {
    need(buf, 2, "fan table needs >= 2 bytes")?;
    let num_fans = buf[0];
    let entries = buf[2..]
        .as_chunks::<3>()
        .0
        .iter()
        .take_while(|c| !(c[0] == 0 && c[1] == 0 && c[2] == 0)) // stop at padding
        .map(|c| FanTableEntry {
            left: c[0],
            right: c[1],
            noise_db: c[2],
        })
        .collect();
    Ok(FanTable {
        num_fans,
        entries,
        raw: buf.to_vec(),
    })
}

/// 0x2F request input: 4 zero bytes (hp-wmi.c).
pub(crate) fn encode_fan_table_request() -> [u8; 4] {
    [0, 0, 0, 0]
}

/// 0x2D → per-fan levels; level * 100 = RPM on V1 (hp-wmi.c line ~776).
pub(crate) fn decode_fan_levels(buf: &[u8]) -> Result<FanLevels, HpWmiError> {
    need(buf, 2, "fan levels need >= 2 bytes")?;
    Ok(FanLevels::new(buf[0] as u16, buf[1] as u16))
}

/// 0x21 → {ctgp, ppab, dstate, slowdown_temp} (hp-wmi.c victus_gpu_power_modes).
pub(crate) fn decode_gpu_policy(buf: &[u8]) -> Result<GpuPlatformPolicy, HpWmiError> {
    need(buf, 4, "gpu policy needs >= 4 bytes")?;
    Ok(GpuPlatformPolicy {
        ctgp: buf[0] != 0,
        ppab: buf[1] != 0,
        dstate: buf[2],
        slowdown_temp_c: buf[3],
    })
}

/// 0x52 get → byte 0 mode: 0=hybrid, 1=discrete, 2=optimus, 3=UMA
/// (hp-wmi.c gpu_mux_mode). UMA is not offerable on this dGPU machine and
/// maps to an error rather than a mode (AR-06).
pub(crate) fn decode_mux(buf: &[u8]) -> Result<MuxMode, HpWmiError> {
    need(buf, 1, "mux needs >= 1 byte")?;
    match buf[0] {
        0x00 => Ok(MuxMode::Hybrid),
        0x01 => Ok(MuxMode::Discrete),
        0x02 => Ok(MuxMode::Optimus),
        _ => Err(HpWmiError::InvalidResponse("unknown mux mode value")),
    }
}

/// 0x26 → byte 0 != 0. DIAGNOSTICS ONLY (unreliable on this firmware).
pub(crate) fn decode_max_fan_diag(buf: &[u8]) -> Result<bool, HpWmiError> {
    need(buf, 1, "max fan diag needs >= 1 byte")?;
    Ok(buf[0] != 0)
}

// ---------------------------------------------------------------- encoders
// (write payloads; layouts verified against hp-wmi.c master 2026-08-26, S1)

/// 0x1A thermal mode set, V1 mapping (8BAB is statically V1 — BoardProfile
/// locks it). Kernel: `char buffer[2] = {-1, mode}` via
/// HPWMI_SET_PERFORMANCE_MODE/HPWMI_GM, insize=2, outsize=0.
#[allow(dead_code)] // call sites live behind `control` (W5); unconditional for tests
pub(crate) fn encode_thermal_mode_v1(mode: ThermalMode) -> [u8; 2] {
    let v1 = match mode {
        ThermalMode::Balanced => 0x30,    // HP_OMEN_V1_THERMAL_PROFILE_DEFAULT
        ThermalMode::Performance => 0x31, // HP_OMEN_V1_THERMAL_PROFILE_PERFORMANCE
    };
    [0xFF, v1]
}

/// 0x2E fan levels set: `u8[2] {channel0, channel1}` in 100-RPM units,
/// presented as left/right on 8BAB; 0 = firmware
/// automatic (hp-wmi.c HPWMI_VICTUS_S_FAN_SPEED_SET_QUERY, insize=2,
/// outsize=0). The wire is u8 per channel — anything above 255 krpm-units
/// cannot be encoded; the safety layer's clamp check runs long before this
/// guard, and both fail closed rather than truncate.
#[allow(dead_code)] // call sites live behind `control` (W5); unconditional for tests
pub(crate) fn encode_fan_levels(levels: FanLevels) -> Result<[u8; 2], HpWmiError> {
    let left =
        u8::try_from(levels.left).map_err(|_| HpWmiError::InvalidInput("left fan level > 255"))?;
    let right = u8::try_from(levels.right)
        .map_err(|_| HpWmiError::InvalidInput("right fan level > 255"))?;
    Ok([left, right])
}

/// 0x27 max fan set: 4-byte LE `int enabled` (hp-wmi.c
/// HPWMI_FAN_SPEED_MAX_SET_QUERY, insize=4, outsize=0).
#[allow(dead_code)] // call sites live behind `control` (W5); unconditional for tests
pub(crate) fn encode_max_fan(on: bool) -> [u8; 4] {
    (on as u32).to_le_bytes()
}

/// 0x22 GPU platform policy set: `{ctgp, ppab, dstate, gpu_slowdown_temp}`,
/// insize=4, outsize=0 (hp-wmi.c HPWMI_SET_GPU_THERMAL_MODES_QUERY via
/// HPWMI_GM; OSH SetGpuPowerState same layout). Full 4-byte write — callers
/// read-modify-write via 0x21 to preserve untouched fields (the kernel
/// preserves `gpu_slowdown_temp` this way).
#[allow(dead_code)] // call sites live behind `control` (M3); unconditional for tests
pub(crate) fn encode_gpu_policy(p: GpuPlatformPolicy) -> [u8; 4] {
    [
        u8::from(p.ctgp),
        u8::from(p.ppab),
        p.dstate,
        p.slowdown_temp_c,
    ]
}

// ---------------------------------------------------------------- 0x29 power limits
// S1 verdict (2026-08-26, hp-wmi.c master + OSH OmenHardware.cs):
// 4-byte payload, per-byte 0xFF = NO_CHANGE, pl1/pl2 = 0x00 restores the
// firmware DEFAULT. byte2=pl4 and byte3=cpu_gpu_concurrent are
// cross-confirmed (kernel struct + OSH SetCpuPowerLimit4/SetConcurrentTdp
// both write single bytes at offsets 2/3). The byte0/byte1 pl1↔pl2 order
// rests on the kernel struct ALONE (OSH only ever writes both to the same
// value; the kernel only writes {0,0,FF,FF} on this board class) — that is
// the unresolved half the on-device arbitration (S2) settles.

/// 0x00 in pl1/pl2 = restore firmware default (hp-wmi.c
/// HP_POWER_LIMIT_DEFAULT; the kernel's AC/DC re-actualization write).
pub(crate) const POWER_LIMIT_DEFAULT: u8 = 0x00;
/// 0xFF in any byte = leave that field unchanged (hp-wmi.c
/// HP_POWER_LIMIT_NO_CHANGE; OSH writes rely on the same sentinel).
pub(crate) const POWER_LIMIT_NO_CHANGE: u8 = 0xFF;

/// Candidate A — kernel struct order `{pl1, pl2, 0xFF, 0xFF}`.
/// **PROVEN WRONG on 8BAB** (M3 S2 arbitration, 2026-08-26): this encoding
/// wrote intent PL1=45/PL2=90 and MSR 0x610 read back PL1=90/PL2=45. Kept
/// only so `power-spike --order kernel` can re-run the experiment.
#[allow(dead_code)] // M3 S2 arbitration
pub(crate) fn encode_power_limits_kernel(pl1_w: u8, pl2_w: u8) -> [u8; 4] {
    [pl1_w, pl2_w, POWER_LIMIT_NO_CHANGE, POWER_LIMIT_NO_CHANGE]
}

/// Candidate B — swapped pl1/pl2 (the documented "OSH order" reading).
#[allow(dead_code)] // M3 S2 arbitration
pub(crate) fn encode_power_limits_swapped(pl1_w: u8, pl2_w: u8) -> [u8; 4] {
    [pl2_w, pl1_w, POWER_LIMIT_NO_CHANGE, POWER_LIMIT_NO_CHANGE]
}

/// THE 0x29 encoder for 8BAB — byte order settled on-device by the M3 S2
/// arbitration (2026-08-26, double A/B): **byte0 = PL2, byte1 = PL1** on
/// this firmware, the OPPOSITE of the kernel struct. Do not "fix" this to
/// the kernel order — that is exactly the trap §25's mandatory three-step
/// verification exists for. cc always goes out as 0xFF (NO_CHANGE): it has
/// no readback channel and no restore semantics — permanently unwritable.
pub(crate) fn encode_power_limits(l: &CpuPowerLimits) -> [u8; 4] {
    [
        l.pl2_w,
        l.pl1_w,
        // byte2 = PL4: 0 means "not requested" in the domain → NO_CHANGE on
        // the wire (M4.1: explicit pl4 writes are verified via MCHBAR 0x59B0
        // readback; the 0x00 DEFAULT sentinel is never emitted — proven
        // ineffective on this firmware).
        if l.pl4_w == 0 {
            POWER_LIMIT_NO_CHANGE
        } else {
            l.pl4_w
        },
        POWER_LIMIT_NO_CHANGE,
    ]
}

/// Restore firmware default PL1/PL2: `{0x00, 0x00, 0xFF, 0xFF}` — the exact
/// write the kernel issues on AC/DC power-source events.
#[allow(dead_code)] // M3
pub(crate) fn encode_power_limits_restore_default() -> [u8; 4] {
    [
        POWER_LIMIT_DEFAULT,
        POWER_LIMIT_DEFAULT,
        POWER_LIMIT_NO_CHANGE,
        POWER_LIMIT_NO_CHANGE,
    ]
}

/// DEV-SPIKE ONLY (M4-mini): byte2-only PL4 write `{FF, FF, pl4, FF}`, all
/// other bytes NO_CHANGE. Used by `pl4-spike` to verify the MCHBAR 0x59B0
/// readback channel; NOT wired into any stable path (the transport rejects
/// pl4≠0 until that verification lands).
#[allow(dead_code)] // M4-mini spike
pub(crate) fn encode_power_limits_pl4_only(pl4_w: u8) -> [u8; 4] {
    [
        POWER_LIMIT_NO_CHANGE,
        POWER_LIMIT_NO_CHANGE,
        pl4_w,
        POWER_LIMIT_NO_CHANGE,
    ]
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_secu_le() {
        assert_eq!(SIGNATURE, 0x5543_4553);
        assert_eq!(SIGNATURE.to_le_bytes(), [0x53, 0x45, 0x43, 0x55]);
        assert_eq!(&SIGNATURE.to_le_bytes(), b"SECU");
    }

    #[test]
    fn method_names_match_mof() {
        assert_eq!(OutputSize::Zero.method_name(), "hpqBIOSInt0");
        assert_eq!(OutputSize::Small4.method_name(), "hpqBIOSInt4");
        assert_eq!(OutputSize::Medium128.method_name(), "hpqBIOSInt128");
        assert_eq!(OutputSize::Large1024.method_name(), "hpqBIOSInt1024");
        assert_eq!(OutputSize::XLarge4096.method_name(), "hpqBIOSInt4096");
    }

    #[test]
    fn read_args_zero_insize_modes() {
        let a = BiosArgs::read(HpCommandGroup::Gaming, cmd::FAN_COUNT_GET, &[], true);
        assert_eq!(a.command, 0x20008);
        assert_eq!(a.command_type, 0x10);
        assert_eq!(a.size, 0);
        let b = BiosArgs::read(
            HpCommandGroup::Gaming,
            cmd::FAN_TABLE_GET,
            &[0, 0, 0, 0],
            false,
        );
        assert_eq!(b.size, 4);
    }

    #[test]
    fn sdd_parses_known_bytes() {
        let mut buf = vec![0u8; 128];
        buf[3] = 0x01; // tp version V1
        buf[4] = 0x01; // sw fan control
        buf[5] = 0xD7; // default PL4 = 215 W (OmenMon observation)
        buf[7] = 0x08; // MUX bit3
        let sdd = decode_sdd(&buf).unwrap();
        assert_eq!(sdd.thermal_policy_version, 1);
        assert!(sdd.sw_fan_control);
        assert_eq!(sdd.default_pl4_w, 215);
        assert!(sdd.mux_supported);
        assert_eq!(sdd.raw.len(), 128);
    }

    #[test]
    fn sdd_rejects_short_buffer() {
        assert!(decode_sdd(&[0u8; 7]).is_err());
    }

    #[test]
    fn fan_table_parses_entries_and_stops_at_padding() {
        let mut buf = vec![0u8; 128];
        buf[0] = 2; // num fans
        buf[2..5].copy_from_slice(&[5, 5, 20]);
        buf[5..8].copy_from_slice(&[30, 30, 35]);
        buf[8..11].copy_from_slice(&[55, 55, 48]);
        let t = decode_fan_table(&buf).unwrap();
        assert_eq!(t.num_fans, 2);
        assert_eq!(t.entries.len(), 3);
        assert_eq!(t.entries[2].left, 55);
        assert_eq!(t.clamp_range(), Some((5, 55)));
    }

    #[test]
    fn fan_table_clamp_rejects_implausible() {
        let mut buf = vec![0u8; 128];
        buf[0] = 2;
        buf[2..5].copy_from_slice(&[200, 210, 0]); // garbage levels
        let t = decode_fan_table(&buf).unwrap();
        assert_eq!(t.clamp_range(), None);
        // empty table
        let t2 = decode_fan_table(&[2, 0]).unwrap();
        assert_eq!(t2.clamp_range(), None);
    }

    #[test]
    fn fan_levels_decode_100rpm_units() {
        let l = decode_fan_levels(&[35, 0]).unwrap();
        assert_eq!(l.left, 35);
        assert_eq!(l.left_rpm(), 3500);
        assert!(FanLevels::AUTO.is_auto());
    }

    #[test]
    fn gpu_policy_decode() {
        let p = decode_gpu_policy(&[1, 1, 1, 0x57]).unwrap();
        assert!(p.ctgp && p.ppab);
        assert_eq!(p.dstate, 1);
        assert_eq!(p.slowdown_temp_c, 87);
    }

    #[test]
    fn gpu_policy_encode_decode_roundtrip() {
        let p = GpuPlatformPolicy {
            ctgp: false,
            ppab: true,
            dstate: 2,
            slowdown_temp_c: 87,
        };
        let wire = encode_gpu_policy(p);
        assert_eq!(wire, [0, 1, 2, 87]);
        assert_eq!(decode_gpu_policy(&wire).unwrap(), p);
    }

    #[test]
    fn power_limits_encoders() {
        fn pl(pl1_w: u8, pl2_w: u8, pl4_w: u8) -> CpuPowerLimits {
            CpuPowerLimits {
                pl1_w,
                pl2_w,
                pl4_w,
                cpu_gpu_concurrent_w: 0,
            }
        }
        assert_eq!(encode_power_limits_kernel(45, 90), [45, 90, 0xFF, 0xFF]);
        assert_eq!(encode_power_limits_swapped(45, 90), [90, 45, 0xFF, 0xFF]);
        // The canonical encoder is the S2-arbitrated one (swapped on 8BAB);
        // pl4_w = 0 → byte2 NO_CHANGE, nonzero → explicit PL4 (M4.1).
        assert_eq!(encode_power_limits(&pl(45, 90, 0)), [90, 45, 0xFF, 0xFF]);
        assert_eq!(encode_power_limits(&pl(45, 90, 150)), [90, 45, 150, 0xFF]);
        assert_eq!(encode_power_limits_restore_default(), [0, 0, 0xFF, 0xFF]);
        assert_eq!(encode_power_limits_pl4_only(150), [0xFF, 0xFF, 150, 0xFF]);
    }

    #[test]
    fn mux_decode_and_unknown_rejected() {
        assert_eq!(decode_mux(&[0]).unwrap(), MuxMode::Hybrid);
        assert_eq!(decode_mux(&[1]).unwrap(), MuxMode::Discrete);
        assert_eq!(decode_mux(&[2]).unwrap(), MuxMode::Optimus);
        assert!(decode_mux(&[3]).is_err());
        assert!(decode_mux(&[0xFF]).is_err());
    }

    #[test]
    fn max_fan_diag_decode() {
        assert!(decode_max_fan_diag(&[1]).unwrap());
        assert!(!decode_max_fan_diag(&[0]).unwrap());
    }

    #[test]
    fn thermal_mode_v1_encode() {
        assert_eq!(encode_thermal_mode_v1(ThermalMode::Balanced), [0xFF, 0x30]);
        assert_eq!(
            encode_thermal_mode_v1(ThermalMode::Performance),
            [0xFF, 0x31]
        );
    }

    #[test]
    fn fan_levels_encode_and_range_guard() {
        assert_eq!(encode_fan_levels(FanLevels::new(30, 0)).unwrap(), [30, 0]);
        assert_eq!(encode_fan_levels(FanLevels::AUTO).unwrap(), [0, 0]);
        assert!(encode_fan_levels(FanLevels::new(256, 0)).is_err());
        assert!(encode_fan_levels(FanLevels::new(0, 1000)).is_err());
    }

    #[test]
    fn max_fan_encode_u32_le() {
        assert_eq!(encode_max_fan(true), [1, 0, 0, 0]);
        assert_eq!(encode_max_fan(false), [0, 0, 0, 0]);
    }

    #[test]
    fn write_args_always_carry_input_len() {
        // insize=0 is a READ-side probe; writes must never see size=0.
        let w = BiosArgs::write(
            HpCommandGroup::Gaming,
            cmd::SET_PERFORMANCE_MODE,
            &[0xFF, 0x31],
        );
        assert_eq!(w.command, 0x20008);
        assert_eq!(w.command_type, 0x1A);
        assert_eq!(w.size, 2);
        assert_eq!(w.data, vec![0xFF, 0x31]);
        assert_eq!(w.signature, SIGNATURE);
        // Even an empty-input write (none exist today) is explicit.
        let w0 = BiosArgs::write(HpCommandGroup::Gaming, cmd::FAN_COUNT_GET, &[]);
        assert_eq!(w0.size, 0);
    }
}
