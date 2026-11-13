//! NB_06: associate for collections + per-track files + locked-tag move semantics.
//!
//! Fixture: `crates/plippa-noubin/tests/fixtures/libraries/kinds` (copied to a temp dir).
//! The NFC side is a closure standing in for the UI's `AssociateResponse`:
//! blank/keyed rewritable, keyed locked, blank locked, tiny capacity (the hardware-side
//! states are exercised in `plippa-hw` / `plippa-ui`; here the server's side of each).

mod common;

use plippa_ipc_proto::{AssociateRequest, AssociateResponse};
use plippa_noubin::{
    classify_type, fs_release_resolver, normalise_noubin_key, parse_noudata, LibraryKind, LibraryReader,
    LibraryRoot, NoudataDocument, NoudataType,
};
use plippa_server::{
    AssociateOutcome, RecordingIpc, ServerConfig, ServerError, ServerService, WARN_DUPLICATE_KEY,
};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn kinds_library() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/plippa-noubin/tests/fixtures/libraries/kinds"),
        &root,
    );
    (tmp, root)
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), to).unwrap();
        }
    }
}

type Nfc = Box<dyn FnMut(&AssociateRequest) -> AssociateResponse + Send>;

fn service_with(root: &Path, nfc: Nfc) -> ServerService {
    ServerService::start(Some(root.to_path_buf()), Box::new(RecordingIpc::new()), nfc)
}

/// Rewritable tag: the key the server asked for is written; `previous` was on the tag before.
fn rewritable(previous: Option<&'static str>) -> Nfc {
    Box::new(move |req| AssociateResponse {
        request_id: req.request_id,
        ok: true,
        noubin_key: Some(req.noubin_key.clone()),
        noubin_key_normalised: Some(normalise_noubin_key(&req.noubin_key)),
        previous_key: previous.map(str::to_string),
        ..Default::default()
    })
}

/// Rewritable tag that ends up carrying a fixed key (`tag_locked: false`).
fn tag_with(key: &'static str, previous: Option<&'static str>) -> Nfc {
    Box::new(move |req| AssociateResponse {
        request_id: req.request_id,
        ok: true,
        noubin_key: Some(key.into()),
        noubin_key_normalised: Some(normalise_noubin_key(key)),
        previous_key: previous.map(str::to_string),
        ..Default::default()
    })
}

/// Locked tag that already carries `key`; nothing was written.
fn locked(key: &'static str) -> Nfc {
    Box::new(move |req| AssociateResponse {
        request_id: req.request_id,
        ok: true,
        noubin_key: Some(key.into()),
        noubin_key_normalised: Some(normalise_noubin_key(key)),
        tag_locked: true,
        ..Default::default()
    })
}

fn id_of(svc: &mut ServerService, kind: LibraryKind, title: &str) -> u64 {
    svc.list_collections(Some(kind))
        .unwrap()
        .into_iter()
        .find(|c| c.title == title)
        .unwrap_or_else(|| panic!("no {kind:?} titled {title}"))
        .id
}

fn doc(root: &Path, rel: &str) -> NoudataDocument {
    parse_noudata(&fs::read(root.join(rel)).unwrap()).unwrap()
}

fn key_of(root: &Path, rel: &str) -> Option<String> {
    doc(root, rel).noubin_key_normalised
}

fn count_noudata(root: &Path) -> usize {
    fn walk(p: &Path, n: &mut usize) {
        for e in fs::read_dir(p).unwrap() {
            let e = e.unwrap();
            let path = e.path();
            if path.is_dir() {
                if e.file_name() != ".plippa" {
                    walk(&path, n);
                }
            } else if path.extension().is_some_and(|x| x == "noudata") {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    walk(root, &mut n);
    n
}

fn holders(root: &Path, key: &str) -> Vec<String> {
    LibraryReader::open(&LibraryRoot::new(root))
        .unwrap()
        .find_by_key(key)
        .unwrap()
        .into_iter()
        .map(|r| r.noudata_relpath)
        .collect()
}

const ALBUM: &str = "Music/Band/Album/_Album.noudata";
const PLAYLIST: &str = "Playlists/Rainy.noudata";

fn ok(r: Result<AssociateOutcome, ServerError>) -> AssociateOutcome {
    let o = r.expect("associate call");
    assert!(o.ok, "{:?}", o.error);
    o
}

// ---------------------------------------------------------------------------
// Per-track associate
// ---------------------------------------------------------------------------

#[test]
fn associate_album_then_single_track_makes_two_resolving_files() {
    let (_t, root) = kinds_library();
    let mut svc = service_with(&root, rewritable(None));
    let album = id_of(&mut svc, LibraryKind::Album, "The Album");
    let before = count_noudata(&root);

    let a = ok(svc.associate_playable(album));
    assert_eq!(a.bound.as_ref().unwrap().item.kind, "album");
    assert!(!a.bound.as_ref().unwrap().created_track_file);
    assert_eq!(
        count_noudata(&root),
        before,
        "no per-track file for a collection"
    );
    let album_key = a.noubin_key_normalised.clone().unwrap();

    // Track 0 ("One") has no file of its own yet.
    let t = ok(svc.associate_track(album, 0, false));
    let b = t.bound.as_ref().unwrap();
    assert!(b.created_track_file);
    assert_eq!(b.track_item, Some(0));
    assert_eq!(b.item.kind, "track");
    assert_eq!(b.item.noudata_relpath, "Music/Band/Album/01 - One.noudata");
    assert_ne!(b.item.id, 0, "bound id resolved from the refreshed index");
    assert_eq!(count_noudata(&root), before + 1);

    let track = doc(&root, &b.item.noudata_relpath);
    assert_eq!(
        track.refer_to_release_noudata.as_deref(),
        Some("_Album.noudata")
    );
    assert!(track.release_data.is_none(), "no duplicated releaseData");
    assert_eq!(track.media_list.len(), 1);
    assert_eq!(
        track.media_list[0].local_file_paths.as_deref(),
        Some(&["01 - One.mp3".to_string()][..])
    );
    assert_eq!(track.playable_item_title.as_deref(), Some("One"));
    assert_eq!(track.noudata_type, Some(NoudataType::LinkTrack));
    assert!(track.media_list[0].media_title.is_none(), "loose/link tracks use playableItemTitle");
    assert!(track.release_data.is_none() && track.playable_item_profile.is_none());
    assert!(track.type_violations().is_empty());
    assert_eq!(track.noubin_key_normalised, t.noubin_key_normalised);
    assert_eq!(
        classify_type(
            &track,
            fs_release_resolver(&root.join(&b.item.noudata_relpath))
        )
        .ty,
        NoudataType::LinkTrack
    );

    // The album keeps its key; both keys resolve, each to its own file.
    assert_eq!(key_of(&root, ALBUM), Some(album_key.clone()));
    assert_eq!(holders(&root, &album_key), [ALBUM]);
    assert_eq!(
        holders(&root, t.noubin_key_normalised.as_ref().unwrap()),
        ["Music/Band/Album/01 - One.noudata"]
    );
    // Indexed as a track (listed once, through its album), not a playlist.
    let tracks = svc.list_tracks(LibraryKind::Track).unwrap();
    assert_eq!(tracks.iter().filter(|t| t.title == "One").count(), 1, "{tracks:?}");
    let playlists = svc.list_collections(Some(LibraryKind::Playlist)).unwrap();
    assert!(!playlists.iter().any(|c| c.title == "One"), "{playlists:?}");
}

#[test]
fn associate_track_reuses_existing_per_track_file() {
    let (_t, root) = kinds_library();
    let mut svc = service_with(&root, rewritable(None));
    let album = id_of(&mut svc, LibraryKind::Album, "The Album");
    let before = count_noudata(&root);

    // Fixture already has `02 - Two.noudata` with key two.noubin.bbb222.
    let o = ok(svc.associate_track(album, 1, false));
    let b = o.bound.as_ref().unwrap();
    assert!(!b.created_track_file, "reused, not duplicated");
    assert_eq!(b.item.noudata_relpath, "Music/Band/Album/02 - Two.noudata");
    assert_eq!(count_noudata(&root), before);
    assert_eq!(o.replaced_key.as_deref(), Some("two.noubin.bbb222"));
    assert_eq!(
        key_of(&root, "Music/Band/Album/02 - Two.noudata"),
        o.noubin_key_normalised
    );
    // Associating the same track again keeps a single file.
    ok(svc.associate_track(album, 1, false));
    assert_eq!(count_noudata(&root), before);
}

#[test]
fn associate_track_rejects_bad_targets_and_creates_nothing() {
    let (_t, root) = kinds_library();
    let mut svc = service_with(&root, rewritable(None));
    let album = id_of(&mut svc, LibraryKind::Album, "The Album");
    let playlist = id_of(&mut svc, LibraryKind::Playlist, "Rainy Day");
    let before = count_noudata(&root);
    let playlist_before = fs::read(root.join(PLAYLIST)).unwrap();

    assert!(matches!(
        svc.associate_track(album, 99, false),
        Err(ServerError::Invalid(_))
    ));
    assert!(matches!(
        svc.associate_track(playlist, 0, false),
        Err(ServerError::Invalid(_))
    ));
    assert_eq!(count_noudata(&root), before);
    assert_eq!(fs::read(root.join(PLAYLIST)).unwrap(), playlist_before);
}

// ---------------------------------------------------------------------------
// Rewritable tag
// ---------------------------------------------------------------------------

#[test]
fn associate_rewritable_reassociation_keeps_old_items_key() {
    let (_t, root) = kinds_library();
    let mut svc = service_with(&root, tag_with("tag.noubin.one", None));
    let album = id_of(&mut svc, LibraryKind::Album, "The Album");
    let playlist = id_of(&mut svc, LibraryKind::Playlist, "Rainy Day");
    ok(svc.associate_playable(album));
    assert_eq!(key_of(&root, ALBUM).as_deref(), Some("tag.noubin.one"));

    // The same physical tag is now rewritten for the playlist; it held the album's key.
    let mut svc = service_with(&root, tag_with("tag.noubin.two", Some("tag.noubin.one")));
    let o = ok(svc.associate_playable(playlist));
    assert_eq!(o.previous_titles, ["The Album"]);
    assert_eq!(o.previous_items.len(), 1);
    assert!(
        o.cleared.is_empty(),
        "rewritable tag never clears other items"
    );
    assert_eq!(
        key_of(&root, ALBUM).as_deref(),
        Some("tag.noubin.one"),
        "old item keeps its key"
    );
    assert_eq!(key_of(&root, PLAYLIST).as_deref(), Some("tag.noubin.two"));
    assert!(o.warnings.is_empty());
    assert!(!o.tag_locked);
}

#[test]
fn associate_blank_rewritable_tag_reports_no_previous() {
    let (_t, root) = kinds_library();
    let mut svc = service_with(&root, rewritable(None));
    let playlist = id_of(&mut svc, LibraryKind::Playlist, "Rainy Day");
    let o = ok(svc.associate_playable(playlist));
    assert!(o.previous_items.is_empty() && o.previous_titles.is_empty());
    assert!(o.noubin_key.as_deref().unwrap().contains(".noubin."));
    assert_eq!(o.bound.unwrap().item.title, "Rainy Day");
}

#[test]
fn associate_duplicate_key_warns_but_succeeds() {
    let (_t, root) = kinds_library();
    // The tag ends up carrying the key `Album.noubin.AAA111`, which the album already holds.
    let mut svc = service_with(&root, tag_with("Album.noubin.AAA111", None));
    let playlist = id_of(&mut svc, LibraryKind::Playlist, "Rainy Day");
    let o = ok(svc.associate_playable(playlist));
    assert_eq!(o.warnings.len(), 1, "{:?}", o.warnings);
    assert_eq!(o.warnings[0].code, WARN_DUPLICATE_KEY);
    assert_eq!(o.warnings[0].items.len(), 1);
    assert_eq!(o.warnings[0].items[0].title, "The Album");
    assert!(o.cleared.is_empty());
    let both = holders(&root, "album.noubin.aaa111");
    assert_eq!(both.len(), 2, "multi-match is indexed: {both:?}");
}

// ---------------------------------------------------------------------------
// Locked tag
// ---------------------------------------------------------------------------

#[test]
fn associate_locked_tag_moves_the_key() {
    let (_t, root) = kinds_library();
    let mut svc = service_with(&root, locked("Album.noubin.AAA111"));
    let playlist = id_of(&mut svc, LibraryKind::Playlist, "Rainy Day");
    assert_eq!(key_of(&root, ALBUM).as_deref(), Some("album.noubin.aaa111"));

    let o = ok(svc.associate_playable(playlist));
    assert!(o.tag_locked);
    assert_eq!(
        o.noubin_key.as_deref(),
        Some("Album.noubin.AAA111"),
        "literal key bound"
    );
    assert_eq!(o.cleared.len(), 1);
    assert_eq!(o.cleared[0].title, "The Album");
    assert!(o.warnings.is_empty());
    let album = doc(&root, ALBUM);
    assert!(album.noubin_key.is_none() && album.noubin_key_normalised.is_none());
    assert!(
        album.release_data.is_some(),
        "rest of the album file untouched"
    );
    assert_eq!(
        key_of(&root, PLAYLIST).as_deref(),
        Some("album.noubin.aaa111")
    );
    assert_eq!(holders(&root, "album.noubin.aaa111"), [PLAYLIST]);
}

#[test]
fn associate_keep_existing_flag_uses_move_semantics_too() {
    let (_t, root) = kinds_library();
    // `keep_existing_key` request, response without an explicit `tag_locked` (older UI).
    let mut svc = service_with(
        &root,
        Box::new(|req| AssociateResponse {
            request_id: req.request_id,
            ok: true,
            noubin_key: Some("Two.noubin.BBB222".into()),
            noubin_key_normalised: Some("two.noubin.bbb222".into()),
            ..Default::default()
        }),
    );
    let playlist = id_of(&mut svc, LibraryKind::Playlist, "Rainy Day");
    let o = ok(svc.associate_playable_keep_existing(playlist));
    assert!(o.tag_locked);
    assert_eq!(o.cleared.len(), 1);
    assert!(key_of(&root, "Music/Band/Album/02 - Two.noudata").is_none());
    assert_eq!(holders(&root, "two.noubin.bbb222"), [PLAYLIST]);
}

#[test]
fn associate_locked_tag_can_move_between_a_track_and_its_album() {
    let (_t, root) = kinds_library();
    let mut svc = service_with(&root, locked("Two.noubin.BBB222"));
    let album = id_of(&mut svc, LibraryKind::Album, "The Album");
    // The per-track file `02 - Two.noudata` holds the key; move it to the album itself.
    let o = ok(svc.associate_playable(album));
    assert_eq!(o.cleared.len(), 1);
    assert_eq!(o.cleared[0].kind, "track");
    assert_eq!(holders(&root, "two.noubin.bbb222"), [ALBUM]);
}

#[test]
fn associate_locked_blank_tag_fails_and_modifies_nothing() {
    let (_t, root) = kinds_library();
    let playlist_before = fs::read(root.join(PLAYLIST)).unwrap();
    let album_before = fs::read(root.join(ALBUM)).unwrap();

    // UI-reported failure (locked, nothing readable).
    let msg = "This tag is locked (read-only) and has no readable Noubin key";
    let mut svc = service_with(
        &root,
        Box::new(move |req| AssociateResponse {
            request_id: req.request_id,
            ok: false,
            error: Some(msg.into()),
            ..Default::default()
        }),
    );
    let playlist = id_of(&mut svc, LibraryKind::Playlist, "Rainy Day");
    let album = id_of(&mut svc, LibraryKind::Album, "The Album");
    let o = svc.associate_playable(playlist).unwrap();
    assert!(!o.ok);
    assert!(o.error.as_deref().unwrap().contains("locked"));
    let o = svc.associate_track(album, 0, false).unwrap();
    assert!(!o.ok);

    // Locked, key reported blank.
    let mut svc = service_with(
        &root,
        Box::new(|req| AssociateResponse {
            request_id: req.request_id,
            ok: true,
            noubin_key: Some("  ".into()),
            noubin_key_normalised: Some(String::new()),
            tag_locked: true,
            ..Default::default()
        }),
    );
    let o = svc.associate_playable(playlist).unwrap();
    assert!(!o.ok && o.error.as_deref().unwrap().contains("blank"));
    let o = svc.associate_track(album, 0, false).unwrap();
    assert!(!o.ok);

    assert_eq!(fs::read(root.join(PLAYLIST)).unwrap(), playlist_before);
    assert_eq!(fs::read(root.join(ALBUM)).unwrap(), album_before);
    assert!(
        !root.join("Music/Band/Album/01 - One.noudata").exists(),
        "no per-track file for a failed associate"
    );
}

#[test]
fn associate_tiny_capacity_failure_is_reported_untouched() {
    let (_t, root) = kinds_library();
    let before = fs::read(root.join(PLAYLIST)).unwrap();
    let mut svc = service_with(
        &root,
        ServerService::failing_mock_associate("tag capacity too small for association"),
    );
    let playlist = id_of(&mut svc, LibraryKind::Playlist, "Rainy Day");
    let o = svc.associate_playable(playlist).unwrap();
    assert!(!o.ok);
    assert!(o.error.unwrap().contains("capacity"));
    assert_eq!(fs::read(root.join(PLAYLIST)).unwrap(), before);
}

#[test]
fn associate_rejects_oversized_or_hostile_tag_keys() {
    let (_t, root) = kinds_library();
    let before = fs::read(root.join(PLAYLIST)).unwrap();
    let long: &'static str = Box::leak("k".repeat(600).into_boxed_str());
    let mut svc = service_with(&root, locked(long));
    let playlist = id_of(&mut svc, LibraryKind::Playlist, "Rainy Day");
    let o = svc.associate_playable(playlist).unwrap();
    assert!(!o.ok && o.error.unwrap().contains("too long"));
    assert_eq!(fs::read(root.join(PLAYLIST)).unwrap(), before);

    // SQL metacharacters / path-like keys are only ever data: bound, indexed, resolvable.
    let nasty = "x'); DROP TABLE playables;--/../../etc/passwd";
    let mut svc = service_with(&root, locked(nasty));
    ok(svc.associate_playable(playlist));
    assert_eq!(
        holders(&root, &normalise_noubin_key(nasty)),
        [PLAYLIST],
        "index intact"
    );
}

// ---------------------------------------------------------------------------
// Every playable kind
// ---------------------------------------------------------------------------

#[test]
fn associate_binds_keys_on_every_collection_kind_without_track_files() {
    let (_t, root) = kinds_library();
    let mut svc = service_with(&root, rewritable(None));
    let targets = [
        (LibraryKind::Album, "The Album"),
        (LibraryKind::Playlist, "Rainy Day"),
        (LibraryKind::Mix, "Mix"),
        (LibraryKind::Audiobook, "The Hobbit"),
        (LibraryKind::Podcast, "Show"),
    ];
    let before = count_noudata(&root);
    for (kind, title) in targets {
        let id = match svc
            .list_collections(Some(kind))
            .unwrap()
            .into_iter()
            .find(|c| c.title.contains(title) || title.contains(&c.title))
        {
            Some(c) => c.id,
            None => continue,
        };
        let o = ok(svc.associate_playable(id));
        let b = o.bound.unwrap();
        assert_eq!(b.item.kind, kind.as_str());
        assert!(!b.created_track_file && b.track_item.is_none());
        assert_eq!(
            key_of(&root, &b.item.noudata_relpath),
            o.noubin_key_normalised
        );
    }
    assert_eq!(count_noudata(&root), before);
    // Orphans still cannot be associated (no .noudata yet).
    let orphan = svc
        .list_collections(Some(LibraryKind::Track))
        .unwrap()
        .into_iter()
        .find(|c| c.is_orphan)
        .unwrap();
    assert!(svc.associate_playable(orphan.id).is_err());
}

#[test]
fn associate_tapping_an_associated_playlist_resolves_to_its_collection() {
    // Mock-level check of "tap loads via LoadCollection": the key resolves to the playlist
    // playable (the engine's tap path loads any indexed playable as a collection — see
    // `engine_nowplaying::playlist_over_two_releases_resolves_metadata_per_track`).
    let (_t, root) = kinds_library();
    let mut svc = service_with(&root, rewritable(None));
    let playlist = id_of(&mut svc, LibraryKind::Playlist, "Rainy Day");
    let o = ok(svc.associate_playable(playlist));
    let hits = LibraryReader::open(&LibraryRoot::new(&root))
        .unwrap()
        .find_by_key(o.noubin_key_normalised.as_deref().unwrap())
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].kind, LibraryKind::Playlist);
    assert!(!hits[0].media_relpaths.is_empty());
}

// ---------------------------------------------------------------------------
// Ordering / HTTP
// ---------------------------------------------------------------------------

#[test]
fn associate_writes_noudata_only_after_a_successful_tag_response() {
    let (_t, root) = kinds_library();
    let seen: Arc<Mutex<Vec<(bool, bool)>>> = Arc::default();
    let seen2 = Arc::clone(&seen);
    let root2 = root.clone();
    // While the "tag" is being written, neither the playlist nor the per-track file changed.
    let mut svc = service_with(
        &root,
        Box::new(move |req| {
            let track_file = root2.join("Music/Band/Album/01 - One.noudata").exists();
            let playlist_keyed = fs::read_to_string(root2.join(PLAYLIST))
                .unwrap()
                .contains("noubinKey");
            seen2.lock().unwrap().push((track_file, playlist_keyed));
            AssociateResponse {
                request_id: req.request_id,
                ok: true,
                noubin_key: Some(req.noubin_key.clone()),
                noubin_key_normalised: Some(normalise_noubin_key(&req.noubin_key)),
                ..Default::default()
            }
        }),
    );
    let album = id_of(&mut svc, LibraryKind::Album, "The Album");
    let playlist = id_of(&mut svc, LibraryKind::Playlist, "Rainy Day");
    ok(svc.associate_track(album, 0, false));
    ok(svc.associate_playable(playlist));
    // (per-track file exists, playlist already keyed) at the moment of each tag write.
    assert_eq!(*seen.lock().unwrap(), [(false, false), (true, false)]);
}

fn http_post(port: u16, path: &str) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (
        status,
        serde_json::from_slice(&raw[split + 4..]).unwrap_or_default(),
    )
}

#[test]
fn associate_http_item_parameter_and_response_shape() {
    let (_t, root) = kinds_library();
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut svc = service_with(&root, rewritable(None));
    let album = id_of(&mut svc, LibraryKind::Album, "The Album");
    let svc = Arc::new(Mutex::new(svc));
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
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

    let (st, v) = http_post(port, &format!("/api/playables/{album}/associate?item=0"));
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["bound"]["kind"], "track");
    assert_eq!(v["bound"]["created_track_file"], true);
    assert_eq!(v["bound"]["track_item"], 0);
    assert!(v["bound"]["id"].is_string());
    for f in ["previous_titles", "previous_items", "cleared", "warnings"] {
        assert!(v[f].is_array(), "{f} missing in {v}");
    }
    assert_eq!(v["tag_locked"], false);
    assert!(root.join("Music/Band/Album/01 - One.noudata").is_file());

    let (st, _) = http_post(port, &format!("/api/playables/{album}/associate?item=abc"));
    assert_eq!(st, 400);
    let (st, _) = http_post(port, &format!("/api/playables/{album}/associate?item=99"));
    assert_eq!(st, 400);
    let (st, _) = http_post(port, "/api/playables/123456/associate?item=0");
    assert_eq!(st, 404);
}
