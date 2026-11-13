//! §6.2 Noubin Key Normalisation.

/// Produce a Normalised Noubin Key for local library matching.
///
/// Transformations run in the exact order defined by the Noubin standard §6.2.
pub fn normalise_noubin_key(key: &str) -> String {
    // 1. Case Normalization (ASCII only)
    let mut s: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .collect();

    // 2. Whitespace Trimming
    let trimmed = s.trim();
    s = trimmed.to_string();

    if !is_url_key(&s) {
        return s;
    }

    // 3. Protocol Stripping
    if let Some(rest) = s.strip_prefix("https://") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("http://") {
        s = rest.to_string();
    }

    // 4. Authority and parameter stripping
    s = strip_authority_and_params(&s);

    // 5. www. stripping
    s = strip_www_label(&s);

    // 6. Path percent-decoding (except %2f)
    s = percent_decode_path_except_slash(&s);

    // 7. Trailing slash
    if !s.ends_with('/') {
        s.push('/');
    }

    s
}

fn is_url_key(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn strip_authority_and_params(s: &str) -> String {
    // 4.1 fragment
    let without_frag = s.split_once('#').map(|(a, _)| a).unwrap_or(s);
    // 4.2 query
    let without_query = without_frag
        .split_once('?')
        .map(|(a, _)| a)
        .unwrap_or(without_frag);

    // Split authority / path. Path starts at first '/'.
    let (authority, path) = match without_query.find('/') {
        Some(i) => (&without_query[..i], &without_query[i..]),
        None => (without_query, ""),
    };

    // 4.3 userinfo
    let authority = authority
        .rsplit_once('@')
        .map(|(_, hostport)| hostport)
        .unwrap_or(authority);

    // 4.4 port — only colon-digits at end of authority (not path colons)
    let host = strip_port(authority);

    format!("{host}{path}")
}

pub(crate) fn strip_port(authority: &str) -> &str {
    if let Some((host, port)) = authority.rsplit_once(':') {
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            return host;
        }
    }
    authority
}

pub(crate) fn strip_www_label(s: &str) -> String {
    // Host is the substring before first '/' (path may be empty).
    let (host, rest) = match s.find('/') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    if let Some(stripped) = host.strip_prefix("www.") {
        format!("{stripped}{rest}")
    } else {
        s.to_string()
    }
}

fn percent_decode_path_except_slash(s: &str) -> String {
    let (host, path) = match s.find('/') {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let decoded_path = percent_decode_except_2f(path);
    format!("{host}{decoded_path}")
}

pub(crate) fn percent_decode_except_2f(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h1 = bytes[i + 1];
            let h2 = bytes[i + 2];
            if is_hex(h1) && is_hex(h2) {
                // Already ASCII-lowercased in step 1, so %2F → %2f.
                if h1.eq_ignore_ascii_case(&b'2') && h2.eq_ignore_ascii_case(&b'f') {
                    out.extend_from_slice(&[b'%', b'2', b'f']);
                    i += 3;
                    continue;
                }
                let v = (hex_val(h1) << 4) | hex_val(h2);
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Path may become invalid UTF-8 if decoding produces lone bytes; lossy is wrong
    // for keys — standard examples are valid UTF-8 sequences. Use lossy only as fallback.
    match String::from_utf8(out.clone()) {
        Ok(s) => s,
        Err(_) => String::from_utf8_lossy(&out).into_owned(),
    }
}

fn is_hex(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F')
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}
