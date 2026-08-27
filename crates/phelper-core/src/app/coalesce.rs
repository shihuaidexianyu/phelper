//! §44 command coalescing — the Application layer's one sanctioned place to
//! drop user intent: a slider drag produces a stream of values, only the
//! LATEST may ever reach the coordinator. Guarantees:
//!
//! - per-knob latest-wins: enqueue 40→41→42→43, only 43 is dispatched;
//! - per-knob serial: a knob with an in-flight command queues, never
//!   overlaps itself (different knobs dispatch independently);
//! - rate limit: firmware-sensitive knobs remain at `MIN_INTERVAL` (250 ms),
//!   while CPU policy sliders use `FAST_INTERVAL` (50 ms); per-knob serial
//!   dispatch still prevents a held slider from filling the coordinator;
//! - Busy backoff: a `Busy` dispatch error re-stages the command after
//!   `BUSY_BACKOFF` (300 ms); only `BUSY_TIMEOUT` (5 s) of continuous
//!   Busy fails the knob.
//! - idempotent dedup: a command identical to the last ACCEPTED one for
//!   the same knob is dropped. Remote-desktop pointer streams (and held
//!   drags) re-emit the same slider value continuously — without this the
//!   coalescer re-dispatches it every interval forever (observed on-device
//!   M6-D6: an 8× identical-EPP storm from a remote pointer feed). A
//!   failed outcome clears the memory so the user can retry the same
//!   value; ApplyProfile is exempt (re-apply is a meaningful re-assert,
//!   not a value).
//!
//! Pure logic with an injected clock — the pump (runtime.rs) is the only
//! driver and calls everything from its single thread.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use phelper_domain::command::{ControlCommand, ControlReceipt};

use super::state::KnobId;

pub const MIN_INTERVAL: Duration = Duration::from_millis(250);
/// CPU policy writes are ordinary Windows power-policy calls on this
/// machine (roughly 80–130 ms end-to-end in the journal), so the old global
/// 250 ms gate made a drag feel slower than the backend. The in-flight guard
/// remains the hard serialization boundary.
pub const FAST_INTERVAL: Duration = Duration::from_millis(50);
pub const BUSY_BACKOFF: Duration = Duration::from_millis(300);
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Slot {
    /// Latest user intent not yet dispatched.
    pending: Option<ControlCommand>,
    /// Receipt of the dispatched command awaiting its outcome.
    in_flight: Option<ControlReceipt>,
    /// Last command the coordinator ACCEPTED (dedup memory; cleared on a
    /// failed outcome so the same value can be retried). Known v0.1
    /// limitation: an out-of-band change (e.g. CLI) is not reconciled —
    /// re-sending the then-stale value is dropped until a different value
    /// passes once.
    last_dispatched: Option<ControlCommand>,
    /// Rate-limit / backoff gate: no dispatch before this instant.
    next_attempt_at: Option<Instant>,
    /// When continuous Busy started (None = not busy-backed-off).
    busy_since: Option<Instant>,
}

/// What the pump should do after a `Busy` dispatch error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyVerdict {
    /// Command re-staged; retry after the backoff.
    Retry,
    /// Busy persisted past the timeout — fail the knob.
    TimedOut,
}

pub struct Coalescer {
    slots: BTreeMap<KnobId, Slot>,
    min_interval: Duration,
    fast_interval: Duration,
    busy_backoff: Duration,
    busy_timeout: Duration,
}

impl Default for Coalescer {
    fn default() -> Self {
        Self::new()
    }
}

impl Coalescer {
    pub fn new() -> Self {
        Self {
            slots: BTreeMap::new(),
            min_interval: MIN_INTERVAL,
            fast_interval: FAST_INTERVAL,
            busy_backoff: BUSY_BACKOFF,
            busy_timeout: BUSY_TIMEOUT,
        }
    }

    pub fn with_intervals(
        min_interval: Duration,
        busy_backoff: Duration,
        busy_timeout: Duration,
    ) -> Self {
        Self {
            slots: BTreeMap::new(),
            min_interval,
            // Test helper: custom interval keeps all knobs on one injected
            // clock policy, preserving deterministic timing tests.
            fast_interval: min_interval,
            busy_backoff,
            busy_timeout,
        }
    }

    fn interval_for(&self, knob: KnobId) -> Duration {
        match knob {
            KnobId::EppAc
            | KnobId::EppDc
            | KnobId::Epp1Ac
            | KnobId::Epp1Dc
            | KnobId::MaxFreqAc
            | KnobId::MaxFreqDc
            | KnobId::Boost => self.fast_interval,
            KnobId::ThermalMode
            | KnobId::FanMode
            | KnobId::GpuPolicy
            | KnobId::PowerLimits
            | KnobId::Profile => self.min_interval,
        }
    }

    /// Record the newest user intent for a knob. Replaces any undispatched
    /// pending command (latest-wins); never touches an in-flight one.
    /// Drops the intent when it is identical to the last accepted command
    /// for this knob (idempotent dedup — ApplyProfile exempt).
    ///
    /// Returns `true` when the command became pending. A duplicate returns
    /// `false` so the application layer can leave the existing lifecycle
    /// state alone instead of showing a stale Pending badge.
    pub fn enqueue(&mut self, knob: KnobId, cmd: ControlCommand) -> bool {
        let slot = self.slots.entry(knob).or_default();
        if knob != KnobId::Profile && slot.last_dispatched.as_ref() == Some(&cmd) {
            return false;
        }
        slot.pending = Some(cmd);
        true
    }

    /// Commands eligible to dispatch right now: pending, nothing in flight,
    /// rate-limit/backoff window expired. Takes the pending command out —
    /// the pump MUST follow up with exactly one of note_dispatched /
    /// note_busy / note_dispatch_error for each returned knob.
    pub fn poll(&mut self, now: Instant) -> Vec<(KnobId, ControlCommand)> {
        let mut out = Vec::new();
        for (knob, slot) in &mut self.slots {
            if slot.pending.is_none() || slot.in_flight.is_some() {
                continue;
            }
            if slot.next_attempt_at.is_some_and(|t| now < t) {
                continue;
            }
            out.push((*knob, slot.pending.take().expect("checked above")));
        }
        out
    }

    /// Whether the pump should keep a short receive timeout. An active slot
    /// means either an outcome may arrive or a latest-wins value may become
    /// eligible soon; sleeping for the idle 100 ms cadence here makes the
    /// second step of a drag visibly lag behind the first.
    pub fn has_work(&self) -> bool {
        self.slots
            .values()
            .any(|slot| slot.pending.is_some() || slot.in_flight.is_some())
    }

    /// Dispatch accepted by the coordinator. The accepted command is
    /// recorded in the dedup memory (poll took it out of the slot, so the
    /// pump hands it back here).
    pub fn note_dispatched(
        &mut self,
        knob: KnobId,
        cmd: ControlCommand,
        receipt: ControlReceipt,
        now: Instant,
    ) {
        let interval = self.interval_for(knob);
        let slot = self.slots.entry(knob).or_default();
        slot.in_flight = Some(receipt);
        slot.last_dispatched = Some(cmd);
        slot.next_attempt_at = Some(now + interval);
        slot.busy_since = None;
    }

    /// Dispatch refused with `Busy`: re-stage the command and back off.
    pub fn note_busy(&mut self, knob: KnobId, cmd: ControlCommand, now: Instant) -> BusyVerdict {
        let slot = self.slots.entry(knob).or_default();
        let since = *slot.busy_since.get_or_insert(now);
        if now.duration_since(since) >= self.busy_timeout {
            slot.pending = None;
            slot.busy_since = None;
            return BusyVerdict::TimedOut;
        }
        slot.pending = Some(cmd);
        slot.next_attempt_at = Some(now + self.busy_backoff);
        BusyVerdict::Retry
    }

    /// Dispatch refused with any other error: the command is dead (the pump
    /// records the knob failure); nothing re-staged.
    pub fn note_dispatch_error(&mut self, knob: KnobId) {
        let slot = self.slots.entry(knob).or_default();
        slot.busy_since = None;
    }

    /// Outcome received for the in-flight command: the knob is free for
    /// the next pending intent (rate limit still applies). `succeeded`
    /// false clears the dedup memory so the user can retry the same value.
    pub fn note_completed(&mut self, knob: KnobId, succeeded: bool) {
        let slot = self.slots.entry(knob).or_default();
        slot.in_flight = None;
        slot.busy_since = None;
        if !succeeded {
            slot.last_dispatched = None;
        }
    }

    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.slots.values().filter(|s| s.pending.is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::policy::ThermalMode;

    fn cmd() -> ControlCommand {
        ControlCommand::SetThermalMode(ThermalMode::Balanced)
    }

    /// A DIFFERENT value — dedup (rightly) drops identical re-enqueues
    /// after an accepted dispatch; timing tests need distinct values.
    fn cmd2() -> ControlCommand {
        ControlCommand::SetThermalMode(ThermalMode::Performance)
    }

    fn receipt(n: u64) -> ControlReceipt {
        ControlReceipt(n)
    }

    #[test]
    fn latest_wins_collapses_drag() {
        let mut c = Coalescer::new();
        let t0 = Instant::now();
        for _ in 0..4 {
            c.enqueue(KnobId::EppAc, cmd());
        }
        let ready = c.poll(t0);
        assert_eq!(ready.len(), 1, "4 enqueues must collapse to 1 dispatch");
        assert_eq!(c.pending_count(), 0);
    }

    #[test]
    fn first_dispatch_is_immediate() {
        let mut c = Coalescer::new();
        c.enqueue(KnobId::EppAc, cmd());
        assert_eq!(c.poll(Instant::now()).len(), 1);
    }

    #[test]
    fn rate_limit_spaces_dispatches() {
        let mut c = Coalescer::with_intervals(MIN_INTERVAL, BUSY_BACKOFF, BUSY_TIMEOUT);
        let t0 = Instant::now();
        c.enqueue(KnobId::EppAc, cmd());
        let (k, _) = c.poll(t0).pop().unwrap();
        c.note_dispatched(k, cmd(), receipt(1), t0);

        c.enqueue(KnobId::EppAc, cmd2());
        // Too soon after the last dispatch: held.
        assert!(c.poll(t0 + Duration::from_millis(100)).is_empty());
        // After the outcome AND the interval: dispatched.
        c.note_completed(k, true);
        assert_eq!(c.poll(t0 + Duration::from_millis(251)).len(), 1);
    }

    #[test]
    fn cpu_policy_rate_limit_is_fast_but_still_serial() {
        let mut c = Coalescer::new();
        let t0 = Instant::now();
        c.enqueue(KnobId::EppAc, cmd());
        let (k, cmd1) = c.poll(t0).pop().unwrap();
        c.note_dispatched(k, cmd1, receipt(1), t0);
        c.enqueue(KnobId::EppAc, cmd2());
        c.note_completed(k, true);
        assert!(c.poll(t0 + Duration::from_millis(49)).is_empty());
        assert_eq!(c.poll(t0 + FAST_INTERVAL).len(), 1);
    }

    #[test]
    fn firmware_knobs_keep_conservative_rate_limit() {
        let mut c = Coalescer::new();
        let t0 = Instant::now();
        c.enqueue(KnobId::FanMode, cmd());
        let (k, cmd1) = c.poll(t0).pop().unwrap();
        c.note_dispatched(k, cmd1, receipt(1), t0);
        c.enqueue(KnobId::FanMode, cmd2());
        c.note_completed(k, true);
        assert!(c.poll(t0 + Duration::from_millis(249)).is_empty());
        assert_eq!(c.poll(t0 + MIN_INTERVAL).len(), 1);
    }

    #[test]
    fn in_flight_serializes_per_knob() {
        let mut c = Coalescer::new();
        let t0 = Instant::now();
        c.enqueue(KnobId::EppAc, cmd());
        let (k, _) = c.poll(t0).pop().unwrap();
        c.note_dispatched(k, cmd(), receipt(1), t0);

        // New intent while in flight: queued, not dispatched — even long
        // after the rate-limit window.
        c.enqueue(KnobId::EppAc, cmd2());
        assert!(c.poll(t0 + Duration::from_secs(60)).is_empty());
        c.note_completed(k, true);
        assert_eq!(c.poll(t0 + Duration::from_secs(60)).len(), 1);
    }

    #[test]
    fn knobs_are_independent() {
        let mut c = Coalescer::new();
        let t0 = Instant::now();
        c.enqueue(KnobId::EppAc, cmd());
        let (k, _) = c.poll(t0).pop().unwrap();
        c.note_dispatched(k, cmd(), receipt(1), t0);
        // A different knob dispatches immediately even while EppAc is
        // in flight and inside its rate-limit window.
        c.enqueue(KnobId::EppDc, cmd());
        assert_eq!(c.poll(t0 + Duration::from_millis(10)).len(), 1);
    }

    #[test]
    fn busy_retries_then_times_out() {
        let mut c = Coalescer::with_intervals(
            Duration::from_millis(250),
            Duration::from_millis(300),
            Duration::from_secs(2),
        );
        let t0 = Instant::now();
        c.enqueue(KnobId::EppAc, cmd());
        let (k, cmd1) = c.poll(t0).pop().unwrap();

        // Busy at t0: re-staged, retry not before t0+300ms.
        assert_eq!(c.note_busy(k, cmd1, t0), BusyVerdict::Retry);
        assert!(c.poll(t0 + Duration::from_millis(100)).is_empty());
        let (k2, cmd2) = c
            .poll(t0 + Duration::from_millis(300))
            .pop()
            .expect("retry after backoff");
        assert_eq!(k2, k);

        // Still Busy at t0+300ms, but a NEWER intent arrived meanwhile —
        // latest-wins replaces the re-staged command, backoff continues.
        c.enqueue(k, cmd());
        assert_eq!(
            c.note_busy(k, cmd2, t0 + Duration::from_millis(300)),
            BusyVerdict::Retry
        );

        // Continuous Busy past the 2 s test timeout: TimedOut, slot cleared.
        let (_, cmd3) = c.poll(t0 + Duration::from_millis(600)).pop().unwrap();
        assert_eq!(
            c.note_busy(k, cmd3, t0 + Duration::from_secs(3)),
            BusyVerdict::TimedOut
        );
        assert_eq!(c.pending_count(), 0);
        // Slot recovered: a fresh enqueue dispatches immediately.
        c.enqueue(k, cmd());
        assert_eq!(c.poll(t0 + Duration::from_secs(3)).len(), 1);
    }

    #[test]
    fn successful_dispatch_clears_busy_clock() {
        let mut c = Coalescer::with_intervals(
            Duration::from_millis(250),
            Duration::from_millis(300),
            Duration::from_secs(1),
        );
        let t0 = Instant::now();
        c.enqueue(KnobId::EppAc, cmd());
        let (k, cmd1) = c.poll(t0).pop().unwrap();
        assert_eq!(c.note_busy(k, cmd1, t0), BusyVerdict::Retry);
        let (k, cmd_b) = c.poll(t0 + Duration::from_millis(900)).pop().unwrap();
        let _ = cmd_b;
        // Dispatch finally accepted well past the would-be timeout — the
        // busy clock must reset, knob lives.
        c.note_dispatched(k, cmd(), receipt(7), t0 + Duration::from_millis(900));
        c.note_completed(k, true);
        c.enqueue(k, cmd2());
        assert_eq!(c.poll(t0 + Duration::from_millis(1151)).len(), 1);
    }

    #[test]
    fn profile_knob_is_rate_limited_not_merged_away() {
        // ApplyProfile is not "merged" semantically, but the coalescer
        // treats it like any knob: rapid double-tap = one dispatch, then a
        // rate-limited second if still pending.
        let mut c = Coalescer::new();
        let t0 = Instant::now();
        let apply = || ControlCommand::ApplyProfile {
            profile: "balanced".into(),
        };
        c.enqueue(KnobId::Profile, apply());
        c.enqueue(KnobId::Profile, apply());
        let (k, _) = c.poll(t0).pop().unwrap();
        c.note_dispatched(k, apply(), receipt(1), t0);
        c.note_completed(k, true);
        assert!(
            c.poll(t0 + Duration::from_secs(1)).is_empty(),
            "no pending left"
        );
        // ...but a fresh explicit re-apply is NOT deduped (re-assert).
        c.enqueue(KnobId::Profile, apply());
        assert_eq!(c.poll(t0 + Duration::from_secs(1)).len(), 1);
    }

    #[test]
    fn dedup_drops_identical_after_accept() {
        // The M6-D6 storm: a held slider (remote pointer stream) re-emits
        // the same value forever — only the first may reach the hardware.
        let mut c = Coalescer::new();
        let t0 = Instant::now();
        c.enqueue(KnobId::EppAc, cmd());
        let (k, cmd1) = c.poll(t0).pop().unwrap();
        c.note_dispatched(k, cmd1, receipt(1), t0);
        c.note_completed(k, true);
        for i in 1..=8u64 {
            c.enqueue(KnobId::EppAc, cmd());
            assert!(
                c.poll(t0 + Duration::from_millis(250 * i + 1)).is_empty(),
                "identical re-enqueue #{i} must be dropped"
            );
        }
        // A DIFFERENT value still passes.
        c.enqueue(
            KnobId::EppAc,
            ControlCommand::SetThermalMode(ThermalMode::Performance),
        );
        assert_eq!(c.poll(t0 + Duration::from_secs(3)).len(), 1);
    }

    #[test]
    fn dedup_allows_retry_after_failure() {
        let mut c = Coalescer::new();
        let t0 = Instant::now();
        c.enqueue(KnobId::EppAc, cmd());
        let (k, cmd1) = c.poll(t0).pop().unwrap();
        c.note_dispatched(k, cmd1, receipt(1), t0);
        c.note_completed(k, false); // e.g. verification failed
        c.enqueue(KnobId::EppAc, cmd());
        assert_eq!(
            c.poll(t0 + Duration::from_millis(251)).len(),
            1,
            "failed outcome clears the memory — same value retries"
        );
    }

    #[test]
    fn dedup_reports_that_no_new_work_was_queued() {
        let mut c = Coalescer::new();
        let t0 = Instant::now();
        assert!(c.enqueue(KnobId::EppAc, cmd()));
        let (k, cmd1) = c.poll(t0).pop().unwrap();
        c.note_dispatched(k, cmd1, receipt(1), t0);
        c.note_completed(k, true);

        assert!(!c.enqueue(KnobId::EppAc, cmd()));
        assert!(!c.has_work());
    }
}
