//! SEAM_01 on the control plane: what the audio thread is offered as the next item, and what the
//! engine's state does when the audio goes on into it by itself. The audio is real (the engine's
//! own audio thread over a gated, capturing sink); nothing here asserts on wall-clock time.

use super::*;
use plippa_audio::AudioSink;
use plippa_ipc_proto::{IpcMessage, NOTICE_MEDIA_FILE_MISSING};
use plippa_noubin::{encode_playable_id, normalise_noubin_key, LibraryReader, LibraryRoot, LibraryWriter};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize};

const RATE: u32 = 8_000;
/// 6 s per track: well over the 2 s head a join appends at this rate.
const TRACK_FRAMES: usize = 6 * RATE as usize;

/// `frames` frames of a 440 Hz sine starting at global frame `from`: consecutive tracks join
/// into one continuous tone, so a gap or an overlap shows in the samples.
fn tone_wav(path: &Path, rate: u32, from: usize, frames: usize) {
    let data_bytes = frames * 2;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut out = std::fs::File::create(path).unwrap();
    out.write_all(b"RIFF").unwrap();
    out.write_all(&(36 + data_bytes as u32).to_le_bytes()).unwrap();
    out.write_all(b"WAVEfmt ").unwrap();
    out.write_all(&16u32.to_le_bytes()).unwrap();
    out.write_all(&1u16.to_le_bytes()).unwrap();
    out.write_all(&1u16.to_le_bytes()).unwrap();
    out.write_all(&rate.to_le_bytes()).unwrap();
    out.write_all(&(rate * 2).to_le_bytes()).unwrap();
    out.write_all(&2u16.to_le_bytes()).unwrap();
    out.write_all(&16u16.to_le_bytes()).unwrap();
    out.write_all(b"data").unwrap();
    out.write_all(&(data_bytes as u32).to_le_bytes()).unwrap();
    for i in 0..frames {
        let t = (from + i) as f64 / f64::from(RATE);
        let s = ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16;
        out.write_all(&s.to_le_bytes()).unwrap();
    }
}

struct Lib {
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// What the tracks decode to, back to back.
    expected: Vec<f32>,
    /// Library-relative paths of the tracks.
    files: Vec<PathBuf>,
}

/// An album "Album" of tracks T1..Tn with the given `(frames, sample rate)`, phase-continuous.
/// `cues` is raw `cueLists` JSON for track 2 (empty for none); `profile` is the playable's.
fn lib(tracks: &[(usize, u32)], profile: &str, cues_on_second: &str) -> Lib {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (mut items, mut files, mut expected, mut from) = (Vec::new(), Vec::new(), Vec::new(), 0usize);
    for (i, (frames, rate)) in tracks.iter().enumerate() {
        let n = i + 1;
        let p = root.join(format!("Music/Album/{n:02}.wav"));
        tone_wav(&p, *rate, from, *frames);
        expected.extend(plippa_audio::decode(&p).unwrap().samples);
        files.push(p);
        from += frames;
        let cues = if n == 2 && !cues_on_second.is_empty() {
            format!(r#","cueLists":{cues_on_second}"#)
        } else {
            String::new()
        };
        items.push(format!(
            r#"{{"mediaTitle":"T{n}","localFilePaths":["{n:02}.wav"]{cues}}}"#
        ));
    }
    let p = root.join("Music/Album/_album.noudata");
    std::fs::write(
        p,
        format!(
            r#"{{"noubinKey":"album.noubin.s","noubinKeyNormalised":"{}","playableItemTitle":"Album","playableItemProfile":"{profile}",
               "releaseData":{{"credits":{{"primaryArtists":[{{"primaryArtistName":"The Band"}}]}}}},
               "mediaList":[{}]}}"#,
            normalise_noubin_key("album.noubin.s"),
            items.join(",")
        ),
    )
    .unwrap();
    LibraryWriter::open(&LibraryRoot::new(&root)).unwrap().full_scan().unwrap();
    Lib { _dir: dir, root, expected, files }
}

fn gid(root: &Path) -> plippa_core::PlayableItemId {
    let reader = LibraryReader::open(&LibraryRoot::new(root)).unwrap();
    let rec = reader
        .list_playables()
        .unwrap()
        .into_iter()
        .find(|p| p.title.as_deref() == Some("Album"))
        .unwrap();
    plippa_core::PlayableItemId::new(encode_playable_id(root, rec.id))
}

/// Capturing sink gated by the test: `allowed` writes may pass, the next one parks until it is
/// raised. `pace_ms` slows a free-running sink so the neighbour's helper thread keeps up the way
/// it does against a real sink.
struct Shared {
    out: Mutex<Vec<f32>>,
    writes: AtomicUsize,
    allowed: AtomicUsize,
    blocked: AtomicBool,
    pace_ms: u64,
}

/// The test's handle on the sink.
struct Gate(Arc<Shared>);

impl std::ops::Deref for Gate {
    type Target = Shared;
    fn deref(&self) -> &Shared {
        &self.0
    }
}

/// An engine over a [`Gate`]. Its fields drop in order: the gate is opened first, so the
/// engine's drop (which joins the audio thread) never waits on a parked write, however the
/// test ended.
struct Played {
    _open: OpenOnDrop,
    engine: Engine,
}

struct OpenOnDrop(Arc<Shared>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.allowed.store(usize::MAX, Ordering::SeqCst);
    }
}

impl std::ops::Deref for Played {
    type Target = Engine;
    fn deref(&self) -> &Engine {
        &self.engine
    }
}

impl std::ops::DerefMut for Played {
    fn deref_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }
}

struct GateSink {
    channels: u16,
    rate: u32,
    gate: Arc<Shared>,
}

impl AudioSink for GateSink {
    fn channels(&self) -> u16 {
        self.channels
    }
    fn sample_rate_hz(&self) -> u32 {
        self.rate
    }
    fn write(&mut self, interleaved: &[f32]) -> Result<(), AudioError> {
        let g = &self.gate;
        let n = g.writes.load(Ordering::SeqCst);
        if n >= g.allowed.load(Ordering::SeqCst) {
            g.blocked.store(true, Ordering::SeqCst);
            let parked = Instant::now();
            while n >= g.allowed.load(Ordering::SeqCst) {
                // (a test that is stuck must fail, not hang)
                if parked.elapsed() > Duration::from_secs(30) {
                    g.allowed.store(usize::MAX, Ordering::SeqCst);
                }
                thread::sleep(Duration::from_micros(200));
            }
            g.blocked.store(false, Ordering::SeqCst);
        }
        g.out.lock().unwrap().extend_from_slice(interleaved);
        g.writes.fetch_add(1, Ordering::SeqCst);
        if g.pace_ms > 0 {
            thread::sleep(Duration::from_millis(g.pace_ms));
        }
        Ok(())
    }
}

impl Gate {
    fn new(pace_ms: u64, allowed: usize) -> Self {
        Self(Arc::new(Shared {
            out: Mutex::default(),
            writes: AtomicUsize::new(0),
            allowed: AtomicUsize::new(allowed),
            blocked: AtomicBool::new(false),
            pace_ms,
        }))
    }

    fn engine(&self, root: &Path) -> Played {
        let g = self.0.clone();
        let engine = Engine::open_with_sink_factory(
            root,
            Capabilities::desktop_mock(),
            move |channels: u16, rate: u32| -> Result<Box<dyn AudioSink>, AudioError> {
                Ok(Box::new(GateSink { channels, rate, gate: g.clone() }))
            },
        )
        .unwrap();
        Played { _open: OpenOnDrop(self.0.clone()), engine }
    }

    fn captured(&self) -> Vec<f32> {
        self.out.lock().unwrap().clone()
    }

    /// Let exactly one more write through and wait until the audio thread is parked again (or
    /// the stream ended); the engine is polled meanwhile.
    fn step(&self, e: &mut Engine) {
        let target = self.writes.load(Ordering::SeqCst) + 1;
        self.allowed.store(target, Ordering::SeqCst);
        let t = Instant::now();
        loop {
            e.poll_stream_lifecycle();
            if self.writes.load(Ordering::SeqCst) == target && self.blocked.load(Ordering::SeqCst) {
                break;
            }
            if e.session().transport != TransportState::Playing {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(10), "the audio thread never parked");
            thread::sleep(Duration::from_micros(200));
        }
        thread::sleep(Duration::from_micros(300));
    }

    /// Step until `pred`, polling the engine; bounded.
    fn step_until(&self, e: &mut Engine, what: &str, mut pred: impl FnMut(&mut Engine) -> bool) {
        for _ in 0..4_000 {
            e.poll_stream_lifecycle();
            if pred(e) {
                return;
            }
            self.step(e);
        }
        panic!("never reached: {what}");
    }
}

fn cmd(e: &mut Engine, c: PlaybackCommand) {
    e.handle_ipc(IpcMessage::PlaybackCommand(c));
}

fn entry(e: &mut Engine) -> u32 {
    e.poll_stream_lifecycle();
    e.sync_session_playback_fields();
    e.session().current_entry_index
}

fn wait_until(e: &mut Engine, what: &str, mut f: impl FnMut(&mut Engine) -> bool) {
    let t = Instant::now();
    loop {
        e.poll_stream_lifecycle();
        if f(e) {
            return;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "timed out: {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// What the engine's output stage makes of `x` at the session volume.
fn at_volume(e: &Engine, x: &[f32]) -> Vec<f32> {
    let g = e.audio_volume_linear();
    x.iter().map(|v| v * g).collect()
}

fn worst_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len(), "lengths");
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

fn notices(events: &[EngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|ev| match ev {
            EngineEvent::Notice(n) => Some(n.code.clone()),
            _ => None,
        })
        .collect()
}

fn current_indices(events: &[EngineEvent]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|ev| match ev {
            EngineEvent::NowPlaying(l) => Some(l.current_index),
            _ => None,
        })
        .collect()
}

#[test]
fn what_is_offered_as_the_next_item_follows_the_end_of_item_rules() {
    let l = lib(&[(2_000, RATE); 3], "music", "");
    let mut e = Engine::open_mock(&l.root, Capabilities::desktop_mock()).unwrap();
    cmd(&mut e, PlaybackCommand::Load(gid(&l.root)));
    let next = |e: &mut Engine| e.seamless_next().map(|n| (n.path, n.item_position, n.seamless));
    let item = |e: &Engine, i: usize| e.loaded.as_ref().unwrap().list.track(i).unwrap().item_position;

    // The next track, as a seamless candidate with the identity the crossing event will carry.
    assert_eq!(next(&mut e), Some((l.files[1].clone(), item(&e, 1), true)));
    // Repeat-item and pause-after-each: the end of the item is not a join.
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Item));
    assert_eq!(next(&mut e), None, "repeat item");
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Off));
    cmd(&mut e, PlaybackCommand::SetPauseAfterEach(true));
    assert_eq!(next(&mut e), None, "pause after each");
    cmd(&mut e, PlaybackCommand::SetPauseAfterEach(false));
    assert!(next(&mut e).is_some());

    // The end of the list: nothing follows, and a repeat-queue wrap joins the first track.
    cmd(&mut e, PlaybackCommand::Next);
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(entry(&mut e), 2);
    assert_eq!(next(&mut e), None, "end of the list");
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Queue));
    assert_eq!(next(&mut e), Some((l.files[0].clone(), item(&e, 0), true)), "wrap");

    // A wrap that starts a fresh shuffled pass cannot be looked at without reshuffling: no
    // join, the ordinary advance handles that boundary.
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(&mut e, PlaybackCommand::SetReshuffleOnRepeat(true));
    while e.loaded.as_ref().unwrap().list.current_index() != 2 && entry(&mut e) < 3 {
        cmd(&mut e, PlaybackCommand::Next);
    }
    let last = e.loaded.as_ref().unwrap().list.current_index();
    let first_in_order = e.loaded.as_ref().unwrap().list.next_track_action();
    if let NavAction::Track { index, .. } = first_in_order {
        assert!(index <= last, "the wrap goes back in the list");
    }
    assert_eq!(next(&mut e), None, "wrap with reshuffle");
    cmd(&mut e, PlaybackCommand::SetReshuffleOnRepeat(false));
    assert!(next(&mut e).is_some(), "wrap without reshuffle");
    // Looking never changes the list.
    let order: Vec<_> = e.now_playing_entries().into_iter().map(|x| x.title).collect();
    let _ = next(&mut e);
    let again: Vec<_> = e.now_playing_entries().into_iter().map(|x| x.title).collect();
    assert_eq!(order, again);
}

#[test]
fn a_cue_permutation_segment_still_to_play_is_a_seek_never_a_join() {
    // A cue-shuffled mix of two files: only after the last segment of the permutation does the
    // end of the item lead to the second file.
    let cues = r#"[{"cueListType":"nav","cues":[
        {"startTime":"0","cueTextSpans":[{"text":"a"}]},
        {"startTime":"2","cueTextSpans":[{"text":"b"}]},
        {"startTime":"4","cueTextSpans":[{"text":"c"}]}]}]"#;
    let l = lib(&[(TRACK_FRAMES, RATE), (TRACK_FRAMES, RATE)], "mix", cues);
    let mut e = Engine::open_mock(&l.root, Capabilities::desktop_mock()).unwrap();
    cmd(&mut e, PlaybackCommand::Load(gid(&l.root)));
    cmd(&mut e, PlaybackCommand::Next); // the first file has no cues; the second is the mix
    assert_eq!(entry(&mut e), 1);
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Queue));
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(&mut e, PlaybackCommand::SetReshuffleOnRepeat(false));
    assert!(e.cue_shuffle_active(), "the fixture must be a cue-shuffled mix");
    // Walk the permutation: every segment but the last has another one after it.
    let mut joins = 0;
    for start_ms in [0u64, 2_000, 4_000] {
        cmd(&mut e, PlaybackCommand::SeekMs(start_ms));
        wait_until(&mut e, "seek landed", |e| e.current_position_ms() == Some(start_ms));
        if e.seamless_next().is_some() {
            joins += 1;
        }
    }
    assert_eq!(joins, 1, "only the last segment of the permutation ends in a join");
}

#[test]
fn an_album_plays_through_on_one_stream_and_the_audio_is_continuous() {
    let l = lib(&[(TRACK_FRAMES, RATE); 3], "music", "");
    // A tenth of real time: the neighbour's helper thread keeps up as it does on the device.
    let gate = Gate::new(10, usize::MAX);
    let mut e = gate.engine(&l.root);
    cmd(&mut e, PlaybackCommand::LoadCollection { playable_id: gid(&l.root) });
    let mut seen = vec![0u32];
    wait_until(&mut e, "album plays through", |e| {
        let i = entry(e);
        if seen.last() != Some(&i) {
            seen.push(i);
        }
        e.session().transport == TransportState::Stopped
    });
    assert_eq!(seen, [0, 1, 2], "items in order, none repeated");
    // One stream open for the whole album: the audio went on by itself at both boundaries.
    assert_eq!(e.stream_prepare_count(), 1, "a boundary restarted the stream");
    // And the samples are exactly the three files back to back: no gap, no overlap.
    let out = gate.captured();
    assert_eq!(out.len(), l.expected.len(), "frames out");
    assert!(
        worst_diff(&out, &at_volume(&e, &l.expected)) < 1e-6,
        "the output differs from the files back to back"
    );
    // The list followed: now playing published each item as it was entered, with no notice.
    let events = e.take_events();
    assert!(current_indices(&events).windows(2).all(|w| w[1] >= w[0]));
    assert!(notices(&events).is_empty(), "{:?}", notices(&events));
}

#[test]
fn a_seamless_crossing_leaves_the_engine_as_go_to_track_does() {
    let cues = r#"[{"cueListType":"nav","cues":[
        {"startTime":"0","cueTextSpans":[{"text":"Intro"}]},
        {"startTime":"2","cueTextSpans":[{"text":"Verse"}]}]}]"#;
    let l = lib(&[(TRACK_FRAMES, RATE); 3], "music", cues);

    // Reference: the same album, moved to the second track by `Next` (go_to_track).
    let mut r = Engine::open_mock(&l.root, Capabilities::desktop_mock()).unwrap();
    cmd(&mut r, PlaybackCommand::Load(gid(&l.root)));
    cmd(&mut r, PlaybackCommand::Next);
    r.take_events();
    r.publish_now_playing();
    let want_list = r.take_events();
    let want_entries = r.now_playing_entries();
    r.sync_session_playback_fields();
    let want = (r.session().current_entry_index, r.session().current_cue_index, r.session().position_ms);

    // Under test: gated, so the crossing is looked at where it happens.
    let gate = Gate::new(0, 0);
    let mut e = gate.engine(&l.root);
    cmd(&mut e, PlaybackCommand::LoadCollection { playable_id: gid(&l.root) });
    e.take_events();
    gate.step_until(&mut e, "the second track entered", |e| entry(e) == 1);
    assert_eq!(e.stream_prepare_count(), 1, "the audio went on by itself");
    let got_events = e.take_events();
    assert!(notices(&got_events).is_empty());
    let got_entries = e.now_playing_entries();
    e.sync_session_playback_fields();
    // Same list, same current item, same cues as after `Next`.
    assert_eq!(
        got_entries.iter().map(|x| (&x.title, x.kind)).collect::<Vec<_>>(),
        want_entries.iter().map(|x| (&x.title, x.kind)).collect::<Vec<_>>()
    );
    assert_eq!(e.session().current_entry_index, want.0);
    assert_eq!(e.session().current_cue_index, want.1, "cue of the item entered");
    assert!(e.session().position_ms < 200, "position {} should restart at 0:00", e.session().position_ms);
    // The republished list names the same current row as the reference's.
    let last_list = |evs: &[EngineEvent]| {
        evs.iter().rev().find_map(|ev| match ev {
            EngineEvent::NowPlaying(l) => Some((l.current_index, l.list_kind, l.entries.len())),
            _ => None,
        })
    };
    assert_eq!(last_list(&got_events), last_list(&want_list));
    // The resume bookmark of an item entered this way names it (a pause writes it).
    gate.step(&mut e);
    cmd(&mut e, PlaybackCommand::Pause);
    gate.step(&mut e);
    assert_eq!(
        e.resume_entry("album.noubin.s").map(|b| b.track_index),
        Some(want.0 as usize),
        "the bookmark names the item entered"
    );
    gate.allowed.store(usize::MAX, Ordering::SeqCst);
}

#[test]
fn next_inside_the_join_window_goes_to_the_track_after_the_joined_one() {
    let l = lib(&[(TRACK_FRAMES, RATE); 3], "music", "");
    let gate = Gate::new(0, 0);
    let mut e = gate.engine(&l.root);
    cmd(&mut e, PlaybackCommand::LoadCollection { playable_id: gid(&l.root) });
    gate.step_until(&mut e, "inside the join window", |e| entry(e) == 1);
    let written = gate.captured().len();
    assert!(written < TRACK_FRAMES + 12_000, "still inside the appended head ({written})");
    // The user presses Next here: exactly what Next at 0:00 of the new track does.
    cmd(&mut e, PlaybackCommand::Next);
    gate.step_until(&mut e, "the third track plays", |e| entry(e) == 2 && e.stream_prepare_count() == 2);
    gate.allowed.store(usize::MAX, Ordering::SeqCst);
    wait_until(&mut e, "the album ends", |e| e.session().transport == TransportState::Stopped);
    // No double advance: the last track played once, from its start, to its end.
    assert_eq!(entry(&mut e), 2);
    assert_eq!(e.stream_prepare_count(), 2);
    let out = gate.captured();
    let third = &at_volume(&e, &l.expected)[2 * TRACK_FRAMES..];
    assert!(out.len() >= third.len());
    assert!(worst_diff(&out[out.len() - third.len()..], third) < 1e-6, "the third track plays whole");
}

#[test]
fn pause_inside_the_join_window_then_play_neither_repeats_nor_skips_audio() {
    let l = lib(&[(TRACK_FRAMES, RATE); 2], "music", "");
    let gate = Gate::new(0, 0);
    let mut e = gate.engine(&l.root);
    cmd(&mut e, PlaybackCommand::LoadCollection { playable_id: gid(&l.root) });
    gate.step_until(&mut e, "inside the join window", |e| entry(e) == 1);
    cmd(&mut e, PlaybackCommand::Pause);
    gate.step(&mut e); // the parked write completes, the Pause is handled behind it
    wait_until(&mut e, "paused", |e| e.session().transport == TransportState::Paused);
    assert_eq!(entry(&mut e), 1, "the listener is on the second track");
    let at_pause = gate.captured().len();
    cmd(&mut e, PlaybackCommand::Play);
    gate.allowed.store(usize::MAX, Ordering::SeqCst);
    wait_until(&mut e, "the album ends", |e| e.session().transport == TransportState::Stopped);
    // The sink reports no queue here, so nothing is put back: the stream goes on exactly where
    // it was, and the whole capture is the two files back to back.
    let out = gate.captured();
    assert!(at_pause < out.len());
    assert_eq!(out.len(), l.expected.len());
    assert!(worst_diff(&out, &at_volume(&e, &l.expected)) < 1e-6);
    assert_eq!(e.stream_prepare_count(), 1);
}

#[test]
fn previous_inside_the_join_window_returns_to_the_track_just_left() {
    let l = lib(&[(TRACK_FRAMES, RATE); 2], "music", "");
    let gate = Gate::new(0, 0);
    let mut e = gate.engine(&l.root);
    cmd(&mut e, PlaybackCommand::LoadCollection { playable_id: gid(&l.root) });
    gate.step_until(&mut e, "inside the join window", |e| entry(e) == 1);
    let at_press = gate.captured().len();
    cmd(&mut e, PlaybackCommand::Previous);
    gate.step_until(&mut e, "the first track restarted", |e| entry(e) == 0 && e.stream_prepare_count() == 2);
    for _ in 0..8 {
        gate.step(&mut e);
    }
    // The first track plays again from its first frame: somewhere behind the press (the write
    // that was parked finishes first) the output is the track's start.
    let out = gate.captured();
    let want = at_volume(&e, &l.expected[..4_096]);
    let restart = (at_press..at_press + 2_048)
        .find(|&k| out.len() >= k + 4_096 && worst_diff(&out[k..k + 4_096], &want) < 1e-6);
    assert!(restart.is_some(), "the first track did not start over after the press");
}

#[test]
fn a_jog_back_across_the_just_crossed_boundary_plays_the_track_just_left_from_there() {
    let l = lib(&[(TRACK_FRAMES, RATE); 2], "music", "");
    let gate = Gate::new(0, 0);
    let mut e = gate.engine(&l.root);
    cmd(&mut e, PlaybackCommand::LoadCollection { playable_id: gid(&l.root) });
    cmd(&mut e, PlaybackCommand::SetSeekOutsideTrack(true));
    gate.step_until(&mut e, "inside the join window", |e| entry(e) == 1);
    // The jog commit that lands in the previous item, 3 s in (WARP_19's path).
    cmd(&mut e, PlaybackCommand::SeekToItem { item_delta: -1, target_ms: 3_000 });
    gate.step_until(&mut e, "the first track plays from the jog", |e| entry(e) == 0 && e.stream_prepare_count() == 2);
    for _ in 0..10 {
        gate.step(&mut e);
    }
    let out = gate.captured();
    let tail = &out[out.len() - 3_000..out.len() - 1_000];
    let first = at_volume(&e, &l.expected[..TRACK_FRAMES]);
    // Somewhere in the first track, from 3 s (24 000 frames) on and no further than the
    // frames played since.
    let found = (24_000..24_000 + 16 * 1024).find(|&k| worst_diff(tail, &first[k..k + 2_000]) < 1e-6);
    assert!(found.is_some(), "the first track is not playing from the jog target");
    let pos = e.current_position_ms().unwrap();
    assert!((3_000..5_000).contains(&pos), "position {pos} ms");
}

#[test]
fn repeat_item_pause_after_each_and_the_queue_wrap_keep_their_old_boundaries() {
    // Repeat-item: the same item again, as a restart of the stream (no join into the next).
    let l = lib(&[(TRACK_FRAMES, RATE); 2], "music", "");
    let gate = Gate::new(2, usize::MAX);
    let mut e = gate.engine(&l.root);
    cmd(&mut e, PlaybackCommand::LoadCollection { playable_id: gid(&l.root) });
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Item)); // (a load clears it)
    wait_until(&mut e, "the first item played twice", |e| {
        gate.captured().len() >= 2 * TRACK_FRAMES && e.stream_prepare_count() >= 1
    });
    cmd(&mut e, PlaybackCommand::Stop);
    assert_eq!(entry(&mut e), 0, "repeat item stays on the item");
    let out = gate.captured();
    assert!(
        worst_diff(&out[TRACK_FRAMES..2 * TRACK_FRAMES], &at_volume(&e, &l.expected[..TRACK_FRAMES])) < 1e-6
    );

    // Pause-after-each: the item ends, the next one is current at 0:00 and nothing plays on.
    let gate = Gate::new(1, usize::MAX);
    let mut e = gate.engine(&l.root);
    cmd(&mut e, PlaybackCommand::SetPauseAfterEach(true));
    cmd(&mut e, PlaybackCommand::LoadCollection { playable_id: gid(&l.root) });
    wait_until(&mut e, "held after the first item", |e| e.session().transport == TransportState::Stopped);
    assert_eq!(entry(&mut e), 1);
    assert_eq!(e.stream_prepare_count(), 1, "nothing was opened for the held item");
    assert_eq!(gate.captured().len(), TRACK_FRAMES, "nothing of the next track was heard");

    // Repeat-queue: the wrap joins the first track again, on the same stream.
    let gate = Gate::new(5, usize::MAX);
    let mut e = gate.engine(&l.root);
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Queue));
    cmd(&mut e, PlaybackCommand::LoadCollection { playable_id: gid(&l.root) });
    wait_until(&mut e, "one and a half passes", |e| {
        entry(e);
        gate.captured().len() >= 3 * TRACK_FRAMES
    });
    cmd(&mut e, PlaybackCommand::Stop);
    let out = gate.captured();
    let two = at_volume(&e, &l.expected);
    assert!(worst_diff(&out[..2 * TRACK_FRAMES], &two) < 1e-6);
    assert!(worst_diff(&out[2 * TRACK_FRAMES..3 * TRACK_FRAMES], &two[..TRACK_FRAMES]) < 1e-6);
    assert_eq!(e.stream_prepare_count(), 1, "the wrap restarted the stream");
}

#[test]
fn a_neighbour_that_cannot_be_joined_advances_the_old_way_with_no_missing_file_warning() {
    // The second track has another sample rate: no join. The ordinary advance opens it.
    let l = lib(&[(TRACK_FRAMES, RATE), (RATE as usize * 2, 44_100)], "music", "");
    let gate = Gate::new(5, usize::MAX);
    let mut e = gate.engine(&l.root);
    cmd(&mut e, PlaybackCommand::LoadCollection { playable_id: gid(&l.root) });
    wait_until(&mut e, "the album ends", |e| {
        entry(e) == 1 && e.session().transport == TransportState::Stopped
    });
    assert_eq!(e.stream_prepare_count(), 2, "the second track was opened by the normal advance");
    let events = e.take_events();
    assert!(
        !notices(&events).iter().any(|c| c == NOTICE_MEDIA_FILE_MISSING),
        "a seamless attempt must not warn: {:?}",
        notices(&events)
    );
    assert!(notices(&events).is_empty(), "{:?}", notices(&events));
}

#[test]
fn a_reorder_that_keeps_the_indices_but_changes_the_next_item_changes_what_is_offered() {
    let l = lib(&[(2_000, RATE); 5], "music", "");
    let mut e = Engine::open_mock(&l.root, Capabilities::desktop_mock()).unwrap();
    cmd(&mut e, PlaybackCommand::Load(gid(&l.root)));
    let before = e.neighbour_key().unwrap();
    let first = e.seamless_next().unwrap().path;
    // Reorder the list underneath (as a shuffle does) until another item sits at the index
    // after the current one: the indices of the key do not change, the item does.
    let mut changed = None;
    for seed in 1..64u64 {
        e.loaded.as_mut().unwrap().list.set_order(OrderMode::Shuffle, seed);
        let now = e.seamless_next().unwrap().path;
        if now != first {
            changed = Some(now);
            break;
        }
    }
    assert!(changed.is_some(), "no seed moved another item next");
    let after = e.neighbour_key().unwrap();
    assert_eq!(before.current, after.current);
    assert_eq!(before.seamless.map(|s| s.0), after.seamless.map(|s| s.0), "same next index");
    assert_ne!(before, after, "the key must see the item behind the index");
    // And the hub tick re-sends: the audio thread is told the item now next.
    e.extras.neighbours_sent = Some(before);
    e.sync_neighbours();
    assert_eq!(e.extras.neighbours_sent, Some(after));
}

#[test]
fn set_order_mode_resends_the_neighbours() {
    let l = lib(&[(2_000, RATE); 5], "music", "");
    let mut e = Engine::open_mock(&l.root, Capabilities::desktop_mock()).unwrap();
    cmd(&mut e, PlaybackCommand::Load(gid(&l.root)));
    e.sync_neighbours();
    let before = e.extras.neighbours_sent.clone().unwrap();
    let mut moved = false;
    for seed in 1..64u64 {
        e.set_shuffle_seed(seed);
        cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
        e.sync_neighbours();
        let now = e.extras.neighbours_sent.clone().unwrap();
        // what is sent always equals what the engine decides now
        assert_eq!(Some(now.clone()), e.neighbour_key());
        if now != before {
            moved = true;
            break;
        }
        cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Normal));
    }
    assert!(moved, "shuffling never changed what is offered next");
}

#[test]
fn a_command_after_the_audio_flipped_acts_on_the_item_being_heard() {
    // The audio went on into the second track; the hub tick has not yet told the engine. A
    // Next arriving now must go to the THIRD track, not restart the second.
    let l = lib(&[(TRACK_FRAMES, RATE); 3], "music", "");
    let gate = Gate::new(0, 0);
    let mut e = gate.engine(&l.root);
    cmd(&mut e, PlaybackCommand::LoadCollection { playable_id: gid(&l.root) });
    let crossings = |e: &Engine| e.audio.as_ref().unwrap().stats.crossings.load(Ordering::SeqCst);
    let t = Instant::now();
    while crossings(&e) < 1 {
        // (the neighbours are sent, the crossings are not drained)
        e.sync_neighbours();
        let target = gate.writes.load(Ordering::SeqCst) + 1;
        gate.allowed.store(target, Ordering::SeqCst);
        // (no engine poll: the crossing event stays queued for the engine)
        while !(gate.writes.load(Ordering::SeqCst) == target && gate.blocked.load(Ordering::SeqCst))
            && crossings(&e) < 1
        {
            assert!(t.elapsed() < Duration::from_secs(10), "no crossing");
            thread::sleep(Duration::from_micros(200));
        }
        thread::sleep(Duration::from_micros(300));
        assert!(t.elapsed() < Duration::from_secs(10), "no crossing");
    }
    assert_eq!(e.loaded.as_ref().unwrap().list.current_index(), 0, "the engine has not been told");
    cmd(&mut e, PlaybackCommand::Next);
    // Straight away (no further stepping): the third track, one new stream.
    assert_eq!(e.loaded.as_ref().unwrap().list.current_index(), 2, "Next acted on a stale item");
    assert_eq!(e.stream_prepare_count(), 2);
    assert_eq!(entry(&mut e), 2);
}
