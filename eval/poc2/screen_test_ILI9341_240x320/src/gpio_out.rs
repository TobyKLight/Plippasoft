use gpio_cdev::{Chip, LineHandle, LineRequestFlags};
use linux_embedded_hal::CdevPin;
use std::thread;
use std::time::Duration;

pub fn open_output(pin: u32, label: &str, initial_high: bool) -> Result<CdevPin, gpio_cdev::Error> {
    let mut chip = Chip::new("/dev/gpiochip0")?;
    let handle = chip.get_line(pin)?.request(
        LineRequestFlags::OUTPUT,
        u8::from(initial_high),
        label,
    )?;
    CdevPin::new(handle)
}

/// Pulse RES low then high so a prior crash or SPI mode mismatch cannot leave the panel stuck.
pub fn pulse_display_reset(pin: u32) -> Result<(), gpio_cdev::Error> {
    let mut chip = Chip::new("/dev/gpiochip0")?;
    let handle = chip
        .get_line(pin)?
        .request(LineRequestFlags::OUTPUT, 1, "rst-pulse")?;
    handle.set_value(0)?;
    thread::sleep(Duration::from_millis(20));
    handle.set_value(1)?;
    thread::sleep(Duration::from_millis(150));
    Ok(())
}

pub struct Backlight {
    handle: LineHandle,
    active_low: bool,
}

impl Backlight {
    pub fn open(pin: u32, active_low: bool) -> Result<Self, gpio_cdev::Error> {
        let mut chip = Chip::new("/dev/gpiochip0")?;
        let on_value = u8::from(!active_low);
        let handle = chip
            .get_line(pin)?
            .request(LineRequestFlags::OUTPUT, on_value, "blk")?;
        Ok(Self { handle, active_low })
    }

    pub fn set_on(&self) -> Result<(), gpio_cdev::Error> {
        self.handle.set_value(u8::from(!self.active_low))
    }

    pub fn set_off(&self) -> Result<(), gpio_cdev::Error> {
        self.handle.set_value(u8::from(self.active_low))
    }
}
