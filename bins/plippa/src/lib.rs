//! `plippa` — launcher and simple crash-restart watchdog (MVP_08 / MVP_12).

mod launch_plan;
mod notify;
mod restart;
mod shutdown;
mod supervise;
mod watchdog;

pub use launch_plan::{
    LaunchArgs, LaunchPlan, ENV_BIND, ENV_BOARD, ENV_LIBRARY_ROOT, ENV_POWEROFF, ENV_ROOT,
    ENV_SINK, ENV_UI_BACKEND,
};
pub use notify::{Notifier, WatchdogPacer, ENV_NOTIFY_SOCKET, WATCHDOG_INTERVAL};
pub use restart::{ChildState, Decision, RestartPolicy};
pub use shutdown::{finish as finish_shutdown, system_poweroff, PoweroffFn, ShutdownAction};
pub use supervise::{
    install_sigterm_handler, note_stop_signal, stop_signalled, supervise, wait_ready, Supervisor,
    EXIT_BUDGET, EXIT_INTERNAL, EXIT_OK, EXIT_START_FAILED,
};
pub use watchdog::{
    count_starts, mirror_log_to_writer, stub_spawn_exit_ok, stub_spawn_sleep, ChildSpec, ClockFn,
    LaunchEvent, LaunchLog, Launcher, LauncherConfig, ProcessName, SpawnFn, WatchStatus,
};
