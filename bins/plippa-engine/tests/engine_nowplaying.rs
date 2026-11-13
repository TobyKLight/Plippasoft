//! `nowPlayingList` engine tests (NB_04): collections, cue navigation, resume.
//!
//! No wall-clock assertions: navigation tests use `Load` (prepared, not playing) plus
//! `SeekMs` so positions are exact; auto-advance tests poll for *state*, not timing.

use plippa_core::{
    Capabilities, Clock, NowPlayingEntryKind, NowPlayingListKind, OrderMode, RepeatMode, ScanDir,
    SourceOrder, TransportState,
};
use plippa_engine::{Engine, EngineEvent};
use plippa_ipc_proto::{
    IpcMessage, NoticeSeverity, PlaybackCommand, PlaybackStatus, ResumeWriteback, TapKeyPayload,
    NOTICE_MEDIA_FILE_MISSING, NOTICE_NO_FIXED_ORDER,
};
use plippa_noubin::{
    encode_playable_id, normalise_noubin_key, LibraryReader, LibraryRoot, LibraryWriter,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const RATE: u32 = 8_000;

/// Mono 16-bit WAV of `secs` seconds (small: 16 kB per second at 8 kHz).
fn wav(path: &Path, secs: f32) {
    use std::io::Write;
    let frames = (RATE as f32 * secs) as usize;
    let data_bytes = frames * 2;
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut out = fs::File::create(path).unwrap();
    out.write_all(b"RIFF").unwrap();
    out.write_all(&(36 + data_bytes as u32).to_le_bytes())
        .unwrap();
    out.write_all(b"WAVEfmt ").unwrap();
    out.write_all(&16u32.to_le_bytes()).unwrap();
    out.write_all(&1u16.to_le_bytes()).unwrap();
    out.write_all(&1u16.to_le_bytes()).unwrap();
    out.write_all(&RATE.to_le_bytes()).unwrap();
    out.write_all(&(RATE * 2).to_le_bytes()).unwrap();
    out.write_all(&2u16.to_le_bytes()).unwrap();
    out.write_all(&16u16.to_le_bytes()).unwrap();
    out.write_all(b"data").unwrap();
    out.write_all(&(data_bytes as u32).to_le_bytes()).unwrap();
    for i in 0..frames {
        let s = ((i as f32 / RATE as f32 * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        out.write_all(&s.to_le_bytes()).unwrap();
    }
}

fn put(root: &Path, rel: &str, contents: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
}

fn scan(root: &Path) {
    LibraryWriter::open(&LibraryRoot::new(root))
        .unwrap()
        .full_scan()
        .unwrap();
}

/// Global (namespaced) playable id of the playable titled `title`.
fn gid(root: &Path, title: &str) -> plippa_core::PlayableItemId {
    let reader = LibraryReader::open(&LibraryRoot::new(root)).unwrap();
    let rec = reader
        .list_playables()
        .unwrap()
        .into_iter()
        .find(|p| p.title.as_deref() == Some(title))
        .unwrap_or_else(|| panic!("no playable titled {title}"));
    plippa_core::PlayableItemId::new(encode_playable_id(root, rec.id))
}

/// Injected monotonic clock (NB_04b): moves only when the test says so; `step_ms` is added
/// after every read (a large step = "every Previous press is far apart").
#[derive(Debug, Clone)]
struct TestClock {
    ms: Arc<AtomicU64>,
    step_ms: u64,
}

impl Clock for TestClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.ms.fetch_add(self.step_ms, Ordering::SeqCst))
    }
}

/// Engine whose clock never lets two Previous presses pair up (each read moves 60 s on).
fn engine(root: &Path) -> Engine {
    let mut e = Engine::open_mock(root, Capabilities::desktop_mock()).unwrap();
    e.set_clock(Box::new(TestClock {
        ms: Arc::new(AtomicU64::new(0)),
        step_ms: 60_000,
    }));
    e
}

/// Engine with a frozen clock plus a handle to advance it.
fn engine_ck(root: &Path) -> (Engine, Arc<AtomicU64>) {
    let ms = Arc::new(AtomicU64::new(1_000_000));
    let mut e = Engine::open_mock(root, Capabilities::desktop_mock()).unwrap();
    e.set_clock(Box::new(TestClock {
        ms: Arc::clone(&ms),
        step_ms: 0,
    }));
    (e, ms)
}

fn advance(ms: &Arc<AtomicU64>, by: u64) {
    ms.fetch_add(by, Ordering::SeqCst);
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

fn titles(e: &Engine) -> Vec<String> {
    e.now_playing_entries()
        .into_iter()
        .map(|x| x.title)
        .collect()
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(15), "timed out: {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

/// Album with `n` tracks "T1".."Tn" of `secs` seconds; `extra` goes into `localPlaybackData`.
fn album(root: &Path, n: usize, secs: f32, profile: &str, key: &str, local: &str) {
    let mut items = Vec::new();
    for i in 1..=n {
        wav(&root.join(format!("Music/Album/{i:02}.wav")), secs);
        items.push(format!(
            r#"{{"mediaTitle":"T{i}","localFilePaths":["{i:02}.wav"]}}"#
        ));
    }
    put(
        root,
        "Music/Album/_album.noudata",
        &format!(
            r#"{{"noubinKey":"{key}","noubinKeyNormalised":"{}","playableItemTitle":"Album","playableItemProfile":"{profile}",
               "releaseData":{{"credits":{{"primaryArtists":[{{"primaryArtistName":"The Band"}}]}}}},
               {local}
               "mediaList":[{}]}}"#,
            normalise_noubin_key(key),
            items.join(",")
        ),
    );
    scan(root);
}

fn mixtape(root: &Path) {
    wav(&root.join("Mixes/mix.wav"), 50.0);
    let cues: Vec<String> = (0..5)
        .map(|i| {
            format!(
                r#"{{"startTime":"{}","cueTextSpans":[{{"text":"Part {}"}}]}}"#,
                i * 10,
                i + 1
            )
        })
        .collect();
    put(
        root,
        "Mixes/_mix.noudata",
        &format!(
            r#"{{"noubinKey":"mix.noubin.k","noubinKeyNormalised":"mix.noubin.k","playableItemTitle":"Mixtape",
               "playableItemProfile": "mix", "releaseData":{{}},
               "mediaList":[{{"localFilePaths":["mix.wav"],
                 "cueLists":[{{"cueListType":"nav","cues":[{}]}}]}}]}}"#,
            cues.join(",")
        ),
    );
    scan(root);
}

fn root() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().to_path_buf();
    (t, p)
}

#[test]
fn album_next_walks_tracks_and_previous_restarts_first() {
    let (_t, r) = root();
    album(&r, 3, 20.0, "music", "album.noubin.a", "");
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    assert_eq!(titles(&e), ["T1", "T2", "T3"]);
    assert_eq!(status(&mut e).session.current_entry_index, 0);
    assert_eq!(e.stream_prepare_count(), 1);

    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(status(&mut e).session.current_entry_index, 1);
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(status(&mut e).session.current_entry_index, 2);
    cmd(&mut e, PlaybackCommand::Next); // list end: no-op, no error
    assert_eq!(status(&mut e).session.current_entry_index, 2);
    assert_eq!(e.stream_prepare_count(), 3);
    assert!(e
        .take_events()
        .iter()
        .all(|ev| !matches!(ev, EngineEvent::Notice(_))));

    // FIX_01C: a first press past 1 s restarts the entry...
    let prepares = e.stream_prepare_count();
    cmd(&mut e, PlaybackCommand::SeekMs(2_000));
    cmd(&mut e, PlaybackCommand::Previous);
    let st = status(&mut e);
    assert_eq!(st.session.current_entry_index, 2);
    assert_eq!(st.session.position_ms, 0);
    assert_eq!(e.stream_prepare_count(), prepares, "restart is a seek");
    cmd(&mut e, PlaybackCommand::SeekMs(10_000));
    cmd(&mut e, PlaybackCommand::PreviousTrack); // legacy alias behaves the same
    assert_eq!(status(&mut e).session.current_entry_index, 2);
    assert_eq!(status(&mut e).session.position_ms, 0);
}

#[test]
fn previous_double_press_goes_to_previous_track_and_window_is_respected() {
    let (_t, r) = root();
    album(&r, 3, 20.0, "music", "album.noubin.pd", "");
    let (mut e, ck) = engine_ck(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    let cur = |e: &mut Engine| status(e).session.current_entry_index;
    assert_eq!(cur(&mut e), 0);

    // FIX_01C: a fresh track, one press 300 ms in, goes back a track — one command, no wasted
    // rewind (rapid taps walk back one track per tap, matching Next).
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(cur(&mut e), 1);
    cmd(&mut e, PlaybackCommand::SeekMs(300));
    let prepares = e.stream_prepare_count();
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!((cur(&mut e), e.current_position_ms()), (0, Some(0)));
    assert_eq!(
        e.stream_prepare_count(),
        prepares + 1,
        "exactly one track reload for one press"
    );

    cmd(&mut e, PlaybackCommand::Next);
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(cur(&mut e), 2);

    // Past 1 s a first press restarts; a second within 3 s goes to the previous track.
    cmd(&mut e, PlaybackCommand::SeekMs(10_000));
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!((cur(&mut e), e.current_position_ms()), (2, Some(0)));
    advance(&ck, 2_999);
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(cur(&mut e), 1);
    // The pair was consumed by going back; under FIX_01C the next press at the entry start
    // steps back again (rapid taps walk back one track per tap), and at the first track it
    // restarts.
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(cur(&mut e), 0);
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(cur(&mut e), 0);
    // First track, nothing before it: restart again, no error notice.
    e.take_events();
    cmd(&mut e, PlaybackCommand::SeekMs(5_000));
    cmd(&mut e, PlaybackCommand::Previous);
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!((cur(&mut e), e.current_position_ms()), (0, Some(0)));
    assert!(e
        .take_events()
        .iter()
        .all(|ev| !matches!(ev, EngineEvent::Notice(_))));

    // Past the pairing window a press is a *first* press again; the restart left the playhead
    // at the entry start, so under FIX_01C it now steps back rather than restarting.
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(cur(&mut e), 1);
    cmd(&mut e, PlaybackCommand::SeekMs(4_000));
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!((cur(&mut e), e.current_position_ms()), (1, Some(0)));
    advance(&ck, 3_001);
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(cur(&mut e), 0, "window expired: a fresh first press near the start");
}

#[test]
fn play_and_pause_keep_the_recent_previous_pair_and_seek_clears_it() {
    // FIX_01C changed what a "recent Previous" looks like: at an entry start (or within the
    // first second) a first press and a pairing press both step back, so the clear rule is only
    // observable once the playhead is past the 1 s restart threshold. These are the two
    // observable cases; the remaining clearing commands behave identically in the reachable
    // near-start positions, and `PreviousPressTracker::clear` is unit-tested in plippa-core.
    let (_t, r) = root();
    album(&r, 3, 60.0, "music", "album.noubin.pc", "");
    let (mut e, _ck) = engine_ck(&r);
    let album_id = gid(&r, "Album");
    cmd(&mut e, PlaybackCommand::Load(album_id));
    let cur = |e: &mut Engine| status(e).session.current_entry_index;

    // Seek clears: the pending pair from before the seek must not turn the next press into a
    // "step back" at 4 s; it restarts track 1 instead.
    cmd(&mut e, PlaybackCommand::JumpToEntry { index: 2 });
    cmd(&mut e, PlaybackCommand::Previous); // first press at the entry start: one track back
    assert_eq!(cur(&mut e), 1);
    cmd(&mut e, PlaybackCommand::SeekMs(4_000));
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(
        (cur(&mut e), e.current_position_ms()),
        (1, Some(0)),
        "Seek must clear the recent-Previous state (this press restarts track 1)"
    );

    // Play / Pause in between do not clear: after 1.5 s of playback the pairing press still
    // steps back a track (a cleared state would have restarted track 1 instead).
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(cur(&mut e), 2);
    cmd(&mut e, PlaybackCommand::Previous); // first press at the entry start: pair pending
    assert_eq!(cur(&mut e), 1);
    cmd(&mut e, PlaybackCommand::Play);
    wait_until("playback past 1.5 s", || {
        e.current_position_ms() > Some(1_500)
    });
    cmd(&mut e, PlaybackCommand::Pause);
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(
        cur(&mut e),
        0,
        "Play/Pause must not clear the recent-Previous state"
    );
}

#[test]
fn previous_with_nothing_loaded_is_a_silent_no_op() {
    let (_t, r) = root();
    album(&r, 1, 2.0, "music", "album.noubin.pn", "");
    let (mut e, _ck) = engine_ck(&r);
    e.take_events();
    cmd(&mut e, PlaybackCommand::Previous);
    cmd(&mut e, PlaybackCommand::Previous);
    assert!(e
        .take_events()
        .iter()
        .all(|ev| !matches!(ev, EngineEvent::Notice(_))));
    assert!(e.now_playing_entries().is_empty());
}

#[test]
fn mixtape_cues_seek_within_one_track_without_reprepare() {
    let (_t, r) = root();
    mixtape(&r);
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Mixtape")));

    // One track + 5 virtual cue entries beneath it.
    let entries = e.now_playing_entries();
    assert_eq!(entries.len(), 6);
    assert_eq!(entries[0].kind, NowPlayingEntryKind::Track);
    assert!(entries[1..]
        .iter()
        .all(|c| c.kind == NowPlayingEntryKind::Cue));
    assert_eq!(entries[3].title, "Part 3");
    assert_eq!(entries[3].start_ms, 20_000);
    assert_eq!(e.stream_prepare_count(), 1);

    let cue = |e: &mut Engine| status(e).session.current_cue_index;
    assert_eq!(cue(&mut e), Some(0));
    for want in 1..5u32 {
        cmd(&mut e, PlaybackCommand::Next);
        assert_eq!(cue(&mut e), Some(want), "after Next #{want}");
        assert_eq!(e.current_position_ms(), Some(u64::from(want) * 10_000));
    }
    // After the last cue there is no next track: no-op, still one prepare.
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(cue(&mut e), Some(4));
    assert_eq!(
        e.stream_prepare_count(),
        1,
        "cue navigation never re-prepares"
    );

    // Highlight follows the media position.
    cmd(&mut e, PlaybackCommand::SeekMs(25_500));
    assert_eq!(cue(&mut e), Some(2));

    // Previous (FIX_01C): within 1 s of the cue start a first press steps back to the previous
    // cue; later it restarts the cue. The seek target is the cue's start_ms (media position).
    cmd(&mut e, PlaybackCommand::SeekMs(21_000)); // exactly 1 s into cue 2
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(
        e.current_position_ms(),
        Some(10_000),
        "within the first second: previous cue"
    );
    cmd(&mut e, PlaybackCommand::SeekMs(21_001)); // 1001 ms into cue 2
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(
        e.current_position_ms(),
        Some(20_000),
        "past the threshold: restart the current cue"
    );
    cmd(&mut e, PlaybackCommand::SeekMs(29_000)); // 9 s into cue 2
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(e.current_position_ms(), Some(20_000));
    assert_eq!(cue(&mut e), Some(2));

    // JumpToEntry on a cue entry (flat index 1 + cue) seeks too.
    cmd(&mut e, PlaybackCommand::JumpToEntry { index: 4 }); // Part 4
    assert_eq!(e.current_position_ms(), Some(30_000));
    assert_eq!(e.stream_prepare_count(), 1);
}

#[test]
fn disabling_a_cue_list_removes_its_entries_and_nav_points() {
    let (_t, r) = root();
    wav(&r.join("m.wav"), 50.0);
    put(
        &r,
        "_m.noudata",
        r#"{"noubinKeyNormalised":"m.k","playableItemTitle":"Two Lists","playableItemProfile": "mix", "releaseData":{},
            "mediaList":[{"localFilePaths":["m.wav"],"cueLists":[
              {"cueListType":"nav","cues":[{"startTime":"0","cueTextSpans":[{"text":"A"}]},{"startTime":"30","cueTextSpans":[{"text":"C"}]}]},
              {"cueListType":"credits","cues":[{"startTime":"10","cueTextSpans":[{"text":"B"}]}]},
              {"cueListType":"text","cues":[{"startTime":"20","cueTextSpans":[{"text":"not navigable"}]}]}]}]}"#,
    );
    scan(&r);
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Two Lists")));
    // Merged across navigable lists, sorted; the non-navigable text list is absent.
    assert_eq!(titles(&e), ["Two Lists", "A", "B", "C"]);
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(e.current_position_ms(), Some(10_000)); // credits cue counts as a nav point
    cmd(
        &mut e,
        PlaybackCommand::SetCueListEnabled {
            list_index: 1,
            enabled: false,
        },
    );
    assert!(!e.cue_list_enabled(1));
    assert_eq!(titles(&e), ["Two Lists", "A", "C"]);
    e.take_events();
    cmd(&mut e, PlaybackCommand::SeekMs(0));
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(
        e.current_position_ms(),
        Some(30_000),
        "disabled list is skipped"
    );
    assert_eq!(e.stream_prepare_count(), 1);
}

#[test]
fn playlist_over_two_releases_resolves_metadata_per_track() {
    let (_t, r) = root();
    for (dir, title, song) in [("A", "Release A", "Song A1"), ("B", "Release B", "Song B1")] {
        wav(&r.join(format!("Music/{dir}/1.wav")), 5.0);
        put(
            &r,
            &format!("Music/{dir}/_rel.noudata"),
            &format!(
                r#"{{"noubinKeyNormalised":"{dir}.k","playableItemTitle":"{title}",
                   "playableItemProfile": "music", "releaseData":{{"credits":{{"primaryArtists":[{{"primaryArtistName":"Artist {dir}"}}]}}}},
                   "mediaList":[{{"mediaTitle":"{song}","localFilePaths":["1.wav"]}}]}}"#
            ),
        );
    }
    put(
        &r,
        "Playlists/_Mixed.noudata",
        r#"{"playableItemTitle":"Mixed","localPlaybackData":{"userNotes":{"createdByUser":"Plippa"}},
            "mediaList":[{"localFilePaths":["../Music/B/1.wav"]},{"localFilePaths":["../Music/A/1.wav"]}]}"#,
    );
    scan(&r);
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Mixed")));
    let entries = e.now_playing_entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].title, "Song B1");
    assert_eq!(entries[0].artist, "Artist B");
    assert_eq!(entries[1].title, "Song A1");
    assert_eq!(entries[1].artist, "Artist A");
    assert_eq!(entries[0].playable_id, Some(gid(&r, "Release B")));
    assert_eq!(entries[1].playable_id, Some(gid(&r, "Release A")));
    e.take_events();
    e.publish_now_playing();
    let list = e
        .take_events()
        .into_iter()
        .find_map(|ev| match ev {
            EngineEvent::NowPlaying(l) => Some(l),
            _ => None,
        })
        .expect("list event for a newly connected client");
    assert_eq!(list.list_kind, NowPlayingListKind::Playlist);
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(status(&mut e).session.current_entry_index, 1);
}

#[test]
fn loading_a_second_collection_replaces_the_list() {
    let (_t, r) = root();
    album(&r, 3, 5.0, "music", "album.noubin.b", "");
    mixtape(&r);
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    assert_eq!(e.now_playing_entries().len(), 3);
    e.take_events();
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Mixtape"),
        },
    );
    assert_eq!(
        titles(&e).len(),
        6,
        "album fully replaced by mixtape + cues"
    );
    assert_eq!(titles(&e)[0], "Mixtape"); // untitled single item shows the playable's title
    let evs = e.take_events();
    let lists: Vec<_> = evs
        .iter()
        .filter_map(|ev| match ev {
            EngineEvent::NowPlaying(l) => Some(l),
            _ => None,
        })
        .collect();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0].list_kind, NowPlayingListKind::Mix);
    assert_eq!(lists[0].current_index, 0);
    let s = status(&mut e);
    assert_eq!(s.session.transport, TransportState::Playing);
    assert_eq!(s.session.title, "Mixtape");
}

#[test]
fn nfc_tap_on_single_track_and_collection_still_work() {
    let (_t, r) = root();
    wav(&r.join("tone.wav"), 3.0);
    put(
        &r,
        "_tone.noudata",
        r#"{"noubinKey":"tone.k","noubinKeyNormalised":"tone.k","playableItemTitle":"Tone","mediaList":[{"localFilePaths":["tone.wav"]}]}"#,
    );
    album(&r, 2, 3.0, "music", "album.noubin.c", "");
    let mut e = engine(&r);
    for (key, n) in [("tone.k", 1usize), ("album.noubin.c", 2)] {
        e.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
            noubin_key: key.into(),
            tap_id: 1,
            already_normalised: false,
        }));
        assert_eq!(e.session().transport, TransportState::Playing, "{key}");
        assert_eq!(e.now_playing_entries().len(), n, "{key}");
    }
    let evs = e.take_events();
    let kinds: Vec<_> = evs
        .iter()
        .filter_map(|ev| match ev {
            EngineEvent::NowPlaying(l) => Some(l.list_kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        [NowPlayingListKind::Single, NowPlayingListKind::Album]
    );
}

#[test]
fn finished_track_advances_and_last_track_stops() {
    let (_t, r) = root();
    album(&r, 3, 0.4, "music", "album.noubin.d", "");
    let mut e = engine(&r);
    let mut seen = vec![0u32];
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    wait_until("album plays through", || {
        e.poll_stream_lifecycle();
        let i = e.session().current_entry_index;
        let _ = status(&mut e);
        let i = status(&mut e).session.current_entry_index.max(i);
        if seen.last() != Some(&i) {
            seen.push(i);
        }
        e.session().transport == TransportState::Stopped
    });
    assert_eq!(*seen.last().unwrap(), 2, "ends on the last track: {seen:?}");
    assert!(
        seen.windows(2).all(|w| w[1] > w[0]),
        "in order, no repeats: {seen:?}"
    );
}

#[test]
fn retired_pause_after_each_key_no_longer_stops_between_items() {
    let (_t, r) = root();
    album(
        &r,
        2,
        0.8,
        "music",
        "album.noubin.e",
        r#""localPlaybackData":{"stopAfterEach":true},"#,
    );
    let mut e = engine(&r);
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    // Noubin v0.4.1: the key is a player setting, not file data, so the list just advances.
    wait_until("second item started on its own", || {
        e.poll_stream_lifecycle();
        e.session().current_entry_index == 1 && e.session().transport == TransportState::Playing
    });
    assert_eq!(status(&mut e).session.current_entry_index, 1);
}

#[test]
fn mixtape_previous_double_press_goes_to_previous_cue() {
    let (_t, r) = root();
    mixtape(&r);
    let (mut e, ck) = engine_ck(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Mixtape")));
    let cue = |e: &mut Engine| status(e).session.current_cue_index;
    cmd(&mut e, PlaybackCommand::SeekMs(27_000)); // 7 s into "Part 3"
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(
        (e.current_position_ms(), cue(&mut e)),
        (Some(20_000), Some(2))
    );
    advance(&ck, 1_500);
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(
        (e.current_position_ms(), cue(&mut e)),
        (Some(10_000), Some(1))
    );
    // FIX_01C: the cleared pair's next press is a first press at the "Part 2" cue start, so it
    // steps back to "Part 1"; at the first cue there is nothing earlier and it restarts.
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!((e.current_position_ms(), cue(&mut e)), (Some(0), Some(0)));
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(e.current_position_ms(), Some(0));
    assert_eq!(
        e.stream_prepare_count(),
        1,
        "cue navigation never re-prepares"
    );
}

#[test]
fn retired_repeat_list_key_no_longer_wraps_the_list() {
    let (_t, r) = root();
    album(
        &r,
        3,
        5.0,
        "music",
        "album.noubin.f",
        r#""localPlaybackData":{"repeatList":true},"#,
    );
    let (mut e, _ck) = engine_ck(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    // Noubin v0.4.1: a load no longer honours the file's repeatList.
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(
        status(&mut e).session.current_entry_index,
        0,
        "no backwards wrap: the first entry restarts"
    );
    assert_eq!(e.current_position_ms(), Some(0));
    cmd(&mut e, PlaybackCommand::Next);
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(status(&mut e).session.current_entry_index, 2);
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(
        status(&mut e).session.current_entry_index,
        2,
        "no forwards wrap at the last entry"
    );
}

// The file-driven shuffle test lived here. Noubin v0.4.1 retired `localPlaybackData.shuffle`
// (a player setting); PLAY_02 re-creates the coverage against `SetOrder(Shuffle)`.

#[test]
fn missing_media_is_skipped_with_a_notice() {
    let (_t, r) = root();
    album(&r, 4, 5.0, "music", "album.noubin.h", "");
    fs::remove_file(r.join("Music/Album/02.wav")).unwrap();
    fs::remove_file(r.join("Music/Album/03.wav")).unwrap();
    let (mut e, _ck) = engine_ck(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    let flags: Vec<bool> = e
        .now_playing_entries()
        .iter()
        .map(|x| x.available)
        .collect();
    assert_eq!(flags, [true, false, false, true]);
    e.take_events();
    cmd(&mut e, PlaybackCommand::Next);
    let warnings = e
        .take_events()
        .into_iter()
        .filter(|ev| matches!(ev, EngineEvent::Notice(n) if n.code == NOTICE_MEDIA_FILE_MISSING))
        .count();
    assert_eq!(warnings, 2, "one notice per skipped entry");
    assert_eq!(
        status(&mut e).session.current_entry_index,
        3,
        "jumped over 2 and 3"
    );
    cmd(&mut e, PlaybackCommand::SeekMs(2_000));
    cmd(&mut e, PlaybackCommand::Previous); // past 1 s: restart
    assert_eq!(status(&mut e).session.current_entry_index, 3);
    cmd(&mut e, PlaybackCommand::Previous); // second press: back over the missing ones
    assert_eq!(status(&mut e).session.current_entry_index, 0);
    // A file that vanishes after the list was built is skipped at play time too.
    fs::remove_file(r.join("Music/Album/04.wav")).unwrap();
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(
        status(&mut e).session.current_entry_index,
        0,
        "nothing playable ahead: stay"
    );
}

fn writebacks(e: &mut Engine) -> Vec<ResumeWriteback> {
    e.take_events()
        .into_iter()
        .filter_map(|ev| match ev {
            EngineEvent::ResumeWriteback(w) => Some(w),
            _ => None,
        })
        .collect()
}

#[test]
fn long_form_stop_requests_resume_writeback_and_load_resumes_there() {
    let (_t, r) = root();
    album(&r, 3, 30.0, "audiobook", "book.noubin.i", "");
    let id = gid(&r, "Album");
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(id));
    cmd(&mut e, PlaybackCommand::Next); // item 2
    cmd(&mut e, PlaybackCommand::SeekMs(5_000));
    wait_until("duration known", || e.current_duration_ms().is_some());
    e.take_events();
    cmd(&mut e, PlaybackCommand::Stop);
    let wb = writebacks(&mut e);
    assert_eq!(
        wb,
        vec![ResumeWriteback {
            playable_id: id,
            resume_at_item: Some(2),
            resume_from_ms: Some(5_000)
        }]
    );
    // Stopping again at the same spot does not repeat the request.
    cmd(&mut e, PlaybackCommand::Stop);
    assert!(writebacks(&mut e).is_empty());

    // Now the server-side state as the server would have written it.
    put(
        &r,
        "Music/Album/_album.noudata",
        &fs::read_to_string(r.join("Music/Album/_album.noudata"))
            .unwrap()
            .replace(
                r#""mediaList""#,
                r#""localPlaybackData":{"resumeAtItem":2},"mediaList""#,
            )
            .replace(
                r#""mediaTitle":"T2","#,
                r#""mediaTitle":"T2","itemLocalPlaybackData":{"resumeFromTime":"0:07"},"#,
            ),
    );
    scan(&r);
    let mut e2 = engine(&r);
    cmd(
        &mut e2,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    let s = status(&mut e2);
    assert_eq!(
        s.session.current_entry_index, 1,
        "resumeAtItem is 1-indexed"
    );
    assert_eq!(e2.current_position_ms(), Some(7_000));
    // Portal Play (LoadAndPlay) still starts from the beginning.
    cmd(&mut e2, PlaybackCommand::LoadAndPlay(gid(&r, "Album")));
    assert_eq!(status(&mut e2).session.current_entry_index, 0);
}

#[test]
fn long_form_completion_clears_resume_and_music_never_writes() {
    let (_t, r) = root();
    album(&r, 2, 0.4, "podcast", "pod.noubin.j", "");
    let mut e = engine(&r);
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    wait_until("podcast finishes", || {
        e.poll_stream_lifecycle();
        e.session().transport == TransportState::Stopped
    });
    let wb = writebacks(&mut e);
    assert_eq!(wb.len(), 1, "{wb:?}");
    assert_eq!((wb[0].resume_at_item, wb[0].resume_from_ms), (None, None));

    let (_t2, r2) = root();
    album(&r2, 2, 30.0, "music", "music.noubin.k", "");
    let mut m = engine(&r2);
    cmd(&mut m, PlaybackCommand::Load(gid(&r2, "Album")));
    cmd(&mut m, PlaybackCommand::SeekMs(5_000));
    cmd(&mut m, PlaybackCommand::Stop);
    assert!(writebacks(&mut m).is_empty(), "music albums do not resume");
}

#[test]
fn never_resume_override_and_short_items_are_not_persisted() {
    let (_t, r) = root();
    album(&r, 2, 30.0, "audiobook", "book.noubin.l", "");
    let p = r.join("Music/Album/_album.noudata");
    fs::write(
        &p,
        fs::read_to_string(&p).unwrap().replace(
            r#""mediaTitle":"T1","#,
            r#""mediaTitle":"T1","itemLocalPlaybackData":{"overrideResumeBehaviour":"neverResume"},"#,
        ),
    )
    .unwrap();
    scan(&r);
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    cmd(&mut e, PlaybackCommand::SeekMs(5_000));
    cmd(&mut e, PlaybackCommand::Stop);
    assert!(writebacks(&mut e).is_empty());
}

#[test]
fn position_and_cues_follow_media_time_when_the_rate_factor_changes() {
    let (_t, r) = root();
    wav(&r.join("m.wav"), 5.0);
    put(
        &r,
        "_m.noudata",
        r#"{"noubinKeyNormalised":"m.k","playableItemTitle":"Rate","playableItemProfile": "mix", "releaseData":{},
            "mediaList":[{"localFilePaths":["m.wav"],"cueLists":[{"cueListType":"nav","cues":[
              {"startTime":"0","cueTextSpans":[{"text":"A"}]},{"startTime":"6","cueTextSpans":[{"text":"B"}]}]}]}]}"#,
    );
    scan(&r);
    let mut e = engine(&r);
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Rate"),
        },
    );
    wait_until("plays to the end", || {
        e.poll_stream_lifecycle();
        e.session().transport == TransportState::Stopped
    });
    // 5 s of output at 1x is 5 s of media (cue A); at 2x the same frames are 10 s (cue B).
    let p1 = e.current_position_ms().unwrap();
    assert!((4_900..=5_100).contains(&p1), "{p1}");
    assert_eq!(status(&mut e).session.current_cue_index, Some(0));
    e.set_playback_rate_factor(2.0);
    let p2 = e.current_position_ms().unwrap();
    assert!((9_800..=10_200).contains(&p2), "{p2}");
    assert_eq!(status(&mut e).session.current_cue_index, Some(1));
}

/// NB_05: cues produced by the timestamp-paste parser (as the server saves them: one `nav`
/// list, no `endTime`s) are navigated by the engine like hand-written ones.
#[test]
fn pasted_timestamps_saved_as_nav_list_are_navigated_by_the_engine() {
    let (_t, r) = root();
    wav(&r.join("Mixes/mix.wav"), 50.0);
    put(
        &r,
        "Mixes/_mix.noudata",
        r#"{"noubinKey":"mix.noubin.k","noubinKeyNormalised":"mix.noubin.k","playableItemTitle":"Mixtape",
            "playableItemProfile": "mix", "releaseData":{},
            "mediaList":[{"localFilePaths":["mix.wav"]}]}"#,
    );
    let path = r.join("Mixes/_mix.noudata");
    let mut doc = plippa_noubin::parse_noudata(&fs::read(&path).unwrap()).unwrap();
    let mut paste = plippa_noubin::parse_cue_paste(
        "Tracklist\n0:00 Intro\n0:10 - Wicked Babylon - Clippers\n0:25 Middle\n0:40 Outro\n1:30 too late\n",
    );
    paste.drop_beyond_length(50_000);
    assert_eq!(paste.cues.len(), 4);
    doc.media_list[0].cue_lists.push(paste.to_cue_list(None));
    doc.write_to_path(&path).unwrap();
    scan(&r);

    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Mixtape")));
    assert_eq!(
        titles(&e),
        [
            "Mixtape",
            "Intro",
            "Wicked Babylon - Clippers",
            "Middle",
            "Outro"
        ]
    );
    for (n, want_ms) in [(1u32, 10_000u64), (2, 25_000), (3, 40_000)] {
        cmd(&mut e, PlaybackCommand::Next);
        assert_eq!(status(&mut e).session.current_cue_index, Some(n));
        assert_eq!(e.current_position_ms(), Some(want_ms));
    }
    // FIX_01C: within 1 s of "Middle" a first press steps back to the previous cue; past 1 s
    // it restarts "Middle".
    cmd(&mut e, PlaybackCommand::SeekMs(26_000));
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(e.current_position_ms(), Some(10_000));
    cmd(&mut e, PlaybackCommand::SeekMs(26_001));
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(e.current_position_ms(), Some(25_000));
    assert_eq!(
        e.stream_prepare_count(),
        1,
        "cue navigation never re-prepares"
    );
}

// ---------------------------------------------------------------------------
// NB_06: per-track files and duplicate keys on tap
// ---------------------------------------------------------------------------

fn tap(e: &mut Engine, key: &str) {
    e.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));
}

#[test]
fn associate_per_track_file_and_album_key_both_tap_resolve() {
    let (_t, r) = root();
    album(&r, 3, 3.0, "music", "album.noubin.e", "");
    // What `associate_track` writes: a per-track file, no releaseData.
    put(
        &r,
        "Music/Album/02.noudata",
        r#"{"noubinKey":"T2.noubin.zz","noubinKeyNormalised":"t2.noubin.zz","playableItemTitle":"T2",
            "referToReleaseNoudata":"_album.noudata",
            "mediaList":[{"mediaTitle":"T2","localFilePaths":["02.wav"]}]}"#,
    );
    scan(&r);
    let mut e = engine(&r);
    tap(&mut e, "T2.noubin.zz");
    assert_eq!(e.session().transport, TransportState::Playing);
    assert_eq!(titles(&e), ["T2"], "the track alone, not the album");
    let evs = e.take_events();
    assert!(
        !evs.iter().any(|ev| matches!(ev, EngineEvent::Notice(_))),
        "{evs:?}"
    );
    tap(&mut e, "album.noubin.e");
    assert_eq!(titles(&e), ["T1", "T2", "T3"]);
}

#[test]
fn tap_with_duplicate_key_plays_first_and_warns() {
    let (_t, r) = root();
    wav(&r.join("a.wav"), 3.0);
    wav(&r.join("b.wav"), 3.0);
    for (f, t) in [("a", "First"), ("b", "Second")] {
        put(
            &r,
            &format!("_{f}.noudata"),
            &format!(
                r#"{{"noubinKey":"dup.k","noubinKeyNormalised":"dup.k","playableItemTitle":"{t}","mediaList":[{{"localFilePaths":["{f}.wav"]}}]}}"#
            ),
        );
    }
    scan(&r);
    let mut e = engine(&r);
    tap(&mut e, "dup.k");
    assert_eq!(e.session().transport, TransportState::Playing);
    assert_eq!(titles(&e), ["First"], "first by .noudata path wins");
    let notices: Vec<_> = e
        .take_events()
        .into_iter()
        .filter_map(|ev| match ev {
            EngineEvent::Notice(n) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].code, plippa_ipc_proto::NOTICE_DUPLICATE_KEY);
    assert_eq!(
        notices[0].severity,
        plippa_ipc_proto::NoticeSeverity::Warning
    );
    assert!(
        notices[0].message.contains("Second"),
        "{}",
        notices[0].message
    );
}

#[test]
fn can_prev_can_next_are_published_in_status() {
    let (_t, r) = root();
    album(&r, 2, 5.0, "music", "album.noubin.cn", "");
    let mut e = engine(&r);
    let flags = |e: &mut Engine| {
        let s = status(e).session;
        (s.can_prev, s.can_next)
    };
    assert_eq!(flags(&mut e), (false, false), "nothing loaded");
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    assert_eq!(flags(&mut e), (true, true), "track 1: prev lit (NB_04b), next lit");
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(flags(&mut e), (true, false), "last track, repeat off");
    // Unload only stops (the list stays loaded), so Previous stays available.
    cmd(&mut e, PlaybackCommand::Unload);
    assert_eq!(flags(&mut e), (true, false));
}

#[test]
fn can_next_follows_the_cue_position() {
    let (_t, r) = root();
    mixtape(&r);
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Mixtape")));
    let flags = |e: &mut Engine| {
        let s = status(e).session;
        (s.can_prev, s.can_next)
    };
    assert_eq!(flags(&mut e), (true, true), "first cue, position 0");
    cmd(&mut e, PlaybackCommand::SeekMs(40_000)); // cue 4 (last)
    assert_eq!(flags(&mut e), (true, false), "no cue or track after the last cue");

    album(
        &r,
        2,
        5.0,
        "music",
        "album.noubin.cr",
        r#""localPlaybackData":{"repeatList":true},"#,
    );
    scan(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(
        flags(&mut e),
        (true, false),
        "Noubin v0.4.1: a file's repeatList no longer wraps Next"
    );
}

/// XCHECK: a tagless single-file mix (a loose *track* with profile `mix`, grouped under
/// Mixtapes) behaves like a mixtape in the engine: cues expand under the one track, the list is
/// announced as a Mix, and a stop asks for a resume write-back (resume defaults follow the
/// profile). A loose *music* track does none of that.
#[test]
fn loose_mix_track_has_cues_mix_list_kind_and_resumes() {
    let (_t, r) = root();
    wav(&r.join("Music/Mixes/set.wav"), 50.0);
    let cues: Vec<String> = (0..5)
        .map(|i| {
            format!(
                r#"{{"startTime":"{}","cueTextSpans":[{{"text":"Part {}"}}]}}"#,
                i * 10,
                i + 1
            )
        })
        .collect();
    put(
        &r,
        "Music/Mixes/set.noudata",
        &format!(
            r#"{{"noudataType":"looseTrack","playableItemTitle":"Loose Set","playableItemProfile":"mix",
               "mediaList":[{{"localFilePaths":["set.wav"],
                 "cueLists":[{{"cueListType":"nav","cues":[{}]}}]}}]}}"#,
            cues.join(",")
        ),
    );
    wav(&r.join("Music/song.wav"), 50.0);
    put(
        &r,
        "Music/song.noudata",
        r#"{"noudataType":"looseTrack","playableItemTitle":"Loose Song","playableItemProfile":"music",
           "mediaList":[{"localFilePaths":["song.wav"]}]}"#,
    );
    scan(&r);
    let id = gid(&r, "Loose Set");
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(id));
    let entries = e.now_playing_entries();
    assert_eq!(entries.len(), 6, "one track + 5 cue entries: {entries:?}");
    assert_eq!(entries[3].title, "Part 3");
    let list = e
        .take_events()
        .into_iter()
        .find_map(|ev| match ev {
            EngineEvent::NowPlaying(l) => Some(l),
            _ => None,
        })
        .expect("list announced");
    assert_eq!(list.list_kind, NowPlayingListKind::Mix);
    cmd(&mut e, PlaybackCommand::SeekMs(25_000));
    wait_until("duration known", || e.current_duration_ms().is_some());
    e.take_events();
    cmd(&mut e, PlaybackCommand::Stop);
    assert_eq!(
        writebacks(&mut e),
        vec![ResumeWriteback {
            playable_id: id,
            resume_at_item: Some(1),
            resume_from_ms: Some(25_000)
        }]
    );

    let mut m = engine(&r);
    cmd(&mut m, PlaybackCommand::Load(gid(&r, "Loose Song")));
    cmd(&mut m, PlaybackCommand::SeekMs(5_000));
    cmd(&mut m, PlaybackCommand::Stop);
    assert!(writebacks(&mut m).is_empty(), "a loose music track does not resume");
}

/// UI_13: a collection load publishes its own title on `SessionState::collection_title`, and
/// the value survives a track change inside the same collection.
#[test]
fn load_collection_sets_collection_title() {
    let (_t, r) = root();
    album(&r, 3, 20.0, "music", "album.noubin.ct", "");
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    assert_eq!(status(&mut e).session.collection_title, "Album");
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(
        status(&mut e).session.collection_title,
        "Album",
        "the collection is the same for every track in it"
    );
}

/// UI_13: a single loose track is its own item, so the collection line stays empty.
#[test]
fn single_track_load_leaves_collection_title_empty() {
    let (_t, r) = root();
    wav(&r.join("Music/solo.wav"), 5.0);
    put(
        &r,
        "Music/solo.noudata",
        r#"{"noudataType":"looseTrack","playableItemTitle":"Solo","playableItemProfile":"music",
           "mediaList":[{"localFilePaths":["solo.wav"]}]}"#,
    );
    scan(&r);
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Solo")));
    assert_eq!(status(&mut e).session.collection_title, "");
}

// ---- PLAY_02: play modes (order / repeat / pause-after-each) ----
//
// The mode is the player setting (session state), never `.noudata` data: every test below
// drives it with `SetOrderMode` / `SetRepeatMode` / `SetPauseAfterEach` / `SetReshuffleOnRepeat`.
// Order changes are list-only — `stream_prepare_count()` proves no stream was touched.

/// Track rows in list order (cue rows excluded, so this is the collection itself).
fn track_titles(e: &Engine) -> Vec<String> {
    e.now_playing_entries()
        .into_iter()
        .filter(|x| x.kind == NowPlayingEntryKind::Track)
        .map(|x| x.title)
        .collect()
}

/// "T1".."Tn" in source (`mediaList`) order.
fn source_album(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("T{i}")).collect()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// A `NOW_PLAYING_EVENT` snapshot of the last list the engine published.
fn last_list(e: &mut Engine) -> Option<plippa_ipc_proto::NowPlayingListChanged> {
    e.take_events()
        .into_iter()
        .rev()
        .find_map(|ev| match ev {
            EngineEvent::NowPlaying(l) => Some(l),
            _ => None,
        })
}

#[test]
fn build_collection_yields_ascending_item_positions() {
    // The source key of every reorder (PLAY_02 §3): the index's `mediaList` position. If it
    // stopped ascending, `restore_source_order`/`index_of_item` would silently pick the wrong
    // track.
    let (_t, r) = root();
    album(&r, 4, 5.0, "music", "album.noubin.pos", "");
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    let positions: Vec<u32> = e
        .now_playing_entries()
        .iter()
        .map(|x| x.item_position)
        .collect();
    assert_eq!(positions, [0, 1, 2, 3], "ascending mediaList positions");
    assert_eq!(track_titles(&e), source_album(4));
}

#[test]
fn set_order_shuffle_mid_album_plays_each_track_once_and_keeps_the_current_one() {
    let (_t, r) = root();
    album(&r, 6, 20.0, "music", "album.noubin.s1", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0x5EED);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    assert_eq!(track_titles(&e), source_album(6), "no mode set: source order");
    cmd(&mut e, PlaybackCommand::Next);
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(e.session().current_entry_index, 2, "playing T3");

    let prepares = e.stream_prepare_count();
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    assert_eq!(
        e.stream_prepare_count(),
        prepares,
        "a reorder is list-only: no prepare, no stream command"
    );
    let shuffled = track_titles(&e);
    assert_eq!(shuffled[0], "T3", "the playing track stays first");
    assert_eq!(
        e.session().current_entry_index,
        0,
        "current follows the track, not the old index"
    );
    assert_eq!(
        sorted(shuffled.clone()),
        source_album(6),
        "each track exactly once"
    );
    assert_ne!(shuffled, source_album(6), "the rest is actually shuffled");

    // `Next` now walks that order, one track at a time, to the end of the list.
    let mut walked = vec![shuffled[0].clone()];
    for _ in 0..5 {
        cmd(&mut e, PlaybackCommand::Next);
        let i = e.session().current_entry_index as usize;
        walked.push(track_titles(&e)[i].clone());
    }
    assert_eq!(walked, shuffled, "every track plays once, in the new order");
}

#[test]
fn set_order_normal_restores_the_album_order_from_the_current_track() {
    let (_t, r) = root();
    album(&r, 6, 20.0, "music", "album.noubin.s2", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0xA11CE);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    for _ in 0..3 {
        cmd(&mut e, PlaybackCommand::Next);
    }
    assert_eq!(e.session().current_entry_index, 3, "playing T4");

    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    assert_eq!(track_titles(&e)[0], "T4");
    assert_eq!(e.session().current_entry_index, 0);

    let prepares = e.stream_prepare_count();
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Normal));
    assert_eq!(e.stream_prepare_count(), prepares, "list-only");
    assert_eq!(track_titles(&e), source_album(6), "mediaList order back");
    // The proof that `current` comes from `item_position`, not the pre-sort index: T4 was at
    // list index 0 and is back at source index 3.
    assert_eq!(
        e.session().current_entry_index,
        3,
        "current is the playing track's source position"
    );
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(track_titles(&e)[e.session().current_entry_index as usize], "T5");
}

#[test]
fn set_order_shuffle_twice_is_a_no_op_then_reshuffles_after_normal() {
    let (_t, r) = root();
    album(&r, 6, 20.0, "music", "album.noubin.s3", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0xC0FFEE);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    let first = track_titles(&e);
    // Repeating the *same* value is a no-op (a UI reconcile must never reshuffle), so a fresh
    // order needs the value to change: the SW2 cycle goes Shuffle → Normal → Shuffle.
    let prepares = e.stream_prepare_count();
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    assert_eq!(track_titles(&e), first, "same value: no reshuffle");
    assert_eq!(e.stream_prepare_count(), prepares, "list-only");
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Normal));
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    let second = track_titles(&e);
    assert_ne!(second, first, "a fresh shuffle each time the mode is turned on");
    assert_eq!(second[0], first[0], "the playing track stays first");
    assert_eq!(sorted(second), source_album(6));
    assert_eq!(e.stream_prepare_count(), prepares, "no order change prepares");
}

#[test]
fn set_order_same_value_is_a_no_op() {
    let (_t, r) = root();
    album(&r, 6, 20.0, "music", "album.noubin.s4", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0x4E5);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    let order = track_titles(&e);
    let rev = e.status_revision();
    e.take_events();

    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    assert_eq!(track_titles(&e), order, "the list is untouched");
    assert_eq!(e.status_revision(), rev, "no status bump");
    assert!(
        e.take_events().is_empty(),
        "no status, no list, no notice for a same-value reconcile"
    );
}

#[test]
fn a_dynamic_queue_reports_no_fixed_order_and_keeps_playing() {
    let (_t, r) = root();
    album(&r, 5, 20.0, "music", "album.noubin.dy", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0xD1CE);
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    assert!(e.mark_list_dynamic(), "the lab hook needs a loaded list");
    assert_eq!(
        status(&mut e).session.source_order,
        SourceOrder::Dynamic,
        "published honestly"
    );

    // Shuffle on a dynamic queue only shuffles the unplayed tail (no fixed order to keep).
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    let queue = track_titles(&e);
    assert_eq!(queue[0], "T1", "the playing track is not moved");
    let prepares = e.stream_prepare_count();
    e.take_events();

    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Normal));
    assert_eq!(
        e.session().order_mode,
        OrderMode::Normal,
        "the setting is stored even though the order cannot change (§0.6)"
    );
    assert_eq!(track_titles(&e), queue, "the queue order is untouched");
    assert_eq!(e.stream_prepare_count(), prepares, "list-only");
    let notices: Vec<_> = e
        .take_events()
        .into_iter()
        .filter_map(|ev| match ev {
            EngineEvent::Notice(n) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "exactly one warning: {notices:?}");
    assert_eq!(notices[0].code, NOTICE_NO_FIXED_ORDER);
    assert_eq!(notices[0].message, "Dynamic playlist, no fixed order");
    assert_eq!(notices[0].severity, NoticeSeverity::Warning);
    // Still playing the same list.
    assert_eq!(status(&mut e).session.transport, TransportState::Playing);
}

#[test]
fn repeat_item_restarts_the_same_track_on_eof() {
    let (_t, r) = root();
    album(&r, 2, 0.3, "music", "album.noubin.ri", "");
    let mut e = engine(&r);
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    // Repeat-item is set *after* the load: a load itself clears it to Off (§0.4).
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Item));
    // The first track ends and is prepared again instead of the second one starting.
    wait_until("the same track restarts", || {
        e.poll_stream_lifecycle();
        e.stream_prepare_count() >= 2
    });
    assert_eq!(
        status(&mut e).session.current_entry_index,
        0,
        "repeat-item never advances"
    );
    assert_eq!(e.session().transport, TransportState::Playing);
    // `Next` still works under repeat-item (the user can move on).
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(status(&mut e).session.current_entry_index, 1);
}

#[test]
fn repeat_queue_starts_a_fresh_shuffled_pass_when_configured() {
    let (_t, r) = root();
    album(&r, 4, 0.3, "music", "album.noubin.rq", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0xB0A7);
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Queue));
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    let pass1 = track_titles(&e);
    assert_eq!(pass1.len(), 4);
    // Four items play, then the wrap re-prepares the first item of the *next* pass.
    wait_until("the queue wrapped", || {
        e.poll_stream_lifecycle();
        e.stream_prepare_count() >= 5 && e.session().current_entry_index == 0
    });
    let pass2 = track_titles(&e);
    assert_ne!(pass2, pass1, "a new order for the new pass");
    assert_ne!(
        pass2[0], pass1[3],
        "never the track that just finished first"
    );
    assert_eq!(sorted(pass2), sorted(pass1), "still every track once");
}

#[test]
fn repeat_queue_replays_the_same_order_when_reshuffle_is_off() {
    let (_t, r) = root();
    album(&r, 4, 0.3, "music", "album.noubin.rq2", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0xB0A7);
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Queue));
    cmd(&mut e, PlaybackCommand::SetReshuffleOnRepeat(false));
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    let pass1 = track_titles(&e);
    wait_until("the queue wrapped", || {
        e.poll_stream_lifecycle();
        e.stream_prepare_count() >= 5 && e.session().current_entry_index == 0
    });
    assert_eq!(
        track_titles(&e),
        pass1,
        "reshuffle off: the identical order every pass"
    );
}

#[test]
fn repeat_item_is_cleared_by_the_next_load() {
    let (_t, r) = root();
    album(&r, 2, 20.0, "music", "album.noubin.cr", "");
    mixtape(&r);
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Item));
    assert_eq!(status(&mut e).session.repeat_mode, RepeatMode::Item);
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Mixtape"),
        },
    );
    assert_eq!(
        status(&mut e).session.repeat_mode,
        RepeatMode::Off,
        "a load clears repeat-item (owner decision 4)"
    );
}

#[test]
fn an_audiobook_never_shuffles() {
    let (_t, r) = root();
    album(&r, 3, 20.0, "audiobook", "album.noubin.ab", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0xBEEF);
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    assert_eq!(
        last_list(&mut e).map(|l| l.list_kind),
        Some(NowPlayingListKind::Audiobook),
        "the fixture really is an audiobook"
    );
    assert_eq!(
        track_titles(&e),
        source_album(3),
        "chapters stay in order with shuffle on"
    );
    let s = status(&mut e).session;
    assert_eq!(
        s.order_mode,
        OrderMode::Shuffle,
        "the setting is kept for the next album"
    );
    assert_eq!(s.source_order, SourceOrder::Fixed);
}

#[test]
fn pause_after_each_holds_the_next_item_ready_to_play() {
    let (_t, r) = root();
    album(&r, 3, 0.3, "music", "album.noubin.sa", "");
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::SetPauseAfterEach(true));
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    wait_until("paused between items", || {
        e.poll_stream_lifecycle();
        e.session().transport == TransportState::Stopped
            && e.session().current_entry_index == 1
    });
    // The next item is current and at 0:00: one Play starts exactly it.
    assert_eq!(
        e.session().position_ms,
        0,
        "the held item is presented at its start"
    );
    assert_eq!(
        e.now_playing_entries()
            .get(1)
            .map(|x| x.title.as_str()),
        Some("T2"),
        "the held item is the second one, not the finished item"
    );
    assert_eq!(
        e.stream_prepare_count(),
        1,
        "the held item is not prepared until Play"
    );
    cmd(&mut e, PlaybackCommand::Play);
    assert_eq!(status(&mut e).session.transport, TransportState::Playing);
    assert_eq!(e.stream_prepare_count(), 2, "Play starts the held item");
}

#[test]
fn pause_after_each_at_the_wrap_queues_the_first_item_of_the_new_pass() {
    let (_t, r) = root();
    album(&r, 3, 0.3, "music", "album.noubin.sw", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0xD00D);
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Queue));
    cmd(&mut e, PlaybackCommand::SetPauseAfterEach(true));
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    let pass1 = track_titles(&e);
    for want in [1usize, 2] {
        wait_until("item stopped at its end", || {
            e.poll_stream_lifecycle();
            e.session().transport == TransportState::Stopped
        });
        assert_eq!(e.session().current_entry_index as usize, want);
        cmd(&mut e, PlaybackCommand::Play);
    }
    // Third item ends: the wrap is resolved *before* the stop, so the queue holds the first
    // item of the fresh pass (PLAY_02 §5.4 step 3), not the old pass's first item.
    wait_until("stopped at the wrap", || {
        e.poll_stream_lifecycle();
        e.session().transport == TransportState::Stopped
    });
    assert_eq!(e.session().current_entry_index, 0);
    let pass2 = track_titles(&e);
    assert_ne!(pass2, pass1, "the queued item belongs to the new pass");
    assert_ne!(pass2[0], pass1[2], "never the track that just finished");
    cmd(&mut e, PlaybackCommand::Play);
    assert_eq!(status(&mut e).session.transport, TransportState::Playing);
}

#[test]
fn next_on_the_last_track_under_repeat_queue_starts_a_new_pass() {
    let (_t, r) = root();
    album(&r, 3, 20.0, "music", "album.noubin.nw", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0x9A9A);
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Queue));
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    let pass1 = track_titles(&e);
    cmd(&mut e, PlaybackCommand::Next);
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(e.session().current_entry_index, 2, "last item of the pass");
    let prepares = e.stream_prepare_count();
    cmd(&mut e, PlaybackCommand::Next);
    let pass2 = track_titles(&e);
    assert_eq!(
        e.session().current_entry_index,
        0,
        "a manual Next at the wrap starts the new pass"
    );
    assert_eq!(e.stream_prepare_count(), prepares + 1, "it plays an item");
    assert_ne!(pass2, pass1, "a fresh order");
    assert_ne!(pass2[0], pass1[2], "never the finished track first");
}

#[test]
fn pause_after_each_and_repeat_item_stop_and_rewind() {
    let (_t, r) = root();
    album(&r, 2, 0.3, "music", "album.noubin.sri", "");
    let mut e = engine(&r);
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    assert_eq!(e.stream_prepare_count(), 1, "the load opened the stream");
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Item));
    cmd(&mut e, PlaybackCommand::SetPauseAfterEach(true));
    // End of the item: stop at 0:00 of *this* item — "stop at the end, and pressing play plays
    // this one again".
    wait_until("stopped at the end of the item", || {
        e.poll_stream_lifecycle();
        e.session().transport == TransportState::Stopped
    });
    assert_eq!(
        e.stream_prepare_count(),
        1,
        "the rewind is a plain seek, not a second open"
    );
    let s = status(&mut e).session;
    assert_eq!(s.current_entry_index, 0, "still the first item");
    assert_eq!(s.position_ms, 0, "rewound to 0:00 for the next Play");
    assert_eq!(s.transport, TransportState::Stopped);
    cmd(&mut e, PlaybackCommand::Play);
    assert_eq!(status(&mut e).session.transport, TransportState::Playing);
    assert_eq!(
        e.stream_prepare_count(),
        2,
        "Play opens the item once, at 0:00"
    );
    // It plays that one again (and stops at its end again) rather than moving on.
    wait_until("stopped at the end of the same item", || {
        e.poll_stream_lifecycle();
        e.session().transport == TransportState::Stopped
    });
    assert_eq!(
        status(&mut e).session.current_entry_index,
        0,
        "repeat-item, not the next item"
    );
}

#[test]
fn list_id_is_stable_across_a_reorder_and_a_track_change() {
    // PLAY_04 keeps the Up Next overlay open by comparing `list_id`: only a *load* may change
    // it, never a reorder or a track change.
    let (_t, r) = root();
    album(&r, 4, 20.0, "music", "album.noubin.lid", "");
    mixtape(&r);
    let mut e = engine(&r);
    e.set_shuffle_seed(7);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    let first = last_list(&mut e).expect("load publishes the list").list_id;
    assert!(first > 0, "0 means 'no list yet'");

    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(
        last_list(&mut e).map(|l| l.list_id),
        Some(first),
        "a track change keeps the list identity"
    );
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    assert_eq!(
        last_list(&mut e).map(|l| l.list_id),
        Some(first),
        "a reshuffle keeps the list identity"
    );
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Normal));
    assert_eq!(
        last_list(&mut e).map(|l| l.list_id),
        Some(first),
        "a restore keeps the list identity"
    );

    // A replacement load — even of the same playable — allocates a new one.
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Mixtape"),
        },
    );
    let second = last_list(&mut e).expect("load publishes the list").list_id;
    assert!(second > first, "a load allocates a new id: {first} -> {second}");
}

/// A second playable in its own folder (`title`), for tests that need two collections.
fn other_album(root: &Path, title: &str, n: usize, key: &str) {
    let dir = format!("Music/{title}");
    let mut items = Vec::new();
    for i in 1..=n {
        wav(&root.join(format!("{dir}/{i:02}.wav")), 20.0);
        items.push(format!(
            r#"{{"mediaTitle":"T{i}","localFilePaths":["{i:02}.wav"]}}"#
        ));
    }
    put(
        root,
        &format!("{dir}/_album.noudata"),
        &format!(
            r#"{{"noubinKey":"{key}","noubinKeyNormalised":"{}","playableItemTitle":"{title}","playableItemProfile":"music",
               "releaseData":{{"credits":{{"primaryArtists":[{{"primaryArtistName":"The Band"}}]}}}},
               "mediaList":[{}]}}"#,
            normalise_noubin_key(key),
            items.join(",")
        ),
    );
    scan(root);
}

/// Three-track mix; track 2 carries five navigable cues.
fn cue_mix(root: &Path) {
    for i in 1..=3 {
        wav(&root.join(format!("Music/Album/{i:02}.wav")), 20.0);
    }
    let cues: Vec<String> = (0..5)
        .map(|i| {
            format!(
                r#"{{"startTime":"{}","cueTextSpans":[{{"text":"P{}"}}]}}"#,
                i * 2,
                i + 1
            )
        })
        .collect();
    let items = [
        r#"{"mediaTitle":"T1","localFilePaths":["01.wav"]}"#.to_string(),
        format!(
            r#"{{"mediaTitle":"T2","localFilePaths":["02.wav"],"cueLists":[{{"cueListType":"nav","cues":[{}]}}]}}"#,
            cues.join(",")
        ),
        r#"{"mediaTitle":"T3","localFilePaths":["03.wav"]}"#.to_string(),
    ];
    put(
        root,
        "Music/Album/_album.noudata",
        &format!(
            r#"{{"noubinKey":"mix.noubin.cues","noubinKeyNormalised":"{}","playableItemTitle":"Album","playableItemProfile":"mix",
               "releaseData":{{}},
               "mediaList":[{}]}}"#,
            normalise_noubin_key("mix.noubin.cues"),
            items.join(",")
        ),
    );
    scan(root);
}

#[test]
fn turning_shuffle_on_mid_playback_does_not_reorder_an_audiobook() {
    let (_t, r) = root();
    album(&r, 4, 20.0, "audiobook", "book.noubin.mid", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0x5EED);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(e.session().current_entry_index, 1);
    let chapters = track_titles(&e);
    let prepares = e.stream_prepare_count();

    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    assert_eq!(
        track_titles(&e),
        chapters,
        "shuffle never reorders an audiobook's chapters mid-playback"
    );
    assert_eq!(
        e.session().current_entry_index,
        1,
        "the playing chapter stays current"
    );
    assert_eq!(e.stream_prepare_count(), prepares, "list-only");
    let s = status(&mut e).session;
    assert_eq!(
        s.order_mode,
        OrderMode::Shuffle,
        "the setting is still stored for the next album"
    );

    // The next (ordinary) album follows the stored setting.
    other_album(&r, "Next", 6, "album.noubin.afterbook");
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Next")));
    assert_ne!(
        track_titles(&e),
        source_album(6),
        "the stored shuffle applies to the next album"
    );
    assert_eq!(status(&mut e).session.order_mode, OrderMode::Shuffle);
}

#[test]
fn an_audiobook_does_not_reshuffle_at_a_repeat_queue_wrap() {
    let (_t, r) = root();
    album(&r, 3, 0.3, "audiobook", "book.noubin.wrap", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0xA11CE);
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Queue));
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Album"),
        },
    );
    let chapters = track_titles(&e);
    assert_eq!(chapters, source_album(3), "loaded in chapter order");
    // Three items play; the queue then wraps back to the first one.
    wait_until("the queue wrapped", || {
        e.poll_stream_lifecycle();
        e.stream_prepare_count() >= 4 && e.session().current_entry_index == 0
    });
    assert_eq!(
        track_titles(&e),
        chapters,
        "the wrap does not reshuffle an audiobook"
    );
    let s = status(&mut e).session;
    assert_eq!(
        s.order_mode,
        OrderMode::Shuffle,
        "the setting is still stored after the wrap"
    );
    assert_eq!(s.repeat_mode, RepeatMode::Queue);

    // The stored setting still applies to the next ordinary album.
    other_album(&r, "After", 6, "album.noubin.afterwrap");
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "After")));
    assert_ne!(
        track_titles(&e),
        source_album(6),
        "the next album follows the stored shuffle"
    );
}

#[test]
fn previous_walks_back_after_a_deshuffle() {
    let (_t, r) = root();
    album(&r, 6, 20.0, "music", "album.noubin.prev", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0xBEE5);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    for _ in 0..3 {
        cmd(&mut e, PlaybackCommand::Next);
    }
    assert_eq!(e.session().current_entry_index, 3);
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Normal));
    assert_eq!(track_titles(&e), source_album(6));
    assert_eq!(e.session().current_entry_index, 3, "T4 is current again");
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(
        e.session().current_entry_index, 2,
        "Previous walks the restored source order"
    );
    assert_eq!(track_titles(&e)[2], "T3");
}

#[test]
fn cues_survive_a_reorder() {
    let (_t, r) = root();
    cue_mix(&r);
    let mut e = engine(&r);
    e.set_shuffle_seed(0xC0DE);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    cmd(&mut e, PlaybackCommand::Next); // T2, the cue-bearing item
    let cue_rows = |e: &mut Engine| {
        e.now_playing_entries()
            .into_iter()
            .filter(|x| x.kind == NowPlayingEntryKind::Cue)
            .collect::<Vec<_>>()
    };
    assert_eq!(cue_rows(&mut e).len(), 5, "the current item's cues");

    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    let entries = e.now_playing_entries();
    assert_eq!(entries[0].title, "T2", "the playing item moved to the front");
    let cues = cue_rows(&mut e);
    assert_eq!(cues.len(), 5, "the playing item's cues survive the reorder");
    assert!(
        cues.iter().all(|c| c.track_index == 0),
        "the cues stay with the current entry"
    );

    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Normal));
    let entries = e.now_playing_entries();
    assert_eq!(entries[1].title, "T2", "restored to its source position");
    let cues = cue_rows(&mut e);
    assert_eq!(cues.len(), 5, "and they survive the restore too");
    assert!(cues.iter().all(|c| c.track_index == 1));
}

#[test]
fn a_second_normal_to_shuffle_still_does_not_prepare() {
    let (_t, r) = root();
    album(&r, 4, 20.0, "music", "album.noubin.prep", "");
    let mut e = engine(&r);
    e.set_shuffle_seed(0xF00D);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Album")));
    let prepares = e.stream_prepare_count();
    for mode in [
        OrderMode::Shuffle,
        OrderMode::Normal,
        OrderMode::Shuffle,
        OrderMode::Normal,
        OrderMode::Shuffle,
    ] {
        cmd(&mut e, PlaybackCommand::SetOrderMode(mode));
        assert_eq!(
            e.stream_prepare_count(),
            prepares,
            "order changes are list-only ({mode:?})"
        );
    }
}

// ---------------------------------------------------------------------------
// PLAY_03: cue-level shuffle (segments, permutation, the boundary poll)
// ---------------------------------------------------------------------------

/// Starts of the published cue rows, in published (play) order.
fn cue_starts(e: &Engine) -> Vec<u64> {
    e.now_playing_entries()
        .into_iter()
        .filter(|x| x.kind == NowPlayingEntryKind::Cue)
        .map(|x| x.start_ms)
        .collect()
}

/// Sink that consumes audio at about real time. `open_mock`'s sink swallows a whole file as
/// fast as the decoder delivers it, so PLAY_03's boundary poll would never see a segment end
/// inside a pass; with one paced write per block the playhead moves at 1x.
struct PacedSink {
    channels: u16,
    rate_hz: u32,
}

impl plippa_audio::AudioSink for PacedSink {
    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate_hz(&self) -> u32 {
        self.rate_hz
    }

    fn write(&mut self, interleaved: &[f32]) -> Result<(), plippa_audio::AudioError> {
        let frames = interleaved.len() / usize::from(self.channels.max(1));
        let ms = (frames as u64 * 1_000) / u64::from(self.rate_hz.max(1));
        thread::sleep(Duration::from_millis(ms.max(1)));
        Ok(())
    }
}

/// Real-time-paced engine (see [`PacedSink`]) for tests that need the playhead to move.
fn engine_paced(root: &Path) -> Engine {
    let mut e = Engine::open_with_sink_factory(root, Capabilities::desktop_mock(), |ch, rate| {
        Ok(Box::new(PacedSink {
            channels: ch,
            rate_hz: rate,
        }) as Box<dyn plippa_audio::AudioSink>)
    })
    .unwrap();
    e.set_clock(Box::new(TestClock {
        ms: Arc::new(AtomicU64::new(0)),
        step_ms: 60_000,
    }));
    e
}

/// Mix titled `title` whose first track is a `secs`-second file with one segment per
/// `nav` start; `credits` adds a second navigable cue list (a cue edit needs two) and
/// `tracks - 1` plain tracks follow it.
fn segment_mix(
    root: &Path,
    key: &str,
    title: &str,
    secs: f32,
    nav: &[u64],
    credits: &[u64],
    tracks: usize,
) {
    let cue_json = |prefix: &str, starts: &[u64]| -> String {
        starts
            .iter()
            .enumerate()
            .map(|(i, s)| {
                // Cue `startTime`s are `SS.mmm` timestamps (seconds, not ms).
                format!(
                    r#"{{"startTime":"{}.{:03}","cueTextSpans":[{{"text":"{prefix}{}"}}]}}"#,
                    s / 1000,
                    s % 1000,
                    i + 1
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    };
    wav(&root.join("Music/Seg/01.wav"), secs);
    let mut lists = vec![format!(
        r#"{{"cueListType":"nav","cues":[{}]}}"#,
        cue_json("S", nav)
    )];
    if !credits.is_empty() {
        lists.push(format!(
            r#"{{"cueListType":"credits","cues":[{}]}}"#,
            cue_json("X", credits)
        ));
    }
    let mut items = vec![format!(
        r#"{{"mediaTitle":"T1","localFilePaths":["01.wav"],"cueLists":[{}]}}"#,
        lists.join(",")
    )];
    for i in 2..=tracks {
        wav(&root.join(format!("Music/Seg/{i:02}.wav")), secs);
        items.push(format!(
            r#"{{"mediaTitle":"T{i}","localFilePaths":["{i:02}.wav"]}}"#
        ));
    }
    put(
        root,
        "Music/Seg/_seg.noudata",
        &format!(
            r#"{{"noubinKey":"{key}","noubinKeyNormalised":"{}","playableItemTitle":"{title}","playableItemProfile":"mix",
               "releaseData":{{}},
               "mediaList":[{}]}}"#,
            normalise_noubin_key(key),
            items.join(",")
        ),
    );
    scan(root);
}

/// Load `title` playing with cue-level shuffle on, from a fixed seed. The cue permutation is
/// `seed + 2 x SHUFFLE_SEED_STEP` (the order change and the load each advance it), so a test
/// that picks a seed knows the play order.
fn load_shuffled_segments(e: &mut Engine, root: &Path, title: &str, seed: u64) {
    e.set_shuffle_seed(seed);
    cmd(e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(
        e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(root, title),
        },
    );
}

#[test]
fn cue_shuffle_plays_every_segment_once_and_advances_at_the_boundary() {
    let (_t, r) = root();
    // Four 400 ms segments. Seed 8 shuffles to [0, 2, 1, 3]: every segment once and the
    // segment that owns EOF (1200) last, so the pass can hear all four.
    segment_mix(&r, "seg.noubin.pass", "Segments", 1.6, &[0, 400, 800, 1_200], &[], 1);
    let mut e = engine_paced(&r);
    load_shuffled_segments(&mut e, &r, "Segments", 8);
    assert_eq!(
        cue_starts(&e),
        [0, 800, 400, 1_200],
        "permutation from seed 8, the heard segment first"
    );
    assert_eq!(e.stream_prepare_count(), 1, "one open stream for the whole pass");

    let mut seen: Vec<u32> = Vec::new();
    let mut at: Vec<u64> = Vec::new();
    wait_until("the permutation played out", || {
        e.poll_stream_lifecycle();
        let s = status(&mut e).session;
        if let Some(i) = s.current_cue_index {
            if seen.last() != Some(&i) {
                seen.push(i);
                at.push(s.position_ms);
            }
        }
        s.transport == TransportState::Stopped
    });
    let order = cue_starts(&e);
    assert_eq!(seen, [0, 1, 2, 3], "one pass along the permutation");
    for (k, pos) in at.iter().enumerate() {
        assert!(
            *pos >= order[k] && *pos < order[k] + 100,
            "row {k} from its own start, got {pos}"
        );
    }
    assert_eq!(
        e.stream_prepare_count(),
        1,
        "segment changes are seeks on the open stream"
    );
    assert_eq!(
        status(&mut e).session.current_entry_index,
        0,
        "a lone file has no next track: the item just ends"
    );
}

#[test]
fn cue_shuffle_repeats_one_segment_when_repeat_item_is_on() {
    let (_t, r) = root();
    segment_mix(&r, "seg.noubin.loop", "Segments", 1.6, &[0, 400, 800], &[], 1);
    let mut e = engine_paced(&r);
    load_shuffled_segments(&mut e, &r, "Segments", 0x5EED); // [0, 1, 2]
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Item));
    let prepares = e.stream_prepare_count();

    // The first segment loops in place: the playhead drops back to its start, twice.
    let mut loops = 0;
    let mut last = 0u64;
    wait_until("the first segment looped twice", || {
        e.poll_stream_lifecycle();
        let p = e.current_position_ms().unwrap_or(0);
        if p + 100 < last {
            loops += 1;
        }
        last = p;
        loops >= 2
    });
    let st = status(&mut e);
    assert_eq!(st.session.current_cue_index, Some(0), "the same segment loops");
    assert_eq!(st.session.current_entry_index, 0);
    assert_eq!(st.session.transport, TransportState::Playing);
    assert_eq!(
        e.stream_prepare_count(),
        prepares,
        "a segment loop is a seek, never a prepare"
    );
}

#[test]
fn cue_shuffle_repeats_the_last_permuted_segment_not_the_next_one() {
    // 10 s file, segments at 0/2/4/6 s, permutation [0, 6000, 4000, 2000]: the last permuted
    // segment ends at 4000 with six seconds of file left. Repeat-item must loop *that* segment
    // (back to 2000), not the chronological neighbour that starts where it ends.
    let (_t, r) = root();
    segment_mix(&r, "seg.noubin.looplast", "Segments", 10.0, &[0, 2_000, 4_000, 6_000], &[], 1);
    let mut e = engine_paced(&r);
    load_shuffled_segments(&mut e, &r, "Segments", 3); // [0, 6000, 4000, 2000]
    assert_eq!(cue_starts(&e), [0, 6_000, 4_000, 2_000]);
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Item));
    let prepares = e.stream_prepare_count();

    let mut reached_end = 0u64;
    wait_until("the last segment looped back to its own start", || {
        e.poll_stream_lifecycle();
        let p = e.current_position_ms().unwrap_or(0);
        if p > 3_700 {
            reached_end = reached_end.max(p);
        }
        reached_end > 0 && p < 2_100
    });
    assert_eq!(
        status(&mut e).session.current_cue_index,
        Some(3),
        "still the last permuted segment"
    );
    assert_eq!(status(&mut e).session.transport, TransportState::Playing);
    assert_eq!(e.stream_prepare_count(), prepares, "a loop is a seek");
}

#[test]
fn cue_shuffle_pauses_at_a_boundary_when_pause_after_each_is_on() {
    let (_t, r) = root();
    segment_mix(&r, "seg.noubin.stop", "Segments", 2.4, &[0, 800, 1_600], &[], 1);
    let mut e = engine_paced(&r);
    e.set_shuffle_seed(0x5EED); // [0, 1, 2]: chronological
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(&mut e, PlaybackCommand::SetPauseAfterEach(true));
    cmd(
        &mut e,
        PlaybackCommand::LoadCollection {
            playable_id: gid(&r, "Segments"),
        },
    );
    let order = cue_starts(&e);
    assert_eq!(order, [0, 800, 1_600]);
    assert_eq!(e.stream_prepare_count(), 1);

    // The first boundary stops *before* the next segment, which becomes current at its start.
    wait_until("stopped at the first boundary", || {
        e.poll_stream_lifecycle();
        e.session().transport == TransportState::Stopped
    });
    let s = status(&mut e).session;
    assert_eq!(s.position_ms, order[1], "presented at the next segment's start");
    assert_eq!(s.current_cue_index, Some(1));
    assert_eq!(e.stream_prepare_count(), 1, "the stop is a seek, not a prepare");

    // One Play opens exactly that segment; its boundary stops on the one after it.
    cmd(&mut e, PlaybackCommand::Play);
    assert_eq!(status(&mut e).session.position_ms, order[1]);
    wait_until("stopped at the second boundary", || {
        e.poll_stream_lifecycle();
        e.session().transport == TransportState::Stopped
    });
    let s = status(&mut e).session;
    assert_eq!(s.position_ms, order[2]);
    assert_eq!(s.current_cue_index, Some(2));
    assert_eq!(e.stream_prepare_count(), 2, "Play opens the held segment once");

    // The last segment runs to EOF: the item is over and playback stops.
    cmd(&mut e, PlaybackCommand::Play);
    wait_until("the item ended", || {
        e.poll_stream_lifecycle();
        e.session().transport == TransportState::Stopped
    });
    assert_eq!(status(&mut e).session.current_entry_index, 0);
    assert_eq!(e.stream_prepare_count(), 3);
}

#[test]
fn a_scan_across_a_cue_boundary_does_not_jump() {
    // PLAY_03: a held FF/RW scan must be able to cross a segment boundary without the
    // boundary poll seeking to the next permuted segment. The playhead stays where the scan
    // put it and the heard segment stays current until the button is released.
    let (_t, r) = root();
    // Four 2 s segments; seed 8 shuffles to [0, 4000, 2000, 6000], so acting on the 2000 ms
    // boundary would jump to 4000 — a visible discontinuity.
    segment_mix(
        &r,
        "seg.noubin.scan",
        "Segments",
        7.0,
        &[0, 2_000, 4_000, 6_000],
        &[],
        1,
    );
    let mut e = engine_paced(&r);
    load_shuffled_segments(&mut e, &r, "Segments", 8);
    assert_eq!(
        cue_starts(&e),
        [0, 4_000, 2_000, 6_000],
        "permutation from seed 8"
    );
    let prepares = e.stream_prepare_count();
    wait_until("the playhead approaches the first boundary", || {
        e.poll_stream_lifecycle();
        e.current_position_ms().unwrap_or(0) >= 1_200
    });
    // Hold a slow FF (10 % of the usual 4x = 0.4x) so the 60 ms lead window is observable.
    cmd(
        &mut e,
        PlaybackCommand::ScanStart {
            dir: ScanDir::Forward,
        },
    );
    cmd(&mut e, PlaybackCommand::ScanRate { permille: 100 });
    let rev = e.status_revision();
    wait_until("the scan crosses the boundary lead window", || {
        e.poll_stream_lifecycle();
        e.current_position_ms().unwrap_or(0) >= 1_940
    });
    let pos = e.current_position_ms().unwrap();
    assert!(
        (1_940..2_000).contains(&pos),
        "inside the heard segment's lead window: {pos}"
    );
    let s = status(&mut e).session;
    // (a) No segment seek: the playhead never jumped to the next permuted segment (4000) and
    // no boundary status was emitted.
    assert_eq!(
        e.status_revision(),
        rev,
        "no segment seek was issued at the boundary"
    );
    // (b) The scan is still a playing hold.
    assert_eq!(s.transport, TransportState::Playing);
    // (c) The heard segment is still the first permutation row.
    assert_eq!(s.current_cue_index, Some(0));
    // (d) The boundary poll never prepares: a segment change is a seek on the open stream.
    assert_eq!(e.stream_prepare_count(), prepares);
    cmd(&mut e, PlaybackCommand::ScanStop);
}

#[test]
fn the_last_permuted_segment_hands_over_without_playing_the_file_on() {
    // Hard requirement: permutation [0, 3, 2, 1] over a 10 s file (segments 0/2000/4000/6000)
    // ends on segment 1, whose tiled end is segment 2's start (4000) — six seconds of audio
    // are still in the file. That boundary is an item end, not a seek: the hand-over goes
    // through `advance_past_current_item`, exactly as EOF does, and the file never plays on.
    let (_t, r) = root();
    segment_mix(&r, "seg.noubin.ho", "Segments", 10.0, &[0, 2_000, 4_000, 6_000], &[], 2);
    let mut e = engine_paced(&r);
    load_shuffled_segments(&mut e, &r, "Segments", 11); // [0, 6000, 4000, 2000]
    assert_eq!(e.stream_prepare_count(), 1);
    let order = cue_starts(&e);
    assert_eq!(order, [0, 6_000, 4_000, 2_000], "mid-file segment last");

    let mut last_seen = 0u64;
    wait_until("handed over to the next track", || {
        e.poll_stream_lifecycle();
        let i = e.session().current_entry_index;
        if i == 0 {
            last_seen = e.current_position_ms().unwrap_or(last_seen);
        }
        i == 1
    });
    assert!(
        last_seen >= 2_000,
        "the last permuted segment was heard ({last_seen})"
    );
    assert!(
        last_seen < 4_500,
        "the boundary at 4000 ended the item; the file did not play on ({last_seen})"
    );
    assert_eq!(
        status(&mut e).session.current_entry_index,
        1,
        "one advance, to the next entry"
    );
    assert_eq!(
        e.stream_prepare_count(),
        2,
        "the advance opens the next item exactly once"
    );
    let pos = e.current_position_ms().unwrap_or(0);
    assert!(pos < 200, "the next item starts at its own beginning: {pos}");
}

#[test]
fn a_cue_edit_mid_playback_does_not_leave_a_stale_permutation() {
    let (_t, r) = root();
    // nav 0/2000/4000 + credits 1000/3000: five segments of 1 s over a 6 s file.
    segment_mix(
        &r,
        "seg.noubin.edit",
        "Segments",
        6.0,
        &[0, 2_000, 4_000],
        &[1_000, 3_000],
        1,
    );
    let mut e = engine_paced(&r);
    load_shuffled_segments(&mut e, &r, "Segments", 0x7); // [0, 1, 2, 3, 4]
    assert_eq!(cue_starts(&e), [0, 1_000, 2_000, 3_000, 4_000]);
    let prepares = e.stream_prepare_count();
    wait_until("into the second segment", || {
        e.poll_stream_lifecycle();
        e.current_position_ms().unwrap_or(0) >= 1_100
    });
    assert_eq!(status(&mut e).session.current_cue_index, Some(1));

    // Disable the credits list: the segments (and the permutation) are rebuilt at once.
    let before = e.current_position_ms().unwrap_or(0);
    cmd(
        &mut e,
        PlaybackCommand::SetCueListEnabled {
            list_index: 1,
            enabled: false,
        },
    );
    let rows = e.now_playing_entries();
    let cues: Vec<u64> = rows
        .iter()
        .filter(|x| x.kind == NowPlayingEntryKind::Cue)
        .map(|x| x.start_ms)
        .collect();
    assert_eq!(cues.len(), 3, "one row per remaining segment: {cues:?}");
    for start in &cues {
        assert!(
            [0, 2_000, 4_000].contains(start),
            "no stale segment row ({start})"
        );
    }
    let st = status(&mut e);
    assert_eq!(
        st.session.current_cue_index,
        Some(0),
        "the heard segment is first in the rebuilt permutation"
    );
    assert!(
        st.session.position_ms >= before && st.session.position_ms < before + 200,
        "a cue edit never moves the playhead ({} -> {})",
        before,
        st.session.position_ms
    );
    assert_eq!(e.stream_prepare_count(), prepares, "a cue edit is list-only");

    // Next walks the rebuilt permutation from the heard segment.
    cmd(&mut e, PlaybackCommand::Next);
    let target = e.current_position_ms().unwrap_or(0);
    assert_eq!(target, cues[1], "Next follows the new permutation: {cues:?}");
    assert_eq!(e.stream_prepare_count(), prepares);
}

#[test]
fn turning_order_normal_mid_mix_returns_to_chronological_cues() {
    let (_t, r) = root();
    segment_mix(&r, "seg.noubin.norm", "Segments", 6.0, &[0, 2_000, 4_000], &[], 1);
    let mut e = engine_paced(&r);
    load_shuffled_segments(&mut e, &r, "Segments", 0x5EED);
    let prepares = e.stream_prepare_count();
    wait_until("into the second segment", || {
        e.poll_stream_lifecycle();
        e.current_position_ms().unwrap_or(0) >= 2_100
    });
    assert_eq!(status(&mut e).session.current_cue_index, Some(1));
    let before = status(&mut e).session.position_ms;

    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Normal));
    let st = status(&mut e);
    assert_eq!(cue_starts(&e), [0, 2_000, 4_000], "chronological rows again");
    assert_eq!(
        st.session.current_cue_index,
        Some(1),
        "the chronological index of the playhead"
    );
    assert!(
        st.session.position_ms >= before && st.session.position_ms < before + 200,
        "the playhead does not move on an order change"
    );
    assert_eq!(e.stream_prepare_count(), prepares, "list-only");
    assert!(
        !e.wants_fast_status(),
        "nothing to poll at 100 ms once the shuffle is off"
    );

    // Playback continues chronologically: no jump back to a segment start.
    thread::sleep(Duration::from_millis(300));
    e.poll_stream_lifecycle();
    let after = e.current_position_ms().unwrap_or(0);
    assert!(
        after >= before,
        "chronological playback never rewinds ({before} -> {after})"
    );
    assert_eq!(e.session().transport, TransportState::Playing);
    assert_eq!(e.session().current_entry_index, 0);
}

#[test]
fn stream_prepare_count_does_not_move_for_a_segment_change() {
    let (_t, r) = root();
    segment_mix(&r, "seg.noubin.seek", "Segments", 1.6, &[0, 400, 800, 1_200], &[], 1);
    let mut e = engine_paced(&r);
    load_shuffled_segments(&mut e, &r, "Segments", 8); // [0, 2, 1, 3]
    assert_eq!(e.stream_prepare_count(), 1);
    wait_until("stream open", || {
        e.poll_stream_lifecycle();
        e.main_stream_open()
    });

    wait_until("into the second segment", || {
        e.poll_stream_lifecycle();
        e.current_position_ms().unwrap_or(0) >= 860
    });
    assert_eq!(status(&mut e).session.current_cue_index, Some(1));
    assert_eq!(e.stream_prepare_count(), 1, "a segment change is a seek");
    assert!(e.main_stream_open(), "the same stream is still open");

    wait_until("into the third segment", || {
        e.poll_stream_lifecycle();
        e.current_position_ms().unwrap_or(0) >= 460
            && status(&mut e).session.current_cue_index == Some(2)
    });
    assert_eq!(e.stream_prepare_count(), 1, "still one stream for the pass");
    assert!(e.main_stream_open());
}


// ---- POLISH_02: the collection line follows the playing mix ----

/// A release with one media file; `cued` adds five navigable cues and profile `mix`.
fn polish02_release(root: &Path, dir: &str, title: &str, cued: bool) {
    wav(&root.join(format!("Music/{dir}/1.wav")), 50.0);
    let (profile, cues) = if cued {
        let cues: Vec<String> = (0..5)
            .map(|i| {
                format!(
                    r#"{{"startTime":"{}","cueTextSpans":[{{"text":"Part {}"}}]}}"#,
                    i * 10,
                    i + 1
                )
            })
            .collect();
        (
            "mix",
            format!(r#","cueLists":[{{"cueListType":"nav","cues":[{}]}}]"#, cues.join(",")),
        )
    } else {
        ("music", String::new())
    };
    put(
        root,
        &format!("Music/{dir}/_rel.noudata"),
        &format!(
            r#"{{"noubinKeyNormalised":"{dir}.k","playableItemTitle":"{title}",
               "playableItemProfile":"{profile}","releaseData":{{}},
               "mediaList":[{{"mediaTitle":"{title}","localFilePaths":["1.wav"]{cues}}}]}}"#
        ),
    );
}

fn polish02_library(root: &Path) {
    polish02_release(root, "P", "Plain Song", false);
    polish02_release(root, "L", "Live \u{202e}In New York", true);
    polish02_release(root, "S", "Second Mix", true);
    put(
        root,
        "Playlists/_Bangers.noudata",
        r#"{"playableItemTitle":"Band Name Bangers",
            "mediaList":[{"localFilePaths":["../Music/P/1.wav"]},
                         {"localFilePaths":["../Music/L/1.wav"]},
                         {"localFilePaths":["../Music/S/1.wav"]}]}"#,
    );
    scan(root);
}

fn polish02_jump_to_track(e: &mut Engine, title: &str) {
    let idx = e
        .now_playing_entries()
        .iter()
        .position(|x| x.kind == NowPlayingEntryKind::Track && x.title.replace('\u{202e}', "") == title)
        .unwrap_or_else(|| panic!("no track {title}"));
    cmd(e, PlaybackCommand::JumpToEntry { index: idx as u32 });
}

fn polish02_line(e: &mut Engine) -> String {
    status(e).session.collection_title
}

const BANGERS: &str = "Band Name Bangers";
const LIVE_LINE: &str = "Band Name Bangers (Live In New York)";

/// The mix title (control/bidi characters stripped) follows the playing item through
/// Next / Previous, jumps, cue navigation, shuffle and repeat wrap.
#[test]
fn collection_line_follows_the_playing_mix() {
    let (_t, r) = root();
    polish02_library(&r);
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, BANGERS)));
    assert_eq!(polish02_line(&mut e), BANGERS, "starts on the plain track");

    polish02_jump_to_track(&mut e, "Live In New York");
    assert_eq!(polish02_line(&mut e), LIVE_LINE, "bidi override is stripped");

    // Cue navigation inside the mix never changes the name.
    cmd(&mut e, PlaybackCommand::Next);
    cmd(&mut e, PlaybackCommand::Next);
    assert_eq!(status(&mut e).session.current_cue_index, Some(2));
    assert_eq!(polish02_line(&mut e), LIVE_LINE);

    polish02_jump_to_track(&mut e, "Second Mix");
    assert_eq!(polish02_line(&mut e), "Band Name Bangers (Second Mix)");
    polish02_jump_to_track(&mut e, "Plain Song");
    assert_eq!(polish02_line(&mut e), BANGERS, "leaving the mix drops the suffix");

    // Next / Previous across track boundaries (end of the mix's last cue steps to the next item).
    polish02_jump_to_track(&mut e, "Live In New York");
    cmd(&mut e, PlaybackCommand::Previous);
    cmd(&mut e, PlaybackCommand::Previous);
    assert_eq!(polish02_line(&mut e), BANGERS);
}

#[test]
fn collection_line_follows_the_item_when_shuffled_and_on_repeat_wrap() {
    let (_t, r) = root();
    polish02_library(&r);
    let mut e = engine(&r);
    e.set_shuffle_seed(0x5EED);
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Shuffle));
    cmd(&mut e, PlaybackCommand::Load(gid(&r, BANGERS)));
    // Whatever the order, the line matches the item that is playing, not the list position.
    for title in ["Second Mix", "Plain Song", "Live In New York", "Second Mix"] {
        polish02_jump_to_track(&mut e, title);
        let want = match title {
            "Plain Song" => BANGERS.to_string(),
            "Live In New York" => LIVE_LINE.to_string(),
            t => format!("{BANGERS} ({t})"),
        };
        assert_eq!(polish02_line(&mut e), want, "shuffled, on {title}");
    }
    // Repeat wrap: from the last item of the list back to the first.
    cmd(&mut e, PlaybackCommand::SetOrderMode(OrderMode::Normal));
    cmd(&mut e, PlaybackCommand::SetRepeatMode(RepeatMode::Queue));
    polish02_jump_to_track(&mut e, "Second Mix");
    assert_eq!(polish02_line(&mut e), "Band Name Bangers (Second Mix)");
    cmd(&mut e, PlaybackCommand::JumpToEntry { index: 0 });
    assert_eq!(polish02_line(&mut e), BANGERS);
}

/// A mix played on its own keeps its own title (kind Mix).
#[test]
fn mix_alone_keeps_its_title_without_a_suffix() {
    let (_t, r) = root();
    polish02_library(&r);
    let mut e = engine(&r);
    cmd(&mut e, PlaybackCommand::Load(gid(&r, "Second Mix")));
    assert_eq!(polish02_line(&mut e), "Second Mix");
}
