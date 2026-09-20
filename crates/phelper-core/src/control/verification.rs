//! Delayed readbacks are scheduled work, never sleeps on the control thread.
use std::time::{Duration, Instant};

use phelper_domain::command::Verification;
use phelper_domain::policy::{CpuPowerLimits, FanLevels, GpuPlatformPolicy};
use phelper_domain::ports::HpBackend;

use super::safety::ThermalFeed;

#[derive(Debug, Clone, Copy)]
pub(super) enum Target {
    Fan(FanLevels),
    Gpu(GpuPlatformPolicy, bool),
    Power(CpuPowerLimits),
}

pub(super) struct PendingVerification {
    pub target: Target,
    pub written_at: Instant,
    pub next_due: Instant,
    remaining: u32,
    interval: Duration,
    pub actual_target: Option<Target>,
}

impl PendingVerification {
    pub fn new(target: Target, written_at: Instant, polls: u32, interval: Duration) -> Self {
        Self {
            target,
            written_at,
            next_due: written_at + interval,
            remaining: polls.max(1),
            interval,
            actual_target: None,
        }
    }

    pub fn poll(
        &mut self,
        hp: Option<&impl HpBackend>,
        feed: &impl ThermalFeed,
    ) -> Option<Verification> {
        let now = Instant::now();
        if now < self.next_due {
            return None;
        }
        let (matches, actual) = match self.target {
            Target::Fan(target) => match hp.map(|h| h.fan_levels_sample()) {
                Some(Ok((actual, at))) => (
                    at >= self.written_at
                        && (target.left == 0
                            || (i32::from(actual.left) - i32::from(target.left)).abs() <= 10)
                        && (target.right == 0
                            || (i32::from(actual.right) - i32::from(target.right)).abs() <= 10),
                    format!("left={} right={} (x100 RPM)", actual.left, actual.right),
                ),
                other => (false, format!("fan readback unavailable: {other:?}")),
            },
            Target::Gpu(target, dstate_requested) => match hp.map(|h| h.gpu_platform_policy()) {
                Some(Ok(actual)) => {
                    self.actual_target = Some(Target::Gpu(actual, dstate_requested));
                    (
                        actual.ctgp == target.ctgp
                            && actual.ppab == target.ppab
                            && actual.slowdown_temp_c == target.slowdown_temp_c
                            && (!dstate_requested || actual.dstate == target.dstate),
                        format!("{actual:?}"),
                    )
                }
                other => (false, format!("GPU readback unavailable: {other:?}")),
            },
            Target::Power(target) => {
                let pl12 = feed.power_limits_w();
                let pl4 = feed.pl4_w();
                let matches = pl12.is_some_and(|(p1, p2, at)| {
                    at >= self.written_at
                        && (p1 - f64::from(target.pl1_w)).abs() <= 1.0
                        && (p2 - f64::from(target.pl2_w)).abs() <= 1.0
                }) && (target.pl4_w == 0
                    || pl4.is_some_and(|(p4, at)| {
                        at >= self.written_at && (p4 - f64::from(target.pl4_w)).abs() <= 1.0
                    }));
                (
                    matches,
                    format!("0x610={pl12:?}; PL4={pl4:?}; samples must follow write"),
                )
            }
        };
        if matches {
            return Some(Verification::Verified);
        }
        self.remaining -= 1;
        self.next_due = Instant::now() + self.interval;
        (self.remaining == 0).then(|| Verification::Failed {
            expected: format!("{:?}", self.target),
            actual,
        })
    }
}
