//! Frame-based position arithmetic (WARP_03).
//!
//! Frames at the file's native rate are the unit everywhere inside the engine; milliseconds
//! exist only at the API boundary. Pure functions, no clocks: unit tested as arithmetic.

/// Safety cap (not a tuning value) on how much already-written audio the audio thread keeps
/// so a Pause can put back what the sink flush throws away (`>= SINK_LATENCY_MS` of any
/// plausible sink queue).
pub const PAUSE_REQUEUE_MAX_MS: u64 = 500;

/// Rate in micro-units (1_000_000 = 1.0x, signed) as published by the audio thread.
pub const RATE_MICRO_UNITY: i32 = 1_000_000;

/// Milliseconds to the frame index at `sample_rate_hz` (floor, exact integer arithmetic).
pub fn ms_to_frame(ms: u64, sample_rate_hz: u32) -> u64 {
    ((u128::from(ms) * u128::from(sample_rate_hz)) / 1000) as u64
}

/// Frame index to milliseconds (floor). `0` when the rate is unknown.
pub fn frame_to_ms(frame: u64, sample_rate_hz: u32) -> u64 {
    if sample_rate_hz == 0 {
        return 0;
    }
    ((u128::from(frame) * 1000) / u128::from(sample_rate_hz)) as u64
}

/// Frames the sink queue holds for a given queue duration in ms (rounded to nearest).
pub fn queue_ms_to_frames(queue_ms: u64, sample_rate_hz: u32) -> u64 {
    ((u128::from(queue_ms) * u128::from(sample_rate_hz) + 500) / 1000) as u64
}

/// Heard-position correction: `pos_heard = pos_written - source span of the sink queue`.
///
/// `written` is the source-frame position of the audio most recently handed to the sink
/// (fractional: a rate stage integrates in f64), `queue_src` the **signed source-frame span**
/// of the audio still queued in the sink (positive forward, negative while reversing, where
/// the queued audio lies *ahead* in media time), `floor` the landing frame of the stream (the
/// heard position never precedes where the stream started). Result is a non-negative frame
/// index.
///
/// The span form — rather than `rate x queue` — keeps the reported position exact across a
/// rate change (UI_15: the FF/RW release jumps the rate from the scan speed back to the set
/// speed while up to a sink buffer of *scan-rate* audio is still queued).
pub fn heard_position_src(written: f64, queue_src: f64, floor: u64) -> u64 {
    let heard = written - queue_src;
    if !heard.is_finite() || heard <= floor as f64 {
        return floor;
    }
    heard as u64
}

/// [`heard_position_src`] for a queue that was written at one rate (the simple model, and the
/// fallback when the write history does not reach back far enough).
pub fn heard_position_frames(written: f64, rate: f64, queue_frames: u64, floor: u64) -> u64 {
    heard_position_src(written, rate * queue_frames as f64, floor)
}

/// How many write slices [`WriteSpans`] keeps: at the engine's block sizes this covers far
/// more than the longest sink queue (`PAUSE_REQUEUE_MAX_MS`), and it never allocates on the
/// audio thread.
pub const WRITE_SPANS: usize = 64;

/// Source-frame spans of the most recent sink writes, newest last (WARP_03/UI_15).
///
/// `AudioSink::queue_frames` counts **output** frames still queued, but each output frame
/// consumed a rate-dependent number of source frames. Keeping the exact source span of every
/// written block makes the heard position exact even when the rate changed inside the queue.
#[derive(Debug, Clone)]
pub struct WriteSpans {
    entries: [Span; WRITE_SPANS],
    /// Next slot to overwrite (oldest once full).
    head: usize,
    len: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct Span {
    out_frames: u64,
    src_frames: f64,
}

impl Default for WriteSpans {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteSpans {
    pub fn new() -> Self {
        Self {
            entries: [Span::default(); WRITE_SPANS],
            head: 0,
            len: 0,
        }
    }

    /// The queue is empty (start, seek landing, pause flush): forget the history.
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Record one written block: `out_frames` output frames consumed `src_frames` source
    /// frames (signed: negative while reversing).
    pub fn push(&mut self, out_frames: u64, src_frames: f64) {
        if out_frames == 0 {
            return;
        }
        self.entries[self.head] = Span {
            out_frames,
            src_frames,
        };
        self.head = (self.head + 1) % WRITE_SPANS;
        self.len = (self.len + 1).min(WRITE_SPANS);
    }

    /// Signed source span of the newest `queue_out` output frames, taking a partial slice
    /// proportionally. Frames older than the history are charged at `fallback_rate`.
    pub fn source_frames(&self, queue_out: u64, fallback_rate: f64) -> f64 {
        let mut need = queue_out;
        let mut src = 0.0;
        for i in 0..self.len {
            if need == 0 {
                break;
            }
            let s = self.entries[(self.head + WRITE_SPANS - 1 - i) % WRITE_SPANS];
            if s.out_frames == 0 {
                continue;
            }
            let take = need.min(s.out_frames);
            src += s.src_frames * (take as f64 / s.out_frames as f64);
            need -= take;
        }
        if need > 0 {
            src += need as f64 * fallback_rate;
        }
        src
    }
}


/// f64 `rate` from the published micro-rate.
pub fn rate_from_micro(rate_micro: i32) -> f64 {
    f64::from(rate_micro) / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ms_frame_round_trip_exact_at_48k() {
        for ms in [0u64, 1, 999, 1000, 61_234, 3_600_000] {
            let f = ms_to_frame(ms, 48_000);
            assert_eq!(f, ms * 48);
            assert_eq!(frame_to_ms(f, 48_000), ms);
        }
        // 44.1 kHz floors, never rounds up past the requested ms.
        assert_eq!(ms_to_frame(1, 44_100), 44);
        assert_eq!(frame_to_ms(44, 44_100), 0);
        assert_eq!(frame_to_ms(44_100, 44_100), 1000);
        assert_eq!(frame_to_ms(5, 0), 0);
    }

    #[test]
    fn queue_ms_converts_to_frames() {
        assert_eq!(queue_ms_to_frames(80, 48_000), 3840);
        assert_eq!(queue_ms_to_frames(80, 44_100), 3528);
        assert_eq!(queue_ms_to_frames(0, 44_100), 0);
    }

    #[test]
    fn heard_position_subtracts_rate_times_queue() {
        // 1.0x: heard = written - queue.
        assert_eq!(heard_position_frames(100_000.0, 1.0, 3840, 0), 96_160);
        // 2.0x: each queued output frame is two source frames.
        assert_eq!(heard_position_frames(100_000.0, 2.0, 3840, 0), 92_320);
        // 0.5x.
        assert_eq!(heard_position_frames(100_000.0, 0.5, 4000, 0), 98_000);
        // Reverse: queued audio lies ahead in media time -> heard is larger than written.
        assert_eq!(heard_position_frames(100_000.0, -1.0, 3840, 0), 103_840);
        // Empty queue: heard == written.
        assert_eq!(heard_position_frames(12_345.0, 1.0, 0, 0), 12_345);
    }

    #[test]
    fn heard_position_never_precedes_the_landing_frame() {
        // Stream just started at frame 5000: the queue holds more than has been written.
        assert_eq!(heard_position_frames(5_100.0, 1.0, 3840, 5_000), 5_000);
        assert_eq!(heard_position_frames(0.0, 1.0, 3840, 0), 0);
        assert_eq!(heard_position_frames(f64::NAN, 1.0, 10, 7), 7);
    }

    #[test]
    fn micro_rate_converts() {
        assert_eq!(rate_from_micro(RATE_MICRO_UNITY), 1.0);
        assert_eq!(rate_from_micro(-500_000), -0.5);
    }

    #[test]
    fn heard_position_src_is_the_span_form_of_the_rate_form() {
        for (rate, queue) in [(1.0, 3840u64), (4.0, 3840), (-1.0, 3840), (0.5, 4000)] {
            assert_eq!(
                heard_position_src(100_000.0, rate * queue as f64, 0),
                heard_position_frames(100_000.0, rate, queue, 0)
            );
        }
        assert_eq!(heard_position_src(5_100.0, 3_840.0, 5_000), 5_000);
    }

    #[test]
    fn write_spans_measure_the_newest_queue_across_a_rate_change() {
        let mut s = WriteSpans::new();
        // 1000 output frames at 4x, then 1000 at 1x.
        s.push(1_000, 4_000.0);
        s.push(1_000, 1_000.0);
        // The newest 1000 output frames are the 1x block.
        assert_eq!(s.source_frames(1_000, 1.0), 1_000.0);
        // The whole queue spans the 1x block plus the 4x one: the release-position case.
        assert_eq!(s.source_frames(2_000, 1.0), 5_000.0);
        // A partial slice is charged proportionally.
        assert_eq!(s.source_frames(1_500, 1.0), 3_000.0);
        // Zero queue: no span, whatever the fallback.
        assert_eq!(s.source_frames(0, 8.0), 0.0);
        // More queue than history: the missing part uses the fallback rate.
        assert_eq!(s.source_frames(3_000, 2.0), 5_000.0 + 2_000.0);
    }

    #[test]
    fn write_spans_reverse_is_negative_and_clear_forgets() {
        let mut s = WriteSpans::new();
        s.push(1_000, -1_000.0);
        assert_eq!(s.source_frames(1_000, 1.0), -1_000.0);
        s.clear();
        assert_eq!(s.source_frames(1_000, 1.0), 1_000.0, "cleared: fallback");
        // Out-of-range / empty writes are ignored.
        s.push(0, 5.0);
        assert_eq!(s.source_frames(0, 1.0), 0.0);
    }

    #[test]
    fn write_spans_keeps_the_newest_slices_when_full() {
        let mut s = WriteSpans::new();
        for _ in 0..(WRITE_SPANS + 8) {
            s.push(100, 400.0);
        }
        // Only the newest WRITE_SPANS slices are remembered; each is 4:1.
        let kept = 100 * WRITE_SPANS as u64;
        assert_eq!(s.source_frames(kept, 1.0), 400.0 * WRITE_SPANS as f64);
        // Anything older than the ring falls back to the given rate.
        assert_eq!(s.source_frames(kept + 50, 1.0), 400.0 * WRITE_SPANS as f64 + 50.0);
    }
}
