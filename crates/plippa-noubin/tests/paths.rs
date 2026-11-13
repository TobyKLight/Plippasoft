//! Library path helpers (§4.4).

use plippa_noubin::{
    collect_noudata_paths, dir_excluded_from_scan, LibraryRoot, MIN_LIBRARY_SCAN_DEPTH,
};
use std::fs;
use std::path::Path;

#[test]
fn dot_dirs_excluded_by_name() {
    assert!(dir_excluded_from_scan(".plippa"));
    assert!(dir_excluded_from_scan(".cache"));
    assert!(!dir_excluded_from_scan("Music"));
    assert!(!dir_excluded_from_scan("Playlists"));
}

#[test]
fn collect_skips_dot_dirs_and_finds_noudata() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    fs::create_dir_all(root.join("Music/Artist/Album")).unwrap();
    fs::write(
        root.join("Music/Artist/Album/_Album.noudata"),
        r#"{"playableItemTitle":"A","mediaList":[{"localFilePaths":["a.mp3"]}]}"#,
    )
    .unwrap();

    fs::create_dir_all(root.join(".plippa")).unwrap();
    fs::write(
        root.join(".plippa/hidden.noudata"),
        r#"{"playableItemTitle":"Hidden","mediaList":[{"localFilePaths":["h.mp3"]}]}"#,
    )
    .unwrap();

    fs::create_dir_all(root.join("Music/.trash")).unwrap();
    fs::write(
        root.join("Music/.trash/nope.noudata"),
        r#"{"playableItemTitle":"Nope","mediaList":[{"localFilePaths":["n.mp3"]}]}"#,
    )
    .unwrap();

    let lib = LibraryRoot::new(root);
    let found = collect_noudata_paths(&lib, MIN_LIBRARY_SCAN_DEPTH).unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].ends_with(Path::new("Music/Artist/Album/_Album.noudata")));
}

#[test]
fn min_scan_depth_constant() {
    assert!(MIN_LIBRARY_SCAN_DEPTH >= 8);
}
