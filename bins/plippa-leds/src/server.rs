//! The helper's socket server: one client at a time, fixed-size frames, fail dark.
//!
//! Rules this module enforces (SYS-02):
//! - the socket lives in the launcher's private IPC directory (`0700`, created by
//!   `plippa_ipc_proto::ensure_ipc_dir`) and is `0600`;
//! - the peer's `SO_PEERCRED` uid must equal our own — the separation from the UI is
//!   capability-based, not uid-based, so "our own uid" is the right bar;
//! - a live helper owns the path: bind refuses a path another process answers on, and removes
//!   only a stale socket of our own;
//! - a new client replaces the old one (the UI restarted); the newest client always wins;
//! - anything that is not a valid frame of the helper's own layout closes the connection
//!   (and the strips go dark);
//! - the strips blank on start-up, on a clean exit, after [`DARK_AFTER`] without a frame, and
//!   whenever the client disappears. On `SIGKILL` of the helper nothing can run, so the strips
//!   latch their last frame until the launcher's restart blanks them — accepted and recorded.

use crate::protocol::{self, Layout, HEADER_LEN};
use plippa_ipc_proto::{ensure_ipc_dir, remove_stale_sock, IpcPaths};
use std::fs;
use std::io::{self, ErrorKind, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Socket file name inside the IPC directory.
pub const SOCKET_NAME: &str = "leds.sock";
/// How long the strips stay lit without a frame before they go dark (the UI crashed or hung
/// but the socket is still open). Two seconds is long enough to ride out a stall and short
/// enough that a dead UI does not leave the LEDs glaring.
pub const DARK_AFTER: Duration = Duration::from_secs(2);
/// Read timeout of the per-client loop: the granularity at which it notices a new client, the
/// stop flag and the fail-dark deadline.
const READ_TIMEOUT: Duration = Duration::from_millis(200);
/// Poll granularity while waiting for a client.
const ACCEPT_TICK: Duration = Duration::from_millis(50);

/// The socket path for one runtime.
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(SOCKET_NAME)
}

/// Where frames go. The hardware implementation lives behind the `ws281x` feature; tests use
/// [`RecordingSink`], and a board without strips uses [`NullSink`].
pub trait FrameSink {
    /// Show one full frame. `rgb` is `3 * sum(count)` bytes: r, g, b for every LED of every
    /// stream, streams concatenated in layout order (validated by the server before this call).
    fn show(&mut self, rgb: &[u8]) -> Result<(), String>;

    /// All LEDs off.
    fn blank(&mut self) -> Result<(), String>;
}

/// A board without LED streams (or a dev build): nothing to drive.
#[derive(Debug, Default)]
pub struct NullSink;

impl FrameSink for NullSink {
    fn show(&mut self, _rgb: &[u8]) -> Result<(), String> {
        Ok(())
    }

    fn blank(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// Records what would have been shown. Every `show` is one frame; `blank` counts separately.
#[derive(Debug, Default)]
pub struct RecordingSink {
    frames: Vec<Vec<u8>>,
    blanks: usize,
}

impl RecordingSink {
    pub fn frames(&self) -> &[Vec<u8>] {
        &self.frames
    }

    pub fn last(&self) -> Option<&[u8]> {
        self.frames.last().map(Vec::as_slice)
    }

    pub fn blanks(&self) -> usize {
        self.blanks
    }
}

impl FrameSink for RecordingSink {
    fn show(&mut self, rgb: &[u8]) -> Result<(), String> {
        self.frames.push(rgb.to_vec());
        Ok(())
    }

    fn blank(&mut self) -> Result<(), String> {
        self.blanks += 1;
        Ok(())
    }
}

/// The peer-credential decision, pure so it can be tested with injected credentials.
pub fn peer_allowed(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid
}

/// The uid of the process on the other end of `stream` (`SO_PEERCRED`).
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the fd is owned by `stream` and valid for the call; `cred`/`len` are a correctly
    // sized out-buffer pair for SO_PEERCRED, and the kernel writes at most `len` bytes.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// Injectable clock (the fail-dark deadline and the log timestamps use it).
pub type ClockFn = Arc<dyn Fn() -> Instant + Send + Sync>;

/// The listening helper. `serve` never returns while the stop flag is clear.
pub struct LedsServer {
    listener: UnixListener,
    path: PathBuf,
    layout: Layout,
    clock: ClockFn,
}

impl LedsServer {
    /// Bind `$PLIPPA_IPC_DIR/leds.sock`, creating/verifying the `0700` IPC directory first.
    pub fn bind(paths: &IpcPaths, layout: Layout) -> io::Result<Self> {
        ensure_ipc_dir(paths)?;
        Self::bind_socket(socket_path(&paths.dir), layout)
    }

    /// Bind an explicit path (tests). The socket is `0600`, replacing only a stale socket that
    /// nothing answers on.
    pub fn bind_socket(sock: impl Into<PathBuf>, layout: Layout) -> io::Result<Self> {
        let sock = sock.into();
        if UnixStream::connect(&sock).is_ok() {
            return Err(io::Error::new(
                ErrorKind::AddrInUse,
                format!("{} is already served by another process", sock.display()),
            ));
        }
        remove_stale_sock(&sock)?;
        let listener = UnixListener::bind(&sock)?;
        fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path: sock,
            layout,
            clock: Arc::new(Instant::now),
        })
    }

    /// Replace the clock (tests inject one so the fail-dark deadline can be jumped over).
    pub fn with_clock(mut self, clock: ClockFn) -> Self {
        self.clock = clock;
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Serve frames into `sink` until `stop` is set. Blanks on entry (fail dark on start-up)
    /// and on a clean exit.
    pub fn serve<S: FrameSink>(self, mut sink: S, stop: &AtomicBool) -> io::Result<S> {
        sink.blank().map_err(sink_error)?;
        eprintln!(
            "plippa-leds: serving {} ({} streams, {} LEDs)",
            self.path.display(),
            self.layout.streams().len(),
            self.layout.led_count()
        );
        let mut pending: Option<UnixStream> = None;
        while !stop.load(Ordering::SeqCst) {
            let stream = match pending.take() {
                Some(stream) => stream,
                None => match self.next_client(stop)? {
                    Some(stream) => stream,
                    None => continue,
                },
            };
            match self.serve_client(stream, &mut sink, stop) {
                ClientEnd::Replaced(next) => pending = Some(next),
                ClientEnd::Closed => {}
                ClientEnd::Stop => break,
            }
        }
        sink.blank().map_err(sink_error)?;
        let _ = fs::remove_file(&self.path);
        Ok(sink)
    }

    /// Wait up to [`ACCEPT_TICK`] for a client. `None` = tick (the caller re-checks `stop`).
    fn next_client(&self, _stop: &AtomicBool) -> io::Result<Option<UnixStream>> {
        match self.poll_client()? {
            Some(stream) => Ok(Some(stream)),
            None => {
                // Sleep so the caller re-checks the stop flag; `None` means "no client yet".
                std::thread::sleep(ACCEPT_TICK);
                Ok(None)
            }
        }
    }

    /// Non-blocking accept: `Some` when a client is waiting, `None` when there is none.
    /// Used between frames so a restarted UI takes over even while the old client still streams.
    fn poll_client(&self) -> io::Result<Option<UnixStream>> {
        match self.listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(READ_TIMEOUT))?;
                Ok(Some(stream))
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(error) if error.kind() == ErrorKind::Interrupted => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// One client's session. Returns when the connection ends, is replaced, or we must stop.
    fn serve_client<S: FrameSink>(
        &self,
        stream: UnixStream,
        sink: &mut S,
        stop: &AtomicBool,
    ) -> ClientEnd {
        let own_uid = current_uid();
        match peer_uid(&stream) {
            Ok(uid) if peer_allowed(uid, own_uid) => {}
            Ok(uid) => {
                eprintln!("plippa-leds: refused a client with uid {uid} (ours is {own_uid})");
                let _ = sink.blank();
                return ClientEnd::Closed;
            }
            Err(error) => {
                eprintln!("plippa-leds: could not read the client's credentials: {error}");
                let _ = sink.blank();
                return ClientEnd::Closed;
            }
        }
        let mut stream = stream;
        let mut header = [0u8; HEADER_LEN];
        let mut sequence: Option<u8> = None;
        let mut last_frame = (self.clock)();
        let mut dark = false;
        loop {
            if stop.load(Ordering::SeqCst) {
                return ClientEnd::Stop;
            }
            // A new client (the UI restarted, perhaps while the old one still streams) takes
            // over at once: replacement must not wait for the old client to fall silent.
            match self.poll_client() {
                Ok(Some(next)) => {
                    eprintln!("plippa-leds: a new client replaced the previous one");
                    return ClientEnd::Replaced(next);
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("plippa-leds: accept failed: {error}");
                    return ClientEnd::Closed;
                }
            }
            match stream.read_exact(&mut header) {
                Ok(()) => {}
                Err(error) if is_timeout(&error) => {
                    if !dark && (self.clock)().saturating_duration_since(last_frame) >= DARK_AFTER {
                        eprintln!(
                            "plippa-leds: no frame for {} s; going dark",
                            DARK_AFTER.as_secs()
                        );
                        if let Err(error) = sink.blank() {
                            eprintln!("plippa-leds: blank failed: {error}");
                        }
                        dark = true;
                    }
                    continue;
                }
                Err(error) if error.kind() == ErrorKind::UnexpectedEof => {
                    eprintln!("plippa-leds: client closed the connection");
                    break;
                }
                Err(error) => {
                    eprintln!("plippa-leds: read failed: {error}");
                    break;
                }
            }
            let header = match protocol::decode_header(&header) {
                Ok(header) => header,
                Err(error) => {
                    eprintln!("plippa-leds: refusing a malformed frame: {error}");
                    break;
                }
            };
            if header.layout != self.layout {
                eprintln!(
                    "plippa-leds: refusing a frame for another layout ({} streams vs {})",
                    header.layout.streams().len(),
                    self.layout.streams().len()
                );
                break;
            }
            sequence = Some(header.seq);
            let mut payload = vec![0u8; self.layout.payload_len()];
            if let Err(error) = stream.read_exact(&mut payload) {
                eprintln!(
                    "plippa-leds: frame (seq {}) ended before its declared payload: {error}",
                    header.seq
                );
                break;
            }
            if let Err(error) = sink.show(&payload) {
                eprintln!("plippa-leds: strip update failed: {error}");
                break;
            }
            last_frame = (self.clock)();
            dark = false;
        }
        if let Some(seq) = sequence {
            eprintln!("plippa-leds: session ended at seq {seq}; going dark");
        }
        if let Err(error) = sink.blank() {
            eprintln!("plippa-leds: blank failed: {error}");
        }
        ClientEnd::Closed
    }
}

/// How one client session ended.
enum ClientEnd {
    /// The connection ended (EOF, malformed frame, read error).
    Closed,
    /// Another client connected: serve that one next.
    Replaced(UnixStream),
    /// The stop flag was set.
    Stop,
}

fn current_uid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, cannot fail and has no preconditions.
    unsafe { libc::geteuid() }
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn sink_error(message: String) -> io::Error {
    io::Error::other(message)
}

/// A board profile for tests: the real plippa-D-poc2 profile the helper loads on the device.
#[cfg(test)]
pub(crate) fn tests_profile() -> plippa_board::BoardProfile {
    plippa_board::BoardProfile::load(plippa_board::workspace_root_from_crate(), "plippa-D-poc2")
        .expect("plippa-D-poc2 board profile")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{encode, Layout, StreamSpec};
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Mutex;
    use std::thread;

    fn board_layout() -> Layout {
        Layout::from_profile(&tests_profile())
    }

    fn frames(layout: &Layout, rgb: [u8; 3]) -> Vec<u8> {
        let pixels = vec![rgb; layout.led_count()];
        encode(layout, 1, &pixels).expect("encode")
    }

    /// Start a server in a thread with a recording sink; returns the socket path, the stop
    /// flag and the thread. The sink is recovered by the test from `serve`'s return value.
    struct Harness {
        /// Kept alive for the socket's directory; dropped after the server stops.
        _dir: tempfile::TempDir,
        path: PathBuf,
        stop: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<RecordingSink>>,
        offset: Arc<Mutex<Duration>>,
    }

    impl Harness {
        fn start(layout: Layout) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join(SOCKET_NAME);
            let server = LedsServer::bind_socket(&path, layout).expect("bind");
            let offset = Arc::new(Mutex::new(Duration::ZERO));
            let clock_offset = Arc::clone(&offset);
            let base = Instant::now();
            let server = server.with_clock(Arc::new(move || base + *clock_offset.lock().unwrap()));
            let stop = Arc::new(AtomicBool::new(false));
            let stop_t = Arc::clone(&stop);
            let handle = thread::spawn(move || server.serve(RecordingSink::default(), &stop_t).expect("serve"));
            let mut harness = Self {
                _dir: dir,
                path,
                stop,
                handle: Some(handle),
                offset,
            };
            harness.wait_until_listening();
            harness
        }

        fn wait_until_listening(&mut self) {
            for _ in 0..200 {
                if UnixStream::connect(&self.path).is_ok() {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!("helper did not start listening");
        }

        fn connect(&self) -> UnixStream {
            UnixStream::connect(&self.path).expect("connect")
        }

        fn advance(&self, by: Duration) {
            *self.offset.lock().unwrap() += by;
        }

        fn finish(mut self) -> RecordingSink {
            self.stop.store(true, Ordering::SeqCst);
            self.handle
                .take()
                .expect("handle")
                .join()
                .expect("server thread")
        }
    }

    /// The client must never take the path away from a live helper.
    #[test]
    fn bind_refuses_a_socket_another_helper_answers_on() {
        let harness = Harness::start(board_layout());
        let sock = harness.path.clone();
        let error = match LedsServer::bind_socket(&sock, board_layout()) {
            Ok(_) => panic!("a live helper must keep its socket"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::AddrInUse);
        drop(harness.connect());
        let _ = harness.finish();
        assert!(!sock.exists(), "a clean exit removes its socket");
    }

    #[test]
    fn the_socket_is_0600_in_a_0700_dir_and_a_peer_with_another_uid_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("plippa");
        let paths = IpcPaths::from_dir(&dir);
        // Bind through the IPC-directory path so the directory rules apply.
        let server = LedsServer::bind(&paths, board_layout()).expect("bind");
        let sock = server.path().to_path_buf();
        let dir_mode = fs::metadata(&dir).unwrap().mode() & 0o777;
        let sock_mode = fs::metadata(&sock).unwrap().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "IPC directory");
        assert_eq!(sock_mode, 0o600, "socket");
        assert!(peer_allowed(1000, 1000));
        assert!(!peer_allowed(0, 1000));
        assert!(!peer_allowed(1000, 0));
        let stop = AtomicBool::new(true);
        let _ = server.serve(RecordingSink::default(), &stop);
        assert!(!sock.exists());
    }

    #[test]
    fn a_bad_magic_or_version_is_refused() {
        let layout = board_layout();
        let good = frames(&layout, [1, 2, 3]);
        for mutate in [
            (|b: &mut Vec<u8>| b[0] = b'X') as fn(&mut Vec<u8>),
            |b: &mut Vec<u8>| b[4] = 99,
        ] {
            let harness = Harness::start(layout.clone());
            let mut client = harness.connect();
            let mut frame = good.clone();
            mutate(&mut frame);
            client.write_all(&frame).unwrap();
            let _ = client.shutdown(std::net::Shutdown::Write);
            let sink = harness.finish();
            assert_eq!(sink.frames().len(), 0, "a bad header must not reach the strips");
        }
    }

    #[test]
    fn a_frame_of_the_wrong_length_closes_the_connection() {
        let layout = board_layout();
        let frame = frames(&layout, [9, 9, 9]);
        let harness = Harness::start(layout.clone());
        let mut client = harness.connect();
        client.write_all(&frame[..HEADER_LEN + 10]).unwrap();
        client
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown");
        let sink = harness.finish();
        assert_eq!(sink.frames().len(), 0, "a truncated payload is not a frame");
    }

    #[test]
    fn a_layout_that_does_not_match_the_board_profile_is_refused() {
        let harness = Harness::start(board_layout());
        let mut client = harness.connect();
        let foreign = Layout::new(vec![
            StreamSpec {
                channel: 1,
                count: 30,
            },
            StreamSpec {
                channel: 0,
                count: 33,
            },
        ]);
        client.write_all(&frames(&foreign, [5, 5, 5])).unwrap();
        let _ = client.shutdown(std::net::Shutdown::Write);
        let sink = harness.finish();
        assert_eq!(sink.frames().len(), 0, "another layout never reaches a strip");
    }

    #[test]
    fn a_good_frame_reaches_the_sink() {
        let layout = board_layout();
        let harness = Harness::start(layout.clone());
        let mut client = harness.connect();
        client.write_all(&frames(&layout, [7, 8, 9])).unwrap();
        thread::sleep(Duration::from_millis(100));
        drop(client);
        let sink = harness.finish();
        assert_eq!(
            sink.last().map(|frame| frame[..3].to_vec()),
            Some(vec![7, 8, 9])
        );
        assert_eq!(sink.last().unwrap().len(), layout.payload_len());
        assert!(sink.blanks() >= 2, "blank on start-up and on the client leaving");
    }

    #[test]
    fn a_new_client_replaces_the_old_one_and_the_last_frame_wins() {
        let layout = board_layout();
        let harness = Harness::start(layout.clone());
        let mut old = harness.connect();
        old.write_all(&frames(&layout, [1, 1, 1])).unwrap();
        thread::sleep(Duration::from_millis(50));

        let mut new = harness.connect();
        // The helper accepts the new client on its next read timeout and drops the old one.
        let mut replaced = false;
        for _ in 0..100 {
            new.write_all(&frames(&layout, [2, 2, 2])).unwrap();
            thread::sleep(Duration::from_millis(20));
            if old.write_all(&frames(&layout, [3, 3, 3])).is_err() {
                replaced = true;
                break;
            }
        }
        assert!(replaced, "the first client must be disconnected by the second");
        drop(old);
        thread::sleep(Duration::from_millis(50));
        new.write_all(&frames(&layout, [2, 3, 4])).unwrap();
        thread::sleep(Duration::from_millis(50));
        drop(new);
        let sink = harness.finish();
        assert_eq!(
            sink.last().map(|frame| frame[..3].to_vec()),
            Some(vec![2, 3, 4]),
            "the newest client's newest frame wins"
        );
    }

    #[test]
    fn the_strips_go_dark_when_the_client_disappears() {
        let layout = board_layout();
        let harness = Harness::start(layout.clone());
        let mut client = harness.connect();
        client.write_all(&frames(&layout, [4, 5, 6])).unwrap();
        thread::sleep(Duration::from_millis(50));
        // A connected but silent client (a hung UI) goes dark after DARK_AFTER.
        harness.advance(DARK_AFTER + Duration::from_secs(1));
        thread::sleep(READ_TIMEOUT * 3);
        drop(client);
        let sink = harness.finish();
        assert!(
            sink.last().map(|frame| frame[..3].to_vec()) == Some(vec![4, 5, 6]),
            "the frame was shown before the deadline"
        );
        assert!(
            sink.blanks() >= 2,
            "start-up plus the fail-dark blank (client dropped): {}",
            sink.blanks()
        );
    }

    /// The per-frame sequence counter is only information; it never changes the frame size.
    #[test]
    fn the_sequence_number_does_not_change_the_frame_size() {
        let layout = board_layout();
        for seq in [0u8, 1, 127, 255] {
            let pixels = vec![[0u8, 0, 0]; layout.led_count()];
            let frame = encode(&layout, seq, &pixels).unwrap();
            assert_eq!(frame.len(), layout.frame_len());
            assert_eq!(frame[5], seq);
        }
    }
}
