//! Engine control plane: session/transport, tap→lookup→play, IPC handlers.
//!
//! # `nowPlayingList` (NB_04)
//!
//! Every load (tap, portal Play, `LoadCollection`) builds a [`NowPlayingList`] from the
//! playable (see `collection.rs`) and replaces the previous one. `Next` / `Previous` /
//! `JumpToEntry` navigate it: with navigable cues they seek **inside the current
//! track** (no re-prepare), otherwise they change track. The playing cue is derived
//! from the *media* position ([`plippa_core::media_position_ms`]), never wall-clock.
//! A track that ends advances automatically ([`RepeatMode::Queue`] wraps,
//! `NowPlayingList::pause_after_each` stops instead, [`RepeatMode::Item`] restarts the same
//! track). All of it is session state, set by the play-mode commands: Noubin v0.4.1 file data
//! no longer sets them. Mid-playback order changes ([`PlaybackCommand::SetOrderMode`]) are
//! **list-only**: they never re-prepare the stream.
//! Missing media is skipped with a warning notice.
//! Long-form playables (`podcast`/`audiobook`/`mix`, ≥ [`RESUME_MIN_DURATION_MS`])
//! send a [`ResumeWriteback`] on stop/pause/switch; the server writes the `.noudata`.
//!
//! # Ignore-repeat-same-tag policy
//!
//! **Owned by `plippa-engine` for MVP_05.** While transport is **Playing**, a tap
//! whose normalised key equals the currently loaded playable's key is ignored
//! (Noubin §3.2 duplicate/repeated tap). While **Paused**, the same tap resumes
//! playback (with tap SFX). `plippa-ui` may also filter in MVP_06/08; engine
//! remains the source of truth so remote/harness taps behave the same way.
//!
//! # Session resume (tap-only)
//!
//! While Stopped, a later tap of the same Noubin resumes from a session bookmark
//! (last 10 tracks, RAM only — not `.noudata`). Every successful tap plays the
//! noubin SFX when configured (user setting later). If the bookmark is finished
//! or within the last 15 s of the track, the next tap restarts from 0.
//! UI / web Play and LoadAndPlay always start at 0 (no session seek).
//! Portal **Unload** stops playback and clears all session bookmarks so the next
//! tap starts from 0 for every remembered key.//!
//! # Local-first
//!
//! The tap-success path performs §6.1 extract → §6.2 normalise → SQLite lookup
//! only. There is **no HTTP client** in this crate; unmatched keys emit a
//! user-visible error, and an unmatched URL key additionally emits a structured
//! [`plippa_ipc_proto::UnmatchedNoubin`] (NB_08) that the server/portal may turn into a
//! consent-gated web lookup.
//!
//! # Database
//!
//! Opens the library index as a WAL **reader** ([`LibraryReader`]). The engine
//! never writes the index (writes belong to `plippa-server` / MVP_07).

use crate::audio_thread::{
    AudioCommand, AudioThreadHandle, CrossEvent, NeighbourSrc, PlayArm, RateChange,
};
use crate::collection::{build_collection, load_cues, BuiltCollection, CollectionPrefs};
use crate::resume::{ResumeEntry, ResumeStore, RESUME_RESTART_TAIL_MS};
use crate::route::RouteManager;
use crate::route_factory::{RouteTarget, RouteTargetCell, MULTI_TARGET};
use plippa_audio::isolate::Isolation;
use plippa_audio::position::frame_to_ms;
use plippa_audio::route::{RouteIntent, Target};
use plippa_audio::{
    decode, select_best_local_path, AudioError, AudioSink, MockSink, OverspecPolicy, PcmBuffer,
    PulseConfig,
};
use plippa_core::{
    audio_format_label, clamp_seek_target_ms, media_position_ms, rate_permille_valid, AudioRoute, Capabilities, Clock,
    NavAction, NowPlayingList, OrderMode, OrderOutcome, PlayableItemId, PreviousPressTracker,
    RepeatMode, ResumePolicy, ScanState, SessionState, SourceOrder, SystemClock, TransportState,
    RATE_UNITY_PERMILLE,
};
use plippa_ipc_proto::{
    IpcMessage, LibraryIndexUpdated, NoticeSeverity, NowPlayingListChanged, PlaybackCommand,
    PlaybackStatus, ResumeWriteback, TapKeyPayload, TapNdefPayload, UnmatchedNoubin, UserNotice,
    NOTICE_DECODE_FAILED, NOTICE_DUPLICATE_KEY, NOTICE_LIBRARY_MISS, NOTICE_MEDIA_FILE_MISSING,
    NOTICE_MEDIA_UNPLAYABLE,
    NOTICE_NDEF_INVALID, NOTICE_NO_FIXED_ORDER, NOTICE_NO_MEDIA_STORAGE,
    MAX_CANDIDATE_URL_BYTES, MAX_UNMATCHED_CANDIDATE_URLS,
};
use plippa_noubin::{
    decode_playable_id, extract_noubin_key, is_url_noubin_key, library_namespace,
    normalise_noubin_key, parse_noudata, MAX_KEY_BYTES,
    resolve_library_roots, LibraryError, LibraryKind, LibraryReader, LibraryRoot, LookupResult,
    MediaListItem, OpenWait, PlayableRecord,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Long-form resume is only persisted for playables at least this long (total, else the
/// current item). Spec: "podcast, audiobook or mix AND longer than X minutes".
pub const RESUME_MIN_DURATION_MS: u64 = 10 * 60 * 1000;
/// Lists longer than this are published truncated (keeps the IPC frame < 1 MiB).
pub const MAX_PUBLISHED_ENTRIES: usize = 1500;

const DEFAULT_BLOCK_FRAMES: usize = 256;
const DEFAULT_CHANNELS: u16 = 2;
const DEFAULT_SAMPLE_RATE_HZ: u32 = 48_000;
/// When tap SFX is enabled, hold main-track audible start until this after Play arm
/// so the WAV cue covers open/decode and overlaps the music on its echo tail.
const SFX_COVER_BEFORE_MAIN: Duration = Duration::from_secs(1);

/// PLAY_02: every reshuffle advances the shuffle seed by this constant, so a test that fixes
/// the starting seed can predict each pass's order.
const SHUFFLE_SEED_STEP: u64 = 0x9E37_79B9_7F4A_7C15;

/// PLAY_03: a segment shorter than this is not seeked away from. A permuted segment shorter
/// than [`plippa_core::CUE_BOUNDARY_LEAD_MS`] would land its own seek inside its lead window and
/// re-trigger on the next tick (boundary thrash), so such a boundary is left alone and playback
/// runs on into the next chronological segment; an item end is never skipped this way.
const CUE_BOUNDARY_MIN_SEGMENT_MS: u64 = 200;

/// Control-plane errors (never panic out of handlers).
#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Library(#[from] plippa_noubin::LibraryError),
    #[error(transparent)]
    Audio(#[from] plippa_audio::AudioError),
    #[error(transparent)]
    Noudata(#[from] plippa_noubin::NoudataError),
    #[error("no playable loaded")]
    NothingLoaded,
    #[error("{0}")]
    Message(String),
}

/// The roots the audio thread opens a neighbour's path beneath: the override root when there is
/// one (tests, single-root installs), else the resolved library roots (multi-USB).
///
/// Passing the resolved roots matters: with USB auto-discovery there is no override root, and an
/// empty list would send every neighbour pre-open down the unconfined no-follow path.
fn audio_roots(override_root: &Option<PathBuf>, resolved: &[PathBuf]) -> Vec<PathBuf> {
    match override_root {
        Some(root) => vec![root.clone()],
        None => resolved.to_vec(),
    }
}

/// Open `path` as a decoder in the media helper (the engine never decodes a file in-process).
fn isolated_open(
    roots: &[PathBuf],
    path: &Path,
    caps: &Capabilities,
    overspec: OverspecPolicy,
) -> Result<plippa_audio::isolate::RemoteDecoder, plippa_audio::AudioError> {
    crate::audio_thread::open_isolated_at(Isolation::global(), roots, path, caps, overspec)
}

/// `validate_for_stream` for `path`, probed in the media helper.
fn isolated_validate(
    roots: &[PathBuf],
    path: &Path,
    caps: &Capabilities,
    overspec: OverspecPolicy,
) -> Result<plippa_audio::AudioProbe, plippa_audio::AudioError> {
    let file = plippa_core::safe_fs::open_media_for_decode_in(roots, path)
        .map_err(|source| plippa_audio::AudioError::Io { path: path.to_path_buf(), source })?;
    Isolation::global().validate_for_stream(&file, path, caps, overspec)
}

/// The notice an open/seek failure maps to.
///
/// A format this board does not decode, or a release with no playable entry, is
/// [`NOTICE_MEDIA_UNPLAYABLE`]: the file is there, another player may well play it, and the
/// message names the format and the board. Everything else keeps
/// [`NOTICE_MEDIA_FILE_MISSING`] (the file could not be opened).
fn open_failure_notice(e: &EngineError, caps: &Capabilities) -> (&'static str, String) {
    use plippa_audio::AudioError;
    let EngineError::Audio(audio) = e else {
        return (NOTICE_MEDIA_FILE_MISSING, e.to_string());
    };
    let board = &caps.board_id;
    match audio {
        AudioError::FormatNotEnabled { ext, .. } => (
            NOTICE_MEDIA_UNPLAYABLE,
            format!("{} is not supported by this player ({board})", audio_format_label(ext)),
        ),
        AudioError::UnsupportedFormat { path } => {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let what = if ext.is_empty() {
                "This file's format".to_string()
            } else {
                audio_format_label(ext)
            };
            (
                NOTICE_MEDIA_UNPLAYABLE,
                format!("{what} is not supported by this player ({board})"),
            )
        }
        AudioError::QualityExceedsCapabilities { .. } => (NOTICE_MEDIA_UNPLAYABLE, audio.to_string()),
        AudioError::Unplayable { .. } => (NOTICE_MEDIA_UNPLAYABLE, audio.to_string()),
        AudioError::NoPlayablePath => (
            NOTICE_MEDIA_UNPLAYABLE,
            format!("None of this item's files can be played by this player ({board})"),
        ),
        _ => (NOTICE_MEDIA_FILE_MISSING, e.to_string()),
    }
}

/// Outbound events collected for the in-process harness (UDS later).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    Status(PlaybackStatus),
    Notice(UserNotice),
    /// `nowPlayingList` loaded / replaced / track or cue set changed.
    NowPlaying(NowPlayingListChanged),
    /// Ask the server to persist (or clear) a long-form resume point.
    ResumeWriteback(ResumeWriteback),
    /// NB_08: a URL Noubin matched nothing locally (offer for the consent-gated web lookup).
    UnmatchedNoubin(UnmatchedNoubin),
    /// AUDIO_01: the routed outputs / delays / diagnostics changed.
    AudioRoute(AudioRoute),
}

/// Loaded playable and its `nowPlayingList`.
#[derive(Debug, Clone)]
struct LoadedPlayable {
    record: PlayableRecord,
    /// Library root this playable was resolved from (multi-USB).
    library_root: PathBuf,
    list: NowPlayingList,
    /// PLAY_02: identity of this list, assigned once per load (replacement). Reorders and
    /// track changes keep it; a new load allocates a new one.
    list_id: u64,
}

/// Where a load starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeSource {
    /// First track, from 0.
    None,
    /// The playable's `resumeAtItem` / `resumeFromTime`.
    Persisted,
    /// RAM bookmark of this process (tap path); `item` is a `mediaList` position.
    Session { item: u32, ms: u64 },
}

#[derive(Debug, Clone, Copy)]
struct LoadOpts {
    autoplay: bool,
    with_sfx: bool,
    resume: ResumeSource,
}

/// Counters for position + test hooks (kept apart so the four constructors stay short).
#[derive(Debug)]
struct PlaybackExtras {
    /// `StartMainStream` commands sent (= track prepares).
    starts_sent: u64,
    /// `SeekMs` commands sent.
    seeks_sent: u64,
    /// Position the newest start/seek asked for, reported until the audio thread catches up.
    pending_target_ms: Option<u64>,
    /// Media seconds per output second (1.0 = normal; tempo/warp will set this).
    rate_factor: f64,
    shuffle_seed: u64,
    /// PLAY_02: monotonic id handed to each loaded list (`NowPlayingListChanged.list_id`).
    /// Starts at 1 so 0 always means "no list yet".
    next_list_id: u64,
    last_writeback: Option<ResumeWriteback>,
    /// Monotonic clock for the `Previous` double-press window (injectable for tests).
    clock: Box<dyn Clock>,
    /// NB_04b: time of the last un-paired `Previous`; cleared by any other transport action.
    previous_press: PreviousPressTracker,
    /// WARP_01: `(starts_sent, seeks_sent)` right after the hard seek that stands in for the
    /// current `SeekTo` chase. `Some` while `session.seek_target_ms` is `Some`; the target is
    /// cleared when the audio thread has completed both counters. While it is `Some`,
    /// `position_ms` stays the **actual** playhead (the `SeekMs` rule of reporting the pending
    /// target does not apply to `SeekTo`).
    seek_to_marker: Option<(u64, u64)>,
    /// WARP_05: tape bounces of the audio thread already mirrored into the set speed.
    bounces_seen: u64,
    /// WARP_05: a reset to 100 % was sent and the audio thread may still be ramping back, so
    /// the fast status cadence stays on until it reports the bypass voice again.
    settling: bool,
    /// WARP_17: `ScanStart` commands sent to the audio thread (compared with its
    /// `scans_handled` / `scan_active` to notice a scan that ended by itself: track end/start).
    scan_cmds_sent: u64,
    /// WARP_17: the running scan started from Stopped (stream opened for it): when the audio
    /// thread has finished it, the transport goes back to Stopped.
    scan_return_stop: bool,
    /// UI_15: a `ScanStop` was sent and the audio thread has not settled it yet. The settled
    /// position (and the restored set speed) is published once, by [`Engine::poll_scan_end`],
    /// so the UI never sees a position that then changes.
    scan_stop_pending: bool,
    /// WARP_19: neighbour-open failures of the audio thread already reported.
    cross_failures_seen: u64,
    /// WARP_19: durations of neighbour files that the list did not know (probed once).
    probed_durations: std::collections::HashMap<PathBuf, Option<u64>>,
    /// WAVE_01: an explicit Unload clears `SessionState.waveform_path` until the next load,
    /// although `loaded` is kept so Play can restart the same item.
    waveform_suppressed: bool,
    /// PLAY_03: a pause-after-each segment boundary left the *next* segment's start for `Play`
    /// (`Stopped` otherwise reopens the item at 0:00). Every other playback command clears it.
    held_segment_start_ms: Option<u64>,
    /// SEAM_01: what the audio thread was last told about its neighbours; a change of the list
    /// or the play modes re-sends them (see [`Engine::sync_neighbours`]).
    neighbours_sent: Option<NeighbourKey>,
}

/// SEAM_01: everything the neighbours handed to the audio thread depend on. Cheap to compute
/// (list state only, no file access) so the hub tick can compare it every turn.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NeighbourKey {
    /// A new stream starts with no neighbours: this counter changes at every prepare.
    starts_sent: u64,
    list_id: u64,
    current: usize,
    /// The item that plays next at the end of this one (`None`: end of list, repeat-item,
    /// pause-after-each, a cue segment left, a wrap that reshuffles).
    seamless: Option<(usize, u32)>,
    /// What `Next` / `Previous` would select, while a scan may cross (index and the item's
    /// identity: a reorder that keeps the indices can put another item there).
    scan_prev: Option<(usize, u32)>,
    scan_next: Option<(usize, u32)>,
    scanning: bool,
    seek_outside_track: bool,
}

impl Default for PlaybackExtras {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5EED);
        Self {
            starts_sent: 0,
            seeks_sent: 0,
            pending_target_ms: None,
            rate_factor: 1.0,
            shuffle_seed: seed,
            next_list_id: 1,
            last_writeback: None,
            clock: Box::new(SystemClock::new()),
            previous_press: PreviousPressTracker::default(),
            seek_to_marker: None,
            bounces_seen: 0,
            settling: false,
            scan_cmds_sent: 0,
            scan_return_stop: false,
            scan_stop_pending: false,
            cross_failures_seen: 0,
            probed_durations: std::collections::HashMap::new(),
            waveform_suppressed: false,
            held_segment_start_ms: None,
            neighbours_sent: None,
        }
    }
}

/// One stream open recorded by the shared mock capture.
#[derive(Debug, Clone, PartialEq)]
pub struct MockStreamOpen {
    /// Requested sink (`None` = server default).
    pub dev: Option<String>,
    /// Latency profile requested for the stream (`None` on the plain mock path).
    pub cfg: Option<PulseConfig>,
    pub channels: u16,
    pub sample_rate_hz: u32,
}

/// Shared mock capture so tests can inspect PCM from track + SFX streams.
struct SharedMockCapture {
    samples: Vec<f32>,
    sample_rate_hz: u32,
    channels: u16,
    /// Rates at which streams were opened (track and/or SFX).
    open_rates: Vec<u32>,
    /// What each stream open asked for (AUDIO_03 routed tests).
    opens: Vec<MockStreamOpen>,
}

impl Default for SharedMockCapture {
    fn default() -> Self {
        Self {
            samples: Vec::new(),
            sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
            channels: DEFAULT_CHANNELS,
            open_rates: Vec::new(),
            opens: Vec::new(),
        }
    }
}

struct SharedMockSink {
    channels: u16,
    sample_rate_hz: u32,
    capture: Arc<Mutex<SharedMockCapture>>,
}

impl AudioSink for SharedMockSink {
    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    fn write(&mut self, interleaved: &[f32]) -> Result<(), AudioError> {
        if interleaved.len() % self.channels as usize != 0 {
            return Err(AudioError::Sink(format!(
                "write length {} not divisible by {} channels",
                interleaved.len(),
                self.channels
            )));
        }
        let mut g = self.capture.lock().expect("mock capture");
        g.sample_rate_hz = self.sample_rate_hz;
        g.channels = self.channels;
        g.samples.extend_from_slice(interleaved);
        Ok(())
    }
}

/// In-process engine harness used by unit tests and the binary.
pub struct Engine {
    capabilities: Capabilities,
    /// Explicit CLI/env override (skip USB discovery when set).
    override_root: Option<PathBuf>,
    /// Open library roots (override or USB discovery). First is primary.
    library_roots: Vec<PathBuf>,
    /// WAL readers paired with their roots. Empty when nothing is indexed yet.
    readers: Vec<(PathBuf, LibraryReader)>,
    session: SessionState,
    audio: Option<AudioThreadHandle>,
    loaded: Option<LoadedPlayable>,
    /// Last successfully loaded normalised key (for ignore-repeat).
    active_normalised_key: Option<String>,
    /// NB_08: counter for [`UnmatchedNoubin::seq`].
    unmatched_seq: u64,
    index_generation: u64,
    /// Backoff for readers that could not be opened (busy index, index not created yet).
    library_retry: LibraryRetry,
    events: Vec<EngineEvent>,
    /// Optional one-shot SFX fired after a successful load+play.
    load_sfx: Option<PcmBuffer>,
    overspec: OverspecPolicy,
    /// Present when opened with [`Engine::open_mock`].
    mock_capture: Option<Arc<Mutex<SharedMockCapture>>>,
    /// True when the audio factory is the silent/mock path (no Pulse/RCA).
    sink_is_mock: bool,
    /// Set when a tap/load begins; cleared once first audio frames are observed.
    tap_load_started: Option<Instant>,
    /// Last measured tap→first-audio latency (for logs / tests).
    last_tap_to_audio: Option<Duration>,
    /// Session-only resume bookmarks (noubin tap path; not `.noudata`).
    resume_store: ResumeStore,
    /// Monotonic `PlaybackStatus.revision` (bumped on transport / item change).
    status_revision: u64,
    /// Board-configured PipeWire sink of the built-in DAC (AUDIO_01 §5.3).
    local_sink: Option<String>,
    /// Board-configured fallback substring for the local sink (AUDIO_01 §5.3).
    local_sink_match: Option<String>,
    /// Audio routing state machine; `None` until a backend is attached (AUDIO_01 §7.4).
    route: Option<RouteManager>,
    /// Sink target the audio thread opens next (AUDIO_03).
    route_target: Option<RouteTargetCell>,
    /// `AudioRoute.revision` last acted on by [`Self::poll_audio_route`].
    last_route_revision: u64,
    /// `AudioRoute.revision` last published as [`EngineEvent::AudioRoute`].
    last_published_route_revision: u64,
    /// Intent the currently open stream was started for (route-change re-stream).
    stream_intent: Option<RouteIntent>,
    /// True after enqueueing `StartMainStream` until audio reports completion.
    pending_start: bool,
    /// Last observed `starts_completed` from the audio thread.
    last_starts_completed: u64,
    extras: PlaybackExtras,
}

impl Engine {
    /// Open a WAL library index (read-only) and start a mock-sink audio thread.
    ///
    /// Missing library root or missing `.plippa` index does **not** fail open —
    /// a [`UserNotice`] is queued and the process stays alive (server may create
    /// the index later).
    pub fn open_mock(
        library_root: impl Into<PathBuf>,
        capabilities: Capabilities,
    ) -> Result<Self, EngineError> {
        Self::open_mock_inner(library_root, capabilities, None)
    }

    /// Mock sink that records which routed target each stream asked for.
    ///
    /// The route manager pushes into `target` exactly as it does for the Pulse
    /// factory, so a test can assert the device string the engine chose without a
    /// real audio server.
    pub fn open_mock_routed(
        library_root: impl Into<PathBuf>,
        capabilities: Capabilities,
        target: RouteTargetCell,
    ) -> Result<Self, EngineError> {
        Self::open_mock_inner(library_root, capabilities, Some(target))
    }

    fn open_mock_inner(
        library_root: impl Into<PathBuf>,
        capabilities: Capabilities,
        target: Option<RouteTargetCell>,
    ) -> Result<Self, EngineError> {
        let library_root = library_root.into();
        let override_root = Some(library_root);
        let (roots, readers, open_notice) = open_roots(override_root.clone());

        let capture = Arc::new(Mutex::new(SharedMockCapture::default()));
        let capture_for_factory = Arc::clone(&capture);
        let target_for_factory = target.clone();
        let audio = AudioThreadHandle::spawn_with_config(
            move |channels, sample_rate_hz| {
                let snapshot = target_for_factory
                    .as_ref()
                    .and_then(|cell| cell.lock().ok().map(|g| g.clone()));
                let dev = snapshot.as_ref().and_then(|t| t.dev.clone());
                let cfg = snapshot.map(|t| t.cfg);
                if let Ok(mut g) = capture_for_factory.lock() {
                    g.open_rates.push(sample_rate_hz);
                    g.opens.push(MockStreamOpen {
                        dev,
                        cfg,
                        channels,
                        sample_rate_hz,
                    });
                }
                Ok(Box::new(SharedMockSink {
                    channels,
                    sample_rate_hz,
                    capture: Arc::clone(&capture_for_factory),
                }) as Box<dyn AudioSink>)
            },
            DEFAULT_BLOCK_FRAMES,
            crate::audio_thread::AudioThreadConfig {
                library_roots: audio_roots(&override_root, &roots),
                ..Default::default()
            },
        );
        let session = SessionState::default();
        // Keep mixer volume aligned with session default.
        audio.volume.set(session.volume.as_linear());
        let _ = audio.try_send(AudioCommand::SetVolume(session.volume.as_linear()));

        let mut engine = Self {
            capabilities,
            override_root,
            library_roots: roots,
            readers,
            session,
            audio: Some(audio),
            loaded: None,
            active_normalised_key: None,
            unmatched_seq: 0,
            index_generation: 0,
            library_retry: LibraryRetry::default(),
            events: Vec::new(),
            load_sfx: None,
            overspec: OverspecPolicy::Downsample,
            mock_capture: Some(capture),
            sink_is_mock: true,
            tap_load_started: None,
            last_tap_to_audio: None,
            resume_store: ResumeStore::new(),
            status_revision: 0,
            local_sink: None,
            local_sink_match: None,
            route: None,
            route_target: None,
            last_route_revision: 0,
            last_published_route_revision: 0,
            stream_intent: None,
            pending_start: false,
            last_starts_completed: 0,
            extras: PlaybackExtras::default(),
        };
        if let Some(n) = open_notice {
            engine.events.push(EngineEvent::Notice(n));
        }
        Ok(engine)
    }

    /// Open with a sink factory (e.g. Pulse at the file’s native rate).
    pub fn open_with_sink_factory<F>(
        library_root: impl Into<PathBuf>,
        capabilities: Capabilities,
        factory: F,
    ) -> Result<Self, EngineError>
    where
        F: plippa_audio::AudioSinkFactory + Send + Sync + 'static,
    {
        let library_root = library_root.into();
        let override_root = Some(library_root);
        let (roots, readers, open_notice) = open_roots(override_root.clone());
        let audio = AudioThreadHandle::spawn_with_config(
            factory,
            DEFAULT_BLOCK_FRAMES,
            crate::audio_thread::AudioThreadConfig {
                library_roots: audio_roots(&override_root, &roots),
                ..Default::default()
            },
        );
        let session = SessionState::default();
        audio.volume.set(session.volume.as_linear());
        let _ = audio.try_send(AudioCommand::SetVolume(session.volume.as_linear()));

        let mut engine = Self {
            capabilities,
            override_root,
            library_roots: roots,
            readers,
            session,
            audio: Some(audio),
            loaded: None,
            active_normalised_key: None,
            unmatched_seq: 0,
            index_generation: 0,
            library_retry: LibraryRetry::default(),
            events: Vec::new(),
            load_sfx: None,
            overspec: OverspecPolicy::Downsample,
            mock_capture: None,
            sink_is_mock: false,
            tap_load_started: None,
            last_tap_to_audio: None,
            resume_store: ResumeStore::new(),
            status_revision: 0,
            local_sink: None,
            local_sink_match: None,
            route: None,
            route_target: None,
            last_route_revision: 0,
            last_published_route_revision: 0,
            stream_intent: None,
            pending_start: false,
            last_starts_completed: 0,
            extras: PlaybackExtras::default(),
        };
        if let Some(n) = open_notice {
            engine.events.push(EngineEvent::Notice(n));
        }
        Ok(engine)
    }

    /// Open using override-or-discover roots (desk multi-USB). Prefer this from the binary.
    pub fn open_mock_discovered(
        override_root: Option<PathBuf>,
        capabilities: Capabilities,
    ) -> Result<Self, EngineError> {
        let (roots, readers, open_notice) = open_roots(override_root.clone());

        let capture = Arc::new(Mutex::new(SharedMockCapture::default()));
        let capture_for_factory = Arc::clone(&capture);
        let audio = AudioThreadHandle::spawn_with_config(
            move |channels, sample_rate_hz| {
                if let Ok(mut g) = capture_for_factory.lock() {
                    g.open_rates.push(sample_rate_hz);
                }
                Ok(Box::new(SharedMockSink {
                    channels,
                    sample_rate_hz,
                    capture: Arc::clone(&capture_for_factory),
                }) as Box<dyn AudioSink>)
            },
            DEFAULT_BLOCK_FRAMES,
            crate::audio_thread::AudioThreadConfig {
                library_roots: audio_roots(&override_root, &roots),
                ..Default::default()
            },
        );
        let session = SessionState::default();
        audio.volume.set(session.volume.as_linear());
        let _ = audio.try_send(AudioCommand::SetVolume(session.volume.as_linear()));

        let mut engine = Self {
            capabilities,
            override_root,
            library_roots: roots,
            readers,
            session,
            audio: Some(audio),
            loaded: None,
            active_normalised_key: None,
            unmatched_seq: 0,
            index_generation: 0,
            library_retry: LibraryRetry::default(),
            events: Vec::new(),
            load_sfx: None,
            overspec: OverspecPolicy::Downsample,
            mock_capture: Some(capture),
            sink_is_mock: true,
            tap_load_started: None,
            last_tap_to_audio: None,
            resume_store: ResumeStore::new(),
            status_revision: 0,
            local_sink: None,
            local_sink_match: None,
            route: None,
            route_target: None,
            last_route_revision: 0,
            last_published_route_revision: 0,
            stream_intent: None,
            pending_start: false,
            last_starts_completed: 0,
            extras: PlaybackExtras::default(),
        };
        if let Some(n) = open_notice {
            engine.events.push(EngineEvent::Notice(n));
        }
        Ok(engine)
    }

    pub fn open_with_sink_factory_discovered<F>(
        override_root: Option<PathBuf>,
        capabilities: Capabilities,
        factory: F,
    ) -> Result<Self, EngineError>
    where
        F: plippa_audio::AudioSinkFactory + Send + Sync + 'static,
    {
        let (roots, readers, open_notice) = open_roots(override_root.clone());
        let audio = AudioThreadHandle::spawn_with_config(
            factory,
            DEFAULT_BLOCK_FRAMES,
            crate::audio_thread::AudioThreadConfig {
                library_roots: audio_roots(&override_root, &roots),
                ..Default::default()
            },
        );
        let session = SessionState::default();
        audio.volume.set(session.volume.as_linear());
        let _ = audio.try_send(AudioCommand::SetVolume(session.volume.as_linear()));

        let mut engine = Self {
            capabilities,
            override_root,
            library_roots: roots,
            readers,
            session,
            audio: Some(audio),
            loaded: None,
            active_normalised_key: None,
            unmatched_seq: 0,
            index_generation: 0,
            library_retry: LibraryRetry::default(),
            events: Vec::new(),
            load_sfx: None,
            overspec: OverspecPolicy::Downsample,
            mock_capture: None,
            sink_is_mock: false,
            tap_load_started: None,
            last_tap_to_audio: None,
            resume_store: ResumeStore::new(),
            status_revision: 0,
            local_sink: None,
            local_sink_match: None,
            route: None,
            route_target: None,
            last_route_revision: 0,
            last_published_route_revision: 0,
            stream_intent: None,
            pending_start: false,
            last_starts_completed: 0,
            extras: PlaybackExtras::default(),
        };
        if let Some(n) = open_notice {
            engine.events.push(EngineEvent::Notice(n));
        }
        Ok(engine)
    }

    /// Open with a fixed sink (legacy); rate changes reopen via a factory that
    /// ignores prior sink and builds Pulse/mock at the requested rate instead.
    /// Prefer [`Self::open_with_sink_factory`].
    pub fn open_with_sink<S: AudioSink + 'static>(
        library_root: impl Into<PathBuf>,
        capabilities: Capabilities,
        sink: S,
    ) -> Result<Self, EngineError> {
        // Keep the provided sink for the first open rate; further opens use a
        // mock at the requested rate so tests without a factory still compile.
        // Production Pulse path uses `open_with_sink_factory`.
        let sink = Arc::new(Mutex::new(Some(sink)));
        let sink_for_factory = Arc::clone(&sink);
        Self::open_with_sink_factory(library_root, capabilities, move |ch, rate| {
            let mut guard = sink_for_factory.lock().expect("sink");
            if let Some(existing) = guard.take() {
                if existing.channels() == ch && existing.sample_rate_hz() == rate {
                    return Ok(Box::new(existing) as Box<dyn AudioSink>);
                }
                // Rate mismatch: drop pre-opened sink and use silence mock.
                let _ = existing;
            }
            Ok(Box::new(MockSink::new(ch, rate)) as Box<dyn AudioSink>)
        })
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Board-profile values that name the built-in DAC's PipeWire sink (AUDIO_01).
    ///
    /// A builder rather than extra `open_*` parameters: the eleven existing `open_*`
    /// test call sites stay unchanged, and a board without `[audio]` values is simply
    /// the default (the engine then falls back to the server default sink).
    pub fn with_local_sink(
        mut self,
        local_sink: Option<String>,
        local_sink_match: Option<String>,
    ) -> Self {
        self.local_sink = local_sink;
        self.local_sink_match = local_sink_match;
        self
    }

    /// Exact local sink name from the board profile, if it has one.
    pub fn local_sink(&self) -> Option<&str> {
        self.local_sink.as_deref()
    }

    /// Board-profile substring fallback for the local sink, if it has one.
    pub fn local_sink_match(&self) -> Option<&str> {
        self.local_sink_match.as_deref()
    }

    pub fn session(&self) -> &SessionState {
        &self.session
    }

    pub fn library_root(&self) -> &Path {
        self.library_roots
            .first()
            .map(PathBuf::as_path)
            .unwrap_or_else(|| Path::new("."))
    }

    pub fn library_roots(&self) -> &[PathBuf] {
        &self.library_roots
    }

    /// Whether at least one WAL library reader is currently open.
    pub fn has_library_index(&self) -> bool {
        !self.readers.is_empty()
    }

    pub fn frames_written(&self) -> u64 {
        self.audio
            .as_ref()
            .map(|a| a.stats.frames_written.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn audio_thread_id(&self) -> Option<thread::ThreadId> {
        self.audio
            .as_ref()
            .and_then(|a| a.stats.audio_thread_id.lock().ok().and_then(|g| *g))
    }

    /// Linear gain currently applied by the audio mixer (0.0..=1.0).
    pub fn audio_volume_linear(&self) -> f32 {
        self.audio.as_ref().map(|a| a.volume.get()).unwrap_or(0.0)
    }

    /// Snapshot of mock-sink capture (empty if not using [`Engine::open_mock`]).
    pub fn mock_capture_snapshot(&self) -> Vec<f32> {
        self.mock_capture
            .as_ref()
            .and_then(|s| s.lock().ok().map(|g| g.samples.clone()))
            .unwrap_or_default()
    }

    /// Sample rates at which mock streams were opened (track + SFX).
    pub fn mock_open_rates(&self) -> Vec<u32> {
        self.mock_capture
            .as_ref()
            .and_then(|s| s.lock().ok().map(|g| g.open_rates.clone()))
            .unwrap_or_default()
    }

    /// What each mock stream open targeted (device + latency profile).
    pub fn mock_stream_opens(&self) -> Vec<MockStreamOpen> {
        self.mock_capture
            .as_ref()
            .and_then(|s| s.lock().ok().map(|g| g.opens.clone()))
            .unwrap_or_default()
    }

    pub fn clear_mock_capture(&self) {
        if let Some(s) = &self.mock_capture {
            if let Ok(mut g) = s.lock() {
                g.samples.clear();
            }
        }
    }

    pub fn set_load_sfx(&mut self, pcm: PcmBuffer) {
        self.load_sfx = Some(pcm);
    }

    /// Decode a tap-load SFX at its **native** rate (second PW stream; no resample).
    pub fn load_sfx_from_path(&mut self, path: &Path) -> Result<(), EngineError> {
        let pcm = decode(path)?;
        eprintln!(
            "plippa-engine: SFX decoded {} ({} Hz, {} ch, {} frames)",
            path.display(),
            pcm.sample_rate_hz,
            pcm.channels,
            pcm.frame_count()
        );
        self.load_sfx = Some(pcm);
        Ok(())
    }

    /// Last measured tap → first-audio latency, if any.
    pub fn last_tap_to_audio(&self) -> Option<Duration> {
        self.last_tap_to_audio
    }

    /// Active track stream sample rate (0 if idle).
    pub fn stream_sample_rate_hz(&self) -> u32 {
        self.audio
            .as_ref()
            .map(|a| a.stats.stream_sample_rate_hz.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Audio-thread first-audio latency for the current/last stream (ms), if any.
    pub fn stream_first_audio_ms(&self) -> u64 {
        self.audio
            .as_ref()
            .map(|a| a.stats.first_audio_ms.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// If a tap load is in flight and frames have started, record latency.
    pub fn poll_tap_to_audio_metric(&mut self) -> Option<Duration> {
        let started = self.tap_load_started?;
        if self.frames_written() == 0 {
            return None;
        }
        let elapsed = started.elapsed();
        self.tap_load_started = None;
        self.last_tap_to_audio = Some(elapsed);
        Some(elapsed)
    }

    /// Drain stream errors / EOF into transport + [`UserNotice`], then reconcile
    /// session transport with audio-thread facts (call from hub tick).
    pub fn poll_stream_lifecycle(&mut self) {
        self.poll_rate_bounce();
        if self.extras.settling && !self.audio_warp_active() {
            self.extras.settling = false;
        }
        let (err, finished, play_ignored, starts_completed, has_stream, main_playing) =
            if let Some(audio) = &self.audio {
                let err = audio.take_last_error();
                let finished = audio.stats.main_finished.swap(false, Ordering::SeqCst);
                let play_ignored = audio.stats.play_ignored.swap(false, Ordering::SeqCst);
                let starts_completed = audio.stats.starts_completed.load(Ordering::SeqCst);
                let has_stream = audio.stats.has_main_stream.load(Ordering::SeqCst);
                let main_playing = audio.stats.main_playing.load(Ordering::SeqCst);
                (
                    err,
                    finished,
                    play_ignored,
                    starts_completed,
                    has_stream,
                    main_playing,
                )
            } else {
                (None, false, false, 0, false, false)
            };

        if starts_completed > self.last_starts_completed {
            self.last_starts_completed = starts_completed;
            self.pending_start = false;
        }

        if let Some(msg) = err {
            self.pending_start = false;
            self.extras.seek_to_marker = None;
            self.session.seek_target_ms = None;
            let _ = self.session.set_transport(TransportState::Stopped);
            // The media helper died on this file (it is marked now): not a decode hiccup but a
            // file that cannot be played, reported as such.
            let unplayable = self
                .loaded_stream_source()
                .is_some_and(|(path, _)| Isolation::global().is_marked_by_path(&path));
            let code = if unplayable { NOTICE_MEDIA_UNPLAYABLE } else { NOTICE_DECODE_FAILED };
            self.emit_notice(code, msg);
            self.emit_status();
        }
        if finished && self.session.transport == TransportState::Playing {
            self.on_stream_finished();
        }
        // PLAY_03: a cue-shuffled mix changes segments inside the open stream (the guard
        // inside also skips it when the EOF path just stopped the item).
        self.poll_cue_boundaries();
        if play_ignored && self.session.transport == TransportState::Playing && !self.pending_start
        {
            eprintln!(
                "plippa-engine: reconcile — Play ignored while session Playing; rolling back"
            );
            let _ = self.session.set_transport(TransportState::Stopped);
            self.emit_status();
        }
        // Sticky Playing with no stream and no start in flight → rollback.
        // (A stream that exists but is paused is fine — Pause clears main_playing.)
        if self.session.transport == TransportState::Playing
            && !has_stream
            && !main_playing
            && !self.pending_start
        {
            eprintln!(
                "plippa-engine: reconcile — session Playing but no audio stream; rolling back"
            );
            let _ = self.session.set_transport(TransportState::Stopped);
            self.emit_status();
        }
        self.poll_seek_completion();
        self.poll_crossings();
        self.poll_scan_end();
        self.sync_neighbours();
        let _ = self.poll_tap_to_audio_metric();
    }

    /// Stop the audio thread and release the sink (SIGINT/SIGTERM path).
    pub fn shutdown_audio(&mut self) {
        if let Some(audio) = self.audio.take() {
            audio.shutdown();
        }
        self.loaded = None;
        self.pending_start = false;
        let _ = self.session.set_transport(TransportState::Stopped);
    }

    /// Drain outbound events (status / notices) since the last call.
    pub fn take_events(&mut self) -> Vec<EngineEvent> {
        std::mem::take(&mut self.events)
    }

    /// Push a fresh [`PlaybackStatus`] without bumping revision (periodic hub
    /// broadcast — position/volume refresh). Hub interval is 500 ms.
    pub fn publish_status(&mut self) {
        self.sync_session_playback_fields();
        self.events
            .push(EngineEvent::Status(PlaybackStatus::from_session_rev(
                self.session.clone(),
                self.status_revision,
            )));
    }

    /// Current status revision (tests / diagnostics).
    pub fn status_revision(&self) -> u64 {
        self.status_revision
    }

    /// Attach the audio routing state machine (AUDIO_01) and its sink target (AUDIO_03).
    /// The board's local-sink values win over the ones the backend was constructed with,
    /// then discovery runs and the first target is pushed.
    pub fn attach_route(&mut self, mut manager: RouteManager, target: RouteTargetCell) {
        if self.local_sink.is_some() || self.local_sink_match.is_some() {
            manager.set_local_sink(self.local_sink.clone(), self.local_sink_match.clone());
        }
        manager.refresh();
        // No stream exists yet on the normal path: a failure here must not arm a
        // recovery re-open for a later unrelated route change.
        let _ = manager.take_apply_failure();
        let notices = manager.take_notices();
        let route = manager.route().clone();
        let intent = manager.intent();
        self.last_route_revision = route.revision;
        self.last_published_route_revision = route.revision;
        self.stream_intent = intent.clone();
        self.route_target = Some(target);
        self.push_route_target(intent.as_ref(), route.revision);
        self.route = Some(manager);
        for n in notices {
            self.events.push(EngineEvent::Notice(n));
        }
        self.events.push(EngineEvent::AudioRoute(route));
    }

    /// Poll the route backend, push a changed target and re-open the stream when the
    /// target identity or profile changed (AUDIO_03 §4.3/§4.4).
    pub fn poll_audio_route(&mut self) {
        let Some(manager) = self.route.as_mut() else {
            return;
        };
        manager.poll();
        let apply_failed = manager.take_apply_failure();
        let notices = manager.take_notices();
        let route = manager.route().clone();
        let intent = manager.intent();
        for n in notices {
            self.events.push(EngineEvent::Notice(n));
        }

        let route_changed = route.revision != self.last_route_revision;
        if route_changed {
            self.last_route_revision = route.revision;
            self.push_route_target(intent.as_ref(), route.revision);
        }
        if route_changed || apply_failed {
            let previous = self.stream_intent.clone();
            self.maybe_restream(previous.as_ref(), intent.as_ref(), apply_failed);
            self.stream_intent = intent;
        }
        if route_changed {
            self.publish_route_if_changed(route);
        }
    }

    /// Publish a route at most once per revision (the command path and the hub tick
    /// both see the same real change).
    fn publish_route_if_changed(&mut self, route: AudioRoute) {
        if route.revision == self.last_published_route_revision {
            return;
        }
        self.last_published_route_revision = route.revision;
        self.events.push(EngineEvent::AudioRoute(route));
    }

    fn push_route_target(&mut self, intent: Option<&RouteIntent>, generation: u64) {
        let Some(cell) = self.route_target.as_ref() else {
            return;
        };
        let (dev, cfg) = match intent {
            Some(intent) => stream_target(intent),
            None => (None, PulseConfig::low_latency()),
        };
        if let Ok(mut target) = cell.lock() {
            *target = RouteTarget {
                dev,
                cfg,
                generation,
            };
        }
    }

    /// Re-open the **loaded** item at the heard position when the route moved to a
    /// different device or profile.
    ///
    /// A route change is not a track change: `loaded`, the list and `resume_store`
    /// stay untouched. `prepare_stream` is the one stream-open path (AUDIO_03 §4.5),
    /// so a paused transport re-opens paused and a playing one resumes at the heard
    /// position. Multi → Multi never re-opens: the combining sink reloads under us and
    /// PipeWire keeps the stream attached by name (AUDIO_03 §4.4). `None` is the
    /// server-default device identity, so the first resolved route also re-opens a
    /// stream that already started on the default. `force` is set once after a failed
    /// apply: the kept target may have a partially changed graph, so re-open it once
    /// even when its identity did not move (AUDIO_03 §4.8/§6.7).
    fn maybe_restream(
        &mut self,
        previous: Option<&RouteIntent>,
        current: Option<&RouteIntent>,
        force: bool,
    ) {
        let (prev_dev, prev_cfg) = stream_target_or_default(previous);
        let (curr_dev, curr_cfg) = stream_target_or_default(current);
        if !force && prev_dev == curr_dev && prev_cfg == curr_cfg {
            return;
        }
        // Nothing loaded means nothing to re-open (attach before the first play).
        let Some((path, item_position)) = self.loaded_stream_source() else {
            return;
        };
        self.sync_session_playback_fields();
        let start_ms = self.session.position_ms;
        let play = (self.session.transport == TransportState::Playing)
            .then_some(PlayArm { not_before: None });
        if let Err(e) = self.prepare_stream(&path, item_position, start_ms, play) {
            self.emit_open_failure(&e);
        }
    }

    fn loaded_stream_source(&self) -> Option<(PathBuf, u32)> {
        let loaded = self.loaded.as_ref()?;
        let index = loaded.list.current_index();
        let track = loaded.list.track(index)?;
        Some((track.media_path.clone()?, track.item_position))
    }

    /// Re-publish the current route (the hub calls this when a client connects).
    pub fn publish_audio_route(&mut self) {
        if let Some(manager) = &self.route {
            let route = manager.route().clone();
            self.events.push(EngineEvent::AudioRoute(route));
        }
    }

    /// Current route (tests / diagnostics).
    pub fn audio_route(&self) -> Option<&AudioRoute> {
        self.route.as_ref().map(RouteManager::route)
    }

    pub fn notices(&self) -> impl Iterator<Item = &UserNotice> {
        self.events.iter().filter_map(|e| match e {
            EngineEvent::Notice(n) => Some(n),
            _ => None,
        })
    }

    fn audio_send(&self, cmd: AudioCommand) {
        if let Some(a) = &self.audio {
            a.try_send(cmd);
        }
    }

    /// Handle one IPC envelope (in-process; UDS framing comes later).
    pub fn handle_ipc(&mut self, msg: IpcMessage) {
        match msg {
            IpcMessage::TapNdef(p) => self.handle_tap_ndef(p),
            IpcMessage::TapKey(p) => self.handle_tap_key(p),
            IpcMessage::PlaybackCommand(cmd) => self.handle_playback_command(cmd),
            IpcMessage::LibraryIndexUpdated(u) => self.handle_library_index_updated(u),
            IpcMessage::PlaybackStatus(_)
            | IpcMessage::UserNotice(_)
            | IpcMessage::AssociateRequest(_)
            | IpcMessage::AssociateResponse(_)
            | IpcMessage::NowPlayingListChanged(_)
            | IpcMessage::ResumeWriteback(_)
            | IpcMessage::UnmatchedNoubin(_)
            | IpcMessage::AudioRouteChanged(_) => {
                // Engine produces status/notices/list/resume; associate is UI↔server.
            }
        }
    }

    fn emit_status(&mut self) {
        self.sync_session_playback_fields();
        self.status_revision = self.status_revision.saturating_add(1);
        self.events
            .push(EngineEvent::Status(PlaybackStatus::from_session_rev(
                self.session.clone(),
                self.status_revision,
            )));
    }

    /// Mirror live audio position/duration (and keep title/artist) into session.
    fn sync_session_playback_fields(&mut self) {
        // WARP_05: instantaneous rate, voice and warp flag come from the audio thread.
        match &self.audio {
            Some(a) => {
                let micro = a.stats.rate_micro.load(Ordering::Relaxed);
                self.session.rate_permille = (f64::from(micro) / 1000.0).round() as i32;
                self.session.warp_active = a.stats.warp_active.load(Ordering::Relaxed);
                self.session.speed_voice = match a.stats.speed_voice.load(Ordering::Relaxed) {
                    1 => plippa_core::VoiceKind::Varispeed,
                    2 => plippa_core::VoiceKind::Stretch,
                    _ => plippa_core::VoiceKind::Bypass,
                };
            }
            None => {
                self.session.rate_permille = RATE_UNITY_PERMILLE;
                self.session.warp_active = false;
                self.session.speed_voice = plippa_core::VoiceKind::Bypass;
            }
        }
        self.session.position_ms = self
            .current_position_ms()
            .unwrap_or(self.session.position_ms);
        if let Some(d) = self.current_duration_ms() {
            self.session.duration_ms = Some(d);
        }
        let (prev_d, next_d) = self.neighbour_durations();
        self.session.prev_duration_ms = prev_d;
        self.session.next_duration_ms = next_d;
        // Current track + cue come from the list and the *media* position.
        match &self.loaded {
            Some(l) => {
                self.session.current_entry_index = l.list.current_index() as u32;
                self.session.current_cue_index = l
                    .list
                    .current_cue_index(self.session.position_ms)
                    .map(|i| i as u32);
                // NB_14: single source of truth for the Previous/Next button LEDs.
                self.session.can_prev = l.list.can_prev();
                self.session.can_next = l.list.can_next(self.session.position_ms);
                // WARP_01: FF/RW usable whenever something is loaded.
                self.session.can_ff = true;
                self.session.can_rw = true;
                // WAVE_01: the item's sidecar path (the UI shows ANALYSING until it exists).
                self.session.waveform_path = if self.extras.waveform_suppressed {
                    None
                } else {
                    let media = l.list.current_track().and_then(|t| t.media_path.as_deref());
                    session_waveform_path(&l.library_root, media)
                };
                // POLISH_02: the collection line follows the playing item (a cued mix inside a
                // playlist / album is appended to the collection name).
                self.session.collection_title = collection_line(&l.record, l.list.current_track());
                // PLAY_02: a dynamic queue has no source order to go back to (the menu says so).
                self.session.source_order = l.list.source_order();
            }
            None => {
                self.session.current_entry_index = 0;
                self.session.current_cue_index = None;
                self.session.can_prev = false;
                self.session.can_next = false;
                self.session.can_ff = false;
                self.session.can_rw = false;
                self.session.waveform_path = None;
                // Nothing loaded: the safe assumption (and what every load produces).
                self.session.source_order = SourceOrder::Fixed;
            }
        }
    }

    /// Apply metadata from a loaded library record into the session snapshot.
    fn apply_loaded_metadata(&mut self, record: &PlayableRecord) {
        self.session.title = record
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_default();
        self.session.artist = record.primary_artist.clone().unwrap_or_default();
        self.session.collection_title = collection_line(record, None);
        self.session.position_ms = 0;
        self.session.duration_ms = None;
    }

    fn emit_warning(&mut self, code: &str, message: impl Into<String>) {
        self.events.push(EngineEvent::Notice(UserNotice {
            severity: NoticeSeverity::Warning,
            code: code.into(),
            message: message.into(),
        }));
    }

    /// An open/seek failure, reported as what it is: a file this player cannot play
    /// ([`NOTICE_MEDIA_UNPLAYABLE`]) or one that could not be opened ([`NOTICE_MEDIA_FILE_MISSING`]).
    fn emit_open_failure(&mut self, e: &EngineError) {
        let (code, message) = open_failure_notice(e, &self.capabilities);
        self.emit_notice(code, message);
    }

    /// [`Self::emit_open_failure`] as a warning for an item that was skipped.
    fn emit_open_failure_warning(&mut self, e: &EngineError, prefix: &str) {
        let (code, message) = open_failure_notice(e, &self.capabilities);
        self.emit_warning(code, format!("{prefix}: {message}"));
    }

    fn emit_notice(&mut self, code: &str, message: impl Into<String>) {
        let notice = UserNotice {
            severity: NoticeSeverity::Error,
            code: code.into(),
            message: message.into(),
        };
        self.events.push(EngineEvent::Notice(notice));
    }

    fn handle_tap_ndef(&mut self, payload: TapNdefPayload) {
        match extract_noubin_key(&payload.ndef_message) {
            Ok(key) => {
                let normalised = normalise_noubin_key(&key);
                // BCS_07: read the platform links once, here, so no consumer has to parse
                // the tag again. Bounded by the protocol's own caps (untrusted tag text).
                let candidate_urls = plippa_noubin::extract_uri_records(
                    &payload.ndef_message,
                    MAX_UNMATCHED_CANDIDATE_URLS,
                    MAX_CANDIDATE_URL_BYTES,
                );
                self.tap_with_normalised_key(normalised, payload.tap_id, Some(key), candidate_urls);
            }
            Err(e) => {
                self.emit_notice(
                    NOTICE_NDEF_INVALID,
                    format!("Tag data could not be read ({e}). Try a URI or Text NDEF record."),
                );
            }
        }
    }

    fn handle_tap_key(&mut self, payload: TapKeyPayload) {
        let (normalised, raw) = if payload.already_normalised {
            (payload.noubin_key, None)
        } else {
            (
                normalise_noubin_key(&payload.noubin_key),
                Some(payload.noubin_key),
            )
        };
        // A key tap (remote control / tests) declares exactly one link: the key itself, when
        // it is a URL that could be offered to a store.
        let candidate_urls = raw
            .as_ref()
            .filter(|key| is_url_noubin_key(key) && key.len() <= MAX_CANDIDATE_URL_BYTES)
            .map(|key| vec![key.clone()])
            .unwrap_or_default();
        self.tap_with_normalised_key(normalised, payload.tap_id, raw, candidate_urls);
    }

    /// Local-first tap path: SQLite only — never contacts the network.
    ///
    /// `raw_key` is the key as read from the tag (`None` when the caller only had a
    /// normalised key); a URL key that misses locally raises [`EngineEvent::UnmatchedNoubin`].
    fn tap_with_normalised_key(
        &mut self,
        normalised: String,
        _tap_id: u64,
        raw_key: Option<String>,
        candidate_urls: Vec<String>,
    ) {
        self.extras.previous_press.clear();
        // Same Noubin while Playing → ignore (§3.2). While Paused → resume + SFX.
        if self.active_normalised_key.as_deref() == Some(normalised.as_str()) {
            match self.session.transport {
                TransportState::Playing => {
                    eprintln!("plippa-engine: same noubin while playing → ignore ({normalised})");
                    return;
                }
                TransportState::Paused => {
                    eprintln!("plippa-engine: same noubin while paused → resume (+SFX)");
                    self.unpause_with_tap_sfx();
                    return;
                }
                TransportState::Stopped => {
                    // Fall through to session resume seek / restart.
                }
            }
        }

        let Some((_, _reader)) = self.readers.first() else {
            self.emit_notice(
                NOTICE_NO_MEDIA_STORAGE,
                format!(
                    "Media library index not ready under {} — waiting for server scan.",
                    self.library_root().display()
                ),
            );
            return;
        };

        let mut found_record: Option<(PathBuf, PlayableRecord)> = None;
        let mut duplicate_note: Option<String> = None;
        for (root, reader) in &self.readers {
            let lookup = match reader.lookup_by_normalised_key(&normalised) {
                Ok(r) => r,
                Err(e) => {
                    self.emit_notice(NOTICE_LIBRARY_MISS, format!("Library lookup failed: {e}"));
                    return;
                }
            };
            if let LookupResult::Found(hit) = lookup {
                found_record = Some((root.clone(), hit.first().clone()));
                // NB_06 policy: a key shared by several items (left by a merge) plays the
                // first by `.noudata` path; the others are named in a warning so the
                // user can fix it (re-associate with a locked tag moves the key).
                if hit.multi_match {
                    let names = |r: &PlayableRecord| -> String {
                        let t = r.title.clone().unwrap_or_else(|| r.noudata_relpath.clone());
                        t.chars().filter(|c| !c.is_control()).take(60).collect()
                    };
                    let others: Vec<String> =
                        hit.records.iter().skip(1).take(5).map(names).collect();
                    duplicate_note = Some(format!(
                        "{} items share this Noubin; playing \"{}\". Also: {}",
                        hit.records.len(),
                        names(hit.first()),
                        others.join(", ")
                    ));
                }
                break;
            }
        }

        match found_record {
            None => {
                self.emit_notice(
                    NOTICE_LIBRARY_MISS,
                    format!(
                        "No local media found for this Noubin ({normalised}). \
                         Add matching files to the media library."
                    ),
                );
                // NB_08 §3.2: only a URL key is an "official Noubin" the player could look up
                // on the web; plain-text keys stay a Private Noubin (associate offer). The
                // engine performs no network I/O — this is just a structured notice.
                if let Some(raw) = raw_key {
                    if is_url_noubin_key(&raw) && raw.len() <= MAX_KEY_BYTES {
                        self.unmatched_seq += 1;
                        self.events.push(EngineEvent::UnmatchedNoubin(UnmatchedNoubin {
                            noubin_key: raw.trim().to_string(),
                            normalised_key: normalised.clone(),
                            seq: self.unmatched_seq,
                            candidate_urls,
                        }));
                    }
                }
            }
            Some((root, record)) => {
                // Leaving another playable — bookmark it before switching.
                if self.active_normalised_key.as_deref() != Some(normalised.as_str()) {
                    self.snapshot_resume_bookmark(false);
                }
                if let Err(e) = self.load_record_from_tap(root, record, normalised) {
                    self.emit_open_failure(&e);
                }
                if let Some(msg) = duplicate_note {
                    self.emit_warning(NOTICE_DUPLICATE_KEY, msg);
                }
            }
        }
    }

    /// Unpause after a same-Noubin tap: always fire tap SFX when configured.
    fn unpause_with_tap_sfx(&mut self) {
        if self.loaded.is_none() {
            return;
        }
        let with_sfx = if let Some(sfx) = self.load_sfx.clone() {
            self.audio_send(AudioCommand::TriggerSfx(sfx));
            true
        } else {
            false
        };
        let not_before = with_sfx.then(|| Instant::now() + SFX_COVER_BEFORE_MAIN);
        // Pulse may have dropped the stream while paused — re-arm at pause position.
        if !self.has_main_stream() {
            let start_ms = self.current_position_ms().unwrap_or(0);
            if let Err(e) = self.prepare_current_track(start_ms, Some(PlayArm { not_before })) {
                self.emit_open_failure(&e);
                return;
            }
        } else {
            self.audio_send(AudioCommand::Play { not_before });
        }
        // UI_20: armed behind the tap's SFX cover the playhead does not move yet; the engine's
        // published rate is still the pre-pause one until the audio thread holds it at zero, and
        // the status below carries it. Zero it here (same reason as a (re)start).
        if not_before.is_some_and(|deadline| deadline > Instant::now()) {
            if let Some(a) = self.audio.as_ref() {
                a.stats.rate_micro.store(0, Ordering::Relaxed);
            }
            self.session.rate_permille = 0;
        }
        if self.session.set_transport(TransportState::Playing).is_ok() {
            self.emit_status();
        }
    }

    fn has_main_stream(&self) -> bool {
        self.audio
            .as_ref()
            .is_some_and(|a| a.stats.has_main_stream.load(Ordering::SeqCst))
    }

    fn handle_playback_command(&mut self, cmd: PlaybackCommand) {
        // SEAM_01: an item change the audio made by itself may still be waiting for the hub
        // tick. Apply it first, so Next / Previous / a seek act on the item being heard. (Not
        // for a play-mode change: PLAY_02 resolves an event queued across a reorder by the
        // item's identity, and that path stays the one exercised.)
        if matches!(
            cmd,
            PlaybackCommand::Next
                | PlaybackCommand::NextTrack
                | PlaybackCommand::Previous
                | PlaybackCommand::PreviousTrack
                | PlaybackCommand::JumpToEntry { .. }
                | PlaybackCommand::SeekMs(_)
                | PlaybackCommand::SeekTo { .. }
                | PlaybackCommand::SeekToItem { .. }
                | PlaybackCommand::Pause
                | PlaybackCommand::Stop
        ) {
            self.poll_crossings();
        }
        // PLAY_03: a segment-boundary pause-after-each stop holds its segment for the next
        // Play. Only Play consumes it; any other command (a nav press, a seek, Stop, a load)
        // drops it so Play cannot resurrect an old segment.
        if !matches!(&cmd, PlaybackCommand::Play) {
            self.extras.held_segment_start_ms = None;
        }
        // NB_04b: only Previous itself (and Play/Pause/volume) keep the "recent Previous" state.
        if !matches!(
            cmd,
            PlaybackCommand::Previous
                | PlaybackCommand::PreviousTrack
                | PlaybackCommand::Play
                | PlaybackCommand::Pause
                | PlaybackCommand::SetVolume(_)
                // WARP_01: speed family and scan commands do not break the window
                // (`SeekTo` does: it is not listed).
                | PlaybackCommand::SetRate { .. }
                | PlaybackCommand::ResetRate
                | PlaybackCommand::ResetRateInstant
                | PlaybackCommand::SetSpeedWarpMode(_)
                | PlaybackCommand::SetSeekWarpMode(_)
                | PlaybackCommand::ScanStart { .. }
                | PlaybackCommand::ScanStop
                | PlaybackCommand::ScanRate { .. }
        ) {
            self.extras.previous_press.clear();
        }
        match cmd {
            PlaybackCommand::Play => {
                if self.loaded.is_none() {
                    return;
                }
                // WAVE_01: playback of the retained item makes its sidecar path current again.
                self.extras.waveform_suppressed = false;
                // Stop → UI/web Play starts from 0. Paused / lost stream → re-arm
                // at the current position so Pause→Play keeps working after resume.
                let rearm =
                    self.session.transport == TransportState::Stopped || !self.has_main_stream();
                if rearm {
                    let start_ms = match self.session.transport {
                        // PLAY_03: a pause-after-each segment boundary queued this segment
                        // start; Play reopens exactly it. Every other Stopped entry (a user
                        // Stop, the end of the list, the held next item) starts at 0:00.
                        TransportState::Stopped => {
                            self.extras.held_segment_start_ms.take().unwrap_or(0)
                        }
                        _ => self.current_position_ms().unwrap_or(0),
                    };
                    if let Err(e) =
                        self.prepare_current_track(start_ms, Some(PlayArm { not_before: None }))
                    {
                        self.emit_open_failure(&e);
                        return;
                    }
                } else {
                    self.audio_send(AudioCommand::Play { not_before: None });
                }
                if self.session.set_transport(TransportState::Playing).is_ok() {
                    self.emit_status();
                }
            }
            PlaybackCommand::Pause => {
                self.session.scan = ScanState::None;
                self.extras.scan_stop_pending = false;
                if self.session.set_transport(TransportState::Paused).is_ok() {
                    self.snapshot_resume_bookmark(false);
                    self.maybe_writeback_resume(false);
                    self.audio_send(AudioCommand::Pause);
                    self.emit_status();
                }
            }
            PlaybackCommand::Stop => {
                self.session.scan = ScanState::None;
                self.extras.scan_return_stop = false;
                self.extras.scan_stop_pending = false;
                self.snapshot_resume_bookmark(false);
                self.maybe_writeback_resume(false);
                let _ = self.session.set_transport(TransportState::Stopped);
                self.audio_send(AudioCommand::Stop);
                // Keep `loaded` / `current_item` so Play (and the physical play/stop
                // button) can restart without a fresh LoadAndPlay.
                self.emit_status();
            }
            PlaybackCommand::Unload => {
                // Stop without bookmarking, then forget all session resume state so
                // the next noubin tap starts from 0 for every remembered key.
                let _ = self.session.set_transport(TransportState::Stopped);
                self.audio_send(AudioCommand::Stop);
                self.resume_store.clear();
                // WARP_01: tempo is session-wide but ends with Unload; warp modes survive.
                self.session.target_rate_permille = RATE_UNITY_PERMILLE;
                // WAVE_01: nothing is presented as current after Unload, though `loaded`
                // stays so Play can restart it (the next load clears the suppression).
                self.extras.waveform_suppressed = true;
                self.session.waveform_path = None;
                self.extras.settling = false;
                self.audio_send(AudioCommand::SetRate {
                    permille: RATE_UNITY_PERMILLE,
                    change: RateChange::Immediate,
                });
                self.clear_seek_family();
                eprintln!("plippa-engine: Unload — stopped and cleared session resume bookmarks");
                self.emit_status();
            }
            PlaybackCommand::SetVolume(v) => {
                self.session.volume = v;
                let linear = v.as_linear();
                if let Some(a) = &self.audio {
                    a.volume.set(linear);
                }
                self.audio_send(AudioCommand::SetVolume(linear));
                self.emit_status();
            }
            PlaybackCommand::Load(id) => {
                let opts = LoadOpts {
                    autoplay: false,
                    with_sfx: false,
                    resume: ResumeSource::None,
                };
                if let Err(e) = self.load_by_id(id.as_u64(), opts) {
                    self.emit_open_failure(&e);
                }
            }
            PlaybackCommand::LoadCollection { playable_id } => {
                eprintln!("plippa-engine: LoadCollection id={}", playable_id.as_u64());
                let opts = LoadOpts {
                    autoplay: true,
                    with_sfx: self.load_sfx.is_some(),
                    resume: ResumeSource::Persisted,
                };
                if let Err(e) = self.load_by_id(playable_id.as_u64(), opts) {
                    eprintln!("plippa-engine: LoadCollection failed: {e}");
                    self.emit_open_failure(&e);
                }
            }
            PlaybackCommand::LoadAndPlay(id) => {
                eprintln!("plippa-engine: LoadAndPlay id={}", id.as_u64());
                let opts = LoadOpts {
                    autoplay: true,
                    with_sfx: self.load_sfx.is_some(),
                    resume: ResumeSource::None,
                };
                match self.load_by_id(id.as_u64(), opts) {
                    Ok(()) => {
                        eprintln!(
                            "plippa-engine: LoadAndPlay ok → {:?}",
                            self.session.transport
                        );
                        if self.sink_is_mock {
                            eprintln!(
                                "plippa-engine: WARNING: Playing on mock sink — no real audio. \
                                 Rebuild with `cargo build -p plippa-engine --features pulse`"
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("plippa-engine: LoadAndPlay failed: {e}");
                        self.emit_open_failure(&e);
                    }
                }
            }
            PlaybackCommand::SeekMs(ms) => {
                if let Err(e) = self.seek_current(ms) {
                    self.emit_open_failure(&e);
                }
            }
            PlaybackCommand::Next | PlaybackCommand::NextTrack => {
                let action = self
                    .loaded
                    .as_ref()
                    .map(|l| l.list.next(self.media_position_now()));
                let Some(action) = action else { return };
                let action = self.resolve_wrap(action);
                self.nav_clearing_seek_family(action, true);
            }
            PlaybackCommand::Previous | PlaybackCommand::PreviousTrack => {
                let Some(l) = &self.loaded else { return };
                let subsequent = self.extras.previous_press.press(self.extras.clock.now());
                let action = l.list.previous(self.media_position_now(), subsequent);
                self.nav_clearing_seek_family(action, false);
            }
            PlaybackCommand::JumpToEntry { index } => {
                let Some(l) = &self.loaded else { return };
                let forward = index as usize > l.list.current_flat_index();
                let action = l.list.jump_to_entry(index as usize);
                self.nav_clearing_seek_family(action, forward);
            }
            PlaybackCommand::SetCueListEnabled {
                list_index,
                enabled,
            } => {
                // PLAY_03: a cue edit rebuilds the segments and the permutation; the segment
                // under the playhead stays first.
                let pos = self.media_position_now();
                let changed = if let Some(l) = self.loaded.as_mut() {
                    l.list.note_cue_anchor(pos);
                    l.list.set_cue_list_enabled(list_index as usize, enabled);
                    true
                } else {
                    false
                };
                if changed {
                    self.emit_now_playing();
                    self.publish_status();
                }
            }
            PlaybackCommand::SetRate { permille } => self.handle_set_rate(permille, false),
            PlaybackCommand::ResetRate => self.handle_set_rate(RATE_UNITY_PERMILLE, true),
            PlaybackCommand::ResetRateInstant => self.handle_reset_rate_instant(),
            PlaybackCommand::SetSpeedWarpMode(m) => {
                if self.session.speed_warp_mode != m {
                    self.session.speed_warp_mode = m;
                    self.audio_send(AudioCommand::SetSpeedWarpMode(m));
                    self.emit_status();
                }
            }
            PlaybackCommand::SetSeekWarpMode(m) => {
                if self.session.seek_warp_mode != m {
                    self.session.seek_warp_mode = m;
                    self.emit_status();
                }
            }
            PlaybackCommand::ScanStart { dir } => {
                if self.loaded.is_none() {
                    eprintln!("plippa-engine: ScanStart ignored — nothing loaded");
                    return;
                }
                let scan = ScanState::from(dir);
                // RW with nothing behind the playhead while not playing: no effect. FF while
                // Stopped opens the stream (paused) at the start and scans from there.
                if self.session.transport != TransportState::Playing
                    && dir == plippa_core::ScanDir::Backward
                    && self.session.seek_target_ms.is_none()
                    && self
                        .current_position_ms()
                        .unwrap_or(self.session.position_ms)
                        == 0
                {
                    return;
                }
                // Seek exclusivity (T11): a scan cancels a pending committed chase (the audio
                // thread cancels the swoop itself and starts from the rate it had).
                let had_target = self.session.seek_target_ms.take().is_some();
                self.extras.seek_to_marker = None;
                if self.session.scan == scan {
                    if had_target {
                        self.emit_status();
                    }
                    return;
                }
                if self.session.transport == TransportState::Stopped && !self.has_main_stream() {
                    if let Err(e) = self.prepare_current_track(0, None) {
                        self.emit_open_failure(&e);
                        return;
                    }
                    self.extras.scan_return_stop = true;
                }
                self.session.scan = scan;
                self.extras.scan_cmds_sent += 1;
                self.audio_send(AudioCommand::ScanStart {
                    dir,
                    mode: self.session.seek_warp_mode,
                });
                if self.session.seek_outside_track {
                    self.send_neighbours();
                }
                self.emit_status();
            }
            PlaybackCommand::ScanStop => {
                if self.session.scan != ScanState::None {
                    self.session.scan = ScanState::None;
                    self.audio_send(AudioCommand::ScanStop);
                    // UI_15: no brake. The audio thread snaps to the set speed (or pauses on
                    // the exact heard position) before it acknowledges, so the status is
                    // published by `poll_scan_end` with that **settled** position: one status,
                    // no intermediate value for the UI to jump to.
                    self.extras.scan_stop_pending = self.audio.is_some();
                    if !self.extras.scan_stop_pending {
                        self.emit_status();
                    }
                }
            }
            // UI_15: the encoder's scan speed while FF/RW is held. Scan-only: it re-targets the
            // live scan and never touches `target_rate_permille` (what the release restores).
            PlaybackCommand::ScanRate { permille } => {
                if !plippa_core::scan_speed_valid(permille) {
                    eprintln!("plippa-engine: ScanRate {permille} rejected (not a ladder entry)");
                    return;
                }
                if self.session.scan == ScanState::None {
                    return;
                }
                self.audio_send(AudioCommand::ScanSpeed { permille });
            }
            PlaybackCommand::SeekTo { target_ms } => self.handle_seek_to(target_ms),
            PlaybackCommand::SetSeekOutsideTrack(on) => {
                if self.session.seek_outside_track != on {
                    self.session.seek_outside_track = on;
                    if self.session.scan != ScanState::None {
                        self.send_neighbours();
                    }
                    self.emit_status();
                }
            }
            PlaybackCommand::SeekToItem {
                item_delta,
                target_ms,
            } => self.handle_seek_to_item(item_delta, target_ms),
            // PLAY_02: the play-mode settings. All four are engine memory, pushed into the
            // loaded list; none of them prepares a stream.
            PlaybackCommand::SetOrderMode(m) => {
                // A UI reconcile sending the current value must never reshuffle.
                if m == self.session.order_mode {
                    return;
                }
                self.extras.shuffle_seed = self.extras.shuffle_seed.wrapping_add(SHUFFLE_SEED_STEP);
                let seed = self.extras.shuffle_seed;
                let pos = self.media_position_now();
                // Audiobooks and podcasts keep their chapter order: the setting is stored for
                // the next album, but this list is not reordered (owner decision 8).
                let (outcome, order_applies) = match self.loaded.as_mut() {
                    Some(l) if l.list.order_applies() => {
                        let outcome = l.list.set_order(m, seed);
                        // PLAY_03: cue-level shuffle follows the same setting. The heard segment
                        // stays first (a list operation; no seek, no prepare).
                        l.list.note_cue_anchor(pos);
                        l.list.set_cue_shuffle(m == OrderMode::Shuffle, seed);
                        (outcome, true)
                    }
                    Some(_) => (OrderOutcome::Unchanged, false),
                    None => (OrderOutcome::TooShort, false),
                };
                if outcome == OrderOutcome::NoFixedOrder {
                    self.emit_warning(
                        NOTICE_NO_FIXED_ORDER,
                        "Dynamic playlist, no fixed order".to_string(),
                    );
                }
                self.session.order_mode = m;
                self.extras.previous_press.clear();
                if self.session.scan != ScanState::None {
                    self.send_neighbours();
                }
                // A list with nothing to reorder (one track) still republishes when its cue
                // rows moved: cue-level shuffle changes the order inside that one item.
                let cue_rows_moved = self
                    .loaded
                    .as_ref()
                    .is_some_and(|l| l.list.cue_shuffle_applies());
                if order_applies && (outcome != OrderOutcome::TooShort || cue_rows_moved) {
                    self.emit_now_playing();
                }
                self.emit_status();
            }
            PlaybackCommand::SetRepeatMode(m) => {
                self.session.repeat_mode = m;
                if let Some(l) = self.loaded.as_mut() {
                    l.list.set_repeat(m);
                }
                self.emit_status();
            }
            PlaybackCommand::SetPauseAfterEach(on) => {
                self.session.pause_after_each = on;
                if let Some(l) = self.loaded.as_mut() {
                    l.list.set_pause_after_each(on);
                }
                self.emit_status();
            }
            PlaybackCommand::SetReshuffleOnRepeat(on) => {
                self.session.reshuffle_on_repeat = on;
                self.emit_status();
            }
            // AUDIO_01: the route manager owns validation and resolution; the engine
            // only forwards the command and republishes the route (revision unchanged
            // when nothing actually changed).
            PlaybackCommand::SetAudioOutputs { enabled, local_only } => {
                let notices = match self.route.as_mut() {
                    Some(manager) => manager.set_outputs(enabled, local_only),
                    None => Vec::new(),
                };
                self.queue_route_notices(notices);
            }
            PlaybackCommand::SetAudioDelayAuto(auto) => {
                let notices = match self.route.as_mut() {
                    Some(manager) => manager.set_delay_auto(auto),
                    None => Vec::new(),
                };
                self.queue_route_notices(notices);
            }
            PlaybackCommand::SetOutputDelay { output, delay_ms } => {
                let notices = match self.route.as_mut() {
                    Some(manager) => manager.set_output_delay(&output, delay_ms),
                    None => Vec::new(),
                };
                self.queue_route_notices(notices);
            }
        }
    }

    fn queue_route_notices(&mut self, notices: Vec<UserNotice>) {
        for n in notices {
            self.events.push(EngineEvent::Notice(n));
        }
        if let Some(route) = self.route.as_ref().map(|m| m.route().clone()) {
            // The hub tick's `poll_audio_route` sees the same revision; publishing
            // here keeps the command response immediate without sending it twice.
            self.publish_route_if_changed(route);
        }
    }

    /// Would reaching `index` from the current item wrap the queue **and** start a fresh
    /// shuffled pass? (A long-form list wraps in chapter order, never reshuffled.)
    fn wrap_reshuffles(&self, index: usize) -> bool {
        match self.loaded.as_ref() {
            Some(l) => {
                index <= l.list.current_index()
                    && l.list.order_applies()
                    && self.session.order_mode == OrderMode::Shuffle
                    && self.session.reshuffle_on_repeat
            }
            None => false,
        }
    }

    /// SEAM_01: what the end of the current item leads to when the file is over and no
    /// repeat-item / pause-after-each rule intervenes: the next available track (a repeat-queue
    /// wrap included), or nothing at the end of the list.
    ///
    /// **One** place decides, for the automatic advance (`commit`: a wrap that reshuffles
    /// starts its fresh pass, the list is changed) and for the seamless join (`!commit`: a pure
    /// look, nothing changes). The look cannot know the first item of a pass that is not
    /// shuffled yet, so a wrap that reshuffles answers `None`: that boundary keeps the
    /// ordinary advance.
    fn next_item_action(&mut self, commit: bool) -> NavAction {
        let next = match &self.loaded {
            Some(l) => l.list.next_track_action(),
            None => NavAction::None,
        };
        if commit {
            return self.resolve_wrap(next);
        }
        match next {
            NavAction::Track { index, .. } if self.wrap_reshuffles(index) => NavAction::None,
            other => other,
        }
    }

    /// PLAY_02: resolve a repeat-queue wrap **before** the caller acts on it.
    ///
    /// `next_track_action` returning a track at or before the current index means it wrapped.
    /// With a shuffled list and reshuffle-on-repeat on, the wrap starts a fresh pass over every
    /// track (never the one that just finished) and the caller plays its first item. List-only:
    /// no stream command, no prepare, no playhead change.
    fn resolve_wrap(&mut self, action: NavAction) -> NavAction {
        let (index, skipped) = match action {
            NavAction::Track { index, skipped } => (index, skipped),
            other => return other,
        };
        if !self.wrap_reshuffles(index) {
            // A long-form list (audiobook / podcast) wraps in chapter order, never reshuffled.
            return NavAction::Track { index, skipped };
        }
        self.extras.shuffle_seed = self.extras.shuffle_seed.wrapping_add(SHUFFLE_SEED_STEP);
        let seed = self.extras.shuffle_seed;
        let restarted = match self.loaded.as_mut() {
            Some(l) => l.list.start_new_pass(seed),
            None => false,
        };
        if !restarted {
            // A one-track queue repeat just replays that track.
            return NavAction::Track { index, skipped };
        }
        // The fresh pass is a visible state change in its own right. `skipped` indexes the
        // *old* order, so it is meaningless now: drop it. The caller publishes the list after
        // navigating, when `current` and the cues describe the same entry.
        self.status_revision = self.status_revision.saturating_add(1);
        NavAction::Track {
            index: 0,
            skipped: Vec::new(),
        }
    }

    /// WARP_19: the item a scan/jog may cross into (what `Next` / `Previous` would select),
    /// resolved to a file. Unplayable candidates are marked unavailable (and reported, like a
    /// skipped track) so the next one is tried; `None` at the end of a non-repeating list.
    fn neighbour_item(&mut self, forward: bool) -> Option<(usize, PathBuf)> {
        for _ in 0..16 {
            let idx = self.loaded.as_ref()?.list.neighbour_track(forward)?;
            match self.resolve_track_path(idx) {
                Ok(path) => return Some((idx, path)),
                Err(e) => {
                    let title = self
                        .loaded
                        .as_mut()
                        .and_then(|l| l.list.track_mut(idx))
                        .map(|t| {
                            t.mark_unavailable();
                            t.title.clone()
                        })
                        .unwrap_or_default();
                    self.emit_open_failure_warning(&e, &format!("Skipped \"{title}\""));
                }
            }
        }
        None
    }

    /// Tell the audio thread its neighbours.
    ///
    /// `next` is, in this order: the item that plays next at the end of this one when the audio
    /// may join it at normal speed (SEAM_01, [`Self::seamless_next`]: always, whatever the
    /// "Seek outside track" setting), else what `Next` would select while a scan runs and the
    /// setting is on (WARP_19). `prev` is only ever for a scan. Both are cleared when nothing
    /// is loaded.
    fn send_neighbours(&mut self) {
        let scan_on =
            self.session.seek_outside_track && self.loaded.is_some() && self.session.scan != ScanState::None;
        let mk = |p: PathBuf, idx: usize, e: &Self| NeighbourSrc {
            path: p,
            capabilities: e.capabilities.clone(),
            overspec: e.overspec,
            item_position: e
                .loaded
                .as_ref()
                .and_then(|l| l.list.track(idx))
                .map(|t| t.item_position)
                .unwrap_or(0),
            seamless: false,
            scan: true,
        };
        let (prev, scan_next) = if scan_on {
            let prev = self.neighbour_item(false).map(|(i, p)| mk(p, i, self));
            let next = self.neighbour_item(true).map(|(i, p)| mk(p, i, self));
            (prev, next)
        } else {
            (None, None)
        };
        let next = self.seamless_next().or(scan_next);
        self.audio_send(AudioCommand::SetNeighbours { prev, next });
        self.extras.neighbours_sent = self.neighbour_key();
    }

    /// SEAM_01: the item the audio thread may join behind the current one at normal speed.
    ///
    /// `Some` only when the end of this item really leads to a *different file* that the
    /// automatic advance would play: not repeat-item, not pause-after-each, no cue-permutation
    /// segment left (that is a seek inside the open file, never a join), and the decision of
    /// [`Self::next_item_action`] is a track. A file that cannot be resolved is simply not
    /// offered, silently: the ordinary advance reports it if it really is unplayable.
    fn seamless_next(&mut self) -> Option<NeighbourSrc> {
        let (index, _) = self.seamless_next_index()?;
        let path = self.peek_track_path(index)?;
        let l = self.loaded.as_ref()?;
        let current = l.list.current_track().and_then(|t| t.media_path.as_deref());
        if current == Some(path.as_path()) {
            return None; // the same file again: nothing to join
        }
        let item_position = l.list.track(index)?.item_position;
        Some(NeighbourSrc {
            path,
            capabilities: self.capabilities.clone(),
            overspec: self.overspec,
            item_position,
            seamless: true,
            // A scan may cross into it only with the setting on (otherwise it ends with the
            // track, as it always did).
            scan: self.session.seek_outside_track,
        })
    }

    /// The list-side half of [`Self::seamless_next`]: the next track's index and the tracks
    /// stepped over to reach it. No file access.
    fn seamless_next_index(&mut self) -> Option<(usize, Vec<usize>)> {
        let (repeat, pause_after_each, current) = {
            let l = self.loaded.as_ref()?;
            (l.list.repeat(), l.list.pause_after_each(), l.list.current_index())
        };
        if repeat == RepeatMode::Item || pause_after_each || self.cue_segment_remaining() {
            return None;
        }
        match self.next_item_action(false) {
            NavAction::Track { index, skipped } if index != current => Some((index, skipped)),
            _ => None,
        }
    }

    /// A cue-shuffled item still has a segment of its permutation to play (`Next` would seek).
    fn cue_segment_remaining(&self) -> bool {
        self.cue_shuffle_active()
            && self
                .loaded
                .as_ref()
                .is_some_and(|l| matches!(l.list.next(self.media_position_now()), NavAction::SeekMs(_)))
    }

    /// The media file of list track `index` as [`Self::resolve_track_path`] would pick it,
    /// without changing the list (no track is marked unavailable, nothing is reported).
    fn peek_track_path(&self, index: usize) -> Option<PathBuf> {
        let track = self.loaded.as_ref()?.list.track(index)?;
        if let Some(p) = track.media_path.as_ref() {
            return p.is_file().then(|| p.clone());
        }
        match track.media_candidates.as_slice() {
            [] => None,
            [only] => only.is_file().then(|| only.clone()),
            many => select_best_local_path(many, &self.capabilities, self.overspec).ok(),
        }
    }

    /// What the neighbours depend on right now (`None` without a loaded list or audio).
    fn neighbour_key(&mut self) -> Option<NeighbourKey> {
        self.audio.as_ref()?;
        let (list_id, current, scan_prev, scan_next) = {
            let l = self.loaded.as_ref()?;
            let ident = |i: usize| l.list.track(i).map(|t| (i, t.item_position));
            (
                l.list_id,
                l.list.current_index(),
                l.list.neighbour_track(false).and_then(ident),
                l.list.neighbour_track(true).and_then(ident),
            )
        };
        let seamless = self.seamless_next_index().and_then(|(i, _)| {
            let l = self.loaded.as_ref()?;
            l.list.track(i).map(|t| (i, t.item_position))
        });
        Some(NeighbourKey {
            starts_sent: self.extras.starts_sent,
            list_id,
            current,
            seamless,
            scan_prev,
            scan_next,
            scanning: self.session.scan != ScanState::None,
            seek_outside_track: self.session.seek_outside_track,
        })
    }

    /// SEAM_01: keep the audio thread's neighbours in step with the list. Run from the hub
    /// tick: a new stream, a track change, a reorder, a play-mode switch or a scan starting
    /// or ending all change the key, and exactly then the neighbours are sent again.
    fn sync_neighbours(&mut self) {
        let Some(key) = self.neighbour_key() else {
            return;
        };
        if self.extras.neighbours_sent.as_ref() != Some(&key) {
            self.send_neighbours();
        }
    }

    /// WARP_19: duration of list item `idx`: the list's own value, else the file's (probed once
    /// per file and remembered; control thread, only for a neighbour item).
    fn item_duration_ms(&mut self, idx: usize) -> Option<u64> {
        let track = self.loaded.as_ref()?.list.track(idx)?;
        if let Some(d) = track.duration_ms.filter(|d| *d > 0) {
            return Some(d);
        }
        let path = track.media_path.clone().or_else(|| match track.media_candidates.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        })?;
        if let Some(cached) = self.extras.probed_durations.get(&path) {
            return *cached;
        }
        let d = isolated_open(&self.library_roots, &path, &self.capabilities, self.overspec)
            .ok()
            .and_then(|d| d.duration_ms())
            .filter(|d| *d > 0);
        self.extras.probed_durations.insert(path, d);
        d
    }

    /// WARP_19: durations of the neighbours for the UI's live jog target.
    fn neighbour_durations(&mut self) -> (Option<u64>, Option<u64>) {
        if !self.session.seek_outside_track || self.loaded.is_none() {
            return (None, None);
        }
        let mut out = [None, None];
        for (slot, fwd) in out.iter_mut().zip([false, true]) {
            let idx = self.loaded.as_ref().and_then(|l| l.list.neighbour_track(fwd));
            *slot = idx.and_then(|i| self.item_duration_ms(i));
        }
        (out[0], out[1])
    }

    /// WARP_19: the audio thread's scan crossed a track boundary: do what `Next` / `Previous`
    /// do to the engine state except restarting audio (current item, cues, now playing, the
    /// `Previous` window, the neighbours of the new item). Resume write-back stays with the
    /// transport actions (pause/stop write the item that is current then).
    fn apply_crossing(&mut self, ev: CrossEvent) {
        if ev.generation != self.extras.starts_sent {
            return; // an older stream's event
        }
        let forward = ev.dir > 0;
        // PLAY_02: the event names the item the listener actually entered, by its stable
        // source position. Resolve that, not "whatever this list's neighbour is now": a
        // reorder during the scan may already have moved the item (or the event may have been
        // queued across the reorder). The old neighbour derivation is only the fallback for an
        // event whose position is not in the current list (a load raced the drain; the
        // generation check above normally filters those).
        let idx = self
            .loaded
            .as_ref()
            .and_then(|l| l.list.index_of_item(ev.item_position))
            .or_else(|| {
                self.loaded
                    .as_ref()
                    .and_then(|l| l.list.neighbour_track(forward))
            });
        let Some(idx) = idx else {
            return;
        };
        if ev.seamless && forward {
            // SEAM_01: the audio went on into the next item by itself (no scan). Everything
            // `go_to_track` does for an automatic advance, except starting the audio: the
            // tracks stepped over are reported, the item becomes current with its cues, the
            // position restarts at 0:00 and the list is republished.
            let skipped = match self.loaded.as_ref().map(|l| l.list.next_track_action()) {
                Some(NavAction::Track { index, skipped }) if index == idx => skipped,
                _ => Vec::new(),
            };
            self.warn_skipped(&skipped);
            match self.enter_item(idx) {
                Ok(cue_source) => self.load_item_cues(cue_source, 0),
                Err(_) => return,
            }
            self.session.position_ms = 0;
        } else {
            self.extras.previous_press.clear();
            if let Some(l) = self.loaded.as_mut() {
                l.list.set_current(idx);
            }
            self.load_current_cues();
            self.session.position_ms = if forward { 0 } else { ev.new_duration_ms };
        }
        // The neighbours of the item entered (the audio thread dropped its own with the
        // exchange of the feeds): seamless playback goes on across the next boundary too.
        self.send_neighbours();
        self.emit_status();
        self.emit_now_playing();
    }

    /// Drain the audio thread's crossing events (and report neighbours it could not open).
    fn poll_crossings(&mut self) {
        let (events, failures) = match &self.audio {
            Some(a) => (
                a.stats
                    .cross_events
                    .lock()
                    .map(|mut g| std::mem::take(&mut *g))
                    .unwrap_or_default(),
                a.stats.cross_failures.load(Ordering::SeqCst),
            ),
            None => return,
        };
        if failures > self.extras.cross_failures_seen {
            self.extras.cross_failures_seen = failures;
            // The audio thread only counts the failure; ask the same capability gate why.
            let neighbour_paths: Vec<PathBuf> = [false, true]
                .into_iter()
                .filter_map(|fwd| {
                    let list = &self.loaded.as_ref()?.list;
                    let track = list.track(list.neighbour_track(fwd)?)?;
                    track
                        .media_path
                        .clone()
                        .or_else(|| track.media_candidates.first().cloned())
                })
                .collect();
            let reasons: Vec<(&'static str, String)> = neighbour_paths
                .iter()
                .filter_map(|path| {
                    isolated_validate(&self.library_roots, path, &self.capabilities, self.overspec).err()
                })
                .map(|e| open_failure_notice(&EngineError::Audio(e), &self.capabilities))
                .collect();
            match reasons.iter().find(|(code, _)| *code == NOTICE_MEDIA_UNPLAYABLE) {
                Some((_, message)) => self.emit_warning(
                    NOTICE_MEDIA_UNPLAYABLE,
                    format!("Seek outside track: the neighbouring item cannot be played: {message}"),
                ),
                None => self.emit_warning(
                    NOTICE_MEDIA_FILE_MISSING,
                    "Seek outside track: the neighbouring item could not be opened",
                ),
            }
        }
        for ev in events {
            self.apply_crossing(ev);
        }
    }

    /// `SeekToItem`: the jog commit that crossed a boundary (see `handle_seek_to` for the
    /// in-item case). Needs the setting on, no scan, and a neighbour that opens; otherwise
    /// it is a clamped `SeekTo` in the current item.
    fn handle_seek_to_item(&mut self, item_delta: i8, target_ms: u64) {
        if self.loaded.is_none() {
            return;
        }
        let clamp_here = |e: &mut Self| e.handle_seek_to(if item_delta < 0 { 0 } else { u64::MAX });
        if item_delta == 0 {
            return self.handle_seek_to(target_ms);
        }
        if self.session.scan != ScanState::None {
            eprintln!("plippa-engine: SeekToItem ignored while a scan is running");
            return;
        }
        if !self.session.seek_outside_track {
            return clamp_here(self);
        }
        let Some((idx, path)) = self.neighbour_item(item_delta > 0) else {
            return clamp_here(self);
        };
        let duration = self.item_duration_ms(idx);
        let target = clamp_seek_target_ms(target_ms, duration);
        // The `Next` / `Previous` state changes: seek family, current item, cues.
        self.clear_seek_family();
        if let Some(l) = self.loaded.as_mut() {
            l.list.set_current(idx);
        }
        let item_position = self
            .loaded
            .as_ref()
            .and_then(|l| l.list.track(idx))
            .map(|t| t.item_position)
            .unwrap_or(0);
        self.load_current_cues();
        let play = (self.session.transport == TransportState::Playing)
            .then_some(PlayArm { not_before: None });
        if let Err(e) = self.prepare_stream_with(&path, item_position, target, play, true) {
            self.emit_open_failure(&e);
            return;
        }
        self.session.position_ms = target;
        self.emit_status();
        self.emit_now_playing();
    }

    /// `SetRate` / `ResetRate`: validate, store the set speed, hand it to the audio thread
    /// (which ramps toward it) and publish. A reset uses the slower reset ramp. The set speed is
    /// sent even when it did not change so a freshly started audio thread state stays in step.
    fn handle_set_rate(&mut self, permille: i32, reset: bool) {
        if !rate_permille_valid(permille) {
            eprintln!(
                "plippa-engine: SetRate {permille} rejected (valid: 100..=2000 or -2000..=-100)"
            );
            return;
        }
        if self.session.target_rate_permille != permille {
            self.session.target_rate_permille = permille;
            self.extras.settling = permille == RATE_UNITY_PERMILLE;
            self.audio_send(AudioCommand::SetRate {
                permille,
                change: if reset {
                    RateChange::Reset
                } else {
                    RateChange::Tempo
                },
            });
            self.emit_status();
        }
    }

    /// `ResetRateInstant` (UI_14): the FF/RW-hold + Play gesture. Back to 100 % **now**: the
    /// audio thread jumps the rate with no ramp and drops a scan / chase where it stands, and
    /// the control plane forgets the seek family so the picture is at the live position too.
    /// Sent even when the set speed is already 1000 so a running scan/chase is still dropped.
    fn handle_reset_rate_instant(&mut self) {
        self.clear_seek_family();
        self.session.target_rate_permille = RATE_UNITY_PERMILLE;
        self.extras.settling = false;
        self.audio_send(AudioCommand::SnapRate {
            permille: RATE_UNITY_PERMILLE,
        });
        self.emit_status();
    }

    /// Mirror the audio thread's tape bounce (reverse reached 0:00 and now runs forward): the
    /// set speed becomes its magnitude. Called from `poll_stream_lifecycle`.
    fn poll_rate_bounce(&mut self) {
        let Some(audio) = &self.audio else { return };
        let bounces = audio.stats.rate_bounces.load(Ordering::SeqCst);
        if bounces != self.extras.bounces_seen {
            self.extras.bounces_seen = bounces;
            let forward = self.session.target_rate_permille.abs();
            if self.session.target_rate_permille != forward {
                self.session.target_rate_permille = forward;
                self.emit_status();
            }
        }
    }

    /// `SeekTo`: clamp, publish `seek_target_ms` and hand the commit to the audio thread
    /// (`AudioCommand::SeekChase`: the smoothed jump, or a silent seek while paused).
    fn handle_seek_to(&mut self, target_ms: u64) {
        if self.loaded.is_none() {
            return;
        }
        if self.session.scan != ScanState::None {
            eprintln!("plippa-engine: SeekTo ignored while a scan is running");
            return;
        }
        let duration = self.current_duration_ms().or(self.session.duration_ms);
        let clamped = clamp_seek_target_ms(target_ms, duration);
        self.session.seek_target_ms = Some(clamped);
        if let Err(e) = self.seek_hard(clamped, true) {
            self.session.seek_target_ms = None;
            self.extras.seek_to_marker = None;
            self.emit_open_failure(&e);
        }
    }

    /// Clear scan and the committed seek target (Unload, track change). Returns whether
    /// anything changed; the caller publishes status.
    fn clear_seek_family(&mut self) -> bool {
        let changed =
            self.session.scan != ScanState::None || self.session.seek_target_ms.is_some();
        self.session.scan = ScanState::None;
        self.session.seek_target_ms = None;
        self.extras.seek_to_marker = None;
        self.extras.scan_return_stop = false;
        changed
    }

    /// `Next` / `Previous` / `JumpToEntry`: clear scan and target, run the navigation, and make
    /// sure a status goes out even when the navigation itself was a no-op.
    fn nav_clearing_seek_family(&mut self, action: NavAction, forward: bool) {
        let changed = self.clear_seek_family();
        let rev = self.status_revision;
        self.apply_nav(action, forward);
        if changed && self.status_revision == rev {
            self.emit_status();
        }
    }

    /// True when the periodic status refresh should run at the fast cadence (100 ms): the set
    /// speed is not 1.0x, a scan runs, a committed seek is in progress, or a cue-shuffled list
    /// may hit a segment boundary (PLAY_03).
    pub fn wants_fast_status(&self) -> bool {
        self.session.target_rate_permille != RATE_UNITY_PERMILLE
            || (self.extras.settling && self.audio_warp_active())
            || self.session.scan != ScanState::None
            || self.session.seek_target_ms.is_some()
            || self.cue_shuffle_active()
    }

    /// WARP_17: a scan that ended by itself (the audio thread ran into the track end/start, or
    /// rejected the start) clears the published scan; a scan that was started from Stopped
    /// sends the transport back to Stopped once the audio thread has finished it.
    ///
    /// UI_15: a released scan publishes its **settled** position here (the audio thread has
    /// already snapped the rate back to the set speed, or paused on the heard position), so the
    /// release is one status with a final position.
    fn poll_scan_end(&mut self) {
        let Some(a) = &self.audio else { return };
        let over = a.stats.scans_handled.load(Ordering::SeqCst) >= self.extras.scan_cmds_sent
            && !a.stats.scan_active.load(Ordering::SeqCst);
        if !over {
            return;
        }
        let mut changed = false;
        if self.session.scan != ScanState::None {
            self.session.scan = ScanState::None;
            changed = true;
        }
        if self.extras.scan_stop_pending {
            self.extras.scan_stop_pending = false;
            changed = true;
        }
        if self.extras.scan_return_stop {
            self.extras.scan_return_stop = false;
            if self.session.transport == TransportState::Stopped {
                self.audio_send(AudioCommand::Stop);
            }
            changed = true;
        }
        if changed {
            self.emit_status();
        }
    }

    /// Clear `seek_target_ms` once the audio thread has applied the seek that stands in for
    /// the chase; publishes status when it clears.
    fn poll_seek_completion(&mut self) {
        let Some((starts, seeks)) = self.extras.seek_to_marker else {
            return;
        };
        let done = match &self.audio {
            Some(a) => {
                // Applied by the audio thread and, for a chase, landed (or the stream is gone).
                a.stats.starts_completed.load(Ordering::SeqCst) >= starts
                    && a.stats.seeks_completed.load(Ordering::SeqCst) >= seeks
                    && (!a.stats.chase_active.load(Ordering::SeqCst)
                        || !a.stats.has_main_stream.load(Ordering::SeqCst))
            }
            None => true,
        };
        if done {
            self.extras.seek_to_marker = None;
            if self.session.seek_target_ms.take().is_some() {
                self.emit_status();
            }
        }
    }

    fn handle_library_index_updated(&mut self, update: LibraryIndexUpdated) {
        // Skip stale *and* duplicate generations — reopen is expensive on USB.
        if update.generation <= self.index_generation {
            return;
        }
        self.index_generation = update.generation;
        eprintln!(
            "plippa-engine: library index gen={} ({} items) — reopening readers",
            update.generation, update.item_count
        );
        let (roots, readers, notice) = open_roots(self.override_root.clone());
        self.library_roots = roots;
        self.readers = readers;
        if let Some(n) = notice {
            self.events.push(EngineEvent::Notice(n));
        }
    }

    /// Retry the readers that could not be opened (another process still opening the index:
    /// `LibraryError::Busy`; or the index not created yet), with backoff 2 s growing to 30 s,
    /// until every root has one. Without this a stable library would never produce a newer
    /// `LibraryIndexUpdated` generation and the engine would stay at "index not ready" for
    /// ever. No notice per attempt; one "ready" notice when the readers are back.
    pub fn poll_library_retry(&mut self) {
        if self.library_roots.is_empty() || self.readers.len() >= self.library_roots.len() {
            self.library_retry.reset();
            return;
        }
        let now = Instant::now();
        match self.library_retry.next_at {
            None => {
                self.library_retry.next_at = Some(now + self.library_retry.delay);
                return;
            }
            Some(at) if now < at => return,
            Some(_) => {}
        }
        let (roots, readers, _notice) = open_roots(self.override_root.clone());
        let complete = !roots.is_empty() && readers.len() >= roots.len();
        self.library_roots = roots;
        self.readers = readers;
        if complete {
            self.library_retry.reset();
            self.events.push(EngineEvent::Notice(UserNotice {
                severity: NoticeSeverity::Info,
                code: NOTICE_NO_MEDIA_STORAGE.into(),
                message: "Media library index is ready.".into(),
            }));
        } else {
            self.library_retry.delay = (self.library_retry.delay * 2).min(LIBRARY_RETRY_MAX);
            self.library_retry.next_at = Some(now + self.library_retry.delay);
        }
    }

    fn load_by_id(&mut self, global_id: u64, opts: LoadOpts) -> Result<(), EngineError> {
        if self.readers.is_empty() {
            return Err(EngineError::Message("Media library index not ready".into()));
        }
        let (ns, local) = decode_playable_id(global_id);
        for (root, reader) in &self.readers {
            if library_namespace(root) != ns {
                continue;
            }
            let record = reader
                .lookup_by_id(local)?
                .ok_or_else(|| EngineError::Message(format!("No playable with id {global_id}")))?;
            let key = record.noubin_key_normalised.clone();
            let root = root.clone();
            return self.load_collection(root, record, key, opts);
        }
        // Compat: single library + raw local id (high bits 0 or unmatched ns).
        if self.readers.len() == 1 {
            let (root, reader) = &self.readers[0];
            if let Some(record) = reader.lookup_by_id(global_id as i64)? {
                let key = record.noubin_key_normalised.clone();
                let root = root.clone();
                return self.load_collection(root, record, key, opts);
            }
        }
        Err(EngineError::Message(format!(
            "No playable with id {global_id}"
        )))
    }

    /// Noubin tap path — session resume when a bookmark exists, else the playable's
    /// persisted `resumeAtItem` / `resumeFromTime`.
    ///
    /// Every tap plays SFX when configured (user setting later).
    fn load_record_from_tap(
        &mut self,
        library_root: PathBuf,
        record: PlayableRecord,
        normalised_key: String,
    ) -> Result<(), EngineError> {
        let with_sfx = self.load_sfx.is_some();
        let resume = if let Some(entry) = self.resume_store.get(&normalised_key).cloned() {
            if entry.should_restart() {
                self.resume_store.remove(&normalised_key);
                eprintln!(
                    "plippa-engine: resume restart key={normalised_key} track={} (finished/tail)",
                    entry.track_index
                );
                ResumeSource::Session {
                    item: entry.track_index as u32,
                    ms: 0,
                }
            } else {
                eprintln!(
                    "plippa-engine: resume seek key={normalised_key} track={} at {}ms (+SFX)",
                    entry.track_index, entry.position_ms
                );
                ResumeSource::Session {
                    item: entry.track_index as u32,
                    ms: entry.position_ms,
                }
            }
        } else {
            ResumeSource::Persisted
        };
        let opts = LoadOpts {
            autoplay: true,
            with_sfx,
            resume,
        };
        self.load_collection(library_root, record, Some(normalised_key), opts)
    }

    /// Build the `nowPlayingList` for `record`, **replace** the current one and start
    /// (or just prepare) the first track.
    fn load_collection(
        &mut self,
        library_root: PathBuf,
        record: PlayableRecord,
        normalised_key: Option<String>,
        opts: LoadOpts,
    ) -> Result<(), EngineError> {
        self.extras.previous_press.clear();
        self.clear_seek_family();
        let built = {
            let reader = self
                .readers
                .iter()
                .find(|(r, _)| *r == library_root)
                .map(|(_, rd)| rd)
                .ok_or_else(|| EngineError::Message("Media library index not ready".into()))?;
            build_collection(
                &library_root,
                reader,
                &record,
                &self.capabilities,
                self.overspec,
            )?
        };
        let BuiltCollection { mut list, prefs } = built;
        if !list.tracks().iter().any(|t| t.available()) {
            return Err(EngineError::Message(
                ".noudata file found but media not found".into(),
            ));
        }
        // Where to start (mediaList position + ms). Keyed by `item_position`, so it is
        // shuffle-safe and can still be resolved after the ordering below.
        let (start_item, start_ms) = match opts.resume {
            ResumeSource::None => (None, 0),
            ResumeSource::Session { item, ms } => (Some(item), ms),
            ResumeSource::Persisted => persisted_start(&list, &prefs),
        };

        // PLAY_02: the play mode is the player setting, applied to every new list.
        // 1. Any load clears repeat-item (owner decision 4).
        if self.session.repeat_mode == RepeatMode::Item {
            self.session.repeat_mode = RepeatMode::Off;
        }
        // 2. The list carries the session's repeat / pause-after-each flags.
        list.set_repeat(self.session.repeat_mode);
        list.set_pause_after_each(self.session.pause_after_each);
        // 3. Audiobooks and podcasts ignore the order for this list only (their chapters are
        //    always in order — owner decision 8); the session setting is kept for the next
        //    album. The predicate is shared with the mid-playback / wrap paths so the guard
        //    cannot diverge.
        // 4. Shuffle on: "start here, shuffle the rest" (the fixed-order rule).
        if list.order_applies() && self.session.order_mode == OrderMode::Shuffle {
            self.extras.shuffle_seed = self.extras.shuffle_seed.wrapping_add(SHUFFLE_SEED_STEP);
            list.shuffle_keep_item_first(self.extras.shuffle_seed, start_item);
        }
        // 5. PLAY_03: cue-level shuffle follows the same setting for a mix / lone file (the
        //    list type decides — audiobooks and podcasts keep their chapters). The cues arrive
        //    with the current track, so this only arms the mode; the permutation is built when
        //    `set_cues` runs, anchored on the starting segment.
        list.set_cue_shuffle(
            self.session.order_mode == OrderMode::Shuffle && list.order_applies(),
            self.extras.shuffle_seed,
        );
        // 6. A load replaces the list: allocate its identity.
        let list_id = self.extras.next_list_id;
        self.extras.next_list_id = self.extras.next_list_id.wrapping_add(1);

        let (start_index, start_ms) = match start_item.and_then(|p| list.index_of_item(p)) {
            Some(i) => (i, start_ms),
            None => (0, 0),
        };

        // Leaving a long-form playable: persist where it stopped.
        if self.loaded.is_some() {
            self.maybe_writeback_resume(false);
        }

        let global_id = plippa_noubin::encode_playable_id(&library_root, record.id);
        self.apply_loaded_metadata(&record);
        // WAVE_01: a fresh load presents its sidecar path again after an Unload.
        self.extras.waveform_suppressed = false;
        self.session.position_ms = start_ms;
        self.loaded = Some(LoadedPlayable {
            record: record.clone(),
            library_root,
            list,
            list_id,
        });

        let play = if opts.autoplay {
            self.tap_load_started = Some(Instant::now());
            // SFX is pre-decoded WAV PCM — fire immediately so the cue covers
            // main-track open. Every noubin tap uses SFX when configured.
            let sfx_armed = if opts.with_sfx {
                if let Some(sfx) = self.load_sfx.clone() {
                    self.audio_send(AudioCommand::TriggerSfx(sfx));
                    true
                } else {
                    false
                }
            } else {
                false
            };
            let not_before = sfx_armed.then(|| Instant::now() + SFX_COVER_BEFORE_MAIN);
            Some(PlayArm { not_before })
        } else {
            None
        };
        self.start_track_skipping(start_index, start_ms, play, true)?;
        self.session.current_item = Some(PlayableItemId::new(global_id));
        self.active_normalised_key = normalised_key.or(record.noubin_key_normalised.clone());
        let _ = self.session.set_transport(if opts.autoplay {
            TransportState::Playing
        } else {
            TransportState::Stopped
        });
        self.emit_status();
        self.emit_now_playing();
        Ok(())
    }

    /// Start track `index`; if its media turns out to be missing/unplayable, warn and
    /// step to the next playable one (`forward`) instead of stalling the list.
    fn start_track_skipping(
        &mut self,
        mut index: usize,
        mut start_ms: u64,
        play: Option<PlayArm>,
        forward: bool,
    ) -> Result<(), EngineError> {
        loop {
            match self.try_start_track(index, start_ms, play.clone()) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let Some(l) = self.loaded.as_mut() else {
                        return Err(e);
                    };
                    let title = l
                        .list
                        .track(index)
                        .map(|t| t.title.clone())
                        .unwrap_or_default();
                    if let Some(t) = l.list.track_mut(index) {
                        t.mark_unavailable();
                    }
                    match l.list.available_from(index, forward) {
                        Some(next) => {
                            self.emit_open_failure_warning(&e, &format!("Skipped \"{title}\""));
                            index = next;
                            start_ms = 0;
                        }
                        None => return Err(e),
                    }
                }
            }
        }
    }

    /// Make `index` current: resolve its file, hand it to the audio thread, load its cues.
    fn try_start_track(
        &mut self,
        index: usize,
        start_ms: u64,
        play: Option<PlayArm>,
    ) -> Result<(), EngineError> {
        let path = self.resolve_track_path(index)?;
        let cue_source = self.enter_item(index)?;
        let item_position = self
            .loaded
            .as_ref()
            .and_then(|l| l.list.track(index))
            .map(|t| t.item_position)
            .unwrap_or(0);
        self.prepare_stream(&path, item_position, start_ms, play)?;
        self.load_item_cues(cue_source, start_ms);
        Ok(())
    }

    /// The list-side half of switching to `index`, shared by starting its audio
    /// ([`Self::try_start_track`]) and by a seamless crossing that the audio thread already
    /// made ([`Self::apply_crossing`]): the scan and the committed seek target end (tempo and
    /// modes are kept) and `index` becomes current. Returns its cue source.
    fn enter_item(&mut self, index: usize) -> Result<Option<(i64, u32)>, EngineError> {
        self.clear_seek_family();
        let l = self.loaded.as_mut().ok_or(EngineError::NothingLoaded)?;
        l.list.set_current(index);
        Ok(l.list.track(index).and_then(|t| t.cue_source))
    }

    /// Cues of the new current track replace the old track's (one `.noudata` read, control
    /// thread, only on a track change); the other half of [`Self::enter_item`].
    fn load_item_cues(&mut self, cue_source: Option<(i64, u32)>, start_ms: u64) {
        if let (Some(src), Some(l)) = (cue_source, self.loaded.as_ref()) {
            if let Some((_, reader)) = self.readers.iter().find(|(r, _)| *r == l.library_root) {
                let cues = load_cues(reader, src);
                if let Some(l) = self.loaded.as_mut() {
                    l.list.set_cues(cues);
                    // PLAY_03: the permutation starts from the segment being opened (the first
                    // one, or the resume cue), not from whichever cue happens to be at 0.
                    l.list.set_cue_anchor(start_ms);
                }
            }
        }
    }

    /// Absolute media file of list track `index`. A single existing file is used as is;
    /// several alternates are ranked once (Symphonia probe) and the choice cached.
    fn resolve_track_path(&mut self, index: usize) -> Result<PathBuf, EngineError> {
        let l = self.loaded.as_mut().ok_or(EngineError::NothingLoaded)?;
        let missing = |t: &plippa_core::ListTrack| {
            EngineError::Message(format!(
                ".noudata file found but media not found ({})",
                t.title
            ))
        };
        let track = l.list.track_mut(index).ok_or(EngineError::NothingLoaded)?;
        if let Some(p) = track.media_path.clone() {
            if p.is_file() {
                return Ok(p);
            }
            let err = missing(track);
            track.mark_unavailable();
            return Err(err);
        }
        match track.media_candidates.len() {
            0 => Err(missing(track)),
            1 => {
                let p = track.media_candidates[0].clone();
                track.media_path = Some(p.clone());
                Ok(p)
            }
            _ => {
                let p = select_best_local_path(
                    &track.media_candidates,
                    &self.capabilities,
                    self.overspec,
                )?;
                track.media_path = Some(p.clone());
                Ok(p)
            }
        }
    }

    /// (Re-)open the **current** track at `start_ms` — used when the stream is gone
    /// (Stopped → Play, lost stream, seek with no stream). Does not touch cues.
    fn prepare_current_track(
        &mut self,
        start_ms: u64,
        play: Option<PlayArm>,
    ) -> Result<(), EngineError> {
        let index = self
            .loaded
            .as_ref()
            .ok_or(EngineError::NothingLoaded)?
            .list
            .current_index();
        let item_position = self
            .loaded
            .as_ref()
            .and_then(|l| l.list.track(index))
            .map(|t| t.item_position)
            .unwrap_or(0);
        let path = self.resolve_track_path(index)?;
        self.prepare_stream(&path, item_position, start_ms, play)
    }

    /// Hand path to the audio thread (no whole-file decode on control plane).
    ///
    /// Extension gate only here; Symphonia probe+open runs once on the audio thread.
    /// When `play` is `Some`, the audio thread arms Playing only after a successful open.
    fn prepare_stream(
        &mut self,
        path: &Path,
        item_position: u32,
        start_ms: u64,
        play: Option<PlayArm>,
    ) -> Result<(), EngineError> {
        self.prepare_stream_with(path, item_position, start_ms, play, false)
    }

    /// [`Self::prepare_stream`]; `fade_old` fades the audio of the replaced stream under the
    /// new one (WARP_19 jog into a neighbouring item).
    fn prepare_stream_with(
        &mut self,
        path: &Path,
        item_position: u32,
        start_ms: u64,
        play: Option<PlayArm>,
        fade_old: bool,
    ) -> Result<(), EngineError> {
        debug_assert!(
            self.audio_thread_id()
                .map(|id| id != thread::current().id())
                .unwrap_or(true),
            "prepare_stream must not run on the audio thread"
        );
        // Extension gate only — full probe+open is a single pass on the audio
        // thread (avoids a second USB open on the Play/tap hot path).
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .ok_or_else(|| {
                EngineError::Message(format!("unsupported media path {}", path.display()))
            })?;
        if !plippa_audio::formats::format_enabled(&self.capabilities, &ext) {
            return Err(EngineError::Audio(
                plippa_audio::AudioError::FormatNotEnabled {
                    path: path.to_path_buf(),
                    ext,
                },
            ));
        }
        // SECFIX_05: open the file **once**, confined beneath the library root, and hand that
        // handle to the decoder through the audio thread. The old code checked the path here
        // (and the index resolution had checked it again) and then dropped the handle, letting
        // the helper re-open by path — a check-then-reopen that a swapped directory wins.
        let file = plippa_core::safe_fs::open_media_for_decode_in(&self.library_roots, path).map_err(
            |source| match source.kind() {
                std::io::ErrorKind::NotFound => EngineError::Message(format!(
                    ".noudata file found but media not found ({})",
                    path.display()
                )),
                _ => EngineError::Audio(plippa_audio::AudioError::Io {
                    path: path.to_path_buf(),
                    source,
                }),
            },
        )?;
        // A file that crashed the media helper before is refused here, without a spawn. The
        // stamp comes from the handle we are about to hand over, not from a second `stat`.
        if Isolation::global().is_marked(path, &file) {
            return Err(EngineError::Audio(plippa_audio::AudioError::Unplayable {
                path: path.to_path_buf(),
                reason: "it crashed the media decoder earlier".into(),
            }));
        }
        eprintln!(
            "plippa-engine: stream {} — chunked native-rate start_ms={start_ms} autoplay={}",
            path.display(),
            play.is_some()
        );
        self.pending_start = true;
        self.extras.starts_sent += 1;
        self.extras.pending_target_ms = Some(start_ms);
        // Any (re)start supersedes a committed seek (`SeekTo` re-sets it after calling us).
        self.extras.seek_to_marker = None;
        self.session.seek_target_ms = None;
        let (capabilities, overspec) = (self.capabilities.clone(), self.overspec);
        let path = path.to_path_buf();
        self.audio_send(if fade_old {
            AudioCommand::SwitchItem {
                path,
                file,
                capabilities,
                overspec,
                start_ms,
                play,
                item_position,
            }
        } else {
            AudioCommand::StartMainStream {
                path,
                file,
                capabilities,
                overspec,
                start_ms,
                play,
                item_position,
            }
        });
        // UI_20: a (re)start anchors the playhead at `start_ms`, which does not move until the
        // new stream's first write. The published rate still describes the *previous* stream
        // until the audio thread takes the command, and the status sent right after a tap
        // carries that stale rate: a UI that dead-reckons the playhead from it ran a status
        // interval of false playback and then jumped back to the anchored position. Zero it
        // here (the audio thread publishes the real rate with its first write).
        if let Some(a) = self.audio.as_ref() {
            a.stats.rate_micro.store(0, Ordering::Relaxed);
        }
        self.session.rate_permille = 0;
        // Non-blocking (MVP_14): hub must return immediately. Open/decode errors
        // surface via [`Self::poll_stream_lifecycle`] → `NOTICE_DECODE_FAILED`.
        Ok(())
    }

    /// Seek inside the current track. With an open (or opening) stream this is a plain
    /// audio-thread seek — **no re-prepare**; only a dropped stream is re-opened at `ms`.
    fn seek_current(&mut self, ms: u64) -> Result<(), EngineError> {
        self.seek_hard(ms, false)
    }

    /// Hard seek shared by `SeekMs` (`from_seek_to == false`: the pending target is reported
    /// as `position_ms` until applied, any committed target is dropped) and `SeekTo`
    /// (`true`: `position_ms` stays the actual playhead; `seek_target_ms` is cleared by
    /// [`Self::poll_seek_completion`] once the seek has landed).
    fn seek_hard(&mut self, ms: u64, from_seek_to: bool) -> Result<(), EngineError> {
        if self.loaded.is_none() {
            return Ok(());
        }
        let clamped = match self.current_duration_ms().or(self.session.duration_ms) {
            Some(dur) if dur > 0 => ms.min(dur),
            _ => ms,
        };
        // PLAY_03: the playhead moves, so the segment under it becomes the anchor a later cue
        // edit keeps first. List-only: the order is not rebuilt here.
        if let Some(l) = self.loaded.as_mut() {
            l.list.note_cue_anchor(clamped);
        }
        if !from_seek_to {
            self.session.position_ms = clamped;
            self.session.seek_target_ms = None;
            self.extras.seek_to_marker = None;
        }
        if self.has_main_stream() || self.pending_start {
            self.extras.seeks_sent += 1;
            self.extras.pending_target_ms = Some(clamped);
            if from_seek_to {
                // The commit chase (WARP_14): a swoop while playing, a silent seek otherwise.
                // The seek warp mode is sampled here: a later change applies to the next seek.
                self.audio_send(AudioCommand::SeekChase {
                    target_ms: clamped,
                    mode: self.session.seek_warp_mode,
                });
            } else {
                self.audio_send(AudioCommand::SeekMs(clamped));
            }
        } else {
            // Stopped / stream dropped — re-open at the seek point.
            let play = if self.session.transport == TransportState::Playing {
                Some(PlayArm { not_before: None })
            } else {
                None
            };
            self.prepare_current_track(clamped, play)?;
        }
        if from_seek_to {
            self.extras.seek_to_marker = Some((self.extras.starts_sent, self.extras.seeks_sent));
        }
        self.emit_status();
        Ok(())
    }

    /// Execute a navigation decision from the list.
    fn apply_nav(&mut self, action: NavAction, forward: bool) {
        match action {
            NavAction::None => {}
            NavAction::SeekMs(ms) => {
                if let Err(e) = self.seek_current(ms) {
                    self.emit_open_failure(&e);
                }
            }
            NavAction::Track { index, skipped } => {
                self.warn_skipped(&skipped);
                if let Err(e) = self.go_to_track(index, forward) {
                    self.emit_open_failure(&e);
                }
            }
        }
    }

    fn warn_skipped(&mut self, skipped: &[usize]) {
        let titles: Vec<String> = match &self.loaded {
            Some(l) => skipped
                .iter()
                .filter_map(|i| l.list.track(*i).map(|t| t.title.clone()))
                .collect(),
            None => return,
        };
        for t in titles {
            self.emit_warning(
                NOTICE_MEDIA_FILE_MISSING,
                format!("Skipped \"{t}\": media not found"),
            );
        }
    }

    /// PLAY_02: after stop-at-EOF, present the queued rewind at 0:00 until Play re-opens the
    /// item. The audio thread's last position (the item's end) is no playhead once the stream is
    /// gone, and a seek on a dropped stream is a no-op; the counters are written by a live
    /// stream only, so nothing races this.
    fn rewind_published_position(&self) {
        self.anchor_published_position(0);
    }

    /// Point the published position counters at `at_ms` (zero frames written), for a stopped
    /// stream whose live counters still describe audio that will never be heard: the queued
    /// rewind at EOF, or a segment held by a boundary pause (PLAY_03).
    fn anchor_published_position(&self, at_ms: u64) {
        let Some(a) = &self.audio else { return };
        a.stats.position_base_ms.store(at_ms, Ordering::Relaxed);
        for counter in [&a.stats.position_src_frames, &a.stats.frames_written] {
            counter.store(0, Ordering::Relaxed);
        }
    }

    /// Switch to another track of the list (playing iff currently Playing).
    fn go_to_track(&mut self, index: usize, forward: bool) -> Result<(), EngineError> {
        let play = if self.session.transport == TransportState::Playing {
            Some(PlayArm { not_before: None })
        } else {
            None
        };
        self.start_track_skipping(index, 0, play, forward)?;
        self.session.position_ms = 0;
        self.emit_status();
        self.emit_now_playing();
        Ok(())
    }

    /// PLAY_03: is cue-level shuffle live on the loaded list?
    ///
    /// True only for a mix — or a lone file — whose order setting is Shuffle and that has at
    /// least two enabled navigable cues; albums, playlists, audiobooks and podcasts reorder
    /// whole tracks instead ([`NowPlayingList::cue_shuffle`], the shared long-form rule).
    fn cue_shuffle_active(&self) -> bool {
        self.loaded.as_ref().is_some_and(|l| l.list.cue_shuffle())
    }

    /// PLAY_03: start of the segment the playhead is in — what repeat-item loops and
    /// pause-after-each stops on. `None` when cue-level shuffle is off; the caller then uses
    /// the item's own start (the pre-PLAY_03 behaviour).
    fn current_segment_start_ms(&self) -> Option<u64> {
        let l = self.loaded.as_ref()?;
        if !l.list.cue_shuffle() {
            return None;
        }
        l.list.current_segment_start_ms(self.media_position_now())
    }

    /// PLAY_03: the segment under the playhead reached its end — seek to the permutation's next
    /// segment, loop it, stop on it, or hand the item over.
    ///
    /// The playhead is the **live** one ([`Self::media_position_now`]), not the published status
    /// position: the hub ticks every 5 ms, so the boundary is seen inside the
    /// [`plippa_core::CUE_BOUNDARY_LEAD_MS`] window and no audio of the next chronological
    /// segment is heard (at most that much of the tail is cut).
    ///
    /// Nothing happens while a scan, a chase, a pending start or an unlanded seek is in flight:
    /// a slow jog through a boundary must not jump without warning.
    fn poll_cue_boundaries(&mut self) {
        if self.session.transport != TransportState::Playing || !self.cue_shuffle_active() {
            return;
        }
        if self.pending_start
            || self.session.scan != ScanState::None
            || self.session.seek_target_ms.is_some()
        {
            return;
        }
        let (stream_open, writing, chasing, seeks_landed) = match &self.audio {
            Some(a) => (
                a.stats.has_main_stream.load(Ordering::SeqCst),
                a.stats.main_playing.load(Ordering::SeqCst),
                a.stats.chase_active.load(Ordering::SeqCst),
                a.stats.seeks_completed.load(Ordering::SeqCst) >= self.extras.seeks_sent,
            ),
            None => return,
        };
        if !stream_open || !writing || chasing || !seeks_landed {
            return;
        }
        let pos = self.media_position_now();
        let (start, end, repeat, pause_after_each, next) = match self.loaded.as_ref() {
            Some(l) => (
                l.list.current_segment_start_ms(pos),
                l.list.current_segment_end_ms(pos),
                l.list.repeat(),
                l.list.pause_after_each(),
                l.list.next(pos),
            ),
            None => return,
        };
        let (Some(start), Some(end)) = (start, end) else {
            // No boundary: the segment runs to EOF, where `main_finished` takes over.
            return;
        };
        if pos.saturating_add(plippa_core::CUE_BOUNDARY_LEAD_MS) < end {
            return;
        }
        // 1. Repeat-item loops this segment — a seek on the open stream, never a prepare.
        if repeat == RepeatMode::Item {
            if let Err(e) = self.seek_current(start) {
                self.emit_open_failure(&e);
            }
            self.emit_status();
            return;
        }
        // 2. Pause-after-each stops *before* the next segment: it becomes the current segment at
        //    its start with the transport Stopped, so one Play starts exactly it — the same
        //    shape as the end-of-item pause (the finished segment is not replayed). Pause first:
        //    the stream must not keep writing past the boundary for a tick.
        if pause_after_each {
            if let NavAction::SeekMs(target) = next {
                self.audio_send(AudioCommand::Pause);
                self.extras.held_segment_start_ms = Some(target);
                if let Err(e) = self.seek_current(target) {
                    self.emit_open_failure(&e);
                }
                let _ = self.session.set_transport(TransportState::Stopped);
                self.emit_status();
                return;
            }
            // The last segment of the permutation: the item is over. The item-end path below
            // holds the next item at 0:00 (or stops at the end of the list).
        }
        match next {
            // 3. Another segment of the permutation: seek to it inside the same stream.
            NavAction::SeekMs(target) => {
                if end.saturating_sub(start) < CUE_BOUNDARY_MIN_SEGMENT_MS {
                    return;
                }
                if let Err(e) = self.seek_current(target) {
                    self.emit_open_failure(&e);
                }
                self.emit_status();
            }
            // 4. The last segment of the permutation: the item is over here, even with minutes
            //    of audio left in the file. Hand over exactly as at EOF — never play on.
            NavAction::Track { .. } | NavAction::None => self.advance_past_current_item(),
        }
    }

    /// The main stream reached EOF while Playing.
    fn on_stream_finished(&mut self) {
        self.advance_past_current_item();
    }

    /// The current item is over: the stream reached EOF, or the last segment of the
    /// cue-shuffle permutation ended while the file still had audio left (PLAY_03 §3.1).
    ///
    /// One path for both, so repeat-item / pause-after-each / repeat-queue wrap / end of list
    /// cannot diverge between "the file ended" and "the permutation did". Repeat-item and
    /// pause-after-each act on the **segment being heard**; with cue-level shuffle off that is
    /// the item's own start, i.e. the pre-PLAY_03 behaviour.
    fn advance_past_current_item(&mut self) {
        let (repeat, pause_after_each) = match &self.loaded {
            Some(l) => (l.list.repeat(), l.list.pause_after_each()),
            None => (RepeatMode::Off, false),
        };
        let segment_start = self.current_segment_start_ms().unwrap_or(0);
        // 1. Repeat-item: the same segment plays again (a pause-after-each stop rewinds it
        //    without playing, so Play repeats it rather than moving on). A plain seek: with the
        //    stream still open (a mid-file boundary) it is a seek; at EOF it re-opens the item
        //    exactly as `go_to_track` used to.
        if repeat == RepeatMode::Item {
            if pause_after_each {
                let _ = self.session.set_transport(TransportState::Stopped);
                self.session.position_ms = segment_start;
                self.extras.held_segment_start_ms = (segment_start > 0).then_some(segment_start);
                self.audio_send(AudioCommand::SeekMs(segment_start));
                // The live counters still describe the item that just ended; anchor them on the
                // held segment so the status says exactly what Play will do.
                self.anchor_published_position(segment_start);
                self.emit_status();
                return;
            }
            match self.seek_current(segment_start) {
                Ok(()) => {
                    self.emit_status();
                    return;
                }
                Err(e) => self.emit_open_failure(&e),
            }
        }
        // 2-3. Resolve a repeat-queue wrap *before* the pause-after-each check, so a pause at the
        //      wrap holds the first item of the new pass (PLAY_02 §5.4).
        let next = self.next_item_action(true);
        match next {
            NavAction::Track { index, skipped } => {
                self.warn_skipped(&skipped);
                if pause_after_each {
                    // Pause between items (classroom / exercise use): the next item becomes
                    // current at 0:00 with its cues loaded and the transport Stopped, so one
                    // Play starts exactly it. The stream stays as the audio thread left it; Play
                    // opens the queued item (no prepare here — asserted by
                    // `pause_after_each_stops_before_the_next_item`).
                    self.snapshot_resume_bookmark(false);
                    let cue_index = index;
                    if let Some(l) = self.loaded.as_mut() {
                        l.list.set_current(cue_index);
                    }
                    self.load_current_cues();
                    self.session.position_ms = 0;
                    // The paused audio stream still belongs to the finished item, so its live
                    // counters would publish 0:03 against the new current item. Rewind them so
                    // the status says exactly what Play will do: the next item at its start.
                    self.rewind_published_position();
                    let _ = self.session.set_transport(TransportState::Stopped);
                    self.emit_status();
                    self.emit_now_playing();
                    return;
                }
                match self.go_to_track(index, true) {
                    Ok(()) => return,
                    Err(e) => self.emit_open_failure(&e),
                }
            }
            NavAction::None | NavAction::SeekMs(_) => {}
        }
        // End of the list.
        self.snapshot_resume_bookmark(true);
        self.maybe_writeback_resume(true);
        let _ = self.session.set_transport(TransportState::Stopped);
        self.emit_status();
    }

    /// Reload the current track's cues (after switching current without preparing).
    fn load_current_cues(&mut self) {
        let Some(l) = self.loaded.as_ref() else {
            return;
        };
        let src = l.list.current_track().and_then(|t| t.cue_source);
        let cues = match (src, self.readers.iter().find(|(r, _)| *r == l.library_root)) {
            (Some(src), Some((_, reader))) => load_cues(reader, src),
            _ => Vec::new(),
        };
        if let Some(l) = self.loaded.as_mut() {
            l.list.set_cues(cues);
        }
    }

    /// Queue a `NowPlayingListChanged` for every client.
    fn emit_now_playing(&mut self) {
        let Some(l) = &self.loaded else { return };
        let mut entries = l.list.entries();
        entries.truncate(MAX_PUBLISHED_ENTRIES);
        self.events
            .push(EngineEvent::NowPlaying(NowPlayingListChanged {
                entries,
                current_index: l.list.current_flat_index() as u32,
                list_kind: l.list.kind(),
                // PLAY_02: the identity of this *load* — unchanged by reorders and track
                // changes (PLAY_04 keeps the Up Next overlay open across a reshuffle).
                list_id: l.list_id,
            }));
    }

    /// Re-publish the current list (hub calls this when a client connects).
    pub fn publish_now_playing(&mut self) {
        self.emit_now_playing();
    }

    /// The `nowPlayingList` display entries (tests / diagnostics).
    pub fn now_playing_entries(&self) -> Vec<plippa_core::NowPlayingEntry> {
        self.loaded
            .as_ref()
            .map(|l| l.list.entries())
            .unwrap_or_default()
    }

    /// Number of track prepares (`StartMainStream`) sent so far. Same-track cue seeks
    /// must not increase it.
    pub fn stream_prepare_count(&self) -> u64 {
        self.extras.starts_sent
    }

    /// True once the audio thread holds an open main stream.
    pub fn main_stream_open(&self) -> bool {
        self.has_main_stream()
    }

    /// Set the media-seconds-per-output-second factor (tempo/warp hook; default 1.0).
    /// Position and cue derivation scale with it; nothing else changes.
    pub fn set_playback_rate_factor(&mut self, factor: f64) {
        self.extras.rate_factor = factor.max(0.0);
    }

    /// Replace the monotonic clock used for the `Previous` double-press window (tests).
    pub fn set_clock(&mut self, clock: Box<dyn Clock>) {
        self.extras.clock = clock;
    }

    /// Fix the shuffle seed (tests). Player-side shuffle (PLAY_02) uses it; a load advances it.
    pub fn set_shuffle_seed(&mut self, seed: u64) {
        self.extras.shuffle_seed = seed;
    }

    /// Lab/test hook (deliberately not an IPC command): mark the loaded list as a **dynamic**
    /// queue, so the "no fixed order" path is reachable on a device before the queue package
    /// exists. Returns `false` when nothing is loaded.
    #[doc(hidden)]
    pub fn mark_list_dynamic(&mut self) -> bool {
        let Some(l) = self.loaded.as_mut() else {
            return false;
        };
        l.list.mark_dynamic();
        self.emit_now_playing();
        self.publish_status();
        true
    }

    /// Set of enabled cue lists is session-only: whether list `i` is enabled.
    pub fn cue_list_enabled(&self, list_index: usize) -> bool {
        self.loaded
            .as_ref()
            .is_none_or(|l| l.list.cue_list_enabled(list_index))
    }

    /// Media position used for navigation (live position, else the last known one).
    fn media_position_now(&self) -> u64 {
        self.current_position_ms()
            .unwrap_or(self.session.position_ms)
    }

    /// Persist (or clear) the long-form resume point of the loaded playable.
    ///
    /// Eligible: `overrideResumeBehaviour` `alwaysResume`, or (default) a `mix` /
    /// `audiobook` / `podcast` at least [`RESUME_MIN_DURATION_MS`] long (unknown length
    /// counts as long). Cleared when the list finished, or the last track is within the
    /// restart tail. The engine only *asks*; the server writes `.noudata`.
    fn maybe_writeback_resume(&mut self, list_finished: bool) {
        let Some(l) = self.loaded.as_ref() else {
            return;
        };
        let Some(track) = l.list.current_track() else {
            return;
        };
        let rec = &l.record;
        if rec.is_orphan || rec.noudata_relpath.is_empty() {
            return;
        }
        let long_form = matches!(
            rec.release_profile.as_deref(),
            Some("podcast" | "audiobook" | "mix")
        ) || matches!(
            rec.kind,
            plippa_noubin::LibraryKind::Mix
                | plippa_noubin::LibraryKind::Audiobook
                | plippa_noubin::LibraryKind::Podcast
        );
        let eligible = match track.resume_policy {
            ResumePolicy::Never => false,
            ResumePolicy::Always => true,
            ResumePolicy::Default => {
                long_form
                    && rec
                        .duration_ms
                        .or(track.duration_ms)
                        .is_none_or(|d| d >= RESUME_MIN_DURATION_MS)
            }
        };
        if !eligible {
            return;
        }
        let Some(playable_id) = self.session.current_item else {
            return;
        };
        let pos = self
            .current_position_ms()
            .unwrap_or(self.session.position_ms);
        let dur = self.current_duration_ms().or(track.duration_ms);
        let is_last = l.list.current_index() + 1 >= l.list.len();
        let near_end =
            dur.is_some_and(|d| d > 0 && pos.saturating_add(RESUME_RESTART_TAIL_MS) >= d);
        let clear = list_finished || (near_end && is_last);
        if !clear && pos == 0 {
            return;
        }
        let wb = ResumeWriteback {
            playable_id,
            resume_at_item: (!clear).then_some(track.item_position + 1),
            resume_from_ms: (!clear).then_some(pos),
        };
        if self.extras.last_writeback.as_ref() == Some(&wb) {
            return;
        }
        eprintln!(
            "plippa-engine: resume writeback id={} item={:?} at {:?}ms",
            playable_id.as_u64(),
            wb.resume_at_item,
            wb.resume_from_ms
        );
        self.extras.last_writeback = Some(wb.clone());
        self.events.push(EngineEvent::ResumeWriteback(wb));
    }

    /// Playback position for the active stream, in **media** time (ms).
    ///
    /// The audio thread's source-frame playhead (heard position, frame exact; WARP_03)
    /// floored to a whole ms. While the NB_04 `rate_factor` hook is not 1.0 the legacy
    /// `frames written x rate_factor` formula is used instead (pure-path tests). Right after a
    /// start/seek command this reports the requested target until the audio thread has applied
    /// it, so back-to-back navigation stays consistent.
    pub fn current_position_ms(&self) -> Option<u64> {
        let audio = self.audio.as_ref()?;
        if let Some(target) = self.extras.pending_target_ms {
            let in_flight = audio.stats.starts_completed.load(Ordering::SeqCst)
                < self.extras.starts_sent
                || audio.stats.seeks_completed.load(Ordering::SeqCst) < self.extras.seeks_sent;
            if reports_pending_target(in_flight, self.extras.seek_to_marker.is_some()) {
                return Some(target);
            }
        }
        let rate = audio.stats.stream_sample_rate_hz.load(Ordering::Relaxed);
        if rate == 0 {
            return None;
        }
        if (self.extras.rate_factor - 1.0).abs() < f64::EPSILON {
            let frames = audio.stats.position_src_frames.load(Ordering::Relaxed);
            return Some(frame_to_ms(frames, rate));
        }
        let frames = audio.stats.frames_written.load(Ordering::Relaxed);
        let base = audio.stats.position_base_ms.load(Ordering::Relaxed);
        Some(media_position_ms(
            base,
            frames,
            rate,
            self.extras.rate_factor,
        ))
    }

    /// Actual playhead in source frames at the file's native rate (`None` without a stream).
    /// Unlike [`current_position_ms`](Self::current_position_ms) this never reports a pending
    /// seek target and ignores the NB_04 `rate_factor` hook.
    pub fn current_position_frames(&self) -> Option<u64> {
        let audio = self.audio.as_ref()?;
        if audio.stats.stream_sample_rate_hz.load(Ordering::Relaxed) == 0 {
            return None;
        }
        Some(audio.stats.position_src_frames.load(Ordering::Relaxed))
    }

    /// Instantaneous rate published by the audio thread, micro-units (1_000_000 = 1.0x).
    pub fn audio_rate_micro(&self) -> Option<i32> {
        Some(self.audio.as_ref()?.stats.rate_micro.load(Ordering::Relaxed))
    }

    /// Whether the audio thread reports a rate stage other than bypass (false until WARP_05).
    pub fn audio_warp_active(&self) -> bool {
        self.audio
            .as_ref()
            .is_some_and(|a| a.stats.warp_active.load(Ordering::Relaxed))
    }

    /// True when the stream reports a duration. MP3 without a Xing/Info header has none
    /// (`current_duration_ms()` is `None`): callers must skip upper-bound clamps then.
    pub fn duration_known(&self) -> bool {
        self.current_duration_ms().is_some()
    }

    /// Known duration for the active stream (`None` if unknown).
    pub fn current_duration_ms(&self) -> Option<u64> {
        let audio = self.audio.as_ref()?;
        let d = audio.stats.duration_ms.load(Ordering::Relaxed);
        if d == 0 {
            None
        } else {
            Some(d)
        }
    }

    /// Test/diagnostics: session resume bookmark for a normalised key.
    pub fn resume_entry(&self, normalised_key: &str) -> Option<&ResumeEntry> {
        self.resume_store.get(normalised_key)
    }

    /// Snapshot the active playable into the session resume store.
    fn snapshot_resume_bookmark(&mut self, finished: bool) {
        let Some(key) = self.active_normalised_key.clone() else {
            return;
        };
        let Some(loaded) = self.loaded.as_ref() else {
            return;
        };
        let Some(track) = loaded.list.current_track() else {
            return;
        };
        let position_ms = self.current_position_ms().unwrap_or(0);
        if !finished && position_ms == 0 {
            return;
        }
        let duration_ms = self.current_duration_ms();
        let entry = ResumeEntry {
            normalised_key: key,
            track_index: track.item_position as usize,
            position_ms: if finished {
                duration_ms.unwrap_or(position_ms)
            } else {
                position_ms
            },
            duration_ms,
            finished,
        };
        eprintln!(
            "plippa-engine: resume bookmark key={} track={} pos={}ms dur={:?} finished={finished}",
            entry.normalised_key, entry.track_index, entry.position_ms, entry.duration_ms
        );
        self.resume_store.upsert(entry);
    }

    /// Block the **control** thread (for starvation tests). Audio keeps advancing.
    pub fn control_plane_sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// Start position of a persisted resume: `(mediaList position, ms)`.
///
/// `resumeAtItem` (1-indexed in the file, 0-based here) picks the item; a lone item with
/// a `resumeFromTime` resumes without `resumeAtItem`. `neverResume` on the item disables it.
/// Whether `position_ms` reports the pending start/seek target instead of the live playhead.
/// True for starts and `SeekMs` while the audio thread has not applied them (NB_04 rule);
/// false while a `SeekTo` is in flight: its target is published as `seek_target_ms` and
/// `position_ms` stays the actual playhead (WARP_01).
fn reports_pending_target(in_flight: bool, seek_to_active: bool) -> bool {
    in_flight && !seek_to_active
}

fn persisted_start(list: &NowPlayingList, prefs: &CollectionPrefs) -> (Option<u32>, u64) {
    let item = prefs
        .resume_at_item
        .filter(|p| list.index_of_item(*p).is_some())
        .or_else(|| {
            list.tracks()
                .iter()
                .find(|t| t.resume_from_ms.is_some())
                .map(|t| t.item_position)
        });
    let Some(item) = item else {
        return (None, 0);
    };
    let track = list.index_of_item(item).and_then(|i| list.track(i));
    match track {
        Some(t) if t.resume_policy == ResumePolicy::Never => (None, 0),
        Some(t) => (Some(item), t.resume_from_ms.unwrap_or(0)),
        None => (None, 0),
    }
}

const LIBRARY_RETRY_FIRST: Duration = Duration::from_secs(2);
const LIBRARY_RETRY_MAX: Duration = Duration::from_secs(30);

/// When to try opening the missing readers again.
struct LibraryRetry {
    next_at: Option<Instant>,
    delay: Duration,
}

impl Default for LibraryRetry {
    fn default() -> Self {
        Self {
            next_at: None,
            delay: LIBRARY_RETRY_FIRST,
        }
    }
}

impl LibraryRetry {
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Resolve override-or-discovered roots and open WAL readers.
///
/// Soft-fails: missing roots / indexes yield notices and empty readers so the
/// process stays up until `LibraryIndexUpdated`.
fn open_roots(
    override_root: Option<PathBuf>,
) -> (
    Vec<PathBuf>,
    Vec<(PathBuf, LibraryReader)>,
    Option<UserNotice>,
) {
    let discovered = resolve_library_roots(override_root.as_deref());
    if discovered.is_empty() {
        let message = if override_root.is_some() {
            "Media library root missing or invalid — staying up.".to_string()
        } else {
            "No media library found (plug a USB or set PLIPPA_LIBRARY_ROOT) — staying up."
                .to_string()
        };
        return (
            Vec::new(),
            Vec::new(),
            Some(UserNotice {
                severity: NoticeSeverity::Warning,
                code: NOTICE_NO_MEDIA_STORAGE.into(),
                message,
            }),
        );
    }

    let roots: Vec<PathBuf> = discovered.iter().map(|d| d.path.clone()).collect();
    let mut readers = Vec::new();
    let mut first_notice = None;
    for d in &discovered {
        match try_open_reader(&d.path) {
            (Some(reader), None) => readers.push((d.path.clone(), reader)),
            (Some(reader), Some(n)) => {
                readers.push((d.path.clone(), reader));
                if first_notice.is_none() {
                    first_notice = Some(n);
                }
            }
            (None, n) => {
                if first_notice.is_none() {
                    first_notice = n;
                }
            }
        }
    }
    (roots, readers, first_notice)
}

/// Try to open a WAL reader; soft-fail with a user-visible notice when the
/// library root or `.plippa` index is missing.
fn try_open_reader(library_root: &Path) -> (Option<LibraryReader>, Option<UserNotice>) {
    let root = LibraryRoot::new(library_root);
    // The engine's command thread must stay responsive: a short wait, then Busy (retried by
    // `poll_library_retry`), never a long block.
    match LibraryReader::open_with(&root, OpenWait::Interactive) {
        Ok(reader) => match reader.journal_mode() {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => (Some(reader), None),
            Ok(mode) => (
                None,
                Some(UserNotice {
                    severity: NoticeSeverity::Error,
                    code: NOTICE_NO_MEDIA_STORAGE.into(),
                    message: format!("Library index is not WAL (got {mode}); refusing to open"),
                }),
            ),
            Err(e) => (
                None,
                Some(UserNotice {
                    severity: NoticeSeverity::Error,
                    code: NOTICE_NO_MEDIA_STORAGE.into(),
                    message: format!("Could not read library journal mode: {e}"),
                }),
            ),
        },
        Err(LibraryError::IndexMissing { path }) => (
            None,
            Some(UserNotice {
                severity: NoticeSeverity::Warning,
                code: NOTICE_NO_MEDIA_STORAGE.into(),
                message: format!(
                    "Library index not found at {} — staying up until server creates it.",
                    path.display()
                ),
            }),
        ),
        Err(LibraryError::MissingRoot { path }) | Err(LibraryError::InvalidRoot { path }) => (
            None,
            Some(UserNotice {
                severity: NoticeSeverity::Warning,
                code: NOTICE_NO_MEDIA_STORAGE.into(),
                message: format!(
                    "Media library root missing or invalid ({}) — staying up.",
                    path.display()
                ),
            }),
        ),
        Err(e) => (
            None,
            Some(UserNotice {
                severity: NoticeSeverity::Error,
                code: NOTICE_NO_MEDIA_STORAGE.into(),
                message: format!("Could not open library index: {e}"),
            }),
        ),
    }
}

/// Resolve default tap SFX under `repo_root/assets/plippaSFX/`.
pub fn default_sfx_candidates(repo_root: &Path) -> Vec<PathBuf> {
    let dir = repo_root.join("assets/plippaSFX");
    vec![
        dir.join("PlippaSFX_Chime1.wav"),
        dir.join("PlippaSFX_Voice1.wav"),
    ]
}

/// Pick the first existing path from `--sfx` / env / default candidates.
pub fn resolve_sfx_path(explicit: Option<&Path>, repo_root: &Path) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    default_sfx_candidates(repo_root)
        .into_iter()
        .find(|p| p.is_file())
}

/// Build absolute best-path-per-mediaList-item for a playable.
///
/// Every path goes through [`plippa_noubin::confine`] (`SECFIX_05`): the `.noudata` and the
/// index row are untrusted, so a path is only returned when it names a plain regular file
/// below `library_root` reachable without a symlink (a FIFO, device, directory, symlink or
/// anything outside the root is dropped without being opened for reading, and without
/// blocking). The decoder's own opener repeats the regular-file check when it opens.
pub(crate) fn resolve_media_list_paths(
    library_root: &Path,
    record: &PlayableRecord,
    caps: &Capabilities,
    policy: OverspecPolicy,
) -> Result<Vec<PathBuf>, EngineError> {
    use plippa_noubin::confine;
    let Some(noudata_rel) = confine::clean_relpath(&record.noudata_relpath)
        .filter(|p| p.ends_with(".noudata"))
    else {
        // An orphan media file (no `.noudata`) or a row whose path is not acceptable.
        return resolve_flat_paths(library_root, &record.media_relpaths, caps, policy);
    };
    let noudata_path = library_root.join(&noudata_rel);
    let bytes = match plippa_noubin::read_noudata_capped(&noudata_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Fall back to flat media_relpaths from the index (single-track / degraded).
            return resolve_flat_paths(library_root, &record.media_relpaths, caps, policy);
        }
        Err(e) => return Err(e.into()),
    };
    let doc = parse_noudata(&bytes)?;
    let base_dir = noudata_rel.rsplit_once('/').map_or("", |(d, _)| d);

    let mut out = Vec::new();
    for item in &doc.media_list {
        if let Some(path) = best_path_for_item(base_dir, library_root, item, caps, policy)? {
            out.push(path);
        }
    }

    if out.is_empty() {
        return resolve_flat_paths(library_root, &record.media_relpaths, caps, policy);
    }
    Ok(out)
}

fn best_path_for_item(
    noudata_dir: &str,
    library_root: &Path,
    item: &MediaListItem,
    caps: &Capabilities,
    policy: OverspecPolicy,
) -> Result<Option<PathBuf>, EngineError> {
    use plippa_noubin::confine;
    let Some(rels) = item.local_file_paths.as_ref() else {
        return Ok(None);
    };
    let abs: Vec<PathBuf> = rels
        .iter()
        .filter_map(|p| {
            // Relative to the `.noudata`'s folder first, then (leniently) to the library root.
            confine::open_media(library_root, noudata_dir, p)
                .or_else(|_| confine::open_media(library_root, "", p))
                .ok()
                .map(|m| (library_root.join(&m.rel), m.file))
                // A file that crashed the media helper is as good as absent: the item is
                // skipped like any other unplayable one, and costs no spawn.
                .filter(|(path, file)| !Isolation::global().is_marked(path, file))
                .map(|(path, _)| path)
        })
        .collect();
    if abs.is_empty() {
        return Ok(None);
    }
    match select_best_local_path(&abs, caps, policy) {
        Ok(p) => Ok(Some(p)),
        Err(_) => Ok(None),
    }
}

fn resolve_flat_paths(
    library_root: &Path,
    relpaths: &[String],
    caps: &Capabilities,
    policy: OverspecPolicy,
) -> Result<Vec<PathBuf>, EngineError> {
    let abs: Vec<PathBuf> = relpaths
        .iter()
        .filter_map(|p| {
            plippa_noubin::confine::open_library_media(library_root, p)
                .ok()
                .map(|m| (library_root.join(&m.rel), m.file))
                .filter(|(path, file)| !Isolation::global().is_marked(path, file))
                .map(|(path, _)| path)
        })
        .collect();
    if abs.is_empty() {
        return Ok(Vec::new());
    }
    // Treat all paths as alternate formats of one track when we lack mediaList structure.
    let best = select_best_local_path(&abs, caps, policy)?;
    Ok(vec![best])
}

/// Device and latency profile a route intent asks the sink factory for.
fn stream_target(intent: &RouteIntent) -> (Option<String>, PulseConfig) {
    match &intent.target {
        Target::Direct(sink) => (Some(sink.clone()), PulseConfig::low_latency()),
        Target::Multi { .. } => (Some(MULTI_TARGET.to_string()), PulseConfig::multi()),
    }
}

/// The same identity for "no route yet": the server default device.
fn stream_target_or_default(intent: Option<&RouteIntent>) -> (Option<String>, PulseConfig) {
    match intent {
        Some(intent) => stream_target(intent),
        None => (None, PulseConfig::low_latency()),
    }
}

/// WAVE_01: the `.wfm` sidecar path published for `media_path` under `library_root`.
///
/// `None` when nothing is resolved, when the media file is not under this library root (a
/// linked file elsewhere has no sidecar this server could write) or when the relative path is
/// not valid UTF-8.
fn session_waveform_path(library_root: &Path, media_path: Option<&Path>) -> Option<String> {
    let rel = media_path?.strip_prefix(library_root).ok()?;
    let rel = rel.to_str()?;
    Some(plippa_noubin::waveform_path(library_root, rel).display().to_string())
}

impl From<std::io::Error> for EngineError {
    fn from(value: std::io::Error) -> Self {
        EngineError::Message(value.to_string())
    }
}

#[cfg(test)]
mod seamless_tests;

#[cfg(test)]
mod waveform_path_tests {
    use super::session_waveform_path;

    #[test]
    fn a_media_file_outside_the_library_root_has_no_waveform_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("library");
        std::fs::create_dir_all(root.join("Music")).unwrap();
        let inside = root.join("Music/a.wav");
        assert_eq!(
            session_waveform_path(&root, Some(&inside)),
            Some(
                plippa_noubin::waveform_path(&root, "Music/a.wav")
                    .display()
                    .to_string()
            )
        );
        assert_eq!(session_waveform_path(&root, None), None);

        let outside = tmp.path().join("loose/elsewhere.wav");
        assert_eq!(session_waveform_path(&root, Some(&outside)), None);
        // A sibling directory whose name merely shares the prefix is not "under" the root.
        let sibling = tmp.path().join("library2/x.wav");
        assert_eq!(session_waveform_path(&root, Some(&sibling)), None);
    }
}

#[cfg(test)]
mod warp_tests {
    use super::reports_pending_target;

    #[test]
    fn pending_target_rule_table() {
        // (in_flight, seek_to_active) -> reports the pending target
        assert!(reports_pending_target(true, false), "SeekMs/start: legacy rule");
        assert!(!reports_pending_target(true, true), "SeekTo: position stays actual");
        assert!(!reports_pending_target(false, false), "landed: live position");
        assert!(!reports_pending_target(false, true));
    }
}

/// UI_13 / POLISH_02: the collection line for `record` while `current` plays. A loose track
/// has none (its title would be noise); a release / playlist / mix / audiobook / podcast names
/// its collection, and a cued mix playing inside a collection is appended to that name.
fn collection_line(record: &PlayableRecord, current: Option<&plippa_core::ListTrack>) -> String {
    use plippa_core::{compose_collection_title, LoadedKind};
    use plippa_noubin::sanitise_line;
    let kind = match record.kind {
        LibraryKind::Track => LoadedKind::Track,
        LibraryKind::Mix => LoadedKind::Mix,
        _ => LoadedKind::Collection,
    };
    let line = |s: &str| sanitise_line(s, plippa_core::COLLECTION_TITLE_MAX_CHARS);
    let collection = record.title.as_deref().map(line).unwrap_or_default();
    let mix = current.filter(|t| t.is_cued_mix).map(|t| line(&t.title));
    compose_collection_title(&collection, kind, mix.as_deref())
}

/// SECFIX_05 (SYS-13): the engine's path resolution treats the `.noudata` and the index row
/// as untrusted. The same corpus is replayed against the server's waveform worker in
/// `bins/plippa-server/tests/stick_untrusted.rs`.
#[cfg(test)]
mod untrusted_path_tests {
    use super::*;
    use plippa_noubin::{LibraryRoot, LibraryWriter};
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::sync::mpsc;
    use std::time::Duration;

    /// 100 frames of silence: a real, probeable WAV.
    fn wav() -> Vec<u8> {
        let data_len = 200u32;
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&48_000u32.to_le_bytes());
        v.extend_from_slice(&96_000u32.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        v.extend(std::iter::repeat(0u8).take(data_len as usize));
        v
    }

    fn mkfifo(path: &Path) {
        extern "C" {
            fn mkfifo(path: *const std::os::raw::c_char, mode: u32) -> i32;
        }
        let c = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    fn noudata(title: &str, path: &str) -> String {
        format!(
            r#"{{"playableItemTitle":"{title}","mediaList":[{{"localFilePaths":[{}]}}]}}"#,
            serde_json_string(path)
        )
    }

    fn serde_json_string(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// A stick with one good track and every hostile reference, plus files outside the root.
    fn hostile_stick() -> (tempfile::TempDir, PathBuf, Vec<PlayableRecord>) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("stick");
        let music = root.join("Music");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("ok.wav"), wav()).unwrap();
        std::fs::write(tmp.path().join("outside.wav"), wav()).unwrap();
        symlink(tmp.path().join("outside.wav"), music.join("sym.wav")).unwrap();
        symlink("/etc", root.join("etc-link")).unwrap();
        mkfifo(&music.join("pipe.wav"));
        let outside_abs = tmp.path().join("outside.wav").display().to_string();
        for (name, path) in [
            ("ok", "ok.wav".to_string()),
            ("absolute", outside_abs),
            ("dotdot", "../../outside.wav".to_string()),
            ("symlink", "sym.wav".to_string()),
            ("fifo", "pipe.wav".to_string()),
            ("symdir", "../etc-link/hostname".to_string()),
        ] {
            std::fs::write(music.join(format!("{name}.noudata")), noudata(name, &path)).unwrap();
        }
        let mut w = LibraryWriter::open(&LibraryRoot::new(&root)).unwrap();
        w.full_scan().unwrap();
        let records = w.list_playables().unwrap();
        (tmp, root, records)
    }

    fn record<'a>(records: &'a [PlayableRecord], title: &str) -> &'a PlayableRecord {
        records
            .iter()
            .find(|r| r.title.as_deref() == Some(title))
            .unwrap_or_else(|| panic!("no record {title}"))
    }

    fn resolve(root: &Path, rec: &PlayableRecord) -> Vec<PathBuf> {
        let caps = Capabilities::desktop_mock();
        resolve_media_list_paths(root, rec, &caps, OverspecPolicy::Downsample).unwrap()
    }

    #[test]
    fn the_engine_refuses_a_noudata_that_points_at_an_absolute_file() {
        let (_tmp, root, records) = hostile_stick();
        assert_eq!(resolve(&root, record(&records, "ok")), [root.join("Music/ok.wav")]);
        assert!(resolve(&root, record(&records, "absolute")).is_empty());
        assert!(resolve(&root, record(&records, "dotdot")).is_empty());
    }

    #[test]
    fn the_engine_refuses_a_noudata_that_points_at_a_fifo_without_blocking() {
        let (_tmp, root, records) = hostile_stick();
        let rec = record(&records, "fifo").clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(resolve(&root, &rec));
        });
        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("path resolution blocked on a FIFO");
        assert!(got.is_empty());
    }

    #[test]
    fn symlinks_are_not_followed_by_the_engine() {
        let (_tmp, root, records) = hostile_stick();
        assert!(resolve(&root, record(&records, "symlink")).is_empty());
        assert!(resolve(&root, record(&records, "symdir")).is_empty());
    }

    #[test]
    fn a_stick_row_pointing_outside_the_root_never_reaches_the_decoder() {
        let (tmp, root, records) = hostile_stick();
        // A (hostile) index row naming files outside the root, as the flat fallback sees it.
        let mut rec = record(&records, "ok").clone();
        rec.noudata_relpath = "/etc/passwd.noudata".into();
        rec.media_relpaths = vec![
            tmp.path().join("outside.wav").display().to_string(),
            "../outside.wav".into(),
            "Music/sym.wav".into(),
            "Music/pipe.wav".into(),
        ];
        assert!(resolve(&root, &rec).is_empty());
        // The decode-time open (`SECFIX_05`) is the check: no symlink, FIFO or device is
        // opened, and the handle it returns is the one the helper decodes.
        let roots = vec![root.clone()];
        let open = |p: &str| plippa_core::safe_fs::open_media_for_decode_in(&roots, &root.join(p));
        assert!(open("Music/pipe.wav").is_err());
        assert!(open("Music/sym.wav").is_err());
        assert!(open("../outside.wav").is_err());
        assert!(open("Music/ok.wav").is_ok());
    }

    #[test]
    fn a_directory_swapped_for_a_symlink_after_resolution_never_reaches_the_decoder() {
        // The residual SECFIX_05 race: resolve/validate the path, then the stick's author
        // swaps a directory on the way for a symlink before the decoder opens it. The decode
        // open is confined beneath the root, so it refuses instead of reading outside.
        let (tmp, root, records) = hostile_stick();
        let resolved = resolve(&root, record(&records, "ok"));
        assert_eq!(resolved, [root.join("Music/ok.wav")]);
        let path = resolved[0].clone();
        // Swap `Music` for a link to the outside directory holding a same-named file.
        std::fs::remove_dir_all(root.join("Music")).unwrap();
        let outside_dir = tmp.path().join("outside-dir");
        std::fs::create_dir_all(&outside_dir).unwrap();
        std::fs::write(outside_dir.join("ok.wav"), wav()).unwrap();
        symlink(&outside_dir, root.join("Music")).unwrap();
        let roots = vec![root.clone()];
        assert!(
            plippa_core::safe_fs::open_media_for_decode_in(&roots, &path).is_err(),
            "the decoder must not follow the swapped directory out of the library"
        );
    }

    #[test]
    fn a_discovered_engine_confines_neighbour_opens_to_the_roots_it_found() {
        // `open_*_discovered` has no override root (USB auto-discovery). The audio thread must
        // still be given the roots the engine resolved, or every neighbour pre-open would fall
        // back to the unconfined no-follow open (SECFIX_05) while the main stream stays confined.
        let root = PathBuf::from("/media/pi/stick");
        assert_eq!(
            audio_roots(&None, std::slice::from_ref(&root)),
            vec![root.clone()],
            "a discovered root must reach the neighbour open"
        );
        assert_eq!(audio_roots(&None, &[]), Vec::<PathBuf>::new());
        // An override root is the whole story: it is the one a test or single-root install uses.
        assert_eq!(
            audio_roots(&Some(root.clone()), &[PathBuf::from("/media/pi/other")]),
            vec![root]
        );
    }
}

/// SECFIX_05: a reader that loses the index-open race (`LibraryError::Busy`) is retried on a
/// timer instead of waiting for a newer index generation.
#[cfg(test)]
mod busy_retry_tests {
    use super::*;
    use plippa_noubin::{LibraryRoot, LibraryWriter};
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};

    const ROLE: &str = "PLIPPA_ENGINE_TEST_ROLE";
    const ROOT: &str = "PLIPPA_ENGINE_TEST_ROOT";

    /// Child role: hold the index's vet lock (a peer that is slowly opening a big index).
    #[test]
    fn child_holds_the_vet_lock() {
        if std::env::var(ROLE).as_deref() != Ok("hold") {
            return;
        }
        use plippa_core::safe_fs::{lock_file_exclusive, FileLock};
        let root = PathBuf::from(std::env::var(ROOT).unwrap());
        let held = lock_file_exclusive(
            &root.join(".plippa/.vet.lock"),
            0,
            1,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(held, FileLock::Held(_)));
        println!("READY");
        std::io::stdout().flush().unwrap();
        let mut sink = String::new();
        let _ = std::io::stdin().read_line(&mut sink);
    }

    #[test]
    fn a_busy_index_at_start_up_is_retried_until_the_readers_are_back() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("stick");
        std::fs::create_dir_all(root.join("Music")).unwrap();
        std::fs::write(root.join("Music/a.wav"), b"RIFF").unwrap();
        std::fs::write(
            root.join("Music/a.noudata"),
            r#"{"playableItemTitle":"A","mediaList":[{"localFilePaths":["a.wav"]}]}"#,
        )
        .unwrap();
        LibraryWriter::open(&LibraryRoot::new(&root)).unwrap().full_scan().unwrap();

        let mut holder = Command::new(std::env::current_exe().unwrap())
            .args(["engine::busy_retry_tests::child_holds_the_vet_lock", "--exact", "--nocapture", "--test-threads=1"])
            .env(ROLE, "hold")
            .env(ROOT, &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = std::io::BufReader::new(holder.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert!(out.read_line(&mut line).unwrap() > 0, "holder exited early");
            if line.contains("READY") {
                break;
            }
        }

        // The engine starts while a peer holds the lock: no reader, but it stays up.
        let started = Instant::now();
        let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "the open blocked for long");
        assert!(engine.readers.is_empty(), "busy: no reader yet");
        assert!(!engine.library_roots.is_empty());
        let _ = engine.take_events();

        // Still busy: the timer retries, with no notice per attempt.
        engine.poll_library_retry(); // arms the timer
        engine.library_retry.next_at = Some(Instant::now() - Duration::from_secs(1));
        engine.poll_library_retry();
        assert!(engine.readers.is_empty());
        assert!(engine.take_events().is_empty(), "no notice spam while busy");
        assert!(engine.library_retry.delay > LIBRARY_RETRY_FIRST, "backoff grows");

        // The peer finishes: the next retry succeeds and says so once.
        drop(holder.stdin.take());
        let _ = holder.wait();
        engine.library_retry.next_at = Some(Instant::now() - Duration::from_secs(1));
        engine.poll_library_retry();
        assert_eq!(engine.readers.len(), 1, "the reader is back without a new generation");
        let events = engine.take_events();
        assert!(events.iter().any(|e| matches!(e, EngineEvent::Notice(n) if n.severity == NoticeSeverity::Info)));
        // And it stops retrying.
        engine.poll_library_retry();
        assert!(engine.library_retry.next_at.is_none());
    }
}
