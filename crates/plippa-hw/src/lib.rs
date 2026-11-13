//! Hardware driver traits and backends for Plippa.
//!
//! MVP_06 delivers NFC: traits, mock, poll/debounce session helpers, associate
//! write-through, and an optional PN532 I2C backend (`feature = "pn532"`).

pub mod accessory;
pub mod nfc;

pub use accessory::{
    auto_connect_target, normalise_address, props_match_address, Accessory, AccessoryBackend,
    AccessoryError, AccessoryEvent, AccessoryId, AccessoryKind, AdapterInfo, AutoConnectBudget,
    canonical_address, MockAccessoryBackend, PairPrompt, Peer, PromptAnswer, ScanWindow,
    TransportAction, MAX_SCAN_SECONDS,
    MAX_TRIES, RETRY_SPACING,
};
pub use nfc::{
    associate_playable_with_tag, AssociateError, AssociateParams, AssociateResult, FakeClock,
    MockNfc, MockTag, NfcDevice, NfcError, NfcEvent, NfcSession, PollClock, PollScheduler,
    SteadyClock, TagInfo, TagType, MIN_POLL_HZ, MIN_POLL_INTERVAL, TAG_HAS_LINK_MSG,
    TAG_READ_ONLY_MSG,
};

#[cfg(feature = "pn532")]
pub use nfc::pn532::{Pn532Nfc, Pn532NfcConfig};

#[cfg(feature = "accessory-2g4")]
pub use accessory::TwoG4Backend;
