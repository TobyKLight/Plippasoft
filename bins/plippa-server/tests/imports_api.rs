//! NB_11: streamed `POST /api/media` (`add_media_*`) and the multi-file import protocol
//! `/api/imports` (`imports_*`): album / playlist creation in the chosen order, partial failure,
//! rollback, and "a killed upload leaves nothing".

mod common;

use plippa_noubin::parse_noudata_with_warnings;
use plippa_server::{RecordingIpc, ServerConfig, ServerService};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn library() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    // Not an empty directory (git-less fixture; the scanner is happy with an empty one too).
    (tmp, root)
}

fn start_http(root: &Path) -> u16 {
    start_http_svc(root).0
}

fn start_http_svc(root: &Path) -> (u16, Arc<Mutex<ServerService>>) {
    start_http_board(root, plippa_core::Capabilities::desktop_mock())
}

/// The portal on a board with these capabilities (MEDIA_01).
fn start_http_board(
    root: &Path,
    caps: plippa_core::Capabilities,
) -> (u16, Arc<Mutex<ServerService>>) {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut service = ServerService::start(
        Some(root.to_path_buf()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    service.set_capabilities(caps);
    let svc = Arc::new(Mutex::new(service));
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        library_root: Some(root.to_path_buf()),
        secrets_dir: None,
        portal_url: None,
        ..ServerConfig::default()
    };
    let served = Arc::clone(&svc);
    common::serve_open(cfg, served);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (port, svc);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

/// `(status, json body or Null)`.
fn req(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, Value) {
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
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no response: {:?}", String::from_utf8_lossy(&raw)));
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let v = serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null);
    (status, v)
}

const X: (&str, &str) = ("X-Plippa", "1");
const JSON: (&str, &str) = ("Content-Type", "application/json");

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'.' || b == b'-' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// A valid mono 8-bit WAV of `secs` seconds (8 kHz); `tag` varies the bytes so files differ.
fn wav(secs: u32, tag: u8) -> Vec<u8> {
    let rate = 8000u32;
    let data = vec![tag; (rate * secs) as usize];
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&8u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

/// Every regular file under `dir` (relative, sorted), `.plippa` excluded.
fn files(dir: &Path) -> Vec<String> {
    fn walk(d: &Path, root: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.file_name().is_some_and(|n| n == ".plippa") {
                continue;
            }
            if p.is_dir() {
                walk(&p, root, out);
            } else {
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, dir, &mut v);
    v.sort();
    v
}

/// Files left in `.plippa/tmp` (staging + temp parts), recursively.
fn tmp_files(root: &Path) -> Vec<String> {
    fn walk(d: &Path, out: &mut Vec<String>) {
        if let Ok(rd) = fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push(p.display().to_string());
                }
            }
        }
    }
    let mut v = Vec::new();
    walk(&root.join(".plippa/tmp"), &mut v);
    v
}

fn media_names(root: &Path, rel_noudata: &str) -> Vec<String> {
    let bytes = fs::read(root.join(rel_noudata)).unwrap();
    let (doc, warnings) = parse_noudata_with_warnings(&bytes).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    doc.media_list
        .iter()
        .map(|m| m.local_file_paths.as_ref().unwrap()[0].clone())
        .collect()
}

fn begin(port: u16) -> String {
    let (st, v) = req(port, "POST", "/api/imports", &[X], b"");
    assert_eq!(st, 200, "{v}");
    v["id"].as_str().unwrap().to_string()
}

/// Upload `name` -> file_id.
fn upload(port: u16, sid: &str, name: &str, body: &[u8]) -> (u16, Value) {
    req(
        port,
        "POST",
        &format!("/api/imports/{sid}/files?name={}", pct(name)),
        &[X],
        body,
    )
}

fn finish(port: u16, sid: &str, mode: &str, name: &str, order: &[u64]) -> (u16, Value) {
    let body = json!({"mode": mode, "name": name, "order": order}).to_string();
    req(
        port,
        "POST",
        &format!("/api/imports/{sid}/finish"),
        &[JSON],
        body.as_bytes(),
    )
}

/// Upload 8 tracks `t1..t8` (distinct bytes), returning their file ids.
fn upload_eight(port: u16, sid: &str) -> Vec<u64> {
    (1..=8u8)
        .map(|i| {
            let (st, v) = upload(port, sid, &format!("t{i}.wav"), &wav(1, 100 + i));
            assert_eq!(st, 200, "{v}");
            v["file_id"].as_u64().unwrap()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// add_media_*: streamed quick import
// ---------------------------------------------------------------------------

#[test]
fn add_media_streams_to_disk_and_leaves_no_temp_files() {
    let (_t, root) = library();
    let port = start_http(&root);
    let (st, v) = req(
        port,
        "POST",
        "/api/media?kind=music&filename=Song.wav",
        &[X],
        &wav(2, 130),
    );
    assert_eq!(st, 200, "{v}");
    let all = files(&root);
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(all.iter().any(|f| f.ends_with("/Song.wav")), "{all:?}");
    assert!(all.iter().any(|f| f.ends_with("/Song.noudata")), "{all:?}");
    assert_eq!(tmp_files(&root), Vec::<String>::new());
}

#[test]
fn add_media_killed_upload_leaves_no_partial_file_or_noudata() {
    let (_t, root) = library();
    let port = start_http(&root);
    let body = wav(4, 140);
    {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            s,
            "POST /api/media?kind=mix&filename=Half.wav HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Plippa: 1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        s.write_all(&body[..body.len() / 2]).unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_millis(150));
        // The browser is killed: the connection just goes away.
    }
    // The server notices, deletes its temp part and frees the upload slot.
    let mut ok = false;
    for _ in 0..100 {
        let (st, _) = req(
            port,
            "POST",
            "/api/media?kind=music&filename=y.wav",
            &[X],
            &wav(1, 141),
        );
        if st == 200 {
            ok = true;
            break;
        }
        assert_eq!(st, 429, "only the slot may still be busy");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(ok, "the upload slot was never released");
    let all = files(&root);
    assert!(all.iter().all(|f| !f.contains("Half")), "{all:?}");
    assert_eq!(all.len(), 2, "only the second upload: {all:?}");
    assert_eq!(tmp_files(&root), Vec::<String>::new());
}

#[test]
fn add_media_bad_cue_paste_or_non_audio_writes_nothing() {
    let (_t, root) = library();
    let port = start_http(&root);
    // music + cues: refused before the body is read
    let (st, _) = req(
        port,
        "POST",
        &format!(
            "/api/media?kind=music&filename=a.wav&cue_text={}",
            pct("0:00 A")
        ),
        &[X],
        &wav(1, 1),
    );
    assert_eq!(st, 400);
    // a paste with no usable line
    let (st, _) = req(
        port,
        "POST",
        &format!(
            "/api/media?kind=mix&filename=a.wav&cue_text={}",
            pct("nothing here")
        ),
        &[X],
        &wav(1, 1),
    );
    assert_eq!(st, 400);
    // not audio
    let (st, _) = req(
        port,
        "POST",
        "/api/media?kind=mix&filename=a.wav",
        &[X],
        b"<html>hi</html>",
    );
    assert_eq!(st, 415);
    assert_eq!(files(&root), Vec::<String>::new());
    assert_eq!(tmp_files(&root), Vec::<String>::new());
}

#[test]
fn add_media_without_content_length_is_refused_and_needs_the_csrf_header() {
    let (_t, root) = library();
    let port = start_http(&root);
    let (st, _) = req(
        port,
        "POST",
        "/api/media?kind=music&filename=a.wav",
        &[],
        &wav(1, 1),
    );
    assert_eq!(st, 415, "no X-Plippa / JSON content type");
    assert_eq!(files(&root), Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// imports_*: multi-file import
// ---------------------------------------------------------------------------

#[test]
fn imports_album_uses_the_chosen_order_and_release_folder() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port);
    let ids = upload_eight(port, &sid);
    // reorder two (t2 <-> t5) and drop t7
    let order = [ids[0], ids[4], ids[2], ids[3], ids[1], ids[5], ids[7]];
    let (st, v) = finish(port, &sid, "album", "X", &order);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["tracks"], 7);
    let rel = v["noudata_relpath"].as_str().unwrap();
    assert_eq!(rel, "Music/Unknown Artist/X/_X.noudata");
    assert_eq!(
        media_names(&root, rel),
        ["t1.wav", "t5.wav", "t3.wav", "t4.wav", "t2.wav", "t6.wav", "t8.wav"]
    );
    let bytes = fs::read(root.join(rel)).unwrap();
    let (doc, _) = parse_noudata_with_warnings(&bytes).unwrap();
    assert_eq!(doc.playable_item_title.as_deref(), Some("X"));
    let all = files(&root);
    assert_eq!(
        all.iter().filter(|f| f.ends_with(".wav")).count(),
        7,
        "{all:?}"
    );
    assert!(
        all.iter().all(|f| !f.ends_with("/t7.wav")),
        "t7 was removed: {all:?}"
    );
    // The session (and its staged leftovers) is gone.
    assert_eq!(tmp_files(&root), Vec::<String>::new());
    let (st, _) = req(port, "GET", &format!("/api/imports/{sid}"), &[], b"");
    assert_eq!(st, 404);
    // It is a real album in the index, in that order.
    let (_, cols) = req(port, "GET", "/api/collections?kind=album", &[], b"");
    let album = cols["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["title"] == "X")
        .unwrap_or_else(|| panic!("album not listed: {cols}"));
    let (_, d) = req(
        port,
        "GET",
        &format!("/api/collections/{}", album["id"].as_str().unwrap()),
        &[],
        b"",
    );
    assert_eq!(d["items"].as_array().unwrap().len(), 7);
    assert_eq!(d["items"][1]["title"], "t5");
}

#[test]
fn imports_album_with_same_name_gets_its_own_folder() {
    let (_t, root) = library();
    let port = start_http(&root);
    for expect in [
        "Music/Unknown Artist/X/_X.noudata",
        "Music/Unknown Artist/X (2)/_X (2).noudata",
    ] {
        let sid = begin(port);
        let (_, v) = upload(port, &sid, "same.wav", &wav(1, 9));
        let (st, v) = finish(port, &sid, "album", "X", &[v["file_id"].as_u64().unwrap()]);
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["noudata_relpath"], expect);
        assert_eq!(v["title"], "X");
    }
}

#[test]
fn imports_playlist_imports_tracks_then_lists_them_in_order() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port);
    let ids = upload_eight(port, &sid);
    let order = [ids[2], ids[0], ids[1]];
    let (st, v) = finish(port, &sid, "playlist", "Mix Tape", &order);
    assert_eq!(st, 200, "{v}");
    let rel = v["noudata_relpath"].as_str().unwrap();
    assert_eq!(rel, "Playlists/_Mix Tape.noudata");
    let names = media_names(&root, rel);
    let stems: Vec<&str> = names
        .iter()
        .map(|n| n.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(stems, ["t3.wav", "t1.wav", "t2.wav"], "{names:?}");
    // tagless files are loose tracks: in the Music root, each with its own .noudata
    let all = files(&root);
    for t in ["t1", "t2", "t3"] {
        assert!(all.contains(&format!("Music/{t}.wav")), "{all:?}");
        assert!(all.contains(&format!("Music/{t}.noudata")));
    }
    assert_eq!(
        all.iter().filter(|f| f.ends_with(".wav")).count(),
        3,
        "unlisted files are not imported"
    );
    assert_eq!(tmp_files(&root), Vec::<String>::new());
    // the API sees a playlist with three items in that order
    let (_, d) = req(port, "GET", "/api/collections?kind=playlist", &[], b"");
    let pl = d["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["title"] == "Mix Tape")
        .unwrap();
    let (_, det) = req(
        port,
        "GET",
        &format!("/api/collections/{}", pl["id"].as_str().unwrap()),
        &[],
        b"",
    );
    let titles: Vec<&str> = det["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["t3", "t1", "t2"]);
}

#[test]
fn imports_a_refused_file_does_not_stop_the_others_and_can_be_retried() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port);
    let (st, a) = upload(port, &sid, "a.wav", &wav(1, 1));
    assert_eq!(st, 200);
    let (st, bad) = upload(
        port,
        &sid,
        "b.wav",
        b"<html>not audio, quite long enough to sniff, really</html>",
    );
    assert_eq!(st, 415, "{bad}");
    assert!(bad["error"].as_str().unwrap().contains("audio"));
    let (st, c) = upload(port, &sid, "c.wav", &wav(1, 3));
    assert_eq!(st, 200);
    // retry the failed one with proper content
    let (st, b) = upload(port, &sid, "b.wav", &wav(1, 2));
    assert_eq!(st, 200);
    // the refused upload left no file in staging
    let staged: Vec<String> = tmp_files(&root);
    assert_eq!(staged.len(), 3, "{staged:?}");
    let order = [
        a["file_id"].as_u64().unwrap(),
        b["file_id"].as_u64().unwrap(),
        c["file_id"].as_u64().unwrap(),
    ];
    let (st, v) = finish(port, &sid, "album", "Retry", &order);
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        media_names(&root, v["noudata_relpath"].as_str().unwrap()),
        ["a.wav", "b.wav", "c.wav"]
    );
}

#[test]
fn imports_finish_validates_before_touching_the_library() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port);
    let (_, a) = upload(port, &sid, "a.wav", &wav(1, 1));
    let a = a["file_id"].as_u64().unwrap();
    let long = "x".repeat(300);
    for (mode, name, order) in [
        ("album", "ok", vec![]),              // nothing chosen
        ("album", "ok", vec![a, a]),          // twice
        ("album", "ok", vec![a, 99]),         // never uploaded
        ("album", "  ", vec![a]),             // empty name
        ("album", "bad\u{7}name", vec![a]),   // control character
        ("playlist", long.as_str(), vec![a]), // too long
    ] {
        let (st, v) = finish(port, &sid, mode, name, &order);
        assert_eq!(st, 400, "{mode} {name:?} {order:?}: {v}");
    }
    assert_eq!(files(&root), Vec::<String>::new());
    // the session is still usable
    let (st, _) = finish(port, &sid, "album", "ok", &[a]);
    assert_eq!(st, 200);
}

#[test]
fn imports_failed_playlist_finish_rolls_everything_back() {
    let (_t, root) = library();
    // `Playlists` is a plain file: the playlist cannot be created *after* the tracks were imported.
    fs::write(root.join("Playlists"), b"blocker").unwrap();
    let port = start_http(&root);
    let sid = begin(port);
    let (_, a) = upload(port, &sid, "a.wav", &wav(1, 1));
    let (_, b) = upload(port, &sid, "b.wav", &wav(1, 2));
    let order = [
        a["file_id"].as_u64().unwrap(),
        b["file_id"].as_u64().unwrap(),
    ];
    let (st, v) = finish(port, &sid, "playlist", "Doomed", &order);
    assert!(st >= 400, "{st} {v}");
    // no half-built collection: only the blocker remains; the audio is back in staging
    assert_eq!(files(&root), ["Playlists"]);
    assert!(
        !root.join("Music").exists(),
        "created folders are removed again"
    );
    assert_eq!(tmp_files(&root).len(), 2);
    // fix the problem and retry the same session
    fs::remove_file(root.join("Playlists")).unwrap();
    let (st, v) = finish(port, &sid, "playlist", "Doomed", &order);
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        files(&root)
            .iter()
            .filter(|f| f.ends_with(".noudata"))
            .count(),
        3
    );
}

#[test]
fn imports_discard_deletes_staged_files_and_startup_sweeps_leftovers() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port);
    upload(port, &sid, "a.wav", &wav(1, 1));
    assert_eq!(tmp_files(&root).len(), 1);
    let (st, _) = req(port, "DELETE", &format!("/api/imports/{sid}"), &[X], b"");
    assert_eq!(st, 200);
    assert_eq!(tmp_files(&root), Vec::<String>::new());
    let (st, _) = req(port, "DELETE", &format!("/api/imports/{sid}"), &[X], b"");
    assert_eq!(st, 404);
    // a staging folder of a crashed earlier run is removed by the next `begin`
    let stale = root.join(".plippa/tmp/import-00000000000000000000000000000001");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("f1.bin"), b"x").unwrap();
    let _ = begin(port);
    assert!(!stale.exists());
}

#[test]
fn imports_guards_and_hostile_names_stay_inside_the_library() {
    let (_t, root) = library();
    let port = start_http(&root);
    // no CSRF header / content type
    let (st, _) = req(port, "POST", "/api/imports", &[], b"");
    assert_eq!(st, 415);
    let sid = begin(port);
    let (st, _) = req(
        port,
        "POST",
        &format!("/api/imports/{sid}/files?name=a.wav"),
        &[],
        &wav(1, 1),
    );
    assert_eq!(st, 415);
    // finish needs application/json
    let (st, _) = req(
        port,
        "POST",
        &format!("/api/imports/{sid}/finish"),
        &[X],
        b"{}",
    );
    assert_eq!(st, 415);
    // foreign Origin
    let (st, _) = req(
        port,
        "POST",
        &format!("/api/imports/{sid}/finish"),
        &[JSON, ("Origin", "http://evil.example")],
        b"{}",
    );
    assert_eq!(st, 403);
    // malformed / unknown ids
    let (st, _) = req(
        port,
        "POST",
        &format!("/api/imports/{sid}/finish"),
        &[JSON],
        b"{",
    );
    assert_eq!(st, 400);
    let (st, _) = req(
        port,
        "POST",
        "/api/imports/nothex/files?name=a.wav",
        &[X],
        &wav(1, 1),
    );
    assert_eq!(st, 404);
    let (st, _) = req(
        port,
        "POST",
        &format!("/api/imports/{}/files?name=a.wav", "0".repeat(32)),
        &[X],
        &wav(1, 1),
    );
    assert_eq!(st, 404);
    // hostile client file name and collection name
    let (st, v) = upload(port, &sid, "../../../evil\u{202e}.mp3", &wav(1, 5));
    assert_eq!(st, 200, "{v}");
    let (st, v) = finish(
        port,
        &sid,
        "album",
        "../../etc/passwd",
        &[v["file_id"].as_u64().unwrap()],
    );
    assert_eq!(st, 200, "{v}");
    for f in files(&root) {
        assert!(f.starts_with("Music/Unknown Artist/"), "{f}");
        assert!(f.split('/').all(|seg| seg != ".." && seg != "."), "{f}");
        assert!(!f.contains('\u{202e}'), "{f}");
    }
    assert!(!root.parent().unwrap().join("evil.wav").exists());
}

#[test]
fn imports_second_upload_in_flight_is_429() {
    let (_t, root) = library();
    let (port, svc) = start_http_svc(&root);
    let sid = begin(port);
    let body = wav(4, 150);
    let mut slow = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        slow,
        "POST /api/imports/{sid}/files?name=slow.wav HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nX-Plippa: 1\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap();
    slow.write_all(&body[..body.len() / 2]).unwrap();
    slow.flush().unwrap();
    // Wait (bounded) for the server-side "slot taken" signal; probing with uploads would race
    // with the first request's handler for the slot.
    let hub = svc.lock().unwrap().imports();
    let mut waited = 0;
    while !hub.upload_in_flight() {
        waited += 1;
        assert!(waited < 2000, "first upload never took the slot");
        std::thread::sleep(Duration::from_millis(5));
    }
    let (st, _) = upload(port, &sid, "other.wav", &wav(1, 1));
    assert_eq!(
        st, 429,
        "second upload must be refused while the first is in flight"
    );
    slow.write_all(&body[body.len() / 2..]).unwrap();
    let mut out = Vec::new();
    let _ = slow.read_to_end(&mut out);
    assert!(
        String::from_utf8_lossy(&out).contains(" 200 "),
        "{}",
        String::from_utf8_lossy(&out)
    );
}

// ---------------------------------------------------------------------------
// MEDIA_01: the import gate follows the board's playable formats
// ---------------------------------------------------------------------------

/// The first bytes of an iTunes-style M4A (`ftyp` + `M4A ` brand), padded past the sniff window.
fn m4a() -> Vec<u8> {
    let mut v = b"\x00\x00\x00\x18ftypM4A \x00\x00\x00\x00M4A mp42".to_vec();
    v.resize(4096, 0);
    v
}

/// A minimal Ogg Vorbis identification header, padded.
fn ogg() -> Vec<u8> {
    // Page header (27 bytes) + one segment-table entry, then the Vorbis identification packet.
    let mut v = b"OggS\x00\x02".to_vec();
    v.extend_from_slice(&[0u8; 20]);
    v.extend_from_slice(&[1, 0x1e]);
    v.extend_from_slice(b"\x01vorbis");
    v.resize(4096, 0);
    v
}

fn board(aac: bool, opus: bool) -> plippa_core::Capabilities {
    let mut caps = plippa_core::Capabilities::desktop_mock();
    caps.board_id = "plippa-test-board".into();
    caps.audio_formats.aac = aac;
    caps.audio_formats.opus = opus;
    caps
}

#[test]
fn import_refuses_m4a_when_the_board_has_no_aac() {
    let (_t, root) = library();
    let (port, _svc) = start_http_board(&root, board(false, false));

    // Multi-file: the M4A files are refused by name; the WAV files still go through.
    let sid = begin(port);
    let (st, a) = upload(port, &sid, "a.wav", &wav(1, 1));
    assert_eq!(st, 200, "{a}");
    let mut refused = Vec::new();
    for name in ["take.m4a", "alt.m4a"] {
        let (st, v) = upload(port, &sid, name, &m4a());
        assert_eq!(st, 415, "{name}: {v}");
        assert_eq!(v["code"], "wrong_type");
        assert_eq!(v["name"], name, "the refusal names the file");
        let msg = v["error"].as_str().unwrap();
        assert!(msg.contains("M4A/AAC") && msg.contains("plippa-test-board"), "{msg}");
        refused.push(name);
    }
    let (st, c) = upload(port, &sid, "c.wav", &wav(1, 3));
    assert_eq!(st, 200, "{c}");
    assert_eq!(refused.len(), 2);
    // Nothing of the refused files is staged: only the two accepted uploads.
    assert_eq!(tmp_files(&root).len(), 2, "{:?}", tmp_files(&root));

    let order = [a["file_id"].as_u64().unwrap(), c["file_id"].as_u64().unwrap()];
    let (st, v) = finish(port, &sid, "album", "Mixed", &order);
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        media_names(&root, v["noudata_relpath"].as_str().unwrap()),
        ["a.wav", "c.wav"]
    );
    assert!(files(&root).iter().all(|f| !f.ends_with(".m4a")), "{:?}", files(&root));

    // The single "Add media" path is the same gate and writes nothing.
    let (st, v) = req(port, "POST", "/api/media?kind=music&filename=Song.m4a", &[X], &m4a());
    assert_eq!(st, 415, "{v}");
    assert!(files(&root).iter().all(|f| !f.ends_with(".m4a")));
    assert_eq!(tmp_files(&root), Vec::<String>::new());
}

/// SECFIX_06: the build decodes mp3, flac and wav only (Symphonia is trimmed), so a board that
/// claims AAC still cannot import an M4A: the import gate is the play gate, and the play gate
/// never promises a format this build cannot decode. Adding AAC later flips this test.
#[test]
fn import_refuses_m4a_even_when_the_board_claims_aac_because_the_build_cannot_decode_it() {
    let (_t, root) = library();
    let (port, _svc) = start_http_board(&root, board(true, false));
    let sid = begin(port);
    let (st, v) = upload(port, &sid, "take.m4a", &m4a());
    assert_eq!(st, 415, "{v}");
    assert_eq!(v["code"], "wrong_type");
    assert!(v["error"].as_str().unwrap().contains("M4A/AAC"), "{v}");
    assert!(files(&root).iter().all(|f| !f.ends_with(".m4a")), "{:?}", files(&root));
}

#[test]
fn import_refuses_ogg_on_every_board() {
    // Vorbis is in no board's capabilities, so even a board with every optional format on
    // refuses it, with the same clear sentence.
    for caps in [board(false, false), board(true, true)] {
        let (_t, root) = library();
        let (port, _svc) = start_http_board(&root, caps);
        let sid = begin(port);
        let (st, v) = upload(port, &sid, "song.ogg", &ogg());
        assert_eq!(st, 415, "{v}");
        let msg = v["error"].as_str().unwrap();
        assert!(msg.contains("not supported by this player"), "{msg}");
        assert_eq!(tmp_files(&root).len(), 0, "{:?}", tmp_files(&root));
    }
}

#[test]
fn status_publishes_the_board_formats() {
    let (_t, root) = library();
    let (port, _svc) = start_http_board(&root, board(false, false));
    let (st, v) = req(port, "GET", "/api/status", &[], b"");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["board_id"], "plippa-test-board");
    assert_eq!(
        v["audio_formats"],
        json!({"mp3": true, "wav": true, "flac": true, "aac": false, "opus": false})
    );
    let (_t2, root2) = library();
    let (port2, _svc2) = start_http_board(&root2, board(true, false));
    let (_, v) = req(port2, "GET", "/api/status", &[], b"");
    assert_eq!(v["audio_formats"]["aac"], true);
}
