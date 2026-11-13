//! BCS_09: the whole Bandcamp journey, over real HTTP, with no socket to the internet.
//!
//! Everything the preceding packages built is assembled here the way `main` assembles it
//! (`ServerService::wire_store_hub`), and then driven the way the portal and the device
//! drive it: `connect → sync → browse → open → prepare → confirm → download → unite →
//! playable → delete → download again`, plus the tap flow and the Path A replay.
//!
//! Two in-process fixtures stand in for the network:
//!
//! * [`Catalog`] — the store API (`plippa_store::Transport`): `getArtists`,
//!   `getAlbumList2`, `getAlbum`, `getCoverArt`, `search3`, and `download` answering the
//!   measured "bad version" envelope, so every download takes the real `stream` fallback.
//! * [`Net`] — the media/web side (`web::fetch::Transport`): `stream.view` redirecting to
//!   the CDN host, the audio body, and a `web.noudata` document for the linked path.
//!
//! The audio fixture is a synthetic WAV rather than an mp3 so that the duration probe has
//! something real to measure: the store still declares the lossy `stream`, and the reports
//! asserted here are the ones the server derives from that declaration (lossy, substituted,
//! not the original) — the container is only what the bytes happen to be.
//!
//! No test opens a socket other than the loopback portal.

mod common;

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plippa_server::web::download::DownloadLimits;
use plippa_server::web::fetch::{HopResponse, Transport as WebTransport, TransportError};
use plippa_server::web::{WebConfig, WebHub};
use plippa_server::{RecordingIpc, ServerConfig, ServerService, StoreHub};
use plippa_store::{HeaderMap, StoreError, Transport, TransportResponse};

/// The fixture account. A test asserts this never reaches a payload or the store log.
const USER: &str = "fan@example.com";
const PASSWORD: &str = "e2e-fixture-password-9f3";
/// The Noubin key a linked download names (Path A).
const KEY: &str = "https://linkhost.example/noubin/album/";
/// The release the journey downloads.
const RELEASE_ID: &str = "a:1";
const TRACK_ID: &str = "t:1";
/// The Sable single BCS_06 matches by title.
const SABLE_URL: &str = "https://sable-music.bandcamp.com/track/were-getting-older-baby";

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A 16-bit PCM WAV of `ms` milliseconds at 8 kHz (lofty measures its length exactly).
fn wav(ms: u64) -> Vec<u8> {
    let rate: u32 = 8_000;
    let channels: u16 = 1;
    let frames = (rate as u64 * ms / 1000) as u32;
    let data_bytes = frames * u32::from(channels) * 2;
    let mut body = Vec::with_capacity(44 + data_bytes as usize);
    body.extend_from_slice(b"RIFF");
    body.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    body.extend_from_slice(b"WAVEfmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // PCM
    body.extend_from_slice(&channels.to_le_bytes());
    body.extend_from_slice(&rate.to_le_bytes());
    body.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
    body.extend_from_slice(&(channels * 2).to_le_bytes());
    body.extend_from_slice(&16u16.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&data_bytes.to_le_bytes());
    body.resize(44 + data_bytes as usize, 0);
    body
}

/// A tiny PNG (signature + IHDR is all the sniffer reads).
fn png() -> Vec<u8> {
    let mut body = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    body.extend_from_slice(&13u32.to_be_bytes());
    body.extend_from_slice(b"IHDR");
    body.extend_from_slice(&512u32.to_be_bytes());
    body.extend_from_slice(&512u32.to_be_bytes());
    body.extend_from_slice(&[8, 6, 0, 0, 0]);
    body.extend_from_slice(&[0, 0, 0, 0]);
    body
}

/// Percent-decode one query parameter value.
fn param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key == name {
            let mut out = String::with_capacity(value.len());
            let bytes = value.as_bytes();
            let mut index = 0;
            while index < bytes.len() {
                match bytes[index] {
                    b'%' if index + 2 < bytes.len() => {
                        let hex = &value[index + 1..index + 3];
                        match u8::from_str_radix(hex, 16) {
                            Ok(byte) => out.push(byte as char),
                            Err(_) => out.push('%'),
                        }
                        index += 3;
                    }
                    b'+' => {
                        out.push(' ');
                        index += 1;
                    }
                    other => {
                        out.push(other as char);
                        index += 1;
                    }
                }
            }
            return Some(out);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The store API fixture
// ---------------------------------------------------------------------------

struct Catalog {
    /// One entry per `getAlbumList2` item, in provider order.
    releases: Mutex<Vec<serde_json::Value>>,
    /// `getAlbum` payloads by id.
    details: Mutex<BTreeMap<String, serde_json::Value>>,
    hits: Mutex<Vec<String>>,
    reject_auth: AtomicBool,
    dead: AtomicBool,
    cover: Vec<u8>,
}

impl Catalog {
    fn standard() -> Self {
        let catalog = Self::empty();
        catalog.add_release(
            RELEASE_ID,
            "We're Getting Older Baby",
            "Sable",
            2026,
            &[(TRACK_ID, "We're Getting Older Baby", 1, 4)],
        );
        catalog.add_release(
            "a:2",
            "Second Release",
            "The Artist",
            2025,
            &[("t:2", "One", 1, 30), ("t:3", "Two", 2, 31)],
        );
        catalog.add_release("a:3", "Third Release", "Another Artist", 2024, &[("t:4", "Only", 1, 12)]);
        catalog
    }

    fn empty() -> Self {
        Self {
            releases: Mutex::new(Vec::new()),
            details: Mutex::new(BTreeMap::new()),
            hits: Mutex::new(Vec::new()),
            reject_auth: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            cover: png(),
        }
    }

    fn add_release(
        &self,
        id: &str,
        title: &str,
        artist: &str,
        year: u16,
        tracks: &[(&str, &str, u32, u64)],
    ) {
        let songs: Vec<serde_json::Value> = tracks
            .iter()
            .map(|(track_id, track_title, number, duration_s)| {
                serde_json::json!({
                    "id": track_id,
                    "title": track_title,
                    "track": number,
                    "discNumber": 1,
                    "duration": duration_s,
                    "suffix": "mp3",
                    "coverArt": "c:1",
                })
            })
            .collect();
        lock(&self.releases).push(serde_json::json!({
            "id": id,
            "name": title,
            "artist": artist,
            "albumArtist": artist,
            "year": year,
            "songCount": tracks.len(),
            "duration": tracks.iter().map(|t| t.3).sum::<u64>(),
            "coverArt": "c:1",
        }));
        lock(&self.details).insert(
            id.to_string(),
            serde_json::json!({
                "subsonic-response": {
                    "status": "ok",
                    "version": "1.16.1",
                    "album": {
                        "id": id,
                        "name": title,
                        "artist": artist,
                        "albumArtist": artist,
                        "year": year,
                        "songCount": songs.len(),
                        "coverArt": "c:1",
                        "song": songs,
                    }
                }
            }),
        );
    }

    fn hits(&self) -> Vec<String> {
        lock(&self.hits).clone()
    }

    fn reset_hits(&self) {
        lock(&self.hits).clear();
    }

    fn json(payload: serde_json::Value) -> TransportResponse {
        TransportResponse {
            status: 200,
            headers: vec![(
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )],
            body: serde_json::to_vec(&payload).unwrap_or_default(),
            final_url: "https://bandcamp.com/api/subsonic/rest/response.view".to_string(),
            redirects: Vec::new(),
        }
    }
}

impl Transport for Catalog {
    fn get(
        &self,
        url: &str,
        _headers: &HeaderMap,
        _max_bytes: u64,
    ) -> Result<TransportResponse, StoreError> {
        lock(&self.hits).push(url.to_string());
        if self.dead.load(Ordering::SeqCst) {
            return Err(StoreError::transport_url(url, "the fixture network is down"));
        }
        if !url.starts_with("https://bandcamp.com/api/subsonic/rest/") {
            return Err(StoreError::transport_url(url, "unexpected host"));
        }
        let endpoint = url
            .split("/rest/")
            .nth(1)
            .and_then(|rest| rest.split('.').next())
            .unwrap_or_default();
        Ok(match endpoint {
            "getArtists" => {
                if self.reject_auth.load(Ordering::SeqCst) {
                    return Ok(Catalog::json(serde_json::json!({
                        "subsonic-response": {
                            "status": "failed",
                            "version": "1.16.1",
                            "error": { "code": 40, "message": "Wrong username or password" }
                        }
                    })));
                }
                Catalog::json(serde_json::json!({
                    "subsonic-response": { "status": "ok", "version": "1.16.1", "artists": { "index": [] } }
                }))
            }
            "getAlbumList2" => {
                let releases = lock(&self.releases).clone();
                let offset = param(url, "offset")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                let size = param(url, "size")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(200);
                let page: Vec<serde_json::Value> =
                    releases.iter().skip(offset).take(size).cloned().collect();
                Catalog::json(serde_json::json!({
                    "subsonic-response": {
                        "status": "ok",
                        "version": "1.16.1",
                        "albumList2": { "album": page }
                    }
                }))
            }
            "getAlbum" => {
                let id = param(url, "id").unwrap_or_default();
                match lock(&self.details).get(&id).cloned() {
                    Some(detail) => Catalog::json(detail),
                    None => Catalog::json(serde_json::json!({
                        "subsonic-response": {
                            "status": "failed",
                            "version": "1.16.1",
                            "error": { "code": 70, "message": "not found" }
                        }
                    })),
                }
            }
            "getCoverArt" => TransportResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "image/png".to_string())],
                body: self.cover.clone(),
                final_url: url.to_string(),
                redirects: Vec::new(),
            },
            "search3" => Catalog::json(serde_json::json!({
                "subsonic-response": {
                    "status": "ok",
                    "version": "1.16.1",
                    "searchResult3": { "song": [] }
                }
            })),
            // The measured shape: `download` is a route stub that answers 200 "bad version".
            "download" => Catalog::json(serde_json::json!({
                "error": true,
                "error_message": "bad version"
            })),
            other => return Err(StoreError::transport_url(url, other)),
        })
    }
}

// ---------------------------------------------------------------------------
// The media / web fixture
// ---------------------------------------------------------------------------

struct Net {
    hits: Mutex<Vec<String>>,
    audio: Mutex<Vec<u8>>,
    doc: Mutex<Option<Vec<u8>>>,
    cover: Vec<u8>,
    dead: AtomicBool,
    /// When set, `stream.view` redirects here instead of to the CDN.
    redirect: Mutex<Option<String>>,
}

impl Net {
    fn standard(audio_ms: u64) -> Self {
        Self {
            hits: Mutex::new(Vec::new()),
            audio: Mutex::new(wav(audio_ms)),
            doc: Mutex::new(None),
            cover: png(),
            dead: AtomicBool::new(false),
            redirect: Mutex::new(None),
        }
    }

    fn hits(&self) -> Vec<String> {
        lock(&self.hits).clone()
    }

    fn reset_hits(&self) {
        lock(&self.hits).clear();
    }

    fn audio(&self) -> Vec<u8> {
        lock(&self.audio).clone()
    }

    fn serve_doc(&self, doc: String) {
        *lock(&self.doc) = Some(doc.into_bytes());
    }

    fn reply(
        status: u16,
        content_type: Option<&str>,
        location: Option<String>,
        body: Vec<u8>,
    ) -> HopResponse {
        HopResponse {
            status,
            content_type: content_type.map(str::to_string),
            location,
            content_length: Some(body.len() as u64),
            body: Box::new(Cursor::new(body)),
        }
    }
}

impl WebTransport for Net {
    fn get(&self, url: &str, _timeout: Duration) -> Result<HopResponse, TransportError> {
        lock(&self.hits).push(url.to_string());
        if self.dead.load(Ordering::SeqCst) {
            return Err(TransportError::Connect("the fixture network is down".into()));
        }
        if url.contains("/rest/stream.view") {
            let target = lock(&self.redirect).clone().unwrap_or_else(|| {
                "https://t4.bcbits.com/stream/track.mp3?token=fixture".to_string()
            });
            return Ok(Self::reply(302, None, Some(target), Vec::new()));
        }
        if url.starts_with("https://t4.bcbits.com/") {
            return Ok(Self::reply(
                200,
                Some("audio/wav"),
                None,
                lock(&self.audio).clone(),
            ));
        }
        if url.ends_with("/web.noudata") {
            let body = lock(&self.doc).clone().unwrap_or_default();
            return Ok(Self::reply(200, Some("application/json"), None, body));
        }
        if url.ends_with(".png") || url.ends_with(".jpg") {
            return Ok(Self::reply(200, Some("image/png"), None, self.cover.clone()));
        }
        Ok(Self::reply(404, None, None, Vec::new()))
    }
}

// ---------------------------------------------------------------------------
// Rig
// ---------------------------------------------------------------------------

fn ample_free(_: &Path) -> Option<u64> {
    Some(1 << 40)
}

fn test_limits() -> DownloadLimits {
    DownloadLimits {
        max_file_bytes: 8 * 1024 * 1024,
        max_batch_bytes: 64 * 1024 * 1024,
        free_reserve: 0,
        free_space: ample_free,
        // What the store hub sets in production: a store audio URL is a bearer capability.
        log_url: false,
        ..DownloadLimits::default()
    }
}

struct Rig {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    secrets: Option<PathBuf>,
    port: u16,
    catalog: Arc<Catalog>,
    net: Arc<Net>,
}

impl Rig {
    fn start(tag: &str) -> Self {
        Self::with(tag, Arc::new(Catalog::standard()), Arc::new(Net::standard(4_000)), true)
    }

    fn with(_tag: &str, catalog: Arc<Catalog>, net: Arc<Net>, with_secrets: bool) -> Self {
        let tmp = tempfile::tempdir().expect("temp dir");
        let root = tmp.path().join("library");
        std::fs::create_dir_all(&root).expect("library");
        let secrets = with_secrets.then(|| tmp.path().join("secrets"));

        let hub = Arc::new(
            StoreHub::with_transport(secrets.clone(), Arc::clone(&catalog) as Arc<dyn Transport + Send + Sync>)
                .with_sync_tuning(200, Duration::ZERO)
                .with_download_tuning(test_limits(), Duration::ZERO),
        );
        hub.set_library_root(Some(root.clone()));

        let mut svc = ServerService::start_with_stores(
            Some(root.clone()),
            Box::new(RecordingIpc::new()),
            ServerService::default_mock_associate(),
            secrets.clone(),
        );
        svc.set_web_hub(Arc::new(WebHub::with_transport(
            WebConfig::default(),
            Arc::clone(&net) as Arc<dyn WebTransport>,
            Box::new(|| true),
        )));
        svc.set_stores_hub(Arc::clone(&hub));
        let service = Arc::new(Mutex::new(svc));
        // The same wiring `main` uses: one fetcher, one index, one deleter.
        ServerService::wire_store_hub(&service);

        let port = TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        let cfg = ServerConfig {
            bind: format!("127.0.0.1:{port}").parse().expect("bind addr"),
            library_root: Some(root.clone()),
            secrets_dir: secrets.clone(),
            portal_url: None,
            ..ServerConfig::default()
        };
        common::serve_open(cfg, service);
        for _ in 0..400 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Self {
                    _tmp: tmp,
                    root,
                    secrets,
                    port,
                    catalog,
                    net,
                };
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("the server did not start");
    }

    fn secret_file(&self) -> PathBuf {
        self.secrets
            .clone()
            .unwrap_or_else(|| self.root.join("no-secrets"))
            .join("bandcamp.json")
    }

    fn cache_file(&self) -> PathBuf {
        self.root.join(".plippa").join("stores").join("bandcamp.json")
    }

    fn staging_dirs(&self) -> Vec<PathBuf> {
        let tmp = self.root.join(".plippa").join("tmp");
        let Ok(entries) = std::fs::read_dir(&tmp) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("store-"))
            })
            .collect()
    }

    fn files_with_ext(&self, ext: &str) -> Vec<PathBuf> {
        fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.filter_map(|entry| entry.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, ext, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, ext, &mut out);
        out.sort();
        out
    }

    /// Every file under the library root with its size and mtime.
    fn snapshot(&self) -> BTreeMap<String, (u64, u128)> {
        fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, (u64, u128)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.filter_map(|entry| entry.ok()) {
                let path = entry.path();
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                if meta.is_dir() {
                    walk(&path, root, out);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned();
                    let stamp = meta
                        .modified()
                        .ok()
                        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    out.insert(rel, (meta.len(), stamp));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.root, &self.root, &mut out);
        out
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// Returns `(status, headers, body)`.
fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
         Content-Length: {}\r\n",
        body.len()
    );
    for (key, value) in headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).expect("write head");
    stream.write_all(body).expect("write body");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .expect("status code");
    let (raw_head, raw_body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let headers = raw_head.split("\r\n").skip(1).collect::<Vec<_>>().join("\n");
    (status, headers, raw_body.to_string())
}

fn json(port: u16, method: &str, path: &str, body: &str) -> (u16, serde_json::Value) {
    let (status, _, text) = request(
        port,
        method,
        path,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    );
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

fn get_json(port: u16, path: &str) -> (u16, serde_json::Value) {
    let (status, _, text) = request(port, "GET", path, &[], b"");
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

/// Connect and sync, the two steps every journey starts with.
fn connect_and_sync(rig: &Rig) -> serde_json::Value {
    let body = format!(r#"{{"user":"{USER}","password":"{PASSWORD}","consent":true}}"#);
    let (status, value) = json(rig.port, "POST", "/api/stores/bandcamp/connect", &body);
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["ok"], true);
    let (status, started) = json(rig.port, "POST", "/api/stores/bandcamp/sync", "");
    assert_eq!(status, 202, "{started}");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (status, sync) = get_json(rig.port, "/api/stores/bandcamp/sync");
        assert_eq!(status, 200, "{sync}");
        if sync["state"] != "running" {
            assert_eq!(sync["state"], "done", "{sync}");
            return sync;
        }
        assert!(Instant::now() < deadline, "the sync never finished");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Prepare a download for `releases`, then start it, and wait for a terminal state.
fn download(rig: &Rig, releases: &[&str], noubin_key: Option<&str>) -> serde_json::Value {
    let mut body = serde_json::json!({ "releases": releases });
    if let Some(key) = noubin_key {
        body["noubinKey"] = serde_json::json!(key);
    }
    let (status, consent) = json(
        rig.port,
        "POST",
        "/api/stores/bandcamp/download/prepare",
        &body.to_string(),
    );
    assert_eq!(status, 200, "{consent}");
    let consent_id = consent["consentId"].as_str().expect("a consent id").to_string();
    assert!(consent["refused"].as_array().is_some_and(|r| r.is_empty()), "{consent}");

    let start = serde_json::json!({ "consent": true, "consentId": consent_id });
    let (status, started) = json(
        rig.port,
        "POST",
        "/api/stores/bandcamp/download",
        &start.to_string(),
    );
    assert_eq!(status, 202, "{started}");
    let session = started["session"].as_str().expect("a session id").to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (status, status_body) = get_json(
            rig.port,
            &format!("/api/stores/bandcamp/download/{session}"),
        );
        assert_eq!(status, 200, "{status_body}");
        if status_body["state"] != "running" {
            return status_body;
        }
        assert!(Instant::now() < deadline, "the download never finished");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn playables(rig: &Rig) -> Vec<serde_json::Value> {
    let (status, value) = get_json(rig.port, "/api/playables");
    assert_eq!(status, 200, "{value}");
    value.as_array().cloned().unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 1. The journey
// ---------------------------------------------------------------------------

#[test]
fn store_e2e_full_journey() {
    let rig = Rig::start("e2e-journey");
    let port = rig.port;

    // -- connect (wrong first) -------------------------------------------------
    // A refused login answers 401 and leaves nothing behind, not even the directory.
    rig.catalog.reject_auth.store(true, Ordering::SeqCst);
    let wrong = format!(r#"{{"user":"{USER}","password":"not-the-password","consent":true}}"#);
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/connect", &wrong);
    assert_eq!(status, 401, "{value}");
    assert_eq!(value["code"], "bad_credentials", "{value}");
    assert!(!value.to_string().contains(USER), "the username leaked: {value}");
    assert!(!value.to_string().contains(PASSWORD), "the password leaked: {value}");
    assert!(!rig.secret_file().exists(), "a refused connect must save nothing");
    let (_, stores) = get_json(port, "/api/stores");
    assert_eq!(stores["stores"][0]["connected"], false, "{stores}");

    // -- connect (good) -------------------------------------------------------
    rig.catalog.reject_auth.store(false, Ordering::SeqCst);
    let body = format!(r#"{{"user":" {USER} ","password":" {PASSWORD} ","consent":true}}"#);
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/connect", &body);
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["store"]["connected"], true, "{value}");
    let secret = rig.secret_file();
    assert!(secret.is_file(), "the credentials were saved");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&secret).expect("stat file").permissions().mode() & 0o777,
        0o600,
        "the credentials file must be 0600"
    );
    let dir = secret.parent().expect("secrets dir");
    assert_eq!(
        std::fs::metadata(dir).expect("stat dir").permissions().mode() & 0o777,
        0o700,
        "the secrets directory must be 0700"
    );
    // The stored bytes contain the password (it is the secret), and nothing that is
    // *returned* does.
    assert!(!value.to_string().contains(PASSWORD), "{value}");

    // -- sync -----------------------------------------------------------------
    let sync = connect_and_sync(&rig);
    assert_eq!(sync["releases"], 3, "{sync}");
    assert!(sync["pages"].as_u64().unwrap_or(0) >= 1, "{sync}");
    let cache = rig.cache_file();
    assert!(cache.is_file(), "the cache was written");
    assert_eq!(
        std::fs::metadata(&cache).expect("stat cache").permissions().mode() & 0o777,
        0o600,
        "the cache must be 0600"
    );
    let cached: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cache).expect("read cache")).expect("cache json");
    assert_eq!(
        cached["releases"].as_array().map(Vec::len),
        Some(3),
        "the cache holds the fixture collection"
    );

    // -- browse and search (zero requests: it is the cache) -------------------
    rig.catalog.reset_hits();
    rig.net.reset_hits();
    let (status, list) = get_json(port, "/api/stores/bandcamp/collection");
    assert_eq!(status, 200, "{list}");
    assert_eq!(list["total"], 3, "{list}");
    assert_eq!(list["releases"].as_array().map(Vec::len), Some(3));
    assert!(list["syncedAt"].as_u64().is_some(), "{list}");
    let (status, found) = get_json(port, "/api/stores/bandcamp/collection?query=getting%20older");
    assert_eq!(status, 200, "{found}");
    assert_eq!(found["total"], 1, "{found}");
    assert_eq!(found["releases"][0]["id"], RELEASE_ID);
    assert_eq!(found["releases"][0]["trackCount"], 1);
    assert_eq!(rig.catalog.hits().len(), 0, "browsing must not touch the store");
    assert_eq!(rig.net.hits().len(), 0, "browsing must not touch the network");

    // -- open the release -----------------------------------------------------
    let (status, view) = get_json(port, &format!("/api/stores/bandcamp/release/{RELEASE_ID}"));
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["release"]["title"], "We're Getting Older Baby");
    assert_eq!(view["tracks"].as_array().map(Vec::len), Some(1));
    assert_eq!(view["tracks"][0]["id"], TRACK_ID);
    // The honesty seams: the quality note travels with the view, and the capability flag
    // does not claim an original file before it has been proven.
    assert_eq!(view["quality"]["audio"], "lossy", "{view}");
    assert!(view["quality"]["bitrateKbps"].is_null(), "{view}");
    assert!(
        view["quality"]["note"].as_str().is_some_and(|n| n.contains("lossy stream")),
        "{view}"
    );
    assert_eq!(view["capabilities"]["download"], false, "{view}");
    assert_eq!(view["capabilities"]["download_unverified"], true, "{view}");

    // Cover art: sniffed type, short private cache.
    let (status, headers, _) = request(
        port,
        "GET",
        &format!("/api/stores/bandcamp/release/{RELEASE_ID}/cover"),
        &[],
        b"",
    );
    assert_eq!(status, 200);
    assert!(headers.to_lowercase().contains("content-type: image/png"), "{headers}");
    assert!(
        headers.contains("Cache-Control: private, max-age=300"),
        "{headers}"
    );

    // -- prepare: a consent sheet and nothing on disk -------------------------
    let before = rig.snapshot();
    let (status, consent) = json(
        port,
        "POST",
        "/api/stores/bandcamp/download/prepare",
        &serde_json::json!({ "releases": [RELEASE_ID] }).to_string(),
    );
    assert_eq!(status, 200, "{consent}");
    assert_eq!(consent["tracks"], 1, "{consent}");
    assert_eq!(consent["quality"]["audio"], "lossy", "{consent}");
    assert!(
        consent["consentMessage"]
            .as_str()
            .is_some_and(|m| m.contains("lossy")),
        "{consent}"
    );
    let after = rig.snapshot();
    assert_eq!(before, after, "prepare must not write one byte");
    assert!(rig.staging_dirs().is_empty(), "prepare must not create a staging area");

    // -- confirm, download, unite --------------------------------------------
    let start = serde_json::json!({
        "consent": true,
        "consentId": consent["consentId"],
    });
    let (status, started) = json(
        port,
        "POST",
        "/api/stores/bandcamp/download",
        &start.to_string(),
    );
    assert_eq!(status, 202, "{started}");
    let session = started["session"].as_str().expect("session").to_string();

    let deadline = Instant::now() + Duration::from_secs(30);
    let finished = loop {
        let (status, body) = get_json(
            port,
            &format!("/api/stores/bandcamp/download/{session}"),
        );
        assert_eq!(status, 200, "{body}");
        if body["state"] != "running" {
            break body;
        }
        assert!(Instant::now() < deadline, "the download never finished");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(finished["state"], "done", "{finished}");
    assert_eq!(finished["errors"].as_array().map(Vec::len), Some(0), "{finished}");

    let item = &finished["items"][0];
    assert_eq!(item["state"], "done", "{finished}");
    assert_eq!(item["original"], false, "the purchased file was not obtained");
    assert_eq!(item["streamSubstituted"], true, "{item}");
    assert_eq!(item["quality"], "lossy", "{item}");
    assert_eq!(item["format"], "wav", "the container comes from the bytes");
    assert_eq!(item["bytes"], rig.net.audio().len() as u64, "{item}");
    assert_eq!(item["durationMs"], 4_000, "the length was measured, not declared");
    assert!(
        finished["warnings"]
            .as_array()
            .is_some_and(|w| w.iter().any(|m| m.as_str().unwrap_or("").contains("lossy stream"))),
        "the result says out loud what arrived: {finished}"
    );
    let unite = &finished["unite"];
    assert_eq!(unite["state"], "placed", "{finished}");
    assert_eq!(unite["metadata"], "store", "Path B: no release was linked");
    assert_eq!(unite["indexed"], true, "{finished}");
    assert_eq!(unite["canLink"], true, "{finished}");

    // Nothing was left in staging; one media file and one sidecar exist.
    assert!(rig.staging_dirs().is_empty(), "staging must be cleaned up");
    let media = rig.files_with_ext("wav");
    assert_eq!(media.len(), 1, "{media:?}");
    assert_eq!(std::fs::read(&media[0]).expect("read audio"), rig.net.audio());
    let noudata = rig.files_with_ext("noudata");
    assert_eq!(noudata.len(), 1, "{noudata:?}");

    // -- the written sidecar validates through plippa-noubin -------------------
    let bytes = std::fs::read(&noudata[0]).expect("read .noudata");
    let doc = plippa_noubin::parse_noudata(&bytes).expect("the .noudata must parse");
    assert_eq!(doc.playable_item_title.as_deref(), Some("We're Getting Older Baby"));
    assert_eq!(doc.media_list.len(), 1);
    let release_dir = noudata[0].parent().expect("release dir");
    let paths = doc
        .media_list
        .iter()
        .flat_map(|item| item.local_file_paths.iter().flatten())
        .collect::<Vec<_>>();
    assert_eq!(paths.len(), 1, "every track is listed");
    for rel in &paths {
        assert!(
            release_dir.join(rel).is_file(),
            "{rel} must exist beside the sidecar"
        );
    }
    let source = doc
        .extra
        .get("plippaStoreSource")
        .expect("the store provenance is recorded");
    assert_eq!(source["provider"], "bandcamp", "{source}");
    assert_eq!(source["metadata"], "store", "{source}");
    assert_eq!(source["releaseId"], RELEASE_ID, "{source}");
    assert_eq!(source["trackIds"].as_array().map(Vec::len), Some(1), "{source}");

    // -- the item is in the index --------------------------------------------
    let listed = playables(&rig);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["title"], "We're Getting Older Baby");
    assert_eq!(listed[0]["kind"], "album");
    assert_eq!(listed[0]["media_relpaths"].as_array().map(Vec::len), Some(1));
    let playable_id = listed[0]["id"].as_str().expect("id").to_string();
    let (status, detail) = get_json(port, &format!("/api/playables/{playable_id}"));
    assert_eq!(status, 200, "{detail}");

    // -- delete (BCS_10): preview, confirm, gone ------------------------------
    let (status, preview) = get_json(
        port,
        &format!("/api/collections/{playable_id}/delete/preview"),
    );
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["files"].as_array().map(Vec::len), Some(1), "{preview}");
    assert_eq!(preview["blockers"].as_array().map(Vec::len), Some(0), "{preview}");
    assert_eq!(preview["playing"], false, "{preview}");

    // Without `force` the server answers 409 with the same preview and deletes nothing.
    let (status, refusal) = json(
        port,
        "DELETE",
        &format!("/api/collections/{playable_id}"),
        "{}",
    );
    assert_eq!(status, 409, "{refusal}");
    assert_eq!(refusal["code"], "confirm_required", "{refusal}");
    assert_eq!(rig.files_with_ext("wav").len(), 1, "the refusal deleted a file");

    let (status, outcome) = json(
        port,
        "DELETE",
        &format!("/api/collections/{playable_id}"),
        &serde_json::json!({ "force": true }).to_string(),
    );
    assert_eq!(status, 200, "{outcome}");
    assert_eq!(outcome["ok"], true, "{outcome}");
    assert_eq!(outcome["index_updated"], true, "{outcome}");
    assert_eq!(outcome["folder_removed"], true, "{outcome}");
    assert!(rig.files_with_ext("wav").is_empty(), "the audio is gone");
    assert!(rig.files_with_ext("noudata").is_empty(), "the sidecar is gone");
    assert!(playables(&rig).is_empty(), "the index rows are gone");
    let (status, _) = get_json(port, &format!("/api/playables/{playable_id}"));
    assert_eq!(status, 404, "a deleted item is not found");

    // -- download again: the same result, one folder, no orphan ---------------
    let again = download(&rig, &[RELEASE_ID], None);
    assert_eq!(again["state"], "done", "{again}");
    assert_eq!(again["unite"]["state"], "placed", "{again}");
    assert_eq!(again["items"][0]["bytes"], rig.net.audio().len() as u64);
    let listed_again = playables(&rig);
    assert_eq!(listed_again.len(), 1, "{listed_again:?}");
    assert_eq!(listed_again[0]["title"], "We're Getting Older Baby");
    let media = rig.files_with_ext("wav");
    assert_eq!(media.len(), 1, "no duplicate folder: {media:?}");
    assert_eq!(rig.files_with_ext("noudata").len(), 1, "no orphan sidecar");
    assert!(rig.staging_dirs().is_empty());
    // Every media file lives under one release folder.
    let dirs: Vec<&Path> = media.iter().filter_map(|p| p.parent()).collect();
    assert_eq!(dirs.len(), 1);
    assert_eq!(dirs[0].components().count() - rig.root.components().count(), 3);

    // -- redaction: the store log ring never carries the account --------------
    let log = plippa_server::store_log_tail().join("\n");
    assert!(!log.contains(PASSWORD), "the password reached the store log");
    assert!(!log.contains(USER), "the username reached the store log");
    assert!(!log.contains("stream.view"), "a token-bearing URL reached the log");
    assert!(!log.contains("bcbits.com"), "a CDN URL reached the log");
}

// ---------------------------------------------------------------------------
// 2. Path A: the linked document, and the replay that fetches nothing
// ---------------------------------------------------------------------------

fn document(title: &str, media: &[(&str, &str)]) -> String {
    let media_list: Vec<serde_json::Value> = media
        .iter()
        .map(|(media_title, length)| {
            serde_json::json!({
                "mediaTitle": media_title,
                "uniteData": { "expectedLength": length },
            })
        })
        .collect();
    serde_json::json!({
        "_whatAmILookingAt": plippa_noubin::WHAT_AM_I_LOOKING_AT,
        "noudataVersion": plippa_noubin::NOUDATA_VERSION,
        "noudataType": "release",
        "playableItemTitle": title,
        "playableItemProfile": "music",
        "releaseData": {
            "credits": { "primaryArtists": [{ "primaryArtistName": "Sable" }] }
        },
        "mediaList": media_list,
    })
    .to_string()
}

#[test]
fn store_e2e_path_a_uses_the_document_and_a_replay_downloads_nothing() {
    let rig = Rig::start("e2e-path-a");
    rig.net
        .serve_doc(document("The Authoritative Album", &[("One", "0:04.000")]));
    connect_and_sync(&rig);

    let status = download(&rig, &[RELEASE_ID], Some(KEY));
    assert_eq!(status["state"], "done", "{status}");
    assert_eq!(status["unite"]["state"], "placed", "{status}");
    assert_eq!(
        status["unite"]["metadata"], "noudata",
        "Path A carries the linked document's metadata: {status}"
    );

    // The document supplied the title, not the store.
    let listed = playables(&rig);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["title"], "The Authoritative Album", "{listed:?}");
    let noudata = rig.files_with_ext("noudata");
    assert_eq!(noudata.len(), 1);
    let doc = plippa_noubin::parse_noudata(&std::fs::read(&noudata[0]).expect("read")).expect("parse");
    let source = doc.extra.get("plippaStoreSource").expect("provenance");
    assert_eq!(source["metadata"], "noudata", "{source}");
    assert_eq!(source["url"], KEY, "{source}");
    assert_eq!(source["releaseId"], RELEASE_ID, "{source}");

    // The duration cross-check found the document's length exact (not "unmeasured").
    let noudata_text = std::fs::read_to_string(&noudata[0]).expect("text");
    assert!(
        noudata_text.contains("expectedLength"),
        "the sidecar records the document's lengths"
    );

    // A replay — the same release downloaded again without `replace` — must not fetch the
    // document again and must not write a second copy.
    let before_folders = rig.files_with_ext("noudata").len();
    let before_disk = rig.snapshot();
    rig.net.reset_hits();
    rig.catalog.reset_hits();
    let replay = download(&rig, &[RELEASE_ID], Some(KEY));
    let document_hits = rig
        .net
        .hits()
        .iter()
        .filter(|url| url.ends_with("/web.noudata"))
        .count();
    assert_eq!(document_hits, 0, "a replay fetched the document again");
    assert_eq!(
        rig.net.hits().len(),
        0,
        "a replay must not download the audio again: {:?}",
        rig.net.hits()
    );
    // The consent sheet still reads the release detail, but nothing may ask for the
    // audio: a replay must never hit `download`/`stream` again.
    assert!(
        rig.catalog
            .hits()
            .iter()
            .all(|url| url.contains("/getAlbum.view")),
        "a replay asked the store for something other than the release detail: {:?}",
        rig.catalog.hits()
    );
    assert_eq!(replay["unite"]["state"], "exists", "{replay}");
    assert_eq!(replay["placedFiles"], 0, "a replay placed nothing: {replay}");
    let detail = replay["unite"]["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("already in the library"), "{detail}");
    assert_eq!(rig.files_with_ext("noudata").len(), before_folders, "no second copy");
    assert_eq!(before_disk, rig.snapshot(), "a replay wrote nothing");

    // And the second tap of the same release is answered from disk: zero requests to
    // either fixture.
    rig.net.reset_hits();
    rig.catalog.reset_hits();
    let (status, tap) = json(
        rig.port,
        "POST",
        "/api/stores/tap",
        &serde_json::json!({ "url": SABLE_URL }).to_string(),
    );
    assert_eq!(status, 200, "{tap}");
    assert_eq!(tap["state"], "owned_downloadable", "{tap}");
    assert_eq!(rig.net.hits().len(), 0, "a tap is network-free");
    assert_eq!(rig.catalog.hits().len(), 0, "a tap is network-free");
}

// ---------------------------------------------------------------------------
// 3. The tap flow: owned, not owned, and owned after the purchase
// ---------------------------------------------------------------------------

#[test]
fn store_e2e_tap_offers_download_then_purchase_then_download() {
    let rig = Rig::start("e2e-tap");
    connect_and_sync(&rig);

    // Owned: the cached release matches the link's title.
    rig.catalog.reset_hits();
    rig.net.reset_hits();
    let (status, owned) = json(
        rig.port,
        "POST",
        "/api/stores/tap",
        &serde_json::json!({ "url": SABLE_URL, "expectedDuration": 4 }).to_string(),
    );
    assert_eq!(status, 200, "{owned}");
    assert_eq!(owned["state"], "owned_downloadable", "{owned}");
    assert!(owned["purchaseUrl"].is_null(), "{owned}");
    assert_eq!(owned["candidates"][0]["storeReleaseId"], RELEASE_ID, "{owned}");
    assert!(
        owned["message"].as_str().is_some_and(|m| m.contains("You own this")),
        "{owned}"
    );
    assert!(rig.catalog.hits().is_empty(), "a tap must not touch the store");

    // Not owned: a purchase URL, and the interest is remembered.
    let absent = "https://someone-else.bandcamp.com/album/not-in-my-collection";
    let (status, missing) = json(
        rig.port,
        "POST",
        "/api/stores/tap",
        &serde_json::json!({ "url": absent }).to_string(),
    );
    assert_eq!(status, 200, "{missing}");
    assert_eq!(missing["state"], "not_owned", "{missing}");
    assert_eq!(missing["purchaseUrl"], absent, "{missing}");
    let wants = std::fs::read_to_string(
        rig.root
            .join(".plippa")
            .join("stores")
            .join("bandcamp.associations.json"),
    )
    .expect("the interest was recorded");
    assert!(wants.contains("not-in-my-collection"), "{wants}");

    // The person buys it, the collection is synced, the next tap offers the download.
    rig.catalog.add_release(
        "a:9",
        "Not In My Collection",
        "Someone Else",
        2026,
        &[("t:9", "Only", 1, 4)],
    );
    let sync = connect_and_sync(&rig);
    assert_eq!(sync["releases"], 4, "{sync}");
    let (status, bought) = json(
        rig.port,
        "POST",
        "/api/stores/tap",
        &serde_json::json!({ "url": absent }).to_string(),
    );
    assert_eq!(status, 200, "{bought}");
    assert_eq!(bought["state"], "owned_downloadable", "{bought}");
    assert!(bought["purchaseUrl"].is_null(), "{bought}");
    assert_eq!(bought["candidates"][0]["storeReleaseId"], "a:9", "{bought}");
}

// ---------------------------------------------------------------------------
// 4. Untrusted text stays text
// ---------------------------------------------------------------------------

#[test]
fn store_e2e_untrusted_title_is_data_not_markup() {
    let rig = Rig::start("e2e-xss");
    let hostile = "<img src=x onerror=alert(1)>\u{202E} & <script>alert(2)</script>";
    rig.catalog.add_release(
        "a:evil",
        hostile,
        "<b>Artist</b>",
        2026,
        &[("t:evil", "Track <img src=x onerror=alert(3)>\u{202E}", 1, 4)],
    );
    connect_and_sync(&rig);

    // The server passes the text through unchanged (it is the client's job to render it as
    // text, and escaping it here would corrupt a legitimate title).
    let (status, list) = get_json(rig.port, "/api/stores/bandcamp/collection?query=onerror");
    assert_eq!(status, 200, "{list}");
    assert_eq!(list["total"], 1, "{list}");
    assert_eq!(list["releases"][0]["title"], hostile);
    assert_eq!(list["releases"][0]["artist"], "<b>Artist</b>");

    // No portal script can turn that into markup: there is no HTML sink at all.
    let web_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("web");
    let mut checked = 0;
    for entry in std::fs::read_dir(&web_dir).expect("web dir").filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("js") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read script");
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        // The property/method forms only: prose comments in these files legitimately say
        // "never innerHTML".
        for sink in [
            ".innerHTML",
            ".outerHTML",
            "insertAdjacentHTML(",
            "document.write(",
            "eval(",
        ] {
            assert!(
                !text.contains(sink),
                "{name} uses {sink}; untrusted titles must go through textContent"
            );
        }
        checked += 1;
    }
    assert!(checked >= 5, "the portal scripts were found ({checked})");

    // The device never parses markup either: the screen code is Rust painting text.
    let ui_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plippa-ui/src");
    let mut ui_files = 0;
    for entry in walk_rs(&ui_src) {
        let text = std::fs::read_to_string(&entry).expect("read ui source");
        for sink in ["innerHTML", "insertAdjacentHTML", "document.write"] {
            assert!(!text.contains(sink), "{} uses {sink}", entry.display());
        }
        ui_files += 1;
    }
    assert!(ui_files > 10, "the device sources were found ({ui_files})");

    // The sidecar is an ingest path too (NBSecurityRequirements §3): a hostile title is
    // stored as text with control/bidi characters stripped, never as markup or a filename.
    let done = download(&rig, &["a:evil"], None);
    assert_eq!(done["state"], "done", "{done}");
    let noudata = rig.files_with_ext("noudata");
    assert_eq!(noudata.len(), 1, "{noudata:?}");
    let doc = plippa_noubin::parse_noudata(&std::fs::read(&noudata[0]).expect("read"))
        .expect("a hostile title must still produce a valid sidecar");
    let title = doc.playable_item_title.unwrap_or_default();
    assert!(
        title.contains("<img src=x onerror=alert(1)>"),
        "the visible text is kept, only unsafe characters are removed: {title:?}"
    );
    assert!(
        !title.contains('\u{202E}') && !title.chars().any(|c| c.is_control()),
        "a bidi/control character reached the sidecar: {title:?}"
    );
    assert!(title.chars().count() <= 256, "the title is capped: {title:?}");
    let media_title = doc
        .media_list
        .first()
        .and_then(|item| item.media_title.clone())
        .unwrap_or_default();
    assert!(media_title.contains("onerror=alert(3)"), "{media_title:?}");
    assert!(!media_title.contains('\u{202E}'), "{media_title:?}");
}

fn walk_rs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_rs(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 5. A CDN redirect to a private address is refused before any connection
// ---------------------------------------------------------------------------

#[test]
fn store_e2e_private_redirect_is_refused_before_any_connection() {
    let rig = Rig::start("e2e-ssrf");
    // A real listener, so "was it contacted?" is answered by the kernel rather than by a
    // flag the code under test could forget to set.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind private listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let private = listener.local_addr().expect("addr");
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    std::thread::spawn(move || loop {
        match listener.accept() {
            Ok(_) => {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    });

    // 1. A private address on the redirect hop, on a port the policy never dials. The
    //    kernel counter proves no socket was opened to it.
    *lock(&rig.net.redirect) = Some(format!("https://{}/track.wav", private));
    connect_and_sync(&rig);

    let status = download(&rig, &[RELEASE_ID], None);
    assert_ne!(status["state"], "done", "the download must fail: {status}");
    assert_eq!(status["items"][0]["state"], "failed", "{status}");
    assert!(
        matches!(
            status["items"][0]["errorCode"].as_str(),
            Some("blocked_target" | "redirect_refused")
        ),
        "the redirect must be refused by policy: {status}"
    );
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        0,
        "the private address was contacted"
    );
    let hits = rig.net.hits();
    assert_eq!(hits.len(), 1, "only the store's stream URL was requested: {hits:?}");
    assert!(hits[0].contains("stream.view"), "{hits:?}");
    assert!(rig.files_with_ext("wav").is_empty(), "nothing was written");
    assert!(rig.files_with_ext("noudata").is_empty(), "nothing was placed");
    assert!(rig.staging_dirs().is_empty(), "no partial was left behind");
    assert!(
        playables(&rig).is_empty(),
        "the library stayed empty rather than half-written"
    );

    // 2. The same private address on the port the policy does dial: the address itself is
    //    what refuses it, before a resolver or a socket is reached.
    *lock(&rig.net.redirect) = Some("https://127.0.0.1/track.wav".to_string());
    rig.net.reset_hits();
    let status = download(&rig, &[RELEASE_ID], None);
    assert_eq!(status["items"][0]["state"], "failed", "{status}");
    assert_eq!(
        status["items"][0]["errorCode"], "redirect_refused",
        "a private host is refused by the address policy: {status}"
    );
    assert!(
        status["items"][0]["error"]
            .as_str()
            .is_some_and(|e| e.contains("private or reserved")),
        "the refusal names the private network: {status}"
    );
    assert_eq!(accepted.load(Ordering::SeqCst), 0, "the private address was contacted");
    assert_eq!(rig.net.hits().len(), 1, "the redirect hop was never requested");
    assert!(rig.files_with_ext("wav").is_empty(), "nothing was written");
    assert!(playables(&rig).is_empty(), "the library stayed empty");
}

// ---------------------------------------------------------------------------
// 6. Simple-request CSRF is refused on every write route
// ---------------------------------------------------------------------------

#[test]
fn store_e2e_plain_text_writes_are_refused_everywhere() {
    let rig = Rig::start("e2e-csrf");
    connect_and_sync(&rig);
    let listed = playables(&rig);
    let _ = listed;

    // Every state-changing route this series added. A cross-site HTML form can only send
    // `text/plain`, so a body it could produce must be refused before routing.
    let routes: &[(&str, &str)] = &[
        ("POST", "/api/stores/bandcamp/connect"),
        ("POST", "/api/stores/bandcamp/validate"),
        ("POST", "/api/stores/bandcamp/sync"),
        ("POST", "/api/stores/bandcamp/sync/cancel"),
        ("DELETE", "/api/stores/bandcamp"),
        ("POST", "/api/stores/bandcamp/download/prepare"),
        ("POST", "/api/stores/bandcamp/download"),
        ("POST", "/api/stores/bandcamp/download/abc/cancel"),
        ("POST", "/api/stores/bandcamp/match"),
        ("POST", "/api/stores/bandcamp/match/confirm"),
        ("POST", "/api/stores/bandcamp/want"),
        ("POST", "/api/stores/tap"),
        ("DELETE", "/api/playables/1"),
        ("DELETE", "/api/playables/1/files/0"),
        ("DELETE", "/api/collections/1"),
    ];
    let simple = br#"{"force":true,"user":"x","consent":true}"#;
    for (method, path) in routes {
        let (status, _, body) = request(
            rig.port,
            method,
            path,
            &[("Content-Type", "text/plain")],
            simple,
        );
        assert_eq!(status, 415, "{method} {path} accepted a simple request: {body}");
        let (status, _, body) = request(
            rig.port,
            method,
            path,
            &[
                ("Content-Type", "application/json"),
                ("Origin", "http://evil.example"),
            ],
            simple,
        );
        assert_eq!(status, 403, "{method} {path} accepted a cross-origin write: {body}");
    }

    // Nothing changed: still connected, still synced, nothing deleted.
    let (_, stores) = get_json(rig.port, "/api/stores");
    assert_eq!(stores["stores"][0]["connected"], true, "{stores}");
    assert_eq!(stores["stores"][0]["cache"]["releases"], 3, "{stores}");
}

// ---------------------------------------------------------------------------
// 7-10. Bring-up degradation
// ---------------------------------------------------------------------------

#[test]
fn store_e2e_starts_without_a_secrets_directory() {
    let rig = Rig::with(
        "e2e-no-secrets",
        Arc::new(Catalog::standard()),
        Arc::new(Net::standard(4_000)),
        false,
    );

    let (status, stores) = get_json(rig.port, "/api/stores");
    assert_eq!(status, 200, "{stores}");
    assert_eq!(stores["secretsAvailable"], false, "{stores}");
    assert!(stores["secretsReason"].is_string(), "{stores}");

    let body = format!(r#"{{"user":"{USER}","password":"{PASSWORD}","consent":true}}"#);
    let (status, value) = json(rig.port, "POST", "/api/stores/bandcamp/connect", &body);
    assert_eq!(status, 503, "{value}");
    assert_eq!(value["code"], "unavailable", "{value}");

    // The library and the portal are untouched by the missing feature.
    let (status, _) = get_json(rig.port, "/api/status");
    assert_eq!(status, 200);
    let (status, value) = get_json(rig.port, "/api/playables");
    assert_eq!(status, 200, "{value}");
    let (status, value) = get_json(rig.port, "/api/stores/bandcamp/collection");
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["total"], 0, "no cache, no connected store: {value}");
    let (status, _, _) = request(rig.port, "GET", "/stores.js", &[], b"");
    assert_eq!(status, 200, "the portal script must still load");
}

#[test]
fn store_e2e_survives_a_corrupt_cache() {
    let rig = Rig::start("e2e-corrupt");
    connect_and_sync(&rig);
    assert!(!rig.catalog.hits().is_empty());

    // Corrupt the cache the way a bad shutdown or a full disk would.
    std::fs::write(rig.cache_file(), b"{ this is not json").expect("corrupt the cache");

    let (status, stores) = get_json(rig.port, "/api/stores");
    assert_eq!(status, 200, "{stores}");
    assert_eq!(stores["stores"][0]["cache"]["releases"], 0, "{stores}");
    assert_eq!(stores["stores"][0]["connected"], true, "the credentials still work");

    // The unreadable file was kept for the operator, not deleted.
    let dir = rig.root.join(".plippa").join("stores");
    let aside: Vec<String> = std::fs::read_dir(&dir)
        .expect("stores dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".corrupt-"))
        .collect();
    assert_eq!(aside.len(), 1, "the corrupt cache was renamed aside: {aside:?}");
    assert!(!rig.cache_file().exists(), "the corrupt file was moved, not left in place");

    // Browsing degrades to "empty and stale", and the library still serves.
    let (status, page) = get_json(rig.port, "/api/stores/bandcamp/collection");
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["total"], 0, "{page}");
    assert_eq!(page["stale"], true, "{page}");
    let (status, _) = get_json(rig.port, "/api/playables");
    assert_eq!(status, 200);

    // And a fresh sync repairs it.
    let sync = connect_and_sync(&rig);
    assert_eq!(sync["releases"], 3, "{sync}");
    let (_, page) = get_json(rig.port, "/api/stores/bandcamp/collection");
    assert_eq!(page["total"], 3, "{page}");
}

#[test]
fn store_e2e_read_only_media_volume_does_not_take_the_library_down() {
    let rig = Rig::start("e2e-read-only");
    connect_and_sync(&rig);
    // An item already in the library must survive a failed download.
    let first = download(&rig, &["a:2"], None);
    assert_eq!(first["state"], "done", "{first}");
    assert_eq!(playables(&rig).len(), 1);
    let existing_media = rig.files_with_ext("wav");
    assert_eq!(existing_media.len(), 2, "{existing_media:?}");

    // The media volume (not the metadata volume) goes read-only, as a full or remounted
    // card does.
    let media_dir = rig.root.join("Music");
    let modes = chmod_tree(&media_dir, 0o555);
    let refused = download(&rig, &[RELEASE_ID], None);
    restore_tree(&modes);

    assert_ne!(refused["state"], "done", "a read-only volume cannot complete: {refused}");
    assert!(
        !refused["errors"].as_array().is_some_and(Vec::is_empty),
        "the failure says why: {refused}"
    );
    assert!(
        refused["errors"]
            .as_array()
            .is_some_and(|errors| errors.iter().any(|e| e.as_str().unwrap_or("").contains("read-only")
                || e.as_str().unwrap_or("").contains("Permission denied"))),
        "the failure names the read-only volume: {refused}"
    );

    // The existing item is untouched and still listed; nothing half-written appeared.
    assert_eq!(playables(&rig).len(), 1);
    assert!(rig.staging_dirs().is_empty(), "no partial staging survived");
    assert_eq!(rig.files_with_ext("wav"), existing_media, "no new media file");
    assert_eq!(rig.files_with_ext("noudata").len(), 1, "no half-placed release");
    let (status, _) = get_json(rig.port, "/api/status");
    assert_eq!(status, 200);
}

/// Make every directory under `root` read-only, returning what to restore.
fn chmod_tree(root: &Path, mode: u32) -> Vec<(PathBuf, u32)> {
    use std::os::unix::fs::PermissionsExt;
    fn visit(dir: &Path, mode: u32, out: &mut Vec<(PathBuf, u32)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.is_dir() {
                let was = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
                out.push((path.clone(), was));
                visit(&path, mode, out);
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                    .expect("chmod");
            }
        }
    }
    let mut out = Vec::new();
    let was = std::fs::metadata(root).expect("stat").permissions().mode() & 0o777;
    out.push((root.to_path_buf(), was));
    visit(root, mode, &mut out);
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(mode)).expect("chmod");
    out
}

fn restore_tree(saved: &[(PathBuf, u32)]) {
    use std::os::unix::fs::PermissionsExt;
    // Children first: a parent that is still read-only would hide its own restore.
    for (path, mode) in saved.iter().rev() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(*mode)).expect("restore");
    }
}

#[test]
fn store_e2e_no_network_still_serves_the_library_and_the_cache() {
    let rig = Rig::start("e2e-offline");
    connect_and_sync(&rig);

    // The network disappears after the cache exists (the realistic case: a stick in a tent).
    rig.catalog.dead.store(true, Ordering::SeqCst);
    rig.net.dead.store(true, Ordering::SeqCst);

    // Browsing is cache-only and keeps working with zero requests.
    rig.catalog.reset_hits();
    let (status, page) = get_json(rig.port, "/api/stores/bandcamp/collection");
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["total"], 3, "{page}");
    assert!(rig.catalog.hits().is_empty(), "browsing hit the dead network");

    // The tap is answered from the cache, not the network.
    let (status, tap) = json(
        rig.port,
        "POST",
        "/api/stores/tap",
        &serde_json::json!({ "url": SABLE_URL }).to_string(),
    );
    assert_eq!(status, 200, "{tap}");
    assert_eq!(tap["state"], "owned_downloadable", "{tap}");

    // A download that needs the store fails cleanly and writes nothing.
    let (status, consent) = json(
        rig.port,
        "POST",
        "/api/stores/bandcamp/download/prepare",
        &serde_json::json!({ "releases": [RELEASE_ID] }).to_string(),
    );
    assert_ne!(status, 200, "the store cannot be read: {consent}");
    assert!(consent["code"].is_string(), "{consent}");
    assert!(rig.staging_dirs().is_empty());

    // The library and the portal are still up.
    let (status, _) = get_json(rig.port, "/api/status");
    assert_eq!(status, 200);
    let (status, _) = get_json(rig.port, "/api/playables");
    assert_eq!(status, 200);
    let (status, _, _) = request(rig.port, "GET", "/library.js", &[], b"");
    assert_eq!(status, 200);
}
