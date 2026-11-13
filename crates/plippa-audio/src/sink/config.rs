//! Output-queue constants for the Pulse sink (WARP_03).
//!
//! One struct for everything sink-latency related so it is retuned in one place (from the
//! `warp_bench latency` result, README section 7). The struct is pure data and compiles
//! without the `pulse` feature; only `PulseSink` consumes the buffer attributes.

use crate::position::queue_ms_to_frames;

/// Pulse buffer sizing and the heard-position latency model.
#[derive(Debug, Clone, PartialEq)]
pub struct PulseConfig {
    /// `BufferAttr::tlength`: target playback buffer fill in ms (`PULSE_TLENGTH_MS`).
    pub tlength_ms: u32,
    /// `BufferAttr::minreq`: minimum write request in ms (`PULSE_MINREQ_MS`).
    pub minreq_ms: u32,
    /// Assumed queued audio in ms when the latency is not measured (`SINK_LATENCY_MS`).
    /// Used by the heard-position correction `pos_heard = pos_written - rate * queue`.
    pub sink_latency_ms: u32,
    /// Poll `Simple::get_latency` instead of trusting `sink_latency_ms` (`pulse` feature).
    pub poll_latency: bool,
    /// Poll period for `get_latency` when `poll_latency` is on (1-2 Hz).
    pub latency_poll_ms: u32,
}

impl Default for PulseConfig {
    fn default() -> Self {
        Self {
            tlength_ms: 80,
            minreq_ms: 20,
            sink_latency_ms: 80,
            poll_latency: false,
            latency_poll_ms: 500,
        }
    }
}

impl PulseConfig {
    /// One direct sink (local-only, or a single extra output with no delay).
    ///
    /// The numbers are the starting rung of the ladder; the device pass in the AUDIO
    /// work package records what was actually kept.
    pub fn low_latency() -> Self {
        Self {
            tlength_ms: 24,
            minreq_ms: 6,
            sink_latency_ms: 24,
            poll_latency: true,
            latency_poll_ms: 250,
        }
    }

    /// Anything routed through `plippa.multi` (including a one-member delay sink).
    ///
    /// The numbers are the starting rung of the ladder; the device pass in the AUDIO
    /// work package records what was actually kept.
    pub fn multi() -> Self {
        Self {
            tlength_ms: 120,
            minreq_ms: 30,
            sink_latency_ms: 120,
            poll_latency: true,
            latency_poll_ms: 250,
        }
    }

    /// Profile for a resolved routing target.
    ///
    /// The numbers are the starting rung of the ladder; the device pass in the AUDIO
    /// work package records what was actually kept.
    pub fn for_target(target: &crate::route::Target) -> Self {
        match target {
            crate::route::Target::Direct(_) => Self::low_latency(),
            crate::route::Target::Multi { .. } => Self::multi(),
        }
    }

    /// Bytes per interleaved f32 frame.
    fn frame_bytes(channels: u16) -> u64 {
        4 * u64::from(channels.max(1))
    }

    /// `(tlength, minreq)` in bytes for the given stream format (what `BufferAttr` wants).
    pub fn buffer_bytes(&self, channels: u16, sample_rate_hz: u32) -> (u32, u32) {
        let fb = Self::frame_bytes(channels);
        let t = queue_ms_to_frames(u64::from(self.tlength_ms), sample_rate_hz) * fb;
        let m = queue_ms_to_frames(u64::from(self.minreq_ms), sample_rate_hz) * fb;
        (
            t.min(u64::from(u32::MAX)) as u32,
            m.min(u64::from(u32::MAX)) as u32,
        )
    }

    /// Assumed sink queue in frames for `sink_latency_ms` at `sample_rate_hz`.
    pub fn assumed_queue_frames(&self, sample_rate_hz: u32) -> u64 {
        queue_ms_to_frames(u64::from(self.sink_latency_ms), sample_rate_hz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_register() {
        let c = PulseConfig::default();
        assert_eq!((c.tlength_ms, c.minreq_ms, c.sink_latency_ms), (80, 20, 80));
        assert!(!c.poll_latency);
    }

    #[test]
    fn target_selects_the_profile_and_both_poll_latency() {
        use crate::route::Target;

        let direct = PulseConfig::for_target(&Target::Direct("local".into()));
        assert_eq!(direct, PulseConfig::low_latency());
        assert_eq!(
            (direct.tlength_ms, direct.minreq_ms, direct.sink_latency_ms),
            (24, 6, 24)
        );
        assert!(direct.poll_latency);

        let multi = PulseConfig::for_target(&Target::Multi { compensate: true });
        assert_eq!(multi, PulseConfig::multi());
        assert_eq!(
            (multi.tlength_ms, multi.minreq_ms, multi.sink_latency_ms),
            (120, 30, 120)
        );
        assert!(multi.poll_latency);
        assert!(PulseConfig::low_latency().poll_latency);
    }

    #[test]
    fn buffer_bytes_are_whole_frames() {
        let c = PulseConfig::default();
        // 80 ms @ 48 kHz stereo f32 = 3840 frames * 8 bytes.
        assert_eq!(c.buffer_bytes(2, 48_000), (3840 * 8, 960 * 8));
        // 44.1 kHz: 3528 / 882 frames.
        assert_eq!(c.buffer_bytes(2, 44_100), (3528 * 8, 882 * 8));
        let (t, m) = c.buffer_bytes(2, 44_100);
        assert_eq!(t % 8, 0);
        assert_eq!(m % 8, 0);
        assert_eq!(c.assumed_queue_frames(48_000), 3840);
    }
}
