//! Role-mapped encoder + switch helpers for plippa-D-poc2.
//!
//! Expander I²C lives in the I²C service (`i2c_service`); this module opens
//! GPIO encoders and resolves board-profile roles. Pin numbers come only from
//! the profile — never hard-coded BCM in `UiApp`.

use super::encoder::{DetentFilter, Encoder};
use crate::i2c_service::{EncoderDeltas, EncoderSource, SwitchRoles};
use plippa_board::{BoardProfile, EncoderRole};

const GPIO_CHIP: &str = "/dev/gpiochip0";

/// POC2 mechanical encoders: 2 quadrature edges per detent click.
const EDGES_PER_DETENT: u32 = 2;

/// Menu + volume + optional Assignable A encoders (GPIO, polled on I²C service thread).
pub struct Poc2Encoders {
    menu: Encoder,
    volume: Encoder,
    assignable_a: Option<Encoder>,
    menu_detent: DetentFilter,
    volume_detent: DetentFilter,
    assignable_a_detent: DetentFilter,
}

impl Poc2Encoders {
    pub fn open(profile: &BoardProfile) -> Result<Self, Box<dyn std::error::Error>> {
        let (menu_name, menu_pins) = required_encoder(profile, EncoderRole::Menu)?;
        let (vol_name, vol_pins) = required_encoder(profile, EncoderRole::Volume)?;

        let menu = Encoder::open(GPIO_CHIP, menu_pins.a_gpio, menu_pins.b_gpio, menu_name)?;
        let volume = Encoder::open(GPIO_CHIP, vol_pins.a_gpio, vol_pins.b_gpio, vol_name)?;

        // Optional: boards without an assignable encoder simply report no deltas.
        let assignable_a = match profile.encoder_name_for(EncoderRole::AssignableA) {
            Some(name) => {
                let pins = profile
                    .encoder_for(EncoderRole::AssignableA)
                    .ok_or_else(|| format!("missing encoder pins for `{name}`"))?;
                Some(Encoder::open(GPIO_CHIP, pins.a_gpio, pins.b_gpio, name)?)
            }
            None => None,
        };

        Ok(Self {
            menu,
            volume,
            assignable_a,
            menu_detent: DetentFilter::new(EDGES_PER_DETENT),
            volume_detent: DetentFilter::new(EDGES_PER_DETENT),
            assignable_a_detent: DetentFilter::new(EDGES_PER_DETENT),
        })
    }
}

fn required_encoder(
    profile: &BoardProfile,
    role: EncoderRole,
) -> Result<(&str, &plippa_board::EncoderPins), Box<dyn std::error::Error>> {
    let name = profile
        .encoder_name_for(role)
        .ok_or_else(|| format!("board has no {} encoder", role.name()))?;
    let pins = profile
        .encoder_for(role)
        .ok_or_else(|| format!("missing encoder pins for `{name}`"))?;
    Ok((name, pins))
}

impl EncoderSource for Poc2Encoders {
    fn poll_deltas(&mut self) -> EncoderDeltas {
        let menu = self.menu_detent.push(self.menu.poll());
        let volume = self.volume_detent.push(self.volume.poll());
        let assignable_a = match self.assignable_a.as_mut() {
            Some(e) => self.assignable_a_detent.push(e.poll()),
            None => 0,
        };
        EncoderDeltas {
            menu,
            volume,
            assignable_a,
        }
    }
}

/// Resolve AW9523 role mapping from the board profile (roles → switches).
pub fn switch_roles_from_profile(
    profile: &BoardProfile,
) -> Result<SwitchRoles, Box<dyn std::error::Error>> {
    let roles = SwitchRoles::from_profile(profile)?;
    eprintln!("plippa-ui: inputs {}", roles.describe());
    eprintln!("plippa-ui: board {}", profile.optional_roles_summary());
    Ok(roles)
}
