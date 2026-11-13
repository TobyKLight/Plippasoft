//! Quadrature rotary encoder (gpio-cdev), adapted from eval/poc2/ui_test.

use gpio_cdev::{Chip, Error, LineHandle, LineRequestFlags};
use std::process::Command;

const LUT: [i8; 16] = [0, -1, 1, 0, 1, 0, 0, -1, -1, 0, 0, 1, 0, 1, -1, 0];

pub struct Encoder {
    a: LineHandle,
    b: LineHandle,
    last_state: u8,
    pub name: String,
}

impl Encoder {
    pub fn open(
        chip_path: &str,
        pin_a: u32,
        pin_b: u32,
        name: impl Into<String>,
    ) -> Result<Self, Error> {
        let name = name.into();
        let mut chip = Chip::new(chip_path)?;
        let a = chip
            .get_line(pin_a)?
            .request(LineRequestFlags::INPUT, 0, &format!("{name}_a"))?;
        let b = chip
            .get_line(pin_b)?
            .request(LineRequestFlags::INPUT, 0, &format!("{name}_b"))?;
        enable_pull_up(pin_a);
        enable_pull_up(pin_b);
        let last_state = read_state(&a, &b);
        Ok(Self {
            a,
            b,
            last_state,
            name,
        })
    }

    /// Returns +1 / -1 per quadrature edge (transition), or 0 if no movement.
    ///
    /// Mechanical detents typically need two edges; use [`DetentFilter`] to
    /// accumulate edges into detent steps.
    pub fn poll(&mut self) -> i8 {
        let state = read_state(&self.a, &self.b);
        let idx = ((self.last_state << 2) | state) as usize;
        self.last_state = state;
        LUT[idx]
    }
}

/// Accumulate quadrature edges until a full mechanical detent is reached.
///
/// POC2 encoders typically produce 2 edges per click; some parts use 4.
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
    pub fn push(&mut self, edge: i8) -> i8 {
        if edge == 0 {
            return 0;
        }
        if self.edges_per_detent <= 1 {
            return edge.signum();
        }
        self.accum += i32::from(edge.signum());
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

fn enable_pull_up(pin: u32) {
    let _ = Command::new("pinctrl")
        .args(["set", &pin.to_string(), "pu"])
        .status();
}

#[cfg(test)]
mod tests {
    use super::DetentFilter;

    #[test]
    fn two_edges_yield_one_detent_step() {
        let mut f = DetentFilter::new(2);
        assert_eq!(f.push(1), 0);
        assert_eq!(f.push(1), 1);
    }

    #[test]
    fn single_edge_yields_zero() {
        let mut f = DetentFilter::new(2);
        assert_eq!(f.push(1), 0);
        assert_eq!(f.push(-1), 0); // cancels partial accum
    }

    #[test]
    fn two_negative_edges_yield_minus_one() {
        let mut f = DetentFilter::new(2);
        assert_eq!(f.push(-1), 0);
        assert_eq!(f.push(-1), -1);
    }

    #[test]
    fn edges_per_detent_one_passes_through() {
        let mut f = DetentFilter::new(1);
        assert_eq!(f.push(1), 1);
        assert_eq!(f.push(-1), -1);
        assert_eq!(f.push(0), 0);
    }
}
