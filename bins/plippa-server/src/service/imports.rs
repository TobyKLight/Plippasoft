//! NB_11: streamed uploads and multi-file imports (album / playlist).
//!
//! **Nothing is buffered in RAM.** Every uploaded file is streamed to a temp file in
//! `<library>/.plippa/tmp` (same volume as the library, so the final move is an atomic rename),
//! its first bytes are sniffed (the content decides it is audio, never the name), and caps /
//! free space are enforced while streaming ([`crate::web::download::stream_to_temp`], the same
//! core the unite upload uses). A browser that dies mid-upload leaves a short body: the partial
//! temp file is deleted and nothing else was written.
//!
//! Multi-add is a small session protocol, so a failed file never poisons the rest and the
//! collection exists only once `finish` succeeds:
//!
//! 1. `POST /api/imports` `{single?: bool}` -> `{id}` (creates the staging folder
//!    `.plippa/tmp/import-<id>/`). A `single` session takes one audio file (the single "Add
//!    media" dialog) and finishes into a loose track instead of a collection.
//! 2. `POST /api/imports/<id>/files?name=<file name>` (raw body) — once per file; a refused file
//!    is simply not registered (retry = upload again). An **audio** file answers `{file_id,
//!    name, size, title, artist, album, art, art_skipped, merged}`; an **image** file (JPEG, PNG
//!    or WebP by its bytes) is staged as cover art and answers `{kind: "image", name, size, art,
//!    art_skipped: 0, merged}`. `art` is the deduplicated list of the whole session after this
//!    file (`{art_id, url, w, h, bytes, kind, origin, file_id|name}`), `art_skipped` the pictures
//!    of this file that were left out (wrong type, over a limit, over the caps), `merged` every
//!    merge so far (`{dropped, kept, exact}`). Art embedded in audio files is extracted
//!    ([`covers::extract_embedded`]) and joins the same set.
//! 3. `GET /api/imports/<id>/art/<art_id>` the staged image (preview);
//!    `DELETE /api/imports/<id>/art/<art_id>` removes that picture and every later copy of it;
//!    `PUT /api/imports/<id>/art/order` `{order: [art_id...]}` sets the save order (unknown ids
//!    are 400, ids merged into another image count as it). Both answer `{art, merged}`.
//! 4. `POST /api/imports/<id>/finish`
//!    - album / playlist: `{mode: "album"|"playlist", name, order: [file_id...], art?: [art_id...]}`
//!    - single: `{kind, cue_text?, title?, order?: [file_id], art?}`
//!
//!    `art` is the final ordered set (default: the session's current visible images). At least
//!    one audio file must be chosen (400 `no_media`). An album's images become `cover.<ext>`,
//!    `cover-2.<ext>`... with their thumbnails and `localCoverImage`; a playlist's become
//!    `<playlist file stem>-cover*.<ext>` in `Playlists/`; a single track's go to its
//!    `itemCoverImage` (embedded art is never copied onto each track of a playlist).
//! 5. `DELETE /api/imports/<id>` discards the session and its staged files, art included.
//!
//! `finish` is all-or-nothing: on any error every move/write is undone (the files go back to
//! staging, the images and thumbnails are removed again, the session stays usable) and no
//! `.noudata` of the collection remains.

use super::art_staging::{ArtOrigin, ArtStaging, StagedArt};
use super::covers::{extract_embedded, OrderEntry, PutCovers, MAX_COVER_BYTES};
use super::{
    actual_rel_path, list_by_rel, unique_media_path, CollectionDetail, CreatePlaylist, IdArg,
    ServerError, ServerService, TrackRef,
};
use crate::tags::extract_tags_from_path;
use crate::unite::fsops::{LibraryFs, Transaction};
use crate::unite::sniff::{sniff_audio, sniff_image_kind, MediaSniff};
use crate::admission::{
    place_session, ActivityReader, Placement, SessionLimits, SessionRow, UploadGate, UploadSlot,
};
use crate::web::download::{
    require_playable, stream_to_temp, Budget, Cancel, DownloadError,
    DownloadLimits, StagingBudget, StreamRules, Streamed,
};
use plippa_noubin::{
    safe_path_segment, validate_playlist_title, Contributor, Credits, ExtractedFields, ImportKind,
    MediaListItem, NoudataDocument, NoudataType, PlayableItemProfile, PrimaryArtist, ReleaseData,
    MAX_SEGMENT_BYTES,
    NOUDATA_VERSION, WHAT_AM_I_LOOKING_AT,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Files one import session may hold.
pub const MAX_IMPORT_FILES: usize = 500;
/// Open sessions per server (each holds a staging folder on the stick).
const MAX_IMPORT_SESSIONS: usize = 4;
/// A session unused this long is dropped together with its staged files. An upload that is
/// still streaming keeps its session alive (it is stamped as the body arrives).
const SESSION_TTL: Duration = Duration::from_secs(2 * 60 * 60);

/// An import problem with the HTTP status it maps to.
#[derive(Debug, Clone)]
pub struct ImportError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    /// The file a refused upload was, as the client named it (untrusted text: the portal shows
    /// it with `textContent` only). Lets a multi-file import name every refused file.
    pub file: Option<String>,
}

impl ImportError {
    pub(crate) fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            file: None,
        }
    }

    fn for_file(mut self, name: &str) -> Self {
        self.file = Some(name.chars().take(255).collect());
        self
    }

    pub fn to_json(&self) -> Value {
        match &self.file {
            Some(name) => json!({"error": self.message, "code": self.code, "name": name}),
            None => json!({"error": self.message, "code": self.code}),
        }
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<DownloadError> for ImportError {
    fn from(e: DownloadError) -> Self {
        let (status, code) = match &e {
            DownloadError::TooLarge { .. } | DownloadError::BatchTooLarge { .. } => {
                (413, "too_large")
            }
            DownloadError::NoSpace { .. } => (507, "no_space"),
            DownloadError::WrongType(_) => (415, "wrong_type"),
            DownloadError::Truncated => (400, "truncated"),
            DownloadError::Timeout => (408, "timeout"),
            DownloadError::Cancelled => (409, "cancelled"),
            _ => (500, "io"),
        };
        Self::new(status, code, e.to_string())
    }
}

impl From<ServerError> for ImportError {
    fn from(e: ServerError) -> Self {
        let (status, code) = match &e {
            ServerError::Invalid(_) => (400, "invalid"),
            ServerError::NotFound(_) => (404, "not_found"),
            ServerError::Conflict(_) => (409, "conflict"),
            ServerError::Unavailable(_) => (503, "unavailable"),
            _ => (500, "failed"),
        };
        Self::new(status, code, e.to_string())
    }
}

/// Stream an audio upload to a new temp file in `tmp_dir`. `declared` = `Content-Length`,
/// checked against the cap and the free space before a byte is read. Errors leave nothing.
pub fn stream_audio_upload(
    body: &mut dyn Read,
    declared: Option<u64>,
    tmp_dir: &Path,
    limits: &DownloadLimits,
    budget: &Budget,
    cancel: &Cancel,
) -> Result<Streamed, DownloadError> {
    stream_upload(body, declared, tmp_dir, limits, budget, cancel, false)
}

/// [`stream_audio_upload`], optionally accepting a JPEG / PNG / WebP image too (capped at
/// [`MAX_COVER_BYTES`] once the first bytes name it).
fn stream_upload(
    body: &mut dyn Read,
    declared: Option<u64>,
    tmp_dir: &Path,
    limits: &DownloadLimits,
    budget: &Budget,
    cancel: &Cancel,
    images: bool,
) -> Result<Streamed, DownloadError> {
    if let Some(len) = declared {
        if len > limits.max_file_bytes {
            return Err(DownloadError::TooLarge {
                limit: limits.max_file_bytes,
            });
        }
        if let Some(free) = (limits.free_space)(tmp_dir) {
            if len.saturating_add(limits.reserve_for(tmp_dir)) > free {
                return Err(DownloadError::NoSpace { needed: len, free });
            }
        }
        budget.admit_shared(limits, tmp_dir, len)?;
    }
    let cap = limits.max_file_bytes;
    let mut classify = |head: &[u8]| -> Result<Option<u64>, DownloadError> {
        match sniff_audio(head) {
            MediaSniff::Audio(kind) => {
                require_playable(&limits.capabilities, kind)?;
                Ok(Some(cap))
            }
            MediaSniff::Unsupported(w) => Err(DownloadError::WrongType(w.into())),
            MediaSniff::Unknown if images && sniff_image_kind(head).is_some() => {
                Ok(Some(MAX_COVER_BYTES.min(cap)))
            }
            MediaSniff::Unknown => Err(DownloadError::WrongType(if images {
                "not an audio file Plippa can play, or a JPEG, PNG or WebP image".into()
            } else {
                "not an audio file Plippa can play".into()
            })),
        }
    };
    stream_to_temp(
        body,
        declared,
        tmp_dir,
        StreamRules {
            cap,
            lim: limits,
            budget,
            cancel,
        },
        &mut classify,
        &mut |_, _| {},
    )
}

/// `mode` of `POST /api/imports/<id>/finish`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportMode {
    Album,
    Playlist,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImportFinish {
    /// Album / playlist sessions only.
    #[serde(default)]
    pub mode: Option<ImportMode>,
    /// Album / playlist sessions only.
    #[serde(default)]
    pub name: Option<String>,
    /// `file_id`s in track order; files not listed are dropped. A `single` session may leave
    /// it out (its one staged file).
    #[serde(default)]
    pub order: Vec<u32>,
    /// The final ordered cover images (`art_id`s); default: everything the session shows.
    #[serde(default)]
    pub art: Option<Vec<String>>,
    /// `single` sessions only: `music|mix|audiobook|podcast|other`.
    #[serde(default)]
    pub kind: Option<String>,
    /// `single` sessions only: cue sheet text (mix / audiobook / podcast).
    #[serde(default)]
    pub cue_text: Option<String>,
    /// `single` sessions only: title override.
    #[serde(default)]
    pub title: Option<String>,
}

/// One uploaded file waiting in staging.
#[derive(Debug, Clone)]
pub(crate) struct StagedImport {
    pub id: u32,
    pub path: PathBuf,
    /// Safe file name (client name reduced to a safe stem + the sniffed extension).
    pub name: String,
    /// Bytes of the staged file (the part of the batch budget it holds).
    pub size: u64,
    pub fields: ExtractedFields,
}

struct ImportSession {
    id: String,
    /// The client that opened it (the per-client session cap counts these).
    owner: IpAddr,
    lib_root: PathBuf,
    stage: PathBuf,
    files: Vec<StagedImport>,
    next_id: u32,
    budget: Arc<Budget>,
    cancel: Cancel,
    last_used: Instant,
    /// `finish` is running (a second one is refused).
    finishing: bool,
    /// One audio file, finishing into a loose track.
    single: bool,
    /// Staged cover art. Its own lock: decoding a big picture must not stall the other sessions.
    art: Arc<Mutex<ArtStaging>>,
}

impl Drop for ImportSession {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Waits for a drop being decoded; later ones are refused, so none can re-create the folder.
        if let Ok(mut a) = self.art.lock() {
            a.close();
        }
        remove_stage(&self.lib_root, &self.stage);
    }
}

/// Delete a staging folder, but only if it really is `<root>/.plippa/tmp/import-*`.
fn remove_stage(root: &Path, stage: &Path) {
    let Ok(fs) = LibraryFs::new(root) else { return };
    let Ok(tmp) = fs.tmp_dir() else { return };
    let name_ok = stage
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("import-"));
    if name_ok && stage.parent() == Some(tmp.as_path()) {
        let _ = std::fs::remove_dir_all(stage);
    }
}

/// All import sessions of this server.
pub struct ImportHub {
    sessions: Mutex<Vec<Arc<Mutex<ImportSession>>>>,
    gate: UploadGate,
    staging: Arc<StagingBudget>,
    limits: DownloadLimits,
    session_limits: SessionLimits,
    session_ttl: Duration,
}

impl Default for ImportHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ImportHub {
    pub fn new() -> Self {
        Self::with_limits(DownloadLimits::from_env())
    }

    pub fn with_limits(limits: DownloadLimits) -> Self {
        Self::with_limits_and_staging(limits, Arc::new(StagingBudget::production()))
    }

    /// A hub whose sessions draw from `staging`, shared with the unite hub and the single-file
    /// route so that everything staged on the stick counts against one cap.
    pub fn with_limits_and_staging(limits: DownloadLimits, staging: Arc<StagingBudget>) -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
            gate: UploadGate::default(),
            staging,
            limits,
            session_limits: SessionLimits::new(MAX_IMPORT_SESSIONS),
            session_ttl: SESSION_TTL,
        }
    }

    /// Replace the session lifetime and caps (tests).
    pub fn with_session_policy(mut self, ttl: Duration, limits: SessionLimits) -> Self {
        self.session_ttl = ttl;
        self.session_limits = limits;
        self
    }

    /// SEC-07: at most one upload body is being received at a time (per server); another one
    /// gets 429. Each one costs disk bandwidth and a staging file on a small board. The slot
    /// records its owner and its last progress: a stalled holder is replaced by an upload from
    /// the **same client** after 15 s without progress, never by another client's.
    ///
    /// Shared by every route that streams a body to the stick: `POST /api/media`, an import
    /// session's `files`, a unite session's `files` and the staged cover upload (one stick, one
    /// disk-bound stream at a time).
    pub fn try_upload_slot(&self, client: IpAddr) -> Option<UploadSlot> {
        self.gate.try_take(client)
    }

    /// The slot gate itself (tests drive it with an injected clock).
    pub fn upload_gate(&self) -> &UploadGate {
        &self.gate
    }

    /// True while an upload body holds the slot. Lets tests wait for "the first upload is really
    /// in flight" instead of probing with requests (a probe that lands first takes the slot
    /// itself and makes the *first* upload the refused one).
    pub fn upload_in_flight(&self) -> bool {
        self.gate.in_flight()
    }

    /// The staging cap shared with the unite hub.
    pub fn staging(&self) -> Arc<StagingBudget> {
        Arc::clone(&self.staging)
    }

    /// A batch budget for one upload outside any session (`POST /api/media`): it counts against
    /// the shared staging cap too.
    pub fn new_budget(&self) -> Budget {
        Budget::with_staging(self.limits.max_batch_bytes, Arc::clone(&self.staging))
    }

    pub fn limits(&self) -> &DownloadLimits {
        &self.limits
    }

    fn get(&self, id: &str) -> Result<Arc<Mutex<ImportSession>>, ImportError> {
        let mut all = self.sessions.lock().map_err(|_| unknown())?;
        all.retain(|s| {
            s.lock()
                .map(|g| g.last_used.elapsed() < self.session_ttl || g.finishing)
                .unwrap_or(false)
        });
        let found = all
            .iter()
            .find(|s| s.lock().map(|g| g.id == id).unwrap_or(false))
            .cloned();
        found.ok_or_else(unknown)
    }

    /// Open a session: creates the staging folder on the primary library. Leftovers of earlier
    /// runs (`import-*` folders no live session owns) are swept first.
    pub fn begin(
        &self,
        client: IpAddr,
        service: &Arc<Mutex<ServerService>>,
        single: bool,
    ) -> Result<Value, ImportError> {
        let (root, tmp) = {
            let svc = service
                .lock()
                .map_err(|_| ImportError::new(500, "failed", "server busy"))?;
            let tmp = svc.staging_dir()?;
            (svc.libraries[0].root.clone(), tmp)
        };
        let mut all = self
            .sessions
            .lock()
            .map_err(|_| ImportError::new(500, "failed", "server busy"))?;
        all.retain(|s| {
            s.lock()
                .map(|g| g.last_used.elapsed() < self.session_ttl || g.finishing)
                .unwrap_or(false)
        });
        // Never evict another client's live session: a full house of live sessions is refused.
        let rows: Vec<SessionRow> = all
            .iter()
            .map(|s| {
                s.lock()
                    .map(|g| SessionRow {
                        owner: g.owner,
                        idle: if g.finishing {
                            Duration::ZERO
                        } else {
                            g.last_used.elapsed()
                        },
                    })
                    .unwrap_or(SessionRow {
                        owner: client,
                        idle: Duration::ZERO,
                    })
            })
            .collect();
        match place_session(&rows, client, &self.session_limits) {
            Placement::Add => {}
            Placement::Replace(i) => {
                // Dropping the session deletes its staging and cancels its work.
                all.remove(i);
            }
            Placement::Refuse => {
                return Err(ImportError::new(
                    429,
                    "too_many",
                    "too many imports are open; finish or cancel one first",
                ));
            }
        }
        let live: Vec<PathBuf> = all
            .iter()
            .filter_map(|s| s.lock().ok().map(|g| g.stage.clone()))
            .collect();
        if let Ok(rd) = std::fs::read_dir(&tmp) {
            for e in rd.flatten() {
                let p = e.path();
                let is_import = e.file_name().to_string_lossy().starts_with("import-");
                if is_import && !live.contains(&p) {
                    remove_stage(&root, &p);
                }
            }
        }
        let id = format!("{:032x}", rand::random::<u128>());
        let stage = tmp.join(format!("import-{id}"));
        std::fs::create_dir(&stage)
            .map_err(|e| ImportError::new(500, "io", format!("cannot create staging: {e}")))?;
        let art = Arc::new(Mutex::new(ArtStaging::new(stage.join("art"))));
        let budget = Arc::new(self.new_budget());
        all.push(Arc::new(Mutex::new(ImportSession {
            id: id.clone(),
            owner: client,
            lib_root: root,
            stage,
            files: Vec::new(),
            next_id: 1,
            budget,
            cancel: Cancel::new(),
            last_used: Instant::now(),
            finishing: false,
            single,
            art,
        })));
        Ok(json!({
            "id": id,
            "single": single,
            "max_files": MAX_IMPORT_FILES,
            "max_file_bytes": self.limits.max_file_bytes,
        }))
    }

    /// Stage one uploaded file (streamed, sniffed). Refused files register nothing. An audio
    /// file also contributes the art embedded in it; an image file is staged as cover art.
    pub fn add_file(
        &self,
        id: &str,
        raw_name: &str,
        content_length: Option<u64>,
        body: &mut dyn Read,
    ) -> Result<Value, ImportError> {
        let sess = self.get(id)?;
        let (stage, budget, cancel, art, single) = {
            let mut g = sess.lock().map_err(|_| unknown())?;
            if g.finishing {
                return Err(ImportError::new(
                    409,
                    "busy",
                    "this import is being finished",
                ));
            }
            g.last_used = Instant::now();
            (
                g.stage.clone(),
                Arc::clone(&g.budget),
                g.cancel.clone(),
                Arc::clone(&g.art),
                g.single,
            )
        };
        // The session stays alive while its own upload is streaming, however long that takes:
        // the body stamps it as data arrives (a long upload must not outlive its session).
        let alive = Arc::clone(&sess);
        let mut body = ActivityReader::new(body, move || {
            if let Ok(mut g) = alive.try_lock() {
                g.last_used = Instant::now();
            }
        });
        let streamed = stream_upload(
            &mut body,
            content_length,
            &stage,
            &self.limits,
            &budget,
            &cancel,
            true,
        )
        .map_err(|e| ImportError::from(e).for_file(raw_name))?;
        let kind = match sniff_audio(&streamed.head) {
            MediaSniff::Audio(k) => k,
            _ => return self.add_dropped_image(id, raw_name, &art, &budget, streamed),
        };
        let file_id = {
            let mut g = sess.lock().map_err(|_| unknown())?;
            let refusal = if single && !g.files.is_empty() {
                Some(ImportError::new(
                    409,
                    "single_full",
                    "this dialog takes one file; start again to use another",
                ))
            } else if g.files.len() >= MAX_IMPORT_FILES {
                Some(ImportError::new(
                    413,
                    "too_many_files",
                    format!("at most {MAX_IMPORT_FILES} files per import"),
                ))
            } else {
                None
            };
            if let Some(e) = refusal {
                budget.release(streamed.reserved);
                return Err(e);
            }
            let fid = g.next_id;
            g.next_id += 1;
            fid
        };
        let name = crate::unite::hub::staged_name(raw_name, kind.ext());
        let size = streamed.size;
        let path = stage.join(format!("f{file_id}.bin"));
        let tmp_path = streamed.temp.keep();
        if let Err(e) = std::fs::rename(&tmp_path, &path) {
            let _ = std::fs::remove_file(&tmp_path);
            budget.release(streamed.reserved);
            return Err(ImportError::new(500, "io", e.to_string()));
        }
        let fields = extract_tags_from_path(&name, &path);
        // Art is never a reason to refuse the upload: whatever cannot be used is only counted.
        let scan = extract_embedded(&path);
        let mut art_skipped = scan.skipped;
        let (art_list, merged) = {
            let mut a = art.lock().map_err(|_| unknown())?;
            for pic in scan.pics {
                if a.add(ArtOrigin::Embedded { file_id }, pic.bytes, &self.limits).is_err() {
                    art_skipped += 1;
                }
            }
            a.to_json(id)
        };
        let out = json!({
            "file_id": file_id,
            "kind": "audio",
            "name": name,
            "size": size,
            "title": fields.title,
            "artist": fields.artist,
            "album": fields.album,
            "art": art_list,
            "art_skipped": art_skipped,
            "merged": merged,
        });
        let mut g = sess.lock().map_err(|_| unknown())?;
        g.files.push(StagedImport {
            id: file_id,
            path,
            name,
            size,
            fields,
        });
        Ok(out)
    }

    /// An image file dropped into the dialog: staged as cover art (an unusable image is a 4xx).
    fn add_dropped_image(
        &self,
        id: &str,
        raw_name: &str,
        art: &Arc<Mutex<ArtStaging>>,
        budget: &Budget,
        streamed: Streamed,
    ) -> Result<Value, ImportError> {
        let ext = sniff_image_kind(&streamed.head)
            .map(|k| k.ext())
            .ok_or_else(|| ImportError::new(415, "wrong_type", "not an audio file or image"))?;
        let size = streamed.size;
        let bytes = std::fs::read(streamed.temp.path())
            .map_err(|e| ImportError::new(500, "io", e.to_string()));
        budget.release(streamed.reserved);
        drop(streamed); // deletes the temp file; the picture lives on in the art folder
        let name = crate::unite::hub::staged_name(raw_name, ext);
        let mut a = art.lock().map_err(|_| unknown())?;
        a.add(ArtOrigin::Dropped { name: name.clone() }, bytes?, &self.limits)?;
        let (art_list, merged) = a.to_json(id);
        Ok(json!({
            "kind": "image",
            "name": name,
            "size": size,
            "art": art_list,
            "art_skipped": 0,
            "merged": merged,
        }))
    }

    fn art_of(&self, id: &str) -> Result<Arc<Mutex<ArtStaging>>, ImportError> {
        let sess = self.get(id)?;
        let mut g = sess.lock().map_err(|_| unknown())?;
        g.last_used = Instant::now();
        Ok(Arc::clone(&g.art))
    }

    /// The staged bytes of image `art_id` (or the survivor it was merged into) for the preview,
    /// with the content type decided from the bytes.
    pub fn art_bytes(&self, id: &str, art_id: &str) -> Result<(Vec<u8>, &'static str), ImportError> {
        let art = self.art_of(id)?;
        let found = art
            .lock()
            .map_err(|_| unknown())?
            .resolve(art_id)
            .ok_or_else(|| ImportError::new(404, "unknown_art", "unknown image"))?;
        let meta = std::fs::metadata(&found.path)
            .map_err(|_| ImportError::new(404, "unknown_art", "unknown image"))?;
        if meta.len() > MAX_COVER_BYTES {
            return Err(ImportError::new(413, "too_large", "image too large"));
        }
        let bytes = std::fs::read(&found.path)
            .map_err(|e| ImportError::new(500, "io", e.to_string()))?;
        let ctype = sniff_image_kind(&bytes)
            .map(|k| k.content_type())
            .ok_or_else(|| ImportError::new(500, "io", "staged image is not an image"))?;
        Ok((bytes, ctype))
    }

    /// Take one uploaded audio file back out of the session: its staged file is deleted, its
    /// budget share is released and the art only it carried disappears. Answers with the
    /// recomputed session-wide `art` / `merged`.
    pub fn remove_file(&self, id: &str, file_id: &str) -> Result<Value, ImportError> {
        let sess = self.get(id)?;
        let mut g = sess.lock().map_err(|_| unknown())?;
        if g.finishing {
            return Err(ImportError::new(409, "busy", "this import is being finished"));
        }
        let at = file_id
            .parse::<u32>()
            .ok()
            .and_then(|n| g.files.iter().position(|f| f.id == n))
            .ok_or_else(|| ImportError::new(404, "unknown_file", "unknown file"))?;
        g.last_used = Instant::now();
        let gone = g.files.remove(at);
        let _ = std::fs::remove_file(&gone.path);
        g.budget.release(gone.size);
        let mut a = g.art.lock().map_err(|_| unknown())?;
        a.forget_file(gone.id);
        let (list, merged) = a.to_json(id);
        Ok(json!({"art": list, "merged": merged}))
    }

    /// Remove a picture (and later copies of it) from the session's art.
    pub fn remove_art(&self, id: &str, art_id: &str) -> Result<Value, ImportError> {
        let art = self.art_of(id)?;
        let mut a = art.lock().map_err(|_| unknown())?;
        a.exclude(art_id)?;
        let (list, merged) = a.to_json(id);
        Ok(json!({"art": list, "merged": merged}))
    }

    /// Set the order the images will be saved in.
    pub fn order_art(&self, id: &str, order: Vec<String>) -> Result<Value, ImportError> {
        let art = self.art_of(id)?;
        let mut a = art.lock().map_err(|_| unknown())?;
        a.set_order(order)?;
        let (list, merged) = a.to_json(id);
        Ok(json!({"art": list, "merged": merged}))
    }

    /// Forget a session and delete its staging. `false` when it does not exist.
    pub fn discard(&self, id: &str) -> bool {
        let gone: Vec<Arc<Mutex<ImportSession>>> = {
            let Ok(mut all) = self.sessions.lock() else {
                return false;
            };
            let (gone, kept) = std::mem::take(&mut *all)
                .into_iter()
                .partition(|s| s.lock().map(|g| g.id == id).unwrap_or(false));
            *all = kept;
            gone
        };
        // Close the art now (an upload still holding the session would otherwise keep writing
        // into the staging until it returns); Drop deletes the staging.
        for s in &gone {
            let art = s.lock().ok().map(|g| Arc::clone(&g.art));
            if let Some(art) = art {
                if let Ok(mut a) = art.lock() {
                    a.close();
                }
            }
        }
        !gone.is_empty()
    }

    /// PWRSW_01: drop every import session (its `Drop` deletes the staging folder).
    /// Returns the number dropped.
    pub fn cancel_all(&self) -> usize {
        let Ok(mut all) = self.sessions.lock() else {
            return 0;
        };
        let n = all.len();
        all.clear();
        n
    }

    /// Create the collection (or the single track) from the chosen files and art
    /// (all-or-nothing) and close the session.
    pub fn finish(
        &self,
        service: &Arc<Mutex<ServerService>>,
        id: &str,
        req: ImportFinish,
    ) -> Result<Value, ImportError> {
        let sess = self.get(id)?;
        let (files, art, single) = {
            let mut g = sess.lock().map_err(|_| unknown())?;
            if g.finishing {
                return Err(ImportError::new(
                    409,
                    "busy",
                    "this import is being finished",
                ));
            }
            let order: Vec<u32> = if g.single && req.order.is_empty() {
                g.files.iter().map(|f| f.id).collect()
            } else {
                req.order.clone()
            };
            if order.is_empty() {
                return Err(ImportError::new(400, "no_media", "no playable media"));
            }
            let mut seen = std::collections::HashSet::new();
            let mut files = Vec::with_capacity(order.len());
            for fid in &order {
                if !seen.insert(*fid) {
                    return Err(ImportError::new(
                        400,
                        "invalid",
                        format!("file {fid} is listed twice"),
                    ));
                }
                let Some(f) = g.files.iter().find(|f| f.id == *fid) else {
                    return Err(ImportError::new(
                        400,
                        "invalid",
                        format!("file {fid} was not uploaded (or failed)"),
                    ));
                };
                files.push(f.clone());
            }
            if g.single {
                if req.kind.as_deref().and_then(ImportKind::parse).is_none() {
                    return Err(ImportError::new(
                        400,
                        "invalid",
                        "missing or invalid kind (required: music|mix|audiobook|podcast|other)",
                    ));
                }
            } else if req.mode.is_none() || req.name.is_none() {
                return Err(ImportError::new(400, "invalid", "mode and name are required"));
            }
            let art = g
                .art
                .lock()
                .map_err(|_| unknown())?
                .select_for_finish(req.art.as_deref(), &order)?;
            g.finishing = true;
            g.last_used = Instant::now();
            (files, art, g.single)
        };
        let result = {
            let mut svc = service
                .lock()
                .map_err(|_| ImportError::new(500, "failed", "server busy"))?;
            if single {
                match (req.kind.as_deref().and_then(ImportKind::parse), files.first()) {
                    (Some(kind), Some(file)) => svc.import_single(
                        kind,
                        file,
                        req.cue_text.as_deref(),
                        req.title.as_deref(),
                        &art,
                    ),
                    _ => Err(ServerError::Invalid("no playable media".into())),
                }
            } else {
                svc.import_collection(
                    req.mode.unwrap_or(ImportMode::Album),
                    req.name.as_deref().unwrap_or_default(),
                    &files,
                    &art,
                )
            }
        };
        match result {
            Ok(v) => {
                self.discard(id);
                Ok(v)
            }
            Err(e) => {
                if let Ok(mut g) = sess.lock() {
                    g.finishing = false;
                }
                Err(e.into())
            }
        }
    }
}

fn unknown() -> ImportError {
    ImportError::new(404, "unknown_import", "unknown or expired import")
}

// ---------------------------------------------------------------------------
// Creating the collection
// ---------------------------------------------------------------------------

impl ServerService {
    /// Import `files` (in this order) as an album or a playlist named `name`. Undoes
    /// everything on failure; the staged files are moved back into staging.
    pub(crate) fn import_collection(
        &mut self,
        mode: ImportMode,
        name: &str,
        files: &[StagedImport],
        art: &[StagedArt],
    ) -> Result<Value, ServerError> {
        if self.libraries.is_empty() {
            return Err(ServerError::Unavailable(
                "No media storage available".into(),
            ));
        }
        // Same naming rules as playlists: no control characters, bidi/zero-width stripped, capped.
        let title = validate_playlist_title(name)
            .map_err(|m| ServerError::Invalid(m.replace("playlist title", "collection name")))?;
        match mode {
            ImportMode::Album => {
                let s = self.import_album(&title, files, art)?;
                Ok(json!({
                    "mode": "album",
                    "id": s.id.to_string(),
                    "title": s.title,
                    "tracks": files.len(),
                    "images": art.len(),
                    "noudata_relpath": s.noudata_relpath,
                }))
            }
            ImportMode::Playlist => {
                let d = self.import_playlist(&title, files, art)?;
                let v = serde_json::to_value(&d)?;
                Ok(json!({
                    "mode": "playlist",
                    "id": v["id"],
                    "title": v["title"],
                    "tracks": files.len(),
                    "images": art.len(),
                    "noudata_relpath": d.noudata_relpath,
                }))
            }
        }
    }

    /// `Music/<artist>/<name>/` with the files in the chosen order and `_<name>.noudata`
    /// (`mediaList` in that order, `playableItemTitle` = name).
    fn import_album(
        &mut self,
        title: &str,
        files: &[StagedImport],
        art: &[StagedArt],
    ) -> Result<super::PlayableSummary, ServerError> {
        let root = self.libraries[0].root.clone();
        let label = self.libraries[0].label.clone();
        let multi = self.libraries.len() > 1;

        // Album artist = the most common artist tag (first wins a tie). None ⇒ the folder is
        // `Unknown Artist` but no credits are written (never a placeholder as data).
        let mut counts: Vec<(String, usize)> = Vec::new();
        for f in files {
            if let Some(a) = f.fields.artist.as_deref().filter(|a| !a.trim().is_empty()) {
                match counts.iter_mut().find(|(n, _)| n == a) {
                    Some(c) => c.1 += 1,
                    None => counts.push((a.to_string(), 1)),
                }
            }
        }
        let artist = counts
            .iter()
            .fold(None::<&(String, usize)>, |best, c| match best {
                Some(b) if b.1 >= c.1 => Some(b),
                _ => Some(c),
            })
            .map(|c| c.0.clone());
        let artist_seg = safe_path_segment(
            artist.as_deref().unwrap_or("Unknown Artist"),
            "Unknown Artist",
            MAX_SEGMENT_BYTES,
        );
        let release_seg = safe_path_segment(title, "Unknown Release", MAX_SEGMENT_BYTES);

        let lfs = LibraryFs::new(&root).map_err(|e| ServerError::Invalid(e.to_string()))?;
        // A fresh folder: never share a folder (and two `.noudata`) with an existing album.
        let mut rel_dir = PathBuf::new();
        let mut seg = release_seg.clone();
        for i in 1..1000 {
            seg = if i == 1 {
                release_seg.clone()
            } else {
                format!("{release_seg} ({i})")
            };
            rel_dir = PathBuf::from("Music").join(&artist_seg).join(&seg);
            let abs = lfs
                .resolve(&rel_dir)
                .map_err(|e| ServerError::Invalid(e.to_string()))?;
            if std::fs::symlink_metadata(&abs).is_err() {
                break;
            }
        }

        let mut tx = Transaction::default();
        match self.write_album(
            &mut tx,
            &lfs,
            &root,
            &rel_dir,
            &seg,
            title,
            artist.as_deref(),
            files,
        ) {
            Ok(noudata_rel) => {
                // The images go in last, before the transaction is committed: if they cannot be
                // placed (or their thumbnails made) the album is taken back out as a whole.
                let _ = self.lazy_rescan();
                let placed = list_by_rel(self, &root, &noudata_rel, &label, multi)
                    .and_then(|s| self.commit_import_art(s.id, art));
                match placed {
                    Ok(()) => {
                        tx.commit();
                        list_by_rel(self, &root, &noudata_rel, &label, multi)
                    }
                    Err(e) => {
                        tx.rollback();
                        let _ = self.lazy_rescan();
                        Err(e)
                    }
                }
            }
            Err(e) => {
                tx.rollback();
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write_album(
        &mut self,
        tx: &mut Transaction,
        lfs: &LibraryFs,
        root: &Path,
        rel_dir: &Path,
        seg: &str,
        title: &str,
        artist: Option<&str>,
        files: &[StagedImport],
    ) -> Result<String, ServerError> {
        lfs.ensure_dir(rel_dir, tx.dirs())
            .map_err(|e| ServerError::Invalid(e.to_string()))?;
        // `Music/<artist>/<release>` is built from tag text: on a case-insensitive volume an
        // existing artist folder may be spelled differently (`OutKast` vs `Outkast`). Use the
        // spelling on disk so the returned `.noudata` path is the one the index records.
        let rel_dir = &actual_rel_path(root, rel_dir);
        let dir = root.join(rel_dir);
        let mut media_list = Vec::with_capacity(files.len());
        for f in files {
            let target = unique_media_path(&dir, &f.name);
            tx.move_in(&f.path, &target)
                .map_err(|e| ServerError::Message(e.to_string()))?;
            let file_name = target
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&f.name)
                .to_string();
            let stem = Path::new(&f.name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Track")
                .to_string();
            media_list.push(MediaListItem {
                local_file_paths: Some(vec![file_name]),
                media_title: Some(f.fields.title.clone().unwrap_or(stem)),
                ..Default::default()
            });
        }
        let doc = NoudataDocument {
            what_am_i_looking_at: Some(WHAT_AM_I_LOOKING_AT.into()),
            noudata_version: Some(NOUDATA_VERSION.into()),
            noudata_type: Some(NoudataType::Release),
            playable_item_title: Some(title.to_string()),
            playable_item_folder_safe_title: Some(seg.to_string()),
            playable_item_profile: Some(PlayableItemProfile::Music),
            release_data: Some(ReleaseData {
                credits: artist.map(|artist| Credits {
                    primary_artists: Some(vec![PrimaryArtist {
                        primary_artist_name: artist.to_string(),
                        extra: Default::default(),
                    }]),
                    primary_artist_folder_safe_name: Some(safe_path_segment(
                        artist,
                        "Unknown Artist",
                        MAX_SEGMENT_BYTES,
                    )),
                    contributors: Some(vec![Contributor {
                        contributor_role: Some("performer".into()),
                        contributor_name: artist.to_string(),
                        contributor_url: None,
                        extra: Default::default(),
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            media_list,
            ..Default::default()
        };
        let noudata = dir.join(safe_path_segment(
            &format!("_{seg}.noudata"),
            "_album.noudata",
            MAX_SEGMENT_BYTES,
        ));
        tx.write_new(&noudata, doc.to_json_pretty_checked()?.as_bytes())
            .map_err(|e| ServerError::Message(e.to_string()))?;
        super::path_relative_posix(root, &noudata)
    }

    /// Every file becomes an ordinary track under the §4.4 tree, then `Playlists/_<name>.noudata`
    /// lists them in the chosen order. If the playlist cannot be made the tracks are removed again.
    fn import_playlist(
        &mut self,
        title: &str,
        files: &[StagedImport],
        art: &[StagedArt],
    ) -> Result<CollectionDetail, ServerError> {
        let root = self.libraries[0].root.clone();
        let label = self.libraries[0].label.clone();
        let multi = self.libraries.len() > 1;
        let mut tx = Transaction::default();
        let mut rels = Vec::with_capacity(files.len());
        for f in files {
            match self.import_file(&mut tx, ImportKind::Music, &f.name, &f.path, None) {
                Ok(i) => rels.push(i.noudata_rel),
                Err(e) => {
                    tx.rollback();
                    return Err(e);
                }
            }
        }
        let _ = self.lazy_rescan();
        let made = (|| {
            let mut ids = HashMap::new();
            let mut tracks = Vec::with_capacity(rels.len());
            for rel in &rels {
                let id = match ids.get(rel) {
                    Some(id) => *id,
                    None => {
                        let s = list_by_rel(self, &root, rel, &label, multi)?;
                        ids.insert(rel.clone(), s.id);
                        s.id
                    }
                };
                tracks.push(TrackRef::Id(IdArg(id)));
            }
            let d = self.create_playlist(CreatePlaylist {
                title: title.to_string(),
                tracks,
                created_by: None,
            })?;
            // The art belongs to the playlist, not to each track.
            if let Err(e) = self.commit_import_art(d.summary.id, art) {
                let _ = self.delete_playlist(d.summary.id, None);
                return Err(e);
            }
            Ok(d)
        })();
        match made {
            Ok(d) => {
                tx.commit();
                Ok(d)
            }
            Err(e) => {
                tx.rollback();
                let _ = self.lazy_rescan();
                Err(e)
            }
        }
    }

    /// A `single` session: one file becomes an ordinary track (exactly like `POST /api/media`),
    /// then the chosen images go to its cover art. All-or-nothing.
    pub(crate) fn import_single(
        &mut self,
        kind: ImportKind,
        file: &StagedImport,
        cue_text: Option<&str>,
        title: Option<&str>,
        art: &[StagedArt],
    ) -> Result<Value, ServerError> {
        if self.libraries.is_empty() {
            return Err(ServerError::Unavailable(
                "No media storage available".into(),
            ));
        }
        let root = self.libraries[0].root.clone();
        let label = self.libraries[0].label.clone();
        let multi = self.libraries.len() > 1;
        let mut tx = Transaction::default();
        let imported =
            match self.import_file_titled(&mut tx, kind, &file.name, &file.path, cue_text, title) {
                Ok(i) => i,
                Err(e) => {
                    tx.rollback();
                    return Err(e);
                }
            };
        let _ = self.lazy_rescan();
        let placed = list_by_rel(self, &root, &imported.noudata_rel, &label, multi)
            .and_then(|s| self.commit_import_art(s.id, art));
        match placed {
            Ok(()) => {
                tx.commit();
                let summary = list_by_rel(self, &root, &imported.noudata_rel, &label, multi)?;
                let mut v = serde_json::to_value(super::AddMediaOutcome {
                    summary,
                    cues: imported.cues,
                })?;
                v["images"] = json!(art.len());
                Ok(v)
            }
            Err(e) => {
                tx.rollback();
                let _ = self.lazy_rescan();
                Err(e)
            }
        }
    }

    /// Give the playable `id` the staged images as its cover art (the same commit the edit
    /// popup uses: files, thumbnails, `.noudata`, index). Nothing to do for an empty set. On
    /// failure the library is as it was and the staged files are still where they were.
    fn commit_import_art(&mut self, id: u64, art: &[StagedArt]) -> Result<(), ServerError> {
        if art.is_empty() {
            return Ok(());
        }
        let (idx, _) = self.require_record(id)?;
        let root = self.libraries[idx].root.clone();
        let store = self.cover_store();
        let ids: Vec<String> = art
            .iter()
            .map(|a| store.register_file(&root, &a.path, a.kind))
            .collect();
        let order = ids
            .iter()
            .map(|s| OrderEntry::Staged { staged: s.clone() })
            .collect();
        let r = self.put_covers(id, PutCovers { order });
        if r.is_err() {
            store.unregister(&ids); // a successful commit consumed them itself
        }
        r.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RecordingIpc;
    use std::fs;

    fn library_files(root: &Path) -> Vec<String> {
        fn walk(d: &Path, root: &Path, out: &mut Vec<String>) {
            for e in fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                if p.file_name().is_some_and(|n| n == ".plippa") {
                    continue;
                }
                if p.is_dir() {
                    walk(&p, root, out);
                } else {
                    out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut v = Vec::new();
        walk(root, root, &mut v);
        v.sort();
        v
    }

    /// An album whose image cannot be turned into thumbnails fails *after* the image was moved
    /// next to the media: the album, the image, the thumbnails and the moved media all go away
    /// and the staged files are where they were.
    #[test]
    fn album_finish_that_fails_after_the_images_were_placed_takes_everything_back() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().to_path_buf();
        let mut svc = ServerService::start_with_roots(
            vec![root.clone()],
            Box::new(RecordingIpc::new()),
            ServerService::default_mock_associate(),
        );
        let stage = svc.staging_dir().unwrap().join("import-test");
        fs::create_dir_all(&stage).unwrap();
        let media = stage.join("f1.bin");
        fs::write(&media, b"RIFF").unwrap();
        // a PNG signature and nothing else: placed fine, but no thumbnail can be made from it
        let img = stage.join("a1.png");
        fs::write(&img, b"\x89PNG\r\n\x1a\nnot really").unwrap();
        let file = StagedImport {
            id: 1,
            path: media.clone(),
            name: "t.wav".into(),
            size: 4,
            fields: ExtractedFields::default(),
        };
        let art = StagedArt {
            art_id: "a".into(),
            origin: ArtOrigin::Dropped { name: "a.png".into() },
            path: img.clone(),
            w: 1,
            h: 1,
            bytes: 18,
            kind: plippa_image::ImageKind::Png,
            dhash: 0,
            excluded: false,
            digest: 0,
            seq: 1,
        };
        let r = svc.import_collection(ImportMode::Album, "Doomed", &[file], &[art]);
        assert!(r.is_err(), "{r:?}");
        assert_eq!(library_files(&root), Vec::<String>::new());
        assert!(media.is_file() && img.is_file(), "staged files are back");
        assert!(!root.join("Music/Unknown Artist/Doomed").exists());
    }
}

#[cfg(test)]
mod ttl_tests {
    use super::*;
    use crate::ipc::RecordingIpc;
    use std::io::Cursor;

    /// A body that arrives in small pieces with a pause before each one.
    struct Trickle {
        data: Cursor<Vec<u8>>,
        chunk: usize,
        pause: Duration,
    }

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(self.pause);
            let n = buf.len().min(self.chunk);
            self.data.read(&mut buf[..n])
        }
    }

    fn wav(data_len: usize) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&8000u32.to_le_bytes());
        v.extend_from_slice(&8000u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&8u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(data_len as u32).to_le_bytes());
        v.extend(std::iter::repeat_n(7u8, data_len));
        v
    }

    #[test]
    fn an_import_upload_longer_than_the_session_ttl_still_finishes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("lib");
        std::fs::create_dir(&root).unwrap();
        let mut svc = ServerService::start_with_roots(
            vec![root],
            Box::new(RecordingIpc::new()),
            ServerService::default_mock_associate(),
        );
        let limits = DownloadLimits {
            free_space: |_| Some(1 << 40),
            ..DownloadLimits::default()
        };
        let ttl = Duration::from_millis(600);
        let hub = Arc::new(
            ImportHub::with_limits(limits).with_session_policy(ttl, SessionLimits::new(4)),
        );
        svc.imports = Arc::clone(&hub);
        let svc = Arc::new(Mutex::new(svc));
        let client: IpAddr = "127.0.0.1".parse().unwrap();
        let id = hub.begin(client, &svc, false).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let body = wav(3000);
        let started = Instant::now();
        let (h, sid, len) = (Arc::clone(&hub), id.clone(), body.len());
        let up = std::thread::spawn(move || {
            h.add_file(
                &sid,
                "slow.wav",
                Some(len as u64),
                &mut Trickle {
                    data: Cursor::new(body),
                    chunk: 200,
                    pause: Duration::from_millis(100),
                },
            )
        });
        // Other traffic keeps pruning expired sessions while the upload streams.
        while !up.is_finished() {
            let _ = hub.get("00000000000000000000000000000000");
            std::thread::sleep(Duration::from_millis(30));
        }
        let r = up.join().unwrap();
        assert!(r.is_ok(), "{r:?}");
        assert!(started.elapsed() > ttl * 2, "the upload must outlive the TTL");
        assert!(hub.get(&id).is_ok(), "the session survived its own upload");
    }
}
