//! WARP_20: the cheap sweep items that earlier packages deferred: click metric through scan
//! start/stop and through a chase with differing voices, and the family-independence rules
//! (`SetRate`/`ResetRate` never cancel a chase or a scan). Logic and signal properties only; the
//! gated sink parks the audio thread between writes, so nothing depends on time.

mod common;
use common::*;
use plippa_audio::testsupport::{click_metric, smooth_bound};
use plippa_core::{ScanDir, WarpMode};
use plippa_engine::{AudioCommand, AudioThreadConfig, RateChange};
use std::sync::atomic::Ordering;

fn sine_rig(play_at_ms: u64, speed_mode: WarpMode, permille: i32) -> (Rig, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("sine.wav");
    sine_wav(&wav, 120 * RATE as usize, 0.5);
    let mut rig = Rig::new(1, usize::MAX, AudioThreadConfig::default());
    rig.audio
        .try_send(AudioCommand::SetSpeedWarpMode(speed_mode));
    if permille != 1000 {
        rig.audio.try_send(AudioCommand::SetRate {
            permille,
            change: RateChange::Immediate,
        });
    }
    rig.start_at(&wav, true, play_at_ms);
    rig.run_writes(8);
    (rig, dir)
}

fn scanning(rig: &Rig) -> bool {
    rig.audio.stats.scan_active.load(Ordering::SeqCst)
}

fn chasing(rig: &Rig) -> bool {
    rig.audio.stats.chase_active.load(Ordering::SeqCst)
}

fn rate(rig: &Rig) -> f64 {
    f64::from(rig.audio.stats.rate_micro.load(Ordering::SeqCst)) / 1e6
}

/// Click bound for a 1013 Hz sine played up to `max_ratio` times faster.
fn bound(max_ratio: f64) -> f64 {
    smooth_bound(0.5, 1013.0 * max_ratio, RATE)
}

#[test]
fn scan_start_and_stop_do_not_click_in_either_seek_mode() {
    for (speed, seek) in [
        (WarpMode::PitchCorrect, WarpMode::PitchShift),
        (WarpMode::PitchShift, WarpMode::PitchShift),
        (WarpMode::PitchCorrect, WarpMode::PitchCorrect),
    ] {
        let (rig, _d) = sine_rig(10_000, speed, 1000);
        let from = rig.out.lock().unwrap().len();
        rig.audio.try_send(AudioCommand::ScanStart {
            dir: ScanDir::Forward,
            mode: seek,
        });
        rig.run_writes(90); // ~1.9 s of audio time: spin-up to 4x, no boost yet
        rig.audio.try_send(AudioCommand::ScanStop);
        rig.run_writes(2);
        let mut n = 0;
        while (scanning(&rig) || (rate(&rig) - 1.0).abs() > 1e-9) && n < 400 {
            rig.run_writes(1);
            n += 1;
        }
        assert!(
            n < 400,
            "the scan never finished braking ({speed:?}/{seek:?})"
        );
        rig.run_writes(10);
        let out = rig.out.lock().unwrap();
        let m = click_metric(&out[from..], bound(4.2));
        assert!(
            m < 1.5,
            "click metric {m} through scan start/stop (speed {speed:?}, seek {seek:?})"
        );
    }
}

#[test]
fn chase_entry_and_landing_with_differing_voices_do_not_click() {
    // Speed voice = stretch (pitch correct, 125%), seek voice = varispeed: the voices differ, so
    // entry and landing are crossfades.
    let (rig, _d) = sine_rig(10_000, WarpMode::PitchCorrect, 1250);
    rig.run_writes(30);
    let from = rig.out.lock().unwrap().len();
    rig.audio.try_send(AudioCommand::SeekChase {
        target_ms: 16_000,
        mode: WarpMode::PitchShift,
    });
    rig.run_writes(2);
    assert!(chasing(&rig));
    let mut n = 0;
    while chasing(&rig) && n < 3_000 {
        rig.run_writes(1);
        n += 1;
    }
    assert!(!chasing(&rig), "the chase never landed");
    rig.run_writes(60);
    let out = rig.out.lock().unwrap();
    let m = click_metric(&out[from..], bound(6.0));
    assert!(m < 1.5, "click metric {m} through chase entry/landing");
}

#[test]
fn reset_rate_during_a_scan_and_set_rate_during_a_chase_cancel_nothing() {
    let (rig, _d) = sine_rig(10_000, WarpMode::PitchShift, 1500);
    rig.audio.try_send(AudioCommand::ScanStart {
        dir: ScanDir::Forward,
        mode: WarpMode::PitchShift,
    });
    rig.run_writes(10);
    assert!(scanning(&rig));
    // ResetRate on the wire is `SetRate{1000}` on the audio thread.
    rig.audio.try_send(AudioCommand::SetRate {
        permille: 1000,
        change: RateChange::Immediate,
    });
    rig.run_writes(10);
    assert!(scanning(&rig), "a tempo reset leaves the scan running");
    assert!(rate(&rig) > 1.5, "still scanning fast: {}", rate(&rig));
    rig.audio.try_send(AudioCommand::ScanStop);
    let mut n = 0;
    while (scanning(&rig) || (rate(&rig) - 1.0).abs() > 1e-9) && n < 600 {
        rig.run_writes(1);
        n += 1;
    }
    assert!(
        (rate(&rig) - 1.0).abs() < 1e-9,
        "brakes to the NEW set speed 1.0, got {}",
        rate(&rig)
    );

    // A chase survives a tempo change and lands at the new set speed.
    rig.audio.try_send(AudioCommand::SeekChase {
        target_ms: 60_000,
        mode: WarpMode::PitchShift,
    });
    rig.run_writes(5);
    assert!(chasing(&rig));
    rig.audio.try_send(AudioCommand::SetRate {
        permille: 750,
        change: RateChange::Immediate,
    });
    rig.run_writes(5);
    assert!(chasing(&rig), "SetRate does not cancel the chase");
    let mut n = 0;
    while chasing(&rig) && n < 3_000 {
        rig.run_writes(1);
        n += 1;
    }
    rig.run_writes(600); // the exponential hand-over settles
    assert!(
        (rate(&rig) - 0.75).abs() < 1e-3,
        "lands at the new set speed: {}",
        rate(&rig)
    );
}
