//! Which hosts may receive an authenticated request, and where a redirect may go.
//!
//! This is the store layer's own guard, not the SSRF layer. It answers one question:
//! *is this destination one the operator configured for this store?* The transport
//! implementor must still vet every resolved address, re-check each redirect hop and
//! enforce its own byte and time limits.
//!
//! Default deny: an empty allowlist rejects everything. Tag data, a provider
//! response or a portal request may choose **which provider** to ask, never **where**
//! the request goes.

use crate::error::StoreError;

/// Longest URL inspected (bytes); anything longer is refused before parsing.
pub const MAX_URL_BYTES: usize = 4096;

/// Hosts a store may send credentials to, plus hosts a redirect from one of those
/// may legitimately land on (a CDN).
///
/// The default is the Bandcamp profile from the research doc: authenticated requests
/// go to `bandcamp.com` and subdomains, and the `stream` redirect is allowed to reach
/// `bcbits.com` and subdomains (`t4.bcbits.com` today). The CDN host is never
/// authenticated: it must not receive Subsonic parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedHosts {
    authenticated: Vec<String>,
    cdn: Vec<String>,
}

impl Default for AuthenticatedHosts {
    fn default() -> Self {
        Self {
            authenticated: vec!["bandcamp.com".to_string()],
            cdn: vec!["bcbits.com".to_string()],
        }
    }
}

impl AuthenticatedHosts {
    /// Build an allowlist from host names.
    ///
    /// Entries are lowercased and validated as DNS names; an empty or malformed
    /// entry is a configuration error, not something to skip silently. An empty
    /// authenticated list denies everything.
    pub fn new(authenticated: &[&str], cdn: &[&str]) -> Result<Self, StoreError> {
        Ok(Self {
            authenticated: normalise_hosts(authenticated)?,
            cdn: normalise_hosts(cdn)?,
        })
    }

    /// The Bandcamp profile: `bandcamp.com` authenticated, `bcbits.com` for redirects.
    pub fn bandcamp() -> Self {
        Self::default()
    }

    /// Hosts that may receive authenticated requests.
    pub fn authenticated_hosts(&self) -> &[String] {
        &self.authenticated
    }

    /// Hosts a redirect may land on, in addition to [`Self::authenticated_hosts`].
    pub fn cdn_hosts(&self) -> &[String] {
        &self.cdn
    }

    /// `true` when `url` is https, has no userinfo, uses port 443 and its host is an
    /// authenticated host or a subdomain of one.
    pub fn authenticated(&self, url: &str) -> bool {
        self.check_against(url, &self.authenticated).is_ok()
    }

    /// `true` when `url` may be followed as a redirect: an authenticated host or a
    /// CDN host, under the same structural rules as [`Self::authenticated`].
    pub fn redirect_allowed(&self, url: &str) -> bool {
        self.check_against(url, &self.authenticated)
            .or_else(|_| self.check_against(url, &self.cdn))
            .is_ok()
    }

    /// [`Self::authenticated`] with the reason on failure.
    pub fn check_authenticated(&self, url: &str) -> Result<(), StoreError> {
        self.check_against(url, &self.authenticated)
    }

    /// [`Self::redirect_allowed`] with the reason on failure.
    pub fn check_redirect(&self, url: &str) -> Result<(), StoreError> {
        self.check_against(url, &self.authenticated)
            .or_else(|_| self.check_against(url, &self.cdn))
    }

    fn check_against(&self, url: &str, hosts: &[String]) -> Result<(), StoreError> {
        let target = parse_https_target(url)?;
        if hosts.iter().any(|host| target.is_or_under(host)) {
            Ok(())
        } else {
            Err(StoreError::policy_url(url, "host is not on the allowlist"))
        }
    }
}

/// [`AuthenticatedHosts::check_authenticated`] as a free function, for call sites
/// that do not own the allowlist.
pub fn check_authenticated_target(
    url: &str,
    allowlist: &AuthenticatedHosts,
) -> Result<(), StoreError> {
    allowlist.check_authenticated(url)
}

/// A structurally valid https target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HttpsTarget<'a> {
    host: &'a str,
    port: u16,
}

impl HttpsTarget<'_> {
    /// Exact host match or subdomain match, case-insensitively; never a suffix
    /// match on a bare string (`evilbandcamp.com` must not pass).
    fn is_or_under(&self, allowed: &str) -> bool {
        if self.host.eq_ignore_ascii_case(allowed) {
            return true;
        }
        if self.host.len() <= allowed.len() {
            return false;
        }
        let suffix = &self.host[self.host.len() - allowed.len()..];
        self.host.as_bytes()[self.host.len() - allowed.len() - 1] == b'.'
            && suffix.eq_ignore_ascii_case(allowed)
    }
}

fn parse_https_target(url: &str) -> Result<HttpsTarget<'_>, StoreError> {
    if url.len() > MAX_URL_BYTES {
        return Err(StoreError::policy("url is too long"));
    }
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(StoreError::policy("url contains forbidden characters"));
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(StoreError::policy("url has no scheme"));
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(StoreError::policy("only https urls are accepted"));
    }

    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return Err(StoreError::policy("url has no host"));
    }
    if authority.contains('@') {
        return Err(StoreError::policy(
            "url must not contain a user name or password",
        ));
    }
    if authority.starts_with('[') {
        return Err(StoreError::policy(
            "ip literals are not accepted as store hosts",
        ));
    }

    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.contains(':') {
                return Err(StoreError::policy("invalid host"));
            }
            let port: u16 = port
                .parse()
                .map_err(|_| StoreError::policy("invalid port"))?;
            (host, port)
        }
        None => (authority, 443),
    };
    if port != 443 {
        return Err(StoreError::policy("only port 443 is allowed"));
    }

    let host_ok = !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.ends_with('.')
        && !host.contains("..")
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'));
    if !host_ok {
        return Err(StoreError::policy("invalid host name"));
    }

    Ok(HttpsTarget { host, port })
}

fn normalise_hosts(hosts: &[&str]) -> Result<Vec<String>, StoreError> {
    hosts
        .iter()
        .map(|host| {
            let host = host.trim().to_ascii_lowercase();
            let ok = !host.is_empty()
                && host.len() <= 253
                && !host.starts_with('.')
                && !host.ends_with('.')
                && !host.contains("..")
                && host
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'));
            if ok {
                Ok(host)
            } else {
                Err(StoreError::policy(format!(
                    "invalid allowlist host: {host:?}"
                )))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow() -> AuthenticatedHosts {
        AuthenticatedHosts::default()
    }

    #[test]
    fn default_carries_bandcamp_and_the_cdn() {
        assert_eq!(allow().authenticated_hosts(), ["bandcamp.com"]);
        assert_eq!(allow().cdn_hosts(), ["bcbits.com"]);
        assert_eq!(AuthenticatedHosts::bandcamp(), allow());
    }

    #[test]
    fn authenticated_accepts_the_store_and_its_subdomains() {
        for url in [
            "https://bandcamp.com",
            "https://bandcamp.com/",
            "https://bandcamp.com:443/api/subsonic/rest/ping.view",
            "https://sub.bandcamp.com/api/subsonic/rest/ping.view?u=U&t=T&s=S",
            "https://a.b.bandcamp.com/x",
        ] {
            assert!(allow().authenticated(url), "should allow {url}");
            assert!(allow().check_authenticated(url).is_ok(), "should allow {url}");
        }
    }

    #[test]
    fn authenticated_rejects_off_allowlist_and_malformed_targets() {
        for url in [
            "https://evil.com",
            "https://bandcamp.com.evil.com",
            "https://evilbandcamp.com",
            "https://evil.com/bandcamp.com",
            "http://bandcamp.com",
            "https://u:p@bandcamp.com",
            "https://u@bandcamp.com",
            "https://bandcamp.com:8443",
            "https://bandcamp.com:0",
            "https://[::1]/x",
            "https://127.0.0.1/x",
            "https:///x",
            "https://",
            "bandcamp.com",
            "//bandcamp.com/x",
            "https://bandcamp.com\\@evil.com",
            "https://bandcamp.com\t/x",
            "https://bcbits.com",
            "https://t4.bcbits.com/stream",
        ] {
            assert!(!allow().authenticated(url), "should reject {url}");
            let err = allow().check_authenticated(url).expect_err("must reject");
            assert_eq!(err.code(), "policy", "{url}");
        }
    }

    #[test]
    fn redirects_may_reach_the_cdn_but_authenticated_requests_may_not() {
        assert!(allow().redirect_allowed("https://t4.bcbits.com/stream/1?token=T"));
        assert!(allow().redirect_allowed("https://bcbits.com/x"));
        assert!(allow().redirect_allowed("https://bandcamp.com/x"));
        assert!(!allow().authenticated("https://t4.bcbits.com/stream/1?token=T"));

        for url in [
            "http://t4.bcbits.com/stream",
            "https://bcbits.com.evil.com/x",
            "https://evil.com/t4.bcbits.com",
            "https://f4.bcbits.com:8443/x",
        ] {
            assert!(!allow().redirect_allowed(url), "should reject {url}");
        }
    }

    #[test]
    fn empty_allowlist_denies_everything_and_malformed_entries_are_errors() {
        let empty = AuthenticatedHosts::new(&[], &[]).unwrap();
        assert!(!empty.authenticated("https://bandcamp.com/x"));
        assert!(!empty.redirect_allowed("https://bcbits.com/x"));

        for bad in ["", " ", "https://bandcamp.com", "band camp", ".bandcamp.com", "a..b"] {
            assert!(
                AuthenticatedHosts::new(&[bad], &[]).is_err(),
                "accepted {bad:?}"
            );
        }
        assert!(AuthenticatedHosts::new(&["BandCamp.com"], &[]).is_ok());
        let mixed_case = AuthenticatedHosts::new(&["BandCamp.com"], &[]).unwrap();
        assert!(mixed_case.authenticated("https://BANDCAMP.com/x"));
        assert!(mixed_case.authenticated("https://Sub.BandCamp.COM/x"));
    }

    #[test]
    fn free_function_matches_the_method() {
        let hosts = allow();
        assert!(check_authenticated_target("https://bandcamp.com/x", &hosts).is_ok());
        assert!(check_authenticated_target("https://evil.com/x", &hosts).is_err());
    }
}
