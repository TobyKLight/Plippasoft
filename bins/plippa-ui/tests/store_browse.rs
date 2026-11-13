//! BCS_08: the store screens on the device, driven through the in-process UI harness.
//!
//! The device is off, so nothing here touches hardware or a real server: a scripted
//! [`StoreApi`] stands in for the local service and the app is fed ordinary input events.
//! Assertions are about decisions, calls and what the frame can show — plus the one thing the
//! work package asks for as arithmetic: the QR module size on a 320x240 panel.

use std::sync::{Arc, Mutex};

use plippa_ui::ui::text::{advance, measure, TypeRole};
use plippa_ui::ui::tokens::{
    BG, CONTENT_BOTTOM, CONTENT_TOP, FG, MARGIN_X, ROW_H, SCREEN_H, SCREEN_W, SPACE_2, SURFACE,
    SURFACE_ALT,
};
use plippa_ui::{
    default_mock_wifi, screen_text, CollectionPageInfo, ConnectView, DeleteAction, DeleteDone,
    DeletePhase, DeletePreviewInfo, DeleteView, DownloadConsent, DownloadPhase, DownloadProgress,
    DownloadSession, DownloadState, FrameBuffer, InputEvent, LibraryDetailAction, LibraryItem,
    MockWifi, PortalAddress, ReleaseAction, ReleaseInfo, ReleasePhase, ReleaseRow,
    ReleaseTrackInfo, ReleasesPhase, ReleasesRow, ReleasesView, ReleaseView, Route, Rgb565,
    ScreenModel, StoreApi, StoreApiError, StoreBlocked, StoreInfo, StoreItem, StoresInfo,
    StoresPhase, StoresView, UiApp, UiConfig, STORE_QR_BOX, STORE_QR_MIN_MODULE_PX,
    STORE_QR_QUIET_MODULES,
};

// ---------------------------------------------------------------------------
// A scripted server
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Calls {
    portals: usize,
    stores: usize,
    /// `(query, offset, size)` of every collection call.
    collections: Vec<(String, u32, u32)>,
    releases: Vec<String>,
    previews: Vec<u64>,
    deletes: Vec<u64>,
    prepares: Vec<(String, String)>,
    starts: Vec<String>,
    statuses: Vec<String>,
    cancels: Vec<String>,
    sync_starts: usize,
    sync_statuses: usize,
}

struct MockStore {
    calls: Arc<Mutex<Calls>>,
    /// Answers in order; the last one repeats.
    portal: Vec<Result<PortalAddress, StoreApiError>>,
    stores: Vec<Result<StoresInfo, StoreApiError>>,
    pages: Vec<Result<CollectionPageInfo, StoreApiError>>,
    release: Option<Result<ReleaseInfo, StoreApiError>>,
    preview: Option<Result<DeletePreviewInfo, StoreApiError>>,
    delete: Option<Result<DeleteDone, StoreApiError>>,
    consent: Option<Result<DownloadConsent, StoreApiError>>,
    session: Option<Result<DownloadSession, StoreApiError>>,
    progress: Vec<Result<DownloadProgress, StoreApiError>>,
    /// What `sync_start` answers.
    sync_start: Option<Result<plippa_ui::SyncStatus, StoreApiError>>,
    /// Answers to `sync_status` in order; the last one repeats.
    sync_status: Vec<Result<plippa_ui::SyncStatus, StoreApiError>>,
}

impl MockStore {
    fn new(calls: Arc<Mutex<Calls>>) -> Self {
        Self {
            calls,
            portal: Vec::new(),
            stores: Vec::new(),
            pages: Vec::new(),
            release: None,
            preview: None,
            delete: None,
            consent: None,
            session: None,
            progress: Vec::new(),
            sync_start: None,
            sync_status: Vec::new(),
        }
    }
}

/// Answer the next scripted value, repeating the last one so a poll never runs dry.
fn next<T: Clone>(queue: &mut Vec<Result<T, StoreApiError>>) -> Result<T, StoreApiError> {
    if queue.len() > 1 {
        queue.remove(0)
    } else {
        queue
            .first()
            .cloned()
            .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
    }
}

impl StoreApi for MockStore {
    fn tap(
        &mut self,
        _url: &str,
        _expected_duration_s: Option<u64>,
        _consent: bool,
    ) -> Result<plippa_ui::TapAnswer, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn want(&mut self, _url: &str) -> Result<(), StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn download_prepare(
        &mut self,
        noubin_key: &str,
        release_id: &str,
    ) -> Result<DownloadConsent, StoreApiError> {
        self.calls
            .lock()
            .unwrap()
            .prepares
            .push((noubin_key.to_string(), release_id.to_string()));
        self.consent
            .clone()
            .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
    }

    fn download_start(&mut self, consent_id: &str) -> Result<DownloadSession, StoreApiError> {
        self.calls
            .lock()
            .unwrap()
            .starts
            .push(consent_id.to_string());
        self.session
            .clone()
            .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
    }

    fn download_status(&mut self, session: &str) -> Result<DownloadProgress, StoreApiError> {
        self.calls
            .lock()
            .unwrap()
            .statuses
            .push(session.to_string());
        next(&mut self.progress)
    }

    fn download_cancel(&mut self, session: &str) -> Result<(), StoreApiError> {
        self.calls
            .lock()
            .unwrap()
            .cancels
            .push(session.to_string());
        Ok(())
    }

    fn portal_address(&mut self) -> Result<PortalAddress, StoreApiError> {
        self.calls.lock().unwrap().portals += 1;
        next(&mut self.portal)
    }

    fn stores(&mut self) -> Result<StoresInfo, StoreApiError> {
        self.calls.lock().unwrap().stores += 1;
        next(&mut self.stores)
    }

    fn collection(
        &mut self,
        query: &str,
        offset: u32,
        size: u32,
    ) -> Result<CollectionPageInfo, StoreApiError> {
        self.calls
            .lock()
            .unwrap()
            .collections
            .push((query.to_string(), offset, size));
        next(&mut self.pages)
    }

    fn release(&mut self, release_id: &str) -> Result<ReleaseInfo, StoreApiError> {
        self.calls
            .lock()
            .unwrap()
            .releases
            .push(release_id.to_string());
        self.release
            .clone()
            .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
    }

    fn delete_preview(&mut self, playable_id: u64) -> Result<DeletePreviewInfo, StoreApiError> {
        self.calls.lock().unwrap().previews.push(playable_id);
        self.preview
            .clone()
            .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
    }

    fn delete_playable(&mut self, playable_id: u64) -> Result<DeleteDone, StoreApiError> {
        self.calls.lock().unwrap().deletes.push(playable_id);
        self.delete
            .clone()
            .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
    }

    // BCS_11: the Stores screen's resync. The cover call belongs to the tap screen's tests.
    fn sync_start(&mut self) -> Result<plippa_ui::SyncStatus, StoreApiError> {
        self.calls.lock().unwrap().sync_starts += 1;
        self.sync_start
            .clone()
            .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
    }

    fn sync_status(&mut self) -> Result<plippa_ui::SyncStatus, StoreApiError> {
        self.calls.lock().unwrap().sync_statuses += 1;
        next(&mut self.sync_status)
    }

    fn sync_cancel(&mut self) -> Result<plippa_ui::SyncStatus, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn release_cover(&mut self, _release_id: &str) -> Result<plippa_ui::CoverBytes, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn waveform_status(
        &mut self,
        _playable_id: u64,
        _item: u32,
    ) -> Result<Vec<plippa_ui::AnalysisKindStatus>, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn reanalyse(
        &mut self,
        _playable_id: u64,
        _item: u32,
        _kind: Option<plippa_ui::AnalysisKind>,
    ) -> Result<(), StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn portal_settings(&mut self) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn set_portal_enabled(
        &mut self,
        _enabled: bool,
    ) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn set_https_enabled(
        &mut self,
        _on: bool,
    ) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn regenerate_certificate(&mut self) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn set_pin_required(
        &mut self,
        _required: bool,
    ) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn set_pin(&mut self, _pin: &str) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn clear_lockout(&mut self) -> Result<plippa_ui::PortalSettingsInfo, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }
}

// ---------------------------------------------------------------------------
// Fixtures and harness
// ---------------------------------------------------------------------------

fn portal(url: &str) -> PortalAddress {
    PortalAddress {
        recommended: Some(url.to_string()),
        candidates: vec![url.to_string()],
        port: 8787,
        note: None,
        portal_off: false,
    }
}

fn stores_info(connected: bool) -> StoresInfo {
    StoresInfo {
        secrets_available: true,
        secrets_reason: None,
        stores: vec![StoreInfo {
            id: "bandcamp".into(),
            name: "Bandcamp".into(),
            connected,
            rejected: false,
            cached_releases: if connected { 12 } else { 0 },
            cache_stale: false,
        }],
    }
}

fn release_row(id: &str, title: &str) -> ReleaseRow {
    ReleaseRow {
        id: id.into(),
        title: title.into(),
        artist: "Joni Mitchell".into(),
        year: Some(1971),
        track_count: Some(10),
        duration_s: Some(2_178),
    }
}

fn page(offset: u32, ids: &[&str], total: u32) -> CollectionPageInfo {
    CollectionPageInfo {
        total,
        offset,
        stale: false,
        synced_at: None,
        releases: ids
            .iter()
            .map(|id| release_row(id, &format!("Release {id}")))
            .collect(),
    }
}

fn release_info() -> ReleaseInfo {
    ReleaseInfo {
        release: release_row("1234", "Blue"),
        tracks: vec![
            ReleaseTrackInfo {
                title: "All I Want".into(),
                track: Some(1),
                duration_s: Some(200),
                declared_bytes: Some(1_000),
                suffix: Some("mp3".into()),
            },
            ReleaseTrackInfo {
                title: "My Old Man".into(),
                track: Some(2),
                duration_s: Some(200),
                declared_bytes: Some(2_000),
                suffix: Some("mp3".into()),
            },
        ],
        lossy: true,
        quality_note: "Bandcamp sends a compressed stream, not the purchased FLAC.".into(),
        declared_bytes: Some(3_000),
        cached: true,
    }
}

fn progress(state: DownloadState, placed: usize, noudata: Option<&str>) -> DownloadProgress {
    DownloadProgress {
        state,
        tracks_done: if state == DownloadState::Done { 10 } else { 2 },
        tracks_total: 10,
        done_bytes: 40 * 1024 * 1024,
        total_bytes: 200 * 1024 * 1024,
        placed_files: placed,
        already_present: false,
        noudata: noudata.map(str::to_string),
        errors: Vec::new(),
        warnings: Vec::new(),
    }
}

fn preview() -> DeletePreviewInfo {
    DeletePreviewInfo {
        title: "Blue".into(),
        files: 10,
        total_bytes: 200 * 1024 * 1024,
        blockers: Vec::new(),
        warnings: Vec::new(),
        playing: false,
        playlists: Vec::new(),
        noudata: Some("Blue/blue.noudata".into()),
    }
}

fn app_with(mock: MockStore) -> UiApp {
    let mut app = UiApp::new(UiConfig::default());
    app.set_store_api(Box::new(mock));
    app
}

/// Tick the app until `done` holds (bounded), so worker-thread timing never flakes a test.
fn pump_until(app: &mut UiApp, wifi: &mut MockWifi, done: impl Fn(&UiApp) -> bool) {
    for step in 0..900u64 {
        if done(app) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        app.handle_input(InputEvent::Tick { now_ms: step * 16 }, wifi);
    }
    panic!("condition not reached");
}

fn press(app: &mut UiApp, wifi: &mut MockWifi, ev: InputEvent) {
    app.handle_input(ev, wifi);
}

fn enter(app: &mut UiApp, wifi: &mut MockWifi) {
    press(app, wifi, InputEvent::MenuEnter);
}

/// UI_16 §7: a negative detent moves down the list.
fn down(app: &mut UiApp, wifi: &mut MockWifi) {
    press(app, wifi, InputEvent::MenuRotate(-1));
}

fn stores_ready(app: &UiApp) -> bool {
    matches!(
        app.model.store_stores.as_ref().map(|v| &v.phase),
        Some(StoresPhase::Ready(_))
    )
}

/// Force `route` and paint one frame of the current model.
fn render_route(app: &mut UiApp, route: Route) -> FrameBuffer {
    app.nav.force_push(route);
    let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
    fb.clear(BG);
    app.model.render_screen(&app.nav, &mut fb);
    fb
}

/// Anything painted where nothing should be: the ground colours are not ink.
fn is_ink(pixel: Rgb565) -> bool {
    pixel != BG && pixel != SURFACE && pixel != SURFACE_ALT
}

fn ink_below_content(fb: &FrameBuffer) -> usize {
    let start = CONTENT_BOTTOM as usize * SCREEN_W as usize;
    fb.pixels[start..].iter().filter(|&&p| p != BG).count()
}

/// Number of consecutive `FG` pixels starting at `x` on row `y` (the QR's quiet zone row is a
/// solid run the width of the symbol).
fn fg_run(fb: &FrameBuffer, x: u16, y: u16) -> u16 {
    let mut run = 0u16;
    let mut col = x;
    while col < SCREEN_W && fb.pixels[y as usize * SCREEN_W as usize + col as usize] == FG {
        run = run.saturating_add(1);
        col = col.saturating_add(1);
    }
    run
}

// ---------------------------------------------------------------------------
// Connect: the QR code and the address
// ---------------------------------------------------------------------------

#[test]
fn connect_shows_the_portal_address_and_refreshes_it() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.stores = vec![Ok(stores_info(false))];
    mock.portal = vec![
        Ok(portal("http://192.168.1.42:8787")),
        Ok(portal("http://192.168.1.77:8787")),
    ];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    app.open_stores(Route::SystemStores);
    pump_until(&mut app, &mut wifi, stores_ready);
    assert_eq!(*app.nav.current(), Route::SystemStores);

    // Row 0 is Bandcamp; not connected → the connect screen, never a download.
    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_connect
            .as_ref()
            .is_some_and(|view| view.qr_url().is_some())
    });
    assert_eq!(*app.nav.current(), Route::StoreConnect);
    let view = app.model.store_connect.as_ref().expect("the connect screen");
    assert_eq!(view.qr_url(), Some("http://192.168.1.42:8787"));
    assert_eq!(view.address_text().as_deref(), Some("192.168.1.42:8787"));
    assert_eq!(calls.lock().unwrap().portals, 1);

    // Refresh: the address changed, and the QR follows it.
    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_connect
            .as_ref()
            .is_some_and(|view| view.qr_url() == Some("http://192.168.1.77:8787"))
    });
    assert_eq!(calls.lock().unwrap().portals, 2);

    // Back returns to Stores and drops the connect state.
    press(&mut app, &mut wifi, InputEvent::Back);
    assert_eq!(*app.nav.current(), Route::SystemStores);
    assert!(app.model.store_connect.is_none());
}

#[test]
fn connect_says_the_service_is_down_instead_of_showing_a_qr() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.portal = vec![Err(StoreApiError::Offline("connection refused".into()))];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    app.open_connect();
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_connect
            .as_ref()
            .is_some_and(|view| view.blocked().is_some())
    });
    let view = app.model.store_connect.as_ref().expect("the connect screen");
    assert_eq!(view.qr_url(), None, "no QR for an address we never had");
    let (blocked, message) = view.blocked().expect("a blocked connect screen");
    assert_eq!(blocked, StoreBlocked::ServiceDown);
    assert!(message.contains("not running"), "{message}");

    // Retrying is offered, and asks again.
    assert!(view.actions().contains(&plippa_ui::ConnectAction::Retry));
    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |_app| {
        calls.lock().unwrap().portals == 2
    });
}

// ---------------------------------------------------------------------------
// Stores and the collection
// ---------------------------------------------------------------------------

#[test]
fn stores_opens_the_collection_only_when_connected() {
    // Connected: the Bandcamp row is the collection.
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.stores = vec![Ok(stores_info(true))];
    mock.pages = vec![Ok(page(0, &["1"], 1))];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();
    app.open_stores(Route::LibraryStores);
    pump_until(&mut app, &mut wifi, stores_ready);
    enter(&mut app, &mut wifi);
    assert_eq!(*app.nav.current(), Route::StoreReleases);
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_releases
            .as_ref()
            .is_some_and(|view| !view.releases.is_empty())
    });
    // BCS_11: the Stores screen asks for one row first, only to read when the cache was last
    // synced; the Releases screen then asks for its own page.
    assert_eq!(
        calls.lock().unwrap().collections,
        vec![(String::new(), 0, 1), (String::new(), 0, 20)]
    );

    // Not connected: the same row leads to the QR instead.
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.stores = vec![Ok(stores_info(false))];
    mock.portal = vec![Ok(portal("http://192.168.1.42:8787"))];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();
    app.open_stores(Route::LibraryStores);
    pump_until(&mut app, &mut wifi, stores_ready);
    enter(&mut app, &mut wifi);
    assert_eq!(*app.nav.current(), Route::StoreConnect);
    assert!(calls.lock().unwrap().collections.is_empty());
}

#[test]
fn collection_pages_to_the_last_page_and_stops() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.pages = vec![
        Ok(page(0, &["1", "2"], 3)),
        Ok(page(2, &["3"], 3)),
        // A third answer would come only if the screen asked for it.
        Ok(page(6, &[], 3)),
    ];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();
    app.open_releases();
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_releases
            .as_ref()
            .is_some_and(|view| view.releases.len() == 2)
    });
    {
        let view = app.model.store_releases.as_ref().unwrap();
        assert_eq!(view.next_offset, Some(2));
        assert!(view.rows().contains(&ReleasesRow::More));
    }

    // The More row is after the two releases: two detents down and press.
    down(&mut app, &mut wifi);
    down(&mut app, &mut wifi);
    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_releases
            .as_ref()
            .is_some_and(|view| view.releases.len() == 3)
    });
    let view = app.model.store_releases.as_ref().unwrap();
    assert_eq!(view.next_offset, None, "the last page closes the list");
    assert!(!view.rows().contains(&ReleasesRow::More));
    assert_eq!(
        calls.lock().unwrap().collections,
        vec![(String::new(), 0, 20), (String::new(), 2, 20)],
        "nothing is asked for past the last page"
    );

    // One release in total: no More row at all.
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls);
    mock.pages = vec![Ok(page(0, &["only"], 1))];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();
    app.open_releases();
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_releases
            .as_ref()
            .is_some_and(|view| !view.releases.is_empty())
    });
    assert_eq!(
        app.model.store_releases.as_ref().unwrap().rows(),
        vec![
            ReleasesRow::Release(0),
            ReleasesRow::Search,
            ReleasesRow::Back
        ],
        "a single release still offers search"
    );
}

#[test]
fn empty_collection_is_a_state_not_an_error() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls);
    mock.pages = vec![Ok(page(0, &[], 0))];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();
    app.open_releases();
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_releases
            .as_ref()
            .is_some_and(|view| view.is_empty())
    });
    let view = app.model.store_releases.as_ref().unwrap();
    assert!(view.is_empty());
    assert_eq!(view.rows(), vec![ReleasesRow::Search, ReleasesRow::Back]);
    // The empty state offers no More row and no retry: there is nothing wrong.
    assert!(!view.rows().contains(&ReleasesRow::Retry));
}

#[test]
fn search_row_opens_the_keyboard_and_sends_the_typed_query() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.pages = vec![Ok(page(0, &["1", "2"], 2)), Ok(page(0, &["2"], 1))];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();
    app.open_releases();
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_releases
            .as_ref()
            .is_some_and(|view| view.releases.len() == 2)
    });

    // Rows are release, release, Search, Back: two detents reach Search.
    down(&mut app, &mut wifi);
    down(&mut app, &mut wifi);
    enter(&mut app, &mut wifi);
    {
        let view = app.model.store_releases.as_ref().unwrap();
        assert!(view.searching);
        assert!(view.rows().is_empty(), "the keyboard replaces the rows");
        assert!(app.model.store_search.is_some());
    }

    // Back leaves the keyboard without losing the list, and stays on the screen.
    press(&mut app, &mut wifi, InputEvent::Back);
    assert_eq!(*app.nav.current(), Route::StoreReleases);
    assert!(app.model.store_search.is_none());
    let view = app.model.store_releases.as_ref().unwrap();
    assert!(!view.searching);
    assert_eq!(view.releases.len(), 2, "the rows were never dropped");

    // Open it again, type (the field is seeded here), and submit: the call carries the query.
    enter(&mut app, &mut wifi);
    assert!(app.model.store_releases.as_ref().unwrap().searching);
    app.model.store_search = Some(plippa_ui::KeyboardField::from_str("Blue"));
    press(&mut app, &mut wifi, InputEvent::MenuRotate(1)); // trailing blank -> OK
    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_releases
            .as_ref()
            .is_some_and(|view| view.query == "Blue" && view.releases.len() == 1)
    });
    assert!(app.model.store_search.is_none());
    assert_eq!(
        calls.lock().unwrap().collections.last().cloned(),
        Some(("Blue".to_string(), 0, 20))
    );
    assert_eq!(
        app.model.store_releases.as_ref().unwrap().releases[0].id,
        "2",
        "the old rows are gone, not appended to"
    );
}

// ---------------------------------------------------------------------------
// One release: consent, progress, the downloaded mark
// ---------------------------------------------------------------------------

#[test]
fn release_download_goes_consent_then_progress_and_is_marked_downloaded() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.pages = vec![Ok(page(0, &["1234"], 1))];
    mock.release = Some(Ok(release_info()));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-1".into(),
        tracks: 2,
        estimated_bytes: Some(3_000),
        note: "Bandcamp sends a compressed stream, not the purchased FLAC.".into(),
    }));
    mock.session = Some(Ok(DownloadSession {
        session: "s-1".into(),
    }));
    mock.progress = vec![
        Ok(progress(DownloadState::Running, 0, None)),
        Ok(progress(DownloadState::Done, 2, Some("Blue/blue.noudata"))),
    ];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    app.open_releases();
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_releases
            .as_ref()
            .is_some_and(|view| !view.releases.is_empty())
    });
    enter(&mut app, &mut wifi);
    assert_eq!(*app.nav.current(), Route::StoreRelease);
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_release
            .as_ref()
            .is_some_and(|view| view.phase == ReleasePhase::Ready)
    });
    assert_eq!(calls.lock().unwrap().releases, vec!["1234".to_string()]);
    assert_eq!(
        app.model.store_release.as_ref().unwrap().actions(),
        vec![ReleaseAction::Download, ReleaseAction::Back]
    );

    // Download prices it first: nothing is fetched by that press.
    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_release
            .as_ref()
            .is_some_and(|view| matches!(view.download, DownloadPhase::Confirm { .. }))
    });
    assert_eq!(
        calls.lock().unwrap().prepares,
        vec![(String::new(), "1234".to_string())]
    );
    assert!(calls.lock().unwrap().starts.is_empty());

    // The consent press starts it.
    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_release
            .as_ref()
            .is_some_and(|view| view.is_downloaded())
    });
    assert_eq!(calls.lock().unwrap().starts, vec!["c-1".to_string()]);
    assert!(
        !calls.lock().unwrap().statuses.is_empty(),
        "the screen polls for progress"
    );

    let view = app.model.store_release.as_ref().unwrap();
    assert!(view.downloaded.is_some());
    assert_eq!(
        view.actions(),
        vec![
            ReleaseAction::OpenInLibrary,
            ReleaseAction::Remove,
            ReleaseAction::Back
        ],
        "a downloaded release never offers a second download"
    );
    assert_eq!(app.model.store_items.len(), 1);
    assert_eq!(
        app.model.store_items[0].noudata.as_deref(),
        Some("Blue/blue.noudata")
    );
    assert_eq!(app.model.store_items[0].release_id, "1234");

    // Re-opening the same release finds it marked, so the list row leads to the library.
    let marks = app.model.store_items.clone();
    app.model.store_release = None;
    app.model.store_items = marks;
    app.open_release("1234");
    assert!(app
        .model
        .store_release
        .as_ref()
        .is_some_and(|view| view.is_downloaded()));
}

#[test]
fn a_failed_download_offers_a_retry_not_a_lie() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls);
    mock.release = Some(Ok(release_info()));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-1".into(),
        tracks: 2,
        estimated_bytes: None,
        note: String::new(),
    }));
    mock.session = Some(Ok(DownloadSession {
        session: "s-1".into(),
    }));
    mock.progress = vec![Ok(progress(DownloadState::Failed, 0, None))];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    app.open_release("1234");
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_release
            .as_ref()
            .is_some_and(|view| view.phase == ReleasePhase::Ready)
    });
    enter(&mut app, &mut wifi); // Download → consent
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_release
            .as_ref()
            .is_some_and(|view| matches!(view.download, DownloadPhase::Confirm { .. }))
    });
    enter(&mut app, &mut wifi); // Start
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_release
            .as_ref()
            .is_some_and(|view| matches!(view.download, DownloadPhase::Failed { .. }))
    });
    let view = app.model.store_release.as_ref().unwrap();
    assert!(!view.is_downloaded(), "a failed download is never marked placed");
    assert_eq!(
        view.actions(),
        vec![ReleaseAction::Retry, ReleaseAction::Back]
    );
    assert!(app.model.store_items.is_empty());
}

#[test]
fn a_replay_of_a_release_already_in_the_library_is_not_reported_as_a_failure() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls);
    mock.release = Some(Ok(release_info()));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-1".into(),
        tracks: 2,
        estimated_bytes: None,
        note: String::new(),
    }));
    mock.session = Some(Ok(DownloadSession {
        session: "s-1".into(),
    }));
    // The server placed nothing (the release was already there) and said so.
    let mut replay = progress(DownloadState::Done, 0, Some("Blue/blue.noudata"));
    replay.already_present = true;
    mock.progress = vec![Ok(replay)];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    app.open_release("1234");
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_release
            .as_ref()
            .is_some_and(|view| view.phase == ReleasePhase::Ready)
    });
    enter(&mut app, &mut wifi); // Download → consent
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_release
            .as_ref()
            .is_some_and(|view| matches!(view.download, DownloadPhase::Confirm { .. }))
    });
    enter(&mut app, &mut wifi); // Start
    pump_until(&mut app, &mut wifi, |app| {
        app.model.store_release.as_ref().is_some_and(|view| {
            matches!(view.download, DownloadPhase::Done { .. }) || view.is_downloaded()
        })
    });
    let view = app.model.store_release.as_ref().unwrap();
    assert!(
        !matches!(view.download, DownloadPhase::Failed { .. }),
        "a replay found the release in the library; it is not a failure"
    );
    assert!(view.is_downloaded(), "the item is on the device");
    assert_eq!(
        view.actions(),
        vec![
            ReleaseAction::OpenInLibrary,
            ReleaseAction::Remove,
            ReleaseAction::Back
        ]
    );
}

// ---------------------------------------------------------------------------
// Remove from Plippa
// ---------------------------------------------------------------------------

#[test]
fn remove_previews_then_deletes_and_goes_back() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.preview = Some(Ok(preview()));
    mock.delete = Some(Ok(DeleteDone {
        deleted: 10,
        freed_bytes: 200 * 1024 * 1024,
        partial: false,
    }));
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    app.open_remove(7, "Blue", Some("1234".into()));
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_delete
            .as_ref()
            .is_some_and(|view| matches!(view.phase, DeletePhase::Ready(_)))
    });
    assert_eq!(calls.lock().unwrap().previews, vec![7]);
    assert_eq!(
        app.model.store_delete.as_ref().unwrap().actions(),
        vec![DeleteAction::Remove, DeleteAction::Back]
    );

    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |app| {
        app.model
            .store_delete
            .as_ref()
            .is_some_and(|view| view.succeeded())
    });
    assert_eq!(calls.lock().unwrap().deletes, vec![7]);
    assert_eq!(*app.nav.current(), Route::LibraryRemove);

    // Back leaves the confirmation.
    press(&mut app, &mut wifi, InputEvent::Back);
    assert_eq!(*app.nav.current(), Route::Main);
    assert!(app.model.store_delete.is_none());
}

#[test]
fn remove_refusals_each_read_differently() {
    for (error, expected) in [
        (
            StoreApiError::Refused {
                status: 409,
                code: Some("playing".into()),
                message: "this item is playing".into(),
            },
            StoreBlocked::Playing,
        ),
        (
            StoreApiError::Refused {
                status: 409,
                code: Some("blocked".into()),
                message: "a playlist owns it".into(),
            },
            StoreBlocked::Blocked,
        ),
        (
            StoreApiError::Refused {
                status: 400,
                code: Some("last_file".into()),
                message: "the only file".into(),
            },
            StoreBlocked::LastFile,
        ),
        (
            StoreApiError::Offline("connection refused".into()),
            StoreBlocked::ServiceDown,
        ),
    ] {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let mut mock = MockStore::new(calls);
        mock.preview = Some(Ok(preview()));
        mock.delete = Some(Err(error));
        let mut app = app_with(mock);
        let mut wifi = default_mock_wifi();
        app.open_remove(7, "Blue", None);
        pump_until(&mut app, &mut wifi, |app| {
            app.model
                .store_delete
                .as_ref()
                .is_some_and(|view| matches!(view.phase, DeletePhase::Ready(_)))
        });
        enter(&mut app, &mut wifi);
        pump_until(&mut app, &mut wifi, |app| {
            app.model
                .store_delete
                .as_ref()
                .is_some_and(|view| matches!(view.phase, DeletePhase::Failed { .. }))
        });
        let view = app.model.store_delete.as_ref().unwrap();
        match &view.phase {
            DeletePhase::Failed { blocked, message } => {
                assert_eq!(*blocked, expected);
                assert_eq!(blocked.title(), expected.title());
                assert!(!message.is_empty());
                assert!(
                    !message.contains("409") && !message.contains("last_file"),
                    "a code reached the screen: {message}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // Retry is offered where it could work; a blocked item never pretends it can.
        if expected == StoreBlocked::Blocked || expected == StoreBlocked::LastFile {
            assert!(!view.actions().contains(&DeleteAction::Retry));
        }
    }
}

#[test]
fn detail_offers_remove_only_for_a_store_download() {
    let mut model = ScreenModel::default();
    let mut item = LibraryItem {
        id: 1,
        title: "Blue".into(),
        associated: false,
        noudata_relpath: "Blue/blue.noudata".into(),
        library_root: Some(std::path::PathBuf::from("/media/noubin")),
        artist: Some("Joni Mitchell".into()),
        duration_ms: None,
        art_path: None,
        item_position: None,
    };
    model.library = vec![item.clone()];
    model.detail_playable_id = Some(1);

    // An ordinary library item: no Remove row, exactly as before.
    assert!(!model.detail_actions().contains(&LibraryDetailAction::Remove));

    // The release this session downloaded is the one that offers it.
    model.store_items = vec![StoreItem {
        release_id: "1234".into(),
        title: "Blue".into(),
        noudata: Some("Blue/blue.noudata".into()),
        files: 10,
    }];
    assert!(model.detail_actions().contains(&LibraryDetailAction::Remove));
    assert_eq!(
        LibraryDetailAction::Remove.label(),
        "Remove from Plippa",
        "the row names the action in the person's terms"
    );

    // Another item is not swept up by the record.
    item.noudata_relpath = "Hejira/hejira.noudata".into();
    model.library = vec![item];
    assert!(!model.detail_actions().contains(&LibraryDetailAction::Remove));
}

// ---------------------------------------------------------------------------
// 320x240 layout
// ---------------------------------------------------------------------------

#[test]
fn connect_screen_fits_320x240_and_the_qr_stays_scannable() {
    // The longest address this box can plausibly advertise: a /16 LAN host and port.
    let url = "http://192.168.100.200:8787";
    let matrix = plippa_core::qr::QrMatrix::encode(url, plippa_core::qr::QrErrorLevel::Medium)
        .expect("the address encodes");
    let module = plippa_ui::store_qr_module_px(matrix.width, STORE_QR_BOX).expect("a module size");
    assert!(
        module >= STORE_QR_MIN_MODULE_PX,
        "the module must be at least {STORE_QR_MIN_MODULE_PX} px, got {module}"
    );

    let mut app = UiApp::new(UiConfig::default());
    let mut view = ConnectView::looking(0);
    view.apply(Ok(portal(url)));
    app.model.store_connect = Some(view);
    let fb = render_route(&mut app, Route::StoreConnect);

    // Nothing paints past the content box.
    assert_eq!(
        ink_below_content(&fb),
        0,
        "the connect screen painted below CONTENT_BOTTOM"
    );

    // Find the symbol: its quiet-zone row is a solid run of `FG` from the left margin.
    let (qr_top, side) = (CONTENT_TOP..CONTENT_BOTTOM)
        .find_map(|y| {
            let run = fg_run(&fb, MARGIN_X, y);
            (run >= 30).then_some((y, run))
        })
        .expect("the QR code is painted");
    let quiet = u16::try_from(STORE_QR_QUIET_MODULES).unwrap();
    let modules = matrix.width as u16;
    assert!(
        u32::from(side) >= u32::from(modules + quiet * 2) * u32::from(STORE_QR_MIN_MODULE_PX),
        "the painted symbol ({side} px over {modules} modules) is below the 3 px module floor"
    );
    assert!(
        side <= STORE_QR_BOX,
        "the symbol ({side} px) overflows its {STORE_QR_BOX} px box"
    );
    assert!(
        qr_top.saturating_add(side) <= CONTENT_BOTTOM,
        "the symbol runs into the action rows"
    );

    // The instruction / address column starts after the symbol: the gap between them is the
    // ground, so the two widgets never overlap.
    let gap_start = MARGIN_X.saturating_add(side);
    for y in qr_top..qr_top.saturating_add(side) {
        for x in gap_start..gap_start.saturating_add(SPACE_2).min(SCREEN_W) {
            assert_eq!(
                fb.pixels[y as usize * SCREEN_W as usize + x as usize],
                BG,
                "the QR and the text column overlap at ({x}, {y})"
            );
        }
    }
    // And the address really is beside it, not hidden under it.
    let right_x = gap_start.saturating_add(SPACE_2);
    let right_ink = (qr_top..qr_top.saturating_add(side))
        .map(|y| {
            (right_x..SCREEN_W)
                .filter(|&x| is_ink(fb.pixels[y as usize * SCREEN_W as usize + x as usize]))
                .count()
        })
        .sum::<usize>();
    assert!(right_ink > 0, "the address is drawn beside the QR");
}

#[test]
fn release_list_truncates_a_long_title_without_overlapping_its_value() {
    const LONG: &str = "A Very Long Bandcamp Release Title That Cannot Possibly Fit On One Row Of A Three Hundred And Twenty Pixel Wide Screen, Not Even Close";
    // Arithmetic: at Small (6 px per cell) the title needs more columns than the screen has.
    assert!(measure(LONG, TypeRole::Small) > SCREEN_W);
    assert!(screen_text(LONG).chars().count() > 60);

    let mut app = UiApp::new(UiConfig::default());
    let mut view = ReleasesView::loading(0);
    view.apply_first(Ok(CollectionPageInfo {
        total: 2,
        offset: 0,
        stale: false,
        synced_at: None,
        releases: vec![release_row("1234", LONG), release_row("2", "Hejira")],
    }));
    app.model.store_releases = Some(view);
    app.model.store_items = vec![StoreItem {
        release_id: "1234".into(),
        title: LONG.into(),
        noudata: Some("Blue/blue.noudata".into()),
        files: 10,
    }];
    let fb = render_route(&mut app, Route::StoreReleases);

    assert_eq!(
        ink_below_content(&fb),
        0,
        "the release list painted below CONTENT_BOTTOM"
    );

    // The first row is focused: its ground is SURFACE. Read the row's geometry from the same
    // tokens the widget uses rather than a guessed offset.
    let row_y = (CONTENT_TOP..CONTENT_BOTTOM)
        .find(|&y| fb.pixels[y as usize * SCREEN_W as usize + 1] == SURFACE)
        .expect("the focused release row is painted");
    let label_x = MARGIN_X + SPACE_2; // the "on device" marker indents the label by 8 px
    let value = "on device";
    let value_px = measure(value, TypeRole::Small) + SPACE_2;
    let label_w = SCREEN_W - label_x - MARGIN_X - value_px;
    let label_end = label_x + label_w;
    let value_start = SCREEN_W - MARGIN_X - measure(value, TypeRole::Small);
    assert!(
        label_end < value_start,
        "the value column is reserved before the label is clipped"
    );
    assert!(
        measure(LONG, TypeRole::Small) > label_w,
        "the longest title must be clipped to fit its column"
    );

    // Inside the row (away from the focus ring) nothing is painted between the label's box
    // and the value's column: truncation happens inside the label, never over the value.
    for y in (row_y + 4)..row_y.saturating_add(ROW_H).saturating_sub(4) {
        for x in label_end..value_start {
            let pixel = fb.pixels[y as usize * SCREEN_W as usize + x as usize];
            assert!(
                !is_ink(pixel),
                "ink at ({x}, {y}) between the title and its value"
            );
        }
    }
    // The value column carries ink, and the title starts inside its own column.
    let value_ink = ((row_y + 4)..row_y.saturating_add(ROW_H).saturating_sub(4))
        .map(|y| {
            (value_start..SCREEN_W)
                .filter(|&x| is_ink(fb.pixels[y as usize * SCREEN_W as usize + x as usize]))
                .count()
        })
        .sum::<usize>();
    assert!(value_ink > 0, "the on-device marker is drawn");
    let first_label_ink = (label_x..value_start)
        .find(|&x| {
            ((row_y + 4)..row_y.saturating_add(ROW_H).saturating_sub(4))
                .any(|y| is_ink(fb.pixels[y as usize * SCREEN_W as usize + x as usize]))
        })
        .expect("the title is painted");
    assert!(
        first_label_ink < label_x + advance(TypeRole::Small) + 6,
        "the title starts at its column ({first_label_ink})"
    );
    // Nothing bleeds into the outer margin: the row's own ground stops at the ring.
    for y in (row_y + 4)..row_y.saturating_add(ROW_H).saturating_sub(4) {
        for x in 0..MARGIN_X / 2 {
            assert!(
                !is_ink(fb.pixels[y as usize * SCREEN_W as usize + x as usize]),
                "ink in the outer margin at ({x}, {y})"
            );
        }
    }
}

#[test]
fn release_detail_fits_320x240_with_its_actions_at_the_bottom() {
    let mut app = UiApp::new(UiConfig::default());
    let mut view = ReleaseView::loading("1234", 0);
    view.apply_info(Ok(release_info()));
    app.model.store_release = Some(view);
    let fb = render_route(&mut app, Route::StoreRelease);

    assert_eq!(
        ink_below_content(&fb),
        0,
        "the release screen painted below CONTENT_BOTTOM"
    );
    // Two action rows are pinned to the bottom and the first one carries the focus ring.
    let actions_top = CONTENT_BOTTOM - 2 * ROW_H;
    let ring = (SPACE_2..SCREEN_W - SPACE_2)
        .filter(|&x| {
            let pixel = fb.pixels[actions_top as usize * SCREEN_W as usize + x as usize];
            pixel == plippa_ui::ui::tokens::ACCENT
        })
        .count();
    assert!(ring > 200, "the focused action row is ringed ({ring} px)");
    // The body stops above the action block, so a long track list cannot cover it.
    assert!(actions_top > CONTENT_TOP + ROW_H);
}

#[test]
fn every_store_state_paints_inside_the_content_box() {
    let mut app = UiApp::new(UiConfig::default());

    // Stores: loading, ready, blocked.
    app.model.store_stores = Some(StoresView::loading(0));
    assert_eq!(ink_below_content(&render_route(&mut app, Route::SystemStores)), 0);
    let mut view = StoresView::loading(0);
    view.apply(Ok(stores_info(true)));
    app.model.store_stores = Some(view);
    assert_eq!(ink_below_content(&render_route(&mut app, Route::SystemStores)), 0);
    let mut view = StoresView::loading(0);
    view.apply(Err(StoreApiError::Offline("refused".into())));
    assert!(view.blocked().is_some());
    app.model.store_stores = Some(view);
    assert_eq!(ink_below_content(&render_route(&mut app, Route::SystemStores)), 0);

    // Connect: looking, ready-with-no-address, unreachable.
    app.model.store_connect = Some(ConnectView::looking(0));
    assert_eq!(ink_below_content(&render_route(&mut app, Route::StoreConnect)), 0);
    let mut view = ConnectView::looking(0);
    view.apply(Ok(PortalAddress::default()));
    assert!(view.qr_url().is_none());
    assert!(view.note().is_some());
    app.model.store_connect = Some(view);
    assert_eq!(ink_below_content(&render_route(&mut app, Route::StoreConnect)), 0);

    // Releases: loading, blocked, page-failed, empty, and a two-page list.
    app.model.store_releases = Some(ReleasesView::loading(0));
    assert_eq!(ink_below_content(&render_route(&mut app, Route::StoreReleases)), 0);
    let mut view = ReleasesView::loading(0);
    view.apply_first(Err(StoreApiError::Offline("refused".into())));
    assert!(matches!(view.phase, ReleasesPhase::Blocked { .. }));
    app.model.store_releases = Some(view);
    assert_eq!(ink_below_content(&render_route(&mut app, Route::StoreReleases)), 0);
    let mut view = ReleasesView::loading(0);
    view.apply_first(Ok(page(0, &["1", "2"], 9)));
    view.apply_more(2, Err(StoreApiError::Offline("dropped".into())));
    assert!(matches!(view.phase, ReleasesPhase::PageFailed { .. }));
    app.model.store_releases = Some(view);
    assert_eq!(ink_below_content(&render_route(&mut app, Route::StoreReleases)), 0);

    // Release: loading, blocked, confirm, running, done, failed.
    app.model.store_release = Some(ReleaseView::loading("1", 0));
    assert_eq!(ink_below_content(&render_route(&mut app, Route::StoreRelease)), 0);
    let mut view = ReleaseView::loading("1", 0);
    view.apply_info(Err(StoreApiError::Refused {
        status: 429,
        code: Some("busy".into()),
        message: "slow down".into(),
    }));
    app.model.store_release = Some(view);
    assert_eq!(ink_below_content(&render_route(&mut app, Route::StoreRelease)), 0);
    for download in [
        DownloadPhase::Confirm {
            consent: DownloadConsent {
                consent_id: "c".into(),
                tracks: 10,
                estimated_bytes: Some(200 * 1024 * 1024),
                note: "Bandcamp sends a compressed stream.".into(),
            },
        },
        DownloadPhase::Running {
            session: "s".into(),
            progress: progress(DownloadState::Running, 0, None),
        },
        DownloadPhase::Done {
            files: 10,
            partial: false,
        },
        DownloadPhase::Failed {
            message: "The download stopped.".into(),
            retry: true,
        },
    ] {
        let mut view = ReleaseView::loading("1", 0);
        view.apply_info(Ok(release_info()));
        view.download = download;
        app.model.store_release = Some(view);
        assert_eq!(ink_below_content(&render_route(&mut app, Route::StoreRelease)), 0);
    }

    // Remove: loading, ready, refused, deleting, done, failed.
    app.model.store_delete = Some(DeleteView::loading(7, "Blue", None, 0));
    assert_eq!(ink_below_content(&render_route(&mut app, Route::LibraryRemove)), 0);
    let states = [
        DeletePhase::Ready(Box::new(preview())),
        DeletePhase::Refused {
            blocked: StoreBlocked::Playing,
            message: "this item is playing".into(),
            preview: None,
        },
        DeletePhase::Deleting,
        DeletePhase::Done(DeleteDone {
            deleted: 10,
            freed_bytes: 1,
            partial: false,
        }),
        DeletePhase::Failed {
            blocked: StoreBlocked::LastFile,
            message: "the only file".into(),
        },
    ];
    for phase in states {
        let mut view = DeleteView::loading(7, "Blue", None, 0);
        view.phase = phase;
        app.model.store_delete = Some(view);
        assert_eq!(ink_below_content(&render_route(&mut app, Route::LibraryRemove)), 0);
    }
}

// ---------------------------------------------------------------------------
// BCS_11: resync the collection from the Stores screen
// ---------------------------------------------------------------------------

fn sync_status(state: plippa_ui::SyncPhase, pages: u32, releases: u32) -> plippa_ui::SyncStatus {
    plippa_ui::SyncStatus {
        state,
        pages,
        releases,
        ..plippa_ui::SyncStatus::default()
    }
}

fn stores_view(app: &UiApp) -> &StoresView {
    app.model.store_stores.as_ref().expect("the Stores screen is open")
}

#[test]
fn stores_screen_resyncs_shows_progress_and_reports_the_result() {
    use plippa_ui::{StoresAction, SyncPhase};
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.stores = vec![Ok(stores_info(true)), Ok(stores_info(true))];
    let mut synced = page(0, &["1"], 1);
    synced.synced_at = Some(1_700_000_000);
    mock.pages = vec![Ok(synced)];
    mock.sync_start = Some(Ok(sync_status(SyncPhase::Running, 0, 0)));
    mock.sync_status = vec![
        Ok(sync_status(SyncPhase::Idle, 0, 0)), // the probe on entering the screen
        Ok(sync_status(SyncPhase::Running, 3, 120)),
        Ok(sync_status(SyncPhase::Done, 6, 240)),
    ];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    app.open_stores(Route::LibraryStores);
    pump_until(&mut app, &mut wifi, stores_ready);
    // The status line knows when the cache was last written.
    pump_until(&mut app, &mut wifi, |app| stores_view(app).synced_at == Some(1_700_000_000));
    assert!(stores_view(&app).actions().contains(&StoresAction::Sync));

    // Down to `Resync library` and press it once.
    down(&mut app, &mut wifi);
    assert_eq!(stores_view(&app).actions()[app.nav.focus], StoresAction::Sync);
    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |_| calls.lock().unwrap().sync_starts == 1);

    // Progress shows in the row's value column while it runs; the row cannot be chosen.
    pump_until(&mut app, &mut wifi, |app| {
        stores_view(app).sync_value().as_deref() == Some("Syncing… 3/120")
    });
    assert!(!stores_view(&app).enabled(StoresAction::Sync));
    app.nav.focus = 1; // forced onto the busy row: Enter there does nothing
    enter(&mut app, &mut wifi);
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(calls.lock().unwrap().sync_starts, 1, "one sync at a time");
    // Turning the encoder steps over the busy row.
    app.nav.focus = 0;
    down(&mut app, &mut wifi);
    assert_eq!(
        stores_view(&app).actions()[app.nav.focus],
        StoresAction::Connect,
        "Sync is skipped while it runs"
    );

    // Done: the flash names the count, and the polling stops.
    pump_until(&mut app, &mut wifi, |app| {
        app.compositor
            .status()
            .is_some_and(|s| s.lines().iter().any(|l| l.contains("Collection synced: 240 releases")))
    });
    assert!(!stores_view(&app).sync_polling);
    assert!(stores_view(&app).enabled(StoresAction::Sync));
    let polls = calls.lock().unwrap().sync_statuses;
    for step in 0..200u64 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        app.handle_input(InputEvent::Tick { now_ms: 40_000 + step * 16 }, &mut wifi);
    }
    // The refresh after `done` asks the status once more; after that nothing polls.
    assert!(
        calls.lock().unwrap().sync_statuses <= polls + 1,
        "a terminal state ends the poll"
    );
}

#[test]
fn stores_screen_failed_sync_states_why_and_can_be_retried() {
    use plippa_ui::{StoresAction, SyncPhase};
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.stores = vec![Ok(stores_info(true))];
    mock.pages = vec![Ok(page(0, &["1"], 1))];
    mock.sync_start = Some(Ok(sync_status(SyncPhase::Running, 0, 0)));
    let mut failed = sync_status(SyncPhase::Failed, 2, 0);
    failed.error = Some("Bandcamp is not answering.".into());
    mock.sync_status = vec![Ok(sync_status(SyncPhase::Idle, 0, 0)), Ok(failed)];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    app.open_stores(Route::LibraryStores);
    pump_until(&mut app, &mut wifi, stores_ready);
    down(&mut app, &mut wifi);
    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |app| stores_view(app).sync_failure().is_some());
    assert_eq!(
        stores_view(&app).sync_failure().as_deref(),
        Some("Bandcamp is not answering.")
    );
    assert!(stores_view(&app).enabled(StoresAction::Sync), "a retry is a normal action");
    // And painting that state works.
    let _ = render_route(&mut app, Route::LibraryStores);

    // The retry starts a new sync.
    app.nav.focus = 1;
    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |_| calls.lock().unwrap().sync_starts == 2);
}

#[test]
fn stores_screen_does_not_poll_a_sync_while_it_is_not_the_screen_showing() {
    use plippa_ui::SyncPhase;
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.stores = vec![Ok(stores_info(true))];
    mock.pages = vec![Ok(page(0, &["1"], 1))];
    mock.sync_start = Some(Ok(sync_status(SyncPhase::Running, 0, 0)));
    mock.sync_status = vec![Ok(sync_status(SyncPhase::Running, 1, 1))];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    app.open_stores(Route::LibraryStores);
    pump_until(&mut app, &mut wifi, stores_ready);
    down(&mut app, &mut wifi);
    enter(&mut app, &mut wifi);
    pump_until(&mut app, &mut wifi, |_| calls.lock().unwrap().sync_statuses >= 2);

    // Leave the screen: the server carries on, the device stops asking.
    press(&mut app, &mut wifi, InputEvent::Back);
    assert!(app.model.store_stores.is_none());
    let polls = calls.lock().unwrap().sync_statuses;
    for step in 0..200u64 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        app.handle_input(InputEvent::Tick { now_ms: 60_000 + step * 16 }, &mut wifi);
    }
    assert_eq!(calls.lock().unwrap().sync_statuses, polls, "no polling off screen");

    // Re-entering reads the server's state again, and picks the running sync back up.
    app.open_stores(Route::LibraryStores);
    pump_until(&mut app, &mut wifi, |app| stores_view(app).syncing());
}
