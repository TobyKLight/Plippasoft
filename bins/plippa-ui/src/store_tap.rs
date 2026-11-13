//! State machine for the store tap screen (BCS_07).
//!
//! Five decision states, plus the download they can lead to:
//!
//! ```text
//! tap → Lookup
//!         ├─ owned_downloadable → Owned → (Download) → Preparing → Confirm → Downloading → Complete
//!         ├─ not_owned          → NotOwned (Buy on Bandcamp + QR)
//!         ├─ ambiguous (1)      → LowConfidence (is this the one?)
//!         ├─ ambiguous (2..3)   → Ambiguous (pick one)
//!         └─ blocked            → Blocked (no store / offline / credentials / consent)
//!               └─ no_store_cache → Blocked (Resync now) → Syncing → (done) → Lookup again
//! ```
//!
//! The view owns no I/O: [`crate::app::UiApp`] turns [`StoreTapAction`]s into
//! [`crate::store_worker::StoreRequest`]s and feeds the answers back in as events. Keeping the
//! decisions here (and not in the painter) means every state is a plain value a test can build.

use std::sync::Arc;

use crate::art::ArtImage;
use crate::store_api::{
    DownloadConsent, DownloadProgress, DownloadState, StoreApiError, SyncPhase, SyncStatus,
    TapAnswer, TapBlocked, TapCandidate, TapState,
};

/// The release a screen may download, resolved from the tap answer.
#[derive(Debug, Clone, PartialEq)]
pub struct TapRelease {
    pub store_release_id: String,
    pub title: String,
    pub artist: String,
    pub track_count: Option<u32>,
}

impl TapRelease {
    fn from_candidate(c: &TapCandidate) -> Self {
        Self {
            store_release_id: c.store_release_id.clone(),
            title: c.title.clone(),
            artist: c.artist.clone(),
            track_count: c.track_count,
        }
    }
}

/// Everything the store tap screen can show.
#[derive(Debug, Clone, PartialEq)]
pub enum StoreTapPhase {
    /// Asking the (local) server; the tag's own title is the only text known yet.
    Lookup,
    /// Owned and downloadable now.
    Owned {
        release: TapRelease,
        message: String,
    },
    /// Not owned: the canonical purchase page and a QR for the buyer's own browser.
    NotOwned {
        title: String,
        artist: String,
        url: String,
        /// The interest was recorded with the server (the person pressed Buy).
        interest_saved: bool,
        /// The link is for a release other than the one the tag named (from a pick).
        from_pick: bool,
        /// The store's id for the matched release, when the answer named one (its cover is
        /// fetched by this id).
        release_id: Option<String>,
    },
    /// Two or three releases could be this one; the person picks.
    Ambiguous {
        candidates: Vec<TapCandidate>,
        /// Cursor inside the candidate list.
        selected: usize,
    },
    /// One candidate only, but not confident enough to act on unasked.
    LowConfidence {
        candidate: TapCandidate,
        url: Option<String>,
    },
    /// The tag names a Noubin page, so the store link lives on that page: say which host
    /// would be contacted and wait for the person to agree.
    LookupConsent {
        /// The host the lookup would contact.
        host: String,
        /// Title from the tag, so the screen can name what is being looked up.
        title: String,
    },
    /// A reason the lookup could not run, in plain words.
    Blocked {
        blocked: TapBlocked,
        message: String,
    },
    /// BCS_11: the collection is being synced from this screen; when it finishes the original
    /// tap is sent once more, with no further press.
    Syncing {
        /// The server's latest status, `None` until the first answer.
        status: Option<SyncStatus>,
    },
    /// BCS_11: the sync that was started from this screen failed. Resync and Retry are both
    /// offered; nothing retries by itself.
    SyncFailed { message: String },
    /// Asking the server what the download would fetch (still nothing downloaded).
    Preparing,
    /// The server priced the download; the person confirms.
    Confirm {
        release: TapRelease,
        consent: DownloadConsent,
    },
    /// The download is running (or has just stopped); polling for progress.
    Downloading {
        session: String,
        progress: DownloadProgress,
    },
    /// Placed in the library: play it or go and look at it.
    Complete {
        files: usize,
        noudata: Option<String>,
        /// Some tracks did not arrive. Never shown as a clean success.
        partial: bool,
    },
    /// The call or the download failed. `retry` says whether Retry is offered.
    Failed {
        message: String,
        retry: bool,
    },
}

/// One thing the person can choose on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreTapAction {
    /// Fetch the priced download (`prepare`, then `start`).
    Download,
    /// Record interest and show how to buy it.
    Buy,
    /// The candidate list row `index`.
    Pick(usize),
    /// "This is the one" on the low-confidence screen.
    ConfirmCandidate,
    /// "Not mine" on the low-confidence screen: show the purchase page instead.
    RejectCandidate,
    /// Play the placed release (after `Complete`).
    Play,
    /// Leave for the library.
    ShowInLibrary,
    /// Ask the server again.
    Retry,
    /// BCS_11: sync the Bandcamp collection from this screen, then repeat the tap.
    Sync,
    /// Agree to read the tag's own release page, then ask again.
    AllowLookup,
    /// Leave the screen.
    Back,
}

/// The screen's whole state for one tap.
#[derive(Debug, Clone, PartialEq)]
pub struct StoreTapView {
    /// The URL the tag declared (the store link the server was asked about).
    pub url: String,
    /// The Noubin key carried by the tag (used to unite the download), when there is one.
    pub noubin_key: Option<String>,
    /// Title text from the tag, shown while looking up.
    pub tag_title: String,
    pub phase: StoreTapPhase,
    /// Cursor for the action rows of the current state.
    pub focus: usize,
    /// Identity of the call this view is waiting for; a late answer for another tap is ignored.
    pub generation: u64,
    /// Loop time the current call was made (drives the timeout).
    pub requested_at_ms: u64,
    /// Loop time the next progress poll is due.
    pub poll_at_ms: u64,
    /// How many times this tap has been retried.
    pub retries: u8,
    /// The release being priced by [`StoreTapPhase::Preparing`], kept so the consent answer
    /// can name it on the confirmation screen.
    pub pending_release: Option<TapRelease>,
    /// The person agreed to read the tag's own release page, so retries may send consent.
    pub lookup_allowed: bool,
    /// BCS_11 debounce: while `false` the screen has no focused action, so a bounced or held
    /// Enter cannot trigger the next phase's default action. An Enter that changes the phase
    /// clears it; the next encoder turn ([`Self::move_focus`]) or a network answer sets it.
    pub armed: bool,
    /// BCS_11: the store release whose cover the screen shows (or is waiting for).
    pub cover_for: Option<String>,
    /// BCS_11: how that cover is doing.
    pub cover: CoverState,
    /// BCS_11: a sync started here was the reason for the next tap, so a finished sync repeats
    /// the tap exactly once.
    pub resync_pending: bool,
}

/// The cover on the confirmation / buy screens. Never waited on: anything but `Ready` draws the
/// existing "no art" placeholder.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CoverState {
    /// No release id is known (or nothing asked yet).
    #[default]
    None,
    /// Requested from the store worker.
    Loading,
    /// Fetched but refused, too large or undecodable.
    Failed,
    Ready(Arc<ArtImage>),
}

/// Most retries a person gets on one tap before the screen stops offering Retry.
pub const MAX_TAP_RETRIES: u8 = 2;
/// How long one store call may go unanswered before the screen says it is offline.
pub const STORE_TIMEOUT_MS: u64 = 12_000;
/// Progress poll interval while a download runs.
pub const DOWNLOAD_POLL_MS: u64 = 700;
/// Poll interval while a sync started from this screen runs.
pub const SYNC_POLL_MS: u64 = 1_000;

impl StoreTapView {
    /// A view waiting on the server for `url`.
    pub fn lookup(url: impl Into<String>, noubin_key: Option<String>, tag_title: String) -> Self {
        Self {
            url: url.into(),
            noubin_key,
            // SEC-12: the title comes from the tag and is drawn, so it is stripped here and
            // nowhere later.
            tag_title: plippa_noubin::sanitise_line(&tag_title, 120),
            phase: StoreTapPhase::Lookup,
            focus: 0,
            generation: 0,
            requested_at_ms: 0,
            poll_at_ms: 0,
            retries: 0,
            pending_release: None,
            lookup_allowed: false,
            armed: true,
            cover_for: None,
            cover: CoverState::None,
            resync_pending: false,
        }
    }

    /// Start a new call: bumps the generation so a stale answer is dropped.
    pub fn begin_call(&mut self, now_ms: u64) {
        self.generation = self.generation.wrapping_add(1);
        self.requested_at_ms = now_ms;
        self.focus = 0;
    }

    /// True when a call has been outstanding for too long.
    pub fn timed_out(&self, now_ms: u64) -> bool {
        matches!(self.phase, StoreTapPhase::Lookup | StoreTapPhase::Preparing)
            && now_ms.saturating_sub(self.requested_at_ms) >= STORE_TIMEOUT_MS
    }

    /// Fold a tap answer into the next phase.
    pub fn apply_answer(&mut self, answer: &TapAnswer) {
        self.focus = 0;
        // A phase that comes from a network answer is the one press the person is waiting to
        // make, so it is armed.
        self.armed = true;
        // The consent step is not a failure: the tag's own page has to be read to learn which
        // store release it refers to. Handled before `blocked` so the screen offers the choice.
        if answer.state == TapState::LookupConsent {
            let host = answer
                .lookup
                .as_ref()
                .map(|l| l.host.clone())
                .unwrap_or_default();
            self.phase = StoreTapPhase::LookupConsent {
                host,
                title: self.tag_title.clone(),
            };
            return;
        }
        if let Some(blocked) = answer.blocked {
            self.phase = StoreTapPhase::Blocked {
                blocked,
                message: sentence_or(&answer.message, blocked_message(blocked)),
            };
            return;
        }
        let release = resolve_release(answer);
        self.phase = match answer.state {
            TapState::OwnedDownloadable => match release {
                Some(release) => StoreTapPhase::Owned {
                    release,
                    message: sentence_or(&answer.message, "You own this. Download it?"),
                },
                // Owned but the store named no release id: nothing can be downloaded, so the
                // screen says so instead of offering a dead Download row.
                None => StoreTapPhase::Blocked {
                    blocked: TapBlocked::NoStore,
                    message: "Your collection has this, but the store did not name the \
                              release. Try again from the portal."
                        .to_string(),
                },
            },
            TapState::NotOwned => {
                let title = release
                    .as_ref()
                    .map(|r| r.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| self.tag_title.clone());
                let artist = release
                    .as_ref()
                    .map(|r| r.artist.clone())
                    .unwrap_or_default();
                match &answer.purchase_url {
                    Some(url) => StoreTapPhase::NotOwned {
                        title,
                        artist,
                        url: url.clone(),
                        interest_saved: false,
                        from_pick: false,
                        release_id: release
                            .as_ref()
                            .map(|r| r.store_release_id.clone())
                            .filter(|id| !id.is_empty()),
                    },
                    None => StoreTapPhase::Blocked {
                        blocked: TapBlocked::NoStore,
                        message: sentence_or(&answer.message, "No purchase page was returned."),
                    },
                }
            }
            TapState::Ambiguous => {
                let candidates: Vec<TapCandidate> =
                    answer.candidates.iter().take(3).cloned().collect();
                if candidates.len() == 1 {
                    match candidates.into_iter().next() {
                        Some(candidate) => StoreTapPhase::LowConfidence {
                            candidate,
                            url: answer.purchase_url.clone(),
                        },
                        None => StoreTapPhase::Failed {
                            message: "The answer named no release to pick.".to_string(),
                            retry: true,
                        },
                    }
                } else if candidates.is_empty() {
                    StoreTapPhase::Failed {
                        message: "The answer named no release to pick.".to_string(),
                        retry: true,
                    }
                } else {
                    StoreTapPhase::Ambiguous {
                        candidates,
                        selected: 0,
                    }
                }
            }
            // Handled above (it is not a failure); this arm keeps the match exhaustive.
            TapState::LookupConsent => StoreTapPhase::LookupConsent {
                host: String::new(),
                title: self.tag_title.clone(),
            },
            TapState::NotFound => {
                if answer.cache_stale {
                    StoreTapPhase::Failed {
                        message: "Your Bandcamp collection is out of date. Refresh it in the \
                                  portal and tap again."
                            .to_string(),
                        retry: true,
                    }
                } else {
                    StoreTapPhase::Blocked {
                        blocked: TapBlocked::NoStore,
                        message: sentence_or(&answer.message, "This release is not in your collection."),
                    }
                }
            }
        };
    }

    /// The actions of the current phase, in draw order (the same order the cursor walks).
    pub fn actions(&self) -> Vec<StoreTapAction> {
        match &self.phase {
            // Transitional phases: the row is drawn so the way out is visible, but nothing is
            // focused (see `focused_action`), so Enter does nothing.
            StoreTapPhase::Lookup
            | StoreTapPhase::Preparing
            | StoreTapPhase::Syncing { .. }
            | StoreTapPhase::Downloading { .. } => vec![StoreTapAction::Back],
            StoreTapPhase::LookupConsent { .. } => {
                vec![StoreTapAction::AllowLookup, StoreTapAction::Back]
            }
            StoreTapPhase::Owned { .. } => {
                vec![StoreTapAction::Download, StoreTapAction::Back]
            }
            StoreTapPhase::NotOwned { .. } => vec![StoreTapAction::Buy, StoreTapAction::Back],
            StoreTapPhase::Ambiguous { candidates, .. } => {
                let mut rows: Vec<StoreTapAction> =
                    (0..candidates.len()).map(StoreTapAction::Pick).collect();
                rows.push(StoreTapAction::Back);
                rows
            }
            StoreTapPhase::LowConfidence { url, .. } => {
                let mut rows = vec![StoreTapAction::ConfirmCandidate];
                if url.is_some() {
                    rows.push(StoreTapAction::RejectCandidate);
                }
                rows.push(StoreTapAction::Back);
                rows
            }
            StoreTapPhase::Blocked { blocked, .. } => {
                let mut rows = Vec::new();
                match blocked {
                    TapBlocked::Offline => rows.push(StoreTapAction::Retry),
                    TapBlocked::NoStoreCache => rows.push(StoreTapAction::Sync),
                    _ => {}
                }
                rows.push(StoreTapAction::Back);
                rows
            }
            StoreTapPhase::SyncFailed { .. } => vec![
                StoreTapAction::Sync,
                StoreTapAction::Retry,
                StoreTapAction::Back,
            ],
            StoreTapPhase::Confirm { .. } => {
                vec![StoreTapAction::Download, StoreTapAction::Back]
            }
            StoreTapPhase::Complete { .. } => vec![
                StoreTapAction::Play,
                StoreTapAction::ShowInLibrary,
                StoreTapAction::Back,
            ],
            StoreTapPhase::Failed { retry, .. } => {
                let mut rows = Vec::new();
                if *retry && self.retries < MAX_TAP_RETRIES {
                    rows.push(StoreTapAction::Retry);
                }
                rows.push(StoreTapAction::Back);
                rows
            }
        }
    }

    /// The action under the cursor.
    ///
    /// `None` when Enter must do nothing: while disarmed (an Enter just changed the phase),
    /// and in the transitional phases whose only action is Back (leaving is Back, never Enter).
    pub fn focused_action(&self) -> Option<StoreTapAction> {
        if !self.armed
            || matches!(
                self.phase,
                StoreTapPhase::Lookup
                    | StoreTapPhase::Preparing
                    | StoreTapPhase::Syncing { .. }
                    | StoreTapPhase::Downloading { .. }
            )
        {
            return None;
        }
        let actions = self.actions();
        if actions.is_empty() {
            return None;
        }
        actions.get(self.focus.min(actions.len() - 1)).copied()
    }

    /// Move the cursor; `delta` follows the raw detent (the caller applies [`menu_step`]).
    pub fn move_focus(&mut self, delta: i8) {
        // Any turn is deliberate: the screen takes presses again.
        self.armed = true;
        let len = self.actions().len();
        if len == 0 {
            self.focus = 0;
            return;
        }
        let len = len as isize;
        let mut idx = self.focus as isize + isize::from(delta);
        while idx < 0 {
            idx += len;
        }
        self.focus = (idx % len) as usize;
    }

    /// Combine the action with the phase's data to make the decision the app executes.
    ///
    /// A decision that will change the phase disarms the screen ([`Self::armed`]); only a turn
    /// or a network answer arms it again. `Back` leaves the screen, so it needs no disarm.
    pub fn resolve(&mut self, action: StoreTapAction) -> StoreTapDecision {
        let decision = self.decide(action);
        if !matches!(decision, StoreTapDecision::None | StoreTapDecision::Back) {
            self.armed = false;
        }
        decision
    }

    fn decide(&self, action: StoreTapAction) -> StoreTapDecision {
        match (&self.phase, action) {
            (StoreTapPhase::Owned { release, .. }, StoreTapAction::Download) => {
                StoreTapDecision::Prepare {
                    release: release.clone(),
                }
            }
            // The consent screen's Download is the consent: start the priced download.
            (StoreTapPhase::Confirm { consent, .. }, StoreTapAction::Download) => {
                StoreTapDecision::StartDownload {
                    consent_id: consent.consent_id.clone(),
                }
            }
            (
                StoreTapPhase::NotOwned {
                    title,
                    artist,
                    url,
                    ..
                },
                StoreTapAction::Buy,
            ) => StoreTapDecision::RecordWant {
                url: url.clone(),
                title: title.clone(),
                artist: artist.clone(),
            },
            (
                StoreTapPhase::Ambiguous { candidates, .. },
                StoreTapAction::Pick(index),
            ) => match candidates.get(index) {
                // A pick is the confirmation the ambiguous band was asking for.
                Some(candidate) => StoreTapDecision::ConfirmPick {
                    candidate: candidate.clone(),
                },
                None => StoreTapDecision::None,
            },
            (StoreTapPhase::LowConfidence { candidate, .. }, StoreTapAction::ConfirmCandidate) => {
                StoreTapDecision::ConfirmPick {
                    candidate: candidate.clone(),
                }
            }
            (
                StoreTapPhase::LowConfidence {
                    candidate, url, ..
                },
                StoreTapAction::RejectCandidate,
            ) => StoreTapDecision::ShowPurchase {
                title: candidate.title.clone(),
                artist: candidate.artist.clone(),
                url: url.clone(),
                release_id: Some(candidate.store_release_id.clone()),
            },
            (StoreTapPhase::Complete { .. }, StoreTapAction::Play) => StoreTapDecision::Play,
            (StoreTapPhase::Complete { .. }, StoreTapAction::ShowInLibrary) => {
                StoreTapDecision::ShowInLibrary
            }
            (StoreTapPhase::Failed { .. }, StoreTapAction::Retry)
            | (StoreTapPhase::SyncFailed { .. }, StoreTapAction::Retry)
            | (StoreTapPhase::Blocked { .. }, StoreTapAction::Retry) => StoreTapDecision::Retry,
            (
                StoreTapPhase::Blocked {
                    blocked: TapBlocked::NoStoreCache,
                    ..
                }
                | StoreTapPhase::SyncFailed { .. },
                StoreTapAction::Sync,
            ) => StoreTapDecision::Sync,
            (StoreTapPhase::LookupConsent { .. }, StoreTapAction::AllowLookup) => {
                StoreTapDecision::AllowLookup
            }
            (_, StoreTapAction::Back) => StoreTapDecision::Back,
            _ => StoreTapDecision::None,
        }
    }
}

/// What the app does for a chosen action.
#[derive(Debug, Clone, PartialEq)]
pub enum StoreTapDecision {
    None,
    /// Ask the server what the download costs.
    Prepare { release: TapRelease },
    /// Start the download the consent screen priced.
    StartDownload { consent_id: String },
    /// Record interest in the canonical page.
    RecordWant {
        url: String,
        title: String,
        artist: String,
    },
    /// The person picked (or confirmed) a candidate: offer the download.
    ConfirmPick { candidate: TapCandidate },
    /// Show the purchase page without a candidate.
    ShowPurchase {
        title: String,
        artist: String,
        url: Option<String>,
        release_id: Option<String>,
    },
    Play,
    ShowInLibrary,
    Retry,
    /// BCS_11: start a collection sync, then repeat the tap.
    Sync,
    /// Ask again, this time agreeing to read the tag's own release page.
    AllowLookup,
    Back,
}

/// What the app does after a sync answer reached the tap screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStep {
    /// The screen is not waiting on a sync: the answer belongs elsewhere.
    Ignored,
    /// Still running, or failed (the screen shows which); nothing to send.
    Waiting,
    /// The sync finished: send the original tap again, exactly once.
    Resend,
}

/// Fold a sync answer into the tap screen. A `done` repeats the tap once (the screen becomes
/// `Lookup` and the app sends it); a failure is its own screen with Resync and Retry, so
/// nothing retries by itself.
pub fn apply_sync(
    view: &mut StoreTapView,
    result: Result<SyncStatus, StoreApiError>,
    now_ms: u64,
) -> SyncStep {
    if !matches!(view.phase, StoreTapPhase::Syncing { .. }) {
        return SyncStep::Ignored;
    }
    let failed = |view: &mut StoreTapView, message: String| {
        view.phase = StoreTapPhase::SyncFailed { message };
        view.resync_pending = false;
        view.focus = 0;
        view.armed = true;
        SyncStep::Waiting
    };
    match result {
        Ok(status) => match status.state {
            SyncPhase::Running => {
                view.phase = StoreTapPhase::Syncing {
                    status: Some(status),
                };
                view.poll_at_ms = now_ms.saturating_add(SYNC_POLL_MS);
                SyncStep::Waiting
            }
            SyncPhase::Done if view.resync_pending => {
                view.resync_pending = false;
                view.begin_call(now_ms);
                view.phase = StoreTapPhase::Lookup;
                view.armed = true;
                SyncStep::Resend
            }
            SyncPhase::Done => SyncStep::Ignored,
            SyncPhase::Failed => {
                let message = status
                    .error
                    .filter(|e| !e.trim().is_empty())
                    .unwrap_or_else(|| "The sync stopped before it finished.".to_string());
                failed(view, message)
            }
            SyncPhase::Idle => failed(view, "The sync stopped before it finished.".to_string()),
        },
        // A sync that was already running (started from the portal or another screen) is
        // followed, not reported as a failure.
        Err(error) if error.status() == Some(429) && error.code() == Some("busy") => {
            view.poll_at_ms = now_ms.saturating_add(SYNC_POLL_MS);
            SyncStep::Waiting
        }
        Err(error) => failed(view, error.message()),
    }
}

impl StoreTapView {
    /// The store release whose cover this phase shows, when the phase names one.
    pub fn cover_release_id(&self) -> Option<&str> {
        match &self.phase {
            StoreTapPhase::Owned { release, .. } | StoreTapPhase::Confirm { release, .. } => {
                Some(release.store_release_id.as_str()).filter(|id| !id.is_empty())
            }
            StoreTapPhase::NotOwned { release_id, .. } => release_id.as_deref(),
            _ => None,
        }
    }

    /// True when the sync status should be asked again.
    pub fn sync_poll_due(&self, now_ms: u64) -> bool {
        matches!(self.phase, StoreTapPhase::Syncing { .. }) && now_ms >= self.poll_at_ms
    }
}

/// Fold a download progress answer into the view.
pub fn apply_progress(
    view: &mut StoreTapView,
    session: &str,
    progress: &DownloadProgress,
) -> bool {
    let StoreTapPhase::Downloading {
        session: current, ..
    } = &mut view.phase
    else {
        return false;
    };
    if current != session {
        return false;
    }
    view.phase = StoreTapPhase::Downloading {
        session: session.to_string(),
        progress: progress.clone(),
    };
    // Any phase this settles into came from a network answer.
    view.armed = true;
    if progress.state == DownloadState::Done || progress.state == DownloadState::Partial {
        // A finished download that added nothing and found nothing is not a success; a
        // replay of a release already in the library is.
        if progress.placed_files == 0 && !progress.already_present {
            view.phase = StoreTapPhase::Failed {
                message: "The download finished but nothing was added to the library."
                    .to_string(),
                retry: true,
            };
        } else {
            view.phase = StoreTapPhase::Complete {
                files: progress.placed_files,
                noudata: progress.noudata.clone(),
                partial: progress.state == DownloadState::Partial,
            };
            view.focus = 0;
        }
    } else if progress.state == DownloadState::Failed {
        let why = progress
            .errors
            .first()
            .cloned()
            .unwrap_or_else(|| "The download stopped.".to_string());
        view.phase = StoreTapPhase::Failed {
            message: why,
            retry: true,
        };
        view.focus = 0;
    } else if progress.state == DownloadState::Cancelled {
        view.phase = StoreTapPhase::Failed {
            message: "Download cancelled. Nothing was added to the library.".to_string(),
            retry: true,
        };
        view.focus = 0;
    }
    true
}

/// Fold a failed call into an honest blocked / failed phase.
pub fn apply_error(view: &mut StoreTapView, error: &StoreApiError, during_download: bool) {
    let why = error.message();
    let (blocked, message) = match error {
        StoreApiError::Offline(_) => (
            TapBlocked::Offline,
            format!("Can't reach the store service. {why}"),
        ),
        // The consent gate is checked before the status code: a server that refuses a lookup
        // *because* the person has not allowed web access says so, whatever code it used.
        StoreApiError::Refused { .. } if why.to_ascii_lowercase().contains("consent") => (
            TapBlocked::ConsentNeeded,
            format!("Web lookups are off. Allow them in the portal. {why}"),
        ),
        StoreApiError::Refused { status, .. } if *status == 401 || *status == 403 => (
            TapBlocked::CredentialsRejected,
            format!("Bandcamp refused the sign-in. Fix it in the portal. {why}"),
        ),
        StoreApiError::Refused { .. } => (
            TapBlocked::NoStore,
            format!("The store service refused this: {why}"),
        ),
        StoreApiError::Malformed(_) => (
            TapBlocked::NoStore,
            format!("The store service sent something unexpected. {why}"),
        ),
    };
    view.phase = if during_download {
        StoreTapPhase::Failed {
            message,
            retry: true,
        }
    } else {
        StoreTapPhase::Blocked { blocked, message }
    };
    view.focus = 0;
    view.armed = true;
}

/// The blocked message per kind, used when the server does not supply one.
pub fn blocked_message(blocked: TapBlocked) -> &'static str {
    match blocked {
        TapBlocked::ConsentNeeded => "Web lookups are off. Allow them in the web portal.",
        TapBlocked::NoStore => "No store is connected. Connect Bandcamp in the web portal.",
        TapBlocked::Offline => "Can't reach the store service. Check the Wi-Fi and try again.",
        TapBlocked::CredentialsRejected => {
            "Bandcamp refused the stored sign-in. Fix it in the web portal."
        }
        TapBlocked::WebUnavailable => "Web lookups are turned off on this Plippa.",
        TapBlocked::NoStoreCache => "This device has not synced your Bandcamp library yet.",
    }
}

/// The best release the answer names: a candidate first, then the owned library item.
fn resolve_release(answer: &TapAnswer) -> Option<TapRelease> {
    answer
        .candidates
        .first()
        .map(TapRelease::from_candidate)
        .or_else(|| {
            answer.release.as_ref().map(|release| TapRelease {
                store_release_id: release.store_release_id.clone(),
                title: release.title.clone(),
                artist: release.artist.clone(),
                track_count: release.track_count,
            })
        })
}

fn sentence_or(message: &str, fallback: &str) -> String {
    if message.trim().is_empty() {
        fallback.to_string()
    } else {
        message.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_api::StoreRelease;

    fn candidate(id: &str, confidence: f32) -> TapCandidate {
        TapCandidate {
            store_release_id: id.to_string(),
            title: format!("Release {id}"),
            artist: "Joni Mitchell".into(),
            track_count: Some(10),
            confidence,
        }
    }

    fn answer(state: TapState) -> TapAnswer {
        TapAnswer {
            state,
            blocked: None,
            candidates: Vec::new(),
            release: None,
            owned_path: None,
            purchase_url: None,
            message: String::new(),
            cache_stale: false,
            requires_confirmation: false,
            confidence: 0.0,
            lookup: None,
        }
    }

    fn view() -> StoreTapView {
        StoreTapView::lookup("https://a.bandcamp.com/album/x", None, "Blue".into())
    }

    #[test]
    fn each_answer_state_maps_to_its_screen() {
        let mut owned = answer(TapState::OwnedDownloadable);
        owned.candidates = vec![candidate("1", 0.95)];
        let mut v = view();
        v.apply_answer(&owned);
        assert!(matches!(v.phase, StoreTapPhase::Owned { .. }));
        assert_eq!(
            v.actions(),
            vec![StoreTapAction::Download, StoreTapAction::Back]
        );

        let mut not_owned = answer(TapState::NotOwned);
        not_owned.purchase_url = Some("https://a.bandcamp.com/album/x".into());
        let mut v = view();
        v.apply_answer(&not_owned);
        assert!(matches!(v.phase, StoreTapPhase::NotOwned { .. }));
        assert_eq!(v.actions()[0], StoreTapAction::Buy);

        // One candidate is a question ("is this the one?"), three is a list.
        let mut low = answer(TapState::Ambiguous);
        low.candidates = vec![candidate("1", 0.62)];
        let mut v = view();
        v.apply_answer(&low);
        assert!(matches!(v.phase, StoreTapPhase::LowConfidence { .. }));

        let mut many = answer(TapState::Ambiguous);
        many.candidates = vec![candidate("1", 0.8), candidate("2", 0.7), candidate("3", 0.6)];
        let mut v = view();
        v.apply_answer(&many);
        match &v.phase {
            StoreTapPhase::Ambiguous { candidates, .. } => assert_eq!(candidates.len(), 3),
            other => panic!("expected Ambiguous, got {other:?}"),
        }

        // Blocked keeps the server's sentence (no codes) and offers Retry only when the
        // failure could pass.
        let mut blocked = answer(TapState::NotFound);
        blocked.blocked = Some(TapBlocked::NoStore);
        let mut v = view();
        v.apply_answer(&blocked);
        match &v.phase {
            StoreTapPhase::Blocked { blocked, message } => {
                assert_eq!(*blocked, TapBlocked::NoStore);
                assert!(message.contains("portal"), "{message:?}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        assert_eq!(v.actions(), vec![StoreTapAction::Back]);
    }

    #[test]
    fn not_found_with_a_stale_cache_asks_for_a_refresh() {
        let mut stale = answer(TapState::NotFound);
        stale.cache_stale = true;
        let mut v = view();
        v.apply_answer(&stale);
        match &v.phase {
            StoreTapPhase::Failed { message, retry } => {
                assert!(message.contains("Refresh"), "{message:?}");
                assert!(*retry);
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        // Owned but with no release id: no dead Download row.
        let mut nameless = answer(TapState::OwnedDownloadable);
        nameless.release = None;
        let mut v = view();
        v.apply_answer(&nameless);
        assert!(matches!(v.phase, StoreTapPhase::Blocked { .. }));

        // Owned with only the library item named still resolves through `release`.
        let mut owned_only = answer(TapState::OwnedDownloadable);
        owned_only.release = Some(StoreRelease {
            store_release_id: "77".into(),
            title: "Blue".into(),
            artist: "Joni Mitchell".into(),
            track_count: None,
        });
        let mut v = view();
        v.apply_answer(&owned_only);
        match &v.phase {
            StoreTapPhase::Owned { release, .. } => {
                assert_eq!(release.store_release_id, "77")
            }
            other => panic!("expected Owned, got {other:?}"),
        }
    }

    #[test]
    fn focus_wraps_both_ways_and_resolves_to_the_picked_action() {
        let mut many = answer(TapState::Ambiguous);
        many.candidates = vec![candidate("1", 0.8), candidate("2", 0.7), candidate("3", 0.6)];
        let mut v = view();
        v.apply_answer(&many);
        // Four rows: three picks and Cancel. `move_focus` takes the already-inverted delta
        // the app passes (`menu_step`), so +1 walks down the list.
        v.move_focus(1);
        assert_eq!(v.focused_action(), Some(StoreTapAction::Pick(1)));
        v.move_focus(1);
        assert_eq!(v.focused_action(), Some(StoreTapAction::Pick(2)));
        v.move_focus(1);
        assert_eq!(v.focused_action(), Some(StoreTapAction::Back));
        // Wraps to the first row.
        v.move_focus(1);
        assert_eq!(v.focused_action(), Some(StoreTapAction::Pick(0)));
        // And backwards past the start lands on Cancel.
        v.move_focus(-1);
        assert_eq!(v.focused_action(), Some(StoreTapAction::Back));

        match v.resolve(StoreTapAction::Pick(2)) {
            StoreTapDecision::ConfirmPick { candidate } => {
                assert_eq!(candidate.store_release_id, "3")
            }
            other => panic!("expected ConfirmPick, got {other:?}"),
        }
        assert_eq!(v.resolve(StoreTapAction::Back), StoreTapDecision::Back);
    }

    #[test]
    fn partial_download_is_never_a_clean_success() {
        let mut v = view();
        v.phase = StoreTapPhase::Downloading {
            session: "s-1".into(),
            progress: DownloadProgress::queued(),
        };
        let mut partial = DownloadProgress::queued();
        partial.state = DownloadState::Partial;
        partial.placed_files = 7;
        partial.noudata = Some("Blue/blue.noudata".into());
        assert!(apply_progress(&mut v, "s-1", &partial));
        match &v.phase {
            StoreTapPhase::Complete {
                files, partial, ..
            } => {
                assert_eq!(*files, 7);
                assert!(*partial, "a partial download says so");
            }
            other => panic!("expected Complete, got {other:?}"),
        }

        // A late answer for another session is ignored.
        let mut v = view();
        v.phase = StoreTapPhase::Downloading {
            session: "s-1".into(),
            progress: DownloadProgress::queued(),
        };
        assert!(!apply_progress(&mut v, "other", &partial));
        assert!(matches!(v.phase, StoreTapPhase::Downloading { .. }));

        // Nothing placed: a failure, not a success screen.
        let mut empty = DownloadProgress::queued();
        empty.state = DownloadState::Done;
        empty.placed_files = 0;
        let mut v = view();
        v.phase = StoreTapPhase::Downloading {
            session: "s-2".into(),
            progress: DownloadProgress::queued(),
        };
        apply_progress(&mut v, "s-2", &empty);
        assert!(matches!(v.phase, StoreTapPhase::Failed { .. }));
    }

    #[test]
    fn errors_become_honest_blocked_states() {
        let mut v = view();
        apply_error(&mut v, &StoreApiError::Offline("refused".into()), false);
        match &v.phase {
            StoreTapPhase::Blocked { blocked, message } => {
                assert_eq!(*blocked, TapBlocked::Offline);
                assert!(message.contains("refused"), "{message:?}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        assert_eq!(v.actions()[0], StoreTapAction::Retry);

        let mut v = view();
        apply_error(&mut v, &StoreApiError::Refused {
            status: 401,
            code: None,
            message: "expired".into(),
        }, false);
        assert!(matches!(
            v.phase,
            StoreTapPhase::Blocked {
                blocked: TapBlocked::CredentialsRejected,
                ..
            }
        ));

        let mut v = view();
        apply_error(
            &mut v,
            &StoreApiError::Refused {
                status: 403,
                code: None,
                message: "web consent is off".into(),
            },
            false,
        );
        assert!(matches!(
            v.phase,
            StoreTapPhase::Blocked {
                blocked: TapBlocked::ConsentNeeded,
                ..
            }
        ));

        // A download that fails offers Retry on the failure screen, not a blocked one.
        let mut v = view();
        apply_error(&mut v, &StoreApiError::Malformed("no session".into()), true);
        assert!(matches!(
            v.phase,
            StoreTapPhase::Failed { retry: true, .. }
        ));
    }

    #[test]
    fn a_lookup_that_never_answers_times_out() {
        let mut v = view();
        v.begin_call(1_000);
        assert!(!v.timed_out(1_000 + STORE_TIMEOUT_MS - 1));
        assert!(v.timed_out(1_000 + STORE_TIMEOUT_MS));
        // Once answered, the timeout no longer applies.
        v.apply_answer(&answer(TapState::NotOwned));
        assert!(!v.timed_out(1_000 + STORE_TIMEOUT_MS * 10));
    }

    // ---- BCS_11 ---------------------------------------------------------------------------

    fn owned_view() -> StoreTapView {
        let mut owned = answer(TapState::OwnedDownloadable);
        owned.candidates = vec![candidate("1", 0.95)];
        let mut v = view();
        v.apply_answer(&owned);
        v
    }

    #[test]
    fn transitional_phases_expose_only_back_and_enter_does_nothing() {
        let phases = [
            StoreTapPhase::Lookup,
            StoreTapPhase::Preparing,
            StoreTapPhase::Syncing { status: None },
            StoreTapPhase::Downloading {
                session: "s".into(),
                progress: DownloadProgress::queued(),
            },
        ];
        for phase in phases {
            let mut v = view();
            v.phase = phase.clone();
            assert_eq!(v.actions(), vec![StoreTapAction::Back], "{phase:?}");
            assert_eq!(v.focused_action(), None, "{phase:?}: Enter has nothing focused");
            for action in [
                StoreTapAction::Download,
                StoreTapAction::Buy,
                StoreTapAction::Retry,
                StoreTapAction::Sync,
                StoreTapAction::AllowLookup,
                StoreTapAction::Play,
                StoreTapAction::ConfirmCandidate,
                StoreTapAction::Pick(0),
            ] {
                assert_eq!(v.resolve(action), StoreTapDecision::None, "{phase:?} {action:?}");
            }
            // Back still leaves.
            assert_eq!(v.resolve(StoreTapAction::Back), StoreTapDecision::Back);
        }
    }

    #[test]
    fn an_enter_that_changes_the_phase_disarms_until_the_next_turn() {
        // Prepare (Owned -> Download).
        let mut v = owned_view();
        assert_eq!(v.focused_action(), Some(StoreTapAction::Download), "armed by the answer");
        assert!(matches!(
            v.resolve(StoreTapAction::Download),
            StoreTapDecision::Prepare { .. }
        ));
        assert_eq!(v.focused_action(), None, "a second press without a turn is a no-op");
        v.move_focus(1);
        assert_eq!(v.focused_action(), Some(StoreTapAction::Back), "one turn re-arms");

        // StartDownload (Confirm -> Download).
        let mut v = view();
        v.phase = StoreTapPhase::Confirm {
            release: TapRelease {
                store_release_id: "1".into(),
                title: "Blue".into(),
                artist: "Joni".into(),
                track_count: None,
            },
            consent: DownloadConsent {
                consent_id: "c-1".into(),
                tracks: 10,
                estimated_bytes: None,
                note: String::new(),
            },
        };
        assert!(matches!(
            v.resolve(StoreTapAction::Download),
            StoreTapDecision::StartDownload { .. }
        ));
        assert_eq!(v.focused_action(), None);

        // The other phase-changing presses: Buy, Retry, AllowLookup, Sync.
        let mut buy = answer(TapState::NotOwned);
        buy.purchase_url = Some("https://a.bandcamp.com/album/x".into());
        let mut v = view();
        v.apply_answer(&buy);
        assert!(matches!(v.resolve(StoreTapAction::Buy), StoreTapDecision::RecordWant { .. }));
        assert_eq!(v.focused_action(), None);

        let mut v = view();
        v.phase = StoreTapPhase::Failed { message: "x".into(), retry: true };
        assert_eq!(v.resolve(StoreTapAction::Retry), StoreTapDecision::Retry);
        assert_eq!(v.focused_action(), None);

        let mut consent = answer(TapState::LookupConsent);
        consent.lookup = None;
        let mut v = view();
        v.apply_answer(&consent);
        assert_eq!(v.resolve(StoreTapAction::AllowLookup), StoreTapDecision::AllowLookup);
        assert_eq!(v.focused_action(), None);

        // A phase that comes from a network answer is armed again.
        v.apply_answer(&answer(TapState::NotOwned));
        assert!(v.armed);
        let mut owned = answer(TapState::OwnedDownloadable);
        owned.candidates = vec![candidate("1", 0.95)];
        v.apply_answer(&owned);
        assert_eq!(v.focused_action(), Some(StoreTapAction::Download));
    }

    #[test]
    fn no_store_cache_offers_resync_and_not_a_bare_retry() {
        let mut blocked = answer(TapState::NotFound);
        blocked.blocked = Some(TapBlocked::NoStoreCache);
        blocked.message = "This device has not synced your Bandcamp library yet.".into();
        let mut v = view();
        v.apply_answer(&blocked);
        assert!(matches!(
            v.phase,
            StoreTapPhase::Blocked { blocked: TapBlocked::NoStoreCache, .. }
        ));
        assert_eq!(v.actions(), vec![StoreTapAction::Sync, StoreTapAction::Back]);
        assert_eq!(v.resolve(StoreTapAction::Sync), StoreTapDecision::Sync);
        // With no server sentence the built-in one says the same thing.
        assert!(blocked_message(TapBlocked::NoStoreCache).contains("not synced"));
    }

    fn sync_status(state: SyncPhase, pages: u32, releases: u32) -> SyncStatus {
        SyncStatus {
            state,
            pages,
            releases,
            ..SyncStatus::default()
        }
    }

    #[test]
    fn a_finished_sync_repeats_the_tap_once_and_a_failed_one_does_not() {
        let mut v = view();
        v.phase = StoreTapPhase::Blocked {
            blocked: TapBlocked::NoStoreCache,
            message: String::new(),
        };
        v.resync_pending = true;
        v.phase = StoreTapPhase::Syncing { status: None };

        // Running: keep waiting, show the counts, schedule the next poll.
        assert_eq!(
            apply_sync(&mut v, Ok(sync_status(SyncPhase::Running, 3, 120)), 5_000),
            SyncStep::Waiting
        );
        match &v.phase {
            StoreTapPhase::Syncing { status: Some(s) } => assert_eq!((s.pages, s.releases), (3, 120)),
            other => panic!("expected Syncing, got {other:?}"),
        }
        assert!(!v.sync_poll_due(5_000));
        assert!(v.sync_poll_due(5_000 + SYNC_POLL_MS));

        // Done: exactly one resend.
        assert_eq!(
            apply_sync(&mut v, Ok(sync_status(SyncPhase::Done, 4, 130)), 6_000),
            SyncStep::Resend
        );
        assert!(matches!(v.phase, StoreTapPhase::Lookup));
        assert_eq!(
            apply_sync(&mut v, Ok(sync_status(SyncPhase::Done, 4, 130)), 7_000),
            SyncStep::Ignored,
            "a second `done` finds no sync to finish"
        );

        // Failed: its own screen with Resync and Retry; nothing is resent.
        let mut v = view();
        v.resync_pending = true;
        v.phase = StoreTapPhase::Syncing { status: None };
        let mut failed = sync_status(SyncPhase::Failed, 1, 10);
        failed.error = Some("Bandcamp refused the sign-in.".into());
        assert_eq!(apply_sync(&mut v, Ok(failed), 1_000), SyncStep::Waiting);
        match &v.phase {
            StoreTapPhase::SyncFailed { message } => assert!(message.contains("refused")),
            other => panic!("expected SyncFailed, got {other:?}"),
        }
        assert_eq!(
            v.actions(),
            vec![StoreTapAction::Sync, StoreTapAction::Retry, StoreTapAction::Back]
        );
        assert!(!v.resync_pending);

        // A busy answer means a sync is already running: follow it.
        let mut v = view();
        v.resync_pending = true;
        v.phase = StoreTapPhase::Syncing { status: None };
        let busy = StoreApiError::Refused { status: 429, code: Some("busy".into()), message: "wait".into() };
        assert_eq!(apply_sync(&mut v, Err(busy), 2_000), SyncStep::Waiting);
        assert!(matches!(v.phase, StoreTapPhase::Syncing { .. }));
    }

    #[test]
    fn the_cover_belongs_to_the_release_the_phase_names() {
        let v = owned_view();
        assert_eq!(v.cover_release_id(), Some("1"));
        let mut not_owned = answer(TapState::NotOwned);
        not_owned.purchase_url = Some("https://a.bandcamp.com/album/x".into());
        let mut v = view();
        v.apply_answer(&not_owned);
        assert_eq!(v.cover_release_id(), None, "no matched release, no cover to fetch");
        let mut v = view();
        v.phase = StoreTapPhase::NotOwned {
            title: "Blue".into(),
            artist: "Joni".into(),
            url: "https://a.bandcamp.com/album/x".into(),
            interest_saved: false,
            from_pick: true,
            release_id: Some("77".into()),
        };
        assert_eq!(v.cover_release_id(), Some("77"));
        v.phase = StoreTapPhase::Lookup;
        assert_eq!(v.cover_release_id(), None);
    }
}
