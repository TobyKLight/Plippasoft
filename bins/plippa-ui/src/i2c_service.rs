//! I²C service: single owner for i2c-1 (AW9523 + PN532) + encoder GPIO poll.
//!
//! See `docs/design/ui-threading.md`. The coordinator only drains channels.
//!
//! Spawn/loop helpers look unused without `--features poc2` or `cfg(test)`.

#![allow(dead_code)]

use crate::input::{InputEvent, InputSource};
pub use crate::control_roles::SwitchRoles;
use crate::switches::SwitchDebouncer;
use plippa_hw::NfcEvent;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Target expander sample period when idle / between NFC slices.
pub const EXPANDER_PERIOD: Duration = Duration::from_millis(5);
/// NFC inject slice so expander keeps sampling during "slow" ops (tests).
pub const NFC_SLICE: Duration = Duration::from_millis(2);

const CMD_CAP: usize = 16;
const INPUT_CAP: usize = 256;
const NFC_EVT_CAP: usize = 32;

/// Commands from the coordinator → I²C service.
#[derive(Debug, Clone)]
pub enum I2cCmd {
    Stop,
    PollTick,
    WriteNdef { ndef: Vec<u8> },
    SetLoadedUid(Option<Vec<u8>>),
    MarkFieldLoaded,
    /// Test-only: occupy the NFC path for `duration_ms` in [`NFC_SLICE`] steps.
    InjectSlowNfc { duration_ms: u64 },
}

/// Results from the I²C service → coordinator.
#[derive(Debug)]
pub enum NfcServiceEvent {
    Events(Vec<NfcEvent>),
    WriteResult { ok: bool, error: Option<String> },
    WorkerGone,
}

/// Hardware / test ports for the expander.
pub trait ExpanderPorts {
    fn read_ports(&mut self) -> Result<(u8, u8), String>;

    /// Wait up to `timeout` for a switch-change hint (IRQ or poll).
    /// Returns `true` when the next turn should I²C-read the ports.
    /// Default: sleep then always sample (mock / no-INT boards).
    fn wait_for_activity(&mut self, timeout: Duration) -> bool {
        thread::sleep(timeout);
        true
    }
}

/// NFC ops performed only on the I²C service thread.
pub trait NfcBusOps {
    fn poll_events(&mut self) -> Result<Vec<NfcEvent>, String>;
    fn write_first_ndef(&mut self, ndef: &[u8]) -> Result<(), String>;
    fn poll_tag_present(&mut self) -> Result<bool, String>;
    fn set_loaded_uid(&mut self, uid: Option<Vec<u8>>);
    fn mark_field_tag_as_loaded(&mut self) -> Result<(), String>;

    /// True when the next [`Self::poll_events`] will run a WUPA wake (after an empty InList).
    fn wake_armed(&self) -> bool {
        false
    }

    /// Short active probe with a known answer; `Err` means the reader is not responding
    /// correctly (an idle and a stuck reader look the same to polling).
    fn health_check(&mut self) -> Result<(), String>;

    /// Re-initialise the reader (and reopen its bus handle if needed).
    fn recover(&mut self) -> Result<(), String>;

    /// A tag is currently selected, so no probe may interrupt it.
    fn tag_active(&self) -> bool;
}

/// Seconds between health probes while the reader is idle.
const WATCHDOG_PROBE_INTERVAL: Duration = Duration::from_secs(10);
/// First retry delay after a failed recovery; doubles up to [`WATCHDOG_BACKOFF_CAP`].
const WATCHDOG_BACKOFF_START: Duration = Duration::from_secs(1);
const WATCHDOG_BACKOFF_CAP: Duration = Duration::from_secs(30);
/// Consecutive hard poll failures that trigger recovery without waiting for the probe.
const WATCHDOG_POLL_FAILURES: u32 = 3;
/// Minimum spacing between repeats of the same watchdog log line.
const WATCHDOG_LOG_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
enum WatchdogLog {
    ProbeFailed,
    Recovered,
    RecoveryFailed,
}

/// Detects a PN532 that stopped detecting tags without reporting errors, and re-initialises it.
/// Pure state machine: time is passed in (as time since an arbitrary epoch).
pub struct NfcWatchdog {
    next_probe: Option<Duration>,
    backoff: Duration,
    poll_failures: u32,
    recoveries: u32,
    last_log: [Option<Duration>; 3],
}

impl NfcWatchdog {
    pub fn new() -> Self {
        Self {
            next_probe: None,
            backoff: WATCHDOG_BACKOFF_START,
            poll_failures: 0,
            recoveries: 0,
            last_log: [None; 3],
        }
    }

    /// Number of successful recoveries so far.
    pub fn recoveries(&self) -> u32 {
        self.recoveries
    }

    /// Record the outcome of a poll: `false` is a hard failure (not an empty field).
    pub fn note_poll(&mut self, ok: bool) {
        if ok {
            self.poll_failures = 0;
        } else {
            self.poll_failures = self.poll_failures.saturating_add(1);
        }
    }

    /// Run a probe and/or recovery if one is due. Call only between NFC commands.
    /// `write_pending`: a write/associate is queued or running (nothing may run).
    /// Returns `true` when the bus was used.
    pub fn tick<N: NfcBusOps>(
        &mut self,
        now: Duration,
        write_pending: bool,
        nfc: &mut N,
        log: &mut dyn FnMut(&str),
    ) -> bool {
        if write_pending {
            return false;
        }
        let next = *self.next_probe.get_or_insert(now + WATCHDOG_PROBE_INTERVAL);
        let forced = self.poll_failures >= WATCHDOG_POLL_FAILURES;
        let tag = nfc.tag_active();
        if !forced && (tag || now < next) {
            return false;
        }

        if !forced {
            match nfc.health_check() {
                Ok(()) => {
                    self.reset_schedule(now);
                    return true;
                }
                Err(e) => self.emit(now, WatchdogLog::ProbeFailed, format!("probe failed ({e})"), log),
            }
        } else {
            let e = format!("{} consecutive poll failures", self.poll_failures);
            self.emit(now, WatchdogLog::ProbeFailed, format!("probe failed ({e})"), log);
        }

        let result = nfc.recover().and_then(|()| nfc.health_check());
        match result {
            Ok(()) => {
                self.recoveries += 1;
                self.poll_failures = 0;
                self.reset_schedule(now);
                let n = self.recoveries;
                self.emit(now, WatchdogLog::Recovered, format!("recovered after reinit (#{n})"), log);
            }
            Err(e) => {
                self.next_probe = Some(now + self.backoff);
                self.backoff = (self.backoff * 2).min(WATCHDOG_BACKOFF_CAP);
                // Retry through the probe path, not the stale poll-failure count.
                self.poll_failures = 0;
                self.emit(now, WatchdogLog::RecoveryFailed, format!("recovery failed ({e})"), log);
            }
        }
        true
    }

    fn reset_schedule(&mut self, now: Duration) {
        self.next_probe = Some(now + WATCHDOG_PROBE_INTERVAL);
        self.backoff = WATCHDOG_BACKOFF_START;
    }

    fn emit(&mut self, now: Duration, kind: WatchdogLog, msg: String, log: &mut dyn FnMut(&str)) {
        let slot = &mut self.last_log[kind as usize];
        if slot.is_some_and(|t| now.saturating_sub(t) < WATCHDOG_LOG_INTERVAL) {
            return;
        }
        *slot = Some(now);
        log(&format!("NFC watchdog: {msg}"));
    }
}

/// Optional encoder deltas (GPIO — may share the service thread).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncoderDeltas {
    pub menu: i8,
    pub volume: i8,
    /// Assignable Encoder A (0 when role absent / unwired).
    pub assignable_a: i8,
}

/// Optional encoder deltas (GPIO — may share the service thread).
pub trait EncoderSource {
    fn poll_deltas(&mut self) -> EncoderDeltas;
}

pub struct NullEncoders;

impl EncoderSource for NullEncoders {
    fn poll_deltas(&mut self) -> EncoderDeltas {
        EncoderDeltas::default()
    }
}

/// Drainable input side for the coordinator.
pub struct ChannelInput {
    rx: Receiver<InputEvent>,
}

impl ChannelInput {
    pub fn from_receiver(rx: Receiver<InputEvent>) -> Self {
        Self { rx }
    }
}

impl InputSource for ChannelInput {
    fn poll(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(ev) => out.push(ev),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        out
    }
}

/// Coordinator-facing NFC client over channels (no blocking I²C).
pub struct NfcChannelClient {
    pub(crate) cmd_tx: SyncSender<I2cCmd>,
    evt_rx: Receiver<NfcServiceEvent>,
    write_inflight: bool,
    worker_gone: bool,
}

impl NfcChannelClient {
    pub fn new(cmd_tx: SyncSender<I2cCmd>, evt_rx: Receiver<NfcServiceEvent>) -> Self {
        Self {
            cmd_tx,
            evt_rx,
            write_inflight: false,
            worker_gone: false,
        }
    }

    pub fn worker_gone(&self) -> bool {
        self.worker_gone
    }

    pub fn request_poll_tick(&mut self) {
        let _ = self.cmd_tx.try_send(I2cCmd::PollTick);
    }

    pub fn request_write(&mut self, ndef: Vec<u8>) -> bool {
        if self.write_inflight || self.worker_gone {
            return false;
        }
        match self.cmd_tx.try_send(I2cCmd::WriteNdef { ndef }) {
            Ok(()) => {
                self.write_inflight = true;
                true
            }
            Err(_) => false,
        }
    }

    pub fn set_loaded_uid(&mut self, uid: Option<Vec<u8>>) {
        let _ = self.cmd_tx.try_send(I2cCmd::SetLoadedUid(uid));
    }

    pub fn mark_field_loaded(&mut self) {
        let _ = self.cmd_tx.try_send(I2cCmd::MarkFieldLoaded);
    }

    pub fn send_stop(&mut self) {
        let _ = self.cmd_tx.send(I2cCmd::Stop);
    }

    /// Non-blocking drain of NFC service events.
    pub fn drain(&mut self) -> Vec<NfcServiceEvent> {
        let mut out = Vec::new();
        loop {
            match self.evt_rx.try_recv() {
                Ok(NfcServiceEvent::WorkerGone) => {
                    self.worker_gone = true;
                    out.push(NfcServiceEvent::WorkerGone);
                }
                Ok(NfcServiceEvent::WriteResult { ok, error }) => {
                    self.write_inflight = false;
                    out.push(NfcServiceEvent::WriteResult { ok, error });
                }
                Ok(ev) => out.push(ev),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.worker_gone = true;
                    out.push(NfcServiceEvent::WorkerGone);
                    break;
                }
            }
        }
        out
    }

    pub fn write_inflight(&self) -> bool {
        self.write_inflight
    }
}

fn push_input(tx: &SyncSender<InputEvent>, ev: InputEvent, overflow_logged: &mut bool) {
    match tx.try_send(ev) {
        Ok(()) => {
            *overflow_logged = false;
        }
        Err(TrySendError::Full(ev)) => {
            let droppable = matches!(
                ev,
                InputEvent::MenuRotate(_)
                    | InputEvent::VolumeRotate(_)
                    | InputEvent::AssignableARotate(_)
            );
            if !*overflow_logged {
                if droppable {
                    eprintln!("plippa-ui: input queue full — dropping encoder delta");
                } else {
                    eprintln!("plippa-ui: input queue full — dropping button edge");
                }
                *overflow_logged = true;
            }
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

fn push_nfc(tx: &SyncSender<NfcServiceEvent>, ev: NfcServiceEvent) {
    if let Err(TrySendError::Full(ev)) = tx.try_send(ev) {
        // Cannot drain from sender; drop this event (prefer not blocking service).
        eprintln!("plippa-ui: NFC event queue full — dropping event");
        let _ = ev;
    }
}

fn sample_inputs<E, Enc>(
    expander: &mut E,
    encoders: &mut Enc,
    debouncer: &mut SwitchDebouncer,
    roles: &SwitchRoles,
    input_tx: &SyncSender<InputEvent>,
    now: Instant,
    epoch: Instant,
    overflow_logged: &mut bool,
    read_fail_logged: &mut bool,
    read_expander: bool,
) where
    E: ExpanderPorts,
    Enc: EncoderSource,
{
    if read_expander {
        match expander.read_ports() {
            Ok((p0, p1)) => {
                *read_fail_logged = false;
                for sev in debouncer.poll_events_at(p0, p1, now) {
                    // WARP_06: edge time on the loop epoch (for release timing).
                    let at_ms = now.saturating_duration_since(epoch).as_millis() as u64;
                    for iev in roles.map_switch_at(sev, at_ms) {
                        push_input(input_tx, iev, overflow_logged);
                    }
                }
            }
            Err(e) => {
                if !*read_fail_logged {
                    eprintln!("plippa-ui: AW9523 read_ports failed: {e}");
                    *read_fail_logged = true;
                }
            }
        }
    }
    let deltas = encoders.poll_deltas();
    if deltas.menu != 0 {
        push_input(input_tx, InputEvent::MenuRotate(deltas.menu), overflow_logged);
    }
    if deltas.volume != 0 {
        push_input(
            input_tx,
            InputEvent::VolumeRotate(deltas.volume),
            overflow_logged,
        );
    }
    if deltas.assignable_a != 0 {
        push_input(
            input_tx,
            InputEvent::AssignableARotate(deltas.assignable_a),
            overflow_logged,
        );
    }
}

fn run_nfc_cmd<N>(
    nfc: &mut Option<N>,
    nfc_tx: &SyncSender<NfcServiceEvent>,
    cmd: I2cCmd,
    slow_nfc_remaining_ms: &mut u64,
    watchdog: &mut NfcWatchdog,
) -> bool
where
    N: NfcBusOps,
{
    match cmd {
        I2cCmd::Stop => return false,
        I2cCmd::InjectSlowNfc { duration_ms } => {
            *slow_nfc_remaining_ms = duration_ms.max(1);
        }
        I2cCmd::SetLoadedUid(uid) => {
            if let Some(n) = nfc.as_mut() {
                n.set_loaded_uid(uid);
            }
        }
        I2cCmd::MarkFieldLoaded => {
            if let Some(n) = nfc.as_mut() {
                if let Err(e) = n.mark_field_tag_as_loaded() {
                    eprintln!("plippa-ui: mark_field_loaded: {e}");
                }
            }
        }
        I2cCmd::WriteNdef { ndef } => {
            let result = match nfc.as_mut() {
                None => NfcServiceEvent::WriteResult {
                    ok: false,
                    error: Some("no NFC device".into()),
                },
                Some(n) => match n.poll_tag_present() {
                    Ok(false) => NfcServiceEvent::WriteResult {
                        ok: false,
                        error: Some("no tag in field".into()),
                    },
                    Ok(true) => match n.write_first_ndef(&ndef) {
                        Ok(()) => NfcServiceEvent::WriteResult {
                            ok: true,
                            error: None,
                        },
                        Err(e) => NfcServiceEvent::WriteResult {
                            ok: false,
                            error: Some(e),
                        },
                    },
                    Err(e) => NfcServiceEvent::WriteResult {
                        ok: false,
                        error: Some(e),
                    },
                },
            };
            push_nfc(nfc_tx, result);
        }
        I2cCmd::PollTick => {
            if let Some(n) = nfc.as_mut() {
                match n.poll_events() {
                    Ok(events) => {
                        watchdog.note_poll(true);
                        if !events.is_empty() {
                            push_nfc(nfc_tx, NfcServiceEvent::Events(events));
                        }
                    }
                    Err(e) => {
                        watchdog.note_poll(false);
                        eprintln!("plippa-ui: NFC poll error: {e}");
                    }
                }
            } else {
                eprintln!("plippa-ui: PollTick but no NFC device on I²C service");
            }
        }
    }
    true
}

/// One service turn: expander first, then at most one NFC slice/cmd.
pub fn service_turn<E, N, Enc>(
    expander: &mut E,
    nfc: &mut Option<N>,
    encoders: &mut Enc,
    debouncer: &mut SwitchDebouncer,
    roles: &SwitchRoles,
    input_tx: &SyncSender<InputEvent>,
    nfc_tx: &SyncSender<NfcServiceEvent>,
    cmd: Option<I2cCmd>,
    slow_nfc_remaining_ms: &mut u64,
    now: Instant,
    epoch: Instant,
    overflow_logged: &mut bool,
) -> bool
where
    E: ExpanderPorts,
    N: NfcBusOps,
    Enc: EncoderSource,
{
    let mut read_fail_logged = false;
    sample_inputs(
        expander,
        encoders,
        debouncer,
        roles,
        input_tx,
        now,
        epoch,
        overflow_logged,
        &mut read_fail_logged,
        true,
    );

    if *slow_nfc_remaining_ms > 0 {
        let step = NFC_SLICE.as_millis() as u64;
        *slow_nfc_remaining_ms = slow_nfc_remaining_ms.saturating_sub(step);
        if *slow_nfc_remaining_ms == 0 {
            push_nfc(nfc_tx, NfcServiceEvent::Events(Vec::new()));
        }
        return true;
    }

    match cmd {
        Some(c) => run_nfc_cmd(nfc, nfc_tx, c, slow_nfc_remaining_ms, &mut NfcWatchdog::new()),
        None => true,
    }
}

/// Minimum pure expander turns between blocking NFC poll/write.
/// At 5 ms cadence this is ≥100 ms of switch sampling before each field check.
const EXPANDER_TURNS_BEFORE_NFC: u32 = 20;
/// Safety net if INT is miswired: still I²C-sample ports this often.
const FORCE_PORT_READ: Duration = Duration::from_millis(100);

/// Run the service loop until Stop / stop_flag / cmd disconnect.
pub fn run_i2c_service_loop<E, N, Enc>(
    mut expander: E,
    mut nfc: Option<N>,
    mut encoders: Enc,
    roles: SwitchRoles,
    debounce_ms: u64,
    cmd_rx: Receiver<I2cCmd>,
    input_tx: SyncSender<InputEvent>,
    nfc_tx: SyncSender<NfcServiceEvent>,
    stop_flag: Option<Arc<AtomicBool>>,
    epoch: Instant,
) where
    E: ExpanderPorts,
    N: NfcBusOps,
    Enc: EncoderSource,
{
    let mut debouncer = SwitchDebouncer::new(debounce_ms);
    let mut slow_nfc_remaining_ms = 0u64;
    let mut overflow_logged = false;
    let mut read_fail_logged = false;
    let mut expander_turns_since_nfc = EXPANDER_TURNS_BEFORE_NFC;
    let mut pending_nfc: Option<I2cCmd> = None;
    let mut watchdog = NfcWatchdog::new();
    let mut running = true;
    let mut read_expander = true;
    let mut last_port_read = Instant::now();
    // Prove the expander path once at start (helps desk debug).
    match expander.read_ports() {
        Ok((p0, p1)) => {
            eprintln!("plippa-ui: I²C service up — AW9523 ports p0=0x{p0:02x} p1=0x{p1:02x}");
            last_port_read = Instant::now();
        }
        Err(e) => eprintln!("plippa-ui: I²C service AW9523 read failed at start: {e}"),
    }

    while running {
        if let Some(flag) = &stop_flag {
            if flag.load(Ordering::Relaxed) {
                break;
            }
        }

        let turn_start = Instant::now();

        // Drain cmds: Stop wins; keep at most one deferred NFC op (write > poll).
        loop {
            match cmd_rx.try_recv() {
                Ok(I2cCmd::Stop) => {
                    running = false;
                    break;
                }
                Ok(I2cCmd::PollTick) => {
                    if !matches!(pending_nfc, Some(I2cCmd::WriteNdef { .. })) {
                        pending_nfc = Some(I2cCmd::PollTick);
                    }
                }
                Ok(cmd @ I2cCmd::WriteNdef { .. }) => {
                    pending_nfc = Some(cmd);
                }
                Ok(other) => {
                    // Lightweight cmds run after this expander sample.
                    pending_nfc = Some(other);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    running = false;
                    break;
                }
            }
        }
        if !running {
            break;
        }

        let do_port_read = read_expander || last_port_read.elapsed() >= FORCE_PORT_READ;
        sample_inputs(
            &mut expander,
            &mut encoders,
            &mut debouncer,
            &roles,
            &input_tx,
            Instant::now(),
            epoch,
            &mut overflow_logged,
            &mut read_fail_logged,
            do_port_read,
        );
        if do_port_read {
            last_port_read = Instant::now();
        }
        expander_turns_since_nfc = expander_turns_since_nfc.saturating_add(1);

        let mut force_read_after_nfc = false;
        if slow_nfc_remaining_ms > 0 {
            let step = NFC_SLICE.as_millis() as u64;
            slow_nfc_remaining_ms = slow_nfc_remaining_ms.saturating_sub(step);
            if slow_nfc_remaining_ms == 0 {
                push_nfc(&nfc_tx, NfcServiceEvent::Events(Vec::new()));
            }
        } else if let Some(cmd) = pending_nfc.take() {
            let is_poll = matches!(cmd, I2cCmd::PollTick);
            let is_blocking = matches!(
                cmd,
                I2cCmd::PollTick | I2cCmd::WriteNdef { .. } | I2cCmd::InjectSlowNfc { .. }
            );
            if is_blocking && expander_turns_since_nfc < EXPANDER_TURNS_BEFORE_NFC {
                // Keep switches snappy: defer long NFC until we've had a few expander turns.
                pending_nfc = Some(cmd);
            } else {
                if is_blocking {
                    expander_turns_since_nfc = 0;
                }
                running = run_nfc_cmd(
                    &mut nfc,
                    &nfc_tx,
                    cmd,
                    &mut slow_nfc_remaining_ms,
                    &mut watchdog,
                );
                // Catch a press held through the NFC critical section ASAP.
                sample_inputs(
                    &mut expander,
                    &mut encoders,
                    &mut debouncer,
                    &roles,
                    &input_tx,
                    Instant::now(),
                    epoch,
                    &mut overflow_logged,
                    &mut read_fail_logged,
                    true,
                );
                last_port_read = Instant::now();

                // InList miss arms WUPA — run wake half now (after expander sample) so
                // HALT tags are seen without waiting for the next 200 ms PollTick.
                if running && is_poll && nfc.as_ref().is_some_and(|n| n.wake_armed()) {
                    running =
                        run_nfc_cmd(
                            &mut nfc,
                            &nfc_tx,
                            I2cCmd::PollTick,
                            &mut slow_nfc_remaining_ms,
                            &mut watchdog,
                        );
                    sample_inputs(
                        &mut expander,
                        &mut encoders,
                        &mut debouncer,
                        &roles,
                        &input_tx,
                        Instant::now(),
                        epoch,
                        &mut overflow_logged,
                        &mut read_fail_logged,
                        true,
                    );
                    last_port_read = Instant::now();
                }
                force_read_after_nfc = true;
            }
        } else if let Some(n) = nfc.as_mut() {
            // Idle turn: no NFC command ran or is queued. Same pacing as other blocking NFC work.
            if expander_turns_since_nfc >= EXPANDER_TURNS_BEFORE_NFC
                && watchdog.tick(
                    epoch.elapsed(),
                    false,
                    n,
                    &mut |m| eprintln!("plippa-ui: {m}"),
                )
            {
                expander_turns_since_nfc = 0;
                sample_inputs(
                    &mut expander,
                    &mut encoders,
                    &mut debouncer,
                    &roles,
                    &input_tx,
                    Instant::now(),
                    epoch,
                    &mut overflow_logged,
                    &mut read_fail_logged,
                    true,
                );
                last_port_read = Instant::now();
                force_read_after_nfc = true;
            }
        }

        if !running {
            break;
        }

        let elapsed = turn_start.elapsed();
        if elapsed < EXPANDER_PERIOD {
            let wait = EXPANDER_PERIOD - elapsed;
            // IRQ (or poll) wait; also peek cmds so Stop / PollTick are not stuck behind INT silence.
            let deadline = Instant::now() + wait;
            let mut woke_irq = false;
            loop {
                let remain = deadline.saturating_duration_since(Instant::now());
                if remain.is_zero() {
                    break;
                }
                // Chunk so mpsc cmds are noticed without a dedicated fd.
                let slice = remain.min(Duration::from_millis(2));
                if expander.wait_for_activity(slice) {
                    woke_irq = true;
                    break;
                }
                match cmd_rx.try_recv() {
                    Ok(I2cCmd::Stop) => {
                        running = false;
                        break;
                    }
                    Ok(I2cCmd::PollTick) => {
                        if !matches!(pending_nfc, Some(I2cCmd::WriteNdef { .. })) {
                            pending_nfc = Some(I2cCmd::PollTick);
                        }
                        break;
                    }
                    Ok(cmd @ I2cCmd::WriteNdef { .. }) => {
                        pending_nfc = Some(cmd);
                        break;
                    }
                    Ok(other) => {
                        pending_nfc = Some(other);
                        break;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => {
                        running = false;
                        break;
                    }
                }
            }
            read_expander = woke_irq || force_read_after_nfc;
        } else {
            read_expander = true;
        }
    }
}

/// Handle returned to the coordinator after spawn.
pub struct I2cServiceHandle {
    input: Option<ChannelInput>,
    nfc: Option<NfcChannelClient>,
    join: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    cmd_tx: SyncSender<I2cCmd>,
}

impl I2cServiceHandle {
    /// Split input/NFC endpoints for the coordinator; keep this handle for join.
    pub fn take_endpoints(&mut self) -> (ChannelInput, NfcChannelClient) {
        (
            self.input.take().expect("I²C input already taken"),
            self.nfc.take().expect("I²C NFC client already taken"),
        )
    }

    pub fn stop_and_join(mut self, timeout: Duration) {
        self.stop_flag.store(true, Ordering::SeqCst);
        // `stop_flag` is the reliable stop (the worker checks it every slice); the command is
        // only an early wake, so never block the caller waiting for queue space.
        let _ = self.cmd_tx.try_send(I2cCmd::Stop);
        if let Some(client) = self.nfc.as_mut() {
            client.send_stop();
        }
        if let Some(h) = self.join.take() {
            let start = Instant::now();
            loop {
                if h.is_finished() {
                    let _ = h.join();
                    break;
                }
                if start.elapsed() >= timeout {
                    eprintln!("plippa-ui: I²C service join timed out after {timeout:?}");
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

impl Drop for I2cServiceHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.send(I2cCmd::Stop);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// Spawn a service thread with the given backends (used by poc2 + tests).
pub fn spawn_i2c_service<E, N, Enc>(
    expander: E,
    nfc: Option<N>,
    encoders: Enc,
    roles: SwitchRoles,
    debounce_ms: u64,
    stop_flag: Arc<AtomicBool>,
    epoch: Instant,
) -> I2cServiceHandle
where
    E: ExpanderPorts + Send + 'static,
    N: NfcBusOps + Send + 'static,
    Enc: EncoderSource + Send + 'static,
{
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<I2cCmd>(CMD_CAP);
    let (input_tx, input_rx) = mpsc::sync_channel::<InputEvent>(INPUT_CAP);
    let (nfc_tx, nfc_rx) = mpsc::sync_channel::<NfcServiceEvent>(NFC_EVT_CAP);
    let flag = Arc::clone(&stop_flag);
    let nfc_tx_thread = nfc_tx.clone();
    let cmd_tx_handle = cmd_tx.clone();

    let join = thread::Builder::new()
        .name("plippa-ui-i2c".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_i2c_service_loop(
                    expander,
                    nfc,
                    encoders,
                    roles,
                    debounce_ms,
                    cmd_rx,
                    input_tx,
                    nfc_tx_thread.clone(),
                    Some(flag),
                    epoch,
                );
            }));
            if result.is_err() {
                eprintln!("plippa-ui: I²C service panicked");
                let _ = nfc_tx_thread.try_send(NfcServiceEvent::WorkerGone);
            }
        })
        .expect("spawn I²C service");

    I2cServiceHandle {
        input: Some(ChannelInput::from_receiver(input_rx)),
        nfc: Some(NfcChannelClient::new(cmd_tx, nfc_rx)),
        join: Some(join),
        stop_flag,
        cmd_tx: cmd_tx_handle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::switches::SwitchEvent;
    use std::sync::{Arc, Mutex};

    struct FakeExpander {
        ports: Arc<Mutex<(u8, u8)>>,
    }

    impl ExpanderPorts for FakeExpander {
        fn read_ports(&mut self) -> Result<(u8, u8), String> {
            Ok(*self.ports.lock().unwrap())
        }
    }

    struct FakeNfc {
        slow_ms: Arc<Mutex<u64>>,
        events: Vec<NfcEvent>,
    }

    impl NfcBusOps for FakeNfc {
        fn poll_events(&mut self) -> Result<Vec<NfcEvent>, String> {
            let ms = *self.slow_ms.lock().unwrap();
            if ms > 0 {
                // Simulate blocking poll — tests use InjectSlowNfc instead.
                thread::sleep(Duration::from_millis(ms));
            }
            Ok(std::mem::take(&mut self.events))
        }

        fn write_first_ndef(&mut self, _ndef: &[u8]) -> Result<(), String> {
            Ok(())
        }

        fn poll_tag_present(&mut self) -> Result<bool, String> {
            Ok(true)
        }

        fn set_loaded_uid(&mut self, _uid: Option<Vec<u8>>) {}

        fn mark_field_tag_as_loaded(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn health_check(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn recover(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn tag_active(&self) -> bool {
            false
        }
    }

    /// Roles resolved from the real plippa-D-poc2 profile (no pin names here).
    fn roles_play_bt_sw3() -> SwitchRoles {
        crate::control_roles::tests_support::poc2_roles()
    }

    /// Raw expander ports (active-low) with the Play role's switch held down.
    fn play_pressed_ports() -> (u8, u8) {
        let roles = roles_play_bt_sw3();
        let (port, bit) = roles
            .switch_for(plippa_board::ControlRole::Play)
            .unwrap()
            .port_bit();
        let mut p = [0xFFu8, 0xFF];
        p[port as usize] &= !bit;
        (p[0], p[1])
    }

    #[test]
    fn i2c_service_stop_flag_joins_service_thread() {
        let ports = Arc::new(Mutex::new((0xFFu8, 0xFFu8)));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_i2c_service(
            FakeExpander {
                ports: Arc::clone(&ports),
            },
            None::<FakeNfc>,
            NullEncoders,
            roles_play_bt_sw3(),
            20,
            Arc::clone(&stop),
            Instant::now(),
        );
        handle.stop_and_join(Duration::from_secs(2));
    }

    #[test]
    fn input_events_during_slow_nfc() {
        // Idle ports then press the Play role's switch while InjectSlowNfc runs.
        let ports = Arc::new(Mutex::new((0xFFu8, 0xFFu8)));
        let stop = Arc::new(AtomicBool::new(false));
        let mut handle = spawn_i2c_service(
            FakeExpander {
                ports: Arc::clone(&ports),
            },
            Some(FakeNfc {
                slow_ms: Arc::new(Mutex::new(0)),
                events: vec![],
            }),
            NullEncoders,
            roles_play_bt_sw3(),
            20,
            Arc::clone(&stop),
            Instant::now(),
        );
        let (mut input, nfc) = handle.take_endpoints();

        // Start a 50 ms injected NFC occupancy (sliced at 2 ms).
        nfc.cmd_tx
            .send(I2cCmd::InjectSlowNfc { duration_ms: 50 })
            .unwrap();
        thread::sleep(Duration::from_millis(5));

        // Press Play while "NFC" is in flight.
        {
            let mut p = ports.lock().unwrap();
            *p = play_pressed_ports();
        }
        let pressed_at = Instant::now();

        // Coordinator drains until PlayStop within 15 ms of first service sample.
        let deadline = pressed_at + Duration::from_millis(15);
        let mut saw_play = false;
        while Instant::now() < deadline + Duration::from_millis(30) {
            for ev in input.poll() {
                if matches!(ev, InputEvent::ControlButton { down: true, .. }) {
                    saw_play = true;
                    let lag = pressed_at.elapsed();
                    assert!(
                        lag <= Duration::from_millis(15),
                        "PlayStop lag {lag:?} exceeded 15ms"
                    );
                    break;
                }
            }
            if saw_play {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(saw_play, "expected PlayStop during slow NFC inject");
        drop(input);
        drop(nfc);
        handle.stop_and_join(Duration::from_secs(2));
    }

    #[test]
    fn role_mapping_emits_play_edge_for_bt_sw3() {
        let roles = roles_play_bt_sw3();
        let play = roles.switch_for(plippa_board::ControlRole::Play).unwrap();
        let ev = roles.map_switch(SwitchEvent::Pressed(play));
        assert!(matches!(
            ev[0],
            InputEvent::ControlButton { role: plippa_board::ControlRole::Play, down: true, .. }
        ));
    }

    #[test]
    fn mock_path_no_i2c_thread_required_for_channel_input() {
        // ChannelInput works without a live service (disconnected = empty poll).
        let (_tx, rx) = mpsc::sync_channel::<InputEvent>(4);
        let mut input = ChannelInput::from_receiver(rx);
        assert!(input.poll().is_empty());
    }

    #[test]
    fn nfc_channel_write_result_clears_inflight() {
        let (cmd_tx, _cmd_rx) = mpsc::sync_channel::<I2cCmd>(4);
        let (nfc_tx, nfc_rx) = mpsc::sync_channel::<NfcServiceEvent>(4);
        let mut client = NfcChannelClient::new(cmd_tx, nfc_rx);
        assert!(client.request_write(vec![1, 2, 3]));
        assert!(client.write_inflight());
        nfc_tx
            .send(NfcServiceEvent::WriteResult {
                ok: true,
                error: None,
            })
            .unwrap();
        let ev = client.drain();
        assert!(matches!(
            ev.as_slice(),
            [NfcServiceEvent::WriteResult { ok: true, .. }]
        ));
        assert!(!client.write_inflight());
    }

    #[test]
    fn service_turn_delivers_press_while_slow_nfc_slices() {
        let mut expander = FakeExpander {
            ports: Arc::new(Mutex::new(play_pressed_ports())),
        };
        let mut nfc: Option<FakeNfc> = None;
        let mut enc = NullEncoders;
        let mut debouncer = SwitchDebouncer::new(20);
        // Prime released state first.
        {
            *expander.ports.lock().unwrap() = (0xFF, 0xFF);
        }
        let (input_tx, input_rx) = mpsc::sync_channel(32);
        let (nfc_tx, _nfc_rx) = mpsc::sync_channel(8);
        let roles = roles_play_bt_sw3();
        let mut slow = 0u64;
        let mut overflow = false;
        let t0 = Instant::now();
        service_turn(
            &mut expander,
            &mut nfc,
            &mut enc,
            &mut debouncer,
            &roles,
            &input_tx,
            &nfc_tx,
            Some(I2cCmd::InjectSlowNfc { duration_ms: 40 }),
            &mut slow,
            t0,
            t0,
            &mut overflow,
        );
        assert_eq!(slow, 40);
        // Press while slices run.
        *expander.ports.lock().unwrap() = play_pressed_ports();
        service_turn(
            &mut expander,
            &mut nfc,
            &mut enc,
            &mut debouncer,
            &roles,
            &input_tx,
            &nfc_tx,
            None,
            &mut slow,
            t0 + Duration::from_millis(2),
            t0,
            &mut overflow,
        );
        let events: Vec<_> = input_rx.try_iter().collect();
        assert!(
            events.iter().any(|e| matches!(e, InputEvent::ControlButton { down: true, .. })),
            "expected the Play press edge in {events:?}"
        );
    }

    /// Scripted reader for watchdog tests: pops one result per call, then defaults to Ok.
    #[derive(Default)]
    struct WdNfc {
        probes: std::collections::VecDeque<Result<(), String>>,
        recovers: std::collections::VecDeque<Result<(), String>>,
        tag: bool,
        probe_calls: u32,
        recover_calls: u32,
    }

    impl NfcBusOps for WdNfc {
        fn poll_events(&mut self) -> Result<Vec<NfcEvent>, String> {
            Ok(Vec::new())
        }
        fn write_first_ndef(&mut self, _: &[u8]) -> Result<(), String> {
            Ok(())
        }
        fn poll_tag_present(&mut self) -> Result<bool, String> {
            Ok(false)
        }
        fn set_loaded_uid(&mut self, _: Option<Vec<u8>>) {}
        fn mark_field_tag_as_loaded(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn health_check(&mut self) -> Result<(), String> {
            self.probe_calls += 1;
            self.probes.pop_front().unwrap_or(Ok(()))
        }
        fn recover(&mut self) -> Result<(), String> {
            self.recover_calls += 1;
            self.recovers.pop_front().unwrap_or(Ok(()))
        }
        fn tag_active(&self) -> bool {
            self.tag
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn tick(wd: &mut NfcWatchdog, now: u64, nfc: &mut WdNfc, lines: &mut Vec<String>) -> bool {
        wd.tick(secs(now), false, nfc, &mut |m| lines.push(m.to_string()))
    }

    #[test]
    fn watchdog_healthy_reader_is_probed_without_recovery_or_logs() {
        let (mut wd, mut nfc, mut lines) = (NfcWatchdog::new(), WdNfc::default(), vec![]);
        assert!(!tick(&mut wd, 0, &mut nfc, &mut lines)); // schedules first probe
        assert!(!tick(&mut wd, 9, &mut nfc, &mut lines));
        assert!(tick(&mut wd, 10, &mut nfc, &mut lines));
        assert_eq!((nfc.probe_calls, nfc.recover_calls), (1, 0));
        assert!(!tick(&mut wd, 15, &mut nfc, &mut lines));
        assert!(tick(&mut wd, 20, &mut nfc, &mut lines));
        assert_eq!((nfc.probe_calls, nfc.recover_calls), (2, 0));
        assert!(lines.is_empty());
    }

    #[test]
    fn watchdog_failed_probe_recovers_once_then_reprobes() {
        let (mut wd, mut nfc, mut lines) = (NfcWatchdog::new(), WdNfc::default(), vec![]);
        nfc.probes.push_back(Err("echo mismatch".into()));
        tick(&mut wd, 0, &mut nfc, &mut lines);
        tick(&mut wd, 10, &mut nfc, &mut lines);
        assert_eq!(nfc.recover_calls, 1);
        assert_eq!(nfc.probe_calls, 2); // failed probe + re-probe after recover
        assert_eq!(wd.recoveries(), 1);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("probe failed (echo mismatch)"));
        assert!(lines[1].contains("recovered after reinit (#1)"));
    }

    #[test]
    fn watchdog_failed_recovery_backs_off_then_retries() {
        let (mut wd, mut nfc, mut lines) = (NfcWatchdog::new(), WdNfc::default(), vec![]);
        for _ in 0..3 {
            nfc.probes.push_back(Err("dead".into()));
            nfc.recovers.push_back(Err("still dead".into()));
        }
        tick(&mut wd, 0, &mut nfc, &mut lines);
        tick(&mut wd, 10, &mut nfc, &mut lines); // fail, retry in 1 s
        assert_eq!(nfc.recover_calls, 1);
        assert!(!tick(&mut wd, 10, &mut nfc, &mut lines));
        tick(&mut wd, 11, &mut nfc, &mut lines); // fail, retry in 2 s
        assert_eq!(nfc.recover_calls, 2);
        assert!(!tick(&mut wd, 12, &mut nfc, &mut lines));
        tick(&mut wd, 13, &mut nfc, &mut lines); // third failure; next one recovers
        assert_eq!(nfc.recover_calls, 3);
        nfc.probes.push_back(Err("dead".into())); // recover now succeeds (default)
        tick(&mut wd, 17, &mut nfc, &mut lines);
        assert_eq!(wd.recoveries(), 1);
        assert!(lines.iter().any(|l| l.contains("recovery failed (still dead)")));
        assert!(lines.last().unwrap().contains("recovered after reinit (#1)"));
    }

    #[test]
    fn watchdog_backoff_is_capped() {
        let (mut wd, mut nfc, mut lines) = (NfcWatchdog::new(), WdNfc::default(), vec![]);
        for _ in 0..12 {
            nfc.probes.push_back(Err("dead".into()));
            nfc.recovers.push_back(Err("dead".into()));
        }
        tick(&mut wd, 0, &mut nfc, &mut lines);
        let mut now = 10;
        for _ in 0..8 {
            tick(&mut wd, now, &mut nfc, &mut lines);
            now = wd.next_probe.unwrap().as_secs();
        }
        let before = wd.next_probe.unwrap();
        tick(&mut wd, before.as_secs(), &mut nfc, &mut lines);
        assert_eq!(wd.next_probe.unwrap() - before, WATCHDOG_BACKOFF_CAP);
    }

    #[test]
    fn watchdog_skips_probe_while_tag_active_or_write_pending() {
        let (mut wd, mut nfc, mut lines) = (NfcWatchdog::new(), WdNfc::default(), vec![]);
        tick(&mut wd, 0, &mut nfc, &mut lines);
        nfc.tag = true;
        assert!(!tick(&mut wd, 30, &mut nfc, &mut lines));
        nfc.tag = false;
        assert!(!wd.tick(secs(30), true, &mut nfc, &mut |_| {}));
        assert_eq!(nfc.probe_calls, 0);
        assert!(tick(&mut wd, 30, &mut nfc, &mut lines));
        assert_eq!(nfc.probe_calls, 1);
    }

    #[test]
    fn watchdog_hard_poll_failures_trigger_recovery_without_probe_due() {
        let (mut wd, mut nfc, mut lines) = (NfcWatchdog::new(), WdNfc::default(), vec![]);
        tick(&mut wd, 0, &mut nfc, &mut lines);
        wd.note_poll(false);
        wd.note_poll(false);
        wd.note_poll(true); // a good poll resets the run
        wd.note_poll(false);
        wd.note_poll(false);
        assert!(!tick(&mut wd, 1, &mut nfc, &mut lines));
        wd.note_poll(false);
        assert!(tick(&mut wd, 1, &mut nfc, &mut lines));
        assert_eq!(nfc.recover_calls, 1);
        assert_eq!(wd.recoveries(), 1);
        // Forced recovery works even with a tag reported active.
        nfc.tag = true;
        for _ in 0..3 {
            wd.note_poll(false);
        }
        tick(&mut wd, 100, &mut nfc, &mut lines);
        assert_eq!(nfc.recover_calls, 2);
    }

    #[test]
    fn watchdog_log_lines_are_rate_limited_but_counter_keeps_counting() {
        let (mut wd, mut nfc, mut lines) = (NfcWatchdog::new(), WdNfc::default(), vec![]);
        tick(&mut wd, 0, &mut nfc, &mut lines);
        for t in [10, 20, 30] {
            nfc.probes.push_back(Err("flap".into()));
            tick(&mut wd, t, &mut nfc, &mut lines);
        }
        assert_eq!(wd.recoveries(), 3);
        assert_eq!(lines.len(), 2); // one probe-failed + one recovered inside the first minute
        nfc.probes.push_back(Err("flap".into()));
        tick(&mut wd, 90, &mut nfc, &mut lines);
        assert!(lines.last().unwrap().contains("(#4)"));
    }
}
