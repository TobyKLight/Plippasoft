//! NB_09 tests: real sockets against a local `tiny_http` linkhost fixture.
//!
//! The fixture host is reached only through [`Rewrite`], a transport wrapper around the
//! `cfg(test)`-only [`UreqTransport::for_local_fixtures`] escape hatch of NB_08: it maps
//! `https://<host>/<path>` to `http://127.0.0.1:<port>/<host>/<path>`. Everything else is the
//! production code path: production [`FetchPolicy`] (https only, block-list on every URL and
//! redirect hop), consent tokens, caps, sniffing, staging, apply.

use super::hub::*;
use super::refresh::{refresh, refresh_install_images, refresh_prepare, RefreshRequest};
use crate::ipc::RecordingIpc;
use crate::service::ServerService;
use crate::web::download::{DownloadLimits, HARD_MAX_BYTES};
use crate::web::fetch::{
    FetchPolicy, HopResponse, Limits, Transport, TransportError, UreqTransport, WebError,
};
use crate::web::{WebConfig, WebHub};
use plippa_noubin::parse_noudata;
use serde_json::{json, Value};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ----- file builders --------------------------------------------------------------------

fn mp3(n: usize, tag: u8) -> Vec<u8> {
    let mut v = b"ID3\x03\0\0\0\0\0\0".to_vec();
    v.resize(n.max(16), tag);
    v
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 2, 0, 0, 0]);
    v.resize(300, 0);
    v
}

fn jpeg(w: u16, h: u16) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 16];
    v.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    v.extend_from_slice(&[0xFF, 0xC0, 0, 11, 8]);
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&[1, 1, 0x11, 0]);
    v.resize(200, 0);
    v
}

fn webp(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&[0, 0, 0, 0]);
    v.extend_from_slice(b"WEBPVP8X");
    v.extend_from_slice(&[10, 0, 0, 0, 0, 0, 0, 0]);
    v.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
    v.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
    v.resize(200, 0);
    v
}

// ----- fixture linkhost -------------------------------------------------------------------

pub(crate) struct Gate {
    first: Vec<u8>,
    started: Mutex<Sender<()>>,
    release: Mutex<Receiver<()>>,
}

pub(crate) enum Reply {
    Bytes(u16, &'static str, Vec<u8>),
    Redirect(String),
    /// Promise `len` bytes, send `sent`, then stall/close.
    Short {
        len: usize,
        sent: Vec<u8>,
    },
    /// First chunk, then block until released, then more.
    Gated(Arc<Gate>),
}

struct GateBody {
    gate: Arc<Gate>,
    stage: u8,
}

impl Read for GateBody {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.stage {
            0 => {
                self.stage = 1;
                let n = self.gate.first.len().min(buf.len());
                buf[..n].copy_from_slice(&self.gate.first[..n]);
                Ok(n)
            }
            1 => {
                self.stage = 2;
                let _ = self.gate.started.lock().unwrap().send(());
                let _ = self
                    .gate
                    .release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
                let n = 4096.min(buf.len());
                buf[..n].fill(9);
                Ok(n)
            }
            _ => Ok(0),
        }
    }
}

pub(crate) struct Host {
    pub(crate) port: u16,
    server: Arc<tiny_http::Server>,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Drop for Host {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

impl Host {
    pub(crate) fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
    pub(crate) fn count(&self) -> usize {
        self.hits.lock().unwrap().len()
    }
}

pub(crate) fn host(handler: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Host {
    let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
    let port = server.server_addr().to_ip().unwrap().port();
    let hits = Arc::new(Mutex::new(Vec::new()));
    let (srv, h) = (Arc::clone(&server), Arc::clone(&hits));
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for rq in srv.incoming_requests() {
            let url = rq.url().to_string();
            h.lock().unwrap().push(url.clone());
            let handler = Arc::clone(&handler);
            std::thread::spawn(move || match handler(&url) {
                Reply::Bytes(code, ctype, b) => {
                    let _ = rq.respond(
                        tiny_http::Response::from_data(b)
                            .with_status_code(tiny_http::StatusCode(code))
                            .with_header(
                                tiny_http::Header::from_bytes(
                                    &b"Content-Type"[..],
                                    ctype.as_bytes(),
                                )
                                .unwrap(),
                            ),
                    );
                }
                Reply::Redirect(to) => {
                    let _ = rq.respond(
                        tiny_http::Response::from_data(Vec::new())
                            .with_status_code(tiny_http::StatusCode(302))
                            .with_header(
                                tiny_http::Header::from_bytes(&b"Location"[..], to.as_bytes())
                                    .unwrap(),
                            ),
                    );
                }
                Reply::Short { len, sent } => {
                    let _ = rq.respond(tiny_http::Response::new(
                        tiny_http::StatusCode(200),
                        vec![],
                        Cursor::new(sent),
                        Some(len),
                        None,
                    ));
                }
                Reply::Gated(g) => {
                    let _ = rq.respond(tiny_http::Response::new(
                        tiny_http::StatusCode(200),
                        vec![],
                        GateBody { gate: g, stage: 0 },
                        None,
                        None,
                    ));
                }
            });
        }
    });
    Host { port, server, hits }
}

/// `https://<host>/<path>` -> the fixture, via the cfg(test) local transport.
pub(crate) struct Rewrite {
    pub(crate) inner: UreqTransport,
    pub(crate) port: u16,
}

impl Rewrite {
    fn map(&self, url: &str) -> String {
        let rest = url.strip_prefix("https://").unwrap_or(url);
        format!("http://127.0.0.1:{}/{rest}", self.port)
    }
}

impl Transport for Rewrite {
    fn get(&self, url: &str, t: Duration) -> Result<HopResponse, TransportError> {
        self.inner.get(&self.map(url), t)
    }
    fn get_download(&self, url: &str, stall: Duration) -> Result<HopResponse, TransportError> {
        self.inner.get_download(&self.map(url), stall)
    }
}

// ----- environment ----------------------------------------------------------------------

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    svc: Arc<Mutex<ServerService>>,
    unite: Arc<UniteHub>,
    host: Host,
}

const KEY: &str = "https://linkhost.example/noubin/album/";

fn env_with(host: Host, limits: DownloadLimits) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    std::fs::create_dir(&root).unwrap();
    let mut svc = ServerService::start_with_roots(
        vec![root.clone()],
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    let web = Arc::new(WebHub::with_parts(
        WebConfig::default(),
        Arc::new(Rewrite {
            inner: UreqTransport::for_local_fixtures(),
            port: host.port,
        }),
        FetchPolicy::production(),
        Limits::default(),
        Box::new(|| true),
    ));
    svc.set_web_hub(web);
    let unite = Arc::new(UniteHub::with_limits(limits));
    svc.set_unite_hub(Arc::clone(&unite));
    Env {
        _tmp: tmp,
        root: std::fs::canonicalize(root).unwrap(),
        svc: Arc::new(Mutex::new(svc)),
        unite,
        host,
    }
}

fn env(host: Host) -> Env {
    env_with(host, DownloadLimits::default())
}

fn start_req(consent: bool) -> StartRequest {
    StartRequest {
        noubin_key: Some(KEY.into()),
        consent: Some(consent),
        ..Default::default()
    }
}

fn sid(v: &Value) -> String {
    v["session"].as_str().unwrap().to_string()
}

fn all_files(dir: &Path) -> Vec<String> {
    fn walk(d: &Path, base: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, dir, &mut v);
    v.sort();
    v
}

/// Files in the library proper (everything outside `.plippa`).
fn library_files(root: &Path) -> Vec<String> {
    all_files(root)
        .into_iter()
        .filter(|f| !f.starts_with(".plippa"))
        .collect()
}

fn stage_files(root: &Path) -> Vec<String> {
    all_files(&root.join(".plippa/tmp"))
}

fn web_doc(extra_item_url: Option<&str>) -> String {
    let third = extra_item_url.unwrap_or("https://dl.example/files/three.mp3");
    format!(
        r#"{{
        "playableItemTitle": "Free Album",
        "playableItemProfile": "music", "releaseData": {{"credits": {{"primaryArtists": [{{"primaryArtistName": "The Band"}}]}},
            "coverImage": {{"images": [{{"imageWebURL": "https://linkhost.example/noubin/album/cover.png"}}]}}
        }},
        "mediaList": [
            {{"mediaTitle": "One", "uniteData": {{"expectedFilename": "01 One.mp3", "freeDownloadURL": "https://dl.example/files/one.mp3"}}}},
            {{"mediaTitle": "Two", "uniteData": {{"expectedFilename": "02 Two.mp3", "freeDownloadURL": "https://dl.example/files/two.mp3"}}}},
            {{"mediaTitle": "Three", "uniteData": {{"expectedFilename": "03 Three.mp3", "freeDownloadURL": "{third}"}}}}
        ]}}"#
    )
}

/// The standard free-release linkhost: web.noudata, three tracks, cover, some thumbnails.
fn linkhost() -> Host {
    host(|p| match p {
        "/linkhost.example/noubin/album/web.noudata" => {
            Reply::Bytes(200, "application/json", web_doc(None).into_bytes())
        }
        "/dl.example/files/one.mp3" => Reply::Bytes(200, "audio/mpeg", mp3(3000, 1)),
        "/dl.example/files/two.mp3" => Reply::Bytes(200, "application/octet-stream", mp3(4000, 2)),
        "/dl.example/files/three.mp3" => Reply::Bytes(200, "audio/mpeg", mp3(5000, 3)),
        "/linkhost.example/noubin/album/cover.png" => Reply::Bytes(200, "image/png", png(600, 600)),
        "/linkhost.example/noubin/album/thumbnails/cover-128.jpg" => {
            Reply::Bytes(200, "image/jpeg", jpeg(128, 128))
        }
        "/linkhost.example/noubin/album/thumbnails/cover-512.webp" => {
            Reply::Bytes(200, "image/webp", webp(512, 512))
        }
        // Only the capitalised folder has this one (the `/Thumbnails/` fallback).
        "/linkhost.example/noubin/album/Thumbnails/cover-512.jpg" => {
            Reply::Bytes(200, "image/jpeg", jpeg(512, 512))
        }
        // A static site answering a missing file with its HTML page must not become a thumbnail.
        "/linkhost.example/noubin/album/thumbnails/cover-1024.jpg" => {
            Reply::Bytes(200, "text/html", b"<html>not found</html>".to_vec())
        }
        _ => Reply::Bytes(404, "text/plain", b"nope".to_vec()),
    })
}

fn wait_done(e: &Env) {
    e.unite.join_workers();
}

fn dl_state(e: &Env, s: &str) -> String {
    e.unite.status(s).unwrap()["download"]["state"]
        .as_str()
        .unwrap()
        .to_string()
}

// ----- Case A: free release end to end -------------------------------------------------------

#[test]
fn unite_case_a_tap_consent_download_apply_then_replay_offline() {
    let e = env(linkhost());
    // Metadata fetch needs consent: without it, zero sockets.
    let err = e.unite.start(&e.svc, start_req(false)).unwrap_err();
    assert_eq!(err.code(), "consent_required");
    assert_eq!(e.host.count(), 0);

    let started = e.unite.start(&e.svc, start_req(true)).unwrap();
    let s = sid(&started);
    assert_eq!(started["overview"]["title"], "Free Album");
    assert_eq!(started["overview"]["free_download"]["available"], true);
    assert_eq!(
        e.host.hits(),
        ["/linkhost.example/noubin/album/web.noudata"]
    );

    // The download consent lists every URL: 3 media, the cover, 6 thumbnail tiers. Nothing sent.
    let prep = e
        .unite
        .download_prepare(&s, PrepareRequest::default())
        .unwrap();
    let urls: Vec<&str> = prep["urls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["url"].as_str().unwrap())
        .collect();
    assert_eq!(urls.len(), 3 + 1 + 6);
    for u in [
        "https://dl.example/files/one.mp3",
        "https://dl.example/files/two.mp3",
        "https://dl.example/files/three.mp3",
        "https://linkhost.example/noubin/album/cover.png",
        "https://linkhost.example/noubin/album/thumbnails/cover-128.jpg",
        "https://linkhost.example/noubin/album/thumbnails/cover-1024.webp",
    ] {
        assert!(
            urls.contains(&u),
            "{u} missing from the consent list: {urls:?}"
        );
    }
    assert!(
        prep["urls"][0]["size"].is_null(),
        "sizes are unknown before the first response"
    );
    assert_eq!(e.host.count(), 1, "prepare sends nothing");
    let cid = prep["consent_id"].as_str().unwrap().to_string();

    // Consent is server-side and bound to the list that was shown.
    for req in [
        DownloadRequest::default(),
        DownloadRequest {
            consent: Some(false),
            consent_id: Some(cid.clone()),
        },
        DownloadRequest {
            consent: Some(true),
            consent_id: None,
        },
        DownloadRequest {
            consent: Some(true),
            consent_id: Some("0".repeat(32)),
        },
    ] {
        assert_eq!(
            e.unite.download_start(&e.svc, &s, req).unwrap_err().code(),
            "consent_required"
        );
    }
    assert_eq!(e.host.count(), 1, "no download socket without consent");

    e.unite
        .download_start(
            &e.svc,
            &s,
            DownloadRequest {
                consent: Some(true),
                consent_id: Some(cid.clone()),
            },
        )
        .unwrap();
    wait_done(&e);
    assert_eq!(dl_state(&e, &s), "done");
    // One consent = one run: the same id cannot be replayed.
    assert!(e
        .unite
        .download_start(
            &e.svc,
            &s,
            DownloadRequest {
                consent: Some(true),
                consent_id: Some(cid)
            }
        )
        .is_err());

    // Nothing has touched the library yet.
    assert!(library_files(&e.root).is_empty());

    let prop = e.unite.proposal(&s).unwrap();
    assert_eq!(prop["deterministic"], true);
    assert_eq!(prop["matches"].as_array().unwrap().len(), 3);
    let preview = e
        .unite
        .confirm(
            &s,
            ConfirmRequest {
                accept: Some("all".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(preview["folder"], "Music/The Band/Free Album");

    let out = e.unite.apply(&e.svc, &s, ApplyRequest::default()).unwrap();
    assert_eq!(out["ok"], true);
    let noudata = out["noudata"].as_str().unwrap();
    assert_eq!(noudata, "Music/The Band/Free Album/_Free Album.noudata");
    let files = library_files(&e.root);
    for f in [
        "Music/The Band/Free Album/01 One.mp3",
        "Music/The Band/Free Album/02 Two.mp3",
        "Music/The Band/Free Album/03 Three.mp3",
        "Music/The Band/Free Album/_Free Album.noudata",
        "Music/The Band/Free Album/cover.png",
        "Music/The Band/Free Album/thumbnails/cover-128.jpg",
        "Music/The Band/Free Album/thumbnails/cover-512.webp",
        "Music/The Band/Free Album/thumbnails/cover-512.jpg",
    ] {
        assert!(files.contains(&f.to_string()), "{f} not in {files:?}");
    }
    assert!(
        !files.iter().any(|f| f.contains("cover-1024")),
        "an HTML page served for a missing thumbnail was not kept"
    );
    assert_eq!(files.len(), 8);
    // The downloaded bytes are what the server sent.
    assert_eq!(
        std::fs::read(e.root.join("Music/The Band/Free Album/02 Two.mp3")).unwrap(),
        mp3(4000, 2)
    );
    // Staging is gone.
    assert!(
        stage_files(&e.root).is_empty(),
        "{:?}",
        stage_files(&e.root)
    );

    // The document is valid, keyed, points at real files, records where it came from.
    let doc = parse_noudata(&std::fs::read(e.root.join(noudata)).unwrap()).unwrap();
    assert_eq!(doc.noubin_key.as_deref(), Some(KEY));
    assert_eq!(
        doc.noubin_key_normalised.as_deref(),
        Some("linkhost.example/noubin/album/")
    );
    assert_eq!(doc.media_list.len(), 3);
    for (i, name) in ["01 One.mp3", "02 Two.mp3", "03 Three.mp3"]
        .iter()
        .enumerate()
    {
        assert_eq!(
            doc.media_list[i].local_file_paths.as_deref(),
            Some(&[name.to_string()][..])
        );
    }
    let cover = doc
        .release_data
        .as_ref()
        .unwrap()
        .cover_image
        .as_ref()
        .unwrap();
    assert_eq!(
        cover.images[0].image_local_filename.as_deref(),
        Some("cover.png")
    );
    assert_eq!(
        cover.images[0].image_web_url.as_deref(),
        Some("https://linkhost.example/noubin/album/cover.png")
    );
    assert!(doc
        .extra
        .get(SOURCE_URL_KEY)
        .and_then(Value::as_str)
        .unwrap()
        .contains("linkhost.example"));

    // Second tap: the key resolves locally; the web path is closed and nothing is requested.
    let before = e.host.count();
    assert!(e.svc.lock().unwrap().has_local_match(KEY));
    let err = crate::web::fetch_unlocked(&e.svc, KEY, Some(true)).unwrap_err();
    assert_eq!(err, WebError::LocalMatch);
    assert_eq!(
        e.host.count(),
        before,
        "second tap made zero network requests"
    );
    assert_eq!(out["playable"]["title"], "Free Album");
}

// ----- failure modes of the download -------------------------------------------------------

fn run_download(e: &Env, s: &str) {
    let prep = e
        .unite
        .download_prepare(s, PrepareRequest::default())
        .unwrap();
    let cid = prep["consent_id"].as_str().unwrap().to_string();
    e.unite
        .download_start(
            &e.svc,
            s,
            DownloadRequest {
                consent: Some(true),
                consent_id: Some(cid),
            },
        )
        .unwrap();
    wait_done(e);
}

#[test]
fn unite_download_cut_off_midway_leaves_no_partial_media_and_no_noudata() {
    let h = host(|p| match p {
        "/linkhost.example/noubin/album/web.noudata" => {
            Reply::Bytes(200, "application/json", web_doc(None).into_bytes())
        }
        "/dl.example/files/one.mp3" => Reply::Bytes(200, "audio/mpeg", mp3(3000, 1)),
        // Track two: promises 30 kB, delivers 10 kB, then the connection stalls/closes.
        "/dl.example/files/two.mp3" => Reply::Short {
            len: 30_000,
            sent: mp3(10_000, 2),
        },
        _ => Reply::Bytes(404, "text/plain", Vec::new()),
    });
    let e = env_with(
        h,
        DownloadLimits {
            stall_timeout: Duration::from_secs(1),
            ..Default::default()
        },
    );
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    run_download(&e, &s);
    let st = e.unite.status(&s).unwrap();
    assert_eq!(st["download"]["state"], "failed");
    assert!(st["download"]["error"]
        .as_str()
        .unwrap()
        .contains("two.mp3"));
    // Only the fully downloaded first file is staged; no `.part` file, nothing in the library.
    let staged = stage_files(&e.root);
    assert_eq!(staged.len(), 1, "{staged:?}");
    assert!(
        staged[0].ends_with("01 One.mp3") || staged[0].contains("one"),
        "{staged:?}"
    );
    assert!(!staged.iter().any(|f| f.ends_with(".part")));
    assert!(library_files(&e.root).is_empty());
    // Nothing can be applied from an unconfirmed/incomplete session.
    assert!(e.unite.apply(&e.svc, &s, ApplyRequest::default()).is_err());
    assert!(library_files(&e.root).is_empty());
    // Discarding the session removes the staging entirely.
    assert!(e.unite.discard(&s));
    assert!(stage_files(&e.root).is_empty());
}

#[test]
fn unite_download_cancel_deletes_the_partial_and_writes_nothing() {
    let (started_tx, started_rx) = channel();
    let (release_tx, release_rx) = channel();
    let gate = Arc::new(Gate {
        first: mp3(2000, 1),
        started: Mutex::new(started_tx),
        release: Mutex::new(release_rx),
    });
    let g2 = Arc::clone(&gate);
    let h = host(move |p| match p {
        "/linkhost.example/noubin/album/web.noudata" => {
            Reply::Bytes(200, "application/json", web_doc(None).into_bytes())
        }
        "/dl.example/files/one.mp3" => Reply::Gated(Arc::clone(&g2)),
        _ => Reply::Bytes(404, "text/plain", Vec::new()),
    });
    let e = env(h);
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    let prep = e
        .unite
        .download_prepare(&s, PrepareRequest::default())
        .unwrap();
    let cid = prep["consent_id"].as_str().unwrap().to_string();
    e.unite
        .download_start(
            &e.svc,
            &s,
            DownloadRequest {
                consent: Some(true),
                consent_id: Some(cid.clone()),
            },
        )
        .unwrap();
    // A second start while one is running is refused (429), and so is a second consent id.
    let p2 = e
        .unite
        .download_prepare(&s, PrepareRequest::default())
        .unwrap();
    let busy = e
        .unite
        .download_start(
            &e.svc,
            &s,
            DownloadRequest {
                consent: Some(true),
                consent_id: p2["consent_id"].as_str().map(String::from),
            },
        )
        .unwrap_err();
    assert_eq!(busy.http_status(), 429);
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    e.unite.download_cancel(&s).unwrap();
    release_tx.send(()).unwrap();
    wait_done(&e);
    assert_eq!(dl_state(&e, &s), "cancelled");
    assert!(
        stage_files(&e.root).is_empty(),
        "partial media deleted: {:?}",
        stage_files(&e.root)
    );
    assert!(library_files(&e.root).is_empty());
    // Only the metadata fetch and the first media request were ever made.
    assert!(e.host.count() <= 2, "{:?}", e.host.hits());
}

#[test]
fn unite_download_refuses_hostile_urls_in_the_metadata() {
    let doc = r#"{
        "playableItemTitle": "Hostile",
        "playableItemProfile": "music", "releaseData": {"coverImage": {"images": [{"imageWebURL": "https://127.0.0.1/cover.png"}]}},
        "mediaList": [
            {"mediaTitle": "a", "uniteData": {"freeDownloadURL": "https://127.0.0.1/a.mp3"}},
            {"mediaTitle": "b", "uniteData": {"freeDownloadURL": "https://169.254.169.254/latest/meta-data/"}},
            {"mediaTitle": "c", "uniteData": {"freeDownloadURL": "https://192.168.1.1/c.mp3"}},
            {"mediaTitle": "d", "uniteData": {"freeDownloadURL": "https://printer.local/d.mp3"}},
            {"mediaTitle": "e", "uniteData": {"freeDownloadURL": "https://x.example:8443/e.mp3"}},
            {"mediaTitle": "f", "uniteData": {"freeDownloadURL": "file:///etc/passwd"}},
            {"mediaTitle": "g", "uniteData": {"freeDownloadURL": "javascript:alert(1)"}},
            {"mediaTitle": "h", "uniteData": {"freeDownloadURL": "https://user:pw@x.example/h.mp3"}},
            {"mediaTitle": "ok", "uniteData": {"freeDownloadURL": "http://dl.example/ok.mp3"}}
        ]}"#;
    let h = host(move |p| match p {
        "/linkhost.example/noubin/album/web.noudata" => {
            Reply::Bytes(200, "application/json", doc.as_bytes().to_vec())
        }
        "/dl.example/ok.mp3" => Reply::Bytes(200, "audio/mpeg", mp3(2000, 1)),
        _ => Reply::Bytes(404, "text/plain", Vec::new()),
    });
    let e = env(h);
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    let prep = e
        .unite
        .download_prepare(&s, PrepareRequest::default())
        .unwrap();
    let urls: Vec<&str> = prep["urls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["url"].as_str().unwrap())
        .collect();
    // The sanitiser dropped the unusable URLs at fetch time; whatever is left passed the rules:
    // only the plain http one, upgraded to https.
    assert_eq!(urls, ["https://dl.example/ok.mp3"], "{urls:?}");
    let before = e.host.count();
    run_download(&e, &s);
    assert_eq!(
        e.host.hits()[before..],
        ["/dl.example/ok.mp3"],
        "no request to a private/odd target"
    );
}

#[test]
fn unite_download_redirect_into_the_private_network_is_refused_and_nothing_is_kept() {
    let h = host(|p| match p {
        "/linkhost.example/noubin/album/web.noudata" => {
            Reply::Bytes(200, "application/json", web_doc(None).into_bytes())
        }
        "/dl.example/files/one.mp3" => {
            Reply::Redirect("https://169.254.169.254/latest/meta-data/".into())
        }
        _ => Reply::Bytes(404, "text/plain", Vec::new()),
    });
    let e = env(h);
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    run_download(&e, &s);
    let st = e.unite.status(&s).unwrap();
    assert_eq!(st["download"]["state"], "failed");
    assert_eq!(st["download"]["items"][0]["state"], "failed");
    assert!(stage_files(&e.root).is_empty());
    assert!(
        !e.host.hits().iter().any(|u| u.contains("169.254")),
        "the private hop was never requested"
    );
}

#[test]
fn unite_download_html_or_video_served_as_mp3_is_rejected_by_content() {
    let h = host(|p| match p {
        "/linkhost.example/noubin/album/web.noudata" => {
            Reply::Bytes(200, "application/json", web_doc(None).into_bytes())
        }
        "/dl.example/files/one.mp3" => Reply::Bytes(
            200,
            "audio/mpeg",
            b"<!doctype html><html>please log in</html>".to_vec(),
        ),
        _ => Reply::Bytes(404, "text/plain", Vec::new()),
    });
    let e = env(h);
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    run_download(&e, &s);
    let st = e.unite.status(&s).unwrap();
    assert_eq!(st["download"]["state"], "failed");
    assert!(st["download"]["items"][0]["error"]
        .as_str()
        .unwrap()
        .contains("unsupported"));
    assert!(stage_files(&e.root).is_empty());
    assert!(st["files"].as_array().unwrap().is_empty());
}

#[test]
fn unite_download_caps_and_free_space_are_enforced() {
    // Per-file cap below the file size.
    let e = env_with(
        linkhost(),
        DownloadLimits {
            max_file_bytes: 3500,
            ..Default::default()
        },
    );
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    run_download(&e, &s);
    let st = e.unite.status(&s).unwrap();
    assert_eq!(st["download"]["state"], "failed");
    assert!(stage_files(&e.root).iter().all(|f| !f.ends_with(".part")));
    assert_eq!(
        st["files"].as_array().unwrap().len(),
        1,
        "only the file under the cap was kept"
    );
    // Batch cap.
    let e = env_with(
        linkhost(),
        DownloadLimits {
            max_batch_bytes: 5000,
            ..Default::default()
        },
    );
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    run_download(&e, &s);
    assert_eq!(dl_state(&e, &s), "failed");
    // Free space.
    let e = env_with(
        linkhost(),
        DownloadLimits {
            free_space: |_| Some(1000),
            ..Default::default()
        },
    );
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    run_download(&e, &s);
    let st = e.unite.status(&s).unwrap();
    assert_eq!(st["download"]["state"], "failed");
    assert!(
        st["download"]["error"]
            .as_str()
            .unwrap()
            .contains("free space"),
        "{st}"
    );
    assert!(stage_files(&e.root).is_empty());
    let _ = HARD_MAX_BYTES;
}

// ----- Case B: files from elsewhere ------------------------------------------------------

fn offline_host() -> Host {
    host(|p| {
        match p {
        "/linkhost.example/noubin/album/web.noudata" => Reply::Bytes(
            200,
            "application/json",
            br#"{"playableItemTitle": "Bought Album",
                "playableItemProfile": "music", "releaseData": {"credits": {"primaryArtists": [{"primaryArtistName": "Some Artist"}]},
                   "coverImage": {"images": [{"imageWebURL": "https://linkhost.example/noubin/album/cover.png"}]}},
                "mediaList": [
                  {"mediaTitle": "Sunrise", "uniteData": {"expectedFilename": "01 Sunrise.mp3"}},
                  {"mediaTitle": "Midday", "uniteData": {"expectedFilename": "02 Midday.mp3"}},
                  {"mediaTitle": "Sunset", "uniteData": {"expectedFilename": "03 Sunset.mp3"}}]}"#
                .to_vec(),
        ),
        "/linkhost.example/noubin/album/cover.png" => Reply::Bytes(200, "image/png", png(500, 500)),
        _ => Reply::Bytes(404, "text/plain", Vec::new()),
    }
    })
}

fn upload(e: &Env, s: &str, name: &str, bytes: &[u8]) -> Result<Value, UniteError> {
    e.unite.add_upload(
        s,
        name,
        Some(bytes.len() as u64),
        &mut Cursor::new(bytes.to_vec()),
    )
}

#[test]
fn unite_case_b_mangled_names_match_and_a_manual_fix_is_applied() {
    let e = env(offline_host());
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    // Store-style mangled names; contents differ so they cannot be confused.
    for (n, tag) in [
        ("Some_Artist_-_Bought_Album_-_01_-_Sunrise.mp3", 1),
        ("Some_Artist_-_Bought_Album_-_02_-_Midday.mp3", 2),
        ("xyz-final-mix.mp3", 3),
    ] {
        upload(&e, &s, n, &mp3(2500, tag)).unwrap();
    }
    let prop = e.unite.proposal(&s).unwrap();
    assert_eq!(prop["deterministic"], false);
    let matches = prop["matches"].as_array().unwrap();
    let by_item = |i: u64| matches.iter().find(|m| m["item"] == i).cloned();
    assert_eq!(by_item(0).unwrap()["level"], "confident");
    assert_eq!(by_item(1).unwrap()["level"], "confident");
    // The unrecognisable third file is not silently assigned with confidence.
    assert!(by_item(2).is_none_or(|m| m["level"] == "low"), "{prop}");
    // Nothing is applicable until confirmed, and incomplete confirmations are refused.
    assert_eq!(
        e.unite
            .apply(&e.svc, &s, ApplyRequest::default())
            .unwrap_err()
            .code(),
        "not_ready"
    );
    let err = e
        .unite
        .confirm(
            &s,
            ConfirmRequest {
                accept: Some("confident".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(err.code(), "incomplete");
    // Manual fix: the user assigns the leftover file to "Sunset".
    let status = e.unite.status(&s).unwrap();
    let odd = status["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"].as_str().unwrap().starts_with("xyz"))
        .unwrap();
    let odd_id = odd["id"].as_u64().unwrap() as u32;
    let preview = e
        .unite
        .confirm(
            &s,
            ConfirmRequest {
                accept: Some("confident".into()),
                assignments: vec![AssignmentIn {
                    item: 2,
                    files: vec![odd_id],
                }],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(preview["noudata"], "_Bought Album.noudata");
    let out = e
        .unite
        .apply(
            &e.svc,
            &s,
            ApplyRequest {
                key: Some("https://linkhost.example/noubin/album/".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let doc = parse_noudata(&std::fs::read(e.root.join(out["noudata"].as_str().unwrap())).unwrap())
        .unwrap();
    let dir = e.root.join("Music/Some Artist/Bought Album");
    for (i, tag) in [(0, 1u8), (1, 2), (2, 3)] {
        let f = &doc.media_list[i].local_file_paths.as_ref().unwrap()[0];
        assert_eq!(
            std::fs::read(dir.join(f)).unwrap(),
            mp3(2500, tag),
            "item {i} got the right file"
        );
    }
    assert_eq!(doc.noubin_key.as_deref(), Some(KEY));
    assert!(stage_files(&e.root).is_empty());
}

#[test]
fn unite_case_b_manual_assignment_is_validated() {
    let e = env(offline_host());
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    let a = upload(&e, &s, "a.mp3", &mp3(2500, 1)).unwrap()["id"]
        .as_u64()
        .unwrap() as u32;
    upload(&e, &s, "cover.png", &png(100, 100)).unwrap();
    e.unite.proposal(&s).unwrap();
    for bad in [
        ConfirmRequest {
            assignments: vec![AssignmentIn {
                item: 9,
                files: vec![a],
            }],
            ..Default::default()
        },
        ConfirmRequest {
            assignments: vec![AssignmentIn {
                item: 0,
                files: vec![999],
            }],
            ..Default::default()
        },
        ConfirmRequest {
            ignored: vec![999],
            ..Default::default()
        },
        ConfirmRequest {
            accept: Some("everything".into()),
            ..Default::default()
        },
    ] {
        assert!(e.unite.confirm(&s, bad).is_err());
    }
    // A file assigned to two items in the request: the later manual choice wins, never both.
    let ok = e
        .unite
        .confirm(
            &s,
            ConfirmRequest {
                assignments: vec![
                    AssignmentIn {
                        item: 0,
                        files: vec![a],
                    },
                    AssignmentIn {
                        item: 1,
                        files: vec![a],
                    },
                ],
                allow_incomplete: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(ok["files"].as_array().unwrap().len(), 1);
    assert_eq!(ok["files"][0]["item"], 1);
    assert_eq!(ok["missing_items"], json!([0, 2]));
}

#[test]
fn unite_uploads_are_sniffed_capped_and_never_trusted_by_name() {
    let e = env_with(
        offline_host(),
        DownloadLimits {
            max_file_bytes: 10_000,
            max_batch_bytes: 15_000,
            ..Default::default()
        },
    );
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    // HTML named .mp3, script named .flac, video with an audio name, PNG named .mp3.
    for (n, b) in [
        (
            "song.mp3",
            b"<html><script>alert(1)</script></html>".to_vec(),
        ),
        ("evil.flac", b"#!/bin/sh\nrm -rf /\n".to_vec()),
        ("movie.mp3", {
            let mut v = vec![0x1A, 0x45, 0xDF, 0xA3];
            v.resize(100, 0);
            v
        }),
        ("notes.noudata", b"[1,2,3]".to_vec()),
        ("plain.noudata", b"just text".to_vec()),
        ("huge.png", png(6000, 100)),
        ("empty.mp3", Vec::new()),
    ] {
        let err = upload(&e, &s, n, &b).unwrap_err();
        assert!(matches!(err.http_status(), 415 | 400), "{n}: {err}");
    }
    assert!(
        stage_files(&e.root).is_empty(),
        "rejected uploads leave nothing: {:?}",
        stage_files(&e.root)
    );
    let st = e.unite.status(&s).unwrap();
    assert_eq!(st["rejected"].as_array().unwrap().len(), 7);
    // The kind follows the content: an MP3 called `.bin` becomes `.mp3`; a PNG called `.jpg` `.png`.
    let a = upload(&e, &s, "track.bin", &mp3(2000, 1)).unwrap();
    assert_eq!(a["name"], "track.mp3");
    assert_eq!(a["kind"], "audio");
    let b = upload(&e, &s, "art.jpg", &png(100, 100)).unwrap();
    assert_eq!(b["name"], "art.png");
    assert_eq!(b["kind"], "image");
    // Traversal in the client's name only ever supplies a harmless stem.
    let c = upload(&e, &s, "../../../etc/cron.d/x.mp3", &mp3(2000, 2)).unwrap();
    assert_eq!(c["name"], "x.mp3");
    let c = upload(&e, &s, "..\\..\\evil.mp3", &mp3(2000, 3)).unwrap();
    assert_eq!(c["name"], "evil.mp3");
    let c = upload(&e, &s, "CON.mp3", &mp3(2000, 4)).unwrap();
    assert_eq!(c["name"], "_CON.mp3");
    // Content-Length over the cap is refused before reading; so is a lying length while streaming.
    let big = mp3(11_000, 1);
    let err = e
        .unite
        .add_upload(&s, "big.mp3", Some(11_000), &mut Cursor::new(big.clone()))
        .unwrap_err();
    assert_eq!(err.http_status(), 413);
    let err = e
        .unite
        .add_upload(&s, "big.mp3", Some(100), &mut Cursor::new(big.clone()))
        .unwrap_err();
    assert_eq!(err.http_status(), 413);
    let err = e
        .unite
        .add_upload(&s, "big.mp3", None, &mut Cursor::new(big))
        .unwrap_err();
    assert_eq!(err.http_status(), 413);
    // Batch budget (15 000 bytes total, ~10 000 already used): the next big one does not fit.
    let err = upload(&e, &s, "more.mp3", &mp3(9000, 5)).unwrap_err();
    assert_eq!(err.http_status(), 413);
    assert!(stage_files(&e.root).iter().all(|f| !f.ends_with(".part")));
    // All staged files stay inside the staging area.
    for f in stage_files(&e.root) {
        assert!(f.starts_with("unite-"), "{f}");
    }
}

#[test]
fn unite_alt_art_keeps_the_higher_resolution_image_primary() {
    for (upload_px, expect_primary_uploaded) in [(800u32, true), (300, false)] {
        let e = env(offline_host());
        let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
        for (n, t) in [
            ("01 Sunrise.mp3", 1),
            ("02 Midday.mp3", 2),
            ("03 Sunset.mp3", 3),
        ] {
            upload(&e, &s, n, &mp3(2500, t)).unwrap();
        }
        upload(
            &e,
            &s,
            "folder.jpg",
            &jpeg(upload_px as u16, upload_px as u16),
        )
        .unwrap();
        // Also download the web cover (500x500) with its own consent.
        let prep = e
            .unite
            .download_prepare(
                &s,
                PrepareRequest {
                    media: false,
                    images: true,
                },
            )
            .unwrap();
        e.unite
            .download_start(
                &e.svc,
                &s,
                DownloadRequest {
                    consent: Some(true),
                    consent_id: prep["consent_id"].as_str().map(String::from),
                },
            )
            .unwrap();
        wait_done(&e);
        e.unite.proposal(&s).unwrap();
        e.unite
            .confirm(
                &s,
                ConfirmRequest {
                    accept: Some("all".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let out = e.unite.apply(&e.svc, &s, ApplyRequest::default()).unwrap();
        let dir = e.root.join("Music/Some Artist/Bought Album");
        let doc =
            parse_noudata(&std::fs::read(e.root.join(out["noudata"].as_str().unwrap())).unwrap())
                .unwrap();
        let imgs = &doc.release_data.unwrap().cover_image.unwrap().images;
        assert_eq!(imgs.len(), 2, "{imgs:?}");
        let primary = imgs[0].image_local_filename.clone().unwrap();
        let alt = imgs[1].image_local_filename.clone().unwrap();
        assert!(alt.contains("-alt"), "{primary} / {alt}");
        let size = |n: &str| std::fs::read(dir.join(n)).unwrap();
        if expect_primary_uploaded {
            assert!(
                primary.ends_with(".jpg") && alt.ends_with(".png"),
                "{primary} {alt}"
            );
            assert_eq!(size(&primary), jpeg(800, 800));
        } else {
            assert!(
                primary.ends_with(".png") && alt.ends_with(".jpg"),
                "{primary} {alt}"
            );
            assert_eq!(size(&alt), jpeg(300, 300));
        }
        assert!(dir.join(&primary).is_file() && dir.join(&alt).is_file());
    }
}

// ----- Case C -----------------------------------------------------------------------------

const CASE_C_DOC: &str = r#"{
    "_whatAmILookingAt": "a .noudata file, see noubin.org",
    "noubinKey": "OLD-KEY-FROM-THE-SELLER",
    "playableItemTitle": "Library Album",
    "playableItemProfile": "music", "releaseData": {"credits": {"primaryArtists": [{"primaryArtistName": "Lib Artist"}]},
        "coverImage": {"images": [{"imageLocalFilename": "front.png"}, {"imageLocalFilename": "gone.png"}]}},
    "localPlaybackData": {"userNotes": {"userComment": "keep me"}},
    "mediaList": [
        {"mediaTitle": "A", "localFilePaths": ["a.mp3"]},
        {"mediaTitle": "B", "localFilePaths": ["b.mp3", "../../etc/passwd", "/abs/c.mp3", "b.flac"]}
    ]}"#;

#[test]
fn unite_case_c_only_associates_and_never_trusts_paths_in_the_document() {
    let e = env(offline_host());
    let s = sid(&e
        .unite
        .start(
            &e.svc,
            StartRequest {
                associate_only: true,
                noubin_key: Some("https://bought.example/rel/".into()),
                ..Default::default()
            },
        )
        .unwrap());
    assert_eq!(
        e.host.count(),
        0,
        "associate-only never touches the network"
    );
    upload(&e, &s, "_Library Album.noudata", CASE_C_DOC.as_bytes()).unwrap();
    upload(&e, &s, "a.mp3", &mp3(2000, 1)).unwrap();
    upload(&e, &s, "b.mp3", &mp3(2000, 2)).unwrap();
    upload(&e, &s, "front.png", &png(200, 200)).unwrap();
    upload(&e, &s, "thumbnails/front-128.jpg", &jpeg(128, 128)).unwrap();
    let prop = e.unite.proposal(&s).unwrap();
    assert_eq!(prop["case"], "C");
    // Case C needs no proposal/confirm step.
    let out = e.unite.apply(&e.svc, &s, ApplyRequest::default()).unwrap();
    let rel = out["noudata"].as_str().unwrap();
    assert_eq!(rel, "Music/Lib Artist/Library Album/_Library Album.noudata");
    let doc = parse_noudata(&std::fs::read(e.root.join(rel)).unwrap()).unwrap();
    assert_eq!(
        doc.noubin_key.as_deref(),
        Some("https://bought.example/rel/"),
        "tapped key replaces the seller's"
    );
    assert_eq!(
        doc.media_list[0].local_file_paths.as_deref(),
        Some(&["a.mp3".to_string()][..])
    );
    assert_eq!(
        doc.media_list[1].local_file_paths.as_deref(),
        Some(&["b.mp3".to_string()][..]),
        "traversal / absolute / missing files were dropped, the real one kept"
    );
    assert_eq!(
        doc.local_playback_data
            .unwrap()
            .user_notes
            .unwrap()
            .user_comment
            .as_deref(),
        Some("keep me")
    );
    let imgs = doc.release_data.unwrap().cover_image.unwrap().images;
    assert_eq!(imgs.len(), 1, "the image that never arrived was dropped");
    assert_eq!(imgs[0].image_local_filename.as_deref(), Some("front.png"));
    let files = library_files(&e.root);
    assert!(
        files.contains(&"Music/Lib Artist/Library Album/thumbnails/front-128.jpg".to_string()),
        "{files:?}"
    );
    assert!(e
        .svc
        .lock()
        .unwrap()
        .has_local_match("https://bought.example/rel/"));
    assert!(!e.root.join("etc").exists() && !e.root.join("Music/etc").exists());
}

/// NB_VERIFY NB_09: an imported (Case C) `.noudata` is hostile input (NBSecurityRequirements
/// §2.2/§3): every string is sanitised before the file is written into the library. A mutation
/// run showed the existing Case C test never looked at string content.
#[test]
fn unite_case_c_hostile_document_is_sanitised_before_it_is_written() {
    let e = env(offline_host());
    let s = sid(&e
        .unite
        .start(
            &e.svc,
            StartRequest {
                associate_only: true,
                ..Default::default()
            },
        )
        .unwrap());
    let long = "x".repeat(1000);
    let doc = format!(
        r#"{{
        "playableItemTitle": "Library\u202e Album\u200b\u0007",
        "playableItemProfile": "music", "releaseData": {{"description": "line1\u202e\nline2\u0000",
            "credits": {{"primaryArtists": [{{"primaryArtistName": "Lib Artist"}}]}},
            "releasePlatforms": [
                {{"platformType": "store", "platformURL": "javascript:alert(1)"}},
                {{"platformType": "stream", "platformURL": "https://ok.example/x\u202e"}}]}},
        "x-unknown": {{"someURL": "data:text/html,<script>", "note": "n\u202eote"}},
        "mediaList": [{{"mediaTitle": "A\u202e{long}", "localFilePaths": ["a.mp3"]}}]}}"#
    );
    upload(&e, &s, "_Library Album.noudata", doc.as_bytes()).unwrap();
    upload(&e, &s, "a.mp3", &mp3(2000, 1)).unwrap();
    let out = e.unite.apply(&e.svc, &s, ApplyRequest::default()).unwrap();
    let rel = out["noudata"].as_str().unwrap();
    assert_eq!(rel, "Music/Lib Artist/Library Album/_Library Album.noudata");
    let raw = std::fs::read_to_string(e.root.join(rel)).unwrap();
    for bad in ['\u{202E}', '\u{200B}', '\u{7}', '\0'] {
        assert!(!raw.contains(bad), "U+{:04X} was written to disk", bad as u32);
    }
    assert!(!raw.contains("javascript:") && !raw.contains("data:text"), "{raw}");
    assert!(!raw.contains(&"x".repeat(300)), "over-long title was written");
    let d = parse_noudata(raw.as_bytes()).unwrap();
    assert_eq!(d.playable_item_title.as_deref(), Some("Library Album"));
    assert!(d.media_list[0].media_title.as_deref().unwrap().starts_with("Ax"));
    assert!(d.media_list[0].media_title.as_deref().unwrap().chars().count() <= 256);
}

#[test]
fn unite_case_c_without_any_matching_file_or_release_data_is_refused() {
    let e = env(offline_host());
    let s = sid(&e
        .unite
        .start(
            &e.svc,
            StartRequest {
                associate_only: true,
                ..Default::default()
            },
        )
        .unwrap());
    upload(&e, &s, "_Library Album.noudata", CASE_C_DOC.as_bytes()).unwrap();
    upload(&e, &s, "unrelated.mp3", &mp3(2000, 1)).unwrap();
    assert!(e.unite.apply(&e.svc, &s, ApplyRequest::default()).is_err());
    assert!(library_files(&e.root).is_empty());
    let s = sid(&e
        .unite
        .start(
            &e.svc,
            StartRequest {
                associate_only: true,
                ..Default::default()
            },
        )
        .unwrap());
    upload(
        &e,
        &s,
        "_Playlist.noudata",
        br#"{"playableItemTitle":"P","mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    )
    .unwrap();
    upload(&e, &s, "a.mp3", &mp3(2000, 1)).unwrap();
    assert!(e.unite.apply(&e.svc, &s, ApplyRequest::default()).is_err());
    assert!(library_files(&e.root).is_empty());
}

// ----- apply safety ----------------------------------------------------------------------

#[test]
fn unite_apply_refuses_a_symlinked_library_folder_and_writes_nothing_outside() {
    let e = env(offline_host());
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), e.root.join("Music")).unwrap();
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    for (n, t) in [
        ("01 Sunrise.mp3", 1),
        ("02 Midday.mp3", 2),
        ("03 Sunset.mp3", 3),
    ] {
        upload(&e, &s, n, &mp3(2500, t)).unwrap();
    }
    e.unite.proposal(&s).unwrap();
    e.unite
        .confirm(
            &s,
            ConfirmRequest {
                accept: Some("all".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(e.unite.apply(&e.svc, &s, ApplyRequest::default()).is_err());
    assert!(
        all_files(outside.path()).is_empty(),
        "nothing was written through the symlink"
    );
    // The staged files are still there for a retry, and no `.noudata` exists anywhere.
    assert_eq!(stage_files(&e.root).len(), 3);
}

#[test]
fn unite_apply_never_overwrites_and_numbers_colliding_names() {
    let e = env(offline_host());
    let dir = e.root.join("Music/Some Artist/Bought Album");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("01 Sunrise.mp3"), b"PRECIOUS").unwrap();
    std::fs::write(dir.join("_Bought Album.noudata"), b"{}").unwrap();
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    for (n, t) in [
        ("01 Sunrise.mp3", 1),
        ("02 Midday.mp3", 2),
        ("03 Sunset.mp3", 3),
    ] {
        upload(&e, &s, n, &mp3(2500, t)).unwrap();
    }
    e.unite.proposal(&s).unwrap();
    e.unite
        .confirm(
            &s,
            ConfirmRequest {
                accept: Some("all".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let out = e.unite.apply(&e.svc, &s, ApplyRequest::default()).unwrap();
    assert_eq!(
        std::fs::read(dir.join("01 Sunrise.mp3")).unwrap(),
        b"PRECIOUS"
    );
    assert_eq!(
        std::fs::read(dir.join("_Bought Album.noudata")).unwrap(),
        b"{}"
    );
    assert_eq!(
        out["noudata"],
        "Music/Some Artist/Bought Album/_Bought Album (2).noudata"
    );
    assert!(dir.join("01 Sunrise (2).mp3").is_file());
}

#[test]
fn unite_write_release_rolls_back_and_leaves_no_noudata_on_failure() {
    use super::fsops::LibraryFs;
    let tmp = tempfile::tempdir().unwrap();
    let fs = LibraryFs::new(tmp.path()).unwrap();
    let stage = fs.tmp_dir().unwrap().join("unite-x");
    std::fs::create_dir(&stage).unwrap();
    let a = stage.join("a.mp3");
    let b = stage.join("b.mp3");
    std::fs::write(&a, b"A").unwrap();
    std::fs::write(&b, b"B").unwrap();
    let doc = parse_noudata(br#"{"playableItemTitle":"R","playableItemProfile": "music", "releaseData":{},"mediaList":[{"localFilePaths":["a.mp3"]},{"localFilePaths":["b.mp3"]}]}"#).unwrap();
    let built = Built {
        doc,
        noudata: PathBuf::from("Music/X/R/_R.noudata"),
        media: vec![
            Move {
                src: a.clone(),
                dst: PathBuf::from("Music/X/R/a.mp3"),
            },
            // The second move fails (its parent folder does not exist): everything is undone.
            Move {
                src: b.clone(),
                dst: PathBuf::from("Music/X/R/missing-dir/b.mp3"),
            },
        ],
        images: vec![],
    };
    assert!(write_release(&fs, &stage, built).is_err());
    assert!(a.exists() && b.exists(), "moved files went back to staging");
    assert!(
        library_files(tmp.path()).is_empty(),
        "{:?}",
        library_files(tmp.path())
    );
    assert!(
        !tmp.path().join("Music").exists(),
        "created folders were removed"
    );
}

#[test]
fn unite_write_release_refuses_a_noudata_that_would_point_at_a_missing_file() {
    use super::fsops::LibraryFs;
    let tmp = tempfile::tempdir().unwrap();
    let fs = LibraryFs::new(tmp.path()).unwrap();
    let stage = fs.tmp_dir().unwrap().join("unite-y");
    std::fs::create_dir(&stage).unwrap();
    let a = stage.join("a.mp3");
    std::fs::write(&a, b"A").unwrap();
    // The document names `ghost.mp3`, which is not among the moves.
    let doc = parse_noudata(br#"{"playableItemTitle":"R","playableItemProfile": "music", "releaseData":{},"mediaList":[{"localFilePaths":["a.mp3"]},{"localFilePaths":["ghost.mp3"]}]}"#).unwrap();
    let built = Built {
        doc,
        noudata: PathBuf::from("Music/X/R/_R.noudata"),
        media: vec![Move {
            src: a.clone(),
            dst: PathBuf::from("Music/X/R/a.mp3"),
        }],
        images: vec![],
    };
    assert!(write_release(&fs, &stage, built).is_err());
    assert!(a.exists());
    assert!(library_files(tmp.path()).is_empty());
}

#[test]
fn unite_hostile_metadata_stays_inside_the_library() {
    let doc = r#"{
        "playableItemTitle": "../../../etc/\u202Epasswd\u0000",
        "playableItemProfile": "music", "releaseData": {"credits": {"primaryArtists": [{"primaryArtistName": "..\\..\\Windows\\System32"}]}},
        "mediaList": [
            {"mediaTitle": "'; DROP TABLE playables;--", "uniteData": {"expectedFilename": "../../evil.mp3", "freeDownloadURL": "https://dl.example/files/one.mp3"}}
        ]}"#;
    let h = host(move |p| match p {
        "/linkhost.example/noubin/album/web.noudata" => {
            Reply::Bytes(200, "application/json", doc.as_bytes().to_vec())
        }
        "/dl.example/files/one.mp3" => Reply::Bytes(200, "audio/mpeg", mp3(2000, 1)),
        _ => Reply::Bytes(404, "text/plain", Vec::new()),
    });
    let e = env(h);
    let s = sid(&e.unite.start(&e.svc, start_req(true)).unwrap());
    run_download(&e, &s);
    e.unite.proposal(&s).unwrap();
    e.unite
        .confirm(
            &s,
            ConfirmRequest {
                accept: Some("all".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let out = e.unite.apply(&e.svc, &s, ApplyRequest::default()).unwrap();
    let rel = out["noudata"].as_str().unwrap();
    assert!(rel.starts_with("Music/"), "{rel}");
    assert!(
        rel.split('/').all(|p| p != ".." && !p.starts_with('.')),
        "{rel}"
    );
    for f in library_files(&e.root) {
        assert!(
            !f.split('/').any(|p| p == ".." || p.starts_with('.')),
            "{f}"
        );
    }
    assert!(e.root.join(rel).is_file());
    assert!(!e.root.parent().unwrap().join("evil.mp3").exists());
    // The library index survived the SQL metacharacters in the titles.
    assert!(e.svc.lock().unwrap().list_playables().unwrap().len() == 1);
    let text = std::fs::read_to_string(e.root.join(rel)).unwrap();
    assert!(!text.contains('\u{202E}'));
}

// ----- sessions ---------------------------------------------------------------------------

#[test]
fn unite_sessions_are_bounded_and_staging_is_cleaned() {
    // SECFIX_01 (SRV-04): the sessions are capped per client and in total, and a full house of
    // live sessions is *refused* instead of evicting the oldest, whoever it belongs to.
    let e = env(offline_host());
    let a: std::net::IpAddr = "192.168.1.10".parse().unwrap();
    let b: std::net::IpAddr = "192.168.1.11".parse().unwrap();
    let c: std::net::IpAddr = "192.168.1.12".parse().unwrap();
    let begin = |client| {
        e.unite.start_for(
            client,
            &e.svc,
            StartRequest {
                associate_only: true,
                ..Default::default()
            },
        )
    };
    let mut ids = Vec::new();
    for client in [a, a, b, b] {
        ids.push(sid(&begin(client).unwrap()));
    }
    assert_eq!(e.unite.session_count(), MAX_SESSIONS);
    // One client over its own cap, another client with the total full: both refused (429).
    assert_eq!(begin(a).unwrap_err().code(), "too_many");
    let refused = begin(c).unwrap_err();
    assert_eq!((refused.code(), refused.http_status()), ("too_many", 429));
    // Nobody was evicted.
    assert_eq!(e.unite.session_count(), MAX_SESSIONS);
    for id in &ids {
        assert!(e.unite.status(id).is_ok(), "a live session must survive");
    }
    let dirs = std::fs::read_dir(e.root.join(".plippa/tmp"))
        .unwrap()
        .count();
    assert_eq!(dirs, MAX_SESSIONS, "refused sessions leave no staging");
    // Discarding frees the slot and deletes the staging.
    assert!(e.unite.discard(&ids[0]));
    // A crashed run's leftovers are swept on the next start.
    std::fs::create_dir_all(e.root.join(".plippa/tmp/unite-deadbeef/f1")).unwrap();
    std::fs::write(e.root.join(".plippa/tmp/unite-deadbeef/f1/x.mp3"), b"x").unwrap();
    ids.push(sid(&begin(c).unwrap()));
    assert!(!e.root.join(".plippa/tmp/unite-deadbeef").exists());
    for i in &ids {
        e.unite.discard(i);
    }
    assert!(e.unite.status("not-a-session").is_err());
}

#[test]
fn unite_start_needs_a_url_token_or_associate_only() {
    let e = env(offline_host());
    assert_eq!(
        e.unite
            .start(&e.svc, StartRequest::default())
            .unwrap_err()
            .code(),
        "invalid"
    );
    let err = e
        .unite
        .start(
            &e.svc,
            StartRequest {
                noubin_key: Some("LOCAL-KEY-NOT-A-URL".into()),
                consent: Some(true),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(err.code(), "not_a_url");
    let err = e
        .unite
        .start(
            &e.svc,
            StartRequest {
                noubin_key: Some("https://192.168.0.9/x/".into()),
                consent: Some(true),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(matches!(err.http_status(), 400 | 502), "{err}");
    assert_eq!(e.host.count(), 0);
    let err = e
        .unite
        .start(
            &e.svc,
            StartRequest {
                token: Some("zz".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(err.code(), "unknown_session");
}

// ----- refresh ---------------------------------------------------------------------------

#[test]
fn unite_refresh_updates_metadata_needs_consent_and_keeps_user_data() {
    let refreshed = r#"{"playableItemTitle": "Free Album (Deluxe)",
        "playableItemProfile": "music", "releaseData": {"credits": {"primaryArtists": [{"primaryArtistName": "The Band"}]},
            "coverImage": {"images": [{"imageWebURL": "https://linkhost.example/noubin/album/cover.png"}]},
            "releasePlatforms": [{"platformName": "Shop", "platformType": "store", "platformURL": "https://shop.example/a"}]},
        "mediaList": [
            {"mediaTitle": "One (2026 Remaster)", "uniteData": {"expectedFilename": "01 One.mp3"}},
            {"mediaTitle": "Two"}, {"mediaTitle": "Three"}]}"#;
    let h = host(move |p| match p {
        "/linkhost.example/noubin/album/web.noudata" => {
            Reply::Bytes(200, "application/json", refreshed.as_bytes().to_vec())
        }
        "/linkhost.example/noubin/album/cover.png" => Reply::Bytes(200, "image/png", png(400, 400)),
        _ => Reply::Bytes(404, "text/plain", Vec::new()),
    });
    let e = env(h);
    // An existing item with user data and a URL key.
    let dir = e.root.join("Music/The Band/Free Album");
    std::fs::create_dir_all(&dir).unwrap();
    for n in ["01 One.mp3", "02 Two.mp3", "03 Three.mp3"] {
        std::fs::write(dir.join(n), mp3(2000, 1)).unwrap();
    }
    std::fs::write(
        dir.join("_Free Album.noudata"),
        format!(
            r#"{{"noubinKey": "{KEY}", "noubinKeyNormalised": "linkhost.example/noubin/album/",
            "playableItemTitle": "Free Album",
            "playableItemProfile": "music", "releaseData": {{"credits": {{"primaryArtists": [{{"primaryArtistName": "The Band"}}]}}}},
            "localPlaybackData": {{"resumeAtItem": 2, "shuffle": true, "repeatList": true, "stopAfterEach": true, "userNotes": {{"userComment": "mine", "userRating": 5, "userTags": ["x"]}}}},
            "mediaList": [
              {{"mediaTitle": "One", "localFilePaths": ["01 One.mp3"], "itemLocalPlaybackData": {{"resumeFromTime": "1:00"}}}},
              {{"mediaTitle": "Two", "localFilePaths": ["02 Two.mp3"]}},
              {{"mediaTitle": "Three", "localFilePaths": ["03 Three.mp3"]}}]}}"#
        ),
    )
    .unwrap();
    let id = {
        let mut g = e.svc.lock().unwrap();
        g.full_scan().unwrap();
        g.list_playables().unwrap()[0].id
    };
    let before = std::fs::read_to_string(dir.join("_Free Album.noudata")).unwrap();
    // Prepare shows the URL; nothing sent.
    let prep = refresh_prepare(&e.svc, id, &RefreshRequest::default()).unwrap();
    assert!(prep["fetch_url"]
        .as_str()
        .unwrap()
        .ends_with("/noubin/album/web.noudata"));
    assert_eq!(prep["source"], "noubinKey");
    assert_eq!(e.host.count(), 0);
    // No consent: refused before anything.
    let err = refresh(&e.unite, &e.svc, id, RefreshRequest::default()).unwrap_err();
    assert_eq!(err.code(), "consent_required");
    assert_eq!(e.host.count(), 0);
    assert_eq!(
        std::fs::read_to_string(dir.join("_Free Album.noudata")).unwrap(),
        before
    );
    // With consent.
    let out = refresh(
        &e.unite,
        &e.svc,
        id,
        RefreshRequest {
            consent: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(out["changed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == "title"));
    let written = std::fs::read_to_string(dir.join("_Free Album.noudata")).unwrap();
    // Noubin v0.4.1: retired player settings from the old file do not survive a refresh.
    for retired in ["shuffle", "repeatList", "stopAfterEach"] {
        assert!(
            !written.contains(retired),
            "{retired} survived the refresh: {written}"
        );
    }
    let doc = parse_noudata(&std::fs::read(dir.join("_Free Album.noudata")).unwrap()).unwrap();
    assert_eq!(
        doc.playable_item_title.as_deref(),
        Some("Free Album (Deluxe)")
    );
    assert_eq!(
        doc.media_list[0].media_title.as_deref(),
        Some("One (2026 Remaster)")
    );
    let lp = doc.local_playback_data.unwrap();
    assert_eq!(lp.resume_at_item, Some(2));
    let n = lp.user_notes.unwrap();
    assert_eq!(
        (n.user_comment.as_deref(), n.user_rating),
        (Some("mine"), Some(5))
    );
    assert_eq!(
        doc.media_list[0].local_file_paths.as_deref(),
        Some(&["01 One.mp3".to_string()][..])
    );
    assert!(doc.media_list[0].item_local_playback_data.is_some());
    assert_eq!(doc.noubin_key.as_deref(), Some(KEY));
    // Images: a separate session and a separate consent.
    let sess = out["images"]["session"].as_str().unwrap().to_string();
    let hits = e.host.count();
    let prep = e
        .unite
        .download_prepare(
            &sess,
            PrepareRequest {
                media: false,
                images: true,
            },
        )
        .unwrap();
    assert_eq!(e.host.count(), hits);
    // A refresh session cannot be applied as a new release.
    assert_eq!(
        e.unite
            .apply(&e.svc, &sess, ApplyRequest::default())
            .unwrap_err()
            .code(),
        "not_ready"
    );
    e.unite
        .download_start(
            &e.svc,
            &sess,
            DownloadRequest {
                consent: Some(true),
                consent_id: prep["consent_id"].as_str().map(String::from),
            },
        )
        .unwrap();
    wait_done(&e);
    let imgs = refresh_install_images(&e.unite, &e.svc, id, &sess).unwrap();
    assert_eq!(imgs["installed"], 1);
    assert!(dir.join("cover.png").is_file());
    let doc = parse_noudata(&std::fs::read(dir.join("_Free Album.noudata")).unwrap()).unwrap();
    assert_eq!(
        doc.release_data.unwrap().cover_image.unwrap().images[0]
            .image_local_filename
            .as_deref(),
        Some("cover.png")
    );
    assert!(stage_files(&e.root).is_empty());
}

#[test]
fn unite_refresh_without_a_url_key_asks_for_one_and_records_it() {
    let refreshed = r#"{"playableItemTitle": "Renamed", "playableItemProfile": "music", "releaseData": {}, "mediaList": [{"mediaTitle": "One"}]}"#;
    let h = host(move |p| match p {
        "/linkhost.example/noubin/album/web.noudata" => {
            Reply::Bytes(200, "application/json", refreshed.as_bytes().to_vec())
        }
        _ => Reply::Bytes(404, "text/plain", Vec::new()),
    });
    let e = env(h);
    let dir = e.root.join("Music/A/B");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("one.mp3"), mp3(2000, 1)).unwrap();
    std::fs::write(
        dir.join("_B.noudata"),
        r#"{"noubinKey": "PRIVATE-LOCAL-KEY", "playableItemTitle": "B",
            "playableItemProfile": "music", "releaseData": {},
            "mediaList": [{"mediaTitle": "One", "localFilePaths": ["one.mp3"]}]}"#,
    )
    .unwrap();
    let id = {
        let mut g = e.svc.lock().unwrap();
        g.full_scan().unwrap();
        g.list_playables().unwrap()[0].id
    };
    let err = refresh_prepare(&e.svc, id, &RefreshRequest::default()).unwrap_err();
    assert_eq!(err.code(), "needs_url");
    let err = refresh(
        &e.unite,
        &e.svc,
        id,
        RefreshRequest {
            consent: Some(true),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "needs_url");
    assert_eq!(e.host.count(), 0);
    // A hostile URL supplied by the user is refused before any socket.
    let err = refresh(
        &e.unite,
        &e.svc,
        id,
        RefreshRequest {
            consent: Some(true),
            url: Some("https://127.0.0.1/x/".into()),
        },
    )
    .unwrap_err();
    assert!(matches!(err.http_status(), 400 | 502), "{err}");
    assert_eq!(e.host.count(), 0);
    let out = refresh(
        &e.unite,
        &e.svc,
        id,
        RefreshRequest {
            consent: Some(true),
            url: Some(KEY.into()),
        },
    )
    .unwrap();
    assert_eq!(out["source_recorded"], true);
    let doc = parse_noudata(&std::fs::read(dir.join("_B.noudata")).unwrap()).unwrap();
    assert_eq!(doc.playable_item_title.as_deref(), Some("Renamed"));
    assert_eq!(
        doc.noubin_key.as_deref(),
        Some("PRIVATE-LOCAL-KEY"),
        "the key is never replaced"
    );
    assert!(doc.extra.get(SOURCE_URL_KEY).is_some());
    // Next time no URL is needed.
    assert_eq!(
        refresh_prepare(&e.svc, id, &RefreshRequest::default()).unwrap()["source"],
        "recorded"
    );
}

// ----- HTTP layer ---------------------------------------------------------------------------

use std::io::Write;
use std::net::{TcpListener, TcpStream};

struct Resp {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_default()
    }
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }
}

fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut req =
        format!("{method} {path} HTTP/1.1\r\nConnection: close\r\nHost: 127.0.0.1:{port}\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    s.write_all(req.as_bytes()).unwrap();
    let _ = s.write_all(body);
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    Resp {
        status: head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        head,
        body: raw[split + 4..].to_vec(),
    }
}

const JSON: (&str, &str) = ("Content-Type", "application/json");
const XP: (&str, &str) = ("X-Plippa", "1");

fn portal(e: &Env) -> u16 {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cfg = crate::config::ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        library_root: None,
        secrets_dir: None,
        portal_url: None,
        control_bind: None,
        ..Default::default()
    };
    let svc = Arc::clone(&e.svc);
    // The portal switched on with no PIN: these tests are about the unite flows, not the gate.
    let access = Arc::new(crate::portal_access::PortalAccess::memory(
        Arc::new(crate::portal_access::ManualClock::new()),
        crate::portal_access::LockoutPolicy::default(),
    ));
    access.set_enabled(true).unwrap();
    access.set_pin_required(false).unwrap();
    // These flows are fetched over plain `TcpStream` (see `http` below), so the portal is asked
    // for plain HTTP: HTTPS is the device default and fails closed without a secrets directory.
    access.set_https_enabled(false).unwrap();
    std::thread::spawn(move || {
        let _ = crate::http::serve_with_access(cfg, svc, None, crate::limits::HttpLimits::default(), access);
    });
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    port
}

#[test]
fn unite_http_case_b_flow_guards_and_headers() {
    let e = env(offline_host());
    let port = portal(&e);
    let start_body = format!(r#"{{"noubinKey": "{KEY}", "consent": true}}"#);
    // CSRF guards: JSON content type required; foreign Origin refused; no consent, no socket.
    let r = http(port, "POST", "/api/unite/start", &[], start_body.as_bytes());
    assert_eq!(r.status, 415);
    let r = http(
        port,
        "POST",
        "/api/unite/start",
        &[JSON, ("Origin", "http://evil.example")],
        start_body.as_bytes(),
    );
    assert_eq!(r.status, 403);
    let r = http(
        port,
        "POST",
        "/api/unite/start",
        &[JSON],
        format!(r#"{{"noubinKey": "{KEY}"}}"#).as_bytes(),
    );
    assert_eq!(
        (r.status, r.json()["code"].as_str().unwrap_or("")),
        (403, "consent_required")
    );
    assert_eq!(e.host.count(), 0);
    let r = http(
        port,
        "POST",
        "/api/unite/start",
        &[JSON],
        &vec![b' '; 100_000],
    );
    assert_eq!(r.status, 413);
    let r = http(port, "POST", "/api/unite/start", &[JSON], b"{not json");
    assert_eq!(r.status, 400);

    let r = http(
        port,
        "POST",
        "/api/unite/start",
        &[JSON],
        start_body.as_bytes(),
    );
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    // Security headers ride on the unite responses too.
    assert_eq!(
        r.header("X-Content-Type-Options").as_deref(),
        Some("nosniff")
    );
    assert!(r
        .header("Content-Security-Policy")
        .unwrap()
        .contains("default-src 'self'"));
    let s = r.json()["session"].as_str().unwrap().to_string();
    assert_eq!(r.json()["overview"]["title"], "Bought Album");

    // Uploads: X-Plippa (or JSON) required, content decides.
    let up = |name: &str, hdrs: &[(&str, &str)], body: &[u8]| {
        http(
            port,
            "POST",
            &format!("/api/unite/{s}/files?name={name}"),
            hdrs,
            body,
        )
    };
    assert_eq!(
        up("a.mp3", &[], &mp3(2500, 1)).status,
        415,
        "a cross-site form post cannot upload"
    );
    let r = up("a.mp3", &[XP], b"<html>not audio</html>");
    assert_eq!(
        (r.status, r.json()["code"].as_str().unwrap_or("")),
        (415, "unsupported_file")
    );
    for (n, t) in [
        ("01 Sunrise.mp3", 1),
        ("02 Midday.mp3", 2),
        ("03 Sunset.mp3", 3),
    ] {
        let r = up(
            &n.replace(' ', "%20"),
            &[XP, ("Content-Type", "application/octet-stream")],
            &mp3(2500, t),
        );
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        assert_eq!(r.json()["name"], n);
    }
    let r = http(port, "GET", &format!("/api/unite/{s}"), &[], b"");
    assert_eq!(r.json()["files"].as_array().unwrap().len(), 3);
    assert_eq!(r.json()["rejected"].as_array().unwrap().len(), 1);

    let r = http(port, "GET", &format!("/api/unite/{s}/proposal"), &[], b"");
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["complete"], true);
    // Apply before confirm is a 409, not a write.
    let r = http(
        port,
        "POST",
        &format!("/api/unite/{s}/apply"),
        &[JSON],
        b"{}",
    );
    assert_eq!(r.status, 409);
    let r = http(
        port,
        "POST",
        &format!("/api/unite/{s}/confirm"),
        &[JSON],
        br#"{"accept": "all"}"#,
    );
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let r = http(
        port,
        "POST",
        &format!("/api/unite/{s}/apply"),
        &[JSON],
        b"{}",
    );
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(
        r.json()["noudata"],
        "Music/Some Artist/Bought Album/_Bought Album.noudata"
    );
    assert!(e
        .root
        .join("Music/Some Artist/Bought Album/01 Sunrise.mp3")
        .is_file());

    // Bad tokens and unknown sessions are 404 JSON, never a panic; DELETE forgets a session.
    assert_eq!(http(port, "GET", "/api/unite/nope", &[], b"").status, 404);
    assert_eq!(
        http(
            port,
            "GET",
            &format!("/api/unite/{}", "0".repeat(32)),
            &[],
            b""
        )
        .status,
        404
    );
    assert_eq!(
        http(port, "DELETE", &format!("/api/unite/{s}"), &[XP], b"").status,
        200
    );
    assert_eq!(
        http(port, "DELETE", &format!("/api/unite/{s}"), &[XP], b"").status,
        404
    );
    assert_eq!(
        http(port, "DELETE", &format!("/api/unite/{s}"), &[], b"").status,
        415
    );
}

#[test]
fn unite_http_refresh_routes_need_json_and_consent() {
    let e = env(offline_host());
    let port = portal(&e);
    let r = http(port, "POST", "/api/playables/1/refresh", &[], b"{}");
    assert_eq!(r.status, 415);
    let r = http(
        port,
        "POST",
        "/api/playables/1/refresh",
        &[JSON],
        br#"{"consent": true}"#,
    );
    assert_eq!(r.status, 404, "unknown item");
    let r = http(port, "POST", "/api/playables/abc/refresh", &[JSON], b"{}");
    assert_eq!(r.status, 400);
    assert_eq!(e.host.count(), 0);
}

// ----- NB_08 gaps closed here: cover sniffing, portal/tag/filename ingest ----------------------

fn album_with_cover(root: &Path, cover_name: &str, cover_bytes: &[u8]) {
    let dir = root.join("Music/A/Alb");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("t.mp3"), mp3(2000, 1)).unwrap();
    std::fs::write(dir.join(cover_name), cover_bytes).unwrap();
    std::fs::write(
        dir.join("_Alb.noudata"),
        format!(
            r#"{{"playableItemTitle":"Alb","playableItemProfile": "music", "releaseData":{{"coverImage":{{"images":[{{"imageLocalFilename":"{cover_name}"}}]}}}},
            "mediaList":[{{"mediaTitle":"T","localFilePaths":["t.mp3"]}}]}}"#
        ),
    )
    .unwrap();
}

#[test]
fn unite_cover_endpoint_serves_only_sniffed_image_types() {
    let cases: Vec<(&str, Vec<u8>, Option<&str>)> = vec![
        (
            "art.jpg",
            b"<html><script>alert(1)</script></html>".to_vec(),
            None,
        ),
        ("art.png", b"#!/bin/sh\necho hi".to_vec(), None),
        ("art.jpg", png(10, 10), Some("image/png")),
        // A name that is not .jpg/.jpeg/.png/.webp is not a cover image at all (ART_01 resolver).
        ("art.txt", jpeg(10, 10), None),
        ("art.jpg", webp(10, 10), Some("image/webp")),
        ("art.png", b"GIF89a....".to_vec(), None),
    ];
    for (name, bytes, want) in cases {
        let e = env(offline_host());
        album_with_cover(&e.root, name, &bytes);
        let mut g = e.svc.lock().unwrap();
        g.full_scan().unwrap();
        let id = g.list_playables().unwrap()[0].id;
        let got = g.collection_cover(id);
        match want {
            Some(ct) => assert_eq!(got.unwrap().1, ct, "{name}"),
            None => assert!(got.is_err(), "{name} must not be served"),
        }
    }
    // A cover that is a symlink to a file outside the library is not followed.
    let e = env(offline_host());
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), png(10, 10)).unwrap();
    album_with_cover(&e.root, "real.png", &png(10, 10));
    let dir = e.root.join("Music/A/Alb");
    std::fs::remove_file(dir.join("real.png")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.join("real.png")).unwrap();
    let mut g = e.svc.lock().unwrap();
    g.full_scan().unwrap();
    let id = g.list_playables().unwrap()[0].id;
    assert!(g.collection_cover(id).is_err());
}

#[test]
fn unite_portal_inputs_and_tags_are_sanitised_before_they_are_stored() {
    let e = env(offline_host());
    // Quick import: hostile file name, hostile tags cannot escape or hide the folder.
    let mut g = e.svc.lock().unwrap();
    let out = g
        .add_media(
            plippa_noubin::ImportKind::Music,
            "../../.plippa/..\\x\u{202E}.mp3",
            &mp3(2000, 1),
        )
        .unwrap();
    let rel = out.noudata_relpath.clone();
    assert!(rel.starts_with("Music/"), "{rel}");
    assert!(
        !rel.split('/').any(|p| p == ".." || p.starts_with('.')),
        "{rel}"
    );
    assert!(!rel.contains('\u{202E}'));
    let id = out.id;
    // Title patches: control/bidi stripped, empty refused.
    let d = g
        .patch_metadata(
            id,
            crate::service::MetadataPatch {
                playable_item_title: Some("A\u{202E}B\u{0007}C  D".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(d.summary.title.as_deref(), Some("ABC D"));
    assert!(g
        .patch_metadata(
            id,
            crate::service::MetadataPatch {
                playable_item_title: Some("\u{200B}\u{0007}".into()),
                ..Default::default()
            }
        )
        .is_err());
    // A symlinked media folder refuses the quick import instead of writing through it.
    drop(g);
    let outside = tempfile::tempdir().unwrap();
    std::fs::remove_dir_all(e.root.join("Music")).unwrap();
    std::os::unix::fs::symlink(outside.path(), e.root.join("Music")).unwrap();
    let r =
        e.svc
            .lock()
            .unwrap()
            .add_media(plippa_noubin::ImportKind::Music, "z.mp3", &mp3(2000, 1));
    assert!(r.is_err());
    assert!(all_files(outside.path()).is_empty());
}

#[test]
fn unite_http_older_endpoints_have_body_caps() {
    let e = env(offline_host());
    let port = portal(&e);
    // PATCH: 64 KiB; playlist create: 1 MiB (both 413, never a panic or an unbounded read).
    let r = http(
        port,
        "PATCH",
        "/api/playables/1",
        &[JSON],
        &vec![b' '; 70_000],
    );
    assert_eq!(r.status, 413);
    let r = http(
        port,
        "POST",
        "/api/playlists",
        &[JSON],
        &vec![b' '; 1_100_000],
    );
    assert_eq!(r.status, 413);
    let r = http(port, "POST", "/api/playlists", &[JSON], b"{broken");
    assert_eq!(r.status, 400);
}

/// NB_VERIFY NB_09: sniffing decides the *kind*, but an audiobook keeps its `.m4b` extension.
#[test]
fn unite_staged_name_keeps_m4b_and_otherwise_uses_the_sniffed_extension() {
    assert_eq!(staged_name("My Book.M4B", "m4a"), "My Book.m4b");
    assert_eq!(staged_name("My Book.m4a", "m4a"), "My Book.m4a");
    assert_eq!(staged_name("x.m4b", "mp3"), "x.mp3", "an m4b name never overrides other content");
    assert_eq!(staged_name("evil.html", "mp3"), "evil.html.mp3");
    assert_eq!(staged_name("../../etc/pass.m4b", "m4a"), "pass.m4b");
}

// ----- SECFIX_01: session policy, long uploads, shared staging ---------------------------------

/// A body that arrives in small pieces with a pause before each one.
struct Trickle {
    data: Cursor<Vec<u8>>,
    chunk: usize,
    pause: Duration,
}

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::thread::sleep(self.pause);
        let n = buf.len().min(self.chunk);
        self.data.read(&mut buf[..n])
    }
}

/// An [`Env`] whose unite hub has this session policy and staging budget.
fn env_with_hub(
    limits: DownloadLimits,
    ttl: Duration,
    sessions: crate::admission::SessionLimits,
    staging: Arc<crate::web::download::StagingBudget>,
) -> Env {
    let mut e = env_with(offline_host(), limits.clone());
    let hub = Arc::new(
        UniteHub::with_limits_and_staging(limits, staging).with_session_policy(ttl, sessions),
    );
    e.svc.lock().unwrap().set_unite_hub(Arc::clone(&hub));
    e.unite = hub;
    e
}

fn start_for(e: &Env, client: std::net::IpAddr) -> Result<Value, UniteError> {
    e.unite.start_for(
        client,
        &e.svc,
        StartRequest {
            associate_only: true,
            ..Default::default()
        },
    )
}

#[test]
fn new_unite_sessions_do_not_evict_other_clients_live_sessions() {
    use crate::admission::SessionLimits;
    let a: std::net::IpAddr = "192.168.1.10".parse().unwrap();
    let b: std::net::IpAddr = "192.168.1.11".parse().unwrap();
    let c: std::net::IpAddr = "192.168.1.12".parse().unwrap();
    let e = env_with_hub(
        DownloadLimits::default(),
        Duration::from_secs(30 * 60),
        SessionLimits {
            live_window: Duration::from_millis(300),
            ..SessionLimits::new(MAX_SESSIONS)
        },
        Arc::new(crate::web::download::StagingBudget::production()),
    );
    let a1 = sid(&start_for(&e, a).unwrap());
    let a2 = sid(&start_for(&e, a).unwrap());
    let b1 = sid(&start_for(&e, b).unwrap());
    let b2 = sid(&start_for(&e, b).unwrap());
    // Every slot is live: a newcomer is refused and nothing is evicted.
    assert_eq!(start_for(&e, c).unwrap_err().code(), "too_many");
    assert_eq!(e.unite.session_count(), 4);
    // Once a session has been idle past the live window it may be replaced: A's two sit idle
    // while B keeps using its own; C takes A's longest-idle slot and B is untouched.
    std::thread::sleep(Duration::from_millis(400));
    e.unite.status(&b1).unwrap();
    e.unite.status(&b2).unwrap();
    let c1 = sid(&start_for(&e, c).unwrap());
    assert_eq!(e.unite.session_count(), 4);
    assert_eq!(e.unite.status(&a1).unwrap_err().code(), "unknown_session");
    for live in [&a2, &b1, &b2, &c1] {
        assert!(e.unite.status(live).is_ok());
    }
}

#[test]
fn an_upload_longer_than_the_session_ttl_still_finishes() {
    use crate::admission::SessionLimits;
    let ttl = Duration::from_millis(600);
    let e = env_with_hub(
        DownloadLimits::default(),
        ttl,
        SessionLimits::new(MAX_SESSIONS),
        Arc::new(crate::web::download::StagingBudget::production()),
    );
    let s = sid(&start_for(&e, "127.0.0.1".parse().unwrap()).unwrap());
    let body = mp3(3000, 7);
    let started = std::time::Instant::now();
    let unite = Arc::clone(&e.unite);
    let (sess, len) = (s.clone(), body.len());
    let up = std::thread::spawn(move || {
        unite.add_upload(
            &sess,
            "slow.mp3",
            Some(len as u64),
            &mut Trickle {
                data: Cursor::new(body),
                chunk: 150,
                pause: Duration::from_millis(100),
            },
        )
    });
    // Other traffic keeps pruning expired sessions while the upload streams.
    while !up.is_finished() {
        let _ = e.unite.status("00000000000000000000000000000000");
        std::thread::sleep(Duration::from_millis(30));
    }
    let r = up.join().unwrap();
    assert!(r.is_ok(), "{r:?}");
    assert!(
        started.elapsed() > ttl * 2,
        "the upload must outlive the session TTL for this test to mean anything"
    );
    // The session is still there and knows its file: `finish` would not say "unknown session".
    let st = e.unite.status(&s).expect("session survived its own upload");
    assert_eq!(st["files"].as_array().unwrap().len(), 1);
}

#[test]
fn staged_bytes_across_sessions_cannot_exceed_the_shared_cap() {
    use crate::admission::SessionLimits;
    use crate::web::download::StagingBudget;
    let staging = Arc::new(StagingBudget::new(5_000));
    let e = env_with_hub(
        DownloadLimits::default(),
        Duration::from_secs(30 * 60),
        SessionLimits::new(MAX_SESSIONS),
        Arc::clone(&staging),
    );
    let s1 = sid(&start_for(&e, "10.0.0.1".parse().unwrap()).unwrap());
    let s2 = sid(&start_for(&e, "10.0.0.2".parse().unwrap()).unwrap());
    upload(&e, &s1, "a.mp3", &mp3(3000, 1)).unwrap();
    assert_eq!(staging.used(), 3000);
    // The second session has its own (huge) batch budget, but the shared cap says no: declared
    // length at admission, and streamed length when the header lies or is absent.
    let err = upload(&e, &s2, "b.mp3", &mp3(3000, 2)).unwrap_err();
    assert_eq!(err.code(), "no_space", "{err}");
    let err = e
        .unite
        .add_upload(&s2, "c.mp3", None, &mut Cursor::new(mp3(3000, 3)))
        .unwrap_err();
    assert_eq!(err.code(), "no_space", "{err}");
    assert_eq!(staging.used(), 3000, "a refused upload leaves nothing reserved");
    // What fits still goes in, and ending a session gives its bytes back.
    upload(&e, &s2, "d.mp3", &mp3(1500, 4)).unwrap();
    assert_eq!(staging.used(), 4500);
    assert!(e.unite.discard(&s1));
    drop(e.unite.status(&s2));
    assert_eq!(staging.used(), 1500, "a discarded session releases its share");
    upload(&e, &s2, "e.mp3", &mp3(3000, 5)).unwrap();
}
