//! Behaviour tests for ownership matching (BCS_06).
//!
//! Every test answers a question a person would ask ("is this mine?", "which release is
//! it?"), and the assertions are about the answer, not about the implementation: the
//! candidates, their server-computed confidence, the recorded association, the files on
//! disk, and the requests that must **not** happen. The store API is a
//! [`plippa_store::CountingTransport`], so a stray request in a "cache-only" path is a
//! failing assertion rather than a hidden fetch.

use super::matching::{associations_path, normalise_title, parse_bandcamp_url};
use super::tests_support::TempDir;
use super::*;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plippa_store::{CountingTransport, Transport as ProviderTransport, TransportResponse};

const SABLE_URL: &str = "https://sable-music.bandcamp.com/track/were-getting-older-baby";
const SABLE_ID: &str = "t:2970735116";
const USER: &str = "sable@example.test";
const PASSWORD: &str = "not-a-real-password";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn release(
    id: &str,
    title: &str,
    artist: &str,
    track_count: Option<u32>,
    duration_ms: Option<u64>,
) -> StoreRelease {
    StoreRelease {
        id: plippa_store::StoreReleaseId::new(id),
        title: title.to_string(),
        artist: artist.to_string(),
        year: Some(2026),
        track_count,
        duration_ms,
        cover: None,
        web_url: None,
        created_unix: None,
    }
}

/// The Sable single from the capture: one track, `3:36`, and the album id *is* the song id.
fn sable() -> StoreRelease {
    release(
        SABLE_ID,
        "We're Getting Older Baby",
        "Sable",
        Some(1),
        Some(216_000),
    )
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

/// A hub whose library is `temp/library` and whose cache already holds `releases`.
fn hub_with(
    temp: &TempDir,
    releases: Vec<StoreRelease>,
) -> (Arc<StoreHub>, PathBuf, Arc<CountingTransport>) {
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

/// A `.noudata` the way BCS_05 writes one: a release with credits and this store's source.
fn write_store_item(
    root: &Path,
    relative: &str,
    title: &str,
    artist: &str,
    release_id: Option<&str>,
    url: Option<&str>,
) -> PathBuf {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("parent")).expect("item dir");
    let mut source = serde_json::Map::new();
    source.insert("store".into(), serde_json::Value::String("bandcamp".into()));
    if let Some(id) = release_id {
        source.insert("releaseId".into(), serde_json::Value::String(id.into()));
    }
    if let Some(url) = url {
        source.insert("url".into(), serde_json::Value::String(url.into()));
    }
    let doc = serde_json::json!({
        "noudataVersion": "0.4.0",
        "noudataType": "release",
        "playableItemTitle": title,
        "playableItemProfile": "music",
        "releaseData": { "credits": { "primaryArtists": [{ "primaryArtistName": artist }] } },
        "mediaList": [],
        "plippaStoreSource": serde_json::Value::Object(source),
    });
    fs::write(&path, serde_json::to_vec_pretty(&doc).expect("encode")).expect("write noudata");
    path
}

fn request(url: &str) -> MatchRequest {
    MatchRequest {
        url: url.to_string(),
        expected_duration: None,
        allow_search: false,
    }
}

fn timed(url: &str, seconds: u64) -> MatchRequest {
    MatchRequest {
        url: url.to_string(),
        expected_duration: Some(seconds),
        allow_search: false,
    }
}

fn confirm_body(url: &str, id: &str, claimed: Option<f32>) -> ConfirmMatchRequest {
    ConfirmMatchRequest {
        url: url.to_string(),
        candidate: ConfirmCandidate {
            store_release_id: id.to_string(),
            title: None,
            artist: None,
            confidence: claimed,
        },
        expected_duration: None,
    }
}

fn json_response(value: serde_json::Value) -> TransportResponse {
    TransportResponse::new(
        200,
        "https://bandcamp.com/api/subsonic/rest/response.view",
        value.to_string().into_bytes(),
    )
}

fn artists_ok() -> TransportResponse {
    json_response(serde_json::json!({
        "subsonic-response": { "status": "ok", "version": "1.16.1", "artists": { "index": [] } }
    }))
}

/// A `search3` hit for the Sable single, shaped the way the store answers.
fn search_hits() -> TransportResponse {
    json_response(serde_json::json!({
        "subsonic-response": {
            "status": "ok",
            "version": "1.16.1",
            "searchResult3": {
                "song": [{
                    "id": SABLE_ID,
                    "albumId": SABLE_ID,
                    "title": "We're Getting Older Baby",
                    "artist": "Sable",
                    "duration": 216
                }]
            }
        }
    }))
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

// ---------------------------------------------------------------------------
// The link
// ---------------------------------------------------------------------------

#[test]
fn parse_bandcamp_url_rejects_non_bandcamp_and_insecure_urls() {
    for bad in [
        "",
        "   ",
        "sable-music.bandcamp.com/track/were-getting-older-baby",
        "http://sable-music.bandcamp.com/track/were-getting-older-baby",
        "ftp://sable-music.bandcamp.com/track/were-getting-older-baby",
        "javascript:alert(1)",
        "https://evil.example/track/were-getting-older-baby",
        "https://sable-music.bandcamp.com.evil.example/track/x",
        "https://bandcamp.com/track/x",
        "https://www.bandcamp.com/track/x",
        "https://user:secret@sable-music.bandcamp.com/track/x",
        "https://sable-music.bandcamp.com:8443/track/x",
        "https://sable-music.bandcamp.com/album/x/",
        "https://sable-music.bandcamp.com/track/",
        "https://sable-music.bandcamp.com/track/..",
        "https://sable-music.bandcamp.com/music",
        "https://sable-music.bandcamp.com/track/two/segments",
        "https://sable-music.bandcamp.com/track/has space",
    ] {
        assert!(
            parse_bandcamp_url(bad).is_err(),
            "{bad:?} must not be accepted as a release link"
        );
    }

    // The tracking parameters and fragment are dropped from the identity but the raw link
    // is kept for display exactly as written.
    let raw = "https://Sable-Music.BandCamp.com/track/were-getting-older-baby?from=fanpub_fnb&utm_source=x#music";
    let url = parse_bandcamp_url(raw).expect("a real link");
    assert_eq!(url.artist_slug, "sable-music");
    assert_eq!(url.kind, BandcampKind::Track);
    assert_eq!(
        url.url,
        "https://sable-music.bandcamp.com/track/were-getting-older-baby"
    );
    assert!(!url.url.contains("from="));
    assert!(!url.url.contains('#'));

    // Percent escapes are never decoded into a path segment: `%2F` stays a literal slug
    // character, so no link can smuggle a separator into a file name or an identity.
    let escaped = parse_bandcamp_url("https://x.bandcamp.com/album/a%2F..%2Fb").expect("a link");
    assert_eq!(escaped.slug, "a%2F..%2Fb");
    assert_eq!(escaped.url, "https://x.bandcamp.com/album/a%2F..%2Fb");
}

#[test]
fn normalise_title_handles_diacritics_punctuation_and_the_prefix() {
    // Diacritics fold to ASCII (the store itself sends "Bjork" for "Björk").
    assert_eq!(normalise_title("Björk"), "bjork");
    assert_eq!(
        normalise_title("Sigur Rós — Ágætis byrjun"),
        "sigur ros agaetis byrjun"
    );
    assert_eq!(normalise_title("Æther"), "aether");
    // Apostrophes vanish, so the slug "were-getting-older-baby" matches "We're …".
    assert_eq!(
        normalise_title("We're Getting Older Baby"),
        "were getting older baby"
    );
    // `&` and `and` are the same title; every other punctuation run becomes one space.
    assert_eq!(
        normalise_title("Rock & Roll"),
        normalise_title("Rock and Roll")
    );
    assert_eq!(normalise_title("Rock-and-Roll!"), "rock and roll");
    // A leading "The" is not part of the title, but a title that *is* "The" keeps it.
    assert_eq!(normalise_title("The National"), "national");
    assert_eq!(normalise_title("The"), "the");
    // Empty and punctuation-only titles normalise to nothing, which never matches.
    assert_eq!(normalise_title("   "), "");
    assert_eq!(normalise_title("!!!"), "");
}

// ---------------------------------------------------------------------------
// Stage 1: what the library / an association already records
// ---------------------------------------------------------------------------

#[test]
fn match_stage1_uses_recorded_store_source_with_zero_requests() {
    let temp = TempDir::new("match-stage1");
    let (hub, root, transport) = hub_with(&temp, vec![sable()]);
    write_store_item(
        &root,
        "Music/Sable/We're Getting Older Baby/release.noudata",
        "We're Getting Older Baby",
        "Sable",
        Some(SABLE_ID),
        Some("https://sable-music.bandcamp.com/track/were-getting-older-baby?from=fanpub"),
    );

    let result = hub.match_release(&request(SABLE_URL)).expect("a result");

    assert_eq!(
        transport.call_count(),
        0,
        "stage 1 must not touch the network"
    );
    let owned = result.owned.expect("the library item proves ownership");
    assert!(owned.noudata_path.ends_with("release.noudata"), "{owned:?}");
    assert_eq!(owned.store_release_id.as_deref(), Some(SABLE_ID));
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.candidates[0].store_release_id, SABLE_ID);
    assert_eq!(result.candidates[0].source, "library");
    assert!(result.confidence >= AUTO_USABLE_CONFIDENCE);
    assert!(result.auto_usable);
    assert!(!result.requires_confirmation);
    assert_eq!(result.reason, None, "owning it is not a miss");
}

#[test]
fn match_single_track_release_where_ids_collide() {
    // The capture's quirk: a one-track release reports the *same* id for the album and the
    // song, and `song.album` holds the artist name. Neither may confuse the matcher: the
    // candidate is the release id exactly as the store sent it.
    let temp = TempDir::new("match-single");
    let (hub, _root, transport) = hub_with(&temp, vec![sable()]);

    let result = hub.match_release(&timed(SABLE_URL, 216)).expect("a result");

    assert_eq!(transport.call_count(), 0);
    assert_eq!(result.candidates.len(), 1);
    let candidate = &result.candidates[0];
    assert_eq!(candidate.store_release_id, SABLE_ID);
    assert_eq!(candidate.title, "We're Getting Older Baby");
    assert_eq!(candidate.artist, "Sable");
    assert_eq!(candidate.track_count, Some(1));
    assert_eq!(candidate.duration_s, Some(216));
    assert_eq!(candidate.source, "cache");
    assert!(
        candidate.confidence >= AUTO_USABLE_CONFIDENCE,
        "{candidate:?}"
    );
    assert_eq!(result.reason, None);
}

#[test]
fn match_artist_name_in_song_album_field_does_not_fool_the_matcher() {
    // Some other release in the collection whose *title* is the artist's name (what
    // `song.album` returns for a single on this store). It must not be offered for a link
    // to a different title, and it must not hide the real release.
    let temp = TempDir::new("match-album-field");
    let decoy = release("a:1000", "Sable", "Sable", Some(1), Some(216_000));
    let (hub, _root, transport) = hub_with(&temp, vec![decoy, sable()]);

    let result = hub.match_release(&request(SABLE_URL)).expect("a result");

    assert_eq!(transport.call_count(), 0);
    assert_eq!(result.candidates.len(), 1, "{:?}", result.candidates);
    assert_eq!(result.candidates[0].store_release_id, SABLE_ID);
    assert!(
        result
            .candidates
            .iter()
            .all(|candidate| candidate.store_release_id != "a:1000"),
        "the artist-name-as-title decoy must never be offered"
    );
}

// ---------------------------------------------------------------------------
// Stage 2: artist + normalised title against the cache
// ---------------------------------------------------------------------------

#[test]
fn match_punctuation_variants_are_the_same_title() {
    // The link says "and", the store says "&". One normalised title, one candidate.
    let temp = TempDir::new("match-punctuation");
    let cached = release("a:2000", "Rock & Roll", "Sable", Some(9), Some(1_800_000));
    let (hub, _root, transport) = hub_with(&temp, vec![cached]);

    let result = hub
        .match_release(&request(
            "https://sable-music.bandcamp.com/album/rock-and-roll",
        ))
        .expect("a result");

    assert_eq!(transport.call_count(), 0);
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.candidates[0].store_release_id, "a:2000");
    assert!(result.candidates[0].confidence >= AUTO_USABLE_CONFIDENCE);
    assert!(result.auto_usable);
}

#[test]
fn match_owned_but_not_downloaded_is_a_candidate_not_a_failure() {
    // The release is in the store collection, the library is empty: the answer is "here is
    // the release to download", not "not found".
    let temp = TempDir::new("match-owned-not-downloaded");
    let (hub, root, transport) = hub_with(&temp, vec![sable()]);
    let entries: Vec<String> = root
        .read_dir()
        .expect("readable")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        entries.iter().all(|name| name == ".plippa"),
        "the library holds no items yet: {entries:?}"
    );

    let result = hub.match_release(&request(SABLE_URL)).expect("a result");

    assert_eq!(transport.call_count(), 0);
    assert!(result.owned.is_none());
    assert_eq!(result.candidates.len(), 1);
    assert_eq!(result.candidates[0].store_release_id, SABLE_ID);
    assert_eq!(result.reason, None);
    assert!(result.auto_usable);
}

#[test]
fn match_duration_fingerprint_promotes_a_weak_title_match() {
    // The link's slug is the clean title; the store's release carries a "Live" suffix, so
    // the title only contains the other. Without a duration that is a "confirm" answer;
    // with the caller's expected length it is an automatic one.
    let temp = TempDir::new("match-duration");
    let cached = release(
        "a:2",
        "We're Getting Older Baby (Live)",
        "Sable",
        Some(1),
        Some(216_000),
    );
    let (hub, _root, transport) = hub_with(&temp, vec![cached]);

    let without = hub.match_release(&request(SABLE_URL)).expect("a result");
    assert_eq!(transport.call_count(), 0);
    assert_eq!(without.candidates.len(), 1);
    assert!(
        (0.6..AUTO_USABLE_CONFIDENCE).contains(&without.confidence),
        "{without:?}"
    );
    assert!(without.requires_confirmation);
    assert!(!without.auto_usable);

    let with = hub.match_release(&timed(SABLE_URL, 216)).expect("a result");
    assert_eq!(transport.call_count(), 0);
    assert_eq!(with.candidates.len(), 1);
    assert!(with.confidence > without.confidence, "{with:?}");
    assert!(with.confidence >= AUTO_USABLE_CONFIDENCE, "{with:?}");
    assert!(with.auto_usable);
    assert!(
        with.candidates[0]
            .reasons
            .iter()
            .any(|reason| reason.contains("2 s")),
        "the duration must be stated as a reason: {:?}",
        with.candidates[0].reasons
    );
}

#[test]
fn match_confidence_bands_are_enforced_server_side() {
    let temp = TempDir::new("match-bands");
    let exact = sable();
    let weak = release(
        "a:2",
        "We're Getting Older Baby (Live)",
        "Sable",
        Some(1),
        None,
    );
    // A cached release the matcher does not connect to this link at all.
    let stranger = release("a:3", "An Unrelated Record", "Someone Else", Some(4), None);
    let (hub, _root, transport) = hub_with(&temp, vec![exact, weak, stranger]);

    // Everything below the confirm band is dropped before the payload leaves the server.
    let result = hub.match_release(&request(SABLE_URL)).expect("a result");
    assert!(!result.candidates.is_empty());
    assert!(
        result
            .candidates
            .iter()
            .all(|candidate| candidate.confidence >= CONFIRM_CONFIDENCE),
        "{:?}",
        result.candidates
    );
    assert!(result
        .leads
        .iter()
        .all(|lead| lead.confidence < CONFIRM_CONFIDENCE));

    // A released 0.9 candidate is auto-usable; a 0.7 one is not.
    let auto = result
        .candidates
        .iter()
        .find(|candidate| candidate.store_release_id == SABLE_ID)
        .expect("the exact release");
    assert!(auto.confidence >= AUTO_USABLE_CONFIDENCE);
    let confirm = result
        .candidates
        .iter()
        .find(|candidate| candidate.store_release_id == "a:2")
        .expect("the live release");
    assert!(confirm.confidence >= CONFIRM_CONFIDENCE);
    assert!(confirm.confidence < AUTO_USABLE_CONFIDENCE);

    // A client claim is not a score: confirming the weak candidate with a fabricated 0.99
    // records the server's 0.7, and confirms the band from that, not from the claim.
    let saved = hub
        .confirm_match(&confirm_body(SABLE_URL, "a:2", Some(0.99)))
        .expect("the candidate is real");
    assert!(
        (CONFIRM_CONFIDENCE..AUTO_USABLE_CONFIDENCE).contains(&saved.confidence),
        "{saved:?}"
    );
    assert_eq!(saved.association.confidence, saved.confidence);

    // A release the server cannot verify is refused outright, whatever the claim is. The
    // store is not asked either: the cache is the only authority for a confirmation.
    let calls_before = transport.call_count();
    let unknown = hub
        .confirm_match(&confirm_body(SABLE_URL, "a:does-not-exist", Some(0.99)))
        .expect_err("an invented release must be refused");
    assert_eq!(unknown.code(), "unknown_release");
    assert_eq!(transport.call_count(), calls_before);

    // A cached release the matcher does not connect to this link scores 0 and is refused.
    let low = hub
        .confirm_match(&confirm_body(SABLE_URL, "a:3", Some(0.99)))
        .expect_err("an unrelated release must be refused");
    assert_eq!(low.code(), "low_confidence");
}

#[test]
fn match_not_found_is_a_result_not_an_error() {
    let temp = TempDir::new("match-absent");
    // The collection holds other releases by other artists; the link's artist is absent.
    let (hub, _root, transport) = hub_with(
        &temp,
        vec![release(
            "a:1",
            "Some Album",
            "Someone Else",
            Some(8),
            Some(600_000),
        )],
    );

    let result = hub
        .match_release(&timed(SABLE_URL, 216))
        .expect("a miss is an answer, not an error");

    assert_eq!(transport.call_count(), 0);
    assert!(result.candidates.is_empty());
    assert!(result.leads.is_empty());
    assert_eq!(result.reason.as_deref(), Some("not_in_collection"));
    assert_eq!(result.confidence, 0.0);
    assert!(!result.auto_usable);
    assert!(!result.requires_confirmation);
    assert!(result.owned.is_none());
}

#[test]
fn match_does_not_consult_search_unless_asked() {
    // Even with credentials, a plain lookup (the GET shape) is cache-only: the artist is
    // not cached, yet no `search3` call is made.
    let temp = TempDir::new("match-no-search");
    let root = library_dir(&temp);
    let transport = Arc::new(CountingTransport::new());
    let hub = Arc::new(StoreHub::with_transport(
        Some(temp.path().join("secrets")),
        Arc::clone(&transport) as Arc<dyn ProviderTransport + Send + Sync>,
    ));
    hub.set_library_root(Some(root.clone()));
    write_cache(&root, Vec::new());
    transport.push_response(artists_ok());
    hub.connect(USER, PASSWORD, Some(true)).expect("connect");
    let after_connect = transport.call_count();

    let result = hub.match_release(&request(SABLE_URL)).expect("a result");
    assert_eq!(result.reason.as_deref(), Some("not_in_collection"));
    assert_eq!(
        transport.call_count(),
        after_connect,
        "a cache-only lookup must make no request: {:?}",
        transport.urls()
    );

    // Opting in turns on the tie-breaker, but a `search3` hit is a lead below the
    // confirmation band, never a candidate.
    transport.push_response(search_hits());
    let with_search = hub
        .match_release(&MatchRequest {
            url: SABLE_URL.to_string(),
            expected_duration: Some(216),
            allow_search: true,
        })
        .expect("a result");
    assert_eq!(transport.call_count(), after_connect + 1);
    assert!(
        transport
            .urls()
            .last()
            .expect("a call")
            .contains("search3.view"),
        "{:?}",
        transport.urls()
    );
    assert!(
        with_search.candidates.is_empty(),
        "{:?}",
        with_search.candidates
    );
    assert_eq!(with_search.reason.as_deref(), Some("not_in_collection"));
    assert_eq!(with_search.leads.len(), 1);
    assert_eq!(with_search.leads[0].source, "search");
    assert!(with_search.leads[0].confidence <= SEARCH_CONFIDENCE_CAP);
    assert!(!with_search.auto_usable);
    assert!(!with_search.requires_confirmation);
}

// ---------------------------------------------------------------------------
// Confirm: the next lookup is stage 1
// ---------------------------------------------------------------------------

#[test]
fn match_confirm_makes_the_next_lookup_zero_request() {
    let temp = TempDir::new("match-confirm");
    let (hub, root, transport) = hub_with(&temp, vec![sable()]);

    let first = hub.match_release(&request(SABLE_URL)).expect("a result");
    assert_eq!(first.candidates[0].source, "cache");

    // The client sends a score of its own; the server recomputes 0.9 and stores that.
    let saved = hub
        .confirm_match(&confirm_body(SABLE_URL, SABLE_ID, Some(0.5)))
        .expect("a real candidate");
    assert!(saved.ok);
    assert_eq!(saved.store_release_id, SABLE_ID);
    assert!(saved.confidence >= AUTO_USABLE_CONFIDENCE, "{saved:?}");
    assert_eq!(saved.association.title, "We're Getting Older Baby");
    assert_eq!(saved.provenance_updated, 0, "no library item exists yet");
    assert_eq!(
        transport.call_count(),
        0,
        "confirming is a disk write, not a fetch"
    );

    let path = associations_path(&root);
    assert!(path.exists(), "the association must be recorded on disk");
    #[cfg(unix)]
    assert_eq!(mode(&path), 0o600, "the associations file must be private");
    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
    assert_eq!(stored["entries"][0]["storeReleaseId"], SABLE_ID);
    assert_eq!(
        path.file_name().and_then(|n| n.to_str()),
        Some("bandcamp.associations.json")
    );

    // The next lookup answers from the association alone: no cache candidate, no request,
    // and the top confidence is the recorded decision.
    let second = hub.match_release(&request(SABLE_URL)).expect("a result");
    assert_eq!(transport.call_count(), 0);
    assert_eq!(second.candidates.len(), 1);
    assert_eq!(second.candidates[0].source, "association");
    assert_eq!(second.candidates[0].store_release_id, SABLE_ID);
    assert!((second.confidence - ASSOCIATION_CONFIDENCE).abs() < 1e-6);
    assert!(second.auto_usable);

    // Confirming again is idempotent: one entry, same answer.
    let again = hub
        .confirm_match(&confirm_body(SABLE_URL, SABLE_ID, Some(0.01)))
        .expect("idempotent");
    assert_eq!(again.association.url_key, saved.association.url_key);
    let stored: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
    assert_eq!(stored["entries"].as_array().expect("entries").len(), 1);
}

#[test]
fn match_confirm_completes_provenance_on_an_existing_item() {
    // BCS_04/05 wrote a source without a release id (the store exposes none in the API);
    // confirming the purchase completes it, so the item becomes exact evidence.
    let temp = TempDir::new("match-provenance");
    let (hub, root, transport) = hub_with(&temp, vec![sable()]);
    let item = write_store_item(
        &root,
        "Music/Sable/We're Getting Older Baby/release.noudata",
        "We're Getting Older Baby",
        "Sable",
        None,
        None,
    );

    let saved = hub
        .confirm_match(&confirm_body(SABLE_URL, SABLE_ID, None))
        .expect("a real candidate");
    assert_eq!(saved.provenance_updated, 1);
    assert_eq!(transport.call_count(), 0);

    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(&item).expect("read")).expect("json");
    assert_eq!(
        written["plippaStoreSource"]["releaseId"], SABLE_ID,
        "{written}"
    );
    assert_eq!(written["plippaStoreSource"]["store"], "bandcamp");
    assert_eq!(written["plippaStoreSource"]["url"], SABLE_URL);

    // Stage 1 now answers from the item itself, not only from the association file.
    let next = hub.match_release(&request(SABLE_URL)).expect("a result");
    let owned = next.owned.expect("the completed item proves ownership");
    assert_eq!(owned.store_release_id.as_deref(), Some(SABLE_ID));
    assert_eq!(transport.call_count(), 0);
}
