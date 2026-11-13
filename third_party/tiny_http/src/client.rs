use ascii::AsciiString;

use std::io::Error as IoError;
use std::io::Result as IoResult;
use std::io::{BufReader, BufWriter, ErrorKind, Read};

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::common::{HTTPVersion, Method};
use crate::limits::{Limits, ReadTimer};
use crate::util::RefinedTcpStream;
use crate::util::{SequentialReader, SequentialReaderBuilder, SequentialWriterBuilder};
use crate::Request;

/// A ClientConnection is an object that will store a socket to a client
/// and return Request objects.
pub struct ClientConnection {
    // address of the client
    remote_addr: IoResult<Option<SocketAddr>>,

    // sequence of Readers to the stream, so that the data is not read in
    //  the wrong order
    source: SequentialReaderBuilder<BufReader<RefinedTcpStream>>,

    // sequence of Writers to the stream, to avoid writing response #2 before
    //  response #1
    sink: SequentialWriterBuilder<BufWriter<RefinedTcpStream>>,

    // Reader to read the next header from
    next_header_source: SequentialReader<BufReader<RefinedTcpStream>>,

    // set to true if we know that the previous request is the last one
    no_more_requests: bool,

    // true if the connection goes through SSL
    secure: bool,

    // PLIPPA PATCH: size limits and the read-timeout policy of this connection
    limits: Limits,
    timer: Arc<ReadTimer>,
    // true once the first byte of the request being read has arrived (starts the head deadline)
    head_started: bool,
    // raised by a request body reader dropped with bytes unread: the connection must close
    leftover: Arc<AtomicBool>,
    /// PLIPPA PATCH (SECFIX_03): no request has been read on this connection yet, so the head
    /// deadline (not the keep-alive idle) governs the wait for its first byte — including a TLS
    /// handshake that never finishes.
    fresh: bool,
}

/// Error that can happen when reading a request.
#[derive(Debug)]
enum ReadError {
    WrongRequestLine,
    WrongHeader(HTTPVersion),
    /// PLIPPA PATCH: request line over the cap (`414`)
    RequestLineTooLong,
    /// PLIPPA PATCH: a header line, the header count or the whole head over its cap (`431`)
    HeadTooLarge(HTTPVersion),
    /// PLIPPA PATCH: a keep-alive connection sent nothing within the idle timeout (closed
    /// silently, there is no request to answer)
    Idle,
    /// PLIPPA PATCH (SRV-02): the request framing cannot be trusted (`400`, then close)
    MalformedFraming(HTTPVersion),
    /// the client sent an unrecognized `Expect` header
    ExpectationFailed(HTTPVersion),
    ReadIoError(IoError),
}

/// PLIPPA PATCH: why a head line could not be read.
enum LineError {
    TooLong,
    Io(IoError),
}

impl ClientConnection {
    /// Creates a new `ClientConnection` that takes ownership of the `TcpStream`.
    pub(crate) fn new(
        write_socket: RefinedTcpStream,
        mut read_socket: RefinedTcpStream,
        limits: Limits,
        timer: Arc<ReadTimer>,
    ) -> ClientConnection {
        let remote_addr = read_socket.peer_addr();
        let secure = read_socket.secure();

        let mut source = SequentialReaderBuilder::new(BufReader::with_capacity(1024, read_socket));
        let first_header = source.next().unwrap();

        ClientConnection {
            source,
            sink: SequentialWriterBuilder::new(BufWriter::with_capacity(1024, write_socket)),
            remote_addr,
            next_header_source: first_header,
            no_more_requests: false,
            secure,
            limits,
            timer,
            head_started: false,
            leftover: Arc::new(AtomicBool::new(false)),
            fresh: true,
        }
    }

    /// Reads the next line from self.next_header_source.
    ///
    /// Reads until `CRLF` is reached. The next read will start
    ///  at the first byte of the new line.
    ///
    /// PLIPPA PATCH: stops accumulating at `cap` bytes (the line is refused, not buffered) and
    /// starts the head deadline when the first byte of a request arrives.
    fn read_next_line(&mut self, cap: usize) -> Result<AsciiString, LineError> {
        let mut buf = Vec::new();
        let mut prev_byte_was_cr = false;

        loop {
            let byte = self.next_header_source.by_ref().bytes().next();

            let byte = match byte {
                Some(b) => b.map_err(LineError::Io)?,
                None => {
                    return Err(LineError::Io(IoError::new(
                        ErrorKind::ConnectionAborted,
                        "Unexpected EOF",
                    )))
                }
            };

            if !self.head_started {
                self.head_started = true;
                self.timer.arm(
                    self.limits.read_inactivity,
                    Some(Instant::now() + self.limits.head_deadline),
                );
            }

            if byte == b'\n' && prev_byte_was_cr {
                buf.pop(); // removing the '\r'
                return AsciiString::from_ascii(buf).map_err(|_| {
                    LineError::Io(IoError::new(
                        ErrorKind::InvalidInput,
                        "Header is not in ASCII",
                    ))
                });
            }

            prev_byte_was_cr = byte == b'\r';

            buf.push(byte);
            // `cap` counts the line without its CRLF; one more byte may be the '\r' still to come.
            if buf.len() > cap + 1 {
                return Err(LineError::TooLong);
            }
        }
    }

    /// PLIPPA PATCH: answer an oversized head (`414` / `431`) and close.
    ///
    /// The answer is flushed first, then a bounded amount of what the client is still sending is
    /// swallowed (64 KiB, at most half a second) so that closing a socket with unread data does
    /// not turn into a reset that destroys the answer before the client has read it. Everything
    /// beyond that is never read, let alone buffered.
    fn refuse_head(&mut self, status: u16, version: HTTPVersion) {
        use crate::{Header, Response, StatusCode};
        use std::io::Write;

        let mut writer = self.sink.next().unwrap();
        let response = Response::new_empty(StatusCode(status)).with_header(
            Header::from_bytes(&b"Connection"[..], &b"close"[..]).expect("static header"),
        );
        if response
            .raw_print(&mut writer, version, &[], false, None)
            .is_err()
            || writer.flush().is_err()
        {
            return;
        }
        self.timer.arm(
            Duration::from_millis(200),
            Some(Instant::now() + Duration::from_millis(500)),
        );
        let mut scratch = [0u8; 4096];
        let mut left = 64 * 1024usize;
        while left > 0 {
            match self.next_header_source.read(&mut scratch) {
                Ok(0) | Err(_) => break,
                Ok(n) => left = left.saturating_sub(n),
            }
        }
    }

    /// Reads a request from the stream.
    /// Blocks until the header has been read.
    fn read(&mut self) -> Result<Request, ReadError> {
        // PLIPPA PATCH: the previous request's body belongs to the previous request's reader
        // until it is dropped, so wait for our turn *before* arming the idle timeout (the
        // socket is not ours to read until then), then wait for the first byte.
        self.next_header_source.wait_turn();
        // PLIPPA PATCH: the previous request's body was not consumed (refused early, drain budget
        // spent, timed out, chunked): what follows on the wire is body, not a request. Close.
        if self.leftover.load(Ordering::SeqCst) {
            return Err(ReadError::Idle);
        }
        self.head_started = false;
        if self.fresh {
            // PLIPPA PATCH (SECFIX_03): the first request on a new connection is bounded by the
            // head deadline from its first moment, so a TLS handshake that stalls (or a client
            // that connects and then says nothing) does not sit in a worker thread for the
            // whole keep-alive idle timeout.
            self.fresh = false;
            // The head deadline (not the shorter inactivity window) bounds the wait for the
            // first byte: the inactivity window is for chatter that has begun, and a browser
            // may open a connection and stay quiet for a moment before its request.
            self.timer.arm(
                self.limits.head_deadline,
                Some(Instant::now() + self.limits.head_deadline),
            );
        } else {
            self.timer.arm(self.limits.keep_alive_idle, None);
        }

        // bytes of the head read so far, counting each line's CRLF
        let mut head_bytes;

        let (method, path, version, headers) = {
            // reading the request line
            let (method, path, version) = {
                let line = match self.read_next_line(self.limits.max_request_line) {
                    Ok(l) => l,
                    Err(LineError::TooLong) => return Err(ReadError::RequestLineTooLong),
                    Err(LineError::Io(e))
                        if !self.head_started && e.kind() == ErrorKind::TimedOut =>
                    {
                        return Err(ReadError::Idle)
                    }
                    Err(LineError::Io(e)) => return Err(ReadError::ReadIoError(e)),
                };
                head_bytes = line.len() + 2;

                parse_request_line(
                    line.as_str().trim(), // TODO: remove this conversion
                )?
            };

            // getting all headers
            let headers = {
                let mut headers = Vec::new();
                loop {
                    let line = match self.read_next_line(self.limits.max_header_line) {
                        Ok(l) => l,
                        Err(LineError::TooLong) => {
                            return Err(ReadError::HeadTooLarge(version));
                        }
                        Err(LineError::Io(e)) => return Err(ReadError::ReadIoError(e)),
                    };
                    head_bytes += line.len() + 2;
                    if head_bytes > self.limits.max_head_bytes {
                        return Err(ReadError::HeadTooLarge(version));
                    }

                    if line.is_empty() {
                        break;
                    };
                    if headers.len() >= self.limits.max_headers {
                        return Err(ReadError::HeadTooLarge(version));
                    }
                    headers.push(match FromStr::from_str(line.as_str().trim()) {
                        // TODO: remove this conversion
                        Ok(h) => h,
                        _ => return Err(ReadError::WrongHeader(version)),
                    });
                }

                headers
            };

            (method, path, version, headers)
        };

        // building the writer for the request
        let writer = self.sink.next().unwrap();

        // follow-up for next potential request
        let mut data_source = self.source.next().unwrap();
        std::mem::swap(&mut self.next_header_source, &mut data_source);

        // building the next reader
        let request = crate::request::new_request(
            self.secure,
            method,
            path,
            version.clone(),
            headers,
            *self.remote_addr.as_ref().unwrap(),
            data_source,
            writer,
            Some(Arc::clone(&self.leftover)),
        )
        .map_err(|e| {
            use crate::request;
            match e {
                request::RequestCreationError::CreationIoError(e) => ReadError::ReadIoError(e),
                request::RequestCreationError::ExpectationFailed => {
                    ReadError::ExpectationFailed(version)
                }
                request::RequestCreationError::MalformedFraming => {
                    ReadError::MalformedFraming(version)
                }
            }
        })?;

        // PLIPPA PATCH: from here on the connection is waiting for the request body (if any), and
        // that is read by the handler: only silence is limited, never the total time.
        self.timer.arm(self.limits.body_stall, None);

        // return the request
        Ok(request)
    }
}

impl Iterator for ClientConnection {
    type Item = Request;

    /// Blocks until the next Request is available.
    /// Returns None when no new Requests will come from the client.
    fn next(&mut self) -> Option<Request> {
        use crate::{Response, StatusCode};

        // the client sent a "connection: close" header in this previous request
        //  or is using HTTP 1.0, meaning that no new request will come
        if self.no_more_requests {
            return None;
        }

        loop {
            let rq = match self.read() {
                Err(ReadError::WrongRequestLine) => {
                    let writer = self.sink.next().unwrap();
                    let response = Response::new_empty(StatusCode(400));
                    response
                        .raw_print(writer, HTTPVersion(1, 1), &[], false, None)
                        .ok();
                    return None; // we don't know where the next request would start,
                                 // se we have to close
                }

                Err(ReadError::WrongHeader(ver)) => {
                    let writer = self.sink.next().unwrap();
                    let response = Response::new_empty(StatusCode(400));
                    response.raw_print(writer, ver, &[], false, None).ok();
                    return None; // we don't know where the next request would start,
                                 // se we have to close
                }

                // PLIPPA PATCH (SRV-02): refused before the offending bytes are buffered.
                Err(ReadError::RequestLineTooLong) => {
                    self.refuse_head(414, HTTPVersion(1, 1));
                    return None;
                }

                Err(ReadError::HeadTooLarge(ver)) => {
                    self.refuse_head(431, ver);
                    return None;
                }

                // PLIPPA PATCH (SRV-02): a `Content-Length` that is not a length, any second
                // `Content-Length` at all (agreeing ones included), or an encoding other than
                // exactly `chunked`. Answering `400` and closing is the only safe reading: the
                // bytes after it are not a request.
                Err(ReadError::MalformedFraming(ver)) => {
                    let mut writer = self.sink.next().unwrap();
                    let response = crate::Response::new_empty(StatusCode(400)).with_header(
                        crate::Header::from_bytes(&b"Connection"[..], &b"close"[..])
                            .expect("static header"),
                    );
                    response.raw_print(&mut writer, ver, &[], false, None).ok();
                    std::io::Write::flush(&mut writer).ok();
                    return None;
                }

                // a keep-alive connection that sent nothing: nothing to answer, just close
                Err(ReadError::Idle) => return None,

                Err(ReadError::ReadIoError(ref err)) if err.kind() == ErrorKind::TimedOut => {
                    // request timeout
                    let writer = self.sink.next().unwrap();
                    let response = Response::new_empty(StatusCode(408));
                    response
                        .raw_print(writer, HTTPVersion(1, 1), &[], false, None)
                        .ok();
                    return None; // closing the connection
                }

                Err(ReadError::ExpectationFailed(ver)) => {
                    let writer = self.sink.next().unwrap();
                    let response = Response::new_empty(StatusCode(417));
                    response.raw_print(writer, ver, &[], true, None).ok();
                    return None; // TODO: should be recoverable, but needs handling in case of body
                }

                Err(ReadError::ReadIoError(_)) => return None,

                Ok(rq) => rq,
            };

            // checking HTTP version
            if *rq.http_version() > (1, 1) {
                let writer = self.sink.next().unwrap();
                let response = Response::from_string(
                    "This server only supports HTTP versions 1.0 and 1.1".to_owned(),
                )
                .with_status_code(StatusCode(505));
                response
                    .raw_print(writer, HTTPVersion(1, 1), &[], false, None)
                    .ok();
                continue;
            }

            // updating the status of the connection
            let connection_header = rq
                .headers()
                .iter()
                .find(|h| h.field.equiv("Connection"))
                .map(|h| h.value.as_str());

            let lowercase = connection_header.map(|h| h.to_ascii_lowercase());

            match lowercase {
                Some(ref val) if val.contains("close") => self.no_more_requests = true,
                Some(ref val) if val.contains("upgrade") => self.no_more_requests = true,
                Some(ref val)
                    if !val.contains("keep-alive") && *rq.http_version() == HTTPVersion(1, 0) =>
                {
                    self.no_more_requests = true
                }
                None if *rq.http_version() == HTTPVersion(1, 0) => self.no_more_requests = true,
                _ => (),
            };

            // returning the request
            return Some(rq);
        }
    }
}

/// Parses a "HTTP/1.1" string.
fn parse_http_version(version: &str) -> Result<HTTPVersion, ReadError> {
    let (major, minor) = match version {
        "HTTP/0.9" => (0, 9),
        "HTTP/1.0" => (1, 0),
        "HTTP/1.1" => (1, 1),
        "HTTP/2.0" => (2, 0),
        "HTTP/3.0" => (3, 0),
        _ => return Err(ReadError::WrongRequestLine),
    };

    Ok(HTTPVersion(major, minor))
}

/// Parses the request line of the request.
/// eg. GET / HTTP/1.1
fn parse_request_line(line: &str) -> Result<(Method, String, HTTPVersion), ReadError> {
    let mut parts = line.split(' ');

    let method = parts.next().and_then(|w| w.parse().ok());
    let path = parts.next().map(ToOwned::to_owned);
    let version = parts.next().and_then(|w| parse_http_version(w).ok());

    method
        .and_then(|method| Some((method, path?, version?)))
        .ok_or(ReadError::WrongRequestLine)
}

#[cfg(test)]
mod test {
    #[test]
    fn test_parse_request_line() {
        let (method, path, ver) = super::parse_request_line("GET /hello HTTP/1.1").unwrap();

        assert!(method == crate::Method::Get);
        assert!(path == "/hello");
        assert!(ver == crate::common::HTTPVersion(1, 1));

        assert!(super::parse_request_line("GET /hello").is_err());
        assert!(super::parse_request_line("qsd qsd qsd").is_err());
    }
}
