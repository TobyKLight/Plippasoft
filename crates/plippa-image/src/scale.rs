use crate::{square_crop_rect, Rgb8};

/// Exact area-average (box) resample of the `sw` x `sh` rectangle at `(x0, y0)` of a packed
/// `ch`-channel 8-bit image of row width `stride` pixels into `dw` x `dh` cells.
///
/// Returns, per cell and channel, the *weighted sum* of source values; the average is that sum
/// divided by `sw * sh`. Integer arithmetic only, so results are identical on every platform.
/// Every source pixel contributes to a cell in proportion to the area they overlap.
pub(crate) fn box_sums(
    src: &[u8],
    stride: usize,
    ch: usize,
    (x0, y0, sw, sh): (usize, usize, usize, usize),
    (dw, dh): (usize, usize),
) -> Vec<u64> {
    let mut out = vec![0u64; dw * dh * ch];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    // Units: one source pixel is `dw` (resp. `dh`) wide, one cell is `sw` (resp. `sh`) wide.
    let overlap = |p: usize, cell: usize, src_len: usize, dst_len: usize| -> u64 {
        let lo = (p * dst_len).max(cell * src_len);
        let hi = ((p + 1) * dst_len).min((cell + 1) * src_len);
        hi.saturating_sub(lo) as u64
    };
    let span = |cell: usize, src_len: usize, dst_len: usize| -> (usize, usize) {
        let first = cell * src_len / dst_len;
        let last = (((cell + 1) * src_len).div_ceil(dst_len)).min(src_len);
        (first, last)
    };
    let mut row = vec![0u64; dw * ch];
    for cy in 0..dh {
        let (py0, py1) = span(cy, sh, dh);
        let acc = &mut out[cy * dw * ch..(cy + 1) * dw * ch];
        for py in py0..py1 {
            let wy = overlap(py, cy, sh, dh);
            if wy == 0 {
                continue;
            }
            let base = ((y0 + py) * stride + x0) * ch;
            let line = &src[base..base + sw * ch];
            for cx in 0..dw {
                let (px0, px1) = span(cx, sw, dw);
                let mut sums = [0u64; 4];
                for px in px0..px1 {
                    let wx = overlap(px, cx, sw, dw);
                    for c in 0..ch {
                        sums[c] += wx * line[px * ch + c] as u64;
                    }
                }
                for c in 0..ch {
                    row[cx * ch + c] = sums[c];
                }
            }
            for (a, r) in acc.iter_mut().zip(row.iter()) {
                *a += wy * *r;
            }
        }
    }
    out
}

/// Area-average resample of a whole RGB image to `dw` x `dh` (rounded to nearest).
pub(crate) fn box_resize(img: &Rgb8, dw: u32, dh: u32) -> Rgb8 {
    let sums = box_sums(
        &img.px,
        img.w as usize,
        3,
        (0, 0, img.w as usize, img.h as usize),
        (dw as usize, dh as usize),
    );
    finish(sums, img.w as u64 * img.h as u64, dw, dh)
}

fn finish(sums: Vec<u64>, area: u64, w: u32, h: u32) -> Rgb8 {
    let px = sums
        .into_iter()
        .map(|s| ((s + area / 2) / area.max(1)) as u8)
        .collect();
    Rgb8 { w, h, px }
}

/// Centre-crop to a square, then area-average (box filter) down to `side` x `side`.
///
/// Never upscales: if `side` is not smaller than the cropped square, the cropped square is
/// returned at its own size. A zero `side` or an empty image yields an empty image.
pub fn scale_square(img: &Rgb8, side: u32) -> Rgb8 {
    let (x, y, s) = square_crop_rect(img.w, img.h);
    if s == 0 || side == 0 || !img.is_consistent() {
        return Rgb8 {
            w: 0,
            h: 0,
            px: Vec::new(),
        };
    }
    let out = side.min(s);
    if out == s && s == img.w && s == img.h {
        return img.clone();
    }
    let sums = box_sums(
        &img.px,
        img.w as usize,
        3,
        (x as usize, y as usize, s as usize, s as usize),
        (out as usize, out as usize),
    );
    finish(sums, s as u64 * s as u64, out, out)
}

/// Packs RGB888 into RGB565, bit-identical to `bins/plippa-ui/src/display.rs::rgb565`.
pub fn to_rgb565(img: &Rgb8) -> Vec<u16> {
    img.px
        .chunks_exact(3)
        .map(|p| {
            (((p[0] as u16) & 0xf8) << 8) | (((p[1] as u16) & 0xfc) << 3) | ((p[2] as u16) >> 3)
        })
        .collect()
}
