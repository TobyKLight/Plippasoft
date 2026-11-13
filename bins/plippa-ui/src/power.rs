//! Hold-to-power thresholds (Model D volume encoder push).
//!
//! Two thresholds, both measured from the same press (PWRSW_01):
//! - [`PowerHoldConfig::warn_ms`] (1 s): the UI says "keep holding to power off".
//! - [`PowerHoldConfig::hold_ms`] (4 s): the host is asked to power off.
//!
//! Releasing between the two cancels the gesture. A press that is already down when the
//! tracker is created must not count — that is the "power on by holding the button" case,
//! where the button is still held while the UI boots. The tracker arms on the first release.

/// Configuration for hold-to-power.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerHoldConfig {
    /// Milliseconds after which the UI warns that the host is about to power off.
    pub warn_ms: u64,
    /// Milliseconds the button must stay down to power the host off.
    pub hold_ms: u64,
}

impl Default for PowerHoldConfig {
    fn default() -> Self {
        Self {
            warn_ms: 1_000,
            hold_ms: 4_000,
        }
    }
}

/// Emitted while a press progresses. At most one per [`PowerHold::on_tick`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerHoldEvent {
    /// Press reached `warn_ms`: show the warning, keep holding.
    Warning,
    /// Press reached `hold_ms`: the host may now be powered off.
    ShutdownRequest,
    /// Released after [`PowerHoldEvent::Warning`] but before the shutdown threshold.
    Cancelled,
}

/// Tracks press duration for hold-to-power.
#[derive(Debug, Clone)]
pub struct PowerHold {
    cfg: PowerHoldConfig,
    down_since_ms: Option<u64>,
    warned: bool,
    fired: bool,
    /// False until the first release: a press inherited from before startup is ignored.
    armed: bool,
}

impl PowerHold {
    pub fn new(cfg: PowerHoldConfig) -> Self {
        Self {
            cfg,
            down_since_ms: None,
            warned: false,
            fired: false,
            armed: false,
        }
    }

    pub fn config(&self) -> PowerHoldConfig {
        self.cfg
    }

    /// True while the warning is up and the host is not yet shutting down.
    pub fn warning_active(&self) -> bool {
        self.down_since_ms.is_some() && self.warned && !self.fired
    }

    /// True once the shutdown threshold has fired for the current press.
    pub fn shutdown_fired(&self) -> bool {
        self.fired
    }

    /// Milliseconds the button has been down (0 when it is not down).
    pub fn held_ms(&self, now_ms: u64) -> u64 {
        self.down_since_ms
            .map_or(0, |start| now_ms.saturating_sub(start))
    }

    pub fn on_button(&mut self, down: bool, now_ms: u64) -> Option<PowerHoldEvent> {
        if down {
            if !self.armed {
                return None;
            }
            if self.down_since_ms.is_none() {
                self.down_since_ms = Some(now_ms);
                self.warned = false;
                self.fired = false;
            }
            None
        } else {
            let was_down = self.down_since_ms.take().is_some();
            let cancelled = was_down && self.warned && !self.fired;
            self.warned = false;
            self.fired = false;
            self.armed = true;
            cancelled.then_some(PowerHoldEvent::Cancelled)
        }
    }

    pub fn on_tick(&mut self, now_ms: u64) -> Option<PowerHoldEvent> {
        let Some(start) = self.down_since_ms else {
            return None;
        };
        let elapsed = now_ms.saturating_sub(start);
        if !self.warned && elapsed >= self.cfg.warn_ms {
            self.warned = true;
            return Some(PowerHoldEvent::Warning);
        }
        if self.warned && !self.fired && elapsed >= self.cfg.hold_ms {
            self.fired = true;
            return Some(PowerHoldEvent::ShutdownRequest);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hold() -> PowerHold {
        PowerHold::new(PowerHoldConfig {
            warn_ms: 1_000,
            hold_ms: 4_000,
        })
    }

    /// Press at `at` and release immediately after: arms the tracker for later presses.
    fn arm(h: &mut PowerHold, at: u64) {
        h.on_button(true, at);
        h.on_button(false, at + 1);
    }

    #[test]
    fn warning_at_one_second_then_shutdown_at_four() {
        let mut h = hold();
        arm(&mut h, 0);
        assert!(h.on_button(true, 100).is_none());
        assert!(h.on_tick(1_099).is_none());
        assert_eq!(h.on_tick(1_100), Some(PowerHoldEvent::Warning));
        assert!(h.on_tick(1_200).is_none(), "warning only once");
        assert!(h.on_tick(4_099).is_none());
        assert_eq!(h.on_tick(4_100), Some(PowerHoldEvent::ShutdownRequest));
        assert!(h.on_tick(4_200).is_none(), "fires once per press");
        assert!(h.shutdown_fired());
    }

    #[test]
    fn release_before_four_cancels_and_rearms() {
        let mut h = hold();
        arm(&mut h, 0);
        h.on_button(true, 100);
        assert_eq!(h.on_tick(1_100), Some(PowerHoldEvent::Warning));
        assert_eq!(
            h.on_button(false, 2_000),
            Some(PowerHoldEvent::Cancelled)
        );
        assert!(!h.warning_active());
        assert!(h.on_tick(9_000).is_none(), "released: nothing to fire");
        // A fresh press works from scratch.
        h.on_button(true, 9_100);
        assert_eq!(h.on_tick(10_100), Some(PowerHoldEvent::Warning));
        assert_eq!(h.on_tick(13_100), Some(PowerHoldEvent::ShutdownRequest));
    }

    #[test]
    fn short_press_never_warns_or_shuts_down() {
        let mut h = hold();
        arm(&mut h, 0);
        h.on_button(true, 100);
        assert!(h.on_tick(900).is_none());
        assert_eq!(h.on_button(false, 900), None, "no warning shown yet");
        assert!(h.on_tick(9_000).is_none());
    }

    #[test]
    fn release_before_warning_is_not_a_cancel() {
        let mut h = hold();
        arm(&mut h, 0);
        h.on_button(true, 100);
        assert!(h.on_tick(500).is_none());
        assert_eq!(h.on_button(false, 500), None);
    }

    #[test]
    fn release_after_firing_is_not_a_cancel() {
        let mut h = hold();
        arm(&mut h, 0);
        h.on_button(true, 100);
        assert_eq!(h.on_tick(4_100), Some(PowerHoldEvent::Warning));
        assert_eq!(h.on_tick(4_200), Some(PowerHoldEvent::ShutdownRequest));
        assert_eq!(h.on_button(false, 4_300), None);
        assert!(!h.warning_active());
    }

    #[test]
    fn no_event_until_first_release_after_start() {
        // "Held while booting": the UI starts with the button already down.
        let mut h = hold();
        assert!(h.on_button(true, 0).is_none());
        for t in [500, 1_000, 3_999, 4_000, 9_000] {
            assert!(h.on_tick(t).is_none(), "unarmed press must never fire at {t}");
        }
        assert_eq!(h.on_button(false, 9_100), None, "arming release is silent");
        h.on_button(true, 9_200);
        assert_eq!(h.on_tick(10_200), Some(PowerHoldEvent::Warning));
    }

    #[test]
    fn a_late_tick_emits_warning_first() {
        let mut h = hold();
        arm(&mut h, 0);
        h.on_button(true, 100);
        // A stalled UI can skip both thresholds in one tick: warn, then fire next tick.
        assert_eq!(h.on_tick(5_000), Some(PowerHoldEvent::Warning));
        assert_eq!(h.on_tick(5_033), Some(PowerHoldEvent::ShutdownRequest));
    }

    #[test]
    fn held_ms_tracks_the_current_press() {
        let mut h = hold();
        arm(&mut h, 0);
        h.on_button(true, 1_000);
        assert_eq!(h.held_ms(1_250), 250);
        h.on_button(false, 1_300);
        assert_eq!(h.held_ms(9_000), 0);
    }
}
