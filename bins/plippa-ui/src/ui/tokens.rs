//! Design tokens: the **only** place in the crate where a colour literal lives, plus the
//! spacing scale and the screen geometry every screen shares.
//!
//! Screens must not invent colours or arbitrary gaps. If something cannot be expressed with
//! these tokens, add a token here (and say why in the module docs) instead of a literal at a
//! call site.

use crate::display::{rgb565, Rgb565};

// ---------------------------------------------------------------------------------------------
// Surface + text
// ---------------------------------------------------------------------------------------------

/// App ground.
pub const BG: Rgb565 = rgb565(12, 16, 32);
/// Slightly lifted ground (footer band, placeholder art).
pub const BG_ALT: Rgb565 = rgb565(16, 21, 40);
/// Header band, raised row, art box.
pub const SURFACE: Rgb565 = rgb565(26, 33, 52);
/// Alternating list row.
pub const SURFACE_ALT: Rgb565 = rgb565(21, 27, 44);
/// Focus ring, box outline.
pub const BORDER: Rgb565 = rgb565(64, 78, 102);
/// Hairlines, bar track.
pub const BORDER_SOFT: Rgb565 = rgb565(38, 47, 66);
/// Primary text.
pub const FG: Rgb565 = rgb565(235, 240, 248);
/// Secondary text (artist, hints).
pub const FG_DIM: Rgb565 = rgb565(150, 165, 185);
/// Hints, disabled rows.
pub const FG_MUTED: Rgb565 = rgb565(96, 110, 130);
/// Text on an accent fill.
pub const FG_INVERSE: Rgb565 = rgb565(10, 14, 24);

// ---------------------------------------------------------------------------------------------
// Accents
// ---------------------------------------------------------------------------------------------

/// Packed 5/6/5 channel mix: `pct` of `b`, the rest `a` (0 → a, 100 → b, clamped).
///
/// Integer arithmetic on the packed channels with rounding (`(a*(100-p) + b*p + 50)/100`),
/// so both endpoints are bit-exact and a percentage is independent of the palette.
pub const fn mix(a: Rgb565, b: Rgb565, pct: u8) -> Rgb565 {
    let p = if pct > 100 { 100 } else { pct } as u32;
    let q = 100 - p;
    let a = a as u32;
    let b = b as u32;
    let r = ((((a >> 11) & 0x1f) * q + ((b >> 11) & 0x1f) * p + 50) / 100) as u16;
    let g = ((((a >> 5) & 0x3f) * q + ((b >> 5) & 0x3f) * p + 50) / 100) as u16;
    let bl = (((a & 0x1f) * q + (b & 0x1f) * p + 50) / 100) as u16;
    (r << 11) | (g << 5) | bl
}

/// Brand blue. Kept at its pre-design-system value so LED roles and existing expectations do
/// not drift.
pub const ACCENT: Rgb565 = rgb565(80, 180, 255);
/// Played portion of a bar, header rule.
pub const ACCENT_DIM: Rgb565 = rgb565(40, 95, 140);
/// Played bars on the Now Playing waveform: half of the old dimming (`ACCENT_DIM` was twice
/// as dark as this). [`mix`] keeps the halfway point in the palette rather than at a call site.
pub const ACCENT_PLAYED: Rgb565 = mix(ACCENT_DIM, ACCENT, 50);
/// Progress fill / teal.
pub const ACCENT_2: Rgb565 = rgb565(90, 215, 200);
/// Success.
pub const OK: Rgb565 = rgb565(120, 220, 140);
/// Amber: jog target, set speed, warnings.
pub const WARN: Rgb565 = rgb565(255, 180, 40);
/// Errors.
pub const ERR: Rgb565 = rgb565(255, 90, 90);

// ---------------------------------------------------------------------------------------------
// Spacing
// ---------------------------------------------------------------------------------------------

/// 4 px.
pub const SPACE_1: u16 = 4;
/// 8 px.
pub const SPACE_2: u16 = 8;
/// 12 px.
pub const SPACE_3: u16 = 12;
/// 16 px.
pub const SPACE_4: u16 = 16;
/// 24 px.
pub const SPACE_6: u16 = 24;
/// Left/right screen margin.
pub const MARGIN_X: u16 = SPACE_2;

// ---------------------------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------------------------

/// Logical screen width (poc2 landscape).
pub const SCREEN_W: u16 = 320;
/// Logical screen height (poc2 landscape).
pub const SCREEN_H: u16 = 240;
/// Title bar height.
pub const HEADER_H: u16 = 30;
/// Header rule thickness.
pub const RULE_H: u16 = 1;
/// Footer band height.
pub const FOOTER_H: u16 = 22;
/// First usable body pixel below the header band and its rule.
pub const CONTENT_TOP: u16 = HEADER_H + RULE_H + SPACE_1;
/// Last usable body pixel (the footer band starts here).
pub const CONTENT_BOTTOM: u16 = SCREEN_H - FOOTER_H;
/// Standard list row: `Body` (15 px) plus a 3 px gap.
pub const ROW_H: u16 = 18;
/// Dense list row: `Small` (13 px) plus a 3 px gap.
pub const ROW_H_DENSE: u16 = 16;

// ---------------------------------------------------------------------------------------------
// Now Playing geometry (UI_13, two-row header in UI_14)
//
// `HEADER_H`, `FOOTER_H`, `CONTENT_TOP` and `CONTENT_BOTTOM` above stay the shell for every
// other route. Now Playing has its own, taller chrome: a two-row header and a one-line
// `Display` footer. Every constant below is derived from those tokens, never from a screen row.
// ---------------------------------------------------------------------------------------------

/// `Display` row: one 20 px cell plus a 5 px inset above and below. One token for the
/// 30 px bands (Now Playing header rows, Now Playing footer, Adjust band, status popup) so
/// they cannot drift apart.
pub const ROW_H_DISPLAY: u16 = 30;
/// Now Playing header: two `Display` rows — title / artist, then release / count.
pub const NP_HEADER_H: u16 = 2 * ROW_H_DISPLAY;
/// Now Playing footer band: one `Display` row for position / length / speed.
pub const NP_FOOTER_H: u16 = ROW_H_DISPLAY;
/// First usable Now Playing pixel below its header band and rule.
pub const NP_CONTENT_TOP: u16 = NP_HEADER_H + RULE_H + SPACE_1;
/// Last usable Now Playing pixel (its footer band starts here).
pub const NP_CONTENT_BOTTOM: u16 = SCREEN_H - NP_FOOTER_H;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_endpoints_and_clamp() {
        let a = rgb565(10, 20, 30);
        let b = rgb565(200, 150, 100);
        assert_eq!(mix(a, b, 0), a, "0 % is exactly a");
        assert_eq!(mix(a, b, 100), b, "100 % is exactly b");
        assert_eq!(mix(a, b, 255), b, "> 100 clamps to b");
        // Channels move independently: red up, green down, blue up.
        let mid = mix(a, b, 50);
        let r = |c: Rgb565| (c >> 11) & 0x1f;
        let g = |c: Rgb565| (c >> 5) & 0x3f;
        let bl = |c: Rgb565| c & 0x1f;
        assert!(r(mid) > r(a) && r(mid) < r(b));
        assert!(g(mid) > g(a) && g(mid) < g(b));
        assert!(bl(mid) > bl(a) && bl(mid) < bl(b));
        // `mix` is the documented integer formula with rounding.
        assert_eq!(
            mid,
            ((((r(a) * 50 + r(b) * 50 + 50) / 100) as u16) << 11)
                | ((((g(a) * 50 + g(b) * 50 + 50) / 100) as u16) << 5)
                | (((bl(a) * 50 + bl(b) * 50 + 50) / 100) as u16)
        );
    }

    #[test]
    fn accent_played_is_halfway() {
        assert_eq!(ACCENT_PLAYED, rgb565(64, 136, 192));
        let r = |c: Rgb565| (c >> 11) & 0x1f;
        let g = |c: Rgb565| (c >> 5) & 0x3f;
        let bl = |c: Rgb565| c & 0x1f;
        for (dim, played, accent) in [
            (r(ACCENT_DIM), r(ACCENT_PLAYED), r(ACCENT)),
            (g(ACCENT_DIM), g(ACCENT_PLAYED), g(ACCENT)),
            (bl(ACCENT_DIM), bl(ACCENT_PLAYED), bl(ACCENT)),
        ] {
            assert!(played > dim && played < accent, "strictly between on every channel");
        }
        assert_ne!(ACCENT_PLAYED, ACCENT_DIM);
    }

    #[test]
    fn now_playing_geometry_is_two_display_rows() {
        // UI_14: the header is exactly two 30 px `Display` rows; the waveform recovers 30 px.
        assert_eq!(NP_HEADER_H, 2 * ROW_H_DISPLAY);
        assert_eq!(NP_HEADER_H, 60);
        assert_eq!(NP_CONTENT_TOP, 65);
        assert_eq!(NP_CONTENT_BOTTOM, 210);
        // Every other route's shell is untouched.
        assert_eq!((HEADER_H, FOOTER_H, CONTENT_TOP, CONTENT_BOTTOM), (30, 22, 35, 218));
    }
}
