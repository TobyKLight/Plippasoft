//! MVP_10 live-runtime tests (mock sink, no hardware).

use plippa_audio::PcmBuffer;
use plippa_core::{Capabilities, TransportState};
use plippa_engine::{
    run_hub, Engine, EngineConfig, EngineEvent, HubShutdown, SinkMode,
};
use plippa_ipc_proto::{
    IpcConnection, IpcMessage, IpcPaths, PlaybackCommand, TapKeyPayload, NOTICE_NO_MEDIA_STORAGE,
};
use plippa_noubin::{normalise_noubin_key, LibraryRoot, LibraryWriter};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn audio_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/plippa-audio/tests/fixtures")
        .join(name)
}

fn one_track_library(key: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    fs::copy(audio_fixture("tone_48k_16.wav"), root.join("tone.wav")).unwrap();
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

fn wait_frames(engine: &Engine, min: u64, timeout: Duration) {
    let start = Instant::now();
    while engine.frames_written() < min {
        assert!(
            start.elapsed() < timeout,
            "timed out waiting for {min} frames (have {})",
            engine.frames_written()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn missing_index_surfaces_notice_without_abort() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    // Empty directory — no `.plippa` index.
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    assert!(!engine.has_library_index());
    let events = engine.take_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            EngineEvent::Notice(n) if n.code == NOTICE_NO_MEDIA_STORAGE
        )),
        "expected no_media_storage notice, got {events:?}"
    );

    // Tap while index missing → notice, stay Stopped.
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: "ghost.noubin".into(),
        tap_id: 1,
        already_normalised: false,
    }));
    let events = engine.take_events();
    assert!(events.iter().any(|e| matches!(e, EngineEvent::Notice(_))));
    assert_eq!(engine.session().transport, TransportState::Stopped);
}

#[test]
fn sfx_triggered_on_successful_load() {
    let key = "sfx.noubin.mvp10";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.set_load_sfx(PcmBuffer {
        samples: vec![0.5; 480], // 10ms @ 48k stereo-ish mono fill; mixer expands by channels
        channels: 2,
        sample_rate_hz: 48_000,
    });
    engine.clear_mock_capture();
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));
    wait_frames(&engine, 64, Duration::from_secs(2));
    assert_eq!(engine.session().transport, TransportState::Playing);
    // SFX + main should produce non-silent capture.
    let cap = engine.mock_capture_snapshot();
    assert!(
        cap.iter().any(|s| s.abs() > 0.01),
        "expected audible mock capture after SFX+main"
    );
}

#[test]
fn shutdown_audio_does_not_hang() {
    let key = "stop.noubin.mvp10";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));
    wait_frames(&engine, 16, Duration::from_secs(2));
    let start = Instant::now();
    engine.shutdown_audio();
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "shutdown_audio hung"
    );
    assert_eq!(engine.frames_written(), 0);
}

#[test]
fn hub_shutdown_flag_exits_cleanly() {
    let key = "hubstop.noubin.mvp10";
    let (_tmp, root) = one_track_library(key);
    let ipc_tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(ipc_tmp.path());
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    let shutdown: HubShutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);
    let paths_hub = paths.clone();
    let hub = thread::spawn(move || {
        run_hub(&mut engine, &paths_hub, None, Some(flag)).unwrap();
    });
    thread::sleep(Duration::from_millis(50));
    shutdown.store(true, Ordering::SeqCst);
    let joined = hub.join();
    assert!(joined.is_ok(), "hub join failed");
}

#[test]
fn live_ipc_tap_reaches_playing_on_mock() {
    let key = "live.noubin.mvp10";
    let (_lib, root) = one_track_library(key);
    let ipc_tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(ipc_tmp.path());
    let mut engine = Engine::open_mock(&root, Default::default()).unwrap();
    let paths_hub = paths.clone();
    let hub = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(6);
        run_hub(&mut engine, &paths_hub, Some(deadline), None).unwrap();
    });

    let mut client =
        IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(3)).unwrap();
    client
        .send(&IpcMessage::TapKey(TapKeyPayload {
            noubin_key: key.into(),
            tap_id: 1,
            already_normalised: false,
        }))
        .unwrap();

    let mut saw_playing = false;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !saw_playing {
        if let Some(msg) = client.recv_timeout(Duration::from_millis(200)).unwrap() {
            if let IpcMessage::PlaybackStatus(s) = msg {
                if s.transport == TransportState::Playing
                    || s.session.transport == TransportState::Playing
                {
                    saw_playing = true;
                }
            }
        }
    }
    assert!(saw_playing, "TapKey over UDS must reach Playing");
    hub.join().unwrap();
}

#[test]
fn tap_to_first_audio_under_one_second_mock() {
    let key = "timing.noubin.mvp10";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    let t0 = Instant::now();
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));
    wait_frames(&engine, 1, Duration::from_secs(2));
    let elapsed = t0.elapsed();
    let _ = engine.poll_tap_to_audio_metric();
    assert!(
        elapsed < Duration::from_millis(1000),
        "tap→first-audio {elapsed:?} exceeds Noubin §3.2.1 1000ms (mock)"
    );
}

#[test]
fn volume_ipc_changes_mock_gain() {
    let key = "vol.noubin.mvp10";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(
        plippa_core::Volume::new(10),
    )));
    let low = engine.audio_volume_linear();
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(
        plippa_core::Volume::new(90),
    )));
    let high = engine.audio_volume_linear();
    assert!(high > low, "volume IPC must change mixer gain ({low} → {high})");
}

#[test]
fn config_parse_library_and_sink() {
    let cfg = EngineConfig::from_args([
        "plippa-engine".into(),
        "--library-root".into(),
        "/tmp/fixture".into(),
        "--sink=mock".into(),
    ]);
    assert_eq!(cfg.library_root, Some(PathBuf::from("/tmp/fixture")));
    assert_eq!(cfg.sink, SinkMode::Mock);
}

#[cfg(feature = "pulse")]
#[test]
#[ignore = "requires a live PipeWire server"]
fn pulse_sink_opens_without_panic() {
    // Documented CI skip: only run when Pulse is available.
    let tmp = tempfile::tempdir().unwrap();
    let result = plippa_audio::PulseSink::new(2, 48_000);
    match result {
        Ok(sink) => {
            let engine = Engine::open_with_sink(tmp.path(), Capabilities::desktop_mock(), sink);
            assert!(engine.is_ok());
            if let Ok(mut e) = engine {
                e.shutdown_audio();
            }
        }
        Err(e) => panic!("Pulse unavailable: {e}"),
    }
}
