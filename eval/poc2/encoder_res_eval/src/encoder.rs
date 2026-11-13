//! Quadrature rotary encoders on RPi GPIO (active-high with pull-ups).

use gpio_cdev::{Chip, Error, LineHandle, LineRequestFlags};
use std::process::Command;

const LUT: [i8; 16] = [0, -1, 1, 0, 1, 0, 0, -1, -1, 0, 0, 1, 0, 1, -1, 0];

pub const GPIO_CHIP: &str = "/dev/gpiochip0";

/// SWE1 A/B, SWE3 A/B, SWE4 A/B, SWE5 A/B — from wiring CSV.
pub const ENC_PINS: [(u32, u32, &str); 4] = [
    (17, 7, "SWE1"),  // A=GPIO17, B=GPIO7
    (14, 5, "SWE3"),  // A=GPIO14, B=GPIO5
    (6, 27, "SWE4"),  // A=GPIO6, B=GPIO27
    (9, 26, "SWE5"),  // A=GPIO9, B=GPIO26
];

pub struct Encoder {
    a: LineHandle,
    b: LineHandle,
    last_state: u8,
    pub name: &'static str,
}

impl Encoder {
    pub fn open(chip_path: &str, pin_a: u32, pin_b: u32, name: &'static str) -> Result<Self, Error> {
        let mut chip = Chip::new(chip_path)?;
        let a = chip
            .get_line(pin_a)?
            .request(LineRequestFlags::INPUT, 0, &format!("{name}_a"))?;
        let b = chip
            .get_line(pin_b)?
            .request(LineRequestFlags::INPUT, 0, &format!("{name}_b"))?;
        let last_state = read_state(&a, &b);
        Ok(Self {
            a,
            b,
            last_state,
            name,
        })
    }

    /// Returns +1 / -1 per quadrature transition, or 0 if no movement.
    pub fn poll(&mut self) -> i32 {
        let state = read_state(&self.a, &self.b);
        let idx = ((self.last_state << 2) | state) as usize;
        self.last_state = state;
        i32::from(LUT[idx])
    }
}

/// Accumulate quadrature edges until a full mechanical detent is reached.
///
/// These POC2 encoders typically produce 2 edges per click; some parts use 4.
/// `edges_per_detent == 1` passes raw edges through (no filter).
pub struct DetentFilter {
    edges_per_detent: u32,
    accum: i32,
}

impl DetentFilter {
    pub fn new(edges_per_detent: u32) -> Self {
        Self {
            edges_per_detent: edges_per_detent.max(1),
            accum: 0,
        }
    }

    /// Feed one quadrature edge (±1). Returns ±1 when a detent completes, else 0.
    pub fn push(&mut self, edge: i32) -> i32 {
        if edge == 0 {
            return 0;
        }
        if self.edges_per_detent <= 1 {
            return edge.signum();
        }
        self.accum += edge.signum();
        let thr = self.edges_per_detent as i32;
        if self.accum >= thr {
            self.accum -= thr;
            1
        } else if self.accum <= -thr {
            self.accum += thr;
            -1
        } else {
            0
        }
    }
}

fn read_state(a: &LineHandle, b: &LineHandle) -> u8 {
    let a_bit = u8::from(a.get_value().unwrap_or(0) != 0);
    let b_bit = u8::from(b.get_value().unwrap_or(0) != 0);
    (a_bit << 1) | b_bit
}

/// Enable internal pull-ups on all encoder A/B pins.
pub fn configure_encoder_pull_ups() {
    for (pin_a, pin_b, name) in ENC_PINS {
        for (pin, side) in [(pin_a, "A"), (pin_b, "B")] {
            match Command::new("pinctrl")
                .args(["set", &pin.to_string(), "pu"])
                .status()
            {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    eprintln!("GPIO {pin} ({name} {side}): pinctrl failed ({status})");
                }
                Err(e) => {
                    eprintln!(
                        "GPIO {pin} ({name} {side}): could not run pinctrl ({e}) — try: pinctrl set {pin} pu"
                    );
                }
            }
        }
    }
}

pub fn open_all() -> Result<[Encoder; 4], Error> {
    configure_encoder_pull_ups();
    Ok([
        Encoder::open(GPIO_CHIP, ENC_PINS[0].0, ENC_PINS[0].1, ENC_PINS[0].2)?,
        Encoder::open(GPIO_CHIP, ENC_PINS[1].0, ENC_PINS[1].1, ENC_PINS[1].2)?,
        Encoder::open(GPIO_CHIP, ENC_PINS[2].0, ENC_PINS[2].1, ENC_PINS[2].2)?,
        Encoder::open(GPIO_CHIP, ENC_PINS[3].0, ENC_PINS[3].1, ENC_PINS[3].2)?,
    ])
}
