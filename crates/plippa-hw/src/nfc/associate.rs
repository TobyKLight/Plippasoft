//! Associate a playable `.noudata` with an NFC tag (§3.4 / §1.16).

use super::traits::NfcDevice;
use super::types::{NfcError, TagType};
use plippa_noubin::{
    encode_text_record, encode_uri_record, generate_local_noubin_key, max_text_key_bytes_type2,
    max_text_key_bytes_type4, normalise_noubin_key, parse_noudata, KeyGenError, LocalKeyParams,
    NoudataDocument, DEFAULT_TEXT_LANG,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Inputs for associating a playable item with the tag currently in the field.
#[derive(Debug)]
pub struct AssociateParams<'a> {
    pub noudata_path: &'a Path,
    pub playable_item_title: &'a str,
    /// Existing `noubinKeyNormalised` values that must not collide.
    pub existing_normalised: &'a HashSet<String>,
    /// If set, write this URI as an NDEF URI record instead of a local Text key.
    pub noubin_url: Option<&'a str>,
    pub random_len: usize,
}

/// Result of a successful associate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssociateResult {
    pub noubin_key: String,
    pub noubin_key_normalised: String,
    pub noudata_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum AssociateError {
    #[error(transparent)]
    Nfc(#[from] NfcError),
    #[error(transparent)]
    KeyGen(#[from] KeyGenError),
    #[error("noudata I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Noudata(#[from] plippa_noubin::NoudataError),
    #[error("tag capacity too small for association")]
    CapacityTooSmall,
    #[error("collision: could not generate a unique key")]
    Collision,
}

/// Generate/shorten key → write NDEF (create or overwrite first record) → update `.noudata`.
pub fn associate_playable_with_tag<D: NfcDevice>(
    device: &mut D,
    params: &AssociateParams<'_>,
) -> Result<AssociateResult, AssociateError> {
    let info = device.poll()?.ok_or(NfcError::NoTag)?;
    let capacity = info.ndef_capacity.ok_or(NfcError::CapacityUnknown)?;

    let (noubin_key, ndef_message) = if let Some(url) = params.noubin_url {
        let key = url.to_string();
        let norm = normalise_noubin_key(&key);
        if params.existing_normalised.contains(&norm) {
            return Err(AssociateError::Collision);
        }
        let msg = encode_uri_record(&key).map_err(NfcError::from)?;
        (key, msg)
    } else {
        let max_key = match info.tag_type {
            TagType::Type2 => max_text_key_bytes_type2(capacity, DEFAULT_TEXT_LANG),
            TagType::Type4 => max_text_key_bytes_type4(capacity, DEFAULT_TEXT_LANG),
        };
        if max_key == 0 {
            return Err(AssociateError::CapacityTooSmall);
        }
        let (key, _norm) = generate_local_noubin_key(&LocalKeyParams {
            playable_item_title: params.playable_item_title,
            max_key_utf8_bytes: max_key,
            existing_normalised: params.existing_normalised,
            random_len: params.random_len.max(1),
        })
        .map_err(|e| match e {
            KeyGenError::CapacityTooSmall => AssociateError::CapacityTooSmall,
            KeyGenError::CollisionExhausted => AssociateError::Collision,
        })?;
        let msg = encode_text_record(&key, DEFAULT_TEXT_LANG).map_err(NfcError::from)?;
        (key, msg)
    };

    let normalised = normalise_noubin_key(&noubin_key);
    device.write_first_ndef_record(&ndef_message)?;

    let bytes = plippa_noubin::read_noudata_capped(params.noudata_path)?;
    let mut doc: NoudataDocument = parse_noudata(&bytes)?;
    doc.set_noubin_keys(&noubin_key, &normalised);
    doc.write_to_path(params.noudata_path).map_err(|e| {
        AssociateError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            e.to_string(),
        ))
    })?;

    Ok(AssociateResult {
        noubin_key,
        noubin_key_normalised: normalised,
        noudata_path: params.noudata_path.to_path_buf(),
    })
}
