//! Shared DSP test toolkit (WARP_03). Compiled for `cfg(test)` and `--features testsupport`.
//!
//! Every later WARP DSP package (varispeed, SourceWindow, stretcher, reverse, scan, seek core)
//! tests its voices offline through this one module. **Logic and numeric correctness only**:
//! nothing here measures time, CPU or latency, and no helper needs an audio device.
//!
//! * Generators (no binary fixtures): [`sine`], [`log_chirp`], [`click_train`], [`pink_noise`],
//!   [`stereo_with_delay`], [`silence_tail`], [`swept_amplitude_sine`], WAV writers.
//! * Analysis: [`fft_peak_frequency`], [`stopband_level_db`], [`alias_level_db`],
//!   [`click_metric`], [`highpass_energy_around`], [`detect_click_times`],
//!   [`discontinuity_positions`], [`scan_numeric`], [`first_bit_difference`],
//!   equal-power crossfade checks, [`measured_pitch_ratio`], [`measured_duration_ratio`].
//! * [`RenderVoice`] + [`render_chunked`]: one harness for offline voice tests.
//! * [`CountingSource`]: instrumented Symphonia `MediaSource` (bytes read, seeks).
//!
//! All analysis helpers take **mono** `&[f32]`; use [`channel`] to split interleaved data.
//! Real music files come from `assets/testMedia`.

use std::f64::consts::PI;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use realfft::RealFftPlanner;
use symphonia::core::io::MediaSource;

/// Test tone frequencies used across packages.
pub const TONE_LOW_HZ: f64 = 440.0;
pub const TONE_MID_HZ: f64 = 1000.0;
pub const TONE_HIGH_HZ: f64 = 19_000.0;

/// FFT length of the spectral helpers (Hann window).
pub const FFT_LEN: usize = 16_384;

/// Deterministic pseudo-random generator (LCG) so tests never depend on `rand`.
#[derive(Debug, Clone)]
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(
            seed.wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407),
        )
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mut z = self.0;
        z ^= z >> 33;
        z = z.wrapping_mul(0xff51_afd7_ed55_8ccd);
        z ^ (z >> 33)
    }

    /// Uniform in `[0, bound)`.
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            (self.next_u64() >> 8) % bound
        }
    }

    /// Uniform in `[-1, 1)`.
    pub fn unit(&mut self) -> f32 {
        ((self.next_u64() >> 11) as f64 / (1u64 << 52) as f64 - 1.0) as f32
    }
}

// ------------------------------------------------------------------------------------------
// Generators
// ------------------------------------------------------------------------------------------

/// Mono sine of `freq_hz` and peak `amp`.
pub fn sine(freq_hz: f64, rate: u32, frames: usize, amp: f32) -> Vec<f32> {
    (0..frames)
        .map(|n| (amp as f64 * (2.0 * PI * freq_hz * n as f64 / rate as f64).sin()) as f32)
        .collect()
}

/// Logarithmic chirp from `f0` to `f1` Hz over `frames` frames.
pub fn log_chirp(f0: f64, f1: f64, rate: u32, frames: usize, amp: f32) -> Vec<f32> {
    let t_total = frames as f64 / rate as f64;
    let k = (f1 / f0).ln();
    (0..frames)
        .map(|n| {
            let t = n as f64 / rate as f64;
            let phase = 2.0 * PI * f0 * t_total / k * ((k * t / t_total).exp() - 1.0);
            (amp as f64 * phase.sin()) as f32
        })
        .collect()
}

/// Single-frame impulses of height `amp` every `period_ms`, first at one period (not at 0,
/// so start-up transients do not alias with the first click). Frame `k * period_frames`.
pub fn click_train(rate: u32, frames: usize, period_ms: u32, amp: f32) -> Vec<f32> {
    let period = (u64::from(rate) * u64::from(period_ms) / 1000).max(1) as usize;
    let mut v = vec![0.0f32; frames];
    let mut at = period;
    while at < frames {
        v[at] = amp;
        at += period;
    }
    v
}

/// Frame indices of the impulses [`click_train`] writes.
pub fn click_train_positions(rate: u32, frames: usize, period_ms: u32) -> Vec<usize> {
    let period = (u64::from(rate) * u64::from(period_ms) / 1000).max(1) as usize;
    (1..)
        .map(|k| k * period)
        .take_while(|&p| p < frames)
        .collect()
}

/// Pink-ish noise (Paul Kellet's filter on white noise), peak normalised to `amp`.
pub fn pink_noise(frames: usize, seed: u64, amp: f32) -> Vec<f32> {
    let mut rng = Lcg::new(seed);
    let (mut b0, mut b1, mut b2, mut b3, mut b4, mut b5, mut b6) =
        (0f32, 0f32, 0f32, 0f32, 0f32, 0f32, 0f32);
    let mut v: Vec<f32> = (0..frames)
        .map(|_| {
            let w = rng.unit();
            b0 = 0.99886 * b0 + w * 0.0555179;
            b1 = 0.99332 * b1 + w * 0.0750759;
            b2 = 0.969 * b2 + w * 0.153_852;
            b3 = 0.86650 * b3 + w * 0.3104856;
            b4 = 0.55000 * b4 + w * 0.5329522;
            b5 = -0.7616 * b5 - w * 0.0168980;
            let out = b0 + b1 + b2 + b3 + b4 + b5 + b6 + w * 0.5362;
            b6 = w * 0.115926;
            out
        })
        .collect();
    let peak = v.iter().fold(0f32, |m, s| m.max(s.abs())).max(1e-9);
    for s in &mut v {
        *s *= amp / peak;
    }
    v
}

/// Interleaved stereo: left = `mono`, right = `mono` delayed by `delay_frames` (zeros first).
/// A negative `delay_frames` delays the left channel instead.
pub fn stereo_with_delay(mono: &[f32], delay_frames: i32) -> Vec<f32> {
    let n = mono.len();
    let d = delay_frames.unsigned_abs() as usize;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let delayed = if i >= d { mono[i - d] } else { 0.0 };
        if delay_frames >= 0 {
            out.push(mono[i]);
            out.push(delayed);
        } else {
            out.push(delayed);
            out.push(mono[i]);
        }
    }
    out
}

/// `sig` followed by `tail_frames` of exact silence.
pub fn silence_tail(sig: &[f32], tail_frames: usize) -> Vec<f32> {
    let mut v = sig.to_vec();
    v.resize(sig.len() + tail_frames, 0.0);
    v
}

/// Sine whose amplitude ramps linearly from `amp_from` to `amp_to`.
pub fn swept_amplitude_sine(
    freq_hz: f64,
    rate: u32,
    frames: usize,
    amp_from: f32,
    amp_to: f32,
) -> Vec<f32> {
    (0..frames)
        .map(|n| {
            let a = amp_from as f64
                + (amp_to as f64 - amp_from as f64) * n as f64 / frames.max(1) as f64;
            (a * (2.0 * PI * freq_hz * n as f64 / rate as f64).sin()) as f32
        })
        .collect()
}

/// Interleave two equal-length channels.
pub fn interleave_stereo(l: &[f32], r: &[f32]) -> Vec<f32> {
    assert_eq!(l.len(), r.len());
    l.iter().zip(r).flat_map(|(a, b)| [*a, *b]).collect()
}

/// Channel `idx` of interleaved `x` with `channels` channels.
pub fn channel(x: &[f32], channels: usize, idx: usize) -> Vec<f32> {
    x.iter().skip(idx).step_by(channels).copied().collect()
}

/// PCM16 WAV bytes; `sample(frame, channel)` yields each sample.
pub fn pcm16_wav_bytes(
    rate: u32,
    channels: u16,
    frames: usize,
    mut sample: impl FnMut(usize, u16) -> i16,
) -> Vec<u8> {
    let data_bytes = frames * channels as usize * 2;
    let mut out = Vec::with_capacity(44 + data_bytes);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for f in 0..frames {
        for c in 0..channels {
            out.extend_from_slice(&sample(f, c).to_le_bytes());
        }
    }
    out
}

/// Write a PCM16 WAV file (see [`pcm16_wav_bytes`]), creating parent directories.
pub fn write_pcm16_wav(
    path: &Path,
    rate: u32,
    channels: u16,
    frames: usize,
    sample: impl FnMut(usize, u16) -> i16,
) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, pcm16_wav_bytes(rate, channels, frames, sample))
}

/// "Frame index" WAV sample: the frame number modulo 65536 as an `i16` bit pattern, so a
/// decoded sample identifies the source frame it came from (files up to 65536 frames are
/// unambiguous). Decoded value is `(frame as u16 as i16) as f32 / 32768`.
pub fn frame_index_sample(frame: usize) -> i16 {
    (frame % 65_536) as u16 as i16
}

/// Inverse of [`frame_index_sample`] for a decoded f32 sample (frame modulo 65536).
pub fn frame_index_from_f32(s: f32) -> usize {
    ((s * 32768.0).round() as i32 as i16 as u16) as usize
}

// ------------------------------------------------------------------------------------------
// Analysis
// ------------------------------------------------------------------------------------------

/// Hann-windowed magnitude spectrum (`FFT_LEN / 2 + 1` bins) of the centre `FFT_LEN` frames
/// of `x` (zero padded when shorter).
pub fn fft_magnitude_spectrum(x: &[f32]) -> Vec<f32> {
    let n = FFT_LEN;
    let mut buf = vec![0f32; n];
    let take = x.len().min(n);
    let start = (x.len() - take) / 2;
    for i in 0..take {
        let w = 0.5 - 0.5 * (2.0 * PI * i as f64 / (n - 1) as f64).cos();
        buf[i] = x[start + i] * w as f32;
    }
    let mut planner = RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(n);
    let mut spec = fft.make_output_vec();
    fft.process(&mut buf, &mut spec).expect("fft");
    spec.iter().map(|c| c.norm()).collect()
}

/// Frequency (Hz) of the strongest spectral peak: Hann 16384 + parabolic interpolation on
/// the log magnitude. Within 0.1 % for a clean tone above ~100 Hz.
pub fn fft_peak_frequency(x: &[f32], rate: u32) -> f64 {
    let mag = fft_magnitude_spectrum(x);
    let (peak, _) =
        mag.iter().enumerate().skip(2).fold(
            (2usize, 0f32),
            |(bi, bm), (i, &m)| if m > bm { (i, m) } else { (bi, bm) },
        );
    let bin = if peak > 0 && peak + 1 < mag.len() {
        let l = (mag[peak - 1].max(1e-20) as f64).ln();
        let c = (mag[peak].max(1e-20) as f64).ln();
        let r = (mag[peak + 1].max(1e-20) as f64).ln();
        let denom = l - 2.0 * c + r;
        let delta = if denom.abs() < 1e-12 {
            0.0
        } else {
            0.5 * (l - r) / denom
        };
        peak as f64 + delta
    } else {
        peak as f64
    };
    bin * rate as f64 / FFT_LEN as f64
}

/// Level in dB of the energy above `above_hz` relative to the total energy (stopband /
/// image level for a filter or resampler test). `-200` when the stopband is empty.
pub fn stopband_level_db(x: &[f32], rate: u32, above_hz: f64) -> f64 {
    let mag = fft_magnitude_spectrum(x);
    let cut = (above_hz * FFT_LEN as f64 / rate as f64).ceil() as usize;
    let total: f64 = mag.iter().map(|m| (*m as f64).powi(2)).sum();
    let stop: f64 = mag.iter().skip(cut).map(|m| (*m as f64).powi(2)).sum();
    if stop <= 0.0 || total <= 0.0 {
        return -200.0;
    }
    10.0 * (stop / total).log10()
}

/// Largest spectral line outside `+-guard_bins` of `fundamental_hz`, in dB relative to the
/// fundamental's peak (alias / spur level of a pure tone through a resampler).
pub fn alias_level_db(x: &[f32], rate: u32, fundamental_hz: f64, guard_bins: usize) -> f64 {
    let mag = fft_magnitude_spectrum(x);
    let centre = (fundamental_hz * FFT_LEN as f64 / rate as f64).round() as usize;
    let lo = centre.saturating_sub(guard_bins);
    let hi = centre + guard_bins;
    let peak = mag[lo.min(mag.len() - 1)..=hi.min(mag.len() - 1)]
        .iter()
        .fold(0f32, |m, v| m.max(*v)) as f64;
    let spur = mag
        .iter()
        .enumerate()
        .filter(|(i, _)| *i < lo || *i > hi)
        .fold(0f32, |m, (_, v)| m.max(*v)) as f64;
    if peak <= 0.0 {
        return 0.0;
    }
    20.0 * (spur.max(1e-20) / peak).log10()
}

/// Largest second difference a *smooth* band-limited signal of peak `amp` and highest
/// frequency `max_freq_hz` can produce: `4 amp sin^2(pi f / fs)` (a sine of that frequency).
pub fn smooth_bound(amp: f64, max_freq_hz: f64, rate: u32) -> f64 {
    let s = (PI * max_freq_hz / rate as f64).sin();
    4.0 * amp * s * s
}

/// Click metric: `max |x[n] - 2 x[n-1] + x[n-2]|` divided by `bound` (see [`smooth_bound`]).
/// A clean signal stays at or below ~1; a splice, a step or an impulse is far above.
pub fn click_metric(x: &[f32], bound: f64) -> f64 {
    let m = x
        .windows(3)
        .map(|w| (w[2] as f64 - 2.0 * w[1] as f64 + w[0] as f64).abs())
        .fold(0.0, f64::max);
    m / bound.max(1e-12)
}

/// Frame indices (of the newest sample of each window) where the second difference exceeds
/// `threshold`, merged when closer than 4 frames.
pub fn discontinuity_positions(x: &[f32], threshold: f64) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for (i, w) in x.windows(3).enumerate() {
        let d = (w[2] as f64 - 2.0 * w[1] as f64 + w[0] as f64).abs();
        if d > threshold {
            let at = i + 2;
            if out.last().is_none_or(|&l| at - l >= 4) {
                out.push(at);
            }
        }
    }
    out
}

/// Energy (sum of squares) of `x` high-passed above `cutoff_hz` (2nd-order Butterworth,
/// run over the whole signal so its start-up does not matter) within
/// `centre +- half_window` frames. Detects the broadband splatter of a click.
pub fn highpass_energy_around(
    x: &[f32],
    rate: u32,
    cutoff_hz: f64,
    centre: usize,
    half_window: usize,
) -> f64 {
    let w0 = 2.0 * PI * cutoff_hz / rate as f64;
    let (cw, sw) = (w0.cos(), w0.sin());
    let alpha = sw / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
    let a0 = 1.0 + alpha;
    let b0 = (1.0 + cw) / 2.0 / a0;
    let b1 = -(1.0 + cw) / a0;
    let b2 = b0;
    let a1 = -2.0 * cw / a0;
    let a2 = (1.0 - alpha) / a0;
    let (mut x1, mut x2, mut y1, mut y2) = (0f64, 0f64, 0f64, 0f64);
    let lo = centre.saturating_sub(half_window);
    let hi = (centre + half_window).min(x.len());
    let mut energy = 0.0;
    for (n, &s) in x.iter().enumerate().take(hi) {
        let xn = s as f64;
        let y = b0 * xn + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
        x2 = x1;
        x1 = xn;
        y2 = y1;
        y1 = y;
        if n >= lo {
            energy += y * y;
        }
    }
    energy
}

/// Positions of isolated impulses: local maxima of `|x|` above `threshold_frac` of the global
/// peak, at least `min_gap` frames apart (a click train rendered through a voice).
pub fn detect_click_times(x: &[f32], threshold_frac: f32, min_gap: usize) -> Vec<usize> {
    let peak = x.iter().fold(0f32, |m, s| m.max(s.abs()));
    if peak <= 0.0 {
        return Vec::new();
    }
    let thr = peak * threshold_frac;
    let mut out: Vec<usize> = Vec::new();
    let mut best: Option<(usize, f32)> = None;
    for (i, s) in x.iter().enumerate() {
        let a = s.abs();
        if a >= thr {
            match best {
                Some((bi, bv)) if i - bi < min_gap => {
                    if a > bv {
                        best = Some((i, a));
                    }
                }
                Some((bi, _)) => {
                    out.push(bi);
                    best = Some((i, a));
                }
                None => best = Some((i, a)),
            }
        }
    }
    if let Some((bi, _)) = best {
        out.push(bi);
    }
    out
}

/// Counts of non-finite and subnormal samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NumericScan {
    pub nan: usize,
    pub inf: usize,
    pub subnormal: usize,
}

impl NumericScan {
    pub fn is_clean(&self) -> bool {
        self.nan == 0 && self.inf == 0 && self.subnormal == 0
    }
}

/// Scan for NaN, infinities and subnormals (the latter stall DSP loops on some CPUs).
pub fn scan_numeric(x: &[f32]) -> NumericScan {
    let mut s = NumericScan::default();
    for v in x {
        if v.is_nan() {
            s.nan += 1;
        } else if v.is_infinite() {
            s.inf += 1;
        } else if v.is_subnormal() {
            s.subnormal += 1;
        }
    }
    s
}

/// Index of the first sample whose bits differ (xor of the f32 bit patterns), or `None` when
/// the slices are bit-identical. A length mismatch reports the shorter length.
pub fn first_bit_difference(a: &[f32], b: &[f32]) -> Option<usize> {
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if x.to_bits() ^ y.to_bits() != 0 {
            return Some(i);
        }
    }
    (a.len() != b.len()).then(|| a.len().min(b.len()))
}

/// Panic unless `a` and `b` are bit-identical (bypass at 100 % must be).
pub fn assert_bit_exact(a: &[f32], b: &[f32]) {
    if let Some(i) = first_bit_difference(a, b) {
        panic!(
            "not bit-exact at sample {i}: {:?} vs {:?} (lens {} / {})",
            a.get(i),
            b.get(i),
            a.len(),
            b.len()
        );
    }
}

/// Equal-power crossfade gains of `n` frames: `(fade_in, fade_out)` = `(sin, cos)` of a
/// quarter period.
pub fn equal_power_gains(n: usize) -> (Vec<f32>, Vec<f32>) {
    let fin = (0..n)
        .map(|i| ((i as f64 + 0.5) / n as f64 * PI / 2.0).sin() as f32)
        .collect();
    let fout = (0..n)
        .map(|i| ((i as f64 + 0.5) / n as f64 * PI / 2.0).cos() as f32)
        .collect();
    (fin, fout)
}

/// Largest `|g_in^2 + g_out^2 - 1|` over a fade: 0 for an exact equal-power pair.
pub fn equal_power_gain_error(g_in: &[f32], g_out: &[f32]) -> f64 {
    g_in.iter()
        .zip(g_out)
        .map(|(a, b)| ((*a as f64).powi(2) + (*b as f64).powi(2) - 1.0).abs())
        .fold(0.0, f64::max)
}

/// RMS of consecutive `win`-frame blocks of `x` (the trailing partial block is dropped).
pub fn rms_profile(x: &[f32], win: usize) -> Vec<f64> {
    x.chunks_exact(win.max(1))
        .map(|c| (c.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / c.len() as f64).sqrt())
        .collect()
}

/// Largest deviation in dB of an RMS profile from `reference_rms`. Equal-power crossfade of
/// two uncorrelated equal-level signals keeps this near 0 dB; a linear fade dips by ~3 dB.
pub fn level_deviation_db(profile: &[f64], reference_rms: f64) -> f64 {
    profile
        .iter()
        .map(|r| (20.0 * (r.max(1e-12) / reference_rms.max(1e-12)).log10()).abs())
        .fold(0.0, f64::max)
}

/// RMS of the whole of `x`.
pub fn rms(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
}

/// Pitch ratio of a rendered tone: strongest FFT peak of `out` over the known input
/// frequency `input_hz` (2.0 = one octave up).
pub fn measured_pitch_ratio(out: &[f32], rate: u32, input_hz: f64) -> f64 {
    fft_peak_frequency(out, rate) / input_hz
}

/// Frames up to and including the last sample with `|x| > threshold` (content length, the
/// silence tail excluded).
pub fn active_length_frames(x: &[f32], threshold: f32) -> usize {
    x.iter()
        .rposition(|s| s.abs() > threshold)
        .map_or(0, |i| i + 1)
}

/// Duration ratio of a rendered buffer: active length of `output` over that of `input`
/// (0.5 = twice as fast / half as long). Build the input with [`silence_tail`] so the end of
/// the content is detectable.
pub fn measured_duration_ratio(input: &[f32], output: &[f32], threshold: f32) -> f64 {
    active_length_frames(output, threshold) as f64
        / active_length_frames(input, threshold).max(1) as f64
}

// ------------------------------------------------------------------------------------------
// RenderVoice harness
// ------------------------------------------------------------------------------------------

/// A voice that fills output buffers: the one interface later packages test offline (no
/// timing, only buffers). `out` is interleaved; the voice knows its channel count.
pub trait RenderVoice {
    fn render(&mut self, out: &mut [f32]);
}

/// Reference voice: plays a fixed buffer once, then silence.
pub struct SliceVoice {
    data: Vec<f32>,
    pos: usize,
}

impl SliceVoice {
    pub fn new(data: Vec<f32>) -> Self {
        Self { data, pos: 0 }
    }
}

impl RenderVoice for SliceVoice {
    fn render(&mut self, out: &mut [f32]) {
        let n = out.len().min(self.data.len() - self.pos);
        out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        out[n..].fill(0.0);
        self.pos += n;
    }
}

/// Render `total` samples through `voice` in blocks whose sizes cycle through `blocks`
/// (odd, unequal sizes expose block-boundary bugs; a voice must give the same output for any
/// block pattern).
pub fn render_chunked(voice: &mut dyn RenderVoice, total: usize, blocks: &[usize]) -> Vec<f32> {
    assert!(!blocks.is_empty() && blocks.iter().all(|b| *b > 0));
    let mut out = vec![0f32; total];
    let mut at = 0;
    let mut i = 0;
    while at < total {
        let n = blocks[i % blocks.len()].min(total - at);
        voice.render(&mut out[at..at + n]);
        at += n;
        i += 1;
    }
    out
}

// ------------------------------------------------------------------------------------------
// Instrumented MediaSource
// ------------------------------------------------------------------------------------------

/// Shared counters of a [`CountingSource`] (clone before handing the source to Symphonia).
#[derive(Debug, Clone, Default)]
pub struct SourceCounters {
    bytes: Arc<AtomicU64>,
    reads: Arc<AtomicU64>,
    seeks: Arc<AtomicU64>,
}

impl SourceCounters {
    /// Bytes returned by `read`.
    pub fn bytes_read(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    /// Number of `read` calls.
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
    /// Number of `seek` calls.
    pub fn seeks(&self) -> u64 {
        self.seeks.load(Ordering::Relaxed)
    }
    /// Zero all counters.
    pub fn reset(&self) {
        self.bytes.store(0, Ordering::Relaxed);
        self.reads.store(0, Ordering::Relaxed);
        self.seeks.store(0, Ordering::Relaxed);
    }
}

/// `Read + Seek` wrapper that counts bytes and seeks and is a Symphonia [`MediaSource`].
pub struct CountingSource<R> {
    inner: R,
    counters: SourceCounters,
    byte_len: Option<u64>,
}

impl<R: Read + Seek> CountingSource<R> {
    /// Wrap `inner`; the length (for `MediaSource::byte_len`) is measured once, uncounted.
    pub fn new(mut inner: R) -> io::Result<Self> {
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(0))?;
        Ok(Self {
            inner,
            counters: SourceCounters::default(),
            byte_len: Some(len),
        })
    }

    /// Like [`new`](Self::new) but adding to existing `counters` (several opens of one file).
    pub fn new_shared(mut inner: R, counters: SourceCounters) -> io::Result<Self> {
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(0))?;
        Ok(Self {
            inner,
            counters,
            byte_len: Some(len),
        })
    }

    /// Handle to the counters (survives moving the source into a `MediaSourceStream`).
    pub fn counters(&self) -> SourceCounters {
        self.counters.clone()
    }
}

impl<R: Read> Read for CountingSource<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.counters.bytes.fetch_add(n as u64, Ordering::Relaxed);
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        Ok(n)
    }
}

impl<R: Seek> Seek for CountingSource<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.counters.seeks.fetch_add(1, Ordering::Relaxed);
        self.inner.seek(pos)
    }
}

impl<R: Read + Seek + Send + Sync> MediaSource for CountingSource<R> {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        self.byte_len
    }
}

// ------------------------------------------------------------------------------------------
// Self-tests: every helper has a pass and a fail case.
// ------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    const R: u32 = 44_100;

    #[test]
    fn generators_have_the_expected_shape() {
        let s = sine(TONE_MID_HZ, R, 4410, 0.5);
        assert_eq!(s.len(), 4410);
        assert!(s[0].abs() < 1e-6);
        assert!((s.iter().fold(0f32, |m, v| m.max(v.abs())) - 0.5).abs() < 1e-3);

        let clicks = click_train(R, R as usize, 100, 0.9);
        let pos = click_train_positions(R, R as usize, 100);
        assert_eq!(pos.len(), 9); // 100..900 ms; the next would be at 1000 ms = len
        assert!(pos.iter().all(|&p| clicks[p] == 0.9));
        assert_eq!(clicks.iter().filter(|v| **v != 0.0).count(), pos.len());

        let (n1, n2) = (pink_noise(2000, 7, 0.8), pink_noise(2000, 7, 0.8));
        assert_eq!(n1, n2, "deterministic");
        assert_ne!(n1, pink_noise(2000, 8, 0.8));
        assert!((n1.iter().fold(0f32, |m, v| m.max(v.abs())) - 0.8).abs() < 1e-4);

        let chirp = log_chirp(100.0, 18_000.0, R, R as usize, 0.5);
        assert_eq!(chirp.len(), R as usize);
        assert!(scan_numeric(&chirp).is_clean());

        let tail = silence_tail(&s, 100);
        assert_eq!(tail.len(), s.len() + 100);
        assert!(tail[s.len()..].iter().all(|v| *v == 0.0));

        let swept = swept_amplitude_sine(TONE_LOW_HZ, R, R as usize, 0.1, 0.9);
        let head = swept[..2000].iter().fold(0f32, |m, v| m.max(v.abs()));
        let end = swept[R as usize - 2000..]
            .iter()
            .fold(0f32, |m, v| m.max(v.abs()));
        assert!(end > head * 4.0);
    }

    #[test]
    fn stereo_delay_is_exact_in_either_direction() {
        let m: Vec<f32> = (1..=8).map(|v| v as f32).collect();
        let st = stereo_with_delay(&m, 3);
        assert_eq!(channel(&st, 2, 0), m);
        assert_eq!(
            channel(&st, 2, 1),
            vec![0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0]
        );
        let neg = stereo_with_delay(&m, -2);
        assert_eq!(channel(&neg, 2, 1), m);
        assert_eq!(channel(&neg, 2, 0)[..3], [0.0, 0.0, 1.0]);
        assert_eq!(
            interleave_stereo(&[1.0, 2.0], &[3.0, 4.0]),
            vec![1.0, 3.0, 2.0, 4.0]
        );
    }

    #[test]
    fn wav_writer_and_frame_index_samples_round_trip() {
        let bytes = pcm16_wav_bytes(8000, 2, 10, |f, c| frame_index_sample(f) + c as i16);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(bytes.len(), 44 + 10 * 2 * 2);
        for f in [0usize, 1, 32_767, 32_768, 65_535, 65_536, 70_000] {
            let s = frame_index_sample(f) as f32 / 32768.0;
            assert_eq!(frame_index_from_f32(s), f % 65_536);
        }
    }

    #[test]
    fn fft_peak_finds_1khz_within_a_tenth_of_a_percent() {
        for (f, len) in [
            (1000.0, 44_100),
            (440.0, 30_000),
            (19_000.0, 20_000),
            (1234.5, 50_000),
        ] {
            let got = fft_peak_frequency(&sine(f, R, len, 0.5), R);
            assert!((got / f - 1.0).abs() < 0.001, "{f}: {got}");
        }
        // Fail case: a 1 kHz sine is not found at 1.01 kHz.
        let got = fft_peak_frequency(&sine(1000.0, R, 44_100, 0.5), R);
        assert!((got / 1010.0 - 1.0).abs() > 0.001);
    }

    #[test]
    fn stopband_and_alias_levels_separate_clean_from_dirty() {
        let clean = sine(1000.0, R, 40_000, 0.5);
        assert!(stopband_level_db(&clean, R, 8000.0) < -60.0);
        assert!(alias_level_db(&clean, R, 1000.0, 8) < -60.0);
        // Dirty: add a tone at 15 kHz, -20 dB relative.
        let spur = sine(15_000.0, R, 40_000, 0.05);
        let dirty: Vec<f32> = clean.iter().zip(&spur).map(|(a, b)| a + b).collect();
        let sb = stopband_level_db(&dirty, R, 8000.0);
        assert!((-30.0..-15.0).contains(&sb), "{sb}");
        let al = alias_level_db(&dirty, R, 1000.0, 8);
        assert!((-24.0..-16.0).contains(&al), "{al}");
        assert_eq!(stopband_level_db(&[0.0; 100], R, 8000.0), -200.0);
    }

    #[test]
    fn click_metric_trips_on_a_splice_not_on_a_clean_sine() {
        let amp = 0.5;
        let clean = sine(1000.0, R, 8000, amp);
        let bound = smooth_bound(amp as f64, 1000.0, R);
        let m = click_metric(&clean, bound);
        assert!(m <= 1.01, "clean sine metric {m}");

        // Splice: second half restarted at phase 0 (a seek without crossfade).
        let mut spliced = clean[..4000].to_vec();
        spliced.extend_from_slice(&sine(1000.0, R, 4000, amp)[7..]);
        let m = click_metric(&spliced, bound);
        assert!(m > 5.0, "spliced metric {m}");
        assert_eq!(discontinuity_positions(&spliced, 5.0 * bound).len(), 1);
        assert!(discontinuity_positions(&clean, 5.0 * bound).is_empty());

        // An impulse is also caught.
        let mut ticked = clean.clone();
        ticked[3000] += 0.5;
        assert!(click_metric(&ticked, bound) > 5.0);
    }

    #[test]
    fn highpass_energy_around_a_point_sees_a_click_only() {
        let clean = sine(440.0, R, 12_000, 0.5);
        let mut clicked = clean.clone();
        for v in clicked.iter_mut().skip(6000) {
            *v += 0.5; // DC step at 6000
        }
        let at = |x: &[f32]| highpass_energy_around(x, R, 8000.0, 6000, 64);
        let (e_clean, e_clicked) = (at(&clean), at(&clicked));
        assert!(e_clean < 1e-4, "{e_clean}");
        assert!(
            e_clicked > e_clean * 100.0 && e_clicked > 1e-2,
            "{e_clicked}"
        );
        // Away from the click the clicked signal is quiet again.
        assert!(highpass_energy_around(&clicked, R, 8000.0, 2000, 64) < 1e-4);
    }

    #[test]
    fn click_times_are_detected_at_their_positions() {
        let train = click_train(R, 22_050, 100, 0.8);
        let want = click_train_positions(R, 22_050, 100);
        assert_eq!(detect_click_times(&train, 0.5, 100), want);
        // Shifted by 37 frames (e.g. a latency): detection tracks it.
        let mut shifted = vec![0f32; 37];
        shifted.extend_from_slice(&train);
        let got = detect_click_times(&shifted, 0.5, 100);
        assert_eq!(got, want.iter().map(|p| p + 37).collect::<Vec<_>>());
        assert!(detect_click_times(&[0.0; 50], 0.5, 10).is_empty());
    }

    #[test]
    fn numeric_scan_finds_bad_samples() {
        assert!(scan_numeric(&[0.0, 1.0, -1.0, 1e-30]).is_clean());
        let bad = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e-40, 0.5];
        let s = scan_numeric(&bad);
        assert_eq!((s.nan, s.inf, s.subnormal), (1, 2, 1));
        assert!(!s.is_clean());
    }

    #[test]
    fn bit_exact_compare_sees_a_single_ulp_and_length() {
        let a = sine(440.0, R, 1000, 0.5);
        assert_eq!(first_bit_difference(&a, &a.clone()), None);
        assert_bit_exact(&a, &a.clone());
        let mut b = a.clone();
        b[500] = f32::from_bits(b[500].to_bits() ^ 1);
        assert_eq!(first_bit_difference(&a, &b), Some(500));
        assert_eq!(first_bit_difference(&a, &a[..900]), Some(900));
        // -0.0 and 0.0 are different bit patterns (and must be reported).
        assert_eq!(first_bit_difference(&[0.0], &[-0.0]), Some(0));
    }

    #[test]
    #[should_panic(expected = "not bit-exact")]
    fn assert_bit_exact_panics_on_difference() {
        assert_bit_exact(&[1.0, 2.0], &[1.0, 2.5]);
    }

    #[test]
    fn equal_power_gains_and_level_check() {
        let (gi, go) = equal_power_gains(2048);
        assert!(equal_power_gain_error(&gi, &go) < 1e-6);
        // A linear fade fails the check (its power dips by 3 dB mid fade).
        let lin_in: Vec<f32> = (0..2048).map(|i| (i as f32 + 0.5) / 2048.0).collect();
        let lin_out: Vec<f32> = lin_in.iter().map(|g| 1.0 - g).collect();
        assert!(equal_power_gain_error(&lin_in, &lin_out) > 0.4);

        // Level of a crossfade between two uncorrelated noises stays flat with equal power
        // and dips with a linear fade.
        let n = 16_384;
        let a = pink_noise(n, 1, 0.5);
        let b = pink_noise(n, 99, 0.5);
        let (gi, go) = equal_power_gains(n);
        let ep: Vec<f32> = (0..n).map(|i| a[i] * go[i] + b[i] * gi[i]).collect();
        let lin: Vec<f32> = (0..n)
            .map(|i| {
                let t = (i as f32 + 0.5) / n as f32;
                a[i] * (1.0 - t) + b[i] * t
            })
            .collect();
        let reference = (rms(&a) + rms(&b)) / 2.0;
        let dev_ep = level_deviation_db(&rms_profile(&ep, 2048), reference);
        let dev_lin = level_deviation_db(&rms_profile(&lin, 2048), reference);
        assert!(dev_ep < 1.5, "equal-power deviates {dev_ep} dB");
        assert!(dev_lin > 2.0, "linear should dip, deviates {dev_lin} dB");
    }

    #[test]
    fn pitch_and_duration_ratio_of_synthetic_renders() {
        // "Varispeed at 2x": the tone is one octave up and half as long.
        let input = silence_tail(&sine(1000.0, R, 40_000, 0.5), 8000);
        let fast: Vec<f32> = sine(2000.0, R, 20_000, 0.5);
        let fast = silence_tail(&fast, 14_000);
        let pr = measured_pitch_ratio(&fast[..20_000], R, 1000.0);
        assert!((pr - 2.0).abs() < 0.002, "{pr}");
        let dr = measured_duration_ratio(&input, &fast, 0.01);
        assert!((dr - 0.5).abs() < 0.01, "{dr}");
        // Identity renders measure 1.0 and 1.0; a wrong pitch is not mistaken for right.
        assert!((measured_pitch_ratio(&input[..40_000], R, 1000.0) - 1.0).abs() < 0.002);
        assert!((measured_duration_ratio(&input, &input, 0.01) - 1.0).abs() < 1e-9);
        assert!((measured_pitch_ratio(&fast[..20_000], R, 1000.0) - 1.0).abs() > 0.5);
        assert_eq!(active_length_frames(&[0.0; 10], 0.01), 0);
        assert_eq!(active_length_frames(&[0.0, 0.5, 0.0, 0.0], 0.01), 2);
    }

    #[test]
    fn render_chunked_is_block_pattern_independent() {
        let src = pink_noise(5000, 3, 0.7);
        let whole = render_chunked(&mut SliceVoice::new(src.clone()), 6000, &[6000]);
        let odd = render_chunked(&mut SliceVoice::new(src.clone()), 6000, &[1, 7, 256, 333]);
        assert_bit_exact(&whole, &odd);
        assert_bit_exact(&whole[..5000], &src);
        assert!(
            whole[5000..].iter().all(|v| *v == 0.0),
            "silence after the end"
        );
        // The harness would notice a voice that depends on the block size.
        struct Bad(f32);
        impl RenderVoice for Bad {
            fn render(&mut self, out: &mut [f32]) {
                self.0 += out.len() as f32; // value depends on block size
                out.fill(self.0);
            }
        }
        let a = render_chunked(&mut Bad(0.0), 64, &[64]);
        let b = render_chunked(&mut Bad(0.0), 64, &[8]);
        assert!(first_bit_difference(&a, &b).is_some());
    }

    #[test]
    fn counting_source_counts_reads_and_seeks_through_symphonia() {
        let bytes = pcm16_wav_bytes(8000, 1, 8000, |f, _| frame_index_sample(f));
        let total = bytes.len() as u64;
        let src = CountingSource::new(io::Cursor::new(bytes)).unwrap();
        assert_eq!(src.byte_len(), Some(total));
        let counters = src.counters();
        assert_eq!((counters.bytes_read(), counters.seeks()), (0, 0));

        let mss = MediaSourceStream::new(Box::new(src), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("wav");
        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .unwrap();
        let mut format = probed.format;
        let after_probe = counters.bytes_read();
        assert!(after_probe > 0 && after_probe <= total);

        counters.reset();
        assert_eq!(
            (counters.bytes_read(), counters.reads(), counters.seeks()),
            (0, 0, 0)
        );
        let track_id = format.tracks()[0].id;
        format
            .seek(SeekMode::Accurate, SeekTo::TimeStamp { ts: 4000, track_id })
            .unwrap();
        let _ = format.next_packet().unwrap();
        assert!(counters.seeks() >= 1, "a seek was counted");
        assert!(
            counters.bytes_read() > 0,
            "reading after the seek was counted"
        );
        assert!(
            counters.bytes_read() < total,
            "a WAV seek does not re-read the file"
        );

        // Plain Read/Seek use is counted too.
        let mut plain = CountingSource::new(io::Cursor::new(vec![7u8; 100])).unwrap();
        let c = plain.counters();
        let mut buf = [0u8; 10];
        plain.seek(SeekFrom::Start(50)).unwrap();
        plain.read_exact(&mut buf).unwrap();
        assert_eq!((c.bytes_read(), c.reads(), c.seeks()), (10, 1, 1));
    }

    #[test]
    fn lcg_is_deterministic_and_bounded() {
        let mut a = Lcg::new(5);
        let mut b = Lcg::new(5);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut r = Lcg::new(9);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let v = r.below(50);
            assert!(v < 50);
            seen.insert(v);
            let u = r.unit();
            assert!((-1.0..1.0).contains(&u));
        }
        assert!(seen.len() > 40, "covers the range");
        assert_eq!(r.below(0), 0);
    }
}

// ---------------------------------------------------------------------------------------------
// Gated decoder (slow-stick simulation for engine tests)
// ---------------------------------------------------------------------------------------------

/// Wraps a decoder so every `next_chunk` / `seek_frame` stalls while `open` is `false`: a slow
/// or stuck USB stick as the decode worker sees it. (The decoder itself may live in the isolated
/// media helper; the stall is applied on the parent's side of the pipe.)
pub struct GatedBlocks {
    inner: Box<dyn crate::worker::BlockSource>,
    open: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl GatedBlocks {
    pub fn new(
        inner: Box<dyn crate::worker::BlockSource>,
        open: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self { inner, open }
    }

    fn wait(&self) {
        while !self.open.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

impl crate::worker::BlockSource for GatedBlocks {
    fn channels(&self) -> usize {
        self.inner.channels()
    }
    fn sample_rate_hz(&self) -> u32 {
        self.inner.sample_rate_hz()
    }
    fn position_frame(&self) -> u64 {
        self.inner.position_frame()
    }
    fn seek_frame(&mut self, frame: u64) -> Result<(), crate::AudioError> {
        self.wait();
        self.inner.seek_frame(frame)
    }
    fn next_chunk(&mut self, max_frames: usize) -> Result<Option<crate::PcmBuffer>, crate::AudioError> {
        self.wait();
        self.inner.next_chunk(max_frames)
    }
    fn install_index(&mut self, index: std::sync::Arc<crate::index::Mp3Index>) {
        self.inner.install_index(index);
    }
    fn poll_index(&mut self) -> bool {
        self.inner.poll_index()
    }
    fn index_entries(&self) -> u64 {
        self.inner.index_entries()
    }
    fn has_index(&self) -> bool {
        self.inner.has_index()
    }
    fn indexed_seeks(&self) -> u64 {
        self.inner.indexed_seeks()
    }
    fn skipped_packets(&self) -> u64 {
        self.inner.skipped_packets()
    }
}
