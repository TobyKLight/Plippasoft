//! Spawn the poc2 I²C service (single owner of i2c-1).

use super::aw9523::{self, Aw9523};
use super::inputs::{switch_roles_from_profile, Poc2Encoders};
use super::nfc::Poc2Nfc;
use crate::env_flag;
use crate::i2c_service::{
    spawn_i2c_service, EncoderDeltas, EncoderSource, ExpanderPorts, I2cServiceHandle, NullEncoders,
};
use gpio_cdev::LineEventHandle;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use plippa_board::BoardProfile;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Aw9523Ports {
    chip: Aw9523,
    irq: Option<LineEventHandle>,
}

impl ExpanderPorts for Aw9523Ports {
    fn read_ports(&mut self) -> Result<(u8, u8), String> {
        self.chip.read_ports().map_err(|e| e.to_string())
    }

    fn wait_for_activity(&mut self, timeout: Duration) -> bool {
        let Some(handle) = self.irq.as_mut() else {
            std::thread::sleep(timeout);
            return true;
        };
        // INTN already low → change pending (cleared by next read_ports).
        if handle.get_value().ok() == Some(0) {
            return true;
        }
        let ms = timeout.as_millis().min(i32::MAX as u128) as u16;
        let mut fds = [PollFd::new(handle.as_fd(), PollFlags::POLLIN)];
        let _ = poll(&mut fds, PollTimeout::from(ms));
        let readable = fds[0]
            .revents()
            .map(|r| r.contains(PollFlags::POLLIN))
            .unwrap_or(false);
        if readable {
            // Consume one kernel event so the fd goes quiet until the next edge.
            let _ = handle.get_event();
            return true;
        }
        handle.get_value().ok() == Some(0)
    }
}

/// Expander stand-in for `PLIPPA_UI_NO_EXPANDER`: no I²C traffic, no switch input.
struct NullPorts;

impl ExpanderPorts for NullPorts {
    fn read_ports(&mut self) -> Result<(u8, u8), String> {
        Ok((0xff, 0x1f))
    }

    fn wait_for_activity(&mut self, timeout: Duration) -> bool {
        std::thread::sleep(timeout);
        true
    }
}

/// Expander selected by the `PLIPPA_UI_NO_EXPANDER` bisect switch.
enum Ports {
    Aw9523(Aw9523Ports),
    Null(NullPorts),
}

impl ExpanderPorts for Ports {
    fn read_ports(&mut self) -> Result<(u8, u8), String> {
        match self {
            Ports::Aw9523(p) => p.read_ports(),
            Ports::Null(p) => p.read_ports(),
        }
    }

    fn wait_for_activity(&mut self, timeout: Duration) -> bool {
        match self {
            Ports::Aw9523(p) => p.wait_for_activity(timeout),
            Ports::Null(p) => p.wait_for_activity(timeout),
        }
    }
}

/// Encoders selected by the `PLIPPA_UI_NO_ENCODERS` bisect switch.
enum Encoders {
    Poc2(Poc2Encoders),
    Null(NullEncoders),
}

impl EncoderSource for Encoders {
    fn poll_deltas(&mut self) -> EncoderDeltas {
        match self {
            Encoders::Poc2(e) => e.poll_deltas(),
            Encoders::Null(e) => e.poll_deltas(),
        }
    }
}

/// Open AW9523 + optional PN532 + encoders and spawn the bus-owner thread.
pub fn spawn_poc2_i2c_service(
    profile: &BoardProfile,
    stop_flag: Arc<AtomicBool>,
    epoch: Instant,
) -> Result<I2cServiceHandle, Box<dyn std::error::Error>> {
    let bus = PathBuf::from(format!("/dev/i2c-{}", profile.expander.i2c_bus));
    // Bisect switches (unset = full service). Each removes one I²C service component so the
    // screen fault can be attributed to it.
    let no_expander = env_flag("PLIPPA_UI_NO_EXPANDER");
    let no_nfc = env_flag("PLIPPA_UI_NO_NFC");
    let no_encoders = env_flag("PLIPPA_UI_NO_ENCODERS");

    let expander = if no_expander {
        eprintln!("plippa-ui: PLIPPA_UI_NO_EXPANDER=1 — AW9523 sampling disabled");
        Ports::Null(NullPorts)
    } else {
        let mut chip = Aw9523::open(
            &bus,
            profile.expander.address as u16,
            profile.expander.rst_gpio,
        )?;
        // Clear any power-on INT before arming the Pi GPIO edge watch.
        let _ = chip.read_ports();

        let irq = match profile.expander.int_gpio {
            Some(pin) => match aw9523::open_interrupt(pin) {
                Ok(h) => {
                    println!("plippa-ui: AW9523 INT on GPIO{pin}");
                    Some(h)
                }
                Err(e) => {
                    eprintln!("plippa-ui: AW9523 INT GPIO{pin} open failed ({e}) — polling only");
                    None
                }
            },
            None => {
                eprintln!("plippa-ui: AW9523 INT not configured — polling only");
                None
            }
        };
        Ports::Aw9523(Aw9523Ports { chip, irq })
    };

    let roles = switch_roles_from_profile(profile)?;
    let encoders = if no_encoders {
        eprintln!("plippa-ui: PLIPPA_UI_NO_ENCODERS=1 — encoder polling disabled");
        Encoders::Null(NullEncoders)
    } else {
        Encoders::Poc2(Poc2Encoders::open(profile)?)
    };
    let nfc = if no_nfc {
        eprintln!("plippa-ui: PLIPPA_UI_NO_NFC=1 — PN532 polling disabled");
        None
    } else {
        match Poc2Nfc::open(&profile.nfc) {
            Ok(n) => {
                println!("plippa-ui: PN532 open ok (I²C service)");
                Some(n)
            }
            Err(e) => {
                eprintln!("plippa-ui: PN532 open failed: {e}");
                None
            }
        }
    };
    Ok(spawn_i2c_service(
        expander,
        nfc,
        encoders,
        roles,
        profile.inputs.debounce_ms,
        stop_flag,
        epoch,
    ))
}
