//! Reference voices for the eval app: windowed-sinc varispeed, boxcar+cubic for |rate| > 2,
//! exact bypass, and a Signalsmith Stretch wrapper. These sound representative of the WARP design but
//! are NOT the production code (the production voices live in `plippa-audio`).

use signalsmith_stretch::Stretch;
use std::f64::consts::PI;

/// Output quantum of every voice (matches the production audio thread).
pub const QUANTUM: usize = 256;

/// Interleaved f32 audio with an absolute frame index: `data[0]` is frame `start`.
#[derive(Clone, Debug)]
pub struct PcmBuf {
    pub rate: u32,
    pub ch: usize,
    pub start: i64,
    pub data: Vec<f32>,
}

impl PcmBuf {
    pub fn new(rate: u32, ch: usize, start: i64, data: Vec<f32>) -> Self {
        debug_assert!(ch > 0 && data.len() / ch * ch == data.len());
        PcmBuf {
            rate,
            ch,
            start,
            data,
        }
    }

    pub fn frames(&self) -> usize {
        self.data.len() / self.ch
    }

    /// One past the last absolute frame.
    pub fn end(&self) -> i64 {
        self.start + self.frames() as i64
    }

    pub fn seconds(&self) -> f64 {
        self.frames() as f64 / self.rate as f64
    }

    /// Sample at an absolute frame; zero outside the buffer.
    #[inline]
    pub fn at(&self, frame: i64, c: usize) -> f32 {
        let i = frame - self.start;
        if i < 0 || i as usize >= self.frames() {
            0.0
        } else {
            self.data[i as usize * self.ch + c]
        }
    }

    /// Copy `n` frames starting at absolute `frame` into `out` (zeros outside).
    pub fn copy_frames(&self, frame: i64, n: usize, out: &mut Vec<f32>) {
        out.clear();
        out.reserve(n * self.ch);
        for k in 0..n as i64 {
            for c in 0..self.ch {
                out.push(self.at(frame + k, c));
            }
        }
    }
}

// ---------------------------------------------------------------- windowed-sinc varispeed

pub const SINC_TAPS: usize = 64;
pub const SINC_PHASES: usize = 256;
const SINC_TABLES: usize = 9;
/// Taps before the centre sample (taps cover `base - 31 ..= base + 32`).
const TAPS_BEFORE: i64 = (SINC_TAPS / 2 - 1) as i64;
/// `|rate|` above this uses the lo-fi boxcar+cubic reader (scan speeds).
pub const SINC_MAX_RATE: f64 = 2.0;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    loop {
        term *= (x / (2.0 * k)) * (x / (2.0 * k));
        sum += term;
        if term < 1e-12 * sum {
            return sum;
        }
        k += 1.0;
    }
}

fn kaiser(x: f64, half: f64, beta: f64) -> f64 {
    let r = x / half;
    if r.abs() >= 1.0 {
        0.0
    } else {
        bessel_i0(beta * (1.0 - r * r).sqrt()) / bessel_i0(beta)
    }
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

/// Nine kernels (cutoff `0.97 / max(1, |rate|)` quantised up in 1/8 steps of |rate|), each with
/// `SINC_PHASES + 1` rows of `SINC_TAPS`. The phase-0 row of the full-band table is an exact delta.
pub struct SincTables {
    t: Vec<Vec<f32>>,
}

impl SincTables {
    pub fn new() -> Self {
        let mut t = Vec::with_capacity(SINC_TABLES);
        for i in 0..SINC_TABLES {
            let bound = 1.0 + 0.125 * i as f64;
            let fc = 0.97 / bound;
            let mut tab = vec![0.0f32; (SINC_PHASES + 1) * SINC_TAPS];
            for p in 0..=SINC_PHASES {
                let frac = p as f64 / SINC_PHASES as f64;
                let row = &mut tab[p * SINC_TAPS..(p + 1) * SINC_TAPS];
                let mut sum = 0.0;
                let mut tmp = [0.0f64; SINC_TAPS];
                for (tap, v) in tmp.iter_mut().enumerate() {
                    let x = (tap as i64 - TAPS_BEFORE) as f64 - frac;
                    *v = fc * sinc(fc * x) * kaiser(x, (SINC_TAPS / 2) as f64, 9.0);
                    sum += *v;
                }
                for (tap, v) in tmp.iter().enumerate() {
                    row[tap] = (*v / sum) as f32;
                }
                if i == 0 && p == 0 {
                    row.fill(0.0);
                    row[TAPS_BEFORE as usize] = 1.0;
                }
            }
            t.push(tab);
        }
        SincTables { t }
    }

    fn table_for(&self, rate_abs: f64) -> &[f32] {
        let idx = (((rate_abs.max(1.0) - 1.0) / 0.125).ceil() as usize).min(SINC_TABLES - 1);
        &self.t[idx]
    }
}

impl Default for SincTables {
    fn default() -> Self {
        Self::new()
    }
}

/// Varispeed reader: pitch follows speed. Signed rate (negative = backwards).
pub struct Varispeed {
    tables: SincTables,
    rates: Vec<f64>,
}

impl Varispeed {
    pub fn new() -> Self {
        Varispeed {
            tables: SincTables::new(),
            rates: Vec::new(),
        }
    }

    /// Render `out.len()/ch` frames with the rate ramping linearly `r0 -> r1` over the block.
    /// `pos` is the fractional source frame of the first output frame and advances by the rate.
    pub fn render(&mut self, src: &PcmBuf, pos: &mut f64, r0: f64, r1: f64, out: &mut [f32]) {
        let ch = src.ch;
        let m = out.len() / ch;
        self.rates.clear();
        for i in 0..m {
            self.rates
                .push(r0 + (r1 - r0) * (i as f64 + 0.5) / m as f64);
        }
        for i in 0..m {
            let r = self.rates[i];
            let dst = &mut out[i * ch..(i + 1) * ch];
            if r.abs() > SINC_MAX_RATE {
                boxcar_cubic(src, *pos, r.abs().round().max(1.0) as i64, dst);
            } else {
                self.sinc_frame(src, *pos, r.abs(), dst);
            }
            *pos += r;
        }
    }

    fn sinc_frame(&self, src: &PcmBuf, pos: f64, rate_abs: f64, dst: &mut [f32]) {
        let ch = src.ch;
        let base = pos.floor();
        let ph = (pos - base) * SINC_PHASES as f64;
        let pi = (ph.floor() as usize).min(SINC_PHASES - 1);
        let w = (ph - pi as f64) as f32;
        let tab = self.tables.table_for(rate_abs);
        let a = &tab[pi * SINC_TAPS..(pi + 1) * SINC_TAPS];
        let b = &tab[(pi + 1) * SINC_TAPS..(pi + 2) * SINC_TAPS];
        let base = base as i64;
        let first = base - TAPS_BEFORE;
        let fast = first >= src.start && first + SINC_TAPS as i64 <= src.end();
        for (c, d) in dst.iter_mut().enumerate() {
            let (mut sa, mut sb) = (0.0f32, 0.0f32);
            if fast {
                let off = (first - src.start) as usize * ch + c;
                for tap in 0..SINC_TAPS {
                    let s = src.data[off + tap * ch];
                    sa += s * a[tap];
                    sb += s * b[tap];
                }
            } else {
                for tap in 0..SINC_TAPS {
                    let s = src.at(first + tap as i64, c);
                    sa += s * a[tap];
                    sb += s * b[tap];
                }
            }
            *d = sa + w * (sb - sa);
        }
    }
}

impl Default for Varispeed {
    fn default() -> Self {
        Self::new()
    }
}

fn boxcar_at(src: &PcmBuf, i: i64, n: i64, c: usize) -> f32 {
    let s0 = i - n / 2;
    let mut sum = 0.0f32;
    for k in 0..n {
        sum += src.at(s0 + k, c);
    }
    sum / n as f32
}

fn cubic(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    0.5 * ((2.0 * p1)
        + (-p0 + p2) * t
        + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t * t
        + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t * t * t)
}

/// Intentionally lo-fi scan reader: boxcar average of `n = round(|rate|)` frames, then 4-point cubic.
fn boxcar_cubic(src: &PcmBuf, pos: f64, n: i64, dst: &mut [f32]) {
    let base = pos.floor();
    let t = (pos - base) as f32;
    let b = base as i64;
    for (c, d) in dst.iter_mut().enumerate() {
        *d = cubic(
            boxcar_at(src, b - 1, n, c),
            boxcar_at(src, b, n, c),
            boxcar_at(src, b + 1, n, c),
            boxcar_at(src, b + 2, n, c),
            t,
        );
    }
}

/// Bypass: exact copy at `round(pos)`; leaves `pos` on the integer grid.
pub fn render_bypass(src: &PcmBuf, pos: &mut f64, out: &mut [f32]) {
    let ch = src.ch;
    let m = out.len() / ch;
    let a = pos.round() as i64;
    for i in 0..m {
        for c in 0..ch {
            out[i * ch + c] = src.at(a + i as i64, c);
        }
    }
    *pos = (a + m as i64) as f64;
}

// ---------------------------------------------------------------- stretch voice

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StretchPreset {
    Default,
    Cheaper,
}

impl StretchPreset {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "default" => Ok(StretchPreset::Default),
            "cheaper" => Ok(StretchPreset::Cheaper),
            _ => anyhow::bail!("unknown stretch preset '{s}' (default|cheaper)"),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            StretchPreset::Default => "default",
            StretchPreset::Cheaper => "cheaper",
        }
    }
}

/// Time-stretch voice (constant pitch). Input is consumed at `rate` x realtime, output is realtime.
pub struct StretchVoice {
    st: Stretch,
    ch: usize,
    rate: u32,
    inbuf: Vec<f32>,
}

impl StretchVoice {
    pub fn new(ch: usize, rate: u32, preset: StretchPreset) -> Self {
        let mut st = match preset {
            StretchPreset::Default => Stretch::preset_default(ch as u32, rate),
            StretchPreset::Cheaper => Stretch::preset_cheaper(ch as u32, rate),
        };
        st.set_transpose_factor(1.0, None);
        StretchVoice {
            st,
            ch,
            rate,
            inbuf: Vec::new(),
        }
    }

    /// Reset and prime with the PAST `block + interval` (about 150 ms) frames ending at `pos`.
    pub fn prime(&mut self, src: &PcmBuf, pos: f64, rate: f64) {
        self.st.reset();
        self.st.set_transpose_factor(1.0, None);
        let n = (0.15 * self.rate as f64) as usize;
        let end = pos.round() as i64;
        src.copy_frames(end - n as i64, n, &mut self.inbuf);
        self.st.seek(&self.inbuf, rate.max(0.05));
    }

    /// Render `out.len()/ch` frames consuming `frames x rate` input frames from `pos` (rate > 0).
    pub fn render(&mut self, src: &PcmBuf, pos: &mut f64, rate: f64, out: &mut [f32]) {
        debug_assert!(rate > 0.0);
        let m = out.len() / self.ch;
        let end = *pos + m as f64 * rate;
        let a = pos.round() as i64;
        let n = ((end.round() as i64) - a).max(0) as usize;
        src.copy_frames(a, n, &mut self.inbuf);
        self.st.process(&self.inbuf, out);
        *pos = end;
    }

    /// Frames of input latency plus output latency, as reported by the library.
    pub fn latencies(&self) -> (usize, usize) {
        (self.st.input_latency(), self.st.output_latency())
    }

    /// How far the heard input position trails the consumed one at `rate`
    /// (`input_latency + output_latency x rate`, measured by the test `stretch_heard_position_...`).
    pub fn lag_frames(&self, rate: f64) -> f64 {
        let (i, o) = self.latencies();
        i as f64 + o as f64 * rate
    }
}

// ---------------------------------------------------------------- layers and crossfades

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bypass,
    Vari,
    Stretch,
}

/// One playing voice with its own source position (a crossfade runs two layers at once).
pub struct Layer {
    pub kind: Kind,
    pub pos: f64,
    stretch: Option<StretchVoice>,
}

impl Layer {
    pub fn bypass(pos: f64) -> Layer {
        Layer {
            kind: Kind::Bypass,
            pos: pos.round(),
            stretch: None,
        }
    }

    pub fn vari(pos: f64) -> Layer {
        Layer {
            kind: Kind::Vari,
            pos,
            stretch: None,
        }
    }

    /// Stretch layer whose HEARD position starts at `heard`: the stretcher is primed with the past audio
    /// ending at `heard + lag`, so the first output sample is the audio at `heard` (the eval aligns
    /// voices exactly; hand-over alignment in production is WARP_14's job).
    pub fn stretch(heard: f64, mut voice: StretchVoice, src: &PcmBuf, rate: f64) -> Layer {
        let pos = heard + voice.lag_frames(rate);
        voice.prime(src, pos, rate);
        Layer {
            kind: Kind::Stretch,
            pos,
            stretch: Some(voice),
        }
    }

    /// Position of the audio being heard (stretch: consumed position minus the stretcher latency).
    pub fn heard(&self, rate: f64) -> f64 {
        match &self.stretch {
            Some(sv) => self.pos - sv.lag_frames(rate),
            None => self.pos,
        }
    }

    /// Give the stretcher back (so it can be reused for the next seek).
    pub fn take_stretch(&mut self) -> Option<StretchVoice> {
        self.stretch.take()
    }

    /// Render one block; rate ramps `r0 -> r1` (stretch uses the mean; bypass ignores the rate).
    pub fn render(
        &mut self,
        vari: &mut Varispeed,
        src: &PcmBuf,
        r0: f64,
        r1: f64,
        out: &mut [f32],
    ) {
        match self.kind {
            Kind::Bypass => render_bypass(src, &mut self.pos, out),
            Kind::Vari => vari.render(src, &mut self.pos, r0, r1, out),
            Kind::Stretch => {
                let r = (0.5 * (r0 + r1)).max(0.05);
                self.stretch
                    .as_mut()
                    .expect("stretch layer without a stretcher")
                    .render(src, &mut self.pos, r, out)
            }
        }
    }
}

/// Equal-power gains `(fade_out, fade_in)` for frame `k` of a crossfade of `total` frames.
pub fn xfade_gains(k: usize, total: usize) -> (f32, f32) {
    let t = (k as f64 + 0.5) / total.max(1) as f64;
    let t = t.clamp(0.0, 1.0);
    (((PI / 2.0) * t).cos() as f32, ((PI / 2.0) * t).sin() as f32)
}

/// Mix block `b` into block `a` (result in `a`) for crossfade frames `k0 ..` of `total`.
/// Frames at or beyond `total` are `b` only.
pub fn xfade_blocks(a: &mut [f32], b: &[f32], ch: usize, k0: usize, total: usize) {
    let m = a.len() / ch;
    for i in 0..m {
        let (go, gi) = xfade_gains(k0 + i, total);
        let (go, gi) = if k0 + i >= total {
            (0.0, 1.0)
        } else {
            (go, gi)
        };
        for c in 0..ch {
            a[i * ch + c] = a[i * ch + c] * go + b[i * ch + c] * gi;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::{generate, goertzel, to_channels, Kind as SigKind};

    fn tone_buf(seconds: f64, rate: u32, ch: usize) -> PcmBuf {
        let mono = generate(SigKind::Tone, seconds, rate);
        PcmBuf::new(rate, ch, 0, to_channels(&mono, ch))
    }

    /// Frequency of the strongest Goertzel bin of channel 0 in `lo..=hi` Hz.
    fn peak(out: &[f32], ch: usize, rate: f64, lo: f64, hi: f64, step: f64) -> f64 {
        let mono: Vec<f32> = out.iter().step_by(ch).cloned().collect();
        let (mut best, mut f) = ((0.0, lo), lo);
        while f <= hi {
            let m = goertzel(&mono, rate, f);
            if m > best.0 {
                best = (m, f);
            }
            f += step;
        }
        best.1
    }

    fn render_all(
        v: &mut Varispeed,
        src: &PcmBuf,
        pos0: f64,
        rate: f64,
        frames: usize,
    ) -> (Vec<f32>, f64) {
        let mut pos = pos0;
        let mut out = vec![0.0f32; frames * src.ch];
        for chunk in out.chunks_mut(QUANTUM * src.ch) {
            v.render(src, &mut pos, rate, rate, chunk);
        }
        (out, pos)
    }

    #[test]
    fn sinc_tables_are_normalised_and_phase0_is_a_delta() {
        let t = SincTables::new();
        for tab in &t.t {
            for p in 0..=SINC_PHASES {
                let s: f32 = tab[p * SINC_TAPS..(p + 1) * SINC_TAPS].iter().sum();
                assert!((s - 1.0).abs() < 1e-4, "row {p} sums to {s}");
            }
        }
        let row0 = &t.t[0][..SINC_TAPS];
        for (i, v) in row0.iter().enumerate() {
            assert_eq!(*v, if i == TAPS_BEFORE as usize { 1.0 } else { 0.0 });
        }
    }

    #[test]
    fn varispeed_at_unity_is_bit_exact_on_the_integer_grid() {
        let src = tone_buf(1.0, 44100, 2);
        let mut v = Varispeed::new();
        let (out, pos) = render_all(&mut v, &src, 1000.0, 1.0, 4096);
        assert_eq!(pos, 1000.0 + 4096.0);
        for (i, o) in out.iter().enumerate().take(4096 * 2) {
            assert_eq!(*o, src.data[1000 * 2 + i], "sample {i}");
        }
    }

    #[test]
    fn varispeed_pitch_follows_rate() {
        let src = tone_buf(4.0, 44100, 1);
        let mut v = Varispeed::new();
        for (rate, expect) in [(0.5, 500.0), (1.5, 1500.0), (2.0, 2000.0)] {
            let (out, _) = render_all(&mut v, &src, 4000.0, rate, 44100);
            let f = peak(&out, 1, 44100.0, expect - 30.0, expect + 30.0, 0.25);
            assert!(
                (f - expect).abs() / expect <= 0.005,
                "rate {rate}: peak {f}"
            );
            assert!(out.iter().all(|s| s.is_finite()));
        }
    }

    #[test]
    fn varispeed_reverse_runs_backwards_at_the_same_pitch() {
        let src = tone_buf(4.0, 44100, 1);
        let mut v = Varispeed::new();
        let (out, pos) = render_all(&mut v, &src, 100_000.0, -1.0, 44100);
        assert!((pos - (100_000.0 - 44100.0)).abs() < 1e-6);
        let f = peak(&out, 1, 44100.0, 970.0, 1030.0, 0.25);
        assert!((f - 1000.0).abs() <= 1.5, "reverse tone at {f}");
        let (out2, _) = render_all(&mut v, &src, 100_000.0, -2.0, 44100);
        let f2 = peak(&out2, 1, 44100.0, 1900.0, 2100.0, 0.5);
        assert!((f2 - 2000.0).abs() <= 4.0, "reverse 2x at {f2}");
    }

    #[test]
    fn boxcar_cubic_scan_rates_scale_pitch_and_stay_finite() {
        let src = tone_buf(12.0, 44100, 2);
        let mut v = Varispeed::new();
        for rate in [4.0f64, 8.0] {
            let (out, pos) = render_all(&mut v, &src, 1000.0, rate, 44100);
            assert!((pos - (1000.0 + rate * 44100.0)).abs() < 1e-6);
            assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
            // 1 kHz tone at 4x lands at 4 kHz (inside Nyquist); at 8x it would be 8 kHz
            let expect = 1000.0 * rate;
            let f = peak(&out, 2, 44100.0, expect - 100.0, expect + 100.0, 1.0);
            assert!((f - expect).abs() / expect <= 0.01, "rate {rate}: {f}");
        }
    }

    #[test]
    fn varispeed_rate_ramp_has_no_discontinuity() {
        let src = tone_buf(4.0, 44100, 1);
        let mut v = Varispeed::new();
        let mut pos = 2000.0;
        let mut out = Vec::new();
        let mut r = 1.0;
        for _ in 0..200 {
            let mut blk = vec![0.0f32; QUANTUM];
            let r1 = (r + 0.02f64).min(2.0);
            v.render(&src, &mut pos, r, r1, &mut blk);
            out.extend_from_slice(&blk);
            r = r1;
        }
        // the first difference of a 1-2 kHz tone at 44.1 kHz is bounded by ~2*pi*2000/44100*0.5
        let max_step = out
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(max_step < 0.2, "max sample step {max_step}");
    }

    #[test]
    fn bypass_is_exact_and_rounds_the_position() {
        let src = tone_buf(1.0, 44100, 2);
        let mut pos = 500.4;
        let mut out = vec![0.0f32; 512 * 2];
        render_bypass(&src, &mut pos, &mut out);
        assert_eq!(pos, 500.0 + 512.0);
        assert_eq!(&out[..], &src.data[500 * 2..500 * 2 + 1024]);
    }

    #[test]
    fn pcmbuf_is_zero_outside_and_offsets_work() {
        let b = PcmBuf::new(48000, 2, 10, vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(b.frames(), 2);
        assert_eq!(b.end(), 12);
        assert_eq!(b.at(10, 1), 2.0);
        assert_eq!(b.at(11, 0), 3.0);
        assert_eq!(b.at(9, 0), 0.0);
        assert_eq!(b.at(12, 0), 0.0);
        let mut v = Vec::new();
        b.copy_frames(9, 4, &mut v);
        assert_eq!(v, vec![0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 0.0, 0.0]);
    }

    #[test]
    fn equal_power_crossfade_keeps_unit_power() {
        for total in [1usize, 64, 2646] {
            for k in 0..total {
                let (o, i) = xfade_gains(k, total);
                assert!((o * o + i * i - 1.0).abs() < 1e-5);
            }
        }
        let mut a = vec![1.0f32; 8];
        let b = vec![0.5f32; 8];
        xfade_blocks(&mut a, &b, 2, 2, 4);
        // frames 0,1 of the block are crossfade frames 2,3; frames 2,3 are past the end (b only)
        assert!(a[0] > 0.5 && a[0] < 1.0);
        assert_eq!(a[4], 0.5);
        assert_eq!(a[6], 0.5);
    }

    #[test]
    fn stretch_keeps_pitch_and_advances_input_at_the_rate() {
        let src = tone_buf(12.0, 44100, 2);
        for rate in [0.5f64, 1.0, 2.0] {
            let mut sv = StretchVoice::new(2, 44100, StretchPreset::Default);
            let mut pos = 20_000.0;
            sv.prime(&src, pos, rate);
            let mut out = vec![0.0f32; 2 * 44100];
            for chunk in out.chunks_mut(QUANTUM * 2) {
                sv.render(&src, &mut pos, rate, chunk);
            }
            assert!(
                (pos - (20_000.0 + 44100.0 * rate)).abs() < 1e-6,
                "input consumed at the rate"
            );
            assert!(out.iter().all(|s| s.is_finite()));
            // skip the first 0.4 s (latency), then the pitch must still be 1 kHz
            let tail = &out[2 * 17_640..];
            let f = peak(tail, 2, 44100.0, 950.0, 1050.0, 0.5);
            assert!((f - 1000.0).abs() <= 10.0, "rate {rate}: stretch pitch {f}");
        }
    }

    /// The heard (output-aligned) input position trails the consumed position by
    /// `input_latency + output_latency x rate` frames. This checks the formula on an irregular click
    /// train by envelope correlation (a sample-count property of the rendered buffer, not a timing).
    #[test]
    fn stretch_heard_position_lags_by_the_reported_latencies() {
        let n = 44100 * 14;
        let mut mono = vec![0.0f32; n];
        let mut rng = crate::gen::Lcg::new(77);
        let mut i = 20000usize;
        while i + 100 < n {
            for k in 0..60 {
                mono[i + k] = (rng.next_f64_signed() * 0.8) as f32;
            }
            i += 6000 + (rng.next_u64() % 30000) as usize;
        }
        let src = PcmBuf::new(44100, 1, 0, mono.clone());
        let env = |x: &[f32]| -> Vec<f32> {
            let w = 441usize;
            let mut e = vec![0.0f32; x.len()];
            let mut acc = 0.0f32;
            for i in 0..x.len() {
                acc += x[i].abs();
                if i >= w {
                    acc -= x[i - w].abs();
                }
                e[i] = acc;
            }
            e
        };
        let src_env = env(&mono);
        for rate in [1.0f64, 2.0] {
            let mut sv = StretchVoice::new(1, 44100, StretchPreset::Default);
            let p = 44100.0 * 2.0;
            let mut pos = p;
            sv.prime(&src, pos, rate);
            let frames = (44100.0 * 5.0) as usize;
            let mut out = vec![0.0f32; frames];
            for chunk in out.chunks_mut(QUANTUM) {
                sv.render(&src, &mut pos, rate, chunk);
            }
            let oe = env(&out);
            let mut best = (f64::MIN, 0i64);
            for lag in (-14000i64..=2000).step_by(100) {
                let mut c = 0.0f64;
                for i in (441..frames).step_by(11) {
                    let sp = (p + i as f64 * rate) as i64 + lag;
                    if sp >= 0 && (sp as usize) < src_env.len() {
                        c += oe[i] as f64 * src_env[sp as usize] as f64;
                    }
                }
                if c > best.0 {
                    best = (c, lag);
                }
            }
            let expect = -sv.lag_frames(rate);
            assert!(
                (best.1 as f64 - expect).abs() <= 1000.0,
                "rate {rate}: measured offset {} vs {expect}",
                best.1
            );
        }
    }

    #[test]
    fn stretch_cheaper_preset_also_runs() {
        let src = tone_buf(4.0, 48000, 2);
        let mut sv = StretchVoice::new(2, 48000, StretchPreset::Cheaper);
        let mut pos = 10_000.0;
        sv.prime(&src, pos, 1.5);
        let mut out = vec![0.0f32; QUANTUM * 2];
        for _ in 0..50 {
            sv.render(&src, &mut pos, 1.5, &mut out);
            assert!(out.iter().all(|s| s.is_finite()));
        }
        assert!(StretchPreset::parse("nope").is_err());
        assert_eq!(StretchPreset::parse("cheaper").unwrap().name(), "cheaper");
    }
}
