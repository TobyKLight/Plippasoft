//! `SourceWindow` (WARP_04): a byte-bounded ring of decoded frames indexed by **absolute
//! source frame**, with history behind the playhead and lookahead in front of it.
//!
//! The audio loop reads through the window instead of a `VecDeque` of decoded chunks. Frames
//! stay readable (forward and backward) until they are older than the history budget, so a
//! seek inside the window needs no decoder seek, a pause can put back what the sink flush
//! discards, and later packages can read backwards (reverse, varispeed) from the same store.
//!
//! The window never holds a whole track: its size is capped in **bytes**
//! ([`WindowConfig::window_cap_bytes`], 6 MiB by default) and the ring is preallocated once, so
//! pushing and reading never allocate.

/// Window cap in bytes (hard memory cap, not tuned by the eval app). README section 7.
pub const WINDOW_CAP_BYTES: usize = 6 * 1024 * 1024;
/// Seconds of history kept behind the playhead (at 44.1 kHz stereo; see [`WindowConfig`]).
pub const HISTORY_S: f64 = 8.0;
/// Seconds of lookahead decoded ahead of the playhead (at 44.1 kHz stereo).
pub const LOOKAHEAD_S: f64 = 9.0;
/// History never drops below this many seconds, whatever the byte budget share says.
pub const MIN_HISTORY_S: f64 = 2.0;
/// Length of one back-window block the decode worker places before the window (WARP_11).
pub const BACK_WINDOW_S: f64 = 2.0;
/// While reversing, the worker keeps at least this many seconds decoded behind the playhead.
pub const BACK_LEAD_S: f64 = 4.0;
/// MP3 frame-index stride: one `(sample_ts, byte_offset)` entry per this many MPEG frames.
pub const MP3_INDEX_STRIDE_FRAMES: u32 = 100;
/// Runtime clamp: the maximum source rate is the measured decode speed times this factor.
pub const DECODE_SAFETY_FACTOR: f64 = 0.5;
/// Frames decoded and dropped ahead of a back-window / indexed seek so the codec is warm
/// (about two MPEG-1 Layer III frames).
pub const BACK_REFERENCE_FRAMES: usize = 2304;
/// History the worker guarantees before a chase landing frame (stretcher block + interval,
/// with margin), in seconds.
pub const LANDING_HISTORY_S: f64 = 0.5;
/// Seconds decoded after a far-jump target into the waypoint window.
pub const WAYPOINT_LOOKAHEAD_S: f64 = 3.0;
/// WARP_19: with "Seek outside track" on, a scan opens the neighbouring track once the
/// boundary is within this many seconds **at the current rate** (the rate is floored at 1x).
pub const CROSS_PREWARM_S: f64 = 10.0;

/// Window sizing: single place for the tuning values (README section 7, `WindowConfig`).
///
/// History and lookahead are both bounded by **bytes**: at 44.1 kHz stereo f32 the default
/// cap holds about 17.8 s (8 s history + 9 s lookahead fit); at 192 kHz stereo it holds about
/// 4.1 s, so history becomes `max(2 s, share of the budget)` and lookahead gets the rest.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowConfig {
    /// Hard cap on the bytes the window may hold (`WINDOW_CAP_BYTES`).
    pub window_cap_bytes: usize,
    /// Wanted history in seconds (`HISTORY_S`); shrinks to its budget share when bytes are short.
    pub history_s: f64,
    /// Wanted lookahead in seconds (`LOOKAHEAD_S`); limited by what the cap leaves after history.
    pub lookahead_s: f64,
    /// Floor for history in seconds (`MIN_HISTORY_S`).
    pub min_history_s: f64,
    /// Back-window block length in seconds (`BACK_WINDOW_S`, WARP_11).
    pub back_window_s: f64,
    /// Seconds the worker keeps decoded behind the playhead while reversing (`BACK_LEAD_S`).
    pub back_lead_s: f64,
    /// MP3 index stride in MPEG frames (`MP3_INDEX_STRIDE_FRAMES`).
    pub mp3_index_stride_frames: u32,
    /// Factor applied to the measured decode speed (`DECODE_SAFETY_FACTOR`).
    pub decode_safety_factor: f64,
    /// Warm-up frames decoded and dropped before a back-window block (`BACK_REFERENCE_FRAMES`).
    pub back_reference_frames: usize,
    /// History guaranteed before a chase landing, seconds (`LANDING_HISTORY_S`).
    pub landing_history_s: f64,
    /// Lookahead decoded after a far target into the waypoint window, seconds.
    pub waypoint_lookahead_s: f64,
    /// Seconds before a track boundary (at the current rate) the neighbour is opened and its
    /// edge window filled (`CROSS_PREWARM_S`, WARP_19).
    pub cross_prewarm_s: f64,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            window_cap_bytes: WINDOW_CAP_BYTES,
            history_s: HISTORY_S,
            lookahead_s: LOOKAHEAD_S,
            min_history_s: MIN_HISTORY_S,
            back_window_s: BACK_WINDOW_S,
            back_lead_s: BACK_LEAD_S,
            mp3_index_stride_frames: MP3_INDEX_STRIDE_FRAMES,
            decode_safety_factor: DECODE_SAFETY_FACTOR,
            back_reference_frames: BACK_REFERENCE_FRAMES,
            landing_history_s: LANDING_HISTORY_S,
            waypoint_lookahead_s: WAYPOINT_LOOKAHEAD_S,
            cross_prewarm_s: CROSS_PREWARM_S,
        }
    }
}

impl WindowConfig {
    /// The same sizing with the byte cap divided by `parts` (two-voice transitions split the
    /// cap 3 + 3 MiB between two windows).
    pub fn split(&self, parts: usize) -> Self {
        Self {
            window_cap_bytes: self.window_cap_bytes / parts.max(1),
            ..self.clone()
        }
    }

    /// Whole frames the cap allows for `channels` interleaved f32 channels.
    pub fn cap_frames(&self, channels: usize) -> usize {
        self.window_cap_bytes / (4 * channels.max(1))
    }

    /// History in frames: `min(history_s, budget share)` but at least `min_history_s`, and
    /// never more than three quarters of the cap (so lookahead always has room).
    pub fn history_frames(&self, channels: usize, rate_hz: u32) -> usize {
        let cap = self.cap_frames(channels) as f64;
        let rate = f64::from(rate_hz);
        let share = cap * self.history_s / (self.history_s + self.lookahead_s).max(1e-9);
        let want = (self.history_s * rate)
            .min(share)
            .max(self.min_history_s * rate);
        (want.min(cap * 0.75)) as usize
    }

    /// Back-window block length in frames.
    pub fn back_window_frames(&self, rate_hz: u32) -> usize {
        ((self.back_window_s * f64::from(rate_hz)) as usize).max(1)
    }

    /// Frames the worker keeps decoded behind the playhead while reversing (never more than
    /// half the cap).
    pub fn back_lead_frames(&self, channels: usize, rate_hz: u32) -> usize {
        let half = self.cap_frames(channels) / 2;
        ((self.back_lead_s * f64::from(rate_hz)) as usize).min(half)
    }

    /// History guaranteed before a chase landing, in frames.
    pub fn landing_history_frames(&self, rate_hz: u32) -> usize {
        (self.landing_history_s * f64::from(rate_hz)) as usize
    }

    /// Lookahead in frames: `lookahead_s`, limited to what the cap leaves after history.
    pub fn lookahead_frames(&self, channels: usize, rate_hz: u32) -> usize {
        let cap = self.cap_frames(channels);
        let hist = self.history_frames(channels, rate_hz);
        ((self.lookahead_s * f64::from(rate_hz)) as usize).min(cap.saturating_sub(hist))
    }
}

/// Ring of decoded interleaved f32 frames, `[start_frame, end_frame)` in absolute source frames.
#[derive(Debug)]
pub struct SourceWindow {
    channels: usize,
    rate_hz: u32,
    /// Ring storage, `cap_frames * channels` samples, allocated once.
    buf: Vec<f32>,
    cap_frames: usize,
    history_frames: usize,
    lookahead_frames: usize,
    /// Absolute frame of the oldest held frame.
    start: u64,
    /// Physical frame slot of `start`.
    head: usize,
    /// Frames held.
    len: usize,
    peak_bytes: usize,
    /// Oldest frame the history trim must keep (a chase hint's landing zone, WARP_20); the
    /// byte cap still wins. `None` = plain history budget.
    pin: Option<u64>,
}

impl SourceWindow {
    /// Allocate the ring for `channels` at `rate_hz` according to `cfg`.
    pub fn new(channels: usize, rate_hz: u32, cfg: &WindowConfig) -> Self {
        let channels = channels.max(1);
        let cap_frames = cfg.cap_frames(channels).max(1);
        let history_frames = cfg.history_frames(channels, rate_hz).min(cap_frames);
        let lookahead_frames = cfg.lookahead_frames(channels, rate_hz);
        Self {
            channels,
            rate_hz,
            buf: vec![0.0; cap_frames * channels],
            cap_frames,
            history_frames,
            lookahead_frames,
            start: 0,
            head: 0,
            len: 0,
            peak_bytes: 0,
            pin: None,
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn sample_rate_hz(&self) -> u32 {
        self.rate_hz
    }

    pub fn cap_frames(&self) -> usize {
        self.cap_frames
    }

    /// Hard byte cap of this window (what the ring was allocated for).
    pub fn cap_bytes(&self) -> usize {
        self.cap_frames * self.channels * 4
    }

    pub fn history_frames(&self) -> usize {
        self.history_frames
    }

    pub fn lookahead_frames(&self) -> usize {
        self.lookahead_frames
    }

    /// Absolute frame of the oldest held frame.
    pub fn start_frame(&self) -> u64 {
        self.start
    }

    /// Absolute frame one past the newest held frame (where the next decoded frame goes).
    pub fn end_frame(&self) -> u64 {
        self.start + self.len as u64
    }

    pub fn len_frames(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes currently held.
    pub fn bytes(&self) -> usize {
        self.len * self.channels * 4
    }

    /// Most bytes ever held since construction (never exceeds [`cap_bytes`](Self::cap_bytes)).
    pub fn peak_bytes(&self) -> usize {
        self.peak_bytes
    }

    /// True if the frame is held.
    pub fn contains(&self, frame: u64) -> bool {
        frame >= self.start && frame < self.end_frame()
    }

    /// True if all of `[frame, frame + frames)` is held.
    pub fn contains_range(&self, frame: u64, frames: usize) -> bool {
        frames == 0 || (frame >= self.start && frame + frames as u64 <= self.end_frame())
    }

    /// Frames held at or after `playhead` (lookahead).
    pub fn ahead(&self, playhead: u64) -> usize {
        self.end_frame().saturating_sub(playhead.max(self.start)) as usize
    }

    /// Frames held before `playhead` (history), at most [`history_frames`](Self::history_frames)
    /// after the next push.
    pub fn behind(&self, playhead: u64) -> usize {
        playhead
            .clamp(self.start, self.end_frame())
            .saturating_sub(self.start) as usize
    }

    /// How many more frames may be pushed while keeping lookahead within its budget.
    pub fn lookahead_room(&self, playhead: u64) -> usize {
        self.lookahead_frames.saturating_sub(self.ahead(playhead))
    }

    /// Empty the window and make `frame` the next frame a push will hold.
    pub fn reset(&mut self, frame: u64) {
        self.pin = None;
        self.start = frame;
        self.head = 0;
        self.len = 0;
    }

    /// Keep frames from `pin` on when trimming history (the byte cap still wins); `None`
    /// restores the plain history budget. Without this a landing zone further behind the
    /// playhead than the history budget is evicted again while the playhead keeps moving.
    pub fn set_pin(&mut self, pin: Option<u64>) {
        self.pin = pin;
    }

    pub fn rate_hz(&self) -> u32 {
        self.rate_hz
    }

    /// Append whole frames at the end, evicting history older than the budget behind
    /// `playhead` (the oldest frames go first; nothing at or after `playhead` is ever evicted).
    ///
    /// Returns false (nothing changed) if the samples are not a whole number of frames or
    /// would not fit without evicting unread frames.
    pub fn push(&mut self, samples: &[f32], playhead: u64) -> bool {
        if samples.len() % self.channels != 0 {
            return false;
        }
        let n = samples.len() / self.channels;
        if n == 0 {
            return true;
        }
        if n > self.cap_frames {
            return false;
        }
        let playhead = playhead.clamp(self.start, self.end_frame());
        let end_after = self.end_frame() + n as u64;
        let mut keep_history = playhead.saturating_sub(self.history_frames as u64);
        if let Some(p) = self.pin {
            keep_history = keep_history.min(p);
        }
        let keep_cap = end_after.saturating_sub(self.cap_frames as u64);
        let new_start = keep_history.max(keep_cap);
        if new_start > playhead {
            return false;
        }
        if new_start > self.start {
            self.evict((new_start - self.start) as usize);
        }
        // Copy in at most two contiguous segments.
        let tail = (self.head + self.len) % self.cap_frames;
        let first = n.min(self.cap_frames - tail);
        let ch = self.channels;
        self.buf[tail * ch..(tail + first) * ch].copy_from_slice(&samples[..first * ch]);
        if first < n {
            self.buf[..(n - first) * ch].copy_from_slice(&samples[first * ch..]);
        }
        self.len += n;
        debug_assert!(self.len <= self.cap_frames);
        self.peak_bytes = self.peak_bytes.max(self.bytes());
        true
    }

    /// Place whole frames **before** the window (`[start - n, start)`), the back-window
    /// counterpart of [`push`](Self::push). The newest frames go first: the tail beyond
    /// `playhead + history` and whatever the byte cap requires is evicted, but nothing at or
    /// before `playhead` is (it is unread in reverse).
    ///
    /// Returns false (nothing changed) if the samples are not whole frames, the window is
    /// empty, there is no room before frame 0, or the cap cannot be met without evicting the
    /// playhead.
    pub fn prepend(&mut self, samples: &[f32], playhead: u64) -> bool {
        if samples.len() % self.channels != 0 || self.len == 0 {
            return false;
        }
        let n = samples.len() / self.channels;
        if n == 0 {
            return true;
        }
        if n > self.cap_frames || (n as u64) > self.start {
            return false;
        }
        let playhead = playhead.clamp(self.start, self.end_frame());
        let new_start = self.start - n as u64;
        let cap_end = new_start + self.cap_frames as u64;
        let keep_end = playhead + self.history_frames as u64;
        let new_end = self.end_frame().min(cap_end).min(keep_end.max(playhead + 1));
        // Frames before the playhead are unread in reverse and are never evicted; with
        // nothing held ahead (`playhead == end`) the whole tail may go.
        if new_end < playhead || new_end <= new_start {
            return false;
        }
        let drop_tail = (self.end_frame() - new_end.min(self.end_frame())) as usize;
        self.len -= drop_tail.min(self.len);
        self.head = (self.head + self.cap_frames - n) % self.cap_frames;
        self.start = new_start;
        let first = n.min(self.cap_frames - self.head);
        let ch = self.channels;
        self.buf[self.head * ch..(self.head + first) * ch].copy_from_slice(&samples[..first * ch]);
        if first < n {
            self.buf[..(n - first) * ch].copy_from_slice(&samples[first * ch..]);
        }
        self.len += n;
        debug_assert!(self.len <= self.cap_frames);
        self.peak_bytes = self.peak_bytes.max(self.bytes());
        true
    }

    /// Drop every frame at and after `end` (WARP_19: a neighbour's frames that were appended
    /// behind the end of this track for a seamless crossing are taken off again). A no-op when
    /// the window ends at or before `end`; frames before the window start empty it.
    pub fn truncate_end(&mut self, end: u64) {
        if end >= self.end_frame() {
            return;
        }
        let keep = end.saturating_sub(self.start) as usize;
        self.len = keep;
        if self.len == 0 {
            self.head = 0;
        }
    }

    fn evict(&mut self, frames: usize) {
        let d = frames.min(self.len);
        self.head = (self.head + d) % self.cap_frames;
        self.start += d as u64;
        self.len -= d;
        if self.len == 0 {
            self.head = 0;
        }
    }

    /// One frame (`channels` samples), if held.
    pub fn frame(&self, frame: u64) -> Option<&[f32]> {
        if !self.contains(frame) {
            return None;
        }
        let slot = (self.head + (frame - self.start) as usize) % self.cap_frames;
        Some(&self.buf[slot * self.channels..(slot + 1) * self.channels])
    }

    /// Copy frames `frame, frame+1, ...` into `out` (whole frames); returns the frames copied
    /// (fewer than asked if the window ends first, 0 if `frame` is not held).
    pub fn read_forward(&self, frame: u64, out: &mut [f32]) -> usize {
        if !self.contains(frame) {
            return 0;
        }
        let ch = self.channels;
        let want = out.len() / ch;
        let avail = (self.end_frame() - frame) as usize;
        let n = want.min(avail);
        let slot = (self.head + (frame - self.start) as usize) % self.cap_frames;
        let first = n.min(self.cap_frames - slot);
        out[..first * ch].copy_from_slice(&self.buf[slot * ch..(slot + first) * ch]);
        if first < n {
            out[first * ch..n * ch].copy_from_slice(&self.buf[..(n - first) * ch]);
        }
        n
    }

    /// Fill `out` (whole frames) with the frames `first, first+1, ...`; frames the window does
    /// not hold (before its start, after its end, or before frame 0 of the track) read as
    /// silence. Used by the rate stage to gather the contiguous span a sinc block needs.
    pub fn read_span(&self, first: i64, out: &mut [f32]) {
        let ch = self.channels;
        let n = out.len() / ch;
        out.fill(0.0);
        if n == 0 || self.len == 0 {
            return;
        }
        let (start, end) = (self.start as i64, self.end_frame() as i64);
        let lo = first.max(start);
        let hi = (first + n as i64).min(end);
        if hi <= lo {
            return;
        }
        let skip = (lo - first) as usize;
        let got = self.read_forward(lo as u64, &mut out[skip * ch..(skip + (hi - lo) as usize) * ch]);
        debug_assert_eq!(got as i64, hi - lo);
    }

    /// Copy frames in **reverse**: `out` frame k is window frame `frame - k` (so the newest
    /// first); returns the frames copied (stops at the oldest held frame, 0 if `frame` is not
    /// held).
    pub fn read_backward(&self, frame: u64, out: &mut [f32]) -> usize {
        if !self.contains(frame) {
            return 0;
        }
        let ch = self.channels;
        let want = out.len() / ch;
        let avail = (frame - self.start) as usize + 1;
        let n = want.min(avail);
        for k in 0..n {
            let slot = (self.head + (frame - k as u64 - self.start) as usize) % self.cap_frames;
            out[k * ch..(k + 1) * ch].copy_from_slice(&self.buf[slot * ch..(slot + 1) * ch]);
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mono window of `cap` frames with `hist` frames of history (tiny sizes for unit tests).
    fn tiny(cap: usize, hist_s: f64, look_s: f64) -> SourceWindow {
        // 1 "second" = 10 frames.
        let cfg = WindowConfig {
            window_cap_bytes: cap * 4,
            history_s: hist_s,
            lookahead_s: look_s,
            min_history_s: 0.0,
            ..WindowConfig::default()
        };
        SourceWindow::new(1, 10, &cfg)
    }

    fn ramp(from: u64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (from + i as u64) as f32).collect()
    }

    #[test]
    fn defaults_match_the_register() {
        let cfg = WindowConfig::default();
        assert_eq!(cfg.window_cap_bytes, 6 * 1024 * 1024);
        assert_eq!((cfg.history_s, cfg.lookahead_s), (8.0, 9.0));
        // 44.1 kHz stereo: 8 s history + 9 s lookahead fit inside the cap.
        let (h, l) = (
            cfg.history_frames(2, 44_100),
            cfg.lookahead_frames(2, 44_100),
        );
        assert_eq!(h, 8 * 44_100);
        assert_eq!(l, 9 * 44_100);
        assert!((h + l) * 8 <= cfg.window_cap_bytes);
        // 192 kHz stereo: history is the 2 s floor, lookahead gets the rest of the 6 MiB.
        let (h, l) = (
            cfg.history_frames(2, 192_000),
            cfg.lookahead_frames(2, 192_000),
        );
        assert_eq!(h, 2 * 192_000);
        assert!(l > 2 * 192_000 && (h + l) * 8 <= cfg.window_cap_bytes);
        // The two-voice split halves the cap and keeps the policy.
        let half = cfg.split(2);
        assert_eq!(half.window_cap_bytes, 3 * 1024 * 1024);
        assert!(
            half.history_frames(2, 44_100) + half.lookahead_frames(2, 44_100) <= half.cap_frames(2)
        );
    }

    #[test]
    fn push_read_forward_and_backward_with_absolute_indices() {
        let mut w = tiny(40, 2.0, 2.0); // 40 frames: 20 history + 20 lookahead
        w.reset(1000);
        assert!(w.push(&ramp(1000, 15), 1000));
        assert_eq!(
            (w.start_frame(), w.end_frame(), w.len_frames()),
            (1000, 1015, 15)
        );
        assert!(w.contains(1000) && w.contains(1014) && !w.contains(1015) && !w.contains(999));
        assert!(w.contains_range(1010, 5) && !w.contains_range(1010, 6));
        let mut out = [0.0f32; 4];
        assert_eq!(w.read_forward(1003, &mut out), 4);
        assert_eq!(out, [1003.0, 1004.0, 1005.0, 1006.0]);
        assert_eq!(w.read_forward(1013, &mut out), 2, "stops at the end");
        assert_eq!(&out[..2], &[1013.0, 1014.0]);
        assert_eq!(w.read_forward(1015, &mut out), 0, "not held");
        assert_eq!(w.read_backward(1005, &mut out), 4);
        assert_eq!(out, [1005.0, 1004.0, 1003.0, 1002.0]);
        assert_eq!(w.read_backward(1001, &mut out), 2, "stops at the oldest");
        assert_eq!(&out[..2], &[1001.0, 1000.0]);
        assert_eq!(w.frame(1007), Some(&[1007.0][..]));
        assert_eq!(w.frame(2000), None);
        assert_eq!((w.ahead(1010), w.behind(1010)), (5, 10));
        assert_eq!(w.lookahead_room(1010), 20 - 5);
    }

    #[test]
    fn wraps_around_the_ring_and_evicts_history_oldest_first() {
        let mut w = tiny(40, 1.0, 3.0); // history 10, lookahead 30
        assert_eq!((w.history_frames(), w.lookahead_frames()), (10, 30));
        w.reset(0);
        let mut pos = 0u64;
        let mut next = 0u64;
        // Stream 400 frames in 7-frame pushes, playhead trailing the end by 9 frames.
        while next < 400 {
            assert!(w.push(&ramp(next, 7), pos), "push at {next}");
            next += 7;
            pos = next.saturating_sub(9);
            assert!(w.len_frames() <= 40);
            assert_eq!(w.end_frame(), next);
            assert!(w.start_frame() + 17 >= pos, "history kept");
            // Every held frame reads back as its own absolute index (wrap included).
            let mut out = vec![0.0f32; w.len_frames()];
            assert_eq!(w.read_forward(w.start_frame(), &mut out), w.len_frames());
            for (k, v) in out.iter().enumerate() {
                assert_eq!(*v as u64, w.start_frame() + k as u64);
            }
        }
        // History never exceeds its budget once the playhead has moved on.
        assert!(w.behind(pos) <= 10 + 7);
        let mut back = [0.0f32; 5];
        assert_eq!(w.read_backward(pos, &mut back), 5);
        assert_eq!(back[0] as u64, pos);
        assert_eq!(back[4] as u64, pos - 4);
        assert!(w.peak_bytes() <= w.cap_bytes());
    }

    #[test]
    fn a_pin_keeps_history_past_the_budget_but_not_past_the_cap() {
        let mut w = tiny(40, 1.0, 3.0); // history 10, cap 40
        w.reset(0);
        w.set_pin(Some(5));
        let (mut next, mut pos) = (0u64, 0u64);
        while next < 30 {
            assert!(w.push(&ramp(next, 5), pos));
            next += 5;
            pos = next.saturating_sub(2);
        }
        assert_eq!(w.start_frame(), 5, "history 10 would have dropped frames < {}", pos - 10);
        while next < 80 {
            assert!(w.push(&ramp(next, 5), pos));
            next += 5;
            pos = next.saturating_sub(2);
        }
        assert!(w.len_frames() <= 40 && w.start_frame() >= next - 40, "the cap wins");
        w.set_pin(None);
        assert!(w.push(&ramp(next, 5), next));
        assert!(w.start_frame() + 10 >= next, "plain budget again");
    }

    #[test]
    fn push_never_evicts_unread_frames() {
        let mut w = tiny(20, 1.0, 1.0); // cap 20
        w.reset(0);
        assert!(w.push(&ramp(0, 20), 0));
        // Full of unread lookahead: a further push must refuse, not drop frame 0.
        assert!(!w.push(&ramp(20, 1), 0));
        assert_eq!((w.start_frame(), w.len_frames()), (0, 20));
        // With the playhead advanced by 5 the oldest 5 may go, then the push fits.
        assert!(w.push(&ramp(20, 5), 5));
        assert_eq!((w.start_frame(), w.end_frame()), (5, 25));
        assert!(!w.push(&ramp(0, 21), 25), "bigger than the cap");
    }

    #[test]
    fn stereo_frames_stay_interleaved_across_the_wrap() {
        let cfg = WindowConfig {
            window_cap_bytes: 8 * 2 * 4, // 8 stereo frames
            history_s: 1.0,
            lookahead_s: 1.0,
            min_history_s: 0.0,
            ..WindowConfig::default()
        };
        let mut w = SourceWindow::new(2, 4, &cfg);
        w.reset(0);
        let mut n = 0u64;
        for _ in 0..10 {
            let block: Vec<f32> = (0..3)
                .flat_map(|k| [(n + k) as f32, -((n + k) as f32)])
                .collect();
            assert!(w.push(&block, n.saturating_sub(1)));
            n += 3;
        }
        for f in w.start_frame()..w.end_frame() {
            let fr = w.frame(f).unwrap();
            assert_eq!((fr[0], fr[1]), (f as f32, -(f as f32)));
        }
        let mut rev = [0.0f32; 6];
        assert_eq!(w.read_backward(w.end_frame() - 1, &mut rev), 3);
        assert_eq!(rev[0] as u64, w.end_frame() - 1);
        assert_eq!(rev[1], -rev[0]);
        // Not a whole number of frames: refused.
        assert!(!w.push(&[0.0; 3], w.end_frame()));
    }

    #[test]
    fn read_span_pads_with_silence_outside_the_window() {
        let mut w = tiny(40, 2.0, 2.0);
        w.reset(10);
        assert!(w.push(&ramp(10, 8), 10)); // holds frames 10..18
        let mut out = [9.0f32; 12];
        w.read_span(7, &mut out); // frames 7..19
        assert_eq!(out, [0.0, 0.0, 0.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0, 0.0]);
        let mut out = [9.0f32; 3];
        w.read_span(-5, &mut out);
        assert_eq!(out, [0.0; 3]);
        w.read_span(16, &mut out);
        assert_eq!(out, [16.0, 17.0, 0.0]);
    }

    #[test]
    fn reset_moves_the_window_to_a_new_position() {
        let mut w = tiny(40, 2.0, 2.0);
        w.reset(0);
        assert!(w.push(&ramp(0, 30), 0));
        w.reset(5000);
        assert!(w.is_empty());
        assert!(!w.contains(0) && !w.contains(5000));
        assert!(w.push(&ramp(5000, 10), 5000));
        assert_eq!((w.start_frame(), w.end_frame()), (5000, 5010));
        assert_eq!(w.frame(5004), Some(&[5004.0][..]));
    }

    #[test]
    fn prepend_places_frames_before_the_window_in_order() {
        let mut w = tiny(30, 1.0, 2.0); // history 10 frames, cap 30
        w.reset(100);
        assert!(w.push(&ramp(100, 20), 100));
        assert!(w.prepend(&ramp(90, 10), 100));
        // The tail beyond playhead + history (110) is not needed in reverse and goes.
        assert_eq!((w.start_frame(), w.end_frame()), (90, 110));
        let mut out = vec![0.0; 20];
        assert_eq!(w.read_forward(90, &mut out), 20);
        assert_eq!(out, ramp(90, 20));
        // More frames before: the cap (30) is not reached yet.
        assert!(w.prepend(&ramp(80, 10), 100));
        assert_eq!((w.start_frame(), w.end_frame()), (80, 110));
        let mut out = vec![0.0; 30];
        assert_eq!(w.read_forward(80, &mut out), 30);
        assert_eq!(out, ramp(80, 30));
        assert!(w.bytes() <= w.cap_bytes());
    }

    #[test]
    fn prepend_wraps_never_evicts_the_playhead_and_clamps_at_frame_zero() {
        let mut w = tiny(20, 1.0, 2.0);
        w.reset(5);
        assert!(w.push(&ramp(5, 15), 5));
        assert!(!w.prepend(&ramp(0, 6), 5), "would go below frame 0");
        assert!(w.prepend(&ramp(0, 5), 5));
        assert_eq!(w.start_frame(), 0);
        assert!(!w.prepend(&ramp(0, 1), 5), "nothing before frame 0");
        // Many back-and-forth operations keep the ring consistent across the wrap.
        let mut w = tiny(16, 0.4, 1.0);
        w.reset(200);
        assert!(w.push(&ramp(200, 8), 200));
        let mut start = 200u64;
        for _ in 0..30 {
            let blk = ramp(start - 4, 4);
            assert!(w.prepend(&blk, start), "start {start}");
            start -= 4;
            assert_eq!(w.start_frame(), start);
            let mut out = vec![0.0; (w.len_frames()).min(16)];
            let n = w.read_forward(start, &mut out);
            assert_eq!(out[..n], ramp(start, n)[..]);
            assert!(w.contains(start) && w.len_frames() <= 16);
        }
        // Nothing held ahead of the playhead (playhead == end) and room to spare: fits, and
        // nothing is evicted. Cap-limited, the frames just before the playhead stay: refused.
        let mut w = tiny(16, 5.0, 5.0);
        w.reset(50);
        assert!(w.push(&ramp(50, 8), 58));
        assert!(w.prepend(&ramp(46, 4), 58));
        assert_eq!((w.start_frame(), w.end_frame()), (46, 58));
        let mut w = tiny(8, 5.0, 5.0);
        w.reset(50);
        assert!(w.push(&ramp(50, 8), 58));
        assert!(!w.prepend(&ramp(46, 4), 58));
        // The playhead frame is never evicted when the cap would require it.
        let mut w = tiny(8, 5.0, 5.0);
        w.reset(50);
        assert!(w.push(&ramp(50, 8), 57));
        assert!(!w.prepend(&ramp(40, 10), 57), "bigger than the cap");
        assert!(!w.prepend(&ramp(46, 4), 57), "would evict the playhead 57");
        assert!(w.contains(57));
    }
}
