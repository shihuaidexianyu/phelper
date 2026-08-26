//! Application layer (architecture.md §41–§44): the toolkit-agnostic bridge
//! between the GPUI shell and the engine. GPUI only ever (a) reads an
//! immutable `AppState` snapshot and (b) enqueues commands through
//! `AppHandle` (AR-01/§43). This module contains NO GPUI imports and no
//! hardware calls — the pump (`runtime`, `control` feature) is the only
//! place that talks to `Engine`/`ControlHandle`.
//!
//! Split by compile lane so the pure logic tests run in the default
//! feature set:
//! - always: `state` (read model + reducers), `coalesce` (§44 slider
//!   coalescing), `validate` (client-side pre-dispatch gates mirroring the
//!   CLI), `fmt` (zh-CN §34 presentation), `settings` (UiSettings TOML).
//! - `control` feature: `runtime` (AppHandle + pump thread), `journal_tail`
//!   (incremental JSONL reader), plus the `journal_tail` field on AppState.

pub mod coalesce;
pub mod fmt;
pub mod settings;
pub mod state;
pub mod validate;

#[cfg(feature = "control")]
pub mod journal_tail;
#[cfg(feature = "control")]
pub mod report;
#[cfg(feature = "control")]
pub mod runtime;

pub use state::{
    AppState, EngineStatus, ExperimentalUi, KnobId, KnobStatus, OutcomeRecord, ProfileSummary,
};

/// Compile-time half of the experimental double gate (the runtime half is
/// `caps.power_limits == Experimental`). True when this build of
/// phelper-core can encode 0x29 writes at all.
pub const EXPERIMENTAL_COMPILED: bool = cfg!(feature = "experimental-hp-power-limits");

/// Wall-clock epoch milliseconds (journal/evidence timestamps).
pub fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
