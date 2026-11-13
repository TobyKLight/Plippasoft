//! NB_03: collections API — read albums/mixes/audiobooks/podcasts/tracks/playlists, and
//! create/edit/delete **playlists** with write-through to `.noudata`.
//!
//! Every playlist mutation is: read the `.noudata` from disk → modify → atomic write
//! (temp + rename, guarded by the file's mtime+size) → `index_write_through` (index row +
//! generation bump + `LibraryIndexUpdated`). Items are addressed by **position + path**,
//! never by index id (ids are derived and change when the file is renamed / rescanned).

use super::{
    encode_playable_id, path_relative_posix, read_noudata, serialize_u64_as_string, ServerError,
    ServerService,
};
use plippa_noubin::{
    new_playlist_document, playlist_entry, relative_media_path, unique_playlist_path,
    validate_playlist_title, write_noudata_atomic, AtomicWriteError, FileRevision, LibraryError,
    LibraryKind, NoudataDocument, PlayableRecord, WriteGuard, PLAYLISTS_DIR,
};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// `createdByUser` written to playlists created through the API when the client gives none.
const DEFAULT_CREATED_BY: &str = "Plippa";

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// A playable id as sent by a client: JSON number or decimal string (ids exceed 2^53).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdArg(pub u64);

impl<'de> Deserialize<'de> for IdArg {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Num(u64),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Num(n) => Ok(IdArg(n)),
            Raw::Str(s) => s
                .trim()
                .parse()
                .map(IdArg)
                .map_err(|_| serde::de::Error::custom(format!("bad playable id {s:?}"))),
        }
    }
}

/// A track to put in a playlist: a playable id, optionally one `mediaList` item of it
/// (`item` = position from `GET /api/collections/:id`). Without `item`, every item of the
/// playable is added in order (a per-track file / orphan has exactly one; an album has all).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TrackRef {
    Id(IdArg),
    Item {
        track_id: IdArg,
        #[serde(default)]
        item: Option<u32>,
    },
}

impl TrackRef {
    fn parts(&self) -> (u64, Option<u32>) {
        match self {
            TrackRef::Id(id) => (id.0, None),
            TrackRef::Item { track_id, item } => (track_id.0, *item),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatePlaylist {
    pub title: String,
    #[serde(default)]
    pub tracks: Vec<TrackRef>,
    /// `localPlaybackData.userNotes.createdByUser` (default `"Plippa"`).
    #[serde(default)]
    pub created_by: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RenamePlaylist {
    #[serde(default)]
    pub title: Option<String>,
    /// `revision` the client last saw (else last-write-wins with an internal race check).
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddPlaylistItems {
    pub track_id: IdArg,
    #[serde(default)]
    pub item: Option<u32>,
    /// Insert before this current position (`0..=len`); absent = append.
    #[serde(default)]
    pub position: Option<usize>,
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReorderPlaylist {
    /// A permutation of `0..len`: entry `i` of the new order is the item that is now at
    /// `positions[i]`.
    pub positions: Vec<usize>,
    #[serde(default)]
    pub revision: Option<String>,
}

/// Row of `GET /api/collections`.
#[derive(Debug, Clone, Serialize)]
pub struct CollectionSummary {
    /// Namespaced playable id as a decimal string.
    #[serde(serialize_with = "serialize_u64_as_string")]
    pub id: u64,
    /// CLASSIFICATION: what the file is. A loose or link track is always `track`, even when
    /// its profile is mix/audiobook/podcast; only releases are album/mix/audiobook/podcast.
    pub kind: LibraryKind,
    /// GROUPING: the library tab that lists it (`?kind=` filters by this). Equal to `kind`
    /// except for a loose mix/audiobook/podcast track, grouped under Mixtapes/Audiobooks/
    /// Podcasts by its `playableItemProfile`.
    pub group: LibraryKind,
    /// Effective `noudataType` (`release`, `userPlaylist`, `looseTrack`, `linkTrack`); absent
    /// for an orphan media file that has no `.noudata` yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noudata_type: Option<String>,
    pub title: String,
    pub artist: Option<String>,
    pub item_count: u32,
    /// Sum of item lengths; `null` until every length is known.
    pub duration_ms: Option<u64>,
    /// URL of the primary cover image at 512 px (`/api/collections/:id/cover`) when there is one.
    pub cover: Option<String>,
    /// URL of the primary image at 128 px, for list rows and cards.
    pub cover_thumb: Option<String>,
    /// Cover images the collection shows (at least 1 when `cover` is set). The exact, servable
    /// list is `GET /api/collections/:id/covers`.
    pub cover_count: u32,
    /// False when at least one item's media file is missing.
    pub available: bool,
    pub is_dynamic: bool,
    pub is_orphan: bool,
    /// Items can be added / removed / renamed (a non-dynamic playlist). Releases keep their
    /// metadata authoritative, so this stays false for them.
    pub editable: bool,
    /// `.noudata` `mediaList` can be reordered: a non-dynamic, non-orphan playlist **or** a
    /// release (the album case). No media file is renamed by a reorder.
    pub reorderable: bool,
    pub has_navigable_cues: bool,
    /// Volume label when more than one library is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_label: Option<String>,
}

/// Row of `GET /api/tracks`: one track of the item-level library views (Tracks, Mixes).
#[derive(Debug, Clone, Serialize)]
pub struct TrackSummary {
    /// Namespaced id of the playable (release / file) the track belongs to, as a decimal string.
    #[serde(serialize_with = "serialize_u64_as_string")]
    pub id: u64,
    /// 0-based `mediaList` position inside that playable (play it with
    /// `POST /api/collections/:id/play {item}`).
    pub position: u32,
    pub title: String,
    pub artist: Option<String>,
    /// Release / file the track belongs to.
    pub collection_title: String,
    /// Items in that release (1 for a loose track or a media file without metadata).
    pub collection_items: u32,
    pub duration_ms: Option<u64>,
    pub available: bool,
    pub is_orphan: bool,
    /// 128 px thumbnail URL of this track's own art, only when it differs from the art of the
    /// collection it belongs to (so the same image is not repeated on every row).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_thumb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub library_label: Option<String>,
}

/// One entry of a collection, metadata resolved through the authoritative release `.noudata`.
#[derive(Debug, Clone, Serialize)]
pub struct CollectionItem {
    /// Index in the `.noudata` `mediaList` — with `media_path`, the item's identity.
    pub position: u32,
    pub title: String,
    pub artist: Option<String>,
    pub duration_ms: Option<u64>,
    /// Media file relative to the library root.
    pub media_path: String,
    /// False when the media file is missing (the entry is still listed).
    pub available: bool,
    /// For playlist entries: the release / track that supplied title + artist.
    #[serde(
        serialize_with = "serialize_opt_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub source_id: Option<u64>,
    /// 128 px thumbnail URL of this item's own art, only when it differs from the collection's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_thumb: Option<String>,
    pub has_navigable_cues: bool,
    pub cue_count: u32,
}

/// `GET /api/collections/:id` and the result of every playlist mutation.
#[derive(Debug, Clone, Serialize)]
pub struct CollectionDetail {
    #[serde(flatten)]
    pub summary: CollectionSummary,
    pub noudata_relpath: String,
    /// Fingerprint of the `.noudata` on disk; send it back as `revision` to get a 409 when
    /// the file changed in between. `null` for orphans.
    pub revision: Option<String>,
    /// `createdByUser` of a playlist.
    pub created_by: Option<String>,
    pub items: Vec<CollectionItem>,
}

/// One row of `GET /api/kinds` (a library tab: counted by GROUP).
#[derive(Debug, Clone, Serialize)]
pub struct KindCount {
    pub kind: LibraryKind,
    pub count: u64,
    pub has_content: bool,
}

fn serialize_opt_id<S: serde::Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(v) => s.serialize_str(&v.to_string()),
        None => s.serialize_none(),
    }
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Everything a playlist edit needs, captured (and checked) before the file is read.
pub(super) struct PlaylistCtx {
    root: PathBuf,
    /// The editable `.noudata`, absolute.
    path: PathBuf,
    /// The editable `.noudata`, library-relative.
    rel: String,
    lib_idx: usize,
    rev: FileRevision,
    /// A release (album): reordering is allowed, renaming/adding/removing is not.
    release: bool,
}

pub(super) fn lib_err(e: LibraryError) -> ServerError {
    match e {
        LibraryError::StorageVanished { path, detail } => ServerError::Unavailable(format!(
            "Media storage vanished at {}: {detail}",
            path.display()
        )),
        other => other.into(),
    }
}

/// Persist an edited playlist. Without a rename target: guarded atomic write in place. With
/// one (a title change, §4.5 names the file after the title): the file is renamed **first**
/// (atomic, same folder; the rename keeps mtime+size, so the revision guard still holds) and
/// the new content is written at the new name; if that write fails the rename is undone, so
/// a failure never leaves the old file name carrying the new title (or vice versa).
/// `fault` is a test seam called at `"before_rename"` and `"after_rename"`.
fn commit_playlist_edit(
    doc: &NoudataDocument,
    path: &Path,
    rev: FileRevision,
    target: Option<&Path>,
    fault: &mut dyn FnMut(&'static str) -> std::io::Result<()>,
) -> Result<PathBuf, ServerError> {
    let Some(target) = target else {
        write_noudata_atomic(doc, path, WriteGuard::Unchanged(rev))
            .map_err(|e| write_err(e, "the playlist"))?;
        return Ok(path.to_path_buf());
    };
    if FileRevision::of(path).ok() != Some(rev) {
        return Err(ServerError::Conflict(
            "the playlist changed since it was read; reload and retry".into(),
        ));
    }
    fault("before_rename")?;
    fs::rename(path, target)?;
    let written = fault("after_rename")
        .map_err(ServerError::from)
        .and_then(|()| {
            write_noudata_atomic(doc, target, WriteGuard::Unchanged(rev))
                .map_err(|e| write_err(e, "the playlist"))
        });
    if let Err(e) = written {
        // Put the file back under its old name; the old content is still in it.
        let _ = fs::rename(target, path);
        return Err(e);
    }
    Ok(target.to_path_buf())
}

/// Validate a reorder request: `positions` is `new position -> old position` and must be a
/// permutation of `0..len`. Nothing is written when it is not.
fn checked_permutation(positions: &[usize], len: usize) -> Result<Vec<usize>, ServerError> {
    let mut seen = vec![false; len];
    let ok = positions.len() == len
        && positions
            .iter()
            .all(|&p| p < len && !std::mem::replace(&mut seen[p], true));
    if !ok {
        return Err(ServerError::Invalid(format!(
            "positions must be a permutation of 0..{len} (each current position once)"
        )));
    }
    Ok(positions.to_vec())
}

/// Apply `permutation` (`new position -> old position`) to `mediaList` and carry the long-form
/// resume point with its item.
fn apply_reorder(doc: &mut NoudataDocument, permutation: &[usize]) {
    let mut old: Vec<Option<_>> = std::mem::take(&mut doc.media_list)
        .into_iter()
        .map(Some)
        .collect();
    doc.media_list = permutation
        .iter()
        .map(|&p| old[p].take().expect("permutation checked"))
        .collect();
    remap_resume_after_reorder(doc, permutation);
}

/// `localPlaybackData.resumeAtItem` is 1-indexed. After a reorder it must still point at the
/// same item; that item's `itemLocalPlaybackData.resumeFromTime` travels with it because whole
/// `mediaList` entries are permuted. Out-of-range values are left untouched.
fn remap_resume_after_reorder(doc: &mut NoudataDocument, permutation: &[usize]) {
    let Some(local) = doc.local_playback_data.as_mut() else {
        return;
    };
    let Some(at) = local.resume_at_item.filter(|&n| n >= 1) else {
        return;
    };
    let old_pos = (at - 1) as usize;
    if let Some(new_pos) = permutation.iter().position(|&p| p == old_pos) {
        local.resume_at_item = Some(new_pos as u64 + 1);
    }
}

pub(super) fn write_err(e: AtomicWriteError, what: &str) -> ServerError {
    match e {
        AtomicWriteError::Conflict => ServerError::Conflict(format!(
            "{what} was changed by someone else while it was being edited; reload and retry"
        )),
        AtomicWriteError::Io(e) => ServerError::Io(e),
        AtomicWriteError::Noudata(e) => ServerError::Noudata(e),
    }
}

/// Lexically resolve `p` (as written in a `.noudata` in `dir`) to a library-relative path.
pub(super) fn resolve_in(dir: &str, p: &str) -> String {
    let mut out: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

pub(super) fn rel_dir(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

impl ServerService {
    fn require_libraries(&self) -> Result<(), ServerError> {
        if self.libraries.is_empty() {
            return Err(ServerError::Unavailable(
                self.warning
                    .clone()
                    .unwrap_or_else(|| "No media storage available".into()),
            ));
        }
        Ok(())
    }

    fn summary_of(
        &self,
        lib_idx: usize,
        rec: PlayableRecord,
        item_count: u32,
    ) -> CollectionSummary {
        let lib = &self.libraries[lib_idx];
        let id = encode_playable_id(&lib.root, rec.id);
        let cover_count = self.shown_cover_count(lib_idx, &rec);
        let title = rec
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| {
                let src = if rec.noudata_relpath.is_empty() {
                    rec.media_relpaths.first().cloned().unwrap_or_default()
                } else {
                    rec.noudata_relpath.clone()
                };
                Path::new(&src)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Untitled")
                    .to_string()
            });
        CollectionSummary {
            id,
            kind: rec.kind,
            group: rec.group,
            noudata_type: rec.noudata_type.clone(),
            title,
            artist: rec.primary_artist.clone(),
            item_count,
            duration_ms: rec.duration_ms,
            cover: (cover_count > 0).then(|| format!("/api/collections/{id}/cover")),
            cover_thumb: (cover_count > 0).then(|| Self::cover_thumb_url(id)),
            cover_count,
            available: !rec.media_missing,
            is_dynamic: rec.is_dynamic,
            is_orphan: rec.is_orphan,
            editable: rec.kind == LibraryKind::Playlist && !rec.is_dynamic && !rec.is_orphan,
            reorderable: !rec.is_dynamic
                && !rec.is_orphan
                && (rec.kind == LibraryKind::Playlist
                    || rec.noudata_type.as_deref() == Some("release")),
            has_navigable_cues: rec.has_navigable_cues,
            library_label: (self.libraries.len() > 1).then(|| lib.label.clone()),
        }
    }

    /// `GET /api/collections?kind=…` — all kinds when `kind` is `None`. Sorted by title. A
    /// `kind` filter selects a library VIEW (tab, see `libraryUIHeirarchy.md`); for the
    /// item-level views (`track`, `mix`) it lists the playables that contain the tracks, the
    /// flat track list is `GET /api/tracks`.
    pub fn list_collections(
        &mut self,
        kind: Option<LibraryKind>,
    ) -> Result<Vec<CollectionSummary>, ServerError> {
        self.require_libraries()?;
        let mut out = Vec::new();
        for idx in 0..self.libraries.len() {
            let w = &self.libraries[idx].writer;
            let records = match kind {
                Some(k) => w.list_by_view(k),
                None => w.list_playables(),
            }
            .map_err(lib_err)?;
            let counts = w.item_counts().map_err(lib_err)?;
            for rec in records {
                let n = counts.get(&rec.id).copied().unwrap_or(0);
                out.push(self.summary_of(idx, rec, n));
            }
        }
        out.sort_by(|a, b| {
            a.title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then(a.id.cmp(&b.id))
        });
        Ok(out)
    }

    /// `GET /api/tracks?view=track|mix` — the flat A-Z track list of an item-level view.
    pub fn list_tracks(&mut self, view: LibraryKind) -> Result<Vec<TrackSummary>, ServerError> {
        self.require_libraries()?;
        let mut out = Vec::new();
        for idx in 0..self.libraries.len() {
            let lib = &self.libraries[idx];
            let rows = lib.writer.list_view_tracks(view).map_err(lib_err)?;
            // Items of a one-item playable always share its art; only multi-item playables
            // (albums, mixes) can hold an item with art of its own.
            let mut differing: HashMap<i64, HashMap<u32, String>> = HashMap::new();
            for row in rows.iter().filter(|r| r.collection_items > 1) {
                if differing.contains_key(&row.playable_id) {
                    continue;
                }
                let found = match lib.writer.lookup_by_id(row.playable_id).map_err(lib_err)? {
                    Some(rec) => {
                        let id = encode_playable_id(&lib.root, rec.id);
                        self.differing_item_thumbs(idx, id, &rec, &HashMap::new())
                    }
                    None => HashMap::new(),
                };
                differing.insert(row.playable_id, found);
            }
            for row in rows {
                let cover_thumb = differing
                    .get(&row.playable_id)
                    .and_then(|m| m.get(&row.position))
                    .cloned();
                out.push(TrackSummary {
                    id: encode_playable_id(&lib.root, row.playable_id),
                    position: row.position,
                    title: row.title,
                    artist: row.artist,
                    collection_title: row.collection_title,
                    collection_items: row.collection_items,
                    duration_ms: row.duration_ms,
                    available: row.available,
                    is_orphan: row.is_orphan,
                    cover_thumb,
                    library_label: (self.libraries.len() > 1).then(|| lib.label.clone()),
                });
            }
        }
        out.sort_by(|a, b| {
            a.title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then(a.id.cmp(&b.id))
                .then(a.position.cmp(&b.position))
        });
        Ok(out)
    }

    /// `GET /api/kinds` — every view with its count (`has_content` drives conditional tabs).
    /// Tracks and Mixes count tracks, the others count playables.
    pub fn collection_kinds(&mut self) -> Result<Vec<KindCount>, ServerError> {
        self.require_libraries()?;
        let mut totals: HashMap<LibraryKind, u64> = HashMap::new();
        for lib in &self.libraries {
            for (k, n) in lib.writer.view_counts().map_err(lib_err)? {
                *totals.entry(k).or_default() += n;
            }
        }
        Ok(LibraryKind::ALL
            .iter()
            .map(|&kind| {
                let count = totals.get(&kind).copied().unwrap_or(0);
                KindCount {
                    kind,
                    count,
                    has_content: count > 0,
                }
            })
            .collect())
    }

    /// `GET /api/collections/:id` — header + ordered items.
    ///
    /// If the `.noudata` on disk is newer than the index (edited by hand / another tool),
    /// the index is refreshed first so the answer matches the `revision` it returns.
    pub fn get_collection(&mut self, id: u64) -> Result<CollectionDetail, ServerError> {
        let (mut idx, mut rec) = self.require_record(id)?;
        if !rec.is_orphan && !rec.noudata_relpath.is_empty() {
            let path = self.libraries[idx].root.join(&rec.noudata_relpath);
            if let Ok(disk) = FileRevision::of(&path) {
                let indexed = self.libraries[idx]
                    .writer
                    .indexed_fingerprint(rec.id)
                    .map_err(lib_err)?;
                if indexed != Some((disk.mtime_ns, disk.size)) {
                    let _ = self.lazy_rescan();
                    (idx, rec) = self.require_record(id)?;
                }
            }
        }
        self.detail_of(idx, rec)
    }

    fn detail_of(&self, idx: usize, rec: PlayableRecord) -> Result<CollectionDetail, ServerError> {
        let lib = &self.libraries[idx];
        let records = lib.writer.items_of(rec.id).map_err(lib_err)?;
        let id = encode_playable_id(&lib.root, rec.id);
        let resolved: HashMap<u32, i64> = records
            .iter()
            .filter_map(|i| i.resolved_from.map(|f| (i.position, f)))
            .collect();
        let thumbs = self.differing_item_thumbs(idx, id, &rec, &resolved);
        let items: Vec<CollectionItem> = records
            .into_iter()
            .map(|i| CollectionItem {
                position: i.position,
                title: i.title,
                artist: i.artist,
                duration_ms: i.duration_ms,
                media_path: i.media_relpath,
                available: i.available,
                source_id: i.resolved_from.map(|f| encode_playable_id(&lib.root, f)),
                has_navigable_cues: i.has_navigable_cues,
                cue_count: i.cue_count,
                cover_thumb: thumbs.get(&i.position).cloned(),
            })
            .collect();
        let (revision, created_by) = if rec.is_orphan || rec.noudata_relpath.is_empty() {
            (None, None)
        } else {
            let path = lib.root.join(&rec.noudata_relpath);
            let revision = FileRevision::of(&path).ok().map(|r| r.token());
            let created_by = if rec.kind == LibraryKind::Playlist {
                read_noudata(&path)
                    .ok()
                    .and_then(|d| d.local_playback_data?.user_notes?.created_by_user)
            } else {
                None
            };
            (revision, created_by)
        };
        let noudata_relpath = rec.noudata_relpath.clone();
        let summary = self.summary_of(idx, rec, items.len() as u32);
        Ok(CollectionDetail {
            summary,
            noudata_relpath,
            revision,
            created_by,
            items,
        })
    }

    // ----- playlist mutations ------------------------------------------------

    /// Media file paths (library-relative) for one [`TrackRef`], which must live on
    /// library `lib_idx` — playlists reference media relative to their own file, and a
    /// path into another volume cannot be represented.
    fn resolve_track_media(
        &mut self,
        lib_idx: usize,
        tref: &TrackRef,
    ) -> Result<Vec<String>, ServerError> {
        let (id, item) = tref.parts();
        let (idx, rec) = self.require_record(id)?;
        if idx != lib_idx {
            return Err(ServerError::Invalid(format!(
                "track {id} is on another volume ({}); a playlist can only reference media on \
                 the same volume as the playlist file ({})",
                self.libraries[idx].label, self.libraries[lib_idx].label
            )));
        }
        if rec.is_dynamic {
            return Err(ServerError::Invalid(format!(
                "{id} is a dynamic (search) playlist and has no tracks to add"
            )));
        }
        let items = self.libraries[idx]
            .writer
            .items_of(rec.id)
            .map_err(lib_err)?;
        let chosen: Vec<_> = match item {
            Some(pos) => vec![items.iter().find(|i| i.position == pos).ok_or_else(|| {
                ServerError::Invalid(format!("{id} has no item at position {pos}"))
            })?],
            None => items.iter().collect(),
        };
        if chosen.is_empty() {
            return Err(ServerError::Invalid(format!("{id} has no tracks to add")));
        }
        Ok(chosen.iter().map(|i| i.media_relpath.clone()).collect())
    }

    fn playlist_entries(
        media: &[String],
        playlist_dir: &str,
    ) -> Result<Vec<plippa_noubin::MediaListItem>, ServerError> {
        media
            .iter()
            .map(|m| {
                relative_media_path(playlist_dir, m)
                    .map(|p| playlist_entry(&p))
                    .ok_or_else(|| {
                        ServerError::Invalid(format!(
                            "media path {m:?} is outside the library and cannot be referenced \
                             from a playlist"
                        ))
                    })
            })
            .collect()
    }

    /// Find the record for a library-relative `.noudata` path in the index. Callers run this
    /// after a write-through ingest, so no rescan is involved.
    fn record_at(&self, lib_idx: usize, rel: &str) -> Result<PlayableRecord, ServerError> {
        let root = self.libraries[lib_idx].root.clone();
        let records = self.libraries[lib_idx]
            .writer
            .list_by_kind(LibraryKind::Playlist)
            .map_err(lib_err)?;
        super::find_by_rel(&records, rel, &root)
            .cloned()
            .ok_or_else(|| {
                ServerError::Message(format!(
                    "playlist {rel} was written but is not in the index (write-through)"
                ))
            })
    }

    /// Index the file just written (write-through, no scan heuristic) and return the playlist
    /// stored at `rel`. `old_rel` is the file a rename replaced, if any.
    fn finish_playlist_write(
        &mut self,
        lib_idx: usize,
        rel: &str,
        old_rel: Option<&str>,
    ) -> Result<CollectionDetail, ServerError> {
        let root = self.libraries[lib_idx].root.clone();
        self.index_write_through(lib_idx, rel, old_rel)?;
        // Roots can shift if a stick vanished during the write.
        let lib_idx = self
            .libraries
            .iter()
            .position(|l| super::paths_same(&l.root, &root))
            .ok_or_else(|| ServerError::Unavailable("media storage vanished".into()))?;
        let rec = self.record_at(lib_idx, rel)?;
        self.detail_of(lib_idx, rec)
    }

    /// `POST /api/playlists` — create `Playlists/_<title>.noudata` on the primary library.
    pub fn create_playlist(
        &mut self,
        req: CreatePlaylist,
    ) -> Result<CollectionDetail, ServerError> {
        self.require_libraries()?;
        let title = validate_playlist_title(&req.title).map_err(ServerError::Invalid)?;
        // Caller text: shared sanitiser (control/bidi/zero-width stripped, NFC) and the spec's
        // 256-character name cap; nothing usable left -> the default author.
        let created_by = plippa_noubin::sanitise_line(
            req.created_by.as_deref().unwrap_or(""),
            plippa_noubin::MAX_TITLE_CHARS,
        );
        let created_by = if created_by.is_empty() {
            DEFAULT_CREATED_BY
        } else {
            created_by.as_str()
        };
        let lib_idx = 0;
        let mut media = Vec::new();
        for t in &req.tracks {
            media.extend(self.resolve_track_media(lib_idx, t)?);
        }
        let mut doc = new_playlist_document(&title, created_by);
        doc.media_list = Self::playlist_entries(&media, PLAYLISTS_DIR)?;

        let root = self.libraries[lib_idx].root.clone();
        let dir = root.join(PLAYLISTS_DIR);
        fs::create_dir_all(&dir)?;
        let path = unique_playlist_path(&dir, &title, None);
        write_noudata_atomic(&doc, &path, WriteGuard::MustNotExist)
            .map_err(|e| write_err(e, "the new playlist file"))?;
        let rel = path_relative_posix(&root, &path)?;
        self.finish_playlist_write(lib_idx, &rel, None)
    }

    /// Resolve + check a `.noudata` whose `mediaList` may be edited. `revision` = what the
    /// client last saw. A playlist is always allowed (except a dynamic one for item edits); a
    /// release only when `releases` is set (reordering).
    fn mutable_ctx(
        &mut self,
        id: u64,
        revision: Option<&str>,
        items_edit: bool,
        releases: bool,
    ) -> Result<PlaylistCtx, ServerError> {
        let (lib_idx, rec) = self.require_record(id)?;
        let playlist = rec.kind == LibraryKind::Playlist && !rec.is_orphan;
        let release = releases
            && !rec.is_orphan
            && rec.noudata_type.as_deref() == Some("release");
        if rec.noudata_relpath.is_empty() || !(playlist || release) {
            let hint = if releases {
                "only playlists and releases can be reordered"
            } else {
                "only playlists can be edited (release metadata is authoritative)"
            };
            return Err(ServerError::Invalid(format!(
                "{id} is a {} — {hint}",
                rec.kind.as_str()
            )));
        }
        if items_edit && rec.is_dynamic {
            return Err(ServerError::Invalid(
                "dynamic (search) playlists have no editable item list".into(),
            ));
        }
        let root = self.libraries[lib_idx].root.clone();
        let path = root.join(&rec.noudata_relpath);
        let rev = FileRevision::of(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ServerError::NotFound("the playlist file no longer exists".into())
            } else {
                e.into()
            }
        })?;
        if let Some(seen) = revision.filter(|r| !r.is_empty()) {
            if seen != rev.token() {
                return Err(ServerError::Conflict(
                    "the playlist changed since you loaded it; reload and retry".into(),
                ));
            }
        }
        Ok(PlaylistCtx {
            root,
            path,
            rel: rec.noudata_relpath,
            lib_idx,
            rev,
            release,
        })
    }

    /// Read → `edit` → atomic write. Returns the refreshed detail.
    pub(super) fn edit_playlist(
        &mut self,
        id: u64,
        revision: Option<&str>,
        items_edit: bool,
        edit: impl FnOnce(&mut NoudataDocument, &PlaylistCtx) -> Result<Option<String>, ServerError>,
    ) -> Result<CollectionDetail, ServerError> {
        let ctx = self.mutable_ctx(id, revision, items_edit, false)?;
        let mut doc = read_noudata(&ctx.path)?;
        // Re-check: the file must still be what `playlist_ctx` fingerprinted.
        if FileRevision::of(&ctx.path).ok() != Some(ctx.rev) {
            return Err(ServerError::Conflict(
                "the playlist changed while it was being read; retry".into(),
            ));
        }
        let new_title = edit(&mut doc, &ctx)?;

        // Rename per §4.5 (`_<title>.noudata`), staying in the same folder.
        let target = new_title.and_then(|title| {
            let dir = ctx.path.parent().unwrap_or(&ctx.root);
            let t = unique_playlist_path(dir, &title, Some(&ctx.path));
            (t != ctx.path).then_some(t)
        });
        let written = commit_playlist_edit(&doc, &ctx.path, ctx.rev, target.as_deref(), &mut |_| Ok(()))?;
        let rel = path_relative_posix(&ctx.root, &written)?;
        // A rename replaces the old file: drop its row in the same write-through.
        let old_rel = (rel != ctx.rel).then(|| ctx.rel.clone());
        self.finish_playlist_write(ctx.lib_idx, &rel, old_rel.as_deref())
    }

    /// `PATCH /api/playlists/:id` `{title}` — rename (the file is renamed too, §4.5, so the
    /// playlist gets a **new id**; use the returned detail).
    pub fn rename_playlist(
        &mut self,
        id: u64,
        req: RenamePlaylist,
    ) -> Result<CollectionDetail, ServerError> {
        let Some(title) = req.title.as_deref() else {
            return Err(ServerError::Invalid(
                "nothing to change (missing title)".into(),
            ));
        };
        let title = validate_playlist_title(title).map_err(ServerError::Invalid)?;
        self.edit_playlist(id, req.revision.as_deref(), false, |doc, _| {
            doc.playable_item_title = Some(title.clone());
            Ok(Some(title))
        })
    }

    /// `DELETE /api/playlists/:id` — removes the playlist file only (never any media).
    pub fn delete_playlist(&mut self, id: u64, revision: Option<&str>) -> Result<(), ServerError> {
        let ctx = self.mutable_ctx(id, revision, false, false)?;
        if FileRevision::of(&ctx.path).ok() != Some(ctx.rev) {
            return Err(ServerError::Conflict(
                "the playlist changed while it was being deleted; retry".into(),
            ));
        }
        fs::remove_file(&ctx.path)?;
        self.forget_indexed(ctx.lib_idx, &ctx.rel)?;
        Ok(())
    }

    /// `POST /api/playlists/:id/items` — append / insert a track (see [`TrackRef`]).
    pub fn add_playlist_items(
        &mut self,
        id: u64,
        req: AddPlaylistItems,
    ) -> Result<CollectionDetail, ServerError> {
        // Appending a playlist to itself would duplicate its own entries (a self-reference the
        // format has no use for), so refuse it before anything is written.
        if req.track_id.0 == id {
            return Err(ServerError::Invalid(
                "a playlist cannot be added to itself".into(),
            ));
        }
        let ctx = self.mutable_ctx(id, req.revision.as_deref(), true, false)?;
        let tref = TrackRef::Item {
            track_id: req.track_id,
            item: req.item,
        };
        let media = self.resolve_track_media(ctx.lib_idx, &tref)?;
        let dir = rel_dir(&ctx.rel).to_string();
        let entries = Self::playlist_entries(&media, &dir)?;
        self.edit_playlist(id, req.revision.as_deref(), true, |doc, _| {
            let at = req.position.unwrap_or(doc.media_list.len());
            if at > doc.media_list.len() {
                return Err(ServerError::Invalid(format!(
                    "position {at} is past the end of the playlist ({} items)",
                    doc.media_list.len()
                )));
            }
            doc.media_list.splice(at..at, entries);
            Ok(None)
        })
    }

    /// `DELETE /api/playlists/:id/items/:position`. `expected_path` (library-relative media
    /// path, or the entry as written in the file) guards against a shifted list: mismatch → 409.
    pub fn remove_playlist_item(
        &mut self,
        id: u64,
        position: usize,
        expected_path: Option<&str>,
        revision: Option<&str>,
    ) -> Result<CollectionDetail, ServerError> {
        self.edit_playlist(id, revision, true, |doc, ctx| {
            let Some(item) = doc.media_list.get(position) else {
                return Err(ServerError::Invalid(format!(
                    "no item at position {position} (playlist has {} items)",
                    doc.media_list.len()
                )));
            };
            if let Some(want) = expected_path.filter(|w| !w.is_empty()) {
                let dir = rel_dir(&ctx.rel);
                let want_norm = resolve_in("", want);
                let matches = item
                    .local_file_paths
                    .iter()
                    .flatten()
                    .any(|p| p == want || resolve_in(dir, p) == want_norm);
                if !matches {
                    return Err(ServerError::Conflict(format!(
                        "item {position} is no longer {want:?}; reload the playlist"
                    )));
                }
            }
            doc.media_list.remove(position);
            Ok(None)
        })
    }

    /// `PUT /api/collections/:id/order` — permute `mediaList` of a playlist **or** a release.
    ///
    /// Reordering a release means reordering the authoritative album file: no media file is
    /// renamed or moved (`mediaTitle` carries the display title). The index re-derives item
    /// positions, `localPlaybackData.resumeAtItem` (1-indexed) is remapped to the item's new
    /// position, and measured item durations survive the permutation (matched by media path).
    /// `PUT /api/playlists/:id/order` keeps working through [`Self::reorder_playlist`].
    pub fn reorder_collection(
        &mut self,
        id: u64,
        req: ReorderPlaylist,
    ) -> Result<CollectionDetail, ServerError> {
        let ctx = self.mutable_ctx(id, req.revision.as_deref(), true, true)?;
        let mut doc = read_noudata(&ctx.path)?;
        if FileRevision::of(&ctx.path).ok() != Some(ctx.rev) {
            return Err(ServerError::Conflict(
                "the file changed while it was being read; retry".into(),
            ));
        }
        let permutation = checked_permutation(&req.positions, doc.media_list.len())?;
        apply_reorder(&mut doc, &permutation);
        write_noudata_atomic(&doc, &ctx.path, WriteGuard::Unchanged(ctx.rev)).map_err(|e| {
            write_err(
                e,
                if ctx.release {
                    "the album order"
                } else {
                    "the playlist order"
                },
            )
        })?;
        self.index_write_through(ctx.lib_idx, &ctx.rel, None)?;
        // Same record (id is stable across an in-place write); re-read it for the fresh items.
        let (idx, rec) = self.require_record(id)?;
        self.detail_of(idx, rec)
    }

    /// `PUT /api/playlists/:id/order` — same as [`Self::reorder_collection`]; kept for
    /// existing clients.
    pub fn reorder_playlist(
        &mut self,
        id: u64,
        req: ReorderPlaylist,
    ) -> Result<CollectionDetail, ServerError> {
        self.reorder_collection(id, req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_noubin::{parse_noudata, ItemLocalPlaybackData, LocalPlaybackData, NoubinTimestamp};

    fn doc(title: &str) -> NoudataDocument {
        new_playlist_document(title, "tester")
    }

    fn setup(dir: &Path) -> (PathBuf, PathBuf, FileRevision) {
        let path = dir.join("_Old.noudata");
        write_noudata_atomic(&doc("Old"), &path, WriteGuard::MustNotExist).unwrap();
        let rev = FileRevision::of(&path).unwrap();
        (path, dir.join("_New.noudata"), rev)
    }

    fn title_of(path: &Path) -> String {
        parse_noudata(&fs::read(path).unwrap())
            .unwrap()
            .playable_item_title
            .unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// G1 open issue (Low): a rename that fails part-way must never leave the old file name
    /// carrying the new title (or the new name carrying the old title).
    #[test]
    fn rename_failure_at_any_step_leaves_a_consistent_state() {
        for stage in ["before_rename", "after_rename"] {
            let tmp = tempfile::tempdir().unwrap();
            let (old, new, rev) = setup(tmp.path());
            let r = commit_playlist_edit(&doc("New"), &old, rev, Some(&new), &mut |s| {
                if s == stage {
                    Err(std::io::Error::other("injected"))
                } else {
                    Ok(())
                }
            });
            assert!(r.is_err(), "{stage}");
            assert_eq!(names(tmp.path()), vec!["_Old.noudata"], "{stage}");
            assert_eq!(title_of(&old), "Old", "{stage}");
            assert_eq!(FileRevision::of(&old).unwrap(), rev, "{stage}: file untouched");
        }
    }

    #[test]
    fn reorder_permutes_the_list_and_carries_the_resume_point() {
        let mut d = doc("Mix");
        d.media_list = ["a.mp3", "b.mp3", "c.mp3"]
            .iter()
            .map(|p| playlist_entry(p))
            .collect();
        d.local_playback_data = Some(LocalPlaybackData {
            resume_at_item: Some(2),
            ..Default::default()
        });
        d.media_list[1].item_local_playback_data = Some(ItemLocalPlaybackData {
            resume_from_time: Some(NoubinTimestamp::from_millis(7_000)),
            ..Default::default()
        });
        // New order [old1, old2, old0]: the resumed item (1-indexed 2 = old index 1) is first.
        let perm = checked_permutation(&[1, 2, 0], 3).unwrap();
        apply_reorder(&mut d, &perm);
        let order: Vec<&str> = d
            .media_list
            .iter()
            .map(|i| i.local_file_paths.as_ref().unwrap()[0].as_str())
            .collect();
        assert_eq!(order, ["b.mp3", "c.mp3", "a.mp3"]);
        assert_eq!(
            d.local_playback_data.as_ref().unwrap().resume_at_item,
            Some(1),
            "resumeAtItem follows the item, not the position"
        );
        assert_eq!(
            d.media_list[0]
                .item_local_playback_data
                .as_ref()
                .unwrap()
                .resume_from_time,
            Some(NoubinTimestamp::from_millis(7_000)),
            "the item's own resume time travels with it"
        );
    }

    #[test]
    fn reorder_rejects_invalid_permutations() {
        for (positions, len) in [
            (vec![0, 0, 2], 3usize), // duplicate
            (vec![0, 1, 3], 3),      // out of range
            (vec![0, 1], 3),         // too short
            (vec![], 1),             // too short for one item
            (vec![0, 1, 2, 3], 3),   // too long
        ] {
            assert!(
                checked_permutation(&positions, len).is_err(),
                "{positions:?} / {len}"
            );
        }
        assert!(checked_permutation(&[0, 1, 2], 3).is_ok());
        assert!(checked_permutation(&[], 0).is_ok());
    }

    #[test]
    fn rename_success_moves_file_and_title_together() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new, rev) = setup(tmp.path());
        let out = commit_playlist_edit(&doc("New"), &old, rev, Some(&new), &mut |_| Ok(())).unwrap();
        assert_eq!(out, new);
        assert_eq!(names(tmp.path()), vec!["_New.noudata"]);
        assert_eq!(title_of(&new), "New");
    }

    #[test]
    fn rename_refuses_when_the_file_changed_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (old, new, rev) = setup(tmp.path());
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_noudata_atomic(&doc("Someone else"), &old, WriteGuard::Unchanged(rev)).unwrap();
        let r = commit_playlist_edit(&doc("New"), &old, rev, Some(&new), &mut |_| Ok(()));
        assert!(matches!(r, Err(ServerError::Conflict(_))), "{r:?}");
        assert_eq!(names(tmp.path()), vec!["_Old.noudata"]);
        assert_eq!(title_of(&old), "Someone else");
    }
}
