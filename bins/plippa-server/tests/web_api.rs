//! NB_08: consent gate, local-first rule, single flight, sanitised overview, and the portal's
//! HTTP guards. The network is a counting mock [`Transport`]: "zero sockets" means zero calls.

mod common;

use plippa_ipc_proto::UnmatchedNoubin;
use plippa_server::web::fetch::{HopResponse, Transport, TransportError, WebError};
use plippa_server::web::{fetch_unlocked, prepare_unlocked, WebConfig, WebHub, MAX_SESSIONS};
use plippa_server::{RecordingIpc, ServerConfig, ServerService};
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const GOOD: &str = r#"{
    "playableItemTitle": "The Album",
    "playableItemProfile": "music", "releaseData": {"credits": {"primaryArtists": [{"primaryArtistName": "The Band"}]},
        "coverImage": {"images": [{"imageWebURL": "https://cdn.example/cover.jpg"}]},
        "releasePlatforms": [
            {"platformName": "Zed", "platformType": "streaming", "platformURL": "https://zed.example/a"},
            {"platformName": "Alpha", "platformType": "store", "platformURL": "https://alpha.example/a"},
            {"platformName": "Mid", "platformType": "other", "platformURL": "https://mid.example/a"},
            {"platformName": "Spin", "platformType": "other", "customVerb": "spin", "platformURL": "https://spin.example/a"}
        ],
        "furtherLinks": [{"linkTitle": "Site", "linkURL": "https://band.example/"}]
    },
    "mediaList": [
        {"mediaTitle": "One", "uniteData": {"expectedLength": "3:05", "freeDownloadURL": "https://dl.example/1.mp3"}},
        {"mediaTitle": "Two", "uniteData": {"expectedLength": "4:00"}}
    ]
}"#;

type Answer = Box<dyn Fn(&str) -> Result<HopResponse, TransportError> + Send + Sync>;

/// Counts every `get` (each would be a connection) and answers from a closure.
struct Counting {
    calls: Arc<AtomicUsize>,
    urls: Arc<Mutex<Vec<String>>>,
    answer: Answer,
}

impl Transport for Counting {
    fn get(&self, url: &str, _t: Duration) -> Result<HopResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.urls.lock().unwrap().push(url.to_string());
        (self.answer)(url)
    }
}

fn ok(body: &str) -> Result<HopResponse, TransportError> {
    Ok(HopResponse {
        status: 200,
        content_type: Some("application/json".into()),
        location: None,
        content_length: Some(body.len() as u64),
        body: Box::new(Cursor::new(body.as_bytes().to_vec())),
    })
}

struct Rig {
    hub: Arc<WebHub>,
    calls: Arc<AtomicUsize>,
    urls: Arc<Mutex<Vec<String>>>,
}

fn rig_with(
    cfg: WebConfig,
    online: bool,
    answer: impl Fn(&str) -> Result<HopResponse, TransportError> + Send + Sync + 'static,
) -> Rig {
    let calls = Arc::new(AtomicUsize::new(0));
    let urls = Arc::new(Mutex::new(Vec::new()));
    let t = Counting {
        calls: Arc::clone(&calls),
        urls: Arc::clone(&urls),
        answer: Box::new(answer),
    };
    let hub = Arc::new(WebHub::with_transport(
        cfg,
        Arc::new(t),
        Box::new(move || online),
    ));
    Rig { hub, calls, urls }
}

fn rig(body: &'static str) -> Rig {
    rig_with(WebConfig::default(), true, move |_| ok(body))
}

/// Service over a temp library holding one playable keyed `https://have.example/a/`.
fn service(rig: &Rig) -> (tempfile::TempDir, Arc<Mutex<ServerService>>) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::write(root.join("a.mp3"), b"x").unwrap();
    std::fs::write(
        root.join("_have.noudata"),
        r#"{"noubinKey": "https://have.example/a/", "noubinKeyNormalised": "have.example/a/",
            "playableItemTitle": "Have", "mediaList": [{"localFilePaths": ["a.mp3"]}]}"#,
    )
    .unwrap();
    let mut svc = ServerService::start_with_roots(
        vec![root],
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    svc.set_web_hub(Arc::clone(&rig.hub));
    (tmp, Arc::new(Mutex::new(svc)))
}

// ----- consent gate ---------------------------------------------------------------------

#[test]
fn web_fetch_without_consent_opens_zero_sockets() {
    let r = rig(GOOD);
    let (_t, svc) = service(&r);
    for consent in [None, Some(false)] {
        let e = fetch_unlocked(&svc, "https://artist.example/noubin/x/", consent).unwrap_err();
        assert_eq!(e, WebError::ConsentRequired);
        assert_eq!(e.http_status(), 403);
        // The hub itself refuses too (the gate is not only in the wrapper).
        let e = r
            .hub
            .fetch("https://artist.example/noubin/x/", consent)
            .unwrap_err();
        assert_eq!(e, WebError::ConsentRequired);
    }
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn web_prepare_shows_the_full_url_and_sends_nothing() {
    let r = rig(GOOD);
    let (_t, svc) = service(&r);
    let p = prepare_unlocked(
        &svc,
        "http://www.Artist.example:8443/noubin/purple%20rain?ref=nfc#t",
    )
    .unwrap();
    assert_eq!(
        p.fetch_url,
        "https://artist.example/noubin/purple rain/web.noudata?ref=nfc"
    );
    assert_eq!(p.host, "artist.example");
    assert!(p.consent_message.contains("artist.example"));
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn web_fetch_with_consent_requests_exactly_the_prepared_wire_url() {
    let r = rig(GOOD);
    let (_t, svc) = service(&r);
    let out = fetch_unlocked(
        &svc,
        "https://artist.example/noubin/purple%20rain",
        Some(true),
    )
    .unwrap();
    assert_eq!(r.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        r.urls.lock().unwrap()[0],
        "https://artist.example/noubin/purple%20rain/web.noudata"
    );
    assert!(plippa_server::web::is_token(&out.token));
}

// ----- local first ----------------------------------------------------------------------

#[test]
fn web_local_hit_never_touches_the_network() {
    let r = rig(GOOD);
    let (_t, svc) = service(&r);
    // Same key spelled differently: normalisation makes it a local hit.
    for key in [
        "https://have.example/a/",
        "http://www.HAVE.example/a?x=1",
        "https://have.example:8443/a",
    ] {
        assert_eq!(
            prepare_unlocked(&svc, key).unwrap_err(),
            WebError::LocalMatch,
            "{key}"
        );
        assert_eq!(
            fetch_unlocked(&svc, key, Some(true)).unwrap_err(),
            WebError::LocalMatch,
            "{key}"
        );
    }
    assert_eq!(
        r.calls.load(Ordering::SeqCst),
        0,
        "tripwire: a local hit opened a socket"
    );
}

#[test]
fn web_only_url_keys_can_be_looked_up() {
    let r = rig(GOOD);
    let (_t, svc) = service(&r);
    for key in [
        "my private key",
        "",
        "ftp://a.example/x",
        "javascript:alert(1)",
        "file:///etc/passwd",
    ] {
        assert!(
            matches!(
                fetch_unlocked(&svc, key, Some(true)),
                Err(WebError::NotAUrl | WebError::BadUrl(_))
            ),
            "{key:?}"
        );
    }
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);
}

// ----- SSRF at the hub boundary -----------------------------------------------------------

#[test]
fn web_private_targets_are_refused_before_any_socket() {
    let r = rig(GOOD);
    let (_t, svc) = service(&r);
    for key in [
        "https://127.0.0.1/x/",
        "https://localhost/x/",
        "https://[::1]/x/",
        "https://169.254.169.254/latest/meta-data/",
        "https://10.0.0.5/x/",
        "https://192.168.0.1/x/",
        "https://172.16.5.5/x/",
        "https://100.64.1.1/x/",
        "https://router.local/x/",
        "https://2130706433/x/",
        "https://0x7f.0.0.1/x/",
        "https://user:pw@127.0.0.1/x/",
    ] {
        let e = fetch_unlocked(&svc, key, Some(true)).unwrap_err();
        assert!(
            matches!(e, WebError::Blocked(_) | WebError::BadUrl(_)),
            "{key}: {e:?}"
        );
        assert!(matches!(e.http_status(), 400 | 502), "{key}");
    }
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);
}

// ----- capability -------------------------------------------------------------------------

#[test]
fn web_disabled_and_offline_report_why_and_open_no_sockets() {
    let off = rig_with(
        WebConfig {
            disabled: Some("web access is turned off (--no-web)".into()),
        },
        true,
        |_| ok(GOOD),
    );
    let st = off.hub.status();
    assert!(!st.web_capable && st.reason.unwrap().contains("--no-web"));
    assert!(matches!(
        off.hub.fetch("https://a.example/x/", Some(true)),
        Err(WebError::Disabled(_))
    ));
    assert!(matches!(
        off.hub.prepare("https://a.example/x/"),
        Err(WebError::Disabled(_))
    ));
    assert_eq!(off.calls.load(Ordering::SeqCst), 0);

    let down = rig_with(WebConfig::default(), false, |_| ok(GOOD));
    let st = down.hub.status();
    assert!(!st.web_capable && st.reason.unwrap().contains("no network"));
    assert!(matches!(
        down.hub.fetch("https://a.example/x/", Some(true)),
        Err(WebError::Offline(_))
    ));
    assert_eq!(down.calls.load(Ordering::SeqCst), 0);
    assert!(rig(GOOD).hub.status().web_capable);
}

// ----- one in flight ---------------------------------------------------------------------

struct Gate {
    entered: Sender<()>,
    release: Mutex<Receiver<()>>,
}

#[test]
fn web_second_concurrent_fetch_is_429() {
    let (entered_tx, entered_rx) = channel();
    let (release_tx, release_rx) = channel();
    let gate = Arc::new(Gate {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let g = Arc::clone(&gate);
    let r = rig_with(WebConfig::default(), true, move |_| {
        g.entered.send(()).unwrap();
        g.release.lock().unwrap().recv().unwrap();
        ok(GOOD)
    });
    let hub = Arc::clone(&r.hub);
    let first = std::thread::spawn(move || hub.fetch("https://a.example/x/", Some(true)));
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let e = r.hub.fetch("https://b.example/y/", Some(true)).unwrap_err();
    assert_eq!(e, WebError::Busy);
    assert_eq!(e.http_status(), 429);
    assert_eq!(
        r.calls.load(Ordering::SeqCst),
        1,
        "the second request opened no socket"
    );
    release_tx.send(()).unwrap();
    assert!(first.join().unwrap().is_ok());
    // The slot is free again.
    release_tx.send(()).unwrap();
    assert!(r.hub.fetch("https://b.example/y/", Some(true)).is_ok());
}

// ----- overview -----------------------------------------------------------------------------

#[test]
fn web_overview_lists_platforms_in_artist_order_with_correct_verbs() {
    let r = rig(GOOD);
    let out = r
        .hub
        .fetch("https://artist.example/x/", Some(true))
        .unwrap();
    let o = &out.overview;
    assert_eq!(o.title.as_deref(), Some("The Album"));
    assert_eq!(o.artist.as_deref(), Some("The Band"));
    assert_eq!(o.profile, "music");
    let names: Vec<&str> = o.platforms.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["Zed", "Alpha", "Mid", "Spin"],
        "artist order, not sorted by type"
    );
    let verbs: Vec<&str> = o.platforms.iter().map(|p| p.verb.as_str()).collect();
    assert_eq!(verbs, ["stream", "buy", "listen to", "spin"]);
    assert_eq!(o.platforms[1].label, "buy The Album on Alpha");
    assert_eq!(o.platforms[0].platform_type, "streaming");
    assert_eq!(o.tracks.len(), 2);
    assert_eq!(o.tracks[0].title.as_deref(), Some("One"));
    assert_eq!(o.tracks[0].expected_length_ms, Some(185_000));
    assert_eq!(o.total_expected_ms, Some(185_000 + 240_000));
    assert_eq!(
        o.cover_url.as_deref(),
        Some("https://cdn.example/cover.jpg")
    );
    assert_eq!(o.further_links[0].url, "https://band.example/");
    assert!(o.free_download.available && !o.free_download.all_items);
    assert_eq!(o.free_download.urls, ["https://dl.example/1.mp3"]);
    // Only the metadata URL was requested: the cover is not fetched in this package.
    assert_eq!(r.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn web_result_is_held_in_memory_by_token_only() {
    let r = rig(GOOD);
    let (tmp, svc) = service(&r);
    let before: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let out = fetch_unlocked(&svc, "https://artist.example/x/", Some(true)).unwrap();
    // Nothing was written to the library.
    let after: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before.len(), after.len());
    assert!(!tmp.path().join("web.noudata").exists());
    // Retrievable by token, sanitised, without the key.
    let held = r.hub.held(&out.token).unwrap();
    assert_eq!(held.noubin_key, "https://artist.example/x/");
    assert!(held.doc.noubin_key.is_none());
    assert_eq!(r.hub.session_overview(&out.token).unwrap(), out.overview);
    assert_eq!(
        r.hub
            .session_overview("00000000000000000000000000000000")
            .unwrap_err(),
        WebError::UnknownSession
    );
    assert!(r.hub.discard(&out.token));
    assert!(r.hub.held(&out.token).is_none());
    // Bounded memory: oldest sessions are evicted.
    let tokens: Vec<String> = (0..MAX_SESSIONS + 2)
        .map(|_| {
            r.hub
                .fetch("https://artist.example/x/", Some(true))
                .unwrap()
                .token
        })
        .collect();
    assert!(r.hub.held(&tokens[0]).is_none());
    assert!(r.hub.held(tokens.last().unwrap()).is_some());
}

#[test]
fn web_hostile_metadata_is_sanitised_before_it_is_held_or_shown() {
    const HOSTILE: &str = r#"{
        "playableItemTitle": "Evil\u202E \u0007 <script>alert(1)</script>",
        "noubinKey": "https://evil.example/",
        "releaseData": {
            "credits": {"primaryArtists": [{"primaryArtistName": "'; DROP TABLE playables;--"}]},
            "releasePlatforms": [
                {"platformName": "X", "platformType": "store", "platformURL": "javascript:alert(document.cookie)"},
                {"platformName": "Y", "platformType": "store", "platformURL": "data:text/html,<script>1</script>"},
                {"platformName": "Ok", "platformType": "store", "platformURL": "http://ok.example/buy?a=\"><b>"}
            ]
        },
        "mediaList": [{"mediaTitle": "../../etc/passwd", "localFilePaths": ["/etc/shadow"]}]
    }"#;
    let r = rig(HOSTILE);
    let out = r
        .hub
        .fetch("https://artist.example/x/", Some(true))
        .unwrap();
    let o = &out.overview;
    assert_eq!(
        o.title.as_deref(),
        Some("Evil <script>alert(1)</script>"),
        "kept as inert text"
    );
    assert_eq!(o.platforms.len(), 1);
    assert!(o.platforms[0].url.starts_with("https://ok.example/buy?a="));
    assert!(!o.platforms[0].url.contains(['"', '<', '>']));
    assert_eq!(o.artist.as_deref(), Some("'; DROP TABLE playables;--"));
    assert_eq!(
        o.tracks[0].title.as_deref(),
        Some("../../etc/passwd"),
        "display text only; never a path"
    );
    let held = r.hub.held(&out.token).unwrap();
    assert!(held.doc.media_list[0].local_file_paths.is_none());
    assert!(!held.warnings.is_empty());
}

#[test]
fn web_upstream_failures_map_to_stable_codes() {
    let cases: Vec<(Result<HopResponse, TransportError>, &str, u16)> = vec![
        (Err(TransportError::Timeout), "timeout", 502),
        (
            Err(TransportError::Connect("dns".into())),
            "connect_failed",
            502,
        ),
        (
            Err(TransportError::Blocked("10.0.0.1".into())),
            "blocked_target",
            502,
        ),
        (
            Ok(HopResponse {
                status: 404,
                content_type: Some("application/json".into()),
                location: None,
                content_length: None,
                body: Box::new(Cursor::new(b"{\"message\":\"Removed by owner\"}".to_vec())),
            }),
            "not_found",
            502,
        ),
    ];
    for (i, (resp, code, status)) in cases.into_iter().enumerate() {
        let cell = Mutex::new(Some(resp));
        let r = rig_with(WebConfig::default(), true, move |_| {
            cell.lock().unwrap().take().unwrap()
        });
        let e = r.hub.fetch("https://a.example/x/", Some(true)).unwrap_err();
        assert_eq!(e.code(), code, "case {i}");
        assert_eq!(e.http_status(), status);
        if let WebError::NotFound(m) = &e {
            assert_eq!(m.as_deref(), Some("Removed by owner"));
        }
    }
}

// ----- HTTP layer -------------------------------------------------------------------------------

fn start_portal(r: &Rig) -> (u16, tempfile::TempDir, RecordingIpc) {
    let (tmp, svc) = service(r);
    let ipc = RecordingIpc::new();
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        library_root: None,
        secrets_dir: None,
        portal_url: None,
        ..ServerConfig::default()
    };
    common::serve_open(cfg, svc);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    (port, tmp, ipc)
}

struct Resp {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_default()
    }
}

fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    let mut has_host = false;
    for (k, v) in headers {
        has_host |= k.eq_ignore_ascii_case("host");
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has_host {
        req.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head");
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    Resp {
        status: head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        head,
        body: raw[split + 4..].to_vec(),
    }
}

const JSON: (&str, &str) = ("Content-Type", "application/json");

#[test]
fn web_http_consent_gate_and_flow() {
    let r = rig(GOOD);
    let (port, _t, _ipc) = start_portal(&r);
    let body =
        |consent: &str| format!(r#"{{"noubinKey": "https://artist.example/noubin/x/"{consent}}}"#);

    // No consent, false consent, wrong-typed consent: refused, zero sockets.
    let a = http(port, "POST", "/api/web/fetch", &[JSON], body("").as_bytes());
    assert_eq!(
        (a.status, a.json()["code"].as_str()),
        (403, Some("consent_required"))
    );
    let b = http(
        port,
        "POST",
        "/api/web/fetch",
        &[JSON],
        body(r#", "consent": false"#).as_bytes(),
    );
    assert_eq!(b.status, 403);
    let c = http(
        port,
        "POST",
        "/api/web/fetch",
        &[JSON],
        body(r#", "consent": "true""#).as_bytes(),
    );
    assert_eq!(c.status, 400);
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);

    // prepare: nothing sent.
    let p = http(
        port,
        "POST",
        "/api/web/prepare",
        &[JSON],
        body("").as_bytes(),
    );
    assert_eq!(p.status, 200);
    assert_eq!(
        p.json()["fetch_url"],
        "https://artist.example/noubin/x/web.noudata"
    );
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);

    // fetch with consent.
    let f = http(
        port,
        "POST",
        "/api/web/fetch",
        &[JSON],
        body(r#", "consent": true"#).as_bytes(),
    );
    assert_eq!(f.status, 200, "{}", String::from_utf8_lossy(&f.body));
    assert_eq!(r.calls.load(Ordering::SeqCst), 1);
    let v = f.json();
    assert_eq!(v["overview"]["platforms"][0]["name"], "Zed");
    let token = v["token"].as_str().unwrap().to_string();

    // Session read + delete; bad tokens never reach the store.
    let g = http(port, "GET", &format!("/api/web/session/{token}"), &[], b"");
    assert_eq!(g.status, 200);
    assert_eq!(
        http(port, "GET", "/api/web/session/../../etc/passwd", &[], b"").status,
        400 // SECFIX_02: a dot-segment target is refused before routing
    );
    assert_eq!(
        http(port, "GET", "/api/web/session/ZZ", &[], b"").status,
        404
    );
    let d = http(
        port,
        "DELETE",
        &format!("/api/web/session/{token}"),
        &[("X-Plippa", "1")],
        b"",
    );
    assert_eq!(d.status, 200);
    assert_eq!(
        http(port, "GET", &format!("/api/web/session/{token}"), &[], b"").status,
        404
    );

    // Local hit over HTTP: 409, still one call in total.
    let l = http(
        port,
        "POST",
        "/api/web/fetch",
        &[JSON],
        br#"{"noubinKey": "https://have.example/a", "consent": true}"#,
    );
    assert_eq!(
        (l.status, l.json()["code"].as_str()),
        (409, Some("local_match"))
    );
    assert_eq!(r.calls.load(Ordering::SeqCst), 1);

    // Status reports capability.
    let s = http(port, "GET", "/api/web/status", &[], b"");
    assert_eq!(s.json()["web_capable"], true);
    let st = http(port, "GET", "/api/status", &[], b"").json();
    assert_eq!(st["web_capable"], true);
}

#[test]
fn web_http_csrf_and_content_type_guards() {
    let r = rig(GOOD);
    let (port, _t, _ipc) = start_portal(&r);
    let ok_body = br#"{"noubinKey": "https://artist.example/x/", "consent": true}"#;

    // Cross-site form post (text/plain or urlencoded) is refused.
    for ct in [
        "text/plain",
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
    ] {
        let x = http(
            port,
            "POST",
            "/api/web/fetch",
            &[("Content-Type", ct)],
            ok_body,
        );
        assert_eq!(x.status, 415, "{ct}");
    }
    // JSON but from another origin (also DNS rebinding: Origin != Host).
    let x = http(
        port,
        "POST",
        "/api/web/fetch",
        &[JSON, ("Origin", "http://evil.example")],
        ok_body,
    );
    assert_eq!(x.status, 403);
    let x = http(
        port,
        "POST",
        "/api/web/fetch",
        &[
            JSON,
            ("Origin", "http://attacker.example:8787"),
            ("Host", "attacker.example:8787"),
        ],
        ok_body,
    );
    // A rebinding page has Origin == Host == its own public name: the Host allow-list stops it,
    // for reads and writes alike.
    assert_eq!(x.status, 403, "public DNS name in Host is refused");
    assert_eq!(
        http(
            port,
            "GET",
            "/api/status",
            &[("Host", "attacker.example:8787")],
            b""
        )
        .status,
        403
    );
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);
    // Same-origin request from the portal page itself (IP literal or a local name).
    let own = format!("http://127.0.0.1:{port}");
    assert_eq!(
        http(
            port,
            "POST",
            "/api/web/fetch",
            &[JSON, ("Origin", &own)],
            ok_body
        )
        .status,
        200
    );
    assert_eq!(
        http(
            port,
            "POST",
            "/api/web/fetch",
            &[
                JSON,
                ("Origin", "http://plippa.local:8787"),
                ("Host", "plippa.local:8787")
            ],
            ok_body
        )
        .status,
        200
    );
    assert_eq!(r.calls.load(Ordering::SeqCst), 2);

    // Malformed / oversized / non-object bodies: 400 / 413, never a panic.
    assert_eq!(
        http(port, "POST", "/api/web/prepare", &[JSON], b"{not json").status,
        400
    );
    assert_eq!(
        http(port, "POST", "/api/web/prepare", &[JSON], b"[1,2]").status,
        400
    );
    assert_eq!(
        http(port, "POST", "/api/web/prepare", &[JSON], b"").status,
        400
    );
    let big = format!(r#"{{"noubinKey": "{}"}}"#, "a".repeat(10_000));
    assert_eq!(
        http(port, "POST", "/api/web/prepare", &[JSON], big.as_bytes()).status,
        413
    );

    // The guard covers the older state-changing endpoints too.
    for (m, p) in [
        ("POST", "/api/transport/stop"),
        ("POST", "/api/transport/unload"),
        ("POST", "/api/rescan"),
        ("POST", "/api/playables/1/play"),
        ("DELETE", "/api/playlists/1"),
    ] {
        assert_eq!(http(port, m, p, &[], b"").status, 415, "{m} {p}");
        assert_eq!(
            http(
                port,
                m,
                p,
                &[("X-Plippa", "1"), ("Origin", "http://evil.example")],
                b""
            )
            .status,
            403,
            "{m} {p}"
        );
        assert_ne!(
            http(port, m, p, &[("X-Plippa", "1")], b"").status,
            415,
            "{m} {p}"
        );
    }
    // GET stays open.
    assert_eq!(http(port, "GET", "/api/status", &[], b"").status, 200);
}

#[test]
fn web_http_security_headers_on_every_response() {
    let r = rig(GOOD);
    let (port, _t, _ipc) = start_portal(&r);
    for (m, p, h) in [
        ("GET", "/", vec![]),
        ("GET", "/app.js", vec![]),
        ("GET", "/styles.css", vec![]),
        ("GET", "/api/status", vec![]),
        ("GET", "/nope", vec![]),
        (
            "POST",
            "/api/web/fetch",
            vec![("Content-Type", "text/plain")],
        ),
        ("POST", "/api/web/prepare", vec![JSON]),
    ] {
        let x = http(port, m, p, &h, b"{}");
        let csp = x.header("Content-Security-Policy").unwrap_or_default();
        for part in [
            "default-src 'self'",
            "img-src 'self' data:",
            "object-src 'none'",
            "base-uri 'none'",
            "frame-ancestors 'none'",
        ] {
            assert!(csp.contains(part), "{p}: {csp}");
        }
        assert!(!csp.contains("unsafe-inline") && !csp.contains("unsafe-eval"));
        assert_eq!(
            x.header("X-Content-Type-Options").as_deref(),
            Some("nosniff"),
            "{p}"
        );
        assert_eq!(
            x.header("Referrer-Policy").as_deref(),
            Some("no-referrer"),
            "{p}"
        );
    }
    // The page itself has no inline script or style that the CSP would block.
    let page = String::from_utf8(http(port, "GET", "/", &[], b"").body).unwrap();
    assert!(!page.contains("onclick=") && !page.contains("<style"));
    assert!(!page
        .split("<script")
        .skip(1)
        .any(|s| !s.trim_start().starts_with("src=")));
}

#[test]
fn web_status_shows_unmatched_url_tap_banner_data() {
    let r = rig(GOOD);
    let ipc = RecordingIpc::new();
    let mut svc = ServerService::start(
        None,
        Box::new(ipc.clone()),
        ServerService::default_mock_associate(),
    );
    svc.set_web_hub(Arc::clone(&r.hub));
    assert!(svc.status().unmatched_noubin.is_none());
    ipc.inject_unmatched(UnmatchedNoubin {
        noubin_key: "https://artist.example/noubin/album?ref=nfc".into(),
        normalised_key: "artist.example/noubin/album/".into(),
        seq: 4,
        candidate_urls: vec!["https://artist.example/noubin/album?ref=nfc".into()],
    });
    let st = svc.status();
    let u = st.unmatched_noubin.unwrap();
    assert_eq!(
        (u.noubin_key.as_str(), u.seq),
        ("https://artist.example/noubin/album?ref=nfc", 4)
    );
    assert!(st.web_capable && st.web_reason.is_none());
    // A key with control characters is not offered.
    ipc.inject_unmatched(UnmatchedNoubin {
        noubin_key: "https://a.example/\u{202E}x".into(),
        normalised_key: String::new(),
        seq: 5,
        candidate_urls: Vec::new(),
    });
    assert!(svc.status().unmatched_noubin.is_none());
    assert_eq!(
        r.calls.load(Ordering::SeqCst),
        0,
        "status never contacts the network"
    );
}
