//! Solid-geometry primitives.
//!
//! Every helper clamps to the framebuffer (`FrameBuffer::set_pixel` / `fill_rect` ignore
//! out-of-range writes), so a widget that is partially off-screen never panics. Screens use
//! these instead of raw coordinate + colour arithmetic; `embedded-graphics` shapes are not
//! needed here because the path has no anti-aliasing and no alpha blending.

use crate::display::{FrameBuffer, Rgb565};

/// Filled rectangle.
pub fn fill(fb: &mut FrameBuffer, x: u16, y: u16, w: u16, h: u16, color: Rgb565) {
    if w == 0 || h == 0 {
        return;
    }
    fb.fill_rect(x, y, w, h, color);
}

/// Outline of `thickness` pixels drawn just inside the rectangle.
pub fn border(
    fb: &mut FrameBuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    thickness: u16,
    color: Rgb565,
) {
    if w == 0 || h == 0 || thickness == 0 {
        return;
    }
    let t = thickness.min(w).min(h);
    fill(fb, x, y, w, t, color);
    fill(fb, x, y.saturating_add(h - t), w, t, color);
    fill(fb, x, y, t, h, color);
    fill(fb, x.saturating_add(w - t), y, t, h, color);
}

/// One-pixel horizontal rule.
pub fn hairline(fb: &mut FrameBuffer, x: u16, y: u16, w: u16, color: Rgb565) {
    fill(fb, x, y, w, 1, color);
}

/// One-pixel vertical rule.
pub fn vline(fb: &mut FrameBuffer, x: u16, y: u16, h: u16, color: Rgb565) {
    fill(fb, x, y, 1, h, color);
}

/// Filled right-pointing triangle: `size` pixels wide, vertically centred on `cy`.
pub fn triangle_right(fb: &mut FrameBuffer, x: u16, cy: u16, size: u16, color: Rgb565) {
    for col in 0..size {
        let half = size.saturating_sub(1).saturating_sub(col) / 2;
        fill(
            fb,
            x.saturating_add(col),
            cy.saturating_sub(half),
            1,
            half.saturating_mul(2).saturating_add(1),
            color,
        );
    }
}

/// `>` chevron: arm length `size`, centred on `cy`, starting at `x`.
pub fn chevron_right(fb: &mut FrameBuffer, x: u16, cy: u16, size: u16, color: Rgb565) {
    for k in 0..=size {
        let dy = size.saturating_sub(k);
        fb.set_pixel(x.saturating_add(k), cy.saturating_sub(dy), color);
        fb.set_pixel(x.saturating_add(k), cy.saturating_add(dy), color);
    }
}

/// `<` chevron: arm length `size`, centred on `cy`, starting at `x`.
pub fn chevron_left(fb: &mut FrameBuffer, x: u16, cy: u16, size: u16, color: Rgb565) {
    for k in 0..=size {
        let dy = size.saturating_sub(k);
        fb.set_pixel(x.saturating_add(size.saturating_sub(k)), cy.saturating_sub(dy), color);
        fb.set_pixel(x.saturating_add(size.saturating_sub(k)), cy.saturating_add(dy), color);
    }
}

/// Filled downward triangle with its `2*half + 1` px base on row `y`, apex `half` rows below.
pub fn triangle_down(fb: &mut FrameBuffer, cx: u16, y: u16, half: u16, color: Rgb565) {
    for row in 0..=half {
        let inset = half - row;
        fill(
            fb,
            cx.saturating_sub(inset),
            y.saturating_add(row),
            inset.saturating_mul(2).saturating_add(1),
            1,
            color,
        );
    }
}

/// Blend every pixel of `x..x+w` × `y..y+h` `percent` of the way toward `colour`.
///
/// `0` leaves the pixels untouched, `100` replaces them with `colour`; the blend is the plain
/// per-channel interpolation `old * (100 - percent) / 100 + colour * percent / 100` on the
/// packed 5/6/5 components, rounded to nearest. It is pure integer maths, so the result is
/// deterministic on every target and needs no alpha channel in the framebuffer.
///
/// The design system stays solid-fill by default (`UI_10`: no alpha blending). This helper is
/// an **owner-approved, deliberately scoped exception** used only for the Now Playing
/// Adjust-mode band (UI_12), where the waveform must remain 40 % visible through the row.
pub fn blend_toward(
    fb: &mut FrameBuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    colour: Rgb565,
    percent: u8,
) {
    let p = u32::from(percent.min(100));
    if p == 0 || w == 0 || h == 0 {
        return;
    }
    if p == 100 {
        fill(fb, x, y, w, h, colour);
        return;
    }
    let x1 = x.saturating_add(w).min(fb.width);
    let y1 = y.saturating_add(h).min(fb.height);
    for yy in y..y1 {
        let row = yy as usize * fb.width as usize;
        for xx in x..x1 {
            let i = row + xx as usize;
            fb.pixels[i] = blend_pixel(fb.pixels[i], colour, percent);
        }
    }
}

/// One pixel of [`blend_toward`]: `old` moved `percent` of the way toward `new`, per packed
/// 5/6/5 channel, rounded to nearest. `0` returns `old`, `100` returns `new`. The same scoped
/// exception to the "no alpha" rule applies; the Now Playing cover fade (ART_08) uses it.
pub fn blend_pixel(old: Rgb565, new: Rgb565, percent: u8) -> Rgb565 {
    let p = u32::from(percent.min(100));
    let keep = 100 - p;
    let mix = |shift: u32, mask: u16| -> u16 {
        let a = u32::from((old >> shift) & mask);
        let b = u32::from((new >> shift) & mask);
        (((a * keep + b * p + 50) / 100) as u16) << shift
    };
    mix(11, 0x1f) | mix(5, 0x3f) | mix(0, 0x1f)
}

/// 2 px scrollbar for lists that overflow: `first` is the first visible row.
///
/// Paints nothing when every row is visible (no dead scrollbar on short lists).
pub fn scrollbar(
    fb: &mut FrameBuffer,
    x: u16,
    y: u16,
    h: u16,
    first: usize,
    visible: usize,
    total: usize,
) {
    if total <= visible || visible == 0 || h == 0 {
        return;
    }
    let track = crate::ui::tokens::BORDER_SOFT;
    let thumb_color = crate::ui::tokens::BORDER;
    fill(fb, x, y, 2, h, track);
    let thumb_h = ((h as usize * visible) / total).max(4) as u16;
    let span = h.saturating_sub(thumb_h);
    let max_first = (total - visible).max(1);
    let offset = (span as usize * first.min(max_first) / max_first) as u16;
    fill(fb, x, y.saturating_add(offset), 2, thumb_h, thumb_color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tokens::{BG, FG};

    fn frame() -> FrameBuffer {
        let mut fb = FrameBuffer::new(8, 4);
        fb.clear(BG);
        fb
    }

    #[test]
    fn blend_endpoints_are_exact_and_clamped() {
        let mut fb = frame();
        fb.set_pixel(1, 1, FG);
        let before = fb.pixels.clone();
        // 0 % is a no-op; out-of-range percentages clamp to 100 %.
        blend_toward(&mut fb, 0, 0, 8, 4, BG, 0);
        assert_eq!(fb.pixels, before);
        blend_toward(&mut fb, 1, 1, 1, 1, BG, 200);
        assert_eq!(fb.pixels[1 * 8 + 1], BG);
        // A blend toward the pixel's own colour leaves it unchanged (rounding is exact here).
        fb.set_pixel(2, 2, FG);
        blend_toward(&mut fb, 2, 2, 1, 1, FG, 60);
        assert_eq!(fb.pixels[2 * 8 + 2], FG);
    }

    #[test]
    fn blend_60_keeps_40_percent_of_the_underlying_pixel() {
        let mut fb = frame();
        // White ground blended 60 % toward black-ish `BG`: the result is nearer BG than white
        // by component, and strictly between the two.
        fb.clear(0xFFFF);
        blend_toward(&mut fb, 0, 0, 8, 4, BG, 60);
        for &p in &fb.pixels {
            assert_ne!(p, 0xFFFF, "white survived the blend");
            assert_ne!(p, BG, "the blend did not move the pixel");
            let r = p >> 11;
            let bg_r = BG >> 11;
            assert!(r < 0x1f && r >= bg_r, "red channel {r} outside the blend range");
        }
        // 40 % of white must remain: the red channel is ~40 % of full scale, not near zero.
        let r = fb.pixels[0] >> 11;
        assert!((12..=14).contains(&r), "red channel {r} is not ~40 %");
    }

    #[test]
    fn blend_is_monotonic_and_local_to_the_rect() {
        let mut fb = frame();
        fb.clear(0xFFFF);
        let untouched = fb.pixels[3 * 8 + 7];
        blend_toward(&mut fb, 1, 1, 2, 1, BG, 30);
        let light = fb.pixels[1 * 8 + 1];
        blend_toward(&mut fb, 1, 1, 2, 1, BG, 30);
        let darker = fb.pixels[1 * 8 + 1];
        assert_ne!(light, darker, "a second blend keeps approaching the target");
        assert_eq!(fb.pixels[3 * 8 + 7], untouched, "pixels outside the rect changed");
        assert_eq!(fb.pixels[0], 0xFFFF, "pixels left of the rect changed");
    }
}
