use crate::scale::box_resize;
use crate::{ImageError, Rgb8};
use jpeg_encoder::{ColorType, Encoder};

const QUALITY: u8 = 82;

/// Encodes `img` as a baseline JPEG (quality 82) whose longer side is `max_side`, keeping the
/// aspect ratio (thumbnails are not cropped). Never upscales: an image already within
/// `max_side` is encoded at its own size.
pub fn encode_jpeg_fit(img: &Rgb8, max_side: u32) -> Result<Vec<u8>, ImageError> {
    if img.w == 0 || img.h == 0 || max_side == 0 || !img.is_consistent() {
        return Err(ImageError::Corrupt("empty or inconsistent image".into()));
    }
    let longest = img.w.max(img.h);
    let (w, h) = if longest <= max_side {
        (img.w, img.h)
    } else {
        let fit = |side: u32| -> u32 {
            ((side as u64 * max_side as u64 + longest as u64 / 2) / longest as u64).max(1) as u32
        };
        (fit(img.w), fit(img.h))
    };
    if w > u16::MAX as u32 || h > u16::MAX as u32 {
        return Err(ImageError::TooLarge {
            width: w,
            height: h,
            bytes: 0,
        });
    }
    let mut out = Vec::new();
    let encoder = Encoder::new(&mut out, QUALITY);
    let result = if (w, h) == (img.w, img.h) {
        encoder.encode(&img.px, w as u16, h as u16, ColorType::Rgb)
    } else {
        let scaled = box_resize(img, w, h);
        encoder.encode(&scaled.px, w as u16, h as u16, ColorType::Rgb)
    };
    result.map_err(|e| ImageError::Corrupt(e.to_string()))?;
    Ok(out)
}
