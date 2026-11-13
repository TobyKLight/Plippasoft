//! SECFIX_02 (SRV-01, SRV-08): the web portal is behind a PIN, can be switched off for real, and
//! the on-device UI keeps working through a separate loopback control listener.
//!
//! Everything here runs over real sockets against `serve_with_access`, with a manual clock so the
//! lockouts are tested without waiting. The state lives in a throw-away secrets directory.

use plippa_core::Capabilities;
use plippa_server::{
    serve_with_access, HttpLimits, LockoutPolicy, ManualClock, PortalAccess, RecordingIpc,
    ServerConfig, ServerService,
};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Rig {
    portal: u16,
    control: u16,
    token: String,
    clock: Arc<ManualClock>,
    access: Arc<PortalAccess>,
    svc: Arc<Mutex<ServerService>>,
    secrets: PathBuf,
    root: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    _tmp: Option<tempfile::TempDir>,
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Rig {
    /// A device in factory default state (no state file).
    fn factory() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let mut rig = Rig::start_in(tmp.path().to_path_buf(), None);
        rig._tmp = Some(tmp);
        rig
    }

    /// Start the server over `base` (library, secrets and token live in it). Reusing the directory
    /// of an earlier rig is a restart of the same device.
    fn start_in(base: PathBuf, clock: Option<Arc<ManualClock>>) -> Rig {
        Rig::start_on(base, clock, None)
    }

    /// [`Rig::start_in`] on a chosen portal port (a test that holds the port busy).
    fn start_on(base: PathBuf, clock: Option<Arc<ManualClock>>, portal_port: Option<u16>) -> Rig {
        let root = base.join("library");
        let secrets = base.join("secrets");
        let token_path = base.join("portal.token");
        fs::create_dir_all(&root).unwrap();
        let svc = Arc::new(Mutex::new(ServerService::start(
            Some(root.clone()),
            Box::new(RecordingIpc::new()),
            ServerService::default_mock_associate(),
        )));
        let clock = clock.unwrap_or_else(|| Arc::new(ManualClock::new()));
        let access = Arc::new(PortalAccess::open_with(
            Some(&secrets),
            clock.clone(),
            LockoutPolicy::default(),
        ));
        // SECFIX_03: this suite drives the plain-HTTP portal, so the rig states that choice in
        // the state file before the listener starts (the HTTPS listener is `portal_tls.rs`).
        // A rig whose state cannot be kept at all (`usable` is `Err`) refuses the change; that
        // is the case the suite tests, and such a portal never listens, on any protocol.
        let _ = access.set_https_enabled(false);
        let portal = portal_port.unwrap_or_else(free_port);
        let control = free_port();
        let cfg = ServerConfig {
            bind: format!("127.0.0.1:{portal}").parse().unwrap(),
            control_bind: Some(format!("127.0.0.1:{control}").parse().unwrap()),
            control_token_path: Some(token_path.clone()),
            library_root: Some(root.clone()),
            secrets_dir: Some(secrets.clone()),
            portal_url: None,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (svc, stop, access) = (Arc::clone(&svc), Arc::clone(&stop), Arc::clone(&access));
            std::thread::spawn(move || {
                let _ = serve_with_access(cfg, svc, Some(stop), HttpLimits::default(), access);
            })
        };
        let t = Instant::now();
        while TcpStream::connect(("127.0.0.1", control)).is_err() || !token_path.exists() {
            assert!(t.elapsed() < Duration::from_secs(10), "control listener did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        let token = fs::read_to_string(&token_path).unwrap();
        Rig {
            portal,
            control,
            token,
            clock,
            access,
            svc,
            secrets,
            root,
            stop,
            thread: Some(thread),
            _tmp: None,
        }
    }

    /// Stop the server and keep its directory alive (the caller restarts it on its own terms).
    fn restart_hold(mut self) -> Rig {
        self.shutdown();
        let base = self.root.parent().unwrap().to_path_buf();
        let clock = Arc::clone(&self.clock);
        let tmp = self._tmp.take();
        // A stopped placeholder that only carries the directory guard and the clock.
        let mut held = Rig::start_on(base, Some(clock), None);
        held._tmp = tmp;
        held.shutdown();
        held
    }

    /// A power cycle: stop the server, then start it again over the same directory.
    fn restart(mut self) -> Rig {
        self.shutdown();
        let base = self.root.parent().unwrap().to_path_buf();
        let clock = Arc::clone(&self.clock);
        let tmp = self._tmp.take();
        let mut next = Rig::start_in(base, Some(clock));
        next._tmp = tmp;
        next
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    // ---- control listener helpers ---------------------------------------------------------

    fn ctl(&self, method: &str, path: &str, body: &str) -> (u16, String, String) {
        let tok = self.token.clone();
        let host = format!("127.0.0.1:{}", self.control);
        let headers = [
            ("Host", host.as_str()),
            ("X-Plippa-Local", tok.as_str()),
            ("X-Plippa", "1"),
            ("Content-Type", "application/json"),
        ];
        raw(self.control, method, path, &headers, body.as_bytes(), None)
    }

    fn settings(&self) -> serde_json::Value {
        let (st, _, body) = self.ctl("GET", "/api/portal/settings", "");
        assert_eq!(st, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    fn set(&self, body: &str) -> (u16, serde_json::Value) {
        let (st, _, text) = self.ctl("POST", "/api/portal/settings", body);
        (st, serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    fn enable(&self) -> String {
        let (st, v) = self.set(r#"{"enabled":true}"#);
        assert_eq!(st, 200, "{v}");
        v["pin"].as_str().expect("a PIN after enabling").to_string()
    }

    // ---- portal helpers ---------------------------------------------------------------------

    /// A request to the portal from a browser-like client, with optional extra headers.
    fn web(&self, method: &str, path: &str, extra: &[(&str, &str)], body: &str) -> (u16, String, String) {
        let host = format!("127.0.0.1:{}", self.portal);
        let mut headers = vec![("Host", host.as_str()), ("X-Plippa", "1")];
        headers.extend_from_slice(extra);
        raw(self.portal, method, path, &headers, body.as_bytes(), None)
    }

    fn login(&self, pin: &str) -> (u16, String, String) {
        self.web(
            "POST",
            "/api/portal/login",
            &[("Content-Type", "application/json")],
            &format!(r#"{{"pin":"{pin}"}}"#),
        )
    }

    /// Sign in with the right PIN and return the `Cookie` header value.
    fn sign_in(&self, pin: &str) -> String {
        let (st, head, body) = self.login(pin);
        assert_eq!(st, 200, "{head} {body}");
        cookie_pair(&head).expect("a session cookie")
    }

    fn portal_refuses_connections(&self) -> bool {
        TcpStream::connect(("127.0.0.1", self.portal)).is_err()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn cookie_pair(head: &str) -> Option<String> {
    head.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|l| l.split_once(':'))
        .map(|(_, v)| v.trim().split(';').next().unwrap_or("").to_string())
}

fn set_cookie_line(head: &str) -> String {
    head.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap_or("")
        .to_string()
}

/// One request, `Connection: close`. Returns (status, head, body text).
fn raw(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    source: Option<&str>,
) -> (u16, String, String) {
    let mut s = connect_from(port, source);
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nConnection: close\r\n{extra}Content-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap();
    let _ = s.write_all(body);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no response to {method} {path}: {:?}", String::from_utf8_lossy(&out)));
    let head = String::from_utf8_lossy(&out[..split]).to_string();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head, String::from_utf8_lossy(&out[split + 4..]).into_owned())
}

/// Connect to 127.0.0.1:`port`, optionally from another loopback source address (every
/// `127.x.y.z` is local on Linux), so a test can be "many clients".
fn connect_from(port: u16, source: Option<&str>) -> TcpStream {
    let Some(source) = source else {
        return TcpStream::connect(("127.0.0.1", port)).expect("connect");
    };
    use std::os::fd::FromRawFd;
    let src: std::net::Ipv4Addr = source.parse().unwrap();
    // SAFETY: plain socket calls; the fd is handed to TcpStream, which owns and closes it.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        assert!(fd >= 0, "socket");
        let mut local: libc::sockaddr_in = std::mem::zeroed();
        local.sin_family = libc::AF_INET as libc::sa_family_t;
        local.sin_addr.s_addr = u32::from(src).to_be();
        let rc = libc::bind(
            fd,
            &local as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        assert_eq!(rc, 0, "bind source {source}");
        let mut remote: libc::sockaddr_in = std::mem::zeroed();
        remote.sin_family = libc::AF_INET as libc::sa_family_t;
        remote.sin_port = port.to_be();
        remote.sin_addr.s_addr = u32::from(std::net::Ipv4Addr::LOCALHOST).to_be();
        let rc = libc::connect(
            fd,
            &remote as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        assert_eq!(rc, 0, "connect from {source}");
        TcpStream::from_raw_fd(fd)
    }
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn mode_of(p: &Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

// ---------------------------------------------------------------------------
// The review's attack
// ---------------------------------------------------------------------------

#[test]
fn a_curl_without_a_session_cannot_delete_read_or_play() {
    let rig = Rig::factory();
    rig.enable();
    // The old attack: the right Host and `X-Plippa: 1`, nothing else.
    let attacks: &[(&str, &str, &str)] = &[
        ("GET", "/api/playables", ""),
        ("GET", "/api/now-playing", ""),
        ("GET", "/api/portal/url", ""),
        ("DELETE", "/api/collections/1", ""),
        ("POST", "/api/transport/play-pause", ""),
        ("POST", "/api/media?kind=music&filename=a.mp3", "RIFFxxxx"),
        ("GET", "/api/stores/bandcamp/collection", ""),
        ("POST", "/api/stores/bandcamp/connect", "{}"),
        ("POST", "/api/rescan", ""),
    ];
    for (method, path, body) in attacks {
        let (st, head, text) = rig.web(method, path, &[], body);
        assert_eq!(st, 401, "{method} {path}: {head} {text}");
        assert_eq!(json(&text)["code"], "pin_required", "{method} {path}");
    }
    // A made-up or empty session does not help either.
    for cookie in ["plippa_session=00000000000000000000000000000000", "plippa_session=", "other=1"] {
        let (st, _, _) = rig.web("GET", "/api/playables", &[("Cookie", cookie)], "");
        assert_eq!(st, 401, "{cookie}");
    }
    // The page itself is public so it can show the sign-in form.
    let (st, _, html) = rig.web("GET", "/", &[], "");
    assert_eq!(st, 200);
    assert!(html.contains("<html"), "the portal page is served");
}

#[test]
fn the_right_pin_signs_in_and_the_cookie_works() {
    let rig = Rig::factory();
    let pin = rig.enable();
    assert_eq!(pin.len(), 6);
    let (st, head, body) = rig.login(&pin);
    assert_eq!(st, 200, "{head} {body}");
    let line = set_cookie_line(&head);
    assert!(line.contains("plippa_session="), "{line}");
    assert!(line.contains("HttpOnly"), "{line}");
    assert!(line.contains("SameSite=Strict"), "{line}");
    assert!(line.contains("Path=/"), "{line}");
    assert!(line.contains("Max-Age=7776000"), "90 days: {line}");
    assert!(!line.contains("Secure"), "plain HTTP must not say Secure: {line}");
    let cookie = cookie_pair(&head).unwrap();
    let (st, _, text) = rig.web("GET", "/api/playables", &[("Cookie", &cookie)], "");
    assert_eq!(st, 200, "{text}");
    // Signing out ends it.
    let (st, head, _) = rig.web("POST", "/api/portal/logout", &[("Cookie", &cookie)], "{}");
    assert_eq!(st, 200);
    assert!(set_cookie_line(&head).contains("Max-Age=0"));
    let (st, _, _) = rig.web("GET", "/api/playables", &[("Cookie", &cookie)], "");
    assert_eq!(st, 401, "the signed-out session is dead");
}

#[test]
fn a_wrong_pin_is_refused_in_constant_looking_time() {
    let rig = Rig::factory();
    let pin = rig.enable();
    // Different kinds of wrong: right length, a prefix, an extension, empty, junk. They are all
    // the same answer: no hint about how close a guess was.
    let near = if pin == "000000" { "000001".to_string() } else { "000000".to_string() };
    let guesses = [near, pin[..5].to_string(), format!("{pin}0"), String::new(), "abcdef".into()];
    let mut answers = Vec::new();
    for g in &guesses {
        let (st, head, body) = rig.login(g);
        assert_eq!(st, 401, "{g:?}: {head} {body}");
        assert!(cookie_pair(&head).is_none(), "no cookie for a wrong PIN");
        answers.push(body);
    }
    assert!(answers.windows(2).all(|w| w[0] == w[1]), "{answers:?}");
    assert_eq!(json(&answers[0])["code"], "bad_pin");
    // The PIN never appears in an answer.
    assert!(!answers[0].contains(&pin));
}

#[test]
fn repeated_wrong_pins_lock_the_client_out_with_backoff() {
    let rig = Rig::factory();
    let pin = rig.enable();
    let wrong = if pin == "123450" { "123451" } else { "123450" };
    for n in 1..=5 {
        let (st, _, _) = rig.login(wrong);
        assert_eq!(st, 401, "failure {n}");
    }
    let (st, head, body) = rig.login(wrong);
    assert_eq!(st, 429, "{head} {body}");
    assert!(head.to_ascii_lowercase().contains("retry-after: 60"), "{head}");
    let v = json(&body);
    assert_eq!(v["code"], "locked");
    assert_eq!(v["retry_after"], 60);
    // The correct PIN during the lockout is refused too.
    let (st, head, _) = rig.login(&pin);
    assert_eq!(st, 429);
    assert!(cookie_pair(&head).is_none());
    // After the minute the owner is in again.
    rig.clock.advance(Duration::from_secs(61));
    let (st, _, _) = rig.login(&pin);
    assert_eq!(st, 200);
    // The device menu shows the lockout and clears it.
    let (st, head, _) = rig.login(wrong);
    assert_eq!(st, 401, "{head}");
    for _ in 0..4 {
        rig.login(wrong);
    }
    assert_eq!(rig.settings()["locked"], true);
    let (st, v) = rig.set(r#"{"clear_lockout":true}"#);
    assert_eq!(st, 200);
    assert_eq!(v["locked"], false);
    assert_eq!(rig.login(&pin).0, 200);
}

#[test]
fn many_clients_cannot_brute_force_past_the_global_limit() {
    let rig = Rig::factory();
    let pin = rig.enable();
    let wrong = if pin == "123450" { "123451" } else { "123450" };
    // A signed-in owner, before the attack.
    let session = rig.sign_in(&pin);
    // Thirty failures from thirty different addresses: no address ever reaches its own lockout.
    let host = format!("127.0.0.1:{}", rig.portal);
    for n in 0..30 {
        let src = format!("127.0.0.{}", 10 + n);
        let body = format!(r#"{{"pin":"{wrong}"}}"#);
        let headers = [
            ("Host", host.as_str()),
            ("X-Plippa", "1"),
            ("Content-Type", "application/json"),
        ];
        let (st, _, text) = raw(rig.portal, "POST", "/api/portal/login", &headers, body.as_bytes(), Some(&src));
        assert_eq!(st, 401, "{src}: {text}");
    }
    // Now the whole sign-in endpoint is locked, for a brand-new address and the right PIN.
    let body = format!(r#"{{"pin":"{pin}"}}"#);
    let headers = [("Host", host.as_str()), ("X-Plippa", "1"), ("Content-Type", "application/json")];
    let (st, _, text) = raw(rig.portal, "POST", "/api/portal/login", &headers, body.as_bytes(), Some("127.0.0.200"));
    assert_eq!(st, 429, "{text}");
    assert_eq!(json(&text)["retry_after"], 600);
    // A session that already exists is not affected.
    let (st, _, _) = rig.web("GET", "/api/playables", &[("Cookie", &session)], "");
    assert_eq!(st, 200);
    rig.clock.advance(Duration::from_secs(601));
    assert_eq!(rig.login(&pin).0, 200);
}

#[test]
fn pin_not_required_means_open_and_the_status_says_so() {
    let rig = Rig::factory();
    rig.enable();
    assert_eq!(rig.web("GET", "/api/playables", &[], "").0, 401);
    let (st, v) = rig.set(r#"{"pin_required":false}"#);
    assert_eq!(st, 200);
    assert_eq!(v["pin_required"], false);
    assert!(v["pin"].is_string(), "the PIN is kept while it is not required");
    let (st, _, text) = rig.web("GET", "/api/playables", &[], "");
    assert_eq!(st, 200, "{text}");
    let (st, _, text) = rig.web("GET", "/api/status", &[], "");
    assert_eq!(st, 200);
    let status = json(&text);
    assert_eq!(status["pin_required"], false);
    assert_eq!(status["signed_in"], true);
    // Back to the safe state.
    rig.set(r#"{"pin_required":true}"#);
    assert_eq!(rig.web("GET", "/api/playables", &[], "").0, 401);
}

// ---------------------------------------------------------------------------
// Factory default, persistence
// ---------------------------------------------------------------------------

#[test]
fn factory_default_is_off_and_has_no_pin() {
    let rig = Rig::factory();
    let s = rig.settings();
    assert_eq!(s["enabled"], false);
    assert!(s["pin"].is_null(), "{s}");
    assert_eq!(s["available"], true);
    assert_eq!(s["listening"], false);
    assert!(rig.portal_refuses_connections(), "nothing listens on the LAN port");
    // SECFIX_03: the rig records its plain-HTTP choice as state up front, so what "factory
    // default" means here is that the file names nothing open and mints no PIN.
    let state = fs::read_to_string(rig.secrets.join("portal-access.state")).expect("state");
    assert!(state.contains("\"enabled\":false"), "{state}");
    assert!(!state.contains("\"pin\":\""), "{state}");
    assert!(state.contains("\"https_enabled\":false"), "{state}");
    // The control listener answers regardless.
    assert_eq!(rig.ctl("GET", "/api/status", "").0, 200);
}

#[test]
fn first_enable_generates_a_six_digit_pin_that_persists_across_a_restart() {
    let rig = Rig::factory();
    let pin = rig.enable();
    assert!(pin.len() == 6 && pin.bytes().all(|b| b.is_ascii_digit()), "{pin}");
    assert!(!rig.portal_refuses_connections(), "enabling opens the LAN port");
    let cookie = rig.sign_in(&pin);

    let rig = rig.restart();
    // The portal comes back on its own, with the same PIN and the same session.
    assert!(!rig.portal_refuses_connections(), "still enabled after a restart");
    assert_eq!(rig.settings()["pin"], pin.as_str());
    let (st, _, _) = rig.web("GET", "/api/playables", &[("Cookie", &cookie)], "");
    assert_eq!(st, 200, "a session survives a restart");
    assert_eq!(rig.login(&pin).0, 200);
}

#[test]
fn disable_then_enable_keeps_the_pin() {
    let rig = Rig::factory();
    let pin = rig.enable();
    let (st, v) = rig.set(r#"{"enabled":false}"#);
    assert_eq!(st, 200);
    assert_eq!(v["enabled"], false);
    assert_eq!(v["pin"], pin.as_str(), "the PIN is kept while the portal is off");
    assert!(rig.portal_refuses_connections());
    let (st, v) = rig.set(r#"{"enabled":true}"#);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["pin"], pin.as_str());
    assert!(!rig.portal_refuses_connections(), "the port is open again");
    assert_eq!(rig.login(&pin).0, 200);
}

#[test]
fn the_owner_types_their_own_pin_and_old_sessions_end() {
    let rig = Rig::factory();
    let old = rig.enable();
    let cookie = rig.sign_in(&old);
    let (st, v) = rig.set(r#"{"pin":"135790"}"#);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["pin"], "135790");
    assert_eq!(rig.web("GET", "/api/playables", &[("Cookie", &cookie)], "").0, 401, "signed out by the change");
    assert_eq!(rig.login("135790").0, 200);
    // Refused PINs: only the wrong shape. A typed PIN is the owner's: 111111 and 123456 are fine.
    for bad in ["12345", "abcdef", "1234567", ""] {
        let (st, v) = rig.set(&format!(r#"{{"pin":"{bad}"}}"#));
        assert_eq!(st, 400, "{bad}: {v}");
        assert_eq!(v["code"], "bad_pin");
    }
    for any in ["111111", "123456", "000000"] {
        let (st, v) = rig.set(&format!(r#"{{"pin":"{any}"}}"#));
        assert_eq!(st, 200, "{any}: {v}");
        assert_eq!(v["pin"], any);
    }
    rig.set(r#"{"pin":"135790"}"#);
    assert_eq!(rig.settings()["pin"], "135790", "a refused PIN changes nothing");
}

#[test]
fn turning_the_portal_off_closes_the_lan_socket_and_drops_live_connections() {
    let rig = Rig::factory();
    let pin = rig.enable();
    let cookie = rig.sign_in(&pin);

    // A keep-alive connection with a request already answered on it.
    let mut live = TcpStream::connect(("127.0.0.1", rig.portal)).unwrap();
    live.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let ask = format!(
        "GET /api/now-playing HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nCookie: {cookie}\r\n\r\n",
        rig.portal
    );
    live.write_all(ask.as_bytes()).unwrap();
    let first = read_one_response(&mut live);
    assert!(first.starts_with("HTTP/1.1 200"), "{first}");

    // The device owner switches the portal off.
    let (st, v) = rig.set(r#"{"enabled":false}"#);
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["listening"], false);

    // New connections are refused at once ...
    assert!(rig.portal_refuses_connections(), "the LAN socket is closed");
    // ... the connection that was open is gone ...
    let _ = live.write_all(ask.as_bytes());
    let mut rest = Vec::new();
    let read = live.read_to_end(&mut rest);
    assert!(
        read.is_err() || rest.is_empty(),
        "the live connection must not be answered after the portal went off: {:?}",
        String::from_utf8_lossy(&rest)
    );
    // ... and the device UI's listener carries on.
    assert_eq!(rig.ctl("GET", "/api/status", "").0, 200);
}

/// Read exactly one HTTP/1.1 response (head plus `Content-Length` body) from a kept-alive socket.
fn read_one_response(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = s.read(&mut byte).expect("read head");
        assert!(n > 0, "closed before a full head: {:?}", String::from_utf8_lossy(&buf));
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let len = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).expect("read body");
    head + &String::from_utf8_lossy(&body)
}

// ---------------------------------------------------------------------------
// Two listeners
// ---------------------------------------------------------------------------

#[test]
fn the_control_listener_requires_its_token_and_the_portal_listener_ignores_it() {
    let rig = Rig::factory();
    rig.enable();
    let host = format!("127.0.0.1:{}", rig.control);
    // No token, a wrong token, an empty one: nothing, not even static files.
    for headers in [
        vec![("Host", host.as_str()), ("X-Plippa", "1")],
        vec![("Host", host.as_str()), ("X-Plippa", "1"), ("X-Plippa-Local", "nope")],
        vec![("Host", host.as_str()), ("X-Plippa", "1"), ("X-Plippa-Local", "")],
    ] {
        for path in ["/api/status", "/api/playables", "/", "/api/portal/settings"] {
            let (st, _, text) = raw(rig.control, "GET", path, &headers, b"", None);
            assert_eq!(st, 401, "{path}: {text}");
            assert_eq!(json(&text)["code"], "local_token");
        }
    }
    // The right token opens the whole API, with the PIN required or not.
    assert_eq!(rig.ctl("GET", "/api/playables", "").0, 200);
    assert_eq!(rig.ctl("GET", "/api/portal/url", "").0, 200);
    // The token means nothing on the LAN listener.
    let token = rig.token.clone();
    let (st, _, _) = rig.web("GET", "/api/playables", &[("X-Plippa-Local", &token)], "");
    assert_eq!(st, 401);
    // Sign-in is a portal thing: the control listener has no such route.
    assert_eq!(rig.ctl("POST", "/api/portal/login", r#"{"pin":"000000"}"#).0, 404);
    // The token is a secret in a private file.
    assert_eq!(token.len(), 64);
}

#[test]
fn portal_settings_routes_do_not_exist_on_the_lan_listener() {
    let rig = Rig::factory();
    let pin = rig.enable();
    let cookie = rig.sign_in(&pin);
    let token = rig.token.clone();
    for (method, body) in [("GET", ""), ("POST", r#"{"enabled":false}"#), ("POST", r#"{"pin":"135790"}"#)] {
        for extra in [
            vec![],
            vec![("Cookie", cookie.as_str())],
            vec![("Cookie", cookie.as_str()), ("X-Plippa-Local", token.as_str())],
        ] {
            let (st, _, text) = rig.web(method, "/api/portal/settings", &extra, body);
            // Signed out callers are told 401 by the gate before routing; everyone who got past it
            // finds nothing. Neither can read the PIN or change the switch.
            assert!(st == 404 || st == 401, "{method} {extra:?}: {st} {text}");
            assert!(!text.contains(&pin), "the PIN must never travel on the LAN listener");
        }
    }
    let (st, _, _) = rig.web("GET", "/api/portal/settings", &[("Cookie", &cookie)], "");
    assert_eq!(st, 404, "even signed in");
    assert_eq!(rig.settings()["enabled"], true, "nothing changed");
    assert_eq!(rig.settings()["pin"], pin.as_str());
}

// ---------------------------------------------------------------------------
// Status (SRV-08)
// ---------------------------------------------------------------------------

#[test]
fn an_unauthenticated_status_reveals_only_the_minimum() {
    let rig = Rig::factory();
    let pin = rig.enable();
    let (st, _, text) = rig.web("GET", "/api/status", &[], "");
    assert_eq!(st, 200, "{text}");
    let v = json(&text);
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    // `https` (SECFIX_03) is the one added field: the page keeps its "no HTTPS" warning up
    // from it, and it says nothing about the machine.
    assert_eq!(
        keys,
        ["available", "https", "ipc_live", "pin_required", "signed_in"],
        "{text}"
    );
    assert_eq!(v["signed_in"], false);
    assert_eq!(v["pin_required"], true);
    // This rig deliberately serves the portal in the clear (the rest of this file drives the
    // HTTP path), so the page must be told exactly that.
    assert_eq!(v["https"], false);
    let root = rig.root.display().to_string();
    assert!(!text.contains(&root) && !text.contains("tmp"), "{text}");
    assert!(!text.contains(&pin));

    // Signed in: the whole status, but the library is named by its label, never its path.
    let cookie = rig.sign_in(&pin);
    let (_, _, text) = rig.web("GET", "/api/status", &[("Cookie", &cookie)], "");
    let v = json(&text);
    assert_eq!(v["signed_in"], true);
    assert!(v.get("index_warnings").is_some() && v.get("board_id").is_some(), "{text}");
    assert!(v["library_root"].is_null(), "{text}");
    assert!(!text.contains(&root), "the absolute path must not reach a browser: {text}");
    // The device's own listener keeps the path (the UI is on the same machine).
    let (_, _, text) = rig.ctl("GET", "/api/status", "");
    assert!(text.contains(&root), "{text}");
}

#[test]
fn status_is_cached_for_a_second() {
    let rig = Rig::factory();
    rig.enable();
    rig.set(r#"{"pin_required":false}"#);
    let board = |rig: &Rig| json(&rig.web("GET", "/api/status", &[], "").2)["board_id"].as_str().unwrap().to_string();
    let first = board(&rig);
    // Change what the status reports behind the cache's back (not through HTTP).
    rig.svc.lock().unwrap().set_capabilities(Capabilities {
        board_id: "changed-board".into(),
        ..Capabilities::desktop_mock()
    });
    assert_eq!(board(&rig), first, "a second read inside the second is the cached answer");
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(board(&rig), "changed-board", "after the second it is computed again");

    // A state-changing request invalidates at once.
    rig.svc.lock().unwrap().set_capabilities(Capabilities {
        board_id: "changed-again".into(),
        ..Capabilities::desktop_mock()
    });
    assert_eq!(board(&rig), "changed-board");
    let (st, _, _) = rig.web("POST", "/api/rescan", &[], "");
    assert!(st == 200 || st == 503, "{st}");
    assert_eq!(board(&rig), "changed-again", "a rescan (any write) drops the cached status");
}

// ---------------------------------------------------------------------------
// State file, guards
// ---------------------------------------------------------------------------

#[test]
fn the_state_file_is_0600_in_a_0700_dir_and_a_symlinked_state_file_is_refused() {
    let rig = Rig::factory();
    rig.enable();
    assert_eq!(mode_of(&rig.secrets), 0o700);
    assert_eq!(mode_of(&rig.secrets.join("portal-access.state")), 0o600);
    let token_file = rig.secrets.parent().unwrap().join("portal.token");
    assert_eq!(mode_of(&token_file), 0o600, "the control token file is private too");

    // A device whose state file was replaced by a symlink refuses it and cannot be enabled.
    let base = tempfile::tempdir().unwrap();
    let secrets = base.path().join("secrets");
    fs::create_dir_all(&secrets).unwrap();
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700)).unwrap();
    let elsewhere = base.path().join("elsewhere");
    let planted = br#"{"version":1,"enabled":true,"pin_required":false,"pin":"135790","sessions":[]}"#;
    fs::write(&elsewhere, planted).unwrap();
    std::os::unix::fs::symlink(&elsewhere, secrets.join("portal-access.state")).unwrap();
    let rig2 = Rig::start_in(base.path().to_path_buf(), None);
    let s = rig2.settings();
    assert_eq!(s["available"], false, "{s}");
    assert_eq!(s["enabled"], false, "what the link points at is not obeyed");
    assert!(rig2.portal_refuses_connections(), "a planted 'enabled' file opens nothing");
    let (st, v) = rig2.set(r#"{"enabled":true}"#);
    assert_eq!(st, 409, "{v}");
    assert_eq!(v["code"], "unavailable");
    assert!(rig2.portal_refuses_connections());
    assert_eq!(fs::read(&elsewhere).unwrap(), planted, "nothing was written through the link");
}

#[test]
fn the_csrf_and_host_guards_still_hold_with_a_session() {
    let rig = Rig::factory();
    let pin = rig.enable();
    let cookie = rig.sign_in(&pin);
    // A valid session plus a foreign Origin is refused.
    let (st, _, _) = rig.web(
        "POST",
        "/api/rescan",
        &[("Cookie", &cookie), ("Origin", "http://evil.example")],
        "",
    );
    assert_eq!(st, 403);
    // A valid session on a form-style POST (no JSON, no X-Plippa) is refused.
    let host = format!("127.0.0.1:{}", rig.portal);
    let (st, _, _) = raw(
        rig.portal,
        "POST",
        "/api/rescan",
        &[("Host", &host), ("Cookie", &cookie), ("Content-Type", "application/x-www-form-urlencoded")],
        b"a=b",
        None,
    );
    assert_eq!(st, 415);
    // A rebinding Host is refused before the session is even looked at.
    let (st, _, _) = raw(
        rig.portal,
        "GET",
        "/api/playables",
        &[("Host", "rebind.attacker.example"), ("Cookie", &cookie)],
        b"",
        None,
    );
    assert_eq!(st, 403);
    // And the plain request still works.
    let (st, _, _) = rig.web("GET", "/api/playables", &[("Cookie", &cookie)], "");
    assert_eq!(st, 200);
}

#[test]
fn the_sign_in_page_and_script_are_public() {
    let rig = Rig::factory();
    rig.enable();
    let (st, _, body) = rig.web("GET", "/signin.js", &[], "");
    assert_eq!(st, 200);
    assert!(body.contains("Signin"));
    let (_, _, html) = rig.web("GET", "/", &[], "");
    assert!(html.contains("signin-form"));
}

// ---------------------------------------------------------------------------
// The auth gate cannot be walked around (review finding: `//api/...` bypass)
// ---------------------------------------------------------------------------

/// Every `/api/...` literal the router source mentions, so a route added later is probed without
/// anyone remembering to list it. Prefix literals (`"/api/playables/"`) get an id appended.
fn routes_from_the_router_source() -> Vec<String> {
    const FULL: &str = include_str!("../src/http.rs");
    // The router, not its own unit tests (which spell hostile paths on purpose).
    let src = FULL.split("#[cfg(test)]").next().unwrap();
    let mut found = std::collections::BTreeSet::new();
    let mut rest = src;
    while let Some(i) = rest.find("\"/api/") {
        let tail = &rest[i + 1..];
        let end = tail.find('"').unwrap_or(0);
        let lit = &tail[..end];
        rest = &tail[end..];
        let plain = lit.chars().all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c));
        if plain && !lit.contains("..") && !lit.contains("//") {
            if lit.ends_with('/') {
                found.insert(format!("{lit}1"));
                found.insert(format!("{lit}1/x"));
            } else {
                found.insert(lit.to_string());
            }
        }
    }
    // Routes the router matches by path segments rather than by literal prefix.
    for extra in [
        "/api/collections/1",
        "/api/collections/1/covers",
        "/api/collections/1/covers/staged",
        "/api/collections/1/covers/order",
        "/api/items/1/covers",
        "/api/items/1/covers/staged",
        "/api/items/1/cues",
        "/api/items/1/noudata",
        "/api/media/1/analysis/reanalyse",
        "/api/playlists/1",
        "/api/playlists/1/items",
        "/api/waveforms/1",
        "/api/imports/1/files",
        "/api/unite/1/files",
        "/api/stores/bandcamp/collection",
        "/api/stores/bandcamp/connect",
        "/api/nope",
    ] {
        found.insert(extra.to_string());
    }
    let routes: Vec<String> = found.into_iter().collect();
    assert!(routes.len() > 40, "the router source should yield many routes: {}", routes.len());
    routes
}

const METHODS: [&str; 7] = ["GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH", "DELETE"];

/// `p` (canonical, starting `/api/`) and the ways to write it that must never get further.
fn variants(p: &str) -> Vec<(String, bool)> {
    let rest = &p[1..]; // api/...
    let after_api = &p["/api".len()..];
    vec![
        (p.to_string(), true),
        (format!("{p}?x=/"), true),
        (p.to_uppercase(), true),
        (format!("/{p}"), false),
        (format!("//{p}"), false),
        (format!("/./{rest}"), false),
        (format!("/api/../{rest}"), false),
        (format!("/%2f{rest}"), false),
        (format!("/%2F{rest}"), false),
        (format!("/%2e%2e/{rest}"), false),
        (format!("/api/{}", after_api.replacen('/', "//", 1).trim_start_matches('/')).replacen("/api/", "/api//", 1), false),
        (format!("{p}/"), false),
        (format!("{p}%00"), false),
        (format!("{p}#frag"), false),
        (format!("{p}\\x"), false),
    ]
}

fn is_public(method: &str, path: &str) -> bool {
    (method == "GET" && (path == "/api/status" || path == "/app.js" || path == "/"))
        || (method == "POST" && (path == "/api/portal/login" || path == "/api/portal/logout"))
}

/// Send `method path` with the portal's usual headers and no credentials.
fn probe(port: u16, host: &str, method: &str, path: &str, extra: &[(&str, &str)]) -> (u16, String) {
    let mut headers = vec![("Host", host), ("X-Plippa", "1"), ("Content-Type", "application/json")];
    headers.extend_from_slice(extra);
    let body: &[u8] = if matches!(method, "POST" | "PUT" | "PATCH") { b"{}" } else { b"" };
    // `raw` writes the target verbatim: that is the point.
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nConnection: close\r\n{extra}Content-Length: {}\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(req.as_bytes());
    let _ = s.write_all(body);
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, text)
}

fn tree_listing(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                let name = p.strip_prefix(root).unwrap().display().to_string();
                if p.is_dir() {
                    walk(&p, root, out);
                } else if !name.contains("portal-access") && !name.ends_with(".sqlite-wal") && !name.ends_with(".sqlite-shm") {
                    out.push(format!("{name}:{}", e.metadata().map(|m| m.len()).unwrap_or(0)));
                }
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, dir, &mut v);
    v.sort();
    v
}

#[test]
fn no_unauthenticated_route_variant_ever_reaches_a_handler_on_the_lan_listener() {
    let rig = Rig::factory();
    rig.enable(); // PIN required, nobody signed in
    let host = format!("127.0.0.1:{}", rig.portal);
    let before = tree_listing(rig.root.parent().unwrap());
    let sessions = rig.access.session_count();
    let mut sent = 0;
    for route in routes_from_the_router_source() {
        for (variant, canonical) in variants(&route) {
            for method in METHODS {
                // The canonical spelling of a public route is allowed through; everything else
                // is a 401 (canonical) or a 400 (not canonical) and nothing else: never a 2xx,
                // a 202, or a handler's own 4xx.
                let path_only = variant.split('?').next().unwrap();
                if canonical && is_public(method, path_only) {
                    continue;
                }
                let (status, text) = probe(rig.portal, &host, method, &variant, &[]);
                let want = if canonical { 401 } else { 400 };
                assert_eq!(status, want, "{method} {variant}: {text}");
                sent += 1;
            }
        }
    }
    assert!(sent > 3_000, "the enumeration ran: {sent}");
    assert_eq!(tree_listing(rig.root.parent().unwrap()), before, "no side effect on disk");
    assert_eq!(rig.access.session_count(), sessions);
    // The public surface is exactly the page and scripts by exact name.
    for p in ["/", "/index.html", "/app.js", "/signin.js", "/styles.css"] {
        assert_eq!(probe(rig.portal, &host, "GET", p, &[]).0, 200, "{p}");
    }
    for p in ["//app.js", "/app.js/", "/./app.js", "/App.js"] {
        let (st, _) = probe(rig.portal, &host, "GET", p, &[]);
        assert!(st == 400 || st == 401, "{p}: {st}");
    }
}

#[test]
fn the_same_enumeration_against_the_control_listener_without_the_token_gets_nothing() {
    let rig = Rig::factory();
    let host = format!("127.0.0.1:{}", rig.control);
    let before = tree_listing(rig.root.parent().unwrap());
    for route in routes_from_the_router_source() {
        for (variant, canonical) in variants(&route) {
            for method in METHODS {
                let (status, text) = probe(rig.control, &host, method, &variant, &[]);
                let want = if canonical { 401 } else { 400 };
                assert_eq!(status, want, "{method} {variant}: {text}");
            }
        }
    }
    for p in ["/", "/app.js", "/api/status", "/api/portal/settings"] {
        assert_eq!(probe(rig.control, &host, "GET", p, &[]).0, 401, "{p}");
    }
    assert_eq!(tree_listing(rig.root.parent().unwrap()), before);
    // A wrong or foreign-listener credential does not help either.
    let (st, _) = probe(rig.control, &host, "GET", "//api/status", &[("X-Plippa-Local", &rig.token)]);
    assert_eq!(st, 400, "even with the token a non-canonical target is refused");
}

#[test]
fn the_reviewers_double_slash_bypass_is_closed() {
    let rig = Rig::factory();
    rig.enable();
    let host = format!("127.0.0.1:{}", rig.portal);
    for path in [
        "//api/collections/1/covers",
        "///api/collections/1/covers",
        "//api/media/1/analysis/reanalyse",
        "//api/collections/1/covers/staged",
        "//api/items/1/covers",
        "//api/items/1/covers/staged",
    ] {
        for method in ["GET", "PUT", "POST", "DELETE"] {
            let (st, text) = probe(rig.portal, &host, method, path, &[]);
            assert_eq!(st, 400, "{method} {path}: {text}");
        }
    }
    // And the canonical spelling is a 401, as before.
    assert_eq!(probe(rig.portal, &host, "GET", "/api/collections/1/covers", &[]).0, 401);
    // A signed-in browser still gets everything, in the canonical spelling.
    let pin = rig.settings()["pin"].as_str().unwrap().to_string();
    let cookie = rig.sign_in(&pin);
    let (st, _) = probe(rig.portal, &host, "GET", "/api/playables", &[("Cookie", &cookie)]);
    assert_eq!(st, 200);
    // Signed in, the non-canonical spelling is still refused (one path, one meaning).
    let (st, _) = probe(rig.portal, &host, "GET", "//api/playables", &[("Cookie", &cookie)]);
    assert_eq!(st, 400);
}

// ---------------------------------------------------------------------------
// Saving, closing, binding
// ---------------------------------------------------------------------------

#[test]
fn a_failed_state_write_never_stops_off_from_meaning_off() {
    let rig = Rig::factory();
    let pin = rig.enable();
    let cookie = rig.sign_in(&pin);
    // Make the final rename fail: the state file's name is now a non-empty directory.
    let state = rig.secrets.join("portal-access.state");
    fs::remove_file(&state).unwrap();
    fs::create_dir(&state).unwrap();
    fs::write(state.join("keep"), b"x").unwrap();

    let (st, v) = rig.set(r#"{"enabled":false}"#);
    assert_eq!(st, 200, "the switch took effect: {v}");
    assert_eq!(v["enabled"], false);
    assert_eq!(v["listening"], false);
    assert!(v["unsaved"].is_string(), "and says it is not saved: {v}");
    assert!(rig.portal_refuses_connections(), "off is off even though the write failed");
    // Turning it on needs a working write: refused honestly, nothing poisoned.
    let (st, v) = rig.set(r#"{"enabled":true}"#);
    assert_eq!(st, 409, "{v}");
    assert!(rig.portal_refuses_connections());
    // Remove the cause: the periodic retry writes the state out and the menu stops saying so.
    fs::remove_file(state.join("keep")).unwrap();
    fs::remove_dir(&state).unwrap();
    let t = Instant::now();
    while rig.settings()["unsaved"].is_string() {
        assert!(t.elapsed() < Duration::from_secs(15), "the save was never retried");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(state.is_file());
    let (st, v) = rig.set(r#"{"enabled":true}"#);
    assert_eq!(st, 200, "settings work again: {v}");
    assert!(!rig.portal_refuses_connections());
    // Off signed everyone out, so the old cookie is gone, and a restart cannot revive it.
    assert_eq!(rig.web("GET", "/api/playables", &[("Cookie", &cookie)], "").0, 401);
    let rig = rig.restart();
    assert_eq!(rig.web("GET", "/api/playables", &[("Cookie", &cookie)], "").0, 401);
}

#[test]
fn a_busy_port_is_reported_and_retried_and_a_quick_toggle_does_not_race() {
    // Enable while something else holds the port: refused, the switch reverts, honestly.
    let rig = Rig::factory();
    let blocker = TcpListener::bind(("127.0.0.1", rig.portal)).unwrap();
    let (st, v) = rig.set(r#"{"enabled":true}"#);
    assert_eq!(st, 500, "{v}");
    assert_eq!(v["code"], "bind_failed");
    assert_eq!(rig.settings()["enabled"], false);
    drop(blocker);
    assert_eq!(rig.set(r#"{"enabled":true}"#).0, 200, "the port is free again");

    // Disable and enable at the same moment, over and over: neither ever loses the port race.
    let port = rig.control;
    let token = rig.token.clone();
    for round in 0..12 {
        let t = token.clone();
        let off = std::thread::spawn(move || {
            let host = format!("127.0.0.1:{port}");
            raw(
                port,
                "POST",
                "/api/portal/settings",
                &[("Host", &host), ("X-Plippa-Local", &t), ("X-Plippa", "1"), ("Content-Type", "application/json")],
                br#"{"enabled":false}"#,
                None,
            )
            .0
        });
        let (on_status, v) = rig.set(r#"{"enabled":true}"#);
        let off_status = off.join().unwrap();
        assert_eq!(off_status, 200, "round {round}");
        assert_eq!(on_status, 200, "round {round}: an enable must not fail to bind: {v}");
    }
}

#[test]
fn enabled_at_boot_with_the_port_busy_is_shown_honestly_and_retried() {
    let rig = Rig::factory();
    rig.enable();
    let port = rig.portal;
    let base = rig.root.parent().unwrap().to_path_buf();
    let rig = rig.restart_hold();
    let blocker = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let rig = Rig::start_on(base, Some(rig.clock.clone()), Some(port));
    let s = rig.settings();
    assert_eq!(s["enabled"], true);
    assert_eq!(s["listening"], false, "enabled but not listening, and it says so: {s}");
    assert!(s["listen_error"].is_string(), "{s}");
    drop(blocker);
    let t = Instant::now();
    while rig.settings()["listening"] != true {
        assert!(t.elapsed() < Duration::from_secs(30), "no retry after the port was freed");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(!rig.portal_refuses_connections());
    assert!(rig.settings()["listen_error"].is_null());
}

#[test]
fn a_stale_control_token_temp_file_cannot_stop_the_token_being_written() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    // Leftovers named like the old predictable temp name.
    for n in 0..4 {
        fs::write(base.join(format!("portal.tmp{}", std::process::id() + n)), b"stale").unwrap();
    }
    let rig = Rig::start_in(base, None);
    assert_eq!(rig.token.len(), 64);
    assert_eq!(rig.ctl("GET", "/api/status", "").0, 200);
}

#[test]
fn the_session_cookie_is_renewed_when_the_server_slides_the_session() {
    let rig = Rig::factory();
    let pin = rig.enable();
    let cookie = rig.sign_in(&pin);
    let (_, head, _) = rig.web("GET", "/api/playables", &[("Cookie", &cookie)], "");
    assert!(cookie_pair(&head).is_none(), "nothing to renew straight after sign-in");
    rig.clock.advance(Duration::from_secs(25 * 3600));
    let (st, head, _) = rig.web("GET", "/api/playables", &[("Cookie", &cookie)], "");
    assert_eq!(st, 200);
    let renewed = set_cookie_line(&head);
    assert!(renewed.contains(&cookie) && renewed.contains("Max-Age=7776000"), "{renewed}");
    let (_, head, _) = rig.web("GET", "/api/playables", &[("Cookie", &cookie)], "");
    assert!(cookie_pair(&head).is_none(), "once a day at most");
}
