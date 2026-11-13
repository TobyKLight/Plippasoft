//! Cover images of a playable: which ones apply, where their thumbnails are, and the single
//! writer the portal uses to change them (Noubin §1.27; cover-art series decisions D3, D4, D5).
//!
//! The JSON shape (`coverImage`, `localCoverImage`, `itemCoverImage`) is the standard's. This
//! module only decides *which entries win* and keeps the rules in one place so the portal, the
//! device and the index agree:
//!
//! - **Collection view** ([`effective_collection_covers`]): `localCoverImage` if it names any
//!   local file, else the artist's `coverImage`, else the primary sibling candidate
//!   (`cover.jpg`, `folder.jpg`, ...). A `localCoverImage` whose `images` array is present and
//!   empty means *no cover* and hides everything below it (that is how the portal "deletes"
//!   artist art without touching the artist's data). A loose or link track puts its
//!   `itemCoverImage` first; a track or playlist that refers to a release shows the release's
//!   list unless it has its own.
//! - **Playing item** ([`effective_item_covers`]): the item's `itemCoverImage`, then the art of
//!   the item's own release, then the collection being played.
//!
//! Everything here is local: nothing is fetched, remote-only entries are skipped. Whether a
//! file is really an image is decided at serve time from its first bytes, not here. Image names
//! are resolved against the `.noudata`'s folder through the same normaliser as the index scan
//! and must stay inside the library.

use crate::error::{CoverEditError, NoubinError};
use crate::index::try_resolve_rel;
use crate::noudata::{effective_type, NoudataDocument};
use crate::noudata_types::{CoverImage, ImageEntry, NoudataType};
use crate::sanitise::{is_safe_relative_path, sanitise_title, MAX_SEGMENT_BYTES};
use crate::sanitise_doc::MAX_IMAGES;
use std::path::{Path, PathBuf};

/// Sibling file names that count as album art when nothing references an image, in order of
/// preference (only the first existing one is used).
pub const ART_CANDIDATES: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "cover.webp",
    "folder.jpg",
    "folder.png",
];

/// Image extensions accepted as covers (Noubin §1.27), lowercase.
pub const COVER_EXTENSIONS: &[&str] = &["jpg", "jpeg", "webp", "png"];

/// Directory (next to the image) holding generated thumbnails; `Thumbnails` is read as a
/// fallback because a stick edited on another OS may carry that spelling.
pub const THUMBNAIL_DIR: &str = "thumbnails";
const THUMBNAIL_DIR_FALLBACK: &str = "Thumbnails";

/// Where a cover entry came from (the D4 tiers plus the dropped-file guess).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoverSource {
    /// `mediaList[].itemCoverImage`.
    Item,
    /// `localPlaybackData.localCoverImage` (set by the user).
    Local,
    /// `releaseData.coverImage` (the artist's).
    Release,
    /// A file such as `cover.jpg` next to the `.noudata` that no document references.
    Sibling,
}

impl CoverSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Local => "local",
            Self::Release => "release",
            Self::Sibling => "sibling",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Self::Item, Self::Local, Self::Release, Self::Sibling]
            .into_iter()
            .find(|k| k.as_str() == s)
    }
}

/// One cover image of a playable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverRef {
    /// Relative to the library root: `/`-separated, normalised, no `..`.
    pub relpath: String,
    pub source: CoverSource,
    pub alt_text: Option<String>,
    pub is_image_of_noubin: bool,
}

// ---------------------------------------------------------------------------
// Collecting declared entries (shared with the index scan)
// ---------------------------------------------------------------------------

/// A declared cover entry with its library-relative path (existence not checked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Declared {
    pub relpath: String,
    pub source: CoverSource,
    pub alt_text: Option<String>,
    pub is_image_of_noubin: bool,
}

impl Declared {
    fn into_ref(self) -> CoverRef {
        CoverRef {
            relpath: self.relpath,
            source: self.source,
            alt_text: self.alt_text,
            is_image_of_noubin: self.is_image_of_noubin,
        }
    }
}

/// The cover entries a document declares for itself (before any inheritance or sibling guess).
#[derive(Debug, Default)]
pub(crate) struct OwnCovers {
    pub list: Vec<Declared>,
    /// An explicitly empty user-level list: nothing may appear, not even inherited art.
    pub hidden: bool,
}

pub(crate) fn has_cover_extension(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| COVER_EXTENSIONS.iter().any(|c| c.eq_ignore_ascii_case(e)))
}

/// Resolve an image file name written in a `.noudata` (relative to its folder `dir`) to a
/// library-relative path. `None` when the name is empty, absolute, has a NUL or backslash,
/// is not a cover image extension, leaves the library, or fails the library path guard.
pub(crate) fn image_relpath(root: &Path, dir: &Path, name: &str) -> Option<String> {
    if name.is_empty() || name.contains(['\0', '\\']) || name.starts_with('/') {
        return None;
    }
    if !has_cover_extension(name) {
        return None;
    }
    let rel = try_resolve_rel(root, dir, name)?;
    is_safe_relative_path(&rel).then_some(rel)
}

fn declared_entries(
    root: &Path,
    dir: &Path,
    cover: &CoverImage,
    source: CoverSource,
) -> Vec<Declared> {
    let mut out: Vec<Declared> = Vec::new();
    for e in &cover.images {
        let Some(name) = e.image_local_filename.as_deref() else {
            continue; // remote-only: no network here
        };
        let Some(relpath) = image_relpath(root, dir, name) else {
            continue;
        };
        if out.iter().any(|d| d.relpath == relpath) {
            continue;
        }
        out.push(Declared {
            relpath,
            source,
            alt_text: e.alt_text.clone(),
            is_image_of_noubin: e.is_image_of_noubin.unwrap_or(false),
        });
    }
    out
}

/// The document's own tiers, in D4 order, without looking at the file system. `ty` is the
/// document's effective type (a loose or link track puts its item cover first).
pub(crate) fn own_covers(
    root: &Path,
    dir: &Path,
    doc: &NoudataDocument,
    ty: NoudataType,
) -> OwnCovers {
    if matches!(ty, NoudataType::LooseTrack | NoudataType::LinkTrack) {
        if let Some(c) = doc.media_list.first().and_then(|i| i.item_cover_image.as_ref()) {
            if c.images.is_empty() {
                return OwnCovers {
                    list: Vec::new(),
                    hidden: true,
                };
            }
            let list = declared_entries(root, dir, c, CoverSource::Item);
            if !list.is_empty() {
                return OwnCovers {
                    list,
                    hidden: false,
                };
            }
        }
    }
    if let Some(c) = doc
        .local_playback_data
        .as_ref()
        .and_then(|l| l.local_cover_image.as_ref())
    {
        if c.images.is_empty() {
            return OwnCovers {
                list: Vec::new(),
                hidden: true,
            };
        }
        let list = declared_entries(root, dir, c, CoverSource::Local);
        if !list.is_empty() {
            return OwnCovers {
                list,
                hidden: false,
            };
        }
    }
    let list = doc
        .release_data
        .as_ref()
        .and_then(|r| r.cover_image.as_ref())
        .map(|c| declared_entries(root, dir, c, CoverSource::Release))
        .unwrap_or_default();
    OwnCovers {
        list,
        hidden: false,
    }
}

// ---------------------------------------------------------------------------
// Public resolvers
// ---------------------------------------------------------------------------

/// A release document together with its library-relative path.
#[derive(Clone, Copy)]
pub struct ReleaseDoc<'a> {
    pub doc: &'a NoudataDocument,
    pub relpath: &'a str,
}

fn dir_of(root: &Path, doc_relpath: &str) -> PathBuf {
    root.join(doc_relpath)
        .parent()
        .map_or_else(|| root.to_path_buf(), Path::to_path_buf)
}

fn exists_in(root: &Path, relpath: &str) -> bool {
    root.join(relpath).is_file()
}

fn push_unique(out: &mut Vec<CoverRef>, r: CoverRef) {
    if !out.iter().any(|o| o.relpath == r.relpath) {
        out.push(r);
    }
}

fn existing_refs(root: &Path, list: Vec<Declared>) -> Vec<CoverRef> {
    let mut out = Vec::new();
    for d in list {
        if exists_in(root, &d.relpath) {
            push_unique(&mut out, d.into_ref());
        }
    }
    out
}

/// The primary sibling candidate of `dir` (an absolute directory): the first of
/// [`ART_CANDIDATES`] that is a file.
pub fn sibling_candidates(dir: &Path) -> Option<PathBuf> {
    ART_CANDIDATES
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

/// [`sibling_candidates`] for a library: `dir_relpath` is the folder relative to `root` (empty
/// for the root); the result is the image's library-relative path.
pub(crate) fn sibling_relpath(root: &Path, dir_relpath: &str) -> Option<String> {
    let dir = if dir_relpath.is_empty() {
        root.to_path_buf()
    } else {
        root.join(dir_relpath)
    };
    let found = sibling_candidates(&dir)?;
    let name = found.file_name()?.to_str()?;
    Some(if dir_relpath.is_empty() {
        name.to_string()
    } else {
        format!("{dir_relpath}/{name}")
    })
}

fn sibling_ref(root: &Path, doc_relpath: &str) -> Option<CoverRef> {
    let dir_rel = doc_relpath.rsplit_once('/').map_or("", |(d, _)| d);
    Some(CoverRef {
        relpath: sibling_relpath(root, dir_rel)?,
        source: CoverSource::Sibling,
        alt_text: None,
        is_image_of_noubin: false,
    })
}

/// Library-relative path of the document `refer` (`referToReleaseNoudata`) names.
fn referred_relpath(root: &Path, dir: &Path, refer: &str) -> Option<String> {
    if refer.is_empty() || refer.contains(['\0', '\\']) || refer.starts_with('/') {
        return None;
    }
    try_resolve_rel(root, dir, refer)
}

fn collection_covers_inner(
    root: &Path,
    doc: &NoudataDocument,
    doc_relpath: &str,
    resolver: &dyn Fn(&str) -> Option<NoudataDocument>,
    follow_refer: bool,
) -> Vec<CoverRef> {
    let dir = dir_of(root, doc_relpath);
    let ty = effective_type(doc, |r| resolver(r));
    let own = own_covers(root, &dir, doc, ty);
    if own.hidden {
        return Vec::new();
    }
    if !own.list.is_empty() {
        return existing_refs(root, own.list);
    }
    if follow_refer && doc.has_refer() {
        if let Some(refer) = doc.refer_to_release_noudata.as_deref() {
            let target = resolver(refer).filter(|d| d.release_data.is_some());
            if let (Some(target), Some(rel)) = (target, referred_relpath(root, &dir, refer)) {
                return collection_covers_inner(root, &target, &rel, resolver, false);
            }
        }
    }
    sibling_ref(root, doc_relpath).into_iter().collect()
}

/// The covers a *collection view* shows, in order (D4): the user's list if it names any local
/// file, else the artist's, else the primary sibling candidate; a track or playlist that refers
/// to a release shows the release's list unless it has its own. An explicitly empty
/// `localCoverImage` yields nothing. Entries whose file does not exist are dropped; remote-only
/// entries are skipped. `doc_relpath` is the `.noudata`'s path relative to `root`; `resolver`
/// maps a `referToReleaseNoudata` value to the parsed release (see
/// [`crate::fs_release_resolver`]).
pub fn effective_collection_covers(
    root: &Path,
    doc: &NoudataDocument,
    doc_relpath: &str,
    resolver: &dyn Fn(&str) -> Option<NoudataDocument>,
) -> Vec<CoverRef> {
    collection_covers_inner(root, doc, doc_relpath, resolver, true)
}

/// The covers for the *playing item* `item_position` (0-based index into `mediaList`), in
/// order (D4): the item's own `itemCoverImage`, then the art of the item's own release, then
/// the collection being played. `item_release` is the release that owns the item when the
/// caller knows it (a playlist entry resolved through the index); without it a document that
/// refers to a release uses that one, and any other item belongs to `doc` itself. An
/// explicitly empty `itemCoverImage` yields nothing.
pub fn effective_item_covers(
    root: &Path,
    doc: &NoudataDocument,
    doc_relpath: &str,
    item_position: usize,
    item_release: Option<ReleaseDoc<'_>>,
    resolver: &dyn Fn(&str) -> Option<NoudataDocument>,
) -> Vec<CoverRef> {
    let dir = dir_of(root, doc_relpath);
    if let Some(c) = doc
        .media_list
        .get(item_position)
        .and_then(|i| i.item_cover_image.as_ref())
    {
        if c.images.is_empty() {
            return Vec::new();
        }
        let own = existing_refs(root, declared_entries(root, &dir, c, CoverSource::Item));
        if !own.is_empty() {
            return own;
        }
    }
    let mut out: Vec<CoverRef> = Vec::new();
    match item_release {
        Some(r) => {
            out = collection_covers_inner(root, r.doc, r.relpath, resolver, false);
        }
        None => {
            if let Some(refer) = doc.refer_to_release_noudata.as_deref().filter(|r| !r.is_empty())
            {
                let target = resolver(refer).filter(|d| d.release_data.is_some());
                if let (Some(target), Some(rel)) = (target, referred_relpath(root, &dir, refer)) {
                    out = collection_covers_inner(root, &target, &rel, resolver, false);
                }
            }
        }
    }
    if out.is_empty() {
        for c in collection_covers_inner(root, doc, doc_relpath, resolver, true) {
            push_unique(&mut out, c);
        }
    }
    out
}

/// True when any cover list of `doc` (`coverImage`, `localCoverImage`, any `itemCoverImage`)
/// names the library-relative image `image_relpath` (compared without regard to ASCII case,
/// as the sticks are FAT). Used to decide whether an image file may be deleted (D9).
pub(crate) fn document_references(
    root: &Path,
    doc: &NoudataDocument,
    doc_relpath: &str,
    target: &str,
) -> bool {
    let dir = dir_of(root, doc_relpath);
    let lists = doc
        .release_data
        .as_ref()
        .and_then(|r| r.cover_image.as_ref())
        .into_iter()
        .chain(
            doc.local_playback_data
                .as_ref()
                .and_then(|l| l.local_cover_image.as_ref()),
        )
        .chain(doc.media_list.iter().filter_map(|i| i.item_cover_image.as_ref()));
    lists
        .flat_map(|c| c.images.iter())
        .filter_map(|e| e.image_local_filename.as_deref())
        .filter_map(|name| image_relpath(root, &dir, name))
        .any(|rel| rel.eq_ignore_ascii_case(target))
}

// ---------------------------------------------------------------------------
// Thumbnails (D5)
// ---------------------------------------------------------------------------

/// Thumbnail sizes (longest side, px).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThumbTier {
    T128 = 128,
    T512 = 512,
    T1024 = 1024,
}

impl ThumbTier {
    pub const ALL: [ThumbTier; 3] = [Self::T128, Self::T512, Self::T1024];

    pub fn px(self) -> u32 {
        self as u32
    }
}

/// A thumbnail file that exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbRef {
    /// Library-relative path.
    pub relpath: String,
    pub tier: ThumbTier,
}

fn split_image(image_relpath: &str) -> Option<(&str, &str)> {
    if !is_safe_relative_path(image_relpath) {
        return None;
    }
    let (dir, file) = image_relpath
        .rsplit_once('/')
        .map_or(("", image_relpath), |(d, f)| (d, f));
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    if stem.is_empty() {
        return None;
    }
    Some((dir, stem))
}

fn thumb_path(dir: &str, folder: &str, stem: &str, tier: ThumbTier, ext: &str) -> String {
    let name = format!("{stem}-{}.{ext}", tier.px());
    if dir.is_empty() {
        format!("{folder}/{name}")
    } else {
        format!("{dir}/{folder}/{name}")
    }
}

/// Where the generated thumbnail of `image_relpath` for `tier` lives (the `thumbnails`
/// spelling): `<image dir>/thumbnails/<stem>-<tier>.<ext>`.
pub fn thumbnail_relpath(image_relpath: &str, tier: ThumbTier, ext: &str) -> String {
    let (dir, file) = image_relpath
        .rsplit_once('/')
        .map_or(("", image_relpath), |(d, f)| (d, f));
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    thumb_path(dir, THUMBNAIL_DIR, stem, tier, ext)
}

/// The best existing thumbnail for an image shown at about `want_px`: the smallest tier that
/// is at least `want_px`, else the largest below it, else `None`. For each tier `thumbnails/`
/// is looked at before `Thumbnails/`, JPEG before WebP.
pub fn resolve_thumbnail(root: &Path, image_relpath: &str, want_px: u32) -> Option<ThumbRef> {
    let (dir, stem) = split_image(image_relpath)?;
    let find = |tier: ThumbTier| -> Option<String> {
        for folder in [THUMBNAIL_DIR, THUMBNAIL_DIR_FALLBACK] {
            for ext in ["jpg", "webp"] {
                let rel = thumb_path(dir, folder, stem, tier, ext);
                if exists_in(root, &rel) {
                    return Some(rel);
                }
            }
        }
        None
    };
    let present: Vec<(ThumbTier, String)> = ThumbTier::ALL
        .into_iter()
        .filter_map(|t| find(t).map(|r| (t, r)))
        .collect();
    let pick = present
        .iter()
        .find(|(t, _)| t.px() >= want_px)
        .or_else(|| present.last())?;
    Some(ThumbRef {
        relpath: pick.1.clone(),
        tier: pick.0,
    })
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Which cover list an edit replaces (D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverTarget {
    /// `localPlaybackData.localCoverImage` (releases, playlists, mixes, ...).
    Collection,
    /// `mediaList[position - 1].itemCoverImage` (1-based; a loose track's primary place).
    Item { position: u32 },
}

/// One image of the final list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverImageSpec {
    /// File name relative to the `.noudata`'s folder.
    pub relpath_from_noudata: String,
    /// `Some(text)` sets the alt text (`Some("")` clears it); `None` keeps the alt text the
    /// image already has in the document.
    pub alt_text: Option<String>,
}

/// Replace one cover list with exactly `images`, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverEdit {
    pub target: CoverTarget,
    pub images: Vec<CoverImageSpec>,
}

/// Alt text of a spec after sanitising: `None` keep what the document has, `Some(None)`
/// clear it, `Some(Some(text))` set it.
type AltChange = Option<Option<String>>;

fn validate_spec(spec: &CoverImageSpec) -> Result<(String, AltChange), CoverEditError> {
    let name = &spec.relpath_from_noudata;
    if !is_safe_relative_path(name) || name.len() > 4 * MAX_SEGMENT_BYTES {
        return Err(CoverEditError::UnsafePath { path: name.clone() });
    }
    if !has_cover_extension(name) {
        return Err(CoverEditError::UnsupportedExtension { path: name.clone() });
    }
    let alt = spec
        .alt_text
        .as_deref()
        .map(|a| Some(sanitise_title(a)).filter(|a| !a.is_empty()));
    Ok((name.clone(), alt))
}

/// Set the cover list `edit.target` names to exactly `edit.images`.
///
/// An empty list is written as an explicit `{"images": []}`, which means "no cover" and keeps
/// the artist's `coverImage` (or inherited art) from reappearing. Entries are matched to the
/// images already in the target list (or, for a collection, the artist's list) by file name so
/// their `isImageOfNoubin`, `imageWebURL` and unknown keys survive; entries that are not
/// re-listed (including remote-only ones) are dropped. A duplicate file name keeps its first
/// place. The artist's `releaseData.coverImage` is never modified.
///
/// On error the document is unchanged. Only mutates `doc`: writing the file is the caller's.
pub fn apply_cover_edit(doc: &mut NoudataDocument, edit: &CoverEdit) -> Result<(), NoubinError> {
    if edit.images.len() > MAX_IMAGES {
        return Err(CoverEditError::TooManyImages {
            max: MAX_IMAGES,
            got: edit.images.len(),
        }
        .into());
    }
    let mut specs: Vec<(String, AltChange)> = Vec::with_capacity(edit.images.len());
    for s in &edit.images {
        let (name, alt) = validate_spec(s)?;
        if specs.iter().any(|(n, _)| *n == name) {
            continue;
        }
        specs.push((name, alt));
    }
    if let CoverTarget::Item { position } = edit.target {
        let idx = (position as usize).checked_sub(1);
        if idx.is_none_or(|i| i >= doc.media_list.len()) {
            return Err(CoverEditError::NoSuchItem { position }.into());
        }
    }

    // Entries to reuse: the target list's own, then (for a collection) the artist's.
    let (existing, fallback): (Option<&CoverImage>, Option<&CoverImage>) = match edit.target {
        CoverTarget::Collection => (
            doc.local_playback_data
                .as_ref()
                .and_then(|l| l.local_cover_image.as_ref()),
            doc.release_data.as_ref().and_then(|r| r.cover_image.as_ref()),
        ),
        CoverTarget::Item { position } => (
            doc.media_list[position as usize - 1].item_cover_image.as_ref(),
            None,
        ),
    };
    let find = |name: &str| -> Option<&ImageEntry> {
        [existing, fallback]
            .into_iter()
            .flatten()
            .flat_map(|c| c.images.iter())
            .find(|e| e.image_local_filename.as_deref() == Some(name))
    };
    let images: Vec<ImageEntry> = specs
        .into_iter()
        .map(|(name, alt)| match find(&name) {
            Some(old) => {
                let mut e = old.clone();
                if let Some(change) = alt {
                    e.alt_text = change;
                }
                e
            }
            None => ImageEntry {
                image_local_filename: Some(name),
                alt_text: alt.flatten(),
                ..ImageEntry::default()
            },
        })
        .collect();
    let extra = existing.map(|c| c.extra.clone()).unwrap_or_default();
    let new = CoverImage { images, extra };

    match edit.target {
        CoverTarget::Collection => {
            doc.local_playback_data
                .get_or_insert_with(Default::default)
                .local_cover_image = Some(new);
        }
        CoverTarget::Item { position } => {
            doc.media_list[position as usize - 1].item_cover_image = Some(new);
        }
    }
    Ok(())
}
