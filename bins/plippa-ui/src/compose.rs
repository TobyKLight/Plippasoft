//! Layered compositor: screen → main-menu overlay → status popup.
//!
//! # Layers (bottom → top)
//! 1. **Screen** — full 320×240 content for the current route.
//! 2. **Main menu overlay** — optional top band (`y = 0..50`).
//! 3. **Status popup** — optional bottom band; height shrink-wraps to content
//!    (max 60 px / two `Display` lines).
//!
//! # Status popup dismiss policy (UI_01)
//! [`Compositor::show_status`] timestamps the popup; [`Compositor::on_tick`]
//! auto-clears it after [`STATUS_POPUP_IDLE_MS`]. Callers may also
//! [`Compositor::clear_status`] early (navigation / replace).

use crate::display::{FrameBuffer, Rgb565};
use crate::ui::{text, tokens};
use crate::ui::text::{cell_height, TypeRole};

/// Logical screen width (poc2 landscape). Canonical value in [`tokens`].
pub const SCREEN_W: u16 = tokens::SCREEN_W;
/// Logical screen height (poc2 landscape). Canonical value in [`tokens`].
pub const SCREEN_H: u16 = tokens::SCREEN_H;
/// Main-menu overlay height (top band).
pub const MAIN_MENU_H: u16 = 50;
/// Status popup maximum height: two `Display` rows (UI_13).
pub const STATUS_POPUP_MAX_H: u16 = 2 * tokens::ROW_H_DISPLAY;
/// Status popup maximum text lines (UI_13: two).
pub const STATUS_POPUP_MAX_LINES: usize = 2;
/// Idle time before the main-menu overlay auto-hides.
pub const MAIN_MENU_IDLE_MS: u64 = 3_000;
/// Time before the status popup auto-clears.
pub const STATUS_POPUP_IDLE_MS: u64 = 3_000;

/// Bottom status popup content (≤2 `Display` lines, one 30 px row per line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusPopup {
    lines: Vec<String>,
}

impl StatusPopup {
    /// Build from free text; splits on newlines and caps at two lines.
    pub fn from_text(text: &str) -> Self {
        // SEC-12: popup text can quote tag keys, titles and notices.
        let text = plippa_noubin::sanitise_multiline(text, 1024);
        let mut lines: Vec<String> = text
            .lines()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .take(STATUS_POPUP_MAX_LINES)
            .collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self { lines }
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Content height in pixels: one 30 px `Display` row per line, capped at
    /// [`STATUS_POPUP_MAX_H`]. The row *is* the padding, so there is no gap or inset term.
    pub fn height_px(&self) -> u16 {
        let n = self.lines.len().max(1) as u16;
        n.saturating_mul(tokens::ROW_H_DISPLAY)
            .min(STATUS_POPUP_MAX_H)
    }
}

/// Overlay + popup state on top of the screen layer.
#[derive(Debug, Clone)]
pub struct Compositor {
    main_menu_visible: bool,
    /// Last input activity while the menu overlay was open (`now_ms`).
    last_activity_ms: Option<u64>,
    status: Option<StatusPopup>,
    /// When the current status popup was shown (`now_ms`).
    status_shown_ms: Option<u64>,
}

impl Default for Compositor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compositor {
    pub fn new() -> Self {
        Self {
            main_menu_visible: false,
            last_activity_ms: None,
            status: None,
            status_shown_ms: None,
        }
    }

    pub fn main_menu_visible(&self) -> bool {
        self.main_menu_visible
    }

    pub fn show_main_menu(&mut self, now_ms: u64) {
        self.main_menu_visible = true;
        self.last_activity_ms = Some(now_ms);
    }

    pub fn hide_main_menu(&mut self) {
        self.main_menu_visible = false;
        self.last_activity_ms = None;
    }

    /// Reset the 3s idle timer (any relevant button/encoder while open).
    pub fn note_activity(&mut self, now_ms: u64) {
        if self.main_menu_visible {
            self.last_activity_ms = Some(now_ms);
        }
    }

    /// Auto-hide the main menu after [`MAIN_MENU_IDLE_MS`] and clear status
    /// after [`STATUS_POPUP_IDLE_MS`]. Returns `true` when either changed.
    pub fn on_tick(&mut self, now_ms: u64) -> bool {
        let mut changed = false;

        if self.main_menu_visible {
            match self.last_activity_ms {
                None => self.last_activity_ms = Some(now_ms),
                Some(start) if now_ms.saturating_sub(start) >= MAIN_MENU_IDLE_MS => {
                    self.hide_main_menu();
                    changed = true;
                }
                Some(_) => {}
            }
        }

        if self.clear_status_if_idle(now_ms) {
            changed = true;
        }
        changed
    }

    fn clear_status_if_idle(&mut self, now_ms: u64) -> bool {
        let Some(shown) = self.status_shown_ms else {
            return false;
        };
        if self.status.is_none() {
            self.status_shown_ms = None;
            return false;
        }
        if now_ms.saturating_sub(shown) >= STATUS_POPUP_IDLE_MS {
            self.clear_status();
            true
        } else {
            false
        }
    }

    pub fn show_status(&mut self, text: &str, now_ms: u64) {
        self.status = Some(StatusPopup::from_text(text));
        self.status_shown_ms = Some(now_ms);
    }

    /// A popup with **no idle timeout**: it stays until [`Self::clear_status`].
    ///
    /// PWRSW_01 needs this for the hold-to-power warning: it appears at 1 s and must remain
    /// up for the whole 1 s → 4 s window, which is exactly the 3 s popup timeout.
    pub fn show_status_sticky(&mut self, text: &str) {
        self.status = Some(StatusPopup::from_text(text));
        self.status_shown_ms = None;
    }

    pub fn clear_status(&mut self) {
        self.status = None;
        self.status_shown_ms = None;
    }

    pub fn status(&self) -> Option<&StatusPopup> {
        self.status.as_ref()
    }

    /// Clear → screen → optional menu overlay → optional status popup.
    pub fn compose_into<FScreen, FMenu>(
        &self,
        fb: &mut FrameBuffer,
        clear_color: Rgb565,
        paint_screen: FScreen,
        paint_menu: FMenu,
    ) where
        FScreen: FnOnce(&mut FrameBuffer),
        FMenu: FnOnce(&mut FrameBuffer),
    {
        fb.clear(clear_color);
        paint_screen(fb);
        if self.main_menu_visible {
            fb.fill_rect(
                0,
                0,
                fb.width,
                MAIN_MENU_H.min(fb.height),
                tokens::SURFACE,
            );
            paint_menu(fb);
        }
        if let Some(popup) = &self.status {
            paint_status_popup(fb, popup);
        }
    }
}

fn paint_status_popup(fb: &mut FrameBuffer, popup: &StatusPopup) {
    let h = popup.height_px().min(fb.height);
    let y0 = fb.height.saturating_sub(h);
    fb.fill_rect(0, y0, fb.width, h, tokens::SURFACE);
    fb.fill_rect(0, y0, fb.width, tokens::RULE_H, tokens::ACCENT_DIM);
    let mut y = y0.saturating_add(
        tokens::ROW_H_DISPLAY.saturating_sub(cell_height(TypeRole::Display)) / 2,
    );
    let line_h = tokens::ROW_H_DISPLAY;
    for line in popup.lines() {
        if y.saturating_add(cell_height(TypeRole::Display)) > fb.height {
            break;
        }
        text::draw_clipped(
            fb,
            tokens::MARGIN_X,
            y,
            fb.width.saturating_sub(tokens::MARGIN_X.saturating_mul(2)),
            line,
            TypeRole::Display,
            tokens::FG,
        );
        y = y.saturating_add(line_h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::MockDisplay;
    use crate::display::DisplayBackend;
    use crate::ui::tokens::{ACCENT_DIM, BG, FG};

    #[test]
    fn status_popup_height_shrink_wraps_and_caps_lines() {
        let one = StatusPopup::from_text("hello");
        assert_eq!(one.lines().len(), 1);
        assert_eq!(one.height_px(), 30, "one Display row");
        assert_eq!(STATUS_POPUP_MAX_H, 60);

        let two = StatusPopup::from_text("a\nb\nc\nd\ne");
        assert_eq!(two.lines().len(), 2, "two-line cap");
        assert_eq!(two.height_px(), 60);
        assert_eq!(two.lines(), ["a", "b"]);

        let five = StatusPopup {
            lines: vec!["x".into(), "y".into(), "z".into(), "t".into(), "u".into()],
        };
        assert_eq!(five.height_px(), 60, "capped at two rows");
    }

    #[test]
    fn status_popup_is_thirty_per_line() {
        let one = StatusPopup::from_text("hello");
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(BG);
        paint_status_popup(&mut fb, &one);
        // Band ground starts exactly 30 px above the screen bottom.
        assert_eq!(fb.pixels[(SCREEN_H - 30) as usize * SCREEN_W as usize], ACCENT_DIM);
        assert_eq!(fb.pixels[(SCREEN_H - 31) as usize * SCREEN_W as usize], BG);
        // Display ink (20 px cell) is present in the single row.
        let ink = |fb: &FrameBuffer, y0: u16, y1: u16| {
            (y0..y1)
                .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
                .filter(|&(x, y)| fb.pixels[y as usize * SCREEN_W as usize + x as usize] == FG)
                .count()
        };
        assert!(ink(&fb, SCREEN_H - 30, SCREEN_H) > 20);

        let two = StatusPopup::from_text("hello\nworld");
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(BG);
        paint_status_popup(&mut fb, &two);
        // Two rows: 60 px, line 1 forty px above the screen bottom, line 2 ten px above it.
        assert!(ink(&fb, SCREEN_H - 60, SCREEN_H - 30) > 20, "first row");
        assert!(ink(&fb, SCREEN_H - 30, SCREEN_H) > 20, "second row");
    }

    #[test]
    fn compose_layers_screen_menu_popup() {
        // Sentinels: deliberately not design tokens, they only mark which layer wrote a pixel.
        const SCREEN_INK: Rgb565 = 0xF800;
        const MENU_INK: Rgb565 = 0x07E0;

        let mut comp = Compositor::new();
        comp.show_main_menu(0);
        comp.show_status("line1\nline2", 0);
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        let mut menu_painted = false;
        let mut screen_painted = false;
        comp.compose_into(
            &mut fb,
            tokens::BG,
            |f| {
                screen_painted = true;
                f.fill_rect(10, 100, 4, 4, SCREEN_INK);
            },
            |f| {
                menu_painted = true;
                f.fill_rect(2, 2, 4, 4, MENU_INK);
            },
        );
        assert!(screen_painted && menu_painted);
        // The overlay band (SURFACE) was painted, then the menu strip over it.
        assert_eq!(fb.pixels[MAIN_MENU_H as usize * SCREEN_W as usize - 1], tokens::SURFACE);
        assert_eq!(fb.pixels[2usize * SCREEN_W as usize + 2], MENU_INK);
        // Screen pixel below menu band.
        assert_eq!(fb.pixels[100usize * SCREEN_W as usize + 10], SCREEN_INK);
        // Popup occupies bottom — its top edge is the accent rule, not the clear colour.
        let popup_h = comp.status().unwrap().height_px();
        let py = (SCREEN_H - popup_h) as usize;
        assert_eq!(fb.pixels[py * SCREEN_W as usize], tokens::ACCENT_DIM);

        let mut display = MockDisplay::new(SCREEN_W, SCREEN_H);
        display.present(&fb);
        assert_eq!(display.frames, 1);
    }

    #[test]
    fn menu_idle_timeout_and_activity_reset() {
        let mut comp = Compositor::new();
        comp.show_main_menu(0);
        assert!(comp.main_menu_visible());
        assert!(!comp.on_tick(2_999));
        assert!(comp.main_menu_visible());
        comp.note_activity(2_999);
        assert!(!comp.on_tick(5_000)); // 5000-2999 = 2001 < 3000
        assert!(comp.main_menu_visible());
        assert!(comp.on_tick(6_000)); // 6000-2999 >= 3000
        assert!(!comp.main_menu_visible());
    }

    #[test]
    fn sticky_status_never_auto_clears() {
        // PWRSW_01: the hold-to-power warning spans the 1 s → 4 s hold, longer than the
        // normal 3 s popup life. Sticky means "until clear_status".
        let mut comp = Compositor::new();
        comp.show_status_sticky("Keep holding to power off");
        assert!(comp.status().is_some());
        assert!(!comp.on_tick(10_000));
        assert!(comp.status().is_some(), "sticky popup must survive the idle timeout");
        comp.clear_status();
        assert!(comp.status().is_none());
        assert!(!comp.on_tick(20_000), "already cleared");
    }

    #[test]
    fn status_popup_auto_clears_after_idle() {
        let mut comp = Compositor::new();
        comp.show_status("hello", 1_000);
        assert!(comp.status().is_some());
        assert!(!comp.on_tick(3_999)); // 3999-1000 = 2999 < 3000
        assert!(comp.status().is_some());
        assert!(comp.on_tick(4_000)); // 4000-1000 >= 3000
        assert!(comp.status().is_none());
        assert!(!comp.on_tick(5_000)); // already cleared
    }

    #[test]
    fn status_clear_independent_of_menu_idle() {
        // Show status later than menu so the timers diverge.
        let mut comp = Compositor::new();
        comp.show_main_menu(0);
        comp.show_status("hi", 2_000);
        assert!(comp.on_tick(3_000)); // menu hides (3000-0 >= 3000); status still up
        assert!(!comp.main_menu_visible());
        assert!(comp.status().is_some());
        assert!(!comp.on_tick(4_999)); // status: 4999-2000 = 2999
        assert!(comp.status().is_some());
        assert!(comp.on_tick(5_000)); // status: 5000-2000 >= 3000
        assert!(comp.status().is_none());
    }
}
