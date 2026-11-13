//! SEAM_01 part B on the audio thread: a join of the next track at normal speed. Sample maths
//! only: the gated sink parks the audio thread between writes, so nothing depends on wall-clock
//! time (apart from the neighbour's helper thread, which the drive loop gives a moment to run).
//!
//! The regression test that a real gap or click would fail is the first one: the output is
//! sample-identical to the two files' own decodes back to back.

mod common;
use common::*;
use plippa_audio::testsupport::{click_metric, smooth_bound, write_pcm16_wav};
use plippa_core::{Capabilities, WarpMode};
use plippa_engine::{AudioCommand, AudioThreadConfig, CrossEvent, NeighbourSrc, DecoderHook};
use plippa_audio::testsupport::GatedBlocks;
use plippa_audio::BlockSource;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const AMP: f64 = 0.5;
/// Track A is exactly one second, B one and a half: different, so a duration names the track.
const A_FRAMES: usize = RATE as usize;
const B_FRAMES: usize = RATE as usize * 3 / 2;

/// `frames` frames of the 1013 Hz sine starting at global frame `from`: consecutive files join
/// into one continuous tone, and the first file ends mid-waveform.
fn tone(path: &Path, from: usize, frames: usize) {
    write_pcm16_wav(path, RATE, 1, frames, |f, _| {
        let v =
            AMP * (2.0 * std::f64::consts::PI * 1013.0 * (from + f) as f64 / f64::from(RATE)).sin();
        (v * 32_767.0).round() as i16
    })
    .unwrap();
}

struct Files {
    _dir: tempfile::TempDir,
    a: PathBuf,
    b: PathBuf,
    /// What A then B decode to, back to back.
    expected: Vec<f32>,
}

fn files() -> Files {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
    tone(&a, 0, A_FRAMES);
    tone(&b, A_FRAMES, B_FRAMES);
    let mut expected = plippa_audio::decode(&a).unwrap().samples;
    expected.extend(plippa_audio::decode(&b).unwrap().samples);
    assert_eq!(expected.len(), A_FRAMES + B_FRAMES);
    Files { _dir: dir, a, b, expected }
}

fn src(p: &Path, seamless: bool) -> NeighbourSrc {
    NeighbourSrc {
        path: p.to_path_buf(),
        capabilities: Capabilities::desktop_mock(),
        overspec: Default::default(),
        item_position: 1,
        seamless,
        scan: false,
    }
}

fn set_next(rig: &Rig, next: Option<NeighbourSrc>) {
    rig.audio.try_send(AudioCommand::SetNeighbours { prev: None, next });
}

fn rig(cfg: AudioThreadConfig) -> Rig {
    let rig = Rig::new(1, usize::MAX, cfg);
    rig.audio
        .try_send(AudioCommand::SetSpeedWarpMode(WarpMode::PitchShift));
    rig
}

fn finished(rig: &Rig) -> bool {
    rig.audio.stats.main_finished.load(Ordering::SeqCst)
}

fn crossings(rig: &Rig) -> u64 {
    rig.audio.stats.crossings.load(Ordering::SeqCst)
}

fn fallbacks(rig: &Rig) -> u64 {
    rig.audio.stats.seamless_fallbacks.load(Ordering::SeqCst)
}

fn events(rig: &Rig) -> Vec<CrossEvent> {
    rig.audio.stats.cross_events.lock().unwrap().clone()
}

fn captured(rig: &Rig) -> Vec<f32> {
    rig.out.lock().unwrap().clone()
}

/// One more write, or the end of the stream. A moment for the neighbour's helper thread.
fn step(rig: &Rig) {
    let target = rig.writes.load(Ordering::SeqCst) + 1;
    rig.allowed.store(target, Ordering::SeqCst);
    let t = Instant::now();
    while !((rig.writes.load(Ordering::SeqCst) == target && rig.blocked.load(Ordering::SeqCst))
        || finished(rig))
    {
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "no write: frames {} crossings {} finished {}",
            rig.frames(),
            crossings(rig),
            finished(rig)
        );
        std::thread::sleep(Duration::from_micros(200));
    }
    std::thread::sleep(Duration::from_micros(300));
}

/// Step until `done` or the stream ends; returns the writes made.
fn drive(rig: &Rig, mut done: impl FnMut(&Rig) -> bool) -> usize {
    let mut n = 0;
    while !finished(rig) && !done(rig) {
        step(rig);
        n += 1;
        assert!(n < 5_000, "the stream never ended");
    }
    n
}

fn worst_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "lengths");
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

/// A started, playing rig with A open and B offered as the seamless next, B given time to open.
fn playing(f: &Files, seamless: bool) -> Rig {
    let mut rig = rig(AudioThreadConfig::default());
    rig.start_at(&f.a, true, 0);
    rig.run_writes(2);
    set_next(&rig, Some(src(&f.b, seamless)));
    rig
}

#[test]
fn a_seamless_join_at_unity_is_sample_identical_to_the_two_decodes_back_to_back() {
    let f = files();
    let rig = playing(&f, true);
    drive(&rig, |_| false);
    assert!(finished(&rig), "the stream never ended");
    let out = captured(&rig);
    // Nothing missing, nothing added, nothing changed: no gap, no click, no overlap.
    assert_eq!(out.len(), f.expected.len(), "frames out");
    let worst = worst_diff(&out, &f.expected);
    assert!(worst < 1e-6, "the join differs from the files back to back by {worst}");
    // The stitch is also free of any click on its own terms.
    let bound = smooth_bound(AMP, 1013.0 * 8.0, RATE);
    assert!(click_metric(&out[1024..], bound) < 1.5);
    // One crossing, forward, at the end of A, and it is the seamless kind.
    let ev = events(&rig);
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!((ev[0].dir, ev[0].seamless, ev[0].item_position), (1, true, 1));
    assert!((ev[0].left_ms as i64 - 1_000).abs() <= 1, "left at {}", ev[0].left_ms);
    assert_eq!(ev[0].new_duration_ms, 1_500);
    assert_eq!(fallbacks(&rig), 0);
    assert_eq!(rig.audio.stats.cross_failures.load(Ordering::SeqCst), 0);
}

#[test]
fn the_position_restarts_at_zero_on_the_flip_and_never_runs_backwards_after() {
    let f = files();
    let rig = playing(&f, true);
    // 5000 frames still queued behind the sink: the heard position lags the written one.
    rig.set_sink_queue(5_000);
    let mut flipped = false;
    let mut seen = Vec::new();
    drive(&rig, |rig| {
        flipped |= crossings(rig) >= 1;
        if flipped {
            seen.push(rig.frames());
        }
        false
    });
    // The status published with the flip (still of the block before it) is the old item's;
    // the first one after it is the next item at 0:00, and from there it only moves forward.
    assert!(seen.len() > 20);
    assert_eq!(seen[1], 0, "first position after the flip: {:?}", &seen[..4]);
    assert!(
        seen[1..].windows(2).all(|w| w[1] >= w[0]),
        "the position ran backwards after the flip: {:?}", seen
    );
    let after_flip = seen;
    // The end of the stream reports the end of B.
    assert_eq!(rig.frames(), B_FRAMES as u64);
    let _ = after_flip;
}

#[test]
fn the_exchange_leaves_the_feed_on_the_next_track_at_the_frames_consumed() {
    let f = files();
    let rig = playing(&f, true);
    // Well past the exchange (13312 frames into B): the position is B's own.
    let past = A_FRAMES as u64 + 20_000;
    drive(&rig, |rig| rig.frames() > 0 && captured(rig).len() as u64 >= past);
    assert_eq!(rig.audio.stats.duration_ms.load(Ordering::SeqCst), 1_500);
    let written = captured(&rig).len() as u64;
    // Unqueued sink: the heard position is the written one, in B's frames.
    assert_eq!(rig.frames(), written - A_FRAMES as u64, "read position after the exchange");
    // And the join does not come back: the rest of B plays out to its end.
    drive(&rig, |_| false);
    assert_eq!(captured(&rig).len(), A_FRAMES + B_FRAMES);
    assert_eq!(crossings(&rig), 1);
}

#[test]
fn a_neighbour_that_never_becomes_ready_ends_the_track_the_old_way_without_a_stall() {
    let f = files();
    let gate = Arc::new(AtomicBool::new(false));
    let (slow, g) = (f.b.clone(), gate.clone());
    let cfg = AudioThreadConfig {
        decoder_hook: Some(DecoderHook(Arc::new(
            move |p: &Path, d: Box<dyn BlockSource>| -> Box<dyn BlockSource> {
                if p == slow.as_path() {
                    Box::new(GatedBlocks::new(d, g.clone()))
                } else {
                    d
                }
            },
        ))),
        ..Default::default()
    };
    let mut rig = rig(cfg);
    rig.start_at(&f.a, true, 0);
    rig.run_writes(2);
    set_next(&rig, Some(src(&f.b, true)));
    let starved0 = rig.audio.stats.starved_blocks.load(Ordering::SeqCst);
    drive(&rig, |_| false);
    // The ordinary end of stream arrived (no hold at the boundary, no `NeedData` forever).
    assert!(finished(&rig), "the transport is stuck");
    assert_eq!(crossings(&rig), 0);
    assert_eq!(fallbacks(&rig), 1);
    assert_eq!(captured(&rig).len(), A_FRAMES, "nothing of B was joined");
    assert_eq!(
        rig.audio.stats.starved_blocks.load(Ordering::SeqCst),
        starved0,
        "a seamless attempt must never hold the audio"
    );
    gate.store(true, Ordering::SeqCst);
}

#[test]
fn off_unity_or_not_offered_as_next_the_seamless_path_is_not_entered() {
    let f = files();
    // 1.5x: the bypass voice is gone.
    let rig = playing(&f, true);
    rig.audio.try_send(AudioCommand::SetRate {
        permille: 1500,
        change: plippa_engine::RateChange::Immediate,
    });
    drive(&rig, |_| false);
    assert_eq!((crossings(&rig), fallbacks(&rig)), (0, 0), "1.5x");
    assert!(finished(&rig));
    // A neighbour the engine did not offer as the next at the end of the item (a scan may
    // use it, playback at 1.0x must not).
    let rig = playing(&f, false);
    drive(&rig, |_| false);
    assert_eq!((crossings(&rig), fallbacks(&rig)), (0, 0), "not seamless");
    assert!(finished(&rig));
    assert_eq!(captured(&rig).len(), A_FRAMES);
}

#[test]
fn a_neighbour_with_another_rate_falls_back_cleanly_and_silently() {
    let f = files();
    let odd = f._dir.path().join("odd.wav");
    write_pcm16_wav(&odd, 44_100, 1, 20_000, |_, _| 1000).unwrap();
    let rig = playing(&f, true);
    set_next(&rig, Some(src(&odd, true)));
    drive(&rig, |_| false);
    assert!(finished(&rig));
    assert_eq!(crossings(&rig), 0);
    assert_eq!(fallbacks(&rig), 1);
    // Silent: the control plane reports `cross_failures` as a "could not be opened" warning.
    assert_eq!(rig.audio.stats.cross_failures.load(Ordering::SeqCst), 0);
    assert_eq!(captured(&rig).len(), A_FRAMES);
}

#[test]
fn withdrawing_the_next_before_the_boundary_cancels_the_join() {
    let f = files();
    let rig = playing(&f, true);
    // Well inside the join zone, the head long since appended.
    drive(&rig, |rig| captured(rig).len() as u64 >= A_FRAMES as u64 - 6_000);
    std::thread::sleep(Duration::from_millis(50));
    // The user switched repeat-item on: the engine says nothing follows.
    set_next(&rig, None);
    drive(&rig, |_| false);
    assert!(finished(&rig));
    assert_eq!(crossings(&rig), 0);
    assert_eq!(captured(&rig).len(), A_FRAMES, "the track ended where it ends");
}

/// Pause `behind` frames after the boundary with `queue` frames still unheard, resume, and
/// check that what is heard from then on is exactly the stream from where the listener
/// stopped: no frame twice, none skipped. Returns the crossing directions seen.
fn pause_resume(behind: usize, queue: u64) -> Vec<i8> {
    let f = files();
    let rig = playing(&f, true);
    rig.set_sink_queue(queue);
    let stop_at = A_FRAMES + behind;
    drive(&rig, |rig| captured(rig).len() >= stop_at);
    // Pause while the thread is parked in a write: handled right after it.
    rig.audio.try_send(AudioCommand::Pause);
    rig.allowed.store(rig.writes.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
    wait_until("paused", || !rig.audio.stats.main_playing.load(Ordering::SeqCst));
    // Everything written so far was queued; `queue` of it was never heard and is put back.
    let at_pause = captured(&rig).len();
    rig.audio.try_send(AudioCommand::Play { not_before: None });
    rig.set_sink_queue(0);
    drive(&rig, |_| false);
    let out = captured(&rig);
    let resumed = &out[at_pause..];
    let want = &f.expected[(at_pause - queue as usize)..];
    assert_eq!(resumed.len(), want.len(), "frames after the resume");
    let worst = worst_diff(resumed, want);
    assert!(worst < 1e-6, "audio after the resume differs from the stream by {worst}");
    assert!(finished(&rig));
    events(&rig).iter().map(|e| e.dir).collect()
}

#[test]
fn pause_past_the_boundary_resumes_on_the_next_track_at_the_frame_the_listener_stopped() {
    // 9000 frames into B, 5000 unheard: the listener stopped 4000 frames into B.
    assert_eq!(pause_resume(9_000, 5_000), [1]);
}

#[test]
fn pause_just_past_the_boundary_resumes_in_the_tail_of_the_previous_track_and_joins_again() {
    // 2000 frames into B, 5000 unheard: the listener is still 3000 frames before the end of A.
    // The item flips back at the pause and forward again when the join is re-entered.
    assert_eq!(pause_resume(2_000, 5_000), [1, -1, 1]);
}

#[test]
fn a_seek_inside_the_join_settles_it_and_lands_in_the_item_the_engine_means() {
    let f = files();
    let mut rig = playing(&f, true);
    drive(&rig, |rig| captured(rig).len() >= A_FRAMES + 3_000);
    assert_eq!(crossings(&rig), 1);
    // Back to 500 ms of B (the engine's current item since the flip).
    rig.seek_parked(500);
    assert_eq!(rig.frames(), 24_000);
    let before = captured(&rig).len();
    rig.run_writes(6);
    let out = captured(&rig);
    let got = &out[before..];
    let want = &f.expected[A_FRAMES + 24_000..];
    // (the first blocks cross-fade from what was queued)
    assert!(got.len() >= 4_096);
    let worst = worst_diff(&got[2_048..4_096], &want[2_048..4_096]);
    assert!(worst < 1e-6, "audio after the seek differs from B by {worst}");
    // Played on to the end of B, no restart, no second crossing.
    drive(&rig, |_| false);
    assert!(finished(&rig));
    assert_eq!(crossings(&rig), 1);
    assert_eq!(fallbacks(&rig), 0);
}

#[test]
fn a_track_never_joins_into_itself() {
    let f = files();
    let rig = playing(&f, true);
    // (a repeat-item or one-item list: the engine does not send this, the audio thread's own
    // guard is the second line)
    set_next(&rig, Some(src(&f.a, true)));
    drive(&rig, |_| false);
    assert!(finished(&rig));
    assert_eq!(crossings(&rig), 0);
    assert_eq!(captured(&rig).len(), A_FRAMES, "A ended once, there was no second A");
}

#[test]
fn offering_the_same_next_again_inside_the_join_leaves_it_alone() {
    let f = files();
    let rig = playing(&f, true);
    drive(&rig, |rig| captured(rig).len() as u64 >= A_FRAMES as u64 - 6_000);
    std::thread::sleep(Duration::from_millis(50));
    // The engine re-sends its neighbours (a status refresh, the same list): nothing changes.
    set_next(&rig, Some(src(&f.b, true)));
    drive(&rig, |_| false);
    let out = captured(&rig);
    assert_eq!(out.len(), f.expected.len());
    assert!(worst_diff(&out, &f.expected) < 1e-6);
    assert_eq!(crossings(&rig), 1);
}

#[test]
fn a_neighbour_offered_only_for_seamless_playback_is_not_crossed_by_a_scan() {
    // "Seek outside track" off: the engine still offers the next item for seamless playback,
    // but a hold scan must end with the track as it always did (WARP_19's off behaviour).
    let f = files();
    let rig = playing(&f, true);
    rig.audio.try_send(AudioCommand::ScanStart {
        dir: plippa_core::ScanDir::Forward,
        mode: WarpMode::PitchShift,
    });
    drive(&rig, |_| false);
    assert!(finished(&rig));
    assert_eq!(crossings(&rig), 0);
    assert_eq!(rig.audio.stats.cross_failures.load(Ordering::SeqCst), 0);
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/plippa-audio/tests/fixtures")
        .join(name)
}

/// Two copies' worth of real files, joined at 1.0x, equal the two decodes back to back.
fn joins_exactly(first: &str, second: &str) {
    let dir = tempfile::tempdir().unwrap();
    let ext = |n: &str| n.rsplit('.').next().unwrap().to_string();
    let a = dir.path().join(format!("first.{}", ext(first)));
    let b = dir.path().join(format!("second.{}", ext(second)));
    std::fs::copy(fixture(first), &a).unwrap();
    std::fs::copy(fixture(second), &b).unwrap();
    let mut expected = plippa_audio::decode(&a).unwrap().samples;
    expected.extend(plippa_audio::decode(&b).unwrap().samples);
    let mut rig = rig(AudioThreadConfig::default());
    rig.start_at(&a, true, 0);
    rig.run_writes(2);
    set_next(&rig, Some(src(&b, true)));
    // These files are a quarter of a second long: give the neighbour's helper thread a few
    // milliseconds per block, as a real sink would (0.25 s of audio takes 250 ms to play).
    drive(&rig, |_| {
        std::thread::sleep(Duration::from_millis(10));
        false
    });
    assert!(finished(&rig), "{first} -> {second}: never ended");
    let out = captured(&rig);
    assert_eq!(out.len(), expected.len(), "{first} -> {second}: frames out");
    let worst = worst_diff(&out, &expected);
    assert!(worst < 1e-6, "{first} -> {second}: differs from the decodes by {worst}");
    assert_eq!(crossings(&rig), 1, "{first} -> {second}");
    assert_eq!(fallbacks(&rig), 0, "{first} -> {second}");
}

#[test]
fn real_formats_join_exactly_mp3_to_mp3_and_across_formats() {
    // Encoder delay and padding are trimmed from both MP3s, so the join has no silence in it.
    joins_exactly("tone_48k_3s.mp3", "tone_48k.mp3");
    joins_exactly("tone_48k_3s.mp3", "tone_48k_16.wav");
    joins_exactly("tone_48k.flac", "tone_48k.flac");
    joins_exactly("tone_48k_16.wav", "tone_48k.mp3");
    joins_exactly("tone_48k.flac", "tone_48k_3s.mp3");
}

/// Pause (the audio thread parked in a write handles it behind that write) and return.
fn pause(rig: &Rig) {
    rig.audio.try_send(AudioCommand::Pause);
    rig.allowed.store(rig.writes.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
    wait_until("paused", || !rig.audio.stats.main_playing.load(Ordering::SeqCst));
}

/// The command settles a join that the playhead is 9000 frames into B by an exchange of the
/// feeds. Afterwards the PUBLISHED heard position must be sane: no jump to 0, no overshoot, and
/// a second Pause must put back exactly what was unheard (sample-exact resume).
fn settled_by_swap(name: &str, second_pause: bool, command: impl Fn(&mut Rig)) {
    let f = files();
    let mut rig = playing(&f, true);
    rig.set_sink_queue(5_000);
    drive(&rig, |rig| captured(rig).len() >= A_FRAMES + 9_000);
    command(&mut rig);
    rig.audio.try_send(AudioCommand::Play { not_before: None });
    let mut series = Vec::new();
    for _ in 0..10 {
        step(&rig);
        series.push(rig.frames());
    }
    // (the first value may still be the one published before the command)
    for w in series[1..].windows(2) {
        assert!(
            w[1] >= w[0] && w[1] - w[0] <= 2 * 1024,
            "{name}: the published position jumped: {series:?}"
        );
    }
    assert!(
        series[3..].iter().all(|&v| v > 0),
        "{name}: the published position fell to 0: {series:?}"
    );
    if !second_pause {
        return;
    }
    // A second Pause: the put-back must reach the frame the listener stopped at.
    pause(&rig);
    let heard = rig.frames() as usize;
    assert!(heard > 0, "{name}: position after the second pause is {heard}");
    let at_pause = captured(&rig).len();
    rig.audio.try_send(AudioCommand::Play { not_before: None });
    rig.set_sink_queue(0);
    for _ in 0..4 {
        step(&rig);
    }
    let out = captured(&rig);
    assert!(out.len() >= at_pause + 3_072, "{name}: only {} frames after the pause at {at_pause} (heard {heard})", out.len());
    let got = &out[at_pause + 1_024..at_pause + 3_072];
    let want = &f.expected[A_FRAMES + heard + 1_024..A_FRAMES + heard + 3_072];
    assert!(
        worst_diff(got, want) < 1e-6,
        "{name}: after the second pause the audio is not the stream from the heard position {heard}"
    );
}

#[test]
fn the_heard_position_stays_sane_after_a_command_settles_the_join_by_a_swap() {
    settled_by_swap("pause", true, |rig| pause(rig));
    settled_by_swap("seek", true, |rig| {
        rig.seek_parked(500);
    });
    // A rate change off 1.0 (varispeed from here on): the position series alone.
    settled_by_swap("rate", false, |rig| {
        rig.audio.try_send(AudioCommand::SetRate {
            permille: 1500,
            change: plippa_engine::RateChange::Immediate,
        });
        step(rig);
    });
}
