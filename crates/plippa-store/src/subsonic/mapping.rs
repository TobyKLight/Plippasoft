//! Mapping from Subsonic JSON objects to Plippa's value types.
//!
//! The shape is deliberately forgiving about text (a `null` year, a missing
//! `songCount`, a field a server quotes as a string) and strict about ids: an entry
//! without an id is unusable and reported as a malformed reply rather than dropped,
//! because silently losing a release from someone's paid collection is worse than an
//! error the operator can see.

use serde_json::Value;

use crate::error::StoreError;
use crate::types::{
    AudioFormat, StoreArtist, StoreCover, StoreCoverId, StoreRelease, StoreReleaseId,
    StoreSearchHit, StoreTrack, StoreTrackRef,
};

use super::time::parse_rfc1123_unix;

/// The single object Subsonic emits where an array of one was expected.
pub(crate) fn as_list(value: Option<&Value>) -> Vec<&Value> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
    }
}

/// A trimmed, non-empty string field.
pub(crate) fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// A non-negative integer field, accepting a quoted number or an integral float.
pub(crate) fn unsigned(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64().or_else(|| {
            number
                .as_f64()
                .filter(|float| float.is_finite() && *float >= 0.0)
                .map(|float| float.round() as u64)
        }),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn u64_field(value: &Value, key: &str) -> Option<u64> {
    unsigned(value.get(key)?)
}

pub(crate) fn u32_field(value: &Value, key: &str) -> Option<u32> {
    u32::try_from(u64_field(value, key)?).ok()
}

pub(crate) fn u16_field(value: &Value, key: &str) -> Option<u16> {
    u16::try_from(u64_field(value, key)?).ok()
}

/// A duration in the protocol's unit (whole seconds) as milliseconds.
fn duration_ms(value: &Value, key: &str) -> Option<u64> {
    u64_field(value, key)?.checked_mul(1_000)
}

fn cover(value: &Value) -> Option<StoreCover> {
    string_field(value, "coverArt").map(|id| StoreCover {
        id: StoreCoverId::new(id),
        // The protocol has no MIME type for a cover; the bytes decide, not a label.
        mime: None,
    })
}

/// Map one album object (from `getAlbumList2`, `getAlbum` or `getArtist`).
pub(crate) fn release(album: &Value) -> Result<StoreRelease, StoreError> {
    let id = string_field(album, "id").ok_or_else(|| StoreError::Malformed {
        detail: "an album entry carried no id".to_string(),
    })?;
    Ok(StoreRelease {
        id: StoreReleaseId::new(id),
        // The release's own title. Never a track's `album` field: on this store that
        // field has been measured holding the artist name.
        title: string_field(album, "name").unwrap_or_default().to_string(),
        artist: string_field(album, "artist")
            .or_else(|| string_field(album, "albumArtist"))
            .unwrap_or_default()
            .to_string(),
        year: u16_field(album, "year").filter(|year| *year > 0),
        track_count: u32_field(album, "songCount"),
        duration_ms: duration_ms(album, "duration"),
        cover: cover(album),
        // No Subsonic response carries a Bandcamp URL or id (every `album_url_fields`
        // list measured was empty), so there is nothing to put here and nothing may
        // be synthesised from the id.
        web_url: None,
        created_unix: string_field(album, "created").and_then(parse_rfc1123_unix),
    })
}

/// Map one song object.
pub(crate) fn track(song: &Value) -> Result<StoreTrack, StoreError> {
    let id = string_field(song, "id").ok_or_else(|| StoreError::Malformed {
        detail: "a song entry carried no id".to_string(),
    })?;
    Ok(StoreTrack {
        // Opaque on purpose. A single-track release reuses one value for its album and
        // its song, so nothing may infer a kind from a prefix or from the id differing
        // between the two.
        id: StoreTrackRef::new(id),
        title: string_field(song, "title").unwrap_or_default().to_string(),
        track_number: u32_field(song, "track"),
        // Discs are 1-based and the field is absent on this store, meaning one disc.
        disc_number: u32_field(song, "discNumber").or(Some(1)),
        duration_ms: duration_ms(song, "duration"),
        suffix: string_field(song, "suffix").map(str::to_string),
        content_type: string_field(song, "contentType").map(str::to_string),
        declared_bytes: u64_field(song, "size"),
        declared_bitrate_kbps: u32_field(song, "bitRate"),
    })
}

/// Map one artist object.
pub(crate) fn artist(value: &Value) -> Result<StoreArtist, StoreError> {
    let id = string_field(value, "id").ok_or_else(|| StoreError::Malformed {
        detail: "an artist entry carried no id".to_string(),
    })?;
    Ok(StoreArtist {
        id: crate::types::StoreArtistId::new(id),
        name: string_field(value, "name").unwrap_or_default().to_string(),
        album_count: u32_field(value, "albumCount"),
        cover: cover(value),
    })
}

/// Map one `search3` song hit.
pub(crate) fn search_hit(song: &Value) -> Result<StoreSearchHit, StoreError> {
    Ok(StoreSearchHit {
        release_id: string_field(song, "albumId").map(StoreReleaseId::new),
        // `song.album` is deliberately unused: on this store it can hold the artist
        // name, so it is not a release title.
        artist: string_field(song, "artist").map(str::to_string),
        track: track(song)?,
    })
}

/// Identify audio by its first bytes.
///
/// `Content-Type` is not consulted: a Fastly HTML page served as `audio/mpeg` must not
/// count as audio, and one store answered a missing file with an HTML challenge. Only
/// a container this crate knows is accepted.
pub(crate) fn sniff_audio_format(body: &[u8]) -> Option<AudioFormat> {
    if body.starts_with(b"ID3") {
        return Some(AudioFormat::Mp3);
    }
    if body.len() >= 2 && body[0] == 0xff && (body[1] & 0xe0) == 0xe0 {
        // Bare MPEG frame sync: this store's stream has no ID3 header at all.
        return Some(AudioFormat::Mp3);
    }
    if body.starts_with(b"fLaC") {
        return Some(AudioFormat::Flac);
    }
    if body.starts_with(b"OggS") {
        let head = &body[..body.len().min(64)];
        if head.windows(8).any(|window| window == b"OpusHead") {
            return Some(AudioFormat::Opus);
        }
        return Some(AudioFormat::Ogg);
    }
    if body.len() >= 12 && body.starts_with(b"RIFF") && &body[8..12] == b"WAVE" {
        return Some(AudioFormat::Wav);
    }
    if body.len() >= 12 && &body[4..8] == b"ftyp" {
        return Some(AudioFormat::Aac);
    }
    None
}

/// Identify cover art by its first bytes, returning the MIME type to serve.
pub(crate) fn sniff_image_mime(body: &[u8]) -> Option<&'static str> {
    if body.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if body.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if body.starts_with(b"GIF87a") || body.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if body.len() >= 12 && body.starts_with(b"RIFF") && &body[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_object_is_treated_as_a_list() {
        let one: Value = serde_json::json!({"album": {"id": "a:1"}});
        assert_eq!(as_list(one.get("album")).len(), 1);
        let many: Value = serde_json::json!({"album": [{"id": "a:1"}, {"id": "a:2"}]});
        assert_eq!(as_list(many.get("album")).len(), 2);
        let missing: Value = serde_json::json!({});
        assert!(as_list(missing.get("album")).is_empty());
        let null: Value = serde_json::json!({"album": null});
        assert!(as_list(null.get("album")).is_empty());
    }

    #[test]
    fn an_entry_without_an_id_is_an_error_not_a_silent_drop() {
        let no_id: Value = serde_json::json!({"name": "Untitled"});
        assert!(matches!(
            release(&no_id),
            Err(StoreError::Malformed { .. })
        ));
        assert!(matches!(track(&no_id), Err(StoreError::Malformed { .. })));
    }

    #[test]
    fn audio_and_image_sniffing_ignore_the_declared_type() {
        assert_eq!(sniff_audio_format(b"ID3\x04\x00"), Some(AudioFormat::Mp3));
        assert_eq!(
            sniff_audio_format(&[0xff, 0xfb, 0x90, 0x00]),
            Some(AudioFormat::Mp3)
        );
        assert_eq!(sniff_audio_format(b"fLaC\x00"), Some(AudioFormat::Flac));
        assert_eq!(sniff_audio_format(b"OggS\x00OpusHead\x01"), Some(AudioFormat::Opus));
        assert_eq!(sniff_audio_format(b"OggS\x00vorbis"), Some(AudioFormat::Ogg));
        assert_eq!(
            sniff_audio_format(b"RIFF\x00\x00\x00\x00WAVEfmt "),
            Some(AudioFormat::Wav)
        );
        assert_eq!(
            sniff_audio_format(b"\x00\x00\x00\x20ftypM4A "),
            Some(AudioFormat::Aac)
        );
        assert_eq!(sniff_audio_format(b"<!DOCTYPE html><html>"), None);
        assert_eq!(sniff_audio_format(br#"{"error":true}"#), None);
        assert_eq!(sniff_audio_format(b""), None);

        assert_eq!(sniff_image_mime(b"\xff\xd8\xff\xe0jpeg"), Some("image/jpeg"));
        assert_eq!(sniff_image_mime(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(sniff_image_mime(b"GIF89a..."), Some("image/gif"));
        assert_eq!(
            sniff_image_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(sniff_image_mime(b"<html>"), None);
    }
}
