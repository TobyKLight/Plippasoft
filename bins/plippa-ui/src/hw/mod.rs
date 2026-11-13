//! Feature-gated hardware backends for plippa-D-poc2.
//!
//! Enabled with `--features poc2`. Compiles without devices present; open()
//! fails at runtime if SPI/I2C/GPIO are missing.

#[cfg(feature = "poc2")]
mod aw9523;
#[cfg(feature = "poc2")]
mod encoder;
#[cfg(feature = "poc2")]
mod i2c_host;
#[cfg(feature = "poc2")]
mod ili9341;
#[cfg(feature = "poc2")]
mod inputs;
#[cfg(feature = "poc2")]
mod leds;
#[cfg(feature = "poc2")]
mod nfc;

#[cfg(feature = "poc2")]
pub use aw9523::{Aw9523, SwitchDebouncer, SwitchEvent, SwitchId};
#[cfg(feature = "poc2")]
pub use encoder::Encoder;
#[cfg(feature = "poc2")]
pub use i2c_host::spawn_poc2_i2c_service;
#[cfg(feature = "poc2")]
pub use ili9341::Ili9341Display;
#[cfg(feature = "poc2")]
pub use inputs::{switch_roles_from_profile, Poc2Encoders};
#[cfg(feature = "poc2")]
pub use leds::Poc2LedCue;
#[cfg(feature = "poc2")]
pub use nfc::Poc2Nfc;

/// Compile-time marker so CI can `cargo check -p plippa-ui --features poc2`.
#[cfg(feature = "poc2")]
pub const POC2_ENABLED: bool = true;

#[cfg(not(feature = "poc2"))]
pub const POC2_ENABLED: bool = false;
