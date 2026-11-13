// see screen_test_ILI9341_240x320/pinout.md for hardware setup

mod gpio_out;
mod smoke;

use embedded_graphics::{
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{Line, PrimitiveStyle, Rectangle},
};
use gpio_out::{open_output, pulse_display_reset, Backlight};
use linux_embedded_hal::spidev::{SpiModeFlags, SpidevOptions};
use linux_embedded_hal::{Delay, SpidevDevice};
use mipidsi::{
    interface::SpiInterface,
    models::ILI9341Rgb565,
    options::{ColorInversion, ColorOrder, Orientation, Rotation},
    Builder, TestImage,
};
use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static RUNNING: AtomicBool = AtomicBool::new(true);

const SPI_DEV: &str = "/dev/spidev0.0";
pub(crate) const PIN_DC: u32 = 24;
pub(crate) const PIN_RST: u32 = 25;
pub(crate) const PIN_BLK: u32 = 16;

const WIDTH: u16 = 240;
const HEIGHT: u16 = 320;

#[derive(Clone, Copy)]
pub(crate) struct DisplayConfig {
    width: u16,
    height: u16,
    offset_x: u16,
    offset_y: u16,
    rotation: Rotation,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            width: WIDTH,
            height: HEIGHT,
            offset_x: 0,
            offset_y: 0,
            rotation: Rotation::Deg0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TestMode {
    Calibration,
    PixelWalk,
    ColorBars,
    GradientSweep,
    CheckerScroll,
}

impl TestMode {
    const ALL: [TestMode; 5] = [
        TestMode::Calibration,
        TestMode::PixelWalk,
        TestMode::ColorBars,
        TestMode::GradientSweep,
        TestMode::CheckerScroll,
    ];

    fn label(self) -> &'static str {
        match self {
            TestMode::Calibration => "Calibration",
            TestMode::PixelWalk => "Pixel walk",
            TestMode::ColorBars => "Color bars",
            TestMode::GradientSweep => "Gradient sweep",
            TestMode::CheckerScroll => "Checker scroll",
        }
    }

    fn next(self) -> Self {
        let idx = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    fn is_animated(self) -> bool {
        matches!(
            self,
            TestMode::PixelWalk | TestMode::GradientSweep | TestMode::CheckerScroll
        )
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
    mode_secs: u64,
    smoke: bool,
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
            spi_speed_hz: 10_000_000,
            mode_secs: 8,
            smoke: false,
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

    if app.smoke {
        return smoke::run_smoke(app.spi_speed_hz);
    }

    println!(
        "ILI9341 {}x{} @ {}° offset ({}, {}), SPI {}",
        cfg.width,
        cfg.height,
        cfg.rotation.degree(),
        cfg.offset_x,
        cfg.offset_y,
        app.spi_dev
    );
    println!(
        "BLK GPIO {PIN_BLK} ({}), modes auto-cycle every {}s — Ctrl-C to exit",
        if app.blk_active_low {
            "active-low"
        } else {
            "active-high"
        },
        app.mode_secs
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
            "cannot open {} ({e}) — enable SPI with dtoverlay=spi0-1cs,no_miso (see poc2_readme.md)",
            app.spi_dev
        )
    })?;
    spi.configure(
        &SpidevOptions::new()
            .max_speed_hz(app.spi_speed_hz)
            .mode(spi_flags(app.spi_mode, app.spi_no_cs))
            .build(),
    )?;

    pulse_display_reset(PIN_RST)?;
    let mut dc = open_output(PIN_DC, "dc", false)?;
    let mut rst = open_output(PIN_RST, "rst", true)?;
    let mut buffer = [0_u8; 4096];
    let di = SpiInterface::new(spi, &mut dc, &mut buffer);

    let mut display = Builder::new(ILI9341Rgb565, di)
        .display_size(cfg.width, cfg.height)
        .display_offset(cfg.offset_x, cfg.offset_y)
        .orientation(Orientation::new().rotate(cfg.rotation))
        .color_order(app.color_order)
        .invert_colors(app.invert)
        .reset_pin(&mut rst)
        .init(&mut delay)
        .map_err(|e| format!("display init failed: {e:?}"))?;

    display
        .clear(Rgb565::WHITE)
        .map_err(|e| format!("startup fill failed: {e:?}"))?;
    println!("Display init OK — 240×320 rot0 offset(0,0) rgb SPI mode 0 + CE0");
    println!("Cycling test modes…");

    let mut mode = TestMode::Calibration;
    let mut mode_started = Instant::now();
    let mut walk_index = 0u32;
    let mut sweep_phase = 0u16;
    let mut scroll_offset = 0u16;
    let mut last_draw = Instant::now();

    draw_frame(
        &mut display,
        cfg,
        mode,
        walk_index,
        sweep_phase,
        scroll_offset,
    )
    .map_err(|e| format!("draw failed: {e:?}"))?;
    println!("Mode: {}", mode.label());

    while RUNNING.load(Ordering::SeqCst) {
        if mode_started.elapsed() >= Duration::from_secs(app.mode_secs) {
            mode = mode.next();
            mode_started = Instant::now();
            walk_index = 0;
            sweep_phase = 0;
            scroll_offset = 0;
            draw_frame(
                &mut display,
                cfg,
                mode,
                walk_index,
                sweep_phase,
                scroll_offset,
            )
            .map_err(|e| format!("draw failed: {e:?}"))?;
            println!("Mode: {}", mode.label());
        }

        let frame_ms = if mode == TestMode::CheckerScroll {
            80
        } else {
            33
        };
        if mode.is_animated() && last_draw.elapsed() >= Duration::from_millis(frame_ms) {
            draw_frame(
                &mut display,
                cfg,
                mode,
                walk_index,
                sweep_phase,
                scroll_offset,
            )
            .map_err(|e| format!("draw failed: {e:?}"))?;
            last_draw = Instant::now();

            match mode {
                TestMode::PixelWalk => walk_index = walk_index.wrapping_add(1),
                TestMode::GradientSweep => sweep_phase = sweep_phase.wrapping_add(4),
                TestMode::CheckerScroll => scroll_offset = scroll_offset.wrapping_add(2),
                _ => {}
            }
        }

        thread::sleep(Duration::from_millis(5));
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

fn draw_frame<D>(
    display: &mut D,
    cfg: DisplayConfig,
    mode: TestMode,
    walk_index: u32,
    sweep_phase: u16,
    scroll_offset: u16,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    match mode {
        TestMode::Calibration => {
            display.clear(Rgb565::BLACK)?;
            TestImage::<Rgb565>::new().draw(display)?;
        }
        TestMode::PixelWalk => draw_pixel_walk(display, cfg, walk_index)?,
        TestMode::ColorBars => draw_color_bars(display, cfg)?,
        TestMode::GradientSweep => draw_gradient_sweep(display, cfg, sweep_phase)?,
        TestMode::CheckerScroll => draw_checker_scroll(display, cfg, scroll_offset)?,
    }
    Ok(())
}

fn draw_pixel_walk<D>(display: &mut D, cfg: DisplayConfig, index: u32) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    const DOT: u32 = 16;
    const STEP: u32 = 12;

    let cols = u32::from(cfg.width).saturating_sub(DOT) / STEP + 1;
    let rows = u32::from(cfg.height).saturating_sub(DOT) / STEP + 1;
    let total = cols * rows;
    let idx = index % total.max(1);
    let x = (idx % cols) * STEP;
    let y = (idx / cols) * STEP;

    display.clear(Rgb565::new(2, 2, 8))?;

    let hue = ((idx * 37) % 360) as u8;
    Rectangle::new(
        Point::new(x as i32, y as i32),
        Size::new(DOT, DOT),
    )
    .into_styled(PrimitiveStyle::with_fill(hsv_to_rgb565(hue, 255, 255)))
    .draw(display)?;
    Ok(())
}

fn draw_color_bars<D>(display: &mut D, cfg: DisplayConfig) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let bar_h = cfg.height / 6;
    let colors = [
        Rgb565::RED,
        Rgb565::GREEN,
        Rgb565::BLUE,
        Rgb565::YELLOW,
        Rgb565::MAGENTA,
        Rgb565::CYAN,
    ];
    display.clear(Rgb565::BLACK)?;
    for (i, color) in colors.iter().enumerate() {
        Rectangle::new(
            Point::new(0, i as i32 * bar_h as i32),
            Size::new(u32::from(cfg.width), u32::from(bar_h)),
        )
        .into_styled(PrimitiveStyle::with_fill(*color))
        .draw(display)?;
    }
    Ok(())
}

fn draw_gradient_sweep<D>(
    display: &mut D,
    cfg: DisplayConfig,
    phase: u16,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    if phase == 0 {
        display.clear(Rgb565::BLACK)?;
    }
    for x in 0..cfg.width {
        let hue = ((x as u16 + phase) % 360) as u8;
        let color = hsv_to_rgb565(hue, 255, 200);
        Line::new(
            Point::new(x as i32, 0),
            Point::new(x as i32, cfg.height as i32 - 1),
        )
        .into_styled(PrimitiveStyle::with_stroke(color, 1))
        .draw(display)?;
    }
    Ok(())
}

fn draw_checker_scroll<D>(
    display: &mut D,
    cfg: DisplayConfig,
    offset: u16,
) -> Result<(), D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let cell = 16u16;
    let cols = cfg.width.div_ceil(cell);
    let rows = cfg.height.div_ceil(cell);
    for row in 0..rows {
        for col in 0..cols {
            let x = col * cell;
            let y = row * cell;
            let cx = (x + offset) / cell;
            let cy = (y + offset / 2) / cell;
            let color = if (cx + cy) % 2 == 0 {
                Rgb565::WHITE
            } else {
                Rgb565::BLACK
            };
            let w = cell.min(cfg.width - x);
            let h = cell.min(cfg.height - y);
            Rectangle::new(
                Point::new(x as i32, y as i32),
                Size::new(u32::from(w), u32::from(h)),
            )
            .into_styled(PrimitiveStyle::with_fill(color))
            .draw(display)?;
        }
    }
    Ok(())
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
            "--smoke" => app.smoke = true,
            "--invert" => app.invert = ColorInversion::Inverted,
            "--no-invert" => app.invert = ColorInversion::Normal,
            "--color-order" => {
                app.color_order = match args.next().as_deref() {
                    Some("bgr") => ColorOrder::Bgr,
                    _ => ColorOrder::Rgb,
                };
            }
            "--spi-speed" => {
                app.spi_speed_hz = args.next().and_then(|v| v.parse().ok()).unwrap_or(10_000_000);
            }
            "--mode-secs" => {
                app.mode_secs = args.next().and_then(|v| v.parse().ok()).unwrap_or(8);
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
        "usage: poc2_screen_test_ILI9341_240x320 [options]\n\
         \n\
         options:\n\
           --offset-x X      GRAM column offset (default 0)\n\
           --offset-y Y      GRAM row offset (default 0)\n\
           --rotation D      0, 90, 180, or 270 (default 0)\n\
           --spi-dev PATH    default /dev/spidev0.0 (CE0)\n\
           --spi-ce          use kernel CE0 (default)\n\
           --spi-no-cs       SPI_NO_CS (not used on POC2 wiring)\n\
           --spi-mode M      0 or 3 (default 0)\n\
           --blk-float       do not drive BLK (GPIO 16)\n\
           --blk-active-low  BLK active-low backlight\n\
           --smoke           cycle RED/GREEN/BLUE/WHITE combos\n\
           --invert          enable colour inversion\n\
           --no-invert       disable colour inversion (default)\n\
           --color-order C   rgb or bgr (default rgb)\n\
           --spi-speed HZ    SPI clock (default 10000000)\n\
           --mode-secs N     seconds per test mode (default 8)\n\
         \n\
         Cycles: calibration → pixel walk → colour bars → gradient → checkerboard."
    );
}
