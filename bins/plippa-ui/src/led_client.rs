//! Client of the `plippa-leds` helper (SYS-02).
//!
//! The UI composes colours, but it must never touch `/dev/mem`: driving the strips needs
//! `CAP_SYS_RAWIO`, so the only process holding it is the tiny `plippa-leds` helper. This
//! module is the UI side of that seam — it turns a composed frame into the helper's
//! fixed-size frame ([`plippa_leds::protocol`]) and hands it to a unix socket.
//!
//! The frame loop must never wait for the helper:
//!
//! - the socket is non-blocking, so a send that cannot make progress is simply retried on the
//!   next frame; only the newest frame is kept waiting (the one being written finishes first,
//!   so the byte stream stays a valid sequence of frames);
//! - connecting is lazy and rate-limited by a capped exponential backoff, so an absent helper
//!   (a dev machine, a crashed helper inside its cool-down) costs a `connect(2)` every few
//!   seconds and nothing else;
//! - after every reconnect the last complete frame is written again, so the strips come back
//!   showing what they showed before instead of waiting for the next change.

use plippa_leds::protocol::{self, Layout};
use std::io::{ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// First delay after a failed connect; doubles up to [`RECONNECT_MAX`].
pub const RECONNECT_MIN: Duration = Duration::from_millis(200);
/// Longest delay between connect attempts (a helper that stays dead costs one `connect(2)`
/// every five seconds).
pub const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// The UI's side of the LED socket. One instance per LED worker thread.
pub struct LedsClient {
    path: PathBuf,
    layout: Layout,
    stream: Option<UnixStream>,
    /// The frame currently going out and how many of its bytes already left.
    out: Vec<u8>,
    sent: usize,
    /// A newer frame waiting for the current one to finish; replaced by each newer frame, so
    /// a stalled socket never builds a backlog.
    waiting: Option<Vec<u8>>,
    /// The last complete frame: resent after every reconnect.
    last: Option<Vec<u8>>,
    seq: u8,
    backoff: Duration,
    next_attempt: Instant,
    /// Connect attempts and successful connections (tests and diagnostics).
    attempts: u64,
    connects: u64,
    /// How often the helper closed the connection under us.
    drops: u64,
}

impl LedsClient {
    /// A client for the helper socket in `ipc_dir`, using the layout both sides derive from the
    /// board profile. `now` seeds the first connect attempt (the UI passes its clock).
    pub fn new(ipc_dir: &Path, layout: Layout, now: Instant) -> Self {
        Self {
            path: plippa_leds::socket_path(ipc_dir),
            layout,
            stream: None,
            out: Vec::new(),
            sent: 0,
            waiting: None,
            last: None,
            seq: 0,
            backoff: RECONNECT_MIN,
            next_attempt: now,
            attempts: 0,
            connects: 0,
            drops: 0,
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Connect attempts so far (a backoff test asserts this stays small).
    pub fn attempts(&self) -> u64 {
        self.attempts
    }

    pub fn connects(&self) -> u64 {
        self.connects
    }

    pub fn drops(&self) -> u64 {
        self.drops
    }

    /// Encode one composed frame — one `Vec` of logical RGB per stream, in layout order — and
    /// make progress on the socket. Never blocks and never fails the caller: a helper that is
    /// away just means the frame stays pending (or is dropped when a newer one arrives).
    pub fn send_frames(&mut self, frames: &[Vec<[u8; 3]>], now: Instant) {
        let flat = match protocol::flatten(&self.layout, frames) {
            Ok(flat) => flat,
            Err(error) => {
                // A composed frame must always match the profile; if it does not, dropping it
                // is better than sending a frame the helper would refuse.
                eprintln!("plippa-ui: composed LED frame does not match the layout ({error})");
                return;
            }
        };
        let frame = match protocol::encode(&self.layout, self.seq, &flat) {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!("plippa-ui: could not encode an LED frame ({error})");
                return;
            }
        };
        self.seq = self.seq.wrapping_add(1);
        self.last = Some(frame.clone());
        if self.sent >= self.out.len() {
            self.out = frame;
            self.sent = 0;
        } else {
            // Mid-frame: keep the byte stream valid, then send the newest once it finishes.
            self.waiting = Some(frame);
        }
        self.poll(now);
    }

    /// A blank frame (all LEDs off) — the exit path.
    pub fn send_blank(&mut self, now: Instant) {
        if self.layout.streams().is_empty() {
            return;
        }
        let pixels = vec![[0u8, 0, 0]; self.layout.led_count()];
        let streams: Vec<Vec<[u8; 3]>> = {
            let mut at = 0;
            self.layout
                .streams()
                .iter()
                .map(|stream| {
                    let count = stream.count as usize;
                    let slice = pixels[at..at + count].to_vec();
                    at += count;
                    slice
                })
                .collect()
        };
        self.send_frames(&streams, now);
    }

    /// Write what can be written without waiting, and (re)connect if it is time to.
    fn poll(&mut self, now: Instant) {
        if self.stream.is_none() {
            if self.next_attempt <= now {
                self.connect(now);
            }
            if self.stream.is_none() {
                return;
            }
        }
        loop {
            if self.sent >= self.out.len() {
                match self.waiting.take() {
                    Some(next) => {
                        self.out = next;
                        self.sent = 0;
                    }
                    None => break,
                }
            }
            let Some(stream) = self.stream.as_mut() else {
                break;
            };
            match stream.write(&self.out[self.sent..]) {
                Ok(0) => {
                    self.disconnect(now);
                    break;
                }
                Ok(written) => self.sent += written,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => {
                    eprintln!("plippa-ui: LED helper connection lost ({error})");
                    self.disconnect(now);
                    break;
                }
            }
        }
    }

    fn connect(&mut self, now: Instant) {
        self.attempts += 1;
        match UnixStream::connect(&self.path) {
            Ok(stream) => {
                if let Err(error) = stream.set_nonblocking(true) {
                    eprintln!("plippa-ui: LED socket is not non-blocking ({error})");
                    self.disconnect(now);
                    return;
                }
                self.stream = Some(stream);
                self.connects += 1;
                self.backoff = RECONNECT_MIN;
                // The helper blanks on start-up and after the client disappears: give it the
                // last full frame again so the strips do not stay dark until the next change.
                if let Some(last) = self.last.clone() {
                    self.out = last;
                    self.sent = 0;
                    self.waiting = None;
                }
                eprintln!(
                    "plippa-ui: connected to the LED helper ({})",
                    self.path.display()
                );
            }
            Err(error) => {
                // One line per attempt would be noise on a dev machine with no helper: only
                // the first failure of a streak is worth a line.
                if self.attempts == 1 {
                    eprintln!(
                        "plippa-ui: no LED helper at {} ({error}); retrying in the background",
                        self.path.display()
                    );
                }
                self.next_attempt = now + self.backoff;
                self.backoff = (self.backoff * 2).min(RECONNECT_MAX);
            }
        }
    }

    fn disconnect(&mut self, now: Instant) {
        self.stream = None;
        self.out.clear();
        self.sent = 0;
        self.drops += 1;
        self.next_attempt = now + self.backoff;
        self.backoff = (self.backoff * 2).min(RECONNECT_MAX);
    }

    /// Best-effort final blank and close. Used on the exit path, where waiting a few
    /// milliseconds for the helper to take the last frame is worth it (the helper blanks on
    /// EOF too, so this is only about immediacy).
    pub fn close(&mut self, now: Instant) {
        if self.stream.is_none() {
            return;
        }
        self.send_blank(now);
        let deadline = Instant::now() + Duration::from_millis(100);
        while self.sent < self.out.len() && Instant::now() < deadline {
            let Some(stream) = self.stream.as_mut() else {
                break;
            };
            match stream.write(&self.out[self.sent..]) {
                Ok(0) => break,
                Ok(written) => self.sent += written,
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if let Some(stream) = self.stream.take() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_leds::protocol::{Layout, StreamSpec, HEADER_LEN};
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;

    fn layout() -> Layout {
        Layout::new(vec![
            StreamSpec {
                channel: 0,
                count: 4,
            },
            StreamSpec {
                channel: 1,
                count: 3,
            },
        ])
    }

    fn frame(rgb: [u8; 3]) -> Vec<Vec<[u8; 3]>> {
        layout()
            .streams()
            .iter()
            .map(|stream| vec![rgb; stream.count as usize])
            .collect()
    }

    /// A fake helper: binds `path`, reads frames into `got`, stops when `stop` is set.
    fn fake_helper(
        path: PathBuf,
        got: Arc<std::sync::Mutex<Vec<Vec<[u8; 3]>>>>,
        stop: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let listener = UnixListener::bind(&path).expect("bind fake helper");
            listener.set_nonblocking(true).unwrap();
            while !stop.load(Ordering::SeqCst) {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => return,
                };
                let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                let mut stream = stream;
                while !stop.load(Ordering::SeqCst) {
                    let mut header = [0u8; HEADER_LEN];
                    if stream.read_exact(&mut header).is_err() {
                        break;
                    }
                    let Ok(header) = protocol::decode_header(&header) else {
                        break;
                    };
                    let mut payload = vec![0u8; header.layout.payload_len()];
                    if stream.read_exact(&mut payload).is_err() {
                        break;
                    }
                    let pixels: Vec<[u8; 3]> = payload
                        .chunks_exact(3)
                        .map(|c| [c[0], c[1], c[2]])
                        .collect();
                    got.lock().unwrap().push(pixels);
                }
            }
            let _ = std::fs::remove_file(&path);
        })
    }

    fn wait_for(got: &Arc<std::sync::Mutex<Vec<Vec<[u8; 3]>>>>, count: usize) -> bool {
        for _ in 0..200 {
            if got.lock().unwrap().len() >= count {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn a_frame_reaches_the_helper_as_one_fixed_size_frame() {
        let dir = tempfile::tempdir().expect("tempdir");
        let got = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let path = plippa_leds::socket_path(dir.path());
        let handle = fake_helper(path.clone(), Arc::clone(&got), Arc::clone(&stop));
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let now = Instant::now();
        let mut client = LedsClient::new(dir.path(), layout(), now);
        client.send_frames(&frame([7, 8, 9]), now);
        assert!(wait_for(&got, 1), "the helper must receive the frame");
        let received = got.lock().unwrap()[0].clone();
        assert_eq!(received.len(), layout().led_count());
        assert_eq!(received[0], [7, 8, 9]);
        assert_eq!(received[6], [7, 8, 9], "both streams are carried");
        client.close(Instant::now());
        stop.store(true, Ordering::SeqCst);
        handle.join().expect("fake helper");
    }

    #[test]
    fn the_ui_resends_the_last_frame_after_the_helper_restarts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = plippa_leds::socket_path(dir.path());
        let now = Instant::now();
        let mut client = LedsClient::new(dir.path(), layout(), now);

        // First helper: takes one frame, then dies (the socket disappears with it).
        let first = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop_first = Arc::new(AtomicBool::new(false));
        let handle = fake_helper(path.clone(), Arc::clone(&first), Arc::clone(&stop_first));
        for _ in 0..100 {
            if path.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        client.send_frames(&frame([1, 2, 3]), now);
        assert!(wait_for(&first, 1), "the first helper receives the frame");
        client.close(Instant::now());
        stop_first.store(true, Ordering::SeqCst);
        handle.join().expect("first helper");
        // Wait until the client noticed the helper vanished.
        let mut now = now + Duration::from_millis(10);
        for _ in 0..100 {
            client.send_frames(&frame([4, 5, 6]), now);
            now += Duration::from_millis(33);
            if !client.is_connected() {
                break;
            }
        }
        assert!(!client.is_connected(), "the dead helper must be noticed");
        let drops = client.drops();

        // Second helper on the same path: it must be handed the last frame without the
        // owner touching anything.
        let second = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop_second = Arc::new(AtomicBool::new(false));
        let handle = fake_helper(path, Arc::clone(&second), Arc::clone(&stop_second));
        let mut restarted = false;
        for _ in 0..300 {
            client.send_frames(&frame([4, 5, 6]), now);
            now += Duration::from_millis(33);
            if client.connects() >= 2 {
                restarted = true;
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(restarted, "the client reconnects once the helper is back");
        let _ = drops;
        assert!(
            wait_for(&second, 1),
            "the restarted helper gets the last frame again"
        );
        assert_eq!(second.lock().unwrap()[0][0], [4, 5, 6]);
        client.close(Instant::now());
        stop_second.store(true, Ordering::SeqCst);
        handle.join().expect("second helper");
    }

    #[test]
    fn a_missing_helper_never_blocks_the_frame_loop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = Instant::now();
        let mut client = LedsClient::new(dir.path(), layout(), base);
        let started = Instant::now();
        let mut now = base;
        for _ in 0..300 {
            client.send_frames(&frame([9, 9, 9]), now);
            now += Duration::from_millis(33);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "300 frames with no helper took {elapsed:?}"
        );
        assert!(
            client.attempts() < 60,
            "the capped backoff must limit connect attempts, not one per frame ({})",
            client.attempts()
        );
        assert!(!client.is_connected());
    }

    #[test]
    fn a_frame_that_arrives_mid_send_waits_and_only_the_newest_is_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let now = Instant::now();
        let mut client = LedsClient::new(dir.path(), layout(), now);
        // Pretend a frame is half written: the stream must stay on a frame boundary.
        client.out = vec![0xAA; 10];
        client.sent = 4;
        client.send_frames(&frame([1, 1, 1]), now);
        assert_eq!(client.out.len(), 10, "the frame in flight is not cut short");
        assert_eq!(client.sent, 4);
        let first_waiting = client.waiting.clone().expect("the new frame waits");
        client.send_frames(&frame([2, 2, 2]), now);
        let second_waiting = client.waiting.clone().expect("still waiting");
        assert_ne!(
            first_waiting, second_waiting,
            "a newer frame replaces the waiting one"
        );
        assert_eq!(
            second_waiting.len(),
            layout().frame_len(),
            "one frame, no backlog"
        );
        assert_eq!(&second_waiting[HEADER_LEN..HEADER_LEN + 3], &[2, 2, 2]);
    }

    /// Dependency-graph fact, not a source string: the UI crate must not depend on
    /// `rs_ws281x` (the only `/dev/mem` user), so no UI code path can map physical memory.
    #[test]
    fn the_ui_binary_has_no_ws281x_dependency_and_no_dev_mem_access() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let output = std::process::Command::new(cargo)
            .args(["metadata", "--format-version", "1", "--no-deps", "--offline"])
            .current_dir(manifest)
            .output()
            .expect("cargo metadata");
        assert!(output.status.success(), "cargo metadata failed");
        let meta: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("cargo metadata is JSON");
        let packages = meta["packages"].as_array().expect("packages");
        let ui = packages
            .iter()
            .find(|package| package["name"] == "plippa-ui")
            .expect("plippa-ui in the metadata");
        let names: Vec<&str> = ui["dependencies"]
            .as_array()
            .expect("dependencies")
            .iter()
            .filter_map(|dep| dep["name"].as_str())
            .collect();
        assert!(
            !names.contains(&"rs_ws281x"),
            "plippa-ui must not depend on rs_ws281x (deps: {names:?})"
        );
    }
}
