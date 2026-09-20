//! phelper-core — headless engine: capability probe, telemetry, control.
//!
//! Layering (architecture.md section 7): depends only on phelper-domain.
//! The future GPUI app consumes `Engine` + domain types and never sees
//! `wmi`/`windows` internals.

pub mod app;
pub mod automatic_scheduler;
#[cfg(feature = "control")]
pub mod automation;
pub mod capability;
#[cfg(feature = "control")]
pub mod control;
mod engine;
pub mod hardware_status;
pub mod measurements;
pub mod os_policy;
pub mod persistence;
mod platform;
pub mod profiles;
pub mod smoke;
pub mod telemetry;

pub use engine::Engine;
pub use phelper_domain as domain;
pub use platform::elevation;
pub use platform::ogh_watch::{OghFinding, OghFindingKind};
