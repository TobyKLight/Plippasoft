//! Web-capable Noubin (NB_08): consent-gated, hardened lookup of a URL Noubin's `web.noudata`.
//!
//! * [`WebHub::prepare`] computes the fetch URL for display; nothing is sent.
//! * [`WebHub::fetch`] performs the request. It needs the explicit `consent` flag (checked
//!   here, server-side, and enforced again by the type system: [`fetch::fetch_noudata`] takes a
//!   [`fetch::Consent`] that only [`fetch::Consent::from_request`] can create).
//! * Result is held **in memory only** under a random session token (short-lived); nothing is
//!   written to the library until the user proceeds in NB_09.
//! * At most one fetch is in flight (a second one gets 429). Nothing here ever runs on its own:
//!   there is no timer, poller or background check.
//!
//! Privacy / how to disable: see the "Web lookups" section of `bins/plippa-server/README.md`.
//! `PLIPPA_WEB=off` in the environment or `--no-web` on the command line turns the whole
//! feature off (`web_capable=false`, reason reported by `/api/status`).

pub mod download;
pub mod fetch;
pub mod overview;

use crate::service::ServerService;
use fetch::{fetch_noudata, Consent, FetchPolicy, Limits, Transport, UreqTransport, WebError};
use overview::ReleaseOverview;
use plippa_noubin::{
    fetch_url_for_key, sanitise_key, sanitise_line, FetchUrlError, NoubinFetchUrl, NoudataDocument,
    NoudataWarning, MAX_KEY_BYTES,
};
use serde::Serialize;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a fetched release is kept in memory.
pub const SESSION_TTL: Duration = Duration::from_secs(10 * 60);
/// Most fetched releases held at once (oldest is evicted).
pub const MAX_SESSIONS: usize = 4;

/// Configuration read once at start-up.
#[derive(Debug, Clone, Default)]
pub struct WebConfig {
    /// `Some(reason)` = the feature is off.
    pub disabled: Option<String>,
}

impl WebConfig {
    /// `PLIPPA_WEB=off|0|false|no|disabled` disables; so does `--no-web` in `args`.
    pub fn from_args_env(args: impl IntoIterator<Item = String>) -> Self {
        if args.into_iter().any(|a| a == "--no-web") {
            return Self {
                disabled: Some("web access is turned off (--no-web)".into()),
            };
        }
        Self::from_env()
    }

    pub fn from_env() -> Self {
        match std::env::var("PLIPPA_WEB") {
            Ok(v)
                if matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "off" | "0" | "false" | "no" | "disabled"
                ) =>
            {
                Self {
                    disabled: Some("web access is turned off (PLIPPA_WEB)".into()),
                }
            }
            _ => Self::default(),
        }
    }
}

/// Reported by `/api/status` and `/api/web/status`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WebStatus {
    /// Enabled by configuration **and** a network interface is up.
    pub web_capable: bool,
    /// Why not, when `web_capable` is false.
    pub reason: Option<String>,
}

/// `POST /api/web/prepare` answer: exactly what a fetch would contact. Nothing was sent.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Prepared {
    /// The key as read from the tag (sanitised for display).
    pub noubin_key: String,
    /// Spec §6.3 Canonical Metadata URL, the full URL to show in the consent dialog.
    pub fetch_url: String,
    pub host: String,
    pub consent_message: String,
}

/// `POST /api/web/fetch` answer.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WebFetchResult {
    /// Opaque session token for NB_09 (`GET /api/web/session/<token>`).
    pub token: String,
    pub overview: ReleaseOverview,
}

/// A fetched, sanitised release held in memory for NB_09.
#[derive(Debug, Clone)]
pub struct HeldRelease {
    pub doc: NoudataDocument,
    pub warnings: Vec<NoudataWarning>,
    /// The Noubin Key that was tapped (`noubinKey` to use when uniting).
    pub noubin_key: String,
    /// URL that answered, query redacted.
    pub source_url: String,
    fetched_at: Instant,
}

/// Network reachability check (injectable for tests).
pub type OnlineCheck = Box<dyn Fn() -> bool + Send + Sync>;

pub struct WebHub {
    cfg: WebConfig,
    transport: Arc<dyn Transport>,
    policy: FetchPolicy,
    limits: Limits,
    online: OnlineCheck,
    in_flight: AtomicBool,
    sessions: Mutex<VecDeque<(String, HeldRelease)>>,
}

/// Clears the in-flight flag however the fetch ends (including a panic).
struct InFlight<'a>(&'a AtomicBool);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// True when some interface other than loopback / link-local has an address.
pub fn network_looks_up() -> bool {
    crate::http::local_interface_ips()
        .iter()
        .any(|ip| match ip {
            IpAddr::V4(v) => !v.is_loopback() && !v.is_link_local() && !v.is_unspecified(),
            IpAddr::V6(v) => !v.is_loopback() && !v.is_unspecified() && !v.is_unicast_link_local(),
        })
}

impl WebHub {
    /// Production hub: real transport, real limits, address-based online check.
    pub fn new(cfg: WebConfig) -> Self {
        Self::with_parts(
            cfg,
            Arc::new(UreqTransport::new()),
            FetchPolicy::production(),
            Limits::default(),
            Box::new(network_looks_up),
        )
    }

    /// Hub with an injected transport / online check (tests, harnesses). The address policy is
    /// always the production one; only `cfg(test)` code inside this crate can relax it.
    pub fn with_transport(
        cfg: WebConfig,
        transport: Arc<dyn Transport>,
        online: OnlineCheck,
    ) -> Self {
        Self::with_parts(
            cfg,
            transport,
            FetchPolicy::production(),
            Limits::default(),
            online,
        )
    }

    pub(crate) fn with_parts(
        cfg: WebConfig,
        transport: Arc<dyn Transport>,
        policy: FetchPolicy,
        limits: Limits,
        online: OnlineCheck,
    ) -> Self {
        Self {
            cfg,
            transport,
            policy,
            limits,
            online,
            in_flight: AtomicBool::new(false),
            sessions: Mutex::new(VecDeque::new()),
        }
    }

    /// The transport downloads must use too (NB_09): one network stack, one address policy.
    pub fn transport(&self) -> Arc<dyn Transport> {
        Arc::clone(&self.transport)
    }

    /// The address/scheme policy shared with downloads.
    pub fn policy(&self) -> FetchPolicy {
        self.policy.clone()
    }

    pub fn status(&self) -> WebStatus {
        if let Some(r) = &self.cfg.disabled {
            return WebStatus {
                web_capable: false,
                reason: Some(r.clone()),
            };
        }
        if !(self.online)() {
            return WebStatus {
                web_capable: false,
                reason: Some("no network connection".into()),
            };
        }
        WebStatus {
            web_capable: true,
            reason: None,
        }
    }

    fn require_capable(&self) -> Result<(), WebError> {
        if let Some(r) = &self.cfg.disabled {
            return Err(WebError::Disabled(r.clone()));
        }
        if !(self.online)() {
            return Err(WebError::Offline("no network connection".into()));
        }
        Ok(())
    }

    fn validate_key(key: &str) -> Result<(String, NoubinFetchUrl), WebError> {
        if key.len() > MAX_KEY_BYTES {
            return Err(WebError::BadUrl("Noubin key too long".into()));
        }
        let clean = sanitise_key(key.trim())
            .ok_or_else(|| WebError::BadUrl("Noubin key contains control characters".into()))?;
        let u = fetch_url_for_key(&clean).map_err(|e| match e {
            FetchUrlError::NotAUrl => WebError::NotAUrl,
            FetchUrlError::KeyTooLong => WebError::BadUrl("Noubin key too long".into()),
            FetchUrlError::Url(u) => match u {
                plippa_noubin::UrlError::Blocked(_) => WebError::Blocked(u.to_string()),
                other => WebError::BadUrl(other.to_string()),
            },
        })?;
        Ok((clean, u))
    }

    /// Step 1 of the two-step API. **Sends nothing.**
    pub fn prepare(&self, key: &str) -> Result<Prepared, WebError> {
        self.require_capable()?;
        let (clean, u) = Self::validate_key(key)?;
        Ok(Prepared {
            consent_message: format!(
                "No local media found. This will contact {} over the internet to look up this \
                 release. Continue?",
                u.host
            ),
            noubin_key: sanitise_line(&clean, MAX_KEY_BYTES),
            fetch_url: u.display,
            host: u.host,
        })
    }

    /// Step 2. `consent` is the request's `consent` field; anything but `Some(true)` fails
    /// **before** any URL work or socket.
    pub fn fetch(&self, key: &str, consent: Option<bool>) -> Result<WebFetchResult, WebError> {
        let held = self.fetch_held(key, consent)?;
        let overview = ReleaseOverview::from_document(&held.doc, &held.warnings);
        let token = new_token();
        let mut s = self.sessions.lock().map_err(|_| WebError::Busy)?;
        prune(&mut s);
        while s.len() >= MAX_SESSIONS {
            s.pop_front();
        }
        s.push_back((token.clone(), held));
        Ok(WebFetchResult { token, overview })
    }

    /// Fetch one release and hand back the document itself, **without** keeping a session.
    ///
    /// The store tap flow only needs the platform links a Noubin declares, not a held
    /// release for NB_09, so it must not take one of the [`MAX_SESSIONS`] slots. Same
    /// consent gate, same policy, same single-flight guard as [`Self::fetch`].
    pub fn fetch_held(&self, key: &str, consent: Option<bool>) -> Result<HeldRelease, WebError> {
        let consent = Consent::from_request(consent)?;
        self.require_capable()?;
        let (clean, u) = Self::validate_key(key)?;
        if self
            .in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(WebError::Busy);
        }
        let _guard = InFlight(&self.in_flight);
        let fetched = fetch_noudata(
            self.transport.as_ref(),
            consent,
            &self.policy,
            &self.limits,
            &u.wire,
        )?;
        Ok(HeldRelease {
            doc: fetched.doc,
            warnings: fetched.warnings,
            noubin_key: clean,
            source_url: fetched.final_url,
            fetched_at: Instant::now(),
        })
    }


    /// Overview of a held release (portal reload).
    pub fn session_overview(&self, token: &str) -> Result<ReleaseOverview, WebError> {
        let mut s = self.sessions.lock().map_err(|_| WebError::UnknownSession)?;
        prune(&mut s);
        s.iter()
            .find(|(t, _)| t == token)
            .map(|(_, h)| ReleaseOverview::from_document(&h.doc, &h.warnings))
            .ok_or(WebError::UnknownSession)
    }

    /// A held release for NB_09 (does not remove it).
    pub fn held(&self, token: &str) -> Option<HeldRelease> {
        let mut s = self.sessions.lock().ok()?;
        prune(&mut s);
        s.iter().find(|(t, _)| t == token).map(|(_, h)| h.clone())
    }

    /// Forget a held release.
    pub fn discard(&self, token: &str) -> bool {
        let Ok(mut s) = self.sessions.lock() else {
            return false;
        };
        let before = s.len();
        s.retain(|(t, _)| t != token);
        s.len() != before
    }
}

fn prune(s: &mut VecDeque<(String, HeldRelease)>) {
    s.retain(|(_, h)| h.fetched_at.elapsed() < SESSION_TTL);
}

/// 128-bit random token, lower-case hex.
fn new_token() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Body of `POST /api/web/prepare` and `POST /api/web/fetch`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WebRequest {
    #[serde(rename = "noubinKey")]
    pub noubin_key: String,
    /// Must be exactly `true` for `fetch`; ignored by `prepare`.
    #[serde(default)]
    pub consent: Option<bool>,
}

fn local_gate(service: &Arc<Mutex<ServerService>>, key: &str) -> Result<Arc<WebHub>, WebError> {
    // Lock only long enough to read the index; the network never runs under this lock.
    let (hub, local) = {
        let g = service.lock().map_err(|_| WebError::Busy)?;
        (g.web(), g.has_local_match(key))
    };
    if local {
        return Err(WebError::LocalMatch);
    }
    Ok(hub)
}

/// `prepare` behind the local-first rule (§3.2): a key that matches the library never reaches
/// the web code. Sends nothing.
pub fn prepare_unlocked(
    service: &Arc<Mutex<ServerService>>,
    key: &str,
) -> Result<Prepared, WebError> {
    local_gate(service, key)?.prepare(key)
}

/// `fetch` behind the consent gate and the local-first rule. Order: consent, local match,
/// then the hub (capability, URL validation, single flight, network).
pub fn fetch_unlocked(
    service: &Arc<Mutex<ServerService>>,
    key: &str,
    consent: Option<bool>,
) -> Result<WebFetchResult, WebError> {
    Consent::from_request(consent)?;
    local_gate(service, key)?.fetch(key, consent)
}

/// True for a well-formed session token (defence for path parameters).
pub fn is_token(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
