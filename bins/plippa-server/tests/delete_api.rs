//! BCS_10: library delete — preview then confirm, path safety, playlists, keys and the
//! delete → re-add acceptance loop.
//!
//! Fixtures are built by hand: empty placeholder audio files, nothing here decodes audio.

mod common;

use plippa_server::{
    CreatePlaylist, DeleteError, DeleteOutcome, DeleteRequest, IdArg, RecordingIpc, ServerConfig,
    ServerService, TrackRef,
};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const ALBUM: &str = r#"{
    "_whatAmILookingAt": "This is a noudata file from the Noubin standard.",
    "noudataVersion": "0.4.0",
    "noudataType": "release",
    "noubinKey": "https://example.test/album/",
    "noubinKeyNormalised": "example.test/album/",
    "playableItemTitle": "The Album",
    "playableItemFolderSafeTitle": "The Album",
    "playableItemProfile": "music",
    "releaseData": {
        "coverImage": {"images": [{"imageLocalFilename": "cover.jpg"}]},
        "credits": {"primaryArtists": [{"primaryArtistName": "The Band"}]}
    },
    "mediaList": [
        {"mediaTitle": "One", "localFilePaths": ["one.mp3"]},
        {"mediaTitle": "Two", "localFilePaths": ["two.mp3"]}
    ]
}"#;

const ALBUM_DIR: &str = "Music/The Band/The Album";
const ALBUM_NOUDATA: &str = "Music/The Band/The Album/_The Album.noudata";

struct Rig {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    svc: ServerService,
    ipc: RecordingIpc,
}

fn service(root: PathBuf, ipc: RecordingIpc) -> ServerService {
    ServerService::start(
        Some(root),
        Box::new(ipc.clone()),
        ServerService::default_mock_associate(),
    )
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// A two-track album with a cover image and its generated thumbnails.
fn album_rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    write(&root.join(ALBUM_NOUDATA), ALBUM.as_bytes());
    write(&root.join(ALBUM_DIR).join("one.mp3"), b"AAAA");
    write(&root.join(ALBUM_DIR).join("two.mp3"), b"BBBBB");
    write(&root.join(ALBUM_DIR).join("cover.jpg"), b"COVER");
    for px in [128u32, 512, 1024] {
        write(
            &root.join(ALBUM_DIR).join("thumbnails").join(format!("cover-{px}.jpg")),
            b"THUMB",
        );
    }
    let ipc = RecordingIpc::new();
    let svc = service(root.clone(), ipc.clone());
    Rig {
        _tmp: tmp,
        root,
        svc,
        ipc,
    }
}

/// An empty library with one release written from `doc` into `dir` (relative to the root).
fn release_rig(dir: &str, doc: &str, files: &[(&str, &[u8])]) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    let name = dir.rsplit('/').next().unwrap_or(dir);
    let rel = if dir.is_empty() {
        format!("_{name}.noudata")
    } else {
        format!("{dir}/_{name}.noudata")
    };
    write(&root.join(&rel), doc.as_bytes());
    for (f, bytes) in files {
        write(&root.join(dir).join(f), bytes);
    }
    let ipc = RecordingIpc::new();
    let svc = service(root.clone(), ipc.clone());
    Rig {
        _tmp: tmp,
        root,
        svc,
        ipc,
    }
}

fn id_of(r: &mut Rig, title: &str) -> u64 {
    r.svc
        .list_collections(None)
        .unwrap()
        .into_iter()
        .find(|c| c.title == title)
        .unwrap_or_else(|| panic!("no collection titled {title}"))
        .id
}

fn force() -> DeleteRequest {
    DeleteRequest {
        force: true,
        reason: Some("test".into()),
    }
}

fn code_of(e: &DeleteError) -> &'static str {
    e.code()
}

fn expect_refused(r: Result<DeleteOutcome, DeleteError>, code: &str) -> DeleteError {
    match r {
        Ok(_) => panic!("expected a refusal with code {code}"),
        Err(e) => {
            assert_eq!(code_of(&e), code, "{}", e.message());
            e
        }
    }
}

/// `(relpath, size, mtime_ns)` of every file outside `.plippa/` (the index's own WAL is the
/// database's business; the library content is what "pure" is about).
fn snapshot(root: &Path) -> BTreeMap<String, (u64, i64)> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, (u64, i64)>) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let rel = e
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel == ".plippa" || rel.starts_with(".plippa/") {
                continue;
            }
            let meta = fs::symlink_metadata(e.path()).unwrap();
            if meta.is_dir() {
                walk(&e.path(), root, out);
            } else {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as i64)
                    .unwrap_or(0);
                out.insert(rel, (meta.len(), mtime));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Preview
// ---------------------------------------------------------------------------

#[test]
fn delete_preview_lists_files_and_blockers_without_touching_anything() {
    let mut r = album_rig();
    let id = id_of(&mut r, "The Album");
    let before = snapshot(&r.root);

    let p = r.svc.preview_delete_playable(id).unwrap();

    assert_eq!(
        snapshot(&r.root),
        before,
        "a preview must not modify the library"
    );
    assert_eq!(p.playable.title.as_deref(), Some("The Album"));
    assert!(!p.playing);
    assert!(p.blockers.is_empty(), "{:?}", p.blockers);
    assert_eq!(p.noudata.as_deref(), Some(ALBUM_NOUDATA));
    assert_eq!(p.folder.as_deref(), Some(ALBUM_DIR));
    assert_eq!(p.keys, vec!["example.test/album/".to_string()]);

    let mut files: Vec<&str> = p.files.iter().map(|f| f.path.as_str()).collect();
    files.sort();
    assert_eq!(
        files,
        vec![
            "Music/The Band/The Album/one.mp3",
            "Music/The Band/The Album/two.mp3"
        ]
    );
    assert_eq!(p.files.iter().map(|f| f.bytes).sum::<u64>(), 9);
    assert!(p.files.iter().all(|f| !f.missing));

    let mut covers: Vec<&str> = p.covers.iter().map(|c| c.path.as_str()).collect();
    covers.sort();
    assert_eq!(
        covers,
        vec![
            "Music/The Band/The Album/cover.jpg",
            "Music/The Band/The Album/thumbnails/cover-1024.jpg",
            "Music/The Band/The Album/thumbnails/cover-128.jpg",
            "Music/The Band/The Album/thumbnails/cover-512.jpg",
        ]
    );
    let noudata_bytes = fs::metadata(r.root.join(ALBUM_NOUDATA)).unwrap().len();
    assert_eq!(p.total_bytes, 9 + 20 + noudata_bytes, "media + covers + .noudata");
}

#[test]
fn delete_requires_force_and_returns_the_preview_first() {
    let mut r = album_rig();
    let id = id_of(&mut r, "The Album");
    let before = snapshot(&r.root);

    let e = expect_refused(
        r.svc.delete_playable(id, DeleteRequest::default()),
        "confirm_required",
    );
    let preview = e.preview().expect("the refusal carries the preview");
    assert_eq!(preview.files.len(), 2);
    assert!(preview.total_bytes > 0);
    assert_eq!(
        snapshot(&r.root),
        before,
        "a refused delete must not touch anything"
    );

    let out = r.svc.delete_playable(id, force()).unwrap();
    assert!(out.ok && out.index_updated && !out.partial);
    assert!(!r.root.join(ALBUM_NOUDATA).exists());
}

// ---------------------------------------------------------------------------
// Whole release
// ---------------------------------------------------------------------------

#[test]
fn delete_collection_removes_noudata_media_and_covers() {
    let mut r = album_rig();
    let id = id_of(&mut r, "The Album");
    let noudata_bytes = fs::metadata(r.root.join(ALBUM_NOUDATA)).unwrap().len();

    // The collections route is the same contract as the playables one.
    let out = r.svc.delete_collection(id, force()).unwrap();
    assert!(out.ok);
    assert!(out.index_updated);
    assert!(!out.partial, "{:?}", out.remaining);
    assert!(out.folder_removed, "{:?}", out.warnings);
    assert_eq!(out.freed_bytes, 9 + 20 + noudata_bytes);
    assert!(out
        .deleted
        .contains(&ALBUM_NOUDATA.to_string()));

    assert!(!r.root.join(ALBUM_NOUDATA).exists());
    assert!(!r.root.join(ALBUM_DIR).join("one.mp3").exists());
    assert!(!r.root.join(ALBUM_DIR).join("two.mp3").exists());
    assert!(!r.root.join(ALBUM_DIR).join("cover.jpg").exists());
    assert!(!r.root.join(ALBUM_DIR).join("thumbnails").exists());
    assert!(!r.root.join(ALBUM_DIR).exists(), "the empty folder goes too");
    assert!(
        r.root.join("Music/The Band").exists(),
        "only the release folder is removed"
    );
    assert!(r.svc.list_collections(None).unwrap().is_empty());
}

#[test]
fn delete_then_rescan_leaves_no_index_rows() {
    let mut r = album_rig();
    let id = id_of(&mut r, "The Album");
    r.svc.delete_playable(id, force()).unwrap();
    r.svc.lazy_rescan().unwrap();
    r.svc.full_scan().unwrap();

    assert!(
        r.svc.list_playables().unwrap().is_empty(),
        "no row may survive for the deleted release or its media"
    );
    assert!(r.svc.list_collections(None).unwrap().is_empty());
    assert!(r.svc.list_tracks(plippa_noubin::LibraryKind::Track).unwrap().is_empty());
    // The id is gone.
    assert!(matches!(
        r.svc.get_playable(id),
        Err(plippa_server::ServerError::NotFound(_))
    ));

    // ---- re-add the same release: it indexes and its tracks are playable ----
    write(&r.root.join(ALBUM_NOUDATA), ALBUM.as_bytes());
    write(&r.root.join(ALBUM_DIR).join("one.mp3"), b"AAAA");
    write(&r.root.join(ALBUM_DIR).join("two.mp3"), b"BBBBB");
    r.svc.lazy_rescan().unwrap();

    let again = id_of(&mut r, "The Album");
    let detail = r.svc.get_collection(again).unwrap();
    assert_eq!(detail.items.len(), 2);
    assert!(detail.items.iter().all(|i| i.available));
    assert_eq!(detail.items[0].title, "One");
    assert_eq!(detail.items[1].title, "Two");
}

// ---------------------------------------------------------------------------
// Safety
// ---------------------------------------------------------------------------

#[test]
fn delete_refuses_a_symlinked_file_and_a_symlinked_folder() {
    // (1) a media file that is a symlink is never followed (and never deleted through).
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.mp3");
    fs::write(&secret, b"SECRET").unwrap();
    let mut r = release_rig(
        ALBUM_DIR,
        &ALBUM.replace("two.mp3", "link.mp3"),
        &[("one.mp3", b"AAAA")],
    );
    std::os::unix::fs::symlink(&secret, r.root.join(ALBUM_DIR).join("link.mp3")).unwrap();
    // Rescan so the index knows about the symlinked media.
    r.svc.lazy_rescan().unwrap();
    let id = id_of(&mut r, "The Album");

    let p = r.svc.preview_delete_playable(id).unwrap();
    assert!(
        p.blockers.iter().any(|b| b.kind == "symlink"),
        "{:?}",
        p.blockers
    );
    let e = expect_refused(r.svc.delete_playable(id, force()), "blocked");
    assert!(e.preview().is_some());
    assert_eq!(fs::read(&secret).unwrap(), b"SECRET", "outside file untouched");
    assert!(r.root.join(ALBUM_NOUDATA).exists(), "nothing was deleted");
    assert!(r.root.join(ALBUM_DIR).join("one.mp3").exists());

    // (2) a symlinked folder inside the library is not traversed. It sits inside the release
    // folder, so the path itself is legal and only the symlink check can catch it.
    let outside2 = tempfile::tempdir().unwrap();
    fs::write(outside2.path().join("gone.mp3"), b"KEEP").unwrap();
    let doc = ALBUM.replace("two.mp3", "linked/two.mp3");
    let mut r2 = release_rig(ALBUM_DIR, &doc, &[("one.mp3", b"AAAA")]);
    std::os::unix::fs::symlink(outside2.path(), r2.root.join(ALBUM_DIR).join("linked")).unwrap();
    r2.svc.lazy_rescan().unwrap();
    let id2 = id_of(&mut r2, "The Album");

    let p2 = r2.svc.preview_delete_playable(id2).unwrap();
    assert!(
        p2.blockers.iter().any(|b| b.kind == "symlink"),
        "{:?}",
        p2.blockers
    );
    expect_refused(r2.svc.delete_playable(id2, force()), "blocked");
    assert_eq!(
        fs::read(outside2.path().join("gone.mp3")).unwrap(),
        b"KEEP",
        "a symlinked folder is never written through"
    );
    assert!(r2.root.join(ALBUM_DIR).join("one.mp3").exists());
}

#[test]
fn delete_refuses_paths_outside_the_library_root() {
    // A hostile `.noudata` cannot reach outside the root: `..`, the index folder and the
    // library root itself are all refused, and nothing is deleted.
    let escape = r#"{
        "noudataType": "release",
        "releaseData": {"credits": {"primaryArtists": [{"primaryArtistName": "Mallory"}]}},
        "playableItemTitle": "Evil",
        "mediaList": [
            {"mediaTitle": "a", "localFilePaths": ["../../etc/passwd"]},
            {"mediaTitle": "b", "localFilePaths": ["one.mp3"]}
        ]
    }"#;
    let mut r = release_rig("Evil", escape, &[("one.mp3", b"RR")]);
    let id = id_of(&mut r, "Evil");
    let before = snapshot(&r.root);

    let p = r.svc.preview_delete_playable(id).unwrap();
    assert!(p.files.iter().all(|f| plippa_noubin::is_safe_relative_path(&f.path)));
    assert!(p.files.iter().all(|f| !f.path.contains("..")));
    let unsafe_paths: Vec<&str> = p
        .blockers
        .iter()
        .filter(|b| b.kind == "unsafe_path")
        .map(|b| b.detail.as_str())
        .collect();
    assert_eq!(unsafe_paths.len(), 1, "{:?}", p.blockers);
    assert!(unsafe_paths[0].contains("../../etc/passwd"));
    assert_eq!(
        p.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        vec!["Evil/one.mp3"],
        "the legal entry is still planned; the hostile one never is"
    );

    expect_refused(r.svc.delete_playable(id, force()), "blocked");
    assert_eq!(snapshot(&r.root), before, "nothing was removed");
    assert!(Path::new("/etc/passwd").exists());
    assert!(r.root.join("Evil/one.mp3").exists());

    // The index database and the library root itself are not addressable either, even from a
    // `.noudata` that sits in the root.
    let root_doc = r#"{
        "noudataType": "looseTrack",
        "playableItemTitle": "Root-ish",
        "mediaList": [{"mediaTitle": "x", "localFilePaths": [".plippa/index.db"]}]
    }"#;
    let mut r2 = release_rig("", root_doc, &[("solo.mp3", b"SS")]);
    let id2 = id_of(&mut r2, "Root-ish");
    let before2 = snapshot(&r2.root);
    let p2 = r2.svc.preview_delete_playable(id2).unwrap();
    assert!(
        p2.blockers
            .iter()
            .any(|b| b.kind == "unsafe_path" && b.detail.contains(".plippa")),
        "{:?}",
        p2.blockers
    );
    expect_refused(r2.svc.delete_playable(id2, force()), "blocked");
    assert_eq!(snapshot(&r2.root), before2, "nothing was removed");
    assert!(r2.root.join(".plippa").is_dir());

    // `"."` (the root) is refused with its own kind so the UI can say why.
    let dot = r#"{
        "noudataType": "looseTrack",
        "playableItemTitle": "Dot",
        "mediaList": [{"mediaTitle": "x", "localFilePaths": ["."]}]
    }"#;
    let mut r3 = release_rig("", dot, &[("solo.mp3", b"SS")]);
    let id3 = id_of(&mut r3, "Dot");
    let p3 = r3.svc.preview_delete_playable(id3).unwrap();
    assert!(
        p3.blockers.iter().any(|b| b.kind == "root"),
        "{:?}",
        p3.blockers
    );
    expect_refused(r3.svc.delete_playable(id3, force()), "blocked");
    assert!(r3.root.join("solo.mp3").exists());
}

#[test]
fn delete_refuses_while_playing() {
    use plippa_core::{PlayableItemId, SessionState, TransportState};
    use plippa_ipc_proto::PlaybackStatus;

    let mut r = album_rig();
    let id = id_of(&mut r, "The Album");
    r.ipc.inject_playback(PlaybackStatus::from_session_rev(
        SessionState {
            transport: TransportState::Playing,
            current_item: Some(PlayableItemId::new(id)),
            ..SessionState::default()
        },
        1,
    ));

    let p = r.svc.preview_delete_playable(id).unwrap();
    assert!(p.playing);
    assert!(p.blockers.iter().any(|b| b.kind == "playing"));

    let e = expect_refused(r.svc.delete_playable(id, force()), "playing");
    assert!(e.preview().expect("preview").playing);
    assert!(r.root.join(ALBUM_NOUDATA).exists());
    assert!(r.root.join(ALBUM_DIR).join("one.mp3").exists());
}

// ---------------------------------------------------------------------------
// Single file
// ---------------------------------------------------------------------------

#[test]
fn delete_single_file_keeps_the_release_and_refuses_the_last_file() {
    let mut r = album_rig();
    let id = id_of(&mut r, "The Album");

    let p = r.svc.preview_delete_playable_file(id, 0).unwrap();
    assert_eq!(p.playable.item, Some(0));
    assert_eq!(
        p.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        vec!["Music/The Band/The Album/one.mp3"]
    );
    assert!(p.covers.is_empty(), "a single-file delete keeps the art");

    let out = r.svc.delete_playable_file(id, 0, force()).unwrap();
    assert!(out.ok && !out.partial);
    assert!(!r.root.join(ALBUM_DIR).join("one.mp3").exists());
    assert!(r.root.join(ALBUM_DIR).join("two.mp3").exists());
    assert!(r.root.join(ALBUM_NOUDATA).exists());
    assert!(r.root.join(ALBUM_DIR).join("cover.jpg").exists());

    let detail = r.svc.get_collection(id).unwrap();
    assert_eq!(detail.items.len(), 1);
    assert_eq!(detail.items[0].title, "Two");
    assert!(detail.items[0].available);

    // The document on disk no longer lists the removed entry.
    let bytes = fs::read(r.root.join(ALBUM_NOUDATA)).unwrap();
    let (doc, warnings) = plippa_noubin::parse_noudata_with_warnings(&bytes).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(doc.media_list.len(), 1);
    assert_eq!(doc.media_list[0].media_title.as_deref(), Some("Two"));

    // The last media file cannot be removed on its own.
    let e = expect_refused(r.svc.delete_playable_file(id, 0, force()), "last_file");
    assert!(e.message().contains("delete the whole item"));
    assert!(r.root.join(ALBUM_DIR).join("two.mp3").exists());

    // ...but the whole item can.
    let out = r.svc.delete_playable(id, force()).unwrap();
    assert!(out.ok);
    assert!(!r.root.join(ALBUM_NOUDATA).exists());
}

// ---------------------------------------------------------------------------
// Keys and playlists
// ---------------------------------------------------------------------------

#[test]
fn delete_releases_noubin_keys_and_a_later_tap_is_a_library_miss() {
    let mut r = album_rig();
    let key = "https://example.test/album/";
    assert!(
        r.svc.has_local_match(key),
        "fixture precondition: the key resolves"
    );
    let id = id_of(&mut r, "The Album");
    let p = r.svc.preview_delete_playable(id).unwrap();
    assert_eq!(p.keys, vec!["example.test/album/".to_string()]);

    r.svc.delete_playable(id, force()).unwrap();

    assert!(
        !r.svc.has_local_match(key),
        "the key must stop resolving, so a tap is an unmatched-noubin miss"
    );
    assert!(r
        .svc
        .list_playables()
        .unwrap()
        .iter()
        .all(|p| p.noubin_key_normalised.as_deref() != Some("example.test/album/")));
}

#[test]
fn delete_scrubs_playlist_items_and_reports_which_playlists() {
    let mut r = album_rig();
    let album = id_of(&mut r, "The Album");
    let playlist = r
        .svc
        .create_playlist(CreatePlaylist {
            title: "Favourites".into(),
            tracks: vec![
                TrackRef::Item {
                    track_id: IdArg(album),
                    item: Some(0),
                },
                TrackRef::Item {
                    track_id: IdArg(album),
                    item: Some(1),
                },
            ],
            created_by: None,
        })
        .unwrap()
        .summary
        .id;

    let p = r.svc.preview_delete_playable(album).unwrap();
    assert_eq!(p.playlists.len(), 2, "{:?}", p.playlists);
    assert_eq!(p.playlists[0].id, playlist.to_string());
    assert_eq!(p.playlists[0].title, "Favourites");
    assert_eq!(
        p.playlists.iter().map(|h| h.item_position).collect::<Vec<_>>(),
        vec![0, 1]
    );

    let out = r.svc.delete_playable(album, force()).unwrap();
    assert_eq!(out.playlists_edited, vec![playlist.to_string()]);

    // The playlist itself survives, valid, with the deleted entries removed.
    let after = r.svc.get_collection(playlist).unwrap();
    assert!(after.items.is_empty(), "{:?}", after.items);
    let rel = r
        .svc
        .list_playables()
        .unwrap()
        .into_iter()
        .find(|p| p.id == playlist)
        .unwrap()
        .noudata_relpath;
    let bytes = fs::read(r.root.join(&rel)).unwrap();
    let (doc, warnings) = plippa_noubin::parse_noudata_with_warnings(&bytes).unwrap();
    // A playlist that lost its last track is a known "empty user playlist": the only warning
    // allowed is the one saying so. Anything else means the scrub corrupted the document.
    assert!(
        warnings.iter().all(|w| w.path == "mediaList"),
        "the scrub must leave a readable playlist: {warnings:?}"
    );
    assert!(doc.media_list.is_empty());
    assert_eq!(doc.playable_item_title.as_deref(), Some("Favourites"));
}

#[test]
fn delete_keeps_the_other_items_of_a_playlist_playable() {
    // A playlist that mixes tracks from two releases: deleting one release leaves the other
    // track in place, still available, with the playlist document valid.
    let mut r = album_rig();
    write(
        &r.root.join("Music/Other/_Single.noudata"),
        br#"{"noudataType": "looseTrack", "playableItemTitle": "Keep Me",
             "mediaList": [{"mediaTitle": "Keep Me", "localFilePaths": ["keep.mp3"]}]}"#,
    );
    write(&r.root.join("Music/Other/keep.mp3"), b"KEEP");
    r.svc.lazy_rescan().unwrap();
    let album = id_of(&mut r, "The Album");
    let keep = r
        .svc
        .list_collections(None)
        .unwrap()
        .into_iter()
        .find(|c| c.title == "Keep Me")
        .expect("the loose track is indexed")
        .id;
    let playlist = r
        .svc
        .create_playlist(CreatePlaylist {
            title: "Mixed".into(),
            tracks: vec![
                TrackRef::Item {
                    track_id: IdArg(album),
                    item: Some(0),
                },
                TrackRef::Item {
                    track_id: IdArg(keep),
                    item: Some(0),
                },
            ],
            created_by: None,
        })
        .unwrap()
        .summary
        .id;

    let out = r.svc.delete_playable(album, force()).unwrap();
    assert_eq!(out.playlists_edited, vec![playlist.to_string()]);

    let after = r.svc.get_collection(playlist).unwrap();
    assert_eq!(after.summary.item_count, 1, "{:?}", after.items);
    assert_eq!(after.items[0].media_path, "Music/Other/keep.mp3");
    assert!(after.items[0].available, "the remaining entry still plays");
    // The playlist file itself parses: the scrub wrote a valid document.
    let rel = r
        .svc
        .list_playables()
        .unwrap()
        .into_iter()
        .find(|p| p.id == playlist)
        .unwrap()
        .noudata_relpath;
    let (doc, warnings) = plippa_noubin::parse_noudata_with_warnings(&fs::read(r.root.join(&rel)).unwrap()).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(doc.media_list.len(), 1);
}

#[test]
fn delete_warns_about_link_tracks_that_refer_to_the_release() {
    // A `linkTrack` owns no media: it points at its release. It is not deleted with the
    // release (that is an explicit, separate action) but the preview says it is left behind.
    let mut r = album_rig();
    write(
        &r.root.join("Music/The Band/Links/_Two link.noudata"),
        br#"{"noudataType": "linkTrack", "playableItemTitle": "Two (link)",
             "referToReleaseNoudata": "../The Album/_The Album.noudata",
             "mediaList": [{"localFilePaths": ["../The Album/two.mp3"]}]}"#,
    );
    r.svc.lazy_rescan().unwrap();
    let id = id_of(&mut r, "The Album");

    let p = r.svc.preview_delete_playable(id).unwrap();
    let warning = p
        .warnings
        .iter()
        .find(|w| w.contains("refer to this release"))
        .unwrap_or_else(|| panic!("no referrer warning: {:?}", p.warnings));
    assert!(
        warning.contains("_Two link.noudata"),
        "the warning names the referring file: {warning}"
    );
    assert!(
        !p.files.iter().any(|f| f.path.contains("Links")),
        "the link track's files are not part of this delete: {:?}",
        p.files
    );

    let out = r.svc.delete_playable(id, force()).unwrap();
    assert!(out.ok);
    assert!(
        r.root.join("Music/The Band/Links/_Two link.noudata").exists(),
        "the link track is left for its own delete"
    );
}

#[test]
fn delete_refuses_a_playlist_and_explains_the_right_route() {
    let mut r = album_rig();
    let album = id_of(&mut r, "The Album");
    let playlist = r
        .svc
        .create_playlist(CreatePlaylist {
            title: "Keepers".into(),
            tracks: vec![TrackRef::Item {
                track_id: IdArg(album),
                item: Some(0),
            }],
            created_by: None,
        })
        .unwrap()
        .summary
        .id;

    let p = r.svc.preview_delete_playable(playlist).unwrap();
    let blocker = p
        .blockers
        .iter()
        .find(|b| b.kind == "playlist")
        .expect("a playlist is refused with a blocker");
    assert!(blocker.detail.contains("/api/playlists/"));

    expect_refused(r.svc.delete_playable(playlist, force()), "blocked");
    assert!(r.svc.get_collection(playlist).is_ok());
    assert!(r.root.join(ALBUM_DIR).join("one.mp3").exists());
}

// ---------------------------------------------------------------------------
// Resumability
// ---------------------------------------------------------------------------

#[test]
fn delete_is_resumable_when_a_file_delete_fails_partway() {
    let mut r = album_rig();
    let id = id_of(&mut r, "The Album");
    let p = r.svc.preview_delete_playable(id).unwrap();
    assert_eq!(p.files.len(), 2);

    // A directory appears where `one.mp3` was between the preview and the confirm: the file
    // cannot be removed (EISDIR) and the loop stops rather than deleting anything else.
    let one = r.root.join(ALBUM_DIR).join("one.mp3");
    fs::remove_file(&one).unwrap();
    fs::create_dir(&one).unwrap();

    let out = r.svc.delete_playable(id, force()).unwrap();
    assert!(out.ok);
    assert!(out.partial, "a failed file makes the delete partial");
    let remaining: Vec<&str> = out.remaining.iter().map(|f| f.path.as_str()).collect();
    assert!(remaining.contains(&"Music/The Band/The Album/one.mp3"), "{remaining:?}");
    assert!(remaining.contains(&"Music/The Band/The Album/two.mp3"), "{remaining:?}");
    assert!(out.remaining[0].reason.is_some(), "the reason is reported");
    assert!(
        out.warnings.iter().any(|w| w.contains("could not be removed")),
        "{:?}",
        out.warnings
    );

    // Index first: the row is gone, and the media that is still on disk is listed as an
    // orphan so the job can be finished with a second delete.
    assert!(!r.root.join(ALBUM_NOUDATA).exists());
    assert!(one.is_dir());
    assert!(r.root.join(ALBUM_DIR).join("two.mp3").exists());
    assert!(
        r.svc
            .list_collections(None)
            .unwrap()
            .iter()
            .all(|c| c.title != "The Album"),
        "the deleted release is gone from the portal"
    );

    let orphan = r
        .svc
        .list_playables()
        .unwrap()
        .into_iter()
        .find(|p| p.media_relpaths.iter().any(|m| m.ends_with("two.mp3")))
        .expect("the leftover media is an orphan playable");
    assert!(
        orphan.noudata_relpath.is_empty(),
        "what is left is an orphan the next delete can finish: {orphan:?}"
    );
    assert!(!orphan.associated, "an orphan has no noubin key: {orphan:?}");
    let out2 = r.svc.delete_playable(orphan.id, force()).unwrap();
    assert!(out2.ok && !out2.partial);
    assert!(!r.root.join(ALBUM_DIR).join("two.mp3").exists());
    assert!(
        one.is_dir(),
        "the directory that caused the failure is not deleted here"
    );
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn start_http(root: &Path) -> u16 {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let svc = Arc::new(Mutex::new(ServerService::start(
        Some(root.to_path_buf()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    )));
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        library_root: Some(root.to_path_buf()),
        secrets_dir: None,
        portal_url: None,
        ..ServerConfig::default()
    };
    common::serve_open(cfg, svc);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return port;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

/// Returns `(status, body)`.
fn request(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, serde_json::Value) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
         Content-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap();
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    let value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// The first playable id the running server reports (also exercises `GET /api/playables`).
fn http_id(port: u16) -> u64 {
    let (st, body) = request(port, "GET", "/api/playables", &[], b"");
    assert_eq!(st, 200, "{body}");
    body[0]["id"]
        .as_str()
        .expect("ids are decimal strings")
        .parse()
        .unwrap()
}

fn http_library() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    write(
        &root.join("_Solo.noudata"),
        br#"{"noudataType": "looseTrack", "playableItemTitle": "Solo",
             "mediaList": [{"mediaTitle": "Solo", "localFilePaths": ["solo.mp3"]}]}"#,
    );
    write(&root.join("solo.mp3"), b"SSS");
    (tmp, root)
}

#[test]
fn delete_http_guards_and_codes() {
    let (_t, root) = http_library();
    let port = start_http(&root);
    let id = http_id(port);

    // Missing CSRF proof: refused before routing.
    let (st, body) = request(port, "DELETE", &format!("/api/playables/{id}"), &[], b"{}");
    assert_eq!(st, 415, "{body}");
    assert!(root.join("solo.mp3").exists());

    // Preview is a plain GET.
    let (st, body) = request(
        port,
        "GET",
        &format!("/api/playables/{id}/delete/preview"),
        &[],
        b"",
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["files"][0]["path"], "solo.mp3");
    assert_eq!(body["noudata"], "_Solo.noudata");
    assert!(body["blockers"].as_array().unwrap().is_empty());

    // Confirmation required without `force`.
    let (st, body) = request(
        port,
        "DELETE",
        &format!("/api/playables/{id}"),
        &[("X-Plippa", "1"), ("Content-Type", "application/json")],
        br#"{"reason": "test"}"#,
    );
    assert_eq!(st, 409, "{body}");
    assert_eq!(body["code"], "confirm_required");
    assert_eq!(body["ok"], false);
    assert_eq!(body["files"][0]["path"], "solo.mp3");
    assert!(root.join("solo.mp3").exists());

    // Oversize body.
    let big = vec![b' '; 8 * 1024];
    let (st, body) = request(
        port,
        "DELETE",
        &format!("/api/playables/{id}"),
        &[("X-Plippa", "1")],
        &big,
    );
    assert_eq!(st, 413, "{body}");
    assert_eq!(body["code"], "too_large");

    // Malformed id (not a number) and an unknown id.
    let (st, body) = request(port, "GET", "/api/playables/abc/delete/preview", &[], b"");
    assert_eq!(st, 400, "{body}");
    assert_eq!(body["code"], "bad_id");
    let (st, body) = request(
        port,
        "DELETE",
        "/api/playables/999999999",
        &[("X-Plippa", "1"), ("Content-Type", "application/json")],
        br#"{"force": true}"#,
    );
    assert_eq!(st, 404, "{body}");
    assert_eq!(body["code"], "not_found");

    let (st, body) = request(port, "GET", "/api/playables/999999999/delete/preview", &[], b"");
    assert_eq!(st, 404, "{body}");
    assert_eq!(body["code"], "not_found");

    // The real thing.
    let (st, body) = request(
        port,
        "DELETE",
        &format!("/api/playables/{id}"),
        &[("X-Plippa", "1"), ("Content-Type", "application/json")],
        br#"{"force": true}"#,
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["index_updated"], true);
    assert_eq!(body["deleted"][0], "_Solo.noudata");
    assert!(!root.join("solo.mp3").exists());
    assert!(!root.join("_Solo.noudata").exists());

    // Gone now.
    let (st, _) = request(
        port,
        "DELETE",
        &format!("/api/playables/{id}"),
        &[("X-Plippa", "1"), ("Content-Type", "application/json")],
        br#"{"force": true}"#,
    );
    assert_eq!(st, 404);
}

#[test]
fn delete_http_single_file_route() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    write(&root.join("_Two.noudata"), ALBUM.as_bytes());
    write(&root.join("one.mp3"), b"AAAA");
    write(&root.join("two.mp3"), b"BBBBB");
    let port = start_http(&root);
    let id = http_id(port);

    let (st, body) = request(
        port,
        "GET",
        &format!("/api/playables/{id}/files/0/delete/preview"),
        &[],
        b"",
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(body["files"][0]["path"], "one.mp3");

    let (st, body) = request(
        port,
        "DELETE",
        &format!("/api/playables/{id}/files/0"),
        &[("X-Plippa", "1"), ("Content-Type", "application/json")],
        br#"{"force": true}"#,
    );
    assert_eq!(st, 200, "{body}");
    assert!(!root.join("one.mp3").exists());
    assert!(root.join("two.mp3").exists());
    assert!(root.join("_Two.noudata").exists());

    let (st, body) = request(
        port,
        "DELETE",
        &format!("/api/playables/{id}/files/0"),
        &[("X-Plippa", "1"), ("Content-Type", "application/json")],
        br#"{"force": true}"#,
    );
    assert_eq!(st, 400, "{body}");
    assert_eq!(body["code"], "last_file");
}
