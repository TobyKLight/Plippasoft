//! Hold-duration detection hooks (menu enter + assignable encoder push).
//!
//! [`UiApp`](crate::UiApp) consumes AssignableEncoder holds for bind-to-A
//! (UI_04) and MenuEnter holds for keyboard delete (UI_05).

/// Which physical control the hold applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldTarget {
    /// Menu encoder push (enter).
    MenuEnter,
    /// Assignable encoder push (SWE3 on poc2).
    AssignableEncoder,
}

/// Fired once per press when `hold_ms` elapses while still down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldEvent {
    pub target: HoldTarget,
}

/// Per-button hold configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldConfig {
    pub hold_ms: u64,
}

impl Default for HoldConfig {
    fn default() -> Self {
        // Placeholder threshold — product timings arrive in later UI packages.
        Self { hold_ms: 800 }
    }
}

/// Tracks one button's press duration.
#[derive(Debug, Clone)]
pub struct HoldDetector {
    target: HoldTarget,
    cfg: HoldConfig,
    down_since_ms: Option<u64>,
    fired: bool,
}

impl HoldDetector {
    pub fn new(target: HoldTarget, cfg: HoldConfig) -> Self {
        Self {
            target,
            cfg,
            down_since_ms: None,
            fired: false,
        }
    }

    pub fn on_button(&mut self, down: bool, now_ms: u64) {
        if down {
            if self.down_since_ms.is_none() {
                self.down_since_ms = Some(now_ms);
                self.fired = false;
            }
        } else {
            self.down_since_ms = None;
            self.fired = false;
        }
    }

    pub fn on_tick(&mut self, now_ms: u64) -> Option<HoldEvent> {
        let Some(start) = self.down_since_ms else {
            return None;
        };
        if self.fired {
            return None;
        }
        if now_ms.saturating_sub(start) >= self.cfg.hold_ms {
            self.fired = true;
            Some(HoldEvent {
                target: self.target,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_enter_hold_fires_once() {
        let mut d = HoldDetector::new(HoldTarget::MenuEnter, HoldConfig { hold_ms: 500 });
        d.on_button(true, 0);
        assert!(d.on_tick(499).is_none());
        assert_eq!(
            d.on_tick(500),
            Some(HoldEvent {
                target: HoldTarget::MenuEnter
            })
        );
        assert!(d.on_tick(600).is_none());
        d.on_button(false, 700);
        d.on_button(true, 800);
        assert_eq!(
            d.on_tick(1_300),
            Some(HoldEvent {
                target: HoldTarget::MenuEnter
            })
        );
    }

    #[test]
    fn assignable_short_press_does_not_fire() {
        let mut d = HoldDetector::new(
            HoldTarget::AssignableEncoder,
            HoldConfig { hold_ms: 800 },
        );
        d.on_button(true, 0);
        assert!(d.on_tick(400).is_none());
        d.on_button(false, 400);
        assert!(d.on_tick(2_000).is_none());
    }
}
