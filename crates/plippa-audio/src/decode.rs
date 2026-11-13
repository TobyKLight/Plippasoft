//! Probe and decode via Symphonia (WAV / MP3 / FLAC).

use std::path::Path;

use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::conv::FromSample;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::sample::Sample;

use crate::error::AudioError;
use crate::stream::{file_opener, probe_gapless_first};

/// Interleaved f32 PCM in [-1.0, 1.0].
#[derive(Debug, Clone)]
pub struct PcmBuffer {
    pub samples: Vec<f32>,
    pub channels: u16,
    pub sample_rate_hz: u32,
}

impl PcmBuffer {
    pub fn frame_count(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    pub fn duration_secs(&self) -> f64 {
        let frames = self.frame_count();
        if self.sample_rate_hz == 0 {
            0.0
        } else {
            frames as f64 / f64::from(self.sample_rate_hz)
        }
    }
}

/// Stream metadata without decoding all packets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioProbe {
    pub sample_rate_hz: u32,
    /// Source bit depth when known; MP3 and similar lossy formats default to 16.
    pub bit_depth: u8,
    pub channels: u16,
    pub extension: String,
}

fn extension_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

fn open_probed(path: &Path) -> Result<(symphonia::core::probe::ProbeResult, String), AudioError> {
    let ext = extension_of(path).unwrap_or_default();
    // Gapless like the stream path, so a duration or a full decode agrees with what plays.
    let (probed, _gapless) = probe_gapless_first(path, &ext, &file_opener(path))?;
    Ok((probed, ext))
}

/// Probe sample rate / bit depth / channels without full decode.
pub fn probe(path: &Path) -> Result<AudioProbe, AudioError> {
    let (probed, ext) = open_probed(path)?;
    let track = probed
        .format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| AudioError::Probe {
            path: path.to_path_buf(),
            reason: "no audio track".into(),
        })?;

    let sample_rate_hz = track.codec_params.sample_rate.ok_or_else(|| AudioError::Probe {
        path: path.to_path_buf(),
        reason: "missing sample rate".into(),
    })?;
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count() as u16)
        .unwrap_or(2);
    let bit_depth = track
        .codec_params
        .bits_per_sample
        .map(|b| b as u8)
        .unwrap_or(16);

    Ok(AudioProbe {
        sample_rate_hz,
        bit_depth,
        channels,
        extension: ext,
    })
}

/// Fully decode a file to interleaved f32 PCM.
pub fn decode(path: &Path) -> Result<PcmBuffer, AudioError> {
    let (probed, _ext) = open_probed(path)?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| AudioError::Decode {
            path: path.to_path_buf(),
            reason: "no audio track".into(),
        })?;

    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let sample_rate_hz = codec_params.sample_rate.ok_or_else(|| AudioError::Decode {
        path: path.to_path_buf(),
        reason: "missing sample rate".into(),
    })?;
    let channels = codec_params
        .channels
        .map(|c| c.count() as u16)
        .unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| AudioError::Decode {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;

    let mut samples = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(err) => {
                return Err(AudioError::Decode {
                    path: path.to_path_buf(),
                    reason: err.to_string(),
                });
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = decoder.decode(&packet).map_err(|e| AudioError::Decode {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        append_decoded(&decoded, channels, &mut samples);
    }

    if samples.is_empty() {
        return Err(AudioError::Decode {
            path: path.to_path_buf(),
            reason: "decoded no samples".into(),
        });
    }

    Ok(PcmBuffer {
        samples,
        channels,
        sample_rate_hz,
    })
}

pub(crate) fn append_decoded(decoded: &AudioBufferRef<'_>, channels: u16, out: &mut Vec<f32>) {
    match decoded {
        AudioBufferRef::U8(buf) => append_typed(buf, channels, out),
        AudioBufferRef::U16(buf) => append_typed(buf, channels, out),
        AudioBufferRef::U24(buf) => append_typed(buf, channels, out),
        AudioBufferRef::U32(buf) => append_typed(buf, channels, out),
        AudioBufferRef::S8(buf) => append_typed(buf, channels, out),
        AudioBufferRef::S16(buf) => append_typed(buf, channels, out),
        AudioBufferRef::S24(buf) => append_typed(buf, channels, out),
        AudioBufferRef::S32(buf) => append_typed(buf, channels, out),
        AudioBufferRef::F32(buf) => append_typed(buf, channels, out),
        AudioBufferRef::F64(buf) => append_typed(buf, channels, out),
    }
}

fn append_typed<S: Sample>(
    buf: &symphonia::core::audio::AudioBuffer<S>,
    channels: u16,
    out: &mut Vec<f32>,
) where
    f32: FromSample<S>,
{
    let frames = buf.frames();
    let ch_count = channels as usize;
    for frame in 0..frames {
        for ch in 0..ch_count {
            let sample = buf
                .chan(ch)
                .get(frame)
                .copied()
                .map(f32::from_sample)
                .unwrap_or(0.0);
            out.push(sample);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn probe_wav_48k_16() {
        let p = probe(&fixture("tone_48k_16.wav")).expect("probe wav");
        assert_eq!(p.sample_rate_hz, 48_000);
        assert_eq!(p.bit_depth, 16);
        assert_eq!(p.channels, 2);
        assert_eq!(p.extension, "wav");
    }

    #[test]
    fn decode_wav_has_frames() {
        let pcm = decode(&fixture("tone_48k_16.wav")).expect("decode wav");
        assert_eq!(pcm.channels, 2);
        assert_eq!(pcm.sample_rate_hz, 48_000);
        assert!(pcm.frame_count() > 1000);
        assert!(pcm.samples.iter().any(|s| s.abs() > 0.01));
    }
}
