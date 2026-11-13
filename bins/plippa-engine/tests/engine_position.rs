//! WARP_03: exact position model through the engine.
//!
//! Frame-index WAVs (sample value = frame number mod 65536) let a test read *which source
//! frame* a captured output sample came from. Logic only: no assertion depends on wall-clock
//! time; the tests poll for state (seek landed, EOF reached, audio thread blocked in write).

use plippa_audio::testsupport::{frame_index_from_f32, frame_index_sample, write_pcm16_wav, Lcg};
use plippa_audio::{AudioError, AudioSink};
use plippa_core::{Capabilities, TransportState, Volume};
use plippa_engine::{Engine, EngineEvent};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand, PlaybackStatus};
use plippa_noubin::{
    encode_playable_id, normalise_noubin_key, LibraryReader, LibraryRoot, LibraryWriter,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// 48 kHz: 1 ms is exactly 48 frames, so ms targets and frame targets agree.
const RATE: u32 = 48_000;
const FRAMES: usize = 60_000; // 1.25 s, below the 65536 frame-index wrap

fn index_wav(path: &Path) {
    write_pcm16_wav(path, RATE, 1, FRAMES, |f, _| frame_index_sample(f)).unwrap();
}

fn library() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().to_path_buf();
    index_wav(&root.join("Music/Album/01.wav"));
    let key = "pos.noubin.a";
    let noudata = format!(
        r#"{{"noubinKey":"{key}","noubinKeyNormalised":"{}","playableItemTitle":"Pos","playableItemProfile":"music",
           "releaseData":{{}},"mediaList":[{{"mediaTitle":"T1","localFilePaths":["01.wav"]}}]}}"#,
        normalise_noubin_key(key)
    );
    let p = root.join("Music/Album/_album.noudata");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, noudata).unwrap();
    LibraryWriter::open(&LibraryRoot::new(&root))
        .unwrap()
        .full_scan()
        .unwrap();
    (t, root)
}

fn gid(root: &Path) -> plippa_core::PlayableItemId {
    let reader = LibraryReader::open(&LibraryRoot::new(root)).unwrap();
    let rec = reader
        .list_playables()
        .unwrap()
        .into_iter()
        .find(|p| p.title.as_deref() == Some("Pos"))
        .expect("playable Pos");
    plippa_core::PlayableItemId::new(encode_playable_id(root, rec.id))
}

fn cmd(e: &mut Engine, c: PlaybackCommand) {
    e.handle_ipc(IpcMessage::PlaybackCommand(c));
}

fn status(e: &mut Engine) -> PlaybackStatus {
    e.take_events();
    e.publish_status();
    e.take_events()
        .into_iter()
        .rev()
        .find_map(|ev| match ev {
            EngineEvent::Status(s) => Some(s),
            _ => None,
        })
        .expect("status")
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(20), "timed out: {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

fn wait_landed(e: &mut Engine, frame: u64) {
    wait_until("seek applied", || {
        e.poll_stream_lifecycle();
        e.current_position_frames() == Some(frame)
    });
}

/// Mock-sink engine with the stream prepared (not playing) at full volume. (`Load` leaves the
/// transport Stopped, and a Play from Stopped restarts at 0, so tests that seek-then-play use
/// [`paused_gated`] instead.)
fn prepared(root: &Path) -> Engine {
    let mut e = Engine::open_mock(root, Capabilities::desktop_mock()).unwrap();
    cmd(&mut e, PlaybackCommand::SetVolume(Volume::new(100)));
    cmd(&mut e, PlaybackCommand::Load(gid(root)));
    wait_until("stream open", || e.current_position_frames() == Some(0));
    e
}

#[test]
fn seeks_land_on_the_target_and_position_matches_the_output() {
    let (_t, root) = library();
    for target_ms in [0u64, 1, 37, 500, 777, 1000, 1200, 1249] {
        let target = target_ms * 48;
        let (mut e, gate) = paused_gated(&root, 0);
        let marker = gate.out.lock().unwrap().len();
        cmd(&mut e, PlaybackCommand::SeekMs(target_ms));
        // Right after the command the legacy rule applies: the requested target.
        assert_eq!(e.current_position_ms(), Some(target_ms));
        wait_landed(&mut e, target);
        e.poll_stream_lifecycle();
        assert_eq!(e.current_position_ms(), Some(target_ms), "landed");
        assert_eq!(e.current_position_frames(), Some(target));
        assert_eq!(
            gate.out.lock().unwrap().len(),
            marker,
            "paused: nothing written"
        );

        gate.allowed.store(usize::MAX, Ordering::SeqCst);
        cmd(&mut e, PlaybackCommand::Play);
        // While playing, the reported position is never ahead of the captured audio
        // (read the position first: the capture only grows).
        wait_until("plays to the end", || {
            e.poll_stream_lifecycle();
            let p = e.current_position_frames().unwrap();
            let captured = (gate.out.lock().unwrap().len() - marker) as u64;
            assert!(
                p <= target + captured,
                "position {p} ahead of output {}",
                target + captured
            );
            assert!(p >= target);
            e.session().transport == TransportState::Stopped
        });
        let out = gate.out.lock().unwrap().clone();
        let out = &out[marker..];
        assert_eq!(out.len(), FRAMES - target as usize, "target {target_ms}");
        for (k, s) in out.iter().enumerate() {
            assert_eq!(
                frame_index_from_f32(*s),
                (target as usize + k) % 65_536,
                "target {target_ms}: output frame {k}"
            );
        }
        // Everything written was heard (queue 0): position is exactly the end.
        assert_eq!(e.current_position_frames(), Some(FRAMES as u64));
        assert_eq!(e.current_position_ms(), Some(FRAMES as u64 / 48));
    }
}

#[test]
fn sum_of_relative_seeks_via_the_engine_is_exact() {
    let (_t, root) = library();
    let mut e = prepared(&root);
    let mut rng = Lcg::new(11);
    let start_ms = 600u64;
    cmd(
        &mut e,
        PlaybackCommand::SeekTo {
            target_ms: start_ms,
        },
    );
    wait_landed(&mut e, start_ms * 48);
    let mut expected_ms = start_ms as i64;
    for _ in 0..30 {
        let here_ms = e.current_position_ms().unwrap() as i64;
        assert_eq!(
            here_ms, expected_ms,
            "position tracks the exact running total"
        );
        let mut delta = rng.below(401) as i64 - 200;
        if !(10..=1_000).contains(&(here_ms + delta)) {
            delta = -delta;
        }
        if delta == 0 {
            delta = 1;
        }
        let target = (here_ms + delta) as u64;
        cmd(&mut e, PlaybackCommand::SeekTo { target_ms: target });
        wait_landed(&mut e, target * 48);
        expected_ms += delta;
    }
    assert_eq!(e.current_position_frames(), Some(expected_ms as u64 * 48));
    assert_eq!(e.current_position_ms(), Some(expected_ms as u64));
    // Landing is the same after a final status round trip.
    assert_eq!(status(&mut e).session.position_ms, expected_ms as u64);
}

#[test]
fn published_defaults_and_duration_flag() {
    let (_t, root) = library();
    let e = prepared(&root);
    assert_eq!(e.audio_rate_micro(), Some(1_000_000));
    assert!(!e.audio_warp_active());
    assert!(e.duration_known());
    assert_eq!(e.current_duration_ms(), Some(1250));
    assert_eq!(e.stream_sample_rate_hz(), 48_000);
}

/// Sink that records output, reports a fixed queue and blocks inside `write` number
/// `block_at` until the test releases it (so the test can inspect state between writes).
struct GateSink {
    out: Arc<Mutex<Vec<f32>>>,
    writes: Arc<AtomicUsize>,
    allowed: Arc<AtomicUsize>,
    blocked: Arc<AtomicBool>,
    flushes: Arc<AtomicUsize>,
    queue: u64,
}

impl AudioSink for GateSink {
    fn channels(&self) -> u16 {
        1
    }
    fn sample_rate_hz(&self) -> u32 {
        RATE
    }
    fn write(&mut self, interleaved: &[f32]) -> Result<(), AudioError> {
        let n = self.writes.load(Ordering::SeqCst);
        if n >= self.allowed.load(Ordering::SeqCst) {
            self.blocked.store(true, Ordering::SeqCst);
            while n >= self.allowed.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(1));
            }
            self.blocked.store(false, Ordering::SeqCst);
        }
        self.out.lock().unwrap().extend_from_slice(interleaved);
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), AudioError> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn queue_frames(&mut self) -> u64 {
        self.queue
    }
}

struct Gate {
    out: Arc<Mutex<Vec<f32>>>,
    writes: Arc<AtomicUsize>,
    allowed: Arc<AtomicUsize>,
    blocked: Arc<AtomicBool>,
    flushes: Arc<AtomicUsize>,
}

impl Drop for Gate {
    fn drop(&mut self) {
        // Never leave the audio thread parked (Engine::drop joins it), also on a panic.
        self.allowed.store(usize::MAX, Ordering::SeqCst);
    }
}

/// Engine on a gated sink, paused just after the first write (`allowed` is then 1 and the
/// audio thread idle): position = the frames of that first chunk (minus nothing: see
/// `queue`). Tests then seek, raise `gate.allowed` and press Play.
fn paused_gated(root: &Path, queue: u64) -> (Engine, Gate) {
    let gate = Gate {
        out: Arc::new(Mutex::new(Vec::new())),
        writes: Arc::new(AtomicUsize::new(0)),
        allowed: Arc::new(AtomicUsize::new(0)),
        blocked: Arc::new(AtomicBool::new(false)),
        flushes: Arc::new(AtomicUsize::new(0)),
    };
    let (out, writes, allowed, blocked, flushes) = (
        Arc::clone(&gate.out),
        Arc::clone(&gate.writes),
        Arc::clone(&gate.allowed),
        Arc::clone(&gate.blocked),
        Arc::clone(&gate.flushes),
    );
    let mut e = Engine::open_with_sink_factory(
        root,
        Capabilities::desktop_mock(),
        move |_ch: u16, _rate: u32| -> Result<Box<dyn AudioSink>, AudioError> {
            Ok(Box::new(GateSink {
                out: Arc::clone(&out),
                writes: Arc::clone(&writes),
                allowed: Arc::clone(&allowed),
                blocked: Arc::clone(&blocked),
                flushes: Arc::clone(&flushes),
                queue,
            }))
        },
    )
    .unwrap();
    cmd(&mut e, PlaybackCommand::SetVolume(Volume::new(100)));
    cmd(&mut e, PlaybackCommand::LoadAndPlay(gid(root)));
    wait_until("parked in the first write", || {
        gate.blocked.load(Ordering::SeqCst)
    });
    // Pause is queued behind the parked write; releasing exactly one write lets the audio
    // thread see it before it can write anything else.
    cmd(&mut e, PlaybackCommand::Pause);
    gate.allowed.store(1, Ordering::SeqCst);
    wait_until("paused", || gate.flushes.load(Ordering::SeqCst) >= 1);
    wait_until("position published", || {
        e.current_position_frames().is_some()
    });
    (e, gate)
}

#[test]
fn heard_position_subtracts_the_sink_queue() {
    let (_t, root) = library();
    const Q: u64 = 3_840; // 80 ms
    let (mut e, gate) = paused_gated(&root, Q);
    let marker = gate.out.lock().unwrap().len();
    let start = 209 * 48; // 209 ms = 10032 frames, an exact ms boundary
    cmd(&mut e, PlaybackCommand::SeekMs(start / 48));
    wait_landed(&mut e, start);
    // Let exactly 40 more writes through, then the audio thread parks in the next one.
    let w0 = gate.writes.load(Ordering::SeqCst);
    gate.allowed.store(w0 + 40, Ordering::SeqCst);
    cmd(&mut e, PlaybackCommand::Play);
    wait_until("parked in write", || {
        gate.blocked.load(Ordering::SeqCst) && gate.writes.load(Ordering::SeqCst) == w0 + 40
    });
    let out = gate.out.lock().unwrap().clone();
    let written = (out.len() - marker) as u64;
    assert!(
        written > Q,
        "enough written for the queue to matter ({written})"
    );
    wait_until("position published", || {
        e.current_position_frames() == Some(start + written - Q)
    });
    // And the written audio really is source frames start..start+written.
    assert_eq!(frame_index_from_f32(out[marker]), start as usize);
    assert_eq!(
        frame_index_from_f32(out[out.len() - 1]),
        (start + written - 1) as usize
    );
    // The legacy ms view follows the same heard position.
    assert_eq!(e.current_position_ms(), Some((start + written - Q) / 48));
}

#[test]
fn near_the_start_the_heard_position_does_not_precede_the_landing() {
    let (_t, root) = library();
    let (mut e, gate) = paused_gated(&root, 3_840);
    let marker = gate.out.lock().unwrap().len();
    let start = 24_000u64;
    cmd(&mut e, PlaybackCommand::SeekMs(start / 48));
    wait_landed(&mut e, start);
    let w0 = gate.writes.load(Ordering::SeqCst);
    gate.allowed.store(w0 + 2, Ordering::SeqCst); // two small chunks: less than the queue
    cmd(&mut e, PlaybackCommand::Play);
    wait_until("parked in write", || {
        gate.blocked.load(Ordering::SeqCst) && gate.writes.load(Ordering::SeqCst) == w0 + 2
    });
    let written = (gate.out.lock().unwrap().len() - marker) as u64;
    assert!(written > 0 && written < 3_840, "{written}");
    assert_eq!(e.current_position_frames(), Some(start));
}

#[test]
fn pause_puts_back_the_unheard_queue_so_resume_is_gapless() {
    let (_t, root) = library();
    const Q: u64 = 3_840;
    let (mut e, gate) = paused_gated(&root, Q);
    let marker = gate.out.lock().unwrap().len();
    cmd(&mut e, PlaybackCommand::SeekMs(0));
    // (WARP_04: a seek no longer flushes the sink, it crossfades, so only the landing is awaited.)
    wait_landed(&mut e, 0);
    let w0 = gate.writes.load(Ordering::SeqCst);
    let flushes0 = gate.flushes.load(Ordering::SeqCst);
    gate.allowed.store(w0 + 40, Ordering::SeqCst);
    cmd(&mut e, PlaybackCommand::Play);
    wait_until("parked in write", || {
        gate.blocked.load(Ordering::SeqCst) && gate.writes.load(Ordering::SeqCst) == w0 + 40
    });
    // Queue the pause while the thread is parked, then allow exactly one more write: the
    // pause is handled at the top of the next loop turn, before any further write.
    cmd(&mut e, PlaybackCommand::Pause);
    gate.allowed.store(w0 + 41, Ordering::SeqCst);
    wait_until("pause flushed", || {
        gate.flushes.load(Ordering::SeqCst) > flushes0
    });
    let captured = (gate.out.lock().unwrap().len() - marker) as u64;
    assert!(captured > Q);
    wait_until("paused position published", || {
        e.current_position_frames() == Some(captured - Q)
    });
    // Resume and run to the end.
    gate.allowed.store(usize::MAX, Ordering::SeqCst);
    cmd(&mut e, PlaybackCommand::Play);
    wait_until("plays to the end", || {
        e.poll_stream_lifecycle();
        e.session().transport == TransportState::Stopped
    });
    let all = gate.out.lock().unwrap().clone();
    let out = &all[marker..];
    // What the listener heard: everything captured except the flushed (unheard) tail.
    let mut heard: Vec<f32> = out[..(captured - Q) as usize].to_vec();
    heard.extend_from_slice(&out[captured as usize..]);
    assert_eq!(heard.len(), FRAMES, "no frame skipped or repeated");
    for (k, s) in heard.iter().enumerate() {
        assert_eq!(frame_index_from_f32(*s), k % 65_536, "heard frame {k}");
    }
    assert_eq!(e.current_position_frames(), Some(FRAMES as u64));
}
