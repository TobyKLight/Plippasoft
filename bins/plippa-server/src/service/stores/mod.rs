//! Bandcamp connection lifecycle for `plippa-server` (BCS_03).
//!
//! This module is the one place that knows how a store connection is *created,
//! stored, checked, refreshed and removed*. Browsing/downloading what the cache holds
//! is BCS_04; this part only has to make a connection that a person can create from
//! the portal and that survives a reboot.
//!
//! ```text
//!   portal ──POST /api/stores/bandcamp/connect──▶ StoreHub::connect
//!                                                     │ verify_credentials() (getArtists)
//!                                                     ▼
//!                                            StoreSecrets (0700/0600)
//!   portal ──POST /api/stores/bandcamp/sync───▶ StoreHub::start_sync
//!                                                     │ crawl, 200/page, 250 ms apart
//!                                                     ▼
//!                              <library>/.plippa/stores/bandcamp.json (0600, atomic)
//! ```
//!
//! Design rules that the rest of the server relies on:
//!
//! * **The service lock is never held across the network.** Handlers clone the
//!   [`Arc<StoreHub>`](StoreHub) out of [`ServerService`](crate::service::ServerService)
//!   and call into it without the service mutex; a sync runs on its own thread.
//! * **Store failure never takes the library with it.** With no usable secrets
//!   directory the hub reports `available: false` with a reason and every other route
//!   works exactly as before.
//! * **No credential ever leaves the secrets file.** Not into a log line, an error
//!   string, a status payload or the cache. Only [`plippa_store::CredentialStore`]
//!   serialises [`Credentials`], and only into its own file.
//! * **One connect/validate at a time** (429 for the second) and **one sync at a
//!   time** (also 429). Nothing validates in the background.
//! * **A failed sync keeps the last good cache.** The cache is only replaced by a
//!   crawl that completed (or by one the crawl bounds cut short, which is written with
//!   `complete: false`).

pub mod browse;
pub mod cache;
pub mod download;
pub mod matching;
pub mod post_unite;
pub mod secrets;
pub mod tap;
pub mod transport;

pub use browse::{
    CollectionPage, CoverImage, QualityNote, ReleaseSummary, ReleaseView, TrackView,
    COVER_CACHE_CONTROL, DEFAULT_PAGE_SIZE as COLLECTION_PAGE_SIZE, MAX_LOOKUP_BODY, MAX_PAGE_SIZE,
    QUALITY_NOTE,
};
pub use download::{
    DownloadConsent, DownloadItem, DownloadLimitsView, DownloadRequest,
    DownloadStart, DownloadState, DownloadStatus, HandOffState, HandedOff, ItemState,
    PrepareDownloadRequest, QualityWarning, RefusedTrack, StagedRelease, StagedStoreDownload,
    StagedTrackFile, TrackSelection, UNITE_ENTRY_POINT,
};
pub use matching::{
    BandcampKind, BandcampUrl, Candidate, ConfirmCandidate, ConfirmMatchRequest, LibraryItemRef,
    MatchConfirmation, MatchRequest, MatchResult, ASSOCIATION_CONFIDENCE, AUTO_USABLE_CONFIDENCE,
    CONFIRM_CONFIDENCE, MAX_ASSOCIATIONS, MAX_MATCH_BODY, NOT_IN_COLLECTION, SEARCH_CONFIDENCE_CAP,
};
pub use tap::{TapRequest, WantRequest, MAX_TAP_BODY};
pub use secrets::StoreSecrets;
pub use transport::StoreTransport;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_bcs04;
#[cfg(test)]
mod tests_bcs05;
#[cfg(test)]
mod tests_bcs06;
#[cfg(test)]
mod tests_bcs07;
#[cfg(test)]
mod tests_bcs11;
#[cfg(test)]
pub(crate) mod tests_support;

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use plippa_store::{
    AuthenticatedHosts, Capabilities, CrawlDecision, CrawlOptions, CrawlStop, Credentials,
    StoreError, StoreProvider, StoreRelease, StoreReleaseDetail, StoreReleaseId, SubsonicProvider,
    Transport, MAX_CRAWL_RELEASES,
};
use serde::{Deserialize, Serialize};

use crate::service::ServerService;
use crate::web::download::DownloadLimits;
use cache::CacheError;
pub use cache::{StoreCache, StorePrefs, CACHE_MAX_AGE_SECS};

/// Stable store slug (also the credentials/cache file name).
pub const STORE_SLUG: &str = "bandcamp";
/// Name the portal shows.
pub const STORE_DISPLAY_NAME: &str = "Bandcamp";
/// The store's Subsonic base URL (the provider appends `/rest/<endpoint>.view`).
pub const BANDCAMP_BASE_URL: &str = "https://bandcamp.com/api/subsonic";
/// Subsonic client name; the store does not validate it.
pub const CLIENT_NAME: &str = "Plippa";
/// Page size used for the collection crawl (the store's own page size).
pub const DEFAULT_PAGE_SIZE: u32 = 200;
/// Gap between pages (no parallel requests, ever).
pub const DEFAULT_SYNC_DELAY: Duration = Duration::from_millis(250);
/// Automatic syncs are at least this far apart, whatever caused the last one.
pub const AUTO_SYNC_MIN_INTERVAL_SECS: u64 = 6 * 60 * 60;
/// How often the background worker looks for a due automatic sync.
pub const AUTO_SYNC_POLL_SECS: u64 = 60;
/// Reported when the server was started without a secrets directory at all.
pub const NO_SECRETS_DIR: &str =
    "no secrets directory is configured (--secrets-dir / PLIPPA_SECRETS_DIR)";
/// Most log lines kept for diagnostics/tests.
const LOG_LINES: usize = 64;

/// Append one store log line (stderr) and keep it for [`log_tail`].
pub(crate) fn log_line(message: &str) {
    eprintln!("plippa-server stores: {message}");
    let slot = log_slot();
    let mut lines = slot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if lines.len() >= LOG_LINES {
        lines.remove(0);
    }
    lines.push(message.to_string());
}

fn log_slot() -> &'static Mutex<Vec<String>> {
    static SLOT: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Vec::new()))
}

/// Recent store log lines (diagnostics and the "no secret in the log" test).
///
/// The same lines also go to stderr; this ring exists so a test can assert what the
/// server said without capturing the process's standard error.
pub fn log_tail() -> Vec<String> {
    log_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Unix seconds.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A store request that cannot be answered. Each variant has a stable [`Self::code`]
/// and the HTTP status the portal should answer with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreApiError {
    /// `consent` was absent or `false`.
    Consent,
    /// A connect/validate or a sync is already running.
    Busy,
    /// The secrets directory cannot hold credentials (or the cache cannot be written).
    Unavailable(String),
    /// The feature is on but no store connection exists yet.
    NotConnected,
    /// A refusal to write something the portal asked for.
    TooLarge(String),
    /// A refusal to download a batch that is over the byte cap.
    BatchTooLarge(String),
    /// There is not enough free space for the prepared download.
    NoSpace {
        /// Bytes the estimates add up to.
        needed: u64,
        /// Bytes available where the files would be staged.
        free: u64,
    },
    /// The release has no cover art (or the bytes were not an image).
    NoCover,
    /// The consent id was not the one last shown (a newer prepare replaced it).
    StaleConsent,
    /// No download session by that id (or a newer one replaced it).
    UnknownSession,
    /// The request was valid but there is nothing it could act on.
    NotReady(String),
    /// A release link was not a Bandcamp release link (BCS_06).
    InvalidUrl(String),
    /// A match/confirm request had a field that cannot be used.
    InvalidRequest(String),
    /// The candidate is not in the cached collection, so the server cannot score it.
    UnknownRelease,
    /// The server re-scored the candidate below [`matching::CONFIRM_CONFIDENCE`].
    LowConfidence {
        /// The score the server computed, to two decimals (a string keeps the error `Eq`).
        score: String,
    },
    /// A local cache write failed.
    Cache(String),
    /// The release is not in the cached collection, so a cache-only GET cannot answer it
    /// (SRV-06: an uncached id must never reach the store from a GET).
    NotCached,
    /// The ad-hoc outbound-call limiter refused this request (SRV-06).
    RateLimited,
    /// The store layer's own error.
    Store(StoreError),
}

impl StoreApiError {
    /// Stable machine code for the portal.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Consent => "consent_required",
            Self::Busy => "busy",
            Self::Unavailable(_) => "unavailable",
            Self::NotConnected => "not_connected",
            Self::TooLarge(_) => "cache_too_large",
            Self::BatchTooLarge(_) => "too_large",
            Self::NoSpace { .. } => "no_space",
            Self::NoCover => "no_cover",
            Self::StaleConsent => "stale_consent",
            Self::UnknownSession => "unknown_session",
            Self::NotReady(_) => "not_ready",
            Self::InvalidUrl(_) => "invalid_url",
            Self::InvalidRequest(_) => "invalid_request",
            Self::UnknownRelease => "unknown_release",
            Self::LowConfidence { .. } => "low_confidence",
            Self::Cache(_) => "cache_error",
            Self::NotCached => "not_cached",
            Self::RateLimited => "rate_limited",
            Self::Store(inner) => match inner {
                // The portal shows "blocked" for a bot challenge, not the crate's word.
                StoreError::Challenged => "blocked",
                other => other.code(),
            },
        }
    }

    /// HTTP status the portal answers with.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Consent => 403,
            Self::Busy => 429,
            Self::Unavailable(_) => 503,
            Self::NotConnected => 409,
            Self::TooLarge(_) => 413,
            Self::BatchTooLarge(_) => 413,
            Self::NoSpace { .. } => 507,
            Self::NoCover => 404,
            Self::StaleConsent => 409,
            Self::UnknownSession => 404,
            Self::NotReady(_) => 409,
            Self::InvalidUrl(_) => 400,
            Self::InvalidRequest(_) => 400,
            Self::UnknownRelease => 404,
            Self::LowConfidence { .. } => 409,
            Self::Cache(_) => 500,
            Self::NotCached => 409,
            Self::RateLimited => 429,
            Self::Store(inner) => match inner {
                StoreError::BadCredentials => 401,
                StoreError::InvalidCredentials => 400,
                StoreError::NotFound => 404,
                StoreError::RateLimited => 429,
                StoreError::Challenged => 503,
                StoreError::Policy { .. } => 400,
                StoreError::Unimplemented { .. } => 502,
                StoreError::Unreachable { .. }
                | StoreError::Malformed { .. }
                | StoreError::Transport { .. } => 502,
            },
        }
    }

    /// The message shown to the person. Never contains a user name or a password.
    pub fn message(&self) -> String {
        match self {
            Self::Consent => {
                "consent is required before store credentials can be stored".to_string()
            }
            Self::Busy => "another store request is already running; wait for it to finish"
                .to_string(),
            Self::Unavailable(reason) => reason.clone(),
            Self::NotConnected => "no Bandcamp connection is stored".to_string(),
            Self::TooLarge(reason) => reason.clone(),
            Self::BatchTooLarge(reason) => reason.clone(),
            Self::NoSpace { needed, free } => format!(
                "not enough free space: the selection is about {} MB and {} MB is free",
                needed / (1024 * 1024),
                free / (1024 * 1024)
            ),
            Self::NoCover => "that release has no cover art".to_string(),
            Self::StaleConsent => {
                "the consent was for an older selection; review the list again".to_string()
            }
            Self::UnknownSession => "unknown or replaced download session".to_string(),
            Self::NotReady(reason) => reason.clone(),
            Self::InvalidUrl(reason) => reason.clone(),
            Self::InvalidRequest(reason) => reason.clone(),
            Self::UnknownRelease => {
                "that release is not in the cached collection; sync the collection and try again"
                    .to_string()
            }
            Self::LowConfidence { score } => format!(
                "the server scores this candidate {score}, below the {:.1} needed to record it",
                matching::CONFIRM_CONFIDENCE
            ),
            Self::Cache(reason) => reason.clone(),
            Self::NotCached => {
                "that release is not in the cached collection; look it up first".to_string()
            }
            Self::RateLimited => {
                "too many store lookups just now; wait a moment and try again".to_string()
            }
            Self::Store(inner) => match inner {
                StoreError::BadCredentials => {
                    "Bandcamp rejected those credentials; generate a new pair in Bandcamp \
                     settings and try again"
                        .to_string()
                }
                StoreError::InvalidCredentials => {
                    "copy the two values from bandcamp.com/settings?pane=fan".to_string()
                }
                StoreError::Challenged => {
                    "Bandcamp answered with a bot-check page instead of data; the request must \
                     carry a User-Agent, so this address is probably blocked"
                        .to_string()
                }
                StoreError::Unreachable { detail } => {
                    format!("Bandcamp could not be reached ({detail})")
                }
                StoreError::Malformed { detail } => {
                    format!("Bandcamp sent a reply Plippa could not read ({detail})")
                }
                StoreError::Unimplemented { endpoint } => format!(
                    "the Bandcamp Subsonic beta does not implement {endpoint}"
                ),
                StoreError::NotFound => "that release is not in the collection".to_string(),
                StoreError::RateLimited => {
                    "Bandcamp asked Plippa to slow down; try again later".to_string()
                }
                StoreError::Transport { detail } => {
                    format!("the request to Bandcamp failed ({detail})")
                }
                StoreError::Policy { reason } => format!("refused: {reason}"),
            },
        }
    }
}

impl std::fmt::Display for StoreApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for StoreApiError {}

impl From<StoreError> for StoreApiError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<CacheError> for StoreApiError {
    fn from(error: CacheError) -> Self {
        let text = error.to_string();
        match &error {
            CacheError::TooLarge { .. } => Self::TooLarge(text),
            _ => Self::Cache(text),
        }
    }
}

// ---------------------------------------------------------------------------
// Status payloads
// ---------------------------------------------------------------------------

/// What the portal needs to draw the store row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreStatus {
    /// Store slug (`bandcamp`).
    pub id: String,
    /// Name shown to the person.
    pub display_name: String,
    /// Credentials are stored and have not been rejected by the last check.
    pub connected: bool,
    /// The last check was refused by the store: the stored pair no longer works.
    pub rejected: bool,
    /// Last successful check (Unix seconds), when known.
    pub last_validated: Option<u64>,
    /// What the provider reports it can do.
    pub capabilities: Capabilities,
    /// The collection cache summary.
    pub cache: CacheStatus,
}

/// Collection cache state for the portal.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStatus {
    /// Releases currently cached.
    pub releases: u32,
    /// When the cache was last filled (Unix seconds).
    pub synced_at: Option<u64>,
    /// Older than a day, incomplete, or never filled.
    pub stale: bool,
    /// The crawl saw the end of the collection.
    pub complete: bool,
    /// Why the cache is incomplete / could not be read, in words.
    pub last_error: Option<String>,
}

/// `GET /api/stores` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoresStatus {
    /// One entry per store this build knows (Bandcamp only).
    pub stores: Vec<StoreStatus>,
    /// A usable secrets directory exists.
    pub secrets_available: bool,
    /// Why not, when `secrets_available` is false.
    pub secrets_reason: Option<String>,
}

/// The `stores` field of `GET /api/status` (a summary; `/api/stores` has the details).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoresSummary {
    /// The store feature can be used at all.
    pub available: bool,
    /// Why not, when `available` is false.
    pub reason: Option<String>,
    /// A connection is stored.
    pub connected: bool,
}

impl StoresStatus {
    /// Fold into the summary `/api/status` carries.
    pub fn summary(&self) -> StoresSummary {
        StoresSummary {
            available: self.secrets_available,
            reason: self.secrets_reason.clone(),
            connected: self.stores.first().map(|s| s.connected).unwrap_or(false),
        }
    }
}

/// Sync worker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncState {
    /// Nothing has run since start-up (or the last run was cancelled).
    #[default]
    Idle,
    Running,
    Done,
    Failed,
}

/// `GET /api/stores/bandcamp/sync` answer and the body of the sync routes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    /// Current phase.
    pub state: SyncState,
    /// Pages fetched so far.
    pub pages: u32,
    /// Releases counted so far.
    pub releases: u32,
    /// What went wrong, in words (never a credential).
    pub error: Option<String>,
    /// Machine code for `error`.
    pub error_code: Option<String>,
    /// When this run started (Unix seconds).
    pub started_at: Option<u64>,
    /// When this run stopped (Unix seconds).
    pub finished_at: Option<u64>,
    /// The last run was stopped by [`StoreHub::cancel_sync`].
    pub cancelled: bool,
}

// ---------------------------------------------------------------------------
// The hub
// ---------------------------------------------------------------------------

/// Memory copy of the cache plus the file stamp it was read at.
#[derive(Default)]
struct CacheSlot {
    cache: Option<StoreCache>,
    /// `(mtime seconds, mtime nanoseconds, length)` of the file the copy came from.
    stamp: Option<(u64, u32, u64)>,
    /// Last read/move-aside report, shown in `cache.lastError`.
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Ad-hoc call limiter (SRV-06)
// ---------------------------------------------------------------------------

/// Ad-hoc lookups: burst of five, then one per second, at most 60 per hour.
pub const ADHOC_BURST: u32 = 5;
/// See [`ADHOC_BURST`].
pub const ADHOC_PER_HOUR: u32 = 60;
/// Cover fetches for a **cached** release. One portal collection page renders 50 rows, each
/// with a lazy `<img>` to this route, so the burst must cover a whole page or thumbnails
/// would fail on first view; the sustained rate and the hourly cap still bound an attacker.
pub const COVER_BURST: u32 = 50;
/// See [`COVER_BURST`].
pub const COVER_PER_HOUR: u32 = 600;

/// A token bucket with an hourly cap.
///
/// Counted calls are the ones a hostile page can trigger with client-chosen ids: the
/// uncached-release lookup POST, the track fetch a cached release view needs, and a CDN
/// cover fetch for a cached release. The sync crawl and the consent-gated download queue
/// are **exempt** (they are user-initiated and already bounded), so a 120-page sync never
/// starves itself on this bucket.
struct TokenBucket {
    tokens: f64,
    last: std::time::Instant,
    window_start: std::time::Instant,
    window_count: u32,
    burst: f64,
    refill_per_sec: f64,
    per_hour: u32,
}

impl TokenBucket {
    fn new(burst: u32, refill_per_sec: f64, per_hour: u32) -> Self {
        let now = std::time::Instant::now();
        Self {
            tokens: burst as f64,
            last: now,
            window_start: now,
            window_count: 0,
            burst: burst as f64,
            refill_per_sec,
            per_hour,
        }
    }

    /// Take one token, or report that the bucket is (still) empty.
    fn try_take(&mut self) -> bool {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.burst);
        self.last = now;
        if now.duration_since(self.window_start).as_secs() >= 3600 {
            self.window_start = now;
            self.window_count = 0;
        }
        if self.window_count >= self.per_hour || self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        self.window_count += 1;
        true
    }
}

/// Called after a store download has been written into the library, so the index picks the
/// release up without waiting for the periodic scan. Returns the playable id the index gave
/// it (BCS_05 reports it in the unite result).
pub type LibraryIndexer =
    Arc<dyn Fn(&std::path::Path, &std::path::Path) -> Result<u64, String> + Send + Sync>;

/// Deletes the playable whose `.noudata` is the second argument (library-relative), through
/// BCS_10's delete implementation. Returns the playable id that was removed. BCS_05 uses it
/// to replace a release this store already produced instead of creating a second folder.
pub type LibraryDeleter = Arc<dyn Fn(&std::path::Path, &str) -> Result<u64, String> + Send + Sync>;

/// Everything the store feature owns: secrets, cache, sync state and the transport.
pub struct StoreHub {
    secrets: Option<StoreSecrets>,
    library_root: Mutex<Option<PathBuf>>,
    transport: Arc<dyn Transport + Send + Sync>,
    page_size: u32,
    sync_delay: Duration,
    /// One connect/validate at a time.
    connect_busy: AtomicBool,
    /// Set by [`Self::cancel_sync`], cleared when a sync starts.
    cancel: AtomicBool,
    sync: Mutex<SyncStatus>,
    /// The stored pair was rejected by the last check.
    rejected: AtomicBool,
    cache: Mutex<CacheSlot>,
    /// Cached secrets-directory probe (`Ok`/`Err(reason)`), computed on first use.
    available: Mutex<Option<Result<(), String>>>,
    /// Hosts this store may reach (BCS_04 downloads reuse it as the fetch allowlist).
    hosts: Arc<AuthenticatedHosts>,
    /// Bounded in-memory cover cache (a collection page shows one cover per row).
    cover_cache: Mutex<VecDeque<browse::CachedCover>>,
    /// One prepare at a time (it makes real store requests).
    prepare_busy: AtomicBool,
    /// The plan the last prepare showed; a download may only start with its id.
    prepared: Mutex<Option<download::PreparedDownload>>,
    /// The one in-flight (or most recently finished) download session.
    download: Mutex<Option<download::DownloadSlot>>,
    /// One download at a time.
    download_busy: AtomicBool,
    /// Caps, stall timeout and free-space probe for the download worker.
    download_limits: DownloadLimits,
    /// What this player can decode; set once at start-up (MEDIA_01). `None` keeps whatever
    /// `download_limits` carries.
    device_capabilities: Mutex<Option<Arc<plippa_core::Capabilities>>>,
    /// Pause between tracks (the beta is slow and rate-limited).
    download_pause: Duration,
    /// Tells the library index about a release a completed store download placed.
    indexer: Mutex<Option<LibraryIndexer>>,
    /// Deletes a library item by `.noudata` path (BCS_10's delete), for replace.
    deleter: Mutex<Option<LibraryDeleter>>,
    /// The shared web hub (BCS_05 Path A and cover art fetch through it).
    web: Mutex<Option<Arc<crate::web::WebHub>>>,
    /// Ad-hoc store lookups (SRV-06).
    adhoc: Mutex<TokenBucket>,
    /// CDN cover fetches for cached releases (SRV-06).
    cover_rate: Mutex<TokenBucket>,
}

impl std::fmt::Debug for StoreHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The transport is a trait object and credentials live elsewhere; neither is
        // printed. `secrets` prints the directory only (never a file).
        f.debug_struct("StoreHub")
            .field("secrets_dir", &self.secrets.as_ref().map(|s| s.dir()))
            .field("page_size", &self.page_size)
            .field("sync_delay", &self.sync_delay)
            .finish_non_exhaustive()
    }
}

impl StoreHub {
    /// The production hub: real transport, real secrets directory.
    pub fn production(secrets_dir: Option<PathBuf>) -> Self {
        Self::with_transport(secrets_dir, Arc::new(StoreTransport::production()))
    }

    /// A hub over an explicit transport (tests, and other store profiles later).
    pub fn with_transport(
        secrets_dir: Option<PathBuf>,
        transport: Arc<dyn Transport + Send + Sync>,
    ) -> Self {
        Self {
            secrets: secrets_dir.map(StoreSecrets::new),
            library_root: Mutex::new(None),
            transport,
            page_size: DEFAULT_PAGE_SIZE,
            sync_delay: DEFAULT_SYNC_DELAY,
            connect_busy: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            sync: Mutex::new(SyncStatus::default()),
            rejected: AtomicBool::new(false),
            cache: Mutex::new(CacheSlot::default()),
            available: Mutex::new(None),
            hosts: Arc::new(AuthenticatedHosts::bandcamp()),
            cover_cache: Mutex::new(VecDeque::new()),
            prepare_busy: AtomicBool::new(false),
            prepared: Mutex::new(None),
            download: Mutex::new(None),
            download_busy: AtomicBool::new(false),
            // A store audio URL is a bearer capability: never log it.
            download_limits: DownloadLimits {
                log_url: false,
                ..DownloadLimits::from_env()
            },
            device_capabilities: Mutex::new(None),
            download_pause: download::DEFAULT_TRACK_PAUSE,
            indexer: Mutex::new(None),
            deleter: Mutex::new(None),
            web: Mutex::new(None),
            adhoc: Mutex::new(TokenBucket::new(ADHOC_BURST, 1.0, ADHOC_PER_HOUR)),
            cover_rate: Mutex::new(TokenBucket::new(COVER_BURST, 4.0, COVER_PER_HOUR)),
        }
    }

    /// Take one ad-hoc lookup token, or refuse with [`StoreApiError::RateLimited`].
    pub(crate) fn take_adhoc(&self) -> Result<(), StoreApiError> {
        let mut bucket = self
            .adhoc
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if bucket.try_take() {
            Ok(())
        } else {
            Err(StoreApiError::RateLimited)
        }
    }

    /// Take one cover-fetch token (a cached release's cover, fetched from the CDN).
    pub(crate) fn take_cover(&self) -> Result<(), StoreApiError> {
        let mut bucket = self
            .cover_rate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if bucket.try_take() {
            Ok(())
        } else {
            Err(StoreApiError::RateLimited)
        }
    }

    /// Tell the download paths which formats this player can decode (MEDIA_01); a store track
    /// in any other format fails with the same sentence the portal import gives.
    pub fn set_device_capabilities(&self, caps: Arc<plippa_core::Capabilities>) {
        *self
            .device_capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(caps);
    }

    /// The download caps with the player's capabilities applied.
    fn limits(&self) -> DownloadLimits {
        let mut limits = self.download_limits.clone();
        if let Some(caps) = self
            .device_capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            limits.capabilities = caps;
        }
        limits
    }

    /// The library root the cache lives under (USB libraries appear after start-up).
    pub fn set_library_root(&self, root: Option<PathBuf>) {
        let mut slot = self
            .library_root
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *slot != root {
            if let Some(root) = &root {
                // A server killed mid-download cannot delete its staging folder. Remove
                // those leftovers before this root is used again: they are partial media
                // with no `.noudata`, never a library item.
                Self::sweep_stale_stages(root);
            }
            *slot = root;
            // The next read must come from the new root, not the old root's memory copy.
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *cache = CacheSlot::default();
        }
    }

    /// Current library root (if any).
    pub fn library_root(&self) -> Option<PathBuf> {
        self.library_root
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Adjust the sync page size and the polite pause (tests use tiny values).
    pub fn with_sync_tuning(mut self, page_size: u32, delay: Duration) -> Self {
        self.page_size = page_size.clamp(1, plippa_store::PageRequest::MAX_LIMIT);
        self.sync_delay = delay;
        self
    }

    /// Adjust the download caps, free-space probe and the pause between tracks (tests use
    /// small caps and a zero pause; production keeps [`DownloadLimits::from_env`]).
    pub fn with_download_tuning(mut self, limits: DownloadLimits, pause: Duration) -> Self {
        self.download_limits = limits;
        self.download_pause = pause;
        self
    }

    /// Replace the host allowlist used for the store's audio URLs. The default is the
    /// Bandcamp profile (`bandcamp.com` authenticated, `bcbits.com` for redirects).
    pub fn with_store_hosts(mut self, hosts: Arc<AuthenticatedHosts>) -> Self {
        self.hosts = hosts;
        self
    }

    /// Let the server tell the library index about a release a store download placed.
    ///
    /// Without an indexer the files and `.noudata` are still written correctly (and the
    /// periodic scan finds them); the indexer only makes them appear immediately.
    pub fn set_library_indexer(&self, indexer: LibraryIndexer) {
        *self
            .indexer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(indexer);
    }

    /// Let the server delete a library item this hub wants to replace (BCS_05).
    pub fn set_library_deleter(&self, deleter: LibraryDeleter) {
        *self
            .deleter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(deleter);
    }

    /// Share the server's web hub, so Path A fetches go through the same consent-gated
    /// machinery NB_09 uses (one fetcher, one policy, one transport).
    pub fn set_web_hub(&self, web: Arc<crate::web::WebHub>) {
        *self
            .web
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(web);
    }

    /// The store's Subsonic base URL.
    fn base_url(&self) -> &str {
        BANDCAMP_BASE_URL
    }

    /// The secrets directory this hub writes to (safe to log).
    pub fn secrets_dir(&self) -> Option<&std::path::Path> {
        self.secrets.as_ref().map(|secrets| secrets.dir())
    }

    /// Log where credentials would go (the directory only, never a file name).
    pub fn log_startup(&self) {
        match &self.secrets {
            Some(secrets) => log_line(&format!(
                "store credentials directory: {}",
                secrets.dir().display()
            )),
            None => log_line(NO_SECRETS_DIR),
        }
        if let Err(reason) = self.secrets_available() {
            log_line(&format!("store feature unavailable: {reason}"));
        }
    }

    // ------------------------------------------------------------ status

    /// `GET /api/stores`.
    pub fn status(&self) -> StoresStatus {
        self.status_at(now_secs())
    }

    /// [`Self::status`] with an explicit clock (tests, and the auto-sync decision).
    pub fn status_at(&self, now: u64) -> StoresStatus {
        let available = self.secrets_available();
        let connected = available.is_ok() && self.connected();
        let prefs = self.prefs();
        StoresStatus {
            stores: vec![StoreStatus {
                id: STORE_SLUG.to_string(),
                display_name: STORE_DISPLAY_NAME.to_string(),
                connected,
                rejected: self.rejected.load(Ordering::Relaxed),
                last_validated: prefs.last_validated_at.filter(|_| connected),
                capabilities: self.capabilities(),
                cache: self.cache_status(now),
            }],
            secrets_available: available.is_ok(),
            secrets_reason: available.err(),
        }
    }

    /// The `stores` summary `/api/status` carries.
    pub fn summary(&self) -> StoresSummary {
        self.status().summary()
    }

    /// `true` when a credentials file exists and the directory is usable.
    pub fn connected(&self) -> bool {
        self.secrets_available().is_ok()
            && self
                .secrets
                .as_ref()
                .map(|secrets| secrets.is_connected())
                .unwrap_or(false)
    }

    /// Sync worker state.
    pub fn sync_status(&self) -> SyncStatus {
        self.sync
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn cache_status(&self, now: u64) -> CacheStatus {
        // Read the file first: reading is what discovers (and reports) a corrupt file.
        let snapshot = self.cache_snapshot();
        let memory_error = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .error
            .clone();
        match snapshot {
            Some(cache) => CacheStatus {
                releases: cache.releases.len() as u32,
                synced_at: Some(cache.crawled_at).filter(|at| *at > 0),
                stale: cache.is_stale(now),
                complete: cache.complete,
                last_error: cache.last_error.clone().or(memory_error),
            },
            None => CacheStatus {
                releases: 0,
                synced_at: None,
                stale: true,
                complete: false,
                last_error: memory_error,
            },
        }
    }

    fn capabilities(&self) -> Capabilities {
        let Some(creds) = self.load_creds() else {
            return default_capabilities();
        };
        match self.provider(&creds) {
            Ok(provider) => provider.capabilities(),
            Err(_) => default_capabilities(),
        }
    }

    // ---------------------------------------------------------- secrets

    fn probe_now(&self) -> Result<(), String> {
        match &self.secrets {
            Some(secrets) => secrets.probe(),
            None => Err(NO_SECRETS_DIR.to_string()),
        }
    }

    fn secrets_available(&self) -> Result<(), String> {
        let mut slot = self
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(self.probe_now());
        }
        slot.clone().expect("just filled")
    }

    /// Probe the secrets directory for a real operation and report why it is unusable.
    fn require_secrets(&self) -> Result<&StoreSecrets, StoreApiError> {
        let probe = self.probe_now();
        *self
            .available
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(probe.clone());
        match (&self.secrets, probe) {
            (Some(secrets), Ok(())) => Ok(secrets),
            (_, Err(reason)) => Err(StoreApiError::Unavailable(reason)),
            (None, Ok(())) => Err(StoreApiError::Unavailable(NO_SECRETS_DIR.to_string())),
        }
    }

    fn load_creds(&self) -> Option<Credentials> {
        self.secrets.as_ref().and_then(|secrets| secrets.load())
    }

    // ----------------------------------------------------------- provider

    fn provider(&self, creds: &Credentials) -> Result<SubsonicProvider, StoreError> {
        SubsonicProvider::new(
            Arc::clone(&self.transport),
            self.base_url(),
            creds.clone(),
            CLIENT_NAME,
        )
    }

    // --------------------------------------------------------- preferences

    fn prefs(&self) -> StorePrefs {
        match self.library_root() {
            Some(root) => cache::read_prefs(&root),
            None => StorePrefs::default(),
        }
    }

    fn update_prefs(&self, change: impl FnOnce(&mut StorePrefs)) {
        let Some(root) = self.library_root() else {
            return;
        };
        let mut prefs = cache::read_prefs(&root);
        change(&mut prefs);
        prefs.store_id = STORE_SLUG.to_string();
        if let Err(err) = cache::write_prefs(&root, &prefs) {
            log_line(&format!("could not save store preferences: {err}"));
        }
    }

    // ------------------------------------------------------------- cache

    /// The cache as the portal should read it, reloading when the file changed.
    fn cache_snapshot(&self) -> Option<StoreCache> {
        let root = self.library_root()?;
        let path = cache::cache_path(&root, STORE_SLUG);
        let stamp = file_stamp(&path);
        {
            let slot = self
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if slot.stamp == stamp {
                return slot.cache.clone();
            }
        }
        match cache::read_cache(&root) {
            Ok((cache, report)) => {
                let mut slot = self
                    .cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                slot.cache = cache.clone();
                slot.stamp = file_stamp(&path);
                if report.is_some() {
                    slot.error = report;
                }
                cache
            }
            Err(err) => {
                let mut slot = self
                    .cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                slot.error = Some(err);
                slot.cache.clone()
            }
        }
    }

    fn store_cache(&self, cache: StoreCache) -> Result<(), StoreApiError> {
        let Some(root) = self.library_root() else {
            return Err(StoreApiError::Unavailable(
                "the media library root is not available yet".to_string(),
            ));
        };
        let bytes = cache.encode()?;
        let path = cache::cache_path(&root, STORE_SLUG);
        cache::write_private_atomic(&path, &bytes).map_err(StoreApiError::Cache)?;
        let mut slot = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.cache = Some(cache);
        slot.stamp = file_stamp(&path);
        slot.error = None;
        Ok(())
    }

    /// Cached releases, in provider order (empty when nothing is cached yet).
    pub fn cached_releases(&self) -> Vec<StoreRelease> {
        self.cache_snapshot()
            .map(|cache| cache.releases)
            .unwrap_or_default()
    }

    /// One cached release, without touching the network.
    pub fn cached_release(&self, id: &str) -> Option<StoreRelease> {
        let cache = self.cache_snapshot()?;
        cache
            .position(id.trim())
            .map(|index| cache.releases[index].clone())
    }

    /// One release with its tracks, fetched from the store.
    ///
    /// This is the "refresh on miss" path: the answer is written back into the cache
    /// (replacing the entry, or adding it when the id was not cached), so a later
    /// browse of the same release sees it without a request. A cache write failure is
    /// logged, never turned into a failed lookup.
    pub fn get_release(&self, id: &str) -> Result<StoreReleaseDetail, StoreApiError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(StoreApiError::Store(StoreError::NotFound));
        }
        let creds = self.load_creds().ok_or(StoreApiError::NotConnected)?;
        let provider = self.provider(&creds)?;
        let detail = provider.get_release(&StoreReleaseId::new(id))?;
        self.remember_release(detail.release.clone());
        Ok(detail)
    }

    fn remember_release(&self, release: StoreRelease) {
        if self.library_root().is_none() {
            return;
        }
        let mut cache = self
            .cache_snapshot()
            .unwrap_or_else(|| StoreCache::empty(STORE_SLUG, self.page_size));
        match cache.position(release.id.as_str()) {
            Some(index) => cache.releases[index] = release,
            None => cache.releases.push(release),
        }
        if cache.releases.len() > 1 && cache.crawled_at == 0 {
            cache.last_error = Some(
                "the cache was built from single-release lookups, not a full sync".to_string(),
            );
        }
        if let Err(err) = self.store_cache(cache) {
            log_line(&format!(
                "could not cache a release after a lookup: {}",
                err.code()
            ));
        }
    }

    // ------------------------------------------------------------ connect

    /// `POST /api/stores/bandcamp/connect`.
    pub fn connect(
        &self,
        user: &str,
        password: &str,
        consent: Option<bool>,
    ) -> Result<StoreStatus, StoreApiError> {
        if consent != Some(true) {
            return Err(StoreApiError::Consent);
        }
        // Trims both fields and refuses an empty result before any request is made.
        let creds = Credentials::new(user, password).normalised()?;
        let _guard = self.begin_call()?;
        let secrets = self.require_secrets()?;
        let provider = self.provider(&creds)?;
        // A real authenticated call, never `ping` (which says `ok` for a bad password).
        provider.verify_credentials()?;
        secrets.save(&creds).map_err(StoreApiError::Unavailable)?;
        self.rejected.store(false, Ordering::Relaxed);
        let now = now_secs();
        self.update_prefs(|prefs| {
            prefs.auto_sync_consent = true;
            prefs.last_validated_at = Some(now);
        });
        log_line("Bandcamp connected: credentials verified and stored");
        let mut status = self.status_at(now);
        Ok(status.stores.remove(0))
    }

    /// `POST /api/stores/bandcamp/validate` (the "Test" button).
    pub fn validate(&self) -> Result<StoreStatus, StoreApiError> {
        let _guard = self.begin_call()?;
        let secrets = self.require_secrets()?;
        let creds = secrets.load().ok_or(StoreApiError::NotConnected)?;
        let provider = self.provider(&creds)?;
        match provider.verify_credentials() {
            Ok(()) => {
                self.rejected.store(false, Ordering::Relaxed);
                let now = now_secs();
                self.update_prefs(|prefs| prefs.last_validated_at = Some(now));
                log_line("Bandcamp credentials accepted");
                let mut status = self.status_at(now);
                Ok(status.stores.remove(0))
            }
            Err(err) => {
                if err == StoreError::BadCredentials {
                    self.rejected.store(true, Ordering::Relaxed);
                }
                log_line(&format!("Bandcamp credential check failed: {}", err.code()));
                Err(StoreApiError::Store(err))
            }
        }
    }

    /// `DELETE /api/stores/bandcamp`. Idempotent.
    pub fn disconnect(&self) -> Result<(), StoreApiError> {
        self.cancel.store(true, Ordering::SeqCst);
        // A sync in flight must stop before the cache it may want to write is removed,
        // or the write would land after the cleanup and resurrect the file.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && self.sync_status().state == SyncState::Running {
            std::thread::sleep(Duration::from_millis(10));
        }
        if self.sync_status().state == SyncState::Running {
            // It noticed the cancel at its next page boundary and will not write; the
            // request is not held hostage by a slow network call.
            log_line("disconnect: a collection sync is still in flight and will not write the cache");
        }
        // A download uses the credentials this call is about to remove: stop it, and the
        // worker deletes its staging directory before it exits.
        self.cancel_download_any();
        if let Some(secrets) = &self.secrets {
            secrets.remove();
            if secrets.load().is_some() {
                return Err(StoreApiError::Unavailable(
                    "the stored credentials could not be removed".to_string(),
                ));
            }
        }
        if let Some(root) = self.library_root() {
            let path = cache::cache_path(&root, STORE_SLUG);
            // Unlink without an `exists()` pre-check: that would be a second path
            // resolution, and a link is only ever unlinked, never followed (SRV-05).
            // A missing file is the outcome being asked for.
            if let Err(err) = fs::remove_file(&path) {
                if err.kind() != std::io::ErrorKind::NotFound {
                    return Err(StoreApiError::Cache(format!(
                        "cannot remove the collection cache: {err}"
                    )));
                }
            }
            // Preferences carry consent and check times; disconnecting forgets both.
            let _ = fs::remove_file(cache::prefs_path(&root, STORE_SLUG));
            // Recorded ownership associations describe this connection's collection, so
            // they go with it; a stale association would claim a release nobody can verify.
            matching::remove_associations(&root);
        }
        {
            let mut slot = self
                .cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = CacheSlot::default();
        }
        *self
            .sync
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = SyncStatus::default();
        self.rejected.store(false, Ordering::Relaxed);
        log_line("Bandcamp disconnected: credentials and collection cache removed");
        Ok(())
    }

    fn begin_call(&self) -> Result<CallGuard<'_>, StoreApiError> {
        if self
            .connect_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(StoreApiError::Busy);
        }
        Ok(CallGuard(&self.connect_busy))
    }

    // --------------------------------------------------------------- sync

    /// Whether an automatic sync should start now (start-up / periodic check).
    ///
    /// Requires stored credentials, consent recorded when the connection was made, a
    /// cache older than a day (or incomplete), and no sync in the last six hours.
    pub fn auto_sync_due(&self, now: u64) -> bool {
        if !self.connected() {
            return false;
        }
        let prefs = self.prefs();
        if !prefs.auto_sync_consent {
            return false;
        }
        if let Some(last) = prefs.last_sync_at {
            if now.saturating_sub(last) < AUTO_SYNC_MIN_INTERVAL_SECS {
                return false;
            }
        }
        match self.cache_snapshot() {
            Some(cache) => cache.is_stale(now),
            None => true,
        }
    }

    /// `POST /api/stores/bandcamp/sync`. Runs on its own thread and returns at once.
    pub fn start_sync(self: &Arc<Self>) -> Result<SyncStatus, StoreApiError> {
        let creds = self.load_creds().ok_or(StoreApiError::NotConnected)?;
        if self.sync_status().state == SyncState::Running {
            return Err(StoreApiError::Busy);
        }
        let provider = self.provider(&creds)?;
        let now = now_secs();
        self.cancel.store(false, Ordering::SeqCst);
        *self
            .sync
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = SyncStatus {
            state: SyncState::Running,
            started_at: Some(now),
            ..SyncStatus::default()
        };
        // Any sync start counts towards the six-hour floor, so a failing store is not
        // retried every minute.
        self.update_prefs(|prefs| prefs.last_sync_at = Some(now));

        let hub = Arc::clone(self);
        if let Err(err) = std::thread::Builder::new()
            .name("plippa-store-sync".to_string())
            .spawn(move || hub.run_sync(provider))
        {
            let message = format!("cannot start the sync worker: {err}");
            self.finish_sync(SyncState::Failed, 0, 0, Some(message.clone()), Some("unavailable"), false);
            return Err(StoreApiError::Unavailable(message));
        }
        log_line("collection sync started");
        Ok(self.sync_status())
    }

    /// `POST /api/stores/bandcamp/sync/cancel`.
    pub fn cancel_sync(&self) -> SyncStatus {
        self.cancel.store(true, Ordering::SeqCst);
        self.sync_status()
    }

    fn run_sync(&self, provider: SubsonicProvider) {
        let options = CrawlOptions {
            page_size: self.page_size,
            max_pages: None,
            max_releases: MAX_CRAWL_RELEASES,
        };
        let result = provider.crawl_releases(options, |progress| {
            {
                let mut status = self
                    .sync
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                status.pages = progress.pages_done;
                status.releases = progress.releases_so_far;
            }
            if self.cancel.load(Ordering::SeqCst) {
                return CrawlDecision::Stop;
            }
            if !self.polite_pause() {
                return CrawlDecision::Stop;
            }
            CrawlDecision::Continue
        });

        let crawl = match result {
            Ok(crawl) => crawl,
            Err(err) => {
                let status = self.sync_status();
                self.finish_sync(
                    SyncState::Failed,
                    status.pages,
                    status.releases,
                    Some(err.to_string()),
                    Some(StoreApiError::Store(err.clone()).code()),
                    false,
                );
                log_line(&format!("collection sync failed: {}", err.code()));
                return;
            }
        };

        let pages = crawl.pages.len() as u32;
        let releases = crawl.releases.len() as u32;
        if crawl.stop == CrawlStop::Cancelled || self.cancel.load(Ordering::SeqCst) {
            self.finish_sync(SyncState::Idle, pages, releases, None, None, true);
            log_line(&format!(
                "collection sync cancelled after {pages} page(s); the previous cache is kept"
            ));
            return;
        }

        let complete = crawl.is_complete();
        let last_error = match crawl.stop {
            CrawlStop::PageCap => {
                Some("the crawl reached its page limit; the cache is incomplete".to_string())
            }
            CrawlStop::ReleaseCap => Some(format!(
                "the crawl reached the {MAX_CRAWL_RELEASES}-release limit; the cache is incomplete"
            )),
            _ => None,
        };
        let cache = StoreCache {
            store_id: STORE_SLUG.to_string(),
            releases: crawl.releases,
            page_size: self.page_size,
            crawled_at: now_secs(),
            complete,
            last_error,
        };
        match self.store_cache(cache) {
            Ok(()) => {
                self.finish_sync(SyncState::Done, pages, releases, None, None, false);
                log_line(&format!(
                    "collection sync finished: {releases} release(s) in {pages} page(s)"
                ));
            }
            Err(err) => {
                self.finish_sync(
                    SyncState::Failed,
                    pages,
                    releases,
                    Some(err.message()),
                    Some(err.code()),
                    false,
                );
                log_line(&format!(
                    "collection sync could not be stored: {}",
                    err.code()
                ));
            }
        }
    }

    /// Sleep `sync_delay` between pages in slices, so a cancel is noticed at once.
    fn polite_pause(&self) -> bool {
        let deadline = Instant::now() + self.sync_delay;
        loop {
            if self.cancel.load(Ordering::SeqCst) {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
        }
    }

    fn finish_sync(
        &self,
        state: SyncState,
        pages: u32,
        releases: u32,
        error: Option<String>,
        error_code: Option<&str>,
        cancelled: bool,
    ) {
        let mut status = self
            .sync
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        status.state = state;
        status.pages = pages;
        status.releases = releases;
        status.error = error;
        status.error_code = error_code.map(str::to_string);
        status.finished_at = Some(now_secs());
        status.cancelled = cancelled;
    }
}

impl Default for StoreHub {
    fn default() -> Self {
        Self::production(None)
    }
}

/// Releases the in-flight gate when the call ends, however it ends.
struct CallGuard<'a>(&'a AtomicBool);

impl Drop for CallGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn default_capabilities() -> Capabilities {
    Capabilities {
        download: false,
        download_unverified: true,
        stream: true,
        cover_art: true,
        playlists: false,
    }
}

fn file_stamp(path: &std::path::Path) -> Option<(u64, u32, u64)> {
    // `symlink_metadata`: the stamp describes the entry we would read, and a link is
    // never followed anywhere else on these paths (SRV-05).
    let meta = fs::symlink_metadata(path).ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((modified.as_secs(), modified.subsec_nanos(), meta.len()))
}

/// Start the automatic-sync worker (BCS_03: on start-up when due, never mid-playback).
///
/// The worker wakes once a minute and is cheap when it does: it reads preferences, the
/// cache stamp and the engine's transport label, and only then starts a sync. It never
/// waits on a sync it started (the sync runs on its own thread) and it stops promptly
/// when the process-wide stop flag is set.
pub fn spawn_store_sync_worker(
    hub: Arc<StoreHub>,
    service: Arc<Mutex<ServerService>>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("plippa-store-autosync".to_string())
        .spawn(move || loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let playing = {
                let mut guard = match service.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard
                    .now_playing()
                    .transport
                    .eq_ignore_ascii_case("playing")
            };
            if !playing && hub.auto_sync_due(now_secs()) {
                match hub.start_sync() {
                    Ok(_) => log_line("automatic collection sync started"),
                    Err(err) => {
                        log_line(&format!("automatic sync not started: {}", err.code()));
                    }
                }
            }
            // Wake every second so a stop signal is honoured without waiting a minute.
            for _ in 0..AUTO_SYNC_POLL_SECS {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
}
