//! Ordered shutdown and the host power-off seam (PWRSW_01).
//!
//! A plain `shutdown` sentinel only stops the app (`systemctl restart plippa`, Ctrl-C, deploy
//! scripts). Only the `poweroff` sentinel — written by the UI after a 4 s volume hold — may halt
//! the host, and only when the launcher was armed with `PLIPPA_POWEROFF=1` / `--poweroff`.
//!
//! The power-off command is injectable ([`PoweroffFn`]) so tests never halt the machine they
//! run on, mirroring [`Launcher::with_spawn`].

use crate::{Launcher, ProcessName};
use std::collections::HashMap;
use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;

/// Runs the host power-off command. Injectable for tests.
pub type PoweroffFn = Arc<dyn Fn() -> io::Result<ExitStatus> + Send + Sync>;

/// What happens once every child has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownAction {
    /// Stop the app only (deploy / restart / Ctrl-C).
    Exit,
    /// Stop the children, sync, then power the host off.
    PowerOff,
}

impl ShutdownAction {
    /// `poweroff_requested` is the UI's `poweroff` sentinel; `enabled` is `PLIPPA_POWEROFF`.
    /// Both are required: a stray sentinel in a dev run must never halt the host.
    pub fn decide(poweroff_requested: bool, enabled: bool) -> Self {
        if poweroff_requested && enabled {
            Self::PowerOff
        } else {
            Self::Exit
        }
    }
}

/// The real host power-off. `--no-block` returns immediately, so the launcher exits without
/// waiting for systemd; the fallback covers systems without `systemctl`.
pub fn system_poweroff() -> io::Result<ExitStatus> {
    match Command::new("systemctl")
        .args(["--no-block", "poweroff"])
        .status()
    {
        Ok(s) if s.success() => Ok(s),
        Ok(s) => {
            eprintln!("plippa: systemctl poweroff exited {s}; trying shutdown -h now");
            Command::new("shutdown").args(["-h", "now"]).status()
        }
        Err(e) => {
            eprintln!("plippa: systemctl poweroff failed: {e}; trying shutdown -h now");
            Command::new("shutdown").args(["-h", "now"]).status()
        }
    }
}

/// Stop the children, then perform `action`. Runs the power-off command **at most once**, and
/// only after `kids` is empty, so the server has checkpointed and closed its SQLite files.
///
/// Errors from `watch_once` are returned; a failing power-off command is logged and swallowed
/// (the launcher still exits 0 — the host is going down either way).
pub fn finish(
    launcher: &Launcher,
    kids: &mut HashMap<ProcessName, Child>,
    action: ShutdownAction,
    poweroff: &PoweroffFn,
) -> io::Result<()> {
    launcher.request_shutdown();
    launcher.watch_once(kids, usize::MAX)?;
    if !kids.is_empty() {
        eprintln!("plippa: children still running after shutdown watch — not powering off");
        return Ok(());
    }
    match action {
        ShutdownAction::Exit => {
            eprintln!("plippa: shutdown complete (no host poweroff)");
        }
        ShutdownAction::PowerOff => {
            // Kids are reaped; make sure the filesystem has flushed before the rails drop.
            unsafe {
                libc::sync();
            }
            eprintln!("plippa: all children stopped, filesystems synced — requesting host poweroff…");
            match poweroff() {
                Ok(s) if s.success() => {}
                Ok(s) => eprintln!("plippa: poweroff exited {s}"),
                Err(e) => eprintln!("plippa: poweroff failed: {e}"),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{stub_spawn_sleep, LauncherConfig, SpawnFn};

    #[test]
    fn sentinel_alone_never_powers_off() {
        assert_eq!(ShutdownAction::decide(false, true), ShutdownAction::Exit);
        assert_eq!(ShutdownAction::decide(true, false), ShutdownAction::Exit);
        assert_eq!(ShutdownAction::decide(false, false), ShutdownAction::Exit);
        assert_eq!(
            ShutdownAction::decide(true, true),
            ShutdownAction::PowerOff
        );
    }

    fn run_with(calls: Arc<std::sync::atomic::AtomicUsize>, action: ShutdownAction) {
        let spawn: SpawnFn = stub_spawn_sleep(30);
        let launcher = Launcher::with_spawn(LauncherConfig::default(), spawn);
        let mut kids = launcher.start_all().unwrap();
        let counter = Arc::clone(&calls);
        let poweroff: PoweroffFn = Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Command::new("true").status()
        });
        finish(&launcher, &mut kids, action, &poweroff).unwrap();
        assert!(kids.is_empty(), "children must be reaped before power-off");
    }

    #[test]
    fn poweroff_runs_once_after_children_stop() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        run_with(Arc::clone(&calls), ShutdownAction::PowerOff);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn plain_exit_never_runs_poweroff() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        run_with(Arc::clone(&calls), ShutdownAction::Exit);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
