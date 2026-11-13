//! Dedicated audio thread: stream decode + native-rate sink writes.
//!
//! Control plane sends [`AudioCommand`]s (paths / short SFX PCM) — never a
//! whole-track buffer. Decode pulls chunks on this thread into a byte-bounded
//! [`SourceWindow`] (history behind the playhead, lookahead in front, WARP_04); the
//! track PipeWire/Pulse stream opens at the **file sample rate**.
//!
//! Tap/UI SFX is already PCM (WAV decoded at engine load). It plays on a
//! **helper thread** so the cue starts immediately and never blocks main-track
//! open/decode. When SFX is used, main Play may be armed with `not_before` so
//! music joins after ~1 s of SFX cover (overlap / echo tail).
//!
//! [`AudioCommand::SeekMs`] seeks within the open main stream: the new position is read from
//! the window (no decoder seek when it is inside) and joined to the audio already on its way
//! to the sink by a short equal-power crossfade ([`XfadeConfig::xfade_seek_ms`]). Next/prev
//! and start-at restart a new stream (hard cut; there is no old output of that stream to fade
//! from).

use plippa_audio::position::{
    frame_to_ms, heard_position_src, ms_to_frame, WriteSpans, PAUSE_REQUEUE_MAX_MS,
    RATE_MICRO_UNITY,
};
use plippa_audio::isolate::{Isolation, RemoteDecoder};
use plippa_audio::worker::WorkerStats;
use plippa_audio::{
    apply_output_stage, chase_step, final_quantum, scan_rate_cap, ScanProfile, AudioSink,
    AudioSinkFactory, BlockSource, Crossfader, FeedParams, Hint, OverspecPolicy,
    ChaseConfig, PcmBuffer, Quantum, RampSpec, RateConfig, RateStage, StretchConfig,
    StretchVoice, Volume as AudioVolume, VoiceSelector, WindowConfig, WindowFeed, XfadeConfig,
    DEFAULT_CHUNK_FRAMES, START_CHUNK_FRAMES, STRETCH_MIN_RATE,
    STRETCH_PREFETCH_BYTES,
};
use plippa_core::{Capabilities, ScanDir, SeekDriver, VoiceKind, WarpMode, SPEED_WARP_DEFAULT};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

mod cross;
pub use cross::{CrossEvent, NeighbourSrc};

/// Optional autoplay after a successful [`AudioCommand::StartMainStream`] open.
#[derive(Debug, Clone)]
pub struct PlayArm {
    /// When set (SFX cover), prefetch may continue but the first main sink write
    /// waits until that instant.
    pub not_before: Option<Instant>,
}

/// Commands from the control plane to the audio thread.
#[derive(Debug)]
pub enum AudioCommand {
    /// Open a chunked decoder for `path` at the file’s native rate.
    ///
    /// `start_ms` seeks before the first prefetch (0 = from the beginning).
    /// When `play` is `Some`, arm Playing only after a successful open (failed
    /// open never leaves the thread in a playing state).
    StartMainStream {
        path: PathBuf,
        /// The already-validated handle for `path` (`SECFIX_05`): the engine opens it once,
        /// confined beneath the library root, and the helper decodes this descriptor. No
        /// reopen, so nothing can be swapped between the check and the decode.
        file: std::fs::File,
        capabilities: Capabilities,
        overspec: OverspecPolicy,
        start_ms: u64,
        play: Option<PlayArm>,
        /// PLAY_02: source `mediaList` position of this item (the crossing identity).
        item_position: u32,
    },
    ClearMain,
    /// Start (or arm) main-track output when a stream is already open.
    ///
    /// When `not_before` is set (SFX cover), prefetch may continue but the first
    /// main sink write waits until that instant.
    Play {
        not_before: Option<Instant>,
    },
    Pause,
    Stop,
    SetVolume(f32),
    /// Seek the open main stream to approximately `position_ms` (clamped by caller).
    SeekMs(u64),
    /// Pre-decoded one-shot cue (WAV→PCM at load). Plays on a helper thread.
    TriggerSfx(PcmBuffer),
    Shutdown,
    /// Set speed (WARP_05), signed permille (`-2000..=2000`, never 0). Session-wide: it stays
    /// in force across streams until changed. Appended: earlier variants keep their meaning.
    SetRate { permille: i32, change: RateChange },
    /// Speed warp mode for the speed voice (`PitchShift` = varispeed, `PitchCorrect` = the
    /// stretch voice for positive rates; reverse is always varispeed). Applied at once with a
    /// 40 ms crossfade (WARP_09).
    SetSpeedWarpMode(WarpMode),
    /// Pre-decode toward a chase target (WARP_11): the decode worker extends the window (or
    /// prepares a waypoint window for a far target) and guarantees the landing history.
    /// `None` clears the hint. Nothing consumes it before WARP_14.
    Hint(Option<Hint>),
    /// `SeekTo` (WARP_14): travel to `target_ms` with the seek voice chosen by `mode` (the seek
    /// warp mode sampled now: a later mode change applies to the next seek). Playing: one
    /// bounded swoop (a waypoint jump first when far) handing over to the set speed at the
    /// target; paused: a silent seek. A newer command while chasing just replaces the target.
    SeekChase { target_ms: u64, mode: WarpMode },
    /// FF/RW hold (WARP_17): start the cassette scan in `dir` with the seek voice chosen by
    /// `mode` (sampled now; backward is always varispeed). While paused (or with the stream
    /// open but not playing) the scan is audible and returns to paused on release. A second
    /// `ScanStart` re-presses (new direction glides through zero). Cancels a running chase.
    ScanStart { dir: ScanDir, mode: WarpMode },
    /// Release: restore the set speed at once (or flush + pause for a scan from paused); the
    /// playhead stays where the audio got to. Harmless without a running scan.
    ///
    /// UI_15: the release is **immediate** — no brake. The audio thread sets the rate straight
    /// back to the session's set speed (`ctl.target`, whatever it is: not hardcoded to 100 %),
    /// or flushes and pauses exactly on the heard position for a scan that started from paused.
    ScanStop,
    /// UI_15: the running hold's scan speed, in permille of the usual scan speed
    /// (`plippa_core::SCAN_SPEED_LADDER_PERMILLE`; `1000` = the usual 4x/8x). Re-targets a live
    /// scan from the next quantum; never touches the set speed. Harmless without a scan.
    ScanSpeed { permille: i32 },
    /// WARP_19: the items `Previous` / `Next` would reach (resolved by the engine; only sent
    /// while "Seek outside track" is on and a scan may cross). `None` on both clears. A scan
    /// that runs into a boundary crosses into the neighbour seamlessly; it opens the neighbour
    /// shortly before. Without this command (setting off) nothing changes.
    SetNeighbours {
        prev: Option<NeighbourSrc>,
        next: Option<NeighbourSrc>,
    },
    /// WARP_19: like `StartMainStream`, but the audio of the stream being replaced is faded
    /// out over the new one (the jog's jump into a neighbouring item).
    SwitchItem {
        path: PathBuf,
        /// The already-validated handle for `path` (`SECFIX_05`), as for `StartMainStream`.
        file: std::fs::File,
        capabilities: Capabilities,
        overspec: OverspecPolicy,
        start_ms: u64,
        play: Option<PlayArm>,
        /// PLAY_02: source `mediaList` position of this item (the crossing identity).
        item_position: u32,
    },
    /// UI_14: the FF/RW-hold + Play gesture. Set the rate with **no ramp** and drop any scan or
    /// seek chase where it stands, so picture and audio are both instantly at the live position
    /// at the new speed (the ordinary `SetRate` would be swallowed by a running seek and would
    /// otherwise glide). Appended: earlier variants keep their meaning.
    SnapRate { permille: i32 },
}

/// How a set-rate change is approached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateChange {
    /// Tap / tempo change: exponential ramp with `RateConfig::tau_tempo_ms`.
    Tempo,
    /// Reset to 100 %: exponential ramp with `RateConfig::tau_reset_ms`.
    Reset,
    /// No ramp (Unload, tests).
    Immediate,
}

/// Shared counters / diagnostics observable from the control / test thread.
#[derive(Debug, Clone)]
pub struct AudioThreadStats {
    pub frames_written: Arc<AtomicU64>,
    pub audio_thread_id: Arc<Mutex<Option<ThreadId>>>,
    pub running: Arc<AtomicBool>,
    /// Sample rate of the active track stream (0 if none).
    pub stream_sample_rate_hz: Arc<AtomicU32>,
    /// Set when the first track sink write succeeds after Start+Play.
    pub first_audio_ms: Arc<AtomicU64>,
    /// Last stream/decode error message (cleared on successful Start).
    pub last_error: Arc<Mutex<Option<String>>>,
    /// True after main stream hits EOF (control may move transport to Stopped).
    pub main_finished: Arc<AtomicBool>,
    /// Peak lookahead bytes (decoded, not yet played) observed for the current stream.
    pub peak_prefetch_bytes: Arc<AtomicU64>,
    /// Peak bytes held by the [`SourceWindow`] (history + lookahead) for the current stream.
    /// Never exceeds `WindowConfig::window_cap_bytes` (WARP_04).
    pub peak_window_bytes: Arc<AtomicU64>,
    /// Decoded frames held by the window when the first main write of the stream was made
    /// (a count, not a time: the first-audio prefetch depth; WARP_04).
    pub first_write_window_frames: Arc<AtomicU64>,
    /// Decoder seeks issued for the current stream (a seek inside the window issues none).
    pub decoder_seeks: Arc<AtomicU64>,
    /// Hard seeks that started an equal-power crossfade for the current stream.
    pub seek_crossfades: Arc<AtomicU64>,
    /// Stream timeline origin in ms: the **actual** landing of the last start/seek (the decoder
    /// trims to the requested frame, so this is the request floored to a whole ms).
    /// Legacy path: position = base + frames_written x rate_factor (NB_04 hook).
    pub position_base_ms: Arc<AtomicU64>,
    /// **Actual playhead** in source frames at the file's native rate: the media position of
    /// the audio being *heard* (`written - rate x sink_queue`, WARP_03). Never a seek target.
    pub position_src_frames: Arc<AtomicU64>,
    /// Instantaneous rate in micro-units (1_000_000 = 1.0x; signed). 1.0x until WARP_05.
    pub rate_micro: Arc<AtomicI32>,
    /// True while a rate stage other than bypass is producing the output (false until WARP_05).
    pub warp_active: Arc<AtomicBool>,
    /// Voice in use by the speed family: 0 Bypass, 1 Varispeed, 2 Stretch (WARP_05).
    pub speed_voice: Arc<AtomicU8>,
    /// Times reverse reached frame 0 and bounced forward (the engine mirrors the new set rate).
    pub rate_bounces: Arc<AtomicU64>,
    /// Sink output queue in frames used by the last position publish (diagnostic).
    pub sink_queue_frames: Arc<AtomicU64>,
    /// Known duration in ms (0 = unknown).
    pub duration_ms: Arc<AtomicU64>,
    /// True while the audio thread holds an open main track stream.
    pub has_main_stream: Arc<AtomicBool>,
    /// True while the audio thread is actively writing main-track PCM (or armed
    /// behind an SFX `not_before` deadline).
    pub main_playing: Arc<AtomicBool>,
    /// Set when a bare [`AudioCommand::Play`] was ignored (no stream). Control
    /// plane must roll back session Playing.
    pub play_ignored: Arc<AtomicBool>,
    /// Incremented when a [`AudioCommand::StartMainStream`] finishes (ok or err)
    /// so the control plane can clear `pending_start`.
    pub starts_completed: Arc<AtomicU64>,
    /// Incremented when an [`AudioCommand::SeekMs`] has been handled (applied or
    /// dropped) so the control plane knows the position counters are up to date.
    pub seeks_completed: Arc<AtomicU64>,
    /// Counters of the decode worker of the open stream (WARP_11): back-windows, decoder
    /// seeks, index state, hand-off counts. `None` while no stream is open.
    pub worker: Arc<Mutex<Option<Arc<WorkerStats>>>>,
    /// `max_sustainable_rate()` x 1000 from the worker's measured decode speed x
    /// `DECODE_SAFETY_FACTOR` (0 = not measured yet). Real values only exist on the device.
    pub max_sustainable_rate_milli: Arc<AtomicU64>,
    /// Render attempts that found no data (the worker was still fetching): the audio loop
    /// held (fade-out/silence) instead of glitching.
    pub starved_blocks: Arc<AtomicU64>,
    /// True while a [`AudioCommand::SeekChase`] swoop is travelling to its target.
    pub chase_active: Arc<AtomicBool>,
    /// Chases started (a retarget does not count) / waypoint jumps made (WARP_14).
    pub chases_started: Arc<AtomicU64>,
    pub waypoint_jumps: Arc<AtomicU64>,
    /// Far jumps that adopted the worker's waypoint window (no hold while the decoder seeks).
    pub waypoints_adopted: Arc<AtomicU64>,
    /// True while a hold scan runs, refreshed every loop turn.
    pub scan_active: Arc<AtomicBool>,
    /// `ScanStart` commands the audio thread has handled (accepted or not): with
    /// `!scan_active` the control plane knows every scan it asked for is over.
    pub scans_handled: Arc<AtomicU64>,
    /// WARP_19: item changes made by a scan that crossed a track boundary, oldest first; the
    /// control plane drains them (`Engine::poll_stream_lifecycle`).
    pub cross_events: Arc<Mutex<Vec<CrossEvent>>>,
    /// Crossings made / neighbours that could not be used (open error, rate or channel
    /// mismatch, decode error).
    pub crossings: Arc<AtomicU64>,
    pub cross_failures: Arc<AtomicU64>,
    /// SEAM_01: tracks that ended with a next item on offer but no join (the neighbour was not
    /// ready, could not be opened, or differs in rate/channels): the old end-of-stream gap.
    pub seamless_fallbacks: Arc<AtomicU64>,
}

impl AudioThreadStats {
    fn new() -> Self {
        Self {
            frames_written: Arc::new(AtomicU64::new(0)),
            audio_thread_id: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(false)),
            stream_sample_rate_hz: Arc::new(AtomicU32::new(0)),
            first_audio_ms: Arc::new(AtomicU64::new(0)),
            last_error: Arc::new(Mutex::new(None)),
            main_finished: Arc::new(AtomicBool::new(false)),
            peak_prefetch_bytes: Arc::new(AtomicU64::new(0)),
            peak_window_bytes: Arc::new(AtomicU64::new(0)),
            first_write_window_frames: Arc::new(AtomicU64::new(0)),
            decoder_seeks: Arc::new(AtomicU64::new(0)),
            seek_crossfades: Arc::new(AtomicU64::new(0)),
            position_base_ms: Arc::new(AtomicU64::new(0)),
            position_src_frames: Arc::new(AtomicU64::new(0)),
            rate_micro: Arc::new(AtomicI32::new(RATE_MICRO_UNITY)),
            warp_active: Arc::new(AtomicBool::new(false)),
            speed_voice: Arc::new(AtomicU8::new(0)),
            rate_bounces: Arc::new(AtomicU64::new(0)),
            sink_queue_frames: Arc::new(AtomicU64::new(0)),
            duration_ms: Arc::new(AtomicU64::new(0)),
            has_main_stream: Arc::new(AtomicBool::new(false)),
            main_playing: Arc::new(AtomicBool::new(false)),
            play_ignored: Arc::new(AtomicBool::new(false)),
            starts_completed: Arc::new(AtomicU64::new(0)),
            seeks_completed: Arc::new(AtomicU64::new(0)),
            worker: Arc::new(Mutex::new(None)),
            max_sustainable_rate_milli: Arc::new(AtomicU64::new(0)),
            starved_blocks: Arc::new(AtomicU64::new(0)),
            chase_active: Arc::new(AtomicBool::new(false)),
            chases_started: Arc::new(AtomicU64::new(0)),
            waypoint_jumps: Arc::new(AtomicU64::new(0)),
            waypoints_adopted: Arc::new(AtomicU64::new(0)),
            scan_active: Arc::new(AtomicBool::new(false)),
            scans_handled: Arc::new(AtomicU64::new(0)),
            cross_events: Arc::new(Mutex::new(Vec::new())),
            crossings: Arc::new(AtomicU64::new(0)),
            cross_failures: Arc::new(AtomicU64::new(0)),
            seamless_fallbacks: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Tuning values of the audio thread's window and crossfades (README section 7). The defaults
/// are the conservative shipped values; tests pass small ones.
#[derive(Debug, Clone, Default)]
pub struct AudioThreadConfig {
    pub window: WindowConfig,
    pub xfade: XfadeConfig,
    pub rate: RateConfig,
    pub stretch: StretchConfig,
    /// Commit-chase tuning (WARP_14).
    pub chase: ChaseConfig,
    /// Hold-scan profile (WARP_17).
    pub scan: ScanProfile,
    /// Test hook: wraps the decoder of every opened file (instrumented / gated sources). The
    /// wrapped decoder still runs in the isolated media helper.
    pub decoder_hook: Option<DecoderHook>,
    /// The media helper starter; `None` uses the process-wide one (`Isolation::global`).
    pub isolation: Option<Isolation>,
    /// The library roots the media paths were resolved against (multi-USB). Used to open a
    /// neighbour (`SECFIX_05`) confined beneath its root; the main stream carries an
    /// already-open handle.
    pub library_roots: Vec<std::path::PathBuf>,
}

/// Wraps the decoder opened for a path (default: used as it is).
#[derive(Clone)]
pub struct DecoderHook(pub Arc<dyn Fn(&Path, Box<dyn BlockSource>) -> Box<dyn BlockSource> + Send + Sync>);

impl std::fmt::Debug for DecoderHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DecoderHook")
    }
}

/// What the audio thread opens a file with: the media helper, over the file handle that was
/// already validated. Every decode of a file from a stick, an upload or a download goes
/// through here, so a decoder that aborts kills the helper and not the engine (SECFIX_06).
///
/// The handle comes from the caller (`prepare_stream` opens it once, `SECFIX_05`), so there is
/// no check-then-reopen gap: the descriptor handed to the helper is the one that was checked,
/// and the helper inherits it (`HelperProc::spawn`) instead of opening the path again.
pub(crate) fn open_isolated(
    isolation: &Isolation,
    path: &Path,
    file: std::fs::File,
    capabilities: &Capabilities,
    overspec: OverspecPolicy,
) -> Result<RemoteDecoder, plippa_audio::AudioError> {
    isolation.open_decoder(&file, path, capabilities, overspec)
}

/// `open_isolated` when the caller has only a path (a neighbour pre-open): the file is opened
/// **confined beneath the library root**, so a directory swapped for a symlink since the path
/// was resolved is refused instead of followed.
pub(crate) fn open_isolated_at(
    isolation: &Isolation,
    library_roots: &[std::path::PathBuf],
    path: &Path,
    capabilities: &Capabilities,
    overspec: OverspecPolicy,
) -> Result<RemoteDecoder, plippa_audio::AudioError> {
    let file = plippa_core::safe_fs::open_media_for_decode_in(library_roots, path).map_err(
        |source| plippa_audio::AudioError::Io { path: path.to_path_buf(), source },
    )?;
    open_isolated(isolation, path, file, capabilities, overspec)
}

/// The decoder as the decode worker uses it, with the test hook applied.
pub(crate) fn boxed_source(
    decoder: RemoteDecoder,
    hook: Option<&DecoderHook>,
) -> Box<dyn BlockSource> {
    let path = decoder.path().to_path_buf();
    match hook {
        Some(h) => (h.0)(&path, Box::new(decoder)),
        None => Box::new(decoder),
    }
}

/// What the loop and command handler need besides the stats (`AudioThreadConfig` plus the
/// derived chunk size).
struct LoopCfg {
    window: WindowConfig,
    xfade: XfadeConfig,
    rate: RateConfig,
    stretch: StretchConfig,
    chase: ChaseConfig,
    scan: ScanProfile,
    decoder_hook: Option<DecoderHook>,
    isolation: Isolation,
    /// Frames decoded per chunk and written per sink write.
    chunk_frames: usize,
    /// Roots a neighbour's path is opened beneath (`SECFIX_05`).
    library_roots: Vec<std::path::PathBuf>,
}

/// Handle used by the control plane to talk to the audio thread.
pub struct AudioThreadHandle {
    pub cmd_tx: Sender<AudioCommand>,
    pub volume: AudioVolume,
    pub stats: AudioThreadStats,
    join: Option<JoinHandle<()>>,
}

impl AudioThreadHandle {
    /// Spawn with a sink factory so each track/SFX opens at its native rate.
    pub fn spawn_with_factory<F>(factory: F, block_frames: usize) -> Self
    where
        F: AudioSinkFactory + Send + Sync + 'static,
    {
        Self::spawn_with_config(factory, block_frames, AudioThreadConfig::default())
    }

    /// [`spawn_with_factory`](Self::spawn_with_factory) with explicit window/crossfade tuning.
    pub fn spawn_with_config<F>(factory: F, block_frames: usize, config: AudioThreadConfig) -> Self
    where
        F: AudioSinkFactory + Send + Sync + 'static,
    {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let volume = AudioVolume::new(0.5);
        let stats = AudioThreadStats::new();
        let vol_for_thread = volume.clone();
        let stats_for_thread = stats.clone();
        let factory = Arc::new(factory);

        let join = thread::Builder::new()
            .name("plippa-audio".into())
            .spawn(move || {
                audio_loop(factory, cmd_rx, vol_for_thread, stats_for_thread, block_frames, config);
            })
            .expect("spawn audio thread");

        Self {
            cmd_tx,
            volume,
            stats,
            join: Some(join),
        }
    }

    /// Non-blocking send; returns false if the audio thread is gone.
    pub fn try_send(&self, cmd: AudioCommand) -> bool {
        self.cmd_tx.send(cmd).is_ok()
    }

    pub fn shutdown(mut self) {
        let _ = self.cmd_tx.send(AudioCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    pub fn take_last_error(&self) -> Option<String> {
        self.stats
            .last_error
            .lock()
            .ok()
            .and_then(|mut g| g.take())
    }
}

impl Drop for AudioThreadHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(AudioCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Loop-level (session-wide) rate control: the set speed survives stream changes.
struct RateControl {
    /// Signed set speed (1.0 = pure).
    target: f64,
    change: RateChange,
    /// Bumped on every `SetRate`.
    generation: u64,
    speed_mode: WarpMode,
    selector: VoiceSelector,
    /// WARP_19: continuation of the replaced stream for the next stream start (jog jump into a
    /// neighbouring item); consumed by `finish_start_open`.
    carry: Option<Vec<f32>>,
}

impl RateControl {
    fn new() -> Self {
        Self {
            target: 1.0,
            change: RateChange::Immediate,
            generation: 0,
            speed_mode: SPEED_WARP_DEFAULT,
            // The in-tree WSOLA backs the voice when the `stretch` feature is off.
            selector: VoiceSelector { stretch_available: true },
            carry: None,
        }
    }

    /// Voice the speed family wants for the set speed (the stage may still be ramping).
    fn wanted_voice(&self) -> VoiceKind {
        self.selector.select(self.speed_mode, self.target, false)
    }
}

fn voice_code(v: VoiceKind) -> u8 {
    match v {
        VoiceKind::Bypass => 0,
        VoiceKind::Varispeed => 1,
        VoiceKind::Stretch => 2,
    }
}

struct TrackState {
    path: PathBuf,
    rate_hz: u32,
    channels: usize,
    /// PLAY_02: source `mediaList` position of the item this stream plays. Scan crossings
    /// carry it back to the control plane, which resolves the list entry by identity — a
    /// reorder during the scan must not change which item was heard.
    item_position: u32,
    /// `WindowConfig::decode_safety_factor` (headroom clamp input).
    max_rate_factor: f64,
    /// The decode worker's hand-off and the window it fills: decoded frames, history behind
    /// `read_pos`, lookahead in front, back-windows when reversing (WARP_04/11). The audio
    /// thread only renders from it; it never decodes.
    feed: WindowFeed,
    /// Reading direction published to the worker: -1 reversing (or about to), +1 otherwise.
    dir: i8,
    sink: Box<dyn AudioSink>,
    /// Absolute source frame of the next frame to read: the integer playhead in bypass, the
    /// floor of the stage playhead while varispeed (drives window eviction).
    read_pos: u64,
    /// The varispeed voice; owns the actual playhead (`stage.pos`) in both states.
    stage: RateStage,
    /// The pitch-correct voice, created on first use and kept (its position is mirrored into
    /// `stage.pos`, its rate comes from the stage's ramp).
    stretch: Option<StretchVoice>,
    /// `Bypass` (integer reads, bit-exact), `Varispeed` (the stage renders) or `Stretch`.
    voice: VoiceKind,
    /// Last `RateControl::generation` applied.
    ctl_generation: u64,
    /// Crossfade of hard seeks and bypass exit (preallocated for the longest configured fade).
    xf: Crossfader,
    /// Continuation of the old signal handed to `xf.begin` (preallocated).
    xf_old: Vec<f32>,
    /// One output block (`chunk_frames` frames), preallocated: the read from the window, the
    /// crossfade mix and the output stage all happen in place here.
    block: Vec<f32>,
    decode_started: Instant,
    first_audio_logged: bool,
    /// The heard position never precedes this frame: the landing of the last start/seek, or
    /// the playhead at the last flush (pause) since the sink queue was emptied there.
    floor_frame: u64,
    /// Output frames written since the sink queue was last empty (start, seek, pause).
    written_since_flush: u64,
    /// Source-frame span of every written block since that flush (WARP_03/UI_15): makes the
    /// heard position exact when the rate changed inside the queue.
    spans: WriteSpans,
    /// Most frames a pause may put back (`PAUSE_REQUEUE_MAX_MS`).
    requeue_cap_frames: usize,
    /// The running commit chase (WARP_14); while set the speed family is suspended.
    chase: Option<ChaseRun>,
    /// Frames the next varispeed quantum may render (a shortened landing quantum).
    chase_limit: Option<usize>,
    /// The running hold scan (WARP_17); like a chase it suspends the speed family.
    scan: Option<ScanRun>,
    /// A scan from paused settled on the heard position (or hit a track end): the loop pauses.
    scan_pause: bool,
    /// Duration (ms) of the track in `feed` (0 = unknown) and its length in frames.
    duration_ms: u64,
    len_frames: Option<u64>,
    /// WARP_19 neighbours / join (`None` = off behaviour, the unchanged code path).
    cross: Option<Box<cross::CrossCtl>>,
    /// Frames the playhead was moved by an exchange of a join's feeds (not by playing) since
    /// the loop last read it: the span of the block that straddled it must not count them.
    rebase_shift: f64,
}

/// State of one hold scan: the seek voice is the track's own rate stage (varispeed) or its
/// stretch voice, its rate set per quantum from [`ScanProfile`].
struct ScanRun {
    /// +1 / -1.
    dir: i8,
    /// Seek warp mode sampled at `ScanStart`.
    mode: WarpMode,
    /// Seconds of continuous hold (audio time).
    hold_s: f64,
    /// Started from paused/stopped: the release stops the audio exactly there and pauses.
    standstill: bool,
    /// UI_15: the encoder's scan speed for **this hold**, in permille of the usual scan speed
    /// (`1000` = the tuning in [`ScanProfile`]). Scan-only: never written to the set speed.
    speed_permille: i32,
}

/// State of one commit chase. The seek voice is the track's own rate stage (varispeed) or its
/// stretch voice, driven quantum by quantum through the stage's rate; the set speed is not
/// touched and takes over again at the landing.
struct ChaseRun {
    /// Target source frame.
    target: f64,
    /// Seek warp mode sampled when the chase started.
    mode: WarpMode,
    /// A far target whose waypoint window the worker is still preparing: the chase keeps
    /// swooping from where the audio is (never a stall) until the window arrives and is adopted.
    awaiting_waypoint: bool,
}

/// Outcome of preparing one output block.
enum Block {
    /// `n` frames are ready in `block`.
    Ready(usize),
    /// Source data is not decoded yet.
    NeedData,
    /// Forward end of the stream.
    End,
}

impl TrackState {
    /// Written playhead in source frames.
    fn pos(&self) -> f64 {
        self.stage.pos
    }

    /// Instantaneous rate of the audio just written (1.0 in bypass).
    fn cur_rate(&self) -> f64 {
        match self.voice {
            VoiceKind::Bypass => 1.0,
            _ => self.stage.rate(),
        }
    }

    /// Publish the heard playhead for a sink queue of `queue_frames`.
    fn publish_position(&self, stats: &AudioThreadStats, queue_frames: u64) {
        // Until more than the queue has been written since it was empty, the listener is
        // still at the anchor; after that heard = written - source span of the queue (signed).
        // The span comes from the write history, so a rate change (a scan release, UI_15) does
        // not move the reported position while old-rate audio is still queued.
        let heard = if self.written_since_flush < queue_frames {
            self.floor_frame
        } else {
            heard_position_src(
                self.logical_pos(),
                self.spans.source_frames(queue_frames, self.cur_rate()),
                0,
            )
        };
        stats.position_src_frames.store(heard, Ordering::Relaxed);
        stats.sink_queue_frames.store(queue_frames, Ordering::Relaxed);
    }

    /// Publish rate, `warp_active` and the voice in use.
    fn publish_voice(&self, stats: &AudioThreadStats) {
        let rate = self.cur_rate();
        stats
            .rate_micro
            .store((rate * 1_000_000.0).round() as i32, Ordering::Relaxed);
        stats
            .warp_active
            .store(self.voice != VoiceKind::Bypass, Ordering::Relaxed);
        stats.speed_voice.store(voice_code(self.voice), Ordering::Relaxed);
    }

    /// Re-anchor after a start or seek landing: written == heard == `frame`, queue empty.
    fn anchor(&mut self, frame: u64, stats: &AudioThreadStats) {
        self.read_pos = frame;
        self.stage.reposition(frame as f64);
        self.floor_frame = frame;
        self.written_since_flush = 0;
        self.spans.clear();
        let rate_hz = self.rate_hz;
        stats
            .position_base_ms
            .store(frame_to_ms(frame, rate_hz), Ordering::Relaxed);
        self.publish_position(stats, 0);
    }

    /// Frames decoded and not yet written.
    fn ahead(&self) -> usize {
        self.feed.window.ahead(self.read_pos)
    }

    /// Take over a new set speed (idempotent per generation). Entering varispeed from bypass
    /// is immediate (no crossfade, the stage starts at exactly 1.0 on the integer position);
    /// the way back to bypass happens in `render_block` once the ramp has settled on 1.0.
    fn apply_rate_control(&mut self, ctl: &RateControl, cfg: &LoopCfg, stats: &AudioThreadStats) {
        if self.ctl_generation == ctl.generation {
            return;
        }
        self.ctl_generation = ctl.generation;
        if self.seek_active() {
            // The chase / scan owns the rate; the new set speed is applied at the landing /
            // release.
            return;
        }
        if self.voice == VoiceKind::Bypass && ctl.target != 1.0 {
            self.stage.enter_from_bypass(self.read_pos as f64);
            if ctl.wanted_voice() == VoiceKind::Stretch {
                self.switch_voice(VoiceKind::Stretch, cfg, stats);
            } else {
                self.voice = VoiceKind::Varispeed;
            }
        }
        let spec = match ctl.change {
            RateChange::Tempo => RampSpec::Exp { tau_ms: cfg.rate.tau_tempo_ms },
            RateChange::Reset => RampSpec::Exp { tau_ms: cfg.rate.tau_reset_ms },
            RateChange::Immediate => RampSpec::Immediate,
        };
        self.stage.set_target(ctl.target, spec);
        self.reconcile_voice(ctl, cfg, stats);
        self.publish_voice(stats);
    }

    fn ensure_stretch(&mut self, cfg: &LoopCfg) {
        if self.stretch.is_none() {
            self.stretch = Some(StretchVoice::new(
                &cfg.stretch,
                self.feed.window.channels(),
                self.rate_hz,
            ));
        }
    }

    /// Switch between the rate voices (Varispeed <-> Stretch, or Bypass -> Stretch right after
    /// `enter_from_bypass`) through the `XFADE_VOICE_MS` equal-power crossfade at the current
    /// playhead: the old voice's continuation fades out while the new one (primed at the same
    /// source position) fades in.
    fn switch_voice(&mut self, to: VoiceKind, cfg: &LoopCfg, stats: &AudioThreadStats) {
        if self.voice == to {
            return;
        }
        if self.written_since_flush > 0 {
            let rate_hz = self.rate_hz;
            let fade = XfadeConfig::frames(cfg.xfade.xfade_voice_ms, rate_hz)
                .min(self.xf.capacity_frames());
            let got = self.old_continuation(fade);
            let ch = self.feed.window.channels();
            self.xf.begin(fade, &self.xf_old[..got * ch]);
        }
        if to == VoiceKind::Stretch {
            self.ensure_stretch(cfg);
            let pos = self.stage.pos;
            if let Some(sv) = self.stretch.as_mut() {
                sv.request_prime(pos);
            }
        }
        self.voice = to;
        self.publish_voice(stats);
    }

    /// Keep the voice in line with the speed mode and the set rate: a mode change or a
    /// negative rate leaves the stretcher at once (reverse is always varispeed); a positive
    /// rate in pitch-correct mode (re)enters it once the rate is a valid stretch rate.
    fn reconcile_voice(&mut self, ctl: &RateControl, cfg: &LoopCfg, stats: &AudioThreadStats) {
        if self.seek_active() {
            return;
        }
        match (self.voice, ctl.wanted_voice()) {
            (VoiceKind::Stretch, VoiceKind::Varispeed) => {
                self.switch_voice(VoiceKind::Varispeed, cfg, stats)
            }
            (VoiceKind::Varispeed, VoiceKind::Stretch)
                if self.stage.rate() >= STRETCH_MIN_RATE && !self.stage.stalled() =>
            {
                self.switch_voice(VoiceKind::Stretch, cfg, stats)
            }
            _ => {}
        }
    }

    /// Continuation of what is currently being output, `frames` frames, into `xf_old`
    /// (returns the frames available).
    fn old_continuation(&mut self, frames: usize) -> usize {
        let ch = self.feed.window.channels();
        match self.voice {
            VoiceKind::Bypass => self.feed.window.read_forward(self.read_pos, &mut self.xf_old[..frames * ch]),
            VoiceKind::Stretch => {
                // The stretcher cannot be previewed without consuming it: the caller is
                // leaving it (switch / exit) or re-primes it right after (seek).
                let rate = self.stage.rate();
                let mut done = 0;
                if let Some(sv) = self.stretch.as_mut() {
                    while done < frames {
                        let n = (frames - done).min(sv.block_frames());
                        let dst = &mut self.xf_old[done * ch..(done + n) * ch];
                        match sv.render(&self.feed.window, self.feed.eof(), rate, dst) {
                            Quantum::Rendered { frames: f, .. } => done += f,
                            _ => break,
                        }
                    }
                }
                done
            }
            _ => {
                self.stage.preview(&self.feed.window, &mut self.xf_old[..frames * ch]);
                frames
            }
        }
    }

    /// Hard seek to source `frame` (WARP_04): inside the window no decoder seek is issued;
    /// outside it a new worker epoch starts (the worker's decoder lands there, possibly slowly:
    /// nothing is rendered until the first block arrives) and the window restarts. If the
    /// listener is hearing
    /// this stream the new position is joined to the audio already on its way to the sink by
    /// an equal-power crossfade instead of a sink flush.
    fn hard_seek(
        &mut self,
        frame: u64,
        audible: bool,
        cfg: &LoopCfg,
        stats: &AudioThreadStats,
    ) -> Result<(), plippa_audio::AudioError> {
        let fade = cfg.xfade.seek_frames(self.rate_hz);
        self.hard_seek_fade(frame, audible, fade, cfg, stats)
    }

    /// [`Self::hard_seek`] with an explicit crossfade length in frames (the far-chase waypoint
    /// jump uses `XFADE_WAYPOINT_MS`).
    fn hard_seek_fade(
        &mut self,
        frame: u64,
        audible: bool,
        fade_frames: usize,
        _cfg: &LoopCfg,
        stats: &AudioThreadStats,
    ) -> Result<(), plippa_audio::AudioError> {
        let in_window = self.feed.window.contains(frame);
        let landing = frame;
        if !in_window {
            stats.decoder_seeks.fetch_add(1, Ordering::Relaxed);
        }
        // Something of this stream was written since the queue was last empty: it is (or is
        // about to be) audible, so fade from where it was heading. With nothing written yet
        // there is nothing to fade from; a fade still running from the previous seek keeps its
        // original "old" signal (that is what is audible).
        let mixing = audible && self.written_since_flush > 0;
        if mixing {
            let fade = fade_frames.min(self.xf.capacity_frames());
            let got = self.old_continuation(fade);
            let ch = self.feed.window.channels();
            self.xf.begin(fade, &self.xf_old[..got * ch]);
            stats.seek_crossfades.fetch_add(1, Ordering::Relaxed);
        } else if !(audible && self.xf.active()) {
            self.xf.cancel();
        }
        if !in_window {
            self.feed.reposition(landing, landing, self.dir);
            self.feed.set_start_prefetch(self.start_prefetch_frames());
        }
        stats.frames_written.store(0, Ordering::Relaxed);
        self.anchor(landing, stats);
        if in_window {
            self.feed.publish(self.read_pos, self.dir);
        }
        if self.voice == VoiceKind::Stretch {
            if let Some(sv) = self.stretch.as_mut() {
                sv.request_prime(landing as f64);
            }
        }
        Ok(())
    }

    /// Begin a commit chase to `target` (source frame), or retarget the running one.
    ///
    /// A new chase takes the seek voice over from the speed voice (same kind: only the rate
    /// source changes, no crossfade; different kind: the 40 ms voice crossfade; from bypass at
    /// exactly 1.0x the entry is seamless). A target farther than the controller can travel in
    /// `CHASE_MAX_TIME_S` (or beyond what the window can hold) first jumps to the waypoint
    /// `target - dir x WAYPOINT_LEAD_S` through a `XFADE_WAYPOINT_MS` crossfade; the swoop then
    /// covers the last stretch audibly.
    fn start_chase(
        &mut self,
        target: u64,
        mode: WarpMode,
        cfg: &LoopCfg,
        stats: &AudioThreadStats,
    ) {
        let fs = f64::from(self.rate_hz);
        let t = target as f64;
        let p = if self.voice == VoiceKind::Bypass {
            self.read_pos as f64
        } else {
            self.stage.pos
        };
        let d = t - p;
        let mode = match &self.chase {
            Some(run) => run.mode,
            None => {
                if d.abs() < 1.0 {
                    return;
                }
                let forward = d > 0.0;
                let want_stretch = forward
                    && plippa_core::seek_voice(mode, ScanDir::Forward) == VoiceKind::Stretch
                    && self.stage.rate() >= STRETCH_MIN_RATE;
                if self.voice == VoiceKind::Bypass {
                    self.stage.enter_from_bypass(self.read_pos as f64);
                    if want_stretch {
                        self.switch_voice(VoiceKind::Stretch, cfg, stats);
                    } else {
                        self.voice = VoiceKind::Varispeed;
                    }
                } else if want_stretch && self.voice == VoiceKind::Varispeed {
                    self.switch_voice(VoiceKind::Stretch, cfg, stats);
                } else if !want_stretch && self.voice == VoiceKind::Stretch {
                    self.switch_voice(VoiceKind::Varispeed, cfg, stats);
                }
                stats.chases_started.fetch_add(1, Ordering::Relaxed);
                mode
            }
        };
        self.chase = Some(ChaseRun { target: t, mode, awaiting_waypoint: false });
        // A waypoint prepared for an earlier target is stale now.
        self.feed.discard_waypoint();
        stats.chase_active.store(true, Ordering::SeqCst);
        let r_max = if self.voice == VoiceKind::Stretch && d > 0.0 {
            cfg.chase.r_max_stretch
        } else {
            cfg.chase.r_max_vari
        };
        let beyond_reach = d.abs() > 0.9 * self.feed.window.cap_frames() as f64;
        let far = d.abs() / fs > cfg.chase.far_jump_s(r_max) || beyond_reach;
        self.dir = if d < 0.0 { -1 } else { 1 };
        if far {
            let sign = if d < 0.0 { -1.0 } else { 1.0 };
            let lead = (cfg.chase.waypoint_lead_s * fs).max(0.0);
            let w = (t - sign * lead).max(0.0) as u64;
            let ready = (0.15 * fs) as usize;
            let w_held = self.feed.window.contains_range(w.saturating_sub(ready as u64), 2 * ready);
            if !w_held && self.feed.waypoint_fits(lead as u64) {
                // The decoder has to seek: the worker prepares the waypoint window while the
                // audio keeps swooping from here; `adopt_waypoint_jump` joins them later.
                if let Some(run) = self.chase.as_mut() {
                    run.awaiting_waypoint = true;
                }
                self.feed.hint(Some(Hint {
                    target_frame: target,
                    direction: if d < 0.0 { -1 } else { 1 },
                    waypoint_lead_frames: lead as u64,
                }));
            } else {
                let fade = XfadeConfig::frames(cfg.xfade.xfade_waypoint_ms, self.rate_hz);
                if let Err(e) = self.hard_seek_fade(w, true, fade, cfg, stats) {
                    set_error(stats, e.to_string());
                }
                stats.waypoint_jumps.fetch_add(1, Ordering::Relaxed);
                self.feed.hint(None);
            }
        } else {
            self.feed.hint(Some(Hint {
                target_frame: target,
                direction: if d < 0.0 { -1 } else { 1 },
                waypoint_lead_frames: 0,
            }));
        }
    }

    /// While a far chase waits for its waypoint window: when the worker has delivered it for
    /// the current target, and it holds at least 150 ms in the travel direction at the waypoint
    /// `T - dir x lead`, join it through the `XFADE_WAYPOINT_MS` equal-power crossfade, swap it
    /// in as the playing window (the old contents are replaced, the waypoint box dropped, so
    /// memory returns to the window cap) and let the swoop cover the last `lead` seconds. A
    /// chase that has already travelled within `lead` of the target just drops the window.
    fn adopt_waypoint_jump(&mut self, cfg: &LoopCfg, stats: &AudioThreadStats) {
        let Some(run) = self.chase.as_ref() else { return };
        if !run.awaiting_waypoint || !self.feed.has_waypoint() {
            return;
        }
        let target = run.target;
        let Some(wp) = self.feed.take_waypoint() else { return };
        let fs = f64::from(self.rate_hz);
        let d = target - self.stage.pos;
        let sign = if d < 0.0 { -1.0 } else { 1.0 };
        let lead = (cfg.chase.waypoint_lead_s * fs).max(0.0);
        let ready = (0.15 * fs) as usize;
        let w = (target - sign * lead).max(0.0) as u64;
        let fits = wp.window.channels() == self.feed.window.channels()
            && wp.window.len_frames() <= self.feed.window.cap_frames();
        let usable = wp.target_frame == target as u64
            && fits
            && if sign > 0.0 {
                wp.window.contains_range(w, ready)
            } else {
                wp.window.contains_range(w.saturating_sub(ready as u64), ready + 1)
            };
        if let Some(run) = self.chase.as_mut() {
            run.awaiting_waypoint = wp.target_frame != target as u64;
        }
        if wp.target_frame != target as u64 {
            return; // stale: the worker is preparing the newer one
        }
        self.feed.hint(None);
        if d.abs() <= lead + 0.25 * fs {
            return; // already arrived within reach: nothing to skip
        }
        if let Some(run) = self.chase.as_mut() {
            run.awaiting_waypoint = false;
        }
        if !usable {
            // Cannot be joined (clamped by the file end, or too short): old behaviour.
            let fade = XfadeConfig::frames(cfg.xfade.xfade_waypoint_ms, self.rate_hz);
            if let Err(e) = self.hard_seek_fade(w, true, fade, cfg, stats) {
                set_error(stats, e.to_string());
            }
            stats.waypoint_jumps.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.written_since_flush > 0 {
            let fade = XfadeConfig::frames(cfg.xfade.xfade_waypoint_ms, self.rate_hz)
                .min(self.xf.capacity_frames());
            let got = self.old_continuation(fade);
            let ch = self.feed.window.channels();
            self.xf.begin(fade, &self.xf_old[..got * ch]);
            stats.seek_crossfades.fetch_add(1, Ordering::Relaxed);
        } else if !self.xf.active() {
            self.xf.cancel();
        }
        let dir = self.dir;
        let adopted = self.feed.adopt_waypoint(&wp, w, dir);
        debug_assert!(adopted);
        drop(wp);
        stats.frames_written.store(0, Ordering::Relaxed);
        self.anchor(w, stats);
        self.feed.publish(self.read_pos, dir);
        if self.voice == VoiceKind::Stretch {
            if let Some(sv) = self.stretch.as_mut() {
                sv.request_prime(w as f64);
            }
        }
        stats.waypoint_jumps.fetch_add(1, Ordering::Relaxed);
        stats.waypoints_adopted.fetch_add(1, Ordering::Relaxed);
    }

    /// Per-quantum chase control (called from `render_block` before the voice renders): lands
    /// the chase when the target is reached, otherwise sets the seek voice's rate for the next
    /// quantum (and shortens a varispeed landing quantum so it ends exactly on the target).
    fn chase_block(&mut self, ctl: &RateControl, cfg: &LoopCfg, stats: &AudioThreadStats) {
        self.adopt_waypoint_jump(cfg, stats);
        let Some(run) = self.chase.as_ref() else { return };
        let target = run.target;
        let fs = f64::from(self.rate_hz);
        let d = target - self.stage.pos;
        self.dir = if d < 0.0 { -1 } else { 1 };
        if self.voice == VoiceKind::Stretch && d < 0.0 {
            // Backward travel is always varispeed.
            self.switch_voice(VoiceKind::Varispeed, cfg, stats);
        }
        let stretch = self.voice == VoiceKind::Stretch;
        let mut r_max = if stretch {
            cfg.chase.r_max_stretch
        } else {
            cfg.chase.r_max_vari
        };
        let sustainable = stats.max_sustainable_rate_milli.load(Ordering::Relaxed) as f64 / 1000.0;
        if sustainable > 0.0 {
            r_max = r_max.min(sustainable);
        }
        let q = if stretch {
            self.stretch.as_ref().map_or(256, |s| s.block_frames())
        } else {
            cfg.rate.quantum_frames
        };
        let mut r = chase_step(self.stage.rate(), &cfg.chase, r_max, d, fs, q as f64 / fs);
        if stretch {
            // The stretcher cannot shorten its quantum: land within the tolerance.
            if d.abs() <= cfg.chase.land_eps_frames {
                self.finish_chase(ctl, cfg, stats);
                return;
            }
        } else {
            match final_quantum(d, r, q) {
                Some(None) => {
                    self.stage.pos = target;
                    self.finish_chase(ctl, cfg, stats);
                    return;
                }
                Some(Some((n, exact))) => {
                    self.chase_limit = Some(n);
                    r = exact;
                }
                None => self.chase_limit = None,
            }
        }
        self.stage.set_target(r, RampSpec::Immediate);
    }

    /// The swoop reached its target: the speed family takes over from the current rate (the
    /// ramp runs to the set speed with `tau_tempo_ms`), the speed voice is reconciled (a
    /// different kind joins through the voice crossfade) and the pre-decode hint is dropped.
    fn finish_chase(&mut self, ctl: &RateControl, cfg: &LoopCfg, stats: &AudioThreadStats) {
        self.chase = None;
        self.chase_limit = None;
        stats.chase_active.store(false, Ordering::SeqCst);
        self.feed.hint(None);
        self.stage.set_target(
            ctl.target,
            RampSpec::Exp { tau_ms: cfg.rate.tau_tempo_ms },
        );
        self.reconcile_voice(ctl, cfg, stats);
        self.publish_voice(stats);
    }

    /// Abandon a running chase where it stands (hard seek, pause): the stage goes straight to
    /// the set speed. Returns the target it was heading for.
    fn cancel_chase(
        &mut self,
        ctl: &RateControl,
        cfg: &LoopCfg,
        stats: &AudioThreadStats,
    ) -> Option<f64> {
        self.cancel_chase_with(ctl, cfg, stats, false)
    }

    /// [`Self::cancel_chase`]; with `keep_rate` the stage keeps its instantaneous rate (a scan
    /// takes over from it, so the pitch stays continuous).
    fn cancel_chase_with(
        &mut self,
        ctl: &RateControl,
        cfg: &LoopCfg,
        stats: &AudioThreadStats,
        keep_rate: bool,
    ) -> Option<f64> {
        let target = self.chase.as_ref().map(|c| c.target)?;
        self.chase = None;
        self.chase_limit = None;
        stats.chase_active.store(false, Ordering::SeqCst);
        self.feed.hint(None);
        self.feed.discard_waypoint();
        if !keep_rate {
            self.stage.set_target(ctl.target, RampSpec::Immediate);
            self.reconcile_voice(ctl, cfg, stats);
        }
        Some(target)
    }

    /// True while a chase or a scan owns the rate (the speed family is suspended).
    fn seek_active(&self) -> bool {
        self.chase.is_some() || self.scan.is_some()
    }

    /// Begin a hold scan in `dir`, or re-press a running one (a new direction glides through
    /// zero, the hold clock restarts). Takes the seek voice over from the speed voice exactly
    /// like a chase (same kind: only the rate source changes; different kinds: the 40 ms voice
    /// crossfade; from bypass at 1.0x seamless) and starts from the current instantaneous rate.
    ///
    /// UI_15: a fresh scan starts at the ladder's default (100 % of the usual scan speed); a
    /// re-press in the other direction keeps the speed the encoder already selected for this
    /// hold. The set speed is never read or written here.
    fn start_scan(
        &mut self,
        dir: ScanDir,
        mode: WarpMode,
        from_paused: bool,
        ctl: &RateControl,
        cfg: &LoopCfg,
        stats: &AudioThreadStats,
    ) {
        let sign: i8 = if dir == ScanDir::Backward { -1 } else { 1 };
        // T11: a scan cancels a running chase and starts from the rate it had.
        let _ = self.cancel_chase_with(ctl, cfg, stats, true);
        if let Some(run) = self.scan.as_mut() {
            run.dir = sign;
            run.mode = mode;
            run.hold_s = 0.0;
            return;
        }
        let want_stretch = sign > 0
            && plippa_core::seek_voice(mode, dir) == VoiceKind::Stretch
            && self.stage.rate() >= STRETCH_MIN_RATE;
        if self.voice == VoiceKind::Bypass {
            self.stage.enter_from_bypass(self.read_pos as f64);
            if want_stretch {
                self.switch_voice(VoiceKind::Stretch, cfg, stats);
            } else {
                self.voice = VoiceKind::Varispeed;
            }
        } else if want_stretch && self.voice == VoiceKind::Varispeed {
            self.switch_voice(VoiceKind::Stretch, cfg, stats);
        } else if !want_stretch && self.voice == VoiceKind::Stretch {
            self.switch_voice(VoiceKind::Varispeed, cfg, stats);
        }
        self.dir = sign;
        self.scan = Some(ScanRun {
            dir: sign,
            mode,
            hold_s: 0.0,
            standstill: from_paused,
            speed_permille: plippa_core::SCAN_SPEED_DEFAULT_PERMILLE,
        });
        self.publish_voice(stats);
    }

    /// UI_15: the encoder's speed for the running hold. Takes effect from the next quantum
    /// (the spin-up lag glides to the new target); nothing happens without a scan.
    fn set_scan_speed(&mut self, permille: i32) {
        if let Some(run) = self.scan.as_mut() {
            run.speed_permille = permille;
        }
    }

    /// UI_15: end the scan **now** — no brake. Playing: the rate jumps straight to the set
    /// speed (`ctl.target`, whatever it is) so what is heard and what is drawn hold their
    /// position. From paused: the sink is flushed and the unheard part put back, so the
    /// playhead settles exactly where the listener was and stays there.
    ///
    /// Returns `true` when the scan started from paused (the caller pauses the transport).
    /// Harmless without a running scan (returns `false`, touches nothing).
    fn stop_scan_now(&mut self, ctl: &RateControl, cfg: &LoopCfg, stats: &AudioThreadStats) -> bool {
        let Some(run) = self.scan.take() else {
            return false;
        };
        self.cross_scan_over(stats);
        if run.standstill {
            // At the scan rate, before the set speed is restored: the queued frames were
            // produced at that rate.
            self.pause_flush(stats, true);
        } else {
            // UI_19: a release while playing is exact too. The sink still holds up to a buffer
            // of scan-rate audio, which (left in place) plays out *after* the release: the
            // heard position keeps travelling in the scan direction for its media span — the
            // picture rewinding/forwarding an extra couple of hundred ms, then turning round.
            // Dropping it and continuing from the **heard** position makes the release land
            // where the listener was, which is what the screen shows.
            self.release_to_heard(stats);
        }
        self.stage.set_target(ctl.target, RampSpec::Immediate);
        self.reconcile_voice(ctl, cfg, stats);
        self.publish_voice(stats);
        run.standstill
    }

    /// Per-quantum scan control (called from `render_block` before the voice renders): sets the
    /// seek voice's rate for the next quantum from the profile. Simulated time only: the hold
    /// clock advances by the quantum rendered.
    fn scan_block(&mut self, cfg: &LoopCfg, stats: &AudioThreadStats) {
        let Some(run) = self.scan.as_ref() else { return };
        let (dir, mode, hold_s, speed, standstill) =
            (run.dir, run.mode, run.hold_s, run.speed_permille, run.standstill);
        let fs = f64::from(self.rate_hz);
        // Stretch cannot run backward or through zero: leave it for varispeed.
        if self.voice == VoiceKind::Stretch && (dir < 0 || self.stage.rate() < STRETCH_MIN_RATE) {
            self.switch_voice(VoiceKind::Varispeed, cfg, stats);
        }
        let stretch = self.voice == VoiceKind::Stretch;
        let sdir = if dir < 0 { ScanDir::Backward } else { ScanDir::Forward };
        let voice_cap = if stretch {
            plippa_core::max_rate(mode, sdir, SeekDriver::Scan)
        } else {
            plippa_core::MAX_SEEK_VARI_SCAN
        };
        let milli = stats.max_sustainable_rate_milli.load(Ordering::Relaxed);
        let cap = scan_rate_cap(voice_cap, (milli > 0).then(|| milli as f64 / 1000.0));
        let q = if stretch {
            self.stretch.as_ref().map_or(256, |s| s.block_frames())
        } else {
            cfg.rate.quantum_frames
        };
        let dt = q as f64 / fs;
        let (target, tau) =
            cfg.scan
                .scan_target(dir, hold_s, cap, plippa_core::scan_speed_scale(speed));
        // UI_19: a scan that started from a **standstill** must never travel through forward
        // playback. A paused transport leaves the stage at the set speed (1.0), so stepping
        // from it played a burst of forward audio before the rate crossed zero — and at the
        // low ladder speeds the crossing is slow enough to hear as a jump. A direction change
        // inside a standstill hold starts from rest for the same reason.
        let from = if standstill && self.stage.rate() * f64::from(dir) < 0.0 {
            0.0
        } else {
            self.stage.rate()
        };
        let r = cfg.scan.step(from, target, tau, dt);
        self.stage.set_target(r, RampSpec::Immediate);
        if let Some(run) = self.scan.as_mut() {
            run.hold_s += dt;
        }
    }

    /// Abandon a running scan where it stands (hard seek, pause): the stage goes straight to
    /// the set speed. Returns whether a scan was running.
    fn cancel_scan(&mut self, ctl: &RateControl, cfg: &LoopCfg, stats: &AudioThreadStats) -> bool {
        if self.scan.is_none() {
            return false;
        }
        self.cross_scan_over(stats);
        self.scan = None;
        self.scan_pause = false;
        self.stage.set_target(ctl.target, RampSpec::Immediate);
        self.reconcile_voice(ctl, cfg, stats);
        true
    }

    /// The scan from paused is over (it ran into a track end/start): the pause itself (sink
    /// flush, put-back of the unheard part) is done here; the set speed is restored after it
    /// so the put-back is measured at the rate the queued audio was written at.
    fn complete_scan_pause(&mut self, ctl: &RateControl, cfg: &LoopCfg, stats: &AudioThreadStats) {
        self.scan_pause = false;
        self.pause_flush(stats, true);
        self.stage.set_target(ctl.target, RampSpec::Immediate);
        self.reconcile_voice(ctl, cfg, stats);
        self.publish_voice(stats);
    }

    /// Frames the worker should hold ahead before the first write of an epoch: one start
    /// chunk, or the stretch voice's lag of lookahead (it cannot produce output before it).
    fn start_prefetch_frames(&self) -> usize {
        if self.voice == VoiceKind::Stretch {
            STRETCH_PREFETCH_BYTES / (4 * self.channels.max(1))
        } else {
            // One start chunk: first audio never waits for more (and the count is exact,
            // whatever the scheduling).
            START_CHUNK_FRAMES
        }
    }

    /// Apply what the decode worker has delivered (never blocks) and publish the window view.
    /// Returns a worker error, if any.
    fn pump(&mut self, stats: &AudioThreadStats) -> Result<(), String> {
        let p = self.feed.pump(self.read_pos, self.dir);
        if let Some(landing) = p.rebased {
            // The decoder landed elsewhere than requested: follow it.
            self.anchor(landing, stats);
            self.feed.publish(self.read_pos, self.dir);
        }
        if let Some(e) = self.feed.take_error() {
            return Err(e);
        }
        let speed = self.feed.stats().speed_milli.load(Ordering::Relaxed);
        stats
            .max_sustainable_rate_milli
            .store((speed as f64 * self.max_rate_factor) as u64, Ordering::Relaxed);
        self.note_peaks(stats);
        Ok(())
    }

    /// Pause: flush the sink, then put back the not-yet-heard part of what was written so
    /// resume continues exactly where the listener stopped (a flush alone would skip it). The
    /// frames are still in the window's history, so this is only a move of the playhead.
    /// The put-back is measured in **source frames** from the write history (`spans`), not
    /// `queued x rate`: during a ramp (or a scan release, UI_15) the queued frames were written
    /// at different rates, and the listener must land exactly where they stopped hearing.
    fn pause_flush(&mut self, stats: &AudioThreadStats, was_playing: bool) {
        let queued = if was_playing { self.sink.queue_frames() } else { 0 };
        if let Err(e) = self.sink.flush() {
            eprintln!("plippa-engine: pause flush failed: {e}");
        }
        let mut back = queued
            .min(self.written_since_flush)
            .min(self.requeue_cap_frames as u64);
        let back_src = self.spans.source_frames(back, self.cur_rate());
        let new_pos = self.pos() - back_src;
        let held = new_pos >= self.feed.window.start_frame() as f64
            && new_pos <= self.feed.window.end_frame() as f64;
        if !held {
            back = 0;
        }
        if back > 0 {
            self.stage.pos = new_pos;
            self.read_pos = new_pos.max(0.0) as u64;
            self.xf.rewind(back as usize);
            if self.voice == VoiceKind::Stretch {
                if let Some(sv) = self.stretch.as_mut() {
                    sv.request_prime(new_pos);
                }
            }
        }
        self.written_since_flush = 0;
        self.spans.clear();
        self.floor_frame = self.pos().max(0.0) as u64;
        self.publish_position(stats, 0);
    }

    /// A hold scan was released **while playing** (UI_19): drop the queued scan-rate audio and
    /// continue from the heard position.
    ///
    /// Left in place, the queue plays out *after* the release, so the listener keeps travelling
    /// in the scan direction for the queue's media span (~170 ms at 2x, far more at the top of
    /// the ladder) before playback turns around. The picture follows the audio, so the release
    /// looked like it over-ran and then jumped back to the real position. Dropping the queue and
    /// moving the playhead to what was already heard makes the release land exactly where the
    /// screen is, and playback carries on forward from there.
    ///
    /// The destination is inside the decode window (the frames were just read from it), so this
    /// is only a move of the playhead; if it ever is not, the queue is left alone rather than
    /// stepping the playhead into audio that cannot be rendered.
    fn release_to_heard(&mut self, stats: &AudioThreadStats) {
        let queued = self
            .sink
            .queue_frames()
            .min(self.written_since_flush)
            .min(self.requeue_cap_frames as u64);
        let heard = self.pos() - self.spans.source_frames(queued, self.cur_rate());
        let held = queued > 0
            && heard >= self.feed.window.start_frame() as f64
            && heard <= self.feed.window.end_frame() as f64;
        if !held {
            return;
        }
        if let Err(e) = self.sink.flush() {
            eprintln!("plippa-engine: scan release flush failed: {e}");
        }
        self.stage.pos = heard;
        self.read_pos = heard.max(0.0) as u64;
        // The queued audio is discarded, so any fade prepared against it is stale.
        self.xf.cancel();
        if self.voice == VoiceKind::Stretch {
            if let Some(sv) = self.stretch.as_mut() {
                sv.request_prime(heard);
            }
        }
        self.written_since_flush = 0;
        self.spans.clear();
        self.floor_frame = self.pos().max(0.0) as u64;
        self.publish_position(stats, 0);
    }

    /// The playhead shift of a feed exchange since the last call (see `rebase_shift`).
    fn take_rebase_shift(&mut self) -> f64 {
        std::mem::take(&mut self.rebase_shift)
    }

    /// SEAM_01: settle a join before a command rebases the playhead (seek, chase, scan start,
    /// a rate change). The exchange restarts the heard-position bookkeeping, but audio is
    /// still queued behind the sink: keep the written count, so the seek that follows still
    /// crossfades from what is about to be heard instead of cutting it.
    fn settle_join_for_rebase(&mut self, stats: &AudioThreadStats) {
        if !self.join_active() {
            return;
        }
        let written = self.written_since_flush;
        self.resolve_join(stats);
        self.written_since_flush = written;
    }

    /// Hand over from the (settled, exactly 1.0x) varispeed stage to the bypass path at
    /// `round(pos)` through a short equal-power crossfade (`XFADE_BYPASS_MS`).
    fn exit_to_bypass(&mut self, cfg: &LoopCfg, stats: &AudioThreadStats) {
        let pos = self.pos();
        let target = pos.round().max(0.0);
        let audible = self.written_since_flush > 0;
        let from_stretch = self.voice == VoiceKind::Stretch;
        if audible && (from_stretch || pos.fract() != 0.0) {
            let rate_hz = self.rate_hz;
            let ms = if from_stretch {
                cfg.xfade.xfade_voice_ms
            } else {
                cfg.xfade.xfade_bypass_ms
            };
            let fade = XfadeConfig::frames(ms, rate_hz).min(self.xf.capacity_frames());
            let got = self.old_continuation(fade);
            let ch = self.feed.window.channels();
            self.xf.begin(fade, &self.xf_old[..got * ch]);
        } else if !audible {
            self.xf.cancel();
        }
        self.read_pos = target as u64;
        self.stage.pos = target;
        self.voice = VoiceKind::Bypass;
        self.publish_voice(stats);
    }

    /// Prepare the next output block in `self.block` (position bookkeeping included).
    ///
    /// A block that finds no data to render (`NeedData`) leaves a **forward** scan's clock and
    /// rate where they were: the clock is audio time (frames rendered), so waiting for the
    /// decoder must not spin a scan up (and a crossing hold or a loaded worker cannot change
    /// what the audio sounds like). A backward scan keeps marching: it may be waiting for a
    /// back-window that the worker only places once the rate has turned negative.
    fn render_block(
        &mut self,
        ctl: &mut RateControl,
        cfg: &LoopCfg,
        stats: &AudioThreadStats,
    ) -> Block {
        let before = self.scan.as_ref().map(|s| (s.hold_s, s.dir, self.stage.rate()));
        let block = self.render_block_inner(ctl, cfg, stats);
        if matches!(block, Block::NeedData) {
            if let (Some((hold_s, dir, rate)), Some(run)) = (before, self.scan.as_mut()) {
                if run.dir == dir && dir > 0 {
                    run.hold_s = hold_s;
                    self.stage.set_target(rate, RampSpec::Immediate);
                }
            }
        }
        block
    }

    fn render_block_inner(
        &mut self,
        ctl: &mut RateControl,
        cfg: &LoopCfg,
        stats: &AudioThreadStats,
    ) -> Block {
        self.apply_rate_control(ctl, cfg, stats);
        self.reconcile_voice(ctl, cfg, stats);
        // Direction for the worker: reversing as soon as the set speed is negative (the
        // back-window is fetched before the rate actually crosses zero).
        self.dir = if let Some(run) = self.scan.as_ref() {
            // A scan tells the worker its direction at once (back-windows before the rate
            // actually crosses zero).
            run.dir
        } else if ctl.target < 0.0 || (self.voice != VoiceKind::Bypass && self.stage.rate() < 0.0)
        {
            -1
        } else {
            1
        };
        if self.chase.is_some() {
            self.chase_block(ctl, cfg, stats);
        }
        if self.scan.is_some() {
            // WARP_19: crossing control first, so a hold at the edge leaves the scan clock and
            // rate profile untouched (the scan resumes at the rate it had).
            if self.cross.is_some() {
                if let Some(hold) = self.cross_step(cfg, stats) {
                    return hold;
                }
            }
            if self.scan.is_some() {
                self.scan_block(cfg, stats);
            }
        }
        if !self.seek_active() && self.voice != VoiceKind::Bypass && self.stage.at_unity() {
            self.exit_to_bypass(cfg, stats);
        }
        // SEAM_01: a join of the next track at normal speed (never while a scan owns it). A
        // join left over when the voice is no longer the bypass one (a rate change) is settled
        // first: nothing but `seamless_step` exchanges the feeds of a non-scan join.
        if self.scan.is_none() && self.join_active() && self.voice != VoiceKind::Bypass {
            self.resolve_join(stats);
        }
        if self.scan.is_none() && self.cross.is_some() {
            if let Some(hold) = self.seamless_step(cfg, stats) {
                return hold;
            }
        }
        let ch = self.feed.window.channels();
        if self.voice == VoiceKind::Stretch {
            let q = self.stretch.as_ref().map_or(256, |s| s.block_frames());
            let rate = self.stage.peek_rate(q);
            let Some(sv) = self.stretch.as_mut() else {
                return Block::NeedData;
            };
            return match sv.render(&self.feed.window, self.feed.eof(), rate, &mut self.block) {
                Quantum::Rendered { frames, .. } => {
                    self.stage.advance_ramp(frames);
                    self.stage.pos = sv.pos();
                    self.xf.process(&mut self.block[..frames * ch]);
                    self.read_pos = self.stage.pos.max(0.0) as u64;
                    // Re-evaluate the landing right away so the hand-over (rate ramp to the set
                    // speed) starts with the very next block.
                    let chase_target = self.chase.as_ref().map(|c| c.target);
                    if let Some(target) = chase_target {
                        if (target - self.stage.pos).abs() <= cfg.chase.land_eps_frames {
                            self.finish_chase(ctl, cfg, stats);
                        }
                    }
                    Block::Ready(frames)
                }
                Quantum::NeedData => Block::NeedData,
                Quantum::End => Block::End,
            };
        }
        if self.voice == VoiceKind::Bypass {
            // Bypass: read at the integer playhead, crossfade if a join is running, volume and
            // clamp follow in the caller (bit-exact with the decoder output at unity).
            let ahead = self.ahead();
            if ahead == 0 {
                if !self.feed.eof() {
                    return Block::NeedData;
                }
                if self.join_active() {
                    // Never an end of stream inside a join: the control plane would advance
                    // a second time (the join delivers the next item itself). The exchange
                    // happens before the appended head runs out, so this is unreachable; if
                    // it ever is, settle the join and render on the next turn.
                    debug_assert!(false, "end of stream inside a live join");
                    self.resolve_join(stats);
                    return Block::NeedData;
                }
                return Block::End;
            }
            let mut n = ahead.min(cfg.chunk_frames);
            // A seamless join ends the block on the boundary itself: the item flips on the
            // first frame of the next one.
            if let Some(to_boundary) = self.frames_to_join_boundary() {
                n = n.min(to_boundary.max(1));
            }
            debug_assert!(n * ch <= self.block.len());
            let got = self.feed.window.read_forward(self.read_pos, &mut self.block[..n * ch]);
            debug_assert_eq!(got, n);
            self.xf.process(&mut self.block[..n * ch]);
            self.read_pos += n as u64;
            self.stage.pos += n as f64;
            return Block::Ready(n);
        }
        let limit = self.chase_limit.take();
        let lim = limit.map_or(self.block.len(), |n| (n * ch).min(self.block.len()));
        match self.stage.render(&self.feed.window, self.feed.eof(), &mut self.block[..lim]) {
            Quantum::Rendered { frames, bounced, .. } => {
                // The running crossfade belongs to the block just rendered by the *current*
                // voice: apply it before a landing below starts a new one (a voice switch at
                // landing begins its fade with the next block; applying it to this one blended
                // the old voice's own continuation into itself and left the new voice's first
                // block unfaded: a click at every landing into a different voice kind).
                self.xf.process(&mut self.block[..frames * ch]);
                let chase_target = self.chase.as_ref().map(|c| c.target);
                if let (Some(_), Some(target)) = (limit, chase_target) {
                    // The shortened landing quantum ends on the target frame exactly.
                    self.stage.pos = target;
                    self.finish_chase(ctl, cfg, stats);
                } else if bounced && self.chase.is_some() {
                    // A chase never bounces: it is clamped to [0, end); treat as landed.
                    self.finish_chase(ctl, cfg, stats);
                }
                self.read_pos = self.stage.pos.max(0.0) as u64;
                if bounced && self.scan.is_some() {
                    // A reverse scan reached 0:00: it clamps and ends. Playing: tape bounce,
                    // forward again at the set speed's magnitude (a negative set speed bounces
                    // like the existing policy); from paused it re-pauses at 0:00.
                    let standstill = self.scan.as_ref().is_some_and(|s| s.standstill);
                    self.scan = None;
                    self.cross_scan_over(stats);
                    self.floor_frame = 0;
                    self.written_since_flush = 0;
                    self.spans.clear();
                    if standstill {
                        self.scan_pause = true;
                    } else {
                        let forward = ctl.target.abs();
                        if ctl.target < 0.0 {
                            ctl.target = forward;
                            stats.rate_bounces.fetch_add(1, Ordering::SeqCst);
                        }
                        self.stage.set_target(
                            forward,
                            RampSpec::Exp { tau_ms: cfg.rate.tau_tempo_ms },
                        );
                    }
                } else if bounced {
                    // Reverse hit frame 0: forward again at the set tempo's magnitude.
                    ctl.target = self.stage.target();
                    stats.rate_bounces.fetch_add(1, Ordering::SeqCst);
                    self.floor_frame = 0;
                    self.written_since_flush = 0;
                    self.spans.clear();
                }
                Block::Ready(frames)
            }
            Quantum::NeedData => Block::NeedData,
            Quantum::End => Block::End,
        }
    }

    /// True when everything decoded has been written (end of the stream reached).
    fn at_stream_end(&self) -> bool {
        self.feed.eof()
            && match self.voice {
                VoiceKind::Bypass => self.ahead() == 0,
                _ => self.stage.pos >= self.feed.window.end_frame() as f64,
            }
    }

    fn note_peaks(&self, stats: &AudioThreadStats) {
        let ahead_bytes = (self.ahead() * self.feed.window.channels() * 4) as u64;
        stats.peak_prefetch_bytes.fetch_max(ahead_bytes, Ordering::Relaxed);
        stats
            .peak_window_bytes
            .fetch_max(self.feed.window.peak_bytes() as u64, Ordering::Relaxed);
    }
}

fn audio_loop<F>(
    factory: Arc<F>,
    cmd_rx: Receiver<AudioCommand>,
    volume: AudioVolume,
    stats: AudioThreadStats,
    block_frames: usize,
    config: AudioThreadConfig,
) where
    F: AudioSinkFactory + Send + Sync + 'static,
{
    stats.running.store(true, Ordering::SeqCst);
    if let Ok(mut guard) = stats.audio_thread_id.lock() {
        *guard = Some(thread::current().id());
    }

    let chunk_frames = block_frames.max(DEFAULT_CHUNK_FRAMES / 4).max(64);
    let cfg = LoopCfg {
        window: config.window,
        xfade: config.xfade,
        rate: config.rate,
        stretch: config.stretch,
        chase: config.chase,
        scan: config.scan,
        decoder_hook: config.decoder_hook,
        isolation: config.isolation.unwrap_or_else(|| Isolation::global().clone()),
        chunk_frames,
        library_roots: config.library_roots,
    };
    let mut track: Option<TrackState> = None;
    let mut ctl = RateControl::new();
    let mut playing = false;
    let mut play_not_before: Option<Instant> = None;
    let idle = Duration::from_millis(2);
    let sfx_gain = 0.5f32;

    loop {
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    if handle_cmd(
                        cmd,
                        &factory,
                        &mut track,
                        &mut playing,
                        &mut play_not_before,
                        &volume,
                        &stats,
                        sfx_gain,
                        &cfg,
                        &mut ctl,
                    ) {
                        stats.running.store(false, Ordering::SeqCst);
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    stats.running.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }

        stats.scan_active.store(
            track.as_ref().is_some_and(|t| t.scan.is_some()),
            Ordering::Relaxed,
        );
        if playing {
            if let Some(t) = track.as_mut() {
                // A scan from paused finished (standstill, or it ran into a track end): pause.
                if t.scan_pause {
                    t.complete_scan_pause(&ctl, &cfg, &stats);
                    playing = false;
                    play_not_before = None;
                    stats.main_playing.store(false, Ordering::SeqCst);
                    continue;
                }
                // While waiting for SFX cover, keep prefetching so main is ready.
                if let Some(deadline) = play_not_before {
                    if Instant::now() < deadline {
                        let _ = t.pump(&stats);
                        // UI_19: the transport is Playing but the main playhead provably does
                        // not move yet, so keep the published rate at **zero**: a UI that
                        // dead-reckons the playhead from it would otherwise show a second of
                        // false playback and snap back when the audio starts. Held every turn
                        // so no command (tempo, scan) can republish a moving rate under the
                        // cover; the first main write republishes the real one.
                        stats.rate_micro.store(0, Ordering::Relaxed);
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    play_not_before = None;
                }

                // Take over what the decode worker has delivered (never blocks).
                if let Err(e) = t.pump(&stats) {
                    set_error(&stats, e);
                    playing = false;
                    play_not_before = None;
                    track = None;
                    stats.main_playing.store(false, Ordering::SeqCst);
                    stats.has_main_stream.store(false, Ordering::SeqCst);
                    stats.main_finished.store(true, Ordering::SeqCst);
                    continue;
                }

                // A shift left by a command that settled a join (Pause, seek, ...) belongs to
                // no block: only an exchange inside this render counts toward its span.
                t.rebase_shift = 0.0;
                let pos_before = t.pos();
                let n = match t.render_block(&mut ctl, &cfg, &stats) {
                    Block::Ready(n) => n,
                    Block::End if t.scan.as_ref().is_some_and(|s| s.standstill) => {
                        // A scan from paused ran into the end: it ends and pauses there (a
                        // later Play finishes the track normally).
                        t.scan = None;
                        t.cross_scan_over(&stats);
                        t.scan_pause = true;
                        continue;
                    }
                    Block::End => {
                        playing = false;
                        play_not_before = None;
                        // The track (and any scan on it) is gone: publish that BEFORE the
                        // finish flag, so nobody sees "finished" with a scan still active
                        // (`scan_active` is otherwise only refreshed at the next loop turn).
                        stats.scan_active.store(false, Ordering::Relaxed);
                        stats.main_playing.store(false, Ordering::SeqCst);
                        stats.main_finished.store(true, Ordering::SeqCst);
                        stats.has_main_stream.store(false, Ordering::SeqCst);
                        eprintln!(
                            "plippa-engine: stream EOF {} (first-audio {}ms, rate {} Hz, skipped={})",
                            t.path.display(),
                            stats.first_audio_ms.load(Ordering::Relaxed),
                            t.rate_hz,
                            t.feed.stats().skipped_packets.load(Ordering::Relaxed)
                        );
                        track = None;
                        continue;
                    }
                    Block::NeedData => {
                        // Nothing to render yet (the worker is still fetching): hold.
                        stats.starved_blocks.fetch_add(1, Ordering::Relaxed);
                        thread::sleep(Duration::from_micros(200));
                        continue;
                    }
                };

                // Volume, clamp, NaN/denormal flush, then the sink.
                let ch = t.feed.window.channels();
                apply_output_stage(&mut t.block[..n * ch], volume.get());
                match t.sink.write(&t.block[..n * ch]) {
                    Ok(()) => {
                        let frames = n as u64;
                        stats.frames_written.fetch_add(frames, Ordering::Relaxed);
                        t.written_since_flush += frames;
                        // The exact source span this block consumed (the stage integrates it,
                        // including a shortened landing quantum): the heard position stays
                        // exact across a rate change inside the queue (UI_15).
                        let moved = t.pos() - pos_before - t.take_rebase_shift();
                        t.spans.push(frames, moved);
                        t.publish_voice(&stats);
                        // At the end of the stream nothing more is queued behind the last
                        // frame in the sense of "where we are": report the end exactly.
                        let queue = if t.at_stream_end() { 0 } else { t.sink.queue_frames() };
                        t.publish_position(&stats, queue);
                        if !t.first_audio_logged {
                            t.first_audio_logged = true;
                            let ms = t.decode_started.elapsed().as_millis() as u64;
                            stats.first_audio_ms.store(ms, Ordering::Relaxed);
                            stats
                                .first_write_window_frames
                                .store(t.feed.window.len_frames() as u64, Ordering::Relaxed);
                            eprintln!(
                                "plippa-engine: first audio {}ms ({} Hz, {} ch) {}",
                                ms,
                                t.rate_hz,
                                t.channels,
                                t.path.display()
                            );
                        }
                        // From here on the worker may run ahead to the full lookahead.
                        t.feed.mark_audio_started(t.read_pos, t.dir);
                    }
                    Err(e) => {
                        set_error(&stats, e.to_string());
                        playing = false;
                        play_not_before = None;
                        track = None;
                        stats.main_playing.store(false, Ordering::SeqCst);
                        stats.has_main_stream.store(false, Ordering::SeqCst);
                        stats.main_finished.store(true, Ordering::SeqCst);
                    }
                }
            } else {
                playing = false;
                play_not_before = None;
                stats.main_playing.store(false, Ordering::SeqCst);
            }
            thread::sleep(Duration::from_micros(100));
        } else {
            // Paused / idle: keep taking over the worker's blocks (start chunk, landing of a
            // paused seek) so Play starts at once.
            if let Some(t) = track.as_mut() {
                if let Err(e) = t.pump(&stats) {
                    set_error(&stats, e);
                    track = None;
                    stats.has_main_stream.store(false, Ordering::SeqCst);
                    stats.main_finished.store(true, Ordering::SeqCst);
                }
            }
            match cmd_rx.recv_timeout(idle) {
                Ok(cmd) => {
                    if handle_cmd(
                        cmd,
                        &factory,
                        &mut track,
                        &mut playing,
                        &mut play_not_before,
                        &volume,
                        &stats,
                        sfx_gain,
                        &cfg,
                        &mut ctl,
                    ) {
                        stats.running.store(false, Ordering::SeqCst);
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    stats.running.store(false, Ordering::SeqCst);
                    return;
                }
            }
        }
    }
}

/// Rate fields while no stream is open (or before the first block): the set speed itself.
fn publish_idle_rate(ctl: &RateControl, stats: &AudioThreadStats) {
    let voice = ctl.wanted_voice();
    stats
        .rate_micro
        .store((ctl.target * 1_000_000.0).round() as i32, Ordering::Relaxed);
    stats
        .warp_active
        .store(voice != VoiceKind::Bypass, Ordering::Relaxed);
    stats.speed_voice.store(voice_code(voice), Ordering::Relaxed);
}

fn set_error(stats: &AudioThreadStats, msg: String) {
    eprintln!("plippa-engine: stream error: {msg}");
    if let Ok(mut g) = stats.last_error.lock() {
        *g = Some(msg);
    }
}

/// Write pre-decoded SFX PCM on a helper thread (does not block main open/decode).
fn spawn_sfx_playback<F>(factory: Arc<F>, mut pcm: PcmBuffer, sfx_gain: f32)
where
    F: AudioSinkFactory + Send + Sync + 'static,
{
    let _ = thread::Builder::new()
        .name("plippa-sfx".into())
        .spawn(move || {
            for s in pcm.samples.iter_mut() {
                *s = (*s * sfx_gain).clamp(-1.0, 1.0);
            }
            let frames = pcm.frame_count();
            let ch = pcm.channels;
            let rate = pcm.sample_rate_hz;
            match factory.open(ch, rate) {
                Ok(mut sink) => {
                    // Chunked writes keep Pulse buffers from blocking for the
                    // full cue duration on this helper thread.
                    let ch_usize = ch as usize;
                    let step = START_CHUNK_FRAMES.saturating_mul(ch_usize).max(ch_usize);
                    let mut off = 0usize;
                    while off < pcm.samples.len() {
                        let end = (off + step).min(pcm.samples.len());
                        let end = end - ((end - off) % ch_usize);
                        if end <= off {
                            break;
                        }
                        if let Err(e) = sink.write(&pcm.samples[off..end]) {
                            eprintln!("plippa-engine: SFX stream write failed: {e}");
                            return;
                        }
                        off = end;
                    }
                    eprintln!("plippa-engine: SFX stream {rate} Hz {ch} ch ({frames} frames)");
                }
                Err(e) => eprintln!("plippa-engine: SFX stream open failed: {e}"),
            }
        });
}

/// Returns true if the loop should exit.
fn handle_cmd<F>(
    cmd: AudioCommand,
    factory: &Arc<F>,
    track: &mut Option<TrackState>,
    playing: &mut bool,
    play_not_before: &mut Option<Instant>,
    volume: &AudioVolume,
    stats: &AudioThreadStats,
    sfx_gain: f32,
    cfg: &LoopCfg,
    ctl: &mut RateControl,
) -> bool
where
    F: AudioSinkFactory + Send + Sync + 'static,
{
    match cmd {
        AudioCommand::StartMainStream {
            path,
            file,
            capabilities,
            overspec,
            start_ms,
            play,
            item_position,
        } => {
            *playing = false;
            *play_not_before = None;
            *track = None;
            stats.main_playing.store(false, Ordering::SeqCst);
            stats.has_main_stream.store(false, Ordering::SeqCst);
            stats.main_finished.store(false, Ordering::SeqCst);
            stats.play_ignored.store(false, Ordering::SeqCst);
            stats.first_audio_ms.store(0, Ordering::Relaxed);
            stats.peak_prefetch_bytes.store(0, Ordering::Relaxed);
            stats.peak_window_bytes.store(0, Ordering::Relaxed);
            stats.first_write_window_frames.store(0, Ordering::Relaxed);
            stats.decoder_seeks.store(0, Ordering::Relaxed);
            stats.seek_crossfades.store(0, Ordering::Relaxed);
            stats.frames_written.store(0, Ordering::Relaxed);
            stats.chase_active.store(false, Ordering::SeqCst);
            stats.position_base_ms.store(start_ms, Ordering::Relaxed);
            stats
                .position_src_frames
                .store(0, Ordering::Relaxed);
            stats.sink_queue_frames.store(0, Ordering::Relaxed);
            stats.max_sustainable_rate_milli.store(0, Ordering::Relaxed);
            stats.starved_blocks.store(0, Ordering::Relaxed);
            if let Ok(mut g) = stats.worker.lock() {
                *g = None;
            }
            publish_idle_rate(ctl, stats);
            stats.duration_ms.store(0, Ordering::Relaxed);
            stats.stream_sample_rate_hz.store(0, Ordering::Relaxed);
            if let Ok(mut g) = stats.last_error.lock() {
                *g = None;
            }

            eprintln!(
                "plippa-engine: decode start {} (start_ms={})",
                path.display(),
                start_ms
            );
            let decode_started = Instant::now();
            let carry = ctl.carry.take();
            match open_isolated(&cfg.isolation, &path, file, &capabilities, overspec) {
                Ok(decoder) => {
                    // The start seek (resume position) is the worker's first job: the audio
                    // thread only probes the file (rate, channels, duration).
                    finish_start_open(
                        factory,
                        decoder,
                        decode_started,
                        start_ms,
                        path.clone(),
                        item_position,
                        track,
                        playing,
                        play_not_before,
                        stats,
                        play,
                        cfg,
                        ctl,
                        carry,
                    );
                }
                Err(e) => {
                    set_error(stats, e.to_string());
                }
            }
            stats.starts_completed.fetch_add(1, Ordering::SeqCst);
        }
        AudioCommand::ClearMain => {
            stats.chase_active.store(false, Ordering::SeqCst);
            *track = None;
            *playing = false;
            *play_not_before = None;
            stats.main_playing.store(false, Ordering::SeqCst);
            stats.has_main_stream.store(false, Ordering::SeqCst);
            stats.stream_sample_rate_hz.store(0, Ordering::Relaxed);
        }
        AudioCommand::Play { not_before } => {
            if track.is_some() {
                *playing = true;
                *play_not_before = not_before;
                stats.main_playing.store(true, Ordering::SeqCst);
                stats.play_ignored.store(false, Ordering::SeqCst);
            } else {
                eprintln!(
                    "plippa-engine: Play ignored — no main stream (control should re-arm)"
                );
                stats.play_ignored.store(true, Ordering::SeqCst);
                stats.main_playing.store(false, Ordering::SeqCst);
            }
        }
        AudioCommand::Pause => {
            let was_playing = *playing && play_not_before.is_none();
            *playing = false;
            *play_not_before = None;
            stats.main_playing.store(false, Ordering::SeqCst);
            if let Some(t) = track.as_mut() {
                // Pausing during a chase completes it silently: the playhead goes to the
                // target (no audio is queued behind it).
                let _ = t.cancel_scan(ctl, cfg, stats);
                if let Some(target) = t.cancel_chase(ctl, cfg, stats) {
                    let _ = t.hard_seek(target as u64, false, cfg, stats);
                }
                // No fade-out before the flush (WARP_03 decision): the flush is what makes
                // Pause immediate, and audio written after the queued audio would be heard
                // late. See the completion notes.
                t.pause_flush(stats, was_playing);
                // SEAM_01: a seamless join the playhead is inside of is settled **after** the
                // put-back (which may have stepped back across the boundary, inside the join
                // window): the listener resumes on the item and frame they stopped on.
                if t.join_active() {
                    t.resolve_join(stats);
                    t.floor_frame = t.logical_pos().max(0.0) as u64;
                    t.publish_position(stats, 0);
                }
            }
        }
        AudioCommand::Stop => {
            stats.chase_active.store(false, Ordering::SeqCst);
            *track = None;
            *playing = false;
            *play_not_before = None;
            stats.main_playing.store(false, Ordering::SeqCst);
            stats.has_main_stream.store(false, Ordering::SeqCst);
            stats.stream_sample_rate_hz.store(0, Ordering::Relaxed);
            stats.main_finished.store(true, Ordering::SeqCst);
        }
        AudioCommand::SetVolume(linear) => {
            volume.set(linear);
        }
        AudioCommand::SeekMs(position_ms) => {
            if let Some(t) = track.as_mut() {
                let audible = *playing && play_not_before.is_none();
                let _ = t.cancel_scan(ctl, cfg, stats);
                let _ = t.cancel_chase(ctl, cfg, stats);
                // SEAM_01: the seek rebases the playhead: settle a join first.
                t.settle_join_for_rebase(stats);
                let frame = ms_to_frame(position_ms, t.rate_hz);
                match t.hard_seek(frame, audible, cfg, stats) {
                    Ok(()) => stats.main_finished.store(false, Ordering::SeqCst),
                    Err(e) => set_error(stats, e.to_string()),
                }
            }
            stats.seeks_completed.fetch_add(1, Ordering::SeqCst);
        }
        AudioCommand::ScanStart { dir, mode } => {
            if let Some(t) = track.as_mut() {
                let from_paused = !*playing || play_not_before.is_some();
                let at_start = t.pos() < 1.0 && dir == ScanDir::Backward;
                if from_paused && t.scan.is_none() && at_start {
                    // RW at 0:00 while paused: nothing to rewind.
                } else {
                    // SEAM_01: a scan takes the playhead over: a seamless join is settled
                    // where the playhead is (the scan builds its own at the boundary).
                    if t.scan.is_none() {
                        t.settle_join_for_rebase(stats);
                    }
                    t.start_scan(dir, mode, from_paused && t.scan.is_none(), ctl, cfg, stats);
                    if from_paused {
                        *playing = true;
                        *play_not_before = None;
                        stats.main_playing.store(true, Ordering::SeqCst);
                        stats.play_ignored.store(false, Ordering::SeqCst);
                    }
                }
                stats
                    .scan_active
                    .store(t.scan.is_some(), Ordering::SeqCst);
            }
            // After `scan_active`: the control plane reads "handled && !active" as "over".
            stats.scans_handled.fetch_add(1, Ordering::SeqCst);
        }
        AudioCommand::ScanStop => {
            if let Some(t) = track.as_mut() {
                // UI_15: no brake. The rate is back at the set speed (or the paused-origin scan
                // is paused exactly where the listener was) before this command returns, so the
                // control plane's settled status reads a final position.
                if t.stop_scan_now(ctl, cfg, stats) {
                    *playing = false;
                    *play_not_before = None;
                    stats.main_playing.store(false, Ordering::SeqCst);
                }
                stats
                    .scan_active
                    .store(t.scan.is_some(), Ordering::SeqCst);
            }
        }
        AudioCommand::ScanSpeed { permille } => {
            if let Some(t) = track.as_mut() {
                t.set_scan_speed(permille);
            }
        }
        AudioCommand::SeekChase { target_ms, mode } => {
            // A released scan is gone before the jog is even read (UI_15: the release ends it
            // immediately), so a jog is never swallowed by a scan that is ending.
            if track.as_ref().is_some_and(|t| t.scan.is_some()) {
                // T11: the jog is ignored while a scan runs.
                stats.seeks_completed.fetch_add(1, Ordering::SeqCst);
                return false;
            }
            if let Some(t) = track.as_mut() {
                let audible = *playing && play_not_before.is_none();
                let frame = ms_to_frame(target_ms, t.rate_hz);
                // SEAM_01: a chase or a seek rebases the playhead: settle a join first.
                t.settle_join_for_rebase(stats);
                if audible {
                    t.start_chase(frame, mode, cfg, stats);
                } else {
                    // Paused / stopped: a silent seek.
                    let _ = t.cancel_chase(ctl, cfg, stats);
                    match t.hard_seek(frame, false, cfg, stats) {
                        Ok(()) => stats.main_finished.store(false, Ordering::SeqCst),
                        Err(e) => set_error(stats, e.to_string()),
                    }
                }
            }
            stats.seeks_completed.fetch_add(1, Ordering::SeqCst);
        }
        AudioCommand::TriggerSfx(pcm) => {
            spawn_sfx_playback(Arc::clone(factory), pcm, sfx_gain);
        }
        AudioCommand::SetRate { permille, change } => {
            let rate = f64::from(permille) / 1000.0;
            if rate == 0.0 || rate.abs() > 2.0 {
                eprintln!("plippa-engine: audio SetRate {permille} ignored (out of range)");
            } else {
                ctl.target = rate;
                ctl.change = change;
                ctl.generation += 1;
                if let Some(t) = track.as_mut() {
                    // SEAM_01: leaving normal speed re-bases nothing, but the voices that
                    // take over (varispeed / stretch) never exchange a join's feeds.
                    if rate != 1.0 && t.scan.is_none() {
                        t.settle_join_for_rebase(stats);
                    }
                    t.apply_rate_control(ctl, cfg, stats);
                } else {
                    publish_idle_rate(ctl, stats);
                }
            }
        }
        AudioCommand::SnapRate { permille } => {
            let rate = f64::from(permille) / 1000.0;
            if rate == 0.0 || rate.abs() > 2.0 {
                eprintln!("plippa-engine: audio SnapRate {permille} ignored (out of range)");
            } else {
                ctl.target = rate;
                ctl.change = RateChange::Immediate;
                ctl.generation += 1;
                if let Some(t) = track.as_mut() {
                    // Both owners of the rate are dropped where they stand: no brake, no
                    // landing swoop. The stage then jumps to the new target in this command.
                    let _ = t.cancel_scan(ctl, cfg, stats);
                    let _ = t.cancel_chase(ctl, cfg, stats);
                    if rate != 1.0 {
                        t.settle_join_for_rebase(stats);
                    }
                    t.apply_rate_control(ctl, cfg, stats);
                } else {
                    publish_idle_rate(ctl, stats);
                }
            }
        }
        AudioCommand::SetSpeedWarpMode(mode) => {
            ctl.speed_mode = mode;
            if let Some(t) = track.as_mut() {
                // Switch now (crossfaded) even while paused; the render loop re-checks too.
                t.reconcile_voice(ctl, cfg, stats);
            } else {
                publish_idle_rate(ctl, stats);
            }
        }
        AudioCommand::Hint(h) => {
            if let Some(t) = track.as_mut() {
                t.feed.hint(h);
            }
        }
        AudioCommand::SetNeighbours { prev, next } => {
            if let Some(t) = track.as_mut() {
                t.set_neighbours(prev, next);
            }
        }
        AudioCommand::SwitchItem {
            path,
            file,
            capabilities,
            overspec,
            start_ms,
            play,
            item_position,
        } => {
            // The audio on its way out is faded under the new stream (waypoint-length fade).
            ctl.carry = None;
            let audible = *playing && play_not_before.is_none();
            if let Some(t) = track.as_mut().filter(|t| audible && t.written_since_flush > 0) {
                let fade = XfadeConfig::frames(cfg.xfade.xfade_waypoint_ms, t.rate_hz)
                    .min(t.xf.capacity_frames());
                let got = t.old_continuation(fade);
                let ch = t.feed.window.channels();
                ctl.carry = Some(t.xf_old[..got * ch].to_vec());
            }
            return handle_cmd(
                AudioCommand::StartMainStream {
                    path,
                    file,
                    capabilities,
                    overspec,
                    start_ms,
                    play,
                    item_position,
                },
                factory,
                track,
                playing,
                play_not_before,
                volume,
                stats,
                sfx_gain,
                cfg,
                ctl,
            );
        }
        AudioCommand::Shutdown => return true,
    }
    false
}

/// Complete StartMainStream after a successful decoder open: hand the decoder to a decode
/// worker and wire the window feed. No audio is decoded here.
fn finish_start_open<F>(
    factory: &Arc<F>,
    mut decoder: RemoteDecoder,
    decode_started: Instant,
    start_ms: u64,
    path: PathBuf,
    item_position: u32,
    track: &mut Option<TrackState>,
    playing: &mut bool,
    play_not_before: &mut Option<Instant>,
    stats: &AudioThreadStats,
    play: Option<PlayArm>,
    cfg: &LoopCfg,
    ctl: &RateControl,
    carry: Option<Vec<f32>>,
) where
    F: AudioSinkFactory + Send + Sync + 'static,
{
    let rate = decoder.sample_rate_hz();
    let ch = decoder.channels();
    let dur = decoder.duration_ms().unwrap_or(0);
    let is_mp3 = decoder.extension() == "mp3";
    let len_frames = decoder.duration_frames();
    stats.stream_sample_rate_hz.store(rate, Ordering::Relaxed);
    stats.duration_ms.store(dur, Ordering::Relaxed);
    match factory.open(ch, rate) {
        Ok(sink) => {
            eprintln!(
                "plippa-engine: stream open {} Hz {} ch start_ms={} {}",
                rate,
                ch,
                start_ms,
                path.display()
            );
            let landing = ms_to_frame(start_ms, rate);
            let requeue_cap_frames = ms_to_frame(PAUSE_REQUEUE_MAX_MS, rate) as usize;
            let chans = ch as usize;
            let xf_frames = cfg.xfade.max_frames(rate);
            // MP3: build the frame index in the background (low priority); the worker picks
            // it up when ready and falls back to the accurate seek until then.
            // The helper builds it and installs it into its own decoder; the worker mirrors the
            // result into the stats.
            if is_mp3 {
                if let Err(e) = decoder.start_index_build(cfg.window.mp3_index_stride_frames) {
                    set_error(stats, e.to_string());
                    return;
                }
            }
            let feed = WindowFeed::spawn(
                boxed_source(decoder, cfg.decoder_hook.as_ref()),
                0,
                &cfg.window,
                &FeedParams {
                    chunk_frames: cfg.chunk_frames,
                    start_chunk_frames: START_CHUNK_FRAMES,
                    start_prefetch_frames: START_CHUNK_FRAMES,
                    start_frame: landing,
                },
                None,
            );
            if let Ok(mut g) = stats.worker.lock() {
                *g = Some(feed.stats_arc());
            }
            let mut state = TrackState {
                path,
                rate_hz: rate,
                channels: chans,
                item_position,
                max_rate_factor: cfg.window.decode_safety_factor,
                feed,
                dir: 1,
                sink,
                read_pos: landing,
                xf: Crossfader::new(chans, xf_frames),
                xf_old: vec![0.0; xf_frames * chans],
                block: vec![0.0; cfg.chunk_frames.max(START_CHUNK_FRAMES) * chans],
                decode_started,
                first_audio_logged: false,
                stage: RateStage::new(cfg.rate.clone(), chans, rate, landing as f64, ctl.target),
                stretch: None,
                voice: ctl.wanted_voice(),
                ctl_generation: ctl.generation,
                floor_frame: landing,
                written_since_flush: 0,
                spans: WriteSpans::new(),
                requeue_cap_frames,
                chase: None,
                chase_limit: None,
                scan: None,
                scan_pause: false,
                duration_ms: dur,
                len_frames,
                cross: None,
                rebase_shift: 0.0,
            };
            if let Some(old) = carry.filter(|c| !c.is_empty() && c.len() % chans == 0) {
                let frames = (old.len() / chans).min(state.xf.capacity_frames());
                state.xf.begin(frames, &old[..frames * chans]);
            }
            state.anchor(landing, stats);
            if state.voice == VoiceKind::Stretch {
                state.ensure_stretch(cfg);
                if let Some(sv) = state.stretch.as_mut() {
                    sv.request_prime(landing as f64);
                }
            }
            state.feed.set_start_prefetch(state.start_prefetch_frames());
            state.publish_voice(stats);
            stats.has_main_stream.store(true, Ordering::SeqCst);
            *track = Some(state);
            if let Some(arm) = play {
                *playing = true;
                *play_not_before = arm.not_before;
                stats.main_playing.store(true, Ordering::SeqCst);
                stats.play_ignored.store(false, Ordering::SeqCst);
                if arm.not_before.is_some_and(|deadline| deadline > Instant::now()) {
                    // UI_19: armed behind the tap's SFX cover — the publish here closes the
                    // window before the loop's cover wait starts holding the rate at zero (the
                    // transport is Playing but the main playhead does not move, so a UI that
                    // dead-reckons the playhead from the published rate would show a second of
                    // false playback and snap back when the audio starts). The first main write
                    // republishes the real rate.
                    stats.rate_micro.store(0, Ordering::Relaxed);
                }
            }
        }
        Err(e) => {
            set_error(stats, format!("stream open failed: {e}"));
        }
    }
}
