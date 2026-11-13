//! QR capacity probe for the 320×240 POC2 panel.
//!
//! Answers one question empirically: **how small a QR module still scans on this panel**, and
//! therefore **how long a URL the connect/buy QR can hold**.
//!
//! The panel is driven exactly like `plippa-ui` drives it (native 240×320, rotation 90,
//! horizontal flip, RGB colour order), so a symbol that scans here scans on the product screen.
//! The symbol is painted white-on-black at maximum contrast — the UI's own palette is close to
//! this, so these numbers are the best case by a hair, not a different experiment.
//!
//! Modes
//!   --mode px     (default) one payload, sweep module sizes 6→2 so a person can scan each and
//!                  report the smallest that still reads. That size is the panel's limit.
//!   --mode len    one module size, sweep the payload length upward to the box limit.
//!   --list        no display: print the capacity table for every module size and ECC level.
//!
//! Examples
//!   qr_probe --mode px --len 100 --dwell 10
//!   qr_probe --mode len --px 3 --dwell 8
//!   qr_probe --box 120 --list          # what the current 120 px product box can hold
//!   qr_probe --list                    # what a full-height symbol can hold

mod gpio_out;

use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    primitives::{PrimitiveStyle, Rectangle},
    text::{Baseline, Text},
};
use gpio_out::{open_output, pulse_display_reset, Backlight};
use linux_embedded_hal::spidev::{SpiModeFlags, SpidevOptions};
use linux_embedded_hal::{Delay, SpidevDevice};
use mipidsi::{
    interface::SpiInterface,
    models::ILI9341Rgb565,
    options::{ColorInversion, ColorOrder, Orientation, Rotation},
    Builder,
};
use plippa_core::qr::{QrErrorLevel, QrMatrix};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(true);

const SPI_DEV: &str = "/dev/spidev0.0";
const PIN_DC: u32 = 24;
const PIN_RST: u32 = 25;
const PIN_BLK: u32 = 16;

/// Native panel size before rotation (board.toml `display.width/height`).
const NATIVE_W: u16 = 240;
const NATIVE_H: u16 = 320;
/// Logical size after rotation 90.
const LOGICAL_W: u16 = 320;
const LOGICAL_H: u16 = 240;

/// The product's quiet zone (`STORE_QR_QUIET_MODULES`). Kept identical so the box arithmetic
/// matches what the UI can actually paint.
const QUIET_MODULES: u32 = 2;
/// The product's minimum module (`STORE_QR_MIN_MODULE_PX`), measured on this panel: a
/// version-4 symbol at 2 px per module scanned reliably, so that is the floor.
const PRODUCT_MIN_MODULE_PX: u16 = 2;

/// Prefix used for every generated payload: a realistic portal URL, so the symbol's data
/// pattern resembles the real thing instead of a run of one character.
const URL_PREFIX: &str = "https://plippa.local:8787/library?store=bandcamp&page=";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Px,
    Len,
    List,
}

struct Args {
    mode: Mode,
    /// Module size for `--mode len`.
    px: u16,
    /// Payload length for `--mode px`.
    len: usize,
    /// Explicit payload lengths for `--mode len` (empty = built-in geometric ladder).
    ladder: Vec<usize>,
    /// Side of the square the symbol may use, quiet zone included.
    box_side: u16,
    /// Seconds each configuration stays on screen.
    dwell: u64,
    /// Run the steps once and hold the last symbol instead of cycling forever.
    once: bool,
    level: QrErrorLevel,
    spi_dev: String,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            mode: Mode::Px,
            px: PRODUCT_MIN_MODULE_PX,
            len: 60,
            ladder: Vec::new(),
            // The largest square that fits the 320x240 logical screen.
            box_side: LOGICAL_H,
            dwell: 10,
            once: false,
            level: QrErrorLevel::Medium,
            spi_dev: SPI_DEV.to_string(),
        }
    }
}

fn level_name(level: QrErrorLevel) -> char {
    match level {
        QrErrorLevel::Low => 'L',
        QrErrorLevel::Medium => 'M',
        QrErrorLevel::Quartile => 'Q',
        QrErrorLevel::High => 'H',
    }
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let flag = argv[i].as_str();
        let mut value = || -> Result<String, String> {
            i += 1;
            argv.get(i)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--mode" => {
                args.mode = match value()?.as_str() {
                    "px" => Mode::Px,
                    "len" => Mode::Len,
                    "list" => Mode::List,
                    other => return Err(format!("--mode must be px|len|list, got {other}")),
                }
            }
            "--list" => args.mode = Mode::List,
            "--px" => args.px = value()?.parse().map_err(|_| "--px wants a number")?,
            "--len" => args.len = value()?.parse().map_err(|_| "--len wants a number")?,
            "--ladder" => {
                let raw = value()?;
                args.ladder = raw
                    .split(',')
                    .map(|part| part.trim().parse::<usize>())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| "--ladder wants comma-separated byte counts, e.g. 60,120,240")?;
                if args.ladder.is_empty() {
                    return Err("--ladder needs at least one value".into());
                }
            }
            "--box" => args.box_side = value()?.parse().map_err(|_| "--box wants a number")?,
            "--dwell" => args.dwell = value()?.parse().map_err(|_| "--dwell wants seconds")?,
            "--once" => args.once = true,
            "--spi" => args.spi_dev = value()?,
            "--ecc" => {
                args.level = match value()?.to_ascii_lowercase().as_str() {
                    "l" | "low" => QrErrorLevel::Low,
                    "m" | "medium" => QrErrorLevel::Medium,
                    "q" | "quartile" => QrErrorLevel::Quartile,
                    "h" | "high" => QrErrorLevel::High,
                    other => return Err(format!("--ecc must be l|m|q|h, got {other}")),
                }
            }
            other => return Err(format!("unknown argument {other} (try --help)")),
        }
        i += 1;
    }
    if args.px == 0 {
        return Err("--px must be at least 1".into());
    }
    Ok(args)
}

const HELP: &str = "\
qr_probe — QR capacity probe for the 320x240 panel

  --mode px|len|list   px sweep (default) | length sweep | capacity table only
  --px N               module size for --mode len (default 4, the product minimum)
  --len N              payload length for --mode px (default 100)
  --box N              square the symbol may use, quiet zone included (default 240)
  --dwell S            seconds per configuration on screen (default 10)
  --once               run the steps once and hold the last symbol (default: cycle forever)
  --ecc l|m|q|h        error-correction level (default m — what the product tries first)
  --spi PATH           SPI device (default /dev/spidev0.0)
";

/// A payload of exactly `len` characters that still looks like a URL.
fn payload(len: usize) -> String {
    if len <= URL_PREFIX.len() {
        return URL_PREFIX[..len].to_string();
    }
    let mut out = String::with_capacity(len);
    out.push_str(URL_PREFIX);
    // Vary the tail so the symbol is not a single repeated module row.
    const TAIL: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut n = 0usize;
    while out.len() < len {
        out.push(TAIL[n % TAIL.len()] as char);
        n += 1;
    }
    out
}

/// Module size that fits `modules` into `box_side` with the quiet zone, product arithmetic.
fn module_px(modules: u32, box_side: u16) -> Option<u16> {
    let total = modules.checked_add(QUIET_MODULES * 2)?;
    let fit = u32::from(box_side) / total;
    u16::try_from(fit).ok().filter(|px| *px >= 1)
}

/// Largest payload (bytes) that fits `box_side` at `px` modules for `level`.
///
/// Capacity grows monotonically with length, so a binary search over the length avoids
/// thousands of encodes on the Pi (the linear version took minutes).
fn max_payload(box_side: u16, px: u16, level: QrErrorLevel) -> Option<(usize, u32)> {
    let limit_total = u32::from(box_side) / u32::from(px.max(1));
    if limit_total <= QUIET_MODULES * 2 {
        return None;
    }
    let max_modules = limit_total - QUIET_MODULES * 2;
    // qrcodegen's hard ceiling is 2953 bytes at version 40 / ECC Low.
    const CEILING: usize = 3000;
    let fits = |len: usize| match QrMatrix::encode(&payload(len), level) {
        Ok(m) => (m.width <= max_modules).then_some(m.width),
        Err(_) => None,
    };
    if fits(1).is_none() {
        return None;
    }
    let (mut lo, mut hi) = (1usize, CEILING);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(mid).is_some() {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    fits(lo).map(|modules| (lo, modules))
}

fn print_table(args: &Args) {
    println!(
        "capacity for a {} px box, quiet zone {} modules, ECC as noted",
        args.box_side, QUIET_MODULES
    );
    println!("  px  ecc  modules  version   max bytes  side(px)");
    for px in 1..=8u16 {
        for level in [QrErrorLevel::Low, QrErrorLevel::Medium] {
            match max_payload(args.box_side, px, level) {
                Some((bytes, modules)) => {
                    let side = (modules + QUIET_MODULES * 2) * u32::from(px);
                    println!(
                        "  {:>2}   {}   {:>7}  {:>7}   {:>9}  {:>8}",
                        px,
                        level_name(level),
                        modules,
                        (modules - 17) / 4,
                        bytes,
                        side
                    );
                }
                None => println!(
                    "  {:>2}   {}         -        -           -         -",
                    px,
                    level_name(level)
                ),
            }
        }
    }
    println!(
        "\nthe product paints a 120 px box and refuses below {} px per module",
        PRODUCT_MIN_MODULE_PX
    );
    for level in [QrErrorLevel::Medium, QrErrorLevel::Low] {
        if let Some((bytes, modules)) = max_payload(120, PRODUCT_MIN_MODULE_PX, level) {
            println!(
                "  product box, {} px modules, ECC {}: {} bytes (version {}, {} modules)",
                PRODUCT_MIN_MODULE_PX,
                level_name(level),
                bytes,
                (modules - 17) / 4,
                modules
            );
        }
    }
}

fn describe(len: usize, level: QrErrorLevel, box_side: u16) -> Option<(QrMatrix, u16)> {
    let matrix = QrMatrix::encode(&payload(len), level).ok()?;
    let px = module_px(matrix.width, box_side)?;
    Some((matrix, px))
}

/// Draw one symbol, centred, white field and black modules (maximum contrast for a phone
/// camera), with step labels in the margin the symbol leaves free.
fn draw(
    display: &mut impl DrawTarget<Color = Rgb565, Error = impl std::fmt::Debug>,
    matrix: &QrMatrix,
    px: u16,
    labels: &[String],
) -> Result<(), String> {
    let quiet = QUIET_MODULES as u16;
    let side = (matrix.width as u16 + quiet * 2).saturating_mul(px);
    let x0 = (LOGICAL_W.saturating_sub(side)) / 2;
    let y0 = (LOGICAL_H.saturating_sub(side)) / 2;

    Rectangle::new(Point::zero(), Size::new(LOGICAL_W as u32, LOGICAL_H as u32))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE))
        .draw(display)
        .map_err(|e| format!("clear: {e:?}"))?;

    // Horizontal runs, not single modules: one SPI window per run instead of per module.
    for my in 0..matrix.width {
        let mut mx = 0;
        while mx < matrix.width {
            if !matrix.get(mx, my) {
                mx += 1;
                continue;
            }
            let start = mx;
            while mx < matrix.width && matrix.get(mx, my) {
                mx += 1;
            }
            let x = x0 + (start as u16 + quiet) * px;
            let y = y0 + (my as u16 + quiet) * px;
            let w = (mx - start) as u16 * px;
            Rectangle::new(Point::new(x as i32, y as i32), Size::new(w as u32, px as u32))
                .into_styled(PrimitiveStyle::with_fill(Rgb565::BLACK))
                .draw(display)
                .map_err(|e| format!("module run: {e:?}"))?;
        }
    }

    // Labels sit in the free margin (left column when the symbol is full-height). They are for
    // the person holding the phone, not for the scanner.
    if x0 >= 30 {
        let style = MonoTextStyle::new(&FONT_10X20, Rgb565::BLACK);
        for (n, text) in labels.iter().enumerate() {
            Text::with_baseline(text, Point::new(2, 8 + n as i32 * 22), style, Baseline::Top)
                .draw(display)
                .map_err(|e| format!("label: {e:?}"))?;
        }
    }
    Ok(())
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    if args.mode == Mode::List {
        print_table(&args);
        return Ok(());
    }

    ctrlc::set_handler(|| {
        RUNNING.store(false, Ordering::SeqCst);
    })
    .map_err(|e| format!("ctrlc: {e}"))?;

    // Planned steps: (module px, payload len).
    let steps: Vec<(u16, usize)> = match args.mode {
        Mode::Px => (2..=6u16).rev().map(|px| (px, args.len)).collect(),
        Mode::Len => {
            let ladder: Vec<usize> = if args.ladder.is_empty() {
                vec![60, 90, 130, 180, 240, 320, 420, 550, 700, 900, 1100, 1400]
            } else {
                args.ladder.clone()
            };
            // Stop where the box runs out: past that every step would look identical.
            let mut v = Vec::new();
            for len in ladder {
                if describe(len, args.level, args.box_side).is_none() {
                    println!("  (ladder stops at {len} bytes: does not fit this box)");
                    break;
                }
                v.push((args.px, len));
            }
            v
        }
        Mode::List => unreachable!("handled above"),
    };
    if steps.is_empty() {
        return Err("nothing fits: raise --box or lower --px".into());
    }

    println!(
        "qr_probe: {} steps, ECC {}, box {} px, quiet {} modules, dwell {} s",
        steps.len(),
        level_name(args.level),
        args.box_side,
        QUIET_MODULES,
        args.dwell
    );
    for (px, len) in &steps {
        match describe(*len, args.level, args.box_side) {
            Some((m, fit)) => println!(
                "  plan: {:>4} bytes at {} px modules -> version {}, {} modules, {:.0} px symbol (fit {})",
                len,
                px,
                (m.width - 17) / 4,
                m.width,
                f64::from(m.width + QUIET_MODULES * 2) * f64::from(*px),
                fit
            ),
            None => println!("  plan: {:>4} bytes at {} px modules -> does not fit", len, px),
        }
    }

    let backlight = Backlight::open(PIN_BLK, false).map_err(|e| format!("backlight: {e}"))?;
    backlight.set_on().map_err(|e| format!("backlight on: {e}"))?;
    let mut delay = Delay;
    let mut spi = SpidevDevice::open(&args.spi_dev)
        .map_err(|e| format!("cannot open {} ({e}) — enable SPI with dtoverlay=spi0-1cs,no_miso", args.spi_dev))?;
    spi.configure(
        &SpidevOptions::new()
            .max_speed_hz(32_000_000)
            .mode(SpiModeFlags::SPI_MODE_0)
            .build(),
    )
    .map_err(|e| format!("spi configure: {e}"))?;
    pulse_display_reset(PIN_RST).map_err(|e| format!("reset pulse: {e}"))?;
    let dc = open_output(PIN_DC, "dc", false).map_err(|e| format!("dc pin: {e}"))?;
    let rst = open_output(PIN_RST, "rst", true).map_err(|e| format!("rst pin: {e}"))?;
    let mut buffer = [0_u8; 4096];
    let di = SpiInterface::new(spi, dc, &mut buffer);
    // Same orientation as plippa-ui: board.toml rotation 90 + mirrored true.
    let orientation = Orientation::new()
        .rotate(Rotation::Deg90)
        .flip_horizontal();
    let mut display = Builder::new(ILI9341Rgb565, di)
        .display_size(NATIVE_W, NATIVE_H)
        .orientation(orientation)
        .color_order(ColorOrder::Rgb)
        .invert_colors(ColorInversion::Normal)
        .reset_pin(rst)
        .init(&mut delay)
        .map_err(|e| format!("display init: {e:?}"))?;

    let mut index = 0usize;
    let mut last_shown: Option<(u16, usize)> = None;
    while RUNNING.load(Ordering::SeqCst) {
        if index >= steps.len() {
            if args.once {
                break;
            }
            // Cycle: whoever walks up to the panel can scan every size without a re-run.
            index = 0;
            last_shown = None;
            println!("-- next pass --");
        }
        let (planned_px, len) = steps[index];
        let Some((matrix, fit_px)) = describe(len, args.level, args.box_side) else {
            println!("step {}: {} bytes does not fit — skipping", index + 1, len);
            index += 1;
            continue;
        };
        let px = match args.mode {
            // Both sweeps honour the requested module size; a payload that cannot reach it is
            // drawn at the largest size that fits, so the overflow is visible.
            Mode::Px | Mode::Len => planned_px.min(fit_px.max(1)),
            Mode::List => unreachable!(),
        };
        if last_shown == Some((px, len)) {
            // The payload already needs smaller modules than this step asks for; showing the
            // same symbol twice would only confuse the person scanning it.
            println!("step {}/{}: same symbol as the previous step — skipped", index + 1, steps.len());
            index += 1;
            continue;
        }
        last_shown = Some((px, len));

        let version = (matrix.width - 17) / 4;
        let labels = [
            format!("S{}", index + 1),
            format!("{}p", px),
            format!("v{}", version),
        ];
        draw(&mut display, &matrix, px, &labels)?;
        println!(
            "step {}/{}: {} bytes, version {}, {} modules, {} px per module, {} px symbol{}",
            index + 1,
            steps.len(),
            len,
            version,
            matrix.width,
            px,
            (matrix.width + QUIET_MODULES * 2) * u32::from(px),
            if px < planned_px {
                format!(" (cannot fit at {} px, shrunk)", planned_px)
            } else {
                String::new()
            }
        );

        let deadline = Duration::from_secs(args.dwell);
        let started = std::time::Instant::now();
        while started.elapsed() < deadline && RUNNING.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
        index += 1;
    }
    match args.mode {
        Mode::Px if args.once => println!("px sweep finished — the smallest module that still scanned is the panel limit"),
        Mode::Len if args.once => println!("length sweep finished — the last symbol that scanned is the capacity bound"),
        _ => {}
    }
    // Hold the last symbol (and the backlight) so a person who looked away can still scan it.
    if args.once && RUNNING.load(Ordering::SeqCst) {
        println!("holding the last symbol on screen — Ctrl-C to exit");
        while RUNNING.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(200));
        }
    }
    Ok(())
}
