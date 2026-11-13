//! The persistent "this file crashed the media helper" mark.
//!
//! Keyed by `(absolute path, mtime_ns, size)`: editing or replacing the file makes it eligible
//! again, anything else keeps it out of every helper, in every process, across restarts.
//! Lives on the **device** (not on the stick: no extra stick writes, and a hostile stick can
//! neither pre-clear nor pre-poison it), in `$PLIPPA_STATE_DIR` or
//! `$HOME/.local/state/plippa`. Bounded ([`MAX_MARKS`], oldest dropped), written atomically
//! (`0600` file in a `0700` directory, symlinks refused). A damaged or oversized mark file is
//! ignored: the cost is that a crashing file is tried (and marked) once more, never a crash.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Entries kept; the oldest is dropped when a new one would exceed it.
pub const MAX_MARKS: usize = 4096;
/// A mark file larger than this is treated as damaged (4096 paths of at most 4 KiB, plus slack).
const MAX_MARK_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MARK_FILE: &str = "media-crash-marks.bin";
const LOCK_FILE: &str = "media-crash-marks.lock";
const FORMAT_VERSION: u16 = 1;

/// Environment variable naming the device state directory.
pub const STATE_DIR_ENV: &str = "PLIPPA_STATE_DIR";

/// What identifies one version of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub mtime_ns: i128,
    pub size: u64,
}

impl FileStamp {
    /// The stamp of an **open** file (`fstat`: the very object that will be decoded).
    pub fn of(file: &File) -> io::Result<Self> {
        let m = file.metadata()?;
        Ok(Self {
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            size: m.len(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    /// Path bytes (unix paths are not necessarily UTF-8).
    path: Vec<u8>,
    stamp: FileStamp,
    marked_at_unix: u64,
    reason: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Disk {
    version: u16,
    entries: VecDeque<Entry>,
}

/// Cache of the parsed file, refreshed when the file on disk changes (another process may
/// have marked something).
#[derive(Default)]
struct Cache {
    fingerprint: Option<(u64, i64, i64, u64)>, // ino, mtime, mtime_nsec, size
    entries: VecDeque<Entry>,
}

/// The mark store for one state directory.
pub struct CrashMarks {
    /// `None`: marks live in this process only (no state directory is known).
    dir: Option<PathBuf>,
    cache: Mutex<Cache>,
}

impl std::fmt::Debug for CrashMarks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrashMarks").field("dir", &self.dir).finish()
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl CrashMarks {
    /// A store in `dir` (created on the first mark, mode `0700`).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: Some(dir.into()), cache: Mutex::new(Cache::default()) }
    }

    /// A store that is never persisted: marks last as long as the process.
    pub fn in_memory() -> Self {
        Self { dir: None, cache: Mutex::new(Cache::default()) }
    }

    /// The store the environment names: `$PLIPPA_STATE_DIR`, else `$HOME/.local/state/plippa`;
    /// with neither set, marks live in this process only.
    pub fn from_env() -> Self {
        if let Some(dir) = std::env::var_os(STATE_DIR_ENV).filter(|d| !d.is_empty()) {
            return Self::new(dir);
        }
        match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            Some(home) => Self::new(Path::new(&home).join(".local/state/plippa")),
            None => Self::in_memory(),
        }
    }

    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    fn file_path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(MARK_FILE))
    }

    /// Read the mark file; any problem (missing, symlink, too large, undecodable) is "no marks".
    fn read_disk(&self) -> (Option<(u64, i64, i64, u64)>, VecDeque<Entry>) {
        let Some(path) = self.file_path() else { return (None, VecDeque::new()) };
        let file = match plippa_core::safe_fs::open_regular_nofollow(&path) {
            Ok(f) => f,
            Err(_) => return (None, VecDeque::new()),
        };
        let Ok(meta) = file.metadata() else { return (None, VecDeque::new()) };
        let fingerprint = Some((meta.ino(), meta.mtime(), meta.mtime_nsec(), meta.len()));
        if meta.len() > MAX_MARK_FILE_BYTES {
            return (fingerprint, VecDeque::new());
        }
        let Ok(bytes) = plippa_core::safe_fs::read_capped(file, MAX_MARK_FILE_BYTES) else {
            return (fingerprint, VecDeque::new());
        };
        let entries = match postcard::from_bytes::<Disk>(&bytes) {
            Ok(d) if d.version == FORMAT_VERSION => d.entries,
            _ => VecDeque::new(),
        };
        (fingerprint, entries)
    }

    fn with_fresh<R>(&self, f: impl FnOnce(&mut VecDeque<Entry>) -> R) -> R {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        let current = self
            .file_path()
            .and_then(|p| fs::symlink_metadata(p).ok())
            .map(|m| (m.ino(), m.mtime(), m.mtime_nsec(), m.len()));
        if current != cache.fingerprint {
            let (fp, entries) = self.read_disk();
            cache.fingerprint = fp;
            cache.entries = entries;
        }
        f(&mut cache.entries)
    }

    /// Whether this exact version of `path` is marked.
    pub fn is_marked(&self, path: &Path, stamp: FileStamp) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let key = path.as_os_str().as_bytes();
        self.with_fresh(|entries| entries.iter().any(|e| e.path == key && e.stamp == stamp))
    }

    /// Mark this version of `path` (idempotent). A failure to persist is returned; the mark is
    /// still held in memory for this process.
    pub fn mark(&self, path: &Path, stamp: FileStamp, reason: &str) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let entry = Entry {
            path: path.as_os_str().as_bytes().to_vec(),
            stamp,
            marked_at_unix: unix_now(),
            reason: reason.chars().take(200).collect(),
        };
        // Held in memory first: even if the state directory cannot be written, this process
        // does not retry the file.
        self.with_fresh(|entries| push_bounded(entries, entry.clone()));
        self.persist(entry)
    }

    fn persist(&self, entry: Entry) -> io::Result<()> {
        let (Some(dir), Some(file_path)) = (self.dir.as_deref(), self.file_path()) else {
            return Ok(());
        };
        ensure_private_dir(dir)?;
        // Serialise writers across processes (engine and server both mark).
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir.join(LOCK_FILE))?;
        // SAFETY: flock on an owned, open descriptor.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Reload under the lock so a concurrent writer's marks are kept.
        let (_, mut entries) = self.read_disk();
        push_bounded(&mut entries, entry);
        let bytes = postcard::to_allocvec(&Disk { version: FORMAT_VERSION, entries: entries.clone() })
            .map_err(|e| io::Error::other(e.to_string()))?;
        write_atomic_private(dir, &file_path, &bytes)?;
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        cache.entries = entries;
        cache.fingerprint = fs::symlink_metadata(&file_path)
            .ok()
            .map(|m| (m.ino(), m.mtime(), m.mtime_nsec(), m.len()));
        Ok(())
    }

    /// Number of marks currently held.
    pub fn len(&self) -> usize {
        self.with_fresh(|e| e.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Add `entry` (replacing an identical key) and keep at most [`MAX_MARKS`], dropping the oldest.
fn push_bounded(entries: &mut VecDeque<Entry>, entry: Entry) {
    entries.retain(|e| !(e.path == entry.path && e.stamp == entry.stamp));
    entries.push_back(entry);
    while entries.len() > MAX_MARKS {
        entries.pop_front();
    }
}

/// Create `dir` (and parents) mode `0700`; refuse a symlink or a non-directory in its place.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state directory is a symlink or not a directory",
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
        }
        Err(e) => Err(e),
    }
}

/// Temp file (`0600`, never follows a planted link) in `dir`, then rename over `target`.
fn write_atomic_private(dir: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!(".{MARK_FILE}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut f = File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&tmp)?;
    let result = f.write_all(bytes).and_then(|()| f.sync_all()).and_then(|()| fs::rename(&tmp, target));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn stamp(n: i128, size: u64) -> FileStamp {
        FileStamp { mtime_ns: n, size }
    }

    #[test]
    fn a_mark_survives_a_restart_and_is_private() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("state/plippa");
        let a = CrashMarks::new(&dir);
        let p = Path::new("/mnt/stick/Music/bad.mp3");
        assert!(!a.is_marked(p, stamp(1, 10)));
        a.mark(p, stamp(1, 10), "died by signal 6").unwrap();
        assert!(a.is_marked(p, stamp(1, 10)));
        // A new store instance on the same directory (a restart) still refuses it.
        let b = CrashMarks::new(&dir);
        assert!(b.is_marked(p, stamp(1, 10)));
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(dir.join(MARK_FILE)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_changed_mtime_or_size_or_path_is_tried_again() {
        let d = tempfile::tempdir().unwrap();
        let m = CrashMarks::new(d.path());
        let p = Path::new("/mnt/stick/bad.flac");
        m.mark(p, stamp(5, 100), "timeout").unwrap();
        assert!(m.is_marked(p, stamp(5, 100)));
        assert!(!m.is_marked(p, stamp(6, 100)), "mtime changed");
        assert!(!m.is_marked(p, stamp(5, 101)), "size changed");
        assert!(!m.is_marked(Path::new("/mnt/stick/other.flac"), stamp(5, 100)));
    }

    #[test]
    fn a_damaged_mark_file_is_ignored_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        let m = CrashMarks::new(d.path());
        let p = Path::new("/x/y.mp3");
        m.mark(p, stamp(1, 1), "r").unwrap();
        for junk in [&b""[..], b"garbage garbage garbage", &[0xFF; 64][..]] {
            fs::write(d.path().join(MARK_FILE), junk).unwrap();
            let fresh = CrashMarks::new(d.path());
            assert!(!fresh.is_marked(p, stamp(1, 1)));
            // and marking again repairs the file
            fresh.mark(p, stamp(1, 1), "r").unwrap();
            assert!(CrashMarks::new(d.path()).is_marked(p, stamp(1, 1)));
        }
        // A symlink in place of the mark file is not followed.
        fs::remove_file(d.path().join(MARK_FILE)).unwrap();
        let target = d.path().join("elsewhere");
        fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink(&target, d.path().join(MARK_FILE)).unwrap();
        assert!(!CrashMarks::new(d.path()).is_marked(p, stamp(1, 1)));
    }

    #[test]
    fn the_store_is_bounded_and_drops_the_oldest() {
        let d = tempfile::tempdir().unwrap();
        let m = CrashMarks::new(d.path());
        for i in 0..(MAX_MARKS + 3) {
            m.mark(Path::new(&format!("/f/{i}.mp3")), stamp(1, 1), "r").unwrap();
        }
        assert_eq!(m.len(), MAX_MARKS);
        assert!(!m.is_marked(Path::new("/f/0.mp3"), stamp(1, 1)));
        assert!(m.is_marked(Path::new(&format!("/f/{}.mp3", MAX_MARKS + 2)), stamp(1, 1)));
    }

    #[test]
    fn another_process_marks_become_visible() {
        let d = tempfile::tempdir().unwrap();
        let reader = CrashMarks::new(d.path());
        let p = Path::new("/z.mp3");
        assert!(!reader.is_marked(p, stamp(1, 1)));
        CrashMarks::new(d.path()).mark(p, stamp(1, 1), "r").unwrap();
        assert!(reader.is_marked(p, stamp(1, 1)));
    }

    #[test]
    fn a_symlinked_state_directory_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let m = CrashMarks::new(&link);
        assert!(m.mark(Path::new("/a.mp3"), stamp(1, 1), "r").is_err());
        assert!(!real.join(MARK_FILE).exists());
        // ... but this process still remembers it.
        assert!(m.is_marked(Path::new("/a.mp3"), stamp(1, 1)));
    }
}
