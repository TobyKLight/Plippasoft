//! MP3 frame index (WARP_11): makes a **backward seek** in a long MP3 cheap.
//!
//! Symphonia's accurate MP3 seek restarts from byte 0 whenever the target is behind the
//! reader, so a rewind or a back-window on a 1-hour file on a USB stick would parse the whole
//! prefix. The index is a coarse map `(sample_ts, byte_offset)`, one entry every
//! [`MP3_INDEX_STRIDE_FRAMES`] MPEG frames, built by a low-priority background walk of the
//! frame headers after load. An indexed seek opens a [`OffsetSource`] that starts the reader
//! at an entry at least [`MP3_WARMUP_FRAMES`] frames before the target (reference frames for
//! the bit reservoir), and Symphonia's own accurate seek then walks only the remaining
//! (at most one stride of) frames.
//!
//! In memory only, rebuilt per load. The builder and the seek take a **source argument**
//! (any `Read + Seek` / [`MediaSource`] factory), not "the current file", so WARP_19 can index
//! the adjacent track. WAV / FLAC / MP4 need no index.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use symphonia::core::io::MediaSource;

pub use crate::window::MP3_INDEX_STRIDE_FRAMES;

/// Reference frames kept between an index entry and the seek target (Symphonia keeps up to 4
/// reference frames for the bit reservoir).
pub const MP3_WARMUP_FRAMES: u64 = 4;

/// Walk read size.
const WALK_BUF_BYTES: usize = 64 * 1024;

/// One index entry: the timestamp (in samples, Symphonia's packet time base) of the MPEG frame
/// starting at `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    pub ts: u64,
    pub offset: u64,
}

/// The index of one MP3 file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mp3Index {
    pub stride_frames: u32,
    pub samples_per_frame: u32,
    pub sample_rate_hz: u32,
    /// Audio frames walked (the Xing/Info frame is not one) times `samples_per_frame`.
    pub total_samples: u64,
    pub entries: Vec<IndexEntry>,
}

impl Mp3Index {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Approximate memory of the entries in bytes.
    pub fn bytes(&self) -> usize {
        self.entries.len() * std::mem::size_of::<IndexEntry>()
    }

    /// The entry an indexed seek to `target_ts` should start from: the last one that is at
    /// least [`MP3_WARMUP_FRAMES`] frames before the target. `None` when the target is so near
    /// the start that the plain accurate seek (a few frames from byte 0) is as cheap.
    pub fn entry_for(&self, target_ts: u64) -> Option<IndexEntry> {
        let margin = MP3_WARMUP_FRAMES * u64::from(self.samples_per_frame);
        let limit = target_ts.checked_sub(margin)?;
        let i = self.entries.partition_point(|e| e.ts <= limit);
        let e = self.entries.get(i.checked_sub(1)?)?;
        // The first entry is the stream start: no gain over a plain seek.
        (e.offset > 0 && e.ts > 0).then_some(*e)
    }
}

/// Why an index could not be built (the caller falls back to the accurate seek).
#[derive(Debug)]
pub enum IndexError {
    Io(io::Error),
    /// Not a Layer III stream, free-format, or no frame found.
    Unsupported(&'static str),
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::Io(e) => write!(f, "index io error: {e}"),
            IndexError::Unsupported(s) => write!(f, "index unsupported: {s}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<io::Error> for IndexError {
    fn from(e: io::Error) -> Self {
        IndexError::Io(e)
    }
}

#[derive(Debug, Clone, Copy)]
struct FrameHeader {
    frame_len: usize,
    samples: u32,
    sample_rate_hz: u32,
    /// Offset of a possible Xing/Info tag from the frame start.
    xing_offset: usize,
}

const BITRATES_V1_L3: [u32; 15] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];
const BITRATES_V2_L3: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

/// Parse an MPEG audio frame header (Layer III only); `None` if it is not a valid one.
fn parse_header(h: [u8; 4]) -> Option<FrameHeader> {
    if h[0] != 0xFF || h[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 3; // 0 = 2.5, 2 = 2, 3 = 1
    let layer = (h[1] >> 1) & 3; // 1 = Layer III
    if version == 1 || layer != 1 {
        return None;
    }
    let crc = h[1] & 1 == 0;
    let br_idx = (h[2] >> 4) as usize;
    let sr_idx = ((h[2] >> 2) & 3) as usize;
    if br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    let padding = u32::from((h[2] >> 1) & 1);
    let mono = h[3] >> 6 == 3;
    let (bitrate, sr, samples, coef, side) = if version == 3 {
        let sr = [44_100, 48_000, 32_000][sr_idx];
        (
            BITRATES_V1_L3[br_idx],
            sr,
            1152,
            144,
            if mono { 17 } else { 32 },
        )
    } else {
        let base = [22_050, 24_000, 16_000][sr_idx];
        let sr = if version == 0 { base / 2 } else { base };
        (
            BITRATES_V2_L3[br_idx],
            sr,
            576,
            72,
            if mono { 9 } else { 17 },
        )
    };
    let frame_len = (coef * bitrate * 1000 / sr + padding) as usize;
    if frame_len < 4 {
        return None;
    }
    Some(FrameHeader {
        frame_len,
        samples,
        sample_rate_hz: sr,
        xing_offset: 4 + if crc { 2 } else { 0 } + side,
    })
}

/// Minimal buffered reader over a sequential `Read` that tracks the absolute position.
struct Walker<R: Read> {
    r: R,
    buf: Vec<u8>,
    pos: usize,
    filled: usize,
    abs: u64,
    eof: bool,
}

impl<R: Read> Walker<R> {
    fn new(r: R) -> Self {
        Self {
            r,
            buf: vec![0; WALK_BUF_BYTES],
            pos: 0,
            filled: 0,
            abs: 0,
            eof: false,
        }
    }

    /// Ensure at least `n` bytes are buffered; returns how many are (fewer only at the end).
    fn fill(&mut self, n: usize) -> io::Result<usize> {
        debug_assert!(n <= self.buf.len());
        while self.filled - self.pos < n && !self.eof {
            if self.pos > 0 {
                self.buf.copy_within(self.pos..self.filled, 0);
                self.filled -= self.pos;
                self.pos = 0;
            }
            let got = self.r.read(&mut self.buf[self.filled..])?;
            if got == 0 {
                self.eof = true;
            }
            self.filled += got;
        }
        Ok(self.filled - self.pos)
    }

    fn peek(&self, n: usize) -> &[u8] {
        &self.buf[self.pos..self.pos + n.min(self.filled - self.pos)]
    }

    /// Skip `n` bytes (reads and drops what is not buffered).
    fn skip(&mut self, mut n: u64) -> io::Result<()> {
        while n > 0 {
            let have = (self.filled - self.pos) as u64;
            if have == 0 {
                if self.fill(1)? == 0 {
                    return Ok(());
                }
                continue;
            }
            let d = have.min(n);
            self.pos += d as usize;
            self.abs += d;
            n -= d;
        }
        Ok(())
    }
}

/// Walk the frame headers of an MP3 stream (`Read` from byte 0) and build the index with one
/// entry every `stride_frames` frames. Skips an ID3v2 tag and the Xing/Info/VBRI frame (it
/// is not an audio packet for Symphonia, so it does not advance the timestamp).
pub fn build_mp3_index<R: Read>(source: R, stride_frames: u32) -> Result<Mp3Index, IndexError> {
    let stride = u64::from(stride_frames.max(1));
    let mut w = Walker::new(source);

    // ID3v2: "ID3", version(2), flags(1), size(4 syncsafe).
    if w.fill(10)? >= 10 && w.peek(3) == b"ID3" {
        let h = w.peek(10);
        let size = (u64::from(h[6] & 0x7F) << 21)
            | (u64::from(h[7] & 0x7F) << 14)
            | (u64::from(h[8] & 0x7F) << 7)
            | u64::from(h[9] & 0x7F);
        let footer = if h[5] & 0x10 != 0 { 10 } else { 0 };
        w.skip(10 + size + footer)?;
    }

    let mut entries = Vec::new();
    let mut ts = 0u64;
    let mut n_frames = 0u64;
    let mut spf = 0u32;
    let mut rate = 0u32;
    let mut first = true;
    loop {
        let have = w.fill(4)?;
        if have < 4 {
            break;
        }
        let h4 = {
            let p = w.peek(4);
            [p[0], p[1], p[2], p[3]]
        };
        let Some(h) = parse_header(h4) else {
            // Junk (padding, a tag): resync byte by byte.
            w.skip(1)?;
            continue;
        };
        // Confirm the sync by the next header (a false sync inside data is common).
        let want = (h.frame_len + 4).min(w.buf.len());
        let have = w.fill(want)?;
        if have >= h.frame_len + 4 {
            let p = w.peek(h.frame_len + 4);
            let n = [
                p[h.frame_len],
                p[h.frame_len + 1],
                p[h.frame_len + 2],
                p[h.frame_len + 3],
            ];
            if parse_header(n).is_none() {
                w.skip(1)?;
                continue;
            }
        } else if have < h.frame_len {
            break; // truncated last frame
        }
        if first {
            first = false;
            spf = h.samples;
            rate = h.sample_rate_hz;
            let body = w.peek(h.frame_len.min(h.xing_offset + 4));
            let is_info = body.len() >= h.xing_offset + 4
                && matches!(&body[h.xing_offset..h.xing_offset + 4], b"Xing" | b"Info");
            let is_vbri = h.frame_len >= 40 && w.peek(40).get(36..40) == Some(b"VBRI".as_ref());
            if is_info || is_vbri {
                w.skip(h.frame_len as u64)?;
                continue;
            }
        }
        if n_frames % stride == 0 {
            entries.push(IndexEntry { ts, offset: w.abs });
        }
        n_frames += 1;
        ts += u64::from(h.samples);
        w.skip(h.frame_len as u64)?;
    }
    if entries.is_empty() {
        return Err(IndexError::Unsupported("no Layer III frames"));
    }
    Ok(Mp3Index {
        stride_frames: stride as u32,
        samples_per_frame: spf,
        sample_rate_hz: rate,
        total_samples: ts,
        entries,
    })
}

/// [`build_mp3_index`] over a file path.
pub fn build_mp3_index_path(path: &Path, stride_frames: u32) -> Result<Mp3Index, IndexError> {
    build_mp3_index(File::open(path)?, stride_frames)
}

/// A [`MediaSource`] that presents `inner` as if it started at byte `offset`.
pub struct OffsetSource {
    inner: Box<dyn MediaSource>,
    offset: u64,
}

impl OffsetSource {
    pub fn new(mut inner: Box<dyn MediaSource>, offset: u64) -> io::Result<Self> {
        inner.seek(SeekFrom::Start(offset))?;
        Ok(Self { inner, offset })
    }
}

impl Read for OffsetSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for OffsetSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let abs = match pos {
            SeekFrom::Start(p) => self.inner.seek(SeekFrom::Start(p + self.offset))?,
            other => self.inner.seek(other)?,
        };
        Ok(abs.saturating_sub(self.offset))
    }
}

impl MediaSource for OffsetSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len().map(|l| l.saturating_sub(self.offset))
    }
}

/// Where a background-built index lands (shared with the decode worker).
#[derive(Debug, Default)]
pub struct IndexSlot {
    index: OnceLock<Arc<Mp3Index>>,
    done: AtomicBool,
    failed: AtomicBool,
    entries: AtomicU64,
}

impl IndexSlot {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The index once the walk has finished.
    pub fn get(&self) -> Option<Arc<Mp3Index>> {
        self.index.get().cloned()
    }

    /// True once the walk has finished (successfully or not).
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    pub fn is_ready(&self) -> bool {
        self.index.get().is_some()
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    pub fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    fn finish(&self, r: Result<Mp3Index, IndexError>) {
        match r {
            Ok(idx) => {
                self.entries.store(idx.len() as u64, Ordering::Relaxed);
                let _ = self.index.set(Arc::new(idx));
            }
            Err(e) => {
                eprintln!("plippa-audio: mp3 index not built: {e}");
                self.failed.store(true, Ordering::Relaxed);
            }
        }
        self.done.store(true, Ordering::Release);
    }
}

/// Lower the calling thread's priority (`nice +10`) so the walk never competes with playback.
#[cfg(unix)]
fn lower_thread_priority() {
    // SAFETY: plain syscalls on the calling thread; failure is ignored.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        let _ = libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
}

#[cfg(not(unix))]
fn lower_thread_priority() {}

/// Build the index of `open()`'s stream on a background thread at low priority. `open` returns
/// a fresh full-file source each call (so any file, including the adjacent track, can be
/// indexed). The result lands in the returned slot.
pub fn spawn_index_build<F>(open: F, stride_frames: u32) -> Arc<IndexSlot>
where
    F: FnOnce() -> io::Result<Box<dyn MediaSource>> + Send + 'static,
{
    let slot = IndexSlot::new();
    let s = Arc::clone(&slot);
    let spawned = std::thread::Builder::new()
        .name("plippa-mp3-index".into())
        .spawn(move || {
            lower_thread_priority();
            let r = match open() {
                Ok(src) => build_mp3_index(src, stride_frames),
                Err(e) => Err(IndexError::Io(e)),
            };
            s.finish(r);
        });
    if spawned.is_err() {
        slot.finish(Err(IndexError::Unsupported("thread spawn failed")));
    }
    slot
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake MPEG1 Layer III 128 kbps 44.1 kHz stereo frame (417/418 bytes) with a header.
    fn fake_frame(padding: bool, fill: u8) -> Vec<u8> {
        let len = if padding { 418 } else { 417 };
        let mut f = vec![fill; len];
        f[0] = 0xFF;
        f[1] = 0xFB; // MPEG1, Layer III, no CRC
        f[2] = 0x90 | if padding { 2 } else { 0 }; // 128 kbps, 44.1 kHz
        f[3] = 0x00; // stereo
        f
    }

    #[test]
    fn header_parse_known_frames() {
        let h = parse_header([0xFF, 0xFB, 0x90, 0x00]).unwrap();
        assert_eq!(
            (h.frame_len, h.samples, h.sample_rate_hz),
            (417, 1152, 44_100)
        );
        let h = parse_header([0xFF, 0xFB, 0x92, 0x00]).unwrap();
        assert_eq!(h.frame_len, 418);
        // 320 kbps 44.1 kHz: 1044 bytes.
        assert_eq!(
            parse_header([0xFF, 0xFB, 0xE0, 0x00]).unwrap().frame_len,
            1044
        );
        // MPEG2 22.05 kHz 64 kbps: 72 * 64000 / 22050 = 208.
        let h = parse_header([0xFF, 0xF3, 0x80, 0x00]).unwrap();
        assert_eq!(
            (h.frame_len, h.samples, h.sample_rate_hz),
            (208, 576, 22_050)
        );
        assert!(
            parse_header([0xFF, 0xFB, 0x00, 0x00]).is_none(),
            "free format"
        );
        assert!(
            parse_header([0xFF, 0xFB, 0xF0, 0x00]).is_none(),
            "bad bitrate"
        );
        assert!(parse_header([0xFF, 0xFD, 0x90, 0x00]).is_none(), "layer II");
        assert!(parse_header([0x12, 0x34, 0x56, 0x78]).is_none());
    }

    #[test]
    fn builder_skips_id3_and_xing_and_strides() {
        let mut data = Vec::new();
        // ID3v2 with 100 bytes of payload.
        data.extend_from_slice(b"ID3\x03\x00\x00\x00\x00\x00\x64");
        data.extend(std::iter::repeat(0u8).take(100));
        let first_audio = data.len() as u64;
        // Xing frame.
        let mut x = fake_frame(false, 0);
        x[36..40].copy_from_slice(b"Xing");
        data.extend_from_slice(&x);
        let audio_start = data.len() as u64;
        let mut offsets = Vec::new();
        for i in 0..25 {
            offsets.push(data.len() as u64);
            data.extend_from_slice(&fake_frame(i % 3 == 0, 0x11));
        }
        let idx = build_mp3_index(io::Cursor::new(&data), 10).unwrap();
        assert_eq!(first_audio + 417, audio_start);
        assert_eq!(idx.len(), 3, "frames 0, 10, 20");
        assert_eq!(
            idx.entries[0],
            IndexEntry {
                ts: 0,
                offset: audio_start
            }
        );
        assert_eq!(
            idx.entries[1],
            IndexEntry {
                ts: 10 * 1152,
                offset: offsets[10]
            }
        );
        assert_eq!(
            idx.entries[2],
            IndexEntry {
                ts: 20 * 1152,
                offset: offsets[20]
            }
        );
        assert_eq!(idx.total_samples, 25 * 1152);
        assert_eq!(idx.samples_per_frame, 1152);
    }

    #[test]
    fn entry_for_keeps_the_warmup_margin_and_ignores_the_stream_start() {
        let idx = Mp3Index {
            stride_frames: 100,
            samples_per_frame: 1152,
            sample_rate_hz: 44_100,
            total_samples: 1152 * 1000,
            entries: (0..10)
                .map(|i| IndexEntry {
                    ts: i * 100 * 1152,
                    offset: 1000 + i * 100_000,
                })
                .collect(),
        };
        // Near the start: plain seek.
        assert_eq!(idx.entry_for(50_000), None);
        // Exactly one stride + margin: the second entry.
        let t = 100 * 1152 + MP3_WARMUP_FRAMES * 1152;
        assert_eq!(idx.entry_for(t).unwrap().ts, 100 * 1152);
        // One sample less: still the first entry (which is the start): None.
        assert_eq!(idx.entry_for(t - 1), None);
        // Far: last entry that leaves the margin.
        assert_eq!(
            idx.entry_for(9 * 100 * 1152 + 5000).unwrap().ts,
            9 * 100 * 1152
        );
    }

    #[test]
    fn offset_source_translates_positions() {
        let data: Vec<u8> = (0..=255u8).collect();
        let inner = crate::index::tests::mem_source(data);
        let mut s = OffsetSource::new(inner, 100).unwrap();
        let mut b = [0u8; 4];
        s.read_exact(&mut b).unwrap();
        assert_eq!(b, [100, 101, 102, 103]);
        assert_eq!(s.seek(SeekFrom::Start(10)).unwrap(), 10);
        s.read_exact(&mut b).unwrap();
        assert_eq!(b, [110, 111, 112, 113]);
        assert_eq!(s.seek(SeekFrom::End(-1)).unwrap(), 155);
        assert_eq!(s.byte_len(), Some(156));
    }

    // ---- real files: the index against the accurate seek ----------------------------------

    use crate::stream::{SourceOpener, StreamDecoder};
    use crate::testsupport::{CountingSource, Lcg, SourceCounters};
    use plippa_core::Capabilities;
    use std::path::PathBuf;

    fn media(rel: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/testMedia");
        assert!(root.is_dir(), "test media missing: run scripts/fetch-testmedia.sh");
        root.join(rel)
    }

    /// Decoder over `path` whose every source open is counted (shared counters).
    fn counted_decoder(path: &Path) -> (StreamDecoder, SourceCounters) {
        let counters = SourceCounters::default();
        let c2 = counters.clone();
        let p = path.to_path_buf();
        let opener: SourceOpener = Arc::new(move || {
            let src = CountingSource::new_shared(File::open(&p)?, c2.clone())?;
            Ok(Box::new(src) as Box<dyn MediaSource>)
        });
        let d = StreamDecoder::open_with(
            path,
            opener,
            &Capabilities::desktop_mock(),
            crate::OverspecPolicy::default(),
        )
        .unwrap();
        (d, counters)
    }

    fn first_chunk(d: &mut StreamDecoder) -> Vec<f32> {
        d.next_chunk(2048).unwrap().unwrap().samples
    }

    fn index_matches_accurate(rel: &str) {
        let path = media(rel);
        let idx = Arc::new(build_mp3_index_path(&path, MP3_INDEX_STRIDE_FRAMES).unwrap());
        assert!(idx.len() > 10, "{rel}: {} entries", idx.len());
        let (mut acc, _) = counted_decoder(&path);
        let (mut ixd, _) = counted_decoder(&path);
        ixd.set_index(idx.clone());
        let total = idx.total_samples;
        let mut rng = Lcg::new(7);
        let mut indexed_used = 0;
        for _ in 0..100 {
            let f = rng.below(total - 5000);
            acc.seek_frame(f).unwrap();
            let before = ixd.indexed_seeks();
            ixd.seek_frame(f).unwrap();
            indexed_used += ixd.indexed_seeks() - before;
            assert_eq!(acc.position_frame(), ixd.position_frame(), "landing at {f}");
            let (a, b) = (first_chunk(&mut acc), first_chunk(&mut ixd));
            assert_eq!(a.len(), b.len());
            let max_diff = a
                .iter()
                .zip(&b)
                .map(|(x, y)| (x - y).abs())
                .fold(0.0f32, f32::max);
            assert!(max_diff < 1e-4, "{rel} seek {f}: max diff {max_diff}");
        }
        assert!(
            indexed_used > 80,
            "most seeks go through the index ({indexed_used})"
        );
    }

    #[test]
    fn indexed_seek_equals_accurate_seek_cbr320() {
        index_matches_accurate(
            "MP3 320/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3",
        );
    }

    #[test]
    fn indexed_seek_equals_accurate_seek_v0() {
        index_matches_accurate(
            "MP3 V0/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3",
        );
    }

    #[test]
    fn indexed_backward_seek_reads_far_fewer_bytes_than_the_accurate_seek() {
        let path = media("MP3 320/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3");
        let idx = Arc::new(build_mp3_index_path(&path, MP3_INDEX_STRIDE_FRAMES).unwrap());
        let late = idx.total_samples * 9 / 10;
        let mid = idx.total_samples / 2;
        let mut bytes = [0u64; 2];
        for (i, use_index) in [false, true].into_iter().enumerate() {
            let (mut d, counters) = counted_decoder(&path);
            if use_index {
                d.set_index(idx.clone());
            }
            d.seek_frame(late).unwrap();
            let _ = first_chunk(&mut d);
            counters.reset();
            d.seek_frame(mid).unwrap(); // backward
            let _ = first_chunk(&mut d);
            bytes[i] = counters.bytes_read();
        }
        let file_len = std::fs::metadata(&path).unwrap().len();
        eprintln!(
            "backward seek bytes: accurate {} indexed {} (file {file_len})",
            bytes[0], bytes[1]
        );
        assert!(
            bytes[0] > file_len / 3,
            "the plain seek re-reads the prefix"
        );
        assert!(
            bytes[1] * 4 < bytes[0],
            "indexed {} vs accurate {}",
            bytes[1],
            bytes[0]
        );
    }

    #[test]
    fn background_build_fills_the_slot_and_a_missing_file_fails_cleanly() {
        let path = media("MP3 V0/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3");
        let p = path.clone();
        let slot = spawn_index_build(
            move || Ok(Box::new(File::open(p)?) as Box<dyn MediaSource>),
            MP3_INDEX_STRIDE_FRAMES,
        );
        while !slot.is_done() {
            std::thread::yield_now();
        }
        assert!(slot.is_ready() && slot.entries() > 10 && !slot.failed());
        let bad = spawn_index_build(|| Err(io::Error::new(io::ErrorKind::NotFound, "gone")), 100);
        while !bad.is_done() {
            std::thread::yield_now();
        }
        assert!(bad.failed() && !bad.is_ready());
    }

    pub(super) fn mem_source(data: Vec<u8>) -> Box<dyn MediaSource> {
        struct Mem(io::Cursor<Vec<u8>>);
        impl Read for Mem {
            fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
                self.0.read(b)
            }
        }
        impl Seek for Mem {
            fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
                self.0.seek(p)
            }
        }
        impl MediaSource for Mem {
            fn is_seekable(&self) -> bool {
                true
            }
            fn byte_len(&self) -> Option<u64> {
                Some(self.0.get_ref().len() as u64)
            }
        }
        Box::new(Mem(io::Cursor::new(data)))
    }
}
