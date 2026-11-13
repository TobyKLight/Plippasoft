//! Board-profile control roles → expander switches → logical [`InputEvent`]s (NB_13).
//!
//! App code only sees [`ControlRole`]. The board profile says which button
//! implements a role (or that the board has none); this module resolves that to
//! the driver-level [`SwitchId`] and back. `SwitchId` never leaks past here.

use crate::input::InputEvent;
use crate::switches::{SwitchEvent, SwitchId};
use plippa_board::{BoardProfile, ButtonRef, ControlRole};

/// Role-mapped expander switches for one board profile.
///
/// Absent optional roles are simply not in the table: pressing anything unmapped
/// is logged and ignored, never an error.
#[derive(Debug, Clone, Default)]
pub struct SwitchRoles {
    map: Vec<(ControlRole, SwitchId)>,
}

impl SwitchRoles {
    /// Build directly from resolved pairs (tests, future non-profile boards).
    pub fn from_pairs(pairs: impl IntoIterator<Item = (ControlRole, SwitchId)>) -> Self {
        Self {
            map: pairs.into_iter().collect(),
        }
    }

    /// Resolve every control role the profile defines. Required roles
    /// (menu enter / back / power) must resolve; optional roles that are absent
    /// are skipped, and an optional role wired to a non-expander pin is skipped
    /// with a warning (POC2 UI only reads the AW9523).
    pub fn from_profile(profile: &BoardProfile) -> Result<Self, String> {
        let mut map = Vec::new();
        for role in ControlRole::ALL {
            let Some(button) = profile.button_for(role) else {
                if role.is_required() {
                    return Err(format!(
                        "board has no button for required role `{}`",
                        role.name()
                    ));
                }
                continue;
            };
            match resolve_button(button) {
                Ok(id) => map.push((role, id)),
                Err(e) if role.is_required() => {
                    return Err(format!("role `{}`: {e}", role.name()));
                }
                Err(e) => eprintln!("plippa-ui: role `{}` unavailable: {e}", role.name()),
            }
        }
        Ok(Self { map })
    }

    /// Driver switch implementing `role`, if the board has it.
    pub fn switch_for(&self, role: ControlRole) -> Option<SwitchId> {
        self.map.iter().find(|(r, _)| *r == role).map(|(_, id)| *id)
    }

    /// Role implemented by driver switch `id`, if any (first match in role order).
    pub fn role_of(&self, id: SwitchId) -> Option<ControlRole> {
        self.map.iter().find(|(_, s)| *s == id).map(|(r, _)| *r)
    }

    /// Does this table have `role`?
    pub fn has(&self, role: ControlRole) -> bool {
        self.switch_for(role).is_some()
    }

    /// Human-readable role table for the startup log.
    pub fn describe(&self) -> String {
        ControlRole::ALL
            .iter()
            .map(|r| {
                let pin = self.switch_for(*r).map(|s| s.label()).unwrap_or("(none)");
                format!("{}={pin}", r.name())
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Map a debounced expander edge to zero or more logical input events.
    pub fn map_switch(&self, ev: SwitchEvent) -> Vec<InputEvent> {
        self.map_switch_at(ev, 0)
    }

    /// [`Self::map_switch`] with the edge timestamp (ms on the loop epoch) for
    /// [`InputEvent::ControlButton`].
    pub fn map_switch_at(&self, ev: SwitchEvent, at_ms: u64) -> Vec<InputEvent> {
        let (id, down) = match ev {
            SwitchEvent::Pressed(id) => (id, true),
            SwitchEvent::Released(id) => (id, false),
        };
        let Some(role) = self.role_of(id) else {
            if down {
                eprintln!("plippa-ui: unmapped switch press {}", id.label());
            }
            return Vec::new();
        };
        // WARP_06: both edges of the transport roles, with a timestamp.
        let mut edge = Vec::new();
        if matches!(
            role,
            ControlRole::Play
                | ControlRole::Rewind
                | ControlRole::FastForward
                | ControlRole::PlayMode
        ) {
            edge.push(InputEvent::ControlButton { role, down, at_ms });
        }
        let mut out = self.map_legacy(role, down);
        out.extend(edge);
        out
    }

    fn map_legacy(&self, role: ControlRole, down: bool) -> Vec<InputEvent> {
        match (role, down) {
            (ControlRole::MenuEnter, true) => vec![
                InputEvent::MenuEnterButton { down: true },
                InputEvent::MenuEnter,
            ],
            (ControlRole::MenuEnter, false) => vec![InputEvent::MenuEnterButton { down: false }],
            (ControlRole::MenuBack, true) => vec![InputEvent::Back],
            // WARP_08: Play is driven by its two timestamped edges (`ControlButton`); the
            // app's `PlayKey` decides when that toggles. `PlayStop` stays for mock / keyboard.
            (ControlRole::Play, _) => Vec::new(),
            (ControlRole::Power, d) => vec![InputEvent::PowerButton { down: d }],
            (ControlRole::AssignableAPush, d) => {
                vec![InputEvent::AssignableEncoderPush { down: d }]
            }
            // Roles without behaviour yet (NB_14 prev/next, later FF/RW/record/mode):
            // surfaced as an event so the app can bind them; press only.
            (
                r @ (ControlRole::PrevTrack
                | ControlRole::NextTrack
                | ControlRole::Rewind
                | ControlRole::FastForward
                | ControlRole::Record
                | ControlRole::PlayMode),
                true,
            ) => vec![InputEvent::ControlPressed(r)],
            _ => Vec::new(),
        }
    }
}

fn resolve_button(button: &ButtonRef) -> Result<SwitchId, String> {
    match button {
        ButtonRef::Expander { expander } => SwitchId::from_board_name(expander)
            .ok_or_else(|| format!("unknown expander pin `{expander}`")),
        ButtonRef::Gpio { gpio } => Err(format!(
            "GPIO{gpio} button not supported by the expander UI"
        )),
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use plippa_board::workspace_root_from_crate;

    pub fn poc2_profile() -> BoardProfile {
        BoardProfile::load(workspace_root_from_crate(), "plippa-D-poc2").expect("poc2 profile")
    }

    pub fn minimal_profile() -> BoardProfile {
        BoardProfile::load_file(
            workspace_root_from_crate().join("crates/plippa-board/fixtures/minimal.board.toml"),
        )
        .expect("minimal fixture")
    }

    pub fn poc2_roles() -> SwitchRoles {
        SwitchRoles::from_profile(&poc2_profile()).expect("poc2 roles")
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;

    fn press(roles: &SwitchRoles, role: ControlRole) -> Vec<InputEvent> {
        roles.map_switch(SwitchEvent::Pressed(roles.switch_for(role).unwrap()))
    }
    fn release(roles: &SwitchRoles, role: ControlRole) -> Vec<InputEvent> {
        roles.map_switch(SwitchEvent::Released(roles.switch_for(role).unwrap()))
    }

    /// Regression (plippa-D-poc2): every role maps to the same driver switch and
    /// logical events as the pre-NB_13 hard-coded table.
    #[test]
    fn poc2_role_to_switch_regression() {
        let roles = poc2_roles();
        let want = [
            (ControlRole::MenuEnter, SwitchId::Swe5),
            (ControlRole::MenuBack, SwitchId::Sw6),
            (ControlRole::Power, SwitchId::Swe1),
            (ControlRole::Play, SwitchId::BtSw3),
            (ControlRole::PrevTrack, SwitchId::BtSw1),
            (ControlRole::Rewind, SwitchId::BtSw2),
            (ControlRole::FastForward, SwitchId::BtSw4),
            (ControlRole::NextTrack, SwitchId::BtSw5),
            (ControlRole::Record, SwitchId::BtSw6),
            (ControlRole::PlayMode, SwitchId::Sw2),
            (ControlRole::AssignableAPush, SwitchId::Swe3),
        ];
        assert_eq!(want.len(), ControlRole::ALL.len());
        for (role, id) in want {
            assert_eq!(roles.switch_for(role), Some(id), "{role:?}");
            assert_eq!(roles.role_of(id), Some(role), "{role:?}");
        }
    }

    /// Regression: existing controls emit exactly the events they always did.
    #[test]
    fn poc2_existing_controls_emit_unchanged_events() {
        let roles = poc2_roles();
        assert_eq!(
            press(&roles, ControlRole::MenuEnter),
            vec![
                InputEvent::MenuEnterButton { down: true },
                InputEvent::MenuEnter
            ]
        );
        assert_eq!(
            release(&roles, ControlRole::MenuEnter),
            vec![InputEvent::MenuEnterButton { down: false }]
        );
        assert_eq!(press(&roles, ControlRole::MenuBack), vec![InputEvent::Back]);
        assert!(release(&roles, ControlRole::MenuBack).is_empty());
        // WARP_08: Play reports both edges only (no `PlayStop` from the hardware path).
        assert_eq!(
            press(&roles, ControlRole::Play),
            vec![InputEvent::ControlButton { role: ControlRole::Play, down: true, at_ms: 0 }]
        );
        assert_eq!(
            release(&roles, ControlRole::Play),
            vec![InputEvent::ControlButton { role: ControlRole::Play, down: false, at_ms: 0 }]
        );
        assert_eq!(
            press(&roles, ControlRole::Power),
            vec![InputEvent::PowerButton { down: true }]
        );
        assert_eq!(
            release(&roles, ControlRole::Power),
            vec![InputEvent::PowerButton { down: false }]
        );
        assert_eq!(
            press(&roles, ControlRole::AssignableAPush),
            vec![InputEvent::AssignableEncoderPush { down: true }]
        );
        assert_eq!(
            release(&roles, ControlRole::AssignableAPush),
            vec![InputEvent::AssignableEncoderPush { down: false }]
        );
    }

    #[test]
    fn poc2_new_roles_surface_as_control_pressed() {
        let roles = poc2_roles();
        // Prev/next/record: press only, releases produce nothing (NB_14 unchanged).
        for role in [ControlRole::PrevTrack, ControlRole::NextTrack, ControlRole::Record] {
            assert_eq!(press(&roles, role), vec![InputEvent::ControlPressed(role)]);
            assert!(release(&roles, role).is_empty());
        }
    }

    /// WARP_06: rewind / FF / play-mode report both edges with the stamped time, and
    /// still the legacy press event.
    #[test]
    fn transport_roles_report_both_edges_with_timestamps() {
        let roles = poc2_roles();
        for role in [ControlRole::Rewind, ControlRole::FastForward, ControlRole::PlayMode] {
            let sw = roles.switch_for(role).unwrap();
            assert_eq!(
                roles.map_switch_at(SwitchEvent::Pressed(sw), 1234),
                vec![
                    InputEvent::ControlPressed(role),
                    InputEvent::ControlButton { role, down: true, at_ms: 1234 }
                ]
            );
            assert_eq!(
                roles.map_switch_at(SwitchEvent::Released(sw), 1500),
                vec![InputEvent::ControlButton { role, down: false, at_ms: 1500 }]
            );
        }
        // A board without the role never emits it.
        let roles = SwitchRoles::from_pairs([(ControlRole::MenuBack, SwitchId::Sw6)]);
        assert!(roles.map_switch_at(SwitchEvent::Pressed(SwitchId::BtSw4), 5).is_empty());
    }

    #[test]
    fn unmapped_switch_is_ignored() {
        let roles = SwitchRoles::from_pairs([(ControlRole::MenuBack, SwitchId::Sw6)]);
        assert!(roles
            .map_switch(SwitchEvent::Pressed(SwitchId::BtSw1))
            .is_empty());
        assert!(roles
            .map_switch(SwitchEvent::Released(SwitchId::BtSw1))
            .is_empty());
    }

    /// Minimal board: required roles work, optional roles are absent and do nothing.
    #[test]
    fn minimal_profile_degrades_gracefully() {
        let p = minimal_profile();
        let roles = SwitchRoles::from_profile(&p).expect("minimal loads");
        for role in ControlRole::ALL {
            assert_eq!(roles.has(role), role.is_required(), "{role:?}");
            assert_eq!(roles.has(role), p.has(role));
        }
        assert_eq!(press(&roles, ControlRole::MenuBack), vec![InputEvent::Back]);
        // POC2's transport switches are simply unmapped on this board.
        for id in [
            SwitchId::BtSw1,
            SwitchId::BtSw5,
            SwitchId::Sw2,
            SwitchId::Swe3,
        ] {
            assert!(roles.map_switch(SwitchEvent::Pressed(id)).is_empty());
        }
        assert!(roles.describe().contains("prev_track=(none)"));
    }

    #[test]
    fn gpio_button_for_optional_role_is_skipped_not_fatal() {
        let text = std::fs::read_to_string(
            plippa_board::workspace_root_from_crate()
                .join("crates/plippa-board/fixtures/minimal.board.toml"),
        )
        .unwrap()
        .replace("[roles]", "[roles]\nprev_track_button = \"GPIO_PREV\"")
            + "\n[buttons.GPIO_PREV]\ngpio = 5\n";
        let p = BoardProfile::from_toml_str(&text, ".".into()).unwrap();
        assert!(p.has(ControlRole::PrevTrack));
        let roles = SwitchRoles::from_profile(&p).unwrap();
        assert!(!roles.has(ControlRole::PrevTrack));
    }
}
