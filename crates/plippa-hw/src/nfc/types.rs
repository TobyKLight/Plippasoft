//! Shared NFC types.

use thiserror::Error;

/// NFC Forum tag family we support for NDEF (§1.6 / §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagType {
    Type2,
    Type4,
}

/// Snapshot of a tag currently in the field (after detection / full read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagInfo {
    pub uid: Vec<u8>,
    pub tag_type: TagType,
    /// Usable NDEF storage bytes when known (Type 2 CC MLEN area, or Type 4 NDEF file size).
    pub ndef_capacity: Option<usize>,
}

/// Events emitted by [`super::session::NfcSession`] after poll semantics are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NfcEvent {
    /// Full NDEF payload is available — clear tap feedback is allowed (§3.2).
    ReadComplete {
        info: TagInfo,
        /// Raw bytes suitable for [`plippa_noubin::extract_noubin_key`] (message or Type 2 TLV).
        ndef: Vec<u8>,
    },
    /// Tag left the field (optional; useful to clear UI “present” state).
    TagRemoved { uid: Vec<u8> },
}

/// Display text of [`NfcError::ReadOnly`] (stringly-typed across the I2C service channel).
pub const TAG_READ_ONLY_MSG: &str = "tag is read-only (locked)";

/// NFC hardware / mock errors.
#[derive(Debug, Error)]
pub enum NfcError {
    #[error("no tag in field")]
    NoTag,
    #[error("tag left the field during operation")]
    TagLost,
    #[error("unsupported tag type")]
    UnsupportedTag,
    /// NB_06: the tag refuses writes (lock bits / read-only capability container).
    /// The message is matched by the UI to fall back to the locked-tag associate path.
    #[error("{}", TAG_READ_ONLY_MSG)]
    ReadOnly,
    #[error("tag NDEF capacity unknown")]
    CapacityUnknown,
    #[error("NDEF payload does not fit tag capacity ({need} > {have})")]
    CapacityExceeded { need: usize, have: usize },
    #[error("NDEF error: {0}")]
    Ndef(#[from] plippa_noubin::NdefError),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("{0}")]
    Other(String),
}
