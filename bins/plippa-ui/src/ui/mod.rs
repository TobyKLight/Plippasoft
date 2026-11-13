//! The Plippa screen design system.
//!
//! This module is the **contract** every screen on the 320×240 ILI9341 paints with. It exists
//! so a screen never does raw `fill_rect` + colour-literal arithmetic: colours, type sizes,
//! spacing and the standard widgets live here, and screens compose them.
//!
//! ```text
//! ui/
//!   mod.rs         // this contract + re-exports
//!   tokens.rs      // colours, spacing, geometry — the only colour literals in the crate
//!   text.rs        // TypeRole, Align, measure/fit_chars/draw/draw_aligned/draw_clipped/wrap
//!   primitives.rs  // fill, border, hairline, triangle, chevron, scrollbar
//!   icons.rs       // the 8×8 Smallbits bitmaps and `draw_icon` (1:1, never scaled)
//!   widgets.rs     // header, footer, row/list, badge, progress, gauge, key/value, banner, art
//! ```
//!
//! # Rules
//!
//! 1. **Colours come from [`tokens`] only.** A `rgb565(…)` call outside `tokens.rs` is a bug.
//!    The one exception is the [`rgb565`](crate::display::rgb565) helper's own definition.
//! 2. **Spacing comes from [`tokens::SPACE_1`] … [`tokens::SPACE_6`] and
//!    [`tokens::MARGIN_X`].** Do not invent gaps; the row rhythm (`ROW_H`) is what makes
//!    screens look like one product.
//! 3. **Type is a role, not a font.** `TypeRole` is `Display` (10×20), `Body` (9×15),
//!    `BodyBold`, `Small` (6×13) or `SmallBold`. `y` is always the *top* of the cell.
//! 4. **Lower case is real.** The old bitmap font upper-cased everything; the new fonts do not.
//!    Put capitals in the string literals where you want them and never call `to_uppercase()`.
//! 5. **Every string is clipped to its box** with [`text::draw_clipped`] (or
//!    [`text::draw_aligned`], which clips too). Widgets must not let text escape their bounds.
//! 6. **Focus is a ring, not a colour flip.** A focused row paints `SURFACE` and a 2 px
//!    `ACCENT` ring *inside* the row; the label keeps its normal colour. Never invert a whole
//!    row to `ACCENT` — on this panel it reads as an error state.
//! 7. **Alternating rows are `SURFACE_ALT`;** the playing row is marked with a filled
//!    `ACCENT_2` [`primitives::triangle_right`], not by recolouring text.
//! 8. **Widgets paint their own background first**, so overdrawing is safe.
//! 9. **Shell chrome:** leaf screens use [`widgets::header`] (`HEADER_H` band, `Display` title,
//!    optional right-aligned `Small` meta, 1 px `ACCENT_DIM` rule) and start their body at
//!    [`tokens::CONTENT_TOP`]; they must stop at [`tokens::CONTENT_BOTTOM`]. A route with
//!    status text uses [`widgets::footer`]; a route with nothing to say paints no footer band.
//! 10. **Characters the Latin-1 font cannot draw are transliterated** (dashes, curly quotes,
//!     ellipsis, arrows …) or dropped by [`text`]; nothing is ever drawn as a box glyph.
//!
//! # Painting a screen
//!
//! ```ignore
//! use crate::ui::{text, tokens, widgets};
//! use crate::ui::widgets::{Header, Row, RowState};
//! use crate::ui::text::TypeRole;
//!
//! fn paint(fb: &mut FrameBuffer, title: &str, items: &[String], focus: usize) {
//!     // 1. Shell chrome.
//!     widgets::header(fb, Header { title, meta: Some("3/12") });
//!
//!     // 2. Body: rows inside CONTENT_TOP..CONTENT_BOTTOM.
//!     let (start, visible) = widgets::list(
//!         fb,
//!         widgets::List {
//!             rows: items,
//!             focus,
//!             top: tokens::CONTENT_TOP,
//!             dense: false,
//!             disabled: None,
//!             warning: None,
//!             marker: None,
//!             show_focus: true,
//!             indent: None,
//!             window_start: None,
//!         },
//!     );
//!     let _ = (start, visible);
//!
//!     // 3. Footer only when there is something to say.
//!     widgets::footer(
//!         fb,
//!         widgets::Footer { left: "Enter=play", centre: "", right: "" },
//!     );
//!
//!     // Free-standing text still goes through a role.
//!     text::draw_clipped(fb, tokens::MARGIN_X, 200, 200, "Muted hint", TypeRole::Small,
//!                        tokens::FG_MUTED);
//! }
//! ```
//!
//! This module is the crate's design-system API: screens paint through it, and the pixel-level
//! integration tests read [`tokens`] to assert colours.

pub mod icons;
pub mod primitives;
pub mod text;
pub mod tokens;
pub mod widgets;
