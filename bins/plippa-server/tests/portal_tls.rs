//! SECFIX_03 (SRV-03): the portal over TLS with a per-device certificate.
//!
//! Everything here runs over real loopback sockets, and the client is a real TLS client
//! (`rustls`, the same version and provider the server uses) that **verifies** the certificate
//! against the one on disk. So these tests prove what a browser sees: the chain is valid, the
//! name matches, the certificate is the one whose fingerprint the device shows, and a plain
//! request on the same port is answered with a redirect and nothing else.

use plippa_server::{
    serve_with_access, tls, HttpLimits, LockoutPolicy, ManualClock, PortalAccess, RecordingIpc,
    ServerConfig, ServerService,
};
use rustls::pki_types::{CertificateDer, ServerName};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A portal on loopback, with its state in a throw-away secrets directory.
struct Rig {
    portal: u16,
    control: u16,
    token: String,
    secrets: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    _tmp: tempfile::TempDir,
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Production-shaped wire limits with the waits shortened, so a stalled handshake is tested in
/// under a second instead of fifteen.
fn fast_wire() -> plippa_server::limits::WireLimits {
    plippa_server::limits::WireLimits {
        head_deadline: Duration::from_millis(700),
        read_inactivity: Duration::from_millis(400),
        keep_alive_idle: Duration::from_millis(700),
        write_timeout: Duration::from_secs(2),
        body_stall: Duration::from_millis(700),
        sniff_timeout: Duration::from_millis(150),
        ..plippa_server::limits::WireLimits::default()
    }
}

impl Rig {
    fn start(https: bool) -> Rig {
        Rig::start_with(https, HttpLimits::default(), false)
    }

    /// A device whose portal is on, on the chosen protocol, with the PIN switch as given.
    fn start_with(https: bool, limits: HttpLimits, pin_required: bool) -> Rig {
        Rig::start_with_choice(Some(https), limits, pin_required)
    }

    /// `None` leaves the setting untouched, which is a device whose state file never carried it
    /// (a fresh one, or one written before SECFIX_03): the only way to test the default.
    fn start_with_choice(https: Option<bool>, limits: HttpLimits, pin_required: bool) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let root = base.join("library");
        let secrets = base.join("secrets");
        fs::create_dir_all(&root).unwrap();

        let portal = free_port();
        let control = free_port();
        let token_path = base.join("portal.token");
        let svc = Arc::new(Mutex::new(ServerService::start(
            Some(root.clone()),
            Box::new(RecordingIpc::new()),
            ServerService::default_mock_associate(),
        )));
        let access = Arc::new(PortalAccess::open_with(
            Some(&secrets),
            Arc::new(ManualClock::new()),
            LockoutPolicy::default(),
        ));
        if let Some(https) = https {
            access.set_https_enabled(https).expect("the protocol choice is saved");
        }
        access.set_enabled(true).expect("enable the portal");
        access.set_pin_required(pin_required).expect("PIN switch");

        let cfg = ServerConfig {
            bind: format!("127.0.0.1:{portal}").parse().unwrap(),
            control_bind: Some(format!("127.0.0.1:{control}").parse().unwrap()),
            control_token_path: Some(token_path.clone()),
            library_root: Some(root),
            secrets_dir: Some(secrets.clone()),
            portal_url: None,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (svc, stop, access) = (Arc::clone(&svc), Arc::clone(&stop), Arc::clone(&access));
            std::thread::spawn(move || {
                let _ = serve_with_access(cfg, svc, Some(stop), limits, access);
            })
        };
        let t = Instant::now();
        while !token_path.exists() {
            assert!(t.elapsed() < Duration::from_secs(10), "the control listener did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        let token = fs::read_to_string(&token_path).unwrap();
        let rig = Rig {
            portal,
            control,
            token,
            secrets,
            stop,
            thread: Some(thread),
            _tmp: tmp,
        };
        rig.wait_listening();
        rig
    }

    fn wait_listening(&self) {
        let t = Instant::now();
        while TcpStream::connect(("127.0.0.1", self.portal)).is_err() {
            assert!(t.elapsed() < Duration::from_secs(10), "the portal listener is not up");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The certificate the device keeps (and claims to serve).
    fn cert(&self) -> Vec<u8> {
        let mut loaded = tls::CertStore::new(&self.secrets)
            .load()
            .expect("a certificate on disk");
        std::mem::take(&mut loaded.cert_der)
    }

    fn fingerprint(&self) -> String {
        tls::fingerprint_of(&self.cert())
    }

    fn settings(&self) -> serde_json::Value {
        let (status, _, body) = self.ctl("GET", "/api/portal/settings", "");
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    fn set(&self, body: &str) -> (u16, serde_json::Value) {
        let (status, _, text) = self.ctl("POST", "/api/portal/settings", body);
        (status, serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    fn ctl(&self, method: &str, path: &str, body: &str) -> (u16, String, String) {
        let host = format!("127.0.0.1:{}", self.control);
        let headers = [
            ("Host", host.as_str()),
            ("X-Plippa-Local", self.token.as_str()),
            ("X-Plippa", "1"),
            ("Content-Type", "application/json"),
        ];
        raw(self.control, method, path, &headers, body.as_bytes())
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.portal)
    }

    /// A verified TLS request through the portal. Returns `(status line + headers, body)`.
    fn tls(&self, request: &str) -> (String, String) {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(self.cert()))
            .expect("the device certificate is a root for this test");
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = ServerName::try_from("127.0.0.1").expect("an IP server name");
        let conn = rustls::ClientConnection::new(Arc::new(config), name).expect("client");
        let sock = TcpStream::connect(("127.0.0.1", self.portal)).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut stream = rustls::StreamOwned::new(conn, sock);
        stream.write_all(request.as_bytes()).expect("write");
        stream.flush().expect("flush");
        // The server closes the connection after the answer without a TLS `close_notify`
        // (that is tiny_http's plain `shutdown`), so `UnexpectedEof` ends the read cleanly.
        let mut all = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(e) => panic!("read the answer: {e}"),
            }
        }
        // The certificate the client was actually offered must be the one on disk: the
        // handshake above could only verify because the name, dates and usage are right.
        let served = stream.conn.peer_certificates().expect("a server certificate");
        assert_eq!(served.len(), 1, "a single self-signed certificate");
        assert_eq!(served[0].as_ref(), self.cert().as_slice());
        split(&String::from_utf8_lossy(&all))
    }

    /// A verified TLS request whose writes are recorded: `(head, body, bytes on the wire)`.
    fn tls_recorded(&self, request: &str) -> (String, String, Vec<u8>) {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(self.cert()))
            .expect("the device certificate is a root for this test");
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = ServerName::try_from("127.0.0.1").expect("an IP server name");
        let conn = rustls::ClientConnection::new(Arc::new(config), name).expect("client");
        let sock = TcpStream::connect(("127.0.0.1", self.portal)).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Recording {
            inner: sock,
            seen: Arc::clone(&seen),
        };
        let mut stream = rustls::StreamOwned::new(conn, recorder);
        stream.write_all(request.as_bytes()).expect("write");
        stream.flush().expect("flush");
        let mut all = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(e) => panic!("read the answer: {e}"),
            }
        }
        let wire = seen.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let (head, body) = split(&String::from_utf8_lossy(&all));
        (head, body, wire)
    }

    /// A verified TLS connection for a request written in pieces (a slow upload).
    fn tls_stream(&self) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(self.cert()))
            .expect("the device certificate is a root for this test");
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = ServerName::try_from("127.0.0.1").expect("an IP server name");
        let conn = rustls::ClientConnection::new(Arc::new(config), name).expect("client");
        let sock = TcpStream::connect(("127.0.0.1", self.portal)).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        rustls::StreamOwned::new(conn, sock)
    }

    /// The library the server imports into.
    fn root(&self) -> PathBuf {
        self.secrets.parent().expect("a parent").join("library")
    }

    fn plain(&self, request: &str) -> (String, String) {
        let mut sock = TcpStream::connect(("127.0.0.1", self.portal)).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        sock.write_all(request.as_bytes()).expect("write");
        sock.flush().ok();
        let mut all = String::new();
        let _ = sock.read_to_string(&mut all);
        split(&all)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A socket that keeps every byte written to it, so a test can look at what actually crossed
/// the wire (ciphertext, never the plaintext the TLS layer was given).
struct Recording<S> {
    inner: S,
    seen: Arc<Mutex<Vec<u8>>>,
}

impl<S: Read> Read for Recording<S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<S: Write> Write for Recording<S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.seen
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend_from_slice(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Read a whole TLS answer, tolerating the close without `close_notify`.
fn read_all_tls<S: Read>(stream: &mut S) -> Vec<u8> {
    let mut all = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => panic!("read the answer: {e}"),
        }
    }
    all
}

/// Every regular file's length under `dir`, recursively.
fn file_sizes(dir: &std::path::Path) -> Vec<u64> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(file_sizes(&path));
        } else if let Ok(meta) = entry.metadata() {
            out.push(meta.len());
        }
    }
    out
}

/// A valid mono 8-bit WAV with `secs` seconds of audio at 8 kHz (8000 bytes per second).
fn wav(secs: u32, tag: u8) -> Vec<u8> {
    let rate = 8000u32;
    let data = vec![tag; (rate * secs) as usize];
    let mut v = b"RIFF".to_vec();
    v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&8u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    v
}

/// `(status line + headers, body)`.
fn split(text: &str) -> (String, String) {
    match text.split_once("\r\n\r\n") {
        Some((head, body)) => (head.to_string(), body.to_string()),
        None => (text.to_string(), String::new()),
    }
}

fn get(path: &str, host: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nX-Plippa: 1\r\nConnection: close\r\n\r\n")
}

/// A request with a body, with `Content-Length` computed from it.
fn post(path: &str, host: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nX-Plippa: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn raw(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String, String) {
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    sock.write_all(request.as_bytes()).expect("write");
    sock.write_all(body).expect("body");
    sock.flush().ok();
    let mut all = String::new();
    let _ = sock.read_to_string(&mut all);
    let (head, rest) = split(&all);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, head, rest)
}

fn cookie_header(head: &str) -> String {
    head.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .expect("a session cookie")
        .to_string()
}

/// The value the browser would send back: `plippa_session=<token>`.
fn cookie_value(set_cookie: &str) -> String {
    let value = set_cookie
        .split_once(':')
        .expect("a Set-Cookie header")
        .1
        .trim()
        .split(';')
        .next()
        .expect("a value");
    let (name, token) = value.split_once('=').expect("name=value");
    assert_eq!(name.trim(), "plippa_session", "{set_cookie}");
    token.trim().to_string()
}

// ---------------------------------------------------------------------------
// The listener
// ---------------------------------------------------------------------------

/// A verified TLS request is served, and the certificate the device keeps is the one it serves.
#[test]
fn the_portal_serves_https_with_the_certificate_whose_fingerprint_it_shows() {
    let rig = Rig::start(true);
    let (head, body) = rig.tls(&get("/api/status", &rig.host()));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(body.trim_start().starts_with('{'), "a JSON answer: {body}");

    // The device menu shows the fingerprint of the certificate it serves, and nothing else.
    let settings = rig.settings();
    assert_eq!(settings["https_enabled"], true, "{settings}");
    assert_eq!(settings["listening"], true, "{settings}");
    let shown = settings["fingerprint"]
        .as_str()
        .expect("a fingerprint while HTTPS is on");
    assert_eq!(shown, rig.fingerprint(), "the menu names the served certificate");
    assert_eq!(shown.len(), 64);
    assert!(shown
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
}

/// A plain request on the TLS port is answered `307` to the same address over HTTPS, and gets
/// nothing else: no login form, no API, no cookie, and no permanent redirect.
#[test]
fn a_plain_request_is_answered_with_a_307_and_nothing_else() {
    let rig = Rig::start(true);
    let host = rig.host();
    let (head, body) = rig.plain(&get("/api/status?x=1", &host));
    assert!(head.starts_with("HTTP/1.1 307"), "{head}");
    assert!(
        head.contains(&format!("Location: https://{host}/api/status?x=1")),
        "the same address over TLS: {head}"
    );
    assert!(!head.to_ascii_lowercase().contains("set-cookie"), "{head}");
    assert!(!head.contains("Strict-Transport-Security"), "no HSTS: {head}");
    // Not a permanent redirect: a cached 308 would make the owner's "switch HTTPS off" useless.
    assert!(!head.starts_with("HTTP/1.1 301"), "{head}");
    assert!(!head.starts_with("HTTP/1.1 302"), "{head}");
    assert!(!head.starts_with("HTTP/1.1 308"), "{head}");
    assert!(!body.trim_start().starts_with('{'), "no API answer in the clear: {body}");

    // A state-changing request is redirected too, with its method intact (307, not 303).
    let (head, _) = rig.plain(&post("/api/portal/login", &host, r#"{"pin":"000000"}"#));
    assert!(head.starts_with("HTTP/1.1 307"), "{head}");
    assert!(head.contains("Location: https://"), "{head}");
}

/// A plaintext request whose request line is not ASCII never reaches routing: the head reader
/// refuses it at the framing layer, so it can be neither redirected nor answered with a `500`
/// carrying a mangled target.
#[test]
fn a_non_ascii_request_line_is_refused_by_the_framing_layer() {
    let rig = Rig::start(true);
    let request = format!(
        "GET /api/status?q=caf\u{e9} HTTP/1.1\r\nHost: {}\r\nX-Plippa: 1\r\n\
         Connection: close\r\n\r\n",
        rig.host()
    );
    let (head, body) = rig.plain(&request);
    assert!(
        head.is_empty(),
        "the head reader refuses the connection before any status line: {head}"
    );
    assert!(!head.contains(" 500"), "a mangled target is not a server error: {head}");
    assert!(!head.contains("Location:"), "nothing is redirected: {head}");
    assert!(body.is_empty(), "and no body is produced: {body}");
}

/// The redirect names the port this listener is on, not whatever port `Host` claimed: otherwise
/// `Host: <device>:80` would send the browser to a port that does not speak TLS.
#[test]
fn the_redirect_ignores_a_wrong_port_in_the_host_header() {
    let rig = Rig::start(true);
    let (head, _) = rig.plain(&get("/api/status", "127.0.0.1:80"));
    assert!(head.starts_with("HTTP/1.1 307"), "{head}");
    assert!(
        head.contains(&format!("Location: https://127.0.0.1:{}/api/status", rig.portal)),
        "the listener's own port: {head}"
    );
}

/// A plain client never reaches the API, and a `Host` that is not this portal's name is never
/// echoed into a redirect.
#[test]
fn a_client_that_cannot_speak_tls_gets_no_api_answer() {
    let rig = Rig::start(true);
    for request in [
        // A bare HTTP/1.0 request with no Host at all.
        "GET /api/playables HTTP/1.0\r\n\r\n".to_string(),
        // Garbage that is not a request line.
        "HELLO\r\n\r\n".to_string(),
    ] {
        let (head, body) = rig.plain(&request);
        // Whatever the answer is, it is not the portal: no session, and none of its data.
        assert!(!head.to_ascii_lowercase().contains("set-cookie"), "{head}");
        assert!(!body.contains("playables"), "{request:?} got {body}");
        assert!(!body.contains("bandcamp"), "{body}");
        assert!(!body.contains("<html"), "no portal page in the clear: {body}");
    }
    let (head, _) = rig.plain(&get("/api/status", "evil.example.com"));
    assert!(!head.contains("evil.example.com"), "no open redirect: {head}");
}

/// No answer carries HSTS: a browser that pins HTTPS could not be talked back down, which would
/// make the setting impossible to turn off.
#[test]
fn no_answer_carries_hsts() {
    let rig = Rig::start(true);
    for request in [get("/api/status", &rig.host()), get("/", &rig.host())] {
        let (head, _) = rig.tls(&request);
        assert!(!head.contains("Strict-Transport-Security"), "{head}");
    }
}

/// A client that connects and says nothing is cut off, and does not delay the clients behind it.
#[test]
fn a_silent_client_is_cut_off_and_does_not_stall_others() {
    let rig = Rig::start_with(
        true,
        HttpLimits::with_admission(fast_wire(), Default::default()),
        false,
    );

    // Eight connections that never say a word, plus one that starts a TLS handshake and stops
    // half way. With the protocol sniff on the accept thread this is eight times
    // `sniff_timeout` of serial delay before the next client is even admitted.
    let mut silent: Vec<TcpStream> = (0..8)
        .map(|_| {
            let sock = TcpStream::connect(("127.0.0.1", rig.portal)).expect("connect");
            // Short enough that a server which held the socket open fails the bound below
            // instead of costing this test ten seconds.
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            sock
        })
        .collect();
    let mut stalled = TcpStream::connect(("127.0.0.1", rig.portal)).expect("connect");
    stalled.write_all(&[0x16, 0x03, 0x01, 0x00, 0xff]).ok();
    stalled.flush().ok();

    // A well-behaved client is served while they are still waiting: admitting one connection
    // must not depend on what any other connection says. Eight silent clients at the 150 ms
    // sniff window would be 1.2 s of serial delay if the peek ran on the accept thread.
    let started = Instant::now();
    let (head, _) = rig.tls(&get("/api/status", &rig.host()));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "silent clients delayed another by {:?}",
        started.elapsed()
    );

    // The silent connection is closed by the server, not held open. The bound matters: a read
    // that merely timed out (two seconds) would also be an `Err`, so measure how long it took.
    let closed_at = Instant::now();
    let mut byte = [0u8; 1];
    let closed = silent[0].read(&mut byte);
    assert!(
        matches!(closed, Ok(0)) || closed.is_err(),
        "the silent connection was left open"
    );
    assert!(
        closed_at.elapsed() < Duration::from_secs(1),
        "the silent connection was held open for {:?}",
        closed_at.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Switching HTTPS off
// ---------------------------------------------------------------------------

/// With HTTPS off the portal serves plain HTTP on the same port, does not redirect, and the
/// session cookie it hands out has no `Secure` flag (it would never be sent back otherwise).
#[test]
fn https_off_serves_plain_http_and_the_cookie_loses_its_secure_flag() {
    let rig = Rig::start_with(false, HttpLimits::default(), true);
    let host = rig.host();
    let pin = rig.settings()["pin"].as_str().expect("a PIN").to_string();

    // Plain HTTP answers directly: no redirect to a TLS port that is not serving TLS.
    let (head, body) = rig.plain(&get("/", &host));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(!head.contains("Location:"), "{head}");
    assert!(body.contains("<html") || body.contains("<!DOCTYPE"), "{body}");

    let (head, body) = rig.plain(&post("/api/portal/login", &host, &format!(r#"{{"pin":"{pin}"}}"#)));
    assert!(head.starts_with("HTTP/1.1 200"), "{head} {body}");
    let cookie = cookie_header(&head);
    assert!(!cookie.to_ascii_lowercase().contains("secure"), "{cookie}");

    // With HTTPS off there is no fingerprint to show, and the settings say so.
    let settings = rig.settings();
    assert_eq!(settings["https_enabled"], false, "{settings}");
    assert!(settings["fingerprint"].is_null(), "{settings}");
    assert_eq!(settings["listening"], true, "{settings}");
}

/// The session cookie carries `Secure` while the portal is HTTPS.
#[test]
fn the_session_cookie_is_secure_under_https() {
    let rig = Rig::start_with(true, HttpLimits::default(), true);
    let host = rig.host();
    let pin = rig.settings()["pin"].as_str().expect("a PIN").to_string();
    let (head, body) = rig.tls(&post(
        "/api/portal/login",
        &host,
        &format!(r#"{{"pin":"{pin}"}}"#),
    ));
    assert!(head.starts_with("HTTP/1.1 200"), "{head} {body}");
    let cookie = cookie_header(&head);
    assert!(cookie.to_ascii_lowercase().contains("secure"), "{cookie}");
}

/// A token that was live when the protocol changed is dead afterwards, in both directions.
///
/// This is the part of SRV-03 that a warning cannot cover: while HTTPS is off the session cookie
/// is on the wire in the clear, so a LAN listener can keep it. If flipping the setting left that
/// token valid, switching HTTPS back on would look like it closed the exposed window while the
/// intruder kept a signed-in browser for the rest of the session's 90-day life.
#[test]
fn a_flip_of_the_setting_revokes_live_sessions() {
    let rig = Rig::start_with(true, HttpLimits::default(), true);
    let host = rig.host();
    let pin = rig.settings()["pin"].as_str().expect("a PIN").to_string();
    let login = post("/api/portal/login", &host, &format!(r#"{{"pin":"{pin}"}}"#));
    let (head, body) = rig.tls(&login);
    assert!(head.starts_with("HTTP/1.1 200"), "{head} {body}");
    let session = cookie_value(&cookie_header(&head));

    // The token works while the protocol it was issued under is still the one serving.
    let signed_in = format!(
        "GET /api/playables HTTP/1.1\r\nHost: {host}\r\nCookie: plippa_session={session}\r\n\
         X-Plippa: 1\r\nConnection: close\r\n\r\n"
    );
    let (head, _) = rig.tls(&signed_in);
    assert!(head.starts_with("HTTP/1.1 200"), "a live session: {head}");

    // HTTPS off: the same token must not be honoured over the new, cleartext transport.
    let (status, body) = rig.set(r#"{"https_enabled":false}"#);
    assert_eq!(status, 200, "{body}");
    rig.wait_listening();
    let (head, _) = rig.plain(&signed_in);
    assert!(
        head.starts_with("HTTP/1.1 401"),
        "a pre-flip session survived the downgrade: {head}"
    );

    // And the other way round: sign in over plain HTTP, flip back on, the token is dead again.
    let (head, body) = rig.plain(&post(
        "/api/portal/login",
        &host,
        &format!(r#"{{"pin":"{pin}"}}"#),
    ));
    assert!(head.starts_with("HTTP/1.1 200"), "{head} {body}");
    let cleartext_session = cookie_value(&cookie_header(&head));
    let (status, body) = rig.set(r#"{"https_enabled":true}"#);
    assert_eq!(status, 200, "{body}");
    rig.wait_listening();
    let signed_in = format!(
        "GET /api/playables HTTP/1.1\r\nHost: {host}\r\nCookie: plippa_session={cleartext_session}\r\n\
         X-Plippa: 1\r\nConnection: close\r\n\r\n"
    );
    let (head, _) = rig.tls(&signed_in);
    assert!(
        head.starts_with("HTTP/1.1 401"),
        "a token sniffed during the clear-text window outlived HTTPS being restored: {head}"
    );
}

/// The whole point of SRV-03: a Bandcamp password POSTed from the browser is not readable on
/// the wire. The client is real TLS; every byte it writes is recorded, and the password and
/// user name appear in none of it.
#[test]
fn the_credentials_post_is_not_readable_on_the_wire() {
    let rig = Rig::start(true);
    let host = rig.host();
    let user = "owner-sentinel-user@example.com";
    let password = "hunter2-sentinel-password";
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);

    // `consent` is absent, so the server refuses before it would dial Bandcamp; the secrets
    // still cross the wire in this request, which is the part under test.
    let (head, _answer, wire) =
        rig.tls_recorded(&post("/api/stores/bandcamp/connect", &host, &body));
    assert!(head.starts_with("HTTP/1.1 4"), "a refusal, not a dial: {head}");
    assert!(!wire.is_empty(), "the client wrote nothing");
    // A TLS record header, not a plain HTTP request line.
    assert_eq!(wire[0], 0x16, "the first record is a handshake");
    let on_the_wire = String::from_utf8_lossy(&wire);
    assert!(!on_the_wire.contains(password), "the password crossed in clear text");
    assert!(!on_the_wire.contains(user), "the user name crossed in clear text");
    assert!(!on_the_wire.contains("connect"), "not even the route is visible");
    // With HTTPS off the same request is readable, which is why the setting warns.
    let rig = Rig::start_with(false, HttpLimits::default(), false);
    let host = rig.host();
    let (head, _) = rig.plain(&post("/api/stores/bandcamp/connect", &host, &body));
    assert!(head.starts_with("HTTP/1.1 4"), "{head}");
}

/// SECFIX_01's large-upload pair, repeated through TLS: the stall and progress rules sit below
/// the TLS layer, so a slow but steadily progressing upload must still complete, and one that
/// stops half way must still be cut off and answered `408`.
#[test]
fn a_slow_but_progressing_upload_completes_over_tls_and_a_stalled_one_is_cut_off() {
    let rig = Rig::start_with(
        true,
        HttpLimits::with_admission(fast_wire(), Default::default()),
        false,
    );
    let host = rig.host();

    // 3.2 MB in 64 KiB pieces, 60 ms apart: seconds of wall clock, many times the 400 ms
    // inactivity timeout, every pause far below it.
    let body = wav(400, 5);
    let mut stream = rig.tls_stream();
    let head = format!(
        "POST /api/media?kind=audiobook&filename=long.wav HTTP/1.1\r\nHost: {host}\r\nX-Plippa: 1\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let started = Instant::now();
    stream.write_all(head.as_bytes()).expect("head");
    for piece in body.chunks(64 * 1024) {
        stream.write_all(piece).expect("a piece");
        stream.flush().expect("flush");
        std::thread::sleep(Duration::from_millis(60));
    }
    let answer = read_all_tls(&mut stream);
    let text = String::from_utf8_lossy(&answer);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(
        started.elapsed() > Duration::from_millis(1_500),
        "the upload took only {:?}: it must outlast the stall timeout many times",
        started.elapsed()
    );
    let stored = file_sizes(&rig.root());
    assert!(
        stored.contains(&(body.len() as u64)),
        "the whole file must be in the library: {stored:?}"
    );

    // Half the body, then silence: the server answers 408 and closes, and the slot is free for
    // the next upload.
    let mut stalled = rig.tls_stream();
    let body = wav(40, 9);
    let head = format!(
        "POST /api/media?kind=audiobook&filename=stall.wav HTTP/1.1\r\nHost: {host}\r\nX-Plippa: 1\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stalled.write_all(head.as_bytes()).expect("head");
    stalled.write_all(&body[..body.len() / 2]).expect("half");
    stalled.flush().expect("flush");
    let answer = read_all_tls(&mut stalled);
    let text = String::from_utf8_lossy(&answer);
    assert!(text.starts_with("HTTP/1.1 408"), "{text}");

    let mut next = rig.tls_stream();
    let small = wav(2, 1);
    let head = format!(
        "POST /api/media?kind=audiobook&filename=next.wav HTTP/1.1\r\nHost: {host}\r\nX-Plippa: 1\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        small.len()
    );
    next.write_all(head.as_bytes()).expect("head");
    next.write_all(&small).expect("body");
    next.flush().expect("flush");
    let text = String::from_utf8_lossy(&read_all_tls(&mut next)).to_string();
    assert!(text.starts_with("HTTP/1.1 200"), "the slot was freed: {text}");
}

/// Flipping the setting from the device menu rebinds the listener in place: plain HTTP starts
/// answering (and TLS stops), and flipping it back restores TLS with the same certificate.
#[test]
fn flipping_the_setting_rebinds_the_listener() {
    let rig = Rig::start(true);
    let host = rig.host();
    let fingerprint = rig.fingerprint();

    // A live connection is open when the setting changes; it must be dropped, not kept.
    let mut live = TcpStream::connect(("127.0.0.1", rig.portal)).expect("connect");
    live.write_all(&[0x16, 0x03, 0x01, 0x00, 0xff]).ok();
    live.flush().ok();

    let (status, body) = rig.set(r#"{"https_enabled":false}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["https_enabled"], false, "{body}");
    rig.wait_listening();

    let (head, plain_body) = rig.plain(&get("/api/status", &host));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(!head.contains("Location:"), "{head}");
    assert!(plain_body.trim_start().starts_with('{'), "{plain_body}");
    // The connection that was live when the setting changed did not survive it.
    live.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut byte = [0u8; 1];
    let gone = live.read(&mut byte);
    assert!(
        matches!(gone, Ok(0)) || gone.is_err(),
        "a live connection outlived the change"
    );

    // And back: TLS again, with the same certificate (only a regenerate replaces it).
    let (status, body) = rig.set(r#"{"https_enabled":true}"#);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["fingerprint"].as_str(),
        Some(fingerprint.as_str()),
        "{body}"
    );
    rig.wait_listening();
    let (head, _) = rig.tls(&get("/api/status", &host));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

/// The host and origin guards sit above the transport, so they must hold when the request
/// arrives over TLS exactly as they do in the clear.
#[test]
fn host_and_origin_guards_hold_over_https() {
    let rig = Rig::start_with(true, HttpLimits::default(), true);
    let host = rig.host();

    // A foreign Origin on a state-changing request is refused.
    let foreign = format!(
        "POST /api/portal/logout HTTP/1.1\r\nHost: {host}\r\nOrigin: https://evil.example.com\r\nContent-Type: application/json\r\nX-Plippa: 1\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    );
    let (head, _) = rig.tls(&foreign);
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");

    // The portal's own https origin is accepted.
    let own = format!(
        "POST /api/portal/logout HTTP/1.1\r\nHost: {host}\r\nOrigin: https://{host}\r\nContent-Type: application/json\r\nX-Plippa: 1\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    );
    let (head, body) = rig.tls(&own);
    assert!(head.starts_with("HTTP/1.1 200"), "{head} {body}");

    // A rebinding Host is refused over TLS too.
    let (head, _) = rig.tls(&get("/api/status", "rebind.attacker.example"));
    assert!(head.starts_with("HTTP/1.1 403"), "{head}");
}

/// While HTTPS is off the port offers no TLS handshake at all: a client that sends one gets a
/// plain HTTP refusal, never a ServerHello that would make it think TLS is available.
#[test]
fn https_off_offers_no_tls_handshake() {
    // Started over HTTPS so a certificate exists, then switched off from the menu.
    let rig = Rig::start(true);
    let (status, body) = rig.set(r#"{"https_enabled":false}"#);
    assert_eq!(status, 200, "{body}");
    rig.wait_listening();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(rig.cert()))
        .expect("a root");
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = ServerName::try_from("127.0.0.1").expect("a name");
    let conn = rustls::ClientConnection::new(Arc::new(config), name).expect("client");
    let sock = TcpStream::connect(("127.0.0.1", rig.portal)).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut stream = rustls::StreamOwned::new(conn, sock);
    // The client's handshake write succeeds; the answer cannot be a TLS record.
    let _ = stream.write_all(get("/api/status", &rig.host()).as_bytes());
    let mut first = [0u8; 1];
    let read = stream.read(&mut first);
    assert!(
        read.is_err() || first[0] != 0x16,
        "the server answered a handshake while HTTPS is off (read {read:?})"
    );
}

/// Regenerating from the menu issues a new certificate: the fingerprint changes, the listener is
/// rebound, and the new certificate verifies for the same names.
#[test]
fn regenerating_issues_a_new_certificate() {
    let rig = Rig::start(true);
    let before = rig.fingerprint();

    let (status, body) = rig.set(r#"{"regenerate_certificate":true}"#);
    assert_eq!(status, 200, "{body}");
    let after = body["fingerprint"].as_str().expect("a fingerprint").to_string();
    assert_ne!(after, before, "a new certificate");
    assert_eq!(after, rig.fingerprint(), "and it is the one on disk");
    rig.wait_listening();

    let (head, _) = rig.tls(&get("/api/status", &rig.host()));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}

/// When the certificate cannot be written, the portal stays off rather than serving plain HTTP:
/// "HTTPS on" that silently degrades is worse than a portal that does not come up. The control
/// listener stays up so the device menu can say why.
#[test]
fn a_certificate_that_cannot_be_written_keeps_the_portal_off() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    let root = base.join("library");
    let secrets = base.join("secrets");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&secrets).unwrap();
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700)).unwrap();
    // The certificate path is taken by a directory, so the final rename cannot succeed.
    fs::create_dir_all(secrets.join(tls::TLS_FILE)).unwrap();

    let portal = free_port();
    let control = free_port();
    let token_path = base.join("portal.token");
    let svc = Arc::new(Mutex::new(ServerService::start(
        Some(root.clone()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    )));
    let access = Arc::new(PortalAccess::open_with(
        Some(&secrets),
        Arc::new(ManualClock::new()),
        LockoutPolicy::default(),
    ));
    access.set_enabled(true).expect("enable the portal");
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{portal}").parse().unwrap(),
        control_bind: Some(format!("127.0.0.1:{control}").parse().unwrap()),
        control_token_path: Some(token_path.clone()),
        library_root: Some(root),
        secrets_dir: Some(secrets.clone()),
        portal_url: None,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let handle = Arc::clone(&access);
    std::thread::spawn(move || {
        let _ = serve_with_access(cfg, svc, Some(stop), HttpLimits::default(), handle);
    });
    // The control listener comes up regardless: that is where the owner reads the reason.
    let started = Instant::now();
    while !token_path.exists() {
        assert!(started.elapsed() < Duration::from_secs(10), "no control listener");
        std::thread::sleep(Duration::from_millis(20));
    }
    let token = fs::read_to_string(&token_path).unwrap();
    let settings = || -> serde_json::Value {
        let host = format!("127.0.0.1:{control}");
        let (status, _, body) = raw(
            control,
            "GET",
            "/api/portal/settings",
            &[
                ("Host", host.as_str()),
                ("X-Plippa-Local", token.as_str()),
                ("X-Plippa", "1"),
            ],
            b"",
        );
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    };

    // Long enough for an open, a failure and a retry to have happened.
    std::thread::sleep(Duration::from_millis(1_200));
    assert!(
        TcpStream::connect(("127.0.0.1", portal)).is_err(),
        "the portal came up without a certificate"
    );
    let shown = settings();
    assert_eq!(shown["enabled"], true, "the owner's switch is not silently flipped");
    assert_eq!(shown["https_enabled"], true, "still the safe default");
    assert_eq!(shown["listening"], false, "and it is not pretending to listen");
    assert!(shown["fingerprint"].is_null(), "{shown}");
    let reason = shown["listen_error"]
        .as_str()
        .unwrap_or_else(|| panic!("the owner is told why: {shown}"));
    assert!(
        reason.to_ascii_lowercase().contains("certificate"),
        "the reason names the certificate: {reason}"
    );
    assert!(
        TcpStream::connect(("127.0.0.1", portal)).is_err(),
        "a retry turned it into a plain-HTTP portal"
    );
}

/// The certificate and key are one private file; nothing in the secrets directory is readable
/// by anyone else.
#[test]
fn the_certificate_and_key_are_one_private_file() {
    let rig = Rig::start(true);
    let mut names: Vec<String> = fs::read_dir(&rig.secrets)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .collect();
    names.sort();
    assert!(names.contains(&tls::TLS_FILE.to_string()), "{names:?}");
    for name in &names {
        let meta = fs::metadata(rig.secrets.join(name)).unwrap();
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "{name} is not owner-only"
        );
        assert!(
            !name.ends_with(".pem") && !name.ends_with(".key"),
            "{name}: key material as text"
        );
    }
}

/// A state file written before the setting existed means "on": an upgrade never quietly turns
/// the encryption off.
#[test]
fn an_absent_https_field_means_on() {
    let tmp = tempfile::tempdir().unwrap();
    let secrets = tmp.path().join("secrets");
    fs::create_dir_all(&secrets).unwrap();
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        secrets.join("portal-access.state"),
        br#"{"version":1,"enabled":true,"pin_required":true,"pin":"135790","sessions":[]}"#,
    )
    .unwrap();
    let access = PortalAccess::open_with(
        Some(&secrets),
        Arc::new(ManualClock::new()),
        LockoutPolicy::default(),
    );
    assert!(access.https_enabled(), "an absent field means HTTPS on, never off");
    assert!(access.settings(false).https_enabled);
}

/// The same default, end to end: a fresh secrets directory with nothing written yet brings the
/// listener up on TLS, and plain HTTP on that port gets the `307` rather than the portal.
#[test]
fn a_fresh_device_serves_tls_without_anyone_touching_the_setting() {
    let rig = Rig::start_with_choice(None, HttpLimits::default(), true);
    let host = rig.host();
    assert_eq!(rig.settings()["https_enabled"], true, "the default is HTTPS on");

    let (head, body) = rig.tls(&get("/api/status", &host));
    assert!(head.starts_with("HTTP/1.1 200"), "{head} {body}");
    assert!(body.trim_start().starts_with('{'), "{body}");

    let (head, _) = rig.plain(&get("/api/status", &host));
    assert!(head.starts_with("HTTP/1.1 307"), "{head}");
}

/// The advertised address follows the switch, because that is what the QR code and the
/// `Stores > Connect` warning are built from.
#[test]
fn the_advertised_address_follows_the_switch() {
    let rig = Rig::start(true);
    let (status, _, body) = rig.ctl("GET", "/api/portal/url", "");
    assert_eq!(status, 200, "{body}");
    let on: serde_json::Value = serde_json::from_str(&body).unwrap();
    let on_url = on["recommended"].as_str().expect("a recommended address");
    assert!(on_url.starts_with("https://"), "{body}");

    let (status, body) = rig.set(r#"{"https_enabled":false}"#);
    assert_eq!(status, 200, "{body}");
    rig.wait_listening();
    let (status, _, body) = rig.ctl("GET", "/api/portal/url", "");
    assert_eq!(status, 200, "{body}");
    let off: serde_json::Value = serde_json::from_str(&body).unwrap();
    let off_url = off["recommended"].as_str().expect("a recommended address");
    assert!(off_url.starts_with("http://"), "the QR code must not claim TLS: {body}");
    assert_eq!(
        on_url.trim_start_matches("https://"),
        off_url.trim_start_matches("http://"),
        "the switch changes the scheme, not the address"
    );
}

/// Switching off and back on reuses the certificate byte for byte: a browser that accepted it
/// once is not asked again, and the fingerprint on the device screen keeps matching.
#[test]
fn flipping_off_and_on_never_rewrites_the_certificate() {
    let rig = Rig::start(true);
    let path = rig.secrets.join(tls::TLS_FILE);
    let before = fs::read(&path).expect("the certificate file");
    let fingerprint = rig.fingerprint();

    let (status, body) = rig.set(r#"{"https_enabled":false}"#);
    assert_eq!(status, 200, "{body}");
    rig.wait_listening();
    let (status, body) = rig.set(r#"{"https_enabled":true}"#);
    assert_eq!(status, 200, "{body}");
    rig.wait_listening();

    let after = fs::read(&path).expect("the certificate file");
    assert_eq!(before, after, "the key was regenerated by a protocol flip");
    assert_eq!(rig.fingerprint(), fingerprint);
    // And the portal really is serving that certificate again.
    let (head, _) = rig.tls(&get("/api/status", &rig.host()));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
}
