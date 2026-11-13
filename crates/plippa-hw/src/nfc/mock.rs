//! Deterministic in-memory NFC backend for unit tests.

use super::traits::NfcDevice;
use super::types::{NfcError, TagInfo, TagType};
use plippa_noubin::wrap_type2_ndef_tlv;

/// In-memory tag state for [`MockNfc`].
#[derive(Debug, Clone)]
pub struct MockTag {
    pub uid: Vec<u8>,
    pub tag_type: TagType,
    /// Usable NDEF storage (Type 2 area / Type 4 file size).
    pub ndef_capacity: usize,
    /// Raw NDEF message bytes (empty = blank tag).
    pub ndef_message: Vec<u8>,
    /// NB_06: locked / read-only tag — every write fails with [`NfcError::ReadOnly`].
    pub locked: bool,
}

impl MockTag {
    pub fn blank_type2(uid: impl Into<Vec<u8>>, capacity: usize) -> Self {
        Self {
            uid: uid.into(),
            tag_type: TagType::Type2,
            ndef_capacity: capacity,
            ndef_message: Vec::new(),
            locked: false,
        }
    }

    pub fn blank_type4(uid: impl Into<Vec<u8>>, capacity: usize) -> Self {
        Self {
            uid: uid.into(),
            tag_type: TagType::Type4,
            ndef_capacity: capacity,
            ndef_message: Vec::new(),
            locked: false,
        }
    }

    /// Make the tag read-only (its current NDEF message stays readable).
    pub fn locked(mut self) -> Self {
        self.locked = true;
        self
    }

    pub fn with_ndef_message(mut self, message: Vec<u8>) -> Self {
        self.ndef_message = message;
        self
    }
}

/// Mock NFC: inject tags, optional staged read delays, inspect memory after writes.
#[derive(Debug, Default)]
pub struct MockNfc {
    /// Tags currently “in field”, polled in order (first present wins).
    present: Vec<MockTag>,
    /// Last successfully polled tag index into `present`.
    selected: Option<usize>,
    /// Extra `read_full_ndef` steps that return incomplete until drained (sequencer tests).
    pending_partial_reads: usize,
}

impl MockNfc {
    pub fn new() -> Self {
        Self::default()
    }

    /// Place a single tag in the field (replaces any previous present set).
    pub fn inject(&mut self, tag: MockTag) {
        self.present = vec![tag];
        self.selected = None;
    }

    pub fn clear_field(&mut self) {
        self.present.clear();
        self.selected = None;
    }

    /// Next `n` calls to [`Self::read_full_ndef`] fail with [`NfcError::Other`] (“partial”)
    /// so session tests can prove feedback is withheld until a complete read.
    pub fn set_pending_partial_reads(&mut self, n: usize) {
        self.pending_partial_reads = n;
    }

    pub fn tag_memory(&self) -> Option<&MockTag> {
        self.selected
            .and_then(|i| self.present.get(i))
            .or_else(|| self.present.first())
    }

    pub fn tag_memory_mut(&mut self) -> Option<&mut MockTag> {
        if let Some(i) = self.selected {
            self.present.get_mut(i)
        } else {
            self.present.first_mut()
        }
    }
}

impl NfcDevice for MockNfc {
    fn poll(&mut self) -> Result<Option<TagInfo>, NfcError> {
        if self.present.is_empty() {
            self.selected = None;
            return Ok(None);
        }
        self.selected = Some(0);
        let t = &self.present[0];
        Ok(Some(TagInfo {
            uid: t.uid.clone(),
            tag_type: t.tag_type,
            ndef_capacity: Some(t.ndef_capacity),
        }))
    }

    fn read_full_ndef(&mut self) -> Result<Vec<u8>, NfcError> {
        if self.pending_partial_reads > 0 {
            self.pending_partial_reads -= 1;
            return Err(NfcError::Other("partial read".into()));
        }
        let idx = self.selected.ok_or(NfcError::NoTag)?;
        let t = self.present.get(idx).ok_or(NfcError::NoTag)?;
        if t.ndef_message.is_empty() {
            return Err(NfcError::Ndef(plippa_noubin::NdefError::NoRecord));
        }
        Ok(match t.tag_type {
            TagType::Type2 => wrap_type2_ndef_tlv(&t.ndef_message),
            TagType::Type4 => t.ndef_message.clone(),
        })
    }

    fn write_first_ndef_record(&mut self, ndef_message: &[u8]) -> Result<(), NfcError> {
        let idx = self.selected.ok_or(NfcError::NoTag)?;
        let t = self.present.get_mut(idx).ok_or(NfcError::NoTag)?;
        if t.locked {
            return Err(NfcError::ReadOnly);
        }
        let stored_len = match t.tag_type {
            TagType::Type2 => wrap_type2_ndef_tlv(ndef_message).len(),
            TagType::Type4 => ndef_message.len(),
        };
        if stored_len > t.ndef_capacity {
            return Err(NfcError::CapacityExceeded {
                need: stored_len,
                have: t.ndef_capacity,
            });
        }
        // Blank create or overwrite first record: replace whole message with this single record.
        t.ndef_message = ndef_message.to_vec();
        Ok(())
    }
}
