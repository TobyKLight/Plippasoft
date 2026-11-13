//! NB_03: collections service + HTTP API (playlists write-through, read access to releases).
//!
//! Fixture: `crates/plippa-noubin/tests/fixtures/libraries/kinds` (copied to a temp dir).
//! Media files are empty placeholders; nothing here decodes audio.

mod common;

use plippa_noubin::{
    effective_type, fs_release_resolver, parse_noudata_with_warnings, LibraryKind, LibraryReader,
    LibraryRoot, NoudataType, PLAYER_DB_DIR,
};
use plippa_server::{
    AddPlaylistItems, CollectionDetail, CreatePlaylist, IdArg, RecordingIpc, RenamePlaylist,
    ReorderPlaylist, ServerConfig, ServerError, ServerService, TrackRef,
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

fn service(root: &Path) -> ServerService {
    ServerService::start(
        Some(root.to_path_buf()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    )
}

/// Namespaced id of the collection titled `title` (any kind).
fn id_of(svc: &mut ServerService, title: &str) -> u64 {
    svc.list_collections(None)
        .unwrap()
        .into_iter()
        .find(|c| c.title == title)
        .unwrap_or_else(|| panic!("no collection titled {title}"))
        .id
}

fn create(svc: &mut ServerService, title: &str, tracks: Vec<TrackRef>) -> CollectionDetail {
    svc.create_playlist(CreatePlaylist {
        title: title.into(),
        tracks,
        created_by: None,
    })
    .unwrap()
}

fn album_item(album: u64, item: u32) -> TrackRef {
    TrackRef::Item {
        track_id: IdArg(album),
        item: Some(item),
    }
}

fn add(track: u64, item: Option<u32>, position: Option<usize>) -> AddPlaylistItems {
    AddPlaylistItems {
        track_id: IdArg(track),
        item,
        position,
        revision: None,
    }
}

fn titles(d: &CollectionDetail) -> Vec<&str> {
    d.items.iter().map(|i| i.title.as_str()).collect()
}

fn read_doc(root: &Path, rel: &str) -> plippa_noubin::NoudataDocument {
    let bytes = fs::read(root.join(rel)).unwrap();
    let (doc, warnings) = parse_noudata_with_warnings(&bytes).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    doc
}

fn err(r: Result<CollectionDetail, ServerError>) -> ServerError {
    r.expect_err("expected an error")
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[test]
fn collections_kinds_and_lists() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);

    let kinds = svc.collection_kinds().unwrap();
    assert_eq!(kinds.len(), 6);
    let count = |k: LibraryKind| kinds.iter().find(|c| c.kind == k).unwrap();
    assert!(count(LibraryKind::Album).has_content);
    assert!(count(LibraryKind::Mix).has_content);
    assert!(count(LibraryKind::Audiobook).has_content);
    assert!(count(LibraryKind::Playlist).count >= 3);

    let albums = svc.list_collections(Some(LibraryKind::Album)).unwrap();
    let album = albums.iter().find(|c| c.title == "The Album").unwrap();
    assert_eq!(album.item_count, 2);
    assert_eq!(album.artist.as_deref(), Some("Band"));
    assert!(!album.editable);
    assert!(album.cover.as_deref().unwrap().ends_with("/cover"));

    // A per-track referring `.noudata` is a track, never a playlist: the track is listed
    // once, through its release, and the link file itself is not a second entry.
    let tracks = svc.list_tracks(LibraryKind::Track).unwrap();
    assert_eq!(tracks.iter().filter(|t| t.title == "Two").count(), 1, "{tracks:?}");
    assert!(
        tracks.iter().any(|c| c.is_orphan),
        "orphan media is a track"
    );
    let playlists = svc.list_collections(Some(LibraryKind::Playlist)).unwrap();
    assert!(!playlists.iter().any(|c| c.title == "Two"));
    assert!(playlists
        .iter()
        .any(|c| c.title == "Rainy Day" && c.editable));
    assert!(playlists
        .iter()
        .any(|c| c.title == "Everything Loud" && c.is_dynamic && !c.editable));
}

#[test]
fn collections_detail_resolves_titles_through_release() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let rainy = id_of(&mut svc, "Rainy Day");
    let d = svc.get_collection(rainy).unwrap();
    assert!(d.summary.editable);
    assert!(d.revision.is_some());
    // "not authoritative" (the playlist's own mediaTitle) loses to the release's "One".
    assert_eq!(titles(&d)[..2], ["One", "Two"]);
    assert_eq!(d.items[0].artist.as_deref(), Some("Band"));
    assert_eq!(d.items[0].media_path, "Music/Band/Album/01 - One.mp3");
    assert_eq!(d.items[0].position, 0);
    assert!(d.items[0].source_id.is_some());

    let album_id = id_of(&mut svc, "The Album");
    let album = svc.get_collection(album_id).unwrap();
    assert_eq!(titles(&album), ["One", "Two"]);
    assert!(album.created_by.is_none());
}

#[test]
fn collections_unavailable_media_is_listed_but_flagged() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let broken = id_of(&mut svc, "Broken Link");
    let d = svc.get_collection(broken).unwrap();
    assert_eq!(d.items.len(), 1);
    assert!(!d.items[0].available);
    assert!(!d.summary.available);
    // …and it is still editable: append a good track next to the broken one.
    let one = id_of(&mut svc, "The Album");
    let d = svc
        .add_playlist_items(broken, add(one, Some(0), None))
        .unwrap();
    assert_eq!(d.items.len(), 2);
    assert!(!d.items[0].available && d.items[1].available);
}

// ---------------------------------------------------------------------------
// Acceptance flow
// ---------------------------------------------------------------------------

#[test]
fn collections_playlist_full_flow_writes_valid_noudata() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let album = id_of(&mut svc, "The Album");
    let mix = id_of(&mut svc, "Rocksteady Set");

    // Create empty, then add three tracks (album item 0, album item 1, the whole mix).
    let d = create(&mut svc, "Road Trip", vec![]);
    assert!(d.items.is_empty());
    assert_eq!(d.summary.kind, LibraryKind::Playlist);
    assert_eq!(d.noudata_relpath, "Playlists/_Road Trip.noudata");
    assert_eq!(d.created_by.as_deref(), Some("Plippa"));
    let id = d.summary.id;
    svc.add_playlist_items(id, add(album, Some(0), None))
        .unwrap();
    svc.add_playlist_items(id, add(album, Some(1), None))
        .unwrap();
    let d = svc.add_playlist_items(id, add(mix, None, None)).unwrap();
    assert_eq!(d.items.len(), 3);
    assert_eq!(d.items[0].title, "One");

    // Insert at the front, reorder, remove one, rename.
    let d = svc
        .add_playlist_items(id, add(album, Some(1), Some(0)))
        .unwrap();
    assert_eq!(d.items.len(), 4);
    assert_eq!(titles(&d)[..3], ["Two", "One", "Two"]);
    let d = svc
        .reorder_playlist(
            id,
            ReorderPlaylist {
                positions: vec![3, 2, 1, 0],
                revision: d.revision.clone(),
            },
        )
        .unwrap();
    assert_eq!(d.items[3].title, "Two");
    let d = svc
        .remove_playlist_item(id, 0, Some(&d.items[0].media_path), None)
        .unwrap();
    assert_eq!(d.items.len(), 3);
    let d = svc
        .rename_playlist(
            id,
            RenamePlaylist {
                title: Some("Holiday: Road/Trip".into()),
                revision: d.revision.clone(),
            },
        )
        .unwrap();
    assert_eq!(d.summary.title, "Holiday: Road/Trip");
    assert_eq!(d.noudata_relpath, "Playlists/_Holiday_ Road_Trip.noudata");
    assert!(!root.join("Playlists/_Road Trip.noudata").exists());
    // The index row is re-created for the moved file: callers must use the returned id.
    assert_eq!(
        svc.get_collection(d.summary.id).unwrap().noudata_relpath,
        d.noudata_relpath
    );
    assert_eq!(d.items.len(), 3);

    // The file on disk: spec-conformant, no release data, relative paths.
    let doc = read_doc(&root, &d.noudata_relpath);
    assert!(doc.release_data.is_none());
    assert!(doc.refer_to_release_noudata.is_none());
    assert_eq!(
        doc.playable_item_title.as_deref(),
        Some("Holiday: Road/Trip")
    );
    assert_eq!(doc.media_list.len(), 3);
    let raw = fs::read_to_string(root.join(&d.noudata_relpath)).unwrap();
    assert!(
        !raw.contains("releaseData") && !raw.contains("credits"),
        "{raw}"
    );
    assert!(raw.contains("createdByUser"));
    assert!(
        raw.trim_start().starts_with("{\n  \"_whatAmILookingAt\""),
        "{raw}"
    );
    for item in &doc.media_list {
        assert!(item.media_title.is_none(), "no release metadata is copied");
        let p = &item.local_file_paths.as_ref().unwrap()[0];
        assert!(p.starts_with("../"), "{p}");
        assert!(root.join("Playlists").join(p).exists(), "{p}");
    }
    let resolver = fs_release_resolver(&root.join(&d.noudata_relpath));
    assert_eq!(effective_type(&doc, resolver), NoudataType::UserPlaylist);
    assert!(!root.join("Playlists").read_dir().unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("plippa-tmp")));

    // A second reader (as the engine / UI would open it) sees the same thing.
    let reader = LibraryReader::open(&LibraryRoot::new(&root)).unwrap();
    let rec = reader
        .list_by_kind(LibraryKind::Playlist)
        .unwrap()
        .into_iter()
        .find(|r| r.noudata_relpath == d.noudata_relpath)
        .unwrap();
    assert_eq!(reader.items_of(rec.id).unwrap().len(), 3);
}

#[test]
fn collections_playlist_survives_index_deletion_and_rescan() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let album = id_of(&mut svc, "The Album");
    let d = create(
        &mut svc,
        "Keeper",
        vec![album_item(album, 1), album_item(album, 0)],
    );
    let before: Vec<String> = d.items.iter().map(|i| i.title.clone()).collect();
    drop(svc);

    fs::remove_dir_all(root.join(PLAYER_DB_DIR)).unwrap();
    let mut svc = service(&root);
    let id = id_of(&mut svc, "Keeper");
    let d = svc.get_collection(id).unwrap();
    assert_eq!(
        d.items.iter().map(|i| i.title.clone()).collect::<Vec<_>>(),
        before
    );
    assert_eq!(d.created_by.as_deref(), Some("Plippa"));
}

#[test]
fn collections_create_with_tracks_and_whole_album() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let album = id_of(&mut svc, "The Album");
    let orphan = svc
        .list_collections(Some(LibraryKind::Track))
        .unwrap()
        .into_iter()
        .find(|c| c.is_orphan)
        .unwrap()
        .id;
    let d = create(
        &mut svc,
        "Mixed bag",
        vec![TrackRef::Id(IdArg(album)), TrackRef::Id(IdArg(orphan))],
    );
    // Whole album (2 items) + the orphan (titled by file name).
    assert_eq!(d.items.len(), 3);
    assert_eq!(d.items[2].title, "orphan song");
    assert_eq!(d.items[2].media_path, "Loose/orphan song.mp3");
    let doc = read_doc(&root, &d.noudata_relpath);
    assert_eq!(
        doc.media_list[2].local_file_paths.as_deref().unwrap(),
        ["../Loose/orphan song.mp3"]
    );
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

#[test]
fn collections_name_collisions_get_unique_files() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let a = create(&mut svc, "Same", vec![]);
    let b = create(&mut svc, "Same", vec![]);
    let c = create(&mut svc, "SAME", vec![]);
    assert_eq!(a.noudata_relpath, "Playlists/_Same.noudata");
    assert_eq!(b.noudata_relpath, "Playlists/_Same (2).noudata");
    assert_eq!(
        c.noudata_relpath, "Playlists/_SAME (3).noudata",
        "case-insensitive (FAT)"
    );
    // Two playlists keep their own display title.
    assert_eq!(b.summary.title, "Same");

    // Renaming onto an existing name is a collision too; renaming to itself is a no-op.
    let r = svc
        .rename_playlist(
            b.summary.id,
            RenamePlaylist {
                title: Some("Same".into()),
                revision: None,
            },
        )
        .unwrap();
    assert_eq!(r.noudata_relpath, "Playlists/_Same (2).noudata");
    assert_eq!(r.summary.id, b.summary.id);
}

// ---------------------------------------------------------------------------
// Empty playlists stay visible
// ---------------------------------------------------------------------------

#[test]
fn collections_removing_last_item_keeps_playlist_listed() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let album = id_of(&mut svc, "The Album");
    let d = create(&mut svc, "Solo", vec![album_item(album, 0)]);
    let d = svc
        .remove_playlist_item(d.summary.id, 0, None, None)
        .unwrap();
    assert!(d.items.is_empty());
    let listed = svc.list_collections(Some(LibraryKind::Playlist)).unwrap();
    let solo = listed.iter().find(|c| c.title == "Solo").unwrap();
    assert_eq!(solo.item_count, 0);
    assert!(solo.editable);
}

#[test]
fn collections_unrelated_empty_noudata_is_still_ignored() {
    let (_t, root) = kinds_library();
    fs::write(
        root.join("Playlists/Junk.noudata"),
        r#"{"playableItemTitle":"Junk","mediaList":[]}"#,
    )
    .unwrap();
    let mut svc = service(&root);
    svc.lazy_rescan().unwrap();
    let listed = svc.list_collections(None).unwrap();
    assert!(!listed.iter().any(|c| c.title == "Junk"));
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

#[test]
fn collections_delete_removes_only_the_playlist_file() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let album = id_of(&mut svc, "The Album");
    let d = create(&mut svc, "Doomed", vec![album_item(album, 0)]);
    svc.delete_playlist(d.summary.id, None).unwrap();
    assert!(!root.join(&d.noudata_relpath).exists());
    assert!(root.join("Music/Band/Album/01 - One.mp3").exists());
    assert!(svc.get_collection(d.summary.id).is_err());
    assert!(matches!(
        svc.delete_playlist(d.summary.id, None),
        Err(ServerError::NotFound(_))
    ));
}

// ---------------------------------------------------------------------------
// Errors: 409 and 4xx
// ---------------------------------------------------------------------------

#[test]
fn collections_external_modification_conflicts() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let album = id_of(&mut svc, "The Album");
    let d = create(&mut svc, "Shared", vec![album_item(album, 0)]);
    let id = d.summary.id;
    let seen = d.revision.clone().unwrap();

    // Someone edits the file by hand (different content, later mtime).
    let path = root.join(&d.noudata_relpath);
    let mut doc = read_doc(&root, &d.noudata_relpath);
    doc.playable_item_title = Some("Shared (edited elsewhere)".into());
    doc.write_to_path(&path).unwrap();
    let f = fs::File::open(&path).unwrap();
    f.set_modified(std::time::SystemTime::now() + Duration::from_secs(5))
        .unwrap();

    let mut req = add(album, Some(1), None);
    req.revision = Some(seen.clone());
    assert!(matches!(
        svc.add_playlist_items(id, req),
        Err(ServerError::Conflict(_))
    ));
    assert!(matches!(
        svc.rename_playlist(
            id,
            RenamePlaylist {
                title: Some("x".into()),
                revision: Some(seen.clone())
            }
        ),
        Err(ServerError::Conflict(_))
    ));
    assert!(matches!(
        svc.delete_playlist(id, Some(&seen)),
        Err(ServerError::Conflict(_))
    ));
    // Nothing was overwritten by the rejected edits.
    assert_eq!(
        read_doc(&root, &d.noudata_relpath)
            .playable_item_title
            .as_deref(),
        Some("Shared (edited elsewhere)")
    );

    // Fresh detail carries the new revision, and the edit goes through with it.
    let fresh = svc.get_collection(id).unwrap();
    assert_ne!(fresh.revision.as_deref(), Some(seen.as_str()));
    let mut req = add(album, Some(1), None);
    req.revision = fresh.revision.clone();
    let d = svc.add_playlist_items(id, req).unwrap();
    assert_eq!(d.items.len(), 2);
    // Without a revision it is last-write-wins on the current file.
    let d = svc
        .add_playlist_items(d.summary.id, add(album, Some(0), None))
        .unwrap();
    assert_eq!(d.items.len(), 3);
}

#[test]
fn collections_item_identity_is_position_plus_path() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let album = id_of(&mut svc, "The Album");
    let d = create(
        &mut svc,
        "Ident",
        vec![album_item(album, 0), album_item(album, 1)],
    );
    let id = d.summary.id;
    // Position 0 is "One"; claiming it is "Two" must not delete anything.
    let wrong = d.items[1].media_path.clone();
    assert!(matches!(
        svc.remove_playlist_item(id, 0, Some(&wrong), None),
        Err(ServerError::Conflict(_))
    ));
    assert_eq!(svc.get_collection(id).unwrap().items.len(), 2);
    let d = svc
        .remove_playlist_item(id, 0, Some(&d.items[0].media_path), None)
        .unwrap();
    assert_eq!(titles(&d), ["Two"]);
}

#[test]
fn collections_invalid_input_is_rejected_cleanly() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let album = id_of(&mut svc, "The Album");
    let dynamic = id_of(&mut svc, "Everything Loud");
    let d = create(&mut svc, "Valid", vec![album_item(album, 0)]);
    let id = d.summary.id;

    let invalid = |e: ServerError| matches!(e, ServerError::Invalid(_));
    for bad in ["", "   ", "line\nbreak"] {
        assert!(invalid(
            svc.create_playlist(CreatePlaylist {
                title: bad.into(),
                tracks: vec![],
                created_by: None
            })
            .unwrap_err()
        ));
    }
    assert!(invalid(err(
        svc.rename_playlist(id, RenamePlaylist::default())
    )));
    // Albums / mixes are read-only; so are dynamic playlists' items.
    assert!(invalid(err(
        svc.add_playlist_items(album, add(album, Some(0), None))
    )));
    assert!(invalid(err(svc.rename_playlist(
        album,
        RenamePlaylist {
            title: Some("x".into()),
            revision: None
        }
    ))));
    assert!(matches!(
        svc.delete_playlist(album, None),
        Err(ServerError::Invalid(_))
    ));
    assert!(invalid(err(
        svc.add_playlist_items(dynamic, add(album, Some(0), None))
    )));
    // Bad source tracks.
    assert!(invalid(err(
        svc.add_playlist_items(id, add(album, Some(99), None))
    )));
    assert!(invalid(err(
        svc.add_playlist_items(id, add(dynamic, None, None))
    )));
    assert!(matches!(
        svc.add_playlist_items(id, add(12345, None, None)),
        Err(ServerError::NotFound(_))
    ));
    // Bad positions / permutations.
    assert!(invalid(err(
        svc.add_playlist_items(id, add(album, Some(0), Some(5)))
    )));
    assert!(invalid(err(svc.remove_playlist_item(id, 7, None, None))));
    for positions in [vec![0, 0], vec![1], vec![0, 2], vec![]] {
        assert!(invalid(err(svc.reorder_playlist(
            id,
            ReorderPlaylist {
                positions,
                revision: None
            }
        ))),);
    }
    // Unknown playlist id.
    assert!(matches!(
        svc.rename_playlist(
            999,
            RenamePlaylist {
                title: Some("x".into()),
                revision: None
            }
        ),
        Err(ServerError::NotFound(_))
    ));
    // Nothing above changed the playlist.
    assert_eq!(svc.get_collection(id).unwrap().items.len(), 1);
}

#[test]
fn collections_media_on_another_volume_is_rejected() {
    let (_t1, primary) = kinds_library();
    let (_t2, second) = kinds_library();
    let mut svc = ServerService::start_with_roots(
        vec![primary.clone(), second.clone()],
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    let albums = svc.list_collections(Some(LibraryKind::Album)).unwrap();
    assert_eq!(albums.len(), 2, "both volumes are listed");
    assert!(albums.iter().all(|c| c.library_label.is_some()));
    let primary_ns = plippa_noubin::library_namespace(&primary) as u64;
    let on_p = albums.iter().find(|c| c.id >> 32 == primary_ns).unwrap().id;
    let on_s = albums.iter().find(|c| c.id >> 32 != primary_ns).unwrap().id;

    // New playlists go on the primary volume (first root).
    let p = create(&mut svc, "Cross", vec![]);
    assert!(primary.join(&p.noudata_relpath).exists());
    assert!(!second.join(&p.noudata_relpath).exists());

    svc.add_playlist_items(p.summary.id, add(on_p, Some(0), None))
        .unwrap();
    let e = err(svc.add_playlist_items(p.summary.id, add(on_s, Some(0), None)));
    assert!(
        matches!(e, ServerError::Invalid(ref m) if m.contains("another volume")),
        "{e}"
    );
    let e = svc
        .create_playlist(CreatePlaylist {
            title: "Cross 2".into(),
            tracks: vec![TrackRef::Id(IdArg(on_s))],
            created_by: None,
        })
        .unwrap_err();
    assert!(matches!(e, ServerError::Invalid(_)), "{e}");
    assert!(
        !primary.join("Playlists/_Cross 2.noudata").exists(),
        "no half-created file"
    );
}

// ---------------------------------------------------------------------------
// Index bookkeeping
// ---------------------------------------------------------------------------

#[test]
fn collections_mutation_bumps_generation_and_notifies() {
    use plippa_ipc_proto::IpcMessage;
    let (_t, root) = kinds_library();
    let ipc = RecordingIpc::new();
    let view = ipc.clone();
    let mut svc = ServerService::start(
        Some(root.clone()),
        Box::new(ipc),
        ServerService::default_mock_associate(),
    );
    let gens = |v: &RecordingIpc| -> Vec<u64> {
        v.messages()
            .iter()
            .filter_map(|m| match m {
                IpcMessage::LibraryIndexUpdated(u) => Some(u.generation),
                _ => None,
            })
            .collect()
    };
    let start = *gens(&view).last().unwrap();
    let album = id_of(&mut svc, "The Album");
    let d = create(&mut svc, "Gen", vec![album_item(album, 0)]);
    assert_eq!(*gens(&view).last().unwrap(), start + 1);
    // A reorder keeps the file size: the mtime guard must still make the index notice.
    let d = svc
        .add_playlist_items(d.summary.id, add(album, Some(1), None))
        .unwrap();
    let g = *gens(&view).last().unwrap();
    let d = svc
        .reorder_playlist(
            d.summary.id,
            ReorderPlaylist {
                positions: vec![1, 0],
                revision: None,
            },
        )
        .unwrap();
    assert_eq!(*gens(&view).last().unwrap(), g + 1);
    assert_eq!(
        titles(&d),
        ["Two", "One"],
        "index reflects the same-size reorder"
    );
    // Read-only calls stay quiet.
    let g = *gens(&view).last().unwrap();
    svc.get_collection(d.summary.id).unwrap();
    svc.list_collections(None).unwrap();
    assert_eq!(*gens(&view).last().unwrap(), g);
}

#[test]
fn collections_playlist_write_through_needs_no_rescan() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);

    // A playlist dropped on disk after the index was built: a request-path scan would list
    // it. Write-through must index only the file the service wrote.
    let manual = r#"{"noubinKeyNormalised":"manual.noubin.m","noudataType":"userPlaylist",
        "playableItemTitle":"Manual","localPlaybackData":{"userNotes":{"createdByUser":"tester"}},
        "mediaList":[]}"#;
    fs::write(root.join("Playlists/_Manual.noudata"), manual).unwrap();

    let d = create(&mut svc, "Blank", vec![]);
    assert_eq!(d.summary.item_count, 0);
    assert!(d.items.is_empty());
    let listed = svc.list_collections(Some(LibraryKind::Playlist)).unwrap();
    let blank = listed
        .iter()
        .find(|c| c.title == "Blank")
        .expect("a new blank playlist is listed immediately");
    assert_eq!(blank.item_count, 0);
    assert_eq!(blank.id, d.summary.id);
    assert!(blank.editable, "an empty playlist still accepts items");
    assert!(
        !listed.iter().any(|c| c.title == "Manual"),
        "the request path must not walk the library"
    );

    // Rename: the old row is dropped and the new file is indexed in the same write-through.
    let r = svc
        .rename_playlist(
            d.summary.id,
            RenamePlaylist {
                title: Some("Blank 2".into()),
                revision: d.revision.clone(),
            },
        )
        .unwrap();
    assert_ne!(r.noudata_relpath, d.noudata_relpath);
    let listed = svc.list_collections(Some(LibraryKind::Playlist)).unwrap();
    assert!(listed.iter().any(|c| c.title == "Blank 2"));
    assert!(!listed.iter().any(|c| c.title == "Blank"));
    assert_eq!(
        svc.get_collection(r.summary.id).unwrap().summary.title,
        "Blank 2"
    );

    // Delete: gone from the list at once (no restart, no rescan).
    svc.delete_playlist(r.summary.id, r.revision.as_deref())
        .unwrap();
    let listed = svc.list_collections(Some(LibraryKind::Playlist)).unwrap();
    assert!(!listed.iter().any(|c| c.title == "Blank 2"));
    assert!(!root.join(&r.noudata_relpath).exists());
}

#[test]
fn collections_detail_refreshes_a_stale_index() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let rainy = id_of(&mut svc, "Rainy Day");
    // Hand-edit: drop the last entry, make sure mtime differs.
    let path = root.join("Playlists/Rainy.noudata");
    fs::write(
        &path,
        r#"{"playableItemTitle":"Rainy Day","mediaList":[{"localFilePaths":["../Music/Band/Album/01 - One.mp3"]}]}"#,
    )
    .unwrap();
    fs::File::open(&path)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(10))
        .unwrap();
    let d = svc.get_collection(rainy).unwrap();
    assert_eq!(
        d.items.len(),
        1,
        "detail matches the file it reports the revision of"
    );
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn http(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, serde_json::Value) {
    let (status, bytes) = http_raw(port, method, path, body, &[]);
    let v = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, v)
}

fn http_raw(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let body = body.unwrap_or("");
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nX-Plippa: 1\r\n{extra}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, raw[split + 4..].to_vec())
}

fn start_http(root: &Path) -> u16 {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let svc = Arc::new(Mutex::new(service(root)));
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

#[test]
fn tracks_endpoint_lists_item_level_views_and_rejects_other_views() {
    let (_t, root) = kinds_library();
    let port = start_http(&root);

    let (st, v) = http(port, "GET", "/api/tracks?view=track", None);
    assert_eq!(st, 200);
    let tracks = v["tracks"].as_array().unwrap();
    let two: Vec<_> = tracks.iter().filter(|t| t["title"] == "Two").collect();
    assert_eq!(two.len(), 1, "listed once, through its album: {tracks:?}");
    assert_eq!(two[0]["collection_title"], "The Album");
    assert_eq!(two[0]["position"], 1);
    assert!(two[0]["id"].is_string());
    assert!(tracks.iter().any(|t| t["is_orphan"] == true));
    // Sorted A-Z.
    let titles: Vec<String> = tracks.iter().map(|t| t["title"].as_str().unwrap().to_lowercase()).collect();
    let mut sorted = titles.clone();
    sorted.sort();
    assert_eq!(titles, sorted);

    assert_eq!(http(port, "GET", "/api/tracks?view=mix", None).0, 200);
    for bad in ["album", "playlist", "bogus", ""] {
        assert_eq!(http(port, "GET", &format!("/api/tracks?view={bad}"), None).0, 400, "{bad}");
    }
    assert_eq!(http(port, "GET", "/api/tracks", None).0, 400);
}

#[test]
fn collections_http_end_to_end() {
    let (_t, root) = kinds_library();
    let port = start_http(&root);

    let (st, v) = http(port, "GET", "/api/kinds", None);
    assert_eq!(st, 200);
    let kinds = v["kinds"].as_array().unwrap();
    assert_eq!(kinds.len(), 6);
    assert!(kinds
        .iter()
        .any(|k| k["kind"] == "audiobook" && k["has_content"] == true));
    assert!(kinds.iter().any(|k| k["kind"] == "podcast"));

    let (st, v) = http(port, "GET", "/api/collections?kind=album", None);
    assert_eq!(st, 200);
    let album = v["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["title"] == "The Album")
        .unwrap()
        .clone();
    let album_id = album["id"].as_str().unwrap().to_string();
    assert_eq!(album["item_count"], 2);
    assert_eq!(
        http(port, "GET", "/api/collections?kind=bogus", None).0,
        400
    );
    assert_eq!(http(port, "GET", "/api/collections/abc", None).0, 400);
    assert_eq!(http(port, "GET", "/api/collections/12345", None).0, 404);
    assert_eq!(
        http(
            port,
            "GET",
            &format!("/api/collections/{album_id}/cover"),
            None
        )
        .0,
        404
    );

    // Create (ids as strings and as {track_id,item}), add, reorder, remove, rename, delete.
    let (st, v) = http(
        port,
        "POST",
        "/api/playlists",
        Some(&format!(
            r#"{{"title":"Web List","tracks":["{album_id}",{{"track_id":"{album_id}","item":0}}]}}"#
        )),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 3);
    let id = v["id"].as_str().unwrap().to_string();
    let rev = v["revision"].as_str().unwrap().to_string();
    assert_eq!(v["kind"], "playlist");
    assert_eq!(v["editable"], true);

    let (st, v) = http(
        port,
        "POST",
        &format!("/api/playlists/{id}/items"),
        Some(&format!(
            r#"{{"track_id":"{album_id}","item":1,"position":0,"revision":"{rev}"}}"#
        )),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["items"][0]["title"], "Two");
    let rev2 = v["revision"].as_str().unwrap().to_string();
    assert_ne!(rev, rev2);

    // A playlist cannot be added to itself: refused (not merely ignored), and the file is
    // untouched — the entry count and the revision stay as they were.
    let before = http(port, "GET", &format!("/api/collections/{id}"), None);
    assert_eq!(before.0, 200, "{}", before.1);
    let (st, v) = http(
        port,
        "POST",
        &format!("/api/playlists/{id}/items"),
        Some(&format!(r#"{{"track_id":"{id}"}}"#)),
    );
    assert_eq!(st, 400, "{v}");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("cannot be added to itself"),
        "{v}"
    );
    let after = http(port, "GET", &format!("/api/collections/{id}"), None);
    assert_eq!(after.0, 200, "{}", after.1);
    assert_eq!(
        after.1["items"].as_array().unwrap().len(),
        before.1["items"].as_array().unwrap().len(),
        "a refused self-append must not touch the playlist"
    );
    assert_eq!(after.1["revision"], before.1["revision"]);

    // Stale revision → 409 (body field, query, and If-Match header).
    let (st, v) = http(
        port,
        "POST",
        &format!("/api/playlists/{id}/items"),
        Some(&format!(
            r#"{{"track_id":"{album_id}","revision":"{rev}"}}"#
        )),
    );
    assert_eq!(st, 409, "{v}");
    assert!(v["error"].as_str().unwrap().contains("changed"));
    assert_eq!(
        http(
            port,
            "DELETE",
            &format!("/api/playlists/{id}/items/0?revision={rev}"),
            None
        )
        .0,
        409
    );
    let (st, _) = http_raw(
        port,
        "PUT",
        &format!("/api/playlists/{id}/order"),
        Some(r#"{"positions":[3,2,1,0]}"#),
        &[("If-Match", &format!("\"{rev}\""))],
    );
    assert_eq!(st, 409);

    let (st, v) = http(
        port,
        "PUT",
        &format!("/api/playlists/{id}/order"),
        Some(r#"{"positions":[3,2,1,0]}"#),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 4);
    assert_eq!(
        http(
            port,
            "PUT",
            &format!("/api/playlists/{id}/order"),
            Some(r#"{"positions":[0]}"#)
        )
        .0,
        400
    );
    assert_eq!(
        http(
            port,
            "PUT",
            &format!("/api/playlists/{id}/order"),
            Some("not json")
        )
        .0,
        400
    );

    let (st, v) = http(
        port,
        "DELETE",
        &format!("/api/playlists/{id}/items/9"),
        None,
    );
    assert_eq!(st, 400, "{v}");
    let (st, v) = http(
        port,
        "DELETE",
        &format!("/api/playlists/{id}/items/0"),
        None,
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 3);

    let (st, v) = http(
        port,
        "PATCH",
        &format!("/api/playlists/{id}"),
        Some(r#"{"title":"Renamed"}"#),
    );
    assert_eq!(st, 200, "{v}");
    let new_id = v["id"].as_str().unwrap().to_string();
    assert_eq!(v["noudata_relpath"], "Playlists/_Renamed.noudata");
    let (st, v) = http(port, "GET", &format!("/api/collections/{new_id}"), None);
    assert_eq!(st, 200);
    assert_eq!(v["items"].as_array().unwrap().len(), 3);

    // Albums cannot be edited through the playlist endpoints.
    assert_eq!(
        http(
            port,
            "PATCH",
            &format!("/api/playlists/{album_id}"),
            Some(r#"{"title":"x"}"#)
        )
        .0,
        400
    );
    assert_eq!(
        http(port, "DELETE", &format!("/api/playlists/{album_id}"), None).0,
        400
    );

    let (st, v) = http(port, "DELETE", &format!("/api/playlists/{new_id}"), None);
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        http(port, "GET", &format!("/api/collections/{new_id}"), None).0,
        404
    );
    assert!(!root.join("Playlists/_Renamed.noudata").exists());
}

#[test]
fn collections_http_serves_cover_images() {
    let (_t, root) = kinds_library();
    // 1x1 PNG-ish bytes are fine: the endpoint serves the file verbatim.
    fs::write(root.join("Music/Band/Album/cover.jpg"), b"\xFF\xD8\xFFfake").unwrap();
    let port = start_http(&root);
    let (_, v) = http(port, "GET", "/api/collections?kind=album", None);
    let album = &v["collections"][0];
    let url = album["cover"].as_str().unwrap().to_string();
    let (st, bytes) = http_raw(port, "GET", &url, None, &[]);
    assert_eq!(st, 200);
    assert_eq!(bytes, b"\xFF\xD8\xFFfake");
}

/// G1 open issue (Low): `createdByUser` is caller text and goes through the shared sanitiser
/// with the spec's 256-char name cap, like the title.
#[test]
fn collections_created_by_is_sanitised_and_capped() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let hostile = format!("Ev\u{202E}il\u{200B}\u{7}Name{}", "x".repeat(600));
    let d = svc
        .create_playlist(CreatePlaylist {
            title: "Owned".into(),
            tracks: vec![],
            created_by: Some(hostile),
        })
        .unwrap();
    let by = d.created_by.clone().unwrap();
    assert!(by.starts_with("EvilName"), "{by:?}");
    assert!(by.chars().count() <= 256, "{}", by.chars().count());
    assert!(!by.chars().any(|c| c.is_control() || c == '\u{202E}' || c == '\u{200B}'));
    let raw = fs::read_to_string(root.join(&d.noudata_relpath)).unwrap();
    assert!(!raw.contains('\u{202E}') && !raw.contains('\u{200B}') && !raw.contains(&"x".repeat(300)));

    // Only invisible junk -> falls back to the default author, never an empty string.
    let d = svc
        .create_playlist(CreatePlaylist {
            title: "Owned 2".into(),
            tracks: vec![],
            created_by: Some("\u{202E}\u{200B}  ".into()),
        })
        .unwrap();
    assert_eq!(d.created_by.as_deref(), Some("Plippa"));
}

#[test]
fn reorder_album_permutes_the_media_list_and_remaps_the_resume_point() {
    let (_t, root) = kinds_library();
    let rel = "Music/Band/Album/_Album.noudata";
    let path = root.join(rel);
    // FIX_01D: a long-form resume point (1-indexed) must follow its item.
    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        text.replace(
            "\"mediaList\": [",
            "\"localPlaybackData\": {\"resumeAtItem\": 2},\n  \"mediaList\": [",
        ),
    )
    .unwrap();
    let mut svc = service(&root);
    let album = id_of(&mut svc, "The Album");
    let media = ["01 - One.mp3", "02 - Two.mp3"]
        .map(|m| fs::read(root.join("Music/Band/Album").join(m)).unwrap());
    assert!(svc
        .list_collections(Some(LibraryKind::Album))
        .unwrap()
        .iter()
        .find(|c| c.id == album)
        .unwrap()
        .reorderable);

    let d = svc
        .reorder_collection(album, ReorderPlaylist { positions: vec![1, 0], revision: None })
        .unwrap();
    assert_eq!(titles(&d), ["Two", "One"]);
    // Nothing on disk is renamed: the authoritative album order is the `mediaList`.
    for (m, bytes) in ["01 - One.mp3", "02 - Two.mp3"].iter().zip(&media) {
        assert_eq!(
            &fs::read(root.join("Music/Band/Album").join(m)).unwrap(),
            bytes,
            "media files are never rewritten by a reorder"
        );
    }
    let doc = read_doc(&root, rel);
    assert_eq!(
        doc.media_list[0].local_file_paths.as_ref().unwrap(),
        &["02 - Two.mp3"]
    );
    assert_eq!(doc.media_list[0].media_title.as_deref(), Some("Two"));
    assert_eq!(doc.playable_item_title.as_deref(), Some("The Album"));
    assert_eq!(
        doc.local_playback_data.as_ref().unwrap().resume_at_item,
        Some(1),
        "resumeAtItem follows the item (old index 1 -> new index 0)"
    );

    // Invalid permutations write nothing.
    for bad in [vec![0, 0], vec![0, 2], vec![0]] {
        assert!(matches!(
            svc.reorder_collection(album, ReorderPlaylist { positions: bad, revision: None }),
            Err(ServerError::Invalid(_))
        ));
    }
    assert_eq!(read_doc(&root, rel).media_list[0].media_title.as_deref(), Some("Two"));

    // A fresh service over the same folder sees the new order (server restart / reload).
    let mut restarted = service(&root);
    let album = id_of(&mut restarted, "The Album");
    let again = restarted.get_collection(album).unwrap();
    assert_eq!(titles(&again), ["Two", "One"]);
    assert_eq!(again.summary.item_count, 2);

    // A stale revision is rejected.
    let rev = d.revision.clone();
    fs::write(&path, fs::read_to_string(&path).unwrap().replace("Band", "Bend")).unwrap();
    assert!(matches!(
        svc.reorder_collection(
            album,
            ReorderPlaylist { positions: vec![0, 1], revision: rev }
        ),
        Err(ServerError::Conflict(_))
    ));
}

#[test]
fn only_playlists_and_releases_are_reorderable() {
    let (_t, root) = kinds_library();
    let mut svc = service(&root);
    let list = svc.list_collections(None).unwrap();
    let find = |t: &str| list.iter().find(|c| c.title == t).unwrap();

    assert!(find("The Album").reorderable, "releases are reorderable");
    assert!(find("Rainy Day").reorderable, "playlists are reorderable");
    assert!(
        !find("Everything Loud").reorderable,
        "dynamic playlists have no item list"
    );
    let orphan = list
        .iter()
        .find(|c| c.kind == LibraryKind::Track && c.is_orphan)
        .expect("fixture has an orphan");
    assert!(!orphan.reorderable);
    assert!(!orphan.editable);

    // Dynamic playlists, orphans and loose tracks are refused by the reorder path.
    for id in [find("Everything Loud").id, orphan.id] {
        let err = svc
            .reorder_collection(
                id,
                ReorderPlaylist {
                    positions: vec![0],
                    revision: None,
                },
            )
            .unwrap_err();
        assert!(matches!(err, ServerError::Invalid(_)), "{id}: {err:?}");
    }
}

#[test]
fn reorder_album_route_end_to_end() {
    let (_t, root) = kinds_library();
    let port = start_http(&root);

    let (st, v) = http(port, "GET", "/api/collections?kind=album", None);
    assert_eq!(st, 200);
    let album = v["collections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["title"] == "The Album")
        .unwrap()
        .clone();
    assert_eq!(album["reorderable"], true, "{album}");
    let id = album["id"].as_str().unwrap().to_string();

    let (st, v) = http(
        port,
        "PUT",
        &format!("/api/collections/{id}/order"),
        Some(r#"{"positions":[1,0]}"#),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["reorderable"], true);
    let got: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    assert_eq!(got, ["Two", "One"]);

    let (st, v) = http(port, "GET", &format!("/api/collections/{id}"), None);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["items"][0]["title"], "Two", "the index saw the write-through");

    assert_eq!(
        http(
            port,
            "PUT",
            &format!("/api/collections/{id}/order"),
            Some(r#"{"positions":[0,0]}"#)
        )
        .0,
        400
    );
    // The playlist route delegates to the same reorder (SPEC FIX_01D), so it works too.
    let (st, v) = http(
        port,
        "PUT",
        &format!("/api/playlists/{id}/order"),
        Some(r#"{"positions":[1,0]}"#),
    );
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["items"][0]["title"], "One");
    assert_eq!(
        http(port, "PUT", "/api/collections/abc/order", Some(r#"{"positions":[]}"#)).0,
        400
    );
}
