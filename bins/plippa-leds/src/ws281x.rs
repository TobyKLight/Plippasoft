//! The real hardware sink: WS2811 strips through the SoC PWM/DMA engine (`rs_ws281x`).
//!
//! This is the only code in the **deployed** binaries that maps `/dev/mem` (the `eval/` apps
//! still link `rs_ws281x`, but none of them is shipped), and it exists only in a build with
//! the `ws281x` feature (the one `scripts/deploy.sh` builds and gives `cap_sys_rawio+ep`).

use crate::protocol::Layout;
use crate::server::FrameSink;
use plippa_board::BoardProfile;
use rs_ws281x::{ChannelBuilder, Controller, ControllerBuilder, RawColor, StripType};

/// Brightness the strips run at (percent of full scale). Composition already scales colours;
/// this keeps the peak current inside the PCB's budget.
pub const BRIGHTNESS_PCT: u8 = 20;
/// Strip type every POC2 data line uses; colour order is compensated per span in the UI.
const STRIP: StripType = StripType::Ws2811Grb;
/// The SoC's PWM/DMA engine has two channels; there is no third.
pub const MAX_CHANNEL: u8 = 1;
/// BCM pin numbering stops at GPIO 53.
pub const MAX_GPIO: u32 = 53;

/// Refuse a profile line the driver cannot build. The board file sits in the plip-writable
/// checkout, so the numbers are checked here too rather than trusted to `plippa-board`.
fn check_stream(name: &str, channel: u8, gpio: u32) -> Result<(), String> {
    if channel > MAX_CHANNEL {
        return Err(format!(
            "led stream `{name}` uses channel {channel}; the SoC has channels 0..={MAX_CHANNEL}"
        ));
    }
    if gpio > MAX_GPIO {
        return Err(format!(
            "led stream `{name}` uses GPIO {gpio}; BCM pins stop at {MAX_GPIO}"
        ));
    }
    Ok(())
}

/// WS2811 strips for one board profile.
pub struct Ws2811Sink {
    controller: Controller,
    /// (rs_ws281x channel, LED count) per stream, the profile's (name) order.
    channels: Vec<(usize, usize)>,
    layout: Layout,
}

impl Ws2811Sink {
    pub fn open(profile: &BoardProfile, layout: &Layout) -> Result<Self, String> {
        if profile.led_streams.is_empty() {
            return Err("the board profile defines no [led_streams]".into());
        }
        // Both checks run before the controller is built, so a rewritten board file cannot make
        // this CAP_SYS_RAWIO process construct a driver from arbitrary numbers.
        layout
            .validate()
            .map_err(|error| format!("the LED layout is not usable: {error}"))?;
        for (name, stream) in &profile.led_streams {
            check_stream(name, stream.channel, stream.gpio)?;
        }
        let brightness = ((BRIGHTNESS_PCT as u16) * 255 / 100) as u8;
        let mut builder = ControllerBuilder::new();
        builder.freq(800_000).dma(10);
        for stream in profile.led_streams.values() {
            builder.channel(
                stream.channel as usize,
                ChannelBuilder::new()
                    .pin(stream.gpio as i32)
                    .count(stream.count as i32)
                    .strip_type(STRIP)
                    .brightness(brightness)
                    .build(),
            );
        }
        let controller = builder.build().map_err(|error| format!("{error:?}"))?;
        let channels = profile
            .led_streams
            .values()
            .map(|stream| (stream.channel as usize, stream.count as usize))
            .collect();
        Ok(Self {
            controller,
            channels,
            layout: layout.clone(),
        })
    }

    /// Logical RGB -> the driver's BGRW word (the strip order is the library's business).
    fn to_raw(rgb: &[u8]) -> RawColor {
        [rgb[2], rgb[1], rgb[0], 0]
    }
}

impl FrameSink for Ws2811Sink {
    fn show(&mut self, rgb: &[u8]) -> Result<(), String> {
        if rgb.len() != self.layout.payload_len() {
            return Err("frame does not match the layout".into());
        }
        let mut at = 0;
        for &(channel, count) in &self.channels {
            let leds = self.controller.leds_mut(channel);
            for led in leds.iter_mut().take(count) {
                *led = Self::to_raw(&rgb[at..at + 3]);
                at += 3;
            }
        }
        self.controller
            .render()
            .map_err(|error| format!("render failed: {error:?}"))
    }

    fn blank(&mut self) -> Result<(), String> {
        for &(channel, _) in &self.channels {
            for led in self.controller.leds_mut(channel) {
                *led = [0, 0, 0, 0];
            }
        }
        self.controller
            .render()
            .map_err(|error| format!("render failed: {error:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_rgb_becomes_the_drivers_bgrw_word() {
        assert_eq!(Ws2811Sink::to_raw(&[255, 96, 0]), [0, 96, 255, 0]);
        assert_eq!(Ws2811Sink::to_raw(&[0, 0, 255]), [255, 0, 0, 0]);
        assert_eq!(Ws2811Sink::to_raw(&[80, 200, 255]), [255, 200, 80, 0]);
    }

    /// A rewritten board file cannot name a channel or pin the driver does not have.
    #[test]
    fn a_profile_line_the_driver_cannot_build_is_refused() {
        assert!(check_stream("front", 0, 13).is_ok());
        assert!(check_stream("ring", 1, 12).is_ok());
        assert!(check_stream("front", 2, 13).is_err(), "no third PWM channel");
        assert!(check_stream("front", 0, 54).is_err(), "no GPIO above 53");
    }
}
