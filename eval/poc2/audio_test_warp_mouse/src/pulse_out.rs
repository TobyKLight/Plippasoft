use anyhow::{Context, Result};
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use std::env;

use crate::engine::SharedEngine;

const CHUNK_FRAMES: usize = 1024;

pub struct PulseOutput {
    simple: Simple,
    channels: usize,
    scratch: Vec<f32>,
}

impl PulseOutput {
    pub fn new(channels: u32, sample_rate: u32) -> Result<Self> {
        let channels = channels as usize;
        let spec = Spec {
            format: Format::F32le,
            channels: channels as u8,
            rate: sample_rate,
        };
        if !spec.is_valid() {
            anyhow::bail!("invalid Pulse sample spec: {channels} ch @ {sample_rate} Hz");
        }

        let server = env::var("PULSE_SERVER").ok();
        let simple = Simple::new(
            server.as_deref(),
            "poc2_audio_test_warp_mouse",
            Direction::Playback,
            None,
            "warp mouse",
            &spec,
            None,
            None,
        )
        .context("connect to PulseAudio (is PipeWire-Pulse running?)")?;

        Ok(Self {
            simple,
            channels,
            scratch: vec![0.0f32; CHUNK_FRAMES * channels],
        })
    }

    pub fn play_loop(&mut self, engine: &SharedEngine) -> Result<()> {
        while !engine.should_stop() {
            engine.render(&mut self.scratch);
            let bytes = f32_slice_as_bytes(&self.scratch);
            self.simple
                .write(bytes)
                .context("Pulse write failed (sink disconnected?)")?;
        }
        Ok(())
    }
}

fn f32_slice_as_bytes(samples: &[f32]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            samples.as_ptr().cast(),
            samples.len() * std::mem::size_of::<f32>(),
        )
    }
}
