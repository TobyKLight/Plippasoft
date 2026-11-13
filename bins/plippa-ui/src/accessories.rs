//! System > Audio > Accessories (ACC_03): the persisted accessory choices, the screen state
//! and the small amount of policy the UI keeps between the radio and the audio route.
//!
//! File (`PLIPPA_STATE_DIR/accessories.json`, else `XDG_STATE_HOME/plippa/`, default
//! `~/.local/state/plippa/`), written atomically and **independent of `audio_outputs.json`**:
//!
//! ```json
//! {"auto_connect":["aa:bb:cc:dd:ee:ff"],"scan_seconds":30,"transport_disabled":[]}
//! ```
//!
//! Only the owner's choices are persisted. The known accessories, the scan result and the
//! connected state are always mirrored from the radio. Loading is lenient: a missing,
//! unreadable or garbage file is the defaults, an unknown key is ignored, an unknown id is
//! kept (the accessory may just be out of range) and every id is normalised.
//!
//! The backend lives on [`crate::accessory_worker`]'s thread. The frame loop only sends
//! commands and drains messages; nothing here calls a backend.

use crate::assign::state_file_path;
use crate::keyboard::KeyboardField;
use crate::screens::screen_text;
use plippa_hw::{
    normalise_address, Accessory, AccessoryError, AccessoryId, AccessoryKind, AdapterInfo,
    AutoConnectBudget, PairPrompt, ScanWindow, MAX_SCAN_SECONDS,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// File name inside the state directory.
pub const ACCESSORY_FILE_NAME: &str = "accessories.json";

/// Shortest and longest scan the owner can store.
pub const MIN_SCAN_SECONDS: u32 = 10;

/// The persisted accessory choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessorySettings {
    /// Ids the owner marked auto-connect (normalised).
    pub auto_connect: BTreeSet<AccessoryId>,
    /// Scan window in seconds (`10..=60`, default 30).
    pub scan_seconds: u32,
    /// Accessories whose transport buttons are ignored. Absence means enabled, so the
    /// default is on for every accessory, including one paired later.
    pub transport_disabled: BTreeSet<AccessoryId>,
}

impl Default for AccessorySettings {
    fn default() -> Self {
        Self {
            auto_connect: BTreeSet::new(),
            scan_seconds: 30,
            transport_disabled: BTreeSet::new(),
        }
    }
}

impl AccessorySettings {
    /// File text. Every key is written, so a reload sees the same choices.
    pub fn to_json(&self) -> String {
        let mut m = serde_json::Map::new();
        m.insert(
            "auto_connect".into(),
            self.auto_connect.iter().cloned().collect::<Vec<_>>().into(),
        );
        m.insert("scan_seconds".into(), self.scan_seconds.into());
        m.insert(
            "transport_disabled".into(),
            self.transport_disabled
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .into(),
        );
        serde_json::to_string_pretty(&Value::Object(m)).unwrap_or_else(|_| "{}".into())
    }

    /// Parse leniently: each key is judged on its own, unknown keys are ignored, a bad value
    /// follows the default for that key only.
    pub fn from_json(text: &str) -> Self {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return Self::default();
        };
        let d = Self::default();
        let mut auto_connect = BTreeSet::new();
        if let Some(ids) = v.get("auto_connect").and_then(Value::as_array) {
            for id in ids.iter().filter_map(Value::as_str) {
                let id = normalise_address(id.trim());
                if !id.is_empty() {
                    auto_connect.insert(id);
                }
            }
        }
        let scan_seconds = v
            .get("scan_seconds")
            .and_then(Value::as_u64)
            .and_then(|s| u32::try_from(s).ok())
            .map(|s| s.clamp(MIN_SCAN_SECONDS, MAX_SCAN_SECONDS))
            .unwrap_or(d.scan_seconds);
        let mut transport_disabled = BTreeSet::new();
        if let Some(ids) = v.get("transport_disabled").and_then(Value::as_array) {
            for id in ids.iter().filter_map(Value::as_str) {
                let id = normalise_address(id.trim());
                if !id.is_empty() {
                    transport_disabled.insert(id);
                }
            }
        }
        Self {
            auto_connect,
            scan_seconds,
            transport_disabled,
        }
    }
}

/// Settings + optional persist path (same shape as `AudioStore`).
#[derive(Debug, Clone, Default)]
pub struct AccessoryStore {
    path: Option<PathBuf>,
    pub settings: AccessorySettings,
}

impl AccessoryStore {
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
            settings: AccessorySettings::default(),
        }
    }

    /// `PLIPPA_STATE_DIR/accessories.json`, else the XDG state dir.
    pub fn default_path() -> PathBuf {
        state_file_path(ACCESSORY_FILE_NAME)
    }

    /// Load `path`; a missing or unreadable file is "nothing chosen".
    pub fn load_or_default(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let settings = match fs::read_to_string(path) {
            Ok(text) => AccessorySettings::from_json(&text),
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    eprintln!(
                        "plippa-ui: accessories load `{}`: {e} - defaults",
                        path.display()
                    );
                }
                AccessorySettings::default()
            }
        };
        Self {
            path: Some(path.to_path_buf()),
            settings,
        }
    }

    pub fn load_default() -> Self {
        Self::load_or_default(Self::default_path())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Atomic write (temp + rename). No-op without a path. A failure here never touches the
    /// audio settings file.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, self.settings.to_json())?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// One device found in the current/last scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundItem {
    pub id: AccessoryId,
    pub name: String,
    pub kind: AccessoryKind,
}

/// Where the Pair new flow is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PairStage {
    #[default]
    Idle,
    Scanning,
    Pairing(AccessoryId),
    Prompt(PairPrompt, Option<u32>),
}

/// Everything the Accessories screens draw plus the session bookkeeping.
#[derive(Debug, Clone)]
pub struct AccessoriesState {
    /// A worker handle was injected (feature on and a device build, or a test handle).
    pub available: bool,
    /// The backend answered a snapshot (a board without a radio answers unavailable).
    pub radio_present: bool,
    pub adapters: Vec<AdapterInfo>,
    pub known: Vec<Accessory>,
    pub settings: AccessorySettings,
    pub stage: PairStage,
    /// Found devices stay on screen after `ScanFinished` (work package 4.3).
    pub found: Vec<FoundItem>,
    pub scan_window: Option<ScanWindow>,
    pub pending_output: Option<(AccessoryId, u64)>,
    pub budget: AutoConnectBudget,
    /// The id of the detail screen currently open.
    pub detail: Option<AccessoryId>,
    /// The id whose pairing prompt is outstanding (`Prompt` has no id of its own).
    pub pairing_id: Option<AccessoryId>,
    pub keyboard: Option<KeyboardField>,
    pub forget_confirm: Option<AccessoryId>,
    /// Deadline for the Forget confirmation, matching the status popup that carries it.
    pub forget_confirm_until_ms: Option<u64>,
    /// A connect/disconnect command is in flight for this id.
    pub pending_action: Option<AccessoryId>,
    /// The in-flight action is a Connect, so a `Failed` means this id is not reachable.
    /// A Disconnect failure must not mark the device unavailable.
    pub pending_connect: Option<AccessoryId>,
    /// Session-only ids whose last connect failed (or that were not reachable). Cleared by
    /// a successful connect or pair; never persisted and never used to hide a row.
    pub unavailable: BTreeSet<AccessoryId>,
    last_failure: Option<(String, u64)>,
    power_requested: bool,
    first_route_done: bool,
    pairing_until_ms: Option<u64>,
    scan_secs_shown: u32,
    /// Loop clock the screens use for the countdown / confirmation timers.
    pub now_ms: u64,
}

impl Default for AccessoriesState {
    fn default() -> Self {
        Self {
            available: false,
            radio_present: true,
            adapters: Vec::new(),
            known: Vec::new(),
            settings: AccessorySettings::default(),
            stage: PairStage::Idle,
            found: Vec::new(),
            scan_window: None,
            pending_output: None,
            budget: AutoConnectBudget::new(),
            detail: None,
            pairing_id: None,
            keyboard: None,
            forget_confirm: None,
            forget_confirm_until_ms: None,
            pending_action: None,
            pending_connect: None,
            unavailable: BTreeSet::new(),
            last_failure: None,
            power_requested: false,
            first_route_done: false,
            pairing_until_ms: None,
            scan_secs_shown: 0,
            now_ms: 0,
        }
    }
}

impl AccessoriesState {
    /// Snapshots the frames need, constructed once at startup.
    pub fn new(available: bool, settings: AccessorySettings) -> Self {
        Self {
            available,
            settings,
            ..Self::default()
        }
    }

    pub fn has_radio(&self) -> bool {
        self.available && self.radio_present
    }

    /// The Audio row value: nothing paired, one connected, else the paired count.

    /// Known accessories, connected first then by display name (then id for a stable order).
    pub fn sorted_known(&self) -> Vec<&Accessory> {
        let mut out: Vec<&Accessory> = self.known.iter().collect();
        out.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then_with(|| {
                    a.name
                        .to_lowercase()
                        .cmp(&b.name.to_lowercase())
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    pub fn accessory(&self, id: &str) -> Option<&Accessory> {
        let id = normalise_address(id);
        self.known.iter().find(|a| a.id == id)
    }

    pub fn found_name(&self, id: &str) -> Option<&str> {
        let id = normalise_address(id);
        self.found.iter().find(|f| f.id == id).map(|f| f.name.as_str())
    }

    /// The best available display name for an id (known, then found, then the id itself).
    pub fn name_for(&self, id: &str) -> String {
        if let Some(a) = self.accessory(id) {
            return a.name.clone();
        }
        if let Some(name) = self.found_name(id) {
            return name.to_string();
        }
        id.to_string()
    }

    pub fn is_auto_connect(&self, id: &str) -> bool {
        self.settings.auto_connect.contains(&normalise_address(id))
    }

    pub fn is_unavailable(&self, id: &str) -> bool {
        self.unavailable.contains(&normalise_address(id))
    }

    pub fn mark_unavailable(&mut self, id: &str) {
        self.unavailable.insert(normalise_address(id));
    }

    pub fn clear_unavailable(&mut self, id: &str) {
        self.unavailable.remove(&normalise_address(id));
    }

    /// Status shown for one accessory: `connected`, `not in range` (a connect failed this
    /// session) or `not connected`.
    pub fn status_text(&self, accessory: &Accessory) -> &'static str {
        if accessory.connected {
            "connected"
        } else if self.is_unavailable(&accessory.id) {
            "not in range"
        } else {
            "not connected"
        }
    }

    pub fn set_auto_connect(&mut self, id: &str, on: bool) {
        let id = normalise_address(id);
        if on {
            self.settings.auto_connect.insert(id);
        } else {
            self.settings.auto_connect.remove(&id);
        }
    }

    /// Per-accessory transport button policy; absence means enabled.
    pub fn is_transport_enabled(&self, id: &str) -> bool {
        !self
            .settings
            .transport_disabled
            .contains(&normalise_address(id))
    }

    pub fn set_transport_enabled(&mut self, id: &str, on: bool) {
        let id = normalise_address(id);
        if on {
            self.settings.transport_disabled.remove(&id);
        } else {
            self.settings.transport_disabled.insert(id);
        }
    }

    /// Seconds left in the open scan window (rounded up), 0 when closed.
    pub fn scan_seconds_left(&self) -> u32 {
        let Some(window) = self.scan_window else {
            return 0;
        };
        let rem = window.remaining(Duration::from_millis(self.now_ms));
        rem.as_millis().div_ceil(1000) as u32
    }

    pub fn is_scanning(&self) -> bool {
        matches!(self.stage, PairStage::Scanning)
    }

    pub fn is_busy_pairing(&self) -> bool {
        self.pairing_id.is_some()
            || matches!(self.stage, PairStage::Pairing(_) | PairStage::Prompt(..))
    }

    /// True when the auto-connect policy may run right now.
    pub fn auto_connect_allowed(&self) -> bool {
        matches!(self.stage, PairStage::Idle)
            && self.scan_window.is_none()
            && !self.is_busy_pairing()
    }

    /// Arm the Forget confirmation with the same lifetime as its status popup.
    pub fn arm_forget_confirm(&mut self, id: &str, now_ms: u64, ttl_ms: u64) {
        self.forget_confirm = Some(normalise_address(id));
        self.forget_confirm_until_ms = Some(now_ms.saturating_add(ttl_ms));
    }

    pub fn forget_confirm_active(&self, now_ms: u64) -> bool {
        self.forget_confirm.is_some()
            && self
                .forget_confirm_until_ms
                .is_some_and(|deadline| now_ms < deadline)
    }

    pub fn clear_forget_confirm(&mut self) {
        self.forget_confirm = None;
        self.forget_confirm_until_ms = None;
    }

    /// Clear an expired confirmation; returns true when something was cleared.
    pub fn expire_forget_confirm(&mut self, now_ms: u64) -> bool {
        if self.forget_confirm.is_some() && !self.forget_confirm_active(now_ms) {
            self.clear_forget_confirm();
            true
        } else {
            false
        }
    }

    pub fn take_power_request(&mut self) -> Option<String> {
        if self.power_requested || self.adapters.iter().any(|a| a.powered) {
            return None;
        }
        let first = self.adapters.first()?.id.clone();
        self.power_requested = true;
        Some(first)
    }

    pub fn mark_first_route(&mut self) -> bool {
        if self.first_route_done {
            false
        } else {
            self.first_route_done = true;
            true
        }
    }

    pub fn pairing_deadline_open(&self) -> bool {
        self.pairing_until_ms.is_none()
    }

    pub fn set_pairing_deadline(&mut self, now_ms: u64) {
        self.pairing_until_ms = Some(now_ms.saturating_add(60_000));
    }

    pub fn pairing_expired(&self, now_ms: u64) -> bool {
        self.pairing_until_ms.is_some_and(|at| now_ms >= at)
            && (self.pairing_id.is_some()
                || matches!(self.stage, PairStage::Pairing(_) | PairStage::Prompt(..)))
    }

    pub fn clear_pairing(&mut self) {
        self.pairing_until_ms = None;
        self.pairing_id = None;
        self.keyboard = None;
    }

    pub fn should_show_failure(&mut self, text: &str, now_ms: u64) -> bool {
        if let Some((last, at)) = &self.last_failure {
            if last == text && now_ms.saturating_sub(*at) < 30_000 {
                return false;
            }
        }
        self.last_failure = Some((text.to_string(), now_ms));
        true
    }

    pub fn note_scan_shown(&mut self) -> bool {
        let secs = if self.is_scanning() {
            self.scan_seconds_left()
        } else {
            0
        };
        if secs == self.scan_secs_shown {
            false
        } else {
            self.scan_secs_shown = secs;
            true
        }
    }
}

/// A human sentence for an API error. Never the raw error/message string.
pub fn accessory_error_text(error: &AccessoryError) -> &'static str {
    match error {
        AccessoryError::NoAdapter => "No radio adapter was found",
        AccessoryError::AdapterNotReady(_) => "The radio is not ready",
        AccessoryError::Blocked => "The radio is blocked",
        AccessoryError::Unknown(_) => "That accessory is no longer known",
        AccessoryError::NoPairing => "No pairing is in progress",
        AccessoryError::Refused(_) => "The radio refused the request",
        AccessoryError::Unavailable => "The accessory radio is unavailable",
    }
}

/// A human sentence for a pairing outcome. The backend reports short outcome tags
/// (`"rejected"`, `"cancelled"`, …); those tags are never shown as they are.
pub fn pair_failure_text(error: Option<&str>) -> String {
    match error {
        Some("rejected") => "Pairing was rejected".into(),
        Some("cancelled") => "Pairing was cancelled".into(),
        Some("passkey did not match") => "The passkey did not match".into(),
        Some("answer did not match the request") => "The accessory did not accept the answer".into(),
        Some("no radio adapter") => "No radio adapter was found".into(),
        Some("radio is blocked") => "The radio is blocked".into(),
        Some("accessory service unavailable") => "The accessory radio is unavailable".into(),
        _ => "Could not pair the accessory".into(),
    }
}

/// True for a name that is really the radio address (`58-8A-D2-D8-22-7F`,
/// `58:8a:d2:d8:22:7f`): such a device never advertised a name, so the found list shows a
/// neutral label instead of a hex string.
pub fn address_like_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.len() < 11 {
        return false;
    }
    let mut hex_digits = 0;
    for ch in trimmed.chars() {
        if ch.is_ascii_hexdigit() {
            hex_digits += 1;
        } else if ch != ':' && ch != '-' && ch != ' ' {
            return false;
        }
    }
    hex_digits >= 8
}

/// Kind label used by the Pair new found list.
pub fn kind_text(kind: AccessoryKind) -> &'static str {
    match kind {
        AccessoryKind::Audio => "Audio",
        AccessoryKind::Input => "Input",
        AccessoryKind::Other => "Other",
    }
}

/// The keyboard a prompt needs: digits for a passkey, the full charset for a legacy PIN.
pub fn prompt_keyboard(prompt: &PairPrompt) -> Option<KeyboardField> {
    match prompt {
        PairPrompt::EnterPasskey { .. } => Some(KeyboardField::digits_only()),
        PairPrompt::EnterPin { .. } => Some(KeyboardField::new()),
        PairPrompt::Confirm { .. } | PairPrompt::Authorise { .. } => None,
    }
}

/// The lines that name the peer on a pairing prompt (SYS-03): the advertised name when there is
/// one, then the canonical address. Hostile text from a nearby device never reaches the screen
/// raw — the name goes through [`screen_text`] — and a prompt with no name still shows the
/// address, never just a number.
pub fn prompt_peer_lines(prompt: &PairPrompt) -> Vec<String> {
    let peer = prompt.peer();
    let mut lines = Vec::new();
    if let Some(name) = &peer.name {
        lines.push(screen_text(name));
    }
    lines.push(peer.address.clone());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_hw::AccessoryKind;
    use tempfile::tempdir;

    fn accessory(id: &str, name: &str, connected: bool) -> Accessory {
        Accessory {
            id: normalise_address(id),
            name: name.into(),
            kind: AccessoryKind::Audio,
            paired: true,
            trusted: true,
            connected,
            battery_pct: None,
        }
    }

    #[test]
    fn power_on_is_sent_once_for_the_first_adapter_only() {
        let mut state = AccessoriesState::default();
        state.adapters = vec![
            AdapterInfo {
                id: "hci0".into(),
                address: "11:22:33:44:55:66".into(),
                name: "radio".into(),
                powered: false,
            },
            AdapterInfo {
                id: "hci1".into(),
                address: "11:22:33:44:55:67".into(),
                name: "radio2".into(),
                powered: false,
            },
        ];
        assert_eq!(state.take_power_request().as_deref(), Some("hci0"));
        assert_eq!(state.take_power_request(), None, "once per boot");

        let mut powered = AccessoriesState::default();
        powered.adapters = vec![
            AdapterInfo {
                id: "hci0".into(),
                address: String::new(),
                name: String::new(),
                powered: true,
            },
            AdapterInfo {
                id: "hci1".into(),
                address: String::new(),
                name: String::new(),
                powered: false,
            },
        ];
        assert_eq!(
            powered.take_power_request(),
            None,
            "at least one powered adapter is enough"
        );
    }

    #[test]
    fn settings_round_trip_and_ids_are_normalised() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(ACCESSORY_FILE_NAME);
        let mut store = AccessoryStore::with_path(&path);
        store.settings.scan_seconds = 45;
        store
            .settings
            .auto_connect
            .insert("AA:BB:CC:DD:EE:FF".into());
        store.save().unwrap();
        assert!(!path.with_extension("json.tmp").exists(), "temp renamed");
        let loaded = AccessoryStore::load_or_default(&path);
        assert_eq!(loaded.settings.scan_seconds, 45);
        assert_eq!(
            loaded.settings.auto_connect.iter().next().unwrap(),
            "aa:bb:cc:dd:ee:ff"
        );
    }

    #[test]
    fn lenient_load_keeps_valid_keys_and_clamps_the_scan() {
        let s = AccessorySettings::from_json(
            r#"{"auto_connect":["AA-BB-CC-DD-EE-FF","zz"],"scan_seconds":999,
                "future":{"x":1}}"#,
        );
        assert!(s.auto_connect.contains("aa:bb:cc:dd:ee:ff"));
        assert!(s.auto_connect.contains("zz"), "unknown id is kept");
        assert_eq!(s.scan_seconds, MAX_SCAN_SECONDS);
        let low = AccessorySettings::from_json(r#"{"scan_seconds":1}"#);
        assert_eq!(low.scan_seconds, MIN_SCAN_SECONDS);
        let bad = AccessorySettings::from_json(r#"{"scan_seconds":"long"}"#);
        assert_eq!(bad.scan_seconds, 30);
        assert!(
            s.transport_disabled.is_empty(),
            "transport is on by default for every accessory"
        );
        assert!(
            AccessorySettings::from_json(r#"{"transport_disabled":"no"}"#)
                .transport_disabled
                .is_empty(),
            "a bad value follows the default for that key only"
        );
        assert_eq!(
            AccessorySettings::from_json(r#"{"transport_disabled":["AA-BB-CC-DD-EE-FF"]}"#)
                .transport_disabled,
            std::collections::BTreeSet::from(["aa:bb:cc:dd:ee:ff".to_string()])
        );
        assert_eq!(AccessorySettings::from_json("not json"), AccessorySettings::default());
    }


    #[test]
    fn scan_seconds_left_is_rounded_up_and_closes_at_zero() {
        let mut state = AccessoriesState::new(true, AccessorySettings::default());
        state.stage = PairStage::Scanning;
        state.scan_window = Some(ScanWindow::start(Duration::ZERO, 30));
        state.now_ms = 0;
        assert_eq!(state.scan_seconds_left(), 30);
        state.now_ms = 29_500;
        assert_eq!(state.scan_seconds_left(), 1);
        state.now_ms = 30_000;
        assert_eq!(state.scan_seconds_left(), 0);
    }

    #[test]
    fn failure_dedupe_holds_for_thirty_seconds() {
        let mut state = AccessoriesState::default();
        assert!(state.should_show_failure("Radio offline", 1_000));
        assert!(!state.should_show_failure("Radio offline", 20_000));
        assert!(state.should_show_failure("Radio offline", 31_000));
        assert!(state.should_show_failure("Different", 31_001));
    }

    #[test]
    fn pair_failure_tags_become_sentences_and_never_leak_raw_text() {
        assert_eq!(pair_failure_text(Some("rejected")), "Pairing was rejected");
        assert_eq!(pair_failure_text(Some("cancelled")), "Pairing was cancelled");
        let odd = pair_failure_text(Some("org.example: secret"));
        assert!(!odd.contains("secret"));
        assert!(!odd.contains("org.example"));
    }

    fn peer_prompt(name: Option<&str>, address: &str) -> PairPrompt {
        PairPrompt::Authorise {
            peer: plippa_hw::Peer {
                name: name.map(str::to_string),
                address: address.to_string(),
            },
        }
    }

    #[test]
    fn prompt_peer_lines_lead_with_the_name_then_the_canonical_address() {
        let lines = prompt_peer_lines(&peer_prompt(Some("Headphones"), "58:8A:D2:D8:22:7F"));
        assert_eq!(
            lines,
            vec!["Headphones".to_string(), "58:8A:D2:D8:22:7F".to_string()]
        );
    }

    #[test]
    fn a_nameless_prompt_still_shows_the_address_never_a_bare_number() {
        // SYS-03: "Authorise ?" let a distracted press pair a hostile device.
        let lines = prompt_peer_lines(&peer_prompt(None, "58:8A:D2:D8:22:7F"));
        assert_eq!(lines, vec!["58:8A:D2:D8:22:7F".to_string()]);
    }

    #[test]
    fn every_pairing_prompt_shows_the_peer_name_and_address() {
        let peer = plippa_hw::Peer {
            name: Some("Headphones".into()),
            address: "58:8A:D2:D8:22:7F".into(),
        };
        for prompt in [
            PairPrompt::Confirm {
                peer: peer.clone(),
                passkey: 123_456,
            },
            PairPrompt::EnterPasskey { peer: peer.clone() },
            PairPrompt::EnterPin { peer: peer.clone() },
            PairPrompt::Authorise { peer },
        ] {
            let lines = prompt_peer_lines(&prompt);
            assert_eq!(lines.first().map(String::as_str), Some("Headphones"), "{prompt:?}");
            assert_eq!(
                lines.last().map(String::as_str),
                Some("58:8A:D2:D8:22:7F"),
                "{prompt:?}"
            );
        }
    }

    #[test]
    fn a_hostile_advertised_name_is_sanitised_on_the_prompt() {
        let hostile = "Ev\u{202E}il\u{200B}\r\n\u{001B}[2Jx";
        let lines = prompt_peer_lines(&peer_prompt(Some(hostile), "58:8A:D2:D8:22:7F"));
        assert!(!lines[0].chars().any(|c| c.is_control()));
        assert!(!lines[0].contains('\u{202E}'));
        assert_eq!(lines[1], "58:8A:D2:D8:22:7F");
    }

    #[test]
    fn every_error_has_a_human_sentence() {
        for error in [
            AccessoryError::NoAdapter,
            AccessoryError::AdapterNotReady("raw detail".into()),
            AccessoryError::Blocked,
            AccessoryError::Unknown("raw id".into()),
            AccessoryError::NoPairing,
            AccessoryError::Refused("raw detail".into()),
            AccessoryError::Unavailable,
        ] {
            let text = accessory_error_text(&error);
            assert!(!text.contains("raw"), "{text}");
            assert!(!text.is_empty());
        }
    }
}
