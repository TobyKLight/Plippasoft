use crate::gpio_out::{open_output, pulse_display_reset, Backlight};
use crate::{DisplayConfig, PIN_BLK, PIN_DC, PIN_RST};
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use linux_embedded_hal::spidev::{SpiModeFlags, SpidevOptions};
use linux_embedded_hal::{Delay, SpidevDevice};
use mipidsi::{
    interface::SpiInterface,
    models::ILI9341Rgb565,
    options::{ColorInversion, ColorOrder, Orientation, Rotation},
    Builder,
};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy)]
struct Combo {
    label: &'static str,
    rotation: Rotation,
    offset_x: u16,
    offset_y: u16,
    invert: ColorInversion,
    color_order: ColorOrder,
    spi_mode: SpiModeFlags,
    spi_speed_hz: u32,
}

const COMBOS: [Combo; 10] = [
    Combo {
        label: "rot0 off(0,0) rgb mode0",
        rotation: Rotation::Deg0,
        offset_x: 0,
        offset_y: 0,
        invert: ColorInversion::Normal,
        color_order: ColorOrder::Rgb,
        spi_mode: SpiModeFlags::SPI_MODE_0,
        spi_speed_hz: 10_000_000,
    },
    Combo {
        label: "rot0 off(0,0) bgr mode0",
        rotation: Rotation::Deg0,
        offset_x: 0,
        offset_y: 0,
        invert: ColorInversion::Normal,
        color_order: ColorOrder::Bgr,
        spi_mode: SpiModeFlags::SPI_MODE_0,
        spi_speed_hz: 10_000_000,
    },
    Combo {
        label: "rot0 off(0,0) inv rgb mode0",
        rotation: Rotation::Deg0,
        offset_x: 0,
        offset_y: 0,
        invert: ColorInversion::Inverted,
        color_order: ColorOrder::Rgb,
        spi_mode: SpiModeFlags::SPI_MODE_0,
        spi_speed_hz: 10_000_000,
    },
    Combo {
        label: "rot90 off(0,0) rgb mode0",
        rotation: Rotation::Deg90,
        offset_x: 0,
        offset_y: 0,
        invert: ColorInversion::Normal,
        color_order: ColorOrder::Rgb,
        spi_mode: SpiModeFlags::SPI_MODE_0,
        spi_speed_hz: 10_000_000,
    },
    Combo {
        label: "rot180 off(0,0) rgb mode0",
        rotation: Rotation::Deg180,
        offset_x: 0,
        offset_y: 0,
        invert: ColorInversion::Normal,
        color_order: ColorOrder::Rgb,
        spi_mode: SpiModeFlags::SPI_MODE_0,
        spi_speed_hz: 10_000_000,
    },
    Combo {
        label: "rot270 off(0,0) rgb mode0",
        rotation: Rotation::Deg270,
        offset_x: 0,
        offset_y: 0,
        invert: ColorInversion::Normal,
        color_order: ColorOrder::Rgb,
        spi_mode: SpiModeFlags::SPI_MODE_0,
        spi_speed_hz: 10_000_000,
    },
    Combo {
        label: "rot0 off(0,0) rgb mode3",
        rotation: Rotation::Deg0,
        offset_x: 0,
        offset_y: 0,
        invert: ColorInversion::Normal,
        color_order: ColorOrder::Rgb,
        spi_mode: SpiModeFlags::SPI_MODE_3,
        spi_speed_hz: 10_000_000,
    },
    Combo {
        label: "rot0 off(0,0) bgr mode3",
        rotation: Rotation::Deg0,
        offset_x: 0,
        offset_y: 0,
        invert: ColorInversion::Normal,
        color_order: ColorOrder::Bgr,
        spi_mode: SpiModeFlags::SPI_MODE_3,
        spi_speed_hz: 10_000_000,
    },
    Combo {
        label: "rot0 off(0,0) 1MHz slow",
        rotation: Rotation::Deg0,
        offset_x: 0,
        offset_y: 0,
        invert: ColorInversion::Normal,
        color_order: ColorOrder::Rgb,
        spi_mode: SpiModeFlags::SPI_MODE_0,
        spi_speed_hz: 1_000_000,
    },
    Combo {
        label: "rot0 off(0,0) inv bgr mode0",
        rotation: Rotation::Deg0,
        offset_x: 0,
        offset_y: 0,
        invert: ColorInversion::Inverted,
        color_order: ColorOrder::Bgr,
        spi_mode: SpiModeFlags::SPI_MODE_0,
        spi_speed_hz: 10_000_000,
    },
];

pub fn run_smoke(_default_spi_speed_hz: u32) -> Result<(), Box<dyn std::error::Error>> {
    println!("SMOKE — backlight ON, then 4-quadrant colours per combo (~6s each).");
    println!("Default expected: rot0 rgb SPI mode 0 + CE0 (combo 1).");
    println!("Ctrl-C to stop.\n");

    let backlight = Backlight::open(PIN_BLK, false)?;
    backlight.set_on()?;

    for (i, combo) in COMBOS.iter().enumerate() {
        println!("\n=== [{}/{}] {} ===", i + 1, COMBOS.len(), combo.label);
        match try_combo(*combo) {
            Ok(()) => {}
            Err(e) => println!("  skipped: {e}"),
        }
    }

    backlight.set_off()?;
    println!("\nDone.");
    Ok(())
}

fn try_combo(combo: Combo) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = DisplayConfig {
        rotation: combo.rotation,
        offset_x: combo.offset_x,
        offset_y: combo.offset_y,
        ..DisplayConfig::default()
    };

    let mut delay = Delay;
    let mut spi = SpidevDevice::open("/dev/spidev0.0")
        .map_err(|e| format!("open spidev0.0: {e}"))?;

    spi.configure(
        &SpidevOptions::new()
            .max_speed_hz(combo.spi_speed_hz)
            .mode(combo.spi_mode)
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
        .color_order(combo.color_order)
        .invert_colors(combo.invert)
        .reset_pin(&mut rst)
        .init(&mut delay)
        .map_err(|e| format!("init: {e:?}"))?;

    for (name, color) in [
        ("RED full", Rgb565::RED),
        ("GREEN full", Rgb565::GREEN),
        ("BLUE full", Rgb565::BLUE),
    ] {
        display
            .clear(color)
            .map_err(|e| format!("clear {name}: {e:?}"))?;
        println!("  {name}…");
        thread::sleep(Duration::from_millis(800));
    }

    display
        .clear(Rgb565::BLACK)
        .map_err(|e| format!("clear black: {e:?}"))?;
    let half_w = cfg.width / 2;
    let half_h = cfg.height / 2;
    let quads = [
        (0, 0, Rgb565::RED),
        (half_w, 0, Rgb565::GREEN),
        (0, half_h, Rgb565::BLUE),
        (half_w, half_h, Rgb565::YELLOW),
    ];
    for (x, y, color) in quads {
        Rectangle::new(
            Point::new(x as i32, y as i32),
            Size::new(u32::from(half_w), u32::from(half_h)),
        )
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(&mut display)
        .map_err(|e| format!("quad draw: {e:?}"))?;
    }
    println!("  4-quadrant RG/B/Y…");
    thread::sleep(Duration::from_millis(1500));

    Ok(())
}
