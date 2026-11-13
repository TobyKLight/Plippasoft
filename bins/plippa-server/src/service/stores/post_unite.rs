//! BCS_05: the refined download -> unite step.
//!
//! A finished store download is not a library item yet. This module turns the staged files
//! into one, and it is the seam BCS_07 calls after a tap:
//!
//! * **Path A** (a Noubin URL/key is known): the canonical `web.noudata` is fetched through
//!   the shared, consent-gated web machinery ([`WebHub`]), the store's track order is mapped
//!   onto its `mediaList`, every measured duration is compared with
//!   `uniteData.expectedLength`, and the files are united against that authoritative
//!   document.
//! * **Path B** (browsing the collection, no URL): the release document is built from the
//!   store's own metadata — the *release* title (never `song.album`, which this store family
//!   sometimes fills with the artist name), a date only when the store gave one, and nothing
//!   invented.
//!
//! Both paths then use the same planner ([`build_local_doc`]) and writer
//! ([`write_store_release`]) as every other unite, so placement, collision handling, cover
//! naming and the `.noudata`-written-last rule are the ones NB_09 proved.
//!
//! Three rules run through the module:
//!
//! * **Never tag the audio.** The sidecar is the metadata record; not one byte of a staged
//!   file is rewritten.
//! * **Never claim success silently.** A duration disagreement beyond the tolerance (2 s by
//!   default) needs the person's confirmation, and a lossy substitute is reported per track.
//! * **Idempotent.** A release this store already produced is found by a disk-only scan;
//!   without `replace` the answer is "already in the library" and **no request is made and
//!   nothing is written**.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use plippa_noubin::unite::{
    build_local_doc, ConfirmedItem, ConfirmedMapping, Placement, UniteError as MatchError,
};
use plippa_noubin::{
    collect_noudata_paths, normalise_noubin_key, parse_noudata, read_noudata_capped,
    sanitise_key, sanitise_line, sanitise_url, Credits, LibraryRoot, MediaListItem,
    NoudataDocument, NoudataType, NoubinTimestamp, PlayableItemProfile, PrimaryArtist,
    ReleaseData, UniteData, MIN_LIBRARY_SCAN_DEPTH,
};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::unite::fsops::LibraryFs;
use crate::unite::hub::{write_store_release, LocalArt, StoreWrite, SOURCE_URL_KEY};
use crate::unite::images::plan_cover_targets;
use crate::unite::sniff::sniff_image;
use crate::web::download::{
    download_to_temp, Budget, Cancel, DownloadError, DownloadLimits, Want, MAX_IMAGE_SIDE,
};
use crate::web::fetch::Consent;
use crate::web::{HeldRelease, WebHub};

use super::browse::CoverImage as StoreCover;
use super::download::{
    common_stage, HandOffState, HandedOff, StagedRelease, StagedStoreDownload, StagedTrackFile,
};
use super::{log_line, LibraryDeleter, LibraryIndexer, StoreApiError, StoreHub, STORE_SLUG};

/// The command the portal is told will place the staged files (BCS_05's refined entry).
pub const POST_UNITE_ENTRY_POINT: &str = "service::stores::post_unite::post_download_unite";
/// Default per-track duration tolerance (research §9 uses the same two seconds).
pub const DEFAULT_DURATION_TOLERANCE_MS: u64 = 2_000;
/// A tolerance above this would accept any file as the right track.
const MAX_DURATION_TOLERANCE_MS: u64 = 60_000;
/// Most `.noudata` files the idempotence scan reads (the same bound BCS_06 uses).
const MAX_LIBRARY_DOCS_SCANNED: usize = 5_000;
/// Longest title/artist/name taken from the store into a sidecar (NBSecurityRequirements §3).
const MAX_STORE_TEXT: usize = 256;
/// Root property recording this store download's provenance (a Plippa extension).
pub const STORE_SOURCE_KEY: &str = "plippaStoreSource";
/// The store slug recorded as `provider`/`store`.
const PROVIDER: &str = STORE_SLUG;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// BCS_05's request: the staged download plus what the caller knows about it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PostDownloadUnite {
    /// The staged files the download worker produced (validated here).
    pub download: StagedStoreDownload,
    /// A Noubin URL/key for the release. Absent = Path B (store metadata only).
    #[serde(default)]
    pub noubin_key: Option<String>,
    /// The person agreed to fetch the Noubin document (Path A; checked before a socket).
    #[serde(default)]
    pub consent: bool,
    /// Replace an item this store release already produced (`BCS_10` delete, then re-add).
    #[serde(default)]
    pub replace: bool,
    /// The person saw the proposal and accepts a count/duration disagreement.
    #[serde(default)]
    pub confirm_mismatch: bool,
    /// Per-track duration tolerance; `None` uses [`DEFAULT_DURATION_TOLERANCE_MS`].
    #[serde(default)]
    pub duration_tolerance_ms: Option<u64>,
}

impl PostDownloadUnite {
    /// A Path B request for the download worker (no URL, no replace, no confirmation).
    pub fn store_metadata_only(download: StagedStoreDownload) -> Self {
        Self {
            download,
            noubin_key: None,
            consent: false,
            replace: false,
            confirm_mismatch: false,
            duration_tolerance_ms: None,
        }
    }

}

/// Which metadata source a release was united against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UnitePath {
    /// The fetched `web.noudata` (authoritative).
    A,
    /// The store's own metadata.
    B,
}

impl UnitePath {
    fn as_metadata(self) -> &'static str {
        match self {
            UnitePath::A => "noudata",
            UnitePath::B => "store",
        }
    }
}

/// What the unite step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UniteState {
    /// Written into the library and (when an indexer is installed) indexed.
    Placed,
    /// This store release is already in the library; nothing was fetched or written.
    Exists,
    /// The proposal disagrees with the document (counts or durations); nothing was written.
    NeedsConfirmation,
    /// Another library item already carries the requested Noubin key.
    KeyInUse,
    /// Nothing usable was staged, or the write failed.
    Rejected,
}

/// How a measured duration compared with `uniteData.expectedLength`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DurationMatch {
    Exact,
    WithinTolerance,
    Mismatch,
}

/// Whether the file that arrived is the file the person bought.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackQuality {
    Original,
    LossySubstitute,
}

/// Per-release outcome, for deciding the aggregate state (not serialised).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Placed,
    Exists,
    NeedsConfirmation,
    Rejected,
}

/// One downloaded track and what the document said about it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UniteTrack {
    pub track_id: String,
    pub title: String,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    /// Position in the document's `mediaList` (null when nothing was matched).
    pub item_index: Option<usize>,
    /// Duration measured from the bytes actually written (null when it could not be read).
    pub duration_ms: Option<u64>,
    /// `mediaList[itemIndex].uniteData.expectedLength`.
    pub expected_duration_ms: Option<u64>,
    /// Null when either duration is unknown: a match is never claimed unmeasured.
    pub duration_match: Option<DurationMatch>,
    pub quality: TrackQuality,
    pub bytes: u64,
}

/// One release of the request.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UniteReleaseResult {
    pub release_id: String,
    pub title: String,
    pub artist: String,
    /// The metadata source this release used (null when nothing was written).
    pub path: Option<UnitePath>,
    /// Library-relative `.noudata` path, when one was written (or already existed).
    pub noudata: Option<String>,
    /// Playable id the index assigned, when indexing succeeded.
    pub item_id: Option<u64>,
    /// The index saw the new item (false: the periodic scan will).
    pub indexed: bool,
    pub track_count: usize,
    pub total_bytes: u64,
    pub tracks: Vec<UniteTrack>,
    pub warnings: Vec<String>,
    pub detail: String,
    /// A previous copy of this store release exists and can be replaced.
    pub can_replace: bool,
    #[serde(skip)]
    outcome: Outcome,
}

/// BCS_05's answer: one entry per release, plus the aggregate state.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UniteResult {
    pub state: UniteState,
    pub entry_point: &'static str,
    pub detail: String,
    /// The metadata source (null for a multi-release Path B request or a rejection).
    pub path: Option<UnitePath>,
    // Repeated from the single-release entry for the portal and the worker.
    pub noudata: Option<String>,
    pub noudata_paths: Vec<String>,
    pub item_ids: Vec<u64>,
    pub indexed: bool,
    pub can_replace: bool,
    pub track_count: usize,
    pub total_bytes: u64,
    pub releases: Vec<UniteReleaseResult>,
    pub warnings: Vec<String>,
}

impl UniteResult {
    fn rejected(detail: impl Into<String>) -> Self {
        Self {
            state: UniteState::Rejected,
            entry_point: POST_UNITE_ENTRY_POINT,
            detail: detail.into(),
            path: None,
            noudata: None,
            noudata_paths: Vec::new(),
            item_ids: Vec::new(),
            indexed: false,
            can_replace: false,
            track_count: 0,
            total_bytes: 0,
            releases: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn no_release(release: &StagedRelease, detail: impl Into<String>, outcome: Outcome) -> UniteReleaseResult {
        UniteReleaseResult {
            release_id: release.release_id.clone(),
            title: release.title.clone(),
            artist: release.artist.clone(),
            path: None,
            noudata: None,
            item_id: None,
            indexed: false,
            track_count: release.tracks.len(),
            total_bytes: release.tracks.iter().map(|t| t.bytes).sum(),
            tracks: release.tracks.iter().map(track_report).collect(),
            warnings: Vec::new(),
            detail: detail.into(),
            can_replace: false,
            outcome,
        }
    }

    /// BCS_04's older shape over the same outcome (the `hand_off_to_unite` wrapper).
    pub(crate) fn to_handed_off(&self) -> HandedOff {
        // "In the library now" covers both a fresh write and a replay that found the copy
        // already there; the state keeps them apart so no report claims a write that did
        // not happen.
        let present = matches!(self.state, UniteState::Placed | UniteState::Exists);
        HandedOff {
            state: match self.state {
                UniteState::Placed => HandOffState::Placed,
                UniteState::Exists => HandOffState::Exists,
                _ => HandOffState::Rejected,
            },
            entry_point: POST_UNITE_ENTRY_POINT,
            detail: self.detail.clone(),
            noudata: self.noudata.clone(),
            noudata_paths: self.noudata_paths.clone(),
            indexed: self.indexed,
            metadata: self
                .path
                .filter(|_| present)
                .map(|path| path.as_metadata().to_string()),
            can_link: present && self.path == Some(UnitePath::B),
        }
    }
}

// ---------------------------------------------------------------------------
// Services the entry point needs (installed on the hub at start-up)
// ---------------------------------------------------------------------------

pub(crate) struct UniteServices<'a> {
    /// The shared web hub (consent-gated fetching, the same one NB_09 uses).
    pub web: Option<&'a WebHub>,
    /// The store hub, for the `store.cover_art` fallback.
    pub store: Option<&'a StoreHub>,
    /// BCS_10's delete, by library-relative `.noudata` path.
    pub delete: Option<&'a LibraryDeleter>,
    /// The write-through index update.
    pub index: Option<&'a LibraryIndexer>,
    pub limits: DownloadLimits,
}

impl StoreHub {
    /// **The refined entry point** (BCS_05): place a finished download in the library.
    ///
    /// BCS_07 calls this after a tap with `noubin_key`/`consent` set; the download worker
    /// calls it with no key (Path B). Nothing here holds the server lock across the network:
    /// the web hub, the delete and the indexer are the services installed at start-up.
    pub fn post_download_unite(&self, req: PostDownloadUnite) -> UniteResult {
        let Some(root) = self.library_root() else {
            return UniteResult::rejected("the media library root is not available");
        };
        let web = lock_clone(&self.web);
        let deleter = lock_clone(&self.deleter);
        let indexer = lock_clone(&self.indexer);
        let services = UniteServices {
            web: web.as_deref(),
            store: Some(self),
            delete: deleter.as_ref(),
            index: indexer.as_ref(),
            limits: self.limits(),
        };
        unite_download(&root, req, services)
    }
}

fn lock_clone<T: Clone>(slot: &std::sync::Mutex<Option<T>>) -> Option<T> {
    slot.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/// Place one store download in the library.
///
/// The staged paths come from the download worker, never from a request body; they are
/// validated here before anything is read or written.
pub(crate) fn unite_download(
    root: &Path,
    req: PostDownloadUnite,
    services: UniteServices<'_>,
) -> UniteResult {
    let PostDownloadUnite {
        download,
        ref noubin_key,
        consent,
        replace,
        confirm_mismatch,
        duration_tolerance_ms,
    } = req;
    let tolerance = duration_tolerance_ms
        .unwrap_or(DEFAULT_DURATION_TOLERANCE_MS)
        .min(MAX_DURATION_TOLERANCE_MS);
    let noubin_key = noubin_key.as_deref();

    if download.releases.iter().any(|release| {
        release.release_id.trim().is_empty() || release.title.trim().is_empty()
    }) {
        return UniteResult::rejected("a staged release has no id or title");
    }

    let key = match noubin_key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(raw) => match sanitise_key(raw) {
            Some(key) => Some(key),
            None => return UniteResult::rejected("the Noubin key is not valid"),
        },
        None => None,
    };
    let path_a = key.is_some();
    if path_a && download.releases.len() != 1 {
        return UniteResult::rejected(
            "a Noubin URL can only be united with one release; download that release on its own",
        );
    }
    if path_a && !consent {
        return UniteResult::rejected(
            "consent is required before the linked Noubin release can be fetched",
        );
    }
    if path_a && services.web.is_none() {
        return UniteResult::rejected(
            "the online lookup is not available, so the linked release cannot be fetched",
        );
    }

    // Disk only: a second tap of the same release answers without any request. This runs
    // before the staged files are read so a hand-off may name a release the worker did not
    // fetch because it is already in the library.
    let release_ids: Vec<String> = download
        .releases
        .iter()
        .map(|release| release.release_id.clone())
        .collect();
    let scan = find_existing(root, &release_ids, key.as_deref());
    let all_present = !download.releases.is_empty()
        && !replace
        && download
            .releases
            .iter()
            .all(|release| scan.existing_for(&release.release_id).is_some());
    let staged_tracks: usize = download.releases.iter().map(|r| r.tracks.len()).sum();
    if staged_tracks == 0 && !all_present {
        return UniteResult::rejected("no staged track to unite");
    }
    if !all_present {
        for release in &download.releases {
            for track in &release.tracks {
                match fs::metadata(&track.path) {
                    Ok(meta) if meta.is_file() && meta.len() > 0 => {}
                    _ => {
                        return UniteResult::rejected(format!(
                            "the staged file for {:?} is missing or empty",
                            track.title
                        ))
                    }
                }
            }
        }
    }
    let stage = if staged_tracks == 0 {
        // Every release answers from disk; no staged path is read.
        root.to_path_buf()
    } else {
        match common_stage(&download) {
            Some(stage) => stage,
            None => {
                return UniteResult::rejected("the staged files are not in one staging folder")
            }
        }
    };

    if let Some(owner) = &scan.key_owner {
        let ours = download
            .releases
            .iter()
            .any(|r| owner.release_id.as_deref() == Some(r.release_id.as_str()));
        if !ours {
            let releases = download
                .releases
                .iter()
                .map(|r| {
                    UniteResult::no_release(
                        r,
                        "another item in the library already carries this Noubin key",
                        Outcome::Rejected,
                    )
                })
                .collect();
            return UniteResult {
                state: UniteState::KeyInUse,
                releases,
                ..UniteResult::rejected(
                    "another item in the library already carries this Noubin key",
                )
            };
        }
    }

    // Fetch the authoritative document before anything is deleted, so a network failure
    // never costs the copy already in the library. A replay of a release that is already
    // in the library answers from disk: no request is made until a replace is asked for.
    let need_document = key.is_some()
        && (replace
            || download
                .releases
                .iter()
                .any(|r| scan.existing_for(&r.release_id).is_none()));
    // When the document is not fetched, every release has an existing item and `replace`
    // is off, so `unite_one_release` answers `Exists` before it ever looks at `held`.
    let held = if need_document {
        let web = services.web.expect("checked above");
        let key = key.as_deref().expect("a document is only fetched with a key");
        match fetch_document(web, key) {
            Ok(held) => Some(held),
            Err(error) => {
                return UniteResult {
                    state: UniteState::Rejected,
                    detail: format!("the linked Noubin release could not be fetched: {error}"),
                    ..UniteResult::rejected("")
                }
            }
        }
    } else {
        None
    };

    let mut results: Vec<UniteReleaseResult> = Vec::with_capacity(download.releases.len());
    for release in &download.releases {
        let existing = scan.existing_for(&release.release_id);
        let result = unite_one_release(
            root,
            &stage,
            &download.session,
            release,
            held.as_ref(),
            existing,
            key.as_deref(),
            tolerance,
            replace,
            confirm_mismatch,
            &services,
        );
        results.push(result);
    }

    aggregate(results)
}

fn aggregate(releases: Vec<UniteReleaseResult>) -> UniteResult {
    let state = if releases.iter().any(|r| r.outcome == Outcome::NeedsConfirmation) {
        UniteState::NeedsConfirmation
    } else if releases.iter().all(|r| r.outcome == Outcome::Exists) {
        UniteState::Exists
    } else if releases.iter().any(|r| r.outcome == Outcome::Placed) {
        // A selection that placed at least one release is a placement; a release that was
        // skipped (already there) or refused is reported in the warnings below, never as
        // "nothing was added".
        UniteState::Placed
    } else {
        UniteState::Rejected
    };
    let placed: Vec<&UniteReleaseResult> = releases
        .iter()
        .filter(|r| r.outcome == Outcome::Placed)
        .collect();
    // An item that is already in the library still has a path worth reporting.
    let located: Vec<&UniteReleaseResult> = releases
        .iter()
        .filter(|r| r.noudata.is_some())
        .collect();
    let noudata_paths: Vec<String> = located.iter().filter_map(|r| r.noudata.clone()).collect();
    let item_ids: Vec<u64> = placed.iter().filter_map(|r| r.item_id).collect();
    let indexed = if placed.is_empty() {
        // Nothing was written: an existing item is already in the index.
        !located.is_empty()
    } else {
        placed.iter().all(|r| r.indexed)
    };
    let track_count: usize = placed.iter().map(|r| r.track_count).sum();
    let total_bytes: u64 = placed.iter().map(|r| r.total_bytes).sum();
    let path = if releases.len() == 1 {
        releases[0].path
    } else {
        None
    };
    let mut warnings: Vec<String> = releases.iter().flat_map(|r| r.warnings.clone()).collect();
    if state == UniteState::Placed {
        // Say what did not go in, so a partly placed selection is never silent about it.
        for release in &releases {
            if release.outcome != Outcome::Placed {
                warnings.push(format!("{}: {}", release.title, release.detail));
            }
        }
    }
    let detail = match state {
        UniteState::Placed => format!(
            "{} track(s) added to the library from {} release(s)",
            track_count,
            placed.len()
        ),
        UniteState::Rejected => releases
            .iter()
            .find(|r| r.outcome == Outcome::Rejected)
            .map(|r| r.detail.clone())
            .unwrap_or_else(|| "nothing was added to the library".to_string()),
        other => state_detail(other),
    };
    UniteResult {
        state,
        entry_point: POST_UNITE_ENTRY_POINT,
        detail,
        path,
        noudata: noudata_paths.first().cloned(),
        noudata_paths,
        item_ids,
        indexed,
        can_replace: releases.iter().any(|r| r.can_replace),
        track_count,
        total_bytes,
        releases,
        warnings,
    }
}

/// The one-line sentence for a state that has no counts to report.
fn state_detail(state: UniteState) -> String {
    match state {
        UniteState::Placed => "the release was added to the library".to_string(),
        UniteState::Exists => {
            "this store release is already in the library; replace it to re-add it".to_string()
        }
        UniteState::NeedsConfirmation => {
            "the download does not match the linked release closely enough; confirm it to \
             unite anyway"
                .to_string()
        }
        UniteState::KeyInUse => {
            "another item in the library already carries this Noubin key".to_string()
        }
        UniteState::Rejected => "nothing was added to the library".to_string(),
    }
}

// ---------------------------------------------------------------------------
// One release
// ---------------------------------------------------------------------------

/// Which item of the document each staged track belongs to (deterministic).
///
/// The store knows disc/track order, so when the counts agree the mapping is positional.
/// When they do not (a partial selection), titles are matched and the caller is asked to
/// confirm.
fn map_items(
    doc: &NoudataDocument,
    tracks: &[StagedTrackFile],
    order: &[usize],
) -> (Vec<Option<usize>>, bool) {
    let items = &doc.media_list;
    let mut mapping: Vec<Option<usize>> = vec![None; tracks.len()];
    if items.len() == tracks.len() {
        for (position, &staged) in order.iter().enumerate() {
            mapping[staged] = Some(position);
        }
        return (mapping, true);
    }
    let mut used: HashSet<usize> = HashSet::new();
    for &staged in order {
        let title = fold_title(&tracks[staged].title);
        if title.is_empty() {
            continue;
        }
        let found = items.iter().enumerate().find_map(|(index, item)| {
            if used.contains(&index) {
                return None;
            }
            let item_title = fold_title(item.media_title.as_deref().unwrap_or(""));
            (!item_title.is_empty() && item_title == title).then_some(index)
        });
        if let Some(index) = found {
            used.insert(index);
            mapping[staged] = Some(index);
        }
    }
    (mapping, false)
}

/// Tolerant title comparison for mapping a staged track onto a document item.
///
/// Deliberately delegated to the matcher's one normalisation helper (lowercase, diacritics
/// folded, apostrophes dropped, `&` → `and`, punctuation collapsed, leading `The` dropped)
/// rather than keeping a second fold here: two normalisers drift, and a title that matches
/// in one place must match in the other.
fn fold_title(input: &str) -> String {
    super::matching::normalise_title(input)
}

#[allow(clippy::too_many_arguments)]
fn unite_one_release(
    root: &Path,
    stage: &Path,
    session: &str,
    release: &StagedRelease,
    held: Option<&HeldRelease>,
    existing: Option<String>,
    key: Option<&str>,
    tolerance_ms: u64,
    replace: bool,
    confirm_mismatch: bool,
    services: &UniteServices<'_>,
) -> UniteReleaseResult {
    if let Some(rel) = existing.as_deref() {
        if !replace {
            return UniteReleaseResult {
                noudata: Some(rel.to_string()),
                indexed: true,
                can_replace: true,
                outcome: Outcome::Exists,
                ..UniteResult::no_release(
                    release,
                    "this store release is already in the library; replace it to re-add it",
                    Outcome::Exists,
                )
            };
        }
        let Some(delete) = services.delete else {
            return UniteResult::no_release(
                release,
                "replacing an item needs the library index, which is not available",
                Outcome::Rejected,
            );
        };
        match delete(root, rel) {
            Ok(id) => log_line(&format!(
                "store download replacing playable {id} ({rel})"
            )),
            Err(error) => {
                return UniteResult::no_release(
                    release,
                    format!("the previous copy could not be deleted: {error}"),
                    Outcome::Rejected,
                )
            }
        }
    }

    let order = track_order(release);
    let (doc, path, matched, counts_agree) = match held {
        Some(web) => {
            let (mapping, counts_agree) = map_items(&web.doc, &release.tracks, &order);
            (web.doc.clone(), UnitePath::A, mapping, counts_agree)
        }
        None => {
            let doc = store_release_document(release, &order);
            (
                doc,
                UnitePath::B,
                (0..release.tracks.len()).map(Some).collect::<Vec<_>>(),
                true,
            )
        }
    };

    // Per-track honesty: the measured duration against the document's expectation.
    let mut tracks: Vec<UniteTrack> = Vec::with_capacity(release.tracks.len());
    let mut mismatches: Vec<String> = Vec::new();
    let mut unmeasured: Vec<String> = Vec::new();
    for (index, track) in release.tracks.iter().enumerate() {
        let item_index = matched.get(index).copied().flatten();
        let expected = item_index
            .and_then(|i| doc.media_list.get(i))
            .and_then(|item| item.unite_data.as_ref())
            .and_then(|data| data.expected_length)
            .map(NoubinTimestamp::as_millis);
        let measured = track.duration_measured.then_some(track.duration_ms).flatten();
        let (duration_match, mismatch, unread) = compare_duration(measured, expected, tolerance_ms);
        if let Some(note) = mismatch {
            mismatches.push(note);
        }
        if let Some(note) = unread {
            unmeasured.push(note);
        }
        tracks.push(UniteTrack {
            track_id: track.track_id.clone(),
            title: track.title.clone(),
            track_number: track.track_number,
            disc_number: track.disc_number,
            item_index,
            duration_ms: measured,
            expected_duration_ms: expected,
            duration_match,
            quality: quality_of(track),
            bytes: track.bytes,
        });
    }

    let mut warnings = Vec::new();
    warnings.extend(unmeasured);
    warnings.extend(mismatches.iter().cloned());
    let substituted = tracks
        .iter()
        .filter(|t| t.quality == TrackQuality::LossySubstitute)
        .count();
    if substituted > 0 {
        warnings.push(format!(
            "{substituted} track(s) are the store's lossy stream, not the purchased file"
        ));
    }
    if !counts_agree {
        warnings.push(format!(
            "the linked release lists {} track(s) but {} arrived; the match is by title",
            doc.media_list.len(),
            release.tracks.len()
        ));
    }
    let unmapped = matched.iter().filter(|m| m.is_none()).count();
    if unmapped > 0 && held.is_some() {
        warnings.push(format!(
            "{unmapped} downloaded track(s) could not be matched to the linked release"
        ));
    }

    let needs_confirmation = held.is_some()
        && (!counts_agree || !mismatches.is_empty() || unmapped > 0)
        && !confirm_mismatch;
    if needs_confirmation {
        return UniteReleaseResult {
            tracks,
            warnings,
            outcome: Outcome::NeedsConfirmation,
            ..UniteResult::no_release(
                release,
                "the download does not match the linked release closely enough; confirm it to unite anyway",
                Outcome::NeedsConfirmation,
            )
        };
    }

    // Cover art: the linked document's images first, the store's own cover as a candidate
    // (so the larger one can be the primary), and the store cover alone for Path B.
    let mut art: Vec<LocalArt> = Vec::new();
    let mut art_warnings = Vec::new();
    if let Some(web) = services.web {
        if held.is_some() {
            download_document_art(
                web,
                &services.limits,
                stage,
                &doc,
                &mut art,
                &mut art_warnings,
            );
        }
    }
    if let Some(store) = services.store {
        if let Some(cover) = store_cover(store, &release.release_id, stage, &mut art_warnings) {
            art.push(cover);
        }
    }
    warnings.extend(art_warnings);

    // The local document, through the same planner every unite uses.
    let mut assignments: Vec<ConfirmedItem> = Vec::with_capacity(matched.len());
    for (index, item) in matched.iter().enumerate() {
        let Some(item_index) = *item else { continue };
        if assignments.iter().any(|a| a.item_index == item_index) {
            continue;
        }
        assignments.push(ConfirmedItem {
            item_index,
            files: vec![release.tracks[index].path.clone()],
        });
    }
    let mapping = ConfirmedMapping {
        assignments,
        known_files: release.tracks.iter().map(|t| t.path.clone()).collect(),
        ignored_files: Vec::new(),
        allow_incomplete: confirm_mismatch && held.is_some(),
    };
    let fs = match LibraryFs::new(root) {
        Ok(fs) => fs,
        Err(error) => {
            return UniteReleaseResult {
                tracks,
                warnings,
                outcome: Outcome::Rejected,
                ..UniteResult::no_release(
                    release,
                    format!("rejected: the library could not be opened: {error}"),
                    Outcome::Rejected,
                )
            }
        }
    };
    let placement = Placement::for_release(&doc, doc.profile());
    let existing_names = fs.existing_names(&placement.dir);
    let placement = placement.with_existing_names(existing_names);
    let plan = match build_local_doc(&doc, &mapping, key, &placement) {
        Ok(plan) => plan,
        Err(error) => {
            let reason = match error {
                MatchError::Incomplete {
                    missing_items,
                    leftover_files,
                } => format!(
                    "{} item(s) have no file and {} file(s) are not assigned",
                    missing_items.len(),
                    leftover_files.len()
                ),
                other => other.to_string(),
            };
            return UniteReleaseResult {
                tracks,
                warnings,
                outcome: Outcome::Rejected,
                ..UniteResult::no_release(
                    release,
                    format!("rejected: {reason}"),
                    Outcome::Rejected,
                )
            };
        }
    };
    let media: Vec<(PathBuf, PathBuf)> = plan
        .file_ops
        .iter()
        .map(|op| (op.source.clone(), op.target.clone()))
        .collect();
    let mut local = plan.doc;
    // Prefer the address the caller supplied over the resolved `web.noudata` one: it is the
    // address a person would recognise, and it is what a rescan or BCS_06 will match on.
    let supplied_url = key.or_else(|| held.map(|h| h.source_url.as_str()));
    local.extra.insert(
        STORE_SOURCE_KEY.to_string(),
        provenance(release, path, supplied_url),
    );
    if let Some(url) = held
        .map(|h| h.source_url.as_str())
        .and_then(sanitise_url)
    {
        local
            .extra
            .insert(SOURCE_URL_KEY.to_string(), Value::String(url));
    }
    let noudata_path = plan.noudata_path;
    match write_store_release(
        root,
        stage,
        StoreWrite {
            doc: local,
            noudata: noudata_path,
            media,
            art,
        },
    ) {
        Ok(abs) => {
            let rel = relative_posix(root, &abs);
            let mut indexed = false;
            let mut item_id = None;
            if let Some(index) = services.index {
                match index(root, &abs) {
                    Ok(id) => {
                        indexed = true;
                        item_id = Some(id);
                    }
                    Err(error) => warnings.push(format!(
                        "the release is in the library but the index has not picked it up yet: {error}"
                    )),
                }
            }
            log_line(&format!(
                "store download placed session {session}: {} track(s) from release {} ({rel})",
                release.tracks.len(),
                sanitise_line(&release.release_id, 64),
            ));
            UniteReleaseResult {
                path: Some(path),
                noudata: Some(rel),
                item_id,
                indexed,
                tracks,
                warnings,
                detail: match path {
                    UnitePath::A => "united against the linked Noubin release".to_string(),
                    UnitePath::B => {
                        "metadata from Bandcamp (no Noubin release linked)".to_string()
                    }
                },
                // Nothing existed to replace: a *later* run reports `Exists` and offers it.
                can_replace: false,
                outcome: Outcome::Placed,
                ..UniteResult::no_release(release, "", Outcome::Placed)
            }
        }
        Err(error) => UniteReleaseResult {
            tracks,
            warnings,
            outcome: Outcome::Rejected,
            ..UniteResult::no_release(
                release,
                format!("rejected: the downloaded files could not be added: {error}"),
                Outcome::Rejected,
            )
        },
    }
}

// ---------------------------------------------------------------------------
// Path B: the document the store's metadata can honestly produce
// ---------------------------------------------------------------------------

/// Build a release document from the store's own metadata.
///
/// The title is the **release** title (the store release's name), never a track's `album`
/// field: on this store family that field can hold the artist name (research §7). Nothing is
/// invented: no date unless the store gave a creation date, no artist unless it gave one.
fn store_release_document(release: &StagedRelease, order: &[usize]) -> NoudataDocument {
    // The store is untrusted metadata (NBSecurityRequirements §3): strip control/bidi
    // characters and cap lengths before the values become a sidecar and a screen title.
    let title = sanitise_line(release.title.trim(), MAX_STORE_TEXT);
    let artist = sanitise_line(release.artist.trim(), MAX_STORE_TEXT);
    let mut credits = Credits::default();
    if !artist.is_empty() {
        credits.primary_artists = Some(vec![PrimaryArtist {
            primary_artist_name: artist.clone(),
            extra: Map::new(),
        }]);
    }
    if let Some(date) = release.created_unix.and_then(release_date) {
        // `releaseDate` lives under `credits` in the standard (v0.4.0).
        credits
            .extra
            .insert("releaseDate".to_string(), Value::String(date));
    }
    let media_list = order
        .iter()
        .map(|&index| {
            let track = &release.tracks[index];
            MediaListItem {
                media_title: Some(sanitise_line(&track.title, MAX_STORE_TEXT))
                    .filter(|t| !t.trim().is_empty()),
                unite_data: track.duration_ms.map(|ms| UniteData {
                    expected_length: Some(NoubinTimestamp::from_millis(ms)),
                    ..UniteData::default()
                }),
                ..MediaListItem::default()
            }
        })
        .collect();
    NoudataDocument {
        noudata_type: Some(NoudataType::Release),
        playable_item_title: Some(if title.is_empty() {
            "Untitled".to_string()
        } else {
            title.to_string()
        }),
        playable_item_profile: Some(PlayableItemProfile::Music),
        release_data: Some(ReleaseData {
            credits: (credits.primary_artists.is_some() || !credits.extra.is_empty())
                .then_some(credits),
            ..ReleaseData::default()
        }),
        media_list,
        ..NoudataDocument::default()
    }
}

/// `YYYY-MM-DD` (UTC) for a Unix timestamp; `None` when the value is not a plausible date.
fn release_date(unix: i64) -> Option<String> {
    // 1970-01-01 .. 2200-01-01. A store date outside that is not a release date.
    if !(0..=7_258_118_400).contains(&unix) {
        return None;
    }
    let days = unix.div_euclid(86_400);
    // Howard Hinnant's civil-from-days (the inverse of the days-from-civil a store uses).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// Provenance recorded as a root extra (a Plippa extension, preserved by unknown-field
/// handling). `store` is the key BCS_06's matcher reads; `provider` is the documented name.
fn provenance(release: &StagedRelease, path: UnitePath, url: Option<&str>) -> Value {
    let mut map = Map::new();
    map.insert("provider".to_string(), Value::String(PROVIDER.to_string()));
    map.insert("store".to_string(), Value::String(PROVIDER.to_string()));
    map.insert(
        "releaseId".to_string(),
        Value::String(release.release_id.clone()),
    );
    map.insert(
        "trackIds".to_string(),
        Value::Array(
            release
                .tracks
                .iter()
                .map(|t| Value::String(t.track_id.clone()))
                .collect(),
        ),
    );
    map.insert(
        "metadata".to_string(),
        Value::String(path.as_metadata().to_string()),
    );
    if let Some(year) = release.year {
        map.insert("year".to_string(), Value::from(year));
    }
    if let Some(url) = url.and_then(sanitise_url) {
        map.insert("url".to_string(), Value::String(url));
    }
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// Matching and reporting helpers
// ---------------------------------------------------------------------------

/// Staged tracks in disc/track order (the order the document lists them in).
fn track_order(release: &StagedRelease) -> Vec<usize> {
    let mut order: Vec<usize> = (0..release.tracks.len()).collect();
    order.sort_by_key(|&index| {
        let track = &release.tracks[index];
        (
            track.disc_number.unwrap_or(1),
            track.track_number.unwrap_or(u32::MAX),
            index,
        )
    });
    order
}

/// `(status, mismatch note, unmeasured note)`; a match is never claimed unmeasured.
fn compare_duration(
    measured: Option<u64>,
    expected: Option<u64>,
    tolerance_ms: u64,
) -> (Option<DurationMatch>, Option<String>, Option<String>) {
    let (Some(measured), Some(expected)) = (measured, expected) else {
        return (None, None, None);
    };
    let diff = measured.abs_diff(expected);
    if diff == 0 {
        return (Some(DurationMatch::Exact), None, None);
    }
    if diff <= tolerance_ms {
        return (Some(DurationMatch::WithinTolerance), None, None);
    }
    (
        Some(DurationMatch::Mismatch),
        Some(format!(
            "a track was expected to be {} but the file measures {} (off by {})",
            seconds(expected),
            seconds(measured),
            seconds(diff)
        )),
        None,
    )
}

fn seconds(ms: u64) -> String {
    format!("{}.{:01}s", ms / 1000, (ms % 1000) / 100)
}

fn quality_of(track: &StagedTrackFile) -> TrackQuality {
    if track.original && !track.stream_substituted {
        TrackQuality::Original
    } else {
        TrackQuality::LossySubstitute
    }
}

fn track_report(track: &StagedTrackFile) -> UniteTrack {
    UniteTrack {
        track_id: track.track_id.clone(),
        title: track.title.clone(),
        track_number: track.track_number,
        disc_number: track.disc_number,
        item_index: None,
        duration_ms: track.duration_measured.then_some(track.duration_ms).flatten(),
        expected_duration_ms: None,
        duration_match: None,
        quality: quality_of(track),
        bytes: track.bytes,
    }
}

// ---------------------------------------------------------------------------
// Path A: the fetched document and its art
// ---------------------------------------------------------------------------

/// Fetch a `web.noudata` through the shared web hub (consent already established) and take
/// it out of the session store.
fn fetch_document(web: &WebHub, key: &str) -> Result<HeldRelease, String> {
    let fetched = web.fetch(key, Some(true)).map_err(|e| e.to_string())?;
    let held = web
        .held(&fetched.token)
        .ok_or_else(|| "the fetched release expired before it could be used".to_string())?;
    web.discard(&fetched.token);
    Ok(held)
}

/// Download the document's cover images and thumbnails into the staging folder.
///
/// A cover that cannot be fetched is a warning: the `.noudata` keeps its `imageWebURL`, and
/// the release is still complete.
fn download_document_art(
    web: &WebHub,
    limits: &DownloadLimits,
    stage: &Path,
    doc: &NoudataDocument,
    art: &mut Vec<LocalArt>,
    warnings: &mut Vec<String>,
) {
    let (targets, refused) = plan_cover_targets(doc);
    for (url, why) in refused {
        warnings.push(format!(
            "cover {} not fetched: {why}",
            sanitise_line(&url, 120)
        ));
    }
    if targets.is_empty() {
        return;
    }
    let Ok(consent) = Consent::from_request(Some(true)) else {
        return;
    };
    let transport = web.transport();
    let policy = web.policy();
    let budget = Budget::new(limits.max_batch_bytes);
    let cancel = Cancel::new();
    for target in targets {
        let dir = stage.join("art").join(format!("c{}", target.entry));
        if fs::create_dir_all(&dir).is_err() {
            warnings.push("cover art could not be staged".to_string());
            continue;
        }
        let got = download_to_temp(
            transport.as_ref(),
            &consent,
            &policy,
            limits,
            &target.url,
            &dir,
            Want::Image,
            &budget,
            &cancel,
            &mut |_, _| {},
        );
        let (info, path) = match got {
            Ok(download) => match download.image {
                Some(info) => {
                    let path = dir.join(format!("{}.{}", target.local_stem, info.kind.ext()));
                    let source = download.temp.keep();
                    if fs::rename(&source, &path).is_err() {
                        let _ = fs::remove_file(&source);
                        warnings.push("cover art could not be kept".to_string());
                        continue;
                    }
                    (info, path)
                }
                None => continue,
            },
            Err(error) => {
                warnings.push(format!("cover image not fetched: {error}"));
                continue;
            }
        };
        let mut thumbs = Vec::new();
        for thumb in target.thumbnails() {
            let mut got = download_to_temp(
                transport.as_ref(),
                &consent,
                &policy,
                limits,
                &thumb.url,
                &dir,
                Want::Thumbnail,
                &budget,
                &cancel,
                &mut |_, _| {},
            );
            if matches!(
                got,
                Err(DownloadError::NotFound
                    | DownloadError::WrongType(_)
                    | DownloadError::BadImage(_))
            ) {
                // §2: retry `/Thumbnails/` (linkhosts MAY omit tiers).
                got = download_to_temp(
                    transport.as_ref(),
                    &consent,
                    &policy,
                    limits,
                    &thumb.fallback_url,
                    &dir,
                    Want::Thumbnail,
                    &budget,
                    &cancel,
                    &mut |_, _| {},
                );
            }
            let Ok(download) = got else { continue };
            let path = dir.join(format!(
                "{}-{}.{}",
                target.local_stem, thumb.tier, thumb.ext
            ));
            let source = download.temp.keep();
            if fs::rename(&source, &path).is_ok() {
                thumbs.push((thumb.tier, thumb.ext, path));
            } else {
                let _ = fs::remove_file(&source);
            }
        }
        art.push(LocalArt {
            entry: Some(target.entry),
            path,
            stem: target.local_stem,
            info,
            thumbs,
        });
    }
}

/// The store's own cover, as an alternative primary (the larger image wins).
///
/// This is part of the consented download batch; it is skipped when the release has no
/// cover, and a failure is a warning rather than a failed download.
fn store_cover(
    store: &StoreHub,
    release_id: &str,
    stage: &Path,
    warnings: &mut Vec<String>,
) -> Option<LocalArt> {
    let cover: StoreCover = match store.cover(release_id) {
        Ok(cover) => cover,
        Err(StoreApiError::NoCover) => return None,
        Err(error) => {
            warnings.push(format!(
                "the store cover could not be read: {}",
                error.message()
            ));
            return None;
        }
    };
    let info = match sniff_image(&cover.bytes, MAX_IMAGE_SIDE) {
        Ok(info) => info,
        Err(error) => {
            warnings.push(format!("the store cover was not usable: {error}"));
            return None;
        }
    };
    let dir = stage.join("art").join("store");
    fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("cover.{}", info.kind.ext()));
    fs::write(&path, &cover.bytes[..]).ok()?;
    Some(LocalArt {
        entry: None,
        path,
        stem: "cover".to_string(),
        info,
        thumbs: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Idempotence
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ExistingScan {
    /// `(release id, library-relative `.noudata`)` for every item this store produced.
    same: Vec<(String, String)>,
    /// The item that owns the requested Noubin key.
    key_owner: Option<ExistingKeyOwner>,
}

impl ExistingScan {
    fn existing_for(&self, release_id: &str) -> Option<String> {
        self.same
            .iter()
            .find(|(id, _)| id == release_id)
            .map(|(_, rel)| rel.clone())
    }
}

struct ExistingKeyOwner {
    #[allow(dead_code)] // kept so a caller can say where the key lives
    rel: String,
    release_id: Option<String>,
}

/// Disk-only scan for an item this store release (or the requested key) already produced.
///
/// No request is made here, so a second tap of the same release answers offline.
/// Release ids among `ids` that already have a library item, from one disk-only scan.
///
/// The download worker uses this before it fetches anything: a release already in the
/// library is answered `exists` without a request or a byte of audio.
pub(crate) fn present_release_ids(root: &Path, ids: &[String]) -> HashSet<String> {
    find_existing(root, ids, None)
        .same
        .into_iter()
        .map(|(release_id, _)| release_id)
        .collect()
}

fn find_existing(root: &Path, release_ids: &[String], key: Option<&str>) -> ExistingScan {
    let mut scan = ExistingScan::default();
    let Ok(paths) = collect_noudata_paths(
        &LibraryRoot::new(root.to_path_buf()),
        MIN_LIBRARY_SCAN_DEPTH,
    ) else {
        return scan;
    };
    let wanted_key = key.map(normalise_noubin_key);
    for path in paths.into_iter().take(MAX_LIBRARY_DOCS_SCANNED) {
        let Ok(bytes) = read_noudata_capped(&path) else {
            continue;
        };
        let Ok(doc) = parse_noudata(&bytes) else {
            continue;
        };
        let rel = relative_posix(root, &path);
        let record = store_source_record(&doc);
        if let Some(record_release) = record.as_deref() {
            if release_ids.iter().any(|id| id == record_release) {
                scan.same.push((record_release.to_string(), rel.clone()));
            }
        }
        if let Some(wanted) = wanted_key.as_deref() {
            let doc_key = doc
                .noubin_key
                .as_deref()
                .map(normalise_noubin_key)
                .or_else(|| doc.noubin_key_normalised.clone());
            if doc_key.as_deref() == Some(wanted) {
                scan.key_owner = Some(ExistingKeyOwner {
                    rel,
                    release_id: record,
                });
                break;
            }
        }
    }
    scan
}

/// `plippaStoreSource.releaseId` of a document (our own provenance, and BCS_06's).
fn store_source_record(doc: &NoudataDocument) -> Option<String> {
    let Value::Object(map) = doc.extra.get(STORE_SOURCE_KEY)? else {
        return None;
    };
    let store = map.get("store").and_then(Value::as_str).unwrap_or(PROVIDER);
    if store != PROVIDER {
        return None;
    }
    map.get("releaseId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn relative_posix(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}
