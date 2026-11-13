//! ART_03: cover art over HTTP — list / serve / stage / commit, thumbnails and backfill.
//!
//! Libraries are built in temp dirs with real (generated) JPEGs; media files are empty
//! placeholders. Everything runs in process.

mod common;

use plippa_image::{decode, encode_jpeg_fit, probe, ImageLimits, Rgb8};
use plippa_server::{
    ensure_thumbnails, spawn_cover_worker, cover_limits, RecordingIpc, ServerConfig, ServerService,
};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, (w + h) as u8]);
        }
    }
    encode_jpeg_fit(&Rgb8 { w, h, px }, w.max(h)).unwrap()
}

fn dims(bytes: &[u8]) -> (u32, u32) {
    let (_, w, h) = probe(bytes, &ImageLimits::default()).unwrap();
    (w, h)
}

fn images_json(names: &[&str]) -> Value {
    json!({ "images": names.iter().map(|n| json!({"imageLocalFilename": n})).collect::<Vec<_>>() })
}

/// `Music/Band/Alb`: one release with `artist` images in `releaseData.coverImage` and `local`
/// ones in `localCoverImage`; every image file is generated at a distinct size.
fn album(root: &Path, artist: &[(&str, u32, u32)], local: &[(&str, u32, u32)]) -> PathBuf {
    let dir = root.join("Music/Band/Alb");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("01.mp3"), b"").unwrap();
    for (n, w, h) in artist.iter().chain(local) {
        fs::write(dir.join(n), jpeg(*w, *h)).unwrap();
    }
    let mut doc = json!({
        "noudataVersion": "0.4.0",
        "noudataType": "release",
        "noubinKey": "Alb.noubin.AAA111",
        "noubinKeyNormalised": "alb.noubin.aaa111",
        "playableItemTitle": "Alb",
        "releaseData": { "credits": { "primaryArtists": [ { "primaryArtistName": "Band" } ] } },
        "mediaList": [ { "mediaTitle": "One", "localFilePaths": ["01.mp3"] } ]
    });
    if !artist.is_empty() {
        let names: Vec<&str> = artist.iter().map(|a| a.0).collect();
        doc["releaseData"]["coverImage"] = images_json(&names);
    }
    if !local.is_empty() {
        let names: Vec<&str> = local.iter().map(|a| a.0).collect();
        doc["localPlaybackData"] = json!({ "localCoverImage": images_json(&names) });
    }
    fs::write(dir.join("_Alb.noudata"), serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
    dir
}

fn read_doc(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn names_of(list: &Value) -> Vec<String> {
    list["images"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["imageLocalFilename"].as_str().unwrap().to_string())
        .collect()
}

/// Every file under `dir` with its bytes, for "nothing changed" checks.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.ends_with(".plippa") {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                stack.push(p);
            } else {
                out.push((p.clone(), fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
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

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }
}

fn http(port: u16, method: &str, path: &str, body: &[u8], headers: &[(&str, &str)]) -> Reply {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-Plippa: 1\r\n{extra}Content-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    Reply {
        status,
        head,
        body: raw[split + 4..].to_vec(),
    }
}

fn get(port: u16, path: &str) -> Reply {
    http(port, "GET", path, b"", &[])
}

fn put_covers(port: u16, id: &str, order: Value) -> Reply {
    http(
        port,
        "PUT",
        &format!("/api/collections/{id}/covers"),
        json!({ "order": order }).to_string().as_bytes(),
        &[("Content-Type", "application/json")],
    )
}

fn stage(port: u16, id: &str, bytes: &[u8]) -> Reply {
    http(
        port,
        "POST",
        &format!("/api/collections/{id}/covers/staged?name=new.jpg"),
        bytes,
        &[("Content-Type", "application/octet-stream")],
    )
}

fn start(root: &Path) -> (u16, Arc<Mutex<ServerService>>) {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let svc = Arc::new(Mutex::new(ServerService::start(
        Some(root.to_path_buf()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    )));
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

/// Id (decimal string) of the collection titled `title`.
fn id_of(port: u16, title: &str) -> String {
    let v = get(port, "/api/collections").json();
    v["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["title"] == title)
        .unwrap_or_else(|| panic!("no collection {title}: {v}"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// Read API
// ---------------------------------------------------------------------------

#[test]
fn list_and_serve_sizes_with_validators() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[("a.jpg", 300, 200), ("b.jpg", 64, 64), ("c.jpg", 700, 500)], &[]);
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");

    let list = get(port, &format!("/api/collections/{id}/covers"));
    assert_eq!(list.status, 200);
    let covers = list.json()["covers"].as_array().unwrap().clone();
    let shape: Vec<_> = covers
        .iter()
        .map(|c| (c["n"].as_u64().unwrap(), c["w"].as_u64().unwrap(), c["h"].as_u64().unwrap()))
        .collect();
    assert_eq!(shape, [(0, 300, 200), (1, 64, 64), (2, 700, 500)]);
    assert!(covers.iter().all(|c| c["kind"] == "jpeg" && c["source"] == "release"));
    assert_eq!(covers[0]["bytes"], fs::metadata(dir.join("a.jpg")).unwrap().len());

    // The summary carries the primary thumbnail and the count.
    let all = get(port, "/api/collections").json();
    let me = all["collections"].as_array().unwrap().iter().find(|c| c["title"] == "Alb").unwrap();
    assert_eq!(me["cover_count"], 3);
    assert!(me["cover_thumb"].as_str().unwrap().contains("size=128"));
    assert!(me["cover"].as_str().unwrap().ends_with("/cover"));

    // No thumbnail yet: the original is served, with a validator and a private cache policy.
    let orig = fs::read(dir.join("c.jpg")).unwrap();
    let small = get(port, &format!("/api/collections/{id}/covers/2?size=128"));
    assert_eq!(small.status, 200);
    assert_eq!(small.body, orig);
    assert_eq!(small.header("content-type").as_deref(), Some("image/jpeg"));
    assert_eq!(small.header("cache-control").as_deref(), Some("private, no-cache"));
    let etag = small.header("etag").unwrap();
    let again = http(port, "GET", &format!("/api/collections/{id}/covers/2?size=128"), b"", &[("If-None-Match", &etag)]);
    assert_eq!(again.status, 304);
    assert!(again.body.is_empty());

    // With thumbnails the tier is served; `full` still gives the original; compat route = 512.
    ensure_thumbnails(t.path(), "Music/Band/Alb/c.jpg", &cover_limits()).unwrap();
    let thumb = get(port, &format!("/api/collections/{id}/covers/2?size=128"));
    assert_eq!(dims(&thumb.body), (128, 91));
    assert_ne!(thumb.header("etag"), Some(etag));
    let full = get(port, &format!("/api/collections/{id}/covers/2?size=full"));
    assert_eq!(full.body, orig);
    assert_eq!(get(port, &format!("/api/collections/{id}/covers/2")).body, orig);
    let compat = get(port, &format!("/api/collections/{id}/cover"));
    assert_eq!(compat.status, 200);
    assert_eq!(dims(&compat.body), (300, 200), "a.jpg is the primary and has no thumbnails: the original");

    // Errors never carry a path.
    for (path, status) in [
        (format!("/api/collections/{id}/covers/2?size=99"), 400),
        (format!("/api/collections/{id}/covers/9"), 404),
        (format!("/api/collections/{id}/covers/x"), 404),
        ("/api/collections/abc/covers".to_string(), 400),
    ] {
        let r = get(port, &path);
        assert_eq!(r.status, status, "{path}");
        assert!(!String::from_utf8_lossy(&r.body).contains("Music"), "{path}");
    }

    // The item route answers for a one-item release with the same list.
    let item = get(port, &format!("/api/items/{id}/covers?item=0")).json();
    assert_eq!(item["covers"].as_array().unwrap().len(), 3);
    assert!(item["covers"][0]["url"].as_str().unwrap().contains("/api/items/"));
    let item_img = get(port, &format!("/api/items/{id}/covers/1?item=0&size=full"));
    assert_eq!(item_img.body, fs::read(dir.join("b.jpg")).unwrap());
    assert_eq!(get(port, &format!("/api/items/{id}/covers?item=7")).status, 404);
}

#[test]
fn files_that_are_not_jpeg_png_webp_are_not_served() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[("good.jpg", 100, 100)], &[]);
    fs::write(dir.join("anim.png"), b"GIF89a\x01\x00\x01\x00\x00\x00\x00;").unwrap();
    fs::write(dir.join("text.png"), b"#!/bin/sh\necho not a picture\n").unwrap();
    // Declare them all (artist list) by rewriting the document.
    let mut doc = read_doc(&dir.join("_Alb.noudata"));
    doc["releaseData"]["coverImage"] = images_json(&["good.jpg", "anim.png", "text.png"]);
    fs::write(dir.join("_Alb.noudata"), serde_json::to_vec(&doc).unwrap()).unwrap();
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");

    let list = get(port, &format!("/api/collections/{id}/covers")).json();
    let ns: Vec<u64> = list["covers"].as_array().unwrap().iter().map(|c| c["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, [0], "only the real image is listed");
    for n in [1, 2] {
        for size in ["full", "128"] {
            let r = get(port, &format!("/api/collections/{id}/covers/{n}?size={size}"));
            assert_eq!(r.status, 404, "{n} {size}");
            assert!(r.header("content-type").unwrap().starts_with("application/json"));
        }
    }
}

// ---------------------------------------------------------------------------
// Edit API
// ---------------------------------------------------------------------------

#[test]
fn keep_reorder_and_delete_with_the_delete_rules() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[], &[("a.jpg", 90, 90), ("b.jpg", 80, 80), ("c.jpg", 70, 70)]);
    for n in ["a", "b", "c"] {
        ensure_thumbnails(t.path(), &format!("Music/Band/Alb/{n}.jpg"), &cover_limits()).unwrap();
    }
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");

    let r = put_covers(port, &id, json!([{"keep": 2}, {"keep": 0}]));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let sizes: Vec<_> = r.json()["covers"].as_array().unwrap().iter().map(|c| c["w"].as_u64().unwrap()).collect();
    assert_eq!(sizes, [70, 90], "reordered");
    let doc = read_doc(&dir.join("_Alb.noudata"));
    assert_eq!(names_of(&doc["localPlaybackData"]["localCoverImage"]), ["c.jpg", "a.jpg"]);
    assert!(!dir.join("b.jpg").exists(), "unreferenced: deleted");
    assert!(!dir.join("thumbnails/b-128.jpg").exists(), "with its thumbnails");
    assert!(dir.join("thumbnails/a-128.jpg").is_file());
    // The portal sees the new list at once.
    let list = get(port, &format!("/api/collections/{id}/covers")).json();
    assert_eq!(list["covers"][0]["w"], 70);

    // A file an artist's coverImage still names is kept when the user drops it.
    let mut doc = read_doc(&dir.join("_Alb.noudata"));
    doc["releaseData"]["coverImage"] = images_json(&["a.jpg"]);
    fs::write(dir.join("_Alb.noudata"), serde_json::to_vec(&doc).unwrap()).unwrap();
    assert_eq!(http(port, "POST", "/api/rescan", b"", &[]).status, 200);
    let r = put_covers(port, &id, json!([{"keep": 0}]));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(names_of(&read_doc(&dir.join("_Alb.noudata"))["localPlaybackData"]["localCoverImage"]), ["c.jpg"]);
    assert!(dir.join("a.jpg").is_file(), "still referenced by the artist's list");
}

#[test]
fn a_save_keeps_entries_the_client_could_not_see_and_their_files() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[], &[("a.jpg", 90, 90), ("b.jpg", 80, 80)]);
    // Between them: a file that fails the probe, and an entry whose file is missing.
    fs::write(dir.join("bad.png"), b"#!/bin/sh\necho not a picture\n").unwrap();
    let mut doc = read_doc(&dir.join("_Alb.noudata"));
    doc["localPlaybackData"]["localCoverImage"] =
        images_json(&["a.jpg", "bad.png", "gone.jpg", "b.jpg"]);
    fs::write(dir.join("_Alb.noudata"), serde_json::to_vec(&doc).unwrap()).unwrap();
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");

    let list = get(port, &format!("/api/collections/{id}/covers")).json();
    let ns: Vec<u64> = list["covers"].as_array().unwrap().iter().map(|c| c["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, [0, 2], "the missing file is not even listed by the library");
    assert_eq!(list["hidden"], 1, "the unservable file counts; the missing one does not");

    let r = put_covers(port, &id, json!([{"keep": 2}, {"keep": 0}]));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["hidden"], 1);
    let doc = read_doc(&dir.join("_Alb.noudata"));
    assert_eq!(
        names_of(&doc["localPlaybackData"]["localCoverImage"]),
        ["b.jpg", "a.jpg", "bad.png"],
        "reordered, hidden entry kept after them, missing one dropped"
    );
    assert!(dir.join("bad.png").is_file());

    // Removing every visible image still never deletes the hidden file.
    let r = put_covers(port, &id, json!([]));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert!(dir.join("bad.png").is_file());
    assert!(!dir.join("a.jpg").exists());
    let doc = read_doc(&dir.join("_Alb.noudata"));
    assert_eq!(names_of(&doc["localPlaybackData"]["localCoverImage"]), ["bad.png"]);
}

#[test]
fn bad_references_and_the_limit_change_nothing() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[], &[("a.jpg", 90, 90), ("b.jpg", 80, 80)]);
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");
    let before = snapshot(t.path());

    for (order, status) in [
        (json!([{"keep": 5}]), 400),
        (json!([{"keep": 0}, {"keep": 0}]), 400),
        (json!([{"staged": "00000000000000000000000000000000"}]), 400),
        (json!([{"nonsense": 1}]), 400),
        (Value::Array((0..13).map(|_| json!({"keep": 0})).collect()), 413),
    ] {
        let r = put_covers(port, &id, order.clone());
        assert_eq!(r.status, status, "{order}: {}", String::from_utf8_lossy(&r.body));
    }
    assert_eq!(snapshot(t.path()), before, "library untouched");
    assert!(dir.join("a.jpg").is_file());

    // Not an image / too big / not editable.
    assert_eq!(stage(port, &id, b"plain text, not an image").status, 415);
    assert_eq!(stage(port, &id, b"GIF89a\x01\x00\x01\x00\x00\x00\x00;").status, 415);
    let mut cut = jpeg(300, 300);
    cut.truncate(cut.len() / 2);
    assert!(stage(port, &id, &cut).status >= 400);
    assert_eq!(get(port, &format!("/api/collections/{id}/covers/staged/{}", "0".repeat(32))).status, 404);
    assert_eq!(snapshot(t.path()), before, "refused uploads leave no file behind");
    assert_eq!(stage(port, "999", &jpeg(10, 10)).status, 404);
}

#[test]
fn a_staged_upload_can_be_discarded_and_then_cannot_be_saved() {
    let t = tempfile::tempdir().unwrap();
    album(t.path(), &[], &[("a.jpg", 90, 90)]);
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");
    let staged = stage(port, &id, &jpeg(300, 300)).json()["staged_id"].as_str().unwrap().to_string();
    let url = format!("/api/collections/{id}/covers/staged/{staged}");
    assert_eq!(get(port, &url).status, 200);
    let gone = http(port, "DELETE", &url, b"", &[]);
    assert_eq!(gone.status, 200);
    assert_eq!(get(port, &url).status, 404);
    assert_eq!(put_covers(port, &id, json!([{"staged": staged}])).status, 400);
    assert_eq!(http(port, "DELETE", &url, b"", &[]).status, 200, "idempotent");
    assert_eq!(http(port, "DELETE", &format!("/api/collections/999/covers/staged/{staged}"), b"", &[]).status, 404);
}

#[test]
fn staged_images_are_added_byte_for_byte_with_thumbnails_and_new_names() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[], &[("a.jpg", 90, 90)]);
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");

    // Non-square images are accepted untouched (D2: the crop is a draw-time operation).
    let wide = jpeg(200, 100);
    let tall = jpeg(100, 200);
    let s1 = stage(port, &id, &wide);
    assert_eq!(s1.status, 200, "{}", String::from_utf8_lossy(&s1.body));
    let (w1, w2) = (s1.json(), stage(port, &id, &tall).json());
    assert_eq!((w1["w"].as_u64(), w1["h"].as_u64(), w1["kind"].as_str()), (Some(200), Some(100), Some("jpeg")));
    let preview = get(port, w1["preview_url"].as_str().unwrap());
    assert_eq!((preview.status, preview.body.clone()), (200, wide.clone()));
    assert_eq!(preview.header("cache-control").as_deref(), Some("no-store"));

    let r = put_covers(
        port,
        &id,
        json!([{"staged": w2["staged_id"]}, {"keep": 0}, {"staged": w1["staged_id"]}]),
    );
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let doc = read_doc(&dir.join("_Alb.noudata"));
    assert_eq!(
        names_of(&doc["localPlaybackData"]["localCoverImage"]),
        ["cover.jpg", "a.jpg", "cover-2.jpg"]
    );
    assert_eq!(fs::read(dir.join("cover.jpg")).unwrap(), tall, "stored as uploaded");
    assert_eq!(fs::read(dir.join("cover-2.jpg")).unwrap(), wide);
    let list = r.json();
    let shape: Vec<_> = list["covers"].as_array().unwrap().iter().map(|c| (c["w"].as_u64().unwrap(), c["h"].as_u64().unwrap())).collect();
    assert_eq!(shape, [(100, 200), (90, 90), (200, 100)]);
    // Served byte-for-byte; thumbnails exist and keep the aspect ratio.
    assert_eq!(get(port, &format!("/api/collections/{id}/covers/2?size=full")).body, wide);
    for stem in ["cover", "cover-2"] {
        assert!(dir.join(format!("thumbnails/{stem}-128.jpg")).is_file(), "{stem}");
    }
    assert_eq!(dims(&fs::read(dir.join("thumbnails/cover-2-128.jpg")).unwrap()), (128, 64));
    // The staged files were consumed; an id cannot be used twice.
    let staging = t.path().join(".plippa/tmp");
    let left: Vec<_> = fs::read_dir(&staging).map(|d| d.flatten().map(|e| e.file_name()).collect()).unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
    assert_eq!(put_covers(port, &id, json!([{"staged": w1["staged_id"]}])).status, 400);
}

#[test]
fn editing_artist_art_copies_on_write_and_never_touches_the_artists_list() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[("a.jpg", 90, 90), ("b.jpg", 80, 80)], &[]);
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");
    let artist_before = read_doc(&dir.join("_Alb.noudata"))["releaseData"]["coverImage"].clone();

    let r = put_covers(port, &id, json!([{"keep": 1}]));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let doc = read_doc(&dir.join("_Alb.noudata"));
    assert_eq!(doc["releaseData"]["coverImage"], artist_before, "artist list untouched");
    assert_eq!(names_of(&doc["localPlaybackData"]["localCoverImage"]), ["b.jpg"]);
    assert!(dir.join("a.jpg").is_file() && dir.join("b.jpg").is_file());
    let sources: Vec<_> = r.json()["covers"].as_array().unwrap().iter().map(|c| c["source"].clone()).collect();
    assert_eq!(sources, ["local"]);

    // Deleting everything hides the artist's art too; its files stay.
    let r = put_covers(port, &id, json!([]));
    assert_eq!(r.status, 200);
    assert!(r.json()["covers"].as_array().unwrap().is_empty());
    let doc = read_doc(&dir.join("_Alb.noudata"));
    assert_eq!(doc["localPlaybackData"]["localCoverImage"], json!({"images": []}));
    assert_eq!(doc["releaseData"]["coverImage"], artist_before);
    assert!(dir.join("a.jpg").is_file() && dir.join("b.jpg").is_file());
    let me = get(port, "/api/collections").json();
    let me = me["collections"].as_array().unwrap().iter().find(|c| c["title"] == "Alb").unwrap().clone();
    assert!(me["cover"].is_null() && me["cover_thumb"].is_null(), "{me}");
}

#[test]
fn loose_tracks_write_item_art_and_playlists_local_art() {
    let (_t, root) = kinds_library();
    let (port, _svc) = start(&root);
    let add = |id: &str| {
        let s = stage(port, id, &jpeg(120, 120));
        assert_eq!(s.status, 200, "{}", String::from_utf8_lossy(&s.body));
        s.json()["staged_id"].clone()
    };

    // A track that refers to its album (its own playable): item art next to its `.noudata`.
    let two = id_of(port, "Two");
    let r = put_covers(port, &two, json!([{"staged": add(&two)}]));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let doc = read_doc(&root.join("Music/Band/Album/02 - Two.noudata"));
    assert_eq!(names_of(&doc["mediaList"][0]["itemCoverImage"]), ["02 - Two-cover.jpg"]);
    assert!(doc.get("localPlaybackData").is_none_or(|l| l.get("localCoverImage").is_none()));
    assert!(root.join("Music/Band/Album/02 - Two-cover.jpg").is_file());
    assert!(!root.join("Music/Band/Album/cover.jpg").exists(), "no shared sibling name");

    // An album row with art of its own is flagged (D16), the others are not.
    let album_dir = root.join("Music/Band/Album");
    fs::write(album_dir.join("two.jpg"), jpeg(50, 50)).unwrap();
    let mut doc = read_doc(&album_dir.join("_Album.noudata"));
    doc["mediaList"][1]["itemCoverImage"] = images_json(&["two.jpg"]);
    fs::write(album_dir.join("_Album.noudata"), serde_json::to_vec(&doc).unwrap()).unwrap();
    assert_eq!(http(port, "POST", "/api/rescan", b"", &[]).status, 200);
    let album_id = id_of(port, "The Album");
    let detail = get(port, &format!("/api/collections/{album_id}")).json();
    let thumbs: Vec<_> = detail["items"].as_array().unwrap().iter().map(|i| i["cover_thumb"].as_str().map(str::to_string)).collect();
    assert_eq!(thumbs[0], None);
    assert!(thumbs[1].as_deref().unwrap().contains("/api/items/"), "{thumbs:?}");
    let tracks = get(port, "/api/tracks?view=track").json();
    let rows: Vec<_> = tracks["tracks"].as_array().unwrap().iter().filter(|t| t["collection_title"] == "The Album").map(|t| (t["title"].as_str().unwrap().to_string(), t["cover_thumb"].is_string())).collect();
    assert!(rows.contains(&("Two".to_string(), true)) && rows.iter().all(|(t, flagged)| *flagged == (t == "Two")), "{rows:?}");
    let thumb = get(port, thumbs[1].as_deref().unwrap());
    assert_eq!((thumb.status, thumb.body), (200, fs::read(album_dir.join("two.jpg")).unwrap()));

    // A playlist: localCoverImage next to the playlist file, under its own name.
    let rainy = id_of(port, "Rainy Day");
    let r = put_covers(port, &rainy, json!([{"staged": add(&rainy)}]));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let doc = read_doc(&root.join("Playlists/Rainy.noudata"));
    assert_eq!(names_of(&doc["localPlaybackData"]["localCoverImage"]), ["Rainy-cover.jpg"]);

    // A media file without a `.noudata` gets the minimal one, with the art as item art.
    let orphan = get(port, "/api/tracks?view=track").json()["tracks"]
        .as_array().unwrap().iter().find(|t| t["is_orphan"] == true).unwrap()["id"].as_str().unwrap().to_string();
    let r = put_covers(port, &orphan, json!([{"staged": add(&orphan)}]));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let made = root.join("Loose/orphan song.noudata");
    let doc = read_doc(&made);
    assert_eq!(doc["noudataType"], "looseTrack");
    assert_eq!(names_of(&doc["mediaList"][0]["itemCoverImage"]), ["orphan song-cover.jpg"]);
    assert_ne!(r.json()["id"].as_str().unwrap(), orphan, "the new playable id is returned");
    assert_eq!(r.json()["covers"].as_array().unwrap().len(), 1);

    // Dynamic playlists have no art to edit.
    let dynamic = id_of(port, "Everything Loud");
    assert_eq!(stage(port, &dynamic, &jpeg(20, 20)).status, 400);
    assert_eq!(put_covers(port, &dynamic, json!([])).status, 400);
}

// ---------------------------------------------------------------------------
// Backfill
// ---------------------------------------------------------------------------

#[test]
fn the_worker_makes_thumbnails_for_art_dropped_onto_the_stick() {
    let t = tempfile::tempdir().unwrap();
    // No `.noudata` mentions the image: it is the sibling `cover.jpg`.
    let dir = album(t.path(), &[], &[]);
    fs::write(dir.join("cover.jpg"), jpeg(600, 400)).unwrap();
    let svc = Arc::new(Mutex::new(ServerService::start(
        Some(t.path().to_path_buf()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    )));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = spawn_cover_worker(Arc::clone(&svc), Arc::clone(&stop));
    let wanted: Vec<PathBuf> = [128, 512, 1024]
        .iter()
        .map(|t| dir.join(format!("thumbnails/cover-{t}.jpg")))
        .collect();
    let started = Instant::now();
    while !wanted.iter().all(|p| p.is_file()) {
        assert!(started.elapsed() < Duration::from_secs(20), "thumbnails never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
    stop.store(true, Ordering::SeqCst);
    worker.join().unwrap();

    // Done once: asking again writes nothing, and the original is as it was.
    assert!(ensure_thumbnails(t.path(), "Music/Band/Alb/cover.jpg", &cover_limits()).unwrap().written.is_empty());
    let d = decode(&fs::read(&wanted[0]).unwrap(), &ImageLimits::default()).unwrap();
    assert_eq!((d.w, d.h), (128, 85));
    assert_eq!(dims(&fs::read(dir.join("cover.jpg")).unwrap()), (600, 400));
}

#[test]
fn a_sibling_cover_shows_in_the_list_and_the_detail_not_only_on_the_device() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[], &[]);
    fs::write(dir.join("cover.jpg"), jpeg(300, 200)).unwrap();
    let (port, svc) = start(t.path());
    let id = id_of(port, "Alb");

    let list = get(port, "/api/collections").json();
    let row = list["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id.as_str())
        .unwrap()
        .clone();
    assert_eq!(row["cover_count"], 1, "a sibling cover.jpg is art the collection shows");
    assert!(row["cover"].is_string(), "the card links its image: {row}");
    let thumb = row["cover_thumb"].as_str().expect("the card offers a thumbnail").to_string();
    let served = get(port, &thumb);
    assert_eq!(served.status, 200);
    // No tier on disk yet in this test: the original stands in.
    assert_eq!(dims(&served.body), (300, 200));

    let detail = get(port, &format!("/api/collections/{id}")).json();
    assert_eq!(detail["cover_count"], 1);
    assert!(detail["cover_thumb"].is_string());

    // Art hidden on purpose (`localCoverImage` with no images) hides the sibling too.
    let mut doc = read_doc(&dir.join("_Alb.noudata"));
    doc["localPlaybackData"] = json!({ "localCoverImage": { "images": [] } });
    fs::write(dir.join("_Alb.noudata"), serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
    svc.lock().unwrap().lazy_rescan().unwrap();
    let row = get(port, "/api/collections").json()["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id.as_str())
        .unwrap()
        .clone();
    assert_eq!(row["cover_count"], 0, "hidden art is not shown: {row}");
    assert!(row["cover"].is_null() && row["cover_thumb"].is_null());
}

#[test]
fn a_failure_before_the_noudata_is_written_leaves_the_library_as_it_was() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[], &[("a.jpg", 90, 90)]);
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");
    let staged = stage(port, &id, &jpeg(300, 300)).json()["staged_id"].clone();
    // A plain file where the thumbnail folder must go makes the thumbnails impossible.
    fs::write(dir.join("thumbnails"), b"in the way").unwrap();
    let before = snapshot(t.path());

    let r = put_covers(port, &id, json!([{"keep": 0}, {"staged": staged}]));
    assert!(r.status >= 400, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(snapshot(t.path()), before, "no new image, no changed .noudata");
    assert!(!dir.join("cover.jpg").exists());

    // The upload is still waiting: once the obstacle is gone the same Save works.
    fs::remove_file(dir.join("thumbnails")).unwrap();
    let r = put_covers(port, &id, json!([{"keep": 0}, {"staged": staged}]));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert!(dir.join("cover.jpg").is_file() && dir.join("thumbnails/cover-128.jpg").is_file());
}

#[test]
fn leftover_staged_uploads_are_swept_at_start_and_oversize_is_refused_up_front() {
    let t = tempfile::tempdir().unwrap();
    album(t.path(), &[], &[("a.jpg", 90, 90)]);
    let tmp = t.path().join(".plippa/tmp");
    fs::create_dir_all(&tmp).unwrap();
    fs::write(tmp.join("cover-00ff"), b"left behind by a crash").unwrap();
    fs::write(tmp.join("other.txt"), b"not ours").unwrap();
    let (port, _svc) = start(t.path());
    assert!(!tmp.join("cover-00ff").exists());
    assert!(tmp.join("other.txt").exists());

    let id = id_of(port, "Alb");
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "POST /api/collections/{id}/covers/staged HTTP/1.1\r\nHost: x\r\nX-Plippa: 1\r\nContent-Length: 17000000\r\n\r\n"
    )
    .unwrap();
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).contains(" 413 "), "{}", String::from_utf8_lossy(&buf[..n]));
}

#[test]
fn a_new_image_never_inherits_the_thumbnails_of_an_earlier_file_of_its_name() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[], &[("cover.jpg", 90, 90)]);
    ensure_thumbnails(t.path(), "Music/Band/Alb/cover.jpg", &cover_limits()).unwrap();
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");

    // Deleting the image takes its thumbnails with it.
    assert_eq!(put_covers(port, &id, json!([])).status, 200);
    assert!(!dir.join("cover.jpg").exists());
    assert!(!dir.join("thumbnails/cover-128.jpg").exists());

    // A leftover thumbnail of the old file (a stick edited elsewhere) must not be served.
    fs::create_dir_all(dir.join("thumbnails")).unwrap();
    fs::write(dir.join("thumbnails/cover-128.jpg"), jpeg(20, 20)).unwrap();
    let wide = jpeg(200, 100);
    let s = stage(port, &id, &wide).json();
    let r = put_covers(port, &id, json!([{"staged": s["staged_id"]}]));
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(dims(&fs::read(dir.join("thumbnails/cover-128.jpg")).unwrap()), (128, 64));
}

#[test]
fn thumbnails_older_than_their_image_are_regenerated() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[], &[("a.jpg", 90, 90)]);
    ensure_thumbnails(t.path(), "Music/Band/Alb/a.jpg", &cover_limits()).unwrap();
    let thumb = dir.join("thumbnails/a-128.jpg");
    // The image is replaced by a different one; the old thumbnail is now older than it.
    fs::write(dir.join("a.jpg"), jpeg(200, 100)).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options().write(true).open(&thumb).unwrap().set_modified(old).unwrap();
    let report = ensure_thumbnails(t.path(), "Music/Band/Alb/a.jpg", &cover_limits()).unwrap();
    assert!(!report.written.is_empty());
    assert_eq!(dims(&fs::read(&thumb).unwrap()), (128, 64));
    // Up to date now: nothing more to do.
    assert!(ensure_thumbnails(t.path(), "Music/Band/Alb/a.jpg", &cover_limits()).unwrap().written.is_empty());
}

#[test]
fn a_read_only_collection_cannot_have_its_covers_edited() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[], &[("a.jpg", 90, 90)]);
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");
    let doc = dir.join("_Alb.noudata");
    fs::set_permissions(&doc, fs::Permissions::from_mode(0o444)).unwrap();
    let before = fs::read(&doc).unwrap();
    assert_eq!(stage(port, &id, &jpeg(40, 40)).status, 409);
    assert_eq!(put_covers(port, &id, json!([])).status, 409);
    assert_eq!(fs::read(&doc).unwrap(), before);
    assert!(dir.join("a.jpg").is_file());
}

#[test]
fn a_dropped_image_is_kept_when_another_noudata_cannot_be_read() {
    let t = tempfile::tempdir().unwrap();
    let dir = album(t.path(), &[], &[("a.jpg", 90, 90)]);
    let other = t.path().join("Music/Band/Other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("01.mp3"), b"").unwrap();
    let mut doc = read_doc(&dir.join("_Alb.noudata"));
    doc["playableItemTitle"] = json!("Other");
    doc["noubinKey"] = json!("Other.noubin.BBB222");
    doc["noubinKeyNormalised"] = json!("other.noubin.bbb222");
    doc.as_object_mut().unwrap().remove("localPlaybackData");
    fs::write(other.join("_Other.noudata"), serde_json::to_vec(&doc).unwrap()).unwrap();
    let (port, _svc) = start(t.path());
    let id = id_of(port, "Alb");

    // The stick hiccups: that document is unreadable when the check runs.
    fs::write(other.join("_Other.noudata"), b"{ torn").unwrap();
    assert_eq!(put_covers(port, &id, json!([])).status, 200);
    assert!(dir.join("a.jpg").is_file(), "cannot prove nobody uses it: kept");
}
