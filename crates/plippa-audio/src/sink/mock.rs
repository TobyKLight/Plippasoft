//! In-memory sink for unit tests (no hardware DAC required).

use crate::error::AudioError;
use crate::sink::AudioSink;

/// Captures every `write` into an expandable buffer.
#[derive(Debug, Default)]
pub struct MockSink {
    channels: u16,
    sample_rate_hz: u32,
    pub captured: Vec<f32>,
}

impl MockSink {
    pub fn new(channels: u16, sample_rate_hz: u32) -> Self {
        Self {
            channels,
            sample_rate_hz,
            captured: Vec::new(),
        }
    }

    pub fn frame_count(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.captured.len() / self.channels as usize
        }
    }

    pub fn clear(&mut self) {
        self.captured.clear();
    }
}

impl AudioSink for MockSink {
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
        self.captured.extend_from_slice(interleaved);
        Ok(())
    }
}
