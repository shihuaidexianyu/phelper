//! phelper-domain — pure domain model and ports.
//!
//! Dependency rules (architecture.md section 7): this crate MUST NOT depend on
//! UI or platform crates. Only serde/thiserror. All hardware-specific wire
//! knowledge lives in phelper-core's platform modules; this crate defines the
//! vocabulary they speak.

pub mod board;
pub mod capability;
pub mod command;
pub mod error;
pub mod hp;
pub mod identity;
pub mod policy;
pub mod ports;
pub mod state;
pub mod telemetry;
