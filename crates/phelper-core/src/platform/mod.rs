//! Platform adapters (Windows-only). Each module implements domain ports;
//! nothing here is visible to the outside except through those ports.

// pub like ogh_watch: the desktop shell needs the elevation gate + runas
// relaunch for its self-elevation startup (M6; gpui.lib owns RT_MANIFEST,
// so a static requireAdministrator manifest is impossible with gpui).
pub mod elevation;
pub(crate) mod hp_wmi;
pub(crate) mod identity;
#[cfg(feature = "nvidia")]
pub(crate) mod nvidia;
// pub within the crate-private platform tree: OghFinding is re-exported
// at the crate root for the Engine API.
pub mod ogh_watch;
#[cfg(feature = "pawnio")]
pub(crate) mod pawnio;
pub(crate) mod windows_os_policy;
pub(crate) mod windows_pdh;
pub(crate) mod windows_power;
pub(crate) mod windows_ppm;
#[cfg(windows)]
pub(crate) mod wmi_util;
