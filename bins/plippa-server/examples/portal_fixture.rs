//! Dev tool: serve the portal over a library folder with a fake engine (no audio, no NFC), so
//! the web UI can be driven by a browser / Playwright.
//!
//! `cargo run -p plippa-server --example portal_fixture -- <library-dir> [port]`
#[path = "../tests/fake_engine/mod.rs"]
mod fake_engine;

use plippa_server::{
    serve_with_access, HttpLimits, LockoutPolicy, ManualClock, PortalAccess, RecordingIpc,
    ServerConfig, ServerService,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = std::path::PathBuf::from(
        args.next()
            .expect("usage: portal_fixture <library-dir> [port]"),
    );
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(8799);
    let ipc = RecordingIpc::new();
    let svc = Arc::new(Mutex::new(ServerService::start(
        Some(root.clone()),
        Box::new(ipc.clone()),
        ServerService::default_mock_associate(),
    )));
    fake_engine::spawn(ipc, Arc::clone(&svc), Duration::from_millis(0));
    let cfg = ServerConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        library_root: Some(root),
        secrets_dir: None,
        portal_url: None,
        // A fixture for browser tests: no control listener, and a portal that is on with no PIN.
        control_bind: None,
        ..ServerConfig::default()
    };
    let access = Arc::new(PortalAccess::memory(
        Arc::new(ManualClock::new()),
        LockoutPolicy::default(),
    ));
    access.set_enabled(true).expect("enable");
    access.set_pin_required(false).expect("open");
    // A browser fixture served on `http://127.0.0.1`: HTTPS is the device default and fails
    // closed without a secrets directory, so this fixture asks for plain HTTP explicitly.
    access.set_https_enabled(false).expect("plain HTTP");
    serve_with_access(cfg, svc, None, HttpLimits::default(), access).expect("serve");
}
