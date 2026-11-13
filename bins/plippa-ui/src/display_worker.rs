//! Display worker: depth-1 latest-frame-wins mailbox for SPI present (MVP_16).
//!
//! Coordinator enqueues via [`DisplayProxy`]; one worker owns the real
//! [`DisplayBackend`] (ILI9341 or test double). Mocks stay synchronous —
//! do not spawn this worker in CI mock paths.

use crate::display::{DisplayBackend, FrameBuffer, Rect};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Shared mailbox: at most one pending frame (replaced on flood).
///
/// The optional [`Rect`] is the region that frame presents: `None` means the whole frame.
/// A newer enqueue **unions** its rect with the pending one before replacing the frame, so a
/// dropped intermediate frame can never leave stale pixels (WAVE_02).
struct MailboxState {
    pending: Option<(FrameBuffer, Option<Rect>)>,
    blank: bool,
    stop: bool,
}

impl MailboxState {
    /// Store `frame` as the pending one, merging the present region with whatever is still
    /// queued. Returns `true` when a frame was dropped by the replace.
    fn enqueue(&mut self, frame: FrameBuffer, rect: Option<Rect>) -> bool {
        let dropped = self.pending.is_some();
        let merged = match (self.pending.take(), rect) {
            (Some((_, None)), _) | (_, None) => None,
            (Some((_, Some(prev))), Some(new)) => Some(prev.union(new)),
            (None, Some(new)) => Some(new),
        };
        self.pending = Some((frame, merged));
        dropped
    }
}

/// Handle returned after spawn — join on UI shutdown.
pub struct DisplayWorkerHandle {
    join: Option<JoinHandle<()>>,
    mailbox: Arc<(Mutex<MailboxState>, Condvar)>,
    /// Test / diagnostics: frames actually presented by the worker.
    presented: Arc<AtomicU64>,
    /// Test / diagnostics: frames dropped by latest-wins replace.
    dropped: Arc<AtomicU64>,
}

impl DisplayWorkerHandle {
    /// Coordinator-facing proxy (non-blocking present).
    pub fn proxy(&self, width: u16, height: u16) -> DisplayProxy {
        DisplayProxy {
            mailbox: Arc::clone(&self.mailbox),
            width,
            height,
            dropped: Arc::clone(&self.dropped),
        }
    }

    pub fn presented_count(&self) -> u64 {
        self.presented.load(Ordering::Relaxed)
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn stop_and_join(mut self, timeout: Duration) {
        {
            let (lock, cv) = &*self.mailbox;
            let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
            st.blank = true;
            st.stop = true;
            st.pending = None;
            cv.notify_one();
        }
        if let Some(h) = self.join.take() {
            let start = Instant::now();
            loop {
                if h.is_finished() {
                    let _ = h.join();
                    break;
                }
                if start.elapsed() >= timeout {
                    eprintln!("plippa-ui: display worker join timed out after {timeout:?}");
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

impl Drop for DisplayWorkerHandle {
    fn drop(&mut self) {
        {
            let (lock, cv) = &*self.mailbox;
            if let Ok(mut st) = lock.lock() {
                st.blank = true;
                st.stop = true;
                st.pending = None;
                cv.notify_one();
            }
        }
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// Non-blocking display sink for the coordinator.
pub struct DisplayProxy {
    mailbox: Arc<(Mutex<MailboxState>, Condvar)>,
    width: u16,
    height: u16,
    dropped: Arc<AtomicU64>,
}

impl DisplayBackend for DisplayProxy {
    fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    fn present(&mut self, frame: &FrameBuffer) {
        let (lock, cv) = &*self.mailbox;
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        if st.enqueue(frame.clone(), None) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        st.blank = false;
        cv.notify_one();
    }

    fn present_rect(&mut self, frame: &FrameBuffer, rect: Rect) {
        let (lock, cv) = &*self.mailbox;
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        if st.enqueue(frame.clone(), Some(rect)) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        st.blank = false;
        cv.notify_one();
    }

    fn blank(&mut self) {
        let (lock, cv) = &*self.mailbox;
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        st.pending = None;
        st.blank = true;
        cv.notify_one();
    }
}

/// Spawn a worker that owns `backend` and presents from the mailbox.
pub fn spawn_display_worker<D>(backend: D) -> DisplayWorkerHandle
where
    D: DisplayBackend + Send + 'static,
{
    let (width, height) = backend.size();
    let _ = (width, height);
    let mailbox = Arc::new((
        Mutex::new(MailboxState {
            pending: None,
            blank: false,
            stop: false,
        }),
        Condvar::new(),
    ));
    let presented = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0));
    let mb = Arc::clone(&mailbox);
    let presented_t = Arc::clone(&presented);

    let join = thread::Builder::new()
        .name("plippa-ui-display".into())
        .spawn(move || {
            let mut backend = backend;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_display_worker_loop(&mut backend, &mb, &presented_t);
            }));
            if result.is_err() {
                eprintln!("plippa-ui: display worker panicked");
            }
        })
        .expect("spawn display worker");

    DisplayWorkerHandle {
        join: Some(join),
        mailbox,
        presented,
        dropped,
    }
}

fn run_display_worker_loop(
    backend: &mut dyn DisplayBackend,
    mailbox: &(Mutex<MailboxState>, Condvar),
    presented: &AtomicU64,
) {
    let (lock, cv) = mailbox;
    loop {
        let (frame, do_blank, stop) = {
            let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if st.stop || st.blank || st.pending.is_some() {
                    break;
                }
                st = cv
                    .wait(st)
                    .unwrap_or_else(|e| e.into_inner());
            }
            let frame = st.pending.take();
            let do_blank = st.blank;
            if do_blank {
                st.blank = false;
            }
            let stop = st.stop;
            (frame, do_blank, stop)
        };

        if let Some((frame, rect)) = frame {
            match rect {
                Some(rect) => backend.present_rect(&frame, rect),
                None => backend.present(&frame),
            }
            presented.fetch_add(1, Ordering::Relaxed);
        }
        if do_blank {
            backend.blank();
        }
        if stop {
            // Ensure black on exit even if Blank was coalesced away.
            backend.blank();
            break;
        }
    }
}

/// Test double: sleeps inside `present` so coordinator latency can be measured.
#[cfg(test)]
pub struct SlowDisplay {
    pub width: u16,
    pub height: u16,
    pub delay: Duration,
    pub frames: Arc<AtomicU64>,
    pub in_present: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(test)]
impl SlowDisplay {
    pub fn new(width: u16, height: u16, delay: Duration) -> Self {
        Self {
            width,
            height,
            delay,
            frames: Arc::new(AtomicU64::new(0)),
            in_present: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

#[cfg(test)]
impl DisplayBackend for SlowDisplay {
    fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    fn present(&mut self, _frame: &FrameBuffer) {
        self.in_present.store(true, Ordering::SeqCst);
        thread::sleep(self.delay);
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.in_present.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::MockDisplay;
    use crate::input::{InputEvent, InputSource, MockInput};

    #[test]
    fn display_worker_latest_frame_wins_drops_intermediate() {
        let slow = SlowDisplay::new(4, 4, Duration::from_millis(30));
        let presented = Arc::clone(&slow.frames);
        let handle = spawn_display_worker(slow);
        let mut proxy = handle.proxy(4, 4);

        for i in 0..20u16 {
            let mut fb = FrameBuffer::new(4, 4);
            fb.clear(i);
            proxy.present(&fb);
        }
        // Wait until at least one present finishes and mailbox drains.
        let deadline = Instant::now() + Duration::from_millis(500);
        while presented.load(Ordering::Relaxed) < 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(80));
        let dropped = handle.dropped_count();
        let got = handle.presented_count();
        handle.stop_and_join(Duration::from_millis(500));
        assert!(
            dropped >= 1,
            "expected latest-wins drops under flood, dropped={dropped} presented={got}"
        );
        assert!(
            got < 20,
            "worker should not present every flooded frame, presented={got}"
        );
    }

    #[test]
    fn coordinator_handles_input_during_slow_present() {
        let slow = SlowDisplay::new(8, 8, Duration::from_millis(80));
        let in_present = Arc::clone(&slow.in_present);
        let handle = spawn_display_worker(slow);
        let mut proxy = handle.proxy(8, 8);

        // Start a slow present.
        let fb = FrameBuffer::new(8, 8);
        proxy.present(&fb);

        let deadline = Instant::now() + Duration::from_millis(100);
        while !in_present.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            in_present.load(Ordering::SeqCst),
            "slow present should be in flight"
        );

        // While SPI (fake) is blocked, coordinator must still drain PlayStop quickly.
        let mut input = MockInput::new();
        input.push(InputEvent::PlayStop);

        let start = Instant::now();
        let mut saw_play = false;
        for _ in 0..50 {
            for ev in input.poll() {
                if matches!(ev, InputEvent::PlayStop) {
                    saw_play = true;
                }
            }
            if saw_play {
                break;
            }
            // Also enqueue more frames — must not block.
            proxy.present(&fb);
            thread::sleep(Duration::from_millis(1));
        }
        let elapsed = start.elapsed();
        assert!(saw_play, "PlayStop must be handled during slow present");
        assert!(
            elapsed < Duration::from_millis(40),
            "coordinator delayed {elapsed:?} waiting on present (bound 40ms)"
        );

        handle.stop_and_join(Duration::from_millis(500));
    }

    #[test]
    fn mock_backend_needs_no_worker_threads() {
        // Synchronous mock path: present on the calling thread.
        let mut display = MockDisplay::new(16, 16);
        let fb = FrameBuffer::new(16, 16);
        display.present(&fb);
        assert_eq!(display.frames, 1);
    }

    /// Composites exactly like the real panel, with an in-flight delay so the mailbox can be
    /// flooded between presents (WAVE_02 rect-union test).
    struct PatchDisplay {
        screen: Arc<Mutex<FrameBuffer>>,
        in_present: Arc<std::sync::atomic::AtomicBool>,
        delay: Duration,
    }

    impl DisplayBackend for PatchDisplay {
        fn size(&self) -> (u16, u16) {
            let s = self.screen.lock().unwrap();
            (s.width, s.height)
        }

        fn present(&mut self, frame: &FrameBuffer) {
            self.in_present.store(true, Ordering::SeqCst);
            thread::sleep(self.delay);
            *self.screen.lock().unwrap() = frame.clone();
            self.in_present.store(false, Ordering::SeqCst);
        }

        fn present_rect(&mut self, frame: &FrameBuffer, rect: Rect) {
            self.in_present.store(true, Ordering::SeqCst);
            thread::sleep(self.delay);
            let mut screen = self.screen.lock().unwrap();
            let r = rect.clamped(screen.width, screen.height);
            for y in r.y..r.y + r.h {
                let src = y as usize * frame.width as usize;
                let dst = y as usize * screen.width as usize;
                for x in r.x..r.x + r.w {
                    screen.pixels[dst + x as usize] = frame.pixels[src + x as usize];
                }
            }
            self.in_present.store(false, Ordering::SeqCst);
        }
    }

    #[test]
    fn display_worker_rect_union_keeps_disjoint_patches() {
        let screen = Arc::new(Mutex::new(FrameBuffer::new(8, 8)));
        let in_present = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let display = PatchDisplay {
            screen: Arc::clone(&screen),
            in_present: Arc::clone(&in_present),
            delay: Duration::from_millis(40),
        };
        let handle = spawn_display_worker(display);
        let mut proxy = handle.proxy(8, 8);

        let mut base = FrameBuffer::new(8, 8);
        base.clear(1);
        proxy.present(&base);
        let deadline = Instant::now() + Duration::from_millis(200);
        while !in_present.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(in_present.load(Ordering::SeqCst), "first present in flight");

        // Two disjoint patches arrive while the full frame is still on the wire. The second
        // replaces the first in the mailbox but unions the rects; the latest frame contains
        // both changes, so the union patch leaves neither stale.
        let mut first = base.clone();
        first.set_pixel(1, 1, 7);
        proxy.present_rect(&first, Rect::new(1, 1, 1, 1));
        let mut second = first.clone();
        second.set_pixel(6, 6, 9);
        proxy.present_rect(&second, Rect::new(6, 6, 1, 1));

        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            let done = {
                let s = screen.lock().unwrap();
                s.pixels[1 * 8 + 1] == 7 && s.pixels[6 * 8 + 6] == 9
            };
            if done {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let s = screen.lock().unwrap().clone();
        handle.stop_and_join(Duration::from_millis(500));
        assert_eq!(s.pixels[1 * 8 + 1], 7, "first patch survived the union");
        assert_eq!(s.pixels[6 * 8 + 6], 9, "second patch present");
        assert_eq!(s.pixels[0], 1, "base frame elsewhere");
    }

    #[test]
    fn mailbox_union_follows_the_documented_rules() {
        let mut st = MailboxState {
            pending: None,
            blank: false,
            stop: false,
        };
        assert!(!st.enqueue(FrameBuffer::new(2, 2), Some(Rect::new(0, 0, 1, 1))));
        assert!(st.enqueue(FrameBuffer::new(2, 2), Some(Rect::new(1, 1, 1, 1))));
        assert_eq!(
            st.pending.as_ref().unwrap().1,
            Some(Rect::new(0, 0, 2, 2)),
            "disjoint rects union"
        );
        // A full frame clears the region: the whole frame is authoritative.
        assert!(st.enqueue(FrameBuffer::new(2, 2), None));
        assert_eq!(st.pending.as_ref().unwrap().1, None);
        // A rect after a pending full frame stays full (the union covers everything).
        assert!(st.enqueue(FrameBuffer::new(2, 2), Some(Rect::new(0, 0, 1, 1))));
        assert_eq!(st.pending.as_ref().unwrap().1, None);
    }
}
