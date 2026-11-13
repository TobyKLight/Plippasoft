//! AUDIO_04: System > Audio, Output devices and Delay.
//!
//! Mock input, injected routes (`AudioRouteChanged`) and a temp `audio_outputs.json`; no
//! engine, no wall-clock time. Every assertion is about what the owner can do and what the
//! app sends, not about pixel design.

use plippa_core::audio_route::{
    AudioOutput, AudioRoute, DELAY_MAX_MS, DELAY_STEP_MS, MAX_ENABLED_OUTPUTS,
};
use plippa_core::{PlayableItemId, SessionState, TransportState, Volume};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand, PlaybackStatus};
use plippa_ui::ui::tokens::{BG, CONTENT_TOP, MARGIN_X, ROW_H, SCREEN_W, SPACE_1};
use plippa_ui::{
    audio_report, AudioRow, AudioSettings, AudioStore, InputEvent, MockWifi, Route, UiApp,
    UiConfig, UiOutgoing, AUDIO_FILE_NAME, AUDIO_ROWS, JOG_COMMIT_IDLE_MS,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

struct Rig {
    app: UiApp,
    wifi: MockWifi,
    path: PathBuf,
    rev: u64,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        Self::with_settings(None)
    }

    /// Build an app on a temp state dir, optionally pre-seeding `audio_outputs.json`.
    fn with_settings(settings: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(AUDIO_FILE_NAME);
        if let Some(text) = settings {
            std::fs::write(&path, text).unwrap();
        }
        let app = UiApp::new(UiConfig {
            audio_path: Some(path.clone()),
            play_mode_path: Some(dir.path().join("play_mode.json")),
            ..UiConfig::default()
        });
        Self {
            app,
            wifi: MockWifi::default(),
            path,
            rev: 0,
            _dir: dir,
        }
    }

    fn input(&mut self, ev: InputEvent) {
        self.app.handle_input(ev, &mut self.wifi);
    }

    fn enter(&mut self) {
        self.input(InputEvent::MenuEnter);
    }

    fn back(&mut self) {
        self.input(InputEvent::Back);
    }

    fn rotate(&mut self, delta: i8) {
        self.input(InputEvent::MenuRotate(delta));
    }

    /// Publish an engine route (revision bumped) and return everything the app sent in reply.
    fn route(&mut self, mut route: AudioRoute) -> Vec<PlaybackCommand> {
        self.rev += 1;
        route.revision = self.rev;
        self.app.handle_ipc(IpcMessage::AudioRouteChanged(route));
        self.cmds()
    }

    /// Publish a route and discard the one-off startup reconciler commands.
    fn seed(&mut self, route: AudioRoute) {
        let _ = self.route(route);
    }

    fn cmds(&mut self) -> Vec<PlaybackCommand> {
        self.app
            .take_outgoing()
            .into_iter()
            .filter_map(|o| match o {
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(c)) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn tick(&mut self, now_ms: u64) {
        self.app.handle_input(InputEvent::Tick { now_ms }, &mut self.wifi);
    }

    /// Publish a paused engine status at `pos`; the picture then sits exactly on `pos`.
    fn status(&mut self, pos: u64) {
        let session = SessionState {
            transport: TransportState::Paused,
            title: "Tone".into(),
            artist: "Artist".into(),
            position_ms: pos,
            rate_permille: 1000,
            duration_ms: Some(180_000),
            current_item: Some(PlayableItemId::new(1)),
            ..SessionState::default()
        };
        self.rev += 1;
        self.app.handle_ipc(IpcMessage::PlaybackStatus(
            PlaybackStatus::from_session_rev(session, self.rev),
        ));
    }

    fn seeks(&mut self) -> Vec<u64> {
        self.cmds()
            .into_iter()
            .filter_map(|c| match c {
                PlaybackCommand::SeekTo { target_ms } => Some(target_ms),
                _ => None,
            })
            .collect()
    }

    fn open_debug(&mut self) {
        self.app.nav.force_push(Route::System);
        let index = self
            .app
            .nav
            .system_items()
            .iter()
            .position(|r| *r == Route::SystemDebug)
            .unwrap();
        self.app.nav.focus = index;
        self.enter();
        assert_eq!(*self.app.nav.current(), Route::SystemDebug);
    }

    fn file(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }

    fn popup(&self) -> String {
        self.app
            .compositor
            .status()
            .map(|s| s.lines().join("|"))
            .unwrap_or_default()
    }

    fn open_audio(&mut self) {
        self.app.nav.force_push(Route::System);
        let index = self
            .app
            .nav
            .system_items()
            .iter()
            .position(|r| *r == Route::SystemAudio)
            .unwrap();
        self.app.nav.focus = index;
        self.enter();
        assert_eq!(*self.app.nav.current(), Route::SystemAudio);
        assert_eq!(self.app.nav.focus, 0);
    }

    fn open_outputs(&mut self) {
        self.open_audio();
        self.enter(); // Output devices (row 0)
        assert_eq!(*self.app.nav.current(), Route::SystemAudioOutputs);
    }

    fn open_delay(&mut self) {
        self.open_audio();
        self.app.nav.focus = 1;
        self.enter(); // Delay (row 1)
        assert_eq!(*self.app.nav.current(), Route::SystemAudioDelay);
    }

    fn pixels(&mut self) -> Vec<u16> {
        self.app.composed_pixels()
    }
}

fn output(id: &str, description: &str, available: bool, local: bool) -> AudioOutput {
    AudioOutput {
        id: id.into(),
        description: description.into(),
        available,
        is_local: local,
        ..AudioOutput::default()
    }
}

fn route(outputs: Vec<AudioOutput>, enabled: &[&str]) -> AudioRoute {
    AudioRoute {
        outputs,
        enabled: enabled.iter().map(|s| s.to_string()).collect(),
        delay_auto: true,
        ..AudioRoute::default()
    }
}

fn three_outputs() -> AudioRoute {
    route(
        vec![
            output("usb", "USB DAC", true, false),
            output("local", "Built-in", true, true),
            output("hdmi", "HDMI", true, false),
        ],
        &["local", "usb", "hdmi"],
    )
}

fn two_outputs(delay_auto: bool) -> AudioRoute {
    let mut r = route(
        vec![
            output("local", "Built-in", true, true),
            output("usb", "USB DAC", true, false),
        ],
        &["local", "usb"],
    );
    r.delay_auto = delay_auto;
    r
}

fn set_outputs(enabled: &[&str], local_only: bool) -> PlaybackCommand {
    PlaybackCommand::SetAudioOutputs {
        enabled: enabled.iter().map(|s| s.to_string()).collect(),
        local_only,
    }
}

fn with_delays(mut route: AudioRoute, delays: &[(&str, u32)]) -> AudioRoute {
    for (id, ms) in delays {
        if let Some(output) = route.outputs.iter_mut().find(|o| o.id == *id) {
            output.delay_ms = *ms;
        }
    }
    route.delay_auto = false;
    route
}

#[test]
fn back_from_child_returns_to_audio_on_the_opener_row() {
    let mut r = Rig::new();
    r.app.nav.force_push(Route::System);

    // System -> Audio, and its three rows are in order.
    r.open_audio();
    assert_eq!(Route::SystemAudio.title(), "Audio");
    assert_eq!(Route::SystemAudioOutputs.title(), "Output devices");
    assert_eq!(Route::SystemAudioDelay.title(), "Delay");

    // Output devices (row 0) -> Back lands on Audio with focus 0.
    r.enter();
    assert_eq!(*r.app.nav.current(), Route::SystemAudioOutputs);
    r.back();
    assert_eq!(*r.app.nav.current(), Route::SystemAudio);
    assert_eq!(r.app.nav.focus, 0);

    // Delay (row 1) -> Back lands on Audio with focus 1.
    r.app.nav.focus = 1;
    r.enter();
    assert_eq!(*r.app.nav.current(), Route::SystemAudioDelay);
    r.back();
    assert_eq!(*r.app.nav.current(), Route::SystemAudio);
    assert_eq!(r.app.nav.focus, 1);
}

#[test]
fn a_three_output_route_toggles_exactly_one_flipped_command() {
    let mut r = Rig::new();
    r.seed(three_outputs());
    r.open_outputs();

    // Sorted display order: local (row 1), HDMI (row 2), USB DAC (row 3).
    // Untick USB DAC.
    r.app.nav.focus = 3;
    r.enter();
    assert_eq!(r.cmds(), [set_outputs(&["local", "hdmi"], false)]);
    assert_eq!(r.app.model.audio.route.enabled, ["local", "usb", "hdmi"]);

    // The engine publishes the new route; only then does the screen change.
    let mut next = three_outputs();
    next.enabled = vec!["local".into(), "hdmi".into()];
    next.revision = 99;
    r.app.handle_ipc(IpcMessage::AudioRouteChanged(next));
    assert_eq!(r.app.model.audio.route.enabled, ["local", "hdmi"]);

    // Tick it back: exactly one command again.
    r.app.nav.focus = 3;
    r.enter();
    assert_eq!(r.cmds(), [set_outputs(&["local", "hdmi", "usb"], false)]);
}

#[test]
fn ticking_a_fifth_output_is_capped_before_send_and_save() {
    let mut r = Rig::new();
    let mut r0 = route(
        vec![
            output("local", "Built-in", true, true),
            output("a", "A", true, false),
            output("b", "B", true, false),
            output("c", "C", true, false),
            output("d", "D", true, false),
            output("e", "E", true, false),
        ],
        &["a", "b", "c", "d"],
    );
    r0.local_only = false;
    r.seed(r0);
    r.open_outputs();

    // Four extras are already ticked; tick the fifth. Sorted rows: Local only, then A..E.
    let row = r
        .app
        .model
        .audio
        .sorted_outputs()
        .iter()
        .position(|o| o.id == "e")
        .unwrap()
        + 1;
    r.app.nav.focus = row;
    r.enter();
    let cmds = r.cmds();
    assert_eq!(cmds.len(), 1, "{cmds:?}");
    match &cmds[0] {
        PlaybackCommand::SetAudioOutputs { enabled, .. } => {
            assert_eq!(
                enabled,
                &["a", "b", "c", "d"],
                "the command is capped at MAX_ENABLED_OUTPUTS"
            );
        }
        other => panic!("unexpected command {other:?}"),
    }
    let text = r.file();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let extras: Vec<&str> = v["extras"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x.as_str())
        .collect();
    assert_eq!(
        extras,
        ["a", "b", "c", "d"],
        "the over-limit id is not persisted: {text}"
    );
}

#[test]
fn an_empty_first_route_does_not_latch_the_startup_seed() {
    let mut r = Rig::with_settings(Some(
        r#"{"local_only":false,"extras":["usb"],"delay_auto":true,
            "delays_ms":{},"waveform_vis_delay":true}"#,
    ));
    let mut empty = AudioRoute {
        delay_auto: true,
        ..AudioRoute::default()
    };
    empty.revision = 1;
    r.app.handle_ipc(IpcMessage::AudioRouteChanged(empty));
    assert!(r.cmds().is_empty(), "nothing to seed before outputs exist");
    assert!(
        !r.app.audio_state.sent_initial,
        "an empty route must not latch the reconciler"
    );

    let mut first = two_outputs(true);
    first.revision = 2;
    r.app.handle_ipc(IpcMessage::AudioRouteChanged(first));
    assert_eq!(
        r.cmds(),
        [
            set_outputs(&["local", "usb"], false),
            PlaybackCommand::SetAudioDelayAuto(true),
            PlaybackCommand::SetVolume(Volume::new(80)),
        ],
        "the first route with outputs seeds once"
    );
    assert!(r.app.audio_state.sent_initial);
    assert!(r.route(two_outputs(true)).is_empty(), "and only once");
}

#[test]
fn local_only_blocks_output_ticks_with_a_popup_and_no_command() {
    let mut r = Rig::new();
    let mut r0 = two_outputs(true);
    r0.local_only = true;
    r0.enabled = vec!["local".into()];
    r0.paused = vec!["usb".into()];
    r.seed(r0);
    r.open_outputs();

    for row in 1..=2 {
        r.app.nav.focus = row;
        r.enter();
        assert!(r.cmds().is_empty(), "row {row} must not send a command");
        assert_eq!(r.popup(), "Local only is on", "row {row} popup");
    }

    // The Local only row itself is never disabled: Enter turns the mode off and brings the
    // paused extra back.
    r.app.nav.focus = 0;
    r.enter();
    assert_eq!(r.cmds(), [set_outputs(&["local", "usb"], false)]);
    assert_eq!(r.popup(), "Local only: Off");
    let text = r.file();
    assert!(text.contains("\"local_only\": false"), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    let extras: Vec<&str> = v["extras"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x.as_str())
        .collect();
    assert_eq!(
        extras,
        ["usb"],
        "the paused extra is restored and persisted: {text}"
    );
}

#[test]
fn unticking_the_only_extra_never_sends_an_empty_enabled_set() {
    let mut r = Rig::new();
    r.seed(two_outputs(true));
    r.open_outputs();

    // USB DAC is row 2. Removing it leaves the built-in output.
    r.app.nav.focus = 2;
    r.enter();
    assert_eq!(r.cmds(), [set_outputs(&["local"], false)]);

    // With only the built-in output left, unticking it is a no-op in the engine and the
    // command still names it, so playback cannot become silent by an untick.
    let mut only_local = two_outputs(true);
    only_local.enabled = vec!["local".into()];
    r.app.handle_ipc(IpcMessage::AudioRouteChanged(only_local));
    r.app.nav.focus = 1;
    r.enter();
    let cmds = r.cmds();
    assert_eq!(cmds.len(), 1, "{cmds:?}");
    match &cmds[0] {
        PlaybackCommand::SetAudioOutputs { enabled, .. } => {
            assert_eq!(enabled, &["local".to_string()], "never empty");
        }
        other => panic!("unexpected command {other:?}"),
    }
}

#[test]
fn delay_row_is_enter_then_scroll_and_sends_once_on_exit() {
    let mut r = Rig::new();
    r.seed(two_outputs(false));
    r.open_delay();

    // Row 0 is Auto, row 1 the built-in output, row 2 USB DAC.
    r.app.nav.focus = 2;
    r.enter();
    assert_eq!(r.app.model.audio.editing, Some(2));
    assert!(r.cmds().is_empty(), "arming a row sends nothing");

    // Four detents right from 0 ms → 40 ms; still no command while editing.
    r.rotate(4);
    assert_eq!(r.app.model.audio.delay_ms("usb"), 40);
    assert!(r.cmds().is_empty(), "no command while editing");

    // Enter leaves the row and sends exactly one command.
    r.enter();
    assert_eq!(r.app.model.audio.editing, None);
    assert_eq!(
        r.cmds(),
        [PlaybackCommand::SetOutputDelay {
            output: "usb".into(),
            delay_ms: 40
        }]
    );
    assert!(r.file().contains("\"usb\": 40"), "{}", r.file());

    // The top clamps at 1000 ms with no wrap.
    r.app.nav.focus = 2;
    r.enter();
    r.rotate(127);
    assert_eq!(r.app.model.audio.delay_ms("usb"), DELAY_MAX_MS);
    r.back();
    assert_eq!(
        r.cmds(),
        [PlaybackCommand::SetOutputDelay {
            output: "usb".into(),
            delay_ms: DELAY_MAX_MS
        }]
    );

    // The bottom clamps at 0 ms.
    r.app.nav.focus = 2;
    r.enter();
    r.rotate(-128);
    assert_eq!(r.app.model.audio.delay_ms("usb"), 0);
    r.back();
    assert_eq!(
        r.cmds(),
        [PlaybackCommand::SetOutputDelay {
            output: "usb".into(),
            delay_ms: 0
        }]
    );
    assert_eq!(DELAY_STEP_MS, 10);
}

#[test]
fn auto_toggles_with_one_command_and_saves() {
    let mut r = Rig::new();
    r.seed(two_outputs(true));
    r.open_delay();

    // Auto is on: the ms rows are disabled but Auto itself toggles.
    r.app.nav.focus = 0;
    r.enter();
    assert_eq!(
        r.cmds(),
        [PlaybackCommand::SetAudioDelayAuto(false)]
    );
    assert_eq!(r.popup(), "Delay: Manual");
    assert!(r.file().contains("\"delay_auto\": false"), "{}", r.file());
}

#[test]
fn delay_rows_are_disabled_under_auto_single_output_and_local_only() {
    // Auto on with two outputs.
    let mut r = Rig::new();
    r.seed(two_outputs(true));
    r.open_delay();
    r.app.nav.focus = 2;
    r.enter();
    assert!(r.cmds().is_empty());
    assert!(r.popup().contains("Automatic compensation"), "{}", r.popup());

    // Manual, but only one effective output.
    let mut r = Rig::new();
    let mut single = two_outputs(false);
    single.enabled = vec!["local".into()];
    r.seed(single);
    r.open_delay();
    r.app.nav.focus = 1;
    r.enter();
    assert!(r.cmds().is_empty());
    assert!(r.popup().contains("one output"), "{}", r.popup());

    // Manual with two outputs, but Local only is on.
    let mut r = Rig::new();
    let mut local_only = two_outputs(false);
    local_only.local_only = true;
    local_only.enabled = vec!["local".into()];
    local_only.paused = vec!["usb".into()];
    r.seed(local_only);
    r.open_delay();
    r.app.nav.focus = 1;
    r.enter();
    assert!(r.cmds().is_empty());
    assert!(r.popup().contains("Local only"), "{}", r.popup());
    // Auto is disabled too, and Enter explains why instead of changing the mode.
    r.app.nav.focus = 0;
    r.enter();
    assert!(r.cmds().is_empty());
    assert!(r.popup().contains("Local only"), "{}", r.popup());
}

#[test]
fn default_volume_row_steps_and_commits_exactly_once() {
    let mut r = Rig::new();
    r.open_audio();
    let row = AUDIO_ROWS
        .iter()
        .position(|row| *row == AudioRow::DefaultVolume)
        .unwrap();
    assert_eq!(row, 2, "Default volume sits between Delay and Accessories");
    r.app.nav.focus = row;
    r.enter();
    assert!(r.app.audio_state.volume_edit);
    assert!(r.cmds().is_empty(), "arming a row sends nothing");

    r.rotate(1);
    assert_eq!(r.app.model.audio.settings.default_volume, 85);
    assert!(r.cmds().is_empty(), "no command while stepping");
    r.rotate(1);
    assert_eq!(r.app.model.audio.settings.default_volume, 90);

    // The top and bottom clamp without wrapping.
    r.rotate(127);
    assert_eq!(r.app.model.audio.settings.default_volume, 100);
    r.rotate(127);
    assert_eq!(r.app.model.audio.settings.default_volume, 100);
    r.rotate(-128);
    assert_eq!(r.app.model.audio.settings.default_volume, 0);
    r.rotate(-128);
    assert_eq!(r.app.model.audio.settings.default_volume, 0);

    // Step back to a known value, still with no command, then commit once.
    r.rotate(17);
    assert_eq!(r.app.model.audio.settings.default_volume, 85);
    assert!(r.cmds().is_empty(), "no command while stepping");
    r.enter();
    assert!(!r.app.audio_state.volume_edit);
    assert_eq!(r.cmds(), [PlaybackCommand::SetVolume(Volume::new(85))]);
    let v: serde_json::Value = serde_json::from_str(&r.file()).unwrap();
    assert_eq!(v["default_volume"], 85, "{}", r.file());

    // Back also leaves the editor with exactly one command.
    r.app.nav.focus = row;
    r.enter();
    r.rotate(-1);
    r.back();
    assert!(!r.app.audio_state.volume_edit);
    assert_eq!(r.cmds(), [PlaybackCommand::SetVolume(Volume::new(80))]);
}

#[test]
fn persisted_settings_round_trip_and_the_local_tick_is_not_written() {
    // The store keeps all five keys.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(AUDIO_FILE_NAME);
    let mut store = AudioStore::with_path(&path);
    store.settings = AudioSettings {
        local_only: true,
        extras: vec!["usb".into()],
        delay_auto: false,
        delays_ms: BTreeMap::from([("usb".to_string(), 40)]),
        default_volume: 65,
        waveform_vis_delay: false,
    };
    store.save().unwrap();
    assert_eq!(AudioStore::load_or_default(&path).settings, store.settings);

    // A garbage value resets only its own key; the unknown key is ignored.
    let lenient = AudioSettings::from_json(
        r#"{"local_only":"yes","extras":["usb"],"unknown_key":7,
            "delay_auto":false,"waveform_vis_delay":true}"#,
    );
    assert!(!lenient.local_only, "bad key follows its default");
    assert_eq!(lenient.extras, ["usb"]);
    assert!(!lenient.delay_auto);
    assert!(lenient.waveform_vis_delay);

    // Through the UI: the built-in output's tick never reaches `extras`.
    let mut r = Rig::new();
    r.seed(two_outputs(true));
    r.open_outputs();
    r.app.nav.focus = 2; // USB DAC
    r.enter();
    assert!(r.cmds().iter().any(|c| matches!(c, PlaybackCommand::SetAudioOutputs { .. })));
    let text = r.file();
    assert!(
        text.contains("\"extras\": []"),
        "the untick cleared extras: {text}"
    );
    let extras = |text: &str| -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        v.get("extras")
            .and_then(|e| e.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    assert!(extras(&text).is_empty(), "no local tick: {text}");

    // The engine publishes the unticked route; ticking USB back stores only its id.
    let mut unticked = two_outputs(true);
    unticked.enabled = vec!["local".into()];
    r.app.handle_ipc(IpcMessage::AudioRouteChanged(unticked));
    r.app.nav.focus = 2;
    r.enter();
    let text = r.file();
    assert_eq!(extras(&text), ["usb"], "{text}");
}

#[test]
fn first_route_seeds_routing_and_delay_exactly_once() {
    // Auto: one routing command, one delay-mode command, nothing on the second route.
    let mut r = Rig::with_settings(Some(
        r#"{"local_only":false,"extras":["usb","gone"],"delay_auto":true,
            "delays_ms":{"usb":40},"waveform_vis_delay":true}"#,
    ));
    let mut first = route(
        vec![
            output("local", "Built-in", true, true),
            output("usb", "USB DAC", true, false),
            output("gone", "Unplugged", false, false),
        ],
        &["local"],
    );
    first.revision = 1;
    r.app.handle_ipc(IpcMessage::AudioRouteChanged(first));
    assert_eq!(
        r.cmds(),
        [
            set_outputs(&["local", "usb"], false),
            PlaybackCommand::SetAudioDelayAuto(true),
            PlaybackCommand::SetVolume(Volume::new(80)),
        ],
        "extras filtered against available ids"
    );
    // A second route sends nothing, even if it differs.
    let mut second = two_outputs(true);
    second.delay_auto = false;
    assert!(r.route(second).is_empty(), "the reconciler runs once");

    // Manual: the same routing command, Auto off, then each persisted trim naming a known
    // output (an unknown id is skipped).
    let mut r = Rig::with_settings(Some(
        r#"{"local_only":true,"extras":["usb"],"delay_auto":false,
            "delays_ms":{"usb":40,"gone":10},"waveform_vis_delay":true}"#,
    ));
    let mut first = route(
        vec![
            output("local", "Built-in", true, true),
            output("usb", "USB DAC", true, false),
        ],
        &["local"],
    );
    first.revision = 1;
    r.app.handle_ipc(IpcMessage::AudioRouteChanged(first));
    assert_eq!(
        r.cmds(),
        [
            set_outputs(&["local", "usb"], true),
            PlaybackCommand::SetAudioDelayAuto(false),
            PlaybackCommand::SetOutputDelay {
                output: "usb".into(),
                delay_ms: 40
            },
            PlaybackCommand::SetVolume(Volume::new(80)),
        ],
        "only known outputs are seeded"
    );
    assert!(r.route(two_outputs(false)).is_empty());
}

#[test]
fn a_zero_output_route_paints_the_empty_state_and_enter_does_nothing() {
    let mut r = Rig::new();
    let empty = AudioRoute {
        delay_auto: true,
        ..AudioRoute::default()
    };
    r.seed(empty);
    r.open_outputs();
    r.app.nav.focus = 0;
    r.enter();
    assert!(r.cmds().is_empty(), "Enter on the empty state does nothing");
    // Something other than the header/footer chrome is painted (the centred message state).
    let px = r.pixels();
    let body_ink = (CONTENT_TOP as usize..200)
        .flat_map(|y| (MARGIN_X as usize..(SCREEN_W - MARGIN_X) as usize).map(move |x| (x, y)))
        .filter(|&(x, y)| px[y * SCREEN_W as usize + x] != BG)
        .count();
    assert!(body_ink > 50, "no empty-state message painted ({body_ink})");
}

#[test]
fn a_hostile_long_description_is_sanitised_and_truncated() {
    let mut r = Rig::new();
    let hostile = format!("{} [2J\n\u{202e}", "X".repeat(300));
    let mut r0 = route(
        vec![
            output("local", "Built-in", true, true),
            output("usb", &hostile, true, false),
        ],
        &["local", "usb"],
    );
    r0.delay_auto = false;
    r.seed(r0);
    let description = r
        .app
        .model
        .audio
        .route
        .output("usb")
        .unwrap()
        .description
        .clone();
    assert!(
        description.chars().count() <= plippa_ui::SCREEN_TEXT_MAX_CHARS,
        "{} chars",
        description.chars().count()
    );
    assert!(!description.contains('\u{202e}'), "bidi override stripped");
    assert!(!description.contains('\n'), "one line only");

    // The output row (not focused) draws only inside the content box: no ink in the right
    // margin, and no panic with the 120-glyph label.
    r.open_outputs();
    let px = r.pixels();
    let row_y = CONTENT_TOP + ROW_H + SPACE_1; // sorted row 1: local
    let row_y = row_y + ROW_H + SPACE_1; // sorted row 2: the hostile description
    for y in row_y..row_y + ROW_H {
        for x in (SCREEN_W - MARGIN_X)..SCREEN_W {
            assert_eq!(
                px[y as usize * SCREEN_W as usize + x as usize],
                BG,
                "ink in the right margin at ({x},{y})"
            );
        }
    }
    // And the delay screen draws the same label without an overdraw.
    r.back();
    r.app.nav.focus = 1;
    r.enter();
    let _ = r.pixels();
    assert_eq!(MAX_ENABLED_OUTPUTS, 4, "the cap the engine enforces");
}

// ---- AUDIO_05: waveform visual delay -------------------------------------------------------

#[test]
fn visual_offset_is_zero_when_off_or_single_and_follows_the_route() {
    let mut r = Rig::new();
    assert_eq!(r.app.vis_offset_ms(), 0, "no route yet");

    let three = with_delays(three_outputs(), &[("usb", 300), ("hdmi", 120)]);
    r.seed(three.clone());
    assert_eq!(r.app.vis_offset_ms(), 300, "max of the three delays");

    r.app.audio.settings.waveform_vis_delay = false;
    assert_eq!(r.app.vis_offset_ms(), 0, "the setting is off");

    r.app.audio.settings.waveform_vis_delay = true;
    let single = with_delays(
        route(vec![output("local", "Built-in", true, true)], &["local"]),
        &[("local", 400)],
    );
    r.seed(single);
    assert_eq!(r.app.vis_offset_ms(), 0, "one output has nothing to sync against");

    let updated = with_delays(three_outputs(), &[("usb", 500), ("hdmi", 120)]);
    r.seed(updated);
    assert_eq!(
        r.app.vis_offset_ms(),
        500,
        "a route update applies without a restart"
    );
}

#[test]
fn visual_offset_moves_the_picture_and_clock_and_clamps_at_zero() {
    let mut r = Rig::new();
    r.seed(with_delays(three_outputs(), &[("usb", 300)]));
    r.tick(1_000);
    r.status(10_000);
    // The un-offset playhead is what every seek computation reads.
    assert_eq!(r.app.playhead_ms_at(1_000), 10_000);
    // The picture and the footer clock read the one compensated value.
    assert_eq!(r.app.model.playhead_ms, 9_700);

    // 400 ms of offset with only 200 ms played: the picture stays at 0:00, audio is untouched.
    r.seed(with_delays(three_outputs(), &[("usb", 400)]));
    r.status(200);
    assert_eq!(r.app.playhead_ms_at(1_000), 200);
    assert_eq!(r.app.model.playhead_ms, 0, "saturating_sub clamps at 0:00");
}

#[test]
fn a_visual_offset_never_changes_a_jog_seek_target() {
    let targets: Vec<u64> = [false, true]
        .into_iter()
        .map(|offset_on| {
            let mut r = Rig::new();
            if offset_on {
                r.seed(with_delays(three_outputs(), &[("usb", 300)]));
            }
            r.tick(1_000);
            r.status(65_000);
            if offset_on {
                assert_eq!(r.app.vis_offset_ms(), 300, "the offset is active in the 'on' branch");
            } else {
                assert_eq!(r.app.vis_offset_ms(), 0, "the offset is off in the 'off' branch");
            }
            let _ = r.cmds();
            r.input(InputEvent::MenuRotate(1));
            assert!(r.seeks().is_empty(), "nothing is sent while the knob turns");
            r.tick(1_000 + JOG_COMMIT_IDLE_MS);
            let seeks = r.seeks();
            assert_eq!(seeks.len(), 1, "one commit per gesture");
            seeks[0]
        })
        .collect();
    assert_eq!(
        targets[0], targets[1],
        "the visual offset must never leak into SeekTo"
    );
}

// ---- AUDIO_05: Debug > Audio info ----------------------------------------------------------

#[test]
fn debug_audio_info_opens_and_scrolls_without_wrapping() {
    let mut r = Rig::new();
    r.seed(three_outputs());
    r.open_debug();
    r.app.nav.focus = 1; // `Audio info`, after `Rec enables NFC poll`
    r.enter();
    assert_eq!(*r.app.nav.current(), Route::SystemDebugAudio);
    assert_eq!(Route::SystemDebugAudio.title(), "Audio info");

    let len = audio_report(
        &r.app.audio_state.route,
        &r.app.audio.settings,
        &r.app.accessories,
        r.app.accessories.available,
        0,
    )
    .len();
    assert!(len > 1);
    for _ in 0..len + 5 {
        r.rotate(-1); // menu inversion: left = down
    }
    assert_eq!(r.app.nav.focus, len - 1, "clamped at the last report line");
    for _ in 0..len + 5 {
        r.rotate(1);
    }
    assert_eq!(r.app.nav.focus, 0, "clamped at the first report line");

    r.back();
    assert_eq!(*r.app.nav.current(), Route::SystemDebug);
    assert_eq!(r.app.nav.focus, 1, "Back returns to the row that opened it");
}
