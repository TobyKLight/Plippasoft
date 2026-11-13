//! Main media + one-shot SFX mixer.

use crate::decode::PcmBuffer;
use crate::volume::Volume;

/// Simple two-layer mixer: a looping/advancing main stream plus one-shot SFX.
///
/// Intended to live on the dedicated audio thread (`plippa-engine` MVP_05).
/// Not internally synchronized — do not call `render` / `trigger_sfx` from
/// multiple threads without external coordination. Prefer queueing SFX from
/// the control plane via a lock-free channel into the audio thread.
#[derive(Debug)]
pub struct Mixer {
    channels: u16,
    sample_rate_hz: u32,
    main: Option<MainPlayhead>,
    sfx: Option<SfxPlayhead>,
    /// Relative gain of SFX vs main (1.0 = same level).
    pub sfx_gain: f32,
    pub volume: Volume,
}

#[derive(Debug)]
struct MainPlayhead {
    samples: Vec<f32>,
    frame: usize,
    finished: bool,
}

#[derive(Debug)]
struct SfxPlayhead {
    samples: Vec<f32>,
    frame: usize,
}

impl Mixer {
    pub fn new(channels: u16, sample_rate_hz: u32) -> Self {
        Self {
            channels,
            sample_rate_hz,
            main: None,
            sfx: None,
            sfx_gain: 0.5,
            volume: Volume::default(),
        }
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    /// Replace the main media stream (resets playhead). Channel/rate must match.
    /// Mismatched buffers are ignored (logged) so a bad prepare never kills the audio thread.
    pub fn set_main(&mut self, pcm: PcmBuffer) {
        if pcm.channels != self.channels || pcm.sample_rate_hz != self.sample_rate_hz {
            eprintln!(
                "plippa-audio: set_main rejected (got {}ch/{}Hz, mixer {}ch/{}Hz)",
                pcm.channels, pcm.sample_rate_hz, self.channels, self.sample_rate_hz
            );
            return;
        }
        self.main = Some(MainPlayhead {
            samples: pcm.samples,
            frame: 0,
            finished: false,
        });
    }

    /// Clear main media (silence until next `set_main`).
    pub fn clear_main(&mut self) {
        self.main = None;
    }

    /// Fire a one-shot SFX (replaces any in-progress SFX).
    pub fn trigger_sfx(&mut self, pcm: PcmBuffer) {
        if pcm.channels != self.channels || pcm.sample_rate_hz != self.sample_rate_hz {
            eprintln!(
                "plippa-audio: trigger_sfx rejected (got {}ch/{}Hz, mixer {}ch/{}Hz)",
                pcm.channels, pcm.sample_rate_hz, self.channels, self.sample_rate_hz
            );
            return;
        }
        self.sfx = Some(SfxPlayhead {
            samples: pcm.samples,
            frame: 0,
        });
    }

    pub fn main_finished(&self) -> bool {
        self.main.as_ref().map(|m| m.finished).unwrap_or(true)
    }

    /// Render `out_frames` of interleaved PCM into `out` (must be exactly
    /// `out_frames * channels` long).
    pub fn render(&mut self, out: &mut [f32]) {
        let ch = self.channels as usize;
        assert_eq!(out.len() % ch, 0);
        let out_frames = out.len() / ch;
        out.fill(0.0);

        if let Some(main) = self.main.as_mut() {
            let main_frames = main.samples.len() / ch;
            for frame in 0..out_frames {
                if main.frame >= main_frames {
                    main.finished = true;
                    break;
                }
                let src = main.frame * ch;
                let dst = frame * ch;
                out[dst..dst + ch].copy_from_slice(&main.samples[src..src + ch]);
                main.frame += 1;
            }
            if main.frame >= main_frames {
                main.finished = true;
            }
        }

        if let Some(sfx) = self.sfx.as_mut() {
            let sfx_frames = sfx.samples.len() / ch;
            let gain = self.sfx_gain;
            let mut done = false;
            for frame in 0..out_frames {
                if sfx.frame >= sfx_frames {
                    done = true;
                    break;
                }
                let src = sfx.frame * ch;
                let dst = frame * ch;
                for c in 0..ch {
                    out[dst + c] += sfx.samples[src + c] * gain;
                }
                sfx.frame += 1;
            }
            if done || sfx.frame >= sfx_frames {
                self.sfx = None;
            }
        }

        // Soft clip to reduce SFX+main overflow noise.
        for s in out.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }

        self.volume.apply(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(channels: u16, frames: usize, amp: f32) -> PcmBuffer {
        let mut samples = Vec::with_capacity(frames * channels as usize);
        for i in 0..frames {
            let s = if i % 2 == 0 { amp } else { -amp };
            for _ in 0..channels {
                samples.push(s);
            }
        }
        PcmBuffer {
            samples,
            channels,
            sample_rate_hz: 48_000,
        }
    }

    #[test]
    fn sfx_alters_output_vs_main_only() {
        let mut mixer = Mixer::new(2, 48_000);
        mixer.set_main(tone(2, 64, 0.1));
        let mut main_only = vec![0.0f32; 64 * 2];
        mixer.render(&mut main_only);

        let mut mixer2 = Mixer::new(2, 48_000);
        mixer2.sfx_gain = 1.0;
        mixer2.set_main(tone(2, 64, 0.1));
        mixer2.trigger_sfx(tone(2, 32, 0.5));
        let mut with_sfx = vec![0.0f32; 64 * 2];
        mixer2.render(&mut with_sfx);

        assert_ne!(main_only, with_sfx);
        assert!(with_sfx[..64].iter().any(|s| s.abs() > 0.2));
    }

    #[test]
    fn volume_zero_on_mixer_is_silence() {
        let mut mixer = Mixer::new(2, 48_000);
        mixer.volume.set(0.0);
        mixer.set_main(tone(2, 32, 0.8));
        let mut out = vec![1.0f32; 32 * 2];
        mixer.render(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
    }
}
