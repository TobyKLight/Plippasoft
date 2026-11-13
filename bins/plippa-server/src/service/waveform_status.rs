//! `GET /api/waveforms/:id` (WAVE_01): per-item analysis state + whole-playable rollup.
//!
//! The registry is a per-process cache. A complete sidecar on disk is authoritative even when
//! the registry is empty (server restart), so every non-analysed item gets one cheap header
//! read before falling back to `queued`. The registry lock is never held across that read, and
//! neither is the service lock: [`ServerService::waveform_plan`] snapshots the items and the
//! registry under the lock, then [`WaveformPlan::resolve`] stats headers after it is released.

use super::{ServerError, ServerService};
use crate::waveform::{item_for, sidecar_is_complete, WaveformSnapshot, WaveformState};
use plippa_noubin::{waveform_path, LibraryError};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;

/// `GET /api/waveforms/:id` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaveformStatus {
    /// Namespaced playable id (as in `/api/playables/:id`).
    pub id: u64,
    /// `all` when every item is analysed; `none` when there are no media items; else `partial`.
    pub state: &'static str,
    pub items: Vec<WaveformItemStatus>,
}

/// One `mediaList` item of the playable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WaveformItemStatus {
    /// 0-based `mediaList` position.
    pub item: u32,
    pub title: String,
    /// Library-relative media path (the registry key).
    pub media_path: String,
    /// `analysed` | `analysing` | `queued` | `failed` | `unavailable`. This is the waveform's
    /// state, kept for older clients; `kinds` is the same answer per analysis kind.
    pub state: &'static str,
    /// One entry per analysis kind this server runs (today: `waveform`). A client reads the
    /// kinds it knows and ignores the rest, so a new kind is an addition here.
    pub kinds: Vec<AnalysisKindStatus>,
}

/// The state of one analysis kind of one media file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AnalysisKindStatus {
    /// `waveform` (later `bpm`, `downbeat`).
    pub kind: &'static str,
    /// Same vocabulary as [`WaveformItemStatus::state`].
    pub state: &'static str,
    /// Malformed frames skipped while analysing (only when analysed in this process and > 0).
    #[serde(skip_serializing_if = "is_zero")]
    pub skipped_frames: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn state_str(state: WaveformState) -> &'static str {
    match state {
        WaveformState::Analysed => "analysed",
        WaveformState::Analysing => "analysing",
        WaveformState::Queued => "queued",
        WaveformState::Failed => "failed",
        WaveformState::Unavailable => "unavailable",
    }
}

/// Everything the endpoint needs from the service lock, captured in one short critical section.
#[derive(Debug, Clone)]
pub struct WaveformPlan {
    id: u64,
    root: PathBuf,
    snapshot: WaveformSnapshot,
    items: Vec<PlannedItem>,
}

#[derive(Debug, Clone)]
struct PlannedItem {
    position: u32,
    title: String,
    media_relpath: String,
    /// `playable_items.available`: the media file exists, so analysis can ever happen.
    available: bool,
}

impl WaveformPlan {
    /// Per-item state; header reads happen here, **after** the service lock is released.
    pub fn resolve(self) -> WaveformStatus {
        let items: Vec<WaveformItemStatus> = self
            .items
            .into_iter()
            .map(|it| {
                let state = if !it.available {
                    // The worker never queues these (`analysis_candidates` is available = 1);
                    // reporting `queued` would promise a waveform that can never arrive.
                    "unavailable"
                } else {
                    item_state(&self.snapshot, &self.root, &it.media_relpath)
                };
                let skipped_frames = if state == "analysed" {
                    self.snapshot
                        .skipped
                        .get(&it.media_relpath)
                        .copied()
                        .unwrap_or(0)
                } else {
                    0
                };
                WaveformItemStatus {
                    item: it.position,
                    title: it.title,
                    media_path: it.media_relpath,
                    state,
                    kinds: vec![AnalysisKindStatus {
                        kind: "waveform",
                        state,
                        skipped_frames,
                    }],
                }
            })
            .collect();
        let state = rollup(&items);
        WaveformStatus {
            id: self.id,
            state,
            items,
        }
    }
}

impl ServerService {
    /// Snapshot the playable's items and the analysis registry, releasing the service lock
    /// before any sidecar header is read. Unknown id → `NotFound` (HTTP 404); no libraries or
    /// vanished storage → `Unavailable` (HTTP 503).
    pub fn waveform_plan(&mut self, id: u64) -> Result<WaveformPlan, ServerError> {
        let (idx, rec) = self.require_record(id)?;
        let root = self.libraries[idx].root.clone();
        let items = self.libraries[idx]
            .writer
            .items_of(rec.id)
            .map_err(lib_err)?
            .into_iter()
            .map(|it| PlannedItem {
                position: it.position,
                title: it.title,
                media_relpath: it.media_relpath,
                available: it.available,
            })
            .collect();
        Ok(WaveformPlan {
            id,
            root,
            snapshot: self.waveforms.snapshot(),
            items,
        })
    }
}

/// Analysis kinds a re-analysis request may name.
pub const REANALYSABLE_KINDS: &[&str] = &["waveform"];

/// Why a re-analysis request was not queued.
#[derive(Debug)]
pub enum ReanalyseRefusal {
    /// The media file is missing (or cannot be analysed at all): re-analysis can never help
    /// (HTTP 409, code `unavailable`).
    Unavailable(String),
    /// Unknown id / position, vanished storage (HTTP 404 / 503).
    Server(ServerError),
}

impl From<ServerError> for ReanalyseRefusal {
    fn from(e: ServerError) -> Self {
        Self::Server(e)
    }
}

/// Check a request body's `kinds` (omitted = all). Unknown names are refused, so a client that
/// asks for a kind this server lacks is told rather than silently getting nothing.
pub fn validate_kinds(kinds: Option<&[String]>) -> Result<(), ServerError> {
    let Some(kinds) = kinds else {
        return Ok(());
    };
    if kinds.is_empty() {
        return Err(ServerError::Invalid(
            "kinds must name at least one analysis kind".into(),
        ));
    }
    for k in kinds {
        if !REANALYSABLE_KINDS.contains(&k.as_str()) {
            return Err(ServerError::Invalid(format!("unknown analysis kind: {k}")));
        }
    }
    Ok(())
}

/// Queue a priority re-analysis of one media item of playable `id`.
///
/// The service lock is held only to resolve the item to `(root, relpath)`; the filesystem
/// checks in [`item_for`] run after it is released, like the status read.
pub fn request_reanalysis(
    service: &Mutex<ServerService>,
    id: u64,
    item: u32,
) -> Result<(), ReanalyseRefusal> {
    let (root, relpath, registry) = {
        let mut svc = service
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (idx, rec) = svc.require_record(id)?;
        let root = svc.libraries[idx].root.clone();
        let items = svc.libraries[idx].writer.items_of(rec.id).map_err(lib_err)?;
        let found = items
            .into_iter()
            .find(|it| it.position == item)
            .ok_or_else(|| ServerError::NotFound(format!("no media item {item} in playable {id}")))?;
        if !found.available {
            return Err(ReanalyseRefusal::Unavailable(
                "the media file is missing".into(),
            ));
        }
        (root, found.media_relpath, svc.waveform_registry())
    };
    let work = item_for(&root, &relpath).ok_or_else(|| {
        ReanalyseRefusal::Unavailable("the media file cannot be analysed".into())
    })?;
    registry.request_reanalysis(work);
    Ok(())
}

/// [`LibraryError::StorageVanished`] is a 503 (the stick was pulled), like `collections.rs`.
fn lib_err(e: LibraryError) -> ServerError {
    match e {
        LibraryError::StorageVanished { path, detail } => ServerError::Unavailable(format!(
            "Media storage vanished at {}: {detail}",
            path.display()
        )),
        other => other.into(),
    }
}

/// Whole-playable rollup: `all` / `none` (no media items) / `partial`.
fn rollup(items: &[WaveformItemStatus]) -> &'static str {
    if items.is_empty() {
        "none"
    } else if items.iter().all(|i| i.state == "analysed") {
        "all"
    } else {
        "partial"
    }
}

/// Registry first (mutually exclusive there), then the on-disk header, then `queued`.
fn item_state(snapshot: &WaveformSnapshot, root: &std::path::Path, relpath: &str) -> &'static str {
    let state = snapshot.state_of(relpath);
    if state != WaveformState::Queued {
        return state_str(state);
    }
    // A re-analysis in the queue: the complete sidecar on disk is the one being replaced.
    if snapshot.requested.contains(relpath) {
        return "queued";
    }
    if sidecar_is_complete(&waveform_path(root, relpath)) {
        "analysed"
    } else {
        "queued"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(state: &'static str) -> WaveformItemStatus {
        WaveformItemStatus {
            item: 0,
            title: String::new(),
            media_path: String::new(),
            state,
            kinds: Vec::new(),
        }
    }

    #[test]
    fn waveform_rollup_is_all_none_or_partial() {
        assert_eq!(rollup(&[]), "none");
        assert_eq!(rollup(&[item("analysed"), item("analysed")]), "all");
        assert_eq!(rollup(&[item("analysed"), item("queued")]), "partial");
        assert_eq!(rollup(&[item("analysing")]), "partial");
        assert_eq!(rollup(&[item("failed")]), "partial");
        assert_eq!(rollup(&[item("unavailable")]), "partial");
        assert_eq!(rollup(&[item("analysed"), item("unavailable")]), "partial");
    }

    #[test]
    fn storage_vanished_maps_to_503() {
        let err = lib_err(LibraryError::StorageVanished {
            path: PathBuf::from("/media/stick"),
            detail: "unmounted".into(),
        });
        assert!(matches!(err, ServerError::Unavailable(_)), "{err:?}");
        // Other library errors keep their own mapping (not 503).
        let err = lib_err(LibraryError::MissingRoot {
            path: PathBuf::from("/media/none"),
        });
        assert!(!matches!(err, ServerError::Unavailable(_)));
    }
}
