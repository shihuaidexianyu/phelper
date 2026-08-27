//! Keep-alive service (§33.1): 8BAB firmware claws user thermal/fan state
//! back to automatic ~120 s after the last "someone is managing this"
//! signal. The kernel's answer is a 90 s heartbeat on 0x10 (fan-count-get);
//! ours is 60 s (margin for tick jitter) — and on the same tick we
//! re-assert every non-default TrustedWrite so a clawback that slipped
//! through is repaired, not just delayed.
//!
//! Lives INSIDE the coordinator thread (recv_timeout drives due()) — a
//! separate thread would be a second writer (AR-03).

use std::time::{Duration, Instant};

use phelper_domain::policy::{FanMode, ThermalMode};
use phelper_domain::state::ObservedState;

/// Heartbeat period. Firmware timeout ~120 s; 60 s keeps two full misses
/// of headroom even with worst-case tick drift.
pub const PERIOD: Duration = Duration::from_secs(60);

/// Consecutive heartbeat failures before failing closed (restore auto).
pub const FAIL_CLOSED_AFTER: u32 = 2;

/// Retry delay after a failed heartbeat (we may have just missed the
/// window; try again soon rather than waiting a full period).
pub const RETRY_DELAY: Duration = Duration::from_secs(5);

/// One trusted write that must be re-asserted on the heartbeat tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReAssert {
    ThermalMode,
    FanLevels,
    MaxFan,
    /// 0x29 power limits — not part of `tracked()` (that fn is pure over
    /// ObservedState and the "non-default" notion needs the coordinator's
    /// dirty flag); the coordinator appends this to its tracked set while
    /// `power_limits_dirty` is set (AC/DC transitions can make the firmware
    /// drop custom limits — the kernel re-actualizes on that event).
    PowerLimits,
}

#[derive(Debug)]
pub struct KeepAliveService {
    /// Heartbeat period (PERIOD in prod; tests inject a short one).
    period: Duration,
    /// When the next heartbeat is due. `None` = nothing tracked → idle.
    next_due: Option<Instant>,
    consecutive_failures: u32,
}

impl Default for KeepAliveService {
    fn default() -> Self {
        Self::new()
    }
}

impl KeepAliveService {
    pub fn new() -> Self {
        Self::with_period(PERIOD)
    }

    pub fn with_period(period: Duration) -> Self {
        Self {
            period,
            next_due: None,
            consecutive_failures: 0,
        }
    }

    /// Which trusted writes currently need keep-alive. Only NON-DEFAULT
    /// states count: Balanced thermal / firmware-auto fans / max-fan-off
    /// are what the firmware falls back to anyway, so a clawback of those
    /// is a no-op and heartbeating for them would be pure noise.
    pub fn tracked(observed: &ObservedState) -> Vec<ReAssert> {
        let mut v = Vec::new();
        if let Some(mode) = observed.thermal_mode.value()
            && *mode != ThermalMode::Balanced
        {
            v.push(ReAssert::ThermalMode);
        }
        if matches!(
            observed.fan_mode.value(),
            Some(FanMode::Manual(_) | FanMode::Curve(_))
        ) {
            v.push(ReAssert::FanLevels);
        }
        if matches!(observed.max_fan.value(), Some(true)) {
            v.push(ReAssert::MaxFan);
        }
        v
    }

    /// Recompute the schedule from the current tracked set. Call after any
    /// state change (user command, safety action, shutdown restore).
    pub fn reschedule(&mut self, observed: &ObservedState, now: Instant) {
        self.reschedule_tracked(&Self::tracked(observed), now);
    }

    /// Recompute the schedule from an explicitly computed tracked set (the
    /// coordinator's version — it can include coordinator-state items like
    /// PowerLimits that `tracked()` cannot see).
    pub fn reschedule_tracked(&mut self, tracked: &[ReAssert], now: Instant) {
        if tracked.is_empty() {
            self.next_due = None;
        } else {
            self.next_due = Some(now + self.period);
        }
    }

    pub fn is_due(&self, now: Instant) -> bool {
        self.next_due.is_some_and(|due| now >= due)
    }

    /// How long until the next heartbeat (for recv_timeout). Falls back to
    /// the long idle wait when nothing is tracked.
    pub fn until_due(&self, now: Instant, idle_wait: Duration) -> Duration {
        match self.next_due {
            Some(due) => due.saturating_duration_since(now),
            None => idle_wait,
        }
    }

    pub fn record_success(&mut self, now: Instant) {
        self.consecutive_failures = 0;
        self.next_due = Some(now + self.period);
    }

    /// Returns true when the failure streak reached the fail-closed
    /// threshold — the coordinator must then restore firmware auto (the
    /// heartbeat isn't landing; we may be the thing that's wrong).
    pub fn record_failure(&mut self, now: Instant) -> bool {
        self.consecutive_failures += 1;
        // Retry soon-ish rather than waiting a full period (clamped to the
        // period so test periods stay meaningful).
        self.next_due = Some(now + RETRY_DELAY.min(self.period));
        self.consecutive_failures >= FAIL_CLOSED_AFTER
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::policy::{FanCurve, FanLevels};
    use phelper_domain::state::ObservedValue;

    fn trusted<T>(value: T) -> ObservedValue<T> {
        ObservedValue::TrustedWrite {
            value,
            at: Instant::now(),
        }
    }

    #[test]
    fn tracked_matrix() {
        // Defaults → nothing tracked.
        let o = ObservedState::default();
        assert!(KeepAliveService::tracked(&o).is_empty());

        // Balanced thermal is the firmware fallback → not tracked.
        let mut o = ObservedState::default();
        o.thermal_mode = trusted(ThermalMode::Balanced);
        assert!(KeepAliveService::tracked(&o).is_empty());

        // Performance → tracked.
        o.thermal_mode = trusted(ThermalMode::Performance);
        assert_eq!(KeepAliveService::tracked(&o), vec![ReAssert::ThermalMode]);

        // Manual fan → tracked; auto fan → not.
        let mut o = ObservedState::default();
        o.fan_mode = trusted(FanMode::Manual(FanLevels::new(30, 30)));
        assert_eq!(KeepAliveService::tracked(&o), vec![ReAssert::FanLevels]);
        o.fan_mode = trusted(FanMode::FirmwareAuto);
        assert!(KeepAliveService::tracked(&o).is_empty());

        // A software curve owns the same manual 0x2E path and therefore
        // needs the same keep-alive signal.
        o.fan_mode = trusted(FanMode::Curve(FanCurve::balanced()));
        assert_eq!(KeepAliveService::tracked(&o), vec![ReAssert::FanLevels]);

        // Max fan on → tracked; off → not.
        let mut o = ObservedState::default();
        o.max_fan = trusted(true);
        assert_eq!(KeepAliveService::tracked(&o), vec![ReAssert::MaxFan]);
        o.max_fan = trusted(false);
        assert!(KeepAliveService::tracked(&o).is_empty());

        // All three at once.
        let mut o = ObservedState::default();
        o.thermal_mode = trusted(ThermalMode::Performance);
        o.fan_mode = trusted(FanMode::Manual(FanLevels::new(30, 30)));
        o.max_fan = trusted(true);
        assert_eq!(
            KeepAliveService::tracked(&o),
            vec![ReAssert::ThermalMode, ReAssert::FanLevels, ReAssert::MaxFan]
        );
    }

    #[test]
    fn reschedule_and_due_math() {
        let mut ka = KeepAliveService::new();
        let t0 = Instant::now();
        let mut o = ObservedState::default();
        o.max_fan = trusted(true);

        ka.reschedule(&o, t0);
        assert!(!ka.is_due(t0));
        assert!(ka.is_due(t0 + PERIOD));
        assert_eq!(ka.until_due(t0, Duration::from_secs(3600)), PERIOD);

        // Nothing tracked → idle wait.
        ka.reschedule(&ObservedState::default(), t0);
        assert_eq!(
            ka.until_due(t0, Duration::from_secs(3600)),
            Duration::from_secs(3600)
        );
        assert!(!ka.is_due(t0 + PERIOD));
    }

    #[test]
    fn fail_closed_after_two_failures() {
        let mut ka = KeepAliveService::new();
        let t0 = Instant::now();
        assert!(!ka.record_failure(t0));
        assert!(ka.record_failure(t0));
        ka.record_success(t0);
        assert!(!ka.record_failure(t0));
    }
}
