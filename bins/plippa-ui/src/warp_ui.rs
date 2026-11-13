//! Pure, clock-injected building blocks of the WARP tempo / FF-RW / jog UI (WARP_06).
//!
//! Nothing in here touches I/O, pins, LED indices or POC2 names: every function takes the
//! time as an argument (`now_ms`, milliseconds on the loop clock) so the state machines are
//! unit-testable without sleeping. The controls are expressed in roles only.
//!
//! All tuning values live in the [`consts`]-style block right below; they are conservative
//! defaults that need on-device tuning (see the WARP_06 completion notes).

use crate::led::{
    Rgb, TransportPulse, TRANSPORT_LED_MIN_PRESSED_MS,
};
use crate::UiOutgoing;
use plippa_core::{
    clamp_seek_target_ms, WarpModeSettings, RATE_MAX_ABS_PERMILLE, RATE_MIN_ABS_PERMILLE,
    RATE_UNITY_PERMILLE,
};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand};

// ---------------------------------------------------------------------------------------
// Constants block (README section 7). Needs on-device tuning.
// ---------------------------------------------------------------------------------------

/// A press held at least this long is a hold (FF/RW scan, PlayMode hold); shorter = tap.
pub const HOLD_START_MS: u64 = 300;
/// Jog target is committed (one `SeekTo`) once no click arrived for this long.
pub const JOG_COMMIT_IDLE_MS: u64 = 120;
/// Velocity estimator forgets after this much idle time (also resets on reversal).
pub const VELOCITY_IDLE_RESET_MS: u64 = 350;
/// EMA weight of the newest inter-click interval.
pub const VELOCITY_EMA_ALPHA: f64 = 0.5;
/// Jog acceleration curve: (clicks per second, multiplier of `fine_step_ms`). Old UI_02
/// curve (ms) divided by 100 ms. Log-linear interpolation, clamped at both ends.
pub const JOG_CURVE: [(f64, f64); 7] = [
    (0.0, 1.0),
    (5.0, 1.0),
    (10.0, 2.5),
    (16.0, 7.5),
    (24.0, 25.0),
    (36.0, 80.0),
    (50.0, 200.0),
];
/// Jog steps on tracks at least this long are tripled above the first rung ...
pub const JOG_LONG_FORM_MS: u64 = 30 * 60 * 1000;
/// ... and capped at this many ms per click.
pub const JOG_LONG_FORM_CAP_MS: u64 = 60_000;
/// A jog click never exceeds this share of the duration (but never less than `fine_step_ms`).
pub const JOG_MAX_STEP_PERCENT: u64 = 5;
/// Tempo knob: clicks per second thresholds for the 1% / 2% / 5% / 10% steps.
pub const TEMPO_STEP_THRESHOLDS: [f64; 3] = [6.0, 12.0, 24.0];
/// Tempo knob steps in permille for the four speed bands.
pub const TEMPO_STEPS_PERMILLE: [i32; 4] = [10, 20, 50, 100];
/// After landing on exactly 100% further clicks in the same direction are swallowed this long.
pub const TEMPO_STICKY_MS: u64 = 300;
/// FF/RW backlight stays in its "pressed" level at least this long even for a quick tap.
pub const FFRW_LED_MIN_PRESSED_MS: u64 = TRANSPORT_LED_MIN_PRESSED_MS;
/// FF/RW backlights (WARP_21): low-intensity purple standing by, high-intensity purple while
/// the button is held. **Guesses: tune on the CM4.** Distinct from the white PlayMode level
/// (which keeps `TRANSPORT_LED_*`).
pub const FFRW_LED_IDLE: Rgb = Rgb::new(60, 0, 90);
pub const FFRW_LED_PRESSED: Rgb = Rgb::new(200, 0, 255);
/// FF backlight pulses **blue** while the set speed is below 100% but above 0 (WARP_21).
pub const FFRW_LED_SLOW_BLUE: Rgb = Rgb::new(0, 60, 255);
/// Pulse length at `|rate| == 1.0`; higher speeds swing faster, lower ones slower, then clamp.
pub const FFRW_PULSE_BASE_PERIOD_MS: u32 = 900;
pub const FFRW_PULSE_MIN_PERIOD_MS: u32 = 150;
pub const FFRW_PULSE_MAX_PERIOD_MS: u32 = 3_600;
/// Holding the PlayMode button (SW2) this long opens System > Playback settings (WARP_10).
/// A short press does nothing.
pub const PLAYMODE_HOLD_MS: u64 = 700;
/// Holding Play this long while warped or seeking resets to pure 100%.
pub const PLAY_RESET_HOLD_MS: u64 = 800;
/// Popup after the reset.
pub const PLAY_RESET_POPUP: &str = "100% pure";
/// An optimistic (UI-set) speed overrides a stale engine status for this long.
pub const DESIRED_RATE_OVERRIDE_MS: u64 = 600;

/// True once the engine plays negative speeds anywhere in the file (WARP_11 landed: decode
/// worker, back-windows, MP3 index). The tap ladder's negative rungs are only reachable while
/// this is true; a regression test pins both halves, so a partial revert of WARP_11 that
/// flips this off cannot leave the UI offering reverse. Set by WARP_13.
pub const LONG_REVERSE_AVAILABLE: bool = true;

/// Shared signed tap-ladder rungs (permille), descending. Zero is never a rung.
pub const LADDER_RUNGS: [i32; 16] = [
    2000, 1750, 1500, 1250, 1000, 750, 500, 250, -250, -500, -750, -1000, -1250, -1500, -1750,
    -2000,
];

// ---------------------------------------------------------------------------------------
// HoldTap
// ---------------------------------------------------------------------------------------

/// What a [`HoldTap`] edge or tick produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldTapEvent {
    /// Released before the hold threshold ([`HOLD_START_MS`] by default).
    Tap,
    /// Still down at the hold threshold (exactly once per press).
    HoldStart,
    /// Released after a hold started.
    HoldEnd,
}

/// Tap / hold classifier for one button. Releases without a press and presses while already
/// down (contact bounce, a lost edge) are ignored.
#[derive(Debug, Clone)]
pub struct HoldTap {
    down_at: Option<u64>,
    hold_fired: bool,
    /// Hold threshold: [`HOLD_START_MS`] unless built with [`Self::with_threshold`].
    hold_ms: u64,
}

impl Default for HoldTap {
    fn default() -> Self {
        Self::with_threshold(HOLD_START_MS)
    }
}

impl HoldTap {
    pub fn new() -> Self {
        Self::default()
    }

    /// A classifier with its own hold threshold (e.g. [`PLAYMODE_HOLD_MS`]).
    pub fn with_threshold(hold_ms: u64) -> Self {
        Self {
            down_at: None,
            hold_fired: false,
            hold_ms,
        }
    }

    pub fn is_down(&self) -> bool {
        self.down_at.is_some()
    }

    /// Time of the current press, if the button is down.
    pub fn down_since(&self) -> Option<u64> {
        self.down_at
    }

    /// Press or release edge at `now_ms`. A release at or after the threshold without an
    /// intervening [`Self::on_tick`] reports `HoldStart` then `HoldEnd` (the tick was late).
    pub fn on_edge(&mut self, down: bool, now_ms: u64) -> Vec<HoldTapEvent> {
        if down {
            if self.down_at.is_none() {
                self.down_at = Some(now_ms);
                self.hold_fired = false;
            }
            return Vec::new();
        }
        let Some(since) = self.down_at.take() else {
            return Vec::new();
        };
        let fired = std::mem::take(&mut self.hold_fired);
        if fired {
            vec![HoldTapEvent::HoldEnd]
        } else if now_ms.saturating_sub(since) < self.hold_ms {
            vec![HoldTapEvent::Tap]
        } else {
            vec![HoldTapEvent::HoldStart, HoldTapEvent::HoldEnd]
        }
    }

    /// Time passes: fires `HoldStart` once when the press reaches [`HOLD_START_MS`].
    pub fn on_tick(&mut self, now_ms: u64) -> Option<HoldTapEvent> {
        let since = self.down_at?;
        if !self.hold_fired && now_ms.saturating_sub(since) >= self.hold_ms {
            self.hold_fired = true;
            Some(HoldTapEvent::HoldStart)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------------------
// Tap ladder
// ---------------------------------------------------------------------------------------

/// Next rung from the **current set speed** (no memory of earlier taps, no tap window).
/// `dir > 0` = FF: the smallest rung strictly above `current_permille`; `dir < 0` = RW: the
/// largest rung strictly below. `None` at the ends (or `dir == 0`).
pub fn ladder_step(current_permille: i32, dir: i8) -> Option<i32> {
    if dir > 0 {
        LADDER_RUNGS
            .iter()
            .rev()
            .copied()
            .find(|r| *r > current_permille)
    } else if dir < 0 {
        LADDER_RUNGS.iter().copied().find(|r| *r < current_permille)
    } else {
        None
    }
}

/// [`ladder_step`] limited to what the engine build can play: without long reverse the
/// negative (and zero) rungs are not reachable.
pub fn ladder_step_with(current_permille: i32, dir: i8, long_reverse: bool) -> Option<i32> {
    ladder_step(current_permille, dir).filter(|r| long_reverse || *r > 0)
}

/// [`ladder_step_with`] for this build ([`LONG_REVERSE_AVAILABLE`]).
pub fn ladder_step_enabled(current_permille: i32, dir: i8) -> Option<i32> {
    ladder_step_with(current_permille, dir, LONG_REVERSE_AVAILABLE)
}

/// Popup text of a tap that set `permille` ("1.25x", "-0.25x").
pub fn tap_rate_text(permille: i32) -> String {
    format!("{:.2}x", f64::from(permille) / 1000.0)
}

/// Popup text of a tap at the end of the ladder ("2.00x max", "-2.00x max").
pub fn tap_end_text(permille: i32) -> String {
    format!("{} max", tap_rate_text(permille))
}

// ---------------------------------------------------------------------------------------
// Velocity + jog acceleration
// ---------------------------------------------------------------------------------------

/// Click-rate estimator (clicks per second, EMA over inter-click intervals), shared by jog
/// and tempo. Rotations carry no hardware timestamp; the caller passes drain time.
#[derive(Debug, Clone, Default)]
pub struct Velocity {
    last_ms: Option<u64>,
    dir: i8,
    ema: Option<f64>,
}

impl Velocity {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Register `count` clicks in direction `dir` at `now_ms`; returns the smoothed rate in
    /// clicks/s (0.0 for the first click after a reset, idle gap or reversal).
    pub fn update(&mut self, now_ms: u64, dir: i8, count: u32) -> f64 {
        let dir = dir.signum();
        let restart = match self.last_ms {
            None => true,
            Some(t) => now_ms.saturating_sub(t) >= VELOCITY_IDLE_RESET_MS || dir != self.dir,
        };
        if restart {
            self.ema = None;
            self.last_ms = Some(now_ms);
            self.dir = dir;
            // Several clicks in one drain still show some speed.
            if count > 1 {
                let inst = (count - 1) as f64 * 1000.0;
                self.ema = Some(inst);
            }
            return self.ema.unwrap_or(0.0);
        }
        let dt = now_ms.saturating_sub(self.last_ms.unwrap_or(now_ms)).max(1) as f64;
        let inst = count.max(1) as f64 * 1000.0 / dt;
        let v = match self.ema {
            None => inst,
            Some(prev) => VELOCITY_EMA_ALPHA * inst + (1.0 - VELOCITY_EMA_ALPHA) * prev,
        };
        self.ema = Some(v);
        self.last_ms = Some(now_ms);
        v
    }
}

/// Multiplier of the finest step at `v` clicks/s (log-linear between [`JOG_CURVE`] points).
pub fn jog_multiplier(v: f64) -> f64 {
    let first = JOG_CURVE[0];
    let last = JOG_CURVE[JOG_CURVE.len() - 1];
    if v <= first.0 {
        return first.1;
    }
    if v >= last.0 {
        return last.1;
    }
    for w in JOG_CURVE.windows(2) {
        let (v0, m0) = w[0];
        let (v1, m1) = w[1];
        if v <= v1 {
            let t = (v - v0) / (v1 - v0);
            return (m0.ln() + t * (m1.ln() - m0.ln())).exp();
        }
    }
    last.1
}

/// Jog step per click. `fine_step_ms` is derived live from the resolved Fine time unit
/// (UI_16 §5), so the menu setting changes the very next click.
#[derive(Debug, Clone)]
pub struct JogAccel {
    fine_step_ms: u64,
    vel: Velocity,
}

impl JogAccel {
    pub fn new(fine_step_ms: u64) -> Self {
        Self {
            fine_step_ms: fine_step_ms.max(1),
            vel: Velocity::new(),
        }
    }

    pub fn fine_step_ms(&self) -> u64 {
        self.fine_step_ms
    }

    pub fn set_fine_step_ms(&mut self, ms: u64) {
        self.fine_step_ms = ms.max(1);
    }

    /// Step magnitude (ms of media) for **one** click registered now. The first click of a
    /// gesture (or after reversal / 350 ms idle) is exactly `fine_step_ms`.
    pub fn click(&mut self, now_ms: u64, dir: i8, count: u32, duration_ms: Option<u64>) -> u64 {
        let v = self.vel.update(now_ms, dir, count);
        self.step_for(v, duration_ms)
    }

    /// Pure step for an already known velocity.
    pub fn step_for(&self, v: f64, duration_ms: Option<u64>) -> u64 {
        let mult = jog_multiplier(v);
        let mut step = (self.fine_step_ms as f64 * mult).round() as u64;
        let long_form = duration_ms.is_some_and(|d| d >= JOG_LONG_FORM_MS);
        if long_form && mult > 1.0 {
            step = (step.saturating_mul(3)).min(JOG_LONG_FORM_CAP_MS.max(self.fine_step_ms));
        }
        if let Some(d) = duration_ms.filter(|d| *d > 0) {
            let cap = (d * JOG_MAX_STEP_PERCENT / 100).max(self.fine_step_ms);
            step = step.min(cap);
        }
        step
    }
}

// ---------------------------------------------------------------------------------------
// Tempo knob
// ---------------------------------------------------------------------------------------

/// Tempo step (permille) for the current click rate: 1% / 2% / 5% / 10%.
pub fn tempo_step_permille(v: f64) -> i32 {
    let band = TEMPO_STEP_THRESHOLDS.iter().filter(|t| v >= **t).count();
    TEMPO_STEPS_PERMILLE[band]
}

/// Sticky-100% memory of the tempo knob.
#[derive(Debug, Clone, Default)]
pub struct TempoSticky {
    /// `(direction, landed_at_ms)` of the click that landed exactly on 100%.
    landed: Option<(i8, u64)>,
}

/// One tempo knob click on the signed set speed. The knob edits the **magnitude** (clockwise =
/// faster), keeps the sign and never crosses zero; magnitude range 100..=2000. Steps snap to
/// multiples of `step_permille`; a step that would jump over 100% lands on it. Returns the new
/// signed speed, or `None` when nothing changes (limit reached, or sticky 100% swallowed the
/// click).
pub fn tempo_apply_click(
    sticky: &mut TempoSticky,
    current_permille: i32,
    dir: i8,
    step_permille: i32,
    now_ms: u64,
) -> Option<i32> {
    let dir = dir.signum();
    if dir == 0 {
        return None;
    }
    let sign = if current_permille < 0 { -1 } else { 1 };
    let m = current_permille
        .abs()
        .clamp(RATE_MIN_ABS_PERMILLE, RATE_MAX_ABS_PERMILLE);
    let step = step_permille.max(1);

    if m == RATE_UNITY_PERMILLE {
        if let Some((d, at)) = sticky.landed {
            if d == dir && now_ms.saturating_sub(at) < TEMPO_STICKY_MS {
                return None;
            }
        }
    }
    sticky.landed = None;

    let next = if dir > 0 {
        (m / step + 1) * step
    } else {
        ((m + step - 1) / step - 1) * step
    };
    let mut next = next.clamp(RATE_MIN_ABS_PERMILLE, RATE_MAX_ABS_PERMILLE);
    // Never jump over 100%: stop on it.
    if (m < RATE_UNITY_PERMILLE && next > RATE_UNITY_PERMILLE)
        || (m > RATE_UNITY_PERMILLE && next < RATE_UNITY_PERMILLE)
    {
        next = RATE_UNITY_PERMILLE;
    }
    if next == m {
        return None;
    }
    if next == RATE_UNITY_PERMILLE {
        sticky.landed = Some((dir, now_ms));
    }
    Some(sign * next)
}

// ---------------------------------------------------------------------------------------
// Jog gesture
// ---------------------------------------------------------------------------------------

/// The one `SeekTo` a finished jog gesture asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit {
    /// Absolute media time inside the item `item_delta` points at.
    pub target_ms: u64,
    /// WARP_19: -1 the previous item, 0 the current one (`SeekTo`), +1 the next item
    /// (`SeekToItem`).
    pub item_delta: i8,
}

/// WARP_19: durations of the neighbouring items, only while "Seek outside track" is on (the
/// engine publishes them). `None` = no neighbour (list end, unknown length, setting off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NeighbourCtx {
    pub prev_ms: Option<u64>,
    pub next_ms: Option<u64>,
}

impl NeighbourCtx {
    pub const NONE: Self = Self { prev_ms: None, next_ms: None };
}

/// Result of a [`JogGesture::click`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JogClick {
    /// The live target after this click.
    pub target_ms: u64,
    /// The click ran into a limit (0 or `duration - SEEK_END_GUARD_MS`) and was swallowed.
    pub pinned: bool,
}

/// The UI-owned live jog target. Nothing is sent while clicks keep coming; one commit per
/// gesture after [`JOG_COMMIT_IDLE_MS`] of silence (or on [`Self::flush`]).
///
/// Limits are applied after **every** click (a click at a limit is swallowed), so the target
/// is a sequentially clamped sum, like a tape stopping at its end.
#[derive(Debug, Clone, Default)]
pub struct JogGesture {
    /// The live target, in the item `item` (0 current, -1 previous, +1 next).
    target_ms: u64,
    item: i8,
    last_click_ms: u64,
    open: bool,
}

impl JogGesture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Live target while a gesture is open (for the display).
    pub fn live_target(&self) -> Option<u64> {
        self.open.then_some(self.target_ms)
    }

    /// The item the live target is in (0 current, -1 previous, +1 next; WARP_19).
    pub fn live_item(&self) -> i8 {
        if self.open {
            self.item
        } else {
            0
        }
    }

    /// One click of `signed_step_ms`. The first click opens the gesture at `base_ms` (the
    /// dead-reckoned playhead, or the engine's pending `seek_target_ms`).
    pub fn click(
        &mut self,
        now_ms: u64,
        signed_step_ms: i64,
        base_ms: u64,
        duration_ms: Option<u64>,
    ) -> JogClick {
        self.click_with(now_ms, signed_step_ms, base_ms, duration_ms, NeighbourCtx::NONE)
    }

    /// [`Self::click`] with the neighbour durations (WARP_19). Below 0 the live target runs
    /// into the previous item (`prev - overshoot`), beyond `duration - SEEK_END_GUARD_MS` into
    /// the next one (`overshoot`); **at most one neighbour per gesture**: inside a neighbour
    /// the target is clamped to `[0, neighbour - guard]`, and a click back the other way
    /// returns to the current item the same way.
    pub fn click_with(
        &mut self,
        now_ms: u64,
        signed_step_ms: i64,
        base_ms: u64,
        duration_ms: Option<u64>,
        nb: NeighbourCtx,
    ) -> JogClick {
        if !self.open {
            self.open = true;
            self.item = 0;
            self.target_ms = clamp_seek_target_ms(base_ms, duration_ms);
        }
        self.last_click_ms = now_ms;
        let raw = self.target_ms as i128 + signed_step_ms as i128;
        let lim = |d: Option<u64>| clamp_seek_target_ms(u64::MAX, d) as i128;
        let cur_lim = lim(duration_ms);
        let clamp_to = |v: i128, d: Option<u64>| clamp_seek_target_ms(v.clamp(0, u64::MAX as i128) as u64, d);
        let (item, target, pinned) = match self.item {
            0 if raw < 0 && nb.prev_ms.is_some() => {
                // `prev - overshoot`, within the previous item's own end guard.
                let p = nb.prev_ms;
                let want = p.unwrap_or(0) as i128 + raw;
                let c = clamp_to(want, p);
                (-1, c, c as i128 != want)
            }
            0 if raw > cur_lim && duration_ms.is_some() && nb.next_ms.is_some() => {
                let want = raw - cur_lim;
                let c = clamp_to(want, nb.next_ms);
                (1, c, c as i128 != want)
            }
            0 => {
                let c = clamp_to(raw, duration_ms);
                (0, c, c as i128 != raw)
            }
            -1 => {
                let p = nb.prev_ms;
                let plim = lim(p);
                if raw > plim && duration_ms.is_some() {
                    // Back toward the current item.
                    (0, clamp_to(raw - plim, duration_ms), false)
                } else {
                    let c = clamp_to(raw, p);
                    (-1, c, c as i128 != raw)
                }
            }
            _ => {
                let n = nb.next_ms;
                if raw < 0 {
                    (0, clamp_to(cur_lim + raw, duration_ms), false)
                } else {
                    let c = clamp_to(raw, n);
                    (1, c, c as i128 != raw)
                }
            }
        };
        self.item = item;
        self.target_ms = target;
        JogClick {
            target_ms: target,
            pinned,
        }
    }

    /// Commit once `JOG_COMMIT_IDLE_MS` passed since the last click (exactly once per gesture).
    pub fn tick(&mut self, now_ms: u64) -> Option<Commit> {
        if self.open && now_ms.saturating_sub(self.last_click_ms) >= JOG_COMMIT_IDLE_MS {
            return self.flush();
        }
        None
    }

    /// Commit immediately (leaving Jog mode, Back, navigation away).
    pub fn flush(&mut self) -> Option<Commit> {
        if !self.open {
            return None;
        }
        self.open = false;
        Some(Commit {
            target_ms: self.target_ms,
            item_delta: self.item,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Jog display model (WARP_16)
// ---------------------------------------------------------------------------------------

/// Target and actual are "landed" when closer than this (also hides the `now` line).
pub const JOG_NEAR_MS: u64 = 50;
/// The target readout stays this long after the playhead has landed.
pub const JOG_LANDED_HOLD_MS: u64 = 1_500;
/// The target marker merges into the playhead and disappears this long after landing.
pub const JOG_MARKER_HOLD_MS: u64 = 600;
/// A status older than this wins over the dead-reckoned playhead.
pub const JOG_DEADRECKON_MAX_AGE_MS: u64 = 600;
/// How far a status sample can legitimately trail the dead-reckoned playhead at a warp: the
/// engine's fast status cadence (100 ms) plus the delivery tick, charged at the current rate.
/// UI_15: with the scan ladder up to 4x the usual 8x scan this is what keeps a *scan* (or a
/// high-tempo playback) from pulling the picture backwards on every status.
pub const JOG_STATUS_LAG_MS: u64 = 120;
/// Minimum back-step tolerance for the floor guard at unity rates (unchanged pre-UI_15 value).
pub const JOG_STALE_MIN_MS: u64 = 400;
/// The bar end flashes this long when a click is swallowed at a limit.
pub const JOG_LIMIT_FLASH_MS: u64 = 200;
/// A committed jump whose status never showed a target (paused silent seek) counts as landed
/// after this long.
pub const JOG_COMMIT_SETTLE_MS: u64 = 500;
/// Displayed times are quantised to this (limits repaints to 10 Hz).
pub const JOG_DISPLAY_QUANT_MS: u64 = 100;
/// Show the committed target on the main timeline while a chase runs after leaving Jog mode
/// (one switch if the owner ever wants it off).
pub const JOG_SHOW_CHASE_OUTSIDE_JOG: bool = true;

/// The scan-speed readout for the held-scan ladder (`1000` -> `"SCAN 100%"`), shown on the
/// status line while the encoder steps a running scan (UI_15).
///
/// It is the **selected** value, not the instantaneous rate: the ladder is relative to the
/// profile's usual speed for the phase (4x, or 8x once boosted), so only the percentage is
/// known to the UI. The Adjust band's scan readout (`>> 4.0x`) keeps showing the rate actually
/// being played.
pub fn scan_speed_text(permille: i32) -> String {
    format!("SCAN {}%", permille / 10)
}

/// What the UI mirrors from an engine status for the jog display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatusSample {
    pub pos_ms: u64,
    /// Instantaneous rate, signed permille.
    pub rate_permille: i32,
    pub playing: bool,
    /// UI_15: an FF/RW hold scan is running (`session.scan != None`).
    ///
    /// A scan started from **paused** plays audio while the engine keeps `transport = Paused`,
    /// so `playing` alone is not "the picture moves". Dead reckoning must follow any moving
    /// rate, or a paused scan freezes the timecode and the next status snaps it forward.
    pub scanning: bool,
    pub seek_target_ms: Option<u64>,
    pub duration_ms: Option<u64>,
}

impl StatusSample {
    /// The playhead is moving (playing, or scanning from paused).
    fn moving(&self) -> bool {
        self.playing || self.scanning
    }
}

/// Everything the Now Playing screen needs to draw the jog target next to the actual playhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JogView {
    /// Where the bar's target marker sits. While the live target is in a neighbouring item
    /// (`item_delta != 0`) this is the clamped end of the bar (0 or the duration).
    pub target_ms: u64,
    /// WARP_19: -1 / +1 while the live target is in the previous / next item (the readout is
    /// prefixed `<` / `>` and shows `item_target_ms`), else 0.
    pub item_delta: i8,
    /// The live target inside that neighbouring item (equals `target_ms` when 0).
    pub item_target_ms: u64,
    pub actual_ms: u64,
    pub duration_ms: Option<u64>,
    /// The time readout shows the target (gesture open, a jump running, or the landed hold).
    pub readout: bool,
    /// Second line: the actual playhead and the signed distance (playing and far apart).
    pub show_now: bool,
    /// Bar: amber target marker and the band to the playhead (never while paused).
    pub show_marker: bool,
    /// Paused: one marker only, the playhead tick sits on the target.
    pub paused: bool,
    /// A click was swallowed at a limit just now: `Some(true)` = the end, `Some(false)` = 0.
    pub limit_flash_end: Option<bool>,
}

#[derive(Debug, Clone, Copy)]
struct Committed {
    target_ms: u64,
    at_ms: u64,
    /// The engine has reported `seek_target_ms = Some` for it.
    seen: bool,
    /// WARP_19: the target is in a neighbouring item (-1 / +1). The band only shows this
    /// file, so the picture clamps to 0 / the duration instead of the raw target.
    item_delta: i8,
}

/// Pure display model: dead-reckoned actual playhead, committed target, landing and limit
/// flash. Clock injected (`now_ms`), no I/O.
#[derive(Debug, Clone, Default)]
pub struct JogDisplay {
    sample: Option<(StatusSample, u64)>,
    /// Never show an actual earlier than this while the rate is forward (a stale status must
    /// not pull the playhead tick backwards).
    floor_ms: Option<u64>,
    committed: Option<Committed>,
    landed_at: Option<u64>,
    last_target: u64,
    flash: Option<(u64, bool)>,
}

impl JogDisplay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Status-derived actual playhead at `now_ms` (dead-reckoned between statuses).
    ///
    /// UI_15: any **moving** playhead dead-reckons — a scan from paused plays audio while the
    /// transport stays `Paused`, so the picture follows the scan rate there too.
    pub fn actual_ms(&self, now_ms: u64) -> Option<u64> {
        let (s, at) = self.sample?;
        let age = now_ms.saturating_sub(at);
        let mut pos = s.pos_ms;
        if s.moving() && age < JOG_DEADRECKON_MAX_AGE_MS {
            let moved = i64::from(s.rate_permille) * age as i64 / 1000;
            pos = (s.pos_ms as i64 + moved).max(0) as u64;
            if s.rate_permille > 0 {
                if let Some(f) = self.floor_ms {
                    pos = pos.max(f);
                }
            }
        }
        if let Some(d) = s.duration_ms.filter(|d| *d > 0) {
            pos = pos.min(d);
        }
        Some(pos)
    }

    /// Fold in an engine status.
    pub fn on_status(&mut self, s: StatusSample, now_ms: u64) {
        let shown = self.actual_ms(now_ms);
        // A status is a sample of the playhead one fast-status window ago; while the playhead
        // moves forward that sample may legitimately trail what is already drawn (by roughly
        // `rate x lag`), so keep the drawn value instead of stepping back over it. Rates above
        // unity (scans, fast tempos) widen the window with the rate; a genuine backward seek
        // still shows, because the rate is negative then and no floor is kept.
        let lag_tolerance = (i64::from(s.rate_permille).unsigned_abs()
            * JOG_STATUS_LAG_MS
            / 1000)
            .max(JOG_STALE_MIN_MS);
        self.floor_ms = match shown {
            Some(sh)
                if s.moving()
                    && s.rate_permille > 0
                    && s.pos_ms < sh
                    && sh - s.pos_ms < lag_tolerance =>
            {
                Some(sh)
            }
            _ => None,
        };
        self.sample = Some((s, now_ms));
        match (self.committed.as_mut(), s.seek_target_ms) {
            (Some(c), Some(t)) => {
                c.seen = true;
                c.target_ms = t;
                self.last_target = t;
            }
            (Some(c), None) => {
                if c.seen || now_ms.saturating_sub(c.at_ms) >= JOG_COMMIT_SETTLE_MS {
                    self.land(now_ms);
                }
            }
            (None, Some(t)) if JOG_SHOW_CHASE_OUTSIDE_JOG => {
                // A chase the UI did not start (`plippa-np seekto`, a second UI).
                self.committed = Some(Committed {
                    target_ms: t,
                    at_ms: now_ms,
                    seen: true,
                    item_delta: 0,
                });
                self.landed_at = None;
                self.last_target = t;
            }
            _ => {}
        }
    }

    fn land(&mut self, now_ms: u64) {
        self.committed = None;
        self.landed_at = Some(now_ms);
    }

    /// Where a new gesture starts: the running chase's target (no jump back), else the actual.
    pub fn gesture_base(&self, now_ms: u64) -> u64 {
        match self.committed {
            Some(c) => c.target_ms,
            None => self.actual_ms(now_ms).unwrap_or(0),
        }
    }

    /// A jog click (resets the landing state; a swallowed click flashes the bar end).
    pub fn on_click(&mut self, target_ms: u64, pinned: bool, at_end: bool, now_ms: u64) {
        self.landed_at = None;
        self.committed = None;
        self.last_target = target_ms;
        if pinned {
            self.flash = Some((now_ms + JOG_LIMIT_FLASH_MS, at_end));
        }
    }

    /// The gesture committed its one `SeekTo` (WARP_19: `item_delta` when it left this item).
    pub fn on_commit(&mut self, target_ms: u64, item_delta: i8, now_ms: u64) {
        self.committed = Some(Committed {
            target_ms,
            at_ms: now_ms,
            seen: false,
            item_delta,
        });
        self.landed_at = None;
        self.last_target = target_ms;
    }

    /// The target of a seek the engine has **not** landed yet: `Some((target, item_delta))`
    /// while it runs, `None` once it has landed (the landed hold keeps only the readout and
    /// the marker, not the picture).
    pub fn running_seek(&self) -> Option<(u64, i8)> {
        self.committed.map(|c| (c.target_ms, c.item_delta))
    }

    /// The gesture was thrown away (engine offline, track changed).
    pub fn discard(&mut self) {
        self.committed = None;
        self.landed_at = None;
    }

    /// The playhead moved to a different item: drop the dead-reckoning baseline too.
    ///
    /// `actual_ms` extrapolates from the last sample, and `on_status` keeps a forward floor
    /// when a new sample only trails that extrapolation. Both are only meaningful **within**
    /// one item: carried across a track change they hold the old item's playhead (and its
    /// rate) for up to one status interval, which reads as the playhead moving once and then
    /// snapping to the new item. Call before folding in the first status of the new item.
    pub fn reset_baseline(&mut self) {
        self.sample = None;
        self.floor_ms = None;
    }

    /// Housekeeping: a committed jump that reached its target (no open gesture) has landed.
    pub fn tick(&mut self, gesture_open: bool, now_ms: u64) {
        if gesture_open {
            return;
        }
        if let (Some(c), Some(a)) = (self.committed, self.actual_ms(now_ms)) {
            let settled = c.seen || now_ms.saturating_sub(c.at_ms) >= JOG_COMMIT_SETTLE_MS;
            if settled && a.abs_diff(c.target_ms) < JOG_NEAR_MS {
                self.land(now_ms);
            }
        }
        if self.flash.is_some_and(|(until, _)| now_ms >= until) {
            self.flash = None;
        }
    }

    /// The view to draw, if anything jog-related is visible.
    pub fn view(&self, gesture_target: Option<u64>, now_ms: u64) -> Option<JogView> {
        self.view_item(gesture_target, 0, now_ms)
    }

    /// [`Self::view`] with the item the gesture's live target is in (WARP_19).
    pub fn view_item(
        &self,
        gesture_target: Option<u64>,
        gesture_item: i8,
        now_ms: u64,
    ) -> Option<JogView> {
        let (s, _) = self.sample?;
        let actual = self.actual_ms(now_ms)?;
        let open = gesture_target.is_some();
        let landed_recent = self
            .landed_at
            .is_some_and(|at| now_ms.saturating_sub(at) < JOG_LANDED_HOLD_MS);
        let target = match (gesture_target, self.committed) {
            (Some(t), _) => t,
            (None, Some(c)) => c.target_ms,
            (None, None) if landed_recent => self.last_target,
            _ => return None,
        };
        let running = open || self.committed.is_some();
        let marker_recent = self
            .landed_at
            .is_some_and(|at| now_ms.saturating_sub(at) < JOG_MARKER_HOLD_MS);
        let paused = !s.moving();
        let q = |v: u64| v - v % JOG_DISPLAY_QUANT_MS;
        // A target in another item: the bar shows its clamped end, the readout the real value.
        let item = if gesture_target.is_some() { gesture_item } else { 0 };
        let bar_target = match item {
            -1 => 0,
            1 => s.duration_ms.unwrap_or(target),
            _ => target,
        };
        let far = actual.abs_diff(bar_target) >= JOG_NEAR_MS || item != 0;
        Some(JogView {
            target_ms: bar_target,
            item_delta: item,
            item_target_ms: target,
            actual_ms: q(actual),
            duration_ms: s.duration_ms,
            readout: true,
            show_now: !paused && far && running,
            show_marker: !paused && (running || marker_recent),
            paused,
            limit_flash_end: self
                .flash
                .filter(|(until, _)| now_ms < *until)
                .map(|(_, end)| end),
        })
    }
}

// ---------------------------------------------------------------------------------------
// FF/RW LED, optimistic speed, settings
// ---------------------------------------------------------------------------------------

/// FF/RW backlight level: `None` = off (control unavailable), the pressed colour while held and
/// for at least [`FFRW_LED_MIN_PRESSED_MS`] after the press, else the idle level.
pub fn ffrw_led_level(
    available: bool,
    pressed_at: Option<u64>,
    released_at: Option<u64>,
    now_ms: u64,
) -> Option<Rgb> {
    if !available {
        return None;
    }
    if let Some(p) = pressed_at {
        let held = released_at.map_or(true, |r| r < p);
        if held || now_ms.saturating_sub(p) < FFRW_LED_MIN_PRESSED_MS {
            return Some(FFRW_LED_PRESSED);
        }
    }
    Some(FFRW_LED_IDLE)
}

/// Pulse period (ms) for a set speed of `mag_permille` (absolute permille, 0 = none):
/// inversely proportional to the speed, so a faster playback pulses faster; clamped.
fn ffrw_pulse_period(mag_permille: u32) -> u32 {
    let mag = mag_permille.max(1) as u64;
    let period = (u64::from(FFRW_PULSE_BASE_PERIOD_MS) * 1_000) / mag;
    period.clamp(
        u64::from(FFRW_PULSE_MIN_PERIOD_MS),
        u64::from(FFRW_PULSE_MAX_PERIOD_MS),
    ) as u32
}

fn purple_pulse(mag_permille: u32) -> TransportPulse {
    TransportPulse {
        lo: FFRW_LED_IDLE,
        hi: FFRW_LED_PRESSED,
        period_ms: ffrw_pulse_period(mag_permille),
    }
}

/// FF backlight pulse for the set speed (WARP_21): above 100% it swings low→high purple,
/// faster the faster the speed; between 0 and 100% the same swing in blue, slower the slower.
/// `None` at exactly 100% / 0 (steady standing-by purple) or for negative speeds (the RW
/// LED carries those).
pub fn ffrw_pulse_ff(permille: i32) -> Option<TransportPulse> {
    let mag = permille.unsigned_abs();
    if permille > RATE_UNITY_PERMILLE {
        Some(purple_pulse(mag))
    } else if permille > 0 && permille < RATE_UNITY_PERMILLE {
        Some(TransportPulse {
            lo: Rgb::blend(FFRW_LED_SLOW_BLUE, Rgb::new(0, 0, 0), 0.35),
            hi: FFRW_LED_SLOW_BLUE,
            period_ms: ffrw_pulse_period(mag),
        })
    } else {
        None
    }
}

/// RW backlight pulse for the set speed (WARP_21): any negative speed pulses purple,
/// including exactly -100% and beyond (mirrors FF; there is no steady reverse state).
pub fn ffrw_pulse_rw(permille: i32) -> Option<TransportPulse> {
    (permille < 0).then(|| purple_pulse(permille.unsigned_abs()))
}

/// Optimistic set speed shown / stepped from before the engine status confirms it.
#[derive(Debug, Clone, Copy, Default)]
pub struct DesiredRate {
    set: Option<(i32, u64)>,
}

impl DesiredRate {
    pub fn set(&mut self, permille: i32, now_ms: u64) {
        self.set = Some((permille, now_ms));
    }

    pub fn clear(&mut self) {
        self.set = None;
    }

    /// Re-stamp a pending override (engine offline: nothing can confirm it).
    pub fn touch(&mut self, now_ms: u64) {
        if let Some((_, at)) = self.set.as_mut() {
            *at = now_ms;
        }
    }

    /// The speed to display / step from: the override while fresh (and not yet confirmed),
    /// else the engine's `target_rate_permille`.
    pub fn effective(&mut self, status_target_permille: i32, now_ms: u64) -> i32 {
        if let Some((v, at)) = self.set {
            if v == status_target_permille || now_ms.saturating_sub(at) >= DESIRED_RATE_OVERRIDE_MS
            {
                self.set = None;
            } else {
                return v;
            }
        }
        status_target_permille
    }

    /// Non-mutating peek of the fresh override.
    pub fn fresh(&self, now_ms: u64) -> Option<i32> {
        self.set
            .filter(|(_, at)| now_ms.saturating_sub(*at) < DESIRED_RATE_OVERRIDE_MS)
            .map(|(v, _)| v)
    }
}

/// `play_mode.json` helpers for the two warp-mode choices (WARP_10 owns the file).
pub fn warp_settings_to_json(s: &WarpModeSettings) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "{}".into())
}

/// Missing / corrupt text = defaults (never chosen).
pub fn warp_settings_from_json(text: &str) -> WarpModeSettings {
    serde_json::from_str(text).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// Play key (WARP_08)
// ---------------------------------------------------------------------------------------

/// When a Play press toggles, fixed **at the press edge**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayKeyMode {
    /// Pure and no seek active: toggle on press (today's path); a hold does nothing.
    Immediate,
    /// Warped or seeking: toggle on release (< 800 ms); at 800 ms reset to pure instead.
    Deferred,
}

/// What the Play key asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayKeyAction {
    /// Toggle Play / Pause.
    Toggle,
    /// Reset the speed family to pure 100% (no toggle).
    Reset,
}

#[derive(Debug, Clone, Copy)]
struct PlayPress {
    mode: PlayKeyMode,
    down_at: u64,
    reset_fired: bool,
}

/// Play button timing FSM (pure, injected clock).
#[derive(Debug, Clone, Default)]
pub struct PlayKey {
    press: Option<PlayPress>,
}

impl PlayKey {
    pub fn new() -> Self {
        Self::default()
    }

    /// Press or release at `now_ms`. `warped_or_seeking` is read **only on the press edge**
    /// (later changes never alter that press's mode). A repeated press while down is ignored.
    pub fn on_edge(
        &mut self,
        down: bool,
        now_ms: u64,
        warped_or_seeking: bool,
    ) -> Option<PlayKeyAction> {
        if down {
            if self.press.is_some() {
                return None;
            }
            let mode = if warped_or_seeking {
                PlayKeyMode::Deferred
            } else {
                PlayKeyMode::Immediate
            };
            self.press = Some(PlayPress {
                mode,
                down_at: now_ms,
                reset_fired: false,
            });
            return (mode == PlayKeyMode::Immediate).then_some(PlayKeyAction::Toggle);
        }
        let p = self.press.take()?;
        if p.mode != PlayKeyMode::Deferred || p.reset_fired {
            return None;
        }
        if now_ms.saturating_sub(p.down_at) >= PLAY_RESET_HOLD_MS {
            // The tick that should have fired the reset was late: still exactly one reset.
            Some(PlayKeyAction::Reset)
        } else {
            Some(PlayKeyAction::Toggle)
        }
    }

    /// Time passes: a deferred press reaching [`PLAY_RESET_HOLD_MS`] fires the reset once.
    pub fn on_tick(&mut self, now_ms: u64) -> Option<PlayKeyAction> {
        let p = self.press.as_mut()?;
        if p.mode == PlayKeyMode::Deferred
            && !p.reset_fired
            && now_ms.saturating_sub(p.down_at) >= PLAY_RESET_HOLD_MS
        {
            p.reset_fired = true;
            Some(PlayKeyAction::Reset)
        } else {
            None
        }
    }

    /// Would an edge now produce a toggle? (Pure query; the loop uses it for the optimistic
    /// Play LED before the app handles the event.)
    pub fn would_toggle(&self, down: bool, now_ms: u64, warped_or_seeking: bool) -> bool {
        if down {
            self.press.is_none() && !warped_or_seeking
        } else {
            self.press.is_some_and(|p| {
                p.mode == PlayKeyMode::Deferred
                    && !p.reset_fired
                    && now_ms.saturating_sub(p.down_at) < PLAY_RESET_HOLD_MS
            })
        }
    }
}

// ---------------------------------------------------------------------------------------
// Outgoing queue coalescing
// ---------------------------------------------------------------------------------------

fn playback_cmd(o: &UiOutgoing) -> Option<&PlaybackCommand> {
    match o {
        UiOutgoing::Ipc(IpcMessage::PlaybackCommand(c)) => Some(c),
        _ => None,
    }
}

/// Which "slot" a command occupies: only the newest of a slot is kept.
fn slot(c: &PlaybackCommand) -> Option<u8> {
    match c {
        PlaybackCommand::SetRate { .. }
        | PlaybackCommand::ResetRate
        | PlaybackCommand::ResetRateInstant => Some(0),
        PlaybackCommand::SeekTo { .. } => Some(1),
        PlaybackCommand::SetSpeedWarpMode(_) => Some(2),
        PlaybackCommand::SetSeekWarpMode(_) => Some(3),
        PlaybackCommand::ScanRate { .. } => Some(4),
        _ => None,
    }
}

/// Append `new` to `queue`, coalescing the warp commands: at most one pending rate command
/// (`SetRate` / `ResetRate` / `ResetRateInstant`), `SeekTo`, `SetSpeedWarpMode`,
/// `SetSeekWarpMode`, `ScanRate` (newest wins); a
/// `ScanStop` cancels a still-pending `ScanStart` (both vanish); a `ScanStop` alone is never
/// dropped; a newer `ScanStart` replaces an older pending one and drops a pending `ScanRate`
/// (a scan that has not started yet has nothing to re-target).
pub fn push_coalesced(queue: &mut Vec<UiOutgoing>, new: UiOutgoing) {
    if let Some(cmd) = playback_cmd(&new) {
        if let Some(s) = slot(cmd) {
            queue.retain(|o| playback_cmd(o).and_then(slot) != Some(s));
        } else if matches!(cmd, PlaybackCommand::ScanStop) {
            let had_start = queue
                .iter()
                .any(|o| matches!(playback_cmd(o), Some(PlaybackCommand::ScanStart { .. })));
            if had_start {
                queue.retain(|o| {
                    !matches!(
                        playback_cmd(o),
                        Some(PlaybackCommand::ScanStart { .. })
                            | Some(PlaybackCommand::ScanRate { .. })
                    )
                });
                return;
            }
        } else if matches!(cmd, PlaybackCommand::ScanStart { .. }) {
            queue.retain(|o| {
                !matches!(
                    playback_cmd(o),
                    Some(PlaybackCommand::ScanStart { .. }) | Some(PlaybackCommand::ScanRate { .. })
                )
            });
        }
    }
    queue.push(new);
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_core::{ScanDir, WarpMode};

    fn ipc(c: PlaybackCommand) -> UiOutgoing {
        UiOutgoing::Ipc(IpcMessage::PlaybackCommand(c))
    }

    #[test]
    fn ladder_table() {
        // Every rung: FF/RW neighbours.
        for (i, r) in LADDER_RUNGS.iter().enumerate() {
            let up = if i == 0 {
                None
            } else {
                Some(LADDER_RUNGS[i - 1])
            };
            let down = LADDER_RUNGS.get(i + 1).copied();
            assert_eq!(ladder_step(*r, 1), up, "ff from {r}");
            assert_eq!(ladder_step(*r, -1), down, "rw from {r}");
        }
        assert_eq!(ladder_step(870, 1), Some(1000));
        assert_eq!(ladder_step(870, -1), Some(750));
        assert_eq!(ladder_step(150, 1), Some(250));
        assert_eq!(ladder_step(150, -1), Some(-250));
        assert_eq!(ladder_step(-100, 1), Some(250));
        assert_eq!(ladder_step(-100, -1), Some(-250));
        for c in -2100..=2100 {
            assert_ne!(ladder_step(c, 1), Some(0));
            assert_ne!(ladder_step(c, -1), Some(0));
        }
        // Stateless: two FF taps from 1.0 give 1.25 then 1.5; one tap at 1.5 gives 1.75.
        let a = ladder_step(1000, 1).unwrap();
        assert_eq!((a, ladder_step(a, 1)), (1250, Some(1500)));
        assert_eq!(ladder_step(1500, 1), Some(1750));
    }

    #[test]
    fn hold_tap_boundaries() {
        for (ms, want) in [
            (299, vec![HoldTapEvent::Tap]),
            (300, vec![HoldTapEvent::HoldStart, HoldTapEvent::HoldEnd]),
            (301, vec![HoldTapEvent::HoldStart, HoldTapEvent::HoldEnd]),
        ] {
            let mut h = HoldTap::new();
            assert!(h.on_edge(true, 1000).is_empty());
            assert_eq!(h.on_edge(false, 1000 + ms), want, "{ms} ms");
        }
        let mut h = HoldTap::new();
        h.on_edge(true, 0);
        assert_eq!(h.on_tick(299), None);
        assert_eq!(h.on_tick(300), Some(HoldTapEvent::HoldStart));
        assert_eq!(h.on_tick(900), None, "exactly once");
        assert_eq!(h.on_edge(false, 1000), vec![HoldTapEvent::HoldEnd]);
        // Bounce: release with no press, press while held.
        assert!(h.on_edge(false, 1100).is_empty());
        h.on_edge(true, 2000);
        assert!(h.on_edge(true, 2100).is_empty(), "down while held ignored");
        assert_eq!(
            h.on_tick(2300),
            Some(HoldTapEvent::HoldStart),
            "timing from the first down"
        );
    }

    #[test]
    fn jog_accel_first_click_and_monotone_and_scaling() {
        for fine in [10u64, 20, 50, 100, 250, 500] {
            let mut a = JogAccel::new(fine);
            assert_eq!(
                a.click(0, 1, 1, Some(10_000_000)),
                fine,
                "first click {fine}"
            );
            // Speeding click train: steps never shrink.
            let mut t = 0;
            let mut last = fine;
            for gap in [250u64, 100, 50, 25, 10] {
                t += gap;
                let s = a.click(t, 1, 1, None);
                assert!(s >= last, "{fine}: {s} < {last} at gap {gap}");
                last = s;
            }
            // Reversal and idle restart at fine.
            assert_eq!(a.click(t + 5, -1, 1, None), fine);
            a.click(t + 10, -1, 1, None);
            assert_eq!(a.click(t + 10 + 350, -1, 1, None), fine);
        }
        // Curve scales linearly with fine_step (no duration cap).
        let (a, b) = (JogAccel::new(100), JogAccel::new(10));
        for v in [0.0, 7.0, 12.0, 20.0, 30.0, 45.0, 60.0] {
            let (x, y) = (a.step_for(v, None), b.step_for(v, None));
            assert!((x as f64 - 10.0 * y as f64).abs() <= 5.0, "{v}: {x} vs {y}");
        }
        // Endpoints exact.
        assert_eq!(a.step_for(0.0, None), 100);
        assert_eq!(a.step_for(5.0, None), 100);
        assert_eq!(a.step_for(50.0, None), 20_000);
        assert_eq!(a.step_for(500.0, None), 20_000);
        // 3 minute track caps at 9 s (5%).
        assert_eq!(a.step_for(50.0, Some(180_000)), 9_000);
        // Long form: x3 above the first rung, cap 60 s; the 5% cap of a 40 min track is 120 s.
        let long = 40 * 60 * 1000;
        assert_eq!(a.step_for(2.0, Some(long)), 100, "first rung unchanged");
        assert_eq!(a.step_for(10.0, Some(long)), 750);
        assert_eq!(a.step_for(50.0, Some(long)), 60_000);
        // Never below the finest step even on a tiny track.
        assert_eq!(a.step_for(50.0, Some(1_000)), 100);
    }

    #[test]
    fn tempo_steps_and_sticky() {
        assert_eq!(tempo_step_permille(0.0), 10);
        assert_eq!(tempo_step_permille(8.0), 20);
        assert_eq!(tempo_step_permille(15.0), 50);
        assert_eq!(tempo_step_permille(40.0), 100);

        let mut s = TempoSticky::default();
        // Slow clicks: 1% steps up and down, never above 2000 or below 100.
        let mut cur = 970;
        let mut seq = vec![];
        for i in 0..3 {
            cur = tempo_apply_click(&mut s, cur, 1, 10, 1000 * i).unwrap();
            seq.push(cur);
        }
        assert_eq!(seq, vec![980, 990, 1000]);
        // Sticky: same direction within 300 ms swallowed; later or opposite passes.
        assert_eq!(tempo_apply_click(&mut s, 1000, 1, 10, 2_100), None);
        assert_eq!(tempo_apply_click(&mut s, 1000, 1, 10, 2_250), None);
        assert_eq!(tempo_apply_click(&mut s, 1000, 1, 10, 2_300), Some(1010));
        let mut s2 = TempoSticky::default();
        tempo_apply_click(&mut s2, 990, 1, 10, 0);
        assert_eq!(tempo_apply_click(&mut s2, 1000, -1, 10, 50), Some(990));
        // Fast steps snap and stop on 100%.
        let mut s3 = TempoSticky::default();
        assert_eq!(tempo_apply_click(&mut s3, 1230, 1, 50, 0), Some(1250));
        assert_eq!(tempo_apply_click(&mut s3, 960, 1, 100, 0), Some(1000));
        assert_eq!(tempo_apply_click(&mut s3, 1040, -1, 100, 5_000), Some(1000));
        // Limits.
        let mut s4 = TempoSticky::default();
        assert_eq!(tempo_apply_click(&mut s4, 2000, 1, 10, 0), None);
        assert_eq!(tempo_apply_click(&mut s4, 100, -1, 10, 0), None);
        assert_eq!(tempo_apply_click(&mut s4, 1950, 1, 100, 0), Some(2000));
        assert_eq!(tempo_apply_click(&mut s4, 150, -1, 100, 0), Some(100));
        // Sign preserved, cannot cross zero.
        assert_eq!(tempo_apply_click(&mut s4, -750, 1, 10, 0), Some(-760));
        assert_eq!(tempo_apply_click(&mut s4, -110, -1, 10, 0), Some(-100));
        assert_eq!(tempo_apply_click(&mut s4, -100, -1, 10, 0), None);
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }

    #[test]
    fn jog_gesture_property() {
        let mut rng = Lcg(7);
        for case in 0..300 {
            let dur = if case % 5 == 0 {
                None
            } else {
                Some(1_000 + rng.next(300_000))
            };
            let mut g = JogGesture::new();
            let mut now = 1_000u64;
            let mut commits = 0;
            let mut model: Option<u64> = None;
            let gestures = 1 + rng.next(4);
            for _ in 0..gestures {
                let base = rng.next(120_000);
                let clicks = 1 + rng.next(30);
                let mut last_click = now;
                for c in 0..clicks {
                    let gap = if c == 0 { 0 } else { 1 + rng.next(119) };
                    now += gap;
                    // Nothing may commit while gaps stay under the idle time.
                    assert_eq!(g.tick(now), None);
                    let step = (rng.next(2001) as i64 - 1000) * 10;
                    let r = g.click(now, step, base, dur);
                    let m = model.get_or_insert(clamp_seek_target_ms(base, dur));
                    let raw = *m as i128 + step as i128;
                    let want = clamp_seek_target_ms(raw.max(0) as u64, dur);
                    assert_eq!(r.target_ms, want);
                    assert_eq!(r.pinned, want as i128 != raw);
                    *m = want;
                    assert_eq!(g.live_target(), Some(want));
                    if let Some(d) = dur {
                        assert!(want <= d.saturating_sub(plippa_core::SEEK_END_GUARD_MS));
                    }
                    last_click = now;
                }
                // Idle: exactly one commit.
                assert_eq!(g.tick(last_click + JOG_COMMIT_IDLE_MS - 1), None);
                let com = if rng.next(2) == 0 {
                    g.tick(last_click + JOG_COMMIT_IDLE_MS)
                } else {
                    g.flush()
                };
                assert_eq!(
                    com,
                    Some(Commit {
                        target_ms: model.unwrap(),
                        item_delta: 0
                    })
                );
                commits += 1;
                assert_eq!(g.tick(last_click + 10_000), None);
                assert_eq!(g.flush(), None);
                assert!(!g.is_open());
                model = None;
                now = last_click + JOG_COMMIT_IDLE_MS + 5;
            }
            assert_eq!(commits, gestures);
        }
    }

    #[test]
    fn ffrw_led_min_pressed() {
        assert_eq!(ffrw_led_level(false, Some(0), None, 10), None);
        assert_eq!(ffrw_led_level(true, None, None, 0), Some(FFRW_LED_IDLE));
        assert_eq!(
            ffrw_led_level(true, Some(100), None, 5_000),
            Some(FFRW_LED_PRESSED)
        );
        assert_eq!(
            ffrw_led_level(true, Some(100), Some(110), 150),
            Some(FFRW_LED_PRESSED)
        );
        assert_eq!(
            ffrw_led_level(true, Some(100), Some(110), 190),
            Some(FFRW_LED_IDLE)
        );
    }

    /// WARP_21: the FF/RW backlight pulse follows the set speed (colour, direction, rate).
    #[test]
    fn ffrw_pulses_follow_the_set_speed() {
        // Steady at exactly 100% / 0, and FF is quiet for reverse (RW carries that).
        for steady in [0, 1000] {
            assert_eq!(ffrw_pulse_ff(steady), None, "ff {steady}");
        }
        for reverse in [-250, -1000, -2000] {
            assert_eq!(ffrw_pulse_ff(reverse), None, "ff {reverse}");
        }
        assert_eq!(ffrw_pulse_rw(0), None);
        assert_eq!(ffrw_pulse_rw(1000), None);
        assert_eq!(ffrw_pulse_rw(250), None);

        // Above 100%: purple, swinging low -> high purple, faster the faster the speed.
        let p1250 = ffrw_pulse_ff(1250).expect("ff 1.25x pulses");
        assert_eq!((p1250.lo, p1250.hi), (FFRW_LED_IDLE, FFRW_LED_PRESSED));
        let p2000 = ffrw_pulse_ff(2000).unwrap();
        assert!(p2000.period_ms < p1250.period_ms, "faster speed, faster pulse");
        assert!(p1250.period_ms < FFRW_PULSE_BASE_PERIOD_MS);

        // 0 < speed < 100%: blue, and slower the slower the speed.
        let b750 = ffrw_pulse_ff(750).expect("ff 0.75x pulses");
        assert_eq!(b750.hi, FFRW_LED_SLOW_BLUE);
        let b250 = ffrw_pulse_ff(250).unwrap();
        assert!(b250.period_ms > b750.period_ms, "slower speed, slower pulse");
        assert!(b750.period_ms > FFRW_PULSE_BASE_PERIOD_MS);

        // Reverse: pulse at exactly -100% and beyond, purple, faster the faster.
        let r1000 = ffrw_pulse_rw(-1000).expect("-1.0x pulses");
        assert_eq!((r1000.lo, r1000.hi), (FFRW_LED_IDLE, FFRW_LED_PRESSED));
        let r2000 = ffrw_pulse_rw(-2000).unwrap();
        assert!(r2000.period_ms < r1000.period_ms);

        // Period = base / |speed|, clamped at the slow end (reachable tempo minimum).
        assert_eq!(p2000.period_ms, FFRW_PULSE_BASE_PERIOD_MS / 2);
        assert_eq!(ffrw_pulse_ff(100).unwrap().period_ms, FFRW_PULSE_MAX_PERIOD_MS);
    }

    /// WARP_21: the pulse swings a full sine between the two colours over its period.
    #[test]
    fn transport_pulse_is_a_sine_between_lo_and_hi() {
        let pulse = TransportPulse {
            lo: Rgb::new(60, 0, 90),
            hi: Rgb::new(200, 0, 255),
            period_ms: 800,
        };
        assert_eq!(pulse.colour_at(0), pulse.lo);
        assert_eq!(pulse.colour_at(400), pulse.hi, "half a period = the top");
        assert_eq!(pulse.colour_at(800), pulse.lo, "one period = back to the bottom");
        // Monotone rise over the first half period; brightness never leaves lo..hi.
        let lum = |c: Rgb| u32::from(c.r) + u32::from(c.g) + u32::from(c.b);
        let mut prev = lum(pulse.colour_at(0));
        for t in (50..=400).step_by(50) {
            let now = lum(pulse.colour_at(t));
            assert!(now >= prev, "rises at {t} ms");
            prev = now;
        }
        assert!(lum(pulse.lo) < lum(pulse.hi));
    }

    #[test]
    fn desired_rate_overrides_then_status_wins() {
        let mut d = DesiredRate::default();
        let status = 1000;
        let a = ladder_step(d.effective(status, 0), 1).unwrap();
        d.set(a, 0);
        let b = ladder_step(d.effective(status, 100), 1).unwrap();
        d.set(b, 100);
        assert_eq!((a, b), (1250, 1500));
        assert_eq!(d.effective(status, 699), 1500);
        assert_eq!(
            d.effective(status, 700),
            1000,
            "600 ms after the last set the status wins"
        );
        // Confirmed by status: override dropped early.
        d.set(1250, 1000);
        assert_eq!(d.effective(1250, 1010), 1250);
        assert_eq!(d.fresh(1010), None);
    }

    #[test]
    fn settings_json_round_trip_and_garbage() {
        let s = WarpModeSettings {
            speed: Some(WarpMode::PitchCorrect),
            seek: None,
        };
        assert_eq!(warp_settings_from_json(&warp_settings_to_json(&s)), s);
        assert_eq!(
            warp_settings_from_json("not json"),
            WarpModeSettings::default()
        );
    }

    #[test]
    fn queue_coalescing() {
        let mut q = vec![];
        for t in [10, 20, 30] {
            push_coalesced(&mut q, ipc(PlaybackCommand::SeekTo { target_ms: t }));
        }
        assert_eq!(q, vec![ipc(PlaybackCommand::SeekTo { target_ms: 30 })]);
        push_coalesced(&mut q, ipc(PlaybackCommand::SetRate { permille: 1250 }));
        push_coalesced(&mut q, ipc(PlaybackCommand::SetRate { permille: 1500 }));
        push_coalesced(
            &mut q,
            ipc(PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchShift)),
        );
        push_coalesced(
            &mut q,
            ipc(PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchCorrect)),
        );
        push_coalesced(
            &mut q,
            ipc(PlaybackCommand::SetSeekWarpMode(WarpMode::PitchShift)),
        );
        assert_eq!(q.len(), 4);
        assert!(q.contains(&ipc(PlaybackCommand::SetRate { permille: 1500 })));
        assert!(q.contains(&ipc(PlaybackCommand::SetSpeedWarpMode(
            WarpMode::PitchCorrect
        ))));
        // Scan start + stop cancel; stop alone stays; other commands untouched.
        let mut q = vec![ipc(PlaybackCommand::Pause)];
        push_coalesced(
            &mut q,
            ipc(PlaybackCommand::ScanStart {
                dir: ScanDir::Forward,
            }),
        );
        push_coalesced(&mut q, ipc(PlaybackCommand::ScanStop));
        assert_eq!(q, vec![ipc(PlaybackCommand::Pause)]);
        push_coalesced(&mut q, ipc(PlaybackCommand::ScanStop));
        assert_eq!(q.len(), 2);
        push_coalesced(&mut q, ipc(PlaybackCommand::ScanStop));
        assert_eq!(q.len(), 3, "a lone ScanStop is never dropped");
    }

    #[test]
    fn previous_and_next_never_coalesce() {
        // FIX_01C: rapid Prev taps each reach the engine (one track back per tap), so a future
        // coalescing slot must never swallow Previous/Next/PreviousTrack.
        let mut q = vec![];
        push_coalesced(&mut q, ipc(PlaybackCommand::Previous));
        push_coalesced(&mut q, ipc(PlaybackCommand::Previous));
        push_coalesced(&mut q, ipc(PlaybackCommand::Next));
        push_coalesced(&mut q, ipc(PlaybackCommand::PreviousTrack));
        assert_eq!(
            q,
            vec![
                ipc(PlaybackCommand::Previous),
                ipc(PlaybackCommand::Previous),
                ipc(PlaybackCommand::Next),
                ipc(PlaybackCommand::PreviousTrack),
            ]
        );
    }

    #[test]
    fn play_key_immediate_toggles_on_press_and_hold_does_nothing() {
        let mut k = PlayKey::new();
        assert_eq!(k.on_edge(true, 0, false), Some(PlayKeyAction::Toggle));
        assert_eq!(k.on_tick(799), None);
        assert_eq!(k.on_tick(800), None, "pure: no reset");
        assert_eq!(k.on_tick(5_000), None);
        assert_eq!(k.on_edge(false, 5_000, false), None);
        // Bounce: down while down ignored, release without press ignored.
        assert_eq!(k.on_edge(true, 6_000, false), Some(PlayKeyAction::Toggle));
        assert_eq!(k.on_edge(true, 6_010, false), None);
        assert_eq!(k.on_edge(false, 6_050, false), None);
        assert_eq!(k.on_edge(false, 6_060, false), None);
    }

    #[test]
    fn play_key_deferred_toggles_on_release_or_resets_once_at_800() {
        // Short press: toggle only on release.
        let mut k = PlayKey::new();
        assert_eq!(k.on_edge(true, 100, true), None);
        assert_eq!(k.on_tick(500), None);
        assert_eq!(k.on_edge(false, 100 + 799, true), Some(PlayKeyAction::Toggle));
        // Boundary 799 / 800: exactly one reset, at 800; the release afterwards never toggles.
        let mut k = PlayKey::new();
        k.on_edge(true, 0, true);
        assert_eq!(k.on_tick(799), None);
        assert_eq!(k.on_tick(800), Some(PlayKeyAction::Reset));
        assert_eq!(k.on_tick(900), None);
        assert_eq!(k.on_edge(false, 2_000, true), None);
        // Release exactly at 800 without a tick: one reset, no toggle.
        let mut k = PlayKey::new();
        k.on_edge(true, 0, true);
        assert_eq!(k.on_edge(false, 800, true), Some(PlayKeyAction::Reset));
        // The mode is fixed at the press: the state flipping to pure mid-press changes nothing.
        let mut k = PlayKey::new();
        k.on_edge(true, 0, true);
        assert_eq!(k.on_edge(false, 100, false), Some(PlayKeyAction::Toggle));
        let mut k = PlayKey::new();
        assert_eq!(k.on_edge(true, 0, false), Some(PlayKeyAction::Toggle));
        assert_eq!(k.on_edge(false, 100, true), None, "immediate press stays immediate");
    }

    #[test]
    fn jog_gesture_crosses_at_most_one_neighbour_and_clamps_inside_it() {
        let nb = NeighbourCtx { prev_ms: Some(60_000), next_ms: Some(90_000) };
        let dur = Some(100_000);
        let mut g = JogGesture::new();
        // Past 0 into the previous item.
        let c = g.click_with(0, -1_500, 1_000, dur, nb);
        assert_eq!((g.live_item(), c.target_ms, c.pinned), (-1, 59_500, false));
        // Moving on past that item's end guard comes back into the current item.
        let c = g.click_with(1, 1_000, 1_000, dur, nb);
        assert_eq!((g.live_item(), c.target_ms), (0, 750));
        // Far back again: into the previous item, clamped at its start (never a second one).
        let c = g.click_with(2, -200_000, 1_000, dur, nb);
        assert_eq!((g.live_item(), c.target_ms, c.pinned), (-1, 0, true));
        let c = g.click_with(3, -10, 1_000, dur, nb);
        assert_eq!((g.live_item(), c.target_ms, c.pinned), (-1, 0, true));
        let commit = g.flush().unwrap();
        assert_eq!((commit.item_delta, commit.target_ms), (-1, 0));
        // Forward past the guard into the next item, clamped at its own guard and list ends.
        let mut g = JogGesture::new();
        let c = g.click_with(0, 3_000, 98_000, dur, nb);
        assert_eq!((g.live_item(), c.target_ms), (1, 1_250));
        let c = g.click_with(1, 500_000, 98_000, dur, nb);
        assert_eq!((g.live_item(), c.target_ms, c.pinned), (1, 89_750, true));
        assert_eq!(g.flush().unwrap().item_delta, 1);
        // No neighbour: the ordinary clamp, item stays 0.
        let mut g = JogGesture::new();
        let c = g.click_with(0, -5_000, 1_000, dur, NeighbourCtx::NONE);
        assert_eq!((g.live_item(), c.target_ms, c.pinned), (0, 0, true));
        let c = g.click_with(1, 500_000, 1_000, dur, NeighbourCtx { prev_ms: Some(1), next_ms: None });
        assert_eq!((g.live_item(), c.target_ms, c.pinned), (0, 99_750, true));
    }
}
