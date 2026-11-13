//! Decode worker (WARP_11): decoding and back-windows run on their own thread; the audio
//! thread never decodes, never seeks a decoder and never takes a blocking lock.
//!
//! # Hand-off
//!
//! The audio thread owns the [`SourceWindow`]. The worker sends **decoded blocks** over a
//! bounded `sync_channel` (preallocated slots, `try_recv` on the audio side); the audio thread
//! copies each block into its window ([`WindowFeed::pump`], one short memcpy per block) and
//! returns the buffer through a second bounded channel so the worker frees it (no deallocation
//! on the audio thread). In the other direction the audio thread publishes the window range,
//! playhead, direction and applied-block counter through a seqlock of atomics
//! ([`ViewShared`]); the worker plans from that view plus what it has already queued.
//!
//! # Epochs
//!
//! A hard seek outside the window starts a new **epoch** (`WindowFeed::reposition`): the audio
//! thread resets its window at once and the worker seeks its decoder (possibly slowly: an MP3
//! on a USB stick) and starts a new run of blocks tagged with the epoch. Blocks of an older
//! epoch are discarded. Until the first block arrives the audio loop has nothing to render
//! (it fades out and holds, never glitches).
//!
//! # Back-windows
//!
//! While the playhead moves backward (`dir < 0`) the worker keeps `back_lead` frames decoded
//! behind it: it seeks the decoder to `start - W - reference`, decodes forward, drops the
//! reference frames and sends the `W` frames before the window **newest piece first** so each
//! piece is contiguous with the window start ([`SourceWindow::prepend`]). The reader indexes
//! backward; there is no block-reversal copy.
//!
//! # Chase support
//!
//! A [`Hint`] (`target_frame`, `direction`) makes the worker decode toward the jog target and
//! guarantee **`landing_history`** frames before it: for a near target the main window is
//! extended (back-window or forward lookahead); for a far one a second window is prepared at a
//! waypoint (`target - landing_history ..`) and handed over whole ([`WorkerEvent::Waypoint`]).
//!
//! # Headroom
//!
//! The worker keeps a smoothed decode speed ([`ThroughputEstimator`], frames decoded per frame
//! of elapsed decode time); `max_sustainable_rate()` = speed x `decode_safety_factor` is the
//! runtime clamp input for later packages. Real values only exist on the device.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{
    channel, sync_channel, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError,
    TrySendError,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::decode::PcmBuffer;
use crate::error::AudioError;
use crate::index::{IndexSlot, Mp3Index};
use crate::stream::StreamDecoder;
use crate::window::{SourceWindow, WindowConfig};

/// Key of a track's decoder in the worker (one in WARP_11; WARP_19 adds the neighbour).
pub type TrackKey = u64;

/// Blocks the worker may have queued toward the audio thread at once.
pub const MAX_INFLIGHT_BLOCKS: usize = 4;
/// Blocks the audio thread applies per pump (bounds its memcpy work per loop turn).
pub const MAX_APPLY_PER_PUMP: usize = 8;
/// Idle wait of the worker between planning rounds when there is nothing to do.
const IDLE_WAIT: Duration = Duration::from_micros(500);

/// What a decoder must offer the worker ([`StreamDecoder`] implements it; tests use
/// synthetic and gated sources).
pub trait BlockSource: Send {
    fn channels(&self) -> usize;
    fn sample_rate_hz(&self) -> u32;
    /// Source frame of the next frame `next_chunk` returns.
    fn position_frame(&self) -> u64;
    /// Seek so the next frame is `frame` (the landing is `position_frame()` afterwards).
    fn seek_frame(&mut self, frame: u64) -> Result<(), AudioError>;
    /// Up to `max_frames` interleaved frames; `Ok(None)` at the end of the stream.
    fn next_chunk(&mut self, max_frames: usize) -> Result<Option<PcmBuffer>, AudioError>;
    /// Install an MP3 index (ignored by sources that do not use one).
    fn install_index(&mut self, _index: Arc<Mp3Index>) {}
    /// Whether no index build is outstanding. A source that builds its index elsewhere (the
    /// isolated helper) uses this call to refresh its view of it; the worker calls it
    /// regularly until it answers `true`.
    fn poll_index(&mut self) -> bool {
        true
    }
    /// Entries of the installed index (0 when none).
    fn index_entries(&self) -> u64 {
        0
    }
    fn has_index(&self) -> bool {
        false
    }
    fn indexed_seeks(&self) -> u64 {
        0
    }
    /// Malformed packets the source dropped and decoded past (0 for sources that cannot).
    fn skipped_packets(&self) -> u64 {
        0
    }
}

impl BlockSource for StreamDecoder {
    fn channels(&self) -> usize {
        StreamDecoder::channels(self) as usize
    }
    fn sample_rate_hz(&self) -> u32 {
        StreamDecoder::sample_rate_hz(self)
    }
    fn position_frame(&self) -> u64 {
        StreamDecoder::position_frame(self)
    }
    fn seek_frame(&mut self, frame: u64) -> Result<(), AudioError> {
        StreamDecoder::seek_frame(self, frame)
    }
    fn next_chunk(&mut self, max_frames: usize) -> Result<Option<PcmBuffer>, AudioError> {
        StreamDecoder::next_chunk(self, max_frames)
    }
    fn install_index(&mut self, index: Arc<Mp3Index>) {
        self.set_index(index);
    }
    fn has_index(&self) -> bool {
        StreamDecoder::has_index(self)
    }
    fn indexed_seeks(&self) -> u64 {
        StreamDecoder::indexed_seeks(self)
    }
    fn skipped_packets(&self) -> u64 {
        StreamDecoder::skipped_packets(self)
    }
}

/// Pre-decode request toward a jog / chase target (consumed by WARP_14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hint {
    pub target_frame: u64,
    /// -1 backward, +1 forward (direction of travel).
    pub direction: i8,
    /// Non-zero = the chase wants a **waypoint window** whatever the distance: the worker
    /// prepares `[T - lead - landing_history, T + lookahead)` (forward) or
    /// `[T - landing_history, T + lead + lookahead)` (backward), so the audio thread can jump
    /// to the waypoint `T - dir x lead` and adopt the window (WARP_14 far jump). 0 = the
    /// distance rule decides and the window starts at the landing history.
    pub waypoint_lead_frames: u64,
}

/// Whether a block goes after or before the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Append,
    Prepend,
}

/// One decoded block on its way to the audio thread.
#[derive(Debug)]
pub struct DecodedBlock {
    pub track: TrackKey,
    pub epoch: u64,
    pub seq: u64,
    pub kind: BlockKind,
    /// Absolute source frame of `samples[0]`.
    pub start_frame: u64,
    pub samples: Vec<f32>,
    /// Set on the append block that reached the end of the stream: total frames. An empty
    /// block with this set is a pure end marker.
    pub eof_at: Option<u64>,
}

/// A second window prepared at a far target (the audio thread keeps it for the chase).
#[derive(Debug)]
pub struct WaypointWindow {
    pub track: TrackKey,
    pub target_frame: u64,
    pub window: SourceWindow,
}

#[derive(Debug)]
pub enum WorkerEvent {
    Block(DecodedBlock),
    Waypoint(Box<WaypointWindow>),
    Error(String),
}

enum WorkerCmd {
    Reposition { epoch: u64, frame: u64 },
    Hint(Option<Hint>),
    Index(Arc<IndexSlot>),
    Stop,
}

// ---------------------------------------------------------------------------------------
// Throughput estimator (pure)
// ---------------------------------------------------------------------------------------

/// Smoothed decode speed: frames decoded per frame of elapsed decode time (1.0 = real time).
#[derive(Debug, Clone)]
pub struct ThroughputEstimator {
    alpha: f64,
    speed: Option<f64>,
}

impl ThroughputEstimator {
    /// `alpha` in (0, 1]: weight of a new sample in the exponential average.
    pub fn new(alpha: f64) -> Self {
        Self {
            alpha: alpha.clamp(0.01, 1.0),
            speed: None,
        }
    }

    /// Feed one measurement: `frames` decoded in `elapsed` of worker time at `rate_hz`.
    /// Zero-length samples are ignored.
    pub fn sample(&mut self, frames: u64, elapsed: Duration, rate_hz: u32) {
        let secs = elapsed.as_secs_f64();
        if frames == 0 || secs <= 0.0 || rate_hz == 0 {
            return;
        }
        let s = frames as f64 / (secs * f64::from(rate_hz));
        self.speed = Some(match self.speed {
            None => s,
            Some(old) => old + self.alpha * (s - old),
        });
    }

    /// Smoothed decode speed, `None` before the first sample.
    pub fn speed(&self) -> Option<f64> {
        self.speed
    }

    /// `speed x safety_factor`: the highest source rate playback can sustain.
    pub fn max_sustainable_rate(&self, safety_factor: f64) -> Option<f64> {
        self.speed.map(|s| s * safety_factor)
    }
}

// ---------------------------------------------------------------------------------------
// Planning (pure)
// ---------------------------------------------------------------------------------------

/// The sizes the planner works with, derived from [`WindowConfig`] for one stream.
#[derive(Debug, Clone)]
pub struct PlanCfg {
    pub chunk_frames: usize,
    pub start_chunk_frames: usize,
    pub start_prefetch_frames: usize,
    pub lookahead_frames: usize,
    pub history_frames: usize,
    pub back_window_frames: usize,
    pub back_lead_frames: usize,
    pub landing_history_frames: usize,
    pub waypoint_lookahead_frames: usize,
    pub cap_frames: usize,
}

impl PlanCfg {
    pub fn new(
        cfg: &WindowConfig,
        channels: usize,
        rate_hz: u32,
        chunk_frames: usize,
        start_prefetch_frames: usize,
    ) -> Self {
        Self {
            chunk_frames: chunk_frames.max(1),
            start_chunk_frames: chunk_frames.max(1),
            start_prefetch_frames,
            lookahead_frames: cfg.lookahead_frames(channels, rate_hz),
            history_frames: cfg.history_frames(channels, rate_hz),
            back_window_frames: cfg.back_window_frames(rate_hz),
            back_lead_frames: cfg.back_lead_frames(channels, rate_hz),
            landing_history_frames: cfg.landing_history_frames(rate_hz),
            waypoint_lookahead_frames: (cfg.waypoint_lookahead_s * f64::from(rate_hz)) as usize,
            cap_frames: cfg.cap_frames(channels),
        }
    }
}

/// What the planner sees: the window as it will be once everything queued is applied.
#[derive(Debug, Clone)]
pub struct PlanView {
    pub start: u64,
    pub end: u64,
    pub playhead: u64,
    /// -1 reversing, 0 idle, +1 forward.
    pub dir: i8,
    pub audio_started: bool,
    pub eof_at: Option<u64>,
    pub hint: Option<Hint>,
    /// A waypoint window for the hint's target was already delivered.
    pub waypoint_done: bool,
}

/// One decode step the worker should take next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch {
    Idle,
    /// Decode one chunk after the window end.
    Forward,
    /// Decode `[from, start)` and place it before the window.
    Back {
        from: u64,
    },
    /// Prepare a second window covering `[lo, ..)` for the far hint target.
    Waypoint {
        lo: u64,
        target: u64,
    },
}

impl PlanView {
    fn at_eof(&self) -> bool {
        self.eof_at.is_some_and(|e| self.end >= e)
    }

    /// True when the landing zone of the hint is held: `landing_history` frames before the
    /// target and a chunk after it.
    pub fn landing_ready(&self, c: &PlanCfg) -> bool {
        let Some(h) = self.hint else { return false };
        let lo = h
            .target_frame
            .saturating_sub(c.landing_history_frames as u64);
        let mut hi = h.target_frame + c.chunk_frames as u64;
        if let Some(e) = self.eof_at {
            hi = hi.min(e);
        }
        self.start <= lo && self.end >= hi
    }
}

/// Decide the next fetch (pure function of the view, so it is table-tested).
pub fn plan(v: &PlanView, c: &PlanCfg) -> Fetch {
    let ahead = v.end.saturating_sub(v.playhead.max(v.start)) as usize;
    let forward_room = ahead + c.chunk_frames <= c.lookahead_frames;
    if v.end <= v.start && !v.at_eof() {
        // Nothing held yet (fresh epoch): the landing chunk comes first, whatever the
        // direction; back windows can only be placed before an existing window.
        return Fetch::Forward;
    }
    if v.audio_started {
        // Playback itself comes first: a hint never starves the direction being played.
        let behind = v.playhead.saturating_sub(v.start) as usize;
        if v.dir >= 0 && ahead < c.lookahead_frames / 4 && !v.at_eof() {
            return Fetch::Forward;
        }
        if v.dir < 0 && v.start > 0 && behind < c.back_lead_frames / 2 {
            return Fetch::Back {
                from: v.start.saturating_sub(c.back_window_frames as u64),
            };
        }
    }
    if let Some(h) = v.hint {
        let lo = h
            .target_frame
            .saturating_sub(c.landing_history_frames as u64);
        let hi = h.target_frame + c.chunk_frames as u64;
        // Frames the window must span to hold the playhead (with its history) and the zone.
        let span_lo = lo.min(v.playhead);
        let span_hi = hi.max(v.playhead + 1);
        let near = (span_hi - span_lo) as usize <= c.cap_frames * 9 / 10;
        if near && h.waypoint_lead_frames == 0 {
            if lo < v.start {
                return Fetch::Back {
                    from: lo.max(v.start.saturating_sub(c.back_window_frames as u64)),
                };
            }
            if v.end < hi && !v.at_eof() {
                return Fetch::Forward;
            }
        }
        if (!near || h.waypoint_lead_frames > 0) && !v.waypoint_done {
            let lo = if h.direction >= 0 {
                lo.saturating_sub(h.waypoint_lead_frames)
            } else {
                lo
            };
            return Fetch::Waypoint {
                lo,
                target: h.target_frame,
            };
        }
    }
    if !v.audio_started {
        // First audio never waits for the lookahead to fill: one start chunk only.
        return if ahead < c.start_prefetch_frames && !v.at_eof() {
            Fetch::Forward
        } else {
            Fetch::Idle
        };
    }
    if v.dir < 0 {
        let behind = v.playhead.saturating_sub(v.start) as usize;
        if v.start > 0 && behind < c.back_lead_frames {
            return Fetch::Back {
                from: v.start.saturating_sub(c.back_window_frames as u64),
            };
        }
        return Fetch::Idle;
    }
    // A pinned landing zone must not be pushed out by the lookahead: stop at the byte cap.
    let pinned_full = v.hint.is_some_and(|h| {
        h.waypoint_lead_frames == 0
            && v.end + c.chunk_frames as u64
                > h.target_frame
                    .saturating_sub(c.landing_history_frames as u64)
                    + c.cap_frames as u64
    });
    if forward_room && !v.at_eof() && !pinned_full {
        Fetch::Forward
    } else {
        Fetch::Idle
    }
}

// ---------------------------------------------------------------------------------------
// Shared view (audio thread -> worker), seqlock of atomics
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct View {
    pub epoch: u64,
    pub start: u64,
    pub end: u64,
    pub playhead: u64,
    pub dir: i8,
    pub audio_started: bool,
    pub applied_seq: u64,
    pub resync: u64,
}

#[derive(Debug, Default)]
struct ViewShared {
    seq: AtomicU64,
    epoch: AtomicU64,
    start: AtomicU64,
    end: AtomicU64,
    playhead: AtomicU64,
    flags: AtomicU64,
    applied_seq: AtomicU64,
    resync: AtomicU64,
}

impl ViewShared {
    fn write(&self, v: &View) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        self.epoch.store(v.epoch, Ordering::SeqCst);
        self.start.store(v.start, Ordering::SeqCst);
        self.end.store(v.end, Ordering::SeqCst);
        self.playhead.store(v.playhead, Ordering::SeqCst);
        let flags = (u64::from((v.dir + 1) as u8)) | (u64::from(v.audio_started) << 2);
        self.flags.store(flags, Ordering::SeqCst);
        self.applied_seq.store(v.applied_seq, Ordering::SeqCst);
        self.resync.store(v.resync, Ordering::SeqCst);
        self.seq.fetch_add(1, Ordering::SeqCst);
    }

    fn read(&self) -> View {
        loop {
            let s1 = self.seq.load(Ordering::SeqCst);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let flags = self.flags.load(Ordering::SeqCst);
            let v = View {
                epoch: self.epoch.load(Ordering::SeqCst),
                start: self.start.load(Ordering::SeqCst),
                end: self.end.load(Ordering::SeqCst),
                playhead: self.playhead.load(Ordering::SeqCst),
                dir: (flags & 3) as i8 - 1,
                audio_started: flags & 4 != 0,
                applied_seq: self.applied_seq.load(Ordering::SeqCst),
                resync: self.resync.load(Ordering::SeqCst),
            };
            if self.seq.load(Ordering::SeqCst) == s1 {
                return v;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------------------

/// Counters shared by both sides (observable from tests and the engine stats).
#[derive(Debug)]
pub struct WorkerStats {
    /// Blocks the worker queued (any kind).
    pub blocks_sent: AtomicU64,
    /// Back-window decodes (each yields several pieces).
    pub back_windows: AtomicU64,
    /// Decoder seeks issued by the worker.
    pub decoder_seeks: AtomicU64,
    /// Seeks served through the MP3 index.
    pub indexed_seeks: AtomicU64,
    pub index_ready: AtomicBool,
    pub index_entries: AtomicU64,
    /// Malformed packets the decoder skipped (diagnostics: the engine logs it at stream end).
    pub skipped_packets: AtomicU64,
    /// Smoothed decode speed x 1000 (0 = unknown).
    pub speed_milli: AtomicU64,
    /// Frames decoded in total (forward chunks, back windows and reference frames).
    pub frames_decoded: AtomicU64,
    /// Target of the hint whose landing zone is currently held (`-1` none).
    pub hint_ready_target: AtomicI64,
    pub waypoints_sent: AtomicU64,
    /// Audio side: blocks applied / discarded as stale epoch / refused (resync).
    pub blocks_applied: AtomicU64,
    pub blocks_stale: AtomicU64,
    pub blocks_refused: AtomicU64,
    /// Audio side: times the audio thread had to wait for the worker. Always 0: the audio
    /// side only ever `try_recv`s (kept so a test can prove it).
    pub audio_blocking_waits: AtomicU64,
}

impl WorkerStats {
    fn new() -> Self {
        Self {
            blocks_sent: AtomicU64::new(0),
            back_windows: AtomicU64::new(0),
            decoder_seeks: AtomicU64::new(0),
            indexed_seeks: AtomicU64::new(0),
            index_ready: AtomicBool::new(false),
            index_entries: AtomicU64::new(0),
            skipped_packets: AtomicU64::new(0),
            speed_milli: AtomicU64::new(0),
            frames_decoded: AtomicU64::new(0),
            hint_ready_target: AtomicI64::new(-1),
            waypoints_sent: AtomicU64::new(0),
            blocks_applied: AtomicU64::new(0),
            blocks_stale: AtomicU64::new(0),
            blocks_refused: AtomicU64::new(0),
            audio_blocking_waits: AtomicU64::new(0),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Worker thread
// ---------------------------------------------------------------------------------------

struct Shared {
    view: ViewShared,
    stats: Arc<WorkerStats>,
    stop: AtomicBool,
    /// Frames the worker decodes before the audio thread's first write of an epoch (the
    /// stretch voice needs more lookahead than plain playback).
    start_prefetch: AtomicU64,
}

/// Spawn parameters of a [`WindowFeed`].
#[derive(Debug, Clone)]
pub struct FeedParams {
    /// Frames per decoded chunk once audio runs.
    pub chunk_frames: usize,
    /// Frames of the single chunk decoded before the first write (first-audio latency).
    pub start_chunk_frames: usize,
    /// Frames to hold ahead before the first write.
    pub start_prefetch_frames: usize,
    /// Frame the stream starts on.
    pub start_frame: u64,
}

/// Worker-side bookkeeping of what it has queued toward the audio thread.
struct Queued {
    seq: u64,
    kind: BlockKind,
    start: u64,
    end: u64,
}

struct Worker {
    source: Box<dyn BlockSource>,
    track: TrackKey,
    cfg: WindowConfig,
    plan_cfg: PlanCfg,
    shared: Arc<Shared>,
    started: bool,
    events: SyncSender<WorkerEvent>,
    recycle: Receiver<Vec<f32>>,
    channels: usize,
    rate_hz: u32,
    epoch: u64,
    hint: Option<Hint>,
    waypoint_done: bool,
    inflight: VecDeque<Queued>,
    /// Back-window pieces decoded but not yet accepted by the channel.
    pending: VecDeque<DecodedBlock>,
    next_seq: u64,
    eof_at: Option<u64>,
    last_resync: u64,
    est: ThroughputEstimator,
    scratch: Vec<f32>,
    index_slot: Option<Arc<IndexSlot>>,
    /// The source's own index build is settled (nothing left to poll).
    index_settled: bool,
    index_polled: Option<Instant>,
    /// The decoder failed: wait for the next reposition.
    failed: bool,
}

impl Worker {
    fn take_buf(&mut self, frames: usize) -> Vec<f32> {
        let want = frames * self.channels;
        let mut v = self.recycle.try_recv().unwrap_or_default();
        v.clear();
        v.reserve(want);
        v
    }

    /// Mirror the source's skipped-packet count into the shared stats.
    fn publish_skips(&self) {
        self.shared
            .stats
            .skipped_packets
            .store(self.source.skipped_packets(), Ordering::Relaxed);
    }

    fn fail(&mut self, e: AudioError) {
        self.publish_skips();
        self.failed = true;
        let _ = self.events.try_send(WorkerEvent::Error(e.to_string()));
    }

    fn note_decode(&mut self, frames: u64, t0: Instant) {
        self.publish_skips();
        self.shared
            .stats
            .frames_decoded
            .fetch_add(frames, Ordering::Relaxed);
        self.est.sample(frames, t0.elapsed(), self.rate_hz);
        if let Some(s) = self.est.speed() {
            self.shared
                .stats
                .speed_milli
                .store((s * 1000.0) as u64, Ordering::Relaxed);
        }
    }

    fn seek(&mut self, frame: u64) -> Result<(), AudioError> {
        if self.source.position_frame() == frame {
            return Ok(());
        }
        let st = &self.shared.stats;
        st.decoder_seeks.fetch_add(1, Ordering::Relaxed);
        let r = self.source.seek_frame(frame);
        st.indexed_seeks
            .store(self.source.indexed_seeks(), Ordering::Relaxed);
        r
    }

    fn check_index(&mut self) {
        // An index the source builds itself (the isolated helper): refresh its view at most
        // every 20 ms until the build is settled, then publish the result once.
        if self.index_slot.is_none() && !self.index_settled {
            let due = self.index_polled.is_none_or(|t| t.elapsed() >= Duration::from_millis(20));
            if due {
                self.index_polled = Some(Instant::now());
                if self.source.poll_index() {
                    self.index_settled = true;
                }
                if self.source.has_index() && !self.shared.stats.index_ready.load(Ordering::Relaxed) {
                    self.shared.stats.index_ready.store(true, Ordering::Relaxed);
                    self.shared
                        .stats
                        .index_entries
                        .store(self.source.index_entries(), Ordering::Relaxed);
                }
            }
        }
        if let Some(slot) = &self.index_slot {
            if slot.is_done() {
                if let Some(idx) = slot.get() {
                    eprintln!("plippa-audio: mp3 index ready ({} entries)", idx.len());
                    self.source.install_index(idx);
                    self.shared.stats.index_ready.store(true, Ordering::Relaxed);
                    self.shared
                        .stats
                        .index_entries
                        .store(slot.entries(), Ordering::Relaxed);
                }
                self.index_slot = None;
            }
        }
    }

    fn handle(&mut self, cmd: WorkerCmd) -> bool {
        match cmd {
            WorkerCmd::Stop => return true,
            WorkerCmd::Hint(h) => {
                self.hint = h;
                self.waypoint_done = false;
                self.shared
                    .stats
                    .hint_ready_target
                    .store(-1, Ordering::Relaxed);
            }
            WorkerCmd::Index(slot) => {
                self.index_slot = Some(slot);
                self.check_index();
            }
            WorkerCmd::Reposition { epoch, frame } => {
                self.epoch = epoch;
                self.inflight.clear();
                self.pending.clear();
                self.eof_at = None;
                self.failed = false;
                self.hint = None;
                self.shared
                    .stats
                    .hint_ready_target
                    .store(-1, Ordering::Relaxed);
                self.check_index();
                if let Err(e) = self.seek(frame) {
                    self.fail(e);
                }
            }
        }
        false
    }

    fn flush_pending(&mut self) {
        while let Some(b) = self.pending.pop_front() {
            match self.events.try_send(WorkerEvent::Block(b)) {
                Ok(()) => {
                    self.shared
                        .stats
                        .blocks_sent
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(WorkerEvent::Block(b))) => {
                    self.pending.push_front(b);
                    break;
                }
                Err(_) => break,
            }
        }
    }

    /// Window range once everything queued has been applied.
    fn effective(&self, view: &View) -> (u64, u64) {
        let mut start = view.start;
        let mut end = view.end;
        for q in &self.inflight {
            match q.kind {
                BlockKind::Append => end = end.max(q.end),
                BlockKind::Prepend => start = start.min(q.start),
            }
        }
        (start, end)
    }

    fn step(&mut self, view: View) -> bool {
        // Retire what the audio thread has applied, and resync after a refused block.
        while self
            .inflight
            .front()
            .is_some_and(|q| q.seq <= view.applied_seq)
        {
            self.inflight.pop_front();
        }
        if view.resync != self.last_resync {
            self.last_resync = view.resync;
            self.inflight.clear();
            self.pending.clear();
            self.eof_at = None;
        }
        if self.failed || !self.pending.is_empty() || self.inflight.len() >= MAX_INFLIGHT_BLOCKS {
            return false;
        }
        let (start, end) = self.effective(&view);
        self.started = view.audio_started;
        self.plan_cfg.start_prefetch_frames =
            self.shared.start_prefetch.load(Ordering::Relaxed) as usize;
        let pv = PlanView {
            start,
            end,
            playhead: view.playhead,
            dir: view.dir,
            audio_started: view.audio_started,
            eof_at: self.eof_at,
            hint: self.hint,
            waypoint_done: self.waypoint_done,
        };
        // Report whether the hinted landing zone is held.
        let ready_target = if pv.landing_ready(&self.plan_cfg) {
            self.hint.map_or(-1, |h| h.target_frame as i64)
        } else if self.waypoint_done {
            self.hint.map_or(-1, |h| h.target_frame as i64)
        } else {
            -1
        };
        self.shared
            .stats
            .hint_ready_target
            .store(ready_target, Ordering::Relaxed);
        match plan(&pv, &self.plan_cfg) {
            Fetch::Idle => false,
            Fetch::Forward => self.fetch_forward(end),
            Fetch::Back { from } => self.fetch_back(from, start),
            Fetch::Waypoint { lo, target } => self.fetch_waypoint(lo, target),
        }
    }

    fn queue(&mut self, kind: BlockKind, start: u64, samples: Vec<f32>, eof_at: Option<u64>) {
        let frames = (samples.len() / self.channels) as u64;
        let seq = self.next_seq;
        self.next_seq += 1;
        let blk = DecodedBlock {
            track: self.track,
            epoch: self.epoch,
            seq,
            kind,
            start_frame: start,
            samples,
            eof_at,
        };
        self.inflight.push_back(Queued {
            seq,
            kind,
            start,
            end: start + frames,
        });
        self.pending.push_back(blk);
        self.flush_pending();
    }

    fn fetch_forward(&mut self, from: u64) -> bool {
        if let Err(e) = self.seek(from) {
            self.fail(e);
            return false;
        }
        let t0 = Instant::now();
        let chunk = if self.started {
            self.plan_cfg.chunk_frames
        } else {
            self.plan_cfg
                .start_chunk_frames
                .min(self.plan_cfg.chunk_frames)
        };
        match self.source.next_chunk(chunk) {
            Ok(Some(pcm)) => {
                let landing =
                    self.source.position_frame() - (pcm.samples.len() / self.channels) as u64;
                let frames = (pcm.samples.len() / self.channels) as u64;
                self.note_decode(frames, t0);
                let mut buf = self.take_buf(frames as usize);
                buf.extend_from_slice(&pcm.samples);
                self.queue(BlockKind::Append, landing, buf, None);
                true
            }
            Ok(None) => {
                self.publish_skips();
                let e = self.source.position_frame();
                self.eof_at = Some(e);
                let buf = self.take_buf(0);
                self.queue(BlockKind::Append, e, buf, Some(e));
                true
            }
            Err(e) => {
                self.fail(e);
                false
            }
        }
    }

    /// Decode `[from, window_start)` and queue it before the window, newest piece first.
    fn fetch_back(&mut self, from: u64, window_start: u64) -> bool {
        if from >= window_start {
            return false;
        }
        let reference = if from == 0 {
            0
        } else {
            self.cfg.back_reference_frames as u64
        };
        let seek_to = from.saturating_sub(reference);
        let drop_frames = from - seek_to;
        let total = (window_start - from) as usize;
        let ch = self.channels;
        let t0 = Instant::now();
        let mut decoded = 0u64;
        // Force a real seek even when the decoder happens to sit at `seek_to`.
        if let Err(e) = self.seek(seek_to) {
            self.fail(e);
            return false;
        }
        let mut result: Result<(), AudioError> = Ok(());
        // Drop the reference frames.
        let mut to_drop = drop_frames as usize;
        while to_drop > 0 {
            match self
                .source
                .next_chunk(to_drop.min(self.plan_cfg.chunk_frames))
            {
                Ok(Some(p)) => {
                    let n = p.samples.len() / ch;
                    decoded += n as u64;
                    to_drop = to_drop.saturating_sub(n);
                }
                Ok(None) => break,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        // Decode the block itself into the scratch buffer.
        self.scratch.clear();
        self.scratch.reserve(total * ch);
        while result.is_ok() && self.scratch.len() < total * ch {
            let want = total - self.scratch.len() / ch;
            match self.source.next_chunk(want.min(self.plan_cfg.chunk_frames)) {
                Ok(Some(p)) => {
                    decoded += (p.samples.len() / ch) as u64;
                    self.scratch.extend_from_slice(&p.samples);
                }
                Ok(None) => break,
                Err(e) => result = Err(e),
            }
        }
        self.note_decode(decoded, t0);
        if let Err(e) = result {
            self.fail(e);
            return false;
        }
        let got = self.scratch.len() / ch;
        if got < total {
            // The file is shorter than the window claims: nothing sane to place.
            self.fail(AudioError::Decode {
                path: Default::default(),
                reason: format!("back window short: {got} of {total} frames"),
            });
            return false;
        }
        self.shared
            .stats
            .back_windows
            .fetch_add(1, Ordering::Relaxed);
        // Pieces of chunk size, newest first.
        let piece = self.plan_cfg.chunk_frames;
        let mut end = total;
        while end > 0 {
            let s = end.saturating_sub(piece);
            let mut buf = self.take_buf(end - s);
            buf.extend_from_slice(&self.scratch[s * ch..end * ch]);
            self.queue(BlockKind::Prepend, from + s as u64, buf, None);
            end = s;
        }
        true
    }

    /// Prepare a second window at a far target and hand it over whole.
    fn fetch_waypoint(&mut self, lo: u64, target: u64) -> bool {
        let wcfg = self.cfg.split(2);
        let mut win = SourceWindow::new(self.channels, self.rate_hz, &wcfg);
        let (lead, backward) = self
            .hint
            .map_or((0, false), |h| (h.waypoint_lead_frames, h.direction < 0));
        let fill_to = (target
            + if backward { lead } else { 0 }
            + self.plan_cfg.waypoint_lookahead_frames as u64)
            .min(lo + win.cap_frames() as u64 - self.plan_cfg.chunk_frames as u64);
        if let Err(e) = self.seek(lo) {
            self.fail(e);
            return false;
        }
        win.reset(lo);
        let t0 = Instant::now();
        let mut decoded = 0u64;
        while win.end_frame() < fill_to {
            match self.source.next_chunk(self.plan_cfg.chunk_frames) {
                Ok(Some(p)) => {
                    decoded += (p.samples.len() / self.channels) as u64;
                    if !win.push(&p.samples, lo) {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    self.fail(e);
                    return false;
                }
            }
        }
        self.note_decode(decoded, t0);
        self.waypoint_done = true;
        let ev = WorkerEvent::Waypoint(Box::new(WaypointWindow {
            track: self.track,
            target_frame: target,
            window: win,
        }));
        // The channel is small; retry until accepted (stop and new commands abort).
        let mut ev = Some(ev);
        while let Some(e) = ev.take() {
            match self.events.try_send(e) {
                Ok(()) => {
                    self.shared
                        .stats
                        .waypoints_sent
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(e)) => {
                    if self.shared.stop.load(Ordering::Relaxed) {
                        return false;
                    }
                    ev = Some(e);
                    std::thread::sleep(IDLE_WAIT);
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        true
    }
}

fn worker_main(mut w: Worker, cmd_rx: Receiver<WorkerCmd>) {
    let mut busy = false;
    loop {
        if w.shared.stop.load(Ordering::Relaxed) {
            return;
        }
        // Commands first; sleep on the command channel only when idle.
        let first = if busy {
            cmd_rx.try_recv().ok()
        } else {
            match cmd_rx.recv_timeout(IDLE_WAIT) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        let mut next = first;
        while let Some(cmd) = next {
            if w.handle(cmd) {
                return;
            }
            next = match cmd_rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => return,
            };
        }
        w.check_index();
        w.flush_pending();
        let view = w.shared.view.read();
        busy = if view.epoch == w.epoch {
            w.step(view)
        } else {
            false
        };
    }
}

// ---------------------------------------------------------------------------------------
// Audio-thread side
// ---------------------------------------------------------------------------------------

/// Result of one [`WindowFeed::pump`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pump {
    pub applied: usize,
    /// The first block of the epoch started elsewhere than requested (the decoder landed
    /// after the target): the window was rebased there and the caller must re-anchor.
    pub rebased: Option<u64>,
}

/// The audio thread's end of the worker: the window it renders from plus the hand-off.
pub struct WindowFeed {
    pub window: SourceWindow,
    track: TrackKey,
    cmd_tx: Sender<WorkerCmd>,
    events: Receiver<WorkerEvent>,
    recycle: SyncSender<Vec<f32>>,
    shared: Arc<Shared>,
    epoch: u64,
    eof_at: Option<u64>,
    applied_seq: u64,
    resync: u64,
    audio_started: bool,
    fresh: bool,
    waypoint: Option<Box<WaypointWindow>>,
    error: Option<String>,
    cfg: WindowConfig,
    chunk_frames: usize,
}

impl WindowFeed {
    /// Spawn the worker for `source` and start it at `start_frame`. `index` (MP3) is picked up
    /// by the worker when its walk finishes.
    pub fn spawn(
        source: Box<dyn BlockSource>,
        track: TrackKey,
        cfg: &WindowConfig,
        params: &FeedParams,
        index: Option<Arc<IndexSlot>>,
    ) -> Self {
        let channels = source.channels().max(1);
        let rate_hz = source.sample_rate_hz();
        let (ev_tx, ev_rx) = sync_channel(MAX_INFLIGHT_BLOCKS + 2);
        let (rec_tx, rec_rx) = sync_channel(MAX_INFLIGHT_BLOCKS * 3);
        let (cmd_tx, cmd_rx) = channel();
        let shared = Arc::new(Shared {
            view: ViewShared::default(),
            stats: Arc::new(WorkerStats::new()),
            stop: AtomicBool::new(false),
            start_prefetch: AtomicU64::new(params.start_prefetch_frames as u64),
        });
        let worker = Worker {
            source,
            track,
            cfg: cfg.clone(),
            plan_cfg: {
                let mut p = PlanCfg::new(
                    cfg,
                    channels,
                    rate_hz,
                    params.chunk_frames,
                    params.start_prefetch_frames,
                );
                p.start_chunk_frames = params.start_chunk_frames.max(1);
                p
            },
            shared: Arc::clone(&shared),
            started: false,
            events: ev_tx,
            recycle: rec_rx,
            channels,
            rate_hz,
            epoch: 0,
            hint: None,
            waypoint_done: false,
            inflight: VecDeque::new(),
            pending: VecDeque::new(),
            next_seq: 1,
            eof_at: None,
            last_resync: 0,
            est: ThroughputEstimator::new(0.2),
            scratch: Vec::new(),
            index_slot: None,
            index_settled: false,
            index_polled: None,
            failed: false,
        };
        let _ = std::thread::Builder::new()
            .name("plippa-decode".into())
            .spawn(move || worker_main(worker, cmd_rx));
        let mut feed = Self {
            window: SourceWindow::new(channels, rate_hz, cfg),
            track,
            cmd_tx,
            events: ev_rx,
            recycle: rec_tx,
            shared,
            epoch: 0,
            eof_at: None,
            applied_seq: 0,
            resync: 0,
            audio_started: false,
            fresh: true,
            waypoint: None,
            error: None,
            cfg: cfg.clone(),
            chunk_frames: params.chunk_frames.max(1),
        };
        if let Some(slot) = index {
            let _ = feed.cmd_tx.send(WorkerCmd::Index(slot));
        }
        feed.reposition(params.start_frame, params.start_frame, 0);
        feed
    }

    pub fn stats(&self) -> &WorkerStats {
        &self.shared.stats
    }

    /// Shared handle to the counters (the engine publishes it for the control plane / tests).
    pub fn stats_arc(&self) -> Arc<WorkerStats> {
        Arc::clone(&self.shared.stats)
    }

    /// Frames to hold ahead before the first write of an epoch (read by the worker's planner).
    pub fn set_start_prefetch(&self, frames: usize) {
        self.shared
            .start_prefetch
            .store(frames as u64, Ordering::Relaxed);
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Start a new epoch at `frame`: the window is reset there and the worker seeks. Nothing
    /// is rendered until the first block of the epoch arrives.
    pub fn reposition(&mut self, frame: u64, playhead: u64, dir: i8) {
        self.epoch += 1;
        self.window.reset(frame);
        self.eof_at = None;
        self.audio_started = false;
        self.fresh = true;
        self.publish(playhead, dir);
        let _ = self.cmd_tx.send(WorkerCmd::Reposition {
            epoch: self.epoch,
            frame,
        });
    }

    /// Ask the worker to decode toward a chase target (`None` clears the hint).
    pub fn hint(&mut self, hint: Option<Hint>) {
        // A landing zone behind the playhead is pinned against the history trim for as long
        // as the hint lives and fits (same near rule as the planner), so forward playback
        // cannot evict what the worker is fetching for it.
        let pin = hint.and_then(|h| {
            let lo = h.target_frame.saturating_sub(self.cfg.landing_history_frames(
                self.window.rate_hz(),
            ) as u64);
            let hi = h.target_frame + self.chunk_frames as u64;
            let span = hi.max(self.window.end_frame()).saturating_sub(lo) as usize;
            (h.waypoint_lead_frames == 0 && span <= self.window.cap_frames() * 9 / 10)
                .then_some(lo)
        });
        self.window.set_pin(pin);
        let _ = self.cmd_tx.send(WorkerCmd::Hint(hint));
    }

    /// The audio thread has written its first block of this epoch: the worker may now run
    /// ahead to the full lookahead.
    pub fn mark_audio_started(&mut self, playhead: u64, dir: i8) {
        if !self.audio_started {
            self.audio_started = true;
            self.publish(playhead, dir);
        }
    }

    pub fn audio_started(&self) -> bool {
        self.audio_started
    }

    /// True while the window holds the end of the stream (every frame to the end is held).
    pub fn eof(&self) -> bool {
        self.eof_at.is_some_and(|e| self.window.end_frame() >= e)
    }

    pub fn eof_at(&self) -> Option<u64> {
        self.eof_at
    }

    /// Take a delivered waypoint window (for the chase), if any.
    pub fn take_waypoint(&mut self) -> Option<Box<WaypointWindow>> {
        self.waypoint.take()
    }

    pub fn has_waypoint(&self) -> bool {
        self.waypoint.is_some()
    }

    /// Drop a delivered waypoint window without using it (retarget, cancel).
    pub fn discard_waypoint(&mut self) {
        self.waypoint = None;
    }

    /// True when a waypoint window (the `cap / 2` split) can hold the span a waypoint hint
    /// with `lead_frames` needs: lead + landing history + lookahead (+ one chunk), within 90%.
    pub fn waypoint_fits(&self, lead_frames: u64) -> bool {
        let ch = self.window.channels();
        let rate = self.window.sample_rate_hz();
        let need = lead_frames as usize
            + self.cfg.landing_history_frames(rate)
            + (self.cfg.waypoint_lookahead_s * f64::from(rate)) as usize
            + self.chunk_frames;
        need <= self.cfg.split(2).cap_frames(ch) * 9 / 10
    }

    /// Make a delivered waypoint window the playing window: its frames are copied into the
    /// existing ring (same allocation, so memory stays at the window cap; the `cap / 2`
    /// waypoint box is dropped by the caller's `Box`), a new epoch starts and the worker
    /// continues from the end of the adopted frames (its decoder already sits there).
    /// Returns false (nothing changed) if the frames do not fit.
    pub fn adopt_waypoint(&mut self, wp: &WaypointWindow, playhead: u64, dir: i8) -> bool {
        let src = &wp.window;
        let ch = self.window.channels();
        if src.channels() != ch || src.len_frames() > self.window.cap_frames() {
            return false;
        }
        let (start, end) = (src.start_frame(), src.end_frame());
        self.epoch += 1;
        self.window.reset(start);
        let mut buf = [0.0f32; 8192];
        let step = (buf.len() / ch).max(1);
        let mut at = start;
        while at < end {
            let n = ((end - at) as usize).min(step);
            let got = src.read_forward(at, &mut buf[..n * ch]);
            if got == 0 || !self.window.push(&buf[..got * ch], start) {
                break;
            }
            at += got as u64;
        }
        self.eof_at = None;
        self.fresh = false;
        self.audio_started = true;
        self.waypoint = None;
        self.publish(playhead, dir);
        let _ = self.cmd_tx.send(WorkerCmd::Reposition {
            epoch: self.epoch,
            frame: self.window.end_frame(),
        });
        true
    }

    /// A decode error reported by the worker (cleared by reading).
    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    /// Publish the window range, playhead and direction for the worker's planning.
    pub fn publish(&self, playhead: u64, dir: i8) {
        self.shared.view.write(&View {
            epoch: self.epoch,
            start: self.window.start_frame(),
            end: self.window.end_frame(),
            playhead,
            dir: dir.clamp(-1, 1),
            audio_started: self.audio_started,
            applied_seq: self.applied_seq,
            resync: self.resync,
        });
    }

    fn recycle(&self, v: Vec<f32>) {
        let _ = self.recycle.try_send(v);
    }

    /// Apply delivered blocks to the window (at most [`MAX_APPLY_PER_PUMP`]) and publish the
    /// view. Never blocks.
    pub fn pump(&mut self, playhead: u64, dir: i8) -> Pump {
        let mut out = Pump::default();
        let st = &self.shared.stats;
        for _ in 0..MAX_APPLY_PER_PUMP {
            match self.events.try_recv() {
                Ok(WorkerEvent::Block(b)) => {
                    self.applied_seq = self.applied_seq.max(b.seq);
                    if b.epoch != self.epoch || b.track != self.track {
                        st.blocks_stale.fetch_add(1, Ordering::Relaxed);
                        self.recycle(b.samples);
                        continue;
                    }
                    let ch = self.window.channels();
                    let frames = (b.samples.len() / ch) as u64;
                    let ok = match b.kind {
                        BlockKind::Append => {
                            if self.fresh && frames > 0 && b.start_frame != self.window.end_frame()
                            {
                                // The decoder landed elsewhere than requested.
                                self.window.reset(b.start_frame);
                                out.rebased = Some(b.start_frame);
                            }
                            if b.start_frame != self.window.end_frame() {
                                false
                            } else if frames == 0 || self.window.push(&b.samples, playhead) {
                                if let Some(e) = b.eof_at {
                                    self.eof_at = Some(e);
                                }
                                true
                            } else {
                                false
                            }
                        }
                        BlockKind::Prepend => {
                            b.start_frame + frames == self.window.start_frame()
                                && self.window.prepend(&b.samples, playhead)
                        }
                    };
                    if ok {
                        self.fresh = false;
                        out.applied += 1;
                        st.blocks_applied.fetch_add(1, Ordering::Relaxed);
                    } else {
                        self.resync += 1;
                        st.blocks_refused.fetch_add(1, Ordering::Relaxed);
                    }
                    self.recycle(b.samples);
                }
                Ok(WorkerEvent::Waypoint(w)) => self.waypoint = Some(w),
                Ok(WorkerEvent::Error(e)) => self.error = Some(e),
                Err(_) => break,
            }
        }
        self.publish(playhead, dir);
        out
    }
}

impl Drop for WindowFeed {
    fn drop(&mut self) {
        // Never join: a decoder stuck in a slow USB seek must not stall the audio thread.
        self.shared.stop.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(WorkerCmd::Stop);
    }
}

#[cfg(test)]
mod tests;
