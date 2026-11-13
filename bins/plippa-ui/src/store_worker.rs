//! One worker thread owns the store connection (BCS_07).
//!
//! The frame loop must never block on an HTTP round trip, so all calls go through a single
//! queue owned by a thread that lives as long as the app. The thread answers with [`StoreEvent`]
//! values the loop drains each frame. Cancelling a download is a request like any other, and a
//! reply that arrives after the screen moved on is dropped by the screen's own generation check.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};

use std::sync::Arc;

use crate::analysis::{AnalysisKind, AnalysisKindStatus};
use crate::art::{decode_store_cover, ArtImage};
use crate::store_api::{
    CollectionPageInfo, DeleteDone, DeletePreviewInfo, DownloadConsent, DownloadProgress,
    DownloadSession, PortalAddress, PortalSettingsInfo, ReleaseInfo, StoreApi, StoreApiError,
    StoresInfo, SyncStatus, TapAnswer,
};

/// A PIN on its way to the server. Its `Debug` hides the digits, so a stray `{:?}` of a request
/// can never put the PIN in a log.
#[derive(Clone, PartialEq, Eq)]
pub struct Pin(pub String);

impl std::fmt::Debug for Pin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pin(<hidden>)")
    }
}

/// A call the app wants made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreRequest {
    Tap {
        url: String,
        expected_duration_s: Option<u64>,
        /// The person agreed to contact the tag's own page (needed for a Noubin URL).
        consent: bool,
    },
    Want {
        url: String,
    },
    Prepare {
        noubin_key: String,
        release_id: String,
    },
    Start {
        consent_id: String,
    },
    Status {
        session: String,
    },
    Cancel {
        session: String,
    },
    /// BCS_08: the portal address for the connect QR code.
    Portal,
    /// BCS_08: is a store connected?
    Stores,
    /// BCS_08: one page of the collection (`query` empty = the whole collection).
    Collection {
        query: String,
        offset: u32,
        size: u32,
    },
    /// BCS_08: one release with its tracks and quality note.
    Release { release_id: String },
    /// BCS_08: what removing a playable would delete.
    DeletePreview { playable_id: u64 },
    /// BCS_08: remove the playable from this device.
    Delete { playable_id: u64 },
    /// BCS_11: start a collection sync.
    SyncStart,
    /// BCS_11: where the sync is (one poll; sends no secrets).
    SyncStatus,
    /// BCS_11: stop the running sync.
    SyncCancel,
    /// BCS_11: when the collection cache was last written (one-row collection page).
    LastSynced,
    /// BCS_11: fetch the release's cover and decode it to a `side` square, off the frame loop.
    ReleaseCover { release_id: String, side: u16 },
    /// TRACK_01: what has been analysed for one track.
    AnalysisStatus { playable_id: u64, item: u32 },
    /// TRACK_01: put one track at the front of the analysis queue (`None` = every kind).
    Reanalyse {
        playable_id: u64,
        item: u32,
        kind: Option<AnalysisKind>,
    },
    /// SECFIX_02: read the web portal's settings.
    PortalSettings,
    /// SECFIX_02: switch the web portal on or off.
    SetPortalEnabled(bool),
    /// SECFIX_03: serve the LAN portal over HTTPS (`true`, the default) or plain HTTP (`false`).
    SetHttps(bool),
    /// SECFIX_03: make a new certificate and key (the fingerprint changes).
    RegenerateCertificate,
    /// SECFIX_02: ask browsers for the PIN, or not.
    SetPinRequired(bool),
    /// SECFIX_02: the owner typed a new six-digit PIN.
    SetPin(Pin),
    /// SECFIX_02: forget every sign-in lockout.
    ClearLockout,
}

/// A call that came back.
#[derive(Debug, Clone, PartialEq)]
pub enum StoreEvent {
    Tap(Result<TapAnswer, StoreApiError>),
    Want(Result<(), StoreApiError>),
    Consent(Result<DownloadConsent, StoreApiError>),
    Session(Result<DownloadSession, StoreApiError>),
    Progress {
        session: String,
        progress: Result<DownloadProgress, StoreApiError>,
    },
    Cancel(Result<(), StoreApiError>),
    /// BCS_08 answers. The query and offset travel with the page so a slow answer for a
    /// list the person already replaced cannot be appended to the wrong place.
    Portal(Result<PortalAddress, StoreApiError>),
    Stores(Result<StoresInfo, StoreApiError>),
    Collection {
        query: String,
        offset: u32,
        page: Result<CollectionPageInfo, StoreApiError>,
    },
    Release { release_id: String, info: Result<ReleaseInfo, StoreApiError> },
    DeletePreview(Result<DeletePreviewInfo, StoreApiError>),
    Deleted(Result<DeleteDone, StoreApiError>),
    /// BCS_11: the answer to any of the three sync requests.
    Sync(Result<SyncStatus, StoreApiError>),
    /// BCS_11: when the collection cache was last written (Unix seconds), `None` if never.
    LastSynced(Result<Option<u64>, StoreApiError>),
    /// BCS_11: a cover, already decoded and cropped. The release id and side travel with it so
    /// a late answer for a screen the person left is dropped by the caller.
    ReleaseCover {
        release_id: String,
        side: u16,
        result: Result<Arc<ArtImage>, StoreApiError>,
    },
    /// TRACK_01: the status of one track. The ids travel with it so a late answer for a screen
    /// the person already left is dropped by the caller.
    AnalysisStatus {
        playable_id: u64,
        item: u32,
        result: Result<Vec<AnalysisKindStatus>, StoreApiError>,
    },
    /// TRACK_01: the answer to a re-analysis request.
    Reanalysed {
        playable_id: u64,
        item: u32,
        kind: Option<AnalysisKind>,
        result: Result<(), StoreApiError>,
    },
    /// SECFIX_02: the answer to any of the portal settings calls (the settings as they are
    /// now, or why the change was refused).
    PortalSettings(Result<PortalSettingsInfo, StoreApiError>),
}

/// The queue plus its thread. Dropping it ends the thread (after the call in flight).
pub struct StoreWorker {
    requests: Sender<StoreRequest>,
    events: Receiver<StoreEvent>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for StoreWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreWorker").finish_non_exhaustive()
    }
}

impl StoreWorker {
    /// Start the thread over `api`.
    pub fn new(api: Box<dyn StoreApi>) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<StoreRequest>();
        let (evt_tx, evt_rx) = mpsc::channel::<StoreEvent>();
        let thread = thread::Builder::new()
            .name("plippa-store".into())
            .spawn(move || run(api, req_rx, evt_tx))
            .ok();
        Self {
            requests: req_tx,
            events: evt_rx,
            thread,
        }
    }

    /// Queue a call. A dead thread is reported so the screen can stop waiting.
    pub fn send(&self, request: StoreRequest) -> bool {
        self.requests.send(request).is_ok()
    }

    /// Take one finished call, if there is one.
    pub fn try_recv(&self) -> Option<StoreEvent> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }

    /// True when the worker thread is gone (a wedged call may still resolve, so this is only
    /// used to avoid queueing forever).
    pub fn gone(&self) -> bool {
        match &self.thread {
            Some(thread) => thread.is_finished(),
            None => true,
        }
    }
}

impl Drop for StoreWorker {
    fn drop(&mut self) {
        // Closing the request channel ends the loop; a call in flight finishes on its own.
        let (dead_tx, _dead_rx) = mpsc::channel();
        let _ = std::mem::replace(&mut self.requests, dead_tx);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(api: Box<dyn StoreApi>, requests: Receiver<StoreRequest>, events: Sender<StoreEvent>) {
    let mut api = api;
    while let Ok(request) = requests.recv() {
        let event = match request {
            StoreRequest::Tap {
                url,
                expected_duration_s,
                consent,
            } => StoreEvent::Tap(api.tap(&url, expected_duration_s, consent)),
            StoreRequest::Want { url } => StoreEvent::Want(api.want(&url)),
            StoreRequest::Prepare {
                noubin_key,
                release_id,
            } => StoreEvent::Consent(api.download_prepare(&noubin_key, &release_id)),
            StoreRequest::Start { consent_id } => StoreEvent::Session(api.download_start(&consent_id)),
            StoreRequest::Status { session } => StoreEvent::Progress {
                progress: api.download_status(&session),
                session,
            },
            StoreRequest::Cancel { session } => StoreEvent::Cancel(api.download_cancel(&session)),
            StoreRequest::Portal => StoreEvent::Portal(api.portal_address()),
            StoreRequest::Stores => StoreEvent::Stores(api.stores()),
            StoreRequest::Collection {
                query,
                offset,
                size,
            } => StoreEvent::Collection {
                page: api.collection(&query, offset, size),
                query,
                offset,
            },
            StoreRequest::Release { release_id } => {
                let info = api.release(&release_id);
                StoreEvent::Release { release_id, info }
            }
            StoreRequest::DeletePreview { playable_id } => {
                StoreEvent::DeletePreview(api.delete_preview(playable_id))
            }
            StoreRequest::Delete { playable_id } => {
                StoreEvent::Deleted(api.delete_playable(playable_id))
            }
            StoreRequest::SyncStart => StoreEvent::Sync(api.sync_start()),
            StoreRequest::SyncStatus => StoreEvent::Sync(api.sync_status()),
            StoreRequest::SyncCancel => StoreEvent::Sync(api.sync_cancel()),
            StoreRequest::LastSynced => {
                StoreEvent::LastSynced(api.collection("", 0, 1).map(|page| page.synced_at))
            }
            StoreRequest::AnalysisStatus { playable_id, item } => StoreEvent::AnalysisStatus {
                result: api.waveform_status(playable_id, item),
                playable_id,
                item,
            },
            StoreRequest::Reanalyse {
                playable_id,
                item,
                kind,
            } => StoreEvent::Reanalysed {
                result: api.reanalyse(playable_id, item, kind),
                playable_id,
                item,
                kind,
            },
            StoreRequest::PortalSettings => StoreEvent::PortalSettings(api.portal_settings()),
            StoreRequest::SetPortalEnabled(on) => {
                StoreEvent::PortalSettings(api.set_portal_enabled(on))
            }
            StoreRequest::SetHttps(on) => StoreEvent::PortalSettings(api.set_https_enabled(on)),
            StoreRequest::RegenerateCertificate => {
                StoreEvent::PortalSettings(api.regenerate_certificate())
            }
            StoreRequest::SetPinRequired(required) => {
                StoreEvent::PortalSettings(api.set_pin_required(required))
            }
            StoreRequest::SetPin(pin) => StoreEvent::PortalSettings(api.set_pin(&pin.0)),
            StoreRequest::ClearLockout => StoreEvent::PortalSettings(api.clear_lockout()),
            StoreRequest::ReleaseCover { release_id, side } => {
                let result = fetch_cover(api.as_mut(), &release_id, side);
                StoreEvent::ReleaseCover {
                    release_id,
                    side,
                    result,
                }
            }
        };
        if events.send(event).is_err() {
            break;
        }
    }
}

/// Fetch and decode one cover. A decoder panic or a refused image is an error here, never a
/// dead worker: the screen shows its placeholder and carries on.
fn fetch_cover(
    api: &mut dyn StoreApi,
    release_id: &str,
    side: u16,
) -> Result<Arc<ArtImage>, StoreApiError> {
    let cover = api.release_cover(release_id)?;
    let decoded = std::panic::catch_unwind(|| decode_store_cover(&cover.bytes, side));
    match decoded {
        Ok(Ok(image)) => Ok(Arc::new(image)),
        Ok(Err(error)) => Err(StoreApiError::Malformed(format!("cover not usable ({error})"))),
        Err(_) => Err(StoreApiError::Malformed("cover not usable (decoder failed)".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_of_a_pin_request_hides_the_digits() {
        let request = StoreRequest::SetPin(Pin("483920".into()));
        assert!(!format!("{request:?}").contains("483920"), "{request:?}");
    }
}
