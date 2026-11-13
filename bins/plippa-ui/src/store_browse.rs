//! BCS_08: the device's store screens — connect by QR, browse the collection, download,
//! and take a downloaded release off the device again.
//!
//! Five screens, each a small state machine a test can build without a server:
//!
//! ```text
//! Stores ──(Bandcamp connected)──▶ Releases ──▶ Release ──▶ (Download → Confirm → Progress)
//!    │                                  │            └──▶ Delete (remove from Plippa)
//!    └──(not connected)──▶ Connect (QR of the portal address)
//! ```
//!
//! The views own no I/O: [`crate::app::UiApp`] turns their actions into
//! [`crate::store_worker::StoreRequest`]s and folds the answers back in. Every failure is a
//! [`StoreBlocked`] the painter turns into a sentence, so an offline service, a refused
//! sign-in, a busy store and a playing item never look the same on the screen.

use crate::store_api::{
    CollectionPageInfo, DeleteDone, DeletePreviewInfo, DownloadConsent, DownloadProgress,
    DownloadSession, DownloadState, PortalAddress, ReleaseInfo, ReleaseRow, StoreApiError,
    StoresInfo, SyncPhase, SyncStatus,
};
use crate::store_tap::{STORE_TIMEOUT_MS, SYNC_POLL_MS};

// ---------------------------------------------------------------------------
// Why a screen cannot do what was asked
// ---------------------------------------------------------------------------

/// A reason a store screen cannot continue, in the person's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreBlocked {
    /// The local service is not running or not reachable (Wi-Fi off, service stopped).
    ServiceDown,
    /// The store refused the stored sign-in (401 / 403): a fresh connect fixes it.
    Credentials,
    /// The store is rate-limiting (429): wait a minute.
    Busy,
    /// The item is playing (409 `playing`): stop playback first.
    Playing,
    /// The server wanted a confirmation that was not sent (409 `confirm_required`).
    ConfirmRequired,
    /// Removing the item is refused by policy (409 `blocked`).
    Blocked,
    /// The last file of a single-track item cannot be removed (400 `last_file`).
    LastFile,
    /// No store is connected to this Plippa.
    NoStore,
    /// The server refused and said why.
    Refused,
    /// The answer was not something this build understands.
    Malformed,
}

impl StoreBlocked {
    /// The heading the screen shows.
    pub fn title(self) -> &'static str {
        match self {
            StoreBlocked::ServiceDown => "Plippa's service is not running",
            StoreBlocked::Credentials => "Bandcamp refused the sign-in",
            StoreBlocked::Busy => "Bandcamp is busy",
            StoreBlocked::Playing => "This item is playing",
            StoreBlocked::ConfirmRequired => "Confirmation needed",
            StoreBlocked::Blocked => "Plippa won't remove this",
            StoreBlocked::LastFile => "That is the item's only file",
            StoreBlocked::NoStore => "No store is connected",
            StoreBlocked::Refused => "The store service refused this",
            StoreBlocked::Malformed => "The store service sent something unexpected",
        }
    }

    /// What the person can do about it.
    pub fn hint(self) -> &'static str {
        match self {
            StoreBlocked::ServiceDown => "Check the Wi-Fi, then try again",
            StoreBlocked::Credentials => "Connect Bandcamp again",
            StoreBlocked::Busy => "Wait a minute, then try again",
            StoreBlocked::Playing => "Stop playback, then try again",
            StoreBlocked::ConfirmRequired => "Try again from this screen",
            StoreBlocked::Blocked => "See the portal for why",
            StoreBlocked::LastFile => "Remove the whole item instead",
            StoreBlocked::NoStore => "Connect Bandcamp first",
            StoreBlocked::Refused => "Try again, or use the portal",
            StoreBlocked::Malformed => "Try again",
        }
    }

    /// True when trying the same call again could work.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            StoreBlocked::ServiceDown
                | StoreBlocked::Busy
                | StoreBlocked::ConfirmRequired
                | StoreBlocked::Malformed
                | StoreBlocked::Refused
        )
    }
}

/// "Last synced 3 h ago" for the Stores screen, from the cache's write time. `None` means the
/// collection has never been synced on this stick. Pure: the caller supplies the clock.
pub fn synced_text(synced_at: Option<u64>, now_unix: u64) -> String {
    let Some(at) = synced_at else {
        return "Not synced yet".to_string();
    };
    let age = now_unix.saturating_sub(at);
    let span = match age {
        0..=59 => return "Last synced just now".to_string(),
        60..=3_599 => format!("{} min", age / 60),
        3_600..=86_399 => format!("{} h", age / 3_600),
        _ => format!("{} d", age / 86_400),
    };
    format!("Last synced {span} ago")
}

/// Map a failed call onto the state the screen shows.
pub fn blocked_for(error: &StoreApiError) -> StoreBlocked {
    match error {
        StoreApiError::Offline(_) => StoreBlocked::ServiceDown,
        StoreApiError::Malformed(_) => StoreBlocked::Malformed,
        StoreApiError::Refused { status, code, .. } => match status {
            401 | 403 => StoreBlocked::Credentials,
            429 => StoreBlocked::Busy,
            409 => match code.as_deref() {
                Some("playing") => StoreBlocked::Playing,
                Some("confirm_required") => StoreBlocked::ConfirmRequired,
                Some("blocked") => StoreBlocked::Blocked,
                _ => StoreBlocked::Refused,
            },
            400 if code.as_deref() == Some("last_file") => StoreBlocked::LastFile,
            _ => StoreBlocked::Refused,
        },
    }
}

/// The sentence a blocked state carries: the server's own words, never a code.
pub fn blocked_message(error: &StoreApiError) -> String {
    let message = error.message();
    let blocked = blocked_for(error);
    if message.trim().is_empty() {
        return format!("{}. {}.", blocked.title(), blocked.hint());
    }
    format!("{}. {message}", blocked.title())
}

// ---------------------------------------------------------------------------
// Stores: is anything connected?
// ---------------------------------------------------------------------------

/// What the Stores screen knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoresPhase {
    /// Asking `GET /api/stores`.
    Loading,
    /// The answer arrived.
    Ready(StoresInfo),
    /// The answer could not be fetched or understood.
    Blocked {
        blocked: StoreBlocked,
        message: String,
    },
}

/// One row of the Stores screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoresAction {
    /// The Bandcamp row: the collection when connected, the connect QR when not.
    Bandcamp,
    /// BCS_11: sync the collection from the device. While a sync runs the row stays, shows the
    /// progress in its value column and cannot be chosen.
    Sync,
    /// Show the connect QR again (a new sign-in).
    Connect,
    /// Ask the server again.
    Refresh,
    /// Retry after a failure.
    Retry,
    Back,
}

/// The Stores screen (Library > Stores and System > Stores).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoresView {
    pub phase: StoresPhase,
    /// Loop time the current call was made (drives the timeout).
    pub requested_at_ms: u64,
    /// BCS_11: the server's last sync status. The poll is stateless, so a screen entered while
    /// a sync runs picks it up from the first answer.
    pub sync: Option<SyncStatus>,
    /// BCS_11: an answer to a sync request is awaited or the sync is running, so the screen
    /// keeps polling once a second. Cleared by a terminal state.
    pub sync_polling: bool,
    /// BCS_11: this screen saw the sync running (or started it), so finishing is news worth a
    /// status flash; a stale `done` from an earlier run is not.
    pub sync_watching: bool,
    /// BCS_11: loop time the next sync poll is due.
    pub sync_due_ms: u64,
    /// BCS_11: why starting or following the sync failed, when it did not come from the
    /// server's own `error`.
    pub sync_error: Option<String>,
    /// BCS_11: when the cache was last written (Unix seconds), from the collection answer.
    pub synced_at: Option<u64>,
}

impl Default for StoresView {
    fn default() -> Self {
        Self::loading(0)
    }
}

/// What folding a sync answer into the Stores screen means for the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncNote {
    /// Nothing to announce.
    None,
    /// The sync this screen was watching finished: say how many releases it holds.
    Finished { releases: u32 },
}

impl StoresView {
    /// A view waiting for the status.
    pub fn loading(now_ms: u64) -> Self {
        Self {
            phase: StoresPhase::Loading,
            requested_at_ms: now_ms,
            sync: None,
            sync_polling: false,
            sync_watching: false,
            sync_due_ms: 0,
            sync_error: None,
            synced_at: None,
        }
    }

    /// A sync is running on the server.
    pub fn syncing(&self) -> bool {
        self.sync.as_ref().is_some_and(|s| s.state == SyncPhase::Running)
    }

    /// The value column of the Sync row while a sync runs: `Syncing… 3/120` (pages/releases).
    pub fn sync_value(&self) -> Option<String> {
        let status = self.sync.as_ref().filter(|s| s.state == SyncPhase::Running)?;
        Some(format!("Syncing… {}/{}", status.pages, status.releases))
    }

    /// The sentence for a failed sync: the server's own words, else the call's.
    pub fn sync_failure(&self) -> Option<String> {
        if let Some(error) = &self.sync_error {
            return Some(error.clone());
        }
        let status = self.sync.as_ref().filter(|s| s.state == SyncPhase::Failed)?;
        Some(
            status
                .error
                .clone()
                .unwrap_or_else(|| "The sync stopped before it finished.".to_string()),
        )
    }

    /// Whether Enter on `action` does anything: the Sync row is not selectable while syncing.
    pub fn enabled(&self, action: StoresAction) -> bool {
        action != StoresAction::Sync || !self.syncing()
    }

    /// Begin following the sync: a request is out, so poll until a terminal answer.
    pub fn sync_requested(&mut self, now_ms: u64) {
        self.sync_polling = true;
        self.sync_watching = true;
        self.sync_error = None;
        self.sync_due_ms = now_ms.saturating_add(SYNC_POLL_MS);
    }

    /// Fold one sync answer in. Polling stops at a terminal state, and a `busy` refusal (a sync
    /// is already running, started elsewhere) is followed instead of reported.
    pub fn apply_sync(
        &mut self,
        result: Result<SyncStatus, StoreApiError>,
        now_ms: u64,
    ) -> SyncNote {
        match result {
            Ok(status) => {
                let state = status.state;
                let releases = status.releases;
                self.sync = Some(status);
                match state {
                    SyncPhase::Running => {
                        self.sync_polling = true;
                        self.sync_watching = true;
                        self.sync_error = None;
                        self.sync_due_ms = now_ms.saturating_add(SYNC_POLL_MS);
                        SyncNote::None
                    }
                    SyncPhase::Done => {
                        let announce = self.sync_watching;
                        self.sync_polling = false;
                        self.sync_watching = false;
                        self.sync_error = None;
                        if announce {
                            SyncNote::Finished { releases }
                        } else {
                            SyncNote::None
                        }
                    }
                    SyncPhase::Failed | SyncPhase::Idle => {
                        self.sync_polling = false;
                        self.sync_watching = false;
                        SyncNote::None
                    }
                }
            }
            Err(error) if error.status() == Some(429) && error.code() == Some("busy") => {
                // Somebody else's sync is running: follow it.
                self.sync_polling = true;
                self.sync_watching = true;
                self.sync_due_ms = now_ms.saturating_add(SYNC_POLL_MS);
                SyncNote::None
            }
            Err(error) => {
                self.sync_polling = false;
                self.sync_watching = false;
                self.sync = None;
                self.sync_error = Some(blocked_message(&error));
                SyncNote::None
            }
        }
    }

    /// True when the next sync poll is due.
    pub fn sync_poll_due(&self, now_ms: u64) -> bool {
        self.sync_polling && now_ms >= self.sync_due_ms
    }

    /// Fold the status answer in.
    pub fn apply(&mut self, result: Result<StoresInfo, StoreApiError>) {
        self.phase = match result {
            Ok(info) => StoresPhase::Ready(info),
            Err(error) => StoresPhase::Blocked {
                blocked: blocked_for(&error),
                message: blocked_message(&error),
            },
        };
    }

    /// The Bandcamp row: connected (not refused), refused, or absent.
    pub fn bandcamp(&self) -> Option<&crate::store_api::StoreInfo> {
        match &self.phase {
            StoresPhase::Ready(info) => info.bandcamp(),
            _ => None,
        }
    }

    /// A stored connection that the last check did not refuse.
    pub fn connected(&self) -> bool {
        self.bandcamp().is_some_and(|s| s.connected && !s.rejected)
    }

    /// The sign-in exists but Bandcamp refused it.
    pub fn rejected(&self) -> bool {
        self.bandcamp().is_some_and(|s| s.rejected)
    }

    /// The one-line status the Bandcamp row shows.
    pub fn status_text(&self) -> String {
        match &self.phase {
            StoresPhase::Loading => "Checking…".to_string(),
            StoresPhase::Blocked { .. } => "Unavailable".to_string(),
            StoresPhase::Ready(info) => {
                if let Some(store) = info.bandcamp() {
                    if store.rejected {
                        "Sign-in refused".to_string()
                    } else if store.connected {
                        if store.cached_releases == 0 {
                            "Connected".to_string()
                        } else {
                            format!("{} releases", store.cached_releases)
                        }
                    } else {
                        "Not connected".to_string()
                    }
                } else if !info.secrets_available {
                    "No secrets folder".to_string()
                } else {
                    "Not connected".to_string()
                }
            }
        }
    }

    /// Why Bandcamp cannot be used at all, when the server says the feature is unavailable.
    pub fn unavailable_reason(&self) -> Option<&str> {
        match &self.phase {
            StoresPhase::Ready(info) if !info.secrets_available => {
                info.secrets_reason.as_deref().or(Some("no secrets folder"))
            }
            _ => None,
        }
    }

    /// The failure message, when the status call failed.
    pub fn blocked(&self) -> Option<(StoreBlocked, &str)> {
        match &self.phase {
            StoresPhase::Blocked { blocked, message } => Some((*blocked, message)),
            _ => None,
        }
    }

    /// The rows, in draw order.
    pub fn actions(&self) -> Vec<StoresAction> {
        match &self.phase {
            StoresPhase::Blocked { blocked, .. } => {
                let mut rows = Vec::new();
                if blocked.retryable() {
                    rows.push(StoresAction::Retry);
                }
                rows.push(StoresAction::Back);
                rows
            }
            StoresPhase::Loading | StoresPhase::Ready(_) => {
                let mut rows = vec![StoresAction::Bandcamp];
                if self.connected() {
                    rows.push(StoresAction::Sync);
                }
                rows.extend([
                    StoresAction::Connect,
                    StoresAction::Refresh,
                    StoresAction::Back,
                ]);
                rows
            }
        }
    }

    /// True when a call has been outstanding for too long.
    pub fn timed_out(&self, now_ms: u64) -> bool {
        matches!(self.phase, StoresPhase::Loading)
            && now_ms.saturating_sub(self.requested_at_ms) >= STORE_TIMEOUT_MS
    }
}

// ---------------------------------------------------------------------------
// Connect: the QR code for the portal
// ---------------------------------------------------------------------------

/// What the connect screen knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectPhase {
    /// Asking `/api/portal/url`.
    Looking,
    /// The server answered. `address.recommended` may still be `None` (nothing reachable).
    Ready(PortalAddress),
    /// The web portal is switched off, so there is nothing for a phone to open (SECFIX_02).
    PortalOff,
    /// The address could not be fetched.
    Unreachable {
        blocked: StoreBlocked,
        message: String,
    },
}

/// One row of the connect screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectAction {
    /// Ask the server again (after Wi-Fi came back, or a new address).
    Retry,
    Back,
}

/// The instruction under the QR code, in one line.
pub const CONNECT_INSTRUCTION: &str = "Open this on your phone, connect Bandcamp there";

/// SECFIX_03: the portal advertises `http://`, so the password typed on the phone crosses the
/// network unencrypted. The screen says so rather than silently letting it happen.
pub const CONNECT_CLEAR_TEXT_WARNING: &str =
    "No HTTPS: your Bandcamp password crosses the network in clear text";

/// The Connect Bandcamp screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectView {
    pub phase: ConnectPhase,
    /// Loop time of the current call (drives the timeout).
    pub requested_at_ms: u64,
}

impl Default for ConnectView {
    fn default() -> Self {
        Self {
            phase: ConnectPhase::Looking,
            requested_at_ms: 0,
        }
    }
}

impl ConnectView {
    /// A view waiting for the address.
    pub fn looking(now_ms: u64) -> Self {
        Self {
            phase: ConnectPhase::Looking,
            requested_at_ms: now_ms,
        }
    }

    /// Fold the portal answer in.
    pub fn apply(&mut self, result: Result<PortalAddress, StoreApiError>) {
        self.phase = match result {
            Ok(address) if address.portal_off => ConnectPhase::PortalOff,
            Ok(address) => ConnectPhase::Ready(address),
            Err(error) => ConnectPhase::Unreachable {
                blocked: blocked_for(&error),
                message: blocked_message(&error),
            },
        };
    }

    /// The address to encode: only a recommended address ever becomes a QR code.
    pub fn qr_url(&self) -> Option<&str> {
        match &self.phase {
            ConnectPhase::Ready(address) => address.recommended.as_deref(),
            _ => None,
        }
    }

    /// The portal this QR points at is served without TLS, so a password typed on it can be
    /// read off the network. Decided from the advertised URL itself, so the screen cannot
    /// disagree with the address it shows.
    pub fn clear_text_warning(&self) -> Option<&'static str> {
        let ConnectPhase::Ready(address) = &self.phase else {
            return None;
        };
        let url = address.recommended.as_deref()?;
        url.starts_with("http://").then_some(CONNECT_CLEAR_TEXT_WARNING)
    }

    /// The address as text (scheme dropped: the person retypes the host and port).
    pub fn address_text(&self) -> Option<String> {
        match &self.phase {
            ConnectPhase::Ready(address) => address.short(),
            _ => None,
        }
    }

    /// Why there is nothing to scan, when the server had no address to advertise.
    pub fn note(&self) -> Option<&str> {
        match &self.phase {
            ConnectPhase::Ready(address) if address.recommended.is_none() => {
                address.note.as_deref().or(Some(
                    "No address another device can reach. Connect this Plippa to Wi-Fi.",
                ))
            }
            _ => None,
        }
    }

    /// The unreachable message, when the call failed.
    pub fn blocked(&self) -> Option<(StoreBlocked, &str)> {
        match &self.phase {
            ConnectPhase::Unreachable { blocked, message } => Some((*blocked, message)),
            _ => None,
        }
    }

    /// The rows, in draw order.
    pub fn actions(&self) -> Vec<ConnectAction> {
        match &self.phase {
            ConnectPhase::Looking => vec![ConnectAction::Back],
            ConnectPhase::Ready(_) | ConnectPhase::PortalOff | ConnectPhase::Unreachable { .. } => {
                vec![ConnectAction::Retry, ConnectAction::Back]
            }
        }
    }

    /// True when the address call has been outstanding for too long.
    pub fn timed_out(&self, now_ms: u64) -> bool {
        matches!(self.phase, ConnectPhase::Looking)
            && now_ms.saturating_sub(self.requested_at_ms) >= STORE_TIMEOUT_MS
    }
}

// ---------------------------------------------------------------------------
// Releases: the collection, one page at a time
// ---------------------------------------------------------------------------

/// How many releases one page asks for (a 320x240 screen shows eight rows, so a page is a
/// few screens' worth and the "More" row is reached rarely).
pub const RELEASE_PAGE_SIZE: u32 = 20;

/// What the release list knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleasesPhase {
    /// The first page is on its way.
    Loading,
    /// At least one page arrived (it may hold no rows).
    Ready,
    /// A later page is on its way; the rows already loaded stay on screen.
    LoadingMore,
    /// The first page failed: nothing to show.
    Blocked {
        blocked: StoreBlocked,
        message: String,
    },
    /// A later page failed: the loaded rows stay, and say the rest is missing.
    PageFailed {
        blocked: StoreBlocked,
        message: String,
    },
}

/// One row of the release list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasesRow {
    /// Index into [`ReleasesView::releases`].
    Release(usize),
    /// Ask the server for the next page.
    More,
    /// Retry the failed page.
    Retry,
    /// Type a query on the on-screen keyboard.
    Search,
    Back,
}

/// The Bandcamp collection list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasesView {
    pub phase: ReleasesPhase,
    /// Rows loaded so far, in the server's order.
    pub releases: Vec<ReleaseRow>,
    /// Releases matching the query across every page.
    pub total: u32,
    /// Offset of the next page (`None` on the last page).
    pub next_offset: Option<u32>,
    /// The collection cache is stale, so a short list may be a lie.
    pub stale: bool,
    /// The query the rows were fetched for (empty = the whole collection).
    pub query: String,
    /// The person is typing a query: the keyboard owns the screen.
    pub searching: bool,
    /// Loop time of the call in flight.
    pub requested_at_ms: u64,
}

impl Default for ReleasesView {
    fn default() -> Self {
        Self {
            phase: ReleasesPhase::Loading,
            releases: Vec::new(),
            total: 0,
            next_offset: None,
            stale: false,
            query: String::new(),
            searching: false,
            requested_at_ms: 0,
        }
    }
}

impl ReleasesView {
    /// A fresh list, waiting for its first page.
    pub fn loading(now_ms: u64) -> Self {
        Self {
            phase: ReleasesPhase::Loading,
            requested_at_ms: now_ms,
            ..Self::default()
        }
    }

    /// Fold the first page in (replaces anything loaded).
    pub fn apply_first(&mut self, result: Result<CollectionPageInfo, StoreApiError>) {
        match result {
            Ok(page) => {
                let total = page.total;
                let next = page.next_offset();
                let stale = page.stale;
                self.releases = page.releases;
                self.total = total;
                self.next_offset = next;
                self.stale = stale;
                self.phase = ReleasesPhase::Ready;
            }
            Err(error) => {
                self.phase = ReleasesPhase::Blocked {
                    blocked: blocked_for(&error),
                    message: blocked_message(&error),
                };
            }
        }
    }

    /// Fold a later page in. A page for another offset is ignored (it is not this list's).
    pub fn apply_more(
        &mut self,
        offset: u32,
        result: Result<CollectionPageInfo, StoreApiError>,
    ) -> bool {
        if self.next_offset != Some(offset) {
            return false;
        }
        match result {
            Ok(page) => {
                // The server is authoritative about order: append and dedupe by id so a
                // release that arrived twice cannot appear twice.
                let total = page.total;
                let stale = page.stale;
                let next = page.next_offset();
                let empty = page.releases.is_empty();
                for release in page.releases {
                    if !self.releases.iter().any(|r| r.id == release.id) {
                        self.releases.push(release);
                    }
                }
                // A page that added no new row must not leave `next_offset` pointing at
                // itself, or the More row would loop forever.
                self.next_offset = if empty { None } else { next };
                self.total = total;
                self.stale |= stale;
                self.phase = ReleasesPhase::Ready;
            }
            Err(error) => {
                self.phase = ReleasesPhase::PageFailed {
                    blocked: blocked_for(&error),
                    message: blocked_message(&error),
                };
            }
        }
        true
    }

    /// Take the failure of the call in flight (timeout, dead worker).
    pub fn fail(&mut self, error: &StoreApiError) {
        let blocked = blocked_for(error);
        let message = blocked_message(error);
        self.phase = if self.releases.is_empty() {
            ReleasesPhase::Blocked { blocked, message }
        } else {
            ReleasesPhase::PageFailed { blocked, message }
        };
    }

    /// Show the query keyboard. The keyboard itself belongs to the screen that types.
    pub fn begin_search(&mut self) {
        if matches!(
            self.phase,
            ReleasesPhase::Ready | ReleasesPhase::PageFailed { .. }
        ) {
            self.searching = true;
        }
    }

    /// Close the keyboard without changing the list.
    pub fn cancel_search(&mut self) {
        self.searching = false;
    }

    /// Show `query` from its first page. The rows loaded for the last query are dropped: a
    /// search result must never be appended to a list fetched for something else.
    pub fn begin_query(&mut self, query: impl Into<String>, now_ms: u64) {
        self.query = query.into();
        self.searching = false;
        self.releases.clear();
        self.total = 0;
        self.next_offset = None;
        self.stale = false;
        self.phase = ReleasesPhase::Loading;
        self.requested_at_ms = now_ms;
    }

    /// Start fetching the next page.
    pub fn begin_more(&mut self, now_ms: u64) {
        self.phase = ReleasesPhase::LoadingMore;
        self.requested_at_ms = now_ms;
    }

    /// The list is ready and holds nothing.
    pub fn is_empty(&self) -> bool {
        self.phase == ReleasesPhase::Ready && self.releases.is_empty()
    }

    /// The rows, in draw order: releases, then the paging / retry / search row, then Back.
    /// The keyboard owns the screen while a query is being typed, so there are no rows.
    pub fn rows(&self) -> Vec<ReleasesRow> {
        if self.searching {
            return Vec::new();
        }
        let mut rows: Vec<ReleasesRow> =
            (0..self.releases.len()).map(ReleasesRow::Release).collect();
        match &self.phase {
            ReleasesPhase::Blocked { blocked, .. } => {
                if blocked.retryable() {
                    rows.push(ReleasesRow::Retry);
                }
            }
            ReleasesPhase::PageFailed { blocked, .. } => {
                if blocked.retryable() {
                    rows.push(ReleasesRow::Retry);
                }
            }
            ReleasesPhase::Ready => {
                if self.next_offset.is_some() {
                    rows.push(ReleasesRow::More);
                }
                // The device has no comfortable typing surface, but the collection is a
                // cache: the same keyboard that enters a Wi-Fi password can search it.
                rows.push(ReleasesRow::Search);
            }
            ReleasesPhase::Loading | ReleasesPhase::LoadingMore => {}
        }
        rows.push(ReleasesRow::Back);
        rows
    }

    /// True when a page has been outstanding for too long.
    pub fn timed_out(&self, now_ms: u64) -> bool {
        matches!(
            self.phase,
            ReleasesPhase::Loading | ReleasesPhase::LoadingMore
        ) && now_ms.saturating_sub(self.requested_at_ms) >= STORE_TIMEOUT_MS
    }
}

// ---------------------------------------------------------------------------
// One release: tracks, size, quality, download
// ---------------------------------------------------------------------------

/// Where the release detail screen is with the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleasePhase {
    /// Asking for the release.
    Loading,
    /// The release arrived.
    Ready,
    /// The release could not be fetched.
    Blocked {
        blocked: StoreBlocked,
        message: String,
    },
}

/// Where the download is.
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadPhase {
    /// Nothing has been asked.
    Idle,
    /// Asking the server what the download would fetch.
    Preparing,
    /// The server priced it; the person confirms.
    Confirm { consent: DownloadConsent },
    /// Running (or just stopped, until the next status poll).
    Running {
        session: String,
        progress: DownloadProgress,
    },
    /// Placed in the library.
    Done { files: usize, partial: bool },
    /// The download or the call failed.
    Failed { message: String, retry: bool },
}

/// A release this session downloaded, and where it went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedRelease {
    /// Library-relative `.noudata`, when the server named one.
    pub noudata: Option<String>,
    /// Files placed in the library.
    pub files: usize,
}

/// One row of the release detail screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseAction {
    /// Price it (Idle) or start it (Confirm).
    Download,
    /// Stop the running download.
    CancelDownload,
    /// Show the downloaded item in the library.
    OpenInLibrary,
    /// Remove the downloaded item from this device.
    Remove,
    /// Try the failed call again.
    Retry,
    Back,
}

/// The release detail screen.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseView {
    /// Store id of the release being shown.
    pub release_id: String,
    pub phase: ReleasePhase,
    pub info: Option<ReleaseInfo>,
    pub download: DownloadPhase,
    /// Set once this session downloaded the release; cleared by a successful remove.
    pub downloaded: Option<DownloadedRelease>,
    /// The Noubin key a download should be united against, when one is known.
    pub noubin_key: Option<String>,
    /// Loop time of the call in flight.
    pub requested_at_ms: u64,
    /// Loop time the next progress poll is due.
    pub poll_at_ms: u64,
}

impl ReleaseView {
    /// A view waiting for `release_id`.
    pub fn loading(release_id: impl Into<String>, now_ms: u64) -> Self {
        Self {
            release_id: release_id.into(),
            phase: ReleasePhase::Loading,
            info: None,
            download: DownloadPhase::Idle,
            downloaded: None,
            noubin_key: None,
            requested_at_ms: now_ms,
            poll_at_ms: 0,
        }
    }

    /// Fold the release answer in.
    pub fn apply_info(&mut self, result: Result<ReleaseInfo, StoreApiError>) {
        match result {
            Ok(info) => {
                self.phase = ReleasePhase::Ready;
                self.info = Some(info);
            }
            Err(error) => {
                self.phase = ReleasePhase::Blocked {
                    blocked: blocked_for(&error),
                    message: blocked_message(&error),
                };
            }
        }
    }

    /// Start pricing the download.
    pub fn begin_prepare(&mut self, now_ms: u64) {
        self.download = DownloadPhase::Preparing;
        self.requested_at_ms = now_ms;
    }

    /// Fold the priced download in.
    pub fn apply_consent(&mut self, result: Result<DownloadConsent, StoreApiError>) {
        self.download = match result {
            Ok(consent) => DownloadPhase::Confirm { consent },
            Err(error) => DownloadPhase::Failed {
                message: blocked_message(&error),
                retry: blocked_for(&error).retryable(),
            },
        };
    }

    /// Fold the started session in. A session answer for a screen that is no longer asking
    /// is ignored (it belongs to something that moved on).
    pub fn apply_session(
        &mut self,
        result: Result<DownloadSession, StoreApiError>,
        now_ms: u64,
    ) -> bool {
        if !matches!(
            self.download,
            DownloadPhase::Confirm { .. } | DownloadPhase::Preparing
        ) {
            return false;
        }
        self.download = match result {
            Ok(session) => {
                self.poll_at_ms = now_ms.saturating_add(crate::store_tap::DOWNLOAD_POLL_MS);
                DownloadPhase::Running {
                    session: session.session,
                    progress: DownloadProgress::queued(),
                }
            }
            Err(error) => DownloadPhase::Failed {
                message: blocked_message(&error),
                retry: blocked_for(&error).retryable(),
            },
        };
        true
    }

    /// Fold a status answer in; `false` when it belongs to another session.
    pub fn apply_progress(&mut self, session: &str, progress: &DownloadProgress) -> bool {
        let DownloadPhase::Running {
            session: current, ..
        } = &mut self.download
        else {
            return false;
        };
        if current != session {
            return false;
        }
        if progress.state == DownloadState::Running {
            self.download = DownloadPhase::Running {
                session: session.to_string(),
                progress: progress.clone(),
            };
            return true;
        }
        // A finished session that added nothing and found nothing is a failure, not a
        // success. A replay (the release was already in the library) is a success with
        // zero new files.
        if matches!(
            progress.state,
            DownloadState::Done | DownloadState::Partial
        ) {
            self.download = if progress.placed_files > 0 || progress.already_present {
                self.downloaded = Some(DownloadedRelease {
                    noudata: progress.noudata.clone(),
                    files: progress.placed_files,
                });
                DownloadPhase::Done {
                    files: progress.placed_files,
                    partial: progress.state == DownloadState::Partial,
                }
            } else {
                DownloadPhase::Failed {
                    message: "The download finished but nothing reached the library."
                        .to_string(),
                    retry: true,
                }
            };
            return true;
        }
        self.download = match progress.state {
            DownloadState::Cancelled => DownloadPhase::Failed {
                message: "Download cancelled. Nothing was added.".to_string(),
                retry: true,
            },
            DownloadState::Failed => DownloadPhase::Failed {
                message: progress
                    .errors
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "The download stopped.".to_string()),
                retry: true,
            },
            // `Running`, `Done` and `Partial` returned above. A variant this build does not
            // know cannot be painted honestly, so the screen stops polling and says so.
            other => DownloadPhase::Failed {
                message: format!(
                    "The download ended in a state this build does not know ({other:?})."
                ),
                retry: false,
            },
        };
        true
    }

    /// Fold a failed call in.
    pub fn apply_error(&mut self, error: &StoreApiError) {
        self.download = DownloadPhase::Failed {
            message: blocked_message(error),
            retry: blocked_for(error).retryable(),
        };
    }

    /// The release is on this device already (this session downloaded it).
    pub fn is_downloaded(&self) -> bool {
        self.downloaded.is_some()
    }

    /// The rows, in draw order.
    pub fn actions(&self) -> Vec<ReleaseAction> {
        match &self.phase {
            ReleasePhase::Loading => vec![ReleaseAction::Back],
            ReleasePhase::Blocked { blocked, .. } => {
                let mut rows = Vec::new();
                if blocked.retryable() {
                    rows.push(ReleaseAction::Retry);
                }
                rows.push(ReleaseAction::Back);
                rows
            }
            ReleasePhase::Ready => match &self.download {
                DownloadPhase::Preparing => vec![ReleaseAction::Back],
                DownloadPhase::Confirm { .. } => {
                    vec![ReleaseAction::Download, ReleaseAction::Back]
                }
                DownloadPhase::Running { progress, .. } => {
                    if progress.finished() {
                        vec![ReleaseAction::Back]
                    } else {
                        vec![ReleaseAction::CancelDownload]
                    }
                }
                DownloadPhase::Idle => {
                    if self.is_downloaded() {
                        vec![
                            ReleaseAction::OpenInLibrary,
                            ReleaseAction::Remove,
                            ReleaseAction::Back,
                        ]
                    } else {
                        vec![ReleaseAction::Download, ReleaseAction::Back]
                    }
                }
                DownloadPhase::Done { .. } => vec![
                    ReleaseAction::OpenInLibrary,
                    ReleaseAction::Remove,
                    ReleaseAction::Back,
                ],
                DownloadPhase::Failed { retry, .. } => {
                    let mut rows = Vec::new();
                    if *retry {
                        rows.push(ReleaseAction::Retry);
                    }
                    rows.push(ReleaseAction::Back);
                    rows
                }
            },
        }
    }

    /// True when the release call has been outstanding for too long.
    pub fn timed_out(&self, now_ms: u64) -> bool {
        matches!(self.phase, ReleasePhase::Loading)
            || (matches!(self.download, DownloadPhase::Preparing)
                && now_ms.saturating_sub(self.requested_at_ms) >= STORE_TIMEOUT_MS)
    }
}

// ---------------------------------------------------------------------------
// Remove from Plippa (BCS_10's preview / confirm)
// ---------------------------------------------------------------------------

/// What the remove screen knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeletePhase {
    /// Asking for the preview.
    Loading,
    /// The preview arrived: what would go, and whether it can go.
    Ready(Box<DeletePreviewInfo>),
    /// The preview itself was refused.
    Refused {
        blocked: StoreBlocked,
        message: String,
        preview: Option<Box<DeletePreviewInfo>>,
    },
    /// The delete is running.
    Deleting,
    /// The files are gone.
    Done(DeleteDone),
    /// The delete failed (playing, blocked, last file, offline…).
    Failed {
        blocked: StoreBlocked,
        message: String,
    },
}

/// One row of the remove screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteAction {
    /// Remove the files.
    Remove,
    /// Ask again (the preview, or the delete).
    Retry,
    Back,
}

/// The Remove from Plippa screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteView {
    /// `encode_playable_id`-encoded id the server deletes.
    pub playable_id: u64,
    /// The store release this item came from, when known (cleared on success).
    pub release_id: Option<String>,
    /// Title shown while the preview loads.
    pub title: String,
    pub phase: DeletePhase,
    /// Loop time of the call in flight.
    pub requested_at_ms: u64,
}

impl DeleteView {
    /// A view waiting for the preview of `playable_id`.
    pub fn loading(
        playable_id: u64,
        title: impl Into<String>,
        release_id: Option<String>,
        now_ms: u64,
    ) -> Self {
        Self {
            playable_id,
            release_id,
            title: title.into(),
            phase: DeletePhase::Loading,
            requested_at_ms: now_ms,
        }
    }

    /// Fold the preview in. A refused preview keeps whatever the server did send.
    pub fn apply_preview(&mut self, result: Result<DeletePreviewInfo, StoreApiError>) {
        self.phase = match result {
            Ok(preview) => {
                if preview.blocked() {
                    // Playback is the one blocker that clears itself: the row stays Retry so
                    // the person can stop playback and ask again.
                    let blocked = if preview.playing {
                        StoreBlocked::Playing
                    } else {
                        StoreBlocked::Blocked
                    };
                    DeletePhase::Refused {
                        blocked,
                        message: preview.blockers.join(". "),
                        preview: Some(Box::new(preview)),
                    }
                } else {
                    DeletePhase::Ready(Box::new(preview))
                }
            }
            Err(error) => {
                let blocked = blocked_for(&error);
                DeletePhase::Refused {
                    blocked,
                    message: blocked_message(&error),
                    preview: None,
                }
            }
        };
    }

    /// The delete is on its way.
    pub fn begin_delete(&mut self, now_ms: u64) {
        self.phase = DeletePhase::Deleting;
        self.requested_at_ms = now_ms;
    }

    /// Fold the delete in.
    pub fn apply_done(&mut self, result: Result<DeleteDone, StoreApiError>) {
        self.phase = match result {
            Ok(done) => DeletePhase::Done(done),
            Err(error) => DeletePhase::Failed {
                blocked: blocked_for(&error),
                message: blocked_message(&error),
            },
        };
    }

    /// The preview, wherever it is held.
    pub fn preview(&self) -> Option<&DeletePreviewInfo> {
        match &self.phase {
            DeletePhase::Ready(preview) => Some(preview),
            DeletePhase::Refused { preview, .. } => preview.as_deref(),
            _ => None,
        }
    }

    /// The files are gone: the caller clears the release's "downloaded" mark.
    pub fn succeeded(&self) -> bool {
        matches!(self.phase, DeletePhase::Done(_))
    }

    /// The rows, in draw order.
    pub fn actions(&self) -> Vec<DeleteAction> {
        match &self.phase {
            DeletePhase::Loading | DeletePhase::Deleting => vec![DeleteAction::Back],
            DeletePhase::Ready(preview) => {
                let mut rows = Vec::new();
                if !preview.blocked() {
                    rows.push(DeleteAction::Remove);
                }
                rows.push(DeleteAction::Back);
                rows
            }
            DeletePhase::Refused { blocked, .. } | DeletePhase::Failed { blocked, .. } => {
                let mut rows = Vec::new();
                // A playing item becomes removable once playback stops.
                if blocked.retryable() || *blocked == StoreBlocked::Playing {
                    rows.push(DeleteAction::Retry);
                }
                rows.push(DeleteAction::Back);
                rows
            }
            DeletePhase::Done(_) => vec![DeleteAction::Back],
        }
    }

    /// True when the preview has been outstanding for too long.
    pub fn timed_out(&self, now_ms: u64) -> bool {
        matches!(self.phase, DeletePhase::Loading | DeletePhase::Deleting)
            && now_ms.saturating_sub(self.requested_at_ms) >= STORE_TIMEOUT_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_api::{ReleaseTrackInfo, StoreInfo};

    fn error(status: u16, code: &str, message: &str) -> StoreApiError {
        StoreApiError::Refused {
            status,
            code: Some(code.to_string()),
            message: message.to_string(),
        }
    }

    fn release_row(id: &str) -> ReleaseRow {
        ReleaseRow {
            id: id.to_string(),
            title: format!("Release {id}"),
            artist: "Joni Mitchell".into(),
            year: Some(1971),
            track_count: Some(10),
            duration_s: Some(2178),
        }
    }

    fn page(offset: u32, ids: &[&str], total: u32) -> CollectionPageInfo {
        CollectionPageInfo {
            total,
            offset,
            stale: false,
            synced_at: None,
            releases: ids.iter().map(|id| release_row(id)).collect(),
        }
    }

    #[test]
    fn every_failure_has_its_own_message() {
        // The five the work package names, each distinct on screen.
        let cases = [
            (
                StoreApiError::Offline("refused".into()),
                StoreBlocked::ServiceDown,
            ),
            (
                error(401, "bad_credentials", "expired"),
                StoreBlocked::Credentials,
            ),
            (error(429, "busy", "slow down"), StoreBlocked::Busy),
            (
                error(409, "playing", "this item is playing"),
                StoreBlocked::Playing,
            ),
            (
                error(409, "confirm_required", "send force"),
                StoreBlocked::ConfirmRequired,
            ),
            (
                error(409, "blocked", "a playlist owns it"),
                StoreBlocked::Blocked,
            ),
            (
                error(400, "last_file", "the only file"),
                StoreBlocked::LastFile,
            ),
        ];
        let mut titles: Vec<&str> = Vec::new();
        for (error, expected) in cases {
            assert_eq!(blocked_for(&error), expected);
            titles.push(expected.title());
        }
        titles.sort_unstable();
        titles.dedup();
        assert_eq!(titles.len(), 7, "each reason reads differently");
        // A 403 is a refused sign-in too, and 409 without a known code is a plain refusal.
        assert_eq!(
            blocked_for(&error(403, "forbidden", "")),
            StoreBlocked::Credentials
        );
        assert_eq!(blocked_for(&error(409, "other", "")), StoreBlocked::Refused);
        // Retry is offered where it could work, and never for a refused sign-in.
        assert!(StoreBlocked::ServiceDown.retryable());
        assert!(StoreBlocked::Busy.retryable());
        assert!(!StoreBlocked::Credentials.retryable());
        assert!(!StoreBlocked::Playing.retryable());
        assert!(!StoreBlocked::Blocked.retryable());
        // The server's sentence is carried, never a raw code.
        let text = blocked_message(&error(429, "busy", "the store asked to wait"));
        assert!(text.contains("the store asked to wait"), "{text}");
        assert!(!text.contains("429"), "{text}");
    }

    #[test]
    fn stores_screen_reads_the_connection_state() {
        let mut view = StoresView::loading(0);
        assert!(view.timed_out(STORE_TIMEOUT_MS));
        assert_eq!(
            view.actions(),
            vec![
                StoresAction::Bandcamp,
                StoresAction::Connect,
                StoresAction::Refresh,
                StoresAction::Back
            ]
        );

        let info = StoresInfo {
            secrets_available: true,
            secrets_reason: None,
            stores: vec![StoreInfo {
                id: "bandcamp".into(),
                name: "Bandcamp".into(),
                connected: true,
                rejected: false,
                cached_releases: 42,
                cache_stale: false,
            }],
        };
        view.apply(Ok(info));
        assert!(view.connected());
        assert_eq!(view.status_text(), "42 releases");

        // A refused sign-in is not a connection.
        let refused = StoresInfo {
            stores: vec![StoreInfo {
                id: "bandcamp".into(),
                name: "Bandcamp".into(),
                connected: false,
                rejected: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        view.apply(Ok(refused));
        assert!(!view.connected());
        assert!(view.rejected());
        assert_eq!(view.status_text(), "Sign-in refused");

        // The server being down is its own row set with a retry.
        view.apply(Err(StoreApiError::Offline("refused".into())));
        assert_eq!(
            view.actions(),
            vec![StoresAction::Retry, StoresAction::Back]
        );

        // No secrets folder: the feature is unavailable, and why.
        view.apply(Ok(StoresInfo {
            secrets_available: false,
            secrets_reason: Some("cannot write /var/lib/plippa/secrets".into()),
            ..Default::default()
        }));
        assert_eq!(view.status_text(), "No secrets folder");
        assert!(view
            .unavailable_reason()
            .is_some_and(|why| why.contains("secrets")));
    }

    #[test]
    fn connect_screen_only_scans_a_recommended_address() {
        let mut view = ConnectView::looking(0);
        assert!(view.qr_url().is_none());
        assert_eq!(view.actions(), vec![ConnectAction::Back]);
        assert!(view.timed_out(STORE_TIMEOUT_MS));

        view.apply(Ok(PortalAddress {
            recommended: Some("http://192.168.1.42:8787".into()),
            candidates: vec!["http://192.168.1.42:8787".into()],
            port: 8787,
            note: None,
            portal_off: false,
        }));
        assert_eq!(view.qr_url(), Some("http://192.168.1.42:8787"));
        assert_eq!(view.address_text().as_deref(), Some("192.168.1.42:8787"));
        assert_eq!(view.note(), None);
        // SECFIX_03: an `http://` portal gets the clear-text warning; the `https://` one does not.
        assert_eq!(view.clear_text_warning(), Some(CONNECT_CLEAR_TEXT_WARNING));
        view.apply(Ok(PortalAddress {
            recommended: Some("https://192.168.1.42:8787".into()),
            candidates: vec!["https://192.168.1.42:8787".into()],
            port: 8787,
            note: None,
            portal_off: false,
        }));
        assert_eq!(view.clear_text_warning(), None);
        assert_eq!(
            view.actions(),
            vec![ConnectAction::Retry, ConnectAction::Back]
        );

        // No reachable address: no QR code, and the server's reason is the screen.
        view.apply(Ok(PortalAddress {
            recommended: None,
            candidates: vec![],
            port: 8787,
            note: Some("connect this Plippa to Wi-Fi".into()),
            portal_off: false,
        }));
        assert!(view.qr_url().is_none());
        assert!(view.note().is_some_and(|n| n.contains("Wi-Fi")));

        // The service itself is down: say so instead of showing a QR for nothing.
        view.apply(Err(StoreApiError::Offline("connection refused".into())));
        assert!(view.qr_url().is_none());
        let (blocked, message) = view.blocked().expect("a blocked connect screen");
        assert_eq!(blocked, StoreBlocked::ServiceDown);
        assert!(message.contains("not running"), "{message}");
    }

    #[test]
    fn paging_appends_pages_and_stops_on_the_last_one() {
        let mut view = ReleasesView::loading(0);
        view.apply_first(Ok(page(0, &["1", "2"], 3)));
        assert_eq!(view.releases.len(), 2);
        assert_eq!(view.next_offset, Some(2));
        assert_eq!(
            view.rows(),
            vec![
                ReleasesRow::Release(0),
                ReleasesRow::Release(1),
                ReleasesRow::More,
                ReleasesRow::Search,
                ReleasesRow::Back
            ]
        );

        // A page for another offset is not this list's.
        assert!(!view.apply_more(7, Ok(page(7, &["9"], 9))));
        assert_eq!(view.releases.len(), 2);

        // The last page closes the list and drops the More row.
        assert!(view.apply_more(2, Ok(page(2, &["3"], 3))));
        assert_eq!(view.releases.len(), 3);
        assert_eq!(view.next_offset, None);
        assert!(!view.rows().contains(&ReleasesRow::More));
        assert!(!view.is_empty());

        // A release that arrived twice is not shown twice.
        let mut view = ReleasesView::loading(0);
        view.apply_first(Ok(page(0, &["1", "2"], 3)));
        view.apply_more(2, Ok(page(2, &["2"], 3)));
        assert_eq!(view.releases.len(), 2);

        // An empty collection is ready and empty, with no paging row.
        let mut view = ReleasesView::loading(0);
        view.apply_first(Ok(page(0, &[], 0)));
        assert!(view.is_empty());
        assert_eq!(view.rows(), vec![ReleasesRow::Search, ReleasesRow::Back]);

        // A single release is the whole list.
        let mut view = ReleasesView::loading(0);
        view.apply_first(Ok(page(0, &["only"], 1)));
        assert_eq!(
            view.rows(),
            vec![
                ReleasesRow::Release(0),
                ReleasesRow::Search,
                ReleasesRow::Back
            ]
        );
        assert!(!view.is_empty());
    }

    #[test]
    fn a_search_replaces_the_rows_and_is_typed_on_the_keyboard() {
        let mut view = ReleasesView::loading(0);
        view.apply_first(Ok(page(0, &["1", "2"], 9)));
        assert!(view.rows().contains(&ReleasesRow::Search));

        // The keyboard owns the screen while typing: no list row can be pressed under it.
        view.begin_search();
        assert!(view.searching);
        assert!(view.rows().is_empty());

        // Back leaves the keyboard and keeps the rows that were already there.
        view.cancel_search();
        assert!(!view.searching);
        assert_eq!(view.releases.len(), 2);

        // Submitting a query drops the old rows: a result must never be appended to a list
        // fetched for something else.
        view.begin_search();
        view.begin_query("blue", 40);
        assert!(!view.searching);
        assert_eq!(view.query, "blue");
        assert!(view.releases.is_empty());
        assert_eq!(view.total, 0);
        assert_eq!(view.next_offset, None);
        assert_eq!(view.phase, ReleasesPhase::Loading);
        assert_eq!(view.requested_at_ms, 40);
    }

    #[test]
    fn a_failed_later_page_keeps_the_rows_already_loaded() {
        let mut view = ReleasesView::loading(0);
        view.apply_first(Ok(page(0, &["1", "2"], 5)));
        assert!(view.apply_more(2, Err(StoreApiError::Offline("dropped".into()))));
        assert_eq!(view.releases.len(), 2, "the loaded rows stay");
        assert!(matches!(view.phase, ReleasesPhase::PageFailed { .. }));
        // The retry row is next to the rows, and the failure did not eat them.
        assert_eq!(
            view.rows(),
            vec![
                ReleasesRow::Release(0),
                ReleasesRow::Release(1),
                ReleasesRow::Retry,
                ReleasesRow::Back
            ]
        );

        // The first page failing has nothing to keep.
        let mut view = ReleasesView::loading(0);
        view.apply_first(Err(StoreApiError::Offline("refused".into())));
        assert!(view.releases.is_empty());
        assert_eq!(view.rows(), vec![ReleasesRow::Retry, ReleasesRow::Back]);

        // A refused sign-in cannot be retried into working.
        let mut view = ReleasesView::loading(0);
        view.apply_first(Err(error(401, "bad_credentials", "expired")));
        assert_eq!(view.rows(), vec![ReleasesRow::Back]);
    }

    fn release_info() -> ReleaseInfo {
        ReleaseInfo {
            release: release_row("1234"),
            tracks: vec![
                ReleaseTrackInfo {
                    title: "All I Want".into(),
                    track: Some(1),
                    duration_s: Some(200),
                    declared_bytes: Some(1_000),
                    suffix: Some("mp3".into()),
                },
                ReleaseTrackInfo {
                    title: "My Old Man".into(),
                    track: Some(2),
                    duration_s: Some(200),
                    declared_bytes: Some(2_000),
                    suffix: Some("mp3".into()),
                },
            ],
            lossy: true,
            quality_note: "Bandcamp sends a compressed stream, not the purchased FLAC.".into(),
            declared_bytes: Some(3_000),
            cached: true,
        }
    }

    #[test]
    fn release_screen_walks_download_through_consent_and_progress() {
        let mut view = ReleaseView::loading("1234", 0);
        assert_eq!(view.actions(), vec![ReleaseAction::Back]);
        view.apply_info(Ok(release_info()));
        assert_eq!(
            view.actions(),
            vec![ReleaseAction::Download, ReleaseAction::Back]
        );

        view.begin_prepare(10);
        assert_eq!(view.actions(), vec![ReleaseAction::Back], "no double start");

        view.apply_consent(Ok(DownloadConsent {
            consent_id: "c-1".into(),
            tracks: 2,
            estimated_bytes: Some(3_000),
            note: "Bandcamp sends a compressed stream, not the purchased FLAC.".into(),
        }));
        assert_eq!(
            view.actions(),
            vec![ReleaseAction::Download, ReleaseAction::Back],
            "Download is the consent"
        );

        // The session answer moves the screen to progress and the poll schedule.
        assert!(view.apply_session(
            Ok(DownloadSession {
                session: "s-1".into()
            }),
            20
        ));
        assert_eq!(view.poll_at_ms, 20 + crate::store_tap::DOWNLOAD_POLL_MS);
        assert_eq!(view.actions(), vec![ReleaseAction::CancelDownload]);

        // Progress for another session is ignored.
        let mut running = DownloadProgress::queued();
        running.tracks_total = 2;
        assert!(view.apply_progress("s-1", &running));
        assert!(!view.apply_progress("other", &running));

        // Done with files placed: a check mark, open in library, and Remove (not a second
        // download).
        let mut done = DownloadProgress::queued();
        done.state = DownloadState::Done;
        done.placed_files = 2;
        done.noudata = Some("Blue/blue.noudata".into());
        assert!(view.apply_progress("s-1", &done));
        assert!(view.is_downloaded());
        assert_eq!(
            view.downloaded.as_ref().and_then(|d| d.noudata.clone()),
            Some("Blue/blue.noudata".to_string())
        );
        assert_eq!(
            view.actions(),
            vec![
                ReleaseAction::OpenInLibrary,
                ReleaseAction::Remove,
                ReleaseAction::Back
            ],
            "a duplicate download is never offered"
        );

        // A finished download that placed nothing is a failure, not a success.
        let mut view = ReleaseView::loading("1", 0);
        view.apply_info(Ok(release_info()));
        view.begin_prepare(0);
        view.apply_consent(Ok(DownloadConsent {
            consent_id: "c-2".into(),
            tracks: 2,
            estimated_bytes: Some(3_000),
            note: "Bandcamp sends a compressed stream, not the purchased FLAC.".into(),
        }));
        view.apply_session(
            Ok(DownloadSession {
                session: "s-2".into(),
            }),
            0,
        );
        let mut empty = DownloadProgress::queued();
        empty.state = DownloadState::Done;
        view.apply_progress("s-2", &empty);
        assert!(!view.is_downloaded());
        assert!(matches!(view.download, DownloadPhase::Failed { .. }));
        assert_eq!(
            view.actions(),
            vec![ReleaseAction::Retry, ReleaseAction::Back]
        );

        // Every failure of the release call itself is a blocked screen with its reason.
        let mut view = ReleaseView::loading("1", 0);
        view.apply_info(Err(error(429, "busy", "slow down")));
        assert!(matches!(
            view.phase,
            ReleasePhase::Blocked {
                blocked: StoreBlocked::Busy,
                ..
            }
        ));
        assert_eq!(
            view.actions(),
            vec![ReleaseAction::Retry, ReleaseAction::Back]
        );
    }

    fn preview(blockers: Vec<&str>, playing: bool) -> DeletePreviewInfo {
        DeletePreviewInfo {
            title: "Blue".into(),
            files: 10,
            total_bytes: 200 * 1024 * 1024,
            blockers: blockers.into_iter().map(str::to_string).collect(),
            warnings: Vec::new(),
            playing,
            playlists: Vec::new(),
            noudata: Some("Blue/blue.noudata".into()),
        }
    }

    #[test]
    fn remove_screen_confirms_and_reports_every_refusal() {
        let mut view = DeleteView::loading(7, "Blue", Some("1234".into()), 0);
        assert_eq!(view.actions(), vec![DeleteAction::Back]);
        assert!(view.timed_out(STORE_TIMEOUT_MS));

        view.apply_preview(Ok(preview(vec![], false)));
        assert_eq!(
            view.actions(),
            vec![DeleteAction::Remove, DeleteAction::Back]
        );
        assert_eq!(view.preview().map(|p| p.files), Some(10));

        // The confirmed delete.
        view.apply_done(Ok(DeleteDone {
            deleted: 10,
            freed_bytes: 200 * 1024 * 1024,
            partial: false,
        }));
        assert!(view.succeeded());
        assert_eq!(view.actions(), vec![DeleteAction::Back]);

        // A playing item: refused with a reason, and Retry once playback stops.
        let mut view = DeleteView::loading(7, "Blue", None, 0);
        view.apply_preview(Ok(preview(vec!["this item is playing"], true)));
        assert!(matches!(view.phase, DeletePhase::Refused { .. }));
        assert_eq!(
            view.actions(),
            vec![DeleteAction::Retry, DeleteAction::Back],
            "stop playback, then retry"
        );

        // The delete call itself can still be refused: 409 playing, 409 blocked, 400 last file.
        for (error, expected) in [
            (
                error(409, "playing", "this item is playing"),
                StoreBlocked::Playing,
            ),
            (
                error(409, "blocked", "a playlist owns it"),
                StoreBlocked::Blocked,
            ),
            (
                error(400, "last_file", "the only file"),
                StoreBlocked::LastFile,
            ),
            (
                StoreApiError::Offline("refused".into()),
                StoreBlocked::ServiceDown,
            ),
        ] {
            let mut view = DeleteView::loading(7, "Blue", None, 0);
            view.apply_preview(Ok(preview(vec![], false)));
            view.apply_done(Err(error));
            match &view.phase {
                DeletePhase::Failed { blocked, message } => {
                    assert_eq!(*blocked, expected);
                    assert!(!message.is_empty());
                }
                other => panic!("expected Failed, got {other:?}"),
            }
            assert!(!view.succeeded());
        }

        // A refused preview with no body is still a sentence, and offers Retry when it can.
        let mut view = DeleteView::loading(7, "Blue", None, 0);
        view.apply_preview(Err(error(409, "confirm_required", "send force")));
        assert_eq!(
            view.actions(),
            vec![DeleteAction::Retry, DeleteAction::Back]
        );
        let mut view = DeleteView::loading(7, "Blue", None, 0);
        view.apply_preview(Err(error(409, "blocked", "a playlist owns it")));
        assert_eq!(view.actions(), vec![DeleteAction::Back]);
    }

    // ---- BCS_11: resync from the Stores screen ----------------------------------------------

    fn connected_view() -> StoresView {
        let mut view = StoresView::loading(0);
        view.apply(Ok(StoresInfo {
            secrets_available: true,
            secrets_reason: None,
            stores: vec![StoreInfo {
                id: "bandcamp".into(),
                name: "Bandcamp".into(),
                connected: true,
                rejected: false,
                cached_releases: 42,
                cache_stale: false,
            }],
        }));
        view
    }

    fn sync(state: SyncPhase, pages: u32, releases: u32) -> SyncStatus {
        SyncStatus {
            state,
            pages,
            releases,
            ..SyncStatus::default()
        }
    }

    #[test]
    fn the_sync_row_is_offered_only_when_connected_and_is_stepped_over_while_it_runs() {
        // Not connected (or still loading): no Sync row.
        assert!(!StoresView::loading(0).actions().contains(&StoresAction::Sync));
        let mut view = connected_view();
        assert_eq!(
            view.actions(),
            vec![
                StoresAction::Bandcamp,
                StoresAction::Sync,
                StoresAction::Connect,
                StoresAction::Refresh,
                StoresAction::Back
            ]
        );
        assert!(view.enabled(StoresAction::Sync));
        assert_eq!(view.sync_value(), None);

        view.sync_requested(1_000);
        assert!(view.sync_polling);
        // Running: the row stays, shows `Syncing… pages/releases`, and cannot be chosen.
        assert_eq!(view.apply_sync(Ok(sync(SyncPhase::Running, 3, 120)), 2_000), SyncNote::None);
        assert!(view.syncing());
        assert_eq!(view.sync_value().as_deref(), Some("Syncing… 3/120"));
        assert!(view.actions().contains(&StoresAction::Sync));
        assert!(!view.enabled(StoresAction::Sync));
        assert!(view.enabled(StoresAction::Bandcamp));
        // One poll a second.
        assert!(!view.sync_poll_due(2_999));
        assert!(view.sync_poll_due(3_000));
    }

    #[test]
    fn a_finished_sync_is_announced_once_and_stops_the_poll() {
        let mut view = connected_view();
        view.sync_requested(0);
        view.apply_sync(Ok(sync(SyncPhase::Running, 1, 10)), 1_000);
        assert_eq!(
            view.apply_sync(Ok(sync(SyncPhase::Done, 4, 130)), 2_000),
            SyncNote::Finished { releases: 130 }
        );
        assert!(!view.sync_polling, "a terminal state ends the poll");
        assert!(view.enabled(StoresAction::Sync));
        assert_eq!(view.sync_failure(), None);
        // A `done` the screen was not waiting for (an older run) is not news.
        assert_eq!(view.apply_sync(Ok(sync(SyncPhase::Done, 4, 130)), 3_000), SyncNote::None);

        // Entering the screen while a sync already runs picks it up from the first answer.
        let mut view = connected_view();
        view.apply_sync(Ok(sync(SyncPhase::Running, 2, 40)), 0);
        assert!(view.sync_polling && view.sync_watching);
    }

    #[test]
    fn a_failed_sync_shows_the_servers_words_and_can_be_retried() {
        let mut view = connected_view();
        view.sync_requested(0);
        let mut failed = sync(SyncPhase::Failed, 2, 0);
        failed.error = Some("Bandcamp is not answering.".into());
        assert_eq!(view.apply_sync(Ok(failed), 1_000), SyncNote::None);
        assert!(!view.sync_polling);
        assert_eq!(view.sync_failure().as_deref(), Some("Bandcamp is not answering."));
        assert!(view.enabled(StoresAction::Sync), "a retry is a normal action");

        // A call that never reached the server is also stated, and also retryable.
        let mut view = connected_view();
        view.sync_requested(0);
        view.apply_sync(Err(StoreApiError::Offline("refused".into())), 1_000);
        assert!(!view.sync_polling);
        assert!(view.sync_failure().expect("stated").contains("refused"));
        assert!(view.enabled(StoresAction::Sync));

        // `busy` means somebody else's sync is running: followed, not reported.
        let mut view = connected_view();
        view.sync_requested(0);
        let busy = StoreApiError::Refused { status: 429, code: Some("busy".into()), message: "wait".into() };
        view.apply_sync(Err(busy), 1_000);
        assert!(view.sync_polling);
        assert_eq!(view.sync_failure(), None);
    }

    #[test]
    fn last_synced_reads_as_an_age() {
        assert_eq!(synced_text(None, 1_000), "Not synced yet");
        assert_eq!(synced_text(Some(1_000), 1_030), "Last synced just now");
        assert_eq!(synced_text(Some(1_000), 1_000 + 5 * 60), "Last synced 5 min ago");
        assert_eq!(synced_text(Some(1_000), 1_000 + 3 * 3_600), "Last synced 3 h ago");
        assert_eq!(synced_text(Some(1_000), 1_000 + 2 * 86_400), "Last synced 2 d ago");
        // A clock behind the cache never underflows.
        assert_eq!(synced_text(Some(5_000), 1_000), "Last synced just now");
    }
}
