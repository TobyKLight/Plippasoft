//! PAR-05 (SECFIX_05): a hostile local `.noudata` must not amplify memory.
//!
//! Alone in its own test binary because it measures the process's peak resident set
//! (`VmHWM`): nothing else may run in the process while it does.

use plippa_noubin::{
    read_noudata_capped, LibraryRoot, LibraryWriter, MAX_LOCAL_NOUDATA_BYTES,
};
use std::fs;
use std::io::Write;
use std::path::Path;

fn peak_rss_kib() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("VmHWM")
}

/// `{"x":[0,0,0,...]}` of exactly `bytes` bytes (the review's proof of concept).
fn nested_array_document(path: &Path, bytes: u64) {
    let mut f = std::io::BufWriter::new(fs::File::create(path).unwrap());
    f.write_all(br#"{"x":["#).unwrap();
    let body = bytes - 8; // `{"x":[` is 6 bytes, `]}` 2
    let mut written = 0u64;
    let chunk = b"0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,";
    while written + chunk.len() as u64 <= body - 1 {
        f.write_all(chunk).unwrap();
        written += chunk.len() as u64;
    }
    while written < body - 1 {
        f.write_all(b"0").unwrap();
        written += 1;
    }
    f.write_all(b"0]}").unwrap();
    f.flush().unwrap();
    assert_eq!(fs::metadata(path).unwrap().len(), bytes);
}

#[test]
fn par05_the_16mib_nested_array_document_does_not_amplify_memory() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("stick");
    fs::create_dir_all(&root).unwrap();
    // Warm up allocator and the index machinery so the measured delta is the document's.
    {
        fs::write(root.join("ok.noudata"), r#"{"playableItemTitle":"ok","mediaList":[{"localFilePaths":["a.mp3"]}]}"#).unwrap();
        fs::write(root.join("a.mp3"), b"A").unwrap();
        let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
        w.full_scan().unwrap();
    }
    let before = peak_rss_kib();

    // The review's file: 16 MiB. Over the size cap, refused without being read.
    let big = root.join("big.noudata");
    nested_array_document(&big, 16 * 1024 * 1024);
    assert!(read_noudata_capped(&big).is_err());

    // Just under the cap: it reaches the structural guard, which refuses it before serde
    // builds a value tree (4 million array elements would be hundreds of MB).
    let near_cap = root.join("near_cap.noudata");
    nested_array_document(&near_cap, MAX_LOCAL_NOUDATA_BYTES);
    assert!(read_noudata_capped(&near_cap).is_err(), "guard did not refuse the document");

    // And a whole scan of a stick carrying both stays cheap and indexes neither.
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    let report = w.full_scan().unwrap();
    assert_eq!(report.skipped, 2, "{report:?}");
    let titles: Vec<_> = w.list_playables().unwrap().into_iter().filter(|p| !p.is_orphan).collect();
    assert_eq!(titles.len(), 1);

    let delta_mib = (peak_rss_kib() - before) / 1024;
    assert!(
        delta_mib < 64,
        "peak RSS grew by {delta_mib} MiB handling the hostile documents (limit 64; was 275)"
    );
}
