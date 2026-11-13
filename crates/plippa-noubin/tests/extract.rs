//! §6.1 extraction examples from the Noubin standard.

use plippa_noubin::{extract_noubin_key, extract_uri_records};
use plippa_noubin::NdefError;

fn uri(prefix: u8, rest: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(prefix);
    payload.extend_from_slice(rest.as_bytes());
    ndef(b'U', &payload)
}

fn text(text: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2); // lang len "en"
    payload.extend_from_slice(b"en");
    payload.extend_from_slice(text.as_bytes());
    ndef(b'T', &payload)
}

fn ndef(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0xD1, 0x01, payload.len() as u8, ty];
    out.extend_from_slice(payload);
    out
}

#[test]
fn uri_prefix_https() {
    let msg = uri(0x04, "artist.com/noubin/purple-rain/");
    assert_eq!(
        extract_noubin_key(&msg).unwrap(),
        "https://artist.com/noubin/purple-rain/"
    );
}

#[test]
fn uri_prefix_https_www() {
    let msg = uri(0x02, "artist.com/noubin/album/");
    assert_eq!(
        extract_noubin_key(&msg).unwrap(),
        "https://www.artist.com/noubin/album/"
    );
}

#[test]
fn uri_prefix_http() {
    let msg = uri(0x03, "artist.com/noubin/album/");
    assert_eq!(
        extract_noubin_key(&msg).unwrap(),
        "http://artist.com/noubin/album/"
    );
}

#[test]
fn uri_prefix_none() {
    let msg = uri(0x00, "https://artist.com/noubin/album/");
    assert_eq!(
        extract_noubin_key(&msg).unwrap(),
        "https://artist.com/noubin/album/"
    );
}

#[test]
fn uri_keeps_query_fragment_port_userinfo_matrix() {
    assert_eq!(
        extract_noubin_key(&uri(0x04, "artist.com/noubin/album?ref=nfc")).unwrap(),
        "https://artist.com/noubin/album?ref=nfc"
    );
    assert_eq!(
        extract_noubin_key(&uri(0x04, "artist.com/noubin/album#track-3")).unwrap(),
        "https://artist.com/noubin/album#track-3"
    );
    assert_eq!(
        extract_noubin_key(&uri(0x04, "artist.com:8443/noubin/album/")).unwrap(),
        "https://artist.com:8443/noubin/album/"
    );
    assert_eq!(
        extract_noubin_key(&uri(0x04, "user:pass@artist.com/noubin/album/")).unwrap(),
        "https://user:pass@artist.com/noubin/album/"
    );
    assert_eq!(
        extract_noubin_key(&uri(0x04, "artist.com/noubin/album;v=1/")).unwrap(),
        "https://artist.com/noubin/album;v=1/"
    );
}

#[test]
fn uri_keeps_percent_encoding() {
    assert_eq!(
        extract_noubin_key(&uri(0x04, "artist.com/noubin/purple%20rain/")).unwrap(),
        "https://artist.com/noubin/purple%20rain/"
    );
    assert_eq!(
        extract_noubin_key(&uri(0x04, "artist.com/noubin/album?q=hello%20world")).unwrap(),
        "https://artist.com/noubin/album?q=hello%20world"
    );
}

#[test]
fn noubin_com_regular_linkhost_no_unwrap() {
    let msg = uri(0x04, "noubin.com/noubin/artist/album/fnj82974");
    assert_eq!(
        extract_noubin_key(&msg).unwrap(),
        "https://noubin.com/noubin/artist/album/fnj82974"
    );
}

#[test]
fn noubin_com_redirect_parameter_is_never_unwrapped() {
    // Standard v0.3.0 removed the `?redirect=` unwrap (phishing risk): the key is literal.
    let msg = uri(
        0x04,
        "noubin.com/r/?redirect=https%3A%2F%2Fartist.com%2Fnoubin%2Falbum%2F",
    );
    assert_eq!(
        extract_noubin_key(&msg).unwrap(),
        "https://noubin.com/r/?redirect=https%3A%2F%2Fartist.com%2Fnoubin%2Falbum%2F"
    );
    let msg = uri(
        0x04,
        "sub.noubin.com/r/?redirect=https%3A%2F%2Fevil.example%2Fnoubin%2Fx%2F",
    );
    assert!(extract_noubin_key(&msg).unwrap().starts_with("https://sub.noubin.com/r/?redirect="));
}

#[test]
fn text_preserves_whitespace() {
    assert_eq!(
        extract_noubin_key(&text("MyPlaylist01")).unwrap(),
        "MyPlaylist01"
    );
    assert_eq!(
        extract_noubin_key(&text("  summer mix  ")).unwrap(),
        "  summer mix  "
    );
}

#[test]
fn unsupported_type_returns_error() {
    // Well-known type "Sp" (Smart Poster) — not URI/Text.
    let mut msg = vec![0xD1, 0x02, 0x01];
    msg.extend_from_slice(b"Sp");
    msg.push(0x00);
    let err = extract_noubin_key(&msg).unwrap_err();
    assert_eq!(err, NdefError::UnsupportedType);
}

#[test]
fn empty_returns_no_record() {
    assert_eq!(extract_noubin_key(&[]).unwrap_err(), NdefError::NoRecord);
}

#[test]
fn type2_tlv_wrapped_message() {
    let inner = uri(0x04, "artist.com/noubin/album/");
    let mut tlv = vec![0x03, inner.len() as u8];
    tlv.extend_from_slice(&inner);
    tlv.push(0xFE);
    assert_eq!(
        extract_noubin_key(&tlv).unwrap(),
        "https://artist.com/noubin/album/"
    );
}

// ---------------------------------------------------------------------------
// BCS_07: the tap flow wants every platform link the tag declares, not just the key.
// ---------------------------------------------------------------------------

#[test]
fn uri_records_are_collected_in_tag_order_without_duplicates() {
    let mut msg = uri(0x04, "sable-music.bandcamp.com/track/were-getting-older-baby");
    // A second record (different type byte order is irrelevant, only `U` counts).
    let second = uri(0x04, "listen.example/album/1");
    msg.extend_from_slice(&second);
    // A text record is not a platform link.
    msg.extend_from_slice(&text("MyPlaylist01"));
    // The same link twice must appear once.
    msg.extend_from_slice(&uri(0x04, "listen.example/album/1"));

    let urls = extract_uri_records(&msg, 8, 512);
    assert_eq!(
        urls,
        vec![
            "https://sable-music.bandcamp.com/track/were-getting-older-baby".to_string(),
            "https://listen.example/album/1".to_string(),
        ]
    );
    // The key extraction is unchanged and agrees with the first link.
    assert_eq!(
        extract_noubin_key(&msg).unwrap(),
        "https://sable-music.bandcamp.com/track/were-getting-older-baby"
    );
}

#[test]
fn uri_records_are_bounded_and_skip_oversized_links() {
    let mut msg = Vec::new();
    for n in 0..12 {
        msg.extend_from_slice(&uri(0x04, &format!("example.test/{n}")));
    }
    msg.extend_from_slice(&uri(0x04, &"a".repeat(600)));

    let urls = extract_uri_records(&msg, 8, 512);
    assert_eq!(urls.len(), 8, "never more than the cap");
    assert_eq!(urls[0], "https://example.test/0");
    assert!(
        urls.iter().all(|u| u.len() <= 512),
        "an oversized link is dropped, not truncated into a wrong URL"
    );
}

#[test]
fn uri_records_of_a_text_only_tag_are_empty() {
    assert!(extract_uri_records(&text("MyPlaylist01"), 8, 512).is_empty());
    assert!(extract_uri_records(&[], 8, 512).is_empty());
}

#[test]
fn uri_records_survive_a_half_readable_tag() {
    let mut msg = uri(0x04, "example.test/a");
    // A truncated second record header: the walk stops but keeps what it read.
    msg.extend_from_slice(&[0xD1, 0x01, 0x10, b'U', 0x04]);
    assert_eq!(
        extract_uri_records(&msg, 8, 512),
        vec!["https://example.test/a".to_string()]
    );
}
