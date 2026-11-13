//! Engine control plane tests (MVP_05).
//!
//! Uses tiny WAV fixtures + temp SQLite libraries — no real NFC / network.

use plippa_audio::PcmBuffer;
use plippa_core::{Capabilities, TransportState, Volume};
use plippa_engine::{Engine, EngineEvent};
use plippa_ipc_proto::{
    IpcMessage, LibraryIndexUpdated, PlaybackCommand, TapKeyPayload, TapNdefPayload,
    NOTICE_LIBRARY_MISS, NOTICE_MEDIA_FILE_MISSING, NOTICE_MEDIA_UNPLAYABLE,
};
use plippa_noubin::{LibraryRoot, LibraryWriter, normalise_noubin_key};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

fn audio_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/plippa-audio/tests/fixtures")
        .join(name)
}

fn text_ndef(text: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2); // lang len "en"
    payload.extend_from_slice(b"en");
    payload.extend_from_slice(text.as_bytes());
    let mut out = vec![0xD1, 0x01, payload.len() as u8, b'T'];
    out.extend_from_slice(&payload);
    out
}

/// Tiny one-track library with a known normalised key.
/// Uses a multi-second WAV so streaming tests can observe Play/Pause/volume
/// before EOF (short fixtures finish in ~250 ms).
fn one_track_library(key: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    write_tone_wav(&root.join("tone.wav"), 48_000, 2, 48_000 * 2); // 2 s
    let noudata = format!(
        r#"{{
          "noubinKey": "{key}",
          "noubinKeyNormalised": "{}",
          "playableItemTitle": "Tone",
          "mediaList": [{{"localFilePaths": ["tone.wav"]}}]
        }}"#,
        normalise_noubin_key(key)
    );
    fs::write(root.join("_tone.noudata"), noudata).unwrap();
    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    (tmp, root)
}

fn two_track_library(key: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    fs::copy(audio_fixture("tone_48k_16.wav"), root.join("a.wav")).unwrap();
    fs::copy(audio_fixture("tone_48k_16.wav"), root.join("b.wav")).unwrap();
    let noudata = format!(
        r#"{{
          "noubinKey": "{key}",
          "noubinKeyNormalised": "{}",
          "playableItemTitle": "Two",
          "mediaList": [
            {{"mediaTitle": "A", "localFilePaths": ["a.wav"]}},
            {{"mediaTitle": "B", "localFilePaths": ["b.wav"]}}
          ]
        }}"#,
        normalise_noubin_key(key)
    );
    fs::write(root.join("_two.noudata"), noudata).unwrap();
    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    (tmp, root)
}

fn missing_media_library(key: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let noudata = format!(
        r#"{{
          "noubinKey": "{key}",
          "noubinKeyNormalised": "{}",
          "playableItemTitle": "Ghost",
          "mediaList": [{{"localFilePaths": ["does-not-exist.wav"]}}]
        }}"#,
        normalise_noubin_key(key)
    );
    fs::write(root.join("_ghost.noudata"), noudata).unwrap();
    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    (tmp, root)
}

fn wait_frames(engine: &Engine, min: u64, timeout: Duration) {
    let start = std::time::Instant::now();
    while engine.frames_written() < min {
        assert!(
            start.elapsed() < timeout,
            "timed out waiting for {min} frames (have {})",
            engine.frames_written()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn last_status(events: &[EngineEvent]) -> Option<&plippa_ipc_proto::PlaybackStatus> {
    events.iter().rev().find_map(|e| match e {
        EngineEvent::Status(s) => Some(s),
        _ => None,
    })
}

fn last_notice_code(events: &[EngineEvent]) -> Option<&str> {
    events.iter().rev().find_map(|e| match e {
        EngineEvent::Notice(n) => Some(n.code.as_str()),
        _ => None,
    })
}

#[test]
fn engine_opens_db_as_wal_reader() {
    let (_tmp, root) = one_track_library("tone.noubin.mvp05");
    let engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    assert_eq!(
        engine.session().transport,
        TransportState::Stopped
    );
    // Journal mode already asserted inside open_mock.
    let _ = engine;
}

#[test]
fn tap_hit_starts_playing_with_expected_playable() {
    let key = "tone.noubin.mvp05hit";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));

    let events = engine.take_events();
    let status = last_status(&events).expect("status");
    assert_eq!(status.transport, TransportState::Playing);
    assert!(status.session.current_item.is_some());
    wait_frames(&engine, 64, Duration::from_secs(2));
}

#[test]
fn tap_miss_emits_error_stays_stopped() {
    let (_tmp, root) = one_track_library("present.noubin.x");
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: "missing.noubin.nowhere".into(),
        tap_id: 2,
        already_normalised: false,
    }));

    let events = engine.take_events();
    assert_eq!(last_notice_code(&events), Some(NOTICE_LIBRARY_MISS));
    assert_eq!(engine.session().transport, TransportState::Stopped);
    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::Notice(n) if n.message.contains("No local media")
    )));
}

#[test]
fn tap_hit_missing_file_emits_error_no_panic() {
    let key = "ghost.noubin.mvp05";
    let (_tmp, root) = missing_media_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 3,
        already_normalised: false,
    }));

    let events = engine.take_events();
    assert_eq!(last_notice_code(&events), Some(NOTICE_MEDIA_FILE_MISSING));
    assert_eq!(engine.session().transport, TransportState::Stopped);
}

/// A release whose one entry lists `paths`; the files in `present` are created (dummy bytes).
fn release_with_files(key: &str, paths: &[&str], present: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    for name in present {
        fs::write(root.join(name), b"not decoded: the gate refuses it first").unwrap();
    }
    let list = paths
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(",");
    let noudata = format!(
        r#"{{
          "noubinKey": "{key}",
          "noubinKeyNormalised": "{}",
          "playableItemTitle": "Release",
          "mediaList": [{{"localFilePaths": [{list}]}}]
        }}"#,
        normalise_noubin_key(key)
    );
    fs::write(root.join("_release.noudata"), noudata).unwrap();
    LibraryWriter::open(&LibraryRoot::new(&root)).unwrap().full_scan().unwrap();
    (tmp, root)
}

/// Tap `key` on a desktop (no AAC / Opus) engine and return the last notice `(code, message)`.
fn tap_and_notice(root: &Path, key: &str) -> (String, String) {
    let mut engine = Engine::open_mock(root, Capabilities::desktop_mock()).unwrap();
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));
    assert_eq!(engine.session().transport, TransportState::Stopped);
    engine
        .take_events()
        .into_iter()
        .rev()
        .find_map(|e| match e {
            EngineEvent::Notice(n) => Some((n.code, n.message)),
            _ => None,
        })
        .expect("a notice")
}

#[test]
fn an_unplayable_format_reports_media_unplayable() {
    let key = "take.noubin.media01";
    let (_tmp, root) = release_with_files(key, &["take.m4a"], &["take.m4a"]);
    let (code, message) = tap_and_notice(&root, key);
    assert_eq!(code, NOTICE_MEDIA_UNPLAYABLE);
    // Names the format and the board, and never claims the file is gone.
    assert!(message.contains("M4A/AAC") && message.contains("desktop-mock"), "{message}");
    assert!(!message.to_lowercase().contains("missing") && !message.contains("not found"), "{message}");
}

#[test]
fn a_missing_file_still_reports_media_file_missing() {
    // A missing file is "missing" whatever its format: nothing was there to refuse.
    for (key, name) in [("gone.noubin.wav01", "gone.wav"), ("gone.noubin.m4a01", "gone.m4a")] {
        let (_tmp, root) = release_with_files(key, &[name], &[]);
        let (code, message) = tap_and_notice(&root, key);
        assert_eq!(code, NOTICE_MEDIA_FILE_MISSING, "{name}: {message}");
    }
}

#[test]
fn a_release_with_no_playable_path_reports_media_unplayable() {
    let key = "alts.noubin.media01";
    let (_tmp, root) =
        release_with_files(key, &["alt.m4a", "alt.opus"], &["alt.m4a", "alt.opus"]);
    let (code, message) = tap_and_notice(&root, key);
    assert_eq!(code, NOTICE_MEDIA_UNPLAYABLE, "{message}");
    assert!(message.contains("desktop-mock"), "{message}");
}

#[test]
fn volume_ipc_affects_audio_gain() {
    let key = "vol.noubin.mvp05";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    // Start then immediately mute — mock sink drains ASAP, so mute before EOF.
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 4,
        already_normalised: false,
    }));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(
        Volume::new(0),
    )));
    assert_eq!(engine.session().volume.percent(), 0);
    assert_eq!(engine.audio_volume_linear(), 0.0);
    wait_frames(&engine, 64, Duration::from_secs(2));
    engine.clear_mock_capture();
    thread::sleep(Duration::from_millis(40));
    let silenced = engine.mock_capture_snapshot();
    assert!(
        !silenced.is_empty() && silenced.iter().all(|s| *s == 0.0),
        "volume 0 must write silence to mock sink (got {} samples)",
        silenced.len()
    );

    // Reload so we still have main-stream audio after the short fixture ends.
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Stop));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(
        Volume::new(80),
    )));
    assert_eq!(engine.session().volume.percent(), 80);
    assert!((engine.audio_volume_linear() - 0.8).abs() < f32::EPSILON);

    engine.clear_mock_capture();
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 40,
        already_normalised: false,
    }));
    let before = engine.frames_written();
    wait_frames(&engine, before + 64, Duration::from_secs(2));
    let loud = engine.mock_capture_snapshot();
    assert!(
        loud.iter().any(|s| s.abs() > 0.0),
        "volume 80 must produce non-silent mock sink output (got {} samples)",
        loud.len()
    );
}

#[test]
fn pause_stop_change_transport_and_halt_main_advancement() {
    let key = "pause.noubin.mvp05";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 5,
        already_normalised: false,
    }));
    wait_frames(&engine, 128, Duration::from_secs(2));

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Pause));
    assert_eq!(engine.session().transport, TransportState::Paused);
    // Allow the audio thread to observe Pause and finish any in-flight block.
    thread::sleep(Duration::from_millis(50));
    let at_pause = engine.frames_written();
    thread::sleep(Duration::from_millis(50));
    let after_pause = engine.frames_written();
    assert_eq!(
        at_pause, after_pause,
        "paused audio thread must not write main-stream frames"
    );

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Play));
    wait_frames(&engine, at_pause + 64, Duration::from_secs(2));

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Stop));
    assert_eq!(engine.session().transport, TransportState::Stopped);
    assert!(
        engine.session().current_item.is_some(),
        "Stop keeps the loaded item so Play can restart"
    );
    // Settle so Stop is applied before sampling the counter.
    thread::sleep(Duration::from_millis(50));
    let at_stop = engine.frames_written();
    thread::sleep(Duration::from_millis(50));
    assert_eq!(engine.frames_written(), at_stop);

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Play));
    wait_frames(&engine, at_stop + 64, Duration::from_secs(2));
    assert_eq!(engine.session().transport, TransportState::Playing);
}

#[test]
fn index_refresh_then_new_key_becomes_playable() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    fs::copy(audio_fixture("tone_48k_16.wav"), root.join("tone.wav")).unwrap();
    // Start with an empty-ish library (no playables yet — still need a scanned DB).
    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();

    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: "later.noubin.mvp05".into(),
        tap_id: 6,
        already_normalised: false,
    }));
    assert_eq!(
        last_notice_code(&engine.take_events()),
        Some(NOTICE_LIBRARY_MISS)
    );

    // Server-side write: add playable + rescan, then notify engine.
    let noudata = r#"{
      "noubinKey": "later.noubin.mvp05",
      "noubinKeyNormalised": "later.noubin.mvp05",
      "playableItemTitle": "Later",
      "mediaList": [{"localFilePaths": ["tone.wav"]}]
    }"#;
    fs::write(root.join("_later.noudata"), noudata).unwrap();
    LibraryWriter::open(&lib).unwrap().lazy_rescan().unwrap();

    engine.handle_ipc(IpcMessage::LibraryIndexUpdated(LibraryIndexUpdated {
        generation: 1,
        item_count: 1,
    }));

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: "later.noubin.mvp05".into(),
        tap_id: 7,
        already_normalised: true,
    }));
    let events = engine.take_events();
    assert_eq!(
        last_status(&events).map(|s| s.transport),
        Some(TransportState::Playing)
    );
}

#[test]
fn repeat_same_tag_while_playing_ignored() {
    let key = "repeat.noubin.mvp05";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 8,
        already_normalised: false,
    }));
    let item = engine.session().current_item;
    engine.take_events();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 9,
        already_normalised: false,
    }));
    // No new status / notice — ignore-repeat.
    assert!(engine.take_events().is_empty());
    assert_eq!(engine.session().current_item, item);
    assert_eq!(engine.session().transport, TransportState::Playing);
}

#[test]
fn audio_thread_keeps_advancing_during_control_sleep() {
    let key = "starve.noubin.mvp13";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    let control_tid = thread::current().id();

    for _ in 0..50 {
        if engine.audio_thread_id().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let audio_tid = engine.audio_thread_id().expect("audio thread id");
    assert_ne!(audio_tid, control_tid);

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 10,
        already_normalised: false,
    }));
    wait_frames(&engine, 64, Duration::from_secs(2));

    let before = engine.frames_written();
    engine.control_plane_sleep(Duration::from_millis(80));
    let after = engine.frames_written();
    assert!(
        after > before,
        "audio frames must advance during control-plane sleep ({before} → {after})"
    );
}

fn write_tone_wav(path: &Path, rate: u32, channels: u16, frames: usize) {
    use std::io::Write;
    let mut samples = Vec::with_capacity(frames * channels as usize);
    for i in 0..frames {
        let t = i as f32 / rate as f32;
        let s = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.25;
        let i16 = (s * 32767.0) as i16;
        for _ in 0..channels {
            samples.push(i16);
        }
    }
    let data_bytes = samples.len() * 2;
    let mut out = fs::File::create(path).unwrap();
    out.write_all(b"RIFF").unwrap();
    out.write_all(&(36 + data_bytes as u32).to_le_bytes()).unwrap();
    out.write_all(b"WAVEfmt ").unwrap();
    out.write_all(&16u32.to_le_bytes()).unwrap();
    out.write_all(&1u16.to_le_bytes()).unwrap();
    out.write_all(&channels.to_le_bytes()).unwrap();
    out.write_all(&rate.to_le_bytes()).unwrap();
    out.write_all(&(rate * u32::from(channels) * 2).to_le_bytes())
        .unwrap();
    out.write_all(&(channels * 2).to_le_bytes()).unwrap();
    out.write_all(&16u16.to_le_bytes()).unwrap();
    out.write_all(b"data").unwrap();
    out.write_all(&(data_bytes as u32).to_le_bytes()).unwrap();
    for s in samples {
        out.write_all(&s.to_le_bytes()).unwrap();
    }
    out.flush().unwrap();
}

#[test]
fn tap_path_has_no_network_client() {
    // Compile-time / dependency guarantee: plippa-engine does not depend on an HTTP stack.
    // Behavioural check: miss path only emits a local notice.
    let (_tmp, root) = one_track_library("net.noubin.mvp05");
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.handle_ipc(IpcMessage::TapNdef(TapNdefPayload {
        ndef_message: text_ndef("https://example.com/unknown-album/"),
        tap_id: 11,
    }));
    let events = engine.take_events();
    assert_eq!(last_notice_code(&events), Some(NOTICE_LIBRARY_MISS));
    // No panic, no HTTP — just a user-readable local miss.
}

#[test]
fn next_previous_track_within_media_list() {
    let key = "album.noubin.mvp05";
    let (_tmp, root) = two_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 12,
        already_normalised: false,
    }));
    assert_eq!(engine.session().transport, TransportState::Playing);

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::NextTrack));
    assert_eq!(engine.session().transport, TransportState::Playing);

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::PreviousTrack));
    assert_eq!(engine.session().transport, TransportState::Playing);
}

#[test]
fn load_sfx_can_be_queued_on_successful_tap() {
    let key = "sfx.noubin.mvp05";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.set_load_sfx(PcmBuffer {
        samples: vec![0.25, 0.25, -0.25, -0.25],
        channels: 2,
        sample_rate_hz: 48_000,
    });
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 13,
        already_normalised: false,
    }));
    wait_frames(&engine, 32, Duration::from_secs(2));
    assert_eq!(engine.session().transport, TransportState::Playing);
}

#[test]
fn frames_counter_is_on_audio_stats() {
    let key = "stats.noubin.mvp05";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 14,
        already_normalised: false,
    }));
    wait_frames(&engine, 16, Duration::from_secs(2));
    assert!(engine.frames_written() > 0);
    assert!(engine
        .audio_thread_id()
        .is_some_and(|_| true));
    let _ = Ordering::Relaxed; // keep import used if optimised away elsewhere
}

#[test]
fn stream_opens_at_fixture_sample_rate_not_forced_48k_bank() {
    let key = "native.noubin.mvp13";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 15,
        already_normalised: false,
    }));
    assert_eq!(engine.session().transport, TransportState::Playing);
    wait_frames(&engine, 32, Duration::from_secs(2));
    assert_eq!(engine.stream_sample_rate_hz(), 48_000);
    assert!(
        engine.mock_open_rates().contains(&48_000),
        "mock track stream must open at file rate, got {:?}",
        engine.mock_open_rates()
    );
}

#[test]
fn sfx_stream_can_use_distinct_native_rate() {
    let key = "sfxrate.noubin.mvp13";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    // SFX at 44.1 kHz must not require resample-to-48 k on the Play path.
    engine.set_load_sfx(PcmBuffer {
        samples: vec![0.2, 0.2, -0.2, -0.2],
        channels: 2,
        sample_rate_hz: 44_100,
    });
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 16,
        already_normalised: false,
    }));
    wait_frames(&engine, 16, Duration::from_secs(2));
    let rates = engine.mock_open_rates();
    assert!(
        rates.iter().any(|r| *r == 44_100),
        "SFX must open a native-rate stream (44.1k), got {rates:?}"
    );
    assert!(
        rates.iter().any(|r| *r == 48_000),
        "track must open at fixture rate 48k, got {rates:?}"
    );
}

#[test]
fn tap_resume_after_stop_seeks_with_sfx() {
    let key = "resume.noubin.mvp";
    // 30s track so a mid-point bookmark is not inside the 15s restart tail.
    let (_tmp, root) = timed_track_library(key, 30);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.set_load_sfx(PcmBuffer {
        samples: vec![0.5; 8],
        channels: 2,
        sample_rate_hz: 44_100,
    });

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 20,
        already_normalised: false,
    }));
    // Advance ~0.5s so the bookmark is mid-track.
    wait_frames(&engine, 24_000, Duration::from_secs(3));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Stop));

    let norm = normalise_noubin_key(key);
    let book = engine
        .resume_entry(&norm)
        .cloned()
        .expect("bookmark after stop");
    assert!(book.position_ms >= 400, "pos={}", book.position_ms);
    assert!(!book.should_restart());
    assert!(!book.finished);

    let rates_before = engine.mock_open_rates().len();
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 21,
        already_normalised: false,
    }));
    // SFX cover holds main ~1s. `wait_frames` counts frames since the engine opened (already
    // past 64 from the first tap), so wait for the resumed track's own stream to open.
    let wait = Instant::now();
    while engine.mock_open_rates()[rates_before..].iter().all(|r| *r != 48_000)
        && wait.elapsed() < Duration::from_secs(5)
    {
        thread::sleep(Duration::from_millis(10));
    }

    // Every tap (including resume) plays SFX at its native rate.
    let rates = engine.mock_open_rates();
    let new_rates = &rates[rates_before..];
    assert!(
        new_rates.iter().any(|r| *r == 44_100),
        "resume tap must open SFX stream, new rates={new_rates:?}"
    );
    assert!(
        new_rates.iter().any(|r| *r == 48_000),
        "resume tap must open track stream, new rates={new_rates:?}"
    );
    let pos = engine
        .current_position_ms()
        .expect("position after resume");
    assert!(
        pos + 200 >= book.position_ms,
        "expected resume near {}ms, got ~{pos}ms",
        book.position_ms
    );
}

#[test]
fn same_noubin_while_paused_resumes_with_sfx() {
    let key = "pausetap.noubin.mvp";
    let (_tmp, root) = timed_track_library(key, 30);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.set_load_sfx(PcmBuffer {
        samples: vec![0.4; 8],
        channels: 2,
        sample_rate_hz: 44_100,
    });

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 30,
        already_normalised: false,
    }));
    wait_frames(&engine, 8_000, Duration::from_secs(3));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Pause));
    assert_eq!(engine.session().transport, TransportState::Paused);
    thread::sleep(Duration::from_millis(40));
    let at_pause = engine.frames_written();

    let rates_before = engine.mock_open_rates().len();
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 31,
        already_normalised: false,
    }));
    assert_eq!(engine.session().transport, TransportState::Playing);
    wait_frames(&engine, at_pause + 64, Duration::from_secs(3));
    let rates = engine.mock_open_rates();
    assert!(
        rates[rates_before..].iter().any(|r| *r == 44_100),
        "paused same-noubin tap must fire SFX"
    );
}

#[test]
fn pause_play_after_resume_keeps_advancing() {
    let key = "resumepp.noubin.mvp";
    let (_tmp, root) = timed_track_library(key, 30);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 40,
        already_normalised: false,
    }));
    wait_frames(&engine, 8_000, Duration::from_secs(3));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Stop));

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 41,
        already_normalised: false,
    }));
    wait_frames(&engine, 64, Duration::from_secs(3));

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Pause));
    thread::sleep(Duration::from_millis(50));
    let at_pause = engine.frames_written();
    thread::sleep(Duration::from_millis(50));
    assert_eq!(
        engine.frames_written(),
        at_pause,
        "pause after resume must halt frames"
    );

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Play));
    wait_frames(&engine, at_pause + 64, Duration::from_secs(3));
    assert_eq!(engine.session().transport, TransportState::Playing);
}

#[test]
fn load_and_play_ignores_resume_bookmark() {
    let key = "resumeui.noubin.mvp";
    let (_tmp, root) = timed_track_library(key, 30);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 22,
        already_normalised: false,
    }));
    wait_frames(&engine, 24_000, Duration::from_secs(3));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Stop));
    let id = engine.session().current_item.expect("item");

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::LoadAndPlay(
        id,
    )));
    for _ in 0..50 {
        if engine.stream_sample_rate_hz() == 48_000 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    wait_frames(&engine, 64, Duration::from_secs(2));
    let pos = engine.current_position_ms().unwrap_or(u64::MAX);
    assert!(
        pos < 500,
        "LoadAndPlay must start near 0 (got {pos}ms), resume is tap-only"
    );
}

#[test]
fn unload_clears_session_resume_bookmarks() {
    let key = "unload.noubin.mvp";
    let (_tmp, root) = timed_track_library(key, 30);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 30,
        already_normalised: false,
    }));
    wait_frames(&engine, 24_000, Duration::from_secs(3));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Stop));

    let norm = normalise_noubin_key(key);
    assert!(
        engine.resume_entry(&norm).is_some(),
        "Stop should leave a resume bookmark"
    );

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Unload));
    assert_eq!(engine.session().transport, TransportState::Stopped);
    assert!(
        engine.resume_entry(&norm).is_none(),
        "Unload must clear session resume bookmarks"
    );

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 31,
        already_normalised: false,
    }));
    wait_frames(&engine, 64, Duration::from_secs(2));
    let pos = engine.current_position_ms().unwrap_or(u64::MAX);
    assert!(
        pos < 500,
        "tap after Unload must start near 0 (got {pos}ms)"
    );
}

#[test]
fn status_revision_bumps_on_transport_change() {
    let key = "rev.noubin.mvp";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    let rev0 = engine.status_revision();
    assert_eq!(rev0, 0);

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));
    let rev1 = engine.status_revision();
    assert!(rev1 > rev0, "tap/play must bump revision");
    let events = engine.take_events();
    let status = last_status(&events).expect("status");
    assert_eq!(status.revision, rev1);
    assert_eq!(status.transport, TransportState::Playing);

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Pause));
    let rev2 = engine.status_revision();
    assert!(rev2 > rev1, "pause must bump revision");
    assert_eq!(engine.session().transport, TransportState::Paused);
}

#[test]
fn seek_ms_moves_reported_position_and_status_metadata() {
    let key = "seek.noubin.mvp";
    let (_tmp, root) = one_track_library(key); // ~2s WAV, title "Tone"
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 40,
        already_normalised: false,
    }));
    wait_frames(&engine, 64, Duration::from_secs(2));
    // Allow duration to land from the audio thread probe.
    for _ in 0..50 {
        if engine.current_duration_ms().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SeekMs(800)));
    for _ in 0..50 {
        let pos = engine.current_position_ms().unwrap_or(0);
        if (700..=1_100).contains(&pos) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let pos = engine.current_position_ms().unwrap_or(u64::MAX);
    assert!(
        (700..=1_100).contains(&pos),
        "SeekMs(800) should move reported position near 800ms, got {pos}"
    );

    engine.publish_status();
    let events = engine.take_events();
    let status = last_status(&events).expect("status after seek");
    assert_eq!(status.session.title, "Tone");
    assert!(
        (700..=1_100).contains(&status.session.position_ms),
        "status position_ms={}, expected ~800",
        status.session.position_ms
    );
    assert!(
        status.session.duration_ms.is_some_and(|d| d >= 1_500),
        "expected duration for 2s fixture, got {:?}",
        status.session.duration_ms
    );

    // Clamp past end.
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SeekMs(
        9_999_999,
    )));
    thread::sleep(Duration::from_millis(40));
    engine.publish_status();
    let events = engine.take_events();
    let status = last_status(&events).expect("clamped seek status");
    let dur = status.session.duration_ms.expect("duration");
    assert!(
        status.session.position_ms <= dur,
        "seek past end must clamp to duration ({dur}), got {}",
        status.session.position_ms
    );
}

#[test]
fn failed_stream_open_rolls_back_playing() {
    use plippa_audio::AudioError;
    use std::sync::atomic::{AtomicUsize, Ordering as AtOrd};
    use std::sync::Arc;

    let key = "failopen.noubin.mvp";
    let (_tmp, root) = one_track_library(key);
    let opens = Arc::new(AtomicUsize::new(0));
    let opens_f = Arc::clone(&opens);
    let mut engine = Engine::open_with_sink_factory(
        &root,
        Capabilities::desktop_mock(),
        move |_ch, _rate| {
            opens_f.fetch_add(1, AtOrd::SeqCst);
            Err(AudioError::Sink("forced open failure".into()))
        },
    )
    .unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));
    // Control plane may briefly claim Playing before audio fails.
    assert_eq!(engine.session().transport, TransportState::Playing);

    let start = std::time::Instant::now();
    loop {
        engine.poll_stream_lifecycle();
        if engine.session().transport == TransportState::Stopped {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "expected rollback to Stopped after stream open failure"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        opens.load(AtOrd::SeqCst) >= 1,
        "factory must have been called"
    );
    let events = engine.take_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            EngineEvent::Notice(n) if n.code == plippa_ipc_proto::NOTICE_DECODE_FAILED
                || n.code == NOTICE_MEDIA_FILE_MISSING
        )) || last_status(&events).is_some_and(|s| s.transport == TransportState::Stopped),
        "expected decode notice or Stopped status, got {events:?}"
    );
}

#[test]
fn rapid_pause_play_stays_consistent() {
    let key = "rapid.noubin.mvp";
    let (_tmp, root) = timed_track_library(key, 30);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));
    wait_frames(&engine, 8_000, Duration::from_secs(3));

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Pause));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Play));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Pause));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Play));

    // Let audio drain the command queue, then reconcile.
    for _ in 0..30 {
        engine.poll_stream_lifecycle();
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        engine.session().transport,
        TransportState::Playing,
        "final Play must leave session Playing"
    );
    let before = engine.frames_written();
    wait_frames(&engine, before + 64, Duration::from_secs(3));
}

fn timed_track_library(key: &str, secs: u32) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    write_tone_wav(
        &root.join("tone.wav"),
        48_000,
        2,
        48_000 * secs as usize,
    );
    let noudata = format!(
        r#"{{
          "noubinKey": "{key}",
          "noubinKeyNormalised": "{}",
          "playableItemTitle": "Tone",
          "mediaList": [{{"localFilePaths": ["tone.wav"]}}]
        }}"#,
        normalise_noubin_key(key)
    );
    fs::write(root.join("_tone.noudata"), noudata).unwrap();
    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    (tmp, root)
}

/// UI_20 (owner bug): tapping a different noubin must not move the playhead before the audio
/// starts. The tap arms the main stream behind the SFX cover, and the transport is Playing from
/// the moment of the tap, so the status sent right then is what a dead-reckoning UI sees: it
/// used to carry the **previous** stream's rate (the audio thread had not taken the start yet),
/// and the picture ran a status interval of false playback before snapping back.
#[test]
fn a_tap_publishes_a_still_playhead_through_the_sfx_cover() {
    let (_tmp, root) = timed_track_library("still.a", 30);
    fs::write(
        root.join("_second.noudata"),
        format!(
            r#"{{"noubinKey": "still.b", "noubinKeyNormalised": "{}", "playableItemTitle": "B",
            "mediaList": [{{"localFilePaths": ["tone.wav"]}}]}}"#,
            normalise_noubin_key("still.b")
        ),
    )
    .unwrap();
    LibraryWriter::open(&LibraryRoot::new(&root)).unwrap().full_scan().unwrap();
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.set_load_sfx(PcmBuffer {
        samples: vec![0.5; 8],
        channels: 2,
        sample_rate_hz: 44_100,
    });
    // Track A playing, so the tap is a real item change at a non-zero rate.
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: "still.a".into(),
        tap_id: 1,
        already_normalised: false,
    }));
    wait_frames(&engine, 24_000, Duration::from_secs(3));

    let tapped = std::time::Instant::now();
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: "still.b".into(),
        tap_id: 2,
        already_normalised: false,
    }));
    // The status the UI sees first, before the audio thread has touched the new stream.
    engine.publish_status();
    let first = engine
        .take_events()
        .into_iter()
        .rev()
        .find_map(|e| match e {
            EngineEvent::Status(s) => Some(s),
            _ => None,
        })
        .expect("status after tap");
    assert_eq!(first.session.transport, TransportState::Playing);
    assert_eq!(
        first.session.rate_permille, 0,
        "the playhead is not moving during the SFX cover"
    );
    // ... and it stays put for the whole cover, then plays from the anchored position.
    let before = first.session.position_ms;
    let t = std::time::Instant::now();
    while t.elapsed() < Duration::from_millis(300) {
        engine.publish_status();
        for s in engine.take_events() {
            if let EngineEvent::Status(s) = s {
                // Measured from the tap: a starved test thread must not outlive the 1s cover.
                if t.elapsed() < Duration::from_millis(250) && tapped.elapsed() < Duration::from_millis(700) {
                    assert_eq!(s.session.rate_permille, 0, "still armed");
                    assert_eq!(s.session.position_ms, before, "the playhead must not move");
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    wait_frames(&engine, 48_000, Duration::from_secs(4));
    engine.publish_status();
    let played = engine
        .take_events()
        .into_iter()
        .rev()
        .find_map(|e| match e {
            EngineEvent::Status(s) => Some(s),
            _ => None,
        })
        .expect("status while playing");
    assert_eq!(played.session.rate_permille, 1_000, "the audio is running");
    assert!(
        played.session.position_ms > before,
        "playback moved on: {} vs {before}",
        played.session.position_ms
    );
}
