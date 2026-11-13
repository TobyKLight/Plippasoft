//! System > Network (SECFIX_02): the IP address, the web portal's switches and PIN, and Wi-Fi.
//!
//! The device owns the portal's state through the server's loopback control listener. This
//! module is only the screen's model: which rows exist, what each says, what a refusal reads
//! like, and the small state machine around "asking the server" (never a blocking call: the
//! app sends a [`crate::store_worker::StoreRequest`] and folds the answer in with
//! [`NetworkView::apply`]).
//!
//! Rows, in order: `IP address`, `Web portal` (On/Off), `Web portal HTTPS` (On/Off),
//! `View certificate` (only while HTTPS is On; opens the certificate screen),
//! `Web portal PIN required` (On/Off), `Web portal PIN` (shows the six digits; opens the screen
//! where the owner types a new one), `Regenerate certificate` (only while HTTPS is On),
//! `Clear lockout` (only while sign-in is locked), `Skip internet ask on tap` (On/Off), `Wi-Fi`.
//! There is no "generate a new PIN" action: the only generated PIN is the first one, created the
//! first time the portal is switched on. A typed PIN is any six digits: the owner's choice is
//! not second-guessed.

use crate::pinpad::PinPad;
use crate::store_api::{PortalSettingsInfo, StoreApiError};

/// How long the server may take to answer before the screen says so.
pub const NETWORK_TIMEOUT_MS: u64 = 6_000;

/// Digits in the PIN.
pub const PIN_DIGITS: usize = 6;

/// Shown under the rows while the portal is on and asks for no PIN.
pub const NO_PIN_WARNING: &str = "No PIN: anyone on your network can use the portal";

/// Shown under the rows while the portal is on and served without TLS (SECFIX_03).
pub const NO_HTTPS_WARNING: &str = "No HTTPS: the store password is sent in clear text";

/// How long `Regenerate certificate` stays armed for the confirming second press.
pub const REGENERATE_CONFIRM_MS: u64 = 8_000;

/// One row of the Network menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkRow {
    /// `IP address`: opens the address list.
    Ip,
    /// `Web portal`: On / Off.
    Portal,
    /// `Web portal HTTPS` (SECFIX_03): On / Off.
    Https,
    /// `View certificate`: opens the certificate screen (fingerprint and how to compare it).
    /// Only while HTTPS is on: with it off there is no certificate.
    Certificate,
    /// `Web portal PIN required`: On / Off.
    PinRequired,
    /// `Web portal PIN`: the digits; opens the screen to type a new one.
    Pin,
    /// `Regenerate certificate` (SECFIX_03): makes a new key and certificate; asks first.
    Regenerate,
    /// `Clear lockout`: only while sign-in is locked.
    ClearLockout,
    /// `Skip internet ask on tap` (BCS_11): a tap that needs the internet goes ahead without
    /// the confirmation. The value lives in `play_mode.json`, not in the portal settings; the
    /// app passes it in (`row_text`'s `skip_internet_ask`).
    SkipInternetAsk,
    /// `Wi-Fi`: the Wi-Fi screens.
    Wifi,
}

impl NetworkRow {
    /// The row's label.
    pub fn label(self) -> &'static str {
        match self {
            NetworkRow::Ip => "IP address",
            NetworkRow::Portal => "Web portal",
            NetworkRow::Https => "Web portal HTTPS",
            NetworkRow::Certificate => "View certificate",
            NetworkRow::PinRequired => "Web portal PIN required",
            NetworkRow::Pin => "Web portal PIN",
            NetworkRow::Regenerate => "Regenerate certificate",
            NetworkRow::ClearLockout => "Clear lockout",
            NetworkRow::SkipInternetAsk => "Skip internet ask on tap",
            NetworkRow::Wifi => "Wi-Fi",
        }
    }

    /// The footer hint while the row is focused.
    pub fn hint(self) -> &'static str {
        match self {
            NetworkRow::Ip => "Push: addresses and portal link",
            NetworkRow::Portal => "Push: switch the web portal on or off",
            NetworkRow::Https => "Push: serve the portal over HTTPS, or not",
            NetworkRow::Certificate => "Push: the certificate a browser shows",
            NetworkRow::PinRequired => "Push: ask browsers for the PIN, or not",
            NetworkRow::Pin => "Push: type a new six-digit PIN",
            NetworkRow::Regenerate => "Push twice: make a new certificate",
            NetworkRow::ClearLockout => "Push: let browsers try the PIN again",
            NetworkRow::SkipInternetAsk => "Push: skip the internet ask on a tap",
            NetworkRow::Wifi => "Push: choose a Wi-Fi network",
        }
    }
}

/// What the screen knows about the settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkPhase {
    /// Asked, nothing back yet.
    Loading,
    /// The settings are in [`NetworkView::settings`].
    Ready,
    /// They could not be read, and why.
    Failed(String),
}

/// The Network menu and the PIN screen's state.
#[derive(Debug, Clone)]
pub struct NetworkView {
    pub phase: NetworkPhase,
    /// The last settings the server sent.
    pub settings: Option<PortalSettingsInfo>,
    /// A change is on its way to the server (a second one waits for the answer).
    pub busy: bool,
    /// Loop time of the call in flight (drives the timeout).
    pub requested_at_ms: u64,
    /// The last refusal, in words ("The PIN must be six digits").
    pub notice: Option<String>,
    /// The new-PIN pad while the PIN screen is open.
    pub pin_pad: Option<PinPad>,
    /// The PIN screen sent a PIN and is waiting for the answer.
    pub pin_pending: bool,
    /// `Regenerate certificate` is armed until this loop time; a second press before it
    /// makes the new certificate. `None` when the row has not been pressed.
    pub regenerate_armed_until_ms: Option<u64>,
}

impl NetworkView {
    /// A view waiting for the settings.
    pub fn looking(now_ms: u64) -> Self {
        Self {
            phase: NetworkPhase::Loading,
            settings: None,
            busy: false,
            requested_at_ms: now_ms,
            notice: None,
            pin_pad: None,
            pin_pending: false,
            regenerate_armed_until_ms: None,
        }
    }

    /// The rows, in draw order.
    pub fn rows(&self) -> Vec<NetworkRow> {
        let mut rows = vec![
            NetworkRow::Ip,
            NetworkRow::Portal,
            NetworkRow::Https,
        ];
        // The certificate rows only exist while HTTPS is on: with it off there is no
        // certificate to look at or to regenerate.
        if self.settings.as_ref().is_some_and(|s| s.https_enabled) {
            rows.push(NetworkRow::Certificate);
        }
        rows.push(NetworkRow::PinRequired);
        rows.push(NetworkRow::Pin);
        if self.settings.as_ref().is_some_and(|s| s.https_enabled) {
            rows.push(NetworkRow::Regenerate);
        }
        if self.settings.as_ref().is_some_and(|s| s.locked) {
            rows.push(NetworkRow::ClearLockout);
        }
        rows.push(NetworkRow::SkipInternetAsk);
        rows.push(NetworkRow::Wifi);
        rows
    }

    /// Fold an answer in: the settings as they are now, or why the call failed.
    pub fn apply(&mut self, result: Result<PortalSettingsInfo, StoreApiError>) {
        self.busy = false;
        match result {
            Ok(settings) => {
                self.phase = NetworkPhase::Ready;
                self.settings = Some(settings);
                self.notice = None;
            }
            Err(error) => {
                let text = refusal_text(&error);
                if self.settings.is_some() {
                    // A change was refused: keep showing what is true and say why.
                    self.notice = Some(text);
                } else {
                    self.phase = NetworkPhase::Failed(text);
                }
            }
        }
    }

    /// True when the first read has been outstanding for too long.
    pub fn timed_out(&self, now_ms: u64) -> bool {
        matches!(self.phase, NetworkPhase::Loading)
            && now_ms.saturating_sub(self.requested_at_ms) >= NETWORK_TIMEOUT_MS
    }

    /// The portal is on and asks for no PIN: the screen keeps a warning up.
    pub fn open_warning(&self) -> Option<&'static str> {
        self.settings
            .as_ref()
            .filter(|s| s.enabled && !s.pin_required)
            .map(|_| NO_PIN_WARNING)
    }

    /// The portal is on and served in the clear: the store password would cross the LAN
    /// unencrypted (SECFIX_03).
    pub fn clear_text_warning(&self) -> Option<&'static str> {
        self.settings
            .as_ref()
            .filter(|s| s.enabled && !s.https_enabled)
            .map(|_| NO_HTTPS_WARNING)
    }

    /// Every warning the screen keeps up, in the order they are drawn.
    pub fn warnings(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if let Some(warning) = self.clear_text_warning() {
            out.push(warning);
        }
        if let Some(warning) = self.open_warning() {
            out.push(warning);
        }
        out
    }

    /// The certificate fingerprint as a browser shows it: uppercase hex pairs, colon separated.
    /// `None` while HTTPS is off, before a certificate exists, or when the value is not a
    /// SHA-256 in hex (nothing is guessed at).
    pub fn fingerprint_display(&self) -> Option<String> {
        let settings = self.settings.as_ref()?;
        if !settings.https_enabled {
            return None;
        }
        let hex = settings.fingerprint.as_deref()?;
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut out = String::with_capacity(95);
        for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
            if i > 0 {
                out.push(':');
            }
            out.push(char::from(pair[0]).to_ascii_uppercase());
            out.push(char::from(pair[1]).to_ascii_uppercase());
        }
        Some(out)
    }

    /// Arm the second press for a new certificate.
    pub fn arm_regenerate(&mut self, now_ms: u64) {
        self.regenerate_armed_until_ms = Some(now_ms.saturating_add(REGENERATE_CONFIRM_MS));
    }

    /// The row is waiting for its confirming press.
    pub fn regenerate_armed(&self) -> bool {
        self.regenerate_armed_until_ms.is_some()
    }

    /// The confirming press: the certificate change is on its way.
    pub fn take_regenerate(&mut self) -> bool {
        self.regenerate_armed_until_ms.take().is_some()
    }

    /// Drop an arm nobody confirmed; true when something changed.
    pub fn expire_regenerate(&mut self, now_ms: u64) -> bool {
        match self.regenerate_armed_until_ms {
            Some(until) if now_ms >= until => {
                self.regenerate_armed_until_ms = None;
                true
            }
            _ => false,
        }
    }

    /// The line under the rows that explains a state, if there is one.
    pub fn status_line(&self) -> Option<String> {
        if let NetworkPhase::Failed(why) = &self.phase {
            return Some(why.clone());
        }
        let settings = self.settings.as_ref()?;
        if !settings.available {
            return Some(
                settings
                    .unavailable_reason
                    .clone()
                    .unwrap_or_else(|| "The portal cannot be turned on here".into()),
            );
        }
        if settings.enabled && !settings.listening {
            return Some(match settings.listen_error.as_deref() {
                Some(why) => format!("Web portal is on but its port is not open yet: {why}"),
                None => "Web portal is on but its port is not open yet".into(),
            });
        }
        if let Some(why) = settings.unsaved.as_deref() {
            return Some(format!("Done, but not saved yet (retrying): {why}"));
        }
        if settings.locked {
            return Some("Sign-in is locked after wrong PINs".into());
        }
        None
    }

    /// `(label, value)` for `row`. `ip` and `ssid` come from the Wi-Fi backend;
    /// `skip_internet_ask` is the resolved tap-consent setting, which belongs to
    /// `play_mode.json` and is owned by the app, not by the portal settings.
    pub fn row_text(
        &self,
        row: NetworkRow,
        ip: Option<&str>,
        ssid: Option<&str>,
        skip_internet_ask: bool,
    ) -> (&'static str, String) {
        let on_off = |on: bool| if on { "On" } else { "Off" }.to_string();
        let value = match row {
            NetworkRow::Ip => ip.unwrap_or_default().to_string(),
            NetworkRow::Wifi => ssid.unwrap_or_default().to_string(),
            NetworkRow::ClearLockout => "Locked".into(),
            NetworkRow::Certificate => "Open".into(),
            NetworkRow::SkipInternetAsk => on_off(skip_internet_ask),
            NetworkRow::Regenerate if self.regenerate_armed() => "Push again".into(),
            NetworkRow::Regenerate => "Change".into(),
            NetworkRow::Portal | NetworkRow::Https | NetworkRow::PinRequired | NetworkRow::Pin => {
                match (&self.phase, &self.settings) {
                    (_, Some(s)) if !s.available => "Unavailable".into(),
                    (_, Some(s)) => match row {
                        NetworkRow::Portal if s.enabled && !s.listening => "On, not open".into(),
                        NetworkRow::Portal => on_off(s.enabled),
                        NetworkRow::Https => on_off(s.https_enabled),
                        NetworkRow::PinRequired => on_off(s.pin_required),
                        _ => s
                            .pin
                            .clone()
                            .unwrap_or_else(|| "not set yet".to_string()),
                    },
                    (NetworkPhase::Loading, None) => "...".into(),
                    (_, None) => "Unavailable".into(),
                }
            }
        };
        (row.label(), value)
    }

    /// True when a toggle row can act now (the settings are known and no change is in flight).
    pub fn can_change(&self) -> bool {
        !self.busy && self.settings.as_ref().is_some_and(|s| s.available)
    }

    /// The six digits for the PIN screen, spaced for reading (`483 920`), or `None` before the
    /// portal has ever been switched on.
    pub fn pin_display(&self) -> Option<String> {
        let pin = self.settings.as_ref()?.pin.as_deref()?;
        (pin.len() == PIN_DIGITS).then(|| format!("{} {}", &pin[..3], &pin[3..]))
    }
}

/// A refusal or failure in the owner's words.
pub fn refusal_text(error: &StoreApiError) -> String {
    match error.code() {
        Some("bad_pin") => return "The PIN must be six digits".into(),
        Some("bind_failed") => return "The portal's port could not be opened".into(),
        _ => {}
    }
    match error {
        StoreApiError::Offline(_) => "Plippa's service did not answer".into(),
        StoreApiError::Refused { message, .. } if !message.trim().is_empty() => {
            crate::screens::screen_text(message)
        }
        _ => "The portal settings could not be read".into(),
    }
}

/// A typed PIN, or why it is not one.
pub fn parse_typed_pin(text: &str) -> Result<String, &'static str> {
    let pin = text.trim();
    if pin.len() == PIN_DIGITS && pin.bytes().all(|b| b.is_ascii_digit()) {
        Ok(pin.to_string())
    } else {
        Err("Type six digits")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn ready(s: PortalSettingsInfo) -> NetworkView {
        let mut v = NetworkView::looking(0);
        v.apply(Ok(s));
        v
    }

    #[test]
    fn the_rows_and_their_order() {
        let v = ready(settings(true, true, Some("483920")));
        let labels: Vec<_> = v.rows().iter().map(|r| r.label()).collect();
        assert_eq!(
            labels,
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
        // Not read yet: the switch rows, so the cursor is near where it will end up. The
        // certificate rows need the settings (they exist only while HTTPS is on).
        assert_eq!(
            NetworkView::looking(0)
                .rows()
                .iter()
                .map(|r| r.label())
                .collect::<Vec<_>>(),
            [
                "IP address",
                "Web portal",
                "Web portal HTTPS",
                "Web portal PIN required",
                "Web portal PIN",
                "Skip internet ask on tap",
                "Wi-Fi"
            ]
        );
    }

    /// The tap consent is a Network row now (it used to be the last Playback settings row), and
    /// its value comes from the caller because it is stored in `play_mode.json`.
    #[test]
    fn the_tap_consent_row_shows_the_setting_it_is_given() {
        let v = ready(settings(true, true, Some("483920")));
        assert!(v.rows().contains(&NetworkRow::SkipInternetAsk));
        assert_eq!(
            v.row_text(NetworkRow::SkipInternetAsk, None, None, false),
            ("Skip internet ask on tap", "Off".into())
        );
        assert_eq!(
            v.row_text(NetworkRow::SkipInternetAsk, None, None, true).1,
            "On"
        );
        assert_eq!(NetworkRow::SkipInternetAsk.label(), "Skip internet ask on tap");
        assert!(
            NetworkRow::SkipInternetAsk.label().chars().count() <= 24,
            "the label fits the row beside its value"
        );
    }

    /// `View certificate` exists only while HTTPS is on, and sits with the other certificate
    /// rows rather than printing the fingerprint in the menu.
    #[test]
    fn the_certificate_row_follows_https_and_has_no_fingerprint_in_the_menu() {
        let mut on_settings = settings(true, true, Some("483920"));
        on_settings.fingerprint =
            Some("ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789".into());
        let on = ready(on_settings);
        let rows = on.rows();
        let cert = rows.iter().position(|r| *r == NetworkRow::Certificate).unwrap();
        assert_eq!(rows[cert + 1], NetworkRow::PinRequired);
        assert_eq!(
            on.row_text(NetworkRow::Certificate, None, None, false),
            ("View certificate", "Open".into())
        );
        let mut off_settings = settings(true, true, Some("483920"));
        off_settings.https_enabled = false;
        let off = ready(off_settings);
        assert!(!off.rows().contains(&NetworkRow::Certificate));
        assert!(!off.rows().contains(&NetworkRow::Regenerate));
        // The fingerprint formatting still exists; the certificate screen is the only reader.
        assert!(on.fingerprint_display().is_some());
    }

    #[test]
    fn a_lockout_adds_the_clear_row_before_the_tap_consent() {
        let mut s = settings(true, true, Some("483920"));
        s.locked = true;
        let v = ready(s);
        let rows = v.rows();
        assert_eq!(rows[rows.len() - 3], NetworkRow::ClearLockout);
        assert_eq!(rows[rows.len() - 2], NetworkRow::SkipInternetAsk);
        assert_eq!(rows[rows.len() - 1], NetworkRow::Wifi);
        assert!(v.status_line().unwrap().contains("locked"));
    }

    #[test]
    fn values_follow_the_settings() {
        let v = ready(settings(true, false, Some("048215")));
        assert_eq!(v.row_text(NetworkRow::Portal, None, None, false), ("Web portal", "On".into()));
        assert_eq!(
            v.row_text(NetworkRow::Https, None, None, false),
            ("Web portal HTTPS", "On".into())
        );
        assert_eq!(
            v.row_text(NetworkRow::Regenerate, None, None, false),
            ("Regenerate certificate", "Change".into())
        );
        assert_eq!(
            v.row_text(NetworkRow::PinRequired, None, None, false),
            ("Web portal PIN required", "Off".into())
        );
        assert_eq!(v.row_text(NetworkRow::Pin, None, None, false).1, "048215", "leading zero kept");
        assert_eq!(v.row_text(NetworkRow::Ip, Some("192.168.1.9"), None, false).1, "192.168.1.9");
        assert_eq!(v.row_text(NetworkRow::Wifi, None, Some("home"), false).1, "home");
        // Before the first enable there is no PIN to show.
        let fresh = ready(settings(false, true, None));
        assert_eq!(fresh.row_text(NetworkRow::Pin, None, None, false).1, "not set yet");
        assert_eq!(fresh.row_text(NetworkRow::Portal, None, None, false).1, "Off");
        // Reading, failed, unavailable.
        assert_eq!(NetworkView::looking(0).row_text(NetworkRow::Portal, None, None, false).1, "...");
        let mut failed = NetworkView::looking(0);
        failed.apply(Err(StoreApiError::Offline("down".into())));
        assert_eq!(failed.row_text(NetworkRow::Portal, None, None, false).1, "Unavailable");
        let mut no_dir = settings(false, true, None);
        no_dir.available = false;
        no_dir.unavailable_reason = Some("no secrets directory".into());
        let v = ready(no_dir);
        assert_eq!(v.row_text(NetworkRow::Portal, None, None, false).1, "Unavailable");
        assert_eq!(v.status_line().as_deref(), Some("no secrets directory"));
        assert!(!v.can_change(), "nothing can be switched on without somewhere to keep the PIN");
    }

    #[test]
    fn the_pin_is_spaced_for_reading_and_absent_before_the_first_enable() {
        assert_eq!(
            ready(settings(true, true, Some("483920"))).pin_display().as_deref(),
            Some("483 920")
        );
        assert_eq!(ready(settings(false, true, None)).pin_display(), None);
    }

    #[test]
    fn the_open_warning_shows_only_while_the_portal_is_on_without_a_pin() {
        assert!(ready(settings(true, false, Some("483920"))).open_warning().is_some());
        assert!(ready(settings(true, true, Some("483920"))).open_warning().is_none());
        assert!(ready(settings(false, false, Some("483920"))).open_warning().is_none());
        assert!(NetworkView::looking(0).open_warning().is_none());
    }

    #[test]
    fn a_refused_change_keeps_the_settings_and_says_why() {
        let mut v = ready(settings(true, true, Some("483920")));
        v.busy = true;
        v.apply(Err(StoreApiError::Refused {
            status: 400,
            code: Some("bad_pin".into()),
            message: "x".into(),
        }));
        assert!(!v.busy);
        assert_eq!(v.phase, NetworkPhase::Ready);
        assert_eq!(v.settings.as_ref().unwrap().pin.as_deref(), Some("483920"));
        assert_eq!(v.notice.as_deref(), Some("The PIN must be six digits"));
        // The next good answer clears it.
        v.apply(Ok(settings(true, true, Some("135790"))));
        assert_eq!(v.notice, None);
    }

    #[test]
    fn the_first_read_times_out_into_a_readable_state() {
        let v = NetworkView::looking(1_000);
        assert!(!v.timed_out(1_000 + NETWORK_TIMEOUT_MS - 1));
        assert!(v.timed_out(1_000 + NETWORK_TIMEOUT_MS));
        assert!(!ready(settings(true, true, None)).timed_out(1_000_000));
    }

    #[test]
    fn a_typed_pin_must_be_six_digits() {
        assert_eq!(parse_typed_pin("135790"), Ok("135790".into()));
        assert_eq!(parse_typed_pin(" 048215 "), Ok("048215".into()));
        for bad in ["", "12345", "1234567", "12a456", "１２３４５６"] {
            assert!(parse_typed_pin(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_enabled_portal_that_is_not_listening_says_so() {
        let mut s = settings(true, true, Some("483920"));
        s.listening = false;
        s.listen_error = Some("address in use".into());
        let v = ready(s);
        assert_eq!(v.row_text(NetworkRow::Portal, None, None, false).1, "On, not open");
        assert!(v.status_line().unwrap().contains("address in use"));
        let mut s = settings(true, true, Some("483920"));
        s.listening = true;
        s.unsaved = Some("disk full".into());
        assert!(ready(s).status_line().unwrap().contains("not saved"));
        // Listening and saved: nothing to report.
        let mut s = settings(true, true, Some("483920"));
        s.listening = true;
        assert!(ready(s).status_line().is_none());
    }

    #[test]
    fn debug_of_the_settings_never_shows_the_pin() {
        let text = format!("{:?}", settings(true, true, Some("483920")));
        assert!(!text.contains("483920"), "{text}");
    }

    #[test]
    fn https_off_reads_off_drops_the_regenerate_row_and_warns_while_the_portal_is_on() {
        let mut off = settings(true, true, Some("483920"));
        off.https_enabled = false;
        let v = ready(off);
        assert_eq!(v.row_text(NetworkRow::Https, None, None, false).1, "Off");
        assert!(
            !v.rows().contains(&NetworkRow::Regenerate),
            "nothing to regenerate while HTTPS is off"
        );
        assert_eq!(v.clear_text_warning(), Some(NO_HTTPS_WARNING));
        assert!(v.warnings().contains(&NO_HTTPS_WARNING));
        // With the portal off there is nothing in the clear to warn about.
        let mut parked = settings(false, true, Some("483920"));
        parked.https_enabled = false;
        assert_eq!(ready(parked).clear_text_warning(), None);
        // And nothing before the settings arrive.
        assert_eq!(NetworkView::looking(0).clear_text_warning(), None);
        assert!(NetworkView::looking(0).warnings().is_empty());
    }

    #[test]
    fn both_warnings_stay_up_together() {
        let mut s = settings(true, false, Some("483920"));
        s.https_enabled = false;
        assert_eq!(ready(s).warnings(), vec![NO_HTTPS_WARNING, NO_PIN_WARNING]);
    }

    #[test]
    fn the_fingerprint_is_shown_as_a_browser_shows_it() {
        let hex = "0123456789abcdef".repeat(4);
        let mut s = settings(true, true, Some("483920"));
        s.fingerprint = Some(hex);
        let shown = ready(s).fingerprint_display().expect("a fingerprint");
        assert!(shown.starts_with("01:23:45:67:89:AB:CD:EF"), "{shown}");
        assert_eq!(shown.len(), 95, "32 pairs and 31 colons");
        assert!(!shown.contains('-'));
        // Nothing to show while HTTPS is off, before a certificate, or for a value that is not
        // a SHA-256 in hex.
        let mut off = settings(true, true, Some("483920"));
        off.https_enabled = false;
        off.fingerprint = Some("0123456789abcdef".repeat(4));
        assert_eq!(ready(off).fingerprint_display(), None);
        assert_eq!(ready(settings(true, true, None)).fingerprint_display(), None);
        let mut junk = settings(true, true, Some("483920"));
        junk.fingerprint = Some("AB:CD".into());
        assert_eq!(ready(junk).fingerprint_display(), None);
        let mut short = settings(true, true, Some("483920"));
        short.fingerprint = Some("0123456789abcdef".repeat(3));
        assert_eq!(ready(short).fingerprint_display(), None);
    }

    #[test]
    fn regenerate_asks_for_a_second_press_before_it_expires() {
        let mut v = ready(settings(true, true, Some("483920")));
        assert!(!v.regenerate_armed());
        v.arm_regenerate(1_000);
        assert!(v.regenerate_armed());
        assert_eq!(v.row_text(NetworkRow::Regenerate, None, None, false).1, "Push again");
        // The confirming press consumes the arm.
        assert!(v.take_regenerate());
        assert!(!v.take_regenerate());
        assert!(!v.regenerate_armed());
        assert_eq!(v.row_text(NetworkRow::Regenerate, None, None, false).1, "Change");
        // An arm nobody confirms lapses.
        v.arm_regenerate(1_000);
        assert!(!v.expire_regenerate(1_000 + REGENERATE_CONFIRM_MS - 1));
        assert!(v.regenerate_armed());
        assert!(v.expire_regenerate(1_000 + REGENERATE_CONFIRM_MS));
        assert!(!v.regenerate_armed());
        assert!(!v.expire_regenerate(1_000_000));
    }
}
