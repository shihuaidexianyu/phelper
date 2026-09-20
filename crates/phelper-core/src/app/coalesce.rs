//! Serialization and bounded Busy retry for the profile-only write surface.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use phelper_domain::command::{ControlCommand, ControlReceipt};

use super::state::KnobId;

pub const MIN_INTERVAL: Duration = Duration::from_millis(250);
pub const BUSY_BACKOFF: Duration = Duration::from_millis(300);
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Slot {
    pending: VecDeque<(KnobId, ControlCommand)>,
    in_flight: Option<ControlReceipt>,
    next_attempt_at: Option<Instant>,
    busy_since: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyVerdict {
    Retry,
    TimedOut,
}

pub struct Coalescer {
    slot: Slot,
    min_interval: Duration,
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
            slot: Slot::default(),
            min_interval: MIN_INTERVAL,
            busy_backoff: BUSY_BACKOFF,
            busy_timeout: BUSY_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_intervals(
        min_interval: Duration,
        busy_backoff: Duration,
        busy_timeout: Duration,
    ) -> Self {
        Self {
            slot: Slot::default(),
            min_interval,
            busy_backoff,
            busy_timeout,
        }
    }

    /// The latest click wins if one arrives before dispatch. Once a profile
    /// is in flight, the next click remains pending until it completes.
    pub fn enqueue(&mut self, knob: KnobId, command: ControlCommand) -> bool {
        self.slot.pending.retain(|(key, _)| *key != knob);
        self.slot.pending.push_back((knob, command));
        true
    }

    pub fn poll(&mut self, now: Instant) -> Vec<(KnobId, ControlCommand)> {
        if self.slot.in_flight.is_some() || self.slot.next_attempt_at.is_some_and(|time| now < time)
        {
            return Vec::new();
        }
        self.slot
            .pending
            .pop_front()
            .map(|command| vec![command])
            .unwrap_or_default()
    }

    pub fn has_work(&self) -> bool {
        !self.slot.pending.is_empty() || self.slot.in_flight.is_some()
    }

    pub fn note_dispatched(
        &mut self,
        knob: KnobId,
        _command: ControlCommand,
        receipt: ControlReceipt,
        now: Instant,
    ) {
        let _ = knob;
        self.slot.in_flight = Some(receipt);
        self.slot.next_attempt_at = Some(now + self.min_interval);
        self.slot.busy_since = None;
    }

    pub fn note_busy(
        &mut self,
        knob: KnobId,
        command: ControlCommand,
        now: Instant,
    ) -> BusyVerdict {
        let _ = knob;
        let since = *self.slot.busy_since.get_or_insert(now);
        if now.duration_since(since) >= self.busy_timeout {
            self.slot.pending.retain(|(key, _)| *key != knob);
            self.slot.busy_since = None;
            return BusyVerdict::TimedOut;
        }
        self.slot.pending.push_front((knob, command));
        self.slot.next_attempt_at = Some(now + self.busy_backoff);
        BusyVerdict::Retry
    }

    pub fn note_dispatch_error(&mut self, knob: KnobId) {
        let _ = knob;
        self.slot.busy_since = None;
    }

    pub fn note_completed(&mut self, knob: KnobId) {
        let _ = knob;
        self.slot.in_flight = None;
        self.slot.busy_since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str) -> ControlCommand {
        ControlCommand::ApplyProfile {
            profile: name.to_string(),
        }
    }

    #[test]
    fn profile_commands_are_serial() {
        let now = Instant::now();
        let mut queue = Coalescer::with_intervals(
            Duration::ZERO,
            Duration::from_millis(10),
            Duration::from_secs(1),
        );
        queue.enqueue(KnobId::Profile, profile("balanced"));
        let (_, command) = queue.poll(now).pop().unwrap();
        queue.note_dispatched(KnobId::Profile, command, ControlReceipt(1), now);
        queue.enqueue(KnobId::Profile, profile("gaming"));
        assert!(queue.poll(now).is_empty());
        queue.note_completed(KnobId::Profile);
        assert_eq!(queue.poll(now).len(), 1);
    }

    #[test]
    fn replacing_one_knob_preserves_other_pending_commands() {
        let now = Instant::now();
        let mut queue =
            Coalescer::with_intervals(Duration::ZERO, Duration::ZERO, Duration::from_secs(1));
        queue.enqueue(KnobId::Cpu, profile("old-cpu"));
        queue.enqueue(KnobId::Fan, profile("fan"));
        queue.enqueue(KnobId::Cpu, profile("new-cpu"));
        assert_eq!(queue.poll(now), vec![(KnobId::Fan, profile("fan"))]);
        assert_eq!(queue.poll(now), vec![(KnobId::Cpu, profile("new-cpu"))]);
        assert!(queue.poll(now).is_empty());
    }

    #[test]
    fn busy_retry_is_bounded() {
        let now = Instant::now();
        let mut queue = Coalescer::with_intervals(
            Duration::ZERO,
            Duration::from_millis(10),
            Duration::from_millis(20),
        );
        assert_eq!(
            queue.note_busy(KnobId::Profile, profile("balanced"), now),
            BusyVerdict::Retry
        );
        assert!(queue.poll(now + Duration::from_millis(9)).is_empty());
        assert_eq!(queue.poll(now + Duration::from_millis(10)).len(), 1);
        assert_eq!(
            queue.note_busy(
                KnobId::Profile,
                profile("balanced"),
                now + Duration::from_millis(20),
            ),
            BusyVerdict::TimedOut
        );
    }
}
