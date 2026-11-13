//! `seek` sub-command: forward and backward `Accurate` seek cost, with an instrumented `MediaSource`
//! that counts the bytes the demuxer really reads and the seeks it performs.

use crate::decode::{self, Opened};
use crate::stats;
use anyhow::{bail, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use symphonia::core::formats::{SeekMode, SeekTo};
use symphonia::core::io::MediaSource;
use symphonia::core::units::Time;

#[derive(Debug, Default)]
pub struct Counters {
    bytes: AtomicU64,
    reads: AtomicU64,
    seeks: AtomicU64,
}

impl Counters {
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
    pub fn seeks(&self) -> u64 {
        self.seeks.load(Ordering::Relaxed)
    }
    pub fn reset(&self) {
        self.bytes.store(0, Ordering::Relaxed);
        self.reads.store(0, Ordering::Relaxed);
        self.seeks.store(0, Ordering::Relaxed);
    }
}

/// `MediaSource` wrapper that counts bytes read, read calls and seek calls.
pub struct CountingSource<R: Read + Seek + Send + Sync> {
    inner: R,
    counters: Arc<Counters>,
    len: Option<u64>,
}

impl<R: Read + Seek + Send + Sync> CountingSource<R> {
    pub fn new(inner: R, counters: Arc<Counters>, len: Option<u64>) -> Self {
        CountingSource {
            inner,
            counters,
            len,
        }
    }
}

impl<R: Read + Seek + Send + Sync> Read for CountingSource<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.counters.bytes.fetch_add(n as u64, Ordering::Relaxed);
        self.counters.reads.fetch_add(1, Ordering::Relaxed);
        Ok(n)
    }
}

impl<R: Read + Seek + Send + Sync> Seek for CountingSource<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.counters.seeks.fetch_add(1, Ordering::Relaxed);
        self.inner.seek(pos)
    }
}

impl<R: Read + Seek + Send + Sync> MediaSource for CountingSource<R> {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Forward,
    Backward,
}

impl Dir {
    pub fn name(self) -> &'static str {
        match self {
            Dir::Forward => "fwd",
            Dir::Backward => "back",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SeekSample {
    pub wall_s: f64,
    pub bytes: u64,
    pub seeks: u64,
    /// `required_ts - actual_ts`: frames the decoder lands before the requested frame and must trim.
    pub landing_offset_frames: u64,
}

/// Seconds before/after the target where the reader is parked before the timed seek.
pub const PARK_DISTANCE_S: f64 = 5.0;

/// One timed `Accurate` seek to `target_s`, starting with the reader parked `park_s` away.
/// The timed section ends when the first decoded frames at or after the target are available.
pub fn seek_once(path: &Path, target_s: f64, dir: Dir) -> Result<SeekSample> {
    let mut o = decode::open(path)?;
    drop_file_cache(path);
    let park_s = match dir {
        Dir::Forward => (target_s - PARK_DISTANCE_S).max(0.0),
        Dir::Backward => target_s + PARK_DISTANCE_S,
    };
    // park (not timed): seek there and decode a few packets so demuxer state is "playing there"
    raw_seek(&mut o, park_s)?;
    let mut sb = None;
    for _ in 0..3 {
        if o.decode_next(&mut sb)?.is_none() {
            break;
        }
    }
    o.counters.reset();
    let t0 = Instant::now();
    let seeked = o.format.seek(
        SeekMode::Accurate,
        SeekTo::Time {
            time: Time::from(target_s),
            track_id: Some(o.track_id),
        },
    )?;
    o.decoder.reset();
    // decode until the frame at the target is available
    let required = seeked.required_ts;
    let mut ts_end = seeked.actual_ts;
    while ts_end < required.saturating_add(1) {
        let packet = match o.format.next_packet() {
            Ok(p) => p,
            Err(e) => bail!("seek: demux ended before the target ({e})"),
        };
        if packet.track_id() != o.track_id {
            continue;
        }
        let end = packet.ts().saturating_add(packet.dur());
        if o.decoder.decode(&packet).is_ok() {
            ts_end = end;
        }
    }
    let wall = t0.elapsed().as_secs_f64();
    Ok(SeekSample {
        wall_s: wall,
        bytes: o.counters.bytes(),
        seeks: o.counters.seeks(),
        landing_offset_frames: required.saturating_sub(seeked.actual_ts),
    })
}

fn raw_seek(o: &mut Opened, secs: f64) -> Result<()> {
    o.format.seek(
        SeekMode::Accurate,
        SeekTo::Time {
            time: Time::from(secs),
            track_id: Some(o.track_id),
        },
    )?;
    o.decoder.reset();
    Ok(())
}

/// Best effort: ask the kernel to drop the file's clean pages so the next read hits the device.
pub fn drop_file_cache(path: &Path) {
    use std::os::unix::io::AsRawFd;
    if let Ok(f) = std::fs::File::open(path) {
        unsafe {
            libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }
}

#[derive(Debug, Clone)]
pub struct SeekCase {
    pub pct: u32,
    pub dir: Dir,
    pub samples: Vec<SeekSample>,
}

impl SeekCase {
    pub fn median_s(&self) -> f64 {
        stats::median(&self.samples.iter().map(|s| s.wall_s).collect::<Vec<_>>())
    }
    pub fn max_s(&self) -> f64 {
        stats::max(&self.samples.iter().map(|s| s.wall_s).collect::<Vec<_>>())
    }
    pub fn median_bytes(&self) -> f64 {
        stats::median(
            &self
                .samples
                .iter()
                .map(|s| s.bytes as f64)
                .collect::<Vec<_>>(),
        )
    }
    pub fn max_seeks(&self) -> u64 {
        self.samples.iter().map(|s| s.seeks).max().unwrap_or(0)
    }
    pub fn max_landing_offset(&self) -> u64 {
        self.samples
            .iter()
            .map(|s| s.landing_offset_frames)
            .max()
            .unwrap_or(0)
    }
}

pub const SEEK_PCTS: [u32; 3] = [10, 50, 90];

/// Target time for a case, keeping the park position inside the file.
pub fn case_target_s(duration_s: f64, pct: u32, dir: Dir) -> f64 {
    let t = duration_s * pct as f64 / 100.0;
    match dir {
        Dir::Forward => t.max(PARK_DISTANCE_S.min(duration_s * 0.5)),
        Dir::Backward => t.min((duration_s - PARK_DISTANCE_S - 1.0).max(0.0)),
    }
}

/// All six cases (forward/backward at 10/50/90 %), `reps` timed seeks each.
pub fn run_cases(
    path: &Path,
    reps: usize,
    mut progress: impl FnMut(&str),
) -> Result<(f64, Vec<SeekCase>)> {
    let mut o = decode::open(path)?;
    let dur_s = o.duration_frames()? as f64 / o.rate as f64;
    drop(o);
    if dur_s < 20.0 {
        bail!(
            "{}: only {dur_s:.1} s long; the seek test needs a file of at least 20 s",
            path.display()
        );
    }
    let mut cases = Vec::new();
    for dir in [Dir::Forward, Dir::Backward] {
        for pct in SEEK_PCTS {
            let target = case_target_s(dur_s, pct, dir);
            progress(&format!(
                "  {} {}% (target {:.1} s) x{reps}",
                dir.name(),
                pct,
                target
            ));
            let mut samples = Vec::new();
            for _ in 0..reps {
                samples.push(seek_once(path, target, dir)?);
            }
            cases.push(SeekCase { pct, dir, samples });
        }
    }
    Ok((dur_s, cases))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen::{write_signal_wav, Kind};
    use std::io::Cursor;

    #[test]
    fn counting_source_counts_bytes_reads_and_seeks() {
        let counters = Arc::new(Counters::default());
        let data: Vec<u8> = (0..=255u8).cycle().take(10_000).collect();
        let mut s = CountingSource::new(Cursor::new(data), counters.clone(), Some(10_000));
        assert!(s.is_seekable());
        assert_eq!(s.byte_len(), Some(10_000));
        let mut buf = [0u8; 100];
        s.read_exact(&mut buf).unwrap();
        s.seek(SeekFrom::Start(5000)).unwrap();
        let n = s.read(&mut buf[..40]).unwrap();
        assert_eq!(n, 40);
        assert_eq!(counters.bytes(), 140);
        assert_eq!(counters.seeks(), 1);
        assert_eq!(counters.reads(), 2);
        assert_eq!(buf[0], (5000 % 256) as u8);
        counters.reset();
        assert_eq!(
            (counters.bytes(), counters.seeks(), counters.reads()),
            (0, 0, 0)
        );
    }

    #[test]
    fn a_wav_seek_reads_far_fewer_bytes_than_the_file_and_lands_within_a_packet() {
        let dir = std::env::temp_dir().join(format!("warp_bench_seek_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        write_signal_wav(&p, Kind::Tone, 60.0, 44100, 2).unwrap();
        let file_len = std::fs::metadata(&p).unwrap().len();
        for dir_ in [Dir::Forward, Dir::Backward] {
            let s = seek_once(&p, 30.0, dir_).unwrap();
            assert!(s.bytes > 0, "the seek must read something");
            assert!(s.bytes < file_len / 10, "{} of {file_len} bytes", s.bytes);
            // Symphonia lands on a packet boundary; the offset is the frames to trim (below one packet)
            assert!(
                s.landing_offset_frames < 8192,
                "landing offset {}",
                s.landing_offset_frames
            );
            assert!(s.wall_s >= 0.0);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_cases_covers_six_cases_and_rejects_short_files() {
        let dir = std::env::temp_dir().join(format!("warp_bench_seek6_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        write_signal_wav(&p, Kind::Tone, 30.0, 44100, 1).unwrap();
        let mut lines = 0;
        let (dur, cases) = run_cases(&p, 2, |_| lines += 1).unwrap();
        assert!((dur - 30.0).abs() < 0.1);
        assert_eq!(cases.len(), 6);
        assert_eq!(lines, 6);
        assert!(cases
            .iter()
            .all(|c| c.samples.len() == 2 && c.median_s() <= c.max_s()));
        assert_eq!(cases.iter().filter(|c| c.dir == Dir::Backward).count(), 3);
        let short = dir.join("s.wav");
        write_signal_wav(&short, Kind::Tone, 5.0, 44100, 1).unwrap();
        assert!(run_cases(&short, 1, |_| {}).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn case_targets_keep_the_park_position_inside_the_file() {
        for dur in [30.0, 180.0, 3600.0] {
            for pct in SEEK_PCTS {
                let f = case_target_s(dur, pct, Dir::Forward);
                assert!(f >= PARK_DISTANCE_S.min(dur * 0.5) && f <= dur);
                let b = case_target_s(dur, pct, Dir::Backward);
                assert!(b + PARK_DISTANCE_S < dur, "dur {dur} pct {pct}: {b}");
            }
        }
        assert_eq!(case_target_s(3600.0, 50, Dir::Backward), 1800.0);
    }
}
