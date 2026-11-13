//! The device's client for the store tap flow (BCS_07).
//!
//! The engine is a player: it never fetches anything. The **UI** asks the local server, over
//! the server's own HTTP surface on the loopback interface, and renders the answer. This module
//! holds the request/response vocabulary and the one implementation that talks to the server,
//! plus the error shape the screens turn into a blocked state.
//!
//! Three rules:
//!
//! * **No store credentials, ever.** The device never sends a store sign-in: the server holds
//!   the store connection. A purchase link is the canonical Bandcamp page, opened in the buyer's
//!   own browser; Plippa never touches payment. What the device does send (SECFIX_02) is the
//!   per-boot **control token** the server wrote to the private runtime directory: it is how the
//!   loopback control listener knows the caller is this device's own UI. The token is read from
//!   its file, held in memory, re-read once when the server answers `401` (it restarted and made
//!   a new one), and never logged. It is not the web portal's PIN; the device never needs that.
//! * **A store path is bounded.** Every call has a connect and read timeout and a byte cap, so
//!   a wedged server cannot hang the UI; the worker thread (not the frame loop) does the I/O.
//! * **Failures are states, not codes.** [`StoreApiError`] is mapped to the screen's blocked
//!   states with plain sentences.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::analysis::{parse_analysis, AnalysisKind, AnalysisKindStatus};

/// Connect timeout for a loopback call: the server is local, so this is generous.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1_500);
/// Read/write timeout for one call.
const IO_TIMEOUT: Duration = Duration::from_secs(4);
/// Most answer bytes accepted (a collection page is far smaller than this).
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Most a single request body may carry.
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// Why a store call failed, in the terms the screen speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreApiError {
    /// The server could not be reached (service down, Wi-Fi off, wrong address).
    Offline(String),
    /// The server answered with an error status, its stable code (when it sent one) and a
    /// message. The code is what separates "you are playing this" from "confirm first".
    Refused {
        status: u16,
        code: Option<String>,
        message: String,
    },
    /// The answer was not what this build understands.
    Malformed(String),
}

impl StoreApiError {
    /// True when retrying could plausibly work (the service may be coming up).
    pub fn retryable(&self) -> bool {
        matches!(self, StoreApiError::Offline(_))
    }

    /// The HTTP status of a refusal, when the call got that far.
    pub fn status(&self) -> Option<u16> {
        match self {
            StoreApiError::Refused { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The server's stable code (`playing`, `confirm_required`, `blocked`, `last_file`…).
    pub fn code(&self) -> Option<&str> {
        match self {
            StoreApiError::Refused { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// The human sentence the server sent (never a code).
    pub fn message(&self) -> String {
        match self {
            StoreApiError::Offline(why) => why.clone(),
            StoreApiError::Refused { message, .. } => message.clone(),
            StoreApiError::Malformed(why) => why.clone(),
        }
    }
}

impl std::fmt::Display for StoreApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreApiError::Offline(why) => write!(f, "store service unreachable: {why}"),
            StoreApiError::Refused {
                status,
                code,
                message,
            } => match code {
                Some(code) => write!(f, "store service refused ({status} {code}): {message}"),
                None => write!(f, "store service refused ({status}): {message}"),
            },
            StoreApiError::Malformed(why) => write!(f, "unexpected store answer: {why}"),
        }
    }
}

/// What the device should show after a tap (mirrors the server's `state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TapState {
    #[default]
    NotFound,
    OwnedDownloadable,
    NotOwned,
    Ambiguous,
    /// The tag is a Noubin page whose store link has to be read from the page first.
    LookupConsent,
}

/// Why the lookup could not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapBlocked {
    /// The person has not allowed web lookups yet.
    ConsentNeeded,
    /// No store is connected to this Plippa.
    NoStore,
    /// The store service (or the network under it) could not be reached.
    Offline,
    /// The store refused the stored credentials.
    CredentialsRejected,
    /// Web lookups are switched off on this Plippa.
    WebUnavailable,
    /// The store is connected but this library has never synced the collection (a new
    /// stick, a first boot, or the cache was removed): the fix is a resync, not a retry.
    NoStoreCache,
}

/// One possible release the person may pick.
#[derive(Debug, Clone, PartialEq)]
pub struct TapCandidate {
    pub store_release_id: String,
    pub title: String,
    pub artist: String,
    pub track_count: Option<u32>,
    pub confidence: f32,
}

/// The release a tap answer resolves to: the best candidate, else the owned library item.
#[derive(Debug, Clone, PartialEq)]
pub struct StoreRelease {
    pub store_release_id: String,
    pub title: String,
    pub artist: String,
    pub track_count: Option<u32>,
}

/// The tap answer as the screen needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct TapAnswer {
    pub state: TapState,
    pub blocked: Option<TapBlocked>,
    pub candidates: Vec<TapCandidate>,
    /// The release to download (best candidate, else the owned item). `None` when the answer
    /// names none, which means Download cannot be offered.
    pub release: Option<StoreRelease>,
    /// Library-relative `.noudata` of the owned item, when the answer names one.
    pub owned_path: Option<String>,
    /// Canonical purchase page; the QR is built from this and it is shown as short text.
    pub purchase_url: Option<String>,
    /// A short sentence from the server (already human, never a code).
    pub message: String,
    /// The collection cache is stale, so a miss may be a false miss.
    pub cache_stale: bool,
    /// The top candidate is in the confirmation band.
    pub requires_confirmation: bool,
    /// The top candidate's confidence (0..=1).
    pub confidence: f32,
    /// What reading the tag's own page would contact, when the answer is `lookup_consent`.
    pub lookup: Option<LookupConsentInfo>,
}

/// The host a tap would contact to read the tag's release page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupConsentInfo {
    /// Host named in the consent sentence (`noubin.com`).
    pub host: String,
    /// The page the lookup would fetch.
    pub fetch_url: String,
    /// The sentence the screen may show verbatim.
    pub consent_message: String,
}

impl TapAnswer {
    /// A blocked answer, built by the UI when the call itself failed.
    pub fn blocked(blocked: TapBlocked, message: impl Into<String>) -> Self {
        Self {
            state: TapState::NotFound,
            blocked: Some(blocked),
            candidates: Vec::new(),
            release: None,
            owned_path: None,
            purchase_url: None,
            message: message.into(),
            cache_stale: false,
            requires_confirmation: false,
            confidence: 0.0,
            lookup: None,
        }
    }
}

/// A download the server has priced (nothing fetched yet).
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadConsent {
    pub consent_id: String,
    pub tracks: usize,
    pub estimated_bytes: Option<u64>,
    /// The lossy-stream note the server insists on showing.
    pub note: String,
}

/// A started download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadSession {
    pub session: String,
}

/// State of a download session (`lowercase` on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    Running,
    Done,
    /// Some tracks arrived, some did not: never shown as "done".
    Partial,
    Cancelled,
    Failed,
}

/// Progress of a download session, as the progress screen paints it.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadProgress {
    pub state: DownloadState,
    pub tracks_done: usize,
    pub tracks_total: usize,
    pub done_bytes: u64,
    pub total_bytes: u64,
    /// Files placed in the library (0 until the hand-off, and still 0 when a replay found
    /// the release already there).
    pub placed_files: usize,
    /// The hand-off found the release already in the library: nothing was written, but the
    /// item is there and the screen must not report a failure.
    pub already_present: bool,
    /// Library-relative `.noudata` that was written, when the release was placed.
    pub noudata: Option<String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl DownloadProgress {
    /// A session that has just started: no bytes and no tracks yet.
    pub fn queued() -> Self {
        Self {
            state: DownloadState::Running,
            tracks_done: 0,
            tracks_total: 0,
            done_bytes: 0,
            total_bytes: 0,
            placed_files: 0,
            already_present: false,
            noudata: None,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// The session reached a state the screen must stop polling for.
    pub fn finished(&self) -> bool {
        !matches!(self.state, DownloadState::Running)
    }
}

// ---------------------------------------------------------------------------
// BCS_08: the portal address, the store status, browsing and delete
// ---------------------------------------------------------------------------

/// `GET /api/portal/url` as the connect screen needs it.
///
/// [`Self::recommended`] is the only address that may become a QR code: the server never
/// recommends a loopback or link-local address, and an address carrying a user name, query
/// or token is refused before it ever reaches this struct.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PortalAddress {
    /// The address to open on the phone.
    pub recommended: Option<String>,
    /// Every candidate the server found (recommended first), for the text line.
    pub candidates: Vec<String>,
    /// The portal's listening port.
    pub port: u16,
    /// Why nothing is reachable, in the server's words.
    pub note: Option<String>,
    /// The web portal is switched off: there is nothing for a phone to open (SECFIX_02).
    pub portal_off: bool,
}

/// The web portal's switches as the device menu shows and changes them (`GET/POST
/// /api/portal/settings` on the control listener). Only the device ever sees the PIN.
#[derive(Clone, PartialEq, Eq)]
pub struct PortalSettingsInfo {
    /// The server can keep the portal's state (so it can be turned on).
    pub available: bool,
    /// Why not, in the server's words.
    pub unavailable_reason: Option<String>,
    /// The portal is switched on.
    pub enabled: bool,
    /// SECFIX_03: the portal serves HTTPS (`true`, the default) rather than plain HTTP.
    pub https_enabled: bool,
    /// Browsers must sign in with the PIN.
    pub pin_required: bool,
    /// The six digits, once the portal has been switched on for the first time (or the owner
    /// typed one).
    pub pin: Option<String>,
    /// A sign-in lockout is in force.
    pub locked: bool,
    /// The LAN socket is open right now.
    pub listening: bool,
    /// Certificate fingerprint of the portal's HTTPS listener, when it has one (SECFIX_03).
    pub fingerprint: Option<String>,
    /// The last change could not be written to the state file yet (it is retried).
    pub unsaved: Option<String>,
    /// The portal is on but its socket could not be opened, and why.
    pub listen_error: Option<String>,
}

impl Default for PortalSettingsInfo {
    fn default() -> Self {
        // SECFIX_03: HTTPS is the factory default and what an older state file reads as.
        Self {
            available: false,
            unavailable_reason: None,
            enabled: false,
            https_enabled: true,
            pin_required: true,
            pin: None,
            locked: false,
            listening: false,
            fingerprint: None,
            unsaved: None,
            listen_error: None,
        }
    }
}

impl std::fmt::Debug for PortalSettingsInfo {
    // The PIN is for the owner's eyes on the panel, not for a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalSettingsInfo")
            .field("available", &self.available)
            .field("enabled", &self.enabled)
            .field("https_enabled", &self.https_enabled)
            .field("pin_required", &self.pin_required)
            .field("pin", &self.pin.as_ref().map(|_| "<set>"))
            .field("locked", &self.locked)
            .field("listening", &self.listening)
            .finish_non_exhaustive()
    }
}

impl PortalAddress {
    /// A short address to retype (`192.168.1.42:8787/stores`): scheme dropped, nothing added.
    pub fn short(&self) -> Option<String> {
        self.recommended.as_deref().map(short_address)
    }
}

/// One store as `GET /api/stores` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoreInfo {
    /// Slug (`bandcamp`).
    pub id: String,
    /// Name for the screen.
    pub name: String,
    /// Credentials are stored and were not refused by the last check.
    pub connected: bool,
    /// The stored sign-in was refused by the store (a fresh connect fixes it).
    pub rejected: bool,
    /// Releases in the cached collection.
    pub cached_releases: u32,
    /// The cache is a day old, incomplete or absent.
    pub cache_stale: bool,
}

/// `GET /api/stores` as the Stores screen needs it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoresInfo {
    /// A usable secrets directory exists, so a connection can be stored at all.
    pub secrets_available: bool,
    /// Why not, when it cannot.
    pub secrets_reason: Option<String>,
    pub stores: Vec<StoreInfo>,
}

impl StoresInfo {
    /// The Bandcamp row, when this build knows the store.
    pub fn bandcamp(&self) -> Option<&StoreInfo> {
        self.stores.iter().find(|s| s.id == "bandcamp")
    }

    /// Any store holds a connection that was not refused.
    pub fn connected(&self) -> bool {
        self.stores.iter().any(|s| s.connected && !s.rejected)
    }
}

/// One release in the collection list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReleaseRow {
    /// Store id, URL-encoded when a detail path is built.
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<u16>,
    pub track_count: Option<u32>,
    pub duration_s: Option<u64>,
}

/// `GET /api/stores/<store>/collection` — one page plus how much there is in total.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CollectionPageInfo {
    /// Releases matching the query (not just this page).
    pub total: u32,
    /// Offset this page was requested at.
    pub offset: u32,
    /// The cache is stale, so a short list may be a lie.
    pub stale: bool,
    /// When the cache was last written (Unix seconds); `None` when it never was.
    pub synced_at: Option<u64>,
    pub releases: Vec<ReleaseRow>,
}

impl CollectionPageInfo {
    /// Offset of the page after this one, or `None` on the last page.
    pub fn next_offset(&self) -> Option<u32> {
        let loaded = self.offset.saturating_add(self.releases.len() as u32);
        (loaded < self.total).then_some(loaded)
    }

    /// The page has rows and is the last one.
    pub fn is_last_page(&self) -> bool {
        self.next_offset().is_none()
    }
}

/// One track of a release, as the detail view lists it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReleaseTrackInfo {
    pub title: String,
    /// 1-based track number within its disc.
    pub track: Option<u32>,
    pub duration_s: Option<u64>,
    /// Declared size of the advertised file (never the stream's).
    pub declared_bytes: Option<u64>,
    /// Container the store advertises (`mp3`, `flac`…).
    pub suffix: Option<String>,
}

/// `GET /api/stores/<store>/release/<id>` as the detail screen needs it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReleaseInfo {
    pub release: ReleaseRow,
    pub tracks: Vec<ReleaseTrackInfo>,
    /// What the bytes actually are: every source this store serves today is lossy.
    pub lossy: bool,
    /// The server's sentence about the audio, shown before a download starts.
    pub quality_note: String,
    /// Sum of the declared track sizes, when the store declared them.
    pub declared_bytes: Option<u64>,
    /// The release was already in the cached collection.
    pub cached: bool,
}

impl ReleaseInfo {
    /// Title plus artist for a one-line row.
    pub fn title_line(&self) -> String {
        if self.release.artist.is_empty() {
            self.release.title.clone()
        } else {
            format!("{} — {}", self.release.title, self.release.artist)
        }
    }
}

/// `GET /api/{playables,collections}/<id>/delete/preview` as the confirm screen needs it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeletePreviewInfo {
    pub title: String,
    /// Media files that would be removed.
    pub files: usize,
    /// Everything the delete reclaims, in bytes.
    pub total_bytes: u64,
    /// Reasons the delete is refused; non-empty means nothing will be deleted.
    pub blockers: Vec<String>,
    /// Things that will be lost or left behind.
    pub warnings: Vec<String>,
    /// The item is playing right now.
    pub playing: bool,
    /// Playlists whose item list will be edited.
    pub playlists: Vec<String>,
    /// The `.noudata` that goes with the files.
    pub noudata: Option<String>,
}

impl DeletePreviewInfo {
    /// Nothing can be deleted until the blocker is cleared.
    pub fn blocked(&self) -> bool {
        !self.blockers.is_empty()
    }
}

/// `200` from a confirmed delete.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeleteDone {
    /// Library-relative paths removed.
    pub deleted: usize,
    pub freed_bytes: u64,
    /// Some files could not be removed (the rest is in the server's answer).
    pub partial: bool,
}

/// Largest cover the server will send (its own cap); the raw read allows this plus headers.
pub const MAX_COVER_BYTES: usize = 8 * 1024 * 1024;
/// Headroom over [`MAX_COVER_BYTES`] for the response head.
const COVER_HEAD_ALLOWANCE: usize = 4 * 1024;

/// A release cover as the server sent it: the declared type and the raw image bytes. Decoding
/// (and its limits) belong to the caller; this only carries what arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverBytes {
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// Where a collection sync is (`GET /api/stores/bandcamp/sync`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncPhase {
    /// Nothing has run since the service started (or the last run was cancelled).
    #[default]
    Idle,
    Running,
    Done,
    Failed,
}

impl SyncPhase {
    /// No further progress will come without a new start.
    pub fn terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// The server's sync status, parsed from its camelCase fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncStatus {
    pub state: SyncPhase,
    /// Pages fetched so far.
    pub pages: u32,
    /// Releases counted so far.
    pub releases: u32,
    /// What went wrong, in words.
    pub error: Option<String>,
    pub error_code: Option<String>,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    /// The last run was stopped by a cancel.
    pub cancelled: bool,
}

/// Everything the UI needs from the server for one tap.
pub trait StoreApi: Send {
    /// `consent` is `true` only after the person agreed to contact the tag's own page. A
    /// Bandcamp link on the tag needs no consent and ignores it.
    fn tap(
        &mut self,
        url: &str,
        expected_duration_s: Option<u64>,
        consent: bool,
    ) -> Result<TapAnswer, StoreApiError>;
    fn want(&mut self, url: &str) -> Result<(), StoreApiError>;
    fn download_prepare(
        &mut self,
        noubin_key: &str,
        release_id: &str,
    ) -> Result<DownloadConsent, StoreApiError>;
    fn download_start(&mut self, consent_id: &str) -> Result<DownloadSession, StoreApiError>;
    fn download_status(&mut self, session: &str) -> Result<DownloadProgress, StoreApiError>;
    fn download_cancel(&mut self, session: &str) -> Result<(), StoreApiError>;

    // ---- BCS_08: connect by QR, browse the collection, remove from the device ----------

    /// The portal address a phone can open (`GET /api/portal/url`).
    fn portal_address(&mut self) -> Result<PortalAddress, StoreApiError>;

    /// Whether a store is connected (`GET /api/stores`).
    fn stores(&mut self) -> Result<StoresInfo, StoreApiError>;

    /// One page of the Bandcamp collection (`?query=&offset=&size=`). An empty `query` is
    /// the whole collection; anything else is the server's search.
    fn collection(
        &mut self,
        query: &str,
        offset: u32,
        size: u32,
    ) -> Result<CollectionPageInfo, StoreApiError>;

    /// One release with its track list and the quality note.
    fn release(&mut self, release_id: &str) -> Result<ReleaseInfo, StoreApiError>;

    /// What removing `playable_id` (`encode_playable_id`-encoded) would delete.
    fn delete_preview(&mut self, playable_id: u64) -> Result<DeletePreviewInfo, StoreApiError>;

    /// Remove the playable from this device (`DELETE /api/playables/<id>` with `force`).
    fn delete_playable(&mut self, playable_id: u64) -> Result<DeleteDone, StoreApiError>;

    // ---- BCS_11: resync from the device, show the cover ----------------------------------

    /// Start a collection sync (`POST /api/stores/bandcamp/sync`).
    fn sync_start(&mut self) -> Result<SyncStatus, StoreApiError>;

    /// Where the sync is (`GET /api/stores/bandcamp/sync`). Sends no secrets.
    fn sync_status(&mut self) -> Result<SyncStatus, StoreApiError>;

    /// Stop the running sync (`POST /api/stores/bandcamp/sync/cancel`).
    fn sync_cancel(&mut self) -> Result<SyncStatus, StoreApiError>;

    /// The release's cover image (`GET /api/stores/bandcamp/release/<id>/cover`).
    fn release_cover(&mut self, release_id: &str) -> Result<CoverBytes, StoreApiError>;

    // ---- TRACK_01: analysis status and priority re-analysis ------------------------------

    /// What has been analysed for media item `item` of `playable_id`
    /// (`GET /api/waveforms/<id>`, the answer narrowed to that item).
    fn waveform_status(
        &mut self,
        playable_id: u64,
        item: u32,
    ) -> Result<Vec<AnalysisKindStatus>, StoreApiError>;

    /// Put media item `item` of `playable_id` at the front of the analysis queue
    /// (`POST /api/media/<id>/analysis/reanalyse`). `kind` names one analysis kind; `None`
    /// re-analyses all of them. A missing media file is refused with the code `unavailable`.
    fn reanalyse(
        &mut self,
        playable_id: u64,
        item: u32,
        kind: Option<AnalysisKind>,
    ) -> Result<(), StoreApiError>;

    // ---- SECFIX_02: the web portal's switches --------------------------------------------

    /// The portal's current settings (`GET /api/portal/settings`).
    fn portal_settings(&mut self) -> Result<PortalSettingsInfo, StoreApiError>;

    /// Switch the portal on or off. The first time it goes on the server generates the PIN.
    fn set_portal_enabled(&mut self, enabled: bool) -> Result<PortalSettingsInfo, StoreApiError>;

    /// SECFIX_03: serve the LAN portal over HTTPS (`true`, the default) or plain HTTP (`false`).
    /// The certificate and key are kept across an Off/On cycle, so the fingerprint does not
    /// change. Flipping this drops every browser's connection.
    fn set_https_enabled(&mut self, on: bool) -> Result<PortalSettingsInfo, StoreApiError>;

    /// SECFIX_03: make a new certificate and key. The fingerprint changes and every browser
    /// must accept the new certificate.
    fn regenerate_certificate(&mut self) -> Result<PortalSettingsInfo, StoreApiError>;

    /// Ask browsers for the PIN (`true`) or serve with no sign-in (`false`).
    fn set_pin_required(&mut self, required: bool) -> Result<PortalSettingsInfo, StoreApiError>;

    /// The owner typed a new six-digit PIN. Every signed-in browser is signed out.
    fn set_pin(&mut self, pin: &str) -> Result<PortalSettingsInfo, StoreApiError>;

    /// Forget every sign-in lockout (the owner is at the device).
    fn clear_lockout(&mut self) -> Result<PortalSettingsInfo, StoreApiError>;
}

/// The real client: HTTP/1.1 on the loopback interface, JSON, no keep-alive.
pub struct HttpStoreApi {
    base: String,
    addr: Option<SocketAddr>,
    /// Where the server writes its per-boot control token (`<ipc dir>/portal.token`). `None`
    /// sends no token (a test server, or a service that does not ask for one).
    token_path: Option<PathBuf>,
    /// The token as last read; replaced when the server answers `401`.
    token: Mutex<Option<String>>,
}

impl HttpStoreApi {
    /// `base` is `host:port` (`127.0.0.1:8786`), also accepted with a scheme prefix. No control
    /// token is sent.
    pub fn new(base: impl Into<String>) -> Self {
        let base = normalise_base(&base.into());
        let addr = base.to_socket_addrs().ok().and_then(|mut a| a.next());
        Self {
            base,
            addr,
            token_path: None,
            token: Mutex::new(None),
        }
    }

    /// [`HttpStoreApi::new`] for the server's control listener: every request carries the token
    /// read from `token_path`, and a `401` makes the client read it once more and retry once.
    pub fn with_token_file(base: impl Into<String>, token_path: impl Into<PathBuf>) -> Self {
        let mut api = Self::new(base);
        api.token_path = Some(token_path.into());
        api
    }

    /// The token from its file, cached. `None` when the file is absent or unusable (the server
    /// has not started yet): the request then goes without one and the server's `401` is the
    /// honest answer.
    fn current_token(&self, refresh: bool) -> Option<String> {
        let path = self.token_path.as_ref()?;
        let mut cached = self.token.lock().unwrap_or_else(|p| p.into_inner());
        if refresh || cached.is_none() {
            *cached = read_token_file(path);
        }
        cached.clone()
    }

    /// The `host:port` this client talks to (for diagnostics).
    pub fn base(&self) -> &str {
        &self.base
    }

    fn call(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<Value, StoreApiError> {
        let reply = self.exchange(method, path, body, "application/json", MAX_RESPONSE_BYTES)?;
        let body = reply.body.as_slice();
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(body)
            .map_err(|e| StoreApiError::Malformed(format!("answer is not JSON ({e})")))
    }

    /// A GET whose answer is not JSON (an image). The cap covers head and body together, so a
    /// larger answer is refused outright rather than truncated into a partial picture.
    fn call_raw(
        &self,
        path: &str,
        accept: &str,
        cap: usize,
        too_large: &str,
    ) -> Result<CoverBytes, StoreApiError> {
        let reply = self
            .exchange("GET", path, None, accept, cap)
            .map_err(|error| match error {
                StoreApiError::Malformed(m) if m == "answer too large" => {
                    StoreApiError::Malformed(too_large.to_string())
                }
                other => other,
            })?;
        Ok(CoverBytes {
            content_type: reply.content_type.unwrap_or_default(),
            bytes: reply.body,
        })
    }

    /// One request/response round trip. A non-2xx status is turned into
    /// [`StoreApiError::Refused`] here, so callers only see successful bodies.
    ///
    /// With a token file configured the control token is sent; a `401` means the server
    /// restarted and wrote a new one, so the file is read again and the request is made once
    /// more (never a loop).
    fn exchange(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        accept: &str,
        cap: usize,
    ) -> Result<Reply, StoreApiError> {
        let token = self.current_token(false);
        match self.exchange_with(token.as_deref(), method, path, body, accept, cap) {
            Err(StoreApiError::Refused { status: 401, .. }) if self.token_path.is_some() => {
                let fresh = self.current_token(true);
                self.exchange_with(fresh.as_deref(), method, path, body, accept, cap)
            }
            other => other,
        }
    }

    fn exchange_with(
        &self,
        token: Option<&str>,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        accept: &str,
        cap: usize,
    ) -> Result<Reply, StoreApiError> {
        let addr = self
            .addr
            .ok_or_else(|| StoreApiError::Offline(format!("{} is not an address", self.base)))?;
        let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
            .map_err(|e| StoreApiError::Offline(e.to_string()))?;
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: {accept}\r\n",
            self.base
        );
        if let Some(token) = token {
            head.push_str(&format!("X-Plippa-Local: {token}\r\n"));
        }
        if let Some(body) = body {
            if body.len() > MAX_REQUEST_BYTES {
                return Err(StoreApiError::Malformed("request body too large".into()));
            }
            head.push_str("Content-Type: application/json\r\n");
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .and_then(|_| match body {
                Some(body) => stream.write_all(body),
                None => Ok(()),
            })
            .and_then(|_| stream.flush())
            .map_err(|e| StoreApiError::Offline(e.to_string()))?;

        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if raw.len() > cap {
                        return Err(StoreApiError::Malformed("answer too large".into()));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(StoreApiError::Offline(e.to_string())),
            }
        }

        let (head, body) = split_response(&raw)?;
        let status = parse_status(head)?;
        if !(200..300).contains(&status) {
            let value = serde_json::from_slice::<Value>(body).ok();
            let detail = value
                .as_ref()
                .and_then(|v| {
                    v.get("error")
                        .or_else(|| v.get("message"))
                        .and_then(|m| m.as_str().map(str::to_string))
                })
                .unwrap_or_else(|| "the store service refused the request".to_string());
            let code = value
                .as_ref()
                .and_then(|v| v.get("code"))
                .and_then(Value::as_str)
                .map(str::to_string);
            return Err(StoreApiError::Refused {
                status,
                code,
                message: detail,
            });
        }
        Ok(Reply {
            content_type: header_value(head, "content-type"),
            body: body.to_vec(),
        })
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value, StoreApiError> {
        let bytes = serde_json::to_vec(body)
            .map_err(|e| StoreApiError::Malformed(format!("cannot encode request ({e})")))?;
        self.call("POST", path, Some(&bytes))
    }
}

/// A successful answer: the declared media type and the body bytes.
struct Reply {
    content_type: Option<String>,
    body: Vec<u8>,
}

/// The value of response header `name` (case-insensitive), without parameters trimmed away.
fn header_value(head: &[u8], name: &str) -> Option<String> {
    let text = std::str::from_utf8(head).ok()?;
    text.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

impl StoreApi for HttpStoreApi {
    fn tap(
        &mut self,
        url: &str,
        expected_duration_s: Option<u64>,
        consent: bool,
    ) -> Result<TapAnswer, StoreApiError> {
        let mut body = serde_json::json!({ "url": url, "consent": consent });
        if let Some(seconds) = expected_duration_s {
            body["expectedDuration"] = serde_json::json!(seconds);
        }
        let answer = self.post("/api/stores/tap", &body)?;
        parse_tap_answer(&answer)
    }

    fn want(&mut self, url: &str) -> Result<(), StoreApiError> {
        self.post("/api/stores/bandcamp/want", &serde_json::json!({ "url": url }))?;
        Ok(())
    }

    fn download_prepare(
        &mut self,
        noubin_key: &str,
        release_id: &str,
    ) -> Result<DownloadConsent, StoreApiError> {
        let answer = self.post(
            "/api/stores/bandcamp/download/prepare",
            &serde_json::json!({ "releases": [release_id], "noubinKey": noubin_key }),
        )?;
        parse_consent(&answer)
    }

    fn download_start(&mut self, consent_id: &str) -> Result<DownloadSession, StoreApiError> {
        let answer = self.post(
            "/api/stores/bandcamp/download",
            &serde_json::json!({ "consent": true, "consentId": consent_id }),
        )?;
        let session = answer
            .get("session")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| StoreApiError::Malformed("no download session".into()))?;
        Ok(DownloadSession {
            session: session.to_string(),
        })
    }

    fn download_status(&mut self, session: &str) -> Result<DownloadProgress, StoreApiError> {
        let answer = self.call(
            "GET",
            &format!("/api/stores/bandcamp/download/{}", encode_path(session)),
            None,
        )?;
        parse_progress(&answer)
    }

    fn download_cancel(&mut self, session: &str) -> Result<(), StoreApiError> {
        self.post(
            &format!("/api/stores/bandcamp/download/{}/cancel", encode_path(session)),
            &Value::Null,
        )?;
        Ok(())
    }

    fn portal_address(&mut self) -> Result<PortalAddress, StoreApiError> {
        let answer = self.call("GET", "/api/portal/url", None)?;
        parse_portal_address(&answer)
    }

    fn stores(&mut self) -> Result<StoresInfo, StoreApiError> {
        let answer = self.call("GET", "/api/stores", None)?;
        parse_stores(&answer)
    }

    fn collection(
        &mut self,
        query: &str,
        offset: u32,
        size: u32,
    ) -> Result<CollectionPageInfo, StoreApiError> {
        let size = size.clamp(1, 200);
        let mut path = format!("/api/stores/bandcamp/collection?offset={offset}&size={size}");
        let query = query.trim();
        if !query.is_empty() {
            path.push_str("&query=");
            path.push_str(&encode_path(query));
        }
        let answer = self.call("GET", &path, None)?;
        parse_collection(&answer)
    }

    fn release(&mut self, release_id: &str) -> Result<ReleaseInfo, StoreApiError> {
        let path = format!("/api/stores/bandcamp/release/{}", encode_path(release_id));
        match self.call("GET", &path, None) {
            Ok(answer) => parse_release(&answer),
            // SRV-06: the GET is cache-only, so an uncached release answers `not_cached`.
            // One JSON POST is the consented lookup (the server writes the cache).
            Err(error) if error.code() == Some("not_cached") => {
                let body = serde_json::json!({ "id": release_id });
                let answer = self.post("/api/stores/bandcamp/release", &body)?;
                parse_release(&answer)
            }
            Err(error) => Err(error),
        }
    }

    fn delete_preview(&mut self, playable_id: u64) -> Result<DeletePreviewInfo, StoreApiError> {
        let answer = self.call(
            "GET",
            &format!("/api/playables/{playable_id}/delete/preview"),
            None,
        )?;
        parse_delete_preview(&answer)
    }

    fn delete_playable(&mut self, playable_id: u64) -> Result<DeleteDone, StoreApiError> {
        // `force` is the confirmation: the person saw the preview and pressed Remove.
        let answer = self.call(
            "DELETE",
            &format!("/api/playables/{playable_id}"),
            Some(b"{\"force\":true}"),
        )?;
        parse_delete_done(&answer)
    }

    fn sync_start(&mut self) -> Result<SyncStatus, StoreApiError> {
        let answer = self.post("/api/stores/bandcamp/sync", &Value::Null)?;
        parse_sync_status(&answer)
    }

    fn sync_status(&mut self) -> Result<SyncStatus, StoreApiError> {
        let answer = self.call("GET", "/api/stores/bandcamp/sync", None)?;
        parse_sync_status(&answer)
    }

    fn sync_cancel(&mut self) -> Result<SyncStatus, StoreApiError> {
        let answer = self.post("/api/stores/bandcamp/sync/cancel", &Value::Null)?;
        parse_sync_status(&answer)
    }

    fn waveform_status(
        &mut self,
        playable_id: u64,
        item: u32,
    ) -> Result<Vec<AnalysisKindStatus>, StoreApiError> {
        let answer = self.call("GET", &format!("/api/waveforms/{playable_id}"), None)?;
        parse_analysis(&answer, item)
    }

    fn reanalyse(
        &mut self,
        playable_id: u64,
        item: u32,
        kind: Option<AnalysisKind>,
    ) -> Result<(), StoreApiError> {
        let mut body = serde_json::json!({ "item": item });
        if let Some(kind) = kind {
            body["kinds"] = serde_json::json!([kind.api_name()]);
        }
        self.post(
            &format!("/api/media/{playable_id}/analysis/reanalyse"),
            &body,
        )?;
        Ok(())
    }

    fn release_cover(&mut self, release_id: &str) -> Result<CoverBytes, StoreApiError> {
        self.call_raw(
            &format!("/api/stores/bandcamp/release/{}/cover", encode_path(release_id)),
            "image/*",
            MAX_COVER_BYTES + COVER_HEAD_ALLOWANCE,
            "cover too large",
        )
    }

    fn portal_settings(&mut self) -> Result<PortalSettingsInfo, StoreApiError> {
        let answer = self.call("GET", "/api/portal/settings", None)?;
        parse_portal_settings(&answer)
    }

    fn set_portal_enabled(&mut self, enabled: bool) -> Result<PortalSettingsInfo, StoreApiError> {
        let answer = self.post("/api/portal/settings", &serde_json::json!({ "enabled": enabled }))?;
        parse_portal_settings(&answer)
    }

    fn set_https_enabled(&mut self, on: bool) -> Result<PortalSettingsInfo, StoreApiError> {
        let answer = self.post(
            "/api/portal/settings",
            &serde_json::json!({ "https_enabled": on }),
        )?;
        parse_portal_settings(&answer)
    }

    fn regenerate_certificate(&mut self) -> Result<PortalSettingsInfo, StoreApiError> {
        let answer = self.post(
            "/api/portal/settings",
            &serde_json::json!({ "regenerate_certificate": true }),
        )?;
        parse_portal_settings(&answer)
    }

    fn set_pin_required(&mut self, required: bool) -> Result<PortalSettingsInfo, StoreApiError> {
        let answer = self.post(
            "/api/portal/settings",
            &serde_json::json!({ "pin_required": required }),
        )?;
        parse_portal_settings(&answer)
    }

    fn set_pin(&mut self, pin: &str) -> Result<PortalSettingsInfo, StoreApiError> {
        let answer = self.post("/api/portal/settings", &serde_json::json!({ "pin": pin }))?;
        parse_portal_settings(&answer)
    }

    fn clear_lockout(&mut self) -> Result<PortalSettingsInfo, StoreApiError> {
        let answer = self.post(
            "/api/portal/settings",
            &serde_json::json!({ "clear_lockout": true }),
        )?;
        parse_portal_settings(&answer)
    }
}

/// Read the control token: a short run of letters and digits, nothing else. A symlink, a huge
/// file or anything that is not token-shaped is no token.
fn read_token_file(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let mut text = String::new();
    Read::take(&mut file, 257).read_to_string(&mut text).ok()?;
    let token = text.trim();
    let shaped = (16..=256).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric());
    shaped.then(|| token.to_string())
}

/// A client for a device with no store service configured: every call is honestly offline.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoStoreApi;

impl StoreApi for NoStoreApi {
    fn tap(
        &mut self,
        _url: &str,
        _expected: Option<u64>,
        _consent: bool,
    ) -> Result<TapAnswer, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn want(&mut self, _url: &str) -> Result<(), StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn download_prepare(
        &mut self,
        _key: &str,
        _release: &str,
    ) -> Result<DownloadConsent, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn download_start(&mut self, _consent: &str) -> Result<DownloadSession, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn download_status(&mut self, _session: &str) -> Result<DownloadProgress, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn download_cancel(&mut self, _session: &str) -> Result<(), StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn portal_address(&mut self) -> Result<PortalAddress, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn stores(&mut self) -> Result<StoresInfo, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn collection(
        &mut self,
        _query: &str,
        _offset: u32,
        _size: u32,
    ) -> Result<CollectionPageInfo, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn release(&mut self, _release_id: &str) -> Result<ReleaseInfo, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn delete_preview(&mut self, _playable_id: u64) -> Result<DeletePreviewInfo, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn delete_playable(&mut self, _playable_id: u64) -> Result<DeleteDone, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn sync_start(&mut self) -> Result<SyncStatus, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn sync_status(&mut self) -> Result<SyncStatus, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn sync_cancel(&mut self) -> Result<SyncStatus, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn release_cover(&mut self, _release_id: &str) -> Result<CoverBytes, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn waveform_status(
        &mut self,
        _playable_id: u64,
        _item: u32,
    ) -> Result<Vec<AnalysisKindStatus>, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn reanalyse(
        &mut self,
        _playable_id: u64,
        _item: u32,
        _kind: Option<AnalysisKind>,
    ) -> Result<(), StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn portal_settings(&mut self) -> Result<PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn set_portal_enabled(&mut self, _enabled: bool) -> Result<PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn set_https_enabled(&mut self, _on: bool) -> Result<PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn regenerate_certificate(&mut self) -> Result<PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn set_pin_required(&mut self, _required: bool) -> Result<PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn set_pin(&mut self, _pin: &str) -> Result<PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
    fn clear_lockout(&mut self) -> Result<PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("no store service is configured".into()))
    }
}

// ---------------------------------------------------------------------------
// Parsing (tolerant: an unknown field or a missing optional never panics)
// ---------------------------------------------------------------------------

fn parse_tap_answer(value: &Value) -> Result<TapAnswer, StoreApiError> {
    if !value.is_object() {
        return Err(StoreApiError::Malformed("the answer is not an object".into()));
    }
    let state = match value.get("state").and_then(Value::as_str) {
        Some("owned_downloadable") => TapState::OwnedDownloadable,
        Some("not_owned") => TapState::NotOwned,
        Some("ambiguous") => TapState::Ambiguous,
        Some("lookup_consent") => TapState::LookupConsent,
        Some("not_found") | Some(_) | None => TapState::NotFound,
    };
    let blocked = match value.get("blocked").and_then(Value::as_str) {
        Some("no_store") => Some(TapBlocked::NoStore),
        Some("credentials_rejected") => Some(TapBlocked::CredentialsRejected),
        Some("consent_required") => Some(TapBlocked::ConsentNeeded),
        Some("web_unavailable") => Some(TapBlocked::WebUnavailable),
        Some("offline") => Some(TapBlocked::Offline),
        Some("no_store_cache") => Some(TapBlocked::NoStoreCache),
        // A newer server may know a reason this build does not: show the one-line message
        // with a retry rather than an answer the screen cannot render.
        Some(_) => Some(TapBlocked::Offline),
        None => None,
    };
    let candidates: Vec<TapCandidate> = value
        .get("candidates")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(parse_candidate).collect())
        .unwrap_or_default();
    let matched = value.get("match");
    let cache_stale = matched
        .and_then(|m| m.get("cacheStale"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let requires_confirmation = matched
        .and_then(|m| m.get("requiresConfirmation"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let owned_path = matched
        .and_then(|m| m.get("owned"))
        .and_then(|o| o.get("noudataPath"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let confidence = matched
        .and_then(|m| m.get("confidence"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0) as f32;
    // The download target: the best candidate's id, else the owned item's recorded id.
    let release = candidates.first().map(|c| StoreRelease {
        store_release_id: c.store_release_id.clone(),
        title: c.title.clone(),
        artist: c.artist.clone(),
        track_count: c.track_count,
    });
    let release = release.or_else(|| {
        let owned = matched.and_then(|m| m.get("owned"))?;
        let id = owned.get("storeReleaseId").and_then(Value::as_str)?;
        Some(StoreRelease {
            store_release_id: id.to_string(),
            title: owned
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            artist: owned
                .get("artist")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            track_count: None,
        })
    });
    Ok(TapAnswer {
        state,
        blocked,
        candidates,
        release,
        owned_path,
        // The device offers only a canonical HTTPS page, and only one that survives the
        // shared SEC-12 strip unchanged: a bidi override or a control character in a link is
        // exactly how a URL is made to look like a different site.
        purchase_url: value
            .get("purchaseUrl")
            .and_then(Value::as_str)
            .filter(|url| {
                url.starts_with("https://")
                    && url.len() <= 512
                    && plippa_noubin::sanitise_line(url, 512) == *url
            })
            .map(str::to_string),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        cache_stale,
        requires_confirmation,
        confidence,
        lookup: value.get("lookup").and_then(|l| {
            let host = l.get("host").and_then(Value::as_str)?;
            let fetch_url = l.get("fetchUrl").and_then(Value::as_str)?;
            // Both are drawn on the screen, so both take the same SEC-12 strip.
            Some(LookupConsentInfo {
                host: plippa_noubin::sanitise_line(host, 120),
                fetch_url: plippa_noubin::sanitise_line(fetch_url, 512),
                consent_message: l
                    .get("consentMessage")
                    .and_then(Value::as_str)
                    .map(|m| plippa_noubin::sanitise_line(m, 320))
                    .unwrap_or_default(),
            })
        }),
    })
}

/// `GET /api/stores/bandcamp/sync`: unknown states read as `idle`, missing counts as zero.
fn parse_sync_status(value: &Value) -> Result<SyncStatus, StoreApiError> {
    if !value.is_object() {
        return Err(StoreApiError::Malformed("the sync status is not an object".into()));
    }
    let state = match value.get("state").and_then(Value::as_str) {
        Some("running") => SyncPhase::Running,
        Some("done") => SyncPhase::Done,
        Some("failed") => SyncPhase::Failed,
        _ => SyncPhase::Idle,
    };
    let count = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
    };
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    };
    Ok(SyncStatus {
        state,
        pages: count("pages"),
        releases: count("releases"),
        error: text("error"),
        error_code: text("errorCode"),
        started_at: value.get("startedAt").and_then(Value::as_u64),
        finished_at: value.get("finishedAt").and_then(Value::as_u64),
        cancelled: value
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn parse_candidate(value: &Value) -> Option<TapCandidate> {
    Some(TapCandidate {
        store_release_id: value.get("storeReleaseId")?.as_str()?.to_string(),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        artist: value
            .get("artist")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        track_count: value
            .get("trackCount")
            .and_then(Value::as_u64)
            .map(|n| n.min(u32::MAX as u64) as u32),
        confidence: value
            .get("confidence")
            .and_then(Value::as_f64)
            .unwrap_or(0.0) as f32,
    })
}

fn parse_consent(value: &Value) -> Result<DownloadConsent, StoreApiError> {
    let consent_id = value
        .get("consentId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| StoreApiError::Malformed("no consent id".into()))?;
    Ok(DownloadConsent {
        consent_id: consent_id.to_string(),
        tracks: value.get("tracks").and_then(Value::as_u64).unwrap_or(0) as usize,
        estimated_bytes: value.get("estimatedBytes").and_then(Value::as_u64),
        note: value
            .get("quality")
            .and_then(|q| q.get("warning"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn parse_progress(value: &Value) -> Result<DownloadProgress, StoreApiError> {
    let state = match value.get("state").and_then(Value::as_str) {
        Some("running") => DownloadState::Running,
        Some("done") => DownloadState::Done,
        Some("partial") => DownloadState::Partial,
        Some("cancelled") => DownloadState::Cancelled,
        Some("failed") => DownloadState::Failed,
        other => {
            return Err(StoreApiError::Malformed(format!(
                "unknown download state {other:?}"
            )))
        }
    };
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let tracks_done = items
        .iter()
        .filter(|i| i.get("state").and_then(Value::as_str) == Some("done"))
        .count();
    Ok(DownloadProgress {
        state,
        tracks_done,
        tracks_total: items.len(),
        done_bytes: value.get("doneBytes").and_then(Value::as_u64).unwrap_or(0),
        total_bytes: value.get("totalBytes").and_then(Value::as_u64).unwrap_or(0),
        placed_files: value
            .get("placedFiles")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        already_present: value
            .get("unite")
            .and_then(|u| u.get("state"))
            .and_then(Value::as_str)
            == Some("exists"),
        noudata: value
            .get("unite")
            .and_then(|u| u.get("noudata"))
            .and_then(Value::as_str)
            .map(str::to_string),
        errors: string_list(value.get("errors")),
        warnings: string_list(value.get("warnings")),
    })
}

/// `http://host:port` → `host:port`, trimmed of a trailing slash and anything after it.
///
/// Used for the address line under the connect QR: the person retypes this, so it is only
/// ever the host and port the server recommended.
pub fn short_address(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    without_scheme.trim_end_matches('/').to_string()
}

/// A store-provided address the device may show: absolute http(s), no user information, no
/// query, no fragment, and nothing the shared SEC-12 stripper would have to remove.
fn displayable_address(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && url.len() <= 512
        && !url.contains('@')
        && !url.contains('?')
        && !url.contains('#')
        && plippa_noubin::sanitise_line(url, 512) == url
}

fn parse_portal_address(value: &Value) -> Result<PortalAddress, StoreApiError> {
    if !value.is_object() {
        return Err(StoreApiError::Malformed(
            "the portal answer is not an object".into(),
        ));
    }
    let recommended = value
        .get("recommended")
        .and_then(Value::as_str)
        .filter(|url| displayable_address(url))
        .map(str::to_string);
    let candidates: Vec<String> = value
        .get("urls")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| entry.get("url").and_then(Value::as_str))
                .filter(|url| displayable_address(url))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(PortalAddress {
        recommended,
        candidates,
        port: value.get("port").and_then(Value::as_u64).unwrap_or(0) as u16,
        note: value
            .get("note")
            .and_then(Value::as_str)
            .map(str::to_string),
        // An older server has no switch: treat the portal as on, as it was.
        portal_off: value.get("portalEnabled").and_then(Value::as_bool) == Some(false),
    })
}

fn parse_portal_settings(value: &Value) -> Result<PortalSettingsInfo, StoreApiError> {
    if !value.is_object() {
        return Err(StoreApiError::Malformed(
            "the portal settings answer is not an object".into(),
        ));
    }
    let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|t| !t.is_empty())
    };
    // The PIN is six digits or nothing: anything else is not shown as a PIN.
    let pin = text("pin").filter(|p| p.len() == 6 && p.bytes().all(|b| b.is_ascii_digit()));
    Ok(PortalSettingsInfo {
        available: flag("available"),
        unavailable_reason: text("unavailable_reason"),
        enabled: flag("enabled"),
        // Absent (an older server) reads as on: that is the factory default.
        https_enabled: value.get("https_enabled").and_then(Value::as_bool).unwrap_or(true),
        pin_required: value.get("pin_required").and_then(Value::as_bool).unwrap_or(true),
        pin,
        locked: flag("locked"),
        listening: flag("listening"),
        fingerprint: text("fingerprint"),
        unsaved: text("unsaved"),
        listen_error: text("listen_error"),
    })
}

fn parse_stores(value: &Value) -> Result<StoresInfo, StoreApiError> {
    if !value.is_object() {
        return Err(StoreApiError::Malformed(
            "the stores answer is not an object".into(),
        ));
    }
    let stores = value
        .get("stores")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| {
                    let id = entry.get("id").and_then(Value::as_str)?.to_string();
                    let cache = entry.get("cache");
                    Some(StoreInfo {
                        name: entry
                            .get("displayName")
                            .and_then(Value::as_str)
                            .unwrap_or("Store")
                            .to_string(),
                        id,
                        connected: entry
                            .get("connected")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        rejected: entry
                            .get("rejected")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        cached_releases: cache
                            .and_then(|c| c.get("releases"))
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32,
                        cache_stale: cache
                            .and_then(|c| c.get("stale"))
                            .and_then(Value::as_bool)
                            .unwrap_or(true),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(StoresInfo {
        secrets_available: value
            .get("secretsAvailable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        secrets_reason: value
            .get("secretsReason")
            .and_then(Value::as_str)
            .map(str::to_string),
        stores,
    })
}

fn parse_release_row(value: &Value) -> Option<ReleaseRow> {
    Some(ReleaseRow {
        id: value.get("id")?.as_str()?.to_string(),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        artist: value
            .get("artist")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        year: value
            .get("year")
            .and_then(Value::as_u64)
            .map(|y| y.min(u16::MAX as u64) as u16),
        track_count: value
            .get("trackCount")
            .and_then(Value::as_u64)
            .map(|n| n.min(u32::MAX as u64) as u32),
        duration_s: value.get("durationS").and_then(Value::as_u64),
    })
}

fn parse_collection(value: &Value) -> Result<CollectionPageInfo, StoreApiError> {
    if !value.is_object() {
        return Err(StoreApiError::Malformed(
            "the collection answer is not an object".into(),
        ));
    }
    Ok(CollectionPageInfo {
        total: value.get("total").and_then(Value::as_u64).unwrap_or(0) as u32,
        offset: value.get("offset").and_then(Value::as_u64).unwrap_or(0) as u32,
        stale: value.get("stale").and_then(Value::as_bool).unwrap_or(false),
        synced_at: value.get("syncedAt").and_then(Value::as_u64),
        releases: value
            .get("releases")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(parse_release_row).collect())
            .unwrap_or_default(),
    })
}

fn parse_release(value: &Value) -> Result<ReleaseInfo, StoreApiError> {
    let release = value
        .get("release")
        .and_then(parse_release_row)
        .ok_or_else(|| StoreApiError::Malformed("the release answer names no release".into()))?;
    let tracks: Vec<ReleaseTrackInfo> = value
        .get("tracks")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|t| ReleaseTrackInfo {
                    title: t
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    track: t
                        .get("track")
                        .and_then(Value::as_u64)
                        .map(|n| n.min(u32::MAX as u64) as u32),
                    duration_s: t.get("durationS").and_then(Value::as_u64),
                    declared_bytes: t.get("declaredBytes").and_then(Value::as_u64),
                    suffix: t
                        .get("suffix")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default();
    let declared: Vec<u64> = tracks.iter().filter_map(|t| t.declared_bytes).collect();
    let declared_bytes = (!declared.is_empty()).then(|| declared.iter().sum());
    let quality = value.get("quality");
    Ok(ReleaseInfo {
        release,
        tracks,
        lossy: quality
            .and_then(|q| q.get("audio"))
            .and_then(Value::as_str)
            .map(|audio| audio != "lossless")
            .unwrap_or(true),
        quality_note: quality
            .and_then(|q| q.get("note"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        declared_bytes,
        cached: value.get("cached").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn parse_delete_preview(value: &Value) -> Result<DeletePreviewInfo, StoreApiError> {
    if !value.is_object() {
        return Err(StoreApiError::Malformed(
            "the delete preview is not an object".into(),
        ));
    }
    let blockers: Vec<(String, String)> = value
        .get("blockers")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|b| {
                    (
                        b.get("kind").and_then(Value::as_str).unwrap_or_default().to_string(),
                        b.get("detail").and_then(Value::as_str).unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let files = value
        .get("files")
        .and_then(Value::as_array)
        .map(|list| list.len())
        .unwrap_or(0);
    Ok(DeletePreviewInfo {
        title: value
            .get("playable")
            .and_then(|p| p.get("title"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        files,
        total_bytes: value.get("total_bytes").and_then(Value::as_u64).unwrap_or(0),
        playing: value.get("playing").and_then(Value::as_bool).unwrap_or(false)
            || blockers.iter().any(|(kind, _)| kind == "playing"),
        blockers: blockers
            .into_iter()
            .map(|(kind, detail)| {
                if detail.is_empty() {
                    kind
                } else {
                    format!("{kind}: {detail}")
                }
            })
            .collect(),
        warnings: string_list(value.get("warnings")),
        playlists: value
            .get("playlists")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|p| p.get("title").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        noudata: value
            .get("noudata")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn parse_delete_done(value: &Value) -> Result<DeleteDone, StoreApiError> {
    if !value.is_object() {
        return Err(StoreApiError::Malformed(
            "the delete answer is not an object".into(),
        ));
    }
    Ok(DeleteDone {
        deleted: value
            .get("deleted")
            .and_then(Value::as_array)
            .map(|list| list.len())
            .unwrap_or(0),
        freed_bytes: value.get("freed_bytes").and_then(Value::as_u64).unwrap_or(0),
        partial: value.get("partial").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn string_list(value: Option<&Value>) -> Vec<String> {    value
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn normalise_base(base: &str) -> String {
    let base = base.trim();
    let base = base
        .strip_prefix("http://")
        .or_else(|| base.strip_prefix("https://"))
        .unwrap_or(base);
    base.trim_end_matches('/').to_string()
}

/// Percent-encode a path segment (session ids are opaque).
fn encode_path(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn split_response(raw: &[u8]) -> Result<(&[u8], &[u8]), StoreApiError> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| StoreApiError::Malformed("no response header terminator".into()))?;
    Ok((&raw[..split], &raw[split + 4..]))
}

fn parse_status(head: &[u8]) -> Result<u16, StoreApiError> {
    let line_end = head.iter().position(|b| *b == b'\r').unwrap_or(head.len());
    let line = std::str::from_utf8(&head[..line_end])
        .map_err(|_| StoreApiError::Malformed("status line is not text".into()))?;
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| StoreApiError::Malformed(format!("bad status line {line:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn json(body: &str) -> Value {
        serde_json::from_str(body).expect("fixture JSON")
    }

    #[test]
    fn tap_answer_parsing_keeps_the_canonical_purchase_page() {
        let answer = parse_tap_answer(&json(
            r#"{
              "state": "not_owned",
              "match": {"confidence": 0.0, "cacheStale": true, "requiresConfirmation": false,
                        "owned": null},
              "purchaseUrl": "https://joni.bandcamp.com/album/blue",
              "candidates": [],
              "blocked": null,
              "message": "You do not own this on Bandcamp."
            }"#,
        ))
        .expect("parse");
        assert_eq!(answer.state, TapState::NotOwned);
        assert_eq!(
            answer.purchase_url.as_deref(),
            Some("https://joni.bandcamp.com/album/blue")
        );
        assert!(answer.cache_stale);
        assert_eq!(answer.blocked, None);
        assert!(!answer.message.is_empty());

        // Anything that is not an absolute HTTPS page is dropped: the device never offers a
        // link it cannot show, and a scheme like `javascript:` is never a purchase page.
        for hostile in [
            "javascript:alert(1)",
            "http://joni.bandcamp.com/album/blue",
            "https://joni.bandcamp.com/album/blue\u{202E}",
            "https://x.example/\u{0000}",
        ] {
            let parsed = parse_tap_answer(&json(
                &serde_json::json!({
                    "state": "not_owned",
                    "purchaseUrl": hostile,
                    "message": "m",
                    "candidates": []
                })
                .to_string(),
            ))
            .expect("parse");
            assert_eq!(parsed.purchase_url, None, "{hostile:?} must not be offered");
        }
    }

    #[test]
    fn blocked_and_candidate_answers_parse() {
        let answer = parse_tap_answer(&json(
            r#"{
              "state": "not_found",
              "match": null,
              "purchaseUrl": null,
              "candidates": [],
              "blocked": "credentials_rejected",
              "message": "Bandcamp refused the stored sign-in."
            }"#,
        ))
        .expect("parse");
        assert_eq!(answer.blocked, Some(TapBlocked::CredentialsRejected));
        assert_eq!(answer.state, TapState::NotFound);

        let answer = parse_tap_answer(&json(
            r#"{
              "state": "ambiguous",
              "match": {"confidence": 0.72, "cacheStale": false,
                        "requiresConfirmation": true,
                        "owned": {"noudataPath": "Blue/blue.noudata", "title": "Blue",
                                  "artist": "Joni Mitchell", "storeReleaseId": "99"}},
              "purchaseUrl": null,
              "candidates": [
                {"storeReleaseId": "1", "title": "Blue", "artist": "Joni Mitchell",
                 "trackCount": 10, "durationS": 2178, "confidence": 0.72,
                 "reasons": ["title"], "source": "cache"}
              ],
              "blocked": null,
              "message": "One possible release was found."
            }"#,
        ))
        .expect("parse");
        assert_eq!(answer.state, TapState::Ambiguous);
        assert!(answer.requires_confirmation);
        assert_eq!(answer.candidates.len(), 1);
        assert_eq!(answer.owned_path.as_deref(), Some("Blue/blue.noudata"));
        assert_eq!(answer.release.as_ref().unwrap().store_release_id, "1");
    }

    #[test]
    fn download_progress_parsing_counts_done_tracks() {
        let progress = parse_progress(&json(
            r#"{
              "session": "s-1", "state": "running",
              "items": [{"state": "done"}, {"state": "running"}],
              "doneBytes": 1024, "totalBytes": 4096, "placedFiles": 0,
              "unite": null, "errors": [], "warnings": ["stream substituted"]
            }"#,
        ))
        .expect("parse");
        assert_eq!(progress.state, DownloadState::Running);
        assert_eq!(progress.tracks_done, 1);
        assert_eq!(progress.tracks_total, 2);
        assert_eq!(progress.done_bytes, 1024);
        assert!(!progress.finished());
        assert_eq!(progress.warnings, vec!["stream substituted".to_string()]);

        let done = parse_progress(&json(
            r#"{"state": "done", "items": [], "doneBytes": 9, "totalBytes": 9,
                "placedFiles": 3, "unite": {"state": "placed", "noudata": "Blue/blue.noudata"}}"#,
        ))
        .expect("parse");
        assert!(done.finished());
        assert_eq!(done.placed_files, 3);
        assert!(!done.already_present);
        assert_eq!(done.noudata.as_deref(), Some("Blue/blue.noudata"));

        // A replay that found the release already there placed nothing, and the screen needs
        // to know that zero files is not a failure.
        let replay = parse_progress(&json(
            r#"{"state": "done", "items": [], "doneBytes": 9, "totalBytes": 9,
                "placedFiles": 0, "unite": {"state": "exists", "noudata": "Blue/blue.noudata"}}"#,
        ))
        .expect("parse");
        assert_eq!(replay.placed_files, 0);
        assert!(replay.already_present);
        assert_eq!(replay.noudata.as_deref(), Some("Blue/blue.noudata"));

        assert!(parse_progress(&json(r#"{"state": "exploded", "items": []}"#)).is_err());
    }

    /// Serve one request on a loopback socket and hand the raw request text back.
    ///
    /// Reads until the body is complete: the client can write its head and body as two
    /// separate packets, and one `read` that catches only the head made this flaky under a
    /// loaded test run.
    fn serve_once(response: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = socket.read(&mut buf).expect("read");
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                let Some(split) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&request[..split]).to_string();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= split + 4 + length {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request).to_string();
            socket.write_all(response.as_bytes()).expect("write");
            let _ = socket.flush();
            request
        });
        (format!("127.0.0.1:{}", addr.port()), handle)
    }

    #[test]
    fn http_client_posts_the_documented_tap_body() {
        let (base, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 78\r\n\r\n\
             {\"state\":\"not_found\",\"match\":null,\"purchaseUrl\":null,\"candidates\":[],\
             \"blocked\":\"no_store\",\"message\":\"No store.\"}",
        );
        let mut api = HttpStoreApi::new(base);
        // The doc's shape, with the query-free release link.
        let answer = api
            .tap("https://joni.bandcamp.com/album/blue", Some(300), true)
            .expect("the tap call answers");
        assert_eq!(answer.blocked, Some(TapBlocked::NoStore));
        let request = server.join().expect("server thread");
        assert!(
            request.starts_with("POST /api/stores/tap HTTP/1.1"),
            "got {request:?}"
        );
        assert!(request.contains("Content-Type: application/json"));
        let body = request.split("\r\n\r\n").nth(1).expect("body");
        assert!(body.contains("\"url\":\"https://joni.bandcamp.com/album/blue\""));
        assert!(body.contains("\"consent\":true"));
        assert!(body.contains("\"expectedDuration\":300"));
    }

    #[test]
    fn http_client_reports_offline_and_refused_plainly() {
        // Nothing listening: an honest "offline", never a hang or a panic.
        let probe = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = probe.local_addr().expect("addr").port();
        drop(probe);
        let mut api = HttpStoreApi::new(format!("127.0.0.1:{port}"));
        match api.want("https://joni.bandcamp.com/album/blue") {
            Err(StoreApiError::Offline(_)) => {}
            other => panic!("expected Offline, got {other:?}"),
        }

        let (base, server) = serve_once(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n\
             {\"error\":\"sign-in expired\"}",
        );
        let mut api = HttpStoreApi::new(base);
        match api.download_status("s-1") {
            Err(StoreApiError::Refused {
                status: 401,
                message,
                ..
            }) => assert_eq!(message, "sign-in expired"),
            other => panic!("expected Refused(401), got {other:?}"),
        }
        let request = server.join().expect("server thread");
        assert!(request.starts_with("GET /api/stores/bandcamp/download/s-1 HTTP/1.1"));
    }

    #[test]
    fn portal_address_parsing_drops_anything_that_is_not_a_plain_address() {
        let answer = parse_portal_address(&json(
            r#"{"recommended":"http://192.168.1.42:8787",
                "urls":[{"url":"http://192.168.1.42:8787","recommended":true},
                        {"url":"http://127.0.0.1:8787","recommended":false}],
                "port":8787,"overrideUsed":false,"note":null}"#,
        ))
        .expect("parse");
        assert_eq!(answer.recommended.as_deref(), Some("http://192.168.1.42:8787"));
        assert_eq!(answer.candidates.len(), 2);
        assert_eq!(answer.short().as_deref(), Some("192.168.1.42:8787"));

        // A device screen never shows a credential, a query or a token, whatever the server
        // says: an address that carries one is dropped, and the screen says nothing to scan.
        let answer = parse_portal_address(&json(
            r#"{"recommended":"http://fan:hunter2@192.168.1.42:8787/?token=abc",
                "urls":[{"url":"http://fan:hunter2@192.168.1.42:8787/?token=abc"}],
                "port":8787}"#,
        ))
        .expect("parse");
        assert_eq!(answer.recommended, None);
        assert!(answer.candidates.is_empty());
        assert_eq!(answer.short(), None);

        // Nothing reachable is a valid answer: no recommended address plus the server's why.
        let answer = parse_portal_address(&json(
            r#"{"recommended":null,"urls":[],"port":8787,
                "note":"no address another device can reach yet"}"#,
        ))
        .expect("parse");
        assert_eq!(answer.recommended, None);
        assert!(answer.note.is_some());
    }

    #[test]
    fn collection_paging_knows_the_last_page_and_an_empty_collection() {
        let page = parse_collection(&json(
            r#"{"total":3,"offset":0,"size":2,"stale":false,"syncedAt":null,
                "releases":[{"id":"1","title":"Blue","artist":"Joni Mitchell","year":1971,
                             "trackCount":10,"durationS":2178,"hasCover":true},
                            {"id":"2","title":"Hejira","artist":"Joni Mitchell","year":1976,
                             "trackCount":9,"durationS":2000,"hasCover":false}]}"#,
        ))
        .expect("parse");
        assert_eq!(page.releases.len(), 2);
        assert_eq!(page.next_offset(), Some(2), "three total, two loaded");
        assert!(!page.is_last_page());
        assert_eq!(page.releases[0].year, Some(1971));

        // Last page: offset plus the rows it holds reaches the total.
        let last = parse_collection(&json(
            r#"{"total":3,"offset":2,"size":2,"stale":true,
                "releases":[{"id":"3","title":"Don Juan","artist":"Joni Mitchell"}]}"#,
        ))
        .expect("parse");
        assert_eq!(last.next_offset(), None);
        assert!(last.is_last_page());
        assert!(last.stale);

        // An empty collection has no next page and does not pretend to.
        let empty = parse_collection(&json(r#"{"total":0,"offset":0,"releases":[]}"#))
            .expect("parse");
        assert!(empty.releases.is_empty());
        assert_eq!(empty.next_offset(), None);
        assert!(empty.is_last_page());

        // A single release is both the first and the last page.
        let single = parse_collection(&json(
            r#"{"total":1,"offset":0,"releases":[{"id":"9","title":"One"}]}"#,
        ))
        .expect("parse");
        assert_eq!(single.next_offset(), None);
    }

    #[test]
    fn http_client_sends_the_search_query_percent_encoded() {
        let (base, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\n\r\n\
             {\"total\":0,\"offset\":0,\"releases\":[]}",
        );
        let mut api = HttpStoreApi::new(base);
        let page = api
            .collection("joni mitchell", 0, 20)
            .expect("the search answers");
        assert!(page.releases.is_empty());
        let request = server.join().expect("server thread");
        assert!(
            request.starts_with(
                "GET /api/stores/bandcamp/collection?offset=0&size=20&query=joni%20mitchell HTTP/1.1"
            ),
            "{request:?}"
        );

        // No query means the whole collection: no empty parameter is sent.
        let (base, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\n\r\n\
             {\"total\":0,\"offset\":0,\"releases\":[]}",
        );
        let mut api = HttpStoreApi::new(base);
        let _ = api.collection("  ", 0, 20).expect("the collection answers");
        let request = server.join().expect("server thread");
        assert!(
            request.starts_with("GET /api/stores/bandcamp/collection?offset=0&size=20 HTTP/1.1"),
            "{request:?}"
        );
    }

    #[test]
    fn release_and_delete_answers_parse() {
        let release = parse_release(&json(
            r#"{"release":{"id":"1234","title":"Blue","artist":"Joni Mitchell","year":1971,
                          "trackCount":2,"durationS":400},
                "tracks":[{"id":"t1","title":"All I Want","track":1,"disc":1,"durationS":200,
                           "suffix":"mp3","declaredBytes":1000,"declaredBitrate":128},
                          {"id":"t2","title":"My Old Man","track":2,"disc":1,"durationS":200,
                           "suffix":"mp3","declaredBytes":2000}],
                "quality":{"audio":"lossy","bitrateKbps":null,
                           "note":"Bandcamp sends a compressed stream, not the purchased FLAC."},
                "capabilities":{},"cached":true}"#,
        ))
        .expect("parse");
        assert_eq!(release.release.title, "Blue");
        assert_eq!(release.tracks.len(), 2);
        assert!(release.lossy, "the stream is lossy even though the purchase is not");
        assert!(release.quality_note.contains("FLAC"));
        assert_eq!(release.declared_bytes, Some(3000));
        assert!(release.cached);
        assert_eq!(release.title_line(), "Blue — Joni Mitchell");

        // Every flavour of refusal is a preview with its own reason, not a silent failure.
        let preview = parse_delete_preview(&json(
            r#"{"playable":{"id":"77","title":"Blue","kind":"music","is_orphan":false},
                "files":[{"path":"Blue/01.mp3","bytes":1000,"missing":false}],
                "covers":[],"noudata":"Blue/blue.noudata","folder":"Blue",
                "total_bytes":1000,
                "blockers":[{"kind":"playing","detail":"this item is playing"}],
                "warnings":["the cue file goes too"],"playlists":[],
                "keys":[],"playing":true}"#,
        ))
        .expect("parse");
        assert!(preview.blocked());
        assert!(preview.playing);
        assert_eq!(preview.files, 1);
        assert_eq!(preview.total_bytes, 1000);
        assert!(preview.blockers[0].contains("playing"));
        assert_eq!(preview.noudata.as_deref(), Some("Blue/blue.noudata"));

        let done = parse_delete_done(&json(
            r#"{"ok":true,"deleted":["a.mp3","b.mp3"],"index_updated":true,
                "freed_bytes":2048,"partial":false}"#,
        ))
        .expect("parse");
        assert_eq!(done.deleted, 2);
        assert_eq!(done.freed_bytes, 2048);
        assert!(!done.partial);
    }

    #[test]
    fn an_uncached_release_is_looked_up_through_the_guarded_post() {
        // SRV-06: the GET is cache-only. The `not_cached` answer makes the UI issue one
        // JSON POST for that id (the consented lookup), and its answer parses normally.
        let miss = r#"{"error":"not in the cache","code":"not_cached"}"#;
        let first = format!(
            "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{miss}",
            miss.len()
        );
        let body = r#"{"release":{"id":"a:1","title":"Blue","artist":"Joni Mitchell",
                       "year":1971,"trackCount":1,"durationS":200},
                      "tracks":[{"id":"t1","title":"All I Want","track":1,"disc":1,
                                 "durationS":200,"suffix":"mp3","declaredBytes":1000}],
                      "quality":{"audio":"lossy","bitrateKbps":null,"note":"compressed stream"},
                      "capabilities":{},"cached":false}"#;
        let second = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let (base, server) = serve_many_owned(vec![first, second]);
        let mut api = HttpStoreApi::new(base);
        let release = api.release("a:1").expect("the lookup answers");
        assert_eq!(release.release.id, "a:1");
        assert!(!release.cached, "the answer says it came from the store");

        let requests = server.join().expect("server thread");
        assert!(requests[0].starts_with("GET /api/stores/bandcamp/release/a%3A1 "), "{}", requests[0]);
        assert!(
            requests[1].starts_with("POST /api/stores/bandcamp/release "),
            "{}",
            requests[1]
        );
        assert!(
            requests[1].contains(r#"{"id":"a:1"}"#),
            "the lookup names the id: {}",
            requests[1]
        );
        // The POST is JSON-guarded like every other store write.
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("content-type: application/json"),
            "{}",
            requests[1]
        );
    }

    #[test]
    fn http_client_asks_for_the_portal_and_deletes_with_force() {
        let (base, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 34\r\n\r\n\
             {\"recommended\":null,\"urls\":[],\"port\":0}",
        );
        let mut api = HttpStoreApi::new(base);
        let answer = api.portal_address().expect("the portal call answers");
        assert_eq!(answer.recommended, None);
        let request = server.join().expect("server thread");
        assert!(request.starts_with("GET /api/portal/url HTTP/1.1"), "{request:?}");

        let (base, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 74\r\n\r\n\
             {\"ok\":true,\"deleted\":[\"a.mp3\"],\"index_updated\":true,\"freed_bytes\":9}",
        );
        let mut api = HttpStoreApi::new(base);
        let done = api.delete_playable(1099511627776).expect("delete answers");
        assert_eq!(done.deleted, 1);
        let request = server.join().expect("server thread");
        assert!(
            request.starts_with("DELETE /api/playables/1099511627776 HTTP/1.1"),
            "{request:?}"
        );
        assert!(request.contains("Content-Type: application/json"), "{request:?}");
        assert!(request.contains("{\"force\":true}"), "the confirmation is sent: {request:?}");
    }

    #[test]
    fn refusals_carry_the_servers_stable_code() {
        let (base, server) = serve_once(
            "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: 57\r\n\r\n\
             {\"ok\":false,\"code\":\"playing\",\"error\":\"this item is playing\"}",
        );
        let mut api = HttpStoreApi::new(base);
        let error = api.delete_playable(7).expect_err("409 is a refusal");
        assert_eq!(error.status(), Some(409));
        assert_eq!(error.code(), Some("playing"));
        assert_eq!(error.message(), "this item is playing");
        server.join().expect("server thread");
    }

    // ---- BCS_11 ---------------------------------------------------------------------------

    #[test]
    fn no_store_cache_parses_and_an_unknown_reason_falls_back_to_offline() {
        let answer = parse_tap_answer(&json(
            r#"{"state":"not_found","match":null,"purchaseUrl":null,"candidates":[],
                "blocked":"no_store_cache","message":"This device has not synced yet."}"#,
        ))
        .expect("parses");
        assert_eq!(answer.blocked, Some(TapBlocked::NoStoreCache));
        assert_eq!(answer.state, TapState::NotFound);

        // A newer server may add a reason: the screen must still render something.
        let answer = parse_tap_answer(&json(
            r#"{"state":"not_found","blocked":"quota_exceeded","message":"Later."}"#,
        ))
        .expect("parses");
        assert_eq!(answer.blocked, Some(TapBlocked::Offline));

        // No `blocked` at all stays unblocked.
        let answer = parse_tap_answer(&json(r#"{"state":"not_owned","blocked":null}"#)).expect("parses");
        assert_eq!(answer.blocked, None);
    }

    #[test]
    fn sync_status_parses_the_servers_camel_case_fields() {
        let running = parse_sync_status(&json(
            r#"{"state":"running","pages":3,"releases":120,"error":null,"errorCode":null,
                "startedAt":1760000000,"finishedAt":null,"cancelled":false}"#,
        ))
        .expect("parses");
        assert_eq!(running.state, SyncPhase::Running);
        assert!(!running.state.terminal());
        assert_eq!((running.pages, running.releases), (3, 120));
        assert_eq!(running.started_at, Some(1_760_000_000));
        assert_eq!(running.finished_at, None);

        let failed = parse_sync_status(&json(
            r#"{"state":"failed","pages":1,"releases":0,"error":"Bandcamp refused it.",
                "errorCode":"credentials_rejected","startedAt":1,"finishedAt":2,"cancelled":false}"#,
        ))
        .expect("parses");
        assert_eq!(failed.state, SyncPhase::Failed);
        assert!(failed.state.terminal());
        assert_eq!(failed.error.as_deref(), Some("Bandcamp refused it."));
        assert_eq!(failed.error_code.as_deref(), Some("credentials_rejected"));

        let cancelled = parse_sync_status(&json(r#"{"state":"idle","cancelled":true}"#)).expect("parses");
        assert_eq!(cancelled.state, SyncPhase::Idle);
        assert!(cancelled.cancelled);
        assert!(cancelled.state.terminal());

        // An unknown state reads as idle, a non-object is refused.
        assert_eq!(
            parse_sync_status(&json(r#"{"state":"warming_up"}"#)).expect("parses").state,
            SyncPhase::Idle
        );
        assert!(parse_sync_status(&json("[]")).is_err());
    }

    #[test]
    fn http_client_drives_the_three_sync_routes() {
        let body = "{\"state\":\"running\",\"pages\":1,\"releases\":2,\"cancelled\":false}";
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        for (call, expected) in [
            (0, "POST /api/stores/bandcamp/sync HTTP/1.1"),
            (1, "GET /api/stores/bandcamp/sync HTTP/1.1"),
            (2, "POST /api/stores/bandcamp/sync/cancel HTTP/1.1"),
        ] {
            let (base, server) = serve_once(response);
            let mut api = HttpStoreApi::new(base);
            let status = match call {
                0 => api.sync_start(),
                1 => api.sync_status(),
                _ => api.sync_cancel(),
            }
            .expect("the sync call answers");
            assert_eq!(status.state, SyncPhase::Running);
            assert_eq!((status.pages, status.releases), (1, 2));
            let request = server.join().expect("server thread");
            assert!(request.starts_with(expected), "got {request:?}");
            assert!(
                !request.to_ascii_lowercase().contains("authorization"),
                "the sync calls send no secrets: {request:?}"
            );
        }
    }

    #[test]
    fn waveform_status_reads_the_requested_track_out_of_the_playable() {
        let body = r#"{"id":7,"state":"partial","items":[
            {"item":0,"title":"One","media_path":"a","state":"analysed",
             "kinds":[{"kind":"waveform","state":"analysed"}]},
            {"item":1,"title":"Two","media_path":"b","state":"analysing",
             "kinds":[{"kind":"waveform","state":"analysing"}]}]}"#;
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (base, server) = serve_once(response);
        let mut api = HttpStoreApi::new(base);
        let kinds = api.waveform_status(7, 1).expect("the status call answers");
        assert_eq!(kinds.len(), 1);
        assert_eq!(kinds[0].state, crate::analysis::AnalysisState::Analysing);
        let request = server.join().expect("server thread");
        assert!(request.starts_with("GET /api/waveforms/7 HTTP/1.1"), "got {request:?}");
    }

    #[test]
    fn reanalyse_posts_json_that_names_the_item_and_only_a_requested_kind() {
        for (kind, expect_kinds) in [(None, false), (Some(AnalysisKind::Waveform), true)] {
            let (base, server) = serve_once(
                "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: 18\r\n\r\n{\"state\":\"queued\"}",
            );
            let mut api = HttpStoreApi::new(base);
            api.reanalyse(7, 3, kind).expect("202 is success");
            let request = server.join().expect("server thread");
            assert!(
                request.starts_with("POST /api/media/7/analysis/reanalyse HTTP/1.1"),
                "got {request:?}"
            );
            // The server's CSRF guard needs a JSON content type on every state change.
            assert!(request.contains("Content-Type: application/json"), "{request:?}");
            let body = request.split("\r\n\r\n").nth(1).expect("body");
            assert!(body.contains("\"item\":3"), "{body}");
            assert_eq!(body.contains("\"kinds\":[\"waveform\"]"), expect_kinds, "{body}");
        }
    }

    #[test]
    fn reanalyse_of_a_missing_file_arrives_as_the_unavailable_code() {
        let body = r#"{"ok":false,"error":"the media file is missing","code":"unavailable"}"#;
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (base, server) = serve_once(response);
        let mut api = HttpStoreApi::new(base);
        let error = api.reanalyse(7, 0, None).expect_err("refused");
        assert_eq!(error.status(), Some(409));
        assert_eq!(error.code(), Some("unavailable"));
        let _ = server.join();
    }

    /// Serve `head` + `body` as one response (the body may be binary), then close.
    fn serve_bytes(head: String, body: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            // The client may hang up early on an oversized answer: that is the point.
            let _ = socket.write_all(head.as_bytes());
            let _ = socket.write_all(&body);
            let _ = socket.flush();
            String::from_utf8_lossy(&request).to_string()
        });
        (format!("127.0.0.1:{}", addr.port()), handle)
    }

    #[test]
    fn release_cover_returns_the_content_type_and_the_exact_bytes() {
        let image: Vec<u8> = (0..=255u8).cycle().take(5_000).collect();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            image.len()
        );
        let (base, server) = serve_bytes(head, image.clone());
        let mut api = HttpStoreApi::new(base);
        let cover = api.release_cover("t:2970735116").expect("the cover arrives");
        assert_eq!(cover.content_type, "image/jpeg");
        assert_eq!(cover.bytes, image, "binary bytes survive untouched");
        let request = server.join().expect("server thread");
        assert!(
            request.starts_with("GET /api/stores/bandcamp/release/t%3A2970735116/cover HTTP/1.1"),
            "got {request:?}"
        );
        assert!(request.contains("Accept: image/*"), "{request:?}");
    }

    #[test]
    fn release_cover_over_the_cap_is_refused_never_truncated() {
        let image = vec![0xAB; MAX_COVER_BYTES + COVER_HEAD_ALLOWANCE + 4096];
        let head = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n\r\n".to_string();
        let (base, server) = serve_bytes(head, image);
        let mut api = HttpStoreApi::new(base);
        let error = api.release_cover("1").expect_err("too large");
        assert_eq!(error, StoreApiError::Malformed("cover too large".into()));
        let _ = server.join();

        // An ordinary refusal is still a plain refusal.
        let (base, server) = serve_once(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 27\r\n\r\n\
             {\"error\":\"no cover found\"}  ",
        );
        let mut api = HttpStoreApi::new(base);
        match api.release_cover("1").expect_err("no cover") {
            StoreApiError::Refused { status, message, .. } => {
                assert_eq!(status, 404);
                assert!(message.contains("no cover"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let _ = server.join();
    }

    // ---- SECFIX_02: the control token and the portal settings --------------------------------

    /// A server that answers each of `responses` on its own connection, in order, and records
    /// every request it saw.
    fn serve_many(
        responses: Vec<&'static str>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        serve_many_owned(responses.into_iter().map(str::to_string).collect())
    }

    fn serve_many_owned(
        responses: Vec<String>,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().expect("accept");
                let _ = socket.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut request = Vec::new();
                let mut buf = [0u8; 4096];
                // Read the head and then the declared body, so the client never sees a reset.
                loop {
                    match socket.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                    let Some(split) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&request[..split]).to_string();
                    let length = head
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= split + 4 + length {
                        break;
                    }
                }
                seen.push(String::from_utf8_lossy(&request).to_string());
                let _ = socket.write_all(response.as_bytes());
                let _ = socket.flush();
            }
            seen
        });
        (format!("127.0.0.1:{}", addr.port()), handle)
    }

    const OK_EMPTY: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
    const UNAUTHORISED: &str = "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 49\r\n\r\n{\"ok\":false,\"error\":\"x\",\"code\":\"local_token\",\"z\":1}";

    fn token_file(dir: &std::path::Path, text: &str) -> std::path::PathBuf {
        let path = dir.join("portal.token");
        std::fs::write(&path, text).expect("write the token");
        path
    }

    const TOKEN_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TOKEN_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn the_ui_sends_the_control_token_and_retries_once_on_401() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = token_file(dir.path(), TOKEN_A);
        let (base, server) = serve_many(vec![OK_EMPTY, UNAUTHORISED, OK_EMPTY]);
        let mut api = HttpStoreApi::with_token_file(base, &path);
        // First call: the token is read from the file and sent.
        let _ = api.portal_settings();
        // The server restarts and writes a new token; the next call still has the old one cached,
        // gets a 401, re-reads the file and succeeds on its one retry.
        std::fs::write(&path, TOKEN_B).unwrap();
        let _ = api.portal_settings();
        let seen = server.join().expect("server thread");
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert!(seen[0].contains(&format!("X-Plippa-Local: {TOKEN_A}")), "{:?}", seen[0]);
        assert!(seen[1].contains(&format!("X-Plippa-Local: {TOKEN_A}")), "stale token first: {:?}", seen[1]);
        assert!(seen[2].contains(&format!("X-Plippa-Local: {TOKEN_B}")), "fresh token on the retry: {:?}", seen[2]);
    }

    #[test]
    fn a_second_401_is_an_answer_not_a_loop() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = token_file(dir.path(), TOKEN_A);
        let (base, server) = serve_many(vec![UNAUTHORISED, UNAUTHORISED]);
        let mut api = HttpStoreApi::with_token_file(base, &path);
        let error = api.portal_settings().expect_err("still refused");
        assert_eq!(error.status(), Some(401));
        assert_eq!(server.join().expect("server thread").len(), 2, "one retry, no more");
    }

    #[test]
    fn without_a_token_file_no_header_is_sent_and_a_401_is_not_retried() {
        let (base, server) = serve_many(vec![UNAUTHORISED]);
        let mut api = HttpStoreApi::new(base);
        assert_eq!(api.portal_settings().expect_err("refused").status(), Some(401));
        let seen = server.join().expect("server thread");
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].contains("X-Plippa-Local"), "{:?}", seen[0]);
    }

    #[test]
    fn a_missing_or_malformed_token_file_sends_no_token() {
        let dir = tempfile::tempdir().expect("tmp");
        let missing = dir.path().join("nope");
        assert_eq!(read_token_file(&missing), None);
        for junk in ["", "short", "has spaces in it, so it is not a token at all....", "ünïcode-ünïcode-ünïcode-ünïcode"] {
            let path = token_file(dir.path(), junk);
            assert_eq!(read_token_file(&path), None, "{junk:?}");
        }
        let huge = token_file(dir.path(), &"a".repeat(10_000));
        assert_eq!(read_token_file(&huge), None);
        let good = token_file(dir.path(), &format!("{TOKEN_A}\n"));
        assert_eq!(read_token_file(&good).as_deref(), Some(TOKEN_A));
        // A symlink is not followed.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        assert_eq!(read_token_file(&link), None);
    }

    #[test]
    fn the_portal_settings_calls_post_the_documented_bodies() {
        let json_body = r#"{"available":true,"enabled":true,"pin_required":false,"pin":"048215","locked":true,"listening":true,"fingerprint":"AB:CD"}"#;
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{json_body}",
            json_body.len()
        );
        let (base, server) = serve_many_owned(vec![reply; 7]);
        let mut api = HttpStoreApi::new(base);
        let got = api.portal_settings().expect("read");
        assert!(got.available && got.enabled && !got.pin_required && got.locked && got.listening);
        assert_eq!(got.pin.as_deref(), Some("048215"));
        assert_eq!(got.fingerprint.as_deref(), Some("AB:CD"));
        assert!(got.https_enabled, "an answer without the field reads as On");
        api.set_portal_enabled(false).expect("enabled");
        api.set_https_enabled(false).expect("https");
        api.regenerate_certificate().expect("regenerate");
        api.set_pin_required(true).expect("required");
        api.set_pin("135790").expect("pin");
        api.clear_lockout().expect("clear");
        let seen = server.join().expect("server thread");
        let body = |i: usize| seen[i].rsplit("\r\n\r\n").next().unwrap_or("").to_string();
        assert!(seen[0].starts_with("GET /api/portal/settings HTTP/1.1"));
        for (i, expected) in [
            (1, r#"{"enabled":false}"#),
            (2, r#"{"https_enabled":false}"#),
            (3, r#"{"regenerate_certificate":true}"#),
            (4, r#"{"pin_required":true}"#),
            (5, r#"{"pin":"135790"}"#),
            (6, r#"{"clear_lockout":true}"#),
        ] {
            assert!(seen[i].starts_with("POST /api/portal/settings HTTP/1.1"), "{:?}", seen[i]);
            assert_eq!(body(i), expected);
        }
    }

    #[test]
    fn portal_settings_parsing_never_trusts_the_pin_shape() {
        let parse = |v: Value| parse_portal_settings(&v).expect("an object parses");
        let s = parse(serde_json::json!({"available": true, "enabled": true, "pin": "12345"}));
        assert_eq!(s.pin, None, "five digits is not a PIN");
        let s = parse(serde_json::json!({"available": true, "pin": "12a456"}));
        assert_eq!(s.pin, None);
        let s = parse(serde_json::json!({}));
        assert!(!s.available && !s.enabled);
        assert!(s.pin_required, "an unknown PIN switch reads as the safe state");
        assert!(s.https_enabled, "an unknown HTTPS switch reads as On, the factory default");
        assert!(
            !parse(serde_json::json!({ "https_enabled": false })).https_enabled,
            "only an explicit false serves plain HTTP"
        );
        assert!(parse_portal_settings(&serde_json::json!([1])).is_err());
    }

    #[test]
    fn the_portal_address_says_when_the_portal_is_off() {
        let off = parse_portal_address(&json(r#"{"recommended":"http://192.168.1.42:8787","urls":[],"port":8787,"portalEnabled":false}"#))
            .expect("parses");
        assert!(off.portal_off);
        let on = parse_portal_address(&json(r#"{"recommended":null,"urls":[],"port":8787,"portalEnabled":true}"#))
            .expect("parses");
        assert!(!on.portal_off);
        // An older server has no switch and the portal was always on.
        let old = parse_portal_address(&json(r#"{"recommended":null,"urls":[],"port":8787}"#)).expect("parses");
        assert!(!old.portal_off);
    }

}
