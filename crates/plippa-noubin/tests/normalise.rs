//! §6.2 normalisation examples from the Noubin standard.

use plippa_noubin::{extract_noubin_key, normalise_noubin_key};

fn uri(prefix: u8, rest: &str) -> Vec<u8> {
    let mut payload = vec![prefix];
    payload.extend_from_slice(rest.as_bytes());
    let mut out = vec![0xD1, 0x01, payload.len() as u8, b'U'];
    out.extend_from_slice(&payload);
    out
}

#[test]
fn plain_text_and_whitespace() {
    assert_eq!(normalise_noubin_key("MyPlaylist01"), "myplaylist01");
    assert_eq!(normalise_noubin_key("  summer mix  "), "summer mix");
}

#[test]
fn typical_urls() {
    assert_eq!(
        normalise_noubin_key("https://artist.com/noubin/purple-rain/"),
        "artist.com/noubin/purple-rain/"
    );
    assert_eq!(
        normalise_noubin_key("https://www.artist.com/noubin/album/"),
        "artist.com/noubin/album/"
    );
}

#[test]
fn ascii_case_fold_non_ascii_preserved() {
    assert_eq!(
        normalise_noubin_key("https://Artist.COM/noubin/ÖAlbum/"),
        "artist.com/noubin/Öalbum/"
    );
}

#[test]
fn strip_query_fragment_port_userinfo() {
    assert_eq!(
        normalise_noubin_key("https://artist.com/noubin/album?ref=nfc"),
        "artist.com/noubin/album/"
    );
    assert_eq!(
        normalise_noubin_key("https://artist.com/noubin/album#track-3"),
        "artist.com/noubin/album/"
    );
    assert_eq!(
        normalise_noubin_key("https://artist.com/noubin/album?ref=nfc#track-3"),
        "artist.com/noubin/album/"
    );
    assert_eq!(
        normalise_noubin_key("https://artist.com:8443/noubin/album/"),
        "artist.com/noubin/album/"
    );
    assert_eq!(
        normalise_noubin_key("https://user:pass@artist.com/noubin/album/"),
        "artist.com/noubin/album/"
    );
}

#[test]
fn matrix_and_path_colon_kept() {
    assert_eq!(
        normalise_noubin_key("https://artist.com/noubin/album;v=1/"),
        "artist.com/noubin/album;v=1/"
    );
    assert_eq!(
        normalise_noubin_key("https://artist.com/noubin/album:deluxe/"),
        "artist.com/noubin/album:deluxe/"
    );
}

#[test]
fn path_percent_decoding() {
    assert_eq!(
        normalise_noubin_key("https://artist.com/noubin/purple%20rain/"),
        "artist.com/noubin/purple rain/"
    );
    assert_eq!(
        normalise_noubin_key("https://artist.com/noubin/%61lbum/"),
        "artist.com/noubin/album/"
    );
    assert_eq!(
        normalise_noubin_key("https://artist.com/noubin/a%2Fb/"),
        "artist.com/noubin/a%2fb/"
    );
    assert_eq!(
        normalise_noubin_key("https://artist.com/noubin/album?q=hello%20world"),
        "artist.com/noubin/album/"
    );
}

#[test]
fn trailing_slash_and_filename() {
    assert_eq!(
        normalise_noubin_key("https://domain.com/noubin/artist/album"),
        "domain.com/noubin/artist/album/"
    );
    assert_eq!(normalise_noubin_key("https://artist.com"), "artist.com/");
    assert_eq!(
        normalise_noubin_key("https://domain.com/noubin/artist/album/index.html"),
        "domain.com/noubin/artist/album/index.html/"
    );
}

#[test]
fn noubin_com_linkhost_and_no_redirect_unwrap_after_extract() {
    assert_eq!(
        normalise_noubin_key("https://noubin.com/noubin/artist/album/fnj82974"),
        "noubin.com/noubin/artist/album/fnj82974/"
    );

    // No `?redirect=` unwrapping (standard v0.3.0): the wrapper is an ordinary key.
    let key = extract_noubin_key(&uri(
        0x04,
        "noubin.com/r/?redirect=https%3A%2F%2Fartist.com%2Fnoubin%2Falbum%2F",
    ))
    .unwrap();
    assert_eq!(
        key,
        "https://noubin.com/r/?redirect=https%3A%2F%2Fartist.com%2Fnoubin%2Falbum%2F"
    );
    assert_eq!(normalise_noubin_key(&key), "noubin.com/r/");
}

#[test]
fn www_subdomain_not_stripped_for_other_labels() {
    assert_eq!(
        normalise_noubin_key("https://shop.artist.com/noubin/album/"),
        "shop.artist.com/noubin/album/"
    );
}
