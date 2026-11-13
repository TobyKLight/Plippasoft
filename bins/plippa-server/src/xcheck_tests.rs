//! Cross-check of Noubin linkhost output (standard v0.4.0): association journey and image checks.
//!
//! The 11 `web.noudata` files are a committed copy of generated linkhost fixtures
//! (`crates/plippa-noubin/tests/fixtures/linkhost-crosscheck/`, see its `PROVENANCE.md`).
//! The "linkhost" here is a local `tiny_http` fixture server that serves exactly the files
//! listed in each case's `publishedFiles`, under the host and path the linkhost uses for the URL
//! shape the case was published under. It is reached only through the `cfg(test)` escape hatch
//! (`Rewrite` + `UreqTransport::for_local_fixtures`) with the **production** `FetchPolicy`: no
//! real network, no env var, no request parameter.
//!
//! The journey per case and tapped form (long URL / short URL):
//! tag tap (real NDEF URI record through a real mock engine) -> `UnmatchedNoubin` -> server
//! banner -> consent-gated prepare/fetch -> overview -> download / upload -> proposal ->
//! confirm -> apply (`build_local_doc`) -> `_<release>.noudata` checks -> library index kind ->
//! second tap plays with zero network requests.

use crate::ipc::RecordingIpc;
use crate::service::ServerService;
use crate::unite::hub::*;
use crate::unite::images::plan_cover_targets;
use crate::unite::tests::{host, Host, Reply, Rewrite};
use crate::web::download::DownloadLimits;
use crate::web::fetch::{FetchPolicy, Limits, UreqTransport};
use crate::web::{fetch_unlocked, prepare_unlocked, WebConfig, WebHub};
use plippa_core::{Capabilities, TransportState};
use plippa_engine::{Engine, EngineEvent};
use plippa_ipc_proto::{IpcMessage, TapNdefPayload};
use plippa_noubin::{
    encode_uri_record, normalise_noubin_key, normalise_noubin_url, parse_noudata_with_warnings,
    LibraryKind, LibraryReader, LibraryRoot, LookupResult, NoudataType, PlayableItemProfile,
};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ----- fixtures ---------------------------------------------------------------------------

struct Fx {
    id: String,
    bytes: Vec<u8>,
    web: Value,
    meta: Value,
}

impl Fx {
    fn s(&self, k: &str) -> String {
        self.meta[k].as_str().unwrap().to_string()
    }
    fn published(&self) -> Vec<String> {
        self.meta["publishedFiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }
    fn long(&self) -> String {
        self.s("longUrl")
    }
    fn short(&self) -> String {
        self.s("shortUrl")
    }
    /// The URL form the linkhost actually published under.
    fn published_is_long(&self) -> bool {
        self.s("publishedUrlShape") == "long"
    }
    fn tapped(&self, long: bool) -> String {
        if long {
            self.long()
        } else {
            self.short()
        }
    }
}

fn fixtures() -> Vec<Fx> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/plippa-noubin/tests/fixtures/linkhost-crosscheck");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    manifest["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| {
            let id = id.as_str().unwrap().to_string();
            let bytes = std::fs::read(dir.join(&id).join("web.noudata")).unwrap();
            Fx {
                web: serde_json::from_slice(&bytes).unwrap(),
                meta: serde_json::from_slice(
                    &std::fs::read(dir.join(&id).join("case.json")).unwrap(),
                )
                .unwrap(),
                bytes,
                id,
            }
        })
        .collect()
}

/// What the test user does with each case, and what must come out.
struct Plan {
    id: &'static str,
    /// Index kind (by profile) the release must be shown as.
    kind: LibraryKind,
    profile: PlayableItemProfile,
    /// Library-relative release folder.
    folder: &'static str,
    /// `.noudata` file name in it.
    noudata: &'static str,
    /// Files the user already owns: (file name as downloaded, declared seconds).
    uploads: &'static [(&'static str, u64)],
    /// `mediaList` positions fetched from their `freeDownloadURL`.
    downloads: &'static [usize],
}

const PLANS: &[Plan] = &[
    Plan {
        id: "music-single-track-long",
        kind: LibraryKind::Album,
        profile: PlayableItemProfile::Music,
        folder: "Music/Ada Lovelace/First Light",
        noudata: "First Light.noudata",
        uploads: &[("First Light.mp3", 204)],
        downloads: &[],
    },
    Plan {
        id: "music-album-unitedata-long",
        kind: LibraryKind::Album,
        profile: PlayableItemProfile::Music,
        folder: "Music/The Aurora Band/Northern Lights",
        noudata: "_Northern Lights.noudata",
        uploads: &[
            ("01 Polar Night.mp3", 225),
            ("02 - Green Sky.flac", 245),
            ("03 Dawn.mp3", 3723),
        ],
        downloads: &[],
    },
    Plan {
        id: "music-album-free-download-short",
        kind: LibraryKind::Album,
        profile: PlayableItemProfile::Music,
        folder: "Music/Open Hands/Free Sessions",
        noudata: "_Free Sessions.noudata",
        uploads: &[],
        downloads: &[0, 1],
    },
    Plan {
        id: "audiobook-long",
        kind: LibraryKind::Audiobook,
        profile: PlayableItemProfile::Audiobook,
        folder: "Audiobooks/Shelley-Jones, Mary/The Clockmaker's Daughter",
        noudata: "_The Clockmaker's Daughter.noudata",
        uploads: &[
            ("Chapter 01.mp3", 2712),
            ("Chapter 02.mp3", 3090),
            ("Chapter 03.mp3", 3765),
        ],
        downloads: &[],
    },
    Plan {
        id: "podcast-episode-short",
        kind: LibraryKind::Podcast,
        profile: PlayableItemProfile::Podcast,
        folder: "Podcasts/Episode 42 Why Tape Is Not Dead",
        noudata: "Episode 42 Why Tape Is Not Dead.noudata",
        uploads: &[],
        downloads: &[0],
    },
    Plan {
        id: "mix-single-file-long",
        kind: LibraryKind::Mix,
        profile: PlayableItemProfile::Mix,
        folder: "Music/Mixes/DJ Nightshift",
        noudata: "Midnight Drive Mix.noudata",
        uploads: &[("Midnight Drive Mix.mp3", 7200)],
        downloads: &[],
    },
    Plan {
        id: "unicode-emoji-explicit-slugs-long",
        kind: LibraryKind::Album,
        profile: PlayableItemProfile::Music,
        folder: "Music/Björk ✨ Guðmundsdóttir/봄날 🌸 Spring Day",
        noudata: "_봄날 🌸 Spring Day.noudata",
        uploads: &[("01 봄날.mp3", 274), ("02 Emoji Song.mp3", 179)],
        downloads: &[],
    },
    Plan {
        id: "unicode-title-derived-slug-long",
        kind: LibraryKind::Album,
        profile: PlayableItemProfile::Music,
        folder: "Music/José González/Café del Mar — Été 2026",
        noudata: "Café del Mar — Été 2026.noudata",
        uploads: &[("Café del Mar — Été 2026.mp3", 342)],
        downloads: &[],
    },
    Plan {
        id: "release-platform-types-long",
        kind: LibraryKind::Album,
        profile: PlayableItemProfile::Music,
        folder: "Music/Multi Platform/All Platforms",
        noudata: "_All Platforms.noudata",
        uploads: &[("01 Track A.mp3", 3)],
        downloads: &[1],
    },
    Plan {
        id: "free-single-track-no-cover-long",
        kind: LibraryKind::Album,
        profile: PlayableItemProfile::Music,
        folder: "Music/Lo Fi Larry/Hiss and Crackle",
        noudata: "Hiss and Crackle.noudata",
        uploads: &[],
        downloads: &[0],
    },
    Plan {
        id: "override-artist-various-short",
        kind: LibraryKind::Album,
        profile: PlayableItemProfile::Music,
        folder: "Music/Ada & Bob/Various Voices",
        noudata: "_Various Voices.noudata",
        uploads: &[("01 - Duet.flac", 213), ("02 - Solo.flac", 142)],
        downloads: &[],
    },
];

fn plan_of(id: &str) -> &'static Plan {
    PLANS.iter().find(|p| p.id == id).unwrap()
}

// ----- synthesised media and images -------------------------------------------------------

/// 8-bit mono WAV whose header declares `secs` seconds (lofty trusts the header), with a short
/// real payload filled with `fill` so tests can tell the files apart. Sniffs as audio and
/// decodes (a truncated file ends early).
fn wav(secs: u64, fill: u8) -> Vec<u8> {
    let rate: u32 = 8000;
    let declared = (secs.max(1) * rate as u64) as u32;
    let real = declared.min(8000) as usize;
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + declared).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&8u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&declared.to_le_bytes());
    v.resize(v.len() + real, fill);
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

fn secs_of(web: &Value, item: usize) -> u64 {
    let s = web["mediaList"][item]["uniteData"]["expectedLength"]
        .as_str()
        .unwrap_or("1:00");
    plippa_noubin::NoubinTimestamp::parse(s)
        .unwrap()
        .as_millis()
        / 1000
}

// ----- the fixture linkhost ----------------------------------------------------------------

/// `https://host/dir/` -> `/host/dir/` (the path the `Rewrite` transport asks the fixture for).
fn fx_path(url: &str) -> String {
    format!("/{}", url.strip_prefix("https://").unwrap())
}

/// Serves `web.noudata` and every published file under the published URL form; with `dual` the
/// other form's `web.noudata` is served too (a hypothetical linkhost that hosts both shapes;
/// the real one locks one shape per release). Free downloads are served from their hosts.
fn linkhost(fx: &Fx, plan: &Plan, dual: bool) -> Host {
    let mut routes: HashMap<String, (&'static str, Vec<u8>)> = HashMap::new();
    let published_dir = fx.s("publicUrl");
    let other_dir = if fx.published_is_long() {
        fx.short()
    } else {
        fx.long()
    };
    let json = "application/json; charset=utf-8";
    for f in fx.published() {
        let body: (&'static str, Vec<u8>) = if f == "web.noudata" {
            (json, fx.bytes.clone())
        } else if f == "index.html" {
            (
                "text/html; charset=utf-8",
                b"<html>release page</html>".to_vec(),
            )
        } else if f == "cover.webp" {
            ("image/webp", webp(1500, 1500))
        } else if let Some(rest) = f.strip_prefix("thumbnails/cover-") {
            let (n, ext) = rest.split_once('.').unwrap();
            let n: u32 = n.parse().unwrap();
            if ext == "webp" {
                ("image/webp", webp(n, n))
            } else {
                ("image/jpeg", jpeg(n as u16, n as u16))
            }
        } else {
            panic!("unknown published file {f}");
        };
        routes.insert(format!("{}{f}", fx_path(&published_dir)), body);
    }
    if dual {
        routes.insert(
            format!("{}web.noudata", fx_path(&other_dir)),
            (json, fx.bytes.clone()),
        );
    }
    for &i in plan.downloads {
        let url = fx.web["mediaList"][i]["uniteData"]["freeDownloadURL"]
            .as_str()
            .unwrap();
        routes.insert(
            fx_path(url),
            (
                "application/octet-stream",
                wav(secs_of(&fx.web, i), 100 + i as u8),
            ),
        );
    }
    host(move |p| {
        let path = p.split('?').next().unwrap();
        match routes.get(path) {
            Some((ct, b)) => Reply::Bytes(200, ct, b.clone()),
            None => Reply::Bytes(
                404,
                "application/json",
                br#"{"message":"Release not found"}"#.to_vec(),
            ),
        }
    })
}

// ----- environment -------------------------------------------------------------------------

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    svc: Arc<Mutex<ServerService>>,
    unite: Arc<UniteHub>,
    ipc: RecordingIpc,
    host: Host,
}

fn env(host: Host) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    std::fs::create_dir(&root).unwrap();
    let ipc = RecordingIpc::new();
    let mut svc = ServerService::start_with_roots(
        vec![root.clone()],
        Box::new(ipc.clone()),
        ServerService::default_mock_associate(),
    );
    svc.set_web_hub(Arc::new(WebHub::with_parts(
        WebConfig::default(),
        Arc::new(Rewrite {
            inner: UreqTransport::for_local_fixtures(),
            port: host.port,
        }),
        FetchPolicy::production(),
        Limits::default(),
        Box::new(|| true),
    )));
    let unite = Arc::new(UniteHub::with_limits(DownloadLimits::default()));
    svc.set_unite_hub(Arc::clone(&unite));
    Env {
        _tmp: tmp,
        root: std::fs::canonicalize(root).unwrap(),
        svc: Arc::new(Mutex::new(svc)),
        unite,
        ipc,
        host,
    }
}

fn tap(engine: &mut Engine, url: &str, id: u64) -> Vec<EngineEvent> {
    engine.take_events();
    engine.handle_ipc(IpcMessage::TapNdef(TapNdefPayload {
        ndef_message: encode_uri_record(url).unwrap(),
        tap_id: id,
    }));
    engine.take_events()
}

fn unmatched(events: &[EngineEvent]) -> Vec<plippa_ipc_proto::UnmatchedNoubin> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::UnmatchedNoubin(u) => Some(u.clone()),
            _ => None,
        })
        .collect()
}

fn files_under(dir: &Path) -> BTreeSet<String> {
    fn walk(d: &Path, base: &Path, out: &mut BTreeSet<String>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.insert(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut s = BTreeSet::new();
    walk(dir, dir, &mut s);
    s
}

fn overview_expectations(fx: &Fx, ov: &Value) {
    let w = &fx.web;
    let id = &fx.id;
    assert_eq!(ov["title"], w["playableItemTitle"], "{id} title");
    assert_eq!(ov["profile"], w["playableItemProfile"], "{id} profile");
    let credits = &w["releaseData"]["credits"];
    let artist = match credits["primaryArtistOverrideName"].as_str() {
        Some(o) => o.to_string(),
        None => credits["primaryArtists"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["primaryArtistName"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join(", "),
    };
    assert_eq!(ov["artist"], artist.as_str(), "{id} artist");
    // Platforms: artist's order, right type, verb and sanitised URL.
    let want: Vec<&Value> = w["releaseData"]["releasePlatforms"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let got = ov["platforms"].as_array().unwrap();
    assert_eq!(got.len(), want.len(), "{id} platform count");
    for (g, p) in got.iter().zip(&want) {
        assert_eq!(g["name"], p["platformName"], "{id}");
        assert_eq!(g["platform_type"], p["platformType"], "{id}");
        assert_eq!(g["url"], p["platformURL"], "{id}");
        let verb = match (
            p["customVerb"].as_str(),
            p["platformType"].as_str().unwrap(),
        ) {
            (Some(v), _) => v,
            (None, "store" | "DRMStore") => "buy",
            (None, "streaming") => "stream",
            _ => "listen to",
        };
        assert_eq!(g["verb"], verb, "{id}");
        assert_eq!(
            g["label"],
            format!(
                "{verb} {} on {}",
                w["playableItemTitle"].as_str().unwrap(),
                p["platformName"].as_str().unwrap()
            ),
            "{id}"
        );
    }
    let links = w["releaseData"]["furtherLinks"].as_array().unwrap();
    let got = ov["further_links"].as_array().unwrap();
    assert_eq!(got.len(), links.len(), "{id} links");
    for (g, l) in got.iter().zip(links) {
        assert_eq!(g["title"], l["linkTitle"], "{id}");
        assert_eq!(g["url"], l["linkURL"], "{id}");
    }
    // Tracks: titles and expected lengths.
    let items = w["mediaList"].as_array().unwrap();
    let tracks = ov["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), items.len(), "{id} tracks");
    for (t, m) in tracks.iter().zip(items) {
        assert_eq!(t["title"], m["mediaTitle"], "{id}");
        let ms = m["uniteData"]["expectedLength"].as_str().map(|s| {
            plippa_noubin::NoubinTimestamp::parse(s)
                .unwrap()
                .as_millis()
        });
        assert_eq!(t["expected_length_ms"].as_u64(), ms, "{id}");
        assert_eq!(
            t["free_download"],
            m["uniteData"]["freeDownloadURL"].is_string(),
            "{id}"
        );
    }
    let cover = w
        .pointer("/releaseData/coverImage/images/0/imageWebURL")
        .and_then(Value::as_str);
    assert_eq!(ov["cover_url"].as_str(), cover, "{id} cover url");
    let free = items
        .iter()
        .any(|m| m["uniteData"]["freeDownloadURL"].is_string());
    assert_eq!(ov["free_download"]["available"], free, "{id} free");
    assert_eq!(
        ov["warnings"].as_array().unwrap().len(),
        0,
        "{id}: fetch/sanitise warnings {}",
        ov["warnings"]
    );
}

/// The whole association journey for one case and tapped form. `dual`: the fixture also serves
/// `web.noudata` under the other URL form (see [`linkhost`]).
fn journey(fx: &Fx, tapped_long: bool, dual: bool) {
    let plan = plan_of(&fx.id);
    let id = fx.id.as_str();
    let tapped = fx.tapped(tapped_long);
    let form = if tapped_long { "long" } else { "short" };
    let tag = format!("{id}/{form}");
    let e = env(linkhost(fx, plan, dual));
    let norm = normalise_noubin_key(&tapped);

    // 1. Unknown tag tap: a real engine on the empty library raises UnmatchedNoubin with the
    //    tapped string and its §6.2 normalisation.
    let mut engine = Engine::open_mock(&e.root, Capabilities::desktop_mock()).unwrap();
    let ev = tap(&mut engine, &tapped, 1);
    let u = unmatched(&ev);
    assert_eq!(u.len(), 1, "{tag}: one UnmatchedNoubin, got {ev:?}");
    assert_eq!(u[0].noubin_key, tapped, "{tag}");
    assert_eq!(u[0].normalised_key, norm, "{tag}");
    // The server shows the banner with the tapped key.
    e.ipc.inject_unmatched(u[0].clone());
    assert_eq!(
        e.svc
            .lock()
            .unwrap()
            .status()
            .unmatched_noubin
            .unwrap()
            .noubin_key,
        tapped,
        "{tag}"
    );
    assert!(!e.svc.lock().unwrap().has_local_match(&tapped), "{tag}");

    // 2. Consent-gated fetch: prepare shows the exact URL and sends nothing; fetch without
    //    consent opens no socket.
    let prep = prepare_unlocked(&e.svc, &tapped).unwrap();
    assert_eq!(prep.fetch_url, normalise_noubin_url(&tapped), "{tag}");
    assert_eq!(e.host.count(), 0, "{tag}: prepare sent something");
    assert_eq!(
        fetch_unlocked(&e.svc, &tapped, None).unwrap_err().code(),
        "consent_required"
    );
    assert_eq!(
        e.host.count(),
        0,
        "{tag}: a socket was opened without consent"
    );

    // 3. Start the unite (consented fetch). The one request is for web.noudata at the URL the
    //    linkhost serves it at (short form: same file, served directly, no redirect).
    let started = e
        .unite
        .start(
            &e.svc,
            StartRequest {
                noubin_key: Some(tapped.clone()),
                consent: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    let s = started["session"].as_str().unwrap().to_string();
    assert_eq!(
        e.host.hits(),
        [fx_path(&normalise_noubin_url(&tapped))],
        "{tag}: exactly one request, for web.noudata"
    );
    overview_expectations(fx, &started["overview"]);

    // 4. Media + images. Free items come from their freeDownloadURL, the rest are files the
    //    user already owns (named like real store downloads).
    let items = fx.web["mediaList"].as_array().unwrap().len();
    let has_cover = fx
        .web
        .pointer("/releaseData/coverImage/images/0/imageWebURL")
        .is_some();
    let want_media = !plan.downloads.is_empty();
    if want_media || has_cover {
        let prep = e
            .unite
            .download_prepare(
                &s,
                PrepareRequest {
                    media: want_media,
                    images: has_cover,
                },
            )
            .unwrap();
        let urls: Vec<String> = prep["urls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["url"].as_str().unwrap().to_string())
            .collect();
        let expect_urls = plan.downloads.len() + if has_cover { 1 + 6 } else { 0 };
        assert_eq!(urls.len(), expect_urls, "{tag}: consent list {urls:?}");
        for &i in plan.downloads {
            assert!(
                urls.contains(
                    &fx.web["mediaList"][i]["uniteData"]["freeDownloadURL"]
                        .as_str()
                        .unwrap()
                        .to_string()
                ),
                "{tag}"
            );
        }
        let cid = prep["consent_id"].as_str().unwrap().to_string();
        e.unite
            .download_start(
                &e.svc,
                &s,
                DownloadRequest {
                    consent: Some(true),
                    consent_id: Some(cid),
                },
            )
            .unwrap();
        e.unite.join_workers();
        let st = e.unite.status(&s).unwrap();
        assert_eq!(st["download"]["state"], "done", "{tag}: {}", st["download"]);
    }
    let mut owned_items: Vec<usize> = (0..items).filter(|i| !plan.downloads.contains(i)).collect();
    for ((name, secs), idx) in plan.uploads.iter().zip(owned_items.drain(..)) {
        let bytes = wav(*secs, 100 + idx as u8);
        e.unite
            .add_upload(&s, name, Some(bytes.len() as u64), &mut Cursor::new(bytes))
            .unwrap();
    }
    assert!(owned_items.is_empty(), "{tag}: plan covers every item");

    // 5. Proposal -> confirm -> apply.
    let prop = e.unite.proposal(&s).unwrap();
    assert_eq!(
        prop["deterministic"],
        plan.uploads.is_empty(),
        "{tag}: {prop}"
    );
    assert_eq!(
        prop["unmatched_items"].as_array().unwrap().len(),
        0,
        "{tag}: {prop}"
    );
    assert_eq!(
        prop["unmatched_files"].as_array().unwrap().len(),
        0,
        "{tag}: {prop}"
    );
    let all_confident = prop["matches"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["level"] == "confident");
    // Releases that publish expectedFilename (and length) for every item must match with
    // confidence; a single item without any unite data can only be matched by elimination.
    let has_unite = fx.web["mediaList"]
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["uniteData"].is_object());
    if has_unite {
        assert!(all_confident, "{tag}: {prop}");
    }
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
    assert_eq!(preview["folder"], plan.folder, "{tag}: {preview}");
    assert_eq!(preview["noudata"], plan.noudata, "{tag}: {preview}");
    let out = e.unite.apply(&e.svc, &s, ApplyRequest::default()).unwrap();
    assert_eq!(out["ok"], true, "{tag}");
    let rel = out["noudata"].as_str().unwrap().to_string();
    assert_eq!(rel, format!("{}/{}", plan.folder, plan.noudata), "{tag}");
    assert_eq!(out["key"], tapped.as_str(), "{tag}");
    assert_eq!(out["normalised_key"], norm.as_str(), "{tag}");

    // 6. The local `.noudata`: conforms, zero warnings, release, keyed with the tapped string.
    let path = e.root.join(&rel);
    let dir = path.parent().unwrap().to_path_buf();
    let (doc, warnings) = parse_noudata_with_warnings(&std::fs::read(&path).unwrap()).unwrap();
    assert!(warnings.is_empty(), "{tag}: {warnings:?}");
    assert_eq!(doc.noudata_type, Some(NoudataType::Release), "{tag}");
    assert!(
        doc.type_violations().is_empty(),
        "{tag}: {:?}",
        doc.type_violations()
    );
    assert_eq!(doc.profile(), plan.profile, "{tag}");
    assert_eq!(doc.noubin_key.as_deref(), Some(tapped.as_str()), "{tag}");
    assert_eq!(
        doc.noubin_key_normalised.as_deref(),
        Some(norm.as_str()),
        "{tag}"
    );
    assert_eq!(doc.noudata_version.as_deref(), Some("0.4.0"), "{tag}");
    assert_eq!(
        doc.what_am_i_looking_at.as_deref(),
        Some(plippa_noubin::WHAT_AM_I_LOOKING_AT),
        "{tag}: a local file carries the standard's fixed _whatAmILookingAt text"
    );
    assert_eq!(
        doc.playable_item_title,
        fx.web["playableItemTitle"].as_str().map(String::from),
        "{tag}"
    );
    assert!(
        doc.refer_to_release_noudata.is_none() && doc.local_playback_data.is_none(),
        "{tag}"
    );
    assert_eq!(doc.media_list.len(), items, "{tag}");
    for (i, item) in doc.media_list.iter().enumerate() {
        let paths = item
            .local_file_paths
            .as_ref()
            .unwrap_or_else(|| panic!("{tag}: item {i} has no localFilePaths"));
        assert!(!paths.is_empty(), "{tag}");
        let f = dir.join(&paths[0]);
        let bytes = std::fs::read(&f).unwrap_or_else(|_| panic!("{tag}: {f:?} missing"));
        assert_eq!(
            *bytes.last().unwrap(),
            100 + i as u8,
            "{tag}: item {i} got the wrong file"
        );
        // The published unite data travelled along (titles, expected names/lengths/URLs).
        assert_eq!(
            item.media_title.as_deref(),
            fx.web["mediaList"][i]["mediaTitle"].as_str(),
            "{tag}"
        );
        let u = &fx.web["mediaList"][i]["uniteData"];
        assert_eq!(
            item.unite_data
                .as_ref()
                .and_then(|u| u.expected_filename.as_deref()),
            u["expectedFilename"].as_str(),
            "{tag}"
        );
        assert_eq!(
            item.unite_data
                .as_ref()
                .and_then(|u| u.free_download_url.as_deref()),
            u["freeDownloadURL"].as_str(),
            "{tag}"
        );
    }
    // The linkhost publishes only standard properties, so the local file carries no unknown
    // release-level property (tolerance of unknown properties is covered in plippa-noubin's
    // `xcheck_a_tolerates_non_standard_linkhost_output`).
    assert!(
        doc.release_data.as_ref().unwrap().extra.is_empty(),
        "{tag}: {:?}",
        doc.release_data.as_ref().unwrap().extra
    );
    let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["noudataType"], "release", "{tag}");
    assert!(
        raw["plippaSourceURL"]
            .as_str()
            .unwrap()
            .starts_with("https://"),
        "{tag}"
    );

    // 7. Images (e): cover and thumbnails exactly as published.
    let local_files = files_under(&dir);
    let published: BTreeSet<String> = fx.published().into_iter().collect();
    let cover_local = doc.cover_local_filename();
    if has_cover {
        assert_eq!(cover_local, Some("cover.webp"), "{tag}");
        let want: BTreeSet<&String> = published
            .iter()
            .filter(|f| f.starts_with("thumbnails/") || *f == "cover.webp")
            .collect();
        let got: BTreeSet<&String> = local_files
            .iter()
            .filter(|f| f.starts_with("thumbnails/") || *f == "cover.webp")
            .collect();
        assert_eq!(got, want, "{tag}: local images vs publishedFiles");
        assert!(
            !local_files
                .iter()
                .any(|f| f == "index.html" || f == "web.noudata"),
            "{tag}: {local_files:?}"
        );
    } else {
        assert_eq!(cover_local, None, "{tag}");
        assert!(
            !local_files
                .iter()
                .any(|f| f.starts_with("thumbnails/") || f.starts_with("cover")),
            "{tag}"
        );
    }
    assert!(
        files_under(&e.root.join(".plippa/tmp")).is_empty(),
        "{tag}: staging left behind"
    );

    // 8. The library indexes it as the right kind, found by the normalised key.
    let reader = LibraryReader::open(&LibraryRoot::new(&e.root)).unwrap();
    let LookupResult::Found(hit) = reader.lookup_by_normalised_key(&norm).unwrap() else {
        panic!("{tag}: not found by normalised key");
    };
    assert_eq!(hit.records.len(), 1, "{tag}");
    let rec = hit.first();
    assert_eq!(rec.kind, plan.kind, "{tag}");
    assert_eq!(rec.noudata_relpath, rel, "{tag}");
    assert_eq!(
        rec.title,
        fx.web["playableItemTitle"].as_str().map(String::from),
        "{tag}"
    );
    assert_eq!(
        rec.release_profile.as_deref(),
        fx.web["playableItemProfile"].as_str(),
        "{tag}"
    );

    // 9. Second tap of the same key: found locally, loads and plays, zero network requests.
    let before = e.host.count();
    let ev = tap(&mut engine, &tapped, 2);
    assert!(
        unmatched(&ev).is_empty(),
        "{tag}: second tap offered the web again: {ev:?}"
    );
    assert!(e.svc.lock().unwrap().has_local_match(&tapped), "{tag}");
    assert_eq!(
        fetch_unlocked(&e.svc, &tapped, Some(true))
            .unwrap_err()
            .code(),
        "local_match",
        "{tag}"
    );
    assert_eq!(
        prepare_unlocked(&e.svc, &tapped).unwrap_err().code(),
        "local_match",
        "{tag}"
    );
    assert_eq!(
        e.host.count(),
        before,
        "{tag}: second tap made network requests"
    );
    let entries = engine.now_playing_entries();
    assert!(entries.len() >= items, "{tag}: {entries:?}");
    let first = if items == 1 {
        fx.web["playableItemTitle"].as_str().unwrap()
    } else {
        fx.web["mediaList"][0]["mediaTitle"].as_str().unwrap()
    };
    assert_eq!(entries[0].title, first, "{tag}");
    engine.publish_status();
    let status = engine
        .take_events()
        .into_iter()
        .rev()
        .find_map(|ev| match ev {
            EngineEvent::Status(s) => Some(s),
            _ => None,
        })
        .unwrap();
    assert_eq!(status.session.transport, TransportState::Playing, "{tag}");
    // A key that was never associated is still unmatched (and not matched by a prefix).
    let other = format!(
        "{}NEVER/",
        tapped
            .trim_end_matches('/')
            .rsplit_once('/')
            .unwrap()
            .0
            .to_string()
            + "/"
    );
    let ev = tap(&mut engine, &other, 3);
    assert_eq!(unmatched(&ev).len(), 1, "{tag}: {other}");
    assert!(!e.svc.lock().unwrap().has_local_match(&other), "{tag}");
}

// ----- tests -------------------------------------------------------------------------------

#[test]
fn xcheck_d_journey_every_case_long_url_tapped() {
    for fx in fixtures() {
        journey(&fx, true, true);
    }
}

#[test]
fn xcheck_d_journey_every_case_short_url_tapped() {
    for fx in fixtures() {
        journey(&fx, false, true);
    }
}

/// (c) Every spelling of a Noubin URL that a tag may carry reaches the same `web.noudata` the
/// linkhost serves, over the production fetch policy. The short host serves `web.noudata`
/// directly (Transform Rule `noub.in/<code>/... -> _releases/_short/<code>/...`), no redirect.
#[test]
fn xcheck_c_fetch_reaches_web_noudata_for_every_url_spelling() {
    for fx in fixtures() {
        let plan = plan_of(&fx.id);
        let published = fx.tapped(fx.published_is_long());
        let meta_path = fx_path(&fx.s("metadataUrl"));
        let rest = published.strip_prefix("https://").unwrap();
        let (h, p) = rest.split_once('/').unwrap();
        let variants = vec![
            published.clone(),
            published.replace("https://", "http://"),
            published.replace("https://", "https://www."),
            format!("https://{}/{p}", h.to_ascii_uppercase()),
            published.trim_end_matches('/').to_string(),
            format!("{published}?ref=nfc"),
            format!("{published}#track-3"),
            format!("{published}index.html"),
            format!("  {published}  "),
        ];
        for v in variants {
            let e = env(linkhost(&fx, plan, false));
            let r = fetch_unlocked(&e.svc, &v, Some(true))
                .unwrap_or_else(|err| panic!("{}: {v:?}: {err}", fx.id));
            assert_eq!(
                r.overview.title.as_deref(),
                fx.web["playableItemTitle"].as_str(),
                "{}: {v:?}",
                fx.id
            );
            let hits = e.host.hits();
            assert_eq!(hits.len(), 1, "{}: {v:?}: {hits:?}", fx.id);
            assert_eq!(
                hits[0].split('?').next().unwrap(),
                meta_path,
                "{}: {v:?}",
                fx.id
            );
            // A query on the tag is passed on; a fragment never is.
            assert_eq!(
                hits[0].contains("ref=nfc"),
                v.contains("?ref=nfc"),
                "{}",
                fx.id
            );
            assert!(!hits[0].contains('#'), "{}", fx.id);
        }
    }
}

/// The real linkhost locks each release to ONE URL shape (long XOR short) and serves
/// `web.noudata` only under that one. Tapping the other spelling is a clean `not_found`:
/// one request, nothing held, nothing created, the tapped key stays unmatched.
#[test]
fn xcheck_c_the_unpublished_url_form_is_a_clean_not_found() {
    for fx in fixtures() {
        let plan = plan_of(&fx.id);
        let other = fx.tapped(!fx.published_is_long());
        let e = env(linkhost(&fx, plan, false));
        let err = fetch_unlocked(&e.svc, &other, Some(true)).unwrap_err();
        assert_eq!(err.code(), "not_found", "{}: {other}", fx.id);
        assert_eq!(e.host.count(), 1, "{}", fx.id);
        assert!(!e.svc.lock().unwrap().has_local_match(&other), "{}", fx.id);
        let session = e.unite.start(
            &e.svc,
            StartRequest {
                noubin_key: Some(other.clone()),
                consent: Some(true),
                ..Default::default()
            },
        );
        assert!(session.is_err(), "{}: unite started from a 404", fx.id);
        assert_eq!(e.unite.session_count(), 0, "{}", fx.id);
        assert!(
            files_under(&e.root)
                .iter()
                .all(|f| f.starts_with(".plippa")),
            "{}",
            fx.id
        );
    }
}

/// A short host that redirects `web.noudata` to the long host (not what the linkhost does
/// today, but allowed by the standard) still works: https-only hop, limits are covered by the fetch tests.
#[test]
fn xcheck_c_short_form_through_a_redirect_hop_works() {
    let fx = fixtures()
        .into_iter()
        .find(|f| f.id == "music-single-track-long")
        .unwrap();
    let long_meta = fx.s("metadataUrl");
    let short_path = "/noub.in/MUSIC001/web.noudata".to_string();
    let body = fx.bytes.clone();
    let lp = fx_path(&long_meta);
    let (sp, lm) = (short_path.clone(), long_meta.clone());
    let h = host(move |p| {
        if p == sp {
            Reply::Redirect(lm.clone())
        } else if p == lp {
            Reply::Bytes(200, "application/json", body.clone())
        } else {
            Reply::Bytes(404, "text/plain", Vec::new())
        }
    });
    let e = env(h);
    let r = fetch_unlocked(&e.svc, "https://noub.in/MUSIC001/", Some(true)).unwrap();
    assert_eq!(r.overview.title.as_deref(), Some("First Light"));
    assert_eq!(e.host.hits(), [short_path, fx_path(&long_meta)]);
}

/// (e) Plippa derives cover and thumbnail URLs from `imageWebURL` and the §1.27 naming; each
/// one exists in the linkhost's `publishedFiles` (or, for a release without cover, there are
/// none and nothing is published).
#[test]
fn xcheck_e_derived_image_urls_match_published_files() {
    for fx in fixtures() {
        let (mut doc, _) = parse_noudata_with_warnings(&fx.bytes).unwrap();
        assert!(plippa_noubin::sanitise_document(
            &mut doc,
            plippa_noubin::SanitiseOptions::fetched()
        )
        .is_empty());
        let (targets, refused) = plan_cover_targets(&doc);
        assert!(refused.is_empty(), "{}: {refused:?}", fx.id);
        let published = fx.published();
        let dir = fx.s("publicUrl");
        let web_url = fx
            .web
            .pointer("/releaseData/coverImage/images/0/imageWebURL")
            .and_then(Value::as_str);
        match web_url {
            None => {
                assert!(targets.is_empty(), "{}", fx.id);
                assert!(
                    published
                        .iter()
                        .all(|f| !f.starts_with("thumbnails/") && !f.starts_with("cover")),
                    "{}",
                    fx.id
                );
            }
            Some(url) => {
                assert_eq!(targets.len(), 1, "{}", fx.id);
                let t = &targets[0];
                assert_eq!(t.url, url, "{}: cover url unchanged (query kept)", fx.id);
                assert_eq!(t.local_stem, "cover", "{}", fx.id);
                let rel = |u: &str| {
                    u.strip_prefix(dir.as_str())
                        .unwrap_or_else(|| panic!("{}: {u} outside {dir}", fx.id))
                        .to_string()
                };
                assert!(
                    published.contains(&rel(t.url.split('?').next().unwrap())),
                    "{}",
                    fx.id
                );
                let th = t.thumbnails();
                assert_eq!(th.len(), 6, "{}", fx.id);
                for x in &th {
                    let r = rel(&x.url);
                    assert!(
                        published.contains(&r),
                        "{}: derived {r} not published",
                        fx.id
                    );
                    assert_eq!(
                        r,
                        format!("thumbnails/cover-{}.{}", x.tier, x.ext),
                        "{}",
                        fx.id
                    );
                    // The `/Thumbnails/` fallback is only a retry; the linkhost uses lowercase.
                    assert!(!published.contains(&rel(&x.fallback_url)), "{}", fx.id);
                }
                // Everything the linkhost publishes as an image is something Plippa asks for.
                let asked: BTreeSet<String> = th
                    .iter()
                    .map(|x| rel(&x.url))
                    .chain(["cover.webp".to_string()])
                    .collect();
                let images: BTreeSet<String> = published
                    .iter()
                    .filter(|f| f.starts_with("thumbnails/") || f.starts_with("cover"))
                    .cloned()
                    .collect();
                assert_eq!(asked, images, "{}", fx.id);
            }
        }
    }
}
