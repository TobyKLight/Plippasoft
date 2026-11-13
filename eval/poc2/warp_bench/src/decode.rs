//! Symphonia decoding: speed measurement (`decode`), file loading for the listening tests.

use crate::seek::{Counters, CountingSource};
use crate::stats;
use crate::voices::PcmBuf;
use anyhow::{anyhow, bail, Context, Result};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// An opened media file with byte counters on the underlying source.
pub struct Opened {
    pub format: Box<dyn FormatReader>,
    pub decoder: Box<dyn Decoder>,
    pub track_id: u32,
    pub rate: u32,
    pub ch: usize,
    pub n_frames: Option<u64>,
    pub codec: String,
    pub file_len: u64,
    pub counters: Arc<Counters>,
}

pub fn open(path: &Path) -> Result<Opened> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let counters = Arc::new(Counters::default());
    let src = CountingSource::new(file, counters.clone(), Some(file_len));
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
        .with_context(|| {
            format!(
                "probe {} (is it an audio file Symphonia supports?)",
                path.display()
            )
        })?;
    let format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("{}: no audio track", path.display()))?;
    let params = track.codec_params.clone();
    let rate = params
        .sample_rate
        .ok_or_else(|| anyhow!("missing sample rate"))?;
    let ch = params
        .channels
        .ok_or_else(|| anyhow!("missing channel layout"))?
        .count();
    let codec = symphonia::default::get_codecs()
        .get_codec(params.codec)
        .map(|c| c.short_name.to_string())
        .unwrap_or_else(|| "?".into());
    let decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .context("unsupported codec")?;
    Ok(Opened {
        track_id: track.id,
        n_frames: params.n_frames,
        format,
        decoder,
        rate,
        ch,
        codec,
        file_len,
        counters,
    })
}

impl Opened {
    /// Estimated duration in frames: the container's frame count, else extrapolated from how many
    /// bytes the first seconds of audio take (VBR MP3 without a Xing header).
    pub fn duration_frames(&mut self) -> Result<u64> {
        if let Some(n) = self.n_frames {
            return Ok(n);
        }
        let b0 = self.counters.bytes();
        let want = (5.0 * self.rate as f64) as u64;
        let mut got = 0u64;
        let mut sb: Option<SampleBuffer<f32>> = None;
        while got < want {
            match self.decode_next(&mut sb)? {
                Some(n) => got += n as u64,
                None => break,
            }
        }
        let used = self.counters.bytes().saturating_sub(b0).max(1);
        if got == 0 {
            bail!("could not decode any audio to estimate the duration");
        }
        Ok(((self.file_len as f64) * (got as f64 / used as f64)) as u64)
    }

    /// Decode the next packet of our track into `sb` (allocated lazily). `Ok(None)` at the end.
    /// Returns the number of frames decoded.
    pub fn decode_next(&mut self, sb: &mut Option<SampleBuffer<f32>>) -> Result<Option<usize>> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok(None)
                }
                Err(SymError::ResetRequired) => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let spec = *buf.spec();
                    let frames = buf.frames();
                    let need = buf.capacity();
                    if sb
                        .as_ref()
                        .is_none_or(|s| s.capacity() < need * spec.channels.count())
                    {
                        *sb = Some(SampleBuffer::<f32>::new(need as u64, spec));
                    }
                    sb.as_mut().unwrap().copy_interleaved_ref(buf);
                    return Ok(Some(frames));
                }
                Err(SymError::DecodeError(_)) => continue, // skip a damaged packet
                Err(e) => return Err(e.into()),
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct DecodeResult {
    pub codec: String,
    pub rate: u32,
    pub ch: usize,
    pub audio_s: f64,
    /// x realtime of each run (audio seconds / wall seconds).
    pub runs_x: Vec<f64>,
}

impl DecodeResult {
    pub fn median_x(&self) -> f64 {
        stats::median(&self.runs_x)
    }
    pub fn min_x(&self) -> f64 {
        stats::min(&self.runs_x)
    }
}

/// Decode the file (whole, or the first `seconds`) `runs` times and report x realtime per run.
/// The timed section is the packet loop (read + decode + f32 conversion), like the product.
pub fn measure(path: &Path, seconds: f64, runs: usize) -> Result<DecodeResult> {
    let mut runs_x = Vec::new();
    let mut info = None;
    for _ in 0..runs.max(1) {
        let mut o = open(path)?;
        let limit = (seconds * o.rate as f64) as u64;
        let mut sb = None;
        let mut frames = 0u64;
        let t0 = Instant::now();
        while frames < limit {
            match o.decode_next(&mut sb)? {
                Some(n) => frames += n as u64,
                None => break,
            }
        }
        let wall = t0.elapsed().as_secs_f64();
        if frames == 0 {
            bail!("{}: decoded no audio", path.display());
        }
        let audio_s = frames as f64 / o.rate as f64;
        runs_x.push(audio_s / wall.max(1e-9));
        info = Some((o.codec.clone(), o.rate, o.ch, audio_s));
    }
    let (codec, rate, ch, audio_s) = info.unwrap();
    Ok(DecodeResult {
        codec,
        rate,
        ch,
        audio_s,
        runs_x,
    })
}

/// Time per decoded packet (seconds), used for the "decode of 3 frames" term of the indexed-seek estimate.
pub fn time_per_packet(path: &Path, packets: usize) -> Result<f64> {
    let mut o = open(path)?;
    let mut sb = None;
    let t0 = Instant::now();
    let mut n = 0usize;
    while n < packets {
        match o.decode_next(&mut sb)? {
            Some(_) => n += 1,
            None => break,
        }
    }
    if n == 0 {
        bail!("decoded no packets");
    }
    Ok(t0.elapsed().as_secs_f64() / n as f64)
}

/// Seek (Accurate) to `start_frame` and decode at least `frames` frames from there. The returned buffer
/// starts at the frame the decoder actually landed on (at or before `start_frame`).
pub fn read_window(o: &mut Opened, start_frame: i64, frames: usize) -> Result<PcmBuf> {
    use symphonia::core::formats::{SeekMode, SeekTo};
    use symphonia::core::units::Time;
    let start_frame = start_frame.max(0);
    let secs = start_frame as f64 / o.rate as f64;
    let seeked = o.format.seek(
        SeekMode::Accurate,
        SeekTo::Time {
            time: Time::from(secs),
            track_id: Some(o.track_id),
        },
    )?;
    o.decoder.reset();
    let first = seeked.actual_ts as i64;
    let mut data: Vec<f32> = Vec::new();
    let mut sb = None;
    while (first + (data.len() / o.ch) as i64) < start_frame + frames as i64 {
        match o.decode_next(&mut sb)? {
            Some(n) => data.extend_from_slice(&sb.as_ref().unwrap().samples()[..n * o.ch]),
            None => break,
        }
    }
    Ok(PcmBuf::new(o.rate, o.ch, first, data))
}

/// Decode `[0, max_s)` of the file into memory (native rate, interleaved f32).
pub fn load_pcm(path: &Path, max_s: f64) -> Result<PcmBuf> {
    let mut o = open(path)?;
    let limit = (max_s * o.rate as f64) as usize;
    let mut data: Vec<f32> = Vec::new();
    let mut sb = None;
    while data.len() / o.ch < limit {
        match o.decode_next(&mut sb)? {
            Some(n) => {
                let s = sb.as_ref().unwrap().samples();
                data.extend_from_slice(&s[..n * o.ch]);
            }
            None => break,
        }
    }
    data.truncate(limit * o.ch);
    if data.is_empty() {
        bail!("{}: decoded no audio", path.display());
    }
    Ok(PcmBuf::new(o.rate, o.ch, 0, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::{write_signal_wav, Kind};

    fn temp_wav(
        name: &str,
        seconds: f64,
        rate: u32,
        ch: u16,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("warp_bench_dec_{}_{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        write_signal_wav(&p, Kind::Tone, seconds, rate, ch).unwrap();
        (dir, p)
    }

    #[test]
    fn open_reports_format_and_duration() {
        let (dir, p) = temp_wav("open", 2.0, 48000, 2);
        let mut o = open(&p).unwrap();
        assert_eq!((o.rate, o.ch), (48000, 2));
        assert_eq!(o.duration_frames().unwrap(), 96000);
        assert!(!o.codec.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_pcm_decodes_the_requested_amount_and_matches_the_tone() {
        let (dir, p) = temp_wav("load", 3.0, 44100, 2);
        let buf = load_pcm(&p, 1.0).unwrap();
        assert_eq!(buf.frames(), 44100);
        assert_eq!(buf.ch, 2);
        // 16-bit quantisation only
        let ref_sig = crate::gen::generate(Kind::Tone, 1.0, 44100);
        for i in (0..44100).step_by(97) {
            assert!((buf.data[i * 2] - ref_sig[i]).abs() < 2e-4);
        }
        let whole = load_pcm(&p, 100.0).unwrap();
        assert_eq!(whole.frames(), 3 * 44100);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_window_returns_the_requested_audio() {
        let (dir, p) = temp_wav("win", 20.0, 44100, 2);
        let whole = load_pcm(&p, 100.0).unwrap();
        let mut o = open(&p).unwrap();
        let start = 5 * 44100 + 123;
        let w = read_window(&mut o, start, 3 * 44100).unwrap();
        assert!(w.start <= start && start - w.start < 8192);
        assert!(w.end() >= start + 3 * 44100);
        for f in (start..start + 3 * 44100).step_by(1001) {
            for c in 0..2 {
                assert_eq!(w.at(f, c), whole.at(f, c), "frame {f}");
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn measure_runs_report_positive_ratios() {
        // numbers are meaningless on the cloud box: only the structure is checked
        let (dir, p) = temp_wav("meas", 2.0, 44100, 2);
        let r = measure(&p, 1.0, 3).unwrap();
        assert_eq!(r.runs_x.len(), 3);
        assert!(r.runs_x.iter().all(|x| *x > 0.0 && x.is_finite()));
        assert!((r.audio_s - 1.0).abs() < 0.2);
        assert!(r.min_x() <= r.median_x());
        assert!(time_per_packet(&p, 5).unwrap() > 0.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_or_non_audio_files_error_cleanly() {
        assert!(open(Path::new("/nonexistent/file.mp3")).is_err());
        let dir = std::env::temp_dir().join(format!("warp_bench_dec_{}_bad", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.mp3");
        std::fs::write(&p, vec![0u8; 4096]).unwrap();
        assert!(open(&p).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
