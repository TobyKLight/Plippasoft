//! AW9523B I2C GPIO expander — switch contacts (active-low). Adapted from eval/poc2/ui_test.

use gpio_cdev::{Chip, EventRequestFlags, LineEventHandle, LineHandle, LineRequestFlags};
use i2cdev::core::I2CDevice;
use i2cdev::linux::{LinuxI2CDevice, LinuxI2CError};
use std::path::Path;
use std::thread;
use std::time::Duration;

const REG_INPUT0: u8 = 0x00;
const REG_INPUT1: u8 = 0x01;
const REG_CONFIG0: u8 = 0x04;
const REG_CONFIG1: u8 = 0x05;
const REG_INT0: u8 = 0x06;
const REG_INT1: u8 = 0x07;
const REG_ID: u8 = 0x10;
const REG_GCR: u8 = 0x11;
const REG_LEDMODE0: u8 = 0x12;
const REG_LEDMODE1: u8 = 0x13;
const REG_SOFTRESET: u8 = 0x7F;
const EXPECTED_ID: u8 = 0x23;

/// P0: all 8 lines are switches — enable INT (0 = enabled).
const INT_MASK0: u8 = 0x00;
/// P1: PB0–PB4 are switches; leave PB5–PB7 IRQ off.
const INT_MASK1: u8 = 0xE0;

pub use crate::switches::{SwitchDebouncer, SwitchEvent, SwitchId};

pub struct Aw9523 {
    i2c: LinuxI2CDevice,
    _rst: LineHandle,
}

impl Aw9523 {
    pub fn open(
        bus_path: &Path,
        addr: u16,
        rst_gpio: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let rst = pulse_reset(rst_gpio)?;
        let mut i2c = LinuxI2CDevice::new(bus_path, addr).map_err(|e| {
            format!("open {} @ 0x{addr:02x}: {e}", bus_path.display())
        })?;
        i2c.smbus_write_byte_data(REG_SOFTRESET, 0x00)
            .map_err(map_i2c)?;
        thread::sleep(Duration::from_millis(5));
        let id = i2c.smbus_read_byte_data(REG_ID).map_err(map_i2c)?;
        if id != EXPECTED_ID {
            eprintln!("warning: AW9523 ID=0x{id:02x} (expected 0x{EXPECTED_ID:02x})");
        }
        i2c.smbus_write_byte_data(REG_LEDMODE0, 0xFF).map_err(map_i2c)?;
        i2c.smbus_write_byte_data(REG_LEDMODE1, 0xFF).map_err(map_i2c)?;
        i2c.smbus_write_byte_data(REG_GCR, 0x10).map_err(map_i2c)?;
        i2c.smbus_write_byte_data(REG_CONFIG0, 0xFF).map_err(map_i2c)?;
        i2c.smbus_write_byte_data(REG_CONFIG1, 0xFF).map_err(map_i2c)?;
        // INT enable: bit 0 = interrupt enabled for that pin (datasheet).
        i2c.smbus_write_byte_data(REG_INT0, INT_MASK0).map_err(map_i2c)?;
        i2c.smbus_write_byte_data(REG_INT1, INT_MASK1).map_err(map_i2c)?;
        Ok(Self { i2c, _rst: rst })
    }

    pub fn read_ports(&mut self) -> Result<(u8, u8), Box<dyn std::error::Error>> {
        let p0 = self.i2c.smbus_read_byte_data(REG_INPUT0).map_err(map_i2c)?;
        let p1 = self.i2c.smbus_read_byte_data(REG_INPUT1).map_err(map_i2c)?;
        Ok((p0, p1))
    }
}

/// Open the AW9523 INTN line (active-low, open-drain). Falling edge = pin change
/// after the chip's 8 µs deglitch. Reading input ports over I²C clears INTN.
pub fn open_interrupt(int_gpio: u32) -> Result<LineEventHandle, Box<dyn std::error::Error>> {
    let mut chip = Chip::new("/dev/gpiochip0")?;
    Ok(chip.get_line(int_gpio)?.events(
        LineRequestFlags::INPUT,
        EventRequestFlags::FALLING_EDGE,
        "aw9523_int",
    )?)
}

fn pulse_reset(rst_gpio: u32) -> Result<LineHandle, Box<dyn std::error::Error>> {
    let mut chip = Chip::new("/dev/gpiochip0")?;
    let line = chip
        .get_line(rst_gpio)?
        .request(LineRequestFlags::OUTPUT, 1, "aw9523_rst")?;
    line.set_value(0)?;
    thread::sleep(Duration::from_millis(1));
    line.set_value(1)?;
    thread::sleep(Duration::from_millis(5));
    Ok(line)
}

fn map_i2c(e: LinuxI2CError) -> Box<dyn std::error::Error> {
    format!("AW9523 I2C: {e}").into()
}
