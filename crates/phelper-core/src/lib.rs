//! phelper-core — headless engine: capability probe, telemetry, control.
//!
//! Layering (architecture.md section 7): depends only on phelper-domain.
//! The future GPUI app consumes `Engine` + domain types and never sees
//! `wmi`/`windows` internals.

pub mod capability;
#[cfg(feature = "control")]
pub mod control;
mod engine;
pub mod persistence;
mod platform;
pub mod smoke;
pub mod telemetry;

pub use engine::Engine;
pub use platform::ogh_watch::{OghFinding, OghFindingKind};
pub use phelper_domain as domain;
