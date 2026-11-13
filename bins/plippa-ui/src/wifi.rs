//! Wi‑Fi screens: the root menu, the scan list, known networks, and the connect state machine.
//!
//! Phases (UI_05, UI_14): **Current** (a menu of actions) → **Scan** / **Known** →
//! (**Keyboard** if needed) → **Connecting** → back to **Current**; `Forget` goes through
//! **ConfirmForget** and `Known` through **KnownActions**. Credentials stay in NetworkManager.

use crate::keyboard::{KeyboardAction, KeyboardEvent, KeyboardField};

/// Outcome of a connect attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiConnectResult {
    Ok { ip: String },
    Failed { message: String },
}

/// Backend for listing / joining networks.
pub trait WifiBackend {
    fn scan(&mut self) -> Vec<String>;
    fn connect(&mut self, ssid: &str, password: &str) -> WifiConnectResult;
    fn current_ip(&self) -> Option<String>;
    fn portal_url(&self) -> Option<String>;
    /// Currently associated SSID, if any.
    fn current_ssid(&self) -> Option<String>;
    /// All IPv4 addresses currently configured (may be empty).
    fn all_ips(&self) -> Vec<String>;
    /// True when NetworkManager (or mock) already has credentials for `ssid`.
    fn has_saved_credentials(&self, ssid: &str) -> bool;
    /// Saved wireless profiles, sorted and deduplicated. Empty when there is no backend.
    fn saved_networks(&self) -> Vec<String>;
    /// Drop the current association but keep the saved profile.
    ///
    /// `Err` is a sentence for the screen, never raw `nmcli` stderr (same rule as `auth_hint`).
    fn disconnect(&mut self) -> Result<(), String>;
    /// Delete the saved profile. Does not disconnect first; the caller orders the two.
    fn forget(&mut self, ssid: &str) -> Result<(), String>;
}

/// Deterministic mock for tests.
#[derive(Debug, Clone)]
pub struct MockWifi {
    pub networks: Vec<String>,
    pub accept_password: String,
    pub ip: Option<String>,
    pub portal_port: u16,
    pub current_ssid: Option<String>,
    /// SSIDs that skip the password keyboard (pretend NM already knows them).
    pub known_ssids: Vec<String>,
    /// Extra addresses returned by [`WifiBackend::all_ips`] (plus `ip` if set).
    pub extra_ips: Vec<String>,
    /// `Some(reason)` makes [`WifiBackend::forget`] fail with that sentence.
    pub fail_forget: Option<String>,
}

impl Default for MockWifi {
    fn default() -> Self {
        Self {
            networks: vec!["HomeLAN".into(), "CafeGuest".into()],
            accept_password: "secret".into(),
            ip: None,
            portal_port: 8787,
            current_ssid: None,
            known_ssids: Vec::new(),
            extra_ips: Vec::new(),
            fail_forget: None,
        }
    }
}

impl WifiBackend for MockWifi {
    fn scan(&mut self) -> Vec<String> {
        self.networks.clone()
    }

    fn connect(&mut self, ssid: &str, password: &str) -> WifiConnectResult {
        if !self.networks.iter().any(|n| n == ssid) {
            return WifiConnectResult::Failed {
                message: format!("Unknown network `{ssid}`"),
            };
        }
        let known = self.known_ssids.iter().any(|s| s == ssid);
        if !known && password != self.accept_password {
            return WifiConnectResult::Failed {
                message: "Incorrect password".into(),
            };
        }
        self.ip = Some("192.168.1.42".into());
        self.current_ssid = Some(ssid.to_string());
        if !self.known_ssids.iter().any(|s| s == ssid) {
            self.known_ssids.push(ssid.to_string());
        }
        WifiConnectResult::Ok {
            ip: self.ip.clone().unwrap(),
        }
    }

    fn current_ip(&self) -> Option<String> {
        self.ip.clone()
    }

    fn portal_url(&self) -> Option<String> {
        self.ip
            .as_ref()
            .map(|ip| format!("http://{ip}:{}", self.portal_port))
    }

    fn current_ssid(&self) -> Option<String> {
        self.current_ssid.clone()
    }

    fn all_ips(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(ip) = &self.ip {
            out.push(ip.clone());
        }
        for e in &self.extra_ips {
            if !out.iter().any(|x| x == e) {
                out.push(e.clone());
            }
        }
        out
    }

    fn has_saved_credentials(&self, ssid: &str) -> bool {
        self.known_ssids.iter().any(|s| s == ssid)
    }

    fn saved_networks(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .known_ssids
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn disconnect(&mut self) -> Result<(), String> {
        self.current_ssid = None;
        self.ip = None;
        Ok(())
    }

    fn forget(&mut self, ssid: &str) -> Result<(), String> {
        if let Some(reason) = &self.fail_forget {
            return Err(reason.clone());
        }
        self.known_ssids.retain(|s| s != ssid);
        if self.current_ssid.as_deref() == Some(ssid) {
            // Forgetting the active network leaves the radio with nothing to fall back to.
            self.current_ssid = None;
            self.ip = None;
        }
        Ok(())
    }
}

/// One selectable row on the Wi‑Fi root screen ([`WifiPhase::Current`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiRow {
    /// Always present, always first. Opens [`WifiPhase::Scan`].
    Search,
    /// Opens the saved-network list ([`WifiPhase::Known`]).
    Known,
    /// Only while associated. Drops the association, keeps the profile.
    Disconnect,
    /// Only while associated **and** a saved profile exists. Opens the confirm.
    Forget,
}

/// One row of the known-network action menu ([`WifiPhase::KnownActions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownAction {
    Connect,
    Forget,
    Back,
}

/// Form focus within the Wi‑Fi screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiPhase {
    /// The root menu: a status line and the action rows.
    Current,
    /// Scanned SSID list.
    Scan,
    /// Saved networks.
    Known,
    /// `Connect` / `Forget` / `Back` for one saved network.
    KnownActions,
    /// `No` / `Yes` before a saved profile is deleted.
    ConfirmForget,
    /// Password keyboard.
    Keyboard,
    /// Connect in progress (paint once; complete on tick).
    Connecting,
}

impl WifiPhase {
    /// True for the phases that are a top-down list (encoder inversion applies).
    pub fn is_list(self) -> bool {
        matches!(
            self,
            WifiPhase::Current
                | WifiPhase::Scan
                | WifiPhase::Known
                | WifiPhase::KnownActions
                | WifiPhase::ConfirmForget
        )
    }
}

/// Events the form consumes (mapped from menu encoder / enter / back / A / hold).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiFormEvent {
    Rotate(i8),
    Enter,
    Back,
    HoldDelete,
    RegionJump(i8),
    /// Complete a deferred connect (from UI tick while [`WifiPhase::Connecting`]).
    TickConnect,
}

/// Wi‑Fi UI state machine.
#[derive(Debug, Clone)]
pub struct WifiForm {
    pub phase: WifiPhase,
    pub ssids: Vec<String>,
    pub focus: usize,
    /// The SSID a connect/keyboard flow is about (also the headline before a connect).
    pub selected_ssid: Option<String>,
    pub keyboard: KeyboardField,
    pub message: Option<String>,
    /// Password captured for the pending connect.
    pending_password: String,
    connect_pending: bool,
    /// The radio's association as of the last [`WifiForm::observe`]. Drives the root rows.
    connected_ssid: Option<String>,
    /// True when the active SSID has a saved profile, also from the last `observe`.
    saved_active: bool,
    /// Saved profiles for the `Known` list (filled on entry, never polled from paint).
    known: Vec<String>,
    /// The SSID `KnownActions` / `ConfirmForget` are about.
    pending_ssid: Option<String>,
    /// The phase `ConfirmForget` returns to when it is not answered with `Yes`.
    forget_from: WifiPhase,
    /// The connect in flight was started from `KnownActions` (no password typed), so a
    /// failure returns there instead of to the scan list.
    connect_from_known: bool,
}

impl WifiForm {
    pub fn new(ssids: Vec<String>) -> Self {
        Self {
            phase: WifiPhase::Current,
            ssids,
            focus: 0,
            selected_ssid: None,
            keyboard: KeyboardField::new(),
            message: None,
            pending_password: String::new(),
            connect_pending: false,
            connected_ssid: None,
            saved_active: false,
            known: Vec::new(),
            pending_ssid: None,
            forget_from: WifiPhase::Current,
            connect_from_known: false,
        }
    }

    /// Refresh scan list from backend and open the scan phase.
    pub fn open_scan(&mut self, wifi: &mut dyn WifiBackend) {
        self.ssids = wifi.scan();
        self.focus = 0;
        self.message = None;
        self.phase = WifiPhase::Scan;
    }

    pub fn in_keyboard(&self) -> bool {
        self.phase == WifiPhase::Keyboard
    }

    /// Take the radio's state from the backend. Called when the screen opens, after every
    /// action, and from the app's Wi‑Fi sync — never from the paint path (it may shell out).
    pub fn observe(&mut self, wifi: &dyn WifiBackend) {
        self.connected_ssid = wifi.current_ssid();
        self.saved_active = self
            .connected_ssid
            .as_deref()
            .is_some_and(|ssid| wifi.has_saved_credentials(ssid));
        // The root row list can shrink underneath the cursor (a roam, or a disconnect from
        // a shell); paint and dispatch both need a focus that points at a real row.
        //
        // Only while the root menu owns the cursor: `focus` is shared with the scan and
        // known lists, and clamping it against these rows there would move their cursor
        // (a scan list of 5 with the radio disconnected would collapse 3 to 1).
        if self.phase == WifiPhase::Current {
            self.focus = self.focus.min(self.rows().len().saturating_sub(1));
        }
    }

    /// The association the last [`Self::observe`] saw.
    pub fn connected_ssid(&self) -> Option<&str> {
        self.connected_ssid.as_deref()
    }

    /// True when the active SSID is one NetworkManager has a profile for.
    pub fn can_forget(&self) -> bool {
        self.connected_ssid.is_some() && self.saved_active
    }

    /// Rows for the current radio state, in display order.
    pub fn rows(&self) -> Vec<WifiRow> {
        let mut rows = vec![WifiRow::Search, WifiRow::Known];
        if self.connected_ssid.is_some() {
            rows.push(WifiRow::Disconnect);
            if self.saved_active {
                rows.push(WifiRow::Forget);
            }
        }
        rows
    }

    /// The label of a root row. `Forget` names the network so the press is unambiguous.
    pub fn row_label(&self, row: WifiRow) -> String {
        match row {
            WifiRow::Search => "Search for networks".into(),
            WifiRow::Known => "Known networks".into(),
            WifiRow::Disconnect => "Disconnect".into(),
            WifiRow::Forget => match self.connected_ssid.as_deref() {
                Some(ssid) => format!("Forget {ssid}"),
                None => "Forget".into(),
            },
        }
    }

    /// The saved-network list, sorted. Filled by [`Self::refresh_known`].
    pub fn known(&self) -> &[String] {
        &self.known
    }

    /// The SSID `KnownActions` / `ConfirmForget` act on.
    pub fn pending_ssid(&self) -> Option<&str> {
        self.pending_ssid.as_deref()
    }

    /// Reload the saved-network list. Called on entry to `Known` (and after a forget), never
    /// from the paint path: `nmcli connection show` is ~100 ms on the device.
    pub fn refresh_known(&mut self, wifi: &mut dyn WifiBackend) {
        let mut names: Vec<String> = wifi
            .saved_networks()
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        names.sort();
        names.dedup();
        self.known = names;
    }

    /// Rows drawn in `KnownActions`, with whether each is selectable. `Connect` is drawn
    /// `Disabled` (not hidden) for the network that is already connected, mirroring how a
    /// zero-count category keeps its muted row.
    pub fn action_rows(&self) -> [(KnownAction, bool); 3] {
        let connected = self
            .pending_ssid
            .as_deref()
            .is_some_and(|ssid| self.connected_ssid.as_deref() == Some(ssid));
        [
            (KnownAction::Connect, !connected),
            (KnownAction::Forget, true),
            (KnownAction::Back, true),
        ]
    }

    /// The selectable actions, in order: what `focus` indexes in `KnownActions`.
    pub fn action_items(&self) -> Vec<KnownAction> {
        self.action_rows()
            .iter()
            .filter(|(_, enabled)| *enabled)
            .map(|(action, _)| *action)
            .collect()
    }

    /// Where `focus` sits in the drawn `KnownActions` rows (the enabled row it points at).
    pub fn action_drawn_index(&self) -> usize {
        let wanted = self.action_items().get(self.focus).copied();
        self.action_rows()
            .iter()
            .position(|(action, enabled)| *enabled && Some(*action) == wanted)
            .unwrap_or(0)
    }

    /// `No` (focused first) or `Yes` on the confirm screen.
    pub fn confirm_label(&self, index: usize) -> &'static str {
        if index == 0 {
            "No"
        } else {
            "Yes"
        }
    }

    pub fn handle(&mut self, ev: WifiFormEvent, wifi: &mut dyn WifiBackend) {
        match self.phase {
            WifiPhase::Current => self.handle_current(ev, wifi),
            WifiPhase::Scan => self.handle_scan(ev, wifi),
            WifiPhase::Known => self.handle_known(ev),
            WifiPhase::KnownActions => self.handle_known_actions(ev),
            WifiPhase::ConfirmForget => self.handle_confirm_forget(ev, wifi),
            WifiPhase::Keyboard => self.handle_keyboard(ev),
            WifiPhase::Connecting => {
                if ev == WifiFormEvent::TickConnect && self.connect_pending {
                    self.finish_connect(wifi);
                }
                // Back is ignored while connecting.
            }
        }
    }

    fn handle_current(&mut self, ev: WifiFormEvent, wifi: &mut dyn WifiBackend) {
        let rows = self.rows();
        match ev {
            WifiFormEvent::Rotate(d) => {
                self.focus = step(self.focus, d, rows.len());
            }
            WifiFormEvent::Enter => {
                // The rows are recomputed from the radio state; a stale focus cannot index one
                // that no longer exists.
                let Some(row) = rows.get(self.focus.min(rows.len().saturating_sub(1))).copied()
                else {
                    return;
                };
                match row {
                    WifiRow::Search => self.open_scan(wifi),
                    WifiRow::Known => {
                        self.refresh_known(wifi);
                        self.focus = 0;
                        self.message = None;
                        self.phase = WifiPhase::Known;
                    }
                    WifiRow::Disconnect => match wifi.disconnect() {
                        Ok(()) => {
                            self.message = None;
                            self.observe(wifi);
                            self.focus = self.focus.min(self.rows().len().saturating_sub(1));
                        }
                        Err(why) => self.message = Some(why),
                    },
                    WifiRow::Forget => {
                        self.pending_ssid = self.connected_ssid.clone();
                        self.forget_from = WifiPhase::Current;
                        self.focus = 0;
                        self.phase = WifiPhase::ConfirmForget;
                    }
                }
            }
            // Back falls through to the route and pops it.
            WifiFormEvent::Back => {}
            _ => {}
        }
    }

    fn handle_scan(&mut self, ev: WifiFormEvent, wifi: &mut dyn WifiBackend) {
        match ev {
            WifiFormEvent::Rotate(d) => {
                self.focus = step(self.focus, d, self.ssids.len());
            }
            WifiFormEvent::Enter => {
                if let Some(ssid) = self.ssids.get(self.focus).cloned() {
                    self.selected_ssid = Some(ssid.clone());
                    if wifi.has_saved_credentials(&ssid) {
                        self.pending_password.clear();
                        self.connect_from_known = false;
                        self.begin_connecting();
                    } else {
                        self.keyboard = KeyboardField::new();
                        self.phase = WifiPhase::Keyboard;
                    }
                }
            }
            WifiFormEvent::Back => {
                self.phase = WifiPhase::Current;
                self.message = None;
                self.focus = 0;
            }
            _ => {}
        }
    }

    fn handle_known(&mut self, ev: WifiFormEvent) {
        match ev {
            WifiFormEvent::Rotate(d) => {
                self.focus = step(self.focus, d, self.known.len());
            }
            WifiFormEvent::Enter => {
                if let Some(ssid) = self.known.get(self.focus).cloned() {
                    self.pending_ssid = Some(ssid);
                    self.focus = 0;
                    self.message = None;
                    self.phase = WifiPhase::KnownActions;
                }
            }
            WifiFormEvent::Back => {
                self.phase = WifiPhase::Current;
                self.focus = 0;
                self.message = None;
            }
            _ => {}
        }
    }

    fn handle_known_actions(&mut self, ev: WifiFormEvent) {
        let items = self.action_items();
        match ev {
            WifiFormEvent::Rotate(d) => {
                self.focus = step(self.focus, d, items.len());
            }
            WifiFormEvent::Enter => {
                match items.get(self.focus).copied() {
                    Some(KnownAction::Connect) => {
                        // NetworkManager has the password: no keyboard, straight to connecting.
                        self.selected_ssid = self.pending_ssid.clone();
                        self.pending_password.clear();
                        self.connect_from_known = true;
                        self.begin_connecting();
                    }
                    Some(KnownAction::Forget) => {
                        self.forget_from = WifiPhase::KnownActions;
                        self.focus = 0;
                        self.phase = WifiPhase::ConfirmForget;
                    }
                    Some(KnownAction::Back) => {
                        self.focus = 0;
                        self.phase = WifiPhase::Known;
                    }
                    None => {}
                }
            }
            WifiFormEvent::Back => {
                self.focus = 0;
                self.phase = WifiPhase::Known;
            }
            _ => {}
        }
    }

    fn handle_confirm_forget(&mut self, ev: WifiFormEvent, wifi: &mut dyn WifiBackend) {
        match ev {
            WifiFormEvent::Rotate(d) => {
                self.focus = step(self.focus, d, 2);
            }
            WifiFormEvent::Enter => {
                if self.focus == 0 {
                    // `No` is the default: nothing is deleted.
                    self.leave_confirm();
                    return;
                }
                let ssid = self.pending_ssid.clone().unwrap_or_default();
                match wifi.forget(&ssid) {
                    Ok(()) => {
                        self.message = None;
                        self.observe(wifi);
                        match self.forget_from {
                            // The list it came from no longer contains that network.
                            WifiPhase::KnownActions => {
                                self.refresh_known(wifi);
                                self.phase = WifiPhase::Known;
                                self.focus = 0;
                            }
                            other => {
                                self.phase = other;
                                self.focus = 0;
                            }
                        }
                    }
                    // The reason stays on the confirm screen; the profile is still there.
                    Err(why) => self.message = Some(why),
                }
            }
            WifiFormEvent::Back => self.leave_confirm(),
            _ => {}
        }
    }

    fn leave_confirm(&mut self) {
        self.focus = 0;
        self.phase = self.forget_from;
    }

    fn handle_keyboard(&mut self, ev: WifiFormEvent) {
        match ev {
            WifiFormEvent::Rotate(d) => {
                let _ = self.keyboard.handle(KeyboardEvent::Rotate(d));
            }
            WifiFormEvent::RegionJump(d) => {
                let _ = self.keyboard.handle(KeyboardEvent::RegionJump(d));
            }
            WifiFormEvent::HoldDelete => {
                let _ = self.keyboard.handle(KeyboardEvent::HoldDelete);
            }
            WifiFormEvent::Enter => {
                if let KeyboardAction::Submit(pw) = self.keyboard.handle(KeyboardEvent::Enter) {
                    self.pending_password = pw;
                    self.connect_from_known = false;
                    self.begin_connecting();
                }
            }
            WifiFormEvent::Back => {
                if self.keyboard.handle(KeyboardEvent::Back) == KeyboardAction::LeaveCancel {
                    self.phase = WifiPhase::Scan;
                    self.keyboard = KeyboardField::new();
                }
            }
            WifiFormEvent::TickConnect => {}
        }
    }

    fn begin_connecting(&mut self) {
        self.phase = WifiPhase::Connecting;
        self.connect_pending = true;
        self.message = Some("Connecting…".into());
    }

    fn finish_connect(&mut self, wifi: &mut dyn WifiBackend) {
        self.connect_pending = false;
        let ssid = self.selected_ssid.clone().unwrap_or_default();
        let from_known = self.connect_from_known;
        match wifi.connect(&ssid, &self.pending_password) {
            WifiConnectResult::Ok { .. } => {
                self.message = None;
                self.keyboard = KeyboardField::new();
                self.pending_password.clear();
                self.pending_ssid = None;
                self.connect_from_known = false;
                self.phase = WifiPhase::Current;
                self.focus = 0;
                self.observe(wifi);
            }
            WifiConnectResult::Failed { message } => {
                self.message = Some(message);
                if from_known {
                    // No password was typed, so there is nothing to correct: back to the
                    // action menu that started it, with the reason on screen.
                    self.pending_ssid = Some(ssid);
                    self.focus = 0;
                    self.phase = WifiPhase::KnownActions;
                } else if wifi.has_saved_credentials(&ssid) && self.pending_password.is_empty() {
                    // Known-credential path from the scan list.
                    self.phase = WifiPhase::Scan;
                } else {
                    self.phase = WifiPhase::Keyboard;
                }
            }
        }
    }
}

/// Move `focus` by `d` over `len` rows, wrapping. An empty list keeps focus at 0.
fn step(focus: usize, d: i8, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let len_i = len as isize;
    let mut idx = focus as isize + d as isize;
    while idx < 0 {
        idx += len_i;
    }
    (idx as usize) % len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn associated_wifi() -> MockWifi {
        MockWifi {
            known_ssids: vec!["HomeLAN".into()],
            current_ssid: Some("HomeLAN".into()),
            ip: Some("192.168.1.42".into()),
            ..MockWifi::default()
        }
    }

    fn form_for(wifi: &MockWifi) -> WifiForm {
        let mut f = WifiForm::new(wifi.networks.clone());
        f.observe(wifi);
        f
    }

    #[test]
    fn wifi_rows_follow_the_radio_state() {
        let offline = MockWifi::default();
        let f = form_for(&offline);
        assert_eq!(f.rows(), vec![WifiRow::Search, WifiRow::Known]);

        // Associated with a saved profile: the destructive rows appear.
        let saved = associated_wifi();
        let f = form_for(&saved);
        assert_eq!(
            f.rows(),
            vec![
                WifiRow::Search,
                WifiRow::Known,
                WifiRow::Disconnect,
                WifiRow::Forget
            ]
        );
        assert!(f.can_forget());
        assert_eq!(f.row_label(WifiRow::Forget), "Forget HomeLAN");

        // Associated but never saved (a captive-portal-only join): no Forget to offer.
        let open = MockWifi {
            current_ssid: Some("CafeGuest".into()),
            ip: Some("10.0.0.2".into()),
            known_ssids: Vec::new(),
            ..MockWifi::default()
        };
        let f = form_for(&open);
        assert_eq!(
            f.rows(),
            vec![WifiRow::Search, WifiRow::Known, WifiRow::Disconnect]
        );
        assert!(!f.can_forget());

        // The search row is always first, so one press scans.
        assert_eq!(form_for(&offline).rows()[0], WifiRow::Search);
    }

    #[test]
    fn entering_on_the_first_row_opens_the_scan() {
        let mut wifi = MockWifi::default();
        let mut form = form_for(&wifi);
        assert_eq!(form.phase, WifiPhase::Current);
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Scan);
        // Back returns to the menu, not to a scan affordance that no longer exists.
        form.handle(WifiFormEvent::Back, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Current);
    }

    #[test]
    fn forget_asks_before_dropping_the_profile() {
        let mut wifi = associated_wifi();
        let mut form = form_for(&wifi);
        form.focus = 3; // Forget
        assert_eq!(form.rows()[form.focus], WifiRow::Forget);
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::ConfirmForget);
        assert_eq!(form.focus, 0, "No is focused: the safe answer is the default");
        assert_eq!(wifi.saved_networks(), vec!["HomeLAN".to_string()]);

        form.handle(WifiFormEvent::Enter, &mut wifi); // No
        assert_eq!(form.phase, WifiPhase::Current);
        assert_eq!(wifi.saved_networks(), vec!["HomeLAN".to_string()]);
        assert_eq!(wifi.current_ssid().as_deref(), Some("HomeLAN"));
    }

    #[test]
    fn confirm_yes_removes_the_profile_and_leaves_the_device_disconnected() {
        let mut wifi = associated_wifi();
        let mut form = form_for(&wifi);
        form.focus = 3;
        form.handle(WifiFormEvent::Enter, &mut wifi); // confirm
        form.handle(WifiFormEvent::Rotate(1), &mut wifi); // Yes
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Current);
        assert_eq!(wifi.saved_networks(), Vec::<String>::new());
        assert_eq!(wifi.current_ssid(), None, "no half state: the association went with it");
        assert_eq!(wifi.current_ip(), None);
        // The rows shrank under the cursor; focus is still on a real row.
        assert!(form.focus < form.rows().len());
        assert_eq!(form.rows(), vec![WifiRow::Search, WifiRow::Known]);
    }

    #[test]
    fn a_failed_forget_says_why_and_stays() {
        let mut wifi = associated_wifi();
        wifi.fail_forget = Some("Wi-Fi settings are blocked by the system.".into());
        let mut form = form_for(&wifi);
        form.focus = 3;
        form.handle(WifiFormEvent::Enter, &mut wifi);
        form.handle(WifiFormEvent::Rotate(1), &mut wifi);
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::ConfirmForget);
        assert_eq!(
            form.message.as_deref(),
            Some("Wi-Fi settings are blocked by the system.")
        );
        assert_eq!(wifi.saved_networks(), vec!["HomeLAN".to_string()]);
        assert_eq!(wifi.current_ssid().as_deref(), Some("HomeLAN"));
    }

    #[test]
    fn disconnect_keeps_the_profile() {
        let mut wifi = associated_wifi();
        let mut form = form_for(&wifi);
        form.focus = 2; // Disconnect
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Current);
        assert_eq!(wifi.current_ssid(), None);
        assert_eq!(wifi.current_ip(), None);
        assert_eq!(
            wifi.saved_networks(),
            vec!["HomeLAN".to_string()],
            "the profile survives, so Known networks still lists it"
        );
        assert_eq!(form.connected_ssid(), None, "the status line follows the radio");
        assert_eq!(form.rows(), vec![WifiRow::Search, WifiRow::Known]);
    }

    #[test]
    fn known_network_connects_without_a_keyboard() {
        let mut wifi = MockWifi {
            known_ssids: vec!["HomeLAN".into()],
            ..MockWifi::default()
        };
        let mut form = form_for(&wifi);
        form.focus = 1; // Known networks
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Known);
        assert_eq!(form.known(), ["HomeLAN".to_string()]);

        form.handle(WifiFormEvent::Enter, &mut wifi); // pick it
        assert_eq!(form.phase, WifiPhase::KnownActions);
        assert_eq!(form.pending_ssid(), Some("HomeLAN"));
        // Nothing to connect to yet, so Connect is selectable and first.
        assert_eq!(form.action_items()[0], KnownAction::Connect);

        form.handle(WifiFormEvent::Enter, &mut wifi); // Connect
        assert_eq!(form.phase, WifiPhase::Connecting);
        form.handle(WifiFormEvent::TickConnect, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Current);
        assert_eq!(wifi.current_ssid().as_deref(), Some("HomeLAN"));
        assert!(form.message.is_none());
    }

    #[test]
    fn the_connected_network_cannot_be_connected_again() {
        let mut wifi = associated_wifi();
        let mut form = form_for(&wifi);
        form.pending_ssid = Some("HomeLAN".into());
        form.phase = WifiPhase::KnownActions;
        assert_eq!(form.action_rows()[0], (KnownAction::Connect, false));
        assert_eq!(
            form.action_items(),
            vec![KnownAction::Forget, KnownAction::Back],
            "Connect is drawn disabled, not hidden"
        );
        // Focus 0 now means Forget; the drawn index skips the disabled Connect.
        assert_eq!(form.action_drawn_index(), 1);
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::ConfirmForget, "focus 0 is Forget here");
    }

    #[test]
    fn forgetting_from_the_known_actions_returns_to_the_list() {
        let mut wifi = MockWifi {
            known_ssids: vec!["CafeGuest".into(), "HomeLAN".into()],
            ..MockWifi::default()
        };
        let mut form = form_for(&wifi);
        form.phase = WifiPhase::Known;
        form.refresh_known(&mut wifi);
        form.focus = 1; // HomeLAN (sorted second)
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::KnownActions);
        form.handle(WifiFormEvent::Enter, &mut wifi); // Connect (selectable: not associated)
        assert_eq!(form.phase, WifiPhase::Connecting);
        form.handle(WifiFormEvent::TickConnect, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Current);

        // Now it is the active one, so the action menu opens on Forget (Connect disabled).
        form.phase = WifiPhase::Known;
        form.refresh_known(&mut wifi);
        form.focus = 1;
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::KnownActions);
        assert_eq!(form.action_items(), vec![KnownAction::Forget, KnownAction::Back]);
        assert_eq!(form.action_drawn_index(), 1, "Connect is drawn above, disabled");
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::ConfirmForget);
        form.handle(WifiFormEvent::Rotate(1), &mut wifi); // Yes
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Known, "back to the list it came from");
        assert_eq!(form.known(), ["CafeGuest".to_string()], "the forgotten one is gone");
        assert_eq!(wifi.current_ssid(), None);
    }

    #[test]
    fn focus_is_clamped_when_the_row_list_shrinks() {
        let mut wifi = associated_wifi();
        let mut form = form_for(&wifi);
        form.focus = form.rows().len() - 1; // Forget
        assert_eq!(form.rows()[form.focus], WifiRow::Forget);

        // The radio drops behind the form's back (a roam failure, or `nmcli` from a shell).
        wifi.current_ssid = None;
        wifi.ip = None;
        form.observe(&wifi);
        assert_eq!(form.rows(), vec![WifiRow::Search, WifiRow::Known]);
        assert!(form.focus < form.rows().len(), "focus was clamped to a real row");

        // Enter must not index a row that no longer exists, and must not panic.
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Known, "the clamped row is Known networks");
        assert_eq!(form.focus, 0);
    }

    #[test]
    fn the_scan_cursor_is_not_clamped_against_the_root_rows() {
        let mut wifi = MockWifi {
            networks: ["HomeLAN", "CafeGuest", "Cafe Two", "Library", "Hotel"]
                .map(str::to_string)
                .to_vec(),
            ..MockWifi::default()
        };
        let mut form = form_for(&wifi);
        form.handle(WifiFormEvent::Enter, &mut wifi); // Search → Scan
        assert_eq!(form.phase, WifiPhase::Scan);
        form.focus = 3;
        let picked = form.ssids[3].clone();

        // The radio is disconnected, so the root menu has only two rows; the scan list
        // still has five. A sync must not drag the scan cursor up to row 1.
        wifi.current_ssid = None;
        wifi.ip = None;
        form.observe(&wifi);
        assert_eq!(form.rows().len(), 2);
        assert_eq!(form.focus, 3, "the cursor belongs to the scan list here");
        assert_eq!(form.ssids[form.focus], picked);

        // Back on the root menu the same sync must still clamp to a real row.
        form.phase = WifiPhase::Current;
        form.observe(&wifi);
        assert!(form.focus < form.rows().len());
    }

    #[test]
    fn wifi_scan_keyboard_connect_updates_current() {
        let mut wifi = MockWifi::default();
        let mut form = form_for(&wifi);
        form.handle(WifiFormEvent::Enter, &mut wifi); // Search
        assert_eq!(form.phase, WifiPhase::Scan);
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Keyboard);
        assert_eq!(form.selected_ssid.as_deref(), Some("HomeLAN"));

        // Type "secret" via keyboard: edit each char on blank, then OK.
        for ch in ['s', 'e', 'c', 'r', 'e', 't'] {
            assert_eq!(
                form.keyboard.handle(KeyboardEvent::Enter),
                KeyboardAction::None
            ); // edit blank
            let target = crate::keyboard::CHARSET
                .iter()
                .position(|&c| c == ch)
                .unwrap();
            let cur = form.keyboard.charset_index();
            let len = crate::keyboard::CHARSET.len();
            let delta = (target + len - cur) % len;
            for _ in 0..delta {
                form.keyboard.handle(KeyboardEvent::Rotate(1));
            }
            form.keyboard.handle(KeyboardEvent::Enter); // commit
        }
        form.keyboard.handle(KeyboardEvent::Rotate(1)); // blank → OK
        form.handle(WifiFormEvent::Enter, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Connecting);
        form.handle(WifiFormEvent::TickConnect, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Current);
        assert_eq!(wifi.current_ssid().as_deref(), Some("HomeLAN"));
        assert_eq!(wifi.current_ip().as_deref(), Some("192.168.1.42"));
    }

    #[test]
    fn known_ssid_skips_keyboard() {
        let mut wifi = MockWifi {
            known_ssids: vec!["HomeLAN".into()],
            ..MockWifi::default()
        };
        let mut form = form_for(&wifi);
        form.handle(WifiFormEvent::Enter, &mut wifi); // Current → Scan
        form.handle(WifiFormEvent::Enter, &mut wifi); // select HomeLAN
        assert_eq!(form.phase, WifiPhase::Connecting);
        form.handle(WifiFormEvent::TickConnect, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Current);
        assert_eq!(wifi.current_ssid().as_deref(), Some("HomeLAN"));
    }

    #[test]
    fn wifi_bad_password() {
        let mut wifi = MockWifi::default();
        let mut form = form_for(&wifi);
        form.handle(WifiFormEvent::Enter, &mut wifi);
        form.handle(WifiFormEvent::Enter, &mut wifi);
        // Submit empty via OK
        form.keyboard.handle(KeyboardEvent::Rotate(1));
        form.handle(WifiFormEvent::Enter, &mut wifi);
        form.handle(WifiFormEvent::TickConnect, &mut wifi);
        assert_eq!(form.phase, WifiPhase::Keyboard);
        assert!(form.message.as_ref().unwrap().contains("Incorrect"));
    }
}
