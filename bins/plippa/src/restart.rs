//! Restart policy for crashed children (SYS-14): exponential backoff, a rolling crash budget and
//! a stable-run reset, as a pure state machine with no clock and no process handling of its own.
//!
//! The launcher feeds it exit times and acts on the [`Decision`]; tests feed it fabricated times.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Numbers that shape restart behaviour. See `plippa.service` and the SECFIX_07 decision log for
/// why these values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Delay before the first restart after a crash.
    pub initial_backoff: Duration,
    /// Backoff doubles per consecutive crash up to this cap.
    pub max_backoff: Duration,
    /// A child that ran at least this long before exiting starts over at `initial_backoff`.
    pub stable_after: Duration,
    /// Abnormal exits inside `budget_window` that exhaust a child's budget.
    pub crash_budget: usize,
    /// Rolling window for `crash_budget`.
    pub budget_window: Duration,
    /// How long a non-essential child is left dead once its budget is exhausted.
    pub cooldown: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            stable_after: Duration::from_secs(60),
            crash_budget: 8,
            budget_window: Duration::from_secs(300),
            cooldown: Duration::from_secs(600),
        }
    }
}

/// What to do with a child that just exited (or failed to spawn) abnormally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Start it again at `at`.
    Restart { at: Instant, delay: Duration },
    /// Budget exhausted, child is not essential: leave it dead until `at`, then try again with a
    /// fresh budget.
    CoolDown { at: Instant, delay: Duration },
    /// Budget exhausted, child is essential: the launcher must stop and exit non-zero.
    GiveUp,
}

/// Per-child bookkeeping.
#[derive(Debug, Clone)]
pub struct ChildState {
    started_at: Option<Instant>,
    next_backoff: Duration,
    abnormal: VecDeque<Instant>,
}

impl ChildState {
    pub fn new(policy: &RestartPolicy) -> Self {
        Self {
            started_at: None,
            next_backoff: policy.initial_backoff,
            abnormal: VecDeque::new(),
        }
    }

    /// The child process is now running.
    pub fn on_start(&mut self, now: Instant) {
        self.started_at = Some(now);
    }

    /// The child exited (or never started: pass `None` for `started`-less spawn failures by not
    /// calling `on_start`). Every exit that was not requested by the launcher is abnormal.
    pub fn on_exit(&mut self, now: Instant, essential: bool, policy: &RestartPolicy) -> Decision {
        let ran = self
            .started_at
            .take()
            .map(|s| now.saturating_duration_since(s))
            .unwrap_or_default();
        if ran >= policy.stable_after {
            self.next_backoff = policy.initial_backoff;
        }
        self.abnormal.push_back(now);
        while let Some(&front) = self.abnormal.front() {
            if now.saturating_duration_since(front) > policy.budget_window {
                self.abnormal.pop_front();
            } else {
                break;
            }
        }
        if self.abnormal.len() >= policy.crash_budget {
            if essential {
                return Decision::GiveUp;
            }
            self.abnormal.clear();
            self.next_backoff = policy.initial_backoff;
            return Decision::CoolDown {
                at: now + policy.cooldown,
                delay: policy.cooldown,
            };
        }
        let delay = self.next_backoff;
        self.next_backoff = (delay * 2).min(policy.max_backoff);
        Decision::Restart {
            at: now + delay,
            delay,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn backoff_grows_then_caps() {
        let p = RestartPolicy {
            crash_budget: 100,
            ..RestartPolicy::default()
        };
        let mut s = ChildState::new(&p);
        let t0 = Instant::now();
        let mut now = t0;
        let mut delays = vec![];
        for _ in 0..8 {
            s.on_start(now);
            match s.on_exit(now, true, &p) {
                Decision::Restart { at, delay } => {
                    assert_eq!(at, now + delay);
                    delays.push(delay.as_secs());
                    now = at;
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn a_stable_run_resets_the_backoff() {
        let p = RestartPolicy::default();
        let mut s = ChildState::new(&p);
        let mut now = Instant::now();
        for _ in 0..3 {
            s.on_start(now);
            if let Decision::Restart { at, .. } = s.on_exit(now, true, &p) {
                now = at;
            }
        }
        // Next delay would be 8 s; a 61 s stable run first resets it to 1 s.
        s.on_start(now);
        now += secs(61);
        match s.on_exit(now, true, &p) {
            Decision::Restart { delay, .. } => assert_eq!(delay, secs(1)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn eight_exits_in_five_minutes_exhaust_an_essential_child() {
        let p = RestartPolicy::default();
        let mut s = ChildState::new(&p);
        let mut now = Instant::now();
        for i in 0..8 {
            s.on_start(now);
            let d = s.on_exit(now, true, &p);
            if i < 7 {
                match d {
                    Decision::Restart { at, .. } => now = at,
                    other => panic!("exit {i}: {other:?}"),
                }
            } else {
                assert_eq!(d, Decision::GiveUp);
            }
        }
    }

    #[test]
    fn exits_spread_beyond_the_window_do_not_exhaust_the_budget() {
        let p = RestartPolicy::default();
        let mut s = ChildState::new(&p);
        let mut now = Instant::now();
        for _ in 0..20 {
            s.on_start(now);
            // Each run lasts 100 s: never stable (60 s counts as stable) ... use 50 s.
            now += secs(50);
            // 50 s apart, so at most 6 exits fit in 300 s.
            let d = s.on_exit(now, true, &p);
            assert!(matches!(d, Decision::Restart { .. }), "{d:?}");
            if let Decision::Restart { at, .. } = d {
                now = at;
            }
        }
    }

    #[test]
    fn a_non_essential_child_cools_down_then_gets_a_fresh_budget() {
        let p = RestartPolicy::default();
        let mut s = ChildState::new(&p);
        let mut now = Instant::now();
        for _ in 0..7 {
            s.on_start(now);
            if let Decision::Restart { at, .. } = s.on_exit(now, false, &p) {
                now = at;
            }
        }
        s.on_start(now);
        match s.on_exit(now, false, &p) {
            Decision::CoolDown { at, delay } => {
                assert_eq!(delay, secs(600));
                assert_eq!(at, now + secs(600));
                now = at;
            }
            other => panic!("{other:?}"),
        }
        s.on_start(now);
        match s.on_exit(now, false, &p) {
            Decision::Restart { delay, .. } => {
                assert_eq!(delay, secs(1), "budget and backoff reset")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_spawn_failure_with_no_start_counts_as_a_zero_length_run() {
        let p = RestartPolicy::default();
        let mut s = ChildState::new(&p);
        let now = Instant::now();
        // never on_start
        assert!(matches!(
            s.on_exit(now, true, &p),
            Decision::Restart { delay, .. } if delay == secs(1)
        ));
        assert!(matches!(
            s.on_exit(now, true, &p),
            Decision::Restart { delay, .. } if delay == secs(2)
        ));
    }
}
