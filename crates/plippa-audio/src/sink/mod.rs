//! Audio output sinks.

mod config;
mod mock;
#[cfg(feature = "pulse")]
mod pulse;

pub use config::PulseConfig;
pub use mock::MockSink;
#[cfg(feature = "pulse")]
pub use pulse::PulseSink;

use crate::error::AudioError;

/// Destination for interleaved f32 PCM blocks.
///
/// Implementations must be usable from a dedicated audio thread without
/// taking control-plane locks. Prefer non-blocking or short-blocking writes.
pub trait AudioSink: Send {
    fn channels(&self) -> u16;
    fn sample_rate_hz(&self) -> u32;

    /// Write interleaved f32 samples. Length must be a multiple of `channels()`.
    fn write(&mut self, interleaved: &[f32]) -> Result<(), AudioError>;

    /// Drop any buffered output so Pause cuts off quickly. Default: no-op.
    fn flush(&mut self) -> Result<(), AudioError> {
        Ok(())
    }

    /// Frames written but not yet heard (the sink's output queue). Feeds the heard-position
    /// correction `pos_heard = pos_written - rate * queue` (WARP_03). Default 0: sinks that
    /// play instantly (mocks) have no queue.
    fn queue_frames(&mut self) -> u64 {
        0
    }
}

/// Opens a playback stream at a requested rate (native-rate track / SFX streams).
///
/// PipeWire/Pulse converts to the device rate; callers open one stream per
/// active role and tear it down when idle — no permanent multi-rate mixer bank.
pub trait AudioSinkFactory: Send {
    fn open(&self, channels: u16, sample_rate_hz: u32) -> Result<Box<dyn AudioSink>, AudioError>;
}

impl<F> AudioSinkFactory for F
where
    F: Fn(u16, u32) -> Result<Box<dyn AudioSink>, AudioError> + Send,
{
    fn open(&self, channels: u16, sample_rate_hz: u32) -> Result<Box<dyn AudioSink>, AudioError> {
        (self)(channels, sample_rate_hz)
    }
}
