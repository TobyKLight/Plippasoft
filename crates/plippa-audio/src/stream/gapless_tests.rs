//! SEAM_01 part A: encoder delay and padding are trimmed at decode, every length agrees, a
//! file that refuses the gapless probe still plays (untrimmed), and other formats are
//! untouched. `burst_44k_lame.mp3` is 0.5 s of a 1 kHz square wave at full level for the first
//! and the last 4000 frames and silence between, encoded by LAME (Info/LAME tag present), so
//! "the first/last non-silent frame" is exact in the source: frame 0 and frame 22049.

use super::*;
use plippa_core::Capabilities;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const SOURCE_FRAMES: u64 = 22_050;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn caps() -> Capabilities {
    Capabilities::desktop_mock()
}

/// Everything `path` decodes to (first channel), through the stream decoder.
fn stream_all(path: &Path) -> (Vec<f32>, Option<u64>) {
    let mut dec = StreamDecoder::open(path, &caps(), OverspecPolicy::Downsample).expect("open");
    drain(&mut dec)
}

fn drain(dec: &mut StreamDecoder) -> (Vec<f32>, Option<u64>) {
    let ch = dec.channels() as usize;
    let declared = dec.duration_frames();
    let mut out = Vec::new();
    while let Some(pcm) = dec.next_chunk(1024).expect("decode") {
        out.extend(pcm.samples.chunks(ch).map(|f| f[0]));
    }
    (out, declared)
}

/// The same file decoded with Symphonia's default options (no trimming): the reference for
/// "how much would have been silence".
fn untrimmed_all(path: &Path) -> Vec<f32> {
    let file = File::open(path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(path.extension().unwrap().to_str().unwrap());
    let mut format = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .unwrap()
        .format;
    let track = format.default_track().unwrap().clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .unwrap();
    let mut out = Vec::new();
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track.id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else {
            continue;
        };
        let ch = track.codec_params.channels.unwrap().count();
        let mut v = Vec::new();
        append_decoded(&decoded, ch as u16, &mut v);
        out.extend(v.chunks(ch).map(|f| f[0]));
    }
    out
}

fn first_loud(x: &[f32]) -> Option<usize> {
    x.iter().position(|v| v.abs() > 0.2)
}

fn last_loud(x: &[f32]) -> Option<usize> {
    x.iter().rposition(|v| v.abs() > 0.2)
}

#[test]
fn mp3_decodes_to_exactly_the_source_length_with_no_encoder_silence() {
    let path = fixture("burst_44k_lame.mp3");
    let (trimmed, declared) = stream_all(&path);
    let untrimmed = untrimmed_all(&path);

    // The container length and the decoded length agree, and both are the source length.
    assert_eq!(declared, Some(SOURCE_FRAMES), "n_frames");
    assert_eq!(trimmed.len() as u64, SOURCE_FRAMES, "decoded frames");
    // Without trimming there is strictly more: the delay in front and the padding behind.
    assert!(
        untrimmed.len() > trimmed.len() + 500,
        "untrimmed {} vs trimmed {}",
        untrimmed.len(),
        trimmed.len()
    );
    // The untrimmed decode starts with the encoder delay: a long run of silence before the
    // burst; trimmed, the burst starts on the very first frames.
    let lead = first_loud(&untrimmed).unwrap();
    assert!(lead > 500, "untrimmed lead-in only {lead} frames");
    let first = first_loud(&trimmed).unwrap();
    assert!(first < 64, "trimmed output starts {first} frames late");
    // And it ends on the last of the burst, not on the padding tail.
    let end = last_loud(&trimmed).unwrap();
    assert!(
        trimmed.len() - 1 - end < 64,
        "trimmed output ends {} frames after the last audio",
        trimmed.len() - 1 - end
    );
    let end_untrimmed = last_loud(&untrimmed).unwrap();
    assert!(untrimmed.len() - 1 - end_untrimmed > 200, "no padding tail to trim");
}

#[test]
fn probe_and_decode_agree_with_the_stream_on_the_trimmed_length() {
    let path = fixture("burst_44k_lame.mp3");
    let pcm = crate::decode::decode(&path).unwrap();
    assert_eq!(pcm.frame_count() as u64, SOURCE_FRAMES);
    let dec = StreamDecoder::open(&path, &caps(), OverspecPolicy::Downsample).unwrap();
    assert_eq!(dec.duration_ms(), Some(SOURCE_FRAMES * 1000 / 44_100));
}

#[test]
fn seeking_on_the_trimmed_timeline_lands_on_the_source_frame() {
    // A steady 440 Hz tone: a seek that landed `delay` frames off would be a visible phase
    // jump against the continuous decode (1105 frames is about 10 periods and a bit: an error
    // near the tone amplitude; a decoder restart costs a few hundredths).
    let path = fixture("tone_48k_3s.mp3");
    let (all, declared) = stream_all(&path);
    assert!(
        untrimmed_all(&path).len() > all.len(),
        "the fixture carries no encoder delay: nothing is being tested"
    );
    assert_eq!(declared, Some(all.len() as u64));
    let mut dec = StreamDecoder::open(&path, &caps(), OverspecPolicy::Downsample).unwrap();
    for target in [0u64, 5_000, 70_000, all.len() as u64 - 3_000] {
        dec.seek_frame(target).unwrap();
        assert_eq!(dec.position_frame(), target);
        let (tail, _) = drain(&mut dec);
        let want = &all[target as usize..];
        assert_eq!(tail.len(), want.len(), "tail length after seek to {target}");
        let worst = tail.iter().zip(want).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
        assert!(worst < 0.1, "seek to {target} differs by {worst}");
    }
}

/// A source whose first instance fails every read, later ones are fine: the gapless probe
/// (the first one) fails, the plain one succeeds.
struct FailingFirst {
    inner: File,
    fail: bool,
}
impl Read for FailingFirst {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.fail {
            return Err(std::io::Error::other("injected: gapless probe refused"));
        }
        self.inner.read(buf)
    }
}
impl Seek for FailingFirst {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(p)
    }
}
impl MediaSource for FailingFirst {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        self.inner.metadata().ok().map(|m| m.len())
    }
}

#[test]
fn a_file_that_refuses_the_gapless_probe_still_opens_and_plays_untrimmed() {
    let path = fixture("burst_44k_lame.mp3");
    let opens = Arc::new(AtomicUsize::new(0));
    let first = Arc::new(AtomicBool::new(true));
    let (o, f, p) = (opens.clone(), first.clone(), path.clone());
    let opener: SourceOpener = Arc::new(move || {
        o.fetch_add(1, Ordering::SeqCst);
        let fail = f.swap(false, Ordering::SeqCst);
        Ok(Box::new(FailingFirst {
            inner: File::open(&p)?,
            fail,
        }) as Box<dyn MediaSource>)
    });
    let mut dec =
        StreamDecoder::open_with(&path, opener, &caps(), OverspecPolicy::Downsample).expect("falls back");
    assert!(opens.load(Ordering::SeqCst) >= 2, "no second probe");
    let (got, _) = drain(&mut dec);
    // Untrimmed: exactly what the plain decode gives (delay and padding included).
    let want = untrimmed_all(&path);
    assert_eq!(got.len(), want.len());
    let worst = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
    assert!(worst < 1e-6, "fallback decode differs from the plain decode by {worst}");
    // A re-open after a seek uses the same (plain) options, so the timeline stays consistent.
    dec.seek_frame(8_000).unwrap();
    let (after, _) = drain(&mut dec);
    assert_eq!(after.len(), want.len() - 8_000);
}

#[test]
fn wav_and_flac_are_untouched_by_the_gapless_option() {
    for name in ["tone_48k_16.wav", "tone_48k.flac"] {
        let path = fixture(name);
        let (got, declared) = stream_all(&path);
        let want = untrimmed_all(&path);
        assert_eq!(got.len(), want.len(), "{name}: length");
        assert_eq!(declared, Some(want.len() as u64), "{name}: declared length");
        assert_eq!(got, want, "{name}: samples");
    }
}

#[test]
fn an_index_seek_on_a_trimmed_mp3_equals_the_plain_seek_and_stops_at_the_trimmed_end() {
    use crate::index::{build_mp3_index_path, MP3_INDEX_STRIDE_FRAMES};
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/testMedia/MP3 V0/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3");
    assert!(path.exists(), "test media missing: run scripts/fetch-testmedia.sh");
    let idx = Arc::new(build_mp3_index_path(&path, MP3_INDEX_STRIDE_FRAMES).unwrap());
    let mut plain = StreamDecoder::open(&path, &caps(), OverspecPolicy::Downsample).unwrap();
    let mut indexed = StreamDecoder::open(&path, &caps(), OverspecPolicy::Downsample).unwrap();
    indexed.set_index(idx);
    let len = plain.duration_frames().expect("Xing length");
    // Close to the end so the padding matters; far enough back that the index is used.
    let target = len - 30_000;
    plain.seek_frame(target).unwrap();
    indexed.seek_frame(target).unwrap();
    assert_eq!(indexed.indexed_seeks(), 1, "the index was not used");
    assert_eq!(plain.position_frame(), indexed.position_frame());
    let (a, _) = drain(&mut plain);
    let (b, _) = drain(&mut indexed);
    assert_eq!(a.len() as u64, 30_000, "plain tail");
    assert_eq!(b.len(), a.len(), "the indexed tail keeps the encoder padding");
    let worst = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max);
    assert!(worst < 1e-4, "indexed tail differs by {worst}");
}

#[test]
fn a_headerless_vbr_mp3_is_never_cut_at_an_estimated_length() {
    // 0.5 s at 320 kb/s then 1.5 s at 32 kb/s, no Xing/Info frame: the reader's length
    // estimate (from the first frames' size) is about a third of the real one. With gapless
    // on it would cut the file there; there is no encoder delay to trim, so it must not.
    let path = fixture("headerless_vbr_44k.mp3");
    let (got, _) = stream_all(&path);
    let want = untrimmed_all(&path);
    assert!(want.len() > 80_000, "fixture is {} frames", want.len());
    assert_eq!(got.len(), want.len(), "the decoded length changed");
}
