use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Structured control errors (architecture.md section 34). The UI maps the
/// variant to a human sentence and may expand the context as technical
/// detail — raw HRESULTs never cross this boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlError {
    #[error("unsupported on this machine")]
    Unsupported,
    #[error("unsafe request: {reason}")]
    UnsafeRequest { reason: String },
    #[error("permission denied (elevation required)")]
    PermissionDenied,
    #[error("driver unavailable: {what}")]
    DriverUnavailable { what: String },
    #[error("firmware rejected the request: {detail}")]
    FirmwareRejected { detail: String },
    #[error("verification failed: expected {expected}, read back {actual}")]
    VerificationFailed { expected: String, actual: String },
    #[error("timed out")]
    Timeout,
    #[error("backend unavailable: {what}")]
    BackendUnavailable { what: String },
    #[error("another control operation is in progress")]
    Busy,
}

/// Engine init/probe failures.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("device identity probe failed: {0}")]
    IdentityFailed(String),
    #[error("WMI unavailable: {0}")]
    WmiUnavailable(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("persistence error: {0}")]
    Persistence(String),
    #[error("channel closed: {0}")]
    ChannelClosed(&'static str),
}

/// HP WMI transport/adapter errors (low-level; mapped to ControlError once,
/// at the ControlCoordinator boundary).
#[derive(Debug, Error)]
pub enum HpWmiError {
    #[error("WMI transport failure: {0}")]
    Transport(String),
    #[error("WMI method returned {code}")]
    MethodReturnCode { code: u32 },
    /// BIOS-level return codes (hp-wmi.c bios_return): 0 ok, 2 bad signature,
    /// 3 unknown command, 4 unknown command type, 5 invalid parameters.
    #[error("firmware return code {code}")]
    FirmwareReturnCode { code: u32 },
    #[error("invalid response: {0}")]
    InvalidResponse(&'static str),
    /// Pre-write input failed the encoder's own guard (the safety layer
    /// should have caught this first — fail closed regardless).
    #[error("invalid input: {0}")]
    InvalidInput(&'static str),
    #[error("probe failed: {0}")]
    ProbeFailed(&'static str),
    #[error("operation timed out")]
    Timeout,
    #[error("backend not available: {0}")]
    NotAvailable(&'static str),
}

impl HpWmiError {
    /// Map BIOS return code to error (call sites check `code == 0` first).
    pub fn from_firmware_code(code: u32) -> Self {
        HpWmiError::FirmwareReturnCode { code }
    }
}

/// Generic platform-adapter errors (PawnIO, NVAPI, PowrProf, PDH).
#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("not available: {0}")]
    NotAvailable(&'static str),
    #[error("driver error: {0}")]
    Driver(String),
    #[error("OS API error: {0}")]
    Os(String),
    #[error("unexpected data: {0}")]
    Data(&'static str),
}
