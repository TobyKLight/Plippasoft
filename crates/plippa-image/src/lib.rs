//! Cover-art image toolkit: header probing, bounded decode (JPEG, PNG, WebP), centred square
//! crop, deterministic box-filter scaling, RGB565 packing, a 64-bit difference hash and JPEG
//! thumbnail encoding.
//!
//! Everything here is a pure function over byte slices and pixel buffers: no file or network IO,
//! no threads, no global state. Callers decide where to run it (server worker, UI loader thread).
//! This crate is a leaf of the workspace and treats its input as hostile: dimensions are checked
//! from the headers before any pixel buffer exists and decoder panics are mapped to errors.
//!
//! Animated WebP and APNG inputs decode their first frame only.
#![forbid(unsafe_code)]

mod crop;
mod decode;
mod hash;
mod jpeg;
mod scale;

pub use crop::square_crop_rect;
pub use decode::{decode, probe};
pub use hash::{dhash64, hamming};
pub use jpeg::encode_jpeg_fit;
pub use scale::{scale_square, to_rgb565};

use std::fmt;

/// Upper bounds applied to an image before it is decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLimits {
    /// Longest permitted side in pixels.
    pub max_side: u32,
    /// Permitted width x height.
    pub max_pixels: u64,
    /// Permitted encoded size in bytes.
    pub max_bytes: u64,
}

impl Default for ImageLimits {
    /// Noubin cover-art limits: 5000 px per side, 25 Mpx, 16 MiB.
    fn default() -> Self {
        Self {
            max_side: 5000,
            max_pixels: 25_000_000,
            max_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Container formats accepted as cover art.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageKind {
    Jpeg,
    Png,
    Webp,
}

/// Why an image was refused or could not be processed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageError {
    /// Not JPEG/PNG/WebP, or a variant of those the toolkit does not decode
    /// (CMYK/YCCK JPEG, 12-bit or lossless JPEG, ...).
    Unsupported,
    /// Over a limit. `width`/`height` are 0 when the size check failed before they were read.
    TooLarge { width: u32, height: u32, bytes: u64 },
    /// The data ends before the headers or pixel data are complete.
    Truncated,
    /// The data is malformed (or a decoder rejected it).
    Corrupt(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Unsupported => write!(f, "unsupported image format"),
            ImageError::TooLarge {
                width,
                height,
                bytes,
            } => write!(f, "image too large ({width}x{height}, {bytes} bytes)"),
            ImageError::Truncated => write!(f, "image data is truncated"),
            ImageError::Corrupt(why) => write!(f, "image data is corrupt: {why}"),
        }
    }
}

impl std::error::Error for ImageError {}

/// Tightly packed 8-bit RGB pixels, row-major (`px.len() == w * h * 3`). Alpha, where the
/// source has it, is composited on black.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgb8 {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

impl Rgb8 {
    /// True when the buffer length matches the dimensions.
    pub fn is_consistent(&self) -> bool {
        (self.w as u64)
            .checked_mul(self.h as u64)
            .and_then(|n| n.checked_mul(3))
            == Some(self.px.len() as u64)
    }
}
