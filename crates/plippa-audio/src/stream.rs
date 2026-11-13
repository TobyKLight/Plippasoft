//! Chunked / pull decode — never holds a whole track in RAM.
//!
//! # Buffer budget
//!
//! [`MAX_STREAM_BUFFER_BYTES`] caps PCM queued ahead of the sink for one track
//! (~2 MiB ≈ 5.5 s stereo f32 @ 48 kHz). Prefetch stays well under a few MB so
//! multi-minute MP3s cannot swap-thrash a 1 GB CM4.
//!
//! First audio uses [`START_PREFETCH_BYTES`] only; the soft
//! [`PREFETCH_TARGET_BYTES`] is filled incrementally after Play so start latency
//! is not ~seconds of decode.
//!
//! # Sample rate
//!
//! Chunks are emitted at the **file’s native sample rate**. PipeWire/Pulse
//! converts to the device rate; Plippa does not whole-file resample on Play.
//!
//! # Malformed frames
//!
//! One undecodable frame is not the end of the file. A `DecodeError` from the demuxer
//! (`next_packet`) or the codec (`decode`) drops that packet, is counted
//! ([`StreamDecoder::skipped_packets`]) and decoding continues with the next one. No silence is
//! inserted and the timeline is not stretched: [`StreamDecoder::position_frame`] advances only
//! by frames actually decoded, so a track with a skipped frame is a few tens of milliseconds
//! **shorter** than its header says (strictly better than stopping there).
//!
//! The skip is bounded: [`MAX_CONSECUTIVE_DECODE_ERRORS`] errors in a row with no decoded
//! output end the stream with [`AudioError::Decode`] (a file that is bad from the start fails
//! fast). An I/O error other than a clean end of file stays fatal, so a dead stick never spins.
//!
//! At a mid-stream skip the last [`SPLICE_RAMP_FRAMES`] frames before it fade out and the first
//! ones after it fade in (linear, about 3 ms), so the join is not a click. To make that
//! possible `next_chunk` keeps that many frames back in `pending` until more audio (or the end
//! of the file) is known; chunk sizes and positions are unchanged.
//!
//! # Gapless (SEAM_01)
//!
//! Every format is probed with Symphonia's `enable_gapless`: an MP3's encoder delay and padding
//! (LAME/Xing tag) are trimmed, and `position_frame`, `duration_frames` and every seek are on the
//! trimmed timeline. A file that refuses the gapless probe, or an MPEG stream with no encoder
//! delay in its header (the reader would cut it at a length it only *estimates*), is opened with
//! the plain options instead. The MP3 frame-index sub-reader always uses the plain options (it
//! starts mid-file, with no header to trim by): `try_indexed_seek` shifts by the delay and
//! `cut_at_trimmed_end` drops the padding.

#[cfg(test)]
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plippa_core::Capabilities;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::decode::{append_decoded, probe, AudioProbe, PcmBuffer};
use crate::error::AudioError;
use crate::index::{Mp3Index, OffsetSource};
use crate::position::ms_to_frame;
use crate::prepare::{exceeds_capabilities, quality_exceeds_error, OverspecPolicy};

/// Peak interleaved f32 PCM bytes allowed in the decode→sink prefetch for one track.
pub const MAX_STREAM_BUFFER_BYTES: usize = 2 * 1024 * 1024;

/// Soft steady-state prefetch target (half of the hard cap).
///
/// Filled **incrementally** after Play starts — never block first audio on this.
pub const PREFETCH_TARGET_BYTES: usize = MAX_STREAM_BUFFER_BYTES / 2;

/// PCM to have before the first sink write (~one small chunk).
///
/// Sized for Noubin §3.2.1 (&lt;1 s, ideal &lt;300 ms): decode the minimum to write,
/// then top up toward [`PREFETCH_TARGET_BYTES`] a few chunks per audio-loop turn.
pub const START_PREFETCH_BYTES: usize = 16 * 1024;

/// Max decode chunks per audio-loop turn when topping up toward [`PREFETCH_TARGET_BYTES`].
pub const REFILL_CHUNKS_PER_TURN: usize = 2;

/// Pull size for the pre-Play start buffer (~23 ms stereo @ 44.1 kHz).
pub const START_CHUNK_FRAMES: usize = 1024;

/// Default pull size (~85 ms stereo @ 48 kHz).
pub const DEFAULT_CHUNK_FRAMES: usize = 4096;

/// Consecutive undecodable frames (no decoded output in between) after which the stream
/// fails instead of skipping on: 64 MP3 frames is about 1.7 s of audio.
pub const MAX_CONSECUTIVE_DECODE_ERRORS: u32 = 64;

/// Frames faded out before and in after a skipped frame (about 3 ms at 44.1 kHz).
pub const SPLICE_RAMP_FRAMES: usize = 128;

/// Gapless decode everywhere a format is opened: MP3 (LAME/Xing) delay+padding are trimmed
/// and timestamps become relative to the trimmed region (SEAM_01 part A).
pub(crate) fn gapless_format_options() -> FormatOptions {
    FormatOptions {
        enable_gapless: true,
        ..Default::default()
    }
}

/// The format options a (re)probe uses: gapless unless this file already fell back.
fn format_options(gapless: bool) -> FormatOptions {
    if gapless {
        gapless_format_options()
    } else {
        FormatOptions::default()
    }
}

/// Probe a fresh source from `opener` with gapless trimming on; if that probe fails, probe
/// again from another fresh source with the plain options, so a library never loses a playable
/// file to the gapless option. Returns whether the gapless probe is the one in use.
pub(crate) fn probe_gapless_first(
    path: &Path,
    ext: &str,
    opener: &SourceOpener,
) -> Result<(symphonia::core::probe::ProbeResult, bool), AudioError> {
    let probe_with = |options: &FormatOptions| {
        let file = opener().map_err(|source| AudioError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mss = MediaSourceStream::new(file, Default::default());
        let mut hint = Hint::new();
        if !ext.is_empty() {
            hint.with_extension(ext);
        }
        symphonia::default::get_probe()
            .format(&hint, mss, options, &MetadataOptions::default())
            .map_err(|e| AudioError::Probe {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })
    };
    match probe_with(&gapless_format_options()) {
        Ok(probed) if trims_nothing_and_may_mislead(&probed) => {
            // No encoder delay/padding to remove, and the MPEG reader would still cut the
            // stream at a length it *estimated* from the first frames (a VBR file without a
            // header would lose its ending): use the plain reader for this file.
            Ok(match probe_with(&FormatOptions::default()) {
                Ok(plain) => (plain, false),
                Err(_) => (probed, true),
            })
        }
        Ok(probed) => Ok((probed, true)),
        Err(first) => match probe_with(&FormatOptions::default()) {
            Ok(probed) => {
                eprintln!(
                    "plippa-audio: {} did not open with gapless trimming ({first}); \
                     playing it untrimmed",
                    path.display()
                );
                Ok((probed, false))
            }
            // Neither worked: the file is genuinely unreadable; report the first verdict.
            Err(_) => Err(first),
        },
    }
}

/// An MPEG audio stream whose header carries no encoder delay/padding: with gapless on, the
/// reader trims packets beyond `n_frames`, which for such a file may be only an estimate.
fn trims_nothing_and_may_mislead(probed: &symphonia::core::probe::ProbeResult) -> bool {
    use symphonia::core::codecs::{CODEC_TYPE_MP1, CODEC_TYPE_MP2, CODEC_TYPE_MP3};
    probed
        .format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .is_some_and(|t| {
            let p = &t.codec_params;
            matches!(p.codec, CODEC_TYPE_MP1 | CODEC_TYPE_MP2 | CODEC_TYPE_MP3)
                && p.delay.is_none()
                && p.padding.is_none()
        })
}

/// Probe + capability gate without decoding sample data.
///
/// On the Play path, overspec with [`OverspecPolicy::Downsample`] is **allowed**
/// at the file’s native rate (PipeWire converts). [`OverspecPolicy::Refuse`]
/// still fails before any stream open.
pub fn validate_for_stream(
    path: &Path,
    capabilities: &Capabilities,
    policy: OverspecPolicy,
) -> Result<AudioProbe, AudioError> {
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
                // Native-rate stream; PW/DAC path handles conversion (Noubin §3.3).
            }
        }
    }
    Ok(meta)
}

/// Opens a fresh full-file source on every call (the decoder re-opens for indexed seeks, and
/// tests pass an instrumented one). Takes no path: it already knows its file.
pub type SourceOpener = Arc<dyn Fn() -> std::io::Result<Box<dyn MediaSource>> + Send + Sync>;

/// Opener for a plain file path, for in-process callers that have no library root to confine
/// them (`decode::decode`, `StreamDecoder::open`, tests). Every open, including the decoder's
/// re-opens for indexed seeks, goes through `safe_fs::open_regular_nofollow`: a path that has
/// become a symlink, FIFO or device is refused instead of followed or waited on.
///
/// The **playback** path does not use this: the engine opens the media file once, confined
/// beneath its library root (`safe_fs::open_media_for_decode_in`) and hands that handle to the
/// helper, so there is no path to re-open and nothing to swap (`SECFIX_05`).
pub fn file_opener(path: &Path) -> SourceOpener {
    let path = path.to_path_buf();
    Arc::new(move || {
        plippa_core::safe_fs::open_regular_nofollow(&path)
            .map(|f| Box::new(f) as Box<dyn MediaSource>)
    })
}

/// Incremental Symphonia decoder. Pull [`next_chunk`] until `Ok(None)` (EOF).
pub struct StreamDecoder {
    path: PathBuf,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    channels: u16,
    sample_rate_hz: u32,
    /// Source bit depth when the container says (16 otherwise).
    bit_depth: u8,
    /// Duration from container/`n_frames` when known.
    duration_ms: Option<u64>,
    /// Total frames from the container when known.
    duration_frames: Option<u64>,
    /// Source frame index of the next frame [`next_chunk`](Self::next_chunk) will return.
    next_frame: u64,
    /// Frame the stream (re)started on: the open position or the last seek's landing.
    start_frame: u64,
    /// Decoded frames still to drop so output starts on the seek target.
    skip_frames: u64,
    /// Leftover interleaved samples from the last packet (not yet returned).
    pending: Vec<f32>,
    eof: bool,
    /// Packets dropped as malformed since open (a re-decode after a seek counts again).
    skipped_packets: u64,
    /// Text of the most recent skipped error.
    last_decode_error: Option<String>,
    /// Frames of fade-in still to apply to freshly decoded output after a skip.
    ramp_up_left: usize,
    /// Peak pending+returned chunk bytes observed (for tests / diagnostics).
    peak_pending_bytes: usize,
    /// Lower-case file extension (selects the index path).
    ext: String,
    opener: SourceOpener,
    /// MP3 frame index (WARP_11); `None` until built or for other formats.
    index: Option<Arc<Mp3Index>>,
    indexed_seeks: u64,
    /// True while `format` is an index sub-reader (it starts mid-file, so its own timestamps
    /// are relative): a plain seek must re-open a full reader first.
    sub_reader: bool,
    /// The probe in use trims encoder delay/padding (`false` after the plain-options fallback).
    gapless: bool,
    /// Encoder delay the demuxer subtracts from every timestamp (0 when none is trimmed). An
    /// index sub-reader starts mid-file with no header, so it does not know it: the index
    /// holds untrimmed timestamps and `try_indexed_seek` shifts by this.
    trim_delay: u64,
    /// Trimmed track length when the demuxer trims, else `None`. A sub-reader cannot see the
    /// padding at the end of the file, so the decoder cuts its output at this frame.
    trim_end: Option<u64>,
}

impl StreamDecoder {
    /// Open `path` after a capability gate — **one** file open / Symphonia probe.
    pub fn open(
        path: &Path,
        capabilities: &Capabilities,
        policy: OverspecPolicy,
    ) -> Result<Self, AudioError> {
        Self::open_with(path, file_opener(path), capabilities, policy)
    }

    /// [`open`](Self::open) with an explicit source opener (instrumented / gated sources in
    /// tests; the opener is also used for indexed seeks). `path` only names the file.
    pub fn open_with(
        path: &Path,
        opener: SourceOpener,
        capabilities: &Capabilities,
        policy: OverspecPolicy,
    ) -> Result<Self, AudioError> {
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

        let (probed, gapless) = probe_gapless_first(path, &ext, &opener)?;

        let format = probed.format;
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
        let bit_depth = codec_params
            .bits_per_sample
            .map(|b| b as u8)
            .unwrap_or(16);
        let trims = gapless && (codec_params.delay.is_some() || codec_params.padding.is_some());
        let trim_delay = if trims {
            u64::from(codec_params.delay.unwrap_or(0))
        } else {
            0
        };
        let trim_end = if trims { codec_params.n_frames } else { None };
        let duration_ms = codec_params.n_frames.map(|n| {
            if sample_rate_hz == 0 {
                0
            } else {
                n.saturating_mul(1000) / u64::from(sample_rate_hz)
            }
        });

        // Capability gate without a second file open (control plane may have
        // probed already for logging; this is the stream open that matters).
        let meta = AudioProbe {
            sample_rate_hz,
            bit_depth,
            channels,
            extension: ext.clone(),
        };
        if exceeds_capabilities(&meta, capabilities) {
            match policy {
                OverspecPolicy::Refuse => {
                    return Err(quality_exceeds_error(path, &meta, capabilities));
                }
                OverspecPolicy::Downsample => {}
            }
        }

        let decoder = symphonia::default::get_codecs()
            .make(&codec_params, &DecoderOptions::default())
            .map_err(|e| AudioError::Decode {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;

        Ok(Self {
            path: path.to_path_buf(),
            format,
            decoder,
            track_id,
            channels,
            sample_rate_hz,
            bit_depth,
            duration_ms,
            duration_frames: codec_params.n_frames,
            next_frame: 0,
            start_frame: 0,
            skip_frames: 0,
            pending: Vec::new(),
            eof: false,
            skipped_packets: 0,
            last_decode_error: None,
            ramp_up_left: 0,
            peak_pending_bytes: 0,
            ext,
            opener,
            index: None,
            indexed_seeks: 0,
            sub_reader: false,
            gapless,
            trim_delay,
            trim_end,
        })
    }

    /// Lower-case file extension.
    pub fn extension(&self) -> &str {
        &self.ext
    }

    /// Install the MP3 frame index: backward seeks now start from the nearest entry instead of
    /// byte 0. Ignored for other formats.
    pub fn set_index(&mut self, index: Arc<Mp3Index>) {
        if self.ext == "mp3" {
            self.index = Some(index);
        }
    }

    /// True once an index is installed.
    pub fn has_index(&self) -> bool {
        self.index.is_some()
    }

    /// Seeks served through the index (the rest used the plain accurate seek).
    pub fn indexed_seeks(&self) -> u64 {
        self.indexed_seeks
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Source bit depth (16 when the container does not say).
    pub fn bit_depth(&self) -> u8 {
        self.bit_depth
    }

    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    /// Container duration when `n_frames` is known (WAV/FLAC usually; MP3 often unknown).
    pub fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    pub fn peak_pending_bytes(&self) -> usize {
        self.peak_pending_bytes
    }

    /// Malformed packets dropped (and decoded past) since open; a seek back over the same
    /// bad frame counts it again.
    pub fn skipped_packets(&self) -> u64 {
        self.skipped_packets
    }

    /// The error of the most recently skipped packet, if any was skipped.
    pub fn last_decode_error(&self) -> Option<String> {
        self.last_decode_error.clone()
    }

    pub fn is_eof(&self) -> bool {
        self.eof && self.pending.is_empty()
    }

    /// True when the container reports a duration (`duration_ms()` is `Some` and non-zero).
    /// MP3 without a Xing/Info header has none: callers must not clamp against 0.
    pub fn duration_known(&self) -> bool {
        self.duration_ms.is_some_and(|d| d > 0)
    }

    /// Container length in frames when known.
    pub fn duration_frames(&self) -> Option<u64> {
        self.duration_frames
    }

    /// Source frame index of the next frame `next_chunk` returns.
    pub fn position_frame(&self) -> u64 {
        self.next_frame
    }

    /// Frame the current stream started on: 0 after open, the landing frame after a seek.
    pub fn start_frame(&self) -> u64 {
        self.start_frame
    }

    /// Seek so the next output frame is the one at `position_ms` (ms boundary only).
    ///
    /// See [`seek_frame`](Self::seek_frame); the landing is exact to the frame.
    pub fn seek_ms(&mut self, position_ms: u64) -> Result<(), AudioError> {
        self.seek_frame(ms_to_frame(position_ms, self.sample_rate_hz))
            .map_err(|e| match e {
                AudioError::Decode { path, reason } => AudioError::Decode {
                    path,
                    reason: format!("seek to {position_ms}ms failed: {reason}"),
                },
                other => other,
            })
    }

    /// Seek so the next frame [`next_chunk`](Self::next_chunk) returns is `frame`.
    ///
    /// Symphonia's accurate seek lands on a packet boundary at or before the target
    /// (`SeekedTo::actual_ts <= required_ts`: MP3 lands up to a few frames early, i.e. about
    /// 100 ms; FLAC at block granularity). The difference is decoded and dropped, so output
    /// starts exactly on `frame`. If the format ever lands *after* the target the landing
    /// is reported as is (nothing can be added): read [`position_frame`](Self::position_frame).
    pub fn seek_frame(&mut self, frame: u64) -> Result<(), AudioError> {
        if let Some(landed) = self.try_indexed_seek(frame) {
            self.finish_seek(frame, landed);
            self.indexed_seeks += 1;
            return Ok(());
        }
        if self.sub_reader {
            self.reopen_full_reader()?;
        }
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::TimeStamp {
                    ts: frame,
                    track_id: self.track_id,
                },
            )
            .map_err(|e| AudioError::Decode {
                path: self.path.clone(),
                reason: format!("seek to frame {frame} failed: {e}"),
            })?;
        self.finish_seek(frame, seeked.actual_ts);
        Ok(())
    }

    /// Replace an index sub-reader by a full reader at the start of the file.
    fn reopen_full_reader(&mut self) -> Result<(), AudioError> {
        let io_err = |source| AudioError::Io {
            path: self.path.clone(),
            source,
        };
        let src = (self.opener)().map_err(io_err)?;
        let mut hint = Hint::new();
        hint.with_extension(&self.ext);
        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                MediaSourceStream::new(src, Default::default()),
                &format_options(self.gapless),
                &MetadataOptions::default(),
            )
            .map_err(|e| AudioError::Probe {
                path: self.path.clone(),
                reason: e.to_string(),
            })?;
        let track_id = probed
            .format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .map(|t| t.id)
            .ok_or_else(|| AudioError::Decode {
                path: self.path.clone(),
                reason: "no audio track".into(),
            })?;
        self.format = probed.format;
        self.track_id = track_id;
        self.sub_reader = false;
        Ok(())
    }

    /// Common tail of a seek that landed on `landed` (a packet boundary, normally `<= frame`).
    fn finish_seek(&mut self, frame: u64, landed: u64) {
        self.decoder.reset();
        self.pending.clear();
        self.ramp_up_left = 0;
        self.eof = false;
        if landed <= frame {
            self.skip_frames = frame - landed;
            self.next_frame = frame;
        } else {
            self.skip_frames = 0;
            self.next_frame = landed;
        }
        self.start_frame = self.next_frame;
    }

    /// Indexed MP3 seek: a new reader starting at the index entry before `frame` (with the
    /// bit-reservoir warm-up margin) and Symphonia's accurate seek over the few frames left.
    /// Returns the landing timestamp, or `None` (nothing changed) to use the plain seek.
    fn try_indexed_seek(&mut self, frame: u64) -> Option<u64> {
        // The index holds the file's own (untrimmed) timestamps; `frame` is on the trimmed
        // timeline the demuxer reports (SEAM_01), `delay` frames earlier. The sub-reader is
        // opened with the plain options on purpose: with gapless on, the demuxer trims by a
        // length it estimates from the bytes that are left (no Xing header mid-file), which
        // ends the stream early on a VBR file. The delay shift here and `cut_at_trimmed_end`
        // do the trimming instead.
        let delay = self.trim_delay;
        let raw_target = frame + delay;
        let entry = self.index.as_ref()?.entry_for(raw_target)?;
        if entry.ts < delay {
            return None;
        }
        let src = (self.opener)().ok()?;
        let src = OffsetSource::new(src, entry.offset).ok()?;
        let mss = MediaSourceStream::new(Box::new(src), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(&self.ext);
        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
            .ok()?;
        let mut format = probed.format;
        let track_id = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)?
            .id;
        let seeked = format
            .seek(
                SeekMode::Accurate,
                SeekTo::TimeStamp {
                    ts: raw_target - entry.ts,
                    track_id,
                },
            )
            .ok()?;
        self.format = format;
        self.track_id = track_id;
        self.sub_reader = true;
        Some(entry.ts + seeked.actual_ts - delay)
    }

    /// Decode up to `max_frames` interleaved frames. `Ok(None)` at EOF.
    pub fn next_chunk(&mut self, max_frames: usize) -> Result<Option<PcmBuffer>, AudioError> {
        let ch = self.channels as usize;
        if ch == 0 || max_frames == 0 {
            return Err(AudioError::InvalidParam("zero chunk size".into()));
        }
        let want_samples = max_frames.saturating_mul(ch);

        // Keep SPLICE_RAMP_FRAMES back until the end is known so a skip can fade them out.
        let hold_samples = SPLICE_RAMP_FRAMES * ch;
        while self.pending.len() < want_samples + hold_samples && !self.eof {
            self.pull_packet()?;
            self.peak_pending_bytes = self.peak_pending_bytes.max(self.pending.len() * 4);
            // Hard cap: never let pending grow past the stream budget.
            if self.pending.len() * 4 > MAX_STREAM_BUFFER_BYTES {
                break;
            }
        }

        if self.pending.is_empty() {
            return Ok(None);
        }

        let take = want_samples.min(self.pending.len());
        // Align to frame boundary.
        let take = take - (take % ch);
        if take == 0 {
            return Ok(None);
        }
        let samples: Vec<f32> = self.pending.drain(..take).collect();
        self.next_frame += (take / ch) as u64;
        Ok(Some(PcmBuffer {
            samples,
            channels: self.channels,
            sample_rate_hz: self.sample_rate_hz,
        }))
    }

    /// Decode one packet into `pending`, skipping malformed ones (see the module docs).
    fn pull_packet(&mut self) -> Result<(), AudioError> {
        let mut consecutive_errors = 0u32;
        let mut faded_out = false;
        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(SymphoniaError::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(SymphoniaError::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    self.eof = true;
                    return Ok(());
                }
                Err(SymphoniaError::DecodeError(why)) => {
                    self.skip_packet(why, &mut consecutive_errors, &mut faded_out)?;
                    continue;
                }
                Err(err) => {
                    self.eof = true;
                    return Err(AudioError::Decode {
                        path: self.path.clone(),
                        reason: err.to_string(),
                    });
                }
            };

            if packet.track_id() != self.track_id {
                continue;
            }

            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(SymphoniaError::DecodeError(why)) => {
                    self.skip_packet(why, &mut consecutive_errors, &mut faded_out)?;
                    continue;
                }
                Err(err) => {
                    self.eof = true;
                    return Err(AudioError::Decode {
                        path: self.path.clone(),
                        reason: err.to_string(),
                    });
                }
            };
            let before = self.pending.len();
            append_decoded(&decoded, self.channels, &mut self.pending);
            let ch = self.channels as usize;
            if self.skip_frames > 0 {
                let have = ((self.pending.len() - before) / ch.max(1)) as u64;
                let drop = have.min(self.skip_frames);
                self.pending.drain(before..before + drop as usize * ch);
                self.skip_frames -= drop;
            }
            if self.ramp_up_left > 0 {
                let new_frames = (self.pending.len() - before) / ch.max(1);
                let n = new_frames.min(self.ramp_up_left);
                for k in 0..n {
                    let gain = (SPLICE_RAMP_FRAMES - self.ramp_up_left + k) as f32
                        / SPLICE_RAMP_FRAMES as f32;
                    for s in &mut self.pending[before + k * ch..before + (k + 1) * ch] {
                        *s *= gain;
                    }
                }
                self.ramp_up_left -= n;
            }
            self.cut_at_trimmed_end();
            return Ok(());
        }
    }

    /// Behind an index seek the reader starts mid-file and never sees the padding of the last
    /// frames, so the output is cut at the trimmed track length here. (The full reader's
    /// demuxer already trims it; its frames never reach the limit.)
    fn cut_at_trimmed_end(&mut self) {
        let Some(limit) = self.trim_end.filter(|_| self.sub_reader) else {
            return;
        };
        let ch = (self.channels as usize).max(1);
        let end = self.next_frame + (self.pending.len() / ch) as u64;
        if end > limit {
            let keep = limit.saturating_sub(self.next_frame) as usize;
            self.pending.truncate(keep * ch);
            self.eof = true;
        }
    }

    /// Account for one malformed packet: count it, fade the audio before the gap out (once per
    /// gap) and arm the fade-in, or fail when the consecutive-error budget is spent.
    fn skip_packet(
        &mut self,
        why: &str,
        consecutive_errors: &mut u32,
        faded_out: &mut bool,
    ) -> Result<(), AudioError> {
        self.skipped_packets += 1;
        self.last_decode_error = Some(why.to_string());
        *consecutive_errors += 1;
        if *consecutive_errors >= MAX_CONSECUTIVE_DECODE_ERRORS {
            self.eof = true;
            return Err(AudioError::Decode {
                path: self.path.clone(),
                reason: format!(
                    "{MAX_CONSECUTIVE_DECODE_ERRORS} consecutive undecodable frames \
                     (skip budget exhausted, {} skipped in total), last error: {why}",
                    self.skipped_packets
                ),
            });
        }
        if !*faded_out && !self.pending.is_empty() {
            *faded_out = true;
            let ch = self.channels as usize;
            let frames = (self.pending.len() / ch.max(1)).min(SPLICE_RAMP_FRAMES);
            let start = self.pending.len() - frames * ch;
            for k in 0..frames {
                let gain = (frames - 1 - k) as f32 / SPLICE_RAMP_FRAMES as f32;
                for s in &mut self.pending[start + k * ch..start + (k + 1) * ch] {
                    *s *= gain;
                }
            }
            self.ramp_up_left = SPLICE_RAMP_FRAMES;
        }
        Ok(())
    }
}

#[cfg(test)]
mod seek_tests;

#[cfg(test)]
mod skip_tests;

#[cfg(test)]
mod gapless_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_core::Capabilities;
    use std::io::Write;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn chunk_decoder_yields_multiple_chunks_then_eof() {
        let caps = Capabilities::desktop_mock();
        let mut dec =
            StreamDecoder::open(&fixture("tone_48k_16.wav"), &caps, OverspecPolicy::Downsample)
                .expect("open");
        assert_eq!(dec.sample_rate_hz(), 48_000);
        assert_eq!(dec.channels(), 2);

        let mut chunks = 0usize;
        let mut total_frames = 0usize;
        // Small pulls so a short fixture spans multiple chunks.
        while let Some(pcm) = dec.next_chunk(256).expect("chunk") {
            chunks += 1;
            total_frames += pcm.frame_count();
            assert_eq!(pcm.sample_rate_hz, 48_000);
            assert!(pcm.pending_under_cap());
        }
        assert!(chunks >= 2, "expected multiple chunks, got {chunks}");
        assert!(total_frames > 1000);
        assert!(dec.is_eof());
        assert!(dec.peak_pending_bytes() <= MAX_STREAM_BUFFER_BYTES);
    }

    #[test]
    fn stream_opens_at_file_sample_rate_44100() {
        let path = write_tone_wav(44_100, 2, 44_100 / 10); // 100 ms
        let caps = Capabilities::desktop_mock();
        let mut dec =
            StreamDecoder::open(&path, &caps, OverspecPolicy::Downsample).expect("open 44.1");
        assert_eq!(dec.sample_rate_hz(), 44_100);
        let first = dec.next_chunk(512).expect("chunk").expect("some pcm");
        assert_eq!(first.sample_rate_hz, 44_100);
        // Must not silently upsample to board max (48 k).
        assert_ne!(first.sample_rate_hz, caps.max_sample_rate_hz);
    }

    #[test]
    fn peak_buffered_under_cap_on_longer_fixture() {
        // ~3 s stereo @ 48 k would be ~1.1 MiB if fully decoded; we only pull chunks.
        let path = write_tone_wav(48_000, 2, 48_000 * 3);
        let caps = Capabilities::desktop_mock();
        let mut dec = StreamDecoder::open(&path, &caps, OverspecPolicy::Downsample).expect("open");
        let mut peak = 0usize;
        while let Some(pcm) = dec.next_chunk(DEFAULT_CHUNK_FRAMES).expect("chunk") {
            peak = peak.max(pcm.samples.len() * 4);
            assert!(
                pcm.samples.len() * 4 <= MAX_STREAM_BUFFER_BYTES,
                "chunk exceeded cap"
            );
            // Simulate consumer: drop chunk (do not accumulate).
        }
        assert!(peak <= MAX_STREAM_BUFFER_BYTES);
        assert!(dec.peak_pending_bytes() <= MAX_STREAM_BUFFER_BYTES);
    }

    #[test]
    fn refuse_overspec_before_stream_open() {
        let caps = Capabilities::desktop_mock();
        let err = match StreamDecoder::open(
            &fixture("overspec_192k_24.wav"),
            &caps,
            OverspecPolicy::Refuse,
        ) {
            Ok(_) => panic!("refuse"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("192") && msg.contains("24"), "{msg}");
    }

    #[test]
    fn downsample_policy_allows_overspec_at_native_rate() {
        let caps = Capabilities::desktop_mock();
        let mut dec = StreamDecoder::open(
            &fixture("overspec_192k_24.wav"),
            &caps,
            OverspecPolicy::Downsample,
        )
        .expect("allow overspec native");
        assert_eq!(dec.sample_rate_hz(), 192_000);
        let pcm = dec.next_chunk(256).expect("chunk").expect("pcm");
        assert_eq!(pcm.sample_rate_hz, 192_000);
    }

    trait PendingCheck {
        fn pending_under_cap(&self) -> bool;
    }
    impl PendingCheck for PcmBuffer {
        fn pending_under_cap(&self) -> bool {
            self.samples.len() * 4 <= MAX_STREAM_BUFFER_BYTES
        }
    }

    #[test]
    fn seek_ms_advances_decoder_past_start() {
        let path = write_tone_wav(48_000, 2, 48_000); // 1s
        let caps = Capabilities::desktop_mock();
        let mut dec =
            StreamDecoder::open(&path, &caps, OverspecPolicy::Downsample).expect("open");
        assert_eq!(dec.duration_ms(), Some(1000));
        dec.seek_ms(500).expect("seek");
        let pcm = dec.next_chunk(256).expect("chunk").expect("pcm");
        assert_eq!(pcm.sample_rate_hz, 48_000);
        assert!(!pcm.samples.is_empty());
    }

    /// Minimal PCM16 stereo WAV writer for rate-specific fixtures.
    fn write_tone_wav(rate: u32, channels: u16, frames: usize) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "plippa_stream_tone_{rate}_{}_{frames}.wav",
            std::process::id()
        ));
        let mut samples = Vec::with_capacity(frames * channels as usize);
        for i in 0..frames {
            let t = i as f32 / rate as f32;
            let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.2;
            let i16 = (s * 32767.0) as i16;
            for _ in 0..channels {
                samples.push(i16);
            }
        }
        let data_bytes = samples.len() * 2;
        let mut out = std::fs::File::create(&path).expect("create wav");
        out.write_all(b"RIFF").unwrap();
        out.write_all(&(36 + data_bytes as u32).to_le_bytes())
            .unwrap();
        out.write_all(b"WAVEfmt ").unwrap();
        out.write_all(&16u32.to_le_bytes()).unwrap(); // fmt chunk size
        out.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        out.write_all(&channels.to_le_bytes()).unwrap();
        out.write_all(&rate.to_le_bytes()).unwrap();
        let byte_rate = rate * u32::from(channels) * 2;
        out.write_all(&byte_rate.to_le_bytes()).unwrap();
        let block_align = channels * 2;
        out.write_all(&block_align.to_le_bytes()).unwrap();
        out.write_all(&16u16.to_le_bytes()).unwrap(); // bits
        out.write_all(b"data").unwrap();
        out.write_all(&(data_bytes as u32).to_le_bytes()).unwrap();
        for s in samples {
            out.write_all(&s.to_le_bytes()).unwrap();
        }
        out.flush().unwrap();
        path
    }
}
