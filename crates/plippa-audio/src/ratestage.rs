//! `RateStage` (WARP_05): the signed-rate varispeed voice between the [`SourceWindow`] and the
//! sink, its rate ramps, the voice selector and the `RateSource` seam.
//!
//! The stage owns the **actual playhead** (`pos`, f64 source frames, signed integration) and
//! renders in 256-frame quanta: the rate is constant inside a quantum, ramps update between
//! quanta, the source position is a fractional accumulator (never `ceil()`), and `|rate|` near
//! zero is shaped by a linear gain ramp so a crossing of zero is silent rather than a click.
//!
//! At exactly 1.0x with no seek the stage is not in the path at all (the audio thread's bypass
//! reads integer positions): this module only renders while the rate is not unity (or is on
//! its way there).

use crate::sinc::{
    boxcar_block, boxcar_span_for, span_for, varispeed_block, SincTables, MAX_BOXCAR_RATE,
    MAX_SINC_RATE, TAPS_AFTER, TAPS_BEFORE,
};
use crate::window::SourceWindow;
use plippa_core::{VoiceKind, WarpMode};

/// Ramp time constant for tempo / tap changes: `TAU_TEMPO_MS`.
pub const TAU_TEMPO_MS: f64 = 120.0;
/// Ramp time constant for a reset to 100 %: `TAU_RESET_MS`.
pub const TAU_RESET_MS: f64 = 150.0;
/// `|rate|` below which the output gain is `|rate| / NEAR_ZERO_RATE`: `NEAR_ZERO_RATE`.
pub const NEAR_ZERO_RATE: f64 = 0.05;
/// Frames per render quantum (rate ramps update per quantum).
pub const QUANTUM_FRAMES: usize = 256;
/// A ramp snaps to its target when closer than this (so a return to 1.0 is exactly 1.0).
pub const RAMP_SNAP: f64 = 1e-4;
/// The stage never renders a rate whose magnitude is below this (rate is never 0).
pub const MIN_ABS_RATE: f64 = 1e-3;

/// Rate-stage constants in one place (README section 7, `RateConfig`).
#[derive(Debug, Clone, PartialEq)]
pub struct RateConfig {
    pub tau_tempo_ms: f64,
    pub tau_reset_ms: f64,
    pub near_zero_rate: f64,
    pub quantum_frames: usize,
}

impl Default for RateConfig {
    fn default() -> Self {
        Self {
            tau_tempo_ms: TAU_TEMPO_MS,
            tau_reset_ms: TAU_RESET_MS,
            near_zero_rate: NEAR_ZERO_RATE,
            quantum_frames: QUANTUM_FRAMES,
        }
    }
}

/// How a rate changes toward a new target (engine-internal; `Linear` and `Immediate` serve
/// tests and the seek drivers of later packages).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RampSpec {
    /// Exponential approach with time constant `tau_ms`.
    Exp { tau_ms: f64 },
    /// Constant slope reaching the target after `ms`.
    Linear { ms: f64 },
    /// Jump to the target.
    Immediate,
}

/// The instantaneous rate and its ramp toward a target.
#[derive(Debug, Clone)]
pub struct RateRamp {
    current: f64,
    target: f64,
    spec: RampSpec,
    /// Linear ramps: change per frame.
    slope: f64,
}

impl RateRamp {
    /// A ramp at rest at `rate`.
    pub fn at(rate: f64) -> Self {
        Self {
            current: rate,
            target: rate,
            spec: RampSpec::Immediate,
            slope: 0.0,
        }
    }

    pub fn current(&self) -> f64 {
        self.current
    }

    pub fn target(&self) -> f64 {
        self.target
    }

    /// Jump the instantaneous rate (the target is unchanged).
    pub fn set_current(&mut self, rate: f64) {
        self.current = rate;
    }

    /// Aim at `target` using `spec`.
    pub fn set_target(&mut self, target: f64, spec: RampSpec, rate_hz: u32) {
        self.target = target;
        self.spec = spec;
        match spec {
            RampSpec::Immediate => self.current = target,
            RampSpec::Linear { ms } => {
                let frames = (ms * f64::from(rate_hz) / 1000.0).max(1.0);
                self.slope = (target - self.current) / frames;
            }
            RampSpec::Exp { .. } => {}
        }
    }

    fn step_value(&self, frames: usize, rate_hz: u32) -> f64 {
        let d = self.target - self.current;
        if d == 0.0 {
            return self.target;
        }
        let next = match self.spec {
            RampSpec::Immediate => self.target,
            RampSpec::Exp { tau_ms } => {
                let k = (-(frames as f64) / (f64::from(rate_hz) * tau_ms / 1000.0).max(1.0)).exp();
                self.target - d * k
            }
            RampSpec::Linear { .. } => {
                let n = self.current + self.slope * frames as f64;
                if (self.slope > 0.0 && n >= self.target) || (self.slope < 0.0 && n <= self.target)
                {
                    self.target
                } else {
                    n
                }
            }
        };
        if (next - self.target).abs() < RAMP_SNAP {
            self.target
        } else {
            next
        }
    }

    /// The rate the next `frames`-frame quantum would use (no state change).
    pub fn peek(&self, frames: usize, rate_hz: u32) -> f64 {
        self.step_value(frames, rate_hz)
    }

    /// Advance by `frames` frames and return the new instantaneous rate.
    pub fn advance(&mut self, frames: usize, rate_hz: u32) -> f64 {
        self.current = self.step_value(frames, rate_hz);
        self.current
    }

    /// True when the rate is exactly `rate` and aims at nothing else.
    pub fn settled_at(&self, rate: f64) -> bool {
        self.current == rate && self.target == rate
    }
}

/// Where the varispeed reader takes its rate from (the voice sharing rule: a seek that uses the
/// same voice kind is only a change of rate source).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateSource {
    /// The set-rate ramp (tempo, taps, reset).
    Speed,
    /// A seek driver supplies the rate (scan WARP_17, chase WARP_14): stubbed here, the value is
    /// used as given for the quantum.
    Override(f64),
}

/// Which voice a quantum uses (`speed_warp_mode`, the set rate, whether a seek is active).
#[derive(Debug, Clone, Copy, Default)]
pub struct VoiceSelector {
    /// True once the pitch-correct stretcher exists (WARP_09).
    pub stretch_available: bool,
}

impl VoiceSelector {
    /// Voice for the speed family; with a seek active the seek voice (varispeed by default) is
    /// returned. Never `Stretch` for a negative rate (reverse is always pitch-warped).
    pub fn select(&self, mode: WarpMode, set_rate: f64, seek_active: bool) -> VoiceKind {
        if seek_active {
            return VoiceKind::Varispeed;
        }
        if set_rate == 1.0 {
            return VoiceKind::Bypass;
        }
        if set_rate < 0.0 {
            return VoiceKind::Varispeed;
        }
        match mode {
            WarpMode::PitchShift => VoiceKind::Varispeed,
            WarpMode::PitchCorrect if self.stretch_available => VoiceKind::Stretch,
            WarpMode::PitchCorrect => VoiceKind::Varispeed,
        }
    }
}

impl VoiceSelector {
    /// Voice of the seek family at `rate` (`travel` forward/backward): `Stretch` only for
    /// `PitchCorrect` forward travel within the stretch cap of `driver`; backward travel and
    /// anything faster is varispeed. A scan/chase also needs `stretch_available`.
    pub fn select_seek(
        &self,
        mode: WarpMode,
        dir: plippa_core::ScanDir,
        driver: plippa_core::SeekDriver,
        rate: f64,
    ) -> VoiceKind {
        let v = plippa_core::seek_voice(mode, dir);
        if v == VoiceKind::Stretch
            && (!self.stretch_available || rate.abs() > plippa_core::max_rate(mode, dir, driver))
        {
            return VoiceKind::Varispeed;
        }
        v
    }
}

/// Result of rendering one quantum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Quantum {
    /// `frames` output frames were written to the buffer.
    Rendered {
        frames: usize,
        /// Instantaneous rate used (signed).
        rate: f64,
        /// Reverse reached frame 0: the stage clamped and now runs forward (tape bounce).
        bounced: bool,
        /// Silence: reverse ran out of window; position held until the worker places the
        /// missing back-window (then the stage fades back in by itself).
        stalled: bool,
    },
    /// The source data the quantum needs is not decoded yet (nothing was consumed).
    NeedData,
    /// Forward end of the stream: nothing left to render.
    End,
}

/// The varispeed voice with its ramp and playhead.
#[derive(Debug)]
pub struct RateStage {
    cfg: RateConfig,
    channels: usize,
    rate_hz: u32,
    /// Actual playhead in source frames (the written position).
    pub pos: f64,
    ramp: RateRamp,
    pub source: RateSource,
    last_gain: f32,
    stalled: bool,
    span: Vec<f32>,
    /// f64 prefix sums for the boxcar reader (one channel at a time).
    prefix: Vec<f64>,
}

impl RateStage {
    /// Stage at `pos` running at `rate` (target = rate).
    pub fn new(cfg: RateConfig, channels: usize, rate_hz: u32, pos: f64, rate: f64) -> Self {
        let channels = channels.max(1);
        // |rate| <= 8 over one quantum plus the boxcar margins (the sinc kernel needs less).
        let span_frames = (cfg.quantum_frames as f64 * MAX_BOXCAR_RATE) as usize + 80;
        Self {
            cfg,
            channels,
            rate_hz,
            pos,
            ramp: RateRamp::at(rate),
            source: RateSource::Speed,
            last_gain: if rate.abs() >= NEAR_ZERO_RATE {
                1.0
            } else {
                (rate.abs() / NEAR_ZERO_RATE) as f32
            },
            stalled: false,
            span: vec![0.0; span_frames * channels],
            prefix: vec![0.0; span_frames + 1],
        }
    }

    pub fn config(&self) -> &RateConfig {
        &self.cfg
    }

    pub fn ramp(&self) -> &RateRamp {
        &self.ramp
    }

    /// Instantaneous rate (as of the last rendered quantum).
    pub fn rate(&self) -> f64 {
        self.ramp.current()
    }

    pub fn target(&self) -> f64 {
        self.ramp.target()
    }

    /// True when the stage is at exactly 1.0x and aims at 1.0x: the caller may hand over to
    /// the bypass path.
    pub fn at_unity(&self) -> bool {
        self.ramp.settled_at(1.0)
    }

    pub fn stalled(&self) -> bool {
        self.stalled
    }

    /// Aim at a new set rate. A forward target ends a reverse stall.
    pub fn set_target(&mut self, target: f64, spec: RampSpec) {
        self.ramp.set_target(target, spec, self.rate_hz);
        if target > 0.0 {
            self.stalled = false;
        }
    }

    /// Start running from `pos` at exactly 1.0x (entering varispeed from the bypass path).
    pub fn enter_from_bypass(&mut self, pos: f64) {
        self.pos = pos;
        self.ramp.set_current(1.0);
        self.last_gain = 1.0;
        self.stalled = false;
    }

    /// Move the playhead (a hard seek landed).
    pub fn reposition(&mut self, pos: f64) {
        self.pos = pos.max(0.0);
        self.stalled = false;
    }

    fn clamp_rate(r: f64) -> f64 {
        let m = r.abs().clamp(MIN_ABS_RATE, MAX_BOXCAR_RATE);
        if r < 0.0 {
            -m
        } else {
            m
        }
    }

    /// Source span the reader needs for `frames` frames at `r` from `pos`.
    fn span(pos: f64, r: f64, frames: usize) -> (i64, usize) {
        if r.abs() > MAX_SINC_RATE {
            boxcar_span_for(pos, r, frames)
        } else {
            span_for(pos, r, frames)
        }
    }

    /// Render `frames` frames at constant `r` from `pos` into `out` (no ramp, no gain). Up to
    /// 2x: the band-limited sinc; beyond it the lo-fi boxcar + cubic reader (scan, WARP_17).
    fn render_const(&mut self, window: &SourceWindow, pos: f64, r: f64, out: &mut [f32]) {
        let ch = self.channels;
        let frames = out.len() / ch;
        let (first, count) = Self::span(pos, r, frames);
        debug_assert!(count * ch <= self.span.len());
        let count = count.min(self.span.len() / ch);
        window.read_span(first, &mut self.span[..count * ch]);
        if r.abs() > MAX_SINC_RATE {
            boxcar_block(&self.span[..count * ch], ch, first, pos, r, out, &mut self.prefix);
            return;
        }
        varispeed_block(
            SincTables::global(),
            &self.span[..count * ch],
            ch,
            first,
            pos,
            r,
            out,
        );
    }

    /// The continuation the stage would output next at its current rate, for a crossfade:
    /// `out.len() / channels` frames, no state change.
    pub fn preview(&mut self, window: &SourceWindow, out: &mut [f32]) {
        let ch = self.channels;
        let total = out.len() / ch;
        let r = Self::clamp_rate(self.ramp.current());
        let mut pos = self.pos;
        let mut done = 0;
        while done < total {
            let n = (total - done).min(self.cfg.quantum_frames);
            self.render_const(window, pos, r, &mut out[done * ch..(done + n) * ch]);
            pos += n as f64 * r;
            done += n;
        }
    }

    /// Render one quantum (up to `quantum_frames` frames) into `out`. `eof` says the decoder has
    /// delivered the whole stream into the window.
    pub fn render(&mut self, window: &SourceWindow, eof: bool, out: &mut [f32]) -> Quantum {
        let ch = self.channels;
        let q = self.cfg.quantum_frames.min(out.len() / ch);
        if q == 0 {
            return Quantum::NeedData;
        }
        let hz = self.rate_hz;
        if self.stalled {
            // Resume (fade back in from the held silence) as soon as the worker has placed
            // the missing back-window in front of the held position.
            let r = match self.source {
                RateSource::Speed => Self::clamp_rate(self.ramp.peek(q, hz)),
                RateSource::Override(v) => Self::clamp_rate(v),
            };
            let (first, _) = Self::span(self.pos, r, q);
            if r < 0.0 && (window.start_frame() == 0 || first >= window.start_frame() as i64) {
                self.stalled = false;
            }
        }
        if self.stalled {
            out[..q * ch].fill(0.0);
            self.last_gain = 0.0;
            let rate = self.ramp.advance(q, hz);
            return Quantum::Rendered {
                frames: q,
                rate,
                bounced: false,
                stalled: true,
            };
        }
        let r = match self.source {
            RateSource::Speed => Self::clamp_rate(self.ramp.peek(q, hz)),
            RateSource::Override(v) => Self::clamp_rate(v),
        };
        let pos0 = self.pos;
        let end = window.end_frame() as i64;
        let mut n = q;
        let mut bounce = false;
        if r > 0.0 {
            if eof {
                let remain = window.end_frame() as f64 - pos0;
                if remain <= 0.0 {
                    return Quantum::End;
                }
                n = n.min(((remain / r).ceil() as usize).max(1));
            } else {
                let (first, count) = Self::span(pos0, r, n);
                if first + count as i64 > end {
                    return Quantum::NeedData;
                }
            }
        } else {
            if !eof && pos0.floor() as i64 + TAPS_AFTER as i64 >= end {
                return Quantum::NeedData;
            }
            let steps_to_zero = (pos0 / -r).floor() as usize + 1;
            if steps_to_zero < n {
                n = steps_to_zero;
            }
            bounce = pos0 + n as f64 * r < 0.0;
        }
        let (first, _) = Self::span(pos0, r, n);
        let stall_now =
            r < 0.0 && !bounce && window.start_frame() > 0 && first < window.start_frame() as i64;
        self.render_const(window, pos0, r, &mut out[..n * ch]);
        // Gain: near-zero shaping, or the fade-out of a reverse stall; linear across the
        // quantum from the previous gain so there is no step.
        let g1 = if stall_now {
            0.0
        } else {
            (r.abs() / self.cfg.near_zero_rate).min(1.0) as f32
        };
        let g0 = self.last_gain;
        if g0 != 1.0 || g1 != 1.0 {
            for k in 0..n {
                let g = g0 + (g1 - g0) * ((k + 1) as f32 / n as f32);
                for c in 0..ch {
                    out[k * ch + c] *= g;
                }
            }
        }
        self.last_gain = g1;
        if matches!(self.source, RateSource::Speed) {
            self.ramp.advance(n, hz);
        }
        self.pos = pos0 + n as f64 * r;
        if stall_now {
            self.stalled = true;
        }
        if bounce {
            // Tape bounce: clamp at frame 0 and run forward again at the set tempo, starting
            // softly (the near-zero gain shaping makes the turn silent).
            self.pos = 0.0;
            let forward = self.ramp.target().abs();
            self.ramp.set_current(self.cfg.near_zero_rate);
            self.ramp.set_target(
                forward,
                RampSpec::Exp {
                    tau_ms: self.cfg.tau_tempo_ms,
                },
                hz,
            );
            self.last_gain = 1.0;
        }
        Quantum::Rendered {
            frames: n,
            rate: r,
            bounced: bounce,
            stalled: stall_now,
        }
    }

    /// Rate the next `frames`-frame quantum would use (ramp preview, no state change): the
    /// stretch voice takes its rate from the same ramp.
    pub fn peek_rate(&self, frames: usize) -> f64 {
        self.ramp.peek(frames, self.rate_hz)
    }

    /// Advance the rate ramp by `frames` rendered by another voice (the stretch voice owns
    /// the position there, see [`RateStage::pos`], which the caller mirrors).
    pub fn advance_ramp(&mut self, frames: usize) -> f64 {
        self.ramp.advance(frames, self.rate_hz)
    }

    /// First source frame the kernel of the current position reaches back to (diagnostics).
    pub fn history_needed_frames() -> usize {
        TAPS_BEFORE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{
        active_length_frames, click_metric, click_train, detect_click_times, fft_peak_frequency,
        rms, scan_numeric, silence_tail, sine, smooth_bound,
    };
    use crate::window::WindowConfig;

    const HZ: u32 = 44_100;

    /// Window holding all of `x` (mono), as if fully decoded.
    fn window_with(x: &[f32]) -> SourceWindow {
        let cfg = WindowConfig {
            window_cap_bytes: (x.len() + 16) * 4,
            history_s: 1.0,
            lookahead_s: 1.0,
            min_history_s: 0.0,
            ..WindowConfig::default()
        };
        let mut w = SourceWindow::new(1, HZ, &cfg);
        w.reset(0);
        assert!(w.push(x, 0));
        w
    }

    /// Run the stage at a settled `rate` over the whole window; returns the output.
    fn render_all(x: &[f32], pos: f64, rate: f64, max_frames: usize) -> Vec<f32> {
        let w = window_with(x);
        let mut st = RateStage::new(RateConfig::default(), 1, HZ, pos, rate);
        let mut out = Vec::new();
        let mut buf = vec![0.0f32; QUANTUM_FRAMES];
        while out.len() < max_frames {
            match st.render(&w, true, &mut buf) {
                Quantum::Rendered { frames, .. } => out.extend_from_slice(&buf[..frames]),
                _ => break,
            }
        }
        out
    }

    #[test]
    fn seek_selector_stretches_only_forward_within_the_cap() {
        use plippa_core::{ScanDir, SeekDriver};
        let sel = VoiceSelector {
            stretch_available: true,
        };
        let pc = WarpMode::PitchCorrect;
        let pick = |m, d, r| sel.select_seek(m, d, SeekDriver::Scan, r);
        assert_eq!(pick(pc, ScanDir::Forward, 2.5), VoiceKind::Stretch);
        assert_eq!(pick(pc, ScanDir::Forward, 3.5), VoiceKind::Varispeed, "over the cap");
        assert_eq!(pick(pc, ScanDir::Backward, 1.5), VoiceKind::Varispeed);
        assert_eq!(pick(WarpMode::PitchShift, ScanDir::Forward, 2.0), VoiceKind::Varispeed);
        let off = VoiceSelector::default();
        assert_eq!(
            off.select_seek(pc, ScanDir::Forward, SeekDriver::Chase, 1.5),
            VoiceKind::Varispeed
        );
    }

    #[test]
    fn selector_table_never_stretches_a_negative_rate() {
        let sel = VoiceSelector {
            stretch_available: true,
        };
        for mode in [WarpMode::PitchShift, WarpMode::PitchCorrect] {
            for rate in [-2.0, -1.0, -0.1] {
                assert_eq!(
                    sel.select(mode, rate, false),
                    VoiceKind::Varispeed,
                    "{mode:?} {rate}"
                );
            }
            assert_eq!(sel.select(mode, 1.0, false), VoiceKind::Bypass);
            assert_eq!(
                sel.select(mode, 1.0, true),
                VoiceKind::Varispeed,
                "a seek is never bypass"
            );
        }
        assert_eq!(
            sel.select(WarpMode::PitchShift, 1.5, false),
            VoiceKind::Varispeed
        );
        assert_eq!(
            sel.select(WarpMode::PitchCorrect, 1.5, false),
            VoiceKind::Stretch
        );
        let none = VoiceSelector::default();
        assert_eq!(
            none.select(WarpMode::PitchCorrect, 1.5, false),
            VoiceKind::Varispeed
        );
    }

    #[test]
    fn exp_ramp_approaches_and_snaps_exactly() {
        let mut r = RateRamp::at(1.0);
        r.set_target(2.0, RampSpec::Exp { tau_ms: 120.0 }, HZ);
        let tau_frames = 0.120 * f64::from(HZ);
        let mut frames = 0.0;
        let mut at_tau = None;
        while r.current() != 2.0 {
            r.advance(256, HZ);
            frames += 256.0;
            if at_tau.is_none() && frames >= tau_frames {
                at_tau = Some(r.current());
            }
            assert!(frames < 10.0 * f64::from(HZ), "never settles");
        }
        // One time constant covers about 63% of the way.
        let v = at_tau.unwrap();
        assert!((v - (1.0 + 0.632)).abs() < 0.05, "{v}");
        assert!(r.settled_at(2.0));
        // Linear and immediate specs.
        let mut l = RateRamp::at(1.0);
        l.set_target(0.5, RampSpec::Linear { ms: 100.0 }, HZ);
        for _ in 0..100 {
            l.advance(256, HZ);
        }
        assert_eq!(l.current(), 0.5);
        l.set_target(1.5, RampSpec::Immediate, HZ);
        assert_eq!(l.current(), 1.5);
    }

    #[test]
    fn pitch_and_duration_follow_the_rate() {
        let n = 3 * HZ as usize;
        let tone = silence_tail(&sine(1000.0, HZ, n, 0.5), HZ as usize);
        for rate in [0.1, 0.25, 0.5, 0.75, 1.25, 1.5, 2.0] {
            // Start well inside so the silent lead-in does not matter, render a bounded span.
            let frames = (1.2 * HZ as f64) as usize;
            let out = render_all(&tone, 0.5 * f64::from(HZ), rate, frames);
            let measured = fft_peak_frequency(&out[2048..], HZ);
            let ratio = measured / 1000.0;
            assert!(
                (ratio / rate - 1.0).abs() < 0.002,
                "rate {rate}: pitch ratio {ratio}"
            );
        }
        // Output length is source / rate within a frame (play a whole short tone to its end).
        for rate in [0.5, 0.75, 1.25, 2.0] {
            let src: Vec<f32> = sine(1000.0, HZ, 20_000, 0.5);
            let out = render_all(&src, 0.0, rate, usize::MAX);
            let want = (20_000.0 / rate).ceil() as usize;
            assert!(
                out.len().abs_diff(want) <= 1,
                "rate {rate}: {} vs {want}",
                out.len()
            );
        }
    }

    #[test]
    fn consumption_has_no_ceil_drift() {
        let x = vec![0.1f32; 400_000];
        let w = window_with(&x);
        let mut st = RateStage::new(RateConfig::default(), 1, HZ, 100.0, 0.65);
        let mut buf = vec![0.0f32; QUANTUM_FRAMES];
        for _ in 0..1000 {
            assert!(matches!(
                st.render(&w, true, &mut buf),
                Quantum::Rendered { .. }
            ));
        }
        let consumed = st.pos - 100.0;
        assert!(
            (consumed - 1000.0 * 256.0 * 0.65).abs() <= 1.0,
            "{consumed}"
        );
    }

    #[test]
    fn alias_and_passband() {
        let n = HZ as usize;
        // 19 kHz at 1.5x and 2.0x: the kernel cutoff removes it (-60 dB re input).
        for rate in [1.5, 2.0] {
            let x = sine(19_000.0, HZ, n, 0.5);
            let out = render_all(&x, 2000.0, rate, 8_000);
            let level = 20.0 * (rms(&out[1024..]) / rms(&x)).log10();
            assert!(level < -60.0, "rate {rate}: residual {level:.1} dB");
        }
        // Passband flat to 0.9 Nyquist at rate <= 1 (rate 0.5: output tone = tone * 0.5).
        let nyq = f64::from(HZ) / 2.0;
        for frac in [0.1, 0.5, 0.8, 0.9] {
            let f = nyq * frac / 0.5; // source tone that lands at frac*Nyquist after 0.5x
            if f >= nyq {
                continue;
            }
            // Use rate 1.0 for the check at frac of Nyquist directly.
            let x = sine(nyq * frac, HZ, n, 0.5);
            let out = render_all(&x, 2000.5, 1.0, 8_000);
            let gain_db = 20.0 * (rms(&out[1024..7000]) / rms(&x[2000..9000])).log10();
            assert!(gain_db.abs() < 0.3, "{frac} Nyquist: {gain_db:.2} dB");
        }
    }

    #[test]
    fn reverse_mirrors_the_input() {
        let n = 20_000usize;
        let x: Vec<f32> = (0..n)
            .map(|i| ((i * 7919 % 1000) as f32 - 500.0) / 1000.0)
            .collect();
        let out = render_all(&x, 15_000.0, -1.0, 4_000);
        assert!(out.len() >= 4_000);
        for k in 0..4_000 {
            assert!((out[k] - x[15_000 - k]).abs() < 1e-6, "frame {k}");
        }
        // A click train keeps its spacing, mirrored.
        let train = click_train(HZ, 30_000, 20, 1.0);
        let out = render_all(&train, 29_000.0, -1.0, 20_000);
        let times = detect_click_times(&out, 0.5, 100);
        let gaps: Vec<usize> = times.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            gaps.len() >= 10 && gaps.iter().all(|g| g.abs_diff(882) <= 1),
            "{gaps:?}"
        );
    }

    #[test]
    fn reverse_bounces_at_frame_zero_and_runs_forward_at_the_set_tempo() {
        let x: Vec<f32> = sine(300.0, HZ, 30_000, 0.5);
        let w = window_with(&x);
        let mut st = RateStage::new(RateConfig::default(), 1, HZ, 2_000.0, -1.0);
        let mut buf = vec![0.0f32; QUANTUM_FRAMES];
        let mut bounced = false;
        for _ in 0..40 {
            if let Quantum::Rendered { bounced: b, .. } = st.render(&w, true, &mut buf) {
                bounced |= b;
            }
            if bounced {
                break;
            }
        }
        assert!(bounced, "reached frame 0");
        assert_eq!(st.pos, 0.0);
        assert_eq!(st.target(), 1.0, "forward at the set tempo magnitude");
        assert!(
            st.rate() > 0.0 && st.rate() < 0.1,
            "starts softly: {}",
            st.rate()
        );
        let before = st.pos;
        for _ in 0..10 {
            st.render(&w, true, &mut buf);
        }
        assert!(st.pos > before, "moves forward again");
    }

    #[test]
    fn reverse_beyond_the_window_fades_out_and_holds_silence() {
        // A window that starts at frame 50_000 (older history evicted): reverse hits its start.
        let cfg = WindowConfig {
            window_cap_bytes: 40_000 * 4,
            history_s: 1.0,
            lookahead_s: 1.0,
            min_history_s: 0.0,
            ..WindowConfig::default()
        };
        let mut w = SourceWindow::new(1, HZ, &cfg);
        w.reset(50_000);
        let x = sine(500.0, HZ, 20_000, 0.5);
        assert!(w.push(&x, 50_000));
        let mut st = RateStage::new(RateConfig::default(), 1, HZ, 50_800.0, -1.0);
        let mut buf = vec![0.0f32; QUANTUM_FRAMES];
        let mut stalled_at = None;
        for i in 0..20 {
            if let Quantum::Rendered { stalled, .. } = st.render(&w, false, &mut buf) {
                if stalled {
                    stalled_at = Some(i);
                    break;
                }
            }
        }
        let i = stalled_at.expect("stalls at the oldest frame");
        // The stalling quantum fades to (near) silence at its end; later ones are silent.
        assert!(buf[QUANTUM_FRAMES - 1].abs() < 0.02);
        let held = st.pos;
        assert!(
            held >= 50_000.0 - QUANTUM_FRAMES as f64 && i < 20,
            "held {held}"
        );
        let q = st.render(&w, false, &mut buf);
        assert!(matches!(q, Quantum::Rendered { stalled: true, .. }));
        assert!(buf.iter().all(|s| *s == 0.0));
        assert_eq!(st.pos, held, "position holds");
        // A forward set rate resumes.
        st.set_target(1.0, RampSpec::Immediate);
        assert!(!st.stalled());
    }

    #[test]
    fn a_stalled_reverse_resumes_when_the_back_window_arrives() {
        let cfg = WindowConfig {
            window_cap_bytes: 40_000 * 4,
            history_s: 1.0,
            lookahead_s: 1.0,
            min_history_s: 0.0,
            ..WindowConfig::default()
        };
        let mut w = SourceWindow::new(1, HZ, &cfg);
        w.reset(50_000);
        let x = sine(500.0, HZ, 20_000, 0.5);
        assert!(w.push(&x, 50_000));
        let mut st = RateStage::new(RateConfig::default(), 1, HZ, 50_800.0, -1.0);
        let mut buf = vec![0.0f32; QUANTUM_FRAMES];
        for _ in 0..20 {
            st.render(&w, false, &mut buf);
            if st.stalled() {
                break;
            }
        }
        assert!(st.stalled());
        let held = st.pos;
        st.render(&w, false, &mut buf);
        assert!(st.stalled() && buf.iter().all(|s| *s == 0.0), "still held without data");
        // The worker places 20_000 frames before the window.
        let before = sine(500.0, HZ, 20_000, 0.5);
        assert!(w.prepend(&before, 50_000));
        assert_eq!(w.start_frame(), 30_000);
        let q = st.render(&w, false, &mut buf);
        assert!(matches!(q, Quantum::Rendered { stalled: false, .. }));
        assert!(!st.stalled());
        assert!(st.pos < held, "moving backward again");
        assert!(buf.iter().any(|s| s.abs() > 0.01), "audio is back");
        assert!(buf[0].abs() < 0.2, "fades in from the held silence");
    }

    #[test]
    fn transitions_have_no_click_and_no_nan() {
        let amp = 0.5;
        let n = 4 * HZ as usize;
        let x = sine(1000.0, HZ, n, amp);
        let w = window_with(&x);
        let bound = smooth_bound(amp as f64, 1000.0 * 2.0, HZ);
        // Unity -> ramp to 1.5 -> ramp back to 1.0 (settles exactly) -> through a zero crossing.
        let mut st = RateStage::new(RateConfig::default(), 1, HZ, 1000.0, 1.0);
        let mut out = Vec::new();
        let mut buf = vec![0.0f32; QUANTUM_FRAMES];
        let mut push = |st: &mut RateStage, quanta: usize, out: &mut Vec<f32>| {
            for _ in 0..quanta {
                if let Quantum::Rendered { frames, .. } = st.render(&w, true, &mut buf) {
                    out.extend_from_slice(&buf[..frames]);
                }
            }
        };
        st.set_target(1.5, RampSpec::Exp { tau_ms: 120.0 });
        push(&mut st, 60, &mut out);
        assert!(st.rate() > 1.4);
        st.set_target(1.0, RampSpec::Exp { tau_ms: 150.0 });
        for _ in 0..400 {
            push(&mut st, 1, &mut out);
            if st.at_unity() {
                break;
            }
        }
        assert!(st.at_unity(), "ramp lands exactly on 1.0");
        let m = click_metric(&out, bound);
        assert!(m < 1.5, "click metric across varispeed ramps {m}");
        // Through zero: forward -> reverse.
        let mut st = RateStage::new(RateConfig::default(), 1, HZ, 3.0 * f64::from(HZ), 1.0);
        let mut out2 = Vec::new();
        st.set_target(-1.0, RampSpec::Exp { tau_ms: 120.0 });
        push(&mut st, 80, &mut out2);
        assert!(st.rate() < -0.9);
        let m = click_metric(&out2, bound);
        assert!(m < 1.5, "click metric through the zero crossing {m}");
        assert!(scan_numeric(&out2).is_clean());
    }

    #[test]
    fn extreme_rates_and_tiny_inputs_stay_finite() {
        let x: Vec<f32> = (0..20_000)
            .map(|i| if i % 2 == 0 { 1e-30 } else { -1e-30 })
            .collect();
        for rate in [-2.0, -0.1, 0.1, 2.0, 0.001, 5.0] {
            let out = render_all(&x, 10_000.0, rate, 3_000);
            assert!(
                scan_numeric(&out).nan == 0 && scan_numeric(&out).inf == 0,
                "rate {rate}"
            );
        }
        assert!(active_length_frames(&[0.0, 0.0], 0.1) == 0);
    }

    #[test]
    fn agrees_with_the_rubato_oracle_on_pitch_and_level() {
        use rubato::{
            Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
            WindowFunction,
        };
        let n = 2 * HZ as usize;
        let x: Vec<f32> = sine(1000.0, HZ, n, 0.4)
            .iter()
            .zip(sine(2500.0, HZ, n, 0.2))
            .map(|(a, b)| a + b)
            .collect();
        for rate in [0.5f64, 1.5] {
            let ours = render_all(&x, 4_000.0, rate, 40_000);
            // Oracle: resample the same signal by the ratio 1/rate (slower = longer).
            let params = SincInterpolationParameters {
                sinc_len: 256,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Linear,
                oversampling_factor: 256,
                window: WindowFunction::BlackmanHarris2,
            };
            let chunk = 1024;
            let mut rs = SincFixedIn::<f32>::new(1.0 / rate, 1.1, params, chunk, 1).unwrap();
            let mut oracle = Vec::new();
            let mut off = 4_000;
            while off + chunk <= x.len() && oracle.len() < 60_000 {
                let out = rs.process(&[&x[off..off + chunk]], None).unwrap();
                oracle.extend_from_slice(&out[0]);
                off += chunk;
            }
            let (a, b) = (&ours[4096..36_000], &oracle[4096..36_000]);
            let (fa, fb) = (fft_peak_frequency(a, HZ), fft_peak_frequency(b, HZ));
            assert!(
                (fa / fb - 1.0).abs() < 0.001,
                "rate {rate}: {fa} vs {fb} Hz"
            );
            let db = 20.0 * (rms(a) / rms(b)).log10();
            assert!(db.abs() < 0.3, "rate {rate}: level differs by {db:.2} dB");
        }
    }

    #[test]
    fn click_positions_map_to_source_within_a_millisecond_through_ramps() {
        let n = 3 * HZ as usize;
        let train = click_train(HZ, n, 100, 1.0);
        let w = window_with(&train);
        let mut st = RateStage::new(RateConfig::default(), 1, HZ, 4_410.0, 1.0);
        st.set_target(1.6, RampSpec::Exp { tau_ms: 120.0 });
        let mut buf = vec![0.0f32; QUANTUM_FRAMES];
        let mut out = Vec::new();
        let mut pos_at = Vec::new(); // source position at the start of each output frame
        for _ in 0..200 {
            let p0 = st.pos;
            let q = st.render(&w, true, &mut buf);
            let Quantum::Rendered { frames, rate, .. } = q else {
                break;
            };
            for k in 0..frames {
                pos_at.push(p0 + k as f64 * rate);
            }
            out.extend_from_slice(&buf[..frames]);
        }
        let times = detect_click_times(&out, 0.3, 200);
        assert!(times.len() > 8);
        for t in times {
            let src = pos_at[t];
            let nearest = (src / 4_410.0).round() * 4_410.0;
            assert!(
                (src - nearest).abs() <= 44.0,
                "click at output {t} maps to source {src}"
            );
        }
    }
}
