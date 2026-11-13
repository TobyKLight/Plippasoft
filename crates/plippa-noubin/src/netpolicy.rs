//! Network target policy and strict URL parsing (`NBSecurityRequirements.md` §1.1–§1.2, §3 URLs).
//!
//! Pure functions only (no sockets, no DNS): the SSRF block-list ([`is_blocked_ip`],
//! [`is_blocked_hostname`]), a strict host parser that refuses every non-canonical IP
//! spelling (decimal / hex / octal integers, zone ids, userinfo tricks), and the URL sanitiser
//! for URLs that are stored or shown ([`sanitise_url`]).
//!
//! The network layer (`plippa-server::web`) calls [`is_blocked_ip`] on every address the
//! resolver returns, at connect time, and [`parse_fetch_url`] on the first URL and on every
//! redirect `Location`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// Longest URL accepted anywhere (bytes).
pub const MAX_URL_BYTES: usize = 2048;

/// Why a fetch target was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    Empty,
    TooLong,
    /// Scheme is neither `http` (upgraded) nor `https`.
    BadScheme,
    /// Control characters, whitespace, bidi/zero-width characters or `\` in the URL.
    BadCharacters,
    /// `user:pass@` in the authority.
    Userinfo,
    BadHost,
    BadPort,
    /// A fetch target must use port 443 (or none).
    PortNotAllowed,
    /// Host resolves (syntactically) to a private / loopback / link-local / reserved target.
    Blocked(BlockReason),
    /// Redirect `Location` that is not absolute, root-relative or protocol-relative.
    UnsupportedRelative,
}

/// Category of a blocked target (for error messages and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    Loopback,
    Private,
    LinkLocal,
    CarrierGradeNat,
    Multicast,
    Reserved,
    Unspecified,
    LocalName,
}

impl std::fmt::Display for UrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            UrlError::Empty => "empty URL",
            UrlError::TooLong => "URL too long",
            UrlError::BadScheme => "only https URLs are accepted",
            UrlError::BadCharacters => "URL contains forbidden characters",
            UrlError::Userinfo => "URL must not contain a user name or password",
            UrlError::BadHost => "invalid host name",
            UrlError::BadPort => "invalid port",
            UrlError::PortNotAllowed => "only port 443 is allowed",
            UrlError::Blocked(_) => "host is on a private or reserved network",
            UrlError::UnsupportedRelative => "unsupported relative redirect",
        };
        f.write_str(s)
    }
}

impl std::error::Error for UrlError {}

// ---------------------------------------------------------------------------
// Block-list
// ---------------------------------------------------------------------------

fn block_v4(ip: Ipv4Addr) -> Option<BlockReason> {
    let o = ip.octets();
    Some(match o {
        [0, ..] => BlockReason::Unspecified,
        [10, ..] => BlockReason::Private,
        [100, b, ..] if (64..=127).contains(&b) => BlockReason::CarrierGradeNat,
        [127, ..] => BlockReason::Loopback,
        [169, 254, ..] => BlockReason::LinkLocal,
        [172, b, ..] if (16..=31).contains(&b) => BlockReason::Private,
        [192, 0, 0, _] => BlockReason::Reserved,
        [192, 0, 2, _] => BlockReason::Reserved,
        [192, 88, 99, _] => BlockReason::Reserved,
        [192, 168, ..] => BlockReason::Private,
        [198, b, ..] if b == 18 || b == 19 => BlockReason::Reserved,
        [198, 51, 100, _] => BlockReason::Reserved,
        [203, 0, 113, _] => BlockReason::Reserved,
        [a, ..] if (224..=239).contains(&a) => BlockReason::Multicast,
        [a, ..] if a >= 240 => BlockReason::Reserved,
        _ => return None,
    })
}

fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let v4 = |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    // ::ffff:a.b.c.d (mapped) and ::a.b.c.d (deprecated compatible)
    if s[..5] == [0, 0, 0, 0, 0] && (s[5] == 0xffff || s[5] == 0) {
        return Some(v4(s[6], s[7]));
    }
    // ::ffff:0:a.b.c.d (SIIT / IPv4-translated, RFC 2765)
    if s[..4] == [0, 0, 0, 0] && s[4] == 0xffff && s[5] == 0 {
        return Some(v4(s[6], s[7]));
    }
    // 64:ff9b::/96 NAT64
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return Some(v4(s[6], s[7]));
    }
    // 2002::/16 6to4
    if s[0] == 0x2002 {
        return Some(v4(s[1], s[2]));
    }
    None
}

fn block_v6(ip: Ipv6Addr) -> Option<BlockReason> {
    if ip.is_unspecified() {
        return Some(BlockReason::Unspecified);
    }
    if ip.is_loopback() {
        return Some(BlockReason::Loopback);
    }
    if let Some(v4) = embedded_v4(ip) {
        if let Some(r) = block_v4(v4) {
            return Some(r);
        }
        // `::` and `::1` were handled above; any other ::/96 form is never legitimate.
        if ip.segments()[..6] == [0; 6] {
            return Some(BlockReason::Reserved);
        }
        return None;
    }
    let s = ip.segments();
    // 64:ff9b:1::/48 is the local-use NAT64 prefix (RFC 8215): private by definition.
    if s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1 {
        return Some(BlockReason::Private);
    }
    if s[0] & 0xfe00 == 0xfc00 {
        return Some(BlockReason::Private); // fc00::/7 unique local
    }
    if s[0] & 0xffc0 == 0xfe80 {
        return Some(BlockReason::LinkLocal);
    }
    if s[0] & 0xffc0 == 0xfec0 {
        return Some(BlockReason::Private); // fec0::/10 site-local (deprecated)
    }
    if s[0] & 0xff00 == 0xff00 {
        return Some(BlockReason::Multicast);
    }
    if s[0] == 0x2001 && (s[1] == 0 || s[1] == 0x0db8) {
        return Some(BlockReason::Reserved); // Teredo, documentation
    }
    if s[0] == 0x0100 && s[1..4] == [0, 0, 0] {
        return Some(BlockReason::Reserved); // discard prefix 100::/64
    }
    // Fail closed: global unicast is 2000::/3; every other prefix is reserved or special.
    if s[0] & 0xe000 != 0x2000 {
        return Some(BlockReason::Reserved);
    }
    None
}

/// Reason `ip` must never be connected to, or `None` if it is a public unicast address.
pub fn block_reason_ip(ip: IpAddr) -> Option<BlockReason> {
    match ip {
        IpAddr::V4(v4) => block_v4(v4),
        IpAddr::V6(v6) => block_v6(v6),
    }
}

/// True for loopback, RFC1918, link-local (incl. `169.254.0.0/16` cloud metadata), CGNAT
/// `100.64.0.0/10`, unique-local / link-local IPv6, multicast, documentation and other
/// non-global ranges; IPv4-in-IPv6 forms are judged by the embedded IPv4 address.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    block_reason_ip(ip).is_some()
}

/// True for names that are local by convention: `localhost`, `*.localhost`, `*.local`,
/// `*.localdomain`, `*.internal`, `*.home.arpa`, `*.lan`, and single-label names.
pub fn is_blocked_hostname(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if !h.contains('.') {
        return true;
    }
    h == "localhost"
        || [
            ".localhost",
            ".local",
            ".localdomain",
            ".internal",
            ".home.arpa",
            ".lan",
            ".intranet",
            ".corp",
            ".home",
        ]
        .iter()
        .any(|suffix| h.ends_with(suffix))
}

// ---------------------------------------------------------------------------
// Host / URL parsing
// ---------------------------------------------------------------------------

/// A syntactically valid host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    /// Lower-case ASCII DNS name (at least two labels, alphabetic TLD).
    Name(String),
    Ip(IpAddr),
}

impl Host {
    /// Text form usable in a URL authority (IPv6 in brackets).
    pub fn authority_text(&self) -> String {
        match self {
            Host::Name(n) => n.clone(),
            Host::Ip(IpAddr::V4(v)) => v.to_string(),
            Host::Ip(IpAddr::V6(v)) => format!("[{v}]"),
        }
    }
}

/// Strict host parser. Rejects empty, non-ASCII (no IDN: punycode `xn--` labels are fine),
/// anything that is not a canonical dotted-quad / bracketed IPv6 / DNS name, and names whose
/// last label does not start with a letter (so `2130706433`, `0x7f.1`, `127.1` are refused).
pub fn parse_host(raw: &str) -> Result<Host, UrlError> {
    if raw.is_empty() || raw.len() > 253 + 2 {
        return Err(UrlError::BadHost);
    }
    if let Some(inner) = raw.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or(UrlError::BadHost)?;
        if inner.contains('%') {
            return Err(UrlError::BadHost); // zone id
        }
        return Ipv6Addr::from_str(inner)
            .map(|v| Host::Ip(IpAddr::V6(v)))
            .map_err(|_| UrlError::BadHost);
    }
    if !raw.is_ascii() {
        return Err(UrlError::BadHost);
    }
    let name = raw.trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() {
        return Err(UrlError::BadHost);
    }
    if name.chars().all(|c| c.is_ascii_digit() || c == '.') {
        // Only the canonical dotted quad is an IP literal; everything else numeric is refused.
        return Ipv4Addr::from_str(&name)
            .map(|v| Host::Ip(IpAddr::V4(v)))
            .map_err(|_| UrlError::BadHost);
    }
    let labels: Vec<&str> = name.split('.').collect();
    if labels.len() < 2 {
        return Ok(Host::Name(name)); // single label: caller's block-list refuses it
    }
    for l in &labels {
        let ok = !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !ok {
            return Err(UrlError::BadHost);
        }
    }
    let tld = labels[labels.len() - 1];
    if !tld.as_bytes()[0].is_ascii_alphabetic() {
        return Err(UrlError::BadHost);
    }
    Ok(Host::Name(name))
}

/// `Ok` if `host` may be connected to (syntactic checks only; addresses are re-checked at
/// connect time by the caller with [`is_blocked_ip`]).
pub fn check_host_allowed(host: &Host) -> Result<(), UrlError> {
    match host {
        Host::Name(n) if is_blocked_hostname(n) => Err(UrlError::Blocked(BlockReason::LocalName)),
        Host::Name(_) => Ok(()),
        Host::Ip(ip) => match block_reason_ip(*ip) {
            Some(r) => Err(UrlError::Blocked(r)),
            None => Ok(()),
        },
    }
}

fn has_forbidden_chars(s: &str) -> bool {
    s.chars().any(|c| {
        c.is_control()
            || c.is_whitespace()
            || c == '\\'
            || matches!(
                c,
                '\u{200B}'..='\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2060}'..='\u{2064}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{061C}'
                    | '\u{FEFF}'
            )
    })
}

/// A parsed `https` URL. `path_and_query` is already percent-encoded for the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    pub host: Host,
    /// `None` = default (443).
    pub port: Option<u16>,
    /// Starts with `/`; includes `?query` (no fragment).
    pub path_and_query: String,
    pub fragment: Option<String>,
}

impl ParsedUrl {
    /// Canonical `https://host[:port]/path?query` (fragment omitted).
    pub fn to_fetch_string(&self) -> String {
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        format!(
            "https://{}{}{}",
            self.host.authority_text(),
            port,
            self.path_and_query
        )
    }

    /// Canonical string including the fragment (for stored/displayed links).
    pub fn to_link_string(&self) -> String {
        let mut s = self.to_fetch_string();
        if let Some(f) = &self.fragment {
            s.push('#');
            s.push_str(f);
        }
        s
    }
}

fn is_url_safe_byte(b: u8, in_query: bool) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
                | b'/'
        )
        || (in_query && b == b'?')
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

/// Percent-encode everything that is not URL-safe, keeping valid existing `%XX` sequences.
/// A lone `%` becomes `%25`. `keep_slash_escape` is irrelevant here: `%2f` is a valid
/// sequence and is kept as written.
pub fn encode_url_component(s: &str, in_query: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 2 < bytes.len() && is_hex(bytes[i + 1]) && is_hex(bytes[i + 2]) {
            out.push('%');
            out.push(bytes[i + 1] as char);
            out.push(bytes[i + 2] as char);
            i += 3;
            continue;
        }
        if is_url_safe_byte(b, in_query) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
        i += 1;
    }
    out
}

/// Parse `raw` as a fetchable/linkable `https` URL (`http` is upgraded).
///
/// * total length <= [`MAX_URL_BYTES`]; no control/whitespace/bidi/backslash characters;
/// * no userinfo; strict host ([`parse_host`]); optional numeric port;
/// * path/query/fragment are re-encoded so the result is plain ASCII with no unsafe bytes.
///
/// The block-list is **not** applied (links are only displayed); use [`parse_fetch_url`].
pub fn parse_https_url(raw: &str) -> Result<ParsedUrl, UrlError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(UrlError::Empty);
    }
    if s.len() > MAX_URL_BYTES {
        return Err(UrlError::TooLong);
    }
    if has_forbidden_chars(s) {
        return Err(UrlError::BadCharacters);
    }
    let (upgraded, rest) = if let Some(r) = strip_prefix_ci(s, "https://") {
        (false, r)
    } else if let Some(r) = strip_prefix_ci(s, "http://") {
        (true, r)
    } else {
        return Err(UrlError::BadScheme);
    };
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    if authority.contains('@') {
        return Err(UrlError::Userinfo);
    }
    let (host_text, port) = split_host_port(authority)?;
    let host = parse_host(host_text)?;
    // The default port of the *original* scheme is dropped; anything else is kept and later
    // refused by fetch callers (`parse_fetch_url`).
    let default_port = if upgraded { 80 } else { 443 };
    let port = port.filter(|p| *p != default_port);

    let (before_frag, fragment) = match tail.split_once('#') {
        Some((a, f)) => (a, Some(encode_url_component(f, true))),
        None => (tail, None),
    };
    let (path, query) = match before_frag.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (before_frag, None),
    };
    let mut path_and_query = if path.is_empty() {
        "/".to_string()
    } else {
        encode_url_component(path, false)
    };
    if let Some(q) = query.filter(|q| !q.is_empty()) {
        path_and_query.push('?');
        path_and_query.push_str(&encode_url_component(q, true));
    }
    let out = ParsedUrl {
        host,
        port,
        path_and_query,
        fragment,
    };
    if out.to_link_string().len() > MAX_URL_BYTES {
        return Err(UrlError::TooLong);
    }
    Ok(out)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
    {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

fn split_host_port(authority: &str) -> Result<(&str, Option<u16>), UrlError> {
    if authority.starts_with('[') {
        let end = authority.find(']').ok_or(UrlError::BadHost)?;
        let host = &authority[..=end];
        let rest = &authority[end + 1..];
        return match rest {
            "" => Ok((host, None)),
            r => Ok((
                host,
                Some(parse_port(r.strip_prefix(':').ok_or(UrlError::BadHost)?)?),
            )),
        };
    }
    match authority.rsplit_once(':') {
        Some((h, p)) => Ok((h, Some(parse_port(p)?))),
        None => Ok((authority, None)),
    }
}

fn parse_port(p: &str) -> Result<u16, UrlError> {
    if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
        return Err(UrlError::BadPort);
    }
    match p.parse::<u16>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(UrlError::BadPort),
    }
}

/// A validated network fetch target: `https`, port 443 only, host not on the block-list
/// (by name / IP literal). Addresses must still be checked at connect time.
pub fn parse_fetch_url(raw: &str) -> Result<ParsedUrl, UrlError> {
    let u = parse_https_url(raw)?;
    if u.port.is_some() {
        return Err(UrlError::PortNotAllowed);
    }
    check_host_allowed(&u.host)?;
    Ok(u)
}

/// Resolve a redirect `Location` against the URL it came from and validate it as a fetch
/// target. Accepts absolute URLs, protocol-relative `//host/..` and root-relative `/path`;
/// rejects every other relative form.
pub fn resolve_redirect(base: &ParsedUrl, location: &str) -> Result<ParsedUrl, UrlError> {
    let loc = location.trim();
    if loc.is_empty() {
        return Err(UrlError::Empty);
    }
    if loc.starts_with("//") {
        return parse_fetch_url(&format!("https:{loc}"));
    }
    if loc.starts_with('/') {
        if has_forbidden_chars(loc) || loc.len() > MAX_URL_BYTES {
            return Err(UrlError::BadCharacters);
        }
        return parse_fetch_url(&format!("https://{}{}", base.host.authority_text(), loc));
    }
    let has_scheme = loc.split_once(':').is_some_and(|(s, _)| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    });
    if has_scheme {
        // Absolute, or a `scheme:` URL such as `javascript:` / `data:` that `parse_fetch_url`
        // refuses as a bad scheme.
        return parse_fetch_url(loc);
    }
    Err(UrlError::UnsupportedRelative)
}

/// Sanitise a URL that will be stored or displayed (`platformURL`, `linkURL`, `imageWebURL`,
/// `cueURL`, `freeDownloadURL`, `contributorURL`, `orgURL`): `https` only (`http` upgraded),
/// no userinfo, strict host, <= 2048 bytes, unsafe bytes percent-encoded. `None` = drop it.
pub fn sanitise_url(raw: &str) -> Option<String> {
    parse_https_url(raw).ok().map(|u| u.to_link_string())
}

/// Strip query strings that look like credentials (`token`, `key`, `sig`, `auth`, ...) so a
/// URL can go into a log line (`NBSecurityRequirements` §6).
pub fn redact_url_for_log(url: &str) -> String {
    let Some((base, rest)) = url.split_once('?') else {
        return url.to_string();
    };
    let (query, frag) = match rest.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (rest, None),
    };
    let sensitive = |name: &str| {
        let n = name.to_ascii_lowercase();
        [
            "token", "key", "sig", "auth", "secret", "pass", "session", "code",
        ]
        .iter()
        .any(|s| n.contains(s))
    };
    let kept: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if sensitive(k) => format!("{k}=REDACTED"),
            _ => kv.to_string(),
        })
        .collect();
    let mut out = format!("{base}?{}", kept.join("&"));
    if let Some(f) = frag {
        out.push('#');
        out.push_str(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_private_and_special_v4() {
        for s in [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "198.18.0.1",
            "192.0.2.1",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s}");
        }
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "172.15.0.1",
            "172.32.0.1",
            "100.63.0.1",
            "100.128.0.1",
            "93.184.216.34",
        ] {
            assert!(!is_blocked_ip(ip(s)), "{s}");
        }
    }

    #[test]
    fn blocks_private_and_special_v6_and_embedded_v4() {
        for s in [
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
            "2002:a9fe:a9fe::1",
            "2001:db8::1",
            "2001::1",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s}");
        }
        for s in [
            "2606:4700:4700::1111",
            "2a00:1450:4001::200e",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_blocked_ip(ip(s)), "{s}");
        }
    }

    #[test]
    fn blocks_local_names() {
        for h in [
            "localhost",
            "LOCALHOST",
            "foo.localhost",
            "printer.local",
            "router.lan",
            "nas.home.arpa",
            "db.internal",
            "intranet",
            "localhost.",
        ] {
            assert!(is_blocked_hostname(h), "{h}");
        }
        for h in [
            "example.com",
            "noubin.com",
            "www.artist.co.uk",
            "localhost.example.com",
        ] {
            assert!(!is_blocked_hostname(h), "{h}");
        }
    }

    #[test]
    fn host_parser_refuses_odd_ip_spellings() {
        for h in [
            "2130706433",
            "0x7f.0.0.1",
            "127.1",
            "0177.0.0.1",
            "1.2.3",
            "999.1.1.1",
            "a_b.example.com",
            "-a.example.com",
            "exa mple.com",
            "[fe80::1%eth0]",
            "[::1",
            "bücher.example",
            "",
        ] {
            assert!(parse_host(h).is_err(), "{h}");
        }
        // Single-label names parse but the block-list refuses them.
        assert!(check_host_allowed(&parse_host("0x7f000001").unwrap()).is_err());
        assert!(matches!(parse_host("127.0.0.1"), Ok(Host::Ip(_))));
        assert!(matches!(parse_host("[::1]"), Ok(Host::Ip(_))));
        assert_eq!(
            parse_host("Example.COM."),
            Ok(Host::Name("example.com".into()))
        );
        assert!(matches!(
            parse_host("xn--bcher-kva.example"),
            Ok(Host::Name(_))
        ));
    }

    #[test]
    fn fetch_url_rules() {
        assert!(parse_fetch_url("https://example.com/a/b").is_ok());
        assert!(parse_fetch_url("http://example.com/a").is_ok()); // upgraded
        assert_eq!(
            parse_fetch_url("http://example.com/a")
                .unwrap()
                .to_fetch_string(),
            "https://example.com/a"
        );
        assert_eq!(
            parse_fetch_url("https://example.com:8443/").unwrap_err(),
            UrlError::PortNotAllowed
        );
        assert_eq!(
            parse_fetch_url("https://example.com:443/").unwrap().port,
            None
        );
        assert_eq!(
            parse_fetch_url("ftp://example.com/").unwrap_err(),
            UrlError::BadScheme
        );
        assert_eq!(
            parse_fetch_url("file:///etc/passwd").unwrap_err(),
            UrlError::BadScheme
        );
        assert_eq!(
            parse_fetch_url("javascript:alert(1)").unwrap_err(),
            UrlError::BadScheme
        );
        assert_eq!(
            parse_fetch_url("https://u:p@example.com/").unwrap_err(),
            UrlError::Userinfo
        );
        assert_eq!(
            parse_fetch_url("https://example.com@evil.com/").unwrap_err(),
            UrlError::Userinfo
        );
        assert!(matches!(
            parse_fetch_url("https://127.0.0.1/"),
            Err(UrlError::Blocked(_))
        ));
        assert!(matches!(
            parse_fetch_url("https://[::1]/"),
            Err(UrlError::Blocked(_))
        ));
        assert!(matches!(
            parse_fetch_url("https://169.254.169.254/latest/meta-data"),
            Err(UrlError::Blocked(_))
        ));
        assert!(matches!(
            parse_fetch_url("https://localhost/"),
            Err(UrlError::Blocked(_))
        ));
        assert!(matches!(
            parse_fetch_url("https://nas.local/"),
            Err(UrlError::Blocked(_))
        ));
        assert!(parse_fetch_url("https://example.com/a\r\nHost: evil").is_err());
        assert!(parse_fetch_url(&format!("https://example.com/{}", "a".repeat(3000))).is_err());
        assert!(parse_fetch_url("https://example.com\\@evil.com/").is_err());
    }

    #[test]
    fn redirect_resolution() {
        let base = parse_fetch_url("https://example.com/a/web.noudata").unwrap();
        assert_eq!(
            resolve_redirect(&base, "/b/web.noudata")
                .unwrap()
                .to_fetch_string(),
            "https://example.com/b/web.noudata"
        );
        assert_eq!(
            resolve_redirect(&base, "//cdn.example.org/x")
                .unwrap()
                .to_fetch_string(),
            "https://cdn.example.org/x"
        );
        assert_eq!(
            resolve_redirect(&base, "https://other.example.net/y?z=1")
                .unwrap()
                .to_fetch_string(),
            "https://other.example.net/y?z=1"
        );
        for bad in [
            "http://127.0.0.1/",
            "https://10.0.0.1/",
            "https://169.254.169.254/",
            "https://[fd00::1]/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,x",
            "gopher://example.com/",
            "relative/path",
            "https://example.com:8080/",
            "",
        ] {
            assert!(resolve_redirect(&base, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn link_sanitiser() {
        assert_eq!(sanitise_url("http://Example.com/a b"), None); // raw whitespace is refused
        assert_eq!(
            sanitise_url("HTTP://Example.COM/a%20b?x=ä#frag"),
            Some("https://example.com/a%20b?x=%C3%A4#frag".into())
        );
        assert_eq!(
            sanitise_url("https://example.com"),
            Some("https://example.com/".into())
        );
        assert_eq!(sanitise_url("javascript:alert(1)"), None);
        assert_eq!(sanitise_url("data:text/html;base64,AAAA"), None);
        assert_eq!(sanitise_url("//example.com/x"), None);
        assert_eq!(sanitise_url("https://user@example.com/"), None);
        assert_eq!(sanitise_url("https://exa\u{202E}mple.com/"), None);
        assert_eq!(sanitise_url("https://example.com/\u{0007}"), None);
        assert_eq!(
            sanitise_url(&format!("https://example.com/{}", "a".repeat(2100))),
            None
        );
        // A quote or angle bracket can never survive into an attribute.
        let u = sanitise_url("https://example.com/\"><script>alert(1)</script>").unwrap();
        assert!(!u.contains(['"', '<', '>']), "{u}");
        // Lone percent is escaped.
        assert_eq!(
            sanitise_url("https://example.com/100%"),
            Some("https://example.com/100%25".into())
        );
        // Displayed links may carry a non-443 port; fetch targets may not.
        assert!(sanitise_url("https://example.com:8443/x").is_some());
    }

    #[test]
    fn log_redaction() {
        assert_eq!(
            redact_url_for_log("https://a.example/x?ref=nfc&token=abc123&Sig=zz#f"),
            "https://a.example/x?ref=nfc&token=REDACTED&Sig=REDACTED#f"
        );
        assert_eq!(
            redact_url_for_log("https://a.example/x"),
            "https://a.example/x"
        );
    }
}
