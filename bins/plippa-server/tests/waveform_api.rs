//! WAVE_01: `GET /api/waveforms/:id` and the status rollup.
//!
//! Fixture: `crates/plippa-noubin/tests/fixtures/libraries/kinds` copied to a temp dir. Media
//! files are empty placeholders; the registry/header paths never decode audio.

mod common;

use plippa_core::waveform::{encode_to_vec, Waveform};
use plippa_noubin::{encode_playable_id, waveform_path};
use plippa_server::{RecordingIpc, ServerConfig, ServerService};
use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn kinds_library() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/plippa-noubin/tests/fixtures/libraries/kinds"),
        &root,
    );
    (tmp, root)
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), to).unwrap();
        }
    }
}

fn service(root: &Path) -> ServerService {
    ServerService::start(
        Some(root.to_path_buf()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    )
}

fn album_id(svc: &mut ServerService) -> u64 {
    svc.list_collections(None)
        .unwrap()
        .into_iter()
        .find(|c| c.title == "The Album")
        .expect("album")
        .id
}

/// A complete sidecar for `rel` at the path the server looks for.
fn write_sidecar(root: &Path, rel: &str) {
    let wf = Waveform::new_complete(20, 1_000, 44_100, 2, 123, 456, vec![200; 50]);
    let path = waveform_path(root, rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, encode_to_vec(&wf)).unwrap();
}

#[test]
fn waveform_status_reports_queued_then_analysed_and_partial() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = album_id(&mut svc);

    let fresh = svc.waveform_plan(id).unwrap().resolve();
    assert_eq!(fresh.state, "partial");
    assert_eq!(fresh.items.len(), 2);
    assert_eq!(fresh.items[0].media_path, "Music/Band/Album/01 - One.mp3");
    assert!(fresh.items.iter().all(|i| i.state == "queued"), "{fresh:?}");

    write_sidecar(&root, "Music/Band/Album/01 - One.mp3");
    let half = svc.waveform_plan(id).unwrap().resolve();
    assert_eq!(half.state, "partial");
    assert_eq!(half.items[0].state, "analysed");
    assert_eq!(half.items[1].state, "queued");

    write_sidecar(&root, "Music/Band/Album/02 - Two.mp3");
    let all = svc.waveform_plan(id).unwrap().resolve();
    assert_eq!(all.state, "all");
    assert!(all.items.iter().all(|i| i.state == "analysed"));
}

#[test]
fn waveform_status_survives_a_restart_with_an_empty_registry() {
    let (_t, root) = kinds_library();
    write_sidecar(&root, "Music/Band/Album/01 - One.mp3");
    // A brand-new service = fresh process, empty in-memory registry.
    let mut svc = service(&root);
    let id = album_id(&mut svc);
    assert!(svc.waveform_registry().snapshot().analysed.is_empty());
    let st = svc.waveform_plan(id).unwrap().resolve();
    assert_eq!(st.items[0].state, "analysed", "header fallback");
    assert_eq!(st.items[1].state, "queued");
}

#[test]
fn waveform_status_precedence_is_analysed_failed_analysing_queued() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = album_id(&mut svc);
    let one = "Music/Band/Album/01 - One.mp3";
    let two = "Music/Band/Album/02 - Two.mp3";
    let registry = svc.waveform_registry();

    registry.mark_failed(one);
    registry.begin(two);
    let st = svc.waveform_plan(id).unwrap().resolve();
    assert_eq!(st.items[0].state, "failed");
    assert_eq!(st.items[1].state, "analysing");

    // A stale `analysing` must not outrank `failed`.
    registry.begin(one);
    assert_eq!(
        svc.waveform_plan(id).unwrap().resolve().items[0].state,
        "failed"
    );

    // Registry `analysed` wins even when the file is not on disk (a just-deleted sidecar is
    // re-queued on the next sweep, but the endpoint reports what the worker knows).
    registry.mark_analysed(two);
    registry.end();
    assert_eq!(
        svc.waveform_plan(id).unwrap().resolve().items[1].state,
        "analysed"
    );
}

#[test]
fn waveform_status_reports_an_orphan_playable_from_playable_items() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let orphan = svc
        .list_collections(None)
        .unwrap()
        .into_iter()
        .find(|c| c.is_orphan)
        .expect("the fixture has an orphan media file");

    let st = svc.waveform_plan(orphan.id).unwrap().resolve();
    assert_eq!(st.items.len(), 1);
    // The media path comes from `playable_items.media_relpath`; there is no `.noudata`.
    assert_eq!(st.items[0].media_path, "Loose/orphan song.mp3");
    assert_eq!(st.items[0].state, "queued");
    assert_eq!(st.state, "partial");

    write_sidecar(&root, "Loose/orphan song.mp3");
    let st = svc.waveform_plan(orphan.id).unwrap().resolve();
    assert_eq!(st.items[0].state, "analysed", "header fallback for an orphan");
    assert_eq!(st.state, "all");
}

#[test]
fn waveform_status_reports_missing_media_as_unavailable_not_queued() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = album_id(&mut svc);
    fs::remove_file(root.join("Music/Band/Album/02 - Two.mp3")).unwrap();
    svc.full_scan().unwrap();

    let st = svc.waveform_plan(id).unwrap().resolve();
    assert_eq!(st.items[1].state, "unavailable", "{st:?}");
    assert_eq!(st.state, "partial");
    // The remaining item is still queued for analysis.
    assert_eq!(st.items[0].state, "queued");
}

#[test]
fn waveform_status_handles_unknown_ids_without_leaking_paths() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = album_id(&mut svc);

    assert!(svc.waveform_plan(id + 999_999).is_err());

    // An id for the same local playable but a different library namespace is not found and
    // leaks no path from this root.
    let (_, local) = plippa_noubin::decode_playable_id(id);
    let foreign = encode_playable_id(Path::new("/media/not-mounted"), local);
    let err = svc.waveform_plan(foreign).unwrap_err();
    assert!(err.to_string().contains("No playable"), "{err}");
    assert!(!err.to_string().contains("Music/"), "{err}");
}

#[test]
fn waveform_status_cannot_reach_another_mounted_root_through_an_id() {
    // Two sticks mounted at once: an id built from root A's namespace and root B's local id
    // must resolve inside A (or 404), never expose B's items.
    let (_ta, root_a) = kinds_library();
    let (_tb, root_b) = kinds_library();
    // Give B a distinctive lone media file so a leak is unmistakable.
    fs::write(root_b.join("BOnly - secret.mp3"), b"").unwrap();
    let mut svc = ServerService::start_with_roots(
        vec![root_a.clone(), root_b.clone()],
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    let b_id = svc
        .list_collections(None)
        .unwrap()
        .into_iter()
        .find(|c| c.is_orphan && c.title.contains("BOnly"))
        .expect("B's orphan playable")
        .id;
    let b_status = svc.waveform_plan(b_id).unwrap().resolve();
    assert!(
        b_status
            .items
            .iter()
            .any(|i| i.media_path.contains("BOnly")),
        "two roots really are mounted: {b_status:?}"
    );

    let (_, b_local) = plippa_noubin::decode_playable_id(b_id);
    let cross = encode_playable_id(&root_a, b_local);
    match svc.waveform_plan(cross) {
        Ok(plan) => {
            let st = plan.resolve();
            assert!(
                st.items.iter().all(|i| !i.media_path.contains("BOnly")),
                "id leaked another root's item: {st:?}"
            );
        }
        Err(e) => assert!(!e.to_string().contains("BOnly"), "{e}"),
    }
}

// ---- HTTP surface ----------------------------------------------------------------------

fn http(port: u16, method: &str, path: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = &raw[split + 4..];
    let value = serde_json::from_slice(body).unwrap_or(Value::Null);
    (status, value)
}

fn start_http(root: &Path) -> u16 {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let svc = Arc::new(Mutex::new(service(root)));
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        library_root: Some(root.to_path_buf()),
        secrets_dir: None,
        portal_url: None,
        ..ServerConfig::default()
    };
    common::serve_open(cfg, svc);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return port;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

#[test]
fn waveforms_endpoint_needs_no_x_plippa_header_and_rejects_bad_ids() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = album_id(&mut svc);
    write_sidecar(&root, "Music/Band/Album/01 - One.mp3");
    let port = start_http(&root);

    let (status, body) = http(port, "GET", &format!("/api/waveforms/{id}"));
    assert_eq!(status, 200);
    assert_eq!(body["id"], serde_json::json!(id));
    assert_eq!(body["state"], "partial");
    assert_eq!(body["items"][0]["item"], 0);
    assert_eq!(body["items"][0]["state"], "analysed");
    assert_eq!(body["items"][1]["state"], "queued");

    assert_eq!(http(port, "GET", "/api/waveforms/not-a-number").0, 400);
    assert_eq!(http(port, "GET", "/api/waveforms/999999999").0, 404);
    assert_eq!(http(port, "GET", "/api/waveforms").0, 404);
}

// ---- TRACK_01: POST /api/media/:id/analysis/reanalyse -----------------------------------

/// `http` with a body. `json` controls the `Content-Type`, which the state-change guard needs.
fn http_post(port: u16, path: &str, body: Option<&str>, json: bool) -> (u16, Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let body = body.unwrap_or("");
    let ctype = if json { "Content-Type: application/json\r\n" } else { "" };
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{ctype}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let value = serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null);
    (status, value)
}

#[test]
fn reanalyse_accepts_an_available_item_and_the_status_reads_queued() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = album_id(&mut svc);
    // Analysed on disk: without the request this would read `analysed`.
    write_sidecar(&root, "Music/Band/Album/01 - One.mp3");
    let port = start_http(&root);
    let path = format!("/api/media/{id}/analysis/reanalyse");

    let (_, before) = http(port, "GET", &format!("/api/waveforms/{id}"));
    assert_eq!(before["items"][0]["state"], "analysed");

    let (status, body) = http_post(port, &path, Some(r#"{"item":0,"kinds":["waveform"]}"#), true);
    assert_eq!((status, &body["state"]), (202, &serde_json::json!("queued")), "{body}");

    let (_, after) = http(port, "GET", &format!("/api/waveforms/{id}"));
    assert_eq!(after["items"][0]["state"], "queued");
    assert_eq!(after["items"][0]["kinds"][0]["kind"], "waveform");
    assert_eq!(after["items"][0]["kinds"][0]["state"], "queued");
    assert_eq!(after["items"][1]["state"], "queued", "the other item is untouched");

    // No body at all means the first item, every kind.
    let (status, _) = http_post(port, &path, None, true);
    assert_eq!(status, 202);
}

#[test]
fn reanalyse_refuses_a_missing_file_an_unknown_id_and_an_unknown_item() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = album_id(&mut svc);
    fs::remove_file(root.join("Music/Band/Album/02 - Two.mp3")).unwrap();
    let port = start_http(&root);

    let (status, body) = http_post(
        port,
        &format!("/api/media/{id}/analysis/reanalyse"),
        Some(r#"{"item":1}"#),
        true,
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["code"], "unavailable");

    let (status, _) = http_post(port, "/api/media/999999999/analysis/reanalyse", None, true);
    assert_eq!(status, 404);
    let (status, _) = http_post(
        port,
        &format!("/api/media/{id}/analysis/reanalyse"),
        Some(r#"{"item":99}"#),
        true,
    );
    assert_eq!(status, 404);
    let (status, _) = http_post(port, "/api/media/not-a-number/analysis/reanalyse", None, true);
    assert_eq!(status, 400);
}

#[test]
fn reanalyse_validates_kinds_strictly_and_keeps_the_csrf_guard() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = album_id(&mut svc);
    let port = start_http(&root);
    let path = format!("/api/media/{id}/analysis/reanalyse");

    for bad in [
        r#"{"kinds":["bpm"]}"#,
        r#"{"kinds":["waveform","tempo"]}"#,
        r#"{"kinds":[]}"#,
        r#"{"kinds":"waveform"}"#,
        r#"{"surprise":1}"#,
        "not json",
    ] {
        let (status, body) = http_post(port, &path, Some(bad), true);
        assert_eq!(status, 400, "{bad}: {body}");
    }
    // The route has no exemption from the guard: a body without a JSON content type is refused.
    let (status, _) = http_post(port, &path, Some("{}"), false);
    assert_eq!(status, 415);
}

#[test]
fn the_status_carries_the_skip_count_of_an_analysis_that_survived_bad_frames() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let id = album_id(&mut svc);
    let one = "Music/Band/Album/01 - One.mp3";
    let registry = svc.waveform_registry();

    registry.mark_analysed_with_skips(one, 2);
    let st = svc.waveform_plan(id).unwrap().resolve();
    assert_eq!(st.items[0].kinds[0].state, "analysed");
    assert_eq!(st.items[0].kinds[0].skipped_frames, 2);
    let json = serde_json::to_value(&st).unwrap();
    assert_eq!(json["items"][0]["kinds"][0]["skipped_frames"], 2);
    assert!(
        json["items"][1]["kinds"][0].get("skipped_frames").is_none(),
        "no skips, no field"
    );
}
