//! The production [`plippa_store::Transport`]: the hardened network seam under the
//! Bandcamp provider (BCS_03).
//!
//! The provider makes no network calls of its own; it hands a credential-bearing URL
//! to this type. Every rule lives here, once:
//!
//! * **https only, port 443 only, no userinfo** and the host must be on the provider's
//!   [`AuthenticatedHosts`] allowlist (`bandcamp.com` and subdomains). Redirects are
//!   followed by hand — at most [`MAX_REDIRECTS`] — and each hop is re-checked against
//!   the same allowlist before a socket is opened (`bcbits.com` is the only extra
//!   destination, and it is never authenticated).
//! * **Every resolved address is vetted** by the existing [`VettingResolver`] from
//!   `web/fetch.rs`: a DNS answer containing a private, loopback, link-local or
//!   otherwise blocked address fails closed (SSRF / DNS rebinding).
//! * **A fixed `User-Agent`** is sent on every request and any caller-supplied one is
//!   dropped. Bandcamp answers a UA-less request with a Fastly "Client Challenge"
//!   HTML page instead of JSON, so this is a functional requirement, not politeness.
//! * **No ambient state**: no cookies, no `Referer`, no `Authorization`, no proxy, no
//!   connection reuse. Only the provider's own protocol headers (`Accept`, and the
//!   credentials in the query string it built) reach the wire.
//! * **Bounded**: connect 10 s, whole call 20 s, and the body is read only up to the
//!   cap the provider asked for; beyond it the read stops with a transport error.
//! * **Nothing is logged.** The URL carries the Subsonic token, so it never reaches a
//!   log line; errors use `StoreError`'s redacting constructors, which keep only
//!   scheme/host/path.
//!
//! The allow-local bypass exists only under `cfg(test)` (local fixture servers); there
//! is no runtime flag, environment variable or request parameter that enables it.

use std::io::Read;
use std::time::Duration;

use plippa_store::{AuthenticatedHosts, HeaderMap, StoreError, Transport, TransportResponse};

use crate::web::fetch::{map_transport, VettingResolver};

/// `User-Agent` sent on every store request (fixed: no user data, no device id).
pub const STORE_USER_AGENT: &str = concat!("Plippa-Store/", env!("CARGO_PKG_VERSION"));
/// TCP connect timeout for one hop.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-call timeout for one hop (the portal budgets 20 s for connect/validate).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// Redirect hops followed before the request is refused.
pub const MAX_REDIRECTS: usize = 3;

/// Headers a caller may not set: they would add ambient credentials or a different
/// identity than the one this transport guarantees.
const DROPPED_HEADERS: [&str; 6] = [
    "host",
    "cookie",
    "referer",
    "authorization",
    "proxy-authorization",
    "user-agent",
];

/// Production transport for the store provider.
#[derive(Debug, Clone)]
pub struct StoreTransport {
    hosts: AuthenticatedHosts,
    #[cfg(test)]
    allow_local: bool,
}

impl StoreTransport {
    /// The Bandcamp profile: authenticated `bandcamp.com`, redirects may reach
    /// `bcbits.com`.
    pub fn production() -> Self {
        Self::with_hosts(AuthenticatedHosts::bandcamp())
    }

    /// A transport for an explicit allowlist (kept for tests of other store profiles).
    pub fn with_hosts(hosts: AuthenticatedHosts) -> Self {
        Self {
            hosts,
            #[cfg(test)]
            allow_local: false,
        }
    }

    /// Plain-http loopback fixture servers (unit tests only).
    #[cfg(test)]
    pub(crate) fn for_local_fixtures(hosts: AuthenticatedHosts) -> Self {
        Self {
            hosts,
            allow_local: true,
        }
    }

    /// The allowlist every request and redirect is checked against.
    pub fn allowlist(&self) -> &AuthenticatedHosts {
        &self.hosts
    }

    #[cfg(test)]
    fn local(&self, url: &str) -> bool {
        self.allow_local && is_loopback_http(url)
    }

    fn check_first(&self, url: &str) -> Result<(), StoreError> {
        #[cfg(test)]
        if self.local(url) {
            return Ok(());
        }
        self.hosts.check_authenticated(url)
    }

    fn check_hop(&self, url: &str) -> Result<(), StoreError> {
        #[cfg(test)]
        if self.local(url) {
            return Ok(());
        }
        self.hosts.check_redirect(url)
    }

    fn agent(&self) -> ureq::Agent {
        let allow_local = {
            #[cfg(test)]
            {
                self.allow_local
            }
            #[cfg(not(test))]
            {
                false
            }
        };
        ureq::AgentBuilder::new()
            .redirects(0)
            .https_only(!allow_local)
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout(CALL_TIMEOUT)
            .timeout_read(CALL_TIMEOUT)
            .timeout_write(CONNECT_TIMEOUT)
            .resolver(VettingResolver::new(allow_local))
            // No connection reuse: one request, one socket, no shared state.
            .max_idle_connections(0)
            .build()
    }

    fn get_once(
        &self,
        url: &str,
        headers: &HeaderMap,
        max_bytes: u64,
    ) -> Result<TransportResponse, StoreError> {
        let agent = self.agent();
        let mut request = agent
            .get(url)
            .set("User-Agent", STORE_USER_AGENT)
            .set("Accept-Encoding", "identity");
        for (name, value) in headers {
            let lower = name.trim().to_ascii_lowercase();
            if lower.is_empty() || DROPPED_HEADERS.contains(&lower.as_str()) {
                continue;
            }
            if !valid_header_name(&lower) || !valid_header_value(value) {
                continue;
            }
            request = request.set(name.trim(), value);
        }
        let response = match request.call() {
            Ok(response) => response,
            // Redirects are off, so 3xx arrives here; keep the head either way.
            Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(failure)) => {
                return Err(match map_transport(&failure) {
                    crate::web::fetch::TransportError::Blocked(reason) => {
                        StoreError::policy_url(url, format!("address refused: {reason}"))
                    }
                    crate::web::fetch::TransportError::Timeout => {
                        StoreError::transport_url(url, "timed out")
                    }
                    crate::web::fetch::TransportError::Connect(reason) => {
                        StoreError::unreachable_url(url, reason)
                    }
                })
            }
        };

        let status = response.status();
        let declared = response
            .header("Content-Length")
            .and_then(|value| value.trim().parse::<u64>().ok());
        if let Some(length) = declared {
            if length > max_bytes {
                return Err(StoreError::transport_url(
                    url,
                    format!("response would be {length} bytes, above the {max_bytes}-byte cap"),
                ));
            }
        }

        let mut out_headers: HeaderMap = Vec::new();
        for name in response.headers_names() {
            for value in response.all(&name) {
                out_headers.push((name.clone(), value.to_string()));
            }
        }

        let mut body: Vec<u8> = Vec::new();
        let mut reader = response.into_reader();
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let read = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) => {
                    return Err(StoreError::transport_url(
                        url,
                        format!("reading the response failed: {err}"),
                    ))
                }
            };
            if body.len() as u64 + read as u64 > max_bytes {
                return Err(StoreError::transport_url(
                    url,
                    format!("response body exceeded the {max_bytes}-byte cap"),
                ));
            }
            body.extend_from_slice(&chunk[..read]);
        }

        Ok(TransportResponse {
            status,
            headers: out_headers,
            body,
            final_url: url.to_string(),
            redirects: Vec::new(),
        })
    }
}

impl Default for StoreTransport {
    fn default() -> Self {
        Self::production()
    }
}

impl Transport for StoreTransport {
    fn get(
        &self,
        url: &str,
        headers: &HeaderMap,
        max_bytes: u64,
    ) -> Result<TransportResponse, StoreError> {
        self.check_first(url)?;

        let mut current = url.to_string();
        let mut redirects: Vec<String> = Vec::new();
        loop {
            let mut response = self.get_once(&current, headers, max_bytes)?;
            let status = response.status;
            if !matches!(status, 301 | 302 | 303 | 307 | 308) {
                response.final_url = current;
                response.redirects = redirects;
                return Ok(response);
            }
            if redirects.len() >= MAX_REDIRECTS {
                return Err(StoreError::policy_url(
                    &current,
                    format!("more than {MAX_REDIRECTS} redirects"),
                ));
            }
            let location = response
                .header("location")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    StoreError::policy_url(&current, "a redirect carried no Location header")
                })?
                .to_string();
            let Some(next) = resolve_location(&current, &location) else {
                return Err(StoreError::policy_url(
                    &current,
                    "a redirect carried a relative or malformed Location",
                ));
            };
            // Vendors redirect to a data CDN; that hop is checked separately and may
            // never receive the authenticated headers again (the query is gone).
            self.check_hop(&next)?;
            redirects.push(next.clone());
            current = next;
        }
    }
}

/// Resolve an absolute `Location` against `current`, or `None` when it is not usable.
///
/// Only absolute `http(s)://host/...` targets are accepted: a relative or
/// protocol-relative value would mean guessing at a host, which is exactly the kind
/// of parsing gap this layer exists to avoid. Bandcamp's own redirects are absolute.
fn resolve_location(_current: &str, location: &str) -> Option<String> {
    let lower = location.to_ascii_lowercase();
    let absolute = lower.starts_with("https://") || lower.starts_with("http://");
    if !absolute {
        return None;
    }
    if location
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return None;
    }
    // An absolute target stands on its own; `_current` documents that relative
    // locations would have to be resolved against it and are deliberately refused.
    Some(location.to_string())
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn valid_header_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value
            .bytes()
            .all(|b| b == b'\t' || (b >= 0x20 && b != 0x7f))
}

#[cfg(test)]
fn is_loopback_http(url: &str) -> bool {
    url.starts_with("http://127.0.0.1:") || url.starts_with("http://127.0.0.1/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A tiny HTTP/1.1 server: one canned response per connection, requests recorded.
    struct Fixture {
        addr: std::net::SocketAddr,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Fixture {
        fn start(responses: Vec<String>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
            let addr = listener.local_addr().expect("addr");
            let seen = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&seen);
            std::thread::spawn(move || {
                for response in responses {
                    let Ok((stream, _)) = listener.accept() else {
                        return;
                    };
                    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                    let mut head = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            break;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        head.push_str(&line);
                    }
                    recorded.lock().unwrap().push(head);
                    let mut stream = stream;
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            Self { addr, seen }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.addr)
        }

        fn requests(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    fn ok(body: &'static str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn redirect(location: &str) -> String {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    fn local_transport() -> StoreTransport {
        StoreTransport::for_local_fixtures(AuthenticatedHosts::bandcamp())
    }

    #[test]
    fn one_get_sends_the_fixed_user_agent_and_provider_headers() {
        let fixture = Fixture::start(vec![ok("{\"subsonic-response\":{\"status\":\"ok\"}}")]);
        let headers: HeaderMap = vec![
            ("Accept".into(), "application/json".into()),
            // A caller must not be able to choose the UA or smuggle a cookie.
            ("User-Agent".into(), "evil".into()),
            ("Cookie".into(), "session=1".into()),
        ];
        let response = local_transport()
            .get(&fixture.url("/rest/getArtists.view"), &headers, 1024)
            .expect("200");
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type().as_deref(), Some("application/json"));
        assert!(!response.looks_like_html());

        let request = fixture.requests().pop().expect("one request");
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("user-agent: plippa-store/"), "{request}");
        assert!(!lower.contains("evil"), "{request}");
        assert!(!lower.contains("cookie:"), "{request}");
        assert!(lower.contains("accept: application/json"), "{request}");
    }

    #[test]
    fn an_off_allowlist_first_url_is_refused_without_a_socket() {
        let err = StoreTransport::production()
            .get("https://evil.example.com/x?u=U&t=T", &HeaderMap::new(), 1024)
            .expect_err("must refuse");
        assert_eq!(err.code(), "policy");
        assert!(err.to_string().contains("evil.example.com"), "{err}");
        assert!(!err.to_string().contains("u=U"), "{err}");
    }

    #[test]
    fn redirects_are_followed_one_hop_at_a_time_and_recorded() {
        let destination = Fixture::start(vec![ok("second")]);
        let second = destination.url("/stream");
        let first = Fixture::start(vec![redirect(&second)]);

        let response = local_transport()
            .get(&first.url("/rest/stream.view"), &HeaderMap::new(), 1024)
            .expect("follow");
        assert_eq!(response.body, b"second");
        assert_eq!(response.final_url, second);
        assert_eq!(response.redirects, vec![second.clone()]);
        assert_eq!(first.requests().len(), 1);
        assert_eq!(destination.requests().len(), 1);
    }

    #[test]
    fn a_redirect_off_the_allowlist_is_refused_before_the_second_request() {
        let first = Fixture::start(vec![redirect("https://evil.example.com/steal?u=U&t=T")]);
        let err = local_transport()
            .get(&first.url("/rest/stream.view"), &HeaderMap::new(), 1024)
            .expect_err("must refuse");
        assert_eq!(err.code(), "policy");
        assert!(err.to_string().contains("evil.example.com"), "{err}");
        // The query of the refused hop is redacted out of the message.
        assert!(!err.to_string().contains("u=U"), "{err}");
        assert_eq!(first.requests().len(), 1);
    }

    #[test]
    fn relative_and_missing_location_headers_are_refused() {
        let fixture = Fixture::start(vec![redirect("/relative")]);
        let err = local_transport()
            .get(&fixture.url("/a"), &HeaderMap::new(), 1024)
            .expect_err("must refuse");
        assert_eq!(err.code(), "policy");

        let fixture = Fixture::start(vec![
            "HTTP/1.1 302 Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        ]);
        let err = local_transport()
            .get(&fixture.url("/a"), &HeaderMap::new(), 1024)
            .expect_err("must refuse");
        assert_eq!(err.code(), "policy");
    }

    #[test]
    fn the_body_cap_is_enforced_while_reading() {
        let body = "x".repeat(4096);
        let fixture = Fixture::start(vec![format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )]);
        let err = local_transport()
            .get(&fixture.url("/big"), &HeaderMap::new(), 256)
            .expect_err("must stop");
        assert_eq!(err.code(), "transport");
        assert!(err.to_string().contains("256"), "{err}");
        assert!(!err.to_string().contains("xxxx"), "body must not be echoed");

        // A declared length above the cap is refused before the body is buffered.
        let fixture = Fixture::start(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 999999\r\nConnection: close\r\n\r\n"
                .to_string(),
        ]);
        let err = local_transport()
            .get(&fixture.url("/big2"), &HeaderMap::new(), 10)
            .expect_err("must stop");
        assert_eq!(err.code(), "transport");
    }

    #[test]
    fn redirect_loops_stop_at_the_hop_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{addr}/loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        let err = local_transport()
            .get(&format!("http://{addr}/start"), &HeaderMap::new(), 1024)
            .expect_err("must stop");
        assert_eq!(err.code(), "policy");
        assert!(err.to_string().contains("redirect"), "{err}");
    }

    #[test]
    fn connection_failures_are_unreachable_and_do_not_leak_the_query() {
        // Nothing listens on port 1 in the loopback range used by fixtures.
        let err = local_transport()
            .get("http://127.0.0.1:1/rest/getArtists.view?u=USER&t=TOKEN&s=SALT", &HeaderMap::new(), 1024)
            .expect_err("must fail");
        let shown = err.to_string();
        assert!(
            matches!(err.code(), "unreachable" | "transport"),
            "unexpected code {}",
            err.code()
        );
        assert!(!shown.contains("USER"), "{shown}");
        assert!(!shown.contains("TOKEN"), "{shown}");
    }

    #[test]
    fn the_local_bypass_does_not_exist_in_production() {
        let transport = StoreTransport::production();
        assert!(transport
            .get("http://127.0.0.1:9/x", &HeaderMap::new(), 16)
            .is_err());
        assert!(transport
            .get("https://bandcamp.com:8443/x", &HeaderMap::new(), 16)
            .is_err());
    }

    #[test]
    fn header_filtering_drops_control_characters_and_unknown_names() {
        let fixture = Fixture::start(vec![ok("{}")]);
        let headers: HeaderMap = vec![
            ("Accept".into(), "application/json".into()),
            ("X-Bad\r\nInjected".into(), "x".into()),
            ("Referer".into(), "https://elsewhere.example".into()),
            ("Authorization".into(), "Bearer abc".into()),
        ];
        local_transport()
            .get(&fixture.url("/x"), &headers, 64)
            .expect("200");
        let request = fixture.requests().pop().expect("one request");
        let lower = request.to_ascii_lowercase();
        assert!(!lower.contains("injected"), "{request}");
        assert!(!lower.contains("referer:"), "{request}");
        assert!(!lower.contains("authorization:"), "{request}");
    }
}
