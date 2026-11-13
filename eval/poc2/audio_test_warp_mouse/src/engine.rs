use crate::control::{speed_from_warp, ControlSnapshot, DspMode, WarpControl};
use anyhow::{Context, Result};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use signalsmith_stretch::Stretch;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const CROSSFADE_S: f64 = 0.03;
const STRETCH_BLOCK: usize = 256;
const STRETCH_PREROLL_S: f64 = 0.2;
const RESAMPLE_CHUNK: usize = 1024;

pub struct PlaybackEngine {
    source: Vec<f32>,
    channels: usize,
    sample_rate: u32,
    source_pos: f64,
    stretch: Stretch,
    current_mode: DspMode,
    crossfade_remaining: usize,
    crossfade_total: usize,
    prev_render: Vec<f32>,
    stretch_input: Vec<f32>,
    stretch_output: Vec<f32>,
    control: Arc<WarpControl>,
}

impl PlaybackEngine {
    pub fn new(
        source: Vec<f32>,
        channels: u32,
        source_rate: u32,
        output_rate: u32,
        control: Arc<WarpControl>,
    ) -> Result<Self> {
        let channels = channels as usize;
        let source = resample_to_rate(source, channels, source_rate, output_rate)?;
        let mut stretch = Stretch::preset_default(channels as u32, output_rate);
        stretch.set_transpose_factor(1.0, None);

        Ok(Self {
            source,
            channels,
            sample_rate: output_rate,
            source_pos: 0.0,
            stretch,
            current_mode: control.snapshot().mode,
            crossfade_remaining: 0,
            crossfade_total: 0,
            prev_render: Vec::new(),
            stretch_input: Vec::new(),
            stretch_output: Vec::new(),
            control,
        })
    }

    pub fn render(&mut self, out: &mut [f32]) -> Result<()> {
        let out_frames = out.len() / self.channels;
        let mut written = 0usize;

        while written < out_frames {
            let chunk = (out_frames - written).min(STRETCH_BLOCK);
            let snapshot = self.control.snapshot();
            self.handle_mode_change(snapshot.mode, snapshot.warp_pct);

            let start = written * self.channels;
            let end = (written + chunk) * self.channels;
            let chunk_buf = &mut out[start..end];
            self.render_chunk(chunk, snapshot, chunk_buf);

            if self.crossfade_remaining > 0 {
                self.apply_crossfade(chunk_buf, chunk);
            } else {
                self.prev_render = chunk_buf.to_vec();
            }

            written += chunk;
        }

        Ok(())
    }

    fn handle_mode_change(&mut self, new_mode: DspMode, warp_pct: i32) {
        if new_mode == self.current_mode {
            return;
        }

        let crossfade_frames = (CROSSFADE_S * self.sample_rate as f64).round() as usize;
        self.crossfade_total = crossfade_frames.max(1);
        self.crossfade_remaining = self.crossfade_total;

        if new_mode == DspMode::PitchPreserved {
            self.prime_stretcher(warp_pct);
        } else if self.current_mode == DspMode::PitchPreserved {
            self.stretch.reset();
            self.stretch.set_transpose_factor(1.0, None);
        }

        self.current_mode = new_mode;
    }

    fn prime_stretcher(&mut self, warp_pct: i32) {
        self.stretch.reset();
        self.stretch.set_transpose_factor(1.0, None);

        let preroll_frames = (STRETCH_PREROLL_S * self.sample_rate as f64) as usize;
        let speed = speed_from_warp(warp_pct).max(0.01);
        let input_frames = ((preroll_frames as f64) * speed).ceil() as usize;
        let mut preroll = vec![0.0f32; input_frames * self.channels];
        self.read_forward_peek(self.source_pos, input_frames, &mut preroll);
        self.stretch.seek(&preroll, speed);
    }

    fn render_chunk(&mut self, frames: usize, snapshot: ControlSnapshot, out: &mut [f32]) {
        match self.current_mode {
            DspMode::Vinyl => self.render_variable_rate(frames, snapshot.warp_pct, out),
            DspMode::PitchPreserved => self.render_pitch_preserved(frames, snapshot.warp_pct, out),
        }
    }

    fn render_variable_rate(&mut self, frames: usize, warp_pct: i32, out: &mut [f32]) {
        let speed = speed_from_warp(warp_pct);
        for frame in 0..frames {
            for ch in 0..self.channels {
                out[frame * self.channels + ch] =
                    self.read_forward_sample(self.source_pos, ch);
            }
            self.advance_source(speed);
        }
    }

    fn render_pitch_preserved(&mut self, frames: usize, warp_pct: i32, out: &mut [f32]) {
        let speed = speed_from_warp(warp_pct).max(0.01);
        let input_frames = ((frames as f64) * speed).ceil() as usize;
        let mut stretch_input = std::mem::take(&mut self.stretch_input);
        stretch_input.resize(input_frames * self.channels, 0.0);
        self.read_forward_interleaved(input_frames, &mut stretch_input);
        self.stretch_input = stretch_input;

        self.stretch_output.resize(frames * self.channels, 0.0);
        self.stretch
            .process(&self.stretch_input, &mut self.stretch_output);
        out.copy_from_slice(&self.stretch_output);
    }

    fn read_forward_interleaved(&mut self, frames: usize, out: &mut [f32]) {
        for frame in 0..frames {
            for ch in 0..self.channels {
                out[frame * self.channels + ch] =
                    self.read_forward_sample(self.source_pos + frame as f64, ch);
            }
        }
        self.advance_source(frames as f64);
    }

    fn read_forward_peek(&self, start_pos: f64, frames: usize, out: &mut [f32]) {
        for frame in 0..frames {
            for ch in 0..self.channels {
                out[frame * self.channels + ch] =
                    self.read_forward_sample(start_pos + frame as f64, ch);
            }
        }
    }

    fn advance_source(&mut self, delta: f64) {
        let max_frame = self.source.len() / self.channels;
        if max_frame == 0 {
            return;
        }
        self.source_pos += delta;
        while self.source_pos >= max_frame as f64 {
            self.source_pos -= max_frame as f64;
        }
        while self.source_pos < 0.0 {
            self.source_pos += max_frame as f64;
        }
    }

    fn read_forward_sample(&self, pos: f64, channel: usize) -> f32 {
        let max_frame = self.source.len() / self.channels;
        if max_frame < 2 {
            return 0.0;
        }
        let wrapped = pos.rem_euclid(max_frame as f64);
        let frame = wrapped.floor() as usize;
        let frac = (wrapped - frame as f64) as f32;
        if frame >= max_frame.saturating_sub(1) {
            return 0.0;
        }
        let i0 = frame * self.channels + channel;
        let i1 = i0 + self.channels;
        let s0 = self.source.get(i0).copied().unwrap_or(0.0);
        let s1 = self.source.get(i1).copied().unwrap_or(0.0);
        s0 + (s1 - s0) * frac
    }

    fn apply_crossfade(&mut self, chunk: &mut [f32], frames: usize) {
        if self.prev_render.len() != chunk.len() {
            self.prev_render = chunk.to_vec();
            self.crossfade_remaining = 0;
            return;
        }

        for frame in 0..frames {
            let done = self.crossfade_total - self.crossfade_remaining;
            let t = done as f32 / self.crossfade_total as f32;
            let fade_out = (std::f32::consts::FRAC_PI_2 * (1.0 - t)).cos();
            let fade_in = (std::f32::consts::FRAC_PI_2 * t).sin();
            for ch in 0..self.channels {
                let idx = frame * self.channels + ch;
                chunk[idx] = self.prev_render[idx] * fade_out + chunk[idx] * fade_in;
            }
            if self.crossfade_remaining > 0 {
                self.crossfade_remaining -= 1;
            }
        }

        if self.crossfade_remaining == 0 {
            self.prev_render = chunk.to_vec();
        }
    }
}

fn resample_to_rate(
    input: Vec<f32>,
    channels: usize,
    from_rate: u32,
    to_rate: u32,
) -> Result<Vec<f32>> {
    if from_rate == to_rate || input.is_empty() {
        return Ok(input);
    }

    let in_frames = input.len() / channels;
    let mut per_channel = vec![Vec::with_capacity(in_frames); channels];
    for frame in 0..in_frames {
        for ch in 0..channels {
            per_channel[ch].push(input[frame * channels + ch]);
        }
    }

    let ratio = to_rate as f64 / from_rate as f64;
    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };
    let mut resampler =
        SincFixedIn::<f32>::new(ratio, 2.0, params, RESAMPLE_CHUNK, channels)
            .context("create source resampler")?;

    let mut out_channels = vec![Vec::new(); channels];
    let mut offset = 0usize;
    while offset < in_frames {
        let chunk_len = (in_frames - offset).min(RESAMPLE_CHUNK);
        let chunk: Vec<&[f32]> = per_channel
            .iter()
            .map(|ch| &ch[offset..offset + chunk_len])
            .collect();
        let out_len = resampler.output_frames_next();
        let mut out_chunk = vec![vec![0.0f32; out_len]; channels];
        resampler
            .process_into_buffer(&chunk, &mut out_chunk, None)
            .context("resample source audio")?;
        for ch in 0..channels {
            out_channels[ch].extend_from_slice(&out_chunk[ch]);
        }
        offset += chunk_len;
    }

    let out_frames = out_channels.first().map(|ch| ch.len()).unwrap_or(0);
    let mut output = vec![0.0f32; out_frames * channels];
    for frame in 0..out_frames {
        for ch in 0..channels {
            output[frame * channels + ch] = out_channels[ch][frame];
        }
    }
    Ok(output)
}

pub struct SharedEngine {
    inner: Mutex<PlaybackEngine>,
    stop: AtomicBool,
}

impl SharedEngine {
    pub fn new(engine: PlaybackEngine) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(engine),
            stop: AtomicBool::new(false),
        })
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn should_stop(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub fn render(&self, out: &mut [f32]) {
        if self.should_stop() {
            out.fill(0.0);
            return;
        }
        let mut guard = self.inner.lock().unwrap();
        if let Err(err) = guard.render(out) {
            eprintln!("render error: {err:#}");
            out.fill(0.0);
        }
    }
}
