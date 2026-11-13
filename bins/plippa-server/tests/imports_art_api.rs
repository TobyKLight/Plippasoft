//! ART_04: embedded-art extraction, dedupe and the import protocol's art half — previews from
//! staging, `finish` writing images / thumbnails / `localCoverImage`, single-add sessions.
//!
//! Audio and art are generated here: ID3v2-tagged MP3 and a FLAC with PICTURE through lofty's
//! writer, JPEGs through `plippa_image::encode_jpeg_fit`, a PNG from the ART_02 fixtures.

mod common;

use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::{Tag, TagType};
use plippa_image::{encode_jpeg_fit, probe, ImageLimits, Rgb8};
use plippa_server::{RecordingIpc, ServerConfig, ServerService};
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PNG_FIXTURE: &[u8] = include_bytes!("../../../crates/plippa-image/tests/fixtures/rgba_64x32.png");

// ---------------------------------------------------------------------------
// Server and HTTP helpers
// ---------------------------------------------------------------------------

fn library() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    (tmp, root)
}

fn start_http(root: &Path) -> u16 {
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
    common::serve_open(cfg, svc);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return port;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

/// `(status, content type, body)`.
fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
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
    let ctype = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-type:").map(|v| v.trim().to_string()))
        .unwrap_or_default();
    let body = raw[split + 4..].to_vec();
    let chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
    (status, ctype, if chunked { dechunk(&body) } else { body })
}

/// Undo `Transfer-Encoding: chunked` (large bodies are sent that way).
fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = b.windows(2).position(|w| w == b"\r\n") else { break };
        let size = usize::from_str_radix(String::from_utf8_lossy(&b[..eol]).trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.extend_from_slice(&b[eol + 2..eol + 2 + size]);
        b = &b[eol + 2 + size + 2..];
    }
    out
}

fn req(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, Value) {
    let (st, _, b) = http(port, method, path, headers, body);
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
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

fn begin(port: u16, single: bool) -> String {
    let body = json!({ "single": single }).to_string();
    let (st, v) = if single {
        req(port, "POST", "/api/imports", &[JSON], body.as_bytes())
    } else {
        req(port, "POST", "/api/imports", &[X], b"")
    };
    assert_eq!(st, 200, "{v}");
    v["id"].as_str().unwrap().to_string()
}

fn upload(port: u16, sid: &str, name: &str, body: &[u8]) -> (u16, Value) {
    req(port, "POST", &format!("/api/imports/{sid}/files?name={}", pct(name)), &[X], body)
}

fn finish_with(port: u16, sid: &str, body: Value) -> (u16, Value) {
    req(port, "POST", &format!("/api/imports/{sid}/finish"), &[JSON], body.to_string().as_bytes())
}

fn art_len(v: &Value) -> usize {
    v["art"].as_array().unwrap().len()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A 9 x 8 grid of pseudo-random greys scaled to `w` x `h` as a JPEG: every size of one `seed`
/// is the same picture, different seeds are different pictures.
fn blocks(seed: u32, w: u32, h: u32) -> Vec<u8> {
    let mut cell = [0u8; 72];
    let mut s = seed.wrapping_mul(2654435761).wrapping_add(12345);
    for c in cell.iter_mut() {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        *c = (s >> 24) as u8;
    }
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let v = cell[((y * 8 / h) * 9 + x * 9 / w) as usize];
            px.extend_from_slice(&[v, v, v]);
        }
    }
    encode_jpeg_fit(&Rgb8 { w, h, px }, w.max(h)).unwrap()
}

fn dims(bytes: &[u8]) -> (u32, u32) {
    let (_, w, h) = probe(bytes, &ImageLimits::default()).unwrap();
    (w, h)
}

fn picture(t: PictureType, mime: MimeType, data: &[u8]) -> Picture {
    Picture::new_unchecked(t, Some(mime), None, data.to_vec())
}

fn jpeg_pic(t: PictureType, data: &[u8]) -> Picture {
    picture(t, MimeType::Jpeg, data)
}

fn silent_mp3_frames() -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..8 {
        let mut f = vec![0u8; 417];
        f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        v.extend(f);
    }
    v
}

/// A tagged MP3 (`title` makes every file's bytes differ) carrying `pics`.
fn mp3(title: &str, pics: Vec<Picture>) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.mp3");
    fs::write(&p, silent_mp3_frames()).unwrap();
    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title(title.into());
    tag.set_artist("Band".into());
    for pic in pics {
        tag.push_picture(pic);
    }
    tag.save_to_path(&p, WriteOptions::default()).unwrap();
    fs::read(p).unwrap()
}

/// A header-only FLAC (STREAMINFO, no frames) with a PICTURE block.
fn flac(title: &str, pics: Vec<Picture>) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.flac");
    let mut v = b"fLaC".to_vec();
    v.extend_from_slice(&[0x00, 0, 0, 34]);
    v.extend_from_slice(&4096u16.to_be_bytes());
    v.extend_from_slice(&4096u16.to_be_bytes());
    v.extend_from_slice(&[0; 6]);
    let packed: u64 = (44100u64 << 44) | (1 << 41) | (15 << 36) | 44100;
    v.extend_from_slice(&packed.to_be_bytes());
    v.extend_from_slice(&[0; 16]);
    v.extend_from_slice(&[0x81, 0, 0, 8]); // last block: PADDING, so lofty can rewrite the tags
    v.extend_from_slice(&[0; 8]);
    fs::write(&p, v).unwrap();
    let mut tag = Tag::new(TagType::VorbisComments);
    tag.set_title(title.into());
    tag.set_artist("Band".into());
    for pic in pics {
        tag.push_picture(pic);
    }
    tag.save_to_path(&p, WriteOptions::default()).unwrap();
    fs::read(p).unwrap()
}

fn wav(tag: u8) -> Vec<u8> {
    let data = vec![tag; 8000];
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&8000u32.to_le_bytes());
    v.extend_from_slice(&8000u32.to_le_bytes());
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

/// Everything under `<root>/.plippa/tmp`.
fn tmp_entries(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(d: &Path, root: &Path, out: &mut Vec<String>) {
        if let Ok(rd) = fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                if p.is_dir() {
                    walk(&p, root, out);
                }
            }
        }
    }
    walk(&root.join(".plippa/tmp"), root, &mut out);
    out.sort();
    out
}

fn read_doc(p: &Path) -> Value {
    serde_json::from_slice(&fs::read(p).unwrap()).unwrap()
}

fn names_of(images: &Value) -> Vec<String> {
    images["images"]
        .as_array()
        .map(|a| a.iter().map(|i| i["imageLocalFilename"].as_str().unwrap().to_string()).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Extraction and dedupe
// ---------------------------------------------------------------------------

#[test]
fn ten_tracks_with_the_same_embedded_cover_preview_one_image() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    let cover = blocks(1, 600, 600);
    let mut last = Value::Null;
    for i in 0..10 {
        let (st, v) = upload(port, &sid, &format!("t{i}.mp3"), &mp3(&format!("T{i}"), vec![jpeg_pic(PictureType::CoverFront, &cover)]));
        assert_eq!(st, 200, "{v}");
        assert_eq!(art_len(&v), 1, "after file {i}");
        last = v;
    }
    assert_eq!(last["merged"].as_array().unwrap().len(), 9);
    assert!(last["merged"].as_array().unwrap().iter().all(|m| m["exact"] == true));
    // staged once on disk, and the preview serves it
    let stage_art: Vec<String> = tmp_entries(&root).into_iter().filter(|e| e.contains("/art/")).collect();
    assert_eq!(stage_art.len(), 1, "{stage_art:?}");
    let url = last["art"][0]["url"].as_str().unwrap();
    let (st, ctype, body) = http(port, "GET", url, &[], b"");
    assert_eq!((st, ctype.as_str(), body), (200, "image/jpeg", cover));
}

#[test]
fn ten_plus_two_tracks_with_a_different_back_cover_preview_two_images_front_first() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    let front = blocks(1, 500, 500);
    let back = blocks(2, 500, 500);
    let mut last = Value::Null;
    for i in 0..10 {
        let (_, v) = upload(port, &sid, &format!("t{i}.mp3"), &mp3(&format!("T{i}"), vec![jpeg_pic(PictureType::CoverFront, &front)]));
        last = v;
    }
    for i in 10..12 {
        // back cover listed first in the file: front still comes first
        let pics = vec![jpeg_pic(PictureType::CoverBack, &back), jpeg_pic(PictureType::CoverFront, &front)];
        let (st, v) = upload(port, &sid, &format!("t{i}.flac"), &flac(&format!("T{i}"), pics));
        assert_eq!(st, 200, "{v}");
        last = v;
    }
    assert_eq!(art_len(&last), 2);
    let (_, _, first) = http(port, "GET", last["art"][0]["url"].as_str().unwrap(), &[], b"");
    let (_, _, second) = http(port, "GET", last["art"][1]["url"].as_str().unwrap(), &[], b"");
    assert_eq!((first, second), (front, back));
}

#[test]
fn the_same_cover_at_two_sizes_previews_once_as_the_larger() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    for i in 0..3 {
        upload(port, &sid, &format!("s{i}.mp3"), &mp3(&format!("S{i}"), vec![jpeg_pic(PictureType::CoverFront, &blocks(7, 600, 600))]));
    }
    let mut last = Value::Null;
    for i in 0..3 {
        let (_, v) = upload(port, &sid, &format!("b{i}.mp3"), &mp3(&format!("B{i}"), vec![jpeg_pic(PictureType::CoverFront, &blocks(7, 1000, 1000))]));
        last = v;
    }
    assert_eq!(art_len(&last), 1);
    assert_eq!((last["art"][0]["w"].as_u64(), last["art"][0]["h"].as_u64()), (Some(1000), Some(1000)));
    let (st, ct, body) = http(port, "GET", last["art"][0]["url"].as_str().unwrap(), &[], b"");
    assert_eq!((st, ct.as_str()), (200, "image/jpeg"), "{}", String::from_utf8_lossy(&body[..body.len().min(200)]));
    assert_eq!(dims(&body), (1000, 1000));
    assert!(last["merged"].as_array().unwrap().iter().any(|m| m["exact"] == false));
}

#[test]
fn dropped_images_join_dedupe_and_a_removed_picture_stays_removed() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    let (st, v) = upload(port, &sid, "t1.wav", &wav(1));
    assert_eq!((st, art_len(&v)), (200, 0));
    let jpg = blocks(3, 400, 400);
    let (st, v) = upload(port, &sid, "front.jpg", &jpg);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["kind"], "image");
    assert!(v.get("file_id").is_none());
    let (_, v) = upload(port, &sid, "front-again.jpg", &jpg); // dropped twice
    assert_eq!(art_len(&v), 1);
    let (_, v) = upload(port, &sid, "logo.png", PNG_FIXTURE);
    assert_eq!(art_len(&v), 2);
    assert_eq!(v["art"][1]["kind"], "png");

    // delete the jpg, then a track embedding the same picture arrives: still gone
    let jpg_id = v["art"][0]["art_id"].as_str().unwrap().to_string();
    let (st, v) = req(port, "DELETE", &format!("/api/imports/{sid}/art/{jpg_id}"), &[X], b"");
    assert_eq!((st, art_len(&v)), (200, 1));
    let (_, v) = upload(port, &sid, "t2.mp3", &mp3("T2", vec![jpeg_pic(PictureType::CoverFront, &jpg)]));
    assert_eq!(art_len(&v), 1);
    assert_eq!(v["art"][0]["kind"], "png");
    // an unknown id is a 404; the preview of a removed image is still addressable by a live id
    let (st, _) = req(port, "DELETE", &format!("/api/imports/{sid}/art/ffff"), &[X], b"");
    assert_eq!(st, 404);
}

#[test]
fn art_order_can_be_set_and_unknown_ids_are_refused() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    upload(port, &sid, "a.jpg", &blocks(1, 300, 300));
    let (_, v) = upload(port, &sid, "b.jpg", &blocks(2, 300, 300));
    let (a, b) = (v["art"][0]["art_id"].as_str().unwrap(), v["art"][1]["art_id"].as_str().unwrap());
    let path = format!("/api/imports/{sid}/art/order");
    let (st, v) = req(port, "PUT", &path, &[JSON], json!({"order": [b, a]}).to_string().as_bytes());
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["art"][0]["art_id"], b);
    let (st, _) = req(port, "PUT", &path, &[JSON], json!({"order": ["nope"]}).to_string().as_bytes());
    assert_eq!(st, 400);
}

#[test]
fn unusable_images_are_refused_with_a_message_and_register_nothing() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    let (st, v) = upload(port, &sid, "x.gif", b"GIF89a\x01\x00\x01\x00\x00\x00\x00;");
    assert_eq!(st, 415, "{v}");
    let mut cut = blocks(1, 300, 300);
    cut.truncate(cut.len() / 2);
    let (st, _) = upload(port, &sid, "cut.jpg", &cut);
    assert!(st == 400 || st == 415, "{st}");
    let (st, v) = upload(port, &sid, "t.wav", &wav(1));
    assert_eq!((st, art_len(&v)), (200, 0));
}

// ---------------------------------------------------------------------------
// Hostile embedded art
// ---------------------------------------------------------------------------

#[test]
fn hostile_embedded_pictures_are_skipped_and_the_upload_still_succeeds() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    let good = blocks(1, 300, 300);

    // a GIF claiming to be a JPEG, plus a good one
    let gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;".to_vec();
    let (st, v) = upload(port, &sid, "gif.mp3", &mp3("G", vec![jpeg_pic(PictureType::CoverFront, &gif), jpeg_pic(PictureType::Other, &good)]));
    assert_eq!(st, 200, "{v}");
    assert_eq!((art_len(&v), v["art_skipped"].as_u64()), (1, Some(1)));

    // a truncated JPEG: header fine, pixels missing
    let mut cut = blocks(2, 400, 400);
    cut.truncate(cut.len() / 3);
    let (st, v) = upload(port, &sid, "cut.mp3", &mp3("C", vec![jpeg_pic(PictureType::CoverFront, &cut)]));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["art_skipped"].as_u64(), Some(1));
    assert_eq!(art_len(&v), 1);

    // 50 distinct pictures in one file: capped at 8, the rest reported
    let pics: Vec<Picture> = (0..50).map(|i| jpeg_pic(PictureType::Other, &blocks(100 + i, 64, 64))).collect();
    let (st, v) = upload(port, &sid, "many.mp3", &mp3("M", pics));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["art_skipped"].as_u64(), Some(42));
    assert_eq!(art_len(&v), 1 + 8);
}

#[test]
fn an_apic_frame_claiming_200_mb_is_survived() {
    // ID3v2.4 tag whose only frame declares 200 MB of picture inside a 40-byte tag.
    let mut tag = b"ID3\x04\x00\x00".to_vec();
    let body_len = 10 + 30usize;
    tag.extend_from_slice(&[0, 0, ((body_len >> 7) & 0x7f) as u8, (body_len & 0x7f) as u8]);
    tag.extend_from_slice(b"APIC");
    let claimed: u32 = 200 * 1024 * 1024;
    tag.extend_from_slice(&[
        ((claimed >> 21) & 0x7f) as u8,
        ((claimed >> 14) & 0x7f) as u8,
        ((claimed >> 7) & 0x7f) as u8,
        (claimed & 0x7f) as u8,
    ]);
    tag.extend_from_slice(&[0, 0]);
    tag.extend_from_slice(&[0u8; 30]);
    tag.extend(silent_mp3_frames());
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    let (st, v) = upload(port, &sid, "evil.mp3", &tag);
    assert_eq!(st, 200, "{v}");
    assert_eq!(art_len(&v), 0);
    assert!(v["file_id"].as_u64().is_some());
}

// ---------------------------------------------------------------------------
// finish
// ---------------------------------------------------------------------------

#[test]
fn finish_without_audio_is_400_and_creates_nothing_but_keeps_the_staging() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    upload(port, &sid, "a.jpg", &blocks(1, 300, 300));
    let before = tmp_entries(&root);
    let (st, v) = finish_with(port, &sid, json!({"mode": "album", "name": "Art only", "order": []}));
    assert_eq!(st, 400, "{v}");
    assert_eq!(v["error"], "no playable media");
    assert_eq!(files(&root), Vec::<String>::new());
    assert_eq!(tmp_entries(&root), before);
    // the session works on
    let (_, v) = upload(port, &sid, "t.wav", &wav(1));
    let fid = v["file_id"].as_u64().unwrap();
    let (st, v) = finish_with(port, &sid, json!({"mode": "album", "name": "Now with audio", "order": [fid]}));
    assert_eq!(st, 200, "{v}");
}

#[test]
fn album_finish_writes_images_thumbnails_and_local_cover_image() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    let front = blocks(1, 600, 600);
    let back = blocks(2, 600, 600);
    let mut fids = Vec::new();
    for i in 0..3 {
        let pics = if i == 2 {
            vec![jpeg_pic(PictureType::CoverFront, &front), jpeg_pic(PictureType::CoverBack, &back)]
        } else {
            vec![jpeg_pic(PictureType::CoverFront, &front)]
        };
        let (_, v) = upload(port, &sid, &format!("t{i}.mp3"), &mp3(&format!("T{i}"), pics));
        fids.push(v["file_id"].as_u64().unwrap());
    }
    let (st, v) = finish_with(port, &sid, json!({"mode": "album", "name": "Two Covers", "order": fids}));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["images"], 2);
    let dir = root.join("Music/Band/Two Covers");
    let all = files(&root);
    for want in [
        "Music/Band/Two Covers/cover.jpg",
        "Music/Band/Two Covers/cover-2.jpg",
        "Music/Band/Two Covers/thumbnails/cover-128.jpg",
        "Music/Band/Two Covers/thumbnails/cover-2-128.jpg",
        "Music/Band/Two Covers/_Two Covers.noudata",
    ] {
        assert!(all.contains(&want.to_string()), "{want} missing in {all:?}");
    }
    assert_eq!(fs::read(dir.join("cover.jpg")).unwrap(), front);
    assert_eq!(fs::read(dir.join("cover-2.jpg")).unwrap(), back);
    let doc = read_doc(&dir.join("_Two Covers.noudata"));
    assert_eq!(names_of(&doc["localPlaybackData"]["localCoverImage"]), ["cover.jpg", "cover-2.jpg"]);
    // the portal sees them
    let (st, v) = req(port, "GET", &format!("/api/collections/{}/covers", v["id"].as_str().unwrap()), &[], b"");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["covers"].as_array().unwrap().len(), 2);
    // staging is gone with the session
    assert_eq!(tmp_entries(&root), Vec::<String>::new());
}

#[test]
fn art_carried_only_by_an_omitted_audio_file_is_not_saved() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    let keep = blocks(1, 600, 600);
    let (_, a) = upload(port, &sid, "t0.mp3", &mp3("T0", vec![jpeg_pic(PictureType::CoverFront, &keep)]));
    let (_, b) = upload(port, &sid, "t1.mp3", &mp3("T1", vec![jpeg_pic(PictureType::CoverFront, &blocks(2, 600, 600))]));
    assert_eq!(art_len(&b), 2);
    let fid = a["file_id"].as_u64().unwrap();
    let (st, v) = finish_with(port, &sid, json!({"mode": "album", "name": "Left Out", "order": [fid]}));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["images"], 1);
    let dir = root.join("Music/Band/Left Out");
    assert_eq!(fs::read(dir.join("cover.jpg")).unwrap(), keep);
    assert!(!dir.join("cover-2.jpg").exists());
}

#[test]
fn the_same_image_dropped_many_times_stays_one_candidate_and_answers_quickly() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    let img = blocks(1, 300, 300);
    let t = std::time::Instant::now();
    for _ in 0..200 {
        let (st, v) = upload(port, &sid, "same.jpg", &img);
        assert_eq!(st, 200, "{v}");
        assert_eq!(art_len(&v), 1);
    }
    assert!(t.elapsed() < Duration::from_secs(60), "{:?}", t.elapsed());
    let (st, v) = upload(port, &sid, "other.jpg", &blocks(2, 300, 300));
    assert_eq!(st, 200, "{v}");
    assert_eq!(art_len(&v), 2, "the budget still has room for a different image");
}

#[test]
fn finish_honours_the_art_list_removal_and_order() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    let (_, v) = upload(port, &sid, "t.wav", &wav(1));
    let fid = v["file_id"].as_u64().unwrap();
    upload(port, &sid, "a.jpg", &blocks(1, 300, 300));
    upload(port, &sid, "b.jpg", &blocks(2, 300, 300));
    let (_, v) = upload(port, &sid, "c.jpg", &blocks(3, 300, 300));
    let a: Vec<&str> = v["art"].as_array().unwrap().iter().map(|x| x["art_id"].as_str().unwrap()).collect();
    // remove b via DELETE, then finish with c before a
    req(port, "DELETE", &format!("/api/imports/{sid}/art/{}", a[1]), &[X], b"");
    let (st, bad) = finish_with(port, &sid, json!({"mode": "album", "name": "Sel", "order": [fid], "art": [a[1]]}));
    assert_eq!(st, 400, "removed image listed: {bad}");
    let (st, v) = finish_with(port, &sid, json!({"mode": "album", "name": "Sel", "order": [fid], "art": [a[2], a[0]]}));
    assert_eq!(st, 200, "{v}");
    let dir = root.join("Music/Unknown Artist/Sel");
    assert_eq!(fs::read(dir.join("cover.jpg")).unwrap(), blocks(3, 300, 300));
    assert_eq!(fs::read(dir.join("cover-2.jpg")).unwrap(), blocks(1, 300, 300));
    assert!(!dir.join("cover-3.jpg").exists());
}

#[test]
fn playlist_finish_puts_the_art_on_the_playlist_and_names_never_collide() {
    let (_t, root) = library();
    let port = start_http(&root);
    let mut made = Vec::new();
    for name in ["Mix One", "Mix Two"] {
        let sid = begin(port, false);
        let (_, v) = upload(port, &sid, &format!("{name}.mp3"), &mp3(name, vec![jpeg_pic(PictureType::CoverFront, &blocks(9, 300, 300))]));
        let fid = v["file_id"].as_u64().unwrap();
        upload(port, &sid, "extra.jpg", &blocks(if name == "Mix One" { 4 } else { 5 }, 300, 300));
        let (st, v) = finish_with(port, &sid, json!({"mode": "playlist", "name": name, "order": [fid]}));
        assert_eq!(st, 200, "{v}");
        made.push(v["noudata_relpath"].as_str().unwrap().to_string());
    }
    let pl = files(&root).into_iter().filter(|f| f.starts_with("Playlists/")).collect::<Vec<_>>();
    let covers: Vec<&String> = pl.iter().filter(|f| f.contains("-cover") && !f.contains("thumbnails")).collect();
    assert_eq!(covers.len(), 4, "{pl:?}");
    let mut uniq = covers.clone();
    uniq.dedup();
    assert_eq!(uniq.len(), 4);
    for rel in &made {
        let doc = read_doc(&root.join(rel));
        let names = names_of(&doc["localPlaybackData"]["localCoverImage"]);
        assert_eq!(names.len(), 2, "{rel}: {doc}");
        assert!(names.iter().all(|n| n.contains("-cover")));
        // the track keeps no copy of the art
        let track_art: Vec<String> = files(&root).into_iter().filter(|f| f.starts_with("Music/") && !f.ends_with(".mp3") && !f.ends_with(".noudata")).collect();
        assert!(track_art.is_empty(), "{track_art:?}");
    }
}

#[test]
fn a_failure_after_the_images_were_placed_leaves_no_images_no_thumbnails_and_no_tracks() {
    let (_t, root) = library();
    // `Playlists/thumbnails` is a file, so the thumbnail step fails after the images were moved in.
    fs::create_dir_all(root.join("Playlists")).unwrap();
    fs::write(root.join("Playlists/thumbnails"), b"in the way").unwrap();
    let before = files(&root);
    let port = start_http(&root);
    let sid = begin(port, false);
    let (_, v) = upload(port, &sid, "t.wav", &wav(1));
    let fid = v["file_id"].as_u64().unwrap();
    upload(port, &sid, "a.jpg", &blocks(1, 300, 300));
    let (st, v) = finish_with(port, &sid, json!({"mode": "playlist", "name": "Doomed", "order": [fid]}));
    assert!(st >= 400, "{st} {v}");
    assert_eq!(files(&root), before, "nothing is left in the library");
    // the staged files are back (or still there) and the session is usable
    let staged = tmp_entries(&root);
    assert!(staged.iter().any(|e| e.contains("/art/")), "{staged:?}");
    fs::remove_file(root.join("Playlists/thumbnails")).unwrap();
    let (st, v) = finish_with(port, &sid, json!({"mode": "playlist", "name": "Doomed", "order": [fid]}));
    assert_eq!(st, 200, "{v}");
    assert!(files(&root).iter().any(|f| f.contains("-cover") && f.starts_with("Playlists/")));
}

#[test]
fn discarding_a_session_removes_its_art() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    upload(port, &sid, "t.mp3", &mp3("T", vec![jpeg_pic(PictureType::CoverFront, &blocks(1, 300, 300))]));
    upload(port, &sid, "a.jpg", &blocks(2, 300, 300));
    assert!(tmp_entries(&root).iter().any(|e| e.contains("/art/")));
    let (st, _) = req(port, "DELETE", &format!("/api/imports/{sid}"), &[X], b"");
    assert_eq!(st, 200);
    assert_eq!(tmp_entries(&root), Vec::<String>::new());
    let (st, _, _) = http(port, "GET", &format!("/api/imports/{sid}/art/aa"), &[], b"");
    assert_eq!(st, 404);
}

#[test]
fn startup_sweep_removes_an_old_sessions_art() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    upload(port, &sid, "a.jpg", &blocks(2, 300, 300));
    // a leftover of a crashed run: a staging folder no live session owns
    let ghost = root.join(".plippa/tmp/import-00000000000000000000000000000000/art");
    fs::create_dir_all(&ghost).unwrap();
    fs::write(ghost.join("a1.jpg"), b"x").unwrap();
    let _second = begin(port, false); // opening a session sweeps leftovers
    assert!(!ghost.exists());
    assert!(tmp_entries(&root).iter().any(|e| e.contains(&sid)), "the live session survives");
}

// ---------------------------------------------------------------------------
// Single add
// ---------------------------------------------------------------------------

#[test]
fn single_session_takes_one_audio_file_and_finishes_into_a_loose_track_with_item_art() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, true);
    let (st, v) = upload(port, &sid, "Solo.wav", &wav(2));
    assert_eq!(st, 200, "{v}");
    let (st, v) = upload(port, &sid, "second.wav", &wav(3));
    assert_eq!(st, 409, "{v}");
    let (st, v) = upload(port, &sid, "cover.jpg", &blocks(4, 300, 300));
    assert_eq!((st, art_len(&v)), (200, 1));
    let (st, v) = finish_with(port, &sid, json!({"kind": "bogus"}));
    assert_eq!(st, 400, "{v}");
    let (st, v) = finish_with(port, &sid, json!({"kind": "music", "title": "My Solo"}));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["images"], 1);
    let all = files(&root);
    let noudata = all.iter().find(|f| f.ends_with(".noudata")).unwrap().clone();
    let doc = read_doc(&root.join(&noudata));
    assert_eq!(doc["playableItemTitle"], "My Solo");
    assert_eq!(doc["noudataType"], "looseTrack");
    let item = names_of(&doc["mediaList"][0]["itemCoverImage"]);
    assert_eq!(item.len(), 1, "{doc}");
    assert!(item[0].contains("-cover"), "{item:?}");
    assert!(doc.get("localPlaybackData").is_none_or(|l| l.get("localCoverImage").is_none()));
    assert!(all.iter().any(|f| f.contains("thumbnails/")));
    assert_eq!(tmp_entries(&root), Vec::<String>::new());
}

#[test]
fn single_finish_without_audio_is_400_and_the_old_media_route_still_works() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, true);
    upload(port, &sid, "cover.jpg", &blocks(4, 300, 300));
    let (st, v) = finish_with(port, &sid, json!({"kind": "music"}));
    assert_eq!(st, 400, "{v}");
    assert_eq!(files(&root), Vec::<String>::new());
    let (st, v) = req(port, "POST", "/api/media?kind=music&filename=legacy.wav", &[X], &wav(1));
    assert_eq!(st, 200, "{v}");
}

#[test]
fn single_finish_with_cue_text_keeps_the_cue_report() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, true);
    upload(port, &sid, "set.wav", &wav(3));
    let (st, v) = finish_with(port, &sid, json!({"kind": "mix", "cue_text": "00:00 First\n00:01 Second"}));
    assert_eq!(st, 200, "{v}");
    assert!(v.get("cues").is_some(), "{v}");
}

#[test]
fn removing_an_uploaded_audio_file_deletes_it_and_the_art_only_it_carried() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, false);
    let shared = blocks(1, 300, 300);
    let only_b = blocks(2, 300, 300);
    let (_, a) = upload(port, &sid, "a.mp3", &mp3("A", vec![jpeg_pic(PictureType::CoverFront, &shared)]));
    let (_, b) = upload(
        port,
        &sid,
        "b.mp3",
        &mp3("B", vec![jpeg_pic(PictureType::CoverFront, &shared), jpeg_pic(PictureType::CoverBack, &only_b)]),
    );
    assert_eq!(art_len(&b), 2);
    let staged = |root: &Path| tmp_entries(root).into_iter().filter(|e| e.ends_with(".bin")).count();
    assert_eq!(staged(&root), 2);
    let b_id = b["file_id"].as_u64().unwrap();
    let (st, v) = req(port, "DELETE", &format!("/api/imports/{sid}/files/{b_id}"), &[X], b"");
    assert_eq!(st, 200, "{v}");
    assert_eq!(art_len(&v), 1, "the back cover only B carried is gone, the shared front stays");
    assert_eq!(staged(&root), 1, "B's staged file is deleted");
    assert_eq!(tmp_entries(&root).iter().filter(|e| e.contains("/art/")).count(), 1);
    // the same file again is unknown; finish refuses the removed id
    let (st, _) = req(port, "DELETE", &format!("/api/imports/{sid}/files/{b_id}"), &[X], b"");
    assert_eq!(st, 404);
    let (st, _) = req(port, "DELETE", &format!("/api/imports/{sid}/files/x"), &[X], b"");
    assert_eq!(st, 404);
    let (st, _) = finish_with(port, &sid, json!({"mode": "album", "name": "N", "order": [b_id]}));
    assert_eq!(st, 400);
    // removing the last audio file leaves the (dropped) art and a session that refuses to finish
    let a_id = a["file_id"].as_u64().unwrap();
    let (st, v) = req(port, "DELETE", &format!("/api/imports/{sid}/files/{a_id}"), &[X], b"");
    assert_eq!((st, art_len(&v)), (200, 0));
    let (st, v) = finish_with(port, &sid, json!({"mode": "album", "name": "N", "order": []}));
    assert_eq!((st, v["error"].as_str().map(|_| ())), (400, Some(())));
    // unknown session
    let (st, _) = req(port, "DELETE", &format!("/api/imports/{}/files/1", "0".repeat(32)), &[X], b"");
    assert_eq!(st, 404);
}

#[test]
fn a_single_session_can_swap_its_file_after_removing_it() {
    let (_t, root) = library();
    let port = start_http(&root);
    let sid = begin(port, true);
    let (_, a) = upload(port, &sid, "a.mp3", &mp3("A", vec![]));
    let (st, _) = upload(port, &sid, "b.mp3", &mp3("B", vec![]));
    assert_eq!(st, 409);
    let (st, _) = req(port, "DELETE", &format!("/api/imports/{sid}/files/{}", a["file_id"]), &[X], b"");
    assert_eq!(st, 200);
    let (st, _) = upload(port, &sid, "b.mp3", &mp3("B", vec![]));
    assert_eq!(st, 200);
}
