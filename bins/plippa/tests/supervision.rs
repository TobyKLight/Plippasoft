//! SYS-14 / SYS-09: exit codes of the supervision loop and graceful SIGTERM.

use plippa::{
    supervise, ChildSpec, Launcher, LauncherConfig, Notifier, PoweroffFn, ProcessName,
    RestartPolicy, SpawnFn, Supervisor, EXIT_BUDGET, EXIT_INTERNAL, EXIT_OK,
};
use plippa_ipc_proto::{ensure_ipc_dir, request_shutdown, IpcPaths};
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixDatagram;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

fn spec(name: ProcessName) -> ChildSpec {
    ChildSpec {
        name,
        program: "x".into(),
        args: vec![],
        env: vec![],
    }
}

fn fast_policy() -> RestartPolicy {
    RestartPolicy {
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(2),
        ..RestartPolicy::default()
    }
}

fn cmd(program: &str, arg: &str) -> std::io::Result<std::process::Child> {
    Command::new(program)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn run(
    cfg: LauncherConfig,
    spawn: SpawnFn,
    stop: &dyn Fn() -> bool,
    pre: impl FnOnce(&IpcPaths),
) -> i32 {
    let tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(tmp.path().join("ipc"));
    ensure_ipc_dir(&paths).unwrap();
    pre(&paths);
    let launcher = Launcher::with_spawn(cfg, spawn);
    let mut kids = launcher.start_all().unwrap_or_default();
    let poweroff: PoweroffFn = Arc::new(|| Command::new("true").status());
    supervise(
        &launcher,
        &mut kids,
        &Supervisor {
            paths: &paths,
            notifier: &Notifier::disabled(),
            poweroff_enabled: false,
            poweroff: &poweroff,
            stop_signal: stop,
        },
    )
}

fn sleepers() -> SpawnFn {
    Arc::new(|_| cmd("sleep", "60"))
}

#[test]
fn a_requested_stop_exits_0_and_abnormal_paths_do_not() {
    let cfg = || LauncherConfig {
        children: vec![spec(ProcessName::Engine), spec(ProcessName::Ui)],
        policy: fast_policy(),
        ..LauncherConfig::default()
    };

    // Owner-requested stops: 0.
    assert_eq!(
        run(cfg(), sleepers(), &|| false, |p| request_shutdown(p)
            .unwrap()),
        EXIT_OK,
        "shutdown sentinel"
    );
    assert_eq!(
        run(cfg(), sleepers(), &|| true, |_| {}),
        EXIT_OK,
        "SIGINT/SIGTERM"
    );

    // An essential child that keeps crashing: 70, not 0.
    let crasher: SpawnFn = Arc::new(|_| cmd("true", "x"));
    assert_eq!(run(cfg(), crasher, &|| false, |_| {}), EXIT_BUDGET);

    // Spawn failures after the first start: 70, not 0 (today's launcher returned the error
    // from watch_once and main exited 0).
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let flaky: SpawnFn = Arc::new(move |_| {
        if c.fetch_add(1, Ordering::SeqCst) < 2 {
            cmd("true", "x")
        } else {
            Err(std::io::Error::from_raw_os_error(24))
        }
    });
    assert_eq!(run(cfg(), flaky, &|| false, |_| {}), EXIT_BUDGET);

    // Nothing left to supervise is a supervision failure.
    let empty = LauncherConfig {
        children: vec![],
        ..LauncherConfig::default()
    };
    assert_eq!(run(empty, sleepers(), &|| false, |_| {}), EXIT_INTERNAL);
}

#[test]
fn sigterm_stops_children_gracefully() {
    let tmp = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_plippa"))
        .arg("--test-stubs")
        .env("PLIPPA_IPC_DIR", tmp.path().join("ipc"))
        .env_remove("NOTIFY_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let launcher_pid = child.id() as i32;
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut pids = vec![];
    while pids.len() < 3 {
        let line = lines.next().expect("launcher output").unwrap();
        if let Some(rest) = line.split("pid=").nth(1) {
            pids.push(rest.trim().parse::<i32>().unwrap());
        }
    }
    for pid in &pids {
        assert_eq!(
            unsafe { libc::kill(*pid, 0) },
            0,
            "child {pid} alive before SIGTERM"
        );
    }
    assert_eq!(unsafe { libc::kill(launcher_pid, libc::SIGTERM) }, 0);
    // Drain stderr so the launcher never blocks on a full pipe.
    let drain = std::thread::spawn(move || lines.for_each(drop));
    let end = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < end, "launcher did not exit after SIGTERM");
        std::thread::sleep(Duration::from_millis(50));
    };
    drain.join().unwrap();
    assert_eq!(
        status.code(),
        Some(0),
        "graceful stop exits 0, got {status:?}"
    );
    for pid in &pids {
        let r = unsafe { libc::kill(*pid, 0) };
        let gone = r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        assert!(gone, "child {pid} must be reaped, not orphaned");
    }
}

/// A monotonic clock that runs 400 ms ahead on every reading, so minutes of backoff pass in
/// seconds of wall time while the launcher still sees consistent, increasing time.
fn fast_clock() -> plippa::ClockFn {
    let base = Instant::now();
    let off = Arc::new(Mutex::new(Duration::ZERO));
    Arc::new(move || {
        let mut o = off.lock().unwrap();
        *o += Duration::from_millis(400);
        base + *o
    })
}

#[test]
fn an_essential_child_crashing_instantly_reaches_the_budget_with_the_watchdog_still_pinged() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = IpcPaths::from_dir(tmp.path().join("ipc"));
    ensure_ipc_dir(&paths).unwrap();
    let sock_path = tmp.path().join("notify.sock");
    let server = UnixDatagram::bind(&sock_path).unwrap();
    server.set_nonblocking(true).unwrap();
    let notifier = Notifier::to_address(sock_path.to_str().unwrap());

    let ui_pid = Arc::new(AtomicUsize::new(0));
    let ui_pid_c = Arc::clone(&ui_pid);
    // Engine dies at once every time; the UI is a healthy sleeper whose reaping proves the
    // ordered stop.
    let spawn: SpawnFn = Arc::new(move |name| match name {
        ProcessName::Engine => cmd("true", "x"),
        _ => {
            let c = cmd("sleep", "60")?;
            ui_pid_c.store(c.id() as usize, Ordering::SeqCst);
            Ok(c)
        }
    });
    let launcher = Launcher::with_spawn(
        LauncherConfig {
            children: vec![spec(ProcessName::Engine), spec(ProcessName::Ui)],
            ..LauncherConfig::default()
        },
        spawn,
    )
    .with_clock(fast_clock());
    let mut kids = launcher.start_all().unwrap();
    let poweroff: PoweroffFn = Arc::new(|| Command::new("true").status());
    let code = supervise(
        &launcher,
        &mut kids,
        &Supervisor {
            paths: &paths,
            notifier: &notifier,
            poweroff_enabled: false,
            poweroff: &poweroff,
            stop_signal: &|| false,
        },
    );
    assert_eq!(
        code, EXIT_BUDGET,
        "the 8th crash must be reached, not pre-empted"
    );
    assert!(kids.is_empty());
    let pid = ui_pid.load(Ordering::SeqCst) as i32;
    assert!(pid > 0);
    let r = unsafe { libc::kill(pid, 0) };
    assert!(
        r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
        "the healthy child is stopped in order before exiting 70"
    );
    let (mut pings, mut stopping) = (0, 0);
    let mut buf = [0u8; 64];
    while let Ok(n) = server.recv(&mut buf) {
        match &buf[..n] {
            b"WATCHDOG=1" => pings += 1,
            b"STOPPING=1" => stopping += 1,
            _ => {}
        }
    }
    // The crash sequence takes ~91 s of launcher time (1+2+4+8+16+30+30); a 10 s ping interval
    // gives about nine pings. Withholding pings while the engine is down gave one.
    assert!(
        pings >= 6,
        "pings must continue during backoff, got {pings}"
    );
    assert_eq!(stopping, 1);
}
