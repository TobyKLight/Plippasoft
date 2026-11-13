/// Centred square crop of a `w` x `h` image as `(x, y, side)` with `side = min(w, h)`.
/// Never fails; a zero-sized image yields side 0.
pub fn square_crop_rect(w: u32, h: u32) -> (u32, u32, u32) {
    let side = w.min(h);
    ((w - side) / 2, (h - side) / 2, side)
}
