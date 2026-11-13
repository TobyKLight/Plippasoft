//! LED worker: owns WS2811 / cue backend; coordinator only sends commands (MVP_16).
//!
//! Worker self-paces at ~30 FPS. Mock backends stay synchronous — do not spawn
//! this worker on the CI mock path.

use crate::led::{LedCue, PlayButtonLed, Rgb, TransportPulse};
use crate::record_leds::RecordFrame;
use plippa_board::LedRole;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CMD_CAP: usize = 32;
const FRAME_INTERVAL: Duration = Duration::from_millis(33); // ~30 FPS

/// Commands from coordinator → LED worker.
#[derive(Debug, Clone, PartialEq)]
pub enum LedCmd {
    OnTapFeedback(u64),
    SetPlayButton(PlayButtonLed),
    /// Steady colour for a LED role span (`None` = off).
    SetRole(LedRole, Option<Rgb>),
    /// Animated pulse for a role span (`None` = no pulse), painted over the steady colour
    /// every frame (WARP_21 FF/RW speed pulse).
    SetRolePulse(LedRole, Option<TransportPulse>),
    /// Timed press flash of a role (NB_14).
    FlashRole(LedRole, Rgb),
    /// ART_10: cover colours on the NFC ring at an angle (`None` = ring back to normal).
    SetRecordArt(Option<RecordFrame>),
    ClearAll,
    Stop,
}

/// Handle for join / stop on UI shutdown.
pub struct LedWorkerHandle {
    join: Option<JoinHandle<()>>,
    cmd_tx: SyncSender<LedCmd>,
    cleared: Arc<AtomicBool>,
    /// LED roles the backend has (snapshot taken before it moved to the worker).
    roles: Arc<BTreeSet<LedRole>>,
}

impl LedWorkerHandle {
    pub fn proxy(&self) -> LedProxy {
        LedProxy {
            cmd_tx: self.cmd_tx.clone(),
            roles: Arc::clone(&self.roles),
        }
    }

    pub fn cleared_on_stop(&self) -> bool {
        self.cleared.load(Ordering::SeqCst)
    }

    pub fn stop_and_join(mut self, timeout: Duration) {
        // Best-effort and non-blocking: a full queue means the worker is not draining, and
        // blocking here would hang shutdown instead of letting the timeout below report it.
        let _ = self.cmd_tx.try_send(LedCmd::ClearAll);
        let _ = self.cmd_tx.try_send(LedCmd::Stop);
        if let Some(h) = self.join.take() {
            let start = Instant::now();
            loop {
                if h.is_finished() {
                    let _ = h.join();
                    break;
                }
                if start.elapsed() >= timeout {
                    eprintln!("plippa-ui: LED worker join timed out after {timeout:?}");
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

impl Drop for LedWorkerHandle {
    fn drop(&mut self) {
        // Non-blocking for the same reason as `stop_and_join`; Drop must not hang exit.
        let _ = self.cmd_tx.try_send(LedCmd::ClearAll);
        let _ = self.cmd_tx.try_send(LedCmd::Stop);
        if let Some(h) = self.join.take() {
            let _ = h.join();
        }
    }
}

/// Non-blocking LED sink for the coordinator.
pub struct LedProxy {
    cmd_tx: SyncSender<LedCmd>,
    roles: Arc<BTreeSet<LedRole>>,
}

impl LedProxy {
    /// `false` when the command was dropped because the queue was full.
    fn try_cmd(&self, cmd: LedCmd) -> bool {
        match self.cmd_tx.try_send(cmd) {
            Ok(()) => true,
            Err(TrySendError::Full(cmd)) => {
                // Prefer Stop / ClearAll / play state over dropping them silently, but never
                // block the coordinator: a full queue means the worker is not draining.
                match cmd {
                    LedCmd::Stop | LedCmd::ClearAll => self.cmd_tx.try_send(cmd).is_ok(),
                    LedCmd::SetPlayButton(_)
                    | LedCmd::SetRole(..)
                    | LedCmd::SetRolePulse(..)
                    | LedCmd::FlashRole(..)
                    | LedCmd::SetRecordArt(_)
                    | LedCmd::OnTapFeedback(_) => {
                        // Best-effort: drop under flood (worker will catch next). The record
                        // ring is the exception: its caller sees `false` and sends it again.
                        false
                    }
                }
            }
            Err(TrySendError::Disconnected(_)) => true,
        }
    }
}

impl LedCue for LedProxy {
    fn on_tap_feedback(&mut self, tap_id: u64) {
        self.try_cmd(LedCmd::OnTapFeedback(tap_id));
    }

    fn tick(&mut self) {
        // Worker self-paces at ~30 FPS — coordinator must not drive DMA.
    }

    fn set_play_button(&mut self, state: PlayButtonLed) {
        self.try_cmd(LedCmd::SetPlayButton(state));
    }

    fn has_role(&self, role: LedRole) -> bool {
        self.roles.contains(&role)
    }

    fn set_role_colour(&mut self, role: LedRole, colour: Option<Rgb>) {
        if self.roles.contains(&role) {
            self.try_cmd(LedCmd::SetRole(role, colour));
        }
    }

    fn set_role_pulse(&mut self, role: LedRole, pulse: Option<TransportPulse>) {
        if self.roles.contains(&role) {
            self.try_cmd(LedCmd::SetRolePulse(role, pulse));
        }
    }

    fn flash_role(&mut self, role: LedRole, colour: Rgb) {
        if self.roles.contains(&role) {
            self.try_cmd(LedCmd::FlashRole(role, colour));
        }
    }

    fn set_record_art(&mut self, frame: Option<RecordFrame>) -> bool {
        !self.roles.contains(&LedRole::NfcRing) || self.try_cmd(LedCmd::SetRecordArt(frame))
    }

    fn clear_all(&mut self) {
        // Non-blocking on purpose: this runs on the shutdown path, and a blocking send here
        // wedged the process (Ctrl-C ignored, no "loop exit") whenever the worker was not
        // draining. The worker also clears on `Stop`.
        let _ = self.cmd_tx.try_send(LedCmd::ClearAll);
    }
}

/// Spawn a worker that owns `backend` and applies [`LedCmd`]s.
pub fn spawn_led_worker<L>(backend: L) -> LedWorkerHandle
where
    L: LedCue + Send + 'static,
{
    let (cmd_tx, cmd_rx) = mpsc::sync_channel::<LedCmd>(CMD_CAP);
    let roles: BTreeSet<LedRole> = LedRole::ALL
        .into_iter()
        .filter(|r| backend.has_role(*r))
        .collect();
    let cleared = Arc::new(AtomicBool::new(false));
    let cleared_t = Arc::clone(&cleared);

    let join = thread::Builder::new()
        .name("plippa-ui-led".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_led_worker_loop(backend, cmd_rx, &cleared_t);
            }));
            if result.is_err() {
                eprintln!("plippa-ui: LED worker panicked");
            }
        })
        .expect("spawn LED worker");

    LedWorkerHandle {
        join: Some(join),
        cmd_tx,
        cleared,
        roles: Arc::new(roles),
    }
}

fn run_led_worker_loop<L: LedCue>(mut backend: L, cmd_rx: Receiver<LedCmd>, cleared: &AtomicBool) {
    let mut last_frame = Instant::now()
        .checked_sub(FRAME_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut force = true;

    loop {
        // Coalesce: drain all pending commands each turn.
        let mut stop = false;
        let mut saw_clear = false;
        loop {
            match cmd_rx.try_recv() {
                Ok(LedCmd::Stop) => {
                    stop = true;
                    break;
                }
                Ok(LedCmd::ClearAll) => {
                    saw_clear = true;
                    backend.clear_all();
                    cleared.store(true, Ordering::SeqCst);
                    force = true;
                }
                Ok(LedCmd::OnTapFeedback(id)) => {
                    backend.on_tap_feedback(id);
                    force = true;
                }
                Ok(LedCmd::SetPlayButton(state)) => {
                    backend.set_play_button(state);
                    force = true;
                }
                Ok(LedCmd::SetRole(role, colour)) => {
                    backend.set_role_colour(role, colour);
                    force = true;
                }
                Ok(LedCmd::SetRolePulse(role, pulse)) => {
                    backend.set_role_pulse(role, pulse);
                    force = true;
                }
                Ok(LedCmd::FlashRole(role, colour)) => {
                    backend.flash_role(role, colour);
                    force = true;
                }
                Ok(LedCmd::SetRecordArt(frame)) => {
                    backend.set_record_art(frame);
                    force = true;
                }
                Err(_) => break,
            }
        }

        if stop {
            if !saw_clear {
                backend.clear_all();
                cleared.store(true, Ordering::SeqCst);
            }
            break;
        }

        let now = Instant::now();
        if force || now.duration_since(last_frame) >= FRAME_INTERVAL {
            backend.tick();
            last_frame = Instant::now();
            force = false;
        }

        // Block briefly for the next command, or wake for the next frame slot.
        let wait = FRAME_INTERVAL.saturating_sub(now.duration_since(last_frame));
        match cmd_rx.recv_timeout(wait.max(Duration::from_millis(1))) {
            Ok(LedCmd::Stop) => {
                backend.clear_all();
                cleared.store(true, Ordering::SeqCst);
                break;
            }
            Ok(LedCmd::ClearAll) => {
                backend.clear_all();
                cleared.store(true, Ordering::SeqCst);
                force = true;
            }
            Ok(LedCmd::OnTapFeedback(id)) => {
                backend.on_tap_feedback(id);
                force = true;
            }
            Ok(LedCmd::SetPlayButton(state)) => {
                backend.set_play_button(state);
                force = true;
            }
            Ok(LedCmd::SetRole(role, colour)) => {
                backend.set_role_colour(role, colour);
                force = true;
            }
            Ok(LedCmd::SetRolePulse(role, pulse)) => {
                backend.set_role_pulse(role, pulse);
                force = true;
            }
            Ok(LedCmd::FlashRole(role, colour)) => {
                backend.flash_role(role, colour);
                force = true;
            }
            Ok(LedCmd::SetRecordArt(frame)) => {
                backend.set_record_art(frame);
                force = true;
            }
            Err(_) => {}
        }
    }
}

/// Recording cue for worker tests (counts clear_all).
#[cfg(test)]
#[derive(Debug, Default)]
struct RecordingLed {
    flashes: Vec<u64>,
    play: PlayButtonLed,
    clears: Arc<std::sync::atomic::AtomicU64>,
    ticks: Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(test)]
impl LedCue for RecordingLed {
    fn on_tap_feedback(&mut self, tap_id: u64) {
        self.flashes.push(tap_id);
    }

    fn tick(&mut self) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
    }

    fn set_play_button(&mut self, state: PlayButtonLed) {
        self.play = state;
    }

    fn clear_all(&mut self) {
        self.clears.fetch_add(1, Ordering::Relaxed);
        self.play = PlayButtonLed::Off;
        self.flashes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::led::MockLedCue;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn led_worker_clear_on_stop() {
        let clears = Arc::new(AtomicU64::new(0));
        let backend = RecordingLed {
            clears: Arc::clone(&clears),
            ticks: Arc::new(AtomicU64::new(0)),
            ..Default::default()
        };
        let handle = spawn_led_worker(backend);
        let mut proxy = handle.proxy();
        proxy.set_play_button(PlayButtonLed::Playing);
        proxy.on_tap_feedback(7);
        thread::sleep(Duration::from_millis(20));
        handle.stop_and_join(Duration::from_millis(500));
        assert!(
            clears.load(Ordering::Relaxed) >= 1,
            "LED worker must clear_all on stop"
        );
    }

    #[test]
    fn led_proxy_optimistic_play_before_status() {
        let backend = RecordingLed {
            clears: Arc::new(AtomicU64::new(0)),
            ticks: Arc::new(AtomicU64::new(0)),
            ..Default::default()
        };
        let handle = spawn_led_worker(backend);
        let mut proxy = handle.proxy();
        // Optimistic update (PlayStop path) — must not block.
        let start = Instant::now();
        proxy.set_play_button(PlayButtonLed::Playing);
        assert!(start.elapsed() < Duration::from_millis(10));
        handle.stop_and_join(Duration::from_millis(500));
    }

    #[test]
    fn proxy_reports_roles_and_forwards_role_colours() {
        use std::sync::Mutex;
        type Log = Arc<Mutex<Vec<(LedRole, Option<Rgb>)>>>;
        struct Rec {
            got: Log,
        }
        impl LedCue for Rec {
            fn on_tap_feedback(&mut self, _: u64) {}
            fn has_role(&self, role: LedRole) -> bool {
                role == LedRole::PrevTrackButton
            }
            fn set_role_colour(&mut self, role: LedRole, c: Option<Rgb>) {
                self.got.lock().unwrap().push((role, c));
            }
        }
        let got = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_led_worker(Rec {
            got: Arc::clone(&got),
        });
        let mut proxy = handle.proxy();
        assert!(proxy.has_role(LedRole::PrevTrackButton));
        assert!(!proxy.has_role(LedRole::NextTrackButton));
        let c = Rgb::new(1, 2, 3);
        proxy.set_role_colour(LedRole::NextTrackButton, Some(c)); // absent: dropped at the proxy
        proxy.set_role_colour(LedRole::PrevTrackButton, Some(c));
        handle.stop_and_join(Duration::from_millis(500));
        assert_eq!(
            *got.lock().unwrap(),
            vec![(LedRole::PrevTrackButton, Some(c))]
        );
    }

    #[test]
    fn proxy_forwards_record_frames_only_for_a_board_with_a_ring() {
        use crate::art::ArtImage;
        use std::sync::Mutex;
        type Log = Arc<Mutex<Vec<Option<f32>>>>;
        struct Rec {
            got: Log,
            ring: bool,
        }
        impl LedCue for Rec {
            fn on_tap_feedback(&mut self, _: u64) {}
            fn has_role(&self, role: LedRole) -> bool {
                self.ring && role == LedRole::NfcRing
            }
            fn set_record_art(&mut self, f: Option<RecordFrame>) -> bool {
                self.got.lock().unwrap().push(f.map(|f| f.angle_deg));
                true
            }
        }
        let art = Arc::new(ArtImage {
            side: 1,
            rgb565: vec![0],
            rgb_side: 64,
            rgb: vec![0; 64 * 64 * 3],
        });
        for (ring, expect) in [(true, vec![Some(12.5), None]), (false, vec![])] {
            let got = Arc::new(Mutex::new(Vec::new()));
            let handle = spawn_led_worker(Rec {
                got: Arc::clone(&got),
                ring,
            });
            let mut proxy = handle.proxy();
            proxy.set_record_art(Some(RecordFrame {
                art: Arc::clone(&art),
                angle_deg: 12.5,
            }));
            proxy.set_record_art(None);
            handle.stop_and_join(Duration::from_millis(500));
            assert_eq!(*got.lock().unwrap(), expect, "ring: {ring}");
        }
    }

    #[test]
    fn a_clear_refused_by_a_full_queue_is_resent_until_the_ring_is_off() {
        use crate::art::ArtImage;
        use crate::record_leds::{RecordFeed, RecordInput, RecordUpdate};
        use std::sync::mpsc;
        use std::sync::Mutex;
        struct Rec {
            got: Arc<Mutex<Vec<Option<f32>>>>,
            started: mpsc::Sender<()>,
            gate: mpsc::Receiver<()>,
        }
        impl LedCue for Rec {
            fn on_tap_feedback(&mut self, _: u64) {}
            fn has_role(&self, role: LedRole) -> bool {
                role == LedRole::NfcRing
            }
            fn set_record_art(&mut self, f: Option<RecordFrame>) -> bool {
                if f.is_some() {
                    // Hold the worker here so the queue behind it fills up.
                    let _ = self.started.send(());
                    let _ = self.gate.recv();
                }
                self.got.lock().unwrap().push(f.map(|f| f.angle_deg));
                true
            }
        }
        let art = Arc::new(ArtImage {
            side: 1,
            rgb565: vec![0],
            rgb_side: 64,
            rgb: vec![0; 64 * 64 * 3],
        });
        let got = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let (gate_tx, gate_rx) = mpsc::channel();
        let handle = spawn_led_worker(Rec {
            got: Arc::clone(&got),
            started: started_tx,
            gate: gate_rx,
        });
        let mut proxy = handle.proxy();
        let mut feed = RecordFeed::new();
        let input = || {
            Some(RecordInput {
                art: Arc::clone(&art),
                position_ms: 0,
            })
        };
        let RecordUpdate::Show(frame) = feed.update(input(), 0) else {
            panic!("first sight shows");
        };
        assert!(proxy.set_record_art(Some(frame)));
        started_rx.recv_timeout(Duration::from_secs(2)).expect("worker took the frame");
        // The worker is stuck: flood the queue until it refuses.
        let mut n = 0;
        while proxy.try_cmd(LedCmd::OnTapFeedback(n)) {
            n += 1;
            assert!(n < 1000, "queue must be bounded");
        }
        // Stop: the Clear is refused, and the feed must keep asking for it.
        let mut now = 100;
        let mut refused = 0;
        let mut delivered = false;
        for _ in 0..3 {
            assert_eq!(feed.update(None, now), RecordUpdate::Clear);
            if proxy.set_record_art(None) {
                delivered = true;
                break;
            }
            refused += 1;
            feed.not_delivered();
            now += 10;
        }
        assert!(!delivered && refused == 3, "a blocked queue refuses the clear");
        gate_tx.send(()).unwrap(); // worker drains
        let t0 = Instant::now();
        loop {
            match feed.update(None, now) {
                RecordUpdate::Keep => break,
                RecordUpdate::Clear => {
                    if !proxy.set_record_art(None) {
                        feed.not_delivered();
                    }
                }
                RecordUpdate::Show(_) => panic!("no input, no show"),
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "clear never delivered");
            now += 10;
            thread::sleep(Duration::from_millis(5));
        }
        handle.stop_and_join(Duration::from_millis(500));
        assert_eq!(got.lock().unwrap().last(), Some(&None), "ring ends off");
    }

    #[test]
    fn proxy_forwards_role_pulses_without_blocking() {
        use crate::led::TransportPulse;
        use crate::warp_ui::{FFRW_LED_IDLE, FFRW_LED_PRESSED};
        use std::sync::Mutex;
        type Log = Arc<Mutex<Vec<(LedRole, Option<TransportPulse>)>>>;
        struct Rec {
            got: Log,
        }
        impl LedCue for Rec {
            fn on_tap_feedback(&mut self, _: u64) {}
            fn has_role(&self, role: LedRole) -> bool {
                role == LedRole::FastForwardButton
            }
            fn set_role_pulse(&mut self, role: LedRole, p: Option<TransportPulse>) {
                self.got.lock().unwrap().push((role, p));
            }
        }
        let got = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_led_worker(Rec {
            got: Arc::clone(&got),
        });
        let mut proxy = handle.proxy();
        let pulse = TransportPulse {
            lo: FFRW_LED_IDLE,
            hi: FFRW_LED_PRESSED,
            period_ms: 800,
        };
        proxy.set_role_pulse(LedRole::RewindButton, Some(pulse)); // absent: dropped at the proxy
        proxy.set_role_pulse(LedRole::FastForwardButton, Some(pulse));
        proxy.set_role_pulse(LedRole::FastForwardButton, None);
        handle.stop_and_join(Duration::from_millis(500));
        assert_eq!(
            *got.lock().unwrap(),
            vec![
                (LedRole::FastForwardButton, Some(pulse)),
                (LedRole::FastForwardButton, None),
            ]
        );
    }

    #[test]
    fn proxy_forwards_press_flashes_without_blocking() {
        use std::sync::Mutex;
        struct Rec {
            got: Arc<Mutex<Vec<(LedRole, Rgb)>>>,
        }
        impl LedCue for Rec {
            fn on_tap_feedback(&mut self, _: u64) {}
            fn has_role(&self, role: LedRole) -> bool {
                role == LedRole::NextTrackButton
            }
            fn flash_role(&mut self, role: LedRole, c: Rgb) {
                self.got.lock().unwrap().push((role, c));
            }
        }
        let got = Arc::new(Mutex::new(Vec::new()));
        let handle = spawn_led_worker(Rec {
            got: Arc::clone(&got),
        });
        let mut proxy = handle.proxy();
        let c = Rgb::new(255, 255, 255);
        let start = Instant::now();
        proxy.flash_role(LedRole::PrevTrackButton, c); // absent: dropped at the proxy
        proxy.flash_role(LedRole::NextTrackButton, c);
        assert!(start.elapsed() < Duration::from_millis(10));
        handle.stop_and_join(Duration::from_millis(500));
        assert_eq!(*got.lock().unwrap(), vec![(LedRole::NextTrackButton, c)]);
    }

    #[test]
    fn clear_all_never_blocks_when_queue_is_full() {
        // Shutdown must not wedge on a full LED queue: `clear_all` runs on the exit path.
        let (tx, _rx) = mpsc::sync_channel::<LedCmd>(1);
        tx.try_send(LedCmd::ClearAll).expect("fill queue");
        let mut proxy = LedProxy {
            cmd_tx: tx,
            roles: Arc::new(BTreeSet::new()),
        };
        let start = Instant::now();
        proxy.clear_all();
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "clear_all blocked on a full queue ({:?})",
            start.elapsed()
        );
    }

    #[test]
    fn mock_backend_needs_no_worker_threads() {
        let mut led = MockLedCue::default();
        led.on_tap_feedback(1);
        led.set_play_button(PlayButtonLed::Standby);
        assert_eq!(led.flashes, vec![1]);
        assert_eq!(led.play_button, PlayButtonLed::Standby);
    }
}
