//! NB_SEC review: portal hardening and upload sniffing over real HTTP.

mod common;

use plippa_server::{RecordingIpc, ServerConfig, ServerService};
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
    fs::write(root.join(".gitkeep"), b"").unwrap();
    (tmp, root)
}

fn start_http(root: &Path) -> u16 {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
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

/// Raw request: returns (status, head, body). `declared_len` overrides Content-Length.
fn raw(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    declared_len: Option<usize>,
) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let has_host = headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host"));
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    let host = if has_host { "" } else { "Host: 127.0.0.1\r\n" };
    write!(
        s,
        "{method} {path} HTTP/1.1\r\n{host}Connection: close\r\n{extra}Content-Length: {}\r\n\r\n",
        declared_len.unwrap_or(body.len())
    )
    .unwrap();
    let _ = s.write_all(body);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no response: {:?}", String::from_utf8_lossy(&out)));
    let head = String::from_utf8_lossy(&out[..split]).to_string();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head, out[split + 4..].to_vec())
}

fn files_under(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else {
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut v = Vec::new();
    walk(root, root, &mut v);
    v.sort();
    v
}

#[test]
fn nb_sec_quick_import_sniffs_content_and_ignores_the_client_extension() {
    let (_t, root) = library();
    let port = start_http(&root);
    let before = files_under(&root);
    let x = [("X-Plippa", "1")];
    for (name, body) in [
        ("x.mp3", &b"<html><script>alert(1)</script></html>"[..]),
        ("x.wav", b"MZ\x90\x00 an executable"),
        ("x.svg", b"<svg onload=alert(1)/>"),
        ("evil.noudata", b"{\"a\":1}"),
        ("run.sh", b"#!/bin/sh\nrm -rf /\n"),
    ] {
        let path = format!("/api/media?kind=music&filename={name}");
        let (st, _, _) = raw(port, "POST", &path, &x, body, None);
        assert_eq!(st, 415, "{name} must be refused as not audio");
    }
    assert_eq!(
        files_under(&root),
        before,
        "nothing may be written on rejection"
    );

    // Real audio bytes under a hostile name: stored with the sniffed extension, inside the root.
    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&36u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&[
        16, 0, 0, 0, 1, 0, 1, 0, 0x40, 0x1f, 0, 0, 0x40, 0x1f, 0, 0, 1, 0, 8, 0,
    ]);
    wav.extend_from_slice(b"data\0\0\0\0");
    let path = "/api/media?kind=music&filename=..%2F..%2Fevil.html";
    let (st, head, _) = raw(port, "POST", path, &x, &wav, None);
    assert_eq!(st, 200, "{head}");
    for f in files_under(&root) {
        assert!(!f.ends_with(".html"), "{f}");
        assert!(!f.contains(".."), "{f}");
    }
}

#[test]
fn nb_sec_state_changes_need_json_or_x_plippa_and_same_origin_host() {
    let (_t, root) = library();
    let port = start_http(&root);
    let body = br#"{"title":"x"}"#;
    // No content type / custom header (what a cross-site <form> can send).
    let (st, _, _) = raw(
        port,
        "POST",
        "/api/playlists",
        &[("Content-Type", "text/plain")],
        body,
        None,
    );
    assert_eq!(st, 415);
    let (st, _, _) = raw(
        port,
        "POST",
        "/api/playlists",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        body,
        None,
    );
    assert_eq!(st, 415);
    for origin in [
        "null",
        "http://evil.example",
        "http://127.0.0.1.evil.example",
        "file://",
    ] {
        let (st, _, _) = raw(
            port,
            "POST",
            "/api/rescan",
            &[("X-Plippa", "1"), ("Origin", origin)],
            b"",
            None,
        );
        assert_eq!(st, 403, "Origin {origin}");
    }
    // DNS rebinding: attacker name in Host (and matching Origin).
    let (st, _, _) = raw(
        port,
        "POST",
        "/api/rescan",
        &[
            ("X-Plippa", "1"),
            ("Host", "rebind.attacker.example"),
            ("Origin", "http://rebind.attacker.example"),
        ],
        b"",
        None,
    );
    assert_eq!(st, 403);
    let (st, _, _) = raw(
        port,
        "GET",
        "/api/status",
        &[("Host", "rebind.attacker.example")],
        b"",
        None,
    );
    assert_eq!(st, 403);
    // GET never changes state: a rescan must not be reachable through GET.
    let (st, _, _) = raw(port, "GET", "/api/rescan", &[], b"", None);
    assert_eq!(st, 404);
}

#[test]
fn nb_sec_every_response_carries_the_hardening_headers_and_csp_has_no_inline() {
    let (_t, root) = library();
    let port = start_http(&root);
    for (m, p, h) in [
        ("GET", "/", vec![]),
        ("GET", "/app.js", vec![]),
        ("GET", "/library.js", vec![]),
        ("GET", "/ui.js", vec![]),
        ("GET", "/signin.js", vec![]),
        ("GET", "/addmedia.js", vec![]),
        ("GET", "/editcues.js", vec![]),
        ("GET", "/editcovers.js", vec![]),
        ("GET", "/api/status", vec![]),
        ("GET", "/nope", vec![]),
        ("POST", "/api/rescan", vec![]),
        ("OPTIONS", "/api/media", vec![]),
        ("GET", "/api/collections/999/cover", vec![]),
    ] {
        let (_, head, _) = raw(port, m, p, &h, b"", None);
        let l = head.to_ascii_lowercase();
        assert!(
            l.contains("x-content-type-options: nosniff"),
            "{m} {p}: {head}"
        );
        assert!(
            l.contains("referrer-policy: no-referrer"),
            "{m} {p}: {head}"
        );
        let csp = l
            .lines()
            .find(|x| x.starts_with("content-security-policy:"))
            .unwrap_or_else(|| panic!("{m} {p}: {head}"));
        assert!(
            !csp.contains("unsafe-inline") && !csp.contains("unsafe-eval"),
            "{csp}"
        );
        assert!(
            csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"),
            "{csp}"
        );
        assert!(!l.contains("access-control-allow-origin"), "{head}");
    }
}

#[test]
fn nb_sec_oversize_and_lying_bodies_are_refused_without_reading() {
    let (_t, root) = library();
    let port = start_http(&root);
    let j = [("Content-Type", "application/json")];
    // Announced bodies over the endpoint cap: 413 before any read.
    let (st, _, _) = raw(port, "POST", "/api/web/fetch", &j, b"{}", Some(5000));
    assert_eq!(st, 413);
    let (st, _, _) = raw(
        port,
        "POST",
        "/api/playlists",
        &j,
        b"{}",
        Some(2 * 1024 * 1024),
    );
    assert_eq!(st, 413);
    let (st, _, _) = raw(port, "PATCH", "/api/playables/1", &j, b"{}", Some(100_000));
    assert_eq!(st, 413);
    let (st, _, _) = raw(
        port,
        "POST",
        "/api/media?kind=music&filename=a.wav",
        &[("X-Plippa", "1")],
        b"RIFF",
        // NB_11: uploads are streamed to disk, so the cap is the streamed-file limit (4 GiB - 1,
        // the FAT32 ceiling, by default since SECFIX_01), no longer the old 256 MiB in-RAM limit.
        Some(5 * 1024 * 1024 * 1024),
    );
    assert_eq!(st, 413);
    // Malformed JSON never panics the server.
    for body in [
        &b"{"[..],
        b"[]",
        b"null",
        b"\xff\xfe",
        b"{\"noubinKey\":1}",
        b"{\"consent\":\"yes\"}",
    ] {
        let (st, _, _) = raw(port, "POST", "/api/web/fetch", &j, body, None);
        assert!((400..500).contains(&st), "{st}");
    }
    let (st, _, _) = raw(port, "GET", "/api/status", &[], b"", None);
    assert_eq!(st, 200, "server still alive");
}

#[test]
fn nb_sec_web_fetch_of_private_targets_is_refused_before_any_network() {
    let (_t, root) = library();
    let port = start_http(&root);
    let j = [("Content-Type", "application/json")];
    for key in [
        "https://127.0.0.1/x",
        "http://169.254.169.254/latest/meta-data/",
        "https://localhost/x",
        "https://[::ffff:10.0.0.1]/x",
        "https://192.168.0.1:443/x",
        "https://2130706433/",
        "https://evil.example@127.0.0.1/",
        "file:///etc/passwd",
    ] {
        let body = format!(
            r#"{{"noubinKey":{},"consent":true}}"#,
            serde_json::json!(key)
        );
        let (st, _, resp) = raw(port, "POST", "/api/web/fetch", &j, body.as_bytes(), None);
        let text = String::from_utf8_lossy(&resp).to_string();
        assert!(matches!(st, 400 | 409 | 502 | 503), "{key}: {st} {text}");
        assert!(
            text.contains("bad_url")
                || text.contains("blocked_target")
                || text.contains("not_a_url")
                || st == 503,
            "{key}: {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// Sniffers on hostile bytes: no panic, and dimension limits hold without decoding.
// ---------------------------------------------------------------------------

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&13u32.to_be_bytes());
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 2, 0, 0, 0]);
    v.extend_from_slice(&[0; 4]);
    v
}

#[test]
fn nb_sec_image_sniffer_refuses_pixel_bombs_and_survives_mutations() {
    use plippa_server::unite::sniff::{sniff_audio, sniff_image, sniff_image_kind};
    // Tiny file, absurd dimensions (decompression bomb): refused from the header alone.
    for (w, h) in [
        (5001, 10),
        (10, 5001),
        (u32::MAX, u32::MAX),
        (0, 0),
        (65535, 65535),
        (1 << 31, 1),
    ] {
        assert!(sniff_image(&png(w, h), 5000).is_err(), "{w}x{h}");
    }
    assert!(sniff_image(&png(500, 500), 5000).is_ok());

    // Deterministic mutation fuzz over image and audio headers.
    let seeds: Vec<Vec<u8>> = vec![
        png(100, 100),
        b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xc0\x00\x11\x08\x00\x10\x00\x10\x03\x01\x11\x00\x02\x11\x01\x03\x11\x01".to_vec(),
        b"RIFF\x24\x00\x00\x00WEBPVP8 \x18\x00\x00\x00\x00\x00\x00\x00\x9d\x01\x2a\x10\x00\x10\x00".to_vec(),
        b"RIFF\x24\x00\x00\x00WEBPVP8X\x0a\x00\x00\x00\x00\x00\x00\x00\x0f\x00\x00\x0f\x00\x00".to_vec(),
        b"RIFF\x24\x00\x00\x00WEBPVP8L\x05\x00\x00\x00\x2f\x0f\x00\x00\x00".to_vec(),
        b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x01\x1e\x01vorbis".to_vec(),
        b"\x00\x00\x00\x20ftypM4A \x00\x00\x00\x00M4A mp42isom".to_vec(),
        b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f".to_vec(),
        b"fLaC\x00\x00\x00\x22".to_vec(),
    ];
    let mut x: u64 = 0x1234_5678_9abc_def1;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for seed in &seeds {
        for _ in 0..3000 {
            let mut b = seed.clone();
            for _ in 0..(1 + rnd() % 4) {
                let i = (rnd() % b.len() as u64) as usize;
                b[i] = rnd() as u8;
            }
            if rnd() % 3 == 0 {
                b.truncate((rnd() % (b.len() as u64 + 1)) as usize);
            }
            if rnd() % 5 == 0 {
                b.extend((0..(rnd() % 40)).map(|_| rnd() as u8));
            }
            let _ = sniff_audio(&b);
            let _ = sniff_image_kind(&b);
            let _ = sniff_image(&b, 5000);
        }
    }
}

#[test]
fn nb_sec_tag_reader_survives_hostile_containers() {
    // Hostile ID3 / RIFF / fLaC claiming huge sizes must not panic or allocate gigabytes.
    let (_t, root) = library();
    let port = start_http(&root);
    let x = [("X-Plippa", "1")];
    let mut id3 = b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f".to_vec(); // ~256 MiB tag claimed
    id3.extend_from_slice(b"TIT2\x7f\x7f\x7f\x7f\x00\x00\x03");
    id3.extend_from_slice(&[0xff; 64]);
    let mut riff = b"RIFF\xff\xff\xff\xffWAVEfmt \xff\xff\xff\xff".to_vec();
    riff.extend_from_slice(&[0u8; 32]);
    let mut flac = b"fLaC\x00\xff\xff\xff".to_vec();
    flac.extend_from_slice(&[0u8; 32]);
    for (name, body) in [("a.mp3", id3), ("b.wav", riff), ("c.flac", flac)] {
        let path = format!("/api/media?kind=music&filename={name}");
        let (st, _, _) = raw(port, "POST", &path, &x, &body, None);
        assert!(st == 200 || st == 400 || st == 415, "{name}: {st}");
    }
    let (st, _, _) = raw(port, "GET", "/api/status", &[], b"", None);
    assert_eq!(st, 200, "server still alive");
}

#[test]
fn nb_sec_cover_endpoint_does_not_follow_symlinked_folders_out_of_the_library() {
    // A stick prepared by someone else: `Art` inside the album folder is a symlink to a folder
    // outside the library, and the release points its cover at `Art/secret.png`.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/plippa-noubin/tests/fixtures/libraries/kinds");
    fn copy(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).unwrap();
        for e in fs::read_dir(src).unwrap().flatten() {
            let to = dst.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy(&e.path(), &to);
            } else {
                fs::copy(e.path(), to).unwrap();
            }
        }
    }
    copy(&src, &root);
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.png"), png(10, 10)).unwrap();
    let album = root.join("Music/Band/Album");
    std::os::unix::fs::symlink(&outside, album.join("Art")).unwrap();
    let nd = album.join("_Album.noudata");
    let text = fs::read_to_string(&nd)
        .unwrap()
        .replace("cover.jpg", "Art/secret.png");
    fs::write(&nd, text).unwrap();

    let port = start_http(&root);
    let (_, _, body) = raw(port, "GET", "/api/collections?kind=album", &[], b"", None);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let cover = v["collections"][0]["cover"].as_str().map(str::to_string);
    if let Some(url) = cover {
        let (st, _, body) = raw(port, "GET", &url, &[], b"", None);
        assert_eq!(st, 404, "must not serve through a symlinked folder");
        assert!(!body.starts_with(b"\x89PNG"));
    }
}
