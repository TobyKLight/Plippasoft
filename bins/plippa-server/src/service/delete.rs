//! BCS_10: delete a library item — a whole playable (release / loose track) or one media
//! file of a release — with a **preview then confirm** contract.
//!
//! A request carries a playable id (plus, for a single file, a `mediaList` position) and a
//! force flag; it never carries a filesystem path. Every path that is removed comes from the
//! release `.noudata` — or, for an orphan media file, from the index — and is re-validated
//! against the library root with [`LibraryFs`] at the moment of deletion
//! (`NBSecurityRequirements` §3 paths, §4 writes).
//!
//! Order of work: index rows (and with them the Noubin key mappings) first, so the item
//! vanishes from the portal atomically; then the files; then the folder if it became empty;
//! then a lazy rescan for anything else that went with them. A file that cannot be removed
//! stops the loop (`partial: true` + `remaining`) and leaves the index describing exactly
//! what is still on disk, so a second delete (of the leftover, now an orphan) can finish.

use super::collections::{lib_err, rel_dir, resolve_in, write_err};
use super::{read_noudata, ServerError, ServerService};
use crate::unite::fsops::LibraryFs;
use plippa_core::TransportState;
use plippa_noubin::covers::{thumbnail_relpath, CoverSource, ThumbTier};
use plippa_noubin::{
    encode_playable_id, is_safe_relative_path, write_noudata_atomic, AtomicWriteError,
    FileRevision, LibraryKind, NoudataDocument, PlayableRecord, ScanReport, WriteGuard,
    PLAYER_DB_DIR,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// `DELETE` body. A missing body means `{ "force": false }`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeleteRequest {
    /// Without it nothing is deleted: the call answers `409 code "confirm_required"`.
    #[serde(default)]
    pub force: bool,
    /// Free text, logged with the delete (never interpreted).
    #[serde(default)]
    pub reason: Option<String>,
}

/// One file the delete would remove — or could not remove (in `remaining`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeleteFile {
    /// Library-relative, `/`-separated.
    pub path: String,
    /// Bytes on disk (`0` when the file is already gone or is not a regular file).
    pub bytes: u64,
    /// Already missing: nothing to remove on confirm.
    pub missing: bool,
    /// Why a `remaining` entry is still on disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl DeleteFile {
    fn at(path: impl Into<String>, bytes: u64, missing: bool) -> Self {
        Self {
            path: path.into(),
            bytes,
            missing,
            reason: None,
        }
    }
}

/// Why a delete is refused. `kind` is stable for the UI: `symlink`, `unsafe_path`,
/// `playlist`, `playing`, `no_files`, `root`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeleteBlocker {
    pub kind: String,
    pub detail: String,
}

/// A playlist entry that points at media this delete would remove.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlaylistHit {
    /// Encoded playable id, as a decimal string (ids exceed 2^53).
    pub id: String,
    pub title: String,
    /// `mediaList` position of the entry that references the media.
    pub item_position: u32,
}

/// The playable a preview / outcome is about.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeleteTarget {
    pub id: String,
    pub title: Option<String>,
    pub kind: String,
    pub is_orphan: bool,
    /// Set for the single-file routes: which `mediaList` entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<u32>,
}

/// Answer of `GET /api/playables/:id/delete/preview` and of a refused delete.
///
/// Pure: it reads the index, the `.noudata` and file metadata, and changes nothing.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeletePreview {
    pub playable: DeleteTarget,
    pub files: Vec<DeleteFile>,
    pub covers: Vec<DeleteFile>,
    /// Library-relative path of the `.noudata` that will be removed (`null` for an orphan).
    pub noudata: Option<String>,
    /// Library-relative folder removed when it becomes empty (`null` in the library root).
    pub folder: Option<String>,
    /// Everything the delete reclaims (media, covers, the `.noudata`).
    pub total_bytes: u64,
    /// Reasons the delete is refused. Non-empty ⇒ nothing will be deleted.
    pub blockers: Vec<DeleteBlocker>,
    /// Things that will be lost or left behind. Not refusals.
    pub warnings: Vec<String>,
    pub playlists: Vec<PlaylistHit>,
    /// Normalised Noubin keys that stop resolving to this item.
    pub keys: Vec<String>,
    pub playing: bool,
}

/// `200` from a confirmed delete.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeleteOutcome {
    pub ok: bool,
    /// Library-relative paths actually removed.
    pub deleted: Vec<String>,
    /// The index was updated before the files were touched.
    pub index_updated: bool,
    pub freed_bytes: u64,
    /// A file could not be removed: the rest is in `remaining`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// Paths still on disk after a partial delete (with the reason the loop stopped).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remaining: Vec<DeleteFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Playlists whose item list was edited.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub playlists_edited: Vec<String>,
    /// The release folder was removed because it became empty.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub folder_removed: bool,
}

/// A refused delete, carrying everything the HTTP layer needs.
#[derive(Debug)]
pub enum DeleteError {
    /// A 4xx with a stable `code`; `preview` is merged into the error body when present.
    Refused {
        status: u16,
        code: &'static str,
        message: String,
        preview: Option<Box<DeletePreview>>,
    },
    /// An unexpected internal failure (IO, index, vanished storage).
    Server(ServerError),
}

impl DeleteError {
    fn refusal(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self::Refused {
            status,
            code,
            message: message.into(),
            preview: None,
        }
    }

    fn with_preview(
        status: u16,
        code: &'static str,
        message: impl Into<String>,
        preview: &DeletePreview,
    ) -> Self {
        Self::Refused {
            status,
            code,
            message: message.into(),
            preview: Some(Box::new(preview.clone())),
        }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::Refused { status, .. } => *status,
            Self::Server(e) => e.http_status(),
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Refused { code, .. } => code,
            Self::Server(ServerError::NotFound(_)) => "not_found",
            Self::Server(ServerError::Unavailable(_)) => "unavailable",
            Self::Server(_) => "internal",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Refused { message, .. } => message.clone(),
            Self::Server(e) => e.to_string(),
        }
    }

    /// The preview merged into the error body, when the refusal carries one.
    pub fn preview(&self) -> Option<&DeletePreview> {
        match self {
            Self::Refused { preview, .. } => preview.as_deref(),
            Self::Server(_) => None,
        }
    }
}

impl From<ServerError> for DeleteError {
    fn from(e: ServerError) -> Self {
        Self::Server(e)
    }
}

// ---------------------------------------------------------------------------
// Planning (shared by preview and confirm)
// ---------------------------------------------------------------------------

/// One path the plan will remove, resolved and checked against the library root.
#[derive(Debug, Clone)]
struct PlannedFile {
    rel: String,
    bytes: u64,
    missing: bool,
}

impl PlannedFile {
    fn out(&self) -> DeleteFile {
        DeleteFile::at(self.rel.clone(), self.bytes, self.missing)
    }
}

/// What a delete is about to do. Built identically by the preview and by the confirm call,
/// so a confirm never acts on a plan the preview did not show.
struct Plan {
    lib_idx: usize,
    record: PlayableRecord,
    root: PathBuf,
    /// The `.noudata` to remove (`None` for an orphan media file).
    noudata_rel: Option<String>,
    noudata_abs: Option<PathBuf>,
    noudata_bytes: u64,
    noudata_missing: bool,
    /// Folder of the `.noudata` (library-relative, `""` in the root).
    release_dir: String,
    /// Folder removed when it becomes empty.
    folder: Option<String>,
    files: Vec<PlannedFile>,
    covers: Vec<PlannedFile>,
    blockers: Vec<DeleteBlocker>,
    warnings: Vec<String>,
    playlists: Vec<PlaylistHit>,
    keys: Vec<String>,
    playing: bool,
    /// The `mediaList` entry a single-file delete removes (0-based).
    item: Option<u32>,
    /// `mediaList` positions removed from the document (single-file delete).
    remove_positions: Vec<usize>,
    /// A link track points at media its release owns: only its `.noudata` is removed.
    link_only: bool,
}

impl Plan {
    fn preview(&self) -> DeletePreview {
        let total = self.files.iter().map(|f| f.bytes).sum::<u64>()
            + self.covers.iter().map(|f| f.bytes).sum::<u64>()
            + self.noudata_bytes;
        DeletePreview {
            playable: DeleteTarget {
                id: encode_playable_id(&self.root, self.record.id).to_string(),
                title: non_empty(&self.record.title),
                kind: self.record.kind.as_str().to_string(),
                is_orphan: self.record.is_orphan,
                item: self.item,
            },
            files: self.files.iter().map(PlannedFile::out).collect(),
            covers: self.covers.iter().map(PlannedFile::out).collect(),
            noudata: self.noudata_rel.clone(),
            folder: self.folder.clone(),
            total_bytes: total,
            blockers: self.blockers.clone(),
            warnings: self.warnings.clone(),
            playlists: self.playlists.clone(),
            keys: self.keys.clone(),
            playing: self.playing,
        }
    }
}

fn non_empty(v: &Option<String>) -> Option<String> {
    v.clone().filter(|t| !t.trim().is_empty())
}

fn blocker(kind: &str, detail: impl Into<String>) -> DeleteBlocker {
    DeleteBlocker {
        kind: kind.to_string(),
        detail: detail.into(),
    }
}

fn merge(into: &mut ScanReport, from: ScanReport) {
    into.scanned_files += from.scanned_files;
    into.ingested += from.ingested;
    into.updated += from.updated;
    into.removed += from.removed;
    into.skipped += from.skipped;
    into.orphans_added += from.orphans_added;
    into.orphans_removed += from.orphans_removed;
    into.availability_changed += from.availability_changed;
    into.noudata_reads += from.noudata_reads;
    into.dirs_listed += from.dirs_listed;
    into.warnings.extend(from.warnings);
}

/// Resolve a library-relative path for deletion. `None` = refused, with a blocker recorded.
///
/// `tolerate_dir`: the index knows this path as media, so a folder sitting where a file was
/// is a delete-time failure (the resumable `partial` path), not a broken document.
fn plan_path(
    lfs: &LibraryFs,
    rel: &str,
    blockers: &mut Vec<DeleteBlocker>,
    tolerate_dir: bool,
) -> Option<PlannedFile> {
    if rel.is_empty() {
        blockers.push(blocker("root", "the library root itself can never be deleted"));
        return None;
    }
    if rel.split('/').next() == Some(PLAYER_DB_DIR) {
        blockers.push(blocker(
            "unsafe_path",
            format!("{rel:?} is inside {PLAYER_DB_DIR}/, which is never touched"),
        ));
        return None;
    }
    if !is_safe_relative_path(rel) {
        blockers.push(blocker(
            "unsafe_path",
            format!("{rel:?} is not a safe library-relative path"),
        ));
        return None;
    }
    match lfs.resolve(Path::new(rel)) {
        Ok(abs) => {
            let (bytes, missing) = match fs::metadata(&abs) {
                Ok(m) if m.is_file() => (m.len(), false),
                Ok(_) if tolerate_dir => (0, false),
                Ok(_) => {
                    blockers.push(blocker(
                        "unsafe_path",
                        format!("{rel:?} is a folder, not a media file"),
                    ));
                    return None;
                }
                Err(_) => (0, true),
            };
            let _ = abs;
            Some(PlannedFile {
                rel: rel.to_string(),
                bytes,
                missing,
            })
        }
        Err(e) => {
            let text = e.to_string();
            let kind = if text.contains("symbolic link") {
                "symlink"
            } else {
                "unsafe_path"
            };
            blockers.push(blocker(kind, format!("{rel:?}: {text}")));
            None
        }
    }
}

/// Remove one regular file. `Ok(0)` when it is already gone; `Err(reason)` when the path
/// cannot (or must not) be touched.
fn remove_one(lfs: &LibraryFs, rel: &str) -> Result<u64, String> {
    let abs = lfs.resolve(Path::new(rel)).map_err(|e| e.to_string())?;
    match fs::symlink_metadata(&abs) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e.to_string()),
        Ok(m) if m.file_type().is_symlink() => {
            Err("it is a symbolic link (never followed)".to_string())
        }
        Ok(m) if !m.file_type().is_file() => Err("it is not a regular file".to_string()),
        Ok(m) => {
            let bytes = m.len();
            fs::remove_file(&abs).map(|()| bytes).map_err(|e| e.to_string())
        }
    }
}

/// `<dir>/thumbnails/<stem>-<px>.<ext>` and its `Thumbnails` fallback spelling.
fn thumbnails_of(image_relpath: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tier in ThumbTier::ALL {
        for ext in ["jpg", "webp"] {
            let base = thumbnail_relpath(image_relpath, tier, ext);
            let alt = match base.strip_prefix("thumbnails/") {
                Some(rest) => format!("Thumbnails/{rest}"),
                None => base.replace("/thumbnails/", "/Thumbnails/"),
            };
            for rel in [base, alt] {
                if !out.contains(&rel) {
                    out.push(rel);
                }
            }
        }
    }
    out
}

impl ServerService {
    /// The id the engine is playing right now, if it is playing anything.
    fn playing_playable_id(&self) -> Option<u64> {
        let mirror = self.with_ipc(|ipc| {
            ipc.poll_inbound();
            ipc.mirror()
        });
        mirror
            .playback
            .filter(|p| p.transport == TransportState::Playing)
            .and_then(|p| p.session.current_item.map(|i| i.as_u64()))
    }

    fn require_delete_record(&mut self, id: u64) -> Result<(usize, PlayableRecord), DeleteError> {
        match self.require_record(id) {
            Ok(v) => Ok(v),
            Err(ServerError::NotFound(m)) => Err(DeleteError::refusal(404, "not_found", m)),
            Err(e) => Err(DeleteError::Server(e)),
        }
    }

    /// Build the plan for deleting playable `id`. `only_item` = remove one `mediaList` entry.
    fn delete_plan(&mut self, id: u64, only_item: Option<u32>) -> Result<Plan, DeleteError> {
        let (lib_idx, record) = self.require_delete_record(id)?;
        let root = self.libraries[lib_idx].root.clone();
        let lfs =
            LibraryFs::new(&root).map_err(|e| DeleteError::refusal(503, "unavailable", format!("library root: {e}")))?;
        let playing = self.playing_playable_id() == Some(id);

        let noudata_rel = (!record.is_orphan && !record.noudata_relpath.is_empty())
            .then(|| record.noudata_relpath.clone());
        let release_dir = noudata_rel.as_deref().map(rel_dir).unwrap_or("").to_string();

        let mut plan = Plan {
            lib_idx,
            root: root.clone(),
            noudata_rel: noudata_rel.clone(),
            noudata_abs: None,
            noudata_bytes: 0,
            noudata_missing: false,
            release_dir: release_dir.clone(),
            folder: None,
            files: Vec::new(),
            covers: Vec::new(),
            blockers: Vec::new(),
            warnings: Vec::new(),
            playlists: Vec::new(),
            keys: record
                .noubin_key_normalised
                .clone()
                .filter(|k| !k.is_empty())
                .into_iter()
                .collect(),
            playing,
            item: only_item,
            remove_positions: Vec::new(),
            link_only: record.noudata_type.as_deref() == Some("linkTrack"),
            record: record.clone(),
        };

        if playing {
            plan.blockers.push(blocker(
                "playing",
                "this item is playing on the player; stop it before deleting",
            ));
        }
        if volume_is_read_only(&root) {
            plan.blockers.push(blocker(
                "read_only",
                "the media volume is mounted read-only, so nothing can be deleted from it",
            ));
        }

        // Playlists are deleted (never media-deleted) by `DELETE /api/playlists/:id`.
        if record.kind == LibraryKind::Playlist {
            plan.blockers.push(blocker(
                "playlist",
                "this is a playlist: delete it with DELETE /api/playlists/<id>; a playlist's \
                 media files are never deleted",
            ));
            return Ok(plan);
        }

        // The `.noudata` is the only source of media paths.
        let doc = match &noudata_rel {
            Some(rel) => match lfs.resolve(Path::new(rel)) {
                Ok(abs) => {
                    plan.noudata_abs = Some(abs.clone());
                    if let Ok(m) = fs::metadata(&abs) {
                        plan.noudata_bytes = m.len();
                    } else {
                        plan.noudata_missing = true;
                        plan.warnings.push(format!(
                            "the metadata file {rel} is already missing; the index rows are \
                             still removed"
                        ));
                    }
                    match read_noudata(&abs) {
                        Ok(d) => Some(d),
                        Err(e) => {
                            plan.warnings.push(format!(
                                "the metadata file {rel} could not be read ({e}); the media files \
                                 listed in the index are removed instead"
                            ));
                            None
                        }
                    }
                }
                Err(e) => {
                    let text = e.to_string();
                    let kind = if text.contains("symbolic link") {
                        "symlink"
                    } else {
                        "unsafe_path"
                    };
                    plan.blockers.push(blocker(kind, format!("{rel:?}: {text}")));
                    None
                }
            },
            None => None,
        };

        // A single media file is addressed by its `mediaList` position: without a readable
        // `.noudata` the position cannot be trusted, and falling back to "all media" would
        // delete far more than the caller asked for.
        if only_item.is_some() && plan.noudata_rel.is_some() && doc.is_none() {
            plan.blockers.push(blocker(
                "unreadable_noudata",
                "the metadata file cannot be read, so a single media file cannot be identified; \
                 delete the whole item instead",
            ));
        }
        if only_item.is_some() && plan.link_only {
            plan.blockers.push(blocker(
                "link_track",
                "this is a link track whose media belongs to its release; delete the whole item \
                 (its .noudata only) instead of one file",
            ));
        }

        // ---- media ----
        if plan.link_only {
            plan.warnings.push(
                "this is a link track: it points at media that belongs to its release, so only \
                 its .noudata is removed and the audio stays"
                    .to_string(),
            );
        } else {
            self.plan_media(&lfs, &mut plan, doc.as_ref(), only_item)?;
        }

        // ---- covers + thumbnails (whole playable only) ----
        if only_item.is_none() && !plan.link_only && noudata_rel.is_some() {
            self.plan_covers(&lfs, &mut plan)?;
        }

        // ---- folder ----
        if !release_dir.is_empty() {
            let mut all: Vec<String> = plan
                .files
                .iter()
                .chain(plan.covers.iter())
                .map(|f| f.rel.clone())
                .collect();
            if let Some(rel) = &plan.noudata_rel {
                all.push(rel.clone());
            }
            let inside = !all.is_empty() && all.iter().all(|p| under(p, &release_dir));
            if inside {
                plan.folder = Some(release_dir.clone());
            } else if !all.is_empty() {
                plan.warnings.push(format!(
                    "some files of this item are outside {release_dir}/; that folder is left in \
                     place"
                ));
            }
        }

        if plan.files.is_empty() && plan.noudata_rel.is_none() {
            plan.blockers.push(blocker(
                "no_files",
                "this item has no files the library can address",
            ));
        }

        // ---- references ----
        self.plan_playlists(&mut plan);
        self.plan_referrers(&mut plan);

        Ok(plan)
    }

    /// Media files of the whole playable, or of the one `mediaList` entry given.
    fn plan_media(
        &mut self,
        lfs: &LibraryFs,
        plan: &mut Plan,
        doc: Option<&NoudataDocument>,
        only_item: Option<u32>,
    ) -> Result<(), DeleteError> {
        // `(mediaList position, paths as written)`; without a readable `.noudata` the index
        // paths are already library-relative.
        let mut pairs: Vec<(usize, Vec<String>)> = Vec::new();
        let mut from_doc = false;
        if let Some(doc) = doc {
            from_doc = true;
            for (pos, item) in doc.media_list.iter().enumerate() {
                let paths = item
                    .local_file_paths
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|p| !p.trim().is_empty())
                    .collect::<Vec<_>>();
                pairs.push((pos, paths));
            }
        } else {
            for rel in &plan.record.media_relpaths {
                if !rel.is_empty() {
                    pairs.push((0, vec![rel.clone()]));
                }
            }
        }

        if let Some(want) = only_item {
            let Some((_, paths)) = pairs.iter().find(|(pos, _)| *pos == want as usize) else {
                return Err(DeleteError::refusal(
                    404,
                    "no_such_item",
                    format!(
                        "no mediaList entry at position {want} (the item has {} entries)",
                        pairs.len()
                    ),
                ));
            };
            let with_files = pairs.iter().filter(|(_, p)| !p.is_empty()).count();
            if with_files <= 1 {
                return Err(DeleteError::refusal(
                    400,
                    "last_file",
                    "this is the only media file of the item; delete the whole item instead \
                     (DELETE /api/playables/<id> or DELETE /api/collections/<id>)",
                ));
            }
            if paths.is_empty() {
                return Err(DeleteError::refusal(
                    400,
                    "no_files",
                    format!("mediaList entry {want} has no local files"),
                ));
            }
        }

        let mut seen: Vec<String> = Vec::new();
        for (pos, paths) in pairs {
            if let Some(want) = only_item {
                if pos != want as usize {
                    continue;
                }
                plan.remove_positions.push(pos);
            }
            if paths.is_empty() && only_item.is_none() {
                plan.warnings.push(format!(
                    "mediaList entry {pos} has no local file; nothing to remove for it"
                ));
            }
            for written in paths {
                let rel = if from_doc {
                    resolve_in(&plan.release_dir, &written)
                } else {
                    resolve_in("", &written)
                };
                if written.trim().is_empty() {
                    plan.warnings
                        .push(format!("mediaList entry {pos} names an empty path"));
                    continue;
                }
                if rel.is_empty() {
                    plan.blockers.push(blocker(
                        "root",
                        format!("mediaList entry {pos} names the library root itself"),
                    ));
                    continue;
                }
                // A `..` segment, an absolute path or a directory is a broken (or hostile)
                // document: refuse instead of quietly retargeting the delete inside the root.
                if let Some(why) = unsafe_media_path(&written, &rel) {
                    plan.blockers
                        .push(blocker("unsafe_path", format!("mediaList entry {pos}: {why}")));
                    continue;
                }
                if seen.iter().any(|s| s.eq_ignore_ascii_case(&rel)) {
                    continue;
                }
                seen.push(rel.clone());
                let known = plan
                    .record
                    .media_relpaths
                    .iter()
                    .any(|m| m.eq_ignore_ascii_case(&rel));
                let Some(f) = plan_path(lfs, &rel, &mut plan.blockers, known) else {
                    continue;
                };
                if from_doc && !plan.release_dir.is_empty() && !under(&rel, &plan.release_dir) {
                    plan.warnings.push(format!(
                        "{rel} is outside the release folder {} and is removed because the \
                         .noudata lists it",
                        plan.release_dir
                    ));
                }
                if f.missing {
                    plan.warnings.push(format!("{rel} is already missing"));
                }
                plan.files.push(f);
            }
        }
        Ok(())
    }

    /// Declared cover images of the record plus their generated thumbnails.
    fn plan_covers(&mut self, lfs: &LibraryFs, plan: &mut Plan) -> Result<(), DeleteError> {
        let lib_idx = plan.lib_idx;
        let rec_id = plan.record.id;
        let own = plan.noudata_rel.clone();
        let covers = self.libraries[lib_idx]
            .writer
            .covers_of(rec_id)
            .map_err(lib_err)?;
        if covers.is_empty() {
            return Ok(());
        }
        // Two `.noudata` files in one folder can share an undeclared sibling `cover.jpg`.
        let folder_shared = self.libraries[lib_idx]
            .writer
            .list_playables()
            .map_err(lib_err)?
            .into_iter()
            .filter(|r| !r.is_orphan && rel_dir(&r.noudata_relpath) == plan.release_dir)
            .count()
            > 1;

        for c in covers {
            if matches!(c.source, CoverSource::Sibling) {
                // A guess from the file name: only remove it when it sits in the release
                // folder and nothing else lives there.
                let inside = !plan.release_dir.is_empty() && under(&c.relpath, &plan.release_dir);
                if !inside || folder_shared {
                    plan.warnings.push(format!(
                        "{} looks like a cover image but is not declared by the metadata; it is \
                         left in place",
                        c.relpath
                    ));
                    continue;
                }
            }
            let others = self.libraries[lib_idx]
                .writer
                .referencing_noudata(&c.relpath)
                .map_err(lib_err)?;
            if let Some(other) = others
                .iter()
                .find(|r| !r.is_empty() && Some(r.as_str()) != own.as_deref())
            {
                plan.warnings.push(format!(
                    "{} is also used by {other} and is left in place",
                    c.relpath
                ));
                continue;
            }
            let Some(cover) = plan_path(lfs, &c.relpath, &mut plan.blockers, false) else {
                continue;
            };
            plan.covers.push(cover);
            for t in thumbnails_of(&c.relpath) {
                if let Some(f) = plan_path(lfs, &t, &mut plan.blockers, false) {
                    if !f.missing {
                        plan.covers.push(f);
                    }
                }
            }
        }
        Ok(())
    }

    /// Playlists whose item list references one of the media files being removed.
    fn plan_playlists(&mut self, plan: &mut Plan) {
        if plan.link_only {
            return;
        }
        let targets: Vec<String> = plan.files.iter().map(|f| f.rel.to_lowercase()).collect();
        if targets.is_empty() {
            return;
        }
        let lib_idx = plan.lib_idx;
        let root = plan.root.clone();
        let playlists = match self.libraries[lib_idx]
            .writer
            .list_by_kind(LibraryKind::Playlist)
        {
            Ok(p) => p,
            Err(e) => {
                plan.warnings
                    .push(format!("playlists could not be checked: {e}"));
                return;
            }
        };
        for pl in playlists {
            if pl.is_dynamic || pl.noudata_relpath.is_empty() {
                continue;
            }
            let dir = rel_dir(&pl.noudata_relpath);
            let Ok(doc) = read_noudata(&root.join(&pl.noudata_relpath)) else {
                continue;
            };
            for (pos, item) in doc.media_list.iter().enumerate() {
                let hit = item
                    .local_file_paths
                    .iter()
                    .flatten()
                    .map(|p| resolve_in(dir, p).to_lowercase())
                    .any(|rel| !rel.is_empty() && targets.contains(&rel));
                if hit {
                    plan.playlists.push(PlaylistHit {
                        id: encode_playable_id(&root, pl.id).to_string(),
                        title: non_empty(&pl.title)
                            .unwrap_or_else(|| pl.noudata_relpath.clone()),
                        item_position: pos as u32,
                    });
                }
            }
        }
        plan.playlists.sort();
        plan.playlists.dedup();
    }

    /// Warnings about documents that point at this item but are not deleted with it.
    fn plan_referrers(&mut self, plan: &mut Plan) {
        let Some(rel) = plan.noudata_rel.clone() else {
            return;
        };
        let lib_idx = plan.lib_idx;
        let own_id = plan.record.id;
        let Ok(records) = self.libraries[lib_idx].writer.list_playables() else {
            return;
        };
        let mut refs: Vec<String> = records
            .into_iter()
            .filter(|r| r.id != own_id && r.refers_to_relpath.as_deref() == Some(rel.as_str()))
            .map(|r| r.noudata_relpath)
            .collect();
        refs.sort();
        if !refs.is_empty() {
            plan.warnings.push(format!(
                "{} other item(s) refer to this release and are left in place (delete them \
                 separately): {}",
                refs.len(),
                refs.join(", ")
            ));
        }
    }
}

/// Is the volume holding `dir` mounted read-only? (`statvfs` is the only portable answer;
/// permissions on the folder itself do not show a read-only mount.)
fn volume_is_read_only(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c` is a valid NUL-terminated path; `st` is a properly sized out-parameter.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return false;
        }
        (st.f_flag & libc::ST_RDONLY) != 0
    }
}

/// Why a media path written in a `.noudata` must not be acted on, if it must not.
fn unsafe_media_path(written: &str, resolved: &str) -> Option<String> {
    let trimmed = written.trim();
    if trimmed.starts_with('/') || trimmed.starts_with('\\') {
        return Some(format!("{written:?} is an absolute path"));
    }
    if written
        .split('/')
        .any(|seg| seg == ".." || seg == "\\..")
    {
        return Some(format!("{written:?} climbs out of its folder with `..`"));
    }
    let first = resolved.split('/').next().unwrap_or("");
    if first == PLAYER_DB_DIR {
        return Some(format!("{written:?} is inside {PLAYER_DB_DIR}/"));
    }
    None
}

/// True when `rel` is `dir` itself or below it.
fn under(rel: &str, dir: &str) -> bool {
    rel == dir || rel.starts_with(&format!("{dir}/"))
}

// ---------------------------------------------------------------------------
// Preview and confirm
// ---------------------------------------------------------------------------

impl ServerService {
    /// `GET /api/playables/:id/delete/preview` — pure.
    pub fn preview_delete_playable(&mut self, id: u64) -> Result<DeletePreview, DeleteError> {
        Ok(self.delete_plan(id, None)?.preview())
    }

    /// `GET /api/playables/:id/files/:index/delete/preview` — pure.
    pub fn preview_delete_playable_file(
        &mut self,
        id: u64,
        index: u32,
    ) -> Result<DeletePreview, DeleteError> {
        Ok(self.delete_plan(id, Some(index))?.preview())
    }

    /// `DELETE /api/playables/:id` — the whole playable.
    pub fn delete_playable(
        &mut self,
        id: u64,
        req: DeleteRequest,
    ) -> Result<DeleteOutcome, DeleteError> {
        self.delete_playable_impl(id, None, req)
    }

    /// BCS_05: delete the playable whose `.noudata` is `rel` (library-relative) in `root`.
    ///
    /// This is the "replace" half of a store download: the caller deletes the copy this
    /// store release produced before re-adding it, so no second folder appears. The delete
    /// itself is [`Self::delete_playable`], with the same index-first order and same safety
    /// rules as the portal's route. Returns the playable id that was removed.
    pub fn delete_library_item(&mut self, root: &Path, rel: &str) -> Result<u64, String> {
        let _ = self.lazy_rescan();
        let idx = self
            .libraries
            .iter()
            .position(|l| super::paths_same(&l.root, root))
            .ok_or_else(|| "the library is no longer mounted".to_string())?;
        let (lib_root, label) = (
            self.libraries[idx].root.clone(),
            self.libraries[idx].label.clone(),
        );
        let multi = self.libraries.len() > 1;
        let summary = super::list_by_rel(self, &lib_root, rel, &label, multi)
            .map_err(|error| error.to_string())?;
        self.delete_playable(
            summary.id,
            DeleteRequest {
                force: true,
                reason: Some("replaced by a store download".to_string()),
            },
        )
        .map(|_| summary.id)
        .map_err(|error| match error {
            DeleteError::Refused { message, .. } => message,
            DeleteError::Server(inner) => inner.to_string(),
        })
    }

    /// `DELETE /api/collections/:id` — the whole playable (the collection view's route).
    pub fn delete_collection(
        &mut self,
        id: u64,
        req: DeleteRequest,
    ) -> Result<DeleteOutcome, DeleteError> {
        self.delete_playable_impl(id, None, req)
    }

    /// `DELETE /api/playables/:id/files/:index` — one `mediaList` entry, keeping the rest.
    pub fn delete_playable_file(
        &mut self,
        id: u64,
        index: u32,
        req: DeleteRequest,
    ) -> Result<DeleteOutcome, DeleteError> {
        self.delete_playable_impl(id, Some(index), req)
    }

    fn delete_playable_impl(
        &mut self,
        id: u64,
        only_item: Option<u32>,
        req: DeleteRequest,
    ) -> Result<DeleteOutcome, DeleteError> {
        let plan = self.delete_plan(id, only_item)?;
        let preview = plan.preview();

        if plan.playing {
            return Err(DeleteError::with_preview(
                409,
                "playing",
                "this item is playing on the player; stop it before deleting",
                &preview,
            ));
        }
        if !plan.blockers.is_empty() {
            let message = plan
                .blockers
                .first()
                .map(|b| b.detail.clone())
                .unwrap_or_else(|| "this item cannot be deleted".into());
            return Err(DeleteError::with_preview(409, "blocked", message, &preview));
        }
        if !req.force {
            return Err(DeleteError::with_preview(
                409,
                "confirm_required",
                "nothing was deleted: repeat the request with {\"force\": true} to confirm",
                &preview,
            ));
        }
        if let Some(reason) = req.reason.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
            eprintln!("plippa-server: delete {id}: {reason}");
        }

        let mut outcome = DeleteOutcome {
            ok: true,
            deleted: Vec::new(),
            index_updated: true,
            freed_bytes: 0,
            partial: false,
            remaining: Vec::new(),
            warnings: plan.warnings.clone(),
            playlists_edited: Vec::new(),
            folder_removed: false,
        };

        // 1. Index rows and Noubin keys first: the item leaves the portal atomically and a
        //    later tap of its tag is an unmatched-noubin miss, not a ghost row. A single-file
        //    delete keeps the item, so its row is re-derived from the edited `.noudata` in
        //    step 2 instead of being dropped here.
        if plan.remove_positions.is_empty() {
            if let Some(rel) = plan.noudata_rel.clone() {
                self.forget_documents(plan.lib_idx, std::slice::from_ref(&rel))?;
            }
        }

        // 2. Single-file delete: the `.noudata` is the index's source of truth, so it is
        //    edited (and re-ingested) before the bytes go.
        if !plan.remove_positions.is_empty() {
            self.remove_items_from_document(&plan)?;
        }

        // 3. Files: `.noudata` first, then media, then covers/thumbnails.
        let mut order: Vec<String> = Vec::new();
        if plan.remove_positions.is_empty() {
            if let Some(rel) = &plan.noudata_rel {
                order.push(rel.clone());
            }
        }
        order.extend(plan.files.iter().map(|f| f.rel.clone()));
        order.extend(plan.covers.iter().map(|f| f.rel.clone()));

        let lfs = match LibraryFs::new(&plan.root) {
            Ok(l) => l,
            Err(e) => {
                return Err(DeleteError::refusal(
                    503,
                    "unavailable",
                    format!("library root: {e}"),
                ))
            }
        };
        let mut failure: Option<(String, String)> = None;
        for rel in &order {
            match remove_one(&lfs, rel) {
                Ok(bytes) => {
                    outcome.freed_bytes += bytes;
                    outcome.deleted.push(rel.clone());
                }
                Err(reason) => {
                    failure = Some((rel.clone(), reason));
                    break;
                }
            }
        }
        if let Some((failed, reason)) = failure {
            outcome.partial = true;
            let mut remaining: Vec<DeleteFile> = Vec::new();
            // The failed path and every path after it are still on disk.
            let mut seen_failed = false;
            for rel in &order {
                if *rel == failed {
                    seen_failed = true;
                }
                if seen_failed {
                    if rel == &failed {
                        remaining.push(DeleteFile {
                            path: rel.clone(),
                            bytes: 0,
                            missing: false,
                            reason: Some(reason.clone()),
                        });
                    } else if !outcome.deleted.contains(rel) {
                        remaining.push(DeleteFile::at(rel.clone(), 0, false));
                    }
                }
            }
            outcome.remaining = remaining;
            outcome
                .warnings
                .push(format!("{failed} could not be removed: {reason}"));
        }

        // 4. Playlists: drop the entries whose media is gone (only what was actually deleted).
        let gone: Vec<String> = outcome.deleted.iter().map(|p| p.to_lowercase()).collect();
        if !gone.is_empty() {
            self.scrub_playlists(plan.lib_idx, &gone, &mut outcome);
        }

        // 5. The release folder (and any sub-folder the delete emptied, e.g. `thumbnails/`),
        //    if nothing is left in it. Deepest first, so a nested folder goes before its parent.
        if let Some(folder) = plan.folder.clone() {
            if outcome.partial {
                outcome
                    .warnings
                    .push(format!("{folder}/ was left in place (it is not empty)"));
            } else {
                let mut dirs: Vec<String> = outcome
                    .deleted
                    .iter()
                    .map(|rel| rel_dir(rel).to_string())
                    .filter(|d| !d.is_empty() && under(d, &folder))
                    .collect();
                dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
                dirs.dedup();
                for dir in dirs {
                    if let Ok(abs) = lfs.resolve(Path::new(&dir)) {
                        let _ = fs::remove_dir(&abs);
                    }
                }
                match lfs.resolve(Path::new(&folder)) {
                    Ok(abs) if !abs.is_dir() => outcome.folder_removed = true,
                    Ok(_) => outcome.warnings.push(format!(
                        "{folder}/ still contains files that were not part of this item, so \
                         the folder was left in place"
                    )),
                    Err(e) => outcome
                        .warnings
                        .push(format!("{folder}/ could not be cleared: {e}")),
                }
            }
        }

        // 6. Rescan: prune orphan rows of files that did go away (and re-list a folder that
        //    was left behind by a partial delete).
        if let Err(e) = self.lazy_rescan() {
            outcome.warnings.push(format!(
                "the index rescan failed ({e}); it will catch up on the next scan"
            ));
        }

        Ok(outcome)
    }

    /// Drop `.noudata` rows (and their key mappings) from the index, one notification.
    fn forget_documents(
        &mut self,
        lib_idx: usize,
        rels: &[String],
    ) -> Result<(), DeleteError> {
        let mut acc = ScanReport::default();
        for rel in rels {
            let report = self.libraries[lib_idx]
                .writer
                .forget_noudata(rel)
                .map_err(lib_err)?;
            merge(&mut acc, report);
        }
        self.after_scan(&acc);
        Ok(())
    }

    /// Remove the planned `mediaList` entries from the release `.noudata` (guarded write) and
    /// re-ingest it.
    fn remove_items_from_document(&mut self, plan: &Plan) -> Result<(), DeleteError> {
        let Some(rel) = plan.noudata_rel.clone() else {
            return Err(DeleteError::refusal(
                400,
                "no_metadata",
                "this item has no .noudata to edit",
            ));
        };
        let Some(abs) = plan.noudata_abs.clone() else {
            return Err(DeleteError::refusal(
                409,
                "blocked",
                "the metadata file cannot be written",
            ));
        };
        let rev = FileRevision::of(&abs).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DeleteError::refusal(404, "not_found", "the metadata file no longer exists")
            } else {
                DeleteError::Server(e.into())
            }
        })?;
        let mut doc = read_noudata(&abs)?;
        let mut positions = plan.remove_positions.clone();
        positions.sort_unstable();
        positions.dedup();
        for pos in positions.iter().rev() {
            if *pos < doc.media_list.len() {
                doc.media_list.remove(*pos);
            }
        }
        if doc.media_list.is_empty() {
            return Err(DeleteError::refusal(
                400,
                "last_file",
                "that is the last media file of the item; delete the whole item instead",
            ));
        }
        // `resumeAtItem` is 1-indexed: keep it pointing at the same entry.
        if let Some(lp) = doc.local_playback_data.as_mut() {
            if let Some(resume) = lp.resume_at_item {
                let removed_here = positions.iter().any(|p| *p as u64 + 1 == resume);
                let removed_before = positions.iter().filter(|p| (**p as u64) < resume).count();
                if removed_here {
                    lp.resume_at_item = None;
                } else if removed_before > 0 {
                    lp.resume_at_item = Some(resume - removed_before as u64);
                }
            }
        }
        write_noudata_atomic(&doc, &abs, WriteGuard::Unchanged(rev)).map_err(|e| match e {
            AtomicWriteError::Conflict => DeleteError::refusal(
                409,
                "conflict",
                "the item changed while it was being edited; reload and retry",
            ),
            other => DeleteError::Server(write_err(other, "the item")),
        })?;
        self.index_write_through(plan.lib_idx, &rel, None)?;
        Ok(())
    }

    /// Remove playlist entries that point at media that was deleted. A playlist is never
    /// deleted here; a failure is a warning (the media is already gone).
    fn scrub_playlists(
        &mut self,
        lib_idx: usize,
        gone_lower: &[String],
        outcome: &mut DeleteOutcome,
    ) {
        let root = self.libraries[lib_idx].root.clone();
        let playlists = match self.libraries[lib_idx]
            .writer
            .list_by_kind(LibraryKind::Playlist)
        {
            Ok(p) => p,
            Err(e) => {
                outcome
                    .warnings
                    .push(format!("playlists could not be checked: {e}"));
                return;
            }
        };
        for pl in playlists {
            if pl.is_dynamic || pl.noudata_relpath.is_empty() {
                continue;
            }
            let dir = rel_dir(&pl.noudata_relpath).to_string();
            let Ok(doc) = read_noudata(&root.join(&pl.noudata_relpath)) else {
                continue;
            };
            let hits: Vec<usize> = doc
                .media_list
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    item.local_file_paths.iter().flatten().any(|p| {
                        let rel = resolve_in(&dir, p).to_lowercase();
                        !rel.is_empty() && gone_lower.contains(&rel)
                    })
                })
                .map(|(pos, _)| pos)
                .collect();
            if hits.is_empty() {
                continue;
            }
            let id = encode_playable_id(&root, pl.id);
            let title = non_empty(&pl.title).unwrap_or_else(|| pl.noudata_relpath.clone());
            let result = self.edit_playlist(id, None, true, |doc, _| {
                doc.media_list.retain(|item| {
                    !item.local_file_paths.iter().flatten().any(|p| {
                        let rel = resolve_in(&dir, p).to_lowercase();
                        !rel.is_empty() && gone_lower.contains(&rel)
                    })
                });
                Ok(None)
            });
            match result {
                Ok(_) => outcome.playlists_edited.push(id.to_string()),
                Err(e) => outcome.warnings.push(format!(
                    "the playlist {title:?} could not be updated ({e}); it now points at \
                     missing media"
                )),
            }
        }
    }
}
