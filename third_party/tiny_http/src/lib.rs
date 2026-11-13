//! # Simple usage
//!
//! ## Creating the server
//!
//! The easiest way to create a server is to call `Server::http()`.
//!
//! The `http()` function returns an `IoResult<Server>` which will return an error
//! in the case where the server creation fails (for example if the listening port is already
//! occupied).
//!
//! ```no_run
//! let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
//! ```
//!
//! A newly-created `Server` will immediately start listening for incoming connections and HTTP
//! requests.
//!
//! ## Receiving requests
//!
//! Calling `server.recv()` will block until the next request is available.
//! This function returns an `IoResult<Request>`, so you need to handle the possible errors.
//!
//! ```no_run
//! # let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
//!
//! loop {
//!     // blocks until the next request is received
//!     let request = match server.recv() {
//!         Ok(rq) => rq,
//!         Err(e) => { println!("error: {}", e); break }
//!     };
//!
//!     // do something with the request
//!     // ...
//! }
//! ```
//!
//! In a real-case scenario, you will probably want to spawn multiple worker tasks and call
//! `server.recv()` on all of them. Like this:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use std::thread;
//! # let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
//! let server = Arc::new(server);
//! let mut guards = Vec::with_capacity(4);
//!
//! for _ in (0 .. 4) {
//!     let server = server.clone();
//!
//!     let guard = thread::spawn(move || {
//!         loop {
//!             let rq = server.recv().unwrap();
//!
//!             // ...
//!         }
//!     });
//!
//!     guards.push(guard);
//! }
//! ```
//!
//! If you don't want to block, you can call `server.try_recv()` instead.
//!
//! ## Handling requests
//!
//! The `Request` object returned by `server.recv()` contains informations about the client's request.
//! The most useful methods are probably `request.method()` and `request.url()` which return
//! the requested method (`GET`, `POST`, etc.) and url.
//!
//! To handle a request, you need to create a `Response` object. See the docs of this object for
//! more infos. Here is an example of creating a `Response` from a file:
//!
//! ```no_run
//! # use std::fs::File;
//! # use std::path::Path;
//! let response = tiny_http::Response::from_file(File::open(&Path::new("image.png")).unwrap());
//! ```
//!
//! All that remains to do is call `request.respond()`:
//!
//! ```no_run
//! # use std::fs::File;
//! # use std::path::Path;
//! # let server = tiny_http::Server::http("0.0.0.0:0").unwrap();
//! # let request = server.recv().unwrap();
//! # let response = tiny_http::Response::from_file(File::open(&Path::new("image.png")).unwrap());
//! let _ = request.respond(response);
//! ```
#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]
#![allow(clippy::match_like_matches_macro)]

#[cfg(any(feature = "ssl-openssl", feature = "ssl-rustls"))]
use zeroize::Zeroizing;

use std::error::Error;
use std::io::Error as IoError;
use std::io::ErrorKind as IoErrorKind;
use std::io::Result as IoResult;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use client::ClientConnection;
use connection::Connection;
use util::MessagesQueue;

pub use common::{HTTPVersion, Header, HeaderField, Method, StatusCode};
pub use connection::{ConfigListenAddr, ListenAddr, Listener};
pub use limits::{ConnectionGate, ConnectionHandle, Limits, Refused, ServerOptions};
pub use request::{ReadWrite, Request};
pub use response::{Response, ResponseBox};
pub use test::TestRequest;

mod client;
mod common;
mod connection;
mod limits;
mod request;
mod response;
mod ssl;
mod test;
mod util;

/// The main class of this library.
///
/// Destroying this object will immediately close the listening socket and the reading
///  part of all the client's connections. Requests that have already been returned by
///  the `recv()` function will not close and the responses will be transferred to the client.
pub struct Server {
    // should be false as long as the server exists
    // when set to true, all the subtasks will close within a few hundreds ms
    close: Arc<AtomicBool>,

    // queue for messages received by child threads
    messages: Arc<MessagesQueue<Message>>,

    // result of TcpListener::local_addr()
    listening_addr: ListenAddr,
}

enum Message {
    Error(IoError),
    NewRequest(Request),
}

impl From<IoError> for Message {
    fn from(e: IoError) -> Message {
        Message::Error(e)
    }
}

impl From<Request> for Message {
    fn from(rq: Request) -> Message {
        Message::NewRequest(rq)
    }
}

// this trait is to make sure that Server implements Share and Send
#[doc(hidden)]
trait MustBeShareDummy: Sync + Send {}
#[doc(hidden)]
impl MustBeShareDummy for Server {}

pub struct IncomingRequests<'a> {
    server: &'a Server,
}

/// Represents the parameters required to create a server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The addresses to try to listen to.
    pub addr: ConfigListenAddr,

    /// If `Some`, then the server will use SSL to encode the communications.
    pub ssl: Option<SslConfig>,
}

/// Configuration of the server for SSL.
#[derive(Debug, Clone)]
pub struct SslConfig {
    /// Contains the DER-encoded X.509 certificate to send to clients (PLIPPA PATCH: DER, not
    /// PEM; the certificate is generated in-process).
    pub certificate: Vec<u8>,
    /// Contains the ultra-secret DER-encoded PKCS#8 private key used to decode communications.
    pub private_key: Vec<u8>,
    /// PLIPPA PATCH (SECFIX_03): accept a plaintext connection on this TLS port as well, and
    /// report it as [`Request::secure`] `false` so the application can answer with a redirect.
    ///
    /// A single port cannot tell a client "you must use HTTPS" unless it is willing to read the
    /// first byte of a connection first; with this set, the first record decides: a TLS
    /// handshake record (content type `0x16`) goes through rustls, anything else is served as
    /// plain HTTP. When `false` (TLS only) a plaintext client simply fails the handshake.
    pub accept_plaintext: bool,
}

impl Server {
    /// Shortcut for a simple server on a specific address.
    #[inline]
    pub fn http<A>(addr: A) -> Result<Server, Box<dyn Error + Send + Sync + 'static>>
    where
        A: ToSocketAddrs,
    {
        Server::new(ServerConfig {
            addr: ConfigListenAddr::from_socket_addrs(addr)?,
            ssl: None,
        })
    }

    /// Shortcut for an HTTPS server on a specific address.
    #[cfg(any(feature = "ssl-openssl", feature = "ssl-rustls"))]
    #[inline]
    pub fn https<A>(
        addr: A,
        config: SslConfig,
    ) -> Result<Server, Box<dyn Error + Send + Sync + 'static>>
    where
        A: ToSocketAddrs,
    {
        Server::new(ServerConfig {
            addr: ConfigListenAddr::from_socket_addrs(addr)?,
            ssl: Some(config),
        })
    }

    #[cfg(unix)]
    #[inline]
    /// Shortcut for a UNIX socket server at a specific path
    pub fn http_unix(
        path: &std::path::Path,
    ) -> Result<Server, Box<dyn Error + Send + Sync + 'static>> {
        Server::new(ServerConfig {
            addr: ConfigListenAddr::unix_from_path(path),
            ssl: None,
        })
    }

    /// Builds a new server that listens on the specified address.
    pub fn new(config: ServerConfig) -> Result<Server, Box<dyn Error + Send + Sync + 'static>> {
        Self::with_options(config, ServerOptions::default())
    }

    /// PLIPPA PATCH: [`Server::new`] with explicit [`Limits`] and an optional
    /// [`ConnectionGate`].
    pub fn with_options(
        config: ServerConfig,
        options: ServerOptions,
    ) -> Result<Server, Box<dyn Error + Send + Sync + 'static>> {
        let listener = config.addr.bind()?;
        Self::from_listener_with_options(listener, config.ssl, options)
    }

    /// Builds a new server using the specified TCP listener.
    ///
    /// This is useful if you've constructed TcpListener using some less usual method
    /// such as from systemd. For other cases, you probably want the `new()` function.
    pub fn from_listener<L: Into<Listener>>(
        listener: L,
        ssl_config: Option<SslConfig>,
    ) -> Result<Server, Box<dyn Error + Send + Sync + 'static>> {
        Self::from_listener_with_options(listener, ssl_config, ServerOptions::default())
    }

    /// PLIPPA PATCH: [`Server::from_listener`] with explicit [`Limits`] and an optional
    /// [`ConnectionGate`].
    pub fn from_listener_with_options<L: Into<Listener>>(
        listener: L,
        ssl_config: Option<SslConfig>,
        options: ServerOptions,
    ) -> Result<Server, Box<dyn Error + Send + Sync + 'static>> {
        let ServerOptions { limits, gate } = options;
        let listener = listener.into();
        // building the "close" variable
        let close_trigger = Arc::new(AtomicBool::new(false));

        // building the TcpListener
        let (server, local_addr) = {
            let local_addr = listener.local_addr()?;
            log::debug!("Server listening on {}", local_addr);
            (listener, local_addr)
        };

        // building the SSL capabilities
        #[cfg(all(feature = "ssl-openssl", feature = "ssl-rustls"))]
        compile_error!(
            "Features 'ssl-openssl' and 'ssl-rustls' must not be enabled at the same time"
        );
        #[cfg(not(any(feature = "ssl-openssl", feature = "ssl-rustls")))]
        type SslContext = ();
        #[cfg(any(feature = "ssl-openssl", feature = "ssl-rustls"))]
        type SslContext = crate::ssl::SslContextImpl;
        // PLIPPA PATCH: with a TLS context the port may also accept plaintext, so that a
        // browser that typed `http://` is redirected instead of failing a handshake.
        let accept_plaintext = ssl_config
            .as_ref()
            .map(|c| c.accept_plaintext)
            .unwrap_or(false);
        let ssl: Option<SslContext> = {
            match ssl_config {
                #[cfg(any(feature = "ssl-openssl", feature = "ssl-rustls"))]
                Some(config) => Some(SslContext::from_der(
                    config.certificate,
                    Zeroizing::new(config.private_key),
                )?),
                #[cfg(not(any(feature = "ssl-openssl", feature = "ssl-rustls")))]
                Some(_) => return Err(
                    "Building a server with SSL requires enabling the `ssl` feature in tiny-http"
                        .into(),
                ),
                None => None,
            }
        };
        // PLIPPA PATCH (SECFIX_03): shared with the worker threads, which do the protocol sniff
        // and build the TLS wrapper themselves (see the accept loop).
        let ssl = Arc::new(ssl);

        // creating a task where server.accept() is continuously called
        // and ClientConnection objects are pushed in the messages queue
        let messages = MessagesQueue::with_capacity(8);

        let inside_close_trigger = close_trigger.clone();
        let inside_messages = messages.clone();
        thread::spawn(move || {
            // a tasks pool is used to dispatch the connections into threads
            // PLIPPA PATCH: with a ceiling; a connection that finds every worker busy is refused.
            let tasks_pool = util::TaskPool::new(limits.max_connection_threads);

            log::debug!("Running accept thread");
            loop {
                let accepted = accept_until_ready(
                    || {
                        if inside_close_trigger.load(Relaxed) {
                            None
                        } else {
                            Some(server.accept())
                        }
                    },
                    |e| {
                        // PLIPPA PATCH: running out of file descriptors or memory (or a client
                        // that gave up before we accepted it) is transient: log, back off,
                        // keep serving. Upstream ended the accept thread on any error, so one
                        // burst of connections could take the portal down for good.
                        log::warn!("transient error accepting a client: {}", e);
                        thread::sleep(ACCEPT_BACKOFF);
                    },
                    |e| {
                        log::error!("Error accepting new client: {}", e);
                        inside_messages.push(e.into());
                    },
                );
                let Some((sock, peer)) = accepted else { break };


                // PLIPPA PATCH: bound every socket before anything is read from it (this also
                // bounds a TLS handshake, which runs in the worker thread).
                let _ = sock.set_write_timeout(Some(limits.write_timeout));
                let _ = sock.set_read_timeout(Some(limits.keep_alive_idle));

                // PLIPPA PATCH: connection admission. Nothing is read, queued or spawned for a
                // refused connection.
                let guard: Option<Box<dyn Send>> = match gate.as_ref() {
                    None => None,
                    Some(gate) => {
                        let admitted = ConnectionHandle::new(&sock)
                            .map_err(|_| Refused)
                            .and_then(|handle| gate.admit(peer, handle));
                        match admitted {
                            Ok(g) => Some(g),
                            Err(Refused) => {
                                refuse_busy(sock);
                                continue;
                            }
                        }
                    }
                };

                // PLIPPA PATCH (SECFIX_03): the protocol sniff and the TLS wrapper are the
                // worker's job, not this loop's. Doing the peek here would let a client that
                // connects and says nothing hold this loop for `sniff_timeout` (a scanner
                // looping "connect, close" would throttle every other client), which is exactly
                // what the one-port redirect design must not cost.
                let messages = inside_messages.clone();
                let ssl = Arc::clone(&ssl);
                let worker_limits = limits.clone();
                let mut pending = Some((sock, guard));
                let spawned = tasks_pool.spawn(Box::new(move || {
                    let Some((sock, guard)) = pending.take() else {
                        return;
                    };
                    let timer = limits::ReadTimer::new(worker_limits.keep_alive_idle);
                    let new_client = {
                        use util::RefinedTcpStream;
                        let (read_closable, write_closable) = match ssl.as_ref() {
                            None => RefinedTcpStream::new(sock, Some(timer.clone())),
                            #[cfg(any(feature = "ssl-openssl", feature = "ssl-rustls"))]
                            Some(ssl) => {
                                // One port, two protocols. When plaintext is accepted, the
                                // first byte decides; a TLS client always speaks first with a
                                // handshake record (content type 0x16) and every HTTP method
                                // starts with an ASCII letter. Nothing is consumed. The peek is
                                // bounded by `sniff_timeout`; when it expires the socket is
                                // closed, and because this is a worker that costs no other
                                // connection anything.
                                let use_tls = match ssl_kind(
                                    &sock,
                                    accept_plaintext,
                                    worker_limits.sniff_timeout,
                                ) {
                                    SslKind::Plain => false,
                                    SslKind::Tls => true,
                                    // Nothing arrived: there is no request to answer.
                                    SslKind::Closed => return,
                                };
                                if !use_tls {
                                    RefinedTcpStream::new(sock, Some(timer.clone()))
                                } else {
                                    // if the handshake cannot even be set up, close and resume
                                    match ssl.accept(sock) {
                                        Ok(s) => RefinedTcpStream::new(s, Some(timer.clone())),
                                        Err(_) => return,
                                    }
                                }
                            }
                            #[cfg(not(any(feature = "ssl-openssl", feature = "ssl-rustls")))]
                            Some(_) => unreachable!(),
                        };

                        ClientConnection::new(
                            write_closable,
                            read_closable,
                            worker_limits.clone(),
                            timer,
                        )
                    };

                    let mut client = Some((new_client, guard));
                    if let Some((client, _guard)) = client.take() {
                        // PLIPPA PATCH: one request at a time per connection, for plain HTTP
                        // as well as HTTPS. The next head is not read until the previous request
                        // has been answered, so a client cannot queue requests without bound
                        // by pipelining (and for HTTPS this was already required to avoid a
                        // deadlock).
                        let (sender, receiver) = mpsc::channel();
                        for rq in client {
                            messages.push(rq.with_notify_sender(sender.clone()).into());
                            if receiver.recv().is_err() {
                                break;
                            }
                        }
                        // `_guard` (the admission) is released here, when the connection ends
                    }
                }));
                if spawned.is_err() {
                    // every worker thread is busy: the connection (and its guard) is dropped
                    log::warn!("connection worker ceiling reached, refusing a connection");
                }
            }
            log::debug!("Terminating accept thread");
        });

        // result
        Ok(Server {
            messages,
            close: close_trigger,
            listening_addr: local_addr,
        })
    }

    /// Returns an iterator for all the incoming requests.
    ///
    /// The iterator will return `None` if the server socket is shutdown.
    #[inline]
    pub fn incoming_requests(&self) -> IncomingRequests<'_> {
        IncomingRequests { server: self }
    }

    /// Returns the address the server is listening to.
    #[inline]
    pub fn server_addr(&self) -> ListenAddr {
        self.listening_addr.clone()
    }

    /// Returns the number of clients currently connected to the server.
    pub fn num_connections(&self) -> usize {
        unimplemented!()
        //self.requests_receiver.lock().len()
    }

    /// Blocks until an HTTP request has been submitted and returns it.
    pub fn recv(&self) -> IoResult<Request> {
        match self.messages.pop() {
            Some(Message::Error(err)) => Err(err),
            Some(Message::NewRequest(rq)) => Ok(rq),
            None => Err(IoError::new(IoErrorKind::Other, "thread unblocked")),
        }
    }

    /// Same as `recv()` but doesn't block longer than timeout
    pub fn recv_timeout(&self, timeout: Duration) -> IoResult<Option<Request>> {
        match self.messages.pop_timeout(timeout) {
            Some(Message::Error(err)) => Err(err),
            Some(Message::NewRequest(rq)) => Ok(Some(rq)),
            None => Ok(None),
        }
    }

    /// Same as `recv()` but doesn't block.
    pub fn try_recv(&self) -> IoResult<Option<Request>> {
        match self.messages.try_pop() {
            Some(Message::Error(err)) => Err(err),
            Some(Message::NewRequest(rq)) => Ok(Some(rq)),
            None => Ok(None),
        }
    }

    /// Unblock thread stuck in recv() or incoming_requests().
    /// If there are several such threads, only one is unblocked.
    /// This method allows graceful shutdown of server.
    pub fn unblock(&self) {
        self.messages.unblock();
    }
}

/// PLIPPA PATCH (SECFIX_03): what a fresh connection turned out to be on a TLS port that also
/// answers plaintext.
#[cfg(any(feature = "ssl-openssl", feature = "ssl-rustls"))]
enum SslKind {
    /// Speak TLS on this socket.
    Tls,
    /// Serve it as plain HTTP (the application answers with the redirect).
    Plain,
    /// The client sent nothing (or the socket failed): close it.
    Closed,
}

/// PLIPPA PATCH (SECFIX_03): decide from the first byte whether a connection is TLS.
///
/// Every TLS client starts with a handshake record, whose content type is `0x16`; every HTTP
/// method name starts with an ASCII letter. One `peek` therefore decides, and nothing is
/// consumed from the socket. The peek is bounded: a client that connects and says nothing is
/// closed rather than held.
#[cfg(any(feature = "ssl-openssl", feature = "ssl-rustls"))]
fn ssl_kind(
    sock: &connection::Connection,
    accept_plaintext: bool,
    timeout: Duration,
) -> SslKind {
    if !accept_plaintext {
        return SslKind::Tls;
    }
    let mut first = [0u8; 1];
    match sock.peek(&mut first, timeout) {
        Ok(1) if first[0] == 0x16 => SslKind::Tls,
        Ok(n) if n > 0 => SslKind::Plain,
        Ok(_) => SslKind::Closed,
        Err(e)
            if matches!(
                e.kind(),
                IoErrorKind::WouldBlock | IoErrorKind::TimedOut | IoErrorKind::UnexpectedEof
            ) =>
        {
            SslKind::Closed
        }
        // A UNIX socket (or a socket that cannot be peeked): there is nothing to negotiate.
        Err(_) => SslKind::Plain,
    }
}

/// PLIPPA PATCH: how long the accept loop waits after a transient `accept()` error, so a burst
/// of connections that exhausts the file descriptor table cannot spin the loop at 100 % CPU.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// PLIPPA PATCH: one `accept()` step of the accept loop, split out so its policy can be
/// exercised without exhausting the process's file descriptors (SECFIX_03).
///
/// `accept` returns `None` once the server is closing, otherwise the listener's result.
/// Transient errors are reported to `retry` (which backs off) and the loop asks again; a fatal
/// error is reported to `fatal` and ends the loop. `None` means "stop the thread".
fn accept_until_ready<S>(
    mut accept: impl FnMut() -> Option<Result<S, IoError>>,
    mut retry: impl FnMut(&IoError),
    mut fatal: impl FnMut(IoError),
) -> Option<S> {
    loop {
        match accept()? {
            Ok(value) => return Some(value),
            Err(e) if is_transient_accept_error(&e) => retry(&e),
            Err(e) => {
                fatal(e);
                return None;
            }
        }
    }
}

/// PLIPPA PATCH: `accept` errors that say "not now", not "the listener is broken".
pub fn is_transient_accept_error(e: &IoError) -> bool {
    if matches!(
        e.kind(),
        IoErrorKind::Interrupted
            | IoErrorKind::ConnectionAborted
            | IoErrorKind::ConnectionReset
            | IoErrorKind::TimedOut
            | IoErrorKind::WouldBlock
    ) {
        return true;
    }
    // EMFILE 24, ENFILE 23, ENOMEM 12, ENOBUFS 105 (Linux values; no libc dependency here)
    matches!(e.raw_os_error(), Some(24 | 23 | 12 | 105))
}

/// PLIPPA PATCH: answer a connection the gate refused. The socket is fresh, so this small write
/// goes straight into the kernel buffer; the write timeout is short anyway.
fn refuse_busy(sock: connection::Connection) {
    use std::io::Write;
    let mut sock = sock;
    let _ = sock.set_write_timeout(Some(Duration::from_millis(200)));
    let _ = sock.write_all(
        b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 5\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    );
    let _ = sock.shutdown(Shutdown::Both);
}

impl Iterator for IncomingRequests<'_> {
    type Item = Request;
    fn next(&mut self) -> Option<Request> {
        self.server.recv().ok()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.close.store(true, Relaxed);
        // Connect briefly to ourselves to unblock the accept thread
        let maybe_stream = match &self.listening_addr {
            ListenAddr::IP(addr) => TcpStream::connect(addr).map(Connection::from),
            #[cfg(unix)]
            ListenAddr::Unix(addr) => {
                // TODO: use connect_addr when its stabilized.
                let path = addr.as_pathname().unwrap();
                std::os::unix::net::UnixStream::connect(path).map(Connection::from)
            }
        };
        if let Ok(stream) = maybe_stream {
            let _ = stream.shutdown(Shutdown::Both);
        }

        #[cfg(unix)]
        if let ListenAddr::Unix(addr) = &self.listening_addr {
            if let Some(path) = addr.as_pathname() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[cfg(test)]
mod accept_loop_tests {
    //! SECFIX_03: the accept loop's reaction to a failing `accept()`. The classifier was tested
    //! from `plippa-server`; this exercises the loop itself — that a transient error (running out
    //! of file descriptors, memory, a client that gave up) is followed by a back-off and another
    //! attempt, that a broken listener ends the loop, and that closing the server stops it
    //! without accepting anything.
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn emfile() -> IoError {
        IoError::from_raw_os_error(24)
    }
    fn enfile() -> IoError {
        IoError::from_raw_os_error(23)
    }

    /// Runs the loop against a scripted `accept()` and records what the loop did, in order.
    fn run(script: Vec<Result<&'static str, IoError>>) -> (Option<String>, Vec<String>) {
        let mut queue: VecDeque<Result<&'static str, IoError>> = script.into();
        let log = RefCell::new(Vec::<String>::new());
        let value = accept_until_ready(
            || {
                log.borrow_mut().push("accept".to_string());
                queue.pop_front()
            },
            |e| log.borrow_mut().push(format!("retry:{:?}", e.raw_os_error())),
            |e| log.borrow_mut().push(format!("fatal:{:?}", e.raw_os_error())),
        )
        .map(str::to_string);
        (value, log.into_inner())
    }

    #[test]
    fn out_of_file_descriptors_backs_off_and_the_loop_keeps_serving() {
        let (value, log) = run(vec![Err(emfile()), Err(enfile()), Ok("connection")]);
        assert_eq!(value.as_deref(), Some("connection"));
        assert_eq!(
            log,
            ["accept", "retry:Some(24)", "accept", "retry:Some(23)", "accept"],
            "each transient error is retried once, and the next connection is still served"
        );
        assert!(!log.iter().any(|l| l.starts_with("fatal")), "{:?}", log);
    }

    #[test]
    fn a_broken_listener_ends_the_loop_and_is_not_retried() {
        let (value, log) = run(vec![Err(IoError::from_raw_os_error(9)), Ok("never")]);
        assert_eq!(value, None);
        assert_eq!(log, ["accept", "fatal:Some(9)"], "no further accept after a fatal error");
    }

    #[test]
    fn a_fatal_error_after_transient_ones_still_ends_the_loop() {
        let (value, log) = run(vec![Err(emfile()), Err(IoError::from_raw_os_error(9))]);
        assert_eq!(value, None);
        assert_eq!(log, ["accept", "retry:Some(24)", "accept", "fatal:Some(9)"]);
    }

    #[test]
    fn closing_the_server_stops_the_loop_without_a_retry_or_a_fatal() {
        // `accept` returns `None` once the close trigger is set; the loop must just stop.
        let mut calls = 0;
        let value: Option<()> = accept_until_ready(
            || {
                calls += 1;
                None
            },
            |_| panic!("a closing server must not be retried"),
            |_| panic!("a closing server is not an error"),
        );
        assert_eq!(value, None);
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_client_that_aborted_is_transient_not_fatal() {
        // ECONNABORTED (103): the client gave up between the SYN and the accept.
        let (value, log) = run(vec![Err(IoError::from_raw_os_error(103)), Ok("connection")]);
        assert_eq!(value.as_deref(), Some("connection"));
        assert!(log.contains(&"retry:Some(103)".to_string()), "{:?}", log);
    }
}
