//! ART_09: Library > detail > Cover art, from a real library on disk with the production
//! loader thread. Images are real JPEGs made with `plippa-image`.

use plippa_image::{encode_jpeg_fit, Rgb8};
use plippa_noubin::{LibraryRoot, LibraryWriter};
use plippa_ui::{
    InputEvent, LibraryDetailAction, LibrarySource, MockWifi, Route, UiApp, UiConfig,
    COVER_VIEW_SIDE, COVER_VIEW_X, COVER_VIEW_Y, DETAIL_ART_SIDE,
};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const RED: [u8; 3] = [230, 20, 20];
const GREEN: [u8; 3] = [20, 200, 20];
const BLUE: [u8; 3] = [20, 20, 230];

fn put(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

fn split_jpeg(w: u32, h: u32, a: [u8; 3], b: [u8; 3]) -> Vec<u8> {
    let mut px = Vec::new();
    for _ in 0..h {
        for x in 0..w {
            px.extend_from_slice(if x < w / 2 { &a } else { &b });
        }
    }
    encode_jpeg_fit(&Rgb8 { w, h, px }, w.max(h)).unwrap()
}

fn solid(c: [u8; 3]) -> Vec<u8> {
    split_jpeg(64, 64, c, c)
}

fn images(names: &[&str]) -> String {
    let list: Vec<String> = names
        .iter()
        .map(|n| format!(r#"{{"imageLocalFilename":"{n}"}}"#))
        .collect();
    format!(r#"{{"images":[{}]}}"#, list.join(","))
}

/// Album `dir` titled `title` with two tracks; `extra` goes into `releaseData`, `item0` into
/// the first media item.
fn album(root: &Path, dir: &str, title: &str, extra: &str, item0: &str) {
    put(root, &format!("{dir}/01.wav"), b"RIFF");
    put(root, &format!("{dir}/02.wav"), b"RIFF");
    put(
        root,
        &format!("{dir}/_a.noudata"),
        format!(
            r#"{{"noubinKey":"{dir}.k","noubinKeyNormalised":"{dir}.k","playableItemTitle":"{title}",
                "playableItemProfile":"music","releaseData":{{{extra}}},
                "mediaList":[{{"mediaTitle":"{title} one","localFilePaths":["01.wav"]{item0}}},
                             {{"mediaTitle":"{title} two","localFilePaths":["02.wav"]}}]}}"#
        )
        .as_bytes(),
    );
}

fn index(root: &Path) {
    LibraryWriter::open(&LibraryRoot::new(root))
        .unwrap()
        .full_scan()
        .unwrap();
}

struct Rig {
    ui: UiApp,
    src: LibrarySource,
    wifi: MockWifi,
    now: u64,
}

impl Rig {
    fn new(root: &Path) -> Self {
        let mut ui = UiApp::new(UiConfig::default());
        let src = LibrarySource::open(Some(root));
        ui.library_index_changed();
        ui.service_browse(&src);
        Self {
            ui,
            src,
            wifi: MockWifi::default(),
            now: 0,
        }
    }

    fn input(&mut self, ev: InputEvent) {
        self.ui.handle_input(ev, &mut self.wifi);
        self.ui.service_browse(&self.src);
        self.ui.service_art(&self.src);
    }

    fn tick(&mut self) {
        self.now += 1;
        self.ui
            .handle_input(InputEvent::Tick { now_ms: self.now }, &mut self.wifi);
    }

    /// Menu lists: `rows > 0` moves down (the encoder is inverted for lists).
    fn down(&mut self, rows: usize) {
        for _ in 0..rows {
            self.input(InputEvent::MenuRotate(-1));
        }
    }

    fn enter(&mut self) {
        self.input(InputEvent::MenuEnter);
    }

    /// Library root > category row `kind_row` (Tracks 0, Albums 1) > list row `item_row`.
    fn open_detail(&mut self, kind_row: usize, item_row: usize) {
        self.ui.nav.force_push(Route::Library);
        self.ui.library_index_changed();
        self.ui.service_browse(&self.src);
        self.down(kind_row);
        self.enter();
        assert_eq!(*self.ui.nav.current(), Route::LibraryCategory);
        self.down(item_row);
        self.enter();
        assert_eq!(*self.ui.nav.current(), Route::LibraryDetail);
        self.settle();
    }

    /// Focus the `Cover art` action and open it.
    fn open_cover_screen(&mut self) {
        let actions = self.ui.model.detail_actions();
        let at = actions
            .iter()
            .position(|a| *a == LibraryDetailAction::CoverArt)
            .expect("Cover art action offered");
        let focus = self.ui.nav.focus;
        assert!(at >= focus);
        self.down(at - focus);
        self.enter();
        assert_eq!(*self.ui.nav.current(), Route::LibraryCover);
        self.settle();
    }

    /// Wait (bounded) until the viewer has settled on the current image.
    fn settle(&mut self) {
        let t0 = Instant::now();
        loop {
            self.ui.service_art(&self.src);
            self.tick();
            let v = &self.ui.model.cover_view;
            let settled = v.image.is_some() || v.failed || v.count == 0;
            if settled || t0.elapsed() > Duration::from_secs(10) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Settle after a rotate: image `index` is current and (unless it is expected to fail)
    /// its picture, not the previous one that stays up while it loads, is on screen.
    fn settle_on(&mut self, index: usize, want: Option<[u8; 3]>) {
        let t0 = Instant::now();
        loop {
            self.ui.service_art(&self.src);
            self.tick();
            let (idx, failed, has_image) = {
                let v = &self.ui.model.cover_view;
                (v.index, v.failed, v.image.is_some())
            };
            let done = match want {
                None => failed && !has_image,
                Some(c) => !failed && has_image && is(self.centre(), c),
            };
            if idx == index && done {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "image {index} never settled");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn rotate_cover(&mut self, dir: i8) {
        self.input(InputEvent::MenuRotate(dir));
    }

    fn pixel(&mut self, x: u16, y: u16) -> (u8, u8, u8) {
        let px = self.ui.composed_pixels()[usize::from(y) * 320 + usize::from(x)];
        (
            ((px >> 11) as u8 & 0x1f) << 3,
            ((px >> 5) as u8 & 0x3f) << 2,
            (px as u8 & 0x1f) << 3,
        )
    }

    fn centre(&mut self) -> (u8, u8, u8) {
        self.pixel(COVER_VIEW_X + COVER_VIEW_SIDE / 2, COVER_VIEW_Y + COVER_VIEW_SIDE / 2)
    }
}

fn is(c: (u8, u8, u8), want: [u8; 3]) -> bool {
    let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs();
    d(c.0, want[0]) < 40 && d(c.1, want[1]) < 40 && d(c.2, want[2]) < 40
}

/// "Pretty" has three covers; "Plain" none.
fn pretty_and_plain() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    put(&root, "Pretty/a.jpg", &solid(RED));
    put(&root, "Pretty/b.jpg", &solid(GREEN));
    put(&root, "Pretty/c.jpg", &solid(BLUE));
    album(
        &root,
        "Pretty",
        "Pretty",
        &format!(r#""coverImage":{}"#, images(&["a.jpg", "b.jpg", "c.jpg"])),
        "",
    );
    album(&root, "Plain", "Plain", "", "");
    index(&root);
    (tmp, root)
}

#[test]
fn cover_art_action_only_with_a_resolved_image_and_before_associate() {
    let (_tmp, root) = pretty_and_plain();
    // Albums are sorted A-Z: Plain (row 0), Pretty (row 1).
    let mut rig = Rig::new(&root);
    rig.open_detail(1, 0);
    assert_eq!(
        rig.ui.model.detail_actions(),
        vec![
            LibraryDetailAction::Play,
            LibraryDetailAction::BrowseTracks,
            LibraryDetailAction::Associate
        ],
        "no art: the action set is unchanged"
    );
    let mut rig = Rig::new(&root);
    rig.open_detail(1, 1);
    assert_eq!(
        rig.ui.model.detail_actions(),
        vec![
            LibraryDetailAction::Play,
            LibraryDetailAction::BrowseTracks,
            LibraryDetailAction::CoverArt,
            LibraryDetailAction::Associate
        ]
    );
}

#[test]
fn a_sibling_file_counts_as_art_and_a_missing_declared_file_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    put(root, "Sib/cover.jpg", &solid(RED));
    album(root, "Sib", "Sib", "", "");
    // Declares a file that is not there and has no sibling art.
    album(root, "Gone", "Gone", &format!(r#""coverImage":{}"#, images(&["nope.jpg"])), "");
    index(root);
    // A-Z: Gone (0), Sib (1).
    let mut rig = Rig::new(root);
    rig.open_detail(1, 0);
    assert!(!rig.ui.model.detail_actions().contains(&LibraryDetailAction::CoverArt));
    let mut rig = Rig::new(root);
    rig.open_detail(1, 1);
    assert!(rig.ui.model.detail_actions().contains(&LibraryDetailAction::CoverArt));
}

#[test]
fn rotate_walks_the_set_clamped_with_matching_indicator_and_pixels() {
    let (_tmp, root) = pretty_and_plain();
    let mut rig = Rig::new(&root);
    rig.open_detail(1, 1);
    rig.open_cover_screen();
    let v = &rig.ui.model.cover_view;
    assert_eq!((v.index, v.count), (0, 3));
    assert!(is(rig.centre(), RED));

    for (step, (want_idx, colour)) in [(1usize, GREEN), (2, BLUE), (2, BLUE)].into_iter().enumerate() {
        rig.rotate_cover(1);
        assert!(rig.ui.model.cover_view.image.is_some(), "a picture stays up while the next loads");
        rig.settle_on(want_idx, Some(colour));
        assert!(is(rig.centre(), colour), "after right #{}", step + 1);
        assert_eq!(rig.ui.model.cover_view.index, want_idx);
    }
    for (want_idx, colour) in [(1usize, GREEN), (0, RED), (0, RED)] {
        rig.rotate_cover(-1);
        rig.settle_on(want_idx, Some(colour));
        assert!(is(rig.centre(), colour));
    }
    // Left once more at 1/1 stays.
    rig.rotate_cover(-1);
    rig.settle_on(0, Some(RED));
    assert_eq!(rig.ui.model.cover_view.index, 0);
}

#[test]
fn the_view_is_centred_and_stays_off_the_header_and_footer() {
    use plippa_ui::ui::tokens::{CONTENT_BOTTOM, CONTENT_TOP, SCREEN_W};
    assert_eq!(COVER_VIEW_SIDE, 176);
    assert_eq!(COVER_VIEW_X, SCREEN_W - COVER_VIEW_X - COVER_VIEW_SIDE, "horizontal margins equal");
    let top = COVER_VIEW_Y - CONTENT_TOP;
    let bottom = CONTENT_BOTTOM - (COVER_VIEW_Y + COVER_VIEW_SIDE);
    assert!(top.abs_diff(bottom) <= 1);

    let (_tmp, root) = pretty_and_plain();
    let mut rig = Rig::new(&root);
    rig.open_detail(1, 1);
    rig.open_cover_screen();
    // The image corners are the image; the pixels just outside the box are not.
    let (x0, y0) = (COVER_VIEW_X, COVER_VIEW_Y);
    let (x1, y1) = (x0 + COVER_VIEW_SIDE - 1, y0 + COVER_VIEW_SIDE - 1);
    for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
        assert!(is(rig.pixel(x, y), RED), "corner {x},{y}");
    }
    for (x, y) in [(x0 - 1, y0 + 80), (x1 + 1, y0 + 80), (x0 + 80, y0 - 1), (x0 + 80, y1 + 1)] {
        assert!(!is(rig.pixel(x, y), RED), "outside {x},{y}");
    }
    assert!(y0 >= CONTENT_TOP && y1 < CONTENT_BOTTOM);
}

#[test]
fn a_non_square_image_is_centre_cropped() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    put(root, "Wide/cover.jpg", &split_jpeg(300, 100, RED, BLUE));
    album(root, "Wide", "Wide", "", "");
    index(root);
    let mut rig = Rig::new(root);
    rig.open_detail(1, 0);
    rig.open_cover_screen();
    let cy = COVER_VIEW_Y + COVER_VIEW_SIDE / 2;
    assert!(is(rig.pixel(COVER_VIEW_X + 12, cy), RED), "left half of the crop");
    assert!(is(rig.pixel(COVER_VIEW_X + COVER_VIEW_SIDE - 12, cy), BLUE), "right half");
}

#[test]
fn a_corrupt_image_shows_no_preview_and_does_not_stop_navigation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    put(root, "Bad/a.jpg", &solid(RED));
    put(root, "Bad/b.jpg", b"definitely not a jpeg");
    put(root, "Bad/c.jpg", &solid(BLUE));
    album(
        root,
        "Bad",
        "Bad",
        &format!(r#""coverImage":{}"#, images(&["a.jpg", "b.jpg", "c.jpg"])),
        "",
    );
    index(root);
    let mut rig = Rig::new(root);
    rig.open_detail(1, 0);
    rig.open_cover_screen();
    rig.rotate_cover(1);
    rig.settle_on(1, None);
    assert_eq!(rig.ui.model.cover_view.count, 3);
    assert!(rig.ui.model.cover_view.image.is_none(), "the placeholder, not a stale picture");
    rig.rotate_cover(1);
    rig.settle_on(2, Some(BLUE));
    assert!(is(rig.centre(), BLUE));
    // And back over the broken one again.
    rig.rotate_cover(-1);
    rig.settle_on(1, None);
    rig.rotate_cover(-1);
    rig.settle_on(0, Some(RED));
    assert!(is(rig.centre(), RED));
}

#[test]
fn back_restores_the_detail_focus() {
    let (_tmp, root) = pretty_and_plain();
    let mut rig = Rig::new(&root);
    rig.open_detail(1, 1);
    rig.open_cover_screen();
    let focus_on_cover_action = rig.ui.model.detail_actions().iter().position(|a| *a == LibraryDetailAction::CoverArt).unwrap();
    rig.input(InputEvent::Back);
    assert_eq!(*rig.ui.nav.current(), Route::LibraryDetail);
    assert_eq!(rig.ui.nav.focus, focus_on_cover_action);
}

#[test]
fn enter_on_the_cover_screen_does_nothing() {
    let (_tmp, root) = pretty_and_plain();
    let mut rig = Rig::new(&root);
    rig.open_detail(1, 1);
    rig.open_cover_screen();
    rig.enter();
    assert_eq!(*rig.ui.nav.current(), Route::LibraryCover);
    assert_eq!(rig.ui.model.cover_view.index, 0);
}

#[test]
fn an_item_row_resolves_its_own_list_and_a_collection_row_the_collections() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    put(root, "Mix/item.jpg", &solid(RED));
    put(root, "Mix/release.jpg", &solid(BLUE));
    album(
        root,
        "Mix",
        "Mix",
        &format!(r#""coverImage":{}"#, images(&["release.jpg"])),
        &format!(r#","itemCoverImage":{}"#, images(&["item.jpg"])),
    );
    index(root);

    // The Albums row is the collection: the release art.
    let mut rig = Rig::new(root);
    rig.open_detail(1, 0);
    rig.open_cover_screen();
    assert_eq!(rig.ui.model.cover_view.count, 1);
    assert!(is(rig.centre(), BLUE), "collection row: release art");

    // The Tracks rows are items: track one carries its own art, track two falls back.
    let mut rig = Rig::new(root);
    rig.open_detail(0, 0);
    assert_eq!(rig.ui.model.library[0].title, "Mix one");
    rig.open_cover_screen();
    assert!(is(rig.centre(), RED), "item row: the item's own art wins");
    let mut rig = Rig::new(root);
    rig.open_detail(0, 1);
    rig.open_cover_screen();
    assert!(is(rig.centre(), BLUE), "item without art: the release's");
}

#[test]
fn the_detail_art_box_shows_the_primary_image_and_keeps_the_placeholder_without_art() {
    use plippa_ui::ui::tokens::{CONTENT_TOP, MARGIN_X};
    let (_tmp, root) = pretty_and_plain();
    let mut rig = Rig::new(&root);
    rig.open_detail(1, 1);
    let mid = DETAIL_ART_SIDE / 2;
    assert!(is(rig.pixel(MARGIN_X + mid, CONTENT_TOP + mid), RED), "primary image at 72 px");
    let mut rig = Rig::new(&root);
    rig.open_detail(1, 0);
    assert!(!is(rig.pixel(MARGIN_X + mid, CONTENT_TOP + mid), RED));
    assert!(rig.ui.model.cover_view.image.is_none());
}

#[test]
fn the_neighbour_is_ready_before_the_encoder_gets_there() {
    let (_tmp, root) = pretty_and_plain();
    let mut rig = Rig::new(&root);
    rig.open_detail(1, 1);
    rig.open_cover_screen();
    // Give the loader time to fetch image 2 in the background, then turn: no waiting.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(300) {
        rig.ui.service_art(&rig.src);
        rig.tick();
        std::thread::sleep(Duration::from_millis(2));
    }
    rig.rotate_cover(1);
    assert!(is(rig.centre(), GREEN), "the next image is on screen immediately");
}
