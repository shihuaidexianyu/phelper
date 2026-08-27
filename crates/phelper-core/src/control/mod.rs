//! Control plane (M2): single-writer ControlCoordinator + journal +
//! safety supervisor + keep-alive. See architecture.md §30-34, §43-45.

mod coordinator;
mod fan_curve;
pub mod journal;
pub mod keepalive;
pub mod safety;

pub use coordinator::ControlHandle;
#[allow(unused_imports)] // wired in W15 (engine)
pub(crate) use coordinator::{ControlConfig, ControlCoordinator, SnapshotFeed};
