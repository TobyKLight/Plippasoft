# tiny-http

[![Crate][crate_img]][crate]
[![Documentation][docs_img]][docs]
![License][license_img]
[![CI Status][ci_badge]][ci_link]

[**Documentation**](https://docs.rs/tiny_http)

Tiny but strong HTTP server in Rust.
Its main objectives are to be 100% compliant with the HTTP standard and to provide an easy way to create an HTTP server.

What does **tiny-http** handle?
 - Accepting and managing connections to the clients
 - Parsing requests
 - Requests pipelining
 - HTTPS (using either OpenSSL or Rustls)
 - Transfer-Encoding and Content-Encoding
 - Turning user input (eg. POST input) into a contiguous UTF-8 string (**not implemented yet**)
 - Ranges (**not implemented yet**)
 - `Connection: upgrade` (used by websockets)

Tiny-http handles everything that is related to client connections and data transfers and encoding.

Everything else (parsing the values of the headers, multipart data, routing, etags, cache-control, HTML templates, etc.) must be handled by your code.
If you want to create a website in Rust, I strongly recommend using a framework instead of this library.

### Installation

Add this to the `Cargo.toml` file of your project:

```toml
[dependencies]
tiny_http = "0.11"
```

### Usage

```rust
use tiny_http::{Server, Response};

let server = Server::http("0.0.0.0:8000").unwrap();

for request in server.incoming_requests() {
    println!("received request! method: {:?}, url: {:?}, headers: {:?}",
        request.method(),
        request.url(),
        request.headers()
    );

    let response = Response::from_string("hello world");
    request.respond(response);
}
```

### Speed

Tiny-http was designed with speed in mind:
 - Each client connection will be dispatched to a thread pool. Each thread will handle one client.
 If there is no thread available when a client connects, a new one is created. Threads that are idle
 for a long time (currently 5 seconds) will automatically die.
 - (Plippa: no longer true, see "Plippa local patches" item 5. A connection is not read again
 until its current request has been answered; pipelined requests are still answered in order.)
 If multiple requests from the same client are being pipelined (ie. multiple requests
 are sent without waiting for the answer), tiny-http will read them all at once and they will
 all be available via `server.recv()`. Tiny-http will automatically rearrange the responses
 so that they are sent in the right order.
 - One exception to the previous statement exists when a request has a large body (currently > 1kB),
 in which case the request handler will read the body directly from the stream and tiny-http
 will wait for it to be read before processing the next request. Tiny-http will never wait for
 a request to be answered to read the next one.
 - When a client connection has sent its last request (by sending `Connection: close` header),
 the thread will immediately stop reading from this client and can be reclaimed, even when the
 request has not yet been answered. The reading part of the socket will also be immediately closed.
 - Decoding the client's request is done lazily. If you don't read the request's body, it will not
 be decoded.

### Examples

Examples of tiny-http in use:

* [heroku-tiny-http-hello-world](https://github.com/frewsxcv/heroku-tiny-http-hello-world) - A simple web application demonstrating how to deploy tiny-http to Heroku
* [crate-deps](https://github.com/frewsxcv/crate-deps) - A web service that generates images of dependency graphs for crates hosted on crates.io
* [rouille](https://crates.io/crates/rouille) - Web framework built on tiny-http

### License

This project is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or
   http://opensource.org/licenses/MIT)

at your option.

#### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in tiny-http by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

<!-- Links and Badges -->
[crate_img]: https://img.shields.io/crates/v/tiny_http.svg?logo=rust "Crate Page"
[crate]: https://crates.io/crates/tiny_http "Crate Link"
[docs]: https://docs.rs/tiny_http "Documentation"
[docs_img]: https://docs.rs/tiny_http/badge.svg "Documentation"
[license_img]: https://img.shields.io/crates/l/tiny_http.svg "License"
[ci_badge]: https://github.com/tiny-http/tiny-http/actions/workflows/ci.yaml/badge.svg "CI Status"
[ci_link]: https://github.com/tiny-http/tiny-http/actions/workflows/ci.yaml "Workflow Link"

---

## Plippa local patches

This is tiny_http 0.12.0 from crates.io (MIT OR Apache-2.0), modified by Plippa. Wired in through
`[patch.crates-io]` in the workspace `Cargo.toml`. Every change is marked `PLIPPA PATCH` in the
source. Drop a patch when upstream fixes the same thing (check a release newer than 0.12.0), and
list this section in `docs/third-party-notices.md` (the notices must say tiny_http is modified).

1. **Worker pool starvation** (`src/util/task_pool.rs`, `TaskPool::spawn`, NB_VERIFY NB_10).
   Connections accepted in a burst could be queued behind a single idle worker and starve while
   keep-alive connections held the other workers.
2. **Request head limits** (`src/client.rs`, `src/limits.rs`; SECFIX_01, review id SRV-02).
   `read_next_line` stops accumulating at a cap instead of after it: request line 8 KiB (`414`),
   header line 8 KiB, 64 headers and 32 KiB of head in total (`431`). The answer is flushed, up
   to 64 KiB of what the client is still sending is swallowed (so closing does not reset the
   connection before the client has read the answer), and the connection is closed. Upstream read
   into an unbounded buffer. All values are in `Limits` (`Limits::default()` is the production
   set).
3. **Socket timeouts** (`src/limits.rs`, `src/client.rs`, `src/util/refined_tcp_stream.rs`,
   `src/connection.rs`, `src/util/sequential.rs`, `src/lib.rs`). Upstream put no timeout on any
   socket. Now: a write timeout (30 s) on every accepted socket; a wait for the first byte of a
   request of at most the keep-alive idle time (30 s; an idle connection is closed silently); the
   whole head within 15 s of its first byte **and** at most 10 s of silence while it arrives
   (both, because inactivity alone does not stop a one-byte-per-9-seconds drip; an expired head
   gets `408`); and while a request body arrives, at most 30 s of silence, re-armed by every read
   that returns data. There is **no limit on the total time of a body**: a large upload over slow
   Wi-Fi must be able to finish, and the application enforces a minimum rate on top. The phase is
   switched by a shared `ReadTimer` that the socket reader applies before each read;
   `SequentialReader::wait_turn` lets the connection thread wait for the previous request's body
   to be consumed before it arms the idle timeout.
4. **Connection gate and worker ceiling** (`src/lib.rs`, `src/limits.rs`,
   `src/util/task_pool.rs`). `ServerOptions { limits, gate }` and `Server::with_options` /
   `Server::from_listener_with_options`. A `ConnectionGate` is asked on the accept thread, before a
   single byte is read, whether a connection may be served; a refused connection gets `503` with
   `Retry-After: 5` and is closed, never queued. The gate also receives a `ConnectionHandle` so
   it can force-close the connection later (Plippa's `Connections` registry uses it to list and
   close live connections by predicate). `TaskPool` has a ceiling on worker threads
   (`Limits::max_connection_threads`); beyond it a connection is refused instead of a thread
   being started. `Server::new` and `Server::from_listener` keep their signatures and use
   `ServerOptions::default()`.
5. **One request at a time per connection** (`src/lib.rs`). Upstream let a plain-HTTP client
   pipeline without limit (its connection thread read the next head while earlier requests were
   still queued); only the HTTPS path waited for each answer. Now both wait, so queued requests
   are bounded by the number of connections. Responses were always sent in order, so clients see
   no difference; only the server no longer reads ahead.
6. **Unread request bodies** (`src/util/equal_reader.rs`, `src/request.rs`, `src/client.rs`).
   When a request is dropped without its body having been read, upstream allocated a buffer of
   the *whole remaining* `Content-Length` to throw it away (a `Content-Length` of 64 GiB aborted
   the process with an allocation failure) and kept reading for as long as the client kept
   sending; an unread chunked body was not drained at all. Now the drain uses an 8 KiB buffer, at
   most 16 MiB and 10 s, and **whenever any byte of a body is left unread** (budget or time spent,
   error, timeout, early EOF, any unread chunked body) the connection is **closed** after the
   current answer. Leftover body bytes are never parsed as a request head: that would let a page
   smuggle a request with forged headers (no `Origin`, a made-up `X-Plippa`) past the checks made
   on the real head.

7. **Accept errors** (`src/lib.rs`, `is_transient_accept_error`, `accept_until_ready`). Upstream
   ended the accept thread on any `accept()` error, so running out of file descriptors
   (`EMFILE`/`ENFILE`), memory or buffers, or a client that aborted, could take the portal down for
   good. Those are now logged, followed by a 100 ms back-off, and the loop carries on; a broken
   listener still ends it. The step is `accept_until_ready`, unit-tested in `lib.rs`
   (`accept_loop_tests`): it drives a scripted `accept()` through `EMFILE`, `ENFILE` and
   `ECONNABORTED`, checks the loop keeps asking and serves the next connection, and checks a fatal
   error ends it without a retry.

8. **Request framing** (`src/request.rs`, `src/client.rs`; SECFIX_03, review id SRV-02). A
   `Content-Length` that is not a run of ASCII digits (`abc`, `-1`), **any** second
   `Content-Length` (even two that agree), or a `Transfer-Encoding` that is anything other than
   `chunked` (so `identity`, `gzip`, an unknown token and a duplicate are all refused) is
   answered `400` with `Connection: close` and the connection is closed. Upstream treated an
   unparsable length as "no body" and parsed the body bytes as the next request head.

9. **HTTPS, single port** (`src/ssl/rustls.rs`, `src/lib.rs`, `src/connection.rs`, `src/limits.rs`,
   `src/client.rs`; SECFIX_03). The `ssl-rustls` feature now builds against **rustls 0.23** with
   `rustls-pki-types` 1 and the `ring` provider, and `SslContext::from_der` takes the certificate
   and PKCS#8 key as DER (the certificate is generated in-process; `rustls-pemfile` is gone).
   `SslConfig::accept_plaintext` lets one port serve both: **the connection's worker thread**
   peeks one byte (`Connection::peek`, bounded by `Limits::sniff_timeout`), a `0x16` record goes
   through rustls and anything else is served as plain HTTP with `Request::secure() == false`.
   Both the peek and the TLS wrapper it chooses are built in the worker, never on the accept
   thread, so a client that connects and says nothing costs only its own worker (a scanner
   looping "connect, close" cannot throttle admission for everyone else). The handshake runs in
   that same worker on the first read, and the phase
   timeouts apply through the TLS wrapper (every read and write re-applies the socket timeout, so
   `Stream::set_read_timeout` is no longer a no-op for HTTPS). A fresh connection's first read is
   bounded by the head deadline, so a TLS client that connects and stalls is cut off and never
   holds a worker for the keep-alive idle time.
