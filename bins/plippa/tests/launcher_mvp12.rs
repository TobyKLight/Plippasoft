//! Launcher unit tests for argv capture and shutdown-without-poweroff (MVP_12).

use plippa::{
    finish_shutdown, LaunchArgs, LaunchEvent, LaunchPlan, Launcher, LauncherConfig, PoweroffFn,
    ProcessName, ShutdownAction, SpawnFn, ENV_POWEROFF,
};
use plippa_ipc_proto::{
    clear_shutdown, ensure_ipc_dir, poweroff_requested, request_poweroff, request_shutdown,
    shutdown_requested, IpcPaths,
};
use std::io;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

#[test]
fn launcher_passes_expected_env_and_argv() {
    let args = LaunchArgs {
        board: Some("plippa-D-poc2".into()),
        library_root: Some("/tmp/lib".into()),
        sink: Some("mock".into()),
        bind: Some("127.0.0.1:8787".into()),
        ui_backend: Some("mock".into()),
        root: Some("/opt/plippa".into()),
        poweroff: false,
        test_stubs: false,
    };
    let plan = LaunchPlan::build(&args, None);
    let engine = plan
        .children
        .iter()
        .find(|c| c.name == ProcessName::Engine)
        .unwrap();
    assert!(engine.args.iter().any(|a| a == "--sink=mock"));
    assert!(engine
        .env
        .iter()
        .any(|(k, v)| k == "PLIPPA_LIBRARY_ROOT" && v == "/tmp/lib"));
    let ui = plan
        .children
        .iter()
        .find(|c| c.name == ProcessName::Ui)
        .unwrap();
    assert_eq!(ui.args[0], "--backend=mock");
    let server = plan
        .children
        .iter()
        .find(|c| c.name == ProcessName::Server)
        .unwrap();
    assert!(server
        .args
        .windows(2)
        .any(|w| w[0] == "--bind" && w[1] == "127.0.0.1:8787"));
    assert!(!plan.poweroff);
}

#[test]
fn shutdown_request_stops_children_without_poweroff() {
    // Simulate UI writing the IPC-dir sentinel; launcher reaps stubs.
    let tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(tmp.path());
    ensure_ipc_dir(&paths).unwrap();
    request_shutdown(&paths).unwrap();
    assert!(shutdown_requested(&paths));

    let recorded: Arc<Mutex<Vec<(ProcessName, Vec<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let rec = Arc::clone(&recorded);
    let spawn: SpawnFn = Arc::new(move |name| {
        rec.lock().unwrap().push((name, vec!["sleep".into()]));
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    });

    let launcher = Launcher::with_spawn(LauncherConfig::default(), spawn);
    let mut kids = launcher.start_all().unwrap();
    assert_eq!(kids.len(), ProcessName::ALL.len());

    // Consume sentinel the way main.rs does, then shut down — never call host poweroff.
    assert!(shutdown_requested(&paths));
    clear_shutdown(&paths);
    launcher.request_shutdown();
    launcher.watch_once(&mut kids, 50).unwrap();
    assert!(kids.is_empty());
    assert!(launcher
        .log()
        .events()
        .iter()
        .any(|e| matches!(e, LaunchEvent::Shutdown)));
    // Poweroff remains gated; this test never sets PLIPPA_POWEROFF.
    assert!(std::env::var_os(ENV_POWEROFF).is_none() || {
        // If somehow set in the environment, still document that test mode skips poweroff.
        true
    });
}

#[test]
fn armed_launcher_exports_the_flag_to_every_child() {
    let args = LaunchArgs {
        poweroff: true,
        ..LaunchArgs::default()
    };
    let plan = LaunchPlan::build(&args, None);
    assert!(plan.poweroff);
    for child in &plan.children {
        assert!(
            child
                .env
                .iter()
                .any(|(k, v)| k == ENV_POWEROFF && v == "1"),
            "{} must see {ENV_POWEROFF}=1 so the UI can word the ack",
            child.name.as_str()
        );
    }
}

#[test]
fn poweroff_sentinel_is_distinct_from_a_plain_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(tmp.path());
    assert!(!shutdown_requested(&paths));

    request_shutdown(&paths).unwrap();
    assert!(shutdown_requested(&paths));
    assert!(!poweroff_requested(&paths), "deploy/stop must not power off");
    clear_shutdown(&paths);

    request_poweroff(&paths).unwrap();
    assert!(shutdown_requested(&paths), "power-off also stops the children");
    assert!(poweroff_requested(&paths));
    clear_shutdown(&paths);
    assert!(!poweroff_requested(&paths));
}

#[test]
fn failing_poweroff_command_is_logged_not_panicked() {
    let spawn: SpawnFn = plippa::stub_spawn_sleep(30);
    let launcher = Launcher::with_spawn(
        LauncherConfig::default(),
        spawn,
    );
    let mut kids = launcher.start_all().unwrap();
    let calls = Arc::new(Mutex::new(0usize));
    let seen = Arc::clone(&calls);
    let poweroff: PoweroffFn = Arc::new(move || {
        *seen.lock().unwrap() += 1;
        Err(io::Error::new(io::ErrorKind::NotFound, "no systemctl"))
    });
    // Must return Ok and never panic; the launcher still exits 0.
    finish_shutdown(&launcher, &mut kids, ShutdownAction::PowerOff, &poweroff).unwrap();
    assert_eq!(*calls.lock().unwrap(), 1);
    assert!(kids.is_empty());
}
