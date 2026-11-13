//! ACC_03: Accessories list, Pair new, detail and output auto-enable.
//!
//! The radio is the real `MockAccessoryBackend` driven by `AccessoryWorker` in-process; the
//! tests act as the coordinator's clock. No engine, no wall-clock sleeps.

use plippa_core::audio_route::{AudioOutput, AudioRoute};
use plippa_hw::{
    normalise_address, Accessory, AccessoryEvent, AccessoryKind, AdapterInfo,
    MockAccessoryBackend, PairPrompt, PromptAnswer, TransportAction,
};
use plippa_ipc_proto::{IpcMessage, PlaybackCommand};
use plippa_ui::ui::tokens::{BG, CONTENT_TOP, MARGIN_X, SCREEN_W};
use plippa_ui::{
    screen_text, AccessoryCmd, AccessoryHandle, AccessoryMsg, AccessoryStore, AccessoryWorker,
    AudioRow, InputEvent, KeyboardField, MockWifi, PairStage, Route, UiApp, UiConfig, UiOutgoing,
    ACCESSORY_FILE_NAME, AUDIO_FILE_NAME, AUDIO_ROWS, SCREEN_TEXT_MAX_CHARS,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

const ID: &str = "aa:bb:cc:dd:ee:01";
const ID2: &str = "aa:bb:cc:dd:ee:02";

fn adapter() -> AdapterInfo {
    AdapterInfo {
        id: "hci0".into(),
        address: "11:22:33:44:55:66".into(),
        name: "radio".into(),
        powered: true,
    }
}

/// A peer prompt payload (SYS-03: every prompt names the device).
fn peer(name: &str) -> plippa_hw::Peer {
    plippa_hw::Peer {
        name: Some(name.into()),
        address: "58:8A:D2:D8:22:7F".into(),
    }
}

fn accessory(id: &str, name: &str, connected: bool) -> Accessory {
    Accessory {
        id: normalise_address(id),
        name: name.into(),
        kind: AccessoryKind::Audio,
        paired: true,
        trusted: true,
        connected,
        battery_pct: None,
    }
}

struct Rig {
    app: UiApp,
    wifi: MockWifi,
    cmd_rx: Receiver<AccessoryCmd>,
    evt_tx: Sender<AccessoryMsg>,
    worker: AccessoryWorker<MockAccessoryBackend>,
    /// Every accessory command the app sent, in order.
    sent: Vec<AccessoryCmd>,
    path: PathBuf,
    audio_path: PathBuf,
    now: u64,
    rev: u64,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn new(mock: MockAccessoryBackend) -> Self {
        Self::with_settings(None, mock)
    }

    fn with_settings(settings: Option<&str>, mock: MockAccessoryBackend) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ACCESSORY_FILE_NAME);
        if let Some(text) = settings {
            std::fs::write(&path, text).unwrap();
        }
        let audio_path = dir.path().join(AUDIO_FILE_NAME);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (evt_tx, evt_rx) = mpsc::channel();
        let app = UiApp::new(UiConfig {
            audio_path: Some(audio_path.clone()),
            play_mode_path: Some(dir.path().join("play_mode.json")),
            accessories_path: Some(path.clone()),
            accessories: Some(AccessoryHandle::new(cmd_tx, evt_rx)),
            ..UiConfig::default()
        });
        let mut rig = Self {
            app,
            wifi: MockWifi::default(),
            cmd_rx,
            evt_tx,
            worker: AccessoryWorker::new(mock),
            sent: Vec::new(),
            path,
            audio_path,
            now: 0,
            rev: 0,
            _dir: dir,
        };
        let snapshot = rig.worker.snapshot();
        rig.evt_tx.send(snapshot).ok();
        rig.pump(0);
        rig
    }

    /// Act as the coordinator: drain messages, run commands through the worker, drain events.
    fn pump(&mut self, now: u64) {
        self.now = now;
        for _ in 0..512 {
            self.app.poll_accessories(now);
            let mut progressed = false;
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                progressed = true;
                self.sent.push(cmd.clone());
                let msg = self.worker.apply(cmd);
                if !msg.is_empty() {
                    let _ = self.evt_tx.send(msg);
                }
            }
            let msg = self.worker.poll();
            if !msg.is_empty() {
                let _ = self.evt_tx.send(msg);
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
    }

    fn send_event(&mut self, event: AccessoryEvent) {
        self.evt_tx
            .send(AccessoryMsg::Events(vec![event]))
            .unwrap();
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

    fn file(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }

    fn audio_file(&self) -> String {
        std::fs::read_to_string(&self.audio_path).unwrap_or_default()
    }

    fn popup(&self) -> String {
        self.app
            .compositor
            .status()
            .map(|s| s.lines().join("|"))
            .unwrap_or_default()
    }

    fn playback_cmds(&mut self) -> Vec<PlaybackCommand> {
        self.app
            .take_outgoing()
            .into_iter()
            .filter_map(|o| match o {
                UiOutgoing::Ipc(IpcMessage::PlaybackCommand(c)) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn publish_route(&mut self, mut route: AudioRoute) -> Vec<PlaybackCommand> {
        self.rev += 1;
        route.revision = self.rev;
        self.app.handle_ipc(IpcMessage::AudioRouteChanged(route));
        self.playback_cmds()
    }

    fn seed_route(&mut self, route: AudioRoute) {
        let _ = self.publish_route(route);
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
    }

    fn open_accessories(&mut self) {
        self.open_audio();
        let index = AUDIO_ROWS
            .iter()
            .position(|r| *r == AudioRow::Accessories)
            .unwrap();
        self.app.nav.focus = index;
        self.enter();
        assert_eq!(*self.app.nav.current(), Route::SystemAudioAccessories);
    }

    fn open_pair_new(&mut self) {
        self.open_accessories();
        self.app.nav.focus = self.app.accessories.sorted_known().len() + 1;
        self.enter();
        assert_eq!(*self.app.nav.current(), Route::SystemAudioPairNew);
        self.pump(self.now);
    }

    fn open_detail(&mut self, index: usize) {
        self.open_accessories();
        self.app.nav.focus = index;
        self.enter();
        assert_eq!(*self.app.nav.current(), Route::SystemAudioAccessory);
    }

    fn pixels(&mut self) -> Vec<u16> {
        self.app.composed_pixels()
    }
}

fn output(id: &str, address: &str, local: bool) -> AudioOutput {
    let mut props = BTreeMap::new();
    props.insert("device.address".into(), address.into());
    AudioOutput {
        id: id.into(),
        description: format!("{id} output"),
        available: true,
        is_local: local,
        props,
        ..AudioOutput::default()
    }
}

fn route_with_match(enabled: &[&str], local_only: bool) -> AudioRoute {
    AudioRoute {
        outputs: vec![
            output("local", "", true),
            output("usb", "AA-BB-CC-DD-EE-01", false),
        ],
        enabled: enabled.iter().map(|s| s.to_string()).collect(),
        local_only,
        delay_auto: true,
        ..AudioRoute::default()
    }
}

fn connects(sent: &[AccessoryCmd], id: &str) -> usize {
    sent.iter()
        .filter(|c| matches!(c, AccessoryCmd::Connect(connect) if connect == id))
        .count()
}

fn set_outputs(cmd: &PlaybackCommand) -> Option<(&[String], bool)> {
    match cmd {
        PlaybackCommand::SetAudioOutputs {
            enabled,
            local_only,
        } => Some((enabled, *local_only)),
        _ => None,
    }
}

#[test]
fn audio_screen_has_five_rows_in_documented_order() {
    assert_eq!(
        AUDIO_ROWS,
        [
            AudioRow::Outputs,
            AudioRow::Delay,
            AudioRow::DefaultVolume,
            AudioRow::Accessories,
            AudioRow::WaveformVisDelay
        ]
    );
    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.open_audio();
    r.app.nav.focus = 3;
    r.enter();
    assert_eq!(*r.app.nav.current(), Route::SystemAudioAccessories);
    assert_eq!(Route::SystemAudioAccessories.title(), "Accessories");
    assert_eq!(Route::SystemAudioPairNew.title(), "Pair new");
}

#[test]
fn list_is_sorted_connected_first_and_pair_new_is_last() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Zed", false))
        .with_accessory(accessory(ID2, "Amy", true));
    let mut r = Rig::new(mock);
    r.open_accessories();
    let names: Vec<String> = r
        .app
        .accessories
        .sorted_known()
        .iter()
        .map(|a| a.name.clone())
        .collect();
    assert_eq!(names, ["Amy", "Zed"], "connected first, then name");
    // The list is selectable: row 0 opens the connected accessory, the last row pairs.
    r.app.nav.focus = 0;
    r.enter();
    assert_eq!(*r.app.nav.current(), Route::SystemAudioAccessory);
    r.back();
    assert_eq!(*r.app.nav.current(), Route::SystemAudioAccessories);
    r.app.nav.focus = 3;
    r.enter();
    assert_eq!(*r.app.nav.current(), Route::SystemAudioPairNew);
}

#[test]
fn an_empty_list_is_still_navigable_to_pair_new() {
    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.open_accessories();
    assert!(r.app.accessories.known.is_empty());
    r.app.nav.focus = 1;
    r.enter();
    assert_eq!(*r.app.nav.current(), Route::SystemAudioPairNew);
    let px = r.pixels();
    let ink = (CONTENT_TOP as usize..220)
        .flat_map(|y| (MARGIN_X as usize..(SCREEN_W - MARGIN_X) as usize).map(move |x| (x, y)))
        .filter(|&(x, y)| px[y * SCREEN_W as usize + x] != BG)
        .count();
    assert!(ink > 50, "nothing was painted ({ink})");
}

#[test]
fn pair_new_sends_pair_once_then_confirm_and_refresh() {
    let mut unpaired = accessory(ID, "Speaker", false);
    unpaired.paired = false;
    let mut mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(unpaired);
    mock.script_scan([(ID.to_string(), "Speaker".to_string(), AccessoryKind::Audio)]);
    let mut r = Rig::new(mock);
    r.open_pair_new();
    assert_eq!(r.app.accessories.found.len(), 1, "the scan reaches the screen");
    r.app.nav.focus = 0;
    r.enter();
    assert_eq!(r.app.accessories.pending_action, None);
    r.pump(r.now);
    assert_eq!(connects(&r.sent, ID), 0);
    assert_eq!(
        r.sent
            .iter()
            .filter(|c| matches!(c, AccessoryCmd::Pair(id) if id == ID))
            .count(),
        1,
        "{:?}",
        r.sent
    );
    // A confirm prompt (the mock's own prompt is Authorise; replace it with the code the
    // owner must accept).
    r.send_event(AccessoryEvent::Prompt(PairPrompt::Confirm { peer: peer("Speaker"), passkey: 1234 }));
    r.pump(r.now);
    assert_eq!(r.app.accessories.stage, PairStage::Prompt(PairPrompt::Confirm { peer: peer("Speaker"), passkey: 1234 }, None));
    r.enter();
    r.pump(r.now);
    assert!(
        r.sent.iter().any(|c| matches!(c, AccessoryCmd::Answer(PromptAnswer::Accept))),
        "{:?}",
        r.sent
    );
    assert!(
        r.sent.iter().any(|c| matches!(c, AccessoryCmd::Refresh)),
        "the list is refreshed after pairing"
    );
    assert!(r.popup().contains("Speaker paired"), "{}", r.popup());
    assert_eq!(r.app.accessories.stage, PairStage::Idle);
    assert_eq!(*r.app.nav.current(), Route::SystemAudioAccessories);
}

#[test]
fn passkey_accepts_digits_only_and_sends_the_number() {
    // The field itself can never produce a letter.
    let mut kb = KeyboardField::digits_only();
    kb.handle(plippa_ui::KeyboardEvent::Enter);
    for _ in 0..80 {
        kb.handle(plippa_ui::KeyboardEvent::Rotate(1));
        assert!(kb.current_char().is_ascii_digit(), "{}", kb.current_char());
    }
    kb.handle(plippa_ui::KeyboardEvent::Rotate(-128));
    assert!(kb.current_char().is_ascii_digit());

    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.open_accessories();
    r.send_event(AccessoryEvent::Prompt(PairPrompt::EnterPasskey { peer: peer("Speaker") }));
    r.pump(r.now);
    let kb = r.app.accessories.keyboard.as_mut().expect("passkey keyboard");
    *kb = KeyboardField::from_str("1234");
    r.rotate(1); // blank → OK
    r.enter();
    r.pump(r.now);
    assert!(
        r.sent
            .iter()
            .any(|c| matches!(c, AccessoryCmd::Answer(PromptAnswer::Passkey(1234)))),
        "{:?}",
        r.sent
    );
}

#[test]
fn back_rejects_a_prompt_and_stops_a_scan() {
    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.open_accessories();
    r.send_event(AccessoryEvent::Prompt(PairPrompt::Authorise { peer: peer("Speaker") }));
    r.pump(r.now);
    assert!(matches!(r.app.accessories.stage, PairStage::Prompt(..)));
    r.back();
    r.pump(r.now);
    assert!(
        r.sent
            .iter()
            .any(|c| matches!(c, AccessoryCmd::Answer(PromptAnswer::Reject))),
        "{:?}",
        r.sent
    );
    assert_eq!(r.app.accessories.stage, PairStage::Idle);

    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.open_pair_new();
    assert!(r.app.accessories.is_scanning());
    r.back();
    r.pump(r.now);
    assert!(
        r.sent.iter().any(|c| matches!(c, AccessoryCmd::StopScan)),
        "{:?}",
        r.sent
    );
    assert_eq!(*r.app.nav.current(), Route::SystemAudioAccessories);
}

#[test]
fn detail_connect_auto_connect_and_confirmed_forget() {
    let mut mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false));
    mock.script_connect(ID);
    let mut r = Rig::new(mock);
    r.seed_route(route_with_match(&["local", "usb"], false));
    r.open_detail(0);

    // Connect sends the command and disables the row while it is in flight.
    r.app.nav.focus = 0;
    r.enter();
    assert_eq!(r.app.accessories.pending_action.as_deref(), Some(ID));
    r.pump(r.now);
    assert!(connects(&r.sent, ID) == 1, "{:?}", r.sent);
    assert!(r.app.accessories.pending_action.is_none());
    r.playback_cmds();

    // Auto connect toggles and persists without touching the radio.
    let before = r.sent.len();
    r.app.nav.focus = 1;
    r.enter();
    assert_eq!(r.sent.len(), before, "no radio command for a local setting");
    let text = r.file();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["auto_connect"][0], ID, "{text}");
    assert_eq!(AccessoryStore::load_or_default(&r.path).settings.auto_connect.len(), 1);

    // Forget asks first; Back cancels and sends nothing.
    r.app.nav.focus = 3;
    r.enter();
    assert!(r.app.accessories.forget_confirm.is_some());
    r.back();
    assert!(r.app.accessories.forget_confirm.is_none());
    assert!(!r.sent.iter().any(|c| matches!(c, AccessoryCmd::Forget(_))));

    // Enter on the confirmation forgets, drops auto-connect and unticked the matching output.
    r.app.nav.focus = 3;
    r.enter();
    r.enter();
    r.pump(r.now);
    assert!(
        r.sent.iter().any(|c| matches!(c, AccessoryCmd::Forget(id) if id == ID)),
        "{:?}",
        r.sent
    );
    let cmds = r.playback_cmds();
    let outputs = cmds
        .iter()
        .find_map(set_outputs)
        .unwrap_or_else(|| panic!("no SetAudioOutputs: {cmds:?}"));
    assert_eq!(outputs.0, ["local"], "the accessory output is unticked");
    let v: serde_json::Value = serde_json::from_str(&r.file()).unwrap();
    assert_eq!(v["auto_connect"].as_array().unwrap().len(), 0);
    assert_eq!(*r.app.nav.current(), Route::SystemAudioAccessories);
}

#[test]
fn auto_connect_only_when_nothing_is_connected_and_once_per_budget() {
    let settings = format!(r#"{{"auto_connect":["{ID}"],"scan_seconds":30}}"#);
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false))
        .with_accessory(accessory(ID2, "Headset", true));
    let r = Rig::with_settings(Some(&settings), mock);
    assert_eq!(connects(&r.sent, ID), 0, "B is already connected");

    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false));
    let mut r = Rig::with_settings(Some(&settings), mock);
    assert_eq!(connects(&r.sent, ID), 1, "{:?}", r.sent);
    // Another event batch with the same state does not try again (budget / pending).
    r.send_event(AccessoryEvent::Adapters(vec![adapter()]));
    r.pump(r.now);
    assert_eq!(connects(&r.sent, ID), 1, "{:?}", r.sent);
}

#[test]
fn a_successful_pair_sets_trust_then_connects_without_a_duplicate() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false));
    let mut r = Rig::new(mock);
    // A known auto-connect target that is not connected: if `maybe_auto_connect` ran again
    // in this batch after `Connected` cleared pending, it would resend the Connect.
    r.app.accessories.known = vec![accessory(ID, "Speaker", false)];
    r.app.accessories.settings.auto_connect.insert(normalise_address(ID));
    r.sent.clear();

    r.evt_tx
        .send(AccessoryMsg::Events(vec![
            AccessoryEvent::Paired {
                id: ID.into(),
                ok: true,
                error: None,
            },
            AccessoryEvent::Connected(ID.into()),
        ]))
        .unwrap();
    r.pump(r.now);

    let trusted: Vec<usize> = r
        .sent
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            matches!(c, AccessoryCmd::SetTrusted { id, trusted: true } if id == ID).then_some(i)
        })
        .collect();
    let connections: Vec<usize> = r
        .sent
        .iter()
        .enumerate()
        .filter_map(|(i, c)| matches!(c, AccessoryCmd::Connect(id) if id == ID).then_some(i))
        .collect();
    assert_eq!(trusted.len(), 1, "{:?}", r.sent);
    assert_eq!(connections.len(), 1, "no duplicate Connect: {:?}", r.sent);
    assert!(
        trusted[0] < connections[0],
        "trust must be sent before connect: {:?}",
        r.sent
    );
    assert!(
        r.sent.iter().any(|c| matches!(c, AccessoryCmd::Refresh)),
        "the list is refreshed after pairing"
    );
    assert!(r.popup().contains("Speaker paired"), "{}", r.popup());
    assert!(!r.app.accessories.is_unavailable(ID));
}

#[test]
fn a_failed_connect_marks_not_in_range_until_a_connect_clears_it() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false));
    let mut r = Rig::new(mock);
    r.open_detail(0);

    r.app.nav.focus = 0;
    r.enter();
    assert_eq!(r.app.accessories.pending_action.as_deref(), Some(ID));
    // Act as a backend that failed: do not run the queued Connect, report the failure.
    while let Ok(cmd) = r.cmd_rx.try_recv() {
        r.sent.push(cmd);
    }
    r.send_event(AccessoryEvent::Failed("connection failed".into()));
    r.pump(r.now);

    assert!(r.app.accessories.pending_action.is_none());
    assert!(r.app.accessories.pending_connect.is_none());
    assert!(r.app.accessories.is_unavailable(ID));
    let status = r.app.accessories.status_text(&r.app.accessories.known[0]);
    assert_eq!(status, "not in range");
    let _ = r.pixels();

    // A later Connected clears the flag; the refreshed snapshot then reads `connected`.
    r.send_event(AccessoryEvent::Connected(ID.into()));
    r.pump(r.now);
    assert!(!r.app.accessories.is_unavailable(ID));
    r.evt_tx
        .send(AccessoryMsg::Snapshot {
            adapters: vec![adapter()],
            known: vec![accessory(ID, "Speaker", true)],
            unavailable: false,
            events: Vec::new(),
        })
        .unwrap();
    r.pump(r.now);
    let status = r.app.accessories.status_text(&r.app.accessories.known[0]);
    assert_eq!(status, "connected");
}

#[test]
fn a_failed_disconnect_does_not_mark_the_accessory_out_of_range() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", true));
    let mut r = Rig::new(mock);
    r.open_detail(0);

    r.app.nav.focus = 0;
    r.enter();
    assert_eq!(r.app.accessories.pending_action.as_deref(), Some(ID));
    while let Ok(cmd) = r.cmd_rx.try_recv() {
        r.sent.push(cmd);
    }
    r.send_event(AccessoryEvent::Failed("disconnect failed".into()));
    r.pump(r.now);

    assert!(r.app.accessories.pending_action.is_none());
    assert!(!r.app.accessories.is_unavailable(ID));
    let status = r.app.accessories.status_text(&r.app.accessories.known[0]);
    assert_eq!(status, "connected");
}

#[test]
fn a_scan_or_pair_failure_does_not_mark_a_device_unavailable() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false));
    let mut r = Rig::new(mock);
    r.open_pair_new();

    r.send_event(AccessoryEvent::Failed(
        "The accessory radio is unavailable".to_string(),
    ));
    r.pump(1_000);
    assert!(!r.app.accessories.is_unavailable(ID));

    r.app.accessories.stage = PairStage::Pairing(ID.into());
    r.app.accessories.pairing_id = Some(ID.into());
    r.send_event(AccessoryEvent::Paired {
        id: ID.into(),
        ok: false,
        error: Some("rejected".into()),
    });
    r.pump(2_000);
    assert!(!r.app.accessories.is_unavailable(ID));
}

#[test]
fn connect_enables_a_matching_output_through_set_audio_outputs() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false));
    let mut r = Rig::new(mock);
    r.seed_route(route_with_match(&["local"], false));
    r.send_event(AccessoryEvent::Connected(ID.into()));
    r.pump(r.now);
    let cmds = r.playback_cmds();
    let (enabled, local_only) = cmds
        .iter()
        .find_map(set_outputs)
        .unwrap_or_else(|| panic!("no SetAudioOutputs: {cmds:?}"));
    assert_eq!(enabled, ["local", "usb"], "the matching output is ticked");
    assert!(!local_only);
    assert!(
        r.audio_file().contains("\"usb\""),
        "the tick is persisted through AudioStore: {}",
        r.audio_file()
    );
}

#[test]
fn an_accessory_connect_never_leaves_the_local_output_out() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false));
    let mut r = Rig::new(mock);
    // Even a route that already has the accessory ticked alone is repaired to local first.
    r.seed_route(route_with_match(&["usb"], false));
    r.send_event(AccessoryEvent::Connected(ID.into()));
    r.pump(r.now);
    let cmds = r.playback_cmds();
    let (enabled, _) = cmds
        .iter()
        .find_map(set_outputs)
        .unwrap_or_else(|| panic!("no SetAudioOutputs: {cmds:?}"));
    assert_eq!(enabled, ["local", "usb"]);
}

#[test]
fn local_only_pauses_the_output_and_no_matching_output_notices_at_the_deadline() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false));
    let mut r = Rig::new(mock);
    let mut local = route_with_match(&["local"], true);
    local.paused = vec!["usb".into()];
    r.seed_route(local);
    r.send_event(AccessoryEvent::Connected(ID.into()));
    r.pump(1_000);
    assert!(r.playback_cmds().iter().all(|c| set_outputs(c).is_none()));
    assert!(r.popup().contains("local only"), "{}", r.popup());

    let mut unmatched = route_with_match(&["local"], false);
    for output in &mut unmatched.outputs {
        output.props.clear();
    }
    let mut r = Rig::new(MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false)));
    r.seed_route(unmatched);
    r.send_event(AccessoryEvent::Connected(ID.into()));
    r.pump(1_000);
    assert!(r.app.accessories.pending_output.is_some(), "waiting for a route");

    // The route appears: the retry ticks the output.
    let cmds = r.publish_route(route_with_match(&["local"], false));
    assert!(
        cmds.iter().any(|c| set_outputs(c).is_some()),
        "the pending output is enabled when it appears: {cmds:?}"
    );

    // Or the deadline passes: exactly one notice, then no pending output.
    let mut r = Rig::new(MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false)));
    let mut unmatched = route_with_match(&["local"], false);
    for output in &mut unmatched.outputs {
        output.props.clear();
    }
    r.seed_route(unmatched);
    r.send_event(AccessoryEvent::Connected(ID.into()));
    r.pump(1_000);
    r.pump(6_100);
    assert!(r.app.accessories.pending_output.is_none());
    assert!(r.popup().contains("no audio output found"), "{}", r.popup());
}

#[test]
fn no_backend_shows_no_radio_and_enter_does_nothing() {
    let mut app = UiApp::new(UiConfig::default());
    let mut wifi = MockWifi::default();
    app.nav.force_push(Route::System);
    let index = app
        .nav
        .system_items()
        .iter()
        .position(|r| *r == Route::SystemAudio)
        .unwrap();
    app.nav.focus = index;
    app.handle_input(InputEvent::MenuEnter, &mut wifi);
    app.nav.focus = 3;
    app.handle_input(InputEvent::MenuEnter, &mut wifi);
    assert_eq!(*app.nav.current(), Route::SystemAudioAccessories);
    assert!(!app.accessories.has_radio());
    app.handle_input(InputEvent::MenuEnter, &mut wifi);
    assert_eq!(*app.nav.current(), Route::SystemAudioAccessories, "no scan without a radio");
    let px = app.composed_pixels();
    let ink = (CONTENT_TOP as usize..220)
        .flat_map(|y| (MARGIN_X as usize..(SCREEN_W - MARGIN_X) as usize).map(move |x| (x, y)))
        .filter(|&(x, y)| px[y * SCREEN_W as usize + x] != BG)
        .count();
    assert!(ink > 20, "No radio state was not painted ({ink})");
}

#[test]
fn a_corrupt_accessories_file_does_not_stop_audio_settings_loading() {
    let dir = tempfile::tempdir().unwrap();
    let accessories_path = dir.path().join(ACCESSORY_FILE_NAME);
    let audio_path = dir.path().join(AUDIO_FILE_NAME);
    std::fs::write(&accessories_path, "not json {{").unwrap();
    std::fs::write(
        &audio_path,
        r#"{"local_only":true,"extras":["usb"],"delay_auto":false,
            "delays_ms":{},"waveform_vis_delay":true}"#,
    )
    .unwrap();
    let app = UiApp::new(UiConfig {
        audio_path: Some(audio_path),
        play_mode_path: Some(dir.path().join("play_mode.json")),
        accessories_path: Some(accessories_path),
        ..UiConfig::default()
    });
    assert!(app.audio.settings.local_only, "audio keys still load");
    assert_eq!(app.audio.settings.extras, ["usb"]);
    assert_eq!(
        app.accessories.settings,
        plippa_ui::AccessorySettings::default(),
        "the corrupt accessory file only resets its own settings"
    );
}

#[test]
fn a_200_char_name_is_sanitised_and_truncated_everywhere() {
    let hostile = format!("{}\u{202e}\u{0007}", "A".repeat(200));
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, &hostile, false));
    let mut r = Rig::new(mock);
    r.open_accessories();
    let _ = r.pixels();
    r.open_detail(0);
    let _ = r.pixels();

    let shown = screen_text(&r.app.accessories.known[0].name);
    assert!(shown.chars().count() <= SCREEN_TEXT_MAX_CHARS, "{}", shown.chars().count());
    assert!(!shown.chars().any(|c| c.is_control() || c == '\u{202e}'));

    let report = plippa_ui::audio_report(
        &r.app.audio_state.route,
        &r.app.audio.settings,
        &r.app.accessories,
        r.app.accessories.available,
        r.now,
    );
    let name = report
        .iter()
        .find(|line| line.label == "accessory 1")
        .expect("accessory line");
    assert!(name.value.chars().count() <= 40, "{}", name.value);
    assert!(!name.value.chars().any(|c| c.is_control() || c == '\u{202e}'));
}

#[test]
fn pairing_from_the_found_list_closes_the_scan_window_and_releases_auto_connect() {
    let settings = format!(r#"{{"auto_connect":["{ID}"],"scan_seconds":30}}"#);
    let mut unpaired = accessory(ID, "Speaker", false);
    unpaired.paired = false;
    let mut mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(unpaired);
    mock.script_scan([(ID.to_string(), "Speaker".to_string(), AccessoryKind::Audio)]);
    mock.script_pair(ID, [PromptAnswer::Accept]);
    let mut r = Rig::with_settings(Some(&settings), mock);
    r.open_pair_new();
    assert!(r.app.accessories.is_scanning(), "scan is still open");
    assert!(r.app.accessories.scan_window.is_some());
    assert_eq!(r.app.accessories.found.len(), 1);

    r.app.nav.focus = 0;
    r.enter();
    assert_eq!(
        r.app.accessories.stage,
        PairStage::Pairing(ID.into()),
        "pairing starts from the found row"
    );
    assert!(
        r.app.accessories.scan_window.is_none(),
        "the scan window closes as soon as pairing starts"
    );
    r.pump(1_000);
    assert!(
        matches!(r.app.accessories.stage, PairStage::Prompt(..)),
        "{:?}",
        r.app.accessories.stage
    );
    r.enter();
    r.pump(2_000);
    assert_eq!(r.app.accessories.stage, PairStage::Idle);
    assert!(r.app.accessories.pairing_id.is_none());
    assert!(r.app.accessories.scan_window.is_none());
    assert!(
        r.app.accessories.auto_connect_allowed(),
        "a completed pair must not keep auto-connect blocked"
    );
    assert_eq!(connects(&r.sent, ID), 1, "{:?}", r.sent);
}

#[test]
fn a_forget_confirmation_expires_with_its_popup_and_must_be_armed_again() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false));
    let mut r = Rig::new(mock);
    r.open_detail(0);
    r.app.nav.focus = 3;
    r.enter();
    assert!(r.app.accessories.forget_confirm.is_some());
    assert!(r.popup().contains("Forget"), "{}", r.popup());

    r.pump(3_001);
    assert!(
        r.app.accessories.forget_confirm.is_none(),
        "the confirmation dies with the popup"
    );
    r.enter();
    assert!(
        !r.sent
            .iter()
            .any(|c| matches!(c, AccessoryCmd::Forget(_))),
        "a stale Enter must not forget: {:?}",
        r.sent
    );
    assert!(
        r.app.accessories.forget_confirm.is_some(),
        "Enter re-arms the confirmation"
    );

    r.enter();
    r.pump(r.now);
    assert!(
        r.sent
            .iter()
            .any(|c| matches!(c, AccessoryCmd::Forget(id) if id == ID)),
        "a fresh arm followed by Enter forgets: {:?}",
        r.sent
    );
}

#[test]
fn auto_connect_never_fires_while_a_pairing_is_in_flight_after_back() {
    let settings = format!(r#"{{"auto_connect":["{ID}"],"scan_seconds":30}}"#);
    let mut r = Rig::with_settings(
        Some(&settings),
        MockAccessoryBackend::new().with_adapter(adapter()),
    );
    r.app.accessories.stage = PairStage::Idle;
    r.app.accessories.pairing_id = Some(ID.into());
    r.app.accessories.known = vec![accessory(ID, "Speaker", false)];
    assert!(!r.app.accessories.auto_connect_allowed());
    assert!(r.app.accessories.is_busy_pairing());

    r.send_event(AccessoryEvent::Adapters(vec![adapter()]));
    r.pump(r.now);
    assert_eq!(
        connects(&r.sent, ID),
        0,
        "a live pairing must block auto-connect: {:?}",
        r.sent
    );

    r.app.accessories.clear_pairing();
    assert!(r.app.accessories.auto_connect_allowed());
    r.send_event(AccessoryEvent::Adapters(vec![adapter()]));
    r.pump(r.now);
    assert_eq!(connects(&r.sent, ID), 1, "{:?}", r.sent);
}

#[test]
fn only_the_accessories_audio_row_has_a_chevron() {
    use plippa_ui::ui::tokens::{FG_MUTED, ROW_H, SPACE_1, SPACE_3};

    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.open_audio();
    let px = r.pixels();
    let right = SCREEN_W - MARGIN_X;
    let row_y = |i: u16| CONTENT_TOP + i.saturating_mul(ROW_H.saturating_add(SPACE_1));
    let chevron_ink = |i: u16| {
        let y = row_y(i);
        (y..y + ROW_H)
            .flat_map(|yy| (right - SPACE_3..right).map(move |x| (x, yy)))
            .filter(|&(x, y)| px[y as usize * SCREEN_W as usize + x as usize] == FG_MUTED)
            .count()
    };
    assert!(chevron_ink(3) > 0, "Accessories row has no chevron");
    assert_eq!(chevron_ink(4), 0, "Waveform vis delay gained a chevron");
}

#[test]
fn back_during_a_prompt_rejects_and_returns_to_the_accessories_list() {
    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.open_pair_new();
    r.send_event(AccessoryEvent::Prompt(PairPrompt::Authorise { peer: peer("Speaker") }));
    r.pump(r.now);
    assert_eq!(*r.app.nav.current(), Route::SystemAudioPairNew);
    r.back();
    r.pump(r.now);
    assert_eq!(
        r.sent
            .iter()
            .filter(|c| matches!(c, AccessoryCmd::Answer(PromptAnswer::Reject)))
            .count(),
        1,
        "{:?}",
        r.sent
    );
    assert_eq!(r.app.accessories.stage, PairStage::Idle);
    assert_eq!(*r.app.nav.current(), Route::SystemAudioAccessories);

    // The same prompt arriving after the owner backed out to the list still rejects and
    // leaves the list in charge.
    r.send_event(AccessoryEvent::Prompt(PairPrompt::Authorise { peer: peer("Speaker") }));
    r.pump(r.now);
    assert!(matches!(r.app.accessories.stage, PairStage::Prompt(..)));
    r.back();
    r.pump(r.now);
    assert_eq!(
        r.sent
            .iter()
            .filter(|c| matches!(c, AccessoryCmd::Answer(PromptAnswer::Reject)))
            .count(),
        2,
        "{:?}",
        r.sent
    );
    assert_eq!(r.app.accessories.stage, PairStage::Idle);
    assert_eq!(*r.app.nav.current(), Route::SystemAudioAccessories);
}

#[test]
fn a_failed_start_scan_returns_to_idle_and_shows_the_mapped_popup() {
    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.open_pair_new();
    assert!(r.app.accessories.is_scanning());

    r.send_event(AccessoryEvent::Failed(
        "The accessory radio is unavailable".to_string(),
    ));
    r.pump(1_000);
    assert_eq!(r.app.accessories.stage, PairStage::Idle);
    assert!(r.app.accessories.scan_window.is_none());
    assert_eq!(r.app.accessories.scan_seconds_left(), 0, "no phantom countdown");
    assert!(
        r.popup().contains("accessory radio is unavailable"),
        "{}",
        r.popup()
    );

    r.pump(31_000);
    assert_eq!(r.app.accessories.stage, PairStage::Idle);
    assert!(r.app.accessories.scan_window.is_none());
}

#[test]
fn a_pairing_timeout_without_a_prompt_does_not_send_an_answer() {
    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.app.accessories.stage = PairStage::Pairing(ID.into());
    r.app.accessories.pairing_id = Some(ID.into());
    r.app.accessories.set_pairing_deadline(0);
    r.pump(60_001);
    assert_eq!(r.app.accessories.stage, PairStage::Idle);
    assert!(r.app.accessories.pairing_id.is_none());
    assert!(
        !r.sent
            .iter()
            .any(|c| matches!(c, AccessoryCmd::Answer(_))),
        "no prompt was outstanding: {:?}",
        r.sent
    );
    assert!(r.popup().contains("Pairing timed out"), "{}", r.popup());
}

#[test]
fn a_pairing_timeout_with_a_prompt_rejects_it() {
    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.app.accessories.stage = PairStage::Prompt(PairPrompt::Authorise { peer: peer("Speaker") }, None);
    r.app.accessories.pairing_id = Some(ID.into());
    r.app.accessories.set_pairing_deadline(0);
    r.pump(60_001);
    assert_eq!(r.app.accessories.stage, PairStage::Idle);
    assert!(r.app.accessories.pairing_id.is_none());
    assert!(
        r.sent
            .iter()
            .any(|c| matches!(c, AccessoryCmd::Answer(PromptAnswer::Reject))),
        "{:?}",
        r.sent
    );
}

#[test]
fn scan_finished_keeps_the_found_list_and_shows_the_count_popup() {
    let mut mock = MockAccessoryBackend::new().with_adapter(adapter());
    mock.script_scan([(ID.to_string(), "Speaker".to_string(), AccessoryKind::Audio)]);
    let mut r = Rig::new(mock);
    r.open_pair_new();
    assert_eq!(r.app.accessories.found.len(), 1);

    r.send_event(AccessoryEvent::ScanFinished);
    r.pump(1_000);
    assert_eq!(r.app.accessories.stage, PairStage::Idle);
    assert!(!r.app.accessories.is_scanning());
    assert!(r.app.accessories.scan_window.is_none());
    assert_eq!(r.app.accessories.found.len(), 1, "the found list stays");
    assert!(r.popup().contains("1 found"), "{}", r.popup());

    r.app.nav.focus = 0;
    assert_eq!(r.app.accessories.found.len(), 1, "the found list stays");
    r.enter();
    r.pump(r.now);
    assert!(
        r.sent
            .iter()
            .any(|c| matches!(c, AccessoryCmd::Pair(id) if id == ID)),
        "the finished list is still selectable: {:?}",
        r.sent
    );
}

#[test]
fn disconnected_clears_pending_output_and_leaves_the_output_ticked() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false));
    let mut r = Rig::new(mock);
    r.seed_route(route_with_match(&["local", "usb"], false));
    r.send_event(AccessoryEvent::Connected(ID.into()));
    r.pump(r.now);
    r.playback_cmds();

    r.app.accessories.pending_output = Some((ID.into(), r.now + 5_000));
    r.send_event(AccessoryEvent::Disconnected(ID.into()));
    r.pump(r.now);
    assert!(r.app.accessories.pending_output.is_none());
    assert!(
        r.playback_cmds().iter().all(|c| set_outputs(c).is_none()),
        "Disconnected must not untick the output"
    );
    assert!(
        r.app.audio_state.route.enabled.iter().any(|id| id == "usb"),
        "the output tick survives a disconnect"
    );
}

#[test]
fn a_corrupt_audio_file_does_not_stop_accessories_loading() {
    let dir = tempfile::tempdir().unwrap();
    let accessories_path = dir.path().join(ACCESSORY_FILE_NAME);
    let audio_path = dir.path().join(AUDIO_FILE_NAME);
    std::fs::write(
        &accessories_path,
        format!(r#"{{"auto_connect":["{ID}"],"scan_seconds":45}}"#),
    )
    .unwrap();
    std::fs::write(&audio_path, "not json {{").unwrap();
    let app = UiApp::new(UiConfig {
        audio_path: Some(audio_path),
        play_mode_path: Some(dir.path().join("play_mode.json")),
        accessories_path: Some(accessories_path),
        ..UiConfig::default()
    });
    assert_eq!(app.accessories.settings.scan_seconds, 45);
    assert!(app.accessories.settings.auto_connect.contains(ID));
    assert_eq!(app.audio.settings, plippa_ui::AudioSettings::default());
}

#[test]
fn repeated_identical_failures_collapse_within_thirty_seconds_at_the_ui() {
    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.send_event(AccessoryEvent::Failed("Radio offline".into()));
    r.pump(1_000);
    assert!(r.popup().contains("Radio offline"), "{}", r.popup());

    r.app.compositor.clear_status();
    r.send_event(AccessoryEvent::Failed("Radio offline".into()));
    r.pump(2_000);
    assert!(r.popup().is_empty(), "the repeat is collapsed: {}", r.popup());

    r.app.compositor.clear_status();
    r.send_event(AccessoryEvent::Failed("Radio offline".into()));
    r.pump(32_000);
    assert!(
        r.popup().contains("Radio offline"),
        "a fresh failure after 30 s is shown again: {}",
        r.popup()
    );
}

#[test]
fn the_connected_accessory_row_draws_the_tick_icon() {
    use plippa_ui::ui::icons::{Icon, ICON_H, ICON_W};
    use plippa_ui::ui::tokens::{ROW_H, SPACE_1};

    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Zed", true))
        .with_accessory(accessory(ID2, "Amy", false));
    let mut r = Rig::new(mock);
    r.open_accessories();
    r.app.nav.focus = 3; // Pair new, so neither accessory row is focused
    let px = r.pixels();
    assert_eq!(
        r.app.accessories.sorted_known()[0].name,
        "Zed",
        "the connected row comes first"
    );
    let icon_x = SCREEN_W - MARGIN_X - ICON_W;
    let row_y = |i: u16| CONTENT_TOP + i.saturating_mul(ROW_H.saturating_add(SPACE_1));
    let icon_y = |y: u16| y + (ROW_H - ICON_H) / 2;
    let has_ink = |row: u16, icon: Icon| {
        let y = icon_y(row_y(row));
        for (dy, bits) in icon.rows().iter().enumerate() {
            for dx in 0..ICON_W {
                let painted = px[(y as usize + dy) * SCREEN_W as usize
                    + (icon_x + dx) as usize]
                    != BG;
                assert_eq!(
                    painted,
                    bits & (1 << dx) != 0,
                    "icon pixel ({dx},{dy}) on row {row}"
                );
            }
        }
    };
    has_ink(0, Icon::Check);
    has_ink(1, Icon::BoxEmpty);
}

#[test]
fn the_list_cursor_is_clamped_when_the_list_shrinks() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "A", false))
        .with_accessory(accessory(ID2, "B", false));
    let mut r = Rig::new(mock);
    r.open_accessories();
    r.app.nav.focus = 2; // Pair new, the last of three rows
    assert_eq!(*r.app.nav.current(), Route::SystemAudioAccessories);

    r.evt_tx
        .send(AccessoryMsg::Snapshot {
            adapters: vec![adapter()],
            known: vec![accessory(ID, "A", false)],
            unavailable: false,
            events: Vec::new(),
        })
        .unwrap();
    r.pump(r.now);
    assert_eq!(r.app.nav.focus, 1, "two rows: accessory + Pair new");

    r.evt_tx
        .send(AccessoryMsg::Snapshot {
            adapters: vec![adapter()],
            known: Vec::new(),
            unavailable: false,
            events: Vec::new(),
        })
        .unwrap();
    r.pump(r.now);
    assert_eq!(r.app.nav.focus, 0, "an empty list still has Pair new");
    let _ = r.pixels();
}

#[test]
fn an_unavailable_snapshot_is_not_an_empty_usable_list() {
    let mut r = Rig::new(MockAccessoryBackend::unavailable());
    assert!(!r.app.accessories.available);
    assert!(!r.app.accessories.has_radio());
    r.open_accessories();
    r.enter();
    assert_eq!(*r.app.nav.current(), Route::SystemAudioAccessories);
    let _ = r.pixels();
}

#[test]
fn a_dead_worker_channel_reports_no_radio() {
    let (cmd_tx, _cmd_rx) = mpsc::channel();
    let (evt_tx, evt_rx) = mpsc::channel();
    drop(evt_tx);
    let mut app = UiApp::new(UiConfig {
        accessories: Some(AccessoryHandle::new(cmd_tx, evt_rx)),
        ..UiConfig::default()
    });
    app.poll_accessories(0);
    assert!(!app.accessories.available);
    assert!(!app.accessories.has_radio());
    assert!(app.accessories.known.is_empty());
}

#[test]
fn unpaired_devices_are_not_listed_as_accessories() {
    let mut unpaired = accessory(ID2, "Not paired yet", false);
    unpaired.paired = false;
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false))
        .with_accessory(unpaired);

    let r = Rig::new(mock);
    assert_eq!(r.app.accessories.known.len(), 1, "only paired rows");
    assert_eq!(r.app.accessories.known[0].id, normalise_address(ID));
}

#[test]
fn scan_hides_other_devices_and_neutralises_address_names() {
    let mock = MockAccessoryBackend::new().with_adapter(adapter());
    let mut r = Rig::new(mock);
    r.open_pair_new();

    r.send_event(AccessoryEvent::Discovered {
        id: "aa:bb:cc:dd:ee:03".into(),
        name: "7E-99-A8-5C-51-94".into(),
        kind: AccessoryKind::Other,
    });
    r.send_event(AccessoryEvent::Discovered {
        id: "aa:bb:cc:dd:ee:04".into(),
        name: "58-8A-D2-D8-22-7F".into(),
        kind: AccessoryKind::Audio,
    });
    r.pump(r.now);

    assert_eq!(r.app.accessories.found.len(), 1, "Other is not an accessory");
    assert_eq!(r.app.accessories.found[0].name, "Unknown accessory");
    assert_eq!(r.app.accessories.found[0].kind, AccessoryKind::Audio);
}

#[test]
fn transport_buttons_send_the_matching_playback_command_when_enabled() {
    let mut r = Rig::new(MockAccessoryBackend::new().with_adapter(adapter()));
    r.pump(r.now);
    let cases = [
        (TransportAction::Play, PlaybackCommand::Play),
        (TransportAction::Pause, PlaybackCommand::Pause),
        (TransportAction::Stop, PlaybackCommand::Stop),
        (TransportAction::Next, PlaybackCommand::Next),
        (TransportAction::Previous, PlaybackCommand::Previous),
    ];
    for (action, expected) in cases {
        r.send_event(AccessoryEvent::Transport(action));
        r.pump(r.now);
        assert_eq!(r.playback_cmds(), vec![expected], "{action:?}");
    }
    for action in [TransportAction::FastForward, TransportAction::Rewind] {
        r.send_event(AccessoryEvent::Transport(action));
        r.pump(r.now);
        assert!(
            r.playback_cmds().is_empty(),
            "{action:?} has no single-press mapping"
        );
    }
}

#[test]
fn the_transport_policy_is_per_accessory_and_persists() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", true));
    let mut r = Rig::new(mock);
    assert!(r.app.accessories.is_transport_enabled(ID), "default on");

    r.open_detail(0);
    r.app.nav.focus = 2;
    r.enter();
    r.pump(r.now);
    assert!(!r.app.accessories.is_transport_enabled(ID));
    assert!(r.popup().contains("Transport commands: Off"), "{}", r.popup());
    let v: serde_json::Value = serde_json::from_str(&r.file()).unwrap();
    assert_eq!(
        v["transport_disabled"],
        serde_json::json!([ID]),
        "{}",
        r.file()
    );

    r.enter();
    r.pump(r.now);
    assert!(r.app.accessories.is_transport_enabled(ID));
    let v: serde_json::Value = serde_json::from_str(&r.file()).unwrap();
    assert_eq!(v["transport_disabled"], serde_json::json!([]), "{}", r.file());
}

#[test]
fn transport_buttons_are_ignored_for_an_accessory_with_the_policy_off() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", true));
    let mut r = Rig::new(mock);
    r.app.accessories.set_transport_enabled(ID, false);

    for action in [
        TransportAction::Play,
        TransportAction::Pause,
        TransportAction::Stop,
        TransportAction::Next,
        TransportAction::Previous,
    ] {
        r.send_event(AccessoryEvent::Transport(action));
        r.pump(r.now);
        assert!(r.playback_cmds().is_empty(), "{action:?}");
    }

    r.app.accessories.set_transport_enabled(ID, true);
    r.send_event(AccessoryEvent::Transport(TransportAction::Play));
    r.pump(r.now);
    assert_eq!(r.playback_cmds(), vec![PlaybackCommand::Play]);
}

#[test]
fn the_detail_rows_are_connect_auto_transport_forget() {
    let mock = MockAccessoryBackend::new()
        .with_adapter(adapter())
        .with_accessory(accessory(ID, "Speaker", false));
    let mut r = Rig::new(mock);
    r.open_detail(0);
    assert_eq!(r.app.nav.focus, 0);
    r.rotate(-1);
    assert_eq!(r.app.nav.focus, 1);
    r.rotate(-1);
    assert_eq!(r.app.nav.focus, 2, "transport row");
    r.rotate(-1);
    assert_eq!(r.app.nav.focus, 3, "forget row");
    r.rotate(-1);
    assert_eq!(r.app.nav.focus, 0, "four rows wrap");
}
