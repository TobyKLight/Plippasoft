//! WARP_19: "Seek outside track" on the audio thread.
//!
//! With the setting on, the engine hands the audio thread the paths of the items `Previous` /
//! `Next` would reach ([`AudioCommand::SetNeighbours`]). While a hold scan runs the audio
//! thread opens the neighbour a few seconds before the boundary (on a helper thread: open,
//! and for an MP3 that is entered backward the frame index first), gives it its own decode
//! worker and window (a `cap / 2` split), and joins the two tracks **in one window** so the
//! rate stage renders the crossing like any other stretch of audio:
//!
//! * the earlier track `E` and the later track `L`; the join window is `E`'s window with the
//!   first [`JOIN_HEAD_FRAMES`] frames of `L` appended behind `E`'s last frame (`e_len`), so
//!   positions are `E` coordinates and `L` frame `j` is `e_len + j`;
//! * forward: `E` is the playing feed, `L`'s head is appended when the playhead comes within
//!   [`JOIN_ENTER_FRAMES`] of the end; reverse: `E`'s feed (opened near its end) is swapped in
//!   and `L`'s head appended when the playhead comes within [`JOIN_ENTER_FRAMES`] of 0;
//! * the logical item flips (one [`CrossEvent`] for the control plane) when the playhead passes
//!   `e_len` (either way); the feeds are exchanged for good once it is [`JOIN_SWAP_FRAMES`]
//!   beyond it, and `E`'s appended frames are taken off again when the playhead is far back;
//! * the stage's position and rate ramp are never touched except for the one rebase by
//!   `e_len`, so the output is sample-continuous at constant scan rate.
//!
//! Not ready when the boundary is reached: the audio holds at the edge (no block is rendered)
//! until the neighbour is; a neighbour that cannot be opened (or differs in rate / channels)
//! keeps the off behaviour for that crossing.
//!
//! SEAM_01: the same join also runs at normal speed with no scan ([`TrackState::seamless_step`]):
//! the engine always hands over the item that would play next, the bypass voice reads straight
//! through the appended head, and the swap happens once the playhead is well into it. There is
//! no hold at 1.0x (a stall is worse than a gap): a neighbour that is not ready when the track
//! ends leaves the ordinary end-of-stream path, which opens the next item as it always did.

use super::*;
use plippa_audio::isolate::{Isolation, RemoteDecoder};
use plippa_audio::SourceWindow;
use std::sync::mpsc::{channel, Receiver as MpscReceiver, TryRecvError as MpscTryRecvError};

/// Frames of the later track appended behind the earlier one.
pub const JOIN_HEAD_FRAMES: usize = 16_384;
/// The join is built when the playhead is this close to the boundary (must leave room for one
/// quantum at the fastest scan rate plus the sinc/boxcar margins inside [`JOIN_HEAD_FRAMES`]).
pub const JOIN_ENTER_FRAMES: u64 = 12_288;
/// Playhead distance beyond the boundary at which the feeds are exchanged (above
/// [`JOIN_ENTER_FRAMES`] so a join entered backward is not undone at once, and far enough
/// inside [`JOIN_HEAD_FRAMES`] that the next quantum at the fastest scan rate still reads only
/// appended frames).
pub const JOIN_SWAP_FRAMES: u64 = 13_312;
/// Frames from the boundary at which an unready neighbour holds the audio (one quantum at the
/// fastest scan rate plus margins).
const HOLD_EDGE_FRAMES: f64 = 2_600.0;

/// A neighbouring item as the engine resolved it.
#[derive(Debug, Clone)]
pub struct NeighbourSrc {
    pub path: PathBuf,
    pub capabilities: Capabilities,
    pub overspec: OverspecPolicy,
    /// PLAY_02: the neighbour's source `mediaList` position. This is the item's identity: a
    /// reorder may move it in the list, and two list entries may even share a file.
    pub item_position: u32,
    /// SEAM_01: the engine says this item plays **next at the end of the current one** (the
    /// decision `advance_past_current_item` would take: not repeat-item, not pause-after-each,
    /// no cue segment left). Only such a neighbour is joined at normal speed.
    pub seamless: bool,
    /// WARP_19: a scan may cross into this neighbour ("Seek outside track" is on). A scan
    /// follows `Next` / `Previous`, which ignore the play modes, so this can be set on a
    /// neighbour that is not `seamless`, and `seamless` can be set with it off: with the
    /// setting off a scan ends with the track, as it always did.
    pub scan: bool,
}

/// One logical item change during a scan (control plane: the `Next` / `Previous` state
/// changes without restarting audio).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrossEvent {
    /// +1 into the next item, -1 into the previous one.
    pub dir: i8,
    /// Position of the item being left at the crossing (its end, or 0), ms.
    pub left_ms: u64,
    /// Duration of the item entered (0 = unknown).
    pub new_duration_ms: u64,
    /// `starts_completed` when the event was made: an event of an older stream than the one
    /// the control plane last started is stale.
    pub generation: u64,
    /// PLAY_02: source `mediaList` position of the item entered. The control plane resolves
    /// the list entry by this, never by "whatever is the neighbour *now*" — a reorder during
    /// the scan must not change which item was heard.
    pub item_position: u32,
    /// SEAM_01: the change was made by a join at normal speed (no scan): the control plane
    /// does what an automatic advance does to its state, minus restarting the audio.
    pub seamless: bool,
}

struct Opened {
    decoder: RemoteDecoder,
    item_position: u32,
}

/// A neighbour whose decoder runs on its own worker.
pub(super) struct Nb {
    feed: WindowFeed,
    path: PathBuf,
    duration_ms: u64,
    len_frames: Option<u64>,
    /// Playhead published for the worker while the neighbour waits (its start frame).
    playhead: u64,
    /// PLAY_02: source `mediaList` position this neighbour stands for.
    item_position: u32,
}

enum NbState {
    Idle,
    Opening(MpscReceiver<Result<Opened, String>>),
    Ready(Box<Nb>),
    Failed,
}

struct Side {
    spec: Option<NeighbourSrc>,
    state: NbState,
}

impl Side {
    fn empty() -> Self {
        Self {
            spec: None,
            state: NbState::Idle,
        }
    }

    /// Path of the neighbour this side stands for (its spec, or the neighbour it holds).
    fn current_path(&self) -> Option<&Path> {
        match (&self.spec, &self.state) {
            (_, NbState::Ready(nb)) => Some(nb.path.as_path()),
            (Some(s), _) => Some(s.path.as_path()),
            _ => None,
        }
    }

    fn set_spec(&mut self, spec: Option<NeighbourSrc>) {
        let same = match (&spec, self.current_path()) {
            (Some(a), Some(b)) => a.path == b,
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.state = NbState::Idle;
        }
        self.spec = spec;
    }

    /// A scan may cross into this side. A spec that says nothing (the neighbour parked by an
    /// exchange of feeds) keeps the WARP_19 behaviour: allowed.
    fn scan_ok(&self) -> bool {
        self.spec.as_ref().is_none_or(|s| s.scan)
    }

    /// The neighbour can still become ready (or is).
    fn usable(&self) -> bool {
        !matches!(self.state, NbState::Failed)
            && (self.spec.is_some() || matches!(self.state, NbState::Ready(_)))
    }

    fn ready(&self) -> Option<&Nb> {
        match &self.state {
            NbState::Ready(nb) => Some(nb),
            _ => None,
        }
    }
}

/// The join of two tracks while a scan runs through it (see the module docs).
struct Join {
    /// Frames of the earlier track; the later one starts here on the join axis.
    e_len: u64,
    /// The playhead has passed `e_len`: the logical item is the later one.
    logical_later: bool,
    /// The later track (its feed keeps filling; exchanged in when the playhead is past).
    later: Box<Nb>,
    e_duration_ms: u64,
    l_duration_ms: u64,
    /// PLAY_02: source position of the earlier track, so a backward flip names the item the
    /// listener re-enters.
    e_item_position: u32,
}

/// Per-track neighbour state (`None` on the track while the setting is off or no scan runs:
/// the off behaviour is the unchanged code path).
pub(super) struct CrossCtl {
    prev: Side,
    next: Side,
    join: Option<Join>,
}

impl CrossCtl {
    fn new() -> Self {
        Self {
            prev: Side::empty(),
            next: Side::empty(),
            join: None,
        }
    }
}

/// Open `spec` on a helper thread: the decoder, and for an MP3 entered backward the frame
/// index first (so the end seek is an indexed one).
fn open_neighbour(
    spec: NeighbourSrc,
    reverse: bool,
    isolation: Isolation,
    library_roots: Vec<std::path::PathBuf>,
    stride: u32,
) -> Result<Opened, String> {
    let item_position = spec.item_position;
    // A neighbour is opened in its own helper: one that crashes on a hostile file fails this
    // open (the file is marked) and never disturbs the item that is playing.
    //
    // The open is confined beneath the library root (`SECFIX_05`): the engine only sent the
    // path, so the symlink-free open is the check, and the helper inherits that descriptor.
    let mut decoder = super::open_isolated_at(
        &isolation,
        &library_roots,
        &spec.path,
        &spec.capabilities,
        spec.overspec,
    )
    .map_err(|e| e.to_string())?;
    if reverse && decoder.extension() == "mp3" {
        // Entered backward: the end seek wants the index, so wait for the walk here (the
        // neighbour is not ready until it has finished, as before).
        decoder
            .build_index_blocking(stride, Duration::from_secs(60))
            .map_err(|e| e.to_string())?;
    }
    Ok(Opened {
        decoder,
        item_position,
    })
}

impl TrackState {
    /// `AudioCommand::SetNeighbours`: remember the neighbours the engine resolved. A neighbour
    /// whose path did not change keeps its opened state.
    pub(super) fn set_neighbours(
        &mut self,
        prev: Option<NeighbourSrc>,
        next: Option<NeighbourSrc>,
    ) {
        // SEAM_01: a seamless join that has not flipped yet is withdrawn when the engine says
        // the next item is no longer the one joined (repeat-item or pause-after-each switched
        // on, the list reordered). A scan owns its join (WARP_19 semantics, untouched), and a
        // join already flipped has been heard: it stays.
        if self.scan.is_none() {
            let withdrawn = self.cross.as_ref().and_then(|c| c.join.as_ref()).is_some_and(|j| {
                !j.logical_later
                    && next.as_ref().is_none_or(|n| n.path.as_path() != j.later.path.as_path())
            });
            if withdrawn {
                self.join_drop_to_earlier();
            }
        }
        if prev.is_none() && next.is_none() && self.cross.as_ref().is_none_or(|c| c.join.is_none())
        {
            self.cross = None;
            return;
        }
        let c = self.cross.get_or_insert_with(|| Box::new(CrossCtl::new()));
        c.prev.set_spec(prev);
        c.next.set_spec(next);
    }

    /// True while the playhead is inside a join of two tracks.
    pub(super) fn join_active(&self) -> bool {
        self.cross.as_ref().is_some_and(|c| c.join.is_some())
    }

    /// Where the listener's item is, for position publishing: inside a join the playhead is
    /// on the join axis and maps to the logical item.
    pub(super) fn logical_pos(&self) -> f64 {
        match self.cross.as_ref().and_then(|c| c.join.as_ref()) {
            Some(j) if j.logical_later => (self.stage.pos - j.e_len as f64).max(0.0),
            Some(j) => self.stage.pos.min(j.e_len as f64),
            None => self.stage.pos,
        }
    }

    fn start_open(&mut self, forward: bool, cfg: &LoopCfg) {
        let Some(c) = self.cross.as_mut() else { return };
        let side = if forward { &mut c.next } else { &mut c.prev };
        if !matches!(side.state, NbState::Idle) {
            return;
        }
        let Some(spec) = side.spec.clone() else {
            return;
        };
        if spec.path == self.path {
            // A track never joins into itself (repeat-item, a one-item list): the engine does
            // not send that neighbour; this is the audio thread's own guard.
            side.state = NbState::Failed;
            return;
        }
        let (tx, rx) = channel();
        let (isolation, stride) = (cfg.isolation.clone(), cfg.window.mp3_index_stride_frames);
        let library_roots = cfg.library_roots.clone();
        let spawned = thread::Builder::new()
            .name("plippa-nbopen".into())
            .spawn(move || {
                let _ = tx.send(open_neighbour(spec, !forward, isolation, library_roots, stride));
            });
        side.state = if spawned.is_ok() {
            NbState::Opening(rx)
        } else {
            NbState::Failed
        };
    }

    /// Poll the helper threads and keep the neighbours' workers pumped.
    fn cross_poll(&mut self, cfg: &LoopCfg, stats: &AudioThreadStats) {
        let (rate_hz, channels) = (self.rate_hz, self.channels);
        let chunk = cfg.chunk_frames;
        let wcfg = cfg.window.split(2);
        let scanning = self.scan.is_some();
        let Some(c) = self.cross.as_mut() else { return };
        for (forward, side) in [(false, &mut c.prev), (true, &mut c.next)] {
            // SEAM_01: a neighbour that is only a seamless candidate (no scan wants it) fails
            // silently: it is not the file playing, and the ordinary advance reports it if it
            // really is unplayable. The track then ends the old way (`seamless_fallbacks`).
            let quiet = side.spec.as_ref().is_some_and(|s| s.seamless && !(scanning && s.scan));
            let outcome = match &mut side.state {
                NbState::Opening(rx) => match rx.try_recv() {
                    Ok(Ok(o)) => Some(Ok(o)),
                    Ok(Err(e)) => Some(Err(e)),
                    Err(MpscTryRecvError::Empty) => None,
                    Err(MpscTryRecvError::Disconnected) => Some(Err("open thread gone".into())),
                },
                NbState::Ready(nb) => {
                    let _ = nb.feed.pump(nb.playhead, 1);
                    if let Some(e) = nb.feed.take_error() {
                        eprintln!("plippa-engine: neighbour decode failed: {e}");
                        side.state = NbState::Failed;
                        if !quiet {
                            stats.cross_failures.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    None
                }
                _ => None,
            };
            match outcome {
                Some(Ok(opened)) => {
                    let Opened {
                        decoder: d,
                        item_position,
                    } = opened;
                    let path = d.path().to_path_buf();
                    let len_frames = d.duration_frames();
                    let ok = d.sample_rate_hz() == rate_hz && d.channels() as usize == channels;
                    let start = if forward {
                        Some(0)
                    } else {
                        // End seek: a back-window's worth before the end (the worker then
                        // decodes forward to the end, so the track length is exact).
                        len_frames.map(|l| {
                            l.saturating_sub((wcfg.back_lead_s * f64::from(rate_hz)) as u64)
                        })
                    };
                    match (ok, start) {
                        (true, Some(start)) => {
                            let duration_ms = d.duration_ms().unwrap_or(0);
                            let mut feed = WindowFeed::spawn(
                                super::boxed_source(d, cfg.decoder_hook.as_ref()),
                                0,
                                &wcfg,
                                &FeedParams {
                                    chunk_frames: chunk,
                                    start_chunk_frames: START_CHUNK_FRAMES,
                                    start_prefetch_frames: START_CHUNK_FRAMES,
                                    start_frame: start,
                                },
                                None,
                            );
                            feed.mark_audio_started(start, 1);
                            side.state = NbState::Ready(Box::new(Nb {
                                feed,
                                path,
                                duration_ms,
                                len_frames,
                                playhead: start,
                                item_position,
                            }));
                        }
                        _ => {
                            eprintln!(
                                "plippa-engine: neighbour {} unusable (rate/channels differ or length unknown)",
                                path.display()
                            );
                            side.state = NbState::Failed;
                            if !quiet {
                                stats.cross_failures.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    eprintln!("plippa-engine: neighbour open failed: {e}");
                    side.state = NbState::Failed;
                    if !quiet {
                        stats.cross_failures.fetch_add(1, Ordering::SeqCst);
                    }
                }
                None => {}
            }
        }
    }

    fn emit_cross(
        &mut self,
        dir: i8,
        left_ms: u64,
        new_duration_ms: u64,
        item_position: u32,
        stats: &AudioThreadStats,
    ) {
        // The heard position restarts in the item entered (the unheard queue is still the
        // old item's tail): same rule as after a flush.
        self.floor_frame = self.logical_pos().max(0.0) as u64;
        let seamless = self.scan.is_none();
        if seamless {
            // (A seamless flip keeps the written count: the heard position is the written one
            // less the queue, which is 0 until the queue holds nothing of the old item any
            // more, and a pause right after the boundary can still put the whole unheard queue
            // back, tail of the old item included.)
            //
            // The position is published **before** the event, so the control plane never
            // reads the new item together with the old item's last position (which would
            // put it at the end of the track it just entered).
            stats.position_src_frames.store(self.floor_frame, Ordering::Relaxed);
        } else {
            self.written_since_flush = 0;
        }
        if let Ok(mut g) = stats.cross_events.lock() {
            g.push(CrossEvent {
                dir,
                left_ms,
                new_duration_ms,
                generation: stats.starts_completed.load(Ordering::SeqCst),
                item_position,
                seamless,
            });
        }
        stats.crossings.fetch_add(1, Ordering::SeqCst);
        stats.duration_ms.store(new_duration_ms, Ordering::Relaxed);
    }

    /// Exchange the feeds for good: the later track becomes the playing one and the earlier
    /// one (truncated back to its own length) waits as the previous neighbour.
    fn join_swap_to_later(&mut self, stats: &AudioThreadStats) {
        let Some(c) = self.cross.as_mut() else { return };
        let Some(j) = c.join.take() else { return };
        let mut nb = j.later;
        let entered = nb.item_position;
        self.feed.window.truncate_end(j.e_len);
        std::mem::swap(&mut self.feed, &mut nb.feed);
        std::mem::swap(&mut self.path, &mut nb.path);
        std::mem::swap(&mut self.len_frames, &mut nb.len_frames);
        // `nb` now holds the earlier track.
        nb.duration_ms = j.e_duration_ms;
        nb.playhead = j.e_len;
        nb.item_position = j.e_item_position;
        self.duration_ms = j.l_duration_ms;
        self.item_position = entered;
        self.stage.pos -= j.e_len as f64;
        self.rebase_shift -= j.e_len as f64;
        self.read_pos = self.stage.pos.max(0.0) as u64;
        // The earlier track waits as the previous neighbour; a next spec that still names the
        // track now playing is stale (the engine re-sends the specs for the new item).
        let prev_spec = c.prev.spec.take();
        c.prev = Side {
            spec: prev_spec,
            state: NbState::Ready(nb),
        };
        if c.next.spec.as_ref().is_some_and(|s| s.path == self.path) {
            c.next = Side::empty();
        }
        if let Ok(mut g) = stats.worker.lock() {
            *g = Some(self.feed.stats_arc());
        }
        self.floor_frame = self.stage.pos.max(0.0) as u64;
        self.written_since_flush = 0;
        self.feed.publish(self.read_pos, self.dir);
    }

    /// Take the appended frames off the earlier track again and park the later one as the
    /// next neighbour.
    fn join_drop_to_earlier(&mut self) {
        let Some(c) = self.cross.as_mut() else { return };
        let Some(j) = c.join.take() else { return };
        self.feed.window.truncate_end(j.e_len);
        let spec = c.next.spec.take();
        c.next = Side {
            spec,
            state: NbState::Ready(j.later),
        };
    }

    /// Flip the logical item when the playhead has passed the boundary (either way).
    fn join_flip(&mut self, stats: &AudioThreadStats) {
        let pos = self.stage.pos;
        let Some(j) = self.cross.as_mut().and_then(|c| c.join.as_mut()) else {
            return;
        };
        let (e_len, rate_hz) = (j.e_len, self.rate_hz);
        if !j.logical_later && pos >= e_len as f64 {
            j.logical_later = true;
            let (left, new) = (frame_to_ms(e_len, rate_hz), j.l_duration_ms);
            let item_position = j.later.item_position;
            self.emit_cross(1, left, new, item_position, stats);
        } else if j.logical_later && pos < e_len as f64 {
            j.logical_later = false;
            let new = j.e_duration_ms;
            let item_position = j.e_item_position;
            self.emit_cross(-1, 0, new, item_position, stats);
        }
    }

    /// Settle an active join where the playhead is (the scan ends or is cancelled inside it).
    pub(super) fn resolve_join(&mut self, stats: &AudioThreadStats) {
        if !self.join_active() {
            return;
        }
        self.join_flip(stats);
        let e_len = self
            .cross
            .as_ref()
            .and_then(|c| c.join.as_ref())
            .map(|j| j.e_len);
        match e_len {
            Some(e) if self.stage.pos >= e as f64 => self.join_swap_to_later(stats),
            _ => self.join_drop_to_earlier(),
        }
    }

    /// Per-quantum crossing control while a scan runs (after the scan set the rate): prewarm,
    /// build / flip / settle the join. `Some(block)` holds the audio at the edge.
    pub(super) fn cross_step(&mut self, cfg: &LoopCfg, stats: &AudioThreadStats) -> Option<Block> {
        self.cross_poll(cfg, stats);
        let dir = self.scan.as_ref()?.dir;
        let pos = self.stage.pos;
        // Inside a join: flips, the exchange and the way back.
        if self.join_active() {
            self.join_flip(stats);
            let e_len = self
                .cross
                .as_ref()
                .and_then(|c| c.join.as_ref())
                .map(|j| j.e_len)?;
            if pos >= (e_len + JOIN_SWAP_FRAMES) as f64 {
                self.join_swap_to_later(stats);
            } else if pos + (JOIN_HEAD_FRAMES as f64) < e_len as f64 {
                self.join_drop_to_earlier();
            }
            return None;
        }
        let fs = f64::from(self.rate_hz);
        let prewarm = cfg.window.cross_prewarm_s * fs * self.stage.rate().abs().max(1.0);
        if dir > 0 {
            let len = self.feed.eof_at().or(self.len_frames)?;
            let remaining = len as f64 - pos;
            let c = self.cross.as_ref()?;
            if !c.next.usable() || !c.next.scan_ok() {
                return None;
            }
            if remaining <= prewarm {
                self.stretch_to_varispeed(cfg, stats);
                self.start_open(true, cfg);
            }
            let c = self.cross.as_ref()?;
            let head_ready = c.next.ready().is_some_and(|nb| {
                nb.feed.window.start_frame() == 0
                    && (nb.feed.window.len_frames() >= JOIN_HEAD_FRAMES || nb.feed.eof())
            });
            if remaining <= JOIN_ENTER_FRAMES as f64 && head_ready && self.feed.eof() {
                self.join_enter_forward();
                return None;
            }
            if remaining <= HOLD_EDGE_FRAMES {
                // The neighbour is not ready at the edge: hold.
                return Some(Block::NeedData);
            }
        } else if dir < 0 {
            let c = self.cross.as_ref()?;
            if !c.prev.usable() || !c.prev.scan_ok() {
                return None;
            }
            if pos <= prewarm {
                self.stretch_to_varispeed(cfg, stats);
                self.start_open(false, cfg);
            }
            let c = self.cross.as_ref()?;
            let earlier_ready = c.prev.ready().is_some_and(|nb| nb.feed.eof());
            if pos <= JOIN_ENTER_FRAMES as f64
                && earlier_ready
                && self.feed.window.start_frame() == 0
            {
                self.join_enter_reverse(stats);
                return None;
            }
            if pos <= HOLD_EDGE_FRAMES {
                return Some(Block::NeedData);
            }
        }
        None
    }

    /// SEAM_01: per-quantum control of a **seamless** join at normal speed, called from
    /// `render_block` after the scan block and the voice reconciliation. `Some(block)` is never
    /// returned today (there is no hold at 1.0x); the signature mirrors [`Self::cross_step`].
    ///
    /// Only while nothing else owns the playhead: no scan, no chase, the bypass voice at unity
    /// moving forward, and an engine-supplied next item. It then does what the forward arm of
    /// `cross_step` does minus the rate change and minus the hold: opens the neighbour a few
    /// seconds ahead, appends its head behind the last frame once the playhead is within
    /// [`JOIN_ENTER_FRAMES`], flips the logical item at the boundary and exchanges the feeds a
    /// little later. If the head is not there when the track ends, nothing happens here and the
    /// ordinary end of stream follows (counted in `seamless_fallbacks`).
    pub(super) fn seamless_step(&mut self, cfg: &LoopCfg, stats: &AudioThreadStats) -> Option<Block> {
        if self.scan.is_some()
            || self.seek_active()
            || self.voice != VoiceKind::Bypass
            || !self.stage.at_unity()
            || self.stage.rate() <= 0.0
        {
            return None;
        }
        self.cross.as_ref()?;
        self.cross_poll(cfg, stats);
        let pos = self.stage.pos;
        if self.join_active() {
            self.join_flip(stats);
            let (e_len, appended) = self
                .cross
                .as_ref()
                .and_then(|c| c.join.as_ref())
                .map(|j| (j.e_len, self.feed.window.end_frame().saturating_sub(j.e_len)))?;
            // Exchange once well inside the appended head; a head shorter than that (a very
            // short track) is exchanged at its last frame.
            let swap_at = e_len + JOIN_SWAP_FRAMES.min(appended);
            if pos >= swap_at as f64 {
                // The flip already restarted the heard-position bookkeeping at the new item's
                // start; the exchange only re-bases the coordinates, so keep it as it is.
                let (floor, written) = (self.floor_frame, self.written_since_flush);
                self.join_swap_to_later(stats);
                self.floor_frame = floor;
                self.written_since_flush = written;
            } else if pos + (JOIN_HEAD_FRAMES as f64) < e_len as f64 {
                self.join_drop_to_earlier();
            }
            return None;
        }
        let len = self.feed.eof_at().or(self.len_frames)?;
        let remaining = len as f64 - pos;
        let c = self.cross.as_ref()?;
        if !c.next.spec.as_ref().is_some_and(|s| s.seamless) {
            return None;
        }
        if !c.next.usable() {
            if remaining <= 0.0 {
                stats.seamless_fallbacks.fetch_add(1, Ordering::SeqCst);
            }
            return None;
        }
        let prewarm = cfg.window.cross_prewarm_s * f64::from(self.rate_hz);
        if remaining <= prewarm {
            self.start_open(true, cfg);
        }
        let c = self.cross.as_ref()?;
        let head_ready = c.next.ready().is_some_and(|nb| {
            nb.feed.window.start_frame() == 0
                && (nb.feed.window.len_frames() >= JOIN_HEAD_FRAMES || nb.feed.eof())
        });
        if remaining <= JOIN_ENTER_FRAMES as f64 && head_ready && self.feed.eof() {
            self.join_enter_forward();
            return None;
        }
        if remaining <= 0.0 {
            // The track ends with no head to join: the ordinary end of stream takes over.
            stats.seamless_fallbacks.fetch_add(1, Ordering::SeqCst);
        }
        None
    }

    /// Frames until the logical boundary of a join that has not flipped yet (the bypass voice
    /// ends its block there, so the item change happens on exactly the first frame of the next
    /// item and the reported position restarts at 0:00).
    pub(super) fn frames_to_join_boundary(&self) -> Option<usize> {
        let j = self.cross.as_ref()?.join.as_ref()?;
        if j.logical_later || self.stage.pos >= j.e_len as f64 {
            return None;
        }
        Some((j.e_len as f64 - self.stage.pos).ceil() as usize)
    }

    /// Append `frames` frames of the later track (read from `src`'s window, from frame 0) to
    /// `dst`'s window, which must end exactly at its track end.
    fn append_head(
        dst: &mut SourceWindow,
        src: &SourceWindow,
        frames: usize,
        playhead: u64,
    ) -> bool {
        let ch = dst.channels();
        let mut buf = [0.0f32; 8192];
        let step = (buf.len() / ch).max(1);
        let end = frames.min(src.len_frames()) as u64;
        let mut at = 0u64;
        while at < end {
            let n = ((end - at) as usize).min(step);
            let got = src.read_forward(at, &mut buf[..n * ch]);
            if got == 0 || !dst.push(&buf[..got * ch], playhead) {
                return false;
            }
            at += got as u64;
        }
        true
    }

    fn stretch_to_varispeed(&mut self, cfg: &LoopCfg, stats: &AudioThreadStats) {
        if self.voice == VoiceKind::Stretch {
            // The stretcher keeps analysis state in the old coordinates: leave it (40 ms
            // crossfade) before the window changes shape.
            self.switch_voice(VoiceKind::Varispeed, cfg, stats);
        }
    }

    fn join_enter_forward(&mut self) {
        let Some(c) = self.cross.as_mut() else { return };
        let NbState::Ready(nb) = std::mem::replace(&mut c.next.state, NbState::Idle) else {
            return;
        };
        let Some(e_len) = self.feed.eof_at() else {
            c.next.state = NbState::Ready(nb);
            return;
        };
        if self.feed.window.end_frame() != e_len
            || !Self::append_head(
                &mut self.feed.window,
                &nb.feed.window,
                JOIN_HEAD_FRAMES,
                self.read_pos,
            )
        {
            self.feed.window.truncate_end(e_len);
            c.next.state = NbState::Ready(nb);
            return;
        }
        let e_duration_ms = self.duration_ms;
        let l_duration_ms = nb.duration_ms;
        c.join = Some(Join {
            e_len,
            logical_later: false,
            later: nb,
            e_duration_ms,
            l_duration_ms,
            e_item_position: self.item_position,
        });
    }

    fn join_enter_reverse(&mut self, stats: &AudioThreadStats) {
        let Some(c) = self.cross.as_mut() else { return };
        let NbState::Ready(mut earlier) = std::mem::replace(&mut c.prev.state, NbState::Idle)
        else {
            return;
        };
        let Some(e_len) = earlier.feed.eof_at() else {
            c.prev.state = NbState::Ready(earlier);
            return;
        };
        if earlier.feed.window.end_frame() != e_len
            || !Self::append_head(
                &mut earlier.feed.window,
                &self.feed.window,
                JOIN_HEAD_FRAMES,
                e_len,
            )
        {
            // (nothing appended on failure: `push` is all-or-nothing per block; take any
            // partial append off again)
            earlier.feed.window.truncate_end(e_len);
            c.prev.state = NbState::Ready(earlier);
            return;
        }
        // The earlier track's feed plays from here; the current (later) one parks in the join.
        let l_duration_ms = self.duration_ms;
        let e_duration_ms = earlier.duration_ms;
        let e_len_frames = earlier.len_frames;
        let e_item_position = earlier.item_position;
        let l_item_position = self.item_position;
        std::mem::swap(&mut self.feed, &mut earlier.feed);
        std::mem::swap(&mut self.path, &mut earlier.path);
        // `earlier` now holds the later (previously playing) track.
        let later = Nb {
            feed: earlier.feed,
            path: earlier.path,
            duration_ms: l_duration_ms,
            len_frames: self.len_frames,
            playhead: 0,
            item_position: l_item_position,
        };
        self.len_frames = e_len_frames;
        self.duration_ms = e_duration_ms;
        self.item_position = e_item_position;
        self.stage.pos += e_len as f64;
        self.rebase_shift += e_len as f64;
        self.read_pos = self.stage.pos.max(0.0) as u64;
        c.prev = Side::empty();
        c.join = Some(Join {
            e_len,
            logical_later: true,
            later: Box::new(later),
            e_duration_ms,
            l_duration_ms,
            e_item_position,
        });
        if let Ok(mut g) = stats.worker.lock() {
            *g = Some(self.feed.stats_arc());
        }
        self.feed.publish(self.read_pos, self.dir);
    }

    /// The scan is over: drop the neighbours (their workers stop) unless a join is still
    /// settling.
    pub(super) fn cross_scan_over(&mut self, stats: &AudioThreadStats) {
        self.resolve_join(stats);
        self.cross = None;
    }
}
