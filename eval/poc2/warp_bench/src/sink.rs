//! Audio output for the listening tests: Pulse (feature `pulse`, native rate, `BufferAttr` like the
//! product) or a WAV file (`--out file.wav`, also what the x86 smoke runs use).

use crate::gen::WavWriter;
use anyhow::{bail, Result};
use std::path::PathBuf;

pub trait Sink {
    /// Blocking write of interleaved f32 frames.
    fn write(&mut self, interleaved: &[f32]) -> Result<()>;
    /// Wait for the audio to be played out (drain) / close the file.
    fn finish(&mut self) -> Result<()>;
    /// Underruns detected so far (Pulse only; see [`UnderrunDetector`]).
    fn underruns(&self) -> u64;
}

#[derive(Debug, Clone)]
pub struct SinkOpts {
    pub tlength_ms: f64,
    pub minreq_ms: f64,
    pub out: Option<PathBuf>,
}

impl Default for SinkOpts {
    fn default() -> Self {
        // Same numbers as the shipped `PulseConfig` (README section 7).
        SinkOpts {
            tlength_ms: 80.0,
            minreq_ms: 20.0,
            out: None,
        }
    }
}

pub fn open_sink(opts: &SinkOpts, rate: u32, ch: usize) -> Result<Box<dyn Sink>> {
    if let Some(p) = &opts.out {
        return Ok(Box::new(WavSink {
            w: Some(WavWriter::create(p, ch as u16, rate)?),
        }));
    }
    #[cfg(feature = "pulse")]
    {
        Ok(Box::new(pulse::PulseSink::open(opts, rate, ch)?))
    }
    #[cfg(not(feature = "pulse"))]
    {
        let _ = (rate, ch);
        bail!("no audio output in this build: rebuild with `--features pulse` (cross-build: scripts/deploy.sh eval warp_bench) or render to a file with `--out file.wav`")
    }
}

struct WavSink {
    w: Option<WavWriter>,
}

impl Sink for WavSink {
    fn write(&mut self, interleaved: &[f32]) -> Result<()> {
        match self.w.as_mut() {
            Some(w) => w.write(interleaved),
            None => bail!("wav sink already finished"),
        }
    }
    fn finish(&mut self) -> Result<()> {
        if let Some(w) = self.w.take() {
            w.finish()?;
        }
        Ok(())
    }
    fn underruns(&self) -> u64 {
        0
    }
}

/// Heuristic underrun detector for the blocking Pulse "simple" API, which has no underflow callback.
///
/// While the producer keeps up, the blocking `write` pins the playback latency near `tlength`. An
/// underrun drains the buffer, so the latency collapses. The detector learns the steady latency from the
/// first samples (median, after a short settling period) and counts one underrun each time the latency
/// falls below `LOW x baseline`, re-arming once it is back above `HIGH x baseline`.
#[cfg(any(feature = "pulse", test))]
#[derive(Debug, Clone)]
pub struct UnderrunDetector {
    warmup: Vec<f64>,
    baseline: Option<f64>,
    in_underrun: bool,
    pub count: u64,
}

#[cfg(any(feature = "pulse", test))]
impl UnderrunDetector {
    pub const SETTLE_SAMPLES: usize = 20;
    pub const BASELINE_SAMPLES: usize = 40;
    pub const LOW: f64 = 0.35;
    pub const HIGH: f64 = 0.7;

    pub fn new() -> Self {
        UnderrunDetector {
            warmup: Vec::new(),
            baseline: None,
            in_underrun: false,
            count: 0,
        }
    }

    #[cfg(test)]
    pub fn baseline_ms(&self) -> Option<f64> {
        self.baseline
    }

    /// Feed one latency observation in ms. Returns `true` when it starts a new underrun.
    pub fn observe(&mut self, latency_ms: f64) -> bool {
        let Some(base) = self.baseline else {
            self.warmup.push(latency_ms);
            if self.warmup.len() >= Self::SETTLE_SAMPLES + Self::BASELINE_SAMPLES {
                self.baseline = Some(crate::stats::median(&self.warmup[Self::SETTLE_SAMPLES..]));
            }
            return false;
        };
        if self.in_underrun {
            if latency_ms > Self::HIGH * base {
                self.in_underrun = false;
            }
            false
        } else if latency_ms < Self::LOW * base {
            self.in_underrun = true;
            self.count += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(any(feature = "pulse", test))]
impl Default for UnderrunDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "pulse")]
pub mod pulse {
    use super::*;
    use libpulse_binding::def::BufferAttr;
    use libpulse_binding::sample::{Format, Spec};
    use libpulse_binding::stream::Direction;
    use libpulse_simple_binding::Simple;

    pub fn ms_to_bytes(ms: f64, rate: u32, ch: usize) -> u32 {
        ((ms / 1000.0 * rate as f64).round() as u32) * ch as u32 * 4
    }

    pub struct PulseSink {
        simple: Simple,
        bytes: Vec<u8>,
        det: UnderrunDetector,
        writes: u64,
    }

    impl PulseSink {
        pub fn open(opts: &SinkOpts, rate: u32, ch: usize) -> Result<Self> {
            let spec = Spec {
                format: Format::F32le,
                channels: ch as u8,
                rate,
            };
            if !spec.is_valid() {
                bail!("invalid Pulse sample spec: {ch} ch @ {rate} Hz");
            }
            let attr = BufferAttr {
                maxlength: u32::MAX,
                tlength: ms_to_bytes(opts.tlength_ms, rate, ch),
                prebuf: u32::MAX,
                minreq: ms_to_bytes(opts.minreq_ms, rate, ch),
                fragsize: u32::MAX,
            };
            let server = std::env::var("PULSE_SERVER").ok();
            let simple = Simple::new(
                server.as_deref(),
                "warp_bench",
                Direction::Playback,
                None,
                "warp_bench",
                &spec,
                None,
                Some(&attr),
            )
            .map_err(|e| anyhow::anyhow!("connect to PulseAudio/PipeWire-Pulse failed: {e} (is the audio server running? is PULSE_SERVER set as for the product?)"))?;
            Ok(PulseSink {
                simple,
                bytes: Vec::new(),
                det: UnderrunDetector::new(),
                writes: 0,
            })
        }

        /// Poll `get_latency` and feed the detector.
        pub fn poll_latency(&mut self) -> Option<f64> {
            let lat = self.simple.get_latency().ok()?;
            let ms = lat.0 as f64 / 1000.0;
            self.det.observe(ms);
            Some(ms)
        }
    }

    impl Sink for PulseSink {
        fn write(&mut self, interleaved: &[f32]) -> Result<()> {
            self.bytes.clear();
            for s in interleaved {
                self.bytes.extend_from_slice(&s.to_le_bytes());
            }
            self.simple
                .write(&self.bytes)
                .map_err(|e| anyhow::anyhow!("Pulse write failed (sink disconnected?): {e}"))?;
            self.writes += 1;
            if self.writes & 3 == 0 {
                self.poll_latency();
            }
            Ok(())
        }

        fn finish(&mut self) -> Result<()> {
            self.simple
                .drain()
                .map_err(|e| anyhow::anyhow!("Pulse drain failed: {e}"))
        }

        fn underruns(&self) -> u64 {
            self.det.count
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(d: &mut UnderrunDetector, vals: &[f64]) {
        for v in vals {
            d.observe(*v);
        }
    }

    #[test]
    fn detector_learns_baseline_then_counts_episodes() {
        let mut d = UnderrunDetector::new();
        let steady: Vec<f64> = (0..60)
            .map(|i| if i < 20 { 5.0 } else { 70.0 + (i % 3) as f64 })
            .collect();
        feed(&mut d, &steady);
        assert!((d.baseline_ms().unwrap() - 71.0).abs() < 1.5);
        assert_eq!(d.count, 0);
        // a dip counts once, however long it lasts
        feed(&mut d, &[60.0, 20.0, 10.0, 12.0, 30.0]);
        assert_eq!(d.count, 1);
        // still below the re-arm level: no new episode
        feed(&mut d, &[10.0, 40.0]);
        assert_eq!(d.count, 1);
        // recovery re-arms; a second dip counts
        feed(&mut d, &[70.0, 72.0, 15.0]);
        assert_eq!(d.count, 2);
    }

    #[test]
    fn detector_ignores_everything_during_warmup() {
        let mut d = UnderrunDetector::new();
        for _ in 0..30 {
            assert!(!d.observe(0.0));
        }
        assert_eq!(d.count, 0);
        assert!(d.baseline_ms().is_none());
    }

    #[test]
    fn wav_sink_writes_a_file() {
        let dir = std::env::temp_dir().join(format!("warp_bench_sink_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let opts = SinkOpts {
            out: Some(dir.join("o.wav")),
            ..Default::default()
        };
        let mut s = open_sink(&opts, 44100, 2).unwrap();
        s.write(&vec![0.25f32; 2048]).unwrap();
        s.finish().unwrap();
        assert_eq!(s.underruns(), 0);
        assert_eq!(
            std::fs::metadata(dir.join("o.wav")).unwrap().len(),
            44 + 2048 * 2
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(feature = "pulse"))]
    #[test]
    fn without_pulse_and_without_out_there_is_a_clear_error() {
        let e = open_sink(&SinkOpts::default(), 44100, 2)
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("--features pulse"), "{e}");
    }
}
