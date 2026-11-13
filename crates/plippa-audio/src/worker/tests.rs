//! Worker tests: logic and ordering only (no wall-clock assertions). The "slow decoder" is a
//! source that only decodes when handed a permit, so pacing is by counters.

use super::*;
use std::sync::atomic::AtomicUsize;

const RATE: u32 = 1000;

/// Mono source whose sample value is its frame index; `len` frames; every `next_chunk` costs
/// one permit when gating is on.
struct Synth {
    pos: u64,
    len: u64,
    gate: Option<Arc<AtomicUsize>>,
    seeks: Arc<AtomicU64>,
    /// What the source reports as skipped malformed packets.
    skipped: u64,
}

impl Synth {
    fn new(len: u64) -> (Self, Arc<AtomicU64>) {
        let seeks = Arc::new(AtomicU64::new(0));
        (
            Self {
                pos: 0,
                len,
                gate: None,
                seeks: seeks.clone(),
                skipped: 0,
            },
            seeks,
        )
    }
    fn gated(len: u64, gate: Arc<AtomicUsize>) -> Self {
        let (mut s, _) = Self::new(len);
        s.gate = Some(gate);
        s
    }
}

impl BlockSource for Synth {
    fn channels(&self) -> usize {
        1
    }
    fn sample_rate_hz(&self) -> u32 {
        RATE
    }
    fn position_frame(&self) -> u64 {
        self.pos
    }
    fn seek_frame(&mut self, frame: u64) -> Result<(), AudioError> {
        self.seeks.fetch_add(1, Ordering::SeqCst);
        self.pos = frame.min(self.len);
        Ok(())
    }
    fn skipped_packets(&self) -> u64 {
        self.skipped
    }
    fn next_chunk(&mut self, max_frames: usize) -> Result<Option<PcmBuffer>, AudioError> {
        if let Some(g) = &self.gate {
            loop {
                let n = g.load(Ordering::SeqCst);
                if n > 0
                    && g.compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    break;
                }
                std::thread::sleep(Duration::from_micros(50));
            }
        }
        if self.pos >= self.len {
            return Ok(None);
        }
        let n = (max_frames as u64).min(self.len - self.pos);
        let samples = (self.pos..self.pos + n).map(|f| f as f32).collect();
        self.pos += n;
        Ok(Some(PcmBuffer {
            samples,
            channels: 1,
            sample_rate_hz: RATE,
        }))
    }
}

/// 1 "second" = 1000 frames: cap 12000 frames, history 2 s, lookahead 3 s, back window 1 s.
fn cfg() -> WindowConfig {
    WindowConfig {
        window_cap_bytes: 12_000 * 4,
        history_s: 2.0,
        lookahead_s: 3.0,
        min_history_s: 0.0,
        back_window_s: 1.0,
        back_lead_s: 2.0,
        back_reference_frames: 100,
        landing_history_s: 0.5,
        waypoint_lookahead_s: 1.0,
        ..WindowConfig::default()
    }
}

const CHUNK: usize = 250;

fn params(start: u64) -> FeedParams {
    FeedParams {
        chunk_frames: CHUNK,
        start_chunk_frames: CHUNK,
        start_prefetch_frames: CHUNK,
        start_frame: start,
    }
}

fn feed_over(source: Synth, start: u64) -> WindowFeed {
    WindowFeed::spawn(Box::new(source), 0, &cfg(), &params(start), None)
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(20), "timed out: {what}");
        std::thread::sleep(Duration::from_micros(100));
    }
}

/// Pump until `f(feed)` holds.
fn pump_until(
    feed: &mut WindowFeed,
    playhead: u64,
    dir: i8,
    what: &str,
    f: impl Fn(&WindowFeed) -> bool,
) {
    let t = Instant::now();
    loop {
        feed.pump(playhead, dir);
        if f(feed) {
            return;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "timed out: {what}");
        std::thread::sleep(Duration::from_micros(100));
    }
}

fn assert_window_is_frame_index(feed: &WindowFeed) {
    let w = &feed.window;
    let mut out = vec![0.0f32; w.len_frames()];
    assert_eq!(w.read_forward(w.start_frame(), &mut out), w.len_frames());
    for (i, v) in out.iter().enumerate() {
        assert_eq!(
            *v,
            (w.start_frame() + i as u64) as f32,
            "frame {}",
            w.start_frame() + i as u64
        );
    }
}

#[test]
fn estimator_table() {
    let rate = 48_000;
    let mut e = ThroughputEstimator::new(0.5);
    assert_eq!(e.speed(), None);
    assert_eq!(e.max_sustainable_rate(0.5), None);
    e.sample(0, Duration::from_millis(10), rate); // ignored
    e.sample(48_000, Duration::ZERO, rate); // ignored
    assert_eq!(e.speed(), None);
    // 48000 frames in 0.5 s = 2x real time.
    e.sample(48_000, Duration::from_millis(500), rate);
    assert!((e.speed().unwrap() - 2.0).abs() < 1e-9);
    assert!((e.max_sustainable_rate(0.5).unwrap() - 1.0).abs() < 1e-9);
    // 48000 in 0.125 s = 8x; alpha 0.5 -> 5x.
    e.sample(48_000, Duration::from_millis(125), rate);
    assert!((e.speed().unwrap() - 5.0).abs() < 1e-9);
    assert!((e.max_sustainable_rate(0.5).unwrap() - 2.5).abs() < 1e-9);
    // A very slow stick pulls it down: 4800 in 1 s = 0.1x -> 2.55.
    e.sample(4_800, Duration::from_secs(1), rate);
    assert!((e.speed().unwrap() - 2.55).abs() < 1e-9);
}

fn pcfg() -> PlanCfg {
    PlanCfg::new(&cfg(), 1, RATE, CHUNK, CHUNK)
}

fn pv(start: u64, end: u64, playhead: u64, dir: i8, started: bool) -> PlanView {
    PlanView {
        start,
        end,
        playhead,
        dir,
        audio_started: started,
        eof_at: None,
        hint: None,
        waypoint_done: false,
    }
}

#[test]
fn plan_table() {
    let c = pcfg();
    assert_eq!(c.lookahead_frames, 3000);
    assert_eq!(c.back_window_frames, 1000);
    assert_eq!(c.back_lead_frames, 2000);
    // A fresh (empty) window always takes its landing chunk first, even in reverse.
    assert_eq!(plan(&pv(900, 900, 900, -1, true), &c), Fetch::Forward);
    // Start gate: one start chunk, then idle until audio started.
    assert_eq!(plan(&pv(0, 0, 0, 1, false), &c), Fetch::Forward);
    assert_eq!(plan(&pv(0, 250, 0, 1, false), &c), Fetch::Idle);
    // Forward fill toward the lookahead, stop when full.
    assert_eq!(plan(&pv(0, 250, 0, 1, true), &c), Fetch::Forward);
    assert_eq!(plan(&pv(0, 2750, 0, 1, true), &c), Fetch::Forward);
    assert_eq!(plan(&pv(0, 2751, 0, 1, true), &c), Fetch::Idle);
    // At the end of the stream: nothing to fetch.
    let mut v = pv(0, 500, 0, 1, true);
    v.eof_at = Some(500);
    assert_eq!(plan(&v, &c), Fetch::Idle);
    // Reverse keeps back_lead behind the playhead: a back window before the window start.
    assert_eq!(
        plan(&pv(5000, 8000, 6000, -1, true), &c),
        Fetch::Back { from: 4000 }
    );
    assert_eq!(plan(&pv(5000, 8000, 7500, -1, true), &c), Fetch::Idle);
    // Clamped at frame 0; nothing before it.
    assert_eq!(
        plan(&pv(300, 3000, 400, -1, true), &c),
        Fetch::Back { from: 0 }
    );
    assert_eq!(plan(&pv(0, 3000, 400, -1, true), &c), Fetch::Idle);
}

#[test]
fn plan_hint_table() {
    let c = pcfg();
    let hint = |t| {
        Some(Hint {
            target_frame: t,
            direction: -1,
            waypoint_lead_frames: 0,
        })
    };
    // Target behind the window but near: back windows toward target - landing history (500).
    let mut v = pv(5000, 8000, 6000, 1, true);
    v.hint = hint(2000);
    assert_eq!(plan(&v, &c), Fetch::Back { from: 4000 });
    v.start = 2400;
    assert_eq!(plan(&v, &c), Fetch::Back { from: 1500 });
    v.start = 1500;
    assert!(v.landing_ready(&c) || v.end < 2250);
    // Playback comes first: with the lookahead nearly gone the forward fill pre-empts a hint.
    let mut v = pv(5000, 6500, 6400, 1, true);
    v.hint = hint(2000);
    assert_eq!(plan(&v, &c), Fetch::Forward);
    // Target ahead within reach: forward until target + chunk is held.
    let mut v = pv(0, 3000, 1000, 1, true);
    v.hint = hint(5000);
    v.hint = Some(Hint {
        target_frame: 4000,
        direction: 1,
        waypoint_lead_frames: 0,
    });
    assert_eq!(plan(&v, &c), Fetch::Forward);
    v.end = 4250;
    assert!(v.landing_ready(&c));
    // Far target (more than the cap away): waypoint, once.
    let mut v = pv(0, 3000, 1000, 1, true);
    v.hint = Some(Hint {
        target_frame: 50_000,
        direction: 1,
        waypoint_lead_frames: 0,
    });
    assert_eq!(
        plan(&v, &c),
        Fetch::Waypoint {
            lo: 49_500,
            target: 50_000
        }
    );
    v.waypoint_done = true;
    assert_ne!(
        plan(&v, &c),
        Fetch::Waypoint {
            lo: 49_500,
            target: 50_000
        }
    );
}

#[test]
fn forward_fill_is_contiguous_and_gated_until_audio_starts() {
    let (src, seeks) = Synth::new(50_000);
    let mut feed = feed_over(src, 0);
    pump_until(&mut feed, 0, 1, "start chunk", |f| {
        f.window.len_frames() >= CHUNK
    });
    // Not started: the worker stays at the start prefetch.
    for _ in 0..200 {
        feed.pump(0, 1);
        std::thread::sleep(Duration::from_micros(100));
    }
    assert_eq!(feed.window.len_frames(), CHUNK, "gated at one start chunk");
    feed.mark_audio_started(0, 1);
    pump_until(&mut feed, 0, 1, "lookahead", |f| {
        f.window.len_frames() >= 2750
    });
    assert_window_is_frame_index(&feed);
    assert_eq!(
        seeks.load(Ordering::SeqCst),
        0,
        "sequential decode needs no seek"
    );
    // Playhead advances: the window slides, history capped, peak under the cap.
    let mut ph = 0u64;
    for _ in 0..40 {
        ph += 500;
        pump_until(&mut feed, ph, 1, "slide", |f| {
            f.window.end_frame() >= (ph + 2750).min(50_000)
        });
    }
    assert_window_is_frame_index(&feed);
    assert!(feed.window.peak_bytes() <= feed.window.cap_bytes());
    assert!(feed.window.start_frame() >= ph - 2000, "history bounded");
}

#[test]
fn eof_is_reported_and_forward_fill_stops() {
    let (src, _) = Synth::new(1_300);
    let mut feed = feed_over(src, 0);
    feed.mark_audio_started(0, 1);
    pump_until(&mut feed, 0, 1, "eof", |f| f.eof());
    assert_eq!(feed.eof_at(), Some(1300));
    assert_eq!(feed.window.end_frame(), 1300);
    assert_window_is_frame_index(&feed);
}

#[test]
fn the_sources_skipped_packet_count_reaches_the_shared_stats() {
    let (mut src, _) = Synth::new(1_300);
    src.skipped = 3;
    let mut feed = feed_over(src, 0);
    feed.mark_audio_started(0, 1);
    pump_until(&mut feed, 0, 1, "eof", |f| f.eof());
    assert_eq!(feed.stats().skipped_packets.load(Ordering::Relaxed), 3);
}

#[test]
fn reverse_walks_back_across_many_back_windows_in_order() {
    let (src, seeks) = Synth::new(60_000);
    let mut feed = feed_over(src, 40_000);
    feed.mark_audio_started(40_000, 1);
    pump_until(&mut feed, 40_000, 1, "initial", |f| {
        f.window.len_frames() >= 2750
    });
    // The "audio thread": read backward one frame at a time, only while data is held.
    let mut ph = 40_000u64;
    let mut visited = 0u64;
    let mut stalls = 0u64;
    while ph > 3_000 {
        feed.pump(ph, -1);
        let w = &feed.window;
        if w.contains(ph) && (ph > w.start_frame() || w.start_frame() == 0) {
            let mut one = [0.0f32];
            assert_eq!(w.read_backward(ph, &mut one), 1);
            assert_eq!(one[0], ph as f32, "frame {ph}");
            assert!(w.bytes() <= w.cap_bytes());
            ph -= 1;
            visited += 1;
        } else {
            stalls += 1;
            std::thread::sleep(Duration::from_micros(50));
        }
    }
    assert!(visited >= 37_000);
    assert!(
        feed.stats().back_windows.load(Ordering::SeqCst) >= 30,
        "many back windows"
    );
    assert!(seeks.load(Ordering::SeqCst) >= 30);
    assert_eq!(feed.stats().blocks_refused.load(Ordering::SeqCst), 0);
    assert_eq!(feed.stats().audio_blocking_waits.load(Ordering::SeqCst), 0);
    let _ = stalls;
}

#[test]
fn reverse_clamps_at_frame_zero() {
    let (src, _) = Synth::new(10_000);
    let mut feed = feed_over(src, 1_200);
    feed.mark_audio_started(1_200, -1);
    pump_until(&mut feed, 1_200, -1, "reach zero", |f| {
        f.window.start_frame() == 0
    });
    assert_window_is_frame_index(&feed);
    // Nothing more to fetch before frame 0.
    for _ in 0..100 {
        feed.pump(600, -1);
        std::thread::sleep(Duration::from_micros(50));
    }
    assert_eq!(feed.window.start_frame(), 0);
    assert_eq!(feed.stats().blocks_refused.load(Ordering::SeqCst), 0);
}

#[test]
fn reposition_discards_old_epoch_blocks_and_lands_on_the_new_frame() {
    let (src, seeks) = Synth::new(40_000);
    let mut feed = feed_over(src, 0);
    feed.mark_audio_started(0, 1);
    pump_until(&mut feed, 0, 1, "fill", |f| f.window.len_frames() >= 2750);
    for target in [30_000u64, 5_000, 35_000, 100] {
        feed.reposition(target, target, 1);
        assert_eq!(feed.window.len_frames(), 0);
        assert_eq!(feed.window.start_frame(), target);
        pump_until(&mut feed, target, 1, "landing", |f| {
            f.window.len_frames() >= CHUNK
        });
        assert_eq!(feed.window.start_frame(), target, "no old data leaks in");
        assert_window_is_frame_index(&feed);
    }
    assert!(seeks.load(Ordering::SeqCst) >= 4);
}

#[test]
fn slow_decoder_never_blocks_the_audio_side_and_data_stays_ordered() {
    let gate = Arc::new(AtomicUsize::new(0));
    let mut feed = WindowFeed::spawn(
        Box::new(Synth::gated(40_000, gate.clone())),
        0,
        &cfg(),
        &params(10_000),
        None,
    );
    feed.mark_audio_started(10_000, 1);
    // Starved: many pumps return at once with nothing applied.
    for _ in 0..1000 {
        assert_eq!(feed.pump(10_000, 1).applied, 0);
    }
    assert_eq!(feed.window.len_frames(), 0);
    // Release the decoder one chunk at a time: the window grows one chunk per permit and
    // stays frame-exact.
    for n in 1..=8usize {
        gate.fetch_add(1, Ordering::SeqCst);
        pump_until(&mut feed, 10_000, 1, "permit consumed", |f| {
            f.window.len_frames() >= n * CHUNK
        });
        assert_window_is_frame_index(&feed);
    }
    assert_eq!(feed.stats().audio_blocking_waits.load(Ordering::SeqCst), 0);
    // Let it run free.
    gate.store(usize::MAX / 2, Ordering::SeqCst);
}

#[test]
fn slow_back_window_holds_until_the_decoder_catches_up() {
    let gate = Arc::new(AtomicUsize::new(usize::MAX / 2));
    let mut feed = WindowFeed::spawn(
        Box::new(Synth::gated(40_000, gate.clone())),
        0,
        &cfg(),
        &params(20_000),
        None,
    );
    feed.mark_audio_started(20_000, 1);
    pump_until(&mut feed, 20_000, 1, "fill", |f| {
        f.window.len_frames() >= 2750
    });
    // Stop the decoder, then reverse past the held history: the audio side runs dry and
    // simply sees no data, and nothing is ever refused or reordered.
    gate.store(0, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(20));
    let mut ph = 20_000u64;
    let mut dry = false;
    for _ in 0..30_000 {
        feed.pump(ph, -1);
        let w = &feed.window;
        if ph > w.start_frame() && w.contains(ph) {
            ph -= 1;
        } else {
            dry = true;
            break;
        }
    }
    assert!(dry, "ran out of history while the decoder was stopped");
    let start_when_dry = feed.window.start_frame();
    for _ in 0..200 {
        feed.pump(ph, -1);
    }
    assert_eq!(
        feed.window.start_frame(),
        start_when_dry,
        "no data without permits"
    );
    // Let one back window through.
    gate.store(usize::MAX / 2, Ordering::SeqCst);
    pump_until(&mut feed, ph, -1, "back window arrives", |f| {
        f.window.start_frame() < start_when_dry
    });
    assert_window_is_frame_index(&feed);
    assert_eq!(feed.stats().blocks_refused.load(Ordering::SeqCst), 0);
    assert_eq!(feed.stats().audio_blocking_waits.load(Ordering::SeqCst), 0);
}

#[test]
fn hint_near_target_behind_extends_the_window_with_landing_history() {
    let (src, _) = Synth::new(60_000);
    let mut feed = feed_over(src, 30_000);
    feed.mark_audio_started(30_000, 1);
    pump_until(&mut feed, 30_000, 1, "fill", |f| {
        f.window.len_frames() >= 2750
    });
    // 3 s behind: outside the 2 s history, inside what the 12 s cap can span.
    let target = 27_000u64;
    feed.hint(Some(Hint {
        target_frame: target,
        direction: -1,
        waypoint_lead_frames: 0,
    }));
    pump_until(&mut feed, 30_000, 1, "landing zone", |f| {
        f.window.start_frame() <= target - 500
            && f.stats().hint_ready_target.load(Ordering::SeqCst) == target as i64
    });
    assert!(feed.window.contains(target - 500) && feed.window.contains(target));
    assert_window_is_frame_index(&feed);
    assert!(feed.window.contains(30_000), "the playhead is still held");
}

#[test]
fn hint_far_target_delivers_a_waypoint_window_with_landing_history() {
    let (src, _) = Synth::new(200_000);
    let mut feed = feed_over(src, 1_000);
    feed.mark_audio_started(1_000, 1);
    pump_until(&mut feed, 1_000, 1, "fill", |f| {
        f.window.len_frames() >= 2750
    });
    let target = 150_000u64;
    feed.hint(Some(Hint {
        target_frame: target,
        direction: 1,
        waypoint_lead_frames: 0,
    }));
    pump_until(&mut feed, 1_000, 1, "waypoint", |f| f.has_waypoint());
    let wp = feed.take_waypoint().unwrap();
    assert_eq!(wp.target_frame, target);
    let w = &wp.window;
    assert!(w.start_frame() <= target - 500, "landing history held");
    assert!(w.contains(target) && w.end_frame() >= target + 1000);
    assert!(w.cap_bytes() <= cfg().window_cap_bytes / 2, "3 + 3 split");
    let mut out = vec![0.0; 1000];
    assert_eq!(w.read_forward(target - 500, &mut out), 1000);
    assert!(out
        .iter()
        .enumerate()
        .all(|(i, v)| *v == (target - 500 + i as u64) as f32));
    // The main window kept serving the playhead meanwhile.
    pump_until(&mut feed, 1_000, 1, "main continues", |f| {
        f.window.contains(1_000)
    });
    assert_window_is_frame_index(&feed);
    assert_eq!(feed.stats().waypoints_sent.load(Ordering::SeqCst), 1);
}

#[test]
fn forced_waypoint_window_spans_the_lead_and_is_adopted_within_the_cap() {
    let (src, _) = Synth::new(200_000);
    let mut feed = feed_over(src, 1_000);
    feed.mark_audio_started(1_000, 1);
    pump_until(&mut feed, 1_000, 1, "fill", |f| f.window.len_frames() >= 2750);
    let (target, lead) = (150_000u64, 3_000u64);
    assert!(feed.waypoint_fits(lead));
    assert!(!feed.waypoint_fits(10_000), "a lead the split window cannot hold");
    feed.hint(Some(Hint {
        target_frame: target,
        direction: 1,
        waypoint_lead_frames: lead,
    }));
    pump_until(&mut feed, 1_000, 1, "waypoint", |f| f.has_waypoint());
    let wp = feed.take_waypoint().unwrap();
    let w = &wp.window;
    // Forward: [T - lead - landing history, T + lookahead): the waypoint T - lead is inside
    // with the landing history before it.
    assert!(w.start_frame() <= target - lead - 500);
    assert!(w.contains_range(target - lead, 150) && w.end_frame() >= target + 1000);
    assert!(w.cap_bytes() <= cfg().window_cap_bytes / 2);
    // Adopt at the waypoint: same ring (cap unchanged), frames equal the frame index, the
    // old contents are gone, and the worker continues contiguously after the adopted frames.
    let cap_before = feed.window.cap_bytes();
    let end = w.end_frame();
    assert!(feed.adopt_waypoint(&wp, target - lead, 1));
    drop(wp);
    assert_eq!(feed.window.cap_bytes(), cap_before);
    assert!(!feed.window.contains(1_000), "old window replaced");
    assert_window_is_frame_index(&feed);
    assert!(feed.window.bytes() <= cfg().window_cap_bytes);
    // The playhead moves on (nearly through the adopted lookahead): the worker appends.
    pump_until(&mut feed, end - 1_000, 1, "continues", |f| {
        f.window.end_frame() > end + 500
    });
    assert_window_is_frame_index(&feed);
    assert!(feed.window.bytes() <= cfg().window_cap_bytes);
    assert_eq!(feed.stats().blocks_refused.load(Ordering::SeqCst), 0);
}

#[test]
fn forced_waypoint_backward_covers_the_lead_above_the_target() {
    let (src, _) = Synth::new(200_000);
    let mut feed = feed_over(src, 150_000);
    feed.mark_audio_started(150_000, -1);
    pump_until(&mut feed, 150_000, -1, "fill", |f| f.window.len_frames() >= 500);
    let (target, lead) = (20_000u64, 3_000u64);
    feed.hint(Some(Hint {
        target_frame: target,
        direction: -1,
        waypoint_lead_frames: lead,
    }));
    pump_until(&mut feed, 150_000, -1, "waypoint", |f| f.has_waypoint());
    let wp = feed.take_waypoint().unwrap();
    // Backward: [T - landing history, T + lead + lookahead).
    assert!(wp.window.start_frame() <= target - 500);
    assert!(wp.window.contains_range(target + lead - 150, 151));
}

#[test]
fn a_decoder_error_is_reported_not_hidden() {
    struct Bad;
    impl BlockSource for Bad {
        fn channels(&self) -> usize {
            1
        }
        fn sample_rate_hz(&self) -> u32 {
            RATE
        }
        fn position_frame(&self) -> u64 {
            0
        }
        fn seek_frame(&mut self, _: u64) -> Result<(), AudioError> {
            Ok(())
        }
        fn next_chunk(&mut self, _: usize) -> Result<Option<PcmBuffer>, AudioError> {
            Err(AudioError::InvalidParam("boom".into()))
        }
    }
    let mut feed = WindowFeed::spawn(Box::new(Bad), 0, &cfg(), &params(0), None);
    wait("error", || {
        feed.pump(0, 1);
        feed.error.is_some()
    });
    assert!(feed.take_error().unwrap().contains("boom"));
}
