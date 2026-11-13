//! SECFIX_05 (SYS-04): the on-device UI opens the library index too, so it meets a hostile
//! stick the same way the server and the engine do: no crash, no hang, nothing poisoned.
//!
//! The SQL-level attacks (foreign schema, planted WAL, poisoned rows) are replayed in
//! `crates/plippa-noubin/tests/index_untrusted.rs`; the UI reaches them through the same
//! `LibraryReader` / `LibraryWriter` choke points.

use plippa_noubin::{index_db_path, LibraryRoot, LibraryWriter};
use plippa_ui::LibrarySource;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;


/// Replace every occurrence of `from` with `to` (same length) in the index file: the rows keep
/// their mtime/size fingerprint, so a scan would trust them. No SQL needed.
fn patch_index(root: &Path, from: &str, to: &str) {
    assert_eq!(from.len(), to.len());
    let db = index_db_path(&LibraryRoot::new(root));
    let mut bytes = fs::read(&db).unwrap();
    let mut n = 0;
    let mut i = 0;
    while i + from.len() <= bytes.len() {
        if &bytes[i..i + from.len()] == from.as_bytes() {
            bytes[i..i + from.len()].copy_from_slice(to.as_bytes());
            n += 1;
            i += from.len();
        } else {
            i += 1;
        }
    }
    assert!(n > 0, "{from:?} not found in the index file");
    fs::write(&db, bytes).unwrap();
}

/// A stick whose index carries a **hot WAL from another library** (a writer on a donor stick
/// stays open while its `.plippa` files are copied, so the WAL is not folded into the main file).
fn stick_with_foreign_hot_wal(tmp: &Path) -> PathBuf {
    let donor = tmp.join("donor");
    fs::create_dir_all(donor.join("Music")).unwrap();
    fs::write(donor.join("Music/Good.wav"), b"RIFF").unwrap();
    // Same path and size as the real document, other title: the donor's rows would be
    // trusted (matching fingerprint) if its WAL were replayed.
    fs::write(
        donor.join("Music/_good.noudata"),
        r#"{"playableItemTitle":"PLANTED!!","mediaList":[{"localFilePaths":["Good.wav"]}]}"#,
    )
    .unwrap();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&donor)).unwrap();
    w.full_scan().unwrap();
    let root = tmp.join("stick");
    fs::create_dir_all(root.join("Music")).unwrap();
    fs::write(root.join("Music/Good.wav"), b"RIFF").unwrap();
    fs::write(
        root.join("Music/_good.noudata"),
        r#"{"playableItemTitle":"Good Song","mediaList":[{"localFilePaths":["Good.wav"]}]}"#,
    )
    .unwrap();
    let mtime = fs::metadata(donor.join("Music/_good.noudata")).unwrap().modified().unwrap();
    fs::File::open(root.join("Music/_good.noudata")).unwrap().set_modified(mtime).unwrap();
    fs::create_dir_all(root.join(".plippa")).unwrap();
    for name in ["library.sqlite", "library.sqlite-wal", "library.sqlite-shm"] {
        fs::copy(donor.join(".plippa").join(name), root.join(".plippa").join(name)).unwrap();
    }
    assert!(root.join(".plippa/library.sqlite-wal").metadata().unwrap().len() > 0, "hot WAL");
    drop(w);
    root
}

fn hostile_stick() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("stick");
    let music = root.join("Music");
    fs::create_dir_all(&music).unwrap();
    fs::write(music.join("Good.wav"), b"RIFF").unwrap();
    fs::write(
        music.join("_good.noudata"),
        r#"{"playableItemTitle":"Good Song","mediaList":[{"localFilePaths":["Good.wav"]}]}"#,
    )
    .unwrap();
    fs::write(
        music.join("abs.noudata"),
        r#"{"playableItemTitle":"Absolute","mediaList":[{"localFilePaths":["/etc/hostname"]}]}"#,
    )
    .unwrap();
    {
        let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
        w.full_scan().unwrap();
    }
    // Damage the index and plant side files next to it.
    let db = index_db_path(&LibraryRoot::new(&root));
    let mut bytes = fs::read(&db).unwrap();
    for b in &mut bytes[8192..16384] {
        *b = 0xFF;
    }
    fs::write(&db, bytes).unwrap();
    fs::write(root.join(".plippa/library.sqlite-wal"), vec![0xABu8; 8192]).unwrap();
    (tmp, root)
}

#[test]
fn the_ui_opens_a_hostile_stick_without_crashing_or_hanging() {
    let (_tmp, root) = hostile_stick();
    let (tx, rx) = mpsc::channel();
    let r2 = root.clone();
    std::thread::spawn(move || {
        // The UI's reader refuses a damaged index: an empty library, not a panic.
        let mut src = LibrarySource::open(Some(&r2));
        let before = src.list_items().len();
        // The server (or the UI's own writer) rebuilds the index; the UI reopens.
        LibraryWriter::open(&LibraryRoot::new(&r2))
            .unwrap()
            .full_scan()
            .unwrap();
        src.reopen();
        let titles: Vec<String> = src.list_items().into_iter().map(|i| i.title).collect();
        let _ = tx.send((before, titles));
    });
    let (before, titles) = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the UI hung on a hostile stick");
    assert_eq!(before, 0, "a damaged index must not be read");
    assert!(titles.iter().any(|t| t == "Good Song"), "{titles:?}");
}

#[test]
fn the_ui_reader_does_not_believe_a_poisoned_row_or_replay_a_planted_wal() {
    // Poisoned row: control character in the title, same fingerprints.
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("stick");
    fs::create_dir_all(root.join("Music")).unwrap();
    fs::write(root.join("Music/Good.wav"), b"RIFF").unwrap();
    fs::write(
        root.join("Music/_good.noudata"),
        r#"{"playableItemTitle":"Good Song","mediaList":[{"localFilePaths":["Good.wav"]}]}"#,
    )
    .unwrap();
    {
        let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
        w.full_scan().unwrap();
    }
    patch_index(&root, "Good Song", "Good\u{7}Song");
    let src = LibrarySource::open(Some(&root));
    let titles: Vec<String> = src.list_items().into_iter().map(|i| i.title).collect();
    assert!(!titles.is_empty());
    assert!(titles.iter().all(|t| !t.chars().any(char::is_control)), "{titles:?}");

    // Planted hot WAL from another library.
    let tmp2 = tempfile::tempdir().unwrap();
    let root2 = stick_with_foreign_hot_wal(tmp2.path());
    let src = LibrarySource::open(Some(&root2));
    let titles: Vec<String> = src.list_items().into_iter().map(|i| i.title).collect();
    assert!(!titles.iter().any(|t| t.contains("PLANTED")), "{titles:?}");
}
