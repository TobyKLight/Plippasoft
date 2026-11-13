//! Full-screen bouncing-balls SPI animation for ILI9341 (POC2 wiring).
//!
//! One vertical-strip bounce pattern is simulated, then stamped across the
//! width with different hues and phase / speed offsets so the whole panel moves.
//! see screen_anim_ILI9341_bouncy/pinout.md for hardware setup

mod gpio_out;
mod leds;

use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};
use gpio_out::{open_output, pulse_display_reset, Backlight};
use leds::{LedConfig, LedHandle, LedMode};
use linux_embedded_hal::spidev::{SpiModeFlags, SpidevOptions};
use linux_embedded_hal::{Delay, SpidevDevice};
use mipidsi::{
    interface::SpiInterface,
    models::ILI9341Rgb565,
    options::{ColorInversion, ColorOrder, Orientation, Rotation},
    Builder,
};
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static RUNNING: AtomicBool = AtomicBool::new(true);

const SPI_DEV: &str = "/dev/spidev0.0";
const PIN_DC: u32 = 24;
const PIN_RST: u32 = 25;
const PIN_BLK: u32 = 16;

const WIDTH: u16 = 240;
const HEIGHT: u16 = 320;

/// How many horizontal copies of the strip animation.
const STRIP_COUNT: usize = 6;
/// Balls defined once for the strip template.
const BALLS_PER_STRIP: usize = 3;

#[derive(Clone, Copy)]
struct DisplayConfig {
    width: u16,
    height: u16,
    offset_x: u16,
    offset_y: u16,
    rotation: Rotation,
    /// Mirror across the display’s horizontal axis (fixes backwards text on POC2).
    mirrored: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            width: WIDTH,
            height: HEIGHT,
            offset_x: 0,
            offset_y: 0,
            // Match boards/prototypes/plippa-D-poc2/config/board.toml
            rotation: Rotation::Deg90,
            mirrored: true,
        }
    }
}

struct AppConfig {
    display: DisplayConfig,
    spi_dev: String,
    spi_no_cs: bool,
    spi_mode: SpiModeFlags,
    blk_active_low: bool,
    blk_float: bool,
    color_order: ColorOrder,
    invert: ColorInversion,
    spi_speed_hz: u32,
    /// `--leds` / `--with-leds`: run a WS2811 animation alongside the SPI one.
    leds_enabled: bool,
    led: LedConfig,
    /// `--paint`: what to draw. `anim` is the bouncy test; `ui` / `ui-scroll` mimic how
    /// `plippa-ui` paints (text-heavy, mostly static, event-driven) so a slow present can be
    /// judged against the same content the real UI shows.
    paint: PaintMode,
    /// `--paint-hz N`: present at N Hz instead of as fast as the panel allows.
    /// `0` (default) = every composed frame.
    paint_hz: u32,
}

/// What the eval draws each frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaintMode {
    /// Bouncy balls: continuous, low-contrast, smoothly moving content.
    Anim,
    /// A static text/border screen whose only changes are a clock digit, a progress bar and a
    /// moving focus rect — i.e. what `plippa-ui` puts on the panel between input events.
    Ui,
    /// Same layout, but every present shifts the whole list by one row: what the UI shows while
    /// an encoder spins (a large area changes between consecutive frames).
    UiScroll,
}

impl PaintMode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "anim" | "balls" => Some(Self::Anim),
            "ui" => Some(Self::Ui),
            "ui-scroll" => Some(Self::UiScroll),
            _ => None,
        }
    }
}

impl AppConfig {
    /// `None` when every composed frame should be presented.
    fn paint_interval(&self) -> Option<Duration> {
        (self.paint_hz > 0).then(|| Duration::from_millis(1000 / u64::from(self.paint_hz.max(1))))
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            display: DisplayConfig::default(),
            spi_dev: SPI_DEV.to_string(),
            spi_no_cs: false,
            spi_mode: SpiModeFlags::SPI_MODE_0,
            blk_active_low: false,
            blk_float: false,
            color_order: ColorOrder::Rgb,
            invert: ColorInversion::Normal,
            // Animation-oriented default; drop with --spi-speed if the panel glitches.
            spi_speed_hz: 40_000_000,
            leds_enabled: false,
            led: LedConfig::default(),
            paint: PaintMode::Anim,
            paint_hz: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct BallTemplate {
    /// Horizontal centre inside the strip, as a fraction of strip width (0..1).
    x_frac: f32,
    y: f32,
    vy: f32,
    radius: f32,
}

#[derive(Clone, Copy)]
struct Ball {
    x: f32,
    y: f32,
    vy: f32,
    radius: f32,
    color: Rgb565,
}

fn spi_flags(base_mode: SpiModeFlags, no_cs: bool) -> SpiModeFlags {
    if no_cs {
        base_mode | SpiModeFlags::SPI_NO_CS
    } else {
        base_mode
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ctrlc::set_handler(|| RUNNING.store(false, Ordering::SeqCst))?;

    let app = parse_args();
    let cfg = app.display;
    let (fb_w, fb_h) = logical_size(cfg);

    println!(
        "ILI9341 bouncy native {}x{} → logical {}x{} @ {}° mirrored={} offset ({}, {}), SPI {} @ {} Hz",
        cfg.width,
        cfg.height,
        fb_w,
        fb_h,
        cfg.rotation.degree(),
        cfg.mirrored,
        cfg.offset_x,
        cfg.offset_y,
        app.spi_dev,
        app.spi_speed_hz
    );
    println!(
        "{STRIP_COUNT} strips × {BALLS_PER_STRIP} balls — Ctrl-C to exit"
    );

    let backlight = if app.blk_float {
        println!("BLK left floating");
        None
    } else {
        let b = Backlight::open(PIN_BLK, app.blk_active_low)?;
        b.set_on()?;
        Some(b)
    };
    let mut delay = Delay;

    let mut spi = SpidevDevice::open(&app.spi_dev).map_err(|e| {
        format!(
            "cannot open {} ({e}) — enable SPI with dtoverlay=spi0-1cs,no_miso (see boardSetup.md)",
            app.spi_dev
        )
    })?;
    spi.configure(
        &SpidevOptions::new()
            .bits_per_word(8)
            .max_speed_hz(app.spi_speed_hz)
            .mode(spi_flags(app.spi_mode, app.spi_no_cs))
            .build(),
    )?;

    pulse_display_reset(PIN_RST)?;
    let mut dc = open_output(PIN_DC, "dc", false)?;
    let mut rst = open_output(PIN_RST, "rst", true)?;
    // Must stay ≤ kernel spidev bufsiz (often 4096) or transfers return EMSGSIZE.
    let mut buffer = [0_u8; 4096];
    let di = SpiInterface::new(spi, &mut dc, &mut buffer);

    let mut orientation = Orientation::new().rotate(cfg.rotation);
    if cfg.mirrored {
        orientation = orientation.flip_horizontal();
    }

    let mut display = Builder::new(ILI9341Rgb565, di)
        .display_size(cfg.width, cfg.height)
        .display_offset(cfg.offset_x, cfg.offset_y)
        .orientation(orientation)
        .color_order(app.color_order)
        .invert_colors(app.invert)
        .reset_pin(&mut rst)
        .init(&mut delay)
        .map_err(|e| format!("display init failed: {e:?}"))?;

    display
        .clear(Rgb565::BLACK)
        .map_err(|e| format!("startup fill failed: {e:?}"))?;
    println!("Display init OK — running bounce loop (stderr hitch log: frame >50ms)");

    // Optional WS2811 animation on its own thread — same split as plippa-ui's
    // display worker + LED worker, so flicker under LED DMA can be reproduced.
    let spi_present_busy = Arc::new(AtomicBool::new(false));
    let mut led_handle: Option<LedHandle> = if app.leds_enabled {
        match leds::spawn(
            app.led.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::clone(&spi_present_busy),
        ) {
            Ok(h) => Some(h),
            Err(e) => {
                let _ = display.clear(Rgb565::BLACK);
                if let Some(b) = &backlight {
                    let _ = b.set_off();
                }
                return Err(e.into());
            }
        }
    } else {
        None
    };

    let templates = ball_templates();
    let mut strips = spawn_strips(fb_w, fb_h, &templates);
    let mut fb = vec![Rgb565::BLACK; fb_w * fb_h];
    // Reused for 2× FPS glyph scale (avoids per-frame alloc).
    let scratch_w = 12 * FONT_10X20.character_size.width as usize;
    let scratch_h = FONT_10X20.character_size.height as usize;
    let mut fps_scratch = vec![Rgb565::BLACK; scratch_w * scratch_h];

    let mut last = Instant::now();
    let mut fps_window = Instant::now();
    let mut frames: u32 = 0;
    let mut hitch_count: u64 = 0;
    let mut last_led_renders: u64 = 0;
    let mut sum_compose_us: u64 = 0;
    let mut sum_present_us: u64 = 0;
    let mut max_compose_us: u64 = 0;
    let mut max_present_us: u64 = 0;
    let mut fps_label = String::from("FPS --.-");
    let mut detail_label = String::from("c-- p--");
    let t0 = Instant::now();
    let paint_interval = app.paint_interval();
    let mut next_paint = Instant::now();
    // Counts presents, not compositions: in `--paint-hz` mode the clock/progress/focus advance
    // once per present, like the UI's 1 Hz tick.
    let mut paint_index: u64 = 0;

    if app.paint != PaintMode::Anim {
        let hz = paint_interval.map_or(0.0, |iv| 1000.0 / iv.as_millis().max(1) as f32);
        eprintln!("paint mode {:?} at {hz:.1} Hz", app.paint);
    }

    while RUNNING.load(Ordering::SeqCst) {
        if let Some(iv) = paint_interval {
            let now = Instant::now();
            if now < next_paint {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            next_paint = now + iv;
        }

        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f32().min(0.05);
        last = now;

        let t_compose0 = Instant::now();
        match app.paint {
            PaintMode::Anim => {
                step_strips(&mut strips, fb_h as f32, dt);
                render_frame(&mut fb, fb_w, fb_h, &strips);
            }
            PaintMode::Ui => draw_ui_frame(&mut fb, fb_w, fb_h, paint_index, 0),
            PaintMode::UiScroll => {
                draw_ui_frame(&mut fb, fb_w, fb_h, paint_index, paint_index as i32)
            }
        }
        paint_index += 1;
        draw_fps_overlay(
            &mut fb,
            fb_w,
            fb_h,
            &fps_label,
            &detail_label,
            &mut fps_scratch,
            scratch_w,
            scratch_h,
        );
        let compose_us = t_compose0.elapsed().as_micros() as u64;

        let t_present0 = Instant::now();
        spi_present_busy.store(true, Ordering::Relaxed);
        let present_result = present(&mut display, fb_w, fb_h, &fb);
        spi_present_busy.store(false, Ordering::Relaxed);
        present_result.map_err(|e| format!("present failed: {e:?}"))?;
        let present_us = t_present0.elapsed().as_micros() as u64;

        let frame_us = compose_us + present_us;
        sum_compose_us += compose_us;
        sum_present_us += present_us;
        max_compose_us = max_compose_us.max(compose_us);
        max_present_us = max_present_us.max(present_us);
        frames += 1;

        // Hard hitch: whole frame well above the ~35ms 40MHz SPI budget.
        if frame_us > 50_000 {
            hitch_count += 1;
            eprintln!(
                "HITCH t={:.2}s frame={:.1}ms compose={:.1}ms present={:.1}ms (#{})",
                t0.elapsed().as_secs_f32(),
                frame_us as f32 / 1000.0,
                compose_us as f32 / 1000.0,
                present_us as f32 / 1000.0,
                hitch_count
            );
        }

        // Update on-screen stats at ~5 Hz.
        if fps_window.elapsed() >= Duration::from_millis(200) {
            let secs = fps_window.elapsed().as_secs_f32().max(1e-3);
            let n = frames.max(1) as u64;
            let avg_c = sum_compose_us / n;
            let avg_p = sum_present_us / n;
            let (led_delta, led_max_us, led_overlap, led_errors) = match led_handle.as_ref() {
                Some(h) => {
                    let renders = h.stats.renders.load(Ordering::Relaxed);
                    let delta = renders.saturating_sub(last_led_renders);
                    last_led_renders = renders;
                    (
                        delta,
                        h.stats.max_render_us.load(Ordering::Relaxed),
                        h.stats.overlap_renders.load(Ordering::Relaxed),
                        h.stats.errors.load(Ordering::Relaxed),
                    )
                }
                None => (0, 0, 0, 0),
            };
            fps_label = format!("FPS {:4.1}", frames as f32 / secs);
            detail_label = if led_handle.is_some() {
                format!(
                    "c{:.0} p{:.0} H{} L{}",
                    avg_c as f32 / 1000.0,
                    avg_p as f32 / 1000.0,
                    hitch_count,
                    led_delta
                )
            } else {
                format!(
                    "c{:.0} p{:.0} H{}",
                    avg_c as f32 / 1000.0,
                    avg_p as f32 / 1000.0,
                    hitch_count
                )
            };
            eprintln!(
                "STAT t={:.1}s fps={:.1} compose_avg={:.1}ms present_avg={:.1}ms compose_max={:.1}ms present_max={:.1}ms hitches={}",
                t0.elapsed().as_secs_f32(),
                frames as f32 / secs,
                avg_c as f32 / 1000.0,
                avg_p as f32 / 1000.0,
                max_compose_us as f32 / 1000.0,
                max_present_us as f32 / 1000.0,
                hitch_count
            );
            if led_handle.is_some() {
                eprintln!(
                    "STATLED t={:.1}s led_renders={} led_max={:.1}ms led_overlap={} led_errors={}",
                    t0.elapsed().as_secs_f32(),
                    led_delta,
                    led_max_us as f32 / 1000.0,
                    led_overlap,
                    led_errors
                );
            }
            frames = 0;
            sum_compose_us = 0;
            sum_present_us = 0;
            max_compose_us = 0;
            max_present_us = 0;
            fps_window = Instant::now();
        }
    }

    if let Some(h) = led_handle.take() {
        let renders = h.stats.renders.load(Ordering::Relaxed);
        let max_us = h.stats.max_render_us.load(Ordering::Relaxed);
        let overlap = h.stats.overlap_renders.load(Ordering::Relaxed);
        let errors = h.stats.errors.load(Ordering::Relaxed);
        h.stop_and_join();
        println!(
            "LEDs: {renders} renders, max {:.1} ms, {overlap} started during an SPI present, {errors} errors",
            max_us as f32 / 1000.0
        );
    }
    display
        .clear(Rgb565::BLACK)
        .map_err(|e| format!("clear failed: {e:?}"))?;
    if let Some(b) = backlight {
        b.set_off()?;
    }
    println!("Done.");
    Ok(())
}

fn logical_size(cfg: DisplayConfig) -> (usize, usize) {
    match cfg.rotation {
        Rotation::Deg90 | Rotation::Deg270 => (cfg.height as usize, cfg.width as usize),
        _ => (cfg.width as usize, cfg.height as usize),
    }
}

/// Canonical bounce set for one strip (positions relative to strip geometry).
fn ball_templates() -> [BallTemplate; BALLS_PER_STRIP] {
    [
        BallTemplate {
            x_frac: 0.32,
            y: 40.0,
            vy: 110.0,
            radius: 11.0,
        },
        BallTemplate {
            x_frac: 0.55,
            y: 140.0,
            vy: -85.0,
            radius: 8.0,
        },
        BallTemplate {
            x_frac: 0.78,
            y: 220.0,
            vy: 145.0,
            radius: 14.0,
        },
    ]
}

fn spawn_strips(
    fb_w: usize,
    fb_h: usize,
    templates: &[BallTemplate; BALLS_PER_STRIP],
) -> Vec<Vec<Ball>> {
    let strip_w = fb_w as f32 / STRIP_COUNT as f32;
    let max_y = (fb_h as f32).max(1.0);
    (0..STRIP_COUNT)
        .map(|i| {
            let hue = ((i * 360) / STRIP_COUNT) as u8;
            // Phase / timing stagger: offset initial y and scale gravity response via vy.
            let phase = (i as f32) * 0.37;
            let speed = 0.75 + (i as f32) * 0.12;
            let tint = hsv_to_rgb565(hue, 255, 255);
            templates
                .iter()
                .enumerate()
                .map(|(bi, t)| {
                    let y_shift = phase * 90.0 + (bi as f32) * 17.0;
                    let span = (max_y - 2.0 * t.radius).max(1.0);
                    Ball {
                        x: (i as f32) * strip_w + t.x_frac * strip_w,
                        y: (t.y + y_shift).rem_euclid(span) + t.radius,
                        vy: t.vy * speed * if bi % 2 == 0 { 1.0 } else { -1.0 },
                        radius: t.radius,
                        color: tint,
                    }
                })
                .collect()
        })
        .collect()
}

fn step_strips(strips: &mut [Vec<Ball>], height: f32, dt: f32) {
    const GRAVITY: f32 = 420.0;
    const DAMP: f32 = 0.992;

    for balls in strips.iter_mut() {
        for b in balls.iter_mut() {
            b.vy += GRAVITY * dt;
            b.y += b.vy * dt;

            let min_y = b.radius;
            let max_y = height - b.radius;
            if b.y > max_y {
                b.y = max_y;
                b.vy = -b.vy.abs() * DAMP;
            } else if b.y < min_y {
                b.y = min_y;
                b.vy = b.vy.abs() * DAMP;
            }
        }
    }
}

fn render_frame(fb: &mut [Rgb565], w: usize, h: usize, strips: &[Vec<Ball>]) {
    fb.fill(Rgb565::new(2, 2, 6));

    // Soft strip gutters so the horizontal copies read clearly.
    let strip_w = w as f32 / STRIP_COUNT as f32;
    for i in 1..STRIP_COUNT {
        let x = (i as f32 * strip_w) as i32;
        for y in 0..h {
            let idx = y * w + x as usize;
            if idx < fb.len() {
                fb[idx] = Rgb565::new(4, 4, 10);
            }
        }
    }

    for balls in strips {
        for b in balls {
            fill_circle(
                fb,
                w,
                h,
                b.x as i32,
                b.y as i32,
                b.radius as i32,
                b.color,
            );
        }
    }
}

/// A `plippa-ui`-shaped screen: 1 px borders, a title bar, a text list with a focus ring, a
/// clock in the header and a progress bar in the footer.
///
/// This exists to answer "does the UI's *content* make a slow present look like flicker?".
/// The bouncy animation can be presented at 81 ms/frame without any visible artefact because
/// consecutive frames differ by a few pixels of ball edge, so the tear line has nothing to
/// show. Here every present still pushes a full 153.6 KB frame over the same SPI path, but the
/// content is high-contrast text on a static background — exactly the case where a tear (or a
/// partial GRAM update) is most visible.
fn draw_ui_frame(fb: &mut [Rgb565], w: usize, h: usize, present: u64, scroll: i32) {
    let white = Rgb565::WHITE;
    let dim = Rgb565::new(12, 24, 20);
    fb.fill(Rgb565::new(2, 2, 6));
    let mut t = FbTarget { buf: fb, w, h };

    // 1 px frame + title bar: the thin bright lines a tear cuts straight through.
    let _ = Rectangle::new(Point::zero(), Size::new(w as u32, h as u32))
        .into_styled(PrimitiveStyle::with_stroke(white, 1))
        .draw(&mut t);
    let _ = Rectangle::new(Point::zero(), Size::new(w as u32, 26))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::new(4, 8, 12)))
        .draw(&mut t);
    let _ = Rectangle::new(Point::new(0, 26), Size::new(w as u32, 1))
        .into_styled(PrimitiveStyle::with_fill(white))
        .draw(&mut t);
    ui_text(&mut t, 10, 3, "PLIPPA", white);
    // The clock: the only thing that changes on an otherwise static screen (1 Hz in the UI).
    let secs = 10 + (present % 50) as i32;
    ui_text(
        &mut t,
        w as i32 - 78,
        3,
        &format!("00:{secs:02}"),
        Rgb565::new(31, 63, 0),
    );

    // Text list. `scroll` shifts every row, which is what an encoder turn does in the UI.
    let row_h = 24u32;
    let rows = ((h as u32).saturating_sub(26 + 30) / row_h).max(1);
    let focus = (present / 6) as u32 % rows;
    for i in 0..rows {
        let idx = (i as i32 + scroll).rem_euclid(9);
        let y = 30 + i * row_h;
        let fill = if i % 2 == 0 {
            Rgb565::new(3, 5, 9)
        } else {
            Rgb565::new(2, 4, 8)
        };
        let _ = Rectangle::new(Point::new(8, y as i32), Size::new(w as u32 - 16, row_h - 2))
            .into_styled(PrimitiveStyle::with_fill(fill))
            .draw(&mut t);
        let label = format!("TRACK {:02} TITLE HERE", idx + 1);
        ui_text(&mut t, 14, (y + 2) as i32, &label, white);
        let sub = format!("artist name {}", idx + 1);
        ui_text(&mut t, 14 + 150, (y + 2) as i32, &sub, dim);
        if i == focus {
            let _ = Rectangle::new(Point::new(8, y as i32), Size::new(w as u32 - 16, row_h - 2))
                .into_styled(PrimitiveStyle::with_stroke(white, 2))
                .draw(&mut t);
        }
    }

    // Footer: a timer + a progress bar that genuinely advances (the UI's 1 s position update).
    let by = h as i32 - 20;
    let _ = Rectangle::new(Point::new(0, by - 6), Size::new(w as u32, 1))
        .into_styled(PrimitiveStyle::with_fill(white))
        .draw(&mut t);
    ui_text(&mut t, 10, by + 2, "01:23", white);
    ui_text(&mut t, w as i32 - 60, by + 2, "04:56", dim);
    let track_x = 70u32;
    let track_w = (w as u32).saturating_sub(track_x + 80);
    let _ = Rectangle::new(Point::new(track_x as i32, by + 4), Size::new(track_w, 8))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::new(6, 12, 18)))
        .draw(&mut t);
    let done = (track_w * (present % 40) as u32) / 40;
    let _ = Rectangle::new(Point::new(track_x as i32, by + 4), Size::new(done, 8))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::new(0, 40, 31)))
        .draw(&mut t);
}

fn ui_text(target: &mut FbTarget<'_>, x: i32, y: i32, s: &str, color: Rgb565) {
    let _ = Text::with_baseline(
        s,
        Point::new(x, y),
        MonoTextStyle::new(&FONT_10X20, color),
        Baseline::Top,
    )
    .draw(target);
}

/// Top-left FPS badge: solid plate so bouncing balls cannot obscure the digits.
fn draw_fps_overlay(
    fb: &mut [Rgb565],
    w: usize,
    h: usize,
    fps_label: &str,
    detail_label: &str,
    scratch: &mut [Rgb565],
    scratch_w: usize,
    scratch_h: usize,
) {
    const PAD: i32 = 4;
    const SCALE: u32 = 2; // FONT_10X20 → ~20×40 glyphs for FPS line
    const SCALE_DETAIL: u32 = 1;

    let line1_w = (fps_label.len() as u32) * FONT_10X20.character_size.width * SCALE;
    let line2_w = (detail_label.len() as u32) * FONT_10X20.character_size.width * SCALE_DETAIL;
    let text_w = line1_w.max(line2_w);
    let line1_h = FONT_10X20.character_size.height * SCALE;
    let line2_h = FONT_10X20.character_size.height * SCALE_DETAIL;
    let box_w = text_w + (PAD as u32) * 2;
    let box_h = line1_h + line2_h + (PAD as u32) * 3;
    let origin = Point::new(4, 4);

    let mut target = FbTarget { buf: fb, w, h };
    Rectangle::new(origin, Size::new(box_w, box_h))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::new(0, 0, 0)))
        .draw(&mut target)
        .ok();
    Rectangle::new(origin, Size::new(box_w, box_h))
        .into_styled(PrimitiveStyle::with_stroke(Rgb565::WHITE, 1))
        .draw(&mut target)
        .ok();

    blit_text_scaled(
        fb,
        w,
        h,
        fps_label,
        origin.x + PAD,
        origin.y + PAD,
        SCALE,
        Rgb565::YELLOW,
        scratch,
        scratch_w,
        scratch_h,
    );
    blit_text_scaled(
        fb,
        w,
        h,
        detail_label,
        origin.x + PAD,
        origin.y + PAD + line1_h as i32 + PAD,
        SCALE_DETAIL,
        Rgb565::CYAN,
        scratch,
        scratch_w,
        scratch_h,
    );
}

fn blit_text_scaled(
    fb: &mut [Rgb565],
    w: usize,
    h: usize,
    label: &str,
    dst_x: i32,
    dst_y: i32,
    scale: u32,
    color: Rgb565,
    scratch: &mut [Rgb565],
    scratch_w: usize,
    scratch_h: usize,
) {
    let need = label.len() * FONT_10X20.character_size.width as usize * scratch_h;
    if need > scratch.len() || scratch_w == 0 || scratch_h == 0 {
        return;
    }
    scratch[..need].fill(Rgb565::BLACK);
    {
        let mut scratch_target = FbTarget {
            buf: scratch,
            w: scratch_w,
            h: scratch_h,
        };
        Text::with_baseline(
            label,
            Point::zero(),
            MonoTextStyle::new(&FONT_10X20, color),
            Baseline::Top,
        )
        .draw(&mut scratch_target)
        .ok();
    }

    let glyph_w = label.len() * FONT_10X20.character_size.width as usize;
    for sy in 0..scratch_h {
        for sx in 0..glyph_w.min(scratch_w) {
            let px = scratch[sy * scratch_w + sx];
            if px == Rgb565::BLACK {
                continue;
            }
            for dy in 0..scale as i32 {
                for dx in 0..scale as i32 {
                    let x = dst_x + sx as i32 * scale as i32 + dx;
                    let y = dst_y + sy as i32 * scale as i32 + dy;
                    if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
                        fb[y as usize * w + x as usize] = px;
                    }
                }
            }
        }
    }
}

struct FbTarget<'a> {
    buf: &'a mut [Rgb565],
    w: usize,
    h: usize,
}

impl DrawTarget for FbTarget<'_> {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(p, color) in pixels {
            if p.x >= 0 && p.y >= 0 {
                let x = p.x as usize;
                let y = p.y as usize;
                if x < self.w && y < self.h {
                    self.buf[y * self.w + x] = color;
                }
            }
        }
        Ok(())
    }
}

impl OriginDimensions for FbTarget<'_> {
    fn size(&self) -> Size {
        Size::new(self.w as u32, self.h as u32)
    }
}

fn fill_circle(fb: &mut [Rgb565], w: usize, h: usize, cx: i32, cy: i32, r: i32, color: Rgb565) {
    if r <= 0 {
        return;
    }
    let r2 = r * r;
    let y0 = (cy - r).max(0);
    let y1 = (cy + r).min(h as i32 - 1);
    let x0 = (cx - r).max(0);
    let x1 = (cx + r).min(w as i32 - 1);
    for y in y0..=y1 {
        let dy = y - cy;
        for x in x0..=x1 {
            let dx = x - cx;
            if dx * dx + dy * dy <= r2 {
                fb[y as usize * w + x as usize] = color;
            }
        }
    }
}

fn present<D>(
    display: &mut D,
    w: usize,
    h: usize,
    fb: &[Rgb565],
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let area = Rectangle::new(Point::zero(), Size::new(w as u32, h as u32));
    display.fill_contiguous(&area, fb.iter().copied())
}

fn hsv_to_rgb565(h: u8, s: u8, v: u8) -> Rgb565 {
    if s == 0 {
        return Rgb565::new(v >> 3, v >> 2, v >> 3);
    }
    let h = h as u16;
    let s = s as u16;
    let v = v as u16;
    let c = (v * s) / 255;
    let x = (c * (255 - ((h * 6) % 510).abs_diff(255))) / 255;
    let m = v - c;
    let (r, g, b) = match h / 60 {
        0 => (c, x, 0),
        1 => (x, c, 0),
        2 => (0, c, x),
        3 => (0, x, c),
        4 => (x, 0, c),
        _ => (c, 0, x),
    };
    Rgb565::new(
        ((r + m) >> 3) as u8,
        ((g + m) >> 2) as u8,
        ((b + m) >> 3) as u8,
    )
}

fn parse_args() -> AppConfig {
    let mut app = AppConfig::default();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--offset-x" => {
                app.display.offset_x = args.next().and_then(|v| v.parse().ok()).unwrap_or(0)
            }
            "--offset-y" => {
                app.display.offset_y = args.next().and_then(|v| v.parse().ok()).unwrap_or(0)
            }
            "--rotation" => {
                app.display.rotation = match args.next().as_deref() {
                    Some("90") => Rotation::Deg90,
                    Some("180") => Rotation::Deg180,
                    Some("270") => Rotation::Deg270,
                    _ => Rotation::Deg0,
                };
            }
            "--mirrored" => app.display.mirrored = true,
            "--no-mirror" => app.display.mirrored = false,
            "--spi-dev" => {
                app.spi_dev = args.next().unwrap_or_else(|| SPI_DEV.to_string());
            }
            "--spi-no-cs" => app.spi_no_cs = true,
            "--spi-ce" => app.spi_no_cs = false,
            "--spi-mode" => {
                app.spi_mode = match args.next().as_deref() {
                    Some("3") => SpiModeFlags::SPI_MODE_3,
                    _ => SpiModeFlags::SPI_MODE_0,
                };
            }
            "--blk-float" => app.blk_float = true,
            "--blk-active-low" => app.blk_active_low = true,
            "--invert" => app.invert = ColorInversion::Inverted,
            "--no-invert" => app.invert = ColorInversion::Normal,
            "--color-order" => {
                app.color_order = match args.next().as_deref() {
                    Some("bgr") => ColorOrder::Bgr,
                    _ => ColorOrder::Rgb,
                };
            }
            "--spi-speed" => {
                app.spi_speed_hz = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(40_000_000);
            }
            "--leds" | "--with-leds" => app.leds_enabled = true,
            "--led-front" => {
                app.led.front_leds = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(leds::DEFAULT_FRONT_LEDS);
            }
            "--led-top" => {
                app.led.top_leds = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(leds::DEFAULT_TOP_LEDS);
            }
            "--led-brightness" => {
                app.led.brightness_pct = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(leds::DEFAULT_BRIGHTNESS_PCT)
                    .min(100);
            }
            "--led-fps" => {
                app.led.fps = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(leds::DEFAULT_FPS)
                    .max(1);
            }
            "--led-dma" => {
                app.led.dma = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(leds::DEFAULT_DMA);
            }
            "--led-mode" => match args.next().as_deref().and_then(LedMode::parse) {
                Some(mode) => app.led.mode = mode,
                None => eprintln!(
                    "--led-mode expects cycle|walk|rainbow|steady|solid|off (default cycle)"
                ),
            },
            "--led-quiet" => app.led.quiet = true,
            "--paint" => match args.next().as_deref().and_then(PaintMode::parse) {
                Some(mode) => app.paint = mode,
                None => eprintln!("--paint expects anim|ui|ui-scroll (default anim)"),
            },
            "--paint-hz" => {
                app.paint_hz = args.next().and_then(|v| v.parse().ok()).unwrap_or(0);
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
        "usage: poc2_screen_anim_ILI9341_bouncy [options]\n\
         \n\
         Full-screen bouncing balls: one strip pattern copied across the width\n\
         with different colours and timing. On-screen FPS badge (updates ~5 Hz).\n\
         Defaults match plippa-D-poc2 board.toml: rot 90 + mirrored.\n\
         \n\
         options:\n\
           --offset-x X      GRAM column offset (default 0)\n\
           --offset-y Y      GRAM row offset (default 0)\n\
           --rotation D      0, 90, 180, or 270 (default 90)\n\
           --mirrored        flip horizontal (default, fixes backwards text)\n\
           --no-mirror       disable horizontal flip\n\
           --spi-dev PATH    default /dev/spidev0.0 (CE0)\n\
           --spi-ce          use kernel CE0 (default)\n\
           --spi-no-cs       SPI_NO_CS (not used on POC2 wiring)\n\
           --spi-mode M      0 or 3 (default 0)\n\
           --blk-float       do not drive BLK (GPIO 16)\n\
           --blk-active-low  BLK active-low backlight\n\
           --invert          enable colour inversion\n\
           --no-invert       disable colour inversion (default)\n\
           --color-order C   rgb or bgr (default rgb)\n\
           --spi-speed HZ    SPI clock (default 40000000)\n\
         \n\
         LED flicker diagnosis (add --leds; needs sudo for PWM/DMA):\n\
           --leds            run the led_test WS2811 animation on its own thread\n\
           --with-leds       alias for --leds\n\
           --led-front N     front strip LED count (default 33, GPIO 13)\n\
           --led-top N       top strip LED count (default 30, GPIO 12)\n\
           --led-brightness P  strip brightness percent (default 10)\n\
           --led-fps N       LED frames per second (default 30)\n\
           --led-dma N       ws2811 DMA channel (default 10)\n\
           --led-mode M      cycle|walk|rainbow|steady|solid|off (default cycle)\n\
           --led-quiet       suppress the per-LED step log\n\
         \n\
         Paint-pattern diagnosis (reproduce how plippa-ui paints):\n\
           --paint P         anim|ui|ui-scroll (default anim)\n\
           --paint-hz N      present at N Hz, 0 = every frame (default 0)\n\
         \n\
         If the image glitches, try --spi-speed 20000000 or 10000000."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_modes_parse_from_cli_names() {
        assert_eq!(PaintMode::parse("anim"), Some(PaintMode::Anim));
        assert_eq!(PaintMode::parse("balls"), Some(PaintMode::Anim));
        assert_eq!(PaintMode::parse("ui"), Some(PaintMode::Ui));
        assert_eq!(PaintMode::parse("ui-scroll"), Some(PaintMode::UiScroll));
        assert_eq!(PaintMode::parse("nope"), None);
    }

    #[test]
    fn paint_hz_zero_means_every_frame() {
        let mut app = AppConfig::default();
        assert_eq!(app.paint_hz, 0);
        assert!(app.paint_interval().is_none());
        app.paint_hz = 4;
        assert_eq!(app.paint_interval(), Some(Duration::from_millis(250)));
        // Never divide by zero on a silly rate.
        app.paint_hz = 5000;
        assert_eq!(app.paint_interval(), Some(Duration::from_millis(0)));
    }

    /// The UI-shaped frame must stay fully inside the buffer for the sizes we ship.
    #[test]
    fn ui_frame_fits_320x240() {
        let (w, h) = (320usize, 240usize);
        let mut fb = vec![Rgb565::BLACK; w * h];
        for present in [0u64, 7, 39, 1234] {
            for scroll in [0i32, 1, 5] {
                draw_ui_frame(&mut fb, w, h, present, scroll);
                assert!(fb.iter().any(|p| *p != Rgb565::BLACK));
            }
        }
    }
}
