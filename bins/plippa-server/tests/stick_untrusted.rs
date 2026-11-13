//! SECFIX_05: a stick prepared by somebody else must not crash, hang or poison the server.
//!
//! The index file, its side files and the `.noudata` documents are all untrusted input. The
//! SQL-level attacks (foreign schema, poisoned rows) are in
//! `crates/plippa-noubin/tests/index_untrusted.rs`; this replays the stick as the server
//! meets it at start-up.

use plippa_noubin::{index_db_path, LibraryRoot, LibraryWriter};
use plippa_server::{RecordingIpc, ServerService};
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

fn mkfifo(path: &Path) {
    extern "C" {
        fn mkfifo(path: *const std::os::raw::c_char, mode: u32) -> i32;
    }
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { mkfifo(c.as_ptr(), 0o600) }, 0);
}

/// `{"x":[0,0,...]}` just under the 8 MiB cap (the review's PAR-05 document).
fn write_nested_array_bomb(path: &Path) {
    let mut f = std::io::BufWriter::new(fs::File::create(path).unwrap());
    f.write_all(br#"{"x":["#).unwrap();
    let chunk = b"0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,";
    let target = 8 * 1024 * 1024 - 64;
    let mut written = 6;
    while written + chunk.len() < target {
        f.write_all(chunk).unwrap();
        written += chunk.len();
    }
    f.write_all(b"0]}").unwrap();
}


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
        r#"{"noubinKey":"local:good","noubinKeyNormalised":"local:good",
            "playableItemTitle":"Good Song","mediaList":[{"localFilePaths":["Good.wav"]}]}"#,
    )
    .unwrap();
    // Hostile documents: absolute and escaping paths, a FIFO, a symlink, a memory bomb.
    fs::write(
        music.join("abs.noudata"),
        r#"{"playableItemTitle":"Absolute","mediaList":[{"localFilePaths":["/etc/hostname"]}]}"#,
    )
    .unwrap();
    fs::write(
        music.join("dotdot.noudata"),
        r#"{"playableItemTitle":"Escape","mediaList":[{"localFilePaths":["../../../etc/hostname"]}]}"#,
    )
    .unwrap();
    mkfifo(&music.join("fifo.noudata"));
    fs::write(tmp.path().join("outside.noudata"), r#"{"playableItemTitle":"Outside","mediaList":[{"localFilePaths":["Good.wav"]}]}"#).unwrap();
    symlink(tmp.path().join("outside.noudata"), music.join("link.noudata")).unwrap();
    write_nested_array_bomb(&music.join("bomb.noudata"));

    // A previous (legitimate) index, then damaged, with garbage side files next to it.
    {
        let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
        w.full_scan().unwrap();
    }
    let db = index_db_path(&LibraryRoot::new(&root));
    let mut bytes = fs::read(&db).unwrap();
    assert!(bytes.len() > 16 * 1024);
    for b in &mut bytes[8192..16384] {
        *b = 0xFF;
    }
    fs::write(&db, bytes).unwrap();
    let plippa = root.join(".plippa");
    fs::write(plippa.join("library.sqlite-wal"), vec![0xABu8; 8192]).unwrap();
    fs::write(plippa.join("library.sqlite-shm"), vec![0xCDu8; 32768]).unwrap();
    fs::write(plippa.join("library.sqlite-journal"), vec![0xEFu8; 4096]).unwrap();
    (tmp, root)
}

#[test]
fn the_server_opens_a_hostile_stick_and_keeps_serving() {
    let (_tmp, root) = hostile_stick();
    let (tx, rx) = mpsc::channel();
    let r2 = root.clone();
    std::thread::spawn(move || {
        let mut svc = ServerService::start(
            Some(r2),
            Box::new(RecordingIpc::new()),
            ServerService::default_mock_associate(),
        );
        let list = svc.list_playables().expect("the server keeps serving");
        let detail = list
            .iter()
            .find(|p| p.title.as_deref() == Some("Good Song"))
            .map(|p| svc.get_playable(p.id).is_ok());
        let _ = tx.send((list, detail));
    });
    let (list, detail) = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the server hung on a hostile stick");

    // The normal track is there (the library was rebuilt from the media) and works.
    assert!(
        list.iter().any(|p| p.title.as_deref() == Some("Good Song")),
        "{:?}",
        list.iter().map(|p| &p.title).collect::<Vec<_>>()
    );
    assert_eq!(detail, Some(true));
    // Nothing hostile surfaced as a playable path, and the bomb / symlink / FIFO were not indexed.
    for p in &list {
        for m in &p.media_relpaths {
            assert!(!m.starts_with('/') && !m.contains(".."), "{m}");
        }
        assert!(
            !matches!(p.title.as_deref(), Some("Outside")),
            "a symlinked .noudata was followed"
        );
        assert!(!p.noudata_relpath.contains("bomb") && !p.noudata_relpath.contains("fifo"));
    }
    // The planted side files are gone (a fresh WAL/SHM of the server's own connection may
    // exist, with another inode), and the damaged index was replaced by a readable one.
    for (name, planted_len) in [("library.sqlite-wal", 8192u64), ("library.sqlite-shm", 32768), ("library.sqlite-journal", 4096)] {
        let p = root.join(".plippa").join(name);
        if let Ok(m) = fs::metadata(&p) {
            assert_ne!(
                (m.len(), fs::read(&p).map(|b| b.iter().all(|x| *x == 0xAB || *x == 0xCD || *x == 0xEF)).unwrap_or(false)),
                (planted_len, true),
                "{name}: the planted file is still there"
            );
        }
    }
    let reader = plippa_noubin::LibraryReader::open(&LibraryRoot::new(&root)).unwrap();
    assert!(!reader.list_playables().unwrap().is_empty());
}

fn serve_titles(root: PathBuf) -> Vec<plippa_server::PlayableSummary> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut svc = ServerService::start(
            Some(root),
            Box::new(RecordingIpc::new()),
            ServerService::default_mock_associate(),
        );
        let _ = tx.send(svc.list_playables().expect("the server keeps serving"));
    });
    rx.recv_timeout(Duration::from_secs(60)).expect("the server hung")
}

#[test]
fn a_poisoned_index_row_is_not_believed_by_the_server() {
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
    // Control character in the title, absolute media path: same lengths, fingerprints intact.
    patch_index(&root, "Good Song", "Good\u{7}Song");
    patch_index(&root, "Music/Good.wav", "/etc//hostname");
    let list = serve_titles(root);
    for p in &list {
        let title = p.title.clone().unwrap_or_default();
        assert!(!title.chars().any(char::is_control), "{title:?}");
        for m in &p.media_relpaths {
            assert!(!m.contains("hostname"), "{m}");
        }
    }
    assert!(list.iter().any(|p| p.title.as_deref() == Some("Good Song")), "healed from the .noudata");
}

#[test]
fn a_hot_wal_planted_next_to_the_index_is_not_replayed_by_the_server() {
    let tmp = tempfile::tempdir().unwrap();
    let root = stick_with_foreign_hot_wal(tmp.path());
    let list = serve_titles(root);
    assert!(!list.iter().any(|p| p.title.as_deref().unwrap_or("").contains("PLANTED")), "{:?}", list.iter().map(|p| &p.title).collect::<Vec<_>>());
    assert!(list.iter().any(|p| p.title.as_deref() == Some("Good Song")));
}

/// A path that reads index rows with NO scan in front of it: the waveform worker's reader.
/// A poisoned media path with a matching fingerprint must not become a work item.
#[test]
fn the_waveform_worker_does_not_believe_a_poisoned_media_path() {
    use plippa_server::waveform::{WaveformRegistry, WaveformWorker};
    use plippa_server::ScanPlan;
    use std::sync::Arc;
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
    // Absolute media path, same length, fingerprints intact; nothing re-scans it.
    patch_index(&root, "Music/Good.wav", "/etc//hostname");
    let mut worker = WaveformWorker::new(Arc::new(WaveformRegistry::new()));
    worker.refresh(&ScanPlan { roots: vec![root.clone()], playing: false, current_waveform: None });
    for item in worker.queue() {
        assert!(!item.relpath.contains("hostname"), "{}", item.relpath);
        assert!(item.media.starts_with(&root), "{}", item.media.display());
    }
}
