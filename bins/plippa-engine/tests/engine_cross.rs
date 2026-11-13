//! WARP_19: scan across a track boundary ("Seek outside track") on the audio thread. Logic and
//! sample maths only: the gated sink parks the audio thread between writes and the scan clock
//! is audio time, so nothing depends on wall-clock time. Forward crossings are compared
//! sample for sample with the same scan over one file holding the tracks back to back (a
//! seamless join is exactly that); reverse ones use the click metric (a reverse stall fades by
//! design, so they cannot be bit-compared under thread scheduling).

mod common;
use common::*;
use plippa_audio::testsupport::GatedBlocks;
use plippa_audio::BlockSource;
use plippa_audio::testsupport::{click_metric, smooth_bound, write_pcm16_wav};
use plippa_core::{Capabilities, ScanDir, WarpMode};
use plippa_engine::{AudioCommand, AudioThreadConfig, CrossEvent, NeighbourSrc, DecoderHook};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const TRACK_FRAMES: usize = 3 * RATE as usize;
const AMP: f64 = 0.5;

/// `frames` frames of the 1013 Hz sine starting at global frame `from`, so consecutive tracks
/// join into one continuous tone.
fn tone(path: &Path, from: usize, frames: usize) {
    write_pcm16_wav(path, RATE, 1, frames, |f, _| {
        let v =
            AMP * (2.0 * std::f64::consts::PI * 1013.0 * (from + f) as f64 / f64::from(RATE)).sin();
        (v * 32_767.0).round() as i16
    })
    .unwrap();
}

struct Set {
    _dir: tempfile::TempDir,
    a: PathBuf,
    b: PathBuf,
    c: PathBuf,
    /// A, B and C back to back.
    abc: PathBuf,
}

fn set() -> Set {
    let dir = tempfile::tempdir().unwrap();
    let p = |n: &str| dir.path().join(n);
    let (a, b, c, abc) = (p("a.wav"), p("b.wav"), p("c.wav"), p("abc.wav"));
    tone(&a, 0, TRACK_FRAMES);
    tone(&b, TRACK_FRAMES, TRACK_FRAMES);
    tone(&c, 2 * TRACK_FRAMES, TRACK_FRAMES);
    tone(&abc, 0, 3 * TRACK_FRAMES);
    Set {
        _dir: dir,
        a,
        b,
        c,
        abc,
    }
}

fn src(p: &Path) -> Option<NeighbourSrc> {
    Some(NeighbourSrc {
        path: p.to_path_buf(),
        capabilities: Capabilities::desktop_mock(),
        overspec: Default::default(),
        // These rig tests exercise the audio thread directly; the cross event's identity is
        // only meaningful to the control plane.
        item_position: 0,
        seamless: false,
        scan: true,
    })
}

fn neighbours(rig: &Rig, prev: Option<&Path>, next: Option<&Path>) {
    rig.audio.try_send(AudioCommand::SetNeighbours {
        prev: prev.and_then(src),
        next: next.and_then(src),
    });
}

fn scan(rig: &Rig, dir: ScanDir) {
    rig.audio.try_send(AudioCommand::ScanStart {
        dir,
        mode: WarpMode::PitchShift,
    });
}

fn crossings(rig: &Rig) -> u64 {
    rig.audio.stats.crossings.load(Ordering::SeqCst)
}

fn scanning(rig: &Rig) -> bool {
    rig.audio.stats.scan_active.load(Ordering::SeqCst)
}

fn events(rig: &Rig) -> Vec<CrossEvent> {
    rig.audio.stats.cross_events.lock().unwrap().clone()
}

/// One more write.
fn step(rig: &Rig) {
    let target = rig.writes.load(Ordering::SeqCst) + 1;
    rig.allowed.store(target, Ordering::SeqCst);
    let t = std::time::Instant::now();
    while !(rig.writes.load(Ordering::SeqCst) == target && rig.blocked.load(Ordering::SeqCst)) {
        assert!(
            t.elapsed() < std::time::Duration::from_secs(10),
            "no write: frames {} scanning {} starved {} crossings {} rate {} playing {}",
            rig.frames(),
            scanning(rig),
            rig.audio.stats.starved_blocks.load(Ordering::SeqCst),
            crossings(rig),
            rig.audio.stats.rate_micro.load(Ordering::SeqCst),
            rig.audio.stats.main_playing.load(Ordering::SeqCst),
        );
        std::thread::sleep(std::time::Duration::from_micros(300));
    }
}

fn captured(rig: &Rig) -> Vec<f32> {
    rig.out.lock().unwrap().clone()
}

fn rig(cfg: AudioThreadConfig) -> Rig {
    let rig = Rig::new(1, usize::MAX, cfg);
    rig.audio
        .try_send(AudioCommand::SetSpeedWarpMode(WarpMode::PitchShift));
    rig
}

/// Forward scan from 2 s of A through B into C, then 40 more writes.
fn forward_through_tracks(s: &Set) -> (Vec<f32>, Vec<CrossEvent>, Rig, usize) {
    let mut rig = rig(AudioThreadConfig::default());
    rig.start_at(&s.a, true, 2_000);
    rig.run_writes(5);
    neighbours(&rig, None, Some(&s.b));
    scan(&rig, ScanDir::Forward);
    let mut second = false;
    let mut writes = 0;
    for _ in 0..6_000 {
        step(&rig);
        writes += 1;
        if crossings(&rig) >= 1 && !second {
            // What the engine does after the first crossing: the neighbours of B.
            neighbours(&rig, Some(&s.a), Some(&s.c));
            second = true;
        }
        if crossings(&rig) >= 2 {
            break;
        }
    }
    assert!(crossings(&rig) >= 2, "the scan never crossed into C");
    for _ in 0..40 {
        step(&rig);
        writes += 1;
    }
    let out = captured(&rig);
    let ev = events(&rig);
    (out, ev, rig, writes)
}

/// The same scan over A+B+C as one file for `writes` writes.
fn reference(s: &Set, writes: usize) -> Vec<f32> {
    let mut rig = rig(AudioThreadConfig::default());
    rig.start_at(&s.abc, true, 2_000);
    rig.run_writes(5);
    scan(&rig, ScanDir::Forward);
    for _ in 0..writes {
        step(&rig);
    }
    captured(&rig)
}

fn worst_diff(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    a[..n]
        .iter()
        .zip(&b[..n])
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max)
}

#[test]
fn forward_scan_crosses_two_boundaries_sample_continuously() {
    let s = set();
    let (out, ev, rig, writes) = forward_through_tracks(&s);
    // The item changed twice, forward, at the end of the item left.
    assert_eq!(ev.len(), 2, "{ev:?}");
    for e in &ev {
        assert_eq!(e.dir, 1);
        assert!(
            (e.left_ms as i64 - 3_000).abs() <= 1,
            "left at {}",
            e.left_ms
        );
        assert_eq!(e.new_duration_ms, 3_000);
    }
    // The scan stays what it was: nothing bounced, still scanning.
    assert!(scanning(&rig));
    assert_eq!(rig.audio.stats.rate_bounces.load(Ordering::SeqCst), 0);
    // The rendered output equals the one-file scan: nothing but the join's own samples.
    let refd = reference(&s, writes);
    let n = out.len().min(refd.len());
    assert!(n > 50_000, "{n}");
    let worst = worst_diff(&out, &refd);
    assert!(
        worst < 1e-5,
        "crossing differs from the one-file scan by {worst}"
    );
    let bound = smooth_bound(AMP, 1013.0 * 8.0, RATE);
    let m = click_metric(&out[4096..n], bound);
    assert!(m < 1.5, "click metric {m}");
    // Window memory stays under the cap.
    let peak = rig.audio.stats.peak_window_bytes.load(Ordering::SeqCst);
    assert!(peak <= 6 * 1024 * 1024, "window peak {peak}");
}

/// Reverse scan from B's start into the end of A, on to A's start (first item: a clamp).
#[test]
fn reverse_scan_enters_the_end_of_the_previous_track_and_clamps_at_the_first_item() {
    let s = set();
    let mut rig = rig(AudioThreadConfig::default());
    rig.start_at(&s.b, true, 300);
    rig.run_writes(5);
    neighbours(&rig, Some(&s.a), None);
    scan(&rig, ScanDir::Backward);
    for _ in 0..3_000 {
        step(&rig);
        if crossings(&rig) >= 1 {
            break;
        }
    }
    assert_eq!(crossings(&rig), 1, "never crossed into A");
    // What the engine does next: A is the first item, nothing before it.
    neighbours(&rig, None, Some(&s.b));
    for _ in 0..40 {
        step(&rig);
    }
    // Now inside A near its end, still scanning backward.
    assert!(
        rig.frames() > (TRACK_FRAMES as u64) * 4 / 10,
        "position {}",
        rig.frames()
    );
    assert!(rig.frames() <= TRACK_FRAMES as u64);
    assert!(scanning(&rig));
    let ev = events(&rig);
    assert_eq!(ev.len(), 1);
    assert_eq!(
        (ev[0].dir, ev[0].left_ms, ev[0].new_duration_ms),
        (-1, 0, 3_000)
    );
    // Run on to the start of A: no neighbour, so the scan clamps (tape bounce / forward).
    for _ in 0..6_000 {
        step(&rig);
        if !scanning(&rig) {
            break;
        }
    }
    assert!(
        !scanning(&rig),
        "the scan never ended at the start of the first item"
    );
    assert_eq!(crossings(&rig), 1);
    for _ in 0..10 {
        step(&rig);
    }
    let out = captured(&rig);
    let bound = smooth_bound(AMP, 1013.0 * 8.0, RATE);
    let m = click_metric(&out[4096..], bound);
    assert!(m < 1.5, "click metric {m}");
    assert!(out.iter().all(|x| x.is_finite()));
}

/// A neighbour that is not ready when the boundary is reached: the audio holds at the edge
/// (no block rendered, no position jump) and resumes, joined exactly, once it is.
#[test]
fn a_neighbour_that_is_not_ready_holds_the_audio_at_the_edge_then_joins_exactly() {
    let s = set();
    let gate = Arc::new(AtomicBool::new(false));
    let slow = s.b.clone();
    let g = gate.clone();
    let cfg = AudioThreadConfig {
        decoder_hook: Some(DecoderHook(Arc::new(
            move |p: &Path, d: Box<dyn BlockSource>| -> Box<dyn BlockSource> {
                if p == slow.as_path() {
                    Box::new(GatedBlocks::new(d, g.clone()))
                } else {
                    d
                }
            },
        ))),
        ..Default::default()
    };
    let mut rig = rig(cfg);
    rig.start_at(&s.a, true, 2_000);
    rig.run_writes(5);
    neighbours(&rig, None, Some(&s.b));
    scan(&rig, ScanDir::Forward);
    // Run until the audio starves at the edge.
    let starved0 = rig.audio.stats.starved_blocks.load(Ordering::SeqCst);
    let mut writes = 0usize;
    loop {
        let target = rig.writes.load(Ordering::SeqCst) + 1;
        rig.allowed.store(target, Ordering::SeqCst);
        wait_until("write or starve", || {
            (rig.writes.load(Ordering::SeqCst) == target && rig.blocked.load(Ordering::SeqCst))
                || rig.audio.stats.starved_blocks.load(Ordering::SeqCst) > starved0 + 20
        });
        if rig.audio.stats.starved_blocks.load(Ordering::SeqCst) > starved0 + 20 {
            break;
        }
        writes += 1;
        assert!(writes < 3_000, "never reached the edge");
    }
    assert_eq!(crossings(&rig), 0, "crossed without the neighbour");
    // Held at the edge: still inside A, nothing past its end was rendered.
    assert!(rig.frames() <= TRACK_FRAMES as u64);
    gate.store(true, Ordering::SeqCst);
    for _ in 0..400 {
        step(&rig);
        writes += 1;
        if crossings(&rig) >= 1 {
            break;
        }
    }
    assert_eq!(
        crossings(&rig),
        1,
        "never crossed once the neighbour was ready"
    );
    for _ in 0..40 {
        step(&rig);
        writes += 1;
    }
    let out = captured(&rig);
    let refd = reference(&s, writes);
    let worst = worst_diff(&out, &refd);
    assert!(
        worst < 1e-5,
        "held crossing differs from the one-file scan by {worst}"
    );
}

/// Off (no neighbours): the end of the track is the normal end, exactly as before.
#[test]
fn without_neighbours_the_scan_ends_with_the_track() {
    let s = set();
    let mut rig = rig(AudioThreadConfig::default());
    rig.start_at(&s.a, true, 2_000);
    rig.run_writes(5);
    scan(&rig, ScanDir::Forward);
    rig.allowed.store(usize::MAX, Ordering::SeqCst);
    wait_until("end of track", || {
        rig.audio.stats.main_finished.load(Ordering::SeqCst)
    });
    assert_eq!(crossings(&rig), 0);
    assert!(events(&rig).is_empty());
}

/// An unopenable neighbour: the crossing falls back to the end of the track, no hang.
#[test]
fn an_unopenable_neighbour_falls_back_to_the_end_of_the_track() {
    let s = set();
    let missing = s.a.with_file_name("missing.wav");
    let mut rig = rig(AudioThreadConfig::default());
    rig.start_at(&s.a, true, 2_000);
    rig.run_writes(5);
    neighbours(&rig, None, Some(&missing));
    scan(&rig, ScanDir::Forward);
    rig.allowed.store(usize::MAX, Ordering::SeqCst);
    wait_until("end of track", || {
        rig.audio.stats.main_finished.load(Ordering::SeqCst)
    });
    assert_eq!(crossings(&rig), 0);
    assert!(rig.audio.stats.cross_failures.load(Ordering::SeqCst) >= 1);
}

/// MP3 as the previous track: its index is built before the end seek and the scan lands in
/// its end (the position is in the previous track's range, near its end).
#[test]
fn reverse_scan_into_an_mp3_previous_track_uses_the_index_and_lands_near_its_end() {
    let media = |rel: &str| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../assets/testMedia");
        assert!(root.is_dir(), "test media missing: run scripts/fetch-testmedia.sh");
        root.join(rel)
    };
    let prev = media("MP3 V0/Bogan Mongrel Jnr - Grooves For Machines To Study - 01 Rain.mp3");
    let cur = media(
        "MP3 V0/Bogan Mongrel - Grooves For Machines To Study - 02 Beat For Machines To Study.mp3",
    );
    let mut rig = Rig::new(2, 0, AudioThreadConfig::default());
    rig.start_at(&cur, true, 400);
    rig.run_writes(10);
    neighbours(&rig, Some(&prev), None);
    scan(&rig, ScanDir::Backward);
    for _ in 0..4_000 {
        step(&rig);
        if crossings(&rig) >= 1 {
            break;
        }
    }
    assert_eq!(crossings(&rig), 1, "never crossed into the previous MP3");
    for _ in 0..20 {
        step(&rig);
    }
    let dur_ms =
        plippa_audio::StreamDecoder::open(&prev, &Capabilities::desktop_mock(), Default::default())
            .unwrap()
            .duration_ms()
            .unwrap();
    let pos_ms = rig.frames() * 1000 / 44_100;
    assert!(
        pos_ms <= dur_ms + 50 && pos_ms + 15_000 > dur_ms,
        "position {pos_ms} of {dur_ms}"
    );
    let ev = events(&rig);
    assert_eq!((ev[0].dir, ev[0].left_ms), (-1, 0));
    let indexed = {
        let g = rig.audio.stats.worker.lock().unwrap();
        g.as_ref()
            .map_or(0, |w| w.indexed_seeks.load(Ordering::SeqCst))
    };
    assert!(
        indexed >= 1,
        "the end seek of the neighbour was not an indexed one"
    );
}
