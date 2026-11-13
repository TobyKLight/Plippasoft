//! ART_03: cover art service — read API, edit API (staging + one atomic commit), thumbnails and
//! the background thumbnail backfill.
//!
//! The rules of *which* images a playable shows live in `plippa_noubin::covers` (D4); this module
//! serves them and edits them:
//!
//! - **Read**: [`ServerService::list_covers`] (effective ordered list with pixel sizes from a
//!   header probe, cached by `(path, mtime, size)`) and [`ServerService::cover_plan`] (which file
//!   to send for image `n` at a size). The plan is resolved under the service lock and *read*
//!   after it is released ([`CoverPlan::read`]), so a slow stick never blocks the portal.
//!   Content types come from the first bytes only (NBSecurityRequirements §5.2); GIF and
//!   friends are not served (D1). Nothing is decoded on a request path.
//! - **Edit**: [`stage_cover`] streams one upload to `<library>/.plippa/tmp/cover-<id>`;
//!   [`ServerService::put_covers`] commits the final ordered list (keep / staged) in one step:
//!   files and thumbnails first, then the `.noudata`, then the index, then D9 deletion. A
//!   failure before the `.noudata` is written undoes every file it created.
//! - **Thumbnails**: [`ensure_thumbnails`] writes the 128 / 512 / 1024 px JPEG tiers (D5);
//!   [`spawn_cover_worker`] generates the missing ones for art dropped straight onto the stick,
//!   one image at a time, at low priority, never while the engine plays.
//!
//! Non-square images are stored and served untouched (D2: the crop happens at draw time).

use super::collections::{lib_err, write_err};
use super::imports::ImportError;
use super::{read_noudata, path_relative_posix, ServerError, ServerService};
use crate::unite::fsops::{write_new_atomic, LibraryFs, Transaction};
use crate::unite::sniff::sniff_image_kind;
use crate::web::download::{stream_to_temp, Budget, Cancel, DownloadError, DownloadLimits, StreamRules};
use plippa_image::{decode, encode_jpeg_fit, probe, ImageError, ImageKind, ImageLimits};
use plippa_noubin::covers::{
    apply_cover_edit, effective_item_covers, resolve_thumbnail, sibling_candidates,
    thumbnail_relpath, CoverEdit, CoverImageSpec, CoverRef, CoverSource, CoverTarget, ReleaseDoc,
    ThumbTier, COVER_EXTENSIONS,
};
use plippa_noubin::{
    fs_release_resolver, FileRevision, ImportKind, LibraryKind, LibraryReader, LibraryRoot,
    PlayableRecord, WriteGuard,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Largest cover file served or accepted (§1.27, D1).
pub const MAX_COVER_BYTES: u64 = 16 * 1024 * 1024;
/// Images one item / collection may carry through the portal.
pub const MAX_COVERS: usize = 12;
/// A staged upload nobody committed is dropped after this long (same as import sessions).
const STAGED_TTL: Duration = Duration::from_secs(2 * 60 * 60);
/// Staged uploads held at once (the popup holds at most [`MAX_COVERS`]).
const MAX_STAGED: usize = 2 * MAX_COVERS;
/// Entries of the header-probe cache.
const PROBE_CACHE_ENTRIES: usize = 512;
/// Prefix sizes tried when reading an image header (a JPEG frame header can follow large EXIF
/// segments).
const PROBE_PREFIXES: [usize; 2] = [64 * 1024, 1024 * 1024];
/// Pause between two thumbnail jobs of the backfill worker.
const BACKFILL_GAP: Duration = Duration::from_millis(250);
/// The backfill worker re-plans at least this often (art dropped on the stick does not change
/// the index generation).
const BACKFILL_REPLAN: Duration = Duration::from_secs(120);
/// Staged uploads are named `cover-<id>` in the library's tmp folder.
const STAGED_PREFIX: &str = "cover-";

/// Limits applied to every cover image (D1).
pub fn cover_limits() -> ImageLimits {
    ImageLimits::default()
}

pub(super) fn kind_ext(k: ImageKind) -> &'static str {
    match k {
        ImageKind::Jpeg => "jpg",
        ImageKind::Png => "png",
        ImageKind::Webp => "webp",
    }
}

pub(super) fn kind_name(k: ImageKind) -> &'static str {
    match k {
        ImageKind::Jpeg => "jpeg",
        ImageKind::Png => "png",
        ImageKind::Webp => "webp",
    }
}

/// Plain-words reason for an image the toolkit refused.
pub(super) fn image_error_text(e: &ImageError) -> String {
    match e {
        ImageError::Unsupported => "not a JPEG, PNG or WebP image Plippa can show".into(),
        ImageError::TooLarge { .. } => {
            "the image is larger than 5000 x 5000 pixels, 25 megapixels or 16 MB".into()
        }
        ImageError::Truncated => "the image file is incomplete".into(),
        ImageError::Corrupt(_) => "the image file is damaged".into(),
        _ => "the image cannot be used".into(),
    }
}

// ---------------------------------------------------------------------------
// File helpers
// ---------------------------------------------------------------------------

/// Modification time (ns) and length: the identity of a file for caches and ETags.
fn stamp_of(meta: &fs::Metadata) -> (u128, u64) {
    let ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    (ns, meta.len())
}

/// Absolute path of the library-relative `rel` when it is a plain file inside the library
/// (no `..`, no symlink on the way, not a symlink itself).
fn resolve_plain_file(root: &Path, rel: &str) -> Result<(PathBuf, fs::Metadata), ServerError> {
    let outside = || ServerError::NotFound("cover image is outside the library".into());
    if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|s| s == "..") {
        return Err(outside());
    }
    let path = LibraryFs::new(root)
        .and_then(|fs| fs.resolve(Path::new(rel)))
        .map_err(|_| outside())?;
    let meta = fs::symlink_metadata(&path)
        .map_err(|_| ServerError::NotFound("cover image file is missing".into()))?;
    if !meta.file_type().is_file() {
        return Err(ServerError::NotFound(
            "cover image is not a plain file".into(),
        ));
    }
    Ok((path, meta))
}

fn open_nofollow(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// At most `cap` bytes of `path` (one more than `cap` is read so an over-long file shows).
fn read_bounded(path: &Path, cap: u64) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    open_nofollow(path)?.take(cap + 1).read_to_end(&mut out)?;
    Ok(out)
}

/// Header facts of the image at `path` (`len` bytes long), reading as little as possible.
fn probe_path(
    path: &Path,
    len: u64,
    limits: &ImageLimits,
) -> Result<(ImageKind, u32, u32), ServerError> {
    if len > limits.max_bytes {
        return Err(ServerError::Invalid(
            "cover image is too large to serve".into(),
        ));
    }
    let mut file = open_nofollow(path)?;
    let mut buf: Vec<u8> = Vec::new();
    let mut tries: Vec<usize> = PROBE_PREFIXES
        .iter()
        .copied()
        .filter(|p| (*p as u64) < len)
        .collect();
    tries.push(len as usize);
    for want in tries {
        let mut chunk = vec![0u8; want - buf.len()];
        let mut got = 0;
        while got < chunk.len() {
            let n = file.read(&mut chunk[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        buf.extend_from_slice(&chunk[..got]);
        match probe(&buf, limits) {
            Err(ImageError::Truncated) if buf.len() as u64 != len => continue,
            Ok(v) => return Ok(v),
            Err(e) => return Err(ServerError::Invalid(image_error_text(&e))),
        }
    }
    Err(ServerError::Invalid(image_error_text(&ImageError::Truncated)))
}

/// Names (ASCII-lowercased) in the library-relative folder `dir_rel` (`""` = the root).
fn folder_names(root: &Path, dir_rel: &str) -> HashSet<String> {
    let dir = if dir_rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(dir_rel)
    };
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

fn stem_of(rel: &str) -> &str {
    let file = rel.rsplit_once('/').map_or(rel, |(_, f)| f);
    file.rsplit_once('.').map_or(file, |(s, _)| s)
}

// ---------------------------------------------------------------------------
// Thumbnails (D5)
// ---------------------------------------------------------------------------

/// Tiers generated for an image whose longer side is `side`: every tier below it, plus the
/// smallest tier that holds it (written at the image's own size: never upscaled). A 100 px
/// image gets the 128 tier, a 700 px image 128 / 512 / 1024, a 5000 px image all three.
fn tiers_for(side: u32) -> Vec<ThumbTier> {
    let mut out: Vec<ThumbTier> = ThumbTier::ALL
        .into_iter()
        .filter(|t| t.px() < side)
        .collect();
    if let Some(fit) = ThumbTier::ALL.into_iter().find(|t| t.px() >= side) {
        out.push(fit);
    }
    out
}

/// True when `tier` of `image_relpath` exists (JPEG or WebP, either folder spelling).
fn tier_exists(root: &Path, image_relpath: &str, tier: ThumbTier) -> bool {
    resolve_thumbnail(root, image_relpath, tier.px()).is_some_and(|t| t.tier == tier)
}

/// True when `tier` of `image_relpath` exists but was written before the image last changed.
fn tier_is_stale(root: &Path, image_relpath: &str, tier: ThumbTier, image: &fs::Metadata) -> bool {
    let Some(t) = resolve_thumbnail(root, image_relpath, tier.px()).filter(|t| t.tier == tier)
    else {
        return false;
    };
    let Ok((_, tmeta)) = resolve_plain_file(root, &t.relpath) else {
        return false;
    };
    matches!((tmeta.modified(), image.modified()), (Ok(tm), Ok(im)) if tm < im)
}

/// What [`ensure_thumbnails`] changed.
#[derive(Debug, Default)]
pub struct ThumbReport {
    /// Library-relative paths of the thumbnails written.
    pub written: Vec<String>,
    /// Folders created for them (parents first), for rollback.
    pub created_dirs: Vec<PathBuf>,
}

impl ThumbReport {
    /// Delete what was written (rollback of a commit that failed afterwards).
    fn undo(&self, root: &Path) {
        for rel in self.written.iter().rev() {
            let _ = fs::remove_file(root.join(rel));
        }
        for d in self.created_dirs.iter().rev() {
            let _ = fs::remove_dir(d);
        }
    }
}

/// Write the missing thumbnail tiers of the library-relative `image_relpath` as
/// `thumbnails/<stem>-<tier>.jpg` next to it (temp file + rename). The aspect ratio is kept
/// (longer side = tier); the original is never touched. Tiers that exist (in `thumbnails/` or
/// `Thumbnails/`) are skipped, and nothing is decoded when none is missing. Images over
/// `limits` are refused before any pixel buffer exists.
pub fn ensure_thumbnails(
    root: &Path,
    image_relpath: &str,
    limits: &ImageLimits,
) -> Result<ThumbReport, ServerError> {
    let (path, meta) = resolve_plain_file(root, image_relpath)?;
    let (_, w, h) = probe_path(&path, meta.len(), limits)?;
    let wanted = tiers_for(w.max(h));
    // Thumbnails older than the image belong to an earlier file of the same name: start over.
    if wanted.iter().any(|t| tier_is_stale(root, image_relpath, *t, &meta)) {
        remove_thumbnails(root, image_relpath);
    }
    let missing: Vec<ThumbTier> = wanted
        .into_iter()
        .filter(|t| !tier_exists(root, image_relpath, *t))
        .collect();
    let mut report = ThumbReport::default();
    if missing.is_empty() {
        return Ok(report);
    }
    let bytes = read_bounded(&path, limits.max_bytes)?;
    let img = decode(&bytes, limits).map_err(|e| ServerError::Invalid(image_error_text(&e)))?;
    drop(bytes);
    let fs = LibraryFs::new(root).map_err(|e| ServerError::Message(e.to_string()))?;
    for tier in missing {
        let jpg = encode_jpeg_fit(&img, tier.px())
            .map_err(|e| ServerError::Invalid(image_error_text(&e)))?;
        let rel = thumbnail_relpath(image_relpath, tier, "jpg");
        let outcome = (|| {
            let parent = Path::new(&rel).parent().unwrap_or(Path::new(""));
            fs.ensure_dir(parent, &mut report.created_dirs)?;
            let dst = fs.resolve(Path::new(&rel))?;
            write_new_atomic(&dst, &jpg)
        })();
        match outcome {
            Ok(()) => report.written.push(rel),
            Err(e) => {
                report.undo(root);
                return Err(ServerError::Message(format!(
                    "cannot write the thumbnail of {image_relpath}: {e}"
                )));
            }
        }
    }
    Ok(report)
}

/// Delete the thumbnails (any tier, either folder spelling, JPEG or WebP) of `image_relpath`.
fn remove_thumbnails(root: &Path, image_relpath: &str) {
    let dir = dir_of(image_relpath);
    let stem = stem_of(image_relpath);
    for folder in ["thumbnails", "Thumbnails"] {
        let folder_rel = if dir.is_empty() {
            folder.to_string()
        } else {
            format!("{dir}/{folder}")
        };
        for tier in ThumbTier::ALL {
            for ext in ["jpg", "webp"] {
                let rel = format!("{folder_rel}/{stem}-{}.{ext}", tier.px());
                if let Ok((path, _)) = resolve_plain_file(root, &rel) {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// One uploaded image waiting for the Save of the edit popup.
#[derive(Debug, Clone)]
struct StagedCover {
    path: PathBuf,
    root: PathBuf,
    kind: ImageKind,
    created: Instant,
}

type ProbeKey = (PathBuf, String, u128, u64);
/// File identity and the item positions whose art differs from the collection's.
type ItemDiff = ((u128, u64), Vec<u32>);

#[derive(Default)]
struct ProbeCache {
    map: HashMap<ProbeKey, (ImageKind, u32, u32)>,
    order: VecDeque<ProbeKey>,
}

#[derive(Default)]
struct Backfill {
    /// Images served without a thumbnail since the worker last looked.
    nudged: Vec<(PathBuf, String)>,
    /// Images whose thumbnails could not be made, with the file identity they failed at: not
    /// retried until the file changes.
    failed: HashMap<(PathBuf, String), (u128, u64)>,
    /// Images whose thumbnails are complete, with the identity checked.
    done: HashMap<(PathBuf, String), (u128, u64)>,
}

/// Per-server cover state, shared by the service and the worker thread.
#[derive(Default)]
pub struct CoverStore {
    staged: Mutex<HashMap<String, StagedCover>>,
    probes: Mutex<ProbeCache>,
    backfill: Mutex<Backfill>,
    /// `(root, noudata, stamp)` -> positions whose art differs from their collection's.
    item_diff: Mutex<HashMap<(PathBuf, String), ItemDiff>>,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl CoverStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn probe_cached(
        &self,
        root: &Path,
        rel: &str,
        path: &Path,
        meta: &fs::Metadata,
    ) -> Result<(ImageKind, u32, u32), ServerError> {
        let (ns, len) = stamp_of(meta);
        let key: ProbeKey = (root.to_path_buf(), rel.to_string(), ns, len);
        if let Some(hit) = locked(&self.probes).map.get(&key) {
            return Ok(*hit);
        }
        let found = probe_path(path, len, &cover_limits())?;
        let mut cache = locked(&self.probes);
        while cache.order.len() >= PROBE_CACHE_ENTRIES {
            if let Some(old) = cache.order.pop_front() {
                cache.map.remove(&old);
            }
        }
        cache.order.push_back(key.clone());
        cache.map.insert(key, found);
        Ok(found)
    }

    fn nudge(&self, root: &Path, rel: &str) {
        let mut b = locked(&self.backfill);
        let key = (root.to_path_buf(), rel.to_string());
        if !b.nudged.contains(&key) {
            b.nudged.push(key);
        }
    }

    fn sweep_expired(&self) {
        let mut staged = locked(&self.staged);
        staged.retain(|_, s| {
            let keep = s.created.elapsed() < STAGED_TTL;
            if !keep {
                let _ = fs::remove_file(&s.path);
            }
            keep
        });
    }

    /// Delete every staged upload (quiesce).
    pub fn discard_all(&self) {
        for (_, s) in locked(&self.staged).drain() {
            let _ = fs::remove_file(&s.path);
        }
    }

    /// Delete `cover-*` files in `tmp` that no live staged upload owns (leftovers of an
    /// earlier run, like the `import-*` sweep).
    fn sweep_orphans(&self, tmp: &Path) {
        let live: HashSet<PathBuf> = locked(&self.staged).values().map(|s| s.path.clone()).collect();
        let Ok(rd) = fs::read_dir(tmp) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let is_cover = e.file_name().to_string_lossy().starts_with(STAGED_PREFIX);
            if is_cover && !live.contains(&p) && e.file_type().is_ok_and(|t| t.is_file()) {
                let _ = fs::remove_file(p);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Read API
// ---------------------------------------------------------------------------

/// Which cover list is addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverScope {
    /// The collection view of a playable (D4).
    Collection,
    /// The art of one item (0-based `mediaList` position) of a playable (D4 item order).
    Item(u32),
}

/// `size` of the image route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverSize {
    Tier(ThumbTier),
    /// The original file.
    Full,
}

impl CoverSize {
    /// `128`, `512`, `1024` or `full`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "128" => Some(Self::Tier(ThumbTier::T128)),
            "512" => Some(Self::Tier(ThumbTier::T512)),
            "1024" => Some(Self::Tier(ThumbTier::T1024)),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// URL of image `n` of `id` in `scope`, optionally at a thumbnail tier.
fn cover_url(id: u64, scope: CoverScope, n: usize, tier: Option<ThumbTier>) -> String {
    let size = tier.map(|t| format!("size={}", t.px()));
    match scope {
        CoverScope::Collection => match size {
            Some(s) => format!("/api/collections/{id}/covers/{n}?{s}"),
            None => format!("/api/collections/{id}/covers/{n}"),
        },
        CoverScope::Item(pos) => match size {
            Some(s) => format!("/api/items/{id}/covers/{n}?item={pos}&{s}"),
            None => format!("/api/items/{id}/covers/{n}?item={pos}"),
        },
    }
}

/// One image of `GET …/covers`.
#[derive(Debug, Clone, Serialize)]
pub struct CoverOut {
    /// Position in the effective list; stable, and what `{"keep": n}` and `…/covers/:n` take.
    /// An entry whose file is not a servable image is left out, so `n` can skip numbers.
    pub n: usize,
    /// The original file.
    pub url: String,
    pub thumb_url: String,
    pub medium_url: String,
    pub w: u32,
    pub h: u32,
    pub bytes: u64,
    /// `jpeg`, `png` or `webp`.
    pub kind: &'static str,
    /// `item`, `local`, `release` or `sibling`.
    pub source: &'static str,
    pub alt: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoverList {
    /// Only set by a commit on a media file that had no `.noudata`: its new playable id.
    #[serde(
        serialize_with = "serialize_opt_u64_as_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<u64>,
    pub covers: Vec<CoverOut>,
    /// How many images of the list exist on the stick but cannot be served (over a limit or not
    /// a decodable image). They are not in `covers`; a save keeps them.
    pub hidden: usize,
}

fn serialize_opt_u64_as_string<S: serde::Serializer>(
    v: &Option<u64>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => s.serialize_str(&v.to_string()),
        None => s.serialize_none(),
    }
}

/// The file to send for one image request, resolved under the service lock.
#[derive(Debug, Clone)]
pub struct CoverPlan {
    path: PathBuf,
    /// The original, tried when the thumbnail is unusable.
    original: PathBuf,
    /// Strong validator of the file served (size + mtime).
    pub etag: String,
}

impl CoverPlan {
    fn of(path: PathBuf, original: PathBuf, meta: &fs::Metadata, thumb: bool) -> Self {
        let (ns, len) = stamp_of(meta);
        Self {
            path,
            original,
            etag: format!("\"{}{len:x}-{ns:x}\"", if thumb { "t" } else { "" }),
        }
    }

    /// Read the file (bounded) and decide its type from its first bytes. Run this without the
    /// service lock.
    pub fn read(&self) -> Result<(Vec<u8>, &'static str), ServerError> {
        match Self::read_one(&self.path) {
            Err(_) if self.path != self.original => Self::read_one(&self.original),
            other => other,
        }
    }

    fn read_one(path: &Path) -> Result<(Vec<u8>, &'static str), ServerError> {
        let bytes = read_bounded(path, MAX_COVER_BYTES)
            .map_err(|_| ServerError::NotFound("cover image file is missing".into()))?;
        if bytes.len() as u64 > MAX_COVER_BYTES {
            return Err(ServerError::Invalid(
                "cover image is too large to serve".into(),
            ));
        }
        let ctype = sniff_image_kind(&bytes)
            .map(|k| k.content_type())
            .ok_or_else(|| ServerError::NotFound("unsupported cover image type".into()))?;
        Ok((bytes, ctype))
    }
}

impl ServerService {
    /// The cover store (shared with the backfill worker).
    pub fn cover_store(&self) -> Arc<CoverStore> {
        Arc::clone(&self.covers)
    }

    /// The effective cover list of `id` in `scope`, plus the library index and record.
    fn effective_covers(
        &mut self,
        id: u64,
        scope: CoverScope,
    ) -> Result<(usize, PlayableRecord, Vec<CoverRef>), ServerError> {
        let (idx, rec) = self.require_record(id)?;
        let w = &self.libraries[idx].writer;
        let list = match scope {
            CoverScope::Collection => w.covers_of(rec.id),
            CoverScope::Item(pos) => {
                let known = w.items_of(rec.id).map_err(lib_err)?;
                if !known.iter().any(|i| i.position == pos) {
                    return Err(ServerError::NotFound(format!("{id} has no item {pos}")));
                }
                w.item_covers(rec.id, pos)
            }
        }
        .map_err(lib_err)?;
        Ok((idx, rec, list))
    }

    /// `GET …/covers`: the effective ordered list, entries that are not servable images left out.
    pub fn list_covers(&mut self, id: u64, scope: CoverScope) -> Result<CoverList, ServerError> {
        let (idx, _, list) = self.effective_covers(id, scope)?;
        let (covers, hidden) = self.describe_covers(idx, id, scope, &list);
        Ok(CoverList {
            id: None,
            covers,
            hidden,
        })
    }

    fn describe_covers(
        &self,
        idx: usize,
        id: u64,
        scope: CoverScope,
        list: &[CoverRef],
    ) -> (Vec<CoverOut>, usize) {
        let root = &self.libraries[idx].root;
        let mut out = Vec::new();
        let mut hidden = 0;
        for (n, c) in list.iter().enumerate() {
            let Ok((path, meta)) = resolve_plain_file(root, &c.relpath) else {
                continue;
            };
            let Ok((kind, w, h)) = self.covers.probe_cached(root, &c.relpath, &path, &meta) else {
                hidden += 1;
                continue;
            };
            out.push(CoverOut {
                n,
                url: cover_url(id, scope, n, None),
                thumb_url: cover_url(id, scope, n, Some(ThumbTier::T128)),
                medium_url: cover_url(id, scope, n, Some(ThumbTier::T512)),
                w,
                h,
                bytes: meta.len(),
                kind: kind_name(kind),
                source: c.source.as_str(),
                alt: c.alt_text.clone(),
            });
        }
        (out, hidden)
    }

    /// Indexes of the entries of `list` the client was not shown although a plain file exists
    /// for them (it failed the size/dimension/decode probe). A save must keep these and must
    /// never delete their files. An entry whose file is missing (or is not a plain file inside
    /// the library) is not in this set: there is nothing to protect, so a save drops it from the
    /// document silently (the library's `covers_of` normally omits missing files already).
    fn hidden_indexes(&self, idx: usize, list: &[CoverRef]) -> Vec<usize> {
        let root = &self.libraries[idx].root;
        list.iter()
            .enumerate()
            .filter(|(_, c)| {
                resolve_plain_file(root, &c.relpath).is_ok_and(|(path, meta)| {
                    self.covers
                        .probe_cached(root, &c.relpath, &path, &meta)
                        .is_err()
                })
            })
            .map(|(n, _)| n)
            .collect()
    }

    /// Which file answers `GET …/covers/:n?size=`: the thumbnail of the wanted tier (else the
    /// nearest that exists), else the original — and the backfill is nudged for that image.
    pub fn cover_plan(
        &mut self,
        id: u64,
        scope: CoverScope,
        n: usize,
        size: CoverSize,
    ) -> Result<CoverPlan, ServerError> {
        let (idx, _, list) = self.effective_covers(id, scope)?;
        let c = list
            .get(n)
            .ok_or_else(|| ServerError::NotFound("no such cover image".into()))?;
        let root = &self.libraries[idx].root;
        let (original, meta) = resolve_plain_file(root, &c.relpath)?;
        if meta.len() > MAX_COVER_BYTES {
            return Err(ServerError::Invalid(
                "cover image is too large to serve".into(),
            ));
        }
        if let CoverSize::Tier(tier) = size {
            let thumb = resolve_thumbnail(root, &c.relpath, tier.px());
            if thumb.as_ref().map(|t| t.tier) != Some(tier) {
                self.covers.nudge(root, &c.relpath);
            }
            if let Some((path, tmeta)) = thumb
                .and_then(|t| resolve_plain_file(root, &t.relpath).ok())
                .filter(|(_, m)| m.len() <= MAX_COVER_BYTES)
            {
                return Ok(CoverPlan::of(path, original, &tmeta, true));
            }
        }
        Ok(CoverPlan::of(original.clone(), original, &meta, false))
    }

    /// `GET /api/collections/:id/cover` (compat): the primary image at 512.
    pub fn collection_cover(&mut self, id: u64) -> Result<(Vec<u8>, &'static str), ServerError> {
        self.cover_plan(id, CoverScope::Collection, 0, CoverSize::Tier(ThumbTier::T512))?
            .read()
    }

    /// `GET …/covers/staged/:sid`: the preview of an upload waiting for Save.
    pub fn staged_cover_plan(&mut self, id: u64, staged_id: &str) -> Result<CoverPlan, ServerError> {
        let (idx, _) = self.require_record(id)?;
        let root = self.libraries[idx].root.clone();
        let s = locked(&self.covers.staged)
            .get(staged_id)
            .filter(|s| s.root == root && s.created.elapsed() < STAGED_TTL)
            .cloned()
            .ok_or_else(|| ServerError::NotFound("unknown or expired staged image".into()))?;
        let meta = fs::symlink_metadata(&s.path)
            .map_err(|_| ServerError::NotFound("unknown or expired staged image".into()))?;
        Ok(CoverPlan::of(s.path.clone(), s.path, &meta, false))
    }

    /// `DELETE …/covers/staged/:sid`: drop an upload the user will not save (Remove / Cancel in
    /// the editor), freeing its slot at once instead of at the TTL. Idempotent.
    pub fn discard_staged_cover(&mut self, id: u64, staged_id: &str) -> Result<(), ServerError> {
        let (idx, _) = self.require_record(id)?;
        let root = self.libraries[idx].root.clone();
        let mut staged = locked(&self.covers.staged);
        if staged.get(staged_id).is_some_and(|s| s.root == root) {
            if let Some(s) = staged.remove(staged_id) {
                let _ = fs::remove_file(&s.path);
            }
        }
        Ok(())
    }

    // ----- summaries ------------------------------------------------------

    /// How many images a collection shows, for the summary fields (`cover`, `cover_thumb`,
    /// `cover_count`). The index records only *declared* art, so a `cover.*` sitting next to
    /// the `.noudata` (D4's last tier) is resolved live here — the same list
    /// `GET …/covers` serves. Without it an image dropped straight onto the stick showed on the
    /// device but in no portal list or detail view, until it was declared by an edit.
    pub(super) fn shown_cover_count(&self, idx: usize, rec: &PlayableRecord) -> u32 {
        let declared = rec.cover_count.max(u32::from(rec.cover_relpath.is_some()));
        // Orphans get a `.noudata` on their first edit; there is nothing to resolve for them.
        if declared > 0 || rec.is_orphan || rec.noudata_relpath.is_empty() {
            return declared;
        }
        // `covers_of` falls back to the sibling, and reports 0 for art hidden on purpose.
        self.libraries[idx]
            .writer
            .covers_of(rec.id)
            .map_or(0, |list| list.len() as u32)
    }

    /// URL of the 128 px image 0 of a collection that has art (see
    /// [`Self::shown_cover_count`]); a missing tier falls back to the original file.
    pub(super) fn cover_thumb_url(id: u64) -> String {
        cover_url(id, CoverScope::Collection, 0, Some(ThumbTier::T128))
    }

    /// Positions of `rec`'s items whose primary art differs from the collection's (D16), with
    /// the thumbnail URLs. `resolved` maps a playlist entry's position to the playable that
    /// supplies it. Results for a plain document are cached by the file's identity.
    pub(super) fn differing_item_thumbs(
        &self,
        idx: usize,
        id: u64,
        rec: &PlayableRecord,
        resolved: &HashMap<u32, i64>,
    ) -> HashMap<u32, String> {
        let lib = &self.libraries[idx];
        if rec.is_orphan || rec.noudata_relpath.is_empty() {
            return HashMap::new();
        }
        let path = lib.root.join(&rec.noudata_relpath);
        let plain = resolved.is_empty() && rec.kind != LibraryKind::Playlist;
        let stamp = fs::metadata(&path).ok().map(|m| stamp_of(&m));
        let key = (lib.root.clone(), rec.noudata_relpath.clone());
        if let (true, Some(stamp)) = (plain, stamp) {
            if let Some((s, positions)) = locked(&self.covers.item_diff).get(&key) {
                if *s == stamp {
                    return Self::thumb_map(id, positions);
                }
            }
        }
        let positions = self.compute_differing(idx, rec, &path, resolved);
        if let (true, Some(stamp)) = (plain, stamp) {
            locked(&self.covers.item_diff).insert(key, (stamp, positions.clone()));
        }
        Self::thumb_map(id, &positions)
    }

    fn thumb_map(id: u64, positions: &[u32]) -> HashMap<u32, String> {
        positions
            .iter()
            .map(|&p| {
                (
                    p,
                    cover_url(id, CoverScope::Item(p), 0, Some(ThumbTier::T128)),
                )
            })
            .collect()
    }

    fn compute_differing(
        &self,
        idx: usize,
        rec: &PlayableRecord,
        path: &Path,
        resolved: &HashMap<u32, i64>,
    ) -> Vec<u32> {
        let lib = &self.libraries[idx];
        let Ok(doc) = read_noudata(path) else {
            return Vec::new();
        };
        if resolved.is_empty() && doc.media_list.iter().all(|m| m.item_cover_image.is_none()) {
            return Vec::new();
        }
        let collection_primary = lib
            .writer
            .covers_of(rec.id)
            .ok()
            .and_then(|l| l.into_iter().next())
            .map(|c| c.relpath);
        let resolver = fs_release_resolver(path);
        let mut releases: HashMap<i64, Option<(String, plippa_noubin::NoudataDocument)>> =
            HashMap::new();
        let mut out = Vec::new();
        for pos in 0..doc.media_list.len() as u32 {
            let release = resolved.get(&pos).and_then(|rid| {
                releases
                    .entry(*rid)
                    .or_insert_with(|| {
                        let rrec = lib.writer.lookup_by_id(*rid).ok().flatten()?;
                        let d = read_noudata(&lib.root.join(&rrec.noudata_relpath)).ok()?;
                        Some((rrec.noudata_relpath, d))
                    })
                    .as_ref()
            });
            let item = effective_item_covers(
                &lib.root,
                &doc,
                &rec.noudata_relpath,
                pos as usize,
                release.map(|(relpath, doc)| ReleaseDoc { doc, relpath }),
                &resolver,
            );
            let primary = item.first().map(|c| c.relpath.as_str());
            if primary.is_some() && primary != collection_primary.as_deref() {
                out.push(pos);
            }
        }
        out
    }

    // ----- worker hooks ---------------------------------------------------

    /// What the thumbnail worker needs from the service, read in one short lock.
    pub(crate) fn cover_worker_state(&mut self) -> CoverWorkerState {
        let mirror = self.with_ipc(|ipc| ipc.mirror());
        CoverWorkerState {
            roots: self.libraries.iter().map(|l| l.root.clone()).collect(),
            playing: mirror
                .playback
                .as_ref()
                .is_some_and(|p| p.transport == plippa_core::TransportState::Playing),
            generation: self.generation,
            quiescing: self.quiescing,
            store: Arc::clone(&self.covers),
        }
    }

    /// Remove `cover-*` leftovers of an earlier run from every library's tmp folder.
    pub(super) fn sweep_cover_staging(&self) {
        for lib in &self.libraries {
            self.covers
                .sweep_orphans(&lib.root.join(plippa_noubin::PLAYER_DB_DIR).join("tmp"));
        }
    }
}

pub(crate) struct CoverWorkerState {
    pub roots: Vec<PathBuf>,
    pub playing: bool,
    pub generation: u64,
    pub quiescing: bool,
    pub store: Arc<CoverStore>,
}

// ---------------------------------------------------------------------------
// Staging upload
// ---------------------------------------------------------------------------

/// What [`stage_cover`] needs, taken under the service lock so the upload itself runs without it.
pub struct StageCtx {
    store: Arc<CoverStore>,
    tmp: PathBuf,
    root: PathBuf,
    id: u64,
}

impl ServerService {
    /// Check that cover art of `id` may be edited at all and prepare the staging folder.
    pub fn begin_cover_stage(&mut self, id: u64) -> Result<StageCtx, ServerError> {
        let (idx, rec) = self.require_record(id)?;
        let root = self.libraries[idx].root.clone();
        Self::require_cover_editable(&root, &rec)?;
        let tmp = LibraryFs::new(&root)
            .and_then(|l| l.tmp_dir())
            .map_err(|e| ServerError::Message(e.to_string()))?;
        self.covers.sweep_expired();
        self.covers.sweep_orphans(&tmp);
        if locked(&self.covers.staged).len() >= MAX_STAGED {
            return Err(ServerError::Conflict(
                "too many images are waiting to be saved; save or cancel the edit first".into(),
            ));
        }
        Ok(StageCtx {
            store: Arc::clone(&self.covers),
            tmp,
            root,
            id,
        })
    }

    fn require_cover_editable(root: &Path, rec: &PlayableRecord) -> Result<(), ServerError> {
        if rec.is_dynamic {
            return Err(ServerError::Invalid(
                "dynamic (search) playlists have no cover art to edit".into(),
            ));
        }
        if rec.media_missing {
            return Err(ServerError::Conflict(
                "the media of this collection is not available, so its cover art cannot be edited"
                    .into(),
            ));
        }
        // A media file without a `.noudata` gets one on the first save.
        let readonly = !rec.noudata_relpath.is_empty()
            && fs::metadata(root.join(&rec.noudata_relpath)).is_ok_and(|m| m.permissions().readonly());
        if readonly {
            return Err(ServerError::Conflict(
                "the .noudata of this collection is read-only".into(),
            ));
        }
        Ok(())
    }
}

/// Stream one image upload into the library's tmp folder and register it as staged. Refuses
/// anything that is not a JPEG / PNG / WebP within the Noubin limits (D1) before it is kept.
/// Reply: `{staged_id, w, h, bytes, kind, preview_url}`.
pub fn stage_cover(
    ctx: &StageCtx,
    body: &mut dyn Read,
    declared: Option<u64>,
    limits: &DownloadLimits,
) -> Result<serde_json::Value, ImportError> {
    let budget = Budget::new(MAX_COVER_BYTES);
    let cancel = Cancel::new();
    let mut check = |head: &[u8]| -> Result<Option<u64>, DownloadError> {
        sniff_image_kind(head).map(|_| Some(MAX_COVER_BYTES)).ok_or_else(|| {
            DownloadError::WrongType("not a JPEG, PNG or WebP image".into())
        })
    };
    let streamed = stream_to_temp(
        body,
        declared,
        &ctx.tmp,
        StreamRules {
            cap: MAX_COVER_BYTES,
            lim: limits,
            budget: &budget,
            cancel: &cancel,
        },
        &mut check,
        &mut |_, _| {},
    )?;
    let limits = cover_limits();
    let bytes = read_bounded(streamed.temp.path(), MAX_COVER_BYTES)
        .map_err(|e| ImportError::new(500, "io", e.to_string()))?;
    let (kind, w, h) = probe(&bytes, &limits).map_err(|e| {
        let status = match e {
            ImageError::TooLarge { .. } => 413,
            ImageError::Unsupported => 415,
            _ => 400,
        };
        ImportError::new(status, "bad_image", image_error_text(&e))
    })?;
    // A header that passes is not enough: the device has to be able to draw it.
    decode(&bytes, &limits)
        .map_err(|e| ImportError::new(415, "bad_image", image_error_text(&e)))?;
    drop(bytes);
    let staged_id = format!("{:032x}", rand::random::<u128>());
    let path = ctx.tmp.join(format!("{STAGED_PREFIX}{staged_id}"));
    let size = streamed.size;
    let temp = streamed.temp.keep();
    if let Err(e) = fs::rename(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(ImportError::new(500, "io", e.to_string()));
    }
    locked(&ctx.store.staged).insert(
        staged_id.clone(),
        StagedCover {
            path,
            root: ctx.root.clone(),
            kind,
            created: Instant::now(),
        },
    );
    Ok(serde_json::json!({
        "staged_id": staged_id,
        "w": w,
        "h": h,
        "bytes": size,
        "kind": kind_name(kind),
        "preview_url": format!("/api/collections/{}/covers/staged/{staged_id}", ctx.id),
    }))
}

// ---------------------------------------------------------------------------
// Commit
// ---------------------------------------------------------------------------

/// One entry of the final list in `PUT …/covers`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OrderEntry {
    /// An image of the list as `GET …/covers` showed it.
    Keep { keep: usize },
    /// An upload from `POST …/covers/staged`.
    Staged { staged: String },
}

/// `PUT /api/collections/:id/covers`.
#[derive(Debug, Clone, Deserialize)]
pub struct PutCovers {
    /// The **final** list, in order. Images not listed are removed.
    pub order: Vec<OrderEntry>,
}

/// Where an entry of the final list comes from.
enum Source {
    /// Already named in the list the document uses; `name` is relative to the `.noudata`.
    InPlace { name: String },
    /// An image elsewhere in the library, copied next to the `.noudata`.
    Copy { from: PathBuf, ext: &'static str },
    Staged { ext: &'static str, from: PathBuf },
}

impl ServerService {
    /// Commit the final cover list of `id` (see the module docs). Holds the service lock for
    /// the whole step, so no rescan can slip between the thumbnails and the `.noudata`.
    pub fn put_covers(&mut self, id: u64, req: PutCovers) -> Result<CoverList, ServerError> {
        if self.quiescing {
            return Err(ServerError::Unavailable("server is shutting down".into()));
        }
        let (mut idx, mut rec) = self.require_record(id)?;
        Self::require_cover_editable(&self.libraries[idx].root, &rec)?;
        if req.order.len() > MAX_COVERS {
            return Err(ServerError::TooLarge(format!(
                "at most {MAX_COVERS} images per item"
            )));
        }
        let root = self.libraries[idx].root.clone();
        let before = self.libraries[idx].writer.covers_of(rec.id).map_err(lib_err)?;
        let protected = self.hidden_indexes(idx, &before);
        let (mut keeps, mut staged_ids): (Vec<usize>, Vec<&str>) = (Vec::new(), Vec::new());
        for e in &req.order {
            match e {
                OrderEntry::Keep { keep } => {
                    if *keep >= before.len() {
                        return Err(ServerError::Invalid(format!("no cover image {keep}")));
                    }
                    if keeps.contains(keep) {
                        return Err(ServerError::Invalid(format!(
                            "cover image {keep} is listed twice"
                        )));
                    }
                    keeps.push(*keep);
                }
                OrderEntry::Staged { staged } => {
                    if staged_ids.contains(&staged.as_str()) {
                        return Err(ServerError::Invalid(
                            "a staged image is listed twice".into(),
                        ));
                    }
                    staged_ids.push(staged);
                }
            }
        }
        // Everything referenced must be a usable image before anything is touched.
        for n in &keeps {
            let (path, meta) = resolve_plain_file(&root, &before[*n].relpath)?;
            self.covers
                .probe_cached(&root, &before[*n].relpath, &path, &meta)
                .map_err(|_| {
                    ServerError::Invalid(format!("cover image {n} is not a usable image"))
                })?;
        }
        self.covers.sweep_expired();
        let staged: HashMap<String, StagedCover> = {
            let all = locked(&self.covers.staged);
            let mut m = HashMap::new();
            for sid in &staged_ids {
                let s = all
                    .get(*sid)
                    .filter(|s| s.root == root && s.path.is_file())
                    .ok_or_else(|| {
                        ServerError::Invalid("a staged image is unknown or has expired".into())
                    })?;
                m.insert((*sid).to_string(), s.clone());
            }
            m
        };

        // A media file without a `.noudata` gets the minimal one first (the existing creator);
        // it is removed again if the commit fails.
        let mut created_noudata: Option<String> = None;
        let mut result_id = None;
        if rec.is_orphan || rec.noudata_relpath.is_empty() {
            let kind = match rec.group {
                LibraryKind::Mix => ImportKind::Mix,
                LibraryKind::Audiobook => ImportKind::Audiobook,
                LibraryKind::Podcast => ImportKind::Podcast,
                _ => ImportKind::Music,
            };
            let made = self.create_noudata_for_orphan(id, kind, None)?;
            created_noudata = Some(made.noudata_relpath.clone());
            result_id = Some(made.id);
            (idx, rec) = self.require_record(made.id)?;
        }
        // Entries the client never saw stay in the list, after the ones it ordered. One that
        // does not sit under the `.noudata`'s folder cannot be named by the document and is
        // left out of the list, but its file is still protected below.
        let noudata_dir = dir_of(&rec.noudata_relpath).to_string();
        let mut order = req.order.clone();
        order.extend(
            protected
                .iter()
                .filter(|n| {
                    let r = &before[**n].relpath;
                    noudata_dir.is_empty()
                        || r.strip_prefix(&noudata_dir).is_some_and(|s| s.starts_with('/'))
                })
                .map(|n| OrderEntry::Keep { keep: *n }),
        );
        let outcome = self.commit_cover_list(idx, &rec, &order, &before, &staged);
        let final_rels = match outcome {
            Ok(v) => v,
            Err(e) => {
                if let Some(rel) = created_noudata {
                    let _ = fs::remove_file(root.join(&rel));
                    let _ = self.forget_indexed(idx, &rel);
                }
                return Err(e);
            }
        };
        // The commit succeeded: the staged files are consumed.
        {
            let mut all = locked(&self.covers.staged);
            for sid in staged.keys() {
                all.remove(sid);
            }
        }
        self.delete_dropped_covers(idx, &rec, &before, &final_rels, &protected);
        let list = self.libraries[idx].writer.covers_of(rec.id).map_err(lib_err)?;
        let (covers, hidden) =
            self.describe_covers(idx, result_id.unwrap_or(id), CoverScope::Collection, &list);
        Ok(CoverList {
            id: result_id,
            covers,
            hidden,
        })
    }

    /// Steps 2..4 of the commit: place files, thumbnails, write the `.noudata`, index it.
    /// Returns the library-relative paths of the final list. Anything that
    /// fails before the `.noudata` is written leaves the library as it was.
    fn commit_cover_list(
        &mut self,
        idx: usize,
        rec: &PlayableRecord,
        order: &[OrderEntry],
        before: &[CoverRef],
        staged: &HashMap<String, StagedCover>,
    ) -> Result<Vec<String>, ServerError> {
        let root = self.libraries[idx].root.clone();
        let rel = rec.noudata_relpath.clone();
        let path = root.join(&rel);
        let dir_rel = dir_of(&rel).to_string();
        let is_release = rec.noudata_type.as_deref() == Some("release");
        let target = match rec.noudata_type.as_deref() {
            Some("looseTrack") | Some("linkTrack") => CoverTarget::Item { position: 1 },
            _ => CoverTarget::Collection,
        };

        // Sources, in the final order.
        let mut sources: Vec<Source> = Vec::new();
        for e in order {
            sources.push(match e {
                OrderEntry::Keep { keep } => {
                    let r = &before[*keep].relpath;
                    let under = if dir_rel.is_empty() {
                        Some(r.as_str())
                    } else {
                        r.strip_prefix(&dir_rel).and_then(|s| s.strip_prefix('/'))
                    };
                    match under {
                        Some(name) => Source::InPlace { name: name.to_string() },
                        None => {
                            let (from, _) = resolve_plain_file(&root, r)?;
                            let ext = ext_of_path(&from)?;
                            Source::Copy { from, ext }
                        }
                    }
                }
                OrderEntry::Staged { staged: sid } => {
                    let s = &staged[sid];
                    Source::Staged {
                        ext: kind_ext(s.kind),
                        from: s.path.clone(),
                    }
                }
            });
        }

        // File names for what is new.
        let mut taken = folder_names(&root, &dir_rel);
        let base = if is_release {
            "cover".to_string()
        } else {
            // Only an album folder's `cover.jpg` is a sibling cover of its own; elsewhere
            // (playlists, loose tracks sharing a folder) it would become every neighbour's.
            let b = format!("{}-cover", stem_of(&rel));
            if b.len() + 16 > plippa_noubin::MAX_SEGMENT_BYTES {
                "cover".to_string()
            } else {
                b
            }
        };
        let mut names: Vec<String> = Vec::new();
        let mut images: Vec<CoverImageSpec> = Vec::new();
        for s in &sources {
            let name = match s {
                Source::InPlace { name } => name.clone(),
                Source::Copy { ext, .. } | Source::Staged { ext, .. } => {
                    let name = free_name(&mut taken, &base, ext);
                    names.push(name.clone());
                    name
                }
            };
            images.push(CoverImageSpec {
                relpath_from_noudata: name,
                alt_text: None,
            });
        }
        let edit = CoverEdit { target, images };

        // The document change is validated on a copy before any file is placed.
        let mut doc = read_noudata(&path)?;
        let rev = FileRevision::of(&path).map_err(ServerError::Io)?;
        apply_cover_edit(&mut doc, &edit).map_err(|e| ServerError::Invalid(e.to_string()))?;

        // Files, then thumbnails.
        let mut tx = Transaction::default();
        let mut thumbs: Vec<ThumbReport> = Vec::new();
        let placed = (|| -> Result<(), ServerError> {
            let mut new_names = names.iter();
            for s in &sources {
                let (src_move, copy_from) = match s {
                    Source::InPlace { .. } => continue,
                    Source::Staged { from, .. } => (Some(from), None),
                    Source::Copy { from, .. } => (None, Some(from)),
                };
                let name = new_names.next().expect("one name per new file");
                let dst_rel = if dir_rel.is_empty() {
                    name.clone()
                } else {
                    format!("{dir_rel}/{name}")
                };
                let fsx = LibraryFs::new(&root).map_err(|e| ServerError::Message(e.to_string()))?;
                let dst = fsx
                    .resolve(Path::new(&dst_rel))
                    .map_err(|e| ServerError::Invalid(e.to_string()))?;
                if let Some(from) = src_move {
                    tx.move_in(from, &dst)
                        .map_err(|e| ServerError::Conflict(e.to_string()))?;
                } else if let Some(from) = copy_from {
                    let bytes = read_bounded(from, MAX_COVER_BYTES)?;
                    if bytes.len() as u64 > MAX_COVER_BYTES {
                        return Err(ServerError::Invalid("cover image is too large".into()));
                    }
                    tx.write_new(&dst, &bytes)
                        .map_err(|e| ServerError::Conflict(e.to_string()))?;
                }
                // A file of this name may have existed before: its thumbnails are not ours.
                remove_thumbnails(&root, &dst_rel);
                thumbs.push(ensure_thumbnails(&root, &dst_rel, &cover_limits())?);
            }
            Ok(())
        })();
        let finish = placed.and_then(|()| {
            write_noudata_guarded(&doc, &path, rev)
                .and_then(|()| self.index_write_through(idx, &rel, None))
        });
        match finish {
            Ok(()) => tx.commit(),
            Err(e) => {
                // Not written (or not indexed): take everything back unless the `.noudata`
                // itself is already the new one.
                if FileRevision::of(&path).ok() == Some(rev) {
                    for t in thumbs.iter().rev() {
                        t.undo(&root);
                    }
                    tx.rollback();
                } else {
                    tx.commit();
                }
                return Err(e);
            }
        }
        Ok(edit
            .images
            .iter()
            .map(|spec| {
                if dir_rel.is_empty() {
                    spec.relpath_from_noudata.clone()
                } else {
                    format!("{dir_rel}/{}", spec.relpath_from_noudata)
                }
            })
            .collect())
    }

    /// D9: delete the images the edit dropped when no `.noudata` references them any more, with
    /// their thumbnails. Best effort and logged: the document is already consistent.
    fn delete_dropped_covers(
        &mut self,
        idx: usize,
        rec: &PlayableRecord,
        before: &[CoverRef],
        kept: &[String],
        protected: &[usize],
    ) {
        let root = self.libraries[idx].root.clone();
        let dir_rel = dir_of(&rec.noudata_relpath).to_string();
        for (n, c) in before.iter().enumerate() {
            // Never delete a file the client could not see.
            if protected.contains(&n) || kept.iter().any(|k| k.eq_ignore_ascii_case(&c.relpath)) {
                continue;
            }
            if !COVER_EXTENSIONS.iter().any(|e| {
                Path::new(&c.relpath)
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case(e))
            }) {
                continue;
            }
            // A guessed `cover.jpg` is shared by every `.noudata` in its folder.
            if c.source == CoverSource::Sibling
                && folder_names(&root, dir_of(&c.relpath))
                    .iter()
                    .filter(|n| n.ends_with(".noudata"))
                    .count()
                    > usize::from(dir_of(&c.relpath) == dir_rel)
            {
                continue;
            }
            match self.libraries[idx].writer.referencing_noudata(&c.relpath) {
                Ok(refs) if refs.is_empty() => {}
                Ok(_) => continue,
                Err(e) => {
                    eprintln!(
                        "plippa-server: keeping {} (cannot tell who references it): {e}",
                        c.relpath
                    );
                    continue;
                }
            }
            match resolve_plain_file(&root, &c.relpath) {
                Ok((path, _)) => {
                    if let Err(e) = fs::remove_file(&path) {
                        eprintln!("plippa-server: cannot delete {}: {e}", c.relpath);
                        continue;
                    }
                    remove_thumbnails(&root, &c.relpath);
                }
                Err(_) => remove_thumbnails(&root, &c.relpath),
            }
        }
    }
}

fn write_noudata_guarded(
    doc: &plippa_noubin::NoudataDocument,
    path: &Path,
    rev: FileRevision,
) -> Result<(), ServerError> {
    plippa_noubin::write_noudata_atomic(doc, path, WriteGuard::Unchanged(rev))
        .map(|_| ())
        .map_err(|e| write_err(e, "the .noudata"))
}

/// Extension of a cover file in `COVER_EXTENSIONS`, as a static string.
fn ext_of_path(p: &Path) -> Result<&'static str, ServerError> {
    let e = p
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match e.as_str() {
        "jpg" | "jpeg" => Ok("jpg"),
        "png" => Ok("png"),
        "webp" => Ok("webp"),
        _ => Err(ServerError::Invalid("not a cover image file".into())),
    }
}

/// `<base>.<ext>`, else `<base>-2.<ext>`, … : the first stem with no cover file of any
/// extension in the folder (`taken`, ASCII-lowercased names). Marks the choice as taken.
fn free_name(taken: &mut HashSet<String>, base: &str, ext: &str) -> String {
    let mut k = 1u32;
    loop {
        let stem = if k == 1 {
            base.to_string()
        } else {
            format!("{base}-{k}")
        };
        let clash = COVER_EXTENSIONS
            .iter()
            .any(|e| taken.contains(&format!("{}.{e}", stem.to_ascii_lowercase())));
        if !clash {
            let name = format!("{stem}.{ext}");
            taken.insert(name.to_ascii_lowercase());
            return name;
        }
        k += 1;
    }
}

// ---------------------------------------------------------------------------
// Backfill worker
// ---------------------------------------------------------------------------

/// Images that should have thumbnails: every effective cover of every playable, plus the
/// primary sibling candidate of its folder. Skips ones known complete or known broken at their
/// current identity.
fn plan_backfill(root: &Path, store: &CoverStore) -> Vec<String> {
    let Ok(reader) = LibraryReader::open(&LibraryRoot::new(root)) else {
        return Vec::new();
    };
    let Ok(records) = reader.list_playables() else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut add = |rel: String| {
        if seen.insert(rel.clone()) {
            out.push(rel);
        }
    };
    for rec in records {
        for c in reader.covers_of(rec.id).unwrap_or_default() {
            add(c.relpath);
        }
        if !rec.noudata_relpath.is_empty() {
            let dir = root.join(dir_of(&rec.noudata_relpath));
            if let Some(found) = sibling_candidates(&dir) {
                if let Ok(rel) = path_relative_posix(root, &found) {
                    add(rel);
                }
            }
        }
    }
    let b = locked(&store.backfill);
    out.retain(|rel| {
        let Ok(meta) = fs::symlink_metadata(root.join(rel)) else {
            return false;
        };
        let stamp = stamp_of(&meta);
        let key = (root.to_path_buf(), rel.clone());
        b.done.get(&key) != Some(&stamp) && b.failed.get(&key) != Some(&stamp)
    });
    out
}

/// Make the thumbnails of one image. Returns true when pixels were decoded (the caller then
/// pauses before the next job).
fn backfill_one(store: &CoverStore, root: &Path, rel: &str) -> bool {
    let Ok((_, meta)) = resolve_plain_file(root, rel) else {
        return false;
    };
    let stamp = stamp_of(&meta);
    let key = (root.to_path_buf(), rel.to_string());
    match ensure_thumbnails(root, rel, &cover_limits()) {
        Ok(report) => {
            locked(&store.backfill).done.insert(key, stamp);
            !report.written.is_empty()
        }
        Err(e) => {
            let first = locked(&store.backfill).failed.insert(key, stamp).is_none();
            if first {
                eprintln!("plippa-server: no thumbnails for {rel}: {e}");
            }
            true
        }
    }
}

/// Whether the worker should replan now: the index changed or the last plan is old, and the
/// engine is not playing (the plan reads the stick that serves audio; it stays due until then).
fn replan_due(planned_gen: Option<u64>, age: Duration, generation: u64, playing: bool) -> bool {
    !playing && (planned_gen != Some(generation) || age > BACKFILL_REPLAN)
}

/// Spawn the thumbnail backfill thread: low priority, one image at a time with a pause
/// between images, idle while the engine plays, stopped by `stop` (quiesce) before the volume
/// goes away. Returns the handle so shutdown can join it.
pub fn spawn_cover_worker(
    service: Arc<Mutex<ServerService>>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("plippa-covers".into())
        .spawn(move || {
            crate::scanner::lower_thread_priority();
            let mut queue: VecDeque<(PathBuf, String)> = VecDeque::new();
            let mut planned_gen: Option<u64> = None;
            let mut planned_at = Instant::now();
            loop {
                let state = match service.lock() {
                    Ok(mut s) => s.cover_worker_state(),
                    Err(p) => p.into_inner().cover_worker_state(),
                };
                if state.quiescing || stop.load(Ordering::Relaxed) {
                    return;
                }
                // A replan walks the whole library: postponed (not skipped) while the engine plays.
                if replan_due(planned_gen, planned_at.elapsed(), state.generation, state.playing) {
                    planned_gen = Some(state.generation);
                    planned_at = Instant::now();
                    for root in &state.roots {
                        for rel in plan_backfill(root, &state.store) {
                            let job = (root.clone(), rel);
                            if !queue.contains(&job) {
                                queue.push_back(job);
                            }
                        }
                    }
                }
                for job in locked(&state.store.backfill).nudged.drain(..) {
                    if !queue.contains(&job) {
                        queue.push_front(job);
                    }
                }
                let pause = match queue.pop_front() {
                    Some((root, rel)) if !state.playing => {
                        if !state.roots.contains(&root) {
                            Duration::ZERO
                        } else if backfill_one(&state.store, &root, &rel) {
                            BACKFILL_GAP
                        } else {
                            Duration::ZERO
                        }
                    }
                    Some(job) => {
                        queue.push_front(job);
                        Duration::from_secs(2)
                    }
                    None => Duration::from_secs(2),
                };
                if crate::scanner::sleep_unless_stopped(&stop, pause.max(Duration::from_millis(5)))
                {
                    return;
                }
            }
        })
        .expect("spawn cover thread")
}

// ---------------------------------------------------------------------------
// Embedded art (ART_04)
// ---------------------------------------------------------------------------

/// Pictures read from one audio file.
const MAX_EMBEDDED_PICS: usize = 8;
/// Largest single embedded picture kept.
const MAX_EMBEDDED_PIC_BYTES: usize = MAX_COVER_BYTES as usize;
/// Picture bytes kept from one audio file.
const MAX_EMBEDDED_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// One picture found in an audio file's tag. Its type is decided by its bytes, never by the
/// mime type or picture role the tag claims.
#[derive(Debug, Clone)]
pub struct EmbeddedPic {
    pub bytes: Vec<u8>,
}

/// What [`extract_embedded`] found: usable pictures in display order plus how many it left out.
#[derive(Debug, Clone, Default)]
pub struct EmbeddedScan {
    pub pics: Vec<EmbeddedPic>,
    /// Pictures dropped for not being JPEG / PNG / WebP, being over a limit, empty, or beyond the
    /// per-file caps.
    pub skipped: usize,
}

fn picture_rank(t: lofty::picture::PictureType) -> u8 {
    use lofty::picture::PictureType;
    match t {
        PictureType::CoverFront => 0,
        PictureType::CoverBack => 1,
        _ => 2,
    }
}

/// Read the pictures embedded in a staged audio file (ID3v2 `APIC`, FLAC `PICTURE`, MP4 `covr`,
/// Ogg / Opus `METADATA_BLOCK_PICTURE`, RIFF `id3 `). The container is found from the file's
/// bytes, never its name, and only the tag regions are read (no audio properties). Order:
/// front cover, back cover, then the rest as they appear. A picture that is not JPEG / PNG /
/// WebP by its magic, is empty, or fails the cover limits is counted in `skipped`, as is
/// everything past 8 pictures, 16 MiB each or 64 MiB per file. A file lofty cannot read yields
/// an empty scan: art is never a reason to refuse an upload.
pub fn extract_embedded(path: &Path) -> EmbeddedScan {
    use lofty::config::ParseOptions;
    use lofty::file::TaggedFileExt;
    use lofty::probe::Probe;
    let mut scan = EmbeddedScan::default();
    let Ok(file) = fs::File::open(path) else {
        return scan;
    };
    let probe = match Probe::new(std::io::BufReader::new(file)).guess_file_type() {
        Ok(p) => p.options(ParseOptions::new().read_properties(false)),
        Err(_) => return scan,
    };
    let Ok(tagged) = probe.read() else {
        return scan;
    };
    let mut found: Vec<(u8, &lofty::picture::Picture)> = Vec::new();
    for tag in tagged.tags() {
        for pic in tag.pictures() {
            found.push((picture_rank(pic.pic_type()), pic));
        }
    }
    found.sort_by_key(|(rank, _)| *rank); // stable: file order within a rank
    let limits = cover_limits();
    let mut total = 0usize;
    for (_, pic) in found {
        let data = pic.data();
        let usable = !data.is_empty()
            && data.len() <= MAX_EMBEDDED_PIC_BYTES
            && sniff_image_kind(data).is_some()
            && probe_header(data, &limits);
        if !usable
            || scan.pics.len() >= MAX_EMBEDDED_PICS
            || total + data.len() > MAX_EMBEDDED_TOTAL_BYTES
        {
            scan.skipped += 1;
            continue;
        }
        total += data.len();
        scan.pics.push(EmbeddedPic {
            bytes: data.to_vec(),
        });
    }
    scan
}

fn probe_header(bytes: &[u8], limits: &ImageLimits) -> bool {
    probe(bytes, limits).is_ok()
}

impl CoverStore {
    /// Register an image file that already sits at its final staging place (an import session's
    /// art folder) so [`ServerService::put_covers`] can commit it by id.
    pub(crate) fn register_file(&self, root: &Path, path: &Path, kind: ImageKind) -> String {
        let id = format!("{:032x}", rand::random::<u128>());
        locked(&self.staged).insert(
            id.clone(),
            StagedCover {
                path: path.to_path_buf(),
                root: root.to_path_buf(),
                kind,
                created: Instant::now(),
            },
        );
        id
    }

    /// Forget registrations without touching the files (a failed commit keeps them in place).
    pub(crate) fn unregister(&self, ids: &[String]) {
        let mut all = locked(&self.staged);
        for id in ids {
            all.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::RecordingIpc;
    use plippa_image::Rgb8;

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 90]);
            }
        }
        encode_jpeg_fit(&Rgb8 { w, h, px }, w.max(h)).unwrap()
    }

    /// An uncompressed (stored-deflate) RGB PNG: valid for any decoder, no encoder needed.
    fn png(w: u32, h: u32) -> Vec<u8> {
        fn crc(data: &[u8]) -> u32 {
            let mut c = !0u32;
            for &b in data {
                c ^= u32::from(b);
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc(&body).to_be_bytes());
        }
        let mut raw = Vec::new();
        for y in 0..h {
            raw.push(0);
            for x in 0..w {
                raw.extend_from_slice(&[(x % 251) as u8, (y % 241) as u8, 40]);
            }
        }
        let mut z = vec![0x78, 0x01];
        let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
        for (i, b) in blocks.iter().enumerate() {
            z.push(u8::from(i + 1 == blocks.len()));
            z.extend_from_slice(&(b.len() as u16).to_le_bytes());
            z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
            z.extend_from_slice(b);
        }
        let (mut a, mut b2) = (1u32, 0u32);
        for &v in &raw {
            a = (a + u32::from(v)) % 65_521;
            b2 = (b2 + a) % 65_521;
        }
        z.extend_from_slice(&((b2 << 16) | a).to_be_bytes());
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &z);
        chunk(&mut out, b"IEND", &[]);
        out
    }

    fn dims(bytes: &[u8]) -> (u32, u32) {
        let (_, w, h) = probe(bytes, &cover_limits()).unwrap();
        (w, h)
    }

    #[test]
    fn tiers_cover_what_the_image_is_larger_than_plus_the_one_that_holds_it() {
        let px = |side| tiers_for(side).iter().map(|t| t.px()).collect::<Vec<_>>();
        assert_eq!(px(100), [128]);
        assert_eq!(px(128), [128]);
        assert_eq!(px(129), [128, 512]);
        assert_eq!(px(512), [128, 512]);
        assert_eq!(px(700), [128, 512, 1024]);
        assert_eq!(px(1024), [128, 512, 1024]);
        assert_eq!(px(5000), [128, 512, 1024]);
    }

    #[test]
    fn thumbnail_names_agree_with_the_download_planner() {
        use crate::unite::images::{THUMB_EXTS, THUMB_TIERS};
        let ours: Vec<u32> = ThumbTier::ALL.iter().map(|t| t.px()).collect();
        assert_eq!(ours, THUMB_TIERS);
        assert!(THUMB_EXTS.contains(&"jpg"));
        assert_eq!(
            thumbnail_relpath("Music/A/Alb/cover.png", ThumbTier::T512, "jpg"),
            "Music/A/Alb/thumbnails/cover-512.jpg"
        );
    }

    #[test]
    fn new_images_never_take_an_existing_name_of_any_extension() {
        let mut taken: HashSet<String> = ["cover.png".to_string(), "cover-2.jpg".to_string()].into();
        assert_eq!(free_name(&mut taken, "cover", "jpg"), "cover-3.jpg");
        assert_eq!(free_name(&mut taken, "cover", "webp"), "cover-4.webp");
        let mut empty = HashSet::new();
        assert_eq!(free_name(&mut empty, "cover", "jpg"), "cover.jpg");
        assert_eq!(free_name(&mut empty, "cover", "png"), "cover-2.png");
    }

    #[test]
    fn ensure_thumbnails_keeps_the_aspect_ratio_never_upscales_and_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        fs::create_dir_all(root.join("Alb")).unwrap();
        let big = jpeg(700, 500);
        fs::write(root.join("Alb/cover.jpg"), &big).unwrap();
        fs::write(root.join("Alb/tiny.png"), png(60, 40)).unwrap();

        let r = ensure_thumbnails(root, "Alb/cover.jpg", &cover_limits()).unwrap();
        assert_eq!(r.written.len(), 3, "{:?}", r.written);
        let d = |tier: u32| {
            dims(&fs::read(root.join(format!("Alb/thumbnails/cover-{tier}.jpg"))).unwrap())
        };
        assert_eq!(d(128), (128, 91));
        assert_eq!(d(512), (512, 366));
        assert_eq!(d(1024), (700, 500), "the 1024 tier holds it at its own size");
        assert_eq!(fs::read(root.join("Alb/cover.jpg")).unwrap(), big, "original untouched");
        assert!(ensure_thumbnails(root, "Alb/cover.jpg", &cover_limits())
            .unwrap()
            .written
            .is_empty());

        // PNG source, smaller than every tier: only the 128 tier, at its own size.
        let r = ensure_thumbnails(root, "Alb/tiny.png", &cover_limits()).unwrap();
        assert_eq!(r.written, ["Alb/thumbnails/tiny-128.jpg"]);
        assert_eq!(
            dims(&fs::read(root.join("Alb/thumbnails/tiny-128.jpg")).unwrap()),
            (60, 40)
        );
        // An existing capitalised folder counts, in either spelling.
        fs::create_dir_all(root.join("Alb/Thumbnails")).unwrap();
        fs::write(root.join("Alb/Thumbnails/mix-128.jpg"), jpeg(10, 10)).unwrap();
        assert!(tier_exists(root, "Alb/mix.jpg", ThumbTier::T128));
    }

    #[test]
    fn ensure_thumbnails_refuses_what_it_cannot_decode_and_writes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let mut cut = jpeg(300, 300);
        cut.truncate(cut.len() / 3);
        fs::write(root.join("cut.jpg"), &cut).unwrap();
        fs::write(root.join("text.png"), b"hello, not a picture at all").unwrap();
        for name in ["cut.jpg", "text.png"] {
            assert!(ensure_thumbnails(root, name, &cover_limits()).is_err(), "{name}");
        }
        assert!(!root.join("thumbnails").exists());
        // A symlink out of the library is not followed.
        std::os::unix::fs::symlink("/etc/hostname", root.join("link.jpg")).unwrap();
        assert!(ensure_thumbnails(root, "link.jpg", &cover_limits()).is_err());
    }

    fn library_with_dropped_covers() -> (tempfile::TempDir, crate::ServerService) {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        for (dir, key) in [("Good", "Good.noubin.AAA111"), ("Bad", "Bad.noubin.BBB222")] {
            let d = root.join("Music").join(dir);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("01.mp3"), b"").unwrap();
            fs::write(
                d.join(format!("_{dir}.noudata")),
                format!(
                    r#"{{"noudataVersion":"0.4.0","noudataType":"release","noubinKey":"{key}",
                    "noubinKeyNormalised":"{}","playableItemTitle":"{dir}",
                    "mediaList":[{{"mediaTitle":"One","localFilePaths":["01.mp3"]}}]}}"#,
                    key.to_lowercase()
                ),
            )
            .unwrap();
        }
        fs::write(root.join("Music/Good/cover.jpg"), jpeg(600, 400)).unwrap();
        let mut cut = jpeg(300, 300);
        cut.truncate(cut.len() / 3);
        fs::write(root.join("Music/Bad/cover.jpg"), cut).unwrap();
        let svc = crate::ServerService::start_with_roots(
            vec![root.to_path_buf()],
            Box::new(RecordingIpc::new()),
            crate::ServerService::default_mock_associate(),
        );
        (t, svc)
    }

    #[test]
    fn backfill_makes_thumbnails_for_dropped_art_once_and_remembers_a_broken_file() {
        let (t, svc) = library_with_dropped_covers();
        let root = t.path();
        let store = svc.cover_store();
        let first = plan_backfill(root, &store);
        assert_eq!(first.len(), 2, "{first:?}");

        for rel in &first {
            backfill_one(&store, root, rel);
        }
        assert!(root.join("Music/Good/thumbnails/cover-128.jpg").is_file());
        assert!(root.join("Music/Good/thumbnails/cover-512.jpg").is_file());
        assert!(!root.join("Music/Bad/thumbnails").exists());
        // Nothing is planned again: one is complete, the broken one is not retried.
        assert!(plan_backfill(root, &store).is_empty());
        // A changed file is looked at again.
        let mut fixed = jpeg(300, 300);
        fixed.extend_from_slice(b"\0");
        fs::write(root.join("Music/Bad/cover.jpg"), fixed).unwrap();
        assert_eq!(plan_backfill(root, &store), ["Music/Bad/cover.jpg"]);
    }

    fn tagged_mp3(dir: &Path, name: &str, pics: Vec<lofty::picture::Picture>) -> PathBuf {
        use lofty::config::WriteOptions;
        use lofty::tag::{Tag, TagExt, TagType};
        let p = dir.join(name);
        let mut frames = Vec::new();
        for _ in 0..8 {
            let mut f = vec![0u8; 417];
            f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            frames.extend(f);
        }
        fs::write(&p, frames).unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        for pic in pics {
            tag.push_picture(pic);
        }
        tag.save_to_path(&p, WriteOptions::default()).unwrap();
        p
    }

    fn pic(t: lofty::picture::PictureType, data: Vec<u8>) -> lofty::picture::Picture {
        lofty::picture::Picture::new_unchecked(t, Some(lofty::picture::MimeType::Jpeg), None, data)
    }

    #[test]
    fn embedded_pictures_come_front_first_then_back_then_the_rest_whatever_the_file_name() {
        use lofty::picture::PictureType::{Artist, CoverBack, CoverFront};
        let t = tempfile::tempdir().unwrap();
        let (a, b, c) = (jpeg(40, 40), jpeg(50, 50), jpeg(60, 60));
        // the name says nothing: the container is found from the bytes
        let p = tagged_mp3(
            t.path(),
            "staged.bin",
            vec![pic(Artist, c.clone()), pic(CoverBack, b.clone()), pic(CoverFront, a.clone())],
        );
        let scan = extract_embedded(&p);
        let got: Vec<&Vec<u8>> = scan.pics.iter().map(|p| &p.bytes).collect();
        assert_eq!(got, [&a, &b, &c]);
        assert_eq!(scan.skipped, 0);
    }

    #[test]
    fn embedded_pictures_that_are_not_jpeg_png_webp_or_exceed_the_count_cap_are_skipped() {
        use lofty::picture::PictureType::Other;
        let t = tempfile::tempdir().unwrap();
        let mut pics = vec![pic(Other, b"GIF89a....".to_vec()), pic(Other, Vec::new())];
        pics.extend((0..10).map(|i| pic(Other, jpeg(32 + i, 32))));
        let scan = extract_embedded(&tagged_mp3(t.path(), "x.mp3", pics));
        assert_eq!(scan.pics.len(), MAX_EMBEDDED_PICS);
        assert_eq!(scan.skipped, 2 + 2);
        // not an audio file at all, or missing: an empty scan, never an error
        fs::write(t.path().join("junk.mp3"), b"hello").unwrap();
        assert!(extract_embedded(&t.path().join("junk.mp3")).pics.is_empty());
        assert!(extract_embedded(&t.path().join("nope.mp3")).pics.is_empty());
    }


    #[test]
    fn a_replan_waits_for_the_end_of_playback_and_then_runs() {
        let old = BACKFILL_REPLAN + Duration::from_secs(1);
        // Index changed or plan aged out: due, unless the engine plays.
        assert!(replan_due(None, Duration::ZERO, 1, false));
        assert!(replan_due(Some(1), old, 1, false));
        assert!(!replan_due(Some(1), Duration::ZERO, 1, false));
        assert!(!replan_due(None, old, 1, true));
        assert!(!replan_due(Some(1), old, 2, true));
        // Nothing was marked as planned while playing, so it is still due afterwards.
        assert!(replan_due(Some(1), old, 2, false));
    }

    #[test]
    fn media_missing_and_read_only_collections_are_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.noudata"), b"{}").unwrap();
        let mut rec = PlayableRecord {
            id: 1,
            noudata_relpath: "a.noudata".into(),
            noubin_key_normalised: None,
            title: None,
            media_relpaths: Vec::new(),
            kind: plippa_noubin::LibraryKind::Album,
            group: plippa_noubin::LibraryKind::Album,
            noudata_type: None,
            release_profile: None,
            is_dynamic: false,
            is_orphan: false,
            primary_artist: None,
            duration_ms: None,
            cover_relpath: None,
            cover_count: 0,
            refers_to_relpath: None,
            has_navigable_cues: false,
            media_missing: false,
        };
        assert!(ServerService::require_cover_editable(tmp.path(), &rec).is_ok());
        rec.media_missing = true;
        assert!(matches!(
            ServerService::require_cover_editable(tmp.path(), &rec),
            Err(ServerError::Conflict(_))
        ));
        rec.media_missing = false;
        let path = tmp.path().join("a.noudata");
        let mut perm = std::fs::metadata(&path).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&path, perm).unwrap();
        assert!(matches!(
            ServerService::require_cover_editable(tmp.path(), &rec),
            Err(ServerError::Conflict(_))
        ));
        rec.noudata_relpath.clear();
        assert!(ServerService::require_cover_editable(tmp.path(), &rec).is_ok());
    }
}
