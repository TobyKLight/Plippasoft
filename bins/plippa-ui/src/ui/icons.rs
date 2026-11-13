//! 16×16 monochrome icons, painted 1:1.
//!
//! Each bitmap is `[u16; 16]`, one word per row: bit `col` of word `row` is the pixel at
//! `(col, row)`, least significant bit at the **left**, so `1u16 << 0` is the leftmost pixel.
//! `1` is ink, `0` is transparent (the row background shows through).
//!
//! These are Smallbits icons (see the *Credits* section of the workspace `README.md`). The
//! source SVGs in the internal docs repo (`design/iconsOriginal/smallbits/`) are a 16×16
//! viewBox with every path coordinate on a whole unit, so **16 px is their native size**: one
//! bitmap pixel is one viewBox unit and the tables below are the drawings, not a resample.
//! That is why 16 is the only larger step that looks right — 10 px (1.25 units per pixel) and
//! 12 px (1.5) both fall off the art's 2-unit grid and shear the strokes.
//!
//! The tables are transcribed once so the icons ship as consts: no build step, no decoder, no
//! dependency, and no chance of a runtime asset going missing on the device.
//!
//! [`ICON_W`] × [`ICON_H`] is 16 × 16 px and [`draw_icon`] never scales. That is taller than
//! the ~13 px ink of the `Display` digits they sit beside, so the badges read as status marks
//! at arm's length rather than as a second column of text.

use crate::display::{FrameBuffer, Rgb565};

/// Icon width in pixels.
pub const ICON_W: u16 = 16;
/// Icon height in pixels.
pub const ICON_H: u16 = 16;

/// The icons this build paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// Play order is shuffle: two crossing arrows.
    Shuffle,
    /// Repeat is on: a loop with an arrow head. Repeat-*item* is this icon plus a `1` drawn
    /// next to it by the caller.
    Repeat,
    /// An output is ticked on (AUDIO_04). Drawn here because the ISO-8859-1 font has no tick
    /// character to type.
    Check,
    /// An output is not ticked (AUDIO_04).
    BoxEmpty,
}

impl Icon {
    /// The ink table: 16 rows, row 0 at the top, bit 0 at the left.
    pub const fn rows(self) -> &'static [u16; 16] {
        match self {
            Icon::Shuffle => &SHUFFLE,
            Icon::Repeat => &REPEAT,
            Icon::Check => &CHECK,
            Icon::BoxEmpty => &BOX_EMPTY,
        }
    }
}

/// `design/iconsOriginal/smallbits/shuffle.svg`, at its native 16 px size.
pub const SHUFFLE: [u16; 16] = [
    0b1111_1100_0000_0011,
    0b1111_1100_0000_0011,
    0b1111_0000_0000_1100,
    0b1111_0000_0000_1100,
    0b1100_1100_0011_0000,
    0b1100_1100_0011_0000,
    0b0000_0011_0000_0000,
    0b0000_0011_0000_0000,
    0b0000_0000_1100_0000,
    0b0000_0000_1100_0000,
    0b1100_1100_0011_0000,
    0b1100_1100_0011_0000,
    0b1111_0000_0000_1100,
    0b1111_0000_0000_1100,
    0b1111_1100_0000_0011,
    0b1111_1100_0000_0011,
];

/// `design/iconsOriginal/smallbits/arrow-rotate-ccw-up.svg`, read as the repeat badge.
pub const REPEAT: [u16; 16] = [
    0b1100_0011_1111_0000,
    0b1100_0011_1111_0000,
    0b1100_1100_0000_1100,
    0b1100_1100_0000_1100,
    0b1111_0000_0000_0011,
    0b1111_0000_0000_0011,
    0b1111_1111_0000_0011,
    0b1111_1111_0000_0011,
    0b0000_0000_0000_0011,
    0b0000_0000_0000_0011,
    0b0000_0000_0000_0011,
    0b0000_0000_0000_0011,
    0b0011_0000_0000_1100,
    0b0011_0000_0000_1100,
    0b0000_1111_1111_0000,
    0b0000_1111_1111_0000,
];

/// A tick for a ticked output (AUDIO_04), drawn rather than typed: the ISO-8859-1 font has no
/// tick character.
pub const CHECK: [u16; 16] = [
    0b0000_0000_0000_0000,
    0b0000_0000_0000_0000,
    0b0110_0000_0000_0000,
    0b0111_0000_0000_0000,
    0b0011_1000_0000_0000,
    0b0001_1100_0000_1100,
    0b0000_1110_0011_1000,
    0b0000_0111_0111_0000,
    0b0000_0011_1110_0000,
    0b0000_0001_1100_0000,
    0b0000_0000_1100_0000,
    0b0000_0000_0000_0000,
    0b0000_0000_0000_0000,
    0b0000_0000_0000_0000,
    0b0000_0000_0000_0000,
    0b0000_0000_0000_0000,
];

/// An empty checkbox for an unticked output (AUDIO_04).
pub const BOX_EMPTY: [u16; 16] = [
    0b0000_0000_0000_0000,
    0b0000_0000_0000_0000,
    0b0011_1111_1111_1100,
    0b0011_1111_1111_1100,
    0b0011_0000_0000_1100,
    0b0011_0000_0000_1100,
    0b0011_0000_0000_1100,
    0b0011_0000_0000_1100,
    0b0011_0000_0000_1100,
    0b0011_0000_0000_1100,
    0b0011_0000_0000_1100,
    0b0011_0000_0000_1100,
    0b0011_0000_0000_1100,
    0b0011_0000_0000_1100,
    0b0011_1111_1111_1100,
    0b0011_1111_1111_1100,
];

/// Paint `icon` with its top-left ink pixel at `(x, y)`, in `colour`, 1:1 (no scaling).
///
/// Pixels are clipped by [`FrameBuffer::set_pixel`], so a partly off-screen icon is safe.
pub fn draw_icon(fb: &mut FrameBuffer, x: u16, y: u16, icon: Icon, colour: Rgb565) {
    for (row, ink) in icon.rows().iter().enumerate() {
        for col in 0..ICON_W {
            if ink & (1 << col) != 0 {
                fb.set_pixel(x.saturating_add(col), y.saturating_add(row as u16), colour);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tokens::{ACCENT, SCREEN_W, SURFACE};

    fn frame() -> FrameBuffer {
        let mut f = FrameBuffer::new(SCREEN_W, ICON_H + 4);
        f.clear(SURFACE);
        f
    }

    fn pixel(f: &FrameBuffer, x: u16, y: u16) -> Rgb565 {
        f.pixels[y as usize * f.width as usize + x as usize]
    }

    #[test]
    fn every_icon_has_ink_and_empty_space() {
        for icon in [Icon::Shuffle, Icon::Repeat, Icon::Check, Icon::BoxEmpty] {
            let ink: u32 = icon.rows().iter().map(|r| r.count_ones()).sum();
            let total = u32::from(ICON_W) * u32::from(ICON_H);
            assert!(
                (32..=total * 2 / 3).contains(&ink),
                "{icon:?} has {ink} ink pixels out of {total} — that is not a legible glyph"
            );
            assert!(ink < total, "{icon:?} is filled solid");
        }
    }

    #[test]
    fn every_icon_has_exactly_the_icon_height_rows() {
        for icon in [Icon::Shuffle, Icon::Repeat, Icon::Check, Icon::BoxEmpty] {
            assert_eq!(icon.rows().len(), ICON_H as usize, "{icon:?}");
        }
    }

    #[test]
    fn every_icon_uses_the_full_canvas() {
        // Both drawings touch all four edges of their 16x16 box in the source art; a
        // transcription that drops a row or shifts a column cannot.
        for icon in [Icon::Shuffle, Icon::Repeat] {
            let rows = icon.rows();
            let ink = |r: usize, c: u16| rows[r] & (1 << c) != 0;
            assert!(
                (0..16).any(|c| ink(0, c)) && (0..16).any(|c| ink(15, c)),
                "{icon:?} does not reach the top and bottom rows"
            );
            assert!(
                (0..16).any(|r| ink(r, 0)) && (0..16).any(|r| ink(r, 15)),
                "{icon:?} does not reach the left and right columns"
            );
        }
    }

    #[test]
    fn draw_icon_paints_the_bitmap_one_to_one() {
        let mut f = frame();
        draw_icon(&mut f, 5, 2, Icon::Repeat, ACCENT);
        for row in 0..ICON_H {
            for col in 0..ICON_W {
                let want = if REPEAT[row as usize] & (1 << col) != 0 {
                    ACCENT
                } else {
                    SURFACE
                };
                assert_eq!(
                    pixel(&f, 5 + col, 2 + row),
                    want,
                    "pixel ({col}, {row}) is not the bitmap's"
                );
            }
        }
        // Nothing outside the 16x16 box.
        for x in 0..SCREEN_W {
            for y in 0..ICON_H + 4 {
                if !((5..5 + ICON_W).contains(&x) && (2..2 + ICON_H).contains(&y)) {
                    assert_eq!(
                        pixel(&f, x, y),
                        SURFACE,
                        "ink at ({x}, {y}) outside the icon"
                    );
                }
            }
        }
    }

    #[test]
    fn the_two_icons_are_different_shapes() {
        assert_ne!(SHUFFLE, REPEAT, "the two icons must not be the same bitmap");
    }
}
