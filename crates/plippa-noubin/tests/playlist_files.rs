//! NB_03: playlist file helpers (naming, atomic write) and how the index classifies them.

use plippa_noubin::{
    effective_type, fs_release_resolver, is_empty_user_playlist, new_playlist_document, parse_noudata,
    playlist_entry, unique_playlist_path, write_noudata_atomic, AtomicWriteError, FileRevision,
    LibraryKind, LibraryRoot, LibraryWriter, NoudataType, WriteGuard,
};
use std::fs;
use std::time::{Duration, SystemTime};

fn doc_with_entries(n: usize) -> plippa_noubin::NoudataDocument {
    let mut doc = new_playlist_document("Mine", "tester");
    for i in 0..n {
        doc.media_list
            .push(playlist_entry(&format!("../Music/t{i}.mp3")));
    }
    doc
}

#[test]
fn new_playlist_document_matches_spec_layout() {
    let json = doc_with_entries(2).to_json_pretty().unwrap();
    assert!(json.starts_with("{\n  \"_whatAmILookingAt\""), "{json}");
    assert!(json.contains("\"createdByUser\": \"tester\""));
    assert!(!json.contains("releaseData") && !json.contains("credits"));
    assert!(!json.contains("mediaTitle"));
    let doc = parse_noudata(json.as_bytes()).unwrap();
    assert_eq!(doc.playable_item_title.as_deref(), Some("Mine"));
    assert_eq!(doc.media_list.len(), 2);
}

#[test]
fn playlist_is_never_a_release_and_release_never_a_playlist() {
    let pl = doc_with_entries(1);
    assert_eq!(effective_type(&pl, |_| None), NoudataType::UserPlaylist);

    let release = parse_noudata(
        br#"{"playableItemTitle":"R","releaseData":{},"playableItemProfile":"music",
             "mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    )
    .unwrap();
    assert_eq!(effective_type(&release, |_| None), NoudataType::Release);
    assert!(!is_empty_user_playlist(&release));

    // Per-track file: refers to a release, so not a playlist even without releaseData.
    let track = parse_noudata(
        br#"{"playableItemTitle":"T","referToReleaseNoudata":"_R.noudata",
             "mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    )
    .unwrap();
    assert_eq!(
        effective_type(&track, |_| Some(release.clone())),
        NoudataType::LinkTrack
    );

    // Only a *titled, user-created* file with no entries counts as an empty playlist.
    assert!(is_empty_user_playlist(&doc_with_entries(0)));
    assert!(!is_empty_user_playlist(&doc_with_entries(1)));
    let untitled = parse_noudata(
        br#"{"mediaList":[],"localPlaybackData":{"userNotes":{"createdByUser":"x"}}}"#,
    )
    .unwrap();
    assert!(!is_empty_user_playlist(&untitled));
    let no_author = parse_noudata(br#"{"playableItemTitle":"x","mediaList":[]}"#).unwrap();
    assert!(!is_empty_user_playlist(&no_author));
}

#[test]
fn atomic_write_guards_against_concurrent_change_and_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("_P.noudata");

    write_noudata_atomic(&doc_with_entries(1), &path, WriteGuard::MustNotExist).unwrap();
    assert!(matches!(
        write_noudata_atomic(&doc_with_entries(1), &path, WriteGuard::MustNotExist),
        Err(AtomicWriteError::Conflict)
    ));

    let rev = FileRevision::of(&path).unwrap();
    let new_rev =
        write_noudata_atomic(&doc_with_entries(2), &path, WriteGuard::Unchanged(rev)).unwrap();
    assert_ne!(new_rev, rev);
    assert_eq!(
        parse_noudata(&fs::read(&path).unwrap())
            .unwrap()
            .media_list
            .len(),
        2
    );

    // A stale fingerprint is refused and the file is untouched.
    assert!(matches!(
        write_noudata_atomic(&doc_with_entries(3), &path, WriteGuard::Unchanged(rev)),
        Err(AtomicWriteError::Conflict)
    ));
    assert_eq!(
        parse_noudata(&fs::read(&path).unwrap())
            .unwrap()
            .media_list
            .len(),
        2
    );

    // A vanished file is a conflict too (deleted underneath us).
    let gone = dir.path().join("_Gone.noudata");
    assert!(matches!(
        write_noudata_atomic(&doc_with_entries(1), &gone, WriteGuard::Unchanged(rev)),
        Err(AtomicWriteError::Conflict)
    ));
    let names: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["_P.noudata"], "no temp files left behind");
}

#[test]
fn atomic_write_moves_mtime_forward_when_the_clock_is_coarse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("_P.noudata");
    write_noudata_atomic(&doc_with_entries(2), &path, WriteGuard::MustNotExist).unwrap();
    // Pretend the file was written "in the future" (FAT's 2 s clock would tie).
    let future = SystemTime::now() + Duration::from_secs(60);
    fs::File::open(&path).unwrap().set_modified(future).unwrap();
    let rev = FileRevision::of(&path).unwrap();
    // Same-size rewrite (same entries).
    let new =
        write_noudata_atomic(&doc_with_entries(2), &path, WriteGuard::Unchanged(rev)).unwrap();
    assert_eq!(new.size, rev.size);
    assert!(new.mtime_ns > rev.mtime_ns, "{new:?} vs {rev:?}");
}

#[test]
fn unique_path_is_case_insensitive_and_can_keep_current_file() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        unique_playlist_path(dir.path(), "A", None),
        dir.path().join("_A.noudata")
    );
    fs::write(dir.path().join("_a.noudata"), "{}").unwrap();
    assert_eq!(
        unique_playlist_path(dir.path(), "A", None),
        dir.path().join("_A (2).noudata")
    );
    assert_eq!(
        unique_playlist_path(dir.path(), "A", Some(&dir.path().join("_a.noudata"))),
        dir.path().join("_A.noudata")
    );
}

#[test]
fn new_and_emptied_user_playlists_are_indexed_as_playlists() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("Playlists")).unwrap();
    fs::create_dir_all(root.path().join("Music")).unwrap();
    fs::write(root.path().join("Music/t0.mp3"), b"").unwrap();
    let path = root.path().join("Playlists/_Mine.noudata");
    write_noudata_atomic(&doc_with_entries(0), &path, WriteGuard::MustNotExist).unwrap();

    let mut w = LibraryWriter::open(&LibraryRoot::new(root.path())).unwrap();
    w.full_scan().unwrap();
    let pl = w.list_by_kind(LibraryKind::Playlist).unwrap();
    assert_eq!(pl.len(), 1);
    assert_eq!(w.items_of(pl[0].id).unwrap().len(), 0);
    assert_eq!(w.item_counts().unwrap().get(&pl[0].id), None);
    assert_eq!(
        w.kind_counts().unwrap(),
        vec![(LibraryKind::Playlist, 1), (LibraryKind::Track, 1)]
    );
    let fp = w.indexed_fingerprint(pl[0].id).unwrap().unwrap();
    let rev = FileRevision::of(&path).unwrap();
    assert_eq!(fp, (rev.mtime_ns, rev.size));

    // Filling it makes it a normal playlist with resolved items.
    write_noudata_atomic(&doc_with_entries(1), &path, WriteGuard::Unchanged(rev)).unwrap();
    let r = w.lazy_rescan().unwrap();
    assert!(r.changed());
    let pl = w.list_by_kind(LibraryKind::Playlist).unwrap();
    assert_eq!(w.item_counts().unwrap().get(&pl[0].id), Some(&1));
    let _ = fs_release_resolver(&path);
}
