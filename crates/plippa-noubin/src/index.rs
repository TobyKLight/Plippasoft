//! Media library SQLite index under `.plippa/` (Noubin §4.6), schema v2.
//!
//! ## API roles
//!
//! - [`LibraryWriter`] — write-oriented helpers for `plippa-server` (scan, ingest, rescan,
//!   duration probe). Scanning lives in `index/scan.rs`.
//! - [`LibraryReader`] — read-only lookup for `plippa-engine` / UI. Prefer slightly stale data
//!   over blocking on writer locks (WAL + short busy timeout).
//!
//! Both open the **same** DB file; do not atomically swap/replace that file between processes.
//!
//! The index is **derived**: `.noudata` files are the source of truth. Deleting `.plippa/`
//! and rescanning reproduces it. See the crate README for the table layout.

mod guard;
mod scan;

pub(crate) use scan::try_resolve_rel;

use crate::covers::{self, CoverRef, CoverSource, ReleaseDoc};
use crate::error::LibraryError;
use crate::noudata::{fs_release_resolver, parse_noudata, NoudataDocument};
use crate::noudata_types::CueListType;
use crate::paths::LibraryRoot;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub use guard::OpenWait;
pub use scan::ProbeReport;

/// Player database directory on the media volume (§4.6 / §4.4).
pub const PLAYER_DB_DIR: &str = ".plippa";

/// SQLite index filename inside [`.plippa/`](PLAYER_DB_DIR).
pub const INDEX_DB_FILENAME: &str = "library.sqlite";

/// Waveform sidecar directory inside [`.plippa/`](PLAYER_DB_DIR) (WAVE_01).
pub const WAVEFORM_DIR: &str = "waveforms";

/// Current schema version (`meta.schema_version`).
pub const SCHEMA_VERSION: u32 = 4;

/// Schema version that introduced `playable_covers`.
const COVERS_SCHEMA_VERSION: u32 = 4;

/// A reference scan ([`LibraryWriter::referencing_noudata`]) reads at most this many `.noudata`
/// files; a bigger library is refused instead of answering from a partial look, because a
/// missed reference could let a still-used image be deleted.
pub const MAX_REFERENCE_SCAN_DOCS: usize = 20_000;
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// PWRSW_01: on writer connections, a successful `wal_checkpoint(TRUNCATE)` shrinks the
/// `-wal` file down to this limit instead of to zero unless the size is reset. 8 MiB is
/// large enough for the scanner's batches and small enough to bound a stuck WAL.
const JOURNAL_SIZE_LIMIT_BYTES: i64 = 8 * 1024 * 1024;

/// What a playable is, for browsing (derived per §4.9 at index time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LibraryKind {
    /// Single-item playable: a per-track file referring to a release, or an orphan media file.
    Track,
    Album,
    Playlist,
    Mix,
    Audiobook,
    Podcast,
}

impl LibraryKind {
    pub const ALL: [LibraryKind; 6] = [
        Self::Track,
        Self::Album,
        Self::Playlist,
        Self::Mix,
        Self::Audiobook,
        Self::Podcast,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::Playlist => "playlist",
            Self::Mix => "mix",
            Self::Audiobook => "audiobook",
            Self::Podcast => "podcast",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Kinds that come from a `.noudata` with its own `releaseData`.
    pub fn is_release(self) -> bool {
        matches!(
            self,
            Self::Album | Self::Mix | Self::Audiobook | Self::Podcast
        )
    }
}

/// One indexed playable (derived from a `.noudata` file, or an orphan media file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayableRecord {
    pub id: i64,
    /// Path to the `.noudata` file, relative to the library root (POSIX separators).
    /// Empty for an orphan media file (no `.noudata` yet).
    pub noudata_relpath: String,
    pub noubin_key_normalised: Option<String>,
    pub title: Option<String>,
    /// Media file paths relative to the library root (POSIX separators): every
    /// `localFilePaths` entry of every item, de-duplicated (alternates are flattened).
    pub media_relpaths: Vec<String>,
    /// CLASSIFICATION (what the file is): a loose or link track is always `Track`, a release
    /// is `Album`/`Mix`/`Audiobook`/`Podcast` by its profile. Drives "is this a release".
    pub kind: LibraryKind,
    /// GROUPING (which library tab / device category lists it): `kind`, except that a loose
    /// track with profile `mix`/`audiobook`/`podcast` is grouped under that tab. Never use
    /// this to decide whether something is a release.
    pub group: LibraryKind,
    /// Effective `noudataType` (`release`, `userPlaylist`, `looseTrack`, `linkTrack`);
    /// `None` for an orphan media file (no `.noudata`).
    pub noudata_type: Option<String>,
    /// Root `playableItemProfile` (`music`, `mix`, ...) of a release or loose track; a link
    /// track carries its release's; `None` for playlists.
    pub release_profile: Option<String>,
    /// Playlist with `searchString` and an empty `mediaList` (§3.7).
    pub is_dynamic: bool,
    /// Media file with no `.noudata` claiming it (listed as a track with the file name).
    pub is_orphan: bool,
    pub primary_artist: Option<String>,
    /// Sum of item durations; `None` until every item's length is known.
    pub duration_ms: Option<u64>,
    /// Cover image, relative to the library root.
    pub cover_relpath: Option<String>,
    /// How many cover images the playable's own `.noudata` (or the release it shows) declares
    /// (the rows of `playable_covers`; the files may be missing, and a sibling `cover.jpg` is
    /// not counted). `0` on an index that predates covers. Use [`LibraryReader::covers_of`]
    /// for the images that exist.
    #[serde(default)]
    pub cover_count: u32,
    /// `referToReleaseNoudata` resolved relative to the library root.
    pub refers_to_relpath: Option<String>,
    pub has_navigable_cues: bool,
    /// At least one item's media file is missing on disk.
    pub media_missing: bool,
}

/// One `mediaList` entry of a playable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRecord {
    pub playable_id: i64,
    /// Index in the `.noudata` `mediaList` (0-based).
    pub position: u32,
    /// Chosen file for this item: the first `localFilePaths` entry that exists, else the first.
    pub media_relpath: String,
    /// All `localFilePaths` alternates, relative to the library root.
    pub media_relpaths: Vec<String>,
    /// `mediaTitle` as written in this playable's own `.noudata`.
    pub media_title: Option<String>,
    /// Display title: the authoritative release's title for playlist entries, else
    /// `mediaTitle`, else the file name.
    pub title: String,
    pub artist: Option<String>,
    /// For playlist entries: the playable (release / orphan) that supplied title + artist.
    pub resolved_from: Option<i64>,
    pub duration_ms: Option<u64>,
    /// Where `duration_ms` came from: `unite` (declared `expectedLength`), `probed`
    /// (measured from the media file), `failed` (probe failed), or `None` (unknown).
    pub duration_src: Option<String>,
    pub available: bool,
    pub has_navigable_cues: bool,
    pub cue_count: u32,
}

/// One row of the item-level library views (Tracks, Mixes): a single `mediaList` entry
/// together with where it lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRow {
    pub playable_id: i64,
    /// 0-based `mediaList` position inside the playable.
    pub position: u32,
    /// Resolved item title (falls back to the playable's title).
    pub title: String,
    pub artist: Option<String>,
    /// Title of the release / file the track belongs to.
    pub collection_title: String,
    /// How many items that playable has (1 for a loose track or an orphan file).
    pub collection_items: u32,
    pub duration_ms: Option<u64>,
    pub available: bool,
    pub is_orphan: bool,
    /// `.noudata` of the playable (relative to the library root); empty for an orphan file.
    pub noudata_relpath: String,
    /// The playable carries a Noubin key.
    pub associated: bool,
}

/// One cue of one item (read on demand from `.noudata`, see [`LibraryReader::cues_for`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueEntry {
    pub cue_list_index: usize,
    pub cue_index: usize,
    pub list_type: CueListType,
    pub list_title: Option<String>,
    pub navigable: bool,
    pub start_ms: u64,
    /// `endTime`, else next cue start, else the item length (when known).
    pub end_ms: Option<u64>,
    pub text: String,
    pub url: Option<String>,
}

/// Result of looking up a normalised Noubin key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupHit {
    /// All matching playables, ordered by `noudata_relpath` for determinism.
    pub records: Vec<PlayableRecord>,
    /// True when more than one playable shares the key (MVP may use [`LookupHit::first`]).
    pub multi_match: bool,
}

impl LookupHit {
    /// First match (deterministic order). Prefer this for MVP single-play behaviour.
    pub fn first(&self) -> &PlayableRecord {
        &self.records[0]
    }
}

/// Outcome of a key lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupResult {
    Found(LookupHit),
    NotFound,
}

/// Summary of a full or lazy scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// `.noudata` files found.
    pub scanned_files: u32,
    pub ingested: u32,
    pub updated: u32,
    pub removed: u32,
    pub skipped: u32,
    /// Orphan media files newly listed / no longer orphans or gone.
    pub orphans_added: u32,
    pub orphans_removed: u32,
    /// Items whose media file appeared / disappeared since the last scan.
    pub availability_changed: u32,
    /// `.noudata` files read + parsed this scan (counter for "lazy" assertions;
    /// includes referred-release reads done for classification).
    pub noudata_reads: u32,
    /// Directories listed with `read_dir` (unchanged directories are skipped by mtime).
    pub dirs_listed: u32,
    pub warnings: Vec<String>,
}

impl ScanReport {
    /// True when the index content changed (drives the generation bump + notify).
    pub fn changed(&self) -> bool {
        self.ingested > 0
            || self.updated > 0
            || self.removed > 0
            || self.orphans_added > 0
            || self.orphans_removed > 0
            || self.availability_changed > 0
    }
}

/// Write-oriented library index (`plippa-server`).
pub struct LibraryWriter {
    root: PathBuf,
    conn: Connection,
}

/// Read-only library index (`plippa-engine`).
pub struct LibraryReader {
    root: PathBuf,
    conn: Connection,
    /// False when the file is still a v1 index (writer has not migrated it yet):
    /// only the flat v1 columns are read, new-schema queries return empty. A long-lived
    /// reader (UI / engine) can be opened before the server migrates the index, so while
    /// this is false [`Self::is_v2`] re-reads the schema version instead of trusting the
    /// value from open time.
    v2: std::cell::Cell<bool>,
    /// False while the file is older than the covers schema (same re-check as `v2`).
    covers: std::cell::Cell<bool>,
}

/// Borrowed view shared by reader + writer query methods.
pub(crate) struct View<'a> {
    conn: &'a Connection,
    root: &'a Path,
    v2: bool,
    covers: bool,
}

/// Generates the query methods shared by [`LibraryWriter`] and [`LibraryReader`].
macro_rules! shared_queries {
    ($ty:ty) => {
        impl $ty {
            fn view(&self) -> View<'_> {
                View {
                    conn: &self.conn,
                    root: &self.root,
                    v2: self.is_v2(),
                    covers: self.has_covers(),
                }
            }

            pub fn library_root(&self) -> &Path {
                &self.root
            }

            /// Report current SQLite `journal_mode` (expected: `wal`).
            pub fn journal_mode(&self) -> Result<String, LibraryError> {
                read_journal_mode(&self.conn)
            }

            /// List all indexed playables (ordered by `noudata_relpath`, orphans by media path).
            pub fn list_playables(&self) -> Result<Vec<PlayableRecord>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().list(None)
            }

            /// Playables of one [`LibraryKind`] (ordered by title, then id).
            pub fn list_by_kind(&self, kind: LibraryKind) -> Result<Vec<PlayableRecord>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().list(Some(kind))
            }

            /// Playables GROUPED under one library tab (see [`PlayableRecord::group`]): like
            /// [`Self::list_by_kind`], but a loose mix / audiobook / podcast track is listed
            /// under Mixtapes / Audiobooks / Podcasts instead of Tracks.
            pub fn list_by_group(&self, group: LibraryKind) -> Result<Vec<PlayableRecord>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().list_group(group)
            }

            /// Playables of one library VIEW (tab), A-Z by title. For the item-level views
            /// (Tracks, Mixes) these are the playables that contain the tracks; use
            /// [`Self::list_view_tracks`] for the flat track list.
            pub fn list_by_view(&self, view: LibraryKind) -> Result<Vec<PlayableRecord>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().list_view(view)
            }

            /// Flat A-Z track list of an item-level view (Tracks or Mixes).
            pub fn list_view_tracks(&self, view: LibraryKind) -> Result<Vec<TrackRow>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().list_view_tracks(view)
            }

            /// Entries per library view: tracks for Tracks / Mixes, playables for the rest.
            /// Views without content are absent.
            pub fn view_counts(&self) -> Result<Vec<(LibraryKind, u64)>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().view_counts()
            }

            /// Count indexed playables.
            pub fn count_playables(&self) -> Result<u64, LibraryError> {
                let n: i64 = self
                    .conn
                    .query_row("SELECT COUNT(*) FROM playables", [], |row| row.get(0))?;
                Ok(n as u64)
            }

            /// One playable by library index id.
            pub fn get_playable(&self, id: i64) -> Result<Option<PlayableRecord>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().get(id)
            }

            /// Same as [`Self::get_playable`] (MVP name, kept for IPC `Load`).
            pub fn lookup_by_id(&self, id: i64) -> Result<Option<PlayableRecord>, LibraryError> {
                self.get_playable(id)
            }

            /// Number of `mediaList` entries per playable id (playables without items are absent).
            pub fn item_counts(&self) -> Result<HashMap<i64, u32>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().item_counts()
            }

            /// Playable count per kind (kinds without content are absent).
            pub fn kind_counts(&self) -> Result<Vec<(LibraryKind, u64)>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().kind_counts()
            }

            /// Playable count per GROUP / tab (groups without content are absent).
            pub fn group_counts(&self) -> Result<Vec<(LibraryKind, u64)>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().group_counts()
            }

            /// `(mtime_ns, size)` of the `.noudata` as last read by the scan (`None` for
            /// orphans / unknown ids). Compare with the file on disk to see whether the index
            /// is stale for this playable.
            pub fn indexed_fingerprint(&self, id: i64) -> Result<Option<(i64, u64)>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().indexed_fingerprint(id)
            }

            /// Every `mediaList` entry of a playable, in order.
            pub fn items_of(&self, playable_id: i64) -> Result<Vec<ItemRecord>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().items_of(playable_id)
            }

            /// **All** playables sharing `noubinKeyNormalised` (duplicates are indexed, not rejected).
            pub fn find_by_key(&self, key: &str) -> Result<Vec<PlayableRecord>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().find_by_key(key)
            }

            /// Look up playables by `noubinKeyNormalised`.
            ///
            /// Multiple matches are all returned; use [`LookupHit::first`] for MVP play-first behaviour.
            pub fn lookup_by_normalised_key(&self, key: &str) -> Result<LookupResult, LibraryError> {
                let records = self.find_by_key(key)?;
                if records.is_empty() {
                    Ok(LookupResult::NotFound)
                } else {
                    let multi_match = records.len() > 1;
                    Ok(LookupResult::Found(LookupHit {
                        records,
                        multi_match,
                    }))
                }
            }

            /// All non-null `noubinKeyNormalised` values (for associate collision checks).
            pub fn all_normalised_keys(&self) -> Result<HashSet<String>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                let mut stmt = self.conn.prepare(
                    "SELECT noubin_key_normalised FROM playables WHERE noubin_key_normalised IS NOT NULL",
                )?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                let mut out = HashSet::new();
                for row in rows {
                    if let Some(key) = guard::clean_key(Some(row?)) {
                        out.insert(key);
                    }
                }
                Ok(out)
            }

            /// Cues of one item, read on demand from the playable's `.noudata`
            /// (lists in file order, cues in list order). Empty for orphans / v1 index.
            /// The item's known length trims cues beyond the real media length.
            pub fn cues_for(
                &self,
                playable_id: i64,
                item_position: u32,
            ) -> Result<Vec<CueEntry>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().cues_for(playable_id, item_position)
            }

            /// The cover images of a playable that exist on disk, in display order (D4:
            /// the playable's own list, else the release it shows, else the primary sibling
            /// `cover.jpg`). Empty for an explicitly hidden cover, an orphan or an index
            /// that predates covers.
            pub fn covers_of(&self, playable_id: i64) -> Result<Vec<CoverRef>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().covers_of(playable_id)
            }

            /// The covers for one playing item (0-based `item_position`): the item's own, else
            /// its release's, else the collection's (D4). Read on demand from the `.noudata`
            /// like [`Self::cues_for`].
            pub fn item_covers(
                &self,
                playable_id: i64,
                item_position: u32,
            ) -> Result<Vec<CoverRef>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().item_covers(playable_id, item_position)
            }

            /// Library-relative paths of every `.noudata` that references the image
            /// `image_relpath` in any cover list (`coverImage`, `localCoverImage`,
            /// `itemCoverImage`), sorted. Decides whether an image file may be deleted (D9).
            /// Errors instead of guessing when the library has more than
            /// [`MAX_REFERENCE_SCAN_DOCS`] documents.
            pub fn referencing_noudata(&self, image_relpath: &str) -> Result<Vec<String>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                self.view().referencing_noudata(image_relpath)
            }

            /// Index-level warnings (duplicate `noubinKeyNormalised`, media claimed by two
            /// `.noudata`). Empty when the index is clean.
            pub fn library_warnings(&self) -> Result<Vec<String>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                if !self.is_v2() {
                    return Ok(Vec::new());
                }
                scan::library_warnings(self.conn_ref())
            }

            /// Every playable media file that can need a waveform sidecar (WAVE_01):
            /// distinct `playable_items.media_relpath` with `available = 1`, with the
            /// `media_files` fingerprint when the index has one (`mtime_ns = -1` otherwise).
            ///
            /// Includes chapters of an audiobook and orphan / loose tracks; cues are inside
            /// a file and need no separate waveform. Ordering is unspecified.
            pub fn analysis_candidates(&self) -> Result<Vec<MediaCandidate>, LibraryError> {
                ensure_root_still_present(&self.root)?;
                if !self.is_v2() {
                    return Ok(Vec::new());
                }
                let mut stmt = self.conn.prepare(
                    "SELECT DISTINCT i.media_relpath, m.size, m.mtime_ns
                     FROM playable_items i
                     LEFT JOIN media_files m ON m.relpath = i.media_relpath
                     WHERE i.available = 1
                     ORDER BY i.media_relpath ASC",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok(MediaCandidate {
                        relpath: row.get(0)?,
                        size: row.get::<_, Option<i64>>(1)?.unwrap_or(0).max(0) as u64,
                        mtime_ns: row.get::<_, Option<i64>>(2)?.unwrap_or(-1),
                    })
                })?;
                let mut out = Vec::new();
                for row in rows {
                    let mut candidate = row?;
                    // The index is untrusted: only an acceptable library path is offered to
                    // the waveform worker.
                    if let Some(rel) = guard::clean_path(&candidate.relpath) {
                        candidate.relpath = rel;
                        out.push(candidate);
                    }
                }
                Ok(out)
            }
        }
    };
}

shared_queries!(LibraryWriter);
shared_queries!(LibraryReader);

impl LibraryWriter {
    /// Open or create the index under `<library_root>/.plippa/` in WAL mode.
    ///
    /// A v1 index is migrated in place (ids are preserved, every row is re-read by the
    /// next scan). Returns [`LibraryError::MissingRoot`] / [`LibraryError::InvalidRoot`]
    /// without panicking when storage is absent or not a directory.
    ///
    /// A damaged index (unclean USB unplug) is **deleted and rebuilt**: the index is derived
    /// from `.noudata`, so this only costs the next scan, whereas refusing to open leaves
    /// the player with a library that looks empty forever.
    pub fn open(root: &LibraryRoot) -> Result<Self, LibraryError> {
        Self::open_with(root, OpenWait::Background)
    }

    /// [`Self::open`] with an explicit wait policy: threads that must stay responsive pass
    /// [`OpenWait::Interactive`] and get [`LibraryError::Busy`] when another process is still
    /// opening the same index (nothing is deleted for that; try again soon).
    pub fn open_with(root: &LibraryRoot, wait: OpenWait) -> Result<Self, LibraryError> {
        // The delete-and-rebuild of a damaged index happens inside `open_writable`, under the
        // volume's lock, after re-checking there that the index is still unusable.
        Self::open_once(root, wait)
    }

    fn open_once(root: &LibraryRoot, wait: OpenWait) -> Result<Self, LibraryError> {
        let root_path = validate_library_root(root)?;
        let db_path = ensure_db_dir_and_path(&root_path)?;
        let conn = open_writable(&db_path, false, wait)?;
        Ok(Self {
            root: root_path,
            conn,
        })
    }

    /// Run one index operation, reopening the connection once when it turns out to be stale.
    ///
    /// This is the recovery path for the USB flapping case: a stick is unplugged and plugged
    /// back in at the same path, leaving the old connection pointing at the gone mount, so
    /// every write fails with `database disk image is malformed`. Closing and reopening the
    /// connection (from the same healthy file on the new mount) fixes it; if the file itself
    /// is damaged, [`Self::recover_connection`] deletes it and the caller's operation
    /// rebuilds every row from `.noudata`.
    fn with_recovery<T>(
        &mut self,
        mut op: impl FnMut(&Path, &mut Connection) -> Result<T, LibraryError>,
    ) -> Result<T, LibraryError> {
        match op(&self.root, &mut self.conn) {
            Err(e) if e.is_unusable_index() => {
                eprintln!(
                    "plippa-noubin: library index connection for {} went bad ({e}); reopening",
                    self.root.display()
                );
                self.recover_connection()?;
                op(&self.root, &mut self.conn)
            }
            other => other,
        }
    }

    /// Replace the connection after a corrupt/stale-handle error, rebuilding the index file
    /// when the damage is on disk as well.
    fn recover_connection(&mut self) -> Result<(), LibraryError> {
        // Drop the stale handle before touching the files: it may still hold the old mount's
        // inode open, and its shared-memory (`-shm`) state is what reports the corruption.
        let placeholder = Connection::open_in_memory()?;
        let stale = std::mem::replace(&mut self.conn, placeholder);
        drop(stale);

        let db_path = ensure_db_dir_and_path(&self.root)?;
        // Rebuilds the file (under the volume lock) when the damage is on disk as well.
        self.conn = open_writable(&db_path, true, OpenWait::Background)?;
        Ok(())
    }

    fn is_v2(&self) -> bool {
        true
    }

    fn has_covers(&self) -> bool {
        true
    }

    fn conn_ref(&self) -> &Connection {
        &self.conn
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join(PLAYER_DB_DIR).join(INDEX_DB_FILENAME)
    }

    /// Full scan: re-list every directory and re-read every `.noudata` (§4.5), skipping
    /// `.*` dirs (§4.4), depth ≥ 8. Rebuilds derived rows from the source files.
    pub fn full_scan(&mut self) -> Result<ScanReport, LibraryError> {
        self.with_recovery(|root, conn| scan::scan(root, conn, true))
    }

    /// Lazy rescan: only directories whose mtime changed are listed, only `.noudata` files
    /// whose mtime/size changed are read. A no-change scan writes nothing.
    pub fn lazy_rescan(&mut self) -> Result<ScanReport, LibraryError> {
        self.with_recovery(|root, conn| scan::scan(root, conn, false))
    }

    /// Index exactly one `.noudata` (library-relative), ignoring the directory mtime cache.
    ///
    /// The service calls this after writing a playlist: the result must not depend on
    /// filesystem timestamps (FAT only records them in 2 s steps). A missing file is treated
    /// as a removal. Errors name the path (caller surfaces them); the index is left untouched.
    pub fn ingest_noudata(&mut self, rel: &str) -> Result<ScanReport, LibraryError> {
        let rel = validate_noudata_rel(&self.root, rel)?;
        self.with_recovery(|root, conn| scan::ingest_noudata(root, conn, &rel))
    }

    /// Remove one `.noudata` (library-relative) from the index and its skip entry. No-op when
    /// absent.
    pub fn forget_noudata(&mut self, rel: &str) -> Result<ScanReport, LibraryError> {
        let rel = validate_noudata_rel(&self.root, rel)?;
        self.with_recovery(|root, conn| scan::forget_noudata(root, conn, &rel))
    }

    /// Move every committed WAL frame back into `library.sqlite` and truncate `-wal`
    /// (PWRSW_01 / USB corruption report). Best effort: `busy != 0` means another
    /// connection holds the checkpoint; that is normal (engine/UI readers) and is not an
    /// error — the next hook retries. Real SQL errors propagate.
    pub fn checkpoint_truncate(&self) -> Result<(), LibraryError> {
        let (busy, log, checkpointed): (i64, i64, i64) = self.conn.query_row(
            "PRAGMA wal_checkpoint(TRUNCATE)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if busy != 0 {
            eprintln!(
                "plippa-noubin: wal checkpoint busy (log {log} frames, {checkpointed} checkpointed) — will retry"
            );
        }
        Ok(())
    }

    /// Measure up to `max_items` items whose real length is not yet known and write it
    /// back to the `.noudata` (`uniteData.expectedLength`). See [`ProbeReport`].
    ///
    /// `probe` maps an absolute media path to its length in ms. Call from a background
    /// thread in small batches; skip while playing.
    pub fn probe_durations(
        &mut self,
        probe: &mut dyn FnMut(&Path) -> Option<u64>,
        max_items: usize,
    ) -> Result<ProbeReport, LibraryError> {
        self.with_recovery(|root, conn| {
            scan::probe_durations(root, conn, &mut *probe, max_items)
        })
    }
}

impl LibraryReader {
    /// Open an existing index for read-only lookups.
    ///
    /// If `.plippa/library.sqlite` does not exist yet, returns
    /// [`LibraryError::IndexMissing`] (engine can treat as empty / warn).
    pub fn open(root: &LibraryRoot) -> Result<Self, LibraryError> {
        Self::open_with(root, OpenWait::Background)
    }

    /// [`Self::open`] with an explicit wait policy (see [`LibraryWriter::open_with`]).
    pub fn open_with(root: &LibraryRoot, wait: OpenWait) -> Result<Self, LibraryError> {
        let root_path = validate_library_root(root)?;
        let db_path = root_path.join(PLAYER_DB_DIR).join(INDEX_DB_FILENAME);
        let gate = guard::VolumeGate::enter(&db_path, wait)?;
        if let Some(problem) = gate.problem() {
            return Err(LibraryError::ForeignIndex {
                detail: problem.to_string(),
            });
        }
        if !db_path.is_file() {
            return Err(LibraryError::IndexMissing { path: db_path });
        }
        let conn = open_connection(&db_path, false)?;
        guard::check_objects(&conn)?;
        let version = read_schema_version(&conn);
        // A current-version index must match this build's structure exactly. An older one
        // (a writer has not migrated it yet) is read stale as before, after the object check.
        if version == Some(SCHEMA_VERSION) {
            guard::check_schema_exact(&conn)?;
        }
        assert_wal(&conn)?;
        if gate.first() {
            guard::quick_check(&conn)?;
            if version == Some(SCHEMA_VERSION) {
                guard::check_types(&conn)?;
            }
        }
        gate.finish();
        let v2 = version.is_some_and(|v| v >= 2);
        let covers = version.is_some_and(|v| v >= COVERS_SCHEMA_VERSION);
        Ok(Self {
            root: root_path,
            conn,
            v2: std::cell::Cell::new(v2),
            covers: std::cell::Cell::new(covers),
        })
    }

    /// Cached once true; while false the writer may have migrated the file since this
    /// reader was opened, so look again (one row of `meta`).
    fn is_v2(&self) -> bool {
        if !self.v2.get() && read_schema_version(&self.conn).is_some_and(|v| v >= 2) {
            self.v2.set(true);
        }
        self.v2.get()
    }

    /// Same re-check as [`Self::is_v2`] for the covers schema.
    fn has_covers(&self) -> bool {
        if !self.covers.get()
            && read_schema_version(&self.conn).is_some_and(|v| v >= COVERS_SCHEMA_VERSION)
        {
            self.covers.set(true);
        }
        self.covers.get()
    }

    fn conn_ref(&self) -> &Connection {
        &self.conn
    }
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

const COLS_V2: &str = "p.id, p.noudata_relpath, p.noubin_key_normalised, p.title, \
    p.media_relpaths_json, p.kind, p.release_profile, p.is_dynamic, p.is_orphan, \
    p.primary_artist, p.duration_ms, p.cover_relpath, p.refers_to_relpath, \
    p.has_navigable_cues, \
    EXISTS(SELECT 1 FROM playable_items i WHERE i.playable_id = p.id AND i.available = 0), \
    p.noudata_type";
const COLS_V1: &str = "p.id, p.noudata_relpath, p.noubin_key_normalised, p.title, \
    p.media_relpaths_json, NULL, NULL, 0, 0, NULL, NULL, NULL, NULL, 0, 0, NULL, 0";
/// Column 16 of the playable row: the number of declared cover images.
const COVER_COUNT_SQL: &str = "(SELECT COUNT(*) FROM playable_covers c WHERE c.playable_id = p.id)";

/// SQL twin of [`group_of`]: the grouping (library tab) of a row of `playables p`.
const GROUP_SQL: &str = "CASE WHEN p.kind = 'track' AND p.is_orphan = 0 \
    AND p.noudata_type = 'looseTrack' AND p.release_profile IN ('mix', 'audiobook', 'podcast') \
    THEN p.release_profile ELSE p.kind END";

/// Library VIEWS (the tabs of the library UI, see `devPlippaSW/libraryUIHeirarchy.md`).
/// A release or loose track with a profile; link tracks are shown through their release,
/// playlists and orphan files are handled separately.
const REL_OR_LOOSE: &str = "p.is_orphan = 0 AND p.noudata_type IN ('release', 'looseTrack')";

/// `WHERE` condition (over `playables p`) selecting the playables a view is made of:
/// - Tracks: every release / loose track of profile `music` or `other`, plus media files
///   without a `.noudata` (they would otherwise not be listed anywhere);
/// - Albums: releases of profile `music`;
/// - Playlists: `noudataType` `userPlaylist`, whatever its profile;
/// - Mixes / Audiobooks / Podcasts: releases and loose tracks of that profile.
fn view_cond(view: LibraryKind) -> &'static str {
    match view {
        LibraryKind::Track => "((p.is_orphan = 0 AND p.noudata_type IN ('release', 'looseTrack') \
            AND COALESCE(p.release_profile, 'other') IN ('music', 'other')) OR p.is_orphan = 1)",
        LibraryKind::Album => "(p.is_orphan = 0 AND p.noudata_type = 'release' \
            AND p.release_profile = 'music')",
        LibraryKind::Playlist => "(p.is_orphan = 0 AND p.noudata_type = 'userPlaylist')",
        LibraryKind::Mix => "(p.is_orphan = 0 AND p.noudata_type IN ('release', 'looseTrack') \
            AND p.release_profile = 'mix')",
        LibraryKind::Audiobook => "(p.is_orphan = 0 AND p.noudata_type IN ('release', 'looseTrack') \
            AND p.release_profile = 'audiobook')",
        LibraryKind::Podcast => "(p.is_orphan = 0 AND p.noudata_type IN ('release', 'looseTrack') \
            AND p.release_profile = 'podcast')",
    }
}

/// Views listed per `mediaList` entry (a flat A-Z list of tracks) instead of per playable.
pub fn view_is_item_level(view: LibraryKind) -> bool {
    matches!(view, LibraryKind::Track | LibraryKind::Mix)
}

/// Grouping of a playable: its classification, except that a loose track with a long-form
/// profile is grouped under Mixtapes / Audiobooks / Podcasts (the profile drives library
/// grouping, standard 03 `playableItemProfile`). Link tracks and orphans stay Tracks.
fn group_of(kind: LibraryKind, noudata_type: Option<&str>, profile: Option<&str>) -> LibraryKind {
    if kind == LibraryKind::Track && noudata_type == Some("looseTrack") {
        match profile {
            Some("mix") => return LibraryKind::Mix,
            Some("audiobook") => return LibraryKind::Audiobook,
            Some("podcast") => return LibraryKind::Podcast,
            _ => {}
        }
    }
    kind
}

/// Profiles and types a row may carry; anything else read from the (untrusted) index is
/// treated as unknown.
const PROFILES: [&str; 5] = ["music", "mix", "audiobook", "podcast", "other"];
const NOUDATA_TYPES: [&str; 4] = ["release", "userPlaylist", "looseTrack", "linkTrack"];

fn known(value: Option<String>, allowed: &[&str]) -> Option<String> {
    value.filter(|v| allowed.contains(&v.as_str()))
}

/// Revalidate one `playables` row (`SECFIX_05`): the index file is untrusted, and a
/// consistent hostile database passes every structural check, so every value that becomes a
/// [`PlayableRecord`] is checked here, in the single read layer. Strings go through the
/// sanitiser; paths must be acceptable library paths (unacceptable ones are dropped, and a
/// row whose own `.noudata` or media list is unusable is not listed at all).
fn map_playable_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<PlayableRecord>> {
    let media_json: String = row.get(4)?;
    let kind: Option<String> = row.get(5)?;
    let noudata: Option<String> = row.get(1)?;
    let kind = kind
        .as_deref()
        .and_then(LibraryKind::parse)
        .unwrap_or(LibraryKind::Album);
    let is_orphan = row.get::<_, i64>(8)? != 0;
    let release_profile = known(row.get(6)?, &PROFILES);
    let noudata_type = known(row.get(15)?, &NOUDATA_TYPES).filter(|_| !is_orphan);
    let noudata_relpath = match noudata {
        None => String::new(),
        Some(p) => match guard::clean_noudata_path(&p) {
            Some(p) => p,
            None => return Ok(None),
        },
    };
    let media_relpaths = guard::clean_path_list(&media_json);
    if is_orphan && media_relpaths.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlayableRecord {
        id: row.get(0)?,
        noudata_relpath,
        noubin_key_normalised: guard::clean_key(row.get(2)?),
        title: guard::clean_title_opt(row.get(3)?),
        media_relpaths,
        // v1 rows carry no kind: best guess until the migrated rows are re-read.
        kind,
        group: group_of(kind, noudata_type.as_deref(), release_profile.as_deref()),
        noudata_type,
        release_profile,
        is_dynamic: row.get::<_, i64>(7)? != 0,
        is_orphan,
        primary_artist: guard::clean_title_opt(row.get(9)?),
        duration_ms: row.get::<_, Option<i64>>(10)?.map(|d| d.max(0) as u64),
        cover_relpath: row
            .get::<_, Option<String>>(11)?
            .and_then(|p| guard::clean_path(&p)),
        cover_count: row.get::<_, i64>(16)?.clamp(0, 10_000) as u32,
        refers_to_relpath: row
            .get::<_, Option<String>>(12)?
            .and_then(|p| guard::clean_noudata_path(&p)),
        has_navigable_cues: row.get::<_, i64>(13)? != 0,
        media_missing: row.get::<_, i64>(14)? != 0,
    }))
}

impl View<'_> {
    fn cols(&self) -> String {
        if !self.v2 {
            COLS_V1.to_string()
        } else if self.covers {
            format!("{COLS_V2}, {COVER_COUNT_SQL}")
        } else {
            format!("{COLS_V2}, 0")
        }
    }

    fn query(
        &self,
        tail: &str,
        args: impl rusqlite::Params,
    ) -> Result<Vec<PlayableRecord>, LibraryError> {
        let sql = format!("SELECT {} FROM playables p {tail}", self.cols());
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(args, map_playable_row)?;
        let mut out = Vec::new();
        for row in rows {
            if let Some(record) = row? {
                out.push(record);
            }
        }
        Ok(out)
    }

    fn list(&self, kind: Option<LibraryKind>) -> Result<Vec<PlayableRecord>, LibraryError> {
        match kind {
            None => self.query(
                "ORDER BY COALESCE(p.noudata_relpath, p.media_relpaths_json) ASC, p.id ASC",
                [],
            ),
            Some(_) if !self.v2 => Ok(Vec::new()),
            Some(k) => self.query(
                "WHERE p.kind = ?1 ORDER BY LOWER(COALESCE(p.title, '')) ASC, p.id ASC",
                params![k.as_str()],
            ),
        }
    }

    fn list_group(&self, group: LibraryKind) -> Result<Vec<PlayableRecord>, LibraryError> {
        if !self.v2 {
            return Ok(Vec::new());
        }
        self.query(
            &format!(
                "WHERE {GROUP_SQL} = ?1 ORDER BY LOWER(COALESCE(p.title, '')) ASC, p.id ASC"
            ),
            params![group.as_str()],
        )
    }

    fn list_view(&self, view: LibraryKind) -> Result<Vec<PlayableRecord>, LibraryError> {
        if !self.v2 {
            return Ok(Vec::new());
        }
        self.query(
            &format!(
                "WHERE {} ORDER BY LOWER(COALESCE(p.title, '')) ASC, p.id ASC",
                view_cond(view)
            ),
            [],
        )
    }

    fn list_view_tracks(&self, view: LibraryKind) -> Result<Vec<TrackRow>, LibraryError> {
        if !self.v2 {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT i.playable_id, i.position,
                    COALESCE(NULLIF(i.title, ''), NULLIF(p.title, ''), ''),
                    COALESCE(i.artist, p.primary_artist),
                    COALESCE(p.title, ''),
                    (SELECT COUNT(*) FROM playable_items j WHERE j.playable_id = p.id),
                    i.duration_ms, i.available, p.is_orphan,
                    COALESCE(p.noudata_relpath, ''),
                    (p.noubin_key_normalised IS NOT NULL OR EXISTS (
                        SELECT 1 FROM playables lt
                          JOIN playable_items li ON li.playable_id = lt.id
                        WHERE lt.noudata_type = 'linkTrack'
                          AND lt.is_orphan = 0
                          AND lt.refers_to_relpath IS NOT NULL
                          AND lt.refers_to_relpath = p.noudata_relpath
                          AND li.media_relpath = i.media_relpath
                          AND lt.noubin_key_normalised IS NOT NULL))
             FROM playable_items i JOIN playables p ON p.id = i.playable_id
             WHERE {}
             ORDER BY LOWER(COALESCE(NULLIF(i.title, ''), NULLIF(p.title, ''), '')) ASC,
                      i.playable_id ASC, i.position ASC",
            view_cond(view)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            let noudata: String = r.get(9)?;
            // An unacceptable `.noudata` path (hostile index) means the row is not listed.
            let noudata_relpath = if noudata.is_empty() {
                String::new()
            } else {
                match guard::clean_noudata_path(&noudata) {
                    Some(p) => p,
                    None => return Ok(None),
                }
            };
            Ok(Some(TrackRow {
                playable_id: r.get(0)?,
                position: r.get::<_, i64>(1).unwrap_or(0).clamp(0, u32::MAX as i64) as u32,
                title: guard::clean_title(r.get(2)?),
                artist: guard::clean_title_opt(r.get(3)?),
                collection_title: guard::clean_title(r.get(4)?),
                collection_items: r.get::<_, i64>(5)?.clamp(0, u32::MAX as i64) as u32,
                duration_ms: r.get::<_, Option<i64>>(6)?.map(|d| d.max(0) as u64),
                available: r.get::<_, i64>(7)? != 0,
                is_orphan: r.get::<_, i64>(8)? != 0,
                noudata_relpath,
                associated: r.get::<_, i64>(10)? != 0,
            }))
        })?;
        let mut out = Vec::new();
        for row in rows {
            if let Some(track) = row? {
                out.push(track);
            }
        }
        Ok(out)
    }

    fn view_counts(&self) -> Result<Vec<(LibraryKind, u64)>, LibraryError> {
        if !self.v2 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for view in LibraryKind::ALL {
            let n: i64 = if view_is_item_level(view) {
                self.conn.query_row(
                    &format!(
                        "SELECT COUNT(*) FROM playable_items i JOIN playables p \
                         ON p.id = i.playable_id WHERE {}",
                        view_cond(view)
                    ),
                    [],
                    |r| r.get(0),
                )?
            } else {
                self.conn.query_row(
                    &format!("SELECT COUNT(*) FROM playables p WHERE {}", view_cond(view)),
                    [],
                    |r| r.get(0),
                )?
            };
            if n > 0 {
                out.push((view, n as u64));
            }
        }
        Ok(out)
    }

    fn get(&self, id: i64) -> Result<Option<PlayableRecord>, LibraryError> {
        Ok(self
            .query("WHERE p.id = ?1", params![id])?
            .into_iter()
            .next())
    }

    fn find_by_key(&self, key: &str) -> Result<Vec<PlayableRecord>, LibraryError> {
        self.query(
            "WHERE p.noubin_key_normalised = ?1 ORDER BY p.noudata_relpath ASC",
            params![key],
        )
    }

    fn items_of(&self, playable_id: i64) -> Result<Vec<ItemRecord>, LibraryError> {
        if !self.v2 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT playable_id, position, media_relpath, media_relpaths_json, media_title,
                    title, artist, resolved_from, duration_ms, duration_src, available,
                    has_navigable_cues, cue_count
             FROM playable_items WHERE playable_id = ?1 ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![playable_id], |row| {
            let alts: String = row.get(3)?;
            // An unacceptable media path is dropped and the item is unavailable: the engine
            // can never be handed it, whatever the (hostile) index claims.
            let media_relpath: String = row.get(2)?;
            let media_relpath = guard::clean_path(&media_relpath);
            Ok(ItemRecord {
                playable_id: row.get(0)?,
                position: row.get::<_, i64>(1)?.clamp(0, u32::MAX as i64) as u32,
                available: media_relpath.is_some() && row.get::<_, i64>(10)? != 0,
                media_relpath: media_relpath.unwrap_or_default(),
                media_relpaths: guard::clean_path_list(&alts),
                media_title: guard::clean_title_opt(row.get(4)?),
                title: guard::clean_title(row.get(5)?),
                artist: guard::clean_title_opt(row.get(6)?),
                resolved_from: row.get(7)?,
                duration_ms: row.get::<_, Option<i64>>(8)?.map(|d| d.max(0) as u64),
                duration_src: known(row.get(9)?, &["unite", "probed", "failed"]),
                has_navigable_cues: row.get::<_, i64>(11)? != 0,
                cue_count: row.get::<_, i64>(12)?.clamp(0, u32::MAX as i64) as u32,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn item_counts(&self) -> Result<HashMap<i64, u32>, LibraryError> {
        if !self.v2 {
            return Ok(HashMap::new());
        }
        let mut stmt = self
            .conn
            .prepare("SELECT playable_id, COUNT(*) FROM playable_items GROUP BY playable_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u32)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, n) = row?;
            out.insert(id, n);
        }
        Ok(out)
    }

    fn kind_counts(&self) -> Result<Vec<(LibraryKind, u64)>, LibraryError> {
        if !self.v2 {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare("SELECT kind, COUNT(*) FROM playables GROUP BY kind")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (k, n) = row?;
            if let Some(kind) = LibraryKind::parse(&k) {
                out.push((kind, n as u64));
            }
        }
        Ok(out)
    }

    fn group_counts(&self) -> Result<Vec<(LibraryKind, u64)>, LibraryError> {
        if !self.v2 {
            return Ok(Vec::new());
        }
        let sql = format!("SELECT {GROUP_SQL} AS g, COUNT(*) FROM playables p GROUP BY g");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (k, n) = row?;
            if let Some(kind) = LibraryKind::parse(&k) {
                out.push((kind, n as u64));
            }
        }
        Ok(out)
    }

    fn indexed_fingerprint(&self, id: i64) -> Result<Option<(i64, u64)>, LibraryError> {
        if !self.v2 {
            return Ok(None);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT mtime_ns, file_size FROM playables WHERE id = ?1 AND is_orphan = 0",
                params![id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64)),
            )
            .optional()?)
    }

    fn cues_for(
        &self,
        playable_id: i64,
        item_position: u32,
    ) -> Result<Vec<CueEntry>, LibraryError> {
        if !self.v2 {
            return Ok(Vec::new());
        }
        let row: Option<(Option<String>, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT p.noudata_relpath, i.duration_ms
                 FROM playables p LEFT JOIN playable_items i
                   ON i.playable_id = p.id AND i.position = ?2
                 WHERE p.id = ?1",
                params![playable_id, item_position],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((Some(rel), duration)) = row else {
            return Ok(Vec::new());
        };
        let Some(rel) = guard::clean_noudata_path(&rel) else {
            return Ok(Vec::new());
        };
        let bytes = crate::noudata::read_noudata_capped(&self.root.join(&rel))?;
        let doc = parse_noudata(&bytes)?;
        Ok(cue_entries(
            &doc,
            item_position as usize,
            duration.map(|d| d as u64),
        ))
    }
}

impl View<'_> {
    fn covers_of(&self, playable_id: i64) -> Result<Vec<CoverRef>, LibraryError> {
        if !self.v2 {
            return Ok(Vec::new());
        }
        let row: Option<(Option<String>, Option<String>, i64)> = self
            .conn
            .query_row(
                if self.covers {
                    "SELECT noudata_relpath, refers_to_relpath, covers_hidden
                     FROM playables WHERE id = ?1"
                } else {
                    "SELECT noudata_relpath, refers_to_relpath, 0 FROM playables WHERE id = ?1"
                },
                params![playable_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((Some(noudata), refers, hidden)) = row else {
            return Ok(Vec::new());
        };
        if hidden != 0 {
            return Ok(Vec::new());
        }
        let mut declared: Vec<CoverRef> = Vec::new();
        if self.covers {
            let mut stmt = self.conn.prepare(
                "SELECT relpath, source, alt_text, of_noubin FROM playable_covers
                 WHERE playable_id = ?1 ORDER BY position ASC",
            )?;
            let rows = stmt.query_map(params![playable_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    CoverRef {
                        relpath: String::new(),
                        source: CoverSource::parse(&r.get::<_, String>(1)?)
                            .unwrap_or(CoverSource::Local),
                        alt_text: guard::clean_title_opt(r.get(2)?),
                        is_image_of_noubin: r.get::<_, i64>(3)? != 0,
                    },
                ))
            })?;
            for row in rows {
                let (relpath, mut cover) = row?;
                if let Some(relpath) = guard::clean_path(&relpath) {
                    cover.relpath = relpath;
                    declared.push(cover);
                }
            }
        }
        if declared.is_empty() {
            // Nothing declared: a `cover.jpg` next to the `.noudata` (or next to the release a
            // track refers to) is the cover.
            let dir_of = |p: &str| p.rsplit_once('/').map_or(String::new(), |(d, _)| d.to_string());
            let sibling = covers::sibling_relpath(self.root, &dir_of(&noudata)).or_else(|| {
                refers
                    .as_deref()
                    .and_then(|r| covers::sibling_relpath(self.root, &dir_of(r)))
            });
            return Ok(sibling
                .map(|relpath| CoverRef {
                    relpath,
                    source: CoverSource::Sibling,
                    alt_text: None,
                    is_image_of_noubin: false,
                })
                .into_iter()
                .collect());
        }
        declared.retain(|c| crate::confine::is_confined_regular_file(self.root, &c.relpath));
        Ok(declared)
    }

    fn item_covers(
        &self,
        playable_id: i64,
        item_position: u32,
    ) -> Result<Vec<CoverRef>, LibraryError> {
        if !self.v2 {
            return Ok(Vec::new());
        }
        let row: Option<(Option<String>, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT p.noudata_relpath, i.resolved_from
                 FROM playables p LEFT JOIN playable_items i
                   ON i.playable_id = p.id AND i.position = ?2
                 WHERE p.id = ?1",
                params![playable_id, item_position],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((Some(rel), resolved_from)) = row else {
            return Ok(Vec::new());
        };
        let Some(rel) = guard::clean_noudata_path(&rel) else {
            return Ok(Vec::new());
        };
        let full = self.root.join(&rel);
        let doc = parse_noudata(&crate::noudata::read_noudata_capped(&full)?)?;
        // The playlist entry's own release, when the index resolved one.
        let release: Option<(String, NoudataDocument)> = match resolved_from {
            Some(rid) => {
                let rrel: Option<Option<String>> = self
                    .conn
                    .query_row(
                        "SELECT noudata_relpath FROM playables WHERE id = ?1",
                        params![rid],
                        |r| r.get(0),
                    )
                    .optional()?;
                match rrel.flatten().and_then(|r| guard::clean_noudata_path(&r)) {
                    Some(rrel) => crate::noudata::read_noudata_capped(&self.root.join(&rrel))
                        .ok()
                        .and_then(|b| parse_noudata(&b).ok())
                        .map(|d| (rrel, d)),
                    None => None,
                }
            }
            None => None,
        };
        let resolver = fs_release_resolver(&full);
        Ok(covers::effective_item_covers(
            self.root,
            &doc,
            &rel,
            item_position as usize,
            release.as_ref().map(|(relpath, doc)| ReleaseDoc { doc, relpath }),
            &resolver,
        ))
    }

    fn referencing_noudata(&self, image_relpath: &str) -> Result<Vec<String>, LibraryError> {
        if !self.v2 {
            return Ok(Vec::new());
        }
        let mut hits: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        if self.covers {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT p.noudata_relpath FROM playable_covers c
                 JOIN playables p ON p.id = c.playable_id
                 WHERE c.relpath = ?1 COLLATE NOCASE AND p.noudata_relpath IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![image_relpath], |r| r.get::<_, String>(0))?;
            for row in rows {
                hits.insert(row?);
            }
        }
        let all: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT noudata_relpath FROM playables WHERE noudata_relpath IS NOT NULL
                 ORDER BY noudata_relpath",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            // A hostile row naming a path outside the library is not read (and cannot make
            // the answer "unreferenced": it is not a document of this library).
            rows.filter_map(|r| match r {
                Ok(p) => guard::clean_noudata_path(&p).map(Ok),
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<_, _>>()?
        };
        if all.len() > MAX_REFERENCE_SCAN_DOCS {
            return Err(LibraryError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} .noudata files exceed the reference scan limit of {MAX_REFERENCE_SCAN_DOCS}",
                    all.len()
                ),
            )));
        }
        for rel in all {
            if hits.contains(&rel) {
                continue;
            }
            // Fail closed: a document that cannot be read or parsed (a flaky stick) may well
            // reference the image, so the caller must not treat the image as unreferenced.
            let bytes = crate::noudata::read_noudata_capped(&self.root.join(&rel)).map_err(|e| {
                LibraryError::Io(std::io::Error::new(e.kind(), format!("cannot read {rel}: {e}")))
            })?;
            let doc = parse_noudata(&bytes)?;
            if covers::document_references(self.root, &doc, &rel, image_relpath) {
                hits.insert(rel);
            }
        }
        Ok(hits.into_iter().collect())
    }
}

/// Cue entries of one item of a parsed document (shared with callers that already hold the doc).
pub fn cue_entries(
    doc: &NoudataDocument,
    item_position: usize,
    item_length_ms: Option<u64>,
) -> Vec<CueEntry> {
    let Some(item) = doc.media_list.get(item_position) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (li, list) in item.cue_lists.iter().enumerate() {
        let mut list = list.clone();
        if let Some(len) = item_length_ms {
            list.drop_beyond_length(len, "cues");
        }
        for ci in 0..list.cues.len() {
            let cue = &list.cues[ci];
            out.push(CueEntry {
                cue_list_index: li,
                cue_index: ci,
                list_type: list.list_type(),
                list_title: list.cue_list_title.clone(),
                navigable: list.is_navigable(),
                start_ms: cue.start_time.as_millis(),
                end_ms: list.resolved_end_ms(ci, item_length_ms),
                text: cue.text(),
                url: cue.cue_url.clone(),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Connection, schema, migration
// ---------------------------------------------------------------------------

fn validate_library_root(root: &LibraryRoot) -> Result<PathBuf, LibraryError> {
    let path = root.path().to_path_buf();
    if !path.exists() {
        return Err(LibraryError::MissingRoot { path });
    }
    if !path.is_dir() {
        return Err(LibraryError::InvalidRoot { path });
    }
    Ok(path)
}

pub(crate) fn ensure_root_still_present(root: &Path) -> Result<(), LibraryError> {
    if !root.exists() {
        return Err(LibraryError::StorageVanished {
            path: root.to_path_buf(),
            detail: "library root no longer exists".into(),
        });
    }
    Ok(())
}

fn ensure_db_dir_and_path(root: &Path) -> Result<PathBuf, LibraryError> {
    let dir = root.join(PLAYER_DB_DIR);
    // `.plippa` must be a real directory. A symlink (or a plain file) in its place is unlinked
    // (only the link itself, never its target) and replaced; nothing is ever created or
    // deleted *through* it.
    match fs::symlink_metadata(&dir) {
        Ok(m) if m.file_type().is_dir() => {}
        Ok(_) => fs::remove_file(&dir)?,
        Err(_) => {}
    }
    fs::create_dir_all(&dir)?;
    Ok(dir.join(INDEX_DB_FILENAME))
}

fn open_connection(db_path: &Path, create: bool) -> Result<Connection, LibraryError> {
    // NOFOLLOW: the index may not be a symlink (it lives on a stick somebody else prepared).
    let flags = if create {
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_FULL_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
    };
    let conn = Connection::open_with_flags(db_path, flags)?;
    guard::harden(&conn, !create)?;
    conn.busy_timeout(std::time::Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))?;
    // WAL must be set while writable; readers inherit the mode from the file.
    if create {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // PWRSW_01 / USB corruption report: commits must really reach the media, and a
        // successful TRUNCATE checkpoint must actually shrink the `-wal` file.
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT_BYTES)?;
    }
    Ok(conn)
}

/// Open the writable index at `db_path` (create/migrate/WAL), validated. A damaged or foreign
/// index is deleted and rebuilt **under the volume lock** and only after re-checking there that
/// it is still unusable (another process may have rebuilt it while this one waited); on a
/// volume that cannot be locked or written nothing is ever deleted. The caller owns the
/// returned connection.
fn open_writable(
    db_path: &Path,
    deep: bool,
    wait: OpenWait,
) -> Result<Connection, LibraryError> {
    let gate = guard::VolumeGate::enter(db_path, wait)?;
    match open_validated(db_path, deep, &gate) {
        Ok(conn) => {
            gate.finish();
            Ok(conn)
        }
        Err(e) if e.is_unusable_index() && gate.can_delete() => {
            eprintln!(
                "plippa-noubin: library index {} is unusable ({e}); rebuilding it from .noudata",
                db_path.display()
            );
            guard::remove_index_files_in(db_path)?;
            gate.problem_removed();
            let conn = open_validated(db_path, true, &gate)?;
            gate.finish();
            Ok(conn)
        }
        Err(e) => Err(e),
    }
}

/// One validated open attempt while the gate is held.
fn open_validated(
    db_path: &Path,
    deep: bool,
    gate: &guard::VolumeGate,
) -> Result<Connection, LibraryError> {
    if let Some(problem) = gate.problem() {
        return Err(LibraryError::ForeignIndex {
            detail: problem.to_string(),
        });
    }
    let mut conn = open_connection(db_path, true)?;
    // Nothing of ours runs against the file until it contains only our own objects; after
    // migration its structure must equal what this build creates, and it must be intact.
    guard::check_objects(&conn)?;
    migrate(&conn)?;
    guard::check_schema_exact(&conn)?;
    assert_wal(&conn)?;
    if gate.first() || deep {
        guard::quick_check(&conn)?;
        guard::check_types(&conn)?;
    }
    // Rows this build's scanner cannot have written are re-read from their `.noudata`.
    let distrusted = scan::distrust_foreign_rows(&mut conn)?;
    if distrusted > 0 {
        eprintln!(
            "plippa-noubin: {distrusted} index row(s) in {} were not written by this player; \
             they will be re-read from the .noudata files",
            db_path.display()
        );
    }
    Ok(conn)
}

fn read_schema_version(conn: &Connection) -> Option<u32> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| v.trim().parse().ok())
}

const SCHEMA_V2: &str = "
    CREATE TABLE IF NOT EXISTS meta (
        key TEXT PRIMARY KEY NOT NULL,
        value TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS playables (
        id INTEGER PRIMARY KEY NOT NULL,
        noudata_relpath TEXT UNIQUE,              -- NULL for orphan media files
        noubin_key_normalised TEXT,
        title TEXT,
        kind TEXT NOT NULL,                       -- track|album|playlist|mix|audiobook|podcast
        kind_ingest TEXT NOT NULL,                -- kind before cross-file resolution
        release_profile TEXT,                     -- root playableItemProfile (link tracks: the release's)
        noudata_type TEXT NOT NULL DEFAULT 'userPlaylist', -- effective noudataType
        type_declared INTEGER NOT NULL DEFAULT 0, -- 1 = declared type was valid and used
        is_dynamic INTEGER NOT NULL DEFAULT 0,
        is_orphan INTEGER NOT NULL DEFAULT 0,
        primary_artist TEXT,                      -- effective (own, else referred release)
        artist_own TEXT,
        cover_relpath TEXT,                       -- effective
        cover_own TEXT,
        covers_hidden INTEGER NOT NULL DEFAULT 0, -- explicit empty cover list: no inherited art
        refers_to_relpath TEXT,
        duration_ms INTEGER,
        has_navigable_cues INTEGER NOT NULL DEFAULT 0,
        media_relpaths_json TEXT NOT NULL,
        mtime_secs INTEGER NOT NULL,
        mtime_ns INTEGER NOT NULL DEFAULT -1,     -- fingerprint (with file_size); -1 = re-read
        file_size INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_playables_key ON playables(noubin_key_normalised);
    CREATE INDEX IF NOT EXISTS idx_playables_kind ON playables(kind);
    CREATE TABLE IF NOT EXISTS playable_items (
        playable_id INTEGER NOT NULL,
        position INTEGER NOT NULL,
        media_relpath TEXT NOT NULL,
        media_relpaths_json TEXT NOT NULL,
        media_title TEXT,
        title TEXT NOT NULL,
        artist_own TEXT,
        artist TEXT,
        resolved_from INTEGER,
        expected_ms INTEGER,                      -- declared uniteData.expectedLength
        duration_ms INTEGER,
        duration_src TEXT,
        available INTEGER NOT NULL DEFAULT 1,
        has_navigable_cues INTEGER NOT NULL DEFAULT 0,
        cue_count INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (playable_id, position)
    );
    CREATE TABLE IF NOT EXISTS media_claims (
        playable_id INTEGER NOT NULL,
        item_position INTEGER NOT NULL,
        media_relpath TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS playable_covers (
        playable_id INTEGER NOT NULL,
        position INTEGER NOT NULL,
        relpath TEXT NOT NULL,                    -- declared image, library-relative
        source TEXT NOT NULL,                     -- item|local|release (see CoverSource)
        alt_text TEXT,
        of_noubin INTEGER NOT NULL DEFAULT 0,     -- isImageOfNoubin
        inherited INTEGER NOT NULL DEFAULT 0,     -- 1 = copied from the referred release
        PRIMARY KEY (playable_id, position)
    );
    CREATE INDEX IF NOT EXISTS idx_covers_playable ON playable_covers(playable_id);
    CREATE INDEX IF NOT EXISTS idx_covers_relpath ON playable_covers(relpath COLLATE NOCASE);
    CREATE INDEX IF NOT EXISTS idx_claims_media ON media_claims(media_relpath);
    CREATE INDEX IF NOT EXISTS idx_claims_playable ON media_claims(playable_id);
    CREATE TABLE IF NOT EXISTS media_files (
        relpath TEXT PRIMARY KEY NOT NULL,
        dir TEXT NOT NULL,
        size INTEGER NOT NULL,
        mtime_ns INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS noudata_skips (
        relpath TEXT PRIMARY KEY NOT NULL,        -- unplayable / unparsable .noudata fingerprint
        mtime_ns INTEGER NOT NULL,
        size INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS dirs (
        relpath TEXT PRIMARY KEY NOT NULL,        -- '' = library root
        parent TEXT,
        mtime_ns INTEGER NOT NULL                 -- -1 = too recent to trust, relist
    );
    CREATE INDEX IF NOT EXISTS idx_dirs_parent ON dirs(parent);
";

fn set_schema_version(conn: &Connection) -> Result<(), LibraryError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}

fn drop_all(conn: &Connection) -> Result<(), LibraryError> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS playable_items;
         DROP TABLE IF EXISTS playable_covers;
         DROP TABLE IF EXISTS media_claims;
         DROP TABLE IF EXISTS media_files;
         DROP TABLE IF EXISTS dirs;
         DROP TABLE IF EXISTS noudata_skips;
         DROP TABLE IF EXISTS playables;
         DROP TABLE IF EXISTS meta;",
    )?;
    Ok(())
}

/// Bring the DB to schema v2. The index is derived, so anything unrecognised is dropped and
/// rebuilt by the next scan; a v1 index is upgraded in place (ids kept, all rows flagged for
/// re-read) so playback keeps working on the stale data until the scan catches up.
fn migrate(conn: &Connection) -> Result<(), LibraryError> {
    let has_meta: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    let version = if has_meta {
        read_schema_version(conn)
    } else {
        None
    };
    let has_playables: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='playables'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);

    match version {
        Some(v) if v == SCHEMA_VERSION => {
            conn.execute_batch(SCHEMA_V2)?;
        }
        Some(3) if has_playables => {
            // v3 -> v4: add the covers table + flag; every row is re-read by the next scan
            // (which fills `playable_covers`), ids and everything else are kept.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "ALTER TABLE playables ADD COLUMN covers_hidden INTEGER NOT NULL DEFAULT 0;",
            )?;
            tx.execute_batch(SCHEMA_V2)?;
            tx.execute_batch("UPDATE playables SET mtime_ns = -1 WHERE is_orphan = 0;")?;
            set_schema_version(&tx)?;
            tx.commit()?;
        }
        Some(1) if has_playables => {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(
                "DROP INDEX IF EXISTS idx_playables_key;
                 ALTER TABLE playables RENAME TO playables_v1;",
            )?;
            tx.execute_batch(SCHEMA_V2)?;
            tx.execute_batch(
                "INSERT INTO playables
                   (id, noudata_relpath, noubin_key_normalised, title, kind, kind_ingest,
                    media_relpaths_json, mtime_secs, mtime_ns, file_size)
                 SELECT id, noudata_relpath, noubin_key_normalised, title, 'album', 'album',
                        media_relpaths_json, mtime_secs, -1, file_size
                 FROM playables_v1;
                 DROP TABLE playables_v1;",
            )?;
            set_schema_version(&tx)?;
            tx.commit()?;
        }
        None if !has_playables => {
            conn.execute_batch(SCHEMA_V2)?;
            set_schema_version(conn)?;
        }
        _ => {
            // Unknown / newer / broken: derived data, start over.
            let tx = conn.unchecked_transaction()?;
            drop_all(&tx)?;
            tx.execute_batch(SCHEMA_V2)?;
            set_schema_version(&tx)?;
            tx.commit()?;
        }
    }
    Ok(())
}

fn assert_wal(conn: &Connection) -> Result<(), LibraryError> {
    let mode = read_journal_mode(conn)?;
    if mode != "wal" {
        return Err(LibraryError::NotWal { mode });
    }
    Ok(())
}

fn read_journal_mode(conn: &Connection) -> Result<String, LibraryError> {
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    Ok(mode.to_ascii_lowercase())
}

/// Reject a `.noudata` relative path that is not safe, so a write-through can never touch a
/// path outside the library (`..`, absolute, backslash, FAT-reserved names, ...).
fn validate_noudata_rel(root: &Path, rel: &str) -> Result<String, LibraryError> {
    if !crate::is_safe_relative_path(rel) || !rel.ends_with(".noudata") {
        return Err(LibraryError::PathNotInLibrary {
            root: root.to_path_buf(),
            path: root.join(rel),
        });
    }
    Ok(rel.to_string())
}

pub(crate) fn path_relative_posix(root: &Path, full: &Path) -> Result<String, LibraryError> {
    let rel = full
        .strip_prefix(root)
        .map_err(|_| LibraryError::PathNotInLibrary {
            root: root.to_path_buf(),
            path: full.to_path_buf(),
        })?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

pub(crate) fn is_vanished_io(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::BrokenPipe
    ) || {
        // Some platforms report unmounted volumes as other kinds.
        let s = err.to_string().to_ascii_lowercase();
        s.contains("no such file") || s.contains("not a directory")
    }
}

/// Absolute path helpers for tests / callers that need the on-disk DB location.
pub fn index_db_path(root: &LibraryRoot) -> PathBuf {
    root.path().join(PLAYER_DB_DIR).join(INDEX_DB_FILENAME)
}

/// One media file that may need a waveform sidecar (WAVE_01).
///
/// `size` / `mtime_ns` come from the index's `media_files` row; `mtime_ns` is `-1` when the
/// index has no row for the file, and the caller falls back to `fs::metadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaCandidate {
    /// Library-relative media path (POSIX separators, as stored in the index).
    pub relpath: String,
    pub size: u64,
    pub mtime_ns: i64,
}

/// Normalise a media relpath for hashing: forward slashes, no leading `./` or `/`, no
/// duplicate separators, `.`/`..` resolved lexically. Case is preserved (Linux).
pub fn normalise_media_relpath(media_relpath: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in media_relpath.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Stable FNV-1a 64 hash of a media relpath: one file's sidecar name is stable across
/// mounts, profiles and playlists.
pub fn waveform_hash(media_relpath: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in normalise_media_relpath(media_relpath).as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Sidecar path relative to the library root: `.plippa/waveforms/<16 lowercase hex>.wfm`.
pub fn waveform_relpath(media_relpath: &str) -> String {
    format!(
        "{}/{}/{:016x}.wfm",
        PLAYER_DB_DIR,
        WAVEFORM_DIR,
        waveform_hash(media_relpath)
    )
}

/// Absolute sidecar path for a media file of `root`.
pub fn waveform_path(root: &Path, media_relpath: &str) -> PathBuf {
    root.join(waveform_relpath(media_relpath))
}

/// Whether `path` looks like a usable library root (exists and is a directory).
pub fn library_root_available(root: &LibraryRoot) -> Result<(), LibraryError> {
    validate_library_root(root).map(|_| ())
}
