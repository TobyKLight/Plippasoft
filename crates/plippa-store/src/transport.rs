//! The network seam.
//!
//! This crate never opens a socket. A provider asks a [`Transport`] for bytes; the
//! binary that owns the hardened network stack (`plippa-server`'s consent/SSRF
//! layer, byte caps, TLS and redirect policy) implements the trait.
//!
//! Deliberately **not** here: timeouts, TLS configuration, retries, DNS vetting,
//! redirect following beyond what the implementor allows, or any policy about which
//! hosts may be contacted. Those stay in one place — the implementor — so an
//! authenticated request can never bypass the checks an unauthenticated one gets.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Mutex;

use crate::error::{redact_url, StoreError};

/// Extra request headers, in wire order. An empty map means "no extra headers".
///
/// Providers add protocol headers (`User-Agent`, `Accept`); the transport
/// implementor may add or forbid its own (for example, refusing to send credentials
/// to a host it has not vetted).
pub type HeaderMap = Vec<(String, String)>;

/// A completed HTTP GET.
#[derive(Clone, PartialEq, Eq)]
pub struct TransportResponse {
    /// HTTP status code. Providers must not treat this as authoritative: several
    /// store APIs answer `200` for authentication failures.
    pub status: u16,
    /// Response headers, case as received.
    pub headers: HeaderMap,
    /// Response body, already capped by the implementor.
    pub body: Vec<u8>,
    /// URL the body actually came from, after redirects. A bearer capability when
    /// it carries signed parameters; `Debug` redacts it.
    pub final_url: String,
    /// Every redirect target followed, in order. `Debug` redacts them.
    pub redirects: Vec<String>,
}

impl TransportResponse {
    /// A response with no headers, redirects or body.
    pub fn new(status: u16, final_url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body,
            final_url: final_url.into(),
            redirects: Vec::new(),
        }
    }

    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// `Content-Type` without parameters, lowercased.
    pub fn content_type(&self) -> Option<String> {
        self.header("content-type").map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
    }

    /// `true` when the body looks like an HTML page (a bot challenge, a login wall).
    pub fn looks_like_html(&self) -> bool {
        if let Some(content_type) = self.content_type() {
            if content_type.contains("html") {
                return true;
            }
        }
        let head = self.body.iter().take(64).copied().collect::<Vec<u8>>();
        let text = String::from_utf8_lossy(&head).trim_start().to_ascii_lowercase();
        text.starts_with("<!doctype html") || text.starts_with("<html")
    }

    /// The body as UTF-8 text, or `None` when it is not valid UTF-8.
    pub fn body_text(&self) -> Option<&str> {
        std::str::from_utf8(&self.body).ok()
    }
}

impl fmt::Debug for TransportResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redirects: Vec<String> = self.redirects.iter().map(|u| redact_url(u)).collect();
        f.debug_struct("TransportResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .field("final_url", &redact_url(&self.final_url))
            .field("redirects", &redirects)
            .finish()
    }
}

/// Performs one GET on behalf of a provider.
///
/// Implementations own every network policy. They must:
///
/// - refuse to contact a host that is not on the provider's allowlist, including on
///   every redirect hop (see [`crate::AuthenticatedHosts`]);
/// - stop reading at `max_bytes` and report [`StoreError::Transport`] instead of
///   buffering more;
/// - apply their own timeouts, TLS configuration and retry policy;
/// - never log `url` or `TransportResponse::final_url` raw — they carry tokens.
///
/// `headers` is the provider's own header set; an empty slice means none.
pub trait Transport {
    /// GET `url`, following only redirects the implementation permits.
    fn get(
        &self,
        url: &str,
        headers: &HeaderMap,
        max_bytes: u64,
    ) -> Result<TransportResponse, StoreError>;
}

/// One call observed by [`CountingTransport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    /// URL as requested. May carry tokens or signed parameters: never log it raw.
    pub url: String,
    /// Headers the caller supplied.
    pub headers: HeaderMap,
    /// Byte cap the caller supplied.
    pub max_bytes: u64,
}

/// A [`Transport`] double for tests: answers from a queue and records every call.
///
/// It performs no I/O. When the queue is exhausted every further call fails with
/// [`StoreError::Transport`], so a test cannot silently rely on unmade requests.
#[derive(Default)]
pub struct CountingTransport {
    queued: Mutex<VecDeque<Result<TransportResponse, StoreError>>>,
    calls: Mutex<Vec<RecordedCall>>,
}

impl CountingTransport {
    /// An empty transport.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a successful response.
    pub fn push_response(&self, response: TransportResponse) {
        self.lock(&self.queued).push_back(Ok(response));
    }

    /// Queue a transport failure.
    pub fn push_error(&self, error: StoreError) {
        self.lock(&self.queued).push_back(Err(error));
    }

    /// Queue a plain-text `200` response.
    pub fn push_body(&self, url: impl Into<String>, body: impl Into<Vec<u8>>) {
        self.push_response(TransportResponse::new(200, url, body.into()));
    }

    /// Every call so far, oldest first.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.lock(&self.calls).clone()
    }

    /// Number of calls so far.
    pub fn call_count(&self) -> usize {
        self.lock(&self.calls).len()
    }

    /// URLs requested so far, oldest first.
    pub fn urls(&self) -> Vec<String> {
        self.lock(&self.calls)
            .iter()
            .map(|call| call.url.clone())
            .collect()
    }

    /// Responses still queued and unused.
    pub fn queued_len(&self) -> usize {
        self.lock(&self.queued).len()
    }

    fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl fmt::Debug for CountingTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CountingTransport")
            .field("calls", &self.calls().len())
            .field("queued", &self.queued_len())
            .finish()
    }
}

impl Transport for CountingTransport {
    fn get(
        &self,
        url: &str,
        headers: &HeaderMap,
        max_bytes: u64,
    ) -> Result<TransportResponse, StoreError> {
        self.lock(&self.calls).push(RecordedCall {
            url: url.to_string(),
            headers: headers.to_vec(),
            max_bytes,
        });
        match self.lock(&self.queued).pop_front() {
            Some(queued) => queued,
            None => Err(StoreError::transport_url(
                url,
                "counting transport has no queued response",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counting_transport_records_arguments_and_returns_queued_responses() {
        let transport = CountingTransport::new();
        transport.push_response(TransportResponse {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json; charset=utf-8".into())],
            body: br#"{"ok":true}"#.to_vec(),
            final_url: "https://t4.bcbits.com/x?token=abc".into(),
            redirects: vec!["https://bandcamp.com/start?u=U".into()],
        });

        let headers: HeaderMap = vec![("User-Agent".into(), "Plippa/1".into())];
        let response = transport
            .get("https://bandcamp.com/rest/ping.view?u=U&t=T", &headers, 1024)
            .expect("queued response");

        assert_eq!(response.status, 200);
        assert_eq!(response.content_type().as_deref(), Some("application/json"));
        assert_eq!(response.body_text(), Some(r#"{"ok":true}"#));
        assert!(!response.looks_like_html());

        let calls = transport.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].max_bytes, 1024);
        assert_eq!(calls[0].headers, headers);
        assert_eq!(transport.call_count(), 1);
        assert_eq!(transport.queued_len(), 0);
    }

    #[test]
    fn debug_redacts_urls() {
        let response = TransportResponse {
            status: 302,
            headers: Vec::new(),
            body: Vec::new(),
            final_url: "https://t4.bcbits.com/stream?p=s&token=SECRET".into(),
            redirects: vec!["https://bandcamp.com/r?t=T".into()],
        };
        let shown = format!("{response:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(!shown.contains("p=s"), "{shown}");
        assert!(!shown.contains("t=T"), "{shown}");
    }

    #[test]
    fn exhausted_queue_is_a_transport_error_not_a_panic() {
        let transport = CountingTransport::new();
        let err = transport
            .get("https://bandcamp.com/rest/x?u=U&s=S", &HeaderMap::new(), 10)
            .expect_err("queue is empty");
        assert_eq!(err.code(), "transport");
        assert!(!err.to_string().contains("s=S"), "{err}");
        assert_eq!(transport.call_count(), 1);
    }

    #[test]
    fn html_detection_covers_challenge_pages() {
        let mut response = TransportResponse::new(
            200,
            "https://bandcamp.com/x",
            b"<!DOCTYPE html><html><body>Client Challenge</body></html>".to_vec(),
        );
        assert!(response.looks_like_html());
        response.headers = vec![("content-type".into(), "text/html; charset=UTF-8".into())];
        assert!(response.looks_like_html());
        response.headers.clear();
        response.body = b"<html>".to_vec();
        assert!(response.looks_like_html());
        response.body = br#"{"ok":true}"#.to_vec();
        assert!(!response.looks_like_html());
    }
}
