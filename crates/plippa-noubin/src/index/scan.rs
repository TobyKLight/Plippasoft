//! Delta scan, cross-file resolution ("finalize") and duration probe for the v2 index.
//!
//! A scan has three phases so the SQLite write lock is only held briefly:
//!
//! 1. **Walk** (filesystem only): list directories whose mtime changed since the last scan,
//!    reuse the cached listing of the rest; collect `.noudata` and media files.
//! 2. **Read** (filesystem only): stat each known `.noudata`; only files whose
//!    mtime (ns) or size differ are read + parsed. Everything is compared to the DB
//!    read-only.
//! 3. **Apply** (one `BEGIN IMMEDIATE` transaction, skipped entirely when nothing changed):
//!    upsert/delete rows, then [`finalize`] resolves per-track files, playlist entries,
//!    orphans and duration aggregates from data already in the index.

use super::{
    ensure_root_still_present, is_vanished_io, path_relative_posix, LibraryKind, ScanReport,
};
use crate::confine;
use crate::sanitise::{sanitise_key, sanitise_line};
use super::guard;
use crate::covers::{own_covers, Declared};
use crate::error::LibraryError;
use crate::normalise::normalise_noubin_key;
use crate::noudata::{
    classify_type, fs_release_resolver, is_dynamic_playlist, is_empty_user_playlist,
    is_minimally_playable, parse_noudata, read_noudata_capped, NoudataDocument,
};
use crate::noudata_types::{Credits, NoudataType, PlayableItemProfile, UniteData};
use crate::paths::MIN_LIBRARY_SCAN_DEPTH;
use crate::timestamp::NoubinTimestamp;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Extensions listed as media (case-insensitive). Anything else is only tracked when a
/// `.noudata` claims it.
const MEDIA_EXTS: &[&str] = &["mp3", "flac", "wav", "ogg", "opus", "m4a", "m4b", "aac"];

/// A directory whose mtime is newer than this is not trusted as "unchanged" (coarse
/// filesystem timestamps, e.g. FAT's 2 s): it is relisted on the next scan.
const RECENT_NS: i64 = 3_000_000_000;

/// Declared vs measured lengths closer than this are considered equal (no file rewrite).
const DURATION_TOLERANCE_MS: u64 = 500;

/// A document with more media items than this is indexed as "not playable" (`PAR-04`). Far
/// above any realistic playlist (the largest realistic one measured was a few thousand).
pub(crate) const MAX_MEDIA_ITEMS_PER_DOC: usize = 20_000;
/// Same for the total number of cues in one document.
///
/// A cue object is at least two JSON elements (key and separator), so the 200,000-element
/// structural guard on local documents normally fires first; this count is the backstop for
/// documents the guard lets through, and is kept at its own number on purpose.
pub(crate) const MAX_CUES_PER_DOC: usize = 100_000;
/// Alternate `localFilePaths` kept per item (the rest are ignored and counted).
const MAX_ALTS_PER_ITEM: usize = 64;

const RELEASE_KINDS_SQL: &str = "('album','mix','audiobook','podcast')";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

struct MediaFileRow {
    rel: String,
    dir: String,
    size: i64,
    mtime_ns: i64,
}

struct PrevPlayable {
    id: i64,
    mtime_ns: i64,
    size: i64,
}

/// What the DB already knows (loaded read-only before the walk).
#[derive(Default)]
struct Prev {
    dirs: HashMap<String, i64>,
    subdirs: HashMap<String, Vec<String>>,
    noudata_by_dir: HashMap<String, Vec<String>>,
    playables: HashMap<String, PrevPlayable>,
    skips: HashMap<String, (i64, i64)>,
    media: HashMap<String, (i64, i64)>,
    media_by_dir: HashMap<String, Vec<String>>,
}

#[derive(Default)]
struct Walk {
    /// `(relpath, parent, stored mtime_ns)`.
    dirs: Vec<(String, Option<String>, i64)>,
    noudata: Vec<String>,
    media: Vec<MediaFileRow>,
    listed: u32,
}

struct ItemRow {
    alts: Vec<String>,
    /// Chosen alternate (first that exists, else first, else empty).
    primary: String,
    available: bool,
    media_title: Option<String>,
    title: String,
    artist: Option<String>,
    expected_ms: Option<u64>,
    has_nav: bool,
    cue_count: u32,
}

struct IngestRow {
    rel: String,
    mtime_ns: i64,
    size: i64,
    key: Option<String>,
    title: Option<String>,
    kind: LibraryKind,
    noudata_type: NoudataType,
    /// The declared `noudataType` was valid and used (not inferred): the type then does not
    /// depend on whether the referred release is indexed.
    type_declared: bool,
    release_profile: Option<String>,
    is_dynamic: bool,
    artist: Option<String>,
    /// Declared cover images of the document itself, in display order (files not checked).
    covers: Vec<Declared>,
    /// An explicitly empty user-level cover list: inherits nothing.
    covers_hidden: bool,
    refers: Option<String>,
    items: Vec<ItemRow>,
    flat: Vec<String>,
    has_nav: bool,
    /// `localFilePaths` / `referToReleaseNoudata` entries ignored because they are not
    /// acceptable library paths (absolute, `..` past the root, control characters, over the
    /// per-item cap). Reported as a counted warning, never with the hostile text.
    ignored_paths: u32,
}

enum Built {
    Row(Box<IngestRow>),
    /// Valid JSON but no playable content (§3.1).
    NotPlayable {
        mtime_ns: i64,
        size: i64,
        /// Refused for a resource limit (size, structural guard, item or cue count) or because
        /// it is not a plain file, rather than for having no playable content: reported.
        over_limit: bool,
    },
}

/// The counted, sanitised warning for a document refused for a resource limit.
fn over_limit_warning(rel: &str) -> String {
    format!(
        "{}: document exceeds the size / item / cue limits (or is not a plain file); not indexed",
        sanitise_line(rel, 120)
    )
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn rel_join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn rel_dir(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

fn is_media_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MEDIA_EXTS.iter().any(|m| m.eq_ignore_ascii_case(e)))
}

fn file_stem(rel: &str) -> String {
    Path::new(rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel)
        .to_string()
}

fn abs_of(root: &Path, rel: &str) -> std::path::PathBuf {
    root.join(rel)
}

/// Resolve a `localFilePaths`-style path (relative to the folder `parent` of the `.noudata`
/// that names it, `../` allowed while it stays inside the library) to a library-relative
/// POSIX path. `None` when the path is not acceptable or leaves the library root: there is
/// deliberately **no absolute fallback** (`PAR-03`); the caller counts what it ignored.
pub(crate) fn try_resolve_rel(root: &Path, parent: &Path, p: &str) -> Option<String> {
    let base = path_relative_posix(root, parent).ok()?;
    confine::resolve_declared(&base, p)
}

fn kind_for_profile(profile: PlayableItemProfile) -> LibraryKind {
    match profile {
        PlayableItemProfile::Mix => LibraryKind::Mix,
        PlayableItemProfile::Audiobook => LibraryKind::Audiobook,
        PlayableItemProfile::Podcast => LibraryKind::Podcast,
        PlayableItemProfile::Music | PlayableItemProfile::Other => LibraryKind::Album,
    }
}

// ---------------------------------------------------------------------------
// Phase 0: previous state
// ---------------------------------------------------------------------------

/// A relpath read from a tracking table of the (untrusted) index: `""` is the root only where
/// `allow_root`; anything that is not exactly its own normalised, acceptable form is foreign
/// and treated as absent (the scan then rebuilds it from the real tree).
fn tracked_rel_ok(rel: &str, allow_root: bool) -> bool {
    if rel.is_empty() {
        return allow_root;
    }
    guard::clean_path(rel).is_some_and(|c| c == rel)
}

fn load_prev(conn: &Connection) -> Result<Prev, LibraryError> {
    let mut prev = Prev::default();
    {
        let mut stmt = conn.prepare("SELECT relpath, parent, mtime_ns FROM dirs")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (rel, parent, m) = row?;
            // The index is untrusted: a row that names a path outside the library (absolute,
            // `..`, `/`) is dropped, so it can never make the walk leave the root.
            if !tracked_rel_ok(&rel, true)
                || parent.as_deref().is_some_and(|p| !tracked_rel_ok(p, true))
            {
                continue;
            }
            if let Some(p) = parent {
                prev.subdirs.entry(p).or_default().push(rel.clone());
            }
            prev.dirs.insert(rel, m);
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT id, noudata_relpath, mtime_ns, file_size FROM playables
             WHERE noudata_relpath IS NOT NULL ORDER BY noudata_relpath",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (id, rel, m, size) = row?;
            if !tracked_rel_ok(&rel, false) {
                continue;
            }
            prev.noudata_by_dir
                .entry(rel_dir(&rel).to_string())
                .or_default()
                .push(rel.clone());
            prev.playables.insert(
                rel,
                PrevPlayable {
                    id,
                    mtime_ns: m,
                    size,
                },
            );
        }
    }
    {
        let mut stmt = conn.prepare("SELECT relpath, mtime_ns, size FROM noudata_skips")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (rel, m, size) = row?;
            if !tracked_rel_ok(&rel, false) {
                continue;
            }
            // Skipped files are cached per directory too (a cached dir must still list them).
            prev.noudata_by_dir
                .entry(rel_dir(&rel).to_string())
                .or_default()
                .push(rel.clone());
            prev.skips.insert(rel, (m, size));
        }
    }
    {
        let mut stmt = conn.prepare("SELECT relpath, dir, size, mtime_ns FROM media_files")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (rel, dir, size, m) = row?;
            if !tracked_rel_ok(&rel, false) || !tracked_rel_ok(&dir, true) {
                continue;
            }
            prev.media.insert(rel.clone(), (size, m));
            prev.media_by_dir.entry(dir).or_default().push(rel);
        }
    }
    Ok(prev)
}

// ---------------------------------------------------------------------------
// Phase 1: walk
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn walk_dir(
    root: &Path,
    rel: &str,
    parent: Option<&str>,
    depth: u32,
    force: bool,
    now: i64,
    prev: &Prev,
    out: &mut Walk,
) -> Result<(), LibraryError> {
    if !tracked_rel_ok(rel, true) {
        return Ok(()); // defence in depth: never walk outside the library root
    }
    let abs = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let meta = match fs::metadata(&abs) {
        Ok(m) => m,
        Err(e) => {
            // Unplugged stick vs. a directory that vanished / is unreadable.
            ensure_root_still_present(root)?;
            if rel.is_empty() && e.kind() != std::io::ErrorKind::PermissionDenied {
                return Err(LibraryError::Io(e));
            }
            return Ok(());
        }
    };
    let mtime = mtime_ns(&meta);
    let cached = !force && mtime >= 0 && prev.dirs.get(rel) == Some(&mtime);

    let mut subdirs: Vec<String> = Vec::new();
    let stored_mtime;
    if cached {
        subdirs = prev.subdirs.get(rel).cloned().unwrap_or_default();
        if let Some(files) = prev.noudata_by_dir.get(rel) {
            out.noudata.extend(files.iter().cloned());
        }
        if let Some(files) = prev.media_by_dir.get(rel) {
            for f in files {
                if let Some(&(size, m)) = prev.media.get(f) {
                    out.media.push(MediaFileRow {
                        rel: f.clone(),
                        dir: rel.to_string(),
                        size,
                        mtime_ns: m,
                    });
                }
            }
        }
        stored_mtime = mtime;
    } else {
        out.listed += 1;
        let mut entries = match fs::read_dir(&abs) {
            Ok(rd) => rd.flatten().collect::<Vec<_>>(),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Vec::new(),
            Err(e) => {
                ensure_root_still_present(root)?;
                if is_vanished_io(&e) {
                    return Ok(());
                }
                return Err(LibraryError::Io(e));
            }
        };
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                // §4.4: `.*` folders are never scanned; depth ≥ 8 levels below the root.
                if !name.starts_with('.') && depth < MIN_LIBRARY_SCAN_DEPTH {
                    subdirs.push(rel_join(rel, &name));
                }
            } else if ft.is_file() {
                if name.starts_with("._") {
                    continue; // macOS AppleDouble sidecars
                }
                if name.ends_with(".noudata") {
                    out.noudata.push(rel_join(rel, &name));
                } else if !name.starts_with('.') && is_media_name(&name) {
                    if let Ok(m) = entry.metadata() {
                        out.media.push(MediaFileRow {
                            rel: rel_join(rel, &name),
                            dir: rel.to_string(),
                            size: m.len() as i64,
                            mtime_ns: mtime_ns(&m),
                        });
                    }
                }
            }
        }
        stored_mtime = if now - mtime < RECENT_NS { -1 } else { mtime };
    }
    out.dirs
        .push((rel.to_string(), parent.map(str::to_string), stored_mtime));
    for sub in subdirs {
        walk_dir(root, &sub, Some(rel), depth + 1, force, now, prev, out)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 2: read one .noudata
// ---------------------------------------------------------------------------

/// If `noubinKeyNormalised` is missing but `noubinKey` is present, derive via §6.2
/// and write it back to `.noudata` when the file is writable (standard SHOULD).
fn ensure_normalised_key(
    doc: &mut NoudataDocument,
    path: &Path,
    read_rev: Option<crate::playlist::FileRevision>,
) -> Option<String> {
    if let Some(n) = doc.noubin_key_normalised.as_ref() {
        return Some(n.clone());
    }
    let key = doc.noubin_key.as_ref()?;
    let n = normalise_noubin_key(key);
    doc.noubin_key_normalised = Some(n.clone());
    // Best-effort write-back; permission / vanished errors do not fail ingest. Guarded by the
    // revision of the bytes we parsed: if the file changed since (a portal edit, an
    // associate), the write-back is skipped instead of clobbering it; the next scan redoes it.
    if let Some(rev) = read_rev {
        let _ = crate::playlist::write_noudata_atomic(
            doc,
            path,
            crate::playlist::WriteGuard::Unchanged(rev),
        );
    }
    Some(n)
}

fn build_row(
    root: &Path,
    full: &Path,
    rel: &str,
    reads: &mut u32,
    check: &mut dyn FnMut(&str) -> bool,
) -> Result<Built, LibraryError> {
    // Fingerprint taken BEFORE the read: guards the key write-back below against lost updates.
    let read_rev = crate::playlist::FileRevision::of(full).ok();
    let bytes = match read_noudata_capped(full) {
        Ok(b) => b,
        // Oversized (hostile or corrupt): never loaded, indexed as "not playable".
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            let meta = fs::metadata(full)?;
            return Ok(Built::NotPlayable {
                mtime_ns: mtime_ns(&meta),
                size: meta.len() as i64,
                over_limit: true,
            });
        }
        Err(e) if is_vanished_io(&e) => {
            return Err(LibraryError::StorageVanished {
                path: full.to_path_buf(),
                detail: e.to_string(),
            });
        }
        Err(e) => return Err(LibraryError::Io(e)),
    };
    *reads += 1;
    let mut doc = parse_noudata(&bytes)?;

    let dynamic = is_dynamic_playlist(&doc);
    if !is_minimally_playable(&doc) && !dynamic && !is_empty_user_playlist(&doc) {
        let meta = fs::metadata(full)?;
        return Ok(Built::NotPlayable {
            mtime_ns: mtime_ns(&meta),
            size: meta.len() as i64,
            over_limit: false,
        });
    }

    let key = ensure_normalised_key(&mut doc, full, read_rev).and_then(|k| sanitise_key(&k));
    // Fingerprint AFTER a possible write-back, so the next scan sees "unchanged".
    let meta = fs::metadata(full)?;

    let aux_reads = Cell::new(0u32);
    let resolver = fs_release_resolver(full);
    let class = classify_type(&doc, |r| {
        aux_reads.set(aux_reads.get() + 1);
        resolver(r)
    });
    *reads += aux_reads.get();
    let ty = class.ty;
    let kind = match ty {
        NoudataType::Release => kind_for_profile(doc.profile()),
        NoudataType::LooseTrack | NoudataType::LinkTrack => LibraryKind::Track,
        NoudataType::UserPlaylist => LibraryKind::Playlist,
    };
    // The root profile of a release / loose track. A link track inherits it (filled in by
    // `resolve_referring_files`); a playlist has none.
    let release_profile = matches!(ty, NoudataType::Release | NoudataType::LooseTrack)
        .then(|| doc.profile().as_str().to_string());

    let parent = full.parent().unwrap_or_else(|| Path::new("."));
    // Every string stored in the index is already sanitised (control / bidi / zero-width
    // removed, capped): the index is then clean by construction and the read layer's
    // revalidation only ever changes foreign rows.
    let doc_artist = guard::clean_title_opt(doc.primary_artist_display());
    // Resource caps (PAR-04): refuse before any per-item work.
    if doc.media_list.len() > MAX_MEDIA_ITEMS_PER_DOC
        || doc.media_list.iter().map(|i| i.cue_lists.iter().map(|l| l.cues.len()).sum::<usize>())
            .sum::<usize>()
            > MAX_CUES_PER_DOC
    {
        return Ok(Built::NotPlayable {
            mtime_ns: mtime_ns(&meta),
            size: meta.len() as i64,
            over_limit: true,
        });
    }
    let mut ignored_paths = 0u32;
    let mut items = Vec::with_capacity(doc.media_list.len());
    let mut flat: Vec<String> = Vec::new();
    let mut flat_seen: HashSet<String> = HashSet::new();
    for item in &doc.media_list {
        let mut alts: Vec<String> = Vec::new();
        let mut alts_seen: HashSet<String> = HashSet::new();
        for p in item.local_file_paths.iter().flatten() {
            if p.is_empty() {
                continue;
            }
            match try_resolve_rel(root, parent, p) {
                Some(r) if alts.len() < MAX_ALTS_PER_ITEM => {
                    if alts_seen.insert(r.clone()) {
                        alts.push(r);
                    }
                }
                _ => ignored_paths = ignored_paths.saturating_add(1),
            }
        }
        for a in &alts {
            if flat_seen.insert(a.clone()) {
                flat.push(a.clone());
            }
        }
        let primary = alts
            .iter()
            .find(|a| check(a))
            .or_else(|| alts.first())
            .cloned()
            .unwrap_or_default();
        let available = alts.iter().any(|a| check(a));
        // A loose/link track is one item titled by `playableItemTitle` (`mediaTitle` is not
        // recommended there), and so is the only item of a release that has no `mediaTitle`
        // (03 metadata `mediaTitle`: "players display the playableItemTitle").
        let own_title = (matches!(ty, NoudataType::LooseTrack | NoudataType::LinkTrack)
            || (ty == NoudataType::Release && doc.media_list.len() == 1))
            .then(|| doc.playable_item_title.clone())
            .flatten();
        let media_title = guard::clean_title_opt(item.media_title.clone());
        let title = media_title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| {
                guard::clean_title_opt(own_title).filter(|t| !t.trim().is_empty())
            })
            .or_else(|| alts.first().map(|a| file_stem(a)))
            .unwrap_or_default();
        items.push(ItemRow {
            primary,
            available,
            media_title,
            title,
            artist: item
                .item_credits
                .as_ref()
                .and_then(Credits::primary_artist_display)
                .map(guard::clean_title)
                .or_else(|| doc_artist.clone()),
            expected_ms: item
                .unite_data
                .as_ref()
                .and_then(|u| u.expected_length)
                .map(NoubinTimestamp::as_millis),
            has_nav: item.cue_lists.iter().any(|l| l.is_navigable()),
            cue_count: item.cue_lists.iter().map(|l| l.cues.len() as u32).sum(),
            alts,
        });
    }
    let has_nav = items.iter().any(|i| i.has_nav);
    let own = own_covers(root, parent, &doc, ty);

    Ok(Built::Row(Box::new(IngestRow {
        rel: rel.to_string(),
        mtime_ns: mtime_ns(&meta),
        size: meta.len() as i64,
        key,
        title: guard::clean_title_opt(doc.playable_item_title.clone()),
        kind,
        noudata_type: ty,
        type_declared: class.declared,
        release_profile,
        is_dynamic: dynamic,
        artist: doc_artist,
        covers: own.list,
        covers_hidden: own.hidden,
        refers: doc
            .refer_to_release_noudata
            .as_deref()
            .filter(|r| !r.is_empty())
            .and_then(|r| {
                let resolved = try_resolve_rel(root, parent, r);
                if resolved.is_none() {
                    ignored_paths = ignored_paths.saturating_add(1);
                }
                resolved
            }),
        items,
        flat,
        has_nav,
        ignored_paths,
    })))
}

// ---------------------------------------------------------------------------
// The scan
// ---------------------------------------------------------------------------

pub(super) fn scan(
    root: &Path,
    conn: &mut Connection,
    force: bool,
) -> Result<ScanReport, LibraryError> {
    ensure_root_still_present(root)?;
    let prev = load_prev(conn)?;
    let mut report = ScanReport::default();

    let mut walk = Walk::default();
    walk_dir(root, "", None, 0, force, now_ns(), &prev, &mut walk)?;
    walk.noudata.sort();
    walk.noudata.dedup();
    report.dirs_listed = walk.listed;
    report.scanned_files = walk.noudata.len() as u32;

    let media_set: HashSet<&str> = walk.media.iter().map(|m| m.rel.as_str()).collect();
    let mut stat_cache: HashMap<String, bool> = HashMap::new();
    let mut check = |rel: &str| -> bool {
        media_set.contains(rel)
            || *stat_cache
                .entry(rel.to_string())
                .or_insert_with(|| confine::is_confined_regular_file(root, rel))
    };

    // Phase 2: which .noudata changed?
    let mut seen: HashSet<&str> = HashSet::new();
    let mut ingests: Vec<IngestRow> = Vec::new();
    let mut skip_upserts: Vec<(String, i64, i64)> = Vec::new();
    let mut drop_ids: Vec<i64> = Vec::new(); // now-unplayable rows (not counted as removed)
    for rel in &walk.noudata {
        let full = abs_of(root, rel);
        let meta = match fs::metadata(&full) {
            Ok(m) => m,
            Err(e) => {
                ensure_root_still_present(root)?;
                if e.kind() != std::io::ErrorKind::NotFound {
                    report.skipped += 1;
                    report.warnings.push(sanitise_line(&format!("{rel}: {e}"), 400));
                    seen.insert(rel);
                }
                continue; // NotFound: deleted since listing → treated as removed
            }
        };
        seen.insert(rel);
        let (m, size) = (mtime_ns(&meta), meta.len() as i64);
        if !force {
            if prev
                .playables
                .get(rel)
                .is_some_and(|p| p.mtime_ns == m && p.size == size)
            {
                continue;
            }
            if prev.skips.get(rel) == Some(&(m, size)) {
                continue;
            }
        }
        match build_row(root, &full, rel, &mut report.noudata_reads, &mut check) {
            Ok(Built::Row(row)) => {
                if row.ignored_paths > 0 {
                    report.warnings.push(format!(
                        "{}: {} file path(s) outside the library ignored",
                        sanitise_line(rel, 120),
                        row.ignored_paths
                    ));
                }
                ingests.push(*row)
            }
            Ok(Built::NotPlayable { mtime_ns, size, over_limit }) => {
                if over_limit {
                    report.warnings.push(over_limit_warning(rel));
                }
                if let Some(p) = prev.playables.get(rel) {
                    drop_ids.push(p.id);
                }
                skip_upserts.push((rel.clone(), mtime_ns, size));
                report.skipped += 1;
            }
            Err(LibraryError::StorageVanished { path, .. }) => {
                ensure_root_still_present(root)?;
                if path.exists() {
                    report.skipped += 1;
                } // else: file vanished between stat and read → removed next scan
            }
            Err(e @ LibraryError::Noudata(_)) => {
                // Broken JSON: warn once, remember the fingerprint, keep any stale row.
                skip_upserts.push((rel.clone(), m, size));
                report.skipped += 1;
                report.warnings.push(sanitise_line(&format!("{rel}: {e}"), 400));
            }
            Err(e) => {
                report.skipped += 1;
                report.warnings.push(sanitise_line(&format!("{rel}: {e}"), 400));
            }
        }
    }
    ingests.sort_by(|a, b| a.rel.cmp(&b.rel));

    let removed_ids: Vec<i64> = prev
        .playables
        .iter()
        .filter(|(rel, _)| !seen.contains(rel.as_str()))
        .map(|(_, p)| p.id)
        .collect();
    let gone_skips: Vec<&String> = prev
        .skips
        .keys()
        .filter(|rel| !seen.contains(rel.as_str()))
        .collect();

    // Media file delta.
    let new_media_keys: HashSet<&str> = walk.media.iter().map(|m| m.rel.as_str()).collect();
    let media_upserts: Vec<&MediaFileRow> = walk
        .media
        .iter()
        .filter(|m| prev.media.get(&m.rel) != Some(&(m.size, m.mtime_ns)))
        .collect();
    let media_changed_in_place: Vec<&str> = media_upserts
        .iter()
        .filter(|m| prev.media.contains_key(&m.rel))
        .map(|m| m.rel.as_str())
        .collect();
    let media_gone: Vec<&String> = prev
        .media
        .keys()
        .filter(|r| !new_media_keys.contains(r.as_str()))
        .collect();

    let dirs_changed = walk.dirs.len() != prev.dirs.len()
        || walk
            .dirs
            .iter()
            .any(|(rel, _, m)| prev.dirs.get(rel) != Some(m));

    // Availability of items of playables that are NOT being re-read.
    let reingest_ids: HashSet<i64> = ingests
        .iter()
        .filter_map(|r| prev.playables.get(&r.rel).map(|p| p.id))
        .chain(removed_ids.iter().copied())
        .chain(drop_ids.iter().copied())
        .collect();
    struct AvailUpdate {
        pid: i64,
        pos: i64,
        primary: String,
        available: bool,
        flipped: bool,
    }
    let mut avail_updates: Vec<AvailUpdate> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT i.playable_id, i.position, i.media_relpaths_json, i.media_relpath, i.available
             FROM playable_items i JOIN playables p ON p.id = i.playable_id
             WHERE p.is_orphan = 0",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)? != 0,
            ))
        })?;
        for row in rows {
            let (pid, pos, alts_json, primary, avail) = row?;
            if reingest_ids.contains(&pid) {
                continue;
            }
            let alts: Vec<String> = serde_json::from_str(&alts_json).unwrap_or_default();
            let now_avail = alts.iter().any(|a| check(a));
            let new_primary = alts
                .iter()
                .find(|a| check(a))
                .or_else(|| alts.first())
                .cloned()
                .unwrap_or_default();
            if now_avail != avail || new_primary != primary {
                avail_updates.push(AvailUpdate {
                    pid,
                    pos,
                    primary: new_primary,
                    available: now_avail,
                    flipped: now_avail != avail,
                });
            }
        }
    }
    report.availability_changed = avail_updates.iter().filter(|u| u.flipped).count() as u32;

    let structural = force
        || !ingests.is_empty()
        || !removed_ids.is_empty()
        || !drop_ids.is_empty()
        || !media_upserts.is_empty()
        || !media_gone.is_empty();
    let anything = structural
        || dirs_changed
        || !avail_updates.is_empty()
        || !skip_upserts.is_empty()
        || !gone_skips.is_empty();
    if !anything {
        return Ok(report);
    }

    // Phase 3: apply.
    let tx = Transaction::new(conn, TransactionBehavior::Immediate)?;
    for id in removed_ids.iter().chain(drop_ids.iter()) {
        delete_playable(&tx, *id)?;
    }
    report.removed = removed_ids.len() as u32;

    for row in &ingests {
        let existing = prev.playables.get(&row.rel).map(|p| p.id);
        upsert_row(&tx, row, existing)?;
        if existing.is_some() {
            report.updated += 1;
        } else {
            report.ingested += 1;
        }
        tx.execute(
            "DELETE FROM noudata_skips WHERE relpath = ?1",
            params![row.rel],
        )?;
    }
    for rel in gone_skips {
        tx.execute("DELETE FROM noudata_skips WHERE relpath = ?1", params![rel])?;
    }
    for (rel, m, size) in &skip_upserts {
        tx.execute(
            "INSERT INTO noudata_skips (relpath, mtime_ns, size) VALUES (?1, ?2, ?3)
             ON CONFLICT(relpath) DO UPDATE SET mtime_ns = excluded.mtime_ns, size = excluded.size",
            params![rel, m, size],
        )?;
    }

    for rel in media_gone {
        tx.execute("DELETE FROM media_files WHERE relpath = ?1", params![rel])?;
    }
    for m in &media_upserts {
        tx.execute(
            "INSERT INTO media_files (relpath, dir, size, mtime_ns) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(relpath) DO UPDATE SET dir = excluded.dir, size = excluded.size,
                                                mtime_ns = excluded.mtime_ns",
            params![m.rel, m.dir, m.size, m.mtime_ns],
        )?;
    }
    // A media file replaced in place: measure it again.
    for rel in media_changed_in_place {
        tx.execute(
            "UPDATE playable_items SET duration_src = CASE WHEN expected_ms IS NULL THEN NULL ELSE 'unite' END,
                    duration_ms = expected_ms
             WHERE duration_src IN ('probed','failed')
               AND playable_id IN (SELECT playable_id FROM media_claims WHERE media_relpath = ?1)",
            params![rel],
        )?;
    }
    if dirs_changed {
        tx.execute("DELETE FROM dirs", [])?;
        for (rel, parent, m) in &walk.dirs {
            tx.execute(
                "INSERT INTO dirs (relpath, parent, mtime_ns) VALUES (?1, ?2, ?3)",
                params![rel, parent, m],
            )?;
        }
    }
    for u in &avail_updates {
        tx.execute(
            "UPDATE playable_items SET available = ?3, media_relpath = ?4
             WHERE playable_id = ?1 AND position = ?2",
            params![u.pid, u.pos, u.available as i64, u.primary],
        )?;
    }
    if structural {
        finalize(&tx, root, &mut report)?;
    }
    tx.commit()?;
    Ok(report)
}

/// Index exactly one `.noudata` (library-relative) without consulting directory mtimes.
///
/// Write-through for a file the service just created or rewrote: [`scan`]'s cached walk trusts
/// a stored directory mtime, and on FAT that timestamp only moves in 2 s steps, so the new
/// name may never enter the walk (the reported "blank playlist missing until reboot" bug). A
/// missing file is treated as a removal. On error nothing is written.
pub(super) fn ingest_noudata(
    root: &Path,
    conn: &mut Connection,
    rel: &str,
) -> Result<ScanReport, LibraryError> {
    ensure_root_still_present(root)?;
    let full = abs_of(root, rel);
    match fs::metadata(&full) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return forget_noudata(root, conn, rel);
        }
        Err(e) => {
            ensure_root_still_present(root)?;
            return Err(LibraryError::Io(e));
        }
    }

    let mut report = ScanReport {
        scanned_files: 1,
        ..ScanReport::default()
    };
    let mut stat_cache: HashMap<String, bool> = HashMap::new();
    let mut check = |r: &str| -> bool {
        *stat_cache
            .entry(r.to_string())
            .or_insert_with(|| confine::is_confined_regular_file(root, r))
    };
    let built = build_row(root, &full, rel, &mut report.noudata_reads, &mut check)?;
    let existing = playable_id_for(conn, rel)?;

    let tx = Transaction::new(conn, TransactionBehavior::Immediate)?;
    match built {
        Built::Row(row) => {
            if row.ignored_paths > 0 {
                report.warnings.push(format!(
                    "{}: {} file path(s) outside the library ignored",
                    sanitise_line(rel, 120),
                    row.ignored_paths
                ));
            }
            upsert_row(&tx, &row, existing)?;
            tx.execute(
                "DELETE FROM noudata_skips WHERE relpath = ?1",
                params![rel],
            )?;
            if existing.is_some() {
                report.updated = 1;
            } else {
                report.ingested = 1;
            }
        }
        Built::NotPlayable { mtime_ns, size, over_limit } => {
            if over_limit {
                report.warnings.push(over_limit_warning(rel));
            }
            // The service wrote a file with no playable content: mirror the scan (skip entry),
            // but count the dropped row as removed so clients are notified.
            if let Some(id) = existing {
                delete_playable(&tx, id)?;
                report.removed = 1;
            }
            tx.execute(
                "INSERT INTO noudata_skips (relpath, mtime_ns, size) VALUES (?1, ?2, ?3)
                 ON CONFLICT(relpath) DO UPDATE SET mtime_ns = excluded.mtime_ns,
                                                    size = excluded.size",
                params![rel, mtime_ns, size],
            )?;
            report.skipped = 1;
        }
    }
    // A file in a directory the cached walk has not listed yet (first playlist in a fresh
    // `Playlists/`) must not be deleted as "vanished" by the next lazy scan: record the
    // directory chain as "relist" (mtime -1), exactly what a listing would have stored.
    touch_dirs_for(&tx, rel)?;
    finalize(&tx, root, &mut report)?;
    tx.commit()?;
    Ok(report)
}

/// Remove one `.noudata` (library-relative) from the index and its skip entry. No-op when
/// absent. `finalize` un-claims its media (orphans reappear) and re-aggregates durations.
pub(super) fn forget_noudata(
    root: &Path,
    conn: &mut Connection,
    rel: &str,
) -> Result<ScanReport, LibraryError> {
    ensure_root_still_present(root)?;
    let mut report = ScanReport::default();
    let existing = playable_id_for(conn, rel)?;
    let skipped = conn
        .query_row(
            "SELECT 1 FROM noudata_skips WHERE relpath = ?1",
            params![rel],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    if existing.is_none() && !skipped {
        return Ok(report);
    }
    let tx = Transaction::new(conn, TransactionBehavior::Immediate)?;
    tx.execute(
        "DELETE FROM noudata_skips WHERE relpath = ?1",
        params![rel],
    )?;
    if let Some(id) = existing {
        delete_playable(&tx, id)?;
        report.removed = 1;
        finalize(&tx, root, &mut report)?;
    }
    tx.commit()?;
    Ok(report)
}

fn playable_id_for(conn: &Connection, rel: &str) -> Result<Option<i64>, LibraryError> {
    Ok(conn
        .query_row(
            "SELECT id FROM playables WHERE noudata_relpath = ?1",
            params![rel],
            |r| r.get(0),
        )
        .optional()?)
}

/// Insert `rel`'s directory and any missing ancestors into `dirs` with the "relist" marker
/// `-1`, so a later walk that has not seen them yet lists them. Existing rows are left
/// untouched (`ON CONFLICT DO NOTHING`) — including their `mtime_ns`. Single-path ingest does
/// not rely on directory mtimes anyway: it upserts the one row directly.
fn touch_dirs_for(conn: &Connection, rel: &str) -> Result<(), LibraryError> {
    let mut dir = rel_dir(rel).to_string();
    while !dir.is_empty() {
        let parent = rel_dir(&dir).to_string();
        conn.execute(
            "INSERT INTO dirs (relpath, parent, mtime_ns) VALUES (?1, ?2, -1)
             ON CONFLICT(relpath) DO NOTHING",
            params![dir, parent],
        )?;
        dir = parent;
    }
    Ok(())
}

fn delete_playable(conn: &Connection, id: i64) -> Result<(), LibraryError> {
    conn.execute(
        "DELETE FROM playable_items WHERE playable_id = ?1",
        params![id],
    )?;
    conn.execute(
        "DELETE FROM media_claims WHERE playable_id = ?1",
        params![id],
    )?;
    conn.execute(
        "DELETE FROM playable_covers WHERE playable_id = ?1",
        params![id],
    )?;
    conn.execute("DELETE FROM playables WHERE id = ?1", params![id])?;
    Ok(())
}

fn upsert_row(
    conn: &Connection,
    row: &IngestRow,
    existing: Option<i64>,
) -> Result<(), LibraryError> {
    let media_json = serde_json::to_string(&row.flat)?;
    let primary_cover = row.covers.first().map(|c| c.relpath.as_str());
    let mtime_secs = row.mtime_ns.div_euclid(1_000_000_000);
    let pid = match existing {
        Some(id) => {
            conn.execute(
                "UPDATE playables SET noubin_key_normalised = ?1, title = ?2, kind = ?3,
                   kind_ingest = ?3, release_profile = ?4, is_dynamic = ?5, is_orphan = 0,
                   primary_artist = ?6, artist_own = ?6, cover_relpath = ?7, cover_own = ?7,
                   refers_to_relpath = ?8, has_navigable_cues = ?9, media_relpaths_json = ?10,
                   mtime_secs = ?11, mtime_ns = ?12, file_size = ?13,
                   noudata_type = ?15, type_declared = ?16, covers_hidden = ?17
                 WHERE id = ?14",
                params![
                    row.key,
                    row.title,
                    row.kind.as_str(),
                    row.release_profile,
                    row.is_dynamic as i64,
                    row.artist,
                    primary_cover,
                    row.refers,
                    row.has_nav as i64,
                    media_json,
                    mtime_secs,
                    row.mtime_ns,
                    row.size,
                    id,
                    row.noudata_type.as_str(),
                    row.type_declared as i64,
                    row.covers_hidden as i64
                ],
            )?;
            id
        }
        None => {
            conn.execute(
                "INSERT INTO playables
                   (noudata_relpath, noubin_key_normalised, title, kind, kind_ingest,
                    release_profile, is_dynamic, is_orphan, primary_artist, artist_own,
                    cover_relpath, cover_own, refers_to_relpath, has_navigable_cues,
                    media_relpaths_json, mtime_secs, mtime_ns, file_size,
                    noudata_type, type_declared, covers_hidden)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6, 0, ?7, ?7, ?8, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                         ?15, ?16, ?17)",
                params![
                    row.rel,
                    row.key,
                    row.title,
                    row.kind.as_str(),
                    row.release_profile,
                    row.is_dynamic as i64,
                    row.artist,
                    primary_cover,
                    row.refers,
                    row.has_nav as i64,
                    media_json,
                    mtime_secs,
                    row.mtime_ns,
                    row.size,
                    row.noudata_type.as_str(),
                    row.type_declared as i64,
                    row.covers_hidden as i64
                ],
            )?;
            conn.last_insert_rowid()
        }
    };

    conn.execute(
        "DELETE FROM playable_covers WHERE playable_id = ?1",
        params![pid],
    )?;
    for (pos, c) in row.covers.iter().enumerate() {
        conn.execute(
            "INSERT INTO playable_covers
               (playable_id, position, relpath, source, alt_text, of_noubin, inherited)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            params![
                pid,
                pos as i64,
                c.relpath,
                c.source.as_str(),
                c.alt_text,
                c.is_image_of_noubin as i64
            ],
        )?;
    }

    // Previously measured lengths survive a re-read when the item still points at the
    // same media and the file's own claim agrees. Keyed by the item's media paths — not its
    // position — so reordering a release/playlist keeps the probed (or failed) durations.
    // Several entries may share a media path (duplicates); they share one measurement.
    let mut old: HashMap<String, (Option<i64>, Option<String>)> = HashMap::new();
    if existing.is_some() {
        let mut stmt = conn.prepare(
            "SELECT media_relpaths_json, duration_ms, duration_src
             FROM playable_items WHERE playable_id = ?1",
        )?;
        let rows = stmt.query_map(params![pid], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?, r.get::<_, Option<String>>(2)?))
        })?;
        for r in rows {
            let (media, duration, src) = r?;
            old.insert(media, (duration, src));
        }
    }
    conn.execute(
        "DELETE FROM playable_items WHERE playable_id = ?1",
        params![pid],
    )?;
    conn.execute(
        "DELETE FROM media_claims WHERE playable_id = ?1",
        params![pid],
    )?;

    for (pos, item) in row.items.iter().enumerate() {
        let alts_json = serde_json::to_string(&item.alts)?;
        let (duration, src): (Option<i64>, Option<&str>) = match old.get(&alts_json) {
            Some((Some(d), Some(s)))
                if (s == "probed" || s == "failed")
                    && item
                        .expected_ms
                        .is_none_or(|e| e.abs_diff(*d as u64) <= DURATION_TOLERANCE_MS) =>
            {
                (
                    Some(*d),
                    Some(if s == "probed" { "probed" } else { "failed" }),
                )
            }
            Some((d, Some(s))) if s == "failed" && d.is_none() => {
                (item.expected_ms.map(|e| e as i64), Some("failed"))
            }
            _ => match item.expected_ms {
                Some(e) => (Some(e as i64), Some("unite")),
                None => (None, None),
            },
        };
        conn.execute(
            "INSERT INTO playable_items
               (playable_id, position, media_relpath, media_relpaths_json, media_title, title,
                artist_own, artist, resolved_from, expected_ms, duration_ms, duration_src,
                available, has_navigable_cues, cue_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, NULL, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                pid,
                pos as i64,
                item.primary,
                alts_json,
                item.media_title,
                item.title,
                item.artist,
                item.expected_ms.map(|e| e as i64),
                duration,
                src,
                item.available as i64,
                item.has_nav as i64,
                item.cue_count as i64
            ],
        )?;
        for alt in &item.alts {
            conn.execute(
                "INSERT INTO media_claims (playable_id, item_position, media_relpath)
                 VALUES (?1, ?2, ?3)",
                params![pid, pos as i64, alt],
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Finalize: everything that depends on more than one file
// ---------------------------------------------------------------------------

/// Resolve cross-file data from what is already in the index (no `.noudata` is read):
/// orphan media, per-track files → their release (§4.9), playlist entries → authoritative
/// release metadata, duration aggregates, and the index warnings.
pub(super) fn finalize(
    conn: &Connection,
    root: &Path,
    report: &mut ScanReport,
) -> Result<(), LibraryError> {
    resolve_referring_files(conn, root)?;
    sync_orphans(conn, report)?;
    resolve_playlist_items(conn)?;
    conn.execute_batch(
        "UPDATE playables SET duration_ms = CASE
            WHEN EXISTS (SELECT 1 FROM playable_items i WHERE i.playable_id = playables.id)
             AND NOT EXISTS (SELECT 1 FROM playable_items i
                             WHERE i.playable_id = playables.id AND i.duration_ms IS NULL)
            THEN (SELECT SUM(i.duration_ms) FROM playable_items i WHERE i.playable_id = playables.id)
            ELSE NULL END;",
    )?;
    report.warnings.extend(library_warnings(conn)?);
    Ok(())
}

fn sync_orphans(conn: &Connection, report: &mut ScanReport) -> Result<(), LibraryError> {
    // Media referenced by *any* `.noudata` (playlists included) has a `.noudata`: not an orphan.
    let claimed: HashSet<String> = {
        let mut stmt = conn.prepare("SELECT DISTINCT media_relpath FROM media_claims")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let files: Vec<(String, i64, i64)> = {
        let mut stmt =
            conn.prepare("SELECT relpath, size, mtime_ns FROM media_files ORDER BY relpath")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let file_set: HashSet<&str> = files.iter().map(|f| f.0.as_str()).collect();
    let existing: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT p.id, i.media_relpath FROM playables p
             JOIN playable_items i ON i.playable_id = p.id AND i.position = 0
             WHERE p.is_orphan = 1",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut have: HashSet<String> = HashSet::new();
    for (id, rel) in existing {
        if !file_set.contains(rel.as_str()) || claimed.contains(&rel) {
            delete_playable(conn, id)?;
            report.orphans_removed += 1;
        } else {
            have.insert(rel);
        }
    }
    for (rel, size, m) in &files {
        if claimed.contains(rel) || have.contains(rel) {
            continue;
        }
        let stem = file_stem(rel);
        let json = serde_json::to_string(&[rel])?;
        conn.execute(
            "INSERT INTO playables
               (noudata_relpath, title, kind, kind_ingest, is_orphan, media_relpaths_json,
                mtime_secs, mtime_ns, file_size)
             VALUES (NULL, ?1, 'track', 'track', 1, ?2, ?3, ?4, ?5)",
            params![stem, json, m.div_euclid(1_000_000_000), m, size],
        )?;
        let pid = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO playable_items
               (playable_id, position, media_relpath, media_relpaths_json, title, available)
             VALUES (?1, 0, ?2, ?3, ?4, 1)",
            params![pid, rel, json, stem],
        )?;
        report.orphans_added += 1;
    }
    Ok(())
}

/// A `linkTrack` is a *track* that shows its release's artist/cover/profile; an *inferred*
/// type (no valid declared `noudataType`, §4.9) is re-checked here against the index: a
/// file without `releaseData` whose `referToReleaseNoudata` resolves to a release is a track,
/// else a playlist. Uses the index (target row's kind) when the target is indexed; otherwise
/// the classification made at read time (which read the target directly). A *declared*
/// linkTrack stays a track even if its release is missing (it just shows no release data).
type TargetRow = (String, Option<String>, Option<String>, Option<String>);

fn resolve_referring_files(conn: &Connection, root: &Path) -> Result<(), LibraryError> {
    struct Ref {
        id: i64,
        refers: String,
        kind_ingest: String,
        artist_own: Option<String>,
        cover_own: Option<String>,
        hidden: bool,
        dynamic: bool,
        declared_link: bool,
    }
    let refs: Vec<Ref> = {
        let mut stmt = conn.prepare(
            "SELECT id, refers_to_relpath, kind_ingest, artist_own, cover_own, is_dynamic,
                    (type_declared = 1 AND noudata_type = 'linkTrack'), covers_hidden
             FROM playables
             WHERE refers_to_relpath IS NOT NULL AND is_orphan = 0
               AND ((noudata_type = 'linkTrack')
                    OR (noudata_type = 'userPlaylist' AND type_declared = 0))",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Ref {
                id: r.get(0)?,
                refers: r.get(1)?,
                kind_ingest: r.get(2)?,
                artist_own: r.get(3)?,
                cover_own: r.get(4)?,
                dynamic: r.get::<_, i64>(5)? != 0,
                declared_link: r.get::<_, i64>(6)? != 0,
                hidden: r.get::<_, i64>(7)? != 0,
            })
        })?;
        rows.collect::<Result<_, _>>()?
    };
    for r in refs {
        // Art copied from a release last time is recomputed from the release as it is now.
        conn.execute(
            "DELETE FROM playable_covers WHERE playable_id = ?1 AND inherited = 1",
            params![r.id],
        )?;
        let target: Option<TargetRow> = conn
            .query_row(
                "SELECT kind, primary_artist, cover_relpath, release_profile FROM playables
                 WHERE noudata_relpath = ?1",
                params![r.refers],
                |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?)),
            )
            .optional()?;
        let has_own_cover = r.cover_own.is_some();
        let release_target = target
            .as_ref()
            .filter(|(k, ..)| LibraryKind::parse(k).is_some_and(LibraryKind::is_release));
        let (kind, artist, cover, profile) = if let Some((_, a, c, p)) = release_target {
            (
                if r.dynamic {
                    LibraryKind::Playlist
                } else {
                    LibraryKind::Track
                },
                r.artist_own.or(a.clone()),
                if r.hidden {
                    None
                } else {
                    r.cover_own.clone().or(c.clone())
                },
                p.clone(),
            )
        } else if r.declared_link {
            (LibraryKind::Track, r.artist_own, r.cover_own, None)
        } else if target.is_some() || !confine::is_confined_regular_file(root, &r.refers) {
            // Target indexed as a non-release, or gone from disk ⇒ playlist.
            (LibraryKind::Playlist, r.artist_own, r.cover_own, None)
        } else {
            // Present but not indexed (e.g. a release without media) ⇒ what reading it
            // decided at ingest.
            (
                LibraryKind::parse(&r.kind_ingest).unwrap_or(LibraryKind::Playlist),
                r.artist_own,
                r.cover_own,
                None,
            )
        };
        conn.execute(
            "UPDATE playables SET kind = ?2, primary_artist = ?3, cover_relpath = ?4,
                    release_profile = ?5
             WHERE id = ?1",
            params![r.id, kind.as_str(), artist, cover, profile],
        )?;
        // The release's whole cover list is shown unless this file has (or hides) its own.
        if release_target.is_some() && !r.hidden && !has_own_cover {
            conn.execute(
                "INSERT INTO playable_covers
                   (playable_id, position, relpath, source, alt_text, of_noubin, inherited)
                 SELECT ?1, c.position, c.relpath, c.source, c.alt_text, c.of_noubin, 1
                 FROM playable_covers c JOIN playables t ON t.id = c.playable_id
                 WHERE t.noudata_relpath = ?2",
                params![r.id, r.refers],
            )?;
        }
    }
    Ok(())
}

/// Playlist entries take title/artist/length from the authoritative release (spec: the
/// release's `.noudata` wins).
fn resolve_playlist_items(conn: &Connection) -> Result<(), LibraryError> {
    let playlists: Vec<i64> = {
        let mut stmt =
            conn.prepare("SELECT id FROM playables WHERE kind = 'playlist' AND is_orphan = 0")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    type ItemRow2 = (
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    );
    for pid in playlists {
        let items: Vec<ItemRow2> = {
            let mut stmt = conn.prepare(
                "SELECT position, media_relpaths_json, media_title, artist_own, expected_ms,
                        title, artist, resolved_from, duration_ms, duration_src
                 FROM playable_items WHERE playable_id = ?1",
            )?;
            let rows = stmt.query_map(params![pid], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        let mut claim_cache: HashMap<String, Option<(i64, i64)>> = HashMap::new();
        for (
            pos,
            alts_json,
            media_title,
            artist_own,
            expected,
            cur_title,
            cur_artist,
            cur_from,
            cur_dur,
            cur_src,
        ) in items
        {
            let alts: Vec<String> = serde_json::from_str(&alts_json).unwrap_or_default();
            // (source playable, source item position)
            let mut source: Option<(i64, i64)> = None;
            for alt in &alts {
                // One lookup per distinct media path and playlist: a playlist that lists the
                // same file thousands of times must not scan that file's claims each time.
                let found = match claim_cache.get(alt) {
                    Some(hit) => *hit,
                    None => {
                        let hit = conn
                            .query_row(
                                &format!(
                                    "SELECT c.playable_id, c.item_position FROM media_claims c
                                     JOIN playables p ON p.id = c.playable_id
                                     WHERE c.media_relpath = ?1 AND c.playable_id != ?2
                                       AND p.kind != 'playlist'
                                     ORDER BY CASE WHEN p.kind IN {RELEASE_KINDS_SQL} THEN 0 ELSE 1 END,
                                              p.noudata_relpath
                                     LIMIT 1"
                                ),
                                params![alt, pid],
                                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
                            )
                            .optional()?;
                        claim_cache.insert(alt.clone(), hit);
                        hit
                    }
                };
                source = found;
                if source.is_some() {
                    break;
                }
            }
            let own_title = media_title
                .clone()
                .filter(|t| !t.trim().is_empty())
                .or_else(|| alts.first().map(|a| file_stem(a)))
                .unwrap_or_default();
            let (title, artist, from, dur, src) = match source {
                Some((sp, spos)) => {
                    let s: (String, Option<String>, Option<i64>, Option<String>) = conn.query_row(
                        "SELECT title, artist, duration_ms, duration_src FROM playable_items
                         WHERE playable_id = ?1 AND position = ?2",
                        params![sp, spos],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )?;
                    let dur = s.2.or(expected);
                    let src = if s.2.is_some() {
                        s.3
                    } else {
                        expected.map(|_| "unite".to_string())
                    };
                    (s.0, s.1.or(artist_own.clone()), Some(sp), dur, src)
                }
                // Unresolved entry: it is its own source, so a measured length stays.
                None if cur_from.is_none()
                    && matches!(cur_src.as_deref(), Some("probed") | Some("failed")) =>
                {
                    (
                        own_title,
                        artist_own.clone(),
                        None,
                        cur_dur,
                        cur_src.clone(),
                    )
                }
                None => (
                    own_title,
                    artist_own.clone(),
                    None,
                    expected,
                    expected.map(|_| "unite".to_string()),
                ),
            };
            if title != cur_title
                || artist != cur_artist
                || from != cur_from
                || dur != cur_dur
                || src != cur_src
            {
                conn.execute(
                    "UPDATE playable_items SET title = ?3, artist = ?4, resolved_from = ?5,
                            duration_ms = ?6, duration_src = ?7
                     WHERE playable_id = ?1 AND position = ?2",
                    params![pid, pos, title, artist, from, dur, src],
                )?;
            }
        }
    }
    Ok(())
}

/// Duplicate `noubinKeyNormalised` and media claimed by more than one release `.noudata`.
pub(super) fn library_warnings(conn: &Connection) -> Result<Vec<String>, LibraryError> {
    let mut out = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT noubin_key_normalised, noudata_relpath FROM playables
             WHERE noubin_key_normalised IN (
                 SELECT noubin_key_normalised FROM playables
                 WHERE noubin_key_normalised IS NOT NULL
                 GROUP BY noubin_key_normalised HAVING COUNT(*) > 1)
             ORDER BY noubin_key_normalised, noudata_relpath",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for row in rows {
            let (k, rel) = row?;
            match groups.last_mut() {
                Some((gk, v)) if *gk == k => v.push(rel),
                _ => groups.push((k, vec![rel])),
            }
        }
        for (k, v) in groups {
            out.push(format!(
                "duplicate noubinKeyNormalised '{k}' in: {}",
                v.join(", ")
            ));
        }
    }
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT c.media_relpath, p.noudata_relpath
             FROM media_claims c JOIN playables p ON p.id = c.playable_id
             WHERE p.kind IN {RELEASE_KINDS_SQL}
               AND c.media_relpath IN (
                   SELECT c2.media_relpath FROM media_claims c2
                   JOIN playables p2 ON p2.id = c2.playable_id
                   WHERE p2.kind IN {RELEASE_KINDS_SQL}
                   GROUP BY c2.media_relpath HAVING COUNT(DISTINCT c2.playable_id) > 1)
             GROUP BY c.media_relpath, p.noudata_relpath
             ORDER BY c.media_relpath, p.noudata_relpath"
        ))?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for row in rows {
            let (m, rel) = row?;
            match groups.last_mut() {
                Some((gm, v)) if *gm == m => v.push(rel),
                _ => groups.push((m, vec![rel])),
            }
        }
        for (m, v) in groups {
            out.push(format!(
                "media '{m}' is claimed by several .noudata: {}",
                v.join(", ")
            ));
        }
    }
    // These lines reach the screen and the portal; the rows came from the index file.
    Ok(out.into_iter().map(|w| sanitise_line(&w, 1000)).collect())
}

/// Longest string a row of this build's own scanner can hold (titles are capped at 256
/// characters, 4 bytes each at most).
const MAX_STORED_TEXT_BYTES: usize = 1024;

fn text_is_foreign(s: &str) -> bool {
    s.len() > MAX_STORED_TEXT_BYTES || crate::sanitise::has_unsafe_chars(s)
}

fn opt_is_foreign(s: &Option<String>) -> bool {
    s.as_deref().is_some_and(text_is_foreign)
}

fn path_is_foreign(s: &str) -> bool {
    guard::clean_path(s).is_none()
}

fn path_list_is_foreign(json: &str) -> bool {
    match serde_json::from_str::<Vec<String>>(json) {
        Ok(list) => list.iter().any(|p| path_is_foreign(p)),
        Err(_) => true,
    }
}

/// Do not trust rows this build's scanner could not have written (`PAR-02`): a row whose
/// strings carry control / bidi characters or whose paths are unacceptable can only come from
/// a foreign index. A `.noudata` row is marked "re-read" (`mtime_ns = -1`), so the next scan
/// rebuilds it from the document instead of believing its matching mtime and size; a
/// tainted orphan row is dropped (the scan lists orphans again from the media files).
/// Returns how many rows were distrusted. Run once when a writer opens the index.
pub(super) fn distrust_foreign_rows(conn: &mut Connection) -> Result<u32, LibraryError> {
    let mut tainted: HashSet<i64> = HashSet::new();
    let mut tainted_orphans: HashSet<i64> = HashSet::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id, noudata_relpath, noubin_key_normalised, title, primary_artist,
                    cover_relpath, refers_to_relpath, media_relpaths_json, is_orphan
             FROM playables",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, i64>(8)? != 0,
            ))
        })?;
        for row in rows {
            let (id, rel, key, title, artist, cover, refers, media, orphan) = row?;
            // A row whose own path is foreign is not a document of this library: dropped.
            if rel.as_deref().is_some_and(path_is_foreign) {
                tainted_orphans.insert(id);
                continue;
            }
            let bad = opt_is_foreign(&key)
                || opt_is_foreign(&title)
                || opt_is_foreign(&artist)
                || cover.as_deref().is_some_and(path_is_foreign)
                || refers.as_deref().is_some_and(path_is_foreign)
                || path_list_is_foreign(&media);
            if bad {
                if orphan {
                    tainted_orphans.insert(id);
                } else {
                    tainted.insert(id);
                }
            }
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT playable_id, media_relpath, media_relpaths_json, media_title, title, artist
             FROM playable_items",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        for row in rows {
            let (id, media, alts, media_title, title, artist) = row?;
            // An item may legitimately have no file (`media_relpath` empty).
            if (!media.is_empty() && path_is_foreign(&media))
                || path_list_is_foreign(&alts)
                || opt_is_foreign(&media_title)
                || text_is_foreign(&title)
                || opt_is_foreign(&artist)
            {
                tainted.insert(id);
            }
        }
    }
    {
        let mut stmt = conn.prepare("SELECT playable_id, relpath, alt_text FROM playable_covers")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (id, relpath, alt) = row?;
            if path_is_foreign(&relpath) || opt_is_foreign(&alt) {
                tainted.insert(id);
            }
        }
    }
    // Orphan ids that appear among the item/cover findings are orphans too.
    let orphan_ids: HashSet<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM playables WHERE is_orphan = 1")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        rows.collect::<Result<_, _>>()?
    };
    for id in tainted.iter().copied().collect::<Vec<_>>() {
        if orphan_ids.contains(&id) {
            tainted.remove(&id);
            tainted_orphans.insert(id);
        }
    }
    // Tracking tables: rows naming paths outside the library are deleted (`load_prev` already
    // ignores them; a scan that finds nothing to change would otherwise never remove them).
    let mut foreign_tracking: Vec<(&'static str, &'static str, String)> = Vec::new();
    for (table, column, allow_root) in [
        ("dirs", "relpath", true),
        ("media_files", "relpath", false),
        ("noudata_skips", "relpath", false),
    ] {
        let mut stmt = conn.prepare(&format!("SELECT {column} FROM {table}"))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            let rel = row?;
            if !tracked_rel_ok(&rel, allow_root) {
                foreign_tracking.push((table, column, rel));
            }
        }
    }
    let total = (tainted.len() + tainted_orphans.len() + foreign_tracking.len()) as u32;
    if total == 0 {
        return Ok(0);
    }
    let tx = Transaction::new(conn, TransactionBehavior::Immediate)?;
    for (table, column, rel) in &foreign_tracking {
        tx.execute(&format!("DELETE FROM {table} WHERE {column} = ?1"), params![rel])?;
    }
    for id in &tainted {
        tx.execute("UPDATE playables SET mtime_ns = -1 WHERE id = ?1", params![id])?;
    }
    for id in &tainted_orphans {
        delete_playable(&tx, *id)?;
    }
    tx.commit()?;
    Ok(total)
}

// ---------------------------------------------------------------------------
// Duration probe + write-through
// ---------------------------------------------------------------------------

/// Outcome of [`super::LibraryWriter::probe_durations`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeReport {
    /// Items whose length was measured.
    pub probed: u32,
    /// `.noudata` files rewritten (`uniteData.expectedLength` added or corrected).
    pub written: u32,
    /// Items whose media could not be measured (not retried until the file changes).
    pub failed: u32,
    /// `.noudata` files that could not be read or written back (index still updated).
    pub write_errors: u32,
}

impl ProbeReport {
    /// Visible index data changed (a length became known). Failures are bookkeeping only.
    pub fn changed(&self) -> bool {
        self.probed > 0
    }
}

pub(super) fn probe_durations(
    root: &Path,
    conn: &mut Connection,
    probe: &mut dyn FnMut(&Path) -> Option<u64>,
    max_items: usize,
) -> Result<ProbeReport, LibraryError> {
    ensure_root_still_present(root)?;
    let mut report = ProbeReport::default();
    if max_items == 0 {
        return Ok(report);
    }
    // (playable id, position, media path, .noudata relpath)
    let candidates: Vec<(i64, i64, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT i.playable_id, i.position, i.media_relpath, p.noudata_relpath
             FROM playable_items i JOIN playables p ON p.id = i.playable_id
             WHERE i.available = 1 AND i.media_relpath != ''
               AND (p.kind != 'playlist' OR i.resolved_from IS NULL)
               AND (i.duration_src IS NULL OR i.duration_src = 'unite')
             ORDER BY i.playable_id, i.position LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![max_items as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect::<Result<_, _>>()?
    };
    if candidates.is_empty() {
        return Ok(report);
    }

    // Measure first (slow, no DB lock), then update files + index.
    struct Measured {
        pid: i64,
        pos: i64,
        noudata: Option<String>,
        ms: Option<u64>,
    }
    let measured: Vec<Measured> = candidates
        .into_iter()
        .map(|(pid, pos, media, noudata)| Measured {
            pid,
            pos,
            // The row came from the index file (untrusted): only a plain in-library regular
            // file is ever handed to the decoder-backed probe.
            ms: confine::clean_relpath(&media)
                .filter(|rel| confine::is_confined_regular_file(root, rel))
                .and_then(|rel| probe(&abs_of(root, &rel)))
                .filter(|&d| d > 0),
            noudata,
        })
        .collect();

    // `.noudata` write-through, one rewrite per file.
    let mut new_fingerprints: HashMap<i64, (i64, i64)> = HashMap::new();
    let mut i = 0;
    while i < measured.len() {
        let pid = measured[i].pid;
        let mut j = i;
        while j < measured.len() && measured[j].pid == pid {
            j += 1;
        }
        let group = &measured[i..j];
        i = j;
        let Some(rel) = group[0].noudata.as_deref() else {
            continue; // orphan: nothing to write back
        };
        if group.iter().all(|m| m.ms.is_none()) {
            continue;
        }
        let path = abs_of(root, rel);
        // Fingerprint taken *before* the read: the atomic write below refuses (and we skip)
        // if the file changed in between (playlist edit, resume write-back, hand edit).
        let rev_before = crate::playlist::FileRevision::of(&path).ok();
        let doc = read_noudata_capped(&path)
            .map_err(LibraryError::from)
            .and_then(|b| parse_noudata(&b).map_err(LibraryError::from));
        let mut doc = match doc {
            Ok(d) => d,
            Err(_) => {
                report.write_errors += 1;
                continue;
            }
        };
        let mut dirty = false;
        for m in group {
            let (Some(ms), Some(item)) = (m.ms, doc.media_list.get_mut(m.pos as usize)) else {
                continue;
            };
            let declared = item.unite_data.as_ref().and_then(|u| u.expected_length);
            if declared.is_some_and(|d| d.as_millis().abs_diff(ms) <= DURATION_TOLERANCE_MS) {
                continue; // idempotent: already recorded
            }
            item.unite_data
                .get_or_insert_with(UniteData::default)
                .expected_length = Some(NoubinTimestamp::from_millis(ms));
            dirty = true;
        }
        if dirty {
            // Temp file + rename (never truncate the source of truth in place: a power cut
            // mid-write must not leave a broken `.noudata`).
            let guard = match rev_before {
                Some(rev) => crate::playlist::WriteGuard::Unchanged(rev),
                None => {
                    report.write_errors += 1;
                    continue;
                }
            };
            match crate::playlist::write_noudata_atomic(&doc, &path, guard) {
                Ok(rev) => {
                    report.written += 1;
                    new_fingerprints.insert(pid, (rev.mtime_ns, rev.size as i64));
                }
                Err(_) => report.write_errors += 1,
            }
        }
    }

    let tx = Transaction::new(conn, TransactionBehavior::Immediate)?;
    for m in &measured {
        match m.ms {
            Some(ms) => {
                tx.execute(
                    "UPDATE playable_items SET duration_ms = ?3, duration_src = 'probed'
                     WHERE playable_id = ?1 AND position = ?2",
                    params![m.pid, m.pos, ms as i64],
                )?;
                report.probed += 1;
            }
            None => {
                tx.execute(
                    "UPDATE playable_items SET duration_src = 'failed'
                     WHERE playable_id = ?1 AND position = ?2",
                    params![m.pid, m.pos],
                )?;
                report.failed += 1;
            }
        }
    }
    for (pid, (m, size)) in &new_fingerprints {
        tx.execute(
            "UPDATE playables SET mtime_ns = ?2, mtime_secs = ?3, file_size = ?4 WHERE id = ?1",
            params![pid, m, m.div_euclid(1_000_000_000), size],
        )?;
    }
    let mut scratch = ScanReport::default();
    finalize(&tx, root, &mut scratch)?;
    tx.commit()?;
    Ok(report)
}
