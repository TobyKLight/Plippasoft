//! Poll interval helper enforcing ≥5 Hz in normal operation (§3.2.1).

use std::time::Duration;

/// Minimum poll rate for hardware Noubin players in normal operation.
pub const MIN_POLL_HZ: u32 = 5;

/// Maximum interval between polls (= 1 / [`MIN_POLL_HZ`]).
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Clock abstraction so poll scheduling is unit-testable without sleeping.
pub trait PollClock {
    fn now(&self) -> Duration;
}

/// Wall-clock based on [`std::time::Instant`].
#[derive(Debug, Clone)]
pub struct SteadyClock {
    start: std::time::Instant,
}

impl SteadyClock {
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for SteadyClock {
    fn default() -> Self {
        Self::new()
    }
}

impl PollClock for SteadyClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }
}

/// Tracks last poll time and answers whether another poll is due for ≥5 Hz.
#[derive(Debug, Clone)]
pub struct PollScheduler<C: PollClock> {
    clock: C,
    last_poll: Option<Duration>,
    max_interval: Duration,
}

impl<C: PollClock> PollScheduler<C> {
    pub fn new(clock: C) -> Self {
        Self {
            clock,
            last_poll: None,
            max_interval: MIN_POLL_INTERVAL,
        }
    }

    pub fn with_max_interval(mut self, interval: Duration) -> Self {
        self.max_interval = interval;
        self
    }

    /// `true` if a poll should run now (first call, or ≥ max interval since last mark).
    pub fn should_poll(&self) -> bool {
        match self.last_poll {
            None => true,
            Some(t) => self.clock.now().saturating_sub(t) >= self.max_interval,
        }
    }

    /// Record that a poll just occurred.
    pub fn mark_polled(&mut self) {
        self.last_poll = Some(self.clock.now());
    }

    /// Time until the next poll is due (`Duration::ZERO` if due now).
    pub fn time_until_due(&self) -> Duration {
        match self.last_poll {
            None => Duration::ZERO,
            Some(t) => {
                let elapsed = self.clock.now().saturating_sub(t);
                self.max_interval.saturating_sub(elapsed)
            }
        }
    }

    pub fn max_interval(&self) -> Duration {
        self.max_interval
    }

    pub fn clock(&self) -> &C {
        &self.clock
    }

    pub fn clock_mut(&mut self) -> &mut C {
        &mut self.clock
    }
}

/// Mutable fake clock for tests.
#[derive(Debug, Clone, Default)]
pub struct FakeClock {
    pub now: Duration,
}

impl PollClock for FakeClock {
    fn now(&self) -> Duration {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_five_hz_ceiling_interval() {
        assert_eq!(MIN_POLL_INTERVAL, Duration::from_millis(200));
        let mut sched = PollScheduler::new(FakeClock {
            now: Duration::ZERO,
        });
        assert!(sched.should_poll());
        sched.mark_polled();
        assert!(!sched.should_poll());

        sched.clock_mut().now = Duration::from_millis(199);
        assert!(!sched.should_poll());
        assert_eq!(sched.time_until_due(), Duration::from_millis(1));

        sched.clock_mut().now = Duration::from_millis(200);
        assert!(sched.should_poll());
    }
}
