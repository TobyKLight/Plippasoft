//! POC2 encoder resolution eval — SWE1 value with screen meters + SWE3/4 LED bars.
//!
//! SWE1 drives 0–100%. SWE5 sets step count (10…100 /10, then …2000 /50). SWE3 = interval
//! accel (slow click always 1 step), SWE4 = EMA ms. Both start OFF.
//! Default SPI 62.5 MHz (~47 FPS full-screen when core stays high).
//! see encoder_res_eval/pinout.md for hardware setup

mod aw9523;
mod encoder;
mod gpio_out;
mod leds;
mod smoothing;

use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::Rectangle,
    text::{Baseline, Text},
};
use aw9523::{Aw9523, SwitchDebouncer, SwitchEvent, SwitchId};
use gpio_out::{open_output, pulse_display_reset, Backlight};
use linux_embedded_hal::spidev::{SpiModeFlags, SpidevOptions};
use linux_embedded_hal::{Delay, SpidevDevice};
use mipidsi::{
    interface::SpiInterface,
    models::ILI9341Rgb565,
    options::{ColorInversion, ColorOrder, Orientation, Rotation},
    Builder,
};
use smoothing::{Swe3Mode, ValueController};
use std::env;
use std::f32::consts::PI;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

static RUNNING: AtomicBool = AtomicBool::new(true);

const SPI_DEV: &str = "/dev/spidev0.0";
const PIN_DC: u32 = 24;
const PIN_RST: u32 = 25;
const PIN_BLK: u32 = 16;

const WIDTH: u16 = 240;
const HEIGHT: u16 = 320;

const DEFAULT_BRIGHTNESS_PCT: u8 = 2;
const DEFAULT_STEPS: u32 = 100;
const START_PCT: f32 = 0.50;
/// Lab sweet spot for ~47 FPS full-screen on CM4.
const DEFAULT_SPI_HZ: u32 = 62_500_000;
/// Quadrature edges per mechanical detent (POC2 parts observed as 2).
const DEFAULT_DETENT_EDGES: u32 = 2;

const COL_BG: Rgb565 = Rgb565::new(2, 2, 6);
const COL_OUTLINE: Rgb565 = Rgb565::WHITE;
const COL_FILL: Rgb565 = Rgb565::new(0, 40, 31); // cyan-ish
const COL_TEXT: Rgb565 = Rgb565::WHITE;
const COL_ACCENT: Rgb565 = Rgb565::YELLOW;
const COL_PARAM: Rgb565 = Rgb565::CYAN;
const COL_FPS: Rgb565 = Rgb565::new(31, 63, 0); // lime-ish

#[derive(Clone, Copy)]
struct DisplayConfig {
    width: u16,
    height: u16,
    offset_x: u16,
    offset_y: u16,
    rotation: Rotation,
    mirrored: bool,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            width: WIDTH,
            height: HEIGHT,
            offset_x: 0,
            offset_y: 0,
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
    brightness_pct: u8,
    steps: u32,
    /// Quadrature edges required per detent step (1 = raw edges).
    detent_edges: u32,
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
            spi_speed_hz: DEFAULT_SPI_HZ,
            brightness_pct: DEFAULT_BRIGHTNESS_PCT,
            steps: DEFAULT_STEPS,
            detent_edges: DEFAULT_DETENT_EDGES,
        }
    }
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
        "POC2 encoder resolution eval — logical {}x{}, SPI {} @ {} Hz, LED brightness {}%\n\
         SWE1=value  SWE5=steps  SWE3=accel|turns (push to toggle)  SWE4=ema_ms\n\
         Start 50%, smoothing OFF. Detent filter: {} edge(s)/click\n\
         Target ~47 FPS @ 62.5 MHz if core stays ~500 MHz\n\
         Ctrl-C to exit",
        fb_w,
        fb_h,
        app.spi_dev,
        app.spi_speed_hz,
        app.brightness_pct,
        app.detent_edges
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

    // Poll encoders on a side thread so SPI present (~20 ms) cannot skip quadrature edges.
    let detent_edges = app.detent_edges;
    let (enc_tx, enc_rx) = mpsc::sync_channel::<(usize, i32)>(512);
    let enc_thread = thread::spawn(move || {
        let mut encoders = match encoder::open_all() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("encoder open failed: {e}");
                return;
            }
        };
        let mut filters = [
            encoder::DetentFilter::new(detent_edges),
            encoder::DetentFilter::new(detent_edges),
            encoder::DetentFilter::new(detent_edges),
            encoder::DetentFilter::new(detent_edges),
        ];
        while RUNNING.load(Ordering::SeqCst) {
            for (i, enc) in encoders.iter_mut().enumerate() {
                let edge = enc.poll();
                if edge == 0 {
                    continue;
                }
                let detent = filters[i].push(edge);
                if detent != 0 {
                    let _ = enc_tx.try_send((i, detent));
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
    });

    let mut led_ctrl = leds::open_strips(
        leds::FRONT_COUNT as i32,
        leds::TOP_COUNT as i32,
        app.brightness_pct,
    )?;
    let mut value = ValueController::new(app.steps, START_PCT);

    let bus_path = PathBuf::from("/dev/i2c-1");
    let mut aw = Aw9523::open(&bus_path, 0x58)?;
    let mut debounce = SwitchDebouncer::new(20);

    let mut fb = vec![COL_BG; fb_w * fb_h];
    let scratch_w = 28 * FONT_10X20.character_size.width as usize;
    let scratch_h = FONT_10X20.character_size.height as usize;
    let mut scratch = vec![Rgb565::BLACK; scratch_w * scratch_h];

    let led_color = leds::rgb(0, 80, 255);
    let mut fps_label = String::from("FPS --.-");
    let mut dirty_leds = true;
    let mut last_steps = value.steps();
    let mut last_swe3 = value.swe3_label();
    let mut last_ema = value.ema_ms;
    let mut last_mode = value.swe3_mode;

    let mut last = Instant::now();
    let mut fps_window = Instant::now();
    let mut frames: u32 = 0;

    while RUNNING.load(Ordering::SeqCst) {
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f32().min(0.05);
        last = now;

        // Drain encoder events
        while let Ok((i, ticks)) = enc_rx.try_recv() {
            match i {
                0 => {
                    value.apply_swe1_ticks(ticks, now);
                    dirty_leds = true;
                }
                1 => value.nudge_swe3(ticks),
                2 => {
                    value.nudge_ema_ms(ticks);
                    dirty_leds = true;
                }
                3 => {
                    value.nudge_steps(ticks);
                    dirty_leds = true;
                }
                _ => {}
            }
        }

        // SWE3 push toggles accel ↔ turns mode
        if let Ok((p0, p1)) = aw.read_ports() {
            for event in debounce.poll_events(p0, p1) {
                if let SwitchEvent::Pressed(SwitchId::Swe3) = event {
                    value.toggle_swe3_mode();
                    let mode = match value.swe3_mode {
                        Swe3Mode::Accel => "accel",
                        Swe3Mode::Turns => "turns",
                    };
                    println!(
                        "SWE3 push → {mode} mode  ({})",
                        value.swe3_label()
                    );
                }
            }
        }

        if value.steps() != last_steps {
            println!(
                "SWE5 steps → {}  ({:.3}%/detent)",
                value.steps(),
                100.0 / value.steps() as f32
            );
            last_steps = value.steps();
        }
        let swe3 = value.swe3_label();
        if swe3 != last_swe3 || value.swe3_mode != last_mode {
            println!("SWE3 → {swe3}");
            last_swe3 = swe3;
            last_mode = value.swe3_mode;
        }
        if value.ema_ms != last_ema {
            println!("SWE4 ema → {}", value.ema_label());
            last_ema = value.ema_ms;
        }

        value.tick_display(dt);
        let disp = value.display_pct();

        if dirty_leds {
            if let Err(e) = leds::apply_value_bars(&mut led_ctrl, disp, led_color) {
                eprintln!("{e}");
            }
            dirty_leds = false;
        }

        // Continuous full-screen present (SPI-limited; ~47 FPS @ 62.5 MHz + pinned core).
        paint_ui(
            &mut fb,
            fb_w,
            fb_h,
            &value,
            &fps_label,
            &mut scratch,
            scratch_w,
            scratch_h,
        );
        if let Err(e) = present(&mut display, fb_w, fb_h, &fb) {
            eprintln!("present failed: {e:?}");
        }
        frames += 1;

        if fps_window.elapsed() >= Duration::from_millis(200) {
            let secs = fps_window.elapsed().as_secs_f32().max(1e-3);
            fps_label = format!("FPS {:4.1}", frames as f32 / secs);
            frames = 0;
            fps_window = Instant::now();
        }
    }

    let _ = enc_thread.join();
    let _ = leds::clear_all(&mut led_ctrl);
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

fn paint_ui(
    fb: &mut [Rgb565],
    w: usize,
    h: usize,
    value: &ValueController,
    fps_label: &str,
    scratch: &mut [Rgb565],
    scratch_w: usize,
    scratch_h: usize,
) {
    fb.fill(COL_BG);

    let pct = value.display_pct();
    let pct_label = format!("{:.2}%", pct * 100.0);
    let step_label = format!("{}/{}", value.display_step(), value.steps());

    // FPS — top left
    blit_text_scaled(
        fb, w, h, fps_label, 4, 4, 1, COL_FPS, scratch, scratch_w, scratch_h,
    );

    // Big percentage — top centre, step index beside it
    let scale = 2u32;
    let text_w = (pct_label.len() as u32) * FONT_10X20.character_size.width * scale;
    let text_x = ((w as i32) - text_w as i32) / 2;
    let pct_x = text_x.max(90);
    blit_text_scaled(
        fb,
        w,
        h,
        &pct_label,
        pct_x,
        6,
        scale,
        COL_ACCENT,
        scratch,
        scratch_w,
        scratch_h,
    );
    blit_text_scaled(
        fb,
        w,
        h,
        &step_label,
        pct_x + text_w as i32 + 8,
        18,
        1,
        COL_ACCENT,
        scratch,
        scratch_w,
        scratch_h,
    );

    // Clock pie — left
    let clock_cx = 70;
    let clock_cy = 100;
    let clock_r = 46;
    fill_clock_pie(fb, w, h, clock_cx, clock_cy, clock_r, pct, COL_FILL);
    stroke_circle(fb, w, h, clock_cx, clock_cy, clock_r, COL_OUTLINE);
    for dy in 0..6 {
        put_px(fb, w, h, clock_cx, clock_cy - clock_r + 2 + dy, COL_OUTLINE);
    }

    // Horizontal bar — right of clock
    let bar_x = 136;
    let bar_y = 72;
    let bar_w = 172;
    let bar_h = 34;
    stroke_rect(fb, w, h, bar_x, bar_y, bar_w, bar_h, COL_OUTLINE);
    let inner_w = ((bar_w - 4) as f32 * pct.clamp(0.0, 1.0)).round() as i32;
    if inner_w > 0 {
        fill_rect(fb, w, h, bar_x + 2, bar_y + 2, inner_w, bar_h - 4, COL_FILL);
    }

    // Params — SWE5 steps in yellow so it reads with the % value
    let owned = [
        (
            format!(
                "SWE1  {:.2}%  (target {:.2}%)",
                pct * 100.0,
                value.target_pct() * 100.0
            ),
            COL_TEXT,
        ),
        (format!("SWE5  steps {}", value.steps()), COL_ACCENT),
        (format!("SWE3  {}  (push:swap)", value.swe3_label()), COL_PARAM),
        (format!("SWE4  ema {}", value.ema_label()), COL_PARAM),
    ];

    let mut y = 152;
    for (line, color) in &owned {
        blit_text_scaled(fb, w, h, line, 6, y, 1, *color, scratch, scratch_w, scratch_h);
        y += 16;
    }
}

fn fill_clock_pie(
    fb: &mut [Rgb565],
    w: usize,
    h: usize,
    cx: i32,
    cy: i32,
    r: i32,
    pct: f32,
    color: Rgb565,
) {
    let pct = pct.clamp(0.0, 1.0);
    if pct <= 0.0 || r <= 0 {
        return;
    }
    let sweep = pct * 2.0 * PI;
    let r2 = r * r;
    let y0 = (cy - r).max(0);
    let y1 = (cy + r).min(h as i32 - 1);
    let x0 = (cx - r).max(0);
    let x1 = (cx + r).min(w as i32 - 1);

    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x - cx;
            let dy = y - cy;
            if dx * dx + dy * dy > r2 {
                continue;
            }
            // Angle from 12 o'clock, clockwise, [0, 2π).
            let ang = (dx as f32).atan2(-(dy as f32));
            let ang = if ang < 0.0 { ang + 2.0 * PI } else { ang };
            if ang <= sweep + 1e-4 {
                fb[y as usize * w + x as usize] = color;
            }
        }
    }
}

fn stroke_circle(fb: &mut [Rgb565], w: usize, h: usize, cx: i32, cy: i32, r: i32, color: Rgb565) {
    if r <= 0 {
        return;
    }
    let mut x = r;
    let mut y = 0;
    let mut err = 1 - r;
    while x >= y {
        plot8(fb, w, h, cx, cy, x, y, color);
        y += 1;
        if err < 0 {
            err += 2 * y + 1;
        } else {
            x -= 1;
            err += 2 * (y - x) + 1;
        }
    }
}

fn plot8(fb: &mut [Rgb565], w: usize, h: usize, cx: i32, cy: i32, x: i32, y: i32, c: Rgb565) {
    put_px(fb, w, h, cx + x, cy + y, c);
    put_px(fb, w, h, cx + y, cy + x, c);
    put_px(fb, w, h, cx - y, cy + x, c);
    put_px(fb, w, h, cx - x, cy + y, c);
    put_px(fb, w, h, cx - x, cy - y, c);
    put_px(fb, w, h, cx - y, cy - x, c);
    put_px(fb, w, h, cx + y, cy - x, c);
    put_px(fb, w, h, cx + x, cy - y, c);
}

fn stroke_rect(fb: &mut [Rgb565], w: usize, h: usize, x: i32, y: i32, bw: i32, bh: i32, c: Rgb565) {
    for i in 0..bw {
        put_px(fb, w, h, x + i, y, c);
        put_px(fb, w, h, x + i, y + bh - 1, c);
    }
    for j in 0..bh {
        put_px(fb, w, h, x, y + j, c);
        put_px(fb, w, h, x + bw - 1, y + j, c);
    }
}

fn fill_rect(fb: &mut [Rgb565], w: usize, h: usize, x: i32, y: i32, bw: i32, bh: i32, c: Rgb565) {
    for j in 0..bh {
        for i in 0..bw {
            put_px(fb, w, h, x + i, y + j, c);
        }
    }
}

fn put_px(fb: &mut [Rgb565], w: usize, h: usize, x: i32, y: i32, c: Rgb565) {
    if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
        fb[y as usize * w + x as usize] = c;
    }
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
    if scratch_w == 0 || scratch_h == 0 || scratch.len() < scratch_w * scratch_h {
        return;
    }
    // Must clear the full 2D grid. A linear `need`-length fill only wiped the first
    // few rows, so later draws blitted leftover yellow strokes as "ghost glyphs".
    scratch[..scratch_w * scratch_h].fill(Rgb565::BLACK);
    {
        let mut scratch_target = FbTarget {
            buf: scratch,
            w: scratch_w,
            h: scratch_h,
        };
        // Draw mask in white; blit uses the caller colour (never stale scratch colours).
        Text::with_baseline(
            label,
            Point::zero(),
            MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
            Baseline::Top,
        )
        .draw(&mut scratch_target)
        .ok();
    }

    let glyph_w = label.len() * FONT_10X20.character_size.width as usize;
    for sy in 0..scratch_h {
        for sx in 0..glyph_w.min(scratch_w) {
            if scratch[sy * scratch_w + sx] == Rgb565::BLACK {
                continue;
            }
            for dy in 0..scale as i32 {
                for dx in 0..scale as i32 {
                    let x = dst_x + sx as i32 * scale as i32 + dx;
                    let y = dst_y + sy as i32 * scale as i32 + dy;
                    put_px(fb, w, h, x, y, color);
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

fn present<D>(display: &mut D, w: usize, h: usize, fb: &[Rgb565]) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let area = Rectangle::new(Point::zero(), Size::new(w as u32, h as u32));
    display.fill_contiguous(&area, fb.iter().copied())
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
                    .unwrap_or(DEFAULT_SPI_HZ);
            }
            "--brightness" => {
                app.brightness_pct = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_BRIGHTNESS_PCT)
                    .min(100);
            }
            "--steps" => {
                app.steps = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_STEPS);
                app.steps = smoothing::clamp_steps(app.steps);
            }
            "--detent-edges" => {
                app.detent_edges = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_DETENT_EDGES)
                    .max(1);
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
        "usage: poc2_encoder_res_eval [options]\n\
         \n\
         Feel 18-detent encoder resolution on SWE1 (0–100%, start 50%).\n\
         Screen: FPS + % digits + clock pie + horizontal bar. SWE3/SWE4 LED\n\
         bars show the same value with fractional brightness.\n\
         Default SPI {spi} Hz (~47 FPS when core ~500 MHz).\n\
         \n\
         Controls:\n\
           SWE1  value up/down\n\
           SWE5  step count {min}…{pivot} /{lo}, then …{max} /{hi} (default {steps})\n\
           SWE3  accel OR turns (push SWE3 to toggle; turns in 0.2)\n\
           SWE4  EMA smooth 0…{emax} ms in {einc} (0=OFF)\n\
         \n\
         options:\n\
           --brightness P     LED max brightness 0–100 (default {br})\n\
           --steps N          initial step count (default {steps})\n\
           --detent-edges N   quadrature edges per click (default {detent}; 1=raw)\n\
           --spi-speed HZ     SPI clock (default {spi})\n\
           --rotation D       0, 90, 180, or 270 (default 90)\n\
           --mirrored / --no-mirror\n\
           --help\n\
         \n\
         Wiring: see eval/poc2/encoder_res_eval/pinout.md",
        min = smoothing::STEPS_MIN,
        max = smoothing::STEPS_MAX,
        pivot = smoothing::STEPS_PIVOT,
        lo = smoothing::STEPS_INC_LO,
        hi = smoothing::STEPS_INC_HI,
        emax = smoothing::EMA_MS_MAX,
        einc = smoothing::EMA_MS_INC,
        br = DEFAULT_BRIGHTNESS_PCT,
        steps = DEFAULT_STEPS,
        detent = DEFAULT_DETENT_EDGES,
        spi = DEFAULT_SPI_HZ,
    );
}
