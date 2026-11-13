//! WARP_20: whole-system walk through the new tempo / warp / FF-RW / jog feature set.
//!
//! Real engine hub (`Engine::open_with_sink_factory`, UDS socket, real audio thread, decode
//! worker, rate stage, seek core) with a **real-time-paced capturing mock sink**, and the
//! real device UI (`UiApp`, `play_mode.json` on a temp dir) as the peer on the same socket.
//! Input is mock (`InputEvent`s with a UI clock = real elapsed time + a virtual skip, so slow
//! detents need no sleeping). Nothing asserts a wall-clock duration or a CPU figure: every wait is a
//! bounded poll on engine status, and the audio checks compare sample values with the source.
//!
//! The fixture is a 10 min mono 8 kHz WAV whose sample value encodes the frame number
//! (`frame mod 65536`), so the captured sink output names the source frame it came from.
//!
//! Journey: load + start; tempo 125% through the Enter ring; speed warp mode switched in the
//! Play-mode menu (voice follows); FF tap ladder up to 200%, down through 0 into -200% (the
//! playhead really moves backward) and back up; hold-FF scan and release (playhead continues
//! from where it got to, set speed unchanged); hold-RW scan; jog turn -> live target, no
//! engine seek while turning, exactly one `SeekTo`, lands, tempo unchanged; long Play hold ->
//! pure 100% (bit-exact output, still playing, position consistent with the audio heard);
//! "Seek outside track" OFF: jog clamps to `duration - 250 ms` with a plain `SeekTo`; the
//! persisted `play_mode.json` is read by a restarted UI which re-sends its warp mode.

use plippa_audio::{AudioError, AudioSink};
use plippa_board::ControlRole;
use plippa_core::{
    Capabilities, ScanState, SessionState, TransportState, VoiceKind, Volume, WarpMode,
};
use plippa_engine::{run_hub, Engine, HubShutdown};
use plippa_ipc_proto::{IpcConnection, IpcMessage, IpcPaths, PlaybackCommand, TapKeyPayload};
use plippa_noubin::{normalise_noubin_key, LibraryRoot, LibraryWriter};
use plippa_ui::{
    InputEvent, MockDisplay, MockWifi, PlayModeRow, PlayModeStore, Route, UiApp, UiConfig,
    UiOutgoing, JOG_COMMIT_IDLE_MS,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RATE: u32 = 8_000;
const DUR_S: u32 = 600;
const DUR_MS: u64 = DUR_S as u64 * 1000;
const KEY: &str = "Journey.noubin.WARP20";

// ---------------------------------------------------------------- fixture

fn write_fixture(path: &std::path::Path) {
    let frames = (RATE * DUR_S) as usize;
    let mut data = Vec::with_capacity(frames * 2);
    for f in 0..frames {
        let v = ((f % 65_536) as i32 - 32_768) as i16;
        data.extend_from_slice(&v.to_le_bytes());
    }
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&RATE.to_le_bytes());
    v.extend_from_slice(&(RATE * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(&data);
    fs::write(path, v).unwrap();
}

/// Source frame (mod 65536) a sample value was decoded from.
fn frame_of(sample: f32) -> i64 {
    ((sample * 32_768.0).round() as i64 + 32_768).rem_euclid(65_536)
}

// ---------------------------------------------------------------- capturing paced sink

#[derive(Default)]
struct Cap {
    /// Samples recorded while `want > 0` (consumed by `capture`).
    rec: Vec<f32>,
    want: usize,
}

struct CapSink {
    cap: Arc<Mutex<Cap>>,
}

impl AudioSink for CapSink {
    fn channels(&self) -> u16 {
        1
    }
    fn sample_rate_hz(&self) -> u32 {
        RATE
    }
    fn write(&mut self, interleaved: &[f32]) -> Result<(), AudioError> {
        {
            let mut c = self.cap.lock().unwrap();
            if c.want > 0 {
                let take = c.want.min(interleaved.len());
                c.rec.extend_from_slice(&interleaved[..take]);
                c.want -= take;
            }
        }
        thread::sleep(Duration::from_micros(
            interleaved.len() as u64 * 1_000_000 / u64::from(RATE),
        ));
        Ok(())
    }
}

// ---------------------------------------------------------------- rig

struct Rig {
    stop: HubShutdown,
    hub: Option<thread::JoinHandle<()>>,
    conn: IpcConnection,
    ui: UiApp,
    wifi: MockWifi,
    display: MockDisplay,
    cap: Arc<Mutex<Cap>>,
    session: Option<SessionState>,
    /// Every status position seen, in order.
    positions: Vec<u64>,
    /// Every playback command the UI sent.
    sent: Vec<PlaybackCommand>,
    t0: Instant,
    skip_ms: u64,
    play_mode_path: PathBuf,
    ipc_sock: PathBuf,
    _lib: tempfile::TempDir,
    _ipc: tempfile::TempDir,
    _state: tempfile::TempDir,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.hub.take() {
            let _ = h.join();
        }
    }
}

impl Rig {
    fn start() -> Rig {
        let lib = tempfile::tempdir().unwrap();
        let root = lib.path().to_path_buf();
        write_fixture(&root.join("long.wav"));
        let noudata = format!(
            r#"{{"noubinKey":"{KEY}","noubinKeyNormalised":"{}","playableItemTitle":"Long",
                "mediaList":[{{"localFilePaths":["long.wav"]}}]}}"#,
            normalise_noubin_key(KEY)
        );
        fs::write(root.join("_long.noudata"), noudata).unwrap();
        LibraryWriter::open(&LibraryRoot::new(&root))
            .unwrap()
            .full_scan()
            .unwrap();
        let ipc = tempfile::tempdir().unwrap();
        let paths = IpcPaths::from_dir(ipc.path());
        let cap = Arc::new(Mutex::new(Cap::default()));
        let stop: HubShutdown = Arc::new(AtomicBool::new(false));
        let hub = {
            let (paths, stop, cap, root) = (
                paths.clone(),
                Arc::clone(&stop),
                Arc::clone(&cap),
                root.clone(),
            );
            thread::spawn(move || {
                let mut engine = Engine::open_with_sink_factory(
                    root,
                    Capabilities::desktop_mock(),
                    move |_ch, _hz| {
                        Ok(Box::new(CapSink {
                            cap: Arc::clone(&cap),
                        }) as Box<dyn AudioSink>)
                    },
                )
                .unwrap();
                run_hub(
                    &mut engine,
                    &paths,
                    Some(Instant::now() + Duration::from_secs(280)),
                    Some(stop),
                )
                .unwrap();
            })
        };
        let t0 = Instant::now();
        while !paths.engine_sock.exists() {
            assert!(t0.elapsed() < Duration::from_secs(15), "engine socket");
            thread::sleep(Duration::from_millis(20));
        }
        let state = tempfile::tempdir().unwrap();
        let play_mode_path = state.path().join("play_mode.json");
        let conn =
            IpcConnection::connect_timeout(&paths.engine_sock, Duration::from_secs(5)).unwrap();
        Rig {
            stop,
            hub: Some(hub),
            conn,
            ui: UiApp::new(UiConfig {
                play_mode_path: Some(play_mode_path.clone()),
                ..UiConfig::default()
            }),
            wifi: MockWifi::default(),
            display: MockDisplay::new(320, 240),
            cap,
            session: None,
            positions: vec![],
            sent: vec![],
            t0: Instant::now(),
            skip_ms: 0,
            play_mode_path,
            ipc_sock: paths.engine_sock.clone(),
            _lib: lib,
            _ipc: ipc,
            _state: state,
        }
    }

    fn now(&self) -> u64 {
        self.t0.elapsed().as_millis() as u64 + self.skip_ms
    }

    fn send(&mut self, c: PlaybackCommand) {
        self.conn.send(&IpcMessage::PlaybackCommand(c)).unwrap();
    }

    /// One loop pass: UI clock tick, engine messages -> UI, UI commands -> engine.
    fn pump(&mut self) {
        let now = self.now();
        self.ui
            .handle_input(InputEvent::Tick { now_ms: now }, &mut self.wifi);
        while let Ok(Some(msg)) = self.conn.recv_timeout(Duration::from_millis(10)) {
            if let IpcMessage::PlaybackStatus(s) = &msg {
                self.positions.push(s.session.position_ms);
                self.session = Some(s.session.clone());
            }
            self.ui.handle_ipc(msg);
        }
        self.flush();
        self.ui.present(&mut self.display);
    }

    fn flush(&mut self) {
        for out in self.ui.take_outgoing() {
            if let UiOutgoing::Ipc(m) = out {
                if let IpcMessage::PlaybackCommand(c) = &m {
                    self.sent.push(c.clone());
                }
                self.conn.send(&m).unwrap();
            }
        }
    }

    fn input(&mut self, ev: InputEvent) {
        self.ui.handle_input(ev, &mut self.wifi);
        self.flush();
    }

    fn s(&self) -> &SessionState {
        self.session.as_ref().expect("status")
    }

    fn until(&mut self, what: &str, f: impl Fn(&Rig) -> bool) {
        let t = Instant::now();
        loop {
            self.pump();
            if self.session.is_some() && f(self) {
                return;
            }
            if t.elapsed() > Duration::from_secs(25) {
                let s = self.session.as_ref();
                panic!(
                    "timed out waiting for: {what} (pos {:?} rate {:?}/{:?} scan {:?} voice {:?} tgt {:?})",
                    s.map(|s| s.position_ms),
                    s.map(|s| s.rate_permille),
                    s.map(|s| s.target_rate_permille),
                    s.map(|s| s.scan),
                    s.map(|s| s.speed_voice),
                    s.map(|s| s.seek_target_ms),
                );
            }
        }
    }

    /// Pump for `ms` of real time (lets the engine run while the UI clock advances).
    fn run_for(&mut self, ms: u64) {
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(ms) {
            self.pump();
        }
    }

    fn take_sent(&mut self) -> Vec<PlaybackCommand> {
        std::mem::take(&mut self.sent)
    }

    // ---- buttons ----

    fn down(&mut self, role: ControlRole) {
        let at_ms = self.now();
        self.input(InputEvent::Tick { now_ms: at_ms });
        self.input(InputEvent::ControlPressed(role));
        self.input(InputEvent::ControlButton {
            role,
            down: true,
            at_ms,
        });
    }
    fn up(&mut self, role: ControlRole) {
        let at_ms = self.now();
        self.input(InputEvent::Tick { now_ms: at_ms });
        self.input(InputEvent::ControlButton {
            role,
            down: false,
            at_ms,
        });
    }
    /// A 40 ms tap on the UI clock (no real waiting).
    fn tap(&mut self, role: ControlRole) {
        self.down(role);
        self.skip_ms += 40;
        self.up(role);
    }
    /// Hold for `ms` of UI clock while the engine runs in real time.
    fn hold(&mut self, role: ControlRole, ms: u64, extra: impl Fn(&Rig) -> bool) {
        self.down(role);
        let start = self.now();
        let t = Instant::now();
        while self.now() < start + ms || !extra(self) {
            self.pump();
            assert!(t.elapsed() < Duration::from_secs(40), "hold timed out");
        }
    }

    /// One detent with a gap far above the velocity reset: always the finest step.
    fn slow_click(&mut self, dir: i8) {
        self.skip_ms += 1_000;
        let now = self.now();
        self.input(InputEvent::Tick { now_ms: now });
        self.input(InputEvent::MenuRotate(dir));
    }

    /// One detent 30 ms after the previous one (inside the jog commit idle window).
    fn fast_click(&mut self, dir: i8) {
        self.skip_ms += 30;
        let now = self.now();
        self.input(InputEvent::Tick { now_ms: now });
        self.input(InputEvent::MenuRotate(dir));
    }

    /// Next `n` sink samples, contiguous.
    fn capture(&mut self, n: usize) -> Vec<f32> {
        {
            let mut c = self.cap.lock().unwrap();
            c.rec.clear();
            c.want = n;
        }
        let t = Instant::now();
        loop {
            self.pump();
            let c = self.cap.lock().unwrap();
            if c.want == 0 {
                return c.rec.clone();
            }
            drop(c);
            assert!(t.elapsed() < Duration::from_secs(20), "capture timed out");
        }
    }

    fn rate_cmds(sent: &[PlaybackCommand]) -> Vec<i32> {
        sent.iter()
            .filter_map(|c| match c {
                PlaybackCommand::SetRate { permille } => Some(*permille),
                _ => None,
            })
            .collect()
    }
}

// ---------------------------------------------------------------- the journey

#[test]
fn whole_warp_feature_set_end_to_end() {
    let mut r = Rig::start();

    // ---- 1. load the long fixture and start; engine defaults are as documented ----------
    r.send(PlaybackCommand::SetVolume(Volume::new(100)));
    r.conn
        .send(&IpcMessage::TapKey(TapKeyPayload {
            noubin_key: KEY.into(),
            tap_id: 1,
            already_normalised: false,
        }))
        .unwrap();
    r.until("playing the long fixture", |r| {
        r.s().transport == TransportState::Playing
            && r.s().duration_ms == Some(DUR_MS)
            && r.s().position_ms > 0
    });
    assert_eq!(r.s().target_rate_permille, 1000);
    assert_eq!(
        r.s().speed_voice,
        VoiceKind::Bypass,
        "100% is the unchanged path"
    );
    assert_eq!(r.s().speed_warp_mode, WarpMode::PitchCorrect);
    assert_eq!(r.s().seek_warp_mode, WarpMode::PitchShift);
    assert!(!r.s().seek_outside_track, "default OFF");
    // Cue/resume style hard seek into the middle so there is room both ways.
    r.send(PlaybackCommand::SeekMs(300_000));
    r.until("at 300 s", |r| {
        (300_000..310_000).contains(&r.s().position_ms)
    });

    // ---- 2. tempo 125% through the Enter ring ------------------------------------------
    for _ in 0..4 {
        if r.ui.model.np_focus.param() == Some(plippa_ui::AdjustParam::Tempo) {
            break;
        }
        r.input(InputEvent::MenuEnter);
    }
    assert_eq!(
        r.ui.model.np_focus.param(),
        Some(plippa_ui::AdjustParam::Tempo)
    );
    for _ in 0..25 {
        r.slow_click(1);
        r.pump();
    }
    r.until("set speed 125%", |r| r.s().target_rate_permille == 1250);
    r.until("pitch-correct voice engaged", |r| {
        r.s().speed_voice == VoiceKind::Stretch
    });
    assert!(r.s().warp_active);
    assert!(Rig::rate_cmds(&r.take_sent()).contains(&1250));
    // Back to the jog knob (Back leaves the ring).
    r.input(InputEvent::Back);
    r.pump();
    assert_eq!(r.ui.model.np_focus.param(), None, "ring left");

    // ---- 3. speed warp mode in the Play-mode menu: the voice follows ---------------------
    let hold_start = r.now();
    r.down(ControlRole::PlayMode);
    while r.now() < hold_start + plippa_ui::PLAYMODE_HOLD_MS + 50 {
        r.pump();
    }
    assert_eq!(
        *r.ui.nav.current(),
        Route::SystemPlayMode,
        "SW2 hold jumps to Playback settings"
    );
    r.up(ControlRole::PlayMode);
    assert_eq!(
        r.ui.model.play_mode.value_text(PlayModeRow::SpeedWarp),
        "Correct"
    );
    // The playback modes lead the merged menu (rows 0-3), then `Playback speed`, then
    // `Speed warp` at 5.
    r.ui.nav.focus = 5;
    r.input(InputEvent::MenuEnter);
    r.until("speed warp = shift, varispeed voice", |r| {
        r.s().speed_warp_mode == WarpMode::PitchShift && r.s().speed_voice == VoiceKind::Varispeed
    });
    assert_eq!(
        r.s().target_rate_permille,
        1250,
        "mode change keeps the set speed"
    );
    assert!(
        fs::read_to_string(&r.play_mode_path)
            .unwrap()
            .contains("speed_warp"),
        "explicit choice persisted"
    );
    r.input(InputEvent::MenuEnter); // cycles back to Correct
    r.until("speed warp = correct again", |r| {
        r.s().speed_warp_mode == WarpMode::PitchCorrect && r.s().speed_voice == VoiceKind::Stretch
    });
    r.input(InputEvent::Back);
    r.pump();
    assert_ne!(
        *r.ui.nav.current(),
        Route::SystemPlayMode,
        "Back leaves the menu"
    );
    r.take_sent();

    // ---- 4. FF/RW tap ladder: up to 200%, down through 0 into -200%, and back ------------
    for want in [1500, 1750, 2000] {
        r.tap(ControlRole::FastForward);
        r.until("ladder up", |r| r.s().target_rate_permille == want);
    }
    r.take_sent();
    r.tap(ControlRole::FastForward);
    r.run_for(200);
    assert!(
        Rig::rate_cmds(&r.take_sent()).is_empty(),
        "no rung above 200%"
    );
    let mut rung = 2000;
    let mut seen_down = vec![];
    while rung > -2000 {
        r.tap(ControlRole::Rewind);
        let prev = rung;
        r.until("ladder down", |r| r.s().target_rate_permille < prev);
        rung = r.s().target_rate_permille;
        seen_down.push(rung);
        if rung == -1000 {
            // Real reverse: the playhead moves backward and the voice is the varispeed one.
            let p0 = r.s().position_ms;
            r.until("reverse really moves the playhead back", |r| {
                r.s().position_ms + 600 < p0
            });
            assert_eq!(
                r.s().speed_voice,
                VoiceKind::Varispeed,
                "reverse is always pitch-warped"
            );
            assert!(r.s().rate_permille < 0);
        }
    }
    assert_eq!(
        seen_down,
        [
            1750, 1500, 1250, 1000, 750, 500, 250, -250, -500, -750, -1000, -1250, -1500, -1750,
            -2000
        ],
        "one shared signed ladder, 0 is never a rung"
    );
    // Back up the same ladder to 125%.
    while r.s().target_rate_permille != 1250 {
        let prev = r.s().target_rate_permille;
        r.tap(ControlRole::FastForward);
        r.until("ladder up again", |r| r.s().target_rate_permille > prev);
    }
    r.until("forward again at 125%", |r| {
        r.s().rate_permille > 0 && r.s().speed_voice == VoiceKind::Stretch
    });
    let pos_a = r.s().position_ms;
    r.run_for(500);
    assert!(r.s().position_ms >= pos_a, "playing forward");
    r.take_sent();

    // ---- 5. hold FF: cassette scan; release leaves playhead and set speed alone ---------
    let before = r.s().position_ms;
    let seen_from = r.positions.len();
    r.hold(ControlRole::FastForward, 300, |r| {
        r.s().scan == ScanState::Forward && r.s().position_ms > before + 6_000
    });
    assert!(r.s().seek_target_ms.is_none(), "a scan has no destination");
    assert_eq!(
        r.s().target_rate_permille,
        1250,
        "scan never edits the set speed"
    );
    let at_release = r.s().position_ms;
    r.up(ControlRole::FastForward);
    r.until("scan over", |r| r.s().scan == ScanState::None);
    r.run_for(600);
    let sent = r.take_sent();
    assert!(sent.contains(&PlaybackCommand::ScanStart {
        dir: plippa_core::ScanDir::Forward
    }));
    assert!(sent.contains(&PlaybackCommand::ScanStop));
    assert!(
        Rig::rate_cmds(&sent).is_empty(),
        "taps only change the set speed"
    );
    assert_eq!(
        r.s().target_rate_permille,
        1250,
        "set speed unchanged after release"
    );
    assert!(
        r.s().position_ms >= at_release,
        "the playhead continues from where it got to"
    );
    let seen = &r.positions[seen_from..];
    for w in seen.windows(2) {
        assert!(
            w[1] + 200 >= w[0],
            "playhead never snaps back: {} -> {}",
            w[0],
            w[1]
        );
        assert!(w[1] <= w[0] + 4_000, "no jump: {} -> {}", w[0], w[1]);
    }

    // ---- 6. hold RW: backward scan ----------------------------------------------------
    let before = r.s().position_ms;
    r.hold(ControlRole::Rewind, 300, |r| {
        r.s().scan == ScanState::Backward && r.s().position_ms + 3_000 < before
    });
    r.up(ControlRole::Rewind);
    r.until("rewind scan over", |r| r.s().scan == ScanState::None);
    assert_eq!(r.s().target_rate_permille, 1250);
    r.take_sent();

    // ---- 7. jog: live target, no engine seek while turning, exactly one commit ------------
    r.until("forward again", |r| r.s().rate_permille > 0);
    let jog_base = r.s().position_ms;
    for _ in 0..6 {
        r.fast_click(1);
    }
    let view = r.ui.model.jog_view.expect("jog target shown live");
    assert!(
        view.target_ms > jog_base,
        "target {} moves ahead of {}",
        view.target_ms,
        jog_base
    );
    assert!(
        r.take_sent()
            .iter()
            .all(|c| !matches!(c, PlaybackCommand::SeekTo { .. })),
        "nothing sent while turning"
    );
    assert!(r.s().seek_target_ms.is_none(), "engine has no target yet");
    r.skip_ms += JOG_COMMIT_IDLE_MS + 50;
    r.pump();
    let seeks: Vec<u64> = r
        .take_sent()
        .into_iter()
        .filter_map(|c| match c {
            PlaybackCommand::SeekTo { target_ms } => Some(target_ms),
            _ => None,
        })
        .collect();
    assert_eq!(seeks.len(), 1, "one commit: {seeks:?}");
    let target = seeks[0];
    r.until("jump landed", |r| {
        r.s().seek_target_ms.is_none() && r.s().position_ms + 1_500 >= target
    });
    assert!(r.s().position_ms >= target.saturating_sub(1_500));
    assert_eq!(
        r.s().target_rate_permille,
        1250,
        "jog leaves the set speed alone"
    );

    // ---- 8. long Play hold: 100% pure, bit-exact, still playing -------------------------
    let hold_start = r.now();
    r.down(ControlRole::Play);
    while r.now() < hold_start + 800 + 50 {
        r.pump();
    }
    r.up(ControlRole::Play);
    r.until("pure 100%", |r| {
        r.s().target_rate_permille == 1000
            && r.s().speed_voice == VoiceKind::Bypass
            && !r.s().warp_active
    });
    assert_eq!(
        r.s().transport,
        TransportState::Playing,
        "the reset hold never toggles play"
    );
    assert!(r.take_sent().contains(&PlaybackCommand::ResetRate));
    r.run_for(500); // let the hand-over crossfade finish
    let pos_ms_before = r.s().position_ms;
    let audio = r.capture(4096);
    let f0 = frame_of(audio[0]);
    for (i, s) in audio.iter().enumerate() {
        let want = (((f0 + i as i64) % 65_536) - 32_768) as f32 / 32_768.0;
        assert_eq!(*s, want, "sample {i}: bypass is bit-exact with the source");
    }
    // The position the UI is told matches the audio actually heard (media time, mod 65536
    // frames = 8.19 s; the status is at most a few hundred ms old).
    let heard_ms = (f0 * 1000 / i64::from(RATE)) as u64;
    let pos_ms = r.s().position_ms.max(pos_ms_before);
    let period_ms = 65_536 * 1000 / u64::from(RATE);
    let d = (pos_ms as i64 - heard_ms as i64).rem_euclid(period_ms as i64);
    let d = d.min(period_ms as i64 - d);
    assert!(
        d < 1_500,
        "reported position {pos_ms} ms vs audio {heard_ms} ms (mod {period_ms})"
    );

    // ---- 9. seek outside track OFF: jog clamps at duration - 250 ms, plain SeekTo --------
    r.send(PlaybackCommand::SeekMs(DUR_MS - 20_000));
    r.until("near the end", |r| {
        r.s().position_ms + 21_000 > DUR_MS && r.s().position_ms + 15_000 < DUR_MS
    });
    r.take_sent();
    for _ in 0..40 {
        r.fast_click(1);
    }
    r.pump();
    r.skip_ms += JOG_COMMIT_IDLE_MS + 50;
    r.pump();
    let sent = r.take_sent();
    let seeks: Vec<&PlaybackCommand> = sent
        .iter()
        .filter(|c| {
            matches!(
                c,
                PlaybackCommand::SeekTo { .. } | PlaybackCommand::SeekToItem { .. }
            )
        })
        .collect();
    assert_eq!(
        seeks,
        [&PlaybackCommand::SeekTo {
            target_ms: DUR_MS - 250
        }],
        "clamped plain SeekTo"
    );

    // ---- 10. persistence: explicit choice saved, reconciled against the engine, restart ---
    let hold_start = r.now();
    r.down(ControlRole::PlayMode);
    while r.now() < hold_start + plippa_ui::PLAYMODE_HOLD_MS + 50 {
        r.pump();
    }
    r.up(ControlRole::PlayMode);
    assert_eq!(*r.ui.nav.current(), Route::SystemPlayMode);
    // The playback modes lead the merged menu (rows 0-3), then `Playback speed`, then
    // `Speed warp` at 5.
    r.ui.nav.focus = 5;
    r.input(InputEvent::MenuEnter); // Correct -> Shift
    r.until("shift chosen", |r| {
        r.s().speed_warp_mode == WarpMode::PitchShift
    });
    // Something else flips the engine back: the UI notices the mismatch and re-sends its choice.
    r.send(PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchCorrect));
    r.run_for(300);
    r.until("UI reconciles the engine to its persisted mode", |r| {
        r.s().speed_warp_mode == WarpMode::PitchShift
    });
    let saved = PlayModeStore::load_or_default(&r.play_mode_path);
    assert_eq!(saved.settings.warp.speed, Some(WarpMode::PitchShift));
    assert_eq!(
        saved.settings.warp.seek, None,
        "only explicit choices are stored"
    );
    let mut ui2 = UiApp::new(UiConfig {
        play_mode_path: Some(r.play_mode_path.clone()),
        ..UiConfig::default()
    });
    ui2.set_engine_connected(false);
    ui2.set_engine_connected(true);
    let mut wifi = MockWifi::default();
    let mut conn2 = IpcConnection::connect_timeout(&r.ipc_sock, Duration::from_secs(5)).unwrap();
    let t = Instant::now();
    let mut resent = false;
    while t.elapsed() < Duration::from_secs(10) && !resent {
        ui2.handle_input(
            InputEvent::Tick {
                now_ms: t.elapsed().as_millis() as u64,
            },
            &mut wifi,
        );
        if let Ok(Some(msg)) = conn2.recv_timeout(Duration::from_millis(50)) {
            ui2.handle_ipc(msg);
        }
        for o in ui2.take_outgoing() {
            if let UiOutgoing::Ipc(IpcMessage::PlaybackCommand(
                PlaybackCommand::SetSpeedWarpMode(m),
            )) = o
            {
                assert_eq!(m, WarpMode::PitchShift);
                resent = true;
            }
        }
    }
    assert!(
        resent,
        "the restarted UI sends its persisted speed warp mode on connect"
    );
}
