//! ART_10: the NFC LED ring plays the cover art like a record — pure maths, no hardware.
//!
//! The ring shows the colours of the playing cover sampled on a circle, rotated by the media
//! position: one turn per [`RECORD_REV_MS`] of media time, so Pause freezes it, tempo scales
//! it and FF/RW scans spin it (backwards for RW). The displayed angle chases that target
//! through a speed-limited damper ([`RecordMotion`]), so a seek or a scan faster than the
//! eye can follow is a quick smooth spin rather than a teleport or an aliased flicker.
//!
//! Every function takes injected time and has no IO. The composer
//! ([`crate::led_compose::LedComposer`]) owns the ring; this module only supplies colours.
//! The colour pipeline is the existing one (`Rgb`, `Rgb::blend`, the span's `max_brightness_pct`
//! and `wire_swizzle` in the composer): there is no gamma table here, because the LED layer
//! applies none to any other role, and a second pipeline would make the ring disagree with them.

use crate::art::ArtImage;
use crate::led::Rgb;
use std::sync::Arc;

/// Media time of one full turn of the ring at 100 % tempo (33 1/3 rpm). Tune: shorter = faster.
pub const RECORD_REV_MS: u32 = 1_800;
/// Brightest any channel of the ring may get, in percent of full scale (WS2811 behind a
/// diffuser: full white is harsh). Tune on the device.
pub const LED_MAX_BRIGHTNESS_PCT: u8 = 55;
/// Angular speed cap of the displayed ring in revolutions per second; faster targets (a 16x
/// scan) show as a smooth spin at this speed. Tune: lower = calmer.
pub const LED_MAX_REV_PER_S: f32 = 2.5;
/// Spatial low-pass across neighbouring LEDs (weights, normalised by their sum). Tune: a wider
/// or flatter kernel blurs the picture more.
pub const LED_SMOOTH_KERNEL: [u16; 3] = [1, 2, 1];
/// Colour crossfade when the track or its art changes. Tune: longer = softer.
pub const LED_ART_CROSSFADE_MS: u32 = 400;
/// Radius of the sampling circle through the art, in percent of the half-side. Tune: smaller
/// shows the middle of the cover, larger its edge.
pub const LED_SAMPLE_RADIUS_PCT: u8 = 65;
/// Diameter in art pixels of the disc averaged per LED, so a thin line cannot flicker one LED.
pub const LED_SAMPLE_DISC_PX: u8 = 5;
/// Cadence of ring updates while the angle changes (the LED worker's own frame rate).
pub const LED_RECORD_FRAME_MS: u64 = 33;
/// Angle changes smaller than this (degrees) are not worth a frame.
const ANGLE_EPS_DEG: f32 = 0.01;

/// What the composer needs to draw the ring: the cover and the angle to show it at.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordFrame {
    /// The primary cover Now Playing shows (64 px square-cropped RGB, shared, not copied).
    pub art: Arc<ArtImage>,
    /// Displayed rotation in degrees clockwise, `0..360`.
    pub angle_deg: f32,
}

/// `n` colours sampled around the art, LED 0 at 12 o'clock, clockwise. `art_rgb64` is the
/// square `ArtImage::rgb` (its side is derived from the length); a malformed buffer gives
/// black.
pub fn sample_ring(art_rgb64: &[u8], n: usize) -> Vec<Rgb> {
    let side = ((art_rgb64.len() / 3) as f64).sqrt() as usize;
    if n == 0 || side == 0 || side * side * 3 != art_rgb64.len() {
        return vec![Rgb::new(0, 0, 0); n];
    }
    let half = side as f64 / 2.0;
    let radius = half * f64::from(LED_SAMPLE_RADIUS_PCT) / 100.0;
    let disc_r = f64::from(LED_SAMPLE_DISC_PX.max(1)) / 2.0;
    (0..n)
        .map(|k| {
            let theta = std::f64::consts::TAU * k as f64 / n as f64;
            let cx = half + radius * theta.sin();
            let cy = half - radius * theta.cos();
            average_disc(art_rgb64, side, cx, cy, disc_r)
        })
        .collect()
}

/// Mean colour of the pixels whose centre lies within `r` of `(cx, cy)` (pixel `i` has its
/// centre at `i + 0.5`); the nearest pixel when none does.
fn average_disc(rgb: &[u8], side: usize, cx: f64, cy: f64, r: f64) -> Rgb {
    let lo = |c: f64| ((c - r).floor().max(0.0)) as usize;
    let hi = |c: f64| ((c + r).ceil().max(0.0) as usize).min(side - 1);
    let (mut sum, mut count) = ([0u32; 3], 0u32);
    for y in lo(cy)..=hi(cy) {
        for x in lo(cx)..=hi(cx) {
            let (dx, dy) = (x as f64 + 0.5 - cx, y as f64 + 0.5 - cy);
            if dx * dx + dy * dy <= r * r {
                let i = (y * side + x) * 3;
                for c in 0..3 {
                    sum[c] += u32::from(rgb[i + c]);
                }
                count += 1;
            }
        }
    }
    if count == 0 {
        let x = (cx.floor().max(0.0) as usize).min(side - 1);
        let y = (cy.floor().max(0.0) as usize).min(side - 1);
        let i = (y * side + x) * 3;
        return Rgb::new(rgb[i], rgb[i + 1], rgb[i + 2]);
    }
    let avg = |s: u32| ((s + count / 2) / count) as u8;
    Rgb::new(avg(sum[0]), avg(sum[1]), avg(sum[2]))
}

/// Linear blend of two sample sets by `t` (`0` = `from`, `1` = `to`), for the art crossfade.
/// The result has `to`'s length; a shorter `from` counts as black beyond its end.
pub fn crossfade(from: &[Rgb], to: &[Rgb], t: f32) -> Vec<Rgb> {
    let black = Rgb::new(0, 0, 0);
    to.iter()
        .enumerate()
        .map(|(i, b)| Rgb::blend(from.get(i).copied().unwrap_or(black), *b, t))
        .collect()
}

/// Rotation target in degrees `0..360` for a media position: `position_ms / RECORD_REV_MS` of
/// a turn. Negative-safe (a position before zero wraps the other way).
pub fn target_angle(position_ms: i64) -> f32 {
    let rev = i64::from(RECORD_REV_MS);
    position_ms.rem_euclid(rev) as f32 * 360.0 / rev as f32
}

/// The damper between the media-time target and the angle drawn.
///
/// The target is tracked *unwrapped* (each step adds the shortest signed change of the wrapped
/// target), the displayed angle chases it at no more than [`LED_MAX_REV_PER_S`] and never
/// overshoots. A lag of more than a turn is folded away, which keeps a runaway scan a steady
/// max-speed spin that stops within one turn of the scan ending.
#[derive(Debug, Clone, Default)]
pub struct RecordMotion {
    /// Unwrapped displayed angle, degrees (kept small by [`Self::renormalise`]).
    displayed: f64,
    /// Unwrapped target angle, degrees.
    unwrapped_target: f64,
    last_target: f64,
    last_now: u64,
    started: bool,
}

impl RecordMotion {
    /// Target angle for a media position (see [`target_angle`]).
    pub fn target_angle(position_ms: i64) -> f32 {
        target_angle(position_ms)
    }

    /// Forget the angle: the next [`Self::step`] starts exactly on its target (no spin from a
    /// stale angle after a stop).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Advance to `now_ms` chasing `target_deg` (`0..360`); returns the displayed angle in
    /// `0..360`. The first call lands on the target. A clock that goes backwards counts as no
    /// time passing.
    pub fn step(&mut self, target_deg: f32, now_ms: u64) -> f32 {
        let target = f64::from(target_deg).rem_euclid(360.0);
        if !self.started {
            *self = Self {
                displayed: target,
                unwrapped_target: target,
                last_target: target,
                last_now: now_ms,
                started: true,
            };
            return self.angle();
        }
        let dt_ms = now_ms.saturating_sub(self.last_now);
        self.last_now = now_ms;
        // Shortest signed change of the wrapped target since the last step.
        let mut delta = (target - self.last_target).rem_euclid(360.0);
        if delta > 180.0 {
            delta -= 360.0;
        }
        self.last_target = target;
        self.unwrapped_target += delta;

        let max_step = f64::from(LED_MAX_REV_PER_S) * 360.0 * dt_ms as f64 / 1000.0;
        let error = self.unwrapped_target - self.displayed;
        self.displayed += error.clamp(-max_step, max_step);
        // Whole turns of lag are invisible; folding them keeps a runaway scan bounded.
        let lag = self.unwrapped_target - self.displayed;
        self.displayed += 360.0 * (lag / 360.0).trunc();
        self.renormalise();
        self.angle()
    }

    /// Displayed angle in `0..360`.
    pub fn angle(&self) -> f32 {
        self.displayed.rem_euclid(360.0) as f32
    }

    fn renormalise(&mut self) {
        let shift = 360.0 * (self.displayed / 360.0).floor();
        self.displayed -= shift;
        self.unwrapped_target -= shift;
    }
}

/// The ring for `samples` rotated clockwise by `angle_deg`: fractional rotation (linear
/// interpolation between neighbouring LEDs, never nearest-LED snapping), then the
/// [`LED_SMOOTH_KERNEL`] low-pass around the ring, then the [`LED_MAX_BRIGHTNESS_PCT`] cap.
/// Output length is `n`; `samples` of another length are spread over the ring.
pub fn render_ring(samples: &[Rgb], angle_deg: f32, n: usize) -> Vec<Rgb> {
    if n == 0 {
        return Vec::new();
    }
    let m = samples.len();
    if m == 0 {
        return vec![Rgb::new(0, 0, 0); n];
    }
    let shift = f64::from(angle_deg) / 360.0 * n as f64;
    let rotated: Vec<[f32; 3]> = (0..n)
        .map(|k| {
            // The picture turns clockwise: LED k shows what sat `shift` LEDs behind it.
            let pos = ((k as f64 - shift).rem_euclid(n as f64)) * m as f64 / n as f64;
            let i0 = pos.floor() as usize % m;
            let i1 = (i0 + 1) % m;
            let f = (pos - pos.floor()) as f32;
            let (a, b) = (samples[i0], samples[i1]);
            let mix = |x: u8, y: u8| f32::from(x) + (f32::from(y) - f32::from(x)) * f;
            [mix(a.r, b.r), mix(a.g, b.g), mix(a.b, b.b)]
        })
        .collect();

    let kernel = LED_SMOOTH_KERNEL.map(f32::from);
    let norm: f32 = kernel.iter().sum();
    let cap = f32::from(LED_MAX_BRIGHTNESS_PCT) / 100.0;
    (0..n)
        .map(|k| {
            let (prev, next) = ((k + n - 1) % n, (k + 1) % n);
            let ch = |c: usize| {
                let v = (rotated[prev][c] * kernel[0] + rotated[k][c] * kernel[1]
                    + rotated[next][c] * kernel[2])
                    / norm;
                (v * cap).round().clamp(0.0, 255.0) as u8
            };
            Rgb::new(ch(0), ch(1), ch(2))
        })
        .collect()
}

/// What the loop should do with the ring this pass.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordUpdate {
    /// Nothing changed (frozen, or between frames).
    Keep,
    /// Draw this frame.
    Show(RecordFrame),
    /// Back to the ring's normal idle state.
    Clear,
}

/// What the app says the ring should show right now.
#[derive(Debug, Clone)]
pub struct RecordInput {
    pub art: Arc<ArtImage>,
    /// The waveform's dead-reckoned playhead (tempo, scan and warp already included).
    pub position_ms: i64,
}

/// The coordinator's side of the ring: runs the motion every loop pass (also under a tap
/// animation, so the hand-off back to the record has no jump) and emits an update only when
/// the picture changed — so a paused session sends nothing at all.
#[derive(Debug, Default)]
pub struct RecordFeed {
    motion: RecordMotion,
    shown: Option<(Arc<ArtImage>, f32)>,
    last_push_ms: u64,
    /// The last update was refused by a full LED queue: send the picture (or the clear) again.
    resend: bool,
}

impl RecordFeed {
    pub fn new() -> Self {
        Self::default()
    }

    /// `input` is `None` whenever the ring is not the record's (setting off, stopped, no art,
    /// shutting down).
    pub fn update(&mut self, input: Option<RecordInput>, now_ms: u64) -> RecordUpdate {
        let resend = std::mem::take(&mut self.resend);
        let Some(input) = input else {
            self.motion.reset();
            return if self.shown.take().is_some() || resend {
                RecordUpdate::Clear
            } else {
                RecordUpdate::Keep
            };
        };
        let angle = self
            .motion
            .step(RecordMotion::target_angle(input.position_ms), now_ms);
        let art_changed = self
            .shown
            .as_ref()
            .is_none_or(|(a, _)| !Arc::ptr_eq(a, &input.art));
        let moved = self.shown.as_ref().is_some_and(|(_, a)| {
            let d = (angle - a).abs();
            d.min(360.0 - d) >= ANGLE_EPS_DEG
        });
        let due = now_ms.saturating_sub(self.last_push_ms) >= LED_RECORD_FRAME_MS;
        if art_changed || resend || (moved && due) {
            self.last_push_ms = now_ms;
            self.shown = Some((Arc::clone(&input.art), angle));
            return RecordUpdate::Show(RecordFrame {
                art: input.art,
                angle_deg: angle,
            });
        }
        RecordUpdate::Keep
    }

    /// The LED queue refused the last [`RecordUpdate`]: the next [`Self::update`] sends the
    /// current picture, or the clear, again instead of assuming the ring got it.
    pub fn not_delivered(&mut self) {
        self.resend = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDE: usize = 64;

    /// Left half `l`, right half `r`.
    fn split_lr(l: [u8; 3], r: [u8; 3]) -> Vec<u8> {
        (0..SIDE * SIDE)
            .flat_map(|i| if i % SIDE < SIDE / 2 { l } else { r })
            .collect()
    }

    fn solid(c: [u8; 3]) -> Vec<u8> {
        split_lr(c, c)
    }

    const RED: [u8; 3] = [255, 0, 0];
    const BLUE: [u8; 3] = [0, 0, 255];

    #[test]
    fn two_colour_split_gives_two_contiguous_arcs_with_zero_at_twelve_clockwise() {
        // Right half blue (3 o'clock side), left half red (9 o'clock side).
        let ring = sample_ring(&split_lr(RED, BLUE), 24);
        assert_eq!(ring.len(), 24);
        // LED 6 is 3 o'clock, LED 18 is 9 o'clock: orientation is fixed by the test.
        assert_eq!(ring[6], Rgb::new(0, 0, 255));
        assert_eq!(ring[18], Rgb::new(255, 0, 0));
        // Clockwise from 12: LEDs 1..=11 sit on the right (blue), 13..=23 on the left (red);
        // 0 and 12 straddle the seam.
        assert!((1..12).all(|k| ring[k] == Rgb::new(0, 0, 255)), "{ring:?}");
        assert!((13..24).all(|k| ring[k] == Rgb::new(255, 0, 0)), "{ring:?}");
        for k in [0, 12] {
            assert!(ring[k].r > 0 && ring[k].b > 0, "seam LED {k} is a mix");
        }
    }

    #[test]
    fn an_odd_ring_length_and_tiny_rings_work() {
        let ring = sample_ring(&solid([10, 20, 30]), 7);
        assert_eq!(ring.len(), 7);
        assert!(ring.iter().all(|p| *p == Rgb::new(10, 20, 30)));
        assert_eq!(sample_ring(&solid([1, 2, 3]), 1).len(), 1);
        assert!(sample_ring(&solid([1, 2, 3]), 0).is_empty());
        // A malformed buffer is black, not a panic.
        assert!(sample_ring(&[1, 2, 3, 4], 5).iter().all(|p| *p == Rgb::new(0, 0, 0)));
    }

    #[test]
    fn a_thin_line_is_averaged_not_one_hot_led() {
        let mut img = solid([0, 0, 0]);
        // One white pixel column exactly under LED 6's sample point.
        let x = SIDE / 2 + (SIDE as f64 / 2.0 * 0.65) as usize;
        for y in 0..SIDE {
            let i = (y * SIDE + x) * 3;
            img[i..i + 3].copy_from_slice(&[255, 255, 255]);
        }
        let ring = sample_ring(&img, 24);
        assert!(ring[6].r > 0 && ring[6].r < 255, "averaged: {:?}", ring[6]);
    }

    fn cap() -> u8 {
        (255.0 * f32::from(LED_MAX_BRIGHTNESS_PCT) / 100.0).round() as u8
    }

    #[test]
    fn render_at_angle_zero_is_the_smoothed_capped_samples() {
        let solid_ring = vec![Rgb::new(200, 100, 0); 24];
        let out = render_ring(&solid_ring, 0.0, 24);
        assert_eq!(out.len(), 24);
        let expect = Rgb::new(110, 55, 0); // 200 * 0.55, 100 * 0.55
        assert!(out.iter().all(|p| *p == expect), "{out:?}");
        // A single lit LED spreads 1:2:1 over its neighbours.
        let mut one = vec![Rgb::new(0, 0, 0); 8];
        one[3] = Rgb::new(200, 0, 0);
        let out = render_ring(&one, 0.0, 8);
        assert_eq!((out[2].r, out[3].r, out[4].r), (28, 55, 28));
        assert_eq!(out[0].r, 0);
    }

    #[test]
    fn rotating_by_one_led_moves_the_picture_clockwise_and_half_a_led_averages() {
        let mut one = vec![Rgb::new(0, 0, 0); 8];
        one[3] = Rgb::new(200, 0, 0);
        let step = 360.0 / 8.0;
        let base = render_ring(&one, 0.0, 8);
        let turned = render_ring(&one, step, 8);
        assert_eq!(turned[4], base[3], "one LED clockwise");
        assert_eq!(turned[5], base[4]);
        // Half an LED: the unsmoothed pixel is the average of two neighbours, so the peak is
        // shared between LED 3 and LED 4 equally.
        let half = render_ring(&one, step / 2.0, 8);
        assert_eq!(half[3], half[4]);
        assert!(half[3].r < base[3].r && half[3].r > 0);
    }

    #[test]
    fn brightness_never_exceeds_the_cap_and_length_is_n() {
        let white = vec![Rgb::new(255, 255, 255); 24];
        for angle in [0.0, 7.3, 90.0, 359.9] {
            let out = render_ring(&white, angle, 24);
            assert_eq!(out.len(), 24);
            assert!(out.iter().all(|p| p.r <= cap() && p.g <= cap() && p.b <= cap()));
        }
        assert_eq!(render_ring(&white, 10.0, 7).len(), 7);
        assert!(render_ring(&white, 10.0, 0).is_empty());
        assert_eq!(render_ring(&[], 10.0, 5).len(), 5);
    }

    #[test]
    fn crossfade_blends_between_the_sets() {
        let a = vec![Rgb::new(0, 0, 0); 3];
        let b = vec![Rgb::new(200, 100, 50); 3];
        assert_eq!(crossfade(&a, &b, 0.0), a);
        assert_eq!(crossfade(&a, &b, 1.0), b);
        assert_eq!(crossfade(&a, &b, 0.5)[1], Rgb::new(100, 50, 25));
        assert_eq!(crossfade(&[], &b, 1.0), b);
    }

    #[test]
    fn target_angle_is_media_time_and_negative_safe() {
        assert_eq!(target_angle(0), 0.0);
        assert_eq!(target_angle(i64::from(RECORD_REV_MS) / 2), 180.0);
        assert_eq!(target_angle(i64::from(RECORD_REV_MS)), 0.0);
        assert_eq!(target_angle(-i64::from(RECORD_REV_MS) / 4), 270.0);
    }

    /// Run the motion at `dt` ms for `steps`, feeding the target of `pos(t)`.
    fn run(m: &mut RecordMotion, t0: u64, steps: u64, dt: u64, pos: impl Fn(u64) -> i64) -> Vec<f32> {
        (1..=steps)
            .map(|i| {
                let now = t0 + i * dt;
                m.step(RecordMotion::target_angle(pos(now)), now)
            })
            .collect()
    }

    /// Signed shortest difference b - a in degrees.
    fn diff(a: f32, b: f32) -> f32 {
        let d = (b - a).rem_euclid(360.0);
        if d > 180.0 {
            d - 360.0
        } else {
            d
        }
    }

    /// Total signed angle travelled (unwrapped) by a sequence of displayed angles.
    fn travelled(start: f32, angles: &[f32]) -> f32 {
        let mut prev = start;
        angles.iter().map(|a| {
            let d = diff(prev, *a);
            prev = *a;
            d
        }).sum()
    }

    #[test]
    fn one_media_rev_is_one_turn_and_double_tempo_is_twice_as_fast() {
        let rev = u64::from(RECORD_REV_MS);
        for (rate, expect) in [(1i64, 360.0f32), (2, 720.0)] {
            // Both are below the speed cap, so the damper does not hold them back.
            let mut m = RecordMotion::default();
            m.step(0.0, 0);
            let angles = run(&mut m, 0, rev / 20, 20, |t| t as i64 * rate);
            let got = travelled(0.0, &angles);
            assert!((got - expect).abs() < 0.5, "rate {rate}: {got}");
        }
    }

    #[test]
    fn reverse_playback_rotates_backwards() {
        let mut m = RecordMotion::default();
        m.step(RecordMotion::target_angle(100_000), 0);
        let angles = run(&mut m, 0, 30, 20, |t| 100_000 - t as i64);
        let got = travelled(RecordMotion::target_angle(100_000), &angles);
        assert!(got < -50.0, "{got}");
    }

    #[test]
    fn paused_converges_and_stays() {
        let mut m = RecordMotion::default();
        m.step(10.0, 0);
        // A target far from the displayed angle (a seek), then constant.
        let a = run(&mut m, 0, 40, 33, |_| 700_000);
        let last = *a.last().unwrap();
        let want = RecordMotion::target_angle(700_000);
        assert!(diff(last, want).abs() < 1e-3, "{last} vs {want}");
        let b = run(&mut m, 40 * 33, 60, 33, |_| 700_000);
        assert!(b.iter().all(|x| (x - last).abs() < 1e-3), "must not wobble");
    }

    #[test]
    fn a_ten_minute_seek_is_speed_limited_and_does_not_overshoot() {
        let mut m = RecordMotion::default();
        let p0 = 5_000i64;
        m.step(RecordMotion::target_angle(p0), 0);
        let from = m.angle();
        let seek = p0 + 600_000;
        let want = RecordMotion::target_angle(seek);
        let dt = 33u64;
        let max_step = LED_MAX_REV_PER_S * 360.0 * dt as f32 / 1000.0;
        let mut prev = from;
        let initial_gap = diff(from, want);
        let mut signed_progress = 0.0f32;
        for i in 1..=60u64 {
            let a = m.step(want, i * dt);
            let d = diff(prev, a);
            assert!(d.abs() <= max_step + 1e-3, "step {i}: {d} > {max_step}");
            signed_progress += d;
            // Never past the destination in the direction travelled.
            assert!(
                signed_progress.abs() <= initial_gap.abs() + 1e-3,
                "overshoot at step {i}"
            );
            prev = a;
        }
        assert!(diff(prev, want).abs() < 1e-3, "converged");
    }

    #[test]
    fn a_runaway_scan_is_a_steady_max_speed_spin_that_stops_soon_after() {
        // 16x forward scan: 3200 deg/s of target, faster than the cap.
        let mut m = RecordMotion::default();
        m.step(0.0, 0);
        let dt = 33u64;
        let max_step = LED_MAX_REV_PER_S * 360.0 * dt as f32 / 1000.0;
        let mut prev = 0.0f32;
        for i in 1..=90u64 {
            let a = m.step(RecordMotion::target_angle((i * dt * 16) as i64), i * dt);
            let d = diff(prev, a);
            assert!(d > 0.0 && d <= max_step + 1e-3, "step {i}: {d}");
            if i > 10 {
                assert!((d - max_step).abs() < 1e-2, "steady max-speed spin at {i}: {d}");
            }
            prev = a;
        }
        // The scan ends: the position holds. The ring converges within one more turn.
        let hold = (90 * dt * 16) as i64;
        let want = RecordMotion::target_angle(hold);
        let mut settled_at = None;
        for i in 91..=200u64 {
            let a = m.step(want, i * dt);
            let d = diff(prev, a);
            assert!(d >= 0.0 && d <= max_step + 1e-3, "no reversal, no overshoot");
            prev = a;
            if settled_at.is_none() && diff(a, want).abs() < 1e-3 {
                settled_at = Some(i);
            }
        }
        let at = settled_at.expect("converges");
        assert!((at - 90) as f32 * dt as f32 <= 1100.0, "settled after {at}");
    }

    #[test]
    fn wrapping_at_360_is_continuous() {
        let mut m = RecordMotion::default();
        m.step(350.0, 0);
        let a = m.step(5.0, 33);
        assert!(diff(350.0, a) > 0.0, "carried forward across 0: {a}");
        assert!((0.0..360.0).contains(&a));
        // Backwards across 0.
        let mut m = RecordMotion::default();
        m.step(5.0, 0);
        let a = m.step(355.0, 33);
        assert!(diff(5.0, a) < 0.0 && diff(5.0, a) >= -10.0 - 1e-3, "{a}");
    }

    #[test]
    fn a_clock_that_goes_backwards_does_not_move_the_ring() {
        let mut m = RecordMotion::default();
        m.step(10.0, 1_000);
        let a = m.step(300.0, 900);
        assert!((a - 10.0).abs() < 1e-3);
    }

    fn art(rgb: [u8; 3]) -> Arc<ArtImage> {
        Arc::new(ArtImage {
            side: 1,
            rgb565: vec![0],
            rgb_side: SIDE as u16,
            rgb: solid(rgb),
        })
    }

    fn input(a: &Arc<ArtImage>, position_ms: i64) -> Option<RecordInput> {
        Some(RecordInput {
            art: Arc::clone(a),
            position_ms,
        })
    }

    #[test]
    fn feed_sends_nothing_while_frozen_and_clears_once() {
        let a = art([9, 9, 9]);
        let mut f = RecordFeed::new();
        // First sight: show.
        assert!(matches!(f.update(input(&a, 1_000), 0), RecordUpdate::Show(_)));
        // Paused: the position does not move => zero updates over many passes.
        for i in 1..200u64 {
            assert_eq!(f.update(input(&a, 1_000), i * 10), RecordUpdate::Keep);
        }
        // Stopped / setting off: one Clear, then silence.
        assert_eq!(f.update(None, 3_000), RecordUpdate::Clear);
        assert_eq!(f.update(None, 3_010), RecordUpdate::Keep);
    }

    #[test]
    fn feed_runs_at_the_led_frame_rate_while_the_angle_changes() {
        let a = art([9, 9, 9]);
        let mut f = RecordFeed::new();
        let mut shown = 0;
        for i in 0..100u64 {
            let now = i * 5; // loop passes every 5 ms
            if matches!(f.update(input(&a, now as i64), now), RecordUpdate::Show(_)) {
                shown += 1;
            }
        }
        // 500 ms at one frame per LED_RECORD_FRAME_MS.
        assert!((13..=16).contains(&shown), "{shown}");
    }

    #[test]
    fn feed_shows_a_new_cover_at_once_and_restarts_on_the_target_after_a_stop() {
        let (a, b) = (art([1, 1, 1]), art([2, 2, 2]));
        let mut f = RecordFeed::new();
        f.update(input(&a, 0), 0);
        let RecordUpdate::Show(fr) = f.update(input(&b, 0), 1) else {
            panic!("new art must show immediately");
        };
        assert!(Arc::ptr_eq(&fr.art, &b));
        f.update(None, 10);
        // After a stop the ring starts on the target of the new position, no spin.
        let RecordUpdate::Show(fr) = f.update(input(&a, 450), 20) else {
            panic!("show");
        };
        assert!((fr.angle_deg - target_angle(450)).abs() < 1e-3);
    }
}
