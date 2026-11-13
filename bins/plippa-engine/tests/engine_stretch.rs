//! WARP_09: the pitch-correct voice through the audio thread (default mode). Numeric / logic
//! properties only; the gated sink parks the audio thread between writes (no timing).

mod common;
use common::*;
use plippa_audio::testsupport::{
    click_metric, fft_peak_frequency, frame_index_from_f32, scan_numeric, smooth_bound,
};
use plippa_core::WarpMode;
use plippa_engine::{AudioCommand, RateChange};
use std::sync::atomic::Ordering;

fn set_rate(rig: &Rig, permille: i32, change: RateChange) {
    rig.audio
        .try_send(AudioCommand::SetRate { permille, change });
}

fn mode(rig: &Rig, m: WarpMode) {
    rig.audio.try_send(AudioCommand::SetSpeedWarpMode(m));
}

fn voice(rig: &Rig) -> u8 {
    rig.audio.stats.speed_voice.load(Ordering::SeqCst)
}

fn run_until_voice(rig: &Rig, want: u8) {
    let mut guard = 0;
    while voice(rig) != want {
        rig.run_writes(2);
        guard += 1;
        assert!(guard < 500, "voice never became {want}");
    }
}

fn sine_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("sine.wav");
    sine_wav(&wav, 40 * RATE as usize, 0.5);
    (dir, wav)
}

#[test]
fn default_mode_keeps_the_pitch_while_the_position_follows_the_rate() {
    let (_d, wav) = sine_file();
    for (permille, rate) in [(1500, 1.5f64), (500, 0.5)] {
        let mut rig = Rig::new(1, usize::MAX, Default::default());
        set_rate(&rig, permille, RateChange::Immediate);
        rig.start(&wav, true);
        rig.run_writes(120);
        assert_eq!(
            voice(&rig),
            2,
            "stretch is the default voice for a positive rate"
        );
        assert!(rig.audio.stats.warp_active.load(Ordering::SeqCst));
        let out = rig.out.lock().unwrap().clone();
        assert!(scan_numeric(&out).is_clean());
        let f = fft_peak_frequency(&out[8192..], RATE);
        assert!((f / 1013.0 - 1.0).abs() < 0.01, "rate {rate}: pitch {f}");
        let frames_out = out.len() as f64;
        let pos = rig.frames() as f64;
        assert!(
            (pos - frames_out * rate).abs() < 0.03 * frames_out * rate + 600.0,
            "rate {rate}: position {pos} vs {}",
            frames_out * rate
        );
    }
}

#[test]
fn negative_rate_selects_varispeed_and_positive_selects_stretch_again() {
    let (_d, wav) = sine_file();
    let mut rig = Rig::new(1, usize::MAX, Default::default());
    rig.start(&wav, true);
    rig.run_writes(20);
    assert_eq!(voice(&rig), 0, "pure at 100 %");
    set_rate(&rig, 1500, RateChange::Immediate);
    rig.run_writes(40);
    assert_eq!(voice(&rig), 2);
    // Park well into the file so reverse has history.
    rig.seek_parked(10_000);
    rig.run_writes(40);
    let from = rig.out.lock().unwrap().len();
    set_rate(&rig, -1000, RateChange::Tempo);
    rig.run_writes(2);
    assert_eq!(
        voice(&rig),
        1,
        "a negative rate is varispeed, never the stretcher"
    );
    rig.run_writes(60);
    set_rate(&rig, 1250, RateChange::Tempo);
    run_until_voice(&rig, 2);
    rig.run_writes(60);
    let out = rig.out.lock().unwrap().clone();
    let seg = &out[from..];
    assert!(scan_numeric(seg).is_clean());
    // Crossfaded voice switches and a ramp through zero: no click (a pure 1013 Hz tone).
    let m = click_metric(seg, smooth_bound(0.5, 1013.0 * 1.6, RATE));
    assert!(m < 1.5, "click metric through the voice switches {m}");
}

#[test]
fn switching_the_mode_while_playing_crossfades_without_a_click() {
    let (_d, wav) = sine_file();
    let mut rig = Rig::new(1, usize::MAX, Default::default());
    set_rate(&rig, 1500, RateChange::Immediate);
    rig.start(&wav, true);
    rig.run_writes(60);
    assert_eq!(voice(&rig), 2);
    let from = rig.out.lock().unwrap().len();
    mode(&rig, WarpMode::PitchShift);
    run_until_voice(&rig, 1);
    rig.run_writes(30);
    mode(&rig, WarpMode::PitchCorrect);
    run_until_voice(&rig, 2);
    rig.run_writes(60);
    let out = rig.out.lock().unwrap().clone();
    let seg = &out[from..];
    assert!(scan_numeric(seg).is_clean());
    // Varispeed at 1.5x is 1520 Hz: bound for the highest tone present.
    let m = click_metric(seg, smooth_bound(0.5, 1013.0 * 1.5, RATE));
    assert!(m < 1.5, "click metric through the mode switches {m}");
}

#[test]
fn returning_to_unity_from_stretch_hands_back_to_the_bit_exact_bypass() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    index_wav(&wav, 40 * RATE as usize / 2);
    let mut rig = Rig::new(1, usize::MAX, Default::default());
    rig.start(&wav, true);
    rig.run_writes(5);
    assert_eq!(voice(&rig), 0, "pure at the start");
    set_rate(&rig, 1250, RateChange::Tempo);
    rig.run_writes(80);
    assert_eq!(voice(&rig), 2);
    set_rate(&rig, 1000, RateChange::Reset);
    run_until_voice(&rig, 0);
    assert!(!rig.audio.stats.warp_active.load(Ordering::SeqCst));
    rig.run_writes(8);
    let out = rig.out.lock().unwrap().clone();
    assert!(scan_numeric(&out).is_clean());
    let tail = &out[out.len() - 1024..];
    for w in tail.windows(2) {
        let (a, b) = (frame_index_from_f32(w[0]), frame_index_from_f32(w[1]));
        assert_eq!(b, (a + 1) % 65_536, "bypass counts frames again");
    }
}

#[test]
fn a_seek_while_stretching_reprimes_at_the_landing() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    index_wav(&wav, 40 * RATE as usize / 2);
    let mut rig = Rig::new(1, usize::MAX, Default::default());
    set_rate(&rig, 1500, RateChange::Immediate);
    rig.start(&wav, true);
    rig.run_writes(30);
    rig.seek_parked(5_000);
    rig.run_writes(30);
    let pos = rig.frames() as f64;
    let landing = 5.0 * f64::from(RATE);
    assert!(pos > landing, "playhead moved on from the landing: {pos}");
    let out = rig.out.lock().unwrap().clone();
    assert!(scan_numeric(&out).is_clean());
    assert_eq!(voice(&rig), 2);
}
