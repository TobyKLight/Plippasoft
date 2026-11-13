//! NB_10: portal transport over the `nowPlayingList` (Next/Previous/jump, play-from-item/cue,
//! the now-playing summary and list) and the SEC-07 upload slot. The engine is the fake from
//! `tests/fake_engine`, which answers commands with the frames the real one publishes.

mod common;

mod fake_engine;

use plippa_ipc_proto::{IpcMessage, PlaybackCommand};
use plippa_server::{
    play_collection_unlocked, PlayCollection, RecordingIpc, ServerConfig, ServerService,
};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn kinds_library() -> (tempfile::TempDir, PathBuf) {
    fn copy_dir(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).unwrap();
        for e in fs::read_dir(src).unwrap() {
            let e = e.unwrap();
            let to = dst.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy_dir(&e.path(), &to);
            } else {
                fs::copy(e.path(), to).unwrap();
            }
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/plippa-noubin/tests/fixtures/libraries/kinds"),
        &root,
    );
    (tmp, root)
}

struct Rig {
    _tmp: tempfile::TempDir,
    svc: Arc<Mutex<ServerService>>,
    ipc: RecordingIpc,
}

fn rig(delay_ms: u64) -> Rig {
    let (tmp, root) = kinds_library();
    let ipc = RecordingIpc::new();
    let svc = Arc::new(Mutex::new(ServerService::start(
        Some(root),
        Box::new(ipc.clone()),
        ServerService::default_mock_associate(),
    )));
    fake_engine::spawn(
        ipc.clone(),
        Arc::clone(&svc),
        Duration::from_millis(delay_ms),
    );
    Rig {
        _tmp: tmp,
        svc,
        ipc,
    }
}

fn id_of(r: &Rig, title: &str) -> u64 {
    r.svc
        .lock()
        .unwrap()
        .list_collections(None)
        .unwrap()
        .into_iter()
        .find(|c| c.title == title)
        .unwrap_or_else(|| panic!("no {title}"))
        .id
}

fn commands(r: &Rig) -> Vec<PlaybackCommand> {
    r.ipc
        .messages()
        .into_iter()
        .filter_map(|m| match m {
            IpcMessage::PlaybackCommand(c) => Some(c),
            _ => None,
        })
        .collect()
}

fn wait_for<F: FnMut() -> bool>(mut f: F) {
    for _ in 0..400 {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not reached");
}

#[test]
fn play_whole_collection_is_load_collection() {
    let r = rig(0);
    let album = id_of(&r, "The Album");
    play_collection_unlocked(&r.svc, album, PlayCollection::default()).unwrap();
    assert!(matches!(
        commands(&r).as_slice(),
        [PlaybackCommand::LoadCollection { .. }]
    ));
}

#[test]
fn play_from_item_loads_then_jumps_to_that_track_row() {
    let r = rig(60); // the engine answers late: the wait must not race it
    let album = id_of(&r, "The Album");
    play_collection_unlocked(
        &r.svc,
        album,
        PlayCollection {
            item: Some(1),
            cue_ms: None,
        },
    )
    .unwrap();
    let cmds = commands(&r);
    assert!(
        matches!(cmds[0], PlaybackCommand::LoadAndPlay(_)),
        "{cmds:?}"
    );
    assert!(
        matches!(cmds[1], PlaybackCommand::JumpToEntry { index: 1 }),
        "{cmds:?}"
    );
    wait_for(|| r.svc.lock().unwrap().now_playing().track_index == Some(1));
    let np = r.svc.lock().unwrap().now_playing();
    assert_eq!(np.collection_title.as_deref(), Some("The Album"));
    assert_eq!(np.title, "Two");
    assert_eq!(
        (np.track_count, np.list_kind.as_deref()),
        (2, Some("album"))
    );
    assert!(np.can_prev && np.can_next);
}

#[test]
fn play_from_cue_seeks_after_loading_and_reuses_a_loaded_collection() {
    let r = rig(20);
    let mix = id_of(&r, "Rocksteady Set");
    play_collection_unlocked(
        &r.svc,
        mix,
        PlayCollection {
            item: Some(0),
            cue_ms: Some(600_000),
        },
    )
    .unwrap();
    let cmds = commands(&r);
    assert!(
        matches!(cmds[0], PlaybackCommand::LoadAndPlay(_)),
        "{cmds:?}"
    );
    // The mix is one track: already the current row, so no jump — just the seek.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, PlaybackCommand::SeekMs(600_000))),
        "{cmds:?}"
    );
    wait_for(|| r.svc.lock().unwrap().now_playing().cue_title.as_deref() == Some("Peak time"));

    // Second cue click while that mix is loaded: no reload.
    let before = commands(&r).len();
    play_collection_unlocked(
        &r.svc,
        mix,
        PlayCollection {
            item: Some(0),
            cue_ms: Some(2_700_000),
        },
    )
    .unwrap();
    let cmds = commands(&r);
    assert!(
        !cmds[before..].iter().any(|c| matches!(
            c,
            PlaybackCommand::LoadAndPlay(_) | PlaybackCommand::LoadCollection { .. }
        )),
        "{cmds:?}"
    );
    wait_for(|| r.svc.lock().unwrap().now_playing().cue_title.as_deref() == Some("Cool down"));
}

#[test]
fn play_from_unknown_item_is_a_404_and_sends_nothing() {
    let r = rig(0);
    let album = id_of(&r, "The Album");
    let e = play_collection_unlocked(
        &r.svc,
        album,
        PlayCollection {
            item: Some(99),
            cue_ms: None,
        },
    )
    .unwrap_err();
    assert!(matches!(e, plippa_server::ServerError::NotFound(_)), "{e}");
    assert!(commands(&r).is_empty());
}

#[test]
fn now_playing_list_and_jump_validation() {
    let r = rig(0);
    let album = id_of(&r, "The Album");
    assert!(r.svc.lock().unwrap().now_playing_list().entries.is_empty());
    assert!(
        r.svc.lock().unwrap().jump_to_entry(0).is_err(),
        "no list yet"
    );
    play_collection_unlocked(&r.svc, album, PlayCollection::default()).unwrap();
    wait_for(|| !r.svc.lock().unwrap().now_playing_list().entries.is_empty());
    let l = r.svc.lock().unwrap().now_playing_list();
    assert_eq!(l.entries.len(), 2);
    assert_eq!(l.entries[0].kind, "track");
    assert!(l.seq >= 1);
    assert!(r.svc.lock().unwrap().jump_to_entry(1).is_ok());
    assert!(r.svc.lock().unwrap().jump_to_entry(2).is_err());
    let np_seq = r.svc.lock().unwrap().now_playing().list_seq;
    assert_eq!(np_seq, l.seq);
}

// ---- HTTP ---------------------------------------------------------------------------

fn start_http(r: &Rig) -> u16 {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        library_root: None,
        secrets_dir: None,
        portal_url: None,
        ..ServerConfig::default()
    };
    let svc = Arc::clone(&r.svc);
    common::serve_open(cfg, svc);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return port;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{extra}Content-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap();
    s.write_all(body).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

const JSON: (&str, &str) = ("Content-Type", "application/json");

#[test]
fn http_transport_and_play_endpoints() {
    let r = rig(0);
    let port = start_http(&r);
    let album = id_of(&r, "The Album");

    // Same CSRF rules as every state change (NBSecurityRequirements §5.3).
    let (st, _) = http(
        port,
        "POST",
        "/api/transport/next",
        &[("Content-Type", "text/plain")],
        b"",
    );
    assert_eq!(st, 415);
    assert!(commands(&r).is_empty());

    let (st, body) = http(
        port,
        "POST",
        &format!("/api/collections/{album}/play"),
        &[("X-Plippa", "1")],
        b"",
    );
    assert_eq!(st, 200, "{body}");
    wait_for(|| !r.svc.lock().unwrap().now_playing_list().entries.is_empty());

    let (st, body) = http(port, "GET", "/api/now-playing/list", &[], b"");
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["entries"].as_array().unwrap().len(), 2);
    assert_eq!(v["list_kind"], "album");

    let (st, body) = http(port, "GET", "/api/now-playing", &[], b"");
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["collection_title"], "The Album");
    assert_eq!(v["track_index"], 0);
    assert_eq!(v["track_count"], 2);

    for p in ["next", "previous"] {
        let (st, _) = http(
            port,
            "POST",
            &format!("/api/transport/{p}"),
            &[("X-Plippa", "1")],
            b"",
        );
        assert_eq!(st, 200);
    }
    let (st, _) = http(
        port,
        "POST",
        "/api/transport/jump",
        &[JSON],
        br#"{"index":1}"#,
    );
    assert_eq!(st, 200);
    let (st, _) = http(
        port,
        "POST",
        "/api/transport/jump",
        &[JSON],
        br#"{"index":77}"#,
    );
    assert_eq!(st, 400);
    let (st, _) = http(
        port,
        "POST",
        "/api/transport/jump",
        &[JSON],
        br#"{"index":"x"}"#,
    );
    assert_eq!(st, 400);
    let (st, _) = http(port, "POST", "/api/collections/abc/play", &[JSON], b"{}");
    assert_eq!(st, 400);
    let (st, _) = http(
        port,
        "POST",
        &format!("/api/collections/{album}/play"),
        &[JSON],
        br#"{"item":99}"#,
    );
    assert_eq!(st, 404);
    let cmds = commands(&r);
    assert!(cmds.iter().any(|c| matches!(c, PlaybackCommand::Next)));
    assert!(cmds.iter().any(|c| matches!(c, PlaybackCommand::Previous)));
    assert!(cmds
        .iter()
        .any(|c| matches!(c, PlaybackCommand::JumpToEntry { index: 1 })));
}

/// SEC-07: one quick import at a time; the second is refused with 429 until the first ends.
#[test]
fn sec07_only_one_upload_in_flight() {
    let r = rig(0);
    let port = start_http(&r);
    let url = "/api/media?kind=music&filename=x.mp3";

    // First upload: headers + half the body, then stall.
    let mut slow = TcpStream::connect(("127.0.0.1", port)).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    write!(
        slow,
        "POST {url} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nX-Plippa: 1\r\nContent-Type: application/octet-stream\r\nContent-Length: 1048576\r\n\r\n"
    )
    .unwrap();
    // (tiny_http hands the request over once part of the body has arrived)
    slow.write_all(&vec![0u8; 524_288]).unwrap();
    slow.flush().unwrap();
    // Wait (bounded) until the server reports the slot taken: the first upload is now inside
    // read_body. Probing with requests instead would race: a probe arriving first takes the
    // slot and the *first* upload is the one refused.
    let hub = r.svc.lock().unwrap().imports();
    let mut waited = 0;
    while !hub.upload_in_flight() {
        waited += 1;
        assert!(waited < 2000, "first upload never took the slot");
        std::thread::sleep(Duration::from_millis(5));
    }
    // The slot stays taken until we send the rest, so this is refused every time.
    let (st, body) = http(port, "POST", url, &[("X-Plippa", "1")], b"whatever");
    assert_eq!(st, 429, "{body}");

    // Finish the first: junk bytes are not audio -> 415, and the slot is released.
    slow.write_all(&vec![0u8; 524_288]).unwrap();
    let mut out = Vec::new();
    let _ = slow.read_to_end(&mut out);
    assert!(
        String::from_utf8_lossy(&out).contains(" 415 "),
        "{}",
        String::from_utf8_lossy(&out)
    );
    let (st, _) = http(port, "POST", url, &[("X-Plippa", "1")], b"whatever");
    assert_eq!(st, 415);
}

/// POLISH_02: the portal shows the collection line the engine composed for the playing item
/// ("Band Name Bangers (Live In New York)"); a loose track (empty engine line) keeps the
/// record-title fallback.
#[test]
fn now_playing_carries_the_engine_composed_collection_line() {
    use plippa_core::{PlayableItemId, SessionState, TransportState};
    use plippa_ipc_proto::PlaybackStatus;
    let r = rig(0);
    let album = id_of(&r, "The Album");
    let inject = |rev: u64, line: &str| {
        r.ipc.inject_playback(PlaybackStatus::from_session_rev(
            SessionState {
                transport: TransportState::Playing,
                current_item: Some(PlayableItemId::new(album)),
                title: "Live In New York".into(),
                collection_title: line.into(),
                ..SessionState::default()
            },
            rev,
        ));
    };
    inject(1, "Band Name Bangers (Live In New York)");
    assert_eq!(
        r.svc.lock().unwrap().now_playing().collection_title.as_deref(),
        Some("Band Name Bangers (Live In New York)")
    );
    inject(2, "Band Name Bangers");
    assert_eq!(
        r.svc.lock().unwrap().now_playing().collection_title.as_deref(),
        Some("Band Name Bangers"),
        "follows the playing item back to a plain track"
    );
    inject(3, "");
    assert_eq!(
        r.svc.lock().unwrap().now_playing().collection_title.as_deref(),
        Some("The Album")
    );
}

/// A loose track has no collection line: the server no longer repeats its own title.
#[test]
fn a_loose_track_has_no_collection_line() {
    use plippa_core::{PlayableItemId, SessionState, TransportState};
    use plippa_ipc_proto::PlaybackStatus;
    let r = rig(0);
    let loose = r
        .svc
        .lock()
        .unwrap()
        .list_tracks(plippa_noubin::LibraryKind::Track)
        .unwrap()
        .into_iter()
        .find(|t| t.title.contains("orphan song"))
        .expect("the loose fixture track")
        .id;
    r.ipc.inject_playback(PlaybackStatus::from_session_rev(
        SessionState {
            transport: TransportState::Playing,
            current_item: Some(PlayableItemId::new(loose)),
            title: "orphan song".into(),
            collection_title: String::new(),
            ..SessionState::default()
        },
        1,
    ));
    assert_eq!(r.svc.lock().unwrap().now_playing().collection_title, None);
}
