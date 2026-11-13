//! Pitch-correct voice (WARP_09): the [`Stretcher`] trait, its two implementations and
//! [`StretchVoice`], the voice the audio thread drives in 256-frame quanta.
//!
//! * Signalsmith Stretch (C++, vendored under `third_party/signalsmith-stretch/`) behind the
//!   cargo feature `stretch`: [`SignalsmithStretcher`].
//! * [`WsolaStretcher`](crate::wsola::WsolaStretcher): in-tree fallback, used when the feature
//!   is off.
//!
//! **Position model.** After priming with audio ending at source frame `c` the first output is
//! the audio at `c - lag`, with `lag = input_latency + output_latency x rate` (WARP_02 finding;
//! [`Stretcher::lag_frames`]). The voice therefore primes with audio ending at
//! `heard + lag` (the lookahead that is already decoded, never decoding for it: it waits for the
//! data instead) so the first output is exactly the audio at the requested position, and its
//! published playhead `pos()` is `consumed - lag(rate)`, the source position of the audio just
//! written. Reverse never reaches this voice (negative rates select varispeed upstream).

use crate::ratestage::Quantum;
use crate::window::SourceWindow;
use crate::wsola::WsolaStretcher;

/// Output frames per stretcher call (`STRETCH_BLOCK_FRAMES`).
pub const STRETCH_BLOCK_FRAMES: usize = 256;
/// First-audio prefetch with the stretch voice active (replaces `START_PREFETCH_BYTES`).
pub const STRETCH_PREFETCH_BYTES: usize = 64 * 1024;
/// Smallest rate the stretch voice accepts (10 %).
pub const STRETCH_MIN_RATE: f64 = 0.1;
/// Largest rate the stretch voice accepts (the seek voice reaches 3-4x; the speed range is 2x).
pub const STRETCH_MAX_RATE: f64 = 4.0;

/// Signalsmith preset (`Default` = 120 ms window / 30 ms hop; `Cheaper` = lighter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StretchPreset {
    Default,
    Cheaper,
}

/// Stretcher constants in one place (README section 7). Conservative defaults; the CM4 decides
/// (see `WarpPackagesTestOnDevice.md`, WARP_09-10).
#[derive(Debug, Clone, PartialEq)]
pub struct StretchConfig {
    /// Output frames per call: the render quantum of the voice.
    pub block_frames: usize,
    /// Signalsmith preset.
    pub preset: StretchPreset,
    /// Past audio fed to a (re)primed stretcher, ms (Signalsmith: window + interval = 150).
    pub prime_ms: f64,
    /// WSOLA fallback: window length, ms.
    pub wsola_window_ms: f64,
    /// WSOLA fallback: search range either side of the nominal position, ms.
    pub wsola_search_ms: f64,
    /// Force the in-tree WSOLA even when the `stretch` feature is built (tests, fallback).
    pub force_wsola: bool,
}

impl Default for StretchConfig {
    fn default() -> Self {
        Self {
            block_frames: STRETCH_BLOCK_FRAMES,
            preset: StretchPreset::Default,
            prime_ms: 150.0,
            wsola_window_ms: 40.0,
            wsola_search_ms: 10.0,
            force_wsola: false,
        }
    }
}

/// A streaming time-stretcher on interleaved f32 frames. `process` takes any number of input
/// frames and fills exactly `out.len()` output frames (input/output ratio = playback rate).
pub trait Stretcher: Send {
    /// Consume `input`, fill `out`.
    fn process(&mut self, input: &[f32], out: &mut [f32]);
    /// Reset and pre-roll with `past` (audio ending at the priming position) at `rate`.
    fn reset_prime(&mut self, past: &[f32], rate: f64);
    /// Output what is still buffered (`output_latency` frames at the end of a stream).
    fn flush(&mut self, out: &mut [f32]);
    /// `(input_latency, output_latency)` in frames.
    fn latency(&self) -> (usize, usize);
    /// How far the first output after priming trails the end of the past, in input frames at
    /// `rate` (`input_latency + output_latency x rate` for Signalsmith).
    fn lag_frames(&self, rate: f64) -> f64 {
        let (i, o) = self.latency();
        i as f64 + o as f64 * rate
    }
    /// Frames of past audio a prime wants.
    fn prime_frames(&self) -> usize;
}

/// Signalsmith Stretch behind the trait (feature `stretch`).
#[cfg(feature = "stretch")]
pub struct SignalsmithStretcher {
    inner: signalsmith_stretch::Stretch,
    prime: usize,
}

#[cfg(feature = "stretch")]
impl SignalsmithStretcher {
    pub fn new(cfg: &StretchConfig, channels: usize, rate_hz: u32) -> Self {
        let inner = match cfg.preset {
            StretchPreset::Default => {
                signalsmith_stretch::Stretch::preset_default(channels as u32, rate_hz)
            }
            StretchPreset::Cheaper => {
                signalsmith_stretch::Stretch::preset_cheaper(channels as u32, rate_hz)
            }
        };
        let prime = (cfg.prime_ms * f64::from(rate_hz) / 1000.0).round() as usize;
        Self { inner, prime }
    }
}

#[cfg(feature = "stretch")]
impl Stretcher for SignalsmithStretcher {
    fn process(&mut self, input: &[f32], out: &mut [f32]) {
        self.inner.process(input, out);
    }
    fn reset_prime(&mut self, past: &[f32], rate: f64) {
        self.inner.reset();
        self.inner.seek(past, rate);
    }
    fn flush(&mut self, out: &mut [f32]) {
        self.inner.flush(out);
    }
    fn latency(&self) -> (usize, usize) {
        (self.inner.input_latency(), self.inner.output_latency())
    }
    fn prime_frames(&self) -> usize {
        self.prime
    }
}

/// Whether this build has the Signalsmith implementation.
pub const fn signalsmith_built() -> bool {
    cfg!(feature = "stretch")
}

/// The stretcher for this build and config (Signalsmith when built and not forced off, else
/// WSOLA).
pub fn make_stretcher(cfg: &StretchConfig, channels: usize, rate_hz: u32) -> Box<dyn Stretcher> {
    #[cfg(feature = "stretch")]
    {
        if !cfg.force_wsola {
            return Box::new(SignalsmithStretcher::new(cfg, channels, rate_hz));
        }
    }
    Box::new(WsolaStretcher::new(cfg, channels, rate_hz))
}

/// The stretch voice: owns a [`Stretcher`], the fractional input accumulator and the position
/// bookkeeping. Rate comes from the caller per quantum (the stage's ramp).
pub struct StretchVoice {
    st: Box<dyn Stretcher>,
    ch: usize,
    block_frames: usize,
    /// Next source frame to feed.
    in_pos: i64,
    /// Fractional input accumulator (the WARP_05 rule: never round per call).
    acc: f64,
    /// Source position of the audio just written (`consumed - lag`), monotone.
    pos: f64,
    /// Heard position to (re)prime at, waiting for the data.
    pending: Option<f64>,
    past: Vec<f32>,
    inbuf: Vec<f32>,
}

impl StretchVoice {
    pub fn new(cfg: &StretchConfig, channels: usize, rate_hz: u32) -> Self {
        let ch = channels.max(1);
        let st = make_stretcher(cfg, ch, rate_hz);
        let prime = st.prime_frames();
        let block = cfg.block_frames.max(16);
        Self {
            st,
            ch,
            block_frames: block,
            in_pos: 0,
            acc: 0.0,
            pos: 0.0,
            pending: None,
            past: vec![0.0; prime * ch],
            inbuf: vec![0.0; (block * 4 + 8) * ch],
        }
    }

    /// Frames per quantum.
    pub fn block_frames(&self) -> usize {
        self.block_frames
    }

    /// Source position of the audio just written.
    pub fn pos(&self) -> f64 {
        self.pos
    }

    /// Current lag in input frames at `rate`.
    pub fn lag(&self, rate: f64) -> f64 {
        self.st.lag_frames(rate)
    }

    pub fn stretcher(&mut self) -> &mut dyn Stretcher {
        self.st.as_mut()
    }

    /// Ask for a (re)prime so the next output is the audio at `heard`; performed by the next
    /// `render` once the window holds the data (`NeedData` until then).
    pub fn request_prime(&mut self, heard: f64) {
        self.pending = Some(heard.max(0.0));
        self.pos = heard.max(0.0);
    }

    pub fn priming_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Render up to one quantum at `rate` (clamped to 0.1..4.0).
    pub fn render(
        &mut self,
        window: &SourceWindow,
        eof: bool,
        rate: f64,
        out: &mut [f32],
    ) -> Quantum {
        let ch = self.ch;
        let mut q = (out.len() / ch).min(self.block_frames);
        if q == 0 {
            return Quantum::NeedData;
        }
        let rate = rate.clamp(STRETCH_MIN_RATE, STRETCH_MAX_RATE);
        let end = window.end_frame() as i64;
        if let Some(h) = self.pending {
            let c = (h + self.st.lag_frames(rate)).round() as i64;
            let first_n = (q as f64 * rate).ceil() as i64 + 1;
            if !eof && end < c + first_n {
                return Quantum::NeedData;
            }
            let prime = self.st.prime_frames();
            window.read_span(c - prime as i64, &mut self.past[..prime * ch]);
            self.st.reset_prime(&self.past[..prime * ch], rate);
            self.in_pos = c;
            self.acc = 0.0;
            self.pos = h;
            self.pending = None;
        }
        if eof {
            let remain = end as f64 - self.pos;
            if remain <= 0.0 {
                return Quantum::End;
            }
            q = q.min(((remain / rate).ceil() as usize).max(1));
        }
        let acc = self.acc + q as f64 * rate;
        let n = acc.floor() as usize;
        if !eof && self.in_pos + n as i64 > end {
            return Quantum::NeedData;
        }
        self.acc = acc - n as f64;
        let n = n.min(self.inbuf.len() / ch);
        window.read_span(self.in_pos, &mut self.inbuf[..n * ch]);
        self.st.process(&self.inbuf[..n * ch], &mut out[..q * ch]);
        self.in_pos += n as i64;
        let heard = self.in_pos as f64 - self.st.lag_frames(rate);
        self.pos = self.pos.max(heard);
        Quantum::Rendered {
            frames: q,
            rate,
            bounced: false,
            stalled: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{
        active_length_frames, click_train, click_train_positions, detect_click_times,
        fft_peak_frequency, scan_numeric, silence_tail, sine,
    };
    use crate::window::{SourceWindow, WindowConfig};

    pub(crate) const HZ: u32 = 44_100;

    pub(crate) fn window_with(x: &[f32], ch: usize) -> SourceWindow {
        let cfg = WindowConfig {
            window_cap_bytes: (x.len() + 64) * 4,
            history_s: 1.0,
            lookahead_s: 1.0,
            min_history_s: 0.0,
            ..WindowConfig::default()
        };
        let mut w = SourceWindow::new(ch, HZ, &cfg);
        w.reset(0);
        assert!(w.push(x, 0));
        w
    }

    /// Render a whole mono signal at a constant rate (eof) through a fresh voice.
    pub(crate) fn render_all(x: &[f32], rate: f64, force_wsola: bool) -> Vec<f32> {
        let cfg = StretchConfig {
            force_wsola,
            ..StretchConfig::default()
        };
        let w = window_with(x, 1);
        let mut v = StretchVoice::new(&cfg, 1, HZ);
        v.request_prime(0.0);
        let mut out = Vec::new();
        let mut blk = vec![0.0f32; 256];
        loop {
            match v.render(&w, true, rate, &mut blk) {
                Quantum::Rendered { frames, .. } => out.extend_from_slice(&blk[..frames]),
                Quantum::End => break,
                Quantum::NeedData => panic!("NeedData at eof"),
            }
            assert!(out.len() < x.len() * 12 + 100_000, "runaway render");
        }
        out
    }

    fn tone_case(rate: f64, wsola: bool, pitch_tol: f64, dur_tol: f64) {
        let tone = sine(1000.0, HZ, (HZ as usize) * 3, 0.5);
        let src = silence_tail(&tone, HZ as usize / 2);
        let out = render_all(&src, rate, wsola);
        let scan = scan_numeric(&out);
        assert!(scan.is_clean(), "{scan:?}");
        let active = active_length_frames(&out, 0.1);
        let expect = tone.len() as f64 / rate;
        let dur = active as f64 / expect;
        assert!(
            (dur - 1.0).abs() < dur_tol,
            "wsola={wsola} rate={rate}: duration ratio {dur}"
        );
        let mid = &out[active / 4..active * 3 / 4];
        let hz = fft_peak_frequency(mid, HZ);
        eprintln!("wsola={wsola} rate={rate} dur_ratio={dur:.4} pitch={hz:.1}");
        assert!(
            (hz / 1000.0 - 1.0).abs() < pitch_tol,
            "wsola={wsola} rate={rate}: pitch {hz}"
        );
    }

    #[test]
    fn wsola_keeps_pitch_and_duration() {
        for r in [0.1, 0.25, 0.5, 0.75, 1.25, 1.5, 2.0] {
            tone_case(r, true, 0.03, 0.04);
        }
        for r in [3.0, 4.0] {
            tone_case(r, true, 0.05, 0.08);
        }
    }

    #[cfg(feature = "stretch")]
    #[test]
    fn signalsmith_keeps_pitch_and_duration() {
        for r in [0.1, 0.25, 0.5, 0.75, 1.25, 1.5, 2.0] {
            tone_case(r, false, 0.01, 0.02);
        }
        for r in [2.0, 3.0, 4.0] {
            tone_case(r, false, 0.02, 0.05);
        }
    }

    /// Click train primed at `h`: the click for source frame `s` is heard at `(s - h) / rate`
    /// output frames (the lag is absorbed by priming ahead), and `pos()` tracks the heard
    /// source position. Sample-count properties only.
    fn click_alignment(rate: f64, wsola: bool, tol_frames: f64) {
        let period_ms = 120;
        let src = click_train(HZ, HZ as usize * 6, period_ms, 0.9);
        let positions = click_train_positions(HZ, src.len(), period_ms);
        let cfg = StretchConfig {
            force_wsola: wsola,
            ..StretchConfig::default()
        };
        let w = window_with(&src, 1);
        let h = 20_000.0;
        let mut v = StretchVoice::new(&cfg, 1, HZ);
        v.request_prime(h);
        let mut out = Vec::new();
        let mut blk = vec![0.0f32; 256];
        for _ in 0..600 {
            if let Quantum::Rendered { frames, .. } = v.render(&w, false, rate, &mut blk) {
                out.extend_from_slice(&blk[..frames]);
            } else {
                break;
            }
        }
        let heard_src = h + out.len() as f64 * rate;
        assert!(
            (v.pos() - heard_src).abs() < tol_frames * rate.max(1.0),
            "wsola={wsola} rate={rate}: pos {} vs {}",
            v.pos(),
            heard_src
        );
        let found = detect_click_times(&out, 0.3, 1000);
        let mut matched = 0;
        for f in found.iter().skip(1).take(20) {
            let want = positions
                .iter()
                .map(|&p| (p as f64 - h) / rate)
                .min_by(|a, b| (a - *f as f64).abs().total_cmp(&(b - *f as f64).abs()))
                .unwrap();
            let err = *f as f64 - want;
            eprintln!("wsola={wsola} rate={rate} click at {f} want {want:.0} err {err:.0}");
            assert!(err.abs() < tol_frames, "click error {err} frames");
            matched += 1;
        }
        assert!(matched >= 3, "found {matched} clicks");
    }

    #[cfg(feature = "stretch")]
    #[test]
    fn signalsmith_clicks_map_to_the_requested_position() {
        click_alignment(1.0, false, 132.0);
        click_alignment(0.5, false, 132.0);
    }

    #[test]
    fn wsola_clicks_map_to_the_requested_position() {
        click_alignment(1.0, true, 132.0);
    }

    /// Chirp mapped through the stretcher: the frequency at output time `t` is the source
    /// frequency at source time `t x rate` (sample-count property, no timing).
    fn chirp_case(rate: f64, wsola: bool, tol: f64) {
        let (f0, f1, secs) = (800.0f64, 1250.0f64, 4.0f64);
        let src = crate::testsupport::log_chirp(f0, f1, HZ, (HZ as f64 * secs) as usize, 0.5);
        let out = render_all(&src, rate, wsola);
        let win = 4096usize;
        let out_secs = out.len() as f64 / f64::from(HZ);
        let mut checked = 0;
        let mut t = 0.25 * out_secs;
        while t < 0.75 * out_secs {
            let c = (t * f64::from(HZ)) as usize;
            let seg = &out[c - win / 2..c + win / 2];
            let measured = fft_peak_frequency(seg, HZ);
            let want = f0 * (f1 / f0).powf((t * rate) / secs);
            let err = (measured / want - 1.0).abs();
            assert!(
                err < tol,
                "wsola={wsola} rate={rate} t={t:.2}: {measured} vs {want} ({err})"
            );
            checked += 1;
            t += 0.2;
        }
        assert!(checked >= 3);
    }

    #[cfg(feature = "stretch")]
    #[test]
    fn signalsmith_chirp_maps_frequency_over_time() {
        chirp_case(0.5, false, 0.02);
        chirp_case(2.0, false, 0.02);
    }

    #[test]
    fn wsola_chirp_maps_frequency_over_time() {
        chirp_case(0.5, true, 0.03);
        chirp_case(2.0, true, 0.04);
    }
}
