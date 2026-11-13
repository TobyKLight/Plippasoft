//! Hostile media fixtures built in code (no binary blobs in git): the three cases from the
//! Nov 2026 review (PAR-01), each a few hundred bytes that make Symphonia 0.5.5 ask the
//! allocator for gigabytes because a length field says so. A failed allocation is `abort()`,
//! which no `catch_unwind` stops, so the process that opens such a file dies.
//!
//! Also here: an oversized-allocation case for each supported format that has one, and the
//! helpers the child-process tests share.

use std::path::{Path, PathBuf};

/// One hostile file: the name a stick would carry and its bytes.
#[derive(Debug, Clone)]
pub struct HostileFile {
    pub name: &'static str,
    pub bytes: Vec<u8>,
}

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len());
    v.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

/// A tiny MP4 (named `.mp3`) whose `stts` atom claims `0xFFFF_FFF0` entries: the old reader
/// did `Vec::with_capacity(entry_count)`. The MP4 reader is no longer compiled in, so on the
/// trimmed build this is simply an unrecognised file; it is kept because it is the review's
/// proof file and must stay harmless whatever readers a future build links.
pub fn mp4_with_huge_stts() -> Vec<u8> {
    let mut ftyp = b"isom\0\0\0\0isom".to_vec();
    ftyp.extend_from_slice(b"mp42");
    let mut stts = vec![0u8; 4]; // version + flags
    stts.extend_from_slice(&0xFFFF_FFF0u32.to_be_bytes()); // entry_count
    let stbl = atom(b"stbl", &atom(b"stts", &stts));
    let mdhd = atom(b"mdhd", &[0u8; 24]);
    let mut hdlr_body = vec![0u8; 8];
    hdlr_body.extend_from_slice(b"soun");
    hdlr_body.extend_from_slice(&[0u8; 13]);
    let hdlr = atom(b"hdlr", &hdlr_body);
    let minf = atom(b"minf", &stbl);
    let mdia = atom(b"mdia", &[mdhd, hdlr, minf].concat());
    let tkhd = atom(b"tkhd", &[0u8; 84]);
    let trak = atom(b"trak", &[tkhd, mdia].concat());
    let moov = atom(b"moov", &trak);
    [atom(b"ftyp", &ftyp), moov].concat()
}

/// A tiny FLAC whose Vorbis comment block holds one comment of length `0xFFFF_FFF0`.
pub fn flac_with_huge_vorbis_comment() -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    // STREAMINFO (not last): 4096/4096 block size, no frame sizes, 44.1 kHz, 2 ch, 16 bit.
    v.extend_from_slice(&[0x00, 0x00, 0x00, 34]);
    v.extend_from_slice(&4096u16.to_be_bytes());
    v.extend_from_slice(&4096u16.to_be_bytes());
    v.extend_from_slice(&[0; 6]);
    let packed: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 44_100;
    v.extend_from_slice(&packed.to_be_bytes());
    v.extend_from_slice(&[0; 16]);
    // VORBIS_COMMENT (last): vendor length 0, one comment, comment length huge.
    let mut body = Vec::new();
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    v.extend_from_slice(&[0x84, 0, 0, body.len() as u8]);
    v.extend_from_slice(&body);
    v
}

/// A tiny MP3 whose ID3v2.3 `TIT2` frame declares `0xFFFF_FF00` bytes.
pub fn mp3_with_huge_id3_frame() -> Vec<u8> {
    let mut frame = b"TIT2".to_vec();
    frame.extend_from_slice(&0xFFFF_FF00u32.to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&[0u8; 20]);
    let mut v = b"ID3\x03\0\0".to_vec();
    v.extend_from_slice(&[0, 0, 0, frame.len() as u8]); // syncsafe tag size (< 128)
    v.extend_from_slice(&frame);
    v.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    v.extend_from_slice(&[0u8; 60]);
    v
}

/// The fixtures every hostile-media test runs: the three review cases.
pub fn review_cases() -> Vec<HostileFile> {
    vec![
        HostileFile { name: "mp4_stts.mp3", bytes: mp4_with_huge_stts() },
        HostileFile { name: "flac_vorbis.flac", bytes: flac_with_huge_vorbis_comment() },
        HostileFile { name: "mp3_id3.mp3", bytes: mp3_with_huge_id3_frame() },
    ]
}

/// Write `files` into `dir` and return their paths.
pub fn write_all(dir: &Path, files: &[HostileFile]) -> Vec<PathBuf> {
    files
        .iter()
        .map(|f| {
            let p = dir.join(f.name);
            std::fs::write(&p, &f.bytes).expect("write hostile fixture");
            p
        })
        .collect()
}
