//! In-process MVP_08 integration harness.
//!
//! **Not multi-process UDS** — wires `UiApp` + `plippa_engine::Engine` + mock NFC
//! payloads in one process so CI can prove tap→play and miss→error without hardware.
//! Full UDS transport lands with the launcher process wiring later.

use plippa_core::{TransportState, Volume};
use plippa_engine::{Engine, EngineEvent};
use plippa_hw::{NfcEvent, TagInfo, TagType};
use plippa_ipc_proto::{
    IpcMessage, NOTICE_LIBRARY_MISS, PlaybackCommand,
};
use plippa_noubin::{normalise_noubin_key, LibraryRoot, LibraryWriter};
use plippa_ui::{
    default_mock_wifi, LibraryItem, MockDisplay, SystemStatus, UiApp, UiConfig, UiOutgoing,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

fn audio_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/plippa-audio/tests/fixtures")
        .join(name)
}

fn text_ndef(text: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(2);
    payload.extend_from_slice(b"en");
    payload.extend_from_slice(text.as_bytes());
    let mut out = vec![0xD1, 0x01, payload.len() as u8, b'T'];
    out.extend_from_slice(&payload);
    out
}

fn one_track_library(key: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    fs::copy(audio_fixture("tone_48k_16.wav"), root.join("tone.wav")).unwrap();
    let noudata = format!(
        r#"{{
          "noubinKey": "{key}",
          "noubinKeyNormalised": "{}",
          "playableItemTitle": "Tone",
          "mediaList": [{{"localFilePaths": ["tone.wav"]}}]
        }}"#,
        normalise_noubin_key(key)
    );
    fs::write(root.join("_tone.noudata"), noudata).unwrap();
    let lib = LibraryRoot::new(&root);
    LibraryWriter::open(&lib).unwrap().full_scan().unwrap();
    (tmp, root)
}

fn drain_engine_to_ui(engine: &mut Engine, ui: &mut UiApp) {
    for ev in engine.take_events() {
        match ev {
            EngineEvent::Status(s) => ui.handle_ipc(IpcMessage::PlaybackStatus(s)),
            EngineEvent::Notice(n) => ui.handle_ipc(IpcMessage::UserNotice(n)),
            EngineEvent::NowPlaying(l) => ui.handle_ipc(IpcMessage::NowPlayingListChanged(l)),
            EngineEvent::AudioRoute(r) => ui.handle_ipc(IpcMessage::AudioRouteChanged(r)),
            // Engine → server only (long-form resume write-through).
            EngineEvent::ResumeWriteback(_) | EngineEvent::UnmatchedNoubin(_) => {}
        }
    }
}

fn apply_ui_outgoing(engine: &mut Engine, ui: &mut UiApp) {
    for out in ui.take_outgoing() {
        match out {
            UiOutgoing::Ipc(msg) => {
                engine.handle_ipc(msg);
            }
            UiOutgoing::ShutdownRequest { .. }
            | UiOutgoing::TapFeedback { .. }
            | UiOutgoing::AssociateSelected { .. }
            | UiOutgoing::CancelAssociate => {}
        }
    }
}

#[test]
fn integration_tap_hit_playing() {
    let key = "Tone.noubin.HIT";
    let (_tmp, root) = one_track_library(key);
    let mut engine = Engine::open_mock(&root, Default::default()).unwrap();
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(
        Volume::new(50),
    )));

    let mut ui = UiApp::new(UiConfig::default()).with_system(SystemStatus {
        time_hhmm: "12:00".into(),
        temperature_c: Some(42),
        fan_status: "idle".into(),
        portal_url: None,
        ip: None,
        ips: Vec::new(),
    });
    ui.set_library(vec![LibraryItem {
        id: 1,
        title: "Tone".into(),
        associated: true,
        noudata_relpath: "_tone.noudata".into(),
        library_root: None,
        artist: None,
        duration_ms: None,
        art_path: None,
        item_position: None,
    }]);
    let mut _wifi = default_mock_wifi();
    let mut display = MockDisplay::new(240, 320);

    ui.handle_nfc_event(NfcEvent::ReadComplete {
        info: TagInfo {
            uid: vec![1, 2, 3],
            tag_type: TagType::Type2,
            ndef_capacity: Some(144),
        },
        ndef: text_ndef(key),
    });
    apply_ui_outgoing(&mut engine, &mut ui);
    // Allow audio thread to start.
    thread::sleep(Duration::from_millis(50));
    drain_engine_to_ui(&mut engine, &mut ui);
    ui.present(&mut display);

    assert_eq!(ui.model.transport_label(), "Playing");
    assert!(display.frames >= 1);
    assert!(engine.session().transport == TransportState::Playing);
}

#[test]
fn integration_tap_miss_error() {
    let (_tmp, root) = one_track_library("Known.noubin.ONLY");
    let mut engine = Engine::open_mock(&root, Default::default()).unwrap();
    let mut ui = UiApp::new(UiConfig::default());
    let mut display = MockDisplay::new(240, 320);

    ui.handle_nfc_event(NfcEvent::ReadComplete {
        info: TagInfo {
            uid: vec![9, 9, 9],
            tag_type: TagType::Type2,
            ndef_capacity: Some(144),
        },
        ndef: text_ndef("Missing.noubin.MISS"),
    });
    apply_ui_outgoing(&mut engine, &mut ui);
    drain_engine_to_ui(&mut engine, &mut ui);
    ui.present(&mut display);

    assert!(
        ui.model
            .banner
            .as_ref()
            .map(|b| b.to_lowercase().contains("no local") || b.contains("Noubin"))
            .unwrap_or(false)
            || ui
                .model
                .notices
                .iter()
                .any(|n| n.code == NOTICE_LIBRARY_MISS),
        "expected miss banner/notice, got banner={:?} notices={:?}",
        ui.model.banner,
        ui.model.notices
    );
    assert_eq!(engine.session().transport, TransportState::Stopped);
}
