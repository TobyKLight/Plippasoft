//! SECFIX_01 (SRV-04): uploads cannot hold the server's single upload slot or the stick forever,
//! and long uploads over slow links still succeed.
//!
//! Real loopback sockets against `serve_with_limits` with shortened timeouts. The stall timeout
//! of the socket is 500 ms and the minimum-progress window is one second (16 KiB per window), so
//! "slow but progressing" means seconds of wall clock for a few megabytes, many times the stall
//! timeout, while "stalled" and "dripping" are cut off within a couple of seconds.

mod common;

use plippa_server::web::download::DownloadLimits;
use plippa_server::{
    AdmissionLimits, HttpLimits, RecordingIpc, ServerConfig, ServerService,
};
use serde_json::Value;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const X: (&str, &str) = ("X-Plippa", "1");
const STALL: Duration = Duration::from_millis(500);

struct Rig {
    port: u16,
    root: PathBuf,
    svc: Arc<Mutex<ServerService>>,
    _tmp: tempfile::TempDir,
}

fn start() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut service = ServerService::start(
        Some(root.clone()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    // Shortened progress window; free space is injected so the test does not depend on the disk
    // it happens to run on.
    service.set_download_limits(DownloadLimits {
        stall_timeout: Duration::from_millis(1000),
        min_progress_bytes: 16 * 1024,
        free_space: |_| Some(1 << 40),
        ..DownloadLimits::from_env()
    });
    let svc = Arc::new(Mutex::new(service));
    let wire = plippa_server::limits::WireLimits {
        body_stall: STALL,
        head_deadline: Duration::from_secs(5),
        read_inactivity: Duration::from_secs(5),
        keep_alive_idle: Duration::from_secs(5),
        ..Default::default()
    };
    let limits = HttpLimits::with_admission(wire, AdmissionLimits::default());
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        library_root: Some(root.clone()),
        secrets_dir: None,
        portal_url: None,
        ..ServerConfig::default()
    };
    let served = Arc::clone(&svc);
    common::serve_open_with_limits(cfg, served, limits);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Rig {
                port,
                root,
                svc,
                _tmp: tmp,
            };
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

/// A valid mono 8-bit WAV with `secs` seconds of audio at 8 kHz (8000 bytes per second).
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

/// `(status, json)` of a complete small request.
fn req(port: u16, method: &str, path: &str, body: &[u8]) -> (u16, Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{}: {}\r\nContent-Length: {}\r\n\r\n",
        X.0,
        X.1,
        body.len()
    )
    .unwrap();
    s.write_all(body).unwrap();
    parse(read_all(&mut s))
}

fn read_all(s: &mut TcpStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => return out,
            Ok(n) => out.extend_from_slice(&buf[..n]),
        }
    }
}

fn parse(raw: Vec<u8>) -> (u16, Value) {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no response: {:?}", String::from_utf8_lossy(&raw)));
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (
        status,
        serde_json::from_slice(&raw[split + 4..]).unwrap_or(Value::Null),
    )
}

/// POST `path` with `body` sent in `chunk`-byte pieces with `pause` before each piece.
fn trickle_post(
    port: u16,
    path: &str,
    body: &[u8],
    chunk: usize,
    pause: Duration,
) -> (u16, Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{}: {}\r\nContent-Length: {}\r\n\r\n",
        X.0,
        X.1,
        body.len()
    )
    .unwrap();
    for piece in body.chunks(chunk) {
        std::thread::sleep(pause);
        s.write_all(piece).unwrap();
    }
    parse(read_all(&mut s))
}

/// Every regular file outside `.plippa`, with its size.
fn library_files(root: &Path) -> Vec<(String, u64)> {
    fn walk(d: &Path, root: &Path, out: &mut Vec<(String, u64)>) {
        for e in fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.file_name().is_some_and(|n| n == ".plippa") {
                continue;
            }
            if p.is_dir() {
                walk(&p, root, out);
            } else {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                out.push((rel, p.metadata().unwrap().len()));
            }
        }
    }
    let mut v = Vec::new();
    walk(root, root, &mut v);
    v.sort();
    v
}

/// Files left in staging (`.plippa/tmp`), recursively.
fn staging_files(root: &Path) -> Vec<String> {
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

fn begin_import(port: u16) -> String {
    let (st, v) = req(port, "POST", "/api/imports", b"");
    assert_eq!(st, 200, "{v}");
    v["id"].as_str().unwrap().to_string()
}

fn wait_for(mut cond: impl FnMut() -> bool, within: Duration, what: &str) {
    let t = Instant::now();
    while !cond() {
        assert!(t.elapsed() < within, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn media_path() -> &'static str {
    "/api/media?kind=audiobook&filename=long.wav"
}

// ---------------------------------------------------------------------------
// Slow but progressing uploads complete
// ---------------------------------------------------------------------------

#[test]
fn a_slow_but_progressing_large_upload_completes() {
    let rig = start();
    let hub = rig.svc.lock().unwrap().imports();
    // 3.2 MB in 64 KiB pieces, 60 ms apart: about 3 s of wall clock, six times the 500 ms stall
    // timeout, every pause far below it.
    let body = wav(400, 5);
    let t = Instant::now();
    let (st, v) = trickle_post(rig.port, media_path(), &body, 64 * 1024, Duration::from_millis(60));
    assert_eq!(st, 200, "{v}");
    assert!(
        t.elapsed() > STALL * 5,
        "the upload took only {:?}: it must outlast the stall timeout many times over",
        t.elapsed()
    );
    let stored = library_files(&rig.root);
    assert!(
        stored.iter().any(|(_, len)| *len == body.len() as u64),
        "the whole file must be in the library: {stored:?}"
    );
    assert!(staging_files(&rig.root).is_empty(), "no temp files left");
    assert!(!hub.upload_in_flight(), "the slot is free afterwards");
    assert_eq!(hub.staging().used(), 0, "nothing stays reserved");

    // The same through an import session.
    let sid = begin_import(rig.port);
    let body = wav(400, 6);
    let t = Instant::now();
    let (st, v) = trickle_post(
        rig.port,
        &format!("/api/imports/{sid}/files?name=long2.wav"),
        &body,
        64 * 1024,
        Duration::from_millis(60),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["size"].as_u64(), Some(body.len() as u64));
    assert!(t.elapsed() > STALL * 5);
    assert!(!hub.upload_in_flight());
}

// ---------------------------------------------------------------------------
// Stalled and dripping uploads are cut off and free everything
// ---------------------------------------------------------------------------

/// Send half of `body`, then go silent; returns the connection.
fn half_then_silence(port: u16, path: &str, body: &[u8]) -> TcpStream {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{}: {}\r\nContent-Length: {}\r\n\r\n",
        X.0,
        X.1,
        body.len()
    )
    .unwrap();
    s.write_all(&body[..body.len() / 2]).unwrap();
    s.flush().unwrap();
    s
}

#[test]
fn a_stalled_upload_is_cut_off_and_frees_everything() {
    let rig = start();
    let hub = rig.svc.lock().unwrap().imports();
    let body = wav(40, 9);

    for (label, path) in [
        ("media", media_path().to_string()),
        ("import", {
            let sid = begin_import(rig.port);
            format!("/api/imports/{sid}/files?name=stall.wav")
        }),
    ] {
        let mut stalled = half_then_silence(rig.port, &path, &body);
        wait_for(|| hub.upload_in_flight(), Duration::from_secs(3), "the upload to take the slot");
        let t = Instant::now();
        // Within about two stall timeouts the server answers (408) and closes.
        stalled.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (st, _) = parse(read_all(&mut stalled));
        assert_eq!(st, 408, "{label}");
        assert!(
            t.elapsed() < STALL * 4,
            "{label}: cut off after {:?}, expected about {STALL:?}",
            t.elapsed()
        );
        // Everything it held is back: slot, staging bytes, temp file.
        wait_for(|| !hub.upload_in_flight(), Duration::from_secs(2), "the slot to be released");
        assert_eq!(hub.staging().used(), 0, "{label}: staging reservation");
        let leftovers: Vec<String> = staging_files(&rig.root)
            .into_iter()
            .filter(|f| f.ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "{label}: temp files {leftovers:?}");
        // And the next upload works at once.
        let (st, v) = trickle_post(rig.port, media_path(), &wav(2, 1), 16 * 1024, Duration::ZERO);
        assert_eq!(st, 200, "{label}: {v}");
    }
}

#[test]
fn a_drip_upload_that_makes_no_real_progress_is_cut_off() {
    let rig = start();
    let hub = rig.svc.lock().unwrap().imports();
    let body = wav(40, 3);
    // The valid audio head arrives at once, then one byte every 300 ms: every read succeeds
    // well inside the 500 ms stall timeout, but a one-second window brings 5 bytes beyond the
    // head, under the 16 KiB minimum.
    let mut s = TcpStream::connect(("127.0.0.1", rig.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
    write!(
        s,
        "POST {} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{}: {}\r\nContent-Length: {}\r\n\r\n",
        media_path(),
        X.0,
        X.1,
        body.len()
    )
    .unwrap();
    s.write_all(&body[..4096]).unwrap();
    let started = Instant::now();
    let mut ended = None;
    for b in &body[4096..4096 + 40] {
        if s.write_all(&[*b]).is_err() {
            ended = Some(started.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
        let mut buf = [0u8; 512];
        match s.read(&mut buf) {
            Ok(_) => {
                ended = Some(started.elapsed());
                break;
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => {
                ended = Some(started.elapsed());
                break;
            }
        }
    }
    let ended = ended.expect("a one-byte-per-300ms drip must be cut off");
    assert!(ended < Duration::from_secs(4), "cut off only after {ended:?}");
    wait_for(|| !hub.upload_in_flight(), Duration::from_secs(2), "the slot to be released");
    assert_eq!(hub.staging().used(), 0);
}

// ---------------------------------------------------------------------------
// One slot for every upload route
// ---------------------------------------------------------------------------

fn start_unite(port: u16) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let body = br#"{"associateOnly":true}"#;
    write!(
        s,
        "POST /api/unite/start HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\n{}: {}\r\nContent-Length: {}\r\n\r\n",
        X.0,
        X.1,
        body.len()
    )
    .unwrap();
    s.write_all(body).unwrap();
    let (st, v) = parse(read_all(&mut s));
    assert_eq!(st, 200, "{v}");
    v["session"].as_str().unwrap().to_string()
}

#[test]
fn the_unite_file_route_takes_the_upload_slot() {
    let rig = start();
    let hub = rig.svc.lock().unwrap().imports();
    let unite = start_unite(rig.port);
    let sid = begin_import(rig.port);
    let body = wav(40, 2);

    // An upload into a unite session holds the one slot: an import upload is told to wait ...
    let mut holding = half_then_silence(rig.port, &format!("/api/unite/{unite}/files?name=a.wav"), &body);
    wait_for(|| hub.upload_in_flight(), Duration::from_secs(3), "the unite upload to take the slot");
    let (st, v) = trickle_post(
        rig.port,
        &format!("/api/imports/{sid}/files?name=b.wav"),
        &wav(1, 4),
        16 * 1024,
        Duration::ZERO,
    );
    assert_eq!(st, 429, "{v}");
    // ... and so is a second unite upload.
    let (st, _) = trickle_post(
        rig.port,
        &format!("/api/unite/{unite}/files?name=c.wav"),
        &wav(1, 4),
        16 * 1024,
        Duration::ZERO,
    );
    assert_eq!(st, 429);
    // When the holder ends (here: its stall timeout), the slot is free again.
    let (st, v) = parse(read_all(&mut holding));
    assert_eq!(st, 408, "a stalled unite upload times out like imports and media");
    assert_eq!(v["code"], "timeout");
    assert!(
        v["error"].as_str().is_some_and(|m| m.contains("stalled")),
        "the error says why: {v}"
    );
    wait_for(|| !hub.upload_in_flight(), Duration::from_secs(2), "the slot to be released");
    let (st, v) = trickle_post(
        rig.port,
        &format!("/api/unite/{unite}/files?name=d.wav"),
        &wav(1, 5),
        16 * 1024,
        Duration::ZERO,
    );
    assert_eq!(st, 200, "{v}");
}

// ---------------------------------------------------------------------------
// Large declared sizes
// ---------------------------------------------------------------------------

/// Declare `len` bytes and send none: answers `Some(status)` if the server refused at once,
/// `None` if it accepted the declaration and is waiting for the body.
fn declare_only(rig: &Rig, len: u64) -> (Option<(u16, String)>, bool) {
    let hub = rig.svc.lock().unwrap().imports();
    let mut s = TcpStream::connect(("127.0.0.1", rig.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
    write!(
        s,
        "POST {} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n{}: {}\r\nContent-Length: {len}\r\n\r\n",
        media_path(),
        X.0,
        X.1,
    )
    .unwrap();
    // Wait until the handler has either answered or taken the slot (waiting for the body).
    let t = Instant::now();
    let mut buf = [0u8; 4096];
    let mut got = Vec::new();
    while t.elapsed() < Duration::from_secs(2) {
        if hub.upload_in_flight() {
            return (None, true);
        }
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(_) => {}
        }
    }
    let text = String::from_utf8_lossy(&got).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (Some((status, text)), false)
}

#[test]
fn the_default_limits_accept_a_2_gib_declared_length_and_the_old_ones_still_refuse_oversize() {
    let rig = start();
    const GIB: u64 = 1024 * 1024 * 1024;
    // 2 GiB (the old per-file default) and the new ceiling, 4 GiB minus one byte: both are taken
    // up (the handler holds the upload slot and waits for the body). No gigabytes are written.
    for len in [2 * GIB, 4 * GIB - 1] {
        let (refused, took_slot) = declare_only(&rig, len);
        assert!(took_slot, "{len} bytes must be accepted, got {refused:?}");
        // wait for the (silent) upload to be cut off before the next probe
        let hub = rig.svc.lock().unwrap().imports();
        wait_for(|| !hub.upload_in_flight(), Duration::from_secs(5), "the probe upload to end");
    }
    // One byte over the ceiling, and far over it: refused up front, naming the limit.
    for len in [4 * GIB, 64 * GIB] {
        let (refused, took_slot) = declare_only(&rig, len);
        assert!(!took_slot, "{len} bytes must be refused");
        let (st, text) = refused.expect("an answer");
        assert_eq!(st, 413, "{text}");
    }
    let (_, text) = declare_only(&rig, 4 * GIB).0.unwrap();
    assert!(text.contains("4 GB"), "the message must say the limit: {text}");
}
