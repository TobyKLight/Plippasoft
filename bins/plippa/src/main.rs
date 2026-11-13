//! `plippa` launcher binary.

use plippa::{
    install_sigterm_handler, note_stop_signal, stop_signalled, supervise, system_poweroff,
    wait_ready, LaunchArgs, LaunchEvent, LaunchPlan, Launcher, Notifier, PoweroffFn, Supervisor,
    ENV_POWEROFF, EXIT_START_FAILED,
};
use plippa_ipc_proto::{clear_shutdown, ensure_ipc_dir, resolve_paths, ENV_IPC_DIR};
use std::env;
use std::sync::Arc;
use std::time::Duration;

fn main() {
    std::process::exit(real_main());
}

fn real_main() -> i32 {
    let args = LaunchArgs::from_args_env(env::args());

    if args.test_stubs {
        eprintln!("plippa: --test-stubs mode (sleep children)");
        let cfg = LaunchPlan::build(&args, None).into_config();
        let launcher = Launcher::with_spawn(cfg, plippa::stub_spawn_sleep(30));
        return run(launcher, false, Arc::new(system_poweroff));
    }

    let bin_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let plan = LaunchPlan::build(&args, bin_dir.as_deref());
    let poweroff = plan.poweroff;

    for (k, v) in &plan.env.pairs {
        env::set_var(k, v);
    }

    let paths = resolve_paths();
    if let Err(e) = ensure_ipc_dir(&paths) {
        eprintln!(
            "plippa: refusing to start, IPC dir {} is not usable: {e}",
            paths.dir.display()
        );
        return EXIT_START_FAILED;
    }
    env::set_var(ENV_IPC_DIR, &paths.dir);
    // PWRSW_01: the UI writes a sentinel while we are watching, so anything present now was
    // left by a previous run (crash, SIGKILL, a restart that raced the UI). Replaying it
    // would stop — or, after a 4 s hold, power off — the host for no reason. `$XDG_RUNTIME_DIR`
    // survives a service restart, so this is not merely theoretical.
    clear_shutdown(&paths);
    let mut cfg = plan.into_config();
    for spec in &mut cfg.children {
        if !spec.env.iter().any(|(k, _)| k == ENV_IPC_DIR) {
            spec.env
                .push((ENV_IPC_DIR.to_string(), paths.dir.to_string_lossy().into_owned()));
        }
    }

    eprintln!(
        "plippa: IPC dir {} (engine sock {})",
        paths.dir.display(),
        paths.engine_sock.display()
    );
    if let Some(lib) = env::var_os(plippa::ENV_LIBRARY_ROOT) {
        eprintln!("plippa: library root {}", lib.to_string_lossy());
    }
    if poweroff {
        eprintln!(
            "plippa: WARNING {ENV_POWEROFF}/--poweroff enabled — a 4 s volume hold powers the host off"
        );
    } else {
        eprintln!(
            "plippa: host poweroff disabled (set {ENV_POWEROFF}=1 or --poweroff for lab devices)"
        );
    }

    let launcher = Launcher::new(cfg);
    run(launcher, poweroff, Arc::new(system_poweroff))
}

fn run(launcher: Launcher, poweroff_enabled: bool, poweroff: PoweroffFn) -> i32 {
    let paths = resolve_paths();
    // SIGINT (ctrlc) and SIGTERM (systemctl stop) both raise one atomic flag; the loop below
    // runs the graceful stop. Nothing else happens in the handlers.
    if let Err(e) = ctrlc::set_handler(note_stop_signal) {
        eprintln!("plippa: ctrlc handler not installed: {e}");
    }
    if let Err(e) = install_sigterm_handler() {
        eprintln!("plippa: SIGTERM handler not installed: {e}");
    }
    let notifier = Notifier::from_env_take();

    let mut kids = match launcher.start_all() {
        Ok(kids) => kids,
        Err(e) => {
            eprintln!("plippa: failed to start children: {e}");
            eprintln!("plippa: hint: put engine/ui/server on PATH or use --test-stubs");
            return EXIT_START_FAILED;
        }
    };
    eprintln!("plippa: started {} processes", kids.len());
    for ev in launcher.log().events() {
        if let LaunchEvent::Started { name, pid } = ev {
            eprintln!("plippa: {} pid={pid}", name.as_str());
        }
    }

    wait_ready(&paths, &notifier, Duration::from_secs(5), &stop_signalled);

    if env::var_os("PLIPPA_ONCE").is_some() {
        eprintln!("plippa: PLIPPA_ONCE set — waiting for children (no restart)");
        for (name, mut child) in kids.drain() {
            match child.wait() {
                Ok(status) => {
                    eprintln!("plippa: {} exited {:?}", name.as_str(), status.code())
                }
                Err(e) => eprintln!("plippa: wait {}: {e}", name.as_str()),
            }
        }
        return 0;
    }

    supervise(
        &launcher,
        &mut kids,
        &Supervisor {
            paths: &paths,
            notifier: &notifier,
            poweroff_enabled,
            poweroff: &poweroff,
            stop_signal: &stop_signalled,
        },
    )
}
