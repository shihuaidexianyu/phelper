//! PawnIO driver binding — read-only hardware telemetry infrastructure
//! (AR-09): signed modules + per-operation allow-lists, never a generic
//! write primitive. Two modules are used:
//!   IntelMSR     — allow-listed MSR reads (thermals, RAPL, APERF/MPERF)
//!   IntelMCHBAR  — MCHBAR window reads (PL4 readback, M4-mini); the module
//!                  exports NO write ioctls at all (source-verified)
//!
//! Protocol constants verified against the public driver interface (the
//! same shape LibreHardwareMonitor's PawnIo.cs uses; cpu-temp's Rust
//! translation confirmed the exact numbers):
//!   device path   \\?\GLOBALROOT\Device\PawnIO
//!   device type   41394 << 16
//!   LoadBinary    (41394<<16) | (0x821<<2)
//!   Execute       (41394<<16) | (0x841<<2)
//!   execute input [32-byte ASCII fn name][input i64s LE] → output i64s LE
//!
//! There is deliberately NO write_msr/write_io_port/write_ec/write_mchbar
//! anywhere in this crate. Only the modules' read functions are called.

use phelper_domain::error::PlatformError;
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::PCWSTR;

const DEVICE_PATH: &str = r"\\?\GLOBALROOT\Device\PawnIO";
const DEVICE_TYPE: u32 = 41_394 << 16;
const IOCTL_LOAD_BINARY: u32 = DEVICE_TYPE | (0x821 << 2);
const IOCTL_EXECUTE: u32 = DEVICE_TYPE | (0x841 << 2);
const FN_NAME_LEN: usize = 32;

/// One loaded PawnIO module (IntelMSR.bin) on an open device handle.
pub(crate) struct PawnIo {
    handle: HANDLE,
}

impl PawnIo {
    /// Open the device and load a signed module image.
    pub(crate) fn load_module(module_image: &[u8]) -> Result<Self, PlatformError> {
        let path: Vec<u16> = DEVICE_PATH.encode_utf16().chain(Some(0)).collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| PlatformError::Driver(format!("PawnIO device open failed: {e}")))?;

        let mut returned: u32 = 0;
        unsafe {
            DeviceIoControl(
                handle,
                IOCTL_LOAD_BINARY,
                Some(module_image.as_ptr().cast()),
                module_image.len() as u32,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| PlatformError::Driver(format!("module load failed: {e}")))?;

        Ok(Self { handle })
    }

    /// Execute a module function. Input/output are i64 lanes (LE).
    pub(crate) fn execute(
        &self,
        func: &str,
        input: &[i64],
        output: &mut [i64],
    ) -> Result<(), PlatformError> {
        let mut request = vec![0u8; FN_NAME_LEN + input.len() * 8];
        let name = func.as_bytes();
        let copy = name.len().min(FN_NAME_LEN - 1);
        request[..copy].copy_from_slice(&name[..copy]);
        for (i, v) in input.iter().enumerate() {
            request[FN_NAME_LEN + i * 8..FN_NAME_LEN + (i + 1) * 8]
                .copy_from_slice(&v.to_le_bytes());
        }

        let mut out_bytes = vec![0u8; output.len() * 8];
        let mut returned: u32 = 0;
        unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_EXECUTE,
                Some(request.as_ptr().cast()),
                request.len() as u32,
                Some(out_bytes.as_mut_ptr().cast()),
                out_bytes.len() as u32,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|e| PlatformError::Driver(format!("execute {func}: {e}")))?;

        let lanes = (returned as usize / 8).min(output.len());
        for (i, slot) in output.iter_mut().enumerate().take(lanes) {
            *slot = i64::from_le_bytes(
                out_bytes[i * 8..(i + 1) * 8]
                    .try_into()
                    .expect("8-byte lane"),
            );
        }
        Ok(())
    }
}

impl Drop for PawnIo {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

// SAFETY: the driver serializes ioctls internally; the handle is moved (not
// shared) between threads when the coordinator re-parks the collector.
unsafe impl Send for PawnIo {}

// ---------------------------------------------------------------------------
// IntelMSR module surface.
// ---------------------------------------------------------------------------

// Intel MSRs used (read-only, allow-listed by the signed module):
//   0x19C IA32_THERM_STATUS (core temp delta bits 22:16, valid bit 31)
//   0x1A2 MSR_TEMPERATURE_TARGET (TjMax bits 23:16)
//   0x1B1 IA32_PACKAGE_THERM_STATUS (pkg delta + status bits; same decode)
//   0x606 MSR_RAPL_POWER_UNIT (energy status unit bits 12:8)
//   0x610 MSR_PKG_POWER_LIMIT (PL1 bits 14:0, PL2 bits 46:32, ×power unit)
//   0x611 MSR_PKG_ENERGY_STATUS (RAPL energy counter, wraps at 2^32)
//   0xE7  IA32_MPERF / 0xE8 IA32_APERF (effective-clock derivation)
// (0x64F MSR_CORE_PERF_LIMIT_REASONS was tried and dropped: the signed
// module's allow-list rejects it with 0x80070005 — on-device evidence.)
pub(crate) const MSR_THERM_STATUS: u32 = 0x19C;
pub(crate) const MSR_TEMPERATURE_TARGET: u32 = 0x1A2;
pub(crate) const MSR_PKG_THERM_STATUS: u32 = 0x1B1;
pub(crate) const MSR_RAPL_POWER_UNIT: u32 = 0x606;
pub(crate) const MSR_PKG_POWER_LIMIT: u32 = 0x610;
pub(crate) const MSR_PKG_ENERGY_STATUS: u32 = 0x611;
pub(crate) const MSR_MPERF: u32 = 0xE7;
pub(crate) const MSR_APERF: u32 = 0xE8;

/// Read one MSR via the module's exported `ioctl_read_msr` (name verified
/// against the signed IntelMSR module's consumers): input lane 0 = MSR
/// index, output lane 0 = 64-bit value.
pub(crate) fn read_msr(io: &PawnIo, msr: u32) -> Result<u64, PlatformError> {
    let mut out = [0i64; 1];
    io.execute("ioctl_read_msr", &[i64::from(msr)], &mut out)?;
    Ok(out[0] as u64)
}

/// RAPL energy counter with 32-bit wrap compensation.
pub(crate) fn delta_energy(prev: u32, now: u32) -> u64 {
    now.wrapping_sub(prev) as u64
}

/// Energy unit in joules: MSR_RAPL_POWER_UNIT bits 12:8 (Energy Status
/// Units — NOT bits 3:0, those are the Power Units for the limit registers;
/// using them inflates energy 1024x on 0xA0E03).
pub(crate) fn energy_unit_j(power_unit: u64) -> f64 {
    1.0 / f64::from(1u32 << ((power_unit >> 8) & 0x1F))
}

/// Power unit in watts: MSR_RAPL_POWER_UNIT bits 3:0 (Power Units — the
/// scale for the 0x610 LIMIT registers, distinct from the energy unit
/// above). On the reference machine raw=0xA0E03 → 1/8 W.
pub(crate) fn power_unit_w(power_unit: u64) -> f64 {
    1.0 / f64::from(1u32 << (power_unit & 0xF))
}

/// MSR_PKG_POWER_LIMIT (0x610) decode: PL1 = raw[14:0] × unit,
/// PL2 = raw[46:32] × unit. This is also step 2 of the 0x29 three-step
/// verification runbook (§25): a 0x29 write must show up here.
pub(crate) fn pkg_power_limits_w(raw: u64, unit_w: f64) -> (f64, f64) {
    let pl1 = (raw & 0x7FFF) as f64 * unit_w;
    let pl2 = ((raw >> 32) & 0x7FFF) as f64 * unit_w;
    (pl1, pl2)
}

/// Package temperature: TjMax (0x1A2[23:16]) minus delta (0x19C[22:16]).
/// Bit 31 of 0x19C must be set (reading valid) — caller checks.
pub(crate) fn pkg_temp_c(tjmax_raw: u64, therm_raw: u64) -> Option<f32> {
    if therm_raw & (1 << 31) == 0 {
        return None;
    }
    let tjmax = ((tjmax_raw >> 16) & 0xFF) as i32;
    let delta = ((therm_raw >> 16) & 0x7F) as i32;
    let t = tjmax - delta;
    (tjmax > 0 && t > -20 && t <= 150).then_some(t as f32)
}

/// Effective clock: tsc × ΔAPERF/ΔMPERF between two (MPERF, APERF) reads.
///
/// The coordinator thread is pinned to one core (see telemetry/mod.rs), so
/// consecutive reads are same-core and the ratio is honest. The clamp is
/// therefore only a physical-plausibility envelope for this CPU: honest
/// ratios span ~0.36 (800 MHz floor) to ~2.45 (5.4 GHz single-core turbo)
/// on a 2.2 GHz TSC — anything outside 0.05..=3.0 is garbage (counter
/// freeze, migration after a pin failure) and is discarded, not published.
pub(crate) fn effective_clock_mhz(tsc_mhz: u32, prev: (u64, u64), now: (u64, u64)) -> Option<f64> {
    let dmperf = now.0.wrapping_sub(prev.0);
    let daperf = now.1.wrapping_sub(prev.1);
    if dmperf == 0 || daperf == 0 {
        return None;
    }
    let ratio = daperf as f64 / dmperf as f64;
    ((0.05..=3.0).contains(&ratio)).then_some(tsc_mhz as f64 * ratio)
}

/// Locate the signed IntelMSR module image (runtime data file, LGPL —
/// shipped alongside its COPYING text, never embedded in the binary).
pub(crate) fn intelmsr_image() -> Result<Vec<u8>, PlatformError> {
    module_image("PHELPER_INTELMSR", "IntelMSR.bin")
}

fn module_image(env_var: &str, file: &str) -> Result<Vec<u8>, PlatformError> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(p) = std::env::var(env_var) {
        candidates.push(p.into());
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("assets").join("pawnio").join(file));
        candidates.push(
            dir.join("..")
                .join("..")
                .join("assets")
                .join("pawnio")
                .join(file),
        );
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("assets").join("pawnio").join(file));
    }
    for path in &candidates {
        if let Ok(bytes) = std::fs::read(path) {
            return Ok(bytes);
        }
    }
    Err(PlatformError::Driver(format!(
        "{file} not found (set {env_var} or run from repo root)"
    )))
}

// ---------------------------------------------------------------------------
// IntelMCHBAR module surface.
// ---------------------------------------------------------------------------
//
// Read-only BY CONSTRUCTION: the signed module exports exactly three ioctls
// (source-verified 2026-08-26 against namazso/PawnIO.Modules IntelMCHBAR.p)
// and NO write ioctls at all:
//   ioctl_read_dword     in[0]=offset → out[0]=value (4-aligned, bounds-checked)
//   ioctl_read_qword     in[0]=offset → out[0]=value (8-aligned, bounds-checked)
//   ioctl_get_mchbar_addr (no input)  → out[0]=physical MCHBAR base
// The module itself resolves the base from PCI 0/0/0 config 0x48/0x4C
// (MCHBAREN-checked), enforces a CPU allow-list (Raptor Lake accepted),
// and bounds-checks offsets against the generation's window (0x10000 on RPL).

/// Read one 32-bit dword at `offset` into the MCHBAR window.
pub(crate) fn mchbar_read_dword(io: &PawnIo, offset: u32) -> Result<u32, PlatformError> {
    let mut out = [0i64; 1];
    io.execute("ioctl_read_dword", &[i64::from(offset)], &mut out)?;
    Ok(out[0] as u32)
}

/// Read one 64-bit qword at `offset` into the MCHBAR window.
pub(crate) fn mchbar_read_qword(io: &PawnIo, offset: u32) -> Result<u64, PlatformError> {
    let mut out = [0i64; 1];
    io.execute("ioctl_read_qword", &[i64::from(offset)], &mut out)?;
    Ok(out[0] as u64)
}

/// Physical MCHBAR base as resolved by the module (diagnostics).
pub(crate) fn mchbar_base_addr(io: &PawnIo) -> Result<u64, PlatformError> {
    let mut out = [0i64; 1];
    io.execute("ioctl_get_mchbar_addr", &[], &mut out)?;
    Ok(out[0] as u64)
}

/// Locate the signed IntelMCHBAR module image (same LGPL runtime-data rules
/// as IntelMSR.bin; override path via PHELPER_INTELMCHBAR).
pub(crate) fn mchbar_image() -> Result<Vec<u8>, PlatformError> {
    module_image("PHELPER_INTELMCHBAR", "IntelMCHBAR.bin")
}

/// Decode one power-limit field (bits 14:0, ×unit) — the layout MSR 0x610
/// uses for PL1/PL2, reused by the MCHBAR power-block scan: candidate PL4
/// fields share this encoding on Intel client platforms.
pub(crate) fn power_limit_field_w(raw: u32, unit_w: f64) -> f64 {
    (raw & 0x7FFF) as f64 * unit_w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_wrap_compensation() {
        // counter wrapped from 0xFFFFFFF0 to 0x00000010 → delta 0x20
        assert_eq!(delta_energy(0xFFFF_FFF0, 0x10), 0x20);
    }

    #[test]
    fn energy_unit_math() {
        // raw 0xA0E03 (typical Raptor Lake): energy status units bits 12:8
        // = 0xE → 1/16384 J ≈ 61 µJ
        assert!((energy_unit_j(0xA0E03) - 1.0 / 16384.0).abs() < f64::EPSILON);
        assert!((energy_unit_j(0x0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn power_unit_math() {
        // raw 0xA0E03: power units bits 3:0 = 3 → 1/8 W.
        assert!((power_unit_w(0xA0E03) - 0.125).abs() < f64::EPSILON);
        assert!((power_unit_w(0x0) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pkg_power_limits_decode() {
        // unit=1/8 W: PL1 raw 440 → 55 W, PL2 raw 880 → 110 W.
        let raw: u64 = 440 | (880u64 << 32);
        let (pl1, pl2) = pkg_power_limits_w(raw, 0.125);
        assert!((pl1 - 55.0).abs() < f64::EPSILON);
        assert!((pl2 - 110.0).abs() < f64::EPSILON);
    }

    #[test]
    fn power_limit_field_decode() {
        // Factory PL4 on 8BAB: 200 W at 1/8 W units → field 1600 = 0x640.
        assert!((power_limit_field_w(0x640, 0.125) - 200.0).abs() < f64::EPSILON);
        assert!((power_limit_field_w(0, 0.125)).abs() < f64::EPSILON);
        // Only bits 14:0 participate (enable/lock bits masked out).
        assert!((power_limit_field_w(0x8000_0640, 0.125) - 200.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pkg_temp_decode() {
        // tjmax=100, delta=25, valid bit set → 75°C
        let tjmax = 100u64 << 16;
        let therm = (1u64 << 31) | (25u64 << 16);
        assert_eq!(pkg_temp_c(tjmax, therm), Some(75.0));
        // valid bit clear → None
        assert_eq!(pkg_temp_c(tjmax, 25u64 << 16), None);
    }

    #[test]
    fn effective_clock_math() {
        // aperf advances half of mperf → 0.5 × tsc
        assert_eq!(
            effective_clock_mhz(2200, (1000, 500), (2000, 1000)),
            Some(1100.0)
        );
        // full ratio → tsc
        assert_eq!(
            effective_clock_mhz(2200, (0, 0), (1000, 1000)),
            Some(2200.0)
        );
        // single-core turbo (ratio 2.4 ≈ 5.3 GHz) is HONEST → admitted
        assert_eq!(
            effective_clock_mhz(2200, (0, 0), (1000, 2400)),
            Some(5280.0)
        );
        // physically impossible (ratio 4) → discarded
        assert_eq!(effective_clock_mhz(2200, (0, 0), (100, 400)), None);
        // mperf stall → None (no division by zero)
        assert_eq!(effective_clock_mhz(2200, (5, 5), (5, 9)), None);
    }
}
