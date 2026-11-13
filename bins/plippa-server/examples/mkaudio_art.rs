//! Browser-test fixtures with embedded / loose cover art (ART_07 `addmedia_art.mjs`).
//! Python's `mutagen` is not guaranteed to exist, so the tagged audio is written with lofty.
//!
//! `cargo run -p plippa-server --example mkaudio_art -- <dir>` writes under `<dir>/art/`:
//!   set12/t01..t10.mp3       the same 600 px front cover (pic A) in 10 files
//!   set12/t11,t12.mp3        a different back cover (pic B)
//!   sizes/small.mp3, large.mp3  the same picture (C) at 600 px and 1000 px
//!   formats/pic.flac, covr.m4a  FLAC PICTURE and MP4 `covr` (pic A)
//!   solo/solo.mp3            one MP3 with a front cover (pic D), title only (becomes a loose track)
//!   hostile/hostile.mp3      hostile tag text and an embedded GIF (unsupported)
//!   plain/p1.mp3, p2.mp3     tagged, no art (p2 embeds pic B for the "remove audio row" check)
//!   loose/front.jpg, back.jpg, wide.jpg (500 x 250, not square), notimage.jpg (text)
//! (`mkaudio.py` adds loose/back.png and loose/wide.png; the two scripts can run in either order.)
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::{Tag, TagType};
use plippa_image::{encode_jpeg_fit, Rgb8};
use std::fs;
use std::path::Path;

/// 9 x 8 pseudo-random grey cells scaled to `w` x `h`: every size of one seed is one picture.
fn blocks(seed: u32, w: u32, h: u32) -> Vec<u8> {
    let mut cell = [0u8; 72];
    let mut s = seed.wrapping_mul(2654435761).wrapping_add(12345);
    for c in cell.iter_mut() {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        *c = (s >> 24) as u8;
    }
    let mut px = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let v = cell[((y * 8 / h) * 9 + x * 9 / w) as usize];
            px.extend_from_slice(&[v, v, v]);
        }
    }
    encode_jpeg_fit(&Rgb8 { w, h, px }, w.max(h)).expect("encode")
}

fn pic(t: PictureType, mime: MimeType, data: &[u8]) -> Picture {
    Picture::new_unchecked(t, Some(mime), None, data.to_vec())
}

fn silent_mp3_frames() -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..40 {
        let mut f = vec![0u8; 417];
        f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        v.extend(f);
    }
    v
}

fn tag_with(title: &str, artist: &str, album: &str, tt: TagType, pics: Vec<Picture>) -> Tag {
    let mut tag = Tag::new(tt);
    tag.set_title(title.into());
    tag.set_artist(artist.into());
    tag.set_album(album.into());
    for p in pics {
        tag.push_picture(p);
    }
    tag
}

fn mp3(path: &Path, title: &str, album: &str, pics: Vec<Picture>) {
    fs::write(path, silent_mp3_frames()).unwrap();
    tag_with(title, "Art Band", album, TagType::Id3v2, pics)
        .save_to_path(path, WriteOptions::default())
        .unwrap();
}

fn flac(path: &Path, title: &str, pics: Vec<Picture>) {
    let mut v = b"fLaC".to_vec();
    v.extend_from_slice(&[0x00, 0, 0, 34]);
    v.extend_from_slice(&4096u16.to_be_bytes());
    v.extend_from_slice(&4096u16.to_be_bytes());
    v.extend_from_slice(&[0; 6]);
    let packed: u64 = (44100u64 << 44) | (1 << 41) | (15 << 36) | 44100;
    v.extend_from_slice(&packed.to_be_bytes());
    v.extend_from_slice(&[0; 16]);
    v.extend_from_slice(&[0x81, 0, 0, 8]); // last block: PADDING, so lofty can rewrite the tags
    v.extend_from_slice(&[0; 8]);
    fs::write(path, v).unwrap();
    tag_with(title, "Art Band", "Formats", TagType::VorbisComments, pics)
        .save_to_path(path, WriteOptions::default())
        .unwrap();
}

fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

fn full_box(kind: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = flags.to_be_bytes().to_vec();
    b.extend_from_slice(body);
    mp4_box(kind, &b)
}

/// A minimal M4A: one 1-sample AAC track, then lofty adds the `ilst` tag with `covr`.
fn m4a(path: &Path, title: &str, pics: Vec<Picture>) {
    let mut mvhd = vec![0u8; 96];
    mvhd[8..12].copy_from_slice(&1000u32.to_be_bytes()); // timescale
    mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes()); // duration
    mvhd[16..20].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // rate
    mvhd[20..22].copy_from_slice(&0x0100u16.to_be_bytes()); // volume
    mvhd[32..36].copy_from_slice(&0x0001_0000u32.to_be_bytes()); // matrix
    mvhd[48..52].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    mvhd[64..68].copy_from_slice(&0x4000_0000u32.to_be_bytes());
    mvhd[92..96].copy_from_slice(&2u32.to_be_bytes()); // next track id
    let mut tkhd = vec![0u8; 80];
    tkhd[12..16].copy_from_slice(&1u32.to_be_bytes());
    tkhd[20..24].copy_from_slice(&1000u32.to_be_bytes());
    tkhd[36..38].copy_from_slice(&0x0100u16.to_be_bytes());
    tkhd[40..44].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    tkhd[56..60].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    tkhd[72..76].copy_from_slice(&0x4000_0000u32.to_be_bytes());
    let mut mdhd = vec![0u8; 20];
    mdhd[8..12].copy_from_slice(&44100u32.to_be_bytes());
    mdhd[12..16].copy_from_slice(&44100u32.to_be_bytes());
    mdhd[16..18].copy_from_slice(&0x55c4u16.to_be_bytes());
    let mut hdlr = vec![0u8; 20];
    hdlr[4..8].copy_from_slice(b"soun");
    hdlr.push(0);
    // mp4a sample entry with an esds (AAC-LC, 44.1 kHz, stereo)
    let esds_body: Vec<u8> = vec![
        0x03, 0x19, 0, 1, 0, // ES_Descr
        0x04, 0x11, 0x40, 0x15, 0, 0, 0, 0, 0, 0, 0xfa, 0, 0, 0, 0xfa, 0, // DecoderConfig
        0x05, 0x02, 0x12, 0x10, // AudioSpecificConfig
        0x06, 0x01, 0x02,
    ];
    let mut mp4a = vec![0u8; 6];
    mp4a.extend_from_slice(&1u16.to_be_bytes());
    mp4a.extend_from_slice(&[0; 8]);
    mp4a.extend_from_slice(&2u16.to_be_bytes());
    mp4a.extend_from_slice(&16u16.to_be_bytes());
    mp4a.extend_from_slice(&[0; 4]);
    mp4a.extend_from_slice(&(44100u32 << 16).to_be_bytes());
    mp4a.extend(full_box(b"esds", 0, &esds_body));
    let mut stsd = 1u32.to_be_bytes().to_vec();
    stsd.extend(mp4_box(b"mp4a", &mp4a));
    let stts = [1u32, 1, 1024].iter().flat_map(|n| n.to_be_bytes()).collect::<Vec<u8>>();
    let stsc = [1u32, 1, 1, 1].iter().flat_map(|n| n.to_be_bytes()).collect::<Vec<u8>>();
    let stsz = [0u32, 1, 8].iter().flat_map(|n| n.to_be_bytes()).collect::<Vec<u8>>();
    let ftyp = mp4_box(b"ftyp", b"M4A \0\0\0\0M4A mp42isom");
    let build = |chunk_offset: u32| -> Vec<u8> {
        let stco = [1u32, chunk_offset].iter().flat_map(|n| n.to_be_bytes()).collect::<Vec<u8>>();
        let mut stbl = full_box(b"stsd", 0, &stsd);
        stbl.extend(full_box(b"stts", 0, &stts));
        stbl.extend(full_box(b"stsc", 0, &stsc));
        stbl.extend(full_box(b"stsz", 0, &stsz));
        stbl.extend(full_box(b"stco", 0, &stco));
        let mut minf = full_box(b"smhd", 0, &[0; 4]);
        minf.extend(mp4_box(b"stbl", &stbl));
        let mut mdia = full_box(b"mdhd", 0, &mdhd);
        mdia.extend(full_box(b"hdlr", 0, &hdlr));
        mdia.extend(mp4_box(b"minf", &minf));
        let mut trak = full_box(b"tkhd", 3, &tkhd);
        trak.extend(mp4_box(b"mdia", &mdia));
        let mut moov = full_box(b"mvhd", 0, &mvhd);
        moov.extend(mp4_box(b"trak", &trak));
        mp4_box(b"moov", &moov)
    };
    let moov_len = build(0).len();
    let mdat_at = (ftyp.len() + moov_len + 8) as u32;
    let mut out = ftyp;
    out.extend(build(mdat_at));
    out.extend(mp4_box(b"mdat", &[0u8; 8]));
    fs::write(path, out).unwrap();
    tag_with(title, "Art Band", "Formats", TagType::Mp4Ilst, pics)
        .save_to_path(path, WriteOptions::default())
        .unwrap();
}

fn main() {
    let root = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: mkaudio_art <dir>")).join("art");
    for d in ["set12", "sizes", "formats", "solo", "hostile", "plain", "loose"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    let a = blocks(1, 600, 600);
    let b = blocks(2, 600, 600);
    let front = |d: &[u8]| pic(PictureType::CoverFront, MimeType::Jpeg, d);
    let back = |d: &[u8]| pic(PictureType::CoverBack, MimeType::Jpeg, d);
    for i in 1..=10 {
        mp3(&root.join(format!("set12/t{i:02}.mp3")), &format!("Track {i}"), "Twelve", vec![front(&a)]);
    }
    for i in 11..=12 {
        mp3(&root.join(format!("set12/t{i:02}.mp3")), &format!("Track {i}"), "Twelve", vec![back(&b)]);
    }
    mp3(&root.join("sizes/small.mp3"), "Small", "Sizes", vec![front(&blocks(3, 600, 600))]);
    mp3(&root.join("sizes/large.mp3"), "Large", "Sizes", vec![front(&blocks(3, 1000, 1000))]);
    flac(&root.join("formats/pic.flac"), "Flac", vec![front(&a)]);
    m4a(&root.join("formats/covr.m4a"), "M4a", vec![front(&a)]);
    // no artist / album tag: the import makes a loose track (cover art on the item itself)
    let solo = root.join("solo/solo.mp3");
    fs::write(&solo, silent_mp3_frames()).unwrap();
    let mut t = Tag::new(TagType::Id3v2);
    t.set_title("Solo Song".into());
    t.push_picture(front(&blocks(4, 400, 400)));
    t.save_to_path(&solo, WriteOptions::default()).unwrap();
    // a tiny but valid GIF header: lofty reads it as a picture, the portal does not support it
    let gif: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;";
    mp3(
        &root.join("hostile/hostile.mp3"),
        "<img src=x onerror=alert(1)>",
        "<script>alert(2)</script>",
        vec![pic(PictureType::CoverFront, MimeType::Gif, gif)],
    );
    mp3(&root.join("plain/p1.mp3"), "Plain One", "Plain", vec![]);
    mp3(&root.join("plain/p2.mp3"), "Plain Two", "Plain", vec![back(&b)]);
    fs::write(root.join("loose/front.jpg"), blocks(5, 500, 500)).unwrap();
    fs::write(root.join("loose/back.jpg"), blocks(6, 500, 500)).unwrap();
    fs::write(root.join("loose/wide.jpg"), blocks(7, 500, 250)).unwrap();
    fs::write(root.join("loose/notimage.jpg"), b"this is just text pretending to be a picture").unwrap();
    // read back what the server will see
    for f in ["formats/pic.flac", "formats/covr.m4a", "set12/t01.mp3", "hostile/hostile.mp3"] {
        let t = lofty::read_from_path(root.join(f)).expect("fixture parses");
        let n = t.tags().iter().map(|t| t.pictures().len()).sum::<usize>();
        println!("{f}: {n} picture(s)");
    }
}
