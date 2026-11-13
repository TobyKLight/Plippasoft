//! WARP_04: `SourceWindow` read path and crossfaded hard seeks, driven through the audio
//! thread with a gated capture sink (no real device, no timing assertions: the sink parks the
//! audio thread inside `write`, so the test inspects state between writes).

use plippa_audio::testsupport::{
    click_metric, frame_index_from_f32, smooth_bound, write_pcm16_wav, Lcg,
};
use plippa_audio::{
    OverspecPolicy, StreamDecoder, WindowConfig, XfadeConfig, REFILL_CHUNKS_PER_TURN,
    START_CHUNK_FRAMES,
};
use plippa_core::Capabilities;
use plippa_engine::{AudioCommand, AudioThreadConfig};
use std::path::Path;
use std::sync::atomic::Ordering;

mod common;
use common::*;

#[test]
fn bypass_output_is_bit_identical_to_the_decoder_times_volume() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("noise.wav");
    let mut rng = Lcg::new(3);
    write_pcm16_wav(&wav, 44_100, 2, 200_000, |_, _| {
        (rng.below(65_536) as i32 - 32_768) as i16
    })
    .unwrap();
    let mp3 = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/testMedia/MP3 V0/Bogan Mongrel - Grooves For Machines To Study - 02 Beat For Machines To Study.mp3");
    let mut files = vec![wav];
    if mp3.exists() {
        files.push(mp3);
    }
    for path in files {
        let want_frames = 150_000usize;
        let gain = 0.37f32;
        // Reference: the plain decoder output, gain and clamp applied the old way.
        let caps = Capabilities::desktop_mock();
        let mut dec = StreamDecoder::open(&path, &caps, OverspecPolicy::default()).unwrap();
        let ch = dec.channels() as usize;
        let mut expect: Vec<f32> = Vec::new();
        while expect.len() < want_frames * ch {
            let pcm = dec.next_chunk(4096).unwrap().expect("long enough");
            expect.extend(pcm.samples.iter().map(|s| (*s * gain).clamp(-1.0, 1.0)));
        }
        expect.truncate(want_frames * ch);

        let mut rig = Rig::new(ch as u16, want_frames * ch, AudioThreadConfig::default());
        rig.audio.try_send(AudioCommand::SetVolume(gain));
        rig.start(&path, true);
        rig.allowed.store(usize::MAX, Ordering::SeqCst);
        wait_until("captured", || {
            rig.out.lock().unwrap().len() >= want_frames * ch
        });
        let got = rig.out.lock().unwrap()[..want_frames * ch].to_vec();
        let diff = got
            .iter()
            .zip(&expect)
            .position(|(a, b)| a.to_bits() ^ b.to_bits() != 0);
        assert_eq!(diff, None, "{}: first differing sample", path.display());
        let _ = rig.audio.try_send(AudioCommand::Stop);
    }
}

#[test]
fn first_write_does_not_wait_for_the_window_to_fill() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    index_wav(&wav, 60_000);
    let mut rig = Rig::new(1, usize::MAX, AudioThreadConfig::default());
    rig.start(&wav, true);
    rig.run_writes(0);
    assert_eq!(
        rig.audio
            .stats
            .first_write_window_frames
            .load(Ordering::SeqCst),
        0
    );
    rig.run_writes(1);
    // The start chunk plus the same two-chunk top-up the old prefetch did before its first
    // write; the window does not wait for its 17 s to fill. (A count of frames, not a time.)
    let held = rig
        .audio
        .stats
        .first_write_window_frames
        .load(Ordering::SeqCst);
    assert!(
        held >= START_CHUNK_FRAMES as u64
            && held <= (START_CHUNK_FRAMES * (1 + REFILL_CHUNKS_PER_TURN)) as u64,
        "frames held at the first write: {held}"
    );
}

#[test]
fn seek_storm_stays_under_the_cap_lands_exactly_and_uses_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("long.wav");
    let total_frames = 20 * 60 * RATE as usize; // 20 min mono: far larger than any window
                                                // Frame-index sample (frame mod 65536): a captured sample names its source frame.
    index_wav(&wav, total_frames);
    let mut rig = Rig::new(1, usize::MAX, AudioThreadConfig::default());
    rig.start(&wav, true);
    rig.run_writes(50);
    let mut rng = Lcg::new(99);
    let mut in_window_seeks = 0u64;
    for k in 0..1000u32 {
        // Run a random number of writes so the window fills ahead of the playhead.
        let more = if k % 50 == 0 {
            300
        } else {
            rng.below(40) as usize
        };
        if more > 0 {
            rig.run_writes(more);
        }
        let here_ms = rig.frames() / 48;
        let before = rig.audio.stats.decoder_seeks.load(Ordering::SeqCst);
        let target_ms = if k % 2 == 0 {
            // Near: within a few seconds either side (mostly inside the window).
            let d = rng.below(3_000) as i64 - 1_500;
            (here_ms as i64 + d).clamp(0, 1_100_000) as u64
        } else {
            rng.below(1_150_000)
        };
        rig.seek_parked(target_ms);
        let after = rig.audio.stats.decoder_seeks.load(Ordering::SeqCst);
        if after == before {
            in_window_seeks += 1;
        }
        // Lands within one frame (exactly, at 48 kHz) and the window never exceeds the cap.
        assert_eq!(rig.frames(), target_ms * 48, "seek {k} to {target_ms} ms");
        assert!(
            rig.audio.stats.peak_window_bytes.load(Ordering::SeqCst) <= 6 * 1024 * 1024,
            "window over the 6 MiB cap"
        );
        // Every 25th seek: read the audio that follows; after the crossfade it is exactly the
        // target's frames.
        if k % 25 == 0 {
            let marker = rig.out.lock().unwrap().len();
            rig.run_writes(3);
            let out = rig.out.lock().unwrap();
            let fade = XfadeConfig::default().seek_frames(RATE);
            for i in fade..3 * BLOCK {
                let want = (target_ms as usize * 48 + i) % 65_536;
                assert_eq!(
                    frame_index_from_f32(out[marker + i]),
                    want,
                    "seek {k} frame {i}"
                );
            }
        }
    }
    let seeks = rig.audio.stats.decoder_seeks.load(Ordering::SeqCst);
    assert!(
        in_window_seeks > 50,
        "near seeks were served from the window ({in_window_seeks})"
    );
    assert!(
        seeks > 100 && seeks < 1000,
        "far seeks used the decoder ({seeks})"
    );
    assert!(rig.audio.stats.seek_crossfades.load(Ordering::SeqCst) >= 900);
    let peak = rig.audio.stats.peak_window_bytes.load(Ordering::SeqCst);
    assert!(peak > 0 && peak <= 6 * 1024 * 1024, "peak {peak}");
}

fn render_with_seek(cfg: AudioThreadConfig) -> Vec<f32> {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("sine.wav");
    sine_wav(&wav, 4 * RATE as usize, 0.5);
    let mut rig = Rig::new(1, usize::MAX, cfg);
    rig.start(&wav, true);
    rig.run_writes(9);
    // The decode worker runs on its own thread and only has `MAX_INFLIGHT_BLOCKS` queued at a
    // time (the audio thread applies them while writing): wait until it has queued a full
    // set so the old continuation is complete when the seek arrives. Without this a loaded
    // box left the window short and the "old" signal was zero-padded: a test race, not a
    // product click.
    wait_until("worker has decoded ahead", || {
        let g = rig.audio.stats.worker.lock().unwrap();
        g.as_ref().is_some_and(|w| {
            w.blocks_sent.load(Ordering::SeqCst) >= w.blocks_applied.load(Ordering::SeqCst) + 4
        })
    });
    // One more write: its pump applies what the worker queued, so the window ahead of the
    // playhead is complete when the seek is handled.
    rig.run_writes(1);
    // A target whose phase differs from the old continuation by 0.3..0.7 periods.
    let old = (10 * BLOCK) as f64;
    let ms = (2_000u64..)
        .find(|ms| {
            let f = 1013.0 * ((*ms * 48) as f64 - old) / f64::from(RATE);
            (0.3..0.7).contains(&f.fract())
        })
        .unwrap();
    rig.seek_parked(ms);
    rig.run_writes(10);
    let out = rig.out.lock().unwrap().clone();
    out
}

#[test]
fn hard_seek_is_a_crossfade_not_a_click() {
    let bound = smooth_bound(0.5, 1013.0, RATE);
    let faded = render_with_seek(AudioThreadConfig::default());
    let m = click_metric(&faded, bound);
    assert!(m < 1.5, "click metric through the crossfaded seek {m}");
    // Same seek with a 1-frame "crossfade" is a plain cut: the test can tell the difference.
    let cut_cfg = AudioThreadConfig {
        rate: Default::default(),
        stretch: Default::default(),
        chase: Default::default(),
        window: WindowConfig::default(),
        xfade: XfadeConfig {
            xfade_seek_ms: 0.0,
            ..XfadeConfig::default()
        },
        decoder_hook: None,
        isolation: None,
        scan: Default::default(),
        library_roots: Vec::new(),
    };
    let cut = render_with_seek(cut_cfg);
    assert!(
        click_metric(&cut, bound) > 5.0,
        "the unfaded seek must click"
    );
}

#[test]
fn seek_inside_the_window_issues_no_decoder_seek_and_back_in_history_works() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("idx.wav");
    index_wav(&wav, 60_000);
    let mut rig = Rig::new(1, usize::MAX, AudioThreadConfig::default());
    rig.start(&wav, true);
    rig.run_writes(20); // ~20 chunks played, more decoded ahead
    assert_eq!(rig.audio.stats.decoder_seeks.load(Ordering::SeqCst), 0);
    // Back into history (the window holds everything since 0) and forward into the lookahead.
    rig.seek_parked(100);
    assert_eq!(rig.frames(), 100 * 48);
    rig.seek_parked(300);
    assert_eq!(rig.frames(), 300 * 48);
    assert_eq!(
        rig.audio.stats.decoder_seeks.load(Ordering::SeqCst),
        0,
        "both targets were inside the window"
    );
    // Past the window: the decoder is used.
    rig.seek_parked(1_200);
    assert_eq!(rig.audio.stats.decoder_seeks.load(Ordering::SeqCst), 1);
}
