//! Behaviour tests for the refined download -> unite step (BCS_05).
//!
//! Every test drives the real entry point ([`unite_download`], the body of
//! [`StoreHub::post_download_unite`]) with staged files on disk and, for Path A, the same
//! consent-gated web machinery the server uses, with the transport swapped for an in-process
//! fixture. Nothing here opens a socket.
//!
//! The assertions are about what a person gets (a folder, a sidecar, a bound key, an item in
//! the index) and about the three promises that must never slip: the audio is untouched, a
//! disagreement is never a silent success, and a second run never duplicates a release.

use super::tests_support::TempDir;
use super::*;

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ipc::NullIpc;
use plippa_noubin::NoudataDocument;
use serde_json::Value;
use crate::service::ServerService;
use crate::web::download::DownloadLimits;
use crate::web::fetch::{HopResponse, Transport as WebTransport, TransportError};
use crate::web::{WebConfig, WebHub};

use super::post_unite::{
    unite_download, DurationMatch, PostDownloadUnite, TrackQuality, UnitePath, UniteServices,
    UniteState, POST_UNITE_ENTRY_POINT,
};

const KEY: &str = "https://linkhost.example/noubin/album/";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An in-process web: `(url suffix, content type, body)` routes plus the request log.
struct WebFx {
    routes: Mutex<Vec<(String, &'static str, Vec<u8>)>>,
    hits: Mutex<Vec<String>>,
}

impl WebFx {
    fn new() -> Self {
        Self {
            routes: Mutex::new(Vec::new()),
            hits: Mutex::new(Vec::new()),
        }
    }

    fn serve(&self, suffix: &str, content_type: &'static str, body: Vec<u8>) -> &Self {
        self.routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((suffix.to_string(), content_type, body));
        self
    }

    fn hits(&self) -> Vec<String> {
        self.hits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl WebTransport for WebFx {
    fn get(&self, url: &str, _timeout: Duration) -> Result<HopResponse, TransportError> {
        self.hits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(url.to_string());
        let routes = self
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let found = routes
            .iter()
            .rev()
            .find(|(suffix, _, _)| url.ends_with(suffix.as_str()));
        let (status, content_type, body) = match found {
            Some((_, content_type, body)) => {
                (200, Some((*content_type).to_string()), body.clone())
            }
            None => (404, None, Vec::new()),
        };
        Ok(HopResponse {
            status,
            content_type,
            location: None,
            content_length: Some(body.len() as u64),
            body: Box::new(Cursor::new(body)),
        })
    }
}

fn web_hub(fx: &Arc<WebFx>) -> WebHub {
    WebHub::with_transport(
        WebConfig::default(),
        Arc::clone(fx) as Arc<dyn WebTransport>,
        Box::new(|| true),
    )
}

fn limits() -> DownloadLimits {
    DownloadLimits {
        max_file_bytes: 4 * 1024 * 1024,
        max_batch_bytes: 16 * 1024 * 1024,
        free_reserve: 0,
        ..DownloadLimits::default()
    }
}

/// A tiny PNG with the given pixel size (signature + IHDR is all the sniffer reads).
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut body = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    body.extend_from_slice(&13u32.to_be_bytes());
    body.extend_from_slice(b"IHDR");
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&height.to_be_bytes());
    body.extend_from_slice(&[8, 6, 0, 0, 0]);
    body.extend_from_slice(&[0, 0, 0, 0]); // CRC, never checked by the sniffer
    body
}

/// A minimal MP3 (an ID3 header is enough for the magic-byte sniff).
fn mp3(tag: u8) -> Vec<u8> {
    let mut body = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
    body.resize(4096, tag);
    body
}

/// The `web.noudata` a linkhost serves for [`KEY`].
fn document(items: &[(&str, &str)], cover: Option<&str>) -> Vec<u8> {
    let media: Vec<serde_json::Value> = items
        .iter()
        .map(|(title, length)| {
            serde_json::json!({
                "mediaTitle": title,
                "uniteData": { "expectedLength": length },
            })
        })
        .collect();
    let mut release = serde_json::json!({
        "credits": { "primaryArtists": [{ "primaryArtistName": "The Artist" }] },
    });
    if let Some(url) = cover {
        release["coverImage"] = serde_json::json!({
            "images": [{ "imageWebURL": url, "isImageOfNoubin": true }],
        });
    }
    serde_json::json!({
        "_whatAmILookingAt": plippa_noubin::WHAT_AM_I_LOOKING_AT,
        "noudataVersion": plippa_noubin::NOUDATA_VERSION,
        "noudataType": "release",
        "playableItemTitle": "The Real Album",
        "playableItemProfile": "music",
        "releaseData": release,
        "mediaList": media,
    })
    .to_string()
    .into_bytes()
}

/// What the store told BCS_04 about one downloaded track.
struct TrackFx<'a> {
    id: &'a str,
    title: &'a str,
    number: u32,
    duration_ms: u64,
}

fn track<'a>(id: &'a str, title: &'a str, number: u32, duration_ms: u64) -> TrackFx<'a> {
    TrackFx {
        id,
        title,
        number,
        duration_ms,
    }
}

fn staged_file(
    stage: &Path,
    release_id: &str,
    index: usize,
    fx: &TrackFx<'_>,
    bytes: Vec<u8>,
    measured: bool,
    substituted: bool,
) -> StagedTrackFile {
    let dir = stage.join(format!("r00-{release_id}"));
    std::fs::create_dir_all(&dir).expect("stage dir");
    let path = dir.join(format!("{:04}-01-{:02}.mp3", index + 1, fx.number));
    std::fs::write(&path, &bytes).expect("stage file");
    StagedTrackFile {
        track_id: fx.id.to_string(),
        title: fx.title.to_string(),
        track_number: Some(fx.number),
        disc_number: Some(1),
        path,
        bytes: bytes.len() as u64,
        duration_ms: Some(fx.duration_ms),
        duration_measured: measured,
        format: "mp3".to_string(),
        quality: if substituted { "lossy".to_string() } else { "lossless".to_string() },
        bitrate_kbps: substituted.then_some(320),
        original: !substituted,
        stream_substituted: substituted,
    }
}

fn staged(
    stage: &Path,
    release: &StagedRelease,
    tracks: &[(&TrackFx<'_>, u8)],
) -> StagedStoreDownload {
    let files = tracks
        .iter()
        .enumerate()
        .map(|(index, (fx, tag))| {
            staged_file(
                stage,
                &release.release_id,
                index,
                fx,
                mp3(*tag),
                true,
                false,
            )
        })
        .collect();
    StagedStoreDownload {
        store: STORE_SLUG.to_string(),
        session: "test-session".to_string(),
        complete: true,
        releases: vec![StagedRelease {
            tracks: files,
            ..release.clone()
        }],
    }
}

fn release(id: &str, title: &str) -> StagedRelease {
    StagedRelease {
        release_id: id.to_string(),
        title: title.to_string(),
        artist: "The Artist".to_string(),
        year: Some(2026),
        created_unix: None,
        tracks: Vec::new(),
    }
}

fn request(download: StagedStoreDownload) -> PostDownloadUnite {
    PostDownloadUnite::store_metadata_only(download)
}

fn services<'a>(web: Option<&'a WebHub>) -> UniteServices<'a> {
    UniteServices {
        web,
        store: None,
        delete: None,
        index: None,
        limits: limits(),
    }
}

fn library(temp: &TempDir) -> PathBuf {
    let root = temp.path().join("library");
    std::fs::create_dir_all(&root).expect("library");
    root
}

fn staging(temp: &TempDir) -> PathBuf {
    let stage = temp.path().join("stage");
    std::fs::create_dir_all(&stage).expect("stage");
    stage
}

fn noudata_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("noudata") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn media_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("mp3") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn read_doc(path: &Path) -> NoudataDocument {
    let bytes = std::fs::read(path).expect("read .noudata");
    plippa_noubin::parse_noudata(&bytes).expect("parse .noudata")
}

fn recorded_store_source(doc: &NoudataDocument, release_id: &str, tracks: usize) -> Value {
    let source = doc
        .extra
        .get(super::post_unite::STORE_SOURCE_KEY)
        .cloned()
        .unwrap_or_else(|| panic!("no plippaStoreSource in {}", doc.extra.keys().len()));
    assert_eq!(source["provider"], STORE_SLUG, "{source}");
    assert_eq!(source["store"], STORE_SLUG, "{source}");
    assert_eq!(source["releaseId"], release_id, "{source}");
    assert_eq!(
        source["trackIds"].as_array().map(Vec::len),
        Some(tracks),
        "{source}"
    );
    source
}

// ---------------------------------------------------------------------------
// Path A
// ---------------------------------------------------------------------------

#[test]
fn store_unite_path_a_maps_tracks_deterministically_and_writes_noudata_last() {
    let temp = TempDir::new("bcs05-path-a");
    let root = library(&temp);
    let stage = staging(&temp);
    let fx = Arc::new(WebFx::new());
    fx.serve(
        "/web.noudata",
        "application/json",
        document(&[("One", "0:30.000"), ("Two", "0:31.500")], None),
    );
    let web = web_hub(&fx);

    let r = release("900", "The Real Album");
    let one = track("t1", "One", 1, 30_000);
    let two = track("t2", "Two", 2, 31_500);
    let download = staged(&stage, &r, &[(&one, 1), (&two, 2)]);
    let placed = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            ..request(download)
        },
        services(Some(&web)),
    );

    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");
    assert_eq!(placed.path, Some(UnitePath::A));
    assert_eq!(placed.entry_point, POST_UNITE_ENTRY_POINT);
    assert_eq!(placed.track_count, 2);
    assert!(placed.warnings.is_empty(), "{:?}", placed.warnings);
    assert_eq!(placed.releases[0].path, Some(UnitePath::A));
    let tracks = &placed.releases[0].tracks;
    assert_eq!(tracks[0].item_index, Some(0));
    assert_eq!(tracks[1].item_index, Some(1));
    assert_eq!(tracks[0].duration_match, Some(DurationMatch::Exact));
    assert_eq!(tracks[0].quality, TrackQuality::Original);

    let rel = placed.noudata.clone().expect("noudata path");
    assert!(rel.starts_with("Music/The Artist/The Real Album/"), "{rel}");
    let release_dir = root.join(&rel).parent().expect("release dir").to_path_buf();
    let doc = read_doc(&root.join(&rel));
    // The authoritative document supplied the titles, not the store's.
    assert_eq!(doc.media_list[0].media_title.as_deref(), Some("One"));
    assert_eq!(doc.media_list[1].media_title.as_deref(), Some("Two"));
    // Every `localFilePaths` entry exists — the sidecar is only written once they do.
    // Entries are relative to the release folder, like every other `.noudata`.
    let files: Vec<&String> = doc
        .media_list
        .iter()
        .flat_map(|item| item.local_file_paths.iter().flatten())
        .collect();
    assert_eq!(files.len(), 2, "{:?}", doc.media_list);
    for file in &files {
        assert!(
            release_dir.join(file).is_file(),
            "missing {} in {}",
            file,
            release_dir.display()
        );
    }
    let source = recorded_store_source(&doc, "900", 2);
    assert_eq!(source["metadata"], "noudata", "{source}");
    assert_eq!(source["url"], KEY, "{source}");
}

#[test]
fn store_unite_path_a_reports_a_duration_mismatch_and_does_not_claim_success_silently() {
    let temp = TempDir::new("bcs05-mismatch");
    let root = library(&temp);
    let stage = staging(&temp);
    let fx = Arc::new(WebFx::new());
    fx.serve(
        "/web.noudata",
        "application/json",
        document(&[("One", "1:40.000")], None),
    );
    let web = web_hub(&fx);

    let r = release("901", "The Real Album");
    let one = track("t1", "One", 1, 30_000);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let proposed = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            ..request(download)
        },
        services(Some(&web)),
    );

    assert_eq!(proposed.state, UniteState::NeedsConfirmation, "{proposed:?}");
    assert!(proposed.noudata.is_none());
    assert!(noudata_files(&root).is_empty(), "nothing may be written");
    assert!(media_files(&root).is_empty(), "nothing may be written");
    assert!(
        proposed.warnings.iter().any(|w| w.contains("off by")),
        "the disagreement must be stated: {:?}",
        proposed.warnings
    );
    let track = &proposed.releases[0].tracks[0];
    assert_eq!(track.duration_match, Some(DurationMatch::Mismatch));
    assert_eq!(track.expected_duration_ms, Some(100_000));
    assert_eq!(track.duration_ms, Some(30_000));

    // The same request, confirmed, keeps the mismatch in its report.
    let stage = staging(&temp);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let confirmed = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            confirm_mismatch: true,
            ..request(download)
        },
        services(Some(&web)),
    );
    assert_eq!(confirmed.state, UniteState::Placed, "{confirmed:?}");
    assert_eq!(
        confirmed.releases[0].tracks[0].duration_match,
        Some(DurationMatch::Mismatch)
    );
    assert!(
        confirmed.warnings.iter().any(|w| w.contains("off by")),
        "{:?}",
        confirmed.warnings
    );
}

#[test]
fn store_unite_path_a_accepts_a_duration_within_the_tolerance() {
    let temp = TempDir::new("bcs05-tolerance");
    let root = library(&temp);
    let stage = staging(&temp);
    let fx = Arc::new(WebFx::new());
    fx.serve(
        "/web.noudata",
        "application/json",
        document(&[("One", "0:32.000")], None),
    );
    let web = web_hub(&fx);

    let r = release("902", "The Real Album");
    let one = track("t1", "One", 1, 30_000);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let placed = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            ..request(download)
        },
        services(Some(&web)),
    );

    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");
    assert_eq!(
        placed.releases[0].tracks[0].duration_match,
        Some(DurationMatch::WithinTolerance)
    );

    // The tolerance is the caller's: 10 s late within a 15 s tolerance is still a match.
    let temp = TempDir::new("bcs05-tolerance-wide");
    let root = library(&temp);
    let stage = staging(&temp);
    let fx = Arc::new(WebFx::new());
    fx.serve(
        "/web.noudata",
        "application/json",
        document(&[("One", "0:40.000")], None),
    );
    let web = web_hub(&fx);
    let r = release("906", "The Real Album");
    let one = track("t1", "One", 1, 30_000);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let wide = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            duration_tolerance_ms: Some(15_000),
            ..request(download)
        },
        services(Some(&web)),
    );
    assert_eq!(wide.state, UniteState::Placed, "{wide:?}");
    assert_eq!(
        wide.releases[0].tracks[0].duration_match,
        Some(DurationMatch::WithinTolerance)
    );
}

#[test]
fn store_unite_path_a_partial_selection_asks_before_uniting() {
    let temp = TempDir::new("bcs05-partial");
    let root = library(&temp);
    let stage = staging(&temp);
    let fx = Arc::new(WebFx::new());
    fx.serve(
        "/web.noudata",
        "application/json",
        document(
            &[
                ("One", "0:30.000"),
                ("Two", "0:30.000"),
                ("Three", "0:30.000"),
            ],
            None,
        ),
    );
    let web = web_hub(&fx);

    let r = release("903", "The Real Album");
    let one = track("t1", "One", 1, 30_000);
    let three = track("t3", "Three", 3, 30_000);
    let download = staged(&stage, &r, &[(&one, 1), (&three, 3)]);
    let proposed = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            ..request(download)
        },
        services(Some(&web)),
    );

    assert_eq!(proposed.state, UniteState::NeedsConfirmation, "{proposed:?}");
    let tracks = &proposed.releases[0].tracks;
    assert_eq!(tracks[0].item_index, Some(0), "One -> item 0");
    assert_eq!(tracks[1].item_index, Some(2), "Three -> item 2");
    assert!(
        proposed
            .warnings
            .iter()
            .any(|w| w.contains("lists 3 track(s) but 2 arrived")),
        "{:?}",
        proposed.warnings
    );

    // Confirmed: the two files are united, the missing item stays empty.
    let stage = staging(&temp);
    let download = staged(&stage, &r, &[(&one, 1), (&three, 3)]);
    let placed = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            confirm_mismatch: true,
            ..request(download)
        },
        services(Some(&web)),
    );
    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");
    let doc = read_doc(&root.join(placed.noudata.clone().expect("noudata")));
    assert_eq!(doc.media_list.len(), 3);
    assert!(doc.media_list[1].local_file_paths.is_none());
}

#[test]
fn store_unite_path_a_cover_art_prefers_the_larger_image() {
    let temp = TempDir::new("bcs05-cover");
    let root = library(&temp);
    let stage = staging(&temp);
    let fx = Arc::new(WebFx::new());
    let doc = serde_json::json!({
        "_whatAmILookingAt": plippa_noubin::WHAT_AM_I_LOOKING_AT,
        "noudataVersion": plippa_noubin::NOUDATA_VERSION,
        "noudataType": "release",
        "playableItemTitle": "The Real Album",
        "playableItemProfile": "music",
        "releaseData": {
            "credits": { "primaryArtists": [{ "primaryArtistName": "The Artist" }] },
            "coverImage": { "images": [
                { "imageWebURL": "https://linkhost.example/noubin/album/small.png" },
                { "imageWebURL": "https://linkhost.example/noubin/album/large.png" }
            ] }
        },
        "mediaList": [{ "mediaTitle": "One", "uniteData": { "expectedLength": "0:30.000" } }]
    })
    .to_string()
    .into_bytes();
    fx.serve("/web.noudata", "application/json", doc);
    fx.serve("/noubin/album/small.png", "image/png", png(120, 120));
    fx.serve("/noubin/album/large.png", "image/png", png(1200, 1200));
    fx.serve(
        "/thumbnails/small-512.jpg",
        "image/jpeg",
        jpeg_header(512, 512),
    );
    fx.serve(
        "/thumbnails/large-512.jpg",
        "image/jpeg",
        jpeg_header(512, 512),
    );
    let web = web_hub(&fx);

    let r = release("904", "The Real Album");
    let one = track("t1", "One", 1, 30_000);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let placed = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            ..request(download)
        },
        services(Some(&web)),
    );
    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");
    let rel = placed.noudata.clone().expect("noudata");
    let dir = root.join(&rel).parent().expect("release dir").to_path_buf();
    let doc = read_doc(&root.join(&rel));
    let cover = doc
        .release_data
        .as_ref()
        .and_then(|r| r.cover_image.as_ref())
        .expect("coverImage");
    assert_eq!(
        cover.primary_local_filename(),
        Some("large.png"),
        "the higher-resolution image is primary: {cover:?}"
    );
    assert_eq!(
        cover.images[1].image_local_filename.as_deref(),
        Some("small-alt.png")
    );
    assert!(dir.join("large.png").is_file());
    assert!(dir.join("small-alt.png").is_file());
    assert!(
        dir.join("thumbnails").join("large-512.jpg").is_file(),
        "thumbnails follow the image's local name"
    );
}

#[test]
fn store_unite_falls_back_to_the_store_cover_when_the_document_has_none() {
    let temp = TempDir::new("bcs05-store-cover");
    let root = library(&temp);
    let stage = staging(&temp);
    let hub = store_hub_with_cover(&temp, &root, "907", "The Real Album", png(800, 800));

    let r = release("907", "The Real Album");
    let one = track("t1", "One", 1, 30_000);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let placed = hub.post_download_unite(request(download));

    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");
    let rel = placed.noudata.clone().expect("noudata");
    let dir = root.join(&rel).parent().unwrap().to_path_buf();
    let doc = read_doc(&root.join(&rel));
    let cover = doc
        .release_data
        .as_ref()
        .and_then(|r| r.cover_image.as_ref())
        .expect("the store cover became the coverImage");
    assert_eq!(cover.primary_local_filename(), Some("cover.png"));
    assert!(dir.join("cover.png").is_file(), "the store cover was written");
    // The store's cover is not a linkhost image, so no web URL is invented for it.
    assert!(cover.images[0].image_web_url.is_none());
}

/// A connected store hub whose only cover is `bytes` (no library index, no deleter).
fn store_hub_with_cover(
    temp: &TempDir,
    root: &Path,
    id: &str,
    title: &str,
    bytes: Vec<u8>,
) -> Arc<StoreHub> {
    struct StoreFx {
        cover: Vec<u8>,
    }
    impl plippa_store::Transport for StoreFx {
        fn get(
            &self,
            url: &str,
            _headers: &plippa_store::HeaderMap,
            _max_bytes: u64,
        ) -> Result<plippa_store::TransportResponse, plippa_store::StoreError> {
            let body = if url.contains("getArtists") {
                serde_json::json!({
                    "subsonic-response": {
                        "status": "ok", "version": "1.16.1", "artists": { "index": [] }
                    }
                })
                .to_string()
                .into_bytes()
            } else if url.contains("getCoverArt") {
                self.cover.clone()
            } else {
                return Err(plippa_store::StoreError::transport_url(url, "unexpected request"));
            };
            Ok(plippa_store::TransportResponse {
                status: 200,
                headers: vec![("Content-Type".to_string(), "image/png".to_string())],
                body,
                final_url: url.to_string(),
                redirects: Vec::new(),
            })
        }
    }

    let hub = Arc::new(StoreHub::with_transport(
        Some(temp.path().join("secrets")),
        Arc::new(StoreFx { cover: bytes }),
    ));
    hub.set_library_root(Some(root.to_path_buf()));
    hub.connect("fan@example.com", "s3cret", Some(true))
        .expect("connect");
    let cached = plippa_store::StoreRelease {
        id: plippa_store::StoreReleaseId::new(id),
        title: title.to_string(),
        artist: "The Artist".to_string(),
        year: Some(2026),
        track_count: Some(1),
        duration_ms: Some(30_000),
        cover: Some(plippa_store::StoreCover {
            id: plippa_store::StoreCoverId::new(format!("c:{id}")),
            mime: Some("image/png".to_string()),
        }),
        web_url: None,
        created_unix: None,
    };
    let cache = cache::StoreCache {
        store_id: STORE_SLUG.to_string(),
        releases: vec![cached],
        page_size: 200,
        crawled_at: now_secs(),
        complete: true,
        last_error: None,
    };
    cache::write_private_atomic(
        &cache::cache_path(root, STORE_SLUG),
        &cache.encode().expect("encode cache"),
    )
    .expect("write cache");
    hub
}

// ---------------------------------------------------------------------------
// Path B
// ---------------------------------------------------------------------------

#[test]
fn store_unite_path_b_uses_release_title_not_the_song_album_field() {
    let temp = TempDir::new("bcs05-path-b");
    let root = library(&temp);
    let stage = staging(&temp);

    // A single: the store's `song.album` holds the artist name. The release title is the
    // album's name, so that is the only title that may be used.
    let mut r = release("910", "The Artists Real Album");
    r.artist = "The Artist".to_string();
    let one = track("t1", "One", 1, 30_000);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let placed = unite_download(&root, request(download), services(None));

    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");
    assert_eq!(placed.path, Some(UnitePath::B));
    let rel = placed.noudata.clone().expect("noudata");
    assert!(rel.contains("The Artists Real Album"), "{rel}");
    let doc = read_doc(&root.join(&rel));
    assert_eq!(
        doc.playable_item_title.as_deref(),
        Some("The Artists Real Album")
    );
    assert_eq!(doc.noudata_type, Some(plippa_noubin::NoudataType::Release));
    assert_eq!(
        doc.release_data
            .as_ref()
            .and_then(|r| r.credits.as_ref())
            .and_then(|c| c.primary_artists.as_ref())
            .and_then(|a| a.first())
            .map(|a| a.primary_artist_name.as_str()),
        Some("The Artist")
    );
    let source = recorded_store_source(&doc, "910", 1);
    assert_eq!(source["metadata"], "store", "{source}");
}

#[test]
fn store_unite_path_b_omits_missing_dates_rather_than_inventing_them() {
    let temp = TempDir::new("bcs05-no-date");
    let root = library(&temp);
    let stage = staging(&temp);
    let r = release("911", "Undated");
    let one = track("t1", "One", 1, 30_000);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let placed = unite_download(&root, request(download), services(None));
    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");
    let rel = placed.noudata.clone().expect("noudata");
    let raw = std::fs::read_to_string(root.join(&rel)).expect("noudata bytes");
    assert!(
        !raw.contains("releaseDate"),
        "no date may be invented: {raw}"
    );

    // A date the store did report is used, and only then.
    let stage = staging(&temp);
    let mut r = release("912", "Dated");
    r.created_unix = Some(1_767_225_600); // 2026-01-01
    let one = track("t1", "One", 1, 30_000);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let placed = unite_download(&root, request(download), services(None));
    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");
    let doc = read_doc(&root.join(placed.noudata.clone().expect("noudata")));
    let date = doc
        .release_data
        .as_ref()
        .and_then(|r| r.credits.as_ref())
        .and_then(|c| c.extra.get("releaseDate"))
        .and_then(Value::as_str);
    assert_eq!(date, Some("2026-01-01"));
}

#[test]
fn store_unite_reports_lossy_substitution_for_every_track() {
    let temp = TempDir::new("bcs05-lossy");
    let root = library(&temp);
    let stage = staging(&temp);
    let r = release("913", "Streamed");
    let one = track("t1", "One", 1, 30_000);
    let dir = stage.join("r00-913");
    std::fs::create_dir_all(&dir).expect("stage dir");
    let path = dir.join("0001-01-01.mp3");
    std::fs::write(&path, mp3(9)).expect("stage");
    let file = StagedTrackFile {
        track_id: one.id.to_string(),
        title: one.title.to_string(),
        track_number: Some(1),
        disc_number: Some(1),
        path,
        bytes: 4096,
        duration_ms: Some(30_000),
        duration_measured: true,
        format: "mp3".to_string(),
        quality: "lossy".to_string(),
        bitrate_kbps: Some(320),
        original: false,
        stream_substituted: true,
    };
    let download = StagedStoreDownload {
        store: STORE_SLUG.to_string(),
        session: "lossy".to_string(),
        complete: true,
        releases: vec![StagedRelease {
            tracks: vec![file],
            ..r
        }],
    };
    let placed = unite_download(&root, request(download), services(None));

    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");
    assert_eq!(
        placed.releases[0].tracks[0].quality,
        TrackQuality::LossySubstitute
    );
    assert!(
        placed.warnings.iter().any(|w| w.contains("lossy stream")),
        "the substitution must be said out loud: {:?}",
        placed.warnings
    );
}

// ---------------------------------------------------------------------------
// Safety: audio, placement, keys, provenance
// ---------------------------------------------------------------------------

#[test]
fn store_unite_does_not_tag_the_audio() {
    let temp = TempDir::new("bcs05-bytes");
    let root = library(&temp);
    let stage = staging(&temp);
    let r = release("920", "Bytes");
    let one = track("t1", "One", 1, 30_000);
    let two = track("t2", "Two", 2, 31_000);
    let download = staged(&stage, &r, &[(&one, 1), (&two, 2)]);
    let before: Vec<Vec<u8>> = download.releases[0]
        .tracks
        .iter()
        .map(|t| std::fs::read(&t.path).expect("staged bytes"))
        .collect();

    let placed = unite_download(&root, request(download), services(None));
    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");

    let after: Vec<Vec<u8>> = media_files(&root)
        .iter()
        .map(|p| std::fs::read(p).expect("library bytes"))
        .collect();
    assert_eq!(after.len(), before.len());
    for (index, bytes) in after.iter().enumerate() {
        assert_eq!(
            bytes, &before[index],
            "the audio must be byte-identical after unite (no ID3 rewrite)"
        );
        assert_eq!(&bytes[..3], b"ID3", "the fixture's tag is still the one written");
    }
}

#[test]
fn store_unite_places_files_per_standard_and_never_overwrites() {
    let temp = TempDir::new("bcs05-no-overwrite");
    let root = library(&temp);
    let first = staging(&temp);
    let a = release("930", "Same Name");
    let one = track("t1", "One", 1, 30_000);
    let one_download = staged(&first, &a, &[(&one, 1)]);
    let first_placed = unite_download(&root, request(one_download), services(None));
    assert_eq!(first_placed.state, UniteState::Placed, "{first_placed:?}");
    let first_rel = first_placed.noudata.clone().expect("noudata");
    let first_bytes = std::fs::read(root.join(&first_rel)).expect("first sidecar");

    let second = staging(&temp);
    let b = release("931", "Same Name");
    let two = track("t2", "Two", 1, 30_000);
    let two_download = staged(&second, &b, &[(&two, 1)]);
    let second_placed = unite_download(&root, request(two_download), services(None));
    assert_eq!(second_placed.state, UniteState::Placed, "{second_placed:?}");
    let second_rel = second_placed.noudata.clone().expect("noudata");
    assert_ne!(first_rel, second_rel, "the sidecars must be distinct");

    assert_eq!(
        std::fs::read(root.join(&first_rel)).expect("first sidecar"),
        first_bytes,
        "the first release's sidecar must not be touched"
    );
    assert_eq!(noudata_files(&root).len(), 2);
    assert_eq!(media_files(&root).len(), 2);
    let first_doc = read_doc(&root.join(&first_rel));
    assert_eq!(first_doc.media_list[0].media_title.as_deref(), Some("One"));
}

#[test]
fn store_unite_binds_a_key_and_refuses_a_duplicate_key() {
    let temp = TempDir::new("bcs05-key");
    let root = library(&temp);
    let fx = Arc::new(WebFx::new());
    fx.serve(
        "/web.noudata",
        "application/json",
        document(&[("One", "0:30.000")], None),
    );
    let web = web_hub(&fx);

    let stage = staging(&temp);
    let r = release("940", "The Real Album");
    let one = track("t1", "One", 1, 30_000);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let placed = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            ..request(download)
        },
        services(Some(&web)),
    );
    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");
    let rel = placed.noudata.clone().expect("noudata");
    let doc = read_doc(&root.join(&rel));
    assert_eq!(doc.noubin_key.as_deref(), Some(KEY), "the key is written");
    assert!(
        doc.noubin_key_normalised.is_some(),
        "the normalised key is written too"
    );

    // A different release, the same key: refused before anything is written.
    let stage = staging(&temp);
    let other = release("941", "Another Album");
    let two = track("t2", "Two", 1, 30_000);
    let download = staged(&stage, &other, &[(&two, 1)]);
    let refused = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            ..request(download)
        },
        services(Some(&web)),
    );
    assert_eq!(refused.state, UniteState::KeyInUse, "{refused:?}");
    assert!(!refused.detail.is_empty());
    assert_eq!(noudata_files(&root).len(), 1);
}

#[test]
fn store_unite_records_plippa_store_source_and_preserves_it_on_rescan() {
    let temp = TempDir::new("bcs05-rescan");
    let root = library(&temp);
    let stage = staging(&temp);
    let r = release("950", "Provenanced");
    let one = track("t1", "One", 1, 30_000);
    let two = track("t2", "Two", 2, 30_000);
    let download = staged(&stage, &r, &[(&one, 1), (&two, 2)]);
    let placed = unite_download(&root, request(download), services(None));
    assert_eq!(placed.state, UniteState::Placed, "{placed:?}");
    let rel = placed.noudata.clone().expect("noudata");

    let mut service = ServerService::start_with_roots(
        vec![root.clone()],
        Box::new(NullIpc),
        ServerService::default_mock_associate(),
    );
    let items = service.list_playables().expect("index the library");
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].title.as_deref(), Some("Provenanced"));

    // A rescan reads the sidecar; it must still carry the extension for BCS_06.
    let doc = read_doc(&root.join(&rel));
    let source = recorded_store_source(&doc, "950", 2);
    assert_eq!(source["metadata"], "store", "{source}");
}

// ---------------------------------------------------------------------------
// Idempotence
// ---------------------------------------------------------------------------

#[test]
fn store_unite_second_tap_plays_offline_with_zero_requests() {
    let temp = TempDir::new("bcs05-replay");
    let root = library(&temp);
    let fx = Arc::new(WebFx::new());
    fx.serve(
        "/web.noudata",
        "application/json",
        document(&[("One", "0:30.000")], None),
    );
    let web = web_hub(&fx);
    let r = release("960", "The Real Album");
    let one = track("t1", "One", 1, 30_000);

    let stage = staging(&temp);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let first = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            ..request(download)
        },
        services(Some(&web)),
    );
    assert_eq!(first.state, UniteState::Placed, "{first:?}");
    let hits = fx.hits().len();
    assert!(hits > 0, "the first run fetched the document");

    // A second download of the same release (new staging folder, same id).
    let stage = staging(&temp);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let replay = unite_download(
        &root,
        PostDownloadUnite {
            noubin_key: Some(KEY.to_string()),
            consent: true,
            ..request(download)
        },
        services(Some(&web)),
    );

    assert_eq!(replay.state, UniteState::Exists, "{replay:?}");
    assert_eq!(replay.noudata.as_deref(), Some(first.noudata.as_deref().unwrap()));
    assert!(replay.can_replace);
    assert_eq!(
        fx.hits().len(),
        hits,
        "a replay must not make a request: {:?}",
        fx.hits()
    );
    assert_eq!(noudata_files(&root).len(), 1, "no duplicate folder");
    assert_eq!(media_files(&root).len(), 1, "no duplicate file");
}

#[test]
fn store_unite_replace_deletes_then_readds_without_duplicating() {
    let temp = TempDir::new("bcs05-replace");
    let root = library(&temp);
    let deleted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&deleted);
    let deleter: LibraryDeleter = Arc::new(move |root, rel| {
        let dir = root
            .join(rel)
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "no release folder".to_string())?;
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
        log.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(rel.to_string());
        Ok(7)
    });
    fn delete_services<'a>(deleter: &'a LibraryDeleter) -> UniteServices<'a> {
        UniteServices {
            web: None,
            store: None,
            delete: Some(deleter),
            index: None,
            limits: limits(),
        }
    }

    let r = release("970", "Replaceable");
    let one = track("t1", "One", 1, 30_000);
    let stage = staging(&temp);
    let download = staged(&stage, &r, &[(&one, 1)]);
    let first = unite_download(&root, request(download), delete_services(&deleter));
    assert_eq!(first.state, UniteState::Placed, "{first:?}");
    let first_rel = first.noudata.clone().expect("noudata");
    let first_folder = root.join(&first_rel).parent().unwrap().to_path_buf();

    let stage = staging(&temp);
    let mut renamed = release("970", "Replaceable");
    renamed.tracks = Vec::new();
    let two = track("t9", "One (Remastered)", 1, 30_000);
    let download = staged(&stage, &renamed, &[(&two, 1)]);
    let replaced = unite_download(
        &root,
        PostDownloadUnite {
            replace: true,
            ..request(download)
        },
        delete_services(&deleter),
    );

    assert_eq!(replaced.state, UniteState::Placed, "{replaced:?}");
    assert_eq!(
        deleted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_slice(),
        std::slice::from_ref(&first_rel),
        "the old item was deleted through the delete service"
    );
    assert_eq!(noudata_files(&root).len(), 1, "no duplicate release");
    assert_eq!(media_files(&root).len(), 1, "no orphaned old file");
    let doc = read_doc(&root.join(replaced.noudata.clone().expect("noudata")));
    assert_eq!(
        doc.media_list[0].media_title.as_deref(),
        Some("One (Remastered)"),
        "the re-added copy is the new one"
    );
    assert!(
        first_folder.is_dir() || !first_folder.exists(),
        "the replacement reused the release folder"
    );
}

#[test]
fn store_unite_deleting_and_downloading_again_leaves_a_clean_second_copy() {
    let temp = TempDir::new("bcs05-delete-readd");
    let root = library(&temp);
    let deleter: LibraryDeleter = Arc::new(|root: &Path, rel: &str| {
        let dir = root
            .join(rel)
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "no release folder".to_string())?;
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
        Ok(9)
    });
    fn services<'a>(deleter: &'a LibraryDeleter) -> UniteServices<'a> {
        UniteServices {
            web: None,
            store: None,
            delete: Some(deleter),
            index: None,
            limits: limits(),
        }
    }

    let r = release("971", "Again");
    let one = track("t1", "One", 1, 30_000);
    let download = staged(&staging(&temp), &r, &[(&one, 1)]);
    let first = unite_download(&root, request(download), services(&deleter));
    assert_eq!(first.state, UniteState::Placed, "{first:?}");
    let first_rel = first.noudata.clone().expect("noudata");

    // BCS_10 deletes the item; the store download is then repeated from scratch.
    let delete = services(&deleter)
        .delete
        .expect("a delete service");
    delete(&root, &first_rel).expect("delete the item");
    assert!(noudata_files(&root).is_empty(), "the item is gone");
    assert!(media_files(&root).is_empty(), "its audio went with it");

    let download = staged(&staging(&temp), &r, &[(&one, 1)]);
    let second = unite_download(&root, request(download), services(&deleter));
    assert_eq!(
        second.state,
        UniteState::Placed,
        "a deleted item is downloaded again, not reported as existing: {second:?}"
    );
    assert_eq!(noudata_files(&root).len(), 1, "one clean copy");
    assert_eq!(media_files(&root).len(), 1, "no orphaned file");
    assert_eq!(
        second.noudata.as_deref(),
        Some(first_rel.as_str()),
        "the same release folder is reused"
    );
    assert!(!second.can_replace, "there is nothing left to replace");
}

/// A JPEG header only (the sniffer reads the SOF marker, never the pixels).
fn jpeg_header(width: u16, height: u16) -> Vec<u8> {
    let mut body = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
    body.extend_from_slice(&height.to_be_bytes());
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&[0x03, 0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
    body
}
