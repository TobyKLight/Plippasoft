//! FIX_01B: write-through ingest / forget of a single `.noudata`.
//!
//! The service writes a playlist and then indexes exactly that file. The result must not
//! depend on the parent directory's mtime, which a FAT stick only records in 2 s steps (the
//! reported "blank playlist missing until a reboot" bug). A tmpfs/ext4 fixture cannot
//! reproduce the coarse timestamp, so these tests pin the deterministic write-through path
//! itself, not the original report.

use plippa_noubin::{index_db_path, LibraryKind, LibraryRoot, LibraryWriter, PlayableRecord};
use rusqlite::Connection;
use std::fs;
use std::path::Path;

fn library() -> (tempfile::TempDir, LibraryRoot) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    let lib = LibraryRoot::new(&root);
    (tmp, lib)
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// The document the server writes for a new blank playlist (§3.7: `userPlaylist`, no items).
fn blank_playlist(title: &str) -> String {
    format!(
        r#"{{"noubinKeyNormalised":"{}.noubin.p","noudataType":"userPlaylist",
            "playableItemTitle":"{title}",
            "localPlaybackData":{{"userNotes":{{"createdByUser":"tester"}}}},
            "mediaList":[]}}"#,
        title.to_lowercase()
    )
}

/// A one-item playlist pointing at `media` relative to `Playlists/`.
fn one_item_playlist(title: &str, media: &str) -> String {
    format!(
        r#"{{"noubinKeyNormalised":"{}.noubin.p","noudataType":"userPlaylist",
            "playableItemTitle":"{title}","mediaList":[{{"localFilePaths":["{media}"]}}]}}"#,
        title.to_lowercase()
    )
}

fn by_title<'a>(all: &'a [PlayableRecord], title: &str) -> Option<&'a PlayableRecord> {
    all.iter().find(|r| r.title.as_deref() == Some(title))
}

fn db(lib: &LibraryRoot) -> Connection {
    Connection::open(index_db_path(lib)).unwrap()
}

#[test]
fn ingest_lists_a_blank_playlist_with_zero_items() {
    let (_tmp, lib) = library();
    let mut w = LibraryWriter::open(&lib).unwrap();
    write(lib.path(), "Playlists/_Blank.noudata", &blank_playlist("Blank"));

    let report = w.ingest_noudata("Playlists/_Blank.noudata").unwrap();
    assert_eq!(report.ingested, 1, "{report:?}");
    assert_eq!(report.updated, 0, "{report:?}");
    assert_eq!(report.noudata_reads, 1, "{report:?}");
    assert!(report.changed(), "the server must bump the generation");

    let all = w.list_playables().unwrap();
    assert_eq!(all.len(), 1, "{:?}", all);
    let rec = by_title(&all, "Blank").expect("playlist listed");
    assert_eq!(rec.kind, LibraryKind::Playlist);
    assert_eq!(rec.noudata_type.as_deref(), Some("userPlaylist"));
    assert!(!rec.is_dynamic && !rec.is_orphan);
    assert!(rec.media_relpaths.is_empty());
    assert!(w.items_of(rec.id).unwrap().is_empty());
    // `item_counts` only carries playables that have item rows: absent == zero items.
    assert_eq!(w.item_counts().unwrap().get(&rec.id).copied().unwrap_or(0), 0);

    // A file in a directory the cached walk does not know yet must be relisted next time,
    // otherwise a lazy scan would treat the fresh row as "vanished" and delete it.
    let conn = db(&lib);
    let mtime: i64 = conn
        .query_row(
            "SELECT mtime_ns FROM dirs WHERE relpath = 'Playlists'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(mtime, -1, "new directory marked 'relist'");
}

#[test]
fn second_ingest_reports_updated_not_ingested() {
    let (_tmp, lib) = library();
    let mut w = LibraryWriter::open(&lib).unwrap();
    write(lib.path(), "Playlists/_Blank.noudata", &blank_playlist("Blank"));
    w.ingest_noudata("Playlists/_Blank.noudata").unwrap();

    let again = w.ingest_noudata("Playlists/_Blank.noudata").unwrap();
    assert_eq!((again.ingested, again.updated), (0, 1), "{again:?}");
    assert!(again.changed());

    let all = w.list_playables().unwrap();
    assert_eq!(all.len(), 1, "no duplicate row: {all:?}");
    let rec = by_title(&all, "Blank").unwrap();
    assert!(w.items_of(rec.id).unwrap().is_empty());
}

#[test]
fn ingest_of_a_missing_file_removes_the_row() {
    let (_tmp, lib) = library();
    let mut w = LibraryWriter::open(&lib).unwrap();
    let rel = "Playlists/_Gone.noudata";
    write(lib.path(), rel, &blank_playlist("Gone"));
    w.ingest_noudata(rel).unwrap();
    assert_eq!(w.list_playables().unwrap().len(), 1);

    fs::remove_file(lib.path().join(rel)).unwrap();
    let report = w.ingest_noudata(rel).unwrap();
    assert_eq!(report.removed, 1, "{report:?}");
    assert!(report.changed());
    assert!(w.list_playables().unwrap().is_empty());
}

#[test]
fn forget_removes_the_row_and_its_skip_entry() {
    let (_tmp, lib) = library();
    let mut w = LibraryWriter::open(&lib).unwrap();
    let rel = "Playlists/_Broken.noudata";
    write(lib.path(), rel, "{ not json");
    let scan = w.lazy_rescan().unwrap();
    assert_eq!(scan.skipped, 1, "{scan:?}");
    assert_eq!(
        db(&lib)
            .query_row("SELECT COUNT(*) FROM noudata_skips", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );

    let report = w.forget_noudata(rel).unwrap();
    assert!(!report.changed(), "a skip entry is invisible to clients: {report:?}");
    assert_eq!(
        db(&lib)
            .query_row("SELECT COUNT(*) FROM noudata_skips", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0,
        "forget drops the skip entry"
    );
    // Idempotent: nothing left to forget.
    let again = w.forget_noudata(rel).unwrap();
    assert!(!again.changed());
}

#[test]
fn forgetting_a_listed_playlist_drops_its_row() {
    let (_tmp, lib) = library();
    let mut w = LibraryWriter::open(&lib).unwrap();
    let rel = "Playlists/_Bye.noudata";
    write(lib.path(), rel, &blank_playlist("Bye"));
    w.ingest_noudata(rel).unwrap();

    let report = w.forget_noudata(rel).unwrap();
    assert_eq!(report.removed, 1, "{report:?}");
    assert!(report.changed());
    assert!(w.list_playables().unwrap().is_empty());
}

#[test]
fn ingest_unorphans_media_claimed_by_the_new_playlist() {
    let (_tmp, lib) = library();
    let mut w = LibraryWriter::open(&lib).unwrap();
    write(lib.path(), "Media/song.mp3", "");
    let scan = w.full_scan().unwrap();
    assert_eq!(scan.orphans_added, 1, "{scan:?}");
    let orphan = w.list_playables().unwrap();
    assert_eq!(orphan.len(), 1);
    assert!(orphan[0].is_orphan);

    write(
        lib.path(),
        "Playlists/_P.noudata",
        &one_item_playlist("P", "../Media/song.mp3"),
    );
    let report = w.ingest_noudata("Playlists/_P.noudata").unwrap();
    assert_eq!(report.ingested, 1, "{report:?}");
    assert_eq!(
        report.orphans_removed, 1,
        "finalize runs in the same write-through: {report:?}"
    );

    let all = w.list_playables().unwrap();
    assert!(
        all.iter().all(|r| !r.is_orphan),
        "the claimed media is no longer an orphan: {all:?}"
    );
    let p = by_title(&all, "P").unwrap();
    assert_eq!(w.items_of(p.id).unwrap().len(), 1);
}

#[test]
fn unsafe_or_non_noudata_paths_are_refused() {
    let (_tmp, lib) = library();
    let mut w = LibraryWriter::open(&lib).unwrap();
    for rel in ["../escape.noudata", "/abs.noudata", "Playlists/x.txt", "a\\b.noudata"] {
        assert!(w.ingest_noudata(rel).is_err(), "{rel:?} must be refused");
        assert!(w.forget_noudata(rel).is_err(), "{rel:?} must be refused");
    }
}
