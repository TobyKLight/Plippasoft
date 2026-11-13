//! Background library scanner (NB_02): lazy delta scan + duration probe, off the service lock.
//!
//! Owns its **own** [`LibraryWriter`] per open root (SQLite WAL allows a second writer
//! connection; commits are short). The [`ServerService`] mutex is only taken briefly to read
//! the plan and to publish results, so HTTP handlers never wait for a directory walk.
//!
//! Playback friendliness (thrash rules): the thread runs at a low `nice` level; while the
//! engine reports `Playing` only every [`PLAYING_SCAN_EVERY`]-th tick scans and no
//! measuring happens (lofty reads media headers = USB I/O).

use crate::service::ServerService;
use plippa_noubin::{LibraryRoot, LibraryWriter};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Media lengths measured per tick (each is a header read on the stick).
pub const PROBE_BATCH: usize = 6;
/// While playing, scan only every n-th tick.
pub const PLAYING_SCAN_EVERY: u64 = 4;

/// Length of a media file in ms via its container headers (`None` if unreadable / zero).
///
/// The headers of a file from a stick are untrusted input: the read runs in the isolated
/// media helper (`plippa_audio::isolate`), on the very file object opened here (no second
/// open by path). A file that kills, hangs or exhausts the helper is marked and answers
/// `None` from then on without a spawn, until it changes.
pub fn probe_duration_ms(path: &Path) -> Option<u64> {
    let file = plippa_core::safe_fs::open_regular_nofollow(path).ok()?;
    isolated_duration(&file, path)
}

/// [`probe_duration_ms`] on an already-opened (and already checked) file: the helper reads that
/// very handle, so nothing is re-opened by path. The format is sniffed from the content.
pub fn probe_duration_of_file(file: std::fs::File) -> Option<u64> {
    let path = path_of_open_file(&file);
    isolated_duration(&file, &path)
}

fn isolated_duration(file: &std::fs::File, path: &Path) -> Option<u64> {
    match plippa_audio::isolate::Isolation::global().header_duration(file, path) {
        Ok(ms) => ms,
        Err(plippa_audio::AudioError::Unplayable { .. }) => None,
        Err(e) => {
            eprintln!("plippa-server: duration probe of {} not run: {e}", path.display());
            None
        }
    }
}

/// The path an open file currently has (`/proc/self/fd`), used only to name it for the crash
/// mark and messages; the file itself is never re-opened through it.
fn path_of_open_file(file: &std::fs::File) -> PathBuf {
    use std::os::unix::io::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .unwrap_or_else(|_| PathBuf::from("unknown-media-file"))
}

/// Scanner state between ticks (one writer connection per root).
#[derive(Default)]
pub struct BackgroundScanner {
    writers: HashMap<PathBuf, LibraryWriter>,
    tick: u64,
}

impl BackgroundScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// One scan cycle: lazy rescan of every open root, then (when idle) one throttled
    /// batch of duration probes. Never panics; errors are reported through the service.
    pub fn tick(
        &mut self,
        service: &Mutex<ServerService>,
        probe: &mut dyn FnMut(&Path) -> Option<u64>,
    ) {
        self.tick += 1;
        let Ok(plan) = service.lock().map(|mut s| s.scan_plan()) else {
            return;
        };
        self.writers
            .retain(|root, _| plan.roots.iter().any(|r| r == root));
        if plan.playing && !self.tick.is_multiple_of(PLAYING_SCAN_EVERY) {
            return;
        }
        for root in &plan.roots {
            if !self.writers.contains_key(root) {
                match LibraryWriter::open(&LibraryRoot::new(root)) {
                    Ok(w) => {
                        self.writers.insert(root.clone(), w);
                    }
                    Err(e) => {
                        eprintln!("plippa-server: scanner cannot open {}: {e}", root.display());
                        continue;
                    }
                }
            }
            let writer = self.writers.get_mut(root).expect("inserted above");
            // The service lock is NOT held while scanning.
            let result = writer.lazy_rescan();
            let vanished = matches!(
                result,
                Err(plippa_noubin::LibraryError::StorageVanished { .. })
            );
            if let Ok(mut s) = service.lock() {
                s.apply_background_scan(root, result);
            }
            if vanished {
                self.writers.remove(root);
                continue;
            }
            if plan.playing {
                continue;
            }
            match writer.probe_durations(probe, PROBE_BATCH) {
                Ok(report) => {
                    // The scanner owns its own writer and bypasses `after_scan`, so a probe
                    // batch that wrote lengths has to bound the WAL here (PWRSW_01 / A10).
                    let changed = report.changed();
                    if let Ok(mut s) = service.lock() {
                        s.apply_background_probe(&report);
                    }
                    if changed {
                        if let Err(e) = writer.checkpoint_truncate() {
                            eprintln!(
                                "plippa-server: WAL checkpoint after duration probe failed: {e}"
                            );
                        }
                    }
                }
                Err(e) => eprintln!("plippa-server: duration probe failed: {e}"),
            }
        }
    }
}

/// Sleep up to `total`, returning early with `true` as soon as `stop` is set. Keeps a
/// shutdown from waiting a whole 30 s scan interval (PWRSW_01).
pub(crate) fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) -> bool {
    const STEP: Duration = Duration::from_millis(50);
    let mut left = total;
    while !left.is_zero() {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        let s = left.min(STEP);
        thread::sleep(s);
        left -= s;
    }
    stop.load(Ordering::Relaxed)
}

/// Spawn the scanner thread (low priority, `interval` between ticks).
///
/// Returns the handle so shutdown can join it before the volume goes away (PWRSW_01).
pub fn spawn_background_scanner(
    service: Arc<Mutex<ServerService>>,
    interval: Duration,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("plippa-scan".into())
        .spawn(move || {
            lower_thread_priority();
            let mut scanner = BackgroundScanner::new();
            let mut probe = probe_duration_ms;
            loop {
                if sleep_unless_stopped(&stop, interval) {
                    return;
                }
                scanner.tick(&service, &mut probe);
            }
        })
        .expect("spawn scanner thread")
}

/// Hot-plug watcher: poll the mounted library roots and reopen index connections when a
/// stick appears, disappears or is remounted.
///
/// The 30 s lazy scanner is far too slow to make "plug a stick in and use it" feel like hot
/// swap, and an unplugged/remounted USB leaves the server's SQLite writer stale (it keeps
/// failing with `database disk image is malformed`). Discovery is two small file reads, so a
/// 1 s poll is cheap and gives the server — and through the `LibraryIndexUpdated` notice the
/// engine and UI — fresh connections within about a second.
pub const MOUNT_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub fn spawn_mount_watcher(
    service: Arc<Mutex<ServerService>>,
    interval: Duration,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("plippa-mounts".into())
        .spawn(move || loop {
            if sleep_unless_stopped(&stop, interval) {
                return;
            }
            if let Ok(mut s) = service.lock() {
                s.poll_mounts();
            }
        })
        .expect("spawn mount watcher thread")
}

/// Write the engine's long-form resume requests (NB_04) through to `.noudata`, ~once a
/// second. The queue is filled by the IPC drain thread; this only takes the service lock
/// when something is pending (checked inside `apply_resume_writebacks`, which is cheap).
pub fn spawn_resume_writer(
    service: Arc<Mutex<ServerService>>,
    interval: Duration,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("plippa-resume".into())
        .spawn(move || loop {
            if sleep_unless_stopped(&stop, interval) {
                return;
            }
            if let Ok(mut s) = service.lock() {
                s.apply_resume_writebacks();
            }
        })
        .expect("spawn resume writer thread")
}

/// `nice +10` for the calling thread (Linux applies `setpriority(PRIO_PROCESS, tid)` per thread).
///
/// Shared with the waveform worker (WAVE_01): both are background work that must never
/// compete with the audio thread.
pub(crate) fn lower_thread_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: plain syscalls with integer arguments; failure is ignored (best effort).
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        let _ = libc::setpriority(libc::PRIO_PROCESS, tid, 10);
    }
}
