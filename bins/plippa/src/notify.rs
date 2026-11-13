//! `sd_notify` with `std` only: one datagram per message to `$NOTIFY_SOCKET`.
//!
//! systemd gives a `Type=notify` service a datagram socket path (or an abstract-namespace name
//! starting with `@`). The launcher reports `READY=1`, `WATCHDOG=1` and `STOPPING=1`. With no
//! `$NOTIFY_SOCKET` (a dev run) every call is a no-op.

use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Environment variable systemd sets for notify services.
pub const ENV_NOTIFY_SOCKET: &str = "NOTIFY_SOCKET";

#[derive(Debug)]
enum Target {
    Path(PathBuf),
    #[cfg(target_os = "linux")]
    Abstract(Vec<u8>),
}

/// Sender for service-manager notifications.
#[derive(Debug)]
pub struct Notifier {
    target: Option<Target>,
    sock: Option<UnixDatagram>,
}

impl Notifier {
    /// A notifier that does nothing.
    pub fn disabled() -> Self {
        Self {
            target: None,
            sock: None,
        }
    }

    /// Read `$NOTIFY_SOCKET` and remove it from the environment so children do not inherit it
    /// (the unit uses `NotifyAccess=main`; children's messages would be ignored anyway).
    pub fn from_env_take() -> Self {
        let v = std::env::var_os(ENV_NOTIFY_SOCKET);
        std::env::remove_var(ENV_NOTIFY_SOCKET);
        match v {
            Some(v) if !v.is_empty() => Self::to_address(&v.to_string_lossy()),
            _ => Self::disabled(),
        }
    }

    /// Send to an explicit socket address (path, or `@name` for the abstract namespace).
    pub fn to_address(addr: &str) -> Self {
        let target = if let Some(name) = addr.strip_prefix('@') {
            #[cfg(target_os = "linux")]
            {
                Target::Abstract(name.as_bytes().to_vec())
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = name;
                return Self::disabled();
            }
        } else {
            Target::Path(PathBuf::from(addr))
        };
        match UnixDatagram::unbound() {
            Ok(sock) => {
                // A full notify queue must never stall supervision: sends fail with
                // WouldBlock (logged) instead of blocking.
                if let Err(e) = sock.set_nonblocking(true) {
                    eprintln!("plippa: cannot make the notify socket non-blocking: {e}");
                    return Self::disabled();
                }
                Self {
                    target: Some(target),
                    sock: Some(sock),
                }
            }
            Err(e) => {
                eprintln!("plippa: cannot create notify socket: {e}");
                Self::disabled()
            }
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.target.is_some()
    }

    fn send(&self, msg: &str) -> io::Result<()> {
        let (Some(target), Some(sock)) = (&self.target, &self.sock) else {
            return Ok(());
        };
        match target {
            Target::Path(p) => sock.send_to(msg.as_bytes(), p).map(|_| ()),
            #[cfg(target_os = "linux")]
            Target::Abstract(name) => {
                use std::os::linux::net::SocketAddrExt;
                let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
                sock.send_to_addr(msg.as_bytes(), &addr).map(|_| ())
            }
        }
    }

    fn send_logged(&self, msg: &str) {
        if let Err(e) = self.send(msg) {
            eprintln!("plippa: sd_notify {msg:?} failed: {e}");
        }
    }

    /// All children started and the engine socket is up (or the readiness window passed).
    pub fn ready(&self) {
        self.send_logged("READY=1");
    }

    /// Keep-alive for `WatchdogSec`.
    pub fn watchdog(&self) {
        self.send_logged("WATCHDOG=1");
    }

    /// The launcher is beginning an orderly stop.
    pub fn stopping(&self) {
        self.send_logged("STOPPING=1");
    }
}

/// How often the launcher pings the systemd watchdog (a third of `WatchdogSec=30`).
pub const WATCHDOG_INTERVAL: Duration = Duration::from_secs(10);

/// Decides when a `WATCHDOG=1` ping is due. The supervision loop polls it on every iteration, so
/// the pings stop only when the loop itself stops making progress (a hung launcher).
#[derive(Debug, Default)]
pub struct WatchdogPacer {
    last: Option<Instant>,
}

impl WatchdogPacer {
    /// True when a ping should be sent now (and records it).
    pub fn poll(&mut self, now: Instant) -> bool {
        match self.last {
            Some(t) if now.saturating_duration_since(t) < WATCHDOG_INTERVAL => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recv(sock: &UnixDatagram) -> String {
        let mut buf = [0u8; 64];
        let n = sock.recv(&mut buf).expect("datagram");
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }

    #[test]
    fn sd_notify_sends_ready_watchdog_and_stopping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.sock");
        let server = UnixDatagram::bind(&path).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let n = Notifier::to_address(path.to_str().unwrap());
        assert!(n.is_enabled());
        n.ready();
        n.watchdog();
        n.stopping();
        assert_eq!(recv(&server), "READY=1");
        assert_eq!(recv(&server), "WATCHDOG=1");
        assert_eq!(recv(&server), "STOPPING=1");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_abstract_namespace_form_is_supported() {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("plippa-notify-test-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let server = UnixDatagram::bind_addr(&addr).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        Notifier::to_address(&format!("@{name}")).ready();
        assert_eq!(recv(&server), "READY=1");
    }

    #[test]
    fn a_disabled_notifier_is_a_no_op() {
        let n = Notifier::disabled();
        assert!(!n.is_enabled());
        n.ready();
        n.watchdog();
        n.stopping();
    }

    #[test]
    fn the_watchdog_pings_every_interval_while_polled() {
        let mut p = WatchdogPacer::default();
        let t0 = Instant::now();
        assert!(p.poll(t0), "first ping at once");
        assert!(!p.poll(t0 + Duration::from_secs(5)), "not due yet");
        assert!(p.poll(t0 + Duration::from_secs(10)));
        assert!(!p.poll(t0 + Duration::from_secs(19)));
        assert!(p.poll(t0 + Duration::from_secs(20)));
    }

    #[test]
    fn a_full_notify_queue_does_not_block_the_sender() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify.sock");
        let _never_read = UnixDatagram::bind(&path).unwrap();
        let n = Notifier::to_address(path.to_str().unwrap());
        let start = Instant::now();
        let mut would_block = false;
        for _ in 0..200_000 {
            if let Err(e) = n.send("WATCHDOG=1") {
                assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
                would_block = true;
                break;
            }
        }
        assert!(would_block, "the queue must fill and report WouldBlock");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
