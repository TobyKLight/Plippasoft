//! SECFIX_01 (SRV-02): the HTTP layer refuses oversized heads, cuts off slow and half-open
//! clients, bounds connections, and keeps loopback (the on-device UI) reachable.
//!
//! Everything runs over real loopback sockets against `serve_with_limits` with shortened
//! timeouts (hundreds of milliseconds), so slow-network behaviour is proven without waiting for
//! the production 10-30 second values.

mod common;

use plippa_server::{
    AdmissionLimits, Connections, HttpLimits, RecordingIpc, ServerConfig,
    ServerService,
};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Production-shaped limits with everything that waits shortened.
fn fast_wire() -> plippa_server::limits::WireLimits {
    plippa_server::limits::WireLimits {
        head_deadline: Duration::from_millis(600),
        read_inactivity: Duration::from_millis(300),
        keep_alive_idle: Duration::from_millis(800),
        write_timeout: Duration::from_secs(2),
        body_stall: Duration::from_millis(600),
        ..plippa_server::limits::WireLimits::default()
    }
}

struct Rig {
    port: u16,
    conns: Connections,
    _tmp: tempfile::TempDir,
}

fn start(limits: HttpLimits) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    std::fs::create_dir_all(&root).unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let svc = Arc::new(Mutex::new(ServerService::start(
        Some(root.clone()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    )));
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        library_root: Some(root),
        secrets_dir: None,
        portal_url: None,
        ..ServerConfig::default()
    };
    let conns = limits.connections.clone();
    common::serve_open_with_limits(cfg, svc, limits);
    for _ in 0..200 {
        // A real request, so that the probe connection has certainly been accepted, admitted and
        // finished before the test starts counting connections.
        if let Ok(mut probe) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = probe.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = write!(
                probe,
                "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
            );
            let mut sink = Vec::new();
            let _ = probe.read_to_end(&mut sink);
            drop(probe);
            wait_for(|| conns.live() == 0, Duration::from_secs(10), "probe connection to leave");
            return Rig {
                port,
                conns,
                _tmp: tmp,
            };
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

fn wait_for(mut cond: impl FnMut() -> bool, within: Duration, what: &str) {
    let t = Instant::now();
    while !cond() {
        assert!(t.elapsed() < within, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn connect(port: u16) -> TcpStream {
    let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
    s
}

/// Everything the server sent before it closed (or reset) the connection, and how it ended.
fn drain(s: &mut TcpStream, within: Duration) -> (Vec<u8>, End) {
    s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let t = Instant::now();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return (out, End::Closed),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if t.elapsed() > within {
                    return (out, End::StillOpen);
                }
            }
            Err(_) => return (out, End::Closed), // reset
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum End {
    Closed,
    StillOpen,
}

fn status_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

fn get_status(port: u16) -> String {
    let mut s = connect(port);
    write!(
        s,
        "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let (out, _) = drain(&mut s, Duration::from_secs(4));
    status_line(&out)
}

// ---------------------------------------------------------------------------
// Head size limits
// ---------------------------------------------------------------------------

#[test]
fn header_flood_is_refused_before_it_is_buffered() {
    let rig = start(HttpLimits::with_admission(fast_wire(), AdmissionLimits::default()));

    // A 4 MiB single header line, sent steadily: 431, and the server stops reading long before
    // the writer is done (the writer is reset before it has sent 256 KiB).
    let mut s = connect(rig.port);
    s.write_all(b"GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Big: ").unwrap();
    let chunk = vec![b'a'; 4096];
    let mut sent = 0usize;
    let mut refused = false;
    for _ in 0..(4 * 1024 * 1024 / chunk.len()) {
        if s.write_all(&chunk).is_err() {
            refused = true;
            break;
        }
        sent += chunk.len();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(refused, "the server kept accepting a 4 MiB header line");
    assert!(sent < 256 * 1024, "the writer got {sent} bytes in before the reset");
    let (out, _) = drain(&mut s, Duration::from_secs(2));
    assert!(
        status_line(&out).contains(" 431 "),
        "answer: {:?}",
        String::from_utf8_lossy(&out)
    );

    // A request line of 100 KiB: 414.
    let mut s = connect(rig.port);
    let mut line = b"GET /".to_vec();
    line.extend(std::iter::repeat_n(b'a', 100 * 1024));
    line.extend_from_slice(b" HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    for part in line.chunks(4096) {
        if s.write_all(part).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let (out, _) = drain(&mut s, Duration::from_secs(2));
    assert!(
        status_line(&out).contains(" 414 "),
        "answer: {:?}",
        String::from_utf8_lossy(&out)
    );

    // 200 header lines: 431.
    let mut s = connect(rig.port);
    let mut head = String::from("GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for i in 0..200 {
        head.push_str(&format!("X-H{i}: v\r\n"));
    }
    head.push_str("\r\n");
    let _ = s.write_all(head.as_bytes());
    let (out, _) = drain(&mut s, Duration::from_secs(2));
    assert!(
        status_line(&out).contains(" 431 "),
        "answer: {:?}",
        String::from_utf8_lossy(&out)
    );

    // The server is fine afterwards.
    assert!(get_status(rig.port).contains(" 200 "));
}

#[test]
fn heads_just_inside_the_caps_are_still_served() {
    let rig = start(HttpLimits::with_admission(fast_wire(), AdmissionLimits::default()));
    // A big cookie-like header (7 KiB) and the maximum header count (64) are legitimate.
    let mut s = connect(rig.port);
    let mut head = String::from("GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    head.push_str(&format!("Cookie: {}\r\n", "c".repeat(7 * 1024)));
    for i in 0..60 {
        head.push_str(&format!("X-H{i}: v\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    let (out, _) = drain(&mut s, Duration::from_secs(3));
    assert!(status_line(&out).contains(" 200 "), "{:?}", String::from_utf8_lossy(&out));
}

// ---------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------

#[test]
fn slowloris_half_open_requests_are_closed_and_free_their_slots() {
    let admission = AdmissionLimits {
        max_total: 256,
        loopback_reserved: 8,
        max_per_client: 16,
    };
    let rig = start(HttpLimits::with_admission(fast_wire(), admission));
    let baseline = rig.conns.live();

    // 100 connections that each send one byte of a request line and then nothing.
    let mut socks: Vec<TcpStream> = (0..100)
        .map(|_| {
            let mut s = connect(rig.port);
            s.write_all(b"G").unwrap();
            s
        })
        .collect();
    wait_for(
        || rig.conns.live() >= baseline + 100,
        Duration::from_secs(10),
        "the half-open connections to be admitted",
    );

    // A normal request is answered while they are all open ...
    assert!(get_status(rig.port).contains(" 200 "), "no answer during the attack");

    // ... and after the head deadline every one of them has been closed by the server.
    let t = Instant::now();
    for (i, s) in socks.iter_mut().enumerate() {
        let (_, end) = drain(s, Duration::from_secs(5));
        assert_eq!(end, End::Closed, "half-open connection {i} is still open");
    }
    assert!(
        t.elapsed() < Duration::from_secs(8),
        "closing took {:?}",
        t.elapsed()
    );
    wait_for(
        || rig.conns.live() <= baseline,
        Duration::from_secs(10),
        "the live-connection count to return to its baseline",
    );
    assert!(get_status(rig.port).contains(" 200 "), "no answer after the attack");
}

#[test]
fn a_drip_that_keeps_every_read_alive_still_hits_the_head_deadline() {
    let rig = start(HttpLimits::with_admission(fast_wire(), AdmissionLimits::default()));
    let mut s = connect(rig.port);
    // One byte every 200 ms never lets a 300 ms inactivity timeout fire, but the 600 ms head
    // deadline still ends it.
    let started = Instant::now();
    let mut closed_after = None;
    for b in b"GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n" {
        if s.write_all(&[*b]).is_err() {
            closed_after = Some(started.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
        s.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        let mut buf = [0u8; 256];
        match s.read(&mut buf) {
            Ok(_) => {
                closed_after = Some(started.elapsed());
                break;
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => {
                closed_after = Some(started.elapsed());
                break;
            }
        }
    }
    let after = closed_after.expect("the drip was never cut off");
    assert!(after < Duration::from_millis(2500), "cut off after {after:?}");
}

#[test]
fn an_idle_connection_is_closed_silently() {
    let rig = start(HttpLimits::with_admission(fast_wire(), AdmissionLimits::default()));
    let mut s = connect(rig.port);
    let started = Instant::now();
    let (out, end) = drain(&mut s, Duration::from_secs(4));
    assert_eq!(end, End::Closed);
    assert!(out.is_empty(), "nothing was asked, nothing is answered: {out:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
    wait_for(|| rig.conns.live() == 0, Duration::from_secs(10), "the registry to empty");
}

// ---------------------------------------------------------------------------
// Admission and the registry, on real sockets
// ---------------------------------------------------------------------------

#[test]
fn the_total_connection_cap_is_enforced_on_real_sockets() {
    let admission = AdmissionLimits {
        max_total: 6,
        loopback_reserved: 2,
        max_per_client: 16,
    };
    let mut wire = fast_wire();
    wire.keep_alive_idle = Duration::from_secs(30); // the held connections must stay open
    let rig = start(HttpLimits::with_admission(wire, admission));
    let held: Vec<TcpStream> = (0..6).map(|_| connect(rig.port)).collect();
    wait_for(|| rig.conns.live() == 6, Duration::from_secs(10), "six admitted connections");

    // The seventh gets 503 + Retry-After and is closed; it is not queued.
    let mut extra = connect(rig.port);
    let (out, end) = drain(&mut extra, Duration::from_secs(3));
    let text = String::from_utf8_lossy(&out).to_string();
    assert!(text.starts_with("HTTP/1.1 503"), "{text:?}");
    assert!(text.contains("Retry-After: 5"), "{text:?}");
    assert_eq!(end, End::Closed);
    assert_eq!(rig.conns.live(), 6, "a refused connection is never registered");

    // Freeing one makes room again.
    drop(held);
    wait_for(|| rig.conns.live() == 0, Duration::from_secs(10), "the held connections to end");
    assert!(get_status(rig.port).contains(" 200 "));
}

/// SECFIX_03 (SRV-02): a body whose framing cannot be trusted is answered `400` and the
/// connection is closed. Upstream treated `abc`/`-1` as "no body" and parsed the bytes that
/// followed as the next request head, which is how a request is smuggled in.
#[test]
fn an_unparsable_or_duplicated_content_length_is_refused() {
    let rig = start(HttpLimits::default());
    let base = "POST /api/transport/next HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Plippa: 1\r\nContent-Type: application/json\r\n";
    for (label, framing) in [
        ("not a number", "Content-Length: abc\r\n"),
        ("negative", "Content-Length: -1\r\n"),
        ("signed", "Content-Length: +5\r\n"),
        ("empty", "Content-Length: \r\n"),
        ("two that disagree", "Content-Length: 0\r\nContent-Length: 5\r\n"),
        ("an encoding we do not implement", "Transfer-Encoding: gzip\r\n"),
    ] {
        let mut s = connect(rig.port);
        let head = format!("{base}{framing}\r\n{{}}");
        s.write_all(head.as_bytes()).unwrap();
        s.flush().unwrap();
        let (out, end) = drain(&mut s, Duration::from_secs(3));
        assert_eq!(status_line(&out), "HTTP/1.1 400 Bad Request", "{label}: {out:?}");
        assert_eq!(end, End::Closed, "{label}: the connection must close");
    }
    // The server is still serving afterwards.
    assert!(get_status(rig.port).starts_with("HTTP/1.1 200"));
}

#[test]
fn the_registry_force_closes_live_sockets_by_predicate() {
    let mut wire = fast_wire();
    wire.keep_alive_idle = Duration::from_secs(30);
    let rig = start(HttpLimits::with_admission(wire, AdmissionLimits::default()));
    let mut socks: Vec<TcpStream> = (0..3).map(|_| connect(rig.port)).collect();
    wait_for(|| rig.conns.live() == 3, Duration::from_secs(10), "three connections");
    assert_eq!(rig.conns.snapshot().len(), 3);
    assert!(rig.conns.snapshot().iter().all(|c| c.is_loopback()));

    // A predicate that matches nothing closes nothing.
    assert_eq!(rig.conns.close_matching(|c| !c.is_loopback()), 0);
    let (_, end) = drain(&mut socks[0], Duration::from_millis(300));
    assert_eq!(end, End::StillOpen);

    // "Everything on loopback" closes all three, and they leave the registry.
    assert_eq!(rig.conns.close_matching(|c| c.is_loopback()), 3);
    for s in &mut socks {
        let (_, end) = drain(s, Duration::from_secs(2));
        assert_eq!(end, End::Closed);
    }
    wait_for(|| rig.conns.live() == 0, Duration::from_secs(10), "the registry to empty");
    assert!(get_status(rig.port).contains(" 200 "), "the server keeps serving");
}

// ---------------------------------------------------------------------------
// Behaviour that must not change
// ---------------------------------------------------------------------------

#[test]
fn keep_alive_and_pipelined_requests_are_still_answered_in_order() {
    let rig = start(HttpLimits::with_admission(fast_wire(), AdmissionLimits::default()));
    let mut s = connect(rig.port);
    let req = "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
    // Two requests in one write (pipelining) ...
    s.write_all(format!("{req}{req}").as_bytes()).unwrap();
    let count = |buf: &[u8]| String::from_utf8_lossy(buf).matches("HTTP/1.1 200").count();
    let mut seen = Vec::new();
    let t = Instant::now();
    s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    while count(&seen) < 2 && t.elapsed() < Duration::from_secs(3) {
        let mut buf = [0u8; 8192];
        if let Ok(n) = s.read(&mut buf) {
            seen.extend_from_slice(&buf[..n]);
        }
    }
    assert_eq!(count(&seen), 2, "{:?}", String::from_utf8_lossy(&seen));
    // ... and a third on the same, still-open connection.
    s.write_all(req.as_bytes()).unwrap();
    let t = Instant::now();
    while count(&seen) < 3 && t.elapsed() < Duration::from_secs(3) {
        let mut buf = [0u8; 8192];
        if let Ok(n) = s.read(&mut buf) {
            seen.extend_from_slice(&buf[..n]);
        }
    }
    assert_eq!(count(&seen), 3, "keep-alive must still work");
}

// ---------------------------------------------------------------------------
// Request smuggling through an unread body
// ---------------------------------------------------------------------------

/// A request that, if it were ever parsed, passes the CSRF guard (no `Origin`, forged
/// `X-Plippa: 1`). It is only ever sent as the *tail of a body* the server refused to read.
const FORGED: &str = "POST /api/transport/next HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Plippa: 1\r\nContent-Length: 0\r\n\r\n";

fn responses(bytes: &[u8]) -> usize {
    String::from_utf8_lossy(bytes).matches("HTTP/1.1 ").count()
}

/// Send `head` then `parts`, ignoring write errors (the server may close early), and return
/// everything the server answered and how the connection ended.
fn smuggle(port: u16, head: &str, parts: &[&[u8]]) -> (Vec<u8>, End) {
    let mut s = connect(port);
    s.write_all(head.as_bytes()).unwrap();
    'send: for part in parts {
        for piece in part.chunks(64 * 1024) {
            if s.write_all(piece).is_err() {
                break 'send;
            }
        }
    }
    drain(&mut s, Duration::from_secs(6))
}

#[test]
fn an_unread_body_is_never_parsed_as_the_next_request() {
    let rig = start(HttpLimits::with_admission(fast_wire(), AdmissionLimits::default()));
    let refused_head = |extra: &str| {
        format!(
            "POST /api/transport/next HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://evil.example\r\nContent-Type: text/plain\r\n{extra}\r\n"
        )
    };

    // 1. Exactly the drain budget (16 MiB) of padding, then the forged request.
    let pad = vec![b'a'; 16 * 1024 * 1024];
    let total = pad.len() + FORGED.len();
    let (out, end) = smuggle(
        rig.port,
        &refused_head(&format!("Content-Length: {total}\r\n")),
        &[&pad, FORGED.as_bytes()],
    );
    assert_eq!(responses(&out), 1, "{:?}", String::from_utf8_lossy(&out));
    assert!(status_line(&out).contains(" 403 "));
    assert_eq!(end, End::Closed, "the connection must be closed after an undrained body");

    // 2. A chunked body (which was never drained at all): the forged request sits in a chunk.
    let chunk = format!("{:x}\r\n{FORGED}\r\n0\r\n\r\n", FORGED.len());
    let (out, end) = smuggle(
        rig.port,
        &refused_head("Transfer-Encoding: chunked\r\n"),
        &[chunk.as_bytes()],
    );
    assert_eq!(responses(&out), 1, "{:?}", String::from_utf8_lossy(&out));
    assert_eq!(end, End::Closed);

    // 3. The client stalls mid-body: the drain times out, and the connection is closed.
    let part = format!("{FORGED}{}", "b".repeat(2000));
    let (out, end) = smuggle(
        rig.port,
        &refused_head("Content-Length: 1048576\r\n"),
        &[part.as_bytes()],
    );
    assert_eq!(responses(&out), 1, "{:?}", String::from_utf8_lossy(&out));
    assert_eq!(end, End::Closed);

    // A body that is small enough to be drained completely keeps the connection usable.
    let mut s = connect(rig.port);
    let body = vec![b'c'; 100 * 1024];
    write!(
        s,
        "POST /api/transport/next HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://evil.example\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap();
    s.write_all(&body).unwrap();
    write!(s, "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
    let (out, _) = drain(&mut s, Duration::from_secs(4));
    assert_eq!(responses(&out), 2, "{:?}", String::from_utf8_lossy(&out));
}

#[test]
fn descriptor_and_memory_exhaustion_do_not_end_the_accept_loop() {
    use std::io::{Error, ErrorKind};
    // EMFILE, ENFILE, ENOMEM, ENOBUFS and an aborted connection are "not now" (the accept loop
    // logs, backs off and carries on); a broken listener (EBADF, EINVAL) still ends it.
    for code in [24, 23, 12, 105] {
        assert!(tiny_http::is_transient_accept_error(&Error::from_raw_os_error(code)), "{code}");
    }
    assert!(tiny_http::is_transient_accept_error(&Error::from(ErrorKind::ConnectionAborted)));
    for code in [9, 22] {
        assert!(!tiny_http::is_transient_accept_error(&Error::from_raw_os_error(code)), "{code}");
    }
}
