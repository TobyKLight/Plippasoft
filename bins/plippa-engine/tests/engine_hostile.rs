//! SECFIX_06 / PAR-01 at the engine: a hostile media file cannot abort `plippa-engine`.
//!
//! Every decode runs in the isolated media helper. The review's files (built in code by
//! `plippa_audio::hostile`) abort a Symphonia decoder; here they must only kill a helper:
//! the engine stays up, reports `media_unplayable`, plays the next item, and never spawns a
//! helper again for a file it has marked.

use plippa_audio::hostile;
use plippa_audio::isolate::Isolation;
use plippa_core::{Capabilities, TransportState};
use plippa_engine::{Engine, EngineEvent};
use plippa_ipc_proto::{
    IpcMessage, PlaybackCommand, TapKeyPayload, NOTICE_MEDIA_UNPLAYABLE,
};
use plippa_noubin::{normalise_noubin_key, LibraryRoot, LibraryWriter};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};

/// Marks go to a throw-away state directory, not the developer's home.
fn isolate_state() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        std::env::set_var("PLIPPA_STATE_DIR", dir);
    });
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/plippa-audio/tests/fixtures")
        .join(name)
}

/// A release listing `items` (name, bytes) in order.
fn library(key: &str, items: &[(&str, Vec<u8>)]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let mut list = Vec::new();
    for (name, bytes) in items {
        fs::write(root.join(name), bytes).unwrap();
        list.push(format!(r#"{{"mediaTitle": "{name}", "localFilePaths": ["{name}"]}}"#));
    }
    fs::write(
        root.join("_hostile.noudata"),
        format!(
            r#"{{"noubinKey": "{key}", "noubinKeyNormalised": "{}",
                 "playableItemTitle": "Hostile", "mediaList": [{}]}}"#,
            normalise_noubin_key(key),
            list.join(",")
        ),
    )
    .unwrap();
    LibraryWriter::open(&LibraryRoot::new(&root)).unwrap().full_scan().unwrap();
    (tmp, root)
}

fn tap(engine: &mut Engine, key: &str) {
    engine.handle_ipc(IpcMessage::TapKey(TapKeyPayload {
        noubin_key: key.into(),
        tap_id: 1,
        already_normalised: false,
    }));
}

/// Poll the engine like the hub tick until `done` says so; returns every event seen.
fn run_until(engine: &mut Engine, what: &str, mut done: impl FnMut(&Engine, &[EngineEvent]) -> bool) -> Vec<EngineEvent> {
    let started = Instant::now();
    let mut seen = Vec::new();
    loop {
        engine.poll_stream_lifecycle();
        seen.extend(engine.take_events());
        if done(engine, &seen) {
            return seen;
        }
        assert!(started.elapsed() < Duration::from_secs(20), "timed out: {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn unplayable_notices(events: &[EngineEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, EngineEvent::Notice(n) if n.code == NOTICE_MEDIA_UNPLAYABLE))
        .count()
}

#[test]
fn the_engine_survives_play_of_a_hostile_file() {
    isolate_state();
    let key = "hostile.noubin.secfix06a";
    let cases = hostile::review_cases();
    let (_tmp, root) = library(
        key,
        &[
            (cases[1].name, cases[1].bytes.clone()), // FLAC: huge Vorbis comment
            (cases[2].name, cases[2].bytes.clone()), // MP3: huge ID3 frame
            ("good.mp3", fs::read(fixture("tone_48k_3s.mp3")).unwrap()),
        ],
    );
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    tap(&mut engine, key);

    // Play of the hostile first item: media_unplayable, not an engine abort.
    let events = run_until(&mut engine, "unplayable notice", |_, ev| unplayable_notices(ev) > 0);
    assert!(unplayable_notices(&events) >= 1);
    // The engine still answers, and the file is marked on the device.
    assert_eq!(engine.session().transport, TransportState::Stopped);
    let flac = root.join(cases[1].name);
    assert!(Isolation::global().is_marked_by_path(&flac));

    // The next hostile item behaves the same, then the good one plays.
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::NextTrack));
    run_until(&mut engine, "second unplayable notice", |_, ev| unplayable_notices(ev) > 0);
    assert!(Isolation::global().is_marked_by_path(&root.join(cases[2].name)));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::NextTrack));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::Play));
    let before = engine.frames_written();
    run_until(&mut engine, "the good item plays", |e, _| e.frames_written() > before + 4096);
    assert_eq!(engine.session().transport, TransportState::Playing);

    // A marked file costs no helper: going back to it is refused at once, with the same notice.
    let spawned = Isolation::global().spawn_count();
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::PreviousTrack));
    engine.handle_ipc(IpcMessage::PlaybackCommand(PlaybackCommand::PreviousTrack));
    let events = engine.take_events();
    assert!(unplayable_notices(&events) >= 1, "{events:?}");
    assert_eq!(Isolation::global().spawn_count(), spawned, "a marked file spawns no helper");
}

#[test]
fn a_hostile_neighbour_does_not_disturb_the_item_that_is_playing() {
    isolate_state();
    let key = "hostile.noubin.secfix06b";
    let cases = hostile::review_cases();
    let (_tmp, root) = library(
        key,
        &[
            ("good.mp3", fs::read(fixture("tone_48k_3s.mp3")).unwrap()),
            (cases[1].name, cases[1].bytes.clone()),
        ],
    );
    let mut engine = Engine::open_mock(&root, Capabilities::desktop_mock()).unwrap();
    let hostile_path = root.join(cases[1].name);
    tap(&mut engine, key);
    // The playing item starts; its next neighbour (the hostile file) is pre-opened in a helper
    // of its own, which dies. The neighbour ends up marked while the engine keeps playing.
    run_until(&mut engine, "neighbour pre-open crashed its helper", |_, _| {
        Isolation::global().is_marked_by_path(&hostile_path)
    });
    let before = engine.frames_written();
    run_until(&mut engine, "playback continues", |e, _| e.frames_written() > before + 8192);
    assert_eq!(engine.session().transport, TransportState::Playing);
}
