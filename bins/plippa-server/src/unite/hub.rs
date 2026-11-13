//! Unite sessions (NB_09): Case A (free release: consent -> download -> unite), Case B (files
//! from elsewhere + the Noubin URL), Case C (a `_<release>.noudata` came with the files: only
//! associate) and everything they share.
//!
//! A session lives in memory for [`SESSION_TTL`]; its files are staged in
//! `<library>/.plippa/tmp/unite-<session>/` (same volume as the final folder, so the last move
//! is an atomic rename; staging is deleted with the session and swept on the next start).
//! Nothing reaches the library proper until [`UniteHub::apply`].
//!
//! Security (see `NBSecurityRequirements.md`): downloads go through
//! [`crate::web::download::download_to_temp`] (https only, SSRF block-list at connect time and on
//! every redirect hop, caps while streaming, free-space check, magic-byte sniffing, partial
//! deletion); uploads use the same streaming/sniffing core; every path written is checked by
//! [`LibraryFs`]; the document is sanitised as a whole before it is written; the network is only
//! entered after a server-side [`Consent`] that was bound to the exact URL list shown.

use super::fsops::{FsError, LibraryFs, Transaction};
use super::images::{plan_cover_targets, url_file_name, CoverTarget};
use super::sniff::{sniff_audio, sniff_image_kind, AudioKind, ImageInfo, MediaSniff};
use crate::admission::{
    place_session, ActivityReader, Placement as SlotPlacement, SessionLimits, SessionRow,
};
use crate::service::ServerService;
use crate::web::download::{
    download_to_temp, require_playable, stream_to_temp, validate_image_file, Budget, Cancel, DownloadError,
    DownloadLimits, StagingBudget, StreamRules, Want, MAX_IMAGE_SIDE,
};
use crate::web::fetch::{Consent, FetchPolicy, Transport, WebError};
use crate::web::overview::ReleaseOverview;
use crate::web::{fetch_unlocked, HeldRelease};
use plippa_noubin::unite::{
    associate_existing, build_local_doc, propose, CandidateFile, ConfirmedItem, ConfirmedMapping,
    EmbeddedTags, Placement, UniteError as MatchError, UniteProposal,
};
use plippa_noubin::{
    check_json_object_limits, first_significant_byte, is_safe_relative_path, normalise_noubin_key,
    parse_fetch_url, parse_noudata_with_warnings, redact_url_for_log, safe_path_segment,
    sanitise_document, sanitise_key, sanitise_line, sanitise_url, CoverImage, ImageEntry,
    NoudataDocument, SanitiseOptions, MAX_SEGMENT_BYTES,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A downloaded cover of a refresh session.
pub(super) struct RefreshCover {
    pub url: String,
    pub stem: String,
    pub ext: &'static str,
    pub path: PathBuf,
    pub thumbs: Vec<(u32, &'static str, PathBuf)>,
}

/// A session is dropped (and its staging deleted) this long after it was last used.
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 60);
/// Most sessions alive at once. A full house of live sessions refuses a new one; only an idle
/// session (no progress for ten minutes) is ever replaced, and a client replaces only its own
/// when it is at its own cap.
pub const MAX_SESSIONS: usize = 4;
/// Most files one session may stage.
pub const MAX_STAGED_FILES: usize = 500;
/// Most URLs one consent may cover.
pub const MAX_DOWNLOAD_TARGETS: usize = 300;
/// A `.noudata` upload is metadata: same cap as `web.noudata`.
pub const MAX_NOUDATA_UPLOAD_BYTES: u64 = 1024 * 1024;
/// Root property recording where a release's metadata came from (used by refresh).
pub const SOURCE_URL_KEY: &str = "plippaSourceURL";

/// Everything a unite call can fail with; each has a stable `code` and HTTP status.
#[derive(Debug)]
pub enum UniteError {
    UnknownSession,
    /// The addressed playable does not exist (HTTP 404).
    NotFound(String),
    /// Refresh needs a source URL and the item has none recorded.
    NeedsUrl,
    Web(WebError),
    Busy,
    /// Every unite session slot belongs to a live session (HTTP 429, `too_many`).
    TooMany(String),
    /// An upload stalled or was too slow (HTTP 408, `timeout`), like imports and media.
    Timeout(String),
    /// The transfer was cancelled or superseded (HTTP 409, `cancelled`), like imports.
    Cancelled(String),
    Invalid(String),
    /// Sniffed content is not something Plippa keeps (HTTP 415).
    Unsupported(String),
    TooLarge(String),
    NoSpace(String),
    /// The step is not possible yet (nothing confirmed, no release loaded...).
    NotReady(String),
    /// Some items have no file / some files are unaccounted for.
    Incomplete {
        missing_items: Vec<usize>,
        leftover_files: Vec<u32>,
    },
    KeyInUse,
    Storage(String),
    Io(String),
}

impl UniteError {
    pub fn code(&self) -> &'static str {
        match self {
            UniteError::UnknownSession => "unknown_session",
            UniteError::NotFound(_) => "not_found",
            UniteError::NeedsUrl => "needs_url",
            UniteError::Web(e) => e.code(),
            UniteError::Busy => "busy",
            UniteError::TooMany(_) => "too_many",
            UniteError::Timeout(_) => "timeout",
            UniteError::Cancelled(_) => "cancelled",
            UniteError::Invalid(_) => "invalid",
            UniteError::Unsupported(_) => "unsupported_file",
            UniteError::TooLarge(_) => "too_large",
            UniteError::NoSpace(_) => "no_space",
            UniteError::NotReady(_) => "not_ready",
            UniteError::Incomplete { .. } => "incomplete",
            UniteError::KeyInUse => "key_in_use",
            UniteError::Storage(_) => "no_storage",
            UniteError::Io(_) => "io",
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            UniteError::UnknownSession | UniteError::NotFound(_) => 404,
            UniteError::Web(e) => e.http_status(),
            UniteError::Busy | UniteError::TooMany(_) => 429,
            UniteError::Invalid(_) => 400,
            UniteError::Unsupported(_) => 415,
            UniteError::TooLarge(_) => 413,
            UniteError::NoSpace(_) => 507,
            UniteError::Timeout(_) => 408,
            UniteError::NotReady(_)
            | UniteError::Cancelled(_)
            | UniteError::NeedsUrl
            | UniteError::Incomplete { .. }
            | UniteError::KeyInUse => 409,
            UniteError::Storage(_) => 503,
            UniteError::Io(_) => 500,
        }
    }

    /// JSON body: `{"error", "code", ...extra}`.
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "error": self.to_string(), "code": self.code() });
        if let UniteError::Incomplete {
            missing_items,
            leftover_files,
        } = self
        {
            v["missing_items"] = json!(missing_items);
            v["leftover_files"] = json!(leftover_files);
        }
        v
    }
}

impl std::fmt::Display for UniteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UniteError::UnknownSession => f.write_str("unknown or expired unite session"),
            UniteError::Web(e) => write!(f, "{e}"),
            UniteError::Busy => f.write_str("a download is already running"),
            UniteError::TooMany(m)
            | UniteError::Timeout(m)
            | UniteError::Cancelled(m)
            | UniteError::Invalid(m)
            | UniteError::Unsupported(m)
            | UniteError::TooLarge(m)
            | UniteError::NoSpace(m)
            | UniteError::NotReady(m)
            | UniteError::Storage(m)
            | UniteError::NotFound(m)
            | UniteError::Io(m) => f.write_str(m),
            UniteError::NeedsUrl => f.write_str(
                "this item has no Noubin URL recorded; enter the release's Noubin URL to refresh it",
            ),
            UniteError::Incomplete {
                missing_items,
                leftover_files,
            } => write!(
                f,
                "{} item(s) have no file and {} file(s) are not assigned",
                missing_items.len(),
                leftover_files.len()
            ),
            UniteError::KeyInUse => f.write_str(
                "another item in the library already carries this Noubin key",
            ),
        }
    }
}

impl std::error::Error for UniteError {}

impl From<WebError> for UniteError {
    fn from(e: WebError) -> Self {
        UniteError::Web(e)
    }
}

impl From<FsError> for UniteError {
    fn from(e: FsError) -> Self {
        match e {
            FsError::Unsafe(m) => UniteError::Invalid(m),
            FsError::Exists(p) => UniteError::Io(format!("{} already exists", p.display())),
            FsError::Io(e) => UniteError::Io(e.to_string()),
        }
    }
}

impl From<DownloadError> for UniteError {
    fn from(e: DownloadError) -> Self {
        match e {
            DownloadError::TooLarge { .. } | DownloadError::BatchTooLarge { .. } => {
                UniteError::TooLarge(e.to_string())
            }
            DownloadError::NoSpace { .. } => UniteError::NoSpace(e.to_string()),
            DownloadError::WrongType(_) | DownloadError::BadImage(_) => {
                UniteError::Unsupported(e.to_string())
            }
            DownloadError::Refused(w) => UniteError::Web(w),
            DownloadError::Timeout => UniteError::Timeout(e.to_string()),
            DownloadError::Cancelled => UniteError::Cancelled(e.to_string()),
            other => UniteError::Io(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// `POST /api/unite/start`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StartRequest {
    /// Token of a release already fetched with `/api/web/fetch` (its consent stands).
    pub token: Option<String>,
    /// The tapped or pasted Noubin URL / key.
    pub noubin_key: Option<String>,
    /// Alias of `noubinKey` (pasted URL).
    pub url: Option<String>,
    /// Consent for the metadata fetch when `token` is not given (same rule as web/fetch).
    pub consent: Option<bool>,
    /// Case C without metadata: no network at all, files must include a `_<release>.noudata`.
    pub associate_only: bool,
}

/// `POST /api/unite/:session/download/prepare`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PrepareRequest {
    pub media: bool,
    pub images: bool,
}

impl Default for PrepareRequest {
    fn default() -> Self {
        Self {
            media: true,
            images: true,
        }
    }
}

/// `POST /api/unite/:session/download`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DownloadRequest {
    pub consent: Option<bool>,
    pub consent_id: Option<String>,
}

/// One manual assignment: `item` (0-based `mediaList` index) -> files (ids from the status).
#[derive(Debug, Deserialize)]
pub struct AssignmentIn {
    pub item: usize,
    #[serde(default)]
    pub files: Vec<u32>,
}

/// `POST /api/unite/:session/confirm`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfirmRequest {
    /// `"all"` or `"confident"`: start from the last proposal. Omit to send everything by hand.
    pub accept: Option<String>,
    /// Manual assignments; they replace the proposal's for those items and files.
    pub assignments: Vec<AssignmentIn>,
    /// Files the user chooses to leave out.
    pub ignored: Vec<u32>,
    /// Finish even with items without a file / unassigned files.
    pub allow_incomplete: bool,
}

/// `POST /api/unite/:session/apply`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApplyRequest {
    /// Noubin key to bind (defaults to the session's key).
    pub key: Option<String>,
    /// Bind the key even if another item already has it.
    pub allow_duplicate_key: bool,
}

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum FileKind {
    Audio(AudioKind),
    Image(ImageInfo),
    Noudata,
}

#[derive(Debug, Clone)]
enum Source {
    Upload,
    Download { url: String, items: Vec<usize> },
}

#[derive(Debug, Clone)]
struct StagedFile {
    id: u32,
    name: String,
    path: PathBuf,
    size: u64,
    kind: FileKind,
    source: Source,
    /// Uploaded into a `thumbnails/` folder (Case C).
    thumb: bool,
}

#[derive(Debug, Clone)]
struct StagedCover {
    entry: usize,
    stem: String,
    path: PathBuf,
    info: ImageInfo,
    /// `(tier, ext, staged path)`.
    thumbs: Vec<(u32, &'static str, PathBuf)>,
}

#[derive(Debug, Clone)]
enum TargetKind {
    Media {
        items: Vec<usize>,
        name_hint: String,
    },
    Cover {
        entry: usize,
    },
    Thumb {
        entry: usize,
        tier: u32,
        ext: &'static str,
        fallback: String,
    },
}

#[derive(Debug, Clone)]
struct Target {
    url: String,
    kind: TargetKind,
}

impl Target {
    fn label(&self) -> &'static str {
        match self.kind {
            TargetKind::Media { .. } => "media",
            TargetKind::Cover { .. } => "image",
            TargetKind::Thumb { .. } => "thumbnail",
        }
    }
}

struct Prepared {
    id: String,
    targets: Vec<Target>,
    covers: Vec<CoverTarget>,
}

#[derive(Debug, Clone)]
struct DlItem {
    url: String,
    kind: &'static str,
    state: &'static str,
    received: u64,
    total: Option<u64>,
    error: Option<String>,
}

#[derive(Debug)]
struct DlStatus {
    state: &'static str,
    items: Vec<DlItem>,
    error: Option<String>,
    warnings: Vec<String>,
}

impl DlStatus {
    fn idle() -> Self {
        Self {
            state: "idle",
            items: Vec::new(),
            error: None,
            warnings: Vec::new(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "state": self.state,
            "error": self.error,
            "warnings": self.warnings,
            "items": self.items.iter().map(|i| json!({
                "url": i.url, "kind": i.kind, "state": i.state,
                "received": i.received, "total": i.total, "error": i.error,
            })).collect::<Vec<_>>(),
        })
    }
}

struct Confirmed {
    mapping: ConfirmedMapping,
}

struct Session {
    id: String,
    /// The client that opened it (the per-client session cap counts these).
    owner: IpAddr,
    lib_root: PathBuf,
    stage: PathBuf,
    touched: Instant,
    release: Option<HeldRelease>,
    key: Option<String>,
    next_id: u32,
    files: Vec<StagedFile>,
    rejected: Vec<(String, String)>,
    covers: Vec<StagedCover>,
    prepared: Option<Prepared>,
    dl: Arc<Mutex<DlStatus>>,
    last_proposal: Option<(UniteProposal, HashMap<PathBuf, u32>)>,
    confirmed: Option<Confirmed>,
    applied: Option<Value>,
    /// Set for a session that only downloads images for this existing playable (refresh).
    refresh_of: Option<u64>,
    budget: Arc<Budget>,
    cancel: Cancel,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.cancel();
        remove_stage(&self.lib_root, &self.stage);
    }
}

/// Delete a staging directory, but only if it really is `<root>/.plippa/tmp/unite-*`.
fn remove_stage(root: &Path, stage: &Path) {
    let Ok(fs) = LibraryFs::new(root) else { return };
    let Ok(tmp) = fs.tmp_dir() else { return };
    let name_ok = stage
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("unite-"));
    if name_ok && stage.parent() == Some(tmp.as_path()) {
        // `remove_dir_all` does not follow symlinks inside the tree.
        let _ = std::fs::remove_dir_all(stage);
    }
}

/// Remember why an upload was refused (bounded: a client cannot grow this without limit).
fn push_rejected(s: &mut Session, raw_name: &str, msg: String) {
    if s.rejected.len() < 100 {
        s.rejected.push((sanitise_line(raw_name, 100), msg));
    }
}

// ---------------------------------------------------------------------------
// Hub
// ---------------------------------------------------------------------------

/// All unite sessions of this server.
pub struct UniteHub {
    sessions: Mutex<Vec<(String, Arc<Mutex<Session>>)>>,
    download_busy: Arc<AtomicBool>,
    limits: DownloadLimits,
    /// Shared with the import hub: everything staged on the stick counts against one cap.
    staging: Arc<StagingBudget>,
    session_limits: SessionLimits,
    session_ttl: Duration,
    /// Running download workers (joined by tests; finished ones are pruned).
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl Default for UniteHub {
    fn default() -> Self {
        Self::new()
    }
}

/// Clears the busy flag however the worker ends.
struct BusyGuard(Arc<AtomicBool>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn new_id() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const KNOWN_EXTS: &[&str] = &[
    "mp3", "flac", "wav", "ogg", "opus", "m4a", "m4b", "aac", "mp4", "m4v", "mkv", "avi", "mov",
    "wmv", "webm", "mpg", "mpeg", "flv", "wma", "aif", "aiff", "ape", "wv", "dsf", "dff", "mka",
    "bin", "dat", "tmp", "download", "png", "jpg", "jpeg", "webp", "gif", "noudata",
];

/// Stored name for untrusted `raw`: last path component only, made safe (§1.28, no leading dot,
/// <= 150 bytes), with the extension of the **sniffed** type (`track.bin` -> `track.flac`).
pub fn staged_name(raw: &str, ext: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    // `.m4b` (audiobook) and `.m4a` are the same MP4 audio container (the sniffer accepts the
    // `M4B ` brand as audio); keep the audiobook extension the user chose.
    let ext = if ext == "m4a" && base.to_ascii_lowercase().ends_with(".m4b") {
        "m4b"
    } else {
        ext
    };
    let safe = safe_path_segment(base, "file", 150);
    let stem = match safe.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && KNOWN_EXTS.contains(&e.to_ascii_lowercase().as_str()) => {
            s.to_string()
        }
        _ => safe,
    };
    safe_path_segment(&format!("{stem}.{ext}"), "file", MAX_SEGMENT_BYTES)
}

impl UniteHub {
    pub fn new() -> Self {
        Self::with_limits(DownloadLimits::from_env())
    }

    pub fn with_limits(limits: DownloadLimits) -> Self {
        Self::with_limits_and_staging(limits, Arc::new(StagingBudget::production()))
    }

    /// A hub whose sessions draw from `staging`, shared with the import hub.
    pub fn with_limits_and_staging(limits: DownloadLimits, staging: Arc<StagingBudget>) -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
            download_busy: Arc::new(AtomicBool::new(false)),
            limits,
            staging,
            session_limits: SessionLimits::new(MAX_SESSIONS),
            session_ttl: SESSION_TTL,
            workers: Mutex::new(Vec::new()),
        }
    }

    /// Replace the session lifetime and caps (tests).
    pub fn with_session_policy(mut self, ttl: Duration, limits: SessionLimits) -> Self {
        self.session_ttl = ttl;
        self.session_limits = limits;
        self
    }

    /// Wait for every download worker to finish (tests: deterministic, no polling).
    #[cfg(test)]
    pub(crate) fn join_workers(&self) {
        let hs: Vec<_> = self
            .workers
            .lock()
            .map(|mut w| w.drain(..).collect())
            .unwrap_or_default();
        for h in hs {
            let _ = h.join();
        }
    }

    pub fn limits(&self) -> &DownloadLimits {
        &self.limits
    }

    fn get(&self, id: &str) -> Result<Arc<Mutex<Session>>, UniteError> {
        let mut s = self
            .sessions
            .lock()
            .map_err(|_| UniteError::UnknownSession)?;
        s.retain(|(_, sess)| {
            sess.lock()
                .map(|g| g.touched.elapsed() < self.session_ttl)
                .unwrap_or(false)
        });
        let sess = s
            .iter()
            .find(|(i, _)| i == id)
            .map(|(_, sess)| Arc::clone(sess))
            .ok_or(UniteError::UnknownSession)?;
        drop(s);
        if let Ok(mut g) = sess.lock() {
            g.touched = Instant::now();
        }
        Ok(sess)
    }

    fn with<R>(
        &self,
        id: &str,
        f: impl FnOnce(&mut Session) -> Result<R, UniteError>,
    ) -> Result<R, UniteError> {
        let sess = self.get(id)?;
        let mut g = sess.lock().map_err(|_| UniteError::UnknownSession)?;
        f(&mut g)
    }

    /// Staged covers of a refresh session (for `refresh_install_images`).
    pub(super) fn take_refresh_images(
        &self,
        session: &str,
        playable: u64,
    ) -> Result<Vec<RefreshCover>, UniteError> {
        self.with(session, |s| {
            if s.refresh_of != Some(playable) {
                return Err(UniteError::Invalid(
                    "this session does not belong to that item".into(),
                ));
            }
            if s.dl.lock().map(|d| d.state == "running").unwrap_or(false) {
                return Err(UniteError::NotReady(
                    "the image download is still running".into(),
                ));
            }
            let images = s
                .release
                .as_ref()
                .and_then(|r| r.doc.release_data.as_ref())
                .and_then(|r| r.cover_image.as_ref())
                .map(|c| c.images.clone())
                .unwrap_or_default();
            let out: Vec<RefreshCover> = s
                .covers
                .iter()
                .filter_map(|c| {
                    let url = images.get(c.entry)?.image_web_url.clone()?;
                    Some(RefreshCover {
                        url,
                        stem: c.stem.clone(),
                        ext: c.info.kind.ext(),
                        path: c.path.clone(),
                        thumbs: c.thumbs.clone(),
                    })
                })
                .collect();
            if out.is_empty() {
                return Err(UniteError::NotReady(
                    "no image has been downloaded yet".into(),
                ));
            }
            Ok(out)
        })
    }

    pub(super) fn stage_of(&self, session: &str) -> Result<PathBuf, UniteError> {
        self.with(session, |s| Ok(s.stage.clone()))
    }

    /// Forget a session and delete its staging.
    pub fn discard(&self, id: &str) -> bool {
        let Ok(mut s) = self.sessions.lock() else {
            return false;
        };
        let before = s.len();
        s.retain(|(i, _)| i != id);
        s.len() != before
    }

    /// PWRSW_01: drop every session — cancels running downloads and deletes staging.
    /// Returns the number dropped.
    pub fn cancel_all(&self) -> usize {
        let Ok(mut s) = self.sessions.lock() else {
            return 0;
        };
        let n = s.len();
        s.clear();
        n
    }

    /// True while `id` is still in the session list.
    fn is_registered(&self, id: &str) -> bool {
        self.sessions
            .lock()
            .map(|s| s.iter().any(|(i, _)| i == id))
            .unwrap_or(false)
    }

    /// Number of live sessions (tests).
    pub fn session_count(&self) -> usize {
        self.sessions.lock().map(|s| s.len()).unwrap_or(0)
    }

    // ----- start ----------------------------------------------------------------------

    /// Open a session. Network only happens here when `token` is absent, `associateOnly` is
    /// not set, and `consent` is true (the NB_08 consent gate, unchanged).
    pub fn start(
        &self,
        svc: &Arc<Mutex<ServerService>>,
        req: StartRequest,
    ) -> Result<Value, UniteError> {
        self.start_for(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), svc, req)
    }

    /// [`UniteHub::start`] for a session opened by `client` (the per-client cap counts these).
    pub fn start_for(
        &self,
        client: IpAddr,
        svc: &Arc<Mutex<ServerService>>,
        req: StartRequest,
    ) -> Result<Value, UniteError> {
        let key_in = req
            .noubin_key
            .as_deref()
            .or(req.url.as_deref())
            .map(str::trim)
            .filter(|k| !k.is_empty());
        let key = match key_in {
            Some(k) => Some(
                sanitise_key(k)
                    .ok_or_else(|| UniteError::Invalid("the Noubin key is not valid".into()))?,
            ),
            None => None,
        };
        let (root, web) = {
            let g = svc.lock().map_err(|_| UniteError::Busy)?;
            (
                g.primary_root()
                    .map_err(|e| UniteError::Storage(e.to_string()))?,
                g.web(),
            )
        };

        let (release, key, mode) = if let Some(token) = req.token.as_deref() {
            if !crate::web::is_token(token) {
                return Err(WebError::UnknownSession.into());
            }
            let held = web.held(token).ok_or(WebError::UnknownSession)?;
            let k = key.or_else(|| Some(held.noubin_key.clone()));
            (Some(held), k, "web")
        } else if req.associate_only {
            (None, key, "offline")
        } else {
            let k = key.ok_or_else(|| {
                UniteError::Invalid(
                    "give a Noubin URL (noubinKey), a web token or associateOnly".into(),
                )
            })?;
            let fetched = fetch_unlocked(svc, &k, req.consent)?;
            let held = web.held(&fetched.token).ok_or(WebError::UnknownSession)?;
            web.discard(&fetched.token);
            (Some(held), Some(k), "web")
        };

        let (id, overview) = self.create_session_for(client, root, release, key.clone(), None)?;
        Ok(json!({
            "session": id,
            "mode": mode,
            "key": key.as_deref().map(|k| sanitise_line(k, 512)),
            "overview": overview,
            "expires_in_secs": self.session_ttl.as_secs(),
            "limits": self.limits_json(),
        }))
    }

    /// Register a session with its staging directory. `refresh_of` marks a session that only
    /// exists to download images for an existing item (it can never be applied as a new release).
    pub(super) fn create_session(
        &self,
        root: PathBuf,
        release: Option<HeldRelease>,
        key: Option<String>,
        refresh_of: Option<u64>,
    ) -> Result<(String, Option<ReleaseOverview>), UniteError> {
        self.create_session_for(
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            root,
            release,
            key,
            refresh_of,
        )
    }

    fn create_session_for(
        &self,
        client: IpAddr,
        root: PathBuf,
        release: Option<HeldRelease>,
        key: Option<String>,
        refresh_of: Option<u64>,
    ) -> Result<(String, Option<ReleaseOverview>), UniteError> {
        let fs = LibraryFs::new(&root)?;
        let tmp = fs.tmp_dir()?;
        let live: HashSet<String> = self
            .sessions
            .lock()
            .map(|s| s.iter().map(|(i, _)| format!("unite-{i}")).collect())
            .unwrap_or_default();
        sweep_stale(&tmp, &live);

        let id = new_id();
        let stage = tmp.join(format!("unite-{id}"));
        std::fs::create_dir(&stage).map_err(|e| UniteError::Io(e.to_string()))?;
        let overview = release
            .as_ref()
            .map(|h| ReleaseOverview::from_document(&h.doc, &h.warnings));
        let session = Session {
            id: id.clone(),
            owner: client,
            lib_root: root,
            stage,
            touched: Instant::now(),
            release,
            key,
            next_id: 1,
            files: Vec::new(),
            rejected: Vec::new(),
            covers: Vec::new(),
            prepared: None,
            dl: Arc::new(Mutex::new(DlStatus::idle())),
            last_proposal: None,
            confirmed: None,
            applied: None,
            refresh_of,
            budget: Arc::new(Budget::with_staging(
                self.limits.max_batch_bytes,
                Arc::clone(&self.staging),
            )),
            cancel: Cancel::new(),
        };
        let mut s = self.sessions.lock().map_err(|_| UniteError::Busy)?;
        s.retain(|(_, sess)| {
            sess.lock()
                .map(|g| g.touched.elapsed() < self.session_ttl)
                .unwrap_or(false)
        });
        // Never evict another client's live session (the old rule dropped the oldest of the
        // four whoever it belonged to): refuse when every slot is live.
        let rows: Vec<SessionRow> = s
            .iter()
            .map(|(_, sess)| {
                sess.lock()
                    .map(|g| SessionRow {
                        owner: g.owner,
                        idle: if g.dl.lock().map(|d| d.state == "running").unwrap_or(false) {
                            Duration::ZERO
                        } else {
                            g.touched.elapsed()
                        },
                    })
                    .unwrap_or(SessionRow {
                        owner: client,
                        idle: Duration::ZERO,
                    })
            })
            .collect();
        match place_session(&rows, client, &self.session_limits) {
            SlotPlacement::Add => {}
            SlotPlacement::Replace(i) => {
                s.remove(i); // Drop deletes its staging
            }
            SlotPlacement::Refuse => {
                return Err(UniteError::TooMany(
                    "too many unite sessions are open; finish or cancel one first".into(),
                ));
            }
        }
        s.push((id.clone(), Arc::new(Mutex::new(session))));
        Ok((id, overview))
    }

    fn limits_json(&self) -> Value {
        json!({
            "max_file_bytes": self.limits.max_file_bytes,
            "max_batch_bytes": self.limits.max_batch_bytes,
            "max_image_bytes": self.limits.max_image_bytes,
            "max_image_pixels_per_side": MAX_IMAGE_SIDE,
        })
    }

    // ----- status ---------------------------------------------------------------------

    pub fn status(&self, id: &str) -> Result<Value, UniteError> {
        self.with(id, |s| Ok(self.status_json(s)))
    }

    fn status_json(&self, s: &Session) -> Value {
        let files: Vec<Value> = s
            .files
            .iter()
            .map(|f| {
                json!({
                    "id": f.id, "name": f.name, "size": f.size,
                    "kind": match f.kind {
                        FileKind::Audio(_) => "audio",
                        FileKind::Image(_) => if f.thumb { "thumbnail" } else { "image" },
                        FileKind::Noudata => "noudata",
                    },
                    "format": match f.kind {
                        FileKind::Audio(k) => Some(k.ext()),
                        FileKind::Image(i) => Some(i.kind.ext()),
                        FileKind::Noudata => None,
                    },
                    "source": match f.source {
                        Source::Upload => "upload",
                        Source::Download { .. } => "download",
                    },
                    "items": match &f.source { Source::Download { items, .. } => json!(items), _ => json!([]) },
                })
            })
            .collect();
        json!({
            "session": s.id,
            "key": s.key.as_deref().map(|k| sanitise_line(k, 512)),
            "has_release": s.release.is_some(),
            "overview": s.release.as_ref().map(|h| ReleaseOverview::from_document(&h.doc, &h.warnings)),
            "files": files,
            "rejected": s.rejected.iter().map(|(n, w)| json!({"name": n, "reason": w})).collect::<Vec<_>>(),
            "covers": s.covers.iter().map(|c| json!({
                "entry": c.entry, "width": c.info.width, "height": c.info.height,
                "thumbnails": c.thumbs.len(),
            })).collect::<Vec<_>>(),
            "download": s.dl.lock().map(|d| d.to_json()).unwrap_or(Value::Null),
            "prepared": s.prepared.as_ref().map(|p| p.id.clone()),
            "confirmed": s.confirmed.is_some(),
            "applied": s.applied,
            "used_bytes": s.budget.used(),
            "expires_in_secs": self.session_ttl.saturating_sub(s.touched.elapsed()).as_secs(),
        })
    }

    // ----- uploads (Case B / C) ---------------------------------------------------------

    /// Stage one uploaded file. `content_length` (when the client sent one) is checked against
    /// the caps and the free space before a byte is read; the same limits are enforced while
    /// streaming. The **content** decides what the file is (audio / image / `.noudata`); the
    /// client's file name only supplies the stem.
    pub fn add_upload(
        &self,
        id: &str,
        raw_name: &str,
        content_length: Option<u64>,
        body: &mut dyn Read,
    ) -> Result<Value, UniteError> {
        let sess = self.get(id)?;
        let (stage, budget, cancel, file_id) = {
            let mut g = sess.lock().map_err(|_| UniteError::UnknownSession)?;
            if g.applied.is_some() {
                return Err(UniteError::NotReady("this unite is already applied".into()));
            }
            if g.files.len() >= MAX_STAGED_FILES {
                return Err(UniteError::TooLarge(format!(
                    "at most {MAX_STAGED_FILES} files per unite"
                )));
            }
            let fid = g.next_id;
            g.next_id += 1;
            (
                g.stage.clone(),
                Arc::clone(&g.budget),
                g.cancel.clone(),
                fid,
            )
        };
        let lim = &self.limits;
        if let Some(len) = content_length {
            if len > lim.max_file_bytes {
                return Err(UniteError::TooLarge(
                    DownloadError::TooLarge {
                        limit: lim.max_file_bytes,
                    }
                    .to_string(),
                ));
            }
            if len > budget.remaining() {
                return Err(UniteError::TooLarge(
                    DownloadError::BatchTooLarge {
                        limit: budget.max(),
                    }
                    .to_string(),
                ));
            }
            if let Some(free) = (lim.free_space)(&stage) {
                if len.saturating_add(lim.reserve_for(&stage)) > free {
                    return Err(UniteError::NoSpace(
                        DownloadError::NoSpace { needed: len, free }.to_string(),
                    ));
                }
            }
            budget
                .admit_shared(lim, &stage, len)
                .map_err(|e| UniteError::NoSpace(e.to_string()))?;
        }
        let name_lc = raw_name.to_ascii_lowercase();
        let in_thumbs = {
            let parts: Vec<&str> = raw_name.split(['/', '\\']).collect();
            parts.len() >= 2 && parts[parts.len() - 2].eq_ignore_ascii_case("thumbnails")
        };
        let max_image = lim.max_image_bytes;
        let max_file = lim.max_file_bytes;
        let noudata_name = name_lc.ends_with(".noudata");
        let mut classify = |head: &[u8]| -> Result<Option<u64>, DownloadError> {
            match sniff_audio(head) {
                MediaSniff::Audio(kind) => {
                    require_playable(&lim.capabilities, kind)?;
                    return Ok(Some(max_file));
                }
                MediaSniff::Unsupported(w) => return Err(DownloadError::WrongType(w.into())),
                MediaSniff::Unknown => {}
            }
            if sniff_image_kind(head).is_some() {
                return Ok(Some(max_image));
            }
            if noudata_name && first_significant_byte(head) == Some(b'{') {
                return Ok(Some(MAX_NOUDATA_UPLOAD_BYTES));
            }
            Err(DownloadError::WrongType(
                "not audio Plippa can play, a JPEG/PNG/WebP image or a .noudata file".into(),
            ))
        };
        // The session stays alive while its own upload is streaming, however long that takes
        // (the 30-minute clock used to run from the start of the call and could drop a session
        // out from under a big upload, after which `finish` said "unknown session").
        let alive = Arc::clone(&sess);
        let mut body = ActivityReader::new(body, move || {
            if let Ok(mut g) = alive.try_lock() {
                g.touched = Instant::now();
            }
        });
        let streamed = stream_to_temp(
            &mut body,
            content_length,
            &stage,
            StreamRules {
                cap: max_file,
                lim,
                budget: &budget,
                cancel: &cancel,
            },
            &mut classify,
            &mut |_, _| {},
        );
        let streamed = match streamed {
            Ok(s) => s,
            Err(e) => {
                let msg = e.to_string();
                if let Ok(mut g) = sess.lock() {
                    push_rejected(&mut g, raw_name, msg.clone());
                }
                return Err(e.into());
            }
        };
        let reserved = streamed.reserved;
        let head = streamed.head.clone();
        let reject = |budget: &Budget, msg: String| -> UniteError {
            budget.release(reserved);
            if let Ok(mut g) = sess.lock() {
                push_rejected(&mut g, raw_name, msg.clone());
            }
            UniteError::Unsupported(msg)
        };
        let (kind, final_name) = match sniff_audio(&head) {
            MediaSniff::Audio(k) => (FileKind::Audio(k), staged_name(raw_name, k.ext())),
            _ => match sniff_image_kind(&head) {
                Some(_) => {
                    let info = validate_image_file(streamed.temp.path(), MAX_IMAGE_SIDE)
                        .map_err(|e| reject(&budget, e.to_string()))?;
                    (
                        FileKind::Image(info),
                        staged_name(raw_name, info.kind.ext()),
                    )
                }
                None => {
                    // `.noudata`: one JSON object within the structural limits.
                    let bytes = std::fs::read(streamed.temp.path())
                        .map_err(|e| UniteError::Io(e.to_string()))?;
                    if let Err(e) = check_json_object_limits(&bytes) {
                        return Err(reject(&budget, format!("not a usable .noudata ({e})")));
                    }
                    let base = raw_name.rsplit(['/', '\\']).next().unwrap_or(raw_name);
                    let stem = base
                        .strip_suffix(".noudata")
                        .or_else(|| base.get(..base.len().saturating_sub(8)));
                    let stem = stem.unwrap_or("release");
                    (
                        FileKind::Noudata,
                        safe_path_segment(
                            &format!("{stem}.noudata"),
                            "release.noudata",
                            MAX_SEGMENT_BYTES,
                        ),
                    )
                }
            },
        };
        let dir = stage.join(format!("f{file_id}"));
        if let Err(e) = std::fs::create_dir(&dir) {
            budget.release(reserved);
            return Err(UniteError::Io(e.to_string()));
        }
        let path = dir.join(&final_name);
        let size = streamed.size;
        let tmp_path = streamed.temp.keep();
        if let Err(e) = super::fsops::move_no_replace(&tmp_path, &path) {
            let _ = std::fs::remove_file(&tmp_path);
            budget.release(reserved);
            return Err(e.into());
        }
        let is_image = matches!(kind, FileKind::Image(_));
        let file = StagedFile {
            id: file_id,
            name: final_name,
            path,
            size,
            kind,
            source: Source::Upload,
            thumb: in_thumbs && is_image,
        };
        let out = json!({
            "id": file.id, "name": file.name, "size": file.size,
            "kind": match file.kind {
                FileKind::Audio(_) => "audio",
                FileKind::Image(_) => if file.thumb { "thumbnail" } else { "image" },
                FileKind::Noudata => "noudata",
            },
        });
        // The session may have been discarded while the body was arriving: do not report a
        // successful upload into a session nobody can reach any more.
        if !self.is_registered(id) {
            let _ = std::fs::remove_file(&file.path);
            budget.release(reserved);
            return Err(UniteError::UnknownSession);
        }
        let mut g = sess.lock().map_err(|_| UniteError::UnknownSession)?;
        g.files.push(file);
        g.last_proposal = None;
        g.confirmed = None;
        Ok(out)
    }

    // ----- download consent + worker (Case A, images) ------------------------------------

    /// Step 1: the list of **every** URL a download would contact. Nothing is sent. Sizes are
    /// unknown at this point (a request to learn them would itself be a network action without
    /// consent); each response's `Content-Length` is checked against the caps and the free
    /// space before its body is read, and the per-file progress shows it.
    pub fn download_prepare(&self, id: &str, req: PrepareRequest) -> Result<Value, UniteError> {
        let lim = self.limits.clone();
        self.with(id, |s| {
            let release = s
                .release
                .as_ref()
                .ok_or_else(|| UniteError::NotReady("this unite has no release metadata".into()))?;
            let doc = &release.doc;
            let mut targets: Vec<Target> = Vec::new();
            let mut refused: Vec<Value> = Vec::new();
            if req.media {
                let mut by_url: Vec<(String, Vec<usize>, String)> = Vec::new();
                for (i, item) in doc.media_list.iter().enumerate() {
                    let Some(u) = item
                        .unite_data
                        .as_ref()
                        .and_then(|u| u.free_download_url.as_deref())
                    else {
                        continue;
                    };
                    match parse_fetch_url(u) {
                        Ok(p) => {
                            let url = p.to_fetch_string();
                            let hint = item
                                .unite_data
                                .as_ref()
                                .and_then(|d| d.expected_filename.clone())
                                .or_else(|| url_file_name(&url))
                                .unwrap_or_else(|| "track".into());
                            match by_url.iter_mut().find(|(x, _, _)| *x == url) {
                                Some((_, items, _)) => items.push(i),
                                None => by_url.push((url, vec![i], hint)),
                            }
                        }
                        Err(e) => refused.push(json!({
                            "url": sanitise_line(u, 300), "reason": e.to_string(), "items": [i],
                        })),
                    }
                }
                for (url, items, name_hint) in by_url {
                    targets.push(Target {
                        url,
                        kind: TargetKind::Media { items, name_hint },
                    });
                }
            }
            let mut covers = Vec::new();
            if req.images {
                let (plan, bad) = plan_cover_targets(doc);
                for (u, why) in bad {
                    refused.push(json!({"url": sanitise_line(&u, 300), "reason": why}));
                }
                for c in &plan {
                    targets.push(Target {
                        url: c.url.clone(),
                        kind: TargetKind::Cover { entry: c.entry },
                    });
                }
                for c in &plan {
                    for t in c.thumbnails() {
                        targets.push(Target {
                            url: t.url,
                            kind: TargetKind::Thumb {
                                entry: c.entry,
                                tier: t.tier,
                                ext: t.ext,
                                fallback: t.fallback_url,
                            },
                        });
                    }
                }
                covers = plan;
            }
            if targets.is_empty() {
                return Err(UniteError::NotReady(
                    "nothing to download: no free-download links or cover images".into(),
                ));
            }
            if targets.len() > MAX_DOWNLOAD_TARGETS {
                return Err(UniteError::TooLarge(format!(
                    "more than {MAX_DOWNLOAD_TARGETS} URLs; refusing"
                )));
            }
            let consent_id = new_id();
            let urls: Vec<Value> = targets
                .iter()
                .map(|t| {
                    let mut v = json!({
                        "url": t.url, "kind": t.label(), "size": Value::Null,
                    });
                    match &t.kind {
                        TargetKind::Media { items, .. } => v["items"] = json!(items),
                        TargetKind::Thumb { fallback, .. } => {
                            v["tried_if_missing"] = json!(fallback);
                            v["optional"] = json!(true);
                        }
                        TargetKind::Cover { .. } => {}
                    }
                    v
                })
                .collect();
            s.prepared = Some(Prepared {
                id: consent_id.clone(),
                targets,
                covers,
            });
            let hosts: Vec<String> = {
                let mut h: Vec<String> = s
                    .prepared
                    .as_ref()
                    .map(|p| {
                        p.targets
                            .iter()
                            .filter_map(|t| parse_fetch_url(&t.url).ok())
                            .map(|u| u.host.authority_text())
                            .collect()
                    })
                    .unwrap_or_default();
                h.sort();
                h.dedup();
                h
            };
            Ok(json!({
                "consent_id": consent_id,
                "urls": urls,
                "refused": refused,
                "hosts": hosts,
                "limits": {
                    "max_file_bytes": lim.max_file_bytes,
                    "max_batch_bytes": lim.max_batch_bytes,
                    "max_image_bytes": lim.max_image_bytes,
                },
                "message": format!(
                    "This will download the files listed here from {} over the internet. \
                     Sizes are checked as each download starts (limit {} MB per file). Continue?",
                    hosts.join(", "), lim.max_file_bytes / (1024 * 1024)
                ),
            }))
        })
    }

    /// Step 2: start the worker. Needs `consent: true` **and** the `consentId` of the list that
    /// was shown; a list that changed since is refused. Returns immediately; poll `status`.
    pub fn download_start(
        &self,
        svc: &Arc<Mutex<ServerService>>,
        id: &str,
        req: DownloadRequest,
    ) -> Result<Value, UniteError> {
        let consent = Consent::from_request(req.consent)?;
        let (transport, policy) = {
            let g = svc.lock().map_err(|_| UniteError::Busy)?;
            let web = g.web();
            (web.transport(), web.policy())
        };
        let sess = self.get(id)?;
        let (targets, covers, stage, budget, cancel, dl) = {
            let mut g = sess.lock().map_err(|_| UniteError::UnknownSession)?;
            if g.applied.is_some() {
                return Err(UniteError::NotReady("this unite is already applied".into()));
            }
            let p = g.prepared.as_ref().ok_or_else(|| {
                UniteError::NotReady("call download/prepare first: the URLs must be shown".into())
            })?;
            if req.consent_id.as_deref() != Some(p.id.as_str()) {
                return Err(WebError::ConsentRequired.into());
            }
            if self
                .download_busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Err(UniteError::Busy);
            }
            let items: Vec<DlItem> = p
                .targets
                .iter()
                .map(|t| DlItem {
                    url: redact_url_for_log(&t.url),
                    kind: t.label(),
                    state: "queued",
                    received: 0,
                    total: None,
                    error: None,
                })
                .collect();
            let (targets, covers) = (p.targets.clone(), p.covers.clone());
            g.prepared = None; // one consent = one run
            g.cancel = Cancel::new();
            let dl = Arc::new(Mutex::new(DlStatus {
                state: "running",
                items,
                error: None,
                warnings: Vec::new(),
            }));
            g.dl = Arc::clone(&dl);
            (
                targets,
                covers,
                g.stage.clone(),
                Arc::clone(&g.budget),
                g.cancel.clone(),
                dl,
            )
        };
        let job = Job {
            session: Arc::clone(&sess),
            dl,
            transport,
            policy,
            lim: self.limits.clone(),
            consent,
            targets,
            covers,
            stage,
            budget,
            cancel,
            busy: BusyGuard(Arc::clone(&self.download_busy)),
        };
        let handle = std::thread::Builder::new()
            .name("plippa-unite-dl".into())
            .spawn(move || run_download(job))
            .map_err(|e| {
                self.download_busy.store(false, Ordering::SeqCst);
                UniteError::Io(e.to_string())
            })?;
        if let Ok(mut w) = self.workers.lock() {
            w.retain(|h| !h.is_finished());
            w.push(handle);
        }
        Ok(json!({"ok": true, "state": "running"}))
    }

    /// Ask the worker to stop; its current partial file is deleted.
    pub fn download_cancel(&self, id: &str) -> Result<Value, UniteError> {
        self.with(id, |s| {
            s.cancel.cancel();
            Ok(json!({"ok": true}))
        })
    }

    // ----- proposal / confirm ----------------------------------------------------------

    pub fn proposal(&self, id: &str) -> Result<Value, UniteError> {
        self.with(id, |s| {
            let case_c = case_c_file(s);
            let mut ids: HashMap<PathBuf, u32> = HashMap::new();
            for f in &s.files {
                ids.insert(f.path.clone(), f.id);
            }
            if let Some(c) = case_c {
                let files = s.files.clone();
                let audio: Vec<Value> = files
                    .iter()
                    .filter(|f| matches!(f.kind, FileKind::Audio(_)))
                    .map(|f| json!({"id": f.id, "name": f.name}))
                    .collect();
                return Ok(json!({
                    "case": "C",
                    "case_c": {"id": c.id, "name": c.name},
                    "files": audio,
                    "complete": true,
                    "needs_review": false,
                    "matches": [], "unmatched_items": [], "unmatched_files": [],
                    "unsupported": s.rejected.iter().map(|(n, w)| json!({"name": n, "reason": w})).collect::<Vec<_>>(),
                    "warnings": ["A release .noudata was supplied: the files will only be associated, not re-matched."],
                }));
            }
            let release = s
                .release
                .as_ref()
                .ok_or_else(|| UniteError::NotReady("no release metadata for this session".into()))?;
            let profile = release.doc.profile();
            let name_of = |p: &Path| -> Value {
                let f = s.files.iter().find(|f| f.path == p);
                json!({"id": f.map(|f| f.id), "name": f.map(|f| f.name.clone())})
            };
            let item_title = |i: usize| -> Value {
                json!({
                    "item": i, "position": i + 1,
                    "title": release.doc.media_list.get(i).and_then(|m| m.media_title.clone()),
                })
            };
            // Downloads from an item's own link map deterministically (no guessing).
            let audio: Vec<&StagedFile> = s
                .files
                .iter()
                .filter(|f| matches!(f.kind, FileKind::Audio(_)))
                .collect();
            let all_downloads = !audio.is_empty()
                && audio.iter().all(|f| matches!(&f.source, Source::Download { items, .. } if items.len() == 1));
            let mut covered: Vec<usize> = audio
                .iter()
                .filter_map(|f| match &f.source {
                    Source::Download { items, .. } if items.len() == 1 => Some(items[0]),
                    _ => None,
                })
                .collect();
            covered.sort_unstable();
            covered.dedup();
            let deterministic = all_downloads
                && covered.len() == audio.len()
                && covered.len() == release.doc.media_list.len();
            if deterministic {
                let mut proposal = UniteProposal::default();
                for f in &audio {
                    let Source::Download { items, .. } = &f.source else { continue };
                    proposal.matches.push(plippa_noubin::unite::UniteMatch {
                        item_index: items[0],
                        files: vec![plippa_noubin::unite::MatchedFile {
                            path: f.path.clone(),
                            confidence: 1.0,
                            reasons: vec![plippa_noubin::unite::MatchReason::ExpectedFilename],
                        }],
                        confidence: 1.0,
                        level: plippa_noubin::unite::Confidence::Confident,
                        reasons: vec![plippa_noubin::unite::MatchReason::ExpectedFilename],
                    });
                }
                proposal.matches.sort_by_key(|m| m.item_index);
                let out = proposal_json(&proposal, &name_of, &item_title, true);
                s.last_proposal = Some((proposal, ids));
                return Ok(out);
            }
            let cands = build_candidates_of(&s.files);
            let proposal = propose(&release.doc, &cands, profile);
            let out = proposal_json(&proposal, &name_of, &item_title, false);
            s.last_proposal = Some((proposal, ids));
            Ok(out)
        })
    }

    /// Fix the mapping: start from the proposal (`accept`), apply manual assignments, validate.
    /// Returns a preview of what `apply` will do.
    pub fn confirm(&self, id: &str, req: ConfirmRequest) -> Result<Value, UniteError> {
        self.with(id, |s| {
            if s.applied.is_some() {
                return Err(UniteError::NotReady("this unite is already applied".into()));
            }
            let release = s.release.as_ref().ok_or_else(|| {
                UniteError::NotReady("no release metadata for this session".into())
            })?;
            let n_items = release.doc.media_list.len();
            let by_id: HashMap<u32, &StagedFile> = s.files.iter().map(|f| (f.id, f)).collect();
            let is_audio = |id: u32| {
                by_id
                    .get(&id)
                    .is_some_and(|f| matches!(f.kind, FileKind::Audio(_)))
            };
            // Working assignment: item -> file ids.
            let mut work: Vec<(usize, Vec<u32>)> = Vec::new();
            if let Some(mode) = req.accept.as_deref() {
                let (p, ids) = s
                    .last_proposal
                    .as_ref()
                    .ok_or_else(|| UniteError::NotReady("ask for the proposal first".into()))?;
                let m = match mode {
                    "all" => p.confirm_all(),
                    "confident" => p.confirm_confident(),
                    _ => {
                        return Err(UniteError::Invalid(
                            "accept must be \"all\" or \"confident\"".into(),
                        ))
                    }
                };
                for a in m.assignments {
                    let fids: Vec<u32> =
                        a.files.iter().filter_map(|f| ids.get(f).copied()).collect();
                    work.push((a.item_index, fids));
                }
            }
            for a in &req.assignments {
                if a.item >= n_items {
                    return Err(UniteError::Invalid(format!(
                        "item {} does not exist",
                        a.item
                    )));
                }
                for f in &a.files {
                    if !is_audio(*f) {
                        return Err(UniteError::Invalid(format!(
                            "file {f} is not an audio file of this session"
                        )));
                    }
                }
                // The manual choice wins: take these files from every other item first.
                for (_, fids) in work.iter_mut() {
                    fids.retain(|f| !a.files.contains(f));
                }
                work.retain(|(i, _)| *i != a.item);
                work.push((a.item, a.files.clone()));
            }
            for f in &req.ignored {
                if !is_audio(*f) {
                    return Err(UniteError::Invalid(format!(
                        "file {f} is not an audio file of this session"
                    )));
                }
            }
            for (_, fids) in work.iter_mut() {
                fids.retain(|f| !req.ignored.contains(f));
            }
            work.retain(|(_, f)| !f.is_empty());
            work.sort_by_key(|(i, _)| *i);
            let path_of = |id: u32| by_id[&id].path.clone();
            let mapping = ConfirmedMapping {
                assignments: work
                    .iter()
                    .map(|(i, fids)| ConfirmedItem {
                        item_index: *i,
                        files: fids.iter().map(|f| path_of(*f)).collect(),
                    })
                    .collect(),
                known_files: s
                    .files
                    .iter()
                    .filter(|f| matches!(f.kind, FileKind::Audio(_)))
                    .map(|f| f.path.clone())
                    .collect(),
                ignored_files: req.ignored.iter().map(|f| path_of(*f)).collect(),
                allow_incomplete: req.allow_incomplete,
            };
            // Dry run of the build: validates the mapping and yields the preview.
            let fs = LibraryFs::new(&s.lib_root)?;
            let placement = Placement::for_release(&release.doc, release.doc.profile());
            let placement = {
                let names = fs.existing_names(&placement.dir);
                placement.with_existing_names(names)
            };
            let plan = build_local_doc(&release.doc, &mapping, s.key.as_deref(), &placement)
                .map_err(|e| match e {
                    MatchError::Incomplete {
                        missing_items,
                        leftover_files,
                    } => {
                        let ids_of: HashMap<PathBuf, u32> =
                            s.files.iter().map(|f| (f.path.clone(), f.id)).collect();
                        UniteError::Incomplete {
                            missing_items,
                            leftover_files: leftover_files
                                .iter()
                                .filter_map(|p| ids_of.get(p).copied())
                                .collect(),
                        }
                    }
                    MatchError::InvalidKey => UniteError::Invalid(e.to_string()),
                    other => UniteError::Invalid(other.to_string()),
                })?;
            let preview = json!({
                "ok": true,
                "folder": plan.noudata_path.parent().map(|p| p.to_string_lossy().into_owned()),
                "noudata": plan.noudata_path.file_name().map(|n| n.to_string_lossy().into_owned()),
                "files": plan.file_ops.iter().map(|op| json!({
                    "item": op.item_index,
                    "from": op.source.file_name().map(|n| n.to_string_lossy().into_owned()),
                    "to": op.target.file_name().map(|n| n.to_string_lossy().into_owned()),
                    "op": op.op,
                })).collect::<Vec<_>>(),
                "missing_items": plan.missing_items,
                "warnings": plan.warnings.iter().map(|w| w.message.clone()).collect::<Vec<_>>(),
            });
            s.confirmed = Some(Confirmed { mapping });
            Ok(preview)
        })
    }

    // ----- apply ---------------------------------------------------------------------

    /// Apply the confirmed mapping (Case A/B) or associate the supplied release (Case C):
    /// create the folder, move media and images in, write the `.noudata` **last**, rescan.
    /// Any failure rolls the moves back and leaves no `.noudata`.
    pub fn apply(
        &self,
        svc: &Arc<Mutex<ServerService>>,
        id: &str,
        req: ApplyRequest,
    ) -> Result<Value, UniteError> {
        let sess = self.get(id)?;
        let mut g = sess.lock().map_err(|_| UniteError::UnknownSession)?;
        if g.applied.is_some() {
            return Err(UniteError::NotReady("this unite is already applied".into()));
        }
        if g.refresh_of.is_some() {
            return Err(UniteError::NotReady(
                "this session refreshes an existing item; use the refresh endpoints".into(),
            ));
        }
        if self.download_busy.load(Ordering::SeqCst)
            && g.dl.lock().map(|d| d.state == "running").unwrap_or(false)
        {
            return Err(UniteError::NotReady("a download is still running".into()));
        }
        let key = match req.key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            Some(k) => Some(
                sanitise_key(k)
                    .ok_or_else(|| UniteError::Invalid("the Noubin key is not valid".into()))?,
            ),
            None => g.key.clone(),
        };
        let case_c = case_c_file(&g).cloned();
        let built = if let Some(c) = case_c {
            build_case_c(&g, &c, key.as_deref())?
        } else {
            build_case_ab(&g, key.as_deref())?
        };
        // The key must not already point somewhere else.
        if let Some(k) = built.doc.noubin_key.as_deref() {
            let taken = svc
                .lock()
                .map(|s| s.has_local_match(k))
                .map_err(|_| UniteError::Busy)?;
            if taken && !req.allow_duplicate_key {
                return Err(UniteError::KeyInUse);
            }
        }
        let fs = LibraryFs::new(&g.lib_root)?;
        let noudata_abs = write_release(&fs, &g.stage, built)?;
        // Index the new release; the library index is derived from the files just written.
        let summary = {
            let mut sv = svc.lock().map_err(|_| UniteError::Busy)?;
            sv.finish_unite_write(&g.lib_root, &noudata_abs)
                .map_err(|e| UniteError::Io(e.to_string()))?
        };
        let rel = noudata_abs
            .strip_prefix(fs.root())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let out = json!({
            "ok": true,
            "noudata": rel,
            "playable": summary,
            "key": key.as_deref().map(|k| sanitise_line(k, 512)),
            "normalised_key": key.as_deref().map(normalise_noubin_key),
        });
        remove_stage(&g.lib_root, &g.stage);
        g.files.clear();
        g.covers.clear();
        g.applied = Some(out.clone());
        Ok(out)
    }
}

fn sweep_stale(tmp: &Path, live: &HashSet<String>) {
    let Ok(rd) = std::fs::read_dir(tmp) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("unite-") && !live.contains(&name) {
            if let Ok(m) = std::fs::symlink_metadata(e.path()) {
                if m.file_type().is_dir() {
                    let _ = std::fs::remove_dir_all(e.path());
                } else {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

/// The supplied `_<release>.noudata` (Case C), if any.
fn case_c_file(s: &Session) -> Option<&StagedFile> {
    s.files
        .iter()
        .find(|f| matches!(f.kind, FileKind::Noudata) && f.name.starts_with('_'))
}

fn probe_candidate(f: &StagedFile) -> CandidateFile {
    use lofty::prelude::*;
    let mut c = CandidateFile::new(f.path.clone(), f.size);
    if let Ok(tagged) = lofty::read_from_path(&f.path) {
        let ms = tagged.properties().duration().as_millis() as u64;
        if ms > 0 {
            c = c.with_duration_ms(ms);
        }
        if let Some(t) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            // Tags are untrusted text: stripped/capped before the matcher ever sees them.
            let clean = |s: Option<std::borrow::Cow<'_, str>>| {
                s.map(|s| sanitise_line(&s, 256)).filter(|s| !s.is_empty())
            };
            c = c.with_tags(EmbeddedTags {
                title: clean(t.title()),
                artist: clean(t.artist()),
                album: clean(t.album()),
                track_number: t.track(),
                disc_number: t.disk(),
            });
        }
    }
    c
}

fn build_candidates_of(files: &[StagedFile]) -> Vec<CandidateFile> {
    files
        .iter()
        .filter(|f| !f.thumb)
        .map(|f| match f.kind {
            FileKind::Audio(_) => probe_candidate(f),
            _ => CandidateFile::new(f.path.clone(), f.size),
        })
        .collect()
}

fn proposal_json(
    p: &UniteProposal,
    name_of: &dyn Fn(&Path) -> Value,
    item: &dyn Fn(usize) -> Value,
    deterministic: bool,
) -> Value {
    let level = |l: plippa_noubin::unite::Confidence| match l {
        plippa_noubin::unite::Confidence::Confident => "confident",
        plippa_noubin::unite::Confidence::Low => "low",
    };
    let guess = |g: &Option<plippa_noubin::unite::Guess>| -> Value {
        match g {
            Some(g) => json!({
                "item": g.item_index,
                "file": g.path.as_deref().map(name_of),
                "confidence": g.confidence,
            }),
            None => Value::Null,
        }
    };
    json!({
        "case": if deterministic { "A" } else { "B" },
        "deterministic": deterministic,
        "complete": p.is_complete(),
        "needs_review": p.needs_review() && !deterministic,
        "matches": p.matches.iter().map(|m| {
            let mut v = item(m.item_index);
            v["files"] = json!(m.files.iter().map(|f| name_of(&f.path)).collect::<Vec<_>>());
            v["confidence"] = json!(m.confidence);
            v["level"] = json!(level(m.level));
            v["reasons"] = serde_json::to_value(&m.reasons).unwrap_or(Value::Null);
            v
        }).collect::<Vec<_>>(),
        "unmatched_items": p.unmatched_items.iter().map(|u| {
            let mut v = item(u.item_index);
            v["best_guess"] = guess(&u.best_guess);
            v
        }).collect::<Vec<_>>(),
        "unmatched_files": p.unmatched_files.iter().map(|u| {
            let mut v = name_of(&u.path);
            v["best_guess"] = guess(&u.best_guess);
            v
        }).collect::<Vec<_>>(),
        "unsupported": p.unsupported_files.iter().map(|f| name_of(f)).collect::<Vec<_>>(),
        "ignored": p.ignored_files.iter().map(|f| name_of(f)).collect::<Vec<_>>(),
        "warnings": p.warnings.iter().map(|w| sanitise_line(&w.message, 300)).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// Download worker
// ---------------------------------------------------------------------------

struct Job {
    session: Arc<Mutex<Session>>,
    dl: Arc<Mutex<DlStatus>>,
    transport: Arc<dyn Transport>,
    policy: FetchPolicy,
    lim: DownloadLimits,
    consent: Consent,
    targets: Vec<Target>,
    covers: Vec<CoverTarget>,
    stage: PathBuf,
    budget: Arc<Budget>,
    cancel: Cancel,
    #[allow(dead_code)] // held only for its Drop (clears the busy flag)
    busy: BusyGuard,
}

impl Job {
    fn set(&self, i: usize, f: impl FnOnce(&mut DlItem)) {
        if let Ok(mut d) = self.dl.lock() {
            if let Some(it) = d.items.get_mut(i) {
                f(it);
            }
        }
    }

    fn finish(&self, state: &'static str, error: Option<String>) {
        if let Ok(mut d) = self.dl.lock() {
            d.state = state;
            d.error = error;
            for it in d
                .items
                .iter_mut()
                .filter(|i| i.state == "queued" || i.state == "running")
            {
                it.state = "skipped";
            }
        }
    }

    fn warn(&self, m: String) {
        if let Ok(mut d) = self.dl.lock() {
            d.warnings.push(m);
        }
    }

    fn fetch(
        &self,
        i: usize,
        url: &str,
        want: Want,
    ) -> Result<crate::web::download::Downloaded, DownloadError> {
        self.set(i, |it| it.state = "running");
        let mut last = Instant::now() - Duration::from_secs(1);
        let r = download_to_temp(
            self.transport.as_ref(),
            &self.consent,
            &self.policy,
            &self.lim,
            url,
            &self.stage,
            want,
            &self.budget,
            &self.cancel,
            &mut |got, total| {
                if last.elapsed() >= Duration::from_millis(200) {
                    last = Instant::now();
                    self.set(i, |it| {
                        it.received = got;
                        it.total = total;
                    });
                }
            },
        );
        match &r {
            Ok(d) => self.set(i, |it| {
                it.state = "done";
                it.received = d.size;
                it.total = Some(d.size);
            }),
            Err(e) => {
                let msg = e.to_string();
                self.set(i, |it| {
                    it.state = if matches!(e, DownloadError::Cancelled) {
                        "cancelled"
                    } else {
                        "failed"
                    };
                    it.error = Some(msg);
                });
            }
        }
        r
    }

    /// Move a finished temp file into `<stage>/<sub>/<name>`.
    fn place(
        &self,
        temp: crate::web::download::TempFile,
        sub: &str,
        name: &str,
    ) -> Result<PathBuf, UniteError> {
        let dir = self.stage.join(sub);
        std::fs::create_dir_all(&dir).map_err(|e| UniteError::Io(e.to_string()))?;
        let dst = dir.join(name);
        let src = temp.keep();
        if let Err(e) = super::fsops::move_no_replace(&src, &dst) {
            let _ = std::fs::remove_file(&src);
            return Err(e.into());
        }
        Ok(dst)
    }
}

fn run_download(job: Job) {
    let n = job.targets.len();
    let mut covers_ok: HashSet<usize> = HashSet::new();
    for i in 0..n {
        if job.cancel.is_cancelled() {
            job.finish("cancelled", None);
            return;
        }
        let target = job.targets[i].clone();
        match &target.kind {
            TargetKind::Media { items, name_hint } => {
                // Already staged by an earlier run of this session: keep it.
                let have = job.session.lock().map(|s| {
                    s.files.iter().any(|f| matches!(&f.source, Source::Download { url, .. } if *url == target.url))
                }).unwrap_or(false);
                if have {
                    job.set(i, |it| it.state = "done");
                    continue;
                }
                match job.fetch(i, &target.url, Want::Media) {
                    Ok(d) => {
                        let kind = d.audio.unwrap_or(AudioKind::Mp3);
                        let name = staged_name(name_hint, kind.ext());
                        let (fid, size) = match job.session.lock() {
                            Ok(mut s) => {
                                let id = s.next_id;
                                s.next_id += 1;
                                (id, d.size)
                            }
                            Err(_) => {
                                job.finish("failed", Some("session lost".into()));
                                return;
                            }
                        };
                        match job.place(d.temp, &format!("f{fid}"), &name) {
                            Ok(path) => {
                                if let Ok(mut s) = job.session.lock() {
                                    s.files.push(StagedFile {
                                        id: fid,
                                        name,
                                        path,
                                        size,
                                        kind: FileKind::Audio(kind),
                                        source: Source::Download {
                                            url: target.url.clone(),
                                            items: items.clone(),
                                        },
                                        thumb: false,
                                    });
                                    s.last_proposal = None;
                                    s.confirmed = None;
                                }
                            }
                            Err(e) => {
                                job.finish("failed", Some(e.to_string()));
                                return;
                            }
                        }
                    }
                    Err(DownloadError::Cancelled) => {
                        job.finish("cancelled", None);
                        return;
                    }
                    Err(e) => {
                        // A media file is the point of the run: stop, keep what is complete.
                        job.finish(
                            "failed",
                            Some(format!("{}: {e}", redact_url_for_log(&target.url))),
                        );
                        return;
                    }
                }
            }
            TargetKind::Cover { entry } => match job.fetch(i, &target.url, Want::Image) {
                Ok(d) => {
                    let info = d.image.expect("image download carries its info");
                    let stem = job
                        .covers
                        .iter()
                        .find(|c| c.entry == *entry)
                        .map(|c| c.local_stem.clone())
                        .unwrap_or_else(|| "cover".into());
                    let name = format!("{stem}.{}", info.kind.ext());
                    match job.place(d.temp, &format!("c{entry}"), &name) {
                        Ok(path) => {
                            covers_ok.insert(*entry);
                            if let Ok(mut s) = job.session.lock() {
                                s.covers.retain(|c| c.entry != *entry);
                                s.covers.push(StagedCover {
                                    entry: *entry,
                                    stem,
                                    path,
                                    info,
                                    thumbs: Vec::new(),
                                });
                            }
                        }
                        Err(e) => job.warn(format!("cover image not kept: {e}")),
                    }
                }
                Err(DownloadError::Cancelled) => {
                    job.finish("cancelled", None);
                    return;
                }
                Err(e) => job.warn(format!("cover image skipped: {e}")),
            },
            TargetKind::Thumb {
                entry,
                tier,
                ext,
                fallback,
            } => {
                if !covers_ok.contains(entry) {
                    job.set(i, |it| it.state = "skipped");
                    continue;
                }
                let mut got = job.fetch(i, &target.url, Want::Thumbnail);
                if matches!(
                    got,
                    Err(DownloadError::NotFound
                        | DownloadError::WrongType(_)
                        | DownloadError::BadImage(_))
                ) {
                    // §2: retry `/Thumbnails/`. The retry URL was part of the consent listing.
                    got = job.fetch(i, fallback, Want::Thumbnail);
                }
                match got {
                    Ok(d) => {
                        let (stem, entry_dir) = job
                            .session
                            .lock()
                            .ok()
                            .and_then(|s| {
                                s.covers
                                    .iter()
                                    .find(|c| c.entry == *entry)
                                    .map(|c| (c.stem.clone(), format!("c{entry}/thumbnails")))
                            })
                            .unwrap_or_else(|| ("cover".into(), format!("c{entry}/thumbnails")));
                        let name = format!("{stem}-{tier}.{ext}");
                        match job.place(d.temp, &entry_dir, &name) {
                            Ok(path) => {
                                if let Ok(mut s) = job.session.lock() {
                                    if let Some(c) = s.covers.iter_mut().find(|c| c.entry == *entry)
                                    {
                                        c.thumbs.push((*tier, ext, path));
                                    }
                                }
                            }
                            Err(e) => job.warn(format!("thumbnail not kept: {e}")),
                        }
                    }
                    Err(DownloadError::Cancelled) => {
                        job.finish("cancelled", None);
                        return;
                    }
                    Err(
                        DownloadError::NotFound
                        | DownloadError::WrongType(_)
                        | DownloadError::BadImage(_),
                    ) => {
                        // Linkhosts MAY omit tiers (§2): not an error.
                        job.set(i, |it| {
                            it.state = "skipped";
                            it.error = None;
                        });
                    }
                    Err(e) => job.warn(format!("thumbnail skipped: {e}")),
                }
            }
        }
    }
    job.finish("done", None);
}

// ---------------------------------------------------------------------------
// Building the release to write (pure planning) and writing it
// ---------------------------------------------------------------------------

/// One planned move from staging into the library.
pub(super) struct Move {
    pub(super) src: PathBuf,
    /// Library-root-relative target.
    pub(super) dst: PathBuf,
}

/// Everything `write_release` needs.
pub(super) struct Built {
    pub(super) doc: NoudataDocument,
    /// Library-relative `.noudata` path.
    pub(super) noudata: PathBuf,
    pub(super) media: Vec<Move>,
    pub(super) images: Vec<Move>,
}

fn lower(names: &[String]) -> HashSet<String> {
    names.iter().map(|n| n.to_lowercase()).collect()
}

fn unique_in(taken: &mut HashSet<String>, name: &str) -> String {
    if taken.insert(name.to_lowercase()) {
        return name.to_string();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    for n in 2..10_000 {
        let c = format!("{stem} ({n}){ext}");
        if taken.insert(c.to_lowercase()) {
            return c;
        }
    }
    let c = format!("{stem} (dup){ext}");
    taken.insert(c.to_lowercase());
    c
}

/// Case A/B: `build_local_doc` (NB_07) on the confirmed mapping, plus images and the source URL.
fn build_case_ab(s: &Session, key: Option<&str>) -> Result<Built, UniteError> {
    let release = s
        .release
        .as_ref()
        .ok_or_else(|| UniteError::NotReady("no release metadata for this session".into()))?;
    let confirmed = s
        .confirmed
        .as_ref()
        .ok_or_else(|| UniteError::NotReady("confirm the assignment first".into()))?;
    let fs = LibraryFs::new(&s.lib_root)?;
    let placement = Placement::for_release(&release.doc, release.doc.profile());
    let existing = fs.existing_names(&placement.dir);
    let placement = placement.with_existing_names(existing.clone());
    let plan =
        build_local_doc(&release.doc, &confirmed.mapping, key, &placement).map_err(
            |e| match e {
                MatchError::Incomplete { missing_items, .. } => UniteError::Incomplete {
                    missing_items,
                    leftover_files: Vec::new(),
                },
                other => UniteError::Invalid(other.to_string()),
            },
        )?;
    let dir = plan
        .noudata_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut taken = lower(&existing);
    taken.insert(
        plan.noudata_path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default(),
    );
    let mut media = Vec::new();
    for op in &plan.file_ops {
        if let Some(n) = op.target.file_name() {
            taken.insert(n.to_string_lossy().to_lowercase());
        }
        media.push(Move {
            src: op.source.clone(),
            dst: op.target.clone(),
        });
    }
    let mut doc = plan.doc;
    // Where the metadata came from: lets "refresh" find it again.
    if let Some(u) = sanitise_url(&release.source_url) {
        doc.extra.insert(SOURCE_URL_KEY.into(), Value::String(u));
    }
    let uploads: Vec<&StagedFile> = s
        .files
        .iter()
        .filter(|f| matches!(f.kind, FileKind::Image(_)) && !f.thumb)
        .collect();
    let images = plan_images(&mut doc, &dir, &mut taken, &s.covers, &uploads, &fs);
    Ok(Built {
        doc,
        noudata: plan.noudata_path,
        media,
        images,
    })
}

/// Give downloaded and uploaded art local names, set `imageLocalFilename`, and keep the
/// higher-resolution image primary with the other as `<name>-alt.<ext>` (§4.8 / NB_09).
fn plan_images(
    doc: &mut NoudataDocument,
    dir: &Path,
    taken: &mut HashSet<String>,
    covers: &[StagedCover],
    uploads: &[&StagedFile],
    fs: &LibraryFs,
) -> Vec<Move> {
    let mut moves = Vec::new();
    let thumbs_dir = dir.join("thumbnails");
    let mut thumb_taken = lower(&fs.existing_names(&thumbs_dir));
    let rd = doc.release_data.get_or_insert_with(Default::default);
    let mut entries: Vec<ImageEntry> = rd
        .cover_image
        .as_ref()
        .map(|c| c.images.clone())
        .unwrap_or_default();
    let mut add = |src: &Path, name: &str, taken: &mut HashSet<String>| -> String {
        let n = unique_in(taken, &safe_path_segment(name, "cover", MAX_SEGMENT_BYTES));
        moves.push(Move {
            src: src.to_path_buf(),
            dst: dir.join(&n),
        });
        n
    };
    let stem_of = |name: &str| -> String {
        safe_path_segment(name.rsplit_once('.').map_or(name, |x| x.0), "cover", 100)
    };
    // Largest uploaded image; other uploads are left out.
    let upload = uploads
        .iter()
        .filter_map(|f| match f.kind {
            FileKind::Image(i) => Some((*f, i)),
            _ => None,
        })
        .max_by_key(|(_, i)| i.pixels());
    // cover entry -> local name its thumbnails follow
    let mut thumb_owner: HashMap<usize, String> = HashMap::new();
    for c in covers.iter().filter(|c| c.entry != 0) {
        let n = add(&c.path, &format!("{}.{}", c.stem, c.info.kind.ext()), taken);
        if let Some(e) = entries.get_mut(c.entry) {
            e.image_local_filename = Some(n.clone());
        }
        thumb_owner.insert(c.entry, n);
    }
    let mut prefix: Vec<ImageEntry> = Vec::new();
    let mut after_first: Vec<ImageEntry> = Vec::new();
    let primary_dl = covers.iter().find(|c| c.entry == 0 && !entries.is_empty());
    match (primary_dl, upload) {
        (Some(c), Some((u, ui))) if ui.pixels() > c.info.pixels() => {
            // Purchased art is bigger: primary. The web cover is kept as the alt.
            let p = add(
                &u.path,
                &format!("{}.{}", stem_of(&u.name), ui.kind.ext()),
                taken,
            );
            let alt = add(
                &c.path,
                &format!("{}-alt.{}", c.stem, c.info.kind.ext()),
                taken,
            );
            entries[0].image_local_filename = Some(alt.clone());
            thumb_owner.insert(0, alt);
            prefix.push(ImageEntry {
                image_local_filename: Some(p),
                ..Default::default()
            });
        }
        (Some(c), Some((u, ui))) => {
            let p = add(&c.path, &format!("{}.{}", c.stem, c.info.kind.ext()), taken);
            let alt = add(
                &u.path,
                &format!("{}-alt.{}", stem_of(&u.name), ui.kind.ext()),
                taken,
            );
            entries[0].image_local_filename = Some(p.clone());
            thumb_owner.insert(0, p);
            after_first.push(ImageEntry {
                image_local_filename: Some(alt),
                ..Default::default()
            });
        }
        (Some(c), None) => {
            let p = add(&c.path, &format!("{}.{}", c.stem, c.info.kind.ext()), taken);
            entries[0].image_local_filename = Some(p.clone());
            thumb_owner.insert(0, p);
        }
        (None, Some((u, ui))) => {
            let p = add(
                &u.path,
                &format!("{}.{}", stem_of(&u.name), ui.kind.ext()),
                taken,
            );
            prefix.push(ImageEntry {
                image_local_filename: Some(p),
                ..Default::default()
            });
        }
        (None, None) => {}
    }
    // Thumbnails follow the local name of their image (§1.27: `thumbnails/<name>-<tier>.<ext>`).
    for c in covers {
        let Some(local) = thumb_owner.get(&c.entry) else {
            continue;
        };
        let stem = local
            .rsplit_once('.')
            .map_or(local.as_str(), |x| x.0)
            .to_string();
        for (tier, ext, src) in &c.thumbs {
            let name = format!("{stem}-{tier}.{ext}");
            if thumb_taken.insert(name.to_lowercase()) {
                moves.push(Move {
                    src: src.clone(),
                    dst: thumbs_dir.join(name),
                });
            }
        }
    }
    let mut all = prefix;
    let mut rest = entries.into_iter();
    all.extend(rest.next());
    all.extend(after_first);
    all.extend(rest);
    if all.is_empty() {
        rd.cover_image = None;
    } else {
        let prev = rd.cover_image.take();
        rd.cover_image = Some(CoverImage {
            images: all,
            extra: prev.map(|p| p.extra).unwrap_or_default(),
        });
    }
    moves
}

/// Case C: an existing `_<release>.noudata` came with its files. Sanitise it as imported
/// data, point `localFilePaths` at the files that really arrived, bind the key. No matching.
fn build_case_c(s: &Session, nfile: &StagedFile, key: Option<&str>) -> Result<Built, UniteError> {
    let bytes = std::fs::read(&nfile.path).map_err(|e| UniteError::Io(e.to_string()))?;
    check_json_object_limits(&bytes)
        .map_err(|e| UniteError::Invalid(format!("not a usable .noudata ({e})")))?;
    let (mut doc, _warn) = parse_noudata_with_warnings(&bytes)
        .map_err(|_| UniteError::Invalid("the .noudata could not be read".into()))?;
    // Whole document, before anything is written; local data of an imported library is kept.
    sanitise_document(&mut doc, SanitiseOptions::imported());
    // Only a `release` (declared, or inferred from its releaseData) is added this way.
    if plippa_noubin::effective_type(&doc, |_| None) != plippa_noubin::NoudataType::Release
        || doc.release_data.is_none()
    {
        return Err(UniteError::Invalid(
            "only release .noudata files (noudataType release, with releaseData) can be added this way"
                .into(),
        ));
    }
    let fs = LibraryFs::new(&s.lib_root)?;
    let placement = Placement::for_release(&doc, doc.profile());
    let dir = placement.dir.clone();
    let existing = fs.existing_names(&dir);
    let mut taken = lower(&existing);
    let audio: Vec<&StagedFile> = s
        .files
        .iter()
        .filter(|f| matches!(f.kind, FileKind::Audio(_)))
        .collect();
    let mut used: HashSet<u32> = HashSet::new();
    let mut media = Vec::new();
    let mut warnings = 0usize;
    let mut with_files = 0usize;
    for item in doc.media_list.iter_mut() {
        let Some(paths) = item.local_file_paths.take() else {
            continue;
        };
        let mut names = Vec::new();
        for p in paths {
            // Only plain names beside the .noudata (the sanitiser already refused unsafe ones).
            let base = if is_safe_relative_path(&p) && !p.contains('/') {
                Some(p.as_str())
            } else {
                None
            };
            let found = base.and_then(|b| {
                audio
                    .iter()
                    .find(|f| !used.contains(&f.id) && f.name.eq_ignore_ascii_case(b))
            });
            match found {
                Some(f) => {
                    used.insert(f.id);
                    let n = unique_in(
                        &mut taken,
                        &safe_path_segment(&f.name, "track", MAX_SEGMENT_BYTES),
                    );
                    media.push(Move {
                        src: f.path.clone(),
                        dst: dir.join(&n),
                    });
                    names.push(n);
                }
                None => warnings += 1,
            }
        }
        if !names.is_empty() {
            with_files += 1;
            item.local_file_paths = Some(names);
        }
    }
    let _ = warnings;
    if with_files == 0 {
        return Err(UniteError::Invalid(
            "none of the files named in the .noudata were supplied".into(),
        ));
    }
    // Key: only set/replace when given; an imported document may keep its own.
    if key.is_some() {
        doc = associate_existing(doc, key).map_err(|e| UniteError::Invalid(e.to_string()))?;
    }
    // Images: keep only entries whose local file arrived (or that have a web URL).
    let mut images = Vec::new();
    let thumbs_dir = dir.join("thumbnails");
    let mut thumb_taken = lower(&fs.existing_names(&thumbs_dir));
    let uploads: Vec<&StagedFile> = s
        .files
        .iter()
        .filter(|f| matches!(f.kind, FileKind::Image(_)))
        .collect();
    let mut placed: HashMap<String, String> = HashMap::new();
    if let Some(rd) = doc.release_data.as_mut() {
        if let Some(ci) = rd.cover_image.as_mut() {
            let mut keep = Vec::new();
            for mut e in std::mem::take(&mut ci.images) {
                if let Some(local) = e.image_local_filename.take() {
                    let f = uploads
                        .iter()
                        .find(|f| !f.thumb && f.name.eq_ignore_ascii_case(&local));
                    if let Some(f) = f {
                        let n = unique_in(
                            &mut taken,
                            &safe_path_segment(&f.name, "cover", MAX_SEGMENT_BYTES),
                        );
                        images.push(Move {
                            src: f.path.clone(),
                            dst: dir.join(&n),
                        });
                        placed.insert(f.name.to_lowercase(), n.clone());
                        e.image_local_filename = Some(n);
                    }
                }
                if e.image_local_filename.is_some() || e.image_web_url.is_some() {
                    keep.push(e);
                }
            }
            ci.images = keep;
        }
        if rd.cover_image.as_ref().is_some_and(|c| c.images.is_empty()) {
            rd.cover_image = None;
        }
    }
    for f in uploads.iter().filter(|f| f.thumb) {
        // `thumbnails/<name>-<tier>.<ext>`: kept when its image arrived.
        let stem = f.name.rsplit_once('-').map_or("", |x| x.0).to_lowercase();
        let owner = placed
            .iter()
            .find(|(orig, _)| orig.rsplit_once('.').map_or(orig.as_str(), |x| x.0) == stem);
        if let Some((_, local)) = owner {
            let lstem = local.rsplit_once('.').map_or(local.as_str(), |x| x.0);
            let tier_ext = f.name.rsplit_once('-').map_or("", |x| x.1);
            let name =
                safe_path_segment(&format!("{lstem}-{tier_ext}"), "thumb", MAX_SEGMENT_BYTES);
            if thumb_taken.insert(name.to_lowercase()) {
                images.push(Move {
                    src: f.path.clone(),
                    dst: thumbs_dir.join(name),
                });
            }
        }
    }
    let noudata_name = unique_in(
        &mut taken,
        &safe_path_segment(&nfile.name, "release.noudata", MAX_SEGMENT_BYTES),
    );
    Ok(Built {
        doc,
        noudata: dir.join(noudata_name),
        media,
        images,
    })
}

/// One image a store download brings into the library (already staged).
///
/// `entry` is the `releaseData.coverImage.images` position the image belongs to when the
/// document named it (Path A); `None` is an image the document did not have (the store's own
/// cover), which becomes a new entry.
pub(crate) struct LocalArt {
    pub entry: Option<usize>,
    pub path: PathBuf,
    /// Preferred local stem, without extension.
    pub stem: String,
    pub info: ImageInfo,
    /// `(tier, ext, staged path)`.
    pub thumbs: Vec<(u32, &'static str, PathBuf)>,
}

/// Everything the store unite path hands to [`write_store_release`].
pub(crate) struct StoreWrite {
    /// The **local** document (`build_local_doc` ran; `localFilePaths` point at `media`).
    pub doc: NoudataDocument,
    /// Library-relative `.noudata` path.
    pub noudata: PathBuf,
    /// `(staged source, library-relative target)` for every media file.
    pub media: Vec<(PathBuf, PathBuf)>,
    pub art: Vec<LocalArt>,
}

/// Place a store download into the library (BCS_05's Path A and Path B).
///
/// The caller has already run the same planner a unite `apply` uses ([`build_local_doc`]):
/// this function only names the cover art and then writes through [`write_release`], so a
/// downloaded release lands exactly like an uploaded one: `Music/<artist>/<release>/`, the
/// `.noudata` written last, nothing overwritten.
pub(crate) fn write_store_release(
    root: &Path,
    stage: &Path,
    write: StoreWrite,
) -> Result<PathBuf, UniteError> {
    let fs = LibraryFs::new(root)?;
    let StoreWrite {
        mut doc,
        noudata,
        media: media_in,
        art,
    } = write;
    let dir = noudata
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut taken = lower(&fs.existing_names(&dir));
    if let Some(name) = noudata.file_name() {
        taken.insert(name.to_string_lossy().to_lowercase());
    }
    let mut media = Vec::with_capacity(media_in.len());
    for (src, dst) in media_in {
        if let Some(name) = dst.file_name() {
            taken.insert(name.to_string_lossy().to_lowercase());
        }
        media.push(Move { src, dst });
    }
    let images = plan_store_art(&mut doc, &dir, &fs, art, &mut taken);
    write_release(
        &fs,
        stage,
        Built {
            doc,
            noudata,
            media,
            images,
        },
    )
}

/// Name the art of a store download: the largest image is primary, the others are
/// `<name>-alt.<ext>` (NB_09's §4.8 rule), and thumbnails follow their image's local name.
///
/// Existing `coverImage` entries are kept in order (their `imageWebURL`, alt text and extras
/// survive); an image the document did not name becomes a new entry at the front when it is
/// the primary or after the first entry otherwise.
fn plan_store_art(
    doc: &mut NoudataDocument,
    dir: &Path,
    fs: &LibraryFs,
    art: Vec<LocalArt>,
    taken: &mut HashSet<String>,
) -> Vec<Move> {
    if art.is_empty() {
        return Vec::new();
    }
    let thumbs_dir = dir.join("thumbnails");
    let mut thumb_taken = lower(&fs.existing_names(&thumbs_dir));
    let rd = doc.release_data.get_or_insert_with(Default::default);
    let previous = rd.cover_image.take();
    let base: Vec<ImageEntry> = previous
        .as_ref()
        .map(|c| c.images.clone())
        .unwrap_or_default();
    // Largest first; the sort is stable, so an earlier image wins a tie.
    let mut order: Vec<usize> = (0..art.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(art[i].info.pixels()));
    let mut moves = Vec::new();
    let mut entries: Vec<ImageEntry> = Vec::new();
    for (rank, &i) in order.iter().enumerate() {
        let a = &art[i];
        let stem = if rank == 0 {
            a.stem.clone()
        } else {
            format!("{}-alt", a.stem)
        };
        let name = unique_in(
            taken,
            &safe_path_segment(
                &format!("{stem}.{}", a.info.kind.ext()),
                "cover",
                MAX_SEGMENT_BYTES,
            ),
        );
        moves.push(Move {
            src: a.path.clone(),
            dst: dir.join(&name),
        });
        let mut entry = a
            .entry
            .and_then(|e| base.get(e).cloned())
            .unwrap_or_default();
        entry.image_local_filename = Some(name.clone());
        entries.push(entry);
        let thumb_stem = name
            .rsplit_once('.')
            .map_or(name.as_str(), |x| x.0)
            .to_string();
        for (tier, ext, src) in &a.thumbs {
            let thumb = format!("{thumb_stem}-{tier}.{ext}");
            if thumb_taken.insert(thumb.to_lowercase()) {
                moves.push(Move {
                    src: src.clone(),
                    dst: thumbs_dir.join(thumb),
                });
            }
        }
    }
    rd.cover_image = Some(CoverImage {
        images: entries,
        extra: previous.map(|p| p.extra).unwrap_or_default(),
    });
    moves
}

/// Write the built release: folder, media, images, then the `.noudata` last. All-or-nothing.
pub(super) fn write_release(
    fs: &LibraryFs,
    stage: &Path,
    built: Built,
) -> Result<PathBuf, UniteError> {
    let Built {
        mut doc,
        noudata,
        media,
        images,
    } = built;
    // Whole-document sanitise once more right before the write (belt and braces: the plan
    // reshaped names and images after the fetch-time pass).
    sanitise_document(&mut doc, SanitiseOptions::imported());
    let dir = noudata.parent().map(Path::to_path_buf).unwrap_or_default();
    // Fail before touching anything if a target is unsafe or already taken.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for m in media.iter().chain(images.iter()) {
        fs.check_staged(stage, &m.src)?;
        let abs = fs.resolve(&m.dst)?;
        if std::fs::symlink_metadata(&abs).is_ok() || !seen.insert(abs) {
            return Err(UniteError::Io(format!(
                "{} already exists",
                m.dst.display()
            )));
        }
    }
    let noudata_abs = fs.resolve(&noudata)?;
    if std::fs::symlink_metadata(&noudata_abs).is_ok() {
        return Err(UniteError::Io(format!(
            "{} already exists",
            noudata.display()
        )));
    }
    let json = doc
        .to_json_pretty()
        .map_err(|e| UniteError::Io(e.to_string()))?;
    let mut tx = Transaction::default();
    let result = (|| -> Result<(), UniteError> {
        fs.ensure_dir(&dir, tx.dirs())?;
        for m in &media {
            let dst = fs.resolve(&m.dst)?;
            tx.move_in(&m.src, &dst)?;
        }
        for m in &images {
            if let Some(parent) = m.dst.parent() {
                fs.ensure_dir(parent, tx.dirs())?;
            }
            let dst = fs.resolve(&m.dst)?;
            tx.move_in(&m.src, &dst)?;
        }
        // Never leave a `.noudata` that points at a missing file.
        let dir_abs = fs.resolve(&dir)?;
        for item in &doc.media_list {
            for name in item.local_file_paths.iter().flatten() {
                match std::fs::symlink_metadata(dir_abs.join(name)) {
                    Ok(m) if m.file_type().is_file() => {}
                    _ => {
                        return Err(UniteError::Io(format!(
                            "{name} is missing after the move; nothing was written"
                        )))
                    }
                }
            }
        }
        tx.write_new(&noudata_abs, json.as_bytes())?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            tx.commit();
            Ok(noudata_abs)
        }
        Err(e) => {
            tx.rollback();
            Err(e)
        }
    }
}
