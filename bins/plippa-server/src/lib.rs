//! `plippa-server` — media library management and desktop browser portal (MVP_07).
//!
//! Owns scan/index writes (WAL SQLite), `.noudata` write-through, and LAN HTTP UI.
//! Engine is notified via Postcard IPC; NFC associate is orchestrated so `.noudata`
//! keys are written only after a successful NFC write (or keep-existing read).

pub mod admission;
mod config;
mod http;
mod ipc;
pub mod limits;
mod portal;
pub mod portal_access;
mod scanner;
mod service;
mod tags;
pub mod tls;
mod uds_ipc;
pub mod unite;
pub mod waveform;
pub mod web;

#[cfg(test)]
mod xcheck_tests;

pub use config::{capabilities_for_board, load_capabilities, ServerConfig, DEFAULT_CONTROL_BIND};
pub use admission::{client_id, AdmissionLimits, ConnectionInfo, Connections, UploadGate, UploadSlot};
pub use http::{serve, serve_until, serve_with_access, serve_with_limits};
pub use portal_access::{
    AccessError, Clock, LockoutPolicy, LoginOutcome, ManualClock, PortalAccess, PortalSettings,
    SystemClock, PIN_LEN, SESSION_COOKIE,
};
pub use limits::HttpLimits;
pub use ipc::{IpcBus, MockIpcBus, NullIpc, RecordingIpc};
pub use portal::{
    is_reachable_from_another_device, normalise_base, PortalConfig, PortalUrlCandidate, PortalUrls,
    ENV_PORTAL_URL,
};
pub use scanner::{
    probe_duration_ms, spawn_background_scanner, spawn_mount_watcher, spawn_resume_writer,
    BackgroundScanner, MOUNT_POLL_INTERVAL, PROBE_BATCH,
};
pub use service::{
    preview_cues, AddMediaOutcome, CueListOut, CueOut, CuePreview, CuePreviewRequest,
    CueSaveReport, ItemCues, SaveCuesRequest, MAX_CUE_PASTE_BYTES,
    associate_playable_keep_existing_unlocked, associate_playable_unlocked,
    associate_track_unlocked, AddPlaylistItems, AssociateBound, AssociateItemRef,
    AssociateOutcome, AssociatePrep, AssociateTarget, AssociateWarning, WARN_DUPLICATE_KEY, CollectionDetail, CollectionItem, CollectionSummary, TrackSummary,
    CreatePlaylist, IdArg, KindCount, LibraryRootInfo, LibraryStatus, MetadataPatch, NowPlaying,
    PlayableDetail, PlayableSummary, RenamePlaylist, ReorderPlaylist, ScanPlan, ServerError,
    ServerService, TrackRef, play_collection_unlocked, ListEntryOut, NowPlayingListOut,
    PlayCollection, PlayCollectionOut, WaveformItemStatus, WaveformStatus,
    cover_limits, ensure_thumbnails, spawn_cover_worker, CoverList, CoverOut, CoverScope,
    CoverStore, OrderEntry, PutCovers, StageCtx, ThumbReport, MAX_COVERS, MAX_COVER_BYTES,
    DeleteBlocker, DeleteError, DeleteFile, DeleteOutcome, DeletePreview, DeleteRequest,
    DeleteTarget, PlaylistHit,
};
pub use uds_ipc::{
    associate_wait_timeout, connect_or_null, ipc_connect_timeout, ReconnectingIpc, UdsIpc,
};
pub use service::stores::{
    log_tail as store_log_tail, spawn_store_sync_worker, BandcampKind, BandcampUrl, Candidate,
    CollectionPage, ConfirmCandidate, ConfirmMatchRequest, CoverImage, DownloadConsent,
    DownloadItem, DownloadLimitsView, DownloadRequest, DownloadStart, DownloadState, DownloadStatus,
    HandOffState, HandedOff, ItemState, LibraryItemRef, MatchConfirmation, MatchRequest, MatchResult,
    PrepareDownloadRequest, QualityNote, QualityWarning, RefusedTrack, ReleaseSummary, ReleaseView,
    StagedRelease, StagedStoreDownload, StagedTrackFile, StoreApiError, StoreCache, StoreHub,
    StorePrefs, StoreSecrets, StoreStatus, StoreTransport, StoresStatus, StoresSummary, SyncState,
    SyncStatus, TrackSelection, TrackView, ASSOCIATION_CONFIDENCE, AUTO_USABLE_CONFIDENCE,
    CACHE_MAX_AGE_SECS, COLLECTION_PAGE_SIZE, CONFIRM_CONFIDENCE, COVER_CACHE_CONTROL,
    DEFAULT_PAGE_SIZE, MAX_ASSOCIATIONS, MAX_MATCH_BODY, MAX_PAGE_SIZE, NOT_IN_COLLECTION,
    QUALITY_NOTE, SEARCH_CONFIDENCE_CAP, UNITE_ENTRY_POINT,
};
pub use waveform::{
    spawn_waveform_worker, WaveformRegistry, WaveformSnapshot, WaveformState,
};
