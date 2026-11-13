//! NB_VERIFY NB_10: a browser opens ~6 keep-alive connections at once when it loads the portal.
//! tiny_http 0.12's worker pool could queue one of them behind the others (stale "idle worker"
//! count), and because the other connections stay open (keep-alive) that request was never
//! answered: the page sat at "loading" until the browser gave up. See `third_party/tiny_http`.

mod common;

use plippa_server::{RecordingIpc, ServerConfig, ServerService};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn start_http(root: &std::path::Path) -> u16 {
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let svc = Arc::new(Mutex::new(ServerService::start(
        Some(root.to_path_buf()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    )));
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        library_root: Some(root.to_path_buf()),
        secrets_dir: None,
        portal_url: None,
        ..ServerConfig::default()
    };
    common::serve_open(cfg, svc);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return port;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

#[test]
fn many_simultaneous_keep_alive_connections_are_all_answered() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("lib");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(".gitkeep"), b"").unwrap();
    // Fresh server per round: the race is in the worker pool right after start-up.
    for round in 0..15 {
        let port = start_http(&root);
        let mut socks: Vec<TcpStream> = (0..8)
            .map(|_| {
                let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
                s
            })
            .collect();
        for s in &mut socks {
            write!(s, "GET /api/kinds HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").unwrap();
        }
        // The connections stay open (keep-alive) while we read, exactly like a browser.
        for (i, s) in socks.iter_mut().enumerate() {
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).unwrap_or(0);
            assert!(
                buf[..n].starts_with(b"HTTP/1.1 200"),
                "round {round}: connection {i} of 8 got no answer (pool starved)"
            );
        }
    }
}
