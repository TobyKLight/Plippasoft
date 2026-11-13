//! Behaviour tests for browsing and downloading a connected collection (BCS_04).
//!
//! The store API is a scripted [`plippa_store::Transport`]; the media path is an in-process
//! [`MediaFx`] that hands back readers exactly as `UreqTransport` does, so `download_to_temp`
//! exercises the same streaming, caps, sniffing and cleanup code NB_09 proved. Every
//! assertion is about what a person would see (a consent payload, a queue item, a file on
//! disk) or what must never happen (a write during prepare, a non-audio file, a request to
//! an unlisted CDN).

use super::tests_support::TempDir;
use super::*;

use std::collections::{BTreeMap, VecDeque};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plippa_store::{
    HeaderMap, StoreError as ProviderError, Transport as ProviderTransport, TransportResponse,
};

use crate::ipc::NullIpc;
use crate::web::download::DownloadLimits;
use crate::web::fetch::{HopResponse, Transport as WebTransport, TransportError};

const MIB: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Scripted store API
// ---------------------------------------------------------------------------

struct Script {
    queue: Mutex<VecDeque<Result<TransportResponse, ProviderError>>>,
    urls: Mutex<Vec<String>>,
}

impl Script {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            urls: Mutex::new(Vec::new()),
        }
    }

    fn push(&self, value: serde_json::Value) {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(Ok(json_response(value.to_string())));
    }

    fn urls(&self) -> Vec<String> {
        self.urls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl ProviderTransport for Script {
    fn get(
        &self,
        url: &str,
        _headers: &HeaderMap,
        _max_bytes: u64,
    ) -> Result<TransportResponse, ProviderError> {
        self.urls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(url.to_string());
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .unwrap_or_else(|| Err(ProviderError::transport_url(url, "unexpected request")))
    }
}

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

fn artists_ok() -> serde_json::Value {
    serde_json::json!({
        "subsonic-response": { "status": "ok", "version": "1.16.1", "artists": { "index": [] } }
    })
}

/// The store's answer for an endpoint it does not implement: `download` today.
fn download_unimplemented() -> serde_json::Value {
    serde_json::json!({ "error": true, "error_message": "bad version" })
}

struct Song<'a> {
    id: &'a str,
    title: &'a str,
    track: u32,
    duration_s: u64,
    size: Option<u64>,
    bitrate: Option<u32>,
    suffix: &'a str,
}

fn song(s: Song<'_>) -> serde_json::Value {
    let mut value = serde_json::json!({
        "id": s.id,
        "title": s.title,
        "track": s.track,
        "discNumber": 1,
        "duration": s.duration_s,
        "suffix": s.suffix,
        "coverArt": "c:release",
    });
    if let Some(size) = s.size {
        value["size"] = serde_json::json!(size);
    }
    if let Some(bitrate) = s.bitrate {
        value["bitRate"] = serde_json::json!(bitrate);
    }
    value
}

fn album(
    id: &str,
    title: &str,
    artist: &str,
    year: u16,
    songs: Vec<serde_json::Value>,
) -> serde_json::Value {
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
                "coverArt": "c:release",
                "song": songs,
            }
        }
    })
}

fn one_track_album(id: &str, track_id: &str) -> serde_json::Value {
    album(
        id,
        "A Release",
        "The Artist",
        2026,
        vec![song(Song {
            id: track_id,
            title: "One",
            track: 1,
            duration_s: 30,
            size: Some(1_000),
            bitrate: Some(320),
            suffix: "mp3",
        })],
    )
}

// ---------------------------------------------------------------------------
// Loopback media fixture
// ---------------------------------------------------------------------------

/// A body reader that yields in chunks, reports completion, and releases its "in flight"
/// slot only once the downloader has drained it (or dropped it, e.g. on cancel).
struct Slow {
    body: Vec<u8>,
    pos: usize,
    chunk: usize,
    delay: Duration,
    log: Arc<Mutex<Vec<String>>>,
    label: String,
    in_flight: Arc<AtomicUsize>,
    released: bool,
}

impl Slow {
    fn finish(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if !self.label.is_empty() {
            self.log
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(format!("done:{}", self.label));
            self.label.clear();
        }
        self.in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
    }
}

impl Read for Slow {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.body.len() {
            self.finish();
            return Ok(0);
        }
        std::thread::sleep(self.delay);
        let take = self.chunk.min(buf.len()).min(self.body.len() - self.pos);
        buf[..take].copy_from_slice(&self.body[self.pos..self.pos + take]);
        self.pos += take;
        Ok(take)
    }
}

impl Drop for Slow {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The media side of a download: a fake HTTPS transport, standing in for the BCS_03
/// `StoreTransport` the way the loopback fixture did before it learned to allow locals.
enum Reply {
    Bytes {
        content_type: &'static str,
        body: Vec<u8>,
    },
    Redirect(String),
    Slow {
        body: Vec<u8>,
        chunk: usize,
        delay: Duration,
    },
}

struct MediaFx {
    handler: Box<dyn Fn(&str) -> Reply + Send + Sync>,
    urls: Mutex<Vec<String>>,
    log: Arc<Mutex<Vec<String>>>,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
}

impl MediaFx {
    fn new(handler: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        Self {
            handler: Box::new(handler),
            urls: Mutex::new(Vec::new()),
            log: Arc::new(Mutex::new(Vec::new())),
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn audio(body: Vec<u8>) -> Self {
        Self::new(move |_| Reply::Bytes {
            content_type: "audio/mpeg",
            body: body.clone(),
        })
    }

    fn hits(&self) -> Vec<String> {
        self.urls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn log(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn peak(&self) -> usize {
        self.max_in_flight.load(AtomicOrdering::SeqCst)
    }
}

impl WebTransport for MediaFx {
    fn get(&self, url: &str, _timeout: Duration) -> Result<HopResponse, TransportError> {
        self.urls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(url.to_string());
        self.log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(format!("start:{url}"));
        let now = self.in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, AtomicOrdering::SeqCst);
        let reply = (self.handler)(url);
        match reply {
            Reply::Bytes { content_type, body } => Ok(HopResponse {
                status: 200,
                content_type: Some(content_type.to_string()),
                location: None,
                content_length: Some(body.len() as u64),
                body: Box::new(Slow {
                    body,
                    pos: 0,
                    chunk: usize::MAX,
                    delay: Duration::ZERO,
                    log: Arc::clone(&self.log),
                    label: String::new(),
                    in_flight: Arc::clone(&self.in_flight),
                    released: false,
                }),
            }),
            Reply::Slow { body, chunk, delay } => Ok(HopResponse {
                status: 200,
                content_type: Some("audio/mpeg".to_string()),
                location: None,
                content_length: Some(body.len() as u64),
                body: Box::new(Slow {
                    body,
                    pos: 0,
                    chunk,
                    delay,
                    log: Arc::clone(&self.log),
                    label: url.to_string(),
                    in_flight: Arc::clone(&self.in_flight),
                    released: false,
                }),
            }),
            Reply::Redirect(to) => Ok(HopResponse {
                status: 302,
                content_type: None,
                location: Some(to),
                content_length: Some(0),
                body: Box::new(Cursor::new(Vec::new())),
            }),
        }
    }
}

/// A minimal MP3 frame (an ID3 tag is enough for the magic-byte sniff).
fn mp3(tag: u8) -> Vec<u8> {
    let mut body = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
    body.resize(4096, tag);
    body
}

// ---------------------------------------------------------------------------
// Hub rig
// ---------------------------------------------------------------------------

fn ample_free(_: &Path) -> Option<u64> {
    Some(1 << 40)
}

fn tiny_free(_: &Path) -> Option<u64> {
    Some(256 * 1024)
}

fn test_limits() -> DownloadLimits {
    DownloadLimits {
        max_file_bytes: 8 * MIB,
        max_batch_bytes: 64 * MIB,
        free_reserve: 0,
        free_space: ample_free,
        stall_timeout: Duration::from_secs(2),
        log_url: false,
        ..DownloadLimits::default()
    }
}

struct Rig {
    temp: TempDir,
    hub: Arc<StoreHub>,
    library: PathBuf,
}

impl Rig {
    /// A connected hub pointed at the real Bandcamp URL. Nothing here opens a socket: the
    /// store API is the scripted provider transport and the media side is a [`MediaFx`].
    fn new(tag: &str, script: Arc<Script>) -> Self {
        let temp = TempDir::new(tag);
        let library = temp.path().join("library");
        std::fs::create_dir_all(&library).expect("library root");
        let hub = Arc::new(
            StoreHub::with_transport(Some(temp.path().join("secrets")), script.clone())
                .with_sync_tuning(200, Duration::ZERO)
                .with_download_tuning(test_limits(), Duration::from_millis(20)),
        );
        hub.set_library_root(Some(library.clone()));
        script.push(artists_ok());
        hub.connect("fan@example.com", "s3cret", Some(true))
            .expect("connect");
        Self {
            temp,
            hub,
            library,
        }
    }

    fn tmp(&self) -> PathBuf {
        self.library.join(cache::STATE_DIR).join("tmp")
    }

    fn stage_dirs(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.tmp())
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok().map(|e| e.path()))
                    .filter(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .map(|name| name.starts_with("store-"))
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn snapshot(&self) -> BTreeMap<String, (bool, u64)> {
        let mut out = BTreeMap::new();
        snapshot_into(&self.library, &self.library, &mut out);
        out
    }

    fn prepare(&self, tracks: &[(&str, &str)]) -> DownloadConsent {
        self.hub
            .download_prepare(PrepareDownloadRequest {
                releases: Vec::new(),
                tracks: tracks
                    .iter()
                    .map(|(release, track)| TrackSelection {
                        release: release.to_string(),
                        track: track.to_string(),
                    })
                    .collect(),
                noubin_key: None,
            })
            .expect("prepare")
    }

    fn start(
        &self,
        transport: Arc<dyn WebTransport>,
        consent: Option<bool>,
        consent_id: Option<&str>,
    ) -> Result<DownloadStart, StoreApiError> {
        self.hub.download_start(
            transport,
            DownloadRequest {
                consent,
                consent_id: consent_id.map(str::to_string),
            },
        )
    }

    fn status(&self, session: &str) -> DownloadStatus {
        self.hub.download_status(session).expect("download status")
    }

    fn wait_terminal(&self, session: &str) -> DownloadStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = self.status(session);
            if status.state != DownloadState::Running {
                return status;
            }
            assert!(Instant::now() < deadline, "download did not finish: {status:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn snapshot_into(root: &Path, dir: &Path, out: &mut BTreeMap<String, (bool, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let key = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        let is_dir = meta.is_dir();
        out.insert(key, (is_dir, if is_dir { 0 } else { meta.len() }));
        if is_dir {
            snapshot_into(root, &path, out);
        }
    }
}

fn release_cache(library: &Path, releases: Vec<plippa_store::StoreRelease>) {
    let cache = cache::StoreCache {
        store_id: STORE_SLUG.to_string(),
        releases,
        page_size: 200,
        crawled_at: now_secs(),
        complete: true,
        last_error: None,
    };
    cache::write_private_atomic(
        &cache::cache_path(library, STORE_SLUG),
        &cache.encode().expect("encode cache"),
    )
    .expect("write cache");
}

fn release(id: &str, title: &str, artist: &str, cover: bool) -> plippa_store::StoreRelease {
    plippa_store::StoreRelease {
        id: plippa_store::StoreReleaseId::new(id),
        title: title.to_string(),
        artist: artist.to_string(),
        year: Some(2026),
        track_count: Some(2),
        duration_ms: Some(125_000),
        cover: cover.then(|| plippa_store::StoreCover {
            // A distinct id per release: the hub caches cover bytes by cover id, so a
            // shared id would hide whether the second release made its own request.
            id: plippa_store::StoreCoverId::new(format!("c:{id}")),
            mime: None,
        }),
        web_url: None,
        created_unix: None,
    }
}

// ---------------------------------------------------------------------------
// Browsing
// ---------------------------------------------------------------------------

#[test]
fn store_collection_search_is_case_insensitive_and_paged() {
    let temp = TempDir::new("collection-paged");
    let hub = Arc::new(StoreHub::with_transport(
        Some(temp.path().join("secrets")),
        Arc::new(Script::new()),
    ));
    let library = temp.path().join("library");
    std::fs::create_dir_all(&library).expect("library");
    hub.set_library_root(Some(library.clone()));
    release_cache(
        &library,
        vec![
            release("a:1", "Björk — Homogenic", "Björk", true),
            release("a:2", "Ágætis byrjun", "Sigur Rós", false),
            release("a:3", "Selected Ambient Works", "Aphex Twin", true),
            release("a:4", "BJORK REMIXES", "Various Artists", false),
        ],
    );

    let page = hub.collection("BJORK", 0, 10);
    assert_eq!(page.total, 2, "title or artist, case folded");
    assert_eq!(page.offset, 0);
    assert_eq!(page.size, 10);
    assert!(!page.stale, "a fresh crawl is not stale");
    assert!(page.synced_at.is_some());
    assert_eq!(page.releases.len(), 2);
    assert_eq!(page.releases[0].id, "a:1");

    // Accented search works in both directions: "sigur ros" finds "Sigur Rós".
    let accented = hub.collection("sigur ros", 0, 10);
    assert_eq!(accented.total, 1);
    assert_eq!(accented.releases[0].id, "a:2");
    assert!(!accented.releases[0].has_cover);

    // Paging is a slice of the matching set, not of the whole collection.
    let second = hub.collection("bjork", 1, 1);
    assert_eq!(second.total, 2, "total counts every match, not the page");
    assert_eq!(second.offset, 1);
    assert_eq!(second.releases.len(), 1);
    assert_eq!(second.releases[0].id, "a:4");

    // Size is clamped to the endpoint's own bounds.
    assert_eq!(hub.collection("", 0, 0).size, 1);
    assert_eq!(hub.collection("", 0, 99_999).size, MAX_PAGE_SIZE);

    // An absent cache is an empty, stale page rather than an error.
    let empty = TempDir::new("collection-empty");
    let empty_hub = Arc::new(StoreHub::with_transport(
        Some(empty.path().join("secrets")),
        Arc::new(Script::new()),
    ));
    empty_hub.set_library_root(Some(empty.path().join("library")));
    let none = empty_hub.collection("anything", 0, 10);
    assert_eq!(none.total, 0);
    assert!(none.stale);
    assert!(none.releases.is_empty());
}

#[test]
fn store_release_detail_reports_lossy_quality_and_declared_fields() {
    let script = Arc::new(Script::new());
    let rig = Rig::new("release-detail", Arc::clone(&script));
    script.push(album(
        "a:9",
        "Detail Release",
        "The Artist",
        1999,
        vec![song(Song {
            id: "t:1",
            title: "First",
            track: 3,
            duration_s: 125,
            size: Some(12_345_678),
            bitrate: Some(1411),
            suffix: "flac",
        })],
    ));

    // SRV-06: the GET is cache-only, so an uncached release is refused without a store
    // call...
    let before = script.urls().len();
    assert_eq!(
        rig.hub.release_view("a:9").expect_err("uncached").code(),
        "not_cached"
    );
    assert_eq!(
        script.urls().len(),
        before,
        "a cache miss must not reach the store"
    );

    // ...and the JSON POST is the lookup that fills the cache.
    let view = rig.hub.lookup_release("a:9").expect("release lookup");
    assert_eq!(view.release.id, "a:9");
    assert_eq!(view.release.title, "Detail Release");
    assert_eq!(view.release.year, Some(1999));
    assert_eq!(view.tracks.len(), 1);
    let track = &view.tracks[0];
    assert_eq!(track.id, "t:1");
    assert_eq!(track.track, Some(3));
    assert_eq!(track.duration_s, Some(125));
    assert_eq!(track.suffix.as_deref(), Some("flac"));
    assert_eq!(track.declared_bytes, Some(12_345_678));
    assert_eq!(track.declared_bitrate, Some(1411));

    // The honesty requirement: the API says lossy, and says why.
    assert_eq!(view.quality.audio, "lossy");
    assert_eq!(view.quality.bitrate_kbps, None);
    assert_eq!(view.quality.note, QUALITY_NOTE);
    assert!(QUALITY_NOTE.contains("lossy stream"));
    assert!(!view.capabilities.download);
    assert!(view.capabilities.download_unverified);
    assert!(!view.cached, "a lookup was not in the cache");

    // No token-bearing audio URL is in the payload.
    let json = serde_json::to_string(&view).expect("serialise view");
    assert!(!json.contains("stream.view"), "{json}");
    assert!(!json.contains("download.view"), "{json}");
    assert!(!json.contains("?u="), "{json}");
}

#[test]
fn store_cover_is_sniffed_and_never_proxies_an_arbitrary_url() {
    let script = Arc::new(Script::new());
    let rig = Rig::new("cover", Arc::clone(&script));

    // A cached release with a provider cover id: the bytes decide the content type.
    release_cache(&rig.library, vec![release("a:1", "Cover Release", "A", true)]);
    // Cache first means no album request; the provider is asked only for the cover id.
    let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 16, 0, 0];
    let jpeg_body = jpeg.clone();
    script.queue_for_cover(jpeg_body);
    let image = rig.hub.cover("a:1").expect("cover");
    assert_eq!(image.content_type, "image/jpeg");
    assert_eq!(image.bytes.as_slice(), jpeg.as_slice());

    // A cover that is HTML is refused, not relayed. The provider classifies an HTML
    // body at HTTP 200 as a bot challenge, so the portal says "blocked" rather than
    // handing the page to an `<img>`.
    release_cache(&rig.library, vec![release("a:2", "Bad Cover", "A", true)]);
    script.queue_for_cover(b"<html>not an image</html>".to_vec());
    let err = rig.hub.cover("a:2").expect_err("html is not an image");
    assert_eq!(err.code(), "blocked");

    // An uncached id (including a client-supplied URL, which is never fetched as a URL)
    // answers `not_cached` without any request at all (SRV-06).
    let evil = "https://evil.example/track.mp3";
    let before = script.urls().len();
    let err = rig.hub.cover(evil).expect_err("an arbitrary url has no cover");
    assert_eq!(err.code(), "not_cached");
    let urls = script.urls();
    assert_eq!(urls.len(), before, "no lookup, no cover request");
    assert!(
        !urls.iter().any(|url| url.contains("evil.example") && url.contains("getCoverArt")),
        "a URL must never become a cover request: {urls:?}"
    );

    // The arbitrary URL never became a cache key either.
    assert!(rig.hub.cached_release(evil).is_none());
}

impl Script {
    /// Queue one `getCoverArt` binary response.
    fn queue_for_cover(&self, body: Vec<u8>) {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(Ok(TransportResponse {
                status: 200,
                headers: vec![(
                    "Content-Type".to_string(),
                    "application/octet-stream".to_string(),
                )],
                body,
                final_url: "https://bandcamp.com/api/subsonic/rest/getCoverArt.view".to_string(),
                redirects: Vec::new(),
            }));
    }
}

// ---------------------------------------------------------------------------
// Prepare and consent
// ---------------------------------------------------------------------------

#[test]
fn store_download_prepare_returns_consent_and_writes_nothing() {
    let script = Arc::new(Script::new());
    let rig = Rig::new("prepare", Arc::clone(&script));
    script.push(album(
        "a:1",
        "Prepare Release",
        "The Artist",
        2026,
        vec![
            song(Song {
                id: "t:1",
                title: "One",
                track: 1,
                duration_s: 30,
                size: Some(2 * MIB),
                bitrate: Some(320),
                suffix: "mp3",
            }),
            song(Song {
                id: "t:2",
                title: "Two",
                track: 2,
                duration_s: 40,
                size: None,
                bitrate: None,
                suffix: "mp3",
            }),
        ],
    ));

    let before = rig.snapshot();
    let consent = rig.prepare(&[("a:1", "t:1"), ("a:1", "t:2")]);
    let after = rig.snapshot();
    assert_eq!(before, after, "prepare must not write one byte");
    assert!(!rig.tmp().exists(), "prepare must not create the staging area");

    assert!(!consent.consent_id.is_empty());
    assert_eq!(consent.store, STORE_SLUG);
    assert_eq!(consent.tracks, 2);
    assert_eq!(consent.releases.len(), 1);
    assert_eq!(consent.releases[0].title, "Prepare Release");
    assert_eq!(consent.releases[0].track_count, 2);
    assert_eq!(consent.releases[0].estimated_bytes, Some(2 * MIB));
    assert!(consent.releases[0].estimated);
    assert_eq!(consent.estimated_bytes, Some(2 * MIB));
    assert!(consent.estimated, "derived from declared sizes");
    assert!(consent.free_bytes.is_some());
    assert_eq!(consent.limits.max_file_bytes, 8 * MIB);
    assert_eq!(consent.limits.max_batch_bytes, 64 * MIB);
    assert_eq!(consent.quality.audio, "lossy");
    assert_eq!(consent.quality.warning, download::STREAM_WARNING);
    assert!(consent.consent_message.contains("lossy"));
    assert!(consent.refused.is_empty());
    // Per track: the declared size is marked as an estimate, and a missing one stays
    // null rather than being invented.
    assert_eq!(consent.releases[0].tracks[0].estimated_bytes, Some(2 * MIB));
    assert!(consent.releases[0].tracks[0].estimated);
    assert_eq!(consent.releases[0].tracks[1].estimated_bytes, None);
    assert!(!consent.releases[0].tracks[1].estimated);
    assert_eq!(consent.releases[0].tracks[0].duration_s, Some(30));
}

#[test]
fn store_download_requires_consent_and_a_fresh_consent_id() {
    let script = Arc::new(Script::new());
    let rig = Rig::new("consent", Arc::clone(&script));
    script.push(one_track_album("a:1", "t:1"));
    let first = rig.prepare(&[("a:1", "t:1")]);

    // No consent: nothing is spawned and the plan is not consumed.
    let err = rig
        .start(Arc::new(UnusedTransport), None, Some(&first.consent_id))
        .expect_err("consent is required");
    assert_eq!(err.code(), "consent_required");
    assert_eq!(err.http_status(), 403);

    // A newer prepare replaces the plan; the old id is stale.
    script.push(one_track_album("a:1", "t:1"));
    let second = rig.prepare(&[("a:1", "t:1")]);
    assert_ne!(first.consent_id, second.consent_id);
    let err = rig
        .start(Arc::new(UnusedTransport), Some(true), Some(&first.consent_id))
        .expect_err("the old id is stale");
    assert_eq!(err.code(), "stale_consent");
    assert_eq!(err.http_status(), 409);

    // The current id starts exactly once.
    script.push(download_unimplemented());
    let started = rig
        .start(Arc::new(UnusedTransport), Some(true), Some(&second.consent_id))
        .expect("start");
    assert!(started.ok);
    assert_eq!(started.items, 1);
    rig.wait_terminal(&started.session);
}

/// A web transport that cannot be reached: these tests only need the start refusal.
struct UnusedTransport;

impl WebTransport for UnusedTransport {
    fn get(&self, _url: &str, _timeout: Duration) -> Result<HopResponse, TransportError> {
        Err(TransportError::Connect("unused in this test".to_string()))
    }
}

#[test]
fn store_download_refuses_when_free_space_is_insufficient() {
    let script = Arc::new(Script::new());
    let rig = Rig::new("free-space", Arc::clone(&script));
    // A second hub with a probe that reports a nearly full disk. The same probe is what
    // each response-header check sees, so prepare and the worker agree.
    let library = rig.library.clone();
    let tuned = Arc::new(
        StoreHub::with_transport(
            Some(rig.temp.path().join("secrets")),
            Arc::clone(&script) as Arc<dyn ProviderTransport + Send + Sync>,
        )
        .with_download_tuning(
            DownloadLimits {
                free_space: tiny_free,
                ..test_limits()
            },
            Duration::ZERO,
        ),
    );
    tuned.set_library_root(Some(library));
    script.push(artists_ok());
    tuned
        .connect("fan@example.com", "s3cret", Some(true))
        .expect("connect");
    script.push(album(
        "a:1",
        "Huge",
        "The Artist",
        2026,
        vec![song(Song {
            id: "t:1",
            title: "One",
            track: 1,
            duration_s: 30,
            size: Some(4 * MIB),
            bitrate: Some(320),
            suffix: "mp3",
        })],
    ));

    let before = rig.snapshot();
    let err = tuned
        .download_prepare(PrepareDownloadRequest {
            releases: vec!["a:1".into()],
            tracks: Vec::new(),
            noubin_key: None,
        })
        .expect_err("the disk is too small");
    assert_eq!(err.code(), "no_space");
    assert_eq!(err.http_status(), 507);
    let message = err.message();
    assert!(message.contains("free space"), "{message}");
    assert_eq!(before, rig.snapshot(), "a refused prepare writes nothing");
    assert!(!rig.tmp().exists());
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

#[test]
fn store_download_rejects_a_cdn_url_outside_the_allowlist() {
    let script = Arc::new(Script::new());
    // The real base URL, so the first URL is https://bandcamp.com and the redirect is
    // checked by the store allowlist rather than the loopback test exemption.
    let rig = Rig::new("cdn-blocked", Arc::clone(&script));
    script.push(one_track_album("a:1", "t:1"));
    script.push(download_unimplemented());
    let consent = rig.prepare(&[("a:1", "t:1")]);

    let web = Arc::new(RedirectingTransport::new(
        "https://cdn.evil.example/track.mp3",
    ));
    let started = rig
        .start(Arc::clone(&web) as Arc<dyn WebTransport>, Some(true), Some(&consent.consent_id))
        .expect("start");
    let status = rig.wait_terminal(&started.session);

    assert_eq!(status.state, DownloadState::Failed);
    assert_eq!(status.items[0].state, ItemState::Failed);
    assert_eq!(status.items[0].error_code.as_deref(), Some("blocked_target"));
    let urls = web.urls();
    assert_eq!(urls.len(), 1, "the CDN is never contacted: {urls:?}");
    assert!(
        urls[0].starts_with("https://bandcamp.com/"),
        "the only request is the store's own URL: {}",
        urls[0]
    );
    assert!(rig.stage_dirs().is_empty(), "nothing is staged");
}

/// Answers the store's first URL with a redirect to a host outside the allowlist.
struct RedirectingTransport {
    to: String,
    urls: Mutex<Vec<String>>,
}

impl RedirectingTransport {
    fn new(to: &str) -> Self {
        Self {
            to: to.to_string(),
            urls: Mutex::new(Vec::new()),
        }
    }

    fn urls(&self) -> Vec<String> {
        self.urls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl WebTransport for RedirectingTransport {
    fn get(&self, url: &str, _timeout: Duration) -> Result<HopResponse, TransportError> {
        self.urls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(url.to_string());
        Ok(HopResponse {
            status: 302,
            content_type: None,
            location: Some(self.to.clone()),
            content_length: Some(0),
            body: Box::new(std::io::Cursor::new(Vec::new())),
        })
    }
}

#[test]
fn store_download_sniffs_audio_and_rejects_html_served_as_mp3() {
    let fx = MediaFx::new(|_| Reply::Bytes {
        content_type: "audio/mpeg",
        body: b"<html><body>not audio</body></html>".to_vec(),
    });
    let script = Arc::new(Script::new());
    let rig = Rig::new("html-audio", Arc::clone(&script));
    script.push(one_track_album("a:1", "t:1"));
    script.push(download_unimplemented());
    let consent = rig.prepare(&[("a:1", "t:1")]);

    let started = rig
        .start(Arc::new(fx), Some(true), Some(&consent.consent_id))
        .expect("start");
    let status = rig.wait_terminal(&started.session);

    assert_eq!(status.state, DownloadState::Failed);
    assert_eq!(status.items[0].state, ItemState::Failed);
    assert_eq!(status.items[0].error_code.as_deref(), Some("wrong_type"));
    assert_eq!(status.placed_files, 0);
    assert!(status.unite.is_none());
    assert!(!status.errors.is_empty());
    assert!(rig.stage_dirs().is_empty(), "no partial file survives");
    assert!(!contains_noudata(&rig.library), "no .noudata is written");
}

#[test]
fn store_download_cancel_deletes_partials_and_writes_nothing() {
    let fx = Arc::new(MediaFx::new(|_| Reply::Slow {
        body: mp3(7),
        chunk: 256,
        delay: Duration::from_millis(40),
    }));
    let script = Arc::new(Script::new());
    let rig = Rig::new("cancel", Arc::clone(&script));
    script.push(one_track_album("a:1", "t:1"));
    script.push(download_unimplemented());
    let consent = rig.prepare(&[("a:1", "t:1")]);

    let before = rig.snapshot();
    let started = rig
        .start(Arc::clone(&fx) as Arc<dyn WebTransport>, Some(true), Some(&consent.consent_id))
        .expect("start");

    // Wait until the worker is actually streaming into the staging folder.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let streaming = rig.stage_dirs().iter().any(|dir| {
            std::fs::read_dir(dir)
                .map(|entries| {
                    entries.filter_map(|e| e.ok()).any(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with(".dl-")
                    })
                })
                .unwrap_or(false)
        });
        if streaming {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the worker never started streaming; hits={:?}",
            fx.hits()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let running = rig.status(&started.session);
    assert_eq!(running.state, DownloadState::Running);

    let cancelled = rig
        .hub
        .download_cancel(&started.session)
        .expect("cancel");
    assert_eq!(cancelled.session, started.session);
    let status = rig.wait_terminal(&started.session);

    assert_eq!(status.state, DownloadState::Cancelled);
    assert!(status.cancelled);
    assert_eq!(status.placed_files, 0);
    assert!(status.unite.is_none());
    assert_eq!(status.items[0].state, ItemState::Cancelled);
    assert!(rig.stage_dirs().is_empty(), "the staging folder is removed");
    assert!(!contains_noudata(&rig.library), "no .noudata is written");
    let after = rig.snapshot();
    assert_only_plippa_state(&before, &after);
}

#[test]
fn store_download_startup_removes_stale_stage_files_without_touching_the_library() {
    let temp = TempDir::new("stale-stage");
    let library = temp.path().join("library");
    let tmp = library.join(cache::STATE_DIR).join("tmp");
    // What a kill -9 during a download leaves: a stage folder with a partial media file.
    let stale = tmp.join("store-deadbeef");
    std::fs::create_dir_all(&stale).expect("stale stage");
    std::fs::write(stale.join(".dl-123"), mp3(1)).expect("partial");
    // A sibling Plippa owner (an import in progress in a previous run) is not ours to delete.
    let import = tmp.join("import-other");
    std::fs::create_dir_all(&import).expect("import stage");
    std::fs::write(import.join("upload.bin"), b"someone else").expect("import file");

    let hub = Arc::new(StoreHub::with_transport(
        Some(temp.path().join("secrets")),
        Arc::new(Script::new()),
    ));
    hub.set_library_root(Some(library.clone()));

    assert!(!stale.exists(), "the crashed download's staging folder is gone");
    assert!(import.exists(), "another owner's staging folder is untouched");
    assert!(
        media_files(&library).is_empty(),
        "a partial download never becomes library media"
    );
    assert!(!contains_noudata(&library), "and never a playable release");
}

#[test]
fn store_download_queue_is_sequential_and_one_at_a_time() {
    let fx = Arc::new(MediaFx::new(|_| Reply::Slow {
        body: mp3(9),
        chunk: 512,
        delay: Duration::from_millis(10),
    }));
    let script = Arc::new(Script::new());
    let rig = Rig::new("sequential", Arc::clone(&script));
    script.push(album(
        "a:1",
        "Two Tracks",
        "The Artist",
        2026,
        vec![
            song(Song {
                id: "t:1",
                title: "One",
                track: 1,
                duration_s: 30,
                size: Some(4_096),
                bitrate: Some(320),
                suffix: "mp3",
            }),
            song(Song {
                id: "t:2",
                title: "Two",
                track: 2,
                duration_s: 31,
                size: Some(4_096),
                bitrate: Some(320),
                suffix: "mp3",
            }),
        ],
    ));
    script.push(download_unimplemented());
    let consent = rig.prepare(&[("a:1", "t:1"), ("a:1", "t:2")]);
    assert_eq!(consent.tracks, 2);

    let started = rig
        .start(Arc::clone(&fx) as Arc<dyn WebTransport>, Some(true), Some(&consent.consent_id))
        .expect("start");

    // While it runs, a second start is refused (429), not queued.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rig.status(&started.session);
        if status.items.iter().any(|item| item.state == ItemState::Running) {
            break;
        }
        assert!(Instant::now() < deadline, "the worker never started: {status:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
    let busy = rig
        .start(Arc::new(UnusedTransport), Some(true), Some(&consent.consent_id))
        .expect_err("a second download is refused");
    assert_eq!(busy.code(), "busy");
    assert_eq!(busy.http_status(), 429);

    let status = rig.wait_terminal(&started.session);
    assert_eq!(status.state, DownloadState::Done, "{status:?}");
    assert!(status.errors.is_empty(), "{:?}", status.errors);
    assert_eq!(
        fx.peak(),
        1,
        "never two media streams at once: {:?}",
        fx.log()
    );
    assert_eq!(fx.hits().len(), 2);
    let first = fx.hits()[0].clone();
    let second = fx.hits()[1].clone();
    assert!(first.contains("id=t%3A1") || first.contains("id=t:1"), "{first}");
    assert!(second.contains("id=t%3A2") || second.contains("id=t:2"), "{second}");

    // The second request starts only after the first body was fully read (and its
    // pacing pause elapsed), not while the first one was still streaming.
    let log = fx.log();
    let first_start = log
        .iter()
        .position(|entry| entry.starts_with("start:") && entry.contains("t%3A1"))
        .unwrap_or_else(|| panic!("no first start: {log:?}"));
    let first_done = log
        .iter()
        .position(|entry| entry.starts_with("done:") && entry.contains("t%3A1"))
        .unwrap_or_else(|| panic!("no first finish: {log:?}"));
    let second_start = log
        .iter()
        .position(|entry| entry.starts_with("start:") && entry.contains("t%3A2"))
        .unwrap_or_else(|| panic!("no second start: {log:?}"));
    assert!(first_start < first_done && first_done < second_start, "{log:?}");

    assert_eq!(status.placed_files, 2, "both files were placed");
    assert_eq!(status.items[0].state, ItemState::Done);
    assert_eq!(status.items[1].state, ItemState::Done);
    // The header totals are what the device screen draws: they must be the sum of the
    // per-track bytes, not the zero they start life as.
    let item_bytes: u64 = status.items.iter().map(|item| item.bytes).sum();
    assert_eq!(status.done_bytes, item_bytes, "{status:?}");
    assert!(status.done_bytes > 0, "no bytes were counted: {status:?}");
    assert!(status.total_bytes >= status.done_bytes, "{status:?}");
    assert!(rig.stage_dirs().is_empty(), "the staging folder is gone");
    assert!(
        contains_noudata(&rig.library),
        "a playable release exists: {:?}",
        rig.snapshot()
    );
}

#[test]
fn store_download_records_lossy_substitution_per_track() {
    // The real shape: the store's `stream` URL redirects to the CDN host on the allowlist.
    let fx = Arc::new(MediaFx::new(|url| {
        if url.starts_with("https://bandcamp.com/") {
            Reply::Redirect("https://t4.bcbits.com/stream/track?token=not-logged".to_string())
        } else {
            Reply::Bytes {
                content_type: "audio/mpeg",
                body: mp3(3),
            }
        }
    }));
    let script = Arc::new(Script::new());
    let rig = Rig::new("lossy", Arc::clone(&script));
    script.push(one_track_album("a:1", "t:1"));
    script.push(download_unimplemented());
    let consent = rig.prepare(&[("a:1", "t:1")]);

    let started = rig
        .start(Arc::clone(&fx) as Arc<dyn WebTransport>, Some(true), Some(&consent.consent_id))
        .expect("start");
    let status = rig.wait_terminal(&started.session);

    assert_eq!(status.state, DownloadState::Done, "{status:?}");
    let item = &status.items[0];
    assert_eq!(item.state, ItemState::Done);
    assert!(!item.original, "the purchased file was not obtained");
    assert!(item.stream_substituted, "the lossy stream was substituted");
    assert_eq!(item.quality.as_deref(), Some("lossy"));
    assert_eq!(item.bitrate_kbps, None);
    assert_eq!(item.format.as_deref(), Some("mp3"));
    assert_eq!(item.bytes, mp3(3).len() as u64);
    assert!(item.error.is_none());
    assert!(
        status.warnings.iter().any(|w| w.contains("lossy stream")),
        "the result says out loud what arrived: {:?}",
        status.warnings
    );
    // The hand-off placed a real, playable release, so BCS_05 has something to refine
    // rather than a stub to replace.
    let handed = status.unite.clone().expect("handed off");
    assert_eq!(handed.state, HandOffState::Placed);
    assert_eq!(handed.entry_point, UNITE_ENTRY_POINT);
    assert_eq!(
        UNITE_ENTRY_POINT,
        "service::stores::post_unite::post_download_unite"
    );
    let rel = handed.noudata.expect("a .noudata was written");
    let noudata = rig.library.join(&rel);
    assert!(noudata.is_file(), "missing {}", noudata.display());
    let media = media_files(&rig.library);
    assert_eq!(media.len(), 1, "{media:?}");
    assert_eq!(media[0].0, mp3(3).len() as u64);
    assert_eq!(media[0].1, "mp3");
    assert!(rig.stage_dirs().is_empty(), "the staging folder is gone");

    // The redirect was followed once, to the allowed CDN host, and nothing token-bearing
    // reached the status payload or the log ring.
    let hits = fx.hits();
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert!(hits[1].starts_with("https://t4.bcbits.com/"), "{hits:?}");
    let json = serde_json::to_string(&status).expect("serialise status");
    assert!(!json.contains("bcbits.com"), "{json}");
    assert!(!json.contains("token="), "{json}");
}

#[test]
fn store_download_lands_in_the_library_index_as_a_playable_release() {
    let script = Arc::new(Script::new());
    let rig = Rig::new("placement-index", Arc::clone(&script));
    script.push(album(
        "a:1",
        "Placed Release",
        "The Artist",
        2026,
        vec![song(Song {
            id: "t:1",
            title: "One",
            track: 1,
            duration_s: 30,
            size: Some(4_096),
            bitrate: Some(320),
            suffix: "mp3",
        })],
    ));
    script.push(download_unimplemented());
    let consent = rig.prepare(&[("a:1", "t:1")]);

    // The service is the real index the portal reads, opened on the same library root.
    let service = Arc::new(Mutex::new(ServerService::start_with_roots(
        vec![rig.library.clone()],
        Box::new(NullIpc),
        ServerService::default_mock_associate(),
    )));
    let placed: Arc<Mutex<Option<crate::PlayableSummary>>> = Arc::new(Mutex::new(None));
    let weak = Arc::downgrade(&service);
    let slot = Arc::clone(&placed);
    rig.hub.set_library_indexer(Arc::new(move |root, noudata| {
        let service = weak
            .upgrade()
            .ok_or_else(|| "the server is shutting down".to_string())?;
        let mut guard = service
            .lock()
            .map_err(|_| "the server lock is poisoned".to_string())?;
        let summary = guard.finish_store_download(root, noudata)?;
        let id = summary.id;
        *slot
            .lock()
            .map_err(|_| "the test slot is poisoned".to_string())? = Some(summary);
        Ok(id)
    }));

    let media = Arc::new(MediaFx::audio(mp3(5)));
    let started = rig
        .start(media, Some(true), Some(&consent.consent_id))
        .expect("start");
    let status = rig.wait_terminal(&started.session);

    assert_eq!(status.state, DownloadState::Done, "{status:?}");
    let handed = status.unite.clone().expect("handed off");
    assert_eq!(handed.state, HandOffState::Placed);
    assert!(handed.indexed, "the index accepted the new release");
    assert!(status.errors.is_empty(), "{:?}", status.errors);

    // What the indexer was given is what the resource browser would list.
    let summary = placed
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .expect("the indexer ran");
    assert_eq!(summary.title.as_deref(), Some("Placed Release"));
    assert_eq!(summary.kind, "album");
    assert_eq!(summary.media_relpaths.len(), 1);
    assert!(
        summary.media_relpaths[0].ends_with(".mp3"),
        "{}",
        summary.media_relpaths[0]
    );

    let listed = service
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .list_playables()
        .expect("list playables");
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].title.as_deref(), Some("Placed Release"));
}

// ---------------------------------------------------------------------------
// BCS_05: a linked Noubin URL on the download itself (the portal's "link" action)
// ---------------------------------------------------------------------------

/// A web transport that reaches nothing: the linked fetch fails without a network.
struct DeadWeb;

impl WebTransport for DeadWeb {
    fn get(&self, _url: &str, _timeout: Duration) -> Result<HopResponse, TransportError> {
        Err(TransportError::Connect("the test network is down".into()))
    }
}

fn dead_web() -> crate::web::WebHub {
    crate::web::WebHub::with_transport(
        crate::web::WebConfig::default(),
        Arc::new(DeadWeb) as Arc<dyn WebTransport>,
        Box::new(|| true),
    )
}

#[test]
fn store_unite_a_link_is_validated_on_the_consent_sheet() {
    let script = Arc::new(Script::new());
    let rig = Rig::new("link-prepare", Arc::clone(&script));
    script.push(album(
        "a:1",
        "Linked Release",
        "The Artist",
        2026,
        vec![song(Song {
            id: "t:1",
            title: "One",
            track: 1,
            duration_s: 30,
            size: Some(4_096),
            bitrate: Some(320),
            suffix: "mp3",
        })],
    ));

    // Without the web hub there is no fetcher, so the link is refused before a sheet.
    let err = rig
        .hub
        .download_prepare(PrepareDownloadRequest {
            releases: vec!["a:1".into()],
            tracks: Vec::new(),
            noubin_key: Some("https://linkhost.example/noubin/album/".into()),
        })
        .expect_err("no fetcher");
    assert!(err.message().contains("online lookup"), "{}", err.message());

    // With one, the sheet names the release to be fetched — still without a request.
    rig.hub.set_web_hub(Arc::new(dead_web()));
    let consent = rig
        .hub
        .download_prepare(PrepareDownloadRequest {
            releases: vec!["a:1".into()],
            tracks: Vec::new(),
            noubin_key: Some("https://linkhost.example/noubin/album/".into()),
        })
        .expect("prepare");
    let linked = consent.noubin.clone().expect("the sheet names the link");
    assert_eq!(linked.host, "linkhost.example");
    assert!(linked.fetch_url.ends_with("/web.noudata"), "{}", linked.fetch_url);
    assert!(
        consent.consent_message.contains("fetch the linked Noubin release"),
        "{}",
        consent.consent_message
    );
    assert!(consent.consent_message.contains("linkhost.example"));

    // A key that is not a URL this server can fetch is refused, not stored.
    let err = rig
        .hub
        .download_prepare(PrepareDownloadRequest {
            releases: vec!["a:1".into()],
            tracks: Vec::new(),
            noubin_key: Some("not a url".into()),
        })
        .expect_err("not a key");
    assert!(err.message().contains("Noubin URL"), "{}", err.message());
}

#[test]
fn store_unite_a_linked_download_falls_back_to_store_metadata_when_the_link_fails() {
    let script = Arc::new(Script::new());
    let rig = Rig::new("link-fallback", Arc::clone(&script));
    script.push(album(
        "a:1",
        "Linked Release",
        "The Artist",
        2026,
        vec![song(Song {
            id: "t:1",
            title: "One",
            track: 1,
            duration_s: 30,
            size: Some(4_096),
            bitrate: Some(320),
            suffix: "mp3",
        })],
    ));
    script.push(download_unimplemented());
    rig.hub.set_web_hub(Arc::new(dead_web()));

    let consent = rig
        .hub
        .download_prepare(PrepareDownloadRequest {
            releases: vec!["a:1".into()],
            tracks: Vec::new(),
            noubin_key: Some("https://linkhost.example/noubin/album/".into()),
        })
        .expect("prepare");
    let started = rig
        .start(Arc::new(MediaFx::audio(mp3(7))), Some(true), Some(&consent.consent_id))
        .expect("start");
    let status = rig.wait_terminal(&started.session);

    // The audio is worth keeping even when the link is not: it is placed, and the person
    // is told in the same breath that the metadata is the store's.
    assert_eq!(status.state, DownloadState::Done, "{status:?}");
    let handed = status.unite.clone().expect("handed off");
    assert_eq!(handed.state, HandOffState::Placed);
    assert_eq!(handed.metadata.as_deref(), Some("store"));
    assert!(handed.can_link, "the item can still be linked");
    assert!(
        status
            .warnings
            .iter()
            .any(|w| w.contains("linked Noubin release could not be used")),
        "{:?}",
        status.warnings
    );
    assert_eq!(media_files(&rig.library).len(), 1, "one copy, no duplicate");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// After a cancel the library is untouched apart from Plippa's own state directory (the
/// staging area it created and then emptied).
fn assert_only_plippa_state(
    before: &BTreeMap<String, (bool, u64)>,
    after: &BTreeMap<String, (bool, u64)>,
) {
    for (key, entry) in before {
        assert_eq!(after.get(key), Some(entry), "an existing entry changed: {key}");
    }
    for (key, entry) in after {
        if before.contains_key(key) {
            continue;
        }
        assert!(key.starts_with(".plippa"), "unexpected new entry: {key}");
        assert!(entry.0, "an unexpected file was left behind: {key}");
    }
}

/// Every regular media file under the library, as `(bytes, extension)`, excluding
/// Plippa's state directory.
fn media_files(root: &Path) -> Vec<(u64, String)> {
    fn walk(dir: &Path, out: &mut Vec<(u64, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ".plippa" || name == ".noudata" {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                walk(&path, out);
            } else if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
                if ext.eq_ignore_ascii_case("mp3") {
                    out.push((meta.len(), ext.to_ascii_lowercase()));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn contains_noudata(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("noudata") {
            return true;
        }
        if path.is_dir() && contains_noudata(&path) {
            return true;
        }
    }
    false
}
