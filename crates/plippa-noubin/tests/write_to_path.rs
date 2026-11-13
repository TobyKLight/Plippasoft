//! NB_VERIFY NB_02/NB_03 (G1 open issue): `NoudataDocument::write_to_path` must be the guarded
//! temp-file + atomic-rename writer, not a truncating `fs::write`.

use plippa_noubin::{new_playlist_document, parse_noudata, NoudataDocument};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

fn doc(title: &str) -> NoudataDocument {
    new_playlist_document(title, "tester")
}

fn stray_temps(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("plippa-tmp"))
        .collect()
}

#[test]
fn write_to_path_replaces_via_new_inode_and_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("_a.noudata");
    doc("One").write_to_path(&path).unwrap();
    let ino = fs::metadata(&path).unwrap().ino();
    doc("Two").write_to_path(&path).unwrap();
    assert_ne!(
        fs::metadata(&path).unwrap().ino(),
        ino,
        "must be temp file + rename, not an in-place truncate"
    );
    let back = parse_noudata(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(back.playable_item_title.as_deref(), Some("Two"));
    assert!(stray_temps(dir.path()).is_empty());
}

#[test]
fn write_to_path_keeps_file_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.noudata");
    doc("One").write_to_path(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    doc("Two").write_to_path(&path).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
}

#[test]
fn write_to_path_refuses_a_read_only_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.noudata");
    doc("One").write_to_path(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    assert!(doc("Two").write_to_path(&path).is_err());
    let back = parse_noudata(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(back.playable_item_title.as_deref(), Some("One"));
    assert!(stray_temps(dir.path()).is_empty());
}

#[test]
fn write_to_path_refuses_to_write_through_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim.noudata");
    fs::write(&victim, "untouched").unwrap();
    let link = dir.path().join("x.noudata");
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    assert!(doc("Evil").write_to_path(&link).is_err());
    assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
    assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    // A dangling symlink is refused as well (create would otherwise write the target).
    let dangling = dir.path().join("y.noudata");
    std::os::unix::fs::symlink(outside.path().join("nope.noudata"), &dangling).unwrap();
    assert!(doc("Evil").write_to_path(&dangling).is_err());
    assert!(!outside.path().join("nope.noudata").exists());
    assert!(stray_temps(dir.path()).is_empty());
}

#[test]
fn write_to_path_failure_keeps_the_original_and_leaves_no_temp() {
    let dir = tempfile::tempdir().unwrap();
    // Missing parent folder: nothing can be created.
    let gone = dir.path().join("nope").join("x.noudata");
    assert!(doc("One").write_to_path(&gone).is_err());
    // A directory in the way of the final name: the rename fails after the temp file was
    // fully written; the temp must be removed again.
    let path = dir.path().join("x.noudata");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("keep"), "k").unwrap();
    assert!(doc("One").write_to_path(&path).is_err());
    assert!(path.join("keep").is_file());
    assert!(stray_temps(dir.path()).is_empty());
}

#[test]
fn write_to_path_concurrent_writers_never_tear_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.noudata");
    doc("seed").write_to_path(&path).unwrap();
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        // Reader: every snapshot must parse (a truncating write exposes empty/partial files).
        let reader = s.spawn(|| {
            let mut n = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let bytes = fs::read(&path).unwrap();
                parse_noudata(&bytes).expect("reader saw a torn .noudata");
                n += 1;
            }
            n
        });
        let writers: Vec<_> = (0..6)
            .map(|t| {
                let path = path.clone();
                s.spawn(move || {
                    for i in 0..60 {
                        doc(&format!("writer {t} round {i} {}", "x".repeat(2000)))
                            .write_to_path(&path)
                            .unwrap();
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
    });
    let back = parse_noudata(&fs::read(&path).unwrap()).unwrap();
    assert!(back
        .playable_item_title
        .unwrap()
        .starts_with("writer "));
    assert!(stray_temps(dir.path()).is_empty());
}
