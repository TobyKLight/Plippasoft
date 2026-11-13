//! PLIPPA PATCH (SECFIX_01 / SRV-02): request-size limits, socket timeouts and the connection
//! gate. Upstream tiny_http reads a header line into an unbounded buffer, puts no timeout on any
//! socket and starts a thread for every accepted connection; on a LAN appliance that lets one
//! request exhaust memory and a pile of half-open sockets exhaust threads.

use std::io::{Error as IoError, ErrorKind, Result as IoResult};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::connection::Connection;

/// Limits applied by a [`Server`](crate::Server). [`Limits::default`] is the production set.
///
/// Every timeout here is a *stall* timeout (re-armed whenever data arrives) or a deadline on the
/// request *head* only. Nothing limits the total duration of a request body: a large upload over
/// a slow link must be able to finish as long as it keeps making progress. The application
/// enforces a minimum rate on top (see `plippa-server`).
#[derive(Debug, Clone)]
pub struct Limits {
    /// Longest request line (`GET /path HTTP/1.1`), answered with `414`.
    pub max_request_line: usize,
    /// Longest single header line, answered with `431`.
    pub max_header_line: usize,
    /// Most header lines, answered with `431`.
    pub max_headers: usize,
    /// Most bytes in the whole head (request line plus headers), answered with `431`.
    pub max_head_bytes: usize,
    /// The whole head must arrive within this time of its first byte (a slow drip that keeps
    /// every read alive is still cut off).
    pub head_deadline: Duration,
    /// Longest silence while the head is arriving.
    pub read_inactivity: Duration,
    /// Longest wait for the first byte of a request on a keep-alive connection.
    pub keep_alive_idle: Duration,
    /// Longest a single write may block (a client that stopped reading).
    pub write_timeout: Duration,
    /// Longest silence while a request body is arriving; re-armed by every read that returns
    /// data. There is deliberately no limit on the total body time.
    pub body_stall: Duration,
    /// Hard ceiling for connection worker threads. The connection gate is the normal limiter;
    /// this stops the pool growing without bound if no gate is installed or it is bypassed.
    pub max_connection_threads: usize,
    /// PLIPPA PATCH (SECFIX_03): how long the accept thread may wait for the first byte of a
    /// connection on a TLS port that also answers plaintext, to tell the two apart. Short,
    /// because the connection gate has already admitted the socket and the loop is blocked
    /// meanwhile; a client that says nothing in this time is closed.
    pub sniff_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_request_line: 8 * 1024,
            max_header_line: 8 * 1024,
            max_headers: 64,
            max_head_bytes: 32 * 1024,
            head_deadline: Duration::from_secs(15),
            read_inactivity: Duration::from_secs(10),
            keep_alive_idle: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            body_stall: Duration::from_secs(30),
            max_connection_threads: 128,
            sniff_timeout: Duration::from_millis(250),
        }
    }
}

/// A way to close a live connection from another thread (`shutdown(Both)`: the connection's
/// thread then sees end of stream and ends).
#[derive(Clone)]
pub struct ConnectionHandle {
    conn: Arc<Connection>,
}

impl ConnectionHandle {
    pub(crate) fn new(conn: &Connection) -> IoResult<Self> {
        Ok(Self {
            conn: Arc::new(conn.try_clone()?),
        })
    }

    /// Close both directions of the connection now.
    pub fn shutdown(&self) {
        let _ = self.conn.shutdown(std::net::Shutdown::Both);
    }
}

/// Why a [`ConnectionGate`] refused a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused;

/// Decides, on the accept thread and before a single byte is read, whether a new connection may
/// be served. The returned guard is dropped when the connection ends. A refused connection gets
/// `503` with `Retry-After: 5` and is closed; it is never queued.
pub trait ConnectionGate: Send + Sync + 'static {
    /// `peer` is `None` for a UNIX socket. `handle` lets the gate close the connection later.
    fn admit(
        &self,
        peer: Option<SocketAddr>,
        handle: ConnectionHandle,
    ) -> Result<Box<dyn Send>, Refused>;
}

/// Server construction options beyond the listen address.
#[derive(Clone, Default)]
pub struct ServerOptions {
    pub limits: Limits,
    pub gate: Option<Arc<dyn ConnectionGate>>,
}

/// What the read side of a connection is allowed to wait for right now. Shared between the
/// connection thread (which moves it between phases) and the socket reader (which applies it).
#[derive(Debug)]
pub(crate) struct ReadTimer {
    state: Mutex<TimerState>,
}

#[derive(Debug, Clone, Copy)]
struct TimerState {
    inactivity: Duration,
    deadline: Option<Instant>,
}

impl ReadTimer {
    pub(crate) fn new(inactivity: Duration) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(TimerState {
                inactivity,
                deadline: None,
            }),
        })
    }

    /// Switch phase: wait at most `inactivity` for each read, and (if given) stop altogether at
    /// `deadline`.
    pub(crate) fn arm(&self, inactivity: Duration, deadline: Option<Instant>) {
        if let Ok(mut s) = self.state.lock() {
            *s = TimerState {
                inactivity,
                deadline,
            };
        }
    }

    /// The socket read timeout to use for the next read, or `TimedOut` if the deadline has
    /// passed.
    pub(crate) fn next_timeout(&self) -> IoResult<Duration> {
        let s = *self.state.lock().map_err(|_| IoError::other("timer"))?;
        match s.deadline {
            None => Ok(s.inactivity),
            Some(d) => {
                let left = d.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    Err(IoError::new(ErrorKind::TimedOut, "request head too slow"))
                } else {
                    // A zero socket timeout means "block forever", so never go below 1 ms.
                    Ok(s.inactivity.min(left).max(Duration::from_millis(1)))
                }
            }
        }
    }
}
