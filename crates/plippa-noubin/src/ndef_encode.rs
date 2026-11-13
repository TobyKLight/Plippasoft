//! Build NDEF URI/Text records and Type 2 NDEF TLV blobs for tag writes (§1.15–1.16, §3.1).

use crate::error::NdefError;

/// Language code used when encoding local Noubin Keys as NDEF Text (`en`).
pub const DEFAULT_TEXT_LANG: &str = "en";

/// Encode a single well-known NDEF Text record (MB+ME+SR, no ID).
///
/// Language code bytes are included in the payload; §6.1 extract discards them.
pub fn encode_text_record(text: &str, lang: &str) -> Result<Vec<u8>, NdefError> {
    if lang.len() > 63 {
        return Err(NdefError::Malformed);
    }
    let text_bytes = text.as_bytes();
    let lang_bytes = lang.as_bytes();
    let payload_len = 1 + lang_bytes.len() + text_bytes.len();
    if payload_len > 255 {
        return Err(NdefError::Malformed);
    }

    let mut out = Vec::with_capacity(4 + payload_len);
    // MB=1 ME=1 CF=0 SR=1 IL=0 TNF=0x01 (NFC Forum well-known)
    out.push(0xD1);
    out.push(0x01); // type length
    out.push(payload_len as u8);
    out.push(b'T');
    out.push(lang_bytes.len() as u8); // UTF-8, lang length in low 6 bits
    out.extend_from_slice(lang_bytes);
    out.extend_from_slice(text_bytes);
    Ok(out)
}

/// Encode a single well-known NDEF URI record (MB+ME+SR, no ID).
///
/// Chooses the longest matching URI Identifier Code prefix (§6.1 expansion table).
pub fn encode_uri_record(uri: &str) -> Result<Vec<u8>, NdefError> {
    let (code, rest) = choose_uri_prefix(uri);
    let rest_bytes = rest.as_bytes();
    let payload_len = 1 + rest_bytes.len();
    if payload_len > 255 {
        return Err(NdefError::Malformed);
    }

    let mut out = Vec::with_capacity(4 + payload_len);
    out.push(0xD1);
    out.push(0x01);
    out.push(payload_len as u8);
    out.push(b'U');
    out.push(code);
    out.extend_from_slice(rest_bytes);
    Ok(out)
}

/// Wrap an NDEF message in a Type 2 NDEF Message TLV + terminator (`0xFE`).
pub fn wrap_type2_ndef_tlv(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 5);
    out.push(0x03);
    if message.len() < 0xFF {
        out.push(message.len() as u8);
    } else {
        out.push(0xFF);
        let len = message.len() as u16;
        out.extend_from_slice(&len.to_be_bytes());
    }
    out.extend_from_slice(message);
    out.push(0xFE);
    out
}

/// Maximum UTF-8 byte length for a local Text key that fits in a Type 2 NDEF area.
///
/// `ndef_area_bytes` is the CC MLEN-derived usable user area (TLV + message + terminator).
pub fn max_text_key_bytes_type2(ndef_area_bytes: usize, lang: &str) -> usize {
    // TLV type+len (short) + terminator + NDEF short header (4) + status + lang
    let fixed = 2 + 1 + 4 + 1 + lang.len();
    ndef_area_bytes.saturating_sub(fixed)
}

/// Maximum UTF-8 byte length for a local Text key in a Type 4 NDEF file (raw message, no TLV).
pub fn max_text_key_bytes_type4(ndef_file_bytes: usize, lang: &str) -> usize {
    let fixed = 4 + 1 + lang.len();
    ndef_file_bytes.saturating_sub(fixed)
}

fn choose_uri_prefix(uri: &str) -> (u8, &str) {
    // Longest-match first among common codes used by Noubin URLs.
    const PREFIXES: &[(u8, &str)] = &[
        (0x02, "https://www."),
        (0x01, "http://www."),
        (0x04, "https://"),
        (0x03, "http://"),
        (0x06, "mailto:"),
        (0x05, "tel:"),
    ];
    for &(code, prefix) in PREFIXES {
        if let Some(rest) = uri.strip_prefix(prefix) {
            return (code, rest);
        }
    }
    (0x00, uri)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndef::extract_noubin_key;

    #[test]
    fn text_round_trip() {
        let msg = encode_text_record("Smooth Jazz.noubin.ABC123", DEFAULT_TEXT_LANG).unwrap();
        assert_eq!(
            extract_noubin_key(&msg).unwrap(),
            "Smooth Jazz.noubin.ABC123"
        );
    }

    #[test]
    fn uri_round_trip_https() {
        let msg = encode_uri_record("https://artist.com/noubin/album/").unwrap();
        assert_eq!(
            extract_noubin_key(&msg).unwrap(),
            "https://artist.com/noubin/album/"
        );
    }

    #[test]
    fn type2_tlv_round_trip() {
        let msg = encode_text_record("play1", DEFAULT_TEXT_LANG).unwrap();
        let tlv = wrap_type2_ndef_tlv(&msg);
        assert_eq!(extract_noubin_key(&tlv).unwrap(), "play1");
    }

    #[test]
    fn forty_eight_byte_budget_fits_random_key() {
        let budget = max_text_key_bytes_type2(48, DEFAULT_TEXT_LANG);
        assert!(budget >= 12, "48-byte tags must fit a short random key");
        assert!(budget < 48);
    }
}
