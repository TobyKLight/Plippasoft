//! Content sniffing by magic bytes (`NBSecurityRequirements` §2.3, §5.2, §5.5).
//!
//! The kind of a downloaded / uploaded file is decided **only** from its first bytes. The URL
//! extension, the client's file name and the server's `Content-Type` are never trusted. Image
//! dimensions are read from the container header with bounds-checked byte parsing (no decoder,
//! nothing is decompressed), so a hostile image cannot exhaust memory here.

/// Audio containers Plippa plays (same set as `plippa_noubin::unite::SUPPORTED_AUDIO_EXTS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioKind {
    Mp3,
    Flac,
    Wav,
    Ogg,
    Opus,
    M4a,
    Aac,
}

impl AudioKind {
    /// Extension a file of this kind is stored under.
    pub fn ext(self) -> &'static str {
        match self {
            AudioKind::Mp3 => "mp3",
            AudioKind::Flac => "flac",
            AudioKind::Wav => "wav",
            AudioKind::Ogg => "ogg",
            AudioKind::Opus => "opus",
            AudioKind::M4a => "m4a",
            AudioKind::Aac => "aac",
        }
    }
}

/// Result of sniffing the head of a media file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSniff {
    Audio(AudioKind),
    /// Recognised but not playable (video, wma, ...): reported, never stored.
    Unsupported(&'static str),
    Unknown,
}

/// Bytes needed before [`sniff_audio`] can decide (an Ogg codec header sits at offset 28, an
/// `ftyp` compatible-brand list is short).
pub const SNIFF_HEAD_BYTES: usize = 64;

fn is_mp3_frame_header(b: &[u8]) -> bool {
    if b.len() < 4 || b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
        return false;
    }
    let version = (b[1] >> 3) & 3;
    let layer = (b[1] >> 1) & 3;
    let bitrate = b[2] >> 4;
    let rate = (b[2] >> 2) & 3;
    version != 1 && layer == 1 && bitrate != 0 && bitrate != 15 && rate != 3
}

fn is_adts_header(b: &[u8]) -> bool {
    // 12 sync bits, layer bits 00 (MP3 has 01), valid sampling index.
    b.len() >= 7 && b[0] == 0xFF && b[1] & 0xF6 == 0xF0 && (b[2] >> 2) & 0x0F < 13
}

/// Classify the first bytes of a media file. Pass at least [`SNIFF_HEAD_BYTES`] when the file
/// has that many.
pub fn sniff_audio(head: &[u8]) -> MediaSniff {
    if head.len() < 4 {
        return MediaSniff::Unknown;
    }
    if head.starts_with(b"fLaC") {
        return MediaSniff::Audio(AudioKind::Flac);
    }
    if head.starts_with(b"RIFF") && head.len() >= 12 {
        return match &head[8..12] {
            b"WAVE" => MediaSniff::Audio(AudioKind::Wav),
            b"AVI " => MediaSniff::Unsupported("AVI video"),
            _ => MediaSniff::Unknown,
        };
    }
    if head.starts_with(b"OggS") {
        let body = head.get(28..).unwrap_or(&[]);
        return if body.starts_with(b"OpusHead") {
            MediaSniff::Audio(AudioKind::Opus)
        } else if body.starts_with(b"\x01vorbis") {
            MediaSniff::Audio(AudioKind::Ogg)
        } else if body.starts_with(b"\x80theora") {
            MediaSniff::Unsupported("Ogg video")
        } else {
            MediaSniff::Unsupported("Ogg with an unsupported codec")
        };
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        // MP4-family: only an audio brand is accepted ("isom"/"mp42" alone may be video).
        let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let end = size.min(head.len());
        let compat: &[u8] = if end > 16 { &head[16..end] } else { &[] };
        let audio = std::iter::once(&head[8..12])
            .chain(compat.chunks_exact(4))
            .any(|b| matches!(b, b"M4A " | b"M4B " | b"M4P "));
        return if audio {
            MediaSniff::Audio(AudioKind::M4a)
        } else {
            MediaSniff::Unsupported("MP4 (video or unknown audio brand)")
        };
    }
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return MediaSniff::Unsupported("Matroska/WebM video");
    }
    if head.starts_with(&[0x30, 0x26, 0xB2, 0x75]) {
        return MediaSniff::Unsupported("Windows Media");
    }
    if head.starts_with(b"FLV\x01") {
        return MediaSniff::Unsupported("Flash video");
    }
    if head.starts_with(&[0, 0, 1, 0xBA]) {
        return MediaSniff::Unsupported("MPEG video");
    }
    if head.starts_with(b"FORM") && head.len() >= 12 && matches!(&head[8..12], b"AIFF" | b"AIFC") {
        return MediaSniff::Unsupported("AIFF audio");
    }
    if head.starts_with(b"ID3") {
        // ID3v2 tag in front of MP3 (the usual case). Version byte 2..=4 only.
        return if matches!(head[3], 2..=4) {
            MediaSniff::Audio(AudioKind::Mp3)
        } else {
            MediaSniff::Unknown
        };
    }
    if is_mp3_frame_header(head) {
        return MediaSniff::Audio(AudioKind::Mp3);
    }
    if is_adts_header(head) {
        return MediaSniff::Audio(AudioKind::Aac);
    }
    MediaSniff::Unknown
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

/// Image formats a player must support (§1.27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Jpeg,
    Png,
    Webp,
}

impl ImageKind {
    pub fn ext(self) -> &'static str {
        match self {
            ImageKind::Jpeg => "jpg",
            ImageKind::Png => "png",
            ImageKind::Webp => "webp",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Png => "image/png",
            ImageKind::Webp => "image/webp",
        }
    }
}

/// Header facts about an image; nothing was decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    pub kind: ImageKind,
    pub width: u32,
    pub height: u32,
}

impl ImageInfo {
    pub fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageError {
    /// Not JPEG / PNG / WebP by magic bytes.
    NotAnImage,
    /// Right magic bytes but the header is truncated or nonsensical.
    Malformed,
    /// A side is 0 or above the cap.
    Dimensions { width: u32, height: u32 },
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::NotAnImage => f.write_str("not a JPEG, PNG or WebP image"),
            ImageError::Malformed => f.write_str("the image header is damaged"),
            ImageError::Dimensions { width, height } => {
                write!(
                    f,
                    "image size {width}x{height} is outside the accepted range"
                )
            }
        }
    }
}

/// Identify the format from the magic bytes alone (first 12 bytes are enough).
pub fn sniff_image_kind(head: &[u8]) -> Option<ImageKind> {
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(ImageKind::Png)
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageKind::Jpeg)
    } else if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        Some(ImageKind::Webp)
    } else {
        None
    }
}

fn be16(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(u16::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
    ])))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    // Signature (8) + IHDR chunk: length 13, "IHDR", width, height.
    if be32(b, 8)? != 13 || b.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be32(b, 16)?, be32(b, 20)?))
}

fn jpeg_size(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    for _ in 0..4096 {
        // Skip fill bytes, find the next marker.
        while *b.get(i)? != 0xFF {
            i += 1;
        }
        while *b.get(i)? == 0xFF {
            i += 1;
        }
        let marker = *b.get(i)?;
        i += 1;
        match marker {
            0x00 | 0x01 | 0xD0..=0xD8 => continue,
            0xD9 | 0xDA => return None,
            _ => {}
        }
        let len = be16(b, i)? as usize;
        if len < 2 {
            return None;
        }
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            // length(2) precision(1) height(2) width(2)
            return Some((be16(b, i + 5)?, be16(b, i + 3)?));
        }
        i = i.checked_add(len)?;
    }
    None
}

fn webp_size(b: &[u8]) -> Option<(u32, u32)> {
    let chunk = b.get(12..16)?;
    let data = 20usize;
    match chunk {
        b"VP8 " => {
            // Frame tag (3 bytes), start code 9D 01 2A, then 14-bit width and height.
            if b.get(data + 3..data + 6)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            let w = u32::from(u16::from_le_bytes([*b.get(data + 6)?, *b.get(data + 7)?])) & 0x3FFF;
            let h = u32::from(u16::from_le_bytes([*b.get(data + 8)?, *b.get(data + 9)?])) & 0x3FFF;
            Some((w, h))
        }
        b"VP8L" => {
            if *b.get(data)? != 0x2F {
                return None;
            }
            let bits = u32::from_le_bytes([
                *b.get(data + 1)?,
                *b.get(data + 2)?,
                *b.get(data + 3)?,
                *b.get(data + 4)?,
            ]);
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        b"VP8X" => {
            let w =
                u32::from_le_bytes([*b.get(data + 4)?, *b.get(data + 5)?, *b.get(data + 6)?, 0]);
            let h =
                u32::from_le_bytes([*b.get(data + 7)?, *b.get(data + 8)?, *b.get(data + 9)?, 0]);
            Some((w + 1, h + 1))
        }
        _ => None,
    }
}

/// Identify an image and read its pixel size from the header. `max_side` is the accepted
/// width/height limit (5000 for cover art, §1.27). The whole file is only *scanned* for a JPEG
/// SOF marker; pixel data is never decoded.
pub fn sniff_image(bytes: &[u8], max_side: u32) -> Result<ImageInfo, ImageError> {
    let kind = sniff_image_kind(bytes).ok_or(ImageError::NotAnImage)?;
    let (width, height) = match kind {
        ImageKind::Png => png_size(bytes),
        ImageKind::Jpeg => jpeg_size(bytes),
        ImageKind::Webp => webp_size(bytes),
    }
    .ok_or(ImageError::Malformed)?;
    if width == 0 || height == 0 || width > max_side || height > max_side {
        return Err(ImageError::Dimensions { width, height });
    }
    Ok(ImageInfo {
        kind,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 2, 0, 0, 0]);
        v
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 16];
        v.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        v.extend_from_slice(&[0xFF, 0xC0, 0, 11, 8]);
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[1, 1, 0x11, 0]);
        v
    }

    fn webp_vp8x(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(b"WEBPVP8X");
        v.extend_from_slice(&[10, 0, 0, 0, 0, 0, 0, 0]);
        v.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
        v.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
        v
    }

    fn webp_vp8l(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(b"WEBPVP8L");
        v.extend_from_slice(&[5, 0, 0, 0, 0x2F]);
        v.extend_from_slice(&((w - 1) | ((h - 1) << 14)).to_le_bytes());
        v
    }

    fn webp_vp8(w: u16, h: u16) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]);
        v.extend_from_slice(b"WEBPVP8 ");
        v.extend_from_slice(&[10, 0, 0, 0, 0, 0, 0, 0x9D, 0x01, 0x2A]);
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v
    }

    #[test]
    fn unite_sniff_images_read_dimensions_from_headers() {
        let p = sniff_image(&png(640, 480), 5000).unwrap();
        assert_eq!((p.kind, p.width, p.height), (ImageKind::Png, 640, 480));
        let j = sniff_image(&jpeg(1200, 900), 5000).unwrap();
        assert_eq!((j.kind, j.width, j.height), (ImageKind::Jpeg, 1200, 900));
        for w in [webp_vp8x(800, 600), webp_vp8l(800, 600), webp_vp8(800, 600)] {
            let i = sniff_image(&w, 5000).unwrap();
            assert_eq!((i.kind, i.width, i.height), (ImageKind::Webp, 800, 600));
        }
    }

    #[test]
    fn unite_sniff_images_reject_oversize_zero_truncated_and_non_images() {
        assert_eq!(
            sniff_image(&png(5001, 10), 5000),
            Err(ImageError::Dimensions {
                width: 5001,
                height: 10
            })
        );
        assert!(sniff_image(&png(5000, 5000), 5000).is_ok());
        // Decompression-bomb shaped header: 60000 x 60000.
        assert!(matches!(
            sniff_image(&png(60000, 60000), 5000),
            Err(ImageError::Dimensions { .. })
        ));
        assert!(matches!(
            sniff_image(&png(0, 10), 5000),
            Err(ImageError::Dimensions { .. })
        ));
        assert_eq!(
            sniff_image(&png(1, 1)[..14], 5000),
            Err(ImageError::Malformed)
        );
        assert_eq!(
            sniff_image(&[0xFF, 0xD8, 0xFF], 5000),
            Err(ImageError::Malformed)
        );
        assert_eq!(sniff_image(b"<html>", 5000), Err(ImageError::NotAnImage));
        assert_eq!(
            sniff_image(b"GIF89a....", 5000),
            Err(ImageError::NotAnImage)
        );
        // JPEG marker walk terminates on garbage / never reads out of bounds.
        let mut junk = vec![0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF];
        junk.extend(std::iter::repeat_n(0xFF, 100));
        assert!(sniff_image(&junk, 5000).is_err());
        // SVG or HTML with an image extension is simply not an image.
        assert_eq!(
            sniff_image(b"<svg xmlns='http://www.w3.org/2000/svg'/>", 5000),
            Err(ImageError::NotAnImage)
        );
    }

    fn head(mut v: Vec<u8>) -> Vec<u8> {
        v.resize(SNIFF_HEAD_BYTES.max(v.len()), 0);
        v
    }

    #[test]
    fn unite_sniff_audio_by_magic_not_extension() {
        assert_eq!(
            sniff_audio(&head(b"fLaC\0\0\0\"".to_vec())),
            MediaSniff::Audio(AudioKind::Flac)
        );
        assert_eq!(
            sniff_audio(&head(b"RIFF\x24\0\0\0WAVEfmt ".to_vec())),
            MediaSniff::Audio(AudioKind::Wav)
        );
        assert_eq!(
            sniff_audio(&head(b"ID3\x03\0\0\0\0\0\0".to_vec())),
            MediaSniff::Audio(AudioKind::Mp3)
        );
        assert_eq!(
            sniff_audio(&head(vec![0xFF, 0xFB, 0x90, 0x64])),
            MediaSniff::Audio(AudioKind::Mp3)
        );
        assert_eq!(
            sniff_audio(&head(vec![0xFF, 0xF1, 0x50, 0x80, 0, 0x1F, 0xFC])),
            MediaSniff::Audio(AudioKind::Aac)
        );
        let mut ogg = b"OggS".to_vec();
        ogg.resize(28, 0);
        let mut opus = ogg.clone();
        opus.extend_from_slice(b"OpusHead");
        let mut vorbis = ogg.clone();
        vorbis.extend_from_slice(b"\x01vorbis");
        assert_eq!(sniff_audio(&head(opus)), MediaSniff::Audio(AudioKind::Opus));
        assert_eq!(
            sniff_audio(&head(vorbis)),
            MediaSniff::Audio(AudioKind::Ogg)
        );
        let mut m4a = vec![0, 0, 0, 0x18];
        m4a.extend_from_slice(b"ftypM4A \0\0\0\0M4A mp42");
        assert_eq!(sniff_audio(&head(m4a)), MediaSniff::Audio(AudioKind::M4a));
        // Video and look-alikes are refused.
        let mut mp4 = vec![0, 0, 0, 0x18];
        mp4.extend_from_slice(b"ftypisom\0\0\0\0isommp42");
        assert!(matches!(
            sniff_audio(&head(mp4)),
            MediaSniff::Unsupported(_)
        ));
        assert!(matches!(
            sniff_audio(&head(vec![0x1A, 0x45, 0xDF, 0xA3])),
            MediaSniff::Unsupported(_)
        ));
        assert!(matches!(
            sniff_audio(&head(b"RIFF\0\0\0\0AVI LIST".to_vec())),
            MediaSniff::Unsupported(_)
        ));
        for junk in [
            &b"<!doctype html><html>"[..],
            b"MZ\x90\0\x03\0\0\0",
            b"#!/bin/sh\necho hi\n",
            b"{\"json\": true}",
            b"PK\x03\x04",
            b"",
            b"ab",
        ] {
            assert_eq!(
                sniff_audio(&head(junk.to_vec())),
                MediaSniff::Unknown,
                "{junk:?}"
            );
        }
        // A short ftyp box must not index out of bounds.
        assert!(matches!(
            sniff_audio(b"\0\0\0\x08ftyp"),
            MediaSniff::Unknown | MediaSniff::Unsupported(_)
        ));
    }
}
