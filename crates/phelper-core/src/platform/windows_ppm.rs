//! Windows PPM via PowrProf (AR-08: PowrProf is the ONLY CPU-policy
//! backend — powercfg.exe is never invoked as a backend, and HWP policy is
//! never dual-written via MSR).
//!
//! Reads are unconditional and work unelevated. Writes
//! (PowerWriteAC/DCValueIndex + PowerSetActiveScheme commit) exist only
//! under the `control` feature and require an elevated token — the
//! capability probe records `ppm.write_privileged` and the coordinator
//! gates on it (fail closed).

use phelper_domain::error::PlatformError;
use phelper_domain::policy::BoostPolicy;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::System::Power::{PowerGetActiveScheme, PowerReadACValueIndex, PowerReadDCValueIndex};
#[cfg(feature = "control")]
use windows::Win32::System::Power::{
    PowerSetActiveScheme, PowerWriteACValueIndex, PowerWriteDCValueIndex,
};
use windows::core::GUID;

/// GUID_PROCESSOR_SUBGROUP 54533251-82be-4824-96c1-47b60b740d00.
const SUB_PROCESSOR: GUID = GUID::from_values(
    0x5453_3251,
    0x82be,
    0x4824,
    [0x96, 0xc1, 0x47, 0xb6, 0x0b, 0x74, 0x0d, 0x00],
);
/// GUID_PROCESSOR_PERFEPP 36687f9e-e3a5-4dbf-b1dc-15eb381c6863 (logical
/// processor 0's energy/performance preference; PERFEPP1 covers 1..n and
/// follows the same policy in practice).
const PERFEPP: GUID = GUID::from_values(
    0x3668_7f9e,
    0xe3a5,
    0x4dbf,
    [0xb1, 0xdc, 0x15, 0xeb, 0x38, 0x1c, 0x68, 0x63],
);
/// GUID_PROCESSOR_PROCFREQMAX 75b0ae3f-bce0-45a7-8c89-c9611c25e100
/// (frequency ceiling in MHz; 0 = unlimited).
const PROCFREQMAX: GUID = GUID::from_values(
    0x75b0_ae3f,
    0xbce0,
    0x45a7,
    [0x8c, 0x89, 0xc9, 0x61, 0x1c, 0x25, 0xe1, 0x00],
);
/// GUID_PROCESSOR_PERFBOOSTMODE be337238-0d82-4146-a960-4f3749d470c7
/// (hidden setting; values = winnt.h PO_BOOST_* 0..=6 — see domain
/// BoostPolicy docs for the 3/4-aliasing and 5/6-rejection caveats).
const PERFBOOSTMODE: GUID = GUID::from_values(
    0xbe33_7238,
    0x0d82,
    0x4146,
    [0xa9, 0x60, 0x4f, 0x37, 0x49, 0xd4, 0x70, 0xc7],
);

/// EPP values in percent (0 = max performance, 100 = max efficiency),
/// split by power source as Windows stores them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EppReading {
    pub ac: u8,
    pub dc: u8,
}

fn active_scheme() -> Result<GUID, PlatformError> {
    unsafe {
        let mut scheme: *mut GUID = std::ptr::null_mut();
        let rc = PowerGetActiveScheme(None, &mut scheme);
        if rc.0 != 0 || scheme.is_null() {
            return Err(PlatformError::Os(format!(
                "PowerGetActiveScheme failed (win32 {})",
                rc.0
            )));
        }
        let guid = *scheme;
        // PowerGetActiveScheme allocates via LocalAlloc.
        let _ = LocalFree(Some(HLOCAL(scheme.cast())));
        Ok(guid)
    }
}

fn read_index(
    ac: bool,
    scheme: &GUID,
    subgroup: &GUID,
    setting: &GUID,
) -> Result<u32, PlatformError> {
    unsafe {
        let mut value: u32 = 0;
        // windows 0.62: AC variant returns WIN32_ERROR, DC variant u32.
        let rc = if ac {
            PowerReadACValueIndex(
                None,
                Some(scheme),
                Some(subgroup),
                Some(setting),
                &mut value,
            )
            .0
        } else {
            PowerReadDCValueIndex(
                None,
                Some(scheme),
                Some(subgroup),
                Some(setting),
                &mut value,
            )
        };
        if rc != 0 {
            return Err(PlatformError::Os(format!(
                "PowerRead{}ValueIndex failed (win32 {})",
                if ac { "AC" } else { "DC" },
                rc
            )));
        }
        Ok(value)
    }
}

/// Read current EPP from the active power scheme. Works unelevated (reads
/// are not privileged; only writes are).
pub(crate) fn read_epp() -> Result<EppReading, PlatformError> {
    let scheme = active_scheme()?;
    let ac = read_index(true, &scheme, &SUB_PROCESSOR, &PERFEPP)?;
    let dc = read_index(false, &scheme, &SUB_PROCESSOR, &PERFEPP)?;
    if ac > 100 || dc > 100 {
        return Err(PlatformError::Data("EPP out of 0-100 range"));
    }
    Ok(EppReading {
        ac: ac as u8,
        dc: dc as u8,
    })
}

/// Read the active scheme's max-frequency ceiling (MHz, 0 = unlimited), AC rail.
pub(crate) fn read_max_freq_mhz() -> Result<u32, PlatformError> {
    let scheme = active_scheme()?;
    read_index(true, &scheme, &SUB_PROCESSOR, &PROCFREQMAX)
}

/// Read the max-frequency ceiling on both rails (AC, DC).
#[allow(dead_code)] // wired in W12 (PpmCollector) / W11 (PpmBackend reads)
pub(crate) fn read_max_freq_mhz_acdc() -> Result<(u32, u32), PlatformError> {
    let scheme = active_scheme()?;
    let ac = read_index(true, &scheme, &SUB_PROCESSOR, &PROCFREQMAX)?;
    let dc = read_index(false, &scheme, &SUB_PROCESSOR, &PROCFREQMAX)?;
    Ok((ac, dc))
}

/// Read PERFBOOSTMODE on both rails (AC, DC).
#[allow(dead_code)] // wired in W11 (PpmBackend reads)
pub(crate) fn read_boost_policy() -> Result<(BoostPolicy, BoostPolicy), PlatformError> {
    let scheme = active_scheme()?;
    let ac = read_index(true, &scheme, &SUB_PROCESSOR, &PERFBOOSTMODE)?;
    let dc = read_index(false, &scheme, &SUB_PROCESSOR, &PERFBOOSTMODE)?;
    let parse = |v: u32| {
        u8::try_from(v)
            .ok()
            .and_then(|b| BoostPolicy::try_from(b).ok())
            .ok_or(PlatformError::Data("unknown PERFBOOSTMODE value"))
    };
    Ok((parse(ac)?, parse(dc)?))
}

#[cfg(feature = "control")]
#[allow(dead_code)] // wired in W11 (ControlCoordinator's PpmBackend)
fn write_index(
    ac: bool,
    scheme: &GUID,
    subgroup: &GUID,
    setting: &GUID,
    value: u32,
) -> Result<(), PlatformError> {
    unsafe {
        // windows-0.62 signature lottery (verified against generated
        // bindings): scheme = raw *const GUID; subgroup/setting =
        // Option<*const GUID>; AC → WIN32_ERROR, DC → u32.
        let rc = if ac {
            PowerWriteACValueIndex(
                None,
                scheme as *const GUID,
                Some(subgroup as *const GUID),
                Some(setting as *const GUID),
                value,
            )
            .0
        } else {
            PowerWriteDCValueIndex(
                None,
                scheme as *const GUID,
                Some(subgroup as *const GUID),
                Some(setting as *const GUID),
                value,
            )
        };
        if rc != 0 {
            return Err(PlatformError::Os(format!(
                "PowerWrite{}ValueIndex failed (win32 {})",
                if ac { "AC" } else { "DC" },
                rc
            )));
        }
        Ok(())
    }
}

/// Re-apply the active scheme so written values take effect (the
/// powercfg /S pattern: PowerWrite*ValueIndex only touches the store).
#[cfg(feature = "control")]
#[allow(dead_code)] // wired in W11 (ControlCoordinator's PpmBackend)
fn commit_active_scheme() -> Result<(), PlatformError> {
    let scheme = active_scheme()?;
    unsafe {
        let rc = PowerSetActiveScheme(None, Some(&scheme));
        if rc.0 != 0 {
            return Err(PlatformError::Os(format!(
                "PowerSetActiveScheme failed (win32 {})",
                rc.0
            )));
        }
        Ok(())
    }
}

/// PowrProf implementation of the domain `CpuPolicyBackend` port (M2 write
/// path). Stateless — every call re-resolves the ACTIVE scheme (the user
/// may switch schemes between commands; we always act on what is active).
#[cfg(feature = "control")]
#[allow(dead_code)] // constructed by the ControlCoordinator (W11)
pub(crate) struct PpmBackend;

#[cfg(feature = "control")]
impl phelper_domain::ports::CpuPolicyBackend for PpmBackend {
    fn read_epp(&self) -> Result<(u8, u8), PlatformError> {
        let r = read_epp()?;
        Ok((r.ac, r.dc))
    }

    fn read_max_freq_mhz(&self) -> Result<(u32, u32), PlatformError> {
        read_max_freq_mhz_acdc()
    }

    fn read_boost_policy(&self) -> Result<(BoostPolicy, BoostPolicy), PlatformError> {
        read_boost_policy()
    }

    fn write_epp(&self, ac: Option<u8>, dc: Option<u8>) -> Result<(), PlatformError> {
        let scheme = active_scheme()?;
        if let Some(v) = ac {
            write_index(true, &scheme, &SUB_PROCESSOR, &PERFEPP, v as u32)?;
        }
        if let Some(v) = dc {
            write_index(false, &scheme, &SUB_PROCESSOR, &PERFEPP, v as u32)?;
        }
        if ac.is_some() || dc.is_some() {
            commit_active_scheme()?;
        }
        Ok(())
    }

    fn write_max_freq_mhz(&self, ac: Option<u32>, dc: Option<u32>) -> Result<(), PlatformError> {
        let scheme = active_scheme()?;
        if let Some(v) = ac {
            write_index(true, &scheme, &SUB_PROCESSOR, &PROCFREQMAX, v)?;
        }
        if let Some(v) = dc {
            write_index(false, &scheme, &SUB_PROCESSOR, &PROCFREQMAX, v)?;
        }
        if ac.is_some() || dc.is_some() {
            commit_active_scheme()?;
        }
        Ok(())
    }

    fn write_boost_policy(&self, mode: BoostPolicy) -> Result<(), PlatformError> {
        let scheme = active_scheme()?;
        let v = u8::from(mode) as u32;
        write_index(true, &scheme, &SUB_PROCESSOR, &PERFBOOSTMODE, v)?;
        write_index(false, &scheme, &SUB_PROCESSOR, &PERFBOOSTMODE, v)?;
        commit_active_scheme()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Change-detector: GUIDs must stay byte-identical to winnt.h.
    #[test]
    fn guids_match_winnt() {
        fn t(g: &GUID) -> (u32, u16, u16, [u8; 8]) {
            (g.data1, g.data2, g.data3, g.data4)
        }
        assert_eq!(
            t(&SUB_PROCESSOR),
            (0x54533251, 0x82be, 0x4824, [0x96, 0xc1, 0x47, 0xb6, 0x0b, 0x74, 0x0d, 0x00])
        );
        assert_eq!(
            t(&PERFEPP),
            (0x36687f9e, 0xe3a5, 0x4dbf, [0xb1, 0xdc, 0x15, 0xeb, 0x38, 0x1c, 0x68, 0x63])
        );
        assert_eq!(
            t(&PROCFREQMAX),
            (0x75b0ae3f, 0xbce0, 0x45a7, [0x8c, 0x89, 0xc9, 0x61, 0x1c, 0x25, 0xe1, 0x00])
        );
        assert_eq!(
            t(&PERFBOOSTMODE),
            (0xbe337238, 0x0d82, 0x4146, [0xa9, 0x60, 0x4f, 0x37, 0x49, 0xd4, 0x70, 0xc7])
        );
    }

    #[test]
    fn boost_policy_wire_roundtrip() {
        for v in 0u8..=6 {
            let p = BoostPolicy::try_from(v).unwrap();
            assert_eq!(u8::from(p), v);
        }
        assert!(BoostPolicy::try_from(7u8).is_err());
    }
}
