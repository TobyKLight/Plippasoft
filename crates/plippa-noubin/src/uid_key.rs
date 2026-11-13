//! §1.18.1 UID Noubin Key: a Private Noubin key derived from the tag UID, for tags that have
//! no readable NDEF Text or URI record and cannot be written (§6.1 "UID DATA").

use std::fmt::Write as _;

/// Literal prefix of a UID Noubin Key (keeps it apart from plain text keys).
pub const UID_KEY_PREFIX: &str = "uid:";

/// Cascade tag byte used during ISO 14443-3A anticollision; never part of the UID.
const CASCADE_TAG: u8 = 0x88;

/// Why a tag UID cannot be a Noubin Key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UidKeyError {
    /// Not an ISO 14443-3A UID (4, 7 or 10 bytes).
    BadLength,
    /// A 4 byte UID starting `0x08` is a random UID per ISO 14443-3: it changes on every
    /// activation, so it can never be matched again.
    RandomUid,
    /// Two activations of the tag gave different UIDs.
    Unstable,
}

impl std::fmt::Display for UidKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UidKeyError::BadLength => f.write_str("this tag has no usable ID"),
            UidKeyError::RandomUid | UidKeyError::Unstable => {
                f.write_str("this tag presents a random ID and cannot be used")
            }
        }
    }
}

impl std::error::Error for UidKeyError {}

/// Remove cascade tag bytes from raw anticollision data (`CT uid0 uid1 uid2 | uid3..`), which
/// some readers return instead of the bare UID. A bare 4, 7 or 10 byte UID is returned as is.
fn strip_cascade_tags(raw: &[u8]) -> Vec<u8> {
    match raw {
        [CASCADE_TAG, rest @ ..] if rest.len() == 7 => rest.to_vec(),
        [CASCADE_TAG, a, b, c, CASCADE_TAG, rest @ ..] if rest.len() == 7 => {
            [&[*a, *b, *c][..], rest].concat()
        }
        _ => raw.to_vec(),
    }
}

/// The UID Noubin Key for a tag UID: `uid:` and the UID bytes as lowercase hex, most
/// significant byte first, no separators (e.g. `uid:04a1b2c3d4e5f6`).
pub fn uid_noubin_key(raw_uid: &[u8]) -> Result<String, UidKeyError> {
    let uid = strip_cascade_tags(raw_uid);
    if !matches!(uid.len(), 4 | 7 | 10) {
        return Err(UidKeyError::BadLength);
    }
    if uid.len() == 4 && uid[0] == 0x08 {
        return Err(UidKeyError::RandomUid);
    }
    let mut key = String::from(UID_KEY_PREFIX);
    for b in &uid {
        let _ = write!(key, "{b:02x}");
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_examples() {
        assert_eq!(uid_noubin_key(&[0x1A, 0x2B, 0x3C, 0x4D]).unwrap(), "uid:1a2b3c4d");
        assert_eq!(
            uid_noubin_key(&[0x04, 0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6]).unwrap(),
            "uid:04a1b2c3d4e5f6"
        );
        assert_eq!(uid_noubin_key(&[0x08, 0x11, 0x22, 0x33]), Err(UidKeyError::RandomUid));
    }

    #[test]
    fn cascade_tags_are_not_part_of_the_uid() {
        assert_eq!(
            uid_noubin_key(&[0x88, 0x04, 0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6]).unwrap(),
            "uid:04a1b2c3d4e5f6"
        );
        assert_eq!(
            uid_noubin_key(&[0x88, 1, 2, 3, 0x88, 4, 5, 6, 7, 8, 9, 10]).unwrap(),
            "uid:0102030405060708090a"
        );
    }

    #[test]
    fn other_lengths_are_refused() {
        assert_eq!(uid_noubin_key(&[]), Err(UidKeyError::BadLength));
        assert_eq!(uid_noubin_key(&[1, 2, 3]), Err(UidKeyError::BadLength));
        assert_eq!(uid_noubin_key(&[1; 8]), Err(UidKeyError::BadLength));
    }

    #[test]
    fn a_random_uid_flag_only_applies_to_four_byte_uids() {
        assert_eq!(uid_noubin_key(&[0x08, 1, 2, 3, 4, 5, 6]).unwrap(), "uid:08010203040506");
    }

    #[test]
    fn uid_keys_survive_normalisation_unchanged() {
        let key = uid_noubin_key(&[0x04, 0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6]).unwrap();
        assert_eq!(crate::normalise_noubin_key(&key), key);
    }
}
