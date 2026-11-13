//! AW9523B I2C GPIO expander — switch contacts (active-low inputs).

use gpio_cdev::{Chip, LineHandle, LineRequestFlags};
use i2cdev::core::I2CDevice;
use i2cdev::linux::{LinuxI2CDevice, LinuxI2CError};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

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

const PIN_RST: u32 = 22;
const EXPECTED_ID: u8 = 0x23;

/// Bit masks on Port0 (PA) / Port1 (PB) — wiring CSV.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchId {
    BtSw1, // PA0
    BtSw2, // PA1
    BtSw3, // PA2
    BtSw4, // PA3
    BtSw5, // PA4
    BtSw6, // PA5
    Sw2,   // PA6
    Sw6,   // PA7
    Sw7,   // PB0 (log only)
    Swe1,  // PB1
    Swe3,  // PB2
    Swe4,  // PB3
    Swe5,  // PB4
}

impl SwitchId {
    const ALL: [SwitchId; 13] = [
        SwitchId::BtSw1,
        SwitchId::BtSw2,
        SwitchId::BtSw3,
        SwitchId::BtSw4,
        SwitchId::BtSw5,
        SwitchId::BtSw6,
        SwitchId::Sw2,
        SwitchId::Sw6,
        SwitchId::Sw7,
        SwitchId::Swe1,
        SwitchId::Swe3,
        SwitchId::Swe4,
        SwitchId::Swe5,
    ];

    fn port_bit(self) -> (u8, u8) {
        match self {
            SwitchId::BtSw1 => (0, 1 << 0),
            SwitchId::BtSw2 => (0, 1 << 1),
            SwitchId::BtSw3 => (0, 1 << 2),
            SwitchId::BtSw4 => (0, 1 << 3),
            SwitchId::BtSw5 => (0, 1 << 4),
            SwitchId::BtSw6 => (0, 1 << 5),
            SwitchId::Sw2 => (0, 1 << 6),
            SwitchId::Sw6 => (0, 1 << 7),
            SwitchId::Sw7 => (1, 1 << 0),
            SwitchId::Swe1 => (1, 1 << 1),
            SwitchId::Swe3 => (1, 1 << 2),
            SwitchId::Swe4 => (1, 1 << 3),
            SwitchId::Swe5 => (1, 1 << 4),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SwitchId::BtSw1 => "BT_SW1",
            SwitchId::BtSw2 => "BT_SW2",
            SwitchId::BtSw3 => "BT_SW3",
            SwitchId::BtSw4 => "BT_SW4",
            SwitchId::BtSw5 => "BT_SW5",
            SwitchId::BtSw6 => "BT_SW6",
            SwitchId::Sw2 => "SW2",
            SwitchId::Sw6 => "SW6",
            SwitchId::Sw7 => "SW7",
            SwitchId::Swe1 => "SWE1",
            SwitchId::Swe3 => "SWE3",
            SwitchId::Swe4 => "SWE4",
            SwitchId::Swe5 => "SWE5",
        }
    }

    pub fn is_swe(self) -> bool {
        matches!(
            self,
            SwitchId::Swe1 | SwitchId::Swe3 | SwitchId::Swe4 | SwitchId::Swe5
        )
    }

    pub fn has_toggle_led(self) -> bool {
        matches!(
            self,
            SwitchId::BtSw1
                | SwitchId::BtSw2
                | SwitchId::BtSw3
                | SwitchId::BtSw4
                | SwitchId::BtSw5
                | SwitchId::BtSw6
                | SwitchId::Sw2
                | SwitchId::Sw6
        )
    }
}

pub struct Aw9523 {
    i2c: LinuxI2CDevice,
    _rst: LineHandle,
}

impl Aw9523 {
    pub fn open(bus_path: &Path, addr: u16) -> Result<Self, Box<dyn std::error::Error>> {
        let rst = pulse_reset()?;
        let mut i2c = LinuxI2CDevice::new(bus_path, addr).map_err(|e| {
            format!(
                "open {bus} @ 0x{addr:02x} failed: {e}",
                bus = bus_path.display()
            )
        })?;

        // Soft reset then configure all pins as GPIO inputs, interrupts off.
        i2c.smbus_write_byte_data(REG_SOFTRESET, 0x00)
            .map_err(map_i2c)?;
        thread::sleep(Duration::from_millis(5));

        let id = i2c.smbus_read_byte_data(REG_ID).map_err(map_i2c)?;
        if id != EXPECTED_ID {
            eprintln!("warning: AW9523 ID=0x{id:02x} (expected 0x{EXPECTED_ID:02x})");
        } else {
            println!("AW9523 OK — ID 0x{id:02x} @ {}", bus_path.display());
        }

        // GPIO mode (not LED current sink): 1 = GPIO.
        i2c.smbus_write_byte_data(REG_LEDMODE0, 0xFF).map_err(map_i2c)?;
        i2c.smbus_write_byte_data(REG_LEDMODE1, 0xFF).map_err(map_i2c)?;
        // P0 push-pull (GCR bit4).
        i2c.smbus_write_byte_data(REG_GCR, 0x10).map_err(map_i2c)?;
        // All inputs (1 = input).
        i2c.smbus_write_byte_data(REG_CONFIG0, 0xFF).map_err(map_i2c)?;
        i2c.smbus_write_byte_data(REG_CONFIG1, 0xFF).map_err(map_i2c)?;
        // Disable interrupts (1 = disabled).
        i2c.smbus_write_byte_data(REG_INT0, 0xFF).map_err(map_i2c)?;
        i2c.smbus_write_byte_data(REG_INT1, 0xFF).map_err(map_i2c)?;

        Ok(Self { i2c, _rst: rst })
    }

    pub fn read_ports(&mut self) -> Result<(u8, u8), Box<dyn std::error::Error>> {
        let p0 = self.i2c.smbus_read_byte_data(REG_INPUT0).map_err(map_i2c)?;
        let p1 = self.i2c.smbus_read_byte_data(REG_INPUT1).map_err(map_i2c)?;
        Ok((p0, p1))
    }
}

fn pulse_reset() -> Result<LineHandle, Box<dyn std::error::Error>> {
    let mut chip = Chip::new("/dev/gpiochip0")?;
    let line = chip
        .get_line(PIN_RST)?
        .request(LineRequestFlags::OUTPUT, 1, "aw9523_rst")?;
    line.set_value(0)?;
    thread::sleep(Duration::from_millis(1));
    line.set_value(1)?;
    thread::sleep(Duration::from_millis(5));
    println!("AW9523 RST GPIO{PIN_RST} pulsed");
    Ok(line)
}

fn map_i2c(e: LinuxI2CError) -> Box<dyn std::error::Error> {
    format!("AW9523 I2C: {e}").into()
}

/// Debounced switch events from AW9523 port reads.
#[derive(Clone, Copy, Debug)]
pub enum SwitchEvent {
    /// Momentary: released → pressed.
    Pressed(SwitchId),
    /// Momentary: pressed → released.
    Released(SwitchId),
    /// Toggle (SW7): level after debounce. `low` = contact closed (active-low pin=0).
    Level { id: SwitchId, low: bool },
}

pub struct SwitchDebouncer {
    debounce: Duration,
    stable_pressed: [bool; 13],
    candidate_pressed: [bool; 13],
    candidate_since: [Option<Instant>; 13],
    last_reported: [bool; 13],
    primed: bool,
}

impl SwitchDebouncer {
    pub fn new(debounce_ms: u64) -> Self {
        Self {
            debounce: Duration::from_millis(debounce_ms),
            stable_pressed: [false; 13],
            candidate_pressed: [false; 13],
            candidate_since: [None; 13],
            last_reported: [false; 13],
            primed: false,
        }
    }

    /// Debounced events. Momentaries emit `Pressed` / `Released` on edges;
    /// SW7 emits `Level` on every stable high/low change (and once at startup).
    pub fn poll_events(&mut self, p0: u8, p1: u8) -> Vec<SwitchEvent> {
        let ports = [p0, p1];
        let now = Instant::now();
        let mut events = Vec::new();

        for (i, id) in SwitchId::ALL.iter().copied().enumerate() {
            let (port, bit) = id.port_bit();
            let pressed = ports[port as usize] & bit == 0;

            if pressed != self.candidate_pressed[i] {
                self.candidate_pressed[i] = pressed;
                self.candidate_since[i] = Some(now);
            }

            if let Some(since) = self.candidate_since[i] {
                if now.duration_since(since) >= self.debounce {
                    self.stable_pressed[i] = self.candidate_pressed[i];
                    self.candidate_since[i] = None;
                }
            }

            let stable = self.stable_pressed[i];
            let changed = stable != self.last_reported[i];

            if id == SwitchId::Sw7 {
                if !self.primed || changed {
                    events.push(SwitchEvent::Level {
                        id,
                        low: stable,
                    });
                }
            } else if changed {
                if stable {
                    events.push(SwitchEvent::Pressed(id));
                } else if self.primed {
                    // Skip spurious release on first poll before we know idle state.
                    events.push(SwitchEvent::Released(id));
                }
            }

            self.last_reported[i] = stable;
        }

        self.primed = true;
        events
    }
}
