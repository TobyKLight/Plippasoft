//! SECFIX_04 (SRV-05): the store state files on the stick are somebody else's input.
//!
//! A multi-GB sparse cache, a `/dev/zero` symlink or a FIFO named like the cache used to be
//! read with plain `fs::read` on every status poll and every boot: the server allocated the
//! apparent size, followed the link, or blocked forever. Each case below must answer "no
//! cache" (which is what the portal shows as "not synced yet") and must not allocate or hang.
//!
//! The reads are driven through the public `StoreHub` surface (`status`, `match_release`,
//! `lookup_release`), so the test proves the feature's behaviour, not a private helper's.
//! `getrusage(ru_maxrss)` is process-wide, so this file deliberately holds only the hostile
//! stick cases and is run as its own test binary.

mod common;

use plippa_server::{MatchRequest, RecordingIpc, ServerConfig, ServerService, StoreHub, StoresStatus};
use plippa_store::{CountingTransport, Transport, TransportResponse};
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 8 GiB of apparent size, almost none of it allocated (the review's sparse-file case).
const SPARSE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// The most a refusal may add to the process peak: the bug allocated the whole 8 GiB.
const MEMORY_BUDGET_KB: i64 = 128 * 1024;

fn mkfifo(path: &Path) {
    extern "C" {
        fn mkfifo(path: *const std::os::raw::c_char, mode: u32) -> i32;
    }
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { mkfifo(c.as_ptr(), 0o600) }, 0);
}

fn peak_rss_kb() -> i64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
    usage.ru_maxrss
}

/// A library whose `.plippa/stores` directory the test may populate.
fn library(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path().join(name);
    fs::create_dir_all(root.join(".plippa/stores")).expect("state dirs");
    (tmp, root)
}

fn stores(root: &Path) -> PathBuf {
    root.join(".plippa/stores")
}

fn hub(root: &Path, secrets: Option<PathBuf>) -> Arc<StoreHub> {
    let hub = StoreHub::with_transport(
        secrets,
        Arc::new(CountingTransport::new()) as Arc<dyn Transport + Send + Sync>,
    );
    hub.set_library_root(Some(root.to_path_buf()));
    Arc::new(hub)
}

fn sparse(path: &Path) {
    let file = fs::File::create(path).expect("create sparse");
    file.set_len(SPARSE_BYTES).expect("set_len");
}

fn association_request() -> MatchRequest {
    MatchRequest {
        url: "https://sable-music.bandcamp.com/track/were-getting-older-baby".to_string(),
        expected_duration: None,
        allow_search: false,
    }
}

/// `status()` reads the cache and the preferences; run it with a bound so a blocking open
/// fails the test instead of hanging the suite.
fn status_within(hub: Arc<StoreHub>, bound: Duration) -> StoresStatus {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(hub.status());
    });
    rx.recv_timeout(bound)
        .expect("the store status read blocked on a hostile stick")
}

/// `match_release` reads the associations file; same bound.
fn match_within(hub: Arc<StoreHub>, bound: Duration) -> plippa_server::MatchResult {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(hub.match_release(&association_request()));
    });
    rx.recv_timeout(bound)
        .expect("the associations read blocked on a hostile stick")
        .expect("a hostile associations file is 'nothing recorded', not an error")
}

// ---------------------------------------------------------------------------
// SRV-05: the three state files
// ---------------------------------------------------------------------------

#[test]
fn a_sparse_multi_gb_cache_file_is_refused_without_allocating() {
    let (_tmp, root) = library("sparse-cache");
    sparse(&stores(&root).join("bandcamp.json"));

    let before = peak_rss_kb();
    let started = Instant::now();
    let status = status_within(hub(&root, None), Duration::from_secs(5));
    let elapsed = started.elapsed();
    let after = peak_rss_kb();

    assert_eq!(status.stores[0].cache.releases, 0, "{status:?}");
    assert!(!status.stores[0].cache.complete);
    assert!(
        elapsed < Duration::from_secs(2),
        "a sparse file must be refused at once, took {elapsed:?}"
    );
    assert!(
        after - before < MEMORY_BUDGET_KB,
        "the read allocated {} MiB of an 8 GiB sparse file",
        (after - before) / 1024
    );
    // Refused, not destroyed: the operator can still look at what is on the stick.
    assert!(stores(&root).join("bandcamp.json").exists());
}

#[test]
fn a_sparse_multi_gb_prefs_file_is_refused_without_allocating() {
    let (_tmp, root) = library("sparse-prefs");
    sparse(&stores(&root).join("bandcamp.prefs.json"));

    let before = peak_rss_kb();
    let status = status_within(hub(&root, None), Duration::from_secs(5));
    let after = peak_rss_kb();

    assert_eq!(status.stores[0].cache.releases, 0);
    assert_eq!(status.stores[0].last_validated, None);
    assert!(
        after - before < MEMORY_BUDGET_KB,
        "preferences allocated {} MiB",
        (after - before) / 1024
    );
    assert!(stores(&root).join("bandcamp.prefs.json").exists());
}

#[test]
fn a_sparse_multi_gb_associations_file_is_refused_without_allocating() {
    let (_tmp, root) = library("sparse-associations");
    sparse(&stores(&root).join("bandcamp.associations.json"));

    let before = peak_rss_kb();
    let started = Instant::now();
    let result = match_within(hub(&root, None), Duration::from_secs(5));
    let elapsed = started.elapsed();
    let after = peak_rss_kb();

    assert!(result.owned.is_none(), "{result:?}");
    assert!(result.candidates.is_empty(), "{result:?}");
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    assert!(
        after - before < MEMORY_BUDGET_KB,
        "associations allocated {} MiB",
        (after - before) / 1024
    );
    assert!(stores(&root).join("bandcamp.associations.json").exists());
}

#[test]
fn a_symlink_to_dev_zero_is_refused() {
    let (_tmp, root) = library("dev-zero");
    for name in [
        "bandcamp.json",
        "bandcamp.prefs.json",
        "bandcamp.associations.json",
    ] {
        symlink("/dev/zero", stores(&root).join(name)).expect("symlink");
    }
    let hub = hub(&root, None);

    let before = peak_rss_kb();
    let started = Instant::now();
    let status = status_within(Arc::clone(&hub), Duration::from_secs(5));
    let result = match_within(Arc::clone(&hub), Duration::from_secs(5));
    let elapsed = started.elapsed();
    let after = peak_rss_kb();

    assert_eq!(status.stores[0].cache.releases, 0);
    assert!(result.candidates.is_empty(), "{result:?}");
    assert!(elapsed < Duration::from_secs(2), "took {elapsed:?}");
    assert!(
        after - before < MEMORY_BUDGET_KB,
        "a /dev/zero link allocated {} MiB",
        (after - before) / 1024
    );
    // The links themselves are left alone (nothing is renamed or deleted).
    assert!(stores(&root).join("bandcamp.json").exists());
    assert!(stores(&root).join("bandcamp.prefs.json").exists());
}

#[test]
fn a_fifo_named_like_the_cache_does_not_block_the_server() {
    let (_tmp, root) = library("fifo-cache");
    mkfifo(&stores(&root).join("bandcamp.json"));

    let started = Instant::now();
    let status = status_within(hub(&root, None), Duration::from_secs(5));
    assert_eq!(status.stores[0].cache.releases, 0);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the status read waited for a FIFO writer"
    );
}

#[test]
fn a_fifo_named_like_the_prefs_does_not_block_the_server() {
    let (_tmp, root) = library("fifo-prefs");
    mkfifo(&stores(&root).join("bandcamp.prefs.json"));

    let started = Instant::now();
    let status = status_within(hub(&root, None), Duration::from_secs(5));
    assert_eq!(status.stores[0].cache.releases, 0);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_fifo_named_like_the_associations_does_not_block_the_server() {
    let (_tmp, root) = library("fifo-associations");
    mkfifo(&stores(&root).join("bandcamp.associations.json"));

    let started = Instant::now();
    let result = match_within(hub(&root, None), Duration::from_secs(5));
    assert!(result.candidates.is_empty(), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// SRV-05: a symlinked state directory
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

fn album_ok() -> TransportResponse {
    json_response(
        serde_json::json!({
            "subsonic-response": {
                "status": "ok",
                "album": {
                    "id": "a:1",
                    "name": "Planted",
                    "artist": "The Artist",
                    "songCount": 1,
                    "song": [{"id": "t:1", "title": "One", "track": 1, "duration": 3}]
                }
            }
        })
        .to_string(),
    )
}

fn cache_with_one_release() -> String {
    serde_json::json!({
        "storeId": "bandcamp",
        "releases": [{
            "id": "a:1",
            "title": "Planted",
            "artist": "The Artist",
            "year": 2026,
            "trackCount": 1,
            "durationMs": 3000,
            "cover": null,
            "webUrl": null,
            "createdUnix": null
        }],
        "pageSize": 200,
        "crawledAt": 1_000,
        "complete": true,
        "lastError": null
    })
    .to_string()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn a_symlinked_stores_directory_is_refused() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path().join("stick");
    fs::create_dir_all(root.join(".plippa")).expect(".plippa");
    let target = tmp.path().join("elsewhere");
    fs::create_dir_all(&target).expect("target");
    // A perfectly good cache in the link target: it must not be read through the link.
    fs::write(target.join("bandcamp.json"), cache_with_one_release()).expect("planted cache");
    symlink(&target, root.join(".plippa/stores")).expect("symlink stores");

    let reader = hub(&root, None);
    assert_eq!(
        reader.status().stores[0].cache.releases,
        0,
        "a cache behind a symlinked state directory was read"
    );
    assert!(reader.cached_releases().is_empty());
    assert_eq!(reader.cached_release("a:1"), None);

    // A write path is refused too: the lookup itself answers, but nothing lands through
    // the link (no cache, no temp file) and the planted file is untouched.
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    transport.push_response(album_ok());
    let writer = StoreHub::with_transport(
        Some(tmp.path().join("secrets")),
        Arc::clone(&transport) as Arc<dyn Transport + Send + Sync>,
    );
    writer.set_library_root(Some(root.clone()));
    writer
        .connect("fan@example.com", "s3cret-copy-pasted", Some(true))
        .expect("connect works without the state directory");
    let view = writer.lookup_release("a:1").expect("the lookup answers");
    assert_eq!(view.release.id.as_str(), "a:1");
    assert_eq!(
        names(&target),
        vec!["bandcamp.json".to_string()],
        "a write went through the symlinked state directory"
    );
}

#[test]
fn a_symlinked_plippa_directory_is_refused() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path().join("stick");
    fs::create_dir_all(&root).expect("root");
    let target = tmp.path().join("elsewhere");
    fs::create_dir_all(target.join("stores")).expect("target");
    fs::write(
        target.join("stores/bandcamp.json"),
        cache_with_one_release(),
    )
    .expect("planted cache");
    symlink(&target, root.join(".plippa")).expect("symlink .plippa");

    let reader = hub(&root, None);
    assert_eq!(reader.status().stores[0].cache.releases, 0);
    assert!(reader.cached_releases().is_empty());
    assert_eq!(
        names(&target.join("stores")),
        vec!["bandcamp.json".to_string()]
    );
}

// ---------------------------------------------------------------------------
// SRV-05: the server boots and serves with a hostile stick
// ---------------------------------------------------------------------------

fn request(port: u16, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .expect("status code");
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    (status, body)
}

#[test]
fn the_first_status_poll_survives_a_hostile_stick() {
    // The startup sync worker is not started here (it needs `spawn_store_sync_worker`'s
    // timer); what this covers is the boot path that reads all three state files and the
    // first `/api/stores` poll a UI makes.
    let (_tmp, root) = library("boots");
    let state = stores(&root);
    mkfifo(&state.join("bandcamp.json"));
    symlink("/dev/zero", state.join("bandcamp.prefs.json")).expect("symlink prefs");
    sparse(&state.join("bandcamp.associations.json"));

    let hub = Arc::new(StoreHub::with_transport(
        Some(root.join("secrets")),
        Arc::new(CountingTransport::new()) as Arc<dyn Transport + Send + Sync>,
    ));
    hub.set_library_root(Some(root.clone()));

    let mut svc = ServerService::start(
        Some(root.clone()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    svc.set_stores_hub(Arc::clone(&hub));
    let svc = Arc::new(std::sync::Mutex::new(svc));

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
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // The first poll answers at once with "not synced yet" rather than a hang or an error.
    let started = Instant::now();
    let (status, body) = request(port, "/api/stores");
    assert_eq!(status, 200, "{body}");
    assert!(started.elapsed() < Duration::from_secs(3), "{started:?}");
    let stores: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(stores["stores"][0]["cache"]["releases"], 0, "{stores}");
    assert_eq!(
        stores["stores"][0]["cache"]["syncedAt"],
        serde_json::Value::Null
    );

    // The portal itself keeps serving.
    let (status, _) = request(port, "/");
    assert_eq!(status, 200);
}
