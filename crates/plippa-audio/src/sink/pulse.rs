//! PulseAudio / PipeWire-Pulse sink (feature `pulse`).

use std::env;
use std::time::{Duration, Instant};

use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

use crate::error::AudioError;
use crate::position::queue_ms_to_frames;
use crate::sink::{AudioSink, PulseConfig};

/// Blocking PCM writer via `libpulse-simple` (same path as eval/poc2).
pub struct PulseSink {
    simple: Simple,
    channels: u16,
    sample_rate_hz: u32,
    cfg: PulseConfig,
    /// Last queue estimate in frames (constant model, or the last `get_latency` poll).
    queue_estimate: u64,
    last_poll: Option<Instant>,
}

impl PulseSink {
    pub fn new(channels: u16, sample_rate_hz: u32) -> Result<Self, AudioError> {
        Self::with_config(channels, sample_rate_hz, PulseConfig::default())
    }

    /// Open with explicit buffer attributes (`tlength` / `minreq` from `cfg`). If the server
    /// rejects them the stream is opened again with the server defaults (logged).
    pub fn with_config(
        channels: u16,
        sample_rate_hz: u32,
        cfg: PulseConfig,
    ) -> Result<Self, AudioError> {
        Self::new_at(channels, sample_rate_hz, cfg, None)
    }

    /// Open targeting the PipeWire sink named `dev` (the `Simple::new` `dev` argument).
    ///
    /// `None` plays into the server's default sink. The same device is used on the
    /// server-default retry path; a missing sink therefore fails with its name in the
    /// error and journal line instead of silently falling back to the default output.
    pub fn new_at(
        channels: u16,
        sample_rate_hz: u32,
        cfg: PulseConfig,
        dev: Option<&str>,
    ) -> Result<Self, AudioError> {
        let spec = Spec {
            format: Format::F32le,
            channels: channels as u8,
            rate: sample_rate_hz,
        };
        if !spec.is_valid() {
            return Err(AudioError::Sink(format!(
                "invalid Pulse sample spec: {channels} ch @ {sample_rate_hz} Hz"
            )));
        }

        let device = dev.unwrap_or("default");
        let server = env::var("PULSE_SERVER").ok();
        let (tlength, minreq) = cfg.buffer_bytes(channels, sample_rate_hz);
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength,
            prebuf: u32::MAX,
            minreq,
            fragsize: u32::MAX,
        };
        let open = |attr: Option<&BufferAttr>| {
            Simple::new(
                server.as_deref(),
                "plippa-audio",
                Direction::Playback,
                dev,
                "plippa playback",
                &spec,
                None,
                attr,
            )
        };
        let simple = match open(Some(&attr)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "plippa-audio: first open failed for device {device} (tlength {tlength} B, minreq {minreq} B): {e}; retrying with server defaults"
                );
                open(None).map_err(|e| {
                    AudioError::Sink(format!(
                        "PipeWire connect failed for device {device}: {e}"
                    ))
                })?
            }
        };

        let queue_estimate = cfg.assumed_queue_frames(sample_rate_hz);
        Ok(Self {
            simple,
            channels,
            sample_rate_hz,
            cfg,
            queue_estimate,
            last_poll: None,
        })
    }
}

impl AudioSink for PulseSink {
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
        let bytes = f32_slice_as_bytes(interleaved);
        self.simple
            .write(bytes)
            .map_err(|e| AudioError::Sink(format!("Pulse write failed: {e}")))?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), AudioError> {
        self.simple
            .flush()
            .map_err(|e| AudioError::Sink(format!("Pulse flush failed: {e}")))?;
        Ok(())
    }

    fn queue_frames(&mut self) -> u64 {
        if !self.cfg.poll_latency {
            return self.queue_estimate;
        }
        let due = self.last_poll.is_none_or(|t| {
            t.elapsed() >= Duration::from_millis(u64::from(self.cfg.latency_poll_ms))
        });
        if due {
            self.last_poll = Some(Instant::now());
            // On error keep the previous estimate (initially the constant model).
            if let Ok(lat) = self.simple.get_latency() {
                self.queue_estimate = queue_ms_to_frames(lat.0 / 1000, self.sample_rate_hz);
            }
        }
        self.queue_estimate
    }
}

fn f32_slice_as_bytes(samples: &[f32]) -> &[u8] {
    // SAFETY: f32 is plain data; Pulse expects little-endian F32le on this platform.
    unsafe {
        std::slice::from_raw_parts(
            samples.as_ptr().cast(),
            samples.len() * std::mem::size_of::<f32>(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires the dev VM/device PipeWire server; no.such.sink must not exist"]
    fn a_missing_device_returns_an_error_naming_the_sink() {
        let result = PulseSink::new_at(2, 48_000, PulseConfig::default(), Some("no.such.sink"));
        match result {
            Err(AudioError::Sink(msg)) => {
                assert!(msg.contains("no.such.sink"), "error did not name the sink: {msg}");
            }
            Err(other) => panic!("expected a sink error for the missing device, got {other:?}"),
            Ok(_) => panic!("opened a stream for a sink that does not exist"),
        }
    }
}
