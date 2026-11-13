//! NB_05: timestamp-paste cue import — preview, save (write-through) and read-back.
//!
//! The owned cue list: the **first** `cueListType: "nav"` list of the item. Saving replaces
//! only that list (in place, keeping its title/language/extra fields unless a title is
//! given); lyrics, `credits`, other-language and any further `nav` lists are untouched. If
//! the item has no `nav` list, one is appended. Empty paste text removes the owned list.
//!
//! Preview and save share [`plippa_noubin::parse_cue_paste`], so what the preview shows is
//! what is written. Saving re-serialises the whole `.noudata` (same lossy write-through as
//! NB_03: elements the NB_01 parser drops as invalid are not kept).

use super::{
    path_relative_posix, read_noudata, serialize_u64_as_string, IdArg, ServerError, ServerService,
};
use crate::scanner::probe_duration_of_file;
use plippa_noubin::{
    cue_list_to_paste_text, format_hms_ms, parse_cue_paste, sanitise_cue_text,
    write_noudata_atomic, AtomicWriteError, CueList, CueListType, CuePaste, CueWarning,
    FileRevision, IgnoredLine, LibraryKind, NoudataDocument, PlayableRecord, WriteGuard,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Longest `text` accepted by preview/save (bytes). Bigger pastes are rejected with 400.
pub const MAX_CUE_PASTE_BYTES: usize = 256 * 1024;
/// Longest cue-list title (chars).
const MAX_CUE_LIST_TITLE_CHARS: usize = 256;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// `POST /api/cues/preview`
#[derive(Debug, Clone, Deserialize)]
pub struct CuePreviewRequest {
    pub text: String,
    /// Media length, when the client knows it. Cues after it are dropped with a warning.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Alternatively name the item whose real length should be used (`duration_ms` wins).
    #[serde(default)]
    pub id: Option<IdArg>,
    #[serde(default)]
    pub item: Option<u32>,
}

/// `PUT /api/items/:id/cues`
#[derive(Debug, Clone, Deserialize)]
pub struct SaveCuesRequest {
    /// The pasted timestamps. Blank = remove the owned `nav` list.
    pub text: String,
    /// `mediaList` position (default 0; also accepted as `?item=`).
    #[serde(default)]
    pub item: Option<u32>,
    /// Optional `cueListTitle` for the owned list (kept from the existing list when absent).
    #[serde(default)]
    pub cue_list_title: Option<String>,
    /// `revision` from the last `GET`; mismatch = 409.
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CueOut {
    /// `HH:MM:SS.mmm`
    pub start: String,
    pub start_ms: u64,
    pub text: String,
    /// 1-based line in the pasted text (preview / save only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CuePreview {
    pub cues: Vec<CueOut>,
    pub cue_count: usize,
    pub ignored_lines: Vec<IgnoredLine>,
    pub warnings: Vec<CueWarning>,
    /// The length the cues were checked against (`null` = unknown, checked on save).
    pub duration_ms: Option<u64>,
}

/// Result of a save (`PUT`), also embedded in the `POST /api/media` response.
#[derive(Debug, Clone, Serialize)]
pub struct CueSaveReport {
    pub saved: bool,
    /// The owned `nav` list was removed (blank text).
    pub cleared: bool,
    pub item: u32,
    pub cue_count: usize,
    pub cues: Vec<CueOut>,
    pub ignored_lines: Vec<IgnoredLine>,
    pub warnings: Vec<CueWarning>,
    pub duration_ms: Option<u64>,
    /// Index in the item's `cueLists` of the list that was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cue_list_index: Option<usize>,
    /// Whether an existing `nav` list was replaced (vs. appended).
    pub replaced: bool,
    /// New file revision (send it with the next save).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CueListOut {
    pub index: usize,
    pub cue_list_type: String,
    pub title: Option<String>,
    pub language: Option<String>,
    pub navigable: bool,
    /// The list `PUT` would replace.
    pub owned: bool,
    pub cues: Vec<CueOut>,
}

/// `GET /api/items/:id/cues`
#[derive(Debug, Clone, Serialize)]
pub struct ItemCues {
    #[serde(serialize_with = "serialize_u64_as_string")]
    pub id: u64,
    pub item: u32,
    pub item_count: usize,
    pub title: Option<String>,
    pub kind: LibraryKind,
    pub duration_ms: Option<u64>,
    pub cue_lists: Vec<CueListOut>,
    /// Index of the owned (first `nav`) list, if any.
    pub owned_index: Option<usize>,
    /// Paste text for the owned list; re-saving it unchanged is a no-op semantically.
    pub text: String,
    pub cue_list_title: Option<String>,
    pub revision: String,
}

impl CuePreview {
    fn from_paste(p: &CuePaste, duration_ms: Option<u64>) -> Self {
        Self {
            cue_count: p.cues.len(),
            cues: cue_outs(p),
            ignored_lines: p.ignored_lines.clone(),
            warnings: p.warnings.clone(),
            duration_ms,
        }
    }
}

fn cue_outs(p: &CuePaste) -> Vec<CueOut> {
    p.cues
        .iter()
        .map(|c| CueOut {
            start: format_hms_ms(c.start.as_millis()),
            start_ms: c.start.as_millis(),
            text: c.text.clone(),
            line: Some(c.line),
        })
        .collect()
}

fn list_cue_outs(list: &CueList) -> Vec<CueOut> {
    list.cues
        .iter()
        .map(|c| CueOut {
            start: format_hms_ms(c.start_time.as_millis()),
            start_ms: c.start_time.as_millis(),
            text: c.text(),
            line: None,
        })
        .collect()
}

/// Parse + length-check a paste. Shared by preview, save and `add_media`.
fn interpret(text: &str, duration_ms: Option<u64>) -> Result<CuePaste, ServerError> {
    if text.len() > MAX_CUE_PASTE_BYTES {
        return Err(ServerError::Invalid(format!(
            "pasted text is too long ({} KiB max)",
            MAX_CUE_PASTE_BYTES / 1024
        )));
    }
    let mut paste = parse_cue_paste(text);
    if let Some(len) = duration_ms {
        paste.drop_beyond_length(len);
    }
    Ok(paste)
}

/// The list `PUT` owns: the first `nav` list.
pub(super) fn owned_index(lists: &[CueList]) -> Option<usize> {
    lists.iter().position(|l| l.list_type() == CueListType::Nav)
}

fn clean_list_title(t: &str) -> Option<String> {
    let t = sanitise_cue_text(t);
    let t: String = t.chars().take(MAX_CUE_LIST_TITLE_CHARS).collect();
    (!t.is_empty()).then_some(t)
}

/// `POST /api/cues/preview` without a service (no writes, no lookups).
pub fn preview_cues(text: &str, duration_ms: Option<u64>) -> Result<CuePreview, ServerError> {
    Ok(CuePreview::from_paste(
        &interpret(text, duration_ms)?,
        duration_ms,
    ))
}

/// Build the report for a paste that has already been written into a document.
pub(super) fn report_for(
    paste: &CuePaste,
    item: u32,
    duration_ms: Option<u64>,
    cue_list_index: Option<usize>,
    replaced: bool,
    revision: Option<String>,
) -> CueSaveReport {
    CueSaveReport {
        saved: true,
        cleared: false,
        item,
        cue_count: paste.cues.len(),
        cues: cue_outs(paste),
        ignored_lines: paste.ignored_lines.clone(),
        warnings: paste.warnings.clone(),
        duration_ms,
        cue_list_index,
        replaced,
        revision,
    }
}

/// Validate a paste for `add_media`: must contain at least one cue. Returns the paste.
pub(super) fn paste_for_new_media(
    text: &str,
    duration_ms: Option<u64>,
) -> Result<CuePaste, ServerError> {
    let paste = interpret(text, duration_ms)?;
    require_some_cues(&paste, text)?;
    Ok(paste)
}

fn require_some_cues(paste: &CuePaste, text: &str) -> Result<(), ServerError> {
    if paste.cues.is_empty() {
        let why = if paste.ignored_lines.is_empty() && paste.warnings.is_empty() {
            "the text is empty".to_string()
        } else {
            format!(
                "no usable timestamps found ({} line(s) ignored, {} warning(s)); \
                 lines must look like `3:45 Title`",
                paste.ignored_lines.len(),
                paste.warnings.len()
            )
        };
        let _ = text;
        return Err(ServerError::Invalid(format!("no cues to save: {why}")));
    }
    Ok(())
}

/// Length of one item in ms: measured from the media file when readable, else the index's
/// value (a declared `expectedLength` or an earlier probe), else unknown.
///
/// The path comes from the index and a `.noudata` on a stick (untrusted): only a plain regular
/// file below `root` (no symlink, FIFO, device, escape) is ever handed to the tag reader, and an
/// unavailable item is not touched at all.
fn item_duration_ms(
    root: &Path,
    media_relpath: &str,
    available: bool,
    indexed: Option<u64>,
) -> Option<u64> {
    if available && !media_relpath.is_empty() {
        if let Ok(media) = plippa_noubin::confine::open_library_media(root, media_relpath) {
            // The checked handle itself is read: no re-open by path.
            if let Some(ms) = probe_duration_of_file(media.file) {
                return Some(ms);
            }
        }
    }
    indexed
}

/// Set (or replace/remove) the owned `nav` list of `doc.media_list[item]`. Returns
/// `(cue_list_index, replaced)`.
pub(super) fn apply_owned_list(
    doc: &mut NoudataDocument,
    item: usize,
    paste: &CuePaste,
    title: Option<&str>,
) -> (Option<usize>, bool) {
    let lists = &mut doc.media_list[item].cue_lists;
    let owned = owned_index(lists);
    if paste.cues.is_empty() {
        if let Some(i) = owned {
            lists.remove(i);
        }
        return (None, owned.is_some());
    }
    match owned {
        Some(i) => {
            let fresh = paste.to_cue_list(title);
            let old = &mut lists[i];
            old.cues = fresh.cues;
            old.cue_list_navigable = Some(true);
            if title.is_some() {
                old.cue_list_title = fresh.cue_list_title;
            }
            (Some(i), true)
        }
        None => {
            lists.push(paste.to_cue_list(title));
            (Some(lists.len() - 1), false)
        }
    }
}

impl ServerService {
    /// The playable behind `id`, if cues may be edited on it (not a playlist / orphan).
    fn cue_target(&mut self, id: u64) -> Result<(usize, PlayableRecord), ServerError> {
        let (lib_idx, rec) = self.require_record(id)?;
        if rec.is_orphan || rec.noudata_relpath.is_empty() {
            return Err(ServerError::Invalid(
                "this media file has no .noudata (metadata) yet; create it first \
                 (portal: “Create metadata…”, or POST /api/items/:id/noudata) before adding cues"
                    .into(),
            ));
        }
        if rec.kind == LibraryKind::Playlist {
            return Err(ServerError::Invalid(
                "cues belong to an album, mix, audiobook, podcast or track — \
                 not to a playlist"
                    .into(),
            ));
        }
        Ok((lib_idx, rec))
    }

    /// Real length of `item` of the playable (see [`item_duration_ms`]).
    fn item_length(&self, lib_idx: usize, rec: &PlayableRecord, item: u32) -> Option<u64> {
        let lib = &self.libraries[lib_idx];
        let items = lib.writer.items_of(rec.id).ok()?;
        let it = items.iter().find(|i| i.position == item)?;
        item_duration_ms(&lib.root, &it.media_relpath, it.available, it.duration_ms)
    }

    /// `POST /api/cues/preview` — pure interpretation, never writes. With `id` (and
    /// optionally `item`) and no `duration_ms`, the item's real length is used.
    pub fn preview_cues(&mut self, req: &CuePreviewRequest) -> Result<CuePreview, ServerError> {
        let mut duration = req.duration_ms;
        if duration.is_none() {
            if let Some(IdArg(id)) = req.id {
                let (lib_idx, rec) = self.cue_target(id)?;
                duration = self.item_length(lib_idx, &rec, req.item.unwrap_or(0));
            }
        }
        preview_cues(&req.text, duration)
    }

    /// `GET /api/items/:id/cues[?item=N]`
    pub fn get_item_cues(&mut self, id: u64, item: Option<u32>) -> Result<ItemCues, ServerError> {
        let (lib_idx, rec) = self.require_record(id)?;
        if rec.is_orphan || rec.noudata_relpath.is_empty() {
            return Err(ServerError::Invalid(
                "this media file has no .noudata yet, so it has no cues".into(),
            ));
        }
        let item = item.unwrap_or(0);
        let path = self.libraries[lib_idx].root.join(&rec.noudata_relpath);
        let rev = FileRevision::of(&path).map_err(ServerError::Io)?;
        let doc = read_noudata(&path)?;
        let Some(mi) = doc.media_list.get(item as usize) else {
            return Err(ServerError::NotFound(format!(
                "no item {item} (this file has {} item(s))",
                doc.media_list.len()
            )));
        };
        let owned = owned_index(&mi.cue_lists);
        let cue_lists = mi
            .cue_lists
            .iter()
            .enumerate()
            .map(|(i, l)| CueListOut {
                index: i,
                cue_list_type: l.list_type().as_str().to_string(),
                title: l.cue_list_title.clone(),
                language: l.cue_list_language.clone(),
                navigable: l.is_navigable(),
                owned: Some(i) == owned,
                cues: list_cue_outs(l),
            })
            .collect();
        Ok(ItemCues {
            id,
            item,
            item_count: doc.media_list.len(),
            title: rec.title.clone().or(doc.playable_item_title.clone()),
            kind: rec.kind,
            duration_ms: self.item_length(lib_idx, &rec, item),
            cue_lists,
            owned_index: owned,
            text: owned
                .map(|i| cue_list_to_paste_text(&mi.cue_lists[i]))
                .unwrap_or_default(),
            cue_list_title: owned.and_then(|i| mi.cue_lists[i].cue_list_title.clone()),
            revision: rev.token(),
        })
    }

    /// `PUT /api/items/:id/cues` — replace the owned `nav` list from pasted text.
    pub fn put_item_cues(
        &mut self,
        id: u64,
        req: SaveCuesRequest,
    ) -> Result<CueSaveReport, ServerError> {
        let (lib_idx, rec) = self.cue_target(id)?;
        let item = req.item.unwrap_or(0);
        let root = self.libraries[lib_idx].root.clone();
        let path = root.join(&rec.noudata_relpath);
        let rev = FileRevision::of(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ServerError::NotFound("the .noudata file no longer exists".into())
            } else {
                e.into()
            }
        })?;
        if let Some(seen) = req.revision.as_deref().filter(|r| !r.is_empty()) {
            if seen != rev.token() {
                return Err(ServerError::Conflict(
                    "the item changed since you loaded it; reload and retry".into(),
                ));
            }
        }
        let mut doc = read_noudata(&path)?;
        if FileRevision::of(&path).ok() != Some(rev) {
            return Err(ServerError::Conflict(
                "the item changed while it was being read; retry".into(),
            ));
        }
        if doc.media_list.get(item as usize).is_none() {
            return Err(ServerError::NotFound(format!(
                "no item {item} (this file has {} item(s))",
                doc.media_list.len()
            )));
        }
        let duration = self.item_length(lib_idx, &rec, item);
        let clearing = req.text.trim().is_empty();
        let paste = interpret(&req.text, duration)?;
        if !clearing {
            // A paste with text but no usable line is refused rather than wiping the list.
            require_some_cues(&paste, &req.text)?;
        }
        let title = req.cue_list_title.as_deref().and_then(clean_list_title);
        let (index, replaced) = apply_owned_list(&mut doc, item as usize, &paste, title.as_deref());

        let new_rev =
            write_noudata_atomic(&doc, &path, WriteGuard::Unchanged(rev)).map_err(|e| match e {
                AtomicWriteError::Conflict => ServerError::Conflict(
                    "the item was changed by someone else while it was being edited; \
                     reload and retry"
                        .into(),
                ),
                AtomicWriteError::Io(e) => ServerError::Io(e),
                AtomicWriteError::Noudata(e) => ServerError::Noudata(e),
            })?;
        let _ = path_relative_posix(&root, &path)?; // stays inside the library root
        let _ = self.lazy_rescan(); // index row + generation bump + LibraryIndexUpdated

        let mut report = report_for(
            &paste,
            item,
            duration,
            index,
            replaced,
            Some(new_rev.token()),
        );
        report.cleared = clearing;
        Ok(report)
    }
}
