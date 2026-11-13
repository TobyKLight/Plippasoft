//! §6.1 Extracting the `noubinKey` from an NDEF record.

use crate::error::NdefError;

/// Extract the literal Noubin Key from an NDEF message (or Type 2 NDEF TLV blob).
///
/// Only the first valid URI (`U`) or Text (`T`) well-known record is accepted.
/// The key is returned literally: since standard v0.3.0 there is **no** `noubin.com`
/// `?redirect=` unwrapping (phishing risk) — such a URL is an ordinary key.
pub fn extract_noubin_key(data: &[u8]) -> Result<String, NdefError> {
    let message = locate_ndef_message(data)?;
    first_uri_or_text_key(message)
}

/// BCS_07: every URI record of the tag, in tag order, without duplicates.
///
/// The tap flow needs the platform links the tag declares; reading them here means no
/// consumer has to parse the NDEF message again. Bounded twice ([`max_records`] links of at
/// most [`max_bytes`] UTF-8 bytes each) because the payload is untrusted tag text. Records
/// that are not well-known URIs are skipped; a malformed record ends the walk with what was
/// collected so far (a tag that is half-readable still yields the readable links).
pub fn extract_uri_records(data: &[u8], max_records: usize, max_bytes: usize) -> Vec<String> {
    let Ok(message) = locate_ndef_message(data) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    let _ = walk_records(message, |record| {
        if record.tnf != 0x01 || record.record_type != b"U" {
            return None;
        }
        if let Ok(uri) = parse_uri_payload(record.payload) {
            if !uri.is_empty() && uri.len() <= max_bytes && !out.iter().any(|seen| seen == &uri) {
                out.push(uri);
            }
        }
        // Keep walking: a tag may declare several links.
        if out.len() >= max_records {
            return Some(());
        }
        None
    });
    out
}

fn locate_ndef_message(data: &[u8]) -> Result<&[u8], NdefError> {
    if data.is_empty() {
        return Err(NdefError::NoRecord);
    }
    if looks_like_ndef_record(data) {
        return Ok(data);
    }
    if let Some(msg) = extract_type2_ndef_tlv(data) {
        return Ok(msg);
    }
    // Fall back: treat input as raw NDEF message bytes.
    Ok(data)
}

fn looks_like_ndef_record(data: &[u8]) -> bool {
    if data.len() < 3 {
        return false;
    }
    let header = data[0];
    let tnf = header & 0x07;
    let type_len = data[1] as usize;
    tnf == 0x01 && type_len == 1 && data.len() > 3 && (data[3] == b'U' || data[3] == b'T')
}

fn extract_type2_ndef_tlv(data: &[u8]) -> Option<&[u8]> {
    let mut i = 0;
    while i < data.len() {
        let tlv_type = *data.get(i)?;
        if tlv_type == 0x00 {
            i += 1;
            continue;
        }
        if tlv_type == 0xFE {
            break;
        }
        if tlv_type != 0x03 {
            let len = *data.get(i + 1)? as usize;
            i = i.checked_add(2 + len)?;
            continue;
        }
        let len_byte = *data.get(i + 1)?;
        let (msg_start, len) = if len_byte == 0xFF {
            let len = u16::from_be_bytes([*data.get(i + 2)?, *data.get(i + 3)?]) as usize;
            (i + 4, len)
        } else {
            (i + 2, len_byte as usize)
        };
        return data.get(msg_start..msg_start + len);
    }
    None
}

/// One NDEF record as [`walk_records`] hands it over.
struct Record<'a> {
    tnf: u8,
    record_type: &'a [u8],
    payload: &'a [u8],
}

/// Walk every record of an NDEF message until `f` returns `Some`.
///
/// Returns `Ok(None)` when the message ended, `Ok(Some(value))` when `f` stopped the walk,
/// and [`NdefError::Malformed`] when a record header / length runs past the input.
fn walk_records<'a, T>(
    message: &'a [u8],
    mut f: impl FnMut(Record<'a>) -> Option<T>,
) -> Result<Option<T>, NdefError> {
    let mut off = 0;
    let mut saw_record = false;
    while off < message.len() {
        let header = *message.get(off).ok_or(NdefError::Malformed)?;
        let sr = header & 0x10 != 0;
        let il = header & 0x08 != 0;
        let tnf = header & 0x07;
        off += 1;

        let type_len = *message.get(off).ok_or(NdefError::Malformed)? as usize;
        off += 1;

        let payload_len = if sr {
            let len = *message.get(off).ok_or(NdefError::Malformed)? as usize;
            off += 1;
            len
        } else {
            if message.len() < off + 4 {
                return Err(NdefError::Malformed);
            }
            let len = u32::from_be_bytes([
                message[off],
                message[off + 1],
                message[off + 2],
                message[off + 3],
            ]) as usize;
            off += 4;
            len
        };

        let id_len = if il {
            let n = *message.get(off).ok_or(NdefError::Malformed)? as usize;
            off += 1;
            n
        } else {
            0
        };

        if message.len() < off + type_len + id_len + payload_len {
            return Err(NdefError::Malformed);
        }
        let record_type = &message[off..off + type_len];
        off += type_len;
        off += id_len; // skip id
        let payload = &message[off..off + payload_len];
        off += payload_len;
        saw_record = true;

        if let Some(value) = f(Record {
            tnf,
            record_type,
            payload,
        }) {
            return Ok(Some(value));
        }
    }

    if saw_record {
        Ok(None)
    } else {
        Err(NdefError::NoRecord)
    }
}

fn first_uri_or_text_key(message: &[u8]) -> Result<String, NdefError> {
    let found = walk_records(message, |record| {
        if record.tnf != 0x01 {
            return None;
        }
        if record.record_type == b"U" {
            return Some(parse_uri_payload(record.payload));
        }
        if record.record_type == b"T" {
            return Some(parse_text_payload(record.payload));
        }
        // Unsupported type — keep scanning for the first URI/Text.
        None
    })?;
    match found {
        Some(key) => key,
        None => Err(NdefError::UnsupportedType),
    }
}

fn parse_uri_payload(payload: &[u8]) -> Result<String, NdefError> {
    let code = *payload.first().ok_or(NdefError::Malformed)?;
    let rest = payload.get(1..).ok_or(NdefError::Malformed)?;
    let prefix = uri_identifier_prefix(code);
    let rest = std::str::from_utf8(rest).map_err(|_| NdefError::InvalidUtf8)?;
    Ok(format!("{prefix}{rest}"))
}

fn uri_identifier_prefix(code: u8) -> &'static str {
    match code {
        0x00 => "",
        0x01 => "http://www.",
        0x02 => "https://www.",
        0x03 => "http://",
        0x04 => "https://",
        0x05 => "tel:",
        0x06 => "mailto:",
        0x07 => "ftp://anonymous:anonymous@",
        0x08 => "ftp://ftp.",
        0x09 => "ftps://",
        0x0A => "sftp://",
        0x0B => "smb://",
        0x0C => "nfs://",
        0x0D => "ftp://",
        0x0E => "dav://",
        0x0F => "news:",
        0x10 => "telnet://",
        0x11 => "imap:",
        0x12 => "rtsp://",
        0x13 => "urn:",
        0x14 => "pop:",
        0x15 => "sip:",
        0x16 => "sips:",
        0x17 => "tftp:",
        0x18 => "btspp://",
        0x19 => "btl2cap://",
        0x1A => "btgoep://",
        0x1B => "tcpobex://",
        0x1C => "irdaobex://",
        0x1D => "file://",
        0x1E => "urn:epc:id:",
        0x1F => "urn:epc:tag:",
        0x20 => "urn:epc:pat:",
        0x21 => "urn:epc:raw:",
        0x22 => "urn:epc:",
        0x23 => "urn:nfc:",
        _ => "",
    }
}

fn parse_text_payload(payload: &[u8]) -> Result<String, NdefError> {
    let status = *payload.first().ok_or(NdefError::Malformed)?;
    let lang_len = (status & 0x3F) as usize;
    let utf16 = status & 0x80 != 0;
    let text_start = 1 + lang_len;
    let text_bytes = payload.get(text_start..).ok_or(NdefError::Malformed)?;
    if utf16 {
        decode_utf16_bytes(text_bytes)
    } else {
        String::from_utf8(text_bytes.to_vec()).map_err(|_| NdefError::InvalidUtf8)
    }
}

fn decode_utf16_bytes(bytes: &[u8]) -> Result<String, NdefError> {
    if bytes.len() < 2 {
        return Err(NdefError::InvalidUtf8);
    }
    let (be, data) = if bytes.starts_with(&[0xFE, 0xFF]) {
        (true, &bytes[2..])
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        (false, &bytes[2..])
    } else {
        // NFC Forum: UTF-16 without BOM is big-endian.
        (true, bytes)
    };
    if data.len() % 2 != 0 {
        return Err(NdefError::InvalidUtf8);
    }
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| {
            if be {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16(&units).map_err(|_| NdefError::InvalidUtf8)
}
