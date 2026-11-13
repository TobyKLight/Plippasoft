//! AUDIO_03: engine routing integration on the mock sink factory.
//!
//! These tests assert *what device and latency profile the engine asked for* and
//! *when it re-opened*, which the routed mock factory records. No audio server is
//! involved, so they run on the host build.

use plippa_audio::route::{
    MockOutputControl, OutputControl, RouteError, RouteEvent, RouteIntent, SinkInfo,
};
use plippa_audio::testsupport::{frame_index_sample, write_pcm16_wav};
use plippa_audio::PulseConfig;
use plippa_core::{Capabilities, TransportState};
use plippa_engine::{Engine, EngineEvent, RouteManager, RouteTarget, RouteTargetCell};
use plippa_ipc_proto::{
    IpcMessage, PlaybackCommand, TapKeyPayload, NOTICE_AUDIO_ROUTE_FAILED,
};
use plippa_noubin::{normalise_noubin_key, LibraryRoot, LibraryWriter};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

const RATE: u32 = 48_000;

fn library(key: &str, frames: usize) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    write_pcm16_wav(&root.join("tone.wav"), RATE, 1, frames, |f, _| {
        frame_index_sample(f)
    })
    .unwrap();
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

fn sink(id: &str, latency_ms: u32, is_default: bool) -> SinkInfo {
    SinkInfo {
        id: id.into(),
        description: id.into(),
        available: true,
        latency_ms,
        props: BTreeMap::new(),
        is_default,
    }
}

fn three_sinks() -> Vec<SinkInfo> {
    vec![
        sink("local", 24, true),
        sink("usb", 40, false),
        sink("aux", 30, false),
    ]
}

fn manager(mock: &MockOutputControl) -> RouteManager {
    RouteManager::new(Box::new(mock.clone()), Some("local".into()), None)
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(5), "timed out: {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_frames(engine: &Engine, min: u64) {
    wait_until("audio frames", || engine.frames_written() >= min);
}

fn wait_opens(engine: &Engine, n: usize) {
    wait_until("stream open", || engine.mock_stream_opens().len() >= n);
}

fn play(engine: &mut Engine, key: &str) {
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));
    wait_frames(engine, 128);
}

fn set_route(engine: &mut Engine, enabled: &[&str], local_only: bool) {
    engine.handle_ipc(IpcMessage::PlaybackCommand(
        PlaybackCommand::SetAudioOutputs {
            enabled: enabled.iter().map(|s| (*s).to_string()).collect(),
            local_only,
        },
    ));
    engine.poll_audio_route();
}

fn set_outputs(engine: &mut Engine, enabled: &[&str]) {
    set_route(engine, enabled, false);
}

#[test]
fn local_only_routes_into_the_local_sink() {
    let key = "route.local.audio03";
    let (_tmp, root) = library(key, RATE as usize);
    let mock = MockOutputControl::new(three_sinks());
    let cell: RouteTargetCell = RouteTarget::default_cell();
    let mut engine =
        Engine::open_mock_routed(&root, Capabilities::desktop_mock(), cell.clone()).unwrap();
    engine.attach_route(manager(&mock), cell);
    set_route(&mut engine, &["local", "usb", "aux"], true);
    play(&mut engine, key);

    let route = engine.audio_route().unwrap();
    assert!(route.local_only, "the command genuinely turned local-only on");
    assert_eq!(route.enabled, ["local"]);
    assert_eq!(route.paused, ["usb", "aux"]);

    let opens = engine.mock_stream_opens();
    assert_eq!(opens.len(), 1, "one main stream: {opens:?}");
    assert_eq!(opens[0].dev.as_deref(), Some("local"));
    assert_eq!(opens[0].cfg, Some(PulseConfig::low_latency()));
}

#[test]
fn a_stream_on_the_server_default_reopens_once_when_the_route_resolves() {
    let key = "route.default.audio03";
    let (_tmp, root) = library(key, RATE as usize);
    let mock = MockOutputControl::new(Vec::new());
    let cell = RouteTarget::default_cell();
    let mut engine =
        Engine::open_mock_routed(&root, Capabilities::desktop_mock(), cell.clone()).unwrap();
    engine.attach_route(manager(&mock), cell);
    assert!(
        engine.mock_stream_opens().is_empty(),
        "attach alone must not open a stream"
    );

    play(&mut engine, key);
    let opens = engine.mock_stream_opens();
    assert_eq!(opens.len(), 1);
    assert_eq!(
        opens[0].dev, None,
        "nothing discovered yet: the server default is the target"
    );

    let starts = engine.stream_prepare_count();
    mock.set_sinks(three_sinks());
    mock.push_event(RouteEvent::SinksChanged);
    engine.poll_audio_route();
    wait_opens(&engine, 2);

    assert_eq!(
        engine.stream_prepare_count(),
        starts + 1,
        "the first resolved route re-opens the default stream once"
    );
    let opens = engine.mock_stream_opens();
    assert_eq!(opens.len(), 2);
    assert_eq!(opens.last().unwrap().dev.as_deref(), Some("local"));
    assert_eq!(engine.audio_route().unwrap().enabled, ["local"]);
}

#[test]
fn failed_route_while_playing_keeps_the_previous_route_and_reopens_it_once() {
    let key = "route.failure.playing.audio03";
    let (_tmp, root) = library(key, RATE as usize);
    let mock = MockOutputControl::new(three_sinks());
    let cell = RouteTarget::default_cell();
    let mut engine =
        Engine::open_mock_routed(&root, Capabilities::desktop_mock(), cell.clone()).unwrap();
    engine.attach_route(manager(&mock), cell);
    play(&mut engine, key);
    wait_opens(&engine, 1);
    let opens_before = engine.mock_stream_opens().len();
    let starts = engine.stream_prepare_count();
    engine.take_events();

    mock.fail_next_apply("combine module missing");
    set_outputs(&mut engine, &["local", "usb"]);
    wait_opens(&engine, opens_before + 1);

    assert_eq!(
        engine.stream_prepare_count(),
        starts + 1,
        "one recovery re-open, not a loop"
    );
    let opens = engine.mock_stream_opens();
    assert_eq!(opens.len(), opens_before + 1);
    assert_eq!(
        opens.last().unwrap().dev.as_deref(),
        Some("local"),
        "the kept route is local, so the recovery stream goes there"
    );
    assert_eq!(
        opens.last().unwrap().cfg,
        Some(PulseConfig::low_latency())
    );

    let route = engine.audio_route().unwrap();
    assert_eq!(
        route.enabled,
        ["local"],
        "the failed candidate is not published; the previous route is kept"
    );
    let problem = route.problem.clone().expect("problem line");
    assert!(problem.contains("combine module missing"), "{problem}");

    let events = engine.take_events();
    let notices: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Notice(n) => Some(n),
            _ => None,
        })
        .collect();
    assert!(
        notices.iter().any(|n| n.code == NOTICE_AUDIO_ROUTE_FAILED),
        "the failure is reported: {notices:?}"
    );
    let route_events = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::AudioRoute(_)))
        .count();
    assert_eq!(route_events, 1, "one command publishes the route once");
}

#[test]
fn enabling_a_second_output_moves_the_stream_once() {
    let key = "route.second.audio03";
    let (_tmp, root) = library(key, RATE as usize);
    let mock = MockOutputControl::new(three_sinks());
    let cell = RouteTarget::default_cell();
    let mut engine =
        Engine::open_mock_routed(&root, Capabilities::desktop_mock(), cell.clone()).unwrap();
    engine.attach_route(manager(&mock), cell);
    play(&mut engine, key);
    let before = engine.mock_stream_opens().len();
    assert_eq!(before, 1);

    set_outputs(&mut engine, &["local", "usb"]);
    wait_opens(&engine, before + 1);
    let opens = engine.mock_stream_opens();
    assert_eq!(opens.len(), before + 1, "exactly one re-open: {opens:?}");
    assert_eq!(opens.last().unwrap().dev.as_deref(), Some("plippa.multi"));
    assert_eq!(opens.last().unwrap().cfg, Some(PulseConfig::multi()));

    let after_second = opens.len();
    set_outputs(&mut engine, &["local", "usb", "aux"]);
    thread::sleep(Duration::from_millis(80));
    assert_eq!(
        engine.mock_stream_opens().len(),
        after_second,
        "multi → multi must not re-open"
    );
}

#[test]
fn route_change_keeps_the_playhead() {
    let key = "route.playhead.audio03";
    let (_tmp, root) = library(key, RATE as usize * 8);
    let mock = MockOutputControl::new(three_sinks());
    let cell = RouteTarget::default_cell();
    let mut engine =
        Engine::open_mock_routed(&root, Capabilities::desktop_mock(), cell.clone()).unwrap();
    engine.attach_route(manager(&mock), cell);
    play(&mut engine, key);

    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::SeekMs(
        4_000,
    )));
    wait_until("seek to 4 s", || {
        engine.current_position_ms().unwrap_or(0) >= 4_000
    });
    engine.take_events();
    engine.publish_status();
    let published = engine
        .take_events()
        .into_iter()
        .find_map(|e| match e {
            plippa_engine::EngineEvent::Status(s) => Some(s.session.position_ms),
            _ => None,
        })
        .expect("a status snapshot");

    let starts = engine.stream_prepare_count();
    let opens = engine.mock_stream_opens().len();
    set_outputs(&mut engine, &["local", "usb"]);

    assert_eq!(
        engine.stream_prepare_count(),
        starts + 1,
        "one re-open for the route change"
    );
    let start_ms = engine.current_position_ms().expect("a requested start");
    assert!(
        start_ms.abs_diff(published) <= 250,
        "re-open at {start_ms} ms, published playhead {published} ms"
    );
    wait_opens(&engine, opens + 1);
    assert_eq!(
        engine.mock_stream_opens().last().unwrap().dev.as_deref(),
        Some("plippa.multi")
    );
}

#[test]
fn route_change_while_paused_does_not_start() {
    let key = "route.paused.audio03";
    let (_tmp, root) = library(key, RATE as usize * 4);
    let mock = MockOutputControl::new(three_sinks());
    let cell = RouteTarget::default_cell();
    let mut engine =
        Engine::open_mock_routed(&root, Capabilities::desktop_mock(), cell.clone()).unwrap();
    engine.attach_route(manager(&mock), cell);
    play(&mut engine, key);
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Pause));
    assert_eq!(engine.session().transport, TransportState::Paused);
    let position = engine.session().position_ms;
    let starts = engine.stream_prepare_count();

    set_outputs(&mut engine, &["local", "usb"]);

    assert_eq!(engine.session().transport, TransportState::Paused);
    assert_eq!(engine.stream_prepare_count(), starts + 1);
    assert_eq!(
        engine.current_position_ms(),
        Some(position),
        "a paused route change re-opens at the same position without playing"
    );
}

#[test]
fn delay_change_does_not_reopen() {
    let key = "route.delay.audio03";
    let (_tmp, root) = library(key, RATE as usize);
    let mock = MockOutputControl::new(three_sinks());
    let cell = RouteTarget::default_cell();
    let mut engine =
        Engine::open_mock_routed(&root, Capabilities::desktop_mock(), cell.clone()).unwrap();
    engine.attach_route(manager(&mock), cell);
    play(&mut engine, key);
    set_outputs(&mut engine, &["local", "usb"]);
    wait_opens(&engine, 2);
    let opens = engine.mock_stream_opens().len();
    let starts = engine.stream_prepare_count();

    engine.handle_ipc(IpcMessage::PlaybackCommand(
        PlaybackCommand::SetAudioDelayAuto(false),
    ));
    engine.poll_audio_route();
    engine.handle_ipc(IpcMessage::PlaybackCommand(
        PlaybackCommand::SetOutputDelay {
            output: "usb".into(),
            delay_ms: 80,
        },
    ));
    engine.poll_audio_route();
    thread::sleep(Duration::from_millis(80));

    assert_eq!(engine.stream_prepare_count(), starts, "no re-open on a delay change");
    assert_eq!(engine.mock_stream_opens().len(), opens);
    assert_eq!(
        engine
            .audio_route()
            .and_then(|r| r.output("usb"))
            .map(|o| o.delay_ms),
        Some(80)
    );
}

#[test]
fn failed_route_falls_back_to_local() {
    let key = "route.fallback.audio03";
    let (_tmp, root) = library(key, RATE as usize);
    let mock = MockOutputControl::new(vec![sink("local", 0, true)]);
    mock.fail_next_apply("module missing");
    let cell = RouteTarget::default_cell();
    let mut engine =
        Engine::open_mock_routed(&root, Capabilities::desktop_mock(), cell.clone()).unwrap();
    engine.attach_route(manager(&mock), cell);

    let notices: Vec<_> = engine
        .take_events()
        .into_iter()
        .filter_map(|e| match e {
            plippa_engine::EngineEvent::Notice(n) => Some(n),
            _ => None,
        })
        .collect();
    assert!(
        notices.iter().any(|n| n.code == NOTICE_AUDIO_ROUTE_FAILED),
        "fallback must raise the route-failed notice: {notices:?}"
    );
    let problem = engine.audio_route().unwrap().problem.clone();
    assert!(
        problem.as_deref().is_some_and(|p| p.contains("module missing")),
        "{problem:?}"
    );

    play(&mut engine, key);
    let opens = engine.mock_stream_opens();
    assert_eq!(opens.last().unwrap().dev.as_deref(), Some("local"));
    assert_eq!(engine.audio_route().unwrap().enabled, ["local"]);
    assert_eq!(engine.session().transport, TransportState::Playing);
}

struct NoServerControl {
    reported: bool,
}

impl OutputControl for NoServerControl {
    fn sinks(&mut self) -> Result<Vec<SinkInfo>, RouteError> {
        Err(RouteError::Backend("audio server not available".into()))
    }

    fn apply(
        &mut self,
        _intent: &RouteIntent,
        _previous: Option<&RouteIntent>,
    ) -> Result<(), RouteError> {
        Err(RouteError::Backend("audio server not available".into()))
    }

    fn poll(&mut self) -> Vec<RouteEvent> {
        if self.reported {
            Vec::new()
        } else {
            self.reported = true;
            vec![RouteEvent::Failed("audio server not available".into())]
        }
    }
}

#[test]
fn no_server_still_plays() {
    let key = "route.noserver.audio03";
    let (_tmp, root) = library(key, RATE as usize);
    let cell = RouteTarget::default_cell();
    let mut engine =
        Engine::open_mock_routed(&root, Capabilities::desktop_mock(), cell.clone()).unwrap();
    engine.attach_route(
        RouteManager::new(
            Box::new(NoServerControl { reported: false }),
            Some("local".into()),
            None,
        ),
        cell,
    );
    engine.poll_audio_route();

    let route = engine.audio_route().unwrap();
    assert_eq!(route.problem.as_deref(), Some("audio server not available"));
    assert!(!route.local_only);
    assert!(route.outputs.is_empty());

    play(&mut engine, key);
    assert_eq!(engine.session().transport, TransportState::Playing);
    assert!(engine.frames_written() > 0);
    let opens = engine.mock_stream_opens();
    assert_eq!(
        opens.last().unwrap().dev, None,
        "falls back to the server default sink"
    );
}
