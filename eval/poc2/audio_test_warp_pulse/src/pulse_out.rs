use anyhow::{Context, Result};
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use std::env;
use std::thread;
use std::time::Duration;

use crate::engine::SharedEngine;

const CHUNK_FRAMES: usize = 1024;

pub struct PulseOutput {
    simple: Simple,
    channels: usize,
    sample_rate: u32,
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
            "poc2_audio_test_warp_pulse",
            Direction::Playback,
            None,
            "warp test",
            &spec,
            None,
            None,
        )
        .context("connect to PulseAudio (is PipeWire-Pulse running?)")?;

        Ok(Self {
            simple,
            channels,
            sample_rate,
            scratch: vec![0.0f32; CHUNK_FRAMES * channels],
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn play_loop(&mut self, engine: &SharedEngine) -> Result<()> {
        loop {
            if engine.should_stop() {
                break;
            }
            engine.render(&mut self.scratch);
            let bytes = f32_slice_as_bytes(&self.scratch);
            self.simple
                .write(bytes)
                .context("Pulse write failed (sink disconnected?)")?;
            if engine.is_finished() {
                break;
            }
        }
        Ok(())
    }

    pub fn wait_until_finished(engine: &SharedEngine) {
        while !engine.should_stop() && !engine.is_finished() {
            thread::sleep(Duration::from_millis(50));
        }
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
