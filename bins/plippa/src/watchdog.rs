//! Start engine / ui / server; restart on unexpected exit; log lifecycle.

use crate::restart::{ChildState, Decision, RestartPolicy};
use std::collections::HashMap;
use std::io::{self, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Logical child process names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProcessName {
    /// The LED strip helper: the only process that holds `CAP_SYS_RAWIO` (SYS-02).
    Leds,
    Engine,
    Ui,
    Server,
}

impl ProcessName {
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessName::Leds => "plippa-leds",
            ProcessName::Engine => "plippa-engine",
            ProcessName::Ui => "plippa-ui",
            ProcessName::Server => "plippa-server",
        }
    }

    pub const ALL: [ProcessName; 4] = [
        ProcessName::Leds,
        ProcessName::Engine,
        ProcessName::Ui,
        ProcessName::Server,
    ];

    /// An essential child cannot be left dead: when its crash budget is exhausted the launcher
    /// stops everything and exits non-zero. A non-essential child is put in a cool-down instead
    /// while the others keep running: the server (indexed items still play) and the LED helper
    /// (the strips are optional hardware — a board without them, or one whose helper cannot get
    /// its capability, is a Plippa with no LED cue, not a broken Plippa).
    pub fn is_essential(self) -> bool {
        match self {
            ProcessName::Engine | ProcessName::Ui => true,
            ProcessName::Server | ProcessName::Leds => false,
        }
    }
}

/// How to spawn a child (real `Command` or test stub).
pub type SpawnFn = Arc<dyn Fn(ProcessName) -> io::Result<Child> + Send + Sync>;

/// Spec for one child binary.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub name: ProcessName,
    pub program: String,
    pub args: Vec<String>,
    /// Extra environment variables (override / set for this child).
    pub env: Vec<(String, String)>,
}

/// Launcher behaviour.
#[derive(Debug, Clone)]
pub struct LauncherConfig {
    pub children: Vec<ChildSpec>,
    /// Backoff, crash budget and cool-down for crashed children.
    pub policy: RestartPolicy,
    /// When true, do not restart after intentional shutdown.
    pub stop_requested: Arc<Mutex<bool>>,
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            children: vec![
                ChildSpec {
                    name: ProcessName::Leds,
                    program: "plippa-leds".into(),
                    args: vec![],
                    env: vec![],
                },
                ChildSpec {
                    name: ProcessName::Engine,
                    program: "plippa-engine".into(),
                    args: vec![],
                    env: vec![],
                },
                ChildSpec {
                    name: ProcessName::Ui,
                    program: "plippa-ui".into(),
                    args: vec![],
                    env: vec![],
                },
                ChildSpec {
                    name: ProcessName::Server,
                    program: "plippa-server".into(),
                    args: vec![],
                    env: vec![],
                },
            ],
            policy: RestartPolicy::default(),
            stop_requested: Arc::new(Mutex::new(false)),
        }
    }
}

/// Lifecycle log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchEvent {
    Starting { name: ProcessName },
    Started { name: ProcessName, pid: u32 },
    Stopped { name: ProcessName, code: Option<i32> },
    /// A restart is scheduled after `delay` (non-blocking backoff).
    Restarting { name: ProcessName, delay: Duration },
    /// The child could not be spawned; counted as an abnormal exit.
    SpawnFailed { name: ProcessName, error: String },
    /// Crash budget exhausted on a non-essential child: left dead for `delay`.
    CoolingDown { name: ProcessName, delay: Duration },
    /// Crash budget exhausted on an essential child: the launcher is stopping.
    GaveUp { name: ProcessName },
    Shutdown,
}

/// Result of a [`Launcher::watch_once`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchStatus {
    /// Still supervising.
    Running,
    /// A shutdown was requested and the children have been stopped.
    Stopped,
    /// An essential child exhausted its crash budget; the remaining children were stopped.
    /// The launcher must exit non-zero ([`crate::EXIT_BUDGET`]).
    Exhausted(ProcessName),
}

/// Supervision bookkeeping that outlives a single `watch_once` call.
#[derive(Default)]
struct Supervision {
    states: HashMap<ProcessName, ChildState>,
    /// Children waiting to be (re)started, and when.
    pending: HashMap<ProcessName, Instant>,
}

/// Injectable monotonic clock (tests advance time without sleeping).
pub type ClockFn = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Thread-safe launch log for tests / stdout mirroring.
#[derive(Debug, Default, Clone)]
pub struct LaunchLog {
    inner: Arc<Mutex<Vec<LaunchEvent>>>,
}

impl LaunchLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, ev: LaunchEvent) {
        self.inner.lock().expect("log").push(ev);
    }

    pub fn events(&self) -> Vec<LaunchEvent> {
        self.inner.lock().expect("log").clone()
    }

    pub fn clear(&self) {
        self.inner.lock().expect("log").clear();
    }
}

fn default_spawn(spec: &ChildSpec) -> io::Result<Child> {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    // Leave the launcher's foreground process group so terminal Ctrl-C only
    // hits `plippa`, which then SIGTERMs children and waits — no orphan stdout
    // after the shell prompt returns.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    cmd.spawn()
}

/// How long children get after SIGINT (server checkpoint on a slow stick needs the time).
const GRACEFUL_STOP_MS: u64 = 5_000;
/// How long children get after SIGTERM before they are killed.
const SIGTERM_GRACE_MS: u64 = 3_000;

/// Watches three (or N) children and restarts on crash.
pub struct Launcher {
    cfg: LauncherConfig,
    log: LaunchLog,
    spawn: SpawnFn,
    clock: ClockFn,
    sup: Mutex<Supervision>,
}

impl Launcher {
    pub fn new(cfg: LauncherConfig) -> Self {
        let specs: HashMap<ProcessName, ChildSpec> = cfg
            .children
            .iter()
            .cloned()
            .map(|c| (c.name, c))
            .collect();
        let spawn: SpawnFn = Arc::new(move |name| {
            let spec = specs.get(&name).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no spec for {name:?}"))
            })?;
            default_spawn(spec)
        });
        Self::with_spawn(cfg, spawn)
    }

    /// Build with an injectable spawn function (unit tests).
    pub fn with_spawn(cfg: LauncherConfig, spawn: SpawnFn) -> Self {
        Self {
            cfg,
            log: LaunchLog::new(),
            spawn,
            clock: Arc::new(Instant::now),
            sup: Mutex::new(Supervision::default()),
        }
    }

    /// Replace the clock (unit tests).
    pub fn with_clock(mut self, clock: ClockFn) -> Self {
        self.clock = clock;
        self
    }

    /// The launcher's notion of "now".
    pub fn now(&self) -> Instant {
        (self.clock)()
    }

    /// Children currently waiting for a scheduled restart (backoff or cool-down).
    pub fn pending_restarts(&self) -> usize {
        self.sup.lock().expect("sup").pending.len()
    }

    pub fn log(&self) -> &LaunchLog {
        &self.log
    }

    pub fn request_shutdown(&self) {
        *self.cfg.stop_requested.lock().expect("stop") = true;
    }

    /// Spawn all children once and return handles (no watch loop). If one cannot be spawned the
    /// ones already started are stopped again before the error is returned, so a failed start
    /// leaves no orphans.
    pub fn start_all(&self) -> io::Result<HashMap<ProcessName, Child>> {
        let mut kids = HashMap::new();
        let mut sup = self.sup.lock().expect("sup");
        for spec in &self.cfg.children {
            self.log.push(LaunchEvent::Starting { name: spec.name });
            match (self.spawn)(spec.name) {
                Ok(child) => {
                    let pid = child.id();
                    self.log.push(LaunchEvent::Started {
                        name: spec.name,
                        pid,
                    });
                    sup.states
                        .entry(spec.name)
                        .or_insert_with(|| ChildState::new(&self.cfg.policy))
                        .on_start(self.now());
                    kids.insert(spec.name, child);
                }
                Err(e) => {
                    drop(sup);
                    let _ = self.shutdown_children(&mut kids);
                    return Err(e);
                }
            }
        }
        Ok(kids)
    }

    /// Poll children; restart unexpected exits (with backoff, never blocking) until shutdown is
    /// requested.
    ///
    /// Returns [`WatchStatus::Stopped`] when shutdown was requested and the children have been
    /// reaped, [`WatchStatus::Exhausted`] when an essential child used up its crash budget (the
    /// rest are stopped first), or [`WatchStatus::Running`] after `max_iterations` (tests, main
    /// loop). A failed re-spawn is a counted abnormal exit, never an error.
    pub fn watch_once(
        &self,
        kids: &mut HashMap<ProcessName, Child>,
        max_iterations: usize,
    ) -> io::Result<WatchStatus> {
        for _ in 0..max_iterations {
            if *self.cfg.stop_requested.lock().expect("stop") {
                self.sup.lock().expect("sup").pending.clear();
                self.shutdown_children(kids)?;
                self.log.push(LaunchEvent::Shutdown);
                return Ok(WatchStatus::Stopped);
            }
            let now = self.now();
            let mut gave_up: Option<ProcessName> = None;

            // Reap exits.
            let names: Vec<ProcessName> = kids.keys().copied().collect();
            for name in names {
                let finished = {
                    let child = kids.get_mut(&name).unwrap();
                    child.try_wait()?.map(|status| status.code())
                };
                if let Some(code) = finished {
                    self.log.push(LaunchEvent::Stopped { name, code });
                    kids.remove(&name);
                    if let Some(n) = self.schedule(name, now) {
                        gave_up.get_or_insert(n);
                    }
                }
            }

            // Start whatever is due.
            let due: Vec<ProcessName> = {
                let sup = self.sup.lock().expect("sup");
                sup.pending
                    .iter()
                    .filter(|(_, at)| **at <= now)
                    .map(|(n, _)| *n)
                    .collect()
            };
            for name in due {
                self.sup.lock().expect("sup").pending.remove(&name);
                self.log.push(LaunchEvent::Starting { name });
                match (self.spawn)(name) {
                    Ok(child) => {
                        let pid = child.id();
                        self.log.push(LaunchEvent::Started { name, pid });
                        self.sup
                            .lock()
                            .expect("sup")
                            .states
                            .entry(name)
                            .or_insert_with(|| ChildState::new(&self.cfg.policy))
                            .on_start(now);
                        kids.insert(name, child);
                    }
                    Err(e) => {
                        self.log.push(LaunchEvent::SpawnFailed {
                            name,
                            error: e.to_string(),
                        });
                        if let Some(n) = self.schedule(name, now) {
                            gave_up.get_or_insert(n);
                        }
                    }
                }
            }

            if let Some(name) = gave_up {
                self.log.push(LaunchEvent::GaveUp { name });
                self.sup.lock().expect("sup").pending.clear();
                self.shutdown_children(kids)?;
                return Ok(WatchStatus::Exhausted(name));
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(WatchStatus::Running)
    }

    /// Record an abnormal exit of `name` and schedule what happens next. Returns the child's name
    /// when it is essential and has exhausted its crash budget.
    fn schedule(&self, name: ProcessName, now: Instant) -> Option<ProcessName> {
        let mut sup = self.sup.lock().expect("sup");
        let decision = sup
            .states
            .entry(name)
            .or_insert_with(|| ChildState::new(&self.cfg.policy))
            .on_exit(now, name.is_essential(), &self.cfg.policy);
        match decision {
            Decision::Restart { at, delay } => {
                sup.pending.insert(name, at);
                self.log.push(LaunchEvent::Restarting { name, delay });
                None
            }
            Decision::CoolDown { at, delay } => {
                sup.pending.insert(name, at);
                self.log.push(LaunchEvent::CoolingDown { name, delay });
                None
            }
            Decision::GiveUp => Some(name),
        }
    }

    fn shutdown_children(&self, kids: &mut HashMap<ProcessName, Child>) -> io::Result<()> {
        // Graceful first: SIGINT (same as Ctrl-C on the child) so plippa-ui can
        // clear WS2811 + blank the TFT and plippa-server can checkpoint the WAL and
        // close its SQLite connections before the volume goes away (PWRSW_01).
        // Then SIGTERM, then kill.
        #[cfg(unix)]
        {
            for child in kids.values() {
                let pid = child.id() as i32;
                unsafe {
                    libc::kill(pid, libc::SIGINT);
                }
            }
            let graceful = Instant::now() + Duration::from_millis(GRACEFUL_STOP_MS);
            while Instant::now() < graceful {
                let mut any_alive = false;
                for child in kids.values_mut() {
                    match child.try_wait()? {
                        None => any_alive = true,
                        Some(_) => {}
                    }
                }
                if !any_alive {
                    break;
                }
                thread::sleep(Duration::from_millis(40));
            }
            for child in kids.values_mut() {
                if child.try_wait()?.is_none() {
                    let pid = child.id() as i32;
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                }
            }
            let deadline = Instant::now() + Duration::from_millis(SIGTERM_GRACE_MS);
            while Instant::now() < deadline {
                let mut any_alive = false;
                for child in kids.values_mut() {
                    match child.try_wait()? {
                        None => any_alive = true,
                        Some(_) => {}
                    }
                }
                if !any_alive {
                    break;
                }
                thread::sleep(Duration::from_millis(40));
            }
        }
        for (name, mut child) in kids.drain() {
            match child.try_wait()? {
                Some(status) => {
                    self.log.push(LaunchEvent::Stopped {
                        name,
                        code: status.code(),
                    });
                }
                None => {
                    let _ = child.kill();
                    let status = child.wait()?;
                    self.log.push(LaunchEvent::Stopped {
                        name,
                        code: status.code(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Write a human-readable line for each event (launcher stdout).
pub fn mirror_log_to_writer(log: &LaunchLog, w: &mut dyn Write) -> io::Result<()> {
    for ev in log.events() {
        match ev {
            LaunchEvent::Starting { name } => writeln!(w, "plippa: starting {}", name.as_str())?,
            LaunchEvent::Started { name, pid } => {
                writeln!(w, "plippa: started {} pid={pid}", name.as_str())?
            }
            LaunchEvent::Stopped { name, code } => {
                writeln!(w, "plippa: stopped {} code={code:?}", name.as_str())?
            }
            LaunchEvent::Restarting { name, delay } => writeln!(
                w,
                "plippa: restarting {} in {} ms",
                name.as_str(),
                delay.as_millis()
            )?,
            LaunchEvent::SpawnFailed { name, error } => {
                writeln!(w, "plippa: spawn of {} failed: {error}", name.as_str())?
            }
            LaunchEvent::CoolingDown { name, delay } => writeln!(
                w,
                "plippa: {} exhausted its crash budget; cooling down for {} s",
                name.as_str(),
                delay.as_secs()
            )?,
            LaunchEvent::GaveUp { name } => writeln!(
                w,
                "plippa: {} exhausted its crash budget; giving up",
                name.as_str()
            )?,
            LaunchEvent::Shutdown => writeln!(w, "plippa: shutdown")?,
        }
    }
    Ok(())
}

/// Test helper: spawn `/bin/sleep` stubs (or `true` for instant exit).
pub fn stub_spawn_sleep(seconds: u64) -> SpawnFn {
    Arc::new(move |_name| {
        Command::new("sleep")
            .arg(seconds.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    })
}

/// Test helper: spawn a process that exits immediately with 0.
pub fn stub_spawn_exit_ok() -> SpawnFn {
    Arc::new(move |_name| {
        Command::new("true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    })
}

/// Count how many times each process was started (for crash-restart tests).
pub fn count_starts(log: &LaunchLog, name: ProcessName) -> usize {
    log.events()
        .iter()
        .filter(|e| matches!(e, LaunchEvent::Started { name: n, .. } if *n == name))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    #[test]
    fn launcher_spawns_and_reaps_stubs() {
        let launcher = Launcher::with_spawn(LauncherConfig::default(), stub_spawn_sleep(2));
        let mut kids = launcher.start_all().expect("start");
        assert_eq!(kids.len(), ProcessName::ALL.len());
        for name in ProcessName::ALL {
            assert!(matches!(
                launcher.log().events().iter().find(|e| matches!(e, LaunchEvent::Started { name: n, .. } if *n == name)),
                Some(_)
            ));
        }
        launcher.request_shutdown();
        launcher.watch_once(&mut kids, 50).unwrap();
        assert!(launcher
            .log()
            .events()
            .iter()
            .any(|e| matches!(e, LaunchEvent::Shutdown)));
        assert!(kids.is_empty());
    }

    #[test]
    fn launcher_restarts_on_crash() {
        let starts = Arc::new(AtomicUsize::new(0));
        let starts_c = Arc::clone(&starts);
        let spawn: SpawnFn = Arc::new(move |_name| {
            starts_c.fetch_add(1, Ordering::SeqCst);
            // Exit immediately → watchdog restarts.
            Command::new("true")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
        });
        let cfg = LauncherConfig {
            children: vec![ChildSpec {
                name: ProcessName::Engine,
                program: "true".into(),
                args: vec![],
                env: vec![],
            }],
            policy: RestartPolicy {
                initial_backoff: Duration::from_millis(5),
                max_backoff: Duration::from_millis(20),
                ..RestartPolicy::default()
            },
            stop_requested: Arc::new(Mutex::new(false)),
        };
        let launcher = Launcher::with_spawn(cfg, spawn);
        let mut kids = launcher.start_all().unwrap();
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline && count_starts(launcher.log(), ProcessName::Engine) < 3 {
            launcher.watch_once(&mut kids, 5).unwrap();
        }
        assert!(
            count_starts(launcher.log(), ProcessName::Engine) >= 3,
            "expected restarts, log={:?}",
            launcher.log().events()
        );
        launcher.request_shutdown();
        launcher.watch_once(&mut kids, 20).unwrap();
    }

    // ---- SYS-14: backoff, crash budget, spawn failure, non-blocking supervision ----

    struct FakeClock {
        base: Instant,
        off: Arc<Mutex<Duration>>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                off: Arc::new(Mutex::new(Duration::ZERO)),
            }
        }
        fn clock(&self) -> ClockFn {
            let base = self.base;
            let off = Arc::clone(&self.off);
            Arc::new(move || base + *off.lock().unwrap())
        }
        fn advance(&self, d: Duration) {
            *self.off.lock().unwrap() += d;
        }
    }

    fn sleeper(_name: ProcessName) -> io::Result<Child> {
        Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    }

    fn spec(name: ProcessName) -> ChildSpec {
        ChildSpec {
            name,
            program: "sleep".into(),
            args: vec![],
            env: vec![],
        }
    }

    fn cfg(names: &[ProcessName]) -> LauncherConfig {
        LauncherConfig {
            children: names.iter().copied().map(spec).collect(),
            ..LauncherConfig::default()
        }
    }

    /// Pump the launcher (real time, tiny sleeps) until `done` holds or 5 s pass.
    fn pump(
        l: &Launcher,
        kids: &mut HashMap<ProcessName, Child>,
        mut done: impl FnMut(&Launcher) -> bool,
    ) -> WatchStatus {
        let end = Instant::now() + Duration::from_secs(5);
        let mut st = WatchStatus::Running;
        while Instant::now() < end && !done(l) {
            st = l.watch_once(kids, 1).unwrap();
            if st != WatchStatus::Running {
                return st;
            }
        }
        st
    }

    fn events_of(l: &Launcher) -> Vec<LaunchEvent> {
        l.log().events()
    }

    fn delays(l: &Launcher) -> Vec<(u64, bool)> {
        events_of(l)
            .iter()
            .filter_map(|e| match e {
                LaunchEvent::Restarting { delay, .. } => Some((delay.as_millis() as u64, false)),
                LaunchEvent::CoolingDown { delay, .. } => Some((delay.as_millis() as u64, true)),
                _ => None,
            })
            .collect()
    }

    /// Kill `name` after it ran for `run_for` of fake time and pump until the launcher has
    /// reacted (a new scheduling event, or a non-Running status).
    fn crash(
        l: &Launcher,
        fc: &FakeClock,
        kids: &mut HashMap<ProcessName, Child>,
        name: ProcessName,
        run_for: Duration,
    ) -> WatchStatus {
        let before = delays(l).len();
        fc.advance(run_for);
        kids.get_mut(&name).expect("running").kill().unwrap();
        pump(l, kids, |l| delays(l).len() > before)
    }

    fn starts(l: &Launcher, name: ProcessName) -> usize {
        count_starts(l.log(), name)
    }

    #[test]
    fn a_crashing_child_is_restarted_with_growing_backoff_and_a_budget() {
        let fc = FakeClock::new();
        let l = Launcher::with_spawn(cfg(&[ProcessName::Server]), Arc::new(sleeper))
            .with_clock(fc.clock());
        let mut kids = l.start_all().unwrap();
        let mut seen = vec![];
        for round in 0..7 {
            assert_eq!(
                crash(&l, &fc, &mut kids, ProcessName::Server, Duration::from_millis(10)),
                WatchStatus::Running
            );
            let (delay_ms, cooling) = *delays(&l).last().unwrap();
            assert!(!cooling, "round {round}");
            seen.push(delay_ms);
            // Not restarted before the delay has elapsed (no tight loop) ...
            fc.advance(Duration::from_millis(delay_ms - 1));
            let n = starts(&l, ProcessName::Server);
            l.watch_once(&mut kids, 20).unwrap();
            assert_eq!(starts(&l, ProcessName::Server), n, "restarted {delay_ms} ms early");
            assert!(!kids.contains_key(&ProcessName::Server));
            // ... and restarted once it has.
            fc.advance(Duration::from_millis(1));
            pump(&l, &mut kids, |l| starts(l, ProcessName::Server) > n);
            assert_eq!(starts(&l, ProcessName::Server), n + 1);
        }
        assert_eq!(seen, vec![1000, 2000, 4000, 8000, 16000, 30000, 30000]);
        // The eighth abnormal exit inside five minutes exhausts the server's budget.
        crash(&l, &fc, &mut kids, ProcessName::Server, Duration::from_millis(10));
        assert_eq!(delays(&l).last(), Some(&(600_000, true)));
        l.request_shutdown();
        l.watch_once(&mut kids, 5).unwrap();
    }

    #[test]
    fn a_stable_run_resets_the_backoff() {
        let fc = FakeClock::new();
        let l = Launcher::with_spawn(cfg(&[ProcessName::Server]), Arc::new(sleeper))
            .with_clock(fc.clock());
        let mut kids = l.start_all().unwrap();
        for _ in 0..3 {
            crash(&l, &fc, &mut kids, ProcessName::Server, Duration::from_millis(10));
            let (d, _) = *delays(&l).last().unwrap();
            fc.advance(Duration::from_millis(d));
            let n = starts(&l, ProcessName::Server);
            pump(&l, &mut kids, |l| starts(l, ProcessName::Server) > n);
        }
        assert_eq!(delays(&l).iter().map(|d| d.0).collect::<Vec<_>>(), [1000, 2000, 4000]);
        // The next delay would be 8 s, but this run lasted 61 s.
        crash(&l, &fc, &mut kids, ProcessName::Server, Duration::from_secs(61));
        assert_eq!(delays(&l).last(), Some(&(1000, false)));
        l.request_shutdown();
        l.watch_once(&mut kids, 5).unwrap();
    }

    #[test]
    fn budget_exhaustion_of_the_server_cools_down_while_engine_and_ui_keep_running() {
        let fc = FakeClock::new();
        let l = Launcher::with_spawn(
            cfg(&[ProcessName::Engine, ProcessName::Ui, ProcessName::Server]),
            Arc::new(sleeper),
        )
        .with_clock(fc.clock());
        let mut kids = l.start_all().unwrap();
        for i in 0..8 {
            assert_eq!(
                crash(&l, &fc, &mut kids, ProcessName::Server, Duration::from_millis(10)),
                WatchStatus::Running
            );
            if i < 7 {
                let (d, cooling) = *delays(&l).last().unwrap();
                assert!(!cooling);
                fc.advance(Duration::from_millis(d));
                let n = starts(&l, ProcessName::Server);
                pump(&l, &mut kids, |l| starts(l, ProcessName::Server) > n);
            }
        }
        assert_eq!(delays(&l).last(), Some(&(600_000, true)));
        assert!(kids.contains_key(&ProcessName::Engine));
        assert!(kids.contains_key(&ProcessName::Ui));
        assert!(!kids.contains_key(&ProcessName::Server));
        // Nothing happens for ten minutes minus a bit ...
        fc.advance(Duration::from_secs(599));
        l.watch_once(&mut kids, 20).unwrap();
        assert!(!kids.contains_key(&ProcessName::Server));
        // ... then one more attempt with a fresh budget.
        fc.advance(Duration::from_secs(2));
        let n = starts(&l, ProcessName::Server);
        pump(&l, &mut kids, |l| starts(l, ProcessName::Server) > n);
        assert!(kids.contains_key(&ProcessName::Server));
        l.request_shutdown();
        l.watch_once(&mut kids, 5).unwrap();
    }

    /// SYS-02: the LED helper is optional hardware. Its crash budget running out must not end
    /// the launcher — the engine and the UI keep running, with LEDs dark.
    #[test]
    fn the_leds_child_is_non_essential_and_cools_down() {
        assert!(!ProcessName::Leds.is_essential());
        let fc = FakeClock::new();
        let l = Launcher::with_spawn(
            cfg(&[ProcessName::Leds, ProcessName::Engine, ProcessName::Ui]),
            Arc::new(sleeper),
        )
        .with_clock(fc.clock());
        let mut kids = l.start_all().unwrap();
        for i in 0..8 {
            assert_eq!(
                crash(&l, &fc, &mut kids, ProcessName::Leds, Duration::from_millis(10)),
                WatchStatus::Running,
                "round {i}"
            );
            if i < 7 {
                let (d, cooling) = *delays(&l).last().unwrap();
                assert!(!cooling);
                fc.advance(Duration::from_millis(d));
                let n = starts(&l, ProcessName::Leds);
                pump(&l, &mut kids, |l| starts(l, ProcessName::Leds) > n);
            }
        }
        assert_eq!(delays(&l).last(), Some(&(600_000, true)));
        assert!(kids.contains_key(&ProcessName::Engine));
        assert!(kids.contains_key(&ProcessName::Ui));
        assert!(!kids.contains_key(&ProcessName::Leds));
        l.request_shutdown();
        l.watch_once(&mut kids, 5).unwrap();
    }

    #[test]
    fn budget_exhaustion_of_an_essential_child_exits_70_after_stopping_the_rest() {
        let fc = FakeClock::new();
        let l = Launcher::with_spawn(
            cfg(&[ProcessName::Engine, ProcessName::Ui, ProcessName::Server]),
            Arc::new(sleeper),
        )
        .with_clock(fc.clock());
        let mut kids = l.start_all().unwrap();
        let mut status = WatchStatus::Running;
        for _ in 0..8 {
            status = crash(&l, &fc, &mut kids, ProcessName::Engine, Duration::from_millis(10));
            if status != WatchStatus::Running {
                break;
            }
            let (d, _) = *delays(&l).last().unwrap();
            fc.advance(Duration::from_millis(d));
            let n = starts(&l, ProcessName::Engine);
            pump(&l, &mut kids, |l| starts(l, ProcessName::Engine) > n);
        }
        assert_eq!(status, WatchStatus::Exhausted(ProcessName::Engine));
        assert!(kids.is_empty(), "the remaining children are stopped first");
        assert!(events_of(&l)
            .iter()
            .any(|e| matches!(e, LaunchEvent::GaveUp { name: ProcessName::Engine })));
    }

    #[test]
    fn a_spawn_failure_is_not_a_clean_exit() {
        let fc = FakeClock::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        // First spawn works; every re-spawn fails like EMFILE / a binary being replaced.
        let spawn: SpawnFn = Arc::new(move |name| {
            if c.fetch_add(1, Ordering::SeqCst) == 0 {
                sleeper(name)
            } else {
                Err(io::Error::from_raw_os_error(24))
            }
        });
        let l = Launcher::with_spawn(cfg(&[ProcessName::Engine]), spawn).with_clock(fc.clock());
        let mut kids = l.start_all().unwrap();
        let mut status = crash(&l, &fc, &mut kids, ProcessName::Engine, Duration::from_millis(10));
        let mut guard = 0;
        while status == WatchStatus::Running {
            guard += 1;
            assert!(guard < 50, "must give up");
            let before = delays(&l).len();
            fc.advance(Duration::from_secs(31));
            status = l.watch_once(&mut kids, 3).unwrap(); // never an Err
            assert!(delays(&l).len() >= before);
        }
        assert_eq!(status, WatchStatus::Exhausted(ProcessName::Engine));
        let failed = events_of(&l)
            .iter()
            .filter(|e| matches!(e, LaunchEvent::SpawnFailed { .. }))
            .count();
        assert!(failed >= 6, "spawn failures are counted, got {failed}");
    }

    #[test]
    fn backoff_does_not_block_supervision_of_other_children_or_shutdown() {
        let fc = FakeClock::new();
        let mut c = cfg(&ProcessName::ALL);
        c.policy.initial_backoff = Duration::from_secs(30);
        let l = Launcher::with_spawn(c, Arc::new(sleeper)).with_clock(fc.clock());
        let mut kids = l.start_all().unwrap();
        let t = Instant::now();
        crash(&l, &fc, &mut kids, ProcessName::Engine, Duration::from_millis(10));
        // Engine is in a 30 s backoff; the UI dying meanwhile is still noticed at once.
        let ui_stops = |l: &Launcher| {
            events_of(l)
                .iter()
                .filter(|e| matches!(e, LaunchEvent::Stopped { name: ProcessName::Ui, .. }))
                .count()
        };
        kids.get_mut(&ProcessName::Ui).unwrap().kill().unwrap();
        pump(&l, &mut kids, |l| ui_stops(l) > 0);
        assert_eq!(ui_stops(&l), 1);
        assert!(t.elapsed() < Duration::from_secs(3), "no inline sleep for the backoff");
        // And a shutdown request is honoured at once, clearing the pending restarts.
        l.request_shutdown();
        assert_eq!(l.watch_once(&mut kids, 5).unwrap(), WatchStatus::Stopped);
        assert!(kids.is_empty());
        assert_eq!(l.pending_restarts(), 0);
        assert!(t.elapsed() < Duration::from_secs(8));
    }
}
