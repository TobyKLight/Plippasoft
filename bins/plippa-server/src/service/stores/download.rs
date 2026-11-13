//! Store download queue (BCS_04): consent, a sequential worker, and the unite hand-off.
//!
//! The worker is a *user* of [`crate::web::download`], not a second downloader: every byte
//! goes through [`download_to_temp`], so the temp file under `.plippa/tmp`, the per-file and
//! batch caps, the free-space check before the body is read, the stall timeout, the drop
//! guard that deletes partials, and the `Cancel` flag are the ones NB_09 already proved.
//!
//! Rules specific to a store source:
//!
//! * **The audio URL is a bearer capability.** The provider hands over a signed CDN URL. It
//!   is never logged (`log_url: false`), never put in an API payload, and the download is
//!   checked against the store allowlist on the first URL **and on every redirect hop**
//!   ([`FetchPolicy::for_store`]).
//! * **Honesty per track.** `AudioSource::stream_fallback` says whether the store substituted
//!   its lossy stream for the purchase. Each result carries `original`, `streamSubstituted`,
//!   the lossy/lossless answer and the format actually sniffed from the bytes.
//! * **Prepare writes nothing.** The only I/O a prepare does is the store lookup and a
//!   `statvfs`; no cache entry, temp directory or `.noudata` is created.
//! * **One prepare, one download.** A second prepare while one runs is `429`; a second
//!   download while one runs is `429`; a consent id from an older prepare is `409`.
//!
//! The package's boundary is *the release is in the library and the index knows about it*.
//! That call is [`StoreHub::post_download_unite`] (BCS_05): the NB_07 planner and the NB_09
//! all-or-nothing writer move the staged files into `Music/<artist>/<release>/` and write
//! the `.noudata` last, so a downloaded track is playable immediately.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use plippa_store::{
    AudioPreference, AudioQuality, StoreProvider, StoreRelease, StoreReleaseDetail, StoreTrack,
    StoreTrackRef,
};
use serde::{Deserialize, Serialize};

use crate::web::download::{
    download_to_temp, Budget, Cancel, DownloadError, DownloadLimits, Want,
};
use crate::web::fetch::{Consent, FetchPolicy, Transport};

use super::browse::{quality, QualityNote};
use super::cache::STATE_DIR;
use super::post_unite::{PostDownloadUnite, UniteState};
use super::{log_line, now_secs, CallGuard, StoreApiError, StoreHub, STORE_SLUG};

/// Pause between tracks. The beta is slow and rate-limited, and this is somebody's
/// personal collection, so the default is deliberately unhurried.
pub const DEFAULT_TRACK_PAUSE: Duration = Duration::from_millis(500);
/// Largest prepare body (release ids and track selections only).
pub const MAX_PREPARE_BODY: usize = 64 * 1024;
/// Largest start/cancel body (`consent` plus one id).
pub const MAX_START_BODY: usize = 4 * 1024;
/// Longest run id accepted in a download session URL.
pub const MAX_SESSION_ID_BYTES: usize = 64;
/// The command the portal is told will place the staged files.
/// The unite entry point BCS_04 recorded; BCS_05 moved the implementation to
/// [`crate::service::stores::post_unite::post_download_unite`] and this is the same value,
/// kept so the worker's [`HandedOff`] carries one stable entry point.
pub const UNITE_ENTRY_POINT: &str =
    "service::stores::post_unite::post_download_unite";
/// The sentence that must appear wherever a store download is described. It is a const so
/// no consent sheet, release view or result can quietly drop it.
pub const STREAM_WARNING: &str =
    "Will be downloaded as MP3.";

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// One track the user picked (`{ release: "<id>", track: "<id>" }`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackSelection {
    pub release: String,
    pub track: String,
}

/// `POST /api/stores/<store>/download/prepare`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareDownloadRequest {
    /// Releases to take in full.
    #[serde(default)]
    pub releases: Vec<String>,
    /// Individual tracks from releases that are not taken in full.
    #[serde(default)]
    pub tracks: Vec<TrackSelection>,
    /// A Noubin URL/key to unite the download against (Path A). Validated with the web
    /// hub's own key rules; the consent sheet then says the release will be fetched.
    #[serde(default)]
    pub noubin_key: Option<String>,
}

/// The linked Noubin release a download will be united against (Path A).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkedNoubin {
    /// The key as it will be written to the item.
    pub noubin_key: String,
    /// The canonical metadata URL that will be fetched (safe to show).
    pub fetch_url: String,
    pub host: String,
}


/// `POST /api/stores/<store>/download` (and the body shape the portal sends).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRequest {
    #[serde(default)]
    pub consent: Option<bool>,
    #[serde(default)]
    pub consent_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

/// What one track will cost, as the consent sheet shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentTrack {
    pub id: String,
    pub title: String,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub duration_s: Option<u64>,
    /// Declared size of the *advertised* file; null when the store declared none.
    pub estimated_bytes: Option<u64>,
    /// `true`: the number above is the store's declared file size, not the size of the
    /// lossy stream that will actually arrive.
    pub estimated: bool,
}

/// One release on the consent sheet.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsentRelease {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<u16>,
    pub track_count: usize,
    pub estimated_bytes: Option<u64>,
    pub estimated: bool,
    pub tracks: Vec<ConsentTrack>,
}

/// The caps, in bytes, shown next to the estimate.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadLimitsView {
    pub max_file_bytes: u64,
    pub max_batch_bytes: u64,
    pub free_reserve_bytes: u64,
}

/// The quality warning that cannot be dropped from the consent sheet.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityWarning {
    pub audio: &'static str,
    pub bitrate_kbps: Option<u32>,
    pub note: &'static str,
    pub warning: &'static str,
}

/// Something in the selection that will not be fetched, and why.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefusedTrack {
    pub release_id: String,
    pub release: String,
    pub track_id: Option<String>,
    pub track: Option<String>,
    pub code: String,
    pub reason: String,
}

/// `POST …/download/prepare` answer: exactly what a download would fetch. Nothing was sent
/// to a CDN and nothing was written.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadConsent {
    /// Opaque id the start call must echo.
    pub consent_id: String,
    pub store: &'static str,
    pub releases: Vec<ConsentRelease>,
    pub tracks: usize,
    /// Sum of the declared sizes (null when the store declared none).
    pub estimated_bytes: Option<u64>,
    /// `true` when `estimated_bytes` is derived from the advertised file sizes.
    pub estimated: bool,
    /// Free bytes where the files would be staged (null when it cannot be read).
    pub free_bytes: Option<u64>,
    pub limits: DownloadLimitsView,
    pub quality: QualityWarning,
    pub refused: Vec<RefusedTrack>,
    pub consent_message: String,
    /// The Noubin release this download will be united against, when one was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noubin: Option<LinkedNoubin>,
}

/// `POST …/download` answer.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadStart {
    pub ok: bool,
    pub session: String,
    pub state: DownloadState,
    pub items: usize,
}

/// State of one download item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
    Skipped,
}

/// State of the whole session. `Partial` is deliberately not `Done`: a person must be able
/// to tell that some tracks are missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadState {
    Running,
    Done,
    Partial,
    Cancelled,
    Failed,
}

/// One track's progress and result.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadItem {
    pub release_id: String,
    pub release: String,
    pub track_id: String,
    pub track: String,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub state: ItemState,
    pub bytes: u64,
    pub total_bytes: Option<u64>,
    /// Measured from the staged file when possible, else the store's declared length.
    pub duration_ms: Option<u64>,
    /// Container sniffed from the bytes (`mp3`, `flac`, …).
    pub format: Option<String>,
    /// `lossy` or `lossless`.
    pub quality: Option<String>,
    pub bitrate_kbps: Option<u32>,
    /// The original file was obtained (not a stream substitute).
    pub original: bool,
    /// The store substituted its lossy stream for the purchase.
    pub stream_substituted: bool,
    pub error: Option<String>,
    pub error_code: Option<String>,
}

/// What the session status endpoint returns.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadStatus {
    pub session: String,
    pub state: DownloadState,
    pub items: Vec<DownloadItem>,
    /// Index into `items` of the track being fetched.
    pub current: Option<usize>,
    pub done_bytes: u64,
    /// Sum of the declared sizes, when the store declared them.
    pub total_bytes: u64,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    /// Files this session placed in the library (0 while a download runs).
    pub placed_files: usize,
    /// What the unite hand-off did: the release was placed in the library, or why it was not.
    pub unite: Option<HandedOff>,
    pub cancelled: bool,
}

/// Outcome of the unite hand-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HandOffState {
    /// The release was written into the library: a playable item exists now.
    Placed,
    /// The release was already in the library: nothing was written, and the item that was
    /// there is still listed. Reported apart from `placed` so no status claims a write
    /// that did not happen.
    Exists,
    /// The staged files could not be placed (or were not usable); nothing was added.
    Rejected,
}

/// What the unite entry point said (BCS_05 returns a richer `UniteResult`; the worker keeps
/// this shape in its status).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandedOff {
    pub state: HandOffState,
    pub entry_point: &'static str,
    pub detail: String,
    /// Library-relative `.noudata` that was written, when the release was placed.
    pub noudata: Option<String>,
    /// Every `.noudata` this session wrote. A multi-release selection places more than
    /// one; the first is repeated in [`Self::noudata`] for the single-release case.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub noudata_paths: Vec<String>,
    /// The index picked every new release up (the scanner retries when this is false).
    pub indexed: bool,
    /// Which metadata the item carries: `"noudata"` (a linked Noubin release) or `"store"`
    /// (built from the store's fields because no release was linked). Null when nothing
    /// was placed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    /// `true` when the item was placed from store metadata: a Noubin URL can still be
    /// linked by downloading the release again with one (the audio is in the library, but
    /// only staging can be united).
    #[serde(default)]
    pub can_link: bool,
}

/// One downloaded file ready for the unite step.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedTrackFile {
    pub track_id: String,
    pub title: String,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub path: PathBuf,
    pub bytes: u64,
    pub duration_ms: Option<u64>,
    /// `duration_ms` was measured from the bytes on disk (not the store's declaration).
    #[serde(default)]
    pub duration_measured: bool,
    pub format: String,
    pub quality: String,
    pub bitrate_kbps: Option<u32>,
    pub original: bool,
    pub stream_substituted: bool,
}

/// All staged files of one release, plus the metadata BCS_05 needs to unite them.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedRelease {
    pub release_id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<u16>,
    /// Release creation time (Unix seconds) when the store reports one; `None` is never a
    /// reason to invent a date.
    #[serde(default)]
    pub created_unix: Option<i64>,
    pub tracks: Vec<StagedTrackFile>,
}

/// The request handed to BCS_05's unite entry point.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedStoreDownload {
    pub store: String,
    pub session: String,
    /// Every selected track arrived (a partial run carries `false`).
    pub complete: bool,
    pub releases: Vec<StagedRelease>,
}

// ---------------------------------------------------------------------------
// Internal planning types
// ---------------------------------------------------------------------------

/// One track the worker will fetch.
#[derive(Debug, Clone)]
pub(crate) struct PlannedTrack {
    pub(crate) release_id: String,
    pub(crate) release_title: String,
    pub(crate) release_artist: String,
    pub(crate) release_year: Option<u16>,
    pub(crate) release_created_unix: Option<i64>,
    pub(crate) track_id: String,
    pub(crate) title: String,
    pub(crate) track_number: Option<u32>,
    pub(crate) disc_number: Option<u32>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) declared_bytes: Option<u64>,
}

/// The consent plan held between prepare and start.
#[derive(Debug, Clone)]
pub(crate) struct PreparedDownload {
    pub(crate) id: String,
    pub(crate) items: Vec<PlannedTrack>,
    pub(crate) known_bytes: u64,
    /// The Noubin key the sheet named (Path A); `None` is Path B.
    pub(crate) noubin_key: Option<String>,
}

/// The active (or last) session, for polling and cancelling.
pub(crate) struct DownloadSlot {
    pub(crate) id: String,
    pub(crate) status: Arc<Mutex<DownloadStatus>>,
    pub(crate) cancel: Cancel,
}

// ---------------------------------------------------------------------------
// StoreHub: prepare / start / status / cancel
// ---------------------------------------------------------------------------

impl StoreHub {
    /// Step 1: the consent sheet. Reads the store (no CDN request) and writes **nothing**.
    pub fn download_prepare(
        &self,
        req: PrepareDownloadRequest,
    ) -> Result<DownloadConsent, StoreApiError> {
        let _guard = self.begin_prepare()?;
        let lim = self.limits();

        // A linked Noubin release is validated here, before any consent is asked: the web
        // hub's own key rules decide what is a key, and `prepare` sends nothing.
        let linked = match req
            .noubin_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            Some(raw) => {
                let web = self
                    .web
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                    .ok_or_else(|| {
                        StoreApiError::NotReady(
                            "the online lookup is not available, so a Noubin URL cannot be \
                             linked to this download"
                                .into(),
                        )
                    })?;
                match web.prepare(raw) {
                    Ok(prepared) => Some(LinkedNoubin {
                        noubin_key: prepared.noubin_key,
                        fetch_url: prepared.fetch_url,
                        host: prepared.host,
                    }),
                    Err(error) => {
                        return Err(StoreApiError::InvalidUrl(format!(
                            "that is not a Noubin URL this server can fetch: {error}"
                        )))
                    }
                }
            }
            None => None,
        };

        let mut refused: Vec<RefusedTrack> = Vec::new();
        let mut loaded: HashMap<String, StoreReleaseDetail> = HashMap::new();
        let mut planned: Vec<PlannedTrack> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for raw in &req.releases {
            let id = raw.trim();
            if id.is_empty() || id.len() > 256 {
                refused.push(refused_release(raw, "bad_id", "that is not a release id"));
                continue;
            }
            match self.load_for_prepare(&mut loaded, id) {
                Ok(detail) => {
                    for track in &detail.tracks {
                        push_planned(&detail.release, track, &mut planned, &mut seen);
                    }
                }
                Err(StoreApiError::Store(plippa_store::StoreError::NotFound)) => {
                    refused.push(refused_release(
                        raw,
                        "not_found",
                        "that release is not in the collection",
                    ));
                }
                Err(error) => return Err(error),
            }
        }

        for selection in &req.tracks {
            let release_id = selection.release.trim();
            let track_id = selection.track.trim();
            if release_id.is_empty() || track_id.is_empty() {
                refused.push(RefusedTrack {
                    release_id: release_id.to_string(),
                    release: release_id.to_string(),
                    track_id: Some(track_id.to_string()),
                    track: None,
                    code: "bad_id".into(),
                    reason: "a track selection needs both a release and a track id".into(),
                });
                continue;
            }
            let detail = match self.load_for_prepare(&mut loaded, release_id) {
                Ok(detail) => detail,
                Err(StoreApiError::Store(plippa_store::StoreError::NotFound)) => {
                    refused.push(refused_release(
                        release_id,
                        "not_found",
                        "that release is not in the collection",
                    ));
                    continue;
                }
                Err(error) => return Err(error),
            };
            match detail.tracks.iter().find(|t| t.id.as_str() == track_id) {
                Some(track) => push_planned(&detail.release, track, &mut planned, &mut seen),
                None => refused.push(RefusedTrack {
                    release_id: release_id.to_string(),
                    release: detail.release.title.clone(),
                    track_id: Some(track_id.to_string()),
                    track: None,
                    code: "not_found".to_string(),
                    reason: "that track is not in this release".into(),
                }),
            }
        }

        // A track whose advertised file is above the per-file cap cannot be fetched. Refuse
        // it here, where the person can see it, rather than failing mid-queue.
        let mut kept: Vec<PlannedTrack> = Vec::with_capacity(planned.len());
        for track in planned {
            if track.declared_bytes.is_some_and(|b| b > lim.max_file_bytes) {
                refused.push(RefusedTrack {
                    release_id: track.release_id.clone(),
                    release: track.release_title.clone(),
                    track_id: Some(track.track_id.clone()),
                    track: Some(track.title.clone()),
                    code: "too_large".into(),
                    reason: format!(
                        "the store declares this file above the {} MB per-file cap",
                        lim.max_file_bytes / (1024 * 1024)
                    ),
                });
            } else {
                kept.push(track);
            }
        }
        let planned = kept;
        if planned.is_empty() {
            return Err(StoreApiError::NotReady(
                "nothing to download: select at least one track".into(),
            ));
        }

        let known_bytes: u64 = planned
            .iter()
            .filter_map(|t| t.declared_bytes)
            .fold(0u64, u64::saturating_add);
        if known_bytes > lim.max_batch_bytes {
            return Err(StoreApiError::BatchTooLarge(format!(
                "the selection is about {} MB, above the {} MB batch cap",
                known_bytes / (1024 * 1024),
                lim.max_batch_bytes / (1024 * 1024)
            )));
        }

        // Free space is checked now, from the estimates, and again on every response header
        // by the downloader. A missing estimate falls back to the header check.
        let staging = self.staging_parent()?;
        let free = free_bytes_near(&staging, lim.free_space);
        if known_bytes > 0 {
            if let Some(free) = free {
                if known_bytes.saturating_add(lim.reserve_for(&staging)) > free {
                    return Err(StoreApiError::NoSpace {
                        needed: known_bytes,
                        free,
                    });
                }
            }
        }

        let consent_id = new_id();
        let releases = consent_releases(&planned);
        let estimate = (known_bytes > 0).then_some(known_bytes);
        self.prepared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(PreparedDownload {
            id: consent_id.clone(),
            items: planned.clone(),
            known_bytes,
            noubin_key: linked.as_ref().map(|l| l.noubin_key.clone()),
        });

        let mut message = format!(
            "This will download {} track(s) from {STORE_SLUG} over the internet. The audio is \
             the store's lossy stream, not the purchased FLAC. Sizes are checked as each \
             download starts ({} MB per file).",
            planned.len(),
            lim.max_file_bytes / (1024 * 1024)
        );
        if let Some(linked) = &linked {
            // The linked fetch is part of the same agreement: say so before any socket opens.
            message.push_str(&format!(
                " It will also fetch the linked Noubin release from {} and use it as the \
                 item's metadata.",
                linked.host
            ));
        }
        message.push_str(" Continue?");
        Ok(DownloadConsent {
            consent_id,
            store: STORE_SLUG,
            releases,
            tracks: planned.len(),
            estimated_bytes: estimate,
            estimated: estimate.is_some(),
            free_bytes: free,
            limits: DownloadLimitsView {
                max_file_bytes: lim.max_file_bytes,
                max_batch_bytes: lim.max_batch_bytes,
                free_reserve_bytes: lim.reserve_for(&staging),
            },
            quality: quality_warning(),
            refused,
            consent_message: message,
            noubin: linked,
        })
    }

    /// Step 2: start the worker. Needs `consent: true` and the `consentId` just shown.
    ///
    /// `transport` is the server's hardened download transport (the same one NB_09 uses);
    /// the worker adds the store's host allowlist through its [`FetchPolicy`].
    pub fn download_start(
        self: &Arc<Self>,
        transport: Arc<dyn Transport>,
        req: DownloadRequest,
    ) -> Result<DownloadStart, StoreApiError> {
        let consent = Consent::from_request(req.consent).map_err(|_| StoreApiError::Consent)?;
        // Take the in-flight gate before consuming the plan, so a second start cannot eat it.
        let guard = self.begin_download()?;
        let plan = {
            let mut slot = self
                .prepared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match slot.as_ref() {
                None => return Err(StoreApiError::StaleConsent),
                Some(plan) if Some(plan.id.as_str()) != req.consent_id.as_deref() => {
                    return Err(StoreApiError::StaleConsent)
                }
                Some(_) => slot.take().expect("checked"),
            }
        };

        // The store policy carries the host allowlist; unlike the general web policy it is
        // never relaxed here.
        let policy = self.download_policy();

        let session = new_id();
        let status = Arc::new(Mutex::new(DownloadStatus {
            session: session.clone(),
            state: DownloadState::Running,
            items: plan.items.iter().map(item_from_plan).collect(),
            current: None,
            done_bytes: 0,
            total_bytes: plan.known_bytes,
            started_at: now_secs(),
            finished_at: None,
            warnings: Vec::new(),
            errors: Vec::new(),
            placed_files: 0,
            unite: None,
            cancelled: false,
        }));
        let cancel = Cancel::new();
        {
            let mut slot = self
                .download
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(DownloadSlot {
                id: session.clone(),
                status: Arc::clone(&status),
                cancel: cancel.clone(),
            });
        }

        let items = plan.items.len();
        let job = DownloadJob {
            hub: Arc::clone(self),
            transport,
            policy,
            consent,
            lim: self.limits(),
            plan,
            session: session.clone(),
            status,
            cancel,
            pause: self.download_pause,
            _guard: guard,
        };
        if let Err(error) = std::thread::Builder::new()
            .name("plippa-store-dl".into())
            .spawn(move || run_download(job))
        {
            *self
                .download
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            return Err(StoreApiError::Unavailable(format!(
                "cannot start the download worker: {error}"
            )));
        }
        log_line(&format!(
            "store download started: {items} track(s), session {session}"
        ));
        Ok(DownloadStart {
            ok: true,
            session,
            state: DownloadState::Running,
            items,
        })
    }

    /// `GET /api/stores/<store>/download/<session>`.
    pub fn download_status(&self, session: &str) -> Result<DownloadStatus, StoreApiError> {
        let slot = self
            .download
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(slot) = slot.as_ref().filter(|slot| slot.id == session) else {
            return Err(StoreApiError::UnknownSession);
        };
        let snapshot = slot
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Ok(snapshot)
    }

    /// `POST /api/stores/<store>/download/<session>/cancel`.
    pub fn download_cancel(&self, session: &str) -> Result<DownloadStatus, StoreApiError> {
        let slot = self
            .download
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(slot) = slot.as_ref().filter(|slot| slot.id == session) else {
            return Err(StoreApiError::UnknownSession);
        };
        slot.cancel.cancel();
        let snapshot = slot
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Ok(snapshot)
    }

    /// Cancel whatever is running (disconnect).
    pub(crate) fn cancel_download_any(&self) {
        let slot = self
            .download
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(slot) = slot.as_ref() {
            slot.cancel.cancel();
        }
    }

    fn begin_prepare(&self) -> Result<CallGuard<'_>, StoreApiError> {
        if self
            .prepare_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(StoreApiError::Busy);
        }
        Ok(CallGuard(&self.prepare_busy))
    }

    fn begin_download(self: &Arc<Self>) -> Result<BusyDownload, StoreApiError> {
        if self
            .download_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(StoreApiError::Busy);
        }
        Ok(BusyDownload(Arc::clone(self)))
    }

    /// The policy every store download uses: the store allowlist on the first URL and on
    /// every redirect hop. Only this crate's tests ask for loopback fixtures.
    fn download_policy(&self) -> FetchPolicy {
        FetchPolicy::for_store(Arc::clone(&self.hosts))
    }

    fn load_for_prepare(
        &self,
        loaded: &mut HashMap<String, StoreReleaseDetail>,
        id: &str,
    ) -> Result<StoreReleaseDetail, StoreApiError> {
        if let Some(detail) = loaded.get(id) {
            return Ok(detail.clone());
        }
        let detail = self.release_detail_readonly(id)?;
        loaded.insert(id.to_string(), detail.clone());
        Ok(detail)
    }

    /// `<library>/.plippa/tmp` — computed, never created here (prepare writes nothing).
    fn staging_parent(&self) -> Result<PathBuf, StoreApiError> {
        let root = self
            .library_root()
            .ok_or_else(|| StoreApiError::Unavailable("the media library root is not available yet".into()))?;
        Ok(root.join(STATE_DIR).join("tmp"))
    }

    /// Remove staging folders a previous process left behind (killed mid-download).
    ///
    /// Only `store-<session>` directories under `<library>/.plippa/tmp` are touched, and
    /// only when they are real directories (never a symlink planted on the stick). They
    /// hold partial media and never a `.noudata`, so nothing playable can be lost.
    pub(crate) fn sweep_stale_stages(root: &Path) {
        let tmp = root.join(STATE_DIR).join("tmp");
        let Ok(entries) = fs::read_dir(&tmp) else {
            return;
        };
        for entry in entries.filter_map(|entry| entry.ok()) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with("store-") {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() && !meta.file_type().is_symlink() {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }

    /// Create `<library>/.plippa/tmp/store-<session>/` for the worker.
    fn stage_dir(&self, session: &str) -> Result<PathBuf, StoreApiError> {
        let tmp = self.staging_parent()?;
        // The tmp chain must be real directories; never follow a link planted on the stick.
        fs::create_dir_all(&tmp)
            .map_err(|error| StoreApiError::Cache(format!("cannot create the staging area: {error}")))?;
        let meta = fs::symlink_metadata(&tmp)
            .map_err(|error| StoreApiError::Cache(format!("cannot inspect the staging area: {error}")))?;
        if !meta.is_dir() {
            return Err(StoreApiError::Cache(
                "the staging area is not a plain directory".into(),
            ));
        }
        let stage = tmp.join(format!("store-{session}"));
        // `create_dir` (not `create_dir_all`) refuses an existing directory, so one session
        // can never write into another session's staging folder.
        fs::create_dir(&stage).map_err(|error| {
            StoreApiError::Cache(format!("cannot create the download staging folder: {error}"))
        })?;
        Ok(stage)
    }
}

/// Releases the download gate at the end of a worker, however it ends.
///
/// Holds the hub (not the flag) so the worker can release the gate from its own thread
/// after [`StoreHub::download_start`] has returned.
struct BusyDownload(Arc<StoreHub>);

impl Drop for BusyDownload {
    fn drop(&mut self) {
        self.0.download_busy.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

struct DownloadJob {
    hub: Arc<StoreHub>,
    transport: Arc<dyn Transport>,
    policy: FetchPolicy,
    consent: Consent,
    lim: DownloadLimits,
    plan: PreparedDownload,
    session: String,
    status: Arc<Mutex<DownloadStatus>>,
    cancel: Cancel,
    pause: Duration,
    /// Held only for its `Drop`, which releases the in-flight gate.
    _guard: BusyDownload,
}

/// Recompute the session-wide totals from the per-track state.
///
/// The device screen draws a single bar and byte counter for the whole selection, so it
/// reads `done_bytes`/`total_bytes` off the status header. Those must be summed here:
/// the per-track `bytes` moving on its own leaves the header stuck at zero for the whole
/// download (the track counter still advances, which hides the fault).
fn refresh_totals(status: &mut DownloadStatus) {
    status.done_bytes = status
        .items
        .iter()
        .map(|item| item.bytes)
        .fold(0u64, u64::saturating_add);
    // A track whose size the store never declared counts as what has arrived so far, so
    // the bar neither stalls at zero nor runs past its end.
    let total = status
        .items
        .iter()
        .map(|item| item.total_bytes.unwrap_or(item.bytes))
        .fold(0u64, u64::saturating_add);
    status.total_bytes = total.max(status.done_bytes);
}

impl DownloadJob {
    fn set(&self, index: usize, change: impl FnOnce(&mut DownloadItem)) {
        if let Ok(mut status) = self.status.lock() {
            if let Some(item) = status.items.get_mut(index) {
                change(item);
            }
            refresh_totals(&mut status);
        }
    }

    fn note_error(&self, message: String) {
        if let Ok(mut status) = self.status.lock() {
            status.errors.push(message);
        }
    }

    fn warn(&self, message: String) {
        if let Ok(mut status) = self.status.lock() {
            status.warnings.push(message);
        }
    }

    fn finish(&self, state: DownloadState) {
        if let Ok(mut status) = self.status.lock() {
            status.state = state;
            status.current = None;
            status.finished_at = Some(now_secs());
            if state == DownloadState::Cancelled {
                status.cancelled = true;
                for item in status.items.iter_mut().filter(|item| {
                    matches!(item.state, ItemState::Queued | ItemState::Running | ItemState::Done)
                }) {
                    item.state = ItemState::Cancelled;
                }
            } else {
                for item in status
                    .items
                    .iter_mut()
                    .filter(|item| matches!(item.state, ItemState::Queued | ItemState::Running))
                {
                    item.state = ItemState::Skipped;
                }
            }
            refresh_totals(&mut status);
        }
    }

    /// Sleep between tracks in slices so a cancel is noticed at once.
    fn pause_between_tracks(&self) -> bool {
        let deadline = Instant::now() + self.pause;
        loop {
            if self.cancel.is_cancelled() {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
        }
    }
}

fn run_download(job: DownloadJob) {
    let stage = match job.hub.stage_dir(&job.session) {
        Ok(stage) => stage,
        Err(error) => {
            job.note_error(error.message());
            job.finish(DownloadState::Failed);
            return;
        }
    };
    let provider = match job.hub.load_creds() {
        Some(creds) => match job.hub.provider(&creds) {
            Ok(provider) => provider,
            Err(error) => {
                job.note_error(StoreApiError::Store(error).message());
                remove_stage(&stage);
                job.finish(DownloadState::Failed);
                return;
            }
        },
        None => {
            job.note_error(StoreApiError::NotConnected.message());
            remove_stage(&stage);
            job.finish(DownloadState::Failed);
            return;
        }
    };

    let budget = Budget::new(job.lim.max_batch_bytes);
    let mut staged: Vec<(PlannedTrack, StagedTrackFile)> = Vec::new();
    let mut release_dirs: HashMap<String, PathBuf> = HashMap::new();
    let mut failed = 0usize;
    let mut saw_cancel = false;
    let total = job.plan.items.len();
    // A release already in the library is answered from disk: the worker fetches nothing
    // for it and the unite step reports `exists`, so a replay costs no requests and no
    // bytes. This scan is disk-only (BCS_05's own rule).
    let present: HashSet<String> = match job.hub.library_root() {
        Some(root) => {
            let ids: Vec<String> = job
                .plan
                .items
                .iter()
                .map(|item| item.release_id.clone())
                .collect();
            super::post_unite::present_release_ids(&root, &ids)
        }
        None => HashSet::new(),
    };

    for (index, planned) in job.plan.items.iter().enumerate() {
        if present.contains(&planned.release_id) {
            job.set(index, |item| {
                item.state = ItemState::Skipped;
                item.bytes = 0;
            });
            continue;
        }
        if job.cancel.is_cancelled() {
            saw_cancel = true;
            break;
        }
        if let Ok(mut status) = job.status.lock() {
            status.current = Some(index);
            if let Some(item) = status.items.get_mut(index) {
                item.state = ItemState::Running;
            }
        }

        let track_ref = StoreTrackRef::new(&planned.track_id);
        let source = match provider.fetch_track_audio(&track_ref, AudioPreference::OriginalOrStream) {
            Ok(source) => source,
            Err(error) => {
                failed += 1;
                let code = super::StoreApiError::Store(error.clone()).code();
                job.set(index, |item| {
                    item.state = ItemState::Failed;
                    item.error = Some(error.to_string());
                    item.error_code = Some(code.to_string());
                });
                job.note_error(format!("{} — {}: {error}", planned.release_title, planned.title));
                continue;
            }
        };

        if source
            .expected_bytes
            .is_some_and(|bytes| bytes > job.lim.max_file_bytes)
        {
            failed += 1;
            job.set(index, |item| {
                item.state = ItemState::Failed;
                item.error = Some(format!(
                    "larger than the {} MB per-file limit",
                    job.lim.max_file_bytes / (1024 * 1024)
                ));
                item.error_code = Some("too_large".into());
            });
            job.note_error(format!(
                "{} — {}: the store declares a file above the per-file cap",
                planned.release_title, planned.title
            ));
            continue;
        }

        let mut last_progress = Instant::now() - Duration::from_secs(1);
        let downloaded = download_to_temp(
            job.transport.as_ref(),
            &job.consent,
            &job.policy,
            &job.lim,
            source.url.expose(),
            &stage,
            Want::Media,
            &budget,
            &job.cancel,
            &mut |received, expected| {
                if last_progress.elapsed() >= Duration::from_millis(200) {
                    last_progress = Instant::now();
                    job.set(index, |item| {
                        item.bytes = received;
                        item.total_bytes = expected;
                    });
                }
            },
        );

        match downloaded {
            Ok(download) => {
                let Some(kind) = download.audio else {
                    failed += 1;
                    job.set(index, |item| {
                        item.state = ItemState::Failed;
                        item.error = Some("the content is not audio Plippa can play".into());
                        item.error_code = Some("wrong_type".into());
                    });
                    continue;
                };
                // One folder per release, numbered so two ids that sanitise to the same
                // name (`a:1` and `a-1`, say) still get distinct directories.
                let release_dir = match release_dirs.get(&planned.release_id) {
                    Some(dir) => dir.clone(),
                    None => {
                        let dir = stage.join(format!(
                            "r{:02}-{}",
                            release_dirs.len(),
                            release_dir_name(&planned.release_id)
                        ));
                        release_dirs.insert(planned.release_id.clone(), dir.clone());
                        dir
                    }
                };
                if let Err(error) = fs::create_dir_all(&release_dir) {
                    failed += 1;
                    job.set(index, |item| {
                        item.state = ItemState::Failed;
                        item.error = Some(format!("cannot stage the file: {error}"));
                        item.error_code = Some("io".into());
                    });
                    job.note_error(format!(
                        "{} — {}: cannot stage the file",
                        planned.release_title, planned.title
                    ));
                    continue;
                }
                // The queue position keeps the name unique even when two tracks share a
                // disc/track number; the tags carry the real ordering into BCS_05.
                let file_name = format!(
                    "{:04}-{:02}-{:02}.{}",
                    index + 1,
                    planned.disc_number.unwrap_or(1),
                    planned.track_number.unwrap_or((index + 1) as u32),
                    kind.ext()
                );
                let destination = release_dir.join(file_name);
                let source_path = download.temp.keep();
                if let Err(error) = fs::rename(&source_path, &destination) {
                    let _ = fs::remove_file(&source_path);
                    failed += 1;
                    job.set(index, |item| {
                        item.state = ItemState::Failed;
                        item.error = Some(format!("cannot keep the file: {error}"));
                        item.error_code = Some("io".into());
                    });
                    continue;
                }
                let probed = crate::scanner::probe_duration_ms(&destination);
                let duration = probed.or(planned.duration_ms);
                let (quality, bitrate) = quality_of(source.quality);
                let file = StagedTrackFile {
                    track_id: planned.track_id.clone(),
                    title: planned.title.clone(),
                    track_number: planned.track_number,
                    disc_number: planned.disc_number,
                    path: destination,
                    bytes: download.size,
                    duration_ms: duration,
                    duration_measured: probed.is_some(),
                    format: kind.ext().to_string(),
                    quality: quality.to_string(),
                    bitrate_kbps: bitrate,
                    original: !source.stream_fallback,
                    stream_substituted: source.stream_fallback,
                };
                job.set(index, |item| {
                    item.state = ItemState::Done;
                    item.bytes = file.bytes;
                    item.total_bytes = Some(file.bytes);
                    item.duration_ms = file.duration_ms;
                    item.format = Some(file.format.clone());
                    item.quality = Some(file.quality.clone());
                    item.bitrate_kbps = file.bitrate_kbps;
                    item.original = file.original;
                    item.stream_substituted = file.stream_substituted;
                    item.error = None;
                    item.error_code = None;
                });
                if file.stream_substituted {
                    job.warn(format!(
                        "{} — {}: the store served its lossy stream, not the purchased file",
                        planned.release_title, planned.title
                    ));
                }
                staged.push((planned.clone(), file));
            }
            Err(DownloadError::Cancelled) => {
                saw_cancel = true;
                job.set(index, |item| item.state = ItemState::Cancelled);
                break;
            }
            Err(error) => {
                failed += 1;
                let code = error.code().to_string();
                let message = error.to_string();
                job.set(index, |item| {
                    item.state = ItemState::Failed;
                    item.error = Some(message.clone());
                    item.error_code = Some(code.clone());
                });
                job.note_error(format!("{} — {}: {message}", planned.release_title, planned.title));
            }
        }

        if index + 1 < total && !job.pause_between_tracks() {
            saw_cancel = true;
            break;
        }
    }

    if job.cancel.is_cancelled() {
        saw_cancel = true;
    }
    if saw_cancel {
        remove_stage(&stage);
        if let Ok(mut status) = job.status.lock() {
            status.placed_files = 0;
            status.unite = None;
        }
        job.finish(DownloadState::Cancelled);
        log_line("store download cancelled; partial files removed");
        return;
    }

    if staged.is_empty() && present.is_empty() {
        remove_stage(&stage);
        job.note_error("no track could be downloaded".to_string());
        job.finish(DownloadState::Failed);
        return;
    }
    if staged.is_empty() {
        // Everything asked for was already in the library. The hand-off still names those
        // releases so the answer is `exists` (with `canReplace`), not an error.
        log_line("store download: every requested release is already in the library");
    }

    let complete = failed == 0;
    let hand_off = build_hand_off(&stage, &job.plan, &job.session, staged, &present, complete);
    if job.hub.library_root().is_none() {
        remove_stage(&stage);
        job.note_error("the media library root is not available".to_string());
        job.finish(DownloadState::Failed);
        return;
    }
    let mut united = match job.plan.noubin_key.clone() {
        // Path A: the sheet named a Noubin release, so the person agreed to its fetch too.
        Some(key) => job.hub.post_download_unite(PostDownloadUnite {
            noubin_key: Some(key),
            consent: true,
            ..PostDownloadUnite::store_metadata_only(hand_off.clone())
        }),
        None => job
            .hub
            .post_download_unite(PostDownloadUnite::store_metadata_only(hand_off.clone())),
    };
    if job.plan.noubin_key.is_some() && united.state == UniteState::Rejected {
        // The linked document is a bonus, not a condition of keeping the audio: place the
        // files with the store's own metadata and say, plainly, that the link failed.
        let why = united.detail.clone();
        job.warn(format!(
            "The linked Noubin release could not be used ({why}) — this item's metadata \
             comes from {STORE_SLUG}. Download it again with a working Noubin URL to link it."
        ));
        united = job
            .hub
            .post_download_unite(PostDownloadUnite::store_metadata_only(hand_off));
    }
    for warning in &united.warnings {
        job.warn(warning.clone());
    }
    let handed = united.to_handed_off();
    // Files this session actually added. A replay that found the release already in the
    // library placed nothing, however many tracks it staged on the way.
    let placed_files = match handed.state {
        HandOffState::Placed => united.track_count,
        HandOffState::Exists | HandOffState::Rejected => 0,
    };
    // The writer moved every file out of staging; the folder and any empty release
    // directories are all that is left. A release that could not be placed is not left
    // behind for the next step to trip over either: nothing was added to the library.
    remove_stage(&stage);
    if let Ok(mut status) = job.status.lock() {
        status.placed_files = placed_files;
        status.unite = Some(handed.clone());
    }
    match handed.state {
        HandOffState::Placed | HandOffState::Exists => {
            // BCS_07: a completed download answers the interest a `not_owned` tap recorded,
            // so the next tap of that link is a plain play from the library.
            if let Some(root) = job.hub.library_root() {
                let mut url_keys: Vec<String> = Vec::new();
                if let Some(key) = job.plan.noubin_key.as_deref() {
                    if let Ok(parsed) = super::matching::parse_bandcamp_url(key) {
                        url_keys.push(parsed.url_key());
                    }
                }
                if !url_keys.is_empty() {
                    let cleared = super::matching::clear_wants(&root, &url_keys);
                    if cleared > 0 {
                        log_line(&format!(
                            "store download placed: cleared {cleared} recorded interest(s)"
                        ));
                    }
                }
            }
            // `partial` is never `done`: a person must be able to see that some tracks
            // did not arrive.
            let state = if complete {
                DownloadState::Done
            } else {
                DownloadState::Partial
            };
            if matches!(handed.state, HandOffState::Exists) {
                log_line("store download found in the library; nothing was added");
            }
            job.finish(state);
        }
        HandOffState::Rejected => {
            job.note_error(handed.detail.clone());
            job.finish(DownloadState::Failed);
        }
    }
}

// ---------------------------------------------------------------------------
// The unite hand-off (BCS_05 boundary)
// ---------------------------------------------------------------------------

/// BCS_05's entry point is [`crate::service::stores::post_unite::post_download_unite`] (the
/// store hub method `StoreHub::post_download_unite`); the worker below calls it directly
/// with `PostDownloadUnite::store_metadata_only` (Path B). BCS_07 calls the same method
/// with the tapped Noubin key and consent (Path A).
///
/// The one staging folder every file of the session lives under.
pub(crate) fn common_stage(download: &StagedStoreDownload) -> Option<PathBuf> {
    let mut stage: Option<PathBuf> = None;
    for track in download.releases.iter().flat_map(|r| r.tracks.iter()) {
        // `<stage>/rNN-<release>/<file>`: the release folder's parent is the stage.
        let parent = track.path.parent()?;
        let candidate = parent.parent()?.to_path_buf();
        match &stage {
            Some(known) if *known != candidate => return None,
            Some(_) => {}
            None => stage = Some(candidate),
        }
    }
    stage
}

fn build_hand_off(
    stage: &Path,
    plan: &PreparedDownload,
    session: &str,
    staged: Vec<(PlannedTrack, StagedTrackFile)>,
    present: &HashSet<String>,
    complete: bool,
) -> StagedStoreDownload {
    let mut order: Vec<String> = Vec::new();
    let mut by_release: HashMap<String, StagedRelease> = HashMap::new();
    for (planned, file) in staged {
        let entry = by_release
            .entry(planned.release_id.clone())
            .or_insert_with(|| {
                order.push(planned.release_id.clone());
                StagedRelease {
                    release_id: planned.release_id.clone(),
                    title: planned.release_title.clone(),
                    artist: planned.release_artist.clone(),
                    year: planned.release_year,
                    created_unix: planned.release_created_unix,
                    tracks: Vec::new(),
                }
            });
        debug_assert!(file.path.starts_with(stage));
        entry.tracks.push(file);
    }
    // A release the worker skipped because it is already in the library still has to be
    // named in the hand-off: that is what makes the answer `exists` instead of silence.
    for planned in &plan.items {
        if !present.contains(&planned.release_id) || by_release.contains_key(&planned.release_id) {
            continue;
        }
        order.push(planned.release_id.clone());
        by_release.insert(
            planned.release_id.clone(),
            StagedRelease {
                release_id: planned.release_id.clone(),
                title: planned.release_title.clone(),
                artist: planned.release_artist.clone(),
                year: planned.release_year,
                created_unix: planned.release_created_unix,
                tracks: Vec::new(),
            },
        );
    }
    let releases = order
        .into_iter()
        .filter_map(|id| by_release.remove(&id))
        .collect();
    StagedStoreDownload {
        store: STORE_SLUG.to_string(),
        session: session.to_string(),
        complete,
        releases,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn push_planned(
    release: &StoreRelease,
    track: &StoreTrack,
    planned: &mut Vec<PlannedTrack>,
    seen: &mut HashSet<String>,
) {
    let key = format!("{}\u{1f}{}", release.id.as_str(), track.id.as_str());
    if !seen.insert(key) {
        return;
    }
    planned.push(PlannedTrack {
        release_id: release.id.as_str().to_string(),
        release_title: release.title.clone(),
        release_artist: release.artist.clone(),
        release_year: release.year,
        release_created_unix: release.created_unix,
        track_id: track.id.as_str().to_string(),
        title: track.title.clone(),
        track_number: track.track_number,
        disc_number: track.disc_number,
        duration_ms: track.duration_ms,
        declared_bytes: track.declared_bytes,
    });
}

fn consent_releases(planned: &[PlannedTrack]) -> Vec<ConsentRelease> {
    let mut order: Vec<String> = Vec::new();
    let mut by_release: HashMap<String, ConsentRelease> = HashMap::new();
    for track in planned {
        let entry = by_release
            .entry(track.release_id.clone())
            .or_insert_with(|| {
                order.push(track.release_id.clone());
                ConsentRelease {
                    id: track.release_id.clone(),
                    title: track.release_title.clone(),
                    artist: track.release_artist.clone(),
                    year: track.release_year,
                    track_count: 0,
                    estimated_bytes: None,
                    estimated: false,
                    tracks: Vec::new(),
                }
            });
        entry.tracks.push(ConsentTrack {
            id: track.track_id.clone(),
            title: track.title.clone(),
            track: track.track_number,
            disc: track.disc_number,
            duration_s: track.duration_ms.map(|ms| (ms + 500) / 1000),
            estimated_bytes: track.declared_bytes,
            estimated: track.declared_bytes.is_some(),
        });
    }
    let mut out: Vec<ConsentRelease> = Vec::with_capacity(order.len());
    for id in order {
        if let Some(mut release) = by_release.remove(&id) {
            release.track_count = release.tracks.len();
            let total: u64 = release
                .tracks
                .iter()
                .filter_map(|t| t.estimated_bytes)
                .fold(0, u64::saturating_add);
            release.estimated_bytes = (total > 0).then_some(total);
            release.estimated = release.estimated_bytes.is_some();
            out.push(release);
        }
    }
    out
}

fn quality_warning() -> QualityWarning {
    let QualityNote {
        audio,
        bitrate_kbps,
        note,
    } = quality();
    QualityWarning {
        audio,
        bitrate_kbps,
        note,
        warning: STREAM_WARNING,
    }
}

fn quality_of(quality: AudioQuality) -> (&'static str, Option<u32>) {
    match quality {
        AudioQuality::Lossless => ("lossless", None),
        AudioQuality::Lossy { bitrate_kbps } => ("lossy", bitrate_kbps),
    }
}

fn item_from_plan(planned: &PlannedTrack) -> DownloadItem {
    DownloadItem {
        release_id: planned.release_id.clone(),
        release: planned.release_title.clone(),
        track_id: planned.track_id.clone(),
        track: planned.title.clone(),
        track_number: planned.track_number,
        disc_number: planned.disc_number,
        state: ItemState::Queued,
        bytes: 0,
        total_bytes: planned.declared_bytes,
        duration_ms: planned.duration_ms,
        format: None,
        quality: None,
        bitrate_kbps: None,
        original: false,
        stream_substituted: false,
        error: None,
        error_code: None,
    }
}

fn refused_release(id: &str, code: &str, reason: &str) -> RefusedTrack {
    RefusedTrack {
        release_id: id.trim().to_string(),
        release: id.trim().to_string(),
        track_id: None,
        track: None,
        code: code.to_string(),
        reason: reason.to_string(),
    }
}

/// Free bytes on the nearest existing directory at or above `dir` (never creates anything).
///
/// The probe is the download limits' own `free_space`, so a test can simulate a full disk
/// without filling one, and the byte the worker checks is the byte a header check sees.
fn free_bytes_near(dir: &Path, probe: fn(&Path) -> Option<u64>) -> Option<u64> {
    let mut current = dir;
    loop {
        if current.is_dir() {
            return probe(current);
        }
        current = current.parent()?;
    }
}

/// A directory name that cannot escape the staging folder and cannot collide.
fn release_dir_name(release_id: &str) -> String {
    let mut out = String::with_capacity(release_id.len().min(48) + 8);
    for ch in release_id.chars() {
        if out.len() >= 48 {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        out.push_str("release");
    }
    out
}

/// Remove `<tmp>/store-<session>/`, but only a path shaped like one we made.
fn remove_stage(stage: &Path) {
    let Some(name) = stage.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    if !name.starts_with("store-") || name.len() > MAX_SESSION_ID_BYTES + 8 {
        return;
    }
    let _ = fs::remove_dir_all(stage);
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    format!("{nanos:016x}{:016x}", rand::random::<u64>())
}
