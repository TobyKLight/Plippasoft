//! The launcher's supervision loop, exit codes and termination signals (SYS-09 / SYS-14).
//!
//! Exit codes:
//! * [`EXIT_OK`] (0): only for an owner-requested stop (shutdown or power-off sentinel,
//!   SIGINT, SIGTERM). With `Restart=on-failure` systemd then leaves the unit stopped.
//! * [`EXIT_START_FAILED`] (1): the launcher could not start (IPC directory refused, children
//!   could not be spawned).
//! * [`EXIT_BUDGET`] (70): an essential child exhausted its crash budget.
//! * [`EXIT_INTERNAL`] (71): the supervision itself failed (an OS error while polling or
//!   stopping children, or no child left to supervise).
//!
//! Every non-zero code makes `Restart=on-failure` restart the unit, bounded by the unit's
//! `StartLimit*`.

use crate::notify::{Notifier, WatchdogPacer};
use crate::shutdown::{finish, PoweroffFn, ShutdownAction};
use crate::watchdog::{LaunchEvent, Launcher, ProcessName, WatchStatus};
use plippa_ipc_proto::{clear_shutdown, poweroff_requested, shutdown_requested, IpcPaths};
use std::collections::HashMap;
use std::io;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const EXIT_OK: i32 = 0;
pub const EXIT_START_FAILED: i32 = 1;
pub const EXIT_BUDGET: i32 = 70;
pub const EXIT_INTERNAL: i32 = 71;

static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// True once SIGINT or SIGTERM arrived (see [`install_sigterm_handler`]).
pub fn stop_signalled() -> bool {
    SIGNALLED.load(Ordering::SeqCst)
}

/// Record a stop request from a signal context. Async-signal-safe: one atomic store.
pub fn note_stop_signal() {
    SIGNALLED.store(true, Ordering::SeqCst);
}

extern "C" fn on_sigterm(_sig: libc::c_int) {
    note_stop_signal();
}

/// Install a SIGTERM handler that only sets an atomic flag (`systemctl stop` sends SIGTERM;
/// without a handler the launcher would die at once and orphan the children without the
/// graceful SIGINT → WAL checkpoint sequence). SIGINT is handled by `ctrlc` in `main`, which
/// calls [`note_stop_signal`]; no `ctrlc` `termination` feature is needed.
pub fn install_sigterm_handler() -> io::Result<()> {
    // SAFETY: `sigaction` is given a zeroed struct fully initialised below and a handler that
    // performs a single atomic store, which is async-signal-safe.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigterm as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        if libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Wait up to `window` for `engine.sock`, then report `READY=1` either way (a late engine is
/// logged, not fatal). Returns early when a stop is signalled.
pub fn wait_ready(
    paths: &IpcPaths,
    notifier: &Notifier,
    window: Duration,
    stop: &dyn Fn() -> bool,
) {
    let sock = &paths.engine_sock;
    let deadline = Instant::now() + window;
    while Instant::now() < deadline && !stop() {
        if sock.exists() {
            eprintln!("plippa: engine.sock ready at {}", sock.display());
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !sock.exists() {
        eprintln!(
            "plippa: warning — engine.sock not yet at {} (children may still be starting)",
            sock.display()
        );
    }
    notifier.ready();
}

/// What the loop needs besides the launcher and its children.
pub struct Supervisor<'a> {
    pub paths: &'a IpcPaths,
    pub notifier: &'a Notifier,
    pub poweroff_enabled: bool,
    pub poweroff: &'a PoweroffFn,
    /// True when SIGINT / SIGTERM arrived (production: [`stop_signalled`]).
    pub stop_signal: &'a dyn Fn() -> bool,
}

/// Run until the owner stops the launcher or supervision fails; returns the process exit code.
pub fn supervise(
    launcher: &Launcher,
    kids: &mut HashMap<ProcessName, Child>,
    sup: &Supervisor<'_>,
) -> i32 {
    let paths = sup.paths;
    let mut pacer = WatchdogPacer::default();
    // PWRSW_01: only the UI's `poweroff` sentinel (4 s hold) may halt the host; a plain
    // `shutdown` sentinel (deploy, systemctl stop) never does.
    let mut wants_poweroff = false;
    let mut code = EXIT_OK;
    let mut log_cursor = 0usize;

    loop {
        // Mirror new lifecycle events (restarts, spawn failures, cool-downs) to the journal.
        let events = launcher.log().events();
        for ev in &events[log_cursor..] {
            match ev {
                LaunchEvent::Starting { .. } | LaunchEvent::Started { .. } => {}
                other => eprintln!("plippa: {other:?}"),
            }
        }
        log_cursor = events.len();

        if (sup.stop_signal)() {
            eprintln!("plippa: SIGINT/SIGTERM — stopping children");
            clear_shutdown(paths);
            wants_poweroff = false;
            break;
        }
        if shutdown_requested(paths) {
            // Read the reason *before* clearing: it is the only distinction between
            // "stop the app" and "power the host off".
            wants_poweroff = poweroff_requested(paths);
            clear_shutdown(paths);
            if wants_poweroff {
                eprintln!("plippa: poweroff sentinel — stopping children, then powering off");
            } else {
                eprintln!("plippa: shutdown sentinel — stopping children");
            }
            break;
        }
        // Keep-alive on every iteration of the loop, whatever the children are doing: the
        // watchdog exists to catch a hung launcher. A crashing child is the backoff's, the crash
        // budget's and exit 70's business; withholding the ping would let systemd SIGABRT the
        // launcher (core dump, no ordered stop) before the budget is reached.
        if pacer.poll(launcher.now()) {
            sup.notifier.watchdog();
        }
        match launcher.watch_once(kids, 1) {
            Ok(WatchStatus::Running) => {}
            Ok(WatchStatus::Stopped) => break,
            Ok(WatchStatus::Exhausted(name)) => {
                eprintln!(
                    "plippa: {} exhausted its crash budget; children stopped, exiting {EXIT_BUDGET}",
                    name.as_str()
                );
                sup.notifier.stopping();
                return EXIT_BUDGET;
            }
            Err(e) => {
                eprintln!("plippa: watch error: {e}");
                code = EXIT_INTERNAL;
                break;
            }
        }
        if kids.is_empty() && launcher.pending_restarts() == 0 {
            eprintln!("plippa: no children left to supervise");
            code = EXIT_INTERNAL;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    sup.notifier.stopping();
    // An internal error never powers the host off.
    let action = if code == EXIT_OK {
        ShutdownAction::decide(wants_poweroff, sup.poweroff_enabled)
    } else {
        ShutdownAction::Exit
    };
    if let Err(e) = finish(launcher, kids, action, sup.poweroff) {
        eprintln!("plippa: shutdown error: {e}");
        code = EXIT_INTERNAL;
    }
    code
}
