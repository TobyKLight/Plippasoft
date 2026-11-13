//! WARP_01 engine control-plane tests: set speed, warp modes, scan, `SeekTo`, `can_ff/can_rw`.
//!
//! No audio change and no wall-clock assertions: tests use `Load` (prepared, not playing) and
//! poll for *state* (seek landed), never for timing.

use plippa_audio::{AudioError, AudioSink};
use plippa_core::{Capabilities, Clock, ScanDir, ScanState, TransportState, VoiceKind, WarpMode};
use plippa_engine::{Engine, EngineEvent};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand, PlaybackStatus};
use plippa_noubin::{
    encode_playable_id, normalise_noubin_key, LibraryReader, LibraryRoot, LibraryWriter,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const RATE: u32 = 8_000;

/// Mono 16-bit WAV of `secs` seconds (small: 16 kB per second at 8 kHz).
fn wav(path: &Path, secs: f32) {
    use std::io::Write;
    let frames = (RATE as f32 * secs) as usize;
    let data_bytes = frames * 2;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut out = fs::File::create(path).unwrap();
    out.write_all(b"RIFF").unwrap();
    out.write_all(&(36 + data_bytes as u32).to_le_bytes())
        .unwrap();
    out.write_all(b"WAVEfmt ").unwrap();
    out.write_all(&16u32.to_le_bytes()).unwrap();
    out.write_all(&1u16.to_le_bytes()).unwrap();
    out.write_all(&1u16.to_le_bytes()).unwrap();
    out.write_all(&RATE.to_le_bytes()).unwrap();
    out.write_all(&(RATE * 2).to_le_bytes()).unwrap();
    out.write_all(&2u16.to_le_bytes()).unwrap();
    out.write_all(&16u16.to_le_bytes()).unwrap();
    out.write_all(b"data").unwrap();
    out.write_all(&(data_bytes as u32).to_le_bytes()).unwrap();
    for i in 0..frames {
        let s = ((i as f32 / RATE as f32 * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        out.write_all(&s.to_le_bytes()).unwrap();
    }
}

fn put(root: &Path, rel: &str, contents: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
}

fn scan(root: &Path) {
    LibraryWriter::open(&LibraryRoot::new(root))
        .unwrap()
        .full_scan()
        .unwrap();
}

/// Global (namespaced) playable id of the playable titled `title`.
fn gid(root: &Path, title: &str) -> plippa_core::PlayableItemId {
    let reader = LibraryReader::open(&LibraryRoot::new(root)).unwrap();
    let rec = reader
        .list_playables()
        .unwrap()
        .into_iter()
        .find(|p| p.title.as_deref() == Some(title))
        .unwrap_or_else(|| panic!("no playable titled {title}"));
    plippa_core::PlayableItemId::new(encode_playable_id(root, rec.id))
}

/// Injected monotonic clock (NB_04b): moves only when the test says so; `step_ms` is added
/// after every read (a large step = "every Previous press is far apart").
#[derive(Debug, Clone)]
struct TestClock {
    ms: Arc<AtomicU64>,
    step_ms: u64,
}

impl Clock for TestClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.ms.fetch_add(self.step_ms, Ordering::SeqCst))
    }
}

/// Engine whose clock never lets two Previous presses pair up (each read moves 60 s on).
fn engine(root: &Path) -> Engine {
    let mut e = Engine::open_mock(root, Capabilities::desktop_mock()).unwrap();
    e.set_clock(Box::new(TestClock {
        ms: Arc::new(AtomicU64::new(0)),
        step_ms: 60_000,
    }));
    e
}

/// Engine with a frozen clock plus a handle to advance it.
fn engine_ck(root: &Path) -> (Engine, Arc<AtomicU64>) {
    let ms = Arc::new(AtomicU64::new(1_000_000));
    let mut e = Engine::open_mock(root, Capabilities::desktop_mock()).unwrap();
    e.set_clock(Box::new(TestClock {
        ms: Arc::clone(&ms),
        step_ms: 0,
    }));
    (e, ms)
}

fn advance(ms: &Arc<AtomicU64>, by: u64) {
    ms.fetch_add(by, Ordering::SeqCst);
}

/// Mock sink paced at real time (the stock shared mock swallows a 20 s track at CPU speed, so
/// `position_ms` races to the end and never sits just past the 1 s restart threshold).
struct PacedSink {
    channels: u16,
    sample_rate_hz: u32,
}

impl AudioSink for PacedSink {
    fn channels(&self) -> u16 {
        self.channels
    }
    fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }
    fn write(&mut self, interleaved: &[f32]) -> Result<(), AudioError> {
        let frames = interleaved.len() / self.channels.max(1) as usize;
        thread::sleep(Duration::from_micros(
            frames as u64 * 1_000_000 / u64::from(self.sample_rate_hz),
        ));
        Ok(())
    }
}

/// Frozen injected clock + a real-time-paced sink, for tests that need a predictable
/// `position_ms` while also pairing Previous presses.
fn engine_paced(root: &Path) -> (Engine, Arc<AtomicU64>) {
    let ms = Arc::new(AtomicU64::new(1_000_000));
    let mut e = Engine::open_with_sink_factory(
        root,
        Capabilities::desktop_mock(),
        |channels, sample_rate_hz| {
            Ok(Box::new(PacedSink {
                channels,
                sample_rate_hz,
            }) as Box<dyn AudioSink>)
        },
    )
    .unwrap();
    e.set_clock(Box::new(TestClock {
        ms: Arc::clone(&ms),
        step_ms: 0,
    }));
    (e, ms)
}

fn cmd(e: &mut Engine, c: PlaybackCommand) {
    e.handle_ipc(IpcMessage::PlaybackCommand(c));
}

fn status(e: &mut Engine) -> PlaybackStatus {
    e.take_events();
    e.publish_status();
    e.take_events()
        .into_iter()
        .rev()
        .find_map(|ev| match ev {
            EngineEvent::Status(s) => Some(s),
            _ => None,
        })
        .expect("status")
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(15), "timed out: {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

/// Album with `n` tracks "T1".."Tn" of `secs` seconds; `extra` goes into `localPlaybackData`.
fn album(root: &Path, n: usize, secs: f32, profile: &str, key: &str, local: &str) {
    let mut items = Vec::new();
    for i in 1..=n {
        wav(&root.join(format!("Music/Album/{i:02}.wav")), secs);
        items.push(format!(
            r#"{{"mediaTitle":"T{i}","localFilePaths":["{i:02}.wav"]}}"#
        ));
    }
    put(
        root,
        "Music/Album/_album.noudata",
        &format!(
            r#"{{"noubinKey":"{key}","noubinKeyNormalised":"{}","playableItemTitle":"Album","playableItemProfile":"{profile}",
               "releaseData":{{"credits":{{"primaryArtists":[{{"primaryArtistName":"The Band"}}]}}}},
               {local}
               "mediaList":[{}]}}"#,
            normalise_noubin_key(key),
            items.join(",")
        ),
    );
    scan(root);
}

fn root() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().to_path_buf();
    (t, p)
}

fn loaded_album() -> (tempfile::TempDir, PathBuf, Engine) {
    let (t, r) = root();
    album(&r, 3, 20.0, "music", "album.noubin.a", "");
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    (t, r, e)
}

/// Poll the lifecycle (what the hub does each tick) until the committed seek has landed.
fn wait_seek_landed(e: &mut Engine) {
    wait_until("SeekTo landed", || {
        e.poll_stream_lifecycle();
        status(e).session.seek_target_ms.is_none()
    });
}

fn warp(e: &mut Engine) -> plippa_core::SessionState {
    status(e).session
}

#[test]
fn set_rate_stores_target_and_status_reports_the_voice() {
    let (_t, _r, mut e) = loaded_album();
    let rev = e.status_revision();
    cmd(&mut e, PlaybackCommand::SetRate { permille: 1500 });
    assert!(
        e.status_revision() > rev,
        "immediate status (revision bump)"
    );
    // The audio thread applies the command asynchronously.
    let t = std::time::Instant::now();
    while !e.audio_warp_active() {
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "audio thread never applied it");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let s = warp(&mut e);
    assert_eq!(s.target_rate_permille, 1500);
    // WARP_05/09: the audio thread applies it; the default mode is pitch correct, so the voice
    // is the stretcher for a positive rate and varispeed once pitch shift is chosen.
    assert!(s.warp_active);
    assert_eq!(s.speed_voice, VoiceKind::Stretch);
    assert!(e.wants_fast_status());
    cmd(&mut e, PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchShift));
    let t = std::time::Instant::now();
    while warp(&mut e).speed_voice != VoiceKind::Varispeed {
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "mode never applied");
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn set_rate_validation_rejects_without_state_change() {
    let (_t, _r, mut e) = loaded_album();
    cmd(&mut e, PlaybackCommand::SetRate { permille: 1250 });
    let rev = e.status_revision();
    for bad in [0, 99, -99, 2001, -2001, i32::MIN, i32::MAX] {
        cmd(&mut e, PlaybackCommand::SetRate { permille: bad });
        assert_eq!(e.status_revision(), rev, "{bad}: no status");
        assert_eq!(warp(&mut e).target_rate_permille, 1250, "{bad}: no change");
    }
    for ok in [-750, 100, 2000, -2000, -100] {
        cmd(&mut e, PlaybackCommand::SetRate { permille: ok });
        assert_eq!(warp(&mut e).target_rate_permille, ok);
    }
    // Same value again: no spurious revision bump.
    let rev = e.status_revision();
    cmd(&mut e, PlaybackCommand::SetRate { permille: -100 });
    assert_eq!(e.status_revision(), rev);
}

#[test]
fn reset_rate_edits_only_the_set_speed() {
    let (_t, _r, mut e) = loaded_album();
    cmd(
        &mut e,
        PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchCorrect),
    );
    cmd(
        &mut e,
        PlaybackCommand::SetSeekWarpMode(WarpMode::PitchCorrect),
    );
    cmd(&mut e, PlaybackCommand::SetRate { permille: 1500 });
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    cmd(&mut e, PlaybackCommand::ResetRate);
    let s = warp(&mut e);
    assert_eq!(s.target_rate_permille, 1000);
    assert_eq!(s.scan, ScanState::Forward, "scan untouched");
    assert_eq!(s.speed_warp_mode, WarpMode::PitchCorrect);
    assert_eq!(s.seek_warp_mode, WarpMode::PitchCorrect);
    assert!(e.wants_fast_status(), "scan keeps the fast cadence");
}

#[test]
fn scan_rate_edits_only_the_running_scan_speed() {
    // UI_15: the encoder's scan speed is a scan-only value. It is accepted while a scan runs,
    // rejected off the ladder, ignored without a scan, and never touches the set speed — so the
    // release restores the set speed, not the scan speed the encoder landed on.
    let (_t, _r, mut e) = loaded_album();
    cmd(&mut e, PlaybackCommand::SetRate { permille: 1500 });
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    let rev = e.status_revision();
    cmd(&mut e, PlaybackCommand::ScanRate { permille: 4000 });
    cmd(&mut e, PlaybackCommand::ScanRate { permille: 10 });
    cmd(&mut e, PlaybackCommand::ScanRate { permille: 50 });
    assert_eq!(
        e.status_revision(),
        rev,
        "a scan-speed change is not a status: the rate readout comes from the audio thread"
    );
    // Off the ladder: rejected (logged) and harmless.
    cmd(&mut e, PlaybackCommand::ScanRate { permille: 875 });
    cmd(&mut e, PlaybackCommand::ScanRate { permille: 0 });
    let s = warp(&mut e);
    assert_eq!(
        s.target_rate_permille, 1500,
        "the scan speed never leaks into the set speed"
    );
    assert_eq!(s.scan, ScanState::Forward, "the scan keeps running");
    assert_eq!(s.speed_warp_mode, plippa_core::SPEED_WARP_DEFAULT);
    // Release: the set speed is what comes back, not 400 % or 1 % of the scan speed.
    cmd(&mut e, PlaybackCommand::ScanStop);
    assert_eq!(warp(&mut e).scan, ScanState::None);
    wait_until("release settled at the set speed", || {
        e.poll_stream_lifecycle();
        warp(&mut e).rate_permille.abs_diff(1500) <= 1
    });
    let s = warp(&mut e);
    assert_eq!(s.target_rate_permille, 1500);
    // No scan: ignored, and no status either.
    let rev = e.status_revision();
    cmd(&mut e, PlaybackCommand::ScanRate { permille: 500 });
    assert_eq!(e.status_revision(), rev);
    assert_eq!(warp(&mut e).target_rate_permille, 1500);
}

#[test]
fn scan_stop_publishes_the_settled_position_and_speed_once() {
    // UI_15: no brake, so the release must not publish the pre-settle position and then move
    // it. The status comes once, from `poll_scan_end`, after the audio thread has settled.
    let (_t, _r, mut e) = loaded_album();
    cmd(&mut e, PlaybackCommand::SetRate { permille: 1500 });
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    cmd(&mut e, PlaybackCommand::ScanStop);
    e.take_events();
    let rev = e.status_revision();
    let none_now = e
        .take_events()
        .into_iter()
        .all(|ev| !matches!(ev, EngineEvent::Status(_)));
    assert!(none_now, "nothing published before the audio thread settles");
    assert_eq!(e.status_revision(), rev);
    wait_until("the settled status", || {
        e.poll_stream_lifecycle();
        e.status_revision() > rev
    });
    let statuses: Vec<PlaybackStatus> = e
        .take_events()
        .into_iter()
        .filter_map(|ev| match ev {
            EngineEvent::Status(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(statuses.len(), 1, "exactly one status for the release");
    let s = &statuses[0].session;
    assert_eq!(s.scan, ScanState::None);
    assert_eq!(s.target_rate_permille, 1500);
    assert_eq!(
        s.rate_permille, 1500,
        "the status carries the restored set speed, not the scan rate"
    );
    // No second status follows with a moved position.
    let pos = s.position_ms;
    let rev = e.status_revision();
    for _ in 0..10 {
        e.poll_stream_lifecycle();
    }
    assert_eq!(e.status_revision(), rev, "the settled position is final");
    assert_eq!(warp(&mut e).position_ms, pos);
}

#[test]
fn reset_rate_instant_drops_scan_and_seek_target() {
    // UI_14 §6: the FF/RW-hold + Play gesture. Unlike `ResetRate` (long-press Play), the
    // instant reset also clears the seek family so picture and audio are at the live position.
    let (_t, _r, mut e) = loaded_album();
    cmd(&mut e, PlaybackCommand::SetRate { permille: 1500 });
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    cmd(&mut e, PlaybackCommand::ResetRateInstant);
    let s = warp(&mut e);
    assert_eq!(s.target_rate_permille, 1000);
    assert_eq!(s.scan, ScanState::None, "the snap drops the scan");
    assert_eq!(s.seek_target_ms, None);

    // A committed seek target (no scan) is dropped too.
    cmd(&mut e, PlaybackCommand::SetRate { permille: 1500 });
    cmd(&mut e, PlaybackCommand::SeekTo { target_ms: 10_000 });
    assert!(warp(&mut e).seek_target_ms.is_some());
    cmd(&mut e, PlaybackCommand::ResetRateInstant);
    let s = warp(&mut e);
    assert_eq!(s.target_rate_permille, 1000);
    assert_eq!(s.seek_target_ms, None, "and any committed seek target");
}

#[test]
fn warp_modes_are_stored_independently() {
    let (_t, _r, mut e) = loaded_album();
    let d = warp(&mut e);
    assert_eq!(d.speed_warp_mode, plippa_core::SPEED_WARP_DEFAULT);
    assert_eq!(d.seek_warp_mode, WarpMode::PitchShift);
    cmd(
        &mut e,
        PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchCorrect),
    );
    let s = warp(&mut e);
    assert_eq!(s.speed_warp_mode, WarpMode::PitchCorrect);
    assert_eq!(s.seek_warp_mode, WarpMode::PitchShift);
    cmd(
        &mut e,
        PlaybackCommand::SetSeekWarpMode(WarpMode::PitchCorrect),
    );
    cmd(
        &mut e,
        PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchShift),
    );
    let s = warp(&mut e);
    assert_eq!(s.speed_warp_mode, WarpMode::PitchShift);
    assert_eq!(s.seek_warp_mode, WarpMode::PitchCorrect);
    // A mode change publishes immediately.
    let rev = e.status_revision();
    cmd(
        &mut e,
        PlaybackCommand::SetSeekWarpMode(WarpMode::PitchShift),
    );
    assert!(e.status_revision() > rev);
}

#[test]
fn unload_resets_tempo_track_change_keeps_it_and_clears_scan_and_target() {
    let (_t, r, mut e) = loaded_album();
    cmd(
        &mut e,
        PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchCorrect),
    );
    cmd(
        &mut e,
        PlaybackCommand::SetSeekWarpMode(WarpMode::PitchCorrect),
    );
    cmd(&mut e, PlaybackCommand::SetRate { permille: 750 });

    // Next (track change) while scanning: tempo + modes kept, scan cleared.
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Backward,
        },
    );
    cmd(&mut e, PlaybackCommand::Next);
    let s = warp(&mut e);
    assert_eq!(s.current_entry_index, 1);
    assert_eq!((s.target_rate_permille, s.scan), (750, ScanState::None));
    assert_eq!(
        (s.speed_warp_mode, s.seek_warp_mode),
        (WarpMode::PitchCorrect, WarpMode::PitchCorrect)
    );

    // Committed target cleared by a track change too.
    cmd(&mut e, PlaybackCommand::SeekTo { target_ms: 5_000 });
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(warp(&mut e).seek_target_ms, None);
    assert_eq!(warp(&mut e).target_rate_permille, 750);

    // A new load (NFC tap / LoadCollection) keeps tempo and modes, clears scan.
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    let s = warp(&mut e);
    assert_eq!((s.target_rate_permille, s.scan), (750, ScanState::None));
    assert_eq!(s.speed_warp_mode, WarpMode::PitchCorrect);

    // Unload: tempo back to 1000, scan/target cleared, modes survive.
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    cmd(&mut e, PlaybackCommand::Unload);
    let s = warp(&mut e);
    assert_eq!(s.target_rate_permille, 1000);
    assert_eq!((s.scan, s.seek_target_ms), (ScanState::None, None));
    assert_eq!(
        (s.speed_warp_mode, s.seek_warp_mode),
        (WarpMode::PitchCorrect, WarpMode::PitchCorrect)
    );
    assert!(!e.wants_fast_status());
}

#[test]
fn next_with_nothing_to_do_still_publishes_a_cleared_scan() {
    let (_t, _r, mut e) = loaded_album();
    cmd(&mut e, PlaybackCommand::Next);
    cmd(&mut e, PlaybackCommand::Next);
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    e.take_events();
    cmd(&mut e, PlaybackCommand::Next); // list end: navigation is a no-op
    let st = e
        .take_events()
        .into_iter()
        .rev()
        .find_map(|ev| match ev {
            EngineEvent::Status(s) => Some(s),
            _ => None,
        })
        .expect("status pushed because the scan was cleared");
    assert_eq!(st.session.scan, ScanState::None);
}

#[test]
fn seek_to_publishes_target_then_lands_with_actual_position() {
    let (_t, _r, mut e) = loaded_album();
    cmd(&mut e, PlaybackCommand::SeekTo { target_ms: 8_000 });
    // Deterministic: the target stays published until the lifecycle poll observes landing.
    assert_eq!(warp(&mut e).seek_target_ms, Some(8_000));
    assert!(e.wants_fast_status());
    wait_seek_landed(&mut e);
    let s = warp(&mut e);
    assert_eq!(s.seek_target_ms, None);
    assert_eq!(
        s.position_ms, 8_000,
        "paused hard seek lands exactly on the target"
    );
    assert!(!e.wants_fast_status());
}

#[test]
fn second_seek_to_replaces_the_first() {
    let (_t, _r, mut e) = loaded_album();
    cmd(&mut e, PlaybackCommand::SeekTo { target_ms: 3_000 });
    cmd(&mut e, PlaybackCommand::SeekTo { target_ms: 12_000 });
    assert_eq!(warp(&mut e).seek_target_ms, Some(12_000));
    wait_seek_landed(&mut e);
    assert_eq!(warp(&mut e).position_ms, 12_000);
}

#[test]
fn seek_to_clamps_to_duration_minus_guard() {
    let (_t, _r, mut e) = loaded_album();
    // Duration becomes known once the stream is open.
    wait_until("duration known", || {
        e.poll_stream_lifecycle();
        status(&mut e).session.duration_ms.is_some()
    });
    let dur = status(&mut e).session.duration_ms.unwrap();
    cmd(
        &mut e,
        PlaybackCommand::SeekTo {
            target_ms: dur + 60_000,
        },
    );
    assert_eq!(warp(&mut e).seek_target_ms, Some(dur - 250));
    wait_seek_landed(&mut e);
    assert_eq!(warp(&mut e).position_ms, dur - 250);
}

#[test]
fn seek_exclusivity_scan_blocks_seek_to_and_scan_start_clears_target() {
    let (_t, _r, mut e) = loaded_album();
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    let rev = e.status_revision();
    cmd(&mut e, PlaybackCommand::SeekTo { target_ms: 9_000 });
    assert_eq!(e.status_revision(), rev, "ignored: nothing published");
    assert_eq!(warp(&mut e).seek_target_ms, None);
    cmd(&mut e, PlaybackCommand::ScanStop);
    assert_eq!(warp(&mut e).scan, ScanState::None);

    cmd(&mut e, PlaybackCommand::SeekTo { target_ms: 9_000 });
    assert_eq!(warp(&mut e).seek_target_ms, Some(9_000));
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Backward,
        },
    );
    let s = warp(&mut e);
    assert_eq!((s.scan, s.seek_target_ms), (ScanState::Backward, None));
    // The stand-in seek may still land, but no target reappears.
    for _ in 0..5 {
        e.poll_stream_lifecycle();
    }
    assert_eq!(warp(&mut e).seek_target_ms, None);
    // Direction change while scanning updates the state.
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    assert_eq!(warp(&mut e).scan, ScanState::Forward);
}

#[test]
fn set_rate_and_modes_never_cancel_a_seek() {
    let (_t, _r, mut e) = loaded_album();
    cmd(&mut e, PlaybackCommand::SeekTo { target_ms: 7_000 });
    cmd(&mut e, PlaybackCommand::SetRate { permille: 1750 });
    cmd(&mut e, PlaybackCommand::ResetRate);
    cmd(
        &mut e,
        PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchCorrect),
    );
    cmd(
        &mut e,
        PlaybackCommand::SetSeekWarpMode(WarpMode::PitchCorrect),
    );
    assert_eq!(warp(&mut e).seek_target_ms, Some(7_000));
}

#[test]
fn seek_to_breaks_the_previous_window_but_set_rate_does_not() {
    let (_t, r) = {
        let (t, r) = root();
        album(&r, 3, 20.0, "music", "album.noubin.a", "");
        (t, r)
    };
    let (mut e, ck) = engine_paced(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    cmd(&mut e, PlaybackCommand::Next);
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(warp(&mut e).current_entry_index, 2);

    // FIX_01C: a *first* Previous restarts only once the playhead is >1 s past the entry
    // start. Let the paced sink reach that point so the press is observably a restart (entry
    // unchanged, playhead rewound) rather than "at the start, so step back a track".
    cmd(&mut e, PlaybackCommand::Play);
    wait_until("playhead past the 1 s restart threshold", || {
        e.poll_stream_lifecycle();
        warp(&mut e).position_ms > 1_500
    });
    cmd(&mut e, PlaybackCommand::Previous);
    let s = warp(&mut e);
    assert_eq!(s.current_entry_index, 2, "first Previous restarted entry 2");
    assert!(s.position_ms < 1_000, "restart rewound to the entry start");

    // Pair a second press with the first one (500 ms later on the injected clock) and run the
    // whole warp family in between. `Pause`/`Play` keep the pairing state (NB_04b), so the pair
    // survives long enough to get the playhead past the 1 s threshold again — which is what
    // makes the two outcomes distinguishable.
    cmd(&mut e, PlaybackCommand::Pause);
    advance(&ck, 500);
    for c in [
        PlaybackCommand::SetRate { permille: 1500 },
        PlaybackCommand::ResetRate,
        PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchCorrect),
        PlaybackCommand::SetSeekWarpMode(WarpMode::PitchCorrect),
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
        PlaybackCommand::ScanStop,
    ] {
        cmd(&mut e, c);
    }
    cmd(&mut e, PlaybackCommand::Play);
    wait_until("playhead past the 1 s threshold again", || {
        e.poll_stream_lifecycle();
        warp(&mut e).position_ms > 1_500
    });
    // Because the window survived, this is a *paired* press and it goes back a track. If any
    // warp command had cleared the tracker it would be a first press at >1 s and would only
    // restart entry 2 instead.
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(
        warp(&mut e).current_entry_index,
        1,
        "window survived the warp commands"
    );

    // A SeekTo *does* break the window (`SeekTo` is not in the retained set). After it the
    // next press is a first press; at 3 s that is a restart, so entry 1 must stay selected.
    cmd(&mut e, PlaybackCommand::SeekTo { target_ms: 3_000 });
    wait_until("SeekTo past the restart threshold", || {
        e.poll_stream_lifecycle();
        warp(&mut e).position_ms > 1_500
    });
    cmd(&mut e, PlaybackCommand::Previous);
    let s = warp(&mut e);
    assert_eq!(
        s.current_entry_index,
        1,
        "SeekTo cleared the tracker: restart, not another step back"
    );
    assert!(s.position_ms < 1_000, "restart rewound entry 1");
}

#[test]
fn can_ff_can_rw_follow_loaded_not_transport() {
    let (_t, r) = root();
    album(&r, 2, 20.0, "music", "album.noubin.a", "");
    let mut e = engine(&r);
    let s = warp(&mut e);
    assert!((s.can_ff, s.can_rw) == (false, false), "nothing loaded");
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    let s = warp(&mut e);
    assert_eq!(s.transport, TransportState::Stopped);
    assert!(s.can_ff && s.can_rw, "loaded, stopped");
    cmd(&mut e, PlaybackCommand::Play);
    let s = warp(&mut e);
    assert!(s.can_ff && s.can_rw, "playing");
    cmd(&mut e, PlaybackCommand::Pause);
    let s = warp(&mut e);
    assert_eq!(s.transport, TransportState::Paused);
    assert!(s.can_ff && s.can_rw, "paused");
    // Independent of tempo and position (last track, near the end).
    cmd(&mut e, PlaybackCommand::SetRate { permille: 2000 });
    cmd(&mut e, PlaybackCommand::Next);
    cmd(&mut e, PlaybackCommand::SeekMs(19_900));
    let s = warp(&mut e);
    assert!(s.can_ff && s.can_rw);
}

#[test]
fn engine_start_defaults() {
    let (_t, r) = root();
    album(&r, 1, 5.0, "music", "album.noubin.a", "");
    let mut e = engine(&r);
    let s = warp(&mut e);
    assert_eq!((s.rate_permille, s.target_rate_permille), (1000, 1000));
    assert_eq!(s.speed_warp_mode, plippa_core::SPEED_WARP_DEFAULT);
    assert_eq!(s.seek_warp_mode, plippa_core::SEEK_WARP_DEFAULT);
    assert_eq!((s.scan, s.seek_target_ms), (ScanState::None, None));
    assert!(!s.warp_active && !e.wants_fast_status());
    // Commands with nothing loaded are harmless.
    cmd(&mut e, PlaybackCommand::SeekTo { target_ms: 1_000 });
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    let s = warp(&mut e);
    assert_eq!((s.scan, s.seek_target_ms), (ScanState::None, None));
}
