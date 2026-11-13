//! §6.3 Noubin URL Normalisation: turn a Noubin Key into the Canonical Metadata URL of
//! `web.noudata`.
//!
//! [`normalise_noubin_url`] is the literal spec transformation (its output for
//! `…/purple%20rain/` contains a space, exactly like the spec example). [`fetch_url_for_key`]
//! wraps it for actual use: it validates the key, applies the SSRF/https rules of
//! `NBSecurityRequirements.md` §1 and produces the percent-encoded wire form.
//!
//! Decision (spec 6.3 step 2.5 is ambiguous about case): only the **host** is ASCII
//! lower-cased; path case is preserved because linkhost paths are case-sensitive. `%2F` and
//! `%2f` are treated alike (never decoded).

use crate::netpolicy::{
    check_host_allowed, encode_url_component, parse_host, UrlError, MAX_URL_BYTES,
};
use crate::normalise::{percent_decode_except_2f, strip_port, strip_www_label};
use crate::sanitise::MAX_KEY_BYTES;

/// File name appended to the directory URL.
pub const WEB_NOUDATA: &str = "web.noudata";

/// Why a Noubin Key cannot be turned into a fetch URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchUrlError {
    /// The key is not an `http(s)://` URL (a plain-text Private Noubin).
    NotAUrl,
    KeyTooLong,
    Url(UrlError),
}

impl std::fmt::Display for FetchUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchUrlError::NotAUrl => f.write_str("this Noubin is not a web address"),
            FetchUrlError::KeyTooLong => f.write_str("Noubin key too long"),
            FetchUrlError::Url(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for FetchUrlError {}

impl From<UrlError> for FetchUrlError {
    fn from(e: UrlError) -> Self {
        FetchUrlError::Url(e)
    }
}

/// True when the (trimmed) key is an `http://` or `https://` URL (case-insensitive scheme).
pub fn is_url_noubin_key(key: &str) -> bool {
    let k = key.trim();
    k.get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("https://"))
        || k.get(..7)
            .is_some_and(|p| p.eq_ignore_ascii_case("http://"))
}

/// Literal §6.3 for a URL Noubin Key. Non-URL input is returned trimmed and unchanged.
///
/// Order: https upgrade, drop fragment, stash query, strip userinfo + port, strip `www.`,
/// percent-decode the path except `%2f`, drop a trailing filename with an extension, ensure a
/// trailing slash, append `web.noudata`, re-append the query.
pub fn normalise_noubin_url(key: &str) -> String {
    let s = key.trim();
    if !is_url_noubin_key(s) {
        return s.to_string();
    }
    // 1. https upgrade (scheme is matched case-insensitively; everything after is kept).
    let rest = match s.find("://") {
        Some(i) => &s[i + 3..],
        None => s,
    };
    // 2.1 fragment
    let rest = rest.split_once('#').map_or(rest, |(a, _)| a);
    // 2.2 query
    let (rest, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };
    // 2.3 userinfo + port
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = strip_port(authority).to_ascii_lowercase();
    // 2.4 www.
    let host = strip_www_label(&host);
    // 2.5 path percent-decoding (except %2f)
    let mut path = percent_decode_except_2f(path);
    // 2.6 directory semantics: drop a final `name.ext` segment, ensure a trailing slash
    if !path.ends_with('/') {
        let (dir, last) = match path.rfind('/') {
            Some(i) => (path[..=i].to_string(), path[i + 1..].to_string()),
            None => (String::from("/"), path.clone()),
        };
        let has_ext = last
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty());
        path = if has_ext || last.is_empty() {
            dir
        } else {
            format!("{path}/")
        };
    }
    if path.is_empty() {
        path.push('/');
    }
    // 2.7 web.noudata, 2.8 query
    let mut out = format!("https://{host}{path}{WEB_NOUDATA}");
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        out.push('?');
        out.push_str(q);
    }
    out
}

/// A validated fetch target for a Noubin Key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoubinFetchUrl {
    /// Spec §6.3 form, for showing to the user.
    pub display: String,
    /// Percent-encoded URL to send (`https`, port 443, host passed the name/literal
    /// block-list; resolved addresses still need [`crate::is_blocked_ip`] at connect time).
    pub wire: String,
    /// Lower-case host.
    pub host: String,
}

/// Validate `key` and compute its Canonical Metadata URL.
pub fn fetch_url_for_key(key: &str) -> Result<NoubinFetchUrl, FetchUrlError> {
    if key.len() > MAX_KEY_BYTES {
        return Err(FetchUrlError::KeyTooLong);
    }
    if !is_url_noubin_key(key) {
        return Err(FetchUrlError::NotAUrl);
    }
    if key.chars().any(|c| c.is_control()) {
        return Err(UrlError::BadCharacters.into());
    }
    let display = normalise_noubin_url(key);
    // Split the display form again to encode path and query separately.
    let rest = &display["https://".len()..];
    let (before_q, query) = match rest.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (rest, None),
    };
    let slash = before_q.find('/').unwrap_or(before_q.len());
    let (host_text, path) = before_q.split_at(slash);
    if host_text.contains('@') || host_text.contains(':') && !host_text.starts_with('[') {
        // Userinfo / port were stripped by §6.3; anything left is malformed.
        return Err(UrlError::BadHost.into());
    }
    let host = parse_host(host_text)?;
    check_host_allowed(&host)?;
    let mut wire = format!(
        "https://{}{}",
        host.authority_text(),
        encode_url_component(path, false)
    );
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        wire.push('?');
        wire.push_str(&encode_url_component(q, true));
    }
    if wire.len() > MAX_URL_BYTES {
        return Err(UrlError::TooLong.into());
    }
    Ok(NoubinFetchUrl {
        display,
        wire,
        host: host.authority_text(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_norm_spec_examples() {
        let cases = [
            (
                "https://domain.com/noubin/artist/album",
                "https://domain.com/noubin/artist/album/web.noudata",
            ),
            (
                "https://domain.com/noubin/artist/album/",
                "https://domain.com/noubin/artist/album/web.noudata",
            ),
            (
                "https://domain.com/noubin/artist/album/fnj82974",
                "https://domain.com/noubin/artist/album/fnj82974/web.noudata",
            ),
            (
                "https://domain.com/noubin/artist/album/index.html",
                "https://domain.com/noubin/artist/album/web.noudata",
            ),
            (
                "https://www.artist.com/noubin/album/",
                "https://artist.com/noubin/album/web.noudata",
            ),
            (
                "https://artist.com/noubin/purple%20rain/",
                "https://artist.com/noubin/purple rain/web.noudata",
            ),
            (
                "https://domain.com/noubin/artist/album?ref=nfc",
                "https://domain.com/noubin/artist/album/web.noudata?ref=nfc",
            ),
            (
                "https://domain.com/noubin/artist/album#track-3",
                "https://domain.com/noubin/artist/album/web.noudata",
            ),
            (
                "https://domain.com/noubin/artist/album?ref=nfc#track-3",
                "https://domain.com/noubin/artist/album/web.noudata?ref=nfc",
            ),
            (
                "https://artist.com:8443/noubin/album/",
                "https://artist.com/noubin/album/web.noudata",
            ),
            (
                "https://user:pass@artist.com/noubin/album/",
                "https://artist.com/noubin/album/web.noudata",
            ),
            (
                "https://noubin.com/noubin/artist/album/fnj82974",
                "https://noubin.com/noubin/artist/album/fnj82974/web.noudata",
            ),
            (
                "https://noub.in/fnj82974/",
                "https://noub.in/fnj82974/web.noudata",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(normalise_noubin_url(input), want, "{input}");
        }
    }

    #[test]
    fn url_norm_step_1_upgrades_http() {
        assert_eq!(
            normalise_noubin_url("http://domain.com/a/b"),
            "https://domain.com/a/b/web.noudata"
        );
        assert_eq!(
            normalise_noubin_url("HTTP://Domain.COM/A/b"),
            "https://domain.com/A/b/web.noudata"
        );
    }

    #[test]
    fn url_norm_edge_cases() {
        // Empty path, host only.
        assert_eq!(
            normalise_noubin_url("https://artist.com"),
            "https://artist.com/web.noudata"
        );
        assert_eq!(
            normalise_noubin_url("https://artist.com?x=1"),
            "https://artist.com/web.noudata?x=1"
        );
        // %2f is kept encoded, other escapes decoded (also non-ASCII).
        assert_eq!(
            normalise_noubin_url("https://a.com/x/a%2Fb/%c3%a4/"),
            "https://a.com/x/a%2fb/ä/web.noudata"
        );
        // Colon in a path segment is not a port; matrix params stay.
        assert_eq!(
            normalise_noubin_url("https://a.com/x/album:deluxe;v=1/"),
            "https://a.com/x/album:deluxe;v=1/web.noudata"
        );
        // Filename directly under the root.
        assert_eq!(
            normalise_noubin_url("https://a.com/page.php"),
            "https://a.com/web.noudata"
        );
        // Empty query dropped; query keeps its own escapes verbatim.
        assert_eq!(
            normalise_noubin_url("https://a.com/x?"),
            "https://a.com/x/web.noudata"
        );
        assert_eq!(
            normalise_noubin_url("https://a.com/x?q=hello%20world&r=2"),
            "https://a.com/x/web.noudata?q=hello%20world&r=2"
        );
        // Whitespace around the key is ignored.
        assert_eq!(
            normalise_noubin_url("  https://a.com/x  "),
            "https://a.com/x/web.noudata"
        );
    }

    #[test]
    fn url_norm_fetch_url_is_wire_safe() {
        let u = fetch_url_for_key("https://artist.com/noubin/purple%20rain/?a=b c").unwrap();
        assert_eq!(
            u.display,
            "https://artist.com/noubin/purple rain/web.noudata?a=b c"
        );
        assert_eq!(
            u.wire,
            "https://artist.com/noubin/purple%20rain/web.noudata?a=b%20c"
        );
        assert_eq!(u.host, "artist.com");
        let u = fetch_url_for_key("https://a.com/x/a%2Fb/").unwrap();
        assert_eq!(u.wire, "https://a.com/x/a%2fb/web.noudata");
    }

    #[test]
    fn url_norm_fetch_url_rejects_bad_targets() {
        assert_eq!(
            fetch_url_for_key("MyPlaylist01").unwrap_err(),
            FetchUrlError::NotAUrl
        );
        assert_eq!(
            fetch_url_for_key("ftp://a.com/x").unwrap_err(),
            FetchUrlError::NotAUrl
        );
        for bad in [
            "https://127.0.0.1/x",
            "https://localhost/x",
            "https://[::1]/x",
            "https://169.254.169.254/latest",
            "https://10.0.0.5/x",
            "https://nas.local/x",
            "https://2130706433/x",
            "https://user@evil.com@127.0.0.1/",
            "https://a.com/x\r\nHost: evil",
            "https:///nohost",
        ] {
            assert!(fetch_url_for_key(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            fetch_url_for_key(&format!("https://a.com/{}", "x".repeat(600))).unwrap_err(),
            FetchUrlError::KeyTooLong
        );
        // Port is stripped by §6.3, so it never reaches the connection.
        assert!(fetch_url_for_key("https://a.com:8443/x/")
            .unwrap()
            .wire
            .starts_with("https://a.com/"));
    }

    #[test]
    fn url_norm_does_not_unwrap_noubin_com_redirect() {
        use crate::{encode_uri_record, extract_noubin_key};
        // Standard v0.3.0 removed `?redirect=` unwrapping: a wrapper is fetched as itself
        // (as noubin.com, never as the target it names).
        let wrapped = "https://noubin.com/go?redirect=https%3A%2F%2Fartist.com%2Fnoubin%2Falbum%2F";
        let ndef = encode_uri_record(wrapped).unwrap();
        let key = extract_noubin_key(&ndef).unwrap();
        assert_eq!(key, wrapped);
        let url = normalise_noubin_url(&key);
        assert!(url.starts_with("https://noubin.com/go/web.noudata"), "{url}");
    }
}
