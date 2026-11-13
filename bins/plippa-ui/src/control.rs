//! Navigate vs edit control graph.
//!
//! Screens own how focus/selection is drawn; this module tracks mode and values.
//! Enter on a focusable item enters **edit**; Back leaves edit back to **navigate**.
//! In navigate mode the menu encoder moves focus; in edit it changes the value.

/// Whether the user is moving focus or adjusting a control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionMode {
    Navigate,
    Edit,
}

/// One focusable parameter (screens decide labels / painting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusableItem {
    pub id: &'static str,
    pub value: i32,
    pub min: i32,
    pub max: i32,
}

/// Focus list + navigate/edit mode for a screen's controls.
#[derive(Debug, Clone)]
pub struct ControlGraph {
    mode: InteractionMode,
    focus: usize,
    items: Vec<FocusableItem>,
}

impl Default for ControlGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlGraph {
    pub fn new() -> Self {
        Self {
            mode: InteractionMode::Navigate,
            focus: 0,
            items: Vec::new(),
        }
    }

    /// Dummy graph used by unit tests (two adjustable values).
    pub fn dummy() -> Self {
        Self {
            mode: InteractionMode::Navigate,
            focus: 0,
            items: vec![
                FocusableItem {
                    id: "dummy_a",
                    value: 0,
                    min: 0,
                    max: 10,
                },
                FocusableItem {
                    id: "dummy_b",
                    value: 5,
                    min: 0,
                    max: 10,
                },
            ],
        }
    }

    pub fn mode(&self) -> InteractionMode {
        self.mode
    }

    pub fn focus(&self) -> usize {
        self.focus
    }

    pub fn items(&self) -> &[FocusableItem] {
        &self.items
    }

    pub fn focused(&self) -> Option<&FocusableItem> {
        self.items.get(self.focus)
    }

    pub fn focused_value(&self) -> Option<i32> {
        self.focused().map(|i| i.value)
    }

    /// Nudge a control by stable id (Assignable Encoder A path). Returns false if unknown.
    pub fn nudge_id(&mut self, id: &str, delta: i8) -> bool {
        let Some(item) = self.items.iter_mut().find(|i| i.id == id) else {
            return false;
        };
        let next = (item.value as i16 + delta as i16).clamp(item.min as i16, item.max as i16);
        item.value = next as i32;
        true
    }

    /// Value of a named item, if present.
    pub fn value_of(&self, id: &str) -> Option<i32> {
        self.items.iter().find(|i| i.id == id).map(|i| i.value)
    }

    /// Enter the focused control (navigate → edit). No-op if already editing or empty.
    pub fn enter(&mut self) {
        if self.items.is_empty() {
            return;
        }
        if self.mode == InteractionMode::Navigate {
            self.mode = InteractionMode::Edit;
        }
    }

    /// Leave edit mode. Returns `true` if Back was consumed by leaving edit.
    pub fn back(&mut self) -> bool {
        if self.mode == InteractionMode::Edit {
            self.mode = InteractionMode::Navigate;
            true
        } else {
            false
        }
    }

    /// Navigate: move focus. Edit: nudge the focused value by `delta`.
    pub fn rotate(&mut self, delta: i8) {
        if self.items.is_empty() {
            return;
        }
        match self.mode {
            InteractionMode::Navigate => {
                let len = self.items.len() as isize;
                let mut idx = self.focus as isize + delta as isize;
                while idx < 0 {
                    idx += len;
                }
                idx %= len;
                self.focus = idx as usize;
            }
            InteractionMode::Edit => {
                if let Some(item) = self.items.get_mut(self.focus) {
                    let next = (item.value as i16 + delta as i16)
                        .clamp(item.min as i16, item.max as i16);
                    item.value = next as i32;
                }
            }
        }
    }
}

/// A parameter the Now Playing knob can adjust after Enter (WARP_07).
///
/// # Extension point (future BPM and others)
///
/// To add a parameter: append an enum variant here, append it to [`PARAM_RING`], add one
/// readout arm in `ScreenModel::render_main` (`screens.rs`, `adjust_readout`) and one knob
/// handler arm in `UiApp::adjust_rotate` (`app.rs`). No other state-machine change is
/// needed: [`AdjustParam::next`] walks the ring and the Enter / Back rules are generic. The
/// test `param_ring_cycles_every_entry` iterates [`PARAM_RING`] so an added entry is cycled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustParam {
    /// Rotate scrubs the position (WARP_15 upgrades the feel).
    Jog,
    /// Rotate sets the permanent playback speed 10%-200%.
    Tempo,
    /// Rotate changes the waveform time window (UI_13, session-only).
    Zoom,
}

/// The Enter ring, in cycling order. The first entry is the implicit default on a
/// single-track screen (rotate = jog before any Enter).
pub const PARAM_RING: &[AdjustParam] = &[AdjustParam::Jog, AdjustParam::Tempo, AdjustParam::Zoom];

impl AdjustParam {
    /// The next ring entry (wraps).
    pub fn next(self) -> AdjustParam {
        let i = PARAM_RING.iter().position(|p| *p == self).unwrap_or(0);
        PARAM_RING[(i + 1) % PARAM_RING.len()]
    }
}

/// The two things the menu knob can select on a collection's Now Playing screen (NB_15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpTarget {
    /// Time / seek bar: push enters jog (scrub) mode.
    CurrentTime,
    /// The `nowPlayingList` panel: push enters list mode.
    List,
}

/// Interaction level on the Now Playing screen while a list panel is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpMode {
    /// Rotate moves between [`NpTarget`]s.
    Select,
    /// A ring parameter is being adjusted (rotate edits it, Enter cycles [`PARAM_RING`],
    /// Back returns to `Select`). `Adjust(AdjustParam::Jog)` is the old jog mode.
    Adjust(AdjustParam),
    /// Rotate moves the list cursor, push jumps to the cursor row (back exits).
    List,
}

/// What a menu push did on the Now Playing screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpPush {
    /// Entered or cycled to this ring parameter.
    Adjust(AdjustParam),
    /// Jump to this flat `nowPlayingList` row (stay in list mode; the caller closes the
    /// overlay, UI_13 §0 decision 4).
    Jump(usize),
}

/// Now Playing focus state machine (pure; the screen model owns the list, the app the I/O).
///
/// Levels: `Select` (target = time | list) -> `Adjust(param)` / `List`. Back unwinds one level.
/// On a single-track screen (no list panel) `Select` is the implicit-jog level and
/// [`Self::push_single`] cycles the ring from there.
/// `cursor` is a flat `nowPlayingList` row and only meaningful in [`NpMode::List`]; while
/// browsing, the list window follows the cursor, otherwise it follows the playing row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NowPlayingFocus {
    pub target: NpTarget,
    pub mode: NpMode,
    pub cursor: usize,
}

impl Default for NowPlayingFocus {
    fn default() -> Self {
        Self {
            target: NpTarget::CurrentTime,
            mode: NpMode::Select,
            cursor: 0,
        }
    }
}

impl NowPlayingFocus {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// UI_13 (owner-approved change to the frozen control rule): any rotation on Now Playing
    /// with a list panel opens the overlay seeded on `playing_row`, then applies `delta`.
    ///
    /// Returns `true` when the overlay is open and the caller must arm its timeout. A single
    /// track (no panel) never reaches here: the app keeps the rotation as jog.
    pub fn rotate_opens_list(&mut self, delta: i8, len: usize, playing_row: usize) -> bool {
        if len == 0 {
            return false;
        }
        self.mode = NpMode::List;
        self.target = NpTarget::List;
        self.cursor = playing_row.min(len - 1);
        self.move_cursor(delta, len);
        true
    }

    /// List mode: move the cursor by `delta` rows within `len` rows (clamped, no wrap).
    pub fn move_cursor(&mut self, delta: i8, len: usize) {
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let next = (self.cursor as isize + delta as isize).clamp(0, len as isize - 1);
        self.cursor = next as usize;
    }

    /// Menu push. The cursor is seeded by [`Self::rotate_opens_list`], so a push in list mode
    /// is always a jump to that row.
    pub fn push(&mut self) -> NpPush {
        match self.mode {
            NpMode::Adjust(cur) => {
                let p = cur.next();
                self.mode = NpMode::Adjust(p);
                NpPush::Adjust(p)
            }
            NpMode::List => NpPush::Jump(self.cursor),
            NpMode::Select => {
                let p = PARAM_RING[0];
                self.mode = NpMode::Adjust(p);
                NpPush::Adjust(p)
            }
        }
    }

    /// Enter on a single-track screen: the implicit level counts as ring entry 0, so the
    /// first Enter goes to the **next** entry (Tempo), then on around the ring.
    pub fn push_single(&mut self) -> AdjustParam {
        let p = match self.mode {
            NpMode::Adjust(cur) => cur.next(),
            _ => PARAM_RING[0].next(),
        };
        self.mode = NpMode::Adjust(p);
        p
    }

    /// The parameter being adjusted, if any.
    pub fn param(&self) -> Option<AdjustParam> {
        match self.mode {
            NpMode::Adjust(p) => Some(p),
            _ => None,
        }
    }

    /// Back: leave jog / list mode.
    ///
    /// WAVE_02: Back **always returns to the waveform**, so leaving `Adjust` or `List` also
    /// resets the target to [`NpTarget::CurrentTime`] (the Up Next overlay closes with it).
    /// Returns `true` when Back was consumed.
    pub fn back(&mut self) -> bool {
        if self.mode == NpMode::Select {
            false
        } else {
            self.mode = NpMode::Select;
            self.target = NpTarget::CurrentTime;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigate_enter_edit_rotate_back() {
        let mut g = ControlGraph::dummy();
        assert_eq!(g.mode(), InteractionMode::Navigate);
        assert_eq!(g.focus(), 0);
        g.rotate(1);
        assert_eq!(g.focus(), 1);
        assert_eq!(g.focused_value(), Some(5));
        g.enter();
        assert_eq!(g.mode(), InteractionMode::Edit);
        g.rotate(1);
        assert_eq!(g.focused_value(), Some(6));
        g.rotate(-3);
        assert_eq!(g.focused_value(), Some(3));
        assert!(g.back());
        assert_eq!(g.mode(), InteractionMode::Navigate);
        // Further rotate moves focus, not value.
        g.rotate(-1);
        assert_eq!(g.focus(), 0);
        assert_eq!(g.focused_value(), Some(0));
        assert!(!g.back());
    }

    #[test]
    fn now_playing_focus_levels_unwind_one_at_a_time() {
        let mut f = NowPlayingFocus::default();
        assert_eq!((f.target, f.mode), (NpTarget::CurrentTime, NpMode::Select));
        assert!(!f.back(), "back in select mode is not consumed");

        // UI_13: a rotation opens the list seeded on the playing row, then moves one row.
        assert!(f.rotate_opens_list(1, 10, 3));
        assert_eq!((f.mode, f.target, f.cursor), (NpMode::List, NpTarget::List, 4));
        f.move_cursor(2, 10);
        assert_eq!(f.cursor, 6);
        f.move_cursor(-9, 10);
        assert_eq!(f.cursor, 0, "clamped at the top");
        f.move_cursor(99, 10);
        assert_eq!(f.cursor, 9, "clamped at the bottom");
        assert_eq!(f.push(), NpPush::Jump(9));
        assert_eq!(f.mode, NpMode::List, "jump stays in list mode; the caller closes it");
        assert!(f.back());
        // WAVE_02: Back leaves list mode and returns to the waveform target.
        assert_eq!((f.mode, f.target), (NpMode::Select, NpTarget::CurrentTime));

        // Enter walks the ring [Jog, Tempo, Zoom] and wraps.
        assert_eq!(f.push(), NpPush::Adjust(AdjustParam::Jog));
        assert_eq!(f.push(), NpPush::Adjust(AdjustParam::Tempo));
        assert_eq!(f.push(), NpPush::Adjust(AdjustParam::Zoom));
        assert_eq!(f.push(), NpPush::Adjust(AdjustParam::Jog));
        // Back from any ring parameter returns to Select (not further out).
        assert!(f.back());
        assert_eq!((f.mode, f.target), (NpMode::Select, NpTarget::CurrentTime));
        assert_eq!(f.push(), NpPush::Adjust(AdjustParam::Jog));
        f.push();
        assert!(f.back());
        assert_eq!(f.mode, NpMode::Select);
        assert!(!f.back());
    }

    #[test]
    fn rotate_opens_list_either_direction() {
        for delta in [1, -1] {
            let mut f = NowPlayingFocus::default();
            assert!(f.rotate_opens_list(delta, 8, 3));
            assert_eq!(f.mode, NpMode::List);
            assert_eq!(f.target, NpTarget::List);
            assert_eq!(
                f.cursor,
                if delta > 0 { 4 } else { 2 },
                "one row in the turn's direction from the playing row"
            );
        }
        // A single-track screen never opens the panel.
        let mut f = NowPlayingFocus::default();
        assert!(!f.rotate_opens_list(1, 0, 0));
        assert_eq!((f.mode, f.target), (NpMode::Select, NpTarget::CurrentTime));
        // The playing row is clamped into the list.
        assert!(f.rotate_opens_list(1, 3, 9));
        assert_eq!(f.cursor, 2);
    }

    #[test]
    fn param_ring_cycles_every_entry() {
        // Panel layout: every ring entry is visited once per lap, then it wraps.
        let mut f = NowPlayingFocus::default();
        let mut seen = vec![];
        for _ in 0..PARAM_RING.len() * 2 {
            match f.push() {
                NpPush::Adjust(p) => seen.push(p),
                other => panic!("{other:?}"),
            }
        }
        let lap: Vec<_> = PARAM_RING.to_vec();
        assert_eq!(&seen[..PARAM_RING.len()], &lap[..]);
        assert_eq!(&seen[PARAM_RING.len()..], &lap[..]);
        // Single-track layout: implicit level = entry 0, so Enter starts at entry 1.
        let mut f = NowPlayingFocus::default();
        let mut seen = vec![];
        for _ in 0..PARAM_RING.len() {
            seen.push(f.push_single());
        }
        let want: Vec<_> = (1..=PARAM_RING.len()).map(|i| PARAM_RING[i % PARAM_RING.len()]).collect();
        assert_eq!(seen, want);
        assert!(f.back());
        assert_eq!(f.param(), None);
    }
}
