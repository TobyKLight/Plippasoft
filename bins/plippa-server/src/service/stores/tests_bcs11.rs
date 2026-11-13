//! BCS_11 server side: the cached collection survives a restart, and a missing cache is its
//! own tap answer rather than "you do not own this".

use super::cache;
use super::tap::{TapBlocked, TapRequest, TapState};
use super::tests_support::TempDir;
use super::*;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plippa_store::{CountingTransport, StoreRelease, Transport as ProviderTransport, TransportResponse};

const SABLE_URL: &str = "https://sable-music.bandcamp.com/track/were-getting-older-baby";
const SABLE_ID: &str = "t:2970735116";

fn sable() -> StoreRelease {
    StoreRelease {
        id: plippa_store::StoreReleaseId::new(SABLE_ID),
        title: "We're Getting Older Baby".to_string(),
        artist: "Sable".to_string(),
        year: Some(2026),
        track_count: Some(1),
        duration_ms: None,
        cover: None,
        web_url: None,
        created_unix: None,
    }
}

fn write_cache(root: &Path, crawled_at: u64, complete: bool) {
    let cache = StoreCache {
        store_id: STORE_SLUG.to_string(),
        releases: vec![sable()],
        page_size: 200,
        crawled_at,
        complete,
        last_error: None,
    };
    fs::create_dir_all(cache::stores_dir(root)).expect("stores dir");
    let bytes = serde_json::to_vec(&cache).expect("encode cache");
    cache::write_private_atomic(&cache::cache_path(root, STORE_SLUG), &bytes).expect("write cache");
}

fn artists_ok() -> TransportResponse {
    TransportResponse {
        status: 200,
        headers: vec![(
            "Content-Type".to_string(),
            "application/json; charset=utf-8".to_string(),
        )],
        body: br#"{"subsonic-response":{"status":"ok","version":"1.16.1","artists":{"index":[]}}}"#
            .to_vec(),
        final_url: "https://bandcamp.com/api/subsonic/rest/response.view".to_string(),
        redirects: Vec::new(),
    }
}

/// A new hub over the same secrets directory: what a restarted server builds. The credentials
/// are stored on disk, so the new hub is connected without any request.
fn fresh_hub(temp: &TempDir) -> (Arc<StoreHub>, Arc<CountingTransport>) {
    let transport = Arc::new(CountingTransport::new());
    let hub = Arc::new(StoreHub::with_transport(
        Some(temp.path().join("secrets")),
        Arc::clone(&transport) as Arc<dyn ProviderTransport + Send + Sync>,
    ));
    (hub, transport)
}

fn connect(temp: &TempDir) {
    let (hub, transport) = fresh_hub(temp);
    transport.push_response(artists_ok());
    hub.connect("fan@example.test", "not-a-real-password", Some(true))
        .expect("connect");
}

fn root(temp: &TempDir) -> PathBuf {
    let root = temp.path().join("library");
    fs::create_dir_all(&root).expect("library root");
    root
}

fn tap() -> TapRequest {
    TapRequest {
        url: SABLE_URL.to_string(),
        expected_duration: None,
        expected_title: None,
        consent: Some(true),
    }
}

#[test]
fn a_restarted_hub_finds_the_cached_collection_without_a_sync() {
    let temp = TempDir::new("bcs11-restart");
    let root = root(&temp);
    connect(&temp);
    {
        let (hub, _) = fresh_hub(&temp);
        hub.set_library_root(Some(root.clone()));
        hub.store_cache(StoreCache {
            store_id: STORE_SLUG.to_string(),
            releases: vec![sable()],
            page_size: 200,
            crawled_at: now_secs(),
            complete: true,
            last_error: None,
        })
        .expect("cache written");
    } // the first process ends

    let (hub, transport) = fresh_hub(&temp);
    hub.set_library_root(Some(root));
    let answer = hub.tap(&tap()).expect("tap answers");

    assert_eq!(answer.state, TapState::OwnedDownloadable, "{}", answer.message);
    assert_eq!(answer.blocked, None);
    assert_eq!(transport.call_count(), 0, "no sync and no request in the new process");
    assert_eq!(hub.sync_status().state, SyncState::Idle, "no sync was started");
}

#[test]
fn a_library_root_that_appears_after_the_first_tap_is_picked_up() {
    let temp = TempDir::new("bcs11-late-root");
    let root = root(&temp);
    connect(&temp);
    write_cache(&root, now_secs(), true);

    // Boot order: the service exists, the stick is not mounted yet.
    let (hub, _) = fresh_hub(&temp);
    let before = hub.tap(&tap()).expect("tap answers");
    assert_eq!(before.blocked, Some(TapBlocked::NoStoreCache), "no root, no cache");

    hub.set_library_root(Some(root));
    let after = hub.tap(&tap()).expect("tap answers");
    assert_eq!(after.state, TapState::OwnedDownloadable, "{}", after.message);
}

#[test]
fn a_tap_with_no_cache_file_is_its_own_blocked_state_and_records_no_want() {
    let temp = TempDir::new("bcs11-no-cache");
    let root = root(&temp);
    connect(&temp);
    let (hub, _) = fresh_hub(&temp);
    hub.set_library_root(Some(root.clone()));

    let answer = hub.tap(&tap()).expect("tap answers");

    assert_eq!(answer.state, TapState::NotFound);
    assert_eq!(answer.blocked, Some(TapBlocked::NoStoreCache));
    assert!(answer.purchase_url.is_none(), "an unsynced library cannot say 'not owned'");
    assert!(answer.message.contains("not synced"), "{}", answer.message);
    assert!(super::matching::read_wants(&root).is_empty());
    let wire = serde_json::to_value(&answer).expect("serialises");
    assert_eq!(wire["blocked"], "no_store_cache");

    // Once a cache file exists the normal answers return.
    write_cache(&root, now_secs(), true);
    let answer = hub.tap(&tap()).expect("tap answers");
    assert_eq!(answer.state, TapState::OwnedDownloadable, "{}", answer.message);
}

#[test]
fn a_stale_cache_is_still_matched_and_is_not_reported_as_missing() {
    let temp = TempDir::new("bcs11-stale");
    let root = root(&temp);
    connect(&temp);
    write_cache(&root, now_secs().saturating_sub(10 * 24 * 3600), true);
    let (hub, _) = fresh_hub(&temp);
    hub.set_library_root(Some(root));

    let answer = hub.tap(&tap()).expect("tap answers");

    assert_eq!(answer.state, TapState::OwnedDownloadable, "{}", answer.message);
    assert_eq!(answer.blocked, None);
    assert!(answer.match_result.expect("match").cache_stale, "stale is reported as stale");
}
