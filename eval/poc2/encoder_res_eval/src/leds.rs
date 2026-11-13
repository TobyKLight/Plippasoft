//! Front-panel SWE3 / SWE4 meter bars (5 LEDs each) with fractional brightness.

use rs_ws281x::{ChannelBuilder, Controller, ControllerBuilder, RawColor, StripType};

pub const PIN_FRONT: i32 = 13;
pub const CH_FRONT: usize = 1;
pub const PIN_TOP: i32 = 12;
pub const CH_TOP: usize = 0;

pub const FRONT_COUNT: usize = 33;
pub const TOP_COUNT: usize = 30;

pub const LED_SWE4_START: usize = 23;
pub const LED_SWE4_LEN: usize = 5;
pub const LED_SWE3_START: usize = 28;
pub const LED_SWE3_LEN: usize = 5;

/// Logical RGB packed as ws2811 0xWWRRGGBB little-endian [B, G, R, W].
pub fn rgb(r: u8, g: u8, b: u8) -> RawColor {
    [b, g, r, 0]
}

pub fn pct_to_brightness(pct: u8) -> u8 {
    ((pct as u16).saturating_mul(255) / 100) as u8
}

pub fn open_strips(
    front_leds: i32,
    top_leds: i32,
    brightness_pct: u8,
) -> Result<Controller, Box<dyn std::error::Error>> {
    let brightness = pct_to_brightness(brightness_pct);
    ControllerBuilder::new()
        .freq(800_000)
        .dma(10)
        .channel(
            CH_TOP,
            ChannelBuilder::new()
                .pin(PIN_TOP)
                .count(top_leds.max(0))
                .strip_type(StripType::Ws2811Grb)
                .brightness(brightness)
                .build(),
        )
        .channel(
            CH_FRONT,
            ChannelBuilder::new()
                .pin(PIN_FRONT)
                .count(front_leds.max(0))
                .strip_type(StripType::Ws2811Grb)
                .brightness(brightness)
                .build(),
        )
        .build()
        .map_err(|e| format!("ws2811 init failed: {e:?} (try sudo)").into())
}

fn scale_rgb(color: RawColor, amount: f32) -> RawColor {
    let a = amount.clamp(0.0, 1.0);
    let scale = |c: u8| -> u8 { ((c as f32) * a).round() as u8 };
    [scale(color[0]), scale(color[1]), scale(color[2]), 0]
}

/// Paint a 0..1 fill across `len` LEDs with fractional brightness on the edge LED.
fn paint_frac_bar(leds: &mut [RawColor], start: usize, len: usize, pct: f32, color: RawColor) {
    let filled = pct.clamp(0.0, 1.0) * len as f32;
    for i in 0..len {
        let amount = (filled - i as f32).clamp(0.0, 1.0);
        leds[start + i] = scale_rgb(color, amount);
    }
}

/// Clear all LEDs, then mirror the same 0..1 fill onto SWE3 and SWE4 bars.
pub fn apply_value_bars(
    controller: &mut Controller,
    pct: f32,
    color: RawColor,
) -> Result<(), Box<dyn std::error::Error>> {
    for ch in controller.channels() {
        for led in controller.leds_mut(ch) {
            *led = [0, 0, 0, 0];
        }
    }

    let front = controller.leds_mut(CH_FRONT);
    if front.len() >= LED_SWE3_START + LED_SWE3_LEN {
        paint_frac_bar(front, LED_SWE4_START, LED_SWE4_LEN, pct, color);
        paint_frac_bar(front, LED_SWE3_START, LED_SWE3_LEN, pct, color);
    }

    controller
        .render()
        .map_err(|e| format!("LED render failed: {e:?}"))?;
    Ok(())
}

pub fn clear_all(controller: &mut Controller) -> Result<(), Box<dyn std::error::Error>> {
    for ch in controller.channels() {
        for led in controller.leds_mut(ch) {
            *led = [0, 0, 0, 0];
        }
    }
    controller
        .render()
        .map_err(|e| format!("LED clear failed: {e:?}"))?;
    Ok(())
}
