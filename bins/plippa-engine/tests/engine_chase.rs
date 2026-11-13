//! WARP_14: the commit chase through the audio thread. Logic only: the gated sink parks the
//! audio thread between writes, so nothing depends on time. The frame-index file (sample =
//! frame mod 65536, so keep it under 65536 frames) names the source frame of every output
//! sample, which makes "lands on the target frame exactly" checkable.

mod common;
use common::*;
use plippa_audio::testsupport::frame_index_from_f32;
use plippa_audio::ChaseConfig;
use plippa_core::WarpMode;
use plippa_audio::testsupport::GatedBlocks;
use plippa_audio::BlockSource;
use plippa_engine::{AudioCommand, AudioThreadConfig, DecoderHook, RateChange};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const FILE_FRAMES: usize = 60_000;

fn chase(rig: &Rig, target_ms: u64, mode: WarpMode) {
    rig.audio
        .try_send(AudioCommand::SeekChase { target_ms, mode });
}

fn active(rig: &Rig) -> bool {
    rig.audio.stats.chase_active.load(Ordering::SeqCst)
}

/// Run writes one at a time until the chase has landed; returns the peak |rate| seen.
fn run_until_landed(rig: &Rig) -> f64 {
    let mut peak: f64 = 0.0;
    for _ in 0..3_000 {
        rig.run_writes(1);
        peak = peak.max(f64::from(rig.audio.stats.rate_micro.load(Ordering::SeqCst)).abs() / 1e6);
        if !active(rig) {
            return peak;
        }
    }
    panic!("the chase never landed");
}

fn rig_playing(cfg: AudioThreadConfig, speed_permille: i32) -> (Rig, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    index_wav(&wav, FILE_FRAMES);
    let mut rig = Rig::new(1, usize::MAX, cfg);
    // The swoop tests exercise the varispeed seek voice against a varispeed speed voice.
    rig.audio
        .try_send(AudioCommand::SetSpeedWarpMode(WarpMode::PitchShift));
    if speed_permille != 1000 {
        rig.audio.try_send(AudioCommand::SetRate {
            permille: speed_permille,
            change: RateChange::Immediate,
        });
    }
    rig.start(&wav, true);
    rig.run_writes(5);
    (rig, dir)
}

/// The audio thread is parked in the write of the landing block; letting it finish publishes
/// the written playhead (the sink queue is 0), which must be the target frame exactly.
fn landed_frame(rig: &Rig) -> u64 {
    rig.run_writes(1);
    rig.frames()
}

#[test]
fn forward_swoop_lands_on_the_target_frame_and_stays_below_the_cap() {
    let (rig, _d) = rig_playing(Default::default(), 1000);
    let cfg = ChaseConfig::default();
    chase(&rig, 1000, WarpMode::PitchShift);
    // Target and actual are separate: right after the command the playhead is still far away.
    rig.run_writes(1);
    assert!(active(&rig));
    assert!(
        rig.frames() < 48_000 - 10_000,
        "actual {} != target",
        rig.frames()
    );
    let peak = run_until_landed(&rig);
    assert!(peak <= cfg.r_max_vari + 1e-6, "peak {peak}");
    assert!(peak > 1.5, "a 0.75 s jump must accelerate, peak {peak}");
    assert_eq!(landed_frame(&rig), 48_000);
    assert_eq!(rig.audio.stats.chases_started.load(Ordering::SeqCst), 1);
    assert_eq!(rig.audio.stats.waypoint_jumps.load(Ordering::SeqCst), 0);
}

#[test]
fn backward_swoop_and_retarget_land_on_the_last_target() {
    let (rig, _d) = rig_playing(Default::default(), 1000);
    chase(&rig, 1000, WarpMode::PitchShift);
    rig.run_writes(6);
    assert!(active(&rig));
    // A newer target (behind) replaces the first: one chase, one landing.
    chase(&rig, 200, WarpMode::PitchShift);
    let peak = run_until_landed(&rig);
    assert!(peak <= 4.0 + 1e-6);
    assert_eq!(landed_frame(&rig), 9_600);
    assert_eq!(rig.audio.stats.chases_started.load(Ordering::SeqCst), 1);
}

#[test]
fn chase_with_a_fast_set_speed_lands_exactly_and_keeps_the_set_speed() {
    let (rig, _d) = rig_playing(Default::default(), 1500);
    chase(&rig, 500, WarpMode::PitchShift);
    run_until_landed(&rig);
    assert_eq!(landed_frame(&rig), 24_000);
    // The hand-over ramps back to the set speed (it was never edited).
    rig.run_writes(60);
    let r = rig.audio.stats.rate_micro.load(Ordering::SeqCst);
    assert!((r - 1_500_000).abs() < 100_000, "rate {r}");
}

#[test]
fn far_target_jumps_to_the_waypoint_first() {
    let cfg = AudioThreadConfig {
        chase: ChaseConfig {
            chase_max_time_s: 0.05,
            waypoint_lead_s: 0.3,
            // A slow swoop: the waypoint window has time to arrive before the audio does.
            r_max_vari: 1.0,
            ..ChaseConfig::default()
        },
        ..Default::default()
    };
    let (rig, _d) = rig_playing(cfg, 1000);
    chase(&rig, 1000, WarpMode::PitchShift);
    // The jump happens as soon as the waypoint is available (held already, or the worker's
    // waypoint window adopted): the swoop keeps going from the audio meanwhile. The worker is
    // given the time it needs before the audio runs on (no race with the swoop arriving).
    rig.run_writes(1);
    wait_until("waypoint prepared or held", || {
        rig.run_writes(1);
        rig.audio.stats.waypoint_jumps.load(Ordering::SeqCst) >= 1
            || rig
                .audio
                .stats
                .worker
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|w| w.waypoints_sent.load(Ordering::SeqCst) >= 1)
    });
    wait_until("waypoint jump", || {
        rig.run_writes(1);
        rig.audio.stats.waypoint_jumps.load(Ordering::SeqCst) == 1
    });
    // The playhead hopped to the waypoint (target - lead), the swoop covers the rest.
    assert!(
        rig.frames() >= 48_000 - 14_400 - 2_000,
        "frames {}",
        rig.frames()
    );
    run_until_landed(&rig);
    assert_eq!(landed_frame(&rig), 48_000);
}

#[test]
fn paused_seek_is_silent_and_immediate() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    index_wav(&wav, FILE_FRAMES);
    let mut rig = Rig::new(1, usize::MAX, Default::default());
    rig.start(&wav, false);
    let before = rig.out.lock().unwrap().len();
    chase(&rig, 700, WarpMode::PitchShift);
    wait_until("seek handled", || {
        rig.audio.stats.seeks_completed.load(Ordering::SeqCst) >= 1
    });
    assert!(!active(&rig));
    assert_eq!(rig.frames(), 33_600);
    assert_eq!(
        rig.out.lock().unwrap().len(),
        before,
        "a paused seek makes no audio"
    );
    // Play continues from the new position.
    rig.audio.try_send(AudioCommand::Play { not_before: None });
    rig.run_writes(2);
    let out = rig.out.lock().unwrap();
    assert_eq!(frame_index_from_f32(out[0]), 33_600);
}

#[test]
fn pausing_during_a_chase_completes_it_silently() {
    let (mut rig, _d) = rig_playing(Default::default(), 1000);
    chase(&rig, 1000, WarpMode::PitchShift);
    rig.run_writes(3);
    assert!(active(&rig));
    let target = rig.writes.load(Ordering::SeqCst) + 1;
    rig.audio.try_send(AudioCommand::Pause);
    rig.allowed.store(target, Ordering::SeqCst);
    wait_until("paused", || {
        !rig.audio.stats.main_playing.load(Ordering::SeqCst)
    });
    wait_until("chase cleared", || !active(&rig));
    // (`chase_active` clears a moment before the silent seek publishes the position)
    wait_until("position is the target", || rig.frames() == 48_000);
    rig.seeks_sent = 0;
}

#[test]
fn stretch_seek_voice_lands_close_to_the_target() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    index_wav(&wav, FILE_FRAMES);
    let mut rig = Rig::new(1, usize::MAX, Default::default());
    // Default speed mode (pitch correct) + forward chase in pitch-correct seek mode.
    rig.start(&wav, true);
    rig.run_writes(5);
    chase(&rig, 1000, WarpMode::PitchCorrect);
    let peak = run_until_landed(&rig);
    assert!(peak <= 2.0 + 1e-6, "stretch cap, peak {peak}");
    let f = rig.frames() as i64;
    assert!((f - 48_000).abs() < 12_000, "landed near the target: {f}");
}

/// Entry, swoop and landing are click-free: the second difference of the rendered sine stays
/// within the smooth bound for the highest instantaneous pitch (rate 1.6).
#[test]
fn swoop_is_continuous_at_entry_and_landing() {
    use plippa_audio::testsupport::{click_metric, smooth_bound};
    for (target_ms, speed_mode) in [
        (150u64, WarpMode::PitchShift),
        (150u64, WarpMode::PitchCorrect),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("sine.wav");
        sine_wav(&wav, 3 * RATE as usize, 0.5);
        let mut rig = Rig::new(1, usize::MAX, Default::default());
        rig.audio
            .try_send(AudioCommand::SetSpeedWarpMode(speed_mode));
        rig.start(&wav, true);
        rig.run_writes(60);
        chase(&rig, 400, WarpMode::PitchShift);
        let _ = target_ms;
        run_until_landed(&rig);
        rig.run_writes(120);
        let out = rig.out.lock().unwrap().clone();
        // Highest instantaneous pitch: the varispeed chase cap (the stage renders it for real
        // since the boxcar reader of WARP_17; before, it clamped at 2x).
        let bound = smooth_bound(0.5, 1013.0 * ChaseConfig::default().r_max_vari, RATE);
        let m = click_metric(&out[4096..], bound);
        assert!(m < 1.5, "{speed_mode:?}: click metric {m}");
    }
}

fn gated_hook(gate: Arc<AtomicBool>) -> DecoderHook {
    DecoderHook(Arc::new(move |_path: &Path, d: Box<dyn BlockSource>| -> Box<dyn BlockSource> {
        Box::new(GatedBlocks::new(d, gate.clone()))
    }))
}

/// WARP_14 leftover: a far jump beyond the window never holds. While the (slow) decoder
/// prepares the waypoint window the audio keeps swooping from where it is (audible, position
/// advancing, no silence); when the window arrives it is adopted through the waypoint
/// crossfade, the chase covers the last lead and lands on the target exactly, and the window
/// never exceeds its cap.
#[test]
fn far_jump_adopts_the_waypoint_window_without_holding() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("long.wav");
    index_wav(&wav, 120 * RATE as usize);
    let gate = Arc::new(AtomicBool::new(true));
    let cfg = AudioThreadConfig {
        decoder_hook: Some(gated_hook(gate.clone())),
        ..Default::default()
    };
    let mut rig = Rig::new(1, usize::MAX, cfg);
    rig.audio
        .try_send(AudioCommand::SetSpeedWarpMode(WarpMode::PitchShift));
    rig.start_at(&wav, true, 10_000);
    rig.run_writes(20);
    // Let the worker fill the lookahead first, then the stick stalls and a 90 s jump is committed.
    // The mock sink consumes without a clock, so pace the consumer (about 4x real time here):
    // the decode worker, a helper process round trip per chunk, then stays ahead of it and the
    // window fills with its full history and lookahead before the stall.
    wait_until("lookahead decoded", || {
        rig.run_writes(4);
        std::thread::sleep(std::time::Duration::from_millis(5));
        rig.audio.stats.peak_window_bytes.load(Ordering::SeqCst) >= 3_000_000
    });
    gate.store(false, Ordering::SeqCst);
    chase(&rig, 100_000, WarpMode::PitchShift);
    rig.run_writes(2);
    let p0 = rig.frames();
    let len0 = rig.out.lock().unwrap().len();
    rig.run_writes(60);
    assert!(active(&rig));
    assert!(rig.frames() > p0 + 20_000, "the swoop keeps travelling: {} -> {}", p0, rig.frames());
    {
        let out = rig.out.lock().unwrap();
        assert!(out[len0..].iter().any(|v| *v != 0.0), "audible while the decoder seeks");
    }
    assert_eq!(rig.audio.stats.waypoints_adopted.load(Ordering::SeqCst), 0);
    assert_eq!(rig.audio.stats.waypoint_jumps.load(Ordering::SeqCst), 0);
    // The decoder catches up: the window is adopted at T - lead (3 s).
    gate.store(true, Ordering::SeqCst);
    wait_until("waypoint adopted", || {
        rig.run_writes(1);
        rig.audio.stats.waypoints_adopted.load(Ordering::SeqCst) == 1
    });
    let lead = 3 * RATE as u64;
    let f = rig.frames();
    assert!(
        f + lead >= 100 * RATE as u64 - 20_000 && f <= 100 * RATE as u64,
        "playhead at the waypoint: {f}"
    );
    run_until_landed(&rig);
    assert_eq!(landed_frame(&rig), 100 * RATE as u64);
    assert_eq!(rig.audio.stats.waypoints_adopted.load(Ordering::SeqCst), 1);
    let peak = rig.audio.stats.peak_window_bytes.load(Ordering::SeqCst);
    assert!(peak <= 6 * 1024 * 1024, "window over the cap: {peak}");
}
