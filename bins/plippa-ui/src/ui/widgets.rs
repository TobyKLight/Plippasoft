//! Widgets: the vocabulary screens paint with.
//!
//! None of these read app state; they take borrowed text and plain values. Every widget paints
//! its own background first, so it is always safe to overdraw, and every text field is clipped
//! with [`crate::ui::text::draw_clipped`] so nothing can escape its box.
//!
//! Rules (see [`super`] for the full design contract):
//!
//! - A focused row is shown by a **2 px `ACCENT` ring inside the row** — never by recolouring
//!   the label and never by a `>` prefix. The playing marker stays a filled `triangle_right`
//!   in `ACCENT_2`.
//! - Alternating rows use `SURFACE_ALT`; the focused row paints `SURFACE` and then the ring.
//!   A row is never inverted to `ACCENT` (it reads as an error state on this panel).

use crate::display::{FrameBuffer, Rgb565};
use crate::ui::icons::{self, Icon, ICON_H, ICON_W};
use crate::ui::primitives::{border, fill, hairline, triangle_right};
use crate::ui::text::{self, advance, cell_height, measure, Align, TypeRole};
use crate::ui::tokens::*;

/// Title bar: band, `FG` title, optional right-aligned meta and the 1 px accent rule.
///
/// The title is always `FG` (§3); colour is not a focus cue for shell chrome.
pub struct Header<'a> {
    pub title: &'a str,
    /// Right-aligned secondary text (`3/12`, clock, …). `None` leaves the slot empty.
    pub meta: Option<&'a str>,
}

/// Title bar: band, `FG` title, optional right-aligned meta and the 1 px accent rule.
pub fn header(fb: &mut FrameBuffer, h: Header<'_>) {
    fill(fb, 0, 0, fb.width, HEADER_H, SURFACE);
    let meta = h.meta.unwrap_or("");
    let meta_px = if meta.is_empty() {
        0
    } else {
        measure(meta, TypeRole::Small).saturating_add(SPACE_2)
    };
    let title_w = fb
        .width
        .saturating_sub(MARGIN_X.saturating_mul(2))
        .saturating_sub(meta_px);
    let title_y = (HEADER_H.saturating_sub(cell_height(TypeRole::Display))) / 2;
    text::draw_clipped(
        fb,
        MARGIN_X,
        title_y,
        title_w,
        h.title,
        TypeRole::Display,
        FG,
    );
    if !meta.is_empty() {
        let meta_y = (HEADER_H.saturating_sub(cell_height(TypeRole::Small))) / 2;
        text::draw_aligned(
            fb,
            MARGIN_X,
            meta_y,
            fb.width.saturating_sub(MARGIN_X.saturating_mul(2)),
            meta,
            TypeRole::Small,
            FG_DIM,
            Align::Right,
        );
    }
    hairline(fb, 0, HEADER_H, fb.width, ACCENT_DIM);
}

/// Status band: three dim `Small` slots along the bottom [`FOOTER_H`] pixels.
pub struct Footer<'a> {
    pub left: &'a str,
    pub centre: &'a str,
    pub right: &'a str,
}

pub fn footer(fb: &mut FrameBuffer, f: Footer<'_>) {
    let y = fb.height.saturating_sub(FOOTER_H);
    fill(fb, 0, y, fb.width, FOOTER_H, BG_ALT);
    hairline(fb, 0, y, fb.width, BORDER_SOFT);
    let ty = y.saturating_add((FOOTER_H.saturating_sub(cell_height(TypeRole::Small))) / 2);
    let w = fb.width.saturating_sub(MARGIN_X.saturating_mul(2));
    text::draw_clipped(fb, MARGIN_X, ty, w, f.left, TypeRole::Small, FG_DIM);
    text::draw_aligned(
        fb,
        MARGIN_X,
        ty,
        w,
        f.centre,
        TypeRole::Small,
        FG_DIM,
        Align::Centre,
    );
    text::draw_aligned(
        fb,
        MARGIN_X,
        ty,
        w,
        f.right,
        TypeRole::Small,
        FG_DIM,
        Align::Right,
    );
}

/// Now Playing header: two `Display` rows in a [`NP_HEADER_H`] band plus the accent rule.
///
/// Row 0 is the track title (`ACCENT`, the only highlighted text) on the left and the artist
/// (`FG_DIM`) on the right. Row 1 is the release title (`FG_DIM`) on the left and, on the
/// right, the play-mode badges and then the track count (`FG_DIM`). The artist owns its pixels
/// first (row 0); the count and its badges own theirs first (row 1), so neither can be pushed
/// off the panel by a long text.
pub struct NpHeader<'a> {
    /// Row 0 left — `ACCENT`, ellipsised to leave room for the artist.
    pub title: &'a str,
    /// Row 0 right — `FG_DIM`, always fully visible.
    pub artist: &'a str,
    /// Row 1 left — `FG_DIM`, ellipsised to leave room for the badges and the count.
    pub release: &'a str,
    /// Row 1 right — `FG_DIM`, measured first and drawn last.
    pub count: &'a str,
    /// Row 1, immediately left of the count: the active play-mode badges.
    pub modes: NpModeBadges,
}

/// Play-mode badges for [`NpHeader`]: what the *engine* says is on, left of the track count.
///
/// Plain booleans rather than `plippa_core` enums, so the widget layer stays free of domain
/// types and cannot disagree with the status it was handed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NpModeBadges {
    /// `OrderMode::Shuffle`: the shuffle icon.
    pub shuffle: bool,
    /// `RepeatMode::Queue` or `RepeatMode::Item`: the repeat icon.
    pub repeat: bool,
    /// `RepeatMode::Item`: a `1` right of the repeat icon.
    pub repeat_one: bool,
}

impl NpModeBadges {
    /// No mode active: the header is pixel-identical to the pre-badge layout.
    pub const NONE: Self = Self {
        shuffle: false,
        repeat: false,
        repeat_one: false,
    };

    /// True when at least one badge is painted.
    pub const fn any(self) -> bool {
        self.shuffle || self.repeat
    }

    /// Gap between the repeat icon and its `1` (the digit's cell has its own side bearing, so
    /// 2 px of cell is about 5 px of visible space).
    const NUMBER_GAP: u16 = 2;

    /// Width of the badge group in pixels, `0` when nothing is shown. Does not include the gap
    /// to the count; [`np_header`] adds `SPACE_2` when it reserves room.
    pub fn width(self) -> u16 {
        let number = if self.repeat_one {
            Self::NUMBER_GAP.saturating_add(advance(TypeRole::Display))
        } else {
            0
        };
        let repeat = if self.repeat {
            icons::ICON_W.saturating_add(number)
        } else {
            0
        };
        let shuffle = if self.shuffle { icons::ICON_W } else { 0 };
        let pair_gap = if self.shuffle && self.repeat {
            SPACE_1
        } else {
            0
        };
        shuffle.saturating_add(pair_gap).saturating_add(repeat)
    }
}

/// Paint [`NpHeader`]. Now Playing does not use [`header`], which stays the 30 px shell.
pub fn np_header(fb: &mut FrameBuffer, h: NpHeader<'_>) {
    fill(fb, 0, 0, fb.width, NP_HEADER_H, SURFACE);
    let line_h = cell_height(TypeRole::Display);
    // Each `Display` row is a `ROW_H_DISPLAY` band with the 20 px cell centred in it.
    let inset = ROW_H_DISPLAY.saturating_sub(line_h) / 2;
    let y0 = inset;
    let y1 = ROW_H_DISPLAY.saturating_add(inset);
    let w = fb.width.saturating_sub(MARGIN_X.saturating_mul(2));

    // Row 0: the artist has priority. Measure it first, reserve its pixels plus a gap, then
    // clip the title into whatever is left. An empty artist takes no gap.
    let artist_px = if h.artist.is_empty() {
        0
    } else {
        measure(h.artist, TypeRole::Display)
            .saturating_add(SPACE_2)
            .min(w)
    };
    text::draw_clipped(
        fb,
        MARGIN_X,
        y0,
        w.saturating_sub(artist_px),
        h.title,
        TypeRole::Display,
        ACCENT,
    );
    if !h.artist.is_empty() {
        text::draw_aligned(
            fb,
            MARGIN_X,
            y0,
            w,
            h.artist,
            TypeRole::Display,
            FG_DIM,
            Align::Right,
        );
    }

    // Row 1 anchors, right to left: the count is right-aligned to the margin, the badge group
    // sits `SPACE_2` left of it, and the release title must stop `SPACE_2` left of the badges.
    // Deriving all three from the same anchors is what guarantees the long release is cut
    // *before* the icons rather than drawn under them.
    let right = MARGIN_X.saturating_add(w);
    let count_w = measure(h.count, TypeRole::Display);
    let count_left = right.saturating_sub(count_w);
    let count_gap = if h.count.is_empty() { 0 } else { SPACE_2 };
    let badge_right = count_left.saturating_sub(count_gap);
    let badge_left = badge_right.saturating_sub(h.modes.width());
    let release_w = if h.modes.any() {
        badge_left.saturating_sub(MARGIN_X).saturating_sub(SPACE_2)
    } else {
        // No badges: the release keeps the whole width left of the count (the pre-badge
        // layout, pixel for pixel).
        badge_right.saturating_sub(MARGIN_X)
    };
    text::draw_clipped(
        fb,
        MARGIN_X,
        y1,
        release_w,
        h.release,
        TypeRole::Display,
        FG_DIM,
    );
    if h.modes.any() {
        // Vertical: centre the 8 px icon on the 20 px text cell so the two share a visual band
        // (the font's own side bearings then govern the exact pixel, as they do for the row).
        let badge_y = y1.saturating_add((line_h.saturating_sub(icons::ICON_H)) / 2);
        let mut x = badge_left;
        if h.modes.shuffle {
            icons::draw_icon(fb, x, badge_y, icons::Icon::Shuffle, ACCENT);
            x = x.saturating_add(icons::ICON_W);
            if h.modes.repeat {
                x = x.saturating_add(SPACE_1);
            }
        }
        if h.modes.repeat {
            icons::draw_icon(fb, x, badge_y, icons::Icon::Repeat, ACCENT);
            if h.modes.repeat_one {
                // `Repeat one`: the `Display` `1` keeps the count's baseline, so the pair reads
                // as one badge rather than a digit floating beside an icon.
                text::draw(
                    fb,
                    x.saturating_add(icons::ICON_W)
                        .saturating_add(NpModeBadges::NUMBER_GAP),
                    y1,
                    "1",
                    TypeRole::Display,
                    ACCENT,
                );
            }
        }
    }
    if !h.count.is_empty() {
        text::draw_aligned(
            fb,
            MARGIN_X,
            y1,
            w,
            h.count,
            TypeRole::Display,
            FG_DIM,
            Align::Right,
        );
    }
    hairline(fb, 0, NP_HEADER_H, fb.width, ACCENT_DIM);
}

/// Now Playing footer: one `Display` row inside [`NP_FOOTER_H`] pixels.
///
/// Left is the position / total length (`FG`), centre the analysis state (`FG_DIM`) and right
/// the set speed (`ACCENT`). The caller keeps the centre and right slots mutually exclusive;
/// the widget additionally skips the centre when the measured clock and speed leave it no room.
pub struct NpFooter<'a> {
    pub left: &'a str,
    pub centre: &'a str,
    pub right: &'a str,
}

/// Paint [`NpFooter`] into the bottom [`NP_FOOTER_H`] pixels.
pub fn np_footer(fb: &mut FrameBuffer, f: NpFooter<'_>) {
    let y = fb.height.saturating_sub(NP_FOOTER_H);
    fill(fb, 0, y, fb.width, NP_FOOTER_H, BG_ALT);
    hairline(fb, 0, y, fb.width, BORDER_SOFT);
    let ty = y.saturating_add(
        NP_FOOTER_H.saturating_sub(cell_height(TypeRole::Display)) / 2,
    );
    let w = fb.width.saturating_sub(MARGIN_X.saturating_mul(2));
    // Both side slots are measured first: the centre only gets the box they leave over. This
    // mirrors the caller's "speed wins over ANALYSING" rule — the clock wins the pixels it
    // actually uses — and is why a long `12:34 / 45:06` can never be overpainted.
    let left_w = measure(f.left, TypeRole::Display).min(w);
    let right_w = measure(f.right, TypeRole::Display).min(w);
    text::draw_clipped(fb, MARGIN_X, ty, w, f.left, TypeRole::Display, FG);
    let centre_x = MARGIN_X
        .saturating_add(left_w)
        .saturating_add(SPACE_2);
    let centre_end = MARGIN_X
        .saturating_add(w)
        .saturating_sub(right_w)
        .saturating_sub(SPACE_2);
    let centre_w = centre_end.saturating_sub(centre_x);
    if measure(f.centre, TypeRole::Display) <= centre_w {
        text::draw_aligned(
            fb,
            centre_x,
            ty,
            centre_w,
            f.centre,
            TypeRole::Display,
            FG_DIM,
            Align::Centre,
        );
    }
    text::draw_aligned(
        fb,
        MARGIN_X,
        ty,
        w,
        f.right,
        TypeRole::Display,
        ACCENT,
        Align::Right,
    );
}

/// Visual state of one [`row`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    Normal,
    /// 2 px `ACCENT` ring inside the row; the label colour is unchanged.
    Focused,
    /// Focused **and** being edited: the same `ACCENT` ring, plus the value column in
    /// `ACCENT` instead of `FG_DIM` (UI_16's "enter, then scroll" settings rows). The label
    /// stays `FG`, so the accent is reserved for the value under edit.
    Editing,
    /// `FG_MUTED` label (unavailable / disabled).
    Disabled,
    /// `WARN` label (missing media, needs attention).
    Warning,
}

/// One list row: label left, optional value right, optional playing marker.
pub struct Row<'a> {
    pub label: &'a str,
    pub value: Option<&'a str>,
    /// Extra left inset in pixels (cue rows, hierarchy levels).
    pub indent: u8,
    pub state: RowState,
    /// `SURFACE_ALT` background (use for odd rows).
    pub striped: bool,
    /// Playing marker: a filled `ACCENT_2` triangle left of the label.
    pub marker: bool,
    /// Row height: [`ROW_H`] normally, [`ROW_H_DENSE`] in a dense list.
    pub height: u16,
}

pub fn row(fb: &mut FrameBuffer, y: u16, r: Row<'_>) {
    let h = r.height.max(1);
    let bg = if r.striped { SURFACE_ALT } else { BG };
    fill(fb, 0, y, fb.width, h, bg);
    if matches!(r.state, RowState::Focused | RowState::Editing) {
        fill(fb, 0, y, fb.width, h, SURFACE);
        border(
            fb,
            SPACE_1,
            y,
            fb.width.saturating_sub(SPACE_1.saturating_mul(2)),
            h,
            2,
            ACCENT,
        );
    }
    let color = match r.state {
        RowState::Disabled => FG_MUTED,
        RowState::Warning => WARN,
        RowState::Normal | RowState::Focused | RowState::Editing => FG,
    };
    let mut x = MARGIN_X.saturating_add(u16::from(r.indent));
    if r.marker {
        // 8 px wide, vertically centred; the label clears it by SPACE_2.
        triangle_right(fb, MARGIN_X, y.saturating_add(h / 2), 8, ACCENT_2);
        x = MARGIN_X.saturating_add(SPACE_2).saturating_add(u16::from(r.indent));
    }
    let value_px = r
        .value
        .map_or(0, |v| measure(v, TypeRole::Small).saturating_add(SPACE_2));
    let label_w = fb
        .width
        .saturating_sub(x)
        .saturating_sub(MARGIN_X)
        .saturating_sub(value_px);
    text::draw_clipped(
        fb,
        x,
        y.saturating_add((h.saturating_sub(cell_height(TypeRole::Body))) / 2),
        label_w,
        r.label,
        TypeRole::Body,
        color,
    );
    if let Some(value) = r.value {
        text::draw_aligned(
            fb,
            MARGIN_X,
            y.saturating_add((h.saturating_sub(cell_height(TypeRole::Small))) / 2),
            fb.width.saturating_sub(MARGIN_X.saturating_mul(2)),
            value,
            TypeRole::Small,
            FG_DIM,
            Align::Right,
        );
    }
}

/// Windowed single-column list.
pub struct List<'a> {
    pub rows: &'a [String],
    /// Cursor row (ring only when [`List::show_focus`]).
    pub focus: usize,
    /// First pixel row.
    pub top: u16,
    /// Dense rows ([`ROW_H_DENSE`]) instead of [`ROW_H`].
    pub dense: bool,
    /// Rows that render disabled (unavailable media, muted: [`FG_MUTED`]).
    pub disabled: Option<&'a dyn Fn(usize) -> bool>,
    /// Rows that render as a warning (missing media in a browse list, amber: [`WARN`]).
    /// Checked after `disabled`.
    pub warning: Option<&'a dyn Fn(usize) -> bool>,
    /// Absolute row index that carries the playing marker.
    pub marker: Option<usize>,
    /// Draw the focus ring (false while the list is only being displayed, not browsed).
    pub show_focus: bool,
    /// Per-row left inset in pixels (cue rows, hierarchy levels). `None` = flush at the margin.
    pub indent: Option<&'a dyn Fn(usize) -> u8>,
    /// Force the first visible row (e.g. the playing row on top); `None` follows the focus.
    pub window_start: Option<usize>,
}

/// Paint the visible window of `l`; returns `(first visible row, visible rows)`.
///
/// The window is bounded by [`CONTENT_BOTTOM`], so a list can never paint into the footer.
pub fn list(fb: &mut FrameBuffer, l: List<'_>) -> (usize, usize) {
    let row_h = if l.dense { ROW_H_DENSE } else { ROW_H };
    let len = l.rows.len();
    if len == 0 {
        return (0, 0);
    }
    let bottom = CONTENT_BOTTOM.min(fb.height);
    if bottom <= l.top {
        return (0, 0);
    }
    let fit = bottom.saturating_sub(l.top) / row_h;
    let visible = (fit.max(1) as usize).min(len);
    let focus = l.focus.min(len - 1);
    let start = match l.window_start {
        Some(start) => start.min(len - visible),
        None if focus + 1 > visible => (focus + 1 - visible).min(len - visible),
        None => 0,
    };
    for i in 0..visible {
        let abs = start + i;
        let state = if l.show_focus && abs == focus {
            RowState::Focused
        } else if l.disabled.is_some_and(|d| d(abs)) {
            RowState::Disabled
        } else if l.warning.is_some_and(|w| w(abs)) {
            RowState::Warning
        } else {
            RowState::Normal
        };
        row(
            fb,
            l.top.saturating_add((i as u16).saturating_mul(row_h)),
            Row {
                label: &l.rows[abs],
                value: None,
                indent: l.indent.map_or(0, |indent| indent(abs)),
                state,
                striped: abs % 2 == 1,
                marker: l.marker == Some(abs),
                height: row_h,
            },
        );
    }
    (start, visible)
}

/// Small chip with a filled background; returns the painted width.
/// Width of a badge carrying `text` (its fill, including the [`SPACE_2`] padding).
pub fn badge_width(text: &str) -> u16 {
    measure(text, TypeRole::SmallBold).saturating_add(SPACE_2)
}

/// Badge: `SmallBold` text on a filled block; returns its width.
pub fn badge(
    fb: &mut FrameBuffer,
    x: u16,
    y: u16,
    text: &str,
    fg: Rgb565,
    bg: Rgb565,
) -> u16 {
    let h = cell_height(TypeRole::SmallBold).saturating_add(SPACE_1);
    let w = measure(text, TypeRole::SmallBold).saturating_add(SPACE_2);
    fill(fb, x, y, w, h, bg);
    text::draw_clipped(
        fb,
        x.saturating_add(SPACE_1),
        y.saturating_add((h.saturating_sub(cell_height(TypeRole::SmallBold))) / 2),
        w.saturating_sub(SPACE_2),
        text,
        TypeRole::SmallBold,
        fg,
    );
    w
}

/// Track progress bar with playhead notch, optional jog marker and optional focus ring.
pub struct Progress {
    pub pos_ms: u64,
    pub dur_ms: Option<u64>,
    /// Jog target: amber tick plus, when it is further than `JOG_NEAR_MS` away, the amber band
    /// between the playhead and the target.
    pub marker_ms: Option<u64>,
    /// Draw the `ACCENT` playhead notch.
    pub show_notch: bool,
    /// Bar thickness in pixels.
    pub height: u16,
    /// The time block has focus: 2 px `ACCENT` ring around the whole band.
    pub focus: bool,
    /// A jog click was swallowed at a limit: `Some(true)` = end, `Some(false)` = start.
    pub limit_flash: Option<bool>,
}

/// Vertical space the widget needs: bar plus the notch arms and a gap.
pub const PROGRESS_BAND_H: u16 = 14;

/// Paint the progress band at `y`; returns the first free pixel row below it.
///
/// Colours: `ACCENT_2` upcoming, `ACCENT_DIM` played, `BORDER_SOFT` when the duration is
/// unknown, `ACCENT` playhead notch, `WARN` jog marker / limit flash.
pub fn progress(fb: &mut FrameBuffer, x: u16, y: u16, w: u16, p: Progress) -> u16 {
    if w == 0 {
        return y.saturating_add(p.height.max(1)).saturating_add(10);
    }
    let h = p.height.max(1);
    let bar_y = y.saturating_add(4);
    let play_x = x.saturating_add(duration_offset_ms(w, p.pos_ms, p.dur_ms));
    match p.dur_ms.filter(|d| *d > 0) {
        Some(_) => {
            fill(fb, x, bar_y, w, h, ACCENT_2);
            let played = play_x.saturating_sub(x).min(w);
            fill(fb, x, bar_y, played, h, ACCENT_DIM);
        }
        None => fill(fb, x, bar_y, w, h, BORDER_SOFT),
    }
    if p.show_notch {
        fill(
            fb,
            play_x.min(x.saturating_add(w.saturating_sub(2))),
            bar_y.saturating_sub(4),
            2,
            h.saturating_add(8),
            ACCENT,
        );
    }
    if let (Some(marker), Some(dur)) = (p.marker_ms, p.dur_ms.filter(|d| *d > 0)) {
        let mut marker_x = x.saturating_add(duration_offset_ms(w, marker, Some(dur)));
        let far = marker.abs_diff(p.pos_ms) >= crate::warp_ui::JOG_NEAR_MS;
        if far && marker_x.abs_diff(play_x) < 2 {
            // Keep the two ticks visually separable; the exact position is the mapping above.
            marker_x = if marker_x >= play_x {
                play_x.saturating_add(2)
            } else {
                play_x.saturating_sub(2)
            };
        }
        let (lo, hi) = (marker_x.min(play_x), marker_x.max(play_x));
        if far && hi > lo {
            fill(fb, lo, bar_y, hi - lo, h, WARN);
        }
        fill(
            fb,
            marker_x.min(x.saturating_add(w.saturating_sub(2))),
            bar_y.saturating_sub(2),
            2,
            h.saturating_add(4),
            WARN,
        );
    }
    if let Some(at_end) = p.limit_flash {
        let flash_x = if at_end {
            x.saturating_add(w.saturating_sub(4))
        } else {
            x
        };
        fill(fb, flash_x, bar_y.saturating_sub(1), 4, h.saturating_add(2), WARN);
    }
    if p.focus {
        border(fb, x, y, w, h.saturating_add(8), 2, ACCENT);
    }
    y.saturating_add(h).saturating_add(10)
}

/// X offset of a media position inside a bar of `w` pixels (0 … `w - 1`).
fn duration_offset_ms(w: u16, pos_ms: u64, dur_ms: Option<u64>) -> u16 {
    let Some(dur) = dur_ms.filter(|d| *d > 0) else {
        return 0;
    };
    let span = u64::from(w.saturating_sub(1).max(1));
    (pos_ms.min(dur).saturating_mul(span) / dur) as u16
}

// ---------------------------------------------------------------------------------------------
// Tempo gauge
// ---------------------------------------------------------------------------------------------

/// Gauge inset from the frame edges (shared with tests).
pub const GAUGE_INSET: u16 = MARGIN_X;
/// Gauge track thickness.
pub const GAUGE_H: u16 = 4;
/// Gauge notch / marker height.
pub const GAUGE_MARK_H: u16 = 8;

/// X of a speed magnitude on the gauge for a frame `width`, clamped to the documented range.
pub fn gauge_x(width: u16, magnitude_permille: i32) -> u16 {
    let bar_w = width
        .saturating_sub(GAUGE_INSET.saturating_mul(2))
        .max(2);
    GAUGE_INSET.saturating_add(magnitude_offset(bar_w, magnitude_permille))
}

fn magnitude_offset(bar_w: u16, magnitude_permille: i32) -> u16 {
    let lo = plippa_core::RATE_MIN_ABS_PERMILLE as i64;
    let hi = plippa_core::RATE_MAX_ABS_PERMILLE as i64;
    let m = (magnitude_permille.unsigned_abs() as i64).clamp(lo, hi);
    let span = i64::from(bar_w.max(2)) - 1;
    ((m - lo) * span / (hi - lo)) as u16
}

/// Thin speed gauge: `BORDER_SOFT` track, `FG_DIM` notch at 100% and a `WARN` marker at the
/// set speed. `x`/`w` describe the track; the marker may overhang it by 1 px on each side.
pub fn gauge(fb: &mut FrameBuffer, x: u16, y: u16, w: u16, magnitude_permille: i32) {
    let bar_y = y.saturating_add((GAUGE_MARK_H.saturating_sub(GAUGE_H)) / 2);
    fill(fb, x, bar_y, w, GAUGE_H, BORDER_SOFT);
    let notch = x.saturating_add(magnitude_offset(w, plippa_core::RATE_UNITY_PERMILLE));
    fill(fb, notch, y, 1, GAUGE_MARK_H, FG_DIM);
    let marker = x.saturating_add(magnitude_offset(w, magnitude_permille));
    fill(fb, marker.saturating_sub(1), y, 3, GAUGE_MARK_H, WARN);
}

/// Label left, value right, on one [`ROW_H`] row.
///
/// Focus follows the same rule as [`row`]: the focused row paints `SURFACE` and a 2 px
/// `ACCENT` ring inside the row; the label and value keep their normal `FG` colour.
pub fn key_value(fb: &mut FrameBuffer, y: u16, label: &str, value: &str, on: bool) {
    fill(fb, 0, y, fb.width, ROW_H, if on { SURFACE } else { BG });
    if on {
        border(
            fb,
            SPACE_1,
            y,
            fb.width.saturating_sub(SPACE_1.saturating_mul(2)),
            ROW_H,
            2,
            ACCENT,
        );
    }
    let half = fb.width / 2;
    text::draw_clipped(
        fb,
        MARGIN_X,
        y.saturating_add((ROW_H.saturating_sub(cell_height(TypeRole::Body))) / 2),
        half.saturating_sub(MARGIN_X),
        label,
        TypeRole::Body,
        FG,
    );
    text::draw_aligned(
        fb,
        half,
        y.saturating_add((ROW_H.saturating_sub(cell_height(TypeRole::SmallBold))) / 2),
        half.saturating_sub(MARGIN_X),
        value,
        TypeRole::SmallBold,
        FG,
        Align::Right,
    );
}

/// One-row message with a 2 px colour bar on the left.
pub fn banner(fb: &mut FrameBuffer, y: u16, text: &str, color: Rgb565) {
    fill(fb, 0, y, fb.width, ROW_H, BG);
    fill(fb, 0, y, 2, ROW_H, color);
    text::draw_clipped(
        fb,
        MARGIN_X.saturating_add(SPACE_1),
        y.saturating_add((ROW_H.saturating_sub(cell_height(TypeRole::Small))) / 2),
        fb.width
            .saturating_sub(MARGIN_X.saturating_mul(2))
            .saturating_sub(SPACE_1),
        text,
        TypeRole::Small,
        color,
    );
}

/// Square art placeholder: filled box, 1 px border and a centred label.
pub fn art_box(fb: &mut FrameBuffer, x: u16, y: u16, size: u16, has_art: bool, label: &str) {
    fill(
        fb,
        x,
        y,
        size,
        size,
        if has_art { SURFACE } else { BG_ALT },
    );
    border(
        fb,
        x,
        y,
        size,
        size,
        1,
        if has_art { ACCENT } else { WARN },
    );
    text::draw_aligned(
        fb,
        x,
        y.saturating_add((size.saturating_sub(cell_height(TypeRole::Small))) / 2),
        size,
        label,
        TypeRole::Small,
        if has_art { FG } else { FG_MUTED },
        Align::Centre,
    );
}

// ---------------------------------------------------------------------------------------------
// Shared data-list / empty-state vocabulary (UI_11)
// ---------------------------------------------------------------------------------------------

/// Centred two-line empty / waiting state: `title` in `BodyBold`/`FG`, `hint` below it in
/// `Small`/`FG_MUTED`.
///
/// This is the **only** way a screen says "nothing here". It is vertically centred in the
/// content box ([`CONTENT_TOP`]..[`CONTENT_BOTTOM`]) so an empty list does not look like a
/// one-row list, and paints no background of its own.
pub fn message_state(fb: &mut FrameBuffer, title: &str, hint: &str) {
    let bottom = CONTENT_BOTTOM.min(fb.height);
    let block_h = cell_height(TypeRole::BodyBold)
        .saturating_add(SPACE_1)
        .saturating_add(cell_height(TypeRole::Small));
    let y = CONTENT_TOP.saturating_add(bottom.saturating_sub(CONTENT_TOP).saturating_sub(block_h) / 2);
    let w = fb.width.saturating_sub(MARGIN_X.saturating_mul(2));
    text::draw_aligned(
        fb,
        MARGIN_X,
        y,
        w,
        title,
        TypeRole::BodyBold,
        FG,
        Align::Centre,
    );
    text::draw_aligned(
        fb,
        MARGIN_X,
        y.saturating_add(cell_height(TypeRole::BodyBold))
            .saturating_add(SPACE_1),
        w,
        hint,
        TypeRole::Small,
        FG_MUTED,
        Align::Centre,
    );
}

/// Row of a read-only data list: label left, optional right-aligned value column and an
/// optional playing marker.
///
/// This is [`row`] with a fixed [`ROW_H`] and a value column reserved *before* the label is
/// clipped: `measure(value, Small) + SPACE_2` pixels are always kept clear, so a long label
/// ends in `..` instead of running into its value or off the right margin.
pub struct DataRow<'a> {
    pub label: &'a str,
    pub value: Option<&'a str>,
    /// Extra left inset in pixels (cue rows, hierarchy levels).
    pub indent: u8,
    pub state: RowState,
    /// `SURFACE_ALT` background (use for odd rows).
    pub striped: bool,
    /// Playing marker: a filled `ACCENT_2` triangle left of the label.
    pub marker: bool,
}

/// Paint one [`DataRow`] at `y` (its height is [`ROW_H`]).
pub fn data_row(fb: &mut FrameBuffer, y: u16, r: DataRow<'_>) {
    row(
        fb,
        y,
        Row {
            label: r.label,
            value: r.value,
            indent: r.indent,
            state: r.state,
            striped: r.striped,
            marker: r.marker,
            height: ROW_H,
        },
    );
}

/// Breadcrumb / context strip for a header meta slot: `"Albums"`, `"Albums / Tracks"` or,
/// when a count is known, `"Albums / 12 tracks"`.
///
/// The count quantifies `leaf` (`context_line(Some("Albums"), "tracks", Some(12))`); it is
/// the caller's job to pass a plural that reads correctly. Pure, so it is unit-testable
/// without a framebuffer.
pub fn context_line(parent: Option<&str>, leaf: &str, count: Option<usize>) -> String {
    let mut out = String::new();
    if let Some(parent) = parent.filter(|p| !p.is_empty()) {
        out.push_str(parent);
        out.push_str(" / ");
    }
    match count {
        Some(n) => {
            out.push_str(&n.to_string());
            out.push(' ');
            out.push_str(leaf);
        }
        None => out.push_str(leaf),
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Settings / prose / notice vocabulary (UI_12)
// ---------------------------------------------------------------------------------------------

/// Settings-style row: label left in `Body`, value right-aligned in `Small`/`FG_DIM`, an
/// optional chevron for "opens a screen", and the shared focus ring.
///
/// `value` is omitted (no value column at all) when it is empty, so an unknown value never
/// draws a placeholder dash. `chevron` reserves the last [`SPACE_3`] of the row and paints a
/// `FG_MUTED` arrow there; the value ends [`SPACE_3`] before the right margin.
pub fn setting_row(
    fb: &mut FrameBuffer,
    y: u16,
    label: &str,
    value: &str,
    state: RowState,
    chevron: bool,
) {
    setting_row_impl(fb, y, label, value, None, state, chevron);
}

/// [`setting_row`] with an icon in the value column instead of text (AUDIO_04 output ticks).
///
/// The icon is right-aligned on the same [`ROW_H`] row and each state maps to one ink colour:
/// `ACCENT` while focused or being edited, `FG_DIM` when disabled, `FG` otherwise. The label
/// keeps [`setting_row`]'s geometry and is clipped with the same `..` ellipsis, so a hostile
/// device name can never push the tick off-screen.
pub fn setting_row_with_icon(
    fb: &mut FrameBuffer,
    y: u16,
    label: &str,
    icon: Option<Icon>,
    state: RowState,
) {
    setting_row_impl(fb, y, label, "", icon, state, false);
}

fn setting_row_impl(
    fb: &mut FrameBuffer,
    y: u16,
    label: &str,
    value: &str,
    icon: Option<Icon>,
    state: RowState,
    chevron: bool,
) {
    let focused = matches!(state, RowState::Focused | RowState::Editing);
    fill(fb, 0, y, fb.width, ROW_H, if focused { SURFACE } else { BG });
    if focused {
        border(
            fb,
            SPACE_1,
            y,
            fb.width.saturating_sub(SPACE_1.saturating_mul(2)),
            ROW_H,
            2,
            ACCENT,
        );
    }
    let right = fb.width.saturating_sub(MARGIN_X);
    let value_right = if chevron {
        right.saturating_sub(SPACE_3)
    } else {
        right
    };
    let value_w = if value.is_empty() {
        0
    } else {
        measure(value, TypeRole::Small)
    };
    let value_x = value_right.saturating_sub(value_w);
    // An icon reserves its full 16 px cell in the value column, exactly like a text value.
    let icon_x = right.saturating_sub(ICON_W);
    let reserved_x = if icon.is_some() {
        icon_x
    } else {
        value_x
    };
    let has_value = !value.is_empty() || icon.is_some();
    let label_w = if !has_value {
        right.saturating_sub(MARGIN_X)
    } else {
        reserved_x.saturating_sub(MARGIN_X).saturating_sub(SPACE_2)
    };
    let colour = match state {
        RowState::Disabled => FG_MUTED,
        RowState::Warning => WARN,
        RowState::Normal | RowState::Focused | RowState::Editing => FG,
    };
    text::draw_clipped(
        fb,
        MARGIN_X,
        y.saturating_add((ROW_H.saturating_sub(cell_height(TypeRole::Body))) / 2),
        label_w,
        label,
        TypeRole::Body,
        colour,
    );
    if !value.is_empty() {
        // UI_16: a value being edited is the one thing on the screen that the encoder is
        // changing, so it is the one accent text in a settings list.
        let value_colour = if state == RowState::Editing {
            ACCENT
        } else {
            FG_DIM
        };
        text::draw_aligned(
            fb,
            MARGIN_X,
            y.saturating_add((ROW_H.saturating_sub(cell_height(TypeRole::Small))) / 2),
            value_right.saturating_sub(MARGIN_X),
            value,
            TypeRole::Small,
            value_colour,
            Align::Right,
        );
    }
    if let Some(icon) = icon {
        let icon_colour = match state {
            RowState::Focused | RowState::Editing => ACCENT,
            RowState::Disabled => FG_DIM,
            RowState::Normal | RowState::Warning => FG,
        };
        crate::ui::icons::draw_icon(
            fb,
            icon_x,
            y.saturating_add(ROW_H.saturating_sub(ICON_H) / 2),
            icon,
            icon_colour,
        );
    }
    if chevron {
        // Arm of 4 px starting just inside the reserved `SPACE_3`, vertically centred.
        crate::ui::primitives::chevron_right(
            fb,
            right.saturating_sub(SPACE_3).saturating_add(SPACE_1),
            y.saturating_add(ROW_H / 2),
            4,
            FG_MUTED,
        );
    }
}

/// Wrapped paragraph of body text; returns the first pixel row below the block.
///
/// One line per [`text::wrap`] line, each clipped to `x..x+w`. Empty text paints nothing and
/// returns `y` unchanged.
pub fn paragraph(
    fb: &mut FrameBuffer,
    x: u16,
    y: u16,
    w: u16,
    text: &str,
    role: TypeRole,
    color: Rgb565,
) -> u16 {
    paragraph_upto(fb, x, y, w, u16::MAX, text, role, color)
}

/// [`paragraph`] with a hard bottom limit (exclusive): whole lines that would end at or below
/// `bottom` are dropped rather than clipped mid-line.
pub fn paragraph_upto(
    fb: &mut FrameBuffer,
    x: u16,
    y: u16,
    w: u16,
    bottom: u16,
    text: &str,
    role: TypeRole,
    color: Rgb565,
) -> u16 {
    if w == 0 || text.is_empty() {
        return y;
    }
    let line_h = cell_height(role);
    let mut yy = y;
    for line in text::wrap(text, w, role) {
        if yy.saturating_add(line_h) > bottom {
            break;
        }
        text::draw_clipped(fb, x, yy, w, &line, role, color);
        yy = yy.saturating_add(line_h);
    }
    yy
}

/// Severity of a [`notice`]; picks only the 3 px bar colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    /// Success / all good.
    Ok,
    /// Information: `ACCENT_DIM`, deliberately **not** green (it is not "OK").
    Info,
    Warning,
    Error,
}

impl NoticeSeverity {
    /// Colour of the severity bar.
    pub const fn bar_colour(self) -> Rgb565 {
        match self {
            Self::Ok => OK,
            Self::Info => ACCENT_DIM,
            Self::Warning => WARN,
            Self::Error => ERR,
        }
    }
}

/// Width of the severity bar painted by [`notice`].
pub const NOTICE_BAR_W: u16 = 3;

/// A notice: 3 px severity bar plus wrapped text. The text stays `FG` — the bar carries the
/// severity, which keeps a long message readable instead of a wall of amber.
///
/// Returns the first pixel row below the notice (its last text line's bottom).
pub fn notice(
    fb: &mut FrameBuffer,
    x: u16,
    y: u16,
    w: u16,
    text: &str,
    severity: NoticeSeverity,
) -> u16 {
    if w <= NOTICE_BAR_W {
        return y;
    }
    let text_x = x.saturating_add(NOTICE_BAR_W).saturating_add(SPACE_1);
    let text_w = w
        .saturating_sub(NOTICE_BAR_W)
        .saturating_sub(SPACE_1);
    let line_h = cell_height(TypeRole::Small);
    let lines = text::wrap(text, text_w, TypeRole::Small);
    let h = (lines.len() as u16).saturating_mul(line_h);
    fill(fb, x, y, NOTICE_BAR_W, h, severity.bar_colour());
    let mut yy = y;
    for line in &lines {
        if yy.saturating_add(line_h) > y.saturating_add(h) {
            break;
        }
        text::draw_clipped(fb, text_x, yy, text_w, line, TypeRole::Small, FG);
        yy = yy.saturating_add(line_h);
    }
    y.saturating_add(h)
}

/// One password slot: `SLOT_W` × `SLOT_H`, `BG_ALT` ground, `BORDER_SOFT` outline, a 2 px
/// `ACCENT` ring when focused, and a mask glyph when filled. The actual character is never
/// drawn. Returns the x just right of the slot (callers add their own row gap).
pub const SLOT_W: u16 = advance(TypeRole::Body) + SPACE_1 * 2;
/// Height of one [`char_slot`].
pub const SLOT_H: u16 = cell_height(TypeRole::Body) + SPACE_1 * 2;

pub fn char_slot(fb: &mut FrameBuffer, x: u16, y: u16, ch: Option<char>, focused: bool) -> u16 {
    fill(fb, x, y, SLOT_W, SLOT_H, BG_ALT);
    border(
        fb,
        x,
        y,
        SLOT_W,
        SLOT_H,
        if focused { 2 } else { 1 },
        if focused { ACCENT } else { BORDER_SOFT },
    );
    if ch.is_some() {
        // The bullet is transliterated to `*` by the text layer (mask, one glyph cell).
        text::draw_aligned(
            fb,
            x,
            y.saturating_add(SPACE_1),
            SLOT_W,
            "\u{2022}",
            TypeRole::Body,
            FG,
            Align::Centre,
        );
    }
    x.saturating_add(SLOT_W)
}

/// A PIN box: `SLOT_W` × `SLOT_H`, the same ground and border as [`char_slot`], but a filled
/// box draws the **digit itself**.
///
/// This is the only unmasked slot in the UI, and deliberately so: the PIN screen is a
/// readability requirement (UI_14 §0) — the current PIN is printed on the same screen in the
/// Network row, so a bullet buys nothing. [`char_slot`] still masks the Wi‑Fi password and the
/// pairing passkey.
pub fn digit_slot(fb: &mut FrameBuffer, x: u16, y: u16, ch: Option<char>, focused: bool) -> u16 {
    fill(fb, x, y, SLOT_W, SLOT_H, BG_ALT);
    border(
        fb,
        x,
        y,
        SLOT_W,
        SLOT_H,
        if focused { 2 } else { 1 },
        if focused { ACCENT } else { BORDER_SOFT },
    );
    if let Some(ch) = ch {
        text::draw_aligned(
            fb,
            x,
            y.saturating_add(SPACE_1),
            SLOT_W,
            &ch.to_string(),
            TypeRole::Body,
            FG,
            Align::Centre,
        );
    }
    x.saturating_add(SLOT_W)
}

/// A short hint line pinned to the footer slot (`Enter=scan`, `Back=cancel`).
///
/// Paints the footer band itself, so a screen with only a hint does not need a second call.
pub fn hint(fb: &mut FrameBuffer, text: &str) {
    let y = fb.height.saturating_sub(FOOTER_H);
    fill(fb, 0, y, fb.width, FOOTER_H, BG_ALT);
    hairline(fb, 0, y, fb.width, BORDER_SOFT);
    let ty = y.saturating_add((FOOTER_H.saturating_sub(cell_height(TypeRole::Small))) / 2);
    text::draw_clipped(
        fb,
        MARGIN_X,
        ty,
        fb.width.saturating_sub(MARGIN_X.saturating_mul(2)),
        text,
        TypeRole::Small,
        FG_MUTED,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tokens::{SCREEN_H, SCREEN_W};

    fn fb() -> FrameBuffer {
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(BG);
        fb
    }

    fn pixel(fb: &FrameBuffer, x: u16, y: u16) -> Rgb565 {
        fb.pixels[y as usize * SCREEN_W as usize + x as usize]
    }

    #[test]
    fn row_focus_ring_is_inside_the_row() {
        let mut fb = fb();
        let y = CONTENT_TOP;
        row(
            &mut fb,
            y,
            Row {
                label: "Café - Live",
                value: None,
                indent: 0,
                state: RowState::Focused,
                striped: false,
                marker: false,
                height: ROW_H,
            },
        );
        // Ring: 2 px of ACCENT on each edge, all within the row.
        for x in SPACE_1..SCREEN_W - SPACE_1 {
            assert_eq!(pixel(&fb, x, y), ACCENT, "top ring at {x}");
            assert_eq!(pixel(&fb, x, y + ROW_H - 1), ACCENT, "bottom ring at {x}");
        }
        assert_eq!(pixel(&fb, SPACE_1, y + ROW_H / 2), ACCENT);
        // Just outside the row is untouched.
        assert_eq!(pixel(&fb, 10, y - 1), BG);
        assert_eq!(pixel(&fb, 10, y + ROW_H), BG);
        // The label is not recoloured to the ring colour.
        let view = &fb;
        let label_ink: Vec<Rgb565> = (y..y + ROW_H)
            .flat_map(|yy| (MARGIN_X..SCREEN_W / 2).map(move |xx| pixel(view, xx, yy)))
            .filter(|c| *c != SURFACE && *c != ACCENT)
            .collect();
        assert!(!label_ink.is_empty(), "label painted");
        assert!(label_ink.iter().all(|c| *c == FG), "label keeps the normal text colour");
    }

    #[test]
    fn list_window_follows_focus() {
        let rows: Vec<String> = (0..20).map(|i| format!("Row {i}")).collect();
        let mut fb = fb();
        // Focus 0 → first row of the window is row 0.
        let (start, visible) = list(
            &mut fb,
            List {
                rows: &rows,
                focus: 0,
                top: CONTENT_TOP,
                dense: false,
                disabled: None,
                warning: None,
                marker: None,
                show_focus: true,
                indent: None,
                window_start: None,
            },
        );
        assert_eq!(start, 0);
        assert_eq!(
            visible,
            ((CONTENT_BOTTOM - CONTENT_TOP) / ROW_H) as usize,
            "fills the content box"
        );
        // Focus past the window scrolls it.
        let (start2, visible2) = list(
            &mut fb,
            List {
                rows: &rows,
                focus: 19,
                top: CONTENT_TOP,
                dense: false,
                disabled: None,
                warning: None,
                marker: None,
                show_focus: true,
                indent: None,
                window_start: None,
            },
        );
        assert_eq!(visible2, visible);
        assert_eq!(start2, rows.len() - visible);
        assert!(start2 >= 1);
        // The returned tuple matches the painted window: with an explicit `window_start`, row
        // `start` is painted first and the marker lands on its own absolute row.
        let (start3, visible3) = list(
            &mut fb,
            List {
                rows: &rows,
                focus: 6,
                top: CONTENT_TOP,
                dense: true,
                disabled: None,
                warning: None,
                marker: Some(6),
                show_focus: false,
                indent: None,
                window_start: Some(5),
            },
        );
        assert_eq!((start3, visible3), (5, ((CONTENT_BOTTOM - CONTENT_TOP) / ROW_H_DENSE) as usize));
        let marker_row_y = CONTENT_TOP + ROW_H_DENSE; // abs row 6 is the second painted row
        let view = &fb;
        let marker_ink = (marker_row_y..marker_row_y + ROW_H_DENSE)
            .flat_map(|yy| (MARGIN_X..MARGIN_X + 8).map(move |xx| pixel(view, xx, yy)))
            .any(|c| c == ACCENT_2);
        assert!(marker_ink, "playing marker on the requested row");
        let first_row_ink = (CONTENT_TOP..CONTENT_TOP + ROW_H_DENSE)
            .flat_map(|yy| (MARGIN_X..MARGIN_X + 8).map(move |xx| pixel(view, xx, yy)))
            .any(|c| c == ACCENT_2);
        assert!(!first_row_ink, "no marker on the first visible row");
    }

    #[test]
    fn progress_maps_position_and_marker() {
        let mut fb = fb();
        let x = MARGIN_X;
        let w = SCREEN_W - MARGIN_X * 2;
        let y = CONTENT_TOP;
        let paint = |fb: &mut FrameBuffer, pos: u64, marker: Option<u64>| {
            progress(
                fb,
                x,
                y,
                w,
                Progress {
                    pos_ms: pos,
                    dur_ms: Some(180_000),
                    marker_ms: marker,
                    show_notch: true,
                    height: 4,
                    focus: false,
                    limit_flash: None,
                },
            )
        };
        // 0% → playhead on the first column.
        assert_eq!(paint(&mut fb, 0, None), y + 14);
        assert_eq!(pixel(&fb, x - 1, y + 6), BG, "nothing left of the bar");
        assert_ne!(pixel(&fb, x, y + 4), BG);
        // Half → centre.
        assert_eq!(paint(&mut fb, 90_000, None), y + 14);
        let centre = x + (w - 1) / 2;
        assert_eq!(pixel(&fb, centre, y + 6), ACCENT, "notch at the centre");
        // End → last column.
        assert_eq!(paint(&mut fb, 180_000, None), y + 14);
        assert_eq!(pixel(&fb, x + w - 2, y + 2), ACCENT, "notch at the end");
        // Jog marker far from the playhead → amber band between them.
        assert_eq!(paint(&mut fb, 30_000, Some(60_000)), y + 14);
        // 30s and 60s of 180s on a 304 px bar → x offsets 50 and 101; 45s is inside the band.
        let between = x + (304 - 1) * 45 / 180;
        assert_eq!(pixel(&fb, between, y + 5), WARN, "amber band between playhead and marker");
        // Near marker: no band, just the tick (2 px wide) at the marker.
        assert_eq!(paint(&mut fb, 60_000 - crate::warp_ui::JOG_NEAR_MS / 2, Some(60_000)), y + 14);
        let marker_x = x + (w - 1) / 3;
        let band = (x..marker_x.saturating_sub(2)).any(|xx| pixel(&fb, xx, y + 5) == WARN);
        assert!(!band, "no amber band for a near marker");
        assert_eq!(pixel(&fb, marker_x, y + 2), WARN, "amber marker tick");
    }

    #[test]
    fn gauge_x_monotonic() {
        let width = SCREEN_W;
        let bar_w = width - GAUGE_INSET * 2;
        assert_eq!(gauge_x(width, plippa_core::RATE_MIN_ABS_PERMILLE), GAUGE_INSET);
        assert_eq!(
            gauge_x(width, plippa_core::RATE_MAX_ABS_PERMILLE),
            GAUGE_INSET + bar_w - 1
        );
        // Clamps outside the documented range.
        assert_eq!(gauge_x(width, 0), GAUGE_INSET);
        assert_eq!(gauge_x(width, 99), GAUGE_INSET);
        assert_eq!(gauge_x(width, 9_999), GAUGE_INSET + bar_w - 1);
        // Non-decreasing across the range.
        let mut last = 0;
        for magnitude in (0..=2_500).step_by(10) {
            let x = gauge_x(width, magnitude);
            assert!(x >= last, "gauge_x dropped at {magnitude}: {x} < {last}");
            last = x;
        }
    }

    #[test]
    fn header_title_is_fg_and_only_the_rule_is_accent() {
        let mut fb = fb();
        header(
            &mut fb,
            Header {
                title: "Library",
                meta: None,
            },
        );
        let ink: Vec<Rgb565> = (0..HEADER_H)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .map(|(x, y)| pixel(&fb, x, y))
            .filter(|c| *c != SURFACE)
            .collect();
        assert!(!ink.is_empty(), "title painted");
        assert!(
            ink.iter().all(|c| *c == FG),
            "the shell title is FG, never ACCENT"
        );
        assert_eq!(pixel(&fb, 0, HEADER_H), ACCENT_DIM, "rule stays accent");
    }

    #[test]
    fn key_value_focus_is_a_ring_not_a_colour_flip() {
        let mut fb = fb();
        let y = CONTENT_TOP;
        key_value(&mut fb, y, "Speed warp", "Correct", true);
        for x in SPACE_1..SCREEN_W - SPACE_1 {
            assert_eq!(pixel(&fb, x, y), ACCENT, "top ring at {x}");
            assert_eq!(pixel(&fb, x, y + ROW_H - 1), ACCENT, "bottom ring at {x}");
        }
        // Label and value keep `FG`; nothing is recoloured to the ring colour.
        let text_colours: Vec<Rgb565> = (y..y + ROW_H)
            .flat_map(|yy| (MARGIN_X..SCREEN_W - MARGIN_X).map(move |xx| (xx, yy)))
            .map(|(x, yy)| pixel(&fb, x, yy))
            .filter(|c| *c != SURFACE && *c != ACCENT)
            .collect();
        assert!(!text_colours.is_empty());
        assert!(text_colours.iter().all(|c| *c == FG));
        // An unfocused row has no ring at all.
        key_value(&mut fb, y + ROW_H, "Seek warp", "Shift", false);
        assert!((0..SCREEN_W).all(|x| pixel(&fb, x, y + ROW_H) != ACCENT));
    }

    #[test]
    fn list_applies_per_row_indent() {
        let rows = vec!["Cue".to_string(), "Cue".to_string()];
        let mut plain = fb();
        let mut indented = fb();
        let indent = |i: usize| if i == 1 { 8 } else { 0 };
        let list_at = |fb: &mut FrameBuffer, indent: Option<&dyn Fn(usize) -> u8>| {
            list(
                fb,
                List {
                    rows: &rows,
                    focus: 99,
                    top: CONTENT_TOP,
                    dense: false,
                    disabled: None,
                    warning: None,
                    marker: None,
                    show_focus: false,
                    indent,
                    window_start: None,
                },
            )
        };
        list_at(&mut plain, None);
        list_at(&mut indented, Some(&indent));
        // First column carrying glyph ink (the row grounds are not text).
        let first_ink = |fb: &FrameBuffer, y0: u16| {
            (0..SCREEN_W)
                .find(|&x| {
                    (y0..y0 + ROW_H).any(|y| {
                        !matches!(pixel(fb, x, y), BG | SURFACE | SURFACE_ALT)
                    })
                })
                .unwrap()
        };
        let row0 = CONTENT_TOP;
        let row1 = CONTENT_TOP + ROW_H;
        // Both rows carry the same label, so no indent means the same first ink column.
        assert_eq!(first_ink(&plain, row0), first_ink(&plain, row1));
        assert!((MARGIN_X..MARGIN_X + 9).contains(&first_ink(&plain, row0)));
        assert_eq!(first_ink(&indented, row0), first_ink(&plain, row0));
        assert_eq!(
            first_ink(&indented, row1),
            first_ink(&indented, row0) + 8,
            "the second row's label starts 8 px further in"
        );
    }

    #[test]
    fn header_meta_is_right_aligned() {
        let mut fb = fb();
        header(
            &mut fb,
            Header {
                title: "Library",
                meta: Some("3/12"),
            },
        );
        // The band is SURFACE and the rule is ACCENT_DIM.
        assert_eq!(pixel(&fb, 0, 0), SURFACE);
        assert_eq!(pixel(&fb, 5, HEADER_H), ACCENT_DIM);
        // The meta string is right-aligned into the band: its ink ends at (or within one glyph
        // cell of) `SCREEN_W - MARGIN_X`, and nothing paints past that margin.
        let cols: Vec<u16> = (0..SCREEN_W)
            .filter(|&x| (0..HEADER_H).any(|y| pixel(&fb, x, y) != SURFACE))
            .collect();
        let last = *cols.iter().filter(|&&x| x > SCREEN_W / 2).max().unwrap();
        assert!(
            last < SCREEN_W - MARGIN_X,
            "meta painted past the right margin: {last}"
        );
        assert!(
            last >= SCREEN_W - MARGIN_X - crate::ui::text::advance(TypeRole::Small),
            "meta not right-aligned: last ink at {last}"
        );
        // The title starts at the left margin.
        let first = *cols.iter().min().unwrap();
        assert!((MARGIN_X..MARGIN_X + 2).contains(&first), "title at {first}");
    }
}

/// UI_11 additions: the shared data-row / empty-state vocabulary.
#[cfg(test)]
mod ui11_tests {
    use super::*;
    use crate::ui::tokens::{SCREEN_H, SCREEN_W};

    fn fb() -> FrameBuffer {
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(BG);
        fb
    }

    fn pixel(fb: &FrameBuffer, x: u16, y: u16) -> Rgb565 {
        fb.pixels[y as usize * SCREEN_W as usize + x as usize]
    }

    #[test]
    fn context_line_formats() {
        assert_eq!(context_line(None, "Library", None), "Library");
        assert_eq!(context_line(Some("Albums"), "Tracks", None), "Albums / Tracks");
        assert_eq!(
            context_line(Some("Albums"), "tracks", Some(12)),
            "Albums / 12 tracks"
        );
        assert_eq!(context_line(None, "tracks", Some(0)), "0 tracks");
        // An empty parent is not a crumb.
        assert_eq!(context_line(Some(""), "Library", None), "Library");
    }

    #[test]
    fn data_row_value_column_never_overlaps() {
        let value = "1:23:45";
        let label = "Café del Mar — Remastered Edition and then some more words";
        let y = CONTENT_TOP;
        let mut frame = fb();
        data_row(
            &mut frame,
            y,
            DataRow {
                label,
                value: Some(value),
                indent: 0,
                state: RowState::Normal,
                striped: false,
                marker: false,
            },
        );

        // The value column is reserved first: exactly the control render of the value alone
        // sits in it, pixel for pixel.
        let mut control = fb();
        control.clear(BG);
        let value_w = measure(value, TypeRole::Small);
        let value_x = SCREEN_W - MARGIN_X - value_w;
        text::draw_aligned(
            &mut control,
            MARGIN_X,
            y.saturating_add((ROW_H.saturating_sub(cell_height(TypeRole::Small))) / 2),
            SCREEN_W.saturating_sub(MARGIN_X.saturating_mul(2)),
            value,
            TypeRole::Small,
            FG_DIM,
            Align::Right,
        );
        for yy in y..y + ROW_H {
            for x in value_x..SCREEN_W - MARGIN_X {
                assert_eq!(
                    pixel(&frame, x, yy),
                    pixel(&control, x, yy),
                    "value column pixel ({x},{yy})"
                );
            }
        }

        // The label is really there, clipped with `..`, and never reaches the value column.
        let label_cols: Vec<u16> = (0..SCREEN_W)
            .filter(|&x| (y..y + ROW_H).any(|yy| pixel(&frame, x, yy) == FG))
            .collect();
        assert!(!label_cols.is_empty(), "label painted");
        assert!(
            (MARGIN_X..MARGIN_X + text::advance(TypeRole::Body)).contains(&*label_cols.iter().min().unwrap()),
            "label starts at the left margin"
        );
        assert!(
            *label_cols.iter().max().unwrap() < value_x,
            "label overlap into the value column"
        );
        assert!(
            measure(label, TypeRole::Body) > value_x.saturating_sub(MARGIN_X),
            "test label must be wider than the space left of the value"
        );
    }

    #[test]
    fn data_row_disabled_is_muted() {
        let y = CONTENT_TOP;
        let mut fb = fb();
        data_row(
            &mut fb,
            y,
            DataRow {
                label: "Gone",
                value: Some("1:05"),
                indent: 0,
                state: RowState::Disabled,
                striped: false,
                marker: false,
            },
        );
        let row_pixels: Vec<Rgb565> = (y..y + ROW_H)
            .flat_map(|yy| (0..SCREEN_W).map(move |x| (x, yy)))
            .map(|(x, yy)| pixel(&fb, x, yy))
            .filter(|c| *c != BG)
            .collect();
        assert!(
            row_pixels.contains(&FG_MUTED),
            "disabled label uses FG_MUTED"
        );
        assert!(!row_pixels.contains(&ACCENT), "no focus ring when disabled");
        assert!(
            !row_pixels.contains(&FG),
            "a disabled row never uses the primary text colour"
        );
    }

    #[test]
    fn message_state_is_centred_and_two_roles() {
        let mut fb = fb();
        message_state(&mut fb, "Library empty", "Add music from the portal");
        let inked: Vec<(u16, u16, Rgb565)> = (0..SCREEN_H)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .map(|(x, y)| (x, y, pixel(&fb, x, y)))
            .filter(|(_, _, c)| *c != BG)
            .collect();
        assert!(inked.iter().any(|(_, _, c)| *c == FG), "title ink");
        assert!(inked.iter().any(|(_, _, c)| *c == FG_MUTED), "hint ink");
        // Both lines sit inside the content box and are horizontally centred: the title's
        // first and last ink columns are symmetric around the screen's centre line.
        let title_cols: Vec<u16> = inked
            .iter()
            .filter(|(_, _, c)| *c == FG)
            .map(|(x, _, _)| *x)
            .collect();
        let first = *title_cols.iter().min().unwrap();
        let last = *title_cols.iter().max().unwrap();
        assert!(first > MARGIN_X, "centred, not left-aligned: {first}");
        let mid = (first + last) / 2;
        assert!(
            mid.abs_diff((SCREEN_W - 1) / 2) <= text::advance(TypeRole::Small),
            "title not centred: {first}..{last}"
        );
        let ys: Vec<u16> = inked.iter().map(|(_, y, _)| *y).collect();
        assert!(*ys.iter().min().unwrap() >= CONTENT_TOP);
        assert!(*ys.iter().max().unwrap() < CONTENT_BOTTOM);
    }
}

/// UI_12 additions: settings rows, paragraphs, notices, password slots and hints.
#[cfg(test)]
mod ui12_tests {
    use super::*;
    use crate::ui::tokens::{SCREEN_H, SCREEN_W};

    fn fb() -> FrameBuffer {
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(BG);
        fb
    }

    fn pixel(fb: &FrameBuffer, x: u16, y: u16) -> Rgb565 {
        fb.pixels[y as usize * SCREEN_W as usize + x as usize]
    }

    #[test]
    fn setting_row_value_and_chevron() {
        let mut frame = fb();
        let y = CONTENT_TOP;
        let value = "192.168.1.42";
        setting_row(
            &mut frame,
            y,
            "A label far too long to fit on one settings row and then some",
            value,
            RowState::Normal,
            true,
        );
        let right = SCREEN_W - MARGIN_X;
        // The value is right-aligned into a control render: its box ends `SPACE_3` before the
        // margin and is exactly the value's glyphs.
        let mut control = fb();
        text::draw_aligned(
            &mut control,
            MARGIN_X,
            y + (ROW_H - cell_height(TypeRole::Small)) / 2,
            right - SPACE_3 - MARGIN_X,
            value,
            TypeRole::Small,
            FG_DIM,
            Align::Right,
        );
        let value_x = right - SPACE_3 - measure(value, TypeRole::Small);
        for yy in y..y + ROW_H {
            for x in value_x..right - SPACE_3 {
                assert_eq!(
                    pixel(&frame, x, yy),
                    pixel(&control, x, yy),
                    "value column pixel ({x},{yy})"
                );
            }
        }
        // The chevron lives in the last `SPACE_3` before the margin and is `FG_MUTED`.
        let mut chevron_px = 0;
        for yy in y..y + ROW_H {
            for x in right - SPACE_3..right {
                if pixel(&frame, x, yy) == FG_MUTED {
                    chevron_px += 1;
                }
            }
        }
        assert!(chevron_px > 0, "no chevron in the reserved right strip");
        // The long label was clipped before the value column: no `FG` glyph reaches it.
        for yy in y..y + ROW_H {
            for x in value_x..right - SPACE_3 {
                assert_ne!(pixel(&frame, x, yy), FG, "label runs into the value");
            }
        }
        // Focus is the ring, not a colour flip.
        let mut focused = fb();
        setting_row(&mut focused, y, "IP", value, RowState::Focused, false);
        for x in SPACE_1..SCREEN_W - SPACE_1 {
            assert_eq!(pixel(&focused, x, y), ACCENT, "top ring at {x}");
        }
        // No chevron: the value uses the full right margin.
        assert!((0..ROW_H).any(|yy| pixel(&focused, right - 1, y + yy) != BG));
    }

    #[test]
    fn setting_row_with_icon_keeps_the_tick_inside_the_row_and_the_label_clear() {
        // AUDIO_04 §5.3: the icon sits in the value column on the row, the label is clipped
        // before it, and nothing reaches the right margin or the row outside its box.
        let mut frame = fb();
        let y = CONTENT_TOP;
        let right = SCREEN_W - MARGIN_X;
        let icon_x = right - ICON_W;
        setting_row_with_icon(
            &mut frame,
            y,
            "A device description far too long to fit before its tick row",
            Some(Icon::Check),
            RowState::Focused,
        );
        // The focus ring is still the shared 2 px ring on the row box.
        assert_eq!(pixel(&frame, SPACE_1, y), ACCENT);
        assert_eq!(pixel(&frame, SPACE_1, y + ROW_H - 1), ACCENT);
        // The focused icon is ACCENT ink inside its 16x16 cell, clear of the ring rows.
        let icon_y = y + (ROW_H - ICON_H) / 2;
        let icon_ink = (icon_x..right)
            .flat_map(|x| (icon_y..icon_y + ICON_H).map(move |yy| (x, yy)))
            .filter(|(x, yy)| pixel(&frame, *x, *yy) == ACCENT)
            .count();
        assert!(icon_ink > 20, "focused icon painted {icon_ink} ACCENT px");
        for x in icon_x..right {
            assert!(
                (y..y + ROW_H).all(|yy| {
                    let p = pixel(&frame, x, yy);
                    p == ACCENT || p == SURFACE || p == BG
                }),
                "unexpected ink in the icon column at {x}"
            );
        }
        // Clipped label ink stops SPACE_2 before the icon cell: it can never push the tick
        // off-screen (the "scrollbar gutter" here is the value column).
        for yy in y + 1..y + ROW_H - 1 {
            for x in icon_x - SPACE_2..SCREEN_W {
                assert_ne!(pixel(&frame, x, yy), FG, "label ink at ({x},{yy})");
            }
        }
        // A disabled row draws the tick in FG_DIM, not ACCENT.
        let mut disabled = fb();
        setting_row_with_icon(
            &mut disabled,
            y,
            "Output",
            Some(Icon::BoxEmpty),
            RowState::Disabled,
        );
        let dim = (icon_x..right)
            .flat_map(|x| (icon_y..icon_y + ICON_H).map(move |yy| (x, yy)))
            .filter(|(x, yy)| pixel(&disabled, *x, *yy) == FG_DIM)
            .count();
        assert!(dim > 20, "disabled icon painted {dim} FG_DIM px");
    }

    #[test]
    fn paragraph_wraps_and_returns_next_y() {
        let mut frame = fb();
        let text = "one two three four five six seven eight nine ten eleven twelve thirteen";
        let w = 180u16;
        let lines = text::wrap(text, w, TypeRole::Small);
        assert_eq!(lines.len(), 3, "{lines:?}");
        let next = paragraph(&mut frame, 10, 20, w, text, TypeRole::Small, FG);
        assert_eq!(next, 20 + 3 * cell_height(TypeRole::Small));
        // Every glyph pixel lies inside the box, and each of the three lines is painted.
        for y in 0..SCREEN_H as usize {
            for x in 0..SCREEN_W as usize {
                if !(10..10 + w as usize).contains(&x) || !(20..next as usize).contains(&y) {
                    assert_eq!(frame.pixels[y * SCREEN_W as usize + x], BG, "({x},{y})");
                }
            }
        }
        for line in 0..3 {
            let y = 20 + line * cell_height(TypeRole::Small);
            assert!(
                (y..y + cell_height(TypeRole::Small))
                    .any(|yy| (10..10 + w).any(|x| pixel(&frame, x, yy) == FG)),
                "line {line} not painted"
            );
        }
        // Empty text is not a block.
        assert_eq!(
            paragraph(&mut frame, 10, 20, w, "", TypeRole::Small, FG),
            20
        );
        // `paragraph_upto` drops a whole trailing line instead of clipping it.
        let next2 = paragraph_upto(
            &mut frame,
            10,
            20,
            w,
            20 + 2 * cell_height(TypeRole::Small),
            text,
            TypeRole::Small,
            FG,
        );
        assert_eq!(next2, 20 + 2 * cell_height(TypeRole::Small));
    }

    #[test]
    fn notice_severity_bar_colour() {
        let y = CONTENT_TOP;
        for (severity, bar) in [
            (NoticeSeverity::Ok, OK),
            (NoticeSeverity::Info, ACCENT_DIM),
            (NoticeSeverity::Warning, WARN),
            (NoticeSeverity::Error, ERR),
        ] {
            let mut frame = fb();
            let next = notice(&mut frame, MARGIN_X, y, 200, "Skipped a file", severity);
            assert_eq!(next, y + cell_height(TypeRole::Small));
            for yy in y..next {
                for x in MARGIN_X..MARGIN_X + NOTICE_BAR_W {
                    assert_eq!(pixel(&frame, x, yy), bar, "{severity:?} bar ({x},{yy})");
                }
            }
            // The text itself stays `FG`, whatever the severity.
            let text_x = MARGIN_X + NOTICE_BAR_W + SPACE_1;
            let ink: Vec<Rgb565> = (y..next)
                .flat_map(|yy| (text_x..MARGIN_X + 200).map(move |x| (x, yy)))
                .map(|(x, yy)| pixel(&frame, x, yy))
                .filter(|c| *c != BG)
                .collect();
            assert!(!ink.is_empty(), "{severity:?} text painted");
            assert!(ink.iter().all(|c| *c == FG), "{severity:?} recoloured text");
        }
        // Info is not green.
        assert_ne!(NoticeSeverity::Info.bar_colour(), OK);
    }

    #[test]
    fn char_slot_masks_and_returns_the_next_x() {
        let y = CONTENT_TOP;
        let mut frame = fb();
        let next = char_slot(&mut frame, MARGIN_X, y, Some('s'), false);
        assert_eq!(next, MARGIN_X + SLOT_W);
        assert_eq!(SLOT_W, text::advance(TypeRole::Body) + SPACE_1 * 2);
        assert_eq!(SLOT_H, cell_height(TypeRole::Body) + SPACE_1 * 2);
        // The outline is soft and the real glyph never appears: only the mask (`*`).
        assert_eq!(pixel(&frame, MARGIN_X, y), BORDER_SOFT);
        let inside: Vec<Rgb565> = (y..y + SLOT_H)
            .flat_map(|yy| (MARGIN_X..MARGIN_X + SLOT_W).map(move |x| (x, yy)))
            .map(|(x, yy)| pixel(&frame, x, yy))
            .filter(|c| *c != BG_ALT && *c != BORDER_SOFT)
            .collect();
        assert!(!inside.is_empty(), "mask glyph painted");
        assert!(inside.iter().all(|c| *c == FG));
        // Empty slot: no mask.
        let mut empty = fb();
        char_slot(&mut empty, MARGIN_X, y, None, false);
        let ink = (y..y + SLOT_H)
            .flat_map(|yy| (MARGIN_X..MARGIN_X + SLOT_W).map(move |x| (x, yy)))
            .map(|(x, yy)| pixel(&empty, x, yy))
            .filter(|c| *c != BG_ALT && *c != BORDER_SOFT)
            .count();
        assert_eq!(ink, 0, "an empty slot shows no glyph");
        // Focused: a 2 px `ACCENT` ring.
        let mut focused = fb();
        char_slot(&mut focused, MARGIN_X, y, None, true);
        for x in MARGIN_X..MARGIN_X + SLOT_W {
            assert_eq!(pixel(&focused, x, y), ACCENT);
            assert_eq!(pixel(&focused, x, y + 1), ACCENT);
            assert_eq!(pixel(&focused, x, y + SLOT_H - 2), ACCENT);
        }
    }

    #[test]
    fn hint_paints_the_footer_slot() {
        let mut frame = fb();
        hint(&mut frame, "Enter=scan");
        let top = SCREEN_H - FOOTER_H;
        assert_eq!(pixel(&frame, 0, top), BORDER_SOFT, "footer hairline");
        assert_eq!(pixel(&frame, 0, top + 1), BG_ALT, "footer band");
        let ink: Vec<Rgb565> = (top + 1..SCREEN_H)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .map(|(x, y)| pixel(&frame, x, y))
            .filter(|c| *c != BG_ALT && *c != BORDER_SOFT)
            .collect();
        assert!(!ink.is_empty());
        assert!(ink.iter().all(|c| *c == FG_MUTED), "hint is muted, not body");
    }

    // ---- UI_13: Now Playing chrome ---------------------------------------------------------

    fn np_frame() -> FrameBuffer {
        let mut f = FrameBuffer::new(SCREEN_W, SCREEN_H);
        f.clear(BG);
        f
    }

    fn np_ink_rows(f: &FrameBuffer, y0: u16, y1: u16) -> usize {
        (y0..y1)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let c = pixel(f, x, y);
                c == FG || c == FG_DIM || c == ACCENT
            })
            .count()
    }

    #[test]
    fn np_header_two_rows_roles() {
        let mut f = np_frame();
        np_header(
            &mut f,
            NpHeader {
                title: "Title",
                artist: "Artist",
                release: "Album",
                count: "2/3",
                modes: NpModeBadges::NONE,
            },
        );
        // Two `Display` rows: 20 px cells at y = 5 and y = 35, 5 px inset above and below.
        assert!(np_ink_rows(&f, 5, 25) > 20, "row 0: title + artist");
        assert!(np_ink_rows(&f, 35, 55) > 20, "row 1: release + count");
        assert_eq!(np_ink_rows(&f, 0, 5), 0, "5 px inset above row 0");
        assert_eq!(np_ink_rows(&f, 25, 35), 0, "5 px inset between the rows");
        assert_eq!(np_ink_rows(&f, 55, NP_HEADER_H), 0, "5 px inset below row 1");
        // Roles: only the title is ACCENT.
        let has = |y0: u16, y1: u16, c: Rgb565| {
            (y0..y1)
                .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
                .any(|(x, y)| pixel(&f, x, y) == c)
        };
        assert!(has(5, 25, ACCENT) && has(5, 25, FG_DIM), "title accent, artist dim");
        assert!(!has(35, 55, ACCENT), "release/count are not accent");
        assert!(has(35, 55, FG_DIM));
        // The rule sits at exactly NP_HEADER_H in ACCENT_DIM.
        assert_eq!(pixel(&f, 100, NP_HEADER_H), ACCENT_DIM);
        assert_eq!(pixel(&f, 100, NP_HEADER_H - 1), SURFACE);
        assert_eq!(pixel(&f, 100, NP_HEADER_H + 1), BG, "header owns 0..NP_HEADER_H + rule");
    }

    // ---- PLAY_05: shuffle / repeat badges ------------------------------------------------

    /// Right-aligned geometry for row 1 with `count` and `modes`, as the widget computes it.
    /// The literal numbers in the badge tests below are the contract (2/3 = 30 px, count ends
    /// at x = 312, `SPACE_2` before the badges).
    fn badge_box(count: &str, modes: NpModeBadges) -> (u16, u16) {
        let right = SCREEN_W - MARGIN_X;
        let count_w = measure(count, TypeRole::Display);
        let gap = if count.is_empty() { 0 } else { SPACE_2 };
        let badge_right = right - count_w - gap;
        (badge_right - modes.width(), badge_right)
    }

    fn accent_box(f: &FrameBuffer, y0: u16, y1: u16) -> Vec<(u16, u16)> {
        (y0..y1)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|&(x, y)| pixel(f, x, y) == ACCENT)
            .collect()
    }

    #[test]
    fn np_mode_badges_width_adds_up() {
        assert_eq!(NpModeBadges::NONE.width(), 0);
        assert!(!NpModeBadges::NONE.any());
        let shuffle = NpModeBadges {
            shuffle: true,
            ..NpModeBadges::NONE
        };
        let repeat = NpModeBadges {
            repeat: true,
            ..NpModeBadges::NONE
        };
        let one = NpModeBadges {
            repeat: true,
            repeat_one: true,
            ..NpModeBadges::NONE
        };
        let both = NpModeBadges {
            shuffle: true,
            repeat: true,
            repeat_one: true,
        };
        assert_eq!(shuffle.width(), icons::ICON_W, "one icon");
        assert_eq!(repeat.width(), icons::ICON_W);
        assert_eq!(
            one.width(),
            icons::ICON_W + NpModeBadges::NUMBER_GAP + advance(TypeRole::Display),
            "the repeat icon plus the 2 px gap and the Display cell of its 1"
        );
        assert_eq!(
            both.width(),
            icons::ICON_W + SPACE_1 + one.width(),
            "shuffle, SPACE_1, repeat(+1)"
        );
        assert!(both.any() && one.any() && repeat.any() && shuffle.any());
    }

    #[test]
    fn np_header_badges_sit_between_the_release_and_the_count() {
        let modes = NpModeBadges {
            shuffle: true,
            repeat: true,
            repeat_one: true,
        };
        let mut f = np_frame();
        np_header(
            &mut f,
            NpHeader {
                title: "Title",
                artist: "Artist",
                release: "Album",
                count: "2/3",
                modes,
            },
        );
        let (left, right) = badge_box("2/3", modes);
        // `2/3` is 30 px, ending at x = 312; SPACE_2 to the group (48 px wide: 16 + 4 + 16 +
        // 2 + 10) puts its left edge at 226.
        assert_eq!((left, right), (226, 274), "badge group geometry");
        // Both icons are ACCENT, and their ink is exactly the bitmap, 1:1 at y = 37.
        // Row 1's 20 px cell starts at ROW_H_DISPLAY + 5 (the band inset), so a 16 px icon
        // centred in it starts at 30 + 5 + 2 = 37.
        let icon_y = ROW_H_DISPLAY
            + (ROW_H_DISPLAY - cell_height(TypeRole::Display)) / 2
            + (cell_height(TypeRole::Display) - icons::ICON_H) / 2;
        assert_eq!(icon_y, 37, "icons are centred on the 20 px text cell");
        for (x, bitmap) in [
            (left, icons::SHUFFLE),
            (left + icons::ICON_W + SPACE_1, icons::REPEAT),
        ] {
            for row in 0..icons::ICON_H {
                for col in 0..icons::ICON_W {
                    let want = if bitmap[row as usize] & (1 << col) != 0 {
                        ACCENT
                    } else {
                        SURFACE
                    };
                    assert_eq!(
                        pixel(&f, x + col, icon_y + row),
                        want,
                        "icon pixel ({col}, {row}) at x = {x}"
                    );
                }
            }
        }
        // No ACCENT leaks out of the group in row 1 (the `1` included).
        for (x, _) in accent_box(&f, ROW_H_DISPLAY, NP_HEADER_H) {
            assert!(
                (left..right).contains(&x),
                "accent ink outside the badge group at {x}"
            );
        }
        // The `1` follows the repeat icon: ink in its Display cell.
        let one_x = left + icons::ICON_W + SPACE_1 + icons::ICON_W + NpModeBadges::NUMBER_GAP;
        assert!(
            accent_box(&f, ROW_H_DISPLAY, NP_HEADER_H)
                .iter()
                .any(|(x, _)| (one_x..one_x + advance(TypeRole::Display)).contains(x)),
            "Repeat one draws its 1 inside the badge group"
        );
    }

    #[test]
    fn np_header_default_rung_paints_no_badge() {
        let (title, artist, count) = ("Title", "Artist", "2/3");
        let mut plain = np_frame();
        np_header(
            &mut plain,
            NpHeader {
                title,
                artist,
                release: "Album",
                count,
                modes: NpModeBadges::NONE,
            },
        );
        // With no mode set the header is unchanged: no icon ink, and the release still uses the
        // full width left of the count.
        assert!(
            accent_box(&plain, ROW_H_DISPLAY, NP_HEADER_H).is_empty(),
            "no badge at the default rung"
        );
        let mut shuffle_only = np_frame();
        np_header(
            &mut shuffle_only,
            NpHeader {
                title,
                artist,
                release: "Album",
                count,
                modes: NpModeBadges {
                    shuffle: true,
                    ..NpModeBadges::NONE
                },
            },
        );
        assert!(
            !accent_box(&shuffle_only, ROW_H_DISPLAY, NP_HEADER_H).is_empty(),
            "Shuffle shows the shuffle badge"
        );
    }

    #[test]
    fn np_header_badges_do_not_move_the_count() {
        let modes = NpModeBadges {
            shuffle: true,
            repeat: true,
            ..NpModeBadges::NONE
        };
        let mut plain = np_frame();
        np_header(
            &mut plain,
            NpHeader {
                title: "Title",
                artist: "Artist",
                release: "Album",
                count: "10/12",
                modes: NpModeBadges::NONE,
            },
        );
        let mut badges = np_frame();
        np_header(
            &mut badges,
            NpHeader {
                title: "Title",
                artist: "Artist",
                release: "Album",
                count: "10/12",
                modes,
            },
        );
        // The count owns the right edge whatever the badges do.
        let count_x = SCREEN_W - MARGIN_X - measure("10/12", TypeRole::Display);
        for y in ROW_H_DISPLAY..NP_HEADER_H {
            for x in count_x..SCREEN_W {
                assert_eq!(
                    pixel(&plain, x, y),
                    pixel(&badges, x, y),
                    "the badge group moved the count at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn np_header_badges_sit_at_the_margin_when_there_is_no_count() {
        // No `ordinal/total` (a collection with no track entries): the badges take the slot at
        // the right margin and the release still stops `SPACE_2` before them.
        let long: String = "B".repeat(80);
        let modes = NpModeBadges {
            shuffle: true,
            repeat: true,
            ..NpModeBadges::NONE
        };
        let mut f = np_frame();
        np_header(
            &mut f,
            NpHeader {
                title: "Title",
                artist: "Artist",
                release: &long,
                count: "",
                modes,
            },
        );
        let (left, right) = badge_box("", modes);
        assert_eq!(right, SCREEN_W - MARGIN_X, "badges end at the margin");
        let icon_y = ROW_H_DISPLAY
            + (ROW_H_DISPLAY - cell_height(TypeRole::Display)) / 2
            + (cell_height(TypeRole::Display) - icons::ICON_H) / 2;
        for (x, bitmap) in [
            (left, icons::SHUFFLE),
            (left + icons::ICON_W + SPACE_1, icons::REPEAT),
        ] {
            for row in 0..icons::ICON_H {
                for col in 0..icons::ICON_W {
                    let want = if bitmap[row as usize] & (1 << col) != 0 {
                        ACCENT
                    } else {
                        SURFACE
                    };
                    assert_eq!(
                        pixel(&f, x + col, icon_y + row),
                        want,
                        "icon pixel ({col}, {row})"
                    );
                }
            }
        }
        // Nothing of the release reaches the gap or the icons.
        for y in ROW_H_DISPLAY..NP_HEADER_H {
            for x in left - SPACE_2..SCREEN_W {
                let c = pixel(&f, x, y);
                assert!(
                    c == SURFACE || c == ACCENT,
                    "release ink at ({x}, {y}) with no count on screen"
                );
            }
        }
    }

    #[test]
    fn np_header_long_release_is_cut_before_the_badges() {
        // A collection title far wider than the panel must be ellipsised *before* the badge
        // group: the icons are never drawn over it and it is never drawn over them. Nothing in
        // this test recomputes the widget's own width arithmetic — it reads the pixels.
        let long: String = "A".repeat(80);
        let modes = NpModeBadges {
            shuffle: true,
            repeat: true,
            repeat_one: true,
        };
        let count = "10/12";
        let mut actual = np_frame();
        np_header(
            &mut actual,
            NpHeader {
                title: "Title",
                artist: "Artist",
                release: &long,
                count,
                modes,
            },
        );
        let (badge_left, _) = badge_box(count, modes);
        let count_x = SCREEN_W - MARGIN_X - measure(count, TypeRole::Display);

        // 1. No text ink in the gap or under the badges. The release owns the pixels left of
        //    the badges; the rest is background or ACCENT icon ink.
        let mut release_ink_max = 0;
        for y in ROW_H_DISPLAY..NP_HEADER_H {
            for x in 0..count_x {
                let c = pixel(&actual, x, y);
                if x >= badge_left {
                    assert!(
                        c == SURFACE || c == ACCENT || c == BORDER_SOFT,
                        "release ink at ({x}, {y}) where the badges live"
                    );
                } else if c == FG_DIM {
                    release_ink_max = release_ink_max.max(x);
                }
            }
        }
        assert!(
            release_ink_max + SPACE_2 <= badge_left,
            "release ink reaches {release_ink_max}, inside the {SPACE_2} px gap before badges at \
             {badge_left}"
        );

        // 2. It is packed up to the limit, not cut far too early: less than one advance of slack.
        let slack = badge_left.saturating_sub(release_ink_max);
        assert!(
            slack <= advance(TypeRole::Display) + SPACE_2,
            "release stops {slack} px short of the gap — it should use the width it has"
        );

        // 3. The last two glyph cells are `..`: the title is visibly truncated. The cells are on
        //    the `Display` advance grid starting at MARGIN_X; compare them with a reference `..`.
        let mut reference = FrameBuffer::new(SCREEN_W, SCREEN_H);
        reference.clear(SURFACE);
        text::draw(
            &mut reference,
            MARGIN_X,
            ROW_H_DISPLAY + (ROW_H_DISPLAY - cell_height(TypeRole::Display)) / 2,
            "..",
            TypeRole::Display,
            FG_DIM,
        );
        let adv = advance(TypeRole::Display);
        let cells = (release_ink_max + 1 - MARGIN_X).div_ceil(adv);
        assert!(
            cells >= 2,
            "even a truncated title keeps a character and the dots"
        );
        let y1 = ROW_H_DISPLAY + (ROW_H_DISPLAY - cell_height(TypeRole::Display)) / 2;
        for (cell, ref_cell) in [(cells - 2, 0u16), (cells - 1, 1u16)] {
            for y in 0..cell_height(TypeRole::Display) {
                for dx in 0..adv {
                    assert_eq!(
                        pixel(&actual, MARGIN_X + cell * adv + dx, y1 + y),
                        pixel(&reference, MARGIN_X + ref_cell * adv + dx, y1 + y),
                        "the release does not end with `..` (cell {cell}, offset {dx})"
                    );
                }
            }
        }

        // 4. The badges are byte-for-byte what they are over an empty release: a long title can
        //    never shift, clip or overwrite an icon.
        let mut empty = np_frame();
        np_header(
            &mut empty,
            NpHeader {
                title: "Title",
                artist: "Artist",
                release: "",
                count,
                modes,
            },
        );
        for y in ROW_H_DISPLAY..NP_HEADER_H {
            for x in badge_left..SCREEN_W {
                assert_eq!(
                    pixel(&actual, x, y),
                    pixel(&empty, x, y),
                    "badges/count differ at ({x}, {y}) because of the long release"
                );
            }
        }
    }

    #[test]
    fn np_header_artist_wins_over_long_title() {
        let long: String = "T".repeat(60);
        let artist = "The Artist";
        let w = SCREEN_W - MARGIN_X * 2;
        let max_title = w - measure(artist, TypeRole::Display) - SPACE_2;
        let mut fitted = String::new();
        for k in (1..=long.chars().count()).rev() {
            let prefix: String = long.chars().take(k).collect();
            let candidate = format!("{prefix}..");
            if measure(&candidate, TypeRole::Display) <= max_title {
                fitted = candidate;
                break;
            }
        }
        assert!(fitted.ends_with(".."), "long title is ellipsised");

        let mut actual = np_frame();
        np_header(
            &mut actual,
            NpHeader {
                title: &long,
                artist,
                release: "",
                count: "",
                modes: NpModeBadges::NONE,
            },
        );
        let mut expected = np_frame();
        np_header(
            &mut expected,
            NpHeader {
                title: &fitted,
                artist,
                release: "",
                count: "",
                modes: NpModeBadges::NONE,
            },
        );
        // Row 0 is pixel-identical to the fitted title render: the artist is intact and the
        // title is exactly the ellipsised prefix.
        for y in 5..25 {
            for x in 0..SCREEN_W {
                assert_eq!(
                    pixel(&actual, x, y),
                    pixel(&expected, x, y),
                    "row 0 differs at ({x}, {y})"
                );
            }
        }
        // The whole artist box is identical to a render with no title at all.
        let mut no_title = np_frame();
        np_header(
            &mut no_title,
            NpHeader {
                title: "",
                artist,
                release: "",
                count: "",
                modes: NpModeBadges::NONE,
            },
        );
        let artist_x = SCREEN_W - MARGIN_X - measure(artist, TypeRole::Display);
        for y in 5..25 {
            for x in artist_x..SCREEN_W {
                assert_eq!(
                    pixel(&actual, x, y),
                    pixel(&no_title, x, y),
                    "the artist must be intact at ({x}, {y})"
                );
            }
        }
        // No title glyph crosses into the artist's box.
        for y in 5..25 {
            for x in artist_x..SCREEN_W {
                let c = pixel(&actual, x, y);
                assert!(c == FG_DIM || c == SURFACE, "title ink crossed into the artist box");
            }
        }
    }

    #[test]
    fn np_header_count_wins_over_long_release() {
        let long: String = "A".repeat(60);
        let count = "10/12";
        let w = SCREEN_W - MARGIN_X * 2;
        let count_px = measure(count, TypeRole::Display) + SPACE_2;
        let max_release = w - count_px;
        // The fitted release is the longest prefix that still leaves room for `..`.
        let mut fitted: String = String::new();
        for k in (1..=long.chars().count()).rev() {
            let prefix: String = long.chars().take(k).collect();
            let candidate = format!("{}..", prefix);
            if measure(&candidate, TypeRole::Display) <= max_release {
                fitted = candidate;
                break;
            }
        }
        assert!(fitted.ends_with(".."), "long release is ellipsised");

        let mut actual = np_frame();
        np_header(
            &mut actual,
            NpHeader {
                title: "Title",
                artist: "Artist",
                release: &long,
                count,
                modes: NpModeBadges::NONE,
            },
        );
        let mut expected = np_frame();
        np_header(
            &mut expected,
            NpHeader {
                title: "Title",
                artist: "Artist",
                release: &fitted,
                count,
                modes: NpModeBadges::NONE,
            },
        );
        // Identical row 1 pixels: the count is painted last and never overdrawn, and the
        // release is exactly the ellipsised prefix.
        for y in 35..NP_HEADER_H {
            for x in 0..SCREEN_W {
                assert_eq!(
                    pixel(&actual, x, y),
                    pixel(&expected, x, y),
                    "row 1 differs at ({x}, {y})"
                );
            }
        }
        // The whole count box is identical to a render with no release at all.
        let mut no_release = np_frame();
        np_header(
            &mut no_release,
            NpHeader {
                title: "Title",
                artist: "Artist",
                release: "",
                count,
                modes: NpModeBadges::NONE,
            },
        );
        let count_x = SCREEN_W - MARGIN_X - measure(count, TypeRole::Display);
        for y in 35..NP_HEADER_H {
            for x in count_x..SCREEN_W {
                assert_eq!(
                    pixel(&actual, x, y),
                    pixel(&no_release, x, y),
                    "the count must be intact at ({x}, {y})"
                );
            }
        }
        // And no release glyph crosses into the count's box.
        for y in 35..NP_HEADER_H {
            for x in count_x..SCREEN_W {
                let c = pixel(&actual, x, y);
                assert!(
                    c == FG_DIM || c == SURFACE,
                    "release ink crossed into the count box"
                );
            }
        }
    }

    #[test]
    fn np_footer_layout() {
        let mut f = np_frame();
        np_footer(
            &mut f,
            NpFooter {
                left: "12:34 / 45:06",
                centre: "ANALYSING",
                right: "150% corr",
            },
        );
        let top = SCREEN_H - NP_FOOTER_H;
        assert_eq!(NP_FOOTER_H, ROW_H_DISPLAY);
        assert_eq!(pixel(&f, 0, top), BORDER_SOFT, "hairline at the band top");
        assert_eq!(pixel(&f, 0, top + 1), BG_ALT, "BG_ALT band");
        assert_eq!(pixel(&f, 0, top - 1), BG, "nothing above the band");
        // Left text starts at the margin; the right text ends at the right margin.
        let left_w = measure("12:34 / 45:06", TypeRole::Display);
        let left_ink = (MARGIN_X..MARGIN_X + left_w)
            .flat_map(|x| (top..SCREEN_H).map(move |y| (x, y)))
            .filter(|&(x, y)| pixel(&f, x, y) == FG)
            .count();
        assert!(left_ink > 20, "left slot is FG");
        let right_w = measure("150% corr", TypeRole::Display);
        let right_ink = (SCREEN_W - MARGIN_X - right_w..SCREEN_W - MARGIN_X)
            .flat_map(|x| (top..SCREEN_H).map(move |y| (x, y)))
            .filter(|&(x, y)| pixel(&f, x, y) == ACCENT)
            .count();
        assert!(right_ink > 20, "right slot is ACCENT speed");
        // With the spec's longest clock and a speed shown, the leftover box is only 68 px,
        // narrower than `ANALYSING` (90 px): suppress it rather than overpaint the clock.
        assert!(
            (0..SCREEN_W).all(|x| (top..SCREEN_H).all(|y| pixel(&f, x, y) != FG_DIM)),
            "no centre ink when the measured slots leave no room"
        );
    }

    /// B1: the `ANALYSING` centre may never overpaint the position/length clock. The current
    /// worst case is a frames clock (`12:34:56 / 45:06:29`, 19 `Display` glyphs = 190 px at
    /// x 8..198); a full-width centred `ANALYSING` would sit at 115..205 and overlap it, so
    /// the centre is placed in the gap right of the clock instead (or skipped).
    #[test]
    fn np_footer_centre_never_overlaps_the_clock() {
        let top = SCREEN_H - NP_FOOTER_H;
        let ink_columns = |f: &FrameBuffer, colour: Rgb565| -> Vec<u16> {
            (0..SCREEN_W)
                .filter(|&x| (top + 1..SCREEN_H).any(|y| pixel(f, x, y) == colour))
                .collect()
        };
        let clock = "12:34:56 / 45:06:29";

        // UI_14: a frames clock with no speed still leaves the ~90 px the centre needs; it is
        // drawn strictly right of the clock and can never overpaint it.
        let mut f = np_frame();
        np_footer(
            &mut f,
            NpFooter {
                left: clock,
                centre: "ANALYSING",
                right: "",
            },
        );
        let left = ink_columns(&f, FG);
        let centre = ink_columns(&f, FG_DIM);
        assert!(!left.is_empty(), "clock painted");
        assert!(!centre.is_empty(), "centre painted when there is room");
        assert!(
            centre[0] > *left.last().unwrap(),
            "ANALYSING starts at {} but the clock ends at {}",
            centre[0],
            left.last().unwrap()
        );
        assert!(
            *left.first().unwrap() >= MARGIN_X,
            "clock starts at the margin"
        );
        assert!(
            left.last().unwrap() < &(SCREEN_W - MARGIN_X),
            "clock stays inside the margin"
        );

        // The same clock with a speed: no room, so the centre is skipped and both side
        // slots stay intact.
        let mut f = np_frame();
        np_footer(
            &mut f,
            NpFooter {
                left: clock,
                centre: "ANALYSING",
                right: "150% corr",
            },
        );
        assert!(
            ink_columns(&f, FG_DIM).is_empty(),
            "the centre must not overpaint the clock for {clock:?} with a speed shown"
        );
        let left = ink_columns(&f, FG);
        let right = ink_columns(&f, ACCENT);
        assert!(!left.is_empty() && !right.is_empty(), "side slots intact");
        assert!(
            right[0] > *left.last().unwrap(),
            "the clock and the speed must not overlap"
        );

        // UI_14 addendum: with the unit Off the clock is the short `MM:SS` form, so the centre
        // fits again — and it still never overpaints either side slot.
        let mut f = np_frame();
        np_footer(
            &mut f,
            NpFooter {
                left: "1:05 / 3:00",
                centre: "ANALYSING",
                right: "-75% rev",
            },
        );
        let left = ink_columns(&f, FG);
        let centre = ink_columns(&f, FG_DIM);
        let right = ink_columns(&f, ACCENT);
        assert!(!left.is_empty(), "off clock painted");
        assert!(!centre.is_empty(), "the centre fits beside an MM:SS clock");
        assert!(!right.is_empty(), "speed painted");
        assert!(
            centre[0] > *left.last().unwrap(),
            "the centre must not overpaint the off clock"
        );
        assert!(
            centre.last().unwrap() < &right[0],
            "the centre must not overpaint the speed"
        );
    }
}
