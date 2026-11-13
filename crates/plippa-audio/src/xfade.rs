//! Equal-power crossfade primitive and the output stage (WARP_04).
//!
//! [`Crossfader`] fades from an **old** signal (the continuation of what was being output,
//! captured when the fade starts) into the **new** signal the caller feeds block by block:
//! `out = old * cos(pi/2 t) + new * sin(pi/2 t)`, `t = i / N`, so the first output frame
//! equals the old continuation exactly and the frame after the last is pure new.
//!
//! All buffers are allocated once in [`Crossfader::new`] (sized for the longest configured
//! fade); starting and running a fade never allocates.

use std::f64::consts::FRAC_PI_2;

/// Crossfade lengths in milliseconds: the single place to tune them (README section 7,
/// `XfadeConfig`). Only `xfade_seek_ms` is used by WARP_04; the others are reserved for the
/// packages that fade at bypass entry/exit, between voices and across far jumps.
#[derive(Debug, Clone, PartialEq)]
pub struct XfadeConfig {
    /// Hard seek (cue jump, resume, `SeekMs`): `XFADE_SEEK_MS`.
    pub xfade_seek_ms: f64,
    /// Entering/leaving the 100 % bypass path: `XFADE_BYPASS_MS` (WARP_05).
    pub xfade_bypass_ms: f64,
    /// Switching between two rate voices: `XFADE_VOICE_MS` (WARP_05/09).
    pub xfade_voice_ms: f64,
    /// Waypoint jump of a far chase: `XFADE_WAYPOINT_MS` (WARP_14).
    pub xfade_waypoint_ms: f64,
}

impl Default for XfadeConfig {
    fn default() -> Self {
        Self {
            xfade_seek_ms: 10.0,
            xfade_bypass_ms: 5.0,
            xfade_voice_ms: 40.0,
            xfade_waypoint_ms: 60.0,
        }
    }
}

impl XfadeConfig {
    /// Frames of a fade of `ms` milliseconds at `rate_hz` (at least 1).
    pub fn frames(ms: f64, rate_hz: u32) -> usize {
        ((ms * f64::from(rate_hz) / 1000.0).round() as usize).max(1)
    }

    /// Length of a hard-seek crossfade in frames.
    pub fn seek_frames(&self, rate_hz: u32) -> usize {
        Self::frames(self.xfade_seek_ms, rate_hz)
    }

    /// Frames of the longest configured fade: what a [`Crossfader`] preallocates.
    pub fn max_frames(&self, rate_hz: u32) -> usize {
        let longest = self
            .xfade_seek_ms
            .max(self.xfade_bypass_ms)
            .max(self.xfade_voice_ms)
            .max(self.xfade_waypoint_ms);
        Self::frames(longest, rate_hz)
    }
}

/// Equal-power gains at fade position `i` of `n`: `(gain_in, gain_out) = (sin, cos)(pi/2 i/n)`.
pub fn equal_power_gain(i: usize, n: usize) -> (f32, f32) {
    let t = i as f64 / n.max(1) as f64;
    ((FRAC_PI_2 * t).sin() as f32, (FRAC_PI_2 * t).cos() as f32)
}

/// Equal-power crossfade state machine with preallocated buffers.
#[derive(Debug)]
pub struct Crossfader {
    channels: usize,
    cap_frames: usize,
    /// Continuation of the outgoing signal, `len` frames valid.
    old: Vec<f32>,
    /// Scratch for rebuilding `old` when a fade is interrupted.
    spare: Vec<f32>,
    gain_in: Vec<f32>,
    gain_out: Vec<f32>,
    /// Fade length in frames of the current/last fade.
    len: usize,
    /// Frames of the current fade already produced. Not capped at `len` so a pause requeue can
    /// [`rewind`](Self::rewind) back into a finished fade (its buffers stay valid).
    done: usize,
    /// Frames passed through [`process`](Self::process) after the fade finished (so a
    /// [`rewind`](Self::rewind) knows how much of a rewind lies beyond the fade).
    post: usize,
}

impl Crossfader {
    /// Allocate for fades up to `max_frames` frames of `channels` interleaved channels.
    pub fn new(channels: usize, max_frames: usize) -> Self {
        let channels = channels.max(1);
        let cap_frames = max_frames.max(1);
        Self {
            channels,
            cap_frames,
            old: vec![0.0; cap_frames * channels],
            spare: vec![0.0; cap_frames * channels],
            gain_in: vec![0.0; cap_frames],
            gain_out: vec![0.0; cap_frames],
            len: 0,
            done: 0,
            post: 0,
        }
    }

    /// Longest fade this instance can run, in frames.
    pub fn capacity_frames(&self) -> usize {
        self.cap_frames
    }

    /// True while a fade has frames left to produce.
    pub fn active(&self) -> bool {
        self.done < self.len
    }

    /// Frames of the current fade still to produce (0 when idle).
    pub fn remaining(&self) -> usize {
        self.len.saturating_sub(self.done)
    }

    /// Length in frames of the current (or last) fade.
    pub fn fade_frames(&self) -> usize {
        self.len
    }

    /// Start a fade of `frames` frames from the signal whose continuation is `old_next`
    /// (interleaved; shorter than `frames` is zero-padded, longer is cut).
    ///
    /// If a fade is still running its mixed output is what is currently audible, so the new
    /// "old" is that mix continued: `old[k]*cos + current_new[j]*sin` for the rest of the
    /// running fade, then `current_new` alone. `old_next` must then be the continuation of the
    /// signal that was fading **in** (the caller reads it from its window), which is exactly
    /// what it passes when no fade runs. This is the "interrupted crossfade restarts from the
    /// current mixed output" rule.
    pub fn begin(&mut self, frames: usize, old_next: &[f32]) {
        let n = frames.clamp(1, self.cap_frames);
        debug_assert!(
            frames <= self.cap_frames,
            "crossfade longer than its buffers"
        );
        let ch = self.channels;
        let have = (old_next.len() / ch).min(n);
        if self.active() {
            for j in 0..n {
                for c in 0..ch {
                    let cur = if j < have { old_next[j * ch + c] } else { 0.0 };
                    let k = self.done + j;
                    self.spare[j * ch + c] = if k < self.len {
                        self.old[k * ch + c] * self.gain_out[k] + cur * self.gain_in[k]
                    } else {
                        cur
                    };
                }
            }
            std::mem::swap(&mut self.old, &mut self.spare);
        } else {
            self.old[..have * ch].copy_from_slice(&old_next[..have * ch]);
            self.old[have * ch..n * ch].fill(0.0);
        }
        self.len = n;
        self.done = 0;
        self.post = 0;
        for i in 0..n {
            let (gi, go) = equal_power_gain(i, n);
            self.gain_in[i] = gi;
            self.gain_out[i] = go;
        }
    }

    /// Mix the leading frames of `new` (interleaved) with the old continuation, in place, and
    /// return how many frames were mixed (0 if idle). Frames beyond the fade pass untouched.
    pub fn process(&mut self, new: &mut [f32]) -> usize {
        let ch = self.channels;
        let total = new.len() / ch;
        let frames = total.min(self.remaining());
        if self.len > 0 {
            self.post = self.post.saturating_add(total - frames);
        }
        for j in 0..frames {
            let k = self.done + j;
            let (gi, go) = (self.gain_in[k], self.gain_out[k]);
            for c in 0..ch {
                let o = j * ch + c;
                new[o] = self.old[k * ch + c] * go + new[o] * gi;
            }
        }
        self.done += frames;
        frames
    }

    /// Take back `frames` produced frames (a pause put that audio back in the source): the
    /// fade resumes from where the replayed audio begins. Frames that were produced after the
    /// fade had finished are not part of it: rewinding over them leaves the fade finished.
    pub fn rewind(&mut self, frames: usize) {
        let beyond = frames.min(self.post);
        self.post -= beyond;
        self.done = self.done.saturating_sub(frames - beyond);
    }

    /// Drop any running fade (a new stream, a track change).
    pub fn cancel(&mut self) {
        self.len = 0;
        self.done = 0;
        self.post = 0;
    }
}

/// Smallest magnitude kept at the output; anything below is flushed to zero so a denormal
/// tail can never reach the sink (WARP_04 design rule 7).
pub const DENORMAL_FLUSH: f32 = 1e-20;

/// Output stage: volume, clamp to +-1, then NaN and denormal flush. For ordinary samples this is
/// exactly `(s * gain).clamp(-1.0, 1.0)` (bit for bit: `-0.0` stays `-0.0`).
pub fn apply_output_stage(samples: &mut [f32], gain: f32) {
    for s in samples.iter_mut() {
        let v = (*s * gain).clamp(-1.0, 1.0);
        *s = if v.is_nan() || (v != 0.0 && v.abs() < DENORMAL_FLUSH) {
            0.0
        } else {
            v
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{
        click_metric, equal_power_gain_error, highpass_energy_around, level_deviation_db, rms,
        rms_profile, scan_numeric, sine, smooth_bound, Lcg,
    };

    const R: u32 = 48_000;

    #[test]
    fn defaults_match_the_register() {
        let c = XfadeConfig::default();
        assert_eq!(
            (
                c.xfade_seek_ms,
                c.xfade_bypass_ms,
                c.xfade_voice_ms,
                c.xfade_waypoint_ms
            ),
            (10.0, 5.0, 40.0, 60.0)
        );
        assert_eq!(c.seek_frames(48_000), 480);
        assert_eq!(c.seek_frames(44_100), 441);
        assert_eq!(
            c.max_frames(48_000),
            2880,
            "longest is the 60 ms waypoint fade"
        );
        assert_eq!(XfadeConfig::frames(0.0, 48_000), 1);
    }

    #[test]
    fn gains_are_exactly_equal_power() {
        for n in [1usize, 2, 441, 480, 2880] {
            let (gi, go): (Vec<f32>, Vec<f32>) = (0..n).map(|i| equal_power_gain(i, n)).unzip();
            assert!(equal_power_gain_error(&gi, &go) < 1e-6, "n {n}");
            assert_eq!(gi[0], 0.0);
            assert_eq!(go[0], 1.0, "first frame is the old signal exactly");
        }
    }

    fn run_fade(old: &[f32], new: &[f32], blocks: &[usize]) -> Vec<f32> {
        let n = old.len();
        let mut x = Crossfader::new(1, n);
        x.begin(n, old);
        let mut out = new.to_vec();
        let mut at = 0;
        let mut b = 0;
        while at < n {
            let step = blocks[b % blocks.len()].min(n - at);
            assert_eq!(x.process(&mut out[at..at + step]), step);
            at += step;
            b += 1;
        }
        assert!(!x.active());
        out
    }

    #[test]
    fn fade_endpoints_and_block_independence() {
        let n = 480;
        let old = vec![0.5f32; n];
        let new = vec![-0.25f32; n];
        let a = run_fade(&old, &new, &[480]);
        let b = run_fade(&old, &new, &[1, 7, 64, 3]);
        assert_eq!(a, b, "result does not depend on the block pattern");
        assert_eq!(a[0], 0.5, "starts on the old signal");
        assert!((a[n - 1] - -0.25).abs() < 0.01, "ends on the new signal");
        // Beyond the fade nothing is touched.
        let mut x = Crossfader::new(1, n);
        x.begin(n, &old);
        let mut tail = vec![0.3f32; n + 50];
        assert_eq!(x.process(&mut tail), n);
        assert!(tail[n..].iter().all(|v| *v == 0.3));
        assert_eq!(x.process(&mut tail), 0, "idle once finished");
    }

    #[test]
    fn short_old_continuation_is_zero_padded() {
        let mut x = Crossfader::new(2, 4);
        x.begin(4, &[1.0, 1.0]); // one stereo frame only
        let mut new = vec![1.0f32; 8];
        x.process(&mut new);
        assert_eq!(new[0], 1.0 * 1.0 + 1.0 * 0.0);
        // Frame 1 onward: old is 0, so the output is the new signal times the fade-in gain.
        let (gi, _) = equal_power_gain(1, 4);
        assert!((new[2] - gi).abs() < 1e-6 && (new[3] - gi).abs() < 1e-6);
    }

    #[test]
    fn hard_seek_between_phase_shifted_sines_has_no_click() {
        // 1 kHz sine, old phase vs a seek target 0.37 periods later (a hard phase jump).
        let amp = 0.5f32;
        let all = sine(1000.0, R, 48_000, amp);
        let jump = 17; // frames: 17/48 of a period, an arbitrary phase offset
        let switch = 4_000usize;
        let fade = XfadeConfig::default().seek_frames(R);
        // Old continues, new is the same sine read `jump` frames ahead.
        let mut x = Crossfader::new(1, fade);
        let mut out: Vec<f32> = all[..switch].to_vec();
        x.begin(fade, &all[switch..switch + fade]);
        let mut newpart: Vec<f32> = all[switch + jump..switch + jump + 4_000].to_vec();
        x.process(&mut newpart);
        out.extend_from_slice(&newpart);
        let bound = smooth_bound(amp as f64, 1000.0, R);
        let m = click_metric(&out, bound);
        assert!(m < 1.5, "click metric through the crossfade {m}");
        // >8 kHz energy around the switch, in excess of what the 1 kHz tone itself leaks through
        // the 2nd-order high-pass (measured on the same stretch of the undisturbed sine), as
        // mean-square per frame in dBFS (full scale = 1.0).
        let (centre, half) = (switch + fade / 2, 600usize);
        let excess_dbfs = |x: &[f32]| {
            let e = highpass_energy_around(x, R, 8000.0, centre, half);
            let e0 = highpass_energy_around(&all[..x.len()], R, 8000.0, centre, half);
            10.0 * ((e - e0).max(1e-30) / (2 * half) as f64).log10()
        };
        let db = excess_dbfs(&out);
        assert!(db < -60.0, "HF excess around the switch {db:.1} dBFS");
        // A plain hard cut (no fade) at the same place is a click by the same measures.
        let mut cut: Vec<f32> = all[..switch].to_vec();
        cut.extend_from_slice(&all[switch + jump..switch + jump + 4_000]);
        assert!(click_metric(&cut, bound) > 5.0);
        assert!(
            excess_dbfs(&cut) > -45.0,
            "the plain cut is audible HF: {:.1}",
            excess_dbfs(&cut)
        );
        assert!(scan_numeric(&out).is_clean());
    }

    #[test]
    fn level_through_the_fade_for_uncorrelated_and_quadrature_tones() {
        let n = 4_800; // 100 ms so the RMS windows are meaningful
        let amp = 0.5f32;
        let ref_rms = amp as f64 / 2f64.sqrt();
        // Uncorrelated (different frequencies): equal power keeps the level flat.
        let old = sine(440.0, R, n, amp);
        let new = sine(1370.0, R, n, amp);
        let mixed = run_fade(&old, &new, &[256]);
        let dev = level_deviation_db(&rms_profile(&mixed[240..n - 240], 480), ref_rms);
        assert!(dev < 0.5, "uncorrelated level deviation {dev} dB");
        // Correlated, same frequency, 90 degrees apart: power sums exactly, within +-0.5 dB.
        let a = sine(1000.0, R, n, amp);
        let b: Vec<f32> = (0..n)
            .map(|i| {
                (amp as f64
                    * (2.0 * std::f64::consts::PI * 1000.0 * (i as f64 + 12.0) / R as f64).sin())
                    as f32
            })
            .collect(); // 12 samples = quarter period at 1 kHz / 48 kHz
        let mixed = run_fade(&a, &b, &[480]);
        let dev = level_deviation_db(&rms_profile(&mixed[480..n - 480], 480), ref_rms);
        assert!(dev < 0.5, "quadrature level deviation {dev} dB");
        // In-phase identical signals are fully correlated: the known +3 dB equal-power bump,
        // bounded (documented behaviour, not a bug).
        let same = run_fade(&a, &a, &[480]);
        let peak = rms_profile(&same, 96).into_iter().fold(0.0, f64::max);
        let bump_db = 20.0 * (peak / rms(&a)).log10();
        assert!(bump_db > 2.0 && bump_db < 3.1, "bump {bump_db}");
    }

    #[test]
    fn interrupted_fade_restarts_from_the_current_mix() {
        // Fade 1: old = A (const 1.0), new = B (const 0.0). Interrupt halfway: the audible mix
        // there is cos(pi/4) = 0.7071. The second fade must start at exactly that level.
        let n = 400;
        let mut x = Crossfader::new(1, n);
        x.begin(n, &vec![1.0f32; n]);
        let mut first = vec![0.0f32; n / 2];
        x.process(&mut first);
        let last_heard = *first.last().unwrap();
        // The next frame the first fade would have produced:
        let (gi, go) = equal_power_gain(n / 2, n);
        let would_be = 1.0 * go + 0.0 * gi;
        // Second fade, new signal C = 0.25 const. `old_next` = continuation of B (0.0).
        x.begin(n, &vec![0.0f32; n]);
        let mut second = vec![0.25f32; n];
        x.process(&mut second);
        assert!(
            (second[0] - would_be).abs() < 1e-6,
            "{} vs {would_be}",
            second[0]
        );
        assert!(
            last_heard > would_be,
            "monotone fade-out: no step at the restart"
        );
        assert!(
            (second[0] - last_heard).abs() < 0.02,
            "continuous to within one fade step"
        );
        // After the second fade completes the output is pure C; no frame jumps.
        assert!((second[n - 1] - 0.25).abs() < 0.01);
        let mut all = first.clone();
        all.extend_from_slice(&second);
        let maxstep = all
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max);
        assert!(maxstep < 0.02, "max step {maxstep}");
    }

    #[test]
    fn interrupt_after_the_fade_finished_is_a_plain_restart() {
        let mut x = Crossfader::new(1, 8);
        x.begin(8, &[1.0; 8]);
        let mut a = vec![0.0f32; 8];
        x.process(&mut a);
        assert!(!x.active());
        x.begin(8, &[0.5; 8]);
        let mut b = vec![0.0f32; 8];
        x.process(&mut b);
        assert_eq!(b[0], 0.5);
    }

    #[test]
    fn rewind_replays_the_fade_consistently() {
        let n = 64;
        let old: Vec<f32> = (0..n).map(|i| i as f32 * 0.01).collect();
        let new = vec![1.0f32; n];
        let mut x = Crossfader::new(1, n);
        x.begin(n, &old);
        let mut a = new.clone();
        x.process(&mut a[..40]);
        // Pretend the last 10 produced frames were put back (pause requeue) and re-render them.
        x.rewind(10);
        assert!(x.active());
        let mut replay = new[30..40].to_vec();
        x.process(&mut replay);
        assert_eq!(&a[30..40], &replay[..]);
        // And a finished fade can be rewound into.
        let mut rest = new[40..].to_vec();
        x.process(&mut rest);
        assert!(!x.active());
        x.rewind(5);
        assert!(x.active() && x.remaining() == 5);
        // Audio produced after the fade ended is not part of it: rewinding over only that
        // audio must not restart the fade.
        let mut y = Crossfader::new(1, 8);
        y.begin(8, &[1.0; 8]);
        y.process(&mut [0.0; 20]);
        assert!(!y.active());
        y.rewind(12);
        assert!(!y.active());
        y.rewind(3);
        assert!(
            y.active() && y.remaining() == 3,
            "only the part inside the fade re-opens it"
        );
    }

    #[test]
    fn output_stage_is_bit_exact_for_ordinary_samples_and_safe_otherwise() {
        let mut rng = Lcg::new(5);
        let src: Vec<f32> = (0..4096).map(|_| rng.unit() * 2.0 - 1.0).collect();
        for gain in [1.0f32, 0.5, 0.37, 0.0] {
            let mut got = src.clone();
            apply_output_stage(&mut got, gain);
            let want: Vec<f32> = src.iter().map(|s| (*s * gain).clamp(-1.0, 1.0)).collect();
            for (g, w) in got.iter().zip(&want) {
                assert_eq!(g.to_bits() ^ w.to_bits(), 0);
            }
        }
        let mut odd = vec![
            -0.0f32,
            f32::NAN,
            1e-30,
            -1e-30,
            1.0e-19,
            7.0,
            -7.0,
            f32::INFINITY,
        ];
        apply_output_stage(&mut odd, 1.0);
        assert_eq!(odd[0].to_bits(), (-0.0f32).to_bits(), "-0.0 untouched");
        assert_eq!(&odd[1..4], &[0.0, 0.0, 0.0]);
        assert_eq!(odd[4], 1.0e-19, "above the flush threshold");
        assert_eq!((odd[5], odd[6], odd[7]), (1.0, -1.0, 1.0));
        assert!(scan_numeric(&odd).is_clean());
    }
}
