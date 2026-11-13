//! Envelope validation for the Subsonic wire format.
//!
//! Status codes are worthless here: the store answers
//! - `200 OK` with `{"error":true,"error_message":"bad version"}` for an endpoint it
//!   does not implement,
//! - an **empty `500`** for a rejected login,
//! - `200 OK` with a Fastly "Client Challenge" HTML page when the request had no
//!   User-Agent,
//! - `200 OK` with an XML body even though `f=json` was asked for.
//!
//! So every response is classified by its body first and its status second, in the
//! order the live measurements established.

use serde_json::{Map, Value};

use crate::error::StoreError;

/// The `subsonic-response` object of a successful reply.
#[derive(Debug, Clone, PartialEq)]
pub struct SubsonicPayload {
    fields: Map<String, Value>,
    from_xml: bool,
}

impl SubsonicPayload {
    fn from_json(fields: Map<String, Value>) -> Self {
        Self {
            fields,
            from_xml: false,
        }
    }

    fn from_xml() -> Self {
        Self {
            fields: Map::new(),
            from_xml: true,
        }
    }

    /// Read a field of the `subsonic-response` object.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    /// A trimmed, non-empty string field of the `subsonic-response` object.
    pub fn string(&self, key: &str) -> Option<&str> {
        self.fields
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    }

    /// A boolean field of the `subsonic-response` object.
    pub fn boolean(&self, key: &str) -> Option<bool> {
        self.fields.get(key).and_then(Value::as_bool)
    }

    /// `true` when the reply was XML rather than the JSON that `f=json` asked for.
    ///
    /// Only `ping` is documented to answer XML, and it carries no data Plippa needs,
    /// so a data endpoint that answers XML has nothing to offer and says so.
    pub fn is_xml(&self) -> bool {
        self.from_xml
    }
}

/// Classify one Subsonic reply.
///
/// `endpoint` is the endpoint name (`getArtists`, `download`, …) and is used for
/// [`StoreError::Unimplemented`] only — never a URL, so nothing here can leak a
/// credential into an error string.
pub fn parse_response(
    body: &[u8],
    status: u16,
    endpoint: &str,
) -> Result<SubsonicPayload, StoreError> {
    let text = std::str::from_utf8(body).ok();
    let trimmed = text.map(str::trim).unwrap_or("");

    // 1. An empty body is the store's own "bad credentials" signal when it comes with
    //    a 500; anything else empty is a broken reply.
    if trimmed.is_empty() {
        return Err(match status {
            500 => StoreError::BadCredentials,
            429 => StoreError::RateLimited,
            _ => StoreError::Malformed {
                detail: format!("{endpoint} answered HTTP {status} with an empty body"),
            },
        });
    }

    // 2. Data first, heuristic last (PAR-06). A body that starts with `{` or `<?xml` is
    //    never a challenge page: a release may legitimately be called "The Client
    //    Challenge EP", and a valid envelope wins. Only a body that is neither JSON nor
    //    an XML declaration is tested for the challenge marker or an HTML opener.
    if trimmed.starts_with('{') {
        return parse_json(trimmed, status, endpoint);
    }

    // 3. XML with a declaration. `ping` is the only endpoint expected to answer XML.
    if trimmed.starts_with("<?xml") {
        return parse_xml(trimmed, endpoint);
    }

    // 4. An XML envelope without a declaration is still data: the element, not the
    //    prolog, decides. A reply whose text happens to contain the challenge phrase
    //    must be parsed, not refused.
    if trimmed.starts_with('<') && contains_ascii_case_insensitive(body, b"<subsonic-response") {
        return parse_xml(trimmed, endpoint);
    }

    // 5. A bot challenge is HTML, never data, even at HTTP 200.
    if looks_like_html(body) {
        return Err(StoreError::Challenged);
    }

    // 6. Any other XML is not an envelope; `parse_xml` names that in the error.
    if trimmed.starts_with('<') {
        return parse_xml(trimmed, endpoint);
    }

    // 7. Anything else (an audio body, a truncated blob) is not an envelope.
    Err(StoreError::Malformed {
        detail: format!("{endpoint} answered with a body that is not JSON or XML"),
    })
}

fn parse_json(text: &str, status: u16, endpoint: &str) -> Result<SubsonicPayload, StoreError> {
    let document: Value = serde_json::from_str(text).map_err(|_| {
        if status == 429 {
            StoreError::RateLimited
        } else {
            StoreError::Malformed {
                detail: format!("{endpoint} answered HTTP {status} with invalid JSON"),
            }
        }
    })?;

    let Some(document) = document.as_object() else {
        return Err(StoreError::Malformed {
            detail: format!("{endpoint} answered with a JSON value that is not an object"),
        });
    };

    // The server's shorthand error, outside any envelope.
    if document.get("error").and_then(Value::as_bool) == Some(true) {
        let message = document
            .get("error_message")
            .and_then(Value::as_str)
            .unwrap_or("");
        return Err(classify_error_message(message, None, status, endpoint));
    }

    let Some(payload) = document.get("subsonic-response") else {
        return Err(StoreError::Malformed {
            detail: format!("{endpoint} answered without a subsonic-response envelope"),
        });
    };
    let Some(payload) = payload.as_object() else {
        return Err(StoreError::Malformed {
            detail: format!("{endpoint} answered with a subsonic-response that is not an object"),
        });
    };

    let payload_status = payload.get("status").and_then(Value::as_str).unwrap_or("");
    if payload_status.eq_ignore_ascii_case("ok") {
        return Ok(SubsonicPayload::from_json(payload.clone()));
    }

    // A failed envelope carries the real reason in `error`.
    let error = payload.get("error").and_then(Value::as_object);
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_i64);
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Err(classify_error_message(message, code, status, endpoint))
}

/// Turn a protocol error into the variant the caller acts on.
///
/// The message is the server's text, trimmed to a short length; `Display` scrubs any
/// URL out of it, so a hostile server cannot make an error render a credential.
fn classify_error_message(
    message: &str,
    code: Option<i64>,
    status: u16,
    endpoint: &str,
) -> StoreError {
    let lower = message.to_ascii_lowercase();
    if lower.contains("bad version") {
        // The store's answer for every endpoint it does not implement.
        return StoreError::Unimplemented {
            endpoint: endpoint.to_string(),
        };
    }
    if status == 429 {
        return StoreError::RateLimited;
    }
    match code {
        // 40 is the Subsonic "wrong username or password" code.
        Some(40) => return StoreError::BadCredentials,
        // 70 is "the requested data was not found".
        Some(70) => return StoreError::NotFound,
        _ => {}
    }
    let detail = if message.is_empty() {
        format!("{endpoint} failed with no message (code {code:?})")
    } else {
        format!("{endpoint} failed: {} (code {code:?})", brief(message))
    };
    StoreError::Malformed { detail }
}

/// The status attribute and first error of an XML reply.
fn parse_xml(text: &str, endpoint: &str) -> Result<SubsonicPayload, StoreError> {
    // Find the envelope element. Some servers prefix namespaces; a plain scan is
    // enough for the only shape that has been measured.
    if !text.to_ascii_lowercase().contains("<subsonic-response") {
        return Err(StoreError::Malformed {
            detail: format!("{endpoint} answered with XML that is not a subsonic response"),
        });
    }
    let status = attribute(text, "status").unwrap_or("");
    if status.eq_ignore_ascii_case("ok") {
        return Ok(SubsonicPayload::from_xml());
    }
    let message = attribute(text, "message").unwrap_or("");
    let code = attribute(text, "code").and_then(|value| value.parse::<i64>().ok());
    Err(classify_error_message(message, code, 200, endpoint))
}

/// The first `name="value"` attribute in `text`, if any.
fn attribute<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn looks_like_html(body: &[u8]) -> bool {
    if contains_ascii_case_insensitive(body, b"Client Challenge") {
        return true;
    }
    let head: Vec<u8> = body
        .iter()
        .copied()
        .skip_while(|byte| byte.is_ascii_whitespace())
        .take(16)
        .collect();
    starts_with_ascii_case_insensitive(&head, b"<!doctype html")
        || starts_with_ascii_case_insensitive(&head, b"<html")
}

fn starts_with_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

fn brief(text: &str) -> String {
    const LIMIT: usize = 200;
    // The server's message ends up in an error the portal and the device show. Strip
    // control, bidi-override/isolate, zero-width and line-separator characters: a hostile
    // server must not be able to reorder or hide text (PAR-06).
    let mut clean = String::with_capacity(text.len().min(LIMIT));
    for ch in text.chars() {
        if ch.is_control() || is_bidi_or_invisible(ch) {
            clean.push(' ');
            continue;
        }
        clean.push(ch);
    }
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() <= LIMIT {
        return clean;
    }
    let mut out: String = clean.chars().take(LIMIT).collect();
    out.push('…');
    out
}

/// Bidi overrides/isolates and zero-width characters (the same set the `.noudata`
/// sanitiser removes).
fn is_bidi_or_invisible(ch: char) -> bool {
    matches!(
        ch,
        // Arabic letter mark: the fourth bidi control, and the one most often forgotten.
        '\u{061C}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            // Line and paragraph separators: they break a log line or a screen row.
            | '\u{2028}'..='\u{2029}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_500_is_bad_credentials() {
        assert_eq!(
            parse_response(b"", 500, "getArtists"),
            Err(StoreError::BadCredentials)
        );
        assert_eq!(
            parse_response(b"  \n", 500, "getArtists"),
            Err(StoreError::BadCredentials)
        );
    }

    #[test]
    fn challenge_pages_are_challenged_at_any_status() {
        for body in [
            &b"<!DOCTYPE html><html><body>Client Challenge</body></html>"[..],
            b"<html><head><title>...</title></head></html>",
            b"  <!doctype HTML><html></html>",
        ] {
            assert_eq!(
                parse_response(body, 200, "getArtists"),
                Err(StoreError::Challenged),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn a_release_named_client_challenge_does_not_break_the_sync() {
        // The review's proven case: a valid envelope that merely mentions the phrase.
        let body = serde_json::json!({
            "subsonic-response": {
                "status": "ok",
                "version": "1.16.1",
                "albumList2": { "album": [
                    { "id": "a:1", "name": "The Client Challenge EP", "artist": "The Artist" },
                    { "id": "a:2", "name": "Ordinary", "artist": "The Artist" }
                ] }
            }
        })
        .to_string();
        let payload = parse_response(body.as_bytes(), 200, "getAlbumList2")
            .expect("a JSON envelope wins over the heuristic");
        assert!(!payload.is_xml());

        // An XML declaration is authoritative too, even when the text says it.
        let xml = br#"<?xml version="1.0"?><subsonic-response status="ok" version="1.16.1"><albumList2 name="The Client Challenge EP"/></subsonic-response>"#;
        assert!(
            parse_response(xml, 200, "getAlbumList2").is_ok(),
            "an XML envelope is parsed before the challenge heuristic"
        );

        // And so is an envelope with no declaration at all: the element decides, not the
        // prolog, so the phrase inside the data cannot turn a reply into a challenge.
        let bare = br#"<subsonic-response status="ok" version="1.16.1"><albumList2 name="The Client Challenge EP"/></subsonic-response>"#;
        assert!(
            parse_response(bare, 200, "getAlbumList2").is_ok(),
            "a declaration-less envelope is data, not a challenge page"
        );
    }

    #[test]
    fn error_detail_strips_controls_and_bidi_from_the_server_message() {
        let body = serde_json::json!({
            "subsonic-response": {
                "status": "failed",
                "error": {
                    "code": 10,
                    "message": "bad\u{7}thing \u{202E}reversed\u{202C} and\u{0}zero\u{200B}width"
                }
            }
        })
        .to_string();
        let detail = match parse_response(body.as_bytes(), 200, "getAlbum") {
            Err(StoreError::Malformed { detail }) => detail,
            other => panic!("expected Malformed, got {other:?}"),
        };
        assert!(!detail.contains('\u{202E}'), "{detail:?}");
        assert!(!detail.contains('\u{202C}'), "{detail:?}");
        assert!(!detail.contains('\u{200B}'), "{detail:?}");
        assert!(!detail.chars().any(|c| c.is_control()), "{detail:?}");
        assert!(detail.contains("bad thing"), "{detail:?}");
        assert!(detail.contains("reversed"), "{detail:?}");

        // The bidi controls and separators that are not `is_control()`: the Arabic letter
        // mark, and the line/paragraph separators that would break a log line.
        for ch in ['\u{061C}', '\u{2028}', '\u{2029}'] {
            let body = serde_json::json!({
                "subsonic-response": {
                    "status": "failed",
                    "error": { "code": 10, "message": format!("a{ch}b") }
                }
            })
            .to_string();
            let detail = match parse_response(body.as_bytes(), 200, "getAlbum") {
                Err(StoreError::Malformed { detail }) => detail,
                other => panic!("expected Malformed, got {other:?}"),
            };
            assert!(
                !detail.contains(ch),
                "U+{:04X} survived sanitisation: {detail:?}",
                ch as u32
            );
        }
    }

    #[test]
    fn bad_version_is_unimplemented_for_the_named_endpoint() {
        let err = parse_response(
            br#"{"error":true,"error_message":"bad version"}"#,
            200,
            "download",
        )
        .expect_err("unimplemented");
        assert_eq!(
            err,
            StoreError::Unimplemented {
                endpoint: "download".to_string()
            }
        );
    }

    #[test]
    fn other_errors_and_rate_limits_are_not_confused_with_unimplemented() {
        assert!(matches!(
            parse_response(br#"{"error":true,"error_message":"nope"}"#, 200, "x"),
            Err(StoreError::Malformed { .. })
        ));
        assert_eq!(
            parse_response(br#"{"error":true,"error_message":"nope"}"#, 429, "x"),
            Err(StoreError::RateLimited)
        );
        assert_eq!(
            parse_response(
                br#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"Wrong username or password"}}}"#,
                200,
                "getArtists"
            ),
            Err(StoreError::BadCredentials)
        );
        assert_eq!(
            parse_response(
                br#"{"subsonic-response":{"status":"failed","error":{"code":70,"message":"not found"}}}"#,
                200,
                "getAlbum"
            ),
            Err(StoreError::NotFound)
        );
    }

    #[test]
    fn xml_status_is_read_without_an_xml_crate() {
        let ok = parse_response(
            br#"<?xml version="1.0" encoding="UTF-8"?><subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="BandcampServer"/>"#,
            200,
            "ping",
        )
        .expect("xml ok");
        assert!(ok.is_xml());
        assert_eq!(ok.get("status"), None, "an XML reply carries no fields");

        let failed = parse_response(
            br#"<?xml version="1.0"?>
            <subsonic-response status="failed" version="1.16.1">
              <error code="40" message="Wrong username or password"/>
            </subsonic-response>"#,
            200,
            "ping",
        );
        assert_eq!(failed, Err(StoreError::BadCredentials));
    }

    #[test]
    fn junk_bodies_are_malformed_not_data() {
        for body in [&b"not json"[..], b"{}", b"[1,2,3]", b"\x00\x01\x02"] {
            assert!(
                matches!(
                    parse_response(body, 200, "getAlbumList2"),
                    Err(StoreError::Malformed { .. })
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn a_successful_envelope_is_returned() {
        let payload = parse_response(
            br#"{"subsonic-response":{"status":"ok","version":"1.16.1","albumList2":{"album":[]}}}"#,
            200,
            "getAlbumList2",
        )
        .expect("ok");
        assert!(!payload.is_xml());
        assert!(payload.get("albumList2").is_some());
    }
}
