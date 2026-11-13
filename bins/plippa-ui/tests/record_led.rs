//! ART_10: the NFC ring plays the Now Playing cover like a record, driven the way the loop
//! drives it (`sync_record_led` each pass), with a real library, the production art loader and
//! the real Playback-settings row.

use plippa_core::{
    NowPlayingEntry, NowPlayingEntryKind, NowPlayingListKind, PlayableItemId, SessionState,
    TransportState,
};
use plippa_image::{encode_jpeg_fit, Rgb8};
use plippa_ipc_proto::{IpcMessage, NowPlayingListChanged, PlaybackStatus};
use plippa_noubin::{encode_playable_id, LibraryRoot, LibraryWriter};
use plippa_ui::{
    sync_record_led, InputEvent, LedCue, LibrarySource, MockLedCue, MockWifi, PlayModeRow,
    RecordFeed, RecordFrame, Route, UiApp, UiConfig, PLAYBACK_SETTINGS_ROWS,
};
use plippa_board::LedRole;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A mock LED backend that also counts every record push.
#[derive(Default)]
struct CountingLed {
    inner: MockLedCue,
    pushes: usize,
}

impl LedCue for CountingLed {
    fn on_tap_feedback(&mut self, tap_id: u64) {
        self.inner.on_tap_feedback(tap_id);
    }
    fn has_role(&self, role: LedRole) -> bool {
        self.inner.has_role(role)
    }
    fn set_record_art(&mut self, frame: Option<RecordFrame>) -> bool {
        self.pushes += 1;
        self.inner.set_record_art(frame)
    }
}

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

fn cover_jpeg() -> Vec<u8> {
    let mut px = Vec::new();
    for _y in 0..32u32 {
        for x in 0..32u32 {
            px.extend_from_slice(if x < 16 { &[230, 20, 20] } else { &[20, 20, 230] });
        }
    }
    encode_jpeg_fit(&Rgb8 { w: 32, h: 32, px }, 32).unwrap()
}

struct Rig {
    ui: UiApp,
    src: LibrarySource,
    wifi: MockWifi,
    id: PlayableItemId,
    rev: u64,
    now: u64,
    _tmp: tempfile::TempDir,
    _state: tempfile::TempDir,
}

impl Rig {
    /// Album "Album" with or without a cover, loaded and `transport`.
    fn new(with_art: bool, transport: TransportState) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        put(root, "Album/01.wav", b"RIFF");
        if with_art {
            put(root, "Album/cover.jpg", &cover_jpeg());
        }
        put(
            root,
            "Album/_a.noudata",
            br#"{"noubinKey":"a.k","noubinKeyNormalised":"a.k","playableItemTitle":"Album",
                "playableItemProfile":"music","releaseData":{},
                "mediaList":[{"mediaTitle":"One","localFilePaths":["01.wav"]}]}"#,
        );
        LibraryWriter::open(&LibraryRoot::new(root))
            .unwrap()
            .full_scan()
            .unwrap();
        let src = LibrarySource::open(Some(root));
        let local = src
            .list_items()
            .into_iter()
            .find(|i| i.title == "Album")
            .unwrap()
            .id;
        let id = PlayableItemId::new(encode_playable_id(root, local));
        let state = tempfile::tempdir().unwrap();
        let ui = UiApp::new(UiConfig {
            play_mode_path: Some(state.path().join("play_mode.json")),
            ..UiConfig::default()
        });
        let mut rig = Self {
            ui,
            src,
            wifi: MockWifi::default(),
            id,
            rev: 0,
            now: 0,
            _tmp: tmp,
            _state: state,
        };
        rig.status(transport, 0);
        rig.ui
            .handle_ipc(IpcMessage::NowPlayingListChanged(NowPlayingListChanged {
                entries: vec![NowPlayingEntry {
                    kind: NowPlayingEntryKind::Track,
                    title: "T".into(),
                    artist: String::new(),
                    start_ms: 0,
                    end_ms: None,
                    duration_ms: Some(600_000),
                    track_index: 0,
                    playable_id: None,
                    available: true,
                    item_position: 0,
                }],
                current_index: 0,
                list_kind: NowPlayingListKind::Album,
                list_id: 1,
            }));
        rig
    }

    fn status(&mut self, transport: TransportState, position_ms: u64) {
        let mut s = SessionState::default();
        s.transport = transport;
        s.current_item = Some(self.id);
        s.title = "Album".into();
        s.duration_ms = Some(600_000);
        s.position_ms = position_ms;
        self.rev += 1;
        self.ui
            .handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(
                s, self.rev,
            )));
    }

    fn tick(&mut self, dt: u64) {
        self.now += dt;
        self.ui
            .handle_input(InputEvent::Tick { now_ms: self.now }, &mut self.wifi);
    }

    /// One loop pass: service the art, then push the ring.
    fn pass(&mut self, dt: u64, feed: &mut RecordFeed, led: &mut CountingLed) {
        self.tick(dt);
        self.ui.service_art(&self.src);
        sync_record_led(&self.ui, led, feed, self.now);
    }

    fn wait_for_art(&mut self) {
        self.ui.service_art(&self.src);
        let t0 = Instant::now();
        while self.ui.model.np_art.is_none() && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(2));
            self.tick(1);
        }
    }

    fn set_art_leds(&mut self, on: bool) {
        self.ui.nav.force_push(Route::SystemPlayMode);
        if self.ui.model.play_mode.art_leds != on {
            // The playback modes lead the merged menu, so index by the setting, not by
            // `PLAY_MODE_ROWS`.
            self.ui.nav.focus = PLAYBACK_SETTINGS_ROWS
                .iter()
                .position(|r| r.setting() == Some(PlayModeRow::ArtLeds))
                .unwrap();
            self.ui
                .handle_input(InputEvent::MenuEnter, &mut self.wifi);
        }
    }
}

fn minimal_profile() -> plippa_board::BoardProfile {
    let text = fs::read_to_string(
        plippa_board::workspace_root_from_crate()
            .join("crates/plippa-board/fixtures/minimal.board.toml"),
    )
    .unwrap();
    plippa_board::BoardProfile::from_toml_str(&text, ".".into()).unwrap()
}

fn angle(led: &CountingLed) -> f32 {
    led.inner.record_art.as_ref().expect("ring shows the record").angle_deg
}

#[test]
fn the_ring_shows_the_now_playing_picture_and_turns_with_media_time() {
    let mut rig = Rig::new(true, TransportState::Playing);
    rig.wait_for_art();
    let (mut feed, mut led) = (RecordFeed::new(), CountingLed::default());
    rig.pass(5, &mut feed, &mut led);
    let frame = led.inner.record_art.clone().expect("a frame is pushed");
    assert!(
        Arc::ptr_eq(&frame.art, rig.ui.model.np_art.as_ref().unwrap()),
        "the ring reuses the picture Now Playing holds, it does not decode again"
    );
    // The position moves with the clock (dead-reckoned), so the angle moves.
    let a0 = angle(&led);
    for _ in 0..20 {
        rig.pass(50, &mut feed, &mut led);
    }
    assert_ne!(angle(&led), a0, "playing: the record turns");
    assert!(led.pushes > 10, "pushes at the frame rate while turning: {}", led.pushes);
}

#[test]
fn paused_freezes_the_ring_with_no_pushes_and_stopped_gives_it_back_once() {
    let mut rig = Rig::new(true, TransportState::Playing);
    rig.wait_for_art();
    let (mut feed, mut led) = (RecordFeed::new(), CountingLed::default());
    for _ in 0..10 {
        rig.pass(40, &mut feed, &mut led);
    }
    rig.status(TransportState::Paused, 400);
    // Let the damper settle on the frozen position, then count.
    for _ in 0..60 {
        rig.pass(40, &mut feed, &mut led);
    }
    let frozen = angle(&led);
    let pushes = led.pushes;
    for _ in 0..100 {
        rig.pass(40, &mut feed, &mut led);
    }
    assert_eq!(led.pushes, pushes, "a paused session sends the ring nothing");
    assert_eq!(angle(&led), frozen, "Paused is a frozen ring, not a blank one");

    rig.status(TransportState::Stopped, 0);
    rig.pass(40, &mut feed, &mut led);
    assert!(led.inner.record_art.is_none(), "Stopped: the ring is the ring again");
    let pushes = led.pushes;
    for _ in 0..20 {
        rig.pass(40, &mut feed, &mut led);
    }
    assert_eq!(led.pushes, pushes, "and it stays quiet");
}

#[test]
fn the_switch_applies_immediately_in_both_directions() {
    let mut rig = Rig::new(true, TransportState::Playing);
    rig.wait_for_art();
    let (mut feed, mut led) = (RecordFeed::new(), CountingLed::default());
    rig.pass(5, &mut feed, &mut led);
    assert!(led.inner.record_art.is_some());
    rig.set_art_leds(false);
    rig.pass(5, &mut feed, &mut led);
    assert!(led.inner.record_art.is_none(), "Off: normal ring at the next pass");
    rig.set_art_leds(true);
    rig.pass(5, &mut feed, &mut led);
    assert!(led.inner.record_art.is_some(), "On again: back without a restart");
}

#[test]
fn no_art_no_ring_and_boards_without_a_ring_are_left_alone() {
    let mut rig = Rig::new(false, TransportState::Playing);
    rig.ui.service_art(&rig.src);
    let (mut feed, mut led) = (RecordFeed::new(), CountingLed::default());
    for _ in 0..30 {
        rig.pass(20, &mut feed, &mut led);
    }
    assert!(rig.ui.model.np_art.is_none());
    assert_eq!(led.pushes, 0, "nothing resolved, nothing pushed");

    // A board profile without the ring: the sync never calls the backend.
    let mut rig = Rig::new(true, TransportState::Playing);
    rig.wait_for_art();
    let mut bare = CountingLed {
        inner: MockLedCue::for_profile(&minimal_profile()),
        pushes: 0,
    };
    let mut feed = RecordFeed::new();
    for _ in 0..10 {
        rig.pass(20, &mut feed, &mut bare);
    }
    assert_eq!(bare.pushes, 0);
}
