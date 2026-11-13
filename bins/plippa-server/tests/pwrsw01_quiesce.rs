//! PWRSW_01: orderly server stop — bound the WAL while running, checkpoint and close on
//! shutdown so the USB stick is safe to remove once the host has powered off.

use plippa_noubin::{
    index_db_path, new_playlist_document, unique_playlist_path, write_noudata_atomic, LibraryReader,
    LibraryRoot, LibraryWriter, WriteGuard, PLAYLISTS_DIR,
};
use plippa_server::{
    spawn_background_scanner, CreatePlaylist, RecordingIpc, ServerError, ServerService,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn library() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    fs::create_dir_all(&root).unwrap();
    (tmp, root)
}

fn service(root: &Path) -> Mutex<ServerService> {
    Mutex::new(ServerService::start(
        Some(root.to_path_buf()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    ))
}

fn wal_path(root: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", index_db_path(&LibraryRoot::new(root)).display()))
}

fn playlist(title: &str) -> CreatePlaylist {
    CreatePlaylist {
        title: title.into(),
        tracks: Vec::new(),
        created_by: None,
    }
}

/// Write a playlist file straight to the stick (no service call, so no checkpoint).
fn write_playlist_file(root: &Path, title: &str) {
    let dir = root.join(PLAYLISTS_DIR);
    fs::create_dir_all(&dir).unwrap();
    let path = unique_playlist_path(&dir, title, None);
    let doc = new_playlist_document(title, "pwrsw01");
    write_noudata_atomic(&doc, &path, WriteGuard::MustNotExist).unwrap();
}

#[test]
fn changed_scan_leaves_no_wal_frames() {
    let (_tmp, root) = library();
    let svc = service(&root);

    svc.lock()
        .unwrap()
        .create_playlist(playlist("Bounded"))
        .unwrap();
    let wal = wal_path(&root);
    let len = fs::metadata(&wal).map_or(0, |m| m.len());
    assert_eq!(len, 0, "a changed scan must checkpoint the WAL ({})", wal.display());

    // The rows are still visible from a fresh reader after the checkpoint.
    let reader = LibraryReader::open(&LibraryRoot::new(&root)).unwrap();
    assert_eq!(reader.count_playables().unwrap(), 1);
}

#[test]
fn no_change_rescan_does_not_checkpoint() {
    let (_tmp, root) = library();
    // The service's initial full scan indexes this and checkpoints (WAL -> 0 bytes).
    write_playlist_file(&root, "SeenAtStartup");
    let svc = service(&root);
    assert_eq!(fs::metadata(wal_path(&root)).map_or(0, |m| m.len()), 0);

    // A second file indexed through another writer leaves real WAL frames behind.
    write_playlist_file(&root, "Uncheckpointed");
    let mut outside = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    assert!(outside.lazy_rescan().unwrap().changed());
    drop(outside);
    let wal = wal_path(&root);
    let frames = fs::metadata(&wal).map_or(0, |m| m.len());
    assert!(
        frames > 0,
        "an un-checkpointed index write must leave WAL frames ({})",
        wal.display()
    );

    // A rescan that changes nothing must leave those frames alone (it is a no-op, not a
    // checkpoint hook — checkpointing here would hammer the stick on the 30 s timer).
    assert!(!svc.lock().unwrap().lazy_rescan().unwrap().changed());
    let frames = fs::metadata(&wal).map_or(0, |m| m.len());
    assert!(
        frames > 0,
        "a no-change rescan must not truncate the WAL ({})",
        wal.display()
    );
}

#[test]
fn quiesce_closes_libraries_and_refuses_new_work() {
    let (_tmp, root) = library();
    let svc = service(&root);
    svc.lock()
        .unwrap()
        .create_playlist(playlist("BeforeShutdown"))
        .unwrap();

    {
        let mut s = svc.lock().unwrap();
        assert!(!s.quiescing());
        s.quiesce();
        assert!(s.quiescing());
        assert!(
            s.status().library_roots.is_empty(),
            "quiesce must drop the open libraries"
        );
        // New work is refused rather than reopening the volume.
        let err = s.create_playlist(playlist("AfterShutdown")).unwrap_err();
        assert!(matches!(err, ServerError::Unavailable(_)), "{err:?}");
    }

    let len = fs::metadata(wal_path(&root)).map_or(0, |m| m.len());
    assert_eq!(len, 0, "quiesce checkpoints and truncates the WAL");
    let reader = LibraryReader::open(&LibraryRoot::new(&root)).unwrap();
    assert_eq!(reader.count_playables().unwrap(), 1);
}

#[test]
fn background_scanner_returns_promptly_when_stopped() {
    let (_tmp, root) = library();
    let svc = Arc::new(service(&root));
    let stop = Arc::new(AtomicBool::new(false));
    let handle = spawn_background_scanner(Arc::clone(&svc), Duration::from_secs(30), Arc::clone(&stop));
    stop.store(true, Ordering::SeqCst);
    let started = Instant::now();
    handle.join().expect("scanner thread");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a 30 s interval must not delay shutdown (took {:?})",
        started.elapsed()
    );
}
