//! Reusable keyboard field control (Wi‑Fi password / SSID-safe charset).
//!
//! # Interaction (UI_05)
//!
//! - **Navigate mode:** menu encoder moves between character slots (including a
//!   trailing blank for append). Enter opens **edit** on the focused slot.
//! - **Edit mode:** menu encoder walks the charset; Enter/Back leave edit and
//!   commit the glyph. Committing on the trailing blank appends a character and
//!   keeps a new blank at the end.
//! - Strings **must not start with a blank** — the blank exists only as the
//!   trailing append cursor, never as leading content.
//! - **Hold Menu Enter** deletes (backspace over slots).
//! - **Encoder A rotate** (while the field is active) jumps charset regions:
//!   upper → lower → digits → symbols (and reverse). Region jumps apply to the
//!   focused slot’s charset index (edit or navigate).
//!
//! Leaving the field (Back in navigate, or [`KeyboardAction::Submit`]) is the
//! caller’s job — typically Wi‑Fi restores Encoder A via [`crate::AssignStore`].

use crate::control::InteractionMode;

/// Full Wi‑Fi SSID/password-safe charset (printable ASCII minus space leading issues).
/// Space is allowed mid-string; regions are used for Encoder A jumps.
pub const CHARSET: &[char] = &[
    // Upper
    'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q',
    'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
    // Lower
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q',
    'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z',
    // Digits
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
    // Symbols (common Wi‑Fi PSK / SSID)
    ' ', '!', '"', '#', '$', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/',
    ':', ';', '<', '=', '>', '?', '@', '[', '\\', ']', '^', '_', '`', '{', '|', '}', '~',
];

const REGION_UPPER: usize = 0;
const REGION_LOWER: usize = 26;
const REGION_DIGIT: usize = 52;
const REGION_SYMBOL: usize = 62;

/// Charset region starts for Encoder A jumps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharsetRegion {
    Upper,
    Lower,
    Digit,
    Symbol,
}

impl CharsetRegion {
    pub fn start_index(self) -> usize {
        match self {
            Self::Upper => REGION_UPPER,
            Self::Lower => REGION_LOWER,
            Self::Digit => REGION_DIGIT,
            Self::Symbol => REGION_SYMBOL,
        }
    }

    pub fn from_index(idx: usize) -> Self {
        let i = idx % CHARSET.len();
        if i < REGION_LOWER {
            Self::Upper
        } else if i < REGION_DIGIT {
            Self::Lower
        } else if i < REGION_SYMBOL {
            Self::Digit
        } else {
            Self::Symbol
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Upper => Self::Lower,
            Self::Lower => Self::Digit,
            Self::Digit => Self::Symbol,
            Self::Symbol => Self::Upper,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Upper => Self::Symbol,
            Self::Lower => Self::Upper,
            Self::Digit => Self::Lower,
            Self::Symbol => Self::Digit,
        }
    }
}

/// Events consumed by [`KeyboardField`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyboardEvent {
    Rotate(i8),
    Enter,
    Back,
    HoldDelete,
    /// Encoder A region jump (positive = next region, negative = previous).
    RegionJump(i8),
}

/// Side-effect returned to the host screen after handling an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyboardAction {
    None,
    /// Back from navigate — leave the field without submitting.
    LeaveCancel,
    /// Enter on the trailing **OK** slot (navigate mode).
    Submit(String),
}

/// One logical slot: char, trailing blank (append), or OK (submit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Char(char),
    Blank,
    Ok,
}

/// Reusable multi-slot keyboard field.
#[derive(Debug, Clone)]
pub struct KeyboardField {
    /// Committed characters (never starts with a conceptual blank).
    chars: Vec<char>,
    /// Focus index in `0..=chars.len()+1` — blank at `chars.len()`, OK after.
    focus: usize,
    /// Charset index for the focused slot while editing (and last used).
    charset_index: usize,
    mode: InteractionMode,
    /// Restrict the charset to the digit region (accessory passkey entry).
    digits_only: bool,
}

impl Default for KeyboardField {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyboardField {
    pub fn new() -> Self {
        Self {
            chars: Vec::new(),
            focus: 0,
            charset_index: REGION_LOWER, // start on 'a'
            mode: InteractionMode::Navigate,
            digits_only: false,
        }
    }

    /// A field that can only produce ASCII digits (pairing passkey).
    pub fn digits_only() -> Self {
        Self {
            charset_index: REGION_DIGIT,
            digits_only: true,
            ..Self::new()
        }
    }

    pub fn from_str(s: &str) -> Self {
        let mut k = Self::new();
        k.chars = s.chars().collect();
        k.focus = k.chars.len(); // blank after content
        k
    }

    pub fn mode(&self) -> InteractionMode {
        self.mode
    }

    pub fn focus(&self) -> usize {
        self.focus
    }

    pub fn charset_index(&self) -> usize {
        self.charset_index % CHARSET.len()
    }

    pub fn current_char(&self) -> char {
        CHARSET[self.charset_index()]
    }

    pub fn region(&self) -> CharsetRegion {
        CharsetRegion::from_index(self.charset_index())
    }

    /// Slot count: chars + trailing blank + OK.
    pub fn slot_count(&self) -> usize {
        self.chars.len() + 2
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_blank_focused(&self) -> bool {
        self.focus == self.chars.len()
    }

    pub fn is_ok_focused(&self) -> bool {
        self.focus == self.chars.len() + 1
    }

    fn slot_kind(&self, idx: usize) -> SlotKind {
        if idx < self.chars.len() {
            SlotKind::Char(self.chars[idx])
        } else if idx == self.chars.len() {
            SlotKind::Blank
        } else {
            SlotKind::Ok
        }
    }

    /// Display glyph for slot `idx` (blank shows pending charset pick when focused+editing).
    pub fn slot_display(&self, idx: usize) -> char {
        match self.slot_kind(idx) {
            SlotKind::Char(c) => c,
            SlotKind::Blank => {
                if idx == self.focus && self.mode == InteractionMode::Edit {
                    self.current_char()
                } else {
                    '_'
                }
            }
            SlotKind::Ok => '✓',
        }
    }

    /// Label for paint (OK slot is three chars).
    pub fn slot_label(&self, idx: usize) -> String {
        match self.slot_kind(idx) {
            SlotKind::Ok => {
                if idx == self.focus {
                    "[OK]".into()
                } else {
                    " OK ".into()
                }
            }
            _ => self.slot_display(idx).to_string(),
        }
    }

    pub fn handle(&mut self, ev: KeyboardEvent) -> KeyboardAction {
        match ev {
            KeyboardEvent::RegionJump(d) => {
                self.jump_region(d);
                KeyboardAction::None
            }
            KeyboardEvent::HoldDelete => {
                self.delete();
                KeyboardAction::None
            }
            KeyboardEvent::Rotate(d) => {
                match self.mode {
                    InteractionMode::Navigate => self.move_focus(d),
                    InteractionMode::Edit => self.rotate_charset(d),
                }
                KeyboardAction::None
            }
            KeyboardEvent::Enter => self.on_enter(),
            KeyboardEvent::Back => self.on_back(),
        }
    }

    fn on_enter(&mut self) -> KeyboardAction {
        match self.mode {
            InteractionMode::Navigate => {
                if self.is_ok_focused() {
                    return KeyboardAction::Submit(self.text());
                }
                self.mode = InteractionMode::Edit;
                // Sync charset index to existing glyph when editing a char slot.
                if let SlotKind::Char(c) = self.slot_kind(self.focus) {
                    if let Some(i) = CHARSET.iter().position(|&x| x == c) {
                        if !self.digits_only || c.is_ascii_digit() {
                            self.charset_index = i;
                        }
                    }
                }
                KeyboardAction::None
            }
            InteractionMode::Edit => {
                self.commit_edit();
                KeyboardAction::None
            }
        }
    }

    fn on_back(&mut self) -> KeyboardAction {
        match self.mode {
            InteractionMode::Edit => {
                // Discard pending blank pick; keep existing chars.
                self.mode = InteractionMode::Navigate;
                KeyboardAction::None
            }
            InteractionMode::Navigate => KeyboardAction::LeaveCancel,
        }
    }

    fn commit_edit(&mut self) {
        let c = self.current_char();
        if self.digits_only && !c.is_ascii_digit() {
            return;
        }
        if self.is_blank_focused() {
            // Append — never allow a "leading blank" as content; blank is only cursor.
            self.chars.push(c);
            self.focus = self.chars.len(); // new trailing blank
        } else {
            self.chars[self.focus] = c;
        }
        self.mode = InteractionMode::Navigate;
    }

    fn move_focus(&mut self, delta: i8) {
        let len = self.slot_count().max(1) as isize;
        let mut idx = self.focus as isize + delta as isize;
        while idx < 0 {
            idx += len;
        }
        self.focus = (idx as usize) % len as usize;
    }

    fn rotate_charset(&mut self, delta: i8) {
        if self.digits_only {
            let len = 10isize;
            let mut idx = (self.charset_index.saturating_sub(REGION_DIGIT) as isize + delta as isize)
                % len;
            if idx < 0 {
                idx += len;
            }
            self.charset_index = REGION_DIGIT + idx as usize;
            return;
        }
        let len = CHARSET.len() as isize;
        let mut idx = self.charset_index as isize + delta as isize;
        while idx < 0 {
            idx += len;
        }
        self.charset_index = (idx as usize) % CHARSET.len();
    }

    /// Jump to the start of the next/previous charset region.
    pub fn jump_region(&mut self, delta: i8) {
        if delta == 0 {
            return;
        }
        let mut region = self.region();
        let steps = delta.unsigned_abs() as usize;
        for _ in 0..steps {
            region = if delta > 0 {
                region.next()
            } else {
                region.prev()
            };
        }
        self.charset_index = region.start_index();
        // If editing a committed slot, preview updates via charset_index; commit on Enter.
        // If navigating, region jump still updates the pick used when entering edit.
    }

    /// Delete at focus: blank/OK focus pops last char; char focus removes that slot.
    /// Never leaves a leading blank (blank is not stored in `chars`).
    pub fn delete(&mut self) {
        self.mode = InteractionMode::Navigate;
        if self.chars.is_empty() {
            self.focus = 0;
            return;
        }
        if self.is_blank_focused() || self.is_ok_focused() {
            self.chars.pop();
            self.focus = self.chars.len();
        } else {
            self.chars.remove(self.focus);
            if self.focus > self.chars.len() {
                self.focus = self.chars.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_blank_and_reject_leading_blank() {
        let mut k = KeyboardField::new();
        assert_eq!(k.slot_count(), 2); // blank + OK
        assert!(k.is_blank_focused());
        // Enter edit on blank, pick 'a', commit → one char + new blank.
        assert_eq!(k.handle(KeyboardEvent::Enter), KeyboardAction::None);
        assert_eq!(k.mode(), InteractionMode::Edit);
        assert_eq!(k.current_char(), 'a');
        assert_eq!(k.handle(KeyboardEvent::Enter), KeyboardAction::None);
        assert_eq!(k.text(), "a");
        assert!(k.is_blank_focused());
        // Content never starts with blank — chars[0] is always a real glyph.
        assert_eq!(k.chars[0], 'a');
        // Can still append after first char (Enter on blank enters edit, not submit).
        assert_eq!(k.handle(KeyboardEvent::Enter), KeyboardAction::None);
        assert_eq!(k.mode(), InteractionMode::Edit);
    }

    #[test]
    fn hold_delete_backspaces() {
        let mut k = KeyboardField::from_str("ab");
        assert_eq!(k.text(), "ab");
        assert!(k.is_blank_focused());
        assert_eq!(k.handle(KeyboardEvent::HoldDelete), KeyboardAction::None);
        assert_eq!(k.text(), "a");
        k.focus = 0;
        assert_eq!(k.handle(KeyboardEvent::HoldDelete), KeyboardAction::None);
        assert_eq!(k.text(), "");
        assert!(k.is_blank_focused());
    }

    #[test]
    fn region_jump_via_a() {
        let mut k = KeyboardField::new();
        k.charset_index = REGION_LOWER;
        assert_eq!(k.region(), CharsetRegion::Lower);
        k.handle(KeyboardEvent::RegionJump(1));
        assert_eq!(k.region(), CharsetRegion::Digit);
        assert_eq!(k.current_char(), '0');
        k.handle(KeyboardEvent::RegionJump(1));
        assert_eq!(k.region(), CharsetRegion::Symbol);
        k.handle(KeyboardEvent::RegionJump(1));
        assert_eq!(k.region(), CharsetRegion::Upper);
        assert_eq!(k.current_char(), 'A');
        k.handle(KeyboardEvent::RegionJump(-1));
        assert_eq!(k.region(), CharsetRegion::Symbol);
    }

    #[test]
    fn submit_on_ok_slot() {
        let mut k = KeyboardField::from_str("secret");
        assert!(k.is_blank_focused());
        k.handle(KeyboardEvent::Rotate(1)); // blank → OK
        assert!(k.is_ok_focused());
        assert_eq!(
            k.handle(KeyboardEvent::Enter),
            KeyboardAction::Submit("secret".into())
        );
    }

    #[test]
    fn back_cancels_from_navigate() {
        let mut k = KeyboardField::from_str("x");
        assert_eq!(k.handle(KeyboardEvent::Back), KeyboardAction::LeaveCancel);
    }

    #[test]
    fn digits_only_never_produces_a_letter() {
        let mut k = KeyboardField::digits_only();
        assert_eq!(k.handle(KeyboardEvent::Enter), KeyboardAction::None);
        for _ in 0..64 {
            for delta in [1i8, -1] {
                k.handle(KeyboardEvent::Rotate(delta));
                assert!(
                    k.current_char().is_ascii_digit(),
                    "got {:?}",
                    k.current_char()
                );
            }
        }
        k.handle(KeyboardEvent::Enter);
        assert!(k.text().chars().all(|c| c.is_ascii_digit()), "{}", k.text());
    }
}
