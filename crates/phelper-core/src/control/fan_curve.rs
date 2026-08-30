//! Software fan-curve scheduling.
//!
//! The curve evaluator is deliberately small and stateful. Telemetry arrives
//! more often than the firmware fan command should be written, so the
//! coordinator smooths the controlling temperature and only emits a new
//! target after a one-second quiet period and a meaningful level change.

use std::time::{Duration, Instant};

use phelper_domain::policy::{FanCurve, FanLevels};

/// Curve control never flies blind for longer than the manual-fan pre-write
/// window. The safety watchdog has a much longer 90 s backstop for a frozen
/// stream; this shorter value only decides whether a new target is eligible.
pub(crate) const TEMP_FRESH_FOR_CURVE: Duration = Duration::from_secs(5);
/// The HP firmware accepts 100-RPM levels, but writing on every telemetry
/// sample makes the fan hunt and blocks other control work. One write/second
/// matches the board's documented fan cadence.
pub(crate) const MIN_WRITE_INTERVAL: Duration = Duration::from_secs(1);
/// Ignore sub-level changes after interpolation/rounding; a one-level change
/// is the smallest meaningful V1 command.
pub(crate) const MIN_LEVEL_CHANGE: u16 = 1;
const TEMPERATURE_SMOOTHING: f64 = 0.5;

#[derive(Debug, Default)]
pub(crate) struct FanCurveController {
    last_target: Option<FanLevels>,
    smoothed_temp_c: Option<f64>,
    last_write_at: Option<Instant>,
}

impl FanCurveController {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn reset(&mut self, target: FanLevels, temp_c: f64, now: Instant) {
        self.last_target = Some(target);
        self.smoothed_temp_c = Some(temp_c);
        self.last_write_at = Some(now);
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn last_target(&self) -> Option<FanLevels> {
        self.last_target
    }

    /// Return the next hardware target, if this tick is allowed to write one.
    /// CPU temperature is required; GPU temperature is included when it is
    /// fresh, so a GPU-heavy workload can raise both fans even when CPU load
    /// is modest. A stale/missing GPU sample falls back to the fresh CPU
    /// sample and is still covered by the CPU safety watchdog.
    pub(crate) fn next_target(
        &mut self,
        curve: &FanCurve,
        cpu: Option<(f64, Instant)>,
        gpu: Option<(f64, Instant)>,
        now: Instant,
    ) -> Option<(FanLevels, f64)> {
        let temp_c = effective_temperature(cpu, gpu, now)?;
        let smoothed = match self.smoothed_temp_c {
            Some(previous) => previous + (temp_c - previous) * TEMPERATURE_SMOOTHING,
            None => temp_c,
        };
        self.smoothed_temp_c = Some(smoothed);
        let target = curve.target_at(smoothed);

        let changed_enough = self.last_target.is_none_or(|last| {
            level_delta(last.left, target.left) >= MIN_LEVEL_CHANGE
                || level_delta(last.right, target.right) >= MIN_LEVEL_CHANGE
        });
        if !changed_enough
            || self
                .last_write_at
                .is_some_and(|at| now.saturating_duration_since(at) < MIN_WRITE_INTERVAL)
        {
            return None;
        }
        Some((target, smoothed))
    }

    pub(crate) fn record_write(&mut self, target: FanLevels, now: Instant) {
        self.last_target = Some(target);
        self.last_write_at = Some(now);
    }

    pub(crate) fn record_failure(&mut self, now: Instant) {
        // Keep a failed backend from being hammered once per coordinator
        // tick, while leaving last_target untouched so a later retry still
        // knows what the hardware should receive.
        self.last_write_at = Some(now);
    }
}

pub(crate) fn effective_temperature(
    cpu: Option<(f64, Instant)>,
    gpu: Option<(f64, Instant)>,
    now: Instant,
) -> Option<f64> {
    let cpu = fresh_finite(cpu, now)?;
    let gpu = fresh_finite(gpu, now);
    Some(gpu.map_or(cpu, |gpu| cpu.max(gpu)))
}

fn fresh_finite(sample: Option<(f64, Instant)>, now: Instant) -> Option<f64> {
    let (value, at) = sample?;
    (value.is_finite() && now.saturating_duration_since(at) <= TEMP_FRESH_FOR_CURVE)
        .then_some(value)
}

fn level_delta(left: u16, right: u16) -> u16 {
    left.abs_diff(right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phelper_domain::policy::FanCurve;

    fn curve() -> FanCurve {
        FanCurve::new([
            phelper_domain::policy::FanCurvePoint::new(40, 20, 20),
            phelper_domain::policy::FanCurvePoint::new(60, 30, 30),
            phelper_domain::policy::FanCurvePoint::new(75, 45, 42),
            phelper_domain::policy::FanCurvePoint::new(90, 55, 55),
        ])
    }

    #[test]
    fn uses_the_hottest_fresh_cpu_or_gpu_sample() {
        let now = Instant::now();
        assert_eq!(
            effective_temperature(Some((55.0, now)), Some((70.0, now)), now,),
            Some(70.0)
        );
        assert_eq!(
            effective_temperature(
                Some((55.0, now)),
                Some((70.0, now - TEMP_FRESH_FOR_CURVE - Duration::from_millis(1))),
                now,
            ),
            Some(55.0)
        );
    }

    #[test]
    fn rate_limits_and_smooths_curve_updates() {
        let now = Instant::now();
        let mut controller = FanCurveController::new();
        controller.reset(FanLevels::new(20, 20), 40.0, now);

        // The first small rise is below the one-second write window.
        assert!(
            controller
                .next_target(&curve(), Some((42.0, now)), Some((42.0, now)), now)
                .is_none()
        );

        // After the window, the smoothed temperature still moves toward the
        // new value rather than jumping straight to the endpoint.
        let update = controller.next_target(
            &curve(),
            Some((75.0, now + Duration::from_secs(1))),
            Some((75.0, now + Duration::from_secs(1))),
            now + Duration::from_secs(1),
        );
        assert_eq!(
            update.map(|(levels, _)| levels),
            Some(FanLevels::new(29, 29))
        );
        controller.record_write(FanLevels::new(29, 29), now + Duration::from_secs(1));
        assert_eq!(controller.last_target(), Some(FanLevels::new(29, 29)));
    }

    #[test]
    fn missing_cpu_sample_never_produces_a_target() {
        let now = Instant::now();
        let mut controller = FanCurveController::new();
        assert!(
            controller
                .next_target(&curve(), None, Some((80.0, now)), now)
                .is_none()
        );
    }
}
