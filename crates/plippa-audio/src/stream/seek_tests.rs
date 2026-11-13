//! WARP_03: frame-exact seeks (`seek_frame` / `seek_ms`) on generated WAV and real
//! FLAC / MP3 files. Position maths and sample content only; nothing here is timed.

use super::*;
use crate::decode::decode;
use crate::position::ms_to_frame;
use crate::testsupport::{frame_index_from_f32, frame_index_sample, write_pcm16_wav, Lcg};
use plippa_core::Capabilities;

fn caps() -> Capabilities {
    Capabilities::desktop_mock()
}

fn open(path: &Path) -> StreamDecoder {
    StreamDecoder::open(path, &caps(), OverspecPolicy::Downsample).expect("open")
}

fn test_media(rel: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/testMedia");
    assert!(root.is_dir(), "test media missing: run scripts/fetch-testmedia.sh");
    root.join(rel)
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Frame-index WAV: sample value identifies its frame (see `frame_index_sample`).
fn index_wav(rate: u32, channels: u16, frames: usize, tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "plippa_seek_{tag}_{rate}_{channels}_{frames}_{}.wav",
        std::process::id()
    ));
    write_pcm16_wav(&path, rate, channels, frames, |f, _| frame_index_sample(f)).unwrap();
    path
}

#[test]
fn wav_seek_frame_lands_on_the_target_for_100_random_targets() {
    let frames = 60_000usize;
    let path = index_wav(48_000, 2, frames, "random");
    let mut dec = open(&path);
    assert!(dec.duration_known());
    assert_eq!(dec.duration_frames(), Some(frames as u64));
    assert_eq!(dec.position_frame(), 0);
    let mut rng = Lcg::new(1234);
    for i in 0..100 {
        // Mix random targets with the awkward ones (0, last frame, packet-ish multiples).
        let target = match i {
            0 => 0,
            1 => frames as u64 - 1,
            2 => 1,
            3 => 4096,
            4 => 4097,
            _ => rng.below(frames as u64),
        };
        dec.seek_frame(target).unwrap();
        assert_eq!(dec.position_frame(), target);
        assert_eq!(dec.start_frame(), target);
        let pcm = dec.next_chunk(64).unwrap().expect("pcm");
        assert_eq!(
            frame_index_from_f32(pcm.samples[0]),
            target as usize % 65_536,
            "first output frame for target {target}"
        );
        // Second channel carries the same index; the run is contiguous.
        assert_eq!(
            frame_index_from_f32(pcm.samples[1]),
            target as usize % 65_536
        );
        for (k, fr) in pcm.samples.chunks(2).enumerate() {
            assert_eq!(frame_index_from_f32(fr[0]), (target as usize + k) % 65_536);
        }
        assert_eq!(dec.position_frame(), target + pcm.frame_count() as u64);
    }
    let _ = std::fs::remove_file(path);
}

#[test]
fn seek_ms_uses_exact_frames_and_44k1_floors() {
    let path = index_wav(44_100, 1, 50_000, "ms");
    let mut dec = open(&path);
    for ms in [0u64, 1, 7, 250, 999, 1000, 1133] {
        dec.seek_ms(ms).unwrap();
        let want = ms_to_frame(ms, 44_100);
        assert_eq!(dec.position_frame(), want, "ms {ms}");
        let pcm = dec.next_chunk(32).unwrap().unwrap();
        assert_eq!(frame_index_from_f32(pcm.samples[0]), want as usize);
    }
    let _ = std::fs::remove_file(path);
}

#[test]
fn sum_of_relative_frame_seeks_equals_the_exact_total() {
    let frames = 60_000usize;
    let path = index_wav(48_000, 2, frames, "relative");
    let mut dec = open(&path);
    let mut rng = Lcg::new(77);
    let mut pos: i64 = 30_000;
    dec.seek_frame(pos as u64).unwrap();
    let mut total_delta: i64 = 0;
    for _ in 0..60 {
        let pcm = dec.next_chunk(16).unwrap().unwrap();
        // Relative seek from where the decoder actually is.
        let here = dec.position_frame() as i64;
        assert_eq!(here, pos + pcm.frame_count() as i64);
        let mut delta = rng.below(9_000) as i64 - 4_500;
        if here + delta < 0 || here + delta >= frames as i64 {
            delta = -delta;
        }
        pos = here + delta;
        total_delta += pcm.frame_count() as i64 + delta;
        dec.seek_frame(pos as u64).unwrap();
        assert_eq!(dec.position_frame() as i64, pos);
    }
    assert_eq!(pos, 30_000 + total_delta, "sum of relative seeks is exact");
    let pcm = dec.next_chunk(8).unwrap().unwrap();
    assert_eq!(frame_index_from_f32(pcm.samples[0]), pos as usize % 65_536);
    let _ = std::fs::remove_file(path);
}

#[test]
fn seek_past_known_end_is_an_error_not_a_panic() {
    let path = index_wav(8_000, 1, 8_000, "end");
    let mut dec = open(&path);
    assert!(dec.seek_frame(8_000 * 1000).is_err());
    // The decoder is still usable afterwards.
    dec.seek_frame(10).unwrap();
    assert!(dec.next_chunk(8).unwrap().is_some());
    let _ = std::fs::remove_file(path);
}

#[test]
fn duration_known_is_false_without_n_frames() {
    // WAV always has a length; the flag mirrors duration_ms().
    let path = index_wav(8_000, 1, 800, "dur");
    let dec = open(&path);
    assert!(dec.duration_known());
    assert_eq!(dec.duration_ms(), Some(100));
    let _ = std::fs::remove_file(path);
}

/// Max abs difference between the decoder output after a seek and the reference decode.
fn max_diff_after_seek(
    dec: &mut StreamDecoder,
    reference: &[f32],
    ch: usize,
    target: u64,
    n: usize,
) -> f32 {
    dec.seek_frame(target).unwrap();
    let mut got: Vec<f32> = Vec::new();
    while got.len() < n * ch {
        match dec.next_chunk(n).unwrap() {
            Some(p) => got.extend_from_slice(&p.samples),
            None => break,
        }
    }
    let have = (got.len() / ch).min(n);
    let base = target as usize * ch;
    (0..have * ch)
        .map(|i| (got[i] - reference[base + i]).abs())
        .fold(0.0, f32::max)
}

#[test]
fn flac_seek_matches_sequential_decode_exactly() {
    for path in [test_media(
        "FLAC/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.flac",
    )] {
        let reference = decode(&path).unwrap();
        let ch = reference.channels as usize;
        let total = reference.frame_count() as u64;
        let mut dec = open(&path);
        let mut rng = Lcg::new(42);
        for i in 0..100 {
            let target = if i == 0 {
                0
            } else {
                rng.below(total.saturating_sub(2048))
            };
            let d = max_diff_after_seek(&mut dec, &reference.samples, ch, target, 1024);
            assert_eq!(
                d,
                0.0,
                "{} target {target}: FLAC must be bit-exact",
                path.display()
            );
        }
    }
}

/// MP3 is lossy and a seek restarts the bit reservoir, so the very first frames after a seek
/// may differ slightly from a sequential decode; the *alignment* must still be exact. The
/// decoder output from `skip` frames after the landing is compared at lags -4..=4 against
/// the sequential decode: the best lag must be 0 and the match tight there.
fn mp3_alignment(path: &Path, seeks: usize) {
    let reference = decode(path).unwrap();
    let ch = reference.channels as usize;
    let total = reference.frame_count() as u64;
    let mut dec = open(path);
    let mut rng = Lcg::new(2024);
    let (skip, win) = (4608usize, 2048usize);
    for i in 0..seeks {
        let target = if i == 0 {
            0
        } else {
            rng.below(total - (skip + win + 64) as u64)
        };
        dec.seek_frame(target).unwrap();
        assert_eq!(dec.position_frame(), target);
        let mut got: Vec<f32> = Vec::new();
        while got.len() < (skip + win) * ch {
            match dec.next_chunk(4096).unwrap() {
                Some(p) => got.extend_from_slice(&p.samples),
                None => break,
            }
        }
        assert!(got.len() >= (skip + win) * ch, "decoded enough after seek");
        let err_at = |lag: i64| -> f64 {
            let mut e = 0.0f64;
            for k in 0..win {
                let g = got[(skip + k) * ch] as f64;
                let r = reference.samples[((target as i64 + (skip + k) as i64 + lag) as usize) * ch]
                    as f64;
                e += (g - r).powi(2);
            }
            e
        };
        let errs: Vec<(i64, f64)> = (-4..=4).map(|l| (l, err_at(l))).collect();
        let best = errs
            .iter()
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();
        assert_eq!(
            best.0,
            0,
            "{} target {target}: best lag {errs:?}",
            path.display()
        );
        let sig: f64 = (0..win)
            .map(|k| (got[(skip + k) * ch] as f64).powi(2))
            .sum();
        assert!(
            errs[4].1 <= sig * 1e-3 + 1e-9,
            "target {target}: lag-0 error {} vs signal {sig}",
            errs[4].1
        );
    }
}

#[test]
fn mp3_320_seek_lands_within_a_frame_for_100_targets() {
    mp3_alignment(
        &test_media("MP3 320/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3"),
        100,
    );
}

#[test]
fn mp3_v0_seek_lands_within_a_frame_for_100_targets() {
    mp3_alignment(
        &test_media("MP3 V0/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3"),
        100,
    );
}

#[test]
fn mp3_fixture_trims_the_early_landing() {
    // The reason this package exists: Symphonia lands before the target on MP3.
    let path = fixture("tone_48k.mp3");
    let reference = decode(&path).unwrap();
    let total = reference.frame_count() as u64;
    if total > 12_000 {
        mp3_alignment_small(&path, total);
    }
}

fn mp3_alignment_small(path: &Path, total: u64) {
    let mut dec = open(path);
    for target in [5_000u64, 6_001, 9_999] {
        if target + 6_000 > total {
            continue;
        }
        dec.seek_frame(target).unwrap();
        assert_eq!(dec.position_frame(), target);
        assert!(dec.next_chunk(256).unwrap().is_some());
        assert_eq!(dec.position_frame(), target + 256);
    }
}

/// The 12000-frame `tone_48k.flac` fixture: the landing (first frames) is exact for 100 random
/// seeks. Not asserted beyond the first packet: after some backward seeks Symphonia 0.5.5
/// returns shifted data for the *second* packet of this tiny file (reproduced independently
/// of the trimming; the real-length FLAC above is bit-exact for 100 seeks).
#[test]
fn flac_fixture_landing_is_exact() {
    let path = fixture("tone_48k.flac");
    let reference = decode(&path).unwrap();
    let ch = reference.channels as usize;
    let total = reference.frame_count() as u64;
    let mut dec = open(&path);
    let mut rng = Lcg::new(42);
    for _ in 0..100 {
        let target = rng.below(total - 512);
        let d = max_diff_after_seek(&mut dec, &reference.samples, ch, target, 64);
        assert_eq!(d, 0.0, "target {target}");
        assert_eq!(dec.position_frame(), target + 64);
    }
}
