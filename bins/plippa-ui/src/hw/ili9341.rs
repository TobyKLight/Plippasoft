//! ILI9341 SPI display backend (mipidsi), adapted from eval/poc2/screen_test.

use crate::display::{DisplayBackend, FrameBuffer, Rect};
use embedded_graphics::pixelcolor::Rgb565 as EgRgb565;
use embedded_graphics::prelude::*;
use gpio_cdev::{Chip, LineHandle, LineRequestFlags};
use linux_embedded_hal::spidev::{SpiModeFlags, SpidevOptions};
use linux_embedded_hal::{Delay, SpidevDevice};
use mipidsi::interface::SpiInterface;
use mipidsi::models::ILI9341Rgb565;
use mipidsi::options::{ColorOrder, Orientation, Rotation};
use mipidsi::Builder;
use plippa_board::DisplayConfig;
use std::thread;
use std::time::{Duration, Instant};

type DisplayDriver = mipidsi::Display<
    SpiInterface<'static, SpidevDevice, linux_embedded_hal::CdevPin>,
    ILI9341Rgb565,
    linux_embedded_hal::CdevPin,
>;

pub struct Ili9341Display {
    display: DisplayDriver,
    _backlight: LineHandle,
    width: u16,
    height: u16,
    scratch: Vec<EgRgb565>,
    /// Reused destination for windowed presents: only `rect.w * rect.h` pixels are filled
    /// and sent (WAVE_02 waveform band), so no full-frame buffer is ever allocated per frame.
    rect_scratch: Vec<EgRgb565>,
    stats: PresentStats,
}

/// SPI present diagnostics, enabled with `PLIPPA_UI_DISPLAY_STATS=1`.
///
/// The lab harness presents at ~28 Hz, so a failed or stalled present is repaired by the next
/// one ~35 ms later and nobody notices. The live UI is event driven and may present ~1 Hz, so
/// the same fault stays on the panel for up to a second. This measures the real present rate
/// and duration, and reports any failed `fill_contiguous` (which the caller used to discard).
struct PresentStats {
    enabled: bool,
    presents: u64,
    errors: u64,
    sum: Duration,
    max: Duration,
    window: Instant,
    window_presents: u64,
}

impl PresentStats {
    fn from_env() -> Self {
        let enabled =
            std::env::var_os("PLIPPA_UI_DISPLAY_STATS").is_some_and(|v| !v.is_empty() && v != "0");
        Self {
            enabled,
            presents: 0,
            errors: 0,
            sum: Duration::ZERO,
            max: Duration::ZERO,
            window: Instant::now(),
            window_presents: 0,
        }
    }

    fn record(&mut self, elapsed: Duration, err: Option<String>) {
        if let Some(e) = err {
            self.errors += 1;
            // A truncated frame is only repaired by the *next* present.
            eprintln!("plippa-ui: display present failed: {e}");
        }
        if !self.enabled {
            return;
        }
        self.presents += 1;
        self.window_presents += 1;
        self.sum += elapsed;
        self.max = self.max.max(elapsed);

        let window = self.window.elapsed();
        if window >= Duration::from_secs(5) {
            let secs = window.as_secs_f64().max(1e-3);
            let avg = self.sum / self.presents.max(1) as u32;
            eprintln!(
                "DISPLAY {:.1}s presents={} ({:.1}/s) avg={:.1}ms max={:.1}ms errors={}",
                secs,
                self.window_presents,
                self.window_presents as f64 / secs,
                avg.as_secs_f64() * 1e3,
                self.max.as_secs_f64() * 1e3,
                self.errors,
            );
            self.window = Instant::now();
            self.window_presents = 0;
        }
    }
}

impl Ili9341Display {
    pub fn open(cfg: &DisplayConfig) -> Result<Self, Box<dyn std::error::Error>> {
        pulse_reset(cfg.rst_gpio)?;
        let backlight = open_backlight(cfg.bl_gpio)?;

        // Match eval bouncy anim default (40 MHz). Drop if the panel sparkles.
        const SPI_HZ: u32 = 40_000_000;
        let mut spi = SpidevDevice::open(&cfg.spi_dev)?;
        let options = SpidevOptions::new()
            .bits_per_word(8)
            .max_speed_hz(SPI_HZ)
            .mode(SpiModeFlags::SPI_MODE_0)
            .build();
        spi.configure(&options)?;

        let dc = open_output(cfg.dc_gpio, "ili9341_dc", false)?;
        let rst = open_output(cfg.rst_gpio, "ili9341_rst", true)?;

        // SpiInterface borrows a buffer for the lifetime of the display — leak once.
        let buffer: &'static mut [u8] = Box::leak(vec![0u8; 4096].into_boxed_slice());
        let di = SpiInterface::new(spi, dc, buffer);
        let mut delay = Delay;
        let orientation = orientation_from_config(cfg)?;
        // Panel colour order: an ILI9341 module wired BGR swaps red and blue on the glass if
        // the MADCTL BGR bit is wrong, which no software test can see. `[display] color_order`
        // declares it per board.
        let color_order = if cfg.bgr_panel() {
            ColorOrder::Bgr
        } else {
            ColorOrder::Rgb
        };
        // Native GRAM size (before rotation). Logical size comes from display.size().
        let display = Builder::new(ILI9341Rgb565, di)
            .display_size(cfg.width, cfg.height)
            .orientation(orientation)
            .color_order(color_order)
            .reset_pin(rst)
            .init(&mut delay)
            .map_err(|e| format!("ILI9341 init: {e:?}"))?;

        let size = display.size();
        let width = size.width as u16;
        let height = size.height as u16;
        let len = (width as usize) * (height as usize);
        eprintln!(
            "plippa-ui: ILI9341 {}x{} native → {}x{} logical (rot={} mirrored={}) SPI {} Hz",
            cfg.width, cfg.height, width, height, cfg.rotation, cfg.mirrored, SPI_HZ
        );
        Ok(Self {
            display,
            _backlight: backlight,
            width,
            height,
            scratch: vec![EgRgb565::BLACK; len],
            rect_scratch: vec![EgRgb565::BLACK; len],
            stats: PresentStats::from_env(),
        })
    }
}

fn orientation_from_config(cfg: &DisplayConfig) -> Result<Orientation, String> {
    let rotation = match cfg.rotation {
        0 => Rotation::Deg0,
        90 => Rotation::Deg90,
        180 => Rotation::Deg180,
        270 => Rotation::Deg270,
        other => {
            return Err(format!(
                "display.rotation must be 0/90/180/270, got {other}"
            ))
        }
    };
    let mut o = Orientation::new().rotate(rotation);
    if cfg.mirrored {
        o = o.flip_horizontal();
    }
    Ok(o)
}

impl DisplayBackend for Ili9341Display {
    fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    fn present(&mut self, frame: &FrameBuffer) {
        let started = Instant::now();
        let w = frame.width.min(self.width) as usize;
        let h = frame.height.min(self.height) as usize;
        for y in 0..h {
            for x in 0..w {
                let px = frame.pixels[y * frame.width as usize + x];
                let r = ((px >> 11) & 0x1f) as u8;
                let g = ((px >> 5) & 0x3f) as u8;
                let b = (px & 0x1f) as u8;
                self.scratch[y * self.width as usize + x] = EgRgb565::new(r, g, b);
            }
        }
        let area = embedded_graphics::primitives::Rectangle::new(
            Point::zero(),
            Size::new(self.width as u32, self.height as u32),
        );
        let result = self
            .display
            .fill_contiguous(&area, self.scratch.iter().copied());
        let elapsed = started.elapsed();
        self.stats
            .record(elapsed, result.err().map(|e| format!("{e:?}")));
    }

    fn present_rect(&mut self, frame: &FrameBuffer, rect: Rect) {
        let r = rect.clamped(self.width, self.height);
        if r.is_empty() {
            return;
        }
        let started = Instant::now();
        let src_w = frame.width as usize;
        let mut n = 0usize;
        for y in r.y..r.y + r.h {
            let row = y as usize * src_w;
            for x in r.x..r.x + r.w {
                let px = frame.pixels[row + x as usize];
                let rr = ((px >> 11) & 0x1f) as u8;
                let g = ((px >> 5) & 0x3f) as u8;
                let b = (px & 0x1f) as u8;
                self.rect_scratch[n] = EgRgb565::new(rr, g, b);
                n += 1;
            }
        }
        let area = embedded_graphics::primitives::Rectangle::new(
            Point::new(i32::from(r.x), i32::from(r.y)),
            Size::new(u32::from(r.w), u32::from(r.h)),
        );
        let result = self
            .display
            .fill_contiguous(&area, self.rect_scratch[..n].iter().copied());
        let elapsed = started.elapsed();
        self.stats
            .record(elapsed, result.err().map(|e| format!("{e:?}")));
    }

    fn blank(&mut self) {
        if self.stats.enabled {
            eprintln!(
                "plippa-ui: display blank() after {} presents (backlight off)",
                self.stats.presents
            );
        }
        self.scratch.fill(EgRgb565::BLACK);
        let area = embedded_graphics::primitives::Rectangle::new(
            Point::zero(),
            Size::new(self.width as u32, self.height as u32),
        );
        let _ = self
            .display
            .fill_contiguous(&area, self.scratch.iter().copied());
        if let Err(e) = self._backlight.set_value(0) {
            eprintln!("plippa-ui: backlight off failed: {e}");
        } else {
            eprintln!("plippa-ui: display blanked (backlight off)");
        }
    }
}

fn open_output(
    pin: u32,
    label: &str,
    initial_high: bool,
) -> Result<linux_embedded_hal::CdevPin, Box<dyn std::error::Error>> {
    let mut chip = Chip::new("/dev/gpiochip0")?;
    let handle = chip.get_line(pin)?.request(
        LineRequestFlags::OUTPUT,
        u8::from(initial_high),
        label,
    )?;
    Ok(linux_embedded_hal::CdevPin::new(handle)?)
}

fn pulse_reset(pin: u32) -> Result<(), Box<dyn std::error::Error>> {
    let mut chip = Chip::new("/dev/gpiochip0")?;
    let handle = chip
        .get_line(pin)?
        .request(LineRequestFlags::OUTPUT, 1, "ili9341_rst_pulse")?;
    handle.set_value(0)?;
    thread::sleep(Duration::from_millis(20));
    handle.set_value(1)?;
    thread::sleep(Duration::from_millis(150));
    Ok(())
}

fn open_backlight(pin: u32) -> Result<LineHandle, Box<dyn std::error::Error>> {
    let mut chip = Chip::new("/dev/gpiochip0")?;
    Ok(chip
        .get_line(pin)?
        .request(LineRequestFlags::OUTPUT, 1, "ili9341_bl")?)
}
