//! NFC device trait.

use super::types::{NfcError, TagInfo};

/// Low-level NFC transceiver operations used by session + associate helpers.
pub trait NfcDevice {
    /// Poll once for a tag in the field. `None` if nothing present.
    fn poll(&mut self) -> Result<Option<TagInfo>, NfcError>;

    /// Read the full NDEF contents of the currently selected / last-polled tag.
    ///
    /// Returned bytes must be parseable by [`plippa_noubin::extract_noubin_key`]
    /// (raw NDEF message and/or Type 2 NDEF TLV).
    fn read_full_ndef(&mut self) -> Result<Vec<u8>, NfcError>;

    /// Write (or create) the first NDEF record on the current tag.
    ///
    /// `ndef_message` is a complete NDEF message (not Type 2 TLV-wrapped).
    /// Blank tags get a new NDEF; tags with existing data overwrite the first record
    /// by replacing the whole message with this single-record message (§3.4).
    fn write_first_ndef_record(&mut self, ndef_message: &[u8]) -> Result<(), NfcError>;
}
