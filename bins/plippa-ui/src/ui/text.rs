//! Type roles and text painting.
//!
//! Five roles cover every screen: a screen picks a semantic size, never a raw font. All of them
//! are real bitmap fonts from `embedded-graphics`' ISO-8859-1 mapping, so accented Latin
//! characters render as glyphs (not boxes) and lower case is real.
//!
//! | Role | Font | Cell | Advance |
//! |---|---|---|---|
//! | [`TypeRole::Display`] | `FONT_10X20` | 10×20 | 10 |
//! | [`TypeRole::Body`] | `FONT_9X15` | 9×15 | 9 |
//! | [`TypeRole::BodyBold`] | `FONT_9X15_BOLD` | 9×15 | 9 |
//! | [`TypeRole::Small`] | `FONT_6X13` | 6×13 | 6 |
//! | [`TypeRole::SmallBold`] | `FONT_6X13_BOLD` | 6×13 | 6 |
//!
//! `y` is always the **top** of the cell (`Baseline::Top`), so layout arithmetic is about the
//! cell rather than a font baseline.
//!
//! Within the UI layer, `embedded-graphics` types are used only by this module (for glyph
//! rendering); [`super::primitives`] draws with plain framebuffer calls. The ILI9341 backend in
//! `crate::hw::ili9341` also uses the crate for its raw draw target. Everywhere else works with
//! the plain `u16` [`Rgb565`](crate::display::Rgb565) of the framebuffer.

use core::convert::Infallible;

use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{OriginDimensions, Point, Size},
    mono_font::{
        iso_8859_1::{FONT_6X13, FONT_6X13_BOLD, FONT_9X15, FONT_9X15_BOLD, FONT_10X20},
        MonoFont, MonoTextStyle,
    },
    pixelcolor::{
        raw::{RawData, RawU16},
        Rgb565 as EgRgb565,
    },
    text::{Baseline, Text},
    Drawable, Pixel,
};

use crate::display::{FrameBuffer, Rgb565};

/// Semantic text size. Screens choose a role, never a font.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeRole {
    /// Titles: 10×20 (32 characters across the screen).
    Display,
    /// Body copy: 9×15 (35 characters).
    Body,
    /// Emphasised body copy (focused menu item, key/value value): 9×15 bold.
    BodyBold,
    /// Secondary text, dense lists: 6×13 (53 characters).
    Small,
    /// Emphasised dense text (badges): 6×13 bold.
    SmallBold,
}

impl TypeRole {
    /// The linked bitmap font for this role.
    const fn font(self) -> &'static MonoFont<'static> {
        match self {
            Self::Display => &FONT_10X20,
            Self::Body => &FONT_9X15,
            Self::BodyBold => &FONT_9X15_BOLD,
            Self::Small => &FONT_6X13,
            Self::SmallBold => &FONT_6X13_BOLD,
        }
    }
}

/// Horizontal alignment inside an explicit box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Centre,
    Right,
}

/// Cell height in pixels (the vertical advance of a line).
pub const fn cell_height(role: TypeRole) -> u16 {
    match role {
        TypeRole::Display => 20,
        TypeRole::Body | TypeRole::BodyBold => 15,
        TypeRole::Small | TypeRole::SmallBold => 13,
    }
}

/// Horizontal advance of one character in pixels (the fonts are monospaced).
pub const fn advance(role: TypeRole) -> u16 {
    match role {
        TypeRole::Display => 10,
        TypeRole::Body | TypeRole::BodyBold => 9,
        TypeRole::Small | TypeRole::SmallBold => 6,
    }
}

/// Pixel width of `text` in `role` after transliteration (may exceed the screen width).
pub fn measure(text: &str, role: TypeRole) -> u16 {
    let adv = u32::from(advance(role));
    let mut px = 0u32;
    for ch in text.chars() {
        px += adv * u32::from(mapped_len(ch));
    }
    px.min(u32::from(u16::MAX)) as u16
}

/// How many characters of `role` fit in `max_px` (for truncation decisions).
pub fn fit_chars(role: TypeRole, max_px: u16) -> usize {
    (max_px / advance(role).max(1)) as usize
}

/// Draw `text` with its top-left cell corner at `(x, y)`, clipped at the framebuffer edge.
pub fn draw(fb: &mut FrameBuffer, x: u16, y: u16, text: &str, role: TypeRole, color: Rgb565) {
    let text = transliterate(text);
    draw_raw(fb, x, y, &text, role, color);
}

/// Draw `text` inside the box `x..x+w`, aligned, never painting outside that box.
pub fn draw_aligned(
    fb: &mut FrameBuffer,
    x: u16,
    y: u16,
    w: u16,
    text: &str,
    role: TypeRole,
    color: Rgb565,
    align: Align,
) {
    if w == 0 {
        return;
    }
    let text = transliterate(text);
    // Drop characters that do not fit *before* aligning, so a right-aligned string can never
    // start left of `x`.
    let fitted = if measure(&text, role) > w {
        fit_str(&text, w, role)
    } else {
        text
    };
    let dx = match align {
        Align::Left => x,
        Align::Centre => x.saturating_add((w.saturating_sub(measure(&fitted, role))) / 2),
        Align::Right => x.saturating_add(w.saturating_sub(measure(&fitted, role))),
    };
    draw_raw(fb, dx, y, &fitted, role, color);
}

/// Draw `text` inside `x..x+w`, ending in `..` when it had to be shortened.
pub fn draw_clipped(
    fb: &mut FrameBuffer,
    x: u16,
    y: u16,
    w: u16,
    text: &str,
    role: TypeRole,
    color: Rgb565,
) {
    if w == 0 {
        return;
    }
    let text = transliterate(text);
    let fitted = fit_str(&text, w, role);
    draw_raw(fb, x, y, &fitted, role, color);
}

/// Wrap `text` into lines of at most `max_px` pixels (hard wrap, no word breaks).
///
/// The concatenation of the returned lines is always the transliterated input, so callers can
/// rely on no characters being lost.
pub fn wrap(text: &str, max_px: u16, role: TypeRole) -> Vec<String> {
    let text = transliterate(text);
    if text.is_empty() {
        return vec![String::new()];
    }
    let adv = u32::from(advance(role).max(1));
    let max = u32::from(max_px);
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_px = 0u32;
    for ch in text.chars() {
        let mut buf = [0u8; 4];
        let mapped = substitute(ch).unwrap_or(ch.encode_utf8(&mut buf));
        let char_px = adv * mapped.chars().count() as u32;
        if !line.is_empty() && line_px + char_px > max {
            out.push(core::mem::take(&mut line));
            line_px = 0;
        }
        line.push_str(mapped);
        line_px += char_px;
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// Map characters the ISO-8859-1 font cannot represent to something drawable, and drop the
/// rest. Guarantees every returned character is `<= '\u{FF}'` so the font never falls back to
/// a box glyph.
fn transliterate(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match substitute(ch) {
            Some(s) => out.push_str(s),
            None => out.push(ch),
        }
    }
    out
}

/// Substitution for one character: `None` keeps it as-is (it is representable), `Some("")`
/// drops it, and any other `Some(s)` is the replacement text.
///
/// This is the one transliteration table. Keep it here: the codebase draws `—` as the Now
/// Playing placeholder, curly quotes in titles and `…` in messages.
fn substitute(ch: char) -> Option<&'static str> {
    match ch {
        // Dashes → hyphen
        '\u{2014}' | '\u{2013}' | '\u{2212}' => Some("-"),
        // Curly quotes → straight quotes
        '\u{2018}' | '\u{2019}' => Some("'"),
        '\u{201C}' | '\u{201D}' => Some("\""),
        '\u{2026}' => Some("..."),
        '\u{2022}' => Some("*"),
        '\u{2192}' => Some(">"),
        '\u{2190}' => Some("<"),
        '\u{00D7}' => Some("x"),
        // Non-breaking space → space
        '\u{00A0}' => Some(" "),
        // Line controls never reach a glyph cell as-is.
        '\n' | '\r' | '\t' => Some(" "),
        // Other C0/C1 controls have no glyph; drop them rather than draw a box.
        c if is_control(c) => Some(""),
        // Everything the ISO-8859-1 mapping covers is drawn as-is.
        c if (c as u32) <= 0xFF => None,
        // Anything above U+00FF has no glyph in this font: drop it.
        _ => Some(""),
    }
}

fn is_control(c: char) -> bool {
    matches!(c, '\u{00}'..='\u{1F}' | '\u{7F}'..='\u{9F}')
}

/// Number of glyph cells `ch` occupies after transliteration.
fn mapped_len(ch: char) -> u8 {
    match substitute(ch) {
        Some(s) => s.chars().count() as u8,
        None => 1,
    }
}

/// Shorten `text` to fit `max_px`, ending in `..` when characters were dropped.
fn fit_str(text: &str, max_px: u16, role: TypeRole) -> String {
    let fit = fit_chars(role, max_px);
    let len = measure(text, role);
    if len <= max_px {
        return text.to_string();
    }
    if fit <= 2 {
        return text.chars().take(fit).collect();
    }
    let mut out: String = text.chars().take(fit - 2).collect();
    out.push_str("..");
    out
}

/// Converts the framebuffer's packed `u16` to/from the `embedded-graphics` colour type.
fn to_eg(color: Rgb565) -> EgRgb565 {
    EgRgb565::from(RawU16::new(color))
}

fn from_eg(color: EgRgb565) -> Rgb565 {
    RawU16::from(color).into_inner()
}

/// `embedded-graphics` draw target writing straight into a [`FrameBuffer`].
///
/// `FrameBuffer::set_pixel` drops out-of-range pixels, which is what clips glyphs at the screen
/// edge; a character that starts on the last column simply loses the columns past the edge.
struct TextTarget<'a> {
    fb: &'a mut FrameBuffer,
}

impl DrawTarget for TextTarget<'_> {
    type Color = EgRgb565;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            let (Ok(x), Ok(y)) = (u16::try_from(point.x), u16::try_from(point.y)) else {
                continue;
            };
            self.fb.set_pixel(x, y, from_eg(color));
        }
        Ok(())
    }
}

impl OriginDimensions for TextTarget<'_> {
    fn size(&self) -> Size {
        Size::new(u32::from(self.fb.width), u32::from(self.fb.height))
    }
}

fn draw_raw(fb: &mut FrameBuffer, x: u16, y: u16, text: &str, role: TypeRole, color: Rgb565) {
    if text.is_empty() {
        return;
    }
    let style = MonoTextStyle::new(role.font(), to_eg(color));
    let mut target = TextTarget { fb };
    // `Infallible`: the only error path is the draw target's, and `TextTarget` cannot fail.
    let _ = Text::with_baseline(
        text,
        Point::new(i32::from(x), i32::from(y)),
        style,
        Baseline::Top,
    )
    .draw(&mut target);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tokens::{SCREEN_H, SCREEN_W};

    #[test]
    fn type_roles_have_expected_metrics() {
        for (role, cell, adv) in [
            (TypeRole::Display, 20, 10),
            (TypeRole::Body, 15, 9),
            (TypeRole::BodyBold, 15, 9),
            (TypeRole::Small, 13, 6),
            (TypeRole::SmallBold, 13, 6),
        ] {
            assert_eq!(cell_height(role), cell, "{role:?} cell height");
            assert_eq!(advance(role), adv, "{role:?} advance");
        }
        for role in [
            TypeRole::Display,
            TypeRole::Body,
            TypeRole::BodyBold,
            TypeRole::Small,
            TypeRole::SmallBold,
        ] {
            for text in ["", "A", "Hello", "Café - Live", "0123456789"] {
                assert_eq!(
                    measure(text, role),
                    text.chars().count() as u16 * advance(role),
                    "{role:?} {text:?}"
                );
            }
        }
        assert_eq!(fit_chars(TypeRole::Small, 60), 10);
        assert_eq!(fit_chars(TypeRole::Display, 320), 32);
    }

    #[test]
    fn transliteration_maps_or_drops() {
        let table = [
            ('\u{2014}', "-"),
            ('\u{2013}', "-"),
            ('\u{2019}', "'"),
            ('\u{2018}', "'"),
            ('\u{201C}', "\""),
            ('\u{201D}', "\""),
            ('\u{2026}', "..."),
            ('\u{2022}', "*"),
            ('\u{2192}', ">"),
            ('\u{2190}', "<"),
            ('\u{00D7}', "x"),
            ('\u{00A0}', " "),
        ];
        for (input, expected) in table {
            assert_eq!(transliterate(&input.to_string()), expected, "{input:?}");
        }
        assert_eq!(transliterate("\u{2014}\u{00E9}"), "-é");
        // Kept as-is: accented Latin-1 letters.
        assert_eq!(transliterate("Café — Live"), "Café - Live");
        // Emoji / CJK / astral characters are dropped, never drawn as a box.
        assert_eq!(transliterate("a\u{1F3B5}\u{65E5}\u{672C}b"), "ab");
        assert_eq!(transliterate(""), "");
        // Whatever the input, every output character is representable by the font.
        for input in [
            "Café — Live",
            "\u{1F3B5}\u{65E5}\u{672C}\u{1D11E}",
            "normal ASCII",
            "\u{00FF}\u{0100}\u{00A0}\u{2026}",
            "",
        ] {
            let out = transliterate(input);
            assert!(
                out.chars().all(|c| (c as u32) <= 0xFF),
                "{input:?} produced {out:?}"
            );
        }
    }

    #[test]
    fn draw_clipped_ellipsises() {
        // "ABCDEFGHIJ" is 10 chars = 90 px at Body; the box is 89 px → one char too wide.
        let mut wide = FrameBuffer::new(SCREEN_W, SCREEN_H);
        wide.clear(0x1234);
        let before = wide.clone();
        draw_clipped(&mut wide, 10, 10, 89, "ABCDEFGHIJ", TypeRole::Body, 0xFFFF);

        // Anything outside the box is untouched.
        for y in 0..SCREEN_H as usize {
            for x in 0..SCREEN_W as usize {
                let inside = (10..10 + 89).contains(&x) && (10..10 + 15).contains(&y);
                if !inside {
                    assert_eq!(
                        wide.pixels[y * SCREEN_W as usize + x],
                        before.pixels[y * SCREEN_W as usize + x],
                        "pixel ({x},{y}) outside the clip box changed"
                    );
                }
            }
        }
        // One char too wide → 8 chars plus ".." = fit(89/9 = 9) - 2 + 2 = 9 chars.
        assert_eq!(fit_str("ABCDEFGHIJ", 89, TypeRole::Body), "ABCDEFG..");
        // A string that fits is untouched.
        assert_eq!(fit_str("ABC", 89, TypeRole::Body), "ABC");
    }

    #[test]
    fn draw_aligned_right_does_not_bleed() {
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(0);
        let x = 100u16;
        let w = 40u16;
        draw_aligned(
            &mut fb,
            100,
            40,
            w,
            "a much too long string for the box",
            TypeRole::Display,
            0xFFFF,
            Align::Right,
        );
        // Nothing paints left of the box.
        for y in 0..SCREEN_H as usize {
            for col in 0..x as usize {
                assert_eq!(fb.pixels[y * SCREEN_W as usize + col], 0, "bleed at ({col},{y})");
            }
        }
        // Something was painted inside the box, right-aligned to its right edge.
        let painted: Vec<usize> = (0..SCREEN_W as usize)
            .filter(|&cx| (40..55).any(|y| fb.pixels[y * SCREEN_W as usize + cx] != 0))
            .collect();
        let last = *painted.iter().max().expect("painted");
        assert!(last < (x + w) as usize, "right edge {last} past the box");
        assert!(last >= (x + w) as usize - advance(TypeRole::Display) as usize);
    }

    #[test]
    fn draw_is_clipped_at_screen_edge() {
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(0);
        let ink = |fb: &FrameBuffer| fb.pixels.iter().filter(|&&p| p != 0).count();
        // Must not panic; a glyph starting on the last column is clipped to that column.
        draw(&mut fb, SCREEN_W - 1, 10, "XYZ", TypeRole::Display, 0xFFFF);
        let after_edge = ink(&fb);
        // No wrap onto the next row: everything below the 20 px cell stays empty, and nothing
        // is painted left of the glyph start.
        let line_h = cell_height(TypeRole::Display) as usize;
        assert!(fb.pixels[(10 + line_h) * SCREEN_W as usize..].iter().all(|&p| p == 0));
        for y in 0..SCREEN_H as usize {
            for x in 0..(SCREEN_W - 1) as usize {
                assert_eq!(fb.pixels[y * SCREEN_W as usize + x], 0, "bleed at ({x},{y})");
            }
        }
        // Fully off-screen draws paint nothing.
        draw(&mut fb, SCREEN_W, SCREEN_H, "XYZ", TypeRole::Body, 0xFFFF);
        assert_eq!(ink(&fb), after_edge);
    }

    #[test]
    fn wrap_respects_pixels_and_keeps_every_character() {
        let url = "http://192.168.1.42:8787/very/long/path?token=abcdef";
        // 300 px only fits 50 cells at the `Small` advance; the URL is 52 characters.
        let lines = wrap(url, 300, TypeRole::Small);
        assert!(lines.len() > 1, "expected wrap, got {lines:?}");
        assert_eq!(lines.concat(), url);
        for line in &lines {
            assert!(measure(line, TypeRole::Small) <= 300, "{line:?}");
        }
        assert_eq!(wrap("192.168.1.42", 300, TypeRole::Small), vec!["192.168.1.42"]);
        assert_eq!(wrap("", 100, TypeRole::Small), vec![String::new()]);
        // A single character wider than the box still gets its own line (no infinite loop).
        assert_eq!(wrap("abc", 1, TypeRole::Display).len(), 3);
    }
}
