//! WS2811 LED framebuffer + LEDPATCH index map.

use rs_ws281x::{ChannelBuilder, Controller, ControllerBuilder, RawColor, StripType};
use std::time::{Duration, Instant};

pub const PIN_FRONT: i32 = 13;
pub const CH_FRONT: usize = 1;
pub const PIN_TOP: i32 = 12;
pub const CH_TOP: usize = 0;

pub const FRONT_COUNT: usize = 33;
pub const TOP_COUNT: usize = 30;

// Front panel (F_LED_DATA)
pub const LED_SW6: usize = 0;
pub const LED_VUL_START: usize = 1;
pub const LED_VUL_LEN: usize = 10;
pub const LED_VUR_START: usize = 11;
pub const LED_VUR_LEN: usize = 10;
pub const LED_PWR: usize = 21;
pub const LED_SW2: usize = 22;
pub const LED_SWE4_START: usize = 23;
pub const LED_SWE4_LEN: usize = 5;
pub const LED_SWE3_START: usize = 28;
pub const LED_SWE3_LEN: usize = 5;

// Top panel (BT_LED_DATA)
pub const LED_NFC_START: usize = 0;
pub const LED_NFC_LEN: usize = 24;
pub const LED_BT_SW1: usize = 24;
pub const LED_BT_SW2: usize = 25;
pub const LED_BT_SW3: usize = 26;
pub const LED_BT_SW4: usize = 27;
pub const LED_BT_SW5: usize = 28;
pub const LED_BT_SW6: usize = 29;

// ws2811_led_t is 0xWWRRGGBB; little-endian RawColor = [B, G, R, W].
// StripType::Ws2811Grb only affects wire order — write logical RGB here.
const RED: RawColor = [0, 0, 255, 0];
const GREEN: RawColor = [0, 255, 0, 0];
const BLUE: RawColor = [255, 0, 0, 0];

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

/// Logical LED state driven by UI events.
pub struct LedState {
    pub front: [RawColor; FRONT_COUNT],
    pub top: [RawColor; TOP_COUNT],
    pub base: RawColor,
    pub vul_level: usize,
    pub vur_level: usize,
    pub swe3_level: usize,
    pub swe4_level: usize,
    pub power_blue: bool,
    /// NFC chase: None = idle; Some((index, next_frame_at)) while animating.
    chase: Option<(usize, Instant)>,
    chase_frame_ms: u64,
}

impl LedState {
    pub fn new_all_base(chase_fps: u64, base: RawColor) -> Self {
        Self {
            front: [base; FRONT_COUNT],
            top: [base; TOP_COUNT],
            base,
            vul_level: 0,
            vur_level: 0,
            swe3_level: 0,
            swe4_level: 0,
            power_blue: false,
            chase: None,
            chase_frame_ms: (1000 / chase_fps.max(1)).max(1),
        }
    }

    /// SW7: low → green base, high → red base. Remap non-blue LEDs.
    pub fn set_base_from_sw7(&mut self, sw7_low: bool) {
        let new_base = if sw7_low { GREEN } else { RED };
        if new_base == self.base {
            return;
        }
        let old = self.base;
        self.base = new_base;
        for led in self.front.iter_mut().chain(self.top.iter_mut()) {
            if *led == old {
                *led = new_base;
            }
        }
        // Re-paint meters so unfilled slots match new base.
        self.repaint_meters();
    }

    pub fn base_name(&self) -> &'static str {
        if self.base == GREEN {
            "green"
        } else {
            "red"
        }
    }

    fn repaint_meters(&mut self) {
        let base = self.base;
        paint_meter(&mut self.front, LED_VUL_START, LED_VUL_LEN, self.vul_level, base);
        paint_meter(&mut self.front, LED_VUR_START, LED_VUR_LEN, self.vur_level, base);
        paint_meter(
            &mut self.front,
            LED_SWE3_START,
            LED_SWE3_LEN,
            self.swe3_level,
            base,
        );
        paint_meter(
            &mut self.front,
            LED_SWE4_START,
            LED_SWE4_LEN,
            self.swe4_level,
            base,
        );
        if !self.power_blue {
            self.front[LED_PWR] = base;
        }
    }

    pub fn toggle_button_led(&mut self, front_or_top: bool, index: usize) {
        let led = if front_or_top {
            &mut self.front[index]
        } else {
            &mut self.top[index]
        };
        *led = if *led == BLUE { self.base } else { BLUE };
    }

    pub fn toggle_power(&mut self) {
        self.power_blue = !self.power_blue;
        self.front[LED_PWR] = if self.power_blue { BLUE } else { self.base };
    }

    pub fn adjust_meter(&mut self, which: Meter, delta: i32) {
        let base = self.base;
        match which {
            Meter::Vul => {
                self.vul_level = clamp_level(self.vul_level, delta, LED_VUL_LEN);
                paint_meter(&mut self.front, LED_VUL_START, LED_VUL_LEN, self.vul_level, base);
            }
            Meter::Vur => {
                self.vur_level = clamp_level(self.vur_level, delta, LED_VUR_LEN);
                paint_meter(&mut self.front, LED_VUR_START, LED_VUR_LEN, self.vur_level, base);
            }
            Meter::Swe3 => {
                self.swe3_level = clamp_level(self.swe3_level, delta, LED_SWE3_LEN);
                paint_meter(
                    &mut self.front,
                    LED_SWE3_START,
                    LED_SWE3_LEN,
                    self.swe3_level,
                    base,
                );
            }
            Meter::Swe4 => {
                self.swe4_level = clamp_level(self.swe4_level, delta, LED_SWE4_LEN);
                paint_meter(
                    &mut self.front,
                    LED_SWE4_START,
                    LED_SWE4_LEN,
                    self.swe4_level,
                    base,
                );
            }
        }
    }

    pub fn start_nfc_chase(&mut self) {
        let base = self.base;
        for i in 0..LED_NFC_LEN {
            self.top[LED_NFC_START + i] = base;
        }
        self.top[LED_NFC_START] = BLUE;
        self.chase = Some((0, Instant::now() + Duration::from_millis(self.chase_frame_ms)));
    }

    /// Advance chase animation; returns true if framebuffer changed.
    pub fn tick_chase(&mut self) -> bool {
        let Some((idx, next_at)) = self.chase else {
            return false;
        };
        if Instant::now() < next_at {
            return false;
        }

        self.top[LED_NFC_START + idx] = self.base;
        let next = idx + 1;
        if next >= LED_NFC_LEN {
            self.chase = None;
            return true;
        }
        self.top[LED_NFC_START + next] = BLUE;
        self.chase = Some((
            next,
            Instant::now() + Duration::from_millis(self.chase_frame_ms),
        ));
        true
    }

    pub fn apply_to(&self, controller: &mut Controller) {
        let front = controller.leds_mut(CH_FRONT);
        let n = front.len().min(FRONT_COUNT);
        front[..n].copy_from_slice(&self.front[..n]);

        let top = controller.leds_mut(CH_TOP);
        let n = top.len().min(TOP_COUNT);
        top[..n].copy_from_slice(&self.top[..n]);
    }

    pub fn clear_all(&mut self) {
        self.front = [rgb(0, 0, 0); FRONT_COUNT];
        self.top = [rgb(0, 0, 0); TOP_COUNT];
        self.chase = None;
    }
}

#[derive(Clone, Copy)]
pub enum Meter {
    Vul,
    Vur,
    Swe3,
    Swe4,
}

fn clamp_level(level: usize, delta: i32, max: usize) -> usize {
    let next = level as i32 + delta;
    next.clamp(0, max as i32) as usize
}

fn paint_meter(leds: &mut [RawColor], start: usize, len: usize, level: usize, base: RawColor) {
    for i in 0..len {
        leds[start + i] = if i < level { BLUE } else { base };
    }
}
