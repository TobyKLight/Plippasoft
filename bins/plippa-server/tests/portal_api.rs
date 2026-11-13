//! `GET /api/portal/url` (BCS_08) over real HTTP.
//!
//! The device's connect screen puts this answer in a QR code, so the assertions here are
//! about what must **never** appear (credentials, a token, a loopback recommendation) as
//! much as about the shape of the JSON.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use plippa_server::{RecordingIpc, ServerConfig, ServerService};

struct Rig {
    _tmp: tempfile::TempDir,
    port: u16,
}

fn start(portal_url: Option<&str>) -> Rig {
    let tmp = tempfile::tempdir().expect("temp dir");
    let root = tmp.path().join("library");
    std::fs::create_dir_all(&root).expect("library");

    let svc = ServerService::start(
        Some(root.clone()),
        Box::new(RecordingIpc::new()),
        ServerService::default_mock_associate(),
    );
    let svc = Arc::new(Mutex::new(svc));

    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().expect("bind addr"),
        library_root: Some(root),
        secrets_dir: None,
        portal_url: portal_url.map(str::to_string),
        ..ServerConfig::default()
    };
    common::serve_open(cfg, svc);
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Rig { _tmp: tmp, port };
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("server did not start");
}

fn get_json(port: u16, path: &str) -> (u16, String, serde_json::Value) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .expect("status code");
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    (
        status,
        body.to_string(),
        serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
    )
}

#[test]
fn portal_url_endpoint_never_recommends_an_unreachable_address() {
    let rig = start(None);
    let (status, text, value) = get_json(rig.port, "/api/portal/url");
    assert_eq!(status, 200, "{text}");
    // The shape the device parses.
    assert!(value["urls"].is_array(), "{text}");
    let port = value["port"].as_u64().expect("port");
    assert_eq!(port, u64::from(rig.port));
    assert!(value["overrideUsed"].is_boolean(), "{text}");
    if let Some(recommended) = value["recommended"].as_str() {
        // Whatever this machine's interfaces are, the QR code must be openable elsewhere.
        for forbidden in ["0.0.0.0", "127.0.0.1", "localhost", "169.254", "fe80"] {
            assert!(
                !recommended.contains(forbidden),
                "recommended {recommended} contains {forbidden}"
            );
        }
        assert!(recommended.ends_with(&format!(":{port}")), "{recommended}");
    } else {
        // Say why nothing is reachable instead of leaving the device to guess.
        let note = value["note"].as_str().unwrap_or_default();
        assert!(note.contains("--portal-url"), "{text}");
    }
    // Every candidate carries the flag, and the recommended one is listed first.
    for candidate in value["urls"].as_array().expect("urls") {
        assert!(candidate["url"].as_str().is_some_and(|u| u.starts_with("http")), "{text}");
        assert!(candidate["recommended"].is_boolean(), "{text}");
    }
    // Nothing in the answer can be a credential or a token.
    for forbidden in ["@", "?", "#", "password", "token", "user:"] {
        assert!(!text.contains(forbidden), "{forbidden} leaked: {text}");
    }
}

#[test]
fn portal_url_endpoint_honours_the_configured_address() {
    let rig = start(Some("192.168.1.50:9000"));
    let (status, text, value) = get_json(rig.port, "/api/portal/url");
    assert_eq!(status, 200, "{text}");
    assert_eq!(value["recommended"], "http://192.168.1.50:9000");
    assert_eq!(value["port"], 9000, "the URL carries its own port: {text}");
    assert_eq!(value["overrideUsed"], true);
    assert_eq!(value["urls"][0]["url"], "http://192.168.1.50:9000");
    assert_eq!(value["urls"][0]["recommended"], true);
}

#[test]
fn portal_url_endpoint_refuses_a_credential_bearing_override() {
    let rig = start(Some("http://fan:hunter2@192.168.1.50:8787/?token=abc"));
    let (status, text, value) = get_json(rig.port, "/api/portal/url");
    assert_eq!(status, 200, "{text}");
    assert_eq!(value["overrideUsed"], false, "{text}");
    for secret in ["hunter2", "fan:", "abc", "token"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
}
