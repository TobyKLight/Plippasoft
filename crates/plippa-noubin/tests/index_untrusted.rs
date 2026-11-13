//! SECFIX_05: the library index and everything else on the stick is untrusted input.
//!
//! Hostile-stick fixtures are built in temp directories. Each test names the attack from
//! `PlippaSecurityReview_Nov26.md` it replays (PAR-02, SYS-04, PAR-03, SYS-13, PAR-04, PAR-05).

use plippa_noubin::confine::{self, ConfineError};
use plippa_noubin::{
    index_db_path, read_noudata_capped, LibraryReader, LibraryRoot, LibraryWriter, ScanReport,
};
use rusqlite::Connection;
use std::ffi::CString;
use std::fs;
use std::io::{BufRead, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn write(path: &Path, text: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn song(title: &str, media: &str) -> String {
    format!(
        r#"{{"playableItemTitle":"{title}","mediaList":[{{"localFilePaths":["{media}"]}}]}}"#
    )
}

/// A one-song stick: `Music/a.noudata` -> `Music/a.mp3`.
fn stick() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("stick");
    write(&root.join("Music/a.mp3"), b"AAA");
    write(&root.join("Music/a.noudata"), song("Legit Title", "a.mp3"));
    (tmp, root)
}

fn indexed_stick() -> (tempfile::TempDir, PathBuf) {
    let (tmp, root) = stick();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    drop(w);
    (tmp, root)
}

fn mkfifo(path: &Path) {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc_mkfifo(c.as_ptr(), 0o600) }, 0);
}

extern "C" {
    #[link_name = "mkfifo"]
    fn libc_mkfifo(path: *const std::os::raw::c_char, mode: u32) -> i32;
    #[link_name = "mknod"]
    fn libc_mknod(path: *const std::os::raw::c_char, mode: u32, dev: u64) -> i32;
}

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to);
        } else {
            fs::copy(entry.path(), to).unwrap();
        }
    }
}

fn titles(w: &LibraryWriter) -> Vec<String> {
    w.list_playables()
        .unwrap()
        .into_iter()
        .filter(|p| !p.is_orphan)
        .filter_map(|p| p.title)
        .collect()
}

fn scan_with_timeout(root: PathBuf, secs: u64) -> ScanReport {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
        let r = w.full_scan().unwrap();
        let _ = tx.send(r);
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .expect("the scan hung")
}

// ---------------------------------------------------------------------------
// PAR-02: the index file is hostile
// ---------------------------------------------------------------------------

/// A copy of a one-song stick whose index carries a **hot WAL** that rewrites every title to
/// `PLANTED IN WAL` (a second connection with autocheckpoint off; the files are copied while
/// it is open, so the WAL is not folded into the database file).
fn stick_with_planted_wal() -> (tempfile::TempDir, PathBuf) {
    let (tmp, root) = indexed_stick();
    let hot = tmp.path().join("hot");
    {
        let conn = Connection::open(index_db_path(&LibraryRoot::new(&root))).unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute("UPDATE playables SET title = 'PLANTED IN WAL'", [])
            .unwrap();
        assert!(root.join(".plippa/library.sqlite-wal").metadata().unwrap().len() > 0);
        copy_tree(&root, &hot);
    }
    assert!(hot.join(".plippa/library.sqlite-wal").exists(), "the fixture has a hot WAL");
    (tmp, hot)
}

#[test]
fn a_planted_hot_wal_is_not_replayed_into_trusted_rows() {
    let (_tmp, hot) = stick_with_planted_wal();
    let w = LibraryWriter::open(&LibraryRoot::new(&hot)).unwrap();
    let all = titles(&w);
    assert!(
        !all.iter().any(|t| t.contains("PLANTED")),
        "a WAL planted on the stick was replayed into the index: {all:?}"
    );
}

#[test]
fn a_reader_that_opens_first_does_not_read_through_a_planted_wal() {
    let (_tmp, hot) = stick_with_planted_wal();
    let r = LibraryReader::open(&LibraryRoot::new(&hot)).unwrap();
    let all: Vec<_> = r.list_playables().unwrap().into_iter().filter_map(|p| p.title).collect();
    assert!(!all.iter().any(|t| t.contains("PLANTED")), "{all:?}");
}

/// A *valid* hot rollback journal that, when rolled back onto `clean_db`, turns it into
/// `poisoned_db` (the journal's "original pages" are the poisoned file's pages).
fn hot_journal_turning(clean_db: &[u8], poisoned_db: &[u8]) -> Vec<u8> {
    let page_size = u16::from_be_bytes([clean_db[16], clean_db[17]]) as usize;
    let page_size = if page_size == 1 { 65536 } else { page_size };
    let pages = clean_db.len() / page_size;
    let nonce: u32 = 0x5EC0_F1C5;
    let sector = 512usize;
    let mut records: Vec<u8> = Vec::new();
    let mut n_rec = 0u32;
    for pg in 0..pages.min(poisoned_db.len() / page_size) {
        let new = &poisoned_db[pg * page_size..(pg + 1) * page_size];
        if new == &clean_db[pg * page_size..(pg + 1) * page_size] {
            continue;
        }
        records.extend_from_slice(&(pg as u32 + 1).to_be_bytes());
        records.extend_from_slice(new);
        let mut sum = nonce;
        let mut i = page_size as isize - 200;
        while i > 0 {
            sum = sum.wrapping_add(new[i as usize] as u32);
            i -= 200;
        }
        records.extend_from_slice(&sum.to_be_bytes());
        n_rec += 1;
    }
    let mut j = vec![0u8; sector];
    j[..8].copy_from_slice(&[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]);
    j[8..12].copy_from_slice(&n_rec.to_be_bytes());
    j[12..16].copy_from_slice(&nonce.to_be_bytes());
    j[16..20].copy_from_slice(&(pages as u32).to_be_bytes());
    j[20..24].copy_from_slice(&(sector as u32).to_be_bytes());
    j[24..28].copy_from_slice(&(page_size as u32).to_be_bytes());
    j.extend(records);
    j
}

#[test]
fn a_planted_rollback_journal_is_not_replayed() {
    let (_tmp, root) = indexed_stick();
    let db = index_db_path(&LibraryRoot::new(&root));
    let clean = fs::read(&db).unwrap();
    // The poisoned twin of the index (checkpointed into its main file).
    let twin = _tmp.path().join("twin");
    copy_tree(&root, &twin);
    tamper(&twin, "UPDATE playables SET title = 'PLANTED IN JOURNAL'");
    let poisoned = fs::read(index_db_path(&LibraryRoot::new(&twin))).unwrap();
    assert_ne!(clean, poisoned);
    // The stick is "inserted" fresh: the index of `root` was already opened in this process.
    let fresh = _tmp.path().join("fresh");
    copy_tree(&root, &fresh);
    write(&fresh.join(".plippa/library.sqlite-journal"), hot_journal_turning(&clean, &poisoned));
    let w = LibraryWriter::open(&LibraryRoot::new(&fresh)).unwrap();
    let all = titles(&w);
    assert!(!all.iter().any(|t| t.contains("PLANTED")), "a planted rollback journal was replayed: {all:?}");
}

fn tamper(root: &Path, sql: &str) {
    let conn = Connection::open(index_db_path(&LibraryRoot::new(root))).unwrap();
    conn.execute_batch(sql).unwrap();
}

fn object_exists(root: &Path, name: &str) -> bool {
    let conn = Connection::open(index_db_path(&LibraryRoot::new(root))).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

#[test]
fn a_database_with_a_foreign_schema_is_rebuilt_not_read() {
    let cases: [(&str, &str, &str); 5] = [
        ("view", "CREATE VIEW evil_view AS SELECT * FROM playables;", "evil_view"),
        (
            "trigger",
            "CREATE TRIGGER evil_trigger AFTER INSERT ON dirs BEGIN SELECT 1; END;",
            "evil_trigger",
        ),
        ("extra column", "ALTER TABLE playables ADD COLUMN evil TEXT;", "evil_view"),
        (
            "changed column",
            "DROP TABLE dirs; CREATE TABLE dirs (relpath TEXT PRIMARY KEY NOT NULL, parent TEXT, mtime_ns TEXT);
             CREATE INDEX idx_dirs_parent ON dirs(parent);",
            "evil_view",
        ),
        ("wrong meta", "INSERT INTO meta VALUES ('evil', '1');", "evil_view"),
    ];
    for (what, sql, object) in cases {
        let (_tmp, root) = indexed_stick();
        tamper(&root, sql);
        // A reader never rebuilds, and never reads it.
        let err = LibraryReader::open(&LibraryRoot::new(&root))
            .err()
            .unwrap_or_else(|| panic!("{what}: a foreign schema was opened by a reader"));
        assert!(err.is_unusable_index(), "{what}: {err}");

        let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
        assert!(w.list_playables().unwrap().is_empty(), "{what}: rows were read from the foreign file");
        if what != "extra column" && what != "changed column" && what != "wrong meta" {
            assert!(!object_exists(&root, object), "{what}: the planted object survived");
        }
        let conn = Connection::open(index_db_path(&LibraryRoot::new(&root))).unwrap();
        let meta_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(meta_rows, 1, "{what}: meta was not rebuilt");
        drop(conn);
        w.full_scan().unwrap();
        assert_eq!(titles(&w), ["Legit Title"], "{what}");
    }
}

#[test]
fn a_corrupt_database_is_rebuilt() {
    let (_tmp, root) = indexed_stick();
    let db = index_db_path(&LibraryRoot::new(&root));
    let mut bytes = fs::read(&db).unwrap();
    assert!(bytes.len() > 16 * 1024, "fixture index spans several pages");
    for b in &mut bytes[8192..16384] {
        *b = 0xFF;
    }
    fs::write(&db, bytes).unwrap();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    assert_eq!(titles(&w), ["Legit Title"]);
}

#[test]
fn an_index_that_is_a_symlink_or_a_fifo_is_replaced_not_followed() {
    let (_tmp, root) = indexed_stick();
    let db = index_db_path(&LibraryRoot::new(&root));
    let outside = _tmp.path().join("outside.sqlite");
    fs::copy(&db, &outside).unwrap();
    fs::remove_file(&db).unwrap();
    symlink(&outside, &db).unwrap();
    let (tx, rx) = mpsc::channel();
    let r2 = root.clone();
    std::thread::spawn(move || {
        let _ = tx.send(LibraryWriter::open(&LibraryRoot::new(&r2)).map(|_| ()).map_err(|e| e.to_string()));
    });
    rx.recv_timeout(Duration::from_secs(10)).expect("opening hung").unwrap();
    assert!(!fs::symlink_metadata(&db).unwrap().file_type().is_symlink());

    fs::remove_file(&db).unwrap();
    mkfifo(&db);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(LibraryWriter::open(&LibraryRoot::new(&root)).map(|_| ()).map_err(|e| e.to_string()));
    });
    rx.recv_timeout(Duration::from_secs(10)).expect("opening a fifo index hung").unwrap();
}

#[test]
fn the_review_poc_poisoned_row_is_not_believed() {
    let (_tmp, root) = indexed_stick();
    // The review's proof of concept: bidi/control title and an absolute media path, with the
    // fingerprint (mtime, size) left matching the real file so a scan would trust the row.
    tamper(
        &root,
        "UPDATE playables SET title = char(8238) || 'Ev' || char(7) || 'il',
                media_relpaths_json = '[\"/etc/hostname\"]';
         UPDATE playable_items SET media_relpath = '/etc/hostname',
                media_relpaths_json = '[\"/etc/hostname\"]',
                title = char(8238) || 'Ev' || char(7) || 'il';",
    );
    let lib = LibraryRoot::new(&root);

    // Read layer (engine / UI readers): sanitised and dropped, `available` false.
    let r = LibraryReader::open(&lib).unwrap();
    let rec = r.list_playables().unwrap().remove(0);
    assert_eq!(rec.title.as_deref(), Some("Evil"));
    assert!(rec.media_relpaths.is_empty(), "{:?}", rec.media_relpaths);
    let item = r.items_of(rec.id).unwrap().remove(0);
    assert_eq!(item.title, "Evil");
    assert_eq!(item.media_relpath, "");
    assert!(item.media_relpaths.is_empty());
    assert!(!item.available);
    drop(r);

    // Writer: the row is no longer trusted, the next scan re-reads the document.
    let mut w = LibraryWriter::open(&lib).unwrap();
    w.lazy_rescan().unwrap();
    let rec = w.list_playables().unwrap().remove(0);
    assert_eq!(rec.title.as_deref(), Some("Legit Title"));
    assert_eq!(rec.media_relpaths, ["Music/a.mp3"]);
    let item = w.items_of(rec.id).unwrap().remove(0);
    assert_eq!(item.media_relpath, "Music/a.mp3");
    assert!(item.available);
}

// ---------------------------------------------------------------------------
// PAR-03 / SYS-13: one confinement function
// ---------------------------------------------------------------------------

#[test]
fn hostile_media_paths_are_confined() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("stick");
    write(&root.join("Music/a.mp3"), b"AAA");
    write(&root.join("Playlists/keep.txt"), b"");
    write(&tmp.path().join("other-stick/x.mp3"), b"OTHER");
    write(&tmp.path().join("secret.mp3"), b"SECRET");
    symlink("/etc/passwd", root.join("Music/passwd.mp3")).unwrap();
    symlink(tmp.path().join("secret.mp3"), root.join("Music/escape.mp3")).unwrap();
    symlink(root.join("Music/a.mp3"), root.join("Music/inside.mp3")).unwrap();
    symlink("/etc", root.join("etcdir")).unwrap();
    symlink(tmp.path().join("other-stick"), root.join("Music/otherdir")).unwrap();
    mkfifo(&root.join("Music/pipe.mp3"));
    fs::create_dir_all(root.join("Music/dir.mp3")).unwrap();
    // A character device where one can be made (needs privilege; /dev/zero is 1:5).
    let dev = root.join("Music/zero.mp3");
    let made_dev = unsafe {
        libc_mknod(
            CString::new(dev.as_os_str().as_bytes()).unwrap().as_ptr(),
            0o020000 | 0o600,
            (1u64 << 8) | 5,
        ) == 0
    };

    let must_not_resolve = [
        "/etc/passwd",
        "/dev/zero",
        "../secret.mp3",
        "../../other-stick/x.mp3",
        "../other-stick/x.mp3",
        "Music/../../secret.mp3",
        "Music/passwd.mp3",
        "Music/escape.mp3",
        "Music/inside.mp3",
        "etcdir/passwd",
        "Music/otherdir/x.mp3",
        "Music/pipe.mp3",
        "Music/dir.mp3",
        "Music/a\\b.mp3",
        "Music/a\0b.mp3",
        "C:/x.mp3",
        "",
        ".",
        "..",
    ];
    for p in must_not_resolve {
        assert!(
            confine::open_media(&root, "", p).is_err(),
            "{p:?} resolved to a file"
        );
    }
    if made_dev {
        assert!(confine::open_media(&root, "", "Music/zero.mp3").is_err());
    }
    // The resolver alone (no filesystem) agrees on the lexical cases.
    for p in ["/etc/passwd", "../../secret.mp3", "a/../../../x", "a\\b", "a\0b", "C:/x"] {
        assert_eq!(confine::resolve_declared("Music", p), None, "{p:?}");
    }
    assert_eq!(confine::resolve_declared("Music", "../Other/x.mp3").as_deref(), Some("Other/x.mp3"));

    // Normal files and the allowed playlist form resolve.
    let mut ok = confine::open_media(&root, "", "Music/a.mp3").unwrap();
    let mut text = String::new();
    ok.file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "AAA");
    assert_eq!(ok.rel, "Music/a.mp3");
    let pl = confine::open_media(&root, "Playlists", "../Music/a.mp3").unwrap();
    assert_eq!(pl.rel, "Music/a.mp3");
    assert!(matches!(
        confine::open_media(&root, "", "Music/missing.mp3"),
        Err(ConfineError::Missing)
    ));
}

#[test]
fn confinement_returns_the_checked_handle() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("stick");
    write(&root.join("a.mp3"), b"CHECKED");
    write(&tmp.path().join("evil.mp3"), b"SWAPPED");
    let mut got = confine::open_media(&root, "", "a.mp3").unwrap();
    // Between the check and the use the file is replaced by a symlink to something else.
    fs::remove_file(root.join("a.mp3")).unwrap();
    symlink(tmp.path().join("evil.mp3"), root.join("a.mp3")).unwrap();
    let mut text = String::new();
    got.file.read_to_string(&mut text).unwrap();
    assert_eq!(text, "CHECKED", "the decoder must read the file that was checked");
    assert!(confine::open_media(&root, "", "a.mp3").is_err(), "the swapped path is refused");
}

// ---------------------------------------------------------------------------
// PAR-04: quadratic dedup, caps
// ---------------------------------------------------------------------------

fn playlist_of(n: usize, distinct: bool) -> String {
    let mut s = String::from(r#"{"playableItemTitle":"Big","mediaList":["#);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        if distinct {
            s.push_str(&format!(r#"{{"localFilePaths":["t{i}.mp3"]}}"#));
        } else {
            s.push_str(r#"{"localFilePaths":["a.mp3"]}"#);
        }
    }
    s.push_str("]}");
    s
}

fn scan_doc(doc: &str) -> (Duration, ScanReport, Vec<String>) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("stick");
    write(&root.join("a.mp3"), b"A");
    write(&root.join("big.noudata"), doc);
    let started = Instant::now();
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    let report = w.full_scan().unwrap();
    let took = started.elapsed();
    (took, report, titles(&w))
}

#[test]
fn a_hostile_noudata_with_80000_items_scans_in_bounded_time_and_is_marked_not_playable() {
    let doc = playlist_of(80_000, true);
    assert!(doc.len() < 8 * 1024 * 1024, "fixture stays under the size cap");
    let (took, report, titles) = scan_doc(&doc);
    assert!(took < Duration::from_secs(5), "80,000 items took {took:?}");
    assert!(titles.is_empty(), "the hostile document was indexed: {titles:?}");
    assert_eq!(report.skipped, 1);
    assert!(report.warnings.iter().any(|w| w.contains("exceeds")), "{:?}", report.warnings);
}

#[test]
fn a_noudata_over_the_item_cap_is_not_playable() {
    let (_, report, titles) = scan_doc(&playlist_of(20_001, false));
    assert!(titles.is_empty());
    assert_eq!(report.skipped, 1);
    assert!(report.warnings.iter().any(|w| w.contains("exceeds")), "{:?}", report.warnings);
    // ... and exactly at the cap it is indexed.
    let (_, report, titles) = scan_doc(&playlist_of(20_000, false));
    assert_eq!(titles, ["Big"]);
    assert_eq!(report.skipped, 0);
}

#[test]
fn scan_time_of_distinct_items_scales_near_linearly() {
    let (small, ..) = scan_doc(&playlist_of(2_500, true));
    let (big, report, titles) = scan_doc(&playlist_of(10_000, true));
    assert_eq!(titles, ["Big"]);
    assert_eq!(report.skipped, 0);
    assert!(big < Duration::from_secs(20), "10,000 distinct items took {big:?}");
    // 4x the items: linear is ~4x; the old quadratic dedup was ~16x and more.
    assert!(
        big.as_secs_f64() < small.as_secs_f64() * 9.0 + 0.5,
        "2,500 items {small:?}, 10,000 items {big:?}: not near-linear"
    );
}

// ---------------------------------------------------------------------------
// PAR-05 (the memory test is in par05_memory.rs, alone in its process)
// ---------------------------------------------------------------------------

#[test]
fn a_fifo_named_noudata_does_not_block_the_scan() {
    let (_tmp, root) = stick();
    mkfifo(&root.join("Music/pipe.noudata"));
    // The directory walk skips it; the direct read paths must not block either.
    let report = scan_with_timeout(root.clone(), 15);
    assert_eq!(report.scanned_files, 1);
    let (tx, rx) = mpsc::channel();
    let fifo = root.join("Music/pipe.noudata");
    std::thread::spawn(move || {
        let _ = tx.send(read_noudata_capped(&fifo).is_err());
    });
    assert!(rx.recv_timeout(Duration::from_secs(10)).expect("read blocked on a FIFO"));
    // Ingesting that exact path (the service does this after writing a playlist) neither
    // blocks nor indexes it.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
        let r = w.ingest_noudata("Music/pipe.noudata").unwrap();
        let _ = tx.send((r.skipped, titles(&w)));
    });
    let (skipped, titles) = rx.recv_timeout(Duration::from_secs(15)).expect("ingest blocked on a FIFO");
    assert_eq!(skipped, 1);
    assert_eq!(titles, ["Legit Title"]);
}

#[test]
fn a_symlinked_noudata_is_not_followed() {
    let (tmp, root) = stick();
    write(&tmp.path().join("elsewhere.noudata"), song("From Outside", "a.mp3"));
    symlink(tmp.path().join("elsewhere.noudata"), root.join("Music/link.noudata")).unwrap();
    symlink(root.join("Music/a.noudata"), root.join("Music/inlink.noudata")).unwrap();
    assert!(read_noudata_capped(&root.join("Music/link.noudata")).is_err());
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    w.full_scan().unwrap();
    w.ingest_noudata("Music/link.noudata").unwrap();
    w.ingest_noudata("Music/inlink.noudata").unwrap();
    assert_eq!(titles(&w), ["Legit Title"]);
}

// ---------------------------------------------------------------------------
// Cross-process behaviour (re-exec of this test binary)
// ---------------------------------------------------------------------------

const CHILD_ENV: &str = "PLIPPA_TEST_CHILD_ROLE";
const CHILD_ROOT_ENV: &str = "PLIPPA_TEST_CHILD_ROOT";

fn child_command(role: &str, root: &Path, test: &str) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, role)
        .env(CHILD_ROOT_ENV, root);
    cmd
}

/// Role "hold": open the index as a writer, add an uncheckpointed WAL frame, say READY, and
/// hold the connection until stdin closes.
#[test]
fn child_holds_the_index_open() {
    if std::env::var(CHILD_ENV).as_deref() != Ok("hold") {
        return;
    }
    let root = PathBuf::from(std::env::var(CHILD_ROOT_ENV).unwrap());
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    // The scan's frames sit in the live WAL (nothing has checkpointed them yet).
    w.full_scan().unwrap();
    println!("READY");
    std::io::stdout().flush().unwrap();
    let mut sink = String::new();
    let _ = std::io::stdin().read_line(&mut sink);
    drop(w);
}

#[test]
fn another_process_that_has_the_index_open_keeps_its_wal() {
    let (_tmp, root) = stick();
    let mut child = child_command("hold", &root, "child_holds_the_index_open")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "child exited early");
        if line.contains("READY") {
            break;
        }
    }
    let wal = root.join(".plippa/library.sqlite-wal");
    let before = fs::metadata(&wal).map(|m| (m.ino(), m.len())).unwrap();
    // This process opens the live index of the child: it must leave the WAL alone.
    let r = LibraryReader::open(&LibraryRoot::new(&root)).unwrap();
    assert_eq!(r.list_playables().unwrap().len(), 1, "the live index is read intact");
    let after = fs::metadata(&wal).map(|m| (m.ino(), m.len())).unwrap();
    assert_eq!(before.0, after.0, "the live WAL was replaced");
    drop(r);
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

/// Role "readonly": run as an unprivileged user against a tree it cannot write.
#[test]
fn child_opens_a_bad_index_on_a_read_only_volume() {
    if std::env::var(CHILD_ENV).as_deref() != Ok("readonly") {
        return;
    }
    let root = PathBuf::from(std::env::var(CHILD_ROOT_ENV).unwrap());
    let lib = LibraryRoot::new(&root);
    // Neither call may panic or hang; the volume cannot be written, so the writer cannot
    // rebuild and says so.
    // (A WAL-mode database cannot even be opened read-only without write access to its
    // directory, so SQLite may refuse before our validation runs; either way: an error.)
    let reader = LibraryReader::open(&lib);
    assert!(reader.is_err(), "a reader opened a foreign index on a read-only volume");
    match LibraryWriter::open(&lib) {
        Ok(_) => panic!("a writer opened a foreign index on a read-only volume"),
        Err(e) => println!("writer refused: {e}"),
    }
    // The foreign index is untouched (nothing was written).
    assert!(Path::new(&root).join(".plippa/library.sqlite").exists());
}

#[test]
fn a_read_only_volume_with_a_bad_index_does_not_crash() {
    // root ignores chmod, so run the child as an unprivileged user when we are root, and make
    // the tree read-only by ownership + mode; when we are not root, chmod itself is enough.
    let tmp = tempfile::tempdir().unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let root = tmp.path().join("stick");
    write(&root.join("Music/a.mp3"), b"AAA");
    write(&root.join("Music/a.noudata"), song("Legit Title", "a.mp3"));
    {
        let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
        w.full_scan().unwrap();
    }
    tamper(&root, "CREATE VIEW evil_view AS SELECT * FROM playables;");
    fn lock_down(dir: &Path) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                lock_down(&entry.path());
            } else {
                fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o444)).unwrap();
            }
        }
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    }
    lock_down(&root);
    let is_root = fs::metadata("/proc/self").unwrap().uid() == 0;
    let mut cmd = child_command("readonly", &root, "child_opens_a_bad_index_on_a_read_only_volume");
    if is_root {
        use std::os::unix::process::CommandExt;
        cmd.uid(65534).gid(65534);
    }
    let out = cmd.output().unwrap();
    // Restore write access so the temp dir can be removed.
    fn unlock(dir: &Path) {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        for entry in fs::read_dir(dir).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                unlock(&entry.path());
            }
        }
    }
    unlock(&root);
    assert!(
        out.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 passed"),
        "the child test did not run:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );
}


// ---------------------------------------------------------------------------
// Tracking tables (dirs, media_files, noudata_skips) are untrusted too
// ---------------------------------------------------------------------------

#[test]
fn injected_tracking_rows_never_make_the_scan_leave_the_root() {
    use std::os::unix::fs::MetadataExt;
    for (what, hostile_dir) in [("absolute", "ABS"), ("dotdot", "../other/Secret"), ("slash", "/")] {
        let (tmp, root) = indexed_stick();
        // A document outside the library that a scan must neither read nor rewrite: its
        // `noubinKey` has no normalised twin, so a scan that read it would write it back.
        let secret = tmp.path().join("other/Secret/s.noudata");
        let body = r#"{"noubinKey":"https://x.example/k","playableItemTitle":"Secret","mediaList":[{"localFilePaths":["s.mp3"]}]}"#;
        write(&secret, body);
        write(&tmp.path().join("other/Secret/s.mp3"), b"S");
        let before = (fs::read(&secret).unwrap(), fs::metadata(&secret).unwrap().mtime_nsec());
        let abs_dir = tmp.path().join("other/Secret").display().to_string();
        let dir = if hostile_dir == "ABS" { abs_dir.clone() } else { hostile_dir.to_string() };

        // The root counts as cached (its real mtime), and hostile rows are injected.
        let root_mtime = fs::metadata(&root).unwrap();
        let ns = root_mtime.mtime() * 1_000_000_000 + root_mtime.mtime_nsec();
        tamper(
            &root,
            &format!(
                "UPDATE dirs SET mtime_ns = {ns} WHERE relpath = '';
                 INSERT OR REPLACE INTO dirs (relpath, parent, mtime_ns) VALUES ('{d}', '', -1);
                 INSERT OR REPLACE INTO media_files (relpath, dir, size, mtime_ns) VALUES ('{d}/s.mp3', '{d}', 1, 1);
                 INSERT OR REPLACE INTO noudata_skips (relpath, mtime_ns, size) VALUES ('{d}/s.noudata', 1, 1);",
                d = dir.replace('\'', "''")
            ),
        );
        let (tx, rx) = mpsc::channel();
        let r2 = root.clone();
        std::thread::spawn(move || {
            let mut w = LibraryWriter::open(&LibraryRoot::new(&r2)).unwrap();
            let report = w.lazy_rescan().unwrap();
            let _ = tx.send((report, titles(&w)));
        });
        let (report, titles) = rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| panic!("{what}: the scan walked outside the library"));
        assert_eq!(
            (fs::read(&secret).unwrap(), fs::metadata(&secret).unwrap().mtime_nsec()),
            before,
            "{what}: a document outside the library was rewritten"
        );
        assert!(!titles.iter().any(|t| t == "Secret"), "{what}: {titles:?}");
        assert!(
            report.warnings.iter().all(|w| !w.contains(&abs_dir) && !w.contains("other/Secret")),
            "{what}: {:?}",
            report.warnings
        );
        // The foreign rows are gone from the index.
        let conn = Connection::open(index_db_path(&LibraryRoot::new(&root))).unwrap();
        let foreign: i64 = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM dirs WHERE relpath != '' AND (substr(relpath,1,1) = '/' OR relpath LIKE '%..%'))
                      + (SELECT COUNT(*) FROM media_files WHERE substr(relpath,1,1) = '/' OR relpath LIKE '%..%')
                      + (SELECT COUNT(*) FROM noudata_skips WHERE substr(relpath,1,1) = '/' OR relpath LIKE '%..%')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(foreign, 0, "{what}: foreign tracking rows survived the scan");
    }
}

// ---------------------------------------------------------------------------
// First open of a volume: races and unusable side files
// ---------------------------------------------------------------------------

#[test]
fn threads_racing_on_the_first_open_all_get_a_clean_index() {
    use std::sync::{Arc, Barrier};
    let (_tmp, hot) = stick_with_planted_wal();
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let (root, barrier) = (hot.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                if i % 2 == 0 {
                    let w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
                    titles(&w)
                } else {
                    let r = LibraryReader::open(&LibraryRoot::new(&root)).unwrap();
                    r.list_playables().unwrap().into_iter().filter_map(|p| p.title).collect()
                }
            })
        })
        .collect();
    for h in handles {
        let t = h.join().expect("a racing open panicked");
        assert!(!t.iter().any(|t| t.contains("PLANTED")), "{t:?}");
        assert_eq!(t, ["Legit Title"]);
    }
}

#[test]
fn a_directory_named_like_the_shm_file_does_not_lock_the_stick_out() {
    let (tmp, root) = indexed_stick();
    let fresh = tmp.path().join("fresh");
    copy_tree(&root, &fresh);
    fs::create_dir(fresh.join(".plippa/library.sqlite-shm")).unwrap();
    fs::write(fresh.join(".plippa/library.sqlite-shm/x"), b"x").unwrap();
    let w = LibraryWriter::open(&LibraryRoot::new(&fresh)).expect("a hostile -shm must not be fatal");
    assert_eq!(titles(&w), ["Legit Title"]);
    drop(w);
    // And a symlink in its place.
    let fresh2 = tmp.path().join("fresh2");
    copy_tree(&root, &fresh2);
    symlink("/etc/passwd", fresh2.join(".plippa/library.sqlite-shm")).unwrap();
    LibraryWriter::open(&LibraryRoot::new(&fresh2)).expect("a symlinked -shm must not be fatal");
}

// ---------------------------------------------------------------------------
// Real processes opening a freshly inserted stick at the same time
// ---------------------------------------------------------------------------

/// Role "cold": wait for "go" on stdin, open the index (writer or reader), list, print the outcome.
#[test]
fn child_cold_opens_the_index() {
    let role = std::env::var(CHILD_ENV).unwrap_or_default();
    if role != "cold-writer" && role != "cold-reader" {
        return;
    }
    let root = PathBuf::from(std::env::var(CHILD_ROOT_ENV).unwrap());
    let mut go = String::new();
    let _ = std::io::stdin().read_line(&mut go);
    let lib = LibraryRoot::new(&root);
    let outcome = if role == "cold-writer" {
        LibraryWriter::open(&lib)
            .and_then(|w| w.list_playables().map(|l| l.len()))
            .map_err(|e| e.to_string())
    } else {
        LibraryReader::open(&lib)
            .and_then(|r| r.list_playables().map(|l| l.len()))
            .map_err(|e| e.to_string())
    };
    match outcome {
        Ok(n) => println!("COLD-OK {n}"),
        Err(e) => println!("COLD-FAIL {e}"),
    }
}

/// Spawn `n` cold-opening children on `root`, release them together, return their outcomes.
fn cold_open_race(root: &Path, n: usize) -> Vec<String> {
    let mut kids: Vec<_> = (0..n)
        .map(|i| {
            let role = if i % 4 == 3 { "cold-reader" } else { "cold-writer" };
            child_command(role, root, "child_cold_opens_the_index")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    std::thread::sleep(Duration::from_millis(150)); // let every child reach its read_line
    for k in &mut kids {
        let _ = writeln!(k.stdin.as_mut().unwrap(), "go");
    }
    kids.into_iter()
        .map(|k| {
            let out = k.wait_with_output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            text.lines()
                .find_map(|l| l.split_once("COLD-").map(|(_, r)| format!("COLD-{r}")))
                .unwrap_or_else(|| format!("COLD-FAIL no result: {text}"))
        })
        .collect()
}

#[test]
fn four_processes_cold_opening_a_stick_with_foreign_side_files_never_fail() {
    let rounds: usize = std::env::var("PLIPPA_COLD_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
    let mut failures = Vec::new();
    for round in 0..rounds {
        // Every third round the stick has no index at all (four processes creating it at once).
        let (tmp, hot) = if round % 3 == 2 { stick() } else { stick_with_planted_wal() };
        // Alternate: plain cold race, or one already-running process that must keep its WAL.
        let mut holder = None;
        let mut live_wal = None;
        if round % 2 == 1 && round % 3 != 2 {
            // The first opener (this child) vets and then stays running.
            let mut child = child_command("hold", &hot, "child_holds_the_index_open")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                assert!(out.read_line(&mut line).unwrap() > 0, "holder exited early");
                if line.contains("READY") {
                    break;
                }
            }
            live_wal = fs::metadata(hot.join(".plippa/library.sqlite-wal")).ok().map(|m| m.ino());
            assert!(live_wal.is_some(), "the running process has a live WAL");
            holder = Some((child, out));
        }
        for r in cold_open_race(&hot, 4) {
            // A reader cannot create the index: "not found" is its right answer on a bare stick.
            let bare_reader = round % 3 == 2 && r.contains("library index not found");
            if !r.starts_with("COLD-OK") && !bare_reader {
                failures.push(format!("round {round}: {r}"));
            }
        }
        if let Some(ino) = live_wal {
            let now = fs::metadata(hot.join(".plippa/library.sqlite-wal")).map(|m| m.ino()).ok();
            if now != Some(ino) {
                failures.push(format!("round {round}: the live WAL of a running process was replaced"));
            }
        }
        if let Some((mut child, _out)) = holder {
            drop(child.stdin.take());
            let _ = child.wait();
        }
        drop(tmp);
    }
    println!("COLD-OPEN failures: {} of {} four-process rounds", failures.len(), rounds);
    assert!(failures.is_empty(), "{} failures, first: {:?}", failures.len(), &failures[..failures.len().min(5)]);
}

// ---------------------------------------------------------------------------
// Read-only volume with leftover side files
// ---------------------------------------------------------------------------

/// Role "readonly-stale": an unprivileged reader on a tree it cannot write, which still has a
/// leftover `-wal` and `-shm` from an unclean unplug.
#[test]
fn child_reads_an_index_with_leftover_side_files_on_a_read_only_volume() {
    if std::env::var(CHILD_ENV).as_deref() != Ok("readonly-stale") {
        return;
    }
    let root = PathBuf::from(std::env::var(CHILD_ROOT_ENV).unwrap());
    let reader = LibraryReader::open(&LibraryRoot::new(&root))
        .unwrap_or_else(|e| panic!("a read-only stick with leftover side files is unusable: {e}"));
    let n = reader.list_playables().unwrap().len();
    println!("READ-ONLY-OK {n}");
    // Nothing was deleted: the volume is not ours to change.
    assert!(root.join(".plippa/library.sqlite-wal").exists());
    assert!(root.join(".plippa/library.sqlite-shm").exists());
}

#[test]
fn a_read_only_stick_with_a_leftover_shm_and_wal_is_still_usable() {
    let (tmp, hot) = stick_with_planted_wal();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(hot.join(".plippa/library.sqlite-shm").exists());
    fn lock_down(dir: &Path) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                lock_down(&entry.path());
            } else {
                fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o444)).unwrap();
            }
        }
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
    }
    fn unlock(dir: &Path) {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        for entry in fs::read_dir(dir).unwrap().flatten() {
            if entry.file_type().unwrap().is_dir() {
                unlock(&entry.path());
            }
        }
    }
    lock_down(&hot);
    let is_root = fs::metadata("/proc/self").unwrap().uid() == 0;
    let mut cmd = child_command(
        "readonly-stale",
        &hot,
        "child_reads_an_index_with_leftover_side_files_on_a_read_only_volume",
    );
    if is_root {
        use std::os::unix::process::CommandExt;
        cmd.uid(65534).gid(65534);
    }
    let out = cmd.output().unwrap();
    unlock(&hot);
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success() && text.contains("READ-ONLY-OK"),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Wrongly typed cells, descriptors, hostile names
// ---------------------------------------------------------------------------

#[test]
fn every_type_confusion_in_every_column_is_rebuilt_not_fatal() {
    // One stick with every table populated (a playlist over a release, a cue list, covers,
    // an orphan, a skipped document).
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("stick");
    write(&root.join("Music/a.mp3"), b"A");
    write(&root.join("Music/cover.jpg"), b"J");
    write(&root.join("Music/stray.mp3"), b"S");
    write(
        &root.join("Music/a.noudata"),
        r#"{"playableItemTitle":"Legit Title","noudataType":"release","releaseData":{},
            "localPlaybackData":{"localCoverImage":[{"imageLocalFilename":"cover.jpg"}]},
            "mediaList":[{"localFilePaths":["a.mp3"],"cueLists":[{"cues":[{"startTime":"00:00:01.000","cueText":"x"}]}]}]}"#,
    );
    write(&root.join("Playlists/p.noudata"), r#"{"noudataType":"userPlaylist","playableItemTitle":"PL","mediaList":[{"localFilePaths":["../Music/a.mp3"]}]}"#);
    write(&root.join("Music/skip.noudata"), "{}");
    {
        let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
        w.full_scan().unwrap();
    }
    let template = tmp.path().join("template");
    copy_tree(&root, &template);
    let (tables, values): (Vec<(String, Vec<String>)>, [&str; 5]) = {
        let conn = Connection::open(index_db_path(&LibraryRoot::new(&template))).unwrap();
        let names: Vec<String> = {
            let mut st = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
                .unwrap();
            st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        let mut out = Vec::new();
        for n in names {
            let mut st = conn.prepare(&format!("PRAGMA table_info(\"{n}\")")).unwrap();
            let cols: Vec<String> = st.query_map([], |r| r.get(1)).unwrap().map(|r| r.unwrap()).collect();
            out.push((n, cols));
        }
        (out, ["x'00ff'", "'abc'", "1.5", "9223372036854775807", "-9223372036854775808"])
    };
    assert!(tables.len() >= 8);
    let mut cases = 0;
    for (table, cols) in &tables {
        for col in cols {
            for (vi, value) in values.iter().enumerate() {
                let stick = tmp.path().join(format!("case-{cases}"));
                cases += 1;
                copy_tree(&template, &stick);
                let conn = Connection::open(index_db_path(&LibraryRoot::new(&stick))).unwrap();
                // One row (rowid-wise) so UNIQUE / PRIMARY KEY columns stay satisfiable.
                let sql = format!(
                    "UPDATE \"{table}\" SET \"{col}\" = {value} WHERE rowid = (SELECT min(rowid) FROM \"{table}\")"
                );
                if conn.execute(&sql, []).is_err() {
                    continue; // e.g. a constraint refuses this value: not a possible file
                }
                drop(conn);
                let what = format!("{table}.{col} = {} (value #{vi})", value);
                // Everything keeps working, in the same process, without deleting `.plippa`.
                let mut w = LibraryWriter::open(&LibraryRoot::new(&stick))
                    .unwrap_or_else(|e| panic!("{what}: open failed: {e}"));
                w.list_playables().unwrap_or_else(|e| panic!("{what}: list failed: {e}"));
                w.full_scan().unwrap_or_else(|e| panic!("{what}: scan failed: {e}"));
                w.lazy_rescan().unwrap_or_else(|e| panic!("{what}: rescan failed: {e}"));
                let all = w.list_playables().unwrap_or_else(|e| panic!("{what}: list failed: {e}"));
                assert!(all.iter().any(|p| p.title.as_deref() == Some("Legit Title")), "{what}");
                drop(w);
                // The second open (a restart) succeeds too, and a reader can open it.
                let w = LibraryWriter::open(&LibraryRoot::new(&stick))
                    .unwrap_or_else(|e| panic!("{what}: second open failed: {e}"));
                drop(w);
                let r = LibraryReader::open(&LibraryRoot::new(&stick))
                    .unwrap_or_else(|e| panic!("{what}: reader failed: {e}"));
                r.list_playables().unwrap_or_else(|e| panic!("{what}: reader list failed: {e}"));
                drop(r);
                fs::remove_dir_all(&stick).unwrap();
            }
        }
    }
    assert!(cases > 200, "{cases}");
}

#[test]
fn a_reader_refuses_a_wrongly_typed_cell_instead_of_failing_every_read() {
    let (tmp, root) = indexed_stick();
    let fresh = tmp.path().join("fresh");
    copy_tree(&root, &fresh);
    tamper(&fresh, "UPDATE playables SET title = x'00ff'");
    let err = LibraryReader::open(&LibraryRoot::new(&fresh)).err().expect("a BLOB title opened");
    assert!(err.is_unusable_index(), "{err}");
}

/// Descriptors of this process that point below `dir`.
fn open_fds_under(dir: &Path) -> Vec<PathBuf> {
    let dir = fs::canonicalize(dir).unwrap();
    fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .filter_map(|e| fs::read_link(e.path()).ok())
        .filter(|t| t.starts_with(&dir))
        .collect()
}

#[test]
fn no_descriptor_stays_open_on_the_volume_after_the_connections_are_gone() {
    let (tmp, hot) = stick_with_planted_wal();
    let w = LibraryWriter::open(&LibraryRoot::new(&hot)).unwrap();
    let r = LibraryReader::open(&LibraryRoot::new(&hot)).unwrap();
    drop(w);
    drop(r);
    let left = open_fds_under(&hot);
    assert!(left.is_empty(), "descriptors still pin the stick: {left:?}");
    drop(tmp);
}

#[test]
fn hostile_file_names_never_reach_the_scan_warnings_raw() {
    let (_tmp, root) = stick();
    // A broken document whose NAME carries a bidi override and a control character.
    let name = "Music/\u{202E}evil\u{7}name.noudata";
    write(&root.join(name), "this is not json");
    let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
    let report = w.full_scan().unwrap();
    assert!(!report.warnings.is_empty(), "the broken document is reported");
    for warning in &report.warnings {
        assert!(
            !warning.chars().any(|c| c.is_control() || c == '\u{202E}'),
            "{warning:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The vet lock: a slow peer must never cost anyone their index
// ---------------------------------------------------------------------------

/// Role "holdlock": hold `.plippa/.vet.lock` like a peer that is slowly opening a big index.
#[test]
fn child_holds_the_vet_lock() {
    if std::env::var(CHILD_ENV).as_deref() != Ok("holdlock") {
        return;
    }
    use plippa_core::safe_fs::{lock_file_exclusive, FileLock};
    let root = PathBuf::from(std::env::var(CHILD_ROOT_ENV).unwrap());
    let held = lock_file_exclusive(&root.join(".plippa/.vet.lock"), 0, 1, Duration::from_secs(5)).unwrap();
    assert!(matches!(held, FileLock::Held(_)));
    println!("READY");
    std::io::stdout().flush().unwrap();
    let mut sink = String::new();
    let _ = std::io::stdin().read_line(&mut sink);
}

fn snapshot_index_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = fs::read_dir(root.join(".plippa"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("library.sqlite"))
        .map(|e| (e.file_name().to_string_lossy().to_string(), fs::read(e.path()).unwrap()))
        .collect();
    v.sort();
    v
}

fn open_in_child(root: &Path, wait_ms: Option<u64>) -> String {
    let mut cmd = child_command("cold-writer", root, "child_cold_opens_the_index");
    if let Some(ms) = wait_ms {
        cmd.env("PLIPPA_INDEX_OPEN_WAIT_MS", ms.to_string());
    }
    let mut kid = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let _ = writeln!(kid.stdin.as_mut().unwrap(), "go");
    let out = kid.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.split_once("COLD-").map(|(_, r)| format!("COLD-{r}")))
        .unwrap_or_else(|| "COLD-FAIL no result".into())
}

#[test]
fn a_peer_holding_the_vet_lock_past_the_wait_costs_nobody_their_index() {
    let (tmp, root) = indexed_stick();
    let stick = tmp.path().join("busy");
    copy_tree(&root, &stick);
    // A live index files set (including a WAL) that must survive untouched.
    assert!(stick.join(".plippa/library.sqlite").exists());
    let mut holder = child_command("holdlock", &stick, "child_holds_the_vet_lock")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(holder.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "holder exited early");
        if line.contains("READY") {
            break;
        }
    }
    let before = snapshot_index_files(&stick);
    let outcome = open_in_child(&stick, Some(300));
    assert!(outcome.starts_with("COLD-FAIL") && outcome.contains("busy"), "{outcome}");
    assert_eq!(snapshot_index_files(&stick), before, "a busy open touched the index files");
    // Released: a later open succeeds, with the same index.
    drop(holder.stdin.take());
    let _ = holder.wait();
    let outcome = open_in_child(&stick, None);
    assert!(outcome.starts_with("COLD-OK 1"), "{outcome}");
}

#[test]
fn a_planted_unopenable_vet_lock_on_a_writable_volume_is_replaced_and_vetting_still_happens() {
    // Needs root to build the scenario (a stick owned by an unprivileged user holding a lock
    // file that user cannot open); elsewhere there is nothing to set up.
    if fs::metadata("/proc/self").unwrap().uid() != 0 {
        return;
    }
    let (tmp, hot) = stick_with_planted_wal();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    fn give(dir: &Path) {
        std::os::unix::fs::chown(dir, Some(65534), Some(65534)).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        for e in fs::read_dir(dir).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                give(&e.path());
            } else {
                std::os::unix::fs::chown(e.path(), Some(65534), Some(65534)).unwrap();
            }
        }
    }
    give(&hot);
    let lock = hot.join(".plippa/.vet.lock");
    fs::write(&lock, b"").unwrap(); // root-owned
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o000)).unwrap();
    let mut cmd = child_command("cold-writer", &hot, "child_cold_opens_the_index");
    use std::os::unix::process::CommandExt;
    cmd.uid(65534).gid(65534);
    let mut kid = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let _ = writeln!(kid.stdin.as_mut().unwrap(), "go");
    let out = kid.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("COLD-OK 1"), "{text}");
    // The planted foreign WAL did not survive (vetting happened, it was not skipped).
    let r = LibraryReader::open(&LibraryRoot::new(&hot)).ok();
    let planted = r
        .map(|r| r.list_playables().unwrap().iter().any(|p| p.title.as_deref().unwrap_or("").contains("PLANTED")))
        .unwrap_or(false);
    assert!(!planted);
}

#[test]
fn a_symlinked_plippa_directory_is_never_written_or_deleted_through() {
    let tmp = tempfile::tempdir().unwrap();
    let victim = tmp.path().join("victim");
    fs::create_dir_all(&victim).unwrap();
    for name in ["library.sqlite-wal", "library.sqlite-shm", "library.sqlite-journal", "library.sqlite"] {
        fs::write(victim.join(name), b"VICTIM").unwrap();
    }
    for writer in [false, true] {
        let root = tmp.path().join(if writer { "stick-w" } else { "stick-r" });
        write(&root.join("Music/a.mp3"), b"A");
        write(&root.join("Music/a.noudata"), song("Legit Title", "a.mp3"));
        symlink(&victim, root.join(".plippa")).unwrap();
        if writer {
            let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
            w.full_scan().unwrap();
            assert!(fs::symlink_metadata(root.join(".plippa")).unwrap().file_type().is_dir(), "replaced by a real directory");
        } else {
            assert!(LibraryReader::open(&LibraryRoot::new(&root)).is_err());
        }
        for name in ["library.sqlite-wal", "library.sqlite-shm", "library.sqlite-journal", "library.sqlite"] {
            assert_eq!(fs::read(victim.join(name)).unwrap(), b"VICTIM", "{name} of the victim was touched");
        }
        assert!(!victim.join(".vet.lock").exists());
    }
}

#[test]
fn processes_racing_to_rebuild_one_corrupt_index_all_succeed() {
    for _round in 0..15 {
        let (tmp, root) = indexed_stick();
        let stick = tmp.path().join("corrupt");
        copy_tree(&root, &stick);
        let db = stick.join(".plippa/library.sqlite");
        let mut bytes = fs::read(&db).unwrap();
        for b in &mut bytes[8192..16384] {
            *b = 0xFF;
        }
        fs::write(&db, bytes).unwrap();
        let _ = fs::remove_file(stick.join(".plippa/library.sqlite-wal"));
        let _ = fs::remove_file(stick.join(".plippa/library.sqlite-shm"));
        for r in cold_open_race(&stick, 4) {
            let bare_reader = r.contains("library index not found") || r.contains("not trustworthy");
            assert!(r.starts_with("COLD-OK") || bare_reader, "{r}");
        }
        // Exactly one valid index results: a fresh writer opens it and scans it.
        let mut w = LibraryWriter::open(&LibraryRoot::new(&stick)).unwrap();
        w.full_scan().unwrap();
        assert_eq!(titles(&w), ["Legit Title"]);
    }
}
