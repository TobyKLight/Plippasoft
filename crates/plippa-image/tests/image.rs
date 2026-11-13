//! Fixtures in `tests/fixtures/` were authored with Pillow (a generator script is not kept):
//! `*_64x32.*` are four flat quadrants (top-left red, top-right blue, bottom-left green,
//! bottom-right yellow; `rgba` is a red left half over a fully transparent right half), JPEG
//! quality 90 with 4:4:4 sampling; `interlaced` was converted with ImageMagick; `picture_*` are
//! procedural 128x128 scenes with structure (a), and a different layout (b), with
//! `picture_a_q60_half` being (a) halved and re-saved at JPEG quality 60. `bomb_*` is a
//! hand-built PNG whose IHDR claims 60000x60000.

use plippa_image::*;

const LIMITS: ImageLimits = ImageLimits {
    max_side: 5000,
    max_pixels: 25_000_000,
    max_bytes: 16 * 1024 * 1024,
};

fn fx(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn px(img: &Rgb8, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * img.w + x) * 3) as usize;
    [img.px[i], img.px[i + 1], img.px[i + 2]]
}

fn near(got: [u8; 3], want: [u8; 3], tol: i32) -> bool {
    got.iter()
        .zip(want)
        .all(|(&g, w)| (g as i32 - w as i32).abs() <= tol)
}

const RED: [u8; 3] = [220, 30, 30];
const BLUE: [u8; 3] = [30, 30, 220];
const GREEN: [u8; 3] = [30, 200, 30];
const YELLOW: [u8; 3] = [230, 220, 30];

fn assert_quadrants(name: &str, tol: i32) {
    let img = decode(&fx(name), &LIMITS).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!((img.w, img.h), (64, 32), "{name}");
    assert!(img.is_consistent());
    for (x, y, want) in [(8, 4, RED), (56, 4, BLUE), (8, 28, GREEN), (56, 28, YELLOW)] {
        let got = px(&img, x, y);
        assert!(
            near(got, want, tol),
            "{name} at ({x},{y}): {got:?} vs {want:?}"
        );
    }
}

#[test]
fn good_fixtures_decode_to_expected_size_and_colours() {
    assert_quadrants("baseline_64x32.jpg", 12);
    assert_quadrants("progressive_64x32.jpg", 12);
    assert_quadrants("rgb_64x32.png", 0);
    assert_quadrants("interlaced_64x32.png", 0);
    assert_quadrants("lossless_64x32.webp", 0);
    assert_quadrants("lossy_64x32.webp", 16);
    // Palette PNG was quantised to 8 colours: allow for the palette approximation.
    assert_quadrants("palette_64x32.png", 8);
}

#[test]
fn alpha_is_composited_on_black() {
    let img = decode(&fx("rgba_64x32.png"), &LIMITS).unwrap();
    assert_eq!((img.w, img.h), (64, 32));
    assert_eq!(px(&img, 8, 8), RED);
    assert_eq!(px(&img, 56, 8), [0, 0, 0]);
}

#[test]
fn sixteen_bit_and_grayscale_decode() {
    let img = decode(&fx("gray16_32x16.png"), &LIMITS).unwrap();
    assert_eq!((img.w, img.h), (32, 16));
    assert_eq!(px(&img, 2, 2), [0, 0, 0]);
    assert!(near(px(&img, 29, 2), [255, 255, 255], 1));
    let img = decode(&fx("gray_64x32.jpg"), &LIMITS).unwrap();
    assert_eq!((img.w, img.h), (64, 32));
    let [r, g, b] = px(&img, 8, 4);
    assert!(r == g && g == b, "gray stays neutral");
}

#[test]
fn probe_reports_kind_and_size() {
    for (name, kind) in [
        ("baseline_64x32.jpg", ImageKind::Jpeg),
        ("progressive_64x32.jpg", ImageKind::Jpeg),
        ("rgb_64x32.png", ImageKind::Png),
        ("palette_64x32.png", ImageKind::Png),
        ("lossy_64x32.webp", ImageKind::Webp),
        ("lossless_64x32.webp", ImageKind::Webp),
    ] {
        assert_eq!(probe(&fx(name), &LIMITS), Ok((kind, 64, 32)), "{name}");
    }
}

#[test]
fn oversize_header_is_refused_before_decoding() {
    let bomb = fx("bomb_60000x60000.png");
    let t = std::time::Instant::now();
    assert!(matches!(
        probe(&bomb, &LIMITS),
        Err(ImageError::TooLarge {
            width: 60000,
            height: 60000,
            ..
        })
    ));
    assert!(matches!(
        decode(&bomb, &LIMITS),
        Err(ImageError::TooLarge { .. })
    ));
    assert!(t.elapsed().as_millis() < 500);
}

#[test]
fn limits_apply_to_side_pixels_and_bytes() {
    let jpg = fx("baseline_64x32.jpg");
    let tight = |f: fn(&mut ImageLimits)| {
        let mut l = LIMITS;
        f(&mut l);
        l
    };
    assert!(matches!(
        probe(&jpg, &tight(|l| l.max_side = 63)),
        Err(ImageError::TooLarge { .. })
    ));
    assert!(matches!(
        probe(&jpg, &tight(|l| l.max_pixels = 2047)),
        Err(ImageError::TooLarge { .. })
    ));
    assert!(matches!(
        probe(&jpg, &tight(|l| l.max_bytes = 100)),
        Err(ImageError::TooLarge { .. })
    ));
    assert!(probe(&jpg, &tight(|l| l.max_pixels = 2048)).is_ok());
}

#[test]
fn unsupported_inputs() {
    assert_eq!(
        probe(&fx("gif_header.bin"), &LIMITS),
        Err(ImageError::Unsupported)
    );
    assert_eq!(
        decode(&fx("gif_header.bin"), &LIMITS),
        Err(ImageError::Unsupported)
    );
    assert_eq!(
        decode(&fx("cmyk_64x32.jpg"), &LIMITS),
        Err(ImageError::Unsupported)
    );
    assert_eq!(decode(b"", &LIMITS), Err(ImageError::Unsupported));
    assert_eq!(
        decode(b"BM\x00\x00hello", &LIMITS),
        Err(ImageError::Unsupported)
    );
}

#[test]
fn truncated_data_is_an_error() {
    let full = fx("baseline_64x32.jpg");
    for cut in [
        2,
        10,
        150,
        full.len() / 2,
        full.len() * 2 / 3,
        full.len() - 3,
    ] {
        let r = decode(&full[..cut], &LIMITS);
        assert!(
            matches!(r, Err(ImageError::Truncated) | Err(ImageError::Corrupt(_))),
            "cut {cut}: {r:?}"
        );
    }
    assert!(decode(&fx("truncated.jpg"), &LIMITS).is_err());
    let png = fx("rgb_64x32.png");
    for cut in [8, 20, 40, png.len() - 20] {
        assert!(decode(&png[..cut], &LIMITS).is_err(), "png cut {cut}");
    }
    let webp = fx("lossless_64x32.webp");
    for cut in [8, 20, webp.len() - 10] {
        assert!(decode(&webp[..cut], &LIMITS).is_err(), "webp cut {cut}");
    }
}

#[test]
fn hostile_inputs_error_without_panicking() {
    // Valid SOI followed by pseudo-random bytes (deterministic xorshift).
    let mut s = 0x2545F4914F6CDD1Du64;
    let mut rnd = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s as u8
    };
    for len in [3usize, 20, 200, 4000] {
        let mut b = vec![0xFF, 0xD8];
        b.extend((0..len).map(|_| rnd()));
        assert!(decode(&b, &LIMITS).is_err(), "random JPEG len {len}");
        let mut p = b"\x89PNG\r\n\x1a\n".to_vec();
        p.extend((0..len).map(|_| rnd()));
        assert!(decode(&p, &LIMITS).is_err(), "random PNG len {len}");
        let mut w = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        w.extend((0..len).map(|_| rnd()));
        assert!(decode(&w, &LIMITS).is_err(), "random WebP len {len}");
    }
    // Deeply truncated progressive scan.
    let prog = fx("progressive_64x32.jpg");
    for cut in [prog.len() / 3, prog.len() / 2, prog.len() - 40] {
        assert!(
            decode(&prog[..cut], &LIMITS).is_err(),
            "progressive cut {cut}"
        );
    }
    // Bit flips across a good file never panic (success or error are both acceptable).
    for name in [
        "baseline_64x32.jpg",
        "rgb_64x32.png",
        "lossy_64x32.webp",
        "lossless_64x32.webp",
    ] {
        let good = fx(name);
        for i in (0..good.len()).step_by(7) {
            let mut b = good.clone();
            b[i] ^= 0xA5;
            let _ = decode(&b, &LIMITS);
        }
    }
}

#[test]
fn zero_dimensions_are_refused() {
    let mut jpg = fx("baseline_64x32.jpg");
    let sof = jpg.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
    jpg[sof + 5..sof + 9].fill(0); // height and width
    assert!(matches!(decode(&jpg, &LIMITS), Err(ImageError::Corrupt(_))));

    let mut png = fx("rgb_64x32.png");
    png[16..20].fill(0);
    assert!(matches!(decode(&png, &LIMITS), Err(ImageError::Corrupt(_))));
}

#[test]
fn square_crop_rects() {
    assert_eq!(square_crop_rect(100, 50), (25, 0, 50));
    assert_eq!(square_crop_rect(50, 100), (0, 25, 50));
    assert_eq!(square_crop_rect(101, 50), (25, 0, 50));
    assert_eq!(square_crop_rect(64, 64), (0, 0, 64));
    assert_eq!(square_crop_rect(1, 1), (0, 0, 1));
    assert_eq!(square_crop_rect(0, 7), (0, 3, 0));
}

fn solid(w: u32, h: u32, c: [u8; 3]) -> Rgb8 {
    Rgb8 {
        w,
        h,
        px: c
            .iter()
            .copied()
            .cycle()
            .take((w * h * 3) as usize)
            .collect(),
    }
}

#[test]
fn scale_square_keeps_a_vertical_split_from_a_wide_source() {
    // 200x100: red left half, blue right half. The centred 100x100 square keeps the split.
    let mut img = solid(200, 100, [0, 0, 0]);
    for y in 0..100 {
        for x in 0..200 {
            let i = ((y * 200 + x) * 3) as usize;
            img.px[i..i + 3].copy_from_slice(if x < 100 { &[255, 0, 0] } else { &[0, 0, 255] });
        }
    }
    let out = scale_square(&img, 10);
    assert_eq!((out.w, out.h), (10, 10));
    for y in 0..10 {
        assert_eq!(px(&out, 0, y), [255, 0, 0]);
        assert_eq!(px(&out, 4, y), [255, 0, 0]);
        assert_eq!(px(&out, 5, y), [0, 0, 255]);
        assert_eq!(px(&out, 9, y), [0, 0, 255]);
    }
}

#[test]
fn scale_square_is_an_exact_area_average() {
    let flat = solid(37, 37, [12, 200, 77]);
    for side in [1, 5, 13, 36] {
        let out = scale_square(&flat, side);
        assert_eq!((out.w, out.h), (side, side));
        assert!(
            out.px.chunks(3).all(|p| p == [12, 200, 77]),
            "flat side {side}"
        );
    }
    // 2x2 checker down to 1x1 averages the four values; 3 -> 2 splits the middle pixel evenly.
    let mut img = solid(2, 2, [0, 0, 0]);
    img.px[0] = 200;
    img.px[3] = 100;
    assert_eq!(scale_square(&img, 1).px, vec![75, 0, 0]);
    let mut line = solid(3, 3, [0, 0, 0]);
    for y in 0..3 {
        line.px[(y * 3 * 3) as usize] = 90;
        line.px[(y * 3 * 3 + 3) as usize] = 60;
        line.px[(y * 3 * 3 + 6) as usize] = 30;
    }
    // Cells cover 1.5 px each: (90 + 0.5*60)/1.5 = 80 and (0.5*60 + 30)/1.5 = 40.
    assert_eq!(
        scale_square(&line, 2).px,
        vec![80, 0, 0, 40, 0, 0, 80, 0, 0, 40, 0, 0]
    );
}

#[test]
fn scale_square_never_upscales() {
    let img = solid(40, 20, [1, 2, 3]);
    let out = scale_square(&img, 500);
    assert_eq!((out.w, out.h), (20, 20));
    let same = solid(20, 20, [9, 9, 9]);
    assert_eq!(scale_square(&same, 20), same);
    assert_eq!(scale_square(&same, 21), same);
    assert!(scale_square(&same, 0).px.is_empty());
}

#[test]
fn rgb565_packs_like_the_display() {
    let img = Rgb8 {
        w: 2,
        h: 1,
        px: vec![255, 255, 255, 8, 4, 8],
    };
    assert_eq!(to_rgb565(&img), vec![0xFFFF, (1 << 11) | (1 << 5) | 1]);
    assert_eq!(to_rgb565(&solid(1, 1, [255, 0, 0])), vec![0xF800]);
}

fn flip_horizontal(img: &Rgb8) -> Rgb8 {
    let mut out = img.clone();
    for y in 0..img.h {
        for x in 0..img.w {
            let s = ((y * img.w + (img.w - 1 - x)) * 3) as usize;
            let d = ((y * img.w + x) * 3) as usize;
            out.px[d..d + 3].copy_from_slice(&img.px[s..s + 3]);
        }
    }
    out
}

#[test]
fn dhash_matches_reencoded_and_separates_different_pictures() {
    let a = decode(&fx("picture_a.jpg"), &LIMITS).unwrap();
    let a_small = decode(&fx("picture_a_q60_half.jpg"), &LIMITS).unwrap();
    let b = decode(&fx("picture_b.jpg"), &LIMITS).unwrap();
    assert_eq!(a_small.w, 64);
    let ha = dhash64(&a);
    assert_eq!(dhash64(&a), ha, "deterministic");
    assert!(
        hamming(ha, dhash64(&a_small)) <= 4,
        "{}",
        hamming(ha, dhash64(&a_small))
    );
    // Same picture through this crate's own thumbnail encoder at half size.
    let thumb = decode(&encode_jpeg_fit(&a, 64).unwrap(), &LIMITS).unwrap();
    assert!(hamming(ha, dhash64(&thumb)) <= 4);
    assert!(hamming(ha, dhash64(&flip_horizontal(&a))) > 10, "mirrored");
    assert!(hamming(ha, dhash64(&b)) > 10, "different picture");
    // Flat and empty images hash to zero.
    assert_eq!(dhash64(&solid(10, 10, [5, 5, 5])), 0);
    assert_eq!(
        dhash64(&Rgb8 {
            w: 0,
            h: 0,
            px: vec![]
        }),
        0
    );
    assert_eq!(hamming(0, u64::MAX), 64);
    assert_eq!(hamming(0b1010, 0b0110), 2);
}

fn gradient(w: u32, h: u32) -> Rgb8 {
    let mut px = Vec::new();
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 128]);
        }
    }
    Rgb8 { w, h, px }
}

#[test]
fn encode_jpeg_fit_round_trips_and_fits() {
    let img = gradient(300, 200);
    let jpg = encode_jpeg_fit(&img, 128).unwrap();
    let (kind, w, h) = probe(&jpg, &LIMITS).unwrap();
    assert_eq!(kind, ImageKind::Jpeg);
    assert_eq!(w, 128);
    assert!(
        (h as f64 - 128.0 * 200.0 / 300.0).abs() < 1.0,
        "aspect: {w}x{h}"
    );
    let back = decode(&jpg, &LIMITS).unwrap();
    assert_eq!((back.w, back.h), (w, h));
    assert!(near(px(&back, 0, 0), [0, 0, 128], 12));

    let tall = encode_jpeg_fit(&gradient(60, 240), 120).unwrap();
    let (_, w, h) = probe(&tall, &LIMITS).unwrap();
    assert_eq!((w, h), (30, 120));

    let small = encode_jpeg_fit(&gradient(100, 50), 400).unwrap();
    let (_, w, h) = probe(&small, &LIMITS).unwrap();
    assert_eq!((w, h), (100, 50));
}

#[test]
fn encode_jpeg_fit_rejects_bad_input() {
    assert!(encode_jpeg_fit(
        &Rgb8 {
            w: 0,
            h: 0,
            px: vec![]
        },
        128
    )
    .is_err());
    assert!(encode_jpeg_fit(&gradient(8, 8), 0).is_err());
    assert!(encode_jpeg_fit(
        &Rgb8 {
            w: 4,
            h: 4,
            px: vec![0; 5]
        },
        8
    )
    .is_err());
}

/// Signature plus an IHDR claiming `w`x`h` (CRC not checked by the header probe), no pixel data.
fn header_only_png(w: u32, h: u32, depth: u8, color: u8) -> Vec<u8> {
    let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
    b.extend_from_slice(&13u32.to_be_bytes());
    b.extend_from_slice(b"IHDR");
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&[depth, color, 0, 0, 0]);
    b.extend_from_slice(&[0; 4]);
    b
}

#[test]
fn png_working_set_follows_the_real_bytes_per_pixel() {
    // 25 Mpx 16-bit RGBA would inflate to 200 MB: refused up front.
    assert!(matches!(
        decode(&header_only_png(5000, 5000, 16, 6), &LIMITS),
        Err(ImageError::TooLarge {
            width: 5000,
            height: 5000,
            ..
        })
    ));
    // 5000x5000 8-bit RGBA (100 MB) passes the size check; the missing pixel data is what fails.
    assert!(!matches!(
        decode(&header_only_png(5000, 5000, 8, 6), &LIMITS),
        Err(ImageError::TooLarge { .. })
    ));
}
