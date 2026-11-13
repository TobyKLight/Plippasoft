// see led_test/pinout.md for hardware setup

use rs_ws281x::{ChannelBuilder, ControllerBuilder, RawColor, StripType};
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static RUNNING: AtomicBool = AtomicBool::new(true);

/// Front panel data (stream 1) — GPIO 13 / PWM1 → must be ws2811 channel 1.
const PIN_FRONT: i32 = 13;
const CH_FRONT: usize = 1;
/// Top / basetop panel data (stream 2) — GPIO 12 / PWM0 → must be ws2811 channel 0.
const PIN_TOP: i32 = 12;
const CH_TOP: usize = 0;

const DEFAULT_FRONT_LEDS: i32 = 33;
const DEFAULT_TOP_LEDS: i32 = 30;
const DEFAULT_BRIGHTNESS_PCT: u8 = 30;
const DEFAULT_FPS: u64 = 30;
const RAINBOW_SECS: u64 = 3;

struct AppConfig {
    front_leds: i32,
    top_leds: i32,
    brightness_pct: u8,
    fps: u64,
    step_ms: Option<u64>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            front_leds: DEFAULT_FRONT_LEDS,
            top_leds: DEFAULT_TOP_LEDS,
            brightness_pct: DEFAULT_BRIGHTNESS_PCT,
            fps: DEFAULT_FPS,
            step_ms: None,
        }
    }
}

impl AppConfig {
    fn step_ms(&self) -> u64 {
        self.step_ms.unwrap_or_else(|| fps_to_step_ms(self.fps))
    }
}

fn fps_to_step_ms(fps: u64) -> u64 {
    1000 / fps.max(1)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ctrlc::set_handler(|| RUNNING.store(false, Ordering::SeqCst))?;

    let app = parse_args();
    let brightness = pct_to_brightness(app.brightness_pct);
    let step_ms = app.step_ms();

    if app.front_leds <= 0 && app.top_leds <= 0 {
        return Err("at least one of --front-leds or --top-leds must be > 0".into());
    }

    println!(
        "WS2811 GRB LED test — stream 1: front GPIO {PIN_FRONT} ({} LEDs), stream 2: top GPIO {PIN_TOP} ({} LEDs)",
        app.front_leds, app.top_leds
    );
    println!(
        "Brightness {}%, {} fps ({} ms/step) — Ctrl-C to exit and clear",
        app.brightness_pct, app.fps, step_ms
    );

    // rpi_ws281x: channel 0 = PWM0 (GPIO 12/18), channel 1 = PWM1 (GPIO 13/19).
    let mut controller = ControllerBuilder::new()
        .freq(800_000)
        .dma(10)
        .channel(
            CH_TOP,
            ChannelBuilder::new()
                .pin(PIN_TOP)
                .count(app.top_leds.max(0))
                .strip_type(StripType::Ws2811Grb)
                .brightness(brightness)
                .build(),
        )
        .channel(
            CH_FRONT,
            ChannelBuilder::new()
                .pin(PIN_FRONT)
                .count(app.front_leds.max(0))
                .strip_type(StripType::Ws2811Grb)
                .brightness(brightness)
                .build(),
        )
        .build()
        .map_err(|e| format!("ws2811 init failed: {e:?} (try sudo)"))?;

    clear_all(&mut controller);
    controller.render().map_err(|e| format!("render failed: {e:?}"))?;

    if app.front_leds > 0 {
        println!("\nStream 1 — front panel (GPIO {PIN_FRONT})");
        walk_stream(
            &mut controller,
            CH_FRONT,
            app.front_leds as usize,
            "front",
            step_ms,
        )?;
    }

    if RUNNING.load(Ordering::SeqCst) && app.top_leds > 0 {
        println!("\nStream 2 — top panel (GPIO {PIN_TOP})");
        walk_stream(
            &mut controller,
            CH_TOP,
            app.top_leds as usize,
            "top",
            step_ms,
        )?;
    }

    if RUNNING.load(Ordering::SeqCst) {
        println!("\nRainbow — all LEDs, full saturation, {RAINBOW_SECS}s");
        run_rainbow(&mut controller, app.fps)?;
    }

    clear_all(&mut controller);
    controller.render().map_err(|e| format!("clear render failed: {e:?}"))?;
    println!("Done.");
    Ok(())
}

fn walk_stream(
    controller: &mut rs_ws281x::Controller,
    channel: usize,
    count: usize,
    label: &str,
    step_ms: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    for index in 0..count {
        if !RUNNING.load(Ordering::SeqCst) {
            break;
        }

        clear_all(controller);
        let color = step_color(index);
        let leds = controller.leds_mut(channel);
        leds[index] = color;

        controller
            .render()
            .map_err(|e| format!("render failed on {label} LED {index}: {e:?}"))?;

        let (r, g, b) = rgb_components(color);
        println!(
            "  {label} LED {index:>2}/{count}  {name}  (R={r} G={g} B={b})",
            name = color_name(index),
        );

        sleep_until_next(step_ms);
    }
    Ok(())
}

/// All LEDs lit: sequential hue along each strip, scrolling for `RAINBOW_SECS`.
fn run_rainbow(
    controller: &mut rs_ws281x::Controller,
    fps: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let frame_ms = fps_to_step_ms(fps);
    let duration = Duration::from_secs(RAINBOW_SECS);
    let started = Instant::now();
    let mut phase: u16 = 0;

    while RUNNING.load(Ordering::SeqCst) && started.elapsed() < duration {
        for ch in controller.channels() {
            let count = controller.leds(ch).len();
            if count == 0 {
                continue;
            }
            let leds = controller.leds_mut(ch);
            for (i, led) in leds.iter_mut().enumerate() {
                let hue = ((phase as usize) + (i * 360 / count)) % 360;
                *led = hsv_to_rgb(hue as u16, 255, 255);
            }
        }

        controller
            .render()
            .map_err(|e| format!("rainbow render failed: {e:?}"))?;

        phase = phase.wrapping_add(8);
        sleep_until_next(frame_ms);
    }
    Ok(())
}

fn clear_all(controller: &mut rs_ws281x::Controller) {
    for ch in controller.channels() {
        for led in controller.leds_mut(ch) {
            *led = [0, 0, 0, 0];
        }
    }
}

/// Pure R / G / B for each LED index (cycles every three positions).
fn step_color(index: usize) -> RawColor {
    match index % 3 {
        0 => rgb(255, 0, 0),   // red
        1 => rgb(0, 255, 0),   // green
        _ => rgb(0, 0, 255),   // blue
    }
}

fn color_name(index: usize) -> &'static str {
    match index % 3 {
        0 => "red",
        1 => "green",
        _ => "blue",
    }
}

/// Pack logical RGB as ws2811 0xWWRRGGBB (little-endian [B,G,R,W]).
/// `StripType::Ws2811Grb` handles wire order.
fn rgb(r: u8, g: u8, b: u8) -> RawColor {
    [b, g, r, 0]
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

fn rgb_components(color: RawColor) -> (u8, u8, u8) {
    (color[2], color[1], color[0])
}

fn pct_to_brightness(pct: u8) -> u8 {
    ((pct as u16).saturating_mul(255) / 100) as u8
}

fn sleep_until_next(step_ms: u64) {
    let step = Duration::from_millis(step_ms);
    let start = std::time::Instant::now();
    while start.elapsed() < step {
        if !RUNNING.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn parse_args() -> AppConfig {
    let mut app = AppConfig::default();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--front-leds" => {
                app.front_leds = args.next().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_FRONT_LEDS);
            }
            "--top-leds" => {
                app.top_leds = args.next().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_TOP_LEDS);
            }
            "--brightness" => {
                app.brightness_pct = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_BRIGHTNESS_PCT)
                    .min(100);
            }
            "--fps" => {
                app.fps = args.next().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_FPS).max(1);
            }
            "--step-ms" => {
                app.step_ms = Some(args.next().and_then(|v| v.parse().ok()).unwrap_or(0).max(1));
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => eprintln!("unknown arg: {other} (try --help)"),
        }
    }
    app
}

fn print_usage() {
    println!(
        "usage: poc2_led_test [options]\n\
         \n\
         Steps through each LED on two WS2811 GRB streams (one lit at a time).\n\
         Colours cycle red → green → blue per LED index. Stream 1 (front) runs first,\n\
         then stream 2 (top). Then all LEDs run a sequential rainbow for {RAINBOW_SECS}s.\n\
         Requires root (sudo) for PWM/DMA access.\n\
         \n\
         options:\n\
           --front-leds N   front panel LED count (default {DEFAULT_FRONT_LEDS}, GPIO {PIN_FRONT})\n\
           --top-leds N     top panel LED count (default {DEFAULT_TOP_LEDS}, GPIO {PIN_TOP})\n\
           --brightness P   brightness percent 0–100 (default {DEFAULT_BRIGHTNESS_PCT})\n\
           --fps N          steps per second (default {DEFAULT_FPS})\n\
           --step-ms N      override --fps with explicit ms per step\n\
         \n\
         Wiring: see eval/poc2/led_test/pinout.md"
    );
}
