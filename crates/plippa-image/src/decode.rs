use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};

use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::zune_core::result::DecodingResult;

use crate::{ImageError, ImageKind, ImageLimits, Rgb8};

type Probed = (ImageKind, u32, u32);

fn corrupt(why: &str) -> ImageError {
    ImageError::Corrupt(why.to_string())
}

fn be16(b: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]) as u32)
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    Some(*b.get(at)? as u32 | (*b.get(at + 1)? as u32) << 8 | (*b.get(at + 2)? as u32) << 16)
}

fn jpeg_header(b: &[u8]) -> Result<(u32, u32), ImageError> {
    let mut at = 2;
    loop {
        // Skip fill bytes up to the next marker.
        while *b.get(at).ok_or(ImageError::Truncated)? != 0xFF {
            at += 1;
        }
        while *b.get(at + 1).ok_or(ImageError::Truncated)? == 0xFF {
            at += 1;
        }
        let marker = b[at + 1];
        at += 2;
        match marker {
            0x00 => return Err(corrupt("stray byte stuffing before frame header")),
            0x01 | 0xD0..=0xD7 => continue,
            0xD8 => return Err(corrupt("nested start of image")),
            0xD9 | 0xDA => return Err(corrupt("no frame header")),
            _ => {}
        }
        let len = be16(b, at).ok_or(ImageError::Truncated)? as usize;
        if len < 2 {
            return Err(corrupt("bad segment length"));
        }
        match marker {
            // Baseline, extended sequential and progressive Huffman DCT.
            0xC0..=0xC2 => {
                if len < 8 {
                    return Err(corrupt("short frame header"));
                }
                let precision = *b.get(at + 2).ok_or(ImageError::Truncated)?;
                let h = be16(b, at + 3).ok_or(ImageError::Truncated)?;
                let w = be16(b, at + 5).ok_or(ImageError::Truncated)?;
                let comps = *b.get(at + 7).ok_or(ImageError::Truncated)?;
                if precision != 8 || !matches!(comps, 1 | 3) {
                    // 12-bit, CMYK/YCCK and other layouts: refused explicitly.
                    return Err(ImageError::Unsupported);
                }
                if w == 0 || h == 0 {
                    return Err(corrupt("zero image dimension"));
                }
                return Ok((w, h));
            }
            // Lossless, hierarchical and arithmetic-coded frames.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => return Err(ImageError::Unsupported),
            _ => at += len,
        }
    }
}

/// True when the marker stream reaches an end-of-image marker. The JPEG decoder fills in
/// missing data instead of failing, so a cut-off file would otherwise decode as a partly grey
/// picture. Bytes after the end-of-image marker are ignored.
fn jpeg_is_complete(b: &[u8]) -> bool {
    let mut at = 2;
    while at + 1 < b.len() {
        if b[at] != 0xFF {
            at += 1;
            continue;
        }
        match b[at + 1] {
            0xFF => at += 1,
            0xD9 => return true,
            0x00 | 0x01 | 0xD0..=0xD7 => at += 2,
            _ => match be16(b, at + 2) {
                // Skip the segment body; entropy-coded data after a scan header is walked
                // byte by byte above (its stuffed 0xFF 0x00 and restart markers are skipped).
                Some(len) => at += 2 + (len as usize).max(2),
                None => return false,
            },
        }
    }
    false
}

fn png_header(b: &[u8]) -> Result<(u32, u32), ImageError> {
    if b.len() < 33 {
        return Err(ImageError::Truncated);
    }
    if &b[12..16] != b"IHDR" || b[8..12] != 13u32.to_be_bytes() {
        return Err(corrupt("first chunk is not IHDR"));
    }
    let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
    let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
    let (depth, color) = (b[24], b[25]);
    let depth_ok = match color {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        2 | 4 | 6 => matches!(depth, 8 | 16),
        _ => false,
    };
    if !depth_ok {
        return Err(corrupt("invalid PNG bit depth / colour type"));
    }
    if w == 0 || h == 0 || w > i32::MAX as u32 || h > i32::MAX as u32 {
        return Err(corrupt("invalid PNG dimensions"));
    }
    Ok((w, h))
}

fn webp_header(b: &[u8]) -> Result<(u32, u32), ImageError> {
    if b.len() < 30 {
        return Err(ImageError::Truncated);
    }
    let (w, h) = match &b[12..16] {
        b"VP8 " => {
            if b[23..26] != [0x9D, 0x01, 0x2A] {
                return Err(corrupt("bad VP8 start code"));
            }
            (
                u16::from_le_bytes([b[26], b[27]]) as u32 & 0x3FFF,
                u16::from_le_bytes([b[28], b[29]]) as u32 & 0x3FFF,
            )
        }
        b"VP8L" => {
            if b[20] != 0x2F {
                return Err(corrupt("bad VP8L signature"));
            }
            let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
            ((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
        }
        b"VP8X" => (
            le24(b, 24).ok_or(ImageError::Truncated)? + 1,
            le24(b, 27).ok_or(ImageError::Truncated)? + 1,
        ),
        _ => return Err(ImageError::Unsupported),
    };
    if w == 0 || h == 0 {
        return Err(corrupt("zero image dimension"));
    }
    Ok((w, h))
}

/// Header-only inspection: container kind and pixel size. Bounds-checked, nothing is
/// decompressed, and an image over `limits` is refused before any pixel buffer exists.
pub fn probe(bytes: &[u8], limits: &ImageLimits) -> Result<(ImageKind, u32, u32), ImageError> {
    if bytes.len() as u64 > limits.max_bytes {
        return Err(ImageError::TooLarge {
            width: 0,
            height: 0,
            bytes: bytes.len() as u64,
        });
    }
    let (kind, w, h): Probed = if bytes.starts_with(&[0xFF, 0xD8]) {
        let (w, h) = jpeg_header(bytes)?;
        (ImageKind::Jpeg, w, h)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let (w, h) = png_header(bytes)?;
        (ImageKind::Png, w, h)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        let (w, h) = webp_header(bytes)?;
        (ImageKind::Webp, w, h)
    } else {
        return Err(ImageError::Unsupported);
    };
    if w > limits.max_side || h > limits.max_side || (w as u64) * (h as u64) > limits.max_pixels {
        return Err(ImageError::TooLarge {
            width: w,
            height: h,
            bytes: bytes.len() as u64,
        });
    }
    Ok((kind, w, h))
}

/// Decodes a JPEG, PNG or WebP into packed RGB. Alpha is composited on black, 16-bit samples
/// are reduced to 8, animated inputs yield their first frame. The size limits are enforced from
/// the headers first, so a decompression bomb never allocates. A panic inside a decoder on
/// hostile input is reported as [`ImageError::Corrupt`].
pub fn decode(bytes: &[u8], limits: &ImageLimits) -> Result<Rgb8, ImageError> {
    let (kind, w, h) = probe(bytes, limits)?;
    let decoded = catch_unwind(AssertUnwindSafe(|| match kind {
        ImageKind::Jpeg => decode_jpeg(bytes, limits),
        ImageKind::Png => decode_png(bytes, limits, w, h),
        ImageKind::Webp => decode_webp(bytes, limits),
    }))
    .unwrap_or_else(|_| Err(corrupt("decoder panicked on malformed input")))?;
    let expected = (w as usize) * (h as usize) * 3;
    if decoded.w != w || decoded.h != h || decoded.px.len() != expected {
        return Err(corrupt("decoded size differs from header"));
    }
    Ok(decoded)
}

fn decode_jpeg(bytes: &[u8], limits: &ImageLimits) -> Result<Rgb8, ImageError> {
    if !jpeg_is_complete(bytes) {
        return Err(ImageError::Truncated);
    }
    let opts = DecoderOptions::default()
        .set_strict_mode(true)
        .set_max_width(limits.max_side as usize)
        .set_max_height(limits.max_side as usize)
        .jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), opts);
    let px = dec.decode().map_err(|e| classify(&format!("{e:?}")))?;
    let info = dec.info().ok_or_else(|| corrupt("missing JPEG info"))?;
    Ok(Rgb8 {
        w: info.width as u32,
        h: info.height as u32,
        px,
    })
}

/// Largest inflated PNG scanline data (and so working set) accepted: a 5000x5000 8-bit RGBA
/// image (100 MB) fits; a 25 Mpx 16-bit RGBA image (200 MB) does not.
const PNG_RAW_CAP: usize = 128 * 1024 * 1024;

/// Upper bound on the inflated size of a PNG's scanlines, from the real bits per pixel in the
/// header. Interlaced images get slack for the extra per-pass filter bytes and row padding.
fn png_raw_bound(bytes: &[u8], w: u32, h: u32) -> usize {
    let channels: usize = match bytes[25] {
        0 | 3 => 1,
        4 => 2,
        2 => 3,
        _ => 4,
    };
    let bits_per_pixel = channels * bytes[24] as usize;
    let row = (w as usize).saturating_mul(bits_per_pixel).div_ceil(8);
    let rows = h as usize;
    let slack = if bytes[28] == 1 { 8 * rows } else { 0 };
    row.saturating_add(1)
        .saturating_mul(rows)
        .saturating_add(slack)
        .saturating_add(1024)
}

fn decode_png(bytes: &[u8], limits: &ImageLimits, w: u32, h: u32) -> Result<Rgb8, ImageError> {
    let raw_cap = png_raw_bound(bytes, w, h);
    if raw_cap > PNG_RAW_CAP {
        return Err(ImageError::TooLarge {
            width: w,
            height: h,
            bytes: bytes.len() as u64,
        });
    }
    let opts = DecoderOptions::default()
        .png_set_strip_to_8bit(true)
        .set_max_width(limits.max_side as usize)
        .set_max_height(limits.max_side as usize)
        .inflate_set_limit(raw_cap);
    let mut dec = zune_png::PngDecoder::new_with_options(ZCursor::new(bytes), opts);
    dec.decode_headers()
        .map_err(|e| classify(&format!("{e:?}")))?;
    let space = dec
        .colorspace()
        .ok_or_else(|| corrupt("missing PNG colour info"))?;
    let mut data = match dec.decode().map_err(|e| classify(&format!("{e:?}")))? {
        DecodingResult::U8(v) => v,
        _ => return Err(corrupt("unexpected PNG sample type")),
    };
    drop(dec);
    let px = match space {
        ColorSpace::Luma => {
            let mut px = Vec::with_capacity(data.len() * 3);
            data.iter().for_each(|&l| px.extend_from_slice(&[l, l, l]));
            px
        }
        ColorSpace::LumaA => {
            let mut px = Vec::with_capacity(data.len() / 2 * 3);
            data.chunks_exact(2).for_each(|p| {
                let l = on_black(p[0], p[1]);
                px.extend_from_slice(&[l, l, l]);
            });
            px
        }
        ColorSpace::RGB => data,
        ColorSpace::RGBA => {
            // Compact in place (the write index never passes the read index), so the working
            // set stays at one buffer.
            let n = data.len() / 4;
            for i in 0..n {
                let (r, g, b, a) = (
                    data[4 * i],
                    data[4 * i + 1],
                    data[4 * i + 2],
                    data[4 * i + 3],
                );
                data[3 * i] = on_black(r, a);
                data[3 * i + 1] = on_black(g, a);
                data[3 * i + 2] = on_black(b, a);
            }
            data.truncate(n * 3);
            data
        }
        _ => return Err(ImageError::Unsupported),
    };
    Ok(Rgb8 { w, h, px })
}

fn decode_webp(bytes: &[u8], limits: &ImageLimits) -> Result<Rgb8, ImageError> {
    let mut dec = image_webp::WebPDecoder::new(Cursor::new(bytes))
        .map_err(|e| classify(&format!("{e:?}")))?;
    dec.set_memory_limit((limits.max_pixels as usize).saturating_mul(8));
    let (w, h) = dec.dimensions();
    let size = dec
        .output_buffer_size()
        .ok_or_else(|| corrupt("WebP size overflow"))?;
    let mut buf = vec![0u8; size];
    dec.read_image(&mut buf)
        .map_err(|e| classify(&format!("{e:?}")))?;
    let px = if dec.has_alpha() {
        let mut px = Vec::with_capacity(size / 4 * 3);
        for p in buf.chunks_exact(4) {
            px.extend_from_slice(&[
                on_black(p[0], p[3]),
                on_black(p[1], p[3]),
                on_black(p[2], p[3]),
            ]);
        }
        px
    } else {
        buf
    };
    Ok(Rgb8 { w, h, px })
}

/// `c` composited over black with opacity `a`, rounded.
fn on_black(c: u8, a: u8) -> u8 {
    ((c as u32 * a as u32 + 127) / 255) as u8
}

/// Decoder errors are reported as truncation when they say the data ran out, otherwise as
/// corruption; the decoder's own text is kept only as a diagnostic.
fn classify(debug: &str) -> ImageError {
    let lower = debug.to_ascii_lowercase();
    if [
        "eof",
        "truncat",
        "unexpectedend",
        "not enough",
        "premature",
        "endofdata",
        "insufficient",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        ImageError::Truncated
    } else {
        ImageError::Corrupt(debug.chars().take(200).collect())
    }
}
