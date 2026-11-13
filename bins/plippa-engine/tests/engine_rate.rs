//! WARP_05: set speed through the audio thread (varispeed voice, bypass hand-over, position).
//! Logic only: the gated sink parks the audio thread between writes, so nothing depends on time.

mod common;
use common::*;
use plippa_audio::testsupport::{fft_peak_frequency, frame_index_from_f32};
use plippa_engine::{AudioCommand, RateChange};
use std::sync::atomic::Ordering;

fn set_rate(rig: &Rig, permille: i32, change: RateChange) {
    rig.audio
        .try_send(AudioCommand::SetRate { permille, change });
}

/// The default speed mode is pitch correct since WARP_09; these tests exercise the varispeed
/// voice, so they ask for pitch shift explicitly.
fn pitch_shift(rig: &Rig) {
    rig.audio
        .try_send(AudioCommand::SetSpeedWarpMode(plippa_core::WarpMode::PitchShift));
}

fn voice(rig: &Rig) -> u8 {
    rig.audio.stats.speed_voice.load(Ordering::SeqCst)
}

#[test]
fn varispeed_changes_pitch_and_media_position_follows_the_rate() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("sine.wav");
    sine_wav(&wav, 30 * RATE as usize, 0.5);
    for (permille, rate) in [(1500, 1.5f64), (500, 0.5)] {
        let mut rig = Rig::new(1, usize::MAX, Default::default());
        pitch_shift(&rig);
        set_rate(&rig, permille, RateChange::Immediate);
        rig.start(&wav, true);
        rig.run_writes(60);
        assert_eq!(voice(&rig), 1, "varispeed while not at 1.0x");
        assert!(rig.audio.stats.warp_active.load(Ordering::SeqCst));
        assert_eq!(
            rig.audio.stats.rate_micro.load(Ordering::SeqCst),
            (rate * 1e6) as i32
        );
        let out = rig.out.lock().unwrap().clone();
        let f = fft_peak_frequency(&out[4096..], RATE);
        assert!(
            (f / 1013.0 / rate - 1.0).abs() < 0.003,
            "rate {rate}: pitch {f}"
        );
        // Media position = output frames x rate, within one quantum (the sink queue is 0).
        let frames_out = out.len() as f64;
        let pos = rig.frames() as f64;
        assert!(
            (pos - frames_out * rate).abs() <= 256.0 * rate + 1.0,
            "rate {rate}: position {pos} vs {}",
            frames_out * rate
        );
    }
}

#[test]
fn returning_to_unity_hands_back_to_the_bit_exact_bypass() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    // Frame-index samples: after the hand-over the output must count up by exactly 1 per frame.
    index_wav(&wav, 40 * RATE as usize / 2);
    let mut rig = Rig::new(1, usize::MAX, Default::default());
    pitch_shift(&rig);
    rig.start(&wav, true);
    rig.run_writes(5);
    assert_eq!(voice(&rig), 0, "pure at the start");
    set_rate(&rig, 1250, RateChange::Tempo);
    rig.run_writes(50);
    assert_eq!(voice(&rig), 1);
    set_rate(&rig, 1000, RateChange::Reset);
    let mut guard = 0;
    while voice(&rig) != 0 {
        rig.run_writes(5);
        guard += 1;
        assert!(guard < 400, "never handed back to bypass");
    }
    assert!(!rig.audio.stats.warp_active.load(Ordering::SeqCst));
    assert_eq!(rig.audio.stats.rate_micro.load(Ordering::SeqCst), 1_000_000);
    // Skip the 5 ms equal-power join, then the output is the source again, one frame at a time.
    rig.run_writes(2);
    let out = rig.out.lock().unwrap().clone();
    let tail = &out[out.len() - 1024..];
    for w in tail.windows(2) {
        let (a, b) = (frame_index_from_f32(w[0]), frame_index_from_f32(w[1]));
        assert_eq!(b, (a + 1) % 65_536, "bypass counts frames again");
    }
}

#[test]
fn snap_rate_jumps_to_unity_and_drops_a_running_scan() {
    // UI_14 §6: the FF/RW-hold + Play gesture sends `SnapRate`, not a ramped `SetRate`. One
    // rendered block later the stage is exactly at 1.0x (no cassette glide) and the scan that
    // owned the rate is gone.
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("snap.wav");
    index_wav(&wav, 40 * RATE as usize);
    let mut rig = Rig::new(1, usize::MAX, Default::default());
    pitch_shift(&rig);
    rig.start(&wav, true);
    rig.run_writes(5);
    set_rate(&rig, 1500, RateChange::Tempo);
    rig.run_writes(30);
    rig.audio.try_send(AudioCommand::ScanStart {
        dir: plippa_ipc_proto::ScanDir::Forward,
        mode: plippa_core::WarpMode::PitchShift,
    });
    rig.run_writes(20);
    assert!(rig.audio.stats.scan_active.load(Ordering::SeqCst));
    rig.audio.try_send(AudioCommand::SnapRate { permille: 1000 });
    rig.run_writes(1);
    assert_eq!(
        rig.audio.stats.rate_micro.load(Ordering::SeqCst),
        1_000_000,
        "the snap is immediate, not a reset ramp"
    );
    assert!(
        !rig.audio.stats.scan_active.load(Ordering::SeqCst),
        "the scan is dropped where it stood"
    );
    assert!(!rig.audio.stats.chase_active.load(Ordering::SeqCst));
}

#[test]
fn reverse_plays_backwards_and_bounces_at_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    index_wav(&wav, 60_000);
    let mut rig = Rig::new(1, usize::MAX, Default::default());
    rig.start(&wav, false);
    // Decode a window, then park at ~0.5 s and play in reverse.
    rig.audio.try_send(AudioCommand::Play { not_before: None });
    rig.run_writes(30);
    set_rate(&rig, -1000, RateChange::Immediate);
    rig.seek_parked(500);
    let marker = rig.out.lock().unwrap().len();
    rig.run_writes(12); // reverse renders 256-frame quanta
    {
        let out = rig.out.lock().unwrap();
        // After the 10 ms seek join the output counts down by exactly one frame per frame.
        for i in 1_000..12 * 256 - 2 {
            let (a, b) = (
                frame_index_from_f32(out[marker + i]),
                frame_index_from_f32(out[marker + i + 1]),
            );
            assert_eq!(a, (b + 1) % 65_536, "reverse counts down at {i}");
        }
    }
    assert_eq!(
        rig.audio.stats.rate_micro.load(Ordering::SeqCst),
        -1_000_000
    );
    // Keep going until reverse hits frame 0: it bounces and runs forward at the set magnitude.
    let mut guard = 0;
    while rig.audio.stats.rate_bounces.load(Ordering::SeqCst) == 0 {
        rig.run_writes(20);
        guard += 1;
        assert!(guard < 100, "no bounce");
    }
    rig.run_writes(40);
    assert!(rig.audio.stats.rate_micro.load(Ordering::SeqCst) > 0);
    assert!(rig.frames() > 0 && rig.frames() < 60_000);
}
