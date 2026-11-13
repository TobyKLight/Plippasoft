//! WARP_17: the hold scan through the audio thread. Logic and position/rate maths only: the
//! gated sink parks the audio thread between writes, and the scan clock is audio time (frames
//! rendered), so nothing here depends on wall-clock time.

mod common;
use common::*;
use plippa_audio::testsupport::frame_index_from_f32;
use plippa_audio::{OverspecPolicy, ScanProfile};
use plippa_core::{Capabilities, ScanDir, WarpMode};
use plippa_engine::{AudioCommand, AudioThreadConfig, PlayArm, RateChange};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn scan(rig: &Rig, dir: ScanDir, mode: WarpMode) {
    rig.audio.try_send(AudioCommand::ScanStart { dir, mode });
}

fn stop(rig: &Rig) {
    rig.audio.try_send(AudioCommand::ScanStop);
}

fn rate(rig: &Rig) -> f64 {
    f64::from(rig.audio.stats.rate_micro.load(Ordering::SeqCst)) / 1e6
}

fn scanning(rig: &Rig) -> bool {
    rig.audio.stats.scan_active.load(Ordering::SeqCst)
}

fn playing(rig: &Rig) -> bool {
    rig.audio.stats.main_playing.load(Ordering::SeqCst)
}

/// One more write, or until the audio thread stops playing (a pause or the end of the stream
/// parks nothing). Returns false when it stopped playing instead of writing.
fn step(rig: &Rig) -> bool {
    let target = rig.writes.load(Ordering::SeqCst) + 1;
    rig.allowed.store(target, Ordering::SeqCst);
    wait_until("write or stop", || {
        (rig.writes.load(Ordering::SeqCst) == target && rig.blocked.load(Ordering::SeqCst))
            || !playing(rig)
    });
    playing(rig)
}

fn rig_at(
    frames_s: usize,
    start_ms: u64,
    play: bool,
    speed_mode: WarpMode,
) -> (Rig, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    index_wav(&wav, frames_s * RATE as usize);
    let mut rig = Rig::new(1, usize::MAX, AudioThreadConfig::default());
    rig.audio
        .try_send(AudioCommand::SetSpeedWarpMode(speed_mode));
    rig.start_at(&wav, play, start_ms);
    if play {
        rig.run_writes(5);
    }
    (rig, dir)
}

#[test]
fn scan_profile_hold_boost_and_release_keep_the_position_continuous() {
    let p = ScanProfile::default();
    let (rig, _d) = rig_at(240, 10_000, true, WarpMode::PitchShift);
    scan(&rig, ScanDir::Forward, WarpMode::PitchShift);
    let q = 256.0 / f64::from(RATE);
    let mut prev_rate = 1.0;
    let mut prev_pos = rig.frames();
    let mut t = 0.0;
    let mut max_delta = 0.0f64;
    // Held for a little over 3 s of audio time.
    while t < 3.0 {
        assert!(step(&rig));
        t += q;
        let (r, pos) = (rate(&rig), rig.frames());
        assert!(
            r >= prev_rate - 1e-6,
            "monotone spin-up: {prev_rate} -> {r} at {t}"
        );
        assert!(r <= p.boost + 1e-6);
        if t < p.boost_after_s - 2.0 * q {
            assert!(r <= p.base + 1e-6, "no boost before 2 s: {r} at {t}");
        }
        if t > 0.95 {
            assert!(r >= 0.95 * p.base, "95% of 4x within ~0.9 s: {r} at {t}");
        }
        assert!(pos >= prev_pos, "position never goes back");
        max_delta = max_delta.max((pos - prev_pos) as f64);
        prev_rate = r;
        prev_pos = pos;
    }
    assert!(
        prev_rate > p.base + 0.5,
        "boosting toward 8x after 2 s: {prev_rate}"
    );
    assert!(
        max_delta <= 256.0 * p.boost + 2.0,
        "bounded derivative: {max_delta}"
    );
    // Release (UI_15): no brake at all. The rate is the set speed again in the very turn that
    // takes the stop, before any further audio is written, and the playhead carries on from
    // where the audio got to.
    stop(&rig);
    let mut n = 0;
    while scanning(&rig) {
        // (the stop command is taken at the next loop turn)
        assert!(step(&rig));
        n += 1;
        assert!(n <= 2, "the release is one turn, not a brake");
    }
    assert!((rate(&rig) - 1.0).abs() < 1e-9, "set speed immediately");
    let rel_pos = rig.frames();
    assert!(rel_pos >= prev_pos, "the playhead never steps back");
    // Back at the set speed in the bypass voice; playback continues from where the audio is.
    assert!(!rig.audio.stats.warp_active.load(Ordering::SeqCst));
    for _ in 0..20 {
        let before = rig.frames();
        assert!(step(&rig));
        assert!(rig.frames() > before);
    }
}

#[test]
fn release_restores_a_non_unity_set_speed_in_one_step() {
    // The owner's correction: on release restore `target_rate_permille`, never hardcoded 100 %.
    let (rig, _d) = rig_at(240, 10_000, true, WarpMode::PitchShift);
    rig.audio.try_send(AudioCommand::SetRate {
        permille: 1500,
        change: RateChange::Immediate,
    });
    rig.run_writes(5);
    assert!((rate(&rig) - 1.5).abs() < 1e-9);
    scan(&rig, ScanDir::Forward, WarpMode::PitchShift);
    let mut peak = 0.0f64;
    for _ in 0..250 {
        assert!(step(&rig));
        peak = peak.max(rate(&rig));
    }
    assert!(peak > 3.5, "scanning near the usual 4x: {peak}");
    let pos_before = rig.frames();
    stop(&rig);
    let mut n = 0;
    while scanning(&rig) {
        assert!(step(&rig));
        n += 1;
        assert!(n <= 2, "no brake: {n} turns");
    }
    // Exactly the set speed: one step from the scan rate, no intermediate values.
    assert!(
        (rate(&rig) - 1.5).abs() < 1e-9,
        "release restores 150 %, not 100 %: {}",
        rate(&rig)
    );
    assert!(rig.audio.stats.warp_active.load(Ordering::SeqCst), "still warped");
    // The playhead carries on from where the scan was, at the set speed.
    let pos_after = rig.frames();
    assert!(pos_after >= pos_before);
    for _ in 0..10 {
        let before = rig.frames();
        assert!(step(&rig));
        let moved = rig.frames() - before;
        // (the varispeed voice writes one quantum at a time, the bypass path whole blocks)
        assert!(
            moved <= BLOCK as u64 && (moved as f64 - 256.0 * 1.5).abs() < 256.0 * 1.5,
            "advancing at 1.5x: {moved}"
        );
    }
    // Calling it again with no scan is harmless.
    stop(&rig);
    rig.run_writes(3);
    assert!((rate(&rig) - 1.5).abs() < 1e-9);
    assert!(!scanning(&rig));
}

#[test]
fn scan_reference_is_absolute_and_returns_to_the_set_speed() {
    let (rig, _d) = rig_at(240, 10_000, true, WarpMode::PitchShift);
    rig.audio.try_send(AudioCommand::SetRate {
        permille: 1500,
        change: RateChange::Immediate,
    });
    rig.run_writes(5);
    assert!((rate(&rig) - 1.5).abs() < 1e-6);
    scan(&rig, ScanDir::Forward, WarpMode::PitchShift);
    // (the command is taken at the next loop turn: the first block may still be unscanned)
    let mut first = 1.5;
    for _ in 0..3 {
        assert!(step(&rig));
        first = rate(&rig);
        if first != 1.5 {
            break;
        }
    }
    assert!(
        first > 1.5 && first < 1.7,
        "starts from 1.5 without a pitch step: {first}"
    );
    let mut peak: f64 = 0.0;
    for _ in 0..300 {
        assert!(step(&rig));
        peak = peak.max(rate(&rig));
    }
    assert!(peak <= 4.0 + 1e-6, "absolute 4x, not 6x: {peak}");
    assert!(peak > 3.8);
    stop(&rig);
    let mut n = 0;
    while scanning(&rig) {
        assert!(step(&rig));
        n += 1;
        assert!(n < 1000);
    }
    assert!(
        (rate(&rig) - 1.5).abs() < 1e-6,
        "back to the set speed: {}",
        rate(&rig)
    );
}

#[test]
fn scan_from_paused_is_audible_and_returns_to_paused_where_the_audio_got_to() {
    let (mut rig, _d) = rig_at(240, 10_000, false, WarpMode::PitchShift);
    assert!(!playing(&rig));
    let before = rig.out.lock().unwrap().len();
    scan(&rig, ScanDir::Forward, WarpMode::PitchShift);
    rig.run_writes(60);
    assert!(playing(&rig) && scanning(&rig));
    {
        let out = rig.out.lock().unwrap();
        assert!(
            out.len() > before && out[before..].iter().any(|v| *v != 0.0),
            "audible"
        );
    }
    let held_pos = rig.frames();
    assert!(
        held_pos > 10_000 * 48 + 5_000,
        "position advanced: {held_pos}"
    );
    stop(&rig);
    let mut n = 0;
    while step(&rig) {
        n += 1;
        assert!(n < 1000, "never returned to paused");
    }
    assert!(!scanning(&rig));
    let paused_pos = rig.frames();
    assert!(
        paused_pos >= held_pos,
        "the playhead is where the audio got to"
    );
    assert!(
        (rate(&rig) - 1.0).abs() < 1e-6,
        "set speed restored for the next Play"
    );
    // Paused: nothing more is written; the jog (SeekChase) is silent.
    let len = rig.out.lock().unwrap().len();
    rig.audio.try_send(AudioCommand::SeekChase {
        target_ms: 20_000,
        mode: WarpMode::PitchShift,
    });
    wait_until("seek handled", || {
        rig.audio.stats.seeks_completed.load(Ordering::SeqCst) >= 1
    });
    assert_eq!(
        rig.out.lock().unwrap().len(),
        len,
        "a paused jog makes no audio"
    );
    rig.seeks_sent = 0;
}

#[test]
fn scan_from_paused_resumes_play_where_it_stopped() {
    let (rig, _d) = rig_at(240, 10_000, false, WarpMode::PitchShift);
    scan(&rig, ScanDir::Forward, WarpMode::PitchShift);
    rig.run_writes(40);
    stop(&rig);
    while step(&rig) {}
    let pos = rig.frames();
    let len = rig.out.lock().unwrap().len();
    rig.audio.try_send(AudioCommand::Play { not_before: None });
    rig.run_writes(2);
    let out = rig.out.lock().unwrap();
    let first = frame_index_from_f32(out[len]) as i64;
    assert!(
        (first - (pos % 65_536) as i64).abs() <= 2,
        "Play continues at the paused playhead: {first} vs {}",
        pos % 65_536
    );
}

#[test]
fn release_from_paused_pauses_exactly_there_and_keeps_a_non_unity_set_speed() {
    // UI_15: from paused the release stops the audio immediately (no `tau_stop` coast) at the
    // exact position, and the set speed it restores for the next Play is the *set* speed, not
    // 100 % and not the scan speed.
    let (rig, _d) = rig_at(240, 10_000, false, WarpMode::PitchShift);
    rig.audio.try_send(AudioCommand::SetRate {
        permille: 500,
        change: RateChange::Immediate,
    });
    assert!(!playing(&rig));
    scan(&rig, ScanDir::Forward, WarpMode::PitchShift);
    rig.run_writes(40);
    assert!(playing(&rig) && scanning(&rig));
    assert!(rate(&rig) > 1.5, "scanning: {}", rate(&rig));
    stop(&rig);
    let mut n = 0;
    while step(&rig) {
        n += 1;
        assert!(n < 5, "the pause happens on the next turn, not after a coast");
    }
    assert!(!scanning(&rig) && !playing(&rig));
    assert!(
        (rate(&rig) - 0.5).abs() < 1e-9,
        "the set speed (500 permille) is restored, not 100 %: {}",
        rate(&rig)
    );
    let settled = rig.frames();
    // Nothing moves afterwards: no writes, no position drift.
    let out_len = rig.out.lock().unwrap().len();
    std::thread::sleep(std::time::Duration::from_millis(80));
    assert_eq!(rig.frames(), settled, "the playhead is settled");
    assert_eq!(rig.out.lock().unwrap().len(), out_len, "no audio after the pause");
    // Play resumes exactly there at the set speed.
    rig.audio.try_send(AudioCommand::Play { not_before: None });
    rig.run_writes(2);
    let out = rig.out.lock().unwrap();
    let first = frame_index_from_f32(out[out_len]) as i64;
    assert!(
        (first - (settled % 65_536) as i64).abs() <= 2,
        "resume at the settled playhead: {first} vs {}",
        settled % 65_536
    );
}

#[test]
fn scan_speed_ladder_retargets_the_live_scan_and_never_touches_the_set_speed() {
    // UI_15: the encoder's ladder is a scan-only multiplier. It re-targets a running scan
    // (upward and down to below 1x) and the release still restores the *set* speed.
    let (rig, _d) = rig_at(240, 10_000, true, WarpMode::PitchShift);
    rig.audio.try_send(AudioCommand::SetRate {
        permille: 1500,
        change: RateChange::Immediate,
    });
    rig.run_writes(5);
    assert!((rate(&rig) - 1.5).abs() < 1e-9);
    scan(&rig, ScanDir::Forward, WarpMode::PitchShift);
    let mut usual = 0.0f64;
    for _ in 0..150 {
        assert!(step(&rig));
        usual = usual.max(rate(&rig));
    }
    assert!(usual > 3.5 && usual <= 4.05, "the usual 4x scan: {usual}");
    // 400 % of the usual scan: the target is 4x of 4x, capped at the 8x vari-cap.
    rig.audio.try_send(AudioCommand::ScanSpeed { permille: 4000 });
    let mut peak = usual;
    for _ in 0..250 {
        assert!(step(&rig));
        peak = peak.max(rate(&rig));
    }
    assert!(peak > usual + 1.0, "re-targeted upward: {peak}");
    assert!(peak <= plippa_core::MAX_SEEK_VARI_SCAN + 1e-6, "capped: {peak}");
    // 5 % of the usual scan: 0.2x, below 1x playback, and it actually gets there (the cap is
    // an upper bound, never a floor).
    rig.audio.try_send(AudioCommand::ScanSpeed { permille: 50 });
    let mut low = f64::MAX;
    for _ in 0..2500 {
        assert!(step(&rig));
        low = low.min(rate(&rig));
    }
    // (the hold has already boosted to 8x here, so 5 % of the usual speed is 0.4x: still
    // well below 1x playback — the multiplier is relative to the phase's usual speed.)
    assert!((low - 0.4).abs() < 0.02, "sub-unity scan target: {low}");
    // Release: the set speed, never the scan speed the encoder landed on.
    stop(&rig);
    while scanning(&rig) {
        assert!(step(&rig));
    }
    assert!(
        (rate(&rig) - 1.5).abs() < 1e-9,
        "release restores 150 %, not 20 %: {}",
        rate(&rig)
    );
}

#[test]
fn rewind_at_zero_while_paused_has_no_effect_and_forward_end_is_the_normal_end() {
    // RW at 0:00 paused: nothing happens (no audio, no scan left behind).
    let (rig, _d) = rig_at(20, 0, false, WarpMode::PitchShift);
    scan(&rig, ScanDir::Backward, WarpMode::PitchShift);
    wait_until("handled", || {
        rig.audio.stats.scans_handled.load(Ordering::SeqCst) >= 1
    });
    assert!(!scanning(&rig) && !playing(&rig));
    assert!(rig.out.lock().unwrap().is_empty());
    // FF while playing, into the end of a short file: the normal end of the track.
    let (rig, _d) = rig_at(20, 18_000, true, WarpMode::PitchShift);
    scan(&rig, ScanDir::Forward, WarpMode::PitchShift);
    let mut n = 0;
    while step(&rig) {
        n += 1;
        assert!(n < 2000, "never reached the end");
    }
    assert!(rig.audio.stats.main_finished.load(Ordering::SeqCst));
    assert!(!rig.audio.stats.has_main_stream.load(Ordering::SeqCst));
    assert!(!scanning(&rig));
}

#[test]
fn reverse_scan_reaching_zero_bounces_and_plays_forward_at_the_set_speed() {
    let (rig, _d) = rig_at(60, 1_500, true, WarpMode::PitchShift);
    scan(&rig, ScanDir::Backward, WarpMode::PitchShift);
    // Hold until the scan ends by itself at 0:00 (the worker places the back-windows).
    let mut n = 0;
    while scanning(&rig) || n < 3 {
        assert!(step(&rig));
        n += 1;
        assert!(n < 4000, "the reverse scan never reached 0:00");
    }
    // Cleared, playing forward again toward the set speed, from near the start.
    assert!(playing(&rig));
    for _ in 0..300 {
        assert!(step(&rig));
    }
    assert!(
        (rate(&rig) - 1.0).abs() < 1e-6,
        "set speed again: {}",
        rate(&rig)
    );
    let pos = rig.frames();
    assert!(
        pos > 0 && pos < 20 * 48_000,
        "forward from the start: {pos}"
    );
    assert_eq!(
        rig.audio.stats.rate_bounces.load(Ordering::SeqCst),
        0,
        "set speed untouched"
    );
}

#[test]
fn scan_cancels_a_chase_and_ignores_the_jog() {
    let (rig, _d) = rig_at(240, 10_000, true, WarpMode::PitchShift);
    rig.audio.try_send(AudioCommand::SeekChase {
        target_ms: 200_000,
        mode: WarpMode::PitchShift,
    });
    rig.run_writes(3);
    assert!(rig.audio.stats.chase_active.load(Ordering::SeqCst));
    scan(&rig, ScanDir::Forward, WarpMode::PitchShift);
    rig.run_writes(2);
    assert!(
        !rig.audio.stats.chase_active.load(Ordering::SeqCst),
        "ScanStart cancels the chase"
    );
    assert!(scanning(&rig));
    rig.audio.try_send(AudioCommand::SeekChase {
        target_ms: 5_000,
        mode: WarpMode::PitchShift,
    });
    rig.run_writes(3);
    assert!(
        !rig.audio.stats.chase_active.load(Ordering::SeqCst),
        "the jog is ignored while scanning"
    );
    stop(&rig);
}

#[test]
fn seek_voice_follows_the_seek_mode_forward_and_backward_is_always_varispeed() {
    // Pitch-correct seek mode, forward: the stretch voice, clamped at 3x.
    let (rig, _d) = rig_at(240, 60_000, true, WarpMode::PitchShift);
    scan(&rig, ScanDir::Forward, WarpMode::PitchCorrect);
    let mut peak: f64 = 0.0;
    for _ in 0..500 {
        assert!(step(&rig));
        peak = peak.max(rate(&rig));
    }
    assert_eq!(
        rig.audio.stats.speed_voice.load(Ordering::SeqCst),
        2,
        "stretch voice"
    );
    assert!(
        peak <= plippa_core::MAX_SEEK_STRETCH_SCAN + 0.05,
        "clamped at 3x: {peak}"
    );
    assert!(peak > 2.5);
    stop(&rig);
    while scanning(&rig) {
        assert!(step(&rig));
    }
    // Backward in pitch-correct seek mode: varispeed anyway, and it reaches the 4x target.
    for _ in 0..800 {
        assert!(step(&rig)); // history behind the playhead for the rewind
    }
    scan(&rig, ScanDir::Backward, WarpMode::PitchCorrect);
    let mut low: f64 = 0.0;
    for _ in 0..300 {
        assert!(step(&rig));
        low = low.min(rate(&rig));
    }
    assert_eq!(
        rig.audio.stats.speed_voice.load(Ordering::SeqCst),
        1,
        "varispeed"
    );
    assert!(
        low < -3.0,
        "backward is not capped at the stretch cap: {low}"
    );
    stop(&rig);
}

/// UI_19 (owner bug): a scan that starts from paused begins at a **standstill**, so it must
/// never travel forward first. The old path left the rate at the set speed (1.0) and glided it
/// to the negative scan target, playing a burst of forward audio before reversing — very
/// audible, and worst at the low ladder speeds where the crossing is slow.
#[test]
fn a_paused_rewind_scan_never_plays_forward_first() {
    let (rig, _d) = rig_at(240, 20_000, false, WarpMode::PitchShift);
    assert!(!playing(&rig), "starts paused");
    let start = rig.frames();
    scan(&rig, ScanDir::Backward, WarpMode::PitchShift);
    rig.run_writes(1);
    let mut seen = Vec::new();
    for i in 0..20 {
        if !step(&rig) {
            break;
        }
        seen.push((i, rate(&rig), rig.frames()));
    }
    assert!(!seen.is_empty(), "the paused scan wrote audio");
    let forward: Vec<_> = seen.windows(2).filter(|w| w[1].2 > w[0].2).collect();
    assert!(
        forward.is_empty() && seen[0].2 <= start,
        "a paused rewind played forward: start {start}, trace {seen:?}"
    );
    assert!(
        seen.last().unwrap().2 < start,
        "the paused rewind went backwards: {seen:?}"
    );
    stop(&rig);
    while step(&rig) {}
}

/// UI_19: pressing FF and then RW (or back) while paused changes direction through **rest**,
/// never through forward playback — a standstill hold has no 1.0x to glide down from.
#[test]
fn a_paused_direction_change_glides_through_rest_not_forward() {
    let (rig, _d) = rig_at(240, 20_000, false, WarpMode::PitchShift);
    scan(&rig, ScanDir::Forward, WarpMode::PitchShift);
    rig.run_writes(1);
    for _ in 0..10 {
        step(&rig);
    }
    scan(&rig, ScanDir::Backward, WarpMode::PitchShift);
    let at_switch = rig.frames();
    let mut seen = Vec::new();
    for i in 0..15 {
        if !step(&rig) {
            break;
        }
    seen.push((i, rate(&rig), rig.frames()));
    }
    let forward: Vec<_> = seen.windows(2).filter(|w| w[1].2 > w[0].2).collect();
    assert!(
        forward.is_empty(),
        "the paused turn-around went forward from {at_switch}: {seen:?}"
    );
    stop(&rig);
    while step(&rig) {}
}

/// UI_19 (owner bug): a tap arms the main stream behind the SFX cover — the transport is
/// Playing but the main playhead does not move for that second. The published rate is 0 then,
/// so a UI dead-reckoning the playhead shows the armed start rather than a second of false
/// playback that snaps back when the audio starts (the first main write republishes 1x).
#[test]
fn an_armed_start_publishes_a_still_playhead_until_the_audio_starts() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    index_wav(&wav, 240 * RATE as usize);
    let rig = Rig::new(1, usize::MAX, AudioThreadConfig::default());
    let file = std::fs::File::open(&wav).expect("fixture opens");
    rig.audio
        .try_send(AudioCommand::StartMainStream {
            path: wav,
            file,
            capabilities: Capabilities::desktop_mock(),
            overspec: OverspecPolicy::default(),
            start_ms: 20_000,
            play: Some(PlayArm {
                not_before: Some(Instant::now() + Duration::from_millis(400)),
            }),
            item_position: 0,
        });
    wait_until("armed", || {
        rig.audio.stats.starts_completed.load(Ordering::SeqCst) >= 1
            && rig.audio.stats.has_main_stream.load(Ordering::SeqCst)
    });
    assert!(playing(&rig), "the transport is Playing while armed");
    assert_eq!(rate(&rig), 0.0, "nothing is being heard while armed");
    assert_eq!(rig.frames(), 20_000 * 48, "the playhead sits at the start");
    rig.run_writes(1);
    assert!(
        (rate(&rig) - 1.0).abs() < 1e-6,
        "the first main write publishes the set speed: {}",
        rate(&rig)
    );
}

/// UI_19 (owner bug): the release must land where the listener heard it. The sink holds up to
/// a buffer of scan-rate audio; left in place it plays out after the release, so the playhead
/// kept travelling in the scan direction for that media span (a rewind carried on ~170 ms and
/// then turned around) — the "release over-runs and then jumps" the owner reported. The release
/// drops the queued scan audio and continues from the heard position at the set speed.
#[test]
fn a_scan_release_lands_on_the_heard_position_in_both_directions() {
    // 80 ms of output buffered, as the Pulse sink holds on the device.
    for dir in [ScanDir::Backward, ScanDir::Forward] {
        let (rig, _d) = rig_at(240, 20_000, true, WarpMode::PitchShift);
        rig.set_sink_queue(3_840);
        scan(&rig, dir, WarpMode::PitchShift);
        for _ in 0..60 {
            step(&rig);
        }
        let held = rig.frames();
        assert!(rate(&rig).abs() > 1.5, "{dir:?}: spinning up");
        stop(&rig);
        while scanning(&rig) {
            step(&rig);
        }
        assert!(
            (rate(&rig) - 1.0).abs() < 1e-6,
            "{dir:?}: the set speed is published at once, got {}",
            rate(&rig)
        );
        let mut prev = rig.frames();
        let mut trace = vec![prev];
        for _ in 0..10 {
            step(&rig);
            let pos = rig.frames();
            trace.push(pos);
            match dir {
                // Nothing further backwards after a rewind release.
                ScanDir::Backward => assert!(
                    pos >= prev,
                    "a rewind release kept rewinding: {trace:?} (held {held})"
                ),
                // And the queued fast-forward does not carry the playhead on at scan speed.
                ScanDir::Forward => {
                    let step_frames = pos.saturating_sub(prev);
                    assert!(
                        step_frames < 1_600,
                        "a forward release ran on at scan speed: {trace:?} (held {held})"
                    );
                }
            }
            prev = pos;
        }
    }
}

/// UI_19: from paused the release already stops exactly (the standstill path flushes); a paused
/// rewind must also not carry the playhead forward first — the direction fix and the flush must
/// hold together.
#[test]
fn a_paused_scan_release_never_touches_the_tape_again() {
    let (rig, _d) = rig_at(240, 20_000, false, WarpMode::PitchShift);
    rig.set_sink_queue(3_840);
    scan(&rig, ScanDir::Backward, WarpMode::PitchShift);
    rig.run_writes(1);
    for _ in 0..20 {
        step(&rig);
    }
    stop(&rig);
    while step(&rig) {}
    assert!(!scanning(&rig), "the standstill release pauses");
    assert_eq!(rate(&rig), 1.0, "the set speed is restored for the next Play");
}
