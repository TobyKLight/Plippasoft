//! PN532 I2C backend for plippa-D-poc2 (feature `pn532`).
//!
//! Wiring: `/dev/i2c-1` @ 0x24 — see `eval/poc2/nfc_test_PN532/pinout.md`.

mod driver;
mod rpi_i2c;

pub use driver::{Pn532Nfc, Pn532NfcConfig};
