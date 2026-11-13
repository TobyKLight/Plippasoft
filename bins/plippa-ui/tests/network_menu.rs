//! SECFIX_02: System > Network on the device, driven through the in-process UI harness.
//!
//! A scripted [`StoreApi`] stands in for the server's control listener: it keeps the portal's
//! switches the way the server does (the first enable generates a PIN, a typed PIN replaces it,
//! a weak one is refused) and can hold an answer back, so the tests can prove that no press ever
//! waits for the network.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plippa_ui::ui::tokens::{BG, CONTENT_BOTTOM, SCREEN_H, SCREEN_W, WARN};
use plippa_ui::WifiBackend;
use plippa_ui::{
    default_mock_wifi, FrameBuffer, InputEvent, MockWifi, NetworkPhase, NoStoreApi, PortalAddress,
    PortalSettingsInfo, Route, StoreApi, StoreApiError, UiApp, UiConfig,
};

#[derive(Default, Debug, Clone)]
struct Calls {
    reads: usize,
    enabled: Vec<bool>,
    https: Vec<bool>,
    regenerated: usize,
    required: Vec<bool>,
    pins: Vec<String>,
    clears: usize,
}

struct MockPortal {
    inner: NoStoreApi,
    calls: Arc<Mutex<Calls>>,
    state: Arc<Mutex<PortalSettingsInfo>>,
    /// `set_portal_enabled` waits for a message here (or five seconds) before answering.
    gate: Option<Receiver<()>>,
    /// What `portal_address` answers.
    address: Option<PortalAddress>,
}

fn settings(enabled: bool, pin_required: bool, pin: Option<&str>) -> PortalSettingsInfo {
    PortalSettingsInfo {
        available: true,
        enabled,
        https_enabled: true,
        pin_required,
        pin: pin.map(str::to_string),
        listening: enabled,
        ..PortalSettingsInfo::default()
    }
}

struct Handles {
    calls: Arc<Mutex<Calls>>,
    state: Arc<Mutex<PortalSettingsInfo>>,
}

fn mock(initial: PortalSettingsInfo, gate: Option<Receiver<()>>) -> (MockPortal, Handles) {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let state = Arc::new(Mutex::new(initial));
    (
        MockPortal {
            inner: NoStoreApi,
            calls: calls.clone(),
            state: state.clone(),
            gate,
            address: None,
        },
        Handles { calls, state },
    )
}

impl StoreApi for MockPortal {
    fn portal_settings(&mut self) -> Result<PortalSettingsInfo, StoreApiError> {
        self.calls.lock().unwrap().reads += 1;
        Ok(self.state.lock().unwrap().clone())
    }

    fn set_portal_enabled(&mut self, enabled: bool) -> Result<PortalSettingsInfo, StoreApiError> {
        if let Some(gate) = &self.gate {
            let _ = gate.recv_timeout(Duration::from_secs(5));
        }
        self.calls.lock().unwrap().enabled.push(enabled);
        let mut s = self.state.lock().unwrap();
        s.enabled = enabled;
        s.listening = enabled;
        if enabled && s.pin.is_none() {
            s.pin = Some("483920".into());
        }
        Ok(s.clone())
    }

    fn set_https_enabled(&mut self, on: bool) -> Result<PortalSettingsInfo, StoreApiError> {
        self.calls.lock().unwrap().https.push(on);
        let mut s = self.state.lock().unwrap();
        s.https_enabled = on;
        // HTTPS off means no certificate is served, so there is no fingerprint to show.
        if !on {
            s.fingerprint = None;
        }
        Ok(s.clone())
    }

    fn regenerate_certificate(&mut self) -> Result<PortalSettingsInfo, StoreApiError> {
        self.calls.lock().unwrap().regenerated += 1;
        let mut s = self.state.lock().unwrap();
        // A new key is a new fingerprint; the test only needs it to change.
        s.fingerprint = Some("ff".repeat(32));
        Ok(s.clone())
    }

    fn set_pin_required(&mut self, required: bool) -> Result<PortalSettingsInfo, StoreApiError> {
        self.calls.lock().unwrap().required.push(required);
        let mut s = self.state.lock().unwrap();
        s.pin_required = required;
        Ok(s.clone())
    }

    fn set_pin(&mut self, pin: &str) -> Result<PortalSettingsInfo, StoreApiError> {
        self.calls.lock().unwrap().pins.push(pin.to_string());
        if pin == "000000" {
            return Err(StoreApiError::Refused {
                status: 400,
                code: Some("bad_pin".into()),
                message: "refused".into(),
            });
        }
        let mut s = self.state.lock().unwrap();
        s.pin = Some(pin.to_string());
        Ok(s.clone())
    }

    fn clear_lockout(&mut self) -> Result<PortalSettingsInfo, StoreApiError> {
        self.calls.lock().unwrap().clears += 1;
        let mut s = self.state.lock().unwrap();
        s.locked = false;
        Ok(s.clone())
    }

    fn portal_address(&mut self) -> Result<PortalAddress, StoreApiError> {
        match &self.address {
            Some(a) => Ok(a.clone()),
            None => self.inner.portal_address(),
        }
    }

    // The rest of the service is not part of this screen.
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
    fn waveform_status(
        &mut self,
        p: u64,
        i: u32,
    ) -> Result<Vec<plippa_ui::AnalysisKindStatus>, StoreApiError> {
        self.inner.waveform_status(p, i)
    }
    fn reanalyse(
        &mut self,
        p: u64,
        i: u32,
        k: Option<plippa_ui::AnalysisKind>,
    ) -> Result<(), StoreApiError> {
        self.inner.reanalyse(p, i, k)
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Rig {
    app: UiApp,
    wifi: MockWifi,
    clock: u64,
    /// Where `play_mode.json` goes (the tap consent lives there), so no test touches the
    /// real state directory.
    _dir: tempfile::TempDir,
    /// The `play_mode.json` the rig writes to (the tap consent lives there).
    play_mode_path: std::path::PathBuf,
}

impl Rig {
    fn new(api: MockPortal) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let play_mode_path = dir.path().join("play_mode.json");
        let mut app = UiApp::new(UiConfig {
            play_mode_path: Some(play_mode_path.clone()),
            ..UiConfig::default()
        });
        app.set_store_api(Box::new(api));
        let mut wifi = default_mock_wifi();
        app.init_wifi(&mut wifi);
        Rig { app, wifi, clock: 0, _dir: dir, play_mode_path }
    }

    fn press(&mut self, ev: InputEvent) {
        self.app.handle_input(ev, &mut self.wifi);
    }

    /// One frame's tick on the simulated clock.
    fn tick(&mut self) {
        self.clock += 16;
        let now_ms = self.clock;
        self.press(InputEvent::Tick { now_ms });
    }

    fn pump_until(&mut self, what: &str, done: impl Fn(&UiApp) -> bool) {
        for _ in 0..1_500 {
            if done(&self.app) {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
            self.tick();
        }
        panic!("condition not reached: {what}");
    }

    /// System menu -> Network, with the settings read.
    fn open_network(&mut self) {
        self.app.nav.force_push(Route::System);
        self.press(InputEvent::MenuEnter);
        assert_eq!(*self.app.nav.current(), Route::SystemNetwork);
        self.pump_until("settings read", |app| {
            app.model
                .network
                .as_ref()
                .is_some_and(|v| !matches!(v.phase, NetworkPhase::Loading))
        });
    }

    /// Move the cursor to row `index` of the Network menu (the encoder reads top-down: a
    /// negative detent moves down).
    fn focus_row(&mut self, index: usize) {
        while self.app.nav.focus != index {
            let delta = if self.app.nav.focus < index { -1 } else { 1 };
            self.press(InputEvent::MenuRotate(delta));
        }
    }

    fn row(&self, label: &str) -> usize {
        self.app
            .model
            .network
            .as_ref()
            .expect("network view")
            .rows()
            .iter()
            .position(|r| r.label() == label)
            .unwrap_or_else(|| panic!("no row {label}"))
    }

    fn frame(&self) -> FrameBuffer {
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(BG);
        self.app.model.render_screen(&self.app.nav, &mut fb);
        fb
    }

    /// Type `digits` on the PIN pad: each box is edit (push), turn to the digit, commit — and
    /// the commit auto-advances to the next empty box, so the last one lands on `OK`.
    fn type_pin(&mut self, digits: &str) {
        for ch in digits.chars() {
            let d = ch.to_digit(10).unwrap() as i8;
            self.press(InputEvent::MenuEnter); // edit this box: the preview starts at 0
            if d != 0 {
                self.press(InputEvent::MenuRotate(d));
            }
            self.press(InputEvent::MenuEnter); // commit + advance
        }
        self.press(InputEvent::MenuEnter); // OK (only reachable once all six are set)
    }
}

fn warn_pixels(fb: &FrameBuffer) -> usize {
    fb.pixels.iter().filter(|p| **p == WARN).count()
}

// ---------------------------------------------------------------------------
// The menu
// ---------------------------------------------------------------------------

#[test]
fn the_system_menu_has_network_and_no_top_level_ip_or_wifi() {
    let (api, h) = mock(settings(false, true, None), None);
    let mut rig = Rig::new(api);
    // The System menu's first row is Network, and entering it asks the server for the settings.
    let titles: Vec<_> = rig.app.nav.system_items().iter().map(|r| r.title()).collect();
    assert_eq!(titles[0], "Network");
    assert!(!titles.iter().any(|t| *t == "IP address" || *t == "Wi\u{2011}Fi" || *t == "IP"));
    rig.open_network();
    assert_eq!(h.calls.lock().unwrap().reads, 1);
}

#[test]
fn the_network_menu_rows_and_order() {
    let (api, _h) = mock(settings(true, true, Some("048215")), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    let view = rig.app.model.network.as_ref().unwrap();
    let rows: Vec<_> = view.rows().iter().map(|r| r.label()).collect();
    assert_eq!(
        rows,
        [
            "IP address",
            "Web portal",
            "Web portal HTTPS",
            "View certificate",
            "Web portal PIN required",
            "Web portal PIN",
            "Regenerate certificate",
            "Skip internet ask on tap",
            "Wi-Fi"
        ]
    );
    // What each says.
    let value = |label: &str| {
        let row = *view.rows().iter().find(|r| r.label() == label).unwrap();
        view.row_text(row, Some("192.168.1.42"), Some("home"), false).1
    };
    assert_eq!(value("IP address"), "192.168.1.42");
    assert_eq!(value("Web portal"), "On");
    assert_eq!(value("Web portal HTTPS"), "On");
    assert_eq!(value("View certificate"), "Open");
    assert_eq!(value("Regenerate certificate"), "Change");
    assert_eq!(value("Web portal PIN required"), "On");
    assert_eq!(value("Web portal PIN"), "048215");
    assert_eq!(value("Skip internet ask on tap"), "Off");
    assert_eq!(value("Wi-Fi"), "home");
    // And every row fits the 320 px panel with its value (nothing is clipped to an ellipsis).
    use plippa_ui::ui::text::{measure, TypeRole};
    use plippa_ui::ui::tokens::{MARGIN_X, SPACE_2};
    let room = SCREEN_W - MARGIN_X * 2 - SPACE_2 * 3; // label, gap, value, chevron
    for row in view.rows() {
        let (label, value) = view.row_text(row, Some("192.168.1.142"), Some("home"), false);
        let used = measure(label, TypeRole::Body) + measure(&value, TypeRole::Small);
        assert!(used <= room, "{label}: {used}px of {room}px");
    }
}

#[test]
fn the_switches_toggle_and_the_first_enable_shows_a_generated_pin() {
    let (api, h) = mock(settings(false, true, None), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    let v = rig.app.model.network.as_ref().unwrap();
    assert_eq!(
        v.row_text(*v.rows().iter().find(|r| r.label() == "Web portal PIN").unwrap(), None, None, false).1,
        "not set yet"
    );

    rig.focus_row(rig.row("Web portal"));
    rig.press(InputEvent::MenuEnter);
    rig.pump_until("portal on", |app| {
        app.model.network.as_ref().and_then(|v| v.settings.as_ref()).is_some_and(|s| s.enabled)
    });
    assert_eq!(h.calls.lock().unwrap().enabled, [true]);
    let v = rig.app.model.network.as_ref().unwrap();
    assert_eq!(v.pin_display().as_deref(), Some("483 920"), "the PIN the server generated");
    assert_eq!(rig.app.model.portal_enabled, Some(true));

    rig.focus_row(rig.row("Web portal PIN required"));
    rig.press(InputEvent::MenuEnter);
    rig.pump_until("pin switch off", |app| {
        app.model.network.as_ref().and_then(|v| v.settings.as_ref()).is_some_and(|s| !s.pin_required)
    });
    assert_eq!(h.calls.lock().unwrap().required, [false]);
}

#[test]
fn the_https_row_switches_tls_warns_and_regenerate_asks_twice() {
    let mut initial = settings(true, true, Some("048215"));
    initial.fingerprint = Some("0123456789abcdef".repeat(4));
    let (api, h) = mock(initial, None);
    let mut rig = Rig::new(api);
    rig.open_network();
    assert_eq!(rig.frame().pixels.iter().filter(|p| **p == WARN).count(), 0);

    // Off: the row reads Off, turns amber, the warning appears, and the regenerate row goes.
    rig.focus_row(rig.row("Web portal HTTPS"));
    rig.press(InputEvent::MenuEnter);
    rig.pump_until("https off", |app| {
        app.model
            .network
            .as_ref()
            .and_then(|v| v.settings.as_ref())
            .is_some_and(|s| !s.https_enabled)
    });
    assert_eq!(h.calls.lock().unwrap().https, [false]);
    let view = rig.app.model.network.as_ref().unwrap();
    assert_eq!(view.row_text(plippa_ui::NetworkRow::Https, None, None, false).1, "Off");
    assert!(!view.rows().contains(&plippa_ui::NetworkRow::Regenerate));
    assert!(
        !view.rows().contains(&plippa_ui::NetworkRow::Certificate),
        "no certificate is served, so there is none to view"
    );
    assert!(rig.frame().pixels.iter().filter(|p| **p == WARN).count() > 0);

    // Back on, then the certificate row: one press only arms it, the second makes the call.
    rig.focus_row(rig.row("Web portal HTTPS"));
    rig.press(InputEvent::MenuEnter);
    rig.pump_until("https on", |app| {
        app.model
            .network
            .as_ref()
            .and_then(|v| v.settings.as_ref())
            .is_some_and(|s| s.https_enabled)
    });
    rig.pump_until("the regenerate row", |app| {
        app.model
            .network
            .as_ref()
            .is_some_and(|v| v.rows().contains(&plippa_ui::NetworkRow::Regenerate))
    });
    rig.focus_row(rig.row("Regenerate certificate"));
    rig.press(InputEvent::MenuEnter);
    assert_eq!(h.calls.lock().unwrap().regenerated, 0, "the first press only asks");
    let view = rig.app.model.network.as_ref().unwrap();
    assert_eq!(
        view.row_text(plippa_ui::NetworkRow::Regenerate, None, None, false).1,
        "Push again"
    );
    rig.press(InputEvent::MenuEnter);
    rig.pump_until("the new certificate", |app| {
        app.model
            .network
            .as_ref()
            .and_then(|v| v.settings.as_ref())
            .is_some_and(|s| s.fingerprint.as_deref() == Some("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"))
    });
    assert_eq!(h.calls.lock().unwrap().regenerated, 1);
}

#[test]
fn the_pin_is_shown_where_the_owner_can_read_it() {
    let paint = |pin: &str| {
        let (api, _h) = mock(settings(true, true, Some(pin)), None);
        let mut rig = Rig::new(api);
        rig.open_network();
        let menu = rig.frame().pixels;
        rig.focus_row(rig.row("Web portal PIN"));
        rig.press(InputEvent::MenuEnter);
        assert_eq!(*rig.app.nav.current(), Route::SystemNetworkPin);
        (menu, rig.frame().pixels)
    };
    let (menu_a, pin_a) = paint("483920");
    let (menu_b, pin_b) = paint("483921");
    assert_ne!(menu_a, menu_b, "the menu row paints the digits");
    assert_ne!(pin_a, pin_b, "and so does the PIN screen");
    // The PIN screen leads with the digits in the largest type: ink in the first band below the
    // header differs between two PINs.
    let band = |fb: &Vec<_>| fb[(SCREEN_W as usize * 32)..(SCREEN_W as usize * 72)].to_vec();
    assert_ne!(band(&pin_a), band(&pin_b), "the digits are big and near the top");
    // Not set yet: the screen says so instead of painting a placeholder PIN.
    let (api, _h) = mock(settings(false, true, None), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    rig.focus_row(rig.row("Web portal PIN"));
    rig.press(InputEvent::MenuEnter);
    let fresh = rig.frame().pixels;
    assert_ne!(fresh, pin_a);
}

#[test]
fn pin_pad_shows_the_typed_digit_and_back_hands_the_screen_back() {
    // Two different digits in box 0, same everything else: the frames can only differ if the
    // box draws the digit itself rather than a mask.
    let paint = |digit: i8| {
        let (api, _h) = mock(settings(true, true, Some("483920")), None);
        let mut rig = Rig::new(api);
        rig.open_network();
        rig.focus_row(rig.row("Web portal PIN"));
        rig.press(InputEvent::MenuEnter);
        rig.press(InputEvent::MenuEnter); // edit box 0
        rig.press(InputEvent::MenuRotate(digit));
        rig.press(InputEvent::MenuEnter); // commit
        rig.frame().pixels
    };
    assert_ne!(paint(3), paint(4), "the boxes draw digits, not bullets");
}

#[test]
fn back_from_the_pin_pad_pops_the_screen() {
    let (api, h) = mock(settings(true, true, Some("483920")), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    rig.focus_row(rig.row("Web portal PIN"));
    rig.press(InputEvent::MenuEnter);
    // Six digits auto-advance the cursor onto OK; leaving from there is a plain cancel.
    for ch in "123456".chars() {
        let d = ch.to_digit(10).unwrap() as i8;
        rig.press(InputEvent::MenuEnter);
        if d != 0 {
            rig.press(InputEvent::MenuRotate(d));
        }
        rig.press(InputEvent::MenuEnter);
    }
    assert!(rig.app.model.network.as_ref().unwrap().pin_pad.as_ref().unwrap().complete());
    rig.press(InputEvent::Back);
    assert_eq!(*rig.app.nav.current(), Route::SystemNetwork);
    assert!(h.calls.lock().unwrap().pins.is_empty(), "Back never submits");
    assert!(rig.app.model.network.as_ref().unwrap().pin_pad.is_none());
}

#[test]
fn reopening_the_pin_screen_starts_from_six_empty_boxes() {
    let (api, _h) = mock(settings(true, true, Some("483920")), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    rig.focus_row(rig.row("Web portal PIN"));
    rig.press(InputEvent::MenuEnter);
    rig.press(InputEvent::MenuEnter); // edit box 0
    rig.press(InputEvent::MenuRotate(5));
    rig.press(InputEvent::MenuEnter); // commit: box 0 = 5, box 1 focused
    rig.press(InputEvent::Back); // Back on an empty box is a plain cancel
    assert_eq!(*rig.app.nav.current(), Route::SystemNetwork);
    assert!(rig.app.model.network.as_ref().unwrap().pin_pad.is_none());

    // Switching the requirement off and back on, or just reopening, always starts empty.
    rig.press(InputEvent::MenuEnter);
    assert_eq!(*rig.app.nav.current(), Route::SystemNetworkPin);
    let pad = rig.app.model.network.as_ref().unwrap().pin_pad.as_ref().unwrap();
    assert!(!pad.complete());
    assert_eq!(pad.digit_at(0), None, "no half-typed PIN survives the screen");
}

#[test]
fn the_pin_off_warning_is_shown() {
    let frame = |required: bool| {
        let (api, _h) = mock(settings(true, required, Some("483920")), None);
        let mut rig = Rig::new(api);
        rig.open_network();
        rig.frame()
    };
    let open = frame(false);
    let safe = frame(true);
    assert!(
        warn_pixels(&open) > warn_pixels(&safe) + 100,
        "the warning bar and row colour are painted: {} vs {}",
        warn_pixels(&open),
        warn_pixels(&safe)
    );
    // The portal off is not "open": no warning.
    let (api, _h) = mock(settings(false, false, Some("483920")), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    assert!(warn_pixels(&rig.frame()) <= warn_pixels(&safe));
    assert!(rig.app.model.network.as_ref().unwrap().open_warning().is_none());
    // The warning view is the model's, with the owner's words.
    let (api, _h) = mock(settings(true, false, Some("483920")), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    assert_eq!(
        rig.app.model.network.as_ref().unwrap().open_warning(),
        Some("No PIN: anyone on your network can use the portal")
    );
}

#[test]
fn every_network_state_stays_inside_the_content_box() {
    let mut worst = settings(true, false, Some("483920"));
    worst.locked = true;
    worst.fingerprint = Some(
        "AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89"
            .into(),
    );
    let mut no_dir = settings(false, true, None);
    no_dir.available = false;
    no_dir.unavailable_reason = Some("no secrets directory is configured, so the portal PIN cannot be kept".into());
    for s in [worst, no_dir, settings(true, true, Some("000001")), settings(false, true, None)] {
        let (api, _h) = mock(s, None);
        let mut rig = Rig::new(api);
        rig.open_network();
        let fb = rig.frame();
        let below = fb.pixels[(CONTENT_BOTTOM as usize * SCREEN_W as usize)..]
            .iter()
            .filter(|p| **p != BG)
            .count();
        // Only the footer hint band may paint below the content box.
        assert!(below < SCREEN_W as usize * 30, "{below}");
    }
}

#[test]
fn an_unavailable_state_dir_blocks_the_switch_and_says_why() {
    let mut s = settings(false, true, None);
    s.available = false;
    s.unavailable_reason = Some("no secrets directory is configured".into());
    let (api, h) = mock(s, None);
    let mut rig = Rig::new(api);
    rig.open_network();
    rig.focus_row(rig.row("Web portal"));
    rig.press(InputEvent::MenuEnter);
    for _ in 0..40 {
        rig.tick();
    }
    assert!(h.calls.lock().unwrap().enabled.is_empty(), "nothing is sent when it cannot work");
    let v = rig.app.model.network.as_ref().unwrap();
    assert_eq!(v.row_text(v.rows()[1], None, None, false).1, "Unavailable");
}

#[test]
fn a_lockout_shows_and_clear_lockout_clears_it() {
    let mut s = settings(true, true, Some("483920"));
    s.locked = true;
    let (api, h) = mock(s, None);
    let mut rig = Rig::new(api);
    rig.open_network();
    // IP, Web portal, HTTPS, View certificate, PIN required, PIN, Regenerate, Clear lockout,
    // Skip internet ask on tap, Wi-Fi.
    assert_eq!(rig.app.model.network.as_ref().unwrap().rows().len(), 10);
    rig.focus_row(rig.row("Clear lockout"));
    rig.press(InputEvent::MenuEnter);
    rig.pump_until("cleared", |app| {
        app.model.network.as_ref().and_then(|v| v.settings.as_ref()).is_some_and(|s| !s.locked)
    });
    assert_eq!(h.calls.lock().unwrap().clears, 1);
    assert_eq!(rig.app.model.network.as_ref().unwrap().rows().len(), 9, "the row is gone");
}

#[test]
fn wifi_is_a_row_under_network_and_back_returns_to_it() {
    let (api, _h) = mock(settings(true, true, Some("483920")), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    rig.focus_row(rig.row("Wi-Fi"));
    rig.press(InputEvent::MenuEnter);
    assert_eq!(*rig.app.nav.current(), Route::Wifi);
    rig.press(InputEvent::Back);
    assert_eq!(*rig.app.nav.current(), Route::SystemNetwork, "one level per press");
    assert_eq!(rig.app.nav.focus, rig.row("Wi-Fi"), "the cursor is where it was");
    rig.press(InputEvent::Back);
    assert_eq!(*rig.app.nav.current(), Route::System);
    assert!(rig.app.model.network.is_none(), "leaving Network drops its state");
}

#[test]
fn wifi_enter_on_the_first_row_opens_the_scan() {
    let (api, _h) = mock(settings(true, true, Some("483920")), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    rig.focus_row(rig.row("Wi-Fi"));
    rig.press(InputEvent::MenuEnter);
    assert_eq!(*rig.app.nav.current(), Route::Wifi);
    // Disconnected: the root screen is the action list `[Search, Known]` and the cursor starts on
    // its first row, so a single press scans. This is the row dispatch in `app.rs`, not the form.
    assert!(matches!(
        rig.app.model.wifi.as_ref().unwrap().phase,
        plippa_ui::WifiPhase::Current
    ));
    rig.press(InputEvent::MenuEnter);
    assert!(
        matches!(rig.app.model.wifi.as_ref().unwrap().phase, plippa_ui::WifiPhase::Scan),
        "Enter on the first row scans"
    );
}

#[test]
fn the_ip_row_opens_the_address_list_with_the_portal_off_message() {
    let (api, _h) = mock(settings(false, true, None), None);
    let mut rig = Rig::new(api);
    // A joined network gives the address list and the portal link their data.
    let _ = rig.wifi.connect("HomeLAN", "secret");
    rig.open_network();
    assert_eq!(rig.app.model.portal_enabled, Some(false), "the settings read told every screen");
    rig.focus_row(0);
    rig.press(InputEvent::MenuEnter);
    assert_eq!(*rig.app.nav.current(), Route::SystemIp);
    let off = rig.frame().pixels;
    // With the portal on the same screen paints the address instead.
    rig.app.model.portal_enabled = Some(true);
    let on = rig.frame().pixels;
    assert_ne!(off, on, "an off portal does not advertise an address");
    // The address is not painted at all while off: removing the URL changes nothing.
    rig.app.model.portal_enabled = Some(false);
    rig.app.model.system.portal_url = None;
    assert_eq!(off, rig.frame().pixels, "the off screen never drew the URL");
}

#[test]
fn portal_off_replaces_the_qr_with_the_off_message() {
    let (mut api, _h) = mock(settings(false, true, None), None);
    api.address = Some(PortalAddress {
        recommended: Some("http://192.168.1.42:8787".into()),
        candidates: vec!["http://192.168.1.42:8787".into()],
        port: 8787,
        note: None,
        portal_off: true,
    });
    let mut rig = Rig::new(api);
    rig.app.open_connect();
    rig.pump_until("connect answered", |app| {
        app.model
            .store_connect
            .as_ref()
            .is_some_and(|v| !matches!(v.phase, plippa_ui::ConnectPhase::Looking))
    });
    let view = rig.app.model.store_connect.as_ref().unwrap();
    assert!(matches!(view.phase, plippa_ui::ConnectPhase::PortalOff));
    assert!(view.qr_url().is_none(), "no QR code that goes nowhere");
    assert!(view.address_text().is_none());
    assert_eq!(rig.app.model.portal_enabled, Some(false));
    // Same answer with the portal on shows the address (a different frame).
    let off = rig.frame().pixels;
    let (mut api, _h) = mock(settings(true, true, Some("483920")), None);
    api.address = Some(PortalAddress {
        recommended: Some("http://192.168.1.42:8787".into()),
        candidates: vec!["http://192.168.1.42:8787".into()],
        port: 8787,
        note: None,
        portal_off: false,
    });
    let mut rig = Rig::new(api);
    rig.app.open_connect();
    rig.pump_until("connect answered", |app| {
        app.model.store_connect.as_ref().is_some_and(|v| v.qr_url().is_some())
    });
    assert_ne!(off, rig.frame().pixels);
}

// ---------------------------------------------------------------------------
// The frame loop never waits
// ---------------------------------------------------------------------------

#[test]
fn toggling_the_portal_never_blocks_the_frame_loop() {
    let (tx, rx): (Sender<()>, Receiver<()>) = std::sync::mpsc::channel();
    let (api, h) = mock(settings(false, true, None), Some(rx));
    let mut rig = Rig::new(api);
    rig.open_network();
    rig.focus_row(rig.row("Web portal"));
    let t = Instant::now();
    rig.press(InputEvent::MenuEnter);
    // The server is holding its answer back: every input and tick still returns at once.
    for _ in 0..30 {
        rig.tick();
        rig.press(InputEvent::MenuRotate(0));
    }
    assert!(
        t.elapsed() < Duration::from_millis(1_500),
        "the loop waited for the slow answer: {:?}",
        t.elapsed()
    );
    assert!(rig.app.model.network.as_ref().unwrap().busy, "the change is in flight");
    // A second press while one is in flight is ignored, not queued.
    rig.press(InputEvent::MenuEnter);
    assert!(h.calls.lock().unwrap().enabled.is_empty(), "nothing answered yet");
    // The frame still paints while waiting.
    assert!(rig.frame().pixels.iter().any(|p| *p != BG));
    tx.send(()).unwrap();
    rig.pump_until("answer applied", |app| {
        app.model.network.as_ref().is_some_and(|v| !v.busy)
    });
    assert_eq!(h.calls.lock().unwrap().enabled, [true], "exactly one change was made");
}

// ---------------------------------------------------------------------------
// Typing a PIN
// ---------------------------------------------------------------------------

#[test]
fn the_owner_types_a_pin_and_it_replaces_the_old_one() {
    let (api, h) = mock(settings(true, true, Some("483920")), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    rig.focus_row(rig.row("Web portal PIN"));
    rig.press(InputEvent::MenuEnter);
    assert_eq!(*rig.app.nav.current(), Route::SystemNetworkPin);
    rig.type_pin("135790");
    rig.pump_until("back on the menu", |app| *app.nav.current() == Route::SystemNetwork);
    assert_eq!(h.calls.lock().unwrap().pins, ["135790"]);
    let v = rig.app.model.network.as_ref().unwrap();
    assert_eq!(v.pin_display().as_deref(), Some("135 790"));
}

#[test]
fn a_refused_pin_stays_on_the_screen_with_the_reason() {
    let (api, h) = mock(settings(true, true, Some("483920")), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    rig.focus_row(rig.row("Web portal PIN"));
    rig.press(InputEvent::MenuEnter);
    rig.type_pin("000000");
    rig.pump_until("refusal shown", |app| {
        app.model.network.as_ref().is_some_and(|v| v.notice.is_some())
    });
    assert_eq!(*rig.app.nav.current(), Route::SystemNetworkPin, "still on the keypad");
    assert_eq!(
        rig.app.model.network.as_ref().unwrap().notice.as_deref(),
        Some("The PIN must be six digits")
    );
    assert_eq!(h.calls.lock().unwrap().pins, ["000000"]);
    assert_eq!(h.state.lock().unwrap().pin.as_deref(), Some("483920"), "the PIN did not change");
    assert!(warn_pixels(&rig.frame()) > 0, "the reason is painted");
}

#[test]
fn an_incomplete_pin_cannot_be_submitted() {
    let (api, h) = mock(settings(true, true, Some("483920")), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    rig.focus_row(rig.row("Web portal PIN"));
    rig.press(InputEvent::MenuEnter);
    rig.type_pin("1357");
    for _ in 0..20 {
        rig.tick();
    }
    // UI_14: the OK box is not reachable while a box is empty, so four digits cannot be sent
    // and the "Type six digits" refusal is unreachable by construction.
    assert!(h.calls.lock().unwrap().pins.is_empty(), "four digits are not a PIN");
    assert_eq!(
        rig.app.model.network.as_ref().unwrap().notice, None,
        "no refusal to show: the press only opened the next empty box"
    );
    assert_eq!(*rig.app.nav.current(), Route::SystemNetworkPin);
    // Back leaves the box being edited, then the screen.
    rig.press(InputEvent::Back);
    assert_eq!(*rig.app.nav.current(), Route::SystemNetworkPin, "Back left the box first");
    rig.press(InputEvent::Back);
    assert_eq!(*rig.app.nav.current(), Route::SystemNetwork);
    assert!(h.calls.lock().unwrap().pins.is_empty());
    assert!(
        rig.app.model.network.as_ref().unwrap().pin_pad.is_none(),
        "leaving the screen drops the half-typed pad"
    );
}

// ---------------------------------------------------------------------------
// The tap consent and the certificate screen
// ---------------------------------------------------------------------------

#[test]
fn skip_internet_ask_on_tap_toggles_and_is_saved_with_the_playback_settings() {
    let (api, _h) = mock(settings(true, true, Some("483920")), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    let path = rig.play_mode_path.clone();
    assert!(!path.exists(), "nothing is written until the person chooses");
    let row = plippa_ui::NetworkRow::SkipInternetAsk;
    let v = rig.app.model.network.as_ref().unwrap();
    assert!(v.rows().contains(&row), "the consent is a Network row now");
    assert_eq!(v.row_text(row, None, None, false).1, "Off");

    rig.focus_row(rig.row("Skip internet ask on tap"));
    rig.press(InputEvent::MenuEnter);
    assert_eq!(*rig.app.nav.current(), Route::SystemNetwork, "a toggle, not a screen");
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("\"skip_internet_consent\": true"), "{saved}");
    let loaded = plippa_ui::PlayModeStore::load_or_default(&path).settings;
    assert!(loaded.resolve_skip_internet_consent());
    let v = rig.app.model.network.as_ref().unwrap();
    assert_eq!(
        v.row_text(row, None, None, loaded.resolve_skip_internet_consent()).1,
        "On"
    );

    rig.press(InputEvent::MenuEnter);
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("\"skip_internet_consent\": false"), "{saved}");

    // Reopening the menu reads the choice from the file, not from UI-local state.
    rig.press(InputEvent::Back);
    assert_eq!(*rig.app.nav.current(), Route::System);
    rig.app.nav.focus = 0;
    rig.open_network();
    let loaded = plippa_ui::PlayModeStore::load_or_default(&path).settings;
    assert!(!loaded.resolve_skip_internet_consent());
    let v = rig.app.model.network.as_ref().unwrap();
    assert_eq!(
        v.row_text(row, None, None, loaded.resolve_skip_internet_consent()).1,
        "Off"
    );
}

#[test]
fn view_certificate_opens_a_screen_instead_of_painting_the_fingerprint_in_the_menu() {
    let with_fingerprint = |fingerprint: &str| {
        let mut s = settings(true, true, Some("483920"));
        s.fingerprint = Some(fingerprint.into());
        s
    };
    let paint = |fingerprint: &str| {
        let (api, _h) = mock(with_fingerprint(fingerprint), None);
        let mut rig = Rig::new(api);
        rig.open_network();
        let menu = rig.frame().pixels;
        rig.focus_row(rig.row("View certificate"));
        rig.press(InputEvent::MenuEnter);
        assert_eq!(*rig.app.nav.current(), Route::SystemCertificate);
        (menu, rig.frame().pixels)
    };
    let a = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789";
    let b = "00112233445566778899AABBCCDDEEFF00112233445566778899AABBCCDDEEFF";
    let (menu_a, screen_a) = paint(a);
    let (menu_b, screen_b) = paint(b);
    assert_eq!(menu_a, menu_b, "the menu no longer draws the fingerprint");
    assert_ne!(screen_a, screen_b, "the certificate screen does");

    // Back returns to Network with the cursor on the row that was pressed.
    let (api, _h) = mock(with_fingerprint(a), None);
    let mut rig = Rig::new(api);
    rig.open_network();
    rig.focus_row(rig.row("View certificate"));
    rig.press(InputEvent::MenuEnter);
    rig.press(InputEvent::Back);
    assert_eq!(*rig.app.nav.current(), Route::SystemNetwork);
    assert_eq!(rig.app.nav.focus, rig.row("View certificate"));
}
