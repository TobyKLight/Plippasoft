//! Band-limited windowed-sinc resampling kernel for the varispeed voice (WARP_05).
//!
//! 64-tap Kaiser-windowed sinc, 256 phases plus linear interpolation between phase rows, with
//! nine cutoff tables for `|rate|` 1.0 ..= 2.0 (`fc = 0.97 / rate`, quantised **up** to the next
//! table so the stopband is never narrower than needed). At `|rate| <= 1` the full-band table
//! is used. The phase-0 row of the full-band table is an exact delta, so a read at an integer
//! position reproduces the source bit for bit and entering or leaving varispeed at 1.0 is
//! seamless. A negative rate is the same kernel with a negative step.
//!
//! Tables are built lazily once ([`SincTables::global`]); each is `257 x 64` f32 (about 66 KB).

use std::f64::consts::PI;
use std::sync::OnceLock;

/// Kernel length in taps.
pub const TAPS: usize = 64;
/// Taps before the read position (`i - 31 ..= i + 32` around `i = floor(pos)`).
pub const TAPS_BEFORE: usize = TAPS / 2 - 1;
/// Taps after the read position.
pub const TAPS_AFTER: usize = TAPS / 2;
/// Phases per source frame (plus one extra row so linear interpolation never wraps).
pub const PHASES: usize = 256;
/// Number of cutoff tables for `|rate|` 1.0 ..= 2.0 in steps of 1/8.
pub const TABLES: usize = 9;
/// Kaiser window beta.
pub const KAISER_BETA: f64 = 9.0;
/// Cutoff of the full-band table as a fraction of Nyquist.
pub const BASE_CUTOFF: f64 = 0.97;
/// Largest `|rate|` the sinc reader accepts (beyond it a boxcar reader is used, WARP_17).
pub const MAX_SINC_RATE: f64 = 2.0;

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..60 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

/// The nine tables, row-major `[(PHASES + 1) * TAPS]` each.
#[derive(Debug)]
pub struct SincTables {
    tables: Vec<Vec<f32>>,
}

impl SincTables {
    /// Build all tables (a few milliseconds).
    pub fn build() -> Self {
        let i0_beta = bessel_i0(KAISER_BETA);
        let half = (TAPS / 2) as f64;
        let mut tables = Vec::with_capacity(TABLES);
        for k in 0..TABLES {
            let rate = 1.0 + k as f64 / 8.0;
            let fc = BASE_CUTOFF / rate;
            let mut t = vec![0.0f32; (PHASES + 1) * TAPS];
            for p in 0..=PHASES {
                let frac = p as f64 / PHASES as f64;
                let mut row = [0.0f64; TAPS];
                let mut sum = 0.0;
                for (m, c) in row.iter_mut().enumerate() {
                    let tau = (m as f64 - TAPS_BEFORE as f64) - frac;
                    let r = tau / half;
                    let w = if r.abs() >= 1.0 {
                        0.0
                    } else {
                        bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0_beta
                    };
                    *c = fc * sinc(fc * tau) * w;
                    sum += *c;
                }
                for (m, c) in row.iter().enumerate() {
                    t[p * TAPS + m] = (*c / sum) as f32;
                }
            }
            if k == 0 {
                // Exact delta at phase 0: integer positions reproduce the source.
                for m in 0..TAPS {
                    t[m] = if m == TAPS_BEFORE { 1.0 } else { 0.0 };
                }
            }
            tables.push(t);
        }
        Self { tables }
    }

    /// The process-wide tables, built on first use.
    pub fn global() -> &'static SincTables {
        static TABLES_ONCE: OnceLock<SincTables> = OnceLock::new();
        TABLES_ONCE.get_or_init(SincTables::build)
    }

    /// Index of the table for `|rate|`: the smallest cutoff table at least as narrow as needed.
    pub fn table_index(rate_abs: f64) -> usize {
        if rate_abs <= 1.0 {
            0
        } else {
            (((rate_abs - 1.0) * 8.0).ceil() as usize).min(TABLES - 1)
        }
    }

    fn table(&self, rate_abs: f64) -> &[f32] {
        &self.tables[Self::table_index(rate_abs)]
    }

    /// Bytes of one table (about 66 KB).
    pub fn table_bytes() -> usize {
        (PHASES + 1) * TAPS * 4
    }
}

/// Source frames a block of `frames` output frames at a constant `rate` starting at `pos`
/// touches: `(first, count)` with `first` the absolute frame of the first needed source frame.
pub fn span_for(pos: f64, rate: f64, frames: usize) -> (i64, usize) {
    let last_pos = pos + rate * frames.saturating_sub(1) as f64;
    let (lo, hi) = if rate >= 0.0 {
        (pos, last_pos)
    } else {
        (last_pos, pos)
    };
    let first = lo.floor() as i64 - TAPS_BEFORE as i64;
    let last = hi.floor() as i64 + TAPS_AFTER as i64;
    (first, (last - first + 1) as usize)
}

/// Resample `out.len() / channels` output frames: output frame `k` is the source signal at
/// position `pos + k * rate` (signed). `src` holds interleaved frames starting at absolute
/// frame `first_frame` ([`span_for`] says what to supply; anything outside reads as silence).
/// Returns the position after the block (`pos + frames * rate`).
pub fn varispeed_block(
    tables: &SincTables,
    src: &[f32],
    channels: usize,
    first_frame: i64,
    pos: f64,
    rate: f64,
    out: &mut [f32],
) -> f64 {
    let ch = channels.max(1);
    let frames = out.len() / ch;
    let table = tables.table(rate.abs());
    let src_frames = (src.len() / ch) as i64;
    for k in 0..frames {
        let p = pos + k as f64 * rate;
        let i = p.floor();
        let frac = p - i;
        let phase = frac * PHASES as f64;
        let pi = (phase.floor() as usize).min(PHASES - 1);
        let pf = (phase - pi as f64) as f32;
        let row0 = &table[pi * TAPS..(pi + 1) * TAPS];
        let row1 = &table[(pi + 1) * TAPS..(pi + 2) * TAPS];
        let base = i as i64 - TAPS_BEFORE as i64 - first_frame;
        let inside = base >= 0 && base + TAPS as i64 <= src_frames;
        for c in 0..ch {
            let (mut a, mut b) = (0.0f32, 0.0f32);
            if inside {
                let mut idx = base as usize * ch + c;
                for m in 0..TAPS {
                    let x = src[idx];
                    a += x * row0[m];
                    b += x * row1[m];
                    idx += ch;
                }
            } else {
                for m in 0..TAPS {
                    let f = base + m as i64;
                    if f >= 0 && f < src_frames {
                        let x = src[f as usize * ch + c];
                        a += x * row0[m];
                        b += x * row1[m];
                    }
                }
            }
            // pf == 0 reproduces row 0 exactly (the exact-delta row at integer positions).
            out[k * ch + c] = if pf == 0.0 { a } else { a + (b - a) * pf };
        }
    }
    pos + frames as f64 * rate
}

/// Largest `|rate|` the boxcar reader accepts (the scan's varispeed ceiling, WARP_17).
pub const MAX_BOXCAR_RATE: f64 = 8.0;

/// Frames of margin the boxcar reader needs around the positions it reads: before the first
/// and after the last source position, for averaging length `m` (cubic reaches one frame
/// before and two after, each average reaches `m / 2` back and `m` forward).
fn boxcar_margins(m: usize) -> (i64, i64) {
    ((m / 2 + 2) as i64, (m + 3) as i64)
}

/// Averaging length of the boxcar reader at `|rate|`: `round(|rate|)`, at least 2.
pub fn boxcar_len(rate_abs: f64) -> usize {
    (rate_abs.round() as usize).max(2)
}

/// Source span (`first`, `count`) the boxcar reader needs for `frames` output frames from
/// `pos` at `rate` (signed), analogous to [`span_for`].
pub fn boxcar_span_for(pos: f64, rate: f64, frames: usize) -> (i64, usize) {
    let last_pos = pos + rate * frames.saturating_sub(1) as f64;
    let (lo, hi) = if rate >= 0.0 {
        (pos, last_pos)
    } else {
        (last_pos, pos)
    };
    let (before, after) = boxcar_margins(boxcar_len(rate.abs()));
    let first = lo.floor() as i64 - before;
    let last = hi.floor() as i64 + after;
    (first, (last - first + 1) as usize)
}

/// Lo-fi fast reader for `|rate| > 2` (cassette scan): the source is averaged with a boxcar of
/// `round(|rate|)` frames (running sum through an f64 prefix, so the cost does not grow with the
/// rate) and sampled at the fractional position with a 4-point Catmull-Rom cubic. Output frame
/// `k` is the signal at `pos + k * rate`; `src` starts at absolute frame `first_frame`
/// ([`boxcar_span_for`]; anything outside reads as silence). `prefix` is scratch of at least
/// `src.len() / channels + 1` entries. Returns the position after the block.
pub fn boxcar_block(
    src: &[f32],
    channels: usize,
    first_frame: i64,
    pos: f64,
    rate: f64,
    out: &mut [f32],
    prefix: &mut [f64],
) -> f64 {
    let ch = channels.max(1);
    let frames = out.len() / ch;
    let n = src.len() / ch;
    let m = boxcar_len(rate.abs());
    let half = (m / 2) as i64;
    let inv = 1.0 / m as f64;
    debug_assert!(prefix.len() > n);
    for c in 0..ch {
        prefix[0] = 0.0;
        for i in 0..n {
            prefix[i + 1] = prefix[i] + f64::from(src[i * ch + c]);
        }
        // Boxcar-filtered sample at absolute integer frame `j`.
        let avg = |j: i64| -> f64 {
            let lo = (j - half - first_frame).clamp(0, n as i64) as usize;
            let hi = (j - half + m as i64 - first_frame).clamp(0, n as i64) as usize;
            (prefix[hi] - prefix[lo]) * inv
        };
        for k in 0..frames {
            let p = pos + k as f64 * rate;
            let i = p.floor();
            let t = p - i;
            let i = i as i64;
            let (y0, y1, y2, y3) = (avg(i - 1), avg(i), avg(i + 1), avg(i + 2));
            let v = y1
                + 0.5
                    * t
                    * (y2 - y0
                        + t * (2.0 * y0 - 5.0 * y1 + 4.0 * y2 - y3
                            + t * (3.0 * (y1 - y2) + y3 - y0)));
            out[k * ch + c] = v as f32;
        }
    }
    pos + frames as f64 * rate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{fft_peak_frequency, sine};

    #[test]
    fn table_selection_rounds_the_cutoff_up() {
        assert_eq!(SincTables::table_index(0.1), 0);
        assert_eq!(SincTables::table_index(1.0), 0);
        assert_eq!(SincTables::table_index(1.01), 1);
        assert_eq!(SincTables::table_index(1.125), 1);
        assert_eq!(SincTables::table_index(1.5), 4);
        assert_eq!(SincTables::table_index(2.0), 8);
        assert_eq!(SincTables::table_index(2.5), 8);
        assert!(SincTables::table_bytes() < 70_000);
    }

    #[test]
    fn rows_have_unit_dc_gain_and_phase_zero_is_an_exact_delta() {
        let t = SincTables::global();
        for tab in &t.tables {
            for p in 0..=PHASES {
                let s: f32 = tab[p * TAPS..(p + 1) * TAPS].iter().sum();
                assert!((s - 1.0).abs() < 1e-5, "row {p} sums to {s}");
            }
        }
        let row0 = &t.tables[0][..TAPS];
        assert_eq!(row0[TAPS_BEFORE], 1.0);
        assert_eq!(row0.iter().filter(|c| **c != 0.0).count(), 1);
    }

    #[test]
    fn integer_positions_reproduce_the_source_bit_exactly() {
        let src: Vec<f32> = (0..400)
            .map(|i| ((i * 37 % 101) as f32 - 50.0) / 50.0)
            .collect();
        let mut out = vec![0.0f32; 200];
        varispeed_block(SincTables::global(), &src, 1, 0, 100.0, 1.0, &mut out);
        for (k, v) in out.iter().enumerate() {
            assert_eq!(v.to_bits(), src[100 + k].to_bits(), "frame {k}");
        }
        // Reverse at -1.0 reads the same frames backwards.
        varispeed_block(SincTables::global(), &src, 1, 0, 300.0, -1.0, &mut out);
        for (k, v) in out.iter().enumerate() {
            assert_eq!(v.to_bits(), src[300 - k].to_bits(), "reverse frame {k}");
        }
    }

    #[test]
    fn half_speed_doubles_the_period_and_keeps_stereo_separate() {
        let rate = 44_100u32;
        let mono = sine(1000.0, rate, 44_100, 0.5);
        let src: Vec<f32> = mono.iter().flat_map(|s| [*s, -*s]).collect();
        let (first, n) = span_for(100.0, 0.5, 16_384);
        assert!(first < 100 && n > 8_000);
        let mut out = vec![0.0f32; 16_384 * 2];
        varispeed_block(SincTables::global(), &src, 2, 0, 100.0, 0.5, &mut out);
        let l: Vec<f32> = out.iter().step_by(2).copied().collect();
        let r: Vec<f32> = out.iter().skip(1).step_by(2).copied().collect();
        let f = fft_peak_frequency(&l, rate);
        assert!((f - 500.0).abs() < 2.0, "pitch {f}");
        assert!(
            l.iter().zip(&r).all(|(a, b)| (a + b).abs() < 1e-5),
            "channels stay independent"
        );
    }

    #[test]
    fn span_covers_the_taps_in_both_directions() {
        let (first, n) = span_for(1000.0, 2.0, 256);
        assert_eq!(first, 1000 - TAPS_BEFORE as i64);
        assert_eq!(n, 510 + TAPS);
        let (first, n) = span_for(1000.0, -1.0, 256);
        assert_eq!(first, 745 - TAPS_BEFORE as i64);
        assert_eq!(n, 255 + TAPS);
    }

    #[test]
    fn boxcar_reader_keeps_pitch_ratio_and_is_finite_at_4x_and_8x() {
        for (rate, tol) in [(4.0f64, 0.03), (8.0, 0.06)] {
            let x = sine(200.0, 48_000, 48_000, 0.5);
            let frames = 4_000;
            let (first, count) = boxcar_span_for(1000.0, rate, frames);
            let src: Vec<f32> = (first..first + count as i64)
                .map(|f| if f >= 0 && (f as usize) < x.len() { x[f as usize] } else { 0.0 })
                .collect();
            let mut out = vec![0.0f32; frames];
            let mut prefix = vec![0.0f64; count + 1];
            boxcar_block(&src, 1, first, 1000.0, rate, &mut out, &mut prefix);
            assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 0.51));
            let f = fft_peak_frequency(&out, 48_000);
            assert!((f / (200.0 * rate) - 1.0).abs() < tol, "rate {rate}: {f}");
        }
    }
}
