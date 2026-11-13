//! NB_02: background library scanner + orphan / warning surfacing in the server.

use plippa_ipc_proto::IpcMessage;
use plippa_noubin::{LibraryKind, LibraryReader, LibraryRoot, MountIdentity};
use plippa_server::{BackgroundScanner, MetadataPatch, RecordingIpc, ServerService};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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

fn service(root: &Path) -> (Mutex<ServerService>, RecordingIpc) {
    let ipc = RecordingIpc::new();
    let view = ipc.clone();
    let svc = ServerService::start(
        Some(root.to_path_buf()),
        Box::new(ipc),
        ServerService::default_mock_associate(),
    );
    (Mutex::new(svc), view)
}

fn updates(ipc: &RecordingIpc) -> Vec<u64> {
    ipc.messages()
        .iter()
        .filter_map(|m| match m {
            IpcMessage::LibraryIndexUpdated(u) => Some(u.generation),
            _ => None,
        })
        .collect()
}

fn write_track(root: &Path, stem: &str, title: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(format!("{stem}.wav")), b"RIFF").unwrap();
    fs::write(
        root.join(format!("_{stem}.noudata")),
        format!(
            r#"{{"noubinKey":"local:{stem}","noubinKeyNormalised":"local:{stem}",
               "playableItemTitle":"{title}",
               "mediaList":[{{"localFilePaths":["{stem}.wav"]}}]}}"#
        ),
    )
    .unwrap();
}

/// Simulate unplug + remount at the same path while the old connection is still open. A real
/// remount always looks different; a plain directory only gets a fresh inode, and ext4 hands
/// a freed inode straight back out, so park the old inode on a decoy directory.
fn remount(root: &Path, old: &MountIdentity) {
    let parent = root.parent().expect("root has a parent");
    for attempt in 0..32 {
        fs::remove_dir_all(root).unwrap();
        fs::create_dir_all(parent.join(format!(".decoy-{attempt}"))).unwrap();
        fs::create_dir_all(root).unwrap();
        if MountIdentity::of(root).as_ref() != Some(old) {
            return;
        }
    }
    panic!("the filesystem kept reusing the same directory identity");
}

#[test]
fn library_background_tick_notifies_only_on_real_change() {
    let (_tmp, root) = kinds_library();
    let (svc, ipc) = service(&root);
    let start_gen = *updates(&ipc).last().expect("initial scan notifies");
    ipc.clear();

    let mut scanner = BackgroundScanner::new();
    let mut no_probe = |_: &Path| None;

    // Nothing changed on disk: no notify, generation unchanged.
    scanner.tick(&svc, &mut no_probe);
    scanner.tick(&svc, &mut no_probe);
    assert!(updates(&ipc).is_empty(), "{:?}", ipc.messages());
    assert_eq!(svc.lock().unwrap().status().generation, start_gen);

    // A dropped-in file: exactly one notify with a bumped generation.
    fs::write(
        root.join("Playlists/New.noudata"),
        r#"{"playableItemTitle":"New","mediaList":[{"localFilePaths":["../Mixes/Japanese/mix.mp3"]}]}"#,
    )
    .unwrap();
    scanner.tick(&svc, &mut no_probe);
    assert_eq!(updates(&ipc), vec![start_gen + 1]);
    scanner.tick(&svc, &mut no_probe);
    assert_eq!(updates(&ipc).len(), 1, "steady state is quiet again");

    let list = svc.lock().unwrap().list_playables().unwrap();
    assert!(list.iter().any(|p| p.title.as_deref() == Some("New")));
}

#[test]
fn library_background_tick_probes_lengths_and_writes_them_back() {
    let (_tmp, root) = kinds_library();
    let (svc, ipc) = service(&root);
    ipc.clear();
    let mut scanner = BackgroundScanner::new();
    let mut probe = |p: &Path| p.extension().map(|_| 90_000u64);

    // Batches are throttled (PROBE_BATCH per tick): a few ticks measure everything.
    for _ in 0..6 {
        scanner.tick(&svc, &mut probe);
    }
    assert!(!updates(&ipc).is_empty(), "measured lengths are announced");
    let text = fs::read_to_string(root.join("Podcasts/Show/_Show.noudata")).unwrap();
    assert!(text.contains("expectedLength"), "{text}");

    let reader = LibraryReader::open(&LibraryRoot::new(&root)).unwrap();
    let show = reader.list_by_kind(LibraryKind::Podcast).unwrap();
    assert_eq!(show[0].duration_ms, Some(90_000));

    // Once everything is measured the scanner goes quiet.
    ipc.clear();
    scanner.tick(&svc, &mut probe);
    assert!(updates(&ipc).is_empty());
}

#[test]
fn library_background_tick_survives_unplug() {
    let (tmp, root) = kinds_library();
    let (svc, ipc) = service(&root);
    let mut scanner = BackgroundScanner::new();
    let mut no_probe = |_: &Path| None;
    scanner.tick(&svc, &mut no_probe);
    let gen = *updates(&ipc).last().expect("initial scan notifies");
    ipc.clear();

    fs::remove_dir_all(tmp.path()).unwrap();
    scanner.tick(&svc, &mut no_probe); // must not panic
    scanner.tick(&svc, &mut no_probe);
    let status = svc.lock().unwrap().status();
    assert!(!status.available);
    assert!(status.warning.is_some());
    assert!(
        updates(&ipc).iter().any(|g| *g > gen),
        "the engine/UI must be told to close their readers on the gone root: {:?}",
        ipc.messages()
    );
}

/// A stick that is pulled and pushed back in is mounted at the same path but is a different
/// filesystem. The SQLite writer opened before the unplug keeps referring to the old mount
/// (on the device it fails every scan with `database disk image is malformed`), so the mount
/// watcher must close it, open the new one, and announce the new library to the engine/UI.
#[test]
fn a_remounted_root_is_reopened_and_announced() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("stick");
    write_track(&root, "first", "First");
    let (svc, ipc) = service(&root);
    let gen = *updates(&ipc).last().expect("initial scan notifies");
    ipc.clear();
    {
        let mut svc = svc.lock().unwrap();
        assert_eq!(svc.list_playables().unwrap()[0].title.as_deref(), Some("First"));
    }

    // Unplug + remount at the same path: a fresh directory inode and a fresh index, while
    // the old writer connection is still open.
    let old = MountIdentity::of(&root).expect("mounted stick");
    remount(&root, &old);
    write_track(&root, "second", "Second");

    svc.lock().unwrap().poll_mounts();

    assert_eq!(
        updates(&ipc),
        vec![gen + 1],
        "a remount must be announced exactly once: {:?}",
        ipc.messages()
    );
    let mut svc = svc.lock().unwrap();
    let status = svc.status();
    assert!(status.available);
    assert_eq!(status.item_count, 1);
    assert_eq!(
        svc.list_playables().unwrap()[0].title.as_deref(),
        Some("Second"),
        "the reopened index must describe the remounted stick"
    );
}

/// An unchanged mount is quiet: the 1 s watcher must not bump the generation (every bump
/// makes the engine reopen its USB readers).
#[test]
fn mount_poll_is_quiet_while_the_stick_stays_mounted() {
    let (_tmp, root) = kinds_library();
    let (svc, ipc) = service(&root);
    ipc.clear();
    svc.lock().unwrap().poll_mounts();
    svc.lock().unwrap().poll_mounts();
    assert!(updates(&ipc).is_empty(), "{:?}", ipc.messages());
}

#[test]
fn library_orphan_media_is_listed_but_not_editable() {
    let (_tmp, root) = kinds_library();
    let (svc, _ipc) = service(&root);
    let mut svc = svc.into_inner().unwrap();
    let list = svc.list_playables().unwrap();
    let orphan = list
        .iter()
        .find(|p| p.title.as_deref() == Some("orphan song"))
        .expect("orphan listed as a track with its file name");
    assert!(orphan.noudata_relpath.is_empty());
    assert!(orphan.paths_summary.contains("orphan song.mp3"));

    let id: u64 = orphan.id;
    let detail = svc.get_playable(id).unwrap();
    assert_eq!(detail.playable_item_title.as_deref(), Some("orphan song"));
    let err = svc
        .patch_metadata(
            id,
            MetadataPatch {
                playable_item_title: Some("x".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("no .noudata"), "{err}");
    assert!(svc.associate_playable(id).is_err());
}

#[test]
fn library_status_surfaces_index_warnings() {
    let (_tmp, root) = kinds_library();
    for name in ["a", "b"] {
        fs::write(
            root.join(format!("{name}.noudata")),
            r#"{"noubinKeyNormalised":"same.key","playableItemTitle":"T","mediaList":[{"localFilePaths":["Loose/orphan song.mp3"]}]}"#,
        )
        .unwrap();
    }
    let (svc, _ipc) = service(&root);
    let status = svc.lock().unwrap().status();
    assert!(
        status.index_warnings.iter().any(|w| w.contains("same.key")),
        "{:?}",
        status.index_warnings
    );
}

/// A stick plugged in **after** the service started must clear the boot-time "no media
/// library root configured" warning once its index opens. The old guard only cleared warnings
/// whose text contained "vanished", so that message stayed on screen (and in `/api/status`)
/// forever even though the library was open and the roots list was populated.
#[test]
fn a_root_that_appears_after_startup_clears_the_boot_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("not_plugged_in_yet");
    // Started while the mount does not exist: the configured root resolves to nothing.
    let (svc, _ipc) = service(&root);
    {
        let mut svc = svc.lock().unwrap();
        let status = svc.status();
        assert!(!status.available, "no root yet");
        assert!(
            status.warning.is_some(),
            "the service must say why it has no storage"
        );
    }

    // The stick is mounted (an empty, brand-new filesystem is enough).
    fs::create_dir_all(&root).unwrap();

    let mut svc = svc.into_inner().unwrap();
    let status = svc.status();
    assert!(status.available, "the root was discovered on the next status poll");
    assert_eq!(
        status.library_roots.first().map(|r| r.path.clone()),
        Some(root.display().to_string())
    );
    assert_eq!(
        status.warning, None,
        "a stale 'no media library root' warning must not outlive the mount"
    );
}
