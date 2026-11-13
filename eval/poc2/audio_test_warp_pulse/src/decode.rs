use anyhow::{bail, Context, Result};
use std::fs::File;
use std::path::Path;
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::conv::FromSample;
use symphonia::core::sample::Sample;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub struct DecodedAudio {
    pub samples: Vec<f32>,
    pub channels: u32,
    pub sample_rate: u32,
}

impl DecodedAudio {
    pub fn duration_s(&self) -> f64 {
        if self.channels == 0 || self.sample_rate == 0 {
            return 0.0;
        }
        let frames = self.samples.len() as f64 / self.channels as f64;
        frames / self.sample_rate as f64
    }

    pub fn with_silence_padding(mut self, padding_s: f64) -> Self {
        let pad_frames = (padding_s * self.sample_rate as f64).ceil() as usize;
        let pad_samples = pad_frames * self.channels as usize;
        self.samples.extend(std::iter::repeat_n(0.0f32, pad_samples));
        self
    }
}

pub fn decode_mp3(path: &Path) -> Result<DecodedAudio> {
    let src = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(src), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .with_context(|| format!("probe {}", path.display()))?;

    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .context("no audio track")?;

    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let sample_rate = codec_params.sample_rate.context("missing sample rate")?;
    let channels = codec_params
        .channels
        .context("missing channel layout")?
        .count() as u32;

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .context("unsupported codec")?;

    let mut samples = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(Error::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(err.into()),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = decoder.decode(&packet)?;
        append_decoded(&decoded, channels, &mut samples);
    }

    if samples.is_empty() {
        bail!("decoded no audio samples from {}", path.display());
    }

    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate,
    })
}

fn append_decoded(decoded: &AudioBufferRef<'_>, channels: u32, out: &mut Vec<f32>) {
    match decoded {
        AudioBufferRef::U8(buf) => append_typed_buffer(&buf, channels, out),
        AudioBufferRef::U16(buf) => append_typed_buffer(&buf, channels, out),
        AudioBufferRef::U24(buf) => append_typed_buffer(&buf, channels, out),
        AudioBufferRef::U32(buf) => append_typed_buffer(&buf, channels, out),
        AudioBufferRef::S8(buf) => append_typed_buffer(&buf, channels, out),
        AudioBufferRef::S16(buf) => append_typed_buffer(&buf, channels, out),
        AudioBufferRef::S24(buf) => append_typed_buffer(&buf, channels, out),
        AudioBufferRef::S32(buf) => append_typed_buffer(&buf, channels, out),
        AudioBufferRef::F32(buf) => append_typed_buffer(&buf, channels, out),
        AudioBufferRef::F64(buf) => append_typed_buffer(&buf, channels, out),
    }
}

fn append_typed_buffer<S: Sample>(
    buf: &symphonia::core::audio::AudioBuffer<S>,
    channels: u32,
    out: &mut Vec<f32>,
) where
    f32: FromSample<S>,
{
    let frames = buf.frames();
    for frame in 0..frames {
        for ch in 0..channels as usize {
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
