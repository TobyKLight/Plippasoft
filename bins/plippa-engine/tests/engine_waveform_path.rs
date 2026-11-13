//! WAVE_01: the engine publishes the loaded item's `.wfm` sidecar path.
//!
//! Tiny WAV fixtures + a temp SQLite library, mock sink (no real audio device).

use plippa_core::Capabilities;
use plippa_engine::{Engine, EngineEvent};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand, TapKeyPayload};
use plippa_noubin::{normalise_noubin_key, waveform_path, LibraryRoot, LibraryWriter};
use std::fs;
use std::path::{Path, PathBuf};

fn audio_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/plippa-audio/tests/fixtures")
        .join(name)
}

/// Two-track library (`a.wav`, `b.wav`) plus its known key.
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
    LibraryWriter::open(&LibraryRoot::new(&root))
        .unwrap()
        .full_scan()
        .unwrap();
    (tmp, root)
}

fn tap(engine: &mut Engine, key: &str) {
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));
}

fn last_status(events: &[EngineEvent]) -> &plippa_ipc_proto::PlaybackStatus {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            EngineEvent::Status(s) => Some(s),
            _ => None,
        })
        .expect("status")
}

fn expected(root: &Path, rel: &str) -> String {
    waveform_path(root, rel).display().to_string()
}

#[test]
fn waveform_path_is_none_before_anything_is_loaded() {
    let (_tmp, root) = two_track_library("wave.noubin.engine.none");
    let engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    assert_eq!(engine.session().waveform_path, None);
}

#[test]
fn waveform_path_follows_the_loaded_track_and_next() {
    let key = "wave.noubin.engine.next";
    let (_tmp, root) = two_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    tap(&mut engine, key);
    let events = engine.take_events();
    assert_eq!(
        last_status(&events).session.waveform_path.as_deref(),
        Some(expected(&root, "a.wav").as_str())
    );

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::NextTrack));
    let events = engine.take_events();
    assert_eq!(
        last_status(&events).session.waveform_path.as_deref(),
        Some(expected(&root, "b.wav").as_str()),
        "Next must publish the new item's sidecar"
    );
}

#[test]
fn unload_clears_the_waveform_path_until_the_next_load() {
    let key = "wave.noubin.engine.unload";
    let (_tmp, root) = two_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();

    tap(&mut engine, key);
    assert!(engine.session().waveform_path.is_some());

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Unload));
    let events = engine.take_events();
    assert_eq!(last_status(&events).session.waveform_path, None);
    assert_eq!(engine.session().waveform_path, None);

    // Play restarts the retained item; the path comes back on the resulting status.
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Play));
    let events = engine.take_events();
    assert_eq!(
        last_status(&events).session.waveform_path.as_deref(),
        Some(expected(&root, "a.wav").as_str())
    );
}

#[test]
fn a_missing_media_item_has_no_waveform_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let key = "wave.noubin.engine.missing";
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
    LibraryWriter::open(&LibraryRoot::new(&root))
        .unwrap()
        .full_scan()
        .unwrap();

    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    tap(&mut engine, key);
    // The tap fails (no media), so nothing is loaded and there is no sidecar path.
    assert_eq!(engine.session().waveform_path, None);
}
