//! Shared gated capture sink and rig for the audio-thread tests (WARP_04/05).
#![allow(dead_code)]

use plippa_audio::testsupport::{frame_index_sample, write_pcm16_wav};
use plippa_audio::{AudioError, AudioSink, OverspecPolicy};
use plippa_core::Capabilities;
use plippa_engine::{AudioCommand, AudioThreadConfig, AudioThreadHandle, PlayArm};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub const RATE: u32 = 48_000;
pub const BLOCK: usize = 1024;

pub struct GateSink {
    channels: u16,
    out: Arc<Mutex<Vec<f32>>>,
    /// Stop recording after this many samples (a long fixture is only compared as a prefix).
    record_cap: usize,
    pub writes: Arc<AtomicUsize>,
    pub allowed: Arc<AtomicUsize>,
    pub blocked: Arc<AtomicBool>,
    /// Output frames the sink reports as still queued (0 = plays instantly, the default).
    queue: Arc<AtomicU64>,
}

impl AudioSink for GateSink {
    fn channels(&self) -> u16 {
        self.channels
    }
    fn sample_rate_hz(&self) -> u32 {
        RATE
    }
    fn write(&mut self, interleaved: &[f32]) -> Result<(), AudioError> {
        let n = self.writes.load(Ordering::SeqCst);
        if n >= self.allowed.load(Ordering::SeqCst) {
            self.blocked.store(true, Ordering::SeqCst);
            while n >= self.allowed.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_micros(200));
            }
            self.blocked.store(false, Ordering::SeqCst);
        }
        let mut out = self.out.lock().unwrap();
        if out.len() < self.record_cap {
            out.extend_from_slice(interleaved);
        }
        drop(out);
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn queue_frames(&mut self) -> u64 {
        self.queue.load(Ordering::SeqCst)
    }
}

pub struct Rig {
    pub audio: AudioThreadHandle,
    pub out: Arc<Mutex<Vec<f32>>>,
    pub writes: Arc<AtomicUsize>,
    pub allowed: Arc<AtomicUsize>,
    pub blocked: Arc<AtomicBool>,
    /// What the sink reports as still queued (UI_19 tests).
    pub queue: Arc<AtomicU64>,
    pub seeks_sent: u64,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.allowed.store(usize::MAX, Ordering::SeqCst);
    }
}

pub fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(30), "timed out: {what}");
        thread::sleep(Duration::from_micros(300));
    }
}

impl Rig {
    pub fn new(channels: u16, record_cap: usize, cfg: AudioThreadConfig) -> Self {
        let out = Arc::new(Mutex::new(Vec::new()));
        let writes = Arc::new(AtomicUsize::new(0));
        let allowed = Arc::new(AtomicUsize::new(0));
        let blocked = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(AtomicU64::new(0));
        let (o, w, a, b, q) = (
            out.clone(),
            writes.clone(),
            allowed.clone(),
            blocked.clone(),
            queue.clone(),
        );
        let audio = AudioThreadHandle::spawn_with_config(
            move |ch: u16, _rate: u32| -> Result<Box<dyn AudioSink>, AudioError> {
                Ok(Box::new(GateSink {
                    channels: ch,
                    out: o.clone(),
                    record_cap,
                    writes: w.clone(),
                    allowed: a.clone(),
                    blocked: b.clone(),
                    queue: q.clone(),
                }))
            },
            BLOCK,
            cfg,
        );
        let _ = channels;
        audio.try_send(AudioCommand::SetVolume(1.0));
        Self {
            audio,
            out,
            writes,
            allowed,
            blocked,
            queue,
            seeks_sent: 0,
        }
    }

    pub fn start(&mut self, path: &Path, play: bool) {
        self.start_at(path, play, 0);
    }

    pub fn start_at(&mut self, path: &Path, play: bool, start_ms: u64) {
        let file = std::fs::File::open(path).expect("test fixture opens");
        self.audio.try_send(AudioCommand::StartMainStream {
            path: path.to_path_buf(),
            file,
            capabilities: Capabilities::desktop_mock(),
            overspec: OverspecPolicy::default(),
            start_ms,
            play: play.then_some(PlayArm { not_before: None }),
            item_position: 0,
        });
        wait_until("stream open", || {
            self.audio.stats.starts_completed.load(Ordering::SeqCst) >= 1
                && self.audio.stats.has_main_stream.load(Ordering::SeqCst)
        });
    }

    /// Set the sink queue the audio thread reports (output frames still to be heard).
    pub fn set_sink_queue(&self, frames: u64) {
        self.queue.store(frames, Ordering::SeqCst);
    }

    /// Let the audio thread run `n` more writes and park in the one after.
    pub fn run_writes(&self, n: usize) {
        let target = self.writes.load(Ordering::SeqCst) + n;
        self.allowed.store(target, Ordering::SeqCst);
        wait_until("parked", || {
            self.writes.load(Ordering::SeqCst) == target && self.blocked.load(Ordering::SeqCst)
        });
    }

    /// Seek while the audio thread is parked in a write: the seek is handled right after the
    /// released write, then the thread parks in the next one.
    pub fn seek_parked(&mut self, ms: u64) {
        let target = self.writes.load(Ordering::SeqCst) + 1;
        self.audio.try_send(AudioCommand::SeekMs(ms));
        self.seeks_sent += 1;
        self.allowed.store(target, Ordering::SeqCst);
        let want = self.seeks_sent;
        wait_until("seek handled and parked", || {
            self.audio.stats.seeks_completed.load(Ordering::SeqCst) >= want
                && self.writes.load(Ordering::SeqCst) == target
                && self.blocked.load(Ordering::SeqCst)
        });
    }

    pub fn frames(&self) -> u64 {
        self.audio.stats.position_src_frames.load(Ordering::SeqCst)
    }
}

pub fn index_wav(path: &Path, frames: usize) {
    write_pcm16_wav(path, RATE, 1, frames, |f, _| frame_index_sample(f)).unwrap();
}

/// 1013 Hz sine file (period not a whole number of frames, so a seek is a phase jump).
pub fn sine_wav(path: &Path, frames: usize, amp: f64) {
    write_pcm16_wav(path, RATE, 1, frames, |f, _| {
        let v = amp * (2.0 * std::f64::consts::PI * 1013.0 * f as f64 / f64::from(RATE)).sin();
        (v * 32_767.0).round() as i16
    })
    .unwrap();
}
