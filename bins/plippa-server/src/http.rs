//! Minimal LAN HTTP front for the media portal (`tiny_http`, sync).

use crate::admission::{client_id, AdmissionLimits, Connections};
use crate::config::ServerConfig;
use crate::portal_access::{
    constant_time_eq, expired_session_cookie, session_cookie, session_token_from_cookie,
    AccessError, LoginOutcome, PortalAccess, TimedCache,
};
use crate::limits::HttpLimits;
use crate::service::{
    associate_playable_keep_existing_unlocked, associate_playable_unlocked,
    associate_track_unlocked, play_collection_unlocked, AddPlaylistItems, PlayCollection,
    preview_cues, CreatePlaylist, CuePreviewRequest, DeleteError, DeleteOutcome, DeletePreview,
    DeleteRequest, MetadataPatch, RenamePlaylist, ReorderPlaylist, SaveCuesRequest, ServerError,
    ServerService, StoreApiError, MAX_CUE_PASTE_BYTES,
};
use crate::unite::sniff::{sniff_audio, MediaSniff};
use crate::service::{
    stage_cover, CoverPlan, CoverScope, CoverSize, ImportFinish, ImportHub, PutCovers,
    MAX_COVER_BYTES,
};
use crate::web::download::describe_bytes;
use crate::unite::hub::{
    ApplyRequest, ConfirmRequest, DownloadRequest, PrepareRequest, StartRequest, UniteError,
};
use crate::unite::refresh::{refresh, refresh_install_images, refresh_prepare, RefreshRequest};
use crate::web::fetch::WebError;
use crate::web::{fetch_unlocked, is_token as is_web_token, prepare_unlocked, WebRequest};
use plippa_noubin::covers::ThumbTier;
use plippa_noubin::{ImportKind, LibraryKind};
use serde::Serialize;
use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// Portal Content-Security-Policy: same-origin scripts/styles only (no inline script), no
/// plugins, no `<base>`, cannot be framed.
pub(crate) const CSP: &str = "default-src 'self'; img-src 'self' data:; media-src 'self' blob:; \
object-src 'none'; base-uri 'none'; frame-ancestors 'none'";

/// Cap for JSON bodies of the older endpoints (NBSecurityRequirements §5.4).
const MAX_JSON_BODY: usize = 1024 * 1024;
const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const COVERS_JS: &str = include_str!("../web/covers.js");
const LIBRARY_JS: &str = include_str!("../web/library.js");
const UNITE_JS: &str = include_str!("../web/unite.js");
const STORES_JS: &str = include_str!("../web/stores.js");
const DELETEITEM_JS: &str = include_str!("../web/deleteitem.js");
const UI_JS: &str = include_str!("../web/ui.js");
const SIGNIN_JS: &str = include_str!("../web/signin.js");
const ADDMEDIA_JS: &str = include_str!("../web/addmedia.js");
const EDITCUES_JS: &str = include_str!("../web/editcues.js");
const EDITCOVERS_JS: &str = include_str!("../web/editcovers.js");
const STYLES_CSS: &str = include_str!("../web/styles.css");

/// Run the HTTP server until the process exits (or the control listener fails).
///
/// Each request runs on its own thread so a long associate wait cannot starve
/// `/api/status` (MVP_14).
pub fn serve(cfg: ServerConfig, service: Arc<Mutex<ServerService>>) -> Result<(), String> {
    serve_until(cfg, service, None)
}

/// [`serve`] with a shutdown flag: when `stop` is set the accept loops unblock and return so
/// the caller can quiesce the service and close SQLite before the host powers off (PWRSW_01).
pub fn serve_until(
    cfg: ServerConfig,
    service: Arc<Mutex<ServerService>>,
    stop: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), String> {
    serve_with_limits(cfg, service, stop, HttpLimits::default())
}

/// [`serve_until`] with explicit [`HttpLimits`] (SECFIX_01). The limits (wire caps, timeouts and
/// admission) apply to the **portal** listener; `limits.connections` is the registry of its live
/// connections, which the caller may keep a clone of to list them.
///
/// The portal PIN state is read from `cfg.secrets_dir` (see [`PortalAccess`]).
pub fn serve_with_limits(
    cfg: ServerConfig,
    service: Arc<Mutex<ServerService>>,
    stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    limits: HttpLimits,
) -> Result<(), String> {
    let access = Arc::new(PortalAccess::open(cfg.secrets_dir.as_deref()));
    serve_with_access(cfg, service, stop, limits, access)
}

/// [`serve_with_limits`] with the portal access state passed in (tests inject a clock and keep a
/// handle to read or change the settings directly).
///
/// SECFIX_02: two listeners.
///
/// * The **control listener** (`cfg.control_bind`, `127.0.0.1:8786`) is always on, plain HTTP on
///   the loopback interface, and answers only requests that carry the per-boot token. It serves
///   the whole API for the on-device UI and the portal settings routes. `None` means no control
///   listener (tests that only need the portal).
/// * The **portal listener** (`cfg.bind`, the LAN) exists only while the portal is switched on:
///   [`Shared::reconcile`] opens the socket when it is enabled and closes it, dropping every
///   live connection, when it is disabled. It serves the browser portal behind the PIN.
///
/// Blocks until `stop` is set (or forever when it is `None`).
pub fn serve_with_access(
    cfg: ServerConfig,
    service: Arc<Mutex<ServerService>>,
    stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    limits: HttpLimits,
    access: Arc<PortalAccess>,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;
    let stop = stop.unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let token = control_token(cfg.control_token_path.as_deref());

    let shared = Arc::new(Shared {
        service,
        portal: crate::portal::PortalConfig::new(cfg.bind, cfg.portal_url.clone()),
        access,
        // SECFIX_03: the certificate lives with the other device secrets, never on the stick.
        tls: cfg.secrets_dir.as_deref().map(crate::tls::CertStore::new),
        control_token: token,
        limits: limits.clone(),
        status_cache: TimedCache::new(STATUS_CACHE_TTL),
        epoch: Instant::now(),
        running: Mutex::new(None),
        shutting_down: AtomicBool::new(false),
        reconcile_lock: Mutex::new(()),
        listen_error: Mutex::new(None),
        log_gate: Mutex::new(std::collections::HashMap::new()),
    });

    // The control listener first: the on-device UI must never be left without it.
    let control = match cfg.control_bind {
        Some(addr) => {
            let conns = Connections::new(AdmissionLimits {
                max_total: 32,
                loopback_reserved: 0,
                max_per_client: 32,
            });
            let control_limits = HttpLimits {
                wire: limits.wire.clone(),
                connections: conns,
            };
            let server = bind_server(addr, &control_limits, None)?;
            eprintln!("plippa-server control listener on http://{addr}/ (device UI only)");
            Some((server, control_limits))
        }
        None => None,
    };

    // The portal listener follows the stored switch (off in factory default).
    if let Err(e) = shared.reconcile() {
        eprintln!("plippa-server: the web portal could not start: {e}");
    }
    // While the setting says enabled but the socket is not open (the port was busy at boot, a
    // change could not be applied) keep trying with a growing, bounded gap; also retry a state
    // write a failed save left behind.
    let retry = {
        let shared = Arc::clone(&shared);
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("plippa-portal-retry".into())
            .spawn(move || {
                let mut gap = 1u64;
                let mut waited = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(100));
                    waited += 100;
                    if waited < gap * 1000 {
                        continue;
                    }
                    waited = 0;
                    shared.access.flush();
                    if shared.access.enabled() && !shared.listening() {
                        match shared.reconcile() {
                            Ok(()) => gap = 1,
                            Err(_) => gap = (gap * 2).min(30),
                        }
                    } else {
                        gap = 1;
                    }
                }
            })
            .ok()
    };

    let result = match control {
        Some((server, control_limits)) => {
            serve_loop(&server, &shared, Surface::Control, &stop, &control_limits)
        }
        None => {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(())
        }
    };
    shared.shutting_down.store(true, Ordering::SeqCst);
    stop.store(true, Ordering::SeqCst);
    if let Some(handle) = retry {
        let _ = handle.join();
    }
    shared.stop_portal();
    result
}

/// Tries to open the LAN socket per reconcile, and the gap between them.
const BIND_ATTEMPTS: u32 = 5;
const BIND_RETRY_GAP: Duration = Duration::from_millis(200);

/// How long a computed status is reused (`SRV-08`: rapid polling must not take the service lock
/// each time).
const STATUS_CACHE_TTL: Duration = Duration::from_secs(1);

/// What both listeners share.
pub(crate) struct Shared {
    service: Arc<Mutex<ServerService>>,
    portal: crate::portal::PortalConfig,
    access: Arc<PortalAccess>,
    /// SECFIX_03: where the portal's certificate and key live (`None` when no secrets
    /// directory is configured, which means HTTPS cannot be served at all).
    tls: Option<crate::tls::CertStore>,
    /// Per-boot secret the on-device UI sends as `X-Plippa-Local`.
    control_token: String,
    /// Wire limits and the connection registry of the portal listener.
    limits: HttpLimits,
    status_cache: TimedCache<crate::service::LibraryStatus>,
    epoch: Instant,
    /// The open portal listener, if any.
    running: Mutex<Option<RunningPortal>>,
    shutting_down: AtomicBool,
    /// Held for the whole of a [`Shared::reconcile`]: an enable that arrives while a disable is
    /// still draining waits for it instead of racing it for the port.
    reconcile_lock: Mutex<()>,
    /// Why the LAN socket could not be opened, while the portal is on but not listening.
    listen_error: Mutex<Option<String>>,
    /// Sign-in log lines per address: `(last line at ms, lines held back since)`.
    log_gate: Mutex<std::collections::HashMap<IpAddr, (u64, u32)>>,
}

struct RunningPortal {
    stop: Arc<AtomicBool>,
    handles: Vec<std::thread::JoinHandle<()>>,
    addr: SocketAddr,
    /// SECFIX_03: this listener accepted TLS (as opposed to plain HTTP).
    secure: bool,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// The LAN socket is open right now.
    pub(crate) fn listening(&self) -> bool {
        self.running.lock().map(|r| r.is_some()).unwrap_or(false)
    }

    /// The port the LAN listener is on, while it is up. The `307` target uses this rather than
    /// echoing whatever port the `Host` header named.
    fn portal_port(&self) -> Option<u16> {
        self.running
            .lock()
            .ok()?
            .as_ref()
            .map(|portal| portal.addr.port())
    }

    /// Make the LAN listener match the stored switch: open it when the portal is enabled, close it
    /// (and every live portal connection) when it is not.
    ///
    /// Serialised: only one reconcile runs at a time, so an enable that arrives while a disable is
    /// draining waits for it. Opening retries a few times (a port that was only just released, or
    /// busy for a moment); if it still fails the reason is kept for the device menu and the error
    /// is returned. [`serve_with_access`]'s retry thread then keeps trying while the setting says
    /// enabled.
    pub(crate) fn reconcile(self: &Arc<Self>) -> Result<(), String> {
        let _serial = self.reconcile_lock.lock().unwrap_or_else(|p| p.into_inner());
        let want = self.access.enabled() && !self.shutting_down.load(Ordering::SeqCst);
        let want_secure = self.access.https_enabled();
        let have = self
            .running
            .lock()
            .map(|r| r.as_ref().map(|p| p.secure))
            .unwrap_or(None);
        let set_error = |e: Option<String>| {
            *self.listen_error.lock().unwrap_or_else(|p| p.into_inner()) = e;
        };
        // A protocol change is a rebind: the listener must come down and go up again, and every
        // live connection goes with it (a session cookie's `Secure` flag depends on the scheme,
        // so keeping the old connections would keep the old promise).
        if have.is_some_and(|secure| secure != (want && want_secure)) {
            self.stop_portal();
        }
        let have = self.listening();
        if want && !have {
            let mut last = String::new();
            for attempt in 0..BIND_ATTEMPTS {
                match self.start_portal() {
                    Ok(()) => {
                        set_error(None);
                        return Ok(());
                    }
                    Err(e) => last = e,
                }
                if attempt + 1 < BIND_ATTEMPTS {
                    std::thread::sleep(BIND_RETRY_GAP);
                }
            }
            set_error(Some(last.clone()));
            Err(last)
        } else {
            if !want && have {
                self.stop_portal();
            }
            set_error(None);
            Ok(())
        }
    }

    /// SECFIX_03: issue a new certificate and drop the live listener, so the next connection is
    /// offered the new one. The caller rebinds ([`Shared::reconcile`]).
    fn regenerate_portal_certificate(&self) -> Result<(), String> {
        let store = self
            .tls
            .as_ref()
            .ok_or_else(|| "no secrets directory to keep the portal certificate in".to_string())?;
        let mut cert = store.regenerate(&crate::tls::Hosts::local())?;
        // Only claim a fingerprint for a listener that will actually present it.
        if self.access.https_enabled() {
            self.access.set_fingerprint(Some(std::mem::take(&mut cert.fingerprint)));
        }
        self.stop_portal();
        Ok(())
    }

    /// The settings as the device menu sees them, with the listener's state.
    pub(crate) fn settings_now(&self) -> crate::portal_access::PortalSettings {
        let mut settings = self.access.settings(self.listening());
        settings.listen_error = self
            .listen_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        settings
    }

    /// One sign-in log line, but at most one per address per minute (the rest are counted and
    /// reported on the next line), so a flood of wrong PINs cannot flood the log.
    fn log_sign_in(&self, client: IpAddr, what: &str) {
        let now = self.now_ms();
        let mut gate = self.log_gate.lock().unwrap_or_else(|p| p.into_inner());
        if gate.len() > 1024 {
            gate.retain(|_, (at, _)| now.saturating_sub(*at) < 60_000);
            if gate.len() > 1024 {
                gate.clear();
            }
        }
        let entry = gate.entry(client).or_insert((0, 0));
        let first = entry.0 == 0 && entry.1 == 0;
        if first || now.saturating_sub(entry.0) >= 60_000 {
            let held = std::mem::take(&mut entry.1);
            entry.0 = now.max(1);
            drop(gate);
            if held > 0 {
                eprintln!("plippa-server: portal sign-in {what} from {client} (and {held} more in the last minute)");
            } else {
                eprintln!("plippa-server: portal sign-in {what} from {client}");
            }
        } else {
            entry.1 = entry.1.saturating_add(1);
        }
    }

    fn start_portal(self: &Arc<Self>) -> Result<(), String> {
        let mut running = self.running.lock().map_err(|e| e.to_string())?;
        if running.is_some() {
            return Ok(());
        }
        // SECFIX_03: HTTPS is the default and fails closed. If the certificate cannot be made
        // or read, the portal does not come up at all: it never silently falls back to plain
        // HTTP, because the owner believes the portal is encrypted.
        let secure = self.access.https_enabled();
        let ssl = if secure {
            let store = self
                .tls
                .as_ref()
                .ok_or_else(|| "no secrets directory to keep the portal certificate in".to_string())?;
            let mut cert = store.load_or_create(&crate::tls::Hosts::local())?;
            self.access.set_fingerprint(Some(cert.fingerprint.clone()));
            Some(tiny_http::SslConfig {
                // `take` so the copies that go to rustls are the only ones left; the `Certificate`
                // is wiped on drop and its key buffer is empty by then.
                certificate: std::mem::take(&mut cert.cert_der),
                private_key: std::mem::take(&mut cert.key_der),
                // One port for both: a browser that typed `http://` is redirected, not left
                // staring at a failed handshake.
                accept_plaintext: true,
            })
        } else {
            self.access.set_fingerprint(None);
            None
        };
        let server = bind_server(self.portal.bind, &self.limits, ssl)?;
        let addr = match server.server_addr() {
            tiny_http::ListenAddr::IP(a) => a,
            #[cfg(unix)]
            _ => self.portal.bind,
        };
        print_listen_urls(addr, secure);
        let stop = Arc::new(AtomicBool::new(false));
        let shared = Arc::clone(self);
        let flag = Arc::clone(&stop);
        let serve = std::thread::Builder::new()
            .name("plippa-http-portal".into())
            .spawn(move || {
                let _ = serve_loop(
                    &server,
                    &shared,
                    Surface::Portal { secure },
                    &flag,
                    &shared.limits,
                );
            })
            .map_err(|e| format!("spawn the portal listener: {e}"))?;
        *running = Some(RunningPortal {
            stop,
            handles: vec![serve],
            addr,
            secure,
        });
        Ok(())
    }

    /// Close the LAN socket, then drop every live portal connection. Returns once new
    /// connections are refused.
    pub(crate) fn stop_portal(&self) {
        let taken = self.running.lock().ok().and_then(|mut r| r.take());
        let Some(portal) = taken else { return };
        self.access.set_fingerprint(None);
        portal.stop.store(true, Ordering::SeqCst);
        for handle in portal.handles {
            let _ = handle.join();
        }
        // The accept thread owns the listening socket until it notices the close; wait until a
        // connect is really refused so "off" means off by the time we return.
        let probe = if portal.addr.ip().is_unspecified() {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), portal.addr.port())
        } else {
            portal.addr
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && std::net::TcpStream::connect_timeout(&probe, Duration::from_millis(200)).is_ok()
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        // Everything on the portal's registry is portal traffic (the on-device UI uses the control
        // listener), so all of it goes, loopback test clients included.
        let closed = self.limits.connections.close_matching(|_| true);
        if closed > 0 {
            eprintln!("plippa-server: web portal off, dropped {closed} connection(s)");
        }
    }
}

/// Bind a `tiny_http` server with `limits`.
fn bind_server(
    addr: SocketAddr,
    limits: &HttpLimits,
    ssl: Option<tiny_http::SslConfig>,
) -> Result<Arc<Server>, String> {
    let bind_err = |e: &dyn std::fmt::Display| format!("bind {addr}: {e}");
    let listen = tiny_http::ConfigListenAddr::from_socket_addrs(addr).map_err(|e| bind_err(&e))?;
    Server::with_options(
        tiny_http::ServerConfig { addr: listen, ssl },
        tiny_http::ServerOptions {
            limits: limits.wire.clone(),
            gate: Some(limits.gate()),
        },
    )
    .map(Arc::new)
    .map_err(|e| bind_err(&e))
}

/// Serve `server` until `stop` is set: one handler thread per request, bounded by the
/// connection cap of `limits`.
fn serve_loop(
    server: &Arc<Server>,
    shared: &Arc<Shared>,
    surface: Surface,
    stop: &Arc<AtomicBool>,
    limits: &HttpLimits,
) -> Result<(), String> {
    let watcher = {
        let srv = Arc::clone(server);
        let stop = Arc::clone(stop);
        std::thread::Builder::new()
            .name("plippa-http-stop".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(25));
                }
                srv.unblock();
            })
            .map_err(|e| format!("spawn stop watcher: {e}"))?
    };

    let handlers = Arc::new(HandlerPermits::new(limits.max_handlers()));
    for request in server.incoming_requests() {
        let permit = handlers.acquire();
        let shared = Arc::clone(shared);
        let spawned = std::thread::Builder::new()
            .name("plippa-http-req".into())
            .spawn(move || {
                let _permit = permit;
                if let Err(e) = handle_one(request, &shared, surface) {
                    eprintln!("request error: {e}");
                }
            });
        if let Err(e) = spawned {
            // The closure (with the request) is dropped, which answers it with a 500.
            eprintln!("could not start a request thread: {e}");
        }
    }
    // `incoming_requests` ended: the stop flag was set (the watcher is done or about to be).
    stop.store(true, Ordering::SeqCst);
    let _ = watcher.join();
    Ok(())
}

/// The per-boot secret for the control listener, also written (atomically, `0600`) to `path` so
/// the on-device UI can read it. If it cannot be written the listener still starts with an
/// in-memory token nobody can read: the portal and everything else keep working.
fn control_token(path: Option<&std::path::Path>) -> String {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let token = format!("{:032x}{:032x}", rand::random::<u128>(), rand::random::<u128>());
    let Some(path) = path else { return token };
    // A fresh, unpredictable temp name (create_new): a leftover from a crash can never block it.
    let tmp = path.with_extension(format!("tmp{:016x}", rand::random::<u64>()));
    let written = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        if let Some(dir) = path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        eprintln!(
            "plippa-server: cannot write the control token {} ({e}); the device UI cannot use the portal API",
            path.display()
        );
    }
    token
}

/// Which listener a request arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    /// The LAN portal. `secure` is true when the connection is TLS (SECFIX_03).
    Portal { secure: bool },
    /// The loopback control listener for the on-device UI.
    Control,
}

/// How much of the API a request may reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// Everything.
    Full,
    /// Static assets, minimal status and sign-in only (portal, PIN required, not signed in).
    Anonymous,
    /// Nothing (control listener without the token).
    Denied,
}

/// Counting semaphore for handler threads.
struct HandlerPermits {
    free: Mutex<usize>,
    cond: std::sync::Condvar,
}

struct HandlerPermit(Arc<HandlerPermits>);

impl HandlerPermits {
    fn new(n: usize) -> Self {
        Self {
            free: Mutex::new(n),
            cond: std::sync::Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> HandlerPermit {
        let mut free = self.free.lock().unwrap_or_else(|p| p.into_inner());
        while *free == 0 {
            free = self.cond.wait(free).unwrap_or_else(|p| p.into_inner());
        }
        *free -= 1;
        HandlerPermit(Arc::clone(self))
    }
}

impl Drop for HandlerPermit {
    fn drop(&mut self) {
        let mut free = self.0.free.lock().unwrap_or_else(|p| p.into_inner());
        *free += 1;
        self.0.cond.notify_one();
    }
}

/// Print concrete URLs. When bound to `0.0.0.0` / `::`, list every local interface address.
fn print_listen_urls(bind: SocketAddr, secure: bool) {
    let scheme = if secure { "https" } else { "http" };
    let port = bind.port();
    let ips: Vec<IpAddr> = if bind.ip().is_unspecified() {
        let mut ips = local_interface_ips();
        if ips.is_empty() {
            ips.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
        ips
    } else {
        vec![bind.ip()]
    };

    eprintln!("plippa-server listening on:");
    for ip in ips {
        match ip {
            IpAddr::V4(v4) => eprintln!("  {scheme}://{v4}:{port}/"),
            IpAddr::V6(v6) => eprintln!("  {scheme}://[{v6}]:{port}/"),
        }
    }
}

pub(crate) fn local_interface_ips() -> Vec<IpAddr> {
    let mut out = Vec::new();
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 || ifap.is_null() {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let iface = &*cur;
            if !iface.ifa_addr.is_null() {
                let family = i32::from((*iface.ifa_addr).sa_family);
                if family == libc::AF_INET {
                    let sin = &*(iface.ifa_addr as *const libc::sockaddr_in);
                    let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    if !ip.is_unspecified() {
                        out.push(IpAddr::V4(ip));
                    }
                } else if family == libc::AF_INET6 {
                    let sin6 = &*(iface.ifa_addr as *const libc::sockaddr_in6);
                    let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
                    // Skip link-local clutter unless you want every fe80::… entry.
                    if !ip.is_unspecified() && !ip.is_unicast_link_local() {
                        out.push(IpAddr::V6(ip));
                    }
                }
            }
            cur = iface.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn handle_one(
    mut request: Request,
    shared: &Arc<Shared>,
    surface: Surface,
) -> Result<(), String> {
    let service = &shared.service;
    let portal = &shared.portal;
    let method = request.method().clone();
    let url = request.url().to_string();
    // SECFIX_03: the portal listener speaks TLS and plain HTTP on the same port, so the scheme
    // is a property of the request, not of the listener.
    let surface = match surface {
        Surface::Portal { .. } => Surface::Portal {
            secure: request.secure(),
        },
        Surface::Control => Surface::Control,
    };
    // SECFIX_02: one canonical path, decided once, before anything looks at it. The gate, the
    // guards and every router see this string and nothing else; a target that is not already
    // canonical is refused, so no router can disagree with the gate about what was asked.
    let (path, query, bad_target) = match canonical_target(&url) {
        Ok((p, q)) => (p, q, false),
        Err(_) => ("/".to_string(), String::new(), true),
    };

    // NBSecurityRequirements §5.3: the Host header is checked on every request (DNS rebinding),
    // and every state-changing request is checked before routing.
    let mut refused = if bad_target {
        Some(error_json(400, "bad request target"))
    } else {
        host_guard(&request).err()
    };
    if refused.is_none() && !matches!(method, Method::Get | Method::Head) {
        refused = state_change_guard(&request).err();
    }
    // PWRSW_01: after quiesce new work is refused; held only long enough to read the flag.
    if refused.is_none() && service.lock().map(|s| s.quiescing()).unwrap_or(true) {
        refused = Some(error_json(503, "server is shutting down"));
    }

    // SECFIX_03: while HTTPS is on, a request that arrived in the clear is answered with a
    // `307` to the same address over TLS and nothing else — no login form, no API, no cookie.
    // `307` and never `308`: a permanent redirect is cached by the browser, and the owner must
    // be able to switch HTTPS off and have plain HTTP work again without fighting the cache.
    // The `Host` header has already been checked against this portal's own names, so the
    // redirect cannot be pointed at another site.
    // A request the guards already refused is answered by the refusal, never by a redirect that
    // would echo the `Host` the guard just rejected.
    if refused.is_none()
        && matches!(surface, Surface::Portal { secure: false })
        && shared.access.https_enabled()
    {
        return match header_value(&request, "Host") {
            Some(host) if !host.trim().is_empty() => {
                // The target is a header value and a real address: printable ASCII only (a raw
                // UTF-8 byte in the query would turn the `307` into a bare `500`), and the port
                // is the one this listener is actually on, not whatever `Host` claimed (a client
                // that dialled 8787 but sent `Host: …:80` would otherwise be sent to a port that
                // does not speak TLS).
                let target = redirect_target(&url);
                let authority = match shared.portal_port() {
                    Some(port) => with_port(host.trim(), port),
                    None => host.trim().to_string(),
                };
                let location = format!("https://{authority}{target}");
                let body = format!("This portal is served over HTTPS: {location}\n");
                let mut response = Response::from_string(body)
                    .with_status_code(StatusCode(307))
                    .with_header(
                        Header::from_bytes(&b"Location"[..], location.as_bytes())
                            .map_err(|_| "the redirect target is not a header value".to_string())?,
                    )
                    .with_header(
                        Header::from_bytes(&b"Connection"[..], &b"close"[..])
                            .expect("static header"),
                    );
                response.add_header(
                    Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                        .expect("static header"),
                );
                // A `307` keeps the method and the body for the retry; the body is not read
                // here, so the connection closes rather than trying to drain an upload.
                request.respond(response).map_err(|e| e.to_string())
            }
            // No `Host`: there is no address to send the browser to, and a plaintext client
            // on the TLS port is never shown a JSON answer.
            _ => {
                let mut r = Response::from_string("Bad request: no Host header\n")
                    .with_status_code(StatusCode(400))
                    .with_header(
                        Header::from_bytes(&b"Connection"[..], &b"close"[..])
                            .expect("static header"),
                    );
                r.add_header(
                    Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
                        .expect("static header"),
                );
                request.respond(r).map_err(|e| e.to_string())
            }
        };
    }

    // SECFIX_02 (SRV-01): who is asking? Decided once, before any routing.
    let (level, slid) = if bad_target {
        (Level::Denied, None)
    } else {
        authorise(shared, surface, &request, &path)
    };
    let mut set_cookie: Option<String> =
        slid.map(|token| session_cookie(&token, surface_is_secure(surface)));
    if refused.is_none() {
        refused = access_gate(surface, level, &method, &path);
    }
    let full = level == Level::Full;

    let response = match (method.clone(), path.as_str()) {
        _ if refused.is_some() => refused.take().expect("checked"),
        (Method::Post, "/api/portal/login") => {
            let (response, cookie) = portal_login(&mut request, shared, surface);
            set_cookie = cookie;
            response
        }
        (Method::Post, "/api/portal/logout") => {
            set_cookie = Some(expired_session_cookie(surface_is_secure(surface)));
            match request_session_token(&request).map(|t| shared.access.sign_out(&t)) {
                Some(Err(e)) => coded_error(500, e.code(), &e.message()),
                _ => json_response(&serde_json::json!({ "ok": true })),
            }
        }
        (Method::Get, "/api/portal/settings") => {
            json_response(&shared.settings_now())
        }
        (Method::Post, "/api/portal/settings") => portal_settings_update(&mut request, shared),
        (Method::Get, "/api/status") => {
            let status = shared.status_cache.get_or_compute(shared.now_ms(), || {
                service.lock().unwrap_or_else(|p| p.into_inner()).status()
            });
            status_response(shared, surface, full, status)
        }
        // BCS_08: the address a phone can open, for the device's connect QR code.
        (Method::Get, "/api/portal/url") => {
            let mut value =
                serde_json::to_value(portal.resolve_with(shared.access.https_enabled()))
                    .unwrap_or_default();
            if let Some(map) = value.as_object_mut() {
                map.insert("portalEnabled".into(), shared.access.enabled().into());
            }
            json_response(&value)
        }
        (Method::Get, p) if static_asset(p).is_some() => {
            let (body, content_type) = static_asset(p).expect("checked");
            text_response(body, content_type)
        }
        (Method::Get, "/api/now-playing") => {
            let np = service.lock().map_err(|e| e.to_string())?.now_playing();
            json_response(&np)
        }
        (Method::Get, "/api/now-playing/list") => {
            let l = service.lock().map_err(|e| e.to_string())?.now_playing_list();
            json_response(&l)
        }
        (Method::Post, "/api/transport/next") => respond(
            service
                .lock()
                .map_err(|e| e.to_string())?
                .next_entry()
                .map(|()| serde_json::json!({"ok": true})),
        ),
        (Method::Post, "/api/transport/previous") => respond(
            service
                .lock()
                .map_err(|e| e.to_string())?
                .previous_entry()
                .map(|()| serde_json::json!({"ok": true})),
        ),
        (Method::Post, "/api/transport/jump") => {
            #[derive(serde::Deserialize)]
            struct Jump {
                index: u32,
            }
            match json_body::<Jump>(&mut request) {
                Ok(j) => respond(
                    service
                        .lock()
                        .map_err(|e| e.to_string())?
                        .jump_to_entry(j.index)
                        .map(|()| serde_json::json!({"ok": true})),
                ),
                Err(resp) => resp,
            }
        }
        (Method::Post, "/api/transport/stop") => {
            let r = service
                .lock()
                .map_err(|e| e.to_string())?
                .stop_playback()
                .map_err(|e| e.to_string());
            match r {
                Ok(()) => json_response(&serde_json::json!({"ok": true})),
                Err(msg) => error_json(400, &msg),
            }
        }
        (Method::Post, "/api/transport/unload") => {
            let r = service
                .lock()
                .map_err(|e| e.to_string())?
                .unload_playback()
                .map_err(|e| e.to_string());
            match r {
                Ok(()) => json_response(&serde_json::json!({"ok": true})),
                Err(msg) => error_json(400, &msg),
            }
        }
        (Method::Post, "/api/transport/play-pause") => {
            let r = service
                .lock()
                .map_err(|e| e.to_string())?
                .toggle_play_pause()
                .map_err(|e| e.to_string());
            match r {
                Ok(()) => json_response(&serde_json::json!({"ok": true})),
                Err(msg) => error_json(400, &msg),
            }
        }
        (Method::Get, "/api/playables") => {
            let list = service
                .lock()
                .map_err(|e| e.to_string())?
                .list_playables()
                .map_err(|e| e.to_string());
            match list {
                Ok(v) => json_response(&v),
                Err(msg) => error_json(503, &msg),
            }
        }
        (_, p) if is_library_delete_path(&method, p) => {
            delete_route(&mut request, p, service)?
        }
        (Method::Get, p) if p.starts_with("/api/playables/") => {
            let id = parse_id_suffix(p, "/api/playables/");
            match id {
                Some(id) => {
                    let detail = service
                        .lock()
                        .map_err(|e| e.to_string())?
                        .get_playable(id)
                        .map_err(|e| e.to_string());
                    match detail {
                        Ok(v) => json_response(&v),
                        Err(msg) => error_json(404, &msg),
                    }
                }
                None => error_json(400, "bad playable id"),
            }
        }
        (Method::Patch, p)
            if p.starts_with("/api/playables/")
                && !p.ends_with("/play")
                && !p.ends_with("/associate") =>
        {
            let id = parse_id_suffix(p, "/api/playables/");
            match id {
                Some(id) => match limited_json_body::<MetadataPatch>(&mut request, 64 * 1024) {
                    Err(resp) => resp,
                    Ok(patch) => {
                        let detail = service
                            .lock()
                            .map_err(|e| e.to_string())?
                            .patch_metadata(id, patch)
                            .map_err(|e| e.to_string());
                        match detail {
                            Ok(v) => json_response(&v),
                            Err(msg) => error_json(400, &msg),
                        }
                    }
                },
                None => error_json(400, "bad playable id"),
            }
        }
        (Method::Post, p) if p.ends_with("/play") && p.starts_with("/api/playables/") => {
            let id = parse_id_before_suffix(p, "/api/playables/", "/play");
            match id {
                Some(id) => {
                    let r = service
                        .lock()
                        .map_err(|e| e.to_string())?
                        .play_playable(id)
                        .map_err(|e| e.to_string());
                    match r {
                        Ok(()) => json_response(&serde_json::json!({"ok": true})),
                        Err(msg) => error_json(400, &msg),
                    }
                }
                None => error_json(400, "bad playable id"),
            }
        }
        (Method::Post, p) if p.ends_with("/associate") && p.starts_with("/api/playables/") => {
            let id = parse_id_before_suffix(p, "/api/playables/", "/associate");
            match id {
                Some(id) => {
                    let keep_existing = query_param(&query, "keep_existing_key")
                        .map(|v| {
                            v == "1"
                                || v.eq_ignore_ascii_case("true")
                                || v.eq_ignore_ascii_case("yes")
                        })
                        .unwrap_or(false);
                    // NB_06: `?item=N` = one track (mediaList position) of a release.
                    let item = query_param(&query, "item").map(|v| v.trim().parse::<u32>());
                    if matches!(item, Some(Err(_))) {
                        error_json(400, "bad item (expected a mediaList position)")
                    } else {
                        let item = item.and_then(Result::ok);
                        // Do not hold ServerService across the NFC wait.
                        let r = match item {
                            Some(n) => associate_track_unlocked(service, id, n, keep_existing),
                            None if keep_existing => {
                                associate_playable_keep_existing_unlocked(service, id)
                            }
                            None => associate_playable_unlocked(service, id),
                        };
                        match r {
                            Ok(v) => json_response(&v),
                            Err(e) => service_error(&e),
                        }
                    }
                }
                None => error_json(400, "bad playable id"),
            }
        }
        (Method::Post, "/api/media") => {
            let filename =
                query_param(query.clone(), "filename").unwrap_or_else(|| "upload.bin".into());
            let cue_text = query_param(query.clone(), "cue_text");
            let kind_raw = query_param(query, "kind");
            match kind_raw.as_deref().and_then(ImportKind::parse) {
                None => error_json(
                    400,
                    "missing or invalid kind (required: music|mix|audiobook|podcast|other)",
                ),
                Some(kind) => media_upload(&mut request, service, kind, &filename, cue_text.as_deref())?,
            }
        }
        (Method::Post, "/api/rescan") => {
            let r = service
                .lock()
                .map_err(|e| e.to_string())?
                .lazy_rescan()
                .map_err(|e| e.to_string());
            match r {
                Ok(report) => json_response(&serde_json::json!({
                    "scanned_files": report.scanned_files,
                    "ingested": report.ingested,
                    "updated": report.updated,
                    "removed": report.removed,
                    "skipped": report.skipped,
                    "warnings": report.warnings,
                })),
                Err(msg) => error_json(503, &msg),
            }
        }
        (_, p) if is_imports_path(p) => imports_route(&mut request, p, &query, service)?,
        (_, p) if is_unite_path(p) => unite_route(&mut request, p, &query, service)?,
        (_, p) if is_refresh_path(p) => refresh_route(&mut request, p, service)?,
        (_, p) if is_stores_path(p) => stores_route(&mut request, p, &query, service)?,
        (_, p) if is_web_path(p) => web_route(&mut request, p, service)?,
        (_, p) if is_covers_path(p) => covers_route(&mut request, p, &query, service)?,
        (_, p) if is_cues_path(p) => cues_route(&mut request, p, &query, service)?,
        (_, p) if is_collections_path(p) => {
            collections_route(&mut request, p, &query, service)?
        }
        (_, p) if is_waveforms_path(p) => waveforms_route(&request, p, service)?,
        (Method::Post, p) if is_reanalyse_path(p) => reanalyse_route(&mut request, p, service),
        _ => error_json(404, "not found"),
    };

    // NBSecurityRequirements §5.2, on every response (pages, scripts, JSON, errors).
    let hardening: [(&[u8], &[u8]); 4] = [
        (b"X-Content-Type-Options", b"nosniff"),
        (b"Referrer-Policy", b"no-referrer"),
        (b"Content-Security-Policy", CSP.as_bytes()),
        (b"Cache-Control", b"no-store"),
    ];
    let mut response = response;
    for (name, value) in hardening {
        // Cover images set their own (private, cacheable) Cache-Control.
        let present = response
            .headers()
            .iter()
            .any(|h| h.field.equiv(std::str::from_utf8(name).unwrap_or_default()));
        if !present {
            response.add_header(Header::from_bytes(name, value).expect("header"));
        }
    }
    if let Some(cookie) = set_cookie {
        response.add_header(Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes()).expect("header"));
    }
    // Whatever a state-changing request did, the next status read is a fresh one.
    if !matches!(method, Method::Get | Method::Head) && full {
        shared.status_cache.invalidate();
    }
    request.respond(response).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// SECFIX_02 portal access: who is asking, sign-in, settings, minimal status
// ---------------------------------------------------------------------------

/// The header the on-device UI sends on the control listener.
const LOCAL_TOKEN_HEADER: &str = "X-Plippa-Local";

fn surface_is_secure(surface: Surface) -> bool {
    matches!(surface, Surface::Portal { secure: true })
}

/// The session token in the request's `Cookie` header, if any.
fn request_session_token(request: &Request) -> Option<String> {
    let header = header_value(request, "Cookie")?;
    session_token_from_cookie(&header).map(str::to_string)
}

/// How much of the API this request may reach.
///
/// * Control listener: everything with the right token, nothing without it.
/// * Portal: static assets are public; under `/api/` it is everything when the PIN is not
///   required or the request carries a live session, and only the sign-in surface otherwise.
///   The control token means nothing here, and the settings routes do not exist on this listener.
fn authorise(
    shared: &Shared,
    surface: Surface,
    request: &Request,
    _path: &str,
) -> (Level, Option<String>) {
    match surface {
        Surface::Control => {
            let presented = header_value(request, LOCAL_TOKEN_HEADER).unwrap_or_default();
            if constant_time_eq(presented.trim().as_bytes(), shared.control_token.as_bytes()) {
                (Level::Full, None)
            } else {
                (Level::Denied, None)
            }
        }
        Surface::Portal { .. } => {
            // Deny by default: with a PIN required, only a live session reaches anything.
            // What an anonymous caller may touch is a short fixed list (`access_gate`).
            if !shared.access.pin_required() {
                return (Level::Full, None);
            }
            match request_session_token(request) {
                Some(token) => match shared.access.session_check(&token) {
                    // Slid: the server extended it, so the browser's cookie is renewed too.
                    Some(true) => (Level::Full, Some(token)),
                    Some(false) => (Level::Full, None),
                    None => (Level::Anonymous, None),
                },
                None => (Level::Anonymous, None),
            }
        }
    }
}

/// The early refusal for what `level` may not reach, or `None` to route normally.
fn access_gate(
    surface: Surface,
    level: Level,
    method: &Method,
    path: &str,
) -> Option<Response<Cursor<Vec<u8>>>> {
    let settings_route = path == "/api/portal/settings";
    let sign_in_route = matches!(
        (method, path),
        (Method::Post, "/api/portal/login") | (Method::Post, "/api/portal/logout")
    );
    // Who may ask comes first; whether the route exists on this listener only matters to someone
    // who is allowed to ask, so a refused caller learns nothing from a 404.
    match level {
        Level::Denied => Some(coded_error(
            401,
            "local_token",
            "the control listener needs the device token",
        )),
        Level::Anonymous => {
            // The whole public surface: the embedded page and scripts (exact names only),
            // sign-in and sign-out, and the minimal status.
            let public = sign_in_route
                || matches!((method, path), (Method::Get, "/api/status"))
                || (matches!(method, Method::Get) && static_asset(path).is_some());
            if public {
                None
            } else {
                Some(coded_error(401, "pin_required", "sign-in required"))
            }
        }
        Level::Full => match surface {
            Surface::Portal { .. } if settings_route => Some(error_json(404, "not found")),
            // Sign-in lives on the LAN portal only; the device never signs in.
            Surface::Control if sign_in_route => Some(error_json(404, "not found")),
            _ => None,
        },
    }
}

/// `GET /api/status`: the minimum for a caller who has not signed in, the whole answer for
/// everyone else. A portal caller never sees the absolute library path.
fn status_response(
    shared: &Shared,
    surface: Surface,
    full: bool,
    status: crate::service::LibraryStatus,
) -> Response<Cursor<Vec<u8>>> {
    let pin_required = shared.access.pin_required();
    if !full {
        return json_response(&serde_json::json!({
            "available": status.available,
            "ipc_live": status.ipc_live,
            "pin_required": pin_required,
            "signed_in": false,
            // SECFIX_03: the page warns while the portal is served in the clear.
            "https": shared.access.https_enabled(),
        }));
    }
    let status = match surface {
        Surface::Control => status,
        Surface::Portal { .. } => portal_view_of(status),
    };
    let mut value = serde_json::to_value(&status).unwrap_or_default();
    if let Some(map) = value.as_object_mut() {
        map.insert("pin_required".into(), pin_required.into());
        map.insert("signed_in".into(), true.into());
        map.insert("https".into(), shared.access.https_enabled().into());
    }
    json_response(&value)
}

/// What a browser may learn about where the library lives: the volume label, never the path.
fn portal_view_of(mut status: crate::service::LibraryStatus) -> crate::service::LibraryStatus {
    status.library_root = None;
    for (n, root) in status.library_roots.iter_mut().enumerate() {
        root.path = if root.label.trim().is_empty() {
            format!("Library {}", n + 1)
        } else {
            root.label.clone()
        };
    }
    status
}

#[derive(serde::Deserialize)]
struct LoginBody {
    pin: String,
}

/// `POST /api/portal/login`: `{ "pin": "123456" }` -> the session cookie.
fn portal_login(
    request: &mut Request,
    shared: &Shared,
    surface: Surface,
) -> (Response<Cursor<Vec<u8>>>, Option<String>) {
    if let Err(resp) = json_write_guard(request) {
        return (resp, None);
    }
    let body = match limited_json_body::<LoginBody>(request, 1024) {
        Ok(b) => b,
        Err(resp) => return (resp, None),
    };
    if !shared.access.pin_required() {
        return (
            json_response(&serde_json::json!({ "ok": true, "pin_required": false })),
            None,
        );
    }
    let client = client_of(request);
    match shared.access.login(client, &body.pin) {
        LoginOutcome::Signed(token) => {
            shared.log_sign_in(client, "ok");
            (
                json_response(&serde_json::json!({ "ok": true })),
                Some(session_cookie(&token, surface_is_secure(surface))),
            )
        }
        LoginOutcome::BadPin => {
            shared.log_sign_in(client, "refused (wrong PIN)");
            (coded_error(401, "bad_pin", "wrong PIN"), None)
        }
        LoginOutcome::Locked { retry_after_secs } => {
            let minutes = retry_after_secs.div_ceil(60);
            let mut response = json_with_status(
                429,
                &serde_json::json!({
                    "ok": false,
                    "code": "locked",
                    "error": format!(
                        "Too many wrong PINs. Try again in {minutes} minute{}.",
                        if minutes == 1 { "" } else { "s" }
                    ),
                    "retry_after": retry_after_secs,
                }),
            );
            response.add_header(
                Header::from_bytes(&b"Retry-After"[..], retry_after_secs.to_string().as_bytes())
                    .expect("header"),
            );
            (response, None)
        }
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SettingsUpdate {
    enabled: Option<bool>,
    /// SECFIX_03: serve the portal over TLS (default on).
    https_enabled: Option<bool>,
    /// SECFIX_03: issue a new certificate (the fingerprint changes, so every browser has to
    /// be told again).
    #[serde(default)]
    regenerate_certificate: bool,
    pin_required: Option<bool>,
    pin: Option<String>,
    #[serde(default)]
    clear_lockout: bool,
}

/// `POST /api/portal/settings` (control listener only): change what the device menu changes.
/// Every field is optional; the answer is the settings as they are afterwards.
fn portal_settings_update(
    request: &mut Request,
    shared: &Arc<Shared>,
) -> Response<Cursor<Vec<u8>>> {
    let update = match limited_json_body::<SettingsUpdate>(request, 4096) {
        Ok(u) => u,
        Err(resp) => return resp,
    };
    let access = &shared.access;
    let refuse = |e: AccessError| {
        let status = if matches!(e, AccessError::Unavailable(_)) { 409 } else { 400 };
        coded_error(status, e.code(), &e.message())
    };
    // A change that took effect but could not be written yet is not a failure of the request:
    // the answer carries `unsaved` and the write is retried.
    let soft = |r: Result<(), AccessError>| match r {
        Err(AccessError::NotSaved(_)) => Ok(()),
        other => other,
    };
    if let Some(pin) = update.pin.as_deref() {
        if let Err(e) = soft(access.set_pin(pin)) {
            return refuse(e);
        }
    }
    if let Some(required) = update.pin_required {
        if let Err(e) = soft(access.set_pin_required(required)) {
            return refuse(e);
        }
    }
    if update.clear_lockout {
        access.clear_lockout();
    }
    let mut rebind = false;
    if let Some(on) = update.https_enabled {
        if let Err(e) = soft(access.set_https_enabled(on)) {
            return refuse(e);
        }
        rebind = true;
    }
    if update.regenerate_certificate {
        if let Err(reason) = shared.regenerate_portal_certificate() {
            return coded_error(500, "certificate_failed", &reason);
        }
        rebind = true;
    }
    if let Some(on) = update.enabled {
        if let Err(e) = soft(access.set_enabled(on)) {
            return refuse(e);
        }
        rebind = true;
    }
    if rebind {
        if let Err(reason) = shared.reconcile() {
            // The socket could not be opened (the port is taken): do not leave the switch on
            // with nothing listening. A certificate that cannot be written leaves the switch
            // where the owner put it and says why (the retry thread keeps trying).
            if update.enabled == Some(true) {
                let _ = access.set_enabled(false);
            }
            return coded_error(500, "bind_failed", &format!("the web portal could not start: {reason}"));
        }
    }
    json_response(&shared.settings_now())
}

// ---------------------------------------------------------------------------
// NB_11 streamed uploads and multi-file imports
// ---------------------------------------------------------------------------

/// After refusing an upload early, swallow (a bounded amount of) the rest of the body so the
/// browser can read our answer instead of seeing a reset connection.
fn drain_body(request: &mut Request) {
    const MAX_DRAIN: u64 = 16 * 1024 * 1024;
    let _ = std::io::copy(
        &mut std::io::Read::take(request.as_reader(), MAX_DRAIN),
        &mut std::io::sink(),
    );
}

fn upload_busy_response() -> Response<Cursor<Vec<u8>>> {
    let mut r = error_json(
        429,
        "another upload is in progress — wait for it to finish and retry",
    );
    r.add_header(Header::from_bytes(&b"Retry-After"[..], &b"5"[..]).expect("header"));
    r
}

fn import_error_response(e: &crate::service::ImportError) -> Response<Cursor<Vec<u8>>> {
    let bytes = serde_json::to_vec(&e.to_json()).unwrap_or_default();
    Response::from_data(bytes)
        .with_status_code(StatusCode(e.status))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
                .expect("header"),
        )
}

/// The address a request is attributed to.
fn client_of(request: &Request) -> IpAddr {
    client_id(request.remote_addr().copied())
}

/// `413` naming the limit, in units a person reads.
fn file_too_large(limit: u64) -> Response<Cursor<Vec<u8>>> {
    error_json(
        413,
        &format!("file too large (limit {})", describe_bytes(limit)),
    )
}

/// `POST /api/media`: the body is streamed to `<library>/.plippa/tmp` (never held in RAM),
/// sniffed, and only then moved into the library with its `.noudata`. A connection that dies
/// half way leaves nothing behind.
///
/// Superseded by a one-file import session (`POST /api/imports {single: true}`, then `files`
/// and `finish`), which also stages and previews cover art before anything is added; this route
/// is kept for scripts and tests.
fn media_upload(
    request: &mut Request,
    service: &Arc<Mutex<ServerService>>,
    kind: ImportKind,
    filename: &str,
    cue_text: Option<&str>,
) -> Result<Response<Cursor<Vec<u8>>>, String> {
    let lock = || service.lock().map_err(|e| e.to_string());
    let Some(len) = request.body_length() else {
        return Ok(error_json(411, "Content-Length is required for uploads"));
    };
    let hub = lock()?.imports();
    if len as u64 > hub.limits().max_file_bytes {
        return Ok(file_too_large(hub.limits().max_file_bytes));
    }
    // SEC-07: the slot is held until this request's response is built.
    let Some(slot) = hub.try_upload_slot(client_of(request)) else {
        return Ok(upload_busy_response());
    };
    // Refuse what can be refused before the body is received.
    let staged = {
        let svc = lock()?;
        svc.precheck_add_media(kind, cue_text)
            .and_then(|_| svc.staging_dir())
    };
    let tmp = match staged {
        Ok(t) => t,
        Err(e) => {
            drain_body(request);
            return Ok(error_json(400, &e.to_string()));
        }
    };
    let budget = hub.new_budget();
    let cancel = crate::web::download::Cancel::new();
    let streamed = crate::service::stream_audio_upload(
        &mut slot.reader(request.as_reader()),
        Some(len as u64),
        &tmp,
        hub.limits(),
        &budget,
        &cancel,
    );
    let streamed = match streamed {
        Ok(s) => s,
        Err(e) => {
            let err = crate::service::ImportError::from(e);
            if err.status == 415 {
                drain_body(request);
            }
            return Ok(import_error_response(&err));
        }
    };
    // The content decides the file type, never the client's name or extension (§5.5).
    let MediaSniff::Audio(k) = sniff_audio(&streamed.head) else {
        return Ok(error_json(415, "not an audio file Plippa can play"));
    };
    let filename = crate::unite::hub::staged_name(filename, k.ext());
    let r = lock()?
        .add_media_from_file(kind, &filename, streamed.temp.path(), cue_text)
        .map_err(|e| e.to_string());
    // (`streamed.temp` deletes the temp file if it is still there)
    Ok(match r {
        Ok(v) => json_response(&v),
        Err(msg) => error_json(400, &msg),
    })
}

fn is_imports_path(p: &str) -> bool {
    p == "/api/imports" || p.starts_with("/api/imports/")
}

fn import_respond(r: Result<serde_json::Value, crate::service::ImportError>) -> Response<Cursor<Vec<u8>>> {
    match r {
        Ok(v) => json_response(&v),
        Err(e) => import_error_response(&e),
    }
}

/// Body of `POST /api/imports`.
#[derive(serde::Deserialize, Default)]
struct ImportBegin {
    #[serde(default)]
    single: bool,
}

/// Body of `PUT /api/imports/:sid/art/order`.
#[derive(serde::Deserialize)]
struct ImportArtOrder {
    order: Vec<String>,
}

fn imports_route(
    request: &mut Request,
    path: &str,
    query: &str,
    service: &Arc<Mutex<ServerService>>,
) -> Result<Response<Cursor<Vec<u8>>>, String> {
    let method = request.method().clone();
    let segs: Vec<&str> = path.strip_prefix('/').unwrap_or_default().split('/').collect();
    let hub: Arc<ImportHub> = service.lock().map_err(|e| e.to_string())?.imports();
    let client = client_of(request);
    Ok(match (method, segs.as_slice()) {
        (Method::Post, ["api", "imports"]) => {
            // The body is optional: `{ "single": true }` opens a one-file session.
            let single = if request.body_length().unwrap_or(0) > 0 {
                if let Err(r) = json_write_guard(request) {
                    return Ok(r);
                }
                match limited_json_body::<ImportBegin>(request, 4096) {
                    Ok(b) => b.single,
                    Err(r) => return Ok(r),
                }
            } else {
                false
            };
            import_respond(hub.begin(client, service, single))
        }
        (m, ["api", "imports", sid, rest @ ..]) => {
            if !is_web_token(sid) {
                return Ok(import_error_response(&crate::service::ImportError {
                    status: 404,
                    code: "unknown_import",
                    message: "unknown or expired import".into(),
                    file: None,
                }));
            }
            match (m, rest) {
                (Method::Delete, []) => {
                    if hub.discard(sid) {
                        json_response(&serde_json::json!({"ok": true}))
                    } else {
                        error_json(404, "unknown or expired import")
                    }
                }
                (Method::Post, ["files"]) => {
                    let name = query_param(query, "name")
                        .unwrap_or_else(|| "upload.bin".into());
                    let Some(len) = request.body_length() else {
                        return Ok(error_json(411, "Content-Length is required for uploads"));
                    };
                    if len as u64 > hub.limits().max_file_bytes {
                        return Ok(file_too_large(hub.limits().max_file_bytes));
                    }
                    let Some(slot) = hub.try_upload_slot(client) else {
                        return Ok(upload_busy_response());
                    };
                    let r = hub.add_file(
                        sid,
                        &name,
                        Some(len as u64),
                        &mut slot.reader(request.as_reader()),
                    );
                    if matches!(&r, Err(e) if e.status == 415) {
                        drain_body(request);
                    }
                    import_respond(r)
                }
                (Method::Delete, ["files", fid]) => import_respond(hub.remove_file(sid, fid)),
                (Method::Get, ["art", aid]) => match hub.art_bytes(sid, aid) {
                    Ok((bytes, ctype)) => Response::from_data(bytes)
                        .with_status_code(StatusCode(200))
                        .with_header(
                            Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes())
                                .expect("header"),
                        ),
                    Err(e) => import_error_response(&e),
                },
                (Method::Delete, ["art", aid]) => import_respond(hub.remove_art(sid, aid)),
                (Method::Put, ["art", "order"]) => {
                    if let Err(r) = json_write_guard(request) {
                        return Ok(r);
                    }
                    match limited_json_body::<ImportArtOrder>(request, 64 * 1024) {
                        Ok(b) => import_respond(hub.order_art(sid, b.order)),
                        Err(r) => r,
                    }
                }
                (Method::Post, ["finish"]) => {
                    if let Err(r) = json_write_guard(request) {
                        return Ok(r);
                    }
                    match limited_json_body::<ImportFinish>(request, 256 * 1024) {
                        Ok(req) => import_respond(hub.finish(service, sid, req)),
                        Err(r) => r,
                    }
                }
                _ => error_json(404, "not found"),
            }
        }
        _ => error_json(404, "not found"),
    })
}

// ---------------------------------------------------------------------------
// NB_05 cue import
// ---------------------------------------------------------------------------

fn is_cues_path(p: &str) -> bool {
    p == "/api/cues/preview"
        || (p.starts_with("/api/items/") && (p.ends_with("/cues") || p.ends_with("/noudata")))
}

/// Header value (case-insensitive name).
fn header_value(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

/// Host part of an `Origin` (`http://host:port`) for comparison with the `Host` header.
fn origin_authority(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://")?.1;
    Some(rest.split('/').next().unwrap_or(rest))
}

/// CSRF / DNS-rebinding guard for state-changing JSON endpoints (NBSecurityRequirements
/// §5.3): the body must be declared `application/json` (a cross-site HTML form cannot send
/// that), and an `Origin` header, when present, must name this portal's own `Host`.
fn json_write_guard(request: &Request) -> Result<(), Response<Cursor<Vec<u8>>>> {
    let ctype = header_value(request, "Content-Type").unwrap_or_default();
    if !ctype
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return Err(error_json(415, "Content-Type must be application/json"));
    }
    if let Some(origin) = header_value(request, "Origin") {
        let host = header_value(request, "Host").unwrap_or_default();
        let same = origin_authority(&origin)
            .is_some_and(|a| !host.is_empty() && a.eq_ignore_ascii_case(host.trim()));
        if !same {
            return Err(error_json(403, "cross-origin request refused"));
        }
    }
    Ok(())
}

/// Extra portal host names (comma separated `PLIPPA_PORTAL_HOSTS`), for people who reach the
/// portal through their own DNS name. Read once.
fn extra_portal_hosts() -> &'static [String] {
    static HOSTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    HOSTS.get_or_init(|| {
        std::env::var("PLIPPA_PORTAL_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect()
    })
}

/// Is `host_header` (`name[:port]`, `[v6][:port]`) a name this LAN portal answers to? IP
/// literals, `localhost`, single-label names (`plippa`) and local suffixes (`*.local`, `*.lan`,
/// `*.home.arpa` ...) are fine; any other DNS name is refused, because that is what a
/// DNS-rebinding page uses (its `Origin` would equal its `Host`, so the Origin check alone
/// cannot stop it).
pub(crate) fn host_allowed(host_header: &str, extra: &[String]) -> bool {
    let h = host_header.trim();
    let name = if let Some(rest) = h.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inner, _)) => return inner.parse::<Ipv6Addr>().is_ok(),
            None => return false,
        }
    } else {
        h.rsplit_once(':')
            .filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit()))
            .map_or(h, |(n, _)| n)
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() {
        return false;
    }
    if name.parse::<Ipv4Addr>().is_ok() || extra.contains(&name) {
        return true;
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.') {
        return false;
    }
    plippa_noubin::is_blocked_hostname(&name)
}

/// `host[:port]` with its port replaced by the listener's own. An IPv6 literal keeps its
/// brackets; a name without a port gains one. Called only after `host_allowed` accepted the
/// header, so the result is still one of this portal's names.
fn with_port(host: &str, port: u16) -> String {
    if let Some(rest) = host.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((inner, _)) => format!("[{inner}]:{port}"),
            None => host.to_string(),
        };
    }
    let name = match host.rsplit_once(':') {
        // An empty port is legal (`port = *DIGIT`) and means "the default": `plippa.local:` is
        // still a bare name and must not become `plippa.local::8787`.
        Some((name, p)) if p.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    format!("{name}:{port}")
}

/// The redirect target as a header value: percent-encode every byte outside the URI character
/// set. `canonical_target` already refused a non-ASCII path, and the head reader refuses a
/// non-ASCII request line outright, so today nothing here can fail `Header::from_bytes` — but the
/// `Location` is built from the raw target, and a future framing change must not be able to turn
/// the `307` into a `500` by putting a raw byte in a header. An existing `%XX` and the usual
/// query punctuation pass through unchanged.
fn redirect_target(url: &str) -> String {
    const PLAIN: &[u8] = b"-._~/:?&=+%!$'()*,;@";
    let mut out = String::with_capacity(url.len() + 16);
    for &b in url.as_bytes() {
        if b.is_ascii_alphanumeric() || PLAIN.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 403 for a `Host` that is not one of the portal's own names. No `Host` header (HTTP/1.0)
/// is let through: browsers always send one.
fn host_guard(request: &Request) -> Result<(), Response<Cursor<Vec<u8>>>> {
    match header_value(request, "Host") {
        Some(h) if !host_allowed(&h, extra_portal_hosts()) => {
            Err(error_json(403, "unexpected Host header (set PLIPPA_PORTAL_HOSTS to allow a DNS name)"))
        }
        _ => Ok(()),
    }
}

/// CSRF / DNS-rebinding guard for **every** POST/PUT/PATCH/DELETE: an `Origin` header, when
/// present, must name this portal's own `Host`, and the request must be one a cross-site HTML
/// form or no-cors `fetch` cannot send: `Content-Type: application/json` or the custom header
/// `X-Plippa: 1` (a custom header forces a CORS preflight, which this server never approves).
fn state_change_guard(request: &Request) -> Result<(), Response<Cursor<Vec<u8>>>> {
    if let Some(origin) = header_value(request, "Origin") {
        let host = header_value(request, "Host").unwrap_or_default();
        let same = origin_authority(&origin)
            .is_some_and(|a| !host.is_empty() && a.eq_ignore_ascii_case(host.trim()));
        if !same {
            return Err(error_json(403, "cross-origin request refused"));
        }
    }
    let json = header_value(request, "Content-Type").is_some_and(|c| {
        c.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
    });
    let custom = header_value(request, "X-Plippa").is_some_and(|v| v.trim() == "1");
    if json || custom {
        Ok(())
    } else {
        Err(error_json(
            415,
            "state-changing requests need Content-Type: application/json or X-Plippa: 1",
        ))
    }
}

// ---------------------------------------------------------------------------
// NB_09 unite flows and refresh
// ---------------------------------------------------------------------------

fn is_unite_path(p: &str) -> bool {
    p == "/api/unite/start" || p.starts_with("/api/unite/")
}

fn is_refresh_path(p: &str) -> bool {
    p.starts_with("/api/playables/")
        && (p.ends_with("/refresh") || p.ends_with("/refresh/prepare") || p.ends_with("/refresh/images"))
}

/// Unite error as JSON `{"error", "code", ...}` with its HTTP status.
fn unite_error(e: &UniteError) -> Response<Cursor<Vec<u8>>> {
    let bytes = serde_json::to_vec(&e.to_json()).unwrap_or_default();
    Response::from_data(bytes)
        .with_status_code(StatusCode(e.http_status()))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
                .expect("header"),
        )
}

fn unite_respond(r: Result<serde_json::Value, UniteError>) -> Response<Cursor<Vec<u8>>> {
    match r {
        Ok(v) => json_response(&v),
        Err(e) => unite_error(&e),
    }
}

/// JSON body of a unite call: at most 64 KiB, or 400/413 (an empty body means `{}`).
fn unite_body<T: serde::de::DeserializeOwned + Default>(
    request: &mut Request,
) -> Result<T, Response<Cursor<Vec<u8>>>> {
    if request.body_length().unwrap_or(0) == 0 {
        return Ok(T::default());
    }
    limited_json_body::<T>(request, 64 * 1024)
}

fn unite_route(
    request: &mut Request,
    path: &str,
    query: &str,
    service: &Arc<Mutex<ServerService>>,
) -> Result<Response<Cursor<Vec<u8>>>, String> {
    let method = request.method().clone();
    let segs: Vec<&str> = path.strip_prefix('/').unwrap_or_default().split('/').collect();
    let hub = service.lock().map_err(|e| e.to_string())?.unite();
    // Every state-changing unite call also needs a JSON content type unless it is the raw
    // upload (which carries `X-Plippa: 1`, enforced by the global guard).
    let json_guard = |request: &Request| json_write_guard(request);
    Ok(match (method, segs.as_slice()) {
        (Method::Post, ["api", "unite", "start"]) => {
            if let Err(r) = json_guard(request) {
                return Ok(r);
            }
            let client = client_of(request);
            match unite_body::<StartRequest>(request) {
                // The metadata fetch (if any) blocks this request's thread only, never the
                // service lock.
                Ok(req) => unite_respond(hub.start_for(client, service, req)),
                Err(r) => r,
            }
        }
        (m, ["api", "unite", sid, rest @ ..]) => {
            if !is_web_token(sid) {
                return Ok(unite_error(&UniteError::UnknownSession));
            }
            match (m, rest) {
                (Method::Get, []) => unite_respond(hub.status(sid)),
                (Method::Delete, []) => {
                    if hub.discard(sid) {
                        json_response(&serde_json::json!({"ok": true}))
                    } else {
                        unite_error(&UniteError::UnknownSession)
                    }
                }
                (Method::Post, ["files"]) => {
                    let name = query_param(query, "name").unwrap_or_else(|| "upload.bin".into());
                    let len = request.body_length().map(|l| l as u64);
                    // The same one slot as every other upload: this body is also written to
                    // the stick, and it used to take no slot at all.
                    let imports = service.lock().map_err(|e| e.to_string())?.imports();
                    let Some(slot) = imports.try_upload_slot(client_of(request)) else {
                        return Ok(upload_busy_response());
                    };
                    // Streamed straight to the staging area: nothing is buffered in memory.
                    unite_respond(hub.add_upload(
                        sid,
                        &name,
                        len,
                        &mut slot.reader(request.as_reader()),
                    ))
                }
                (Method::Post, ["download", "prepare"]) => {
                    if let Err(r) = json_guard(request) {
                        return Ok(r);
                    }
                    match unite_body::<PrepareRequest>(request) {
                        Ok(req) => unite_respond(hub.download_prepare(sid, req)),
                        Err(r) => r,
                    }
                }
                (Method::Post, ["download"]) => {
                    if let Err(r) = json_guard(request) {
                        return Ok(r);
                    }
                    match unite_body::<DownloadRequest>(request) {
                        Ok(req) => unite_respond(hub.download_start(service, sid, req)),
                        Err(r) => r,
                    }
                }
                (Method::Post, ["download", "cancel"]) => {
                    unite_respond(hub.download_cancel(sid))
                }
                (Method::Get, ["proposal"]) => unite_respond(hub.proposal(sid)),
                (Method::Post, ["confirm"]) => {
                    if let Err(r) = json_guard(request) {
                        return Ok(r);
                    }
                    match unite_body::<ConfirmRequest>(request) {
                        Ok(req) => unite_respond(hub.confirm(sid, req)),
                        Err(r) => r,
                    }
                }
                (Method::Post, ["apply"]) => {
                    if let Err(r) = json_guard(request) {
                        return Ok(r);
                    }
                    match unite_body::<ApplyRequest>(request) {
                        Ok(req) => unite_respond(hub.apply(service, sid, req)),
                        Err(r) => r,
                    }
                }
                _ => error_json(404, "not found"),
            }
        }
        _ => error_json(404, "not found"),
    })
}

fn refresh_route(
    request: &mut Request,
    path: &str,
    service: &Arc<Mutex<ServerService>>,
) -> Result<Response<Cursor<Vec<u8>>>, String> {
    if request.method() != &Method::Post {
        return Ok(error_json(405, "method not allowed"));
    }
    if let Err(r) = json_write_guard(request) {
        return Ok(r);
    }
    let rest = &path["/api/playables/".len()..];
    let (id, action) = match rest.split_once('/') {
        Some((id, a)) => (id, a),
        None => return Ok(error_json(404, "not found")),
    };
    let Ok(id) = id.parse::<u64>() else {
        return Ok(error_json(400, "bad playable id"));
    };
    let hub = service.lock().map_err(|e| e.to_string())?.unite();
    Ok(match action {
        "refresh/prepare" => match unite_body::<RefreshRequest>(request) {
            Ok(req) => unite_respond(refresh_prepare(service, id, &req)),
            Err(r) => r,
        },
        "refresh" => match unite_body::<RefreshRequest>(request) {
            Ok(req) => unite_respond(refresh(&hub, service, id, req)),
            Err(r) => r,
        },
        "refresh/images" => {
            #[derive(Default, serde::Deserialize)]
            struct Body {
                session: String,
            }
            match unite_body::<Body>(request) {
                Ok(b) if is_web_token(&b.session) => {
                    unite_respond(refresh_install_images(&hub, service, id, &b.session))
                }
                Ok(_) => unite_error(&UniteError::UnknownSession),
                Err(r) => r,
            }
        }
        _ => error_json(404, "not found"),
    })
}

// ---------------------------------------------------------------------------
// BCS_03 Bandcamp connection (secrets, cache, sync)
// ---------------------------------------------------------------------------

fn is_stores_path(p: &str) -> bool {
    p == "/api/stores" || p.starts_with("/api/stores/")
}

/// Store error as JSON: `{"error": message, "code": stable_code}`.
fn store_error(e: &StoreApiError) -> Response<Cursor<Vec<u8>>> {
    let body = serde_json::json!({ "error": e.message(), "code": e.code() });
    Response::from_data(serde_json::to_vec(&body).unwrap_or_default())
        .with_status_code(StatusCode(e.http_status()))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
                .expect("header"),
        )
}

/// A proxied cover: the type comes from the bytes, and the cache is short and private.
fn cover_response(image: &crate::service::stores::CoverImage) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(image.bytes.as_ref().clone())
        .with_status_code(StatusCode(200))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], image.content_type.as_bytes()).expect("header"),
        )
        .with_header(
            Header::from_bytes(
                &b"Cache-Control"[..],
                crate::service::stores::COVER_CACHE_CONTROL.as_bytes(),
            )
            .expect("header"),
        )
}

/// `202 Accepted` with a JSON body (the sync worker was started).
fn accepted_json<T: Serialize>(value: &T) -> Response<Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(body)
        .with_status_code(StatusCode(202))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
                .expect("header"),
        )
}

fn stores_route(
    request: &mut Request,
    path: &str,
    query: &str,
    service: &Arc<Mutex<ServerService>>,
) -> Result<Response<Cursor<Vec<u8>>>, String> {
    let method = request.method().clone();
    // Cloned out so no store call (network, disk, thread spawn) runs under the lock.
    let hub = || service.lock().map(|svc| svc.stores()).map_err(|e| e.to_string());
    Ok(match (method, path) {
        (Method::Get, "/api/stores") => json_response(&hub()?.status()),
        (Method::Post, "/api/stores/bandcamp/connect") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            #[derive(serde::Deserialize)]
            struct ConnectBody {
                user: String,
                // A candidate password goes straight into a buffer that wipes itself.
                password: plippa_store::SecretString,
                #[serde(default)]
                consent: Option<bool>,
            }
            // Small body: two short generated strings and a flag.
            match limited_json_body::<ConnectBody>(request, 4096) {
                Ok(body) => match hub()?.connect(&body.user, body.password.expose(), body.consent) {
                    Ok(store) => json_response(&serde_json::json!({"ok": true, "store": store})),
                    Err(e) => store_error(&e),
                },
                Err(resp) => resp,
            }
        }
        (Method::Post, "/api/stores/bandcamp/validate") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match hub()?.validate() {
                Ok(store) => json_response(&serde_json::json!({"ok": true, "store": store})),
                Err(e) => store_error(&e),
            }
        }
        (Method::Post, "/api/stores/bandcamp/sync") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match hub()?.start_sync() {
                Ok(sync) => accepted_json(&serde_json::json!({"ok": true, "sync": sync})),
                Err(e) => store_error(&e),
            }
        }
        (Method::Get, "/api/stores/bandcamp/sync") => json_response(&hub()?.sync_status()),
        (Method::Post, "/api/stores/bandcamp/sync/cancel") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            let sync = hub()?.cancel_sync();
            json_response(&serde_json::json!({"ok": true, "sync": sync}))
        }
        (Method::Delete, "/api/stores/bandcamp") => match hub()?.disconnect() {
            Ok(()) => json_response(&serde_json::json!({"ok": true})),
            Err(e) => store_error(&e),
        },
        // BCS_04: collection (cache-only, searchable, paged).
        (Method::Get, "/api/stores/bandcamp/collection") => {
            let offset = query_param(query, "offset")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            let size = query_param(query, "size")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(crate::service::stores::COLLECTION_PAGE_SIZE);
            let text = query_param(query, "query").unwrap_or_default();
            if text.len() > 256 {
                return Ok(error_json(400, "search text is too long"));
            }
            json_response(&hub()?.collection(&text, offset, size))
        }
        // SRV-06: the cache-only GETs never call the store for an uncached id. A release
        // that is not in the cached collection is looked up through the JSON-guarded POST
        // below (the portal and the device call it after a `not_cached` answer).
        (Method::Post, "/api/stores/bandcamp/release") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            #[derive(serde::Deserialize)]
            struct LookupBody {
                id: String,
            }
            match limited_json_body::<LookupBody>(
                request,
                crate::service::stores::MAX_LOOKUP_BODY,
            ) {
                Ok(body) => match hub()?.lookup_release(&body.id) {
                    Ok(view) => json_response(&view),
                    Err(e) => store_error(&e),
                },
                Err(resp) => resp,
            }
        }
        // BCS_04: one release, in the shape the detail view needs (cache-only gate).
        (Method::Get, p) if p.starts_with("/api/stores/bandcamp/release/") => {
            let raw = &p["/api/stores/bandcamp/release/".len()..];
            if let Some(raw) = raw.strip_suffix("/cover") {
                if raw.is_empty() || raw.len() > 256 {
                    return Ok(error_json(404, "not found"));
                }
                let id = urlencoding_decode(raw);
                return Ok(match hub()?.cover(&id) {
                    Ok(image) => cover_response(&image),
                    Err(e) => store_error(&e),
                });
            }
            if raw.is_empty() || raw.len() > 256 {
                return Ok(error_json(404, "not found"));
            }
            let id = urlencoding_decode(raw);
            match hub()?.release_view(&id) {
                Ok(view) => json_response(&view),
                Err(e) => store_error(&e),
            }
        }
        // BCS_04: consent sheet. Writes nothing (the unit tests snapshot the library).
        (Method::Post, "/api/stores/bandcamp/download/prepare") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match limited_json_body::<crate::service::stores::download::PrepareDownloadRequest>(
                request,
                crate::service::stores::download::MAX_PREPARE_BODY,
            ) {
                Ok(body) => match hub()?.download_prepare(body) {
                    Ok(consent) => json_response(&consent),
                    Err(e) => store_error(&e),
                },
                Err(resp) => resp,
            }
        }
        // BCS_04: start the worker (consent + a consent id from the last prepare).
        (Method::Post, "/api/stores/bandcamp/download") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match limited_json_body::<crate::service::stores::download::DownloadRequest>(
                request,
                crate::service::stores::download::MAX_START_BODY,
            ) {
                Ok(body) => {
                    let transport = match service.lock() {
                        Ok(svc) => svc.web().transport(),
                        Err(_) => return Ok(error_json(503, "the server is busy")),
                    };
                    match hub()?.download_start(transport, body) {
                        Ok(started) => accepted_json(&started),
                        Err(e) => store_error(&e),
                    }
                }
                Err(resp) => resp,
            }
        }
        // BCS_04: download progress and cancel, keyed by the session id.
        (Method::Get, p) if p.starts_with("/api/stores/bandcamp/download/") => {
            let session = &p["/api/stores/bandcamp/download/".len()..];
            if session.is_empty()
                || session.len() > crate::service::stores::download::MAX_SESSION_ID_BYTES
            {
                return Ok(error_json(404, "not found"));
            }
            match hub()?.download_status(session) {
                Ok(status) => json_response(&status),
                Err(e) => store_error(&e),
            }
        }
        (Method::Post, p) if p.starts_with("/api/stores/bandcamp/download/") => {
            let Some(session) = p["/api/stores/bandcamp/download/".len()..].strip_suffix("/cancel")
            else {
                return Ok(error_json(404, "not found"));
            };
            if session.is_empty()
                || session.len() > crate::service::stores::download::MAX_SESSION_ID_BYTES
            {
                return Ok(error_json(404, "not found"));
            }
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match hub()?.download_cancel(session) {
                Ok(status) => json_response(&status),
                Err(e) => store_error(&e),
            }
        }
        // BCS_06: ownership matching. GET is cache-only: it never fetches the page or the
        // store, so the tap flow can ask before any internet consent exists.
        (Method::Get, "/api/stores/bandcamp/match") => {
            let Some(url) = query_param(query, "url") else {
                return Ok(error_json(400, "url is required"));
            };
            let expected = match expected_duration_param(query) {
                Ok(expected) => expected,
                Err(response) => return Ok(response),
            };
            let request = crate::service::stores::MatchRequest {
                url,
                expected_duration: expected,
                allow_search: false,
            };
            match hub()?.match_release(&request) {
                Ok(result) => json_response(&result),
                Err(e) => store_error(&e),
            }
        }
        // BCS_06: the same question with the store allowed to be asked (`search3` is an
        // opt-in lead only). The GET route above is the zero-network path and must stay so.
        (Method::Post, "/api/stores/bandcamp/match") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match limited_json_body::<crate::service::stores::MatchRequest>(
                request,
                crate::service::stores::MAX_MATCH_BODY,
            ) {
                Ok(body) => match hub()?.match_release(&body) {
                    Ok(result) => json_response(&result),
                    Err(e) => store_error(&e),
                },
                Err(resp) => resp,
            }
        }
        // BCS_07: the composite tap answer. Cache only (never the network), so the device may
        // ask before the general web-consent prompt: one round trip for the whole screen.
        (Method::Post, "/api/stores/tap") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match limited_json_body::<crate::service::stores::TapRequest>(
                request,
                crate::service::stores::MAX_TAP_BODY,
            ) {
                Ok(body) => match hub()?.tap(&body) {
                    Ok(answer) => json_response(&answer),
                    Err(e) => store_error(&e),
                },
                Err(resp) => resp,
            }
        }
        // BCS_07: remember interest in a release the person does not own yet, so the next
        // tap after the purchase re-offers the download.
        (Method::Post, "/api/stores/bandcamp/want") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match limited_json_body::<crate::service::stores::WantRequest>(
                request,
                crate::service::stores::MAX_TAP_BODY,
            ) {
                Ok(body) => match hub()?.want(&body) {
                    Ok(answer) => json_response(&answer),
                    Err(e) => store_error(&e),
                },
                Err(resp) => resp,
            }
        }
        // BCS_06: record the candidate the person confirmed (the server re-scores it).
        (Method::Post, "/api/stores/bandcamp/match/confirm") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match limited_json_body::<crate::service::stores::ConfirmMatchRequest>(
                request,
                crate::service::stores::MAX_MATCH_BODY,
            ) {
                Ok(body) => match hub()?.confirm_match(&body) {
                    Ok(confirmation) => json_response(&confirmation),
                    Err(e) => store_error(&e),
                },
                Err(resp) => resp,
            }
        }
        // Cache reads for browsing (BCS_04 consumes these): memory first, no network.
        (Method::Get, "/api/stores/bandcamp/releases") => {
            let hub = hub()?;
            let cache = hub
                .status()
                .stores
                .first()
                .map(|store| store.cache.clone())
                .unwrap_or_default();
            json_response(&serde_json::json!({
                "cache": cache,
                "releases": hub.cached_releases(),
            }))
        }
        (Method::Get, p) if p.starts_with("/api/stores/bandcamp/releases/") => {
            let raw = &p["/api/stores/bandcamp/releases/".len()..];
            if raw.is_empty() || raw.len() > 256 {
                return Ok(error_json(404, "not found"));
            }
            let id = urlencoding_decode(raw);
            // SRV-06: legacy alias of the cache-only release view; no cache miss reaches
            // the store from a GET.
            match hub()?.release_view(&id) {
                Ok(view) => json_response(&view),
                Err(e) => store_error(&e),
            }
        }
        _ => error_json(404, "not found"),
    })
}

// ---------------------------------------------------------------------------
// NB_08 web lookup (consent-gated)
// ---------------------------------------------------------------------------

fn is_web_path(p: &str) -> bool {
    p == "/api/web/status"
        || p == "/api/web/prepare"
        || p == "/api/web/fetch"
        || p.starts_with("/api/web/session/")
}

/// Web error as JSON: `{"error": message, "code": stable_code}`.
fn web_error(e: &WebError) -> Response<Cursor<Vec<u8>>> {
    let body = serde_json::json!({ "error": e.to_string(), "code": e.code() });
    Response::from_data(serde_json::to_vec(&body).unwrap_or_default())
        .with_status_code(StatusCode(e.http_status()))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
                .expect("header"),
        )
}

fn web_route(
    request: &mut Request,
    path: &str,
    service: &Arc<Mutex<ServerService>>,
) -> Result<Response<Cursor<Vec<u8>>>, String> {
    let method = request.method().clone();
    let lock = || service.lock().map_err(|e| e.to_string());
    Ok(match (method, path) {
        (Method::Get, "/api/web/status") => {
            let hub = lock()?.web();
            json_response(&hub.status())
        }
        (Method::Post, "/api/web/prepare") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match limited_json_body::<WebRequest>(request, 4096) {
                Ok(req) => match prepare_unlocked(service, &req.noubin_key) {
                    Ok(p) => json_response(&p),
                    Err(e) => web_error(&e),
                },
                Err(resp) => resp,
            }
        }
        (Method::Post, "/api/web/fetch") => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match limited_json_body::<WebRequest>(request, 4096) {
                // The fetch blocks this request's own thread only, never the service lock.
                Ok(req) => match fetch_unlocked(service, &req.noubin_key, req.consent) {
                    Ok(r) => json_response(&r),
                    Err(e) => web_error(&e),
                },
                Err(resp) => resp,
            }
        }
        (m, p) if p.starts_with("/api/web/session/") => {
            let token = &p["/api/web/session/".len()..];
            if !is_web_token(token) {
                return Ok(web_error(&WebError::UnknownSession));
            }
            let hub = lock()?.web();
            match m {
                Method::Get => match hub.session_overview(token) {
                    Ok(o) => json_response(&o),
                    Err(e) => web_error(&e),
                },
                Method::Delete => {
                    if hub.discard(token) {
                        json_response(&serde_json::json!({"ok": true}))
                    } else {
                        web_error(&WebError::UnknownSession)
                    }
                }
                _ => error_json(405, "method not allowed"),
            }
        }
        _ => error_json(404, "not found"),
    })
}

/// Body of a JSON request, at most `max` bytes (413 beyond), parsed as `T` (400 if malformed).
fn limited_json_body<T: serde::de::DeserializeOwned>(
    request: &mut Request,
    max: usize,
) -> Result<T, Response<Cursor<Vec<u8>>>> {
    let len = request.body_length().unwrap_or(0);
    if len > max {
        return Err(error_json(413, "request body too large"));
    }
    let mut buf = vec![0u8; len];
    std::io::Read::read_exact(request.as_reader(), &mut buf)
        .map_err(|e| error_json(400, &format!("bad body: {e}")))?;
    let parsed = serde_json::from_slice(&buf).map_err(|e| error_json(400, &format!("bad JSON: {e}")));
    // The body may be a password (a store connect, a portal PIN); it must not sit in
    // freed memory after the request is answered (PAR-07).
    plippa_store::wipe_bytes(&mut buf);
    parsed
}

fn cues_route(
    request: &mut Request,
    path: &str,
    query: &str,
    service: &Arc<Mutex<ServerService>>,
) -> Result<Response<Cursor<Vec<u8>>>, String> {
    let method = request.method().clone();
    let segs: Vec<&str> = path.strip_prefix('/').unwrap_or_default().split('/').collect();
    let lock = || service.lock().map_err(|e| e.to_string());
    // JSON body limit: the paste plus JSON escaping headroom.
    let max_body = MAX_CUE_PASTE_BYTES * 2 + 4096;
    let item_q = query_param(query, "item").and_then(|v| v.parse::<u32>().ok());

    let response = match (method, segs.as_slice()) {
        (Method::Post, ["api", "cues", "preview"]) => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match limited_json_body::<CuePreviewRequest>(request, max_body) {
                Ok(req) => {
                    // Without an item to look up, no service state is needed at all.
                    if req.duration_ms.is_some() || req.id.is_none() {
                        respond(preview_cues(&req.text, req.duration_ms))
                    } else {
                        respond(lock()?.preview_cues(&req))
                    }
                }
                Err(resp) => resp,
            }
        }
        (Method::Get, ["api", "items", id, "cues"]) => match id.parse::<u64>() {
            Ok(id) => respond(lock()?.get_item_cues(id, item_q)),
            Err(_) => error_json(400, "bad playable id"),
        },
        (Method::Put, ["api", "items", id, "cues"]) => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match id.parse::<u64>() {
                Ok(id) => match limited_json_body::<SaveCuesRequest>(request, max_body) {
                    Ok(mut req) => {
                        req.item = req.item.or(item_q);
                        req.revision = revision_arg(request, query, req.revision.take());
                        respond(lock()?.put_item_cues(id, req))
                    }
                    Err(resp) => resp,
                },
                Err(_) => error_json(400, "bad playable id"),
            }
        }
        // NB_16: give an orphan media file its `.noudata` (kind chosen by the user).
        (Method::Post, ["api", "items", id, "noudata"]) => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match id.parse::<u64>() {
                Ok(id) => match limited_json_body::<CreateNoudataRequest>(request, 8192) {
                    Ok(req) => match ImportKind::parse(&req.kind) {
                        Some(kind) => respond(lock()?.create_noudata_for_orphan(
                            id,
                            kind,
                            req.title.as_deref(),
                        )),
                        None => {
                            error_json(400, "kind must be music, mix, audiobook, podcast or other")
                        }
                    },
                    Err(resp) => resp,
                },
                Err(_) => error_json(400, "bad playable id"),
            }
        }
        _ => error_json(404, "not found"),
    };
    Ok(response)
}

/// `POST /api/items/:id/noudata` (NB_16).
#[derive(serde::Deserialize)]
struct CreateNoudataRequest {
    kind: String,
    #[serde(default)]
    title: Option<String>,
}

// ---------------------------------------------------------------------------
// ART_03 cover art
// ---------------------------------------------------------------------------

fn is_covers_path(p: &str) -> bool {
    let segs: Vec<&str> = p.strip_prefix('/').unwrap_or_default().split('/').collect();
    matches!(
        segs.as_slice(),
        ["api", "collections", _, "covers", ..] | ["api", "items", _, "covers", ..]
    )
}

/// `Cache-Control` of a served cover: the URL does not change when the edit popup replaces or
/// reorders images, so the browser revalidates every time (the strong `ETag` answers `304`).
const COVER_CACHE_CONTROL: &str = "private, no-cache";

/// Send the planned image: `304` when the browser's `ETag` still matches, else the bytes with
/// the sniffed type. The file is read here, after the service lock was released. `cache` is
/// false for previews of uploads that are not part of the library yet.
fn serve_cover(
    request: &Request,
    plan: Result<CoverPlan, ServerError>,
    cache: bool,
) -> Response<Cursor<Vec<u8>>> {
    let plan = match plan {
        Ok(p) => p,
        Err(e) => return service_error(&e),
    };
    let etag = Header::from_bytes(&b"ETag"[..], plan.etag.as_bytes()).expect("header");
    let cache_control = Header::from_bytes(
        &b"Cache-Control"[..],
        if cache { COVER_CACHE_CONTROL } else { "no-store" }.as_bytes(),
    )
    .expect("header");
    let unchanged = header_value(request, "If-None-Match").is_some_and(|v| {
        v.split(',')
            .map(|t| t.trim().trim_start_matches("W/"))
            .any(|t| t == "*" || t == plan.etag)
    });
    if unchanged {
        return Response::from_data(Vec::new())
            .with_status_code(StatusCode(304))
            .with_header(etag)
            .with_header(cache_control);
    }
    match plan.read() {
        Ok((bytes, ctype)) => Response::from_data(bytes)
            .with_status_code(StatusCode(200))
            .with_header(
                Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).expect("header"),
            )
            .with_header(etag)
            .with_header(cache_control),
        Err(e) => service_error(&e),
    }
}

fn covers_route(
    request: &mut Request,
    path: &str,
    query: &str,
    service: &Arc<Mutex<ServerService>>,
) -> Result<Response<Cursor<Vec<u8>>>, String> {
    let method = request.method().clone();
    let segs: Vec<&str> = path.strip_prefix('/').unwrap_or_default().split('/').collect();
    let lock = || service.lock().map_err(|e| e.to_string());
    let bad_id = || error_json(400, "bad playable id");
    let scope_of = |items: bool| {
        if items {
            CoverScope::Item(
                query_param(query, "item")
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(0),
            )
        } else {
            CoverScope::Collection
        }
    };
    let size = match query_param(query, "size") {
        None => CoverSize::Full,
        Some(s) => match CoverSize::parse(&s) {
            Some(v) => v,
            None => return Ok(error_json(400, "size must be 128, 512, 1024 or full")),
        },
    };
    Ok(match (method, segs.as_slice()) {
        (Method::Get, ["api", area @ ("collections" | "items"), id, "covers"]) => {
            match id.parse::<u64>() {
                Ok(id) => respond(lock()?.list_covers(id, scope_of(*area == "items"))),
                Err(_) => bad_id(),
            }
        }
        (Method::Get, ["api", area @ ("collections" | "items"), id, "covers", n]) => {
            match (id.parse::<u64>(), n.parse::<usize>()) {
                (Ok(id), Ok(n)) => {
                    let plan = lock()?.cover_plan(id, scope_of(*area == "items"), n, size);
                    serve_cover(request, plan, true)
                }
                (Err(_), _) => bad_id(),
                (_, Err(_)) => error_json(404, "no such cover image"),
            }
        }
        (Method::Get, ["api", "collections", id, "covers", "staged", sid]) => {
            match id.parse::<u64>() {
                Ok(id) => {
                    let plan = lock()?.staged_cover_plan(id, sid);
                    serve_cover(request, plan, false)
                }
                Err(_) => bad_id(),
            }
        }
        (Method::Delete, ["api", "collections", id, "covers", "staged", sid]) => {
            match id.parse::<u64>() {
                Ok(id) => match lock()?.discard_staged_cover(id, sid) {
                    Ok(()) => json_response(&serde_json::json!({ "ok": true })),
                    Err(e) => service_error(&e),
                },
                Err(_) => bad_id(),
            }
        }
        (Method::Post, ["api", "collections", id, "covers", "staged"]) => {
            let Ok(id) = id.parse::<u64>() else {
                return Ok(bad_id());
            };
            let Some(len) = request.body_length() else {
                return Ok(error_json(411, "Content-Length is required for uploads"));
            };
            if len as u64 > MAX_COVER_BYTES {
                return Ok(error_json(413, "image too large (limit 16 MB)"));
            }
            let hub = lock()?.imports();
            let Some(slot) = hub.try_upload_slot(client_of(request)) else {
                return Ok(upload_busy_response());
            };
            let ctx = match lock()?.begin_cover_stage(id) {
                Ok(c) => c,
                Err(e) => {
                    drain_body(request);
                    return Ok(service_error(&e));
                }
            };
            match stage_cover(
                &ctx,
                &mut slot.reader(request.as_reader()),
                Some(len as u64),
                hub.limits(),
            ) {
                Ok(v) => json_response(&v),
                Err(e) => {
                    if e.status == 415 {
                        drain_body(request);
                    }
                    import_error_response(&e)
                }
            }
        }
        (Method::Put, ["api", "collections", id, "covers"]) => {
            if let Err(resp) = json_write_guard(request) {
                return Ok(resp);
            }
            match id.parse::<u64>() {
                Ok(id) => match limited_json_body::<PutCovers>(request, 64 * 1024) {
                    Ok(req) => respond(lock()?.put_covers(id, req)),
                    Err(resp) => resp,
                },
                Err(_) => bad_id(),
            }
        }
        _ => error_json(404, "not found"),
    })
}

// ---------------------------------------------------------------------------
// NB_03 collections / playlists
// ---------------------------------------------------------------------------

fn is_collections_path(p: &str) -> bool {
    p == "/api/kinds"
        || p == "/api/tracks"
        || p == "/api/collections"
        || p.starts_with("/api/collections/")
        || p == "/api/playlists"
        || p.starts_with("/api/playlists/")
}

/// WAVE_01: waveform analysis status (read-only GET).
fn is_waveforms_path(p: &str) -> bool {
    p == "/api/waveforms" || p.starts_with("/api/waveforms/")
}

/// `GET /api/waveforms/:id` — registry snapshot + header fallback, no CSRF / `X-Plippa` header
/// (read-only, same as the other collection GETs).
fn waveforms_route(
    request: &Request,
    path: &str,
    service: &Arc<Mutex<ServerService>>,
) -> Result<Response<Cursor<Vec<u8>>>, String> {
    let method = request.method().clone();
    let segs: Vec<&str> = path.strip_prefix('/').unwrap_or_default().split('/').collect();
    let response = match (method, segs.as_slice()) {
        (Method::Get, ["api", "waveforms", id]) => match id.parse::<u64>() {
            Ok(id) => {
                // Take the snapshot under the lock, then stat sidecar headers after it is
                // released: a slow stick must not block the rest of the portal.
                let plan = service
                    .lock()
                    .map_err(|e| e.to_string())?
                    .waveform_plan(id);
                respond(plan.map(|p| p.resolve()))
            }
            Err(_) => error_json(400, "bad playable id"),
        },
        _ => error_json(404, "not found"),
    };
    Ok(response)
}

/// TRACK_01: `POST /api/media/:id/analysis/reanalyse`.
fn is_reanalyse_path(p: &str) -> bool {
    let segs: Vec<&str> = p.strip_prefix('/').unwrap_or_default().split('/').collect();
    matches!(segs.as_slice(), ["api", "media", _, "analysis", "reanalyse"])
}

/// Body of the re-analysis request. Both fields are optional: no `item` is the first media
/// item; no `kinds` is every analysis kind.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ReanalyseBody {
    item: Option<u32>,
    kinds: Option<Vec<String>>,
}

/// `POST /api/media/:id/analysis/reanalyse`: put one media item at the front of the analysis
/// queue. 202 `{"state":"queued"}`; 400 bad body / unknown kind; 404 unknown id or item; 409
/// `unavailable` when the media file is missing; 503 when the storage vanished. The generic
/// `state_change_guard` has already run, so this route has no exemption of its own.
fn reanalyse_route(
    request: &mut Request,
    path: &str,
    service: &Arc<Mutex<ServerService>>,
) -> Response<Cursor<Vec<u8>>> {
    use crate::service::{request_reanalysis, validate_kinds, ReanalyseRefusal};
    let segs: Vec<&str> = path.strip_prefix('/').unwrap_or_default().split('/').collect();
    let Some(id) = segs.get(2).and_then(|s| s.parse::<u64>().ok()) else {
        return error_json(400, "bad playable id");
    };
    let body = if request.body_length().unwrap_or(0) == 0 {
        ReanalyseBody::default()
    } else {
        match limited_json_body::<ReanalyseBody>(request, 4 * 1024) {
            Ok(b) => b,
            Err(resp) => return resp,
        }
    };
    if let Err(e) = validate_kinds(body.kinds.as_deref()) {
        return service_error(&e);
    }
    match request_reanalysis(service, id, body.item.unwrap_or(0)) {
        Ok(()) => json_with_status(202, &serde_json::json!({ "state": "queued" })),
        Err(ReanalyseRefusal::Unavailable(message)) => coded_error(409, "unavailable", &message),
        Err(ReanalyseRefusal::Server(e)) => service_error(&e),
    }
}

/// Map a service error to its HTTP status: 404 / 409 / 503 / 500, else 400.
fn service_error(e: &ServerError) -> Response<Cursor<Vec<u8>>> {
    error_json(e.http_status(), &e.to_string())
}

fn respond<T: Serialize>(r: Result<T, ServerError>) -> Response<Cursor<Vec<u8>>> {
    match r {
        Ok(v) => json_response(&v),
        Err(e) => service_error(&e),
    }
}

/// Parse a JSON request body (at most [`MAX_JSON_BODY`] bytes: 413 beyond, 400 if malformed).
fn json_body<T: serde::de::DeserializeOwned>(
    request: &mut Request,
) -> Result<T, Response<Cursor<Vec<u8>>>> {
    limited_json_body(request, MAX_JSON_BODY)
}

/// Client's last-seen revision: body field wins, then `?revision=`, then `If-Match`.
fn revision_arg(request: &Request, query: &str, from_body: Option<String>) -> Option<String> {
    from_body
        .filter(|r| !r.is_empty())
        .or_else(|| query_param(query, "revision"))
        .or_else(|| {
            request
                .headers()
                .iter()
                .find(|h| h.field.equiv("If-Match"))
                .map(|h| h.value.as_str().trim().trim_matches('"').to_string())
        })
        .filter(|r| !r.is_empty())
}

// ---------------------------------------------------------------------------
// BCS_10 library delete (preview / confirm)
// ---------------------------------------------------------------------------

/// Which paths this module owns. Only the exact shapes; anything else falls through to the
/// older routes (`GET /api/playables/:id`, `DELETE /api/collections/:id/covers/staged/:sid`…).
fn is_library_delete_path(method: &Method, p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/") else {
        return false;
    };
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    matches!(
        (method, segs.as_slice()),
        (Method::Get, ["playables", _, "delete", "preview"])
            | (Method::Get, ["playables", _, "files", _, "delete", "preview"])
            | (Method::Get, ["collections", _, "delete", "preview"])
            | (Method::Delete, ["playables", _])
            | (Method::Delete, ["playables", _, "files", _])
            | (Method::Delete, ["collections", _])
    )
}

fn json_with_status(status: u16, value: &serde_json::Value) -> Response<Cursor<Vec<u8>>> {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(bytes)
        .with_status_code(StatusCode(status))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
                .expect("header"),
        )
}

fn coded_error(status: u16, code: &str, message: &str) -> Response<Cursor<Vec<u8>>> {
    json_with_status(
        status,
        &serde_json::json!({ "ok": false, "error": message, "code": code }),
    )
}

/// A refused delete: the preview (when there is one) plus `ok`/`code`/`error`.
fn delete_error_response(e: &DeleteError) -> Response<Cursor<Vec<u8>>> {
    match e.preview() {
        Some(preview) => {
            let mut value = serde_json::to_value(preview).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = value.as_object_mut() {
                obj.insert("ok".into(), serde_json::Value::Bool(false));
                obj.insert("code".into(), serde_json::json!(e.code()));
                obj.insert("error".into(), serde_json::json!(e.message()));
            }
            json_with_status(e.status(), &value)
        }
        None => coded_error(e.status(), e.code(), &e.message()),
    }
}

/// `DELETE` body: optional `{ "force": true, "reason": "…" }`.
fn delete_body(request: &mut Request) -> Result<DeleteRequest, Response<Cursor<Vec<u8>>>> {
    const MAX: usize = 4 * 1024;
    let len = request.body_length().unwrap_or(0);
    if len > MAX {
        return Err(coded_error(413, "too_large", "request body too large"));
    }
    if len == 0 {
        return Ok(DeleteRequest::default());
    }
    let mut buf = vec![0u8; len];
    std::io::Read::read_exact(request.as_reader(), &mut buf)
        .map_err(|e| coded_error(400, "bad_body", &format!("bad body: {e}")))?;
    serde_json::from_slice(&buf)
        .map_err(|e| coded_error(400, "bad_json", &format!("bad JSON: {e}")))
}

fn delete_route(
    request: &mut Request,
    path: &str,
    service: &Arc<Mutex<ServerService>>,
) -> Result<Response<Cursor<Vec<u8>>>, String> {
    let method = request.method().clone();
    let segs: Vec<&str> = path
        .strip_prefix('/').unwrap_or_default()
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let lock = || service.lock().map_err(|e| e.to_string());
    let id_of = |s: &str| s.parse::<u64>().ok();
    let bad_id = || coded_error(400, "bad_id", "bad playable id");
    let bad_item = || coded_error(400, "bad_item", "bad mediaList position");
    let preview = |r: Result<DeletePreview, DeleteError>| match r {
        Ok(p) => json_response(&p),
        Err(e) => delete_error_response(&e),
    };
    let outcome = |r: Result<DeleteOutcome, DeleteError>| match r {
        Ok(o) => json_response(&o),
        Err(e) => delete_error_response(&e),
    };

    let response = match (method, segs.as_slice()) {
        (Method::Get, ["api", "playables", id, "delete", "preview"]) => match id_of(id) {
            Some(id) => preview(lock()?.preview_delete_playable(id)),
            None => bad_id(),
        },
        (Method::Get, ["api", "playables", id, "files", n, "delete", "preview"]) => {
            match (id_of(id), n.parse::<u32>().ok()) {
                (Some(id), Some(n)) => preview(lock()?.preview_delete_playable_file(id, n)),
                (None, _) => bad_id(),
                (_, None) => bad_item(),
            }
        }
        (Method::Get, ["api", "collections", id, "delete", "preview"]) => match id_of(id) {
            Some(id) => preview(lock()?.preview_delete_playable(id)),
            None => bad_id(),
        },
        (Method::Delete, ["api", "playables", id]) => {
            let Some(id) = id_of(id) else {
                return Ok(bad_id());
            };
            let body = match delete_body(request) {
                Ok(b) => b,
                Err(resp) => return Ok(resp),
            };
            outcome(lock()?.delete_playable(id, body))
        }
        (Method::Delete, ["api", "playables", id, "files", n]) => {
            let (Some(id), Some(n)) = (id_of(id), n.parse::<u32>().ok()) else {
                return Ok(bad_item());
            };
            let body = match delete_body(request) {
                Ok(b) => b,
                Err(resp) => return Ok(resp),
            };
            outcome(lock()?.delete_playable_file(id, n, body))
        }
        (Method::Delete, ["api", "collections", id]) => {
            let Some(id) = id_of(id) else {
                return Ok(bad_id());
            };
            let body = match delete_body(request) {
                Ok(b) => b,
                Err(resp) => return Ok(resp),
            };
            outcome(lock()?.delete_collection(id, body))
        }
        _ => error_json(404, "not found"),
    };
    Ok(response)
}

fn collections_route(
    request: &mut Request,
    path: &str,
    query: &str,
    service: &Arc<Mutex<ServerService>>,
) -> Result<Response<Cursor<Vec<u8>>>, String> {
    let method = request.method().clone();
    let segs: Vec<&str> = path.strip_prefix('/').unwrap_or_default().split('/').collect();
    let id_of = |s: &str| s.parse::<u64>().ok();
    let bad_id = || error_json(400, "bad playable id");
    let lock = || service.lock().map_err(|e| e.to_string());

    let response = match (method, segs.as_slice()) {
        (Method::Get, ["api", "kinds"]) => respond(lock()?.collection_kinds().map(|kinds| {
            serde_json::json!({ "kinds": kinds })
        })),
        (Method::Get, ["api", "tracks"]) => {
            let view = match query_param(query, "view").as_deref().and_then(LibraryKind::parse) {
                Some(v) if plippa_noubin::view_is_item_level(v) => v,
                _ => return Ok(error_json(400, "invalid view (track|mix)")),
            };
            respond(lock()?.list_tracks(view).map(|t| serde_json::json!({ "tracks": t })))
        }
        (Method::Get, ["api", "collections"]) => {
            let kind = match query_param(query, "kind").filter(|k| !k.is_empty()) {
                None => None,
                Some(k) => match LibraryKind::parse(&k) {
                    Some(k) => Some(k),
                    None => {
                        return Ok(error_json(
                            400,
                            "invalid kind (track|album|playlist|mix|audiobook|podcast)",
                        ))
                    }
                },
            };
            respond(lock()?.list_collections(kind).map(|c| {
                serde_json::json!({ "collections": c })
            }))
        }
        (Method::Get, ["api", "collections", id]) => match id_of(id) {
            Some(id) => respond(lock()?.get_collection(id)),
            None => bad_id(),
        },
        (Method::Get, ["api", "collections", id, "cover"]) => match id_of(id) {
            Some(id) => {
                let plan = lock()?.cover_plan(
                    id,
                    CoverScope::Collection,
                    0,
                    CoverSize::Tier(ThumbTier::T512),
                );
                serve_cover(request, plan, true)
            }
            None => bad_id(),
        },
        (Method::Post, ["api", "collections", id, "play"]) => match id_of(id) {
            Some(id) => {
                // Body optional: `{}` / empty = the whole collection.
                let req = if request.body_length().unwrap_or(0) == 0 {
                    Ok(PlayCollection::default())
                } else {
                    json_body::<PlayCollection>(request)
                };
                match req {
                    // Waits for the engine between steps: must not hold the service lock.
                    Ok(req) => respond(play_collection_unlocked(service, id, req)),
                    Err(resp) => resp,
                }
            }
            None => bad_id(),
        },
        (Method::Post, ["api", "playlists"]) => match json_body::<CreatePlaylist>(request) {
            Ok(req) => respond(lock()?.create_playlist(req)),
            Err(resp) => resp,
        },
        (Method::Patch, ["api", "playlists", id]) => match id_of(id) {
            Some(id) => match json_body::<RenamePlaylist>(request) {
                Ok(mut req) => {
                    req.revision = revision_arg(request, query, req.revision.take());
                    respond(lock()?.rename_playlist(id, req))
                }
                Err(resp) => resp,
            },
            None => bad_id(),
        },
        (Method::Delete, ["api", "playlists", id]) => match id_of(id) {
            Some(id) => {
                let rev = revision_arg(request, query, None);
                respond(
                    lock()?
                        .delete_playlist(id, rev.as_deref())
                        .map(|()| serde_json::json!({"ok": true})),
                )
            }
            None => bad_id(),
        },
        (Method::Post, ["api", "playlists", id, "items"]) => match id_of(id) {
            Some(id) => match json_body::<AddPlaylistItems>(request) {
                Ok(mut req) => {
                    req.revision = revision_arg(request, query, req.revision.take());
                    respond(lock()?.add_playlist_items(id, req))
                }
                Err(resp) => resp,
            },
            None => bad_id(),
        },
        (Method::Delete, ["api", "playlists", id, "items", pos]) => {
            match (id_of(id), pos.parse::<usize>().ok()) {
                (Some(id), Some(pos)) => {
                    let rev = revision_arg(request, query, None);
                    let want = query_param(query, "path");
                    respond(lock()?.remove_playlist_item(id, pos, want.as_deref(), rev.as_deref()))
                }
                (None, _) => bad_id(),
                (_, None) => error_json(400, "bad item position"),
            }
        }
        (Method::Put, ["api", "playlists", id, "order"]) => match id_of(id) {
            Some(id) => match json_body::<ReorderPlaylist>(request) {
                Ok(mut req) => {
                    req.revision = revision_arg(request, query, req.revision.take());
                    respond(lock()?.reorder_playlist(id, req))
                }
                Err(resp) => resp,
            },
            None => bad_id(),
        },
        (Method::Put, ["api", "collections", id, "order"]) => match id_of(id) {
            Some(id) => match json_body::<ReorderPlaylist>(request) {
                Ok(mut req) => {
                    req.revision = revision_arg(request, query, req.revision.take());
                    respond(lock()?.reorder_collection(id, req))
                }
                Err(resp) => resp,
            },
            None => bad_id(),
        },
        _ => error_json(404, "not found"),
    };
    Ok(response)
}

/// The embedded page and scripts, by exact path. The one table behind both the router and the
/// gate's public list, so an asset cannot be served without being public or the reverse.
fn static_asset(path: &str) -> Option<(&'static str, &'static str)> {
    const JS: &str = "application/javascript; charset=utf-8";
    Some(match path {
        "/" | "/index.html" => (INDEX_HTML, "text/html; charset=utf-8"),
        "/app.js" => (APP_JS, JS),
        "/covers.js" => (COVERS_JS, JS),
        "/library.js" => (LIBRARY_JS, JS),
        "/ui.js" => (UI_JS, JS),
        "/signin.js" => (SIGNIN_JS, JS),
        "/addmedia.js" => (ADDMEDIA_JS, JS),
        "/editcues.js" => (EDITCUES_JS, JS),
        "/editcovers.js" => (EDITCOVERS_JS, JS),
        "/unite.js" => (UNITE_JS, JS),
        "/stores.js" => (STORES_JS, JS),
        "/deleteitem.js" => (DELETEITEM_JS, JS),
        "/styles.css" => (STYLES_CSS, "text/css; charset=utf-8"),
        _ => return None,
    })
}

/// The request target as `(path, query)`, or why it is not canonical.
///
/// Canonical means: it starts with exactly one `/` (no absolute-form target), has no fragment,
/// no control character, no backslash and no non-ASCII byte in the path, no empty segment (`//`),
/// no `.` or `..` segment, no trailing slash (except the root itself), and no percent-encoded
/// slash, backslash, dot, NUL or control character. Everything the portal's own page asks for is
/// canonical; anything else is refused with 400 before routing, so every router sees the same
/// path the gate did.
fn canonical_target(url: &str) -> Result<(String, String), &'static str> {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    if !path.starts_with('/') {
        return Err("no leading slash");
    }
    if url.bytes().any(|b| b < 0x20 || b == 0x7f) || url.contains('#') {
        return Err("control character or fragment");
    }
    if !path.is_ascii() || path.contains('\\') {
        return Err("non-ASCII or backslash in the path");
    }
    if path != "/" {
        if path.ends_with('/') {
            return Err("trailing slash");
        }
        for seg in path[1..].split('/') {
            if seg.is_empty() || seg == "." || seg == ".." {
                return Err("empty or dot segment");
            }
        }
    }
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = path.get(i + 1..i + 3).ok_or("truncated escape")?;
            let value = u8::from_str_radix(hex, 16).map_err(|_| "bad escape")?;
            if matches!(value, b'/' | b'\\' | b'.') || value < 0x20 || value == 0x7f {
                return Err("encoded separator, dot or control character");
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    Ok((path.to_string(), query.to_string()))
}

/// `expectedDuration` in whole seconds (BCS_06); an empty value is "not supplied".
fn expected_duration_param(query: &str) -> Result<Option<u64>, Response<Cursor<Vec<u8>>>> {
    match query_param(query, "expectedDuration") {
        None => Ok(None),
        Some(text) if text.trim().is_empty() => Ok(None),
        Some(text) => text
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| error_json(400, "expectedDuration must be whole seconds")),
    }
}

fn query_param(query: impl AsRef<str>, key: &str) -> Option<String> {
    for pair in query.as_ref().split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(urlencoding_decode(v));
            }
        }
    }
    None
}

fn urlencoding_decode(s: &str) -> String {
    // Percent-decode into bytes, then read them as UTF-8 (multi-byte characters such as
    // `%E2%80%94` must not be split); `+` is a space (form-style query strings).
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_id_suffix(path: &str, prefix: &str) -> Option<u64> {
    path.strip_prefix(prefix)?.parse().ok()
}

fn parse_id_before_suffix(path: &str, prefix: &str, suffix: &str) -> Option<u64> {
    let rest = path.strip_prefix(prefix)?;
    let id = rest.strip_suffix(suffix)?;
    id.parse().ok()
}

fn json_response<T: Serialize>(value: &T) -> Response<Cursor<Vec<u8>>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(body)
        .with_status_code(StatusCode(200))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
                .expect("header"),
        )
}

fn error_json(code: u16, message: &str) -> Response<Cursor<Vec<u8>>> {
    let body = serde_json::json!({ "error": message });
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    Response::from_data(bytes)
        .with_status_code(StatusCode(code))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
                .expect("header"),
        )
}

fn html_response(body: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(body.as_bytes().to_vec())
        .with_status_code(StatusCode(200))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
                .expect("header"),
        )
}

fn text_response(body: &str, content_type: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_data(body.as_bytes().to_vec())
        .with_status_code(StatusCode(200))
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).expect("header"),
        )
}

#[cfg(test)]
mod tests {
    use super::{canonical_target, host_allowed, redirect_target, static_asset, with_port};

    #[test]
    fn only_canonical_request_targets_are_routed() {
        for ok in [
            "/",
            "/index.html",
            "/api/status",
            "/api/collections/12/covers?size=128",
            "/api/stores/bandcamp/release/t%3A2970735116/cover",
            "/api/playables/1099511627776",
            "/api/status?x=/../..//",
        ] {
            assert!(canonical_target(ok).is_ok(), "{ok}");
        }
        for bad in [
            "//api/status",
            "///api/status",
            "/api//status",
            "/./api/status",
            "/api/../api/status",
            "/api/./status",
            "/api/status/",
            "api/status",
            "",
            "*",
            "http://x/api/status",
            "/%2fapi/status",
            "/%2Fapi/status",
            "/api%2fstatus",
            "/%2e%2e/api/status",
            "/api/%2e/status",
            "/api/%5cstatus",
            "/api/status%00",
            "/api/%0d%0a",
            "/api/status\\x",
            "/api/status#frag",
            "/api/\u{e9}",
            "/api/%zz",
            "/api/%4",
            "/api/st\tatus",
        ] {
            assert!(canonical_target(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_redirect_target_is_a_header_value() {
        // Ordinary targets come back unchanged, including an existing percent-escape.
        for same in [
            "/",
            "/api/status?x=1",
            "/api/stores/bandcamp/release/t%3A2970735116/cover",
            "/a/b?q=one+two&r=%20&s=:;@!$'()*,=",
        ] {
            assert_eq!(redirect_target(same), same);
        }
        // Anything that is not printable ASCII is encoded, never copied into the header.
        assert_eq!(redirect_target("/api/status?q=caf\u{e9}"), "/api/status?q=caf%C3%A9");
        assert_eq!(redirect_target("/a b"), "/a%20b");
        assert_eq!(redirect_target("/a\"b"), "/a%22b");
        assert_eq!(redirect_target("/a\\b"), "/a%5Cb");
        assert!(redirect_target("/api/status?q=caf\u{e9}").is_ascii());
    }

    #[test]
    fn the_redirect_port_is_the_listeners() {
        assert_eq!(with_port("127.0.0.1:8787", 8787), "127.0.0.1:8787");
        // A client that dialled the right port but sent the wrong one is still sent to the right
        // one: `Host: <device>:80` must not become `https://<device>:80`.
        assert_eq!(with_port("192.168.1.42:80", 8787), "192.168.1.42:8787");
        assert_eq!(with_port("plippa.local", 8787), "plippa.local:8787");
        assert_eq!(with_port("[fe80::1]:80", 8787), "[fe80::1]:8787");
        assert_eq!(with_port("[::1]", 8787), "[::1]:8787");
        // An empty port is legal and means the default; it must not produce `name::8787`.
        assert_eq!(with_port("plippa.local:", 8787), "plippa.local:8787");
    }

    #[test]
    fn the_static_table_is_exact() {
        assert!(static_asset("/app.js").is_some() && static_asset("/").is_some());
        for no in ["/App.js", "/app.js/", "/api/status", "/app.jsx", "/app", ""] {
            assert!(static_asset(no).is_none(), "{no}");
        }
    }

    #[test]
    fn web_portal_host_allow_list() {
        let none: Vec<String> = Vec::new();
        for ok in [
            "127.0.0.1:8787",
            "192.168.1.20:8787",
            "10.0.0.5",
            "[::1]:8787",
            "[fe80::1]:8787",
            "localhost:8787",
            "plippa",
            "plippa:8787",
            "plippa.local:8787",
            "Plippa.LOCAL",
            "nas.lan",
            "x",
        ] {
            assert!(host_allowed(ok, &none), "{ok}");
        }
        for bad in [
            "attacker.example:8787",
            "evil.com",
            "rebind.attacker.example",
            "127.0.0.1.attacker.example",
            "",
            "[::1",
            "exa mple",
            "a.b.c",
        ] {
            assert!(!host_allowed(bad, &none), "{bad}");
        }
        assert!(host_allowed("plippa.example.com:80", &["plippa.example.com".to_string()]));
    }
}
