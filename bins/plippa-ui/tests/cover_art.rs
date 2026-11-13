//! ART_08: the Now Playing cover from a real library on disk: which image wins (D4), the
//! centre crop (D2), what the device refuses, and that the painted frame stays inside its box.
//!
//! Images are real JPEGs made with `plippa-image`; the loader thread is the production one.

use plippa_core::{NowPlayingEntry, NowPlayingEntryKind, NowPlayingListKind, PlayableItemId, SessionState, TransportState};
use plippa_image::{encode_jpeg_fit, Rgb8};
use plippa_ipc_proto::{IpcMessage, NowPlayingListChanged, PlaybackStatus};
use plippa_noubin::{encode_playable_id, LibraryRoot, LibraryWriter};
use plippa_ui::art::{
    load_art, resolve_playing_art, ArtErrorKind, ArtKey, ART_ORIGINAL_MAX_PIXELS,
};
use plippa_ui::{
    InputEvent, LibrarySource, MockWifi, UiApp, UiConfig, NP_ART_SIDE, NP_ART_X, NP_ART_Y,
    WAVE_BAND_BOTTOM, WAVE_BAND_Y, WAVE_PLAYHEAD_X,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const RED: [u8; 3] = [230, 20, 20];
const BLUE: [u8; 3] = [20, 20, 230];
const GREEN: [u8; 3] = [20, 200, 20];

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

/// `w` x `h` picture split into two colours: left/right halves, or top/bottom when `vertical`.
fn split_jpeg(w: u32, h: u32, a: [u8; 3], b: [u8; 3], vertical: bool) -> Vec<u8> {
    let mut px = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let first = if vertical { y < h / 2 } else { x < w / 2 };
            px.extend_from_slice(if first { &a } else { &b });
        }
    }
    encode_jpeg_fit(&Rgb8 { w, h, px }, w.max(h)).unwrap()
}

fn solid_jpeg(w: u32, h: u32, c: [u8; 3]) -> Vec<u8> {
    split_jpeg(w, h, c, c, false)
}

fn index(root: &Path) {
    LibraryWriter::open(&LibraryRoot::new(root))
        .unwrap()
        .full_scan()
        .unwrap();
}

fn images(names: &[&str]) -> String {
    let list: Vec<String> = names
        .iter()
        .map(|n| format!(r#"{{"imageLocalFilename":"{n}"}}"#))
        .collect();
    format!(r#"{{"images":[{}]}}"#, list.join(","))
}

/// A one-album library: two tracks. `extra` is spliced into the top-level object (cover lists).
fn album(root: &Path, extra: &str, item0: &str) {
    put(root, "Album/01.wav", b"RIFF");
    put(root, "Album/02.wav", b"RIFF");
    put(
        root,
        "Album/_a.noudata",
        format!(
            r#"{{"noubinKey":"a.k","noubinKeyNormalised":"a.k","playableItemTitle":"Album",
                "playableItemProfile":"music","releaseData":{{{extra}}},
                "mediaList":[{{"mediaTitle":"One","localFilePaths":["01.wav"]{item0}}},
                             {{"mediaTitle":"Two","localFilePaths":["02.wav"]}}]}}"#
        )
        .as_bytes(),
    );
}

fn local_id(src: &LibrarySource, title: &str) -> i64 {
    src.list_items()
        .into_iter()
        .find(|i| i.title == title)
        .unwrap_or_else(|| panic!("{title} not indexed"))
        .id
}

struct Rig {
    ui: UiApp,
    src: LibrarySource,
    wifi: MockWifi,
    root: PathBuf,
    now: u64,
}

impl Rig {
    /// Now Playing `title` at `position` (0-based `mediaList` item), transport playing.
    fn playing(root: &Path, title: &str, position: u32) -> Self {
        let src = LibrarySource::open(Some(root));
        let id = PlayableItemId::new(encode_playable_id(root, local_id(&src, title)));
        let mut ui = UiApp::new(UiConfig::default());
        let mut s = SessionState::default();
        s.transport = TransportState::Playing;
        s.current_item = Some(id);
        s.title = title.into();
        s.duration_ms = Some(60_000);
        ui.handle_ipc(IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(s, 1)));
        let entry = |i: u32| NowPlayingEntry {
            kind: NowPlayingEntryKind::Track,
            title: format!("T{i}"),
            artist: String::new(),
            start_ms: 0,
            end_ms: None,
            duration_ms: Some(60_000),
            track_index: i,
            playable_id: None,
            available: true,
            item_position: i,
        };
        ui.handle_ipc(IpcMessage::NowPlayingListChanged(NowPlayingListChanged {
            entries: vec![entry(0), entry(1)],
            current_index: position,
            list_kind: NowPlayingListKind::Album,
            list_id: 1,
        }));
        Self {
            ui,
            src,
            wifi: MockWifi::default(),
            root: root.to_path_buf(),
            now: 0,
        }
    }

    fn tick(&mut self, dt: u64) {
        self.now += dt;
        self.ui
            .handle_input(InputEvent::Tick { now_ms: self.now }, &mut self.wifi);
    }

    /// The loop runner's job: resolve, then wait (bounded) for the loader thread.
    fn settle(&mut self) {
        self.ui.service_art(&self.src);
        let t0 = Instant::now();
        while self.ui.model.np_art.is_none() && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(2));
            self.tick(1);
        }
    }

    /// Wait long enough for a load that is expected to fail to have been attempted.
    fn settle_expecting_nothing(&mut self) {
        self.ui.service_art(&self.src);
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(2));
            self.tick(1);
        }
        assert!(self.ui.model.np_art.is_none(), "no art expected");
    }

    fn pixel(&mut self, x: u16, y: u16) -> (u8, u8, u8) {
        let px = self.ui.composed_pixels()[usize::from(y) * 320 + usize::from(x)];
        (
            ((px >> 11) as u8 & 0x1f) << 3,
            ((px >> 5) as u8 & 0x3f) << 2,
            (px as u8 & 0x1f) << 3,
        )
    }

    fn first_cover(&self, position: u32) -> Option<String> {
        let id = encode_playable_id(&self.root, local_id(&self.src, "Album"));
        resolve_playing_art(&self.src, id, position)
            .covers
            .into_iter()
            .next()
            .map(|c| c.relpath)
    }
}

fn reddish((r, g, b): (u8, u8, u8)) -> bool {
    r > 150 && g < 90 && b < 90
}

fn bluish((r, g, b): (u8, u8, u8)) -> bool {
    b > 150 && r < 90 && g < 90
}

#[test]
fn the_item_beats_its_release_which_beats_the_sibling_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    put(root, "Album/item.jpg", &solid_jpeg(32, 32, RED));
    put(root, "Album/release.jpg", &solid_jpeg(32, 32, BLUE));
    put(root, "Album/cover.jpg", &solid_jpeg(32, 32, GREEN));
    album(
        root,
        &format!(r#""coverImage":{}"#, images(&["release.jpg"])),
        &format!(r#","itemCoverImage":{}"#, images(&["item.jpg"])),
    );
    index(root);
    let mut rig = Rig::playing(root, "Album", 0);
    assert_eq!(rig.first_cover(0).as_deref(), Some("Album/item.jpg"), "item art first");
    assert_eq!(rig.first_cover(1).as_deref(), Some("Album/release.jpg"), "else the release's");
    assert!(!rig.ui.playing_item_has_art(), "nothing resolved before the loop services it");
    rig.settle();
    assert!(rig.ui.playing_item_has_art());
    let cy = NP_ART_Y + NP_ART_SIDE / 2;
    assert!(reddish(rig.pixel(NP_ART_X + 40, cy)), "track 1 shows its own (red) art");
}

#[test]
fn a_release_without_declared_art_falls_back_to_the_sibling_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    put(root, "Album/cover.jpg", &solid_jpeg(32, 32, GREEN));
    album(root, "", "");
    index(root);
    let rig = Rig::playing(root, "Album", 1);
    assert_eq!(rig.first_cover(1).as_deref(), Some("Album/cover.jpg"));
}

#[test]
fn the_cover_is_painted_as_a_centre_square_crop_of_a_landscape_image() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    put(root, "Album/cover.jpg", &split_jpeg(200, 100, RED, BLUE, false));
    album(root, "", "");
    index(root);
    let mut rig = Rig::playing(root, "Album", 0);
    rig.settle();
    assert!(rig.ui.model.np_art.is_some(), "art loaded");
    assert!(rig.ui.playing_item_has_art());
    let cy = NP_ART_Y + NP_ART_SIDE / 2;
    // 200 x 100: the 100 x 100 middle is kept, so each half of the square is one colour.
    assert!(reddish(rig.pixel(NP_ART_X + 12, cy)), "left of the crop is red");
    assert!(bluish(rig.pixel(NP_ART_X + NP_ART_SIDE - 12, cy)), "right of the crop is blue");
}

#[test]
fn a_portrait_image_is_cropped_to_its_middle_rows() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    put(root, "Album/cover.jpg", &split_jpeg(100, 200, RED, BLUE, true));
    album(root, "", "");
    index(root);
    let mut rig = Rig::playing(root, "Album", 0);
    rig.settle();
    let cx = NP_ART_X + NP_ART_SIDE / 2;
    assert!(reddish(rig.pixel(cx, NP_ART_Y + 12)), "top of the crop is red");
    assert!(bluish(rig.pixel(cx, NP_ART_Y + NP_ART_SIDE - 12)), "bottom of the crop is blue");
}

#[test]
fn art_stays_inside_its_box_and_the_frame_without_art_is_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    put(root, "Album/cover.jpg", &solid_jpeg(128, 128, [255, 0, 255]));
    album(root, "", "");
    index(root);
    let mut with_art = Rig::playing(root, "Album", 0);
    with_art.settle();
    let framed = with_art.ui.composed_pixels();
    let magenta = |p: u16| (p >> 11) > 26 && ((p >> 5) & 0x3f) < 8 && (p & 0x1f) > 26;
    let mut inside = 0;
    for (i, p) in framed.iter().enumerate() {
        let (x, y) = ((i % 320) as u16, (i / 320) as u16);
        let in_box = (NP_ART_X..NP_ART_X + NP_ART_SIDE).contains(&x)
            && (NP_ART_Y..NP_ART_Y + NP_ART_SIDE).contains(&y);
        if in_box {
            inside += usize::from(magenta(*p));
        } else {
            assert!(!magenta(*p), "art colour outside the box at ({x}, {y})");
        }
    }
    assert!(inside > usize::from(NP_ART_SIDE) * usize::from(NP_ART_SIDE) * 95 / 100);
    assert!(NP_ART_Y >= WAVE_BAND_Y && NP_ART_Y + NP_ART_SIDE <= WAVE_BAND_BOTTOM);
    assert!(NP_ART_X + NP_ART_SIDE < WAVE_PLAYHEAD_X);

    // The same screen with nothing to show: byte-identical to an app that never looked.
    let tmp2 = tempfile::tempdir().unwrap();
    album(tmp2.path(), "", "");
    index(tmp2.path());
    let mut none = Rig::playing(tmp2.path(), "Album", 0);
    none.ui.service_art(&none.src);
    let mut untouched = Rig::playing(tmp2.path(), "Album", 0);
    assert!(!none.ui.playing_item_has_art());
    assert_eq!(none.ui.composed_pixels(), untouched.ui.composed_pixels());
}

#[test]
fn an_original_over_two_megapixels_without_a_thumbnail_is_not_decoded() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // 2000 x 1250 = 2.5 MP, well inside the Noubin limits but over what the device decodes.
    assert!(2000u64 * 1250 > ART_ORIGINAL_MAX_PIXELS);
    put(root, "Album/cover.jpg", &solid_jpeg(2000, 1250, RED));
    album(root, "", "");
    index(root);
    let key = ArtKey {
        root_idx: 0,
        image_relpath: "Album/cover.jpg".into(),
        px: NP_ART_SIDE,
    };
    let err = load_art(root, &key).unwrap_err();
    assert_eq!(err.kind, ArtErrorKind::TooLargeForDevice);
    let mut rig = Rig::playing(root, "Album", 0);
    rig.settle_expecting_nothing();
    assert!(rig.ui.playing_item_has_art(), "the cover exists: ART_09 may still list it");

    // A 128 px thumbnail next to it is used instead, and the big file is never read.
    put(root, "Album/thumbnails/cover-128.jpg", &solid_jpeg(128, 80, BLUE));
    let ok = load_art(root, &key).unwrap();
    assert_eq!((ok.side, ok.rgb565.len()), (NP_ART_SIDE, 96 * 96));
    assert_eq!(ok.rgb.len(), 64 * 64 * 3);
}

#[test]
fn a_decompression_bomb_is_refused_without_a_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let bomb = include_bytes!("../../../crates/plippa-image/tests/fixtures/bomb_60000x60000.png");
    put(root, "Album/cover.png", bomb);
    album(root, "", "");
    index(root);
    let key = ArtKey {
        root_idx: 0,
        image_relpath: "Album/cover.png".into(),
        px: NP_ART_SIDE,
    };
    assert!(load_art(root, &key).is_err());
    let mut rig = Rig::playing(root, "Album", 0);
    rig.settle_expecting_nothing();
}

#[test]
fn a_corrupt_file_shows_no_art_and_is_not_an_error_screen() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    put(root, "Album/cover.jpg", b"\xFF\xD8\xFF\xE0 this is not a jpeg at all");
    album(root, "", "");
    index(root);
    let mut rig = Rig::playing(root, "Album", 0);
    rig.settle_expecting_nothing();
    assert!(rig.ui.playing_item_has_art(), "the file is listed; it just cannot be drawn");
}

#[test]
fn a_library_change_resolves_afresh() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    put(root, "Album/cover.jpg", &solid_jpeg(64, 64, RED));
    album(root, "", "");
    index(root);
    let mut rig = Rig::playing(root, "Album", 0);
    rig.settle();
    let first = rig.ui.model.np_art.clone().expect("loaded");
    // Same item, nothing changed: the cached picture is kept, not decoded again.
    rig.ui.service_art(&rig.src);
    assert!(std::sync::Arc::ptr_eq(&first, rig.ui.model.np_art.as_ref().unwrap()));
    // The index changed (cover replaced): a fresh decode of the new file.
    put(root, "Album/cover.jpg", &solid_jpeg(64, 64, BLUE));
    rig.ui.library_index_changed();
    rig.ui.service_art(&rig.src);
    let t0 = Instant::now();
    loop {
        rig.tick(1);
        let now = rig.ui.model.np_art.clone();
        if now.as_ref().is_some_and(|n| !std::sync::Arc::ptr_eq(n, &first)) {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "new art never arrived");
        std::thread::sleep(Duration::from_millis(2));
    }
    let cy = NP_ART_Y + NP_ART_SIDE / 2;
    assert!(bluish(rig.pixel(NP_ART_X + 40, cy)));
}
