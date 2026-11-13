//! Media library scan + SQLite index (MVP_03 / Noubin §4.6).
//!
//! Real media comes from `assets/testMedia/` (Bandcamp dumps). Tests materialise a
//! temp library root by symlinking those format folders and copying fixture `.noudata`.

use plippa_noubin::{
    index_db_path, library_root_available, normalise_noubin_key, LibraryError, LibraryReader,
    LibraryRoot, LibraryWriter, LookupResult, PLAYER_DB_DIR,
};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

fn repo_test_media() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/testMedia")
}

fn fixture_library(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/libraries")
        .join(name)
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

fn writable_fixture(name: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join(name);
    copy_dir(&fixture_library(name), &root);
    (tmp, root)
}

/// Bandcamp dump as a Noubin Media Library: symlink format folders + fixture `.noudata`.
fn grooves_bandcamp_library() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let media = repo_test_media();
    assert!(
        media.is_dir(),
        "expected assets/testMedia at {} (run scripts/fetch-testmedia.sh)",
        media.display()
    );

    for name in ["AAC", "FLAC", "MP3 320", "MP3 V0", "OGG", "WAV"] {
        let src = media.join(name);
        assert!(src.is_dir(), "missing format folder {}", src.display());
        symlink(&src, root.join(name)).unwrap();
    }

    // Copy only metadata / dot-dirs from the fixture (not the Music/ placeholder tree).
    let fix = fixture_library("grooves_for_machines");
    for name in [
        "_Grooves For Machines To Study.noudata",
        "Bogan Mongrel - Grooves For Machines To Study - 02 Beat For Machines To Study.noudata",
    ] {
        fs::copy(fix.join(name), root.join(name)).unwrap();
    }
    fs::create_dir_all(root.join(".should_ignore")).unwrap();
    fs::copy(
        fix.join(".should_ignore/hidden.noudata"),
        root.join(".should_ignore/hidden.noudata"),
    )
    .unwrap();

    (tmp, root)
}

const ALBUM_KEY: &str = "boganmongrel.bandcamp.com/album/grooves-for-machines-to-study/";
const TRACK_KEY: &str = "beat for machines to study.noubin.gfms02";

#[test]
fn index_empty_library() {
    let (_tmp, root) = writable_fixture("empty_library");
    let lib = LibraryRoot::new(&root);
    let mut writer = LibraryWriter::open(&lib).unwrap();
    let report = writer.full_scan().unwrap();
    assert_eq!(report.scanned_files, 0);
    assert_eq!(report.ingested, 0);
    assert!(root.join(PLAYER_DB_DIR).is_dir());
    assert!(index_db_path(&lib).is_file());
    assert_eq!(writer.journal_mode().unwrap(), "wal");
}

#[test]
fn index_media_only_no_noudata() {
    // Symlink WAV folder only — Bandcamp media, no `.noudata`.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    symlink(repo_test_media().join("WAV"), root.join("WAV")).unwrap();

    let lib = LibraryRoot::new(root);
    let mut writer = LibraryWriter::open(&lib).unwrap();
    let report = writer.full_scan().unwrap();
    assert_eq!(report.scanned_files, 0);
    assert_eq!(report.ingested, 0);
}

#[test]
fn index_album_and_lookup_by_normalised_key() {
    let (_tmp, root) = grooves_bandcamp_library();
    let lib = LibraryRoot::new(&root);
    let mut writer = LibraryWriter::open(&lib).unwrap();
    let report = writer.full_scan().unwrap();
    assert_eq!(report.ingested, 2, "album + single-track noudata; warnings={:?}", report.warnings);
    assert!(root.join(PLAYER_DB_DIR).join("library.sqlite").is_file());
    // Index lives on the library (media) root, not elsewhere.
    assert_eq!(
        index_db_path(&lib),
        root.join(PLAYER_DB_DIR).join("library.sqlite")
    );

    let reader = LibraryReader::open(&lib).unwrap();
    assert_eq!(reader.journal_mode().unwrap(), "wal");

    match reader.lookup_by_normalised_key(ALBUM_KEY).unwrap() {
        LookupResult::Found(hit) => {
            assert!(!hit.multi_match);
            assert_eq!(
                hit.first().title.as_deref(),
                Some("Grooves For Machines To Study")
            );
            assert_eq!(
                hit.first().noudata_relpath,
                "_Grooves For Machines To Study.noudata"
            );
            assert!(hit.first().media_relpaths.len() >= 3);
            assert!(hit.first().media_relpaths.iter().any(|p| p.contains("02 Beat")));
        }
        LookupResult::NotFound => panic!("expected album hit"),
    }
}

#[test]
fn lookup_miss_is_not_found() {
    let (_tmp, root) = grooves_bandcamp_library();
    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    let reader = LibraryReader::open(&lib).unwrap();
    assert!(matches!(
        reader.lookup_by_normalised_key("no.such.key").unwrap(),
        LookupResult::NotFound
    ));
}

#[test]
fn lookup_local_text_key_and_url_style() {
    let (_tmp, root) = grooves_bandcamp_library();
    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    let reader = LibraryReader::open(&lib).unwrap();

    // Local text key on the single-track `.noudata`.
    match reader.lookup_by_normalised_key(TRACK_KEY).unwrap() {
        LookupResult::Found(hit) => {
            assert_eq!(
                hit.first().title.as_deref(),
                Some("Beat For Machines To Study")
            );
        }
        LookupResult::NotFound => panic!("local key miss"),
    }

    // URL-style album key (already normalised in fixture; also verify §6.2 agrees).
    let expected_url = normalise_noubin_key(
        "https://boganmongrel.bandcamp.com/album/grooves-for-machines-to-study",
    );
    assert_eq!(expected_url, ALBUM_KEY);
    assert!(matches!(
        reader.lookup_by_normalised_key(&expected_url).unwrap(),
        LookupResult::Found(_)
    ));
}

#[test]
fn scan_skips_dot_directories() {
    let (_tmp, root) = grooves_bandcamp_library();
    let lib = LibraryRoot::new(&root);
    let report = LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    // album + track noudata only; hidden under `.should_ignore` excluded
    assert_eq!(report.scanned_files, 2);
    assert_eq!(report.ingested, 2);
}

#[test]
fn lazy_rescan_adds_and_removes() {
    let (_tmp, root) = grooves_bandcamp_library();
    let lib = LibraryRoot::new(&root);
    let mut writer = LibraryWriter::open(&lib).unwrap();
    writer.full_scan().unwrap();

    let extra = root.join("extra-single.noudata");
    fs::write(
        &extra,
        r#"{
          "noubinKeyNormalised":"extra-key",
          "playableItemTitle":"Extra",
          "mediaList":[{"localFilePaths":["MP3 V0/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3"]}]
        }"#,
    )
    .unwrap();
    thread::sleep(Duration::from_millis(20));

    let add = writer.lazy_rescan().unwrap();
    assert_eq!(add.ingested, 1);

    let reader = LibraryReader::open(&lib).unwrap();
    assert!(matches!(
        reader.lookup_by_normalised_key("extra-key").unwrap(),
        LookupResult::Found(_)
    ));

    fs::remove_file(&extra).unwrap();
    let rem = writer.lazy_rescan().unwrap();
    assert_eq!(rem.removed, 1);
    assert!(matches!(
        reader.lookup_by_normalised_key("extra-key").unwrap(),
        LookupResult::NotFound
    ));
}

#[test]
fn wal_mode_enabled_on_open() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = LibraryRoot::new(tmp.path());
    let writer = LibraryWriter::open(&lib).unwrap();
    assert_eq!(writer.journal_mode().unwrap(), "wal");
}

#[test]
fn reader_lookup_while_writer_holds_transaction() {
    let (_tmp, root) = grooves_bandcamp_library();
    let lib = LibraryRoot::new(&root);
    let mut writer = LibraryWriter::open(&lib).unwrap();
    writer.full_scan().unwrap();

    let db = index_db_path(&lib);
    let write_conn = rusqlite::Connection::open(&db).unwrap();
    write_conn
        .busy_timeout(Duration::from_millis(5_000))
        .unwrap();
    write_conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    write_conn
        .execute(
            "UPDATE playables SET title = title || ' (editing)' WHERE id = 1",
            [],
        )
        .unwrap();

    let reader = LibraryReader::open(&lib).unwrap();
    let result = reader.lookup_by_normalised_key(ALBUM_KEY).unwrap();
    assert!(matches!(result, LookupResult::Found(_)));

    write_conn.execute_batch("COMMIT;").unwrap();
}

#[test]
fn missing_root_returns_structured_error() {
    let lib = LibraryRoot::new("/tmp/plippa-no-such-library-root-mvp03");
    match LibraryWriter::open(&lib) {
        Ok(_) => panic!("expected MissingRoot"),
        Err(err) => assert!(matches!(err, LibraryError::MissingRoot { .. })),
    }
    assert!(library_root_available(&lib).is_err());
}

#[test]
fn hot_swap_vanished_path_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(root.join("Music")).unwrap();
    fs::write(
        root.join("Music/a.noudata"),
        r#"{"noubinKeyNormalised":"k","playableItemTitle":"A","mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    )
    .unwrap();

    let lib = LibraryRoot::new(&root);
    let mut writer = LibraryWriter::open(&lib).unwrap();
    writer.full_scan().unwrap();

    fs::remove_dir_all(&root).unwrap();

    let err = writer.lazy_rescan().unwrap_err();
    assert!(
        matches!(
            err,
            LibraryError::StorageVanished { .. }
                | LibraryError::MissingRoot { .. }
                | LibraryError::Io(_)
        ),
        "unexpected error: {err:?}"
    );
}

/// An unclean USB unplug can leave `.plippa/library.sqlite` damaged. The index is **derived**
/// from the `.noudata` files, so opening it must delete the damaged files and rebuild them
/// instead of leaving the player with a permanently empty library.
#[test]
fn a_damaged_index_is_deleted_and_rebuilt_on_open() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.mp3"), b"RIFF").unwrap();
    fs::write(
        root.join("a.noudata"),
        r#"{"noudataType":"looseTrack","playableItemProfile":"music",
           "noubinKeyNormalised":"k","playableItemTitle":"A",
           "mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    )
    .unwrap();

    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();

    // Simulate the damage an unplug mid-write can leave behind: a damaged main file plus a
    // stale `-wal` from the gone mount. The rebuild has to discard both.
    let dir = root.join(PLAYER_DB_DIR);
    fs::write(index_db_path(&lib), b"this is not a sqlite database").unwrap();
    fs::write(dir.join("library.sqlite-wal"), b"stale wal").unwrap();
    fs::write(dir.join("library.sqlite-shm"), b"stale shm").unwrap();

    let mut writer = LibraryWriter::open(&lib).expect("damaged index is rebuilt");
    writer.full_scan().unwrap();
    let reader = LibraryReader::open(&lib).unwrap();
    let rows = reader.list_playables().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title.as_deref(), Some("A"));
}

#[test]
fn open_existing_noubin_media_library() {
    let (_tmp, root) = grooves_bandcamp_library();
    assert!(root
        .join("_Grooves For Machines To Study.noudata")
        .is_file());
    assert!(root
        .join("MP3 V0/Bogan Mongrel - Grooves For Machines To Study - 02 Beat For Machines To Study.mp3")
        .is_file());

    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    let reader = LibraryReader::open(&lib).unwrap();
    assert!(matches!(
        reader.lookup_by_normalised_key(ALBUM_KEY).unwrap(),
        LookupResult::Found(_)
    ));
}

#[test]
fn rebuild_deterministic_lookup() {
    let (_tmp, root) = grooves_bandcamp_library();
    let lib = LibraryRoot::new(&root);
    let mut w = LibraryWriter::open(&lib).unwrap();
    w.full_scan().unwrap();
    let first = match LibraryReader::open(&lib)
        .unwrap()
        .lookup_by_normalised_key(ALBUM_KEY)
        .unwrap()
    {
        LookupResult::Found(h) => h.first().clone(),
        LookupResult::NotFound => panic!("missing"),
    };

    fs::remove_dir_all(root.join(PLAYER_DB_DIR)).unwrap();
    let mut w2 = LibraryWriter::open(&lib).unwrap();
    w2.full_scan().unwrap();
    let second = match LibraryReader::open(&lib)
        .unwrap()
        .lookup_by_normalised_key(ALBUM_KEY)
        .unwrap()
    {
        LookupResult::Found(h) => h.first().clone(),
        LookupResult::NotFound => panic!("missing after rebuild"),
    };

    assert_eq!(first.noudata_relpath, second.noudata_relpath);
    assert_eq!(first.noubin_key_normalised, second.noubin_key_normalised);
    assert_eq!(first.title, second.title);
    assert_eq!(first.media_relpaths, second.media_relpaths);
}

#[test]
fn derive_missing_normalised_key_write_back() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    symlink(repo_test_media().join("MP3 V0"), root.join("MP3 V0")).unwrap();

    // Writable noudata beside the symlink tree (not inside the assets dir).
    fs::write(
        root.join("needs-norm.noudata"),
        r#"{
          "noubinKey": "HTTPS://BoganMongrel.bandcamp.com/track/beat-for-machines-to-study?from=album",
          "playableItemTitle": "Beat For Machines To Study",
          "mediaList": [{
            "localFilePaths": ["MP3 V0/Bogan Mongrel - Grooves For Machines To Study - 02 Beat For Machines To Study.mp3"]
          }]
        }"#,
    )
    .unwrap();

    let lib = LibraryRoot::new(root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();

    let expected = normalise_noubin_key(
        "HTTPS://BoganMongrel.bandcamp.com/track/beat-for-machines-to-study?from=album",
    );
    let reader = LibraryReader::open(&lib).unwrap();
    assert!(matches!(
        reader.lookup_by_normalised_key(&expected).unwrap(),
        LookupResult::Found(_)
    ));

    let written = fs::read_to_string(root.join("needs-norm.noudata")).unwrap();
    assert!(written.contains("noubinKeyNormalised"));
    assert!(written.contains(&expected));
}
