//! PWRSW_01 / USB corruption report: bound the index WAL in normal operation.
//!
//! The card must be safe to remove after a power-off. That requires the server to stop
//! writing, checkpoint the WAL and close SQLite before the volume goes away — and to keep
//! the WAL small while running, rather than leaving hours of frames uncheckpointed.

use plippa_noubin::{index_db_path, LibraryReader, LibraryRoot, LibraryWriter};
use std::fs;
use std::path::{Path, PathBuf};

fn library() -> (tempfile::TempDir, LibraryRoot) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    let lib = LibraryRoot::new(&root);
    (tmp, lib)
}

fn write_playlist(root: &Path, title: &str) {
    let rel = format!("Playlists/_{title}.noudata");
    let path = root.join(&rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!(
            r#"{{"noubinKeyNormalised":"{t}.noubin.p","noudataType":"userPlaylist",
                "playableItemTitle":"{title}",
                "localPlaybackData":{{"userNotes":{{"createdByUser":"tester"}}}},
                "mediaList":[]}}"#,
            t = title.to_lowercase()
        ),
    )
    .unwrap();
}

fn wal_path(lib: &LibraryRoot) -> PathBuf {
    PathBuf::from(format!("{}-wal", index_db_path(lib).display()))
}

fn wal_len(lib: &LibraryRoot) -> u64 {
    fs::metadata(wal_path(lib)).map_or(0, |m| m.len())
}

#[test]
fn checkpoint_truncates_the_wal_and_keeps_rows_readable() {
    let (_tmp, lib) = library();
    let mut writer = LibraryWriter::open(&lib).unwrap();
    write_playlist(lib.path(), "Truncate");
    assert!(writer
        .ingest_noudata("Playlists/_Truncate.noudata")
        .unwrap()
        .changed());
    assert!(wal_len(&lib) > 0, "a write-through must leave WAL frames");

    writer.checkpoint_truncate().unwrap();
    assert_eq!(wal_len(&lib), 0, "TRUNCATE checkpoint empties the WAL");
    assert_eq!(writer.journal_mode().unwrap(), "wal", "still WAL mode");

    // Rows survive the checkpoint and are visible to a fresh connection.
    let reader = LibraryReader::open(&lib).unwrap();
    assert_eq!(reader.count_playables().unwrap(), 1);
    drop(reader);
    drop(writer);
}

#[test]
fn checkpoint_with_a_concurrent_reader_does_not_panic() {
    let (_tmp, lib) = library();
    let mut writer = LibraryWriter::open(&lib).unwrap();
    write_playlist(lib.path(), "Shared");
    writer.ingest_noudata("Playlists/_Shared.noudata").unwrap();

    // The engine / UI keep a long-lived read-only connection open in production.
    let reader = LibraryReader::open(&lib).unwrap();
    writer.checkpoint_truncate().unwrap();
    assert_eq!(reader.count_playables().unwrap(), 1);
}
