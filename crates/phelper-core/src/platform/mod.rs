//! Platform adapters (Windows-only). Each module implements domain ports;
//! nothing here is visible to the outside except through those ports.

pub(crate) mod elevation;
pub(crate) mod hp_wmi;
pub(crate) mod identity;
#[cfg(feature = "nvidia")]
pub(crate) mod nvidia;
// pub within the crate-private platform tree: OghFinding is re-exported
// at the crate root for the Engine API.
pub mod ogh_watch;
#[cfg(feature = "pawnio")]
pub(crate) mod pawnio;
pub(crate) mod windows_pdh;
pub(crate) mod windows_power;
pub(crate) mod windows_ppm;
pub(crate) mod wmi_util;
