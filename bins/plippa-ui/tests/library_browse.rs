//! NB_12 integration: browse the library by category on the (mock) device UI and load
//! collections / jump to entries in a real in-process engine (mock sink, no hardware).
//!
//! Same in-process shape as `integration_harness.rs`: `UiApp` + `plippa_engine::Engine`,
//! `MockDisplay` for frames. No wall-clock assertions: the engine is driven synchronously by
//! `handle_ipc` and events are drained in order.

use plippa_core::{Capabilities, NowPlayingEntryKind, TransportState};
use plippa_engine::{Engine, EngineEvent};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand};
use plippa_noubin::{LibraryRoot, LibraryWriter};
use plippa_ui::{
    AnalysisKind, AnalysisKindStatus, AnalysisPhase, AnalysisState, FrameBuffer, InputEvent,
    LibrarySource, MockDisplay, MockWifi, NoStoreApi, Route, StoreApi, StoreApiError, UiApp,
    UiConfig, UiOutgoing,
};
use std::fs;
use std::path::{Path, PathBuf};

const RATE: u32 = 8_000;

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

/// Library with one album (3 tracks), one playlist (2 of the album's tracks) and one
/// mixtape (a single 50 s track with 5 navigable cues). No audiobooks, no podcasts.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let mut items = Vec::new();
    for i in 1..=3 {
        wav(&root.join(format!("Music/Album/{i:02}.wav")), 2.0);
        items.push(format!(
            r#"{{"mediaTitle":"T{i}","localFilePaths":["{i:02}.wav"]}}"#
        ));
    }
    put(
        &root,
        "Music/Album/_album.noudata",
        &format!(
            r#"{{"noubinKey":"album.noubin.k","noubinKeyNormalised":"album.noubin.k","playableItemTitle":"Album",
               "playableItemProfile": "music", "releaseData":{{"credits":{{"primaryArtists":[{{"primaryArtistName":"The Band"}}]}}}},
               "mediaList":[{}]}}"#,
            items.join(",")
        ),
    );
    put(
        &root,
        "Playlists/_Road trip.noudata",
        r#"{"playableItemTitle":"Road trip","localPlaybackData":{"userNotes":{"createdByUser":"Plippa"}},
           "mediaList":[{"localFilePaths":["../Music/Album/03.wav"]},{"localFilePaths":["../Music/Album/01.wav"]}]}"#,
    );
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
        &root,
        "Mixes/_mix.noudata",
        &format!(
            r#"{{"noubinKey":"mix.noubin.k","noubinKeyNormalised":"mix.noubin.k","playableItemTitle":"Mixtape",
               "playableItemProfile": "mix", "releaseData":{{}},
               "mediaList":[{{"localFilePaths":["mix.wav"],
                 "cueLists":[{{"cueListType":"nav","cues":[{}]}}]}}]}}"#,
            cues.join(",")
        ),
    );
    LibraryWriter::open(&LibraryRoot::new(&root))
        .unwrap()
        .full_scan()
        .unwrap();
    (tmp, root)
}

struct Rig {
    ui: UiApp,
    engine: Engine,
    src: LibrarySource,
    wifi: MockWifi,
    display: MockDisplay,
}

impl Rig {
    fn new(root: &Path) -> Self {
        let mut ui = UiApp::new(UiConfig::default());
        let src = LibrarySource::open(Some(root));
        ui.library_index_changed();
        ui.service_browse(&src);
        Self {
            ui,
            engine: Engine::open_mock(root, Capabilities::desktop_mock()).unwrap(),
            src,
            wifi: MockWifi::default(),
            display: MockDisplay::new(320, 240),
        }
    }

    /// One input, then the loop runner's job: service lookups, flush commands to the engine,
    /// feed the engine's events back (in order), repaint.
    fn press(&mut self, ev: InputEvent) {
        self.ui.handle_input(ev, &mut self.wifi);
        self.settle();
    }

    fn settle(&mut self) {
        for _ in 0..6 {
            self.ui.service_browse(&self.src);
            for out in self.ui.take_outgoing() {
                if let UiOutgoing::Ipc(msg) = out {
                    self.engine.handle_ipc(msg);
                }
            }
            for ev in self.engine.take_events() {
                match ev {
                    EngineEvent::Status(s) => self.ui.handle_ipc(IpcMessage::PlaybackStatus(s)),
                    EngineEvent::Notice(n) => self.ui.handle_ipc(IpcMessage::UserNotice(n)),
                    EngineEvent::NowPlaying(l) => {
                        self.ui.handle_ipc(IpcMessage::NowPlayingListChanged(l))
                    }
                    EngineEvent::AudioRoute(r) => {
                        self.ui.handle_ipc(IpcMessage::AudioRouteChanged(r))
                    }
                    EngineEvent::ResumeWriteback(_) | EngineEvent::UnmatchedNoubin(_) => {}
                }
            }
        }
        self.ui.present(&mut self.display);
    }

    /// Scroll a menu-style list: `n > 0` moves **down**. UI_16 §7 inverted the encoder, so
    /// down is now a left (negative) detent.
    fn rotate(&mut self, n: i8) {
        for _ in 0..n.unsigned_abs() {
            self.press(InputEvent::MenuRotate(-n.signum()));
        }
    }

    fn enter(&mut self) {
        self.press(InputEvent::MenuEnter);
    }

    /// Open the library root from a fresh Now Playing screen.
    fn open_library_root(&mut self) {
        self.ui.nav.force_push(Route::Library);
        self.ui.library_index_changed();
        self.settle();
    }

    /// Root → category `kind_label` → first list item's detail.
    fn open_category_item(&mut self, kind_row: i8, item_row: i8) {
        self.open_library_root();
        self.rotate(kind_row);
        self.enter();
        assert_eq!(*self.ui.nav.current(), Route::LibraryCategory);
        self.rotate(item_row);
        self.enter();
        assert_eq!(*self.ui.nav.current(), Route::LibraryDetail);
    }

    /// Rows that carry ink above the screen ground (the frame is cleared to `BG`, so "not
    /// zero" no longer means "painted"; the bottom-right pixel is always untouched ground).
    fn lit_rows(&self, y0: usize, step: usize, n: usize) -> Vec<bool> {
        let fb: &FrameBuffer = self.display.last.as_ref().unwrap();
        let ground = fb.pixels[fb.pixels.len() - 1];
        (0..n)
            .map(|r| {
                let y = y0 + r * step;
                (y..y + step - 2).any(|yy| {
                    (0..fb.width as usize)
                        .any(|x| fb.pixels[yy * fb.width as usize + x] != ground)
                })
            })
            .collect()
    }
}

fn titles(engine: &Engine) -> Vec<String> {
    engine
        .now_playing_entries()
        .into_iter()
        .map(|e| e.title)
        .collect()
}

#[test]
fn root_shows_all_six_categories_then_stores_and_paints_seven_rows() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    rig.open_library_root();

    use plippa_noubin::LibraryKind::*;
    assert_eq!(
        rig.ui.model.library_root_kinds(),
        vec![Track, Album, Playlist, Podcast, Audiobook, Mix],
        "all six categories, in the order of libraryUIHeirarchy.md"
    );
    // Rows are painted from `CONTENT_TOP` at the `ROW_H` rhythm: the six categories and the
    // BCS_08 Stores row after them, then nothing.
    let lit = rig.lit_rows(35, 18, 8);
    assert_eq!(
        lit,
        vec![true, true, true, true, true, true, true, false]
    );
}

#[test]
fn empty_library_root_still_lists_all_six_categories() {
    let tmp = tempfile::tempdir().unwrap();
    LibraryWriter::open(&LibraryRoot::new(tmp.path()))
        .unwrap()
        .full_scan()
        .unwrap();
    let mut rig = Rig::new(tmp.path());
    rig.open_library_root();
    use plippa_noubin::LibraryKind::*;
    assert_eq!(
        rig.ui.model.library_root_kinds(),
        vec![Track, Album, Playlist, Podcast, Audiobook, Mix]
    );
    // An empty category shows the empty notice, not a crash.
    rig.enter(); // Tracks
    assert_eq!(*rig.ui.nav.current(), Route::LibraryCategory);
    assert!(rig.ui.model.library.is_empty());
}

#[test]
fn album_play_loads_all_tracks_into_the_engine_list() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    // Root order: Tracks(0) Albums(1) Playlists(2) Podcasts(3) Audiobooks(4) Mixes(5).
    rig.open_category_item(1, 0);
    assert_eq!(rig.ui.model.library[0].title, "Album");

    rig.enter(); // Play (focus 0) -> LoadCollection
    assert_eq!(titles(&rig.engine), vec!["T1", "T2", "T3"]);
    assert_eq!(rig.engine.session().transport, TransportState::Playing);
    let list = rig
        .ui
        .model
        .now_playing
        .as_ref()
        .expect("list reached the UI");
    assert_eq!(list.entries.len(), 3);
}

/// Tracks lists every track of the releases (not the release itself) and playing one starts
/// that release at that track.
#[test]
fn tracks_view_lists_individual_tracks_and_plays_from_the_picked_one() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    rig.open_library_root();
    rig.enter(); // Tracks (row 0)
    assert_eq!(*rig.ui.nav.current(), Route::LibraryCategory);
    let rows: Vec<(String, Option<u32>)> = rig
        .ui
        .model
        .library
        .iter()
        .map(|i| (i.title.clone(), i.item_position))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("T1".to_string(), Some(0)),
            ("T2".to_string(), Some(1)),
            ("T3".to_string(), Some(2))
        ],
        "the album's tracks, A-Z; not the album, the playlist or the mix"
    );

    rig.rotate(1); // T2
    rig.enter();
    assert_eq!(*rig.ui.nav.current(), Route::LibraryDetail);
    assert_eq!(rig.ui.model.detail_track_pos, Some(1));
    assert_eq!(
        rig.ui.model.detail_actions(),
        vec![plippa_ui::LibraryDetailAction::Play, plippa_ui::LibraryDetailAction::Associate],
        "a track row has nothing to browse"
    );
    rig.enter(); // Play
    assert_eq!(titles(&rig.engine), vec!["T1", "T2", "T3"], "its release is loaded");
    assert_eq!(rig.engine.session().current_entry_index, 1, "starting at T2");
    assert_eq!(rig.engine.session().transport, TransportState::Playing);
}

#[test]
fn browse_tracks_plays_from_the_picked_entry() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    rig.open_category_item(1, 0);
    // Detail actions for an album: Play, Browse tracks, Associate.
    rig.rotate(1);
    rig.enter();
    assert_eq!(*rig.ui.nav.current(), Route::LibraryTracks);
    rig.rotate(2); // T3
    rig.enter();
    // TRACK_01: a press opens the track's action menu; nothing plays or loads yet.
    assert_eq!(*rig.ui.nav.current(), Route::LibraryTrackActions);
    assert!(titles(&rig.engine).is_empty(), "opening the menu loads nothing");
    assert_eq!(rig.engine.session().transport, TransportState::Stopped);
    rig.enter(); // Play (row 0)
    // Not loaded yet -> LoadCollection, then JumpToEntry for T3's row.
    assert_eq!(titles(&rig.engine), vec!["T1", "T2", "T3"]);
    let cur = rig.engine.session().current_entry_index;
    assert_eq!(cur, 2, "engine moved to T3");
    assert_eq!(rig.engine.session().transport, TransportState::Playing);
    assert_eq!(
        *rig.ui.nav.current(),
        Route::LibraryTrackActions,
        "Play stays on the menu; playing shows in Now Playing"
    );

    // Back leaves the menu for the tracks list with the same row under the cursor.
    rig.press(InputEvent::Back);
    assert_eq!(rig.ui.nav.focus, 2, "T3 is still the highlighted row");

    // Back to the tracks list keeps the highlighted row (selection memory), back again
    // lands on the detail with "Browse tracks" still focused.
    assert_eq!(*rig.ui.nav.current(), Route::LibraryTracks);
    rig.press(InputEvent::Back);
    assert_eq!(*rig.ui.nav.current(), Route::LibraryDetail);
    assert_eq!(rig.ui.nav.focus, 1);
    rig.press(InputEvent::Back);
    assert_eq!(*rig.ui.nav.current(), Route::LibraryCategory);
    rig.press(InputEvent::Back);
    assert_eq!(*rig.ui.nav.current(), Route::Library);
    assert_eq!(rig.ui.nav.focus, 1, "Albums row remembered");
}

#[test]
fn mixtape_chapter_seeks_within_the_one_loaded_track() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    // Mixtapes is root row 3; its only item is the mixtape.
    rig.open_category_item(5, 0); // Mixes
    assert_eq!(rig.ui.model.library[0].title, "Mixtape");
    let info = rig.ui.model.current_detail_info().expect("detail loaded");
    assert_eq!(info.chapters.len(), 5);
    assert_eq!(info.chapters[2].title, "Part 3");
    // Actions: Play, Browse chapters (single track: no Browse tracks), Associate.
    rig.rotate(1);
    rig.enter();
    assert_eq!(*rig.ui.nav.current(), Route::LibraryChapters);
    rig.rotate(2); // Part 3 @ 20 s
    rig.enter();

    // One track loaded once, then a seek to the cue: no re-prepare.
    assert_eq!(rig.engine.stream_prepare_count(), 1);
    let s = rig.engine.session();
    assert_eq!(s.position_ms, 20_000);
    assert_eq!(s.current_cue_index, Some(2));
    let kinds: Vec<_> = rig
        .engine
        .now_playing_entries()
        .iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds[0], NowPlayingEntryKind::Track);
    assert_eq!(kinds.len(), 6);

    // Second chapter pick on the now-loaded mixtape: direct JumpToEntry, still one prepare.
    rig.rotate(-2); // Part 1
    rig.enter();
    assert_eq!(rig.engine.stream_prepare_count(), 1);
    assert_eq!(rig.engine.session().position_ms, 0);
}

#[test]
fn playlist_play_loads_its_entries_in_order() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    rig.open_category_item(2, 0);
    assert_eq!(rig.ui.model.library[0].title, "Road trip");
    rig.enter(); // Play
    assert_eq!(titles(&rig.engine), vec!["T3", "T1"]);
}

#[test]
fn unplugging_the_stick_empties_lists_without_crashing() {
    let (tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    rig.open_category_item(1, 0);
    // Pull the stick, then the index-updated notification arrives.
    drop(tmp);
    assert!(!root.exists());
    rig.ui.library_index_changed();
    rig.settle();
    assert!(!rig.ui.model.storage_present);
    assert!(rig.ui.model.library.is_empty());
    // Every screen still paints and every input is harmless.
    for ev in [
        InputEvent::MenuRotate(1),
        InputEvent::MenuEnter,
        InputEvent::Back,
        InputEvent::Back,
        InputEvent::MenuRotate(-1),
        InputEvent::MenuEnter,
    ] {
        rig.press(ev);
    }
    rig.ui.nav.force_push(Route::Library);
    rig.settle();
    rig.press(InputEvent::MenuEnter);
    rig.press(InputEvent::MenuRotate(1));
    assert!(rig.display.frames > 5);
}

#[test]
fn ui_only_sends_load_and_jump_commands() {
    // The UI never edits the list itself: everything it sends is a plain engine command.
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    rig.open_category_item(1, 0);
    rig.rotate(1);
    rig.enter();
    rig.rotate(1);
    // TRACK_01: the list press opens the track menu; Play is its first row.
    rig.ui.handle_input(InputEvent::MenuEnter, &mut rig.wifi);
    assert_eq!(*rig.ui.nav.current(), Route::LibraryTrackActions);
    assert!(rig.ui.take_outgoing().is_empty(), "opening the menu sends nothing");
    rig.ui.handle_input(InputEvent::MenuEnter, &mut rig.wifi);
    let cmds: Vec<PlaybackCommand> = rig
        .ui
        .take_outgoing()
        .into_iter()
        .filter_map(|o| match o {
            UiOutgoing::Ipc(IpcMessage::PlaybackCommand(c)) => Some(c),
            _ => None,
        })
        .collect();
    assert!(
        matches!(cmds.as_slice(), [PlaybackCommand::LoadCollection { .. }]),
        "first the load, the jump follows once the engine's list arrives: {cmds:?}"
    );
}

// ---- TRACK_01: the per-track action menu ------------------------------------------------

/// Library > Albums > Album > Browse tracks, with the cursor on `row`.
fn tracks_list(rig: &mut Rig, row: i8) {
    rig.open_category_item(1, 0);
    rig.rotate(1);
    rig.enter();
    assert_eq!(*rig.ui.nav.current(), Route::LibraryTracks);
    rig.rotate(row);
}

/// Press without servicing the engine, so the commands the press produced can be inspected.
fn press_and_take(rig: &mut Rig, ev: InputEvent) -> Vec<UiOutgoing> {
    rig.ui.handle_input(ev, &mut rig.wifi);
    rig.ui.take_outgoing()
}

fn playback_commands(out: &[UiOutgoing]) -> usize {
    out.iter()
        .filter(|o| matches!(o, UiOutgoing::Ipc(IpcMessage::PlaybackCommand(_))))
        .count()
}

#[test]
fn pressing_a_track_opens_the_menu_and_sends_no_playback_command() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    tracks_list(&mut rig, 1);
    let out = press_and_take(&mut rig, InputEvent::MenuEnter);
    assert_eq!(*rig.ui.nav.current(), Route::LibraryTrackActions);
    assert_eq!(playback_commands(&out), 0, "{out:?}");
    assert_eq!(rig.ui.model.track_menu_row().map(|t| t.title.as_str()), Some("T2"));
    assert_eq!(rig.ui.nav.focus, 0, "the menu opens on Play");

    // Play sends the commands the list used to send on a press.
    let out = press_and_take(&mut rig, InputEvent::MenuEnter);
    assert!(playback_commands(&out) > 0, "Play must start the track: {out:?}");
}

#[test]
fn back_from_the_menu_returns_to_the_list_on_the_track_it_was_opened_from() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    tracks_list(&mut rig, 2);
    rig.enter();
    rig.rotate(2); // Associate
    rig.press(InputEvent::Back);
    assert_eq!(*rig.ui.nav.current(), Route::LibraryTracks);
    assert_eq!(rig.ui.nav.focus, 2, "T3 is still under the cursor");
    assert_eq!(rig.ui.model.track_menu_pos, None, "the menu forgets its track");
}

#[test]
fn associate_opens_the_associate_flow_for_this_release_and_back_returns_to_the_menu() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    tracks_list(&mut rig, 1);
    rig.enter();
    rig.rotate(2); // Associate
    let out = press_and_take(&mut rig, InputEvent::MenuEnter);
    assert_eq!(*rig.ui.nav.current(), Route::LibraryAssociate);
    let id = rig.ui.model.detail_item().expect("detail item").id;
    // NB_18: the track menu sends the *track* — its position and its title — so the associate
    // writes a `linkTrack` next to that track's media file.
    assert!(
        out.iter().any(|o| matches!(
            o,
            UiOutgoing::AssociateSelected { playable_id, item, title }
                if *playable_id == id && *item == Some(1) && title == "T2"
        )),
        "{out:?}"
    );
    rig.press(InputEvent::Back);
    assert_eq!(*rig.ui.nav.current(), Route::LibraryTrackActions);
    assert_eq!(rig.ui.nav.focus, 2, "back on the Associate row");
}

#[test]
fn cover_art_opens_the_cover_screen_when_there_is_art_and_says_so_when_there_is_none() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    tracks_list(&mut rig, 0);
    rig.enter();
    rig.rotate(1); // Cover art
    assert!(rig.ui.model.current_detail_covers().is_none_or(|c| c.covers.is_empty()));
    rig.enter();
    assert_eq!(
        *rig.ui.nav.current(),
        Route::LibraryTrackActions,
        "no art: a popup, not an empty screen"
    );

    let playable = rig.ui.model.detail_playable_id.expect("detail");
    rig.ui.model.detail_covers = Some(plippa_ui::DetailCovers {
        playable_id: playable,
        track_pos: rig.ui.model.detail_track_pos,
        root_idx: 0,
        covers: vec![plippa_noubin::covers::CoverRef {
            relpath: "Music/Album/cover.jpg".into(),
            source: plippa_noubin::covers::CoverSource::Sibling,
            alt_text: None,
            is_image_of_noubin: false,
        }],
    });
    rig.enter();
    assert_eq!(*rig.ui.nav.current(), Route::LibraryCover);
    rig.press(InputEvent::Back);
    assert_eq!(*rig.ui.nav.current(), Route::LibraryTrackActions);
    assert_eq!(rig.ui.nav.focus, 1, "back on the Cover art row");
}

#[test]
fn a_track_that_left_the_list_returns_to_the_list_on_press_and_on_back() {
    let (_tmp, root) = fixture();
    for leave in [InputEvent::MenuEnter, InputEvent::Back] {
        let mut rig = Rig::new(&root);
        tracks_list(&mut rig, 1);
        rig.enter();
        assert_eq!(*rig.ui.nav.current(), Route::LibraryTrackActions);
        // The stick was swapped between the list render and the press.
        rig.ui.model.detail_info = None;
        assert!(rig.ui.model.track_menu_row().is_none());
        rig.press(InputEvent::Tick { now_ms: 10 });
        assert_eq!(*rig.ui.nav.current(), Route::LibraryTrackActions, "no panic, no jump");
        rig.press(leave.clone());
        assert_eq!(*rig.ui.nav.current(), Route::LibraryTracks, "{leave:?}");
        assert_eq!(rig.ui.model.track_menu_pos, None);
    }
}

/// A store that answers the analysis calls from a script and records them. A call to
/// `waveform_status` first waits for `gate` (when set), so a test can prove the input handler
/// never waits on the network: if it did, the screen would already be answered.
struct AnalysisStore {
    inner: NoStoreApi,
    calls: std::sync::Arc<std::sync::Mutex<AnalysisCalls>>,
    statuses: Vec<Result<Vec<AnalysisKindStatus>, StoreApiError>>,
    reanalyse: Result<(), StoreApiError>,
    gate: Option<std::sync::mpsc::Receiver<()>>,
}

#[derive(Default, Debug)]
struct AnalysisCalls {
    /// `(playable_id, item)` of every status read.
    status: Vec<(u64, u32)>,
    /// `(playable_id, item, kind)` of every re-analysis.
    reanalyse: Vec<(u64, u32, Option<AnalysisKind>)>,
}

fn waveform(state: AnalysisState) -> Vec<AnalysisKindStatus> {
    vec![AnalysisKindStatus { kind: AnalysisKind::Waveform, state, skipped_frames: 0 }]
}

impl StoreApi for AnalysisStore {
    fn waveform_status(
        &mut self,
        playable_id: u64,
        item: u32,
    ) -> Result<Vec<AnalysisKindStatus>, StoreApiError> {
        if let Some(gate) = &self.gate {
            let _ = gate.recv_timeout(std::time::Duration::from_secs(5));
        }
        self.calls.lock().unwrap().status.push((playable_id, item));
        if self.statuses.len() > 1 {
            self.statuses.remove(0)
        } else {
            self.statuses.first().cloned().unwrap_or_else(|| Ok(vec![]))
        }
    }
    fn reanalyse(
        &mut self,
        playable_id: u64,
        item: u32,
        kind: Option<AnalysisKind>,
    ) -> Result<(), StoreApiError> {
        self.calls.lock().unwrap().reanalyse.push((playable_id, item, kind));
        self.reanalyse.clone()
    }
    fn tap(&mut self, u: &str, d: Option<u64>, c: bool) -> Result<plippa_ui::TapAnswer, StoreApiError> {
        self.inner.tap(u, d, c)
    }
    fn want(&mut self, u: &str) -> Result<(), StoreApiError> {
        self.inner.want(u)
    }
    fn download_prepare(&mut self, k: &str, r: &str) -> Result<plippa_ui::DownloadConsent, StoreApiError> {
        self.inner.download_prepare(k, r)
    }
    fn download_start(&mut self, c: &str) -> Result<plippa_ui::DownloadSession, StoreApiError> {
        self.inner.download_start(c)
    }
    fn download_status(&mut self, s: &str) -> Result<plippa_ui::DownloadProgress, StoreApiError> {
        self.inner.download_status(s)
    }
    fn download_cancel(&mut self, s: &str) -> Result<(), StoreApiError> {
        self.inner.download_cancel(s)
    }
    fn portal_address(&mut self) -> Result<plippa_ui::PortalAddress, StoreApiError> {
        self.inner.portal_address()
    }
    fn stores(&mut self) -> Result<plippa_ui::StoresInfo, StoreApiError> {
        self.inner.stores()
    }
    fn collection(&mut self, q: &str, o: u32, n: u32) -> Result<plippa_ui::CollectionPageInfo, StoreApiError> {
        self.inner.collection(q, o, n)
    }
    fn release(&mut self, r: &str) -> Result<plippa_ui::ReleaseInfo, StoreApiError> {
        self.inner.release(r)
    }
    fn delete_preview(&mut self, p: u64) -> Result<plippa_ui::DeletePreviewInfo, StoreApiError> {
        self.inner.delete_preview(p)
    }
    fn delete_playable(&mut self, p: u64) -> Result<plippa_ui::DeleteDone, StoreApiError> {
        self.inner.delete_playable(p)
    }
    fn sync_start(&mut self) -> Result<plippa_ui::SyncStatus, StoreApiError> {
        self.inner.sync_start()
    }
    fn sync_status(&mut self) -> Result<plippa_ui::SyncStatus, StoreApiError> {
        self.inner.sync_status()
    }
    fn sync_cancel(&mut self) -> Result<plippa_ui::SyncStatus, StoreApiError> {
        self.inner.sync_cancel()
    }
    fn release_cover(&mut self, r: &str) -> Result<plippa_ui::CoverBytes, StoreApiError> {
        self.inner.release_cover(r)
    }
    fn portal_settings(&mut self) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        self.inner.portal_settings()
    }
    fn set_portal_enabled(&mut self, e: bool) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        self.inner.set_portal_enabled(e)
    }
    fn set_https_enabled(&mut self, on: bool) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        self.inner.set_https_enabled(on)
    }
    fn regenerate_certificate(&mut self) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        self.inner.regenerate_certificate()
    }
    fn set_pin_required(&mut self, r: bool) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        self.inner.set_pin_required(r)
    }
    fn set_pin(&mut self, p: &str) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        self.inner.set_pin(p)
    }
    fn clear_lockout(&mut self) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        self.inner.clear_lockout()
    }
}

fn analysis_store(
    statuses: Vec<Result<Vec<AnalysisKindStatus>, StoreApiError>>,
    reanalyse: Result<(), StoreApiError>,
    gate: Option<std::sync::mpsc::Receiver<()>>,
) -> (AnalysisStore, std::sync::Arc<std::sync::Mutex<AnalysisCalls>>) {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(AnalysisCalls::default()));
    let store = AnalysisStore {
        inner: NoStoreApi,
        calls: calls.clone(),
        statuses,
        reanalyse,
        gate,
    };
    (store, calls)
}

/// Tick the app (simulated clock, real worker thread) until `done`, bounded.
fn tick_until(rig: &mut Rig, clock: &mut u64, step_ms: u64, done: impl Fn(&Rig) -> bool) {
    for _ in 0..3_000 {
        if done(rig) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        *clock += step_ms;
        rig.press(InputEvent::Tick { now_ms: *clock });
    }
    panic!("condition not reached");
}

fn open_analysis(rig: &mut Rig) {
    tracks_list(rig, 1); // T2, mediaList position 1
    rig.enter();
    rig.rotate(3); // Analysis
    rig.enter();
    assert_eq!(*rig.ui.nav.current(), Route::LibraryTrackAnalysis);
}

#[test]
fn the_analysis_screen_reads_the_status_off_the_input_thread_and_names_this_track() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    let (gate_tx, gate_rx) = std::sync::mpsc::channel();
    let (store, calls) =
        analysis_store(vec![Ok(waveform(AnalysisState::Analysed))], Ok(()), Some(gate_rx));
    rig.ui.set_store_api(Box::new(store));
    open_analysis(&mut rig);

    // The status call is held at the gate: had the press called it on the input thread, the
    // screen would be answered (or the test stuck) here instead of still loading.
    let view = rig.ui.model.track_analysis.as_ref().expect("view");
    assert_eq!(view.phase, AnalysisPhase::Loading);
    assert_eq!(view.item, 1, "T2 is mediaList position 1");
    let playable_id = view.playable_id;

    gate_tx.send(()).unwrap();
    let mut clock = 0;
    tick_until(&mut rig, &mut clock, 16, |r| {
        r.ui.model.track_analysis.as_ref().is_some_and(|v| v.phase == AnalysisPhase::Ready)
    });
    assert_eq!(calls.lock().unwrap().status, vec![(playable_id, 1)]);
    let view = rig.ui.model.track_analysis.as_ref().unwrap();
    assert_eq!(view.waveform_state(), Some(AnalysisState::Analysed));
    let rows = view.rows();
    assert_eq!(rows[1].value, "Not available yet");
    assert!(!rows[1].selectable && !rows[2].selectable);

    // Analysed: nothing is moving, so the screen stops asking.
    for _ in 0..40 {
        clock += 500;
        rig.press(InputEvent::Tick { now_ms: clock });
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(calls.lock().unwrap().status.len(), 1, "no polling once analysed");
}

#[test]
fn re_analyse_queues_the_track_polls_until_done_and_stops_when_the_screen_is_left() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    let (store, calls) = analysis_store(
        vec![
            Ok(waveform(AnalysisState::Failed)),
            Ok(waveform(AnalysisState::Analysing)),
            Ok(waveform(AnalysisState::Analysing)),
            Ok(waveform(AnalysisState::Analysed)),
        ],
        Ok(()),
        None,
    );
    rig.ui.set_store_api(Box::new(store));
    open_analysis(&mut rig);
    let mut clock = 0;
    tick_until(&mut rig, &mut clock, 16, |r| {
        r.ui.model.track_analysis.as_ref().is_some_and(|v| v.phase == AnalysisPhase::Ready)
    });
    assert_eq!(rig.ui.model.track_analysis.as_ref().unwrap().waveform_state(), Some(AnalysisState::Failed));
    assert_eq!(calls.lock().unwrap().status.len(), 1, "Failed is not polled");

    rig.rotate(1); // Re-analyse (Waveform is row 0, BPM / Downbeat are skipped)
    rig.enter();
    tick_until(&mut rig, &mut clock, 16, |_| !calls.lock().unwrap().reanalyse.is_empty());
    let playable_id = rig.ui.model.track_analysis.as_ref().unwrap().playable_id;
    assert_eq!(calls.lock().unwrap().reanalyse, vec![(playable_id, 1, None)], "every kind");
    tick_until(&mut rig, &mut clock, 16, |r| {
        r.ui.model.track_analysis.as_ref().and_then(|v| v.waveform_state())
            == Some(AnalysisState::Queued)
    });

    // The poll then walks Analysing, Analysing, Analysed and stops there.
    tick_until(&mut rig, &mut clock, 100, |r| {
        r.ui.model.track_analysis.as_ref().and_then(|v| v.waveform_state())
            == Some(AnalysisState::Analysed)
    });
    let settled = calls.lock().unwrap().status.len();
    assert_eq!(settled, 4, "status reads: first, then three polls");
    for _ in 0..40 {
        clock += 500;
        rig.press(InputEvent::Tick { now_ms: clock });
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(calls.lock().unwrap().status.len(), settled);
}

#[test]
fn leaving_the_analysis_screen_stops_the_polling() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    let (store, calls) = analysis_store(vec![Ok(waveform(AnalysisState::Queued))], Ok(()), None);
    rig.ui.set_store_api(Box::new(store));
    open_analysis(&mut rig);
    let mut clock = 0;
    // Queued: it keeps polling while the screen is up.
    tick_until(&mut rig, &mut clock, 200, |_| calls.lock().unwrap().status.len() >= 3);

    rig.press(InputEvent::Back);
    assert_eq!(*rig.ui.nav.current(), Route::LibraryTrackActions);
    assert!(rig.ui.model.track_analysis.is_none());
    std::thread::sleep(std::time::Duration::from_millis(50));
    let after_leave = calls.lock().unwrap().status.len();
    for _ in 0..40 {
        clock += 500;
        rig.press(InputEvent::Tick { now_ms: clock });
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(calls.lock().unwrap().status.len(), after_leave, "no polling after Back");
}

#[test]
fn a_refused_re_analysis_of_a_missing_file_says_missing_not_queued() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    let refusal = StoreApiError::Refused {
        status: 409,
        code: Some("unavailable".into()),
        message: "the media file is missing".into(),
    };
    let (store, calls) =
        analysis_store(vec![Ok(waveform(AnalysisState::Analysed))], Err(refusal), None);
    rig.ui.set_store_api(Box::new(store));
    open_analysis(&mut rig);
    let mut clock = 0;
    tick_until(&mut rig, &mut clock, 16, |r| {
        r.ui.model.track_analysis.as_ref().is_some_and(|v| v.phase == AnalysisPhase::Ready)
    });
    rig.rotate(1);
    rig.enter();
    tick_until(&mut rig, &mut clock, 16, |r| {
        r.ui.model.track_analysis.as_ref().and_then(|v| v.waveform_state())
            == Some(AnalysisState::Unavailable)
    });
    assert_eq!(calls.lock().unwrap().reanalyse.len(), 1);
    // Pressing again does not ask the server again: the file is known to be gone.
    rig.enter();
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(calls.lock().unwrap().reanalyse.len(), 1);
}

#[test]
fn an_unreachable_server_shows_try_again_and_pressing_it_asks_again() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    let (store, calls) = analysis_store(
        vec![
            Err(StoreApiError::Offline("no route".into())),
            Ok(waveform(AnalysisState::Analysed)),
        ],
        Ok(()),
        None,
    );
    rig.ui.set_store_api(Box::new(store));
    open_analysis(&mut rig);
    let mut clock = 0;
    tick_until(&mut rig, &mut clock, 16, |r| {
        r.ui.model.track_analysis.as_ref().is_some_and(|v| matches!(v.phase, AnalysisPhase::Failed(_)))
    });
    rig.enter(); // Try again is the only reachable row
    tick_until(&mut rig, &mut clock, 16, |r| {
        r.ui.model.track_analysis.as_ref().is_some_and(|v| v.phase == AnalysisPhase::Ready)
    });
    assert_eq!(calls.lock().unwrap().status.len(), 2);
}
