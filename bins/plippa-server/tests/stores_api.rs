//! BCS_03 over HTTP: the routes the portal and BCS_04 call, their status codes and JSON
//! shapes, and the promise that a broken secrets directory never takes the library down.
//!
//! The network is a scripted [`CountingTransport`]; nothing here opens a socket beyond
//! the loopback portal.

mod common;

use plippa_server::{RecordingIpc, ServerConfig, ServerService, StoreHub};
use plippa_store::{
    CountingTransport, HeaderMap, StoreError, Transport, TransportResponse,
};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const USER: &str = "fan@example.com";
const PASSWORD: &str = "s3cret-copy-pasted";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn json_response(body: String) -> TransportResponse {
    TransportResponse {
        status: 200,
        headers: vec![(
            "Content-Type".to_string(),
            "application/json; charset=utf-8".to_string(),
        )],
        body: body.into_bytes(),
        final_url: "https://bandcamp.com/api/subsonic/rest/response.view".to_string(),
        redirects: Vec::new(),
    }
}

fn artists_ok() -> TransportResponse {
    json_response(
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","artists":{"index":[]}}}"#
            .to_string(),
    )
}

fn artists_bad() -> TransportResponse {
    json_response(
        r#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"Wrong username or password"}}}"#
            .to_string(),
    )
}

fn album_page(start: usize, count: usize) -> TransportResponse {
    let albums: Vec<serde_json::Value> = (start..start + count)
        .map(|index| {
            serde_json::json!({
                "id": format!("a:{index}"),
                "name": format!("Release {index}"),
                "artist": "The Artist",
                "year": 2020,
                "songCount": 5,
                "duration": 600,
                "coverArt": format!("c:{index}"),
            })
        })
        .collect();
    json_response(
        serde_json::json!({
            "subsonic-response": { "status": "ok", "albumList2": { "album": albums } }
        })
        .to_string(),
    )
}

/// A transport that answers `ping` but nothing else, to prove connect never relies on it.
struct PingOnlyTransport {
    calls: Mutex<Vec<String>>,
}

impl Transport for PingOnlyTransport {
    fn get(
        &self,
        url: &str,
        _headers: &HeaderMap,
        _max_bytes: u64,
    ) -> Result<TransportResponse, StoreError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(url.to_string());
        if url.contains("ping.view") {
            Ok(json_response(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#.to_string(),
            ))
        } else {
            Err(StoreError::transport_url(url, "unexpected endpoint"))
        }
    }
}

struct Rig {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    port: u16,
}

fn start(hub_transport: Arc<dyn Transport + Send + Sync>) -> Rig {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path().join("library");
    std::fs::create_dir_all(&root).expect("library");
    let secrets = tmp.path().join("secrets");
    let hub = Arc::new(StoreHub::with_transport(Some(secrets), hub_transport));
    hub.set_library_root(Some(root.clone()));

    let mut svc = ServerService::start(
        Some(root.clone()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    svc.set_stores_hub(Arc::clone(&hub));
    let svc = Arc::new(Mutex::new(svc));

    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().expect("bind addr"),
        library_root: Some(root.clone()),
        secrets_dir: None,
        portal_url: None,
        ..ServerConfig::default()
    };
    common::serve_open(cfg, svc);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Rig {
                _tmp: tmp,
                root,
                port,
            };
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

/// A rig whose hub uses the scripted transport (the common case).
fn rig() -> (Rig, Arc<CountingTransport>) {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path().join("library");
    std::fs::create_dir_all(&root).expect("library");
    let secrets = tmp.path().join("secrets");
    let transport = Arc::new(CountingTransport::new());
    let hub = Arc::new(
        StoreHub::with_transport(
            Some(secrets),
            Arc::clone(&transport) as Arc<dyn Transport + Send + Sync>,
        )
        // Small pages and a short pause keep the tests quick; the production defaults
        // (200 per page, 250 ms apart) are covered by the unit tests.
        .with_sync_tuning(2, Duration::from_millis(5)),
    );
    hub.set_library_root(Some(root.clone()));

    let mut svc = ServerService::start(
        Some(root.clone()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    svc.set_stores_hub(Arc::clone(&hub));
    let svc = Arc::new(Mutex::new(svc));

    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().expect("bind addr"),
        library_root: Some(root.clone()),
        secrets_dir: None,
        portal_url: None,
        ..ServerConfig::default()
    };
    common::serve_open(cfg, svc);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            let rig = Rig {
                _tmp: tmp,
                root,
                port,
            };
            return (rig, transport);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Returns `(status, headers, body text)`.
fn request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
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
        .and_then(|s| s.parse::<u16>().ok())
        .expect("status code");
    let (raw_head, raw_body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let headers = raw_head
        .split("\r\n")
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn stores_connect_rejects_bad_credentials_without_leaking_them() {
    let (rig, transport) = rig();
    let port = rig.port;
    transport.push_response(artists_bad());

    let body = format!(r#"{{"user":"{USER}","password":"{PASSWORD}","consent":true}}"#);
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/connect", &body);
    assert_eq!(status, 401, "{value}");
    assert_eq!(value["code"], "bad_credentials");
    let message = value["error"].as_str().unwrap_or_default();
    assert!(!message.contains(PASSWORD), "{message}");
    assert!(!message.contains(USER), "{message}");

    // Nothing was saved and the status still says "not connected".
    let (status, stores) = get_json(port, "/api/stores");
    assert_eq!(status, 200);
    assert_eq!(stores["stores"][0]["connected"], false);
    assert!(!rig.root.join("secrets").join("bandcamp.json").exists());
    // And the status payload the portal receives is credential-free.
    assert!(!stores.to_string().contains(PASSWORD));
    assert!(!stores.to_string().contains(USER));
}

#[test]
fn stores_connect_never_uses_ping() {
    let calls = Arc::new(PingOnlyTransport {
        calls: Mutex::new(Vec::new()),
    });
    let rig = start(Arc::clone(&calls) as Arc<dyn Transport + Send + Sync>);
    let body = format!(r#"{{"user":"{USER}","password":"{PASSWORD}","consent":true}}"#);
    let (status, value) = json(rig.port, "POST", "/api/stores/bandcamp/connect", &body);
    assert_ne!(status, 200, "connect must not succeed: {value}");
    let recorded = calls
        .calls
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert!(
        recorded.iter().any(|url| url.contains("getArtists.view")),
        "the check must call getArtists: {recorded:?}"
    );
    assert!(
        !recorded.iter().any(|url| url.contains("ping.view")),
        "connect must never call ping: {recorded:?}"
    );
}

#[test]
fn stores_write_routes_require_json_and_consent() {
    let (rig, transport) = rig();
    transport.push_response(artists_ok());
    let port = rig.port;

    // No Content-Type: refused before any credential is looked at.
    let (status, _, _) = request(
        port,
        "POST",
        "/api/stores/bandcamp/connect",
        &[],
        br#"{"user":"a","password":"b","consent":true}"#,
    );
    assert_eq!(status, 415);

    // Consent is required and explicit.
    let body = format!(r#"{{"user":"{USER}","password":"{PASSWORD}"}}"#);
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/connect", &body);
    assert_eq!(status, 403, "{value}");
    assert_eq!(value["code"], "consent_required");
    assert_eq!(transport.call_count(), 0, "nothing is sent without consent");

    // Cross-origin writes are refused too.
    let (status, _, _) = request(
        port,
        "POST",
        "/api/stores/bandcamp/connect",
        &[
            ("Content-Type", "application/json"),
            ("Origin", "http://evil.example"),
        ],
        body.as_bytes(),
    );
    assert_eq!(status, 403);
}

#[test]
fn stores_full_lifecycle_over_http() {
    let (rig, transport) = rig();
    let port = rig.port;
    transport.push_response(artists_ok());
    transport.push_response(album_page(0, 2));
    transport.push_response(album_page(2, 1));

    // 1. Connect.
    let body = format!(r#"{{"user":" {USER} ","password":" {PASSWORD} ","consent":true}}"#);
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/connect", &body);
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["ok"], true);
    assert_eq!(value["store"]["connected"], true);
    assert_eq!(value["store"]["id"], "bandcamp");
    assert!(!value.to_string().contains(PASSWORD));

    // 2. Status.
    let (status, stores) = get_json(port, "/api/stores");
    assert_eq!(status, 200, "{stores}");
    assert_eq!(stores["secretsAvailable"], true);
    assert_eq!(stores["stores"][0]["connected"], true);
    assert_eq!(stores["stores"][0]["rejected"], false);
    assert_eq!(stores["stores"][0]["cache"]["releases"], 0);
    assert_eq!(stores["stores"][0]["cache"]["stale"], true);

    // 3. Sync starts with 202 and finishes in the background.
    let (status, started) = json(port, "POST", "/api/stores/bandcamp/sync", "");
    assert_eq!(status, 202, "{started}");
    assert_eq!(started["ok"], true);
    assert_eq!(started["sync"]["state"], "running");

    let deadline = Instant::now() + Duration::from_secs(10);
    let finished = loop {
        let (status, sync) = get_json(port, "/api/stores/bandcamp/sync");
        assert_eq!(status, 200, "{sync}");
        assert!(
            sync["state"] != "running" || Instant::now() < deadline,
            "sync never finished"
        );
        if sync["state"] != "running" {
            break sync;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(finished["state"], "done", "{finished}");
    assert_eq!(finished["pages"], 2);
    assert_eq!(finished["releases"], 3);

    // 4. The cache is readable without a network call, and is private on disk.
    let before = transport.call_count();
    let (status, releases) = get_json(port, "/api/stores/bandcamp/releases");
    assert_eq!(status, 200, "{releases}");
    assert_eq!(releases["releases"].as_array().map(Vec::len), Some(3));
    assert_eq!(releases["cache"]["releases"], 3);
    assert_eq!(releases["cache"]["complete"], true);
    assert_eq!(transport.call_count(), before, "a browse is cache-only");

    let cache = rig.root.join(".plippa/stores/bandcamp.json");
    assert!(cache.exists());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&cache).expect("stat").permissions().mode() & 0o777,
        0o600
    );

    // 5. One release, fetched through the store (the refresh-on-miss route).
    transport.push_response(json_response(
        serde_json::json!({
            "subsonic-response": {
                "status": "ok",
                "album": {
                    "id": "a:0",
                    "name": "Release 0",
                    "artist": "The Artist",
                    "songCount": 1,
                    "song": [{ "id": "t:1", "title": "One", "track": 1, "duration": 3 }]
                }
            }
        })
        .to_string(),
    ));
    let (status, detail) = get_json(port, "/api/stores/bandcamp/releases/a%3A0");
    assert_eq!(status, 200, "{detail}");
    assert_eq!(detail["release"]["id"], "a:0");
    assert_eq!(detail["tracks"].as_array().map(Vec::len), Some(1));

    // 6. Disconnect removes both secrets and cache.
    let (status, value) = json(port, "DELETE", "/api/stores/bandcamp", "");
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["ok"], true);
    assert!(!rig.root.join("secrets").join("bandcamp.json").exists());
    assert!(!cache.exists());
    let (_, stores) = get_json(port, "/api/stores");
    assert_eq!(stores["stores"][0]["connected"], false);
    assert_eq!(stores["stores"][0]["cache"]["releases"], 0);
}

#[test]
fn stores_sync_is_one_at_a_time_and_cancellable_over_http() {
    let (rig, transport) = rig();
    let port = rig.port;
    transport.push_response(artists_ok());
    for start in (0..40).step_by(2) {
        transport.push_response(album_page(start, 2));
    }

    let body = format!(r#"{{"user":"{USER}","password":"{PASSWORD}","consent":true}}"#);
    let (status, _) = json(port, "POST", "/api/stores/bandcamp/connect", &body);
    assert_eq!(status, 200);

    let (status, _) = json(port, "POST", "/api/stores/bandcamp/sync", "");
    assert_eq!(status, 202);
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/sync", "");
    assert_eq!(status, 429, "{value}");
    assert_eq!(value["code"], "busy");

    let (status, value) = json(port, "POST", "/api/stores/bandcamp/sync/cancel", "");
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["ok"], true);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, sync) = get_json(port, "/api/stores/bandcamp/sync");
        if sync["state"] != "running" {
            assert_eq!(sync["cancelled"], true, "{sync}");
            break;
        }
        assert!(Instant::now() < deadline, "cancel was not honoured");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn stores_degrade_when_the_secrets_directory_is_unwritable() {
    // The default hub (no override) with a secrets path that cannot be a directory, and
    // no library at all: the portal and the library routes still work.
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path().join("library");
    std::fs::create_dir_all(&root).expect("library");
    let blocked = tmp.path().join("blocked");
    std::fs::write(&blocked, b"not a directory").expect("blocker");

    let svc = Arc::new(Mutex::new(ServerService::start_with_stores(
        Some(root.clone()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
        Some(blocked.clone()),
    )));
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().expect("bind addr"),
        library_root: Some(root.clone()),
        secrets_dir: Some(blocked),
        portal_url: None,
        ..ServerConfig::default()
    };
    common::serve_open(cfg, svc);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // Stores report themselves unavailable, with a reason...
    let (status, stores) = get_json(port, "/api/stores");
    assert_eq!(status, 200, "{stores}");
    assert_eq!(stores["secretsAvailable"], false);
    assert!(stores["secretsReason"].is_string());
    assert_eq!(stores["stores"][0]["connected"], false);

    // ...and connecting says so instead of reading credentials.
    let body = format!(r#"{{"user":"{USER}","password":"{PASSWORD}","consent":true}}"#);
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/connect", &body);
    assert_eq!(status, 503, "{value}");
    assert_eq!(value["code"], "unavailable");

    // The library still serves.
    let (status, _) = get_json(port, "/api/status");
    assert_eq!(status, 200);
    let (status, collections) = get_json(port, "/api/collections");
    assert_eq!(status, 200, "{collections}");
    assert!(collections.is_array() || collections["collections"].is_array());
    // The status summary says the store feature is off, without breaking the payload.
    let (_, status_body) = get_json(port, "/api/status");
    let text = status_body.to_string();
    assert!(
        text.contains("\"available\":false") || status_body["stores"]["available"] == false,
        "{status_body}"
    );

    // The portal script is still served (the UI must load with the feature off).
    let (status, _, _) = request(port, "GET", "/stores.js", &[], b"");
    assert_eq!(status, 200);
}

// ---------------------------------------------------------------------------
// BCS_06: ownership matching over HTTP
// ---------------------------------------------------------------------------

/// Write a one-release collection cache into the rig's library, the way a sync leaves it.
fn write_sable_cache(root: &std::path::Path, crawled_at: u64) {
    let cache = serde_json::json!({
        "storeId": "bandcamp",
        "releases": [{
            "id": "t:2970735116",
            "title": "We're Getting Older Baby",
            "artist": "Sable",
            "year": 2026,
            "trackCount": 1,
            "durationMs": 216000,
            "cover": null,
            "webUrl": null,
            "createdUnix": null
        }],
        "pageSize": 200,
        "crawledAt": crawled_at,
        "complete": true,
        "lastError": null
    });
    let dir = root.join(".plippa").join("stores");
    std::fs::create_dir_all(&dir).expect("stores dir");
    std::fs::write(
        dir.join("bandcamp.json"),
        serde_json::to_vec(&cache).expect("encode"),
    )
    .expect("write cache");
}

#[test]
fn stores_match_routes_answer_from_cache_and_reject_client_scores() {
    let (rig, transport) = rig();
    let port = rig.port;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    write_sable_cache(&rig.root, now);
    let link = "https://sable-music.bandcamp.com/track/were-getting-older-baby";

    // GET: the answer comes from the library and the cache, with zero requests and no
    // write. `expectedDuration` promotes the exact title to a usable match.
    let (status, value) = get_json(
        port,
        &format!("/api/stores/bandcamp/match?url={link}&expectedDuration=216"),
    );
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["url"], link);
    assert_eq!(value["candidates"][0]["storeReleaseId"], "t:2970735116");
    assert_eq!(value["candidates"][0]["source"], "cache");
    assert!(value["confidence"].as_f64().expect("score") >= 0.9, "{value}");
    assert_eq!(value["autoUsable"], true);
    assert_eq!(value["requiresConfirmation"], false);
    assert_eq!(value["reason"], serde_json::Value::Null);
    assert_eq!(transport.call_count(), 0, "{:?}", transport.urls());
    let associations = rig.root.join(".plippa/stores/bandcamp.associations.json");
    assert!(!associations.exists(), "GET must not write anything");

    // A link that is not an https Bandcamp release link is a client error, not a miss.
    let (status, value) = get_json(
        port,
        "/api/stores/bandcamp/match?url=http%3A%2F%2Fsable-music.bandcamp.com%2Ftrack%2Fx",
    );
    assert_eq!(status, 400, "{value}");
    assert_eq!(value["code"], "invalid_url");
    let (status, _) = get_json(port, "/api/stores/bandcamp/match");
    assert_eq!(status, 400, "a missing url is refused");

    // POST confirm: the client claims 0.99, the server records the score it computed.
    let body = serde_json::json!({
        "url": link,
        "candidate": { "storeReleaseId": "t:2970735116", "confidence": 0.99 }
    })
    .to_string();
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/match/confirm", &body);
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["ok"], true);
    assert_eq!(value["storeReleaseId"], "t:2970735116");
    assert_eq!(value["confidence"], 0.9);
    assert_eq!(value["provenanceUpdated"], 0);
    assert!(associations.exists());
    assert_eq!(transport.call_count(), 0);

    // The next lookup is stage 1: the association answers, still with no request.
    let (status, value) = get_json(port, &format!("/api/stores/bandcamp/match?url={link}"));
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["candidates"][0]["source"], "association");
    assert_eq!(value["candidates"][0]["confidence"], 0.99);
    assert_eq!(value["autoUsable"], true);
    assert_eq!(transport.call_count(), 0);

    // A candidate the server cannot verify is refused, whatever score the client sends.
    let body = serde_json::json!({
        "url": link,
        "candidate": { "storeReleaseId": "a:invented", "confidence": 0.99 }
    })
    .to_string();
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/match/confirm", &body);
    assert_eq!(status, 404, "{value}");
    assert_eq!(value["code"], "unknown_release");

    // A POST body is required, and it is not a GET-shaped form post.
    let (status, _, _) = request(port, "POST", "/api/stores/bandcamp/match", &[], b"");
    assert_eq!(status, 415, "the write routes require a JSON body");
}

// ---------------------------------------------------------------------------
// SECFIX_04 (SRV-06): cache-only GETs, the guarded lookup POST and the limiter
// ---------------------------------------------------------------------------

// The ad-hoc limiter's burst, mirroring `stores::ADHOC_BURST` (the module is private to
// the crate; the server tests know the documented number from the SECFIX_04 decision log).
const ADHOC_BURST: usize = 5;

fn connect_ok(port: u16, transport: &CountingTransport) {
    transport.push_response(artists_ok());
    let body = format!(r#"{{"user":"{USER}","password":"{PASSWORD}","consent":true}}"#);
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/connect", &body);
    assert_eq!(status, 200, "{value}");
}

/// A `getAlbum` detail reply, the answer one lookup uses.
fn album_detail(id: &str, title: &str) -> TransportResponse {
    json_response(
        serde_json::json!({
            "subsonic-response": {
                "status": "ok",
                "album": {
                    "id": id,
                    "name": title,
                    "artist": "The Artist",
                    "songCount": 1,
                    "song": [{ "id": "t:1", "title": "One", "track": 1, "duration": 3 }]
                }
            }
        })
        .to_string(),
    )
}

#[test]
fn get_release_routes_never_call_the_store_on_a_cache_miss() {
    let (rig, transport) = rig();
    let port = rig.port;
    connect_ok(port, &transport);
    assert_eq!(transport.call_count(), 1, "only the connect check so far");

    // A hostile page firing these blind with an `<img src>` must reach no store at all.
    for path in [
        "/api/stores/bandcamp/release/t%3A1",
        "/api/stores/bandcamp/release/t%3A1/cover",
        "/api/stores/bandcamp/releases/t%3A1",
        // A client id that looks like a URL is an id like any other, never a fetch target
        // (slashes cannot appear here: the request target would be refused).
        "/api/stores/bandcamp/release/https%3Aevil.example%3Ftrack%3D1",
        "/api/stores/bandcamp/release/https%3Aevil.example%3Ftrack%3D1/cover",
    ] {
        let (status, value) = get_json(port, path);
        assert_eq!(status, 409, "{path}: {value}");
        assert_eq!(value["code"], "not_cached", "{path}: {value}");
    }
    assert_eq!(
        transport.call_count(),
        1,
        "a cache miss made an outbound call: {:?}",
        transport.urls()
    );
    // Nothing was written either: a GET is not a lookup.
    assert!(!rig
        .root
        .join(".plippa/stores/bandcamp.json")
        .exists());
}

#[test]
fn the_lookup_post_requires_json_and_the_guards_and_is_rate_limited() {
    let (rig, transport) = rig();
    let port = rig.port;
    connect_ok(port, &transport);
    let before = transport.call_count();

    // No Content-Type: refused before the body is parsed.
    let (status, _, _) = request(
        port,
        "POST",
        "/api/stores/bandcamp/release",
        &[],
        br#"{"id":"a:1"}"#,
    );
    assert_eq!(status, 415);
    // Cross-origin writes are refused too.
    let (status, _, _) = request(
        port,
        "POST",
        "/api/stores/bandcamp/release",
        &[
            ("Content-Type", "application/json"),
            ("Origin", "http://evil.example"),
        ],
        br#"{"id":"a:1"}"#,
    );
    assert_eq!(status, 403);
    // The body is capped (MAX_LOOKUP_BODY is 512 bytes).
    let huge = format!(r#"{{"id":"{}"}}"#, "a".repeat(600));
    let (status, _, _) = request(
        port,
        "POST",
        "/api/stores/bandcamp/release",
        &[("Content-Type", "application/json")],
        huge.as_bytes(),
    );
    assert_eq!(status, 413);
    // A missing or empty id is a client error, not a lookup.
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/release", r#"{"id":""}"#);
    assert_eq!(status, 400, "{value}");
    assert_eq!(value["code"], "invalid_request");
    assert_eq!(
        transport.call_count(),
        before,
        "a refused lookup reached the store: {:?}",
        transport.urls()
    );

    // A valid lookup answers 200 and is written to the cache.
    transport.push_response(album_detail("a:1", "Looked Up"));
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/release", r#"{"id":"a:1"}"#);
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["release"]["id"], "a:1");
    assert_eq!(value["cached"], false);
    assert!(rig.root.join(".plippa/stores/bandcamp.json").exists());
    // The next GET is a cache hit: no further lookup needed.
    transport.push_response(album_detail("a:1", "Looked Up"));
    let before = transport.call_count();
    let (status, value) = get_json(port, "/api/stores/bandcamp/release/a%3A1");
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["cached"], true);
    assert_eq!(value["release"]["id"], "a:1");
    assert!(transport.call_count() > before, "a cached view fetches tracks");
}

#[test]
fn a_second_lookup_inside_the_limit_is_refused_with_429() {
    // The bucket allows ADHOC_BURST (5) lookups, then one per second: the sixth inside the
    // burst window is refused with a stable code and no outbound call.
    let (rig, transport) = rig();
    let port = rig.port;
    connect_ok(port, &transport);
    let burst = ADHOC_BURST;
    for index in 0..burst {
        transport.push_response(album_detail(&format!("a:{index}"), "In Burst"));
        let body = format!(r#"{{"id":"a:{index}"}}"#);
        let (status, value) = json(port, "POST", "/api/stores/bandcamp/release", &body);
        assert_eq!(status, 200, "lookup {index}: {value}");
    }
    let before = transport.call_count();
    let (status, value) = json(port, "POST", "/api/stores/bandcamp/release", r#"{"id":"a:burst"}"#);
    assert_eq!(status, 429, "{value}");
    assert_eq!(value["code"], "rate_limited");
    assert_eq!(transport.call_count(), before, "a refused lookup still called");
    // The answer says nothing about the credentials or the refused id's content.
    assert!(!value.to_string().contains(PASSWORD));
}

#[test]
fn sync_and_download_are_not_starved_by_the_adhoc_limiter() {
    let (rig, transport) = rig();
    let port = rig.port;
    connect_ok(port, &transport);

    // Spend the ad-hoc budget on lookups. Each of these is inside the burst, so each is
    // admitted and answered.
    for index in 0..ADHOC_BURST {
        transport.push_response(album_detail(&format!("a:{index}"), "In Burst"));
        let body = format!(r#"{{"id":"a:{index}"}}"#);
        let (status, _) = json(port, "POST", "/api/stores/bandcamp/release", &body);
        assert_eq!(status, 200);
    }
    // Prove the refusal without depending on the clock: the bucket refills one token a
    // second, so a burst that is slow to run can be handed a token back. Probe with
    // nothing queued instead — an admitted probe reaches the transport and fails (5xx),
    // a refused one is 429, and neither leaves a scripted answer behind. The loop ends
    // within a couple of attempts because requests are milliseconds apart.
    let mut refused = false;
    for _ in 0..(ADHOC_BURST * 4) {
        let (status, value) =
            json(port, "POST", "/api/stores/bandcamp/release", r#"{"id":"a:none"}"#);
        if status == 429 {
            refused = true;
            break;
        }
        assert!(
            status >= 500,
            "an admitted lookup with no scripted answer must fail, not {status}: {value}"
        );
    }
    assert!(refused, "the limiter must refuse once the burst is spent");

    // The sync crawl is user-initiated and bounded by its own page cap: not starved.
    transport.push_response(album_page(0, 2));
    transport.push_response(album_page(2, 1));
    let (status, started) = json(port, "POST", "/api/stores/bandcamp/sync", "");
    assert_eq!(status, 202, "{started}");
    let deadline = Instant::now() + Duration::from_secs(10);
    let finished = loop {
        let (_, sync) = get_json(port, "/api/stores/bandcamp/sync");
        if sync["state"] != "running" {
            break sync;
        }
        assert!(Instant::now() < deadline, "sync never finished: {sync}");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(finished["state"], "done", "{finished}");

    // The consent-gated download prepare is not starved either.
    transport.push_response(album_detail("a:0", "Release 0"));
    let (status, value) = json(
        port,
        "POST",
        "/api/stores/bandcamp/download/prepare",
        r#"{"releases":["a:0"]}"#,
    );
    assert_eq!(status, 200, "{value}");
    assert!(value["consentId"].is_string(), "{value}");
}

#[test]
fn a_cached_release_cover_is_still_served_to_the_device_confirm_screen() {
    let (rig, transport) = rig();
    let port = rig.port;
    connect_ok(port, &transport);
    transport.push_response(album_page(0, 1));
    let (status, _) = json(port, "POST", "/api/stores/bandcamp/sync", "");
    assert_eq!(status, 202);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, sync) = get_json(port, "/api/stores/bandcamp/sync");
        if sync["state"] != "running" {
            assert_eq!(sync["state"], "done", "{sync}");
            break;
        }
        assert!(Instant::now() < deadline, "sync never finished");
        std::thread::sleep(Duration::from_millis(10));
    }

    // BCS_11 D1: the Confirm screen shows the cover of a release in the *cached*
    // collection. The cover id comes from the cache, not from the client.
    let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 16, 0, 0];
    transport.push_response(TransportResponse {
        status: 200,
        headers: vec![("Content-Type".to_string(), "image/jpeg".to_string())],
        body: jpeg,
        final_url: "https://bandcamp.com/api/subsonic/rest/getCoverArt.view".to_string(),
        redirects: Vec::new(),
    });
    let (status, headers, body) = request(port, "GET", "/api/stores/bandcamp/release/a%3A0/cover", &[], b"");
    assert_eq!(status, 200, "{body} {:?}", transport.urls());
    assert!(
        headers.to_lowercase().contains("image/jpeg"),
        "the sniffer decides the type: {headers}"
    );

    // A second request is served from the bounded in-memory cover cache.
    let before = transport.call_count();
    let (status, _, _) = request(port, "GET", "/api/stores/bandcamp/release/a%3A0/cover", &[], b"");
    assert_eq!(status, 200);
    assert_eq!(transport.call_count(), before, "the cover was fetched twice");
}
