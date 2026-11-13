//! `usbread` sub-command: sequential read speed and random 4 KiB read latency of the device that
//! holds a (large) media file, plus the two derived estimates the mapping table uses.

use crate::gen::Lcg;
use crate::seek::drop_file_cache;
use crate::stats;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::Instant;

pub const SEQ_CHUNK: usize = 1 << 20;
pub const RANDOM_BLOCK: usize = 4096;
/// Index build time = whole-file read time x this (parsing overhead).
pub const INDEX_BUILD_FACTOR: f64 = 1.5;
/// Frames decoded after the random read in the indexed-seek estimate.
pub const INDEXED_SEEK_DECODE_FRAMES: f64 = 3.0;

#[derive(Debug, Clone)]
pub struct UsbRead {
    pub size_bytes: u64,
    pub seq_bytes: u64,
    pub seq_mb_s: f64,
    pub rand_median_ms: f64,
    pub rand_p95_ms: f64,
    pub rand_max_ms: f64,
    pub reads: usize,
}

/// Estimated time to build the MP3 index: `size / (MB/s) x 1.5`.
pub fn index_build_estimate_s(size_bytes: u64, seq_mb_s: f64) -> f64 {
    (size_bytes as f64 / 1e6) / seq_mb_s.max(1e-9) * INDEX_BUILD_FACTOR
}

/// Estimated indexed-seek cost: p95 random read + decode of 3 frames.
pub fn indexed_seek_estimate_s(rand_p95_ms: f64, decode_s_per_frame: f64) -> f64 {
    rand_p95_ms / 1000.0 + INDEXED_SEEK_DECODE_FRAMES * decode_s_per_frame
}

/// Measure. `seq_mb` caps how much of the file the sequential pass reads (default 512 MB).
pub fn measure(path: &Path, seq_mb: f64, random_reads: usize) -> Result<UsbRead> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let size = f.metadata()?.len();
    if size < 8 * RANDOM_BLOCK as u64 {
        bail!("{}: file too small for a read test", path.display());
    }
    drop_file_cache(path);
    // sequential
    let limit = ((seq_mb * 1e6) as u64).min(size);
    let mut buf = vec![0u8; SEQ_CHUNK];
    let mut total = 0u64;
    let t0 = Instant::now();
    while total < limit {
        let want = SEQ_CHUNK.min((limit - total) as usize);
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    let seq_s = t0.elapsed().as_secs_f64().max(1e-9);
    // random
    drop_file_cache(path);
    let mut rng = Lcg::new(0xC0FF_EE11);
    let blocks = size / RANDOM_BLOCK as u64;
    let mut lat_ms = Vec::with_capacity(random_reads);
    let mut b = [0u8; RANDOM_BLOCK];
    for _ in 0..random_reads {
        let off = (rng.next_u64() % blocks) * RANDOM_BLOCK as u64;
        let t = Instant::now();
        f.read_exact_at(&mut b, off)?;
        lat_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    Ok(UsbRead {
        size_bytes: size,
        seq_bytes: total,
        seq_mb_s: total as f64 / 1e6 / seq_s,
        rand_median_ms: stats::median(&lat_ms),
        rand_p95_ms: stats::percentile(&lat_ms, 95.0),
        rand_max_ms: stats::max(&lat_ms),
        reads: lat_ms.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_estimates_follow_the_documented_formulas() {
        // 1 GB at 100 MB/s = 10 s x 1.5
        assert!((index_build_estimate_s(1_000_000_000, 100.0) - 15.0).abs() < 1e-9);
        assert!((index_build_estimate_s(500_000_000, 20.0) - 37.5).abs() < 1e-9);
        // p95 40 ms + 3 frames x 1 ms
        assert!((indexed_seek_estimate_s(40.0, 0.001) - 0.043).abs() < 1e-12);
    }

    #[test]
    fn measure_reports_consistent_fields_on_a_temp_file() {
        // numbers are meaningless on the cloud box: only structure and bounds are checked
        let dir = std::env::temp_dir().join(format!("warp_bench_usb_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("blob.bin");
        std::fs::write(&p, vec![7u8; 3 * 1024 * 1024 + 123]).unwrap();
        let r = measure(&p, 2.0, 50).unwrap();
        assert_eq!(r.size_bytes, 3 * 1024 * 1024 + 123);
        assert_eq!(r.seq_bytes, 2_000_000);
        assert_eq!(r.reads, 50);
        assert!(r.seq_mb_s > 0.0 && r.rand_median_ms >= 0.0);
        assert!(r.rand_median_ms <= r.rand_p95_ms && r.rand_p95_ms <= r.rand_max_ms);
        let tiny = dir.join("tiny.bin");
        std::fs::write(&tiny, [0u8; 100]).unwrap();
        assert!(measure(&tiny, 1.0, 5).is_err());
        assert!(measure(&dir.join("missing"), 1.0, 5).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
