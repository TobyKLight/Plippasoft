//! The portal address a phone can open (BCS_08).
//!
//! The device connects Bandcamp by showing a QR code for the LAN portal, because typing two
//! generated credentials on a 320x240 screen is not realistic. For that QR to be useful the
//! address inside it must be one **another device can reach**: `0.0.0.0` is a bind address,
//! `127.0.0.1` is this machine, and a link-local `169.254.x.x` / `fe80::` address only works
//! on the same wire. [`resolve`] therefore returns every candidate it can find, marks exactly
//! one as `recommended`, and never recommends a host that cannot be reached from a phone.
//!
//! Two rules keep the answer safe:
//!
//! * **An address with credentials or a token is refused.** A URL is shown on a small screen
//!   and encoded into a QR code that any camera can read, so `user:pass@host` and `?token=`
//!   are dropped at the boundary with a reason, never rendered.
//! * **The operator's answer wins.** `--portal-url` / `PLIPPA_PORTAL_URL` is honoured (it is
//!   how a reverse proxy or a DNS name is configured), but an override that is itself a
//!   loopback or link-local address is still only a candidate, because recommending it would
//!   produce a QR code that cannot work.

use std::net::{IpAddr, SocketAddr};

use serde::Serialize;

/// Environment variable that overrides the advertised portal address.
pub const ENV_PORTAL_URL: &str = "PLIPPA_PORTAL_URL";

/// One address the portal may be reached at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortalUrlCandidate {
    /// Scheme, host and port, with no path, query or user information.
    pub url: String,
    /// The address the device's QR code uses.
    pub recommended: bool,
}

/// `GET /api/portal/url` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortalUrls {
    /// The address to advertise, when one exists. Never a loopback or link-local address.
    pub recommended: Option<String>,
    /// Every candidate found, recommended first. May contain an unreachable override so the
    /// portal can explain what was configured.
    pub urls: Vec<PortalUrlCandidate>,
    /// Port of the recommended address (the listen port when nothing is reachable).
    pub port: u16,
    /// `--portal-url` / `PLIPPA_PORTAL_URL` produced the recommended address.
    pub override_used: bool,
    /// Why an address was ignored or why nothing is reachable, in words. No secrets.
    pub note: Option<String>,
}

impl PortalUrls {
    /// The address the device shows, if any.
    pub fn recommended_url(&self) -> Option<&str> {
        self.recommended.as_deref()
    }
}

/// What the process knows about the portal address: where it listens and what was configured.
///
/// Resolution is repeated per request, so a machine that joins Wi-Fi after start-up still
/// advertises a usable address (and the device's QR screen refreshes when it does).
#[derive(Debug, Clone)]
pub struct PortalConfig {
    /// The configured listen address; its port is the default for every candidate.
    pub bind: SocketAddr,
    /// `--portal-url` / `PLIPPA_PORTAL_URL`, unvalidated until a request needs it.
    pub override_url: Option<String>,
}

impl PortalConfig {
    pub fn new(bind: SocketAddr, override_url: Option<String>) -> Self {
        Self { bind, override_url }
    }

    /// Resolve the candidates against this machine's current interfaces, advertising the scheme
    /// the portal actually answers on (SECFIX_03). An operator override keeps the scheme it was
    /// written with (it may point at a proxy).
    ///
    /// There is deliberately no scheme-less variant: a caller that forgot the flag would print
    /// `http://` on the QR code while the portal serves TLS.
    pub fn resolve_with(&self, https: bool) -> PortalUrls {
        resolve_with_scheme(
            self.bind,
            self.override_url.as_deref(),
            &crate::http::local_interface_ips(),
            https,
        )
    }
}

/// Resolve the candidates an operator would accept as "the portal address".
///
/// `bind` is the configured listen address (its port is used whenever an address does not
/// name one), `override_url` is the operator's answer, and `interfaces` are the machine's
/// own addresses ([`crate::http::local_interface_ips`]).
#[cfg(test)]
pub fn resolve(bind: SocketAddr, override_url: Option<&str>, interfaces: &[IpAddr]) -> PortalUrls {
    resolve_with_scheme(bind, override_url, interfaces, false)
}

/// [`resolve`] for the scheme this portal serves: `https` when TLS is on (SECFIX_03).
pub fn resolve_with_scheme(
    bind: SocketAddr,
    override_url: Option<&str>,
    interfaces: &[IpAddr],
    https: bool,
) -> PortalUrls {
    let default_port = bind.port();
    let mut note: Option<String> = None;
    let mut candidates: Vec<String> = Vec::new();
    let mut override_used = false;

    if let Some(raw) = override_url.map(str::trim).filter(|raw| !raw.is_empty()) {
        match normalise_base(raw, default_port) {
            Ok(url) => {
                override_used = true;
                if !is_reachable_from_another_device(&url) {
                    note = Some(format!(
                        "the configured portal address {url} is not reachable from another \
                         device; set --portal-url to this machine's LAN address"
                    ));
                }
                candidates.push(url);
            }
            Err(reason) => {
                note = Some(format!("the configured portal address was ignored: {reason}"));
            }
        }
    }

    let scheme = if https { "https" } else { "http" };
    // The listen address itself is a candidate when it names one interface.
    if !bind.ip().is_unspecified() {
        if let Some(url) = url_for_ip(bind.ip(), default_port, scheme) {
            candidates.push(url);
        }
    }
    // Every interface address, best first (private IPv4, other IPv4, IPv6).
    let mut ranked: Vec<(u8, String)> = interfaces
        .iter()
        .filter_map(|ip| {
            let url = url_for_ip(*ip, default_port, scheme)?;
            Some((reachability_rank(ip), url))
        })
        .collect();
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    for (_, url) in ranked {
        candidates.push(url);
    }

    let mut seen: Vec<String> = Vec::new();
    let mut unique: Vec<String> = Vec::new();
    for url in candidates {
        if !seen.contains(&url) {
            seen.push(url.clone());
            unique.push(url);
        }
    }

    let recommended = unique
        .iter()
        .find(|url| is_reachable_from_another_device(url))
        .cloned();
    if recommended.is_none() && note.is_none() {
        note = Some(
            "no address another device can reach yet; connect this Plippa to Wi-Fi or set \
             --portal-url"
                .to_string(),
        );
    }

    let urls: Vec<PortalUrlCandidate> = unique
        .into_iter()
        .map(|url| PortalUrlCandidate {
            recommended: recommended.as_deref() == Some(url.as_str()),
            url,
        })
        .collect();
    // Recommended first so the portal and the device read the same address first.
    let mut urls = urls;
    urls.sort_by_key(|candidate| !candidate.recommended);
    let port = recommended
        .as_deref()
        .and_then(port_of)
        .unwrap_or(default_port);

    PortalUrls {
        recommended,
        urls,
        port,
        override_used,
        note,
    }
}

/// Parse and normalise one address (`host`, `host:port`, `http://host:port/`).
///
/// Refuses anything a QR code must not carry: user information, a query, a fragment, control
/// characters, a scheme other than `http`/`https`, and an empty or malformed host. The result
/// is `scheme://host:port` with the port always present and no trailing slash.
pub fn normalise_base(raw: &str, default_port: u16) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("the address is empty".to_string());
    }
    if raw
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\u{202E}' || c == '\u{200B}')
    {
        return Err("the address contains whitespace or invisible characters".to_string());
    }
    if raw.contains('@') {
        return Err("an address that carries a user name or secret is never shown or scanned".into());
    }
    if raw.contains('?') || raw.contains('#') {
        return Err("an address with a query or fragment is never shown or scanned".into());
    }
    let (scheme, rest) = match raw.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => ("http".to_string(), raw),
    };
    if scheme != "http" && scheme != "https" {
        return Err("the scheme is not http or https".to_string());
    }
    let rest = rest.trim_end_matches('/');
    let (host, port) = split_host_port(rest, default_port)?;
    let host = host.trim();
    if host.is_empty() {
        return Err("the address names no host".to_string());
    }
    if !valid_host(host) {
        return Err("the address does not name a host this portal can serve".to_string());
    }
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Ok(format!("{scheme}://{host}:{port}"))
}

/// Split `host`, `host:port`, `[v6]` or `[v6]:port` into the host and a port.
fn split_host_port(rest: &str, default_port: u16) -> Result<(String, u16), String> {
    if let Some(stripped) = rest.strip_prefix('[') {
        let (host, tail) = stripped
            .split_once(']')
            .ok_or_else(|| "the address has an unclosed IPv6 bracket".to_string())?;
        let port = match tail {
            "" => default_port,
            tail => match tail.strip_prefix(':') {
                Some(port) => parse_port(port)?,
                None => return Err("unexpected text after the IPv6 address".to_string()),
            },
        };
        return Ok((host.to_string(), port));
    }
    if let Ok(ip) = rest.parse::<IpAddr>() {
        // A bare IPv6 literal (`::1`, `2001:db8::1`) has colons but no port.
        return Ok((ip.to_string(), default_port));
    }
    match rest.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) && !port.is_empty() => {
            Ok((host.to_string(), parse_port(port)?))
        }
        _ => Ok((rest.to_string(), default_port)),
    }
}

fn parse_port(text: &str) -> Result<u16, String> {
    match text.parse::<u16>() {
        Ok(0) | Err(_) => Err("the port is not a number between 1 and 65535".to_string()),
        Ok(port) => Ok(port),
    }
}

/// `true` for an IPv4/IPv6 literal or a plain DNS-style host name.
fn valid_host(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    if host.len() > 253 || host.starts_with('-') || host.ends_with('-') {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// `true` when a phone on the LAN can plausibly open this address.
pub fn is_reachable_from_another_device(url: &str) -> bool {
    let Some((_, host)) = url.split_once("://") else {
        return false;
    };
    let Some((host, _)) = split_host_port(host.trim_end_matches('/'), 0).ok() else {
        return false;
    };
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if host == "localhost" {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            !v4.is_unspecified()
                && !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_multicast()
        }
        Ok(IpAddr::V6(v6)) => {
            !v6.is_unspecified()
                && !v6.is_loopback()
                && !v6.is_unicast_link_local()
                && !v6.is_multicast()
        }
        // A DNS name is reachable as far as this machine can tell.
        Err(_) => !host.is_empty(),
    }
}

/// Preference for the interface list: private IPv4 first, then other IPv4, then IPv6.
fn reachability_rank(ip: &IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v4) if v4.is_private() => 0,
        IpAddr::V4(v4) if v4.is_loopback() || v4.is_link_local() => 3,
        IpAddr::V4(_) => 1,
        IpAddr::V6(v6) if v6.is_loopback() || v6.is_unicast_link_local() => 3,
        IpAddr::V6(_) => 2,
    }
}

/// `scheme://host:port` for one interface address, or `None` when it cannot be written as a URL.
fn url_for_ip(ip: IpAddr, port: u16, scheme: &str) -> Option<String> {
    let host = match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    normalise_base(&format!("{scheme}://{host}:{port}"), port).ok()
}

/// Port of an already-normalised URL.
fn port_of(url: &str) -> Option<u16> {
    let (_, host) = url.split_once("://")?;
    let (_, port) = split_host_port(host.trim_end_matches('/'), 0).ok()?;
    Some(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn bind(port: u16) -> SocketAddr {
        SocketAddr::from(([0, 0, 0, 0], port))
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    /// The machine's usual set: loopback, one wired LAN address, one Wi-Fi address.
    fn interfaces() -> Vec<IpAddr> {
        vec![
            v4(127, 0, 0, 1),
            v4(169, 254, 10, 7),
            v4(192, 168, 1, 24),
            v4(10, 0, 0, 5),
            "fe80::1".parse().expect("v6"),
        ]
    }

    #[test]
    fn portal_url_never_advertises_loopback_as_recommended() {
        // A server bound to the wildcard: the recommendation is a LAN address, not 0.0.0.0.
        let urls = resolve(bind(8787), None, &interfaces());
        let recommended = urls.recommended.as_deref().expect("a LAN address exists");
        assert!(recommended.starts_with("http://10.0.0.5:8787"), "{recommended}");
        assert!(!recommended.contains("0.0.0.0"));
        assert!(!recommended.contains("127.0.0.1"));
        assert!(!recommended.contains("169.254"));
        assert!(!recommended.contains("fe80"));
        // Every candidate is listed, each carries the flag, and exactly one is recommended.
        assert!(urls.urls.len() >= 3, "{:?}", urls.urls);
        assert_eq!(urls.urls.iter().filter(|c| c.recommended).count(), 1);
        assert!(urls.urls[0].recommended, "the recommended address is first");
        assert!(urls.urls.iter().any(|c| c.url.contains("127.0.0.1")),
            "loopback is still listed as a candidate");

        // Bound to loopback only: there is nothing to advertise, and saying so is the answer.
        let local = SocketAddr::from(([127, 0, 0, 1], 8787));
        let urls = resolve(local, None, &[v4(127, 0, 0, 1)]);
        assert_eq!(urls.recommended, None);
        assert!(urls.note.as_deref().is_some_and(|n| n.contains("--portal-url")));

        // An override that is loopback is listed but never recommended.
        let urls = resolve(bind(8787), Some("http://127.0.0.1:9999"), &interfaces());
        assert!(urls.override_used);
        assert_eq!(
            urls.recommended.as_deref(),
            Some("http://10.0.0.5:8787"),
            "a loopback override cannot become the QR code"
        );
        let override_entry = urls
            .urls
            .iter()
            .find(|c| c.url.contains("9999"))
            .expect("the configured address is still reported");
        assert!(!override_entry.recommended);
        assert!(urls.note.as_deref().is_some_and(|n| n.contains("not reachable")));
    }

    #[test]
    fn portal_url_honours_the_override() {
        // The operator's LAN address (a reverse proxy on another port) is the recommendation.
        let urls = resolve(
            bind(8787),
            Some("  http://192.168.1.50:9000/  "),
            &interfaces(),
        );
        assert!(urls.override_used);
        assert_eq!(urls.recommended.as_deref(), Some("http://192.168.1.50:9000"));
        assert_eq!(urls.port, 9000, "the URL always carries its own port");
        assert_eq!(urls.urls[0].url, "http://192.168.1.50:9000");

        // A bare host takes the listen port; a DNS name is accepted as written.
        let urls = resolve(bind(8420), Some("plippa.local"), &interfaces());
        assert_eq!(urls.recommended.as_deref(), Some("http://plippa.local:8420"));
        // A scheme other than http(s) is refused, with a reason and no panic.
        let urls = resolve(bind(8787), Some("ftp://192.168.1.50/"), &interfaces());
        assert!(!urls.override_used);
        assert_eq!(urls.recommended.as_deref(), Some("http://10.0.0.5:8787"));
        assert!(urls.note.as_deref().is_some_and(|n| n.contains("scheme")));
        // A port of 0 or a non-numeric port is refused rather than advertised.
        for bad in ["http://192.168.1.50:0", "http://192.168.1.50:http"] {
            let urls = resolve(bind(8787), Some(bad), &interfaces());
            assert!(!urls.override_used, "{bad}");
        }
        // Bound to a specific LAN address with no override: that address is the answer.
        let specific = SocketAddr::from(([10, 0, 0, 5], 8787));
        let urls = resolve(specific, None, &interfaces());
        assert_eq!(urls.recommended.as_deref(), Some("http://10.0.0.5:8787"));
    }

    #[test]
    fn portal_url_contains_no_secrets() {
        // Anything a QR code or a screen must never carry is stripped at the boundary.
        for hostile in [
            "http://fan:s3cret@192.168.1.50:8787",
            "http://192.168.1.50:8787/?token=abc123",
            "http://192.168.1.50:8787/#token=abc123",
            "http://192.168.1.50:8787/\u{202E}",
            "http://192.168.1.50:8787/ /",
            "javascript:alert(1)",
        ] {
            let urls = resolve(bind(8787), Some(hostile), &interfaces());
            assert!(!urls.override_used, "{hostile:?} must not be used");
            let rendered = serde_json::to_string(&urls).expect("serialise");
            for secret in ["s3cret", "abc123", "fan:", "javascript", "token"] {
                assert!(
                    !rendered.contains(secret),
                    "{hostile:?} leaked {secret:?}: {rendered}"
                );
            }
        }
        // A normal answer carries a port and nothing else: no path, query, user or token.
        let urls = resolve(bind(8787), None, &interfaces());
        for candidate in &urls.urls {
            assert!(candidate.url.starts_with("http://"), "{candidate:?}");
            assert!(!candidate.url.contains('@'), "{candidate:?}");
            assert!(!candidate.url.contains('?'), "{candidate:?}");
            assert!(!candidate.url.contains('#'), "{candidate:?}");
            assert!(port_of(&candidate.url).is_some(), "{candidate:?}");
        }
    }

    #[test]
    fn portal_url_normalises_what_it_accepts() {
        assert_eq!(
            normalise_base("http://192.168.1.9:8787/", 8787).as_deref(),
            Ok("http://192.168.1.9:8787")
        );
        assert_eq!(
            normalise_base("192.168.1.9", 8787).as_deref(),
            Ok("http://192.168.1.9:8787")
        );
        assert_eq!(
            normalise_base("https://plippa.lan:443", 8787).as_deref(),
            Ok("https://plippa.lan:443")
        );
        assert_eq!(
            normalise_base("[fe80::1]:8787", 8787).as_deref(),
            Ok("http://[fe80::1]:8787")
        );
        assert!(normalise_base("", 8787).is_err());
        assert!(normalise_base("http://", 8787).is_err());
        assert!(is_reachable_from_another_device("http://192.168.1.9:8787"));
        assert!(!is_reachable_from_another_device("http://0.0.0.0:8787"));
        assert!(!is_reachable_from_another_device("http://127.0.0.1:8787"));
        assert!(!is_reachable_from_another_device("http://localhost:8787"));
        assert!(!is_reachable_from_another_device("http://[::1]:8787"));
    }
}
