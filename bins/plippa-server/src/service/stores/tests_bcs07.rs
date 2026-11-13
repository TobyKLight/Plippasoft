//! Behaviour tests for the composite tap answer (BCS_07).
//!
//! Each test is a question the device asks after a tap, answered by the server exactly as the
//! screen will draw it: owned → download, not owned → buy, ambiguous → pick one, blocked →
//! today's unmatched screen. The store API is a [`plippa_store::CountingTransport`], so a tap
//! that reached the network would be a failing assertion rather than a hidden fetch.

use super::matching::{associations_path, read_wants, Associations};
use super::tap::{TapBlocked, TapRequest, TapState, MAX_TAP_CANDIDATES};
use super::tests_support::TempDir;
use super::*;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plippa_store::{CountingTransport, StoreRelease, Transport as ProviderTransport, TransportResponse};
use crate::web::fetch::{HopResponse, Transport as WebTransport, TransportError};
use std::time::Duration;

const SABLE_URL: &str = "https://sable-music.bandcamp.com/track/were-getting-older-baby";
const SABLE_ID: &str = "t:2970735116";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn release(id: &str, title: &str, artist: &str, tracks: Option<u32>) -> StoreRelease {
    StoreRelease {
        id: plippa_store::StoreReleaseId::new(id),
        title: title.to_string(),
        artist: artist.to_string(),
        year: Some(2026),
        track_count: tracks,
        duration_ms: None,
        cover: None,
        web_url: None,
        created_unix: None,
    }
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

fn library_dir(temp: &TempDir) -> PathBuf {
    let root = temp.path().join("library");
    fs::create_dir_all(&root).expect("library root");
    root
}

fn write_cache(root: &Path, releases: Vec<StoreRelease>) {
    let cache = StoreCache {
        store_id: STORE_SLUG.to_string(),
        releases,
        page_size: 200,
        crawled_at: now_secs(),
        complete: true,
        last_error: None,
    };
    let dir = cache::stores_dir(root);
    fs::create_dir_all(&dir).expect("stores dir");
    let bytes = serde_json::to_vec(&cache).expect("encode cache");
    cache::write_private_atomic(&cache::cache_path(root, STORE_SLUG), &bytes).expect("write cache");
}

/// A hub whose cache holds `releases`, with a usable (but unconnected) secrets directory.
fn hub(temp: &TempDir, releases: Vec<StoreRelease>) -> (Arc<StoreHub>, PathBuf, Arc<CountingTransport>) {
    let root = library_dir(temp);
    let transport = Arc::new(CountingTransport::new());
    let hub = Arc::new(StoreHub::with_transport(
        Some(temp.path().join("secrets")),
        Arc::clone(&transport) as Arc<dyn ProviderTransport + Send + Sync>,
    ));
    hub.set_library_root(Some(root.clone()));
    write_cache(&root, releases);
    (hub, root, transport)
}

/// A connected hub: `connect` is the one call that may reach the store, and its call is
/// counted so a test can assert the tap itself added none.
fn connected_hub(
    temp: &TempDir,
    releases: Vec<StoreRelease>,
) -> (Arc<StoreHub>, PathBuf, Arc<CountingTransport>, usize) {
    let (hub, root, transport) = hub(temp, releases);
    transport.push_response(artists_ok());
    hub.connect("fan@example.test", "not-a-real-password", Some(true))
        .expect("connect");
    let after_connect = transport.call_count();
    (hub, root, transport, after_connect)
}

fn tap(url: &str) -> TapRequest {
    TapRequest {
        url: url.to_string(),
        expected_duration: None,
        expected_title: None,
        consent: Some(true),
    }
}

// ---------------------------------------------------------------------------
// Owned
// ---------------------------------------------------------------------------

#[test]
fn tap_flow_owned_returns_downloadable_state_and_never_downloads() {
    let temp = TempDir::new("bcs07-owned");
    let (hub, root, transport, after_connect) = connected_hub(
        &temp,
        vec![release(SABLE_ID, "We're Getting Older Baby", "Sable", Some(1))],
    );

    let answer = hub.tap(&tap(SABLE_URL)).expect("tap answers");

    assert_eq!(answer.state, TapState::OwnedDownloadable, "{}", answer.message);
    assert_eq!(answer.blocked, None);
    assert_eq!(answer.purchase_url, None, "an owned release is not offered for sale");
    assert_eq!(answer.candidates.len(), 1);
    let top = &answer.candidates[0];
    assert_eq!(top.store_release_id, SABLE_ID);
    assert_eq!(top.track_count, Some(1));
    assert!(
        answer.message.contains("You own this"),
        "the screen gets a sentence, not a code: {}",
        answer.message
    );

    // "No download" is the contract: the tap is a lookup, nothing more.
    assert_eq!(
        transport.call_count(),
        after_connect,
        "the tap must make zero store requests"
    );
    assert!(
        hub.download_status("anything").is_err(),
        "no download session may exist after a tap"
    );
    let stage = root.join(cache::STATE_DIR).join("tmp");
    let staged = fs::read_dir(&stage)
        .map(|entries| entries.filter_map(|e| e.ok()).count())
        .unwrap_or(0);
    assert_eq!(staged, 0, "nothing may be staged by a tap");
}

// ---------------------------------------------------------------------------
// Not owned
// ---------------------------------------------------------------------------

#[test]
fn tap_flow_not_owned_returns_purchase_url_and_records_interest() {
    let temp = TempDir::new("bcs07-not-owned");
    // A collection that does not hold this release.
    let (hub, root, transport, after_connect) = connected_hub(
        &temp,
        vec![release("a:999", "Something Else Entirely", "Other Artist", Some(8))],
    );

    let answer = hub.tap(&tap(SABLE_URL)).expect("tap answers");

    assert_eq!(answer.state, TapState::NotOwned, "{}", answer.message);
    assert_eq!(
        answer.purchase_url.as_deref(),
        Some("https://sable-music.bandcamp.com/track/were-getting-older-baby"),
        "the purchase link is the canonical page, query-free"
    );
    assert!(answer.candidates.is_empty());
    assert_eq!(answer.blocked, None);
    assert_eq!(transport.call_count(), after_connect);

    // The tap is remembered, so the next tap after the purchase re-offers the download.
    let wants = read_wants(&root);
    assert_eq!(wants.len(), 1, "one tap, one recorded interest");
    assert_eq!(wants[0].url_key, "sable-music/track/were-getting-older-baby");
    assert_eq!(wants[0].artist.as_deref(), Some("sable music"));

    // Tapping twice refreshes the one entry instead of growing the file.
    let again = hub.tap(&tap(SABLE_URL)).expect("tap answers");
    assert_eq!(again.state, TapState::NotOwned);
    assert_eq!(read_wants(&root).len(), 1);
}

// ---------------------------------------------------------------------------
// Ambiguous
// ---------------------------------------------------------------------------

#[test]
fn tap_flow_ambiguous_returns_at_most_three_candidates() {
    let temp = TempDir::new("bcs07-ambiguous");
    // Five releases whose titles contain the tapped title: each scores in the confirmation
    // band, so none may be used without a person choosing.
    let releases = (0..5)
        .map(|n| {
            release(
                &format!("a:{n}"),
                &format!("We're Getting Older Baby (Edition {n})"),
                "Sable",
                Some(1),
            )
        })
        .collect();
    let (hub, root, transport, after_connect) = connected_hub(&temp, releases);

    let answer = hub.tap(&tap(SABLE_URL)).expect("tap answers");

    assert_eq!(answer.state, TapState::Ambiguous, "{}", answer.message);
    assert_eq!(
        answer.candidates.len(),
        MAX_TAP_CANDIDATES,
        "a person picks from at most three"
    );
    assert!(
        answer.candidates.iter().all(|c| c.confidence >= matching::CONFIRM_CONFIDENCE),
        "only candidates worth showing are returned"
    );
    assert!(
        answer.candidates.iter().all(|c| c.confidence < matching::AUTO_USABLE_CONFIDENCE),
        "a candidate in the auto band would be owned, not ambiguous"
    );
    assert_eq!(answer.purchase_url, None, "nothing is sold until it is identified");
    assert_eq!(transport.call_count(), after_connect);
    let _ = root;
}

// ---------------------------------------------------------------------------
// Blocked
// ---------------------------------------------------------------------------

#[test]
fn tap_flow_no_store_connected_degrades_to_unmatched() {
    let temp = TempDir::new("bcs07-no-store");
    let (hub, _root, transport) = hub(
        &temp,
        vec![release(SABLE_ID, "We're Getting Older Baby", "Sable", Some(1))],
    );

    let answer = hub.tap(&tap(SABLE_URL)).expect("a blocked tap is still an answer");

    assert_eq!(answer.state, TapState::NotFound, "the device shows its unmatched screen");
    assert_eq!(answer.blocked, Some(TapBlocked::NoStore));
    assert!(answer.candidates.is_empty());
    assert!(answer.purchase_url.is_none());
    assert!(
        !answer.message.is_empty() && !answer.message.contains('_'),
        "a readable sentence, never a code: {}",
        answer.message
    );
    assert_eq!(transport.call_count(), 0, "no store, no request");
}

#[test]
fn tap_flow_rejected_credentials_say_so_plainly() {
    let temp = TempDir::new("bcs07-rejected");
    let (hub, _root, transport, _) = connected_hub(&temp, Vec::new());
    // What BCS_03's validate does when the store refuses the stored pair.
    transport.push_error(plippa_store::StoreError::BadCredentials);
    hub.validate().expect_err("the store refused the pair");
    assert!(hub.status().stores[0].rejected, "the rejection is recorded");

    let answer = hub.tap(&tap(SABLE_URL)).expect("tap answers");
    assert_eq!(answer.state, TapState::NotFound);
    assert_eq!(answer.blocked, Some(TapBlocked::CredentialsRejected));
    assert!(
        answer.message.contains("portal"),
        "the device tells the person where to fix it: {}",
        answer.message
    );
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

#[test]
fn tap_flow_does_not_touch_the_network() {
    let temp = TempDir::new("bcs07-network");
    let (hub, _root, transport, after_connect) = connected_hub(
        &temp,
        vec![release(SABLE_ID, "We're Getting Older Baby", "Sable", Some(1))],
    );

    // Every shape of tap, including the one that ends in a purchase offer.
    let _ = hub.tap(&tap(SABLE_URL)).expect("owned");
    let _ = hub.tap(&tap("https://sable-music.bandcamp.com/album/unknown-thing"))
        .expect("not owned");
    let _ = hub.tap(&tap("https://example.test/not-bandcamp")).expect("not a store link");
    let request = TapRequest {
        expected_duration: Some(216),
        expected_title: Some("We're Getting Older Baby".into()),
        ..tap(SABLE_URL)
    };
    let _ = hub.tap(&request).expect("owned with hints");

    assert_eq!(
        transport.call_count(),
        after_connect,
        "the composite tap endpoint must stay cache-only: {:?}",
        transport.urls()
    );
}

// ---------------------------------------------------------------------------
// A Noubin URL: the store link lives on the release page
// ---------------------------------------------------------------------------

const NOUBIN_KEY: &str = "https://noubin.example/n/ori-moto/ep-01/E1O6E5KB/";

/// The release page's `web.noudata`, as the linkhost serves it.
fn release_page(platform_url: &str) -> Vec<u8> {
    serde_json::json!({
        "noudataVersion": "0.4.0",
        "noudataType": "release",
        "playableItemTitle": "EP 01",
        "playableItemProfile": "music",
        "releaseData": {
            "credits": { "primaryArtists": [{ "primaryArtistName": "Ori Moto" }] },
            "releasePlatforms": [{
                "platformName": "Bandcamp",
                "platformType": "store",
                "platformURL": platform_url,
            }],
        },
        "mediaList": [{ "mediaTitle": "Babel" }, { "mediaTitle": "Lshn" }],
    })
    .to_string()
    .into_bytes()
}

/// An in-process web that answers every GET with `body` and counts the requests.
struct WebFx {
    body: Vec<u8>,
    hits: Mutex<Vec<String>>,
}

impl WebFx {
    fn new(body: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            body,
            hits: Mutex::new(Vec::new()),
        })
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl WebTransport for WebFx {
    fn get(&self, url: &str, _timeout: Duration) -> Result<HopResponse, TransportError> {
        self.hits
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(url.to_string());
        Ok(HopResponse {
            status: 200,
            content_type: Some("application/json".to_string()),
            location: None,
            content_length: Some(self.body.len() as u64),
            body: Box::new(std::io::Cursor::new(self.body.clone())),
        })
    }
}

fn web_hub(fx: &Arc<WebFx>) -> crate::web::WebHub {
    crate::web::WebHub::with_transport(
        crate::web::WebConfig::default(),
        Arc::clone(fx) as Arc<dyn WebTransport + Send + Sync>,
        Box::new(|| true),
    )
}

fn noubin_tap(consent: bool) -> TapRequest {
    TapRequest {
        consent: Some(consent),
        ..tap(NOUBIN_KEY)
    }
}

#[test]
fn tap_reads_the_noubin_page_only_after_consent_and_then_matches_its_store_link() {
    let temp = TempDir::new("bcs07-noubin-page");
    let (hub, _root, transport, after_connect) = connected_hub(
        &temp,
        vec![release("a:3957118897", "EP 01", "Ori Moto", Some(2))],
    );
    let fx = WebFx::new(release_page("https://orimoto.bandcamp.com/album/ep-01"));
    hub.set_web_hub(Arc::new(web_hub(&fx)));

    // First tap: the person has not agreed yet, so nothing may be contacted.
    let first = hub.tap(&noubin_tap(false)).expect("the tap answers");
    assert_eq!(first.state, TapState::LookupConsent, "{}", first.message);
    let lookup = first.lookup.as_ref().expect("the answer names what it would read");
    assert_eq!(lookup.host, "noubin.example");
    assert!(lookup.fetch_url.ends_with("/web.noudata"), "{}", lookup.fetch_url);
    assert!(fx.hits().is_empty(), "no fetch before consent: {:?}", fx.hits());

    // Second tap with consent: the page names the release, and the cache has it.
    let second = hub.tap(&noubin_tap(true)).expect("the tap answers");
    assert_eq!(
        second.state,
        TapState::OwnedDownloadable,
        "{} (candidates {:?})",
        second.message,
        second.candidates
    );
    assert_eq!(fx.hits().len(), 1, "one read of the tag's own page");
    assert!(
        second
            .candidates
            .first()
            .is_some_and(|c| c.store_release_id == "a:3957118897"),
        "the page's Bandcamp link named the cached release: {:?}",
        second.candidates
    );
    assert_eq!(
        transport.call_count(),
        after_connect,
        "the store itself is still never called"
    );
}

#[test]
fn tap_says_so_when_the_release_page_lists_no_bandcamp_release() {
    let temp = TempDir::new("bcs07-noubin-nostore");
    let (hub, _root, _transport, _) = connected_hub(&temp, Vec::new());
    let fx = WebFx::new(release_page("https://example.test/not-a-release"));
    hub.set_web_hub(Arc::new(web_hub(&fx)));

    let answer = hub.tap(&noubin_tap(true)).expect("the tap answers");
    assert_eq!(answer.state, TapState::NotFound);
    assert_eq!(answer.blocked, None, "a page without a store link is not a store failure");
    assert!(
        answer.message.contains("Noubin") || answer.message.contains("Bandcamp"),
        "the sentence says what was wrong: {}",
        answer.message
    );
    assert_eq!(fx.hits().len(), 1);
}

#[test]
fn tap_without_a_web_hub_degrades_instead_of_failing() {
    let temp = TempDir::new("bcs07-noubin-noweb");
    let (hub, _root, _transport, _) = connected_hub(&temp, Vec::new());

    let answer = hub.tap(&noubin_tap(false)).expect("the tap answers");
    assert_eq!(answer.state, TapState::NotFound);
    assert_eq!(answer.blocked, Some(TapBlocked::WebUnavailable));
    assert!(answer.lookup.is_none(), "nothing to consent to");
}

// ---------------------------------------------------------------------------
// Interest (ownLater)
// ---------------------------------------------------------------------------

#[test]
fn tap_flow_interest_is_bounded_and_cleared_after_download() {
    let temp = TempDir::new("bcs07-wants");
    let (hub, root, _transport, _) = connected_hub(&temp, Vec::new());

    // The API path records one interest.
    let answer = hub
        .want(&WantRequest {
            url: SABLE_URL.to_string(),
        })
        .expect("want records");
    assert!(answer.ok);
    assert_eq!(answer.wants, 1);
    assert_eq!(answer.max_wants, matching::MAX_WANTS);

    // Bounded, newest first: one more than the cap drops only the oldest.
    for n in 0..(matching::MAX_WANTS + 5) {
        matching::record_want(
            &root,
            matching::Want {
                url_key: format!("artist-{n}/album/release-{n}"),
                url: format!("https://artist-{n}.bandcamp.com/album/release-{n}"),
                wanted_at: now_secs(),
                title: None,
                artist: None,
            },
        )
        .expect("record");
    }
    let wants = read_wants(&root);
    assert_eq!(wants.len(), matching::MAX_WANTS, "never more than the cap");
    assert_eq!(
        wants[0].url_key, "artist-204/album/release-204",
        "the newest is first"
    );
    assert!(
        !wants.iter().any(|w| w.url_key == "artist-0/album/release-0"),
        "the oldest is what was dropped"
    );

    // Re-record the tapped link (the cap had no room for it), then answer it as a completed
    // download does: `download.rs` calls this exact function at the placed hand-off.
    hub.want(&WantRequest {
        url: SABLE_URL.to_string(),
    })
    .expect("want records");
    assert_eq!(read_wants(&root).len(), matching::MAX_WANTS, "still bounded");
    let removed =
        matching::clear_wants(&root, &["sable-music/track/were-getting-older-baby".to_string()]);
    assert_eq!(removed, 1);
    assert!(
        !read_wants(&root)
            .iter()
            .any(|w| w.url_key == "sable-music/track/were-getting-older-baby"),
        "the next tap is a plain play, not a purchase offer"
    );

    // The wants live in the associations file and are dropped with it (a disconnect).
    let file: Associations =
        serde_json::from_slice(&fs::read(associations_path(&root)).expect("file"))
            .expect("associations");
    assert_eq!(file.store_id, STORE_SLUG);
}
