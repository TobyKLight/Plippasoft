//! Library scan, `.noudata` write-through, play/associate orchestration.

use crate::ipc::IpcBus;
use crate::tags::extract_tags_from_path;
use crate::unite::fsops::LibraryFs;
use crate::unite::UniteHub;
use crate::web::download::{DownloadLimits, StagingBudget};
use crate::web::{WebConfig, WebHub, WebStatus};
use plippa_core::{AudioFormatSupport, Capabilities, PlayableItemId, TransportState};
use plippa_ipc_proto::{
    AssociateRequest, AssociateResponse, IpcMessage, LibraryIndexUpdated, NoticeSeverity,
    PlaybackCommand, UserNotice, NOTICE_NO_MEDIA_STORAGE,
};
use plippa_noubin::{
    decode_playable_id, display_path_with_library, safe_path_segment, sanitise_title, MAX_SEGMENT_BYTES, encode_playable_id, generate_local_noubin_key,
    import_placement, library_namespace, library_root_available, normalise_noubin_key,
    parse_noudata, resolve_library_roots, sanitize_filename, DiscoveredLibrary, ImportKind,
    LibraryError, LibraryRoot, LibraryWriter, LocalKeyParams, MediaListItem, MountIdentity,
    NoudataDocument,
    PlayableRecord, ProbeReport, ScanReport,
    Contributor, Credits, NoudataType, PrimaryArtist, ReleaseData, NOUDATA_VERSION,
    WHAT_AM_I_LOOKING_AT,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;

mod art_staging;
mod associate;
mod collections;
mod covers;
mod cues;
mod delete;
mod imports;
mod playback;
mod refresh;
mod resume;
pub mod stores;
mod waveform_status;
pub use delete::{
    DeleteBlocker, DeleteError, DeleteFile, DeleteOutcome, DeletePreview, DeleteRequest,
    DeleteTarget, PlaylistHit,
};
pub use associate::{
    AssociateBound, AssociateItemRef, AssociateOutcome, AssociateTarget, AssociateWarning,
    WARN_DUPLICATE_KEY,
};
pub use cues::{
    preview_cues, CueListOut, CueOut, CuePreview, CuePreviewRequest, CueSaveReport, ItemCues,
    SaveCuesRequest, MAX_CUE_PASTE_BYTES,
};
pub use imports::{stream_audio_upload, ImportError, ImportFinish, ImportHub};
pub use playback::{
    play_collection_unlocked, ListEntryOut, NowPlayingListOut, PlayCollection, PlayCollectionOut,
};
pub use collections::{
    AddPlaylistItems, CollectionDetail, CollectionItem, CollectionSummary, TrackSummary, CreatePlaylist,
    IdArg, KindCount, RenamePlaylist, ReorderPlaylist, TrackRef,
};
pub use covers::{
    cover_limits, ensure_thumbnails, spawn_cover_worker, stage_cover, CoverList, CoverOut,
    CoverPlan, CoverScope, CoverSize, CoverStore, OrderEntry, PutCovers, StageCtx, ThumbReport,
    MAX_COVERS, MAX_COVER_BYTES,
};
pub(crate) use stores::StoreApiError;
use stores::{StoreHub, StoresSummary, SyncState};
pub use waveform_status::{
    request_reanalysis, validate_kinds, ReanalyseRefusal, WaveformItemStatus,
    WaveformStatus,
};

/// How NFC associate is performed for tests / lab hooks.
pub type AssociateNfcFn = Box<dyn FnMut(&AssociateRequest) -> AssociateResponse + Send>;

struct OpenLibrary {
    root: PathBuf,
    label: String,
    writer: LibraryWriter,
    /// Filesystem identity captured when the connection was opened. A different identity
    /// means the stick was remounted and the SQLite handle must be reopened.
    mount: Option<MountIdentity>,
}

/// What the background scanner should do this tick (see [`ServerService::scan_plan`]).
#[derive(Debug, Clone)]
pub struct ScanPlan {
    pub roots: Vec<PathBuf>,
    /// Engine is playing: skip heavy work (duration probing) this tick.
    pub playing: bool,
    /// WAVE_01: the loaded track's `.wfm` sidecar, if the engine published one. The waveform
    /// worker analyses this before anything else and abandons its current file when it changes.
    pub current_waveform: Option<PathBuf>,
}

/// Media-management service (HTTP handlers call into this).
pub struct ServerService {
    /// When set, only this root is used (CLI / `PLIPPA_LIBRARY_ROOT`). Discovery skipped.
    override_root: Option<PathBuf>,
    /// When set (tests / harness), use these roots and skip `/media` discovery.
    pinned_roots: Option<Vec<PathBuf>>,
    libraries: Vec<OpenLibrary>,
    /// Shared so associate wait can run without holding the service mutex.
    ipc: Arc<Mutex<Box<dyn IpcBus>>>,
    generation: u64,
    warning: Option<String>,
    /// The last "no media storage" notice sent: the same one is not sent again on every poll
    /// (one notice per episode); cleared when a library opens.
    no_storage_notified: Option<String>,
    associate_nfc: AssociateNfcFn,
    next_associate_id: AtomicU64,
    /// NB_08: consent-gated web lookup state (shared so a fetch never holds the service lock).
    web: Arc<WebHub>,
    /// NB_09: unite sessions (uploads, downloads, apply, refresh).
    unite: Arc<UniteHub>,
    imports: Arc<ImportHub>,
    /// WAVE_01: waveform analysis state, shared with the worker thread.
    waveforms: Arc<crate::waveform::WaveformRegistry>,
    /// ART_03: staged cover uploads, header-probe cache and thumbnail backfill state.
    covers: Arc<CoverStore>,
    /// BCS_03: Bandcamp connection state (shared with the automatic-sync worker).
    stores: Arc<StoreHub>,
    /// PWRSW_01: set by [`Self::quiesce`]; every mutating entry point refuses new work.
    quiescing: bool,
    /// MEDIA_01: what this board can decode. The import gate, the store/unite downloads and
    /// the waveform analyser all read this one value.
    capabilities: Arc<Capabilities>,
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("{0}")]
    Message(String),
    /// More than the format or the portal allows (HTTP 413).
    #[error("{0}")]
    TooLarge(String),
    /// The addressed playable / item / file does not exist (HTTP 404).
    #[error("{0}")]
    NotFound(String),
    /// The request is well-formed JSON but not acceptable (HTTP 400).
    #[error("{0}")]
    Invalid(String),
    /// The `.noudata` changed since the client read it (HTTP 409).
    #[error("{0}")]
    Conflict(String),
    /// No media storage mounted / it vanished (HTTP 503).
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Library(#[from] LibraryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Noudata(#[from] plippa_noubin::NoudataError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    KeyGen(#[from] plippa_noubin::KeyGenError),
}

impl ServerError {
    /// HTTP status the portal answers with (shared with `http::service_error`).
    pub fn http_status(&self) -> u16 {
        match self {
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 409,
            Self::TooLarge(_) => 413,
            Self::Unavailable(_) => 503,
            Self::Io(_) | Self::Library(_) => 500,
            Self::Message(_)
            | Self::Invalid(_)
            | Self::Noudata(_)
            | Self::Json(_)
            | Self::KeyGen(_) => 400,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryRootInfo {
    pub path: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryStatus {
    /// First open root (compat) or `None`.
    pub library_root: Option<String>,
    /// All open roots + volume labels (MVP_17).
    pub library_roots: Vec<LibraryRootInfo>,
    pub available: bool,
    pub warning: Option<String>,
    pub item_count: u64,
    pub generation: u64,
    pub journal_mode: Option<String>,
    pub ipc_live: bool,
    pub now_playing_transport: Option<String>,
    /// Namespaced id as decimal string (JS-safe; may exceed Number.MAX_SAFE_INTEGER).
    #[serde(serialize_with = "serialize_opt_u64_as_string")]
    pub now_playing_id: Option<u64>,
    pub last_notice: Option<String>,
    /// Index warnings across open libraries (duplicate keys, media claimed twice).
    #[serde(default)]
    pub index_warnings: Vec<String>,
    /// NB_08: can this player look a URL Noubin up on the web (config + network)?
    #[serde(default)]
    pub web_capable: bool,
    /// NB_08: why not, when `web_capable` is false.
    #[serde(default)]
    pub web_reason: Option<String>,
    /// NB_08: latest URL tap with no local match (the portal offers the consent-gated lookup).
    #[serde(default)]
    pub unmatched_noubin: Option<UnmatchedNoubinInfo>,
    /// BCS_03: whether the store feature can be used, and why not.
    #[serde(default)]
    pub stores: StoresSummary,
    /// MEDIA_01: the board this player runs as (`desktop-mock` when none is configured).
    #[serde(default)]
    pub board_id: String,
    /// MEDIA_01: which audio formats this player decodes. The portal reads it before an upload
    /// so it can say why a file will not be added.
    #[serde(default)]
    pub audio_formats: AudioFormatSupport,
}

/// Portal view of an [`plippa_ipc_proto::UnmatchedNoubin`] (key sanitised for display).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnmatchedNoubinInfo {
    pub noubin_key: String,
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NowPlaying {
    pub ipc_live: bool,
    pub transport: String,
    #[serde(serialize_with = "serialize_opt_u64_as_string")]
    pub playable_id: Option<u64>,
    pub volume_percent: u8,
    pub last_notice: Option<String>,
    /// Track title of the loaded entry (as the engine shows it).
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    /// NB_10: title of the loaded collection (album / playlist / …), from the index.
    #[serde(default)]
    pub collection_title: Option<String>,
    /// `single|album|playlist|mix|audiobook|podcast` once the engine published a list.
    #[serde(default)]
    pub list_kind: Option<String>,
    /// Counts list publications; refetch `/api/now-playing/list` when it changes.
    #[serde(default)]
    pub list_seq: u64,
    /// Ordinal of the current track among the list's tracks, and how many there are.
    #[serde(default)]
    pub track_index: Option<u32>,
    #[serde(default)]
    pub track_count: u32,
    /// Title of the playing cue (long-form), if any.
    #[serde(default)]
    pub cue_title: Option<String>,
    #[serde(default)]
    pub position_ms: u64,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// From the engine (NB_14): Previous = something loaded (NB_04b); Next = a cue/track follows
    /// the playhead, or `repeatList`.
    #[serde(default)]
    pub can_prev: bool,
    #[serde(default)]
    pub can_next: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayableSummary {
    /// Namespaced playable id as decimal string in JSON (JS cannot safely use u64 numbers).
    #[serde(serialize_with = "serialize_u64_as_string")]
    pub id: u64,
    pub title: Option<String>,
    pub associated: bool,
    pub noubin_key_normalised: Option<String>,
    pub noudata_relpath: String,
    pub media_relpaths: Vec<String>,
    pub paths_summary: String,
    /// Library kind / classification (`track`, `album`, `playlist`, `mix`, `audiobook`,
    /// `podcast`). A tagless mix is a loose *track*: `kind` says `track` (see `group`).
    #[serde(default)]
    pub kind: String,
    /// Library tab that lists it (`kind`, except a loose mix/audiobook/podcast track is
    /// grouped under its profile's tab). `kind` keeps the classification (`track`).
    #[serde(default)]
    pub group: String,
    /// Effective `noudataType` (`release`, `looseTrack`, `linkTrack`, `userPlaylist`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noudata_type: Option<String>,
    /// Volume label when multiple libraries are open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayableDetail {
    pub summary: PlayableSummary,
    pub playable_item_title: Option<String>,
    pub noubin_key: Option<String>,
    pub noubin_key_normalised: Option<String>,
    pub media_titles: Vec<Option<String>>,
    /// Effective `noudataType` (`release`, `userPlaylist`, `looseTrack`, `linkTrack`); a
    /// media file with no `.noudata` yet is shown as `looseTrack` (orphan).
    pub noudata_type: String,
    /// Release-level artist (own or, for a link track, none) for the Details panel.
    pub primary_artist: Option<String>,
}

/// Result of [`ServerService::add_media_with_cues`]: the summary (flattened, so the JSON is
/// the old `POST /api/media` shape) plus a `cues` report when `cue_text` was given.
#[derive(Debug, Clone, Serialize)]
pub struct AddMediaOutcome {
    #[serde(flatten)]
    pub summary: PlayableSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cues: Option<CueSaveReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetadataPatch {
    #[serde(rename = "playableItemTitle", default)]
    pub playable_item_title: Option<String>,
    /// Release-level artist. Setting it on a `looseTrack` upgrades the file to a `release`
    /// (minimum `releaseData`, `noudataType` `release`) in place; refused for a `userPlaylist`
    /// or `linkTrack` (the artist belongs to the release).
    #[serde(rename = "primaryArtist", default)]
    pub primary_artist: Option<String>,
}

/// In-flight associate state between [`ServerService::begin_associate`] and finish.
pub struct AssociatePrep {
    pub req: AssociateRequest,
    /// Where the key will be written (NB_06: may be a per-track file created on success).
    pub target: AssociateTarget,
    /// The target's document before the associate (default for a not-yet-created file).
    pub doc_before: NoudataDocument,
    pub fallback_key: String,
}

impl ServerService {
    /// Open libraries (override or USB discovery), initial scan, notify engine.
    pub fn start(
        library_root: Option<PathBuf>,
        ipc: Box<dyn IpcBus>,
        associate_nfc: AssociateNfcFn,
    ) -> Self {
        Self::start_with_stores(library_root, ipc, associate_nfc, None)
    }

    /// [`Self::start`] with the store secrets directory (`--secrets-dir`).
    ///
    /// Without a directory the store feature reports itself unavailable and everything
    /// else behaves exactly as before.
    pub fn start_with_stores(
        library_root: Option<PathBuf>,
        ipc: Box<dyn IpcBus>,
        associate_nfc: AssociateNfcFn,
        secrets_dir: Option<PathBuf>,
    ) -> Self {
        let stores = Arc::new(StoreHub::production(secrets_dir));
        stores.set_library_root(library_root.clone());
        let (unite, imports) = new_hubs();
        let mut svc = Self {
            override_root: library_root,
            pinned_roots: None,
            libraries: Vec::new(),
            ipc: Arc::new(Mutex::new(ipc)),
            generation: 0,
            warning: None,
            no_storage_notified: None,
            associate_nfc,
            next_associate_id: AtomicU64::new(1),
            web: Arc::new(WebHub::new(WebConfig::from_env())),
            unite,
            imports,
            waveforms: Arc::new(crate::waveform::WaveformRegistry::new()),
            covers: Arc::new(CoverStore::new()),
            stores,
            quiescing: false,
            capabilities: Arc::new(Capabilities::desktop_mock()),
        };
        svc.detect_and_open();
        svc.sweep_cover_staging();
        if !svc.libraries.is_empty() {
            let _ = svc.full_scan();
        }
        svc
    }

    /// Open an explicit list of library roots (tests / multi-USB harness).
    ///
    /// Skips `/media` discovery. First root is the Add primary.
    pub fn start_with_roots(
        roots: Vec<PathBuf>,
        ipc: Box<dyn IpcBus>,
        associate_nfc: AssociateNfcFn,
    ) -> Self {
        let stores_root = roots.first().cloned();
        let (unite, imports) = new_hubs();
        let mut svc = Self {
            override_root: None,
            pinned_roots: Some(roots),
            libraries: Vec::new(),
            ipc: Arc::new(Mutex::new(ipc)),
            generation: 0,
            warning: None,
            no_storage_notified: None,
            associate_nfc,
            next_associate_id: AtomicU64::new(1),
            web: Arc::new(WebHub::new(WebConfig::from_env())),
            unite,
            imports,
            waveforms: Arc::new(crate::waveform::WaveformRegistry::new()),
            covers: Arc::new(CoverStore::new()),
            // Tests pin roots: no secrets directory unless a test installs a hub.
            stores: Arc::new(StoreHub::production(None)),
            quiescing: false,
            capabilities: Arc::new(Capabilities::desktop_mock()),
        };
        svc.stores.set_library_root(stores_root);
        svc.detect_and_open();
        svc.sweep_cover_staging();
        if !svc.libraries.is_empty() {
            let _ = svc.full_scan();
        }
        svc
    }

    fn candidate_roots(&self) -> Vec<DiscoveredLibrary> {
        if let Some(pinned) = &self.pinned_roots {
            return pinned
                .iter()
                .filter(|p| p.is_dir())
                .map(|p| DiscoveredLibrary {
                    label: plippa_noubin::library_display_label(p),
                    path: p.clone(),
                })
                .collect();
        }
        resolve_library_roots(self.override_root.as_deref())
    }

    fn with_ipc<R>(&self, f: impl FnOnce(&mut dyn IpcBus) -> R) -> R {
        let mut guard = self.ipc.lock().expect("ipc");
        f(guard.as_mut())
    }

    pub fn ipc_handle(&self) -> Arc<Mutex<Box<dyn IpcBus>>> {
        Arc::clone(&self.ipc)
    }

    /// PWRSW_01: true once [`Self::quiesce`] started; HTTP answers 503 from then on.
    pub fn quiescing(&self) -> bool {
        self.quiescing
    }

    /// PWRSW_01: make the media volume safe to remove.
    ///
    /// Refuses new work, drops unite / import staging sessions (their `Drop` cancels
    /// downloads and deletes staging), checkpoints each library's WAL and closes every
    /// SQLite connection. Called from `main` after the HTTP server has stopped, so the
    /// engine can keep its read-only connection open: SQLite handles the checkpoint.
    pub fn quiesce(&mut self) {
        if self.quiescing {
            return;
        }
        self.quiescing = true;
        // BCS_03: a sync writing into the library must stop before the WAL checkpoint.
        let sync = self.stores.cancel_sync();
        if sync.state == SyncState::Running {
            eprintln!("plippa-server: quiesce — collection sync cancelled");
        }
        let unite = self.unite.cancel_all();
        let imports = self.imports.cancel_all();
        self.covers.discard_all();
        if unite + imports > 0 {
            eprintln!(
                "plippa-server: quiesce — dropped {unite} unite / {imports} import session(s)"
            );
        }
        let roots: Vec<PathBuf> = self.libraries.iter().map(|l| l.root.clone()).collect();
        for lib in &self.libraries {
            if let Err(e) = lib.writer.checkpoint_truncate() {
                eprintln!(
                    "plippa-server: WAL checkpoint before shutdown failed for {}: {e}",
                    lib.root.display()
                );
            }
        }
        // Dropping the writers closes their connections; nothing remains to write the WAL.
        self.libraries.clear();
        eprintln!(
            "plippa-server: quiesced — {} library root(s) checkpointed and closed",
            roots.len()
        );
    }

    pub fn default_mock_associate() -> AssociateNfcFn {
        Box::new(|req: &AssociateRequest| {
            if req.keep_existing_key {
                // Lab mock: pretend the tag already holds this key.
                let key = "existing.noubin.TAGKEY".to_string();
                AssociateResponse {
                    request_id: req.request_id,
                    ok: true,
                    noubin_key: Some(key.clone()),
                    noubin_key_normalised: Some(normalise_noubin_key(&key)),
                    error: None,
                    tag_locked: true,
                    ..Default::default()
                }
            } else {
                AssociateResponse {
                    request_id: req.request_id,
                    ok: true,
                    noubin_key: Some(req.noubin_key.clone()),
                    noubin_key_normalised: Some(normalise_noubin_key(&req.noubin_key)),
                    error: None,
                    ..Default::default()
                }
            }
        })
    }

    /// Mock NFC that reports a blank normalised key (keep-existing should fail).
    pub fn blank_key_mock_associate() -> AssociateNfcFn {
        Box::new(|req: &AssociateRequest| AssociateResponse {
            request_id: req.request_id,
            ok: true,
            noubin_key: Some("   ".into()),
            noubin_key_normalised: Some(String::new()),
            error: None,
            ..Default::default()
        })
    }

    pub fn failing_mock_associate(message: impl Into<String>) -> AssociateNfcFn {
        let message = message.into();
        Box::new(move |req: &AssociateRequest| AssociateResponse {
            request_id: req.request_id,
            ok: false,
            noubin_key: None,
            noubin_key_normalised: None,
            error: Some(message.clone()),
            ..Default::default()
        })
    }

    fn detect_and_open(&mut self) {
        if self.quiescing {
            return;
        }
        let discovered = self.candidate_roots();
        if discovered.is_empty() {
            self.libraries.clear();
            let msg = if self.override_root.is_some() || self.pinned_roots.is_some() {
                "No media storage found at configured library root"
            } else {
                "No media library root configured (plug a USB stick or set PLIPPA_LIBRARY_ROOT)"
            };
            self.set_no_storage_warning(msg);
            return;
        }

        let mut opened = Vec::new();
        let mut errors = Vec::new();
        for d in discovered {
            match open_one(&d) {
                Ok(lib) => opened.push(lib),
                Err(e) => errors.push(format!("{}: {e}", d.path.display())),
            }
        }

        if opened.is_empty() {
            self.libraries.clear();
            self.set_no_storage_warning(format!(
                "Could not open library index: {}",
                errors.first().cloned().unwrap_or_else(|| "unknown".into())
            ));
            return;
        }

        self.libraries = opened;
        self.warning = if errors.is_empty() {
            None
        } else {
            Some(format!("Some libraries failed to open: {}", errors.join("; ")))
        };
    }

    /// Re-discover mounts; open new sticks, drop vanished ones (no process abort).
    fn refresh_libraries(&mut self, scan_new: bool) {
        // After quiesce the index connections are closed on purpose: never reopen them.
        if self.quiescing {
            return;
        }
        let discovered = self.candidate_roots();
        if discovered.is_empty() {
            if !self.libraries.is_empty() {
                self.libraries.clear();
                self.set_no_storage_warning(
                    "Media storage vanished — no library roots available",
                );
                // The engine/UI hold readers on the same (now gone) index; tell them to
                // close them instead of leaving them stuck on a dead connection.
                self.bump_generation_and_notify(true);
            }
            return;
        }

        let want: Vec<(PathBuf, String, Option<MountIdentity>)> = discovered
            .into_iter()
            .map(|d| (d.path, d.label))
            .map(|(path, label)| {
                let mount = MountIdentity::of(&path);
                (path, label, mount)
            })
            .collect();

        // Drop vanished roots and remounted ones. A stick re-inserted at the same path is a
        // different filesystem: the SQLite handle opened before the unplug refers to the old
        // mount, answers reads from stale cache and fails writes with `database disk image
        // is malformed` forever. `library_is_current` sees the changed identity and drops it,
        // so the loop below opens a fresh connection on the new mount.
        let before = self.libraries.len();
        self.libraries.retain(|lib| {
            want.iter().any(|(p, _, _)| paths_same(p, &lib.root)) && library_is_current(lib)
        });
        let dropped = self.libraries.len() != before;

        // Open newly appeared (and freshly remounted) roots.
        let mut added = false;
        let mut open_failed = false;
        let mut added_needs_scan = false;
        for (path, label, _) in &want {
            if self.libraries.iter().any(|l| paths_same(&l.root, path)) {
                continue;
            }
            if !path.exists() {
                continue;
            }
            match open_one(&DiscoveredLibrary {
                path: path.clone(),
                label: label.clone(),
            }) {
                Ok(lib) => {
                    // A stick that was merely unplugged and re-inserted carries its
                    // populated index; only a brand-new filesystem needs an immediate walk.
                    if scan_new && matches!(lib.writer.count_playables(), Ok(0)) {
                        added_needs_scan = true;
                    }
                    self.libraries.push(lib);
                    added = true;
                }
                Err(e) => {
                    open_failed = true;
                    self.warning = Some(format!("Could not open {}: {e}", path.display()));
                }
            }
        }

        if self.libraries.is_empty() {
            self.set_no_storage_warning("No media storage available");
        } else if !open_failed {
            self.no_storage_notified = None;
            // The library is open, so whatever was complained about before no longer
            // applies — including the boot-time "No media library root configured", which a
            // stick plugged in after start-up used to leave on screen forever (the old guard
            // only cleared warnings containing "vanished", which that message does not).
            self.warning = None;
        }

        // The background scanner passes `false`: a newly opened index is filled by its own
        // (low-priority) lazy scan instead of blocking the service lock here. Hot-plug polls
        // pass `true` when idle. A stick that was merely unplugged and re-inserted already
        // carries its populated index, so a flapping connection does not trigger a full
        // rescan on every remount; only a brand-new (row-less) filesystem is walked now.
        let roots_changed = added || dropped;
        let generation_before = self.generation;
        if added_needs_scan {
            let _ = self.full_scan();
        }
        // Root additions / removals / remounts must reach the engine and UI even when the
        // index content itself did not change: they hold readers opened on the previous
        // mount, and a hot-swapped library would otherwise stay invisible.
        if roots_changed && self.generation == generation_before {
            self.bump_generation_and_notify(true);
        }
    }

    /// Cheap hot-plug poll (mount watcher, ~1 Hz): re-discover roots and open/close/reopen
    /// index connections when the mounted set or a filesystem identity changed.
    ///
    /// Scans a newly appeared library immediately when nothing is playing; during playback
    /// the low-priority background scanner fills it instead, so hot-plugging cannot make the
    /// audio thread compete with a directory walk.
    pub fn poll_mounts(&mut self) {
        let playing = self
            .with_ipc(|ipc| ipc.mirror())
            .playback
            .as_ref()
            .is_some_and(|p| p.transport == TransportState::Playing);
        self.refresh_libraries(!playing);
    }

    fn set_no_storage_warning(&mut self, message: impl Into<String>) {
        let message = message.into();
        self.warning = Some(message.clone());
        if self.no_storage_notified.as_deref() == Some(message.as_str()) {
            return;
        }
        self.no_storage_notified = Some(message.clone());
        self.with_ipc(|ipc| {
            ipc.send(IpcMessage::UserNotice(UserNotice {
                severity: NoticeSeverity::Warning,
                code: NOTICE_NO_MEDIA_STORAGE.into(),
                message,
            }));
        });
    }

    pub fn status(&mut self) -> LibraryStatus {
        self.with_ipc(|ipc| ipc.poll_inbound());
        self.refresh_libraries(false);

        // Drop any root that disappeared (or was remounted) mid-flight.
        let before = self.libraries.len();
        self.libraries.retain(library_is_current);
        if before > 0 && self.libraries.is_empty() {
            self.set_no_storage_warning("Media storage vanished");
        }

        let item_count = self
            .libraries
            .iter()
            .map(|l| l.writer.count_playables().unwrap_or(0))
            .sum();
        let journal_mode = self
            .libraries
            .first()
            .and_then(|l| l.writer.journal_mode().ok());

        let mirror = self.with_ipc(|ipc| ipc.mirror());
        let web: WebStatus = self.web.status();
        // A USB library can appear after start-up; the cache follows the first root.
        self.stores
            .set_library_root(self.libraries.first().map(|l| l.root.clone()));
        let stores = self.stores.summary();
        let (transport, now_id) = mirror
            .playback
            .as_ref()
            .map(|p| {
                (
                    Some(transport_label(p.transport).to_string()),
                    p.session.current_item.map(|i| i.as_u64()),
                )
            })
            .unwrap_or((None, None));

        let roots: Vec<LibraryRootInfo> = self
            .libraries
            .iter()
            .map(|l| LibraryRootInfo {
                path: l.root.display().to_string(),
                label: l.label.clone(),
            })
            .collect();

        LibraryStatus {
            library_root: roots.first().map(|r| r.path.clone()),
            library_roots: roots,
            available: !self.libraries.is_empty(),
            warning: self.warning.clone(),
            item_count,
            generation: self.generation,
            journal_mode,
            ipc_live: mirror.live,
            now_playing_transport: transport,
            now_playing_id: now_id,
            last_notice: mirror.last_notice.map(|n| n.message),
            index_warnings: self
                .libraries
                .iter()
                .flat_map(|l| l.writer.library_warnings().unwrap_or_default())
                .collect(),
            web_capable: web.web_capable,
            web_reason: web.reason,
            stores,
            board_id: self.capabilities.board_id.clone(),
            audio_formats: self.capabilities.audio_formats.clone(),
            // Untrusted text from a tag: refused (not altered) unless it is clean.
            unmatched_noubin: mirror.unmatched.and_then(|u| {
                plippa_noubin::sanitise_key(&u.noubin_key).map(|k| UnmatchedNoubinInfo {
                    noubin_key: k,
                    seq: u.seq,
                })
            }),
        }
    }

    /// Apply the board's capabilities (start-up). Import, unite and store downloads refuse
    /// audio outside them, and the waveform analyser skips it. Rebuilds the import and unite
    /// hubs (keeping their limits) because each carries the gate in its limits; call before
    /// serving.
    pub fn set_capabilities(&mut self, capabilities: Capabilities) {
        let caps = Arc::new(capabilities);
        let mut limits = self.imports.limits().clone();
        limits.capabilities = Arc::clone(&caps);
        self.set_download_limits(limits);
        self.stores.set_device_capabilities(Arc::clone(&caps));
        self.capabilities = caps;
    }

    /// Rebuild the import and unite hubs with these limits (start-up, tests with short stall
    /// windows). Both hubs keep drawing from the one staging cap. Sessions are dropped, so call
    /// it before serving.
    pub fn set_download_limits(&mut self, limits: DownloadLimits) {
        let staging = self.imports.staging();
        self.imports = Arc::new(ImportHub::with_limits_and_staging(
            limits.clone(),
            Arc::clone(&staging),
        ));
        self.unite = Arc::new(UniteHub::with_limits_and_staging(limits, staging));
    }

    /// What this board can decode.
    pub fn capabilities(&self) -> Arc<Capabilities> {
        Arc::clone(&self.capabilities)
    }

    /// The shared web-lookup state. Callers clone this `Arc`, drop the service lock, and only
    /// then talk to the network.
    pub fn web(&self) -> Arc<WebHub> {
        Arc::clone(&self.web)
    }

    /// The streamed-upload / multi-import sessions (NB_11).
    pub fn imports(&self) -> Arc<ImportHub> {
        Arc::clone(&self.imports)
    }

    /// The unite sessions (NB_09). Callers clone this `Arc`, drop the service lock, and only
    /// then upload/download; `apply` and refresh take the lock briefly for the rescan.
    pub fn unite(&self) -> Arc<UniteHub> {
        Arc::clone(&self.unite)
    }

    /// Replace the unite hub (tests with small caps).
    pub fn set_unite_hub(&mut self, hub: Arc<UniteHub>) {
        self.unite = hub;
    }

    /// Replace the web hub (start-up configuration, tests with a mock transport).
    pub fn set_web_hub(&mut self, hub: Arc<WebHub>) {
        self.web = hub;
    }

    /// The web hub, so the store hub can fetch with the same consent-gated machinery.
    pub fn web_hub(&self) -> Arc<WebHub> {
        Arc::clone(&self.web)
    }

    /// The Bandcamp connection state (BCS_03). Callers clone this `Arc`, drop the
    /// service lock, and only then talk to the store.
    pub fn stores(&self) -> Arc<StoreHub> {
        Arc::clone(&self.stores)
    }

    /// Replace the store hub (tests with a mock transport or a temp secrets dir).
    pub fn set_stores_hub(&mut self, hub: Arc<StoreHub>) {
        self.stores = hub;
    }

    /// Connect the store hub to this server: one web fetcher, one library index, one
    /// deleter (BCS_04/05).
    ///
    /// A store download needs all three: Path A fetches the canonical `web.noudata`
    /// through the same consent-gated hub as every other online lookup, a placed release
    /// must become a playable item without waiting for the periodic scan, and a replace
    /// deletes through the portal's own delete implementation rather than a second one.
    ///
    /// `main` and the end-to-end test call this same function, so what the test proves is
    /// what the running server assembles — the wiring lives here, not in `main`.
    pub fn wire_store_hub(service: &Arc<Mutex<ServerService>>) {
        let (stores, web) = match service.lock() {
            Ok(service) => (service.stores(), service.web()),
            Err(poisoned) => {
                let service = poisoned.into_inner();
                (service.stores(), service.web())
            }
        };
        stores.set_web_hub(web);
        let weak = Arc::downgrade(service);
        stores.set_library_indexer(Arc::new(move |root, noudata| {
            let service = weak
                .upgrade()
                .ok_or_else(|| "the server is shutting down".to_string())?;
            let mut guard = service
                .lock()
                .map_err(|_| "the server lock is poisoned".to_string())?;
            guard
                .finish_store_download(root, noudata)
                .map(|summary| summary.id)
        }));
        let weak = Arc::downgrade(service);
        stores.set_library_deleter(Arc::new(move |root, rel| {
            let service = weak
                .upgrade()
                .ok_or_else(|| "the server is shutting down".to_string())?;
            let mut guard = service
                .lock()
                .map_err(|_| "the server lock is poisoned".to_string())?;
            guard.delete_library_item(root, rel)
        }));
    }

    /// BCS_04: index one release a store download has just written.
    ///
    /// The store hub calls this (through the indexer it is given at start-up) after the
    /// unite writer placed a release, so the new item is playable without waiting for
    /// the periodic scan. It is the same "rescan, then read the row back" step a unite
    /// `apply` performs.
    /// Index a release the store download manager just wrote (BCS_04 hand-off).
    ///
    /// Returns the summary the reader would see, so the caller can log what landed and a
    /// test can prove the item is in the index rather than only on disk.
    pub fn finish_store_download(
        &mut self,
        root: &std::path::Path,
        noudata: &std::path::Path,
    ) -> Result<PlayableSummary, String> {
        self.finish_unite_write(root, noudata)
            .map_err(|error| error.to_string())
    }

    /// WAVE_01: the registry shared between this service and the waveform worker thread.
    pub fn waveform_registry(&self) -> Arc<crate::waveform::WaveformRegistry> {
        Arc::clone(&self.waveforms)
    }

    /// Does `key` (a raw Noubin Key) match anything in an open library? The web path is only
    /// offered when this is false (§3.2 local first).
    pub fn has_local_match(&self, key: &str) -> bool {
        let normalised = normalise_noubin_key(key);
        self.libraries.iter().any(|l| {
            matches!(
                l.writer.lookup_by_normalised_key(&normalised),
                Ok(plippa_noubin::LookupResult::Found(_))
            )
        })
    }

    pub fn now_playing(&mut self) -> NowPlaying {
        let mirror = self.with_ipc(|ipc| {
            ipc.poll_inbound();
            ipc.mirror()
        });
        let notice = mirror.last_notice.as_ref().map(|n| n.message.clone());
        let Some(p) = mirror.playback else {
            return NowPlaying {
                ipc_live: mirror.live,
                transport: "Stopped".into(),
                playable_id: None,
                volume_percent: 0,
                last_notice: notice,
                title: String::new(),
                artist: String::new(),
                collection_title: None,
                list_kind: None,
                list_seq: mirror.list_seq,
                track_index: None,
                track_count: 0,
                cue_title: None,
                position_ms: 0,
                duration_ms: None,
                can_prev: false,
                can_next: false,
            };
        };
        let playable_id = p.session.current_item.map(|i| i.as_u64());
        // POLISH_02: the engine composes the collection line (it follows the playing item, e.g.
        // "Band Name Bangers (Live In New York)"). A loose track has none from the engine, so
        // the record's own title stays as the fallback.
        let engine_line = Some(p.session.collection_title.clone()).filter(|t| !t.is_empty());
        let collection_title = engine_line.or_else(|| {
            playable_id.and_then(|id| {
                self.require_record(id)
                    .ok()
                    .filter(|(_, r)| r.kind != plippa_noubin::LibraryKind::Track)
                    .and_then(|(_, r)| r.title.filter(|t| !t.is_empty()))
            })
        });
        // The list rows pair loosely with status: only trust indices that are in range.
        let (mut list_kind, mut track_index, mut track_count, mut cue_title, mut entry_title) =
            (None, None, 0u32, None, None);
        if let Some(l) = &mirror.list {
            use plippa_core::NowPlayingEntryKind::Track;
            list_kind = Some(format!("{:?}", l.list_kind).to_lowercase());
            track_count = l.entries.iter().filter(|e| e.kind == Track).count() as u32;
            if let Some(cur) = l.entries.get(l.current_index as usize).filter(|e| e.kind == Track) {
                track_index = Some(cur.track_index);
                entry_title = Some(cur.title.clone());
                if let Some(c) = p.session.current_cue_index {
                    cue_title = l
                        .entries
                        .get(l.current_index as usize + 1 + c as usize)
                        .filter(|e| e.kind != Track)
                        .map(|e| e.title.clone());
                }
            }
        }
        NowPlaying {
            ipc_live: mirror.live,
            transport: transport_label(p.transport).into(),
            playable_id,
            volume_percent: p.session.volume.percent(),
            last_notice: notice,
            title: entry_title.unwrap_or_else(|| p.session.title.clone()),
            artist: p.session.artist.clone(),
            collection_title,
            list_kind,
            list_seq: mirror.list_seq,
            track_index,
            track_count,
            cue_title,
            position_ms: p.session.position_ms,
            duration_ms: p.session.duration_ms,
            // NB_14: engine-computed (`NowPlayingList::can_prev/can_next`).
            can_prev: p.session.can_prev,
            can_next: p.session.can_next,
        }
    }

    pub fn full_scan(&mut self) -> Result<ScanReport, ServerError> {
        let mut total = ScanReport::default();
        let mut i = 0;
        while i < self.libraries.len() {
            match self.libraries[i].writer.full_scan() {
                Ok(r) => {
                    merge_report(&mut total, r);
                    i += 1;
                }
                Err(LibraryError::StorageVanished { path, detail }) => {
                    self.drop_root(&path, &detail);
                }
                Err(e) => return Err(e.into()),
            }
        }
        self.after_scan(&total);
        Ok(total)
    }

    pub fn lazy_rescan(&mut self) -> Result<ScanReport, ServerError> {
        self.refresh_libraries(true);
        let mut total = ScanReport::default();
        let mut i = 0;
        while i < self.libraries.len() {
            match self.libraries[i].writer.lazy_rescan() {
                Ok(r) => {
                    merge_report(&mut total, r);
                    i += 1;
                }
                Err(LibraryError::StorageVanished { path, detail }) => {
                    self.drop_root(&path, &detail);
                }
                Err(e) => return Err(e.into()),
            }
        }
        self.after_scan(&total);
        Ok(total)
    }

    /// Bump the index generation and tell the engine (`LibraryIndexUpdated`).
    fn bump_generation_and_notify(&mut self, log: bool) {
        self.generation = self.generation.saturating_add(1);
        let item_count = self
            .libraries
            .iter()
            .map(|l| l.writer.count_playables().unwrap_or(0))
            .sum();
        let generation = self.generation;
        if log {
            eprintln!("plippa-server: library index updated gen={generation} items={item_count}");
        }
        self.with_ipc(|ipc| {
            ipc.send(IpcMessage::LibraryIndexUpdated(LibraryIndexUpdated {
                generation,
                item_count,
            }));
        });
    }

    /// Background scanner hook: refresh the mounted roots (without scanning inline) and say
    /// what to scan. `playing` = the engine reports Playing, so heavy work should wait.
    pub fn scan_plan(&mut self) -> ScanPlan {
        self.with_ipc(|ipc| ipc.poll_inbound());
        self.refresh_libraries(false);
        let mirror = self.with_ipc(|ipc| ipc.mirror());
        ScanPlan {
            roots: self.libraries.iter().map(|l| l.root.clone()).collect(),
            playing: mirror
                .playback
                .as_ref()
                .is_some_and(|p| p.transport == TransportState::Playing),
            current_waveform: mirror
                .playback
                .as_ref()
                .and_then(|p| p.session.waveform_path.as_ref())
                .map(PathBuf::from),
        }
    }

    /// Background scanner hook: publish the outcome of a scan that ran on its own
    /// `LibraryWriter` (the service lock was not held while scanning).
    pub fn apply_background_scan(&mut self, root: &Path, result: Result<ScanReport, LibraryError>) {
        match result {
            Ok(report) => self.after_scan(&report),
            Err(LibraryError::StorageVanished { path, detail }) => self.drop_root(&path, &detail),
            Err(e) => eprintln!("plippa-server: background scan of {} failed: {e}", root.display()),
        }
    }

    /// Background scanner hook: lengths were measured / written back.
    pub fn apply_background_probe(&mut self, report: &ProbeReport) {
        if report.changed() {
            self.bump_generation_and_notify(true);
        }
        if report.write_errors > 0 {
            eprintln!(
                "plippa-server: {} .noudata file(s) could not be updated with measured lengths",
                report.write_errors
            );
        }
    }

    fn drop_root(&mut self, path: &Path, detail: &str) {
        let before = self.libraries.len();
        self.libraries.retain(|l| !paths_same(&l.root, path));
        let msg = format!("Media storage vanished at {}: {detail}", path.display());
        self.warning = Some(msg.clone());
        self.with_ipc(|ipc| {
            ipc.send(IpcMessage::UserNotice(UserNotice {
                severity: NoticeSeverity::Warning,
                code: NOTICE_NO_MEDIA_STORAGE.into(),
                message: msg,
            }));
        });
        if self.libraries.len() != before {
            // Readers in the engine/UI were opened on this root; make them close them.
            self.bump_generation_and_notify(true);
        }
    }

    fn after_scan(&mut self, report: &ScanReport) {
        // No-op rescans (common on the 30 s background timer) must not push
        // LibraryIndexUpdated — each notify makes the engine reopen WAL readers
        // on USB and can starve TapNdef / PlayStop on the hub.
        let changed = report.changed();
        let first_scan = self.generation == 0;
        if !changed && !first_scan {
            return;
        }
        if changed {
            // PWRSW_01: every write path funnels through here (full/lazy scan, write-through
            // index, forget, resume write-backs via a rescan). Bounding the WAL here keeps
            // hours of frames from piling up uncheckpointed on the stick.
            self.checkpoint_libraries();
            eprintln!(
                "plippa-server: library changed (+{} ~{} -{}, orphans +{} -{}, availability {})",
                report.ingested,
                report.updated,
                report.removed,
                report.orphans_added,
                report.orphans_removed,
                report.availability_changed
            );
            for w in report
                .warnings
                .iter()
                .filter(|w| !w.to_ascii_lowercase().contains("vanished"))
            {
                eprintln!("plippa-server: index warning: {w}");
            }
        }
        self.bump_generation_and_notify(false);
        if !report.warnings.is_empty() {
            for w in &report.warnings {
                if w.to_ascii_lowercase().contains("vanished") {
                    self.warning = Some(w.clone());
                }
            }
        }
    }

    /// PWRSW_01: checkpoint each open writer after an index change (bounded WAL).
    fn checkpoint_libraries(&mut self) {
        for lib in &self.libraries {
            if let Err(e) = lib.writer.checkpoint_truncate() {
                eprintln!(
                    "plippa-server: WAL checkpoint failed for {}: {e}",
                    lib.root.display()
                );
            }
        }
    }

    /// Index a `.noudata` the service just wrote (and optionally the one it replaced).
    ///
    /// Write-through, not `lazy_rescan()`: the new row must be visible before the HTTP
    /// response is sent, and a FAT directory timestamp (2 s granularity) can leave a cached
    /// walk unaware of the new name until the next reboot. `after_scan` skips the IPC notify
    /// when nothing changed and bumps the generation otherwise.
    fn index_write_through(
        &mut self,
        lib_idx: usize,
        new_rel: &str,
        old_rel: Option<&str>,
    ) -> Result<(), ServerError> {
        let writer = &mut self.libraries[lib_idx].writer;
        if let Some(old) = old_rel.filter(|old| *old != new_rel) {
            writer
                .forget_noudata(old)
                .map_err(|e| ServerError::Message(format!("index {old}: {e}")))?;
        }
        let report = writer
            .ingest_noudata(new_rel)
            .map_err(|e| ServerError::Message(format!("index {new_rel}: {e}")))?;
        self.after_scan(&report);
        Ok(())
    }

    /// Drop a `.noudata` the service just deleted from the index (FIX_01B).
    fn forget_indexed(&mut self, lib_idx: usize, rel: &str) -> Result<(), ServerError> {
        let report = self.libraries[lib_idx]
            .writer
            .forget_noudata(rel)
            .map_err(|e| ServerError::Message(format!("index {rel}: {e}")))?;
        self.after_scan(&report);
        Ok(())
    }

    pub fn list_playables(&mut self) -> Result<Vec<PlayableSummary>, ServerError> {
        if self.libraries.is_empty() {
            return Err(ServerError::Message(
                self.warning
                    .clone()
                    .unwrap_or_else(|| "No media storage available".into()),
            ));
        }
        let multi = self.libraries.len() > 1;
        let mut out = Vec::new();
        for lib in &self.libraries {
            let records = lib.writer.list_playables().map_err(|e| match e {
                LibraryError::StorageVanished { path, detail } => {
                    ServerError::Message(format!(
                        "Media storage vanished at {}: {detail}",
                        path.display()
                    ))
                }
                other => other.into(),
            })?;
            for r in records {
                out.push(summary_from_record(r, &lib.root, &lib.label, multi));
            }
        }
        out.sort_by(|a, b| {
            a.paths_summary
                .cmp(&b.paths_summary)
                .then(a.id.cmp(&b.id))
        });
        Ok(out)
    }

    pub fn get_playable(&mut self, id: u64) -> Result<PlayableDetail, ServerError> {
        let (lib_idx, record) = self.require_record(id)?;
        let root = self.libraries[lib_idx].root.clone();
        let path = root.join(&record.noudata_relpath);
        // Orphan media file: no `.noudata` to read yet.
        let doc = if record.is_orphan {
            NoudataDocument::default()
        } else {
            read_noudata(&path)?
        };
        let multi = self.libraries.len() > 1;
        let label = self.libraries[lib_idx].label.clone();
        let record_title = record.title.clone();
        let noudata_type = if record.is_orphan {
            NoudataType::LooseTrack
        } else {
            plippa_noubin::effective_type(&doc, plippa_noubin::fs_release_resolver(&path))
        };
        Ok(PlayableDetail {
            noudata_type: noudata_type.as_str().to_string(),
            primary_artist: doc.primary_artist_display(),
            summary: summary_from_record(record, &root, &label, multi),
            playable_item_title: doc
                .playable_item_title
                .clone()
                .or_else(|| record_title.clone()),
            noubin_key: doc.noubin_key.clone(),
            noubin_key_normalised: doc.noubin_key_normalised.clone(),
            media_titles: doc
                .media_list
                .iter()
                .map(|m| m.media_title.clone())
                .collect(),
        })
    }

    pub fn patch_metadata(
        &mut self,
        id: u64,
        patch: MetadataPatch,
    ) -> Result<PlayableDetail, ServerError> {
        let (lib_idx, record) = self.require_record(id)?;
        require_noudata(&record)?;
        let path = self.libraries[lib_idx]
            .root
            .join(&record.noudata_relpath);
        let mut doc = read_noudata(&path)?;
        if let Some(title) = patch.playable_item_title {
            // Portal input: control/bidi characters stripped, NFC, capped (NBSecurityRequirements §3).
            let clean = sanitise_title(&title);
            if clean.is_empty() {
                return Err(ServerError::Invalid("title must not be empty".into()));
            }
            doc.playable_item_title = Some(clean);
        }
        if let Some(artist) = patch.primary_artist {
            let clean = sanitise_title(&artist);
            if clean.is_empty() {
                return Err(ServerError::Invalid("artist must not be empty".into()));
            }
            let ty = plippa_noubin::effective_type(&doc, plippa_noubin::fs_release_resolver(&path));
            match ty {
                NoudataType::LooseTrack => {
                    // Release-level data on a loose track: upgrade to a release in place
                    // (atomic write below). Moving it into a release folder is a follow-up.
                    doc.upgrade_loose_to_release();
                }
                NoudataType::Release => {}
                NoudataType::UserPlaylist | NoudataType::LinkTrack => {
                    return Err(ServerError::Invalid(format!(
                        "a {} has no artist of its own; edit the release instead",
                        ty.as_str()
                    )));
                }
            }
            doc.set_release_primary_artist(&clean);
        }
        doc.write_to_path(&path)
            .map_err(|e| ServerError::Message(e.to_string()))?;
        let _ = self.lazy_rescan();
        self.get_playable(id)
    }

    /// Copy media into the primary library under the §4.4 tree for `kind`.
    ///
    /// Primary = first open root (first discovered, or the override). Kind is required.
    pub fn add_media(
        &mut self,
        kind: ImportKind,
        filename: &str,
        bytes: &[u8],
    ) -> Result<PlayableSummary, ServerError> {
        Ok(self.add_media_with_cues(kind, filename, bytes, None)?.summary)
    }

    /// [`Self::add_media`] plus optional pasted timestamps (`cue_text`, NB_05).
    ///
    /// Cues are only meaningful for the long-form kinds (`mix`, `audiobook`, `podcast`);
    /// for `music`/`other` a non-blank `cue_text` is **rejected** (400) before anything is
    /// written. Blank `cue_text` counts as absent. A non-blank paste with no usable line is
    /// also rejected up front. Cues beyond the file's real length (read from its headers)
    /// are dropped with a warning in the outcome.
    ///
    /// This variant takes the whole file as bytes (tests, small callers); the portal streams
    /// to disk and uses [`Self::add_media_from_file`].
    pub fn add_media_with_cues(
        &mut self,
        kind: ImportKind,
        filename: &str,
        bytes: &[u8],
        cue_text: Option<&str>,
    ) -> Result<AddMediaOutcome, ServerError> {
        use std::io::Write;
        self.precheck_add_media(kind, cue_text)?;
        let (mut f, guard) = crate::web::download::create_temp(&self.staging_dir()?)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        self.add_media_from_file(kind, filename, guard.path(), cue_text)
    }

    /// Everything about an import that can be refused before a byte of the file is received
    /// (no storage, a cue paste on a kind that takes none, an unusable paste).
    pub fn precheck_add_media(
        &self,
        kind: ImportKind,
        cue_text: Option<&str>,
    ) -> Result<(), ServerError> {
        if self.libraries.is_empty() {
            return Err(ServerError::Message("No media storage available".into()));
        }
        let cue_text = cue_text.filter(|t| !t.trim().is_empty());
        if cue_text.is_some()
            && !matches!(
                kind,
                ImportKind::Mix | ImportKind::Audiobook | ImportKind::Podcast
            )
        {
            return Err(ServerError::Invalid(format!(
                "cue_text is only accepted for kind mix, audiobook or podcast (got {})",
                kind.as_str()
            )));
        }
        // Reject an unusable paste before any file is written (length unknown yet).
        if let Some(t) = cue_text {
            cues::paste_for_new_media(t, None)?;
        }
        Ok(())
    }

    /// `<primary library>/.plippa/tmp` — where uploads are staged (same volume as the library,
    /// so the final move is an atomic rename).
    pub fn staging_dir(&self) -> Result<PathBuf, ServerError> {
        let lib = self
            .libraries
            .first()
            .ok_or_else(|| ServerError::Message("No media storage available".into()))?;
        LibraryFs::new(&lib.root)
            .and_then(|l| l.tmp_dir())
            .map_err(|e| ServerError::Message(e.to_string()))
    }

    /// Import the already-complete file `src` (inside [`Self::staging_dir`]; it is moved, not
    /// copied) into the primary library under the §4.4 tree, exactly like
    /// [`Self::add_media_with_cues`]. On any error nothing is left in the library.
    pub fn add_media_from_file(
        &mut self,
        kind: ImportKind,
        filename: &str,
        src: &Path,
        cue_text: Option<&str>,
    ) -> Result<AddMediaOutcome, ServerError> {
        let mut tx = crate::unite::fsops::Transaction::default();
        let imported = match self.import_file(&mut tx, kind, filename, src, cue_text) {
            Ok(i) => i,
            Err(e) => {
                tx.rollback();
                return Err(e);
            }
        };
        tx.commit();
        let _ = self.lazy_rescan();
        let root = self.libraries[0].root.clone();
        let label = self.libraries[0].label.clone();
        let multi = self.libraries.len() > 1;
        let summary = list_by_rel(self, &root, &imported.noudata_rel, &label, multi)?;
        Ok(AddMediaOutcome {
            summary,
            cues: imported.cues,
        })
    }

    /// The write half of an import: moves `src` into place and writes its companion `.noudata`,
    /// recording every change in `tx` (the caller rolls back or commits). No rescan.
    pub(crate) fn import_file(
        &mut self,
        tx: &mut crate::unite::fsops::Transaction,
        kind: ImportKind,
        filename: &str,
        src: &Path,
        cue_text: Option<&str>,
    ) -> Result<ImportedFile, ServerError> {
        self.import_file_titled(tx, kind, filename, src, cue_text, None)
    }

    /// [`Self::import_file`] with an optional title that replaces the tag / file-name title.
    pub(crate) fn import_file_titled(
        &mut self,
        tx: &mut crate::unite::fsops::Transaction,
        kind: ImportKind,
        filename: &str,
        src: &Path,
        cue_text: Option<&str>,
        title: Option<&str>,
    ) -> Result<ImportedFile, ServerError> {
        self.precheck_add_media(kind, cue_text)?;
        let cue_text = cue_text.filter(|t| !t.trim().is_empty());
        // The client's file name and the tags are untrusted text: made safe before they
        // become folder or file names (no `..`, hidden/reserved names, control characters).
        let filename = safe_path_segment(
            filename.rsplit(['/', '\\']).next().unwrap_or(filename),
            "upload",
            MAX_SEGMENT_BYTES,
        );
        let filename = filename.as_str();
        let mut fields = extract_tags_from_path(filename, src);
        if let Some(t) = title.map(sanitise_title).filter(|t| !t.is_empty()) {
            fields.title = Some(t);
        }
        let placement = import_placement(kind, &fields, filename);
        let root = self.libraries[0].root.clone();
        let rel_dir: PathBuf = placement
            .relative_dir
            .components()
            .map(|c| safe_path_segment(&c.as_os_str().to_string_lossy(), "Unknown", MAX_SEGMENT_BYTES))
            .collect();
        // Refuses symlinked folders; creates the folders one by one.
        LibraryFs::new(&root)
            .and_then(|lfs| lfs.ensure_dir(&rel_dir, tx.dirs()).map(|_| ()))
            .map_err(|e| ServerError::Invalid(e.to_string()))?;
        // Existing folders may be spelled differently on a case-insensitive volume; the index
        // records the on-disk spelling, so continue with that (see `actual_rel_spelling`).
        let rel_dir = actual_rel_path(&root, &rel_dir);
        let dest_dir = root.join(&rel_dir);

        let media_path = unique_media_path(&dest_dir, &placement.media_filename);
        tx.move_in(src, &media_path)
            .map_err(|e| ServerError::Message(e.to_string()))?;

        let media_name = media_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&placement.media_filename)
            .to_string();
        let noudata_path = media_path.with_extension("noudata");

        let mut doc = companion_doc(kind, &fields, &placement, &media_name);
        let mut cue_report = None;
        if let Some(text) = cue_text {
            let duration = crate::scanner::probe_duration_ms(&media_path);
            let paste = cues::paste_for_new_media(text, duration)?;
            cues::apply_owned_list(&mut doc, 0, &paste, None);
            cue_report = Some(cues::report_for(&paste, 0, duration, Some(0), false, None));
        }
        tx.write_new(&noudata_path, doc.to_json_pretty_checked()?.as_bytes())
            .map_err(|e| ServerError::Message(e.to_string()))?;
        Ok(ImportedFile {
            noudata_rel: path_relative_posix(&root, &noudata_path)?,
            cues: cue_report,
        })
    }

    /// Give an orphan media file (no `.noudata`) its companion `<stem>.noudata`, like the Add
    /// dialog does for a new upload: tags naming an artist/album ⇒ a `release`, else a
    /// `looseTrack` titled from the tags (or `title`, or the file name); `playableItemProfile`
    /// from `kind`, single-item `mediaList`. Nothing is moved; the file is
    /// written atomically without replacing anything. Returns the summary of the new playable
    /// (its id differs from the orphan's).
    pub fn create_noudata_for_orphan(
        &mut self,
        id: u64,
        kind: ImportKind,
        title: Option<&str>,
    ) -> Result<PlayableSummary, ServerError> {
        let (lib_idx, rec) = self.require_record(id)?;
        if !rec.is_orphan || !rec.noudata_relpath.is_empty() {
            return Err(ServerError::Invalid(
                "this item already has a .noudata; nothing to create".into(),
            ));
        }
        let (root, label) = {
            let lib = &self.libraries[lib_idx];
            (lib.root.clone(), lib.label.clone())
        };
        let media_rel = {
            let items = self.libraries[lib_idx].writer.items_of(rec.id)?;
            items
                .iter()
                .find(|i| i.position == 0)
                .map(|i| i.media_relpath.clone())
                .filter(|r| !r.is_empty())
                .ok_or_else(|| ServerError::NotFound("the media file is not indexed".into()))?
        };
        // Resolves inside the library root and refuses symlinked folders.
        let media_path = LibraryFs::new(&root)
            .and_then(|lfs| lfs.resolve(Path::new(&media_rel)))
            .map_err(|e| ServerError::Invalid(e.to_string()))?;
        if !media_path.is_file() {
            return Err(ServerError::NotFound("the media file is missing".into()));
        }
        let file_name = media_path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| ServerError::Invalid("bad media file name".into()))?
            .to_string();
        let mut fields = extract_tags_from_path(&file_name, &media_path);
        if let Some(t) = title {
            let clean = sanitise_title(t);
            if !clean.is_empty() {
                fields.title = Some(clean);
            }
        }
        let placement = import_placement(kind, &fields, &file_name);
        let doc = companion_doc(kind, &fields, &placement, &file_name);
        let noudata_path = media_path.with_extension("noudata");
        let mut tx = crate::unite::fsops::Transaction::default();
        if let Err(e) = tx.write_new(&noudata_path, doc.to_json_pretty_checked()?.as_bytes()) {
            tx.rollback();
            return Err(ServerError::Conflict(format!(
                "could not create {}: {e}",
                noudata_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("the .noudata")
            )));
        }
        tx.commit();
        let noudata_rel = path_relative_posix(&root, &noudata_path)?;
        let _ = self.lazy_rescan();
        let multi = self.libraries.len() > 1;
        list_by_rel(self, &root, &noudata_rel, &label, multi)
    }

    pub fn play_playable(&mut self, id: u64) -> Result<(), ServerError> {
        let _ = self.require_record(id)?;
        let live = self.with_ipc(|ipc| {
            if !ipc.is_live() {
                return false;
            }
            ipc.poll_inbound();
            ipc.send(IpcMessage::PlaybackCommand(PlaybackCommand::LoadAndPlay(
                PlayableItemId::new(id),
            )));
            true
        });
        if !live {
            return Err(ServerError::Message(
                "Engine not connected — start `plippa` / `plippa-engine` so Play can reach audio"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn stop_playback(&mut self) -> Result<(), ServerError> {
        let live = self.with_ipc(|ipc| {
            if !ipc.is_live() {
                return false;
            }
            ipc.poll_inbound();
            ipc.send(IpcMessage::PlaybackCommand(PlaybackCommand::Stop));
            true
        });
        if !live {
            return Err(ServerError::Message(
                "Engine not connected — start `plippa` / `plippa-engine` so Stop can reach audio"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Stop playback and clear session resume bookmarks in the engine.
    pub fn unload_playback(&mut self) -> Result<(), ServerError> {
        let live = self.with_ipc(|ipc| {
            if !ipc.is_live() {
                return false;
            }
            ipc.poll_inbound();
            ipc.send(IpcMessage::PlaybackCommand(PlaybackCommand::Unload));
            true
        });
        if !live {
            return Err(ServerError::Message(
                "Engine not connected — start `plippa` / `plippa-engine` so Unload can reach audio"
                    .into(),
            ));
        }
        Ok(())
    }

    pub fn toggle_play_pause(&mut self) -> Result<(), ServerError> {
        let mirror = self.with_ipc(|ipc| {
            ipc.poll_inbound();
            ipc.mirror()
        });
        if !mirror.live {
            return Err(ServerError::Message(
                "Engine not connected — start `plippa` / `plippa-engine` for Play/Pause"
                    .into(),
            ));
        }
        let playing = mirror
            .playback
            .as_ref()
            .map(|p| p.transport == TransportState::Playing)
            .unwrap_or(false);
        let cmd = if playing {
            PlaybackCommand::Pause
        } else {
            PlaybackCommand::Play
        };
        self.with_ipc(|ipc| {
            ipc.send(IpcMessage::PlaybackCommand(cmd));
        });
        Ok(())
    }

    pub fn associate_playable(&mut self, id: u64) -> Result<AssociateOutcome, ServerError> {
        self.associate_playable_with_mode(id, false)
    }

    /// Associate one track (`mediaList` position `item`) of an album/mix/audiobook/podcast:
    /// creates/reuses that track's own `.noudata` (`referToReleaseNoudata`, no `releaseData`).
    pub fn associate_track(
        &mut self,
        id: u64,
        item: u32,
        keep_existing_key: bool,
    ) -> Result<AssociateOutcome, ServerError> {
        let prep = self.begin_associate_item(id, Some(item), keep_existing_key)?;
        let resp = self.wait_associate_response(&prep);
        self.finish_associate(prep, resp)
    }

    pub fn associate_playable_keep_existing(
        &mut self,
        id: u64,
    ) -> Result<AssociateOutcome, ServerError> {
        self.associate_playable_with_mode(id, true)
    }

    pub fn associate_playable_with_mode(
        &mut self,
        id: u64,
        keep_existing_key: bool,
    ) -> Result<AssociateOutcome, ServerError> {
        let prep = self.begin_associate(id, keep_existing_key)?;
        let resp = self.wait_associate_response(&prep);
        self.finish_associate(prep, resp)
    }

    pub fn begin_associate(
        &mut self,
        id: u64,
        keep_existing_key: bool,
    ) -> Result<AssociatePrep, ServerError> {
        self.begin_associate_item(id, None, keep_existing_key)
    }

    pub fn begin_associate_item(
        &mut self,
        id: u64,
        item: Option<u32>,
        keep_existing_key: bool,
    ) -> Result<AssociatePrep, ServerError> {
        let (lib_idx, record) = self.require_record(id)?;
        require_noudata(&record)?;
        let own_path = self.libraries[lib_idx].root.join(&record.noudata_relpath);
        let own_doc = read_noudata(&own_path)?;
        let target = self.resolve_associate_target(
            id,
            item,
            &record,
            lib_idx,
            own_doc.playable_item_title.clone(),
        )?;
        let doc_before = match &target.new_doc {
            Some(d) => d.clone(),
            None if item.is_none() => own_doc,
            None => read_noudata(&self.libraries[lib_idx].root.join(&target.rel))?,
        };
        let title = target.title.clone();

        let noubin_key = if keep_existing_key {
            String::new()
        } else {
            let mut existing = std::collections::HashSet::new();
            for lib in &self.libraries {
                if let Ok(keys) = lib.writer.all_normalised_keys() {
                    existing.extend(keys);
                }
            }
            let (key, _norm) = generate_local_noubin_key(&LocalKeyParams {
                playable_item_title: &title,
                max_key_utf8_bytes: 120,
                existing_normalised: &existing,
                random_len: 6,
            })?;
            key
        };

        let request_id = self.next_associate_id.fetch_add(1, Ordering::Relaxed);
        let req = AssociateRequest {
            request_id,
            playable_id: PlayableItemId::new(id),
            noudata_relpath: record.noudata_relpath.clone(),
            playable_item_title: title,
            noubin_key: noubin_key.clone(),
            keep_existing_key,
        };
        self.with_ipc(|ipc| ipc.send(IpcMessage::AssociateRequest(req.clone())));
        let mode = if keep_existing_key {
            "keep existing key (read tag)"
        } else {
            "write new key"
        };
        eprintln!(
            "plippa-server: associate ({mode}) — tap a Noubin on the player (request_id={request_id}, waiting up to {}s)",
            crate::uds_ipc::associate_wait_timeout().as_secs()
        );
        Ok(AssociatePrep {
            req,
            target,
            doc_before,
            fallback_key: noubin_key,
        })
    }

    fn wait_associate_response(&mut self, prep: &AssociatePrep) -> AssociateResponse {
        let request_id = prep.req.request_id;
        let timeout = crate::uds_ipc::associate_wait_timeout();
        let (waited, live) = self.with_ipc(|ipc| {
            let r = ipc.wait_associate_response(request_id, timeout);
            (r, ipc.supports_live_associate())
        });
        match waited {
            Some(r) => r,
            None if live => AssociateResponse {
                request_id,
                ok: false,
                noubin_key: None,
                noubin_key_normalised: None,
                error: Some("NFC associate timed out waiting for UI".into()),
                ..Default::default()
            },
            None => (self.associate_nfc)(&prep.req),
        }
    }
}

/// Associate without holding `ServerService` across the NFC wait (MVP_14).
pub fn associate_playable_unlocked(
    service: &Arc<Mutex<ServerService>>,
    id: u64,
) -> Result<AssociateOutcome, ServerError> {
    associate_playable_unlocked_with_mode(service, id, false)
}

/// Like [`associate_playable_unlocked`], reading the existing tag key instead of writing.
pub fn associate_playable_keep_existing_unlocked(
    service: &Arc<Mutex<ServerService>>,
    id: u64,
) -> Result<AssociateOutcome, ServerError> {
    associate_playable_unlocked_with_mode(service, id, true)
}

/// Per-track variant of [`associate_playable_unlocked`] (see [`ServerService::associate_track`]).
pub fn associate_track_unlocked(
    service: &Arc<Mutex<ServerService>>,
    id: u64,
    item: u32,
    keep_existing_key: bool,
) -> Result<AssociateOutcome, ServerError> {
    associate_unlocked_impl(service, id, Some(item), keep_existing_key)
}

fn associate_playable_unlocked_with_mode(
    service: &Arc<Mutex<ServerService>>,
    id: u64,
    keep_existing_key: bool,
) -> Result<AssociateOutcome, ServerError> {
    associate_unlocked_impl(service, id, None, keep_existing_key)
}

fn associate_unlocked_impl(
    service: &Arc<Mutex<ServerService>>,
    id: u64,
    item: Option<u32>,
    keep_existing_key: bool,
) -> Result<AssociateOutcome, ServerError> {
    let (prep, ipc) = {
        let mut s = service
            .lock()
            .map_err(|_| ServerError::Message("service lock poisoned".into()))?;
        let prep = s.begin_associate_item(id, item, keep_existing_key)?;
        let ipc = s.ipc_handle();
        (prep, ipc)
    };

    let request_id = prep.req.request_id;
    let live = {
        let guard = ipc
            .lock()
            .map_err(|_| ServerError::Message("ipc lock poisoned".into()))?;
        guard.supports_live_associate()
    };

    let waited = if live {
        let timeout = crate::uds_ipc::associate_wait_timeout();
        let deadline = std::time::Instant::now() + timeout;
        let mut found = None;
        while std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let slice = remaining.min(std::time::Duration::from_millis(100));
            let mut guard = ipc
                .lock()
                .map_err(|_| ServerError::Message("ipc lock poisoned".into()))?;
            if let Some(r) = guard.wait_associate_response(request_id, slice) {
                found = Some(r);
                break;
            }
            drop(guard);
        }
        found
    } else {
        None
    };

    let resp = match waited {
        Some(r) => r,
        None if live => AssociateResponse {
            request_id,
            ok: false,
            noubin_key: None,
            noubin_key_normalised: None,
            error: Some("NFC associate timed out waiting for UI".into()),
            ..Default::default()
        },
        None => {
            let mut s = service
                .lock()
                .map_err(|_| ServerError::Message("service lock poisoned".into()))?;
            (s.associate_nfc)(&prep.req)
        }
    };

    let mut s = service
        .lock()
        .map_err(|_| ServerError::Message("service lock poisoned".into()))?;
    s.finish_associate(prep, resp)
}

impl ServerService {
    fn require_record(&mut self, id: u64) -> Result<(usize, PlayableRecord), ServerError> {
        if self.libraries.is_empty() {
            return Err(ServerError::Unavailable(
                self.warning
                    .clone()
                    .unwrap_or_else(|| "No media storage available".into()),
            ));
        }
        let (ns, local) = decode_playable_id(id);
        for (idx, lib) in self.libraries.iter().enumerate() {
            if library_namespace(&lib.root) != ns {
                continue;
            }
            match lib.writer.lookup_by_id(local) {
                Ok(Some(rec)) => return Ok((idx, rec)),
                Ok(None) => {
                    return Err(ServerError::NotFound(format!("No playable with id {id}")));
                }
                Err(LibraryError::StorageVanished { path, detail }) => {
                    return Err(ServerError::Unavailable(format!(
                        "Media storage vanished at {}: {detail}",
                        path.display()
                    )));
                }
                Err(e) => return Err(e.into()),
            }
        }
        // Backward-compat: if high bits are 0 and we have a single library, try local id.
        if ns == 0 && self.libraries.len() == 1 {
            let lib = &self.libraries[0];
            if let Ok(Some(rec)) = lib.writer.lookup_by_id(id as i64) {
                return Ok((0, rec));
            }
        }
        Err(ServerError::NotFound(format!("No playable with id {id}")))
    }
}

/// Orphan media files (no `.noudata`) cannot be patched / associated until one exists.
fn require_noudata(record: &PlayableRecord) -> Result<(), ServerError> {
    if record.is_orphan || record.noudata_relpath.is_empty() {
        return Err(ServerError::Message(
            "This media file has no .noudata yet (listed from the file name); \
             it cannot be edited or associated until one is created"
                .into(),
        ));
    }
    Ok(())
}

/// The import and unite hubs of a new service. They share one staging cap: everything staged on
/// the stick counts against it, whichever route staged it.
fn new_hubs() -> (Arc<UniteHub>, Arc<ImportHub>) {
    let staging = Arc::new(StagingBudget::production());
    let limits = DownloadLimits::from_env();
    (
        Arc::new(UniteHub::with_limits_and_staging(
            limits.clone(),
            Arc::clone(&staging),
        )),
        Arc::new(ImportHub::with_limits_and_staging(limits, staging)),
    )
}

fn open_one(d: &DiscoveredLibrary) -> Result<OpenLibrary, ServerError> {
    let lib = LibraryRoot::new(&d.path);
    library_root_available(&lib)?;
    // Capture before opening: if the mount changes under us the next poll notices the
    // mismatch and reopens rather than trusting a half-open connection.
    let mount = MountIdentity::of(&d.path);
    // Called under the service mutex (start-up, status polls, hot-plug): never wait long for
    // another process that is opening the same index. A busy index is `Busy`, reported once
    // and retried by the next poll.
    let writer = LibraryWriter::open_with(&lib, plippa_noubin::OpenWait::Interactive)?;
    Ok(OpenLibrary {
        root: d.path.clone(),
        label: d.label.clone(),
        writer,
        mount,
    })
}

/// True while the open library still refers to the filesystem it was opened on.
fn library_is_current(lib: &OpenLibrary) -> bool {
    if !lib.root.is_dir() {
        return false;
    }
    match &lib.mount {
        // Identity unavailable when it was opened (non-unix): fall back to existence.
        None => true,
        Some(mount) => mount.still_valid(&lib.root),
    }
}

fn paths_same(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

fn merge_report(into: &mut ScanReport, other: ScanReport) {
    into.scanned_files += other.scanned_files;
    into.ingested += other.ingested;
    into.updated += other.updated;
    into.removed += other.removed;
    into.skipped += other.skipped;
    into.orphans_added += other.orphans_added;
    into.orphans_removed += other.orphans_removed;
    into.availability_changed += other.availability_changed;
    into.noudata_reads += other.noudata_reads;
    into.dirs_listed += other.dirs_listed;
    into.warnings.extend(other.warnings);
}

fn transport_label(t: TransportState) -> &'static str {
    match t {
        TransportState::Playing => "Playing",
        TransportState::Paused => "Paused",
        TransportState::Stopped => "Stopped",
    }
}

fn summary_from_record(
    r: PlayableRecord,
    root: &Path,
    label: &str,
    multi: bool,
) -> PlayableSummary {
    let associated = r.noubin_key_normalised.is_some();
    let raw_paths = if r.media_relpaths.is_empty() {
        r.noudata_relpath.clone()
    } else if r.media_relpaths.len() == 1 {
        r.media_relpaths[0].clone()
    } else {
        format!(
            "{} (+{} more)",
            r.media_relpaths[0],
            r.media_relpaths.len() - 1
        )
    };
    let paths_summary = display_path_with_library(label, &raw_paths, multi);
    PlayableSummary {
        id: encode_playable_id(root, r.id),
        title: r.title,
        associated,
        noubin_key_normalised: r.noubin_key_normalised,
        noudata_relpath: if multi {
            display_path_with_library(label, &r.noudata_relpath, true)
        } else {
            r.noudata_relpath
        },
        media_relpaths: r.media_relpaths,
        paths_summary,
        kind: r.kind.as_str().to_string(),
        group: r.group.as_str().to_string(),
        noudata_type: r.noudata_type.clone(),
        library_label: multi.then(|| label.to_string()),
    }
}

fn list_by_rel(
    svc: &mut ServerService,
    root: &Path,
    rel: &str,
    label: &str,
    multi: bool,
) -> Result<PlayableSummary, ServerError> {
    let lib = svc
        .libraries
        .iter()
        .find(|l| paths_same(&l.root, root))
        .ok_or_else(|| ServerError::Message("primary library missing".into()))?;
    let records = lib.writer.list_playables()?;
    let rec = find_by_rel(&records, rel, root).ok_or_else(|| {
        ServerError::Message(format!(
            "added media not found after rescan: {rel} is not in the index"
        ))
    })?;
    Ok(summary_from_record(rec.clone(), root, label, multi))
}

/// The indexed row for a library-relative `.noudata` path, tolerating letter case.
///
/// The index keys playables by the spelling the scan read from disk. Paths handed in here were
/// built from tag (or client) text, and on a case-insensitive volume (exFAT/FAT on the stick)
/// that can differ in letter case from the stored name — `Music/Outkast/…` for an existing
/// `Music/OutKast/…`. This is the read-side safety net (with [`actual_rel_spelling`] fixing the
/// write side), and it names the path when the item really is missing (the failure used to be an
/// unattributed "not found after rescan").
///
/// The fallback only runs when the path is on disk: a path with no on-disk spelling must stay a
/// miss rather than resolve to some other row (callers only ask about files they just wrote, so
/// the requested spelling and the stored one always name the same file).
fn find_by_rel<'a>(
    records: &'a [PlayableRecord],
    rel: &str,
    root: &Path,
) -> Option<&'a PlayableRecord> {
    if let Some(r) = records.iter().find(|r| r.noudata_relpath == rel) {
        return Some(r);
    }
    if !root.join(actual_rel_spelling(root, rel)).exists() {
        return None;
    }
    // ASCII-insensitive first (no allocation; every tag/folder case mismatch seen so far is
    // ASCII), then a Unicode-lowercase pass for the rarer `É`/`é` kind.
    records
        .iter()
        .find(|r| r.noudata_relpath.eq_ignore_ascii_case(rel))
        .or_else(|| {
            let want = rel.to_lowercase();
            records
                .iter()
                .find(|r| r.noudata_relpath.to_lowercase() == want)
        })
}

/// `rel` with the letter case the volume actually stores.
///
/// Import paths are built from tag text, but exFAT/FAT (the USB stick) are case-insensitive and
/// keep the spelling a folder was first created with: a tag `Outkast` lands in an existing
/// `Music/OutKast/`, and the index — which records what `read_dir` returns — then holds
/// `Music/OutKast/...`. Returning the path we asked for would name a playable the index never
/// has (that lookup used to fail the whole import with "not found after rescan"). Each segment is
/// therefore re-spelled from its directory listing; one that is not in the listing stays as asked
/// (the callers only re-spell directories that `ensure_dir` just created, so an exact spelling is
/// always present and the new media file name is never rewritten).
///
/// The listing decides — never a `stat`: on exFAT/FAT, `symlink_metadata("…/Outkast")` succeeds
/// for an on-disk `OutKast`, so a probe cannot tell the spellings apart and would turn this
/// whole function into a no-op on the very filesystem it exists for.
fn actual_rel_spelling(root: &Path, rel: &str) -> String {
    // `..`/`.` never come out of `safe_path_segment`; refuse to walk such a path rather than
    // re-spell it (the callers' own containment checks stay authoritative).
    if rel.split('/').any(|p| p == "." || p == "..") {
        return rel.to_string();
    }
    let mut dir = root.to_path_buf();
    let mut out: Vec<String> = Vec::new();
    for part in rel.split('/').filter(|p| !p.is_empty()) {
        let names: Vec<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let name = names
            .iter()
            .find(|n| *n == part)
            .or_else(|| names.iter().find(|n| n.eq_ignore_ascii_case(part)))
            .cloned()
            .unwrap_or_else(|| part.to_string());
        dir = dir.join(&name);
        out.push(name);
    }
    out.join("/")
}

/// [`actual_rel_spelling`] for a `Path` (library-relative).
fn actual_rel_path(root: &Path, rel: &Path) -> PathBuf {
    actual_rel_spelling(root, &rel.to_string_lossy().replace('\\', "/")).into()
}

/// Result of [`ServerService::import_file`] (before the rescan).
pub(crate) struct ImportedFile {
    pub noudata_rel: String,
    pub cues: Option<CueSaveReport>,
}

/// The companion `.noudata` of one freshly imported (or adopted orphan) media file, per the
/// single-file import rule: tags naming an artist and/or album ⇒ a `release` (single-item
/// `mediaList`, `playableItemProfile` from the kind, real credits from the tags); no usable
/// tags ⇒ a `looseTrack` (title from the tag or file name, no `releaseData`, no credits and
/// never a placeholder artist).
fn companion_doc(
    kind: ImportKind,
    fields: &plippa_noubin::ExtractedFields,
    placement: &plippa_noubin::ImportPlacement,
    media_name: &str,
) -> NoudataDocument {
    let loose = placement.noudata_type == NoudataType::LooseTrack;
    let release_data = (!loose).then(|| {
        let role = match kind {
            ImportKind::Music => "performer",
            ImportKind::Mix => "DJ",
            ImportKind::Audiobook => "author",
            ImportKind::Podcast => "host",
            ImportKind::Other => "performer",
        };
        // Spec layout: `credits` under `releaseData`; roles belong on `contributors`, not
        // `primaryArtists`; safe names use the FolderSafe spelling. No artist tag ⇒ no credits.
        let credits = placement.primary_artist.as_ref().map(|primary| Credits {
            primary_artists: Some(vec![PrimaryArtist {
                primary_artist_name: primary.clone(),
                extra: Default::default(),
            }]),
            primary_artist_folder_safe_name: Some(sanitize_filename(primary, "Unknown")),
            contributors: Some(vec![Contributor {
                contributor_role: Some(role.to_string()),
                contributor_name: primary.clone(),
                contributor_url: None,
                extra: Default::default(),
            }]),
            ..Default::default()
        });
        ReleaseData {
            credits,
            ..Default::default()
        }
    });

    NoudataDocument {
        what_am_i_looking_at: Some(WHAT_AM_I_LOOKING_AT.into()),
        noudata_version: Some(NOUDATA_VERSION.into()),
        noudata_type: Some(placement.noudata_type),
        playable_item_title: Some(placement.playable_item_title.clone()),
        playable_item_folder_safe_title: Some(placement.playable_item_safe_title.clone()),
        playable_item_profile: Some(placement.profile),
        release_data,
        media_list: vec![MediaListItem {
            local_file_paths: Some(vec![media_name.to_string()]),
            // `mediaTitle` is not recommended on a loose track (use `playableItemTitle`).
            media_title: (!loose).then(|| {
                fields
                    .title
                    .clone()
                    .unwrap_or_else(|| placement.playable_item_title.clone())
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// A media path in `dir` whose companion `.noudata` name is free too (nothing is replaced).
fn unique_media_path(dir: &Path, filename: &str) -> PathBuf {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = Path::new(filename).extension().and_then(|s| s.to_str());
    for i in 0..10_000 {
        let name = match (i, ext) {
            (0, _) => filename.to_string(),
            (_, Some(e)) => format!("{stem}-{i}.{e}"),
            (_, None) => format!("{stem}-{i}"),
        };
        let p = dir.join(name);
        if std::fs::symlink_metadata(&p).is_err()
            && std::fs::symlink_metadata(p.with_extension("noudata")).is_err()
        {
            return p;
        }
    }
    dir.join(format!("{filename}.{}", std::process::id()))
}

fn read_noudata(path: &Path) -> Result<NoudataDocument, ServerError> {
    let bytes = plippa_noubin::read_noudata_capped(path)?;
    Ok(parse_noudata(&bytes)?)
}

fn path_relative_posix(root: &Path, full: &Path) -> Result<String, ServerError> {
    let rel = full
        .strip_prefix(root)
        .map_err(|_| ServerError::Message("path not under library root".into()))?;
    Ok(rel.to_string_lossy().replace('\\', "/"))
}

fn serialize_u64_as_string<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

fn serialize_opt_u64_as_string<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        Some(v) => serializer.serialize_str(&v.to_string()),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(rel: &str) -> PlayableRecord {
        PlayableRecord {
            id: 1,
            noudata_relpath: rel.to_string(),
            noubin_key_normalised: None,
            title: None,
            media_relpaths: Vec::new(),
            kind: plippa_noubin::LibraryKind::Track,
            group: plippa_noubin::LibraryKind::Track,
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
        }
    }

    /// The tag text `Outkast` and the stored folder `OutKast` name the same file on the stick.
    #[test]
    fn lookup_tolerates_letter_case_differences() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let root = tmp.path();
        let on_disk = "Music/OutKast/Greatest Hits/01.noudata";
        std::fs::create_dir_all(root.join("Music/OutKast/Greatest Hits")).expect("dirs");
        std::fs::write(root.join(on_disk), b"{}").expect("file");
        let records = vec![record(on_disk)];

        let found = find_by_rel(&records, "Music/Outkast/Greatest Hits/01.noudata", root)
            .expect("case-insensitive fallback finds the row");
        assert_eq!(found.noudata_relpath, on_disk);

        // A path that is not on disk stays a miss: the fallback must never resolve a genuinely
        // missing file to a different item that only differs in case.
        assert!(find_by_rel(&records, "Music/Outkast/Stankonia/01.noudata", root).is_none());

        // An exact match always wins over the fallback.
        let exact = vec![
            record("Music/Outkast/a.noudata"),
            record("Music/OutKast/a.noudata"),
        ];
        assert_eq!(
            find_by_rel(&exact, "Music/OutKast/a.noudata", root)
                .expect("exact")
                .noudata_relpath,
            "Music/OutKast/a.noudata"
        );
    }

    /// Segments already on disk are re-spelled to what the volume stores; a new file keeps the
    /// name the import asked for.
    #[test]
    fn actual_rel_spelling_uses_the_name_on_disk() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("Music/OutKast/Greatest Hits")).expect("dirs");
        assert_eq!(
            actual_rel_spelling(root, "Music/Outkast/Greatest Hits/01 - Hey Ya.mp3"),
            "Music/OutKast/Greatest Hits/01 - Hey Ya.mp3"
        );
        // A folder that does not exist yet is left exactly as asked.
        assert_eq!(
            actual_rel_spelling(root, "Music/Brand New/Album/x.mp3"),
            "Music/Brand New/Album/x.mp3"
        );
        // An exact spelling is preserved verbatim (never rewritten to the listing's order).
        assert_eq!(
            actual_rel_spelling(root, "Music/OutKast/Greatest Hits"),
            "Music/OutKast/Greatest Hits"
        );
        // A path that could climb out of the root is refused rather than walked.
        assert_eq!(actual_rel_spelling(root, "../etc/passwd"), "../etc/passwd");
    }
}
