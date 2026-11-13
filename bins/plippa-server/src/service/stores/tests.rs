//! Behaviour tests for the store connection lifecycle (BCS_03).
//!
//! Everything here drives a real [`StoreHub`] over a scripted
//! [`CountingTransport`](plippa_store::CountingTransport): no sockets, no clock
//! dependence beyond small poll windows, and every assertion is about what a person or
//! the portal would observe (status payload, files on disk, filesystem modes, log text).

use super::tests_support::TempDir;
use super::*;

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use plippa_store::{subsonic_token, CountingTransport, HeaderMap, Transport, TransportResponse};

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

/// A `getArtists` reply that satisfies the credential check.
fn artists_ok() -> TransportResponse {
    json_response(
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","artists":{"index":[]}}}"#
            .to_string(),
    )
}

/// A `getArtists` reply for a wrong password (the store's error 40).
fn artists_bad() -> TransportResponse {
    json_response(
        r#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"Wrong username or password"}}}"#
            .to_string(),
    )
}

/// One `getAlbumList2` page.
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

fn hub_for(
    temp: &TempDir,
    transport: Arc<dyn Transport + Send + Sync>,
    page_size: u32,
    delay: Duration,
) -> Arc<StoreHub> {
    let library = temp.path().join("library");
    std::fs::create_dir_all(&library).expect("library root");
    let hub = Arc::new(
        StoreHub::with_transport(Some(temp.path().join("secrets")), transport)
            .with_sync_tuning(page_size, delay),
    );
    hub.set_library_root(Some(library));
    hub
}

fn cache_file(temp: &TempDir) -> PathBuf {
    cache::cache_path(&temp.path().join("library"), STORE_SLUG)
}

fn secrets_file(hub: &StoreHub) -> PathBuf {
    hub.secrets_dir().expect("secrets dir").join("bandcamp.json")
}

fn mode_of(path: &std::path::Path) -> u32 {
    std::fs::metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o777
}

fn wait_until(label: &str, timeout: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {label}");
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?').map(|(_, rest)| rest)?;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (percent_decode(name) == key).then(|| percent_decode(value))
    })
}

/// A transport whose `get` blocks until the gate opens, so a test can hold a request
/// in flight and observe the second caller being refused.
struct Gate {
    open: Mutex<bool>,
    ready: Condvar,
}

impl Gate {
    fn open() -> Self {
        Self {
            open: Mutex::new(true),
            ready: Condvar::new(),
        }
    }

    fn hold(&self) {
        *self.lock() = false;
    }

    fn release(&self) {
        *self.lock() = true;
        self.ready.notify_all();
    }

    fn wait(&self) {
        let mut open = self.lock();
        while !*open {
            open = self
                .ready
                .wait(open)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, bool> {
        self.open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct GatedTransport {
    inner: CountingTransport,
    gate: Arc<Gate>,
    entered: AtomicUsize,
}

impl GatedTransport {
    fn new(gate: Arc<Gate>) -> Self {
        Self {
            inner: CountingTransport::new(),
            gate,
            entered: AtomicUsize::new(0),
        }
    }

    fn entered(&self) -> usize {
        self.entered.load(AtomicOrdering::SeqCst)
    }
}

impl Transport for GatedTransport {
    fn get(
        &self,
        url: &str,
        headers: &HeaderMap,
        max_bytes: u64,
    ) -> Result<TransportResponse, StoreError> {
        self.entered.fetch_add(1, AtomicOrdering::SeqCst);
        self.gate.wait();
        self.inner.get(url, headers, max_bytes)
    }
}

// ---------------------------------------------------------------------------
// Connect
// ---------------------------------------------------------------------------

#[test]
fn store_connect_rejects_bad_credentials_with_401_and_saves_nothing() {
    let temp = TempDir::new("connect-bad");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_bad());
    let hub = hub_for(&temp, transport, 200, Duration::ZERO);

    let err = hub
        .connect(USER, PASSWORD, Some(true))
        .expect_err("a wrong password must not connect");
    assert_eq!(err.code(), "bad_credentials");
    assert_eq!(err.http_status(), 401);
    assert!(
        !secrets_file(&hub).exists(),
        "nothing may be written when the check fails"
    );
    assert!(!hub.connected());
    let status = hub.status();
    assert!(!status.stores[0].connected);
    assert!(status.secrets_available);
}

#[test]
fn store_connect_uses_an_authenticated_call_not_ping() {
    let temp = TempDir::new("connect-not-ping");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    let hub = hub_for(&temp, transport.clone(), 200, Duration::ZERO);

    hub.connect(USER, PASSWORD, Some(true)).expect("connect");

    let urls = transport.urls();
    assert_eq!(urls.len(), 1, "exactly one request: {urls:?}");
    assert!(
        urls[0].contains("getArtists.view"),
        "the check must be a real authenticated call: {}",
        urls[0]
    );
    assert!(!urls.iter().any(|url| url.contains("ping.view")));
    // And the authenticated call carries the credentials this person typed.
    assert_eq!(param(&urls[0], "u").as_deref(), Some(USER));
    assert!(param(&urls[0], "t").is_some());
}

#[test]
fn store_connect_trims_whitespace_and_accepts_the_leading_space_copy_bug() {
    let temp = TempDir::new("connect-trim");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    let hub = hub_for(&temp, transport.clone(), 200, Duration::ZERO);

    // The settings page selects the value with a leading space; both fields arrive
    // padded. The stored pair must be the trimmed one, or the token never matches.
    hub.connect("  fan@example.com \n", " s3cret-copy-pasted ", Some(true))
        .expect("connect with padded fields");

    let url = transport.urls().remove(0);
    assert_eq!(param(&url, "u").as_deref(), Some(USER));
    let salt = param(&url, "s").expect("salt");
    let token = param(&url, "t").expect("token");
    assert_eq!(
        token,
        subsonic_token(PASSWORD, &salt),
        "the token must use the trimmed password"
    );

    let stored = hub
        .secrets_dir()
        .map(|dir| std::fs::read_to_string(dir.join("bandcamp.json")))
        .expect("secrets dir")
        .expect("stored credentials");
    assert!(stored.contains(USER), "the trimmed user is stored");
    assert!(
        !stored.contains("  fan@example.com"),
        "the stored user is trimmed"
    );
}

#[test]
fn store_credentials_file_is_0600_and_secrets_dir_is_0700() {
    let temp = TempDir::new("connect-modes");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    let hub = hub_for(&temp, transport, 200, Duration::ZERO);

    hub.connect(USER, PASSWORD, Some(true)).expect("connect");

    let dir = hub.secrets_dir().expect("secrets dir");
    assert_eq!(mode_of(dir), 0o700, "secrets directory must be private");
    assert_eq!(mode_of(&secrets_file(&hub)), 0o600, "credentials are 0600");
}

#[test]
fn store_connect_error_text_contains_no_username_or_password() {
    let temp = TempDir::new("connect-secret-text");
    // 1. The store rejects the pair.
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_bad());
    let hub = hub_for(&temp, transport, 200, Duration::ZERO);
    let err = hub
        .connect(USER, PASSWORD, Some(true))
        .expect_err("bad credentials");
    assert!(!err.message().contains(USER), "{}", err.message());
    assert!(!err.message().contains(PASSWORD), "{}", err.message());
    assert!(!format!("{err:?}").contains(PASSWORD));

    // 2. The network fails on a URL that carries the token: the error must be redacted.
    let temp = TempDir::new("connect-secret-text-2");
    let transport = Arc::new(CountingTransport::new());
    transport.push_error(StoreError::transport_url(
        "https://bandcamp.com/api/subsonic/rest/getArtists.view?u=fan&t=SECRETTOKEN&s=SALT",
        "connection reset",
    ));
    let hub = hub_for(&temp, transport, 200, Duration::ZERO);
    let err = hub
        .connect(USER, PASSWORD, Some(true))
        .expect_err("transport error");
    for text in [err.message(), err.to_string(), format!("{err:?}")] {
        assert!(!text.contains("SECRETTOKEN"), "{text}");
        assert!(!text.contains("SALT"), "{text}");
        assert!(!text.contains(PASSWORD), "{text}");
    }

    // 3. Nothing the server said (its log ring) may carry either value.
    let logs = log_tail().join("\n");
    assert!(!logs.contains(PASSWORD), "the password reached the log");
    assert!(!logs.contains(USER), "the user name reached the log");
}

#[test]
fn store_connect_requires_consent_and_refuses_an_empty_pair() {
    let temp = TempDir::new("connect-consent");
    let transport = Arc::new(CountingTransport::new());
    let hub = hub_for(&temp, transport.clone(), 200, Duration::ZERO);

    let err = hub.connect(USER, PASSWORD, None).expect_err("no consent");
    assert_eq!(err.code(), "consent_required");
    assert_eq!(err.http_status(), 403);
    let err = hub.connect(USER, PASSWORD, Some(false)).expect_err("no consent");
    assert_eq!(err.code(), "consent_required");
    assert_eq!(transport.call_count(), 0, "nothing is sent without consent");

    let err = hub
        .connect("   ", "  ", Some(true))
        .expect_err("empty pair");
    assert_eq!(err.code(), "invalid_credentials");
    assert_eq!(transport.call_count(), 0);
    assert!(!secrets_file(&hub).exists());
}

#[test]
fn store_validate_marks_rejected_credentials() {
    let temp = TempDir::new("validate");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    transport.push_response(artists_bad());
    let hub = hub_for(&temp, transport, 200, Duration::ZERO);

    hub.connect(USER, PASSWORD, Some(true)).expect("connect");
    assert!(!hub.status().stores[0].rejected);

    let err = hub.validate().expect_err("the stored pair stopped working");
    assert_eq!(err.code(), "bad_credentials");
    let status = hub.status();
    assert!(status.stores[0].rejected, "the portal must show the pair is dead");
    assert!(status.stores[0].connected, "it is still stored, not deleted");
    assert!(secrets_file(&hub).exists());
}

// ---------------------------------------------------------------------------
// Status payloads
// ---------------------------------------------------------------------------

#[test]
fn store_status_never_serialises_credentials() {
    let temp = TempDir::new("status-secret");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    let hub = hub_for(&temp, transport, 200, Duration::ZERO);
    hub.connect(USER, PASSWORD, Some(true)).expect("connect");

    let payload = serde_json::to_string(&hub.status()).expect("serialise status");
    assert!(!payload.contains(PASSWORD), "{payload}");
    assert!(!payload.contains(USER), "{payload}");
    // The status does carry the facts the portal needs.
    assert!(payload.contains("\"connected\":true"), "{payload}");
    assert!(payload.contains("\"secretsAvailable\":true"), "{payload}");

    // The cache (once filled) is metadata only.
    let summary = serde_json::to_string(&hub.summary()).expect("serialise summary");
    assert!(!summary.contains(PASSWORD));
    assert!(!summary.contains(USER));

    let debug = format!("{hub:?}");
    assert!(!debug.contains(PASSWORD), "{debug}");
    assert!(!debug.contains(USER), "{debug}");
}

#[test]
fn stores_degrade_when_secrets_dir_is_unwritable() {
    let temp = TempDir::new("secrets-unwritable");
    // A regular file where the directory should be: the directory cannot be created.
    let blocked = temp.path().join("blocked");
    std::fs::write(&blocked, b"not a directory").expect("blocker file");
    let hub = Arc::new(StoreHub::with_transport(
        Some(blocked),
        Arc::new(CountingTransport::new()),
    ));
    hub.set_library_root(Some(temp.path().join("library")));

    let status = hub.status();
    assert!(!status.secrets_available);
    assert!(
        status.secrets_reason.is_some(),
        "the reason must be reportable"
    );
    assert!(!status.stores[0].connected);
    let summary = status.summary();
    assert!(!summary.available);
    assert!(summary.reason.is_some());

    let err = hub
        .connect(USER, PASSWORD, Some(true))
        .expect_err("unavailable");
    assert_eq!(err.code(), "unavailable");
    assert_eq!(err.http_status(), 503);
    assert!(!err.message().contains(PASSWORD));

    // No secrets directory configured at all is the same shape of answer.
    let none = StoreHub::with_transport(None, Arc::new(CountingTransport::new()));
    let status = none.status();
    assert!(!status.secrets_available);
    assert_eq!(status.secrets_reason.as_deref(), Some(NO_SECRETS_DIR));
    assert_eq!(
        none.connect(USER, PASSWORD, Some(true))
            .expect_err("unavailable")
            .code(),
        "unavailable"
    );
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

#[test]
fn store_sync_pages_until_short_page_and_persists_the_cache() {
    let temp = TempDir::new("sync-pages");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    transport.push_response(album_page(0, 2));
    transport.push_response(album_page(2, 1));
    let hub = hub_for(&temp, transport.clone(), 2, Duration::from_millis(1));
    hub.connect(USER, PASSWORD, Some(true)).expect("connect");

    hub.start_sync().expect("sync starts");
    wait_until("the sync to finish", Duration::from_secs(5), || {
        hub.sync_status().state != SyncState::Running
    });

    let status = hub.sync_status();
    assert_eq!(status.state, SyncState::Done, "{status:?}");
    assert_eq!(status.pages, 2, "a short page ends the crawl");
    assert_eq!(status.releases, 3);
    assert!(!status.cancelled);
    assert!(status.error.is_none());

    let releases = hub.cached_releases();
    assert_eq!(releases.len(), 3);
    assert_eq!(releases[2].id.as_str(), "a:2");
    assert_eq!(releases[2].title, "Release 2");

    let path = cache_file(&temp);
    assert!(path.exists(), "the cache is written under the library");
    assert_eq!(mode_of(&path), 0o600, "the cache file is private");
    let bytes = std::fs::read(&path).expect("read cache");
    let stored = cache::StoreCache::decode(&bytes).expect("decode cache");
    assert_eq!(stored.store_id, STORE_SLUG);
    assert!(stored.complete);
    assert_eq!(stored.releases.len(), 3);
    assert!(stored.crawled_at > 0);
    assert!(
        !String::from_utf8_lossy(&bytes).contains(PASSWORD),
        "the cache holds metadata only"
    );

    let cache_status = hub.status().stores[0].cache.clone();
    assert_eq!(cache_status.releases, 3);
    assert!(cache_status.complete);
    assert!(!cache_status.stale);
    assert!(cache_status.synced_at.is_some());

    // Two page requests, one credential check: no parallel or repeated calls.
    assert_eq!(transport.call_count(), 3);
    let urls = transport.urls();
    assert!(urls[1].contains("offset=0"));
    assert!(urls[2].contains("offset=2"));
}

#[test]
fn store_sync_progress_is_reported_and_cancellable() {
    let temp = TempDir::new("sync-cancel");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    for start in (0..12).step_by(2) {
        transport.push_response(album_page(start, 2));
    }
    let hub = hub_for(&temp, transport, 2, Duration::from_millis(80));
    hub.connect(USER, PASSWORD, Some(true)).expect("connect");

    hub.start_sync().expect("sync starts");
    wait_until("the first page", Duration::from_secs(5), || {
        hub.sync_status().pages >= 1
    });
    let running = hub.sync_status();
    assert_eq!(running.state, SyncState::Running);
    assert!(running.pages >= 1);
    assert!(running.releases >= 2);

    hub.cancel_sync();
    wait_until("the cancel to be honoured", Duration::from_secs(5), || {
        hub.sync_status().state != SyncState::Running
    });

    let stopped = hub.sync_status();
    assert!(stopped.cancelled, "{stopped:?}");
    assert_eq!(stopped.state, SyncState::Idle);
    assert!(stopped.pages < 6, "the crawl stopped early: {stopped:?}");
    assert!(
        !cache_file(&temp).exists(),
        "a cancelled first sync leaves no partial cache"
    );
    assert!(hub.cached_releases().is_empty());
}

#[test]
fn store_sync_keeps_the_previous_cache_when_a_crawl_fails() {
    let temp = TempDir::new("sync-failure");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    transport.push_response(album_page(0, 1));
    let hub = hub_for(&temp, transport.clone(), 2, Duration::from_millis(1));
    hub.connect(USER, PASSWORD, Some(true)).expect("connect");
    hub.start_sync().expect("first sync");
    wait_until("the first sync", Duration::from_secs(5), || {
        hub.sync_status().state != SyncState::Running
    });
    assert_eq!(hub.cached_releases().len(), 1);

    // The next crawl dies on its first page.
    transport.push_error(StoreError::transport_url(
        "https://bandcamp.com/x?t=SECRET",
        "timed out",
    ));
    hub.start_sync().expect("second sync");
    wait_until("the failed sync", Duration::from_secs(5), || {
        hub.sync_status().state != SyncState::Running
    });

    let status = hub.sync_status();
    assert_eq!(status.state, SyncState::Failed);
    assert_eq!(status.error_code.as_deref(), Some("transport"));
    assert!(
        !status.error.clone().unwrap_or_default().contains("SECRET"),
        "the error is redacted"
    );
    assert_eq!(
        hub.cached_releases().len(),
        1,
        "the last good cache is kept"
    );
    assert!(cache_file(&temp).exists());
}

#[test]
fn store_sync_refuses_an_oversized_cache_instead_of_truncating_it() {
    let temp = TempDir::new("sync-oversized");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    // 24 full pages of 500 releases with long titles: comfortably above the 2 MB cap.
    let long_title = "L".repeat(220);
    for page in 0..23 {
        let albums: Vec<serde_json::Value> = (0..500)
            .map(|index| {
                serde_json::json!({
                    "id": format!("a:{}", page * 500 + index),
                    "name": long_title,
                    "artist": "The Artist",
                    "year": 2020,
                    "songCount": 5,
                    "duration": 600,
                    "coverArt": "c:1",
                })
            })
            .collect();
        transport.push_response(json_response(
            serde_json::json!({
                "subsonic-response": { "status": "ok", "albumList2": { "album": albums } }
            })
            .to_string(),
        ));
    }
    // A short last page ends the crawl cleanly, so the cache is encoded (and refused)
    // instead of the crawl failing on an exhausted transport queue.
    transport.push_response(album_page(11_500, 1));
    let hub = hub_for(&temp, transport, 500, Duration::ZERO);
    hub.connect(USER, PASSWORD, Some(true)).expect("connect");

    hub.start_sync().expect("sync starts");
    wait_until("the sync to give up", Duration::from_secs(20), || {
        hub.sync_status().state != SyncState::Running
    });

    let status = hub.sync_status();
    assert_eq!(status.state, SyncState::Failed, "{status:?}");
    assert_eq!(status.error_code.as_deref(), Some("cache_too_large"));
    assert!(
        !cache_file(&temp).exists(),
        "an oversized cache must not be written half-way"
    );
}

#[test]
fn store_sync_is_rate_limited_to_one_at_a_time() {
    let temp = TempDir::new("sync-one-at-a-time");
    let gate = Arc::new(Gate::open());
    let transport = Arc::new(GatedTransport::new(Arc::clone(&gate)));
    let hub = hub_for(&temp, transport.clone(), 2, Duration::ZERO);
    transport.inner.push_response(artists_ok());
    hub.connect(USER, PASSWORD, Some(true)).expect("connect");

    gate.hold();
    transport.inner.push_response(album_page(0, 1));
    hub.start_sync().expect("the first sync starts");
    wait_until("the first request to be in flight", Duration::from_secs(5), || {
        transport.entered() >= 2
    });

    let err = hub.start_sync().expect_err("the second sync is refused");
    assert_eq!(err.code(), "busy");
    assert_eq!(err.http_status(), 429);

    gate.release();
    wait_until("the first sync to finish", Duration::from_secs(5), || {
        hub.sync_status().state != SyncState::Running
    });
    assert_eq!(hub.sync_status().state, SyncState::Done);
    assert_eq!(hub.cached_releases().len(), 1);
}

#[test]
fn store_connect_and_validate_are_rate_limited_to_one_at_a_time() {
    let temp = TempDir::new("connect-one-at-a-time");
    let gate = Arc::new(Gate::open());
    let transport = Arc::new(GatedTransport::new(Arc::clone(&gate)));
    let hub = hub_for(&temp, transport.clone(), 200, Duration::ZERO);
    transport.inner.push_response(artists_ok());

    gate.hold();
    let background = {
        let hub = Arc::clone(&hub);
        std::thread::spawn(move || hub.connect(USER, PASSWORD, Some(true)))
    };
    wait_until("the first connect to be in flight", Duration::from_secs(5), || {
        transport.entered() >= 1
    });

    let err = hub
        .connect("other@example.com", "other-password", Some(true))
        .expect_err("the second connect is refused");
    assert_eq!(err.code(), "busy");
    assert_eq!(err.http_status(), 429);

    gate.release();
    background
        .join()
        .expect("connect thread")
        .expect("the first connect finishes");
    assert!(hub.connected());
}

#[test]
fn store_auto_sync_due_follows_consent_age_and_the_six_hour_floor() {
    let temp = TempDir::new("auto-sync");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    let hub = hub_for(&temp, transport, 2, Duration::from_millis(1));

    let now = now_secs();
    assert!(!hub.auto_sync_due(now), "no connection, nothing to do");

    hub.connect(USER, PASSWORD, Some(true)).expect("connect");
    assert!(hub.auto_sync_due(now), "no cache at all is due");

    // An old, complete cache is due...
    let root = temp.path().join("library");
    let old = cache::StoreCache {
        store_id: STORE_SLUG.to_string(),
        releases: Vec::new(),
        page_size: 2,
        crawled_at: now.saturating_sub(cache::CACHE_MAX_AGE_SECS + 60),
        complete: true,
        last_error: None,
    };
    cache::write_private_atomic(
        &cache::cache_path(&root, STORE_SLUG),
        &old.encode().expect("encode"),
    )
    .expect("write cache");
    assert!(hub.auto_sync_due(now));

    // ...but not while a sync ran in the last six hours.
    let mut prefs = cache::read_prefs(&root);
    prefs.auto_sync_consent = true;
    prefs.last_sync_at = Some(now.saturating_sub(60));
    cache::write_prefs(&root, &prefs).expect("write prefs");
    assert!(!hub.auto_sync_due(now), "the six-hour floor holds");
    assert!(hub.auto_sync_due(now + AUTO_SYNC_MIN_INTERVAL_SECS + 1));

    // Consent withdrawn: never automatic again.
    prefs.auto_sync_consent = false;
    prefs.last_sync_at = None;
    cache::write_prefs(&root, &prefs).expect("write prefs");
    assert!(!hub.auto_sync_due(now + AUTO_SYNC_MIN_INTERVAL_SECS * 10));
}

// ---------------------------------------------------------------------------
// Cache, lookup and disconnect
// ---------------------------------------------------------------------------

#[test]
fn store_cache_corrupt_file_is_renamed_not_deleted() {
    let temp = TempDir::new("cache-corrupt");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    let hub = hub_for(&temp, transport, 200, Duration::ZERO);
    hub.connect(USER, PASSWORD, Some(true)).expect("connect");

    let path = cache_file(&temp);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    std::fs::write(&path, b"{ this is not the cache }").expect("write junk");

    let status = hub.status().stores[0].cache.clone();
    assert_eq!(status.releases, 0);
    assert!(
        status
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("corrupt"),
        "{status:?}"
    );
    assert!(!path.exists(), "the corrupt file is moved out of the way");

    let aside: Vec<PathBuf> = std::fs::read_dir(path.parent().expect("parent"))
        .expect("read dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|entry| {
            entry
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("bandcamp.json.corrupt-"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(aside.len(), 1, "exactly one moved-aside copy: {aside:?}");
    let moved = std::fs::read(&aside[0]).expect("read moved-aside copy");
    assert_eq!(moved, b"{ this is not the cache }", "nothing is lost");
    assert_eq!(mode_of(&aside[0]), 0o600);
}

#[test]
fn store_disconnect_removes_credentials_and_cache() {
    let temp = TempDir::new("disconnect");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    transport.push_response(album_page(0, 2));
    transport.push_response(album_page(2, 1));
    let hub = hub_for(&temp, transport, 2, Duration::from_millis(1));
    hub.connect(USER, PASSWORD, Some(true)).expect("connect");
    hub.start_sync().expect("sync");
    wait_until("the sync", Duration::from_secs(5), || {
        hub.sync_status().state != SyncState::Running
    });
    let root = temp.path().join("library");
    assert!(secrets_file(&hub).exists());
    assert!(cache_file(&temp).exists());
    assert!(cache::prefs_path(&root, STORE_SLUG).exists());

    hub.disconnect().expect("disconnect");
    assert!(!secrets_file(&hub).exists(), "credentials are removed");
    assert!(!cache_file(&temp).exists(), "the cache is removed");
    assert!(
        !cache::prefs_path(&root, STORE_SLUG).exists(),
        "consent and check times are forgotten"
    );
    assert!(!hub.connected());
    assert!(hub.cached_releases().is_empty());
    let status = hub.status();
    assert!(!status.stores[0].connected);
    assert!(!status.stores[0].rejected);
    assert_eq!(status.stores[0].cache.releases, 0);
    assert_eq!(hub.sync_status(), SyncStatus::default());

    // Idempotent: a second disconnect is not an error.
    hub.disconnect().expect("disconnect twice");
}

#[test]
fn store_disconnect_stops_a_running_sync_and_leaves_no_cache() {
    let temp = TempDir::new("disconnect-mid-sync");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    for start in (0..20).step_by(2) {
        transport.push_response(album_page(start, 2));
    }
    let hub = hub_for(&temp, transport, 2, Duration::from_millis(100));
    hub.connect(USER, PASSWORD, Some(true)).expect("connect");

    hub.start_sync().expect("sync starts");
    wait_until("the first page", Duration::from_secs(5), || {
        hub.sync_status().pages >= 1
    });

    hub.disconnect().expect("disconnect during a sync");

    // The sync stopped before the cache removal, so nothing is written afterwards.
    assert_ne!(hub.sync_status().state, SyncState::Running);
    assert!(!cache_file(&temp).exists());
    assert!(!secrets_file(&hub).exists());
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !cache_file(&temp).exists(),
        "a late cache write must not resurrect the file"
    );
    assert!(!hub.connected());
}

#[test]
fn store_get_release_refreshes_the_cache_on_a_miss() {
    let temp = TempDir::new("release-miss");
    let transport = Arc::new(CountingTransport::new());
    transport.push_response(artists_ok());
    transport.push_response(json_response(
        serde_json::json!({
            "subsonic-response": {
                "status": "ok",
                "album": {
                    "id": "a:99",
                    "name": "Fetched On Demand",
                    "artist": "The Artist",
                    "songCount": 1,
                    "song": [{ "id": "t:1", "title": "One", "track": 1, "duration": 3 }]
                }
            }
        })
        .to_string(),
    ));
    let hub = hub_for(&temp, transport.clone(), 200, Duration::ZERO);
    hub.connect(USER, PASSWORD, Some(true)).expect("connect");

    let detail = hub.get_release(" a:99 ").expect("release lookup");
    assert_eq!(detail.release.title, "Fetched On Demand");
    assert_eq!(detail.tracks.len(), 1);

    // The answer is now cached, so a browse reads it without asking again.
    assert!(hub.cached_release("a:99").is_some());
    assert!(cache_file(&temp).exists());
    assert_eq!(
        transport.call_count(),
        2,
        "one credential check and one lookup, no repeat"
    );
    let cached = hub.cached_release("a:99").expect("cached release");
    assert_eq!(cached.id.as_str(), "a:99");
    assert_eq!(cached.title, "Fetched On Demand");
    assert_eq!(hub.cached_releases().len(), 1);
    let stored = hub.status().stores[0].cache.clone();
    assert_eq!(stored.releases, 1);
    assert!(!stored.complete, "a lookup-built cache is not a full sync");
    assert!(
        stored.synced_at.is_none(),
        "a lookup-built cache has no crawl time"
    );

    let err = hub.get_release("   ").expect_err("empty id");
    assert_eq!(err.code(), "not_found");
}

#[test]
fn store_routes_need_a_connection() {
    let temp = TempDir::new("not-connected");
    let hub = hub_for(&temp, Arc::new(CountingTransport::new()), 200, Duration::ZERO);

    assert_eq!(hub.validate().expect_err("validate").code(), "not_connected");
    assert_eq!(hub.start_sync().expect_err("sync").code(), "not_connected");
    assert_eq!(hub.get_release("a:1").expect_err("lookup").code(), "not_connected");
    assert_eq!(hub.sync_status(), SyncStatus::default());
}

// ---------------------------------------------------------------------------
// SECFIX_04 (SRV-06): the ad-hoc limiter's own arithmetic
// ---------------------------------------------------------------------------

#[test]
fn the_adhoc_bucket_allows_its_burst_then_refuses() {
    // No refill inside the test, so only the burst counts.
    let mut bucket = TokenBucket::new(3, 0.0, 100);
    for index in 0..3 {
        assert!(bucket.try_take(), "token {index} inside the burst must pass");
    }
    assert!(!bucket.try_take(), "the bucket must refuse once drained");
}

#[test]
fn the_hourly_cap_bounds_the_bucket_even_when_it_refills() {
    // A fast refill cannot beat the hourly ceiling.
    let mut bucket = TokenBucket::new(100, 1000.0, 2);
    assert!(bucket.try_take());
    assert!(bucket.try_take());
    assert!(!bucket.try_take(), "the hourly cap must win over the tokens");
    assert_eq!(bucket.window_count, 2);
}
