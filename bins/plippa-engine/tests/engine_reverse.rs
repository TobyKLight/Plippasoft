//! WARP_11 engine integration: long reverse through back-windows, the bounce at 0:00, window
//! cap during reverse/seek storms, and the audio loop holding (never glitching) while a slow
//! decoder catches up. Logic and sample correctness only: no wall-clock assertions (pacing is
//! by write counts and by a source gate).

mod common;

use common::*;
use plippa_audio::testsupport::GatedBlocks;
use plippa_audio::BlockSource;
use plippa_audio::testsupport::{click_metric, smooth_bound};
use plippa_engine::{AudioCommand, AudioThreadConfig, DecoderHook, RateChange};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const SINE_HZ: f64 = 1013.0;

/// The sample `sine_wav` writes for frame `f` (pcm16 quantised, as decoded).
fn sine_sample(f: u64, amp: f64) -> f32 {
    let v = amp * (2.0 * std::f64::consts::PI * SINE_HZ * f as f64 / f64::from(RATE)).sin();
    (v * 32_767.0).round() as i16 as f32 / 32_768.0
}

/// First frame of real audio after the hold at the start: reverse right after a start has
/// nothing behind the start window, so the loop fades out its first quantum, holds (silence,
/// for as long as the worker takes) and fades back in when the first back-window lands.
/// Returns the first non-silent frame after a silent run of >= 64 frames that begins within
/// the first 512 frames; without such a hold, the first non-silent frame.
fn audio_start(out: &[f32], ch: usize) -> usize {
    let frames = out.len() / ch;
    let silent = |k: usize| out[k * ch..(k + 1) * ch].iter().all(|v| *v == 0.0);
    let first = (0..frames).find(|k| !silent(*k)).expect("audio");
    // Start of a silent run within the first 512 frames (after the faded quantum).
    let run_start = (first..frames.min(512))
        .find(|k| silent(*k) && (*k..*k + 64).all(|j| j < frames && silent(j)));
    match run_start {
        Some(r) => (r..frames)
            .find(|k| !silent(*k))
            .expect("audio after the hold"),
        None => first,
    }
}

fn set_rate(rig: &Rig, permille: i32) {
    rig.audio.try_send(AudioCommand::SetRate {
        permille,
        change: RateChange::Immediate,
    });
}

fn play(rig: &Rig) {
    rig.audio.try_send(AudioCommand::Play { not_before: None });
}

fn worker_stat<T>(rig: &Rig, f: impl Fn(&plippa_audio::WorkerStats) -> T) -> T {
    let g = rig.audio.stats.worker.lock().unwrap();
    f(g.as_ref().expect("worker stats"))
}

#[test]
fn reverse_minus_one_over_60_s_equals_the_reversed_source_and_has_no_click() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("sine.wav");
    let total = 100 * RATE as usize;
    sine_wav(&wav, total, 0.5);
    let mut rig = Rig::new(1, usize::MAX, AudioThreadConfig::default());
    let start_ms = 80_000u64;
    rig.start_at(&wav, false, start_ms);
    set_rate(&rig, -1000);
    play(&rig);
    // 60 s of reverse = 60 * 48000 frames at 256 frames per write. Small batches pace the
    // audio thread by write counts so the worker is never asked to outrun a whole back-lead
    // (4 s) between two looks at it.
    let writes = 60 * RATE as usize / 256;
    for _ in 0..writes.div_ceil(20) {
        rig.run_writes(20);
    }
    let out = rig.out.lock().unwrap().clone();
    assert!(out.len() >= 59 * RATE as usize);
    // Reverse straight after the start has nothing behind the start window yet: the loop
    // holds (silence) until the first back-window arrives, then resumes (fade in over one
    // quantum). Find where audio starts and which source frame it continues from.
    let k0 = audio_start(&out, 1);
    let k1 = k0 + 512;
    let f0 = start_ms * 48;
    let p = (f0.saturating_sub(4_000)..=f0)
        .find(|p| {
            (0..64).all(|j| {
                out[k1 + j] == sine_sample(p - (k1 - k0) as u64 - j as u64 + (k1 - k0) as u64, 0.5)
            })
        })
        .expect("the audio after the hold is the reversed source");
    let mut worst = 0.0f32;
    for (j, v) in out[k1..].iter().enumerate() {
        worst = worst.max((v - sine_sample(p - j as u64, 0.5)).abs());
    }
    assert!(
        worst < 1e-5,
        "output differs from the reversed source by {worst}"
    );
    let out = out[k0..].to_vec();
    let bound = smooth_bound(0.5, SINE_HZ, RATE);
    let m = click_metric(&out, bound);
    assert!(m < 1.5, "click metric through the back-window joins {m}");
    // Many back-windows were used, none was refused or stale-reordered, nothing blocked.
    assert!(worker_stat(&rig, |s| s.back_windows.load(Ordering::SeqCst)) >= 20);
    assert_eq!(
        worker_stat(&rig, |s| s.blocks_refused.load(Ordering::SeqCst)),
        0
    );
    assert_eq!(
        worker_stat(&rig, |s| s.audio_blocking_waits.load(Ordering::SeqCst)),
        0
    );
    assert_eq!(rig.audio.stats.rate_bounces.load(Ordering::SeqCst), 0);
    let peak = rig.audio.stats.peak_window_bytes.load(Ordering::SeqCst);
    assert!(peak > 0 && peak <= 6 * 1024 * 1024, "peak {peak}");
}

#[test]
fn reverse_from_near_the_end_and_bounce_forward_at_zero() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    let total = 20 * RATE as usize;
    index_wav(&wav, total);
    let mut rig = Rig::new(1, usize::MAX, AudioThreadConfig::default());
    // Near the end of the file, reversing at -2.0.
    rig.start_at(&wav, false, 19_000);
    set_rate(&rig, -2000);
    play(&rig);
    let mut guard = 0;
    while rig.audio.stats.rate_bounces.load(Ordering::SeqCst) == 0 {
        rig.run_writes(200);
        guard += 1;
        assert!(guard < 400, "never reached frame 0");
    }
    // Forward again at the set magnitude: the playhead now increases.
    rig.run_writes(50);
    let a = rig.frames();
    rig.run_writes(200);
    let b = rig.frames();
    assert!(b > a, "moves forward after the bounce ({a} -> {b})");
    assert_eq!(rig.audio.stats.rate_bounces.load(Ordering::SeqCst), 1);
    assert_eq!(rig.audio.stats.rate_micro.load(Ordering::SeqCst), 2_000_000);
    // Everything captured is finite and within range (no glitch values at the turn).
    let out = rig.out.lock().unwrap();
    assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
}

#[test]
fn reverse_and_seek_storm_never_exceeds_the_window_cap() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("long.wav");
    let total = 20 * 60 * RATE as usize;
    index_wav(&wav, total);
    let mut rig = Rig::new(1, usize::MAX, AudioThreadConfig::default());
    rig.start_at(&wav, false, 900_000);
    set_rate(&rig, -2000);
    play(&rig);
    let mut rng = plippa_audio::testsupport::Lcg::new(5);
    for k in 0..300u32 {
        rig.run_writes(rng.below(60) as usize + 1);
        if k % 3 == 0 {
            rig.seek_parked(rng.below(1_190_000));
        }
        let peak = rig.audio.stats.peak_window_bytes.load(Ordering::SeqCst);
        assert!(peak <= 6 * 1024 * 1024, "window over the cap: {peak}");
    }
    assert_eq!(
        worker_stat(&rig, |s| s.audio_blocking_waits.load(Ordering::SeqCst)),
        0
    );
    let peak = rig.audio.stats.peak_window_bytes.load(Ordering::SeqCst);
    assert!(
        peak > 0 && peak < (total * 4) as u64 / 10,
        "never holds a whole track: {peak}"
    );
}

fn gated_hook(gate: Arc<AtomicBool>) -> DecoderHook {
    DecoderHook(Arc::new(move |_path: &Path, d: Box<dyn BlockSource>| -> Box<dyn BlockSource> {
        Box::new(GatedBlocks::new(d, gate.clone()))
    }))
}

#[test]
fn a_slow_decoder_makes_the_reverse_hold_smoothly_and_resume_without_a_click() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("sine.wav");
    sine_wav(&wav, 120 * RATE as usize, 0.5);
    let gate = Arc::new(AtomicBool::new(true));
    let cfg = AudioThreadConfig {
        decoder_hook: Some(gated_hook(gate.clone())),
        ..AudioThreadConfig::default()
    };
    let mut rig = Rig::new(1, usize::MAX, cfg);
    rig.start_at(&wav, false, 100_000);
    set_rate(&rig, -2000);
    play(&rig);
    rig.run_writes(100);
    // The decoder stalls (a slow USB stick): reverse eats the decoded history and then holds.
    gate.store(false, Ordering::SeqCst);
    // At -2.0 a 256-frame quantum eats 512 source frames: 8 s of history and the back lead
    // are gone well before 1500 writes.
    for _ in 0..3 {
        rig.run_writes(500);
    }
    let held_pos = rig.frames();
    let out_len = rig.out.lock().unwrap().len();
    rig.run_writes(50);
    assert_eq!(
        rig.frames(),
        held_pos,
        "position holds while the worker is behind"
    );
    {
        let out = rig.out.lock().unwrap();
        assert!(
            out[out_len..].iter().all(|v| *v == 0.0),
            "silence while held"
        );
    }
    // The decoder catches up: the reverse picks up where it held.
    gate.store(true, Ordering::SeqCst);
    wait_until("reverse resumes", || {
        rig.run_writes(50);
        rig.frames() < held_pos
    });
    rig.run_writes(100);
    let out = rig.out.lock().unwrap().clone();
    // At -2.0 the 1013 Hz sine is heard at 2026 Hz.
    let bound = smooth_bound(0.5, SINE_HZ * 2.0, RATE);
    let m = click_metric(&out, bound);
    assert!(
        m < 1.5,
        "click metric through fade-out, hold and fade-in {m}"
    );
    assert_eq!(
        worker_stat(&rig, |s| s.audio_blocking_waits.load(Ordering::SeqCst)),
        0
    );
    assert_eq!(
        worker_stat(&rig, |s| s.blocks_refused.load(Ordering::SeqCst)),
        0
    );
}

// ---- real fixtures: reverse beyond the 8 s history on WAV / FLAC / MP3 -------------------

fn media(rel: &str) -> std::path::PathBuf {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/testMedia");
    assert!(root.is_dir(), "test media missing: run scripts/fetch-testmedia.sh");
    root.join(rel)
}

/// Plain sequential decode of `[from, from + frames)` (interleaved).
fn decode_region(path: &Path, from: u64, frames: usize) -> (Vec<f32>, usize, u32) {
    let mut d = plippa_audio::StreamDecoder::open(
        path,
        &plippa_core::Capabilities::desktop_mock(),
        plippa_audio::OverspecPolicy::default(),
    )
    .unwrap();
    let ch = d.channels() as usize;
    let rate = d.sample_rate_hz();
    d.seek_frame(from).unwrap();
    let mut out = Vec::with_capacity(frames * ch);
    while out.len() < frames * ch {
        let c = d.next_chunk(4096).unwrap().expect("file long enough");
        out.extend_from_slice(&c.samples);
    }
    out.truncate(frames * ch);
    (out, ch, rate)
}

/// Reverse `path` from `start_ms` at `rate_permille` until the playhead passed `travel_s`
/// seconds backward (beyond the 8 s history); returns the captured output, channels, and the
/// frames travelled.
fn reverse_run(
    path: &Path,
    start_ms: u64,
    rate_permille: i32,
    travel_s: f64,
) -> (Vec<f32>, usize, u32) {
    let mut rig = Rig::new(2, usize::MAX, AudioThreadConfig::default());
    rig.start_at(path, false, start_ms);
    set_rate(&rig, rate_permille);
    play(&rig);
    let rate_hz = rig.audio.stats.stream_sample_rate_hz.load(Ordering::SeqCst);
    let start_frame = start_ms * u64::from(rate_hz) / 1000;
    let goal = start_frame - (travel_s * f64::from(rate_hz)) as u64;
    let mut guard = 0;
    loop {
        rig.run_writes(20);
        let pos = rig.frames();
        if pos <= goal && pos > 0 {
            break;
        }
        guard += 1;
        assert!(guard < 200_000, "{}: stuck at frame {pos}", path.display());
    }
    assert_eq!(rig.audio.stats.rate_bounces.load(Ordering::SeqCst), 0);
    assert!(rig.audio.stats.peak_window_bytes.load(Ordering::SeqCst) <= 6 * 1024 * 1024);
    let ch = {
        let out = rig.out.lock().unwrap();
        assert!(out.iter().all(|v| v.is_finite()));
        assert!(
            out.iter().any(|v| v.abs() > 1e-3),
            "{} is not silent",
            path.display()
        );
        2
    };
    assert!(worker_stat(&rig, |s| s.back_windows.load(Ordering::SeqCst)) >= 2);
    assert_eq!(
        worker_stat(&rig, |s| s.blocks_refused.load(Ordering::SeqCst)),
        0
    );
    let out = rig.out.lock().unwrap().clone();
    (out, ch, rate_hz)
}

const FIXTURES: [&str; 4] = [
    "WAV/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.wav",
    "FLAC/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.flac",
    "MP3 V0/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3",
    "MP3 320/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3",
];

#[test]
fn reverse_beyond_the_history_works_on_wav_flac_and_mp3_at_several_rates() {
    for rel in FIXTURES {
        let path = media(rel);
        // -0.1 is the slowest legal reverse: one fixture only, a short travel past the
        // 8 s history keeps the test quick.
        let rates: &[i32] = if rel.starts_with("FLAC") {
            &[-2000, -1000, -250, -100]
        } else {
            &[-2000, -1000, -250]
        };
        for &rate in rates {
            let travel = if rate == -100 { 8.5 } else { 9.0 };
            let _ = reverse_run(&path, 40_000, rate, travel);
        }
    }
}

#[test]
fn reverse_at_minus_one_equals_the_reversed_decode_on_flac_and_mp3() {
    for rel in &FIXTURES[1..] {
        let path = media(rel);
        let (out, ch, rate_hz) = reverse_run(&path, 40_000, -1000, 12.0);
        let start_frame = 40_000 * u64::from(rate_hz) / 1000;
        let span = 14 * rate_hz as usize;
        let from = start_frame - span as u64 + 2048;
        let (reference, _, _) = decode_region(&path, from, span);
        let ref_frame =
            |p: u64| &reference[(p - from) as usize * ch..(p - from) as usize * ch + ch];
        // Skip the hold at the start and the fade-in quantum.
        let k0 = audio_start(&out, ch);
        let k1 = k0 + 512;
        let n = (out.len() / ch).min(10 * rate_hz as usize) - k1;
        let near = |a: &[f32], b: &[f32]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 2e-4);
        let p = (start_frame - 5 * u64::from(rate_hz)..=start_frame)
            .find(|p| {
                (0..32).all(|j| {
                    near(
                        &out[(k1 + j) * ch..(k1 + j + 1) * ch],
                        ref_frame(p - j as u64),
                    )
                })
            })
            .unwrap_or_else(|| panic!("{rel}: reverse output does not match the reversed decode"));
        let mut worst = 0.0f32;
        for j in 0..n {
            let a = &out[(k1 + j) * ch..(k1 + j + 1) * ch];
            let b = ref_frame(p - j as u64);
            for c in 0..ch {
                worst = worst.max((a[c] - b[c]).abs());
            }
        }
        assert!(
            worst < 2e-4,
            "{rel}: worst difference to the reversed decode {worst}"
        );
    }
}

#[test]
fn the_mp3_index_is_built_after_load_and_serves_backward_seeks() {
    let path = media("MP3 V0/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3");
    let mut rig = Rig::new(2, 0, AudioThreadConfig::default());
    rig.start_at(&path, true, 40_000);
    rig.run_writes(10);
    wait_until("index ready", || {
        worker_stat(&rig, |s| s.index_ready.load(Ordering::SeqCst))
    });
    assert!(worker_stat(&rig, |s| s.index_entries.load(Ordering::SeqCst)) > 10);
    // A backward seek far outside the window goes through the worker's indexed seek.
    let before = worker_stat(&rig, |s| s.indexed_seeks.load(Ordering::SeqCst));
    rig.seek_parked(12_000);
    assert_eq!(rig.frames(), 12_000 * 44_100 / 1000);
    assert!(worker_stat(&rig, |s| s.indexed_seeks.load(Ordering::SeqCst)) > before);
}

#[test]
fn a_hint_extends_the_window_so_the_landing_is_served_without_a_decoder_seek() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    index_wav(&wav, 120 * RATE as usize);
    let mut rig = Rig::new(1, 0, AudioThreadConfig::default());
    rig.start_at(&wav, true, 60_000);
    rig.run_writes(30);
    // 4 s behind the window start: outside the history (there is none: we started here).
    let target_ms = 56_000u64;
    let target = target_ms * 48;
    rig.audio
        .try_send(AudioCommand::Hint(Some(plippa_audio::Hint {
            target_frame: target,
            direction: -1,
            waypoint_lead_frames: 0,
        })));
    wait_until("landing zone held", || {
        rig.run_writes(1);
        worker_stat(&rig, |s| s.hint_ready_target.load(Ordering::SeqCst)) == target as i64
    });
    // `hint_ready_target` is reported for the planned window (queued blocks included): let the
    // audio thread apply what is in flight before the seek (pump runs once per write).
    rig.run_writes(4);
    let seeks = rig.audio.stats.decoder_seeks.load(Ordering::SeqCst);
    rig.seek_parked(target_ms);
    assert_eq!(
        rig.audio.stats.decoder_seeks.load(Ordering::SeqCst),
        seeks,
        "served from the window"
    );
    assert_eq!(rig.frames(), target);
}
