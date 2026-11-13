//! Six-box PIN entry (UI_14 §5).
//!
//! A dedicated control, not a [`crate::keyboard::KeyboardField`]: the PIN screen always has
//! exactly six boxes, there is no append slot and no delete, and the digits are drawn as
//! digits. Masking buys nothing here — the current PIN is already printed on this screen in
//! the Network row (`483 920`), and the owner asked to be able to read what they typed.
//!
//! Interaction mirrors the keypad so it feels the same on the encoder:
//!
//! - **Navigate + Rotate:** move across the six boxes and the `OK` box, wrapping. `OK` is
//!   skipped while the PIN is incomplete, so an incomplete PIN cannot be submitted.
//! - **Navigate + Enter** on a box: enter **edit** with that box's digit (or `0`).
//! - **Edit + Rotate:** step the previewed digit `0..9`, wrapping.
//! - **Edit + Enter:** commit the box, auto-advance to the next unset box (or `OK` when it was
//!   the last one). One press per digit plus one to submit.
//! - **Edit + Back:** discard the pending digit, stay on the box.
//! - **Navigate + Back:** leave without saving.

use crate::control::InteractionMode;
use crate::network::PIN_DIGITS;

/// The possible events. There is deliberately no delete/hold event: nothing about this
/// control can remove a digit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinEvent {
    Rotate(i8),
    Enter,
    Back,
}

/// What the screen should do after handling an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinAction {
    None,
    /// `OK` pressed with all six digits set.
    Submit(String),
    /// Back from navigate: the caller pops the route.
    LeaveCancel,
    /// `OK` pressed while a box is unset (defensive; `OK` is not reachable while incomplete).
    Incomplete,
}

/// Fixed six-digit PIN entry: always six boxes, no append, no delete, digits drawn as digits.
#[derive(Debug, Clone)]
pub struct PinPad {
    digits: [Option<u8>; PIN_DIGITS],
    /// `0..=PIN_DIGITS`; `PIN_DIGITS` is the `OK` box.
    focus: usize,
    mode: InteractionMode,
    /// The digit previewed while editing a box.
    pick: u8,
}

impl Default for PinPad {
    fn default() -> Self {
        Self::new()
    }
}

impl PinPad {
    pub fn new() -> Self {
        Self {
            digits: [None; PIN_DIGITS],
            focus: 0,
            mode: InteractionMode::Navigate,
            pick: 0,
        }
    }

    /// True when all six boxes hold a digit.
    pub fn complete(&self) -> bool {
        self.digits.iter().all(Option::is_some)
    }

    /// The six digits, or `None` while any box is unset.
    pub fn text(&self) -> Option<String> {
        if !self.complete() {
            return None;
        }
        Some(
            self.digits
                .iter()
                .map(|d| char::from(b'0' + d.expect("complete checked")))
                .collect(),
        )
    }

    pub fn focus(&self) -> usize {
        self.focus
    }

    pub fn mode(&self) -> InteractionMode {
        self.mode
    }

    /// The digit in box `i`, or `None` for an empty box or the `OK` box.
    pub fn digit_at(&self, i: usize) -> Option<u8> {
        self.digits.get(i).copied().flatten()
    }

    /// The digit previewed while editing; the box shows this once committed.
    pub fn pick(&self) -> u8 {
        self.pick
    }

    /// True when the focus is on the `OK` box (never while incomplete).
    pub fn ok_focused(&self) -> bool {
        self.focus == PIN_DIGITS
    }

    pub fn handle(&mut self, ev: PinEvent) -> PinAction {
        match self.mode {
            InteractionMode::Navigate => self.handle_navigate(ev),
            InteractionMode::Edit => self.handle_edit(ev),
        }
    }

    fn handle_navigate(&mut self, ev: PinEvent) -> PinAction {
        match ev {
            PinEvent::Rotate(d) => {
                self.focus = self.step_focus(d);
                PinAction::None
            }
            PinEvent::Enter => {
                if self.focus == PIN_DIGITS {
                    return match self.text() {
                        Some(pin) => PinAction::Submit(pin),
                        None => PinAction::Incomplete,
                    };
                }
                self.pick = self.digits[self.focus].unwrap_or(0);
                self.mode = InteractionMode::Edit;
                PinAction::None
            }
            PinEvent::Back => PinAction::LeaveCancel,
        }
    }

    fn handle_edit(&mut self, ev: PinEvent) -> PinAction {
        match ev {
            PinEvent::Rotate(d) => {
                self.pick = wrap_digit(self.pick, d);
                PinAction::None
            }
            PinEvent::Enter => {
                self.digits[self.focus] = Some(self.pick);
                self.mode = InteractionMode::Navigate;
                self.focus = self.next_unset_after(self.focus);
                PinAction::None
            }
            // Discard: the box keeps whatever it held.
            PinEvent::Back => {
                self.mode = InteractionMode::Navigate;
                PinAction::None
            }
        }
    }

    /// Move the focus by `d`, wrapping. `OK` is skipped while the PIN is incomplete.
    fn step_focus(&self, d: i8) -> usize {
        let last = if self.complete() { PIN_DIGITS } else { PIN_DIGITS - 1 };
        let len = last as isize + 1;
        let mut idx = self.focus as isize + d as isize;
        // A focus left on `OK` by a completed PIN that is now being re-entered still moves.
        while idx < 0 {
            idx += len;
        }
        (idx % len) as usize
    }

    /// The next unset box after `from`, else the `OK` box.
    fn next_unset_after(&self, from: usize) -> usize {
        ((from + 1)..PIN_DIGITS)
            .find(|i| self.digits[*i].is_none())
            .unwrap_or(PIN_DIGITS)
    }
}

/// `digit + d` on the `0..=9` ring.
fn wrap_digit(digit: u8, d: i8) -> u8 {
    let mut v = digit as isize + d as isize;
    while v < 0 {
        v += 10;
    }
    (v % 10) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type one digit: enter edit on the focused box, step to `digit`, commit.
    fn type_digit(pad: &mut PinPad, digit: u8) {
        assert_eq!(pad.handle(PinEvent::Enter), PinAction::None, "edit the box");
        assert_eq!(pad.mode(), InteractionMode::Edit);
        let mut guard = 0;
        while pad.pick() != digit {
            pad.handle(PinEvent::Rotate(1));
            guard += 1;
            assert!(guard < 20, "wheel did not reach {digit}");
        }
        assert_eq!(pad.handle(PinEvent::Enter), PinAction::None, "commit");
    }

    /// If a fourth variant were ever added to [`PinEvent`] this function stops compiling:
    /// there is no delete event, by construction.
    fn all_events_are_navigation(ev: PinEvent) -> bool {
        match ev {
            PinEvent::Rotate(_) | PinEvent::Enter | PinEvent::Back => true,
        }
    }

    #[test]
    fn always_six_boxes_and_ok_skipped_until_complete() {
        let mut pad = PinPad::new();
        assert!(!pad.complete());
        assert_eq!(pad.text(), None);
        assert_eq!(pad.digit_at(5), None);

        // Six boxes, so the last reachable focus is box 5 — never the OK box.
        for _ in 0..12 {
            pad.handle(PinEvent::Rotate(1));
            assert!(!pad.ok_focused(), "OK is unreachable while incomplete");
            assert!(pad.focus() < PIN_DIGITS);
        }
        for _ in 0..12 {
            pad.handle(PinEvent::Rotate(-1));
            assert!(pad.focus() < PIN_DIGITS);
        }
        // A press on OK itself is defensive, not reachable: it reports Incomplete.
        assert_eq!(pad.handle(PinEvent::Enter), PinAction::None, "edit, not submit");
        pad.handle(PinEvent::Back);
    }

    #[test]
    fn entering_six_digits_auto_advances_and_submits() {
        let mut pad = PinPad::new();
        for digit in [0u8, 4, 8, 2, 1, 5] {
            type_digit(&mut pad, digit);
        }
        assert!(pad.complete());
        assert_eq!(pad.text().as_deref(), Some("048215"), "a leading zero survives");
        // The sixth commit lands on OK, which is now reachable.
        assert!(pad.ok_focused());
        pad.handle(PinEvent::Rotate(1));
        assert_eq!(pad.focus(), 0, "the ring wraps back to the first box");
        for _ in 0..PIN_DIGITS {
            pad.handle(PinEvent::Rotate(1));
        }
        assert!(pad.ok_focused(), "one step past the last box is OK");
        assert_eq!(pad.handle(PinEvent::Enter), PinAction::Submit("048215".into()));
    }

    #[test]
    fn a_slot_can_be_re_entered_without_shifting_its_neighbours() {
        let mut pad = PinPad::new();
        for digit in [1u8, 2, 3, 4, 5, 6] {
            type_digit(&mut pad, digit);
        }
        // Go back to box 2 (index 1) and change it to 9.
        pad.handle(PinEvent::Rotate(-1));
        while pad.focus() != 1 {
            pad.handle(PinEvent::Rotate(-1));
        }
        type_digit(&mut pad, 9);
        assert_eq!(pad.text().as_deref(), Some("193456"), "six long, no shift");
        assert_eq!(pad.digit_at(0), Some(1));
        assert_eq!(pad.digit_at(2), Some(3));
    }

    #[test]
    fn hold_delete_is_inert() {
        // No delete path exists: the event enum has exactly these three variants...
        assert!(all_events_are_navigation(PinEvent::Rotate(1)));
        assert!(all_events_are_navigation(PinEvent::Enter));
        assert!(all_events_are_navigation(PinEvent::Back));
        // ...and back from navigate leaves the digits alone for the caller to drop.
        let mut pad = PinPad::new();
        type_digit(&mut pad, 7);
        assert_eq!(pad.handle(PinEvent::Back), PinAction::LeaveCancel);
        assert_eq!(pad.digit_at(0), Some(7));
        assert_eq!(pad.text(), None, "five boxes are still empty");
    }

    #[test]
    fn back_in_edit_discards_only_the_pending_pick() {
        let mut pad = PinPad::new();
        type_digit(&mut pad, 3);
        // Enter box 1, wheel away, leave without committing.
        pad.handle(PinEvent::Enter);
        pad.handle(PinEvent::Rotate(4));
        assert_eq!(pad.handle(PinEvent::Back), PinAction::None);
        assert_eq!(pad.mode(), InteractionMode::Navigate);
        assert_eq!(pad.focus(), 1, "back stays on the box, it does not move");
        assert_eq!(pad.digit_at(1), None, "the previewed digit was never committed");
        assert_eq!(pad.digit_at(0), Some(3), "the committed box is untouched");
    }
}
