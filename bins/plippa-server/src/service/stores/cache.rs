//! The collection cache for a store connection (BCS_03).
//!
//! A JSON file, not a SQLite table: a Bandcamp collection is a few hundred releases
//! and a few hundred kilobytes, and the index schema is the wrong place for data that
//! is refreshed wholesale from a network service.
//!
//! * Location: `<library>/.plippa/stores/<store>.json`.
//! * Written temp-then-rename with mode `0600`; readers therefore never see a partial
//!   file, and an interrupted write cannot corrupt the previous cache.
//! * Refuses to write more than [`MAX_CACHE_BYTES`] of JSON. A collection that size is
//!   a product decision (paging or a database), not something to truncate silently: the
//!   sync reports failure and keeps whatever cache existed.
//! * A file that cannot be parsed is **renamed aside** to `<name>.corrupt-<ts>` and
//!   reported; it is never deleted, so the operator can look at what went wrong.
//! * Nothing here holds credentials: the file contains only the provider's own public
//!   metadata.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use plippa_store::StoreRelease;
use serde::{Deserialize, Serialize};

use super::{log_line, STORE_SLUG};

/// Directory under the library root that Plippa owns for its own state.
pub const STATE_DIR: &str = ".plippa";
/// Sub-directory holding store caches and preferences.
pub const STORES_DIR: &str = "stores";
/// Hard cap on the serialised cache.
pub const MAX_CACHE_BYTES: usize = 2 * 1024 * 1024;
/// Age after which a cache is refreshed on start-up.
pub const CACHE_MAX_AGE_SECS: u64 = 24 * 60 * 60;
/// Directory mode for Plippa's own state directories.
pub const DIR_MODE: u32 = 0o700;
/// File mode for the cache and preferences (not secret, but private).
pub const FILE_MODE: u32 = 0o600;
/// Hard cap on the preferences file (a few flags and timestamps).
pub const MAX_PREFS_BYTES: u64 = 64 * 1024;

/// Refuse a `.plippa` or `.plippa/stores` that is a symlink (or not a directory).
///
/// The stick is somebody else's input. A symlinked state directory would make every
/// read and every `0600` write land wherever the link points, so it is checked before
/// either. A missing directory is fine: reads answer "no state yet" and the write path
/// creates real directories `0700`.
pub fn check_state_dirs(root: &Path) -> Result<(), String> {
    let state = root.join(STATE_DIR);
    let stores = state.join(STORES_DIR);
    for dir in [&state, &stores] {
        match fs::symlink_metadata(dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "{} is a symbolic link; refusing to follow it",
                    dir.display()
                ))
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(format!("{} is not a directory", dir.display()));
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!("cannot inspect {}: {err}", dir.display()));
            }
        }
    }
    Ok(())
}

/// Read one state file with the shared hardened reader (PAR-02/SRV-05): `O_NOFOLLOW`,
/// `O_NONBLOCK`, regular-file only, `cap` enforced before allocating.
///
/// `Ok(None)` means "nothing usable": missing, refused (symlink, FIFO, device,
/// directory, over cap) or a symlinked state directory. `Err` is a genuine I/O error.
pub(crate) fn read_state_file(path: &Path, cap: u64) -> Result<Option<Vec<u8>>, String> {
    match plippa_core::safe_fs::read_regular_capped(path, cap) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) if err.kind() == ErrorKind::InvalidData => {
            log_line(&format!(
                "refused to read {}: it is not a regular file within the size cap ({err})",
                path.display()
            ));
            Ok(None)
        }
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

/// The persisted collection of one store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreCache {
    /// Store slug this cache belongs to (`bandcamp`).
    pub store_id: String,
    /// Releases in the order the provider listed them.
    pub releases: Vec<StoreRelease>,
    /// Page size the crawl used (so a reader knows how it was built).
    pub page_size: u32,
    /// When the crawl finished (Unix seconds); `0` for a cache built by single lookups.
    pub crawled_at: u64,
    /// `false` when the crawl stopped before the end of the collection.
    pub complete: bool,
    /// Why the cache is incomplete, in words the portal can show.
    #[serde(default)]
    pub last_error: Option<String>,
}

impl StoreCache {
    /// An empty cache for `store_id`.
    pub fn empty(store_id: &str, page_size: u32) -> Self {
        Self {
            store_id: store_id.to_string(),
            releases: Vec::new(),
            page_size,
            crawled_at: 0,
            complete: false,
            last_error: None,
        }
    }

    /// Serialise, refusing an oversized cache instead of truncating it.
    pub fn encode(&self) -> Result<Vec<u8>, CacheError> {
        let bytes = serde_json::to_vec(self).map_err(|err| CacheError::Encode(err.to_string()))?;
        if bytes.len() > MAX_CACHE_BYTES {
            return Err(CacheError::TooLarge {
                bytes: bytes.len(),
                releases: self.releases.len(),
            });
        }
        Ok(bytes)
    }

    /// Parse a cache file.
    pub fn decode(bytes: &[u8]) -> Result<Self, CacheError> {
        if bytes.len() > MAX_CACHE_BYTES {
            return Err(CacheError::TooLarge {
                bytes: bytes.len(),
                releases: 0,
            });
        }
        serde_json::from_slice(bytes).map_err(|err| CacheError::Decode(err.to_string()))
    }

    /// `true` when the cache is older than a day or is known to be incomplete.
    pub fn is_stale(&self, now: u64) -> bool {
        !self.complete || now.saturating_sub(self.crawled_at) >= CACHE_MAX_AGE_SECS
    }

    /// Index of a release by id.
    pub fn position(&self, id: &str) -> Option<usize> {
        self.releases.iter().position(|r| r.id.as_str() == id)
    }
}

/// Why a cache file could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheError {
    /// The serialised form would exceed [`MAX_CACHE_BYTES`].
    TooLarge { bytes: usize, releases: usize },
    Encode(String),
    Decode(String),
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::TooLarge { bytes, releases } => write!(
                f,
                "the collection cache would be {bytes} bytes{} which is above the {MAX_CACHE_BYTES}-byte limit",
                if *releases > 0 {
                    format!(" for {releases} releases")
                } else {
                    String::new()
                }
            ),
            CacheError::Encode(reason) => write!(f, "cannot encode the collection cache: {reason}"),
            CacheError::Decode(reason) => write!(f, "cannot read the collection cache: {reason}"),
        }
    }
}

impl std::error::Error for CacheError {}

/// Non-secret preferences for one store (auto-sync consent, last check times).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorePrefs {
    /// Store slug.
    #[serde(default)]
    pub store_id: String,
    /// The user agreed to an automatic collection refresh.
    #[serde(default)]
    pub auto_sync_consent: bool,
    /// Last successful credential check (Unix seconds).
    #[serde(default)]
    pub last_validated_at: Option<u64>,
    /// Last sync start of any kind (manual or automatic), for the 6-hour floor.
    #[serde(default)]
    pub last_sync_at: Option<u64>,
}

/// `<library>/.plippa/stores`
pub fn stores_dir(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join(STORES_DIR)
}

/// `<library>/.plippa/stores/<store>.json`
pub fn cache_path(root: &Path, store: &str) -> PathBuf {
    stores_dir(root).join(format!("{store}.json"))
}

/// `<library>/.plippa/stores/<store>.prefs.json`
pub fn prefs_path(root: &Path, store: &str) -> PathBuf {
    stores_dir(root).join(format!("{store}.prefs.json"))
}

/// The library root when `path` is `<root>/.plippa/stores/<file>`, else `None`.
fn state_root_of(path: &Path) -> Option<&Path> {
    let stores = path.parent()?;
    if stores.file_name()?.to_str()? != STORES_DIR {
        return None;
    }
    let state = stores.parent()?;
    if state.file_name()?.to_str()? != STATE_DIR {
        return None;
    }
    state.parent()
}

/// Read the cache file.
///
/// On a parse failure (or an impossibly large file) the file is renamed aside to
/// `<name>.corrupt-<ts>` and `Ok((None, Some(aside_name)))` is returned, so the caller
/// can report it. A missing file is `Ok((None, None))`; a symlinked state directory or a
/// cache that is not a regular file within [`MAX_CACHE_BYTES`] reads as "no cache".
pub fn read_cache(root: &Path) -> Result<(Option<StoreCache>, Option<String>), String> {
    if let Err(reason) = check_state_dirs(root) {
        log_line(&format!("store state directory refused: {reason}"));
        return Ok((None, Some(reason)));
    }
    let path = cache_path(root, STORE_SLUG);
    let bytes = match read_state_file(&path, MAX_CACHE_BYTES as u64)? {
        Some(bytes) => bytes,
        None => return Ok((None, None)),
    };
    match StoreCache::decode(&bytes) {
        Ok(cache) => {
            if cache.store_id != STORE_SLUG {
                let aside = move_aside(&path)?;
                return Ok((
                    None,
                    Some(format!(
                        "the store cache named a different store; moved aside as {aside}"
                    )),
                ));
            }
            Ok((Some(cache), None))
        }
        Err(err) => {
            let aside = move_aside(&path)?;
            Ok((
                None,
                Some(format!("{err}; moved aside as {aside}")),
            ))
        }
    }
}

/// Write `bytes` to `path` atomically with mode `0600`.
///
/// The temporary file lives in the same directory so the rename cannot cross a
/// filesystem, is flushed before the rename and is cleaned up on any failure. A state
/// directory that is a symlink is refused before anything is created, so a hostile stick
/// cannot redirect the write.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let Some(dir) = path.parent() else {
        return Err("the cache path has no parent directory".to_string());
    };
    if let Some(root) = state_root_of(path) {
        check_state_dirs(root)?;
    }
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(dir)
        .map_err(|err| format!("cannot create the store state directory: {err}"))?;
    // Re-check after creating: the directory now exists, so a link race is visible.
    if let Some(root) = state_root_of(path) {
        check_state_dirs(root)?;
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("store.json");
    let temp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));

    let write = || -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        fs::set_permissions(&temp, fs::Permissions::from_mode(FILE_MODE))
    };
    if let Err(err) = write() {
        let _ = fs::remove_file(&temp);
        return Err(format!("cannot write the store state file: {err}"));
    }
    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(format!("cannot store the state file: {err}"));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))
        .map_err(|err| format!("cannot set the state file mode: {err}"))
}

/// Read preferences; a missing, unreadable or refused file is "no preferences yet".
pub fn read_prefs(root: &Path) -> StorePrefs {
    if let Err(reason) = check_state_dirs(root) {
        log_line(&format!("store preferences refused: {reason}"));
        return default_prefs();
    }
    let path = prefs_path(root, STORE_SLUG);
    let bytes = match read_state_file(&path, MAX_PREFS_BYTES) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return default_prefs(),
        Err(err) => {
            log_line(&format!("{err}; using defaults"));
            return default_prefs();
        }
    };
    match serde_json::from_slice::<StorePrefs>(&bytes) {
        Ok(mut prefs) => {
            prefs.store_id = STORE_SLUG.to_string();
            prefs
        }
        Err(err) => {
            log_line(&format!(
                "store preferences could not be read ({err}); using defaults"
            ));
            default_prefs()
        }
    }
}

/// Write preferences (best effort; a failure is logged, never fatal).
pub fn write_prefs(root: &Path, prefs: &StorePrefs) -> Result<(), String> {
    let path = prefs_path(root, STORE_SLUG);
    let bytes = serde_json::to_vec(prefs)
        .map_err(|err| format!("cannot encode store preferences: {err}"))?;
    write_private_atomic(&path, &bytes)
}

fn default_prefs() -> StorePrefs {
    StorePrefs {
        store_id: STORE_SLUG.to_string(),
        ..StorePrefs::default()
    }
}

/// Rename `path` to `path.corrupt-<ts>` (adding a counter if that already exists) and
/// return the new file name.
fn move_aside(path: &Path) -> Result<String, String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("store.json");
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for attempt in 0..10 {
        let suffix = if attempt == 0 {
            format!("{name}.corrupt-{ts}")
        } else {
            format!("{name}.corrupt-{ts}-{attempt}")
        };
        let target = path.with_file_name(&suffix);
        match fs::rename(path, &target) {
            Ok(()) => {
                // The file was not written by us, so its mode is unknown: a copy of
                // metadata the portal may show is still kept private.
                if let Err(err) = fs::set_permissions(&target, fs::Permissions::from_mode(FILE_MODE)) {
                    log_line(&format!(
                        "could not set the moved-aside cache to {FILE_MODE:o}: {err}"
                    ));
                }
                log_line(&format!(
                    "the store cache was corrupt; moved aside to {suffix} (not deleted)"
                ));
                return Ok(suffix);
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("cannot move the corrupt store cache aside: {err}")),
        }
    }
    Err("cannot move the corrupt store cache aside: too many attempts".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::stores::tests_support::TempDir;
    use plippa_store::{StoreRelease, StoreReleaseId};

    fn release(id: &str) -> StoreRelease {
        StoreRelease {
            id: StoreReleaseId::new(id),
            title: format!("Release {id}"),
            artist: "Artist".into(),
            year: Some(2020),
            track_count: Some(10),
            duration_ms: Some(1_000),
            cover: None,
            web_url: None,
            created_unix: None,
        }
    }

    #[test]
    fn a_cache_round_trips_and_is_written_0600() {
        let temp = TempDir::new("cache-roundtrip");
        let root = temp.path();
        let mut cache = StoreCache::empty(STORE_SLUG, 200);
        cache.releases = vec![release("a:1"), release("a:2")];
        cache.crawled_at = 1_000;
        cache.complete = true;

        write_private_atomic(&cache_path(root, STORE_SLUG), &cache.encode().expect("encode"))
            .expect("write");
        let path = cache_path(root, STORE_SLUG);
        assert!(path.exists());
        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let (loaded, error) = read_cache(root).expect("read");
        assert_eq!(error, None);
        assert_eq!(loaded, Some(cache));
    }

    #[test]
    fn an_oversized_cache_is_refused_not_truncated() {
        let mut cache = StoreCache::empty(STORE_SLUG, 200);
        cache.releases = (0..20_000).map(|i| release(&format!("a:{i}"))).collect();
        match cache.encode() {
            Err(CacheError::TooLarge { bytes, releases }) => {
                assert!(bytes > MAX_CACHE_BYTES);
                assert_eq!(releases, 20_000);
            }
            // Never print the body: it is megabytes of JSON.
            Ok(bytes) => panic!(
                "expected TooLarge, but {} releases encoded to {} bytes",
                cache.releases.len(),
                bytes.len()
            ),
            Err(other) => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn a_corrupt_cache_is_renamed_aside_never_deleted() {
        let temp = TempDir::new("cache-corrupt");
        let root = temp.path();
        let path = cache_path(root, STORE_SLUG);
        fs::create_dir_all(path.parent().unwrap()).expect("dir");
        fs::write(&path, b"{not json").expect("write");

        let (cache, report) = read_cache(root).expect("read");
        assert!(cache.is_none());
        let report = report.expect("reported");
        assert!(report.contains("corrupt"), "{report}");
        assert!(!path.exists(), "the corrupt file must be moved, not left in place");
        let names: Vec<String> = fs::read_dir(path.parent().unwrap())
            .expect("dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].starts_with("bandcamp.json.corrupt-"), "{names:?}");
        // The content survives the move, so it can be inspected.
        let moved = path.parent().unwrap().join(&names[0]);
        assert_eq!(fs::read(&moved).expect("read moved"), b"{not json");
    }

    #[test]
    fn a_cache_for_another_store_is_moved_aside() {
        let temp = TempDir::new("cache-wrong-store");
        let root = temp.path();
        let mut cache = StoreCache::empty("otherstore", 200);
        cache.complete = true;
        write_private_atomic(&cache_path(root, STORE_SLUG), &cache.encode().unwrap()).expect("write");
        let (loaded, report) = read_cache(root).expect("read");
        assert!(loaded.is_none());
        assert!(report.expect("reported").contains("different store"));
        assert!(!cache_path(root, STORE_SLUG).exists());
    }

    #[test]
    fn staleness_follows_age_and_completeness() {
        let mut cache = StoreCache::empty(STORE_SLUG, 200);
        assert!(cache.is_stale(1_000), "an empty cache is stale");
        cache.complete = true;
        cache.crawled_at = 1_000;
        assert!(!cache.is_stale(1_000));
        assert!(!cache.is_stale(1_000 + CACHE_MAX_AGE_SECS - 1));
        assert!(cache.is_stale(1_000 + CACHE_MAX_AGE_SECS));
        cache.complete = false;
        assert!(cache.is_stale(1_000), "an incomplete cache is always stale");
    }

    #[test]
    fn preferences_round_trip_and_a_broken_file_reads_as_default() {
        let temp = TempDir::new("prefs");
        let root = temp.path();
        assert_eq!(read_prefs(root), default_prefs());

        let prefs = StorePrefs {
            store_id: STORE_SLUG.into(),
            auto_sync_consent: true,
            last_validated_at: Some(42),
            last_sync_at: Some(41),
        };
        write_prefs(root, &prefs).expect("write");
        assert_eq!(read_prefs(root), prefs);

        fs::write(prefs_path(root, STORE_SLUG), b"junk").expect("corrupt");
        assert_eq!(read_prefs(root), default_prefs());
    }

    #[test]
    fn a_missing_cache_is_not_an_error() {
        let temp = TempDir::new("cache-missing");
        assert_eq!(read_cache(temp.path()).expect("read"), (None, None));
    }
}
