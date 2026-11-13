//! NB_15 end-to-end (mock engine + fixture library, no hardware): load a mixtape with cues
//! and a playlist from the device UI, navigate with Next / Previous / knob jump, watch the
//! cue highlight follow the *media position*, and check the Now Playing list panel.
//!
//! Same in-process shape as `integration_harness.rs`: `UiApp` + `plippa_engine::Engine`,
//! `MockDisplay` for frames. No wall-clock assertions: the engine is driven synchronously by
//! `handle_ipc` and events are drained in order.

use plippa_audio::{AudioError, AudioSink};
use plippa_board::ControlRole;
use plippa_core::Capabilities;
use plippa_engine::{Engine, EngineEvent};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand};
use plippa_noubin::{LibraryRoot, LibraryWriter};
use plippa_ui::ui::tokens::{ACCENT, ACCENT_2, BG, BG_ALT, BORDER_SOFT, SURFACE, SURFACE_ALT};
use plippa_ui::{
    FrameBuffer, InputEvent, LibrarySource, MockDisplay, MockWifi, NpMode, NpTarget, Route, UiApp,
    UiConfig, UiOutgoing, UP_NEXT_ROWS, UP_NEXT_ROWS_Y, UP_NEXT_TOP,
};
use std::fs;
use std::path::{Path, PathBuf};

const RATE: u32 = 8_000;

/// Grounds and row fills: painted by the widgets but carrying no information.
fn is_background(p: &u16) -> bool {
    [BG, BG_ALT, SURFACE, SURFACE_ALT, BORDER_SOFT].contains(p)
}

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
    // A one-entry playlist: loads as a single track with no cues.
    put(
        &root,
        "Playlists/_Solo list.noudata",
        r#"{"playableItemTitle":"Solo list","localPlaybackData":{"userNotes":{"createdByUser":"Plippa"}},
           "mediaList":[{"localFilePaths":["../Music/Album/02.wav"]}]}"#,
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

/// The stock `Engine::open_mock` sink swallows audio at CPU speed, so the playhead races. This
/// test presses Previous at the *start* of an entry and checks where a jump landed, so pace the
/// sink at real time: the playhead then advances like it does on the device.
struct PacedSink {
    channels: u16,
    sample_rate_hz: u32,
}

impl AudioSink for PacedSink {
    fn channels(&self) -> u16 {
        self.channels
    }
    fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }
    fn write(&mut self, interleaved: &[f32]) -> Result<(), AudioError> {
        let frames = interleaved.len() / self.channels.max(1) as usize;
        std::thread::sleep(std::time::Duration::from_micros(
            frames as u64 * 1_000_000 / u64::from(self.sample_rate_hz),
        ));
        Ok(())
    }
}

impl Rig {
    fn new(root: &Path) -> Self {
        let mut ui = UiApp::new(UiConfig::default());
        let src = LibrarySource::open(Some(root));
        ui.library_index_changed();
        ui.service_browse(&src);
        Self {
            ui,
            engine: Engine::open_with_sink_factory(
                root,
                Capabilities::desktop_mock(),
                |channels, sample_rate_hz| {
                    Ok(Box::new(PacedSink {
                        channels,
                        sample_rate_hz,
                    }) as Box<dyn AudioSink>)
                },
            )
            .unwrap(),
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

    fn rotate(&mut self, n: i8) {
        for _ in 0..n.unsigned_abs() {
            self.press(InputEvent::MenuRotate(n.signum()));
        }
    }

    /// Scroll a **menu-style** list (library root / category): `n > 0` moves down. UI_16 §7
    /// inverted the encoder for menus and the library, so down is a left (negative) detent.
    /// The Now Playing overlay cursor keeps the natural direction and uses [`Self::rotate`].
    fn menu_rotate(&mut self, n: i8) {
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
        self.menu_rotate(kind_row);
        self.enter();
        assert_eq!(*self.ui.nav.current(), Route::LibraryCategory);
        self.menu_rotate(item_row);
        self.enter();
        assert_eq!(*self.ui.nav.current(), Route::LibraryDetail);
    }
}

impl Rig {
    /// Back out to Now Playing with the menu overlay closed.
    fn go_now_playing(&mut self) {
        for _ in 0..8 {
            if *self.ui.nav.current() == Route::Main {
                break;
            }
            self.press(InputEvent::Back);
        }
        if self.ui.compositor.main_menu_visible() {
            self.press(InputEvent::Back);
        }
        assert_eq!(*self.ui.nav.current(), Route::Main);
        assert!(!self.ui.compositor.main_menu_visible());
    }

    /// Engine status reaches the UI (what the hub's periodic refresh does on the device).
    fn refresh(&mut self) {
        self.engine.publish_status();
        self.settle();
    }

    /// Poll the engine (real-time mock sink, bounded) until `f` holds; never asserts on timing.
    fn wait(&mut self, what: &str, f: impl Fn(&Rig) -> bool) {
        let t = std::time::Instant::now();
        loop {
            self.engine.poll_stream_lifecycle();
            self.refresh();
            if f(self) {
                return;
            }
            assert!(
                t.elapsed() < std::time::Duration::from_secs(15),
                "timed out: {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Foreground ink from the Up Next overlay's first row: grounds and row fills are painted
    /// by design, so only text / accent / marker pixels count as "lit".
    fn overlay_lit(&self) -> usize {
        let fb: &FrameBuffer = self.display.last.as_ref().unwrap();
        let start = UP_NEXT_ROWS_Y as usize * fb.width as usize;
        let end = (UP_NEXT_ROWS_Y + UP_NEXT_ROWS * plippa_ui::ui::tokens::ROW_H) as usize
            * fb.width as usize;
        fb.pixels[start..end]
            .iter()
            .filter(|&&p| !is_background(&p))
            .count()
    }

    /// `ACCENT` pixels painted on one overlay row (the focus ring; the marker is `ACCENT_2`).
    fn row_accent(&self, row: u16) -> usize {
        let fb: &FrameBuffer = self.display.last.as_ref().unwrap();
        let y0 = (UP_NEXT_ROWS_Y + row * plippa_ui::ui::tokens::ROW_H) as usize * fb.width as usize;
        let y1 = y0 + plippa_ui::ui::tokens::ROW_H as usize * fb.width as usize;
        fb.pixels[y0..y1].iter().filter(|&&p| p == ACCENT).count()
    }

    /// `ACCENT` pixels inside the overlay rows (the focus ring; the marker is `ACCENT_2`).
    fn overlay_accent(&self) -> usize {
        let fb: &FrameBuffer = self.display.last.as_ref().unwrap();
        let start = UP_NEXT_ROWS_Y as usize * fb.width as usize;
        let end = (UP_NEXT_ROWS_Y + UP_NEXT_ROWS * plippa_ui::ui::tokens::ROW_H) as usize
            * fb.width as usize;
        fb.pixels[start..end]
            .iter()
            .filter(|&&p| p == ACCENT)
            .count()
    }

    fn cmd(&mut self, c: PlaybackCommand) {
        self.engine.handle_ipc(IpcMessage::PlaybackCommand(c));
        self.settle();
    }
}

#[test]
fn mixtape_with_cues_loads_navigates_and_highlights_from_media_position() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    rig.open_category_item(5, 0); // Mixes -> Mixtape
    rig.enter(); // Play = LoadCollection
    rig.go_now_playing();
    rig.refresh();

    // The mock sink plays in real time, so a slow host can already be past cue 2 by now;
    // pin the position to 0:00 (the engine's "next" rule is "next after now").
    rig.cmd(PlaybackCommand::SeekMs(0));
    rig.refresh();
    assert_eq!(rig.engine.session().current_cue_index, Some(0));

    // Track row + 5 cue rows; the list exists but the waveform owns the screen.
    let list = rig.ui.model.now_playing.clone().expect("list");
    assert_eq!(list.entries.len(), 6);
    assert!(rig.ui.model.list_panel_visible());
    assert_eq!(rig.ui.model.now_playing_row(), Some(1));
    assert!(
        !rig.ui.model.up_next_visible(),
        "the waveform fills the screen until the list target is selected"
    );

    // Next (hardware button role) moves to cue 2 by seeking inside the one loaded track.
    rig.press(InputEvent::ControlPressed(ControlRole::NextTrack));
    rig.refresh();
    assert_eq!(rig.engine.session().current_cue_index, Some(1));
    assert_eq!(rig.ui.model.now_playing_row(), Some(2));
    assert_eq!(
        rig.engine.stream_prepare_count(),
        1,
        "cue navigation never reloads the track"
    );

    // FIX_01C: at the cue start a first Previous steps back one cue (it restarts only once the
    // cue has played for >1 s), still without reloading the track. The paced sink keeps the
    // playhead at the cue start for the press, so this is not a timing accident.
    rig.press(InputEvent::ControlPressed(ControlRole::PrevTrack));
    rig.refresh();
    assert_eq!(rig.engine.session().current_cue_index, Some(0));
    assert_eq!(rig.ui.model.now_playing_row(), Some(1));
    assert_eq!(rig.engine.stream_prepare_count(), 1);

    // UI_13: the hardware Prev press auto-opened the overlay; Back dismisses it so the knob
    // starts from the plain waveform again.
    rig.press(InputEvent::Back);
    assert_eq!(rig.ui.model.np_focus.target, NpTarget::CurrentTime);
    // Rotate right opens the overlay on the playing row (1) and moves the cursor to 2.
    rig.rotate(1);
    assert_eq!(rig.ui.model.np_focus.target, NpTarget::List);
    assert_eq!(rig.ui.model.np_focus.mode, NpMode::List);
    assert_eq!(rig.ui.model.np_focus.cursor, 2);
    assert!(rig.ui.model.up_next_visible());
    let lit = rig.overlay_lit();
    assert!(lit > 200, "overlay painted: {lit}");
    // The cursor (playing row + one) carries the 2 px ring; the playing row carries the marker.
    assert!(rig.overlay_accent() > 1_000, "cursor row paints its ring");
    let fb = rig.display.last.as_ref().unwrap();
    assert!(
        fb.pixels[UP_NEXT_ROWS_Y as usize * fb.width as usize..]
            .iter()
            .any(|p| *p == ACCENT_2),
        "the playing row carries its ACCENT_2 marker"
    );

    // Two more turns reach cue 3 (row 4); six rows are visible (UI_14), so the window still
    // starts at row 0 and the cursor is the fifth visible row: only that row carries the ring.
    rig.rotate(2);
    assert_eq!(rig.ui.model.np_focus.cursor, 4);
    assert!(rig.row_accent(4) > 1_000, "cursor row paints its 2 px ring");
    assert!(rig.row_accent(2) < 100, "only the cursor row carries the ring");
    // Time the jump so the position can be checked against the cue start; the paced sink only
    // advances with wall time.
    let jumped_at = std::time::Instant::now();
    rig.enter();
    rig.refresh();
    assert_eq!(
        rig.ui.model.np_focus.mode,
        NpMode::Select,
        "a push jumps and closes the overlay"
    );
    assert!(!rig.ui.model.up_next_visible());
    assert_eq!(rig.engine.session().current_cue_index, Some(3));
    assert_eq!(rig.ui.model.now_playing_row(), Some(4));
    assert_eq!(rig.engine.stream_prepare_count(), 1);
    let pos = rig.engine.session().position_ms;
    // It landed at the 30 s cue start, not somewhere in the fourth cue's 10 s span: the only
    // advance allowed is the wall time since the jump (plus a sink-start margin).
    let elapsed_ms = jumped_at.elapsed().as_millis() as u64;
    assert!(pos >= 30_000, "{pos}");
    assert!(
        pos < 30_000 + elapsed_ms + 750,
        "{pos} after {elapsed_ms} ms: the jump did not land at the cue start"
    );

    // The highlight advances on its own as *playback* crosses the next cue boundary.
    rig.cmd(PlaybackCommand::SeekMs(39_800));
    rig.wait("playback crosses cue 5", |r| {
        r.engine.session().current_cue_index == Some(4)
    });
    assert_eq!(rig.ui.model.now_playing_row(), Some(5));
    assert_eq!(rig.engine.stream_prepare_count(), 1);

    // The jump closed the overlay; Back opens the menu directly.
    rig.press(InputEvent::Back);
    assert_eq!(rig.ui.model.np_focus.mode, NpMode::Select);
    assert_eq!(rig.ui.model.np_focus.target, NpTarget::CurrentTime);
    assert!(!rig.ui.model.up_next_visible());
    assert!(rig.ui.compositor.main_menu_visible());
}

#[test]
fn playlist_loads_and_navigates_with_next_previous_and_jump() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    rig.open_category_item(2, 0); // Playlists -> Road trip (T3, T1)
    rig.enter();
    rig.go_now_playing();
    rig.refresh();
    let titles: Vec<String> = rig
        .ui
        .model
        .now_playing
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .map(|e| e.title.clone())
        .collect();
    assert_eq!(titles, vec!["T3", "T1"]);
    assert!(rig.ui.model.list_panel_visible());
    assert_eq!(rig.ui.model.now_playing_row(), Some(0));

    rig.press(InputEvent::ControlPressed(ControlRole::NextTrack));
    rig.refresh();
    assert_eq!(rig.engine.session().current_entry_index, 1);
    assert_eq!(rig.ui.model.now_playing_row(), Some(1));
    // Last entry: Next is unavailable, the press does nothing.
    assert!(!rig.ui.nav_availability().can_next);

    // Knob jump back to row 0: a left turn opens the overlay on the playing row (1, clamped
    // toward 0) and a push jumps and closes it.
    rig.rotate(-1);
    assert_eq!(rig.ui.model.np_focus.mode, NpMode::List);
    assert_eq!(rig.ui.model.np_focus.cursor, 0);
    rig.enter();
    rig.refresh();
    assert_eq!(rig.engine.session().current_entry_index, 0);
    assert_eq!(rig.ui.model.np_focus.mode, NpMode::Select);
    assert_eq!(rig.ui.model.now_playing_row(), Some(0));

    // Loading another collection replaces the list; the cursor resets to its playing row.
    rig.ui.nav.force_push(Route::Library);
    rig.settle();
    assert_eq!(rig.engine.now_playing_entries().len(), 2);
}

#[test]
fn single_track_load_keeps_the_plain_now_playing_layout_and_scrub() {
    let (_tmp, root) = fixture();
    let mut rig = Rig::new(&root);
    // Playlists (row 2) -> second playlist: one entry, no cues.
    rig.open_category_item(2, 1);
    rig.enter();
    rig.go_now_playing();
    rig.refresh();
    assert!(!rig.ui.model.list_panel_visible());
    rig.ui.compositor.clear_status(); // "Playing ..." popup
    rig.settle();
    // Single row: the waveform stays uncovered, there is no list target to rotate to.
    assert!(!rig.ui.model.up_next_visible(), "no Up Next overlay");
    assert_ne!(
        rig.display.last.as_ref().unwrap().pixels[UP_NEXT_TOP as usize * 320],
        ACCENT,
        "no overlay top edge over the waveform"
    );
    rig.ui
        .handle_input(InputEvent::MenuRotate(1), &mut rig.wifi);
    // WARP_15: nothing while the knob turns; the idle tick sends one SeekTo.
    assert!(rig.ui.take_outgoing().is_empty());
    let now = rig.ui.now_ms() + 500;
    rig.ui
        .handle_input(InputEvent::Tick { now_ms: now }, &mut rig.wifi);
    let cmds: Vec<_> = rig.ui.take_outgoing();
    assert!(cmds.iter().any(|o| matches!(
        o,
        UiOutgoing::Ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SeekTo { .. }))
    )));
}
