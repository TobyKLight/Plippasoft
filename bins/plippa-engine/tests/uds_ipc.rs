//! UDS integration: client ↔ engine hub without hardware.

use plippa_audio::route::{MockOutputControl, SinkInfo};
use plippa_core::{TransportState, Volume};
use plippa_engine::{run_hub, Engine, RouteManager, RouteTarget};
use plippa_ipc_proto::{
    IpcConnection, IpcMessage, IpcPaths, LibraryIndexUpdated, PlaybackCommand, TapKeyPayload,
};
use plippa_noubin::{normalise_noubin_key, LibraryRoot, LibraryWriter};
use std::fs;
use std::path::{Path, PathBuf};
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

#[test]
fn uds_tap_key_round_trip_playback_status() {
    let key = "Tone.noubin.UDS";
    let (_lib_tmp, root) = one_track_library(key);
    let ipc_tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(ipc_tmp.path());

    let mut engine = Engine::open_mock(&root, Default::default()).unwrap();
    let paths_hub = paths.clone();
    let hub = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        run_hub(&mut engine, &paths_hub, Some(deadline), None).unwrap();
    });

    let mut client =
        IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(3)).unwrap();

    client
        .send(&IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(
            Volume::new(40),
        )))
        .unwrap();

    // Hub may push an initial PlaybackStatus on connect; wait for volume=40.
    let mut saw_vol = false;
    let deadline_vol = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline_vol && !saw_vol {
        if let Some(msg) = client.recv_timeout(Duration::from_millis(200)).unwrap() {
            if let IpcMessage::PlaybackStatus(s) = msg {
                if s.session.volume.percent() == 40 {
                    saw_vol = true;
                }
            }
        }
    }
    assert!(saw_vol, "expected PlaybackStatus volume=40 after SetVolume");

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
    assert!(saw_playing, "expected Playing status over UDS");

    // Second client acts as server: LibraryIndexUpdated must reach the engine
    // (no panic; optional status may follow). Hub stays up until deadline.
    let mut server =
        IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(2)).unwrap();
    server
        .send(&IpcMessage::LibraryIndexUpdated(LibraryIndexUpdated {
            generation: 9,
            item_count: 1,
        }))
        .unwrap();
    thread::sleep(Duration::from_millis(100));

    hub.join().unwrap();
}

#[test]
fn hub_stays_responsive_during_load() {
    // After LoadAndPlay, hub must still accept commands / push status quickly
    // (no multi-second block on the dispatch loop).
    let key = "Tone.noubin.HubResp";
    let (_lib_tmp, root) = one_track_library(key);
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

    // Immediately poke the hub; response must arrive well under a second.
    let t0 = Instant::now();
    client
        .send(&IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(
            Volume::new(55),
        )))
        .unwrap();

    let mut saw = false;
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline && !saw {
        if let Some(msg) = client.recv_timeout(Duration::from_millis(50)).unwrap() {
            if let IpcMessage::PlaybackStatus(s) = msg {
                if s.session.volume.percent() == 55 {
                    saw = true;
                }
            }
        }
    }
    assert!(
        saw,
        "hub must answer SetVolume during load within 500ms (took {:?})",
        t0.elapsed()
    );
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "hub turn too slow: {:?}",
        t0.elapsed()
    );

    hub.join().unwrap();
}

#[test]
fn connect_succeeds_after_delayed_bind() {
    let ipc_tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(ipc_tmp.path());
    let sock = paths.engine_sock.clone();

    let client = thread::spawn(move || {
        // Start connecting before the hub binds.
        IpcConnection::connect_timeout(&sock, Duration::from_secs(5))
    });

    thread::sleep(Duration::from_millis(200));

    let lib = tempfile::tempdir().unwrap();
    let mut engine = Engine::open_mock(lib.path(), Default::default()).unwrap();
    let mock = MockOutputControl::new(vec![SinkInfo {
        id: "local.sink".into(),
        description: "Built-in output".into(),
        available: true,
        latency_ms: 0,
        props: Default::default(),
        is_default: true,
    }]);
    engine.attach_route(
        RouteManager::new(Box::new(mock), Some("local.sink".into()), None),
        RouteTarget::default_cell(),
    );
    let paths_hub = paths.clone();
    let hub = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(4);
        run_hub(&mut engine, &paths_hub, Some(deadline), None).unwrap();
    });

    let mut conn = client.join().unwrap().expect("connect after delayed bind");
    // The accept path publishes the route before any status, so a connecting
    // client always has the Audio screen's data first.
    let first = conn
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .expect("a message on connect");
    assert!(
        matches!(first, IpcMessage::AudioRouteChanged(_)),
        "first message must be the audio route, got {first:?}"
    );
    conn.send(&IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(
        Volume::new(10),
    )))
    .unwrap();

    hub.join().unwrap();
}

#[test]
fn load_and_play_then_stop_no_deadlock() {
    let key = "Tone.noubin.Supersede";
    let (_lib_tmp, root) = one_track_library(key);
    let ipc_tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(ipc_tmp.path());

    let mut engine = Engine::open_mock(&root, Default::default()).unwrap();
    let paths_hub = paths.clone();
    let hub = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
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
    client
        .send(&IpcMessage::PlaybackCommand(PlaybackCommand::Stop))
        .unwrap();

    let mut saw_stopped = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !saw_stopped {
        if let Some(msg) = client.recv_timeout(Duration::from_millis(100)).unwrap() {
            if let IpcMessage::PlaybackStatus(s) = msg {
                if s.transport == TransportState::Stopped {
                    saw_stopped = true;
                }
            }
        }
    }
    assert!(saw_stopped, "Stop after LoadAndPlay must complete without deadlock");

    hub.join().unwrap();
}

#[test]
fn uds_now_playing_list_reaches_clients_including_late_joiners() {
    use plippa_core::{NowPlayingEntryKind, NowPlayingListKind};
    let key = "Tone.noubin.NowPlaying";
    let (_lib_tmp, root) = one_track_library(key);
    let ipc_tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(ipc_tmp.path());

    let mut engine = Engine::open_mock(&root, Default::default()).unwrap();
    let paths_hub = paths.clone();
    let hub = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(6);
        run_hub(&mut engine, &paths_hub, Some(deadline), None).unwrap();
    });

    fn wait_list(c: &mut IpcConnection) -> plippa_ipc_proto::NowPlayingListChanged {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some(IpcMessage::NowPlayingListChanged(l)) =
                c.recv_timeout(Duration::from_millis(100)).unwrap()
            {
                return l;
            }
        }
        panic!("no NowPlayingListChanged received");
    }

    let mut first =
        IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(3)).unwrap();
    first
        .send(&IpcMessage::TapKey(TapKeyPayload {
            noubin_key: key.into(),
            tap_id: 1,
            already_normalised: false,
        }))
        .unwrap();
    let list = wait_list(&mut first);
    assert_eq!(list.list_kind, NowPlayingListKind::Single);
    assert_eq!(list.entries.len(), 1);
    assert_eq!(list.entries[0].kind, NowPlayingEntryKind::Track);
    assert_eq!(list.current_index, 0);

    // A client that connects afterwards gets the current list right away.
    let mut late =
        IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(2)).unwrap();
    assert_eq!(wait_list(&mut late), list);

    hub.join().unwrap();
}

#[test]
fn warp_commands_round_trip_over_uds_into_status() {
    use plippa_core::{ScanDir, ScanState, WarpMode};
    let key = "Tone.noubin.Warp";
    let (_lib_tmp, root) = one_track_library(key);
    let ipc_tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(ipc_tmp.path());
    let mut engine = Engine::open_mock(&root, Default::default()).unwrap();
    let paths_hub = paths.clone();
    let hub = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
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
    for c in [
        PlaybackCommand::SetRate { permille: 1500 },
        PlaybackCommand::SetRate { permille: 0 }, // rejected, must not change anything
        PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchCorrect),
        PlaybackCommand::SetSeekWarpMode(WarpMode::PitchCorrect),
        PlaybackCommand::ScanStart { dir: ScanDir::Backward },
    ] {
        client.send(&IpcMessage::PlaybackCommand(c)).unwrap();
    }
    let mut ok = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !ok {
        if let Some(IpcMessage::PlaybackStatus(s)) =
            client.recv_timeout(Duration::from_millis(100)).unwrap()
        {
            let x = &s.session;
            ok = x.target_rate_permille == 1500
                && x.speed_warp_mode == WarpMode::PitchCorrect
                && x.seek_warp_mode == WarpMode::PitchCorrect
                && x.scan == ScanState::Backward
                && x.rate_permille == 1000
                && !x.warp_active
                && x.can_ff
                && x.can_rw;
        }
    }
    assert!(ok, "expected the warp state in a PlaybackStatus over UDS");
    hub.join().unwrap();
}
