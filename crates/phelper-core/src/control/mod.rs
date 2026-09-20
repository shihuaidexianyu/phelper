//! Control plane (M2): single-writer ControlCoordinator + journal +
//! safety supervisor + keep-alive. See architecture.md §30-34, §43-45.

mod coordinator;
mod drift;
mod fan_curve;
pub mod journal;
pub mod keepalive;
pub(crate) mod lease;
mod mux;
mod recovery;
pub mod safety;
mod verification;

pub use coordinator::ControlHandle;
pub(crate) use coordinator::{ControlConfig, ControlCoordinator, SnapshotFeed};
