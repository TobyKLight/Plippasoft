//! Synthetic test signals (mono f32, generated on demand) and a minimal WAV writer.
//! Signals are chosen so that warp artefacts are obvious: a pure tone (pitch, clicks), a log chirp
//! (aliasing/birdies), a click train (transient smear, pre-echo), a stepped sweep (zipper, level).

use anyhow::{anyhow, bail, Context, Result};
use std::f64::consts::PI;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Tone,
    Chirp,
    Clicks,
    Sweep,
    Music,
}

impl Kind {
    pub fn parse(s: &str) -> Result<Kind> {
        Ok(match s {
            "tone" => Kind::Tone,
            "chirp" => Kind::Chirp,
            "clicks" => Kind::Clicks,
            "sweep" => Kind::Sweep,
            "music" => Kind::Music,
            _ => bail!("unknown signal '{s}' (tone|chirp|clicks|sweep|music)"),
        })
    }
}

pub const TONE_HZ: f64 = 1000.0;
pub const CHIRP_F0: f64 = 100.0;
pub const CHIRP_F1: f64 = 18000.0;
pub const CLICK_PERIOD_S: f64 = 0.1;
/// Period of one chirp / sweep repetition when a longer signal is requested.
pub const SWEEP_PERIOD_S: f64 = 10.0;

/// Generate `seconds` of the signal as mono f32 at `rate` Hz (peak about 0.5 unless noted).
pub fn generate(kind: Kind, seconds: f64, rate: u32) -> Vec<f32> {
    let n = (seconds * rate as f64).round().max(0.0) as usize;
    let sr = rate as f64;
    let mut out = vec![0.0f32; n];
    match kind {
        Kind::Tone => {
            for (i, o) in out.iter_mut().enumerate() {
                *o = (0.5 * (2.0 * PI * TONE_HZ * i as f64 / sr).sin()) as f32;
            }
        }
        Kind::Chirp => {
            let period = SWEEP_PERIOD_S.min(seconds.max(0.1));
            let ratio = CHIRP_F1 / CHIRP_F0;
            let k = 2.0 * PI * CHIRP_F0 * period / ratio.ln();
            for (i, o) in out.iter_mut().enumerate() {
                let t = (i as f64 / sr) % period;
                let phase = k * (ratio.powf(t / period) - 1.0);
                // short fade at the repetition seam so the loop itself does not click
                let seam = (t / 0.01)
                    .min(1.0)
                    .min(((period - t) / 0.01).max(0.0))
                    .min(1.0);
                *o = (0.5 * phase.sin() * seam) as f32;
            }
        }
        Kind::Clicks => {
            let period = (CLICK_PERIOD_S * sr).round() as usize;
            let pulse = [0.3f32, 0.8, 0.8, 0.3];
            let mut i = 0;
            while i < n {
                for (j, p) in pulse.iter().enumerate() {
                    if i + j < n {
                        out[i + j] = *p;
                    }
                }
                i += period.max(8);
            }
        }
        Kind::Sweep => {
            let steps = 24usize;
            let period = SWEEP_PERIOD_S.min(seconds.max(0.1));
            let step_s = period / steps as f64;
            let (f0, f1) = (100.0f64, 16000.0f64);
            let mut phase = 0.0f64;
            for (i, o) in out.iter_mut().enumerate() {
                let t = (i as f64 / sr) % period;
                let idx = ((t / step_s) as usize).min(steps - 1);
                let f = f0 * (f1 / f0).powf(idx as f64 / (steps - 1) as f64);
                let in_step = t - idx as f64 * step_s;
                let env = (in_step / 0.01)
                    .min(1.0)
                    .min(((step_s - in_step) / 0.01).max(0.0))
                    .min(1.0);
                phase += 2.0 * PI * f / sr;
                *o = (0.5 * phase.sin() * env) as f32;
            }
        }
        Kind::Music => {
            // Deterministic "music-like" material: a slow chord progression of detuned saw+sine voices
            // with a decaying noise hit every 0.5 s. Broadband enough to exercise the DSP realistically.
            let mut rng = Lcg::new(0x5eed_1234);
            let chords: [[f64; 3]; 4] = [
                [220.0, 277.18, 329.63],
                [196.0, 246.94, 293.66],
                [174.61, 220.0, 261.63],
                [196.0, 246.94, 392.0],
            ];
            for (i, o) in out.iter_mut().enumerate() {
                let t = i as f64 / sr;
                let chord = chords[((t / 2.0) as usize) % chords.len()];
                let mut s = 0.0;
                for (v, f) in chord.iter().enumerate() {
                    let det = 1.0 + 0.002 * (v as f64 - 1.0);
                    let ph = (t * f * det) % 1.0;
                    s += 0.10 * (2.0 * ph - 1.0) + 0.08 * (2.0 * PI * f * t).sin();
                }
                let hit_t = t % 0.5;
                let env = (-hit_t * 18.0).exp();
                s += 0.25 * env * rng.next_f64_signed();
                *o = s.clamp(-1.0, 1.0) as f32;
            }
        }
    }
    out
}

/// Small deterministic generator (no `rand` dependency).
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Lcg(seed | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn next_f64_signed(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// Replicate a mono signal into `ch` interleaved channels.
pub fn to_channels(mono: &[f32], ch: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(mono.len() * ch);
    for &s in mono {
        for _ in 0..ch {
            out.push(s);
        }
    }
    out
}

/// A short sine beep with a raised-cosine envelope, mono.
pub fn beep(rate: u32, ms: f64, freq: f64, gain: f32) -> Vec<f32> {
    let n = (ms / 1000.0 * rate as f64).round() as usize;
    (0..n)
        .map(|i| {
            let env = 0.5 - 0.5 * (2.0 * PI * i as f64 / n.max(1) as f64).cos();
            gain * env as f32 * (2.0 * PI * freq * i as f64 / rate as f64).sin() as f32
        })
        .collect()
}

/// Magnitude of the DFT of `signal` at `freq` (Goertzel); used by tests to find a tone's pitch.
#[cfg(test)]
pub fn goertzel(signal: &[f32], rate: f64, freq: f64) -> f64 {
    let w = 2.0 * PI * freq / rate;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f64, 0.0f64);
    for &x in signal {
        let s0 = x as f64 + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).max(0.0).sqrt()
}

/// Streaming 16-bit PCM WAV writer (header patched on `finish`).
pub struct WavWriter {
    w: BufWriter<File>,
    ch: u16,
    data_bytes: u64,
}

impl WavWriter {
    pub fn create(path: &Path, ch: u16, rate: u32) -> Result<Self> {
        let f = File::create(path).with_context(|| format!("create {}", path.display()))?;
        let mut w = BufWriter::new(f);
        w.write_all(b"RIFF")?;
        w.write_all(&0u32.to_le_bytes())?;
        w.write_all(b"WAVEfmt ")?;
        w.write_all(&16u32.to_le_bytes())?;
        w.write_all(&1u16.to_le_bytes())?; // PCM
        w.write_all(&ch.to_le_bytes())?;
        w.write_all(&rate.to_le_bytes())?;
        w.write_all(&(rate * ch as u32 * 2).to_le_bytes())?;
        w.write_all(&(ch * 2).to_le_bytes())?;
        w.write_all(&16u16.to_le_bytes())?;
        w.write_all(b"data")?;
        w.write_all(&0u32.to_le_bytes())?;
        Ok(WavWriter {
            w,
            ch,
            data_bytes: 0,
        })
    }

    pub fn write(&mut self, interleaved: &[f32]) -> Result<()> {
        debug_assert_eq!(interleaved.len() % self.ch as usize, 0);
        for &s in interleaved {
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            self.w.write_all(&v.to_le_bytes())?;
        }
        self.data_bytes += interleaved.len() as u64 * 2;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.w.flush()?;
        let mut f = self.w.into_inner().map_err(|e| anyhow!("flush wav: {e}"))?;
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&((36 + self.data_bytes) as u32).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&(self.data_bytes as u32).to_le_bytes())?;
        Ok(())
    }
}

/// `gen <kind> <out.wav>` helper.
pub fn write_signal_wav(path: &Path, kind: Kind, seconds: f64, rate: u32, ch: u16) -> Result<()> {
    let mono = generate(kind, seconds, rate);
    let inter = to_channels(&mono, ch as usize);
    let mut w = WavWriter::create(path, ch, rate)?;
    w.write(&inter)?;
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Peak of the Goertzel magnitude over a fine grid around `centre`; returns the best frequency.
    fn peak_freq(sig: &[f32], rate: f64, centre: f64, span: f64, step: f64) -> f64 {
        let mut best = (0.0, centre);
        let mut f = centre - span;
        while f <= centre + span {
            let m = goertzel(sig, rate, f);
            if m > best.0 {
                best = (m, f);
            }
            f += step;
        }
        best.1
    }

    #[test]
    fn tone_peaks_at_1khz_within_0_1_percent() {
        let sig = generate(Kind::Tone, 2.0, 44100);
        let f = peak_freq(&sig, 44100.0, 1000.0, 20.0, 0.25);
        assert!((f - 1000.0).abs() <= 1.0, "peak at {f}");
        // and nothing comparable elsewhere
        assert!(goertzel(&sig, 44100.0, 2000.0) < 0.01 * goertzel(&sig, 44100.0, 1000.0));
    }

    #[test]
    fn chirp_is_bounded_and_covers_the_band() {
        let sig = generate(Kind::Chirp, 10.0, 44100);
        assert_eq!(sig.len(), 441_000);
        assert!(sig.iter().all(|s| s.is_finite() && s.abs() <= 0.5001));
        // early part has 100-300 Hz energy, late part has 10-18 kHz energy
        let early = &sig[..44100 / 4];
        let late = &sig[sig.len() - 44100 / 4..];
        assert!(goertzel(early, 44100.0, 130.0) > 10.0 * goertzel(early, 44100.0, 12000.0));
        assert!(goertzel(late, 44100.0, 16000.0) > goertzel(late, 44100.0, 130.0));
    }

    #[test]
    fn clicks_repeat_every_100_ms() {
        let sig = generate(Kind::Clicks, 1.0, 44100);
        let onsets: Vec<usize> = (0..sig.len())
            .filter(|&i| sig[i] > 0.1 && (i == 0 || sig[i - 1] <= 0.1))
            .collect();
        assert_eq!(onsets.len(), 10);
        for w in onsets.windows(2) {
            assert_eq!(w[1] - w[0], 4410);
        }
    }

    #[test]
    fn sweep_and_music_are_finite_and_nonsilent() {
        for k in [Kind::Sweep, Kind::Music] {
            let sig = generate(k, 3.0, 48000);
            assert!(sig.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
            assert!(sig.iter().map(|s| s * s).sum::<f32>() > 1.0);
        }
    }

    #[test]
    fn channels_and_beep_shapes() {
        assert_eq!(to_channels(&[1.0, 2.0], 2), vec![1.0, 1.0, 2.0, 2.0]);
        let b = beep(48000, 100.0, 880.0, 0.25);
        assert_eq!(b.len(), 4800);
        assert!(b.iter().all(|s| s.abs() <= 0.25));
        assert!(Kind::parse("nope").is_err());
        assert_eq!(Kind::parse("sweep").unwrap(), Kind::Sweep);
    }

    #[test]
    fn wav_header_is_consistent() {
        let dir = std::env::temp_dir().join(format!("warp_bench_wav_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        write_signal_wav(&p, Kind::Tone, 0.5, 44100, 2).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        assert_eq!(data_len, 22050 * 2 * 2);
        assert_eq!(bytes.len(), 44 + data_len);
        let riff = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff, bytes.len() - 8);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
