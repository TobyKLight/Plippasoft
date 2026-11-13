//! The library index file is **untrusted input** (`SECFIX_05`, `PAR-02` / `SYS-04`).
//!
//! `.plippa/library.sqlite` lives on the stick, so whoever prepared the stick controls its
//! bytes, its `-wal` / `-shm` / `-journal` siblings and every row. Every process that opens
//! it (server, engine, UI) goes through [`super::open_connection`] and the checks here:
//!
//! - **Hardening** on every connection: `SQLITE_DBCONFIG_DEFENSIVE`, `trusted_schema=OFF`,
//!   `cell_size_check=ON`, `query_only` for readers, `SQLITE_OPEN_NOFOLLOW`.
//! - **Foreign `-wal` / `-shm` / `-journal`**: a hot WAL or journal planted next to a clean
//!   database is replayed into it when the database is opened. [`prepare_files`] removes the
//!   side files at the first open of a volume in a process, but only when it can prove no
//!   other process has the index open (see there). The index is derived data: losing a
//!   WAL tail costs one rescan.
//! - **Object check** before anything of ours runs against the file: only our own tables and
//!   indexes, no view, trigger, virtual table or unknown object.
//! - **Schema check** after migration: the live structure (columns, indexes, flags) must
//!   equal the structure generated from this code's own `SCHEMA_V2`.
//! - **`PRAGMA quick_check`**, bounded by a file-size cap.
//!
//! Any failure is [`LibraryError::ForeignIndex`], which counts as an unusable index: the
//! writer deletes the file and rebuilds it from the `.noudata` files (the existing recovery
//! path). A read-only volume cannot be rebuilt: the error surfaces and the library stays
//! empty with a notice, nothing is written and nothing crashes. Rows that survive every
//! structural check are still revalidated one by one when they are read (`rows` below),
//! because a consistent hostile database passes all of this.

use super::{INDEX_DB_FILENAME, PLAYER_DB_DIR, SCHEMA_V2};
use crate::error::LibraryError;
use crate::sanitise::{has_unsafe_chars, sanitise_key, sanitise_title};
use plippa_core::safe_fs::{lock_file_exclusive, try_lock_range_exclusive, FileLock, RangeLock};
use rusqlite::config::DbConfig;
use rusqlite::Connection;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Mutex;

/// Refuse an index file larger than this (bytes): a real index of a 20,000-playable library
/// is a few tens of MB, and `quick_check` is linear in the file size.
pub(super) const MAX_INDEX_BYTES: u64 = 512 * 1024 * 1024;

/// Side files SQLite would replay or share when it opens the database.
const SIDE_FILES: [&str; 3] = [
    "library.sqlite-wal",
    "library.sqlite-shm",
    "library.sqlite-journal",
];

/// Byte SQLite's unix VFS locks (shared) on the `-shm` file for as long as a connection is
/// open (`UNIX_SHM_DMS` = `UNIX_SHM_BASE` 120 + 8 locks).
const SHM_DMS_BYTE: u64 = 128;

fn foreign(detail: impl Into<String>) -> LibraryError {
    LibraryError::ForeignIndex {
        detail: detail.into(),
    }
}

// ---------------------------------------------------------------------------
// Connection hardening
// ---------------------------------------------------------------------------

/// Apply the defensive settings to a freshly opened connection (before it reads anything).
pub(super) fn harden(conn: &Connection, read_only: bool) -> Result<(), LibraryError> {
    // DEFENSIVE: no writable_schema, no direct shadow-table writes, no corrupting pragmas.
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    // trusted_schema=OFF: SQL inside the file's own views/triggers/defaults cannot call
    // functions with side effects.
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    conn.pragma_update(None, "cell_size_check", "ON")?;
    if read_only {
        conn.pragma_update(None, "query_only", "ON")?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Side files
// ---------------------------------------------------------------------------

type Identity = (u64, u64, u128);

/// Volumes this process has already vetted (the side-file removal is done once per volume).
static SEEN: Mutex<Option<HashSet<Identity>>> = Mutex::new(None);

/// One mutex per volume, so a stalled read on one stick never blocks another stick's open
/// (including the hot-swap path). A plain flag + condvar so the guard owns its `Arc`.
struct VolumeMutex {
    busy: Mutex<bool>,
    cv: std::sync::Condvar,
}

static VOLUMES: Mutex<Option<std::collections::HashMap<Identity, std::sync::Arc<VolumeMutex>>>> =
    Mutex::new(None);

struct VolumeGuard(std::sync::Arc<VolumeMutex>);

impl VolumeGuard {
    /// `None` when another thread of this process still holds the volume after `wait`.
    fn lock(identity: Identity, wait: std::time::Duration) -> Option<Self> {
        let vol = {
            let mut map = VOLUMES.lock().unwrap_or_else(|p| p.into_inner());
            map.get_or_insert_with(Default::default)
                .entry(identity)
                .or_insert_with(|| {
                    std::sync::Arc::new(VolumeMutex {
                        busy: Mutex::new(false),
                        cv: std::sync::Condvar::new(),
                    })
                })
                .clone()
        };
        let deadline = std::time::Instant::now() + wait;
        let mut busy = vol.busy.lock().unwrap_or_else(|p| p.into_inner());
        while *busy {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            busy = vol.cv.wait_timeout(busy, left).unwrap_or_else(|p| p.into_inner()).0;
        }
        *busy = true;
        drop(busy);
        Some(Self(vol))
    }
}

impl Drop for VolumeGuard {
    fn drop(&mut self) {
        *self.0.busy.lock().unwrap_or_else(|p| p.into_inner()) = false;
        self.0.cv.notify_one();
    }
}

/// Environment override of the wait for another process that is opening the same index
/// (milliseconds; used by the tests, and a tunable for very slow sticks).
pub const OPEN_WAIT_ENV: &str = "PLIPPA_INDEX_OPEN_WAIT_MS";

/// How long a process waits for another one that is opening the same index: 15 s plus one
/// second per 16 MiB of index (the peer runs `quick_check` and the cell-type check over it),
/// at most 90 s. A timeout is [`LibraryError::Busy`]: never a reason to delete anything.
fn open_wait(db_path: &Path) -> std::time::Duration {
    if let Some(ms) = std::env::var(OPEN_WAIT_ENV).ok().and_then(|v| v.parse::<u64>().ok()) {
        return std::time::Duration::from_millis(ms);
    }
    let size = fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    std::time::Duration::from_secs((15 + size / (16 * 1024 * 1024)).min(90))
}

/// How long an open may wait for another process (or thread) that is opening the same index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenWait {
    /// For threads that must stay responsive (the UI loop, the server's service mutex, the
    /// engine): a short wait ([`INTERACTIVE_WAIT`]); a busy index is reported as
    /// [`LibraryError::Busy`] and tried again soon.
    Interactive,
    /// For background workers (scanner, waveform worker, cover backfill): the long adaptive
    /// wait ([`open_wait`]).
    Background,
}

/// The wait of [`OpenWait::Interactive`].
pub const INTERACTIVE_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

impl OpenWait {
    fn duration(self, db_path: &Path) -> std::time::Duration {
        match self {
            Self::Interactive => INTERACTIVE_WAIT,
            Self::Background => open_wait(db_path),
        }
    }
}

fn busy(detail: impl Into<String>) -> LibraryError {
    LibraryError::Busy {
        detail: detail.into(),
    }
}

/// Name of the dedicated lock file in `.plippa` (never deleted, never used by SQLite).
const VET_LOCK: &str = ".vet.lock";

/// The checks around the index that must complete before SQLite touches it, held until the
/// connection is open (`SECFIX_05`).
///
/// **Exclusion.** Every open of an index (first or not, reader or writer) takes an exclusive
/// lock on the dedicated file `.plippa/.vet.lock` for as long as it takes to open and validate
/// its connection, then releases it (closing that descriptor only affects that file, never
/// SQLite's). Opens of the same volume are thereby serialised across processes (this also
/// ends the creation race of several processes opening a bare stick at once) and, within
/// the process, by a per-volume mutex. The lock file is never deleted, so the exclusion
/// cannot be undermined by removing the file that is locked.
/// - The index itself must be absent or a regular file (a symlink, FIFO or device named
///   `library.sqlite` is [`LibraryError::ForeignIndex`]; opening a FIFO would block).
/// - On the **first open of this volume in this process**, under that lock: a `-shm` that is
///   not a regular file is removed, then `-wal`, `-shm` and `-journal` are deleted **if no
///   other process has the index open**, decided by an exclusive probe of the byte-128 lock
///   every open SQLite WAL connection holds on `-shm`. A live index is left alone. The probe
///   cannot see this process's own other connections, hence "first open in this process".
/// - On a volume that refuses writes (read-only, no permission) nothing is deleted and no lock
///   is taken; SQLite opens the index read-only and every validation still runs.
/// - `quick_check` and the cell-type check run only on that first open.
pub(super) struct VolumeGate {
    volume: Option<VolumeGuard>,
    lock: Option<fs::File>,
    identity: Option<Identity>,
    first: bool,
    /// What is wrong with the index file itself (not a regular file, implausibly large),
    /// found under the lock so that the rebuild can delete it there.
    problem: std::cell::Cell<Option<&'static str>>,
}

impl Drop for VolumeGate {
    /// Order matters: close the lock file first, release the in-process volume mutex after.
    /// (The other way round, another thread could take its lock while this thread's still
    /// holds the file, and the later close would drop the process's lock for both.)
    fn drop(&mut self) {
        self.lock.take();
        self.volume.take();
    }
}

impl VolumeGate {
    pub(super) fn enter(db_path: &Path, how: OpenWait) -> Result<Self, LibraryError> {
        let problem = match fs::symlink_metadata(db_path) {
            Ok(m) if !m.file_type().is_file() => Some("the index is not a regular file"),
            Ok(m) if m.len() > MAX_INDEX_BYTES => Some("the index file is implausibly large"),
            Ok(_) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let unguarded = || Self {
            volume: None,
            lock: None,
            identity: None,
            first: false,
            problem: std::cell::Cell::new(problem),
        };
        let Some(dir) = db_path.parent() else {
            return Ok(unguarded());
        };
        // `.plippa` must be a real directory: through a symlink the lock file would be created
        // and the side files deleted in somebody else's directory.
        match fs::symlink_metadata(dir) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => return Err(foreign("the index directory is not a real directory")),
            Err(_) => return Ok(unguarded()),
        }
        // Birth time too: a freed inode number is handed straight back out by ext4, so
        // `(dev, ino)` alone could mistake a newly inserted stick for one already vetted.
        let identity: Identity = match fs::metadata(dir) {
            Ok(m) => (
                m.dev(),
                m.ino(),
                m.created()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos()),
            ),
            Err(_) => return Ok(unguarded()),
        };
        let wait = how.duration(db_path);
        let volume = VolumeGuard::lock(identity, wait)
            .ok_or_else(|| busy("another thread is still opening the index"))?;
        let lock_path = dir.join(VET_LOCK);
        let lock = {
            let mut replaced = false;
            loop {
                match lock_file_exclusive(&lock_path, 0, 1, wait) {
                    Ok(l) => break l,
                    // Another process still opens the index: temporary, nothing is deleted.
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                        return Err(busy("another process is still opening the index"));
                    }
                    // A hostile directory/symlink/FIFO, or a file this user cannot open, where
                    // the lock file belongs: it lives in our own `.plippa`, replace it once.
                    Err(e)
                        if !replaced
                            && matches!(
                                e.kind(),
                                std::io::ErrorKind::InvalidData
                                    | std::io::ErrorKind::IsADirectory
                                    | std::io::ErrorKind::PermissionDenied
                            ) =>
                    {
                        replaced = true;
                        if fs::symlink_metadata(&lock_path).is_err() {
                            // Nothing there to replace: the directory itself refuses us
                            // (a read-only tree for this user): not ours to vet.
                            break FileLock::ReadOnly;
                        }
                        match remove_any(&lock_path) {
                            Ok(()) => {}
                            Err(LibraryError::Io(e)) if refuses_writes(&e) => {
                                break FileLock::ReadOnly;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    // Anything else (including a second refusal): fail closed, never an
                    // unvetted open and never a deletion.
                    Err(e) => return Err(busy(format!("cannot lock the index directory: {e}"))),
                }
            }
        };
        // Read-only volume or a file system that cannot lock: nothing can be vetted or
        // deleted here; SQLite opens the index as it can and every validation still runs.
        let (lock, writable) = match lock {
            FileLock::Held(f) => (Some(f), true),
            FileLock::ReadOnly | FileLock::Unsupported => (None, false),
        };
        let first = !SEEN
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(HashSet::new)
            .contains(&identity);
        if first && writable {
            remove_side_files_if_unused(dir)?;
        }
        Ok(Self {
            volume: Some(volume),
            lock,
            identity: Some(identity),
            first,
            problem: std::cell::Cell::new(problem),
        })
    }

    /// What is wrong with the index file itself, if anything.
    pub(super) fn problem(&self) -> Option<&'static str> {
        self.problem.get()
    }

    /// The index files were removed under the lock: the recorded problem is gone with them.
    pub(super) fn problem_removed(&self) {
        self.problem.set(None);
    }

    /// Whether this gate may delete the index files: only while the dedicated lock is held
    /// (a read-only volume or a file system without locks never deletes anything).
    pub(super) fn can_delete(&self) -> bool {
        self.lock.is_some()
    }

    /// True on the first open of this volume in this process (deep checks run then).
    pub(super) fn first(&self) -> bool {
        self.first
    }

    /// The vetting connection is open and validated: remember the volume. Dropping the gate
    /// releases the volume mutex and the lock file (no descriptor on the volume outlives the
    /// open, so the stick can be ejected).
    pub(super) fn finish(self) {
        if let (true, Some(identity)) = (self.first, self.identity) {
            SEEN.lock()
                .unwrap_or_else(|p| p.into_inner())
                .get_or_insert_with(HashSet::new)
                .insert(identity);
        }
    }
}

/// The volume refuses writes (read-only file system, no permission).
fn refuses_writes(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ReadOnlyFilesystem | std::io::ErrorKind::PermissionDenied
    )
}

/// Delete `-wal` / `-shm` / `-journal` unless another process has the index open (called under
/// the dedicated lock, first open of the volume in this process only).
fn remove_side_files_if_unused(dir: &Path) -> Result<(), LibraryError> {
    let shm = dir.join("library.sqlite-shm");
    // A directory or symlink where `-shm` should be is hostile, never a live SQLite's file.
    if let Ok(m) = fs::symlink_metadata(&shm) {
        if !m.file_type().is_file() {
            remove_any(&shm)?;
        }
    }
    // The probe lock is released again before anything is deleted: nobody else can open the
    // index meanwhile (they wait on the dedicated lock), and a live connection is exactly
    // what makes the probe fail.
    match try_lock_range_exclusive(&shm, SHM_DMS_BYTE, 1) {
        Ok(RangeLock::Absent) | Ok(RangeLock::Acquired(_)) => {}
        // A live index of another process: leave its files alone.
        Ok(RangeLock::Contended { .. }) => return Ok(()),
        // The side files cannot be opened for writing (read-only volume, no permission):
        // not ours to delete.
        Err(e) if refuses_writes(&e) => return Ok(()),
        Err(e) => return Err(foreign(format!("cannot lock the index side files: {e}"))),
    }
    for name in SIDE_FILES {
        match remove_any(&dir.join(name)) {
            Ok(()) => {}
            // Read-only volume: nothing to delete here.
            Err(LibraryError::Io(e)) if refuses_writes(&e) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Remove a file, symlink or (hostile) directory sitting where an index file should be.
pub(super) fn remove_any(path: &Path) -> Result<(), LibraryError> {
    let removed = match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) => Err(e),
    };
    match removed {
        Ok(()) => Ok(()),
        // Already gone (another process removed it first): that is the goal.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Objects and schema
// ---------------------------------------------------------------------------

const KNOWN_TABLES: [&str; 8] = [
    "meta",
    "playables",
    "playable_items",
    "media_claims",
    "playable_covers",
    "media_files",
    "noudata_skips",
    "dirs",
];

const KNOWN_INDEXES: [&str; 7] = [
    "idx_playables_key",
    "idx_playables_kind",
    "idx_covers_playable",
    "idx_covers_relpath",
    "idx_claims_media",
    "idx_claims_playable",
    "idx_dirs_parent",
];

/// Before migration (or reading a legacy file): only our own tables and indexes may exist.
/// A view, trigger, virtual table, `sqlite_stat*` table or unknown object is foreign.
pub(super) fn check_objects(conn: &Connection) -> Result<(), LibraryError> {
    let mut stmt = conn.prepare("SELECT type, name, sql FROM sqlite_master")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (ty, name, sql) = row?;
        let ok = match ty.as_str() {
            "table" => {
                KNOWN_TABLES.contains(&name.as_str())
                    && !sql
                        .as_deref()
                        .is_some_and(|s| s.to_ascii_uppercase().starts_with("CREATE VIRTUAL"))
            }
            "index" => {
                KNOWN_INDEXES.contains(&name.as_str())
                    || (sql.is_none() && name.starts_with("sqlite_autoindex_"))
            }
            _ => false,
        };
        if !ok {
            return Err(foreign(format!("unexpected {ty} in the index file")));
        }
    }
    Ok(())
}

/// One canonical string per schema object: structure, not text (comments, whitespace and the
/// column order an `ALTER TABLE ADD COLUMN` migration leaves behind do not matter).
fn structure_of(conn: &Connection) -> Result<BTreeMap<String, String>, LibraryError> {
    let mut out = BTreeMap::new();
    let objects: Vec<(String, String, String, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    for (ty, name, tbl, sql) in objects {
        let q = name.replace('"', "\"\"");
        let desc = match ty.as_str() {
            "table" => {
                let mut cols: Vec<String> = Vec::new();
                let mut stmt = conn.prepare(&format!("PRAGMA table_xinfo(\"{q}\")"))?;
                let rows = stmt.query_map([], |r| {
                    Ok(format!(
                        "{}|{}|{}|{:?}|{}|{}",
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                })?;
                for c in rows {
                    cols.push(c?);
                }
                cols.sort();
                let flags: (i64, i64) = conn.query_row(
                    "SELECT wr, strict FROM pragma_table_list WHERE name = ?1",
                    [&name],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let fks: i64 = conn.query_row(
                    &format!("SELECT COUNT(*) FROM pragma_foreign_key_list(\"{q}\")"),
                    [],
                    |r| r.get(0),
                )?;
                let upper = sql.as_deref().unwrap_or("").to_ascii_uppercase();
                format!(
                    "table;{tbl};wr={};strict={};fks={fks};check={};{}",
                    flags.0,
                    flags.1,
                    upper.contains("CHECK"),
                    cols.join(";")
                )
            }
            "index" => {
                let (unique, origin, partial): (i64, String, i64) = conn.query_row(
                    "SELECT \"unique\", origin, partial FROM pragma_index_list(?1) WHERE name = ?2",
                    [&tbl, &name],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                let mut keys: Vec<String> = Vec::new();
                let mut stmt = conn.prepare(&format!("PRAGMA index_xinfo(\"{q}\")"))?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })?;
                for k in rows {
                    let (seq, col, desc, coll, key) = k?;
                    if key == 1 {
                        keys.push(format!("{seq}:{col:?}:{desc}:{coll}"));
                    }
                }
                format!("index;{tbl};u={unique};o={origin};p={partial};{}", keys.join(";"))
            }
            other => format!("{other};{tbl}"),
        };
        out.insert(format!("{ty}:{name}"), desc);
    }
    Ok(out)
}

/// The structure this build creates from `SCHEMA_V2` (an in-memory database).
fn canonical_structure() -> Result<BTreeMap<String, String>, LibraryError> {
    let mem = Connection::open_in_memory()?;
    mem.execute_batch(SCHEMA_V2)?;
    structure_of(&mem)
}

/// After migration: the live structure must equal the canonical one, and `meta` must say the
/// current schema version and nothing surprising.
pub(super) fn check_schema_exact(conn: &Connection) -> Result<(), LibraryError> {
    let live = structure_of(conn)?;
    let want = canonical_structure()?;
    if live != want {
        let differing: Vec<&String> = live
            .keys()
            .chain(want.keys())
            .filter(|k| live.get(*k) != want.get(*k))
            .collect();
        return Err(foreign(format!(
            "the index structure differs from this build's ({} object(s), first: {})",
            differing.len(),
            differing.first().map(|s| s.as_str()).unwrap_or("?")
        )));
    }
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .ok();
    if version.as_deref().map(str::trim) != Some(&super::SCHEMA_VERSION.to_string()) {
        return Err(foreign("the index schema version is not the current one"));
    }
    let extra: i64 = conn.query_row(
        "SELECT COUNT(*) FROM meta WHERE key != 'schema_version'",
        [],
        |r| r.get(0),
    )?;
    if extra != 0 {
        return Err(foreign("unexpected rows in the index meta table"));
    }
    Ok(())
}

/// Every cell must have the type its column declares (`INTEGER` -> integer, `TEXT` -> text,
/// NULL only in nullable columns): a BLOB in a text column or a string in an integer column
/// passes `quick_check` and the schema checks, and then fails typed reads for ever. Runs on
/// a database whose structure already equals this build's (see [`check_schema_exact`]).
pub(super) fn check_types(conn: &Connection) -> Result<(), LibraryError> {
    for table in KNOWN_TABLES {
        let mut conds: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare(&format!("PRAGMA table_xinfo(\"{table}\")"))?;
            let cols = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(5)?))
            })?;
            for c in cols {
                let (name, ty, notnull, pk) = c?;
                let allowed = match ty.to_ascii_uppercase().as_str() {
                    "INTEGER" => "'integer'",
                    "TEXT" => "'text'",
                    other => return Err(foreign(format!("unexpected column type {other}"))),
                };
                let nullable = notnull == 0 && pk == 0;
                let list = if nullable { format!("{allowed},'null'") } else { allowed.to_string() };
                conds.push(format!("typeof(\"{}\") NOT IN ({list})", name.replace('"', "\"\"")));
            }
        }
        let sql = format!("SELECT 1 FROM \"{table}\" WHERE {} LIMIT 1", conds.join(" OR "));
        let bad: Option<i64> = conn.query_row(&sql, [], |r| r.get(0)).ok();
        if bad.is_some() {
            return Err(foreign(format!("a cell of {table} has the wrong type")));
        }
    }
    Ok(())
}

/// `PRAGMA quick_check`: page-level integrity, bounded by [`MAX_INDEX_BYTES`] (checked in
/// [`prepare_files`]) and by reporting at most a few problems.
pub(super) fn quick_check(conn: &Connection) -> Result<(), LibraryError> {
    let mut stmt = conn.prepare("PRAGMA quick_check(4)")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut first = true;
    for row in rows {
        let line = row?;
        if !(first && line == "ok") {
            return Err(foreign(format!("integrity check failed: {line}")));
        }
        first = false;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Row revalidation (the read layer)
// ---------------------------------------------------------------------------

/// A display string read from the index: control, bidi and zero-width characters stripped,
/// whitespace collapsed, NFC, capped (cheap fast path for the common clean ASCII case).
pub(super) fn clean_title(s: String) -> String {
    if s.len() <= 200
        && s.is_ascii()
        && !has_unsafe_chars(&s)
        && !s.starts_with(' ')
        && !s.ends_with(' ')
        && !s.contains("  ")
    {
        return s;
    }
    sanitise_title(&s)
}

pub(super) fn clean_title_opt(s: Option<String>) -> Option<String> {
    s.map(clean_title)
}

/// A Noubin key read from the index: refused (`None`) when it carries unsafe characters.
pub(super) fn clean_key(s: Option<String>) -> Option<String> {
    s.and_then(|k| sanitise_key(&k))
}

/// A library-relative path read from the index; `None` when it is not acceptable.
pub(super) fn clean_path(s: &str) -> Option<String> {
    crate::confine::clean_relpath(s)
}

/// A path list stored as JSON; unacceptable entries are dropped.
pub(super) fn clean_path_list(json: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(json)
        .unwrap_or_default()
        .iter()
        .filter_map(|p| clean_path(p))
        .collect()
}

/// A `.noudata` path from the index: acceptable and really a `.noudata`.
pub(super) fn clean_noudata_path(s: &str) -> Option<String> {
    clean_path(s).filter(|p| p.ends_with(".noudata"))
}

/// [`remove_index_files`] for the directory of `db_path` (under the volume lock).
pub(super) fn remove_index_files_in(db_path: &Path) -> Result<(), LibraryError> {
    match db_path.parent().and_then(Path::parent) {
        Some(root) => remove_index_files(root),
        None => Err(foreign("the index has no directory")),
    }
}

/// Remove the index and its side files (also a hostile directory or symlink in their place).
pub(super) fn remove_index_files(root: &Path) -> Result<(), LibraryError> {
    let dir = root.join(PLAYER_DB_DIR);
    // Never delete through a symlinked (or otherwise replaced) `.plippa`.
    if !fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_dir()) {
        return Err(foreign("the index directory is not a real directory"));
    }
    remove_any(&dir.join(INDEX_DB_FILENAME))?;
    for name in SIDE_FILES {
        remove_any(&dir.join(name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::open_connection;
    use super::*;

    #[test]
    fn a_defensive_connection_refuses_writable_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.sqlite");
        let conn = open_connection(&db, true).unwrap();
        conn.execute_batch("CREATE TABLE t (a)").unwrap();
        assert!(conn.db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE).unwrap());
        assert!(!conn.db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA).unwrap());
        // The classic route to a hostile schema: edit sqlite_master directly.
        let _ = conn.execute_batch("PRAGMA writable_schema = ON");
        let err = conn
            .execute_batch("UPDATE sqlite_master SET name = 'evil' WHERE name = 't'")
            .unwrap_err();
        assert!(err.to_string().contains("may not be modified"), "{err}");
    }

    #[test]
    fn readers_are_query_only_and_cell_checked() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("x.sqlite");
        drop(open_connection(&db, true).unwrap());
        let reader = open_connection(&db, false).unwrap();
        let q: i64 = reader.query_row("PRAGMA query_only", [], |r| r.get(0)).unwrap();
        assert_eq!(q, 1);
        let c: i64 = reader.query_row("PRAGMA cell_size_check", [], |r| r.get(0)).unwrap();
        assert_eq!(c, 1);
        assert!(reader.execute_batch("CREATE TABLE t (a)").is_err());
    }

    #[test]
    fn the_canonical_structure_matches_what_migration_leaves_behind() {
        // A v3 index upgraded in place has `covers_hidden` at the end of `playables`: the
        // structural comparison must not mind the column order.
        let mem = Connection::open_in_memory().unwrap();
        mem.execute_batch(&SCHEMA_V2.replace(
            "        covers_hidden INTEGER NOT NULL DEFAULT 0, -- explicit empty cover list: no inherited art\n",
            "",
        ))
        .unwrap();
        mem.execute_batch("ALTER TABLE playables ADD COLUMN covers_hidden INTEGER NOT NULL DEFAULT 0;")
            .unwrap();
        assert_eq!(structure_of(&mem).unwrap(), canonical_structure().unwrap());
    }

    #[test]
    fn foreign_objects_are_found() {
        let mem = Connection::open_in_memory().unwrap();
        mem.execute_batch(SCHEMA_V2).unwrap();
        assert!(check_objects(&mem).is_ok());
        mem.execute_batch("CREATE VIEW v AS SELECT 1").unwrap();
        assert!(matches!(check_objects(&mem), Err(LibraryError::ForeignIndex { .. })));
    }
}
