//! `latency` sub-command: `Simple::get_latency` and underruns at the real sink for several
//! `tlength` values (`minreq = tlength / 4`). Needs the `pulse` feature.

use crate::stats;
use anyhow::Result;

/// `tlength` values tested (ms).
pub const TLENGTHS_MS: [f64; 5] = [60.0, 80.0, 120.0, 160.0, 240.0];
/// Seconds of audio per tlength.
pub const RUN_S: f64 = 10.0;
/// Latency observations in the first second are ignored (stream start-up).
#[cfg_attr(not(feature = "pulse"), allow(dead_code))]
pub const WARMUP_S: f64 = 1.0;

#[derive(Debug, Clone)]
pub struct LatencyResult {
    pub min_ms: f64,
    pub avg_ms: f64,
    pub max_ms: f64,
    pub underruns: u64,
    pub samples: usize,
}

/// `(min, avg, max)` of the observations.
#[cfg_attr(not(feature = "pulse"), allow(dead_code))]
pub fn summarize(samples: &[f64]) -> (f64, f64, f64) {
    (
        stats::min(samples),
        stats::mean(samples),
        stats::max(samples),
    )
}

/// Run one tlength. `load_threads` busy threads simulate other load on the board during the run.
#[cfg(feature = "pulse")]
pub fn run(tlength_ms: f64, rate: u32, load_threads: usize) -> Result<LatencyResult> {
    use crate::gen::{generate, Kind};
    use crate::sink::{pulse::PulseSink, Sink, SinkOpts};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for _ in 0..load_threads {
        let stop = stop.clone();
        handles.push(std::thread::spawn(move || {
            let mut x = 1.0f64;
            while !stop.load(Ordering::Relaxed) {
                for _ in 0..10_000 {
                    x = (x * 1.000_001).sin().abs() + 1.0;
                }
            }
            x
        }));
    }
    let opts = SinkOpts {
        tlength_ms,
        minreq_ms: tlength_ms / 4.0,
        out: None,
    };
    let result = (|| -> Result<LatencyResult> {
        let ch = 2usize;
        let mut sink = PulseSink::open(&opts, rate, ch)?;
        let tone = generate(Kind::Tone, 1.0, rate);
        let tone_len = tone.len();
        let total_frames = (RUN_S * rate as f64) as usize;
        let block = 256usize;
        let mut obs = Vec::new();
        let mut done = 0usize;
        let mut buf = vec![0.0f32; block * ch];
        while done < total_frames {
            let t = done as f64 / rate as f64;
            let audible = (1.0..RUN_S - 1.0).contains(&t);
            for i in 0..block {
                let s = if audible {
                    0.12 * tone[(done + i) % tone_len]
                } else {
                    0.0
                };
                for c in 0..ch {
                    buf[i * ch + c] = s;
                }
            }
            sink.write(&buf)?;
            if let Some(ms) = sink.poll_latency() {
                if t >= WARMUP_S {
                    obs.push(ms);
                }
            }
            done += block;
        }
        sink.finish()?;
        let (min_ms, avg_ms, max_ms) = summarize(&obs);
        Ok(LatencyResult {
            min_ms,
            avg_ms,
            max_ms,
            underruns: sink.underruns(),
            samples: obs.len(),
        })
    })();
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }
    result
}

#[cfg(not(feature = "pulse"))]
pub fn run(_tlength_ms: f64, _rate: u32, _load_threads: usize) -> Result<LatencyResult> {
    anyhow::bail!("the `latency` sub-command needs the pulse build: rebuild with `--features pulse` (scripts/deploy.sh eval warp_bench does this)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_and_constants() {
        assert_eq!(summarize(&[60.0, 70.0, 80.0]), (60.0, 70.0, 80.0));
        assert_eq!(TLENGTHS_MS, [60.0, 80.0, 120.0, 160.0, 240.0]);
    }

    #[cfg(not(feature = "pulse"))]
    #[test]
    fn without_pulse_the_error_names_the_feature() {
        let e = run(80.0, 48000, 0).unwrap_err().to_string();
        assert!(e.contains("--features pulse"));
    }
}
