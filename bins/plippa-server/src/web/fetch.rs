//! Hardened HTTPS fetch of `web.noudata` (`NBSecurityRequirements.md` §1 and §2.1).
//!
//! Layering (so every rule is testable without a network):
//!
//! * [`fetch_noudata`] is the policy: consent token, URL validation on the first URL **and on
//!   every redirect hop**, hop limit, deadline, content-type + first-byte sniffing, size cap
//!   enforced while streaming, JSON-object-only parse behind the structural guard, then the
//!   whole-document sanitiser. It only talks to a [`Transport`].
//! * [`Transport`] does exactly one GET without following redirects. [`UreqTransport`] is the
//!   production implementation (`ureq` + rustls, no cookies / proxy / referrer / auth
//!   headers, `Accept-Encoding: identity`, DNS results vetted against the block-list so the
//!   connection is made to the checked address).
//!
//! The only way to switch off the address block-list / https-only / port-443 rules is
//! `cfg(test)` ([`FetchPolicy::local_fixtures`]); there is no runtime flag, env var or request
//! parameter for it.

use plippa_noubin::{
    check_json_object_limits, first_significant_byte, is_blocked_ip, parse_fetch_url,
    parse_noudata_with_warnings, redact_url_for_log, resolve_redirect, sanitise_document,
    sanitise_line, JsonLimitError, NoudataDocument, NoudataWarning, SanitiseOptions, UrlError,
};
use std::io::Read;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `web.noudata` body cap (§3.5: "1MB is expected to be more than enough").
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Redirect hops followed (the 4th redirect is refused).
pub const MAX_REDIRECTS: usize = 3;
/// TCP connect timeout.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-request timeout (all hops together).
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Fixed minimal `User-Agent`: no version detail, no device id.
pub const USER_AGENT: &str = "Plippa-Noubin";
/// Most bytes of a 404 body read to look for the linkhost's `{"message": ...}`.
pub const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
/// Longest linkhost error message shown (characters).
pub const MAX_ERROR_MESSAGE_CHARS: usize = 256;

/// Proof that the user agreed to this specific request. Only [`Consent::from_request`] makes
/// one, and [`fetch_noudata`] cannot be called without it, so an HTTP handler that forgets to
/// check the `consent` field cannot compile a network call.
#[derive(Debug)]
pub struct Consent(());

impl Consent {
    /// `Some(true)` is consent; absent / `false` is refused.
    pub fn from_request(flag: Option<bool>) -> Result<Consent, WebError> {
        if flag == Some(true) {
            Ok(Consent(()))
        } else {
            Err(WebError::ConsentRequired)
        }
    }
}

/// Everything that can go wrong; each maps to a stable [`WebError::code`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebError {
    ConsentRequired,
    /// Web access switched off by configuration.
    Disabled(String),
    /// No network interface with an address.
    Offline(String),
    /// A web fetch is already running.
    Busy,
    /// The key has a local match: the web is never contacted.
    LocalMatch,
    NotAUrl,
    /// URL refused before or during the request (scheme, port, userinfo, private target...).
    BadUrl(String),
    /// A redirect `Location` was refused.
    RedirectRefused(String),
    TooManyRedirects,
    /// DNS answered with a private/reserved address.
    Blocked(String),
    Timeout,
    Connect(String),
    /// 404 with the linkhost's own message when it sent one.
    NotFound(Option<String>),
    HttpStatus(u16),
    TooLarge,
    /// Wrong content type or body is not a JSON object (HTML, XML, array...).
    NotNoudata(String),
    Malformed(String),
    /// `noudataType` is not `release` (or absent): linkhosts MUST NOT publish the other types
    /// (and an unknown value can't be trusted to be one), so it is refused (v0.4.0).
    WrongType(String),
    UnknownSession,
}

impl WebError {
    /// Stable machine code for the portal.
    pub fn code(&self) -> &'static str {
        match self {
            WebError::ConsentRequired => "consent_required",
            WebError::Disabled(_) => "web_disabled",
            WebError::Offline(_) => "offline",
            WebError::Busy => "busy",
            WebError::LocalMatch => "local_match",
            WebError::NotAUrl => "not_a_url",
            WebError::BadUrl(_) => "bad_url",
            WebError::RedirectRefused(_) => "redirect_refused",
            WebError::TooManyRedirects => "too_many_redirects",
            WebError::Blocked(_) => "blocked_target",
            WebError::Timeout => "timeout",
            WebError::Connect(_) => "connect_failed",
            WebError::NotFound(_) => "not_found",
            WebError::HttpStatus(_) => "http_error",
            WebError::TooLarge => "too_large",
            WebError::NotNoudata(_) => "not_noudata",
            WebError::Malformed(_) => "malformed",
            WebError::WrongType(_) => "wrong_noudata_type",
            WebError::UnknownSession => "unknown_session",
        }
    }

    /// HTTP status the portal answers with.
    pub fn http_status(&self) -> u16 {
        match self {
            WebError::ConsentRequired => 403,
            WebError::Disabled(_) | WebError::Offline(_) => 503,
            WebError::Busy => 429,
            WebError::LocalMatch => 409,
            WebError::NotAUrl | WebError::BadUrl(_) => 400,
            WebError::UnknownSession => 404,
            // The request was fine; the upstream failed.
            _ => 502,
        }
    }
}

impl std::fmt::Display for WebError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebError::ConsentRequired => f.write_str("consent is required before any web request"),
            WebError::Disabled(r) => write!(f, "web access is disabled: {r}"),
            WebError::Offline(r) => write!(f, "no network: {r}"),
            WebError::Busy => f.write_str("another web lookup is already running"),
            WebError::LocalMatch => {
                f.write_str("this Noubin is already in your library; the web is not contacted")
            }
            WebError::NotAUrl => f.write_str("this Noubin is not a web address"),
            WebError::BadUrl(m) => write!(f, "address refused: {m}"),
            WebError::RedirectRefused(m) => write!(f, "redirect refused: {m}"),
            WebError::TooManyRedirects => f.write_str("too many redirects"),
            WebError::Blocked(m) => write!(f, "address refused: {m}"),
            WebError::Timeout => f.write_str("the server took too long to answer"),
            WebError::Connect(m) => write!(f, "could not connect: {m}"),
            WebError::NotFound(Some(m)) => write!(f, "not found: {m}"),
            WebError::NotFound(None) => f.write_str("not found (404)"),
            WebError::HttpStatus(c) => write!(f, "the server answered HTTP {c}"),
            WebError::TooLarge => f.write_str("the answer is larger than 1 MB; dropped"),
            WebError::NotNoudata(m) => write!(f, "the server did not send Noubin metadata ({m})"),
            WebError::Malformed(m) => write!(f, "the metadata could not be read ({m})"),
            WebError::WrongType(t) => write!(
                f,
                "the server sent a \"{t}\" Noubin; only a release can be fetched from the web"
            ),
            WebError::UnknownSession => f.write_str("unknown or expired web session"),
        }
    }
}

impl std::error::Error for WebError {}

pub(crate) fn url_error(e: UrlError) -> WebError {
    match e {
        UrlError::Blocked(_) => WebError::Blocked(e.to_string()),
        other => WebError::BadUrl(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Why a single GET failed before a response arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The resolver produced a blocked address (SSRF / DNS rebinding).
    Blocked(String),
    Timeout,
    /// DNS failure, refused connection, TLS failure ...
    Connect(String),
}

/// One HTTP response head plus a streaming body. Dropping `body` closes the connection.
pub struct HopResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub location: Option<String>,
    pub content_length: Option<u64>,
    pub body: Box<dyn Read + Send>,
}

/// A single GET, redirects **not** followed. `url` is already validated and percent-encoded.
pub trait Transport: Send + Sync {
    fn get(&self, url: &str, timeout: Duration) -> Result<HopResponse, TransportError>;

    /// NB_09: one GET for a large body (media / images). There is **no** total timeout (a
    /// 2 GB file legitimately takes long); `stall` is the longest silence tolerated on the
    /// connection, and the caller enforces byte caps, cancel and its own deadline while it
    /// streams. Redirects are not followed and the same address rules apply as for [`get`].
    /// Implementations without a streaming mode (test doubles) fall back to `get`.
    ///
    /// [`get`]: Transport::get
    fn get_download(&self, url: &str, stall: Duration) -> Result<HopResponse, TransportError> {
        self.get(url, stall)
    }
}

/// Reject a resolver answer that contains any blocked address (fail closed: one bad record
/// poisons the whole answer) or that is empty. `allow_local` exists only for `cfg(test)`.
pub(crate) fn vet_addresses(
    addrs: Vec<SocketAddr>,
    allow_local: bool,
) -> std::io::Result<Vec<SocketAddr>> {
    use std::io::{Error, ErrorKind};
    if addrs.is_empty() {
        return Err(Error::new(ErrorKind::NotFound, "no address"));
    }
    if !allow_local {
        for a in &addrs {
            if is_blocked_ip(a.ip()) {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{BLOCKED_MARK}{}", a.ip()),
                ));
            }
            if a.port() != 443 {
                return Err(Error::new(
                    ErrorKind::PermissionDenied,
                    format!("{BLOCKED_MARK}port {}", a.port()),
                ));
            }
        }
    }
    Ok(addrs)
}

const BLOCKED_MARK: &str = "plippa-blocked-target: ";

pub(crate) struct VettingResolver {
    allow_local: bool,
}

impl VettingResolver {
    /// `allow_local` skips the public-address/port-443 check: local fixture servers
    /// only, reachable exclusively from `cfg(test)` constructors.
    pub(crate) fn new(allow_local: bool) -> Self {
        Self { allow_local }
    }
}

impl ureq::Resolver for VettingResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
        let addrs: Vec<SocketAddr> = netloc.to_socket_addrs()?.collect();
        vet_addresses(addrs, self.allow_local)
    }
}

/// Production transport: `ureq` 2 with rustls (Mozilla roots bundled, verification always on).
pub struct UreqTransport {
    allow_local: bool,
}

impl UreqTransport {
    pub fn new() -> Self {
        Self { allow_local: false }
    }

    /// Plain-http, loopback, any port: local fixture servers only.
    #[cfg(test)]
    pub(crate) fn for_local_fixtures() -> Self {
        Self { allow_local: true }
    }

    fn download_agent(&self, stall: Duration) -> ureq::Agent {
        ureq::AgentBuilder::new()
            .redirects(0)
            .https_only(!self.allow_local)
            .timeout_connect(CONNECT_TIMEOUT.min(stall))
            .timeout_read(stall)
            .timeout_write(stall)
            .user_agent(USER_AGENT)
            .resolver(VettingResolver::new(self.allow_local))
            .max_idle_connections(0)
            .build()
    }

    fn agent(&self, total: Duration) -> ureq::Agent {
        ureq::AgentBuilder::new()
            .redirects(0)
            .https_only(!self.allow_local)
            .timeout_connect(CONNECT_TIMEOUT.min(total))
            .timeout(total)
            .user_agent(USER_AGENT)
            .resolver(VettingResolver::new(self.allow_local))
            // No connection reuse: every consented request opens (and closes) its own socket.
            .max_idle_connections(0)
            .build()
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl Transport for UreqTransport {
    fn get(&self, url: &str, timeout: Duration) -> Result<HopResponse, TransportError> {
        let req = self
            .agent(timeout)
            .get(url)
            .set("Accept", "application/json")
            .set("Accept-Encoding", "identity");
        let resp = match req.call() {
            Ok(r) => r,
            // 3xx (redirects are off), 4xx, 5xx arrive as `Status`; the head is still a response.
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => return Err(map_transport(&t)),
        };
        Ok(hop_response(resp))
    }

    fn get_download(&self, url: &str, stall: Duration) -> Result<HopResponse, TransportError> {
        let req = self
            .download_agent(stall)
            .get(url)
            .set("Accept", "*/*")
            .set("Accept-Encoding", "identity");
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => return Err(map_transport(&t)),
        };
        Ok(hop_response(resp))
    }
}

fn hop_response(resp: ureq::Response) -> HopResponse {
    HopResponse {
        status: resp.status(),
        content_type: resp.header("Content-Type").map(str::to_string),
        location: resp.header("Location").map(str::to_string),
        content_length: resp
            .header("Content-Length")
            .and_then(|v| v.trim().parse().ok()),
        body: Box::new(resp.into_reader()),
    }
}

pub(crate) fn map_transport(t: &ureq::Transport) -> TransportError {
    let mut msg = t.to_string();
    let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(t);
    let mut timed_out = false;
    while let Some(e) = src {
        msg.push_str(": ");
        msg.push_str(&e.to_string());
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            timed_out |= matches!(
                io.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            );
        }
        src = e.source();
    }
    if let Some(i) = msg.find(BLOCKED_MARK) {
        return TransportError::Blocked(msg[i + BLOCKED_MARK.len()..].to_string());
    }
    if timed_out || msg.to_ascii_lowercase().contains("timed out") {
        return TransportError::Timeout;
    }
    // ureq prefixes the URL (possibly with a token in its query): drop it, keep a short reason.
    let reason = if msg.starts_with("http") {
        msg.split_once(": ").map_or(msg.as_str(), |(_, r)| r)
    } else {
        msg.as_str()
    };
    TransportError::Connect(reason.chars().take(160).collect())
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// Tunable limits; production values are the constants above.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_body: usize,
    pub max_redirects: usize,
    pub total_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body: MAX_BODY_BYTES,
            max_redirects: MAX_REDIRECTS,
            total_timeout: TOTAL_TIMEOUT,
        }
    }
}

/// Address/scheme policy. Production: https, port 443, public addresses only.
///
/// A policy may additionally carry a store's host allowlist (BCS_04): when it does, the
/// first URL **and every redirect hop** must also be a host that store may reach
/// (`AuthenticatedHosts::redirect_allowed`), so a signed CDN URL and its redirects can never
/// wander off the destinations the store configured.
#[derive(Debug, Clone, Default)]
pub struct FetchPolicy {
    #[cfg(test)]
    allow_local: bool,
    /// None for ordinary web/general downloads; Some for store downloads.
    hosts: Option<Arc<plippa_store::AuthenticatedHosts>>,
}

impl FetchPolicy {
    pub fn production() -> Self {
        Self::default()
    }

    /// A policy confined to one store's allowlist (BCS_04 download worker).
    pub fn for_store(hosts: Arc<plippa_store::AuthenticatedHosts>) -> Self {
        Self {
            #[cfg(test)]
            allow_local: false,
            hosts: Some(hosts),
        }
    }

    /// `http://127.0.0.1:<port>` fixture servers (tests only; other targets stay refused).
    #[cfg(test)]
    pub(crate) fn local_fixtures() -> Self {
        Self {
            allow_local: true,
            hosts: None,
        }
    }

    /// Refuse a host this policy is not allowed to contact (a no-op without an allowlist).
    fn check_store_host(&self, url: &str) -> Result<(), WebError> {
        let Some(hosts) = &self.hosts else {
            return Ok(());
        };
        if hosts.redirect_allowed(url) {
            Ok(())
        } else {
            Err(WebError::Blocked(
                "the address is not a host this store may be contacted at".to_string(),
            ))
        }
    }

    pub(crate) fn first_url(&self, wire: &str) -> Result<String, WebError> {
        #[cfg(test)]
        if self.allow_local && is_local_fixture(wire) {
            return Ok(wire.to_string());
        }
        let url = parse_fetch_url(wire)
            .map(|u| u.to_fetch_string())
            .map_err(url_error)?;
        self.check_store_host(&url)?;
        Ok(url)
    }

    pub(crate) fn next_url(&self, current: &str, location: &str) -> Result<String, WebError> {
        #[cfg(test)]
        if self.allow_local && is_local_fixture(current) {
            let loc = location.trim();
            if is_local_fixture(loc) {
                return Ok(loc.to_string());
            }
            if loc.starts_with('/') && !loc.starts_with("//") {
                let origin_end = current["http://".len()..]
                    .find('/')
                    .map_or(current.len(), |i| i + "http://".len());
                return Ok(format!("{}{loc}", &current[..origin_end]));
            }
            // Anything else (https://169.254..., file:, ...) goes through the real rules with
            // a placeholder base, exactly like production.
            let base = parse_fetch_url("https://placeholder.example/").unwrap();
            return resolve_redirect(&base, location)
                .map(|u| u.to_fetch_string())
                .map_err(|e| WebError::RedirectRefused(e.to_string()));
        }
        let base = parse_fetch_url(current).map_err(url_error)?;
        let next = resolve_redirect(&base, location)
            .map(|u| u.to_fetch_string())
            .map_err(|e| WebError::RedirectRefused(e.to_string()))?;
        self.check_store_host(&next)?;
        Ok(next)
    }
}

#[cfg(test)]
fn is_local_fixture(url: &str) -> bool {
    url.starts_with("http://127.0.0.1:") || url.starts_with("http://127.0.0.1/")
}

/// Result of a successful fetch: sanitised document plus every warning raised on the way.
#[derive(Debug, Clone)]
pub struct FetchedNoudata {
    pub doc: NoudataDocument,
    pub warnings: Vec<NoudataWarning>,
    /// The URL that finally answered (after redirects), query redacted for display/logs.
    pub final_url: String,
}

fn is_json_content_type(ct: &str) -> Option<bool> {
    // Some(true) = JSON-ish, Some(false) = text/plain (tolerated only because the body must
    // be a JSON object anyway), None = refused (HTML, XML, octet-stream, missing ...).
    let base = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if base == "application/json"
        || base == "text/json"
        || (base.starts_with("application/") && base.ends_with("+json"))
    {
        Some(true)
    } else if base == "text/plain" {
        Some(false)
    } else {
        None
    }
}

fn read_capped(
    body: &mut dyn Read,
    cap: usize,
    deadline: Instant,
    sniff_object: bool,
) -> Result<Vec<u8>, WebError> {
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let mut sniffed = !sniff_object;
    loop {
        if Instant::now() >= deadline {
            return Err(WebError::Timeout);
        }
        let n = match body.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(WebError::Timeout)
            }
            Err(e) => return Err(WebError::Connect(e.to_string().chars().take(120).collect())),
        };
        // Enforced while streaming: stop the moment the cap is exceeded, drop what we have.
        if out.len() + n > cap {
            return Err(WebError::TooLarge);
        }
        out.extend_from_slice(&buf[..n]);
        if !sniffed {
            if let Some(b) = first_significant_byte(&out) {
                if b != b'{' {
                    return Err(WebError::NotNoudata(
                        "the answer does not start like a JSON object".into(),
                    ));
                }
                sniffed = true;
            }
        }
    }
    Ok(out)
}

fn not_found_message(bytes: &[u8]) -> Option<String> {
    check_json_object_limits(bytes).ok()?;
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let m = v.get("message")?.as_str()?;
    Some(sanitise_line(m, MAX_ERROR_MESSAGE_CHARS)).filter(|m| !m.is_empty())
}

/// Fetch, validate and sanitise a `web.noudata`.
///
/// `wire_url` comes from [`plippa_noubin::fetch_url_for_key`]. **No socket is opened before
/// `consent` exists and the URL passed validation.**
pub fn fetch_noudata(
    transport: &dyn Transport,
    _consent: Consent,
    policy: &FetchPolicy,
    limits: &Limits,
    wire_url: &str,
) -> Result<FetchedNoudata, WebError> {
    let deadline = Instant::now() + limits.total_timeout;
    let mut url = policy.first_url(wire_url)?;
    let mut hops = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(WebError::Timeout);
        }
        eprintln!("plippa-server: web GET {}", redact_url_for_log(&url));
        let mut resp = transport.get(&url, remaining).map_err(|e| match e {
            TransportError::Blocked(m) => WebError::Blocked(format!("target {m} is not public")),
            TransportError::Timeout => WebError::Timeout,
            TransportError::Connect(m) => WebError::Connect(m),
        })?;
        match resp.status {
            301 | 302 | 303 | 307 | 308 => {
                // Connection dropped with `resp`; the body of a redirect is never read.
                let Some(loc) = resp.location.clone() else {
                    return Err(WebError::RedirectRefused("no Location header".into()));
                };
                drop(resp);
                if hops >= limits.max_redirects {
                    return Err(WebError::TooManyRedirects);
                }
                hops += 1;
                url = policy.next_url(&url, &loc)?;
            }
            200 => {
                let ct = resp.content_type.clone().unwrap_or_default();
                if is_json_content_type(&ct).is_none() {
                    return Err(WebError::NotNoudata(format!(
                        "content type {}",
                        sanitise_line(&ct, 64)
                    )));
                }
                if resp
                    .content_length
                    .is_some_and(|l| l > limits.max_body as u64)
                {
                    return Err(WebError::TooLarge);
                }
                // Both JSON and tolerated text/plain must be objects; sniff while streaming.
                let bytes = read_capped(&mut *resp.body, limits.max_body, deadline, true)?;
                drop(resp);
                return finish(bytes, url);
            }
            404 => {
                let msg = read_capped(&mut *resp.body, MAX_ERROR_BODY_BYTES, deadline, false)
                    .ok()
                    .and_then(|b| not_found_message(&b));
                return Err(WebError::NotFound(msg));
            }
            code => return Err(WebError::HttpStatus(code)),
        }
    }
}

fn finish(bytes: Vec<u8>, url: String) -> Result<FetchedNoudata, WebError> {
    check_json_object_limits(&bytes).map_err(|e| match e {
        JsonLimitError::NotAnObject | JsonLimitError::Empty => WebError::NotNoudata(e.to_string()),
        other => WebError::Malformed(other.to_string()),
    })?;
    let (mut doc, mut warnings) = parse_noudata_with_warnings(&bytes)
        .map_err(|_| WebError::Malformed("not valid JSON".into()))?;
    drop(bytes);
    // `web.noudata` MUST be a `release` (or omit the type): refuse any other declared value
    // before anything else looks at the document. The text is shown in the portal, so it is
    // cleaned and short.
    let declared_other = match doc.noudata_type {
        Some(plippa_noubin::NoudataType::Release) => None,
        Some(t) => Some(t.as_str().to_string()),
        None => doc.extra.get("noudataType").map(|v| match v {
            serde_json::Value::String(s) => sanitise_line(s, 40),
            _ => "?".to_string(),
        }),
    };
    if let Some(t) = declared_other {
        return Err(WebError::WrongType(t));
    }
    // Type rules on the typed document (e.g. a `release` without `releaseData`).
    warnings.extend(plippa_noubin::classify_type(&doc, |_| None).warnings);
    // Sanitise the typed result before anything stores, indexes, writes or shows it.
    warnings.extend(sanitise_document(&mut doc, SanitiseOptions::fetched()));
    Ok(FetchedNoudata {
        doc,
        warnings,
        final_url: redact_url_for_log(&url),
    })
}

// ---------------------------------------------------------------------------
// Tests: real sockets against a local tiny_http fixture server (cfg(test) escape hatch only).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    type Headers = Vec<(String, String)>;

    enum Body {
        Bytes(Vec<u8>),
        /// Unknown length (chunked): `{"a":"` + n filler bytes.
        Chunked(usize),
        /// Known `Content-Length` but a body of `n` filler bytes.
        Sized(usize),
        /// One byte every 100 ms.
        Drip,
    }

    struct Reply {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Body,
        delay_ms: u64,
    }

    impl Reply {
        fn json(body: &str) -> Self {
            Reply {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: Body::Bytes(body.as_bytes().to_vec()),
                delay_ms: 0,
            }
        }
        fn with(status: u16, ct: &str, body: &str) -> Self {
            Reply {
                status,
                headers: vec![("Content-Type", ct.into())],
                body: Body::Bytes(body.as_bytes().to_vec()),
                delay_ms: 0,
            }
        }
        fn redirect(to: &str) -> Self {
            Reply {
                status: 302,
                headers: vec![("Location", to.into())],
                body: Body::Bytes(Vec::new()),
                delay_ms: 0,
            }
        }
    }

    struct Filler(usize);
    impl Read for Filler {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.0);
            buf[..n].fill(b'x');
            self.0 -= n;
            Ok(n)
        }
    }

    struct Drip(usize);
    impl Read for Drip {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.0 == 0 || buf.is_empty() {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(100));
            buf[0] = if self.0 == 60 { b'{' } else { b' ' };
            self.0 -= 1;
            Ok(1)
        }
    }

    struct Fixture {
        port: u16,
        hits: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<Headers>>>,
        server: Arc<tiny_http::Server>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.unblock();
        }
    }

    impl Fixture {
        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }
    }

    fn fixture(handler: impl Fn(&str, usize) -> Reply + Send + Sync + 'static) -> Fixture {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let port = server.server_addr().to_ip().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let (srv, h, sn) = (Arc::clone(&server), Arc::clone(&hits), Arc::clone(&seen));
        std::thread::spawn(move || {
            for rq in srv.incoming_requests() {
                let n = h.fetch_add(1, Ordering::SeqCst) + 1;
                sn.lock().unwrap().push(
                    rq.headers()
                        .iter()
                        .map(|x| {
                            (
                                x.field.to_string().to_ascii_lowercase(),
                                x.value.to_string(),
                            )
                        })
                        .collect(),
                );
                let handler = Arc::clone(&handler);
                std::thread::spawn(move || {
                    let path = rq.url().to_string();
                    let r = handler(&path, n);
                    if r.delay_ms > 0 {
                        std::thread::sleep(Duration::from_millis(r.delay_ms));
                    }
                    let headers: Vec<tiny_http::Header> = r
                        .headers
                        .iter()
                        .map(|(k, v)| {
                            tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
                        })
                        .collect();
                    let code = tiny_http::StatusCode(r.status);
                    let _ = match r.body {
                        Body::Bytes(b) => {
                            let len = b.len();
                            rq.respond(tiny_http::Response::new(
                                code,
                                headers,
                                Cursor::new(b),
                                Some(len),
                                None,
                            ))
                        }
                        Body::Chunked(n) => rq.respond(tiny_http::Response::new(
                            code,
                            headers,
                            Cursor::new(b"{\"a\":\"".to_vec()).chain(Filler(n)),
                            None,
                            None,
                        )),
                        Body::Sized(n) => rq.respond(tiny_http::Response::new(
                            code,
                            headers,
                            Cursor::new(b"{\"a\":\"".to_vec()).chain(Filler(n)),
                            Some(n + 6),
                            None,
                        )),
                        Body::Drip => rq.respond(tiny_http::Response::new(
                            code,
                            headers,
                            Drip(60),
                            None,
                            None,
                        )),
                    };
                });
            }
        });
        Fixture {
            port,
            hits,
            seen,
            server,
        }
    }

    fn run_with(f: &Fixture, path: &str, limits: Limits) -> Result<FetchedNoudata, WebError> {
        let t = UreqTransport::for_local_fixtures();
        fetch_noudata(
            &t,
            Consent::from_request(Some(true)).unwrap(),
            &FetchPolicy::local_fixtures(),
            &limits,
            &f.url(path),
        )
    }

    fn run(f: &Fixture, path: &str) -> Result<FetchedNoudata, WebError> {
        run_with(f, path, Limits::default())
    }

    const GOOD: &str = r#"{
        "noudataType": "release",
        "playableItemTitle": "Album Ünï",
        "playableItemProfile": "music",
        "releaseData": {
            "credits": {"primaryArtists": [{"primaryArtistName": "The Band"}]},
            "releasePlatforms": [
                {"platformName": "Zed", "platformType": "streaming", "platformURL": "https://zed.example/a"},
                {"platformName": "Alpha", "platformType": "store", "platformURL": "https://alpha.example/a"}
            ]
        },
        "mediaList": [{"mediaTitle": "One", "uniteData": {"expectedLength": "3:00"}}]
    }"#;

    #[test]
    fn web_fixture_valid_document_is_fetched_parsed_and_sanitised() {
        let fx = fixture(|_, _| Reply::json(GOOD));
        let got = run(&fx, "/noubin/album/web.noudata").unwrap();
        assert_eq!(got.doc.playable_item_title.as_deref(), Some("Album Ünï"));
        let p = got.doc.release_data.unwrap().release_platforms.unwrap();
        assert_eq!(
            p.iter()
                .map(|x| x.platform_name.as_str())
                .collect::<Vec<_>>(),
            ["Zed", "Alpha"],
            "artist order preserved"
        );
        assert_eq!(fx.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn web_fixture_accepts_vendor_json_and_tolerates_text_plain_objects() {
        for ct in [
            "application/vnd.noudata+json",
            "text/json; charset=utf-8",
            "text/plain",
        ] {
            let fx = fixture(move |_, _| Reply::with(200, ct, GOOD));
            assert!(run(&fx, "/x").is_ok(), "{ct}");
        }
    }

    #[test]
    fn web_fixture_rejects_every_declared_type_but_release() {
        for t in [
            "userPlaylist",
            "looseTrack",
            "linkTrack",
            "hologram",
            "rel\u{202E}ease ",
        ] {
            let body = GOOD.replacen(
                r#""noudataType": "release""#,
                &format!(r#""noudataType": "{t}""#),
                1,
            );
            let fx = fixture(move |_, _| Reply::json(&body));
            let e = run(&fx, "/x").unwrap_err();
            assert!(matches!(e, WebError::WrongType(_)), "{t}: {e:?}");
            assert_eq!(e.code(), "wrong_noudata_type");
            assert!(!e.to_string().contains('\u{202E}'), "shown text is cleaned");
        }
        // a non-string declaration is refused too
        let body = GOOD.replacen(r#""noudataType": "release""#, r#""noudataType": 7"#, 1);
        let fx = fixture(move |_, _| Reply::json(&body));
        assert!(matches!(
            run(&fx, "/x").unwrap_err(),
            WebError::WrongType(_)
        ));
        // `release` and an omitted type are fine
        let omitted = GOOD.replacen(r#""noudataType": "release","#, "", 1);
        let fx = fixture(move |_, _| Reply::json(&omitted));
        assert!(run(&fx, "/x").is_ok());
        // release without releaseData: accepted (nothing to trust), but warned
        let no_rd =
            r#"{"noudataType":"release","playableItemTitle":"T","mediaList":[{"mediaTitle":"x"}]}"#;
        let fx = fixture(move |_, _| Reply::json(no_rd));
        let got = run(&fx, "/x").unwrap();
        assert!(got.warnings.iter().any(|w| w.path == "noudataType"));
    }

    #[test]
    fn web_fixture_two_megabyte_body_aborts_at_one_megabyte() {
        // Known Content-Length: refused from the header.
        let fx = fixture(|_, _| Reply {
            body: Body::Sized(2 * 1024 * 1024),
            ..Reply::json("")
        });
        assert_eq!(run(&fx, "/x").unwrap_err(), WebError::TooLarge);
        // Unknown length (chunked) or a lying header: enforced while streaming.
        let fx = fixture(|_, _| Reply {
            body: Body::Chunked(2 * 1024 * 1024),
            ..Reply::json("")
        });
        assert_eq!(run(&fx, "/x").unwrap_err(), WebError::TooLarge);
    }

    #[test]
    fn web_fixture_html_and_other_non_json_are_rejected() {
        let html = "<!doctype html><html><body>hello</body></html>";
        let fx = fixture(move |_, _| Reply::with(200, "text/html", html));
        assert!(matches!(
            run(&fx, "/x").unwrap_err(),
            WebError::NotNoudata(_)
        ));
        // JSON content type but an HTML body: dropped on the first byte.
        let fx = fixture(move |_, _| Reply::with(200, "application/json", html));
        assert!(matches!(
            run(&fx, "/x").unwrap_err(),
            WebError::NotNoudata(_)
        ));
        // Arrays, XML, octet-stream, missing content type.
        for (ct, body) in [
            ("application/json", "[1,2,3]"),
            ("application/json", "<?xml version=\"1.0\"?><a/>"),
            ("application/octet-stream", GOOD),
            ("image/png", GOOD),
        ] {
            let fx = fixture(move |_, _| Reply::with(200, ct, body));
            assert!(
                matches!(run(&fx, "/x").unwrap_err(), WebError::NotNoudata(_)),
                "{ct} {body:.10}"
            );
        }
        let fx = fixture(|_, _| Reply {
            status: 200,
            headers: vec![],
            body: Body::Bytes(GOOD.as_bytes().to_vec()),
            delay_ms: 0,
        });
        assert!(matches!(
            run(&fx, "/x").unwrap_err(),
            WebError::NotNoudata(_)
        ));
    }

    #[test]
    fn web_fixture_json_bombs_are_refused_before_parsing() {
        let deep = format!("{{\"a\":{}0{}}}", "[".repeat(100), "]".repeat(100));
        let fx = fixture(move |_, _| Reply::json(&deep));
        assert!(matches!(
            run(&fx, "/x").unwrap_err(),
            WebError::Malformed(_)
        ));
        let many = format!("{{\"a\":[{}]}}", vec!["1"; 20_000].join(","));
        let fx = fixture(move |_, _| Reply::json(&many));
        assert!(matches!(
            run(&fx, "/x").unwrap_err(),
            WebError::Malformed(_)
        ));
        let fx = fixture(|_, _| Reply::json("{\"a\": tru"));
        assert!(matches!(
            run(&fx, "/x").unwrap_err(),
            WebError::Malformed(_)
        ));
    }

    #[test]
    fn web_fixture_redirects_are_limited_and_rechecked_on_every_hop() {
        // 3 hops followed, then the document.
        let fx = fixture(|p, _| match p {
            "/a" => Reply::redirect("/b"),
            "/b" => Reply::redirect("/c"),
            "/c" => Reply::redirect("/d"),
            _ => Reply::json(GOOD),
        });
        assert!(run(&fx, "/a").is_ok());
        assert_eq!(fx.hits.load(Ordering::SeqCst), 4);
        // Five-hop chain is rejected on the 4th redirect: 4 requests, never the 5th.
        let fx = fixture(|p, _| {
            let n: usize = p.trim_start_matches("/h").parse().unwrap_or(0);
            if n < 6 {
                Reply::redirect(&format!("/h{}", n + 1))
            } else {
                Reply::json(GOOD)
            }
        });
        assert_eq!(run(&fx, "/h0").unwrap_err(), WebError::TooManyRedirects);
        assert_eq!(fx.hits.load(Ordering::SeqCst), 4);
        // A loop is the same thing.
        let fx = fixture(|_, _| Reply::redirect("/loop"));
        assert_eq!(run(&fx, "/loop").unwrap_err(), WebError::TooManyRedirects);
    }

    #[test]
    fn web_fixture_redirect_to_private_or_bad_targets_is_refused() {
        for target in [
            "https://169.254.169.254/latest/meta-data/",
            "https://10.0.0.1/",
            "https://192.168.1.1/",
            "https://[::1]/",
            "https://[fd00::1]/",
            "https://localhost/",
            "https://printer.local/",
            "http://127.0.0.2/",
            "https://example.com:8443/",
            "https://user:pw@example.com/",
            "file:///etc/passwd",
            "ftp://example.com/x",
            "javascript:alert(1)",
            "data:text/html,hi",
            "gopher://example.com/",
            "relative/path",
        ] {
            let t = target.to_string();
            let fx = fixture(move |_, _| Reply::redirect(&t));
            let err = run(&fx, "/x").unwrap_err();
            assert!(
                matches!(err, WebError::RedirectRefused(_)),
                "{target}: {err:?}"
            );
            assert_eq!(
                fx.hits.load(Ordering::SeqCst),
                1,
                "{target}: followed a refused redirect"
            );
        }
        let fx = fixture(|_, _| Reply {
            headers: vec![],
            ..Reply::redirect("")
        });
        assert!(matches!(
            run(&fx, "/x").unwrap_err(),
            WebError::RedirectRefused(_)
        ));
    }

    #[test]
    fn web_fixture_timeout_is_honoured() {
        let fx = fixture(|_, _| Reply {
            delay_ms: 3000,
            ..Reply::json(GOOD)
        });
        let limits = Limits {
            total_timeout: Duration::from_millis(400),
            ..Limits::default()
        };
        let t0 = Instant::now();
        assert_eq!(run_with(&fx, "/x", limits).unwrap_err(), WebError::Timeout);
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn web_fixture_slow_drip_body_hits_the_total_deadline() {
        let fx = fixture(|_, _| Reply {
            body: Body::Drip,
            ..Reply::json("")
        });
        let limits = Limits {
            total_timeout: Duration::from_millis(500),
            ..Limits::default()
        };
        let t0 = Instant::now();
        assert_eq!(run_with(&fx, "/x", limits).unwrap_err(), WebError::Timeout);
        assert!(t0.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn web_fixture_404_surfaces_linkhost_message_sanitised() {
        let fx = fixture(|_, _| {
            Reply::with(
                404,
                "application/json",
                "{\"message\": \"Content was\\u202E removed\\u0007 by owner\"}",
            )
        });
        assert_eq!(
            run(&fx, "/x").unwrap_err(),
            WebError::NotFound(Some("Content was removed by owner".into()))
        );
        let fx = fixture(|_, _| Reply::with(404, "text/html", "<html>nope</html>"));
        assert_eq!(run(&fx, "/x").unwrap_err(), WebError::NotFound(None));
        let fx = fixture(|_, _| Reply::with(404, "application/json", "{\"message\": 5}"));
        assert_eq!(run(&fx, "/x").unwrap_err(), WebError::NotFound(None));
        let long = "m".repeat(5000);
        let fx = fixture(move |_, _| {
            Reply::with(
                404,
                "application/json",
                &format!("{{\"message\": \"{long}\"}}"),
            )
        });
        match run(&fx, "/x").unwrap_err() {
            WebError::NotFound(Some(m)) => assert_eq!(m.chars().count(), MAX_ERROR_MESSAGE_CHARS),
            e => panic!("{e:?}"),
        }
        let fx = fixture(|_, _| Reply::with(500, "text/plain", "boom"));
        assert_eq!(run(&fx, "/x").unwrap_err(), WebError::HttpStatus(500));
    }

    #[test]
    fn web_fixture_sends_no_ambient_credentials() {
        let fx = fixture(|_, _| {
            let mut r = Reply::json(GOOD);
            r.headers.push(("Set-Cookie", "sid=1; Path=/".into()));
            r
        });
        run(&fx, "/x").unwrap();
        run(&fx, "/x").unwrap();
        for req in fx.seen.lock().unwrap().iter() {
            let names: Vec<&str> = req.iter().map(|(k, _)| k.as_str()).collect();
            for bad in [
                "cookie",
                "referer",
                "authorization",
                "proxy-authorization",
                "origin",
            ] {
                assert!(!names.contains(&bad), "{bad} sent: {names:?}");
            }
            let get = |k: &str| req.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
            assert_eq!(get("user-agent"), Some(USER_AGENT));
            assert_eq!(get("accept-encoding"), Some("identity"));
            assert_eq!(get("accept"), Some("application/json"));
        }
    }

    #[test]
    fn web_fixture_fetched_local_only_properties_are_stripped_with_warning() {
        let body = r#"{"noubinKey": "https://evil/", "noubinKeyNormalised": "evil/",
            "playableItemTitle": "T", "mediaList": [{"localFilePaths": ["/etc/passwd"], "mediaTitle": "x"}]}"#;
        let fx = fixture(move |_, _| Reply::json(body));
        let got = run(&fx, "/x").unwrap();
        assert!(got.doc.noubin_key.is_none() && got.doc.noubin_key_normalised.is_none());
        assert!(got.doc.media_list[0].local_file_paths.is_none());
        assert!(got.warnings.iter().any(|w| w.path == "noubinKey"));
    }

    #[test]
    fn web_fixture_connection_failure_maps_to_connect_error() {
        // A port nobody listens on.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let t = UreqTransport::for_local_fixtures();
        let err = fetch_noudata(
            &t,
            Consent::from_request(Some(true)).unwrap(),
            &FetchPolicy::local_fixtures(),
            &Limits::default(),
            &format!("http://127.0.0.1:{port}/x"),
        )
        .unwrap_err();
        assert!(
            matches!(err, WebError::Connect(_) | WebError::Timeout),
            "{err:?}"
        );
    }

    // ----- production transport: the block-list holds at connect time -------------------

    #[test]
    fn web_production_transport_never_connects_to_loopback_or_plain_http() {
        let fx = fixture(|_, _| Reply::json(GOOD));
        let t = UreqTransport::new();
        // Plain http is refused before any socket is opened.
        assert!(t.get(&fx.url("/x"), Duration::from_secs(2)).is_err());
        // https to a loopback literal: the vetted resolver refuses the address.
        let e = t
            .get(
                &format!("https://127.0.0.1:{}/x", fx.port),
                Duration::from_secs(2),
            )
            .err()
            .unwrap();
        assert!(matches!(e, TransportError::Blocked(_)), "{e:?}");
        // A *name* that resolves to loopback (DNS-rebinding shape) is refused the same way.
        let e = t
            .get(
                &format!("https://localhost:{}/x", fx.port),
                Duration::from_secs(2),
            )
            .err()
            .unwrap();
        assert!(matches!(e, TransportError::Blocked(_)), "{e:?}");
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            fx.hits.load(Ordering::SeqCst),
            0,
            "a request reached the loopback server"
        );
    }

    #[test]
    fn web_vetting_resolver_rules() {
        let sa = |s: &str| -> SocketAddr { s.parse().unwrap() };
        assert!(vet_addresses(vec![sa("93.184.216.34:443")], false).is_ok());
        assert!(vet_addresses(vec![sa("[2606:4700::1111]:443")], false).is_ok());
        for bad in [
            "127.0.0.1:443",
            "10.1.2.3:443",
            "169.254.169.254:443",
            "[::1]:443",
            "[fd00::1]:443",
            "100.64.0.1:443",
            "93.184.216.34:8443",
        ] {
            assert!(vet_addresses(vec![sa(bad)], false).is_err(), "{bad}");
        }
        // One bad record poisons the answer (split-horizon / rebinding mix).
        assert!(vet_addresses(vec![sa("93.184.216.34:443"), sa("10.0.0.1:443")], false).is_err());
        assert!(vet_addresses(vec![], false).is_err());
        assert!(vet_addresses(vec![sa("127.0.0.1:8080")], true).is_ok());
    }

    /// Manual check that the production transport completes a real TLS handshake
    /// (`cargo test -p plippa-server web_live -- --ignored`); needs internet, so not run by default.
    #[test]
    #[ignore = "needs internet access"]
    fn web_live_tls_smoke() {
        let r = UreqTransport::new()
            .get("https://example.com/", Duration::from_secs(15))
            .expect("TLS handshake + GET");
        assert!(r.status >= 200, "status {}", r.status);
    }
}
