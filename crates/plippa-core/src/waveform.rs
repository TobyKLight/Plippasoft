//! `.wfm` waveform sidecar format (WAVE_01).
//!
//! A fixed 48-byte little-endian header followed by one `u8` peak per bucket:
//!
//! ```text
//! <library root>/.plippa/waveforms/<16 hex>.wfm
//! ```
//!
//! Stored bytes are **dB positions**, not amplitudes: `20*log10(peak)` clamped to
//! `[-60, 0]` dB mapped linearly to `0..=255` (silence is `0`, full scale `255`). The UI
//! applies whatever display curve it likes.
//!
//! The file lives on removable media that may have been prepared by somebody else, so
//! [`decode`] validates the magic, version, bucket size, the payload cap and the FNV
//! checksum **before** it allocates anything from the header.
//!
//! Peaks are produced incrementally by [`Accumulator`] over decoded chunks; nothing here
//! knows about decoding, files or threads.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// File magic (`"PLWF"`).
pub const MAGIC: [u8; 4] = *b"PLWF";

/// Format version written by [`encode_to_vec`].
pub const VERSION: u8 = 1;

/// Header size in bytes (payload starts here).
pub const HEADER_LEN: usize = 48;

/// Bucket size written by WAVE_01 (20 ms).
pub const BUCKET_MS_DEFAULT: u16 = 20;

/// Smallest `bucket_ms` a reader accepts.
pub const BUCKET_MS_MIN: u16 = 5;

/// Largest `bucket_ms` a reader accepts.
pub const BUCKET_MS_MAX: u16 = 100;

/// Hard cap on the payload (≈ 22 h at 20 ms). Anything larger is treated as corrupt.
pub const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// `flags` bit 0: the analyser reached a clean EOF.
pub const FLAG_COMPLETE: u8 = 1;

/// Peaks below this amplitude are treated as digital silence.
pub const SILENCE_FLOOR: f32 = 1e-6;

/// Quietest level the stored byte can express.
pub const DB_FLOOR: f32 = -60.0;

/// A waveform sidecar: the header fields plus one dB position per bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waveform {
    pub flags: u8,
    /// Bucket duration in ms (`20` for v1).
    pub bucket_ms: u16,
    /// Decoded length (authoritative for scrolling).
    pub duration_ms: u64,
    /// Media file size at analysis time.
    pub source_size: u64,
    /// Media mtime in ns since the Unix epoch, `-1` when unknown.
    pub source_mtime_ns: i64,
    /// Decoded stream rate (informational).
    pub sample_rate_hz: u32,
    /// Decoded channel count (informational).
    pub channels: u16,
    /// One `u8` per bucket, `0` (silence) ..= `255` (full scale).
    pub peaks: Vec<u8>,
}

/// The 48-byte header on its own (cheap check without reading the payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaveformHeader {
    pub flags: u8,
    pub bucket_ms: u16,
    pub bucket_count: u32,
    pub duration_ms: u64,
    pub source_size: u64,
    pub source_mtime_ns: i64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub payload_fnv: u32,
}

impl Waveform {
    /// A complete waveform (`COMPLETE` set) with the given peaks.
    #[allow(clippy::too_many_arguments)]
    pub fn new_complete(
        bucket_ms: u16,
        duration_ms: u64,
        sample_rate_hz: u32,
        channels: u16,
        source_size: u64,
        source_mtime_ns: i64,
        peaks: Vec<u8>,
    ) -> Self {
        Self {
            flags: FLAG_COMPLETE,
            bucket_ms,
            duration_ms,
            source_size,
            source_mtime_ns,
            sample_rate_hz,
            channels,
            peaks,
        }
    }

    /// Whether the analyser reached a clean EOF (an interrupted run is stale).
    pub fn is_complete(&self) -> bool {
        self.flags & FLAG_COMPLETE != 0
    }

    /// Number of stored buckets.
    pub fn bucket_count(&self) -> u32 {
        self.peaks.len() as u32
    }

    /// The header this waveform encodes (computes the payload checksum).
    pub fn header(&self) -> WaveformHeader {
        WaveformHeader {
            flags: self.flags,
            bucket_ms: self.bucket_ms,
            bucket_count: self.bucket_count(),
            duration_ms: self.duration_ms,
            source_size: self.source_size,
            source_mtime_ns: self.source_mtime_ns,
            sample_rate_hz: self.sample_rate_hz,
            channels: self.channels,
            payload_fnv: fnv1a32(&self.peaks),
        }
    }

    /// Peak dB position at `position_ms` (`None` outside the track).
    pub fn peak_at_ms(&self, position_ms: u64) -> Option<u8> {
        let idx = (position_ms / u64::from(self.bucket_ms.max(1))) as usize;
        self.peaks.get(idx).copied()
    }
}

impl WaveformHeader {
    /// Whether this header describes the media file with `size`/`mtime_ns`.
    ///
    /// An unknown mtime (`-1`, e.g. a filesystem that cannot report one) only compares
    /// the size, because there is nothing else to compare.
    pub fn matches_source(&self, size: u64, mtime_ns: i64) -> bool {
        self.source_size == size && (mtime_ns < 0 || self.source_mtime_ns == mtime_ns)
    }

    /// Complete (the analyser reached a clean EOF).
    pub fn is_complete(&self) -> bool {
        self.flags & FLAG_COMPLETE != 0
    }
}

/// Why a `.wfm` file was rejected. Every variant is a plain `Err`; the reader never panics.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WaveformError {
    #[error("waveform data too short (header is 48 bytes)")]
    TooShort { len: usize },
    #[error("bad waveform magic")]
    BadMagic,
    #[error("unsupported waveform version")]
    UnsupportedVersion { version: u8 },
    #[error("bad waveform bucket_ms (must be 5..=100)")]
    BadBucketMs { bucket_ms: u16 },
    #[error("waveform payload too large (cap is 4 MiB)")]
    TooLarge { bucket_count: u32 },
    #[error("waveform payload length mismatch")]
    LengthMismatch { declared: u32, actual: usize },
    #[error("waveform checksum mismatch")]
    Checksum,
}

/// Serialise a waveform: 48-byte header, then `peaks`.
pub fn encode_to_vec(waveform: &Waveform) -> Vec<u8> {
    let header = waveform.header();
    let mut out = Vec::with_capacity(HEADER_LEN + waveform.peaks.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(header.flags);
    out.extend_from_slice(&header.bucket_ms.to_le_bytes());
    out.extend_from_slice(&header.bucket_count.to_le_bytes());
    out.extend_from_slice(&header.duration_ms.to_le_bytes());
    out.extend_from_slice(&header.source_size.to_le_bytes());
    out.extend_from_slice(&header.source_mtime_ns.to_le_bytes());
    out.extend_from_slice(&header.sample_rate_hz.to_le_bytes());
    out.extend_from_slice(&header.channels.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&header.payload_fnv.to_le_bytes());
    debug_assert_eq!(out.len(), HEADER_LEN);
    out.extend_from_slice(&waveform.peaks);
    out
}

/// Parse and validate the header of `data` (at least [`HEADER_LEN`] bytes).
///
/// Rejects a bucket count over [`MAX_PAYLOAD_BYTES`] **before** the caller allocates from
/// it: the header is attacker-controlled on a foreign stick.
pub fn decode_header(data: &[u8]) -> Result<WaveformHeader, WaveformError> {
    if data.len() < HEADER_LEN {
        return Err(WaveformError::TooShort { len: data.len() });
    }
    if data[0..4] != MAGIC {
        return Err(WaveformError::BadMagic);
    }
    let version = data[4];
    if version != VERSION {
        return Err(WaveformError::UnsupportedVersion { version });
    }
    let flags = data[5];
    let bucket_ms = u16::from_le_bytes([data[6], data[7]]);
    if !(BUCKET_MS_MIN..=BUCKET_MS_MAX).contains(&bucket_ms) {
        return Err(WaveformError::BadBucketMs { bucket_ms });
    }
    let bucket_count = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    if bucket_count as usize > MAX_PAYLOAD_BYTES {
        return Err(WaveformError::TooLarge { bucket_count });
    }
    Ok(WaveformHeader {
        flags,
        bucket_ms,
        bucket_count,
        duration_ms: u64::from_le_bytes(data[12..20].try_into().expect("slice")),
        source_size: u64::from_le_bytes(data[20..28].try_into().expect("slice")),
        source_mtime_ns: i64::from_le_bytes(data[28..36].try_into().expect("slice")),
        sample_rate_hz: u32::from_le_bytes(data[36..40].try_into().expect("slice")),
        channels: u16::from_le_bytes(data[40..42].try_into().expect("slice")),
        payload_fnv: u32::from_le_bytes(data[44..48].try_into().expect("slice")),
    })
}

/// Parse and validate a whole `.wfm` byte string.
///
/// Checks magic, version, `bucket_ms`, the payload cap, the exact payload length and the
/// FNV-1a checksum. Any failure is an `Err` — never a panic, never a partial value.
pub fn decode(data: &[u8]) -> Result<Waveform, WaveformError> {
    let header = decode_header(data)?;
    let payload = &data[HEADER_LEN..];
    if payload.len() != header.bucket_count as usize {
        return Err(WaveformError::LengthMismatch {
            declared: header.bucket_count,
            actual: payload.len(),
        });
    }
    if fnv1a32(payload) != header.payload_fnv {
        return Err(WaveformError::Checksum);
    }
    Ok(Waveform {
        flags: header.flags,
        bucket_ms: header.bucket_ms,
        duration_ms: header.duration_ms,
        source_size: header.source_size,
        source_mtime_ns: header.source_mtime_ns,
        sample_rate_hz: header.sample_rate_hz,
        channels: header.channels,
        peaks: payload.to_vec(),
    })
}

/// FNV-1a (32-bit) over `bytes` — the payload checksum, written by hand (no crate).
pub fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Map one bucket peak (linear amplitude) to its stored dB position.
///
/// `20*log10(max(peak, 1e-6))` clamped to `[-60, 0]` dB, mapped linearly to `0..=255`:
/// digital silence is `0`, full scale is `255`.
pub fn peak_to_byte(peak: f32) -> u8 {
    let amplitude = peak.abs().max(SILENCE_FLOOR);
    let db = (20.0 * amplitude.log10()).clamp(DB_FLOOR, 0.0);
    let frac = (db - DB_FLOOR) / (0.0 - DB_FLOOR);
    (frac * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Incremental peak accumulator: feed decoded interleaved `f32`, get one `u8` per bucket.
///
/// The peak of a bucket is `max(|L|, |R|)` across all channels — a max, never a downmix
/// (a downmix cancels out-of-phase bass).
#[derive(Debug, Clone)]
pub struct Accumulator {
    sample_rate_hz: u32,
    channels: u16,
    bucket_ms: u16,
    frames_per_bucket: u64,
    frames_in_bucket: u64,
    peak_in_bucket: f32,
    frames_total: u64,
    peaks: Vec<u8>,
    /// The payload cap was hit: the caller must not write this waveform.
    truncated: bool,
    finished: bool,
}

impl Accumulator {
    /// Create an accumulator for a stream of `sample_rate_hz` / `channels`.
    ///
    /// `bucket_ms` is clamped to the readable range so the writer cannot produce a file
    /// its own reader would reject.
    pub fn new(sample_rate_hz: u32, channels: u16, bucket_ms: u16) -> Self {
        let bucket_ms = bucket_ms.clamp(BUCKET_MS_MIN, BUCKET_MS_MAX);
        let rate = u64::from(sample_rate_hz.max(1));
        // Nearest whole frame so 44.1 kHz / 20 ms is exactly 882 frames.
        let frames_per_bucket = ((rate * u64::from(bucket_ms) + 500) / 1000).max(1);
        Self {
            sample_rate_hz: sample_rate_hz.max(1),
            channels: channels.max(1),
            bucket_ms,
            frames_per_bucket,
            frames_in_bucket: 0,
            peak_in_bucket: 0.0,
            frames_total: 0,
            peaks: Vec::new(),
            truncated: false,
            finished: false,
        }
    }

    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn bucket_ms(&self) -> u16 {
        self.bucket_ms
    }

    /// Frames per bucket as used by the bucketing rule.
    pub fn frames_per_bucket(&self) -> u64 {
        self.frames_per_bucket
    }

    /// Buckets emitted so far (the partial one only after [`Self::finish`]).
    pub fn bucket_count(&self) -> usize {
        self.peaks.len()
    }

    /// Decoded length in ms from the frame count (not from container tags).
    pub fn duration_ms(&self) -> u64 {
        self.frames_total.saturating_mul(1000) / u64::from(self.sample_rate_hz)
    }

    /// True when the payload cap was reached: the waveform is incomplete and must be
    /// reported as a failure instead of written.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Feed interleaved samples. Trailing samples that do not form a whole frame are
    /// ignored (a decoder never splits a frame, but callers may).
    pub fn push(&mut self, interleaved: &[f32]) {
        if self.finished {
            return;
        }
        let channels = usize::from(self.channels);
        for frame in interleaved.chunks_exact(channels) {
            let mut peak = 0.0f32;
            for &s in frame {
                let a = s.abs();
                // `f32::max` ignores NaN, so a broken sample cannot poison the peak.
                peak = peak.max(a);
            }
            self.peak_in_bucket = self.peak_in_bucket.max(peak);
            self.frames_in_bucket += 1;
            self.frames_total += 1;
            if self.frames_in_bucket >= self.frames_per_bucket {
                self.flush_bucket();
            }
        }
    }

    /// Emit the trailing partial bucket (a sample count that is not a whole bucket).
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.frames_in_bucket > 0 {
            self.flush_bucket();
        }
    }

    /// Consume the accumulator, returning the peaks.
    pub fn into_peaks(self) -> Vec<u8> {
        self.peaks
    }

    fn flush_bucket(&mut self) {
        self.frames_in_bucket = 0;
        if self.truncated {
            return;
        }
        if self.peaks.len() >= MAX_PAYLOAD_BYTES {
            self.truncated = true;
            return;
        }
        self.peaks.push(peak_to_byte(self.peak_in_bucket));
        self.peak_in_bucket = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, frames: usize, amplitude: f32, freq: f32) -> Vec<f32> {
        (0..frames)
            .map(|i| {
                let t = i as f32 / rate as f32;
                amplitude * (std::f32::consts::TAU * freq * t).sin()
            })
            .collect()
    }

    fn stereo(samples: Vec<f32>) -> Vec<f32> {
        samples.into_iter().flat_map(|s| [s, s]).collect()
    }

    fn accumulate(rate: u32, channels: u16, interleaved: &[f32]) -> Accumulator {
        let mut acc = Accumulator::new(rate, channels, BUCKET_MS_DEFAULT);
        acc.push(interleaved);
        acc.finish();
        acc
    }

    #[test]
    fn round_trip_preserves_every_field_and_byte_length() {
        let w = Waveform::new_complete(
            BUCKET_MS_DEFAULT,
            1_000,
            44_100,
            2,
            123_456,
            1_700_000_000_000_000_000,
            vec![0, 1, 128, 200, 255],
        );
        let bytes = encode_to_vec(&w);
        assert_eq!(bytes.len(), HEADER_LEN + 5);
        assert_eq!(decode(&bytes).unwrap(), w);
        assert_eq!(decode_header(&bytes).unwrap(), w.header());
        assert_eq!(w.bucket_count(), 5);
        assert!(w.is_complete());
        assert_eq!(w.peak_at_ms(0), Some(0));
        assert_eq!(w.peak_at_ms(20 * 4 + 19), Some(255));
        assert_eq!(w.peak_at_ms(100), None);
    }

    #[test]
    fn header_is_little_endian_at_the_documented_offsets() {
        let w =
            Waveform::new_complete(20, 0x1122_3344_5566_7788, 44_100, 2, 0x0102, -1, vec![7, 9]);
        let b = encode_to_vec(&w);
        assert_eq!(&b[0..4], b"PLWF");
        assert_eq!(b[4], 1);
        assert_eq!(b[5], FLAG_COMPLETE);
        assert_eq!(&b[6..8], &20u16.to_le_bytes());
        assert_eq!(&b[8..12], &2u32.to_le_bytes());
        assert_eq!(&b[12..20], &0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(&b[20..28], &0x0102u64.to_le_bytes());
        assert_eq!(&b[28..36], &(-1i64).to_le_bytes());
        assert_eq!(&b[36..40], &44_100u32.to_le_bytes());
        assert_eq!(&b[40..42], &2u16.to_le_bytes());
        assert_eq!(&b[42..44], &[0, 0]);
        assert_eq!(&b[44..48], &fnv1a32(&[7, 9]).to_le_bytes());
        assert_eq!(&b[HEADER_LEN..], &[7, 9]);
    }

    #[test]
    fn bucket_boundaries_one_second_is_fifty_buckets() {
        let acc = accumulate(44_100, 2, &stereo(sine(44_100, 44_100, 0.5, 440.0)));
        assert_eq!(acc.frames_per_bucket(), 882);
        assert_eq!(acc.bucket_count(), 50);
        assert_eq!(acc.duration_ms(), 1000);

        let acc = accumulate(44_000, 2, &stereo(sine(44_000, 44_000, 0.5, 440.0)));
        assert_eq!(acc.frames_per_bucket(), 880);
        assert_eq!(acc.bucket_count(), 50);
        assert_eq!(acc.duration_ms(), 1000);
    }

    #[test]
    fn the_sample_on_a_bucket_edge_belongs_to_the_next_bucket() {
        // Silence for exactly one bucket, then one loud frame.
        let mut acc = Accumulator::new(44_100, 1, BUCKET_MS_DEFAULT);
        let mut samples = vec![0.0f32; 882];
        samples.push(1.0);
        acc.push(&samples);
        assert_eq!(acc.bucket_count(), 1, "the edge frame is not in bucket 0");
        acc.finish();
        let peaks = acc.into_peaks();
        assert_eq!(peaks.len(), 2);
        assert_eq!(peaks[0], 0);
        assert_eq!(peaks[1], 255);
    }

    #[test]
    fn a_partial_trailing_bucket_is_emitted() {
        let acc = accumulate(44_100, 1, &vec![0.25f32; 900]);
        assert_eq!(acc.bucket_count(), 2, "882 + 18 frames");
        assert_eq!(acc.duration_ms(), 20);
    }

    #[test]
    fn full_scale_sine_is_top_of_range() {
        let acc = accumulate(44_100, 2, &stereo(sine(44_100, 44_100, 1.0, 440.0)));
        let peaks = acc.into_peaks();
        assert_eq!(peaks.len(), 50);
        assert!(peaks.iter().all(|&p| p >= 250), "{peaks:?}");
    }

    #[test]
    fn minus_six_dbfs_is_about_ninety_percent_of_the_range() {
        let peak = peak_to_byte(0.5);
        assert!(
            (228..=231).contains(&peak),
            "-6 dBFS should map to ~0.9 of the range, got {peak}"
        );
    }

    #[test]
    fn silence_is_zero() {
        let acc = accumulate(44_100, 2, &vec![0.0f32; 44_100 * 2]);
        assert!(acc.into_peaks().iter().all(|&p| p == 0));
        assert_eq!(peak_to_byte(0.0), 0);
        assert_eq!(peak_to_byte(SILENCE_FLOOR / 2.0), 0);
    }

    #[test]
    fn out_of_phase_channels_never_cancel() {
        // L = +0.5, R = -0.5: a downmix would be silence, a max is 0.5.
        let mut interleaved = Vec::new();
        for _ in 0..44_100 {
            interleaved.push(0.5);
            interleaved.push(-0.5);
        }
        let acc = accumulate(44_100, 2, &interleaved);
        let peaks = acc.into_peaks();
        assert!(peaks.iter().all(|&p| p == peak_to_byte(0.5)));
        assert!(peaks[0] > 0);
    }

    #[test]
    fn nan_samples_do_not_poison_a_bucket() {
        let acc = accumulate(44_100, 1, &[f32::NAN, f32::NAN, 0.5, 0.5]);
        let peaks = acc.into_peaks();
        assert_eq!(peaks[0], peak_to_byte(0.5));
    }

    #[test]
    fn empty_stream_is_a_complete_zero_bucket_waveform() {
        let acc = accumulate(44_100, 2, &[]);
        assert_eq!(acc.bucket_count(), 0);
        assert_eq!(acc.duration_ms(), 0);
        assert!(!acc.is_truncated());
    }

    #[test]
    fn reject_bad_magic() {
        let mut bytes =
            encode_to_vec(&Waveform::new_complete(20, 1000, 44_100, 2, 1, 1, vec![1, 2]));
        bytes[0] = b'X';
        assert_eq!(decode(&bytes), Err(WaveformError::BadMagic));
        assert_eq!(decode_header(&bytes), Err(WaveformError::BadMagic));
    }

    #[test]
    fn reject_version_two() {
        let mut bytes =
            encode_to_vec(&Waveform::new_complete(20, 1000, 44_100, 2, 1, 1, vec![1]));
        bytes[4] = 2;
        assert_eq!(
            decode(&bytes),
            Err(WaveformError::UnsupportedVersion { version: 2 })
        );
    }

    #[test]
    fn reject_bucket_ms_outside_the_range() {
        let base = encode_to_vec(&Waveform::new_complete(20, 1000, 44_100, 2, 1, 1, vec![1]));
        for bad in [0u16, 4, 101, u16::MAX] {
            let mut bytes = base.clone();
            bytes[6..8].copy_from_slice(&bad.to_le_bytes());
            assert_eq!(
                decode(&bytes),
                Err(WaveformError::BadBucketMs { bucket_ms: bad }),
                "bucket_ms {bad}"
            );
        }
        for ok in [BUCKET_MS_MIN, BUCKET_MS_MAX] {
            let mut bytes = base.clone();
            bytes[6..8].copy_from_slice(&ok.to_le_bytes());
            assert!(decode(&bytes).is_ok(), "bucket_ms {ok}");
        }
    }

    #[test]
    fn reject_bucket_count_larger_than_the_payload() {
        let mut bytes =
            encode_to_vec(&Waveform::new_complete(20, 1000, 44_100, 2, 1, 1, vec![1, 2, 3]));
        bytes[8..12].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(
            decode(&bytes),
            Err(WaveformError::LengthMismatch {
                declared: 7,
                actual: 3
            })
        );
    }

    #[test]
    fn reject_payload_over_the_cap_without_allocating() {
        let peaks = vec![0u8; MAX_PAYLOAD_BYTES + 1];
        let bytes = encode_to_vec(&Waveform::new_complete(20, 0, 44_100, 2, 1, 1, peaks));
        assert_eq!(
            decode(&bytes),
            Err(WaveformError::TooLarge {
                bucket_count: (MAX_PAYLOAD_BYTES + 1) as u32
            })
        );
        // The header alone is rejected too (the file is untrusted).
        assert_eq!(
            decode_header(&bytes),
            Err(WaveformError::TooLarge {
                bucket_count: (MAX_PAYLOAD_BYTES + 1) as u32
            })
        );
    }

    #[test]
    fn reject_fnv_mismatch() {
        let mut bytes =
            encode_to_vec(&Waveform::new_complete(20, 1000, 44_100, 2, 1, 1, vec![1, 2, 3]));
        bytes[HEADER_LEN + 1] ^= 0xff;
        assert_eq!(decode(&bytes), Err(WaveformError::Checksum));
        let mut bytes =
            encode_to_vec(&Waveform::new_complete(20, 1000, 44_100, 2, 1, 1, vec![1]));
        bytes[44] ^= 0x01;
        assert_eq!(decode(&bytes), Err(WaveformError::Checksum));
    }

    #[test]
    fn reject_a_file_shorter_than_the_header() {
        let bytes = encode_to_vec(&Waveform::new_complete(20, 1000, 44_100, 2, 1, 1, vec![]));
        for cut in [0usize, 1, HEADER_LEN - 1] {
            assert_eq!(
                decode(&bytes[..cut]),
                Err(WaveformError::TooShort { len: cut })
            );
        }
        // Exactly the header, no payload: a valid zero-bucket file.
        assert_eq!(decode(&bytes).unwrap().peaks, Vec::<u8>::new());
    }

    #[test]
    fn truncated_flag_is_readable_but_not_complete() {
        let mut w = Waveform::new_complete(20, 1000, 44_100, 2, 1, 1, vec![1]);
        w.flags = 0;
        let decoded = decode(&encode_to_vec(&w)).unwrap();
        assert!(!decoded.is_complete());
        assert!(!decoded.header().is_complete());
    }

    #[test]
    fn header_matches_source_compares_size_and_mtime() {
        let w = Waveform::new_complete(20, 1000, 44_100, 2, 100, 555, vec![]);
        let h = w.header();
        assert!(h.matches_source(100, 555));
        assert!(!h.matches_source(101, 555));
        assert!(!h.matches_source(100, 556));
        // Unknown now: the size is all we can check.
        assert!(h.matches_source(100, -1));
    }

    #[test]
    fn accumulator_caps_the_payload_and_reports_truncation() {
        // 1 kHz / 5 ms = 5 frames per bucket, so the 4 MiB cap is reached quickly.
        let mut acc = Accumulator::new(1_000, 1, BUCKET_MS_MIN);
        assert_eq!(acc.frames_per_bucket(), 5);
        let chunk = vec![0.1f32; 1_000_000];
        for _ in 0..22 {
            acc.push(&chunk);
        }
        acc.finish();
        assert!(acc.is_truncated());
        assert!(acc.bucket_count() <= MAX_PAYLOAD_BYTES);
    }
}
