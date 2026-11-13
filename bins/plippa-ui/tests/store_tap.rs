//! BCS_07: the store tap screen, driven through the in-process UI harness.
//!
//! The device is off, so none of this touches hardware or a real server: a scripted
//! [`StoreApi`] stands in for the local service and the app is fed ordinary input events.
//! The assertions are about **decisions and calls**, never pixels (one exception the work
//! package explicitly asks for: the QR module size is arithmetic, so it is asserted as
//! arithmetic).

use std::sync::{Arc, Mutex};

use plippa_ui::{
    default_mock_wifi, store_qr_module_px, store_short_url, DownloadConsent, DownloadProgress,
    DownloadSession, DownloadState, FrameBuffer, InputEvent, MockWifi, Route,
    screen_text, CoverBytes, StoreApi, StoreApiError, StoreTapPhase, StoreTapView, SyncPhase,
    SyncStatus, TapAnswer, TapBlocked, TapCandidate, TapRelease, TapState, UiApp, UiConfig,
    STORE_QR_BOX, STORE_QR_MIN_MODULE_PX,
};

// ---------------------------------------------------------------------------
// A scripted server
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Calls {
    taps: Vec<(String, bool)>,
    wants: Vec<String>,
    prepares: Vec<(String, String)>,
    starts: Vec<String>,
    statuses: Vec<String>,
    cancels: Vec<String>,
    sync_starts: usize,
    sync_statuses: usize,
    covers: Vec<String>,
}

struct MockStore {
    calls: Arc<Mutex<Calls>>,
    tap: Option<Result<TapAnswer, StoreApiError>>,
    consent: Option<Result<DownloadConsent, StoreApiError>>,
    session: Option<Result<DownloadSession, StoreApiError>>,
    /// Answers in order; the last one repeats so a poll never runs dry.
    progress: Vec<Result<DownloadProgress, StoreApiError>>,
    /// Tap answers in order, consumed one per call; once empty `tap` answers.
    tap_queue: Vec<Result<TapAnswer, StoreApiError>>,
    /// What `sync_start` answers.
    sync_start: Option<Result<SyncStatus, StoreApiError>>,
    /// Answers to `sync_status` in order; the last one repeats.
    sync_status: Vec<Result<SyncStatus, StoreApiError>>,
    /// What `release_cover` answers.
    cover: Option<Result<CoverBytes, StoreApiError>>,
}

impl MockStore {
    fn new(calls: Arc<Mutex<Calls>>) -> Self {
        Self {
            calls,
            tap: None,
            consent: None,
            session: None,
            progress: Vec::new(),
            tap_queue: Vec::new(),
            sync_start: None,
            sync_status: Vec::new(),
            cover: None,
        }
    }
}

impl StoreApi for MockStore {
    fn tap(
        &mut self,
        url: &str,
        _expected_duration_s: Option<u64>,
        consent: bool,
    ) -> Result<TapAnswer, StoreApiError> {
        self.calls
            .lock()
            .unwrap()
            .taps
            .push((url.to_string(), consent));
        if !self.tap_queue.is_empty() {
            return self.tap_queue.remove(0);
        }
        self.tap
            .clone()
            .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
    }

    fn want(&mut self, url: &str) -> Result<(), StoreApiError> {
        self.calls.lock().unwrap().wants.push(url.to_string());
        Ok(())
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
        if self.progress.len() > 1 {
            self.progress.remove(0)
        } else {
            self.progress
                .first()
                .cloned()
                .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
        }
    }

    fn download_cancel(&mut self, session: &str) -> Result<(), StoreApiError> {
        self.calls
            .lock()
            .unwrap()
            .cancels
            .push(session.to_string());
        Ok(())
    }

    // BCS_08's screens are exercised by `tests/store_browse.rs`; this mock is the tap flow's
    // and answers them honestly rather than pretending to have a server.
    fn portal_address(&mut self) -> Result<plippa_ui::PortalAddress, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn stores(&mut self) -> Result<plippa_ui::StoresInfo, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn collection(
        &mut self,
        _query: &str,
        _offset: u32,
        _size: u32,
    ) -> Result<plippa_ui::CollectionPageInfo, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn release(&mut self, _release_id: &str) -> Result<plippa_ui::ReleaseInfo, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn delete_preview(
        &mut self,
        _playable_id: u64,
    ) -> Result<plippa_ui::DeletePreviewInfo, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn delete_playable(&mut self, _playable_id: u64) -> Result<plippa_ui::DeleteDone, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn sync_start(&mut self) -> Result<SyncStatus, StoreApiError> {
        self.calls.lock().unwrap().sync_starts += 1;
        self.sync_start
            .clone()
            .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
    }

    fn sync_status(&mut self) -> Result<SyncStatus, StoreApiError> {
        self.calls.lock().unwrap().sync_statuses += 1;
        if self.sync_status.len() > 1 {
            self.sync_status.remove(0)
        } else {
            self.sync_status
                .first()
                .cloned()
                .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
        }
    }

    fn sync_cancel(&mut self) -> Result<SyncStatus, StoreApiError> {
        Err(StoreApiError::Offline("not scripted".into()))
    }

    fn release_cover(&mut self, release_id: &str) -> Result<CoverBytes, StoreApiError> {
        self.calls.lock().unwrap().covers.push(release_id.to_string());
        self.cover
            .clone()
            .unwrap_or_else(|| Err(StoreApiError::Offline("not scripted".into())))
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
// Fixtures
// ---------------------------------------------------------------------------

fn candidate(id: &str, title: &str, artist: &str, confidence: f32) -> TapCandidate {
    TapCandidate {
        store_release_id: id.to_string(),
        title: title.to_string(),
        artist: artist.to_string(),
        track_count: Some(10),
        confidence,
    }
}

fn owned_answer() -> TapAnswer {
    TapAnswer {
        state: TapState::OwnedDownloadable,
        blocked: None,
        candidates: vec![candidate("1234", "Blue", "Joni Mitchell", 0.97)],
        release: None,
        owned_path: None,
        purchase_url: None,
        message: "You own this on Bandcamp (10 tracks). Download it to this Plippa?".into(),
        cache_stale: false,
        requires_confirmation: false,
        confidence: 0.97,
        lookup: None,
    }
}

fn not_owned_answer() -> TapAnswer {
    TapAnswer {
        state: TapState::NotOwned,
        blocked: None,
        candidates: Vec::new(),
        release: None,
        owned_path: None,
        purchase_url: Some("https://joni.bandcamp.com/album/blue".into()),
        message: "You do not own this on Bandcamp.".into(),
        cache_stale: false,
        requires_confirmation: false,
        confidence: 0.0,
        lookup: None,
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

fn app_with(mock: MockStore) -> UiApp {
    let mut app = UiApp::new(UiConfig::default());
    app.set_store_api(Box::new(mock));
    app
}

fn tap(app: &mut UiApp, wifi: &mut MockWifi, url: &str) {
    assert!(
        app.open_store_tap(
            &[url.to_string()],
            Some("https://joni.example/blue/".into()),
            ""
        ),
        "the tap screen opens for a declared store link"
    );
    pump_until(app, wifi, |app| {
        !matches!(
            app.model.store_tap.as_ref().map(|v| &v.phase),
            Some(StoreTapPhase::Lookup)
        )
    });
}

/// Tick the app until `done` holds (bounded), so worker-thread timing never flakes a test.
fn pump_until(app: &mut UiApp, wifi: &mut MockWifi, done: impl Fn(&UiApp) -> bool) {
    for step in 0..600u64 {
        if done(app) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
        app.handle_input(InputEvent::Tick { now_ms: step * 16 }, wifi);
    }
    panic!(
        "condition not reached; phase = {:?}",
        app.model.store_tap.as_ref().map(|v| v.phase.clone())
    );
}

fn press(app: &mut UiApp, wifi: &mut MockWifi, ev: InputEvent) {
    app.handle_input(ev, wifi);
}

fn phase(app: &UiApp) -> StoreTapPhase {
    app.model
        .store_tap
        .as_ref()
        .expect("the tap screen is open")
        .phase
        .clone()
}

fn actions(app: &UiApp) -> Vec<plippa_ui::StoreTapAction> {
    app.model
        .store_tap
        .as_ref()
        .expect("the tap screen is open")
        .actions()
}

// ---------------------------------------------------------------------------
// The five decision states and the download
// ---------------------------------------------------------------------------

#[test]
fn tap_flow_owned_shows_download_then_progress_and_complete() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap = Some(Ok(owned_answer()));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-1".into(),
        tracks: 10,
        estimated_bytes: Some(200 * 1024 * 1024),
        note: "Bandcamp sends a compressed stream, not the purchased FLAC.".into(),
    }));
    mock.session = Some(Ok(DownloadSession {
        session: "s-1".into(),
    }));
    mock.progress = vec![
        Ok(progress(DownloadState::Running, 0, None)),
        Ok(progress(DownloadState::Done, 10, Some("Blue/blue.noudata"))),
    ];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");

    // The owned state offers Download first, then Cancel.
    match phase(&app) {
        StoreTapPhase::Owned { ref release, .. } => {
            assert_eq!(release.store_release_id, "1234");
            assert_eq!(release.title, "Blue");
        }
        other => panic!("expected the owned state, got {other:?}"),
    }
    assert_eq!(
        actions(&app),
        vec![plippa_ui::StoreTapAction::Download, plippa_ui::StoreTapAction::Back]
    );

    // Download → the server is asked what it would fetch, then it is started.
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Confirm { .. })
    });
    {
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.prepares,
            vec![(
                "https://joni.example/blue/".to_string(),
                "1234".to_string()
            )],
            "the tagged Noubin key is what the download is united against"
        );
    }
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Downloading { .. })
    });
    assert_eq!(calls.lock().unwrap().starts, vec!["c-1".to_string()]);

    // Polling reaches the placed release and offers Play first.
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Complete { .. })
    });
    match phase(&app) {
        StoreTapPhase::Complete {
            files,
            ref noudata,
            partial,
        } => {
            assert_eq!(files, 10);
            assert_eq!(noudata.as_deref(), Some("Blue/blue.noudata"));
            assert!(!partial);
        }
        other => panic!("expected Complete, got {other:?}"),
    }
    assert_eq!(actions(&app)[0], plippa_ui::StoreTapAction::Play);
    assert!(
        !calls.lock().unwrap().statuses.is_empty(),
        "the screen polls for progress"
    );
    assert!(calls.lock().unwrap().cancels.is_empty());
}

#[test]
fn tap_flow_not_owned_shows_purchase_page_and_records_interest() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap = Some(Ok(not_owned_answer()));
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");

    let url = match phase(&app) {
        StoreTapPhase::NotOwned {
            ref url,
            interest_saved,
            from_pick,
            ..
        } => {
            assert!(!interest_saved);
            assert!(!from_pick);
            url.clone()
        }
        other => panic!("expected NotOwned, got {other:?}"),
    };
    assert_eq!(url, "https://joni.bandcamp.com/album/blue");
    // The QR is only offered when a module is at least STORE_QR_MIN_MODULE_PX; assert the
    // arithmetic for the real link rather than a picture.
    let matrix = plippa_core::qr::QrMatrix::encode(
        &url,
        plippa_core::qr::QrErrorLevel::Medium,
    )
    .expect("a Bandcamp release link fits a QR symbol");
    let module = store_qr_module_px(matrix.width, STORE_QR_BOX)
        .expect("the purchase link still scans at the minimum module size");
    assert!(
        module >= STORE_QR_MIN_MODULE_PX,
        "module {module} px is below the {STORE_QR_MIN_MODULE_PX} px floor"
    );
    assert_eq!(
        store_short_url(&url),
        "joni.bandcamp.com/album/blue",
        "the link under the QR is the same page without the scheme"
    );
    // The primary action is Buy, and the screen paints without panicking.
    assert_eq!(actions(&app)[0], plippa_ui::StoreTapAction::Buy);
    let mut fb = FrameBuffer::new(320, 240);
    app.model.render_screen(&app.nav, &mut fb);

    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(
            phase(app),
            StoreTapPhase::NotOwned {
                interest_saved: true,
                ..
            }
        )
    });
    assert_eq!(
        calls.lock().unwrap().wants,
        vec!["https://joni.bandcamp.com/album/blue".to_string()]
    );
}

#[test]
fn tap_flow_ambiguous_offers_at_most_three_candidates() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls);
    let mut answer = not_owned_answer();
    answer.state = TapState::Ambiguous;
    answer.purchase_url = None;
    answer.candidates = vec![
        candidate("1", "Blue", "Joni Mitchell", 0.8),
        candidate("2", "Blue (Live)", "Joni Mitchell", 0.7),
        candidate("3", "Blue Sessions", "Joni Mitchell", 0.6),
        // The server never sends more than three; a fourth must still be dropped here.
        candidate("4", "Blue Again", "Someone Else", 0.5),
    ];
    mock.tap = Some(Ok(answer));
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");

    match phase(&app) {
        StoreTapPhase::Ambiguous { ref candidates, .. } => {
            assert_eq!(candidates.len(), 3, "three rows, never four");
            assert_eq!(candidates[0].store_release_id, "1");
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
    // Three picks plus Cancel.
    assert_eq!(actions(&app).len(), 4);
    assert_eq!(actions(&app)[3], plippa_ui::StoreTapAction::Back);

    // Turning left moves down the list (UI_16 §7) and picking row 2 prices that release.
    press(&mut app, &mut wifi, InputEvent::MenuRotate(-1));
    assert_eq!(app.model.store_tap.as_ref().unwrap().focus, 1);
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(
            phase(app),
            StoreTapPhase::Preparing | StoreTapPhase::Blocked { .. } | StoreTapPhase::Failed { .. }
        )
    });
    assert_eq!(
        app.model.store_tap.as_ref().unwrap().pending_release,
        Some(TapRelease {
            store_release_id: "2".into(),
            title: "Blue (Live)".into(),
            artist: "Joni Mitchell".into(),
            track_count: Some(10),
        }),
        "the picked release is the one being priced"
    );
}

#[test]
fn tap_flow_low_confidence_asks_before_downloading() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    let mut answer = not_owned_answer();
    answer.state = TapState::Ambiguous;
    answer.requires_confirmation = true;
    answer.candidates = vec![candidate("42", "Hejira", "Joni Mitchell", 0.62)];
    mock.tap = Some(Ok(answer));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-2".into(),
        tracks: 9,
        estimated_bytes: None,
        note: String::new(),
    }));
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/hejira");

    match phase(&app) {
        StoreTapPhase::LowConfidence { ref candidate, .. } => {
            assert_eq!(candidate.store_release_id, "42");
        }
        other => panic!("expected LowConfidence, got {other:?}"),
    }
    // Confirm / Not mine / Cancel: nothing starts until the person says which.
    assert_eq!(actions(&app).len(), 3);
    assert!(calls.lock().unwrap().prepares.is_empty());

    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Confirm { .. })
    });
    assert_eq!(
        calls.lock().unwrap().prepares,
        vec![("https://joni.example/blue/".to_string(), "42".to_string())]
    );
}

#[test]
fn tap_flow_blocked_states_say_what_is_wrong() {
    // 1. No store service configured: a plain sentence, no spinner.
    let mut app = UiApp::new(UiConfig::default());
    let mut wifi = default_mock_wifi();
    assert!(app.open_store_tap(
        &["https://joni.bandcamp.com/album/blue".to_string()],
        None,
        ""
    ));
    match phase(&app) {
        StoreTapPhase::Blocked { blocked, ref message } => {
            assert_eq!(blocked, TapBlocked::NoStore);
            assert!(message.contains("web portal"), "got {message:?}");
        }
        other => panic!("expected a blocked state, got {other:?}"),
    }
    // The screen paints and Back leaves it.
    let mut fb = FrameBuffer::new(320, 240);
    app.model.render_screen(&app.nav, &mut fb);
    press(&mut app, &mut wifi, InputEvent::Back);
    assert_eq!(*app.nav.current(), Route::Main);
    assert!(app.model.store_tap.is_none());

    // 2. The service is unreachable: Offline, with a Retry under the cursor.
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls);
    mock.tap = Some(Err(StoreApiError::Offline("connection refused".into())));
    let mut app = app_with(mock);
    assert!(app.open_store_tap(
        &["https://joni.bandcamp.com/album/blue".to_string()],
        None,
        ""
    ));
    pump_until(&mut app, &mut wifi, |app| {
        matches!(
            phase(app),
            StoreTapPhase::Blocked {
                blocked: TapBlocked::Offline,
                ..
            }
        )
    });
    assert_eq!(actions(&app)[0], plippa_ui::StoreTapAction::Retry);

    // 3. Rejected credentials and a consent gate each keep their own sentence.
    for (answer, expected) in [
        (
            TapAnswer::blocked(
                TapBlocked::CredentialsRejected,
                "Bandcamp refused the stored sign-in. Fix it in the web portal.",
            ),
            TapBlocked::CredentialsRejected,
        ),
        (
            TapAnswer::blocked(TapBlocked::ConsentNeeded, "Web lookups are off."),
            TapBlocked::ConsentNeeded,
        ),
    ] {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let mut mock = MockStore::new(calls);
        mock.tap = Some(Ok(answer));
        let mut app = app_with(mock);
        assert!(app.open_store_tap(
            &["https://joni.bandcamp.com/album/blue".to_string()],
            None,
            ""
        ));
        pump_until(&mut app, &mut wifi, |app| {
            matches!(phase(app), StoreTapPhase::Blocked { .. })
        });
        match phase(&app) {
            StoreTapPhase::Blocked { blocked, ref message } => {
                assert_eq!(blocked, expected);
                assert!(!message.is_empty());
                assert!(!message.contains("E_") && !message.contains("code "), "{message:?}");
            }
            other => panic!("expected a blocked state, got {other:?}"),
        }
    }
}

/// BCS_11 §C1: Enter on a running download does nothing at all. The only way out is Back,
/// which cancels on the way out (`tap_flow_leaving_the_screen_cancels_a_running_download`).
#[test]
fn tap_flow_enter_on_a_running_download_never_cancels() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap = Some(Ok(owned_answer()));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-3".into(),
        tracks: 10,
        estimated_bytes: None,
        note: String::new(),
    }));
    mock.session = Some(Ok(DownloadSession {
        session: "s-3".into(),
    }));
    mock.progress = vec![Ok(progress(DownloadState::Running, 0, None))];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Confirm { .. })
    });
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Downloading { .. })
    });

    // The only row is Back, and Enter has nothing focused: pressing (or holding) it is a no-op.
    assert_eq!(actions(&app), vec![plippa_ui::StoreTapAction::Back]);
    assert_eq!(app.model.store_tap.as_ref().unwrap().focused_action(), None);
    for _ in 0..5 {
        press(&mut app, &mut wifi, InputEvent::MenuEnter);
    }
    // Let a few polls run: the download is still going and nothing was cancelled.
    let polls = calls.lock().unwrap().statuses.len();
    pump_until(&mut app, &mut wifi, |_| calls.lock().unwrap().statuses.len() > polls + 1);
    assert!(matches!(phase(&app), StoreTapPhase::Downloading { .. }));
    assert!(calls.lock().unwrap().cancels.is_empty(), "Enter never cancels");
}

#[test]
fn tap_flow_complete_plays_the_new_release_then_can_show_it_in_the_library() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap = Some(Ok(owned_answer()));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-5".into(),
        tracks: 10,
        estimated_bytes: None,
        note: String::new(),
    }));
    mock.session = Some(Ok(DownloadSession {
        session: "s-5".into(),
    }));
    mock.progress = vec![Ok(progress(
        DownloadState::Done,
        10,
        Some("Blue/blue.noudata"),
    ))];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");
    let landed = |app: &UiApp| matches!(phase(app), StoreTapPhase::Complete { .. });
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Confirm { .. })
    });
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, landed);

    // Play asks the index for the placed `.noudata` (the screen cannot guess the item id) and
    // leaves the screen behind.
    let _ = app.take_browse_requests();
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    assert!(app.model.store_tap.is_none(), "Play closes the screen");
    assert!(
        app.take_browse_requests()
            .contains(&plippa_ui::BrowseRequest::FindForPlay {
                relpath: "Blue/blue.noudata".into()
            }),
        "Play resolves the downloaded release through the library index"
    );

    // The same completion, taking "Show in library" instead, lands on the library root.
    let mut mock = MockStore::new(calls.clone());
    mock.tap = Some(Ok(owned_answer()));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-6".into(),
        tracks: 10,
        estimated_bytes: None,
        note: String::new(),
    }));
    mock.session = Some(Ok(DownloadSession {
        session: "s-6".into(),
    }));
    mock.progress = vec![Ok(progress(
        DownloadState::Done,
        10,
        Some("Blue/blue.noudata"),
    ))];
    let mut app = app_with(mock);
    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Confirm { .. })
    });
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Complete { .. })
    });
    assert_eq!(actions(&app)[1], plippa_ui::StoreTapAction::ShowInLibrary);
    // Rotate counts up the list (`menu_step` negates the raw detent).
    press(&mut app, &mut wifi, InputEvent::MenuRotate(-1));
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    assert!(app.model.store_tap.is_none());
    assert_eq!(*app.nav.current(), Route::Library);
}

#[test]
fn tap_flow_leaving_the_screen_cancels_a_running_download() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap = Some(Ok(owned_answer()));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-4".into(),
        tracks: 10,
        estimated_bytes: None,
        note: String::new(),
    }));
    mock.session = Some(Ok(DownloadSession {
        session: "s-4".into(),
    }));
    mock.progress = vec![
        Ok(progress(DownloadState::Running, 0, None)),
        Ok(progress(DownloadState::Cancelled, 0, None)),
    ];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Confirm { .. })
    });
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Downloading { .. })
    });

    // Back is the way out; a transfer still running is cancelled on the way out so the
    // screen never leaves an orphan download behind.
    press(&mut app, &mut wifi, InputEvent::Back);
    assert!(app.model.store_tap.is_none(), "the screen is closed");
    assert_ne!(*app.nav.current(), Route::LibraryStoreTap);

    // The cancel reaches the service (the store client is off-thread, so let it run)…
    for _ in 0..200 {
        if !calls.lock().unwrap().cancels.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
        app.handle_input(InputEvent::Tick { now_ms: 0 }, &mut wifi);
    }
    assert_eq!(calls.lock().unwrap().cancels, vec!["s-4".to_string()]);

    // …the service acknowledges it, and the popup says what happened.
    pump_until(&mut app, &mut wifi, |app| {
        app.compositor
            .status()
            .is_some_and(|status| status.lines().iter().any(|l| l.contains("cancelled")))
    });
}

#[test]
fn tap_flow_noubin_link_asks_before_reading_the_page_then_sends_consent() {
    // A tag normally carries a Noubin URL, and the Bandcamp link lives on that page. The
    // device must not contact it without the person agreeing, and must not offer Download
    // until the store has actually answered.
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    let mut consent_answer = owned_answer();
    consent_answer.state = plippa_ui::TapState::LookupConsent;
    consent_answer.candidates.clear();
    consent_answer.confidence = 0.0;
    consent_answer.lookup = Some(plippa_ui::LookupConsentInfo {
        host: "noubin.example".into(),
        fetch_url: "https://noubin.example/n/joni/blue/web.noudata".into(),
        consent_message: "This will contact noubin.example.".into(),
    });
    mock.tap = Some(Ok(consent_answer));
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    tap(&mut app, &mut wifi, "https://noubin.example/n/joni/blue/");

    // The first call went out without consent, and the screen offers the lookup.
    assert_eq!(
        calls.lock().unwrap().taps,
        vec![("https://noubin.example/n/joni/blue/".to_string(), false)],
        "the first call must not carry consent"
    );
    match phase(&app) {
        StoreTapPhase::LookupConsent { host, .. } => assert_eq!(host, "noubin.example"),
        other => panic!("expected the consent step, got {other:?}"),
    }
    assert_eq!(
        actions(&app),
        vec![plippa_ui::StoreTapAction::AllowLookup, plippa_ui::StoreTapAction::Back],
        "the consent step is a choice, not a dead end"
    );

    // Agreeing repeats the call, this time with consent.
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        calls.lock().unwrap().taps.len() == 2
    });
    assert_eq!(
        calls.lock().unwrap().taps[1],
        ("https://noubin.example/n/joni/blue/".to_string(), true),
        "the second call carries the consent the person just gave"
    );
}

#[test]
fn tap_flow_download_is_united_against_the_tag_key_with_its_scheme() {
    use plippa_ipc_proto::UnmatchedNoubin;

    // The engine also reports the normalised key, which has no scheme. The server cannot fetch
    // `host/path` as a URL, so the tagged key is what must reach the download.
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap = Some(Ok(owned_answer()));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-9".into(),
        tracks: 10,
        estimated_bytes: None,
        note: String::new(),
    }));
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    app.handle_ipc(plippa_ipc_proto::IpcMessage::UnmatchedNoubin(
        UnmatchedNoubin {
            noubin_key: "https://noubin.example/n/joni/blue/".into(),
            normalised_key: "noubin.example/n/joni/blue/".into(),
            seq: 9,
            candidate_urls: vec!["https://noubin.example/n/joni/blue/".into()],
        },
    ));
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Owned { .. })
    });
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Confirm { .. })
    });
    assert_eq!(
        calls.lock().unwrap().prepares,
        vec![(
            "https://noubin.example/n/joni/blue/".to_string(),
            "1234".to_string()
        )],
        "the key keeps its scheme so the server can fetch its web.noudata"
    );
}

#[test]
fn tap_flow_unmatched_notice_opens_the_store_screen() {
    use plippa_ipc_proto::UnmatchedNoubin;

    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap = Some(Ok(not_owned_answer()));
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    // The engine carries the tag's platform URLs; the UI never re-reads the tag.
    app.handle_ipc(plippa_ipc_proto::IpcMessage::UnmatchedNoubin(
        UnmatchedNoubin {
            noubin_key: "https://joni.example/blue/".into(),
            normalised_key: "joni.example/blue/".into(),
            seq: 7,
            candidate_urls: vec![
                "https://joni.example/blue/".into(),
                "https://joni.bandcamp.com/album/blue".into(),
            ],
        },
    ));
    assert_eq!(*app.nav.current(), Route::LibraryStoreTap);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::NotOwned { .. })
    });
    assert_eq!(
        calls.lock().unwrap().taps,
        vec![("https://joni.bandcamp.com/album/blue".to_string(), false)],
        "the Bandcamp link wins over any other platform link"
    );
    assert_eq!(
        app.model.store_tap.as_ref().unwrap().noubin_key.as_deref(),
        Some("https://joni.example/blue/"),
        "the tagged key keeps its scheme; the server cannot fetch a scheme-less key"
    );

    // A tag with no platform links keeps the old portal popup and does not open the screen.
    let mut app = UiApp::new(UiConfig::default());
    app.handle_ipc(plippa_ipc_proto::IpcMessage::UnmatchedNoubin(
        UnmatchedNoubin {
            noubin_key: "https://joni.example/blue/".into(),
            normalised_key: "joni.example/blue/".into(),
            seq: 8,
            candidate_urls: Vec::new(),
        },
    ));
    assert_eq!(*app.nav.current(), Route::Main);
    let status = app.compositor.status().expect("portal popup");
    assert!(status.lines().iter().any(|l| l.contains("web portal")));
}

#[test]
fn tap_flow_screens_paint_for_every_state() {
    // Layout arithmetic (the action block, the QR box, the clipping) is exercised for each
    // phase: a panic or an out-of-range paint fails here, before anyone powers the panel on.
    let phases = vec![
        StoreTapPhase::Lookup,
        StoreTapPhase::Owned {
            release: TapRelease {
                store_release_id: "1".into(),
                title: "A title far too long for one 320 px line, repeated to be sure".into(),
                artist: "Joni Mitchell".into(),
                track_count: Some(10),
            },
            message: "You own this on Bandcamp.".into(),
        },
        StoreTapPhase::NotOwned {
            title: "Blue".into(),
            artist: "Joni Mitchell".into(),
            url: "https://joni.bandcamp.com/album/blue".into(),
            interest_saved: true,
            from_pick: false,
            release_id: None,
        },
        StoreTapPhase::NotOwned {
            title: "A picked release with an awkwardly long title that wraps".into(),
            artist: "Joni Mitchell".into(),
            url: "https://joni.bandcamp.com/album/a-very-long-slug-for-a-purchase-page-link".into(),
            interest_saved: false,
            from_pick: true,
            release_id: Some("1".into()),
        },
        StoreTapPhase::Ambiguous {
            candidates: vec![
                candidate("1", "Blue", "Joni Mitchell", 0.8),
                candidate("2", "Blue (Live)", "Joni Mitchell", 0.7),
            ],
            selected: 0,
        },
        StoreTapPhase::Blocked {
            blocked: TapBlocked::NoStoreCache,
            message: "This device has not synced your Bandcamp library yet.".into(),
        },
        StoreTapPhase::Syncing { status: None },
        StoreTapPhase::Syncing {
            status: Some(sync_status(SyncPhase::Running, 3, 120)),
        },
        StoreTapPhase::SyncFailed {
            message: "Bandcamp is not answering.".into(),
        },
        StoreTapPhase::LowConfidence {
            candidate: candidate("3", "Hejira", "Joni Mitchell", 0.62),
            url: Some("https://joni.bandcamp.com/album/hejira".into()),
        },
        StoreTapPhase::Blocked {
            blocked: TapBlocked::Offline,
            message: "Can't reach the store service.".into(),
        },
        StoreTapPhase::Preparing,
        StoreTapPhase::Confirm {
            release: TapRelease {
                store_release_id: "1".into(),
                title: "Blue".into(),
                artist: "Joni Mitchell".into(),
                track_count: Some(10),
            },
            consent: DownloadConsent {
                consent_id: "c".into(),
                tracks: 10,
                estimated_bytes: Some(200 * 1024 * 1024),
                note: "Bandcamp sends a compressed stream.".into(),
            },
        },
        StoreTapPhase::Downloading {
            session: "s".into(),
            progress: progress(DownloadState::Running, 0, None),
        },
        StoreTapPhase::Complete {
            files: 1,
            noudata: Some("Blue/blue.noudata".into()),
            partial: false,
        },
        StoreTapPhase::Complete {
            files: 7,
            noudata: None,
            partial: true,
        },
        StoreTapPhase::Failed {
            message: "The download stopped.".into(),
            retry: true,
        },
    ];
    let mut app = UiApp::new(UiConfig::default());
    app.nav.force_push(Route::LibraryStoreTap);
    let mut fb = FrameBuffer::new(320, 240);
    for phase in phases {
        let mut view = StoreTapView::lookup(
            "https://joni.bandcamp.com/album/blue",
            None,
            "Blue".into(),
        );
        view.phase = phase.clone();
        // Each screen is painted with the placeholder, then with a decoded cover.
        let with_cover = view.cover_release_id().is_some();
        app.model.store_tap = Some(view.clone());
        fb.clear(plippa_ui::ui::tokens::BG);
        app.model.render_screen(&app.nav, &mut fb);
        if with_cover {
            let image = plippa_ui::art::decode_store_cover(&jpeg_cover(), plippa_ui::STORE_COVER_SIDE)
                .expect("fixture cover decodes");
            view.cover_for = view.cover_release_id().map(str::to_string);
            view.cover = plippa_ui::CoverState::Ready(std::sync::Arc::new(image));
            app.model.store_tap = Some(view);
            fb.clear(plippa_ui::ui::tokens::BG);
            app.model.render_screen(&app.nav, &mut fb);
        }
        // The action block sits on whole rows above the footer: the screen is never a wall
        // of text with the primary action pushed off the bottom.
        let actions = app.model.store_tap.as_ref().unwrap().actions().len() as u16;
        assert!(
            actions * plippa_ui::ui::tokens::ROW_H <= plippa_ui::ui::tokens::CONTENT_BOTTOM
                - plippa_ui::ui::tokens::CONTENT_TOP,
            "{phase:?} leaves no room for its actions"
        );
    }
}

#[test]
fn tap_flow_short_url_and_qr_rules() {
    // Never a scheme in the text under the QR, never a query in either.
    assert_eq!(
        store_short_url("https://a.bandcamp.com/album/x?from=tag#t"),
        "a.bandcamp.com/album/x"
    );
    assert_eq!(store_short_url("https://a.bandcamp.com/album/x/"), "a.bandcamp.com/album/x");
    // A symbol wider than the box can carry at the minimum module size is refused, not shrunk.
    assert_eq!(store_qr_module_px(21, STORE_QR_BOX), Some(4));
    assert_eq!(store_qr_module_px(40, STORE_QR_BOX), Some(2));
    assert_eq!(store_qr_module_px(60, STORE_QR_BOX), None);
    assert_eq!(store_qr_module_px(0, STORE_QR_BOX), None);
    // A typical release link still fits at the minimum module size.
    let url = "https://fourtet.bandcamp.com/album/there-is-love-in-you";
    let matrix = plippa_core::qr::QrMatrix::encode(url, plippa_core::qr::QrErrorLevel::Medium)
        .expect("encode");
    let module = store_qr_module_px(matrix.width, STORE_QR_BOX).expect("fits");
    assert!(module >= STORE_QR_MIN_MODULE_PX);
    assert!(
        u32::from(module) * (matrix.width + 4) <= u32::from(STORE_QR_BOX),
        "the painted square stays inside its box"
    );
}

#[test]
fn tap_flow_text_from_the_air_is_sanitised_before_it_is_drawn() {
    // SEC-12: a hostile tag title cannot reach the screen verbatim.
    let mut hostile = not_owned_answer();
    hostile.message = "Buy it \u{202E}now".into();
    assert!(!screen_text(&hostile.message).contains('\u{202E}'));
    let mut app = UiApp::new(UiConfig::default());
    app.model.store_tap = Some(StoreTapView::lookup(
        "https://a.bandcamp.com/album/x",
        None,
        "T\u{202E}itle".into(),
    ));
    let view = app.model.store_tap.as_ref().unwrap();
    assert!(!view.tag_title.contains('\u{202E}'));
}

// ---------------------------------------------------------------------------
// BCS_11: resync from the tap screen, skip the internet ask, covers, debounce
// ---------------------------------------------------------------------------

fn no_store_cache_answer() -> TapAnswer {
    let mut answer = TapAnswer::blocked(
        TapBlocked::NoStoreCache,
        "This device has not synced your Bandcamp library yet.",
    );
    answer.state = TapState::NotFound;
    answer
}

fn sync_status(state: SyncPhase, pages: u32, releases: u32) -> SyncStatus {
    SyncStatus {
        state,
        pages,
        releases,
        ..SyncStatus::default()
    }
}

#[test]
fn tap_flow_no_store_cache_resyncs_then_repeats_the_tap_exactly_once() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap_queue = vec![Ok(no_store_cache_answer()), Ok(owned_answer())];
    mock.sync_start = Some(Ok(sync_status(SyncPhase::Running, 0, 0)));
    mock.sync_status = vec![
        Ok(sync_status(SyncPhase::Running, 3, 120)),
        Ok(sync_status(SyncPhase::Done, 6, 240)),
    ];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();
    let url = "https://joni.bandcamp.com/album/blue";

    tap(&mut app, &mut wifi, url);
    match phase(&app) {
        StoreTapPhase::Blocked { blocked, ref message } => {
            assert_eq!(blocked, TapBlocked::NoStoreCache);
            assert!(message.contains("not synced"), "{message:?}");
        }
        other => panic!("expected the no-cache state, got {other:?}"),
    }
    assert_eq!(
        actions(&app),
        vec![plippa_ui::StoreTapAction::Sync, plippa_ui::StoreTapAction::Back],
        "Resync now, not only Retry"
    );

    // One press starts the sync and the screen shows its progress.
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    assert!(matches!(phase(&app), StoreTapPhase::Syncing { .. }));
    pump_until(&mut app, &mut wifi, |app| {
        matches!(
            app.model.store_tap.as_ref().map(|v| &v.phase),
            Some(StoreTapPhase::Syncing { status: Some(s) }) if s.releases == 120
        )
    });

    // When the sync is done the original tap is sent again, and the normal screen appears
    // with no second press.
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Owned { .. })
    });
    // Keep ticking: nothing may re-send after the one repeat.
    for step in 0..120u64 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        app.handle_input(InputEvent::Tick { now_ms: 20_000 + step * 16 }, &mut wifi);
    }
    let calls = calls.lock().unwrap();
    assert_eq!(calls.sync_starts, 1);
    assert_eq!(
        calls.taps,
        vec![(url.to_string(), false), (url.to_string(), false)],
        "exactly one re-send of the same tap"
    );
}

#[test]
fn tap_flow_failed_sync_says_so_and_offers_retry_without_looping() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap_queue = vec![Ok(no_store_cache_answer())];
    mock.sync_start = Some(Ok(sync_status(SyncPhase::Running, 0, 0)));
    let mut failed = sync_status(SyncPhase::Failed, 1, 0);
    failed.error = Some("Bandcamp is not answering.".into());
    mock.sync_status = vec![Ok(failed)];
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::SyncFailed { .. })
    });
    match phase(&app) {
        StoreTapPhase::SyncFailed { ref message } => assert!(message.contains("not answering")),
        other => panic!("expected SyncFailed, got {other:?}"),
    }
    assert_eq!(
        actions(&app),
        vec![
            plippa_ui::StoreTapAction::Sync,
            plippa_ui::StoreTapAction::Retry,
            plippa_ui::StoreTapAction::Back
        ]
    );
    // Time passing re-sends nothing.
    for step in 0..120u64 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        app.handle_input(InputEvent::Tick { now_ms: 30_000 + step * 16 }, &mut wifi);
    }
    let calls = calls.lock().unwrap();
    assert_eq!(calls.taps.len(), 1, "no automatic retry");
    assert_eq!(calls.sync_starts, 1);
}

fn consent_answer() -> TapAnswer {
    let mut answer = answer_with_state(TapState::LookupConsent);
    answer.lookup = Some(plippa_ui::LookupConsentInfo {
        host: "noubin.example".into(),
        fetch_url: "https://noubin.example/n/joni/blue/web.noudata".into(),
        consent_message: "Reading this page contacts noubin.example.".into(),
    });
    answer
}

fn answer_with_state(state: TapState) -> TapAnswer {
    TapAnswer {
        state,
        ..not_owned_answer()
    }
}

fn app_with_skip_setting(mock: MockStore, skip: Option<&str>) -> (UiApp, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("play_mode.json");
    if let Some(text) = skip {
        std::fs::write(&path, text).expect("settings file");
    }
    let mut app = UiApp::new(UiConfig {
        play_mode_path: Some(path),
        ..UiConfig::default()
    });
    app.set_store_api(Box::new(mock));
    (app, dir)
}

#[test]
fn tap_flow_skip_internet_ask_sends_the_consent_call_immediately() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap_queue = vec![Ok(consent_answer()), Ok(owned_answer())];
    let (mut app, _dir) = app_with_skip_setting(mock, Some(r#"{"skip_internet_consent": true}"#));
    let mut wifi = default_mock_wifi();
    let key = "https://noubin.example/n/joni/blue/";

    assert!(app.open_store_tap(&[key.to_string()], None, "Blue"));
    // No press: the lookup went ahead and the screen is on the release.
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Owned { .. })
    });
    assert_eq!(
        calls.lock().unwrap().taps,
        vec![(key.to_string(), false), (key.to_string(), true)],
        "the second call is the one AllowLookup makes today"
    );
    assert!(
        !matches!(phase(&app), StoreTapPhase::LookupConsent { .. }),
        "the consent screen never appeared"
    );
}

#[test]
fn tap_flow_skip_internet_ask_off_or_garbage_still_shows_the_consent_screen() {
    for setting in [
        None,
        Some(r#"{"skip_internet_consent": false}"#),
        Some(r#"{"skip_internet_consent": "yes"}"#),
        Some("not json"),
    ] {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let mut mock = MockStore::new(calls.clone());
        mock.tap_queue = vec![Ok(consent_answer())];
        let (mut app, _dir) = app_with_skip_setting(mock, setting);
        let mut wifi = default_mock_wifi();
        assert!(app.open_store_tap(
            &["https://noubin.example/n/joni/blue/".to_string()],
            None,
            "Blue"
        ));
        pump_until(&mut app, &mut wifi, |app| {
            matches!(phase(app), StoreTapPhase::LookupConsent { .. })
        });
        assert_eq!(calls.lock().unwrap().taps.len(), 1, "{setting:?}: nothing sent unasked");
    }
}

#[test]
fn tap_flow_skip_internet_ask_does_not_touch_bandcamp_links_or_start_downloads() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap_queue = vec![Ok(owned_answer())];
    let (mut app, _dir) = app_with_skip_setting(mock, Some(r#"{"skip_internet_consent": true}"#));
    let mut wifi = default_mock_wifi();
    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");
    // A Bandcamp link needs no consent: one call, and the download still waits for a press.
    assert!(matches!(phase(&app), StoreTapPhase::Owned { .. }));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.taps.len(), 1);
    assert!(calls.prepares.is_empty() && calls.starts.is_empty());
}

#[test]
fn consent_audit_line_names_the_host_and_nothing_else() {
    let line = plippa_ui::consent_skipped_log_line("plippa.page");
    assert_eq!(line, "store: contacted plippa.page without confirmation (setting on)");
    // Anything that is not a plain host (a URL, a query, a credential) never reaches the log.
    for hostile in [
        "https://plippa.page/x?token=secret",
        "plippa.page/path",
        "user:pass@plippa.page",
        "host name",
        "",
    ] {
        let line = plippa_ui::consent_skipped_log_line(hostile);
        assert_eq!(
            line,
            "store: contacted an unknown host without confirmation (setting on)",
            "{hostile:?}"
        );
    }
}

fn jpeg_cover() -> Vec<u8> {
    let (w, h) = (96u32, 64u32);
    let px = (0..w * h).flat_map(|i| [(i % 256) as u8, 40, 200]).collect();
    plippa_image::encode_jpeg_fit(&plippa_image::Rgb8 { w, h, px }, 96).expect("fixture jpeg")
}

#[test]
fn tap_flow_cover_is_fetched_off_the_loop_and_shown_when_it_decodes() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap = Some(Ok(owned_answer()));
    mock.cover = Some(Ok(CoverBytes {
        content_type: "image/jpeg".into(),
        bytes: jpeg_cover(),
    }));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-1".into(),
        tracks: 10,
        estimated_bytes: None,
        note: String::new(),
    }));
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();

    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");
    // The screen is usable at once; the cover arrives on its own.
    assert!(matches!(phase(&app), StoreTapPhase::Owned { .. }));
    pump_until(&mut app, &mut wifi, |app| {
        matches!(
            app.model.store_tap.as_ref().map(|v| &v.cover),
            Some(plippa_ui::CoverState::Ready(_))
        )
    });
    match &app.model.store_tap.as_ref().unwrap().cover {
        plippa_ui::CoverState::Ready(image) => {
            assert_eq!(image.side, plippa_ui::STORE_COVER_SIDE);
            assert_eq!(image.rgb565.len(), usize::from(image.side).pow(2));
        }
        other => panic!("expected a ready cover, got {other:?}"),
    }
    assert_eq!(calls.lock().unwrap().covers, vec!["1234".to_string()]);

    // Download -> Confirm names the same release: the picture is reused, not fetched again.
    press(&mut app, &mut wifi, InputEvent::MenuEnter);
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Confirm { .. })
    });
    assert_eq!(calls.lock().unwrap().covers.len(), 1);
    assert!(matches!(
        app.model.store_tap.as_ref().unwrap().cover,
        plippa_ui::CoverState::Ready(_)
    ));
}

#[test]
fn tap_flow_a_cover_that_fails_leaves_the_phase_and_the_screen_working() {
    for cover in [
        Err(StoreApiError::Malformed("cover too large".into())),
        Err(StoreApiError::Offline("service stopped".into())),
        Ok(CoverBytes {
            content_type: "image/jpeg".into(),
            bytes: b"this is not an image".to_vec(),
        }),
    ] {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let mut mock = MockStore::new(calls.clone());
        mock.tap = Some(Ok(owned_answer()));
        mock.cover = Some(cover.clone());
        mock.consent = Some(Ok(DownloadConsent {
            consent_id: "c-1".into(),
            tracks: 10,
            estimated_bytes: None,
            note: String::new(),
        }));
        let mut app = app_with(mock);
        let mut wifi = default_mock_wifi();
        tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");
        pump_until(&mut app, &mut wifi, |app| {
            matches!(
                app.model.store_tap.as_ref().map(|v| &v.cover),
                Some(plippa_ui::CoverState::Failed)
            )
        });
        assert!(
            matches!(phase(&app), StoreTapPhase::Owned { .. }),
            "{cover:?}: the phase is untouched"
        );
        // The placeholder is drawn and the flow carries on.
        let painted = app.composed_pixels();
        assert!(!painted.is_empty());
        press(&mut app, &mut wifi, InputEvent::MenuEnter);
        pump_until(&mut app, &mut wifi, |app| {
            matches!(phase(app), StoreTapPhase::Confirm { .. })
        });
    }
}

#[test]
fn tap_flow_a_release_with_no_id_shows_the_placeholder_and_fetches_nothing() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap = Some(Ok(not_owned_answer()));
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();
    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");
    assert!(matches!(phase(&app), StoreTapPhase::NotOwned { .. }));
    for step in 0..30u64 {
        std::thread::sleep(std::time::Duration::from_millis(1));
        app.handle_input(InputEvent::Tick { now_ms: step * 16 }, &mut wifi);
    }
    assert!(calls.lock().unwrap().covers.is_empty());
    assert_eq!(
        app.model.store_tap.as_ref().unwrap().cover,
        plippa_ui::CoverState::None
    );
}

#[test]
fn tap_flow_held_enter_cannot_skip_the_confirmation() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let mut mock = MockStore::new(calls.clone());
    mock.tap = Some(Ok(owned_answer()));
    mock.consent = Some(Ok(DownloadConsent {
        consent_id: "c-held".into(),
        tracks: 10,
        estimated_bytes: None,
        note: String::new(),
    }));
    let mut app = app_with(mock);
    let mut wifi = default_mock_wifi();
    tap(&mut app, &mut wifi, "https://joni.bandcamp.com/album/blue");

    // Enter on the release asks what the download costs; further presses in the same breath
    // (a held or bounced key) change nothing.
    for _ in 0..4 {
        press(&mut app, &mut wifi, InputEvent::MenuEnter);
    }
    pump_until(&mut app, &mut wifi, |app| {
        matches!(phase(app), StoreTapPhase::Confirm { .. })
    });
    assert_eq!(calls.lock().unwrap().prepares.len(), 1, "one prepare for one deliberate press");
    assert!(calls.lock().unwrap().starts.is_empty(), "the download waits on the consent screen");
}
