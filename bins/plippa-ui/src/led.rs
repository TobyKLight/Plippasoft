//! LED cues: NFC tap flash + play-button transport colour, plus role-addressed
//! steady colours (NB_13). Software names [`LedRole`]s, never LED indices.

use crate::record_leds::RecordFrame;
use plippa_board::{BoardProfile, LedRole};
use std::collections::{BTreeMap, BTreeSet};

/// Logical RGB colour (the hardware backend converts to the strip's order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Linear blend `a -> b` by `t` in `0..=1` (rounded). The LED layer's only
    /// interpolation primitive (WARP_21 transport pulse).
    pub fn blend(a: Rgb, b: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let mix = |x: u8, y: u8| ((f32::from(x)) + (f32::from(y) - f32::from(x)) * t).round() as u8;
        Rgb::new(mix(a.r, b.r), mix(a.g, b.g), mix(a.b, b.b))
    }
}

/// A sinusoidal brightness pulse for one transport backlight (WARP_21): the colour is
/// interpolated between `lo` and `hi` on a sine of period `period_ms`. Purely time-based
/// (injected `now_ms`), so the composer and tests share it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportPulse {
    /// Colour at the bottom of the swing.
    pub lo: Rgb,
    /// Colour at the top of the swing.
    pub hi: Rgb,
    /// One full low-high-low cycle in milliseconds.
    pub period_ms: u32,
}

impl TransportPulse {
    /// Sine phase `0..=1` at `now_ms` (0 = `lo`, 1 = `hi`). Continuous, no origin tracked.
    pub fn level_at(&self, now_ms: u64) -> f32 {
        let period = self.period_ms.max(1);
        let phase = (now_ms % u64::from(period)) as f32 / period as f32;
        0.5 - 0.5 * (phase * std::f32::consts::TAU).cos()
    }

    /// Pulse colour at `now_ms`.
    pub fn colour_at(&self, now_ms: u64) -> Rgb {
        Rgb::blend(self.lo, self.hi, self.level_at(now_ms))
    }
}

/// Play button backlight (the `PlayButton` LED role).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayButtonLed {
    /// Nothing loaded.
    #[default]
    Off,
    /// Loaded / paused / stopped-after-EOF — standby to play again.
    Standby,
    /// Currently playing.
    Playing,
}

impl PlayButtonLed {
    pub fn from_transport(transport: plippa_core::TransportState, has_loaded_item: bool) -> Self {
        use plippa_core::TransportState;
        match transport {
            TransportState::Playing => Self::Playing,
            TransportState::Paused | TransportState::Stopped if has_loaded_item => Self::Standby,
            _ => Self::Off,
        }
    }
}

/// Prev/next button backlight when the control is available (NB_14): the same standby purple
/// as the FF/RW backlights, so the whole transport row reads as one family.
///
/// The press flash ([`NAV_LED_FLASH`], full white) is still a visible brightening over it.
/// Kept in step with `warp_ui::FFRW_LED_IDLE`; the test
/// `nav_led_colours_are_distinct_from_existing_states` pins that. To only dim/brighten the
/// prev/next backlights without touching code, add `max_brightness_pct` to
/// `[leds.prev_track_button]` / `[leds.next_track_button]` in `board.toml`. Flash length:
/// `FLASH_MS` in `led_compose.rs`. Documented in `NBPackagesTestOnDevice.md` (NB_14, step 10).
pub const NAV_LED_ON: Rgb = Rgb::new(60, 0, 90);
/// Colour of the press flash on prev/next backlights (NB_14).
pub const NAV_LED_FLASH: Rgb = Rgb::new(255, 255, 255);

/// Top-panel REC button (SW6): a steady red glow even though the button has no transport
/// function yet, so it reads as a live control rather than a dead one.
pub const REC_LED_GLOW: Rgb = Rgb::new(120, 0, 0);
/// Front-panel Back button (SW6): a steady white glow, matching the transport family.
pub const BACK_LED_GLOW: Rgb = Rgb::new(120, 120, 120);

/// Transport-button backlights (PlayMode now, FF/RW reuse them in WARP_12): steady level while
/// the role LED exists, brighter while the button is held. **Guesses: tune on the CM4.**
pub const TRANSPORT_LED_IDLE: Rgb = Rgb::new(90, 90, 90);
pub const TRANSPORT_LED_PRESSED: Rgb = NAV_LED_FLASH;
/// PLAY_04: PlayMode (SW2) backlight while a non-default play-mode rung is active (shuffle,
/// repeat, pause after each or repeat reshuffles). Blue, distinct from the standby/FF-RW
/// purples, the power blue and the white transport idle. **Guess: tune on the CM4.**
pub const PLAYMODE_LED_ACTIVE: Rgb = Rgb::new(0, 120, 255);
/// A pressed transport backlight stays at its pressed level at least this long (ms), so a
/// 20 ms tap is visible at the 30 FPS the LED worker refreshes at (WARP_12).
pub const TRANSPORT_LED_MIN_PRESSED_MS: u64 = 90;

/// PWRSW_01: power LED while the hold-to-power warning is up. Amber, the near-universal
/// "about to power off" colour; the steady blue returns as soon as the press is cancelled.
pub const POWER_LED_WARN: Rgb = Rgb::new(255, 96, 0);

/// Hardware or mock LED feedback for NFC tap + transport.
pub trait LedCue {
    fn on_tap_feedback(&mut self, tap_id: u64);

    /// Optional per-frame housekeeping (clear timed flashes).
    fn tick(&mut self) {}

    /// Update play-button backlight from engine transport.
    fn set_play_button(&mut self, _state: PlayButtonLed) {}

    /// Does this backend have LED(s) for `role`? Consumers skip absent roles
    /// (feature simply unavailable — no error, no notice).
    fn has_role(&self, _role: LedRole) -> bool {
        false
    }

    /// Steady colour for the whole span of `role` (`None` = off). Absent role: no-op.
    fn set_role_colour(&mut self, _role: LedRole, _colour: Option<Rgb>) {}

    /// Animated pulse for the whole span of `role` (`None` = no pulse), painted over the
    /// steady colour each frame (WARP_21). Absent role: no-op.
    fn set_role_pulse(&mut self, _role: LedRole, _pulse: Option<TransportPulse>) {}

    /// Short timed flash of `role` in `colour` (~[`crate::led_compose::FLASH_MS`]), then the
    /// steady state returns by itself. Absent role: no-op. Called on the *press*, not the ack.
    fn flash_role(&mut self, _role: LedRole, _colour: Rgb) {}

    /// ART_10: show the cover colours on the NFC ring, rotated to `frame.angle_deg`
    /// (`None` = the ring returns to its normal state). A tap animation or flash still wins
    /// while it runs. Callers check [`Self::has_role`] for the ring first; default: no-op.
    /// Returns `false` when a queued backend could not take the command (the caller must send
    /// it again); a no-op default counts as delivered.
    fn set_record_art(&mut self, _frame: Option<RecordFrame>) -> bool {
        true
    }

    /// All LEDs off (Ctrl-C / shutdown). Default: no-op.
    fn clear_all(&mut self) {}
}

/// Records flash events for tests / mock backend.
#[derive(Debug, Default, Clone)]
pub struct MockLedCue {
    pub flashes: Vec<u64>,
    pub play_button: PlayButtonLed,
    /// Steady colours recorded by role (off = removed).
    pub role_colours: BTreeMap<LedRole, Rgb>,
    /// Active pulses recorded by role (no pulse = removed) (WARP_21).
    pub role_pulses: BTreeMap<LedRole, TransportPulse>,
    /// Press flashes requested, in order (NB_14).
    pub role_flashes: Vec<(LedRole, Rgb)>,
    /// The record frame last requested for the NFC ring (ART_10).
    pub record_art: Option<RecordFrame>,
    /// Roles this mock "has"; `None` (default) = every role (desktop mock).
    supported: Option<BTreeSet<LedRole>>,
}

impl MockLedCue {
    /// Mock limited to the roles a board profile defines (absent roles are ignored).
    pub fn for_profile(profile: &BoardProfile) -> Self {
        Self {
            supported: Some(profile.leds.keys().copied().collect()),
            ..Self::default()
        }
    }
}

impl LedCue for MockLedCue {
    fn has_role(&self, role: LedRole) -> bool {
        self.supported.as_ref().is_none_or(|s| s.contains(&role))
    }

    fn set_role_colour(&mut self, role: LedRole, colour: Option<Rgb>) {
        if !self.has_role(role) {
            return;
        }
        match colour {
            Some(c) => {
                self.role_colours.insert(role, c);
            }
            None => {
                self.role_colours.remove(&role);
            }
        }
    }

    fn set_role_pulse(&mut self, role: LedRole, pulse: Option<TransportPulse>) {
        if !self.has_role(role) {
            return;
        }
        match pulse {
            Some(p) => {
                self.role_pulses.insert(role, p);
            }
            None => {
                self.role_pulses.remove(&role);
            }
        }
    }

    fn flash_role(&mut self, role: LedRole, colour: Rgb) {
        if self.has_role(role) {
            self.role_flashes.push((role, colour));
        }
    }

    fn set_record_art(&mut self, frame: Option<RecordFrame>) -> bool {
        if self.has_role(LedRole::NfcRing) {
            self.record_art = frame;
        }
        true
    }

    fn on_tap_feedback(&mut self, tap_id: u64) {
        self.flashes.push(tap_id);
    }

    fn set_play_button(&mut self, state: PlayButtonLed) {
        self.play_button = state;
    }
}

/// No-op (desktop without LED hardware).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullLedCue;

impl LedCue for NullLedCue {
    fn on_tap_feedback(&mut self, _tap_id: u64) {}
}

/// Logs a tap flash and is safe without WS2811.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogLedCue;

impl LedCue for LogLedCue {
    fn on_tap_feedback(&mut self, tap_id: u64) {
        eprintln!("plippa-ui: LED tap feedback tap_id={tap_id} (Noubin §3.2)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_core::TransportState;

    #[test]
    fn mock_records_flash_after_tap() {
        let mut led = MockLedCue::default();
        led.on_tap_feedback(3);
        led.on_tap_feedback(4);
        assert_eq!(led.flashes, vec![3, 4]);
    }

    #[test]
    fn mock_records_role_colours_and_ignores_absent_roles() {
        let mut led = MockLedCue::default();
        led.set_role_colour(LedRole::PrevTrackButton, Some(Rgb::new(0, 0, 255)));
        assert_eq!(led.role_colours.len(), 1);
        led.set_role_colour(LedRole::PrevTrackButton, None);
        assert!(led.role_colours.is_empty());

        // Minimal board: no LED roles at all.
        let p = crate::control_roles::tests_support::minimal_profile();
        let mut led = MockLedCue::for_profile(&p);
        assert!(!led.has_role(LedRole::PlayButton));
        led.set_role_colour(LedRole::PlayButton, Some(Rgb::new(1, 2, 3)));
        assert!(led.role_colours.is_empty());

        // POC2: prev/next backlights exist.
        let p = crate::control_roles::tests_support::poc2_profile();
        let mut led = MockLedCue::for_profile(&p);
        assert!(led.has_role(LedRole::PrevTrackButton) && led.has_role(LedRole::NextTrackButton));
        led.set_role_colour(LedRole::NextTrackButton, Some(Rgb::new(9, 9, 9)));
        assert_eq!(
            led.role_colours[&LedRole::NextTrackButton],
            Rgb::new(9, 9, 9)
        );
    }

    #[test]
    fn mock_records_flashes_and_ignores_absent_roles() {
        let mut led = MockLedCue::default();
        led.flash_role(LedRole::NextTrackButton, NAV_LED_FLASH);
        assert_eq!(
            led.role_flashes,
            vec![(LedRole::NextTrackButton, NAV_LED_FLASH)]
        );
        let p = crate::control_roles::tests_support::minimal_profile();
        let mut led = MockLedCue::for_profile(&p);
        led.flash_role(LedRole::NextTrackButton, NAV_LED_FLASH);
        assert!(led.role_flashes.is_empty());
    }

    #[test]
    fn play_button_from_transport() {
        assert_eq!(
            PlayButtonLed::from_transport(TransportState::Playing, true),
            PlayButtonLed::Playing
        );
        assert_eq!(
            PlayButtonLed::from_transport(TransportState::Stopped, true),
            PlayButtonLed::Standby
        );
        assert_eq!(
            PlayButtonLed::from_transport(TransportState::Paused, true),
            PlayButtonLed::Standby
        );
        assert_eq!(
            PlayButtonLed::from_transport(TransportState::Stopped, false),
            PlayButtonLed::Off
        );
    }
}

#[cfg(test)]
mod nav_colour_tests {
    use super::*;
    use crate::led_compose::{BLUE, ORANGE, WHOOSH};

    /// NB_VERIFY NB_14: prev/next backlight colours must not be mistaken for power/play/NFC
    /// states, and the flash must be clearly brighter than the steady level.
    #[test]
    fn nav_led_colours_are_distinct_from_existing_states() {
        let lum = |c: Rgb| u32::from(c.r) + u32::from(c.g) + u32::from(c.b);
        for existing in [BLUE, ORANGE, WHOOSH] {
            assert_ne!(NAV_LED_ON, existing);
            assert_ne!(NAV_LED_FLASH, existing);
        }
        assert_ne!(NAV_LED_ON, NAV_LED_FLASH);
        assert!(lum(NAV_LED_FLASH) >= 2 * lum(NAV_LED_ON), "flash must be a visible brightening");
        assert!(lum(NAV_LED_ON) > 0, "available must be lit");
        // WARP_12: FF/RW (and PlayMode) use the transport levels; same rules.
        for existing in [BLUE, ORANGE, WHOOSH] {
            assert_ne!(TRANSPORT_LED_IDLE, existing);
            assert_ne!(TRANSPORT_LED_PRESSED, existing);
        }
        assert!(lum(TRANSPORT_LED_PRESSED) >= 2 * lum(TRANSPORT_LED_IDLE), "pressed >= 2x idle");
        assert!(lum(TRANSPORT_LED_IDLE) > 0, "idle glow is lit");
        assert!(TRANSPORT_LED_MIN_PRESSED_MS >= 34, "visible for at least one 30 FPS frame");
        // Owner request: prev/next standby is the same purple as the FF/RW standby glow, so the
        // transport row reads as one family (the white flash still stands out on top).
        assert_eq!(NAV_LED_ON, crate::warp_ui::FFRW_LED_IDLE);
        assert!(NAV_LED_ON.r > 0 && NAV_LED_ON.b > 0, "purple has red and blue");
        assert_eq!(NAV_LED_ON.g, 0, "purple has no green");
        // PLAY_04: the active-mode blue is its own state, not a nearby warning/transport colour.
        for existing in [
            BLUE,
            ORANGE,
            WHOOSH,
            POWER_LED_WARN,
            TRANSPORT_LED_IDLE,
            TRANSPORT_LED_PRESSED,
            crate::warp_ui::FFRW_LED_IDLE,
            crate::warp_ui::FFRW_LED_PRESSED,
            crate::warp_ui::FFRW_LED_SLOW_BLUE,
        ] {
            assert_ne!(PLAYMODE_LED_ACTIVE, existing);
        }
        assert!(lum(PLAYMODE_LED_ACTIVE) > 0, "active mode is lit");
        assert_eq!(PLAYMODE_LED_ACTIVE.r, 0, "active mode is blue");
        // REC / Back glows: lit, and neither is a transport state.
        assert!(lum(REC_LED_GLOW) > 0 && is_red_only(REC_LED_GLOW), "REC glows red");
        assert!(lum(BACK_LED_GLOW) > 0, "Back glows");
        for existing in [NAV_LED_ON, ORANGE, WHOOSH, PLAYMODE_LED_ACTIVE] {
            assert_ne!(REC_LED_GLOW, existing);
            assert_ne!(BACK_LED_GLOW, existing);
        }
    }

    /// Red with no green or blue component.
    fn is_red_only(c: Rgb) -> bool {
        c.r > 0 && c.g == 0 && c.b == 0
    }
}
