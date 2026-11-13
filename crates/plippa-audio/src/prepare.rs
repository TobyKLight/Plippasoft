//! Prepare decoded audio against board capabilities (downsample or refuse).

use std::path::Path;

use plippa_core::Capabilities;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

use crate::decode::{decode, probe, AudioProbe, PcmBuffer};
use crate::error::AudioError;

const RESAMPLE_CHUNK: usize = 1024;

/// What to do when a file exceeds `Capabilities` max rate / bit depth.
///
/// Noubin §3.3 allows either correct downsample (when the DAC cannot take the
/// source rate/depth) or a clear pre-playback refusal. Default for Plippa MVP
/// is **downsample** so Bandcamp 24-bit / high-rate dumps still play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverspecPolicy {
    /// Resample to `max_sample_rate_hz` and play as f32 (bit depth becomes
    /// sink/DAC concern; we never feed overspec integer PCM into a narrow DAC).
    #[default]
    Downsample,
    /// Refuse before playback with a Noubin-style clear error string.
    Refuse,
}

pub fn exceeds_capabilities(probe: &AudioProbe, caps: &Capabilities) -> bool {
    probe.sample_rate_hz > caps.max_sample_rate_hz || probe.bit_depth > caps.max_bit_depth
}

fn format_label(ext: &str) -> &str {
    match ext {
        "flac" => "FLAC",
        "wav" => "WAV",
        "mp3" => "MP3",
        other => other,
    }
}

pub fn quality_exceeds_error(
    path: &Path,
    probe: &AudioProbe,
    caps: &Capabilities,
) -> AudioError {
    AudioError::QualityExceedsCapabilities {
        path: path.to_path_buf(),
        format: format_label(&probe.extension).to_string(),
        file_rate: probe.sample_rate_hz,
        file_bits: probe.bit_depth,
        max_rate: caps.max_sample_rate_hz,
        max_bits: caps.max_bit_depth,
    }
}

/// Whole-buffer decode + optional in-process resample.
///
/// **Not for the Play/tap hot path** (see [`crate::stream::StreamDecoder`]).
/// Kept for short assets, offline tooling, and unit tests of the refuse /
/// downsample policies. Live playback opens a native-rate stream and lets
/// PipeWire convert; `max_sample_rate_hz` is a capability ceiling for
/// [`OverspecPolicy::Refuse`], not a forced convert-everything target.
pub fn prepare_for_playback(
    path: &Path,
    capabilities: &Capabilities,
    policy: OverspecPolicy,
) -> Result<PcmBuffer, AudioError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .ok_or_else(|| AudioError::UnsupportedFormat {
            path: path.to_path_buf(),
        })?;
    if !crate::formats::format_enabled(capabilities, &ext) {
        return Err(AudioError::FormatNotEnabled {
            path: path.to_path_buf(),
            ext,
        });
    }

    let meta = probe(path)?;
    if exceeds_capabilities(&meta, capabilities) {
        match policy {
            OverspecPolicy::Refuse => {
                return Err(quality_exceeds_error(path, &meta, capabilities));
            }
            OverspecPolicy::Downsample => {
                // Fall through: decode then resample.
            }
        }
    }

    let mut pcm = decode(path)?;
    if pcm.sample_rate_hz != capabilities.max_sample_rate_hz {
        pcm = resample_pcm(pcm, capabilities.max_sample_rate_hz)?;
    }
    // Bit depth: we already decode to f32; refusing is the only bit-depth gate
    // for OverspecPolicy::Refuse (handled above). Downsample path keeps f32.
    Ok(pcm)
}

fn resample_pcm(input: PcmBuffer, to_rate: u32) -> Result<PcmBuffer, AudioError> {
    if input.sample_rate_hz == to_rate || input.samples.is_empty() {
        return Ok(input);
    }
    let channels = input.channels as usize;
    if channels == 0 {
        return Err(AudioError::InvalidParam("zero channels".into()));
    }
    // Upsample (e.g. CD 44.1 → 48 kHz): linear is fine for MVP and far cheaper / leaner
    // than rubato sinc on a 1GB Pi (full-track sinc was pegging CPU for minutes).
    if input.sample_rate_hz < to_rate {
        return resample_linear(input, to_rate);
    }
    resample_sinc_downsample(input, to_rate)
}

/// Interleaved linear resample (good enough for mild rate changes; low RAM peak).
fn resample_linear(input: PcmBuffer, to_rate: u32) -> Result<PcmBuffer, AudioError> {
    let channels = input.channels as usize;
    let in_frames = input.samples.len() / channels;
    let out_frames = ((in_frames as u64)
        .saturating_mul(u64::from(to_rate))
        .saturating_div(u64::from(input.sample_rate_hz))
        .max(1)) as usize;
    let mut samples = vec![0.0f32; out_frames.saturating_mul(channels)];
    let pos_step = f64::from(input.sample_rate_hz) / f64::from(to_rate);
    for (of, chunk) in samples.chunks_exact_mut(channels).enumerate() {
        let src = of as f64 * pos_step;
        let i0 = (src.floor() as usize).min(in_frames.saturating_sub(1));
        let i1 = (i0 + 1).min(in_frames.saturating_sub(1));
        let t = (src - i0 as f64) as f32;
        let base0 = i0 * channels;
        let base1 = i1 * channels;
        for (c, sample) in chunk.iter_mut().enumerate() {
            let a = input.samples[base0 + c];
            let b = input.samples[base1 + c];
            *sample = a + (b - a) * t;
        }
    }
    Ok(PcmBuffer {
        samples,
        channels: input.channels,
        sample_rate_hz: to_rate,
    })
}

fn resample_sinc_downsample(input: PcmBuffer, to_rate: u32) -> Result<PcmBuffer, AudioError> {
    let channels = input.channels as usize;
    let in_frames = input.samples.len() / channels;
    let mut per_channel = vec![Vec::with_capacity(in_frames); channels];
    for frame in 0..in_frames {
        for ch in 0..channels {
            per_channel[ch].push(input.samples[frame * channels + ch]);
        }
    }

    let ratio = f64::from(to_rate) / f64::from(input.sample_rate_hz);
    let params = SincInterpolationParameters {
        sinc_len: 64,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
    };
    let mut resampler = SincFixedIn::<f32>::new(ratio, 2.0, params, RESAMPLE_CHUNK, channels)
        .map_err(|e| AudioError::Resample(e.to_string()))?;

    let mut out_channels = vec![Vec::new(); channels];
    let mut offset = 0usize;
    while offset + RESAMPLE_CHUNK <= in_frames {
        let chunk: Vec<&[f32]> = per_channel
            .iter()
            .map(|ch| &ch[offset..offset + RESAMPLE_CHUNK])
            .collect();
        let out_len = resampler.output_frames_next();
        let mut out_chunk = vec![vec![0.0f32; out_len]; channels];
        resampler
            .process_into_buffer(&chunk, &mut out_chunk, None)
            .map_err(|e| AudioError::Resample(e.to_string()))?;
        for ch in 0..channels {
            out_channels[ch].extend_from_slice(&out_chunk[ch]);
        }
        offset += RESAMPLE_CHUNK;
    }

    // Flush a final partial chunk (silence-padded) so short fixtures fully convert.
    if offset < in_frames {
        let mut padded = vec![vec![0.0f32; RESAMPLE_CHUNK]; channels];
        let rem = in_frames - offset;
        for ch in 0..channels {
            padded[ch][..rem].copy_from_slice(&per_channel[ch][offset..]);
        }
        let chunk: Vec<&[f32]> = padded.iter().map(|ch| ch.as_slice()).collect();
        let out_len = resampler.output_frames_next();
        let mut out_chunk = vec![vec![0.0f32; out_len]; channels];
        resampler
            .process_into_buffer(&chunk, &mut out_chunk, None)
            .map_err(|e| AudioError::Resample(e.to_string()))?;
        // Keep only the proportion corresponding to real input frames.
        let keep = ((rem as f64) * ratio).round() as usize;
        for ch in 0..channels {
            let end = keep.min(out_chunk[ch].len());
            out_channels[ch].extend_from_slice(&out_chunk[ch][..end]);
        }
    }

    let out_frames = out_channels.first().map(|ch| ch.len()).unwrap_or(0);
    let mut samples = vec![0.0f32; out_frames * channels];
    for frame in 0..out_frames {
        for ch in 0..channels {
            samples[frame * channels + ch] = out_channels[ch][frame];
        }
    }

    Ok(PcmBuffer {
        samples,
        channels: input.channels,
        sample_rate_hz: to_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_core::Capabilities;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn refuse_overspec_has_clear_message() {
        let caps = Capabilities::desktop_mock();
        let path = fixture("overspec_192k_24.wav");
        let err = prepare_for_playback(&path, &caps, OverspecPolicy::Refuse)
            .expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("192000") || msg.contains("192"), "{msg}");
        assert!(msg.contains("24"), "{msg}");
        assert!(msg.contains("48000") || msg.contains("48"), "{msg}");
        assert!(msg.contains("16"), "{msg}");
    }

    #[test]
    fn downsample_overspec_to_48k() {
        let caps = Capabilities::desktop_mock();
        let path = fixture("overspec_192k_24.wav");
        let pcm = prepare_for_playback(&path, &caps, OverspecPolicy::Downsample)
            .expect("downsample");
        assert_eq!(pcm.sample_rate_hz, 48_000);
        assert_eq!(pcm.channels, 2);
        assert!(pcm.frame_count() > 100);
        assert!(pcm.samples.iter().any(|s| s.abs() > 0.01));
    }

    #[test]
    fn upsample_44100_to_48k() {
        // Synthetic CD-rate stereo tone → device rate (mixer/Pulse are 48 kHz).
        let frames = 4410; // 100 ms
        let mut samples = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let t = i as f32 / 44_100.0;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.2;
            samples.push(s);
            samples.push(s);
        }
        let pcm = resample_pcm(
            PcmBuffer {
                samples,
                channels: 2,
                sample_rate_hz: 44_100,
            },
            48_000,
        )
        .expect("upsample");
        assert_eq!(pcm.sample_rate_hz, 48_000);
        assert_eq!(pcm.channels, 2);
        // ~100 ms at 48 kHz ≈ 4800 frames (± pad/round).
        assert!((4500..5200).contains(&pcm.frame_count()));
        assert!(pcm.samples.iter().any(|s| s.abs() > 0.01));
    }
}
