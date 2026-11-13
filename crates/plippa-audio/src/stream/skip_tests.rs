//! MEDIA_02: a malformed frame is skipped, not the end of the file. Every case starts from the
//! committed `tone_48k_3s.mp3` (126 MP3 frames of 384 bytes, a 440 Hz tone) and damages a copy
//! at test time.

use super::*;
use crate::testsupport::Lcg;
use plippa_core::Capabilities;

/// Bytes per MP3 frame in the fixture (128 kb/s at 48 kHz, no padding).
const FRAME_BYTES: usize = 384;
const RATE: usize = 48_000;

fn base_bytes() -> Vec<u8> {
    std::fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone_48k_3s.mp3"),
    )
    .expect("read fixture")
}

fn write_copy(dir: &tempfile::TempDir, bytes: &[u8]) -> PathBuf {
    let path = dir.path().join("damaged.mp3");
    std::fs::write(&path, bytes).expect("write copy");
    path
}

/// Overwrite `len` bytes at `at` with 0xFF noise.
fn overwrite(bytes: &mut [u8], at: usize, len: usize) {
    bytes[at..at + len].fill(0xFF);
}

struct Decoded {
    /// Left channel only.
    left: Vec<f32>,
    skipped: u64,
    last_error: Option<String>,
    result: Result<(), AudioError>,
}

fn decode_to_end(path: &Path) -> Decoded {
    let mut dec =
        StreamDecoder::open(path, &Capabilities::desktop_mock(), OverspecPolicy::Downsample)
            .expect("open");
    let ch = dec.channels() as usize;
    let mut left = Vec::new();
    let result = loop {
        match dec.next_chunk(1024) {
            Ok(Some(pcm)) => left.extend(pcm.samples.chunks(ch).map(|f| f[0])),
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    Decoded {
        left,
        skipped: dec.skipped_packets(),
        last_error: dec.last_decode_error(),
        result,
    }
}

fn max_step(x: &[f32]) -> f32 {
    x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// The 2 KiB of damage sits mid-file; `FRAME_BYTES`-aligned so the first intact frame after it
/// is easy to reason about.
const DAMAGE_AT: usize = 62 * FRAME_BYTES + 100;
const DAMAGE_LEN: usize = 2048;

#[test]
fn a_malformed_frame_is_skipped_and_decoding_continues() {
    let dir = tempfile::tempdir().unwrap();
    let clean = decode_to_end(&write_copy(&dir, &base_bytes()));
    assert!(clean.result.is_ok() && clean.skipped == 0, "the clean file must not skip");

    let mut bytes = base_bytes();
    overwrite(&mut bytes, DAMAGE_AT, DAMAGE_LEN);
    let bad = decode_to_end(&write_copy(&dir, &bytes));

    bad.result.as_ref().expect("a bad frame must not fail the stream");
    assert!(bad.skipped >= 1, "the skip must be counted");
    assert!(bad.last_error.is_some(), "the last error text is kept");

    // The bytes themselves are gone (2 KiB is ~5 frames of audio); everything else must be
    // there: shorter than clean by about the damaged span, never longer.
    let lost_ms_budget = (DAMAGE_LEN / FRAME_BYTES + 2) * 1152 * 1000 / RATE + 50;
    let lost_ms = (clean.left.len() - bad.left.len()) * 1000 / RATE;
    assert!(bad.left.len() < clean.left.len());
    assert!(lost_ms <= lost_ms_budget, "lost {lost_ms} ms, budget {lost_ms_budget} ms");

    // Frames on both sides of the splice carry the tone (not silence).
    let before = &bad.left[RATE / 4..RATE / 2];
    let after = &bad.left[bad.left.len() - RATE / 2..bad.left.len() - RATE / 4];
    let tone_rms = rms(&clean.left[RATE / 4..RATE / 2]);
    assert!((rms(before) - tone_rms).abs() < tone_rms * 0.1);
    assert!((rms(after) - tone_rms).abs() < tone_rms * 0.1);
}

/// An undecodable frame as the demuxer hands it on: a valid MPEG-1 Layer III header at
/// 44.1 kHz in a 48 kHz stream, so the codec refuses it.
fn undecodable_frame() -> Vec<u8> {
    let mut frame = vec![0u8; 417];
    frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    frame
}

#[test]
fn a_skip_does_not_click() {
    let dir = tempfile::tempdir().unwrap();
    let tone = base_bytes();
    let clean = decode_to_end(&write_copy(&dir, &tone));
    // Skip the codec priming at the start and the padding at the end.
    let clean_step = max_step(&clean.left[2000..clean.left.len() - 2000]);

    // Cut five whole frames out (a phase jump of most of a tone cycle) and leave two
    // undecodable ones in the gap: the decoder meets an error, then a different phase.
    const CUT_AT: usize = 60;
    let mut bytes = tone[..CUT_AT * FRAME_BYTES].to_vec();
    bytes.extend(undecodable_frame());
    bytes.extend(undecodable_frame());
    bytes.extend_from_slice(&tone[(CUT_AT + 5) * FRAME_BYTES..]);
    let bad = decode_to_end(&write_copy(&dir, &bytes));
    bad.result.as_ref().expect("a skip is not a failure");
    assert!(bad.skipped >= 1, "the gap must have produced a skip");

    // The joint sits at the frame count before the gap (minus the Info frame, and the encoder
    // delay the decoder trims from the front: SEAM_01).
    let delay = StreamDecoder::open(
        &write_copy(&dir, &tone),
        &Capabilities::desktop_mock(),
        OverspecPolicy::Downsample,
    )
    .unwrap()
    .trim_delay as usize;
    assert!(delay > 0, "the fixture is expected to carry an encoder delay");
    let joint = (CUT_AT - 1) * 1152 - delay;
    let across = max_step(&bad.left[joint - 64..joint + 64]);
    assert!(
        across <= clean_step,
        "step across the splice {across} exceeds the tone's own max step {clean_step}"
    );
}

/// The fades themselves, on a known signal: DC already queued before a skip fades to zero over
/// the last `SPLICE_RAMP_FRAMES` frames, and the first frames decoded after it fade in from
/// zero. (The MP3 codec's own overlap-add already hides most gaps in the end-to-end test, so
/// this is what pins the ramp.)
#[test]
fn the_splice_fades_out_before_and_in_after_a_skip() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_copy(&dir, &base_bytes());
    let open = || {
        StreamDecoder::open(&path, &Capabilities::desktop_mock(), OverspecPolicy::Downsample)
            .unwrap()
    };
    let ch = 2usize;
    let r = SPLICE_RAMP_FRAMES;

    // The first packet is short (the encoder delay is trimmed from it): two packets give the
    // `2 * r` frames the comparison below needs.
    let mut reference = open();
    reference.pull_packet().unwrap();
    reference.pull_packet().unwrap();
    let decoded = reference.pending.clone();
    assert!(decoded.len() / ch > 2 * r);

    let mut dec = open();
    let lead = 3 * r;
    dec.pending = vec![1.0; lead * ch];
    let (mut consecutive, mut faded) = (0, false);
    dec.skip_packet("test", &mut consecutive, &mut faded).unwrap();
    assert_eq!(dec.skipped_packets(), 1);
    assert_eq!(dec.last_decode_error().as_deref(), Some("test"));
    for f in 0..lead {
        let want = if f < lead - r { 1.0 } else { (lead - 1 - f) as f32 / r as f32 };
        assert_eq!(dec.pending[f * ch], want, "frame {f} of the lead");
        assert_eq!(dec.pending[f * ch + 1], want);
    }
    assert_eq!(dec.pending[(lead - 1) * ch], 0.0, "the last frame before the gap is silent");

    // A second skip in the same gap does not fade the (already faded) tail again.
    dec.skip_packet("test", &mut consecutive, &mut faded).unwrap();
    assert_eq!(dec.pending[(lead - 2) * ch], 1.0 / r as f32);

    dec.pull_packet().unwrap();
    dec.pull_packet().unwrap();
    for f in 0..2 * r {
        let gain = if f < r { f as f32 / r as f32 } else { 1.0 };
        for c in 0..ch {
            let got = dec.pending[(lead + f) * ch + c];
            let want = decoded[f * ch + c] * gain;
            assert!((got - want).abs() < 1e-6, "frame {f} ch {c}: {got} vs {want}");
        }
    }
    assert_eq!(dec.pending[lead * ch], 0.0, "the first frame after the gap is silent");
}

/// A valid start followed by many frames whose header declares another sample rate: the
/// demuxer reads them fine and the codec refuses every one.
fn undecodable_tail(valid_frames: usize, bad_frames: usize) -> Vec<u8> {
    let mut bytes = base_bytes()[..valid_frames * FRAME_BYTES].to_vec();
    for _ in 0..bad_frames {
        // MPEG-1 Layer III, no CRC, 128 kb/s, 44.1 kHz, no padding, stereo: 417 bytes.
        let mut frame = vec![0u8; 417];
        frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        bytes.extend_from_slice(&frame);
    }
    bytes
}

#[test]
fn a_hopelessly_corrupt_file_still_fails() {
    let dir = tempfile::tempdir().unwrap();

    // Bad after a good start: the skip budget ends it, with a message that names the budget.
    let path = write_copy(&dir, &undecodable_tail(10, 400));
    let started = std::time::Instant::now();
    let bad = decode_to_end(&path);
    match bad.result {
        Err(AudioError::Decode { reason, .. }) => {
            assert!(reason.contains("skip budget"), "{reason}");
            assert!(reason.contains(&MAX_CONSECUTIVE_DECODE_ERRORS.to_string()), "{reason}");
        }
        other => panic!("expected a Decode error, got {other:?}"),
    }
    assert_eq!(bad.skipped, u64::from(MAX_CONSECUTIVE_DECODE_ERRORS));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    // The good frames before it were delivered (ten frames, less the encoder delay SEAM_01
    // trims from the first, less the chunk still being filled when the budget ran out).
    assert!(bad.left.len() >= 7 * 1152, "got {} frames", bad.left.len());

    // Noise from the first byte: nothing to decode at all, refused quickly and cleanly.
    let mut rng = Lcg::new(7);
    let noise: Vec<u8> = (0..65_536).map(|_| rng.next_u64() as u8).collect();
    let path = write_copy(&dir, &noise);
    let opened = StreamDecoder::open(&path, &Capabilities::desktop_mock(), OverspecPolicy::Downsample);
    if let Ok(mut dec) = opened {
        assert!(dec.next_chunk(1024).is_err() || dec.next_chunk(1024).unwrap().is_none());
    }
}

#[test]
fn bad_frames_among_good_ones_do_not_exhaust_the_budget() {
    // Pairs of undecodable frames between runs of good ones, for far longer than the budget
    // in total: only *consecutive* errors are fatal.
    let tone = base_bytes();
    let mut bytes = tone[..4 * FRAME_BYTES].to_vec();
    let mut next_good = 0usize;
    for _ in 0..80 {
        for _ in 0..2 {
            let mut f = vec![0u8; 417];
            f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            bytes.extend_from_slice(&f);
        }
        for _ in 0..3 {
            let n = 1 + next_good % 100; // re-use the fixture's good frames in a loop
            bytes.extend_from_slice(&tone[n * FRAME_BYTES..(n + 1) * FRAME_BYTES]);
            next_good += 1;
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let out = decode_to_end(&write_copy(&dir, &bytes));
    out.result.expect("scattered bad frames are not a hopeless file");
    assert!(
        out.skipped > u64::from(MAX_CONSECUTIVE_DECODE_ERRORS),
        "{} skips",
        out.skipped
    );
    // The decode ran on past the bad stretches (the file's own header says 126 frames, which
    // is where the trimmed decode ends: bad frames count in that timeline too).
    assert!(out.left.len() > 60 * 1152, "{} frames", out.left.len());
}

#[test]
fn a_truncated_last_frame_is_eof_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let clean = decode_to_end(&write_copy(&dir, &base_bytes()));
    let mut bytes = base_bytes();
    bytes.truncate(bytes.len() - FRAME_BYTES / 2);
    let cut = decode_to_end(&write_copy(&dir, &bytes));
    cut.result.expect("a truncated tail is an end of file");
    assert_eq!(cut.skipped, 0, "{:?}", cut.last_error);
    // Only the cut frame (and at most one more) is missing.
    assert!(cut.left.len() + 2 * 1152 >= clean.left.len());
    assert!(cut.left.len() < clean.left.len());
}

#[test]
fn a_clean_file_has_no_skips_and_the_same_length_chunked_any_way() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_copy(&dir, &base_bytes());
    let a = decode_to_end(&path);
    let mut dec =
        StreamDecoder::open(&path, &Capabilities::desktop_mock(), OverspecPolicy::Downsample)
            .unwrap();
    let mut n = 0usize;
    while let Some(pcm) = dec.next_chunk(4096).unwrap() {
        n += pcm.samples.len() / 2;
    }
    assert_eq!(n, a.left.len());
    assert_eq!(dec.skipped_packets(), 0);
    assert_eq!(dec.last_decode_error(), None);
    assert!(dec.is_eof());
}
