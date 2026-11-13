//! `nowPlayingList` — the live, bidirectional list the engine plays from (NB_04).
//!
//! Albums, playlists, mixes, audiobooks and single tracks all load into one
//! [`NowPlayingList`]. Loading **replaces** the whole list; it is a `Vec` plus a
//! current index (nothing is consumed on play), so a future "append" is an
//! ordinary `Vec::push`.
//!
//! # Entries
//!
//! * [`NowPlayingEntryKind::Track`] — a real media file (own decode/prepare).
//! * [`NowPlayingEntryKind::Cue`] — a *virtual* entry of the **current** track,
//!   taken from its navigable cue lists. Navigating to it seeks inside the same
//!   track ([`NavAction::SeekMs`]); it never causes a track reload.
//!
//! # Display structure (what UIs receive)
//!
//! [`NowPlayingList::entries`] returns one flat, nested-by-position list: every
//! `Track` entry in play order, and directly after the **current** track its cue
//! entries (enabled navigable cue lists merged, sorted by start time — or, while
//! cue-level shuffle is on, in the order the segments will play). When the
//! current track changes the previous track's cue entries disappear and the new
//! track's appear. `current_index` (IPC) is the flat index of the current track
//! entry; the highlighted cue is `current_index + 1 + current_cue_index`
//! (`current_cue_index` is delivered with playback status because it changes
//! with the playhead, not with the list).
//!
//! # Position model (warp-safe)
//!
//! The "current cue" is derived from the **media position** in media-time ms —
//! [`media_position_ms`] — never from wall-clock. A future tempo/warp mode only
//! has to pass its rate factor; cue highlighting stays exact.
//!
//! # Cue segments and cue-level shuffle (PLAY_03)
//!
//! The current track's enabled navigable cues define **segments** that tile the file exactly
//! once: segment *i* runs from its start to the next distinct start, the last one runs to EOF.
//! Explicit `endTime`s do not bound a segment ([`NowPlayingList::segment_at`]).
//!
//! A mix (or a lone file with at least two cues) may play those segments in a random order
//! inside the one stream: [`NowPlayingList::set_cue_shuffle`] installs a permutation, cues
//! [`NowPlayingList::next`] / [`NowPlayingList::previous`] walk it, and
//! [`NowPlayingList::entries`] publishes the cue rows in that order. Consequently
//! [`NowPlayingList::current_cue_index`] is the **permutation position** of the segment under
//! the playhead while shuffling (the chronological index when it is off, where the permutation
//! is the identity). The permutation lives in `cue_order` (play order → chronological segment
//! index) and is rebuilt from the last seed whenever the cue set changes, keeping the heard
//! segment first.

use crate::play_mode::{OrderMode, RepeatMode, SourceOrder};
use crate::PlayableItemId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A *second* `Previous` arriving within this window of the first goes to the
/// previous entry instead (NB_04b); measured between command arrivals on a
/// monotonic clock, never media time. Whether the *first* press of a pair
/// restarts or steps back is decided by [`PREVIOUS_RESTART_THRESHOLD_MS`].
pub const PREVIOUS_DOUBLE_PRESS_WINDOW: Duration = Duration::from_secs(3);

/// "Has this entry really been playing?" window for the **first** `Previous` of a
/// press pair. A first press restarts the current entry only once the playhead is
/// more than this far past the entry start; at the entry start (or within the first
/// second) it steps back one entry, so one tap per track matches `Next` (FIX_01C).
/// Not a pairing window — that is [`PREVIOUS_DOUBLE_PRESS_WINDOW`].
pub const PREVIOUS_RESTART_THRESHOLD_MS: u64 = 1000;

/// PLAY_03: playback boundary lead for cue-level shuffle, in media ms. The engine seeks this
/// early so that no audio from the *next chronological* segment is ever heard; at most this
/// much of the segment's tail may be cut. Sample-accurate scheduling is a later audio-thread
/// concern, not this constant's.
pub const CUE_BOUNDARY_LEAD_MS: u64 = 60;

/// Monotonic time source for [`PreviousPressTracker`] (injectable so tests need no sleeps).
/// `now` is the time since an arbitrary fixed origin.
pub trait Clock: Send + std::fmt::Debug {
    fn now(&self) -> Duration;
}

/// [`Clock`] backed by `std::time::Instant` (origin = creation).
#[derive(Debug, Clone, Copy)]
pub struct SystemClock(Instant);

impl SystemClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// Remembers the time of the last un-paired `Previous` press.
///
/// [`Self::press`] returns `true` when this press is a *subsequent* one (arrives within
/// [`PREVIOUS_DOUBLE_PRESS_WINDOW`], inclusive, of the last one) and then clears the
/// state, so a third quick press restarts again. Call [`Self::clear`] on any other
/// transport action (Next, Jump, Load*, Stop, Unload, seeks, cue-list toggles).
#[derive(Debug, Clone, Copy, Default)]
pub struct PreviousPressTracker {
    last: Option<Duration>,
}

impl PreviousPressTracker {
    pub fn press(&mut self, now: Duration) -> bool {
        let double = self
            .last
            .is_some_and(|t| now.saturating_sub(t) <= PREVIOUS_DOUBLE_PRESS_WINDOW);
        self.last = if double { None } else { Some(now) };
        double
    }

    pub fn clear(&mut self) {
        self.last = None;
    }
}

/// Media-time position of a stream.
///
/// `base_ms` is the media time at the stream origin (seek target), `frames` the
/// output frames written since, `rate_factor` the media seconds consumed per
/// output second (1.0 = normal speed; 1.25 = 25 % faster). `rate_factor == 1.0`
/// uses exact integer arithmetic.
pub fn media_position_ms(base_ms: u64, frames: u64, sample_rate_hz: u32, rate_factor: f64) -> u64 {
    if sample_rate_hz == 0 {
        return base_ms;
    }
    let advanced = if (rate_factor - 1.0).abs() < f64::EPSILON {
        frames.saturating_mul(1000) / u64::from(sample_rate_hz)
    } else {
        (frames as f64 * 1000.0 * rate_factor.max(0.0) / f64::from(sample_rate_hz)) as u64
    };
    base_ms.saturating_add(advanced)
}

/// What kind of collection is loaded (drives UI wording; from the release profile / kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum NowPlayingListKind {
    /// One track (orphan file, per-track playable, one-item list).
    #[default]
    Single,
    Album,
    Playlist,
    Mix,
    Audiobook,
    Podcast,
}

impl NowPlayingListKind {
    /// PLAY_02 (owner decision 8): audiobooks and podcasts always play their chapters in
    /// source order. The order setting is still stored for the next album, but it never
    /// reorders a list of this kind — neither at load, nor mid-playback, nor at a wrap.
    pub fn follows_order_mode(self) -> bool {
        !matches!(self, Self::Audiobook | Self::Podcast)
    }
}

/// Track (real media file) or Cue (virtual entry inside the current track).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NowPlayingEntryKind {
    Track,
    Cue,
}

/// One row of the display list sent over IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NowPlayingEntry {
    pub kind: NowPlayingEntryKind,
    pub title: String,
    pub artist: String,
    /// Track: 0. Cue: start inside the owning track (media ms).
    pub start_ms: u64,
    /// Cue: end inside the owning track when known. Track: `None`.
    pub end_ms: Option<u64>,
    /// Track: media length when known. Cue: `None`.
    pub duration_ms: Option<u64>,
    /// Index of the owning entry among the **Track** entries (not the flat index).
    pub track_index: u32,
    /// Library playable this track's metadata resolves from (release for playlist
    /// entries when known). `None` for cues.
    pub playable_id: Option<PlayableItemId>,
    /// False when the media file is missing (entry is skipped by navigation).
    pub available: bool,
    /// PLAY_02: source `mediaList` position for Track entries, `0` for Cue entries (cues are
    /// identified by `start_ms`). Appended: Postcard is positional.
    pub item_position: u32,
}

/// Resume behaviour override of one item (`overrideResumeBehaviour`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResumePolicy {
    #[default]
    Default,
    Never,
    Always,
}

/// One real track of the list (engine-side; not sent over IPC as-is).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListTrack {
    pub title: String,
    pub artist: String,
    pub duration_ms: Option<u64>,
    pub playable_id: Option<PlayableItemId>,
    /// 0-based position in the source `mediaList` (stable under shuffle).
    pub item_position: u32,
    /// Existing alternate files for this item. Empty ⇒ missing media.
    pub media_candidates: Vec<PathBuf>,
    /// Chosen file (set once resolved; a single candidate resolves immediately).
    pub media_path: Option<PathBuf>,
    /// `(library playable id, item position)` whose `.noudata` holds this track's cues.
    pub cue_source: Option<(i64, u32)>,
    /// This item is a mix (profile `mix`) with navigable cues: while it plays inside a
    /// collection its title is appended to the collection name.
    pub is_cued_mix: bool,
    pub resume_policy: ResumePolicy,
    /// Persisted `resumeFromTime` of this item, when present.
    pub resume_from_ms: Option<u64>,
}

impl ListTrack {
    pub fn available(&self) -> bool {
        !self.media_candidates.is_empty()
    }

    /// Mark the media as missing (found out at play time); navigation skips it.
    pub fn mark_unavailable(&mut self) {
        self.media_candidates.clear();
        self.media_path = None;
    }
}

/// One navigable cue of the current track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueSpan {
    /// Index of the cue list inside the track's `cueLists` (for enable/disable).
    pub list_index: usize,
    pub start_ms: u64,
    /// Effective end (explicit `endTime`, else next cue start, else track length).
    pub end_ms: Option<u64>,
    pub title: String,
}

/// Result of a navigation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavAction {
    /// Nothing to do (list end without wrap, nothing loaded).
    None,
    /// Seek inside the current track — **no re-prepare**.
    SeekMs(u64),
    /// Switch to another track (from its start). `skipped` are unavailable
    /// tracks passed over on the way (engine reports a notice for each).
    Track { index: usize, skipped: Vec<usize> },
}

/// Result of a mid-playback order change ([`NowPlayingList::set_order`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderOutcome {
    /// A shuffle was applied (fixed: current first, dynamic: only the unplayed tail).
    Applied,
    /// The source order was restored.
    Restored,
    /// Nothing changed: the fixed list was already in source order, or nothing is loaded.
    Unchanged,
    /// A dynamic queue has no source order to restore; the setting is still stored.
    NoFixedOrder,
    /// Fewer than two tracks: the mode is stored but there is nothing to reorder.
    TooShort,
}

/// The list itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlayingList {
    kind: NowPlayingListKind,
    tracks: Vec<ListTrack>,
    current: usize,
    /// Navigable cues of the current track, sorted by start (all lists, enabled or not).
    cues: Vec<CueSpan>,
    disabled_cue_lists: Vec<usize>,
    repeat: RepeatMode,
    pause_after_each: bool,
    source_order: SourceOrder,
    /// PLAY_03: the engine's copy of the order setting for cue playback (`OrderMode::Shuffle`).
    /// Whether it *applies* here is derived by [`NowPlayingList::cue_shuffle`]: only a mix or a
    /// lone file with at least two enabled navigable cues shuffles its segments.
    cue_shuffle: bool,
    /// PLAY_03: play order → chronological segment index (a permutation of
    /// `0..cue_segments()` while [`NowPlayingList::cue_shuffle`] is true; the identity when it
    /// is false). Rebuilt from `cue_seed` whenever the cue set changes.
    cue_order: Vec<usize>,
    /// PLAY_03: seed of the last permutation build, so a cue edit mid-playback rebuilds with
    /// the same seed instead of leaving a stale order.
    cue_seed: u64,
    /// PLAY_03: media-time start of the segment the listener is in. A rebuild keeps it at
    /// permutation position 0; `None` means "the first chronological segment".
    cue_anchor_ms: Option<u64>,
}

impl NowPlayingList {
    pub fn new(kind: NowPlayingListKind, tracks: Vec<ListTrack>) -> Self {
        Self {
            kind,
            tracks,
            current: 0,
            cues: Vec::new(),
            disabled_cue_lists: Vec::new(),
            repeat: RepeatMode::Off,
            pause_after_each: false,
            source_order: SourceOrder::Fixed,
            cue_shuffle: false,
            cue_order: Vec::new(),
            cue_seed: 0,
            cue_anchor_ms: None,
        }
    }

    /// What happens at the end of the item / queue (session setting, copied in by the engine).
    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    pub fn set_repeat(&mut self, repeat: RepeatMode) {
        self.repeat = repeat;
    }

    /// Pause between items instead of auto-advancing; the engine leaves the next item current
    /// at 0:00 (session setting).
    pub fn pause_after_each(&self) -> bool {
        self.pause_after_each
    }

    pub fn set_pause_after_each(&mut self, on: bool) {
        self.pause_after_each = on;
    }

    /// Whether this list has a source order to go back to.
    pub fn source_order(&self) -> SourceOrder {
        self.source_order
    }

    /// Mark this list as a dynamic queue (built by add-to-queue / play-next): its order can
    /// never be restored. Only the queue package (and the engine's lab hook) calls this.
    pub fn mark_dynamic(&mut self) {
        self.source_order = SourceOrder::Dynamic;
    }

    pub fn kind(&self) -> NowPlayingListKind {
        self.kind
    }

    /// Whether the order setting may reorder this list (see
    /// [`NowPlayingListKind::follows_order_mode`]): false for audiobooks and podcasts.
    pub fn order_applies(&self) -> bool {
        self.kind.follows_order_mode()
    }

    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn tracks(&self) -> &[ListTrack] {
        &self.tracks
    }

    pub fn track(&self, index: usize) -> Option<&ListTrack> {
        self.tracks.get(index)
    }

    pub fn track_mut(&mut self, index: usize) -> Option<&mut ListTrack> {
        self.tracks.get_mut(index)
    }

    pub fn current_index(&self) -> usize {
        self.current
    }

    pub fn current_track(&self) -> Option<&ListTrack> {
        self.tracks.get(self.current)
    }

    /// Make `index` current (clamped). The old track's cues are dropped; the
    /// caller supplies the new ones with [`Self::set_cues`].
    pub fn set_current(&mut self, index: usize) {
        self.current = index.min(self.tracks.len().saturating_sub(1));
        self.clear_cues();
    }

    /// Drop the current track's cues and everything derived from them (the permutation and its
    /// anchor): they belong to the entry that is no longer current.
    fn clear_cues(&mut self) {
        self.cues.clear();
        self.cue_order.clear();
        self.cue_anchor_ms = None;
    }

    /// List index of the track whose source `mediaList` position is `item_position`.
    pub fn index_of_item(&self, item_position: u32) -> Option<usize> {
        self.tracks
            .iter()
            .position(|t| t.item_position == item_position)
    }

    /// Reorder tracks into a random order (Fisher–Yates, deterministic in `seed`).
    /// Each track appears exactly once, so nothing repeats before all have played.
    /// Resets current to 0 and drops the cues (load-time only; use the `*_keep_*`
    /// operations mid-playback).
    pub fn shuffle(&mut self, seed: u64) {
        fisher_yates(&mut self.tracks, seed);
        self.current = 0;
        self.clear_cues();
    }

    /// Shuffle, then move the track with source position `item_position` to the front
    /// (resume-at-item + shuffle: start there, shuffle the rest).
    pub fn shuffle_keep_item_first(&mut self, seed: u64, item_position: Option<u32>) {
        self.shuffle(seed);
        if let Some(pos) = item_position {
            if let Some(i) = self.index_of_item(pos) {
                let t = self.tracks.remove(i);
                self.tracks.insert(0, t);
            }
        }
    }

    /// Fixed list, shuffle on: the **current track keeps playing** and everything else is
    /// shuffled ahead of it. Never touches the stream, the cues or the playhead: this is a
    /// list operation. `false` when there are fewer than two tracks.
    pub fn shuffle_keep_current_first(&mut self, seed: u64) -> bool {
        if self.tracks.len() < 2 {
            return false;
        }
        let current = self.tracks.remove(self.current);
        fisher_yates(&mut self.tracks, seed);
        self.tracks.insert(0, current);
        self.current = 0;
        true
    }

    /// Dynamic queue, shuffle on: only the tracks **after** the current one are shuffled; the
    /// played prefix and the current index stay as they are. `false` when fewer than two
    /// tracks are ahead of the current one.
    pub fn shuffle_after_current(&mut self, seed: u64) -> bool {
        let start = self.current + 1;
        if self.tracks.len().saturating_sub(start) < 2 {
            return false;
        }
        fisher_yates(&mut self.tracks[start..], seed);
        true
    }

    /// Fixed list, shuffle off: sort back into source (`mediaList`) order around the current
    /// track. The current index is recomputed from the playing track's `item_position` — never
    /// from the pre-sort index. Cues and the playhead are untouched. `false` when the list is
    /// already in source order (or empty).
    pub fn restore_source_order(&mut self) -> bool {
        if self.tracks.windows(2).all(|w| w[0].item_position <= w[1].item_position) {
            return false;
        }
        let Some(current_pos) = self.current_track().map(|t| t.item_position) else {
            return false;
        };
        self.tracks.sort_by_key(|t| t.item_position);
        self.current = self.index_of_item(current_pos).unwrap_or(0);
        true
    }

    /// Repeat-queue wrap on a shuffled list: a fresh pass over every track, never starting
    /// with the track that just finished (`current` is that track). The finished track's cues
    /// are dropped: they belong to the old current entry and the caller loads the new entry's
    /// cues. `false` for fewer than two tracks (a one-track queue repeat just restarts the
    /// track).
    pub fn start_new_pass(&mut self, seed: u64) -> bool {
        if self.tracks.len() < 2 {
            return false;
        }
        let finished = self.current_track().map(|t| t.item_position);
        fisher_yates(&mut self.tracks, seed);
        if let Some(finished) = finished {
            if self.tracks[0].item_position == finished {
                self.tracks.swap(0, 1);
            }
        }
        self.clear_cues();
        self.current = 0;
        true
    }

    /// Apply the player's order setting to this list (mid-playback). `Shuffle` keeps the
    /// playing track where it is; `Normal` puts a fixed list back into source order and
    /// leaves a dynamic queue alone ([`OrderOutcome::NoFixedOrder`]).
    pub fn set_order(&mut self, order: OrderMode, seed: u64) -> OrderOutcome {
        match order {
            OrderMode::Shuffle => {
                let applied = match self.source_order {
                    SourceOrder::Fixed => self.shuffle_keep_current_first(seed),
                    SourceOrder::Dynamic => self.shuffle_after_current(seed),
                };
                if applied {
                    OrderOutcome::Applied
                } else {
                    OrderOutcome::TooShort
                }
            }
            OrderMode::Normal => match self.source_order {
                SourceOrder::Dynamic => OrderOutcome::NoFixedOrder,
                SourceOrder::Fixed => {
                    if self.restore_source_order() {
                        OrderOutcome::Restored
                    } else {
                        OrderOutcome::Unchanged
                    }
                }
            },
        }
    }

    /// Replace the current track's navigable cues (sorted by start; stable) and rebuild the
    /// cue permutation from the last seed, so a cue edit mid-playback cannot leave a stale
    /// order (PLAY_03).
    pub fn set_cues(&mut self, mut cues: Vec<CueSpan>) {
        cues.sort_by_key(|c| c.start_ms);
        self.cues = cues;
        self.rebuild_cue_order();
    }

    pub fn cue_list_enabled(&self, list_index: usize) -> bool {
        !self.disabled_cue_lists.contains(&list_index)
    }

    /// Enable/disable one cue list for navigation and display (session only). The segments (and
    /// with them the permutation) are rebuilt at once (PLAY_03).
    pub fn set_cue_list_enabled(&mut self, list_index: usize, enabled: bool) {
        self.disabled_cue_lists.retain(|i| *i != list_index);
        if !enabled {
            self.disabled_cue_lists.push(list_index);
        }
        self.rebuild_cue_order();
    }

    /// Enabled navigable cues of the current track, sorted by start.
    pub fn active_cues(&self) -> impl Iterator<Item = &CueSpan> {
        self.cues
            .iter()
            .filter(|c| self.cue_list_enabled(c.list_index))
    }

    /// PLAY_03: whether cue-level shuffle can apply to this list: the kind must be a **mix** or
    /// a **lone file** (the file *is* the collection), and it must have at least two enabled
    /// navigable cues. Albums, playlists, audiobooks and podcasts reorder whole tracks instead
    /// (PLAY_02) — a cue-bearing track inside them never shuffles its segments.
    ///
    /// Every [`CueSpan`] is built from a navigable cue list, so "at least one navigable cue" is
    /// implied; the count is over the *enabled* cues, which are the only ones playback reaches.
    pub fn cue_shuffle_applies(&self) -> bool {
        matches!(self.kind, NowPlayingListKind::Mix | NowPlayingListKind::Single)
            && self.active_cues().count() >= 2
    }

    /// PLAY_03: cue-level shuffle is on for this list: the engine's order setting is `Shuffle`
    /// **and** [`Self::cue_shuffle_applies`].
    pub fn cue_shuffle(&self) -> bool {
        self.cue_shuffle && self.cue_shuffle_applies()
    }

    /// PLAY_03: install cue-level shuffle and (re)build the permutation from `seed`; `on ==
    /// false` installs the identity. Advances nothing and never touches the playhead. The
    /// caller supplies the seed (the engine's shuffle seed, advanced once per order change).
    pub fn set_cue_shuffle(&mut self, on: bool, seed: u64) {
        self.cue_shuffle = on;
        self.cue_seed = seed;
        self.rebuild_cue_order();
    }

    /// PLAY_03: tell the list where the playhead is. The segment containing `media_pos_ms`
    /// becomes the head of the permutation (rebuilding it), so a later cue edit keeps the
    /// segment being heard at permutation position 0 — exactly like
    /// [`Self::shuffle_keep_current_first`] keeps the playing track first.
    pub fn set_cue_anchor(&mut self, media_pos_ms: u64) {
        self.note_cue_anchor(media_pos_ms);
        self.rebuild_cue_order();
    }

    /// PLAY_03: remember the segment the playhead is in **without** touching the order.
    ///
    /// The engine's boundary advance walks the permutation as it plays, so the anchor has to
    /// follow the heard segment on every transition — but a rebuild there would reshuffle the
    /// segments that are still to come (and could repeat or skip one). The anchor only takes
    /// effect at the next rebuild (cue edit, order change), which then keeps the heard segment
    /// first, exactly as [`Self::set_cue_anchor`] does.
    pub fn note_cue_anchor(&mut self, media_pos_ms: u64) {
        let starts = self.segment_starts();
        self.cue_anchor_ms = starts
            .iter()
            .rposition(|s| *s <= media_pos_ms)
            .map(|i| starts[i]);
    }

    /// Distinct start times of the enabled navigable cues, ascending — the spec's navigable
    /// points (next `startTime` on **any** enabled navigable cue list) and, with PLAY_03, the
    /// segment starts.
    fn segment_starts(&self) -> Vec<u64> {
        let mut starts: Vec<u64> = self.active_cues().map(|c| c.start_ms).collect();
        starts.dedup();
        starts
    }

    /// PLAY_03: chronological index of the segment containing `media_pos_ms`.
    ///
    /// Segments tile the file exactly once: segment *i* runs from its start to the next distinct
    /// start and the last one runs to EOF, so an explicit `endTime` does **not** bound a
    /// segment here — a playhead in an `endTime` gap belongs to the segment before the gap.
    /// `None` before the first start, or when there is no enabled navigable cue.
    pub fn segment_at(&self, media_pos_ms: u64) -> Option<usize> {
        self.segment_starts()
            .iter()
            .rposition(|s| *s <= media_pos_ms)
    }

    /// PLAY_03: first millisecond of the segment containing `media_pos_ms` (`None` as
    /// [`Self::segment_at`]).
    pub fn current_segment_start_ms(&self, media_pos_ms: u64) -> Option<u64> {
        let starts = self.segment_starts();
        starts
            .iter()
            .rposition(|s| *s <= media_pos_ms)
            .map(|i| starts[i])
    }

    /// PLAY_03: end of the segment containing `media_pos_ms` — the next distinct start, or
    /// `None` when it runs to EOF (also `None` when no segment contains the position). This is
    /// the tiled end, never the cue's explicit `endTime`.
    pub fn current_segment_end_ms(&self, media_pos_ms: u64) -> Option<u64> {
        let starts = self.segment_starts();
        let seg = starts.iter().rposition(|s| *s <= media_pos_ms)?;
        starts.get(seg + 1).copied()
    }

    /// PLAY_03: build the permutation: the heard segment first, then one Fisher–Yates pass over
    /// every other segment (deterministic in `cue_seed`). With shuffle off — or when it cannot
    /// apply — the order is the identity.
    fn rebuild_cue_order(&mut self) {
        let starts = self.segment_starts();
        let n = starts.len();
        self.cue_order = (0..n).collect();
        if n < 2 || !self.cue_shuffle || !self.cue_shuffle_applies() {
            return;
        }
        let anchor = self
            .cue_anchor_ms
            .and_then(|ms| starts.iter().rposition(|s| *s <= ms))
            .unwrap_or(0);
        let mut rest: Vec<usize> = (0..n).filter(|&i| i != anchor).collect();
        fisher_yates_indices(&mut rest, self.cue_seed);
        self.cue_order = Vec::with_capacity(n);
        self.cue_order.push(anchor);
        self.cue_order.extend(rest);
        self.cue_anchor_ms = Some(starts[anchor]);
    }

    /// PLAY_03: start times of the published cue rows, in published order — the permutation's
    /// segments while shuffling, the chronological enabled cues otherwise.
    fn cue_row_starts(&self) -> Vec<u64> {
        if self.cue_shuffle() {
            let starts = self.segment_starts();
            self.cue_order
                .iter()
                .filter_map(|&seg| starts.get(seg).copied())
                .collect()
        } else {
            self.active_cues().map(|c| c.start_ms).collect()
        }
    }

    /// Index into the cue rows of [`Self::entries`] — the **permutation position** of the
    /// segment containing `media_pos_ms` — while cue-level shuffle is on (PLAY_03).
    ///
    /// With shuffle off this keeps the pre-PLAY_03 meaning: the chronological index (into
    /// [`Self::active_cues`]) of the cue containing `media_pos_ms`, i.e. the last cue with
    /// `start ≤ pos` whose end (explicit `endTime` or next start) is still ahead; a position in
    /// a gap left by an explicit `endTime` is `None`.
    ///
    /// While shuffling, containment is **tiled** ([`Self::segment_at`]): a playhead in an
    /// `endTime` gap is inside the preceding segment — the only visible difference for a file
    /// with explicit gaps. The published order is [`Self::entries`]' cue-row order, so
    /// `current_index + 1 + this` is always the row being heard.
    pub fn current_cue_index(&self, media_pos_ms: u64) -> Option<usize> {
        if self.cue_shuffle() {
            let seg = self.segment_at(media_pos_ms)?;
            return self.cue_order.iter().position(|&c| c == seg);
        }
        let active: Vec<&CueSpan> = self.active_cues().collect();
        (0..active.len()).rev().find(|&i| {
            let c = active[i];
            media_pos_ms >= c.start_ms && c.end_ms.is_none_or(|e| media_pos_ms < e)
        })
    }

    /// Distinct start times of all enabled navigable cues (spec: next `startTime`
    /// on **any** enabled navigable cue list), ascending.
    fn nav_starts(&self) -> Vec<u64> {
        self.segment_starts()
    }

    /// Display list (see module docs). While cue-level shuffle is on, the current track's cue
    /// rows are published in **play order** (`cue_order`), one row per segment; each row's
    /// `end_ms` is the segment's tiled end (its explicit `endTime` does not bound it).
    pub fn entries(&self) -> Vec<NowPlayingEntry> {
        let mut out = Vec::with_capacity(self.tracks.len() + self.cues.len());
        for (i, t) in self.tracks.iter().enumerate() {
            out.push(NowPlayingEntry {
                kind: NowPlayingEntryKind::Track,
                title: t.title.clone(),
                artist: t.artist.clone(),
                start_ms: 0,
                end_ms: None,
                duration_ms: t.duration_ms,
                track_index: i as u32,
                playable_id: t.playable_id,
                available: t.available(),
                item_position: t.item_position,
            });
            if i == self.current && self.cue_shuffle() {
                let cues: Vec<&CueSpan> = self.active_cues().collect();
                let starts = self.segment_starts();
                for (row, &seg) in self.cue_order.iter().enumerate() {
                    let Some(&start) = starts.get(seg) else {
                        continue;
                    };
                    let cue = cues.iter().find(|c| c.start_ms == start);
                    out.push(NowPlayingEntry {
                        kind: NowPlayingEntryKind::Cue,
                        title: match cue {
                            Some(c) if !c.title.trim().is_empty() => c.title.clone(),
                            _ => format!("Cue {}", row + 1),
                        },
                        artist: String::new(),
                        start_ms: start,
                        end_ms: starts.get(seg + 1).copied(),
                        duration_ms: None,
                        track_index: i as u32,
                        playable_id: None,
                        available: true,
                        // Cue rows are identified by `start_ms`, never by a source position.
                        item_position: 0,
                    });
                }
            } else if i == self.current {
                for (n, c) in self.active_cues().enumerate() {
                    out.push(NowPlayingEntry {
                        kind: NowPlayingEntryKind::Cue,
                        title: if c.title.trim().is_empty() {
                            format!("Cue {}", n + 1)
                        } else {
                            c.title.clone()
                        },
                        artist: String::new(),
                        start_ms: c.start_ms,
                        end_ms: c.end_ms,
                        duration_ms: None,
                        track_index: i as u32,
                        playable_id: None,
                        available: true,
                        // Cue rows are identified by `start_ms`, never by a source position.
                        item_position: 0,
                    });
                }
            }
        }
        out
    }

    /// Flat index (into [`Self::entries`]) of the current track entry.
    pub fn current_flat_index(&self) -> usize {
        self.current
    }

    /// `Next`: next cue start (any enabled navigable list) after the playhead, else next track.
    ///
    /// PLAY_03: with cue-level shuffle on, this walks the **permutation** instead of the
    /// clock: the segment after the one under the playhead, else the next track (the last
    /// segment of the pass behaves exactly like the old "last cue" branch).
    pub fn next(&self, media_pos_ms: u64) -> NavAction {
        if self.tracks.is_empty() {
            return NavAction::None;
        }
        if self.cue_shuffle() {
            return self.next_segment(media_pos_ms);
        }
        if let Some(s) = self.nav_starts().into_iter().find(|s| *s > media_pos_ms) {
            return NavAction::SeekMs(s);
        }
        self.next_track_action()
    }

    /// PLAY_03: `Next` along the segment permutation — the successor of the segment under the
    /// playhead; before the first start, the pass's first entry; after the last entry, the next
    /// track (or nothing).
    fn next_segment(&self, media_pos_ms: u64) -> NavAction {
        let starts = self.segment_starts();
        let Some(seg) = self.segment_at(media_pos_ms) else {
            let first = self.cue_order.first().copied().unwrap_or(0);
            return match starts.get(first) {
                Some(&start) => NavAction::SeekMs(start),
                None => self.next_track_action(),
            };
        };
        let next = self
            .cue_order
            .iter()
            .position(|&c| c == seg)
            .and_then(|p| self.cue_order.get(p + 1).copied());
        match next.and_then(|n| starts.get(n).copied()) {
            Some(start) => NavAction::SeekMs(start),
            None => self.next_track_action(),
        }
    }

    /// `Previous` (NB_04b / FIX_01C). A first press (`subsequent == false`) restarts the
    /// current entry (last cue with start <= playhead, else the track) only once the playhead
    /// is more than [`PREVIOUS_RESTART_THRESHOLD_MS`] past that entry start; at the entry start
    /// (or within the first second) it steps back one entry instead, so rapid taps walk back
    /// one entry per tap like `Next`. A subsequent press (see [`PreviousPressTracker`]) always
    /// goes to the previous entry (previous cue, then the track start when the first cue starts
    /// later, then the previous available track; at the very start of the list: restart again).
    pub fn previous(&self, media_pos_ms: u64, subsequent: bool) -> NavAction {
        if self.tracks.is_empty() {
            return NavAction::None;
        }
        if self.cue_shuffle() {
            return self.previous_segment(media_pos_ms, subsequent);
        }
        let starts = self.nav_starts();
        let anchor = starts.iter().rev().copied().find(|s| *s <= media_pos_ms);
        let restart = !subsequent
            && media_pos_ms.saturating_sub(anchor.unwrap_or(0)) > PREVIOUS_RESTART_THRESHOLD_MS;
        if restart {
            return NavAction::SeekMs(anchor.unwrap_or(0));
        }
        if let Some(a) = anchor {
            if let Some(prev) = starts.iter().rev().copied().find(|s| *s < a) {
                return NavAction::SeekMs(prev);
            }
            if a > 0 {
                // First cue starts after the track start: the track entry itself is "before" it.
                return NavAction::SeekMs(0);
            }
        }
        self.previous_track_action()
    }

    /// PLAY_03: `Previous` along the segment permutation. The "< 1 s past the start" rule
    /// applies to the **current segment's** start (not the chronological one); an earlier press
    /// goes to the permutation predecessor, then to the previous track (and at the very start
    /// of the list, restarts again).
    fn previous_segment(&self, media_pos_ms: u64, subsequent: bool) -> NavAction {
        let starts = self.segment_starts();
        let seg = self.segment_at(media_pos_ms);
        let anchor = seg.map(|s| starts[s]).unwrap_or(0);
        let restart = !subsequent
            && media_pos_ms.saturating_sub(anchor) > PREVIOUS_RESTART_THRESHOLD_MS;
        if restart {
            return NavAction::SeekMs(anchor);
        }
        if let Some(seg) = seg {
            let prev = self
                .cue_order
                .iter()
                .position(|&c| c == seg)
                .and_then(|p| p.checked_sub(1))
                .and_then(|p| self.cue_order.get(p).copied());
            if let Some(start) = prev.and_then(|s| starts.get(s).copied()) {
                return NavAction::SeekMs(start);
            }
        }
        self.previous_track_action()
    }

    /// Whether the Previous control is usable: always, once anything is loaded (at worst it
    /// restarts the first entry). NB_04b.
    pub fn can_prev(&self) -> bool {
        !self.tracks.is_empty()
    }

    /// Whether the Next control is usable at `media_pos_ms` (NB_14): a cue start after the
    /// playhead or another available track exists, or [`RepeatMode::Queue`] wraps a non-empty
    /// list. Exactly "`next(pos)` would do something", so the LED can never disagree with the
    /// press.
    pub fn can_next(&self, media_pos_ms: u64) -> bool {
        !matches!(self.next(media_pos_ms), NavAction::None)
    }

    /// Next available track after the current one (wraps when [`RepeatMode::Queue`]).
    pub fn next_track_action(&self) -> NavAction {
        let n = self.tracks.len();
        let mut skipped = Vec::new();
        for i in self.current + 1..n {
            if self.tracks[i].available() {
                return NavAction::Track { index: i, skipped };
            }
            skipped.push(i);
        }
        if self.repeat == RepeatMode::Queue {
            for i in 0..=self.current.min(n.saturating_sub(1)) {
                if self.tracks[i].available() {
                    return NavAction::Track { index: i, skipped };
                }
                skipped.push(i);
            }
        }
        NavAction::None
    }

    /// Previous available track (wraps when [`RepeatMode::Queue`]; at the very start: restart).
    pub fn previous_track_action(&self) -> NavAction {
        let n = self.tracks.len();
        let mut skipped = Vec::new();
        for i in (0..self.current).rev() {
            if self.tracks[i].available() {
                return NavAction::Track { index: i, skipped };
            }
            skipped.push(i);
        }
        if self.repeat == RepeatMode::Queue && n > 1 {
            for i in (self.current + 1..n).rev() {
                if self.tracks[i].available() {
                    return NavAction::Track { index: i, skipped };
                }
                skipped.push(i);
            }
        }
        NavAction::SeekMs(0)
    }

    /// WARP_19: the track `Next` (`forward`) / a second `Previous` press would switch to, as a
    /// list index: the neighbour a scan or jog may cross into. `None` at the first/last item
    /// of a non-repeating list, when no other track is available, or when the only
    /// candidate is the current track itself (one-track repeat). Cue entries inside a file
    /// are not tracks and never count.
    pub fn neighbour_track(&self, forward: bool) -> Option<usize> {
        let action = if forward {
            self.next_track_action()
        } else {
            self.previous_track_action()
        };
        match action {
            NavAction::Track { index, .. } if index != self.current => Some(index),
            _ => None,
        }
    }

    /// First available track after (`forward`) or before `index`, wrapping only when
    /// [`RepeatMode::Queue`]. Used to step over media found missing while starting a track.
    pub fn available_from(&self, index: usize, forward: bool) -> Option<usize> {
        let n = self.tracks.len();
        if n == 0 {
            return None;
        }
        let step = |i: usize| -> Option<usize> {
            if forward {
                if i + 1 < n {
                    Some(i + 1)
                } else if self.repeat == RepeatMode::Queue {
                    Some(0)
                } else {
                    None
                }
            } else if i > 0 {
                Some(i - 1)
            } else if self.repeat == RepeatMode::Queue {
                Some(n - 1)
            } else {
                None
            }
        };
        let mut i = index;
        for _ in 0..n {
            i = step(i)?;
            if self.tracks[i].available() {
                return Some(i);
            }
        }
        None
    }

    /// `JumpToEntry`: `flat_index` addresses [`Self::entries`].
    ///
    /// PLAY_03: the cue rows are published in play order while cue-level shuffle is on, so the
    /// row is a **permutation position** and this is the only place that maps it back (through
    /// `cue_order`) to a chronological segment start.
    pub fn jump_to_entry(&self, flat_index: usize) -> NavAction {
        let cue_starts = self.cue_row_starts();
        let cue_end = self.current + cue_starts.len(); // last flat index of the current cues
        if flat_index > self.current && flat_index <= cue_end {
            return match cue_starts.get(flat_index - self.current - 1) {
                Some(start) => NavAction::SeekMs(*start),
                None => NavAction::None,
            };
        }
        let track = if flat_index > cue_end {
            flat_index - cue_starts.len()
        } else {
            flat_index
        };
        if track >= self.tracks.len() {
            return NavAction::None;
        }
        if track == self.current {
            return NavAction::SeekMs(0);
        }
        NavAction::Track {
            index: track,
            skipped: Vec::new(),
        }
    }
}

/// Fisher–Yates shuffle of a slice with the tiny deterministic PRNG below, so every reshuffle
/// is reproducible from its seed.
fn fisher_yates(tracks: &mut [ListTrack], seed: u64) {
    let mut rng = XorShift64::new(seed);
    let n = tracks.len();
    for i in (1..n).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        tracks.swap(i, j);
    }
}

/// The same Fisher–Yates pass over plain indices (PLAY_03's cue-segment permutation).
fn fisher_yates_indices(order: &mut [usize], seed: u64) {
    let mut rng = XorShift64::new(seed);
    let n = order.len();
    for i in (1..n).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
}

/// Tiny deterministic PRNG (no `rand` dependency; used only for shuffle).
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        // Avoid the all-zero fixed point; warm up so nearby seeds diverge.
        let mut r = Self((seed ^ 0x9E37_79B9_7F4A_7C15).max(1));
        for _ in 0..8 {
            r.next();
        }
        r
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str) -> ListTrack {
        ListTrack {
            title: title.into(),
            media_candidates: vec![PathBuf::from(format!("{title}.wav"))],
            ..Default::default()
        }
    }

    fn missing(title: &str) -> ListTrack {
        ListTrack {
            title: title.into(),
            ..Default::default()
        }
    }

    fn album(n: usize) -> NowPlayingList {
        let tracks = (0..n)
            .map(|i| {
                let mut t = track(&format!("T{}", i + 1));
                t.item_position = i as u32;
                t
            })
            .collect();
        NowPlayingList::new(NowPlayingListKind::Album, tracks)
    }

    fn cue(list: usize, start: u64, end: Option<u64>) -> CueSpan {
        CueSpan {
            list_index: list,
            start_ms: start,
            end_ms: end,
            title: format!("c{start}"),
        }
    }

    /// One file, five cues at 0/60/120/180/240 s in a 300 s track.
    fn mixtape() -> NowPlayingList {
        let mut l = NowPlayingList::new(NowPlayingListKind::Mix, vec![track("mix")]);
        l.set_cues(
            (0..5)
                .map(|i| cue(0, i * 60_000, Some((i + 1) * 60_000)))
                .collect(),
        );
        l
    }

    fn idx(a: NavAction) -> Option<usize> {
        match a {
            NavAction::Track { index, .. } => Some(index),
            _ => None,
        }
    }

    #[test]
    fn media_position_is_frame_based_and_rate_scaled() {
        assert_eq!(media_position_ms(0, 48_000, 48_000, 1.0), 1_000);
        assert_eq!(media_position_ms(5_000, 24_000, 48_000, 1.0), 5_500);
        // 1.5x tempo: 1 s of output covers 1.5 s of media.
        assert_eq!(media_position_ms(0, 48_000, 48_000, 1.5), 1_500);
        assert_eq!(media_position_ms(10_000, 48_000, 48_000, 0.5), 10_500);
        assert_eq!(media_position_ms(7, 100, 0, 1.0), 7);
    }

    #[test]
    fn cue_derivation_uses_media_time_not_wall_clock() {
        let l = mixtape();
        // 50 s of wall clock at 2x tempo = 100 s of media -> cue 1 (60..120 s).
        let media = media_position_ms(0, 50 * 48_000, 48_000, 2.0);
        assert_eq!(media, 100_000);
        assert_eq!(l.current_cue_index(media), Some(1));
        // The same 50 s of wall clock at 1x would still be cue 0.
        let media_1x = media_position_ms(0, 50 * 48_000, 48_000, 1.0);
        assert_eq!(l.current_cue_index(media_1x), Some(0));
        // 0.5x: 130 s of wall clock = 65 s of media -> cue 1.
        assert_eq!(
            l.current_cue_index(media_position_ms(0, 130 * 48_000, 48_000, 0.5)),
            Some(1)
        );
    }

    #[test]
    fn cue_derivation_honours_explicit_end_gaps_and_edges() {
        let mut l = NowPlayingList::new(NowPlayingListKind::Mix, vec![track("m")]);
        l.set_cues(vec![
            cue(0, 10_000, Some(20_000)), // explicit end, gap until 30 s
            cue(0, 30_000, Some(40_000)),
        ]);
        assert_eq!(l.current_cue_index(0), None); // before the first cue
        assert_eq!(l.current_cue_index(10_000), Some(0));
        assert_eq!(l.current_cue_index(19_999), Some(0));
        assert_eq!(l.current_cue_index(20_000), None); // gap
        assert_eq!(l.current_cue_index(29_999), None);
        assert_eq!(l.current_cue_index(30_000), Some(1));
        assert_eq!(l.current_cue_index(40_000), None);
    }

    #[test]
    fn album_next_walks_tracks_and_stops_at_end() {
        let mut l = album(3);
        assert_eq!(idx(l.next(0)), Some(1));
        l.set_current(1);
        assert_eq!(idx(l.next(0)), Some(2));
        l.set_current(2);
        assert_eq!(l.next(0), NavAction::None);
        l.set_repeat(RepeatMode::Queue);
        assert_eq!(idx(l.next(0)), Some(0));
    }

    #[test]
    fn album_previous_restart_then_previous_table() {
        let mut l = album(3);
        l.set_current(1);
        // FIX_01C: a first press at the entry start or within the first second steps back.
        for pos in [0, 500, 1_000] {
            assert_eq!(
                l.previous(pos, false),
                NavAction::Track {
                    index: 0,
                    skipped: vec![]
                },
                "pos {pos}"
            );
        }
        // Later in the entry a first press restarts it.
        for pos in [1_001, 3_000, 3_001, 60_000] {
            assert_eq!(l.previous(pos, false), NavAction::SeekMs(0), "pos {pos}");
        }
        // Subsequent press: previous track, whatever the position.
        for pos in [0, 500, 60_000] {
            assert_eq!(
                l.previous(pos, true),
                NavAction::Track {
                    index: 0,
                    skipped: vec![]
                },
                "pos {pos}"
            );
        }
        // First track: a press restarts again (never an error)...
        l.set_current(0);
        assert_eq!(l.previous(500, true), NavAction::SeekMs(0));
        assert_eq!(l.previous(500, false), NavAction::SeekMs(0));
        // ...unless the list wraps: "back a track" wraps for a first press too.
        l.set_repeat(RepeatMode::Queue);
        assert_eq!(idx(l.previous(500, true)), Some(2));
        assert_eq!(idx(l.previous(500, false)), Some(2));
        assert_eq!(l.previous(5_000, false), NavAction::SeekMs(0));
    }

    #[test]
    fn one_track_list_always_restarts() {
        let l = album(1);
        // Single item: "back a track" degenerates to a restart; never None, never a panic.
        assert_eq!(l.previous(0, false), NavAction::SeekMs(0));
        assert_eq!(l.previous(1_000, false), NavAction::SeekMs(0));
        assert_eq!(l.previous(40_000, false), NavAction::SeekMs(0));
        assert_eq!(l.previous(0, true), NavAction::SeekMs(0));
        assert_eq!(l.previous(40_000, true), NavAction::SeekMs(0));
    }

    #[test]
    fn previous_threshold_boundary() {
        let mut l = album(3);
        l.set_current(1);
        // Exactly 1000 ms is still "at the start" -> step back; 1001 ms restarts.
        assert_eq!(
            l.previous(1_000, false),
            NavAction::Track {
                index: 0,
                skipped: vec![]
            }
        );
        assert_eq!(l.previous(1_001, false), NavAction::SeekMs(0));
    }

    #[test]
    fn previous_at_track_one_within_threshold() {
        let mut l = album(3);
        l.set_current(0);
        assert_eq!(l.previous(0, false), NavAction::SeekMs(0));
        assert_eq!(l.previous(1_000, false), NavAction::SeekMs(0));
        assert_eq!(l.previous(20_000, false), NavAction::SeekMs(0));
    }

    #[test]
    fn previous_subsequent_ignores_threshold() {
        // Rapid taps: the tracker alternates first/subsequent, and every press near the entry
        // start walks back one track — but the 3 s pairing window still governs press 2.
        let ms = Duration::from_millis;
        let mut l = album(3);
        let mut t = PreviousPressTracker::default();
        l.set_current(2);
        let first = t.press(ms(0));
        assert!(!first);
        assert_eq!(idx(l.previous(200, first)), Some(1));
        l.set_current(1);
        let second = t.press(ms(300));
        assert!(second);
        assert_eq!(idx(l.previous(200, second)), Some(0));
        l.set_current(0);
        let third = t.press(ms(600));
        assert!(!third, "third tap is a new pair");
        assert_eq!(l.previous(200, third), NavAction::SeekMs(0));
        // A first press after more than 1 s still restarts the entry.
        l.set_current(1);
        let later = t.press(ms(10_000));
        assert!(!later);
        assert_eq!(l.previous(10_000, later), NavAction::SeekMs(0));
    }

    #[test]
    fn can_prev_is_true_whenever_something_is_loaded() {
        let mut l = album(3);
        for i in 0..3 {
            l.set_current(i);
            assert!(l.can_prev());
        }
        l.set_repeat(RepeatMode::Queue);
        assert!(l.can_prev());
        assert!(album(1).can_prev());
        assert!(!NowPlayingList::new(NowPlayingListKind::Single, vec![]).can_prev());
    }

    #[test]
    fn can_next_tables_for_album_repeat_and_cues() {
        let empty = NowPlayingList::new(NowPlayingListKind::Single, vec![]);
        assert!(!empty.can_next(0), "nothing loaded");
        let mut l = album(3);
        assert!(l.can_next(0));
        l.set_current(1);
        assert!(l.can_next(0));
        l.set_current(2);
        assert!(!l.can_next(0), "last track, repeat off");
        l.set_repeat(RepeatMode::Queue);
        assert!(l.can_next(0), "repeat wraps");
        let mut one = album(1);
        assert!(!one.can_next(5_000));
        one.set_repeat(RepeatMode::Queue);
        assert!(one.can_next(5_000), "repeat on a one-track list");

        // Mixtape: cue starts after the playhead, none after the last cue.
        let m = mixtape();
        assert!(m.can_next(0));
        assert!(m.can_next(239_999));
        assert!(!m.can_next(240_000), "on the last cue, nothing after");
        assert!(!m.can_next(299_000));
        // Missing later tracks do not count.
        let mut u = NowPlayingList::new(NowPlayingListKind::Album, vec![track("a"), missing("b")]);
        assert!(!u.can_next(0));
        u.set_repeat(RepeatMode::Queue);
        assert!(u.can_next(0), "wraps to the (available) current track");
    }

    #[test]
    fn previous_tracker_window_and_clear() {
        let ms = Duration::from_millis;
        let mut t = PreviousPressTracker::default();
        assert!(!t.press(ms(10_000))); // first
        assert!(t.press(ms(12_000))); // within window -> subsequent, state cleared
        assert!(!t.press(ms(12_500))); // third restarts again
        assert!(t.press(ms(12_500) + PREVIOUS_DOUBLE_PRESS_WINDOW)); // exactly X: within
        assert!(!t.press(ms(20_000)));
        assert!(!t.press(ms(20_000) + PREVIOUS_DOUBLE_PRESS_WINDOW + ms(1))); // > X: restart
        assert!(t.press(ms(21_000) + PREVIOUS_DOUBLE_PRESS_WINDOW)); // pairs with previous restart
                                                                     // Other transport actions clear.
        assert!(!t.press(ms(30_000)));
        t.clear();
        assert!(!t.press(ms(30_100)));
    }

    #[test]
    fn mixtape_next_walks_cues_then_leaves_track() {
        let l = mixtape();
        assert_eq!(l.next(0), NavAction::SeekMs(60_000));
        assert_eq!(l.next(59_999), NavAction::SeekMs(60_000));
        assert_eq!(l.next(60_000), NavAction::SeekMs(120_000));
        assert_eq!(l.next(200_000), NavAction::SeekMs(240_000));
        // After the last cue: advance to next track (none here) -> no-op.
        assert_eq!(l.next(250_000), NavAction::None);
    }

    #[test]
    fn mixtape_next_after_last_cue_advances_to_next_track() {
        let mut l = NowPlayingList::new(NowPlayingListKind::Album, vec![track("a"), track("b")]);
        l.set_cues(vec![cue(0, 0, Some(10_000)), cue(0, 10_000, None)]);
        assert_eq!(
            l.next(12_000),
            NavAction::Track {
                index: 1,
                skipped: vec![]
            }
        );
    }

    #[test]
    fn mixtape_previous_rule_table() {
        let l = mixtape();
        // cue 2 starts at 120 s. A first press at the cue start or within its first second
        // steps back to the previous cue; later it restarts the cue (FIX_01C).
        for pos in [120_000, 120_500, 121_000] {
            assert_eq!(
                l.previous(pos, false),
                NavAction::SeekMs(60_000),
                "pos {pos}"
            );
        }
        for pos in [121_001, 123_001, 130_000] {
            assert_eq!(
                l.previous(pos, false),
                NavAction::SeekMs(120_000),
                "pos {pos}"
            );
        }
        // A subsequent press always steps back one entry.
        for pos in [120_000, 121_000, 123_001, 130_000] {
            assert_eq!(
                l.previous(pos, true),
                NavAction::SeekMs(60_000),
                "pos {pos}"
            );
        }
        // First cue at 0: a press at 1 s steps back; with no earlier entry that restarts.
        assert_eq!(l.previous(1_000, false), NavAction::SeekMs(0));
        assert_eq!(l.previous(1_000, true), NavAction::SeekMs(0));
    }

    #[test]
    fn first_cue_after_track_start_steps_back_to_track_start() {
        let mut l = NowPlayingList::new(NowPlayingListKind::Mix, vec![track("a"), track("b")]);
        l.set_current(1);
        l.set_cues(vec![cue(0, 20_000, None)]);
        // FIX_01C: a first press 1 s into the cue steps back to the intro; later it restarts.
        assert_eq!(l.previous(21_000, false), NavAction::SeekMs(0));
        assert_eq!(l.previous(20_000, true), NavAction::SeekMs(0));
        assert_eq!(l.previous(21_001, false), NavAction::SeekMs(20_000));
        // In the intro: a first press within the first second steps back a track; after 1 s it
        // restarts, and a subsequent press always steps back.
        assert_eq!(l.previous(2_000, false), NavAction::SeekMs(0));
        assert_eq!(idx(l.previous(500, false)), Some(0));
        assert_eq!(idx(l.previous(0, true)), Some(0));
    }

    #[test]
    fn multiple_navigable_lists_merge_and_can_be_disabled() {
        let mut l = NowPlayingList::new(NowPlayingListKind::Audiobook, vec![track("book")]);
        l.set_cues(vec![
            cue(0, 0, None),
            cue(0, 100_000, None),
            cue(1, 40_000, Some(50_000)),
            cue(1, 100_000, None), // same start as list 0: one nav point
        ]);
        assert_eq!(l.next(0), NavAction::SeekMs(40_000));
        assert_eq!(l.next(40_000), NavAction::SeekMs(100_000));
        assert_eq!(l.next(100_000), NavAction::None);
        l.set_cue_list_enabled(1, false);
        assert!(!l.cue_list_enabled(1));
        assert_eq!(l.next(0), NavAction::SeekMs(100_000));
        assert_eq!(l.active_cues().count(), 2);
        l.set_cue_list_enabled(1, true);
        assert_eq!(l.active_cues().count(), 4);
    }

    #[test]
    fn unavailable_tracks_are_skipped_with_report() {
        let l = NowPlayingList::new(
            NowPlayingListKind::Album,
            vec![track("a"), missing("b"), missing("c"), track("d")],
        );
        assert_eq!(
            l.next(0),
            NavAction::Track {
                index: 3,
                skipped: vec![1, 2]
            }
        );
        let mut l2 = l.clone();
        l2.set_current(3);
        assert_eq!(
            l2.previous(0, true),
            NavAction::Track {
                index: 0,
                skipped: vec![2, 1]
            }
        );
        // Nothing playable ahead -> no-op.
        let l3 = NowPlayingList::new(NowPlayingListKind::Album, vec![track("a"), missing("b")]);
        assert_eq!(l3.next(0), NavAction::None);
    }

    #[test]
    fn available_from_steps_over_missing_and_wraps_only_on_repeat() {
        let mut l = NowPlayingList::new(
            NowPlayingListKind::Album,
            vec![track("a"), missing("b"), track("c")],
        );
        assert_eq!(l.available_from(0, true), Some(2));
        assert_eq!(l.available_from(2, true), None);
        assert_eq!(l.available_from(2, false), Some(0));
        assert_eq!(l.available_from(0, false), None);
        l.set_repeat(RepeatMode::Queue);
        assert_eq!(l.available_from(2, true), Some(0));
        assert_eq!(l.available_from(0, false), Some(2));
    }

    #[test]
    fn entries_nest_current_tracks_cues_only() {
        let mut l = album(3);
        l.set_current(1);
        l.set_cues(vec![cue(0, 0, Some(5_000)), cue(0, 5_000, None)]);
        let e = l.entries();
        assert_eq!(e.len(), 5);
        let kinds: Vec<_> = e.iter().map(|x| x.kind).collect();
        use NowPlayingEntryKind::*;
        assert_eq!(kinds, vec![Track, Track, Cue, Cue, Track]);
        assert_eq!(l.current_flat_index(), 1);
        assert_eq!(e[2].track_index, 1);
        assert_eq!(e[2].start_ms, 0);
        assert_eq!(e[3].start_ms, 5_000);
        assert_eq!(e[4].track_index, 2);
        // Changing track drops the old cues.
        l.set_current(2);
        assert_eq!(l.entries().len(), 3);
    }

    #[test]
    fn jump_to_entry_maps_flat_indices() {
        let mut l = album(3);
        l.set_current(1);
        l.set_cues(vec![cue(0, 0, Some(5_000)), cue(0, 5_000, None)]);
        // flat: 0 T1, 1 T2(cur), 2 cue0, 3 cue1, 4 T3
        assert_eq!(
            l.jump_to_entry(0),
            NavAction::Track {
                index: 0,
                skipped: vec![]
            }
        );
        assert_eq!(l.jump_to_entry(1), NavAction::SeekMs(0));
        assert_eq!(l.jump_to_entry(2), NavAction::SeekMs(0));
        assert_eq!(l.jump_to_entry(3), NavAction::SeekMs(5_000));
        assert_eq!(
            l.jump_to_entry(4),
            NavAction::Track {
                index: 2,
                skipped: vec![]
            }
        );
        assert_eq!(l.jump_to_entry(5), NavAction::None);
    }

    #[test]
    fn shuffle_is_a_permutation_deterministic_and_keeps_item_first() {
        let n = 20;
        let mut a = album(n);
        a.shuffle(42);
        let mut positions: Vec<u32> = a.tracks().iter().map(|t| t.item_position).collect();
        let shuffled = positions.clone();
        positions.sort_unstable();
        assert_eq!(
            positions,
            (0..n as u32).collect::<Vec<_>>(),
            "no repeats, none lost"
        );
        assert_ne!(shuffled, (0..n as u32).collect::<Vec<_>>(), "order changed");

        let mut b = album(n);
        b.shuffle(42);
        assert_eq!(
            b.tracks()
                .iter()
                .map(|t| t.item_position)
                .collect::<Vec<_>>(),
            shuffled,
            "same seed, same order"
        );
        let mut c = album(n);
        c.shuffle(43);
        assert_ne!(
            c.tracks()
                .iter()
                .map(|t| t.item_position)
                .collect::<Vec<_>>(),
            shuffled
        );

        let mut d = album(n);
        d.shuffle_keep_item_first(7, Some(11));
        assert_eq!(d.tracks()[0].item_position, 11);
        let mut all: Vec<u32> = d.tracks().iter().map(|t| t.item_position).collect();
        all.sort_unstable();
        assert_eq!(all, (0..n as u32).collect::<Vec<_>>());
        assert_eq!(d.current_index(), 0);
    }

    #[test]
    fn walking_a_shuffled_list_visits_every_track_once() {
        let mut l = album(8);
        l.shuffle(99);
        let mut seen = vec![l.tracks()[0].item_position];
        while let NavAction::Track { index, .. } = l.next_track_action() {
            l.set_current(index);
            seen.push(l.tracks()[index].item_position);
        }
        seen.sort_unstable();
        assert_eq!(seen, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn empty_list_never_errors() {
        let l = NowPlayingList::new(NowPlayingListKind::Single, vec![]);
        assert_eq!(l.next(0), NavAction::None);
        assert_eq!(l.previous(0, false), NavAction::None);
        assert_eq!(l.jump_to_entry(0), NavAction::None);
        assert!(l.entries().is_empty());
    }

    #[test]
    fn neighbour_track_follows_next_and_previous_selection() {
        let mk = |avail: &[bool]| {
            NowPlayingList::new(
                NowPlayingListKind::Album,
                avail
                    .iter()
                    .map(|a| ListTrack {
                        media_candidates: if *a { vec![PathBuf::from("x.wav")] } else { vec![] },
                        ..Default::default()
                    })
                    .collect(),
            )
        };
        let mut l = mk(&[true, false, true]);
        assert_eq!(l.neighbour_track(false), None, "first item of a non-repeating list");
        assert_eq!(l.neighbour_track(true), Some(2), "skips the unavailable track");
        l.set_current(2);
        assert_eq!(l.neighbour_track(true), None, "last item");
        assert_eq!(l.neighbour_track(false), Some(0));
        l.set_repeat(RepeatMode::Queue);
        assert_eq!(l.neighbour_track(true), Some(0), "wraps like Next");
        let mut single = mk(&[true]);
        single.set_repeat(RepeatMode::Queue);
        assert_eq!(single.neighbour_track(true), None, "never the current track itself");
        assert_eq!(single.neighbour_track(false), None);
    }

    // ---- PLAY_02: mid-playback order operations (list-only: cues follow the track) ----

    fn positions(l: &NowPlayingList) -> Vec<u32> {
        l.tracks().iter().map(|t| t.item_position).collect()
    }

    #[test]
    fn shuffle_keep_current_first_is_a_permutation_with_the_current_item_first() {
        let mut l = album(10);
        l.set_current(3);
        assert!(l.shuffle_keep_current_first(42));
        let order = positions(&l);
        assert_eq!(order[0], 3, "the playing track stays first");
        assert_eq!(l.current_index(), 0);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..10u32).collect::<Vec<_>>(), "each track exactly once");
        assert_ne!(order[1..], (0..10u32).filter(|p| *p != 3).collect::<Vec<_>>()[..]);

        // Deterministic in the seed.
        let mut same = album(10);
        same.set_current(3);
        assert!(same.shuffle_keep_current_first(42));
        assert_eq!(positions(&same), order);
        let mut other = album(10);
        other.set_current(3);
        assert!(other.shuffle_keep_current_first(43));
        assert_ne!(positions(&other)[1..], order[1..]);

        // `set_order` uses this operation for a fixed list, and restores around it.
        let mut s = album(6);
        s.set_current(2);
        assert_eq!(s.set_order(OrderMode::Shuffle, 5), OrderOutcome::Applied);
        assert_eq!(s.tracks()[0].item_position, 2);
        assert_eq!(s.set_order(OrderMode::Normal, 5), OrderOutcome::Restored);
        assert_eq!(positions(&s), (0..6u32).collect::<Vec<_>>());
        assert_eq!(s.current_index(), 2, "back on the same item");

        let mut one = album(1);
        assert!(!one.shuffle_keep_current_first(1), "nothing to reorder");
    }

    #[test]
    fn shuffle_keep_current_first_keeps_the_cues_and_the_playhead_target() {
        let mut l = album(3);
        l.set_current(1);
        l.set_cues(vec![cue(0, 0, Some(5_000)), cue(0, 5_000, None)]);
        assert!(l.shuffle_keep_current_first(9));
        assert_eq!(l.current_index(), 0);
        assert_eq!(l.current_track().unwrap().item_position, 1);
        assert_eq!(l.current_cue_index(6_000), Some(1), "cues stay with the track");
        assert_eq!(l.active_cues().count(), 2);
        // The cue rows are published directly under the (now first) current track.
        let e = l.entries();
        assert_eq!(e[0].item_position, 1);
        assert_eq!(e[1].kind, NowPlayingEntryKind::Cue);
        assert_eq!(e[1].start_ms, 0);
        assert_eq!(e[1].item_position, 0, "cues carry no source position");
    }

    #[test]
    fn shuffle_after_current_leaves_the_played_prefix_alone() {
        let mut l = album(8);
        l.set_current(3);
        let prefix = positions(&l)[..=3].to_vec();
        assert!(l.shuffle_after_current(7));
        assert_eq!(l.current_index(), 3);
        assert_eq!(positions(&l)[..=3], prefix, "played prefix untouched");
        let mut tail = positions(&l)[4..].to_vec();
        tail.sort_unstable();
        assert_eq!(tail, vec![4, 5, 6, 7], "the tail is a permutation");

        // Dynamic + shuffle uses this operation (never resets the current index).
        let mut d = album(8);
        d.set_current(3);
        d.mark_dynamic();
        assert_eq!(d.set_order(OrderMode::Shuffle, 7), OrderOutcome::Applied);
        assert_eq!(d.current_index(), 3);
        assert_eq!(positions(&d)[..=3], prefix);
    }

    #[test]
    fn shuffle_after_current_is_idempotent_for_a_single_unplayed_item() {
        let mut l = album(4);
        l.set_current(2);
        let order = positions(&l);
        assert!(!l.shuffle_after_current(1), "one item ahead: nothing to shuffle");
        assert_eq!(positions(&l), order);
        l.set_current(3);
        assert!(!l.shuffle_after_current(1), "nothing ahead at all");
        assert_eq!(l.current_index(), 3);
    }

    #[test]
    fn restore_source_order_keeps_the_current_item_and_reorders_around_it() {
        let mut l = album(6);
        l.set_current(2);
        assert!(l.shuffle_keep_current_first(11));
        assert_eq!(l.tracks()[0].item_position, 2);
        l.set_cues(vec![cue(0, 0, None)]);
        assert!(l.restore_source_order());
        assert_eq!(positions(&l), (0..6u32).collect::<Vec<_>>());
        assert_eq!(
            l.current_index(),
            2,
            "current is recomputed from item_position, not the old index"
        );
        assert_eq!(l.current_track().unwrap().item_position, 2);
        assert_eq!(l.active_cues().count(), 1, "cues are not cleared by a restore");
    }

    #[test]
    fn restore_source_order_is_a_no_op_when_already_sorted() {
        let mut l = album(4);
        l.set_current(1);
        assert!(!l.restore_source_order());
        assert_eq!(l.current_index(), 1);
        assert_eq!(
            l.set_order(OrderMode::Normal, 1),
            OrderOutcome::Unchanged,
            "nothing to restore"
        );
    }

    #[test]
    fn a_dynamic_list_cannot_restore_its_source_order() {
        let mut l = album(6);
        l.set_current(3);
        assert_eq!(l.source_order(), SourceOrder::Fixed);
        l.mark_dynamic();
        assert_eq!(l.source_order(), SourceOrder::Dynamic);
        assert!(l.shuffle_after_current(3));
        let shuffled = positions(&l);
        assert_eq!(l.set_order(OrderMode::Normal, 9), OrderOutcome::NoFixedOrder);
        assert_eq!(positions(&l), shuffled, "no order to go back to: untouched");
        assert_eq!(l.current_index(), 3);
        // Repeating the request never restores anything (there is no source order).
        assert_eq!(l.set_order(OrderMode::Normal, 9), OrderOutcome::NoFixedOrder);
        assert_eq!(positions(&l), shuffled);

        let mut one = album(1);
        one.mark_dynamic();
        assert_eq!(one.set_order(OrderMode::Shuffle, 1), OrderOutcome::TooShort);
    }

    #[test]
    fn start_new_pass_never_puts_the_finished_item_first() {
        for seed in 0..32u64 {
            let mut l = album(6);
            l.set_current(5);
            l.set_cues(vec![CueSpan {
                list_index: 0,
                start_ms: 1_000,
                end_ms: None,
                title: "old".into(),
            }]);
            let finished = l.current_track().unwrap().item_position;
            assert!(l.start_new_pass(seed));
            assert_eq!(l.current_index(), 0);
            assert_ne!(l.tracks()[0].item_position, finished, "seed {seed}");
            assert!(
                l.entries()
                    .iter()
                    .all(|e| e.kind != NowPlayingEntryKind::Cue),
                "the finished item's cues must not follow the new pass"
            );
            let mut all = positions(&l);
            all.sort_unstable();
            assert_eq!(all, (0..6u32).collect::<Vec<_>>(), "a full pass");
        }
        let mut one = album(1);
        assert!(!one.start_new_pass(1), "a one-track queue repeat restarts it");
    }

    #[test]
    fn repeat_item_does_not_wrap_the_queue() {
        let mut l = album(3);
        l.set_repeat(RepeatMode::Item);
        assert_eq!(l.repeat(), RepeatMode::Item);
        l.set_current(2);
        assert_eq!(
            l.next_track_action(),
            NavAction::None,
            "repeat-one is not a queue repeat"
        );
        assert_eq!(l.next(0), NavAction::None);
        assert!(!l.can_next(0));
        assert_eq!(idx(l.previous(0, true)), Some(1), "stepping back still works");
        l.set_current(0);
        assert_eq!(l.previous(0, true), NavAction::SeekMs(0));
        l.set_current(2);
        assert_eq!(l.available_from(2, true), None);
    }

    #[test]
    fn repeat_queue_wraps_next_previous_and_available_from() {
        let mut l = album(3);
        l.set_repeat(RepeatMode::Queue);
        l.set_current(2);
        assert_eq!(idx(l.next_track_action()), Some(0));
        assert!(l.can_next(0));
        l.set_current(0);
        assert_eq!(idx(l.previous_track_action()), Some(2));
        assert_eq!(l.available_from(2, true), Some(0));
        assert_eq!(l.available_from(0, false), Some(2));

        let mut one = album(1);
        one.set_repeat(RepeatMode::Queue);
        assert_eq!(idx(one.next_track_action()), Some(0), "one-track queue wraps");
        assert!(one.can_next(0));
        assert_eq!(one.available_from(0, true), Some(0));
    }

    // ---- PLAY_03: cue segments and cue-level shuffle ----

    /// The published cue rows' starts, in published order (the play order while shuffling).
    fn cue_row_starts(l: &NowPlayingList) -> Vec<u64> {
        l.entries()
            .into_iter()
            .filter(|e| e.kind == NowPlayingEntryKind::Cue)
            .map(|e| e.start_ms)
            .collect()
    }

    #[test]
    fn cue_segments_tile_the_file_exactly_once() {
        // 3 cues at 0/30/60 s over a 90 s file, each with an explicit `endTime` that would
        // leave a gap. Tiling ignores those ends (owner decision 3).
        let mut l = NowPlayingList::new(NowPlayingListKind::Mix, vec![track("m")]);
        l.set_cues(vec![
            cue(0, 0, Some(10_000)),
            cue(0, 30_000, Some(40_000)),
            cue(0, 60_000, None),
        ]);
        assert_eq!(l.segment_at(0), Some(0));
        assert_eq!(l.segment_at(20_000), Some(0), "an endTime gap belongs to segment 0");
        assert_eq!(l.segment_at(29_999), Some(0));
        assert_eq!(l.segment_at(30_000), Some(1));
        assert_eq!(l.segment_at(60_000), Some(2));
        assert_eq!(l.current_segment_start_ms(0), Some(0));
        assert_eq!(l.current_segment_start_ms(30_000), Some(30_000));
        assert_eq!(l.current_segment_start_ms(59_999), Some(30_000));
        assert_eq!(l.current_segment_end_ms(0), Some(30_000));
        assert_eq!(l.current_segment_end_ms(29_999), Some(30_000));
        assert_eq!(l.current_segment_end_ms(30_000), Some(60_000));
        assert_eq!(l.current_segment_end_ms(60_000), None, "the last segment runs to EOF");
        assert_eq!(l.current_segment_end_ms(89_999), None);

        // Every millisecond of the 90 s file is owned by exactly one segment, and the three
        // segments are the same size (30 s each): no gap, no double-owned millisecond.
        let mut owned = vec![0u32; 3];
        for ms in 0..90_000u64 {
            let seg = l.segment_at(ms).expect("every millisecond is in a segment");
            owned[seg] += 1;
        }
        assert_eq!(
            owned,
            vec![30_000, 30_000, 30_000],
            "no gap, no double-owned millisecond"
        );

        // Shuffle off keeps the pre-PLAY_03 gap behaviour; tiling is what shuffle *changes*.
        assert_eq!(l.current_cue_index(20_000), None, "shuffle off: an endTime gap is None");
        l.set_cue_shuffle(true, 1);
        assert!(l.current_cue_index(20_000).is_some(), "shuffle on: tiled containment");
    }

    #[test]
    fn cue_shuffle_is_a_permutation_and_keeps_the_current_segment_first() {
        let mut l = mixtape();
        // The playhead is in the 120..180 s segment.
        l.set_cue_anchor(130_000);
        l.set_cue_shuffle(true, 42);
        assert!(l.cue_shuffle());
        assert_eq!(
            l.current_cue_index(130_000),
            Some(0),
            "the segment being heard stays at permutation position 0"
        );
        let order = cue_row_starts(&l);
        assert_eq!(order.len(), 5, "one row per segment");
        assert_eq!(order[0], 120_000);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![0, 60_000, 120_000, 180_000, 240_000],
            "each segment exactly once"
        );

        // Deterministic in the seed...
        let mut same = mixtape();
        same.set_cue_anchor(130_000);
        same.set_cue_shuffle(true, 42);
        assert_eq!(cue_row_starts(&same), order);
        // ...and a new seed reshuffles.
        let mut other = mixtape();
        other.set_cue_anchor(130_000);
        other.set_cue_shuffle(true, 43);
        assert_ne!(cue_row_starts(&other), order);

        // Shuffle off puts the rows back in chronological order.
        other.set_cue_shuffle(false, 43);
        assert!(!other.cue_shuffle());
        assert_eq!(cue_row_starts(&other), vec![0, 60_000, 120_000, 180_000, 240_000]);
    }

    #[test]
    fn cue_shuffle_applies_to_mixes_and_lone_files_only() {
        for (kind, want) in [
            (NowPlayingListKind::Mix, true),
            (NowPlayingListKind::Single, true),
            (NowPlayingListKind::Album, false),
            (NowPlayingListKind::Playlist, false),
            (NowPlayingListKind::Audiobook, false),
            (NowPlayingListKind::Podcast, false),
        ] {
            let mut l = NowPlayingList::new(kind, vec![track("x")]);
            l.set_cues(vec![cue(0, 0, None), cue(0, 10_000, None)]);
            assert_eq!(l.cue_shuffle_applies(), want, "{kind:?}");
            l.set_cue_shuffle(true, 7);
            assert_eq!(l.cue_shuffle(), want, "{kind:?} (applied)");
            // A list that cannot shuffle keeps its cues chronological.
            assert_eq!(cue_row_starts(&l), vec![0, 10_000], "{kind:?}");
        }
    }

    #[test]
    fn a_one_cue_mix_does_not_shuffle() {
        let mut l = NowPlayingList::new(NowPlayingListKind::Mix, vec![track("m")]);
        l.set_cues(vec![cue(0, 20_000, None)]);
        assert!(!l.cue_shuffle_applies(), "a one-cue mix has nothing to shuffle");
        l.set_cue_shuffle(true, 3);
        assert!(!l.cue_shuffle(), "the setting does not apply");
        assert_eq!(cue_row_starts(&l), vec![20_000]);
        assert_eq!(l.next(0), NavAction::SeekMs(20_000), "chronological as before");
        assert_eq!(l.current_cue_index(20_000), Some(0));
    }

    #[test]
    fn next_follows_the_permutation_not_the_clock() {
        // First cue at 10 s so "before the first start" is reachable.
        let mut l = NowPlayingList::new(NowPlayingListKind::Mix, vec![track("m")]);
        l.set_cues(vec![
            cue(0, 10_000, None),
            cue(0, 20_000, None),
            cue(0, 30_000, None),
        ]);
        l.set_cue_shuffle(true, 7);
        let order = cue_row_starts(&l);
        assert_eq!(
            l.next(2_000),
            NavAction::SeekMs(order[0]),
            "before the first start: the pass's first entry"
        );
        for p in 0..order.len() - 1 {
            // At the segment's own start, `Next` is the permutation's next entry — not the
            // next chronological one.
            assert_eq!(
                l.next(order[p]),
                NavAction::SeekMs(order[p + 1]),
                "permutation position {p}"
            );
            assert_eq!(l.next(order[p] + 5_000), NavAction::SeekMs(order[p + 1]));
        }
        assert_eq!(
            l.next(*order.last().unwrap()),
            NavAction::None,
            "after the last entry: next track (none here)"
        );

        // Chronological order really would have picked something else for at least one entry,
        // so the assertions above are not vacuous.
        assert!(order.windows(2).any(|w| w[1] != w[0] + 10_000));
    }

    #[test]
    fn previous_restarts_the_segment_it_is_inside_of() {
        let mut l = mixtape();
        l.set_cue_shuffle(true, 42);
        let order = cue_row_starts(&l);
        let seg = order[2];
        assert_eq!(
            l.previous(seg + 1_000, false),
            NavAction::SeekMs(order[1]),
            "within 1 s of *this* segment's start: the permutation predecessor"
        );
        assert_eq!(
            l.previous(seg + 1_001, false),
            NavAction::SeekMs(seg),
            "past the threshold: restart the heard segment, not a chronological one"
        );
        assert_eq!(l.previous(seg + 5_000, true), NavAction::SeekMs(order[1]));
        // First entry of the pass, single track, repeat off: restart.
        assert_eq!(l.previous(order[0] + 5_000, true), NavAction::SeekMs(0));
    }

    #[test]
    fn a_manual_seek_re_anchors_the_current_segment() {
        let mut l = mixtape();
        l.set_cue_shuffle(true, 42);
        let order = cue_row_starts(&l);
        let p60 = order.iter().position(|s| *s == 60_000).unwrap();
        assert_eq!(l.current_cue_index(62_000), Some(p60), "the seeked segment is current");
        match order.get(p60 + 1) {
            Some(&next) => assert_eq!(l.next(62_000), NavAction::SeekMs(next)),
            None => assert_eq!(l.next(62_000), NavAction::None),
        }
        // The last entry of the pass has no successor inside the file.
        let last = *order.last().unwrap();
        assert_eq!(l.current_cue_index(last + 2_000), Some(order.len() - 1));
        assert_eq!(l.next(last + 2_000), NavAction::None);
        assert_eq!(
            l.next(order[order.len() - 2] + 2_000),
            NavAction::SeekMs(last),
            "Next from the second-to-last entry is the last one"
        );
    }

    #[test]
    fn disabling_a_cue_list_rebuilds_the_segments_and_keeps_the_anchor() {
        let mut l = NowPlayingList::new(NowPlayingListKind::Mix, vec![track("m")]);
        l.set_cues(vec![
            cue(0, 0, None),
            cue(0, 30_000, None),
            cue(1, 15_000, None),
            cue(1, 45_000, None),
        ]);
        l.set_cue_anchor(30_000); // the segment being heard
        l.set_cue_shuffle(true, 11);
        assert_eq!(cue_row_starts(&l)[0], 30_000, "the heard segment is first");
        assert_eq!(l.current_segment_end_ms(30_000), Some(45_000));

        l.set_cue_list_enabled(1, false);
        let rows = cue_row_starts(&l);
        assert_eq!(rows.len(), 2, "one row per segment after the edit");
        assert_eq!(rows[0], 30_000, "the anchor survives the rebuild");
        let mut sorted = rows.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 30_000]);
        assert_eq!(l.current_segment_end_ms(15_000), Some(30_000), "new tiling");
        assert_eq!(l.current_segment_end_ms(30_000), None, "now the last segment: to EOF");
        assert_eq!(l.current_cue_index(30_000), Some(0));

        // Turning the list back on restores the old segments, still anchored.
        l.set_cue_list_enabled(1, true);
        assert_eq!(cue_row_starts(&l)[0], 30_000);
        assert_eq!(cue_row_starts(&l).len(), 4);
    }

    #[test]
    fn jump_to_entry_targets_the_permuted_cue() {
        let mut l = mixtape();
        l.set_cue_shuffle(true, 42);
        let order = cue_row_starts(&l);
        for (row, &start) in order.iter().enumerate() {
            assert_eq!(
                l.jump_to_entry(1 + row),
                NavAction::SeekMs(start),
                "cue row {row} plays segment {start}"
            );
        }
        assert_eq!(l.jump_to_entry(0), NavAction::SeekMs(0), "the current track restarts");
        assert_eq!(l.jump_to_entry(1 + order.len()), NavAction::None);
    }
}
