//! In-tree WSOLA time-stretcher (WARP_09): the [`Stretcher`](crate::stretch::Stretcher)
//! implementation used when the `stretch` cargo feature (Signalsmith, C++) is off. Plain Rust,
//! no C++ toolchain. Quality is below Signalsmith (it is a fallback); the numeric properties
//! (pitch kept, duration `source / rate`) hold within looser tolerances.
//!
//! Streaming WSOLA: output frames of `n` samples (Hann, 50 % overlap) are taken from the input
//! at a nominal analysis position that advances `hs x rate` per output hop; around the nominal
//! position the start with the best (decimated, normalised) cross-correlation against the
//! natural continuation of the previous frame is chosen (`+-d` search). Input is buffered, so
//! the voice sees a fixed start-up latency exactly like the Signalsmith one; priming with past
//! audio makes the first output the audio `lag_frames` before the end of the past.

use crate::stretch::{StretchConfig, Stretcher};
use std::collections::VecDeque;

/// WSOLA stretcher; window and search sizes come from [`StretchConfig`] (milliseconds).
pub struct WsolaStretcher {
    ch: usize,
    n: usize,
    hs: usize,
    d: usize,
    win: Vec<f32>,
    /// Buffered input (`in_len` frames), absolute index of its first frame = `in_base`.
    inbuf: Vec<f32>,
    in_base: i64,
    in_len: usize,
    /// Nominal analysis start of the next frame (absolute input frames).
    a_nom: f64,
    prev_start: Option<i64>,
    /// The first frame after a prime only fills the overlap buffer (its output is dropped).
    warm: bool,
    ola: Vec<f32>,
    outq: VecDeque<f32>,
    tmpl: Vec<f32>,
    cand: Vec<f32>,
}

impl WsolaStretcher {
    pub fn new(cfg: &StretchConfig, channels: usize, rate_hz: u32) -> Self {
        let ch = channels.max(1);
        let ms = |v: f64| ((v * f64::from(rate_hz) / 1000.0).round() as usize).max(8);
        let n = (ms(cfg.wsola_window_ms) / 2).max(4) * 2;
        let hs = n / 2;
        let d = ms(cfg.wsola_search_ms);
        let win = (0..n)
            .map(|k| 0.5 * (1.0 - (2.0 * std::f64::consts::PI * k as f64 / n as f64).cos()) as f32)
            .collect();
        let cap = (n + 2 * d + 2 * hs) * 8;
        Self {
            ch,
            n,
            hs,
            d,
            win,
            inbuf: vec![0.0; cap * ch],
            in_base: 0,
            in_len: 0,
            a_nom: 0.0,
            prev_start: None,
            warm: false,
            ola: vec![0.0; n * ch],
            outq: VecDeque::with_capacity((n * 4 + 8192) * ch),
            tmpl: vec![0.0; hs],
            cand: vec![0.0; 2 * d + 1 + hs],
        }
    }

    fn in_end(&self) -> i64 {
        self.in_base + self.in_len as i64
    }

    fn mono_at(&self, abs: i64) -> f32 {
        let i = (abs - self.in_base) as usize * self.ch;
        let mut s = 0.0;
        for c in 0..self.ch {
            s += self.inbuf[i + c];
        }
        s / self.ch as f32
    }

    /// Append input, dropping what can no longer be needed (or growing if it must).
    fn append(&mut self, input: &[f32]) {
        let ch = self.ch;
        let frames = input.len() / ch;
        let keep_from = {
            let nominal = self.a_nom.round() as i64 - self.d as i64;
            let tmpl = self.prev_start.map_or(nominal, |p| p + self.hs as i64);
            nominal.min(tmpl).max(self.in_base)
        };
        if (self.in_len + frames) * ch > self.inbuf.len() {
            let drop = ((keep_from - self.in_base) as usize).min(self.in_len);
            if drop > 0 {
                self.inbuf.copy_within(drop * ch..self.in_len * ch, 0);
                self.in_base += drop as i64;
                self.in_len -= drop;
            }
            if (self.in_len + frames) * ch > self.inbuf.len() {
                self.inbuf.resize((self.in_len + frames) * ch * 2, 0.0);
            }
        }
        self.inbuf[self.in_len * ch..(self.in_len + frames) * ch].copy_from_slice(input);
        self.in_len += frames;
    }

    /// Choose the start of the next frame around `s_nom`.
    fn best_start(&mut self, s_nom: i64) -> i64 {
        let (hs, d) = (self.hs, self.d as i64);
        let Some(prev) = self.prev_start else {
            return s_nom;
        };
        let lo = s_nom - d;
        let span = 2 * d as usize + 1 + hs;
        for i in 0..hs {
            self.tmpl[i] = self.mono_at(prev + hs as i64 + i as i64);
        }
        for i in 0..span {
            self.cand[i] = self.mono_at(lo + i as i64);
        }
        let score = |cand: &[f32], tmpl: &[f32], off: usize| -> f32 {
            let (mut num, mut en) = (0.0f32, 1e-9f32);
            let mut i = 0;
            while i < tmpl.len() {
                let x = cand[off + i];
                num += tmpl[i] * x;
                en += x * x;
                i += 2;
            }
            num / en.sqrt()
        };
        // Coarse scan from the centre outwards, so on a tie (silence) the nominal position wins.
        let centre = d as usize;
        let mut best = (score(&self.cand, &self.tmpl, centre), centre);
        let mut k = 4usize;
        while k <= centre {
            for off in [centre - k, centre + k] {
                let sc = score(&self.cand, &self.tmpl, off);
                if sc > best.0 {
                    best = (sc, off);
                }
            }
            k += 4;
        }
        let (c0, lo_f, hi_f) = (
            best.1,
            best.1.saturating_sub(3),
            (best.1 + 3).min(2 * d as usize),
        );
        let mut best_off = c0;
        let mut best_sc = best.0;
        for off in lo_f..=hi_f {
            let sc = score(&self.cand, &self.tmpl, off);
            if sc > best_sc {
                best_sc = sc;
                best_off = off;
            }
        }
        lo + best_off as i64
    }

    /// Produce one synthesis hop if the input it needs is buffered.
    fn try_frame(&mut self, rate: f64) -> bool {
        let s_nom = self.a_nom.round() as i64;
        if s_nom + self.d as i64 + self.n as i64 > self.in_end() {
            return false;
        }
        let s = self.best_start(s_nom);
        let ch = self.ch;
        let base = ((s - self.in_base) as usize) * ch;
        for k in 0..self.n {
            let w = self.win[k];
            for c in 0..ch {
                self.ola[k * ch + c] += w * self.inbuf[base + k * ch + c];
            }
        }
        if self.warm {
            self.outq.extend(self.ola[..self.hs * ch].iter().copied());
        }
        self.warm = true;
        self.ola.copy_within(self.hs * ch.., 0);
        let tail = (self.n - self.hs) * ch;
        self.ola[tail..].fill(0.0);
        self.prev_start = Some(s);
        self.a_nom += self.hs as f64 * rate;
        true
    }
}

impl Stretcher for WsolaStretcher {
    fn process(&mut self, input: &[f32], out: &mut [f32]) {
        let ch = self.ch;
        let out_frames = out.len() / ch;
        let in_frames = input.len() / ch;
        if out_frames == 0 {
            return;
        }
        self.append(input);
        let rate = (in_frames as f64 / out_frames as f64).clamp(0.05, 4.0);
        while self.outq.len() < out.len() {
            if !self.try_frame(rate) {
                break;
            }
        }
        for o in out.iter_mut() {
            *o = self.outq.pop_front().unwrap_or(0.0);
        }
    }

    fn reset_prime(&mut self, past: &[f32], _rate: f64) {
        let ch = self.ch;
        self.inbuf.iter_mut().for_each(|s| *s = 0.0);
        self.in_base = 0;
        self.in_len = 0;
        self.outq.clear();
        self.ola.fill(0.0);
        self.prev_start = None;
        self.warm = false;
        let need = self.prime_frames();
        let have = past.len() / ch;
        // Always exactly `need` frames of past (zero-padded in front if shorter).
        let pad = need.saturating_sub(have);
        let skip = have.saturating_sub(need);
        if (self.in_len + need) * ch > self.inbuf.len() {
            self.inbuf.resize((need + self.n) * ch * 8, 0.0);
        }
        self.inbuf[..pad * ch].fill(0.0);
        self.inbuf[pad * ch..need * ch].copy_from_slice(&past[skip * ch..]);
        self.in_len = need;
        self.a_nom = need as f64 - (self.d + self.n + 2 * self.hs) as f64;
    }

    fn flush(&mut self, out: &mut [f32]) {
        // Zero input at a unity analysis hop drains what is buffered (rare path: may allocate).
        while self.outq.len() < out.len() {
            if !self.try_frame(1.0) {
                let zeros = vec![0.0f32; self.n * self.ch];
                self.append(&zeros);
            }
        }
        for o in out.iter_mut() {
            *o = self.outq.pop_front().unwrap_or(0.0);
        }
    }

    fn latency(&self) -> (usize, usize) {
        (self.n + self.d + self.hs, self.hs)
    }

    fn lag_frames(&self, rate: f64) -> f64 {
        (self.n + self.d + 2 * self.hs) as f64 - self.hs as f64 * rate
    }

    fn prime_frames(&self) -> usize {
        self.n + 2 * self.d + 2 * self.hs
    }
}
