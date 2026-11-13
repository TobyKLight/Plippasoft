//! Accessory model, backend trait, scripted mock and pure connect policy.
//!
//! This module is the UI-facing surface for 2.4 GHz accessories: it lists adapters and known
//! accessories, drives discovery/pairing/connection, and answers the small policy questions
//! the UI needs. It is not an audio router, it does not persist settings, and it does not
//! decide when or whether to scan — those belong to the UI.
//!
//! One backend object lives per process. The UI worker thread owns it, sends commands and
//! drains [`AccessoryEvent`]s; the frame loop never calls the backend directly.

use thiserror::Error;

mod mock;
mod policy;
#[cfg(feature = "accessory-2g4")]
mod two_g4;

pub use mock::MockAccessoryBackend;
#[cfg(feature = "accessory-2g4")]
pub use two_g4::TwoG4Backend;
pub use policy::{
    auto_connect_target, props_match_address, AutoConnectBudget, ScanWindow, MAX_SCAN_SECONDS,
    MAX_TRIES, RETRY_SPACING,
};

/// Stable identity of an accessory: its radio address, normalised to lowercase hex with
/// `:` separators. Never the display name (two devices can share one).
pub type AccessoryId = String;

/// One radio adapter the system can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    pub id: String,
    pub address: String,
    pub name: String,
    pub powered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessoryKind {
    Audio,
    Input,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accessory {
    pub id: AccessoryId,
    /// Alias if present, else the reported name. Untrusted: the UI sanitises before drawing.
    pub name: String,
    pub kind: AccessoryKind,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    pub battery_pct: Option<u8>,
}

/// Who is asking, as far as the radio service knows (SYS-03).
///
/// The name is text a nearby device chose: it is untrusted, and the UI passes it through its
/// screen-text sanitiser before drawing it. The address is the identity — the name is only a
/// label, because two devices can advertise the same one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Peer {
    /// Advertised name, or `None` when the device never sent one (or sent its own address).
    pub name: Option<String>,
    /// Canonical address: uppercase hex with `:` separators (`AA:BB:CC:DD:EE:FF`).
    pub address: String,
}

impl Peer {
    pub fn new(name: Option<String>, address: &str) -> Self {
        let address = canonical_address(address);
        let name = name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty() && !same_address(name, &address));
        Self { name, address }
    }

    /// The one-line form the owner reads: `Name  AA:BB:CC:DD:EE:FF`, or just the address when
    /// the device never advertised a name. Never a bare number: the address is always shown.
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("{name}  {}", self.address),
            None => self.address.clone(),
        }
    }
}

/// Canonical display/identity form of an address: uppercase hex, `:` separated.
pub fn canonical_address(address: &str) -> String {
    normalise_address(address).to_ascii_uppercase()
}

/// True when `text` is the same address as `address`, in any of the separator/case forms a
/// device may report (some advertise their own address as their name).
fn same_address(text: &str, address: &str) -> bool {
    let strip = |s: &str| -> String {
        s.chars()
            .filter(|c| *c != ':' && *c != '-' && *c != ' ')
            .collect::<String>()
            .to_ascii_lowercase()
    };
    strip(text) == strip(address)
}

/// What the user has to answer while pairing. Every prompt names the peer (SYS-03): a number
/// on its own was enough for a distracted press to pair a hostile device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairPrompt {
    /// Show the number, ask Accept/Reject on the device.
    Confirm { peer: Peer, passkey: u32 },
    /// The accessory shows a number; the user types it here (numeric keypad).
    EnterPasskey { peer: Peer },
    /// Legacy PIN entry (alphanumeric allowed).
    EnterPin { peer: Peer },
    /// No number: authorise this accessory.
    Authorise { peer: Peer },
}

impl PairPrompt {
    /// The device this prompt is about.
    pub fn peer(&self) -> &Peer {
        match self {
            PairPrompt::Confirm { peer, .. }
            | PairPrompt::EnterPasskey { peer }
            | PairPrompt::EnterPin { peer }
            | PairPrompt::Authorise { peer } => peer,
        }
    }

    /// The passkey the accessory displays, when the prompt carries one.
    pub fn passkey(&self) -> Option<u32> {
        match self {
            PairPrompt::Confirm { passkey, .. } => Some(*passkey),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptAnswer {
    Accept,
    Reject,
    Passkey(u32),
    Pin(String),
}

/// A transport button pressed on a connected accessory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportAction {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    FastForward,
    Rewind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessoryEvent {
    Adapters(Vec<AdapterInfo>),
    Discovered { id: AccessoryId, name: String, kind: AccessoryKind },
    Prompt(PairPrompt),
    Paired { id: AccessoryId, ok: bool, error: Option<String> },
    Connected(AccessoryId),
    Disconnected(AccessoryId),
    Transport(TransportAction),
    ScanFinished,
    Failed(String),
}

#[derive(Debug, Error)]
pub enum AccessoryError {
    #[error("no radio adapter")]
    NoAdapter,
    #[error("radio adapter is not ready ({0})")]
    AdapterNotReady(String),
    #[error("radio is blocked")]
    Blocked,
    #[error("unknown accessory {0}")]
    Unknown(String),
    #[error("not pairing")]
    NoPairing,
    #[error("the radio service refused the request: {0}")]
    Refused(String),
    #[error("accessory backend unavailable")]
    Unavailable,
}

/// UI-facing accessory backend.
///
/// Every method must return within 500 ms: the UI worker thread also feeds the screen and
/// cannot afford a blocking radio call. Long operations (pairing, connecting) complete
/// asynchronously and are reported through [`Self::poll`].
///
/// Methods are idempotent where they can be: powering on a powered adapter, connecting a
/// connected accessory and stopping when no scan is running all succeed without side effects.
pub trait AccessoryBackend: Send {
    /// Adapters present right now. Cheap (cached by the backend); never blocks on discovery.
    fn adapters(&mut self) -> Result<Vec<AdapterInfo>, AccessoryError>;

    /// Paired/known accessories and their current state.
    fn known(&mut self) -> Result<Vec<Accessory>, AccessoryError>;

    /// Power the adapter on (needed once per boot; the app does it, not an operator script).
    fn power_on(&mut self, adapter: &str) -> Result<(), AccessoryError>;

    /// Start discovery for at most `seconds` (clamped by the backend to 60).
    fn start_scan(&mut self, seconds: u32) -> Result<(), AccessoryError>;

    fn stop_scan(&mut self) -> Result<(), AccessoryError>;

    /// Begin pairing `id` (must have been discovered in this scan). Completion and prompts
    /// arrive through `poll`.
    fn pair(&mut self, id: &AccessoryId) -> Result<(), AccessoryError>;

    /// Answer the outstanding [`PairPrompt`]. [`AccessoryError::NoPairing`] when nothing is
    /// outstanding.
    fn answer(&mut self, answer: PromptAnswer) -> Result<(), AccessoryError>;

    fn connect(&mut self, id: &AccessoryId) -> Result<(), AccessoryError>;

    fn disconnect(&mut self, id: &AccessoryId) -> Result<(), AccessoryError>;

    fn set_trusted(&mut self, id: &AccessoryId, trusted: bool) -> Result<(), AccessoryError>;

    /// Remove the pairing (and the device from the adapter's list).
    fn forget(&mut self, id: &AccessoryId) -> Result<(), AccessoryError>;

    /// Non-blocking drain of everything that happened since the last call.
    fn poll(&mut self) -> Vec<AccessoryEvent>;
}

/// Normalise a radio address to lowercase hex with `:` separators.
///
/// Accepts `:`-separated, `-`-separated or unseparated 48-bit addresses. Anything that is
/// not exactly six bytes of hex is rejected by returning the lowercased input untouched.
pub fn normalise_address(s: &str) -> AccessoryId {
    let stripped: String = s.chars().filter(|c| *c != ':' && *c != '-').collect();
    if stripped.len() == 12 && stripped.chars().all(|c| c.is_ascii_hexdigit()) {
        let lower = stripped.to_ascii_lowercase();
        let mut out = String::with_capacity(17);
        for (i, ch) in lower.chars().enumerate() {
            if i > 0 && i % 2 == 0 {
                out.push(':');
            }
            out.push(ch);
        }
        out
    } else {
        s.to_lowercase()
    }
}
