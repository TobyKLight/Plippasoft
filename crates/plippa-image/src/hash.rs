use crate::scale::box_sums;
use crate::{square_crop_rect, Rgb8};

/// Cells must differ by more than this many luma levels (of 255) for a bit to be set, so flat
/// areas and compression noise do not flip bits.
const MARGIN_LEVELS: u64 = 2;

/// 64-bit difference hash of the centred square of `img`, reduced to 9 x 8 luma cells. Bit
/// `y * 8 + x` is set when cell `(x, y)` is brighter than cell `(x + 1, y)` by more than
/// [`MARGIN_LEVELS`] luma levels.
///
/// Computed with exact integer area averages, so it is identical on every platform. An empty
/// or inconsistent image hashes to 0.
pub fn dhash64(img: &Rgb8) -> u64 {
    let (x, y, s) = square_crop_rect(img.w, img.h);
    if s == 0 || !img.is_consistent() {
        return 0;
    }
    // Luma is linear in the channels, so the luma of the reduced colour equals the reduced luma;
    // every cell shares one divisor, so the unnormalised sums compare directly.
    let sums = box_sums(
        &img.px,
        img.w as usize,
        3,
        (x as usize, y as usize, s as usize, s as usize),
        (9, 8),
    );
    let luma = |cx: usize, cy: usize| -> u64 {
        let i = (cy * 9 + cx) * 3;
        299 * sums[i] + 587 * sums[i + 1] + 114 * sums[i + 2]
    };
    // Luma sums carry a factor 1000 (the 299/587/114 weights) and the divisor `s * s`.
    let margin = MARGIN_LEVELS * 1000 * (s as u64) * (s as u64);
    let mut hash = 0u64;
    for cy in 0..8 {
        for cx in 0..8 {
            if luma(cx, cy) > luma(cx + 1, cy) + margin {
                hash |= 1 << (cy * 8 + cx);
            }
        }
    }
    hash
}

/// Number of differing bits between two hashes.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}
