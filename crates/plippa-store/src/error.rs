//! Error model for the store layer.
//!
//! Every variant has a stable [`StoreError::code`] for logs and the portal, and every
//! `Display` implementation is safe to log: URL-bearing detail is passed through
//! [`redact_url`] and [`scrub`] before it is rendered.
//!
//! Store protocols are not HTTP-semantic: Bandcamp's Subsonic endpoint answers
//! `200 OK` for bad credentials and for unimplemented endpoints alike, so the variant
//! names here describe what the provider *decided*, not what a status line said.

use std::fmt;

use thiserror::Error;

/// Every failure the store layer can report.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StoreError {
    /// The store rejected the credentials (a real authenticated call failed).
    #[error("bad credentials")]
    BadCredentials,

    /// The endpoint exists in the protocol but the server does not implement it.
    #[error("endpoint not implemented by this store: {}", scrub(.endpoint))]
    Unimplemented {
        /// Endpoint name, never a URL.
        endpoint: String,
    },

    /// The server answered with a bot/interstitial page instead of data.
    #[error("challenged by the store (bot check)")]
    Challenged,

    /// The store could not be reached, or the connection failed.
    #[error("store unreachable: {}", scrub(.detail))]
    Unreachable {
        /// Pre-redacted detail.
        detail: String,
    },

    /// The response did not validate against the protocol envelope.
    #[error("malformed response: {}", scrub(.detail))]
    Malformed {
        /// Pre-redacted detail.
        detail: String,
    },

    /// The requested item does not exist.
    #[error("not found")]
    NotFound,

    /// The store asked the caller to slow down.
    #[error("rate limited by the store")]
    RateLimited,

    /// Credentials were empty or malformed before any request was made.
    #[error("invalid credentials")]
    InvalidCredentials,

    /// The transport itself failed (DNS, TLS, byte cap, refused redirect).
    #[error("transport failure: {}", scrub(.detail))]
    Transport {
        /// Pre-redacted detail.
        detail: String,
    },

    /// A crate policy refused the operation: an off-allowlist host, an unsafe
    /// identifier, or a local credential-store failure.
    #[error("policy refusal: {}", scrub(.reason))]
    Policy {
        /// Pre-redacted reason.
        reason: String,
    },
}

impl StoreError {
    /// Stable machine-readable code for logs, the portal and metrics.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadCredentials => "bad_credentials",
            Self::Unimplemented { .. } => "unimplemented",
            Self::Challenged => "challenged",
            Self::Unreachable { .. } => "unreachable",
            Self::Malformed { .. } => "malformed",
            Self::NotFound => "not_found",
            Self::RateLimited => "rate_limited",
            Self::InvalidCredentials => "invalid_credentials",
            Self::Transport { .. } => "transport",
            Self::Policy { .. } => "policy",
        }
    }

    /// A policy refusal with a plain reason (no URL).
    pub fn policy(reason: impl Into<String>) -> Self {
        Self::Policy {
            reason: reason.into(),
        }
    }

    /// A transport failure with a plain detail (no URL).
    pub fn transport(detail: impl Into<String>) -> Self {
        Self::Transport {
            detail: detail.into(),
        }
    }

    /// An `Unreachable` error naming a URL; the URL is redacted before it is stored.
    pub fn unreachable_url(url: &str, detail: impl fmt::Display) -> Self {
        Self::Unreachable {
            detail: format!("{} ({})", redact_url(url), detail),
        }
    }

    /// A `Transport` error naming a URL; the URL is redacted before it is stored.
    pub fn transport_url(url: &str, detail: impl fmt::Display) -> Self {
        Self::Transport {
            detail: format!("{} ({})", redact_url(url), detail),
        }
    }

    /// A `Policy` refusal naming a URL; the URL is redacted before it is stored.
    pub fn policy_url(url: &str, reason: impl fmt::Display) -> Self {
        Self::Policy {
            reason: format!("{} ({})", redact_url(url), reason),
        }
    }
}

/// Render a URL so it can be logged: scheme, host and path only.
///
/// Any query string is replaced with `?<redacted>` and any fragment is dropped,
/// because store URLs carry tokens, salts and signatures in the query (Subsonic
/// `u`/`s`/`t`; the CDN's own signed parameters). Userinfo is dropped as well.
/// A string that does not parse as an absolute URL is never echoed.
pub fn redact_url(url: &str) -> String {
    const UNPARSABLE: &str = "<unparsable url>";

    let Some((scheme, rest)) = url.split_once("://") else {
        return UNPARSABLE.to_string();
    };
    let scheme_ok = !scheme.is_empty()
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
    if !scheme_ok {
        return UNPARSABLE.to_string();
    }

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];
    // Drop any `user:password@` prefix.
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);

    let path = tail.split(['?', '#']).next().unwrap_or("");
    let mut out = format!("{scheme}://{host}{path}");
    if tail.contains('?') {
        out.push_str("?<redacted>");
    }
    out
}

/// Replace every absolute URL embedded in `text` with its [`redact_url`] form.
///
/// Used by `Display`, so a detail message assembled from a raw URL cannot leak the
/// query string even if a provider bypassed the URL-aware constructors.
fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = next_url_start(rest) {
        out.push_str(&rest[..start]);
        let candidate = &rest[start..];
        let end = candidate
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '}')
            })
            .unwrap_or(candidate.len());
        out.push_str(&redact_url(&candidate[..end]));
        rest = &candidate[end..];
    }
    out.push_str(rest);
    out
}

fn next_url_start(text: &str) -> Option<usize> {
    let https = text.find("https://");
    let http = text.find("http://");
    match (https, http) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        assert_eq!(StoreError::BadCredentials.code(), "bad_credentials");
        assert_eq!(
            StoreError::Unimplemented {
                endpoint: "download".into()
            }
            .code(),
            "unimplemented"
        );
        assert_eq!(StoreError::Challenged.code(), "challenged");
        assert_eq!(
            StoreError::Unreachable {
                detail: String::new()
            }
            .code(),
            "unreachable"
        );
        assert_eq!(
            StoreError::Malformed {
                detail: String::new()
            }
            .code(),
            "malformed"
        );
        assert_eq!(StoreError::NotFound.code(), "not_found");
        assert_eq!(StoreError::RateLimited.code(), "rate_limited");
        assert_eq!(StoreError::InvalidCredentials.code(), "invalid_credentials");
        assert_eq!(
            StoreError::Transport {
                detail: String::new()
            }
            .code(),
            "transport"
        );
        assert_eq!(
            StoreError::Policy {
                reason: String::new()
            }
            .code(),
            "policy"
        );
    }

    #[test]
    fn redact_url_drops_query_userinfo_and_fragment() {
        assert_eq!(
            redact_url("https://bandcamp.com/api/subsonic/rest/stream.view?u=U&t=T&s=S"),
            "https://bandcamp.com/api/subsonic/rest/stream.view?<redacted>"
        );
        assert_eq!(
            redact_url("https://user:pass@bandcamp.com:443/x?token=abc#frag"),
            "https://bandcamp.com:443/x?<redacted>"
        );
        assert_eq!(redact_url("https://bcbits.com/a/b"), "https://bcbits.com/a/b");
        assert_eq!(redact_url("not a url"), "<unparsable url>");
        assert_eq!(redact_url(""), "<unparsable url>");
    }

    #[test]
    fn a_failed_signed_request_never_renders_its_secrets() {
        let url = "https://bandcamp.com/api/subsonic/rest/stream.view?u=U&t=T&s=S";
        for err in [
            StoreError::transport_url(url, "connection reset"),
            StoreError::unreachable_url(url, "timeout"),
            StoreError::policy_url(url, "not allowed"),
        ] {
            let shown = err.to_string();
            for secret in ['U', 'T', 'S'] {
                assert!(
                    !shown.contains(secret),
                    "error {shown:?} leaked {secret:?}"
                );
            }
            assert!(shown.contains("bandcamp.com"), "{shown}");
        }
    }

    #[test]
    fn display_scrubs_a_raw_url_smuggled_in_detail() {
        let err = StoreError::Malformed {
            detail: "bad body from https://bandcamp.com/rest?u=U&t=T&s=S now".into(),
        };
        let shown = err.to_string();
        assert!(!shown.contains("u=U"), "{shown}");
        assert!(!shown.contains("t=T"), "{shown}");
        assert!(!contains_secret_query(&shown), "{shown}");
        assert!(shown.contains("https://bandcamp.com/rest?<redacted>"), "{shown}");
    }

    fn contains_secret_query(shown: &str) -> bool {
        shown.contains("s=S") || shown.contains("&t=")
    }

    #[test]
    fn unimplemented_names_only_the_endpoint() {
        let err = StoreError::Unimplemented {
            endpoint: "download".into(),
        };
        assert_eq!(
            err.to_string(),
            "endpoint not implemented by this store: download"
        );
    }
}
