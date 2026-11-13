//! NB_SEC security review: adversarial fixtures for the URL / host policy and the sanitisers.

use plippa_noubin::{
    is_blocked_ip, parse_fetch_url, parse_noudata_with_warnings, resolve_redirect,
    sanitise_document, sanitise_line, sanitise_multiline, sanitise_url, SanitiseOptions,
};
use serde_json::Value;
use std::net::IpAddr;

#[test]
fn nb_sec_ip_block_list_covers_embedded_and_exotic_forms() {
    for ip in [
        "0.0.0.0",
        "0.1.2.3",
        "10.1.2.3",
        "100.64.0.1",
        "100.127.255.255",
        "127.0.0.1",
        "127.255.255.254",
        "169.254.169.254",
        "172.16.0.1",
        "172.31.255.255",
        "192.0.0.1",
        "192.168.1.1",
        "198.18.0.1",
        "224.0.0.1",
        "239.255.255.250",
        "240.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::ffff:169.254.169.254",
        "::ffff:7f00:1",
        "::127.0.0.1",
        "::ffff:0:127.0.0.1", // SIIT / IPv4-translated
        "64:ff9b::7f00:1",    // NAT64
        "64:ff9b::a9fe:a9fe",
        "64:ff9b:1::7f00:1", // RFC 8215 local-use NAT64
        "2002:7f00:1::1",    // 6to4 of 127.0.0.1
        "2002:a9fe:a9fe::1",
        "2002:c0a8:101::1",
        "2001:0:4136:e378:8000:63bf:3fff:fdd2", // Teredo
        "2001:db8::1",
        "fc00::1",
        "fd12:3456::1",
        "fe80::1",
        "febf::1",
        "fec0::1",
        "ff02::1",
        "100::1",
    ] {
        let ip: IpAddr = ip.parse().unwrap();
        assert!(is_blocked_ip(ip), "{ip} must be blocked");
    }
    for ip in [
        "1.1.1.1",
        "8.8.8.8",
        "93.184.216.34",
        "2606:4700:4700::1111",
        "2a00:1450:4001::200e",
    ] {
        let ip: IpAddr = ip.parse().unwrap();
        assert!(!is_blocked_ip(ip), "{ip} is public");
    }
}

#[test]
fn nb_sec_hostile_fetch_urls_are_refused() {
    for u in [
        "https://127.0.0.1/",
        "https://2130706433/",
        "https://0x7f000001/",
        "https://0x7f.0.0.1/",
        "https://017700000001/",
        "https://0177.0.0.1/",
        "https://127.1/",
        "https://127.0.1/",
        "https://[::1]/",
        "https://[::ffff:127.0.0.1]/",
        "https://[::ffff:7f00:1]/",
        "https://[0:0:0:0:0:ffff:7f00:1]/",
        "https://[fe80::1%25eth0]/",
        "https://localhost/",
        "https://localhost./",
        "https://LOCALHOST/",
        "https://foo.localhost/",
        "https://foo.localhost./",
        "https://printer.local/",
        "https://metadata.google.internal/",
        "https://router.lan/",
        "https://intranet/",
        "https://good.example@evil.example/",
        "https://good.example:pw@127.0.0.1/",
        "https://evil.example\\@good.example/",
        "https://good.example%2f@127.0.0.1/",
        "https://127.0.0.1:443/",
        "https://example.com:8443/",
        "https://example.com:0/",
        "https://example.com:99999/",
        "https://exa mple.com/",
        "https://example.com\u{202E}/",
        "https://example\u{3002}com/", // ideographic full stop
        "https://\u{ff11}\u{ff12}\u{ff17}.0.0.1/", // fullwidth digits
        "https://xn--/",
        "https://.example.com/",
        "https://example..com/",
        "https://-a.example.com/",
        "https://%31%32%37.0.0.1/",
        "ftp://example.com/",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "data:text/plain,hi",
        "//example.com/",
        "https:///etc/passwd",
        "https://",
        "https://[::1",
        "https://1.2.3.4.5/",
        "https://999.1.1.1/",
        "https://1.1.1.1.example/x\nHost: evil",
        "https://example.com/\r\nX: y",
        "https://example.com/\0",
    ] {
        assert!(parse_fetch_url(u).is_err(), "must refuse {u:?}");
    }
}

#[test]
fn nb_sec_redirects_are_revalidated() {
    let base = parse_fetch_url("https://good.example/a").unwrap();
    for loc in [
        "http://127.0.0.1/",
        "https://127.0.0.1/",
        "//127.0.0.1/",
        "//localhost/x",
        "https://good.example:8443/",
        "//good.example:8443/",
        "ftp://good.example/",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "data:text/html,x",
        "https://user@good.example/",
        "relative/path",
        "../x",
        "?q=1",
        "#frag",
        "",
        "https://[::ffff:169.254.169.254]/",
        "\\\\evil.example\\x",
        "/\\evil.example",
        "///evil.example",
        "https:evil.example",
    ] {
        match resolve_redirect(&base, loc) {
            Err(_) => {}
            Ok(u) => {
                // Anything accepted must still be a public https host on the default port.
                let s = u.to_fetch_string();
                assert!(
                    s.starts_with("https://good.example/") || s.starts_with("https://evil.example"),
                    "{loc:?} -> {s}"
                );
                assert!(!s.contains('\\'), "{loc:?} -> {s}");
            }
        }
    }
}

#[test]
fn nb_sec_stored_urls_never_keep_dangerous_schemes() {
    for u in [
        "javascript:alert(1)",
        "JaVaScRiPt:alert(1)",
        " javascript:alert(1)",
        "\tjavascript:alert(1)",
        "java\nscript:alert(1)",
        "data:text/html;base64,PHNjcmlwdD4=",
        "vbscript:x",
        "file:///etc/passwd",
        "blob:https://x.example/1",
        "https://u:p@example.com/",
        "https://example.com/\"onmouseover=\"alert(1)",
    ] {
        match sanitise_url(u) {
            None => {}
            Some(s) => {
                assert!(s.starts_with("https://"), "{u:?} -> {s}");
                assert!(
                    !s.contains(['"', '<', '>', ' ', '\'']) || !s.contains('"'),
                    "{u:?} -> {s}"
                );
                assert!(!s.contains('"'), "{u:?} -> {s}");
            }
        }
    }
}

const JUNK: &str = "\u{202E}\u{0007}\u{200B}\u{206A}\u{E0041}\u{FFF9}\u{2066}\u{180E}";

fn is_bad_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
            | '\u{061C}' | '\u{FEFF}' | '\u{2028}' | '\u{2029}' | '\u{E0000}'..='\u{E007F}'
            | '\u{FFF9}'..='\u{FFFB}' | '\u{180E}')
}

fn prefix_strings(v: &mut Value) {
    match v {
        Value::String(s) => *s = format!("{JUNK}{s}{JUNK}"),
        Value::Array(a) => a.iter_mut().for_each(prefix_strings),
        Value::Object(o) => o.values_mut().for_each(prefix_strings),
        _ => {}
    }
}

fn walk(v: &Value, path: &str, multiline: bool, bad: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            let ml = multiline;
            if s.chars()
                .any(|c| is_bad_char(c) && !(ml && (c == '\n' || c == '\t')))
            {
                bad.push(format!("{path}: {s:?}"));
            }
            let lower = s.trim().to_ascii_lowercase();
            for scheme in ["javascript:", "data:", "vbscript:", "file:"] {
                if lower.starts_with(scheme) {
                    bad.push(format!("{path}: dangerous scheme {s:?}"));
                }
            }
            if s.len() > 64 * 1024 + 8 {
                bad.push(format!("{path}: {} bytes", s.len()));
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                walk(x, &format!("{path}[{i}]"), multiline, bad);
            }
        }
        Value::Object(o) => {
            for (k, x) in o {
                let ml = matches!(
                    k.as_str(),
                    "description"
                        | "legalNotice"
                        | "additionalCreditsText"
                        | "lyricsTranscript"
                        | "userComment"
                );
                walk(x, &format!("{path}.{k}"), ml, bad);
            }
        }
        _ => {}
    }
}

fn all_fixtures() -> Vec<Value> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "noudata") {
            if let Ok(v) = serde_json::from_slice::<Value>(&std::fs::read(&p).unwrap()) {
                out.push(v);
            }
        }
    }
    out
}

#[test]
fn nb_sec_sanitise_document_reaches_every_string_leaf() {
    for (n, mut v) in all_fixtures().into_iter().enumerate() {
        prefix_strings(&mut v);
        // Also shove a hostile unknown property at the root and in the first media item.
        v["x-unknown"] =
            serde_json::json!({"a": [{"b": format!("{JUNK}deep")}], "someURL": "javascript:1"});
        let (mut doc, _) = parse_noudata_with_warnings(v.to_string().as_bytes()).unwrap();
        for opts in [SanitiseOptions::fetched(), SanitiseOptions::imported()] {
            let mut d = doc.clone();
            sanitise_document(&mut d, opts);
            let out: Value = serde_json::from_slice(&serde_json::to_vec(&d).unwrap()).unwrap();
            let mut bad = Vec::new();
            walk(&out, "$", false, &mut bad);
            assert!(bad.is_empty(), "fixture {n}: {bad:#?}");
        }
        let _ = &mut doc;
    }
}

#[test]
fn nb_sec_line_and_multiline_strip_all_invisible_format_chars() {
    let s = format!("a{JUNK}b");
    assert_eq!(sanitise_line(&s, 64), "ab");
    assert_eq!(sanitise_multiline(&s, 64), "ab");
}

#[test]
fn nb_sec_oversized_local_noudata_is_skipped_not_read_into_memory() {
    use plippa_noubin::{LibraryRoot, LibraryWriter};
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("a.mp3"), b"x").unwrap();
    let ok = r#"{"playableItemTitle":"Ok","mediaList":[{"localFilePaths":["a.mp3"]}]}"#;
    std::fs::write(root.join("ok.noudata"), ok).unwrap();
    // 20 MiB of otherwise valid JSON (a hostile stick can make this arbitrarily large).
    let big = format!(
        r#"{{"playableItemTitle":"{}","mediaList":[{{"localFilePaths":["a.mp3"]}}]}}"#,
        "x".repeat(20 * 1024 * 1024)
    );
    std::fs::write(root.join("big.noudata"), big).unwrap();
    let lib = LibraryRoot::new(root);
    let mut w = LibraryWriter::open(&lib).unwrap();
    w.full_scan().unwrap();
    let titles: Vec<String> = w
        .list_playables()
        .unwrap()
        .into_iter()
        .filter_map(|p| p.title)
        .collect();
    assert_eq!(titles.len(), 1, "the 20 MiB document must be skipped");
    assert_eq!(titles[0], "Ok");
}

// ---------------------------------------------------------------------------
// Fuzz-style hostile input: nothing may panic, and the invariants must hold.
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() % xs.len() as u64) as usize]
    }
}

const ATOMS: &[&str] = &[
    "https://",
    "http://",
    "HTTPS://",
    "//",
    "/",
    "..",
    "../",
    ".",
    ":",
    "@",
    "%",
    "%2f",
    "%2F",
    "%00",
    "%zz",
    "%",
    "[",
    "]",
    "::",
    "::1",
    "127.0.0.1",
    "0x7f",
    "0177",
    "2130706433",
    "localhost",
    ".local",
    "example.com",
    "xn--",
    "\u{202E}",
    "\u{200B}",
    "\u{0}",
    "\u{7}",
    "\n",
    "\r\n",
    " ",
    "\t",
    "\\",
    "é",
    "日本",
    "😀",
    "\u{ff11}",
    "\u{3002}",
    "?",
    "#",
    "&",
    "=",
    "\"",
    "<",
    ">",
    "'",
    ";",
    "*",
    "CON",
    "nul",
    "COM1",
    "a",
    "b",
    "Z",
    "0",
    "1",
    "9",
    "-",
    "_",
    "~",
    ",",
    "!",
    "$",
    "(",
    ")",
    "0:00",
    "1:02:03",
    "99:99:99",
    "-1",
    "18446744073709551615",
    "1e999",
    "  ",
    "|",
    "`",
];

fn hostile_string(r: &mut Rng) -> String {
    let n = 1 + r.next() % 12;
    (0..n).map(|_| *r.pick(ATOMS)).collect()
}

#[test]
fn nb_sec_fuzz_parsers_never_panic_and_keep_their_invariants() {
    use plippa_noubin::{
        check_json_object_limits, fetch_url_for_key, is_safe_relative_path, normalise_noubin_key,
        parse_cue_paste, parse_https_url, redact_url_for_log, safe_path_segment, sanitise_key,
        sanitise_tag, sanitise_title, NoubinTimestamp,
    };
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..30_000 {
        let s = hostile_string(&mut r);
        let base = parse_fetch_url("https://good.example/a/b").unwrap();
        // URL family
        if let Ok(u) = parse_fetch_url(&s) {
            let f = u.to_fetch_string();
            assert!(f.is_ascii() && f.starts_with("https://"), "{s:?} -> {f}");
            assert!(!f.contains(['\\', '\n', '\r', ' ', '\0']), "{s:?} -> {f}");
            let authority = f["https://".len()..].split(['/', '?', '#']).next().unwrap();
            assert!(
                !authority.contains('@') && !authority.contains(':') || authority.starts_with('['),
                "{s:?} -> {f}"
            );
            match &u.host {
                plippa_noubin::Host::Ip(ip) => assert!(!is_blocked_ip(*ip), "{s:?}"),
                plippa_noubin::Host::Name(n) => {
                    assert!(!plippa_noubin::is_blocked_hostname(n), "{s:?} -> {n}");
                    assert!(n.contains('.'), "{s:?}");
                }
            }
        }
        if let Ok(u) = resolve_redirect(&base, &s) {
            let f = u.to_fetch_string();
            assert!(f.is_ascii() && f.starts_with("https://"), "{s:?} -> {f}");
            assert!(u.port.is_none());
        }
        if let Some(u) = sanitise_url(&s) {
            assert!(u.starts_with("https://") && u.is_ascii(), "{s:?} -> {u}");
        }
        let _ = parse_https_url(&s);
        let _ = redact_url_for_log(&s);
        let _ = normalise_noubin_key(&s);
        if let Ok(f) = fetch_url_for_key(&s) {
            assert!(parse_fetch_url(&f.wire).is_ok(), "{s:?} -> {}", f.wire);
        }
        // Text and path family
        let seg = safe_path_segment(&s, "fallback", 200);
        assert!(is_safe_relative_path(&seg), "{s:?} -> {seg:?}");
        assert!(
            seg.len() <= 200 && !seg.starts_with('.'),
            "{s:?} -> {seg:?}"
        );
        for t in [
            sanitise_title(&s),
            sanitise_line(&s, 64),
            sanitise_multiline(&s, 64),
        ] {
            assert!(
                !t.chars().any(|c| is_bad_char(c) && c != '\n' && c != '\t'),
                "{s:?} -> {t:?}"
            );
        }
        let _ = sanitise_tag(&s);
        let _ = sanitise_key(&s);
        // Parsers of user text
        let _ = NoubinTimestamp::parse(&s);
        let paste = format!(
            "{s}\n{}\n{} {}\n",
            hostile_string(&mut r),
            hostile_string(&mut r),
            hostile_string(&mut r)
        );
        let _ = parse_cue_paste(&paste);
        // JSON structural guard and typed parser on arbitrary bytes / junk JSON
        let json = format!("{{\"playableItemTitle\":{:?},\"mediaList\":[{{\"mediaTitle\":{:?},\"localFilePaths\":[{:?}]}}]}}", s, hostile_string(&mut r), hostile_string(&mut r));
        assert!(check_json_object_limits(json.as_bytes()).is_ok());
        if let Ok((mut d, _)) = parse_noudata_with_warnings(json.as_bytes()) {
            sanitise_document(&mut d, SanitiseOptions::imported());
            sanitise_document(&mut d, SanitiseOptions::fetched());
        }
        let bytes: Vec<u8> = (0..(r.next() % 64)).map(|_| r.next() as u8).collect();
        let _ = check_json_object_limits(&bytes);
        let _ = parse_noudata_with_warnings(&bytes);
    }
}

#[test]
fn nb_sec_json_bombs_are_refused_before_parsing() {
    use plippa_noubin::check_json_object_limits;
    let deep = format!("{{\"a\":{}{}}}", "[".repeat(100_000), "]".repeat(100_000));
    assert!(check_json_object_limits(deep.as_bytes()).is_err());
    let wide = format!("{{\"a\":[{}]}}", "1,".repeat(200_000) + "1");
    assert!(check_json_object_limits(wide.as_bytes()).is_err());
    let keys: String = (0..20_000).map(|i| format!("\"k{i}\":1,")).collect();
    assert!(check_json_object_limits(format!("{{{keys}\"z\":1}}").as_bytes()).is_err());
    // Brackets inside strings must not count.
    let in_str = format!("{{\"a\":\"{}\"}}", "[".repeat(50_000));
    assert!(check_json_object_limits(in_str.as_bytes()).is_ok());
    // The typed parser itself survives a document at the guard's limits.
    let ok = format!(
        "{{\"playableItemTitle\":\"x\",\"mediaList\":[{}]}}",
        "{\"mediaTitle\":\"t\"},".repeat(2000) + "{}"
    );
    assert!(check_json_object_limits(ok.as_bytes()).is_ok());
    let _ = parse_noudata_with_warnings(ok.as_bytes());
}

#[test]
fn nb_sec_unite_on_hostile_names_and_tags_stays_inside_the_library() {
    use plippa_noubin::unite::{build_local_doc, propose, CandidateFile, EmbeddedTags, Placement};
    use plippa_noubin::{is_safe_relative_path, PlayableItemProfile};
    let web = r#"{"playableItemTitle":"..\/..\/evil","playableItemProfile":"music","releaseData":{
      "credits":{"primaryArtists":[{"primaryArtistName":"../../../etc"}]}},
      "mediaList":[{"mediaTitle":"CON","uniteData":{"expectedFilename":"../../x.mp3"}},
                   {"mediaTitle":"/etc/passwd"},{"mediaTitle":"a:b*c"},{"mediaTitle":"nul.txt"}]}"#;
    let (doc, _) = parse_noudata_with_warnings(web.as_bytes()).unwrap();
    let mut r = Rng(42);
    for round in 0..200 {
        let files: Vec<CandidateFile> = (0..(1 + round % 6))
            .map(|_| {
                let name = format!(
                    "{}{}",
                    hostile_string(&mut r),
                    r.pick(&[".mp3", ".flac", ".mp4", ".noudata", "", ".jpg"])
                );
                let mut c = CandidateFile::new(format!("stage/{name}"), 1000);
                c = c.with_duration_ms(r.next() % 4_000_000);
                c.with_tags(EmbeddedTags {
                    title: Some(hostile_string(&mut r)),
                    artist: Some(hostile_string(&mut r)),
                    album: None,
                    track_number: Some((r.next() % 300) as u32),
                    disc_number: None,
                })
            })
            .collect();
        let proposal = propose(&doc, &files, PlayableItemProfile::Music);
        let mapping = proposal.confirm_all();
        let placement = Placement::for_release(&doc, PlayableItemProfile::Music);
        if let Ok(plan) = build_local_doc(&doc, &mapping, None, &placement) {
            let n = plan.noudata_path.to_string_lossy().to_string();
            assert!(is_safe_relative_path(&n), "{n}");
            for op in &plan.file_ops {
                let t = op.target.to_string_lossy().to_string();
                assert!(is_safe_relative_path(&t), "{t}");
            }
            for it in &plan.doc.media_list {
                for p in it.local_file_paths.iter().flatten() {
                    assert!(is_safe_relative_path(p), "{p}");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NB_VERIFY NB_08: SEC-02 / SEC-05 fixes pinned individually (a mutation run showed the
// existing list tests did not notice the fail-closed rule or the tail of the 206x range).
// ---------------------------------------------------------------------------

#[test]
fn nb_sec_ipv6_outside_global_unicast_is_fail_closed() {
    // Only 2000::/3 is global unicast: every other prefix (reserved, future, unassigned) is refused.
    for ip in [
        "1000::1", "4000::1", "5000::1", "6000::1", "8000::1", "a000::1", "c000::1", "e000::1",
        "f000::1", "0100:1::1", "2::1",
    ] {
        let ip: IpAddr = ip.parse().unwrap();
        assert!(is_blocked_ip(ip), "{ip} is outside 2000::/3 and must be refused");
    }
    // Real global unicast (all of 2000::/3 except the special ranges) stays reachable.
    for ip in ["2000::1", "2606:4700:4700::1111", "2a00:1450:4001::200e", "3ffe::1"] {
        let ip: IpAddr = ip.parse().unwrap();
        assert!(!is_blocked_ip(ip), "{ip} is global unicast");
    }
}

#[test]
fn nb_sec_every_claimed_invisible_format_char_is_stripped() {
    let ranges: &[(u32, u32)] = &[
        (0x200B, 0x200F),
        (0x202A, 0x202E),
        (0x2060, 0x2064),
        (0x2066, 0x2069),
        (0x206A, 0x206F),
        (0x180E, 0x180E),
        (0xFFF9, 0xFFFB),
        (0xE0000, 0xE007F),
        (0x061C, 0x061C),
        (0xFEFF, 0xFEFF),
    ];
    for &(lo, hi) in ranges {
        for cp in lo..=hi {
            let c = char::from_u32(cp).unwrap();
            let s = format!("a{c}b");
            assert_eq!(sanitise_line(&s, 64), "ab", "U+{cp:04X} survived sanitise_line");
            assert_eq!(sanitise_multiline(&s, 64), "ab", "U+{cp:04X} survived sanitise_multiline");
        }
    }
}
