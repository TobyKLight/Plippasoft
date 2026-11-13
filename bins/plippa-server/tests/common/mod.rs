//! Shared by the integration tests (SECFIX_02): start the portal the way the device runs it with
//! the switch **on** and the PIN **not required**, so a test can exercise the API behind the
//! gate without hand-copying the sign-in set-up. The gate itself is tested in `portal_auth.rs`.
//!
//! Each test file builds its own [`ServerConfig`] (`..ServerConfig::default()` keeps it valid as
//! fields are added) and hands it to [`serve_open`]. No control listener is started: these tests
//! talk to the portal port.
#![allow(dead_code)]

use plippa_server::{
    serve_with_access, HttpLimits, LockoutPolicy, ManualClock, PortalAccess, ServerConfig,
    ServerService,
};
use std::sync::{Arc, Mutex};

/// A portal switch that is on, with no PIN required, kept in memory.
pub fn open_access() -> Arc<PortalAccess> {
    let access = Arc::new(PortalAccess::memory(
        Arc::new(ManualClock::new()),
        LockoutPolicy::default(),
    ));
    // SECFIX_03: these suites exercise the plain-HTTP portal on a loopback port. The HTTPS
    // listener (certificate, `307` redirect, `Secure` cookie) has its own file, `portal_tls.rs`.
    access.set_https_enabled(false).expect("plain HTTP");
    access.set_enabled(true).expect("enable the portal");
    access.set_pin_required(false).expect("no PIN required");
    access
}

/// Serve `cfg.bind` on a background thread, open (no PIN). Returns the access handle.
pub fn serve_open(cfg: ServerConfig, svc: Arc<Mutex<ServerService>>) -> Arc<PortalAccess> {
    serve_open_with_limits(cfg, svc, HttpLimits::default())
}

/// [`serve_open`] with explicit limits (the portal listener's wire caps and admission).
pub fn serve_open_with_limits(
    cfg: ServerConfig,
    svc: Arc<Mutex<ServerService>>,
    limits: HttpLimits,
) -> Arc<PortalAccess> {
    serve_with(cfg, svc, limits, open_access())
}

/// Serve with an access object the test built (a PIN, an injected clock ...).
pub fn serve_with(
    cfg: ServerConfig,
    svc: Arc<Mutex<ServerService>>,
    limits: HttpLimits,
    access: Arc<PortalAccess>,
) -> Arc<PortalAccess> {
    let cfg = ServerConfig {
        control_bind: None,
        ..cfg
    };
    let handle = Arc::clone(&access);
    std::thread::spawn(move || {
        let _ = serve_with_access(cfg, svc, None, limits, handle);
    });
    access
}
