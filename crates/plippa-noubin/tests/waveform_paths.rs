//! WAVE_01: waveform sidecar paths and the analysis candidate query.
//!
//! Fixture: a tiny temp library with a release whose third item is missing on disk (so the
//! index marks it unavailable) plus a playlist that references one of the release's files
//! again (so the candidate query must de-duplicate).

use plippa_noubin::{
    waveform_hash, waveform_path, waveform_relpath, LibraryRoot, LibraryWriter, PLAYER_DB_DIR,
    WAVEFORM_DIR,
};
use std::fs;
use std::path::{Path, PathBuf};

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

/// Release with three items; `03.wav` does not exist.
fn library() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    write(&root, "Music/One/01.wav", b"one");
    write(&root, "Music/One/02.wav", b"two");
    write(
        &root,
        "Music/One/_One.noudata",
        br#"{
  "noudataVersion": "0.4.0",
  "noudataType": "release",
  "playableItemTitle": "One",
  "playableItemProfile": "music",
  "releaseData": {},
  "mediaList": [
    {"mediaTitle": "First", "localFilePaths": ["01.wav"]},
    {"mediaTitle": "Second", "localFilePaths": ["02.wav"]},
    {"mediaTitle": "Gone", "localFilePaths": ["03.wav"]}
  ]
}"#,
    );
    write(
        &root,
        "Playlists/Again.noudata",
        br#"{
  "noudataVersion": "0.4.0",
  "noudataType": "userPlaylist",
  "playableItemTitle": "Again",
  "mediaList": [
    {"localFilePaths": ["../Music/One/01.wav"]}
  ]
}"#,
    );
    let mut writer = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    writer.full_scan().unwrap();
    (tmp, root)
}

#[test]
fn waveform_hash_is_stable_across_spellings_of_the_same_relpath() {
    let want = waveform_hash("Music/Band/Album/01 - One.mp3");
    for spelling in [
        "./Music/Band/Album/01 - One.mp3",
        "/Music/Band/Album/01 - One.mp3",
        "Music//Band/Album/01 - One.mp3",
        "Music/Band/./Album/01 - One.mp3",
        "Music/Band/Other/../Album/01 - One.mp3",
        "Music/One/../Band/Album/01 - One.mp3",
    ] {
        assert_eq!(waveform_hash(spelling), want, "{spelling}");
    }
}

#[test]
fn waveform_hash_differs_for_different_files_and_keeps_case() {
    assert_ne!(waveform_hash("Music/a.mp3"), waveform_hash("Music/b.mp3"));
    // Case is preserved (Linux): different files, different hashes.
    assert_ne!(waveform_hash("Music/A.mp3"), waveform_hash("Music/a.mp3"));
}

#[test]
fn waveform_relpath_is_lowercase_hex_under_the_player_dir() {
    let rel = waveform_relpath("Music/a.mp3");
    let hash = waveform_hash("Music/a.mp3");
    assert_eq!(rel, format!(".plippa/waveforms/{hash:016x}.wfm"));
    assert_eq!(rel, format!("{PLAYER_DB_DIR}/{WAVEFORM_DIR}/{hash:016x}.wfm"));
    assert_eq!(rel.len(), ".plippa/waveforms/".len() + 16 + 4);
    assert!(rel
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .all(|c| c.is_ascii_lowercase()));
}

#[test]
fn waveform_path_joins_the_root() {
    let root = Path::new("/media/stick");
    assert_eq!(
        waveform_path(root, "Music/a.mp3"),
        root.join(waveform_relpath("Music/a.mp3"))
    );
}

#[test]
fn analysis_candidates_lists_each_available_media_file_once() {
    let (_t, root) = library();
    let writer = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    let candidates = writer.analysis_candidates().unwrap();
    let mut rels: Vec<&str> = candidates.iter().map(|c| c.relpath.as_str()).collect();
    rels.sort();
    assert_eq!(rels, vec!["Music/One/01.wav", "Music/One/02.wav"]);
    // The missing third item is `available = 0`, so it is not a candidate.
    for c in &candidates {
        assert!(c.size > 0, "{c:?}");
        assert!(c.mtime_ns > 0, "{c:?}");
    }
}

#[test]
fn analysis_candidates_is_empty_without_a_library() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("empty");
    fs::create_dir_all(&root).unwrap();
    let writer = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    assert!(writer.analysis_candidates().unwrap().is_empty());
}
