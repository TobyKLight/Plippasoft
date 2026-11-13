//! Integration tests for plippa-audio (fixtures under tests/fixtures/).

use std::path::{Path, PathBuf};

use plippa_audio::{
    decode, prepare_for_playback, select_best_local_path, validate_for_stream, AudioSink, Mixer,
    MockSink, OverspecPolicy, PcmBuffer, StreamDecoder, Volume, DEFAULT_CHUNK_FRAMES,
    MAX_STREAM_BUFFER_BYTES, PREFETCH_TARGET_BYTES, REFILL_CHUNKS_PER_TURN, START_CHUNK_FRAMES,
    START_PREFETCH_BYTES,
};
use plippa_core::Capabilities;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn play_to_mock(path: &Path) -> MockSink {
    let caps = Capabilities::desktop_mock();
    let pcm = prepare_for_playback(path, &caps, OverspecPolicy::Downsample).expect("prepare");
    let mut sink = MockSink::new(pcm.channels, pcm.sample_rate_hz);
    sink.write(&pcm.samples).expect("write");
    sink
}

#[test]
fn decode_wav_fixture() {
    let sink = play_to_mock(&fixture("tone_48k_16.wav"));
    assert_eq!(sink.channels(), 2);
    assert_eq!(sink.sample_rate_hz(), 48_000);
    assert!(sink.frame_count() > 1000);
    assert!(sink.captured.iter().any(|s| s.abs() > 0.01));
}

#[test]
fn decode_mp3_fixture() {
    let sink = play_to_mock(&fixture("tone_48k.mp3"));
    assert_eq!(sink.channels(), 2);
    assert!(sink.frame_count() > 100);
    assert!(sink.captured.iter().any(|s| s.abs() > 0.01));
}

#[test]
fn decode_flac_fixture() {
    let sink = play_to_mock(&fixture("tone_48k.flac"));
    assert_eq!(sink.channels(), 2);
    assert_eq!(sink.sample_rate_hz(), 48_000);
    assert!(sink.frame_count() > 1000);
    assert!(sink.captured.iter().any(|s| s.abs() > 0.01));
}

#[test]
fn best_path_prefers_flac_over_mp3() {
    let caps = Capabilities::desktop_mock();
    let paths = [
        fixture("tone_48k.mp3"),
        fixture("tone_48k_16.wav"),
        fixture("tone_48k.flac"),
    ];
    let best = select_best_local_path(&paths, &caps, OverspecPolicy::Downsample).unwrap();
    assert_eq!(best.file_name().unwrap(), "tone_48k.flac");
}

#[test]
fn refuse_when_sample_rate_and_bit_depth_exceed() {
    let caps = Capabilities::desktop_mock();
    let err = prepare_for_playback(
        &fixture("overspec_192k_24.wav"),
        &caps,
        OverspecPolicy::Refuse,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("192000") && msg.contains("24") && msg.contains("48000") && msg.contains("16"),
        "clear Noubin-style error expected, got: {msg}"
    );
}

#[test]
fn downsample_when_overspec() {
    let caps = Capabilities::desktop_mock();
    let pcm = prepare_for_playback(
        &fixture("overspec_192k_24.wav"),
        &caps,
        OverspecPolicy::Downsample,
    )
    .unwrap();
    assert_eq!(pcm.sample_rate_hz, 48_000);
    assert!(pcm.samples.iter().any(|s| s.abs() > 0.01));
}

#[test]
fn mock_sink_frame_count_matches_fixture() {
    let pcm = decode(&fixture("tone_48k_16.wav")).unwrap();
    let mut sink = MockSink::new(pcm.channels, pcm.sample_rate_hz);
    sink.write(&pcm.samples).unwrap();
    assert_eq!(sink.frame_count(), pcm.frame_count());
    assert_eq!(sink.channels(), 2);
}

#[test]
fn sfx_mix_alters_output_and_does_not_panic() {
    let silence = PcmBuffer {
        samples: vec![0.0f32; 48_000 / 10 * 2], // 100ms stereo silence
        channels: 2,
        sample_rate_hz: 48_000,
    };
    let sfx = decode(&fixture("tone_48k_16.wav")).unwrap();
    // Trim SFX to ~50ms at 48k for a short window.
    let sfx_frames = (48_000 / 20) * 2;
    let sfx = PcmBuffer {
        samples: sfx.samples.into_iter().take(sfx_frames).collect(),
        channels: 2,
        sample_rate_hz: 48_000,
    };

    let mut mixer = Mixer::new(2, 48_000);
    mixer.sfx_gain = 1.0;
    mixer.set_main(silence);
    let mut main_only = vec![0.0f32; 4800]; // 50ms
    mixer.render(&mut main_only);
    assert!(main_only.iter().all(|s| *s == 0.0));

    let mut mixer = Mixer::new(2, 48_000);
    mixer.sfx_gain = 1.0;
    mixer.set_main(PcmBuffer {
        samples: vec![0.0f32; 48_000 / 10 * 2],
        channels: 2,
        sample_rate_hz: 48_000,
    });
    mixer.trigger_sfx(sfx);
    let mut with_sfx = vec![0.0f32; 4800];
    mixer.render(&mut with_sfx);
    assert!(
        with_sfx.iter().any(|s| s.abs() > 0.01),
        "SFX window must be audible (non-zero) in mock capture"
    );
}

#[test]
fn volume_zero_produces_silence() {
    let v = Volume::new(0.0);
    let mut samples = vec![0.25f32; 64];
    v.apply(&mut samples);
    assert!(samples.iter().all(|s| (*s).abs() < 1e-6));
}

#[test]
fn stream_play_writes_before_eof() {
    let caps = Capabilities::desktop_mock();
    let mut dec =
        StreamDecoder::open(&fixture("tone_48k_16.wav"), &caps, OverspecPolicy::Downsample)
            .expect("open");
    let mut sink = MockSink::new(dec.channels(), dec.sample_rate_hz());
    let first = dec
        .next_chunk(DEFAULT_CHUNK_FRAMES)
        .expect("chunk")
        .expect("first pcm");
    sink.write(&first.samples).expect("first write");
    assert!(sink.frame_count() > 0, "first sink write before EOF");
    assert!(!dec.is_eof(), "decoder must not be at EOF after first chunk");
    // Drain remaining without accumulating full-track buffer on the sink side beyond writes.
    while let Some(pcm) = dec.next_chunk(DEFAULT_CHUNK_FRAMES).expect("chunk") {
        sink.write(&pcm.samples).expect("write");
    }
    assert!(dec.is_eof());
}

#[test]
fn validate_for_stream_allows_underspec_at_native_rate() {
    let caps = Capabilities::desktop_mock();
    // 48k fixture — rate equals max; stream path must not require prepare upsample.
    let probe = validate_for_stream(
        &fixture("tone_48k_16.wav"),
        &caps,
        OverspecPolicy::Downsample,
    )
    .expect("validate");
    assert_eq!(probe.sample_rate_hz, 48_000);
}

#[test]
fn stream_buffer_cap_documented() {
    // Architecture assert: prefetch budget stays well under a few MB.
    assert!(MAX_STREAM_BUFFER_BYTES <= 2 * 1024 * 1024);
    assert!(MAX_STREAM_BUFFER_BYTES >= 256 * 1024);
    assert!(START_PREFETCH_BYTES < PREFETCH_TARGET_BYTES);
    assert!(PREFETCH_TARGET_BYTES <= MAX_STREAM_BUFFER_BYTES / 2);
    assert!(REFILL_CHUNKS_PER_TURN >= 1);
    assert!(START_CHUNK_FRAMES >= 64);
    assert!(START_CHUNK_FRAMES <= DEFAULT_CHUNK_FRAMES);
}
