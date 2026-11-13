//! NB_06: associate (Noubin §3.4) for every playable kind, including a single track of an
//! album (a per-track `.noudata` with `referToReleaseNoudata`).
//!
//! Flow (crash-safe ordering, unchanged from MVP_06/07): the UI finalises the tag write and
//! reads it back (`AssociateResponse.ok`), and only then does [`ServerService::finish_associate`]
//! write `.noudata` (atomic temp + rename). An NFC failure leaves every file untouched.
//!
//! Two tag situations:
//! * **Rewritable** — a new Local Noubin Key was written to the tag. The key is bound on the
//!   *new* target only; nothing is removed from the item the tag pointed at before (several
//!   Noubins per item is normal, other tags may still carry the old key). The response
//!   lists those items as `previous_items` so the UI can say "was X, now Y".
//! * **Locked / keep-existing** — the tag could not (or must not) be rewritten, so the key it
//!   already carries is bound on the target and, spec §3.4 step 3, **cleared from every other
//!   item** holding it (`cleared`). A locked tag with no readable key fails before any file
//!   is touched.

use super::{
    encode_playable_id, read_noudata, serialize_u64_as_string, AssociatePrep, ServerError,
    ServerService,
};
use plippa_ipc_proto::{AssociateResponse, IpcMessage};
use plippa_noubin::{
    normalise_noubin_key, write_noudata_atomic, AtomicWriteError, FileRevision, LibraryKind,
    NoudataDocument, PlayableRecord, WriteGuard,
};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Longest Noubin key we accept from a tag (NBSecurityRequirements §3, "Noubin keys").
pub const MAX_NOUBIN_KEY_BYTES: usize = 512;

/// Stable warning code: another item already carries the key that was just bound.
pub const WARN_DUPLICATE_KEY: &str = "duplicate_key";

/// One library item mentioned in an associate response.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AssociateItemRef {
    /// Namespaced playable id (decimal string in JSON).
    #[serde(serialize_with = "serialize_u64_as_string")]
    pub id: u64,
    pub title: String,
    pub kind: String,
    pub noudata_relpath: String,
}

/// What the key was bound to.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AssociateBound {
    #[serde(flatten)]
    pub item: AssociateItemRef,
    /// `mediaList` position when this is a single track of a release (per-track file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_item: Option<u32>,
    /// A per-track `.noudata` was created for this associate (false: reused / not per-track).
    pub created_track_file: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AssociateWarning {
    /// `duplicate_key` (see [`WARN_DUPLICATE_KEY`]).
    pub code: String,
    pub message: String,
    pub items: Vec<AssociateItemRef>,
}

/// Where the key is going: the playable's own `.noudata`, or a per-track file (existing, or
/// to be created after the tag is verified).
#[derive(Debug, Clone)]
pub struct AssociateTarget {
    pub lib_idx: usize,
    /// `.noudata` to write, library-relative.
    pub rel: String,
    /// Kind reported in the response.
    pub kind: LibraryKind,
    pub title: String,
    /// The playable's namespaced id; `None` for a per-track file that does not exist yet.
    pub id: Option<u64>,
    /// `mediaList` position (per-track associate only).
    pub track_item: Option<u32>,
    /// Document to create (per-track file that does not exist yet).
    pub new_doc: Option<NoudataDocument>,
}

fn is_release_kind(k: LibraryKind) -> bool {
    plippa_noubin::is_release_kind(k)
}

impl ServerService {
    /// Decide the write target for `id` (and optional `mediaList` position `item`). Nothing is
    /// created on disk here.
    pub(super) fn resolve_associate_target(
        &mut self,
        id: u64,
        item: Option<u32>,
        record: &PlayableRecord,
        lib_idx: usize,
        own_title: Option<String>,
    ) -> Result<AssociateTarget, ServerError> {
        let title = own_title
            .or_else(|| record.title.clone())
            .unwrap_or_else(|| "Playable".into());
        let Some(pos) = item else {
            return Ok(AssociateTarget {
                lib_idx,
                rel: record.noudata_relpath.clone(),
                kind: record.kind,
                title,
                id: Some(id),
                track_item: None,
                new_doc: None,
            });
        };
        if !is_release_kind(record.kind) {
            return Err(ServerError::Invalid(format!(
                "cannot associate a single item of a {}: only an album, mix, audiobook or \
                 podcast has tracks that can be associated one by one",
                record.kind.as_str()
            )));
        }
        let root = self.libraries[lib_idx].root.clone();
        let items = self.libraries[lib_idx].writer.items_of(record.id)?;
        let it = items.iter().find(|i| i.position == pos).ok_or_else(|| {
            ServerError::Invalid(format!(
                "no item {pos} in this collection ({} items)",
                items.len()
            ))
        })?;
        let media_rel = it.media_relpath.clone();
        let track_title = it.media_title.clone().unwrap_or_else(|| it.title.clone());

        // Reuse a per-track file that already refers to this release and lists this media.
        let existing = {
            let w = &self.libraries[lib_idx].writer;
            let mut found: Option<PlayableRecord> = None;
            for rec in w.list_by_kind(LibraryKind::Track)? {
                if rec.is_orphan
                    || rec.refers_to_relpath.as_deref() != Some(record.noudata_relpath.as_str())
                {
                    continue;
                }
                if w.items_of(rec.id)?
                    .iter()
                    .any(|x| x.media_relpath == media_rel)
                {
                    found = Some(rec);
                    break;
                }
            }
            found
        };

        // NB_18: the document comes from the one shared implementation of §4.5, so the portal
        // and the device UI cannot drift.
        let target = plippa_noubin::track_associate_target(
            &root,
            &record.noudata_relpath,
            &media_rel,
            &track_title,
            existing.as_ref().map(|r| r.noudata_relpath.as_str()),
        )
        .map_err(ServerError::Invalid)?;

        if let Some(rec) = existing {
            // An existing per-track file keeps its own title and its id.
            let t = target
                .doc
                .playable_item_title
                .clone()
                .or(rec.title.clone())
                .unwrap_or(track_title);
            return Ok(AssociateTarget {
                lib_idx,
                rel: target.relpath,
                kind: LibraryKind::Track,
                title: t,
                id: Some(encode_playable_id(&root, rec.id)),
                track_item: Some(pos),
                new_doc: None,
            });
        }
        Ok(AssociateTarget {
            lib_idx,
            rel: target.relpath,
            kind: LibraryKind::Track,
            title: track_title,
            id: None,
            track_item: Some(pos),
            new_doc: target.created.then_some(target.doc),
        })
    }

    fn item_ref(&self, lib_idx: usize, rec: &PlayableRecord) -> AssociateItemRef {
        AssociateItemRef {
            id: encode_playable_id(&self.libraries[lib_idx].root, rec.id),
            title: rec.title.clone().unwrap_or_else(|| {
                Path::new(&rec.noudata_relpath)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("(untitled)")
                    .to_string()
            }),
            kind: rec.kind.as_str().to_string(),
            noudata_relpath: rec.noudata_relpath.clone(),
        }
    }

    /// Every indexed playable (all volumes) whose normalised key is `norm`, except the
    /// target file itself.
    fn holders_of_key(
        &self,
        norm: &str,
        target_lib: usize,
        target_rel: &str,
    ) -> Vec<(usize, PlayableRecord)> {
        let mut out = Vec::new();
        for (i, lib) in self.libraries.iter().enumerate() {
            let Ok(recs) = lib.writer.find_by_key(norm) else {
                continue;
            };
            for r in recs {
                if i == target_lib && r.noudata_relpath == target_rel {
                    continue;
                }
                out.push((i, r));
            }
        }
        out
    }

    /// Set (or clear, with `None`) the keys in `rel` and write atomically. Returns the key the
    /// file carried before.
    fn write_keys(
        &self,
        lib_idx: usize,
        rel: &str,
        new_doc: Option<&NoudataDocument>,
        keys: Option<(&str, &str)>,
    ) -> Result<Option<String>, ServerError> {
        let path: PathBuf = self.libraries[lib_idx].root.join(rel);
        let (mut doc, guard) = match new_doc {
            Some(d) => (d.clone(), WriteGuard::MustNotExist),
            None => {
                // Revision first, content second: an edit in between shows up as a conflict.
                let rev = FileRevision::of(&path)?;
                (read_noudata(&path)?, WriteGuard::Unchanged(rev))
            }
        };
        let before = doc.noubin_key_normalised.clone();
        match keys {
            Some((k, n)) => doc.set_noubin_keys(k, n),
            None => {
                doc.noubin_key = None;
                doc.noubin_key_normalised = None;
            }
        }
        write_noudata_atomic(&doc, &path, guard).map_err(|e| match e {
            AtomicWriteError::Conflict => ServerError::Conflict(
                "the .noudata changed while the tag was being written; try again".into(),
            ),
            AtomicWriteError::Io(e) => e.into(),
            AtomicWriteError::Noudata(e) => e.into(),
        })?;
        Ok(before)
    }

    pub fn finish_associate(
        &mut self,
        prep: AssociatePrep,
        resp: AssociateResponse,
    ) -> Result<AssociateOutcome, ServerError> {
        self.with_ipc(|ipc| ipc.send(IpcMessage::AssociateResponse(resp.clone())));
        let t = &prep.target;
        let path = self.libraries[t.lib_idx].root.join(&t.rel);

        if !resp.ok {
            // Nothing may have changed on disk (transactional contract).
            let unchanged = match (&t.new_doc, read_noudata(&path)) {
                (Some(_), Err(_)) => !path.exists(),
                (Some(_), Ok(_)) => false,
                (None, Ok(after)) => {
                    after.noubin_key == prep.doc_before.noubin_key
                        && after.noubin_key_normalised == prep.doc_before.noubin_key_normalised
                }
                (None, Err(e)) => return Err(e),
            };
            if !unchanged {
                return Err(ServerError::Message(
                    "internal error: .noudata changed after NFC failure".into(),
                ));
            }
            return Ok(AssociateOutcome::failed(
                resp.error.unwrap_or_else(|| "NFC associate failed".into()),
            ));
        }

        let key = resp
            .noubin_key
            .clone()
            .unwrap_or_else(|| prep.fallback_key.clone());
        let normalised = resp
            .noubin_key_normalised
            .clone()
            .unwrap_or_else(|| normalise_noubin_key(&key));
        if key.len() > MAX_NOUBIN_KEY_BYTES {
            return Ok(AssociateOutcome::failed(format!(
                "Tag key is too long ({} bytes, limit {MAX_NOUBIN_KEY_BYTES}) — not saved",
                key.len()
            )));
        }
        if normalised.is_empty() {
            return Ok(AssociateOutcome::failed(
                "Tag key is blank after normalisation — cannot keep existing key",
            ));
        }
        let locked = resp.tag_locked || prep.req.keep_existing_key;

        // Snapshot of who holds the keys involved, before anything is written.
        let others = self.holders_of_key(&normalised, t.lib_idx, &t.rel);
        let previous_norm = resp
            .previous_key
            .as_deref()
            .map(normalise_noubin_key)
            .filter(|p| !p.is_empty() && *p != normalised);
        let previous: Vec<(usize, PlayableRecord)> = match (&previous_norm, locked) {
            (Some(p), false) => self.holders_of_key(p, t.lib_idx, &t.rel),
            _ => Vec::new(),
        };

        // 1. bind the target (the tag is final, so the key is always bound here first).
        let own_before = self.write_keys(
            t.lib_idx,
            &t.rel,
            t.new_doc.as_ref(),
            Some((&key, &normalised)),
        )?;

        // 2. locked tag: move semantics — clear the key everywhere else.
        let mut cleared = Vec::new();
        let mut leftovers: Vec<AssociateItemRef> = Vec::new();
        if locked {
            for (i, rec) in &others {
                let iref = self.item_ref(*i, rec);
                if rec.is_orphan || rec.noudata_relpath.is_empty() {
                    continue;
                }
                let p = self.libraries[*i].root.join(&rec.noudata_relpath);
                // Trust the file, not the index: only clear if it really carries this key.
                let carries = read_noudata(&p)
                    .map(|d| d.noubin_key_normalised.as_deref() == Some(normalised.as_str()))
                    .unwrap_or(false);
                if !carries {
                    continue;
                }
                match self.write_keys(*i, &rec.noudata_relpath, None, None) {
                    Ok(_) => cleared.push(iref),
                    Err(e) => {
                        eprintln!(
                            "plippa-server: associate: could not clear key from {}: {e}",
                            rec.noudata_relpath
                        );
                        leftovers.push(iref);
                    }
                }
            }
        } else {
            for (i, rec) in &others {
                leftovers.push(self.item_ref(*i, rec));
            }
        }

        let _ = self.lazy_rescan();

        // Resolve the bound item's id/title from the refreshed index.
        let root = self.libraries[t.lib_idx].root.clone();
        let (bound_id, bound_kind) = match t.id {
            Some(id) => (id, t.kind),
            None => {
                let rec = self.libraries[t.lib_idx]
                    .writer
                    .list_by_kind(LibraryKind::Track)?
                    .into_iter()
                    .find(|r| r.noudata_relpath == t.rel);
                match rec {
                    Some(r) => (encode_playable_id(&root, r.id), LibraryKind::Track),
                    None => (0, LibraryKind::Track),
                }
            }
        };
        let mut warnings = Vec::new();
        // Duplicates that remain after the move (or, on a rewritable tag, that predate it).
        if !leftovers.is_empty() {
            warnings.push(AssociateWarning {
                code: WARN_DUPLICATE_KEY.into(),
                message: format!(
                    "{} other item(s) still carry this Noubin key; tapping the tag plays the \
                     first by file path",
                    leftovers.len()
                ),
                items: leftovers,
            });
        }
        let previous_items: Vec<AssociateItemRef> =
            previous.iter().map(|(i, r)| self.item_ref(*i, r)).collect();
        let target_previous_key = own_before.filter(|b| *b != normalised);
        Ok(AssociateOutcome {
            ok: true,
            noubin_key: Some(key),
            noubin_key_normalised: Some(normalised),
            error: None,
            tag_locked: locked,
            bound: Some(AssociateBound {
                item: AssociateItemRef {
                    id: bound_id,
                    title: t.title.clone(),
                    kind: bound_kind.as_str().to_string(),
                    noudata_relpath: t.rel.clone(),
                },
                track_item: t.track_item,
                created_track_file: t.new_doc.is_some(),
            }),
            previous_titles: previous_items.iter().map(|i| i.title.clone()).collect(),
            previous_items,
            cleared,
            replaced_key: target_previous_key,
            warnings,
        })
    }
}

/// Result of an associate as sent to the portal.
///
/// The first four fields are the pre-NB_06 response and keep their meaning.
#[derive(Debug, Clone, Serialize)]
pub struct AssociateOutcome {
    pub ok: bool,
    pub noubin_key: Option<String>,
    pub noubin_key_normalised: Option<String>,
    pub error: Option<String>,
    /// The tag could not be rewritten: its existing key was bound (move semantics applied).
    pub tag_locked: bool,
    /// The item the key now points at (a new per-track file when `created_track_file`).
    pub bound: Option<AssociateBound>,
    /// Rewritable tag: items that *still* carry the key the tag held before (they were not
    /// touched). Empty when the tag was blank / unreadable.
    pub previous_items: Vec<AssociateItemRef>,
    /// Titles of [`Self::previous_items`] ("was X, now also/instead Y").
    pub previous_titles: Vec<String>,
    /// Locked tag: items that lost the key because it now belongs to `bound`.
    pub cleared: Vec<AssociateItemRef>,
    /// The bound item's own previous normalised key, when it had a different one (that key
    /// is no longer resolvable to this item).
    pub replaced_key: Option<String>,
    pub warnings: Vec<AssociateWarning>,
}

impl AssociateOutcome {
    pub fn failed(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            noubin_key: None,
            noubin_key_normalised: None,
            error: Some(msg.into()),
            tag_locked: false,
            bound: None,
            previous_items: Vec::new(),
            previous_titles: Vec::new(),
            cleared: Vec::new(),
            replaced_key: None,
            warnings: Vec::new(),
        }
    }
}
