//! Role-addressed LED frame composer (NB_13) — pure, no hardware.
//!
//! Resolves [`LedRole`]s through the board profile to spans on WS2811 streams and
//! composes the steady layer (power, transport button colours, role colours) plus
//! the NFC tap animation into per-stream frames. The hardware backend
//! (`hw::leds`) only copies [`LedComposer::frames`] to the strips; mock/desktop
//! tests assert on the same frames. Indices, GPIOs and stream names come only from
//! `board.toml`.

use crate::led::{PlayButtonLed, Rgb, TransportPulse};
use crate::record_leds::{crossfade, render_ring, sample_ring, RecordFrame, LED_ART_CROSSFADE_MS};
use plippa_board::{BoardProfile, LedRole, LedSpan, LedStream};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WHOOSH_STEP_MS: u64 = 33;
const PULSE_MS: u64 = 180;
const FADE_MS: u64 = 2_000;
const TRAIL_LEN: usize = 5;
const TRAIL_WEIGHTS: [f32; TRAIL_LEN] = [1.0, 0.55, 0.28, 0.12, 0.04];
const PULSE_END_BRIGHTNESS: f32 = 0.55;
/// Duration of a button-press flash (NB_14; design range 100-150 ms).
pub const FLASH_MS: u64 = 120;

pub const OFF: Rgb = Rgb::new(0, 0, 0);
pub const BLUE: Rgb = Rgb::new(0, 0, 255);
/// Test-only: the amber the play button used to show in standby. Kept so the colour-distinctness
/// tests keep pinning "nothing else drifts into that amber" after the switch to
/// [`PLAY_STANDBY`].
#[cfg(test)]
pub const ORANGE: Rgb = Rgb::new(255, 96, 0);
/// Play button while paused / stopped with an item loaded: purple, the same family as the
/// FF/RW standby glow (so "waiting to play again" is never read as the power-warning orange).
pub const PLAY_STANDBY: Rgb = Rgb::new(120, 0, 200);
/// Cyan-white whoosh / pulse colour.
pub const WHOOSH: Rgb = Rgb::new(80, 200, 255);

/// A role's resolved location: stream index (into [`LedLayout::streams`]), start, count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSpan {
    pub stream: usize,
    pub start: usize,
    pub count: usize,
    pub max_pct: Option<u8>,
    /// Wire colour order of this span if the profile declares one (`None` = the driver's
    /// strip order, GRB). See [`wire_swizzle`].
    pub order: Option<[u8; 3]>,
}

/// Roles → spans for one board profile.
#[derive(Debug, Clone, Default)]
pub struct LedLayout {
    streams: Vec<(String, LedStream)>,
    spans: BTreeMap<LedRole, ResolvedSpan>,
}

impl LedLayout {
    /// Boards without a `[leds]` section give an empty layout (no LED features).
    pub fn from_profile(profile: &BoardProfile) -> Self {
        let streams: Vec<(String, LedStream)> = profile
            .led_streams
            .iter()
            .map(|(n, s)| (n.clone(), s.clone()))
            .collect();
        let spans = profile
            .leds
            .iter()
            .filter_map(|(role, span)| resolve_span(&streams, span).map(|r| (*role, r)))
            .collect();
        Self { streams, spans }
    }

    /// Streams in stable (name) order; index = [`ResolvedSpan::stream`].
    pub fn streams(&self) -> &[(String, LedStream)] {
        &self.streams
    }

    pub fn span(&self, role: LedRole) -> Option<ResolvedSpan> {
        self.spans.get(&role).copied()
    }

    pub fn has(&self, role: LedRole) -> bool {
        self.spans.contains_key(&role)
    }

    pub fn roles(&self) -> impl Iterator<Item = LedRole> + '_ {
        self.spans.keys().copied()
    }
}

fn resolve_span(streams: &[(String, LedStream)], span: &LedSpan) -> Option<ResolvedSpan> {
    let stream = streams.iter().position(|(n, _)| *n == span.stream)?;
    Some(ResolvedSpan {
        stream,
        start: span.start as usize,
        count: span.count as usize,
        max_pct: span.max_brightness_pct,
        order: span.color_order.as_deref().and_then(parse_order),
    })
}

enum NfcTapAnim {
    Whoosh { started: Instant },
    Pulse { started: Instant },
    Fade { started: Instant },
}

/// The cover on the NFC ring (ART_10): the sampled colours, the angle to draw them at and a
/// possible crossfade from the previous picture.
struct RecordRing {
    art: Arc<crate::art::ArtImage>,
    samples: Vec<Rgb>,
    angle_deg: f32,
    fade: Option<RecordFade>,
}

struct RecordFade {
    from: Vec<Rgb>,
    started: Instant,
}

impl RecordRing {
    /// Progress `0..=1` of the crossfade at `now` (1 when there is none).
    fn fade_t(&self, now: Instant) -> f32 {
        match &self.fade {
            Some(f) => {
                let ms = now.saturating_duration_since(f.started).as_millis() as f32;
                (ms / LED_ART_CROSSFADE_MS.max(1) as f32).clamp(0.0, 1.0)
            }
            None => 1.0,
        }
    }

    /// The colours to rotate at `now`: the new samples, blended in from the old ones.
    fn current_samples(&self, now: Instant) -> Vec<Rgb> {
        match &self.fade {
            Some(f) => crossfade(&f.from, &self.samples, self.fade_t(now)),
            None => self.samples.clone(),
        }
    }
}

/// Steady + animated LED state addressed by role.
pub struct LedComposer {
    layout: LedLayout,
    frames: Vec<Vec<Rgb>>,
    play_btn: PlayButtonLed,
    /// Extra steady per-role colours (NB_14 availability, etc.).
    steady: BTreeMap<LedRole, Rgb>,
    /// Animated per-role pulses (WARP_21 FF/RW), painted over `steady` each frame.
    pulses: BTreeMap<LedRole, TransportPulse>,
    nfc_anim: Option<NfcTapAnim>,
    /// Timed per-role overrides (NB_14 press flash): colour + start. Painted over `steady`.
    flashes: BTreeMap<LedRole, (Rgb, Instant)>,
    /// ART_10: the cover on the NFC ring. Below the tap animation and flashes, above the
    /// steady / pulse colour of the ring.
    record: Option<RecordRing>,
    /// Origin of the composer's millisecond clock, so pulses can be phased from `Instant`s.
    epoch: Instant,
}

impl LedComposer {
    pub fn new(layout: LedLayout) -> Self {
        let frames = layout
            .streams()
            .iter()
            .map(|(_, s)| vec![OFF; s.count as usize])
            .collect();
        Self {
            layout,
            frames,
            play_btn: PlayButtonLed::Off,
            steady: BTreeMap::new(),
            pulses: BTreeMap::new(),
            nfc_anim: None,
            flashes: BTreeMap::new(),
            record: None,
            epoch: Instant::now(),
        }
    }

    pub fn layout(&self) -> &LedLayout {
        &self.layout
    }

    /// Composed frame per stream (same order as [`LedLayout::streams`]).
    pub fn frames(&self) -> &[Vec<Rgb>] {
        &self.frames
    }

    /// Frames as handed to the strip driver: like [`Self::frames`] but with each span's
    /// declared `color_order` compensated for the driver's fixed GRB strip type (NB_VERIFY:
    /// the profile key used to be validated but ignored). Spans without `color_order`
    /// (all of plippa-D-poc2) are unchanged.
    pub fn driver_frames(&self) -> Vec<Vec<Rgb>> {
        let mut out = self.frames.clone();
        for span in self.layout.spans.values() {
            if let Some(order) = span.order {
                for px in &mut out[span.stream][span.start..span.start + span.count] {
                    *px = wire_swizzle(order, *px);
                }
            }
        }
        out
    }

    /// The LED(s) of `role` in the last composed frame (empty if role absent).
    pub fn role_pixels(&self, role: LedRole) -> Vec<Rgb> {
        match self.layout.span(role) {
            Some(s) => self.frames[s.stream][s.start..s.start + s.count].to_vec(),
            None => Vec::new(),
        }
    }

    pub fn set_play_button(&mut self, state: PlayButtonLed) {
        self.play_btn = state;
    }

    pub fn play_button(&self) -> PlayButtonLed {
        self.play_btn
    }

    /// Steady colour for a whole role span; `None` = off. Absent role: no-op.
    pub fn set_role_colour(&mut self, role: LedRole, colour: Option<Rgb>) {
        if !self.layout.has(role) {
            return;
        }
        match colour {
            Some(c) => {
                self.steady.insert(role, c);
            }
            None => {
                self.steady.remove(&role);
            }
        }
    }

    /// Start the NFC whoosh → pulse → fade. No-op when the board has no ring.
    pub fn on_tap_feedback(&mut self, now: Instant) {
        if self.layout.has(LedRole::NfcRing) {
            self.nfc_anim = Some(NfcTapAnim::Whoosh { started: now });
        }
    }

    /// Animated pulse for a whole role span (`None` = no pulse); painted over the steady
    /// colour on every [`Self::compose`] (WARP_21). Absent role: no-op.
    pub fn set_role_pulse(&mut self, role: LedRole, pulse: Option<TransportPulse>) {
        if !self.layout.has(role) {
            return;
        }
        match pulse {
            Some(p) => {
                self.pulses.insert(role, p);
            }
            None => {
                self.pulses.remove(&role);
            }
        }
    }

    /// Flash `role` in `colour` for [`FLASH_MS`] from `now` (overrides its steady colour,
    /// then the steady colour returns). Absent role: no-op. A new flash restarts the timer.
    pub fn flash_role(&mut self, role: LedRole, colour: Rgb, now: Instant) {
        if self.layout.has(role) {
            self.flashes.insert(role, (colour, now));
        }
    }

    /// ART_10: show `frame` on the NFC ring (`None` = back to the normal ring). A new cover
    /// crossfades from what the ring showed ([`LED_ART_CROSSFADE_MS`]); the same cover only
    /// turns. No-op without a ring.
    pub fn set_record_art(&mut self, frame: Option<RecordFrame>, now: Instant) {
        let n = self.ring_len();
        let Some(frame) = frame.filter(|_| n > 0) else {
            self.record = None;
            return;
        };
        if let Some(r) = self.record.as_mut() {
            if Arc::ptr_eq(&r.art, &frame.art) {
                r.angle_deg = frame.angle_deg;
                return;
            }
        }
        // The old picture (mid-fade included) is what the new one fades in from; the very
        // first picture fades in from dark.
        let from = self
            .record
            .as_ref()
            .map_or_else(|| vec![OFF; n], |r| r.current_samples(now));
        self.record = Some(RecordRing {
            samples: sample_ring(&frame.art.rgb, n),
            art: frame.art,
            angle_deg: frame.angle_deg,
            fade: Some(RecordFade { from, started: now }),
        });
    }

    /// True while something on the strips moves by itself: a tap animation, a flash or a
    /// cover crossfade. A record ring that only turns when told to (Pause, a stopped target)
    /// is not animating: nothing pushes frames then.
    pub fn animating(&self) -> bool {
        self.nfc_anim.is_some()
            || !self.flashes.is_empty()
            || self.record.as_ref().is_some_and(|r| r.fade.is_some())
    }

    /// Reset steady + animation state (shutdown).
    pub fn clear_all(&mut self) {
        self.play_btn = PlayButtonLed::Off;
        self.steady.clear();
        self.pulses.clear();
        self.nfc_anim = None;
        self.flashes.clear();
        self.record = None;
        for f in &mut self.frames {
            f.fill(OFF);
        }
    }

    /// Advance Whoosh → Pulse → Fade → idle. Returns true if the phase changed.
    pub fn advance(&mut self, now: Instant) -> bool {
        let flashes_before = self.flashes.len();
        self.flashes
            .retain(|_, (_, at)| now.duration_since(*at) < Duration::from_millis(FLASH_MS));
        let mut flash_ended = self.flashes.len() != flashes_before;
        if let Some(r) = self.record.as_mut() {
            if r.fade.is_some() {
                // Every frame of a crossfade differs from the last; the end drops the state.
                flash_ended = true;
                if r.fade_t(now) >= 1.0 {
                    r.fade = None;
                }
            }
        }
        let ring_len = self.ring_len() as u64;
        let Some(anim) = self.nfc_anim.as_ref() else {
            return flash_ended;
        };
        flash_ended
            | match anim {
                NfcTapAnim::Whoosh { started } => {
                    let step = now.duration_since(*started).as_millis() as u64 / WHOOSH_STEP_MS;
                    if step >= ring_len {
                        self.nfc_anim = Some(NfcTapAnim::Pulse { started: now });
                        true
                    } else {
                        false
                    }
                }
                NfcTapAnim::Pulse { started } => {
                    if now.duration_since(*started) >= Duration::from_millis(PULSE_MS) {
                        self.nfc_anim = Some(NfcTapAnim::Fade { started: now });
                        true
                    } else {
                        false
                    }
                }
                NfcTapAnim::Fade { started } => {
                    if now.duration_since(*started) >= Duration::from_millis(FADE_MS) {
                        self.nfc_anim = None;
                        true
                    } else {
                        false
                    }
                }
            }
    }

    fn ring_len(&self) -> usize {
        self.layout.span(LedRole::NfcRing).map_or(0, |s| s.count)
    }

    /// Rebuild all frames from the steady layer + animation at `now`.
    pub fn compose(&mut self, now: Instant) {
        for f in &mut self.frames {
            f.fill(OFF);
        }
        self.paint(LedRole::PowerLed, BLUE);
        let play = match self.play_btn {
            PlayButtonLed::Off => OFF,
            PlayButtonLed::Standby => PLAY_STANDBY,
            PlayButtonLed::Playing => BLUE,
        };
        self.paint(LedRole::PlayButton, play);
        let steady: Vec<(LedRole, Rgb)> = self.steady.iter().map(|(r, c)| (*r, *c)).collect();
        for (role, colour) in steady {
            self.paint(role, colour);
        }
        // Animated pulses (WARP_21) sit over the steady layer; the phase comes from the
        // composer's own monotonic ms clock so consecutive frames advance smoothly.
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        let pulses: Vec<(LedRole, TransportPulse)> =
            self.pulses.iter().map(|(r, p)| (*r, *p)).collect();
        for (role, pulse) in pulses {
            self.paint(role, pulse.colour_at(now_ms));
        }
        self.paint_record_ring(now);
        let flashes: Vec<(LedRole, Rgb)> = self
            .flashes
            .iter()
            .filter(|(_, (_, at))| now.duration_since(*at) < Duration::from_millis(FLASH_MS))
            .map(|(r, (c, _))| (*r, *c))
            .collect();
        for (role, colour) in flashes {
            self.paint(role, colour);
        }
        self.paint_nfc_ring(now);
    }

    /// The cover on the ring, unless a tap animation owns it. The animation paints after this,
    /// so it simply covers the record; the motion underneath keeps its angle (the loop keeps
    /// feeding it), which makes the hand-back jump-free.
    fn paint_record_ring(&mut self, now: Instant) {
        let Some(span) = self.layout.span(LedRole::NfcRing) else {
            return;
        };
        let Some(record) = self.record.as_ref() else {
            return;
        };
        if self.nfc_anim.is_some() {
            return;
        }
        let ring = render_ring(&record.current_samples(now), record.angle_deg, span.count);
        for (i, colour) in ring.into_iter().enumerate() {
            self.put(span, i, colour);
        }
    }

    fn paint_nfc_ring(&mut self, now: Instant) {
        let Some(span) = self.layout.span(LedRole::NfcRing) else {
            return;
        };
        let len = span.count;
        let Some(anim) = self.nfc_anim.as_ref() else {
            return;
        };
        match anim {
            NfcTapAnim::Whoosh { started } => {
                let step =
                    (now.duration_since(*started).as_millis() as u64 / WHOOSH_STEP_MS) as usize;
                let head = step.min(len.saturating_sub(1));
                // Soft entry: trail only as long as LEDs already swept.
                let trail_avail = (head + 1).min(TRAIL_LEN);
                for (t, weight) in TRAIL_WEIGHTS.iter().enumerate().take(trail_avail) {
                    let idx = (head + len - t) % len;
                    self.put(span, idx, scale_rgb(WHOOSH, *weight));
                }
            }
            NfcTapAnim::Pulse { started } => {
                let t = (now.duration_since(*started).as_secs_f32() / (PULSE_MS as f32 / 1000.0))
                    .clamp(0.0, 1.0);
                let brightness = 1.0 - ease_out_cubic(t) * (1.0 - PULSE_END_BRIGHTNESS);
                self.paint(LedRole::NfcRing, scale_rgb(WHOOSH, brightness));
            }
            NfcTapAnim::Fade { started } => {
                let t = (now.duration_since(*started).as_secs_f32() / (FADE_MS as f32 / 1000.0))
                    .clamp(0.0, 1.0);
                self.paint(LedRole::NfcRing, scale_rgb(WHOOSH, 1.0 - ease_out_cubic(t)));
            }
        }
    }

    /// Fill a whole role span with one colour (absent role: no-op).
    fn paint(&mut self, role: LedRole, colour: Rgb) {
        let Some(span) = self.layout.span(role) else {
            return;
        };
        for i in 0..span.count {
            self.put(span, i, colour);
        }
    }

    fn put(&mut self, span: ResolvedSpan, index: usize, colour: Rgb) {
        let colour = match span.max_pct {
            Some(pct) => scale_rgb(colour, f32::from(pct) / 100.0),
            None => colour,
        };
        self.frames[span.stream][span.start + index] = colour;
    }
}

/// `board.toml` colour order (`"rgb"`, `"grb"`, ...) -> component index (0 = r, 1 = g, 2 = b)
/// at each wire position. `None` for anything unknown (validation already rejects those).
fn parse_order(s: &str) -> Option<[u8; 3]> {
    let idx = |c: char| match c {
        'r' => Some(0u8),
        'g' => Some(1),
        'b' => Some(2),
        _ => None,
    };
    let mut it = s.chars();
    let o = [idx(it.next()?)?, idx(it.next()?)?, idx(it.next()?)?];
    (it.next().is_none() && o[0] != o[1] && o[1] != o[2] && o[0] != o[2]).then_some(o)
}

/// The driver is configured `Ws2811Grb` and sends wire bytes `[g', r', b']` for the logical
/// colour `(r', g', b')` it is given. An LED whose wire order is `order` must receive
/// component `order[i]` as its i-th byte, so feed the driver `g' = comp(order[0])`,
/// `r' = comp(order[1])`, `b' = comp(order[2])`. For `grb` this is the identity.
pub fn wire_swizzle(order: [u8; 3], c: Rgb) -> Rgb {
    let comp = |i: u8| match i {
        0 => c.r,
        1 => c.g,
        _ => c.b,
    };
    Rgb::new(comp(order[1]), comp(order[0]), comp(order[2]))
}

fn scale_rgb(c: Rgb, amount: f32) -> Rgb {
    let a = amount.clamp(0.0, 1.0);
    let s = |v: u8| ((v as f32) * a).round() as u8;
    Rgb::new(s(c.r), s(c.g), s(c.b))
}

fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_roles::tests_support::{minimal_profile, poc2_profile};

    fn poc2() -> LedComposer {
        LedComposer::new(LedLayout::from_profile(&poc2_profile()))
    }

    /// Stream indices follow the sorted stream names: BT_LED_DATA = 0, F_LED_DATA = 1.
    fn stream(c: &LedComposer, name: &str) -> usize {
        c.layout()
            .streams()
            .iter()
            .position(|(n, _)| n == name)
            .unwrap()
    }

    /// Regression (plippa-D-poc2): raw LEDPATCH indices driven by the pre-NB_13 code.
    #[test]
    fn poc2_steady_frame_matches_legacy_indices() {
        let mut c = poc2();
        let (top, front) = (stream(&c, "BT_LED_DATA"), stream(&c, "F_LED_DATA"));
        assert_eq!(c.frames()[top].len(), 30);
        assert_eq!(c.frames()[front].len(), 33);
        let t0 = Instant::now();

        c.compose(t0);
        assert_eq!(c.frames()[front][21], BLUE, "power LED = F_LED_PWR1");
        assert_eq!(c.frames()[front].iter().filter(|p| **p != OFF).count(), 1);
        assert_eq!(c.frames()[top][26], OFF);
        assert!(c.frames()[top].iter().all(|p| *p == OFF));

        c.set_play_button(PlayButtonLed::Standby);
        c.compose(t0);
        assert_eq!(c.frames()[top][26], PLAY_STANDBY, "play backlight = BT_LED_SW3");
        assert_eq!(c.frames()[top].iter().filter(|p| **p != OFF).count(), 1);

        c.set_play_button(PlayButtonLed::Playing);
        c.compose(t0);
        assert_eq!(c.frames()[top][26], BLUE);
    }

    #[test]
    fn poc2_nfc_animation_matches_legacy_phases() {
        let mut c = poc2();
        let top = stream(&c, "BT_LED_DATA");
        let t0 = Instant::now();
        c.on_tap_feedback(t0);

        // Whoosh head at step 0 = LED 0 only, full colour.
        c.compose(t0);
        let lit: Vec<usize> = (0..30).filter(|i| c.frames()[top][*i] != OFF).collect();
        assert_eq!(lit, vec![0]);
        assert_eq!(c.frames()[top][0], WHOOSH);

        // Step 3: head at 3, trail behind it (indices 3,2,1,0), weights 1, .55, .28, .12.
        c.compose(t0 + Duration::from_millis(3 * WHOOSH_STEP_MS));
        let lit: Vec<usize> = (0..30).filter(|i| c.frames()[top][*i] != OFF).collect();
        assert_eq!(lit, vec![0, 1, 2, 3]);
        assert_eq!(c.frames()[top][3], WHOOSH);
        assert_eq!(c.frames()[top][2], scale_rgb(WHOOSH, 0.55));

        // Ring never touches the button backlights (24..30).
        c.compose(t0 + Duration::from_millis(23 * WHOOSH_STEP_MS));
        assert!(c.frames()[top][24..].iter().all(|p| *p == OFF));

        // Whoosh ends after 24 steps -> pulse -> fade -> idle.
        let mut now = t0 + Duration::from_millis(24 * WHOOSH_STEP_MS);
        assert!(c.advance(now));
        c.compose(now);
        assert!(
            (0..24).all(|i| c.frames()[top][i] == WHOOSH),
            "pulse fills ring"
        );
        assert_eq!(c.frames()[top][24], OFF);
        now += Duration::from_millis(PULSE_MS);
        assert!(c.advance(now));
        now += Duration::from_millis(FADE_MS);
        assert!(c.advance(now));
        assert!(!c.animating());
        c.compose(now);
        assert!(c.frames()[top].iter().all(|p| *p == OFF));
    }

    #[test]
    fn play_button_stays_above_ring_animation() {
        let mut c = poc2();
        let top = stream(&c, "BT_LED_DATA");
        let t0 = Instant::now();
        c.set_play_button(PlayButtonLed::Playing);
        c.on_tap_feedback(t0);
        c.compose(t0 + Duration::from_millis(200));
        assert_eq!(c.frames()[top][26], BLUE);
    }

    #[test]
    fn role_colour_applies_to_whole_span_and_clears() {
        let mut c = poc2();
        let front = stream(&c, "F_LED_DATA");
        let top = stream(&c, "BT_LED_DATA");
        let t0 = Instant::now();
        c.set_role_colour(LedRole::VuLeft, Some(ORANGE));
        c.set_role_colour(LedRole::NextTrackButton, Some(BLUE));
        c.compose(t0);
        assert!((1..=10).all(|i| c.frames()[front][i] == ORANGE));
        assert_eq!(c.frames()[front][11], OFF);
        assert_eq!(c.frames()[top][28], BLUE);
        assert_eq!(c.role_pixels(LedRole::VuLeft), vec![ORANGE; 10]);
        c.set_role_colour(LedRole::VuLeft, None);
        c.compose(t0);
        assert_eq!(c.role_pixels(LedRole::VuLeft), vec![OFF; 10]);
    }

    /// WARP_12: FF/RW pixels are off / glow / pressed from their steady colour and no other
    /// role's pixels change.
    #[test]
    fn ffrw_pixels_follow_their_levels_and_leave_other_roles_alone() {
        use crate::led::{TRANSPORT_LED_IDLE, TRANSPORT_LED_PRESSED};
        let t0 = Instant::now();
        let others = [
            LedRole::PrevTrackButton,
            LedRole::NextTrackButton,
            LedRole::PlayButton,
            LedRole::PlayModeButton,
            LedRole::PowerLed,
            LedRole::VuLeft,
        ];
        let snapshot = |c: &LedComposer| -> Vec<Vec<Rgb>> {
            others.iter().map(|r| c.role_pixels(*r)).collect()
        };
        let mut base = poc2();
        base.set_role_colour(LedRole::PrevTrackButton, Some(TRANSPORT_LED_IDLE));
        base.set_play_button(PlayButtonLed::Standby);
        base.compose(t0);
        let before = snapshot(&base);

        let mut c = poc2();
        c.set_role_colour(LedRole::PrevTrackButton, Some(TRANSPORT_LED_IDLE));
        c.set_play_button(PlayButtonLed::Standby);
        c.compose(t0);
        for r in [LedRole::RewindButton, LedRole::FastForwardButton] {
            assert!(!c.role_pixels(r).is_empty(), "poc2 has the {r:?} LED");
            assert!(c.role_pixels(r).iter().all(|p| *p == OFF), "off until set");
        }
        c.set_role_colour(LedRole::RewindButton, Some(TRANSPORT_LED_IDLE));
        c.set_role_colour(LedRole::FastForwardButton, Some(TRANSPORT_LED_PRESSED));
        c.compose(t0);
        assert!(c.role_pixels(LedRole::RewindButton).iter().all(|p| *p == TRANSPORT_LED_IDLE));
        assert!(c.role_pixels(LedRole::FastForwardButton).iter().all(|p| *p == TRANSPORT_LED_PRESSED));
        assert_eq!(snapshot(&c), before, "other roles' pixels are identical");
        c.set_role_colour(LedRole::FastForwardButton, None);
        c.compose(t0);
        assert!(c.role_pixels(LedRole::FastForwardButton).iter().all(|p| *p == OFF));
        assert_eq!(snapshot(&c), before);
        // Minimal profile (no roles): no commands, no panic.
        let mut m = LedComposer::new(LedLayout::from_profile(&minimal_profile()));
        m.set_role_colour(LedRole::RewindButton, Some(TRANSPORT_LED_PRESSED));
        m.compose(t0);
        assert!(m.role_pixels(LedRole::RewindButton).is_empty());
    }

    /// WARP_21: a role pulse paints over its steady colour every frame, from low to high and
    /// back; clearing it returns the steady colour. Absent-role boards ignore it.
    #[test]
    fn role_pulse_rides_over_the_steady_colour_and_clears() {
        use crate::led::TransportPulse;
        use crate::warp_ui::{FFRW_LED_IDLE, FFRW_LED_PRESSED};
        let pulse = TransportPulse {
            lo: FFRW_LED_IDLE,
            hi: FFRW_LED_PRESSED,
            period_ms: 800,
        };
        let mut c = poc2();
        let t0 = Instant::now();
        c.set_role_colour(LedRole::FastForwardButton, Some(FFRW_LED_IDLE));
        c.set_role_pulse(LedRole::FastForwardButton, Some(pulse));
        c.compose(t0);
        assert!(c
            .role_pixels(LedRole::FastForwardButton)
            .iter()
            .all(|p| *p == FFRW_LED_IDLE));
        c.compose(t0 + Duration::from_millis(400));
        assert!(c
            .role_pixels(LedRole::FastForwardButton)
            .iter()
            .all(|p| *p == FFRW_LED_PRESSED));
        c.set_role_pulse(LedRole::FastForwardButton, None);
        c.compose(t0 + Duration::from_millis(100));
        assert!(c
            .role_pixels(LedRole::FastForwardButton)
            .iter()
            .all(|p| *p == FFRW_LED_IDLE));
        // Minimal profile: no FF role, pulse is a no-op, no panic.
        let mut m = LedComposer::new(LedLayout::from_profile(&minimal_profile()));
        m.set_role_pulse(LedRole::FastForwardButton, Some(pulse));
        m.compose(t0);
        assert!(m.role_pixels(LedRole::FastForwardButton).is_empty());
    }

    #[test]
    fn max_brightness_scales_role() {
        let text = std::fs::read_to_string(
            plippa_board::workspace_root_from_crate()
                .join("boards/prototypes/plippa-D-poc2/config/board.toml"),
        )
        .unwrap()
        .replace(
            "[leds.power_led]             # F_LED_PWR1",
            "[leds.power_led]\nmax_brightness_pct = 50",
        );
        let p = BoardProfile::from_toml_str(&text, ".".into()).unwrap();
        let mut c = LedComposer::new(LedLayout::from_profile(&p));
        c.compose(Instant::now());
        assert_eq!(c.role_pixels(LedRole::PowerLed), vec![Rgb::new(0, 0, 128)]);
    }

    /// Minimal board: no LEDs at all — every LED call is a harmless no-op.
    #[test]
    fn minimal_profile_has_no_leds_and_never_panics() {
        let mut c = LedComposer::new(LedLayout::from_profile(&minimal_profile()));
        assert!(c.frames().is_empty());
        let t0 = Instant::now();
        c.set_play_button(PlayButtonLed::Playing);
        c.set_role_colour(LedRole::PrevTrackButton, Some(BLUE));
        c.on_tap_feedback(t0);
        assert!(!c.animating());
        assert!(!c.advance(t0));
        c.compose(t0);
        assert!(c.role_pixels(LedRole::PlayButton).is_empty());
        c.clear_all();
    }

    /// Board with a ring but nothing else (e.g. no button backlights).
    #[test]
    fn partial_profile_runs_with_only_a_ring() {
        let text = std::fs::read_to_string(
            plippa_board::workspace_root_from_crate()
                .join("crates/plippa-board/fixtures/minimal.board.toml"),
        )
        .unwrap()
            + "\n[led_streams.RING]\ngpio = 12\nchannel = 0\ncount = 8\n\n[leds.nfc_ring]\nstream = \"RING\"\nstart = 0\ncount = 8\n";
        let p = BoardProfile::from_toml_str(&text, ".".into()).unwrap();
        let mut c = LedComposer::new(LedLayout::from_profile(&p));
        let t0 = Instant::now();
        c.set_play_button(PlayButtonLed::Playing); // no play LED: ignored
        c.on_tap_feedback(t0);
        assert!(c.animating());
        c.compose(t0);
        assert_eq!(c.frames()[0][0], WHOOSH);
        assert!(c.advance(t0 + Duration::from_millis(8 * WHOOSH_STEP_MS)));
    }
    // ---- NB_14 press flash ----------------------------------------------------------

    fn prev_next(c: &LedComposer) -> (Rgb, Rgb) {
        (
            c.role_pixels(LedRole::PrevTrackButton)[0],
            c.role_pixels(LedRole::NextTrackButton)[0],
        )
    }

    #[test]
    fn flash_overrides_steady_for_flash_ms_then_steady_returns() {
        let dim = Rgb::new(90, 90, 90);
        let full = Rgb::new(255, 255, 255);
        let mut c = poc2();
        let t0 = Instant::now();
        c.set_role_colour(LedRole::PrevTrackButton, Some(dim));
        c.set_role_colour(LedRole::NextTrackButton, Some(dim));
        c.compose(t0);
        assert_eq!(prev_next(&c), (dim, dim));

        c.flash_role(LedRole::NextTrackButton, full, t0);
        assert!(c.animating());
        c.compose(t0);
        assert_eq!(prev_next(&c), (dim, full), "only the pressed role flashes");
        let just_before = t0 + Duration::from_millis(FLASH_MS - 1);
        assert!(!c.advance(just_before));
        c.compose(just_before);
        assert_eq!(prev_next(&c).1, full);

        let end = t0 + Duration::from_millis(FLASH_MS);
        assert!(c.advance(end), "expiry asks for a re-render");
        c.compose(end);
        assert_eq!(prev_next(&c), (dim, dim), "back to the availability state");
        assert!(!c.animating());
    }

    #[test]
    fn flash_is_compose_safe_even_if_advance_is_late() {
        let mut c = poc2();
        let t0 = Instant::now();
        c.set_role_colour(LedRole::PrevTrackButton, Some(Rgb::new(90, 90, 90)));
        c.flash_role(LedRole::PrevTrackButton, Rgb::new(255, 255, 255), t0);
        c.compose(t0 + Duration::from_millis(500)); // no advance() in between
        assert_eq!(prev_next(&c).0, Rgb::new(90, 90, 90));
    }

    #[test]
    fn flash_on_unlit_role_returns_to_off_and_restarts_on_repeat() {
        let mut c = poc2();
        let t0 = Instant::now();
        c.flash_role(LedRole::PrevTrackButton, WHOOSH, t0);
        c.flash_role(
            LedRole::PrevTrackButton,
            WHOOSH,
            t0 + Duration::from_millis(100),
        );
        c.compose(t0 + Duration::from_millis(150));
        assert_eq!(prev_next(&c).0, WHOOSH, "second flash restarted the timer");
        c.advance(t0 + Duration::from_millis(100 + FLASH_MS));
        c.compose(t0 + Duration::from_millis(100 + FLASH_MS));
        assert_eq!(prev_next(&c).0, OFF);
    }

    #[test]
    fn flash_coexists_with_nfc_animation_and_other_buttons() {
        let mut c = poc2();
        let t0 = Instant::now();
        c.set_play_button(PlayButtonLed::Playing);
        c.on_tap_feedback(t0);
        let mut reference = poc2();
        reference.set_play_button(PlayButtonLed::Playing);
        reference.on_tap_feedback(t0);

        c.flash_role(LedRole::NextTrackButton, Rgb::new(255, 255, 255), t0);
        let t = t0 + Duration::from_millis(60);
        c.compose(t);
        reference.compose(t);
        for role in [LedRole::NfcRing, LedRole::PlayButton, LedRole::PowerLed] {
            assert_eq!(c.role_pixels(role), reference.role_pixels(role), "{role:?}");
        }
        assert_eq!(prev_next(&c).1, Rgb::new(255, 255, 255));
        // Channel map: prev/next share BT_LED_DATA with the ring but have their own pixels.
        let next = c.layout().span(LedRole::NextTrackButton).unwrap();
        let ring = c.layout().span(LedRole::NfcRing).unwrap();
        assert_eq!(next.stream, ring.stream);
        assert!(next.start >= ring.start + ring.count || next.start + next.count <= ring.start);
    }

    #[test]
    fn flash_respects_max_brightness_and_absent_roles() {
        let mut c = LedComposer::new(LedLayout::from_profile(&minimal_profile()));
        c.flash_role(
            LedRole::NextTrackButton,
            Rgb::new(255, 255, 255),
            Instant::now(),
        );
        assert!(!c.animating(), "board without the role: no-op");
        c.compose(Instant::now());
        assert!(c.frames().iter().all(|f| f.iter().all(|p| *p == OFF)));
    }

    /// NB_VERIFY NB_13: every frame the pre-NB_13 `Poc2LedCue` composed (power, play
    /// button states, NFC whoosh/pulse/fade at fixed ages) must equal the role-addressed
    /// composer's frame. The golden file was dumped from the code at `d35ce14` (before
    /// NB_13), colours as logical R,G,B, front = F_LED_DATA, top = BT_LED_DATA. The old code
    /// read the wall clock, so pulse/fade values may differ by 1 from rounding.
    ///
    /// One deliberate deviation: the play-button standby pixels in the fixture were updated
    /// from the old amber (255,96,0) to the current purple standby when the owner asked for a
    /// purple paused play button (2026-11-04). Nothing else in the file changed.
    #[test]
    fn frames_equal_pre_nb13_golden() {
        let golden = include_str!("../tests/fixtures/pre_nb13_led_frames.txt");
        let mut n = 0;
        for line in golden.lines() {
            let (name, rest) = line.split_once(" | F ").unwrap();
            let (front, top) = rest.split_once(" | T ").unwrap();
            let (play, phase) = name.split_once(' ').unwrap();
            let mut c = poc2();
            c.set_play_button(match play {
                "off" => PlayButtonLed::Off,
                "standby" => PlayButtonLed::Standby,
                _ => PlayButtonLed::Playing,
            });
            let t0 = Instant::now();
            let ms = |v: u64| Duration::from_millis(v);
            let at = if phase == "steady" {
                t0
            } else if let Some(k) = phase.strip_prefix("whoosh") {
                c.on_tap_feedback(t0);
                t0 + ms(WHOOSH_STEP_MS * k.parse::<u64>().unwrap() + 16)
            } else {
                let age: u64 = phase.trim_start_matches(|ch: char| ch.is_alphabetic()).parse().unwrap();
                c.on_tap_feedback(t0);
                let pulse_start = t0 + ms(24 * WHOOSH_STEP_MS);
                assert!(c.advance(pulse_start));
                if phase.starts_with("pulse") {
                    pulse_start + ms(age)
                } else {
                    let fade_start = pulse_start + ms(PULSE_MS);
                    assert!(c.advance(fade_start));
                    fade_start + ms(age)
                }
            };
            c.compose(at);
            let parse = |s: &str| -> Vec<Rgb> {
                s.split(' ')
                    .map(|p| {
                        let v: Vec<u8> = p.split(',').map(|x| x.parse().unwrap()).collect();
                        Rgb::new(v[0], v[1], v[2])
                    })
                    .collect()
            };
            for (got, want, label) in [
                (&c.frames()[stream(&c, "F_LED_DATA")], parse(front), "front"),
                (&c.frames()[stream(&c, "BT_LED_DATA")], parse(top), "top"),
            ] {
                assert_eq!(got.len(), want.len(), "{name} {label} length");
                for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                    let d = |a: u8, b: u8| a.abs_diff(b) <= 1;
                    assert!(
                        d(g.r, w.r) && d(g.g, w.g) && d(g.b, w.b),
                        "{name} {label}[{i}]: got {g:?} want {w:?}"
                    );
                }
            }
            n += 1;
        }
        assert_eq!(n, 108, "golden file scenario count");
    }

    // ---- NB_VERIFY NB_13: color_order ---------------------------------------------------

    #[test]
    fn wire_swizzle_reaches_the_led_in_its_declared_order() {
        let c = Rgb::new(10, 20, 30);
        let grb = parse_order("grb").unwrap();
        assert_eq!(wire_swizzle(grb, c), c, "grb = driver order = identity");
        // Driver emits [g', r', b'] on the wire; check the bytes an LED of each order sees.
        let wire = |order: &str| {
            let d = wire_swizzle(parse_order(order).unwrap(), c);
            [d.g, d.r, d.b]
        };
        assert_eq!(wire("grb"), [20, 10, 30]);
        assert_eq!(wire("rgb"), [10, 20, 30]);
        assert_eq!(wire("bgr"), [30, 20, 10]);
        assert_eq!(wire("brg"), [30, 10, 20]);
        assert_eq!(wire("rbg"), [10, 30, 20]);
        assert_eq!(wire("gbr"), [20, 30, 10]);
        assert_eq!(parse_order("rrg"), None);
        assert_eq!(parse_order("rg"), None);
    }

    #[test]
    fn profile_color_order_is_applied_only_to_its_span() {
        let text = std::fs::read_to_string(
            plippa_board::workspace_root_from_crate()
                .join("boards/prototypes/plippa-D-poc2/config/board.toml"),
        )
        .unwrap()
        .replace(
            "[leds.power_led]             # F_LED_PWR1",
            "[leds.power_led]\ncolor_order = \"rgb\"",
        );
        let p = BoardProfile::from_toml_str(&text, ".".into()).unwrap();
        let mut c = LedComposer::new(LedLayout::from_profile(&p));
        c.set_play_button(PlayButtonLed::Standby);
        c.set_role_colour(LedRole::PowerLed, Some(ORANGE));
        c.compose(Instant::now());
        let (front, top) = (stream(&c, "F_LED_DATA"), stream(&c, "BT_LED_DATA"));
        let d = c.driver_frames();
        // Logical orange (255,96,0) on an RGB-wired LED needs r/g swapped for the GRB driver.
        assert_eq!(c.frames()[front][21], ORANGE, "logical frame unchanged");
        assert_eq!(d[front][21], Rgb::new(96, 255, 0), "rgb-wired LED: swapped for the GRB driver");
        // Play button (no color_order) is untouched.
        assert_eq!(d[top][26], PLAY_STANDBY);
    }

    #[test]
    fn poc2_declares_no_color_order_so_driver_frames_equal_frames() {
        let mut c = poc2();
        c.set_play_button(PlayButtonLed::Standby);
        c.on_tap_feedback(Instant::now());
        c.compose(Instant::now());
        assert_eq!(c.driver_frames(), c.frames().to_vec());
    }
    // ---- ART_10 record ring -----------------------------------------------------------

    use crate::art::ArtImage;
    use crate::record_leds::{LED_MAX_BRIGHTNESS_PCT, LED_RECORD_FRAME_MS};

    fn cover(left: [u8; 3], right: [u8; 3]) -> Arc<ArtImage> {
        let rgb = (0..64 * 64)
            .flat_map(|i| if i % 64 < 32 { left } else { right })
            .collect();
        Arc::new(ArtImage {
            side: 1,
            rgb565: vec![0],
            rgb_side: 64,
            rgb,
        })
    }

    fn frame(art: &Arc<ArtImage>, angle_deg: f32) -> Option<RecordFrame> {
        Some(RecordFrame {
            art: Arc::clone(art),
            angle_deg,
        })
    }

    fn ring(c: &LedComposer) -> Vec<Rgb> {
        c.role_pixels(LedRole::NfcRing)
    }

    /// Past the crossfade, so the picture is the cover itself.
    fn settled(c: &mut LedComposer, t0: Instant) -> Instant {
        let t = t0 + Duration::from_millis(u64::from(LED_ART_CROSSFADE_MS) + 10);
        c.advance(t);
        t
    }

    #[test]
    fn record_ring_shows_the_cover_and_never_touches_other_roles() {
        let mut c = poc2();
        let t0 = Instant::now();
        c.set_play_button(PlayButtonLed::Playing);
        c.compose(t0);
        let before_other = c.frames().to_vec();
        let art = cover([255, 0, 0], [0, 0, 255]);
        c.set_record_art(frame(&art, 0.0), t0);
        let t = settled(&mut c, t0);
        c.compose(t);
        let r = ring(&c);
        assert_eq!(r.len(), 24);
        let cap = (255.0 * f32::from(LED_MAX_BRIGHTNESS_PCT) / 100.0).round() as u8;
        assert_eq!(r[6], Rgb::new(0, 0, cap), "3 o'clock = the right half, capped");
        assert_eq!(r[18], Rgb::new(cap, 0, 0));
        // Everything outside the 24 ring LEDs is exactly what it was.
        let top = stream(&c, "BT_LED_DATA");
        assert_eq!(c.frames()[top][24..], before_other[top][24..]);
        assert_eq!(c.frames()[1 - top], before_other[1 - top]);
    }

    #[test]
    fn record_ring_turns_with_the_angle_and_a_quarter_turn_moves_six_leds() {
        let mut c = poc2();
        let t0 = Instant::now();
        let art = cover([255, 0, 0], [0, 0, 255]);
        c.set_record_art(frame(&art, 0.0), t0);
        let t = settled(&mut c, t0);
        c.compose(t);
        let base = ring(&c);
        c.set_record_art(frame(&art, 90.0), t);
        c.compose(t);
        let turned = ring(&c);
        for k in 0..24 {
            assert_eq!(turned[(k + 6) % 24], base[k], "LED {k}");
        }
    }

    #[test]
    fn off_is_byte_identical_to_the_baseline() {
        let t0 = Instant::now();
        let mut plain = poc2();
        plain.set_play_button(PlayButtonLed::Standby);
        plain.set_role_colour(LedRole::NfcRing, Some(Rgb::new(1, 2, 3)));
        plain.compose(t0);

        let mut c = poc2();
        c.set_play_button(PlayButtonLed::Standby);
        c.set_role_colour(LedRole::NfcRing, Some(Rgb::new(1, 2, 3)));
        let art = cover([255, 0, 0], [0, 0, 255]);
        c.set_record_art(frame(&art, 40.0), t0);
        c.compose(t0);
        assert_ne!(c.frames(), plain.frames(), "record changes the ring");
        c.set_record_art(None, t0);
        c.compose(t0);
        assert_eq!(c.frames(), plain.frames());
        assert!(!c.animating());
        // Same after clear_all.
        c.set_record_art(frame(&art, 40.0), t0);
        c.clear_all();
        c.compose(t0);
        let mut cleared = poc2();
        cleared.clear_all();
        cleared.compose(t0);
        assert_eq!(c.frames(), cleared.frames());
    }

    #[test]
    fn tap_animation_overrides_the_record_then_hands_it_back_without_a_jump() {
        let mut c = poc2();
        let t0 = Instant::now();
        let art = cover([255, 0, 0], [0, 0, 255]);
        c.set_record_art(frame(&art, 0.0), t0);
        let t1 = settled(&mut c, t0);
        c.compose(t1);
        let before = ring(&c);

        c.on_tap_feedback(t1);
        c.compose(t1);
        assert_eq!(ring(&c)[0], WHOOSH, "the tap whoosh owns the ring");
        // The record keeps turning underneath (the loop keeps feeding angles).
        let mut t = t1;
        let mut angle = 0.0f32;
        while c.animating() {
            t += Duration::from_millis(33);
            angle = (angle + 4.0) % 360.0;
            c.set_record_art(frame(&art, angle), t);
            c.advance(t);
            c.compose(t);
        }
        // The animation is over: the ring is the record at the current angle, not at 0.
        let expect = {
            let mut e = poc2();
            e.set_record_art(frame(&art, angle), t0);
            let te = settled(&mut e, t0);
            e.compose(te);
            ring(&e)
        };
        assert_eq!(ring(&c), expect);
        assert_ne!(ring(&c), before);
        // And one frame later it moves on smoothly by the next small step, not by a jump.
        let _ = before;
    }

    #[test]
    fn flashes_on_the_ring_also_beat_the_record() {
        let mut c = poc2();
        let t0 = Instant::now();
        let art = cover([255, 0, 0], [0, 0, 255]);
        c.set_record_art(frame(&art, 0.0), t0);
        c.flash_role(LedRole::NfcRing, WHOOSH, t0);
        c.compose(t0);
        assert!(ring(&c).iter().all(|p| *p == WHOOSH));
        let t = t0 + Duration::from_millis(FLASH_MS + 1);
        c.advance(t);
        c.compose(t);
        assert!(ring(&c).iter().any(|p| *p != WHOOSH), "the record returns");
    }

    #[test]
    fn a_new_cover_crossfades_and_only_the_fade_counts_as_animating() {
        let mut c = poc2();
        let t0 = Instant::now();
        let a = cover([255, 0, 0], [255, 0, 0]);
        let b = cover([0, 0, 255], [0, 0, 255]);
        c.set_record_art(frame(&a, 0.0), t0);
        assert!(c.animating(), "fades in from dark");
        let t1 = settled(&mut c, t0);
        assert!(!c.animating(), "settled: only angle pushes move it");
        c.compose(t1);
        let cap = (255.0 * f32::from(LED_MAX_BRIGHTNESS_PCT) / 100.0).round() as u8;
        assert_eq!(ring(&c)[3], Rgb::new(cap, 0, 0));

        c.set_record_art(frame(&b, 0.0), t1);
        assert!(c.animating());
        let mid = t1 + Duration::from_millis(u64::from(LED_ART_CROSSFADE_MS) / 2);
        assert!(c.advance(mid), "a running fade changes the frame");
        c.compose(mid);
        let px = ring(&c)[3];
        assert!(px.r > 0 && px.b > 0, "halfway is a blend: {px:?}");
        let end = t1 + Duration::from_millis(u64::from(LED_ART_CROSSFADE_MS) + 1);
        c.advance(end);
        c.compose(end);
        assert_eq!(ring(&c)[3], Rgb::new(0, 0, cap));
        assert!(!c.animating());
    }

    #[test]
    fn a_frozen_record_is_stable_and_costs_no_wakeups() {
        let mut c = poc2();
        let t0 = Instant::now();
        let art = cover([255, 0, 0], [0, 0, 255]);
        c.set_record_art(frame(&art, 123.0), t0);
        let t = settled(&mut c, t0);
        c.compose(t);
        let frozen = c.frames().to_vec();
        for i in 1..50u64 {
            let ti = t + Duration::from_millis(i * LED_RECORD_FRAME_MS);
            assert!(!c.advance(ti), "no phase change while frozen");
            assert!(!c.animating());
            c.compose(ti);
            assert_eq!(c.frames(), frozen.as_slice());
        }
    }

    #[test]
    fn a_board_without_a_ring_ignores_the_record() {
        let mut c = LedComposer::new(LedLayout::from_profile(&minimal_profile()));
        let art = cover([1, 2, 3], [4, 5, 6]);
        c.set_record_art(frame(&art, 0.0), Instant::now());
        assert!(!c.animating());
        c.compose(Instant::now());
        assert!(c.frames().is_empty());
    }
}
