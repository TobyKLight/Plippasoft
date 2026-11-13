//! Optional concurrent WS2811 animation, for chasing screen-flicker reports.
//!
//! This mirrors [`eval/poc2/led_test`](../../led_test/src/main.rs): walk the
//! front strip (GPIO 13 / channel 1), walk the top strip (GPIO 12 / channel 0),
//! then run a scrolling rainbow; the cycle repeats for as long as the SPI
//! animation runs. It runs on its own thread so the SPI present path is
//! exercised exactly like `plippa-ui` (display worker + LED worker).
//!
//! `--led-mode` makes the LED side selectable so a flicker report can be
//! narrowed down:
//!
//! | Mode | DMA renders | LED current |
//! |------|-------------|-------------|
//! | `off` | every frame | none (all black) |
//! | `solid` | once | max (all white) |
//! | `steady` | every frame | max (all white) |
//! | `cycle` / `walk` / `rainbow` | every frame | animated |
//!
//! `off` vs `cycle` isolates DMA/timing; `solid` vs `off` isolates supply /
//! ground noise from the strips.
//!
//! see led_test/pinout.md for wiring.

use rs_ws281x::{ChannelBuilder, Controller, ControllerBuilder, RawColor, StripType};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Front panel data (stream 1) — GPIO 13 / PWM1 → ws2811 channel 1.
pub const PIN_FRONT: i32 = 13;
pub const CH_FRONT: usize = 1;
/// Top / basetop panel data (stream 2) — GPIO 12 / PWM0 → ws2811 channel 0.
pub const PIN_TOP: i32 = 12;
pub const CH_TOP: usize = 0;

pub const DEFAULT_FRONT_LEDS: i32 = 33;
pub const DEFAULT_TOP_LEDS: i32 = 30;
/// Default LED brightness. Deliberately low: the bouncy harness is meant to
/// match `plippa-ui`'s dim steady state, not to be a blinkenlights demo.
pub const DEFAULT_BRIGHTNESS_PCT: u8 = 10;
pub const DEFAULT_FPS: u64 = 30;
/// rpi_ws281x default. Change it to test DMA-channel contention with SPI0.
pub const DEFAULT_DMA: i32 = 10;

const RAINBOW_SECS: u64 = 3;
/// How often a blocked LED thread notices Ctrl-C / stop.
const STOP_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedMode {
    /// led_test sequence, repeated: walk front, walk top, 3 s rainbow.
    Cycle,
    /// Walk both strips, repeating (no rainbow).
    Walk,
    /// Continuous scrolling rainbow.
    Rainbow,
    /// All white, re-rendered every frame (max current + DMA load).
    Steady,
    /// All white, rendered once (max current, no repeated DMA).
    Solid,
    /// All black, re-rendered every frame (DMA load, no light).
    Off,
}

impl LedMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cycle" => Some(Self::Cycle),
            "walk" => Some(Self::Walk),
            "rainbow" => Some(Self::Rainbow),
            "steady" => Some(Self::Steady),
            "solid" => Some(Self::Solid),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Cycle => "cycle",
            Self::Walk => "walk",
            Self::Rainbow => "rainbow",
            Self::Steady => "steady",
            Self::Solid => "solid",
            Self::Off => "off",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LedConfig {
    pub front_leds: i32,
    pub top_leds: i32,
    pub brightness_pct: u8,
    pub fps: u64,
    pub dma: i32,
    pub mode: LedMode,
    /// Suppress the per-LED step log (phase headers still print).
    pub quiet: bool,
}

impl Default for LedConfig {
    fn default() -> Self {
        Self {
            front_leds: DEFAULT_FRONT_LEDS,
            top_leds: DEFAULT_TOP_LEDS,
            brightness_pct: DEFAULT_BRIGHTNESS_PCT,
            fps: DEFAULT_FPS,
            dma: DEFAULT_DMA,
            mode: LedMode::Cycle,
            quiet: false,
        }
    }
}

impl LedConfig {
    fn step_ms(&self) -> u64 {
        1000 / self.fps.max(1)
    }
}

/// Counters shared with the animation thread; read by the SPI `STAT` line.
#[derive(Debug, Default)]
pub struct LedStats {
    /// WS2811 `render()` calls completed.
    pub renders: AtomicU64,
    /// Slowest `render()` seen (µs) — includes waiting for the DMA burst.
    pub max_render_us: AtomicU64,
    /// Renders started while an SPI `present` was in flight.
    pub overlap_renders: AtomicU64,
    /// Renders that failed — the loop keeps going.
    pub errors: AtomicU64,
}

pub struct LedHandle {
    join: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    pub stats: Arc<LedStats>,
}

impl LedHandle {
    pub fn stop_and_join(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

impl Drop for LedHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// `rs_ws281x::Controller` holds raw DMA pointers and is not `Send`; it is
/// created here on the calling thread and then owned by exactly one LED
/// thread for its whole life (same arrangement as `plippa-ui`).
struct SendController(Controller);
unsafe impl Send for SendController {}

/// Open the strips (fails fast in the caller) and start the animation thread.
pub fn spawn(
    cfg: LedConfig,
    stop: Arc<AtomicBool>,
    spi_present_busy: Arc<AtomicBool>,
) -> Result<LedHandle, String> {
    if cfg.front_leds <= 0 && cfg.top_leds <= 0 {
        return Err("at least one of --led-front / --led-top must be > 0".into());
    }

    let brightness = pct_to_brightness(cfg.brightness_pct);
    let mut builder = ControllerBuilder::new();
    builder.freq(800_000).dma(cfg.dma);
    builder.channel(
        CH_TOP,
        ChannelBuilder::new()
            .pin(PIN_TOP)
            .count(cfg.top_leds.max(0))
            .strip_type(StripType::Ws2811Grb)
            .brightness(brightness)
            .build(),
    );
    builder.channel(
        CH_FRONT,
        ChannelBuilder::new()
            .pin(PIN_FRONT)
            .count(cfg.front_leds.max(0))
            .strip_type(StripType::Ws2811Grb)
            .brightness(brightness)
            .build(),
    );
    let controller = builder
        .build()
        .map_err(|e| format!("ws2811 init failed: {e:?} (try sudo)"))?;

    println!(
        "LEDs: mode={} front GPIO{PIN_FRONT} ({} LEDs), top GPIO{PIN_TOP} ({} LEDs), \
         brightness {}%, {} fps, DMA channel {}",
        cfg.mode.label(),
        cfg.front_leds,
        cfg.top_leds,
        cfg.brightness_pct,
        cfg.fps,
        cfg.dma
    );

    let stats = Arc::new(LedStats::default());
    let stats_t = Arc::clone(&stats);
    let stop_t = Arc::clone(&stop);
    let send_ctrl = SendController(controller);
    let join = thread::Builder::new()
        .name("bouncy-leds".into())
        .spawn(move || run(send_ctrl, cfg, stop_t, spi_present_busy, stats_t))
        .map_err(|e| format!("LED thread spawn failed: {e}"))?;

    Ok(LedHandle {
        join: Some(join),
        stop,
        stats,
    })
}

fn run(
    SendController(mut ctrl): SendController,
    cfg: LedConfig,
    stop: Arc<AtomicBool>,
    spi_present_busy: Arc<AtomicBool>,
    stats: Arc<LedStats>,
) {
    let step_ms = cfg.step_ms();
    let mut round = 0u64;

    while !stop.load(Ordering::Relaxed) {
        match cfg.mode {
            LedMode::Cycle => {
                round += 1;
                println!("\nLED round {round} — stream 1 front panel (GPIO {PIN_FRONT})");
                walk(
                    &mut ctrl,
                    CH_FRONT,
                    cfg.front_leds.max(0) as usize,
                    "front",
                    step_ms,
                    &stop,
                    &spi_present_busy,
                    &stats,
                    cfg.quiet,
                );
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                println!("LED round {round} — stream 2 top panel (GPIO {PIN_TOP})");
                walk(
                    &mut ctrl,
                    CH_TOP,
                    cfg.top_leds.max(0) as usize,
                    "top",
                    step_ms,
                    &stop,
                    &spi_present_busy,
                    &stats,
                    cfg.quiet,
                );
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                println!("LED round {round} — rainbow, all LEDs (full saturation)");
                rainbow(&mut ctrl, &cfg, &stop, &spi_present_busy, &stats, RAINBOW_SECS);
            }
            LedMode::Walk => {
                round += 1;
                if !cfg.quiet {
                    println!("\nLED round {round} — walk front + top");
                }
                walk(
                    &mut ctrl,
                    CH_FRONT,
                    cfg.front_leds.max(0) as usize,
                    "front",
                    step_ms,
                    &stop,
                    &spi_present_busy,
                    &stats,
                    cfg.quiet,
                );
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                walk(
                    &mut ctrl,
                    CH_TOP,
                    cfg.top_leds.max(0) as usize,
                    "top",
                    step_ms,
                    &stop,
                    &spi_present_busy,
                    &stats,
                    cfg.quiet,
                );
            }
            LedMode::Rainbow => {
                rainbow(&mut ctrl, &cfg, &stop, &spi_present_busy, &stats, 0);
            }
            LedMode::Steady => {
                fill_all(&mut ctrl, rgb(255, 255, 255));
                let _ = render(&mut ctrl, &stats, &spi_present_busy);
                sleep_ms(step_ms, &stop);
            }
            LedMode::Solid => {
                fill_all(&mut ctrl, rgb(255, 255, 255));
                let _ = render(&mut ctrl, &stats, &spi_present_busy);
                // Hold the current; no DMA traffic until stop.
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(STOP_POLL);
                }
                break;
            }
            LedMode::Off => {
                fill_all(&mut ctrl, rgb(0, 0, 0));
                let _ = render(&mut ctrl, &stats, &spi_present_busy);
                sleep_ms(step_ms, &stop);
            }
        }
    }

    fill_all(&mut ctrl, rgb(0, 0, 0));
    let _ = render(&mut ctrl, &stats, &spi_present_busy);
    println!("LEDs: cleared");
}

fn walk(
    ctrl: &mut Controller,
    channel: usize,
    count: usize,
    label: &str,
    step_ms: u64,
    stop: &AtomicBool,
    spi_present_busy: &AtomicBool,
    stats: &LedStats,
    quiet: bool,
) {
    for index in 0..count {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        fill_all(ctrl, rgb(0, 0, 0));
        let color = step_color(index);
        ctrl.leds_mut(channel)[index] = color;
        if let Err(e) = render(ctrl, stats, spi_present_busy) {
            stats.errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("LED render failed on {label} LED {index}: {e}");
        }
        if !quiet {
            let (r, g, b) = rgb_components(color);
            println!(
                "  {label} LED {index:>2}/{count}  {name}  (R={r} G={g} B={b})",
                name = color_name(index)
            );
        }
        sleep_ms(step_ms, stop);
    }
}

/// All LEDs lit: sequential hue along each strip, scrolling for `secs`
/// (`0` = until stop).
fn rainbow(
    ctrl: &mut Controller,
    cfg: &LedConfig,
    stop: &AtomicBool,
    spi_present_busy: &AtomicBool,
    stats: &LedStats,
    secs: u64,
) {
    let frame_ms = cfg.step_ms();
    let started = Instant::now();
    let mut phase: u16 = 0;

    while !stop.load(Ordering::Relaxed) {
        if secs != 0 && started.elapsed() >= Duration::from_secs(secs) {
            break;
        }
        for ch in ctrl.channels() {
            let count = ctrl.leds(ch).len();
            if count == 0 {
                continue;
            }
            let leds = ctrl.leds_mut(ch);
            for (i, led) in leds.iter_mut().enumerate() {
                let hue = ((phase as usize) + (i * 360 / count)) % 360;
                *led = hsv_to_rgb(hue as u16, 255, 255);
            }
        }
        if let Err(e) = render(ctrl, stats, spi_present_busy) {
            stats.errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("LED rainbow render failed: {e}");
        }
        phase = phase.wrapping_add(8);
        sleep_ms(frame_ms, stop);
    }
}

fn render(
    ctrl: &mut Controller,
    stats: &LedStats,
    spi_present_busy: &AtomicBool,
) -> Result<(), String> {
    if spi_present_busy.load(Ordering::Relaxed) {
        stats.overlap_renders.fetch_add(1, Ordering::Relaxed);
    }
    let started = Instant::now();
    ctrl.render().map_err(|e| format!("{e:?}"))?;
    let us = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
    stats.renders.fetch_add(1, Ordering::Relaxed);
    stats.max_render_us.fetch_max(us, Ordering::Relaxed);
    Ok(())
}

fn fill_all(ctrl: &mut Controller, color: RawColor) {
    for ch in ctrl.channels() {
        for led in ctrl.leds_mut(ch) {
            *led = color;
        }
    }
}

fn sleep_ms(ms: u64, stop: &AtomicBool) {
    let step = Duration::from_millis(ms);
    let start = Instant::now();
    while start.elapsed() < step {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(STOP_POLL.min(step));
    }
}

fn step_color(index: usize) -> RawColor {
    match index % 3 {
        0 => rgb(255, 0, 0), // red
        1 => rgb(0, 255, 0), // green
        _ => rgb(0, 0, 255), // blue
    }
}

fn color_name(index: usize) -> &'static str {
    match index % 3 {
        0 => "red",
        1 => "green",
        _ => "blue",
    }
}

/// Logical RGB packed as ws2811 0xWWRRGGBB little-endian [B, G, R, W].
/// `StripType::Ws2811Grb` handles wire order.
fn rgb(r: u8, g: u8, b: u8) -> RawColor {
    [b, g, r, 0]
}

fn rgb_components(color: RawColor) -> (u8, u8, u8) {
    (color[2], color[1], color[0])
}

fn hsv_to_rgb(h: u16, s: u8, v: u8) -> RawColor {
    let h = h % 360;
    if s == 0 {
        return rgb(v, v, v);
    }
    let s = s as u16;
    let v = v as u16;
    let region = h / 60;
    let remainder = (h % 60) * 255 / 60;
    let p = (v * (255 - s)) / 255;
    let q = (v * (255 - (s * remainder) / 255)) / 255;
    let t = (v * (255 - (s * (255 - remainder)) / 255)) / 255;
    let (r, g, b) = match region {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    rgb(r as u8, g as u8, b as u8)
}

fn pct_to_brightness(pct: u8) -> u8 {
    ((pct as u16).saturating_mul(255) / 100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_from_cli_names() {
        for (name, mode) in [
            ("cycle", LedMode::Cycle),
            ("walk", LedMode::Walk),
            ("rainbow", LedMode::Rainbow),
            ("steady", LedMode::Steady),
            ("solid", LedMode::Solid),
            ("off", LedMode::Off),
        ] {
            assert_eq!(LedMode::parse(name), Some(mode));
            assert_eq!(mode.label(), name);
        }
        assert_eq!(LedMode::parse("rainbow!"), None);
    }

    #[test]
    fn rgb_is_bgrw_for_ws2811_grb() {
        assert_eq!(rgb(255, 96, 0), [0, 96, 255, 0]);
        assert_eq!(rgb_components(rgb(1, 2, 3)), (1, 2, 3));
    }

    #[test]
    fn brightness_percent_maps_to_byte() {
        assert_eq!(pct_to_brightness(0), 0);
        assert_eq!(pct_to_brightness(100), 255);
        assert_eq!(pct_to_brightness(DEFAULT_BRIGHTNESS_PCT), 25);
        assert_eq!(pct_to_brightness(30), 76);
    }

    #[test]
    fn step_colours_cycle_red_green_blue() {
        assert_eq!(step_color(0), rgb(255, 0, 0));
        assert_eq!(step_color(1), rgb(0, 255, 0));
        assert_eq!(step_color(2), rgb(0, 0, 255));
        assert_eq!(step_color(3), rgb(255, 0, 0));
    }

    #[test]
    fn hsv_primaries() {
        assert_eq!(hsv_to_rgb(0, 255, 255), rgb(255, 0, 0));
        assert_eq!(hsv_to_rgb(120, 255, 255), rgb(0, 255, 0));
        assert_eq!(hsv_to_rgb(240, 255, 255), rgb(0, 0, 255));
    }
}
