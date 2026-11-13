//! Multi-peer UDS E2E (CI, no hardware): tap hit → Playing; tap miss → notice.
//!
//! Topology matches production: engine hub + UI peer + injector peer on the same
//! `engine.sock` (see `docs/design/ipc.md`). Package: `plippa` (`--test e2e_multiprocess`).

use plippa_core::TransportState;
use plippa_engine::{run_hub, Engine};
use plippa_ipc_proto::{
    IpcConnection, IpcMessage, IpcPaths, NOTICE_LIBRARY_MISS, TapKeyPayload,
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

fn spawn_hub(root: PathBuf, paths: IpcPaths) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut engine = Engine::open_mock(&root, Default::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        run_hub(&mut engine, &paths, Some(deadline), None).unwrap();
    })
}

#[test]
fn e2e_tap_hit_playing_across_peers() {
    let key = "Tone.noubin.E2EHit";
    let (_lib, root) = one_track_library(key);
    let ipc_tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(ipc_tmp.path());
    let hub = spawn_hub(root, paths.clone());

    let mut ui =
        IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(3)).unwrap();
    let mut injector =
        IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(3)).unwrap();

    injector
        .send(&IpcMessage::TapKey(TapKeyPayload {
            noubin_key: key.into(),
            tap_id: 1,
            already_normalised: false,
        }))
        .unwrap();

    let mut saw_playing = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !saw_playing {
        if let Ok(Some(IpcMessage::PlaybackStatus(s))) = ui.recv_timeout(Duration::from_millis(200))
        {
            if s.transport == TransportState::Playing
                || s.session.transport == TransportState::Playing
            {
                saw_playing = true;
            }
        }
    }
    assert!(saw_playing, "UI peer should observe Playing after tap hit");
    let _ = hub.join();
}

#[test]
fn e2e_tap_miss_user_notice_to_ui_peer() {
    let key = "Tone.noubin.E2EMissLib";
    let (_lib, root) = one_track_library(key);
    let ipc_tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(ipc_tmp.path());
    let hub = spawn_hub(root, paths.clone());

    let mut ui =
        IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(3)).unwrap();
    let mut injector =
        IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(3)).unwrap();

    injector
        .send(&IpcMessage::TapKey(TapKeyPayload {
            noubin_key: "TotallyUnknown.noubin.ZZZ".into(),
            tap_id: 2,
            already_normalised: false,
        }))
        .unwrap();

    let mut saw_miss = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !saw_miss {
        if let Ok(Some(IpcMessage::UserNotice(n))) = ui.recv_timeout(Duration::from_millis(200)) {
            if n.code == NOTICE_LIBRARY_MISS {
                saw_miss = true;
            }
        }
    }
    assert!(saw_miss, "UI peer should observe library_miss UserNotice");
    let _ = hub.join();
}
